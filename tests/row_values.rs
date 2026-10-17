//! Row values: `(a, b) IN (SELECT ...)`, `= ANY`, `<> ALL` and `= (SELECT ...)` against a subquery's rows,
//! `UPDATE ... SET (a, b) = (x, y)` or `= (SELECT ...)`, and a row as a value (`select t from t`, `row(1, 'a b')`,
//! `count(t)`, `t is null`, `t = t`, `(t).col`), which reads as the text Postgres writes a composite as. Postgres's
//! answers are compared in tests/pgdiff.rs (`row_values_against_subqueries_and_as_values`); these run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

/// What a query gives back, a row a string, its columns joined with `|` and booleans as `t`/`f`
/// the way psql prints them.
fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    let text = |v: &Value| match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Value::Bytea(b) => format!("\\x{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        v => v.to_text(),
    };
    out.last().unwrap().rows.iter().map(|r| r.iter().map(text).collect::<Vec<_>>().join("|")).collect()
}

/// Statements run in order in one session: a query's rows, or the SQLSTATE of the statement
/// Postgres refuses.
fn script(steps: &[(&str, Result<&[&str], &str>)]) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, want) in steps {
        match want {
            Ok(rows_want) => assert_eq!(&rows(&mut s, &db, q), rows_want, "{q}"),
            Err(code) => {
                let e = s.run(&db, q, &[]).expect_err(q);
                assert_eq!(&e.code, code, "{q}: {}", e.message);
            }
        }
    }
}

#[test]
fn row_values_answer_as_postgres_does() {
    script(&[
        (r"create table t1 (a int, b text, c int)", Ok(&[])),
        (r"create table t2 (a int, b text, x text)", Ok(&[])),
        (r"insert into t1 values (1,'one',10),(2,'two',20),(3,null,30)", Ok(&[])),
        (r"insert into t2 values (1,'one','p'),(2,'zz','q')", Ok(&[])),
        (r"select a from t1 where (a, b) = (select a, b from t2 where x = 'p')", Ok(&[r"1"])),
        (r"select a from t1 where (a, b) = (select a, 'one' from t2 where x = 'p') order by a", Ok(&[r"1"])),
        (r"select a from t1 where (a, b) <> (select a, b from t2 where x = 'p') order by a", Ok(&[r"2", r"3"])),
        (r"select a from t1 where (a, b) < (select a, b from t2 where x = 'q') order by a", Ok(&[r"1", r"2"])),
        (r"select a from t1 where (a, b) in (select a, b from t2) order by a", Ok(&[r"1"])),
        (r"select a from t1 where (a, b) not in (select a, b from t2) order by a", Ok(&[r"2", r"3"])),
        (r"select a from t1 where (a, b) = any (select a, b from t2) order by a", Ok(&[r"1"])),
        (r"select a from t1 where (a, b) = (select a, b from t2)", Err(r"21000")),
        (r"select a from t1 where (a, b) = (select a, b, x from t2 where x = 'p')", Err(r"42601")),
        (r"select a from t1 where (a) = (select a from t2 where x = 'p')", Ok(&[r"1"])),
        (r"select a from t1 where row(a, b) = (select a, b from t2 where x = 'p')", Ok(&[r"1"])),
        (r"select a from t1 where (a, b) = (select a, b from t2 where x = 'nope')", Ok(&[])),
        (r"update t1 set (a, b) = (select a + 10, 'upd' from t2 where x = 'p') where a = 1", Ok(&[])),
        (r"select a, b from t1 order by a", Ok(&[r"2|two", r"3|", r"11|upd"])),
        (r"update t1 set (a, b) = (select a + 100, b from t2 where x = 'nope') where a = 2", Ok(&[])),
        (r"select a, b from t1 order by a", Ok(&[r"3|", r"11|upd", r"|"])),
        (r"update t1 set (b, c) = (select x, a from t2 where t2.a = t1.a) where a in (2, 3)", Ok(&[])),
        (r"select a, b, c from t1 order by a", Ok(&[r"3||", r"11|upd|10", r"||20"])),
        (r"update t1 set (b, c) = ('q', 1), a = 7 where a = 3", Ok(&[])),
        (r"update t1 set (b, c) = (select x, a from t2) where a = 7", Err(r"21000")),
        (r"select t1 from t1 where a = 7", Ok(&[r"(7,q,1)"])),
        (r"select t1.* from t1 where a = 7", Ok(&[r"7|q|1"])),
        (r"select (t1).a from t1 where a = 7", Ok(&[r"7"])),
        (r"select count(t1), count(*) from t1", Ok(&[r"3|3"])),
        (r"select to_jsonb(t1) from t1 where a = 7", Ok(&[r#"{"a": 7, "b": "q", "c": 1}"#])),
        (r"select t1 is null, t1 is not null from t1 where a = 7", Ok(&[r"f|t"])),
        (r"select (a, b) is null, (a, b) is not null from t1", Ok(&[r"f|t", r"t|f", r"f|t"])),
        (r"select t1 = t1 from t1 limit 1", Ok(&[r"t"])),
        (r"select x from (select t1 as x from t1 where a = 7) s", Ok(&[r"(7,q,1)"])),
        (
            r"select (1, 'x') = (1, 'x'), row(1, 2) < row(1, 3), (1, null) = (1, 1), row(1, null) is null, row(null, null) is null, row(null, null) is not null",
            Ok(&[r"t|t||f|t|f"]),
        ),
        (r"select t1.*, t1 from t1 where a = 7", Ok(&[r"7|q|1|(7,q,1)"])),
        (r"select t1 = t1, t1 <> t1, t1 is null, t1 is not null from t1 where a = 7", Ok(&[r"t|f|f|t"])),
        (r"select count(t1) from t1 where a >= 7", Ok(&[r"2"])),
        (r"select t1 from t1 order by a", Ok(&[r"(7,q,1)", r"(11,upd,10)", r"(,,20)"])),
        (r"select t1 from t1 where t1 = t1 and a = 7", Ok(&[r"(7,q,1)"])),
        (r"select (t1).b, (t1).* from t1 where a = 7", Ok(&[r"q|7|q|1"])),
        (r"select x.a, x.b from (select t1.a, t1.b from t1 where a = 7) x", Ok(&[r"7|q"])),
        (r"select jsonb_build_object('k', t1) from t1 where a = 7", Ok(&[r#"{"k": {"a": 7, "b": "q", "c": 1}}"#])),
        (r"select string_agg(t1::text, ';') from t1 where a >= 7", Ok(&[r"(11,upd,10);(7,q,1)"])),
        (r"select row(t1.*)::text from t1 where a = 7", Ok(&[r"(7,q,1)"])),
        (r"select array_agg(t1::text) from t1", Ok(&[r#"{"(11,upd,10)","(,,20)","(7,q,1)"}"#])),
        (
            r#"select row(1, 'a b', null, true, 2.50, '{1,2}'::int[], 'q"x', '', 'a,b', 'a(b)')::text"#,
            Ok(&[r#"(1,"a b",,t,2.50,"{1,2}","q""x","","a,b","a(b)")"#]),
        ),
    ]);
}
