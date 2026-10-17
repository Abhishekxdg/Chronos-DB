//! SQL/JSON path in SQL: `jsonb_path_exists`, `_match`, `_query_array`, `_query_first` and `jsonb_path_query` (with their
//! `_tz` forms), and the operators `@?` and `@@`. Postgres's answers are compared in tests/pgdiff.rs
//! (`jsonpath_functions`); these run without one.

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
fn jsonpath_functions() {
    script(&[
        (r"create table docs (id int primary key, j jsonb)", Ok(&[])),
        (
            r#"insert into docs values (1, '{"a":1,"b":[1,2,3],"c":{"d":"x"}}'), (2, '{"items":[{"sku":"a","qty":2,"price":1.5},{"sku":"b","qty":5,"price":2.25}]}'), (3, '[1,2,3,4,5]')"#,
            Ok(&[]),
        ),
        (r"select jsonb_path_exists(j, '$.a') from docs order by id", Ok(&[r"t", r"f", r"f"])),
        (r"select jsonb_path_query_array(j, '$.b[*]') from docs where id=1", Ok(&[r"[1, 2, 3]"])),
        (r"select jsonb_path_query_first(j, '$.items[*].sku') from docs where id=2", Ok(&[r#""a""#])),
        (
            r"select jsonb_path_query(j, '$.items[*] ? (@.qty > 2)') from docs where id=2",
            Ok(&[r#"{"qty": 5, "sku": "b", "price": 2.25}"#]),
        ),
        (r"select jsonb_path_query(j, '$.items[*].price') from docs where id=2", Ok(&[r"1.5", r"2.25"])),
        (r"select id from docs where j @? '$.items[*] ? (@.qty > 4)' order by id", Ok(&[r"2"])),
        (r"select id from docs where j @@ '$.a == 1' order by id", Ok(&[r"1"])),
        (r"select j @@ '$.b[0] > 0' from docs where id=1", Ok(&[r"t"])),
        (r"select jsonb_path_match(j, '$.a == 1') from docs where id=1", Ok(&[r"t"])),
        (r"select jsonb_path_query_array(j, '$[*] ? (@ > 2)') from docs where id=3", Ok(&[r"[3, 4, 5]"])),
        (r"select jsonb_path_query_array(j, '$.items[*].qty * 2') from docs where id=2", Err(r"22038")),
        (r#"select jsonb_path_query_array('{"a":[1,2,3]}', '$.a[*] ? (@ > $min)', '{"min":1}')"#, Ok(&[r"[2, 3]"])),
        (r"select jsonb_path_query_array(j, '$.b.size()') from docs where id=1", Ok(&[r"[3]"])),
        (r"select jsonb_path_query_array(j, 'strict $.nope') from docs where id=1", Err(r"2203A")),
        (r"select jsonb_path_query_array(j, 'strict $.nope', '{}', true) from docs where id=1", Ok(&[r"[]"])),
        (r"select jsonb_path_exists(j, '$.nope') from docs where id=1", Ok(&[r"f"])),
        (r"select jsonb_path_exists(j, 'strict $.nope') from docs where id=1", Err(r"2203A")),
        (r"select jsonb_path_query_array(j, '$.a.type()') from docs where id=1", Ok(&[r#"["number"]"#])),
        (r"select jsonb_path_query_array(j, '$.**.d') from docs where id=1", Ok(&[r#"["x"]"#])),
        (r#"select jsonb_path_query(j, '$.c.d ? (@ like_regex "^x")') from docs where id=1"#, Ok(&[r#""x""#])),
        (r#"select jsonb_path_query(j, '$.c.d ? (@ starts with "x")') from docs where id=1"#, Ok(&[r#""x""#])),
        (r"select jsonb_path_query_array(j, '$.b[last]') from docs where id=1", Ok(&[r"[3]"])),
        (r"select jsonb_path_query_array(j, '$.b[1 to 2]') from docs where id=1", Ok(&[r"[2, 3]"])),
        (r"select jsonb_path_query_array('[1.5, 2.5]', '$[*].floor()')", Ok(&[r"[1, 2]"])),
        (r#"select jsonb_path_query_array('["2020-01-01"]', '$[*].datetime()')"#, Ok(&[r#"["2020-01-01"]"#])),
        (r#"select jsonb_path_query('{"a":1}', '$.a.double()')"#, Ok(&[r"1"])),
        (r#"select jsonb_path_exists('{"a":1}', '$.a ? (@ == 1 && @ < 2)')"#, Ok(&[r"t"])),
        (r#"select jsonb_path_exists('{"a":1}', '$.a ? (@ == 2 || @ == 1)')"#, Ok(&[r"t"])),
        (r#"select jsonb_path_query_array('{"a":null}', '$.a')"#, Ok(&[r"[null]"])),
        (r#"select jsonb_path_query_first('{"a":null}', '$.a')"#, Ok(&[r"null"])),
        (r#"select jsonb_path_query_first('{"a":1}', '$.b')"#, Ok(&[r""])),
        (r#"select jsonb_path_exists('{"a":1}', '$.a.b.c')"#, Ok(&[r"f"])),
        (r#"select jsonb_path_exists('{"a":1}', '$[')"#, Err(r"42601")),
        (r#"select jsonb_path_exists('{"a":1}', 'lax $.a', '[]')"#, Err(r"22023")),
        (r#"select jsonb_path_match('{"a":1}', '$.a')"#, Err(r"22038")),
        (r#"select '{"a":1}'::jsonb @? '$.a'"#, Ok(&[r"t"])),
        (r#"select '{"a":1}'::jsonb @@ '$.a == 1'"#, Ok(&[r"t"])),
        (r"select '$.a'::jsonpath is null", Ok(&[r"f"])),
        (r"select jsonb_path_query_array(null, '$.a'), jsonb_path_exists('{}', null)", Ok(&[r"|"])),
        (r"select x from jsonb_path_query('[1,2,3]', '$[*]') x", Ok(&[r"1", r"2", r"3"])),
        (r"select jsonb_path_query('[1,2,3]', '$[*] ? (@ > 1)') order by 1", Ok(&[r"2", r"3"])),
        (
            r"select id, jsonb_path_query_first(j, '$.items[0].sku') from docs order by id",
            Ok(&[r"1|", r#"2|"a""#, r"3|"]),
        ),
        (
            r"select jsonb_path_query_array(j, '$.keyvalue()') from docs where id=1",
            Ok(&[
                r#"[{"id": 0, "key": "a", "value": 1}, {"id": 0, "key": "b", "value": [1, 2, 3]}, {"id": 0, "key": "c", "value": {"d": "x"}}]"#,
            ]),
        ),
        (
            r#"select jsonb_path_query_array('{"a":1}', '$.keyvalue()')"#,
            Ok(&[r#"[{"id": 0, "key": "a", "value": 1}]"#]),
        ),
        (r#"select jsonb_path_query('{"a":"abc"}', '$.a ? (@ == "abc")')"#, Ok(&[r#""abc""#])),
        (r"select jsonb_path_query_array('[1,2,3]', '$[*] ? (@ != 2)')", Ok(&[r"[1, 3]"])),
        (r"select jsonb_path_exists('[1,2,3]', '$[*] ? (@ == 4)')", Ok(&[r"f"])),
        (
            r#"select jsonb_path_exists_tz('["2020-01-01"]', '$[*] ? (@.datetime() < "2021-01-01 00:00:00+00".datetime())')"#,
            Ok(&[r"t"]),
        ),
        (
            r#"select jsonb_path_exists('["2020-01-01"]', '$[*] ? (@.datetime() < "2021-01-01 00:00:00+00".datetime())', '{}', true)"#,
            Err(r"0A000"),
        ),
        (
            r#"select jsonb_path_query_array_tz('["2020-01-01 10:00:00"]', '$[*].datetime()')"#,
            Ok(&[r#"["2020-01-01T10:00:00"]"#]),
        ),
        (r"select jsonb_path_query_first_tz('[1,2]', '$[*]')", Ok(&[r"1"])),
        (r"select jsonb_path_match_tz('[1]', '$[0] == 1')", Ok(&[r"t"])),
        (r"select jsonb_path_query_tz('[1,2]', '$[*]')", Ok(&[r"1", r"2"])),
    ]);
}
