//! A ROWS or GROUPS frame offset may read the query's parameters, as Postgres allows: a SQL function's argument by
//! name (window.sql's unbounded_syntax_test1a), worked out when the window runs; a column is still refused.

use chronos::sql::{Session, Type, Value, parse};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn a_functions_argument_is_a_frame_offset() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE t (i int4); INSERT INTO t SELECT generate_series(1, 5)");
    run(
        &db,
        &mut s,
        "CREATE FUNCTION sums(x int) RETURNS TABLE (s bigint, i int) LANGUAGE SQL
         BEGIN ATOMIC SELECT sum(i) OVER (ORDER BY i ROWS BETWEEN x PRECEDING AND x FOLLOWING), i FROM t; END",
    );
    assert_eq!(
        run(&db, &mut s, "SELECT * FROM sums(1) ORDER BY i"),
        [["3", "1"], ["6", "2"], ["9", "3"], ["12", "4"], ["9", "5"]]
    );
    assert_eq!(run(&db, &mut s, "SELECT * FROM sums(0) ORDER BY i").len(), 5);
    // a column is not a constant: refused when the window runs
    let e = s.run(&db, "SELECT sum(i) OVER (ROWS BETWEEN i PRECEDING AND CURRENT ROW) FROM t", &[]).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("42P10", "argument of ROWS must not contain variables"));
}

#[test]
fn a_frame_offsets_parameter_is_a_bigint() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    run(&db, &mut s, "CREATE TABLE t (i int4); INSERT INTO t VALUES (1), (2), (3)");
    let sql = "SELECT sum(i) OVER (ORDER BY i ROWS $1 PRECEDING) FROM t";
    let st = &parse(sql).unwrap()[0];
    assert_eq!(st.params(&db, MAIN, &[]).unwrap(), [Type::BigInt]);
    let out = s.run(&db, sql, &[Value::Int(1)]).unwrap().pop().unwrap();
    let sums: Vec<String> = out.rows.iter().map(|r| r[0].to_text()).collect();
    assert_eq!(sums, ["1", "3", "5"]);
}
