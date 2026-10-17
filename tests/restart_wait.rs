//! A counter one transaction restarts: another session's nextval of it waits for that transaction,
//! as Postgres's waits for the lock the restart took, so the two never hand out the same value.

use chronos::sql::{Output, Session, Value};
use chronos::{Db, MAIN};
use std::sync::mpsc;
use std::time::Duration;

fn rows(o: &Output) -> Vec<String> {
    o.rows.iter().map(|r| r.iter().map(Value::to_text).collect::<Vec<_>>().join("|")).collect()
}

fn q(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    rows(&s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap())
}

fn waits_for(end: &str) -> (String, String) {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "select nextval('s'), nextval('s'), nextval('s')");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "alter sequence s restart with 1");
    let mine = q(&db, &mut a, "select nextval('s')").remove(0);
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let db = &db;
        scope.spawn(move || {
            let mut b = Session::new(MAIN);
            tx.send(q(db, &mut b, "select nextval('s')").remove(0)).unwrap();
        });
        // it waits while the restart is pending
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "nextval didn't wait for the restart");
        q(db, &mut a, end);
        (mine, rx.recv_timeout(Duration::from_secs(10)).expect("nextval still waiting after the transaction ended"))
    })
}

#[test]
fn nextval_waits_for_a_restart_another_transaction_commits() {
    assert_eq!(waits_for("commit"), ("1".into(), "2".into()));
}

#[test]
fn nextval_waits_for_a_restart_another_transaction_rolls_back() {
    assert_eq!(waits_for("rollback"), ("1".into(), "4".into()));
}

/// Two transactions restarting two counters in opposite orders: one gets 40P01, as in Postgres,
/// instead of both waiting forever.
#[test]
fn restarts_waiting_for_each_other_are_a_deadlock() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s1");
    q(&db, &mut a, "create sequence s2");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "alter sequence s1 restart with 1");
    let codes = std::thread::scope(|scope| {
        let db = &db;
        let other = scope.spawn(move || {
            let mut b = Session::new(MAIN);
            q(db, &mut b, "begin");
            q(db, &mut b, "alter sequence s2 restart with 1");
            let r = b.run(db, "alter sequence s1 restart with 1", &[]).err().map(|e| e.code);
            let _ = b.run(db, "rollback", &[]);
            r
        });
        std::thread::sleep(Duration::from_millis(200));
        let mine = a.run(db, "alter sequence s2 restart with 1", &[]).err().map(|e| e.code);
        let _ = a.run(db, "rollback", &[]);
        (mine, other.join().unwrap())
    });
    assert!(matches!(codes, (Some("40P01"), None) | (None, Some("40P01"))), "{codes:?}");
}

/// A restart rolled back to a savepoint no longer holds other sessions up.
#[test]
fn a_restart_rolled_back_to_a_savepoint_lets_others_go_on() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "savepoint p");
    q(&db, &mut a, "alter sequence s restart with 1");
    q(&db, &mut a, "rollback to p");
    let mut b = Session::new(MAIN);
    assert_eq!(q(&db, &mut b, "select nextval('s')"), ["1"]);
    q(&db, &mut a, "commit");
}

/// A session dropped in the middle of a restarting transaction lets others go on.
#[test]
fn a_dropped_session_lets_others_go_on() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "alter sequence s restart with 5");
    q(&db, &mut a, "select setval('s', 3)");
    drop(a);
    let mut b = Session::new(MAIN);
    q(&db, &mut b, "set statement_timeout = 5000");
    assert_eq!(q(&db, &mut b, "select nextval('s')").len(), 1); // (it doesn't wait for the dropped one)
}

/// The waiter takes its value by the definition the restarting transaction committed.
#[test]
fn the_waiter_uses_the_committed_definition() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "alter sequence s increment by 10 restart with 1");
    assert_eq!(q(&db, &mut a, "select nextval('s')"), ["1"]);
    let got = std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let db = &db;
        scope.spawn(move || {
            let mut b = Session::new(MAIN);
            tx.send(q(db, &mut b, "select nextval('s')").remove(0)).unwrap();
        });
        std::thread::sleep(Duration::from_millis(200));
        q(db, &mut a, "commit");
        rx.recv_timeout(Duration::from_secs(10)).unwrap()
    });
    assert_eq!(got, "11");
}
