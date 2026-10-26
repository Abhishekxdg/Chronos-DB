//! The catalog's rows name each other as Postgres's do (oidjoins.sql checks every pair `pg_get_catalog_foreign_keys`
//! lists): the system catalogs are rows of pg_class, a table's `reltoastrelid` is its TOAST table's row, and the
//! input and output functions of the built-in extensions' types are rows of pg_proc.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn the_system_catalogs_are_rows_of_pg_class() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(
        run(&db, &mut s, "SELECT relname, relkind, relnamespace::regnamespace FROM pg_class WHERE oid = 1255"),
        [["pg_proc", "r", "pg_catalog"]]
    );
    assert_eq!(run(&db, &mut s, "SELECT relkind FROM pg_class WHERE relname = 'pg_roles'"), [["v"]]);
    assert_eq!(run(&db, &mut s, "SELECT relisshared FROM pg_class WHERE relname = 'pg_authid'"), [["true"]]);
    // what pg_description and pg_depend name by classoid is there
    assert_eq!(
        run(
            &db,
            &mut s,
            "SELECT count(*) FROM pg_description d WHERE NOT EXISTS (SELECT 1 FROM pg_class c WHERE c.oid = d.classoid)"
        ),
        [["0"]]
    );
}

#[test]
fn a_tables_toast_table_is_a_row_of_pg_class() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE t (a int4, b text); CREATE TABLE u (a int4)");
    let rows = run(
        &db,
        &mut s,
        "SELECT c.relname = 'pg_toast_' || t.oid, c.relkind, c.relnamespace::regnamespace
         FROM pg_class t JOIN pg_class c ON c.oid = t.reltoastrelid WHERE t.relname = 't'",
    );
    assert_eq!(rows, [["true", "t", "pg_toast"]]);
    // a table of fixed-length columns only has none
    assert_eq!(run(&db, &mut s, "SELECT reltoastrelid FROM pg_class WHERE relname = 'u'"), [["0"]]);
    assert_eq!(run(&db, &mut s, "SELECT oid FROM pg_namespace WHERE nspname = 'pg_toast'"), [["99"]]);
}

#[test]
fn the_extension_types_functions_are_rows_of_pg_proc() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(
        run(
            &db,
            &mut s,
            "SELECT count(*) FROM pg_type t WHERE t.typname IN ('hstore', 'ltree', 'lquery')
             AND EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = t.typinput)
             AND EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = t.typoutput)
             AND EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = t.typreceive)
             AND EXISTS (SELECT 1 FROM pg_proc p WHERE p.oid = t.typsend)"
        ),
        [["3"]]
    );
    assert_eq!(
        run(&db, &mut s, "SELECT prorettype::regtype, proargtypes::text FROM pg_proc WHERE proname = 'hstore_out'"),
        [["cstring", "16002"]]
    );
}

#[test]
fn regclass_reads_and_shows_the_new_relations() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE t (a int4, b text)");
    assert_eq!(
        run(&db, &mut s, "SELECT 'pg_catalog.pg_class_oid_index'::regclass::oid, 2662::regclass"),
        [["2662", "pg_class_oid_index"]]
    );
    let toast =
        run(&db, &mut s, "SELECT reltoastrelid::regclass::text, reltoastrelid FROM pg_class WHERE relname = 't'");
    let (name, oid) = (&toast[0][0], &toast[0][1]);
    assert!(name.starts_with("pg_toast.pg_toast_"), "{name}");
    assert_eq!(run(&db, &mut s, &format!("SELECT '{name}'::regclass::oid")), [[oid.clone()]]);
}
