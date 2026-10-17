//! ALTER statements whose clauses tune what this database does not have (replica identity, cluster,
//! column statistics and storage, function attributes): accepted, as `pg_dump`'s output needs.
//! Postgres's answers are compared in tests/pgdiff.rs (`alter_options_that_only_tune_postgres`).

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
fn alter_options_that_only_tune_postgres() {
    script(&[
        (r"create table g1 (a int, b int)", Ok(&[])),
        (r"create unique index g1u on g1 (a)", Ok(&[])),
        (r"alter table g1 replica identity full", Ok(&[])),
        (r"alter table g1 replica identity default", Ok(&[])),
        (r"alter table g1 replica identity nothing", Ok(&[])),
        (r"alter table g1 replica identity bogus", Err(r"42601")),
        (r"alter table g1 cluster on g1u", Ok(&[])),
        (r"alter table g1 set without cluster", Ok(&[])),
        (r"alter table g1 alter column a set statistics 100", Ok(&[])),
        (r"alter table g1 alter column a set statistics -1", Ok(&[])),
        (r"alter table g1 alter column a set storage plain", Ok(&[])),
        (r"alter table g1 alter column a set (n_distinct = 10)", Ok(&[])),
        (r"alter table g1 alter column a reset (n_distinct)", Ok(&[])),
        (r"alter table only g1 alter column b set statistics 0", Ok(&[])),
        (r"create function f1(int) returns int language sql as 'select $1 + 1'", Ok(&[])),
        (r"alter function f1(int) immutable", Ok(&[])),
        (r"alter function f1(int) security definer", Ok(&[])),
        (r"alter function f1(int) cost 10", Ok(&[])),
        (r"alter function f1(int) set search_path = public", Ok(&[])),
        (r"alter function f1(int) called on null input", Ok(&[])),
        (r"alter function f1(int) parallel safe", Ok(&[])),
        (r"alter function f1(int) strict", Ok(&[])),
        (r"alter function f1(int) volatile", Ok(&[])),
        (r"alter function f1(int) owner to postgres", Ok(&[])),
        (r"select f1(1)", Ok(&[r"2"])),
        (r"create procedure p1() language sql as 'select 1'", Ok(&[])),
        (r"alter procedure p1() security invoker", Ok(&[])),
    ]);
}
