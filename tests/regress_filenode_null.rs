//! pg_filenode_relation and its neighbours on a relation's storage (pg_relation_filenode, pg_tablespace_location,
//! pg_relation_size) are strict in Postgres's catalog: a NULL argument gives NULL, never the relation a default for it
//! would find. pg_filenode_relation's two arguments are oids, so text that isn't one is an error, not the database's
//! default tablespace or a relation's name, and text that is one is read as oidin reads it (`+1663`, `0x67F`, `-1`
//! wrapped around); pg_tablespace_location's is an oid too. The relation functions' argument is a regclass: a name of
//! none is an error, an oid of none NULL. Each case's answer is Postgres 16's.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn value(db: &Db, s: &mut Session, q: &str, params: &[Value]) -> Value {
    let out = s.run(db, q, params).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows[0][0].clone()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    value(db, s, q, &[]).to_text()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code, e.message),
    }
}

fn session(db: &Db) -> Session {
    let mut s = Session::new(MAIN);
    s.run(db, "create table t (a int primary key)", &[]).unwrap();
    s
}

#[test]
fn a_null_tablespace_or_filenode_finds_no_relation() {
    let db = Db::new();
    let mut s = session(&db);
    for q in [
        // (a NULL tablespace is not the database's default, 0)
        "select pg_filenode_relation(null, pg_relation_filenode('t'))",
        "select pg_filenode_relation(null::oid, pg_relation_filenode('t'))",
        "select pg_filenode_relation(null, pg_relation_filenode('t_pkey'))",
        "select pg_filenode_relation(x, pg_relation_filenode('t')) from (values (null::oid)) v(x)",
        "select pg_filenode_relation(0, null)",
        "select pg_filenode_relation(null, null)",
    ] {
        assert_eq!(value(&db, &mut s, q, &[]), Value::Null, "{q}");
    }
    let q = "select pg_filenode_relation($1::oid, pg_relation_filenode('t'))";
    assert_eq!(value(&db, &mut s, q, &[Value::Null]), Value::Null, "{q}");
}

#[test]
fn the_default_tablespace_by_zero_or_by_oid_finds_the_relation() {
    let db = Db::new();
    let mut s = session(&db);
    assert_eq!(one(&db, &mut s, "select pg_filenode_relation(0, pg_relation_filenode('t'))"), "t");
    let by_oid = "select pg_filenode_relation((select oid from pg_tablespace where spcname = 'pg_default'), \
                  pg_relation_filenode('t'))";
    assert_eq!(one(&db, &mut s, by_oid), "t");
    assert_eq!(one(&db, &mut s, "select pg_filenode_relation(0, pg_relation_filenode('t_pkey'))"), "t_pkey");
    // (filenode 0 is none, as is a tablespace that holds nothing)
    assert_eq!(value(&db, &mut s, "select pg_filenode_relation(0, 0)", &[]), Value::Null);
    let elsewhere = "select pg_filenode_relation(12345, pg_relation_filenode('t'))";
    assert_eq!(value(&db, &mut s, elsewhere, &[]), Value::Null);
    // an oid written as text is read as one, as oidin reads it: a sign, hex, octal
    for space in ["1663", " 1663 ", "+1663", "0x67F", " 0x67f "] {
        let q = format!("select pg_filenode_relation('{space}', pg_relation_filenode('t'))");
        assert_eq!(one(&db, &mut s, &q), "t", "{q}");
    }
    // (-1 is 4294967295, 01663 octal 947: tablespaces of nothing, as -2147483648 is)
    for q in [
        "select pg_filenode_relation('-1', 0)",
        "select pg_filenode_relation(' -1 ', pg_relation_filenode('t'))",
        "select pg_filenode_relation(0, '-1')",
        "select pg_filenode_relation('01663', pg_relation_filenode('t'))",
        "select pg_filenode_relation('-2147483648', 0)",
        "select pg_filenode_relation(0, -1)",
        "select pg_filenode_relation(-1, pg_relation_filenode('t'))",
    ] {
        assert_eq!(value(&db, &mut s, q, &[]), Value::Null, "{q}");
    }
}

#[test]
fn text_that_is_not_an_oid_is_an_error() {
    let db = Db::new();
    let mut s = session(&db);
    for (q, text) in [
        ("select pg_filenode_relation('x', pg_relation_filenode('t'))", "x"),
        // (not the relation t's oid: the argument is no regclass)
        ("select pg_filenode_relation('t', pg_relation_filenode('t'))", "t"),
        ("select pg_filenode_relation(0, 'x')", "x"),
        ("select pg_filenode_relation(0, 't')", "t"),
        ("select pg_filenode_relation('', 0)", ""),
        ("select pg_filenode_relation(' x ', 0)", " x "),
        ("select pg_filenode_relation('0x', 0)", "0x"),
        ("select pg_filenode_relation('08', 0)", "08"),
    ] {
        assert_eq!(fails(&db, &mut s, q), ("22P02", format!("invalid input syntax for type oid: \"{text}\"")), "{q}");
    }
    for text in ["4294967296", "-2147483649"] {
        let q = format!("select pg_filenode_relation('{text}', 0)");
        assert_eq!(fails(&db, &mut s, &q), ("22003", format!("value \"{text}\" is out of range for type oid")), "{q}");
    }
}

#[test]
fn an_integer_past_an_oid_is_out_of_range() {
    let db = Db::new();
    let mut s = session(&db);
    for q in [
        "select pg_filenode_relation(0, 4294967296)",
        "select pg_filenode_relation(4294967296, 0)",
        "select pg_filenode_relation(0, pg_relation_filenode('t')::bigint + 4294967296)",
        "select pg_tablespace_location(4294967296)",
        "select pg_relation_filenode(4294967296)",
    ] {
        assert_eq!(fails(&db, &mut s, q), ("22003", "OID out of range".to_string()), "{q}");
    }
}

#[test]
fn pg_tablespace_location_reads_its_argument_as_an_oid() {
    let db = Db::new();
    let mut s = session(&db);
    for text in ["x", "", " x "] {
        let q = format!("select pg_tablespace_location('{text}')");
        assert_eq!(fails(&db, &mut s, &q), ("22P02", format!("invalid input syntax for type oid: \"{text}\"")), "{q}");
    }
    let q = "select pg_tablespace_location('4294967296')";
    assert_eq!(fails(&db, &mut s, q), ("22003", "value \"4294967296\" is out of range for type oid".to_string()));
    // (pg_default's and pg_global's, in hex or signed)
    for q in [
        "select pg_tablespace_location('0x67F')",
        "select pg_tablespace_location('+1664')",
        "select pg_tablespace_location(' 1663 ')",
    ] {
        assert_eq!(one(&db, &mut s, q), "", "{q}");
    }
    // (-1 is 4294967295, which has no link)
    let gone = ("58P01", "could not stat file \"pg_tblspc/4294967295\": No such file or directory".to_string());
    for q in ["select pg_tablespace_location('-1')", "select pg_tablespace_location(-1)"] {
        assert_eq!(fails(&db, &mut s, q), gone, "{q}");
    }
}

#[test]
fn a_relation_named_as_none_is_an_error_and_an_oid_of_none_is_null() {
    let db = Db::new();
    let mut s = session(&db);
    for (q, name) in [
        ("select pg_relation_filenode('x')", "x"),
        ("select pg_relation_filenode('-1')", "-1"),
        ("select pg_relation_filenode('nosuch.t')", "nosuch.t"),
        ("select pg_relation_size('x')", "x"),
        ("select pg_table_size('x')", "x"),
        // (the relation is read before the fork's name)
        ("select pg_relation_size('x', 'bogus')", "x"),
    ] {
        assert_eq!(fails(&db, &mut s, q), ("42P01", format!("relation \"{name}\" does not exist")), "{q}");
    }
    for q in [
        "select pg_relation_filenode(12345)",
        "select pg_relation_filenode('12345')",
        "select pg_relation_filenode(0)",
        "select pg_relation_filenode(-1)",
        "select pg_relation_size(12345)",
        "select pg_relation_size('12345')",
        "select pg_relation_size(0)",
        "select pg_relation_size(-1)",
        "select pg_relation_size(12345, 'main')",
        "select pg_relation_size(12345, 'bogus')",
        "select pg_table_size(12345)",
        "select pg_indexes_size(12345)",
        "select pg_total_relation_size(12345)",
    ] {
        assert_eq!(value(&db, &mut s, q, &[]), Value::Null, "{q}");
    }
    let same = "select pg_relation_filenode(' t ') = pg_relation_filenode('t'), \
                pg_relation_filenode('public.t') = pg_relation_filenode('t'::regclass)";
    assert_eq!(one(&db, &mut s, same), "true");
    assert_eq!(fails(&db, &mut s, "select pg_relation_size('t', 'bogus')").0, "22023");
}

#[test]
fn the_other_storage_functions_are_null_on_a_null_argument() {
    let db = Db::new();
    let mut s = session(&db);
    for q in [
        "select pg_relation_filenode(null)",
        "select pg_tablespace_location(null)",
        "select pg_relation_size(null)",
        "select pg_relation_size('t', null)",
        "select pg_table_size(null)",
        "select pg_indexes_size(null)",
        "select pg_total_relation_size(null)",
    ] {
        assert_eq!(value(&db, &mut s, q, &[]), Value::Null, "{q}");
    }
}
