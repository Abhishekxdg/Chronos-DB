//! ALTER TABLE ... DROP COLUMN of a primary key column drops the key with it, as in Postgres (Rails' migrations
//! `remove_column :t, :id`): the rows stay, the foreign keys that reference the key keep it (2BP01) unless CASCADE
//! takes them too.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn the_key_goes_with_its_column() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE t (id int4 PRIMARY KEY, name text); INSERT INTO t VALUES (1, 'a'), (2, 'b');
         ALTER TABLE t DROP COLUMN id",
    );
    assert_eq!(run(&db, &mut s, "SELECT name FROM t ORDER BY name"), [["a"], ["b"]]);
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_constraint WHERE conrelid = 't'::regclass"), [["0"]]);
    // a table without a key takes any rows, the same twice too
    run(&db, &mut s, "INSERT INTO t VALUES ('a')");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM t WHERE name = 'a'"), [["2"]]);
    // a column of a key of several
    run(&db, &mut s, "CREATE TABLE p (a int4, b int4, PRIMARY KEY (a, b)); INSERT INTO p VALUES (1, 1), (1, 2)");
    run(&db, &mut s, "ALTER TABLE p DROP COLUMN b; INSERT INTO p VALUES (1)");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM p WHERE a = 1"), [["3"]]);
}

#[test]
fn a_referenced_key_needs_cascade() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE t (id int4 PRIMARY KEY); CREATE TABLE c (tid int4 REFERENCES t); INSERT INTO t VALUES (1);
         INSERT INTO c VALUES (1)",
    );
    let e = s.run(&db, "ALTER TABLE t DROP COLUMN id", &[]).unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        ("2BP01", "cannot drop column id of table t because other objects depend on it")
    );
    run(&db, &mut s, "ALTER TABLE t DROP COLUMN id CASCADE; INSERT INTO c VALUES (5)");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_constraint WHERE contype = 'f'"), [["0"]]);
}

#[test]
fn cascade_takes_the_keys_and_the_uniques_references_together() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE p (a int4 UNIQUE, b int4, PRIMARY KEY (a, b)); INSERT INTO p VALUES (1, 1);
         CREATE TABLE c (x int4, y int4, FOREIGN KEY (x, y) REFERENCES p, FOREIGN KEY (x) REFERENCES p (a));
         ALTER TABLE p DROP COLUMN a CASCADE",
    );
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_constraint WHERE conrelid = 'c'::regclass"), [["0"]]);
    run(&db, &mut s, "INSERT INTO c VALUES (5, 5)");
}

#[test]
fn a_foreign_key_of_its_own_on_the_column_goes_with_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE t (id int4 PRIMARY KEY, payload text, FOREIGN KEY (id) REFERENCES t (id));
         INSERT INTO t VALUES (1, 'a'); ALTER TABLE t DROP COLUMN id",
    );
    assert_eq!(run(&db, &mut s, "SELECT payload FROM t"), [["a"]]);
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_constraint WHERE conrelid = 't'::regclass"), [["0"]]);
}
