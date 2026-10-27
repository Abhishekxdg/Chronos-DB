//! contrib/bloom: `CREATE EXTENSION bloom` and `CREATE INDEX ... USING bloom (...) WITH (length = ..., col1 = ...)`,
//! as Django's BloomIndex makes them. The index is a btree on its columns here; its parameters are checked as bloom
//! checks them, and the catalog names its access method as Postgres does.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (String, String) {
    let e = s.run(db, q, &[]).expect_err(q);
    (e.code.to_string(), e.message)
}

#[test]
fn a_bloom_index_is_made_and_named_as_postgres_names_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE EXTENSION IF NOT EXISTS bloom;
         CREATE TABLE t (id int4, field varchar(10), n int4);
         INSERT INTO t VALUES (1, 'a', 1), (2, 'b', 2);
         CREATE INDEX t_field_bloom ON t USING bloom (field, n) WITH (length = 512, col1 = 3)",
    );
    // Django's introspection: the index's access method, and its parameters
    assert_eq!(
        run(
            &db,
            &mut s,
            "SELECT am.amname, c.reloptions FROM pg_class c JOIN pg_am am ON c.relam = am.oid
             WHERE c.relname = 't_field_bloom'"
        ),
        [["bloom", "{length=512,col1=3}"]]
    );
    assert_eq!(
        run(&db, &mut s, "SELECT pg_get_indexdef('t_field_bloom'::regclass) LIKE '% USING bloom (field, n)%'"),
        [["true"]]
    );
    assert_eq!(run(&db, &mut s, "SELECT id FROM t WHERE field = 'b' AND n = 2"), [["2"]]);
    run(&db, &mut s, "DROP INDEX t_field_bloom");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_am WHERE amname = 'bloom'"), [["0"]]);
}

#[test]
fn what_bloom_refuses() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE t (a int4, b text, d date)");
    let e = |q: &str, code: &str, message: &str| (q.to_string(), code.to_string(), message.to_string());
    for (q, code, message) in [
        e(
            "CREATE INDEX ON t USING bloom (a) WITH (length = 5000)",
            "22023",
            "value 5000 out of bounds for option \"length\"",
        ),
        e("CREATE INDEX ON t USING bloom (a) WITH (col1 = 0)", "22023", "value 0 out of bounds for option \"col1\""),
        e("CREATE INDEX ON t USING bloom (a) WITH (col33 = 2)", "22023", "unrecognized parameter \"col33\""),
        e("CREATE INDEX ON t USING bloom (a) WITH (fillfactor = 50)", "22023", "unrecognized parameter \"fillfactor\""),
        e(
            "CREATE UNIQUE INDEX ON t USING bloom (a)",
            "0A000",
            "access method \"bloom\" does not support unique indexes",
        ),
        e(
            "CREATE INDEX ON t USING bloom (d)",
            "42704",
            "data type date has no default operator class for access method \"bloom\"",
        ),
    ] {
        assert_eq!(fails(&db, &mut s, &q), (code, message), "{q}");
    }
    run(&db, &mut s, "CREATE INDEX ON t USING bloom (a, b) WITH (col2 = 4)");
}

#[test]
fn its_classes_are_the_defaults_and_it_drops() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE EXTENSION bloom; CREATE TABLE t (a int4, b varchar(5));
         CREATE INDEX t_ab ON t USING bloom (a int4_ops, b text_ops)",
    );
    // a class named that is its column's default is left out, as an index naming none has it
    assert_eq!(run(&db, &mut s, "SELECT pg_get_indexdef('t_ab'::regclass) LIKE '%USING bloom (a, b)%'"), [["true"]]);
    // a class named is one of bloom's, of the column's type
    run(&db, &mut s, "CREATE TABLE d (a date, b text)");
    let code = |s: &mut Session, q: &str| s.run(&db, q, &[]).unwrap_err().code.to_string();
    assert_eq!(code(&mut s, "CREATE INDEX ON d USING bloom (a date_ops)"), "42704");
    assert_eq!(code(&mut s, "CREATE INDEX ON d USING bloom (b int4_ops)"), "42804");
    run(&db, &mut s, "DROP EXTENSION bloom; DROP EXTENSION IF EXISTS bloom");
}
