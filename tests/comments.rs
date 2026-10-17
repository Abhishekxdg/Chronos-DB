//! COMMENT ON is kept as rows of the world: what Postgres says about each object is in
//! tests/pgdiff.rs (`comments`), and pg_dump's in tests/pg_dump.rs. Here: what makes it Chronos's:
//! a world's comments are its own, a merge brings them, DIFF shows them as the statements that make
//! them, and writing one needs the right to write.

use chronos::sql::{Session, Value};
use chronos::{AgentSpec, Can, Db, MAIN};

fn one(db: &Db, s: &mut Session, q: &str) -> Value {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    out.rows.first().map_or(Value::Null, |r| r[0].clone())
}

fn run(db: &Db, s: &mut Session, q: &str) {
    s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
}

#[test]
fn a_worlds_comments_are_its_own_and_a_merge_brings_them() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for q in [
        "create table t (id int primary key, v text)",
        "comment on table t is 'on main'",
        "create branch side",
        "use branch side",
        "comment on table t is 'on side'",
        "comment on column t.v is 'the value'",
    ] {
        run(&db, &mut s, q);
    }
    let q = "select obj_description('t'::regclass, 'pg_class')";
    assert_eq!(one(&db, &mut s, q), Value::Text("on side".into()));
    run(&db, &mut s, "use branch main");
    assert_eq!(one(&db, &mut s, q), Value::Text("on main".into()));
    assert_eq!(one(&db, &mut s, "select col_description('t'::regclass, 2)"), Value::Null);
    run(&db, &mut s, "merge branch side");
    assert_eq!(one(&db, &mut s, q), Value::Text("on side".into()));
    assert_eq!(one(&db, &mut s, "select col_description('t'::regclass, 2)"), Value::Text("the value".into()));
}

#[test]
fn diff_as_sql_writes_the_comments_after_what_they_are_on() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    run(&db, &mut s, "create branch side");
    run(&db, &mut s, "use branch side");
    for q in [
        "create table app_t (id int primary key, note text)",
        "comment on table app_t is 'it''s a table'",
        "comment on column app_t.note is 'free text'",
    ] {
        run(&db, &mut s, q);
    }
    let out = s.run(&db, "diff branch side as sql", &[]).unwrap().pop().unwrap();
    let lines: Vec<String> = out.rows.iter().map(|r| r[0].to_text()).collect();
    let at =
        |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle}: {lines:?}"));
    assert!(at("create table app_t") < at("comment on table app_t is 'it''s a table'"), "{lines:?}");
    assert!(at("create table app_t") < at("comment on column app_t.note is 'free text'"), "{lines:?}");
}

#[test]
fn writing_a_comment_needs_the_right_to_write() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    run(&db, &mut s, "create table t (id int primary key)");
    db.create_agent("reader", &AgentSpec { can: Some(vec![Can::Read]), ..Default::default() }).unwrap();
    let mut r = Session::new(MAIN);
    r.agent = Some("reader".into());
    let e = r.run(&db, "comment on table t is 'nope'", &[]).err().unwrap();
    assert_eq!(e.code, "42501", "{e}");
    assert_eq!(one(&db, &mut s, "select obj_description('t'::regclass, 'pg_class')"), Value::Null);
}

#[test]
fn what_is_not_kept_says_so() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    run(&db, &mut s, "create function f() returns int language sql as 'select 1'");
    let out = s.run(&db, "comment on function f() is 'x'", &[]).unwrap().pop().unwrap();
    assert_eq!(out.tag, "COMMENT");
}

#[test]
fn a_view_another_view_reads_keeps_its_name() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for q in [
        "create table t (a int)",
        "create view v as select a from t",
        "create view w as select a from v",
        "comment on view w is 'w'",
    ] {
        run(&db, &mut s, q);
    }
    // views name what they read and aren't rewritten (Postgres follows the rename): refused, and nothing moves
    let e = s.run(&db, "alter view v rename to v2", &[]).err().unwrap();
    assert_eq!(e.code, "0A000", "{e}");
    assert_eq!(one(&db, &mut s, "select count(*) from v"), Value::Int(0));
    // one nothing reads goes, with its comment
    run(&db, &mut s, "alter view w rename to w2");
    assert_eq!(one(&db, &mut s, "select obj_description('w2'::regclass, 'pg_class')"), Value::Text("w".into()));
}
