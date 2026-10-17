//! `||` on values that are not text: `jsonb || 'literal'` reads the literal as jsonb (merge or append), two
//! typed numbers or booleans are `42883` rather than silently text, and `jsonb_array_elements(..) as i` names its
//! column `i`. Postgres's answers are compared in tests/pgdiff.rs (`concat_operator_by_type`); these run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

/// What a query gives back, a row a string, its columns joined with `|` and booleans as `t`/`f`
/// the way psql prints them.
fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    let text = |v: &Value| match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
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
fn concat_operator() {
    script(&[
        (r"create table ev (id int primary key, payload jsonb)", Ok(&[])),
        (r#"insert into ev values (1, '{"a":1}'), (2, '{"items":[{"sku":"x"},{"sku":"y"}]}')"#, Ok(&[])),
        (r#"select '{"a":1}'::jsonb || '{"b":2}'"#, Ok(&[r#"{"a": 1, "b": 2}"#])),
        (r#"select '{"a":1}' || '{"b":2}'::jsonb"#, Ok(&[r#"{"a": 1, "b": 2}"#])),
        (r"select '[1,2]'::jsonb || '[3]'", Ok(&[r"[1, 2, 3]"])),
        (r#"update ev set payload = payload || '{"verified":true}' where id = 1"#, Ok(&[])),
        (
            r"select id, payload from ev order by id",
            Ok(&[r#"1|{"a": 1, "verified": true}"#, r#"2|{"items": [{"sku": "x"}, {"sku": "y"}]}"#]),
        ),
        (r"select i->>'sku' from ev, jsonb_array_elements(payload->'items') as i order by 1", Ok(&[r"x", r"y"])),
        (r"select value from jsonb_array_elements('[1,2]'::jsonb)", Ok(&[r"1", r"2"])),
        (r"select v from jsonb_array_elements('[1,2]'::jsonb) as t(v)", Ok(&[r"1", r"2"])),
        (r"select i from jsonb_array_elements('[1,2]'::jsonb) as i", Ok(&[r"1", r"2"])),
        (r#"select value from jsonb_array_elements_text('["a","b"]'::jsonb)"#, Ok(&[r"a", r"b"])),
        (r#"select t from jsonb_array_elements_text('["a","b"]'::jsonb) as t"#, Ok(&[r"a", r"b"])),
        (r"select 1 || 2", Err(r"42883")),
        (r"select 1 || '2'", Ok(&[r"12"])),
        (r"select 'a' || 1", Ok(&[r"a1"])),
        (r"select 1.5 || 2.5", Err(r"42883")),
        (r"select true || false", Err(r"42883")),
        (r"select 1 || null", Ok(&[r""])),
        (r"select (1 + 1) || 'x'", Ok(&[r"2x"])),
        (r"select array[1,2] || 3", Ok(&[r"{1,2,3}"])),
        (r"select array[1,2] || array[3]", Ok(&[r"{1,2,3}"])),
        (r"select 'a' || 'b'", Ok(&[r"ab"])),
        (r"select 1::int || 2::int", Err(r"42883")),
        (r"select 1::bigint || 2::int", Err(r"42883")),
        (r"select 1::int || 'x'::text", Ok(&[r"1x"])),
        (r"select null || null", Ok(&[r""])),
        (r"select ('a'::text) || ('b'::varchar)", Ok(&[r"ab"])),
    ]);
}
