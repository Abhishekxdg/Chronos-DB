//! Composite types of no attributes (`CREATE TYPE name AS ()`, or one ALTER TYPE ... DROP ATTRIBUTE left with
//! none) are composite types, as Postgres has them: typtype `c` with a relation and an array type, a record to the
//! regression library's binary_coercible, values `()`. A shell type (`CREATE TYPE name`) is not: typtype `p`, with
//! no relation, no array type and shell_in and shell_out, until a CREATE TYPE fills it in.
//! (The registry of composite types is the process's: each test names its own.)

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> chronos::sql::SqlError {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => e,
    }
}

/// Each row of query `q`, its cells joined by spaces.
fn lines(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    run(db, s, q).into_iter().map(|r| r.join(" ")).collect()
}

/// The pg_type facts of the types named (a list of quoted names), in name order: typname, typtype, whether it has a
/// relation and an array type, typisdefined, typlen, typbyval, typcategory, its input, output, receive and send
/// functions, and its alignment and storage.
fn type_rows(db: &Db, s: &mut Session, names: &str) -> Vec<String> {
    let q = format!(
        "select typname, typtype, typrelid <> 0, typarray <> 0, typisdefined, typlen, typbyval, typcategory, \
         typinput, typoutput, typreceive, typsend, typalign, typstorage from pg_type \
         where typname in ({names}) order by typname"
    );
    lines(db, s, &q)
}

#[test]
fn a_composite_of_no_attributes_is_a_record() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE FUNCTION binary_coercible(oid, oid) RETURNS bool AS '/usr/lib/postgresql/regress.so',
         'binary_coercible' LANGUAGE C STRICT STABLE PARALLEL SAFE",
    );
    run(&db, &mut s, "create type ec_c1 as ()");
    run(&db, &mut s, "create type ec_d1 as (a int)");
    run(&db, &mut s, "alter type ec_d1 drop attribute a");
    run(&db, &mut s, "create type ec_s1");
    for t in ["ec_c1", "ec_d1"] {
        assert_eq!(one(&db, &mut s, &format!("select binary_coercible('{t}'::regtype, 'record'::regtype)")), "true");
        let q = format!("select binary_coercible('{t}[]'::regtype, 'record[]'::regtype)");
        assert_eq!(one(&db, &mut s, &q), "true", "{t}[]");
    }
    // (a shell type is no record)
    let q = "select binary_coercible((select oid from pg_type where typname = 'ec_s1'), 'record'::regtype)";
    assert_eq!(one(&db, &mut s, q), "false");
    // one given an attribute again is a record still
    run(&db, &mut s, "alter type ec_c1 add attribute b text");
    assert_eq!(one(&db, &mut s, "select binary_coercible('ec_c1'::regtype, 'record'::regtype)"), "true");
    // and a domain over one is a record, an array of the domain a record[] (Postgres's ISCOMPLEX follows the domain)
    run(&db, &mut s, "create domain ec_dom1 as ec_c1");
    assert_eq!(one(&db, &mut s, "select binary_coercible('ec_dom1'::regtype, 'record'::regtype)"), "true");
    assert_eq!(one(&db, &mut s, "select binary_coercible('ec_dom1[]'::regtype, 'record[]'::regtype)"), "true");
}

#[test]
fn pg_type_tells_a_composite_of_no_attributes_from_a_shell_type() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type ec_c2 as ()");
    run(&db, &mut s, "create type ec_d2 as (a int)");
    run(&db, &mut s, "alter type ec_d2 drop attribute a");
    run(&db, &mut s, "create type ec_s2");
    assert_eq!(
        type_rows(&db, &mut s, "'ec_c2', 'ec_d2', 'ec_s2'"),
        [
            "ec_c2 c true true true -1 false C record_in record_out record_recv record_send d x",
            "ec_d2 c true true true -1 false C record_in record_out record_recv record_send d x",
            "ec_s2 p false false false 4 true P shell_in shell_out - - i p",
        ]
    );
    // the two composite types have array types, aligned as their elements are, the shell type none
    let q = "select typname, typtype, typcategory, typalign from pg_type where typname like '\\_ec\\__2' order by 1";
    assert_eq!(lines(&db, &mut s, q), ["_ec_c2 b A d", "_ec_d2 b A d"]);
    // and relations, of relkind c, `AS ()`'s of no attributes; the shell type none
    let q = "select relname, relkind from pg_class where relname in ('ec_c2', 'ec_d2', 'ec_s2') order by 1";
    assert_eq!(lines(&db, &mut s, q), ["ec_c2 c", "ec_d2 c"]);
    let q = "select relnatts from pg_class where oid = (select typrelid from pg_type where typname = 'ec_c2')";
    assert_eq!(one(&db, &mut s, q), "0");
    // \dT+ lists each, a composite type's size `tuple`, a shell type's its length
    let q = "SELECT pg_catalog.format_type(t.oid, NULL) AS \"Name\", t.typname AS \"Internal name\",
           CASE WHEN t.typrelid != 0 THEN CAST('tuple' AS pg_catalog.text)
             WHEN t.typlen < 0 THEN CAST('var' AS pg_catalog.text)
             ELSE CAST(t.typlen AS pg_catalog.text) END AS \"Size\"
         FROM pg_catalog.pg_type t LEFT JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
         WHERE (t.typrelid = 0 OR (SELECT c.relkind = 'c' FROM pg_catalog.pg_class c WHERE c.oid = t.typrelid))
           AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_type el WHERE el.oid = t.typelem AND el.typarray = t.oid)
           AND t.typname LIKE 'ec\\__2' AND pg_catalog.pg_type_is_visible(t.oid)
         ORDER BY 1, 2";
    assert_eq!(lines(&db, &mut s, q), ["ec_c2 ec_c2 tuple", "ec_d2 ec_d2 tuple", "ec_s2 ec_s2 4"]);
}

#[test]
fn a_shell_type_filled_in_is_a_composite_type() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let q = |n: &str| format!("select typname, typtype, typrelid <> 0 from pg_type where typname in ('{n}', '_{n}')");
    run(&db, &mut s, "create type ec_s3");
    run(&db, &mut s, "create type ec_s3 as (q int)");
    assert_eq!(lines(&db, &mut s, &(q("ec_s3") + " order by 1")), ["_ec_s3 b false", "ec_s3 c true"]);
    assert_eq!(one(&db, &mut s, "select (row(3)::ec_s3).q"), "3");
    // with no attributes too
    run(&db, &mut s, "create type ec_s4");
    run(&db, &mut s, "create type ec_s4 as ()");
    assert_eq!(lines(&db, &mut s, &(q("ec_s4") + " order by 1")), ["_ec_s4 b false", "ec_s4 c true"]);
    assert_eq!(one(&db, &mut s, "select row()::ec_s4"), "()");
    // (it is no shell type now: a CREATE TYPE of the name finds it there)
    let e = fails(&db, &mut s, "create type ec_s4 as (z int)");
    assert_eq!((e.code, e.message.as_str()), ("42710", "type \"ec_s4\" already exists"));
}

#[test]
fn a_shell_type_has_no_array_type() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE FUNCTION binary_coercible(oid, oid) RETURNS bool AS '/usr/lib/postgresql/regress.so',
         'binary_coercible' LANGUAGE C STRICT STABLE PARALLEL SAFE",
    );
    run(&db, &mut s, "create type ec_s6");
    // no name finds one, as pg_type lists none
    for (q, shown) in [("select 'ec_s6[]'::regtype", "ec_s6[]"), ("select '_ec_s6'::regtype", "_ec_s6")] {
        let e = fails(&db, &mut s, q);
        assert_eq!((e.code, e.message), ("42704", format!("type \"{shown}\" does not exist")), "{q}");
    }
    let q = "select to_regtype('ec_s6[]') is null, to_regtype('_ec_s6') is null";
    assert_eq!(lines(&db, &mut s, q), ["true true"]);
    assert_eq!(one(&db, &mut s, "select count(*) from pg_type where typname = '_ec_s6'"), "0");
    // and the oid one past the shell type's, its array type's once it is filled in, is no type's yet
    let next = "(select (oid::int8 + 1)::oid from pg_type where typname = 'ec_s6')";
    let q = format!("select format_type({next}, null), binary_coercible({next}, 'anyarray'::regtype)");
    assert_eq!(lines(&db, &mut s, &q), ["??? false"]);
    // filled in, it has one
    run(&db, &mut s, "create type ec_s6 as (a int)");
    let q = "select 'ec_s6[]'::regtype, '_ec_s6'::regtype, binary_coercible('ec_s6[]'::regtype, 'anyarray'::regtype)";
    assert_eq!(lines(&db, &mut s, q), ["ec_s6[] ec_s6[] true"]);
}

#[test]
fn values_of_a_composite_of_no_attributes() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type ec_c5 as ()");
    run(&db, &mut s, "create type ec_d5 as (a int)");
    run(&db, &mut s, "alter type ec_d5 drop attribute a");
    assert_eq!(run(&db, &mut s, "select '()'::ec_c5, row()::ec_c5, '()'::ec_d5, row()::ec_d5"), [["()"; 4]]);
    let q = "select format_type('ec_c5'::regtype, null), format_type('ec_c5[]'::regtype, null)";
    assert_eq!(lines(&db, &mut s, q), ["ec_c5 ec_c5[]"]);
    run(&db, &mut s, "create table ec_t5 (x ec_c5, y ec_d5, z ec_c5[])");
    run(&db, &mut s, "insert into ec_t5 values ('()', row(), array['()'::ec_c5])");
    assert_eq!(lines(&db, &mut s, "select x, y, z, x = '()'::ec_c5 from ec_t5"), ["() () {()} true"]);
    // (a shell type's name formats as itself)
    run(&db, &mut s, "create type ec_s5");
    let q = "select format_type((select oid from pg_type where typname = 'ec_s5'), null)";
    assert_eq!(one(&db, &mut s, q), "ec_s5");
}
