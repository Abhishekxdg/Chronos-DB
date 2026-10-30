//! UNIQUE constraints DEFERRABLE INITIALLY DEFERRED (and SET CONSTRAINTS ... DEFERRED): a transaction may let two rows
//! hold a key until COMMIT, which checks it again, as Postgres does. Django's and Rails' test schemas make them, and
//! apps use them to swap positions.

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

fn setup(db: &Db, s: &mut Session) {
    run(
        db,
        s,
        "CREATE TABLE t (id int4 PRIMARY KEY, pos int4 UNIQUE DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO t VALUES (1, 1), (2, 2)",
    );
}

#[test]
fn a_transaction_swaps_two_keys() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(&db, &mut s, "BEGIN; UPDATE t SET pos = 2 WHERE id = 1; UPDATE t SET pos = 1 WHERE id = 2; COMMIT");
    assert_eq!(run(&db, &mut s, "SELECT id, pos FROM t ORDER BY id"), [["1", "2"], ["2", "1"]]);
    // the keys' entries name their rows after it: a third row of either key is refused, and a row frees its own
    assert_eq!(fails(&db, &mut s, "INSERT INTO t VALUES (3, 2)").0, "23505");
    assert_eq!(fails(&db, &mut s, "INSERT INTO t VALUES (3, 1)").0, "23505");
    run(&db, &mut s, "UPDATE t SET pos = 9 WHERE id = 1; INSERT INTO t VALUES (3, 2)");
}

#[test]
fn a_duplicate_left_at_commit_is_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(&db, &mut s, "BEGIN; INSERT INTO t VALUES (3, 1)");
    let (code, message) = fails(&db, &mut s, "COMMIT");
    // (with the note that the transaction went: Postgres's own words under pg_exact)
    assert_eq!(code, "23505");
    assert!(message.starts_with("duplicate key value violates unique constraint \"t_pos_key\""), "{message}");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM t"), [["2"]]);
    // a duplicate made and undone inside the transaction is no matter, a third row taking the key too
    run(
        &db,
        &mut s,
        "BEGIN; INSERT INTO t VALUES (3, 1); UPDATE t SET pos = 5 WHERE id = 1; INSERT INTO t VALUES (4, 7)",
    );
    run(&db, &mut s, "UPDATE t SET pos = 7 WHERE id = 1");
    assert_eq!(fails(&db, &mut s, "COMMIT").0, "23505");
    // outside a transaction a statement's duplicate is refused at once
    assert_eq!(fails(&db, &mut s, "INSERT INTO t VALUES (3, 1)").0, "23505");
}

#[test]
fn set_constraints_defers_a_deferrable_one_and_checks_it_when_made_immediate() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE u (id int4 PRIMARY KEY, pos int4, CONSTRAINT u_pos UNIQUE (pos) DEFERRABLE);
         INSERT INTO u VALUES (1, 1), (2, 2)",
    );
    // INITIALLY IMMEDIATE: checked as each statement writes
    run(&db, &mut s, "BEGIN");
    assert_eq!(fails(&db, &mut s, "UPDATE u SET pos = 2 WHERE id = 1").0, "23505");
    run(&db, &mut s, "ROLLBACK");
    run(
        &db,
        &mut s,
        "BEGIN; SET CONSTRAINTS u_pos DEFERRED; UPDATE u SET pos = 2 WHERE id = 1; UPDATE u SET pos = 1 WHERE id = 2;
         COMMIT",
    );
    assert_eq!(run(&db, &mut s, "SELECT id, pos FROM u ORDER BY id"), [["1", "2"], ["2", "1"]]);
    run(&db, &mut s, "BEGIN; SET CONSTRAINTS ALL DEFERRED; UPDATE u SET pos = 1 WHERE id = 1");
    assert_eq!(fails(&db, &mut s, "SET CONSTRAINTS ALL IMMEDIATE").0, "23505");
    run(&db, &mut s, "ROLLBACK");
    // a constraint that isn't DEFERRABLE can't be put off
    run(&db, &mut s, "CREATE TABLE w (pos int4 CONSTRAINT w_pos UNIQUE)");
    assert_eq!(fails(&db, &mut s, "BEGIN; SET CONSTRAINTS w_pos DEFERRED").0, "42809");
}

#[test]
fn the_catalog_says_it_is_deferred() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    assert_eq!(
        run(
            &db,
            &mut s,
            "SELECT condeferrable, condeferred, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 't_pos_key'"
        ),
        [["true", "true", "UNIQUE (pos) DEFERRABLE INITIALLY DEFERRED"]]
    );
    // ALTER TABLE ... ADD CONSTRAINT, as Django and Rails make them
    run(
        &db,
        &mut s,
        "CREATE TABLE v (pos int4); ALTER TABLE v ADD CONSTRAINT v_pos UNIQUE (pos) DEFERRABLE INITIALLY DEFERRED",
    );
    assert_eq!(
        run(&db, &mut s, "SELECT condeferrable, condeferred FROM pg_constraint WHERE conname = 'v_pos'"),
        [["true", "true"]]
    );
    assert_eq!(
        fails(&db, &mut s, "CREATE TABLE x (pos int4 UNIQUE NOT DEFERRABLE INITIALLY DEFERRED)"),
        ("42601".to_string(), "constraint declared INITIALLY DEFERRED must be DEFERRABLE".to_string())
    );
}

#[test]
fn a_rename_waits_for_the_checks() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    // (each in a transaction of its own: the refusal ends the transaction's work)
    for q in ["ALTER TABLE t RENAME CONSTRAINT t_pos_key TO renamed", "ALTER TABLE t RENAME TO t2"] {
        run(&db, &mut s, "BEGIN; INSERT INTO t VALUES (3, 1)");
        assert_eq!(fails(&db, &mut s, q).0, "55006", "{q}");
        run(&db, &mut s, "ROLLBACK");
    }
}

#[test]
fn a_savepoint_rolled_back_to_brings_its_check_back() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    // a duplicate made before the savepoint, mended after it and checked, then the mending undone
    run(
        &db,
        &mut s,
        "BEGIN; INSERT INTO t VALUES (3, 1); SAVEPOINT a; DELETE FROM t WHERE id = 3;
         SET CONSTRAINTS ALL IMMEDIATE; ROLLBACK TO SAVEPOINT a; SET CONSTRAINTS ALL DEFERRED",
    );
    assert_eq!(fails(&db, &mut s, "COMMIT").0, "23505");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM t"), [["2"]]);
}

#[test]
fn a_temporary_tables_is_checked_in_its_own_database() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TEMP TABLE tt (id int4 PRIMARY KEY, pos int4 UNIQUE DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO tt VALUES (1, 1), (2, 2)",
    );
    run(&db, &mut s, "BEGIN; INSERT INTO tt VALUES (3, 1)");
    assert_eq!(fails(&db, &mut s, "COMMIT").0, "23505");
    run(&db, &mut s, "BEGIN; UPDATE tt SET pos = 2 WHERE id = 1; UPDATE tt SET pos = 1 WHERE id = 2; COMMIT");
    assert_eq!(run(&db, &mut s, "SELECT id, pos FROM tt ORDER BY id"), [["1", "2"], ["2", "1"]]);
    assert_eq!(fails(&db, &mut s, "INSERT INTO tt VALUES (3, 2)").0, "23505");
}

#[test]
fn no_alter_or_drop_while_checks_are_out() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    for q in ["ALTER TABLE t DROP CONSTRAINT t_pos_key", "DROP TABLE t"] {
        run(&db, &mut s, "BEGIN; INSERT INTO t VALUES (3, 1); DELETE FROM t WHERE id = 3");
        assert_eq!(fails(&db, &mut s, q).0, "55006", "{q}");
        run(&db, &mut s, "ROLLBACK");
    }
    // once SET CONSTRAINTS ... IMMEDIATE has checked them, as Django does before its ALTERs, they're no hold
    run(
        &db,
        &mut s,
        "BEGIN; INSERT INTO t VALUES (3, 1); DELETE FROM t WHERE id = 3; SET CONSTRAINTS ALL IMMEDIATE;
         ALTER TABLE t ADD COLUMN note text; COMMIT",
    );
}

#[test]
fn a_temporary_table_of_the_name_doesnt_take_the_check() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(&db, &mut s, "BEGIN; INSERT INTO t VALUES (3, 1); CREATE TEMP TABLE t (id int4, pos int4)");
    assert_eq!(fails(&db, &mut s, "COMMIT").0, "23505");
}

#[test]
fn set_constraints_names_a_temporary_tables() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TEMP TABLE tt (id int4, pos int4 CONSTRAINT tt_pos UNIQUE DEFERRABLE); INSERT INTO tt VALUES (1, 1), (2, 2);
         BEGIN; SET CONSTRAINTS tt_pos DEFERRED; UPDATE tt SET pos = 2 WHERE id = 1; UPDATE tt SET pos = 1 WHERE id = 2;
         COMMIT",
    );
    assert_eq!(run(&db, &mut s, "SELECT id, pos FROM tt ORDER BY id"), [["1", "2"], ["2", "1"]]);
}

#[test]
fn a_temporary_tables_check_is_its_own_beside_a_permanent_ones() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    run(
        &db,
        &mut s,
        "BEGIN; INSERT INTO t VALUES (3, 1); DELETE FROM t WHERE id = 3;
         CREATE TEMP TABLE t (id int4 PRIMARY KEY, pos int4 UNIQUE DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO t VALUES (1, 1), (2, 1)",
    );
    assert_eq!(fails(&db, &mut s, "COMMIT").0, "23505");
}

#[test]
fn no_foreign_key_references_one() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    let (code, message) = fails(&db, &mut s, "CREATE TABLE c (pos int4 REFERENCES t (pos))");
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("55000", "cannot use a deferrable unique constraint for referenced table \"t\"")
    );
}
