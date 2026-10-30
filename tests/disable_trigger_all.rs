//! ALTER TABLE ... DISABLE TRIGGER ALL turns off the triggers a foreign key is kept by, as Postgres's
//! RI_ConstraintTrigger ones are: Rails loads its test fixtures with them off, in any order, and turns them back on
//! (ENABLE TRIGGER ALL) without anything being checked again. DISABLE TRIGGER USER leaves them on.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn code(db: &Db, s: &mut Session, q: &str) -> String {
    s.run(db, q, &[]).expect_err(q).code.to_string()
}

fn setup(db: &Db, s: &mut Session) {
    run(
        db,
        s,
        "CREATE TABLE authors (id int4 PRIMARY KEY);
         CREATE TABLE books (id int4 PRIMARY KEY, author int4 REFERENCES authors ON DELETE CASCADE);
         INSERT INTO authors VALUES (1); INSERT INTO books VALUES (10, 1)",
    );
}

#[test]
fn fixtures_load_in_any_order_with_them_off() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    // as Rails' disable_referential_integrity does it, in a savepoint
    run(
        &db,
        &mut s,
        "BEGIN; SAVEPOINT a; ALTER TABLE authors DISABLE TRIGGER ALL; ALTER TABLE books DISABLE TRIGGER ALL;
         RELEASE SAVEPOINT a;
         DELETE FROM authors; INSERT INTO books VALUES (11, 2); INSERT INTO authors VALUES (2);
         SAVEPOINT b; ALTER TABLE authors ENABLE TRIGGER ALL; ALTER TABLE books ENABLE TRIGGER ALL; RELEASE SAVEPOINT b;
         COMMIT",
    );
    // the delete didn't cascade, and nothing was checked again: book 10's author is gone
    assert_eq!(run(&db, &mut s, "SELECT id, author FROM books ORDER BY id"), [["10", "1"], ["11", "2"]]);
    // back on, they're checked again
    assert_eq!(code(&db, &mut s, "INSERT INTO books VALUES (12, 3)"), "23503");
    run(&db, &mut s, "DELETE FROM authors WHERE id = 2");
    assert_eq!(run(&db, &mut s, "SELECT id FROM books ORDER BY id"), [["10"]]);
}

#[test]
fn user_triggers_only_leave_them_on() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(&db, &mut s, "ALTER TABLE books DISABLE TRIGGER USER");
    assert_eq!(code(&db, &mut s, "INSERT INTO books VALUES (11, 2)"), "23503");
    // a rolled-back DISABLE leaves them on
    run(&db, &mut s, "BEGIN; ALTER TABLE books DISABLE TRIGGER ALL; ROLLBACK");
    assert_eq!(code(&db, &mut s, "INSERT INTO books VALUES (11, 2)"), "23503");
}

#[test]
fn only_a_superuser_turns_them_off() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(&db, &mut s, "CREATE ROLE app; ALTER TABLE books OWNER TO app; ALTER TABLE authors OWNER TO app; SET ROLE app");
    assert_eq!(code(&db, &mut s, "ALTER TABLE books DISABLE TRIGGER ALL"), "42501");
    run(&db, &mut s, "ALTER TABLE books DISABLE TRIGGER USER");
}

#[test]
fn a_referenced_tables_are_a_superusers_to_turn_off_too() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(&db, &mut s, "CREATE ROLE app; ALTER TABLE authors OWNER TO app; SET ROLE app");
    assert_eq!(code(&db, &mut s, "ALTER TABLE authors DISABLE TRIGGER ALL"), "42501");
}

#[test]
fn a_partitioned_tables_partitions_each_have_their_own() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE authors (id int4 PRIMARY KEY);
         CREATE TABLE books (id int4, author int4 REFERENCES authors) PARTITION BY RANGE (id);
         CREATE TABLE books_a PARTITION OF books FOR VALUES FROM (0) TO (100);
         CREATE TABLE books_b PARTITION OF books FOR VALUES FROM (100) TO (200);
         ALTER TABLE books DISABLE TRIGGER ALL;
         INSERT INTO books VALUES (1, 7), (101, 7);
         ALTER TABLE books_b ENABLE TRIGGER ALL",
    );
    // the one turned back on checks again; its sibling doesn't
    assert_eq!(code(&db, &mut s, "INSERT INTO books VALUES (102, 8)"), "23503");
    run(&db, &mut s, "INSERT INTO books VALUES (2, 8)");
    // ONLY the partitioned table: its partitions keep theirs
    run(&db, &mut s, "ALTER TABLE books ENABLE TRIGGER ALL; ALTER TABLE ONLY books DISABLE TRIGGER ALL");
    assert_eq!(code(&db, &mut s, "INSERT INTO books VALUES (3, 9)"), "23503");
}

#[test]
fn a_foreign_key_added_later_has_its_own_on() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE authors (id int4 PRIMARY KEY); CREATE TABLE books (id int4, author int4);
         ALTER TABLE books DISABLE TRIGGER ALL;
         ALTER TABLE books ADD CONSTRAINT books_author FOREIGN KEY (author) REFERENCES authors",
    );
    assert_eq!(code(&db, &mut s, "INSERT INTO books VALUES (1, 7)"), "23503");
}
