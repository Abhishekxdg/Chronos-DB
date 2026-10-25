//! An aggregate in a subquery whose arguments read only the outer query's columns is the outer query's, as Postgres
//! has it (an aggregate's level is its arguments'; aggregates.sql): it aggregates the outer group, and makes the
//! outer query aggregate.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn setup(db: &Db, s: &mut Session) {
    run(
        db,
        s,
        "CREATE TABLE onek (ten int4, four int4);
         INSERT INTO onek SELECT i % 10, i % 4 FROM generate_series(0, 999) i",
    );
}

#[test]
fn a_having_subquerys_aggregate_of_the_outer_columns_is_the_groups() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    let rows = run(
        &db,
        &mut s,
        "SELECT ten, sum(DISTINCT four) FROM onek a GROUP BY ten
         HAVING EXISTS (SELECT 1 FROM onek b WHERE sum(DISTINCT a.four) = b.four) ORDER BY 1",
    );
    assert_eq!(rows, [["0", "2"], ["2", "2"], ["4", "2"], ["6", "2"], ["8", "2"]]);
    // beside the subquery's own aggregate
    assert_eq!(
        run(
            &db,
            &mut s,
            "SELECT ten, (SELECT max(a.four) + count(*) FROM onek b WHERE b.ten = 0) FROM onek a
             GROUP BY ten ORDER BY 1 LIMIT 2"
        ),
        [["0", "102"], ["1", "103"]]
    );
}

#[test]
fn it_makes_the_outer_query_aggregate() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    assert_eq!(
        run(
            &db,
            &mut s,
            "SELECT (SELECT count(*) FILTER (WHERE outer_c <> 0) FROM (VALUES (1)) t0(inner_c))
             FROM (VALUES (2), (3)) t1(outer_c)"
        ),
        [["2"]]
    );
    assert_eq!(run(&db, &mut s, "SELECT (SELECT sum(a.four)) FROM onek a"), [["1500"]]);
    // the subquery's own aggregate leaves the outer query a row each
    assert_eq!(
        run(&db, &mut s, "SELECT (SELECT count(*) FROM (VALUES (1)) t0(inner_c)) FROM (VALUES (2), (3)) t1(outer_c)"),
        [["1"], ["1"]]
    );
}

#[test]
fn nested_in_the_outer_querys_aggregate_it_is_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let e = s.run(&db, "SELECT sum((SELECT sum(a.x))) FROM (VALUES (1), (2)) a(x)", &[]).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("42803", "aggregate function calls cannot be nested"));
}

#[test]
fn a_subquerys_own_aggregate_keeps_group_by_matching() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    assert_eq!(
        run(
            &db,
            &mut s,
            "SELECT a.ten + (SELECT count(*) FROM onek) FROM onek a
             GROUP BY a.ten + (SELECT count(*) FROM onek) ORDER BY 1 LIMIT 2"
        ),
        [["1000"], ["1001"]]
    );
}

#[test]
fn a_volatile_subquery_runs_for_each_group() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE SEQUENCE s");
    assert_eq!(
        run(
            &db,
            &mut s,
            "SELECT (SELECT sum(a.x) + nextval('s')) FROM (VALUES (1, 1), (2, 1)) a(g, x) GROUP BY a.g ORDER BY 1"
        ),
        [["2"], ["3"]]
    );
}
