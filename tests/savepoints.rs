//! SAVEPOINT, RELEASE SAVEPOINT and ROLLBACK TO SAVEPOINT inside transactions.

use chronos::sql::{Session, Value, run};
use chronos::{Db, MAIN};

fn ids(db: &Db) -> Vec<i64> {
    run(db, MAIN, "select id from t order by id", &[])
        .unwrap()
        .pop()
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect()
}

fn setup() -> (Db, Session) {
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, n int check (n >= 0))", &[]).unwrap();
    (db, Session::new(MAIN))
}

fn ok(s: &mut Session, db: &Db, q: &str) -> String {
    s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap().tag
}

fn code(s: &mut Session, db: &Db, q: &str) -> &'static str {
    s.run(db, q, &[]).expect_err(q).code
}

#[test]
fn nested_savepoints_roll_back_and_release() {
    let (db, mut s) = setup();
    ok(&mut s, &db, "begin");
    ok(&mut s, &db, "insert into t values (1, 1)");
    assert_eq!(ok(&mut s, &db, "savepoint a"), "SAVEPOINT");
    ok(&mut s, &db, "insert into t values (2, 1)");
    ok(&mut s, &db, "savepoint b");
    ok(&mut s, &db, "insert into t values (3, 1)");
    assert_eq!(ok(&mut s, &db, "rollback to savepoint a"), "ROLLBACK"); // drops b and everything after a
    ok(&mut s, &db, "insert into t values (4, 1)");
    assert_eq!(ok(&mut s, &db, "release savepoint a"), "RELEASE");
    assert!(ids(&db).is_empty(), "nothing is visible before COMMIT");
    assert_eq!(ok(&mut s, &db, "commit"), "COMMIT");
    assert_eq!(ids(&db), [1, 4]);
    assert!(db.branches().iter().all(|(b, _)| !b.starts_with("_tx_")), "no branches left behind");
}

#[test]
fn rollback_to_recovers_a_failed_transaction() {
    let (db, mut s) = setup();
    ok(&mut s, &db, "begin");
    ok(&mut s, &db, "insert into t values (1, 1)");
    ok(&mut s, &db, "savepoint before_risky");
    assert_eq!(code(&mut s, &db, "insert into t values (2, -1)"), "23514");
    assert_eq!(code(&mut s, &db, "insert into t values (3, 1)"), "25P02", "the transaction is failed");
    ok(&mut s, &db, "rollback to before_risky");
    ok(&mut s, &db, "insert into t values (3, 1)"); // usable again
    ok(&mut s, &db, "savepoint x"); // open savepoints are released by COMMIT
    ok(&mut s, &db, "insert into t values (5, 1)");
    assert_eq!(ok(&mut s, &db, "commit"), "COMMIT");
    assert_eq!(ids(&db), [1, 3, 5]);
}

#[test]
fn rollback_drops_savepoints_too() {
    let (db, mut s) = setup();
    ok(&mut s, &db, "begin");
    ok(&mut s, &db, "savepoint a");
    ok(&mut s, &db, "insert into t values (1, 1)");
    ok(&mut s, &db, "savepoint a"); // the same name again: the newer one is used
    ok(&mut s, &db, "insert into t values (2, 1)");
    ok(&mut s, &db, "rollback to a");
    ok(&mut s, &db, "rollback");
    assert!(ids(&db).is_empty());
    assert!(db.branches().iter().all(|(b, _)| !b.starts_with("_tx_")));
    assert_eq!(code(&mut s, &db, "savepoint a"), "25P01", "only inside a transaction");
    ok(&mut s, &db, "begin");
    ok(&mut s, &db, "savepoint a");
    ok(&mut s, &db, "savepoint b");
    ok(&mut s, &db, "rollback to a");
    assert_eq!(code(&mut s, &db, "release b"), "3B001", "b went with the rollback to a");
    ok(&mut s, &db, "rollback");
    assert_eq!(code(&mut s, &db, "release a"), "25P01");
    // leftovers of a session that died are cleaned up, savepoints before their transaction
    ok(&mut s, &db, "begin");
    ok(&mut s, &db, "savepoint a");
    ok(&mut s, &db, "savepoint b");
    std::mem::forget(s);
    chronos::sql::cleanup(&db);
    assert!(db.branches().iter().all(|(b, _)| !b.starts_with("_tx_")));
}
