//! A table's name as a value in to_json, json_agg and json_build_object is its whole row, as a JSON
//! object of its columns; arrays inside are JSON arrays. Postgres's answers are compared in
//! tests/pgdiff.rs (`whole_rows_as_json`); these run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn setup() -> (Db, Session) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (id int primary key, name text, tags text[], n int[])");
    run(&db, &mut s, "insert into t values (1, 'a', '{x,y}', '{1,2}'), (2, null, null, '{}')");
    (db, s)
}

#[test]
fn a_table_name_is_its_row() {
    let (db, mut s) = setup();
    let want = [
        (r#"select to_jsonb(t) from t where id = 1"#, r#"{"n": [1, 2], "id": 1, "name": "a", "tags": ["x", "y"]}"#),
        (r#"select to_jsonb(x) from t x where id = 2"#, r#"{"n": [], "id": 2, "name": null, "tags": null}"#),
        (
            r#"select row_to_json(t.*)::jsonb from t where id = 1"#,
            r#"{"n": [1, 2], "id": 1, "name": "a", "tags": ["x", "y"]}"#,
        ),
        (r#"select jsonb_agg(x order by x.id) -> 1 from t x"#, r#"{"n": [], "id": 2, "name": null, "tags": null}"#),
        (r#"select jsonb_build_object('r', x) -> 'r' -> 'name' from t x where id = 1"#, r#""a""#),
        (r#"select to_jsonb(tags), array_to_json(n)::jsonb from t where id = 1"#, r#"["x", "y"]"#),
    ];
    for (q, json) in want {
        assert_eq!(run(&db, &mut s, q)[0][0], json, "{q}");
    }
}

#[test]
fn a_column_named_as_its_table_is_the_column() {
    let (db, mut s) = setup();
    run(&db, &mut s, "create table u (u int, v int)");
    run(&db, &mut s, "insert into u values (7, 8)");
    assert_eq!(run(&db, &mut s, "select to_jsonb(u) from u")[0][0], "7");
    assert_eq!(run(&db, &mut s, "select to_jsonb(u.*) from u")[0][0], r#"{"u": 7, "v": 8}"#);
}

/// Outside JSON a bare table name is a row value, held as the text Postgres writes a composite as (tests/row_values.rs).
#[test]
fn a_bare_row_is_its_composite_text() {
    let (db, mut s) = setup();
    let rows = run(&db, &mut s, "select t from t order by id");
    assert_eq!(rows, [["(1,a,\"{x,y}\",\"{1,2}\")"], ["(2,,,{})"]]);
}
