//! Parameters used only inside a WITH query, a derived table or a function's arguments in FROM
//! are counted (the extended protocol refused `$1` there: "1 parameters given, but the statement
//! takes 0").

use chronos::sql::{Type, Value, parse, run};
use chronos::{Db, MAIN};

fn n_params(db: &Db, sql: &str) -> usize {
    let st = &parse(sql).unwrap_or_else(|e| panic!("{sql}: {}", e.message))[0];
    st.params(db, MAIN, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).len()
}

#[test]
fn parameters_inside_sources_are_counted() {
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, n int)", &[]).unwrap();
    for (sql, want) in [
        ("with a as (select $1::int as x) select x from a", 1),
        ("with a as (select $1::int as x), b as (select $2::int as y) select x, y from a, b", 2),
        ("with a as (select $1::int as x) select x, $2::int from a", 2),
        ("select x from (select $1::int as x) d", 1),
        ("select * from t join (select $1::int as k) d on d.k = t.id", 1),
        ("select x from generate_series(1, $1) x", 1),
        ("with recursive r(i) as (select 1 union all select i + 1 from r where i < $1) select i from r", 1),
        ("select 1 union select x from (select $1::int as x) d", 1),
        ("insert into t select $1::int, 0", 1),
        ("insert into t (id, n) with a as (select $1::int as x) select x, $2::int from a", 2),
    ] {
        assert_eq!(n_params(&db, sql), want, "{sql}");
    }
}

#[test]
fn a_cte_parameter_runs_and_takes_its_type_from_use() {
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, n int)", &[]).unwrap();
    run(&db, MAIN, "insert into t values (1, 10), (2, 20)", &[]).unwrap();
    let sql = "with a as (select n from t where id = $1) select n from a";
    let st = &parse(sql).unwrap()[0];
    assert_eq!(st.params(&db, MAIN, &[]).unwrap(), [Type::Int]);
    let out = run(&db, MAIN, sql, &[Value::Int(2)]).unwrap().pop().unwrap();
    assert_eq!(out.rows[0][0].to_text(), "20");
}
