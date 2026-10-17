//! `CREATE DOMAIN`: a column of a domain is its base type with the domain's NOT NULL, DEFAULT and CHECKs.
//! Postgres's answers are compared in tests/pgdiff.rs (`domains_carry_their_checks_into_columns`); these run without one.

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
fn domains_carry_their_checks_into_columns() {
    script(&[
        (r"create domain posint as int check (value > 0)", Ok(&[])),
        (r"create domain email as text not null default 'x@y.z' check (value ~ '@')", Ok(&[])),
        (r"create domain shortname varchar(5)", Ok(&[])),
        (r"create domain money2 as numeric(10,2) check (value >= 0) check (value < 1000000)", Ok(&[])),
        (
            r"create table dm (id int primary key, n posint, e email, s shortname, m money2, n2 posint default 7)",
            Ok(&[]),
        ),
        (r"insert into dm (id, n, e) values (1, 5, 'a@b')", Ok(&[])),
        (r"insert into dm (id, n) values (2, 6)", Ok(&[])),
        (r"insert into dm (id, n) values (3, 0)", Err(r"23514")),
        (r"insert into dm (id, n) values (4, -1)", Err(r"23514")),
        (r"insert into dm (id, n, e) values (5, 1, 'nope')", Err(r"23514")),
        (r"insert into dm (id, n, e) values (6, 1, null)", Err(r"23502")),
        (r"insert into dm (id, n, s) values (7, 1, 'toolong')", Err(r"22001")),
        (r"insert into dm (id, n, m) values (8, 1, 12.345)", Ok(&[])),
        (r"insert into dm (id, n, m) values (9, 1, -1)", Err(r"23514")),
        (r"insert into dm (id, n, m) values (10, 1, 2000000)", Err(r"23514")),
        (
            r"select id, n, e, s, m, n2 from dm order by id",
            Ok(&[r"1|5|a@b|||7", r"2|6|x@y.z|||7", r"8|1|x@y.z||12.35|7"]),
        ),
        (r"update dm set n = 0 where id = 1", Err(r"23514")),
        (r"update dm set e = null where id = 1", Err(r"23502")),
        (r"update dm set n = n + 1 where id = 1", Ok(&[])),
        (r"select n from dm where id = 1", Ok(&[r"6"])),
        (r"alter table dm add column n3 posint", Ok(&[])),
        (r"alter table dm add column n4 posint not null default 3", Ok(&[])),
        (r"alter table dm add column n5 posint check (n5 < 100)", Ok(&[])),
        (r"insert into dm (id, n, n5) values (11, 1, 200)", Err(r"23514")),
        (r"create domain posint as int", Err(r"42710")),
        (r"create domain nosuchbase as nosuchtype", Err(r"42704")),
        (r"create domain d3 as int default 'abc'", Err(r"22P02")),
        (r"drop domain posint", Err(r"2BP01")),
        (r"drop domain if exists nosuch", Ok(&[])),
        (r"drop domain nosuch", Err(r"42704")),
        (r"create table dm2 (a email)", Ok(&[])),
        (r"insert into dm2 values (default)", Ok(&[])),
        (r"select a from dm2", Ok(&[r"x@y.z"])),
        (r"create temp table dm3 (a posint)", Ok(&[])),
        (r"insert into dm3 values (-5)", Err(r"23514")),
        (r"select 5::posint", Ok(&[r"5"])),
        (r"select '5'::posint", Ok(&[r"5"])),
        (r"create table dm4 as select 1 as a", Ok(&[])),
        (r"create domain d_arr as int[]", Ok(&[])),
        (r"create table dm5 (a d_arr)", Ok(&[])),
        (r"insert into dm5 values ('{1,2}')", Ok(&[])),
        (r"select a from dm5", Ok(&[r"{1,2}"])),
        (r"create domain cz as int check (value > 0)", Ok(&[])),
        (r"create table czt (a cz)", Ok(&[])),
        (r"drop domain cz cascade", Ok(&[])),
        (r"select count(*) from information_schema.columns where table_name = 'czt'", Ok(&[r"0"])),
    ]);
}
