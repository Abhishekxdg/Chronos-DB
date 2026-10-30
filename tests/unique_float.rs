//! UNIQUE constraints and unique indexes on real and double precision columns, as Django's unique_together of a
//! FloatField makes them: -0 and 0 are one key, every NaN is one key, and NULLs are none.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn code(db: &Db, s: &mut Session, q: &str) -> String {
    s.run(db, q, &[]).expect_err(q).code.to_string()
}

#[test]
fn a_double_column_is_a_key() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE t (pink int4, weight float8, UNIQUE (pink, weight)); INSERT INTO t VALUES (1, 1.5)");
    assert_eq!(code(&db, &mut s, "INSERT INTO t VALUES (1, 1.5)"), "23505");
    run(&db, &mut s, "INSERT INTO t VALUES (2, 1.5), (1, 2.5)");
    // -0 is 0
    run(&db, &mut s, "INSERT INTO t VALUES (1, 0)");
    assert_eq!(code(&db, &mut s, "INSERT INTO t VALUES (1, '-0')"), "23505");
    // NaN equals NaN, the infinities are themselves
    run(&db, &mut s, "INSERT INTO t VALUES (1, 'NaN'), (1, 'Infinity'), (1, '-Infinity')");
    assert_eq!(code(&db, &mut s, "INSERT INTO t VALUES (1, 'NaN')"), "23505");
    assert_eq!(code(&db, &mut s, "INSERT INTO t VALUES (1, 'Infinity')"), "23505");
    // NULLs are never the same key
    run(&db, &mut s, "INSERT INTO t VALUES (1, NULL), (1, NULL)");
    // a value apart only past a float's 15th digit is apart
    run(&db, &mut s, "INSERT INTO t VALUES (3, 0.1), (3, 0.1000000000000001)");
    // an UPDATE onto a held key
    assert_eq!(code(&db, &mut s, "UPDATE t SET weight = 1.5 WHERE pink = 1 AND weight = 2.5"), "23505");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM t"), [["11"]]);
}

#[test]
fn a_real_column_and_a_unique_index() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE r (w float4 UNIQUE); INSERT INTO r VALUES (0.5);
         CREATE TABLE i (w float8); CREATE UNIQUE INDEX i_w ON i (w); INSERT INTO i VALUES (2.25)",
    );
    assert_eq!(code(&db, &mut s, "INSERT INTO r VALUES (0.5)"), "23505");
    assert_eq!(code(&db, &mut s, "INSERT INTO i VALUES (2.25)"), "23505");
    // ON CONFLICT names it
    run(&db, &mut s, "INSERT INTO i VALUES (2.25) ON CONFLICT (w) DO NOTHING");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM i"), [["1"]]);
}

#[test]
fn added_to_a_table_with_rows() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE p (pink int4, weight float8); INSERT INTO p VALUES (1, 1.0), (1, 2.0)");
    // (Django's AlterUniqueTogether)
    run(&db, &mut s, "ALTER TABLE p ADD CONSTRAINT p_pink_weight_uniq UNIQUE (pink, weight)");
    assert_eq!(code(&db, &mut s, "INSERT INTO p VALUES (1, 1)"), "23505");
    run(&db, &mut s, "ALTER TABLE p DROP CONSTRAINT p_pink_weight_uniq; INSERT INTO p VALUES (1, 1)");
    assert_eq!(code(&db, &mut s, "ALTER TABLE p ADD CONSTRAINT again UNIQUE (pink, weight)"), "23505");
}

#[test]
fn a_foreign_key_references_one() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE p (w float8 UNIQUE); INSERT INTO p VALUES (2.5), (0);
         CREATE TABLE c (w float8 REFERENCES p (w)); INSERT INTO c VALUES (2.5)",
    );
    assert_eq!(code(&db, &mut s, "INSERT INTO c VALUES (3.5)"), "23503");
    assert_eq!(code(&db, &mut s, "DELETE FROM p WHERE w = 2.5"), "23503");
}

#[test]
fn a_key_is_the_same_under_any_extra_float_digits() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE t (w float8 UNIQUE); INSERT INTO t VALUES (1.2345678901234567);
         CREATE TABLE c (w float8 REFERENCES t (w))",
    );
    run(&db, &mut s, "SET extra_float_digits = 0");
    // the same value is found, though it prints as 1.23456789012346 now
    assert_eq!(code(&db, &mut s, "INSERT INTO t VALUES (1.2345678901234567)"), "23505");
    // and one that only prints the same is another key
    run(&db, &mut s, "INSERT INTO t VALUES (1.234567890123459)");
    run(&db, &mut s, "INSERT INTO c VALUES (1.2345678901234567)");
    run(&db, &mut s, "SET extra_float_digits = 3");
    assert_eq!(code(&db, &mut s, "DELETE FROM t WHERE w = 1.2345678901234567"), "23503");
    run(&db, &mut s, "DELETE FROM t WHERE w = 1.234567890123459");
}

#[test]
fn a_partial_indexs_where_is_read_under_the_sessions_digits() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE t (id int4, w float8); CREATE UNIQUE INDEX t_id ON t (id) WHERE w::text = '1.23456789012346'",
    );
    run(&db, &mut s, "SET extra_float_digits = 0; INSERT INTO t VALUES (1, 1.2345678901234567)");
    assert_eq!(code(&db, &mut s, "INSERT INTO t VALUES (1, 1.2345678901234567)"), "23505");
}
