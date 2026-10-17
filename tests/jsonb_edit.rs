//! jsonb editing (`-`, `#-`, `jsonb_insert`, `jsonb_pretty`, `jsonb_set_lax`) and numbers that keep the form they were written in (`7.50`). Postgres's answers are compared in tests/pgdiff.rs (`jsonb_delete_insert_pretty_and_set_lax`);
//! these expectations, all taken from a real Postgres 16, run without one.

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
fn jsonb_delete_insert_pretty_and_set_lax() {
    script(&[
        (
            r#"select '{"a":1,"b":2}'::jsonb - 'a', '[1,2,3]'::jsonb - 1, '["a","b","a"]'::jsonb - 'a', '[1,2,3]'::jsonb - -1, '[1,2,3]'::jsonb - 5, '[1,2,3]'::jsonb - -5"#,
            Ok(&[r#"{"b": 2}|[1, 3]|["b"]|[1, 2]|[1, 2, 3]|[1, 2, 3]"#]),
        ),
        (
            r#"select '{"a":1,"b":2,"c":3}'::jsonb - array['a','b'], '{"a":1}'::jsonb - 'z', '["a","b","c"]'::jsonb - array['a','c'], '{"a":1}'::jsonb - array[]::text[]"#,
            Ok(&[r#"{"c": 3}|{"a": 1}|["b"]|{"a": 1}"#]),
        ),
        (r#"select '{"a":1}'::jsonb - 1"#, Err(r"22023")),
        (r"select '1'::jsonb - 'a'", Err(r"22023")),
        (r#"select '"x"'::jsonb - 1"#, Err(r"22023")),
        (r"select 'null'::jsonb - array['a']", Err(r"22023")),
        (r#"select '[1,"1",1]'::jsonb - '1', '["1"]'::jsonb - '1', '[[1],[2]]'::jsonb - 0"#, Ok(&[r"[1, 1]|[]|[[2]]"])),
        (
            r#"select '{"a":{"b":[1,2]}}'::jsonb #- '{a,b,0}', '{"a":{"b":1}}'::jsonb #- '{a,b}', '{"a":1}'::jsonb #- '{x,y}', '[1,2,3]'::jsonb #- '{-1}', '[1,2,3]'::jsonb #- '{5}', '{"a":1}'::jsonb #- '{}'"#,
            Ok(&[r#"{"a": {"b": [2]}}|{"a": {}}|{"a": 1}|[1, 2]|[1, 2, 3]|{"a": 1}"#]),
        ),
        (r"select '[1,2]'::jsonb #- '{x}'", Err(r"22P02")),
        (r"select '1'::jsonb #- '{a}'", Err(r"22023")),
        (r#"select '{"a":[1]}'::jsonb #- '{a,x}'"#, Err(r"22P02")),
        (r#"select '{"a":1}'::jsonb #- null::text[]"#, Ok(&[r""])),
        (
            r#"select '{"a":{"b":{"c":1,"d":2}}}'::jsonb #- '{a,b,c}', '{"a":[{"x":1},{"y":2}]}'::jsonb #- '{a,1,y}'"#,
            Ok(&[r#"{"a": {"b": {"d": 2}}}|{"a": [{"x": 1}, {}]}"#]),
        ),
        (
            r#"select jsonb_insert('{"a":[0,1]}', '{a,1}', '9'), jsonb_insert('{"a":[0,1]}', '{a,1}', '9', true), jsonb_insert('{"a":1}', '{b}', '2'), jsonb_insert('[0,1]', '{-1}', '9'), jsonb_insert('[0,1]', '{-1}', '9', true), jsonb_insert('[0,1]', '{5}', '9'), jsonb_insert('[0,1]', '{-9}', '9'), jsonb_insert('[0,1]', '{5}', '9', true)"#,
            Ok(&[
                r#"{"a": [0, 9, 1]}|{"a": [0, 1, 9]}|{"a": 1, "b": 2}|[0, 9, 1]|[0, 1, 9]|[0, 1, 9]|[9, 0, 1]|[0, 1, 9]"#,
            ]),
        ),
        (r#"select jsonb_insert('{"a":1}', '{a}', '2')"#, Err(r"22023")),
        (r"select jsonb_insert('1', '{a}', '2')", Err(r"22023")),
        (r"select jsonb_insert('[1]', '{x}', '2')", Err(r"22P02")),
        (
            r#"select jsonb_insert('{"a":{"b":1}}', '{a,c}', '2'), jsonb_insert('{"a":1}', '{x,y}', '2'), jsonb_insert('{}', '{}', '2')"#,
            Ok(&[r#"{"a": {"b": 1, "c": 2}}|{"a": 1}|{}"#]),
        ),
        (
            r#"select jsonb_insert('{"a":[1]}', '{a,0}', 'null'), jsonb_insert('{"a":[1]}', '{a,0}', '{"k":[1,2]}', true)"#,
            Ok(&[r#"{"a": [null, 1]}|{"a": [1, {"k": [1, 2]}]}"#]),
        ),
        (
            r#"select replace(jsonb_pretty('{"a":[1,{"b":2}],"c":"x"}'), chr(10), '/'), replace(jsonb_pretty('[]'), chr(10), '/'), replace(jsonb_pretty('{}'), chr(10), '/'), replace(jsonb_pretty('1'), chr(10), '/'), replace(jsonb_pretty('"s"'), chr(10), '/'), replace(jsonb_pretty('null'), chr(10), '/')"#,
            Ok(&[
                r#"{/    "a": [/        1,/        {/            "b": 2/        }/    ],/    "c": "x"/}|[/]|{/}|1|"s"|null"#,
            ]),
        ),
        (
            r#"select replace(jsonb_pretty('{"bb":1,"a":[],"c":{},"aa":[[1,2],[]]}'), chr(10), '/')"#,
            Ok(&[
                r#"{/    "a": [/    ],/    "c": {/    },/    "aa": [/        [/            1,/            2/        ],/        [/        ]/    ],/    "bb": 1/}"#,
            ]),
        ),
        (
            r#"select replace(jsonb_pretty('[1,[2,[3,{"x":null}]]]'), chr(10), '/')"#,
            Ok(&[
                r#"[/    1,/    [/        2,/        [/            3,/            {/                "x": null/            }/        ]/    ]/]"#,
            ]),
        ),
        (r"select replace(jsonb_pretty(null), chr(10), '/')", Ok(&[r""])),
        (
            r#"select jsonb_set_lax('{"a":1}', '{a}', null, true, 'delete_key'), jsonb_set_lax('{"a":1}', '{a}', null), jsonb_set_lax('{"a":1}', '{a}', null, true, 'return_target'), jsonb_set_lax('{"a":1}', '{a}', null, true, 'use_json_null'), jsonb_set_lax('{"a":1}', '{b}', '2', false), jsonb_set_lax('{"a":1}', '{b}', '2')"#,
            Ok(&[r#"{}|{"a": null}|{"a": 1}|{"a": null}|{"a": 1}|{"a": 1, "b": 2}"#]),
        ),
        (r#"select jsonb_set_lax('{"a":1}', '{a}', null, true, 'raise_exception')"#, Err(r"22004")),
        (r#"select jsonb_set_lax('{"a":1}', '{a}', null, true, 'bogus')"#, Err(r"22023")),
        (
            r"select jsonb_set_lax(null, '{a}', '1'), jsonb_set_lax('{}', null, '1'), jsonb_set_lax('{}', '{a}', '1', null)",
            Ok(&[r"||"]),
        ),
        (r"create table jt (id int primary key, doc jsonb)", Ok(&[])),
        (r#"insert into jt values (1, '{"a":1,"b":[1,2],"c":{"d":1}}'), (2, '[1,2,3]'), (3, null)"#, Ok(&[])),
        (r"select id, doc - 'a', doc #- '{c,d}' from jt order by id", Err(r"22P02")),
        (
            r"select id, replace(jsonb_pretty(doc), chr(10), '/') from jt where id = 1",
            Ok(&[r#"1|{/    "a": 1,/    "b": [/        1,/        2/    ],/    "c": {/        "d": 1/    }/}"#]),
        ),
        (r#"select id from jt where doc - 'a' = '{"b":[1,2],"c":{"d":1}}'"#, Ok(&[r"1"])),
        (r"update jt set doc = doc - 'a' where id = 1", Ok(&[])),
        (r"select doc from jt where id = 1", Ok(&[r#"{"b": [1, 2], "c": {"d": 1}}"#])),
        (r"update jt set doc = doc #- '{b,0}' where id = 1", Ok(&[])),
        (r"select doc from jt where id = 1", Ok(&[r#"{"b": [2], "c": {"d": 1}}"#])),
        (
            r"select 5 - 3, 2.5 - 1, date '2020-01-05' - 1, '[1]'::jsonb || '[2]'::jsonb",
            Ok(&[r"2|1.5|2020-01-04|[1, 2]"]),
        ),
    ]);
}

#[test]
fn jsonb_numbers_keep_their_written_form() {
    script(&[
        (
            r"select '7.50'::jsonb, to_jsonb(7.50::numeric), '1e3'::jsonb, '1E-2'::jsonb, '100000000000000000000'::jsonb, '0.1e1'::jsonb, '-0'::jsonb, '[0.10, 1.0e2]'::jsonb",
            Ok(&[r"7.50|7.50|1000|0.01|100000000000000000000|1|0|[0.10, 100]"]),
        ),
        (
            r#"select '12345678901234567890.123456789'::jsonb, '-0.000'::jsonb, '1e-5'::jsonb, '0.0001230e3'::jsonb, '{"a":1.0}'::jsonb"#,
            Ok(&[r#"12345678901234567890.123456789|0.000|0.00001|0.1230|{"a": 1.0}"#]),
        ),
        (
            r"select to_jsonb(1.5::float8), to_jsonb(1e21::float8), to_jsonb(0.1::float8), to_jsonb(3), to_jsonb(2.50::numeric), to_jsonb(1e-7::float8), jsonb_build_object('a', 1.10::numeric, 'b', 2.5::float8)",
            Ok(&[r#"1.5|1000000000000000000000|0.1|3|2.50|0.0000001|{"a": 1.10, "b": 2.5}"#]),
        ),
        (
            r#"select ('7.50'::jsonb)::text, ('[7.50]'::jsonb -> 0)::text, ('[7.50]'::jsonb ->> 0), ('{"a":7.50}'::jsonb #>> '{a}')"#,
            Ok(&[r"7.50|7.50|7.50|7.50"]),
        ),
        (
            r#"select '1.0'::jsonb = '1'::jsonb, '{"a":1.0}'::jsonb = '{"a":1}'::jsonb, '[1.0]'::jsonb @> '[1]'::jsonb, '1.0'::jsonb > '0.5'::jsonb, (select count(distinct x) from (values ('1.0'::jsonb),('1'::jsonb),('1.00'::jsonb)) v(x))"#,
            Ok(&[r"t|t|t|t|1"]),
        ),
        (
            r"select x from (values ('10'::jsonb),('9.5'::jsonb),('2'::jsonb)) v(x) order by x",
            Ok(&[r"2", r"9.5", r"10"]),
        ),
        (r"select jsonb_agg(x) from (values (1.10::numeric), (2.0::numeric)) v(x)", Ok(&[r"[1.10, 2.0]"])),
    ]);
}
