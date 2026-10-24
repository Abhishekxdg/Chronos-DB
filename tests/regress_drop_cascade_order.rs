//! What DROP SCHEMA ... CASCADE and DROP TABLE ... CASCADE say they drop, as Postgres's reportDependentObjects says
//! it: the objects in the order they were made, each followed by what depends on it, the schemas named last first;
//! partitions and the sequences of serial columns left out (Postgres reaches them through their tables); each named
//! as getObjectDescription names it, qualified only where the search_path doesn't find it.

use chronos::sql::Session;
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) {
    s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
}

/// The "drop cascades to" notice `q` sends: its message, and what it lists (the DETAIL's lines, or the one object).
fn cascades(db: &Db, s: &mut Session, q: &str) -> (String, Vec<String>) {
    let _ = chronos::sql::take_notices();
    run(db, s, q);
    let n = chronos::sql::take_notices()
        .into_iter()
        .find(|n| n.message.starts_with("drop cascades to"))
        .unwrap_or_else(|| panic!("{q}: no drop cascades notice"));
    let listed = match &n.detail {
        Some(d) => d.lines().map(|l| l.trim_start_matches("drop cascades to ").to_string()).collect(),
        None => vec![n.message.trim_start_matches("drop cascades to ").to_string()],
    };
    (n.message, listed)
}

/// `q` refused because other objects depend on what it drops: the message, and the DETAIL's lines.
fn refused(db: &Db, s: &mut Session, q: &str) -> (String, Vec<String>) {
    let e = s.run(db, q, &[]).expect_err(q);
    assert_eq!(e.code, "2BP01", "{q}: {}", e.message);
    let detail = e.info.and_then(|i| i.detail).unwrap_or_default();
    (e.message, detail.lines().map(String::from).collect())
}

#[test]
fn drop_schema_lists_what_is_in_it_in_the_order_it_was_made() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        "create schema sc",
        "set search_path = sc",
        "create function f_leak(text) returns boolean language sql as $$ select true $$",
        "create table zeta (id serial primary key)",
        "create table alpha (a int) partition by range (a)",
        "create table alpha_1 partition of alpha for values from (1) to (10)",
        "create view mid as select id from zeta",
        "create table beta (b int)",
        "create materialized view mv as select b from beta",
        "create sequence counter",
    ] {
        run(&db, &mut s, q);
    }
    // (zeta's serial column's sequence and alpha's partition go with their tables, unsaid)
    let (message, listed) = refused(&db, &mut s, "drop schema sc");
    assert_eq!(message, "cannot drop schema sc because other objects depend on it");
    assert_eq!(
        listed,
        [
            "function f_leak(text) depends on schema sc",
            "table zeta depends on schema sc",
            "table alpha depends on schema sc",
            "view mid depends on schema sc",
            "table beta depends on schema sc",
            "materialized view mv depends on schema sc",
            "sequence counter depends on schema sc",
        ]
    );
    let (message, listed) = cascades(&db, &mut s, "drop schema sc cascade");
    assert_eq!(message, "drop cascades to 7 other objects");
    assert_eq!(
        listed,
        [
            "function f_leak(text)",
            "table zeta",
            "table alpha",
            "view mid",
            "table beta",
            "materialized view mv",
            "sequence counter"
        ]
    );
}

#[test]
fn schemas_dropped_together_are_listed_the_last_named_first() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        "create schema one",
        "create schema two",
        "create table one.t1 (a int)",
        "create table one.\"table two\" (a int)",
        "create table two.t2 (a int)",
        "create function two.add(int, int) returns int language sql as $$ select $1 + $2 $$",
        "create table two.t3 (a int)",
        "create procedure two.put(int) language sql as $$ insert into two.t3 values ($1) $$",
    ] {
        run(&db, &mut s, q);
    }
    let (message, _) = refused(&db, &mut s, "drop schema one, two");
    assert_eq!(message, "cannot drop desired object(s) because other objects depend on them");
    // (off the search_path, qualified; a function's argument types with no space, and a procedure is a function)
    let (message, listed) = cascades(&db, &mut s, "drop schema one, two cascade");
    assert_eq!(message, "drop cascades to 6 other objects");
    assert_eq!(
        listed,
        [
            "table two.t2",
            "function two.add(integer,integer)",
            "table two.t3",
            "function two.put(integer)",
            "table one.t1",
            "table one.\"table two\""
        ]
    );
}

#[test]
fn what_other_schemas_have_over_its_objects_follows_them() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        "create schema s",
        "create table s.t1 (a int primary key)",
        "create table s.t2 (b int)",
        "create view v1 as select a from s.t1",
        "create table child (c int) inherits (s.t2)",
        "create table refs (a int references s.t1)",
    ] {
        run(&db, &mut s, q);
    }
    let (_, listed) = refused(&db, &mut s, "drop schema s");
    assert_eq!(
        listed,
        [
            "table s.t1 depends on schema s",
            "view v1 depends on table s.t1",
            "constraint refs_a_fkey on table refs depends on table s.t1",
            "table s.t2 depends on schema s",
            "table child depends on table s.t2",
        ]
    );
    let (message, listed) = cascades(&db, &mut s, "drop schema s cascade");
    assert_eq!(message, "drop cascades to 5 other objects");
    assert_eq!(listed, ["table s.t1", "view v1", "constraint refs_a_fkey on table refs", "table s.t2", "table child"]);
    run(&db, &mut s, "insert into refs values (1)");
    assert_eq!(s.run(&db, "select * from child", &[]).expect_err("child is gone").code, "42P01");
}

#[test]
fn drop_table_cascade_lists_the_tables_below_as_postgres_does() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    // (as inherit.sql: c3 inherits from both c1 and c2, so it comes after the newer of them)
    for q in [
        "create table p1 (f1 int)",
        "create table p2 (f2 int)",
        "create table c1 (f3 int) inherits (p1, p2)",
        "create table c2 (f3 int) inherits (p1, p2)",
        "create table c3 (f4 int) inherits (c1, c2)",
    ] {
        run(&db, &mut s, q);
    }
    let (message, listed) = cascades(&db, &mut s, "drop table p1 cascade");
    assert_eq!(message, "drop cascades to 3 other objects");
    assert_eq!(listed, ["table c1", "table c2", "table c3"]);
    // two named: what the second has below it, then the first's, each after the newest of what it inherits from
    for q in [
        "create table inht1 (a int, b int)",
        "create table inhs1 (b int, c int)",
        "create table inht2 (x int) inherits (inht1)",
        "create table inht3 (y int) inherits (inht1)",
        "create table inht4 (z int) inherits (inht2, inht3)",
        "create table inhts (d int) inherits (inht2, inhs1)",
    ] {
        run(&db, &mut s, q);
    }
    let (message, listed) = cascades(&db, &mut s, "drop table inht1, inhs1 cascade");
    assert_eq!(message, "drop cascades to 4 other objects");
    assert_eq!(listed, ["table inht2", "table inhts", "table inht3", "table inht4"]);
}

#[test]
fn a_renamed_object_keeps_its_place_and_a_dropped_one_leaves_nothing_behind() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        "create schema r",
        "set search_path = r",
        "create table a (x int)",
        "create table b (x int)",
        "create table c (x int)",
        "alter table a rename to z",
        "drop table b",
        "create table b (x int)",
    ] {
        run(&db, &mut s, q);
    }
    let (_, listed) = cascades(&db, &mut s, "drop schema r cascade");
    assert_eq!(listed, ["table z", "table c", "table b"]);
    // (when each was made is kept beside it, and goes with it)
    assert!(db.prefix(MAIN, "_sonos_made/").unwrap().is_empty());
}

#[test]
fn past_a_hundred_objects_the_rest_are_counted() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create schema many");
    for i in 0..101 {
        run(&db, &mut s, &format!("create table many.t{i:03} (a int)"));
    }
    let (message, listed) = cascades(&db, &mut s, "drop schema many cascade");
    assert_eq!(message, "drop cascades to 101 other objects");
    assert_eq!(listed.len(), 101);
    assert_eq!((listed[0].as_str(), listed[99].as_str()), ("table many.t000", "table many.t099"));
    assert_eq!(listed[100], "and 1 other object (see server log for list)");
}

#[test]
fn a_policy_comes_where_it_was_made_among_what_reads_a_table() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        "create table other (a int)",
        "create table base (a int)",
        "create view bv as select a from base",
        "create policy p on other using (a in (select a from base))",
        "create view bv2 as select a from base",
    ] {
        run(&db, &mut s, q);
    }
    let (message, listed) = cascades(&db, &mut s, "drop table base cascade");
    assert_eq!(message, "drop cascades to 3 other objects");
    assert_eq!(listed, ["view bv", "policy p on table other", "view bv2"]);
    // a view, a table below, another table's foreign key, then a policy: as they were made
    for q in [
        "create table pa (a int primary key)",
        "create view pav as select a from pa",
        "create table ch (b int) inherits (pa)",
        "create table refs (a int references pa)",
        "create policy p2 on other using (a in (select a from pa))",
    ] {
        run(&db, &mut s, q);
    }
    let (message, listed) = cascades(&db, &mut s, "drop table pa cascade");
    assert_eq!(message, "drop cascades to 4 other objects");
    assert_eq!(listed, ["view pav", "table ch", "constraint refs_a_fkey on table refs", "policy p2 on table other"]);
    // a renamed policy keeps its place
    for q in [
        "create table base (a int)",
        "create view bv3 as select a from base",
        "create policy p3 on other using (a in (select a from base))",
        "alter policy p3 on other rename to p4",
    ] {
        run(&db, &mut s, q);
    }
    let (_, listed) = cascades(&db, &mut s, "drop table base cascade");
    assert_eq!(listed, ["view bv3", "policy p4 on table other"]);
    // when a policy was made moves with its table, and goes with it
    let made = |db: &Db| -> Vec<String> {
        db.prefix(MAIN, "_sonos_made/policy/").unwrap().into_iter().map(|(k, _)| k).collect()
    };
    assert!(made(&db).is_empty());
    run(&db, &mut s, "create policy p5 on other using (a > 0)");
    run(&db, &mut s, "alter table other rename to other2");
    assert_eq!(made(&db), ["_sonos_made/policy/other2/p5"]);
    run(&db, &mut s, "drop policy p5 on other2");
    assert!(made(&db).is_empty());
}

#[test]
fn the_functions_of_a_table_s_row_type_come_right_after_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    // (pg_depend has them on the table's row type, made with it: before the view made after the table)
    for q in [
        "create table t (a int)",
        "create view v as select a from t",
        "create function f(t) returns int language sql as $$ select 1 $$",
        "create function g(t, int) returns int language sql as $$ select $2 $$",
    ] {
        run(&db, &mut s, q);
    }
    let (message, listed) = cascades(&db, &mut s, "drop table t cascade");
    assert_eq!(message, "drop cascades to 3 other objects");
    assert_eq!(listed, ["function f(t)", "function g(t,integer)", "view v"]);
    // a function of another schema on the row type of a table DROP SCHEMA drops: it goes, and is said, with the table
    for q in [
        "create schema rs",
        "create table rs.t (a int)",
        "create view rs.v as select a from rs.t",
        "create function h(rs.t) returns int language sql as $$ select 1 $$",
    ] {
        run(&db, &mut s, q);
    }
    let (_, listed) = refused(&db, &mut s, "drop schema rs");
    assert_eq!(
        listed,
        ["table rs.t depends on schema rs", "function h(rs.t) depends on type rs.t", "view rs.v depends on schema rs"]
    );
    let (message, listed) = cascades(&db, &mut s, "drop schema rs cascade");
    assert_eq!(message, "drop cascades to 3 other objects");
    assert_eq!(listed, ["table rs.t", "function h(rs.t)", "view rs.v"]);
}

#[test]
fn a_row_type_argument_is_named_where_the_search_path_finds_its_table() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        "set search_path = nosuch, public",
        "create table tq (a int)",
        "create function fq(tq) returns int language sql as $$ select 1 $$",
    ] {
        run(&db, &mut s, q);
    }
    let (message, listed) = refused(&db, &mut s, "drop table tq");
    assert_eq!(message, "cannot drop table tq because other objects depend on it");
    assert_eq!(listed, ["function fq(tq) depends on type tq"]);
    let (_, listed) = cascades(&db, &mut s, "drop table tq cascade");
    assert_eq!(listed, ["function fq(tq)"]);
    for q in [
        "create schema s",
        "set search_path = s, public",
        "create table t5 (a int)",
        // (made in s, the first schema of the path; its argument's type written qualified)
        "create function f6(s.t5, int) returns int language sql as $$ select $2 $$",
    ] {
        run(&db, &mut s, q);
    }
    let (_, listed) = cascades(&db, &mut s, "drop table t5 cascade");
    assert_eq!(listed, ["function f6(t5,integer)"]);
    // off the search_path, qualified
    for q in [
        "create table t7 (a int)",
        "create function f8(s.t7) returns int language sql as $$ select 1 $$",
        "set search_path = public",
    ] {
        run(&db, &mut s, q);
    }
    let (_, listed) = cascades(&db, &mut s, "drop table s.t7 cascade");
    assert_eq!(listed, ["function s.f8(s.t7)"]);
}
