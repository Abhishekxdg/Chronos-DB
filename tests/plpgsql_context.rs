//! PL/pgSQL as plpgsql.sql and fast_default.sql read it: a polymorphic function's CONTEXT lines name it as declared
//! (`foreach_test(anyarray)`, not the instance a call made of it), and an expression's subquery of a catalog table
//! (`RETURN (SELECT ... FROM pg_class)`) reads the catalog, as a statement does.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn a_polymorphic_functions_context_names_its_declared_types() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE FUNCTION foreach_test(anyarray) RETURNS void AS $$
         DECLARE x int;
         BEGIN
           FOREACH x IN ARRAY $1
           LOOP
             RAISE EXCEPTION 'boom %', x;
           END LOOP;
         END $$ LANGUAGE plpgsql",
    );
    let e = s.run(&db, "SELECT foreach_test(ARRAY[1,2])", &[]).unwrap_err();
    assert_eq!(e.message, "boom 1");
    let context = e.info.and_then(|i| i.context).unwrap_or_default();
    assert!(context.contains("PL/pgSQL function foreach_test(anyarray) line 6 at RAISE"), "{context}");
}

#[test]
fn an_expressions_subquery_reads_the_catalog() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE t (a int4);
         CREATE FUNCTION n() RETURNS bigint AS $$
         BEGIN RETURN (SELECT count(*) FROM pg_class WHERE relname = 't'); END $$ LANGUAGE plpgsql;
         CREATE FUNCTION q() RETURNS bigint AS $$
         BEGIN RETURN (SELECT count(*) FROM pg_catalog.pg_class c WHERE c.relname = 't'); END $$ LANGUAGE plpgsql",
    );
    assert_eq!(run(&db, &mut s, "SELECT n(), q()"), [["1", "1"]]);
    // and sees what changes after its first call
    run(&db, &mut s, "CREATE TABLE u (a int4); DROP TABLE t");
    assert_eq!(run(&db, &mut s, "SELECT n()"), [["0"]]);
}
