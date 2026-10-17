//! Unaliased output columns are named as Postgres's FigureColname names them: a cast takes its
//! operand's name, CASE / EXISTS / ARRAY their keyword.

use chronos::sql::run;
use chronos::{Db, MAIN};

fn names(db: &Db, sql: &str) -> Vec<String> {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap();
    out.columns.unwrap().into_iter().map(|c| c.0).collect()
}

fn db() -> Db {
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, code text, price numeric, created_at timestamptz)", &[])
        .unwrap();
    db
}

#[test]
fn a_cast_takes_its_operands_name() {
    let db = db();
    for (sql, want) in [
        ("select id::text from t", vec!["id"]),
        ("select u.code::text from t u", vec!["code"]),
        ("select (price)::int from t", vec!["price"]),
        ("select created_at::date from t", vec!["created_at"]),
        ("select id::text::int from t", vec!["id"]),
        ("select cast(id as text) from t", vec!["id"]),
        ("select id::text, code::text from t", vec!["id", "code"]),
        ("select now()::date", vec!["now"]),
        ("select id::text as x from t", vec!["x"]),
    ] {
        assert_eq!(names(&db, sql), want, "{sql}");
    }
}

#[test]
fn a_cast_of_a_constant_takes_the_types_name() {
    let db = db();
    for (sql, want) in [
        ("select 1::int", "int4"),
        ("select 1::bigint", "int8"),
        ("select 'a'::text", "text"),
        ("select 1.5::float8", "float8"),
        ("select true::bool", "bool"),
    ] {
        assert_eq!(names(&db, sql), [want], "{sql}");
    }
}

#[test]
fn case_exists_and_array_are_named_by_keyword() {
    let db = db();
    assert_eq!(names(&db, "select case when id > 1 then 'a' else 'b' end from t"), ["case"]);
    assert_eq!(names(&db, "select exists(select 1)"), ["exists"]);
    assert_eq!(names(&db, "select array[1, 2]"), ["array"]);
    // not a keyword or a name: unchanged
    assert_eq!(names(&db, "select id + 1, not exists(select 1), 1 from t"), ["?column?", "?column?", "?column?"]);
}

/// A call of a function made in a schema is named by the function's plain name, however the search_path finds it.
#[test]
fn a_function_in_a_schema_takes_its_plain_name() {
    let db = Db::new();
    let mut s = chronos::sql::Session::new(MAIN);
    for q in [
        "create schema cna",
        "create schema cnb",
        "create function cna.f(int) returns int language sql as 'select 1'",
        "create function cnb.f(text) returns int language sql as 'select 2'",
        "create function cnb.lower(int) returns int language sql as 'select 3'",
        "set search_path = cna, cnb",
    ] {
        s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {}", e.message));
    }
    for (sql, want) in [
        ("select f(1), f('x'), cnb.f('y')", vec!["f", "f", "f"]),
        ("select lower(1), lower('A')", vec!["lower", "lower"]),
    ] {
        let out = s.run(&db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap();
        assert_eq!(out.columns.unwrap().into_iter().map(|c| c.0).collect::<Vec<_>>(), want, "{sql}");
    }
}
