//! varchar(n) / char(n): longer values are refused (22001), explicit casts cut, and the limit
//! survives a restart.

use chronos::sql::run;
use chronos::{Db, MAIN};

fn code(db: &Db, sql: &str) -> &'static str {
    match run(db, MAIN, sql, &[]) {
        Ok(_) => "ok",
        Err(e) => e.code,
    }
}

fn one(db: &Db, sql: &str) -> String {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    out.last().unwrap().rows[0][0].to_text()
}

#[test]
fn a_too_long_value_is_refused() {
    let db = Db::new();
    run(&db, MAIN, "create table z (id int primary key, code varchar(4), ch char(3), c2 character varying(2))", &[])
        .unwrap();
    assert_eq!(code(&db, "insert into z values (1, 'abcd', 'xyz', 'ab')"), "ok");
    assert_eq!(code(&db, "insert into z values (2, 'abcdefgh', 'x', 'a')"), "22001");
    assert_eq!(code(&db, "insert into z values (3, 'a', 'wxyz', 'a')"), "22001");
    assert_eq!(code(&db, "insert into z values (4, 'a', 'x', 'abc')"), "22001");
    // blanks past the limit are cut, not refused; characters, not bytes, are counted
    assert_eq!(code(&db, "insert into z values (5, 'ab      ', 'x', 'é日')"), "ok");
    assert_eq!(one(&db, "select code from z where id = 5"), "ab  ");
    // an UPDATE is held to it too
    assert_eq!(code(&db, "update z set code = 'toolong' where id = 1"), "22001");
    assert_eq!(one(&db, "select code from z where id = 1"), "abcd");
    let e = run(&db, MAIN, "insert into z values (6, 'abcdefgh', 'x', 'a')", &[]).unwrap_err();
    // (as Postgres words it: no space before the length)
    assert_eq!(e.message, "value too long for type character varying(4)");
    // a text column has no limit; a bare char is char(1)
    run(&db, MAIN, "create table y (t text, v varchar, c char)", &[]).unwrap();
    assert_eq!(code(&db, "insert into y values (repeat('x', 1000), repeat('y', 1000), 'a')"), "ok");
    assert_eq!(code(&db, "insert into y values ('a', 'a', 'ab')"), "22001");
}

#[test]
fn an_explicit_cast_cuts() {
    let db = Db::new();
    assert_eq!(one(&db, "select 'abcdefgh'::varchar(4)"), "abcd");
    assert_eq!(one(&db, "select cast('abcdefgh' as character varying(3))"), "abc");
    assert_eq!(one(&db, "select 'ab'::varchar(4)"), "ab");
    assert_eq!(code(&db, "select 'a'::varchar(0)"), "22023");
}

#[test]
fn the_limit_is_kept_by_a_persisted_table() {
    let dir = std::env::temp_dir().join(format!("chronos-varchar-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, "create table z (id int primary key, code varchar(4))", &[]).unwrap();
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(code(&db, "insert into z values (1, 'abcdefgh')"), "22001");
    assert_eq!(code(&db, "insert into z values (1, 'abcd')"), "ok");
    let _ = std::fs::remove_dir_all(&dir);
}
