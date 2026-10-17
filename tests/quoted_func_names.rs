//! A quoted lowercase name before `(` is a function call ("right"(x, 2)), as pg_dump and
//! pg_get_indexdef write a function named like a keyword.

use chronos::sql::run;
use chronos::{Db, MAIN};

fn one(db: &Db, sql: &str) -> String {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    out.last().unwrap().rows[0][0].to_text()
}

#[test]
fn quoted_names_call_functions() {
    let db = Db::new();
    assert_eq!(one(&db, "select \"right\"('abc', 2)"), "bc");
    assert_eq!(one(&db, "select \"lower\"('ABC')"), "abc");
    assert_eq!(
        one(&db, "select \"right\"(regexp_replace(('+91 98450-12345')::text, '\\D'::text, ''::text, 'g'::text), 10)"),
        "9845012345"
    );
}

#[test]
fn a_unique_index_on_a_quoted_call_is_kept_and_enforced() {
    let db = Db::new();
    run(&db, MAIN, "create table t (org int, e164 text)", &[]).unwrap();
    run(&db, MAIN, "create unique index u on t (org, \"right\"(regexp_replace((e164)::text, '\\D'::text, ''::text, 'g'::text), 10))", &[])
        .unwrap();
    run(&db, MAIN, "insert into t values (1, '+91 98450 12345')", &[]).unwrap();
    let dup = run(&db, MAIN, "insert into t values (1, '9845012345')", &[]).expect_err("same last 10 digits");
    assert_eq!(dup.code, "23505");
    run(&db, MAIN, "insert into t values (2, '9845012345')", &[]).unwrap();
}

#[test]
fn a_quoted_name_that_is_not_lowercase_or_not_a_call_stays_a_column() {
    let db = Db::new();
    run(&db, MAIN, "create table t (\"Right\" int, \"right\" int)", &[]).unwrap();
    run(&db, MAIN, "insert into t values (1, 2)", &[]).unwrap();
    assert_eq!(one(&db, "select \"right\" from t"), "2");
    assert_eq!(one(&db, "select \"Right\" from t"), "1");
}
