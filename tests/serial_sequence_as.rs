//! A serial column's own sequence takes `ALTER SEQUENCE ... AS` the type its column has: Django changes an
//! AutoField to a BigAutoField by altering the column to bigint, then `ALTER SEQUENCE IF EXISTS t_id_seq AS bigint`.
//! Its other options are still the column's type's.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn as_the_columns_type() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE t (id serial PRIMARY KEY, n int4); INSERT INTO t (n) VALUES (1);
         ALTER TABLE t ALTER COLUMN id TYPE bigint;
         ALTER SEQUENCE IF EXISTS t_id_seq AS bigint;
         INSERT INTO t (n) VALUES (2)",
    );
    assert_eq!(run(&db, &mut s, "SELECT id FROM t ORDER BY id"), [["1"], ["2"]]);
    assert_eq!(run(&db, &mut s, "SELECT data_type FROM pg_sequences WHERE sequencename = 't_id_seq'"), [["bigint"]]);
    // another type than the column's, or another option, is still refused
    for q in ["ALTER SEQUENCE t_id_seq AS smallint", "ALTER SEQUENCE t_id_seq INCREMENT 2"] {
        assert_eq!(s.run(&db, q, &[]).unwrap_err().code, "0A000", "{q}");
    }
}
