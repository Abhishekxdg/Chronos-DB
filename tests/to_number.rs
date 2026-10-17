//! `to_number(text, template)`, ported from Postgres's `NUM_processor` (src/sql/tonumber.rs, which was checked
//! against Postgres 16 on 14,000 random and hand-picked cases). Postgres's answers are compared in tests/pgdiff.rs
//! (`to_number_reads_a_number_by_a_template`); these run without one.

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
fn to_number_reads_a_number_by_a_template() {
    script(&[
        (
            r"select to_number('12,345.67','99G999D99'), to_number('-1,234', '9,999'), to_number('0042', '9999'), to_number('12.50', '99.99'), to_number('12', '99')",
            Ok(&[r"12345.67|-1234|42|12.50|12"]),
        ),
        (
            r"select to_number('1,234.56','9,999.99'), to_number('12','99'), to_number('  12 ', '99'), to_number('12-', '99S'), to_number('<12>', '99PR'), to_number('XII', 'RN'), to_number('3rd','9th')",
            Err(r"0A000"),
        ),
        (r"select to_number('$12.5', 'L99D9')", Ok(&[r"12.5"])),
        (r"select to_number('abc', '999')", Err(r"22P02")),
        (r"select to_number('', '9')", Err(r"22P02")),
        (r"select to_number('12', '9')", Ok(&[r"1"])),
        (r"select to_number('1e3','9EEEE')", Err(r"0A000")),
        (r"select to_number('12', '')", Ok(&[r""])),
        (r"select to_number(null, '99'), to_number('12', null)", Ok(&[r"|"])),
        (
            r"select to_number('1,2,3', '9,9,9'), to_number('-12.5', 'S99.9'), to_number('+7', 'S9'), to_number('12.3456', '99.99'), to_number('1234', '99'), to_number('000', '999'), to_number('0', '9V9'), to_number('12', '9V99')",
            Ok(&[r"123|-12.5|7|12.34|12|0|0.00000000000000000|0.120000000000000000"]),
        ),
        (r"select pg_typeof(to_number('1', '9'))", Ok(&[r"numeric"])),
        (r"select to_number(x, '999') from (values ('1'),('22'),('333')) v(x)", Ok(&[r"1", r"22", r"333"])),
        (r"select sum(to_number(x, '99.9')) from (values ('1.5'),('2.25')) v(x)", Ok(&[r"3.7"])),
    ]);
}
