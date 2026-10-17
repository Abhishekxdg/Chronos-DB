//! The notices and warnings Postgres sends where a statement does less than it was asked
//! (IF [NOT] EXISTS, COMMIT with nothing to commit), worded as Postgres words them: the differential
//! tests (`pgdiff.rs`) compare rows and error classes, not what the server says on the side.

use chronos::sql::{Session, take_notices};
use chronos::{Db, MAIN};

/// Runs `sql`, and gives the notices it sent as `LEVEL: message`.
fn said(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    take_notices();
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    take_notices().into_iter().map(|n| format!("{}: {}", n.level, n.message)).collect()
}

fn setup() -> (Db, Session) {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for q in ["create table t (id int primary key, v text)", "create index t_v on t (v)", "create sequence s1"] {
        s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    (db, s)
}

#[test]
fn dropping_what_isnt_there() {
    let (db, mut s) = setup();
    for (sql, want) in [
        ("drop table if exists nosuch", "NOTICE: table \"nosuch\" does not exist, skipping"),
        ("drop index if exists noi", "NOTICE: index \"noi\" does not exist, skipping"),
        ("drop extension if exists noext", "NOTICE: extension \"noext\" does not exist, skipping"),
        (
            "alter table t drop column if exists nocol",
            "NOTICE: column \"nocol\" of relation \"t\" does not exist, skipping",
        ),
        (
            "alter table t drop constraint if exists nocon",
            "NOTICE: constraint \"nocon\" of relation \"t\" does not exist, skipping",
        ),
        ("drop trigger if exists g on nosuchtable", "NOTICE: relation \"nosuchtable\" does not exist, skipping"),
    ] {
        assert_eq!(said(&db, &mut s, sql), [want], "{sql}");
    }
}

#[test]
fn altering_what_isnt_there() {
    let (db, mut s) = setup();
    for (sql, want) in [
        ("alter table if exists nosuch add column x int", "NOTICE: relation \"nosuch\" does not exist, skipping"),
        ("alter table if exists nosuch drop column x", "NOTICE: relation \"nosuch\" does not exist, skipping"),
        ("alter table if exists nosuch rename to other", "NOTICE: relation \"nosuch\" does not exist, skipping"),
        ("alter index if exists noi rename to noi2", "NOTICE: relation \"noi\" does not exist, skipping"),
        ("alter sequence if exists nos restart", "NOTICE: relation \"nos\" does not exist, skipping"),
        ("alter view if exists nov rename to nov2", "NOTICE: relation \"nov\" does not exist, skipping"),
        (
            "alter materialized view if exists nomv rename to nomv2",
            "NOTICE: relation \"nomv\" does not exist, skipping",
        ),
    ] {
        assert_eq!(said(&db, &mut s, sql), [want], "{sql}");
    }
    // without IF EXISTS it is an error, as before
    assert_eq!(s.run(&db, "alter table nosuch add column x int", &[]).err().map(|e| e.code), Some("42P01"));
}

#[test]
fn creating_what_is_already_there() {
    let (db, mut s) = setup();
    for (sql, want) in [
        ("create table if not exists t (id int)", "NOTICE: relation \"t\" already exists, skipping"),
        ("create index if not exists t_v on t (v)", "NOTICE: relation \"t_v\" already exists, skipping"),
        ("create sequence if not exists s1", "NOTICE: relation \"s1\" already exists, skipping"),
        ("create schema if not exists public", "NOTICE: schema \"public\" already exists, skipping"),
    ] {
        assert_eq!(said(&db, &mut s, sql), [want], "{sql}");
    }
    // and nothing when it makes it
    assert!(said(&db, &mut s, "create table if not exists fresh (id int)").is_empty());
    assert!(said(&db, &mut s, "create index if not exists fresh_id on fresh (id)").is_empty());
    assert!(said(&db, &mut s, "create sequence if not exists s2").is_empty());
}

#[test]
fn transaction_commands_with_nothing_to_do() {
    let (db, mut s) = setup();
    assert_eq!(said(&db, &mut s, "commit"), ["WARNING: there is no transaction in progress"]);
    assert_eq!(said(&db, &mut s, "rollback"), ["WARNING: there is no transaction in progress"]);
    assert_eq!(said(&db, &mut s, "end"), ["WARNING: there is no transaction in progress"]);
    assert!(said(&db, &mut s, "begin").is_empty());
    assert_eq!(said(&db, &mut s, "begin"), ["WARNING: there is already a transaction in progress"]);
    assert!(said(&db, &mut s, "commit").is_empty());
}
