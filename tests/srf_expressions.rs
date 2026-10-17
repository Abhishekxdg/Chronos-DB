//! Set-returning functions inside select-list expressions. Postgres's answers are compared in
//! tests/pgdiff.rs (`set_returning_functions_in_expressions`); these run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect::<Vec<_>>().join("|")).collect()
}

#[test]
fn an_expression_around_a_set_returning_function_is_per_row() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(rows(&mut s, &db, "select generate_series(1, 3) * 2"), ["2", "4", "6"]);
    assert_eq!(rows(&mut s, &db, "select generate_series(1, 2)::text || 'z'"), ["1z", "2z"]);
    assert_eq!(rows(&mut s, &db, "select upper(unnest(array['x', 'y']))"), ["X", "Y"]);
    assert_eq!(rows(&mut s, &db, "select upper(regexp_split_to_table('a,b', ','))"), ["A", "B"]);
    assert_eq!(rows(&mut s, &db, "select jsonb_array_elements('[1, 2]')::text || '!'"), ["1!", "2!"]);
    assert_eq!(rows(&mut s, &db, "select generate_series(1, 0) * 2"), Vec::<String>::new());
    s.run(&db, "create table t (id int primary key, arr int[])", &[]).unwrap();
    s.run(&db, "insert into t values (1, '{10,20}'), (2, '{5}'), (3, '{}')", &[]).unwrap();
    assert_eq!(rows(&mut s, &db, "select id, unnest(arr) + id from t order by 1, 2"), ["1|11", "1|21", "2|7"]);
    assert_eq!(rows(&mut s, &db, "select unnest(arr)::text from t where id = 1"), ["10", "20"]);
}

#[test]
fn several_run_side_by_side_and_the_shorter_is_padded() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(rows(&mut s, &db, "select generate_series(1, 3), generate_series(10, 20, 10)"), ["1|10", "2|20", "3|"]);
    assert_eq!(rows(&mut s, &db, "select generate_series(1, 3) + generate_series(1, 3)"), ["2", "4", "6"]);
}

#[test]
fn a_lone_json_value_is_an_array_of_one() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(rows(&mut s, &db, "select '1'::jsonb -> 0, '1'::jsonb -> -1, '1'::jsonb ->> 0"), ["1|1|1"]);
    assert_eq!(rows(&mut s, &db, "select '1'::jsonb -> 1 is null"), ["true"]);
}

#[test]
fn not_in_where() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let e = s.run(&db, "select 1 where generate_series(1, 2) = 1", &[]).unwrap_err();
    assert_eq!(e.code, "0A000");
}
