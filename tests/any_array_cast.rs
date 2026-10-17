//! `x = ANY(ARRAY[...]::type[])`: a cast (or anything else) after the array constructor.

use chronos::sql::Value;
use chronos::sql::run;
use chronos::{Db, MAIN};

fn one(sql: &str, params: &[Value]) -> String {
    let db = Db::new();
    let out = run(&db, MAIN, sql, params).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    out.last().unwrap().rows[0][0].to_text()
}

#[test]
fn any_and_all_over_a_cast_array_constructor() {
    for (sql, want) in [
        ("select 'a' = any(array['a','b']::text[])", "true"),
        ("select 'c' = any(array['a','b']::text[])", "false"),
        ("select 'c' <> all(array['a','b']::text[])", "true"),
        ("select 2 = any(array[1,2]::int[])", "true"),
        ("select 3 > all(array[1,2]::int[])", "true"),
        ("select 'b' = any((array['a','b'])::text[])", "true"),
        ("select 'a' = any(array['a','b'])", "true"),
    ] {
        assert_eq!(one(sql, &[]), want, "{sql}");
    }
}

#[test]
fn bound_parameters_in_a_cast_array() {
    let p = [Value::Text("x".into()), Value::Text("y".into())];
    assert_eq!(one("select 'y' = any(array[$1, $2]::text[])", &p), "true");
    assert_eq!(one("select 'z' = any(array[$1, $2]::text[])", &p), "false");
}
