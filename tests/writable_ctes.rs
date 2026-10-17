//! `WITH x AS (INSERT | UPDATE | DELETE ... RETURNING ...)`: what Postgres does is in
//! tests/pgdiff.rs (`writable_ctes`). Here: what it refuses (a statement that reads a table an
//! earlier one of the WITH changed, a world in the past, an agent that can only read), that it is
//! atomic, and that a driver's parameters and its describe of the result work.

mod common;

use chronos::sql::{Session, Value};
use chronos::{AgentSpec, Can, Db, MAIN};
use postgres::{Client, NoTls};
use std::process::Command;

fn rows(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    out.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> chronos::sql::SqlError {
    s.run(db, q, &[]).err().unwrap_or_else(|| panic!("{q} should fail"))
}

fn setup() -> (Db, Session) {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, "create table t (id serial primary key, v int); create table log (id serial primary key, n int)", &[])
        .unwrap();
    s.run(&db, "insert into t (v) values (1), (2), (3)", &[]).unwrap();
    (db, s)
}

#[test]
fn a_statement_that_reads_a_table_an_earlier_one_changes_is_refused() {
    let (db, mut s) = setup();
    for q in [
        // the main statement reads what a WITH query changed: Postgres shows it the old rows
        "with u as (update t set v = v + 10 returning id) select count(*) from t",
        "with u as (delete from t where v = 1 returning id) select sum(v) from t",
        "with i as (insert into t (v) values (9) returning id) delete from t where id in (select id from i)",
        // through a view, a subquery, and a WITH query that isn't a change
        "with u as (update t set v = 0 returning id) select * from (select * from t) x",
        "with u as (update t set v = 0 returning id), r as (select * from t) select * from r",
        // a later WITH query that reads it
        "with u as (update t set v = 0 returning id), d as (delete from t where v = 0 returning id) select * from d",
    ] {
        let e = fails(&db, &mut s, q);
        assert_eq!(e.code, "0A000", "{q}: {e}");
        assert!(e.message.contains("reads table"), "{q}: {e}");
    }
    s.run(&db, "create view tv as select * from t", &[]).unwrap();
    let e = fails(&db, &mut s, "with u as (update t set v = 0 returning id) select * from tv");
    assert_eq!(e.code, "0A000", "a view of it is a read of it: {e}");
    // nothing ran
    assert_eq!(rows(&db, &mut s, "select v from t order by id"), ["1", "2", "3"]);
    // what is allowed: the main statement reads only the rows the WITH query gave back, and other tables
    assert_eq!(
        rows(&db, &mut s, "with u as (update t set v = v * 2 returning id, v) select v from u order by id"),
        ["2", "4", "6"]
    );
    assert_eq!(
        rows(
            &db,
            &mut s,
            "with d as (delete from t where v > 2 returning v) insert into log (n) select v from d returning n"
        ),
        ["4", "6"]
    );
}

#[test]
fn all_of_it_happens_or_none_of_it() {
    let (db, mut s) = setup();
    let e = fails(
        &db,
        &mut s,
        "with i as (insert into log (id, n) values (100, 1) returning id) insert into t (id, v) select id, 1 from i union all select 1, 1",
    );
    assert_eq!(e.code, "23505", "{e}");
    assert_eq!(
        rows(&db, &mut s, "select count(*) from log"),
        ["0"],
        "the WITH query's insert was undone with the statement"
    );
    // in a transaction it rolls back with it
    s.run(&db, "begin", &[]).unwrap();
    assert_eq!(rows(&db, &mut s, "with i as (insert into log (n) values (5) returning n) select n from i"), ["5"]);
    s.run(&db, "rollback", &[]).unwrap();
    assert_eq!(rows(&db, &mut s, "select count(*) from log"), ["0"]);
    // outside one, a WITH is a transaction of its own
    assert_eq!(rows(&db, &mut s, "with i as (insert into log (n) values (6) returning n) select n from i"), ["6"]);
    assert_eq!(rows(&db, &mut s, "select count(*) from log"), ["1"]);
}

#[test]
fn an_agent_that_can_only_read_cannot_write_through_a_with() {
    let (db, _) = setup();
    db.create_agent("reader", &AgentSpec { can: Some(vec![Can::Read]), ..Default::default() }).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("reader".into());
    let e = fails(&db, &mut s, "with i as (insert into log (n) values (1) returning n) select n from i");
    assert_eq!(e.code, "42501", "{e}");
    let e = fails(&db, &mut s, "with i as (select 1 as n), d as (delete from t returning id) select n from i");
    assert_eq!(e.code, "42501", "{e}");
    assert_eq!(rows(&db, &mut Session::new(MAIN), "select count(*) from t"), ["3"]);
    // the database's own tables stay out of reach inside one too
    let mut me = Session::new(MAIN);
    let e = fails(&db, &mut me, "with d as (delete from _sonos_agents returning 1) select * from d");
    assert_eq!(e.code, "42501", "{e}");
}

#[test]
fn a_driver_binds_parameters_and_describes_the_result() {
    let dir = std::env::temp_dir().join(format!("chronos-wctes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = common::serve(Command::new(env!("CARGO_BIN_EXE_chronos")).args(["serve", dir.to_str().unwrap()]), true);
    let mut child = s.child;
    let mut c = Client::connect(&format!("host=127.0.0.1 port={} user=tester dbname=main", s.pg), NoTls).unwrap();
    c.batch_execute("create table t (id serial primary key, name text, qty int)").unwrap();
    let stmt = c
        .prepare("with i as (insert into t (name, qty) values ($1, $2) returning id, name, qty * 2 as twice) select id, name, twice from i")
        .unwrap();
    let names: Vec<_> = stmt.columns().iter().map(|c| (c.name().to_string(), c.type_().name().to_string())).collect();
    assert_eq!(names, [("id".into(), "int4".into()), ("name".into(), "text".into()), ("twice".into(), "int4".into())]);
    assert_eq!(stmt.params().iter().map(|t| t.name()).collect::<Vec<_>>(), ["text", "int4"]);
    for (n, q) in [("a", 1i32), ("b", 2)] {
        let r = c.query_one(&stmt, &[&n, &q]).unwrap();
        assert_eq!((r.get::<_, String>(1), r.get::<_, i32>(2)), (n.to_string(), q * 2));
    }
    let r = c.query("select name, qty from t order by id", &[]).unwrap();
    assert_eq!(r.len(), 2, "each run inserted once, not once for the describe and once for the run");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
