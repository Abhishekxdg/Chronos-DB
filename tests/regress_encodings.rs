//! Encodings and conversions as Postgres 16 has them (see src/sql/encoding): conversion.sql's regress.c functions
//! test_enc_setup and test_enc_conversion, CREATE [DEFAULT] CONVERSION's checks, and convert, convert_to,
//! convert_from and pg_char_to_encoding over every encoding, with Postgres's messages and SQLSTATEs.

use chronos::sql::{Session, SqlError, Value, take_notices};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> SqlError {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => e,
    }
}

const REGRESS_FUNCTIONS: &str = "
    CREATE FUNCTION test_enc_setup() RETURNS void
        AS '/usr/lib/postgresql/regress.so', 'test_enc_setup' LANGUAGE C STRICT;
    CREATE FUNCTION test_enc_conversion(bytea, name, name, bool, validlen OUT int, result OUT bytea)
        AS '/usr/lib/postgresql/regress.so', 'test_enc_conversion' LANGUAGE C STRICT;";

#[test]
fn test_enc_functions_are_regress_cs() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, REGRESS_FUNCTIONS);
    take_notices();
    run(&db, &mut s, "SELECT FROM test_enc_setup()");
    assert!(take_notices().is_empty(), "test_enc_setup warns");
    // conversion.out's rows: converted, and with noError the good part and how long it is
    let row = |s: &mut Session, q: &str| run(&db, s, q)[0].clone();
    assert_eq!(
        row(&mut s, r"select * from test_enc_conversion('\xe382abe3829a', 'utf8', 'euc_jis_2004', false)"),
        ["6", r"\xa5f7"]
    );
    assert_eq!(
        row(&mut s, r"select * from test_enc_conversion('\x66e8b1ff6f6f', 'utf8', 'euc_jis_2004', true)"),
        ["1", r"\x66"]
    );
    let e = fails(&db, &mut s, r"select * from test_enc_conversion('\x66e8b1ff6f6f', 'utf8', 'euc_jis_2004', false)");
    assert_eq!((e.code, e.message.as_str()), ("22021", "invalid byte sequence for encoding \"UTF8\": 0xe8 0xb1 0xff"));
    let e = fails(&db, &mut s, r"select * from test_enc_conversion('\xecbd94', 'utf8', 'latin1', false)");
    assert_eq!(e.code, "22P05");
    assert_eq!(
        e.message,
        "character with byte sequence 0xec 0xbd 0x94 in encoding \"UTF8\" has no equivalent in encoding \"LATIN1\""
    );
    // one encoding to itself only verifies
    assert_eq!(row(&mut s, r"select * from test_enc_conversion('\x66006f', 'utf8', 'utf8', true)"), ["1", r"\x66"]);
}

#[test]
fn create_conversion_is_checked_as_postgres_checks_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE CONVERSION myconv FOR 'LATIN1' TO 'UTF8' FROM iso8859_1_to_utf8");
    let e = fails(&db, &mut s, "CREATE CONVERSION myconv FOR 'LATIN1' TO 'UTF8' FROM iso8859_1_to_utf8");
    assert_eq!(e.message, "conversion \"myconv\" already exists");
    run(&db, &mut s, "CREATE DEFAULT CONVERSION public.mydef FOR 'LATIN1' TO 'UTF8' FROM iso8859_1_to_utf8");
    let e = fails(&db, &mut s, "CREATE DEFAULT CONVERSION public.mydef2 FOR 'LATIN1' TO 'UTF8' FROM iso8859_1_to_utf8");
    assert_eq!((e.code, e.message.as_str()), ("42710", "default conversion for LATIN1 to UTF8 already exists"));
    // a name taken is reported before the pair
    let e = fails(&db, &mut s, "CREATE DEFAULT CONVERSION mydef FOR 'LATIN1' TO 'UTF8' FROM iso8859_1_to_utf8");
    assert_eq!(e.message, "conversion \"mydef\" already exists");
    let e = fails(&db, &mut s, "CREATE CONVERSION c1 FOR 'NOPE' TO 'UTF8' FROM iso8859_1_to_utf8");
    assert_eq!((e.code, e.message.as_str()), ("42704", "source encoding \"NOPE\" does not exist"));
    let e = fails(&db, &mut s, "CREATE CONVERSION c1 FOR 'SQL_ASCII' TO 'UTF8' FROM iso8859_1_to_utf8");
    assert_eq!(e.message, "encoding conversion to or from \"SQL_ASCII\" is not supported");
    let e = fails(&db, &mut s, "CREATE CONVERSION c1 FOR 'LATIN1' TO 'UTF8' FROM no_such_conv");
    assert_eq!(e.code, "42883");
    // the function is called on an empty string, and refuses encodings it isn't for
    let e = fails(&db, &mut s, "CREATE CONVERSION c1 FOR 'LATIN2' TO 'UTF8' FROM iso8859_1_to_utf8");
    assert_eq!(e.message, "expected source encoding \"LATIN1\", but got \"LATIN2\"");
    run(&db, &mut s, "DROP CONVERSION myconv");
    run(&db, &mut s, "DROP CONVERSION mydef");
    // its pair is free again
    run(&db, &mut s, "CREATE DEFAULT CONVERSION mydef2 FOR 'LATIN1' TO 'UTF8' FROM iso8859_1_to_utf8");
}

#[test]
fn convert_works_for_every_encoding() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(one(&db, &mut s, "select convert_to('日本', 'EUC_JP')"), r"\xc6fccbdc");
    assert_eq!(one(&db, &mut s, "select convert_to('日本', 'SJIS')"), r"\x93fa967b");
    assert_eq!(one(&db, &mut s, "select convert_from('\\xc6fccbdc', 'EUC_JP')"), "日本");
    assert_eq!(one(&db, &mut s, "select convert('\\xc6fccbdc', 'EUC_JP', 'SJIS')"), r"\x93fa967b");
    assert_eq!(one(&db, &mut s, "select convert_to('привет', 'KOI8R')"), r"\xd0d2c9d7c5d4");
    assert_eq!(one(&db, &mut s, "select convert_from(convert_to('añb', 'WIN1252'), 'WIN1252')"), "añb");
    let e = fails(&db, &mut s, "select convert_to('€', 'LATIN1')");
    assert_eq!(e.code, "22P05");
    let e = fails(&db, &mut s, "select convert_from('\\xff', 'UTF8')");
    assert_eq!((e.code, e.message.as_str()), ("22021", "invalid byte sequence for encoding \"UTF8\": 0xff"));
    let e = fails(&db, &mut s, "select convert_to('x', 'nope')");
    assert_eq!((e.code, e.message.as_str()), ("22023", "invalid destination encoding name \"nope\""));
    // names and aliases
    assert_eq!(one(&db, &mut s, "select pg_char_to_encoding('unicode')"), "6");
    assert_eq!(one(&db, &mut s, "select pg_char_to_encoding('Windows-1251')"), "23");
    assert_eq!(one(&db, &mut s, "select pg_encoding_to_char(41)"), "SHIFT_JIS_2004");
}
