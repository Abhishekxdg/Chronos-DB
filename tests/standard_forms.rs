//! SQL-standard spellings: BETWEEN SYMMETRIC, TABLE t, MATERIALIZED hints on WITH queries and IS
//! [NOT] NULL of a row. Postgres's answers are compared in tests/pgdiff.rs (`standard_syntax_forms`);
//! these run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn rows(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn first(db: &Db, s: &mut Session, q: &str) -> String {
    rows(db, s, q)[0].join("|")
}

#[test]
fn between_symmetric_takes_the_bounds_in_either_order() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(first(&db, &mut s, "select 5 between symmetric 10 and 1, 5 between 10 and 1"), "true|false");
    assert_eq!(
        first(&db, &mut s, "select 5 not between symmetric 10 and 1, 0 between symmetric 10 and 1"),
        "false|false"
    );
    assert_eq!(first(&db, &mut s, "select 5 between asymmetric 1 and 10"), "true");
}

#[test]
fn table_is_select_star() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&db, &mut s, "create table t (id int primary key, v text)");
    rows(&db, &mut s, "insert into t values (1, 'a'), (2, 'b')");
    assert_eq!(rows(&db, &mut s, "table t order by id desc limit 1"), [["2", "b"]]);
    assert_eq!(first(&db, &mut s, "select count(*) from (table t) x"), "2");
    assert_eq!(first(&db, &mut s, "select count(*) from (table t union all table t) x"), "4");
}

#[test]
fn materialized_is_a_hint_only() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(first(&db, &mut s, "with c as materialized (select 1 as a) select a from c"), "1");
    assert_eq!(first(&db, &mut s, "with c as not materialized (select 2 as a) select a from c"), "2");
}

#[test]
fn a_row_is_null_when_every_field_is() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let q = "select row(null, null) is null, row(1, null) is null, row(1, null) is not null, row(1, 2) is not null";
    assert_eq!(first(&db, &mut s, q), "true|false|false|true");
}

#[test]
fn fetch_with_ties_adds_the_rows_that_tie_with_the_last() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&db, &mut s, "create table t (id int primary key, v int)");
    rows(&db, &mut s, "insert into t values (1, 10), (2, 20), (3, 20), (4, 30), (5, 30), (6, NULL)");
    let col = |q: &str, s: &mut Session| rows(&db, s, q).into_iter().map(|r| r[0].clone()).collect::<Vec<_>>();
    assert_eq!(col("select v from t order by v fetch first 2 rows with ties", &mut s), ["10", "20", "20"]);
    assert_eq!(col("select v from t order by v fetch first 1 row only", &mut s), ["10"]);
    assert_eq!(col("select v from t order by v offset 1 rows fetch first 1 rows with ties", &mut s), ["20", "20"]);
    // the key need not be selected
    assert_eq!(col("select count(*) from (select id from t order by v fetch first 4 rows with ties) x", &mut s), ["5"]);
    assert_eq!(col("select v from t order by v fetch first 0 rows with ties", &mut s), Vec::<String>::new());
    let e = s.run(&db, "select v from t fetch first 1 row with ties", &[]).unwrap_err();
    assert_eq!(e.code, "42601");
}

#[test]
fn with_ordinality_numbers_a_functions_rows() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let q = "select x, n from unnest(array['a', 'b', 'c']) with ordinality as t(x, n) order by n desc";
    assert_eq!(rows(&db, &mut s, q), [["c", "3"], ["b", "2"], ["a", "1"]]);
    // the column is `ordinality` unless the alias names it; the function's own column has the alias's name
    assert_eq!(rows(&db, &mut s, "select t, ordinality from unnest(array['a']) with ordinality t"), [["a", "1"]]);
    assert_eq!(first(&db, &mut s, "select sum(ordinality) from generate_series(10, 12) with ordinality"), "6");
    // after a table, it is read for each of its rows
    rows(&db, &mut s, "create table t (id int primary key, tags text[])");
    rows(&db, &mut s, "insert into t values (1, '{x,y}'), (2, '{z}')");
    let q = "select id, tag, n from t, unnest(tags) with ordinality as u(tag, n) order by id, n";
    assert_eq!(rows(&db, &mut s, q), [["1", "x", "1"], ["1", "y", "2"], ["2", "z", "1"]]);
    // and inside a subquery, which reads the outer row through a subquery of its own
    let q = "select id, (select count(*) from (select * from unnest(tags) u) z) from t order by id";
    assert_eq!(rows(&db, &mut s, q), [["1", "2"], ["2", "1"]]);
    assert_eq!(first(&db, &mut s, "select * from lateral (select 1 as a) t"), "1");
}
