//! `TABLESAMPLE BERNOULLI|SYSTEM (p) [REPEATABLE (seed)]` and `generate_series` with a numeric step.
//! Postgres's answers are compared in tests/pgdiff.rs (`tablesample_and_numeric_series_steps`); these run without one.

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
fn tablesample_and_numeric_series_steps() {
    script(&[
        (r"create table ts (id int primary key, v text)", Ok(&[])),
        (r"insert into ts select g, 'r'||g from generate_series(1,1000) g", Ok(&[])),
        (r"select count(*) from ts tablesample bernoulli (100)", Ok(&[r"1000"])),
        (r"select count(*) from ts tablesample bernoulli (0)", Ok(&[r"0"])),
        (r"select count(*) between 50 and 160 from ts tablesample bernoulli (10)", Ok(&[r"t"])),
        (r"select count(*) from ts t tablesample bernoulli (100) repeatable (1) where t.id < 10", Ok(&[r"9"])),
        (
            r"select (select count(*) from ts tablesample bernoulli (30) repeatable (42)) = (select count(*) from ts tablesample bernoulli (30) repeatable (42))",
            Ok(&[r"t"]),
        ),
        (r"select count(*) from ts tablesample bernoulli (101)", Err(r"2202H")),
        (r"select count(*) from ts tablesample bernoulli (-1)", Err(r"2202H")),
        (r"select count(*) from ts tablesample foo (10)", Err(r"42704")),
        (r"select count(*) from ts tablesample bernoulli (null)", Err(r"2202H")),
        (r"select x.id from ts as x tablesample bernoulli (100) where x.id <= 2 order by 1", Ok(&[r"1", r"2"])),
        (r"select count(*) between 300 and 700 from ts tablesample bernoulli (50) join ts u using (id)", Ok(&[r"t"])),
        (r"select count(*) from ts tablesample system (100)", Ok(&[r"1000"])),
        (r"select count(*) from ts tablesample system (0)", Ok(&[r"0"])),
        (r"select count(*) from ts tablesample bernoulli (100.0)", Ok(&[r"1000"])),
        (r"select count(*) from ts tablesample bernoulli (50 + 50)", Ok(&[r"1000"])),
        (r"select count(*) from (select * from ts tablesample bernoulli (100)) s", Ok(&[r"1000"])),
        (r"select t.v from ts t tablesample bernoulli(100) where id = 5", Ok(&[r"r5"])),
        (r"select count(*) from ts tablesample bernoulli (100) repeatable (null)", Err(r"2202G")),
        (r"select generate_series(1, 2, 0.5)", Ok(&[r"1", r"1.5", r"2.0"])),
        (r"select generate_series(2, 1, -0.5)", Ok(&[r"2", r"1.5", r"1.0"])),
        (r"select * from generate_series(0, 1, 0.25)", Ok(&[r"0", r"0.25", r"0.50", r"0.75", r"1.00"])),
        (r"select sum(x) from generate_series(1.5, 3.0, 0.5) x", Ok(&[r"9.0"])),
        (r"select generate_series(1, 2, 0.0)", Err(r"22023")),
        (r"select pg_typeof(x) from generate_series(1, 2, 0.5) x limit 1", Ok(&[r"numeric"])),
    ]);
}
