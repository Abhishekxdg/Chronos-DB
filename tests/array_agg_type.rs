//! `array_agg(x)` is an array of x's type (so a driver returns an array, not text), and aggregate
//! columns are named as Postgres names them.

use chronos::sql::{Type, run};
use chronos::{Db, MAIN};

fn cols(db: &Db, sql: &str) -> Vec<(String, String)> {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap();
    out.columns.unwrap().into_iter().map(|c| (c.0, c.1.pg_name().into_owned())).collect()
}

fn db() -> Db {
    let db = Db::new();
    run(&db, MAIN, "create table u (id int primary key, code text, n bigint, p numeric, ok boolean)", &[]).unwrap();
    run(&db, MAIN, "insert into u values (1, 'T1-1001', 5, 1.5, true), (2, 'T1-1002', 6, 2.5, false)", &[]).unwrap();
    db
}

#[test]
fn array_agg_reports_its_element_array_type() {
    let db = db();
    for (sql, ty) in [
        ("select array_agg(code order by code) from u", "text[]"),
        ("select array_agg(id) from u", "integer[]"),
        ("select array_agg(n) from u", "bigint[]"),
        ("select array_agg(p) from u", "numeric[]"),
        ("select array_agg(ok) from u", "boolean[]"),
        ("select array_agg(distinct id) from u", "integer[]"),
    ] {
        assert_eq!(cols(&db, sql), [("array_agg".to_string(), ty.to_string())], "{sql}");
    }
    let out = run(&db, MAIN, "select array_agg(code order by code) from u", &[]).unwrap().pop().unwrap();
    assert_eq!(out.columns.unwrap()[0].1.oid(), Type::Array(Type::Text.oid()).oid());
    assert_eq!(out.rows[0][0].to_text(), "{T1-1001,T1-1002}");
}

#[test]
fn aggregate_columns_are_named_as_postgres_names_them() {
    let db = db();
    let names = |sql: &str| cols(&db, sql).into_iter().map(|c| c.0).collect::<Vec<_>>();
    assert_eq!(
        names("select count(*), sum(id), avg(id), min(id), max(id), bool_and(ok), bool_or(ok) from u"),
        ["count", "sum", "avg", "min", "max", "bool_and", "bool_or"]
    );
    assert_eq!(
        names("select string_agg(code, ','), array_agg(id), json_agg(id), json_object_agg(code, id) from u"),
        ["string_agg", "array_agg", "json_agg", "json_object_agg"]
    );
    assert_eq!(names("select array_agg(id) as ids from u"), ["ids"]);
}
