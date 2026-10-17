//! `avg` of integers is numeric, as in Postgres (a string in node-postgres, exact); of floats it
//! stays double precision. And `pg_typeof`.

use chronos::sql::run;
use chronos::{Db, MAIN};

fn one(db: &Db, sql: &str) -> String {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    out.last().unwrap().rows[0][0].to_text()
}

fn db() -> Db {
    let db = Db::new();
    run(&db, MAIN, "create table u (id int primary key, floor int, big bigint, price numeric, f float8)", &[]).unwrap();
    run(&db, MAIN, "insert into u values (1, 1, 10, 1.5, 1.5), (2, 2, 20, 2.5, 2.5), (3, 10, 40, 2.5, 4.5)", &[])
        .unwrap();
    db
}

#[test]
fn avg_of_integers_is_numeric() {
    let db = db();
    assert_eq!(one(&db, "select pg_typeof(avg(floor)) from u"), "numeric");
    assert_eq!(one(&db, "select pg_typeof(avg(big)) from u"), "numeric");
    assert_eq!(one(&db, "select pg_typeof(avg(price)) from u"), "numeric");
    assert_eq!(one(&db, "select pg_typeof(avg(f)) from u"), "double precision");
    assert_eq!(one(&db, "select avg(floor) from u"), "4.3333333333333333");
    assert_eq!(one(&db, "select avg(big) from u"), "23.3333333333333333");
    assert_eq!(one(&db, "select avg(id) from u"), "2.0000000000000000");
    assert_eq!(one(&db, "select avg(f) from u"), "2.8333333333333335");
    assert_eq!(one(&db, "select avg(floor) from u where id > 100"), "");
}

#[test]
fn avg_of_integers_by_group_and_over_a_window() {
    let db = db();
    let out = run(&db, MAIN, "select id % 2, avg(floor) from u group by 1 order by 1", &[]).unwrap();
    let got: Vec<String> = out[0].rows.iter().map(|r| format!("{}|{}", r[0].to_text(), r[1].to_text())).collect();
    assert_eq!(got, ["0|2.0000000000000000", "1|5.5000000000000000"]);
    let out = run(&db, MAIN, "select avg(floor) over () from u", &[]).unwrap().pop().unwrap();
    assert_eq!(out.columns.unwrap()[0].1.name(), "numeric");
    let all: Vec<String> = out.rows.iter().map(|r| r[0].to_text()).collect();
    assert_eq!(all, ["4.3333333333333333"; 3]);
}

#[test]
fn pg_typeof_names_the_type() {
    let db = db();
    for (sql, want) in [
        ("select pg_typeof(1)", "integer"),
        ("select pg_typeof(1::bigint)", "bigint"),
        ("select pg_typeof(1.5)", "numeric"),
        ("select pg_typeof(1.5::float8)", "double precision"),
        ("select pg_typeof('a'::text)", "text"),
        ("select pg_typeof(true)", "boolean"),
        ("select pg_typeof(now())", "timestamp with time zone"),
        ("select pg_typeof(id + 1) from u limit 1", "integer"),
        ("select pg_typeof(price) from u limit 1", "numeric"),
    ] {
        assert_eq!(one(&db, sql), want, "{sql}");
    }
}
