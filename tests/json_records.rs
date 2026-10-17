//! `jsonb_to_record`, `jsonb_to_recordset` (and the `json_` names): a JSON object, or array of objects, read
//! as rows by the column definition list in FROM (`f(..) AS t(a int, b text)`). Postgres's answers are compared
//! in tests/pgdiff.rs (`json_to_record_reads_objects_by_a_column_list`); these run without one.

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
fn json_to_record_reads_objects_by_a_column_list() {
    script(&[
        (r#"select * from jsonb_to_record('{"a":1,"b":"x"}') as r(a int, b text)"#, Ok(&[r"1|x"])),
        (r#"select * from jsonb_to_record('{"a":1}') as r(a int, b text)"#, Ok(&[r"1|"])),
        (r#"select * from jsonb_to_record('{"a":null,"b":"x"}') as r(a int, b text)"#, Ok(&[r"|x"])),
        (r#"select * from jsonb_to_record('{"a":"12","b":3}') as r(a int, b text)"#, Ok(&[r"12|3"])),
        (r#"select * from jsonb_to_record('{"a":"zz"}') as r(a int)"#, Err(r"22P02")),
        (r"select * from jsonb_to_record('[1,2]') as r(a int)", Err(r"22023")),
        (
            r#"select * from jsonb_to_record('{"a":{"k":1},"b":[1,2]}') as r(a jsonb, b jsonb)"#,
            Ok(&[r#"{"k": 1}|[1, 2]"#]),
        ),
        (
            r#"select * from jsonb_to_record('{"a":{"k":1},"b":[1,2]}') as r(a json, b text)"#,
            Ok(&[r#"{"k": 1}|[1, 2]"#]),
        ),
        (
            r#"select * from jsonb_to_record('{"a":true,"b":1.5,"c":"2020-01-02"}') as r(a boolean, b numeric, c date)"#,
            Ok(&[r"t|1.5|2020-01-02"]),
        ),
        (r#"select r.a, r.b from jsonb_to_record('{"a":1,"b":"x"}') as r(a int, b text) where r.a = 1"#, Ok(&[r"1|x"])),
        (
            r#"select * from jsonb_to_recordset('[{"a":1,"b":"x"},{"a":2,"b":"y"}]') as t(a int, b text) order by a"#,
            Ok(&[r"1|x", r"2|y"]),
        ),
        (
            r#"select * from jsonb_to_recordset('[{"a":1},{"b":"y"},{}]') as t(a int, b text)"#,
            Ok(&[r"1|", r"|y", r"|"]),
        ),
        (r#"select sum(a) from jsonb_to_recordset('[{"a":1},{"a":2},{"a":3}]') as t(a int)"#, Ok(&[r"6"])),
        (r#"select * from jsonb_to_recordset('{"a":1}') as t(a int)"#, Err(r"22023")),
        (r"select * from jsonb_to_recordset('[1,2]') as t(a int)", Err(r"22023")),
        (r"select * from jsonb_to_recordset('[]') as t(a int)", Ok(&[])),
        (r#"select * from json_to_record('{"a":1,"b":"x"}') as r(a int, b text)"#, Ok(&[r"1|x"])),
        (r#"select * from json_to_recordset('[{"a":1,"b":"x"}]') as r(a int, b text)"#, Ok(&[r"1|x"])),
        (r"select * from jsonb_to_record(null) as r(a int)", Ok(&[r""])),
        (r"select * from jsonb_to_recordset(null) as t(a int)", Ok(&[])),
        (
            r#"select x.id, t.a from (values (1, '[{"a":10},{"a":20}]'::jsonb)) as x(id, j), jsonb_to_recordset(x.j) as t(a int)"#,
            Ok(&[r"1|10", r"1|20"]),
        ),
        (
            r#"select x.id, t.a from (values (1, '{"a":10}'::jsonb), (2, '{"a":20}')) as x(id, j), lateral jsonb_to_record(x.j) as t(a int)"#,
            Ok(&[r"1|10", r"2|20"]),
        ),
        (r#"select * from jsonb_to_record('{"A":1,"a":2}') as r(a int)"#, Ok(&[r"2"])),
        (r#"select * from jsonb_to_record('{"a":1}') as r(a int, a int)"#, Err(r"42701")),
        (r#"select * from jsonb_to_record('{"a":1}')"#, Err(r"42601")),
        (r#"select * from jsonb_to_record('{"a":1.9}') as r(a int)"#, Err(r"22P02")),
        (r#"select * from jsonb_to_record('{"a":1.5}') as r(a numeric(5,0))"#, Ok(&[r"2"])),
        (r#"select * from jsonb_to_record('{"a":"abc"}') as r(a varchar(2))"#, Err(r"22001")),
        (r#"select * from jsonb_to_record('{"a":[1,2,3]}') as r(a text)"#, Ok(&[r"[1, 2, 3]"])),
        (
            r#"select * from jsonb_to_record('{"a":"2020-01-02 03:04:05"}') as r(a timestamp)"#,
            Ok(&[r"2020-01-02 03:04:05"]),
        ),
        (
            r#"select * from jsonb_to_record('{"a":1}') as r("a" bigint), jsonb_to_record('{"b":2}') as s(b int)"#,
            Ok(&[r"1|2"]),
        ),
    ]);
}
