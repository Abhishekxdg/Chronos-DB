//! A writing statement's nextval reserves values for its session ahead; another session's
//! restart of the counter, once committed, makes them stale: the session's next value continues
//! from the restart, as Postgres's (CACHE 1, so nothing reserved) does.

use chronos::sql::{Output, Session, Value};
use chronos::{Db, MAIN};

fn rows(o: &Output) -> Vec<String> {
    o.rows.iter().map(|r| r.iter().map(Value::to_text).collect::<Vec<_>>().join("|")).collect()
}

fn q(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    rows(&s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap())
}

/// Session b takes values of `seq` in two writes, so it holds some reserved; session a runs
/// `restart` in a transaction and commits; returns b's next value then.
fn after_restart(setup: &[&str], seq: &str, restart: &str) -> Vec<String> {
    let db = Db::new();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    for s in setup {
        q(&db, &mut a, s);
    }
    q(&db, &mut a, "create table taken (v bigint)");
    for _ in 0..2 {
        q(&db, &mut b, &format!("insert into taken select nextval('{seq}') from generate_series(1, 7)"));
    }
    q(&db, &mut a, "begin");
    q(&db, &mut a, restart);
    q(&db, &mut a, "commit");
    let mut got = q(&db, &mut b, &format!("select nextval('{seq}')"));
    got.extend(q(&db, &mut b, &format!("insert into taken select nextval('{seq}') returning v")));
    got
}

#[test]
fn an_alter_restart_another_session_commits() {
    assert_eq!(after_restart(&["create sequence s"], "s", "alter sequence s restart"), ["1", "2"]);
}

#[test]
fn a_truncate_restart_identity_another_session_commits() {
    let setup = ["create table t (id serial, v text)"];
    assert_eq!(after_restart(&setup, "t_id_seq", "truncate t restart identity"), ["1", "2"]);
}

#[test]
fn a_serial_table_made_again() {
    let setup = ["create table t (id serial, v text)"];
    let again = "drop table t; create table t (id serial, v text)";
    assert_eq!(after_restart(&setup, "t_id_seq", again), ["1", "2"]);
}

#[test]
fn a_restart_outside_a_transaction() {
    let db = Db::new();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create table taken (v bigint)");
    for _ in 0..2 {
        q(&db, &mut b, "insert into taken select nextval('s') from generate_series(1, 7)");
    }
    q(&db, &mut a, "alter sequence s restart with 100");
    assert_eq!(q(&db, &mut b, "select nextval('s')"), ["100"]);
    q(&db, &mut a, "select setval('s', 500)");
    assert_eq!(q(&db, &mut b, "insert into taken select nextval('s') returning v"), ["501"]);
}

/// A restart another session rolls back leaves the reserved values good.
#[test]
fn a_rolled_back_restart_leaves_them() {
    let db = Db::new();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create table taken (v bigint)");
    for _ in 0..2 {
        q(&db, &mut b, "insert into taken select nextval('s') from generate_series(1, 7)");
    }
    q(&db, &mut a, "begin");
    q(&db, &mut a, "alter sequence s restart");
    q(&db, &mut a, "rollback");
    let next: i64 = q(&db, &mut b, "select nextval('s')")[0].parse().unwrap();
    let max: i64 = q(&db, &mut b, "select max(v) from taken")[0].parse().unwrap();
    assert!(next > max, "{next} after {max}");
}
