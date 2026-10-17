//! Small pieces of syntax and where they refuse (what they answer is checked against Postgres by
//! `pgdiff.rs`, `array_slices_and_small_operators`).

use chronos::sql::run;
use chronos::{Db, MAIN};

fn code(db: &Db, sql: &str) -> &'static str {
    run(db, MAIN, sql, &[]).expect_err(sql).code
}

#[test]
fn what_would_answer_differently_is_refused() {
    let db = Db::new();
    // (a subscript after a slice is a slice of a second dimension, `{}` here as in Postgres: arrays have their
    // dimensions now, and pgdiff's `multidimensional_arrays` compares it)
    let out = run(&db, MAIN, "select (array[1,2,3])[1:2][1]", &[]).unwrap();
    assert_eq!(out[0].rows[0][0].to_text(), "{}");
    // collations: an ICU locale ICU4X has no rules for is refused rather than sorted as the root locale (a libc
    // locale's name is the ICU collation of that locale)
    assert_eq!(code(&db, "select 'a' < 'b' collate \"se-x-icu\""), "0A000");
    // names only for functions whose parameters are known
    assert_eq!(code(&db, "select lower(s => 'A')"), "42883");
    assert_eq!(code(&db, "select make_interval(weeks := 2, 3)"), "42601");
}

#[test]
fn named_arguments_reach_functions_made_with_create_function() {
    let db = Db::new();
    run(&db, MAIN, "create function pow2(base int, exp int) returns int language sql as 'select base * exp'", &[])
        .unwrap();
    let one = |sql: &str| run(&db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"))[0].rows[0][0].to_text();
    assert_eq!(one("select pow2(exp => 5, base => 2)"), "10");
    assert_eq!(one("select pow2(3, exp => 4)"), "12");
    // in a query over rows
    run(&db, MAIN, "create table t (id int primary key)", &[]).unwrap();
    run(&db, MAIN, "insert into t (id) values (1)", &[]).unwrap();
    assert_eq!(one("select sum(pow2(exp => id, base => 10)) from t"), "10");
    assert_eq!(code(&db, "select pow2(base => 1)"), "42883");
    assert_eq!(code(&db, "select pow2(base => 1, nope => 2)"), "42883");
}
