//! Sequence access isn't MVCC: a transaction's nextval uses the sequence's definition as other
//! sessions last committed it, not as its snapshot saw it; its own uncommitted ALTER it sees.

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

fn after_alter(isolation: &str) -> Vec<String> {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create table t (x int)");
    q(&db, &mut a, &format!("begin isolation level {isolation}"));
    q(&db, &mut a, "select count(*) from t");
    q(&db, &mut b, "alter sequence s increment by 10");
    let got = q(&db, &mut a, "select nextval('s') || ',' || nextval('s')");
    q(&db, &mut a, "commit");
    got
}

#[test]
fn read_committed_nextval_uses_the_committed_definition() {
    assert_eq!(after_alter("read committed"), ["1,11"]);
}

#[test]
fn repeatable_read_nextval_uses_the_committed_definition() {
    assert_eq!(after_alter("repeatable read"), ["1,11"]);
}

/// A column default's nextval, taken by an INSERT, counts the same way.
#[test]
fn an_insert_default_uses_the_committed_definition() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create table t (x bigint default nextval('s'), y int)");
    q(&db, &mut a, "begin isolation level repeatable read");
    q(&db, &mut a, "select count(*) from t");
    q(&db, &mut b, "alter sequence s increment by 10");
    q(&db, &mut a, "insert into t (y) values (1)");
    q(&db, &mut a, "insert into t (y) values (2)");
    assert_eq!(q(&db, &mut a, "select x from t order by y"), ["1", "11"]);
    q(&db, &mut a, "commit");
}

/// A MAXVALUE another session committed stops the transaction's nextval.
#[test]
fn a_committed_maxvalue_counts() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create table t (x int)");
    q(&db, &mut a, "begin isolation level repeatable read");
    q(&db, &mut a, "select count(*) from t");
    q(&db, &mut b, "alter sequence s maxvalue 2");
    assert_eq!(q(&db, &mut a, "select nextval('s') || ',' || nextval('s')"), ["1,2"]);
    let e = a.run(&db, "select nextval('s')", &[]).unwrap_err();
    assert_eq!(e.code, "2200H", "{e}");
}

/// The transaction's own ALTER, not yet committed, is what it counts by.
#[test]
fn its_own_alter_counts() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "alter sequence s increment by 100");
    assert_eq!(q(&db, &mut a, "select nextval('s') || ',' || nextval('s')"), ["1,101"]);
    q(&db, &mut a, "commit");
    assert_eq!(q(&db, &mut b, "select nextval('s')"), ["201"]);
}

/// Its own ALTER counts even when it leaves the definition as the transaction began with it.
#[test]
fn its_own_alter_back_counts() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create table t (x int)");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select count(*) from t");
    q(&db, &mut b, "alter sequence s increment by 10");
    q(&db, &mut a, "alter sequence s increment by 1");
    assert_eq!(q(&db, &mut a, "select nextval('s') || ',' || nextval('s')"), ["1,2"]);
    q(&db, &mut a, "commit");
    // and it commits as it says
    assert_eq!(q(&db, &mut b, "select increment_by from pg_sequences where sequencename = 's'"), ["1"]);
    assert_eq!(q(&db, &mut b, "select nextval('s')"), ["3"]);
}

/// Runs `alter` in another session while `a`'s transaction is open, and `mine` in `a` meanwhile,
/// then commits `a`: what `mine` gave, and what `alter` gave once `a` committed.
fn alter_meanwhile(db: &Db, a: &mut Session, alter: &[&str], mine: &[&str]) -> (Vec<String>, Vec<String>) {
    let alter: Vec<String> = alter.iter().map(|s| s.to_string()).collect();
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            let mut b = Session::new(MAIN);
            let got: Vec<String> = alter.iter().flat_map(|sql| q(db, &mut b, sql)).collect();
            tx.send(got).unwrap();
        });
        // its ALTER waits for the transaction that took values, as Postgres's does for its lock
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "ALTER SEQUENCE didn't wait");
        let got = mine.iter().flat_map(|sql| q(db, a, sql)).collect();
        q(db, a, "commit");
        (got, rx.recv_timeout(Duration::from_secs(10)).expect("ALTER SEQUENCE still waiting after COMMIT"))
    })
}

/// Once the transaction took a value, it counts by the definition it took it by: another
/// session's ALTER SEQUENCE waits for it to end, and then counts on from its values.
#[test]
fn a_later_alter_waits_for_the_values_taken() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "begin");
    assert_eq!(q(&db, &mut a, "select nextval('s')"), ["1"]);
    let got = alter_meanwhile(
        &db,
        &mut a,
        &["alter sequence s increment by 10", "select nextval('s')"],
        &["select nextval('s') || ',' || nextval('s')"],
    );
    assert_eq!(got, (vec!["2,3".to_string()], vec!["13".to_string()]));
}

/// The same for a MAXVALUE, and for a sequence the transaction setval'd.
#[test]
fn a_later_maxvalue_waits_for_the_values_taken() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create sequence s2");
    q(&db, &mut a, "begin");
    assert_eq!(q(&db, &mut a, "select nextval('s')"), ["1"]);
    assert_eq!(q(&db, &mut a, "select setval('s2', 5)"), ["5"]);
    let got = alter_meanwhile(
        &db,
        &mut a,
        &["alter sequence s maxvalue 3", "alter sequence s2 increment by 10", "select nextval('s2')"],
        &["select nextval('s') || ',' || nextval('s')", "select nextval('s2') || ',' || nextval('s2')"],
    );
    assert_eq!(got, (vec!["2,3".to_string(), "6,7".to_string()], vec!["17".to_string()]));
    let e = a.run(&db, "select nextval('s')", &[]).unwrap_err();
    assert_eq!(e.code, "2200H", "{e}");
}

/// Two transactions each holding a sequence the other alters: one gets 40P01, as in Postgres,
/// instead of both waiting forever.
#[test]
fn alters_waiting_for_each_other_are_a_deadlock() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s1");
    q(&db, &mut a, "create sequence s2");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select nextval('s1')");
    let codes = std::thread::scope(|scope| {
        let db = &db;
        let other = scope.spawn(move || {
            let mut b = Session::new(MAIN);
            q(db, &mut b, "begin");
            q(db, &mut b, "select nextval('s2')");
            let r = b.run(db, "alter sequence s1 increment by 10", &[]).err().map(|e| e.code);
            let _ = b.run(db, "rollback", &[]);
            r
        });
        std::thread::sleep(Duration::from_millis(200));
        let mine = a.run(db, "alter sequence s2 increment by 10", &[]).err().map(|e| e.code);
        let _ = a.run(db, "rollback", &[]);
        (mine, other.join().unwrap())
    });
    assert!(matches!(codes, (Some("40P01"), None) | (None, Some("40P01"))), "{codes:?}");
}

/// `last_value` agrees with the values nextval handed out.
#[test]
fn last_value_follows_the_committed_definition() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create sequence s2");
    q(&db, &mut a, "create table t (x int)");
    q(&db, &mut a, "begin isolation level repeatable read");
    q(&db, &mut a, "select count(*) from t");
    q(&db, &mut b, "alter sequence s increment by 10");
    q(&db, &mut b, "alter sequence s2 increment by 10");
    assert_eq!(q(&db, &mut a, "select nextval('s') || ',' || nextval('s')"), ["1,11"]);
    assert_eq!(q(&db, &mut a, "select last_value from s"), ["11"]);
    assert_eq!(q(&db, &mut a, "select last_value from pg_sequences where sequencename = 's'"), ["11"]);
    assert_eq!(q(&db, &mut a, "select setval('s2', 5)"), ["5"]);
    assert_eq!(q(&db, &mut a, "select nextval('s2') || ',' || nextval('s2')"), ["15,25"]);
    assert_eq!(q(&db, &mut a, "select last_value from s2"), ["25"]);
    q(&db, &mut a, "commit");
}

/// Its own ALTER builds on the definition as last committed, as Postgres's reads the catalog as
/// it is now: the values go on from there, and COMMIT keeps both changes.
#[test]
fn its_own_alter_builds_on_the_committed_definition() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create sequence s2");
    q(&db, &mut a, "create table t (x int)");
    q(&db, &mut a, "begin isolation level repeatable read");
    q(&db, &mut a, "select count(*) from t");
    q(&db, &mut b, "alter sequence s increment by 10");
    q(&db, &mut b, "alter sequence s2 increment by 10");
    assert_eq!(q(&db, &mut a, "select nextval('s') || ',' || nextval('s')"), ["1,11"]);
    q(&db, &mut a, "alter sequence s maxvalue 1000");
    assert_eq!(q(&db, &mut a, "select nextval('s')"), ["21"]);
    // without values taken first, too
    q(&db, &mut a, "alter sequence s2 maxvalue 1000");
    assert_eq!(q(&db, &mut a, "select nextval('s2') || ',' || nextval('s2')"), ["1,11"]);
    q(&db, &mut a, "commit");
    let seqs = "select sequencename, increment_by, max_value from pg_sequences order by 1";
    assert_eq!(q(&db, &mut b, seqs), ["s|10|1000", "s2|10|1000"]);
    assert_eq!(q(&db, &mut b, "select nextval('s') || ',' || nextval('s2')"), ["31,21"]);
}

/// An ALTER rolled back to a savepoint is gone: the transaction counts by the definition as last
/// committed again, whether or not it took values before.
#[test]
fn an_alter_rolled_back_to_a_savepoint_is_gone() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create sequence s2");
    q(&db, &mut a, "create table t (x int)");
    q(&db, &mut a, "begin isolation level repeatable read");
    q(&db, &mut a, "select count(*) from t");
    q(&db, &mut b, "alter sequence s increment by 10");
    q(&db, &mut b, "alter sequence s2 increment by 10");
    assert_eq!(q(&db, &mut a, "select nextval('s') || ',' || nextval('s')"), ["1,11"]);
    q(&db, &mut a, "savepoint p");
    q(&db, &mut a, "alter sequence s cycle");
    q(&db, &mut a, "alter sequence s2 increment by 5");
    q(&db, &mut a, "rollback to p");
    assert_eq!(q(&db, &mut a, "select nextval('s') || ',' || nextval('s')"), ["21,31"]);
    assert_eq!(q(&db, &mut a, "select nextval('s2') || ',' || nextval('s2')"), ["1,11"]);
    q(&db, &mut a, "commit");
    assert_eq!(q(&db, &mut b, "select nextval('s') || ',' || nextval('s2')"), ["41,21"]);
}

/// A sequence a transaction altered and rolled back to a savepoint stays the transaction's until
/// it ends, as Postgres keeps its lock on a sequence for the whole transaction: another session's
/// ALTER SEQUENCE waits. The transaction counts by the definition last committed.
#[test]
fn a_sequence_altered_before_a_rollback_to_savepoint_is_still_held() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create table t (x int)");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select count(*) from t");
    q(&db, &mut b, "alter sequence s increment by 10");
    q(&db, &mut a, "savepoint p");
    q(&db, &mut a, "alter sequence s increment by 5");
    q(&db, &mut a, "rollback to p");
    let got = alter_meanwhile(
        &db,
        &mut a,
        &["alter sequence s increment by 3", "select nextval('s')"],
        &["select nextval('s') || ',' || nextval('s')"],
    );
    assert_eq!(got, (vec!["1,11".to_string()], vec!["14".to_string()]));
}

/// Runs `sql` in `a` while `b`'s transaction has an ALTER SEQUENCE not yet committed, then ends
/// `b`'s with `end`: `a`'s nextval waits for it, as Postgres's does for its lock. What `sql` gave.
fn nextval_meanwhile(begin: bool, end: &str) -> Vec<String> {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut b, "begin");
    q(&db, &mut b, "alter sequence s increment by 10");
    if begin {
        q(&db, &mut a, "begin");
    }
    let first = std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        let db = &db;
        let a = &mut a;
        scope.spawn(move || tx.send(q(db, a, "select nextval('s')")).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "nextval didn't wait for the ALTER");
        q(db, &mut b, end);
        rx.recv_timeout(Duration::from_secs(10)).expect("nextval still waiting after the ALTER's transaction ended")
    });
    let mut got = first;
    got.extend(q(&db, &mut a, "select nextval('s')"));
    if begin {
        q(&db, &mut a, "commit");
    }
    got.extend(q(&db, &mut b, "select nextval('s')"));
    got
}

#[test]
fn nextval_waits_for_another_transactions_alter_commit() {
    assert_eq!(nextval_meanwhile(true, "commit"), ["1", "11", "21"]);
    assert_eq!(nextval_meanwhile(false, "commit"), ["1", "11", "21"]);
}

#[test]
fn nextval_waits_for_another_transactions_alter_rollback() {
    assert_eq!(nextval_meanwhile(true, "rollback"), ["1", "2", "3"]);
    assert_eq!(nextval_meanwhile(false, "rollback"), ["1", "2", "3"]);
}

/// An ALTER SEQUENCE waiting for a transaction that waits for its session's advisory lock: one
/// gets 40P01, as in Postgres, instead of both waiting forever.
#[test]
fn an_alter_and_an_advisory_lock_waiting_for_each_other_are_a_deadlock() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "set statement_timeout = 5000"); // (a missed deadlock fails, not hangs)
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select nextval('s')");
    let codes = std::thread::scope(|scope| {
        let db = &db;
        let other = scope.spawn(move || {
            let mut b = Session::new(MAIN);
            q(db, &mut b, "set statement_timeout = 5000");
            q(db, &mut b, "begin");
            q(db, &mut b, "select pg_advisory_xact_lock(4242)");
            let r = b.run(db, "alter sequence s increment by 10", &[]).err().map(|e| e.code);
            let _ = b.run(db, "rollback", &[]);
            r
        });
        std::thread::sleep(Duration::from_millis(200));
        let mine = a.run(db, "select pg_advisory_xact_lock(4242)", &[]).err().map(|e| e.code);
        let _ = a.run(db, "rollback", &[]);
        (mine, other.join().unwrap())
    });
    assert!(matches!(codes, (Some("40P01"), None) | (None, Some("40P01"))), "{codes:?}");
}

/// setval and currval wait for it too, so the transaction counts on by its definition.
#[test]
fn setval_and_currval_wait_for_another_transactions_alter() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    let mut meanwhile = |a: &mut Session, alter: &str, sql: &str| {
        q(&db, &mut b, "begin");
        q(&db, &mut b, alter);
        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let db = &db;
            scope.spawn(move || tx.send(q(db, a, sql)).unwrap());
            assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "{sql} didn't wait for the ALTER");
            q(db, &mut b, "commit");
            rx.recv_timeout(Duration::from_secs(10)).expect("still waiting after the ALTER's transaction ended")
        })
    };
    q(&db, &mut a, "begin");
    assert_eq!(meanwhile(&mut a, "alter sequence s increment by 10", "select setval('s', 5)"), ["5"]);
    assert_eq!(q(&db, &mut a, "select nextval('s')"), ["15"]);
    q(&db, &mut a, "commit");
    q(&db, &mut a, "begin");
    assert_eq!(meanwhile(&mut a, "alter sequence s increment by 2", "select currval('s')"), ["15"]);
    assert_eq!(q(&db, &mut a, "select nextval('s')"), ["17"]);
    q(&db, &mut a, "commit");
}

/// What such a COMMIT kept is what replaying the log keeps.
#[test]
fn its_own_alter_over_a_commit_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!("chronos-seq-committed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let seqs = "select sequencename, increment_by, max_value from pg_sequences order by 1";
    {
        let db = Db::open(&dir).unwrap();
        let mut a = Session::new(MAIN);
        let mut b = Session::new(MAIN);
        q(&db, &mut a, "create sequence s");
        q(&db, &mut a, "create sequence s2");
        q(&db, &mut a, "create table t (x int)");
        q(&db, &mut a, "begin");
        q(&db, &mut a, "select count(*) from t");
        q(&db, &mut b, "alter sequence s increment by 10");
        q(&db, &mut b, "alter sequence s2 increment by 10");
        q(&db, &mut a, "alter sequence s maxvalue 1000");
        q(&db, &mut a, "alter sequence s2 increment by 1");
        q(&db, &mut a, "commit");
        assert_eq!(q(&db, &mut b, seqs), ["s|10|1000", "s2|1|9223372036854775807"]);
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(q(&db, &mut Session::new(MAIN), seqs), ["s|10|1000", "s2|1|9223372036854775807"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// After the transaction's own TRUNCATE ... RESTART IDENTITY: the restart starts at the START
/// last committed, and nextval, currval and last_value count by the committed definition too.
fn truncated_after_alter(isolation: &str, alter: &str, then: &[&str]) -> Vec<Vec<String>> {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create table t (x bigint, y int)");
    q(&db, &mut a, "create sequence s owned by t.x");
    q(&db, &mut a, "alter table t alter column x set default nextval('s')");
    q(&db, &mut a, "create table w (x int)");
    q(&db, &mut a, "insert into t (y) values (0), (0), (0)");
    q(&db, &mut a, &format!("begin isolation level {isolation}"));
    q(&db, &mut a, "select count(*) from w");
    q(&db, &mut b, alter);
    q(&db, &mut a, "truncate t restart identity");
    let got = then.iter().map(|sql| q(&db, &mut a, sql)).collect();
    q(&db, &mut a, "commit");
    got
}

#[test]
fn nextval_after_its_own_restart_uses_the_committed_definition() {
    for isolation in ["read committed", "repeatable read"] {
        let then = ["select nextval('s') || ',' || nextval('s')", "select last_value from s", "select currval('s')"];
        let got = truncated_after_alter(isolation, "alter sequence s increment by 10 maxvalue 15", &then);
        assert_eq!(got, [["1,11"], ["11"], ["11"]], "{isolation}");
    }
}

#[test]
fn an_insert_after_its_own_restart_uses_the_committed_definition() {
    let then = [
        "insert into t (y) values (1), (2)",
        "select string_agg(x::text, ',' order by y) from t",
        "select nextval('s')",
    ];
    let got = truncated_after_alter("read committed", "alter sequence s increment by 10", &then);
    assert_eq!(got[1..], [["1,11"], ["21"]]);
}

#[test]
fn its_own_restart_starts_at_the_committed_start() {
    let then = [
        "select last_value || ',' || is_called from s",
        "select nextval('s') || ',' || nextval('s')",
        "select currval('s')",
    ];
    let got = truncated_after_alter("repeatable read", "alter sequence s start 100 increment by 5", &then);
    assert_eq!(got, [["100,false"], ["100,105"], ["105"]]);
}

/// ALTER SEQUENCE takes the sequence in the same step as it checks no one else uses it, autocommit
/// or in a transaction: a nextval starting meanwhile waits, instead of counting by the old
/// definition while the ALTER moves the shared counter, which handed out a value twice.
#[test]
fn nextval_during_an_alter_hands_out_no_value_twice() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    let stop = std::sync::atomic::AtomicBool::new(false);
    let taken = std::thread::scope(|scope| {
        let (db, stop) = (&db, &stop);
        let takers: Vec<_> = (0..4)
            .map(|i| {
                scope.spawn(move || {
                    let mut s = Session::new(MAIN);
                    let mut got = vec![];
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        if i % 2 == 1 {
                            q(db, &mut s, "begin");
                        }
                        got.extend(q(db, &mut s, "select nextval('s')"));
                        if i % 2 == 1 {
                            q(db, &mut s, "commit");
                        }
                    }
                    got
                })
            })
            .collect();
        for n in 0..150 {
            let tx = n % 2 == 1;
            if tx {
                q(db, &mut a, "begin");
            }
            q(db, &mut a, &format!("alter sequence s increment by {}", 1 + n % 7));
            if tx {
                q(db, &mut a, "commit");
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        takers.into_iter().flat_map(|t| t.join().unwrap()).collect::<Vec<_>>()
    });
    let mut seen = std::collections::HashSet::new();
    for v in &taken {
        assert!(seen.insert(v.clone()), "value {v} handed out twice");
    }
}

/// Its own ALTER INCREMENT after values taken by another session's committed INCREMENT rebases
/// the counter from that definition, not its snapshot's: 1, 11, then 111, as in Postgres.
#[test]
fn its_own_increment_rebases_from_the_committed_definition() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "create sequence s");
    q(&db, &mut a, "create table t (x int)");
    q(&db, &mut a, "begin isolation level repeatable read");
    q(&db, &mut a, "select count(*) from t");
    q(&db, &mut b, "alter sequence s increment by 10");
    assert_eq!(q(&db, &mut a, "select nextval('s') || ',' || nextval('s')"), ["1,11"]);
    q(&db, &mut a, "alter sequence s increment by 100");
    assert_eq!(q(&db, &mut a, "select nextval('s')"), ["111"]);
    q(&db, &mut a, "commit");
}
