//! READ COMMITTED, Postgres's default isolation level: each statement of a transaction reads what was committed
//! before it began (plus the transaction's own writes), a write that meets a row another transaction changed re-reads
//! its newest version, and one that meets a row another transaction is still changing waits for it. REPEATABLE READ
//! keeps one snapshot, and fails a write to a row changed since with 40001.
//!
//! The scenarios that need no waiting run on a Postgres too (the one at host=/tmp, port PGPORT, as pgdiff uses; skipped
//! when there is none) and must read the same.

use chronos::sql::{Output, Session, Value};
use chronos::{Db, MAIN};
use postgres::{Client, NoTls, SimpleQueryMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn q(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    rows(&s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap())
}

fn fails(db: &Db, s: &mut Session, sql: &str) -> (&'static str, String) {
    let e = s.run(db, sql, &[]).expect_err(sql);
    (e.code, e.message)
}

fn shared(setup: &str) -> Arc<Db> {
    let db = Arc::new(Db::new());
    let mut s = Session::new(MAIN);
    for stmt in setup.split(';').filter(|s| !s.trim().is_empty()) {
        q(&db, &mut s, stmt);
    }
    db
}

// ---- the same scenario on Chronos and on Postgres

/// One scripted step: which of two connections runs it, and the SQL.
type Step = (usize, &'static str);

/// Runs `steps` on two sessions of a fresh Chronos database, and on two connections to Postgres when there is one,
/// and returns what each step gave (its rows joined, or `error <SQLSTATE>`).
fn both(setup: &str, steps: &[Step]) -> (Vec<String>, Option<Vec<String>>) {
    let db = shared(setup);
    let mut ses = [Session::new(MAIN), Session::new(MAIN)];
    let mine: Vec<String> = steps
        .iter()
        .map(|(who, sql)| match ses[*who].run(&db, sql, &[]) {
            Ok(mut o) => rows(&o.pop().unwrap()).join(";"),
            Err(e) => format!("error {}", e.code),
        })
        .collect();
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    let conn = format!("host=/tmp port={port} dbname=postgres");
    let (Ok(mut a), Ok(mut b)) = (Client::connect(&conn, NoTls), Client::connect(&conn, NoTls)) else {
        eprintln!("skipped the Postgres half: no Postgres at host=/tmp");
        return (mine, None);
    };
    let schema = format!("rc_{}_{}", std::process::id(), SCHEMA.fetch_add(1, Ordering::Relaxed));
    a.batch_execute(&format!("drop schema if exists {schema} cascade; create schema {schema}")).unwrap();
    for c in [&mut a, &mut b] {
        c.batch_execute(&format!("set search_path = {schema}, public")).unwrap();
    }
    a.batch_execute(setup).unwrap();
    let mut conns = [a, b];
    let theirs: Vec<String> = steps
        .iter()
        .map(|(who, sql)| match conns[*who].simple_query(sql) {
            Ok(msgs) => msgs
                .iter()
                .filter_map(|m| match m {
                    SimpleQueryMessage::Row(r) => {
                        Some((0..r.len()).map(|i| r.get(i).unwrap_or("null").to_string()).collect::<Vec<_>>().join("|"))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(";"),
            Err(e) => format!("error {}", e.code().map_or("?", |c| c.code())),
        })
        .collect();
    for c in &mut conns {
        let _ = c.batch_execute("rollback");
    }
    conns[0].batch_execute(&format!("drop schema {schema} cascade")).unwrap();
    (mine, Some(theirs))
}

static SCHEMA: AtomicUsize = AtomicUsize::new(0);

fn same_as_postgres(setup: &str, steps: &[Step], expect: &[&str]) {
    let (mine, theirs) = both(setup, steps);
    assert_eq!(mine, expect, "Chronos");
    if let Some(theirs) = theirs {
        assert_eq!(theirs, expect, "Postgres");
    }
}

const JQ: &str = "create table jq (id int primary key, state text not null, n int not null default 0);
                  insert into jq values (1, 'new', 0), (2, 'new', 0), (3, 'new', 0)";

#[test]
fn each_statement_sees_what_was_committed_before_it() {
    // (a) s1 reads, s0 changes and commits, s1 reads again: a new snapshot
    same_as_postgres(
        JQ,
        &[
            (1, "begin"),
            (1, "select count(*) from jq where state = 'done'"),
            (0, "update jq set state = 'done' where id = 1"),
            (1, "select count(*) from jq where state = 'done'"),
            (1, "commit"),
        ],
        &["", "0", "", "1", ""],
    );
}

#[test]
fn repeatable_read_keeps_its_snapshot_and_refuses_a_write_to_a_changed_row() {
    same_as_postgres(
        JQ,
        &[
            (1, "begin isolation level repeatable read"),
            (1, "select count(*) from jq where state = 'done'"),
            (0, "update jq set state = 'done' where id = 1"),
            (1, "select count(*) from jq where state = 'done'"),
            (1, "update jq set n = n + 1 where id = 1"),
            (1, "rollback"),
        ],
        &["", "0", "", "0", "error 40001", ""],
    );
}

#[test]
fn an_update_rereads_a_row_another_transaction_changed() {
    // (c) the row changed to no longer match (skipped), and one that still does (its newest version is updated)
    same_as_postgres(
        JQ,
        &[
            (1, "begin"),
            (1, "select count(*) from jq"),
            (0, "update jq set state = 'taken' where id = 1"),
            (0, "update jq set n = 10 where id = 2"),
            (1, "update jq set n = n + 1 where state = 'new'"),
            (1, "select id, state, n from jq order by id"),
            (1, "commit"),
        ],
        &["", "3", "", "", "", "1|taken|0;2|new|11;3|new|1", ""],
    );
}

#[test]
fn set_transaction_isolation_level_is_for_before_the_first_query() {
    same_as_postgres(
        JQ,
        &[
            (0, "show transaction_isolation"),
            (0, "begin"),
            (0, "set transaction isolation level repeatable read"),
            (0, "show transaction_isolation"),
            (0, "select 1"),
            (0, "set transaction isolation level read committed"),
            (0, "rollback"),
            (0, "begin isolation level read uncommitted"),
            (0, "show transaction_isolation"),
            (0, "commit"),
            (0, "set default_transaction_isolation = 'repeatable read'"),
            (0, "show default_transaction_isolation"),
            (0, "begin"),
            (0, "show transaction_isolation"),
            (0, "select current_setting('transaction_isolation')"),
            (0, "commit"),
            (0, "reset default_transaction_isolation"),
            (0, "show default_transaction_isolation"),
        ],
        &[
            "read committed",
            "",
            "",
            "repeatable read",
            "1",
            "error 25001",
            "",
            "",
            "read uncommitted",
            "",
            "",
            "repeatable read",
            "",
            "repeatable read",
            "repeatable read",
            "",
            "",
            "read committed",
        ],
    );
}

// ---- Chronos alone: waits and races

#[test]
fn a_write_waits_for_the_transaction_holding_its_row_and_then_reads_the_row_it_left() {
    let db = shared(JQ);
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut a, "update jq set n = 5 where id = 1");
    let db2 = db.clone();
    let (ready, started) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let mut b = Session::new(MAIN);
        q(&db2, &mut b, "begin");
        q(&db2, &mut b, "select count(*) from jq");
        let t = Instant::now();
        ready.send(()).unwrap();
        let out = q(&db2, &mut b, "update jq set n = n + 100 where id = 1 returning n");
        q(&db2, &mut b, "commit");
        (out, t.elapsed())
    });
    started.recv().unwrap(); // (a loaded machine may start it late)
    std::thread::sleep(Duration::from_millis(300));
    assert!(!waiter.is_finished(), "it must wait for the holder");
    q(&db, &mut a, "commit");
    let (out, waited) = waiter.join().unwrap();
    assert_eq!(out, ["105"], "it reads the row as the holder left it");
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert_eq!(q(&db, &mut b, "select n from jq where id = 1"), ["105"]);
}

#[test]
fn a_holder_that_rolls_back_lets_the_waiter_through_to_the_original_row() {
    let db = shared(JQ);
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "begin");
    q(&db, &mut a, "update jq set n = 5 where id = 1");
    let db2 = db.clone();
    let (ready, started) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let mut b = Session::new(MAIN);
        ready.send(()).unwrap();
        q(&db2, &mut b, "update jq set n = n + 100 where id = 1 returning n")
    });
    started.recv().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(!waiter.is_finished());
    q(&db, &mut a, "rollback");
    assert_eq!(waiter.join().unwrap(), ["100"]);
}

#[test]
fn a_counter_incremented_from_eight_threads_sums_with_no_errors() {
    let db = shared("create table c (id int primary key, n int not null); insert into c values (1, 0)");
    let errors = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|sc| {
        for w in 0..8 {
            let (db, errors) = (db.clone(), errors.clone());
            sc.spawn(move || {
                let mut s = Session::new(MAIN);
                for i in 0..25 {
                    // transactions of one statement, of two, and a bare statement
                    let script: &[&str] = match (w + i) % 3 {
                        0 => &["begin", "update c set n = n + 1", "commit"],
                        1 => &["begin", "select n from c", "update c set n = n + 1 where id = 1", "commit"],
                        _ => &["update c set n = n + 1"],
                    };
                    for sql in script {
                        if let Err(e) = s.run(&db, sql, &[]) {
                            eprintln!("{sql}: {} {}", e.code, e.message);
                            errors.fetch_add(1, Ordering::Relaxed);
                            let _ = s.run(&db, "rollback", &[]);
                            break;
                        }
                    }
                }
            });
        }
    });
    assert_eq!(errors.load(Ordering::Relaxed), 0);
    let mut s = Session::new(MAIN);
    assert_eq!(q(&db, &mut s, "select n from c"), ["200"]);
}

#[test]
fn a_counter_read_with_for_update_then_written_loses_no_update() {
    let db = shared("create table c (id int primary key, n int not null); insert into c values (1, 0)");
    let errors = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|sc| {
        for _ in 0..8 {
            let (db, errors) = (db.clone(), errors.clone());
            sc.spawn(move || {
                let mut s = Session::new(MAIN);
                for _ in 0..20 {
                    let run = |s: &mut Session, sql: &str| s.run(&db, sql, &[]).map(|mut o| rows(&o.pop().unwrap()));
                    let ok = (|| -> Result<(), chronos::sql::SqlError> {
                        run(&mut s, "begin")?;
                        let v = run(&mut s, "select n from c where id = 1 for update")?;
                        let n: i64 = v[0].parse().unwrap();
                        run(&mut s, &format!("update c set n = {} where id = 1", n + 1))?;
                        run(&mut s, "commit")?;
                        Ok(())
                    })();
                    if let Err(e) = ok {
                        eprintln!("{} {}", e.code, e.message);
                        errors.fetch_add(1, Ordering::Relaxed);
                        let _ = s.run(&db, "rollback", &[]);
                    }
                }
            });
        }
    });
    assert_eq!(errors.load(Ordering::Relaxed), 0);
    let mut s = Session::new(MAIN);
    assert_eq!(q(&db, &mut s, "select n from c"), ["160"]);
}

#[test]
fn six_workers_drain_a_queue_with_skip_locked_and_no_job_twice() {
    let db = shared("create table jobs (id int primary key, state text not null, worker int)");
    {
        let mut s = Session::new(MAIN);
        q(&db, &mut s, "insert into jobs select g, 'new', null from generate_series(1, 200) g");
    }
    let errors = Arc::new(AtomicUsize::new(0));
    let claimed: Arc<std::sync::Mutex<Vec<i64>>> = Default::default();
    std::thread::scope(|sc| {
        for w in 0..6 {
            let (db, errors, claimed) = (db.clone(), errors.clone(), claimed.clone());
            sc.spawn(move || {
                let mut s = Session::new(MAIN);
                loop {
                    let step = (|| -> Result<Option<i64>, chronos::sql::SqlError> {
                        s.run(&db, "begin", &[])?;
                        let got = s.run(
                            &db,
                            "select id from jobs where state = 'new' order by id limit 1 for update skip locked",
                            &[],
                        )?;
                        let got = rows(&got[0]);
                        let Some(id) = got.first().map(|r| r.parse::<i64>().unwrap()) else {
                            s.run(&db, "commit", &[])?;
                            return Ok(None);
                        };
                        s.run(&db, &format!("update jobs set state = 'done', worker = {w} where id = {id}"), &[])?;
                        s.run(&db, "commit", &[])?;
                        Ok(Some(id))
                    })();
                    match step {
                        Ok(Some(id)) => claimed.lock().unwrap().push(id),
                        Ok(None) => break,
                        Err(e) => {
                            eprintln!("{} {}", e.code, e.message);
                            errors.fetch_add(1, Ordering::Relaxed);
                            let _ = s.run(&db, "rollback", &[]);
                        }
                    }
                }
            });
        }
    });
    assert_eq!(errors.load(Ordering::Relaxed), 0);
    let mut ids = claimed.lock().unwrap().clone();
    ids.sort();
    assert_eq!(ids, (1..=200).collect::<Vec<i64>>(), "every job claimed once");
    let mut s = Session::new(MAIN);
    assert_eq!(q(&db, &mut s, "select count(*) from jobs where state = 'done'"), ["200"]);
}

#[test]
fn skip_locked_and_nowait_see_rows_held_by_open_transactions() {
    let db = shared(JQ);
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select id from jq where id = 1 for update");
    q(&db, &mut b, "begin");
    assert_eq!(q(&db, &mut b, "select id from jq order by id for update skip locked"), ["2", "3"]);
    assert_eq!(fails(&db, &mut b, "select id from jq where id = 1 for update nowait").0, "55P03");
    q(&db, &mut b, "rollback");
    q(&db, &mut a, "commit");
}

#[test]
fn a_lock_timeout_ends_a_wait() {
    let db = shared(JQ);
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut a, "update jq set n = 1 where id = 1");
    q(&db, &mut b, "set lock_timeout = '150ms'");
    let t = Instant::now();
    let (code, _) = fails(&db, &mut b, "update jq set n = 2 where id = 1");
    assert_eq!(code, "55P03");
    assert!(t.elapsed() >= Duration::from_millis(140));
    q(&db, &mut a, "rollback");
}

#[test]
fn two_transactions_updating_rows_in_opposite_orders_end_in_one_deadlock() {
    let db = shared(JQ);
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    q(&db, &mut a, "update jq set n = 1 where id = 1");
    q(&db, &mut b, "update jq set n = 2 where id = 2");
    let db2 = db.clone();
    let first = std::thread::spawn(move || {
        let r = a.run(&db2, "update jq set n = 1 where id = 2", &[]).map(drop).map_err(|e| e.code);
        let end = a.run(&db2, if r.is_ok() { "commit" } else { "rollback" }, &[]).map(drop).map_err(|e| e.code);
        (r, end)
    });
    std::thread::sleep(Duration::from_millis(200));
    let second = b.run(&db, "update jq set n = 2 where id = 1", &[]).map(drop).map_err(|e| e.code);
    let end = b.run(&db, if second.is_ok() { "commit" } else { "rollback" }, &[]).map(drop).map_err(|e| e.code);
    let (first, first_end) = first.join().unwrap();
    let victims = [&first, &second].iter().filter(|r| **r == &Err("40P01")).count();
    assert_eq!(victims, 1, "{first:?} {second:?}");
    assert!(first_end.is_ok() && end.is_ok());
    let mut s = Session::new(MAIN);
    assert_eq!(q(&db, &mut s, "select count(*) from jq where n <> 0"), ["2"]);
}

#[test]
fn two_inserts_of_one_key_make_the_second_wait_and_then_fail_with_a_duplicate_key() {
    // as in Postgres: the second INSERT waits for the first transaction, and is 23505 if it commits...
    for (end, second) in [("commit", Err("23505")), ("rollback", Ok(()))] {
        let db = shared("create table u (id int primary key, v text)");
        let mut a = Session::new(MAIN);
        q(&db, &mut a, "begin");
        q(&db, &mut a, "insert into u values (1, 'a')");
        let db2 = db.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let b = std::thread::spawn(move || {
            let mut b = Session::new(MAIN);
            q(&db2, &mut b, "begin");
            ready.send(()).unwrap();
            let out = b.run(&db2, "insert into u values (1, 'b')", &[]).map(drop).map_err(|e| e.code);
            let end = b.run(&db2, if out.is_ok() { "commit" } else { "rollback" }, &[]).map(drop).map_err(|e| e.code);
            (out, end)
        });
        started.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!b.is_finished(), "it waits for the first insert's transaction");
        q(&db, &mut a, end);
        let (out, ended) = b.join().unwrap();
        assert_eq!(out, second, "after {end}");
        assert_eq!(ended, Ok(()));
        let want = if end == "commit" { "a" } else { "b" };
        assert_eq!(q(&db, &mut a, "select v from u"), [want], "after {end}");
    }
}

#[test]
fn an_insert_of_a_key_committed_since_its_transaction_began_is_a_duplicate_at_once() {
    let db = shared("create table u (id int primary key, v text)");
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut b, "begin");
    q(&db, &mut b, "select 1");
    q(&db, &mut a, "insert into u values (1, 'a')");
    assert_eq!(fails(&db, &mut b, "insert into u values (1, 'b')").0, "23505");
    q(&db, &mut b, "rollback");
}

#[test]
fn rollbacks_leave_nothing_held_or_changed() {
    let db = shared(JQ);
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut a, "update jq set n = 9 where id = 1");
    q(&db, &mut a, "savepoint s");
    q(&db, &mut a, "update jq set n = 8 where id = 2");
    q(&db, &mut a, "rollback to s");
    q(&db, &mut a, "rollback");
    // nothing held: this doesn't wait, and sees the original rows
    assert_eq!(q(&db, &mut b, "update jq set n = n + 1 where id in (1, 2) returning n"), ["1", "1"]);
}

/// An UPDATE of row `row` of jq from a new session with a short lock_timeout: whether it went through (else nobody
/// let go of the row within it).
fn free(db: &Db, row: i64) -> bool {
    let mut s = Session::new(MAIN);
    q(db, &mut s, "set lock_timeout = '200ms'");
    match s.run(db, &format!("update jq set n = n + 1 where id = {row}"), &[]) {
        Ok(_) => true,
        Err(e) if e.code == "55P03" => false,
        Err(e) => panic!("{} {}", e.code, e.message),
    }
}

/// As Postgres's AbortTransaction does, an error in a transaction lets go of its row locks at once: another
/// session's UPDATE of a row it held goes through while it sits aborted, before its ROLLBACK (which it still needs:
/// its statements are refused with 25P02 until then). Whatever the error: a failed statement, a lock_timeout, a
/// statement_timeout, a constraint, one raised inside a function or a DO block.
#[test]
fn an_error_lets_go_of_the_transactions_row_locks_at_once() {
    let db = shared(
        "create table jq (id int primary key, state text not null, n int not null default 0 check (n < 1000));
         insert into jq select g, 'new', 0 from generate_series(1, 8) g",
    );
    let mut s = Session::new(MAIN);
    q(&db, &mut s, "create function boom() returns int language plpgsql as $$ begin raise exception 'boom'; end $$");
    let errors: &[&str] = &[
        "select 1/0",
        "update jq set n = 5000 where id = 8",
        "select boom()",
        "do $$ begin perform 1/0; end $$",
        "insert into jq values (1, 'dup', 0)",
        "set statement_timeout = 50; select count(*) from generate_series(1, 100000000) g where g % 7 = 3",
    ];
    for (i, bad) in errors.iter().enumerate() {
        let mut a = Session::new(MAIN);
        q(&db, &mut a, "begin");
        q(&db, &mut a, "update jq set n = n + 1 where id = 1");
        q(&db, &mut a, "select id from jq where id = 2 for update");
        assert!(!free(&db, 1), "{i}: held while the transaction is fine");
        assert!(a.run(&db, bad, &[]).is_err(), "{bad}");
        assert!(free(&db, 1) && free(&db, 2), "after {bad}: let go of");
        assert_eq!(fails(&db, &mut a, "select 1").0, "25P02");
        q(&db, &mut a, "rollback");
    }
    // a lock_timeout of its own
    let (mut a, mut c) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut c, "begin");
    q(&db, &mut c, "update jq set n = n + 1 where id = 3");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "set local lock_timeout = '100ms'");
    q(&db, &mut a, "update jq set n = n + 1 where id = 4");
    assert_eq!(fails(&db, &mut a, "update jq set n = n + 1 where id = 3").0, "55P03");
    assert!(free(&db, 4));
    q(&db, &mut a, "rollback");
    q(&db, &mut c, "rollback");
}

/// With savepoints, an error aborts the innermost one only (Postgres's AbortSubTransaction): the rows locked since it
/// was made are let go of at once, the ones locked before stay until the transaction ends. A savepoint RELEASEd
/// passes its locks to the level it was released into, so a later ROLLBACK TO, or an error past a later savepoint,
/// keeps them.
#[test]
fn an_error_in_a_savepoint_lets_go_of_what_was_locked_since_it() {
    let db = shared(JQ);
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "begin");
    q(&db, &mut a, "update jq set n = 1 where id = 1");
    q(&db, &mut a, "savepoint s");
    q(&db, &mut a, "update jq set n = 1 where id = 2");
    assert!(fails(&db, &mut a, "select 1/0").0 == "22012");
    assert!(free(&db, 2), "taken in the savepoint: let go of");
    assert!(!free(&db, 1), "taken before it: held");
    q(&db, &mut a, "rollback to s");
    assert!(!free(&db, 1), "a ROLLBACK TO lets go of nothing more");
    q(&db, &mut a, "savepoint t");
    q(&db, &mut a, "update jq set n = 1 where id = 3");
    q(&db, &mut a, "release t");
    q(&db, &mut a, "savepoint u");
    q(&db, &mut a, "rollback to u");
    assert!(!free(&db, 3), "a released savepoint's row is the transaction's");
    q(&db, &mut a, "savepoint v");
    assert!(fails(&db, &mut a, "select 1/0").0 == "22012");
    assert!(!free(&db, 3) && !free(&db, 1), "an error past a later savepoint keeps it too");
    q(&db, &mut a, "rollback");
    assert!(free(&db, 1) && free(&db, 3));
}

/// The victim of a deadlock lets go of its rows when it gets 40P01, as Postgres's does: the survivor goes on at once,
/// however long the victim's client takes to roll back.
#[test]
fn a_deadlock_victim_lets_the_survivor_through_before_it_rolls_back() {
    let db = shared(JQ);
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    q(&db, &mut a, "update jq set n = 1 where id = 1");
    q(&db, &mut b, "update jq set n = 2 where id = 2");
    let db2 = db.clone();
    let first = std::thread::spawn(move || {
        let t = Instant::now();
        let r = a.run(&db2, "update jq set n = 1 where id = 2", &[]).map(drop).map_err(|e| e.code);
        (a, r, t.elapsed())
    });
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    let second = b.run(&db, "update jq set n = 2 where id = 1", &[]).map(drop).map_err(|e| e.code);
    let second_took = t.elapsed();
    // neither has ended its transaction yet: the survivor must be through all the same
    let (mut a, first, first_took) = first.join().unwrap();
    assert_eq!([&first, &second].iter().filter(|r| **r == &Err("40P01")).count(), 1, "{first:?} {second:?}");
    assert!(first_took.max(second_took) < Duration::from_secs(3), "{first_took:?} {second_took:?}");
    for (s, r) in [(&mut a, &first), (&mut b, &second)] {
        q(&db, s, if r.is_ok() { "commit" } else { "rollback" });
    }
}

#[test]
fn an_isolation_level_survives_a_savepoint_and_a_chain() {
    let db = shared(JQ);
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "begin isolation level repeatable read");
    q(&db, &mut a, "savepoint s");
    assert_eq!(q(&db, &mut a, "show transaction_isolation"), ["repeatable read"]);
    assert_eq!(fails(&db, &mut a, "set transaction isolation level read committed").0, "25001");
    q(&db, &mut a, "rollback to s");
    q(&db, &mut a, "commit and chain");
    assert_eq!(q(&db, &mut a, "show transaction_isolation"), ["repeatable read"]);
    q(&db, &mut a, "commit");
    assert_eq!(fails(&db, &mut a, "begin isolation level serializable").0, "0A000");
}

#[test]
fn savepoints_read_what_was_committed_too() {
    let db = shared(JQ);
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut a, "update jq set n = 7 where id = 3");
    q(&db, &mut a, "savepoint s");
    q(&db, &mut b, "update jq set n = 4 where id = 1");
    assert_eq!(q(&db, &mut a, "select n from jq where id = 1"), ["4"]);
    q(&db, &mut a, "update jq set n = n + 1 where id = 1");
    q(&db, &mut a, "release s");
    q(&db, &mut a, "commit");
    assert_eq!(q(&db, &mut b, "select id, n from jq order by id"), ["1|5", "2|0", "3|7"]);
}

/// A TPC-B loop as pgbench runs it, from eight clients: none fails, and the balances add up.
#[test]
fn a_tpcb_loop_from_eight_clients_fails_nothing() {
    let db = shared(
        "create table accounts (aid int primary key, abalance int not null);
         create table tellers (tid int primary key, bid int not null, tbalance int not null);
         create table branches (bid int primary key, bbalance int not null);
         create table history (tid int, bid int, aid int, delta int, mtime timestamp default now());
         insert into branches values (1, 0);
         insert into tellers select g, 1, 0 from generate_series(1, 10) g;
         insert into accounts select g, 0 from generate_series(1, 1000) g",
    );
    let (failed, total) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    std::thread::scope(|sc| {
        for client in 0..8 {
            let (db, failed, total) = (db.clone(), failed.clone(), total.clone());
            sc.spawn(move || {
                let mut s = Session::new(MAIN);
                for i in 0..40 {
                    let aid = 1 + (client * 7919 + i * 31) % 1000;
                    let tid = 1 + (client + i) % 10;
                    let delta = (i % 11) as i64 - 5;
                    let script = [
                        "begin".to_string(),
                        format!("update accounts set abalance = abalance + {delta} where aid = {aid}"),
                        format!("select abalance from accounts where aid = {aid}"),
                        format!("update tellers set tbalance = tbalance + {delta} where tid = {tid}"),
                        format!("update branches set bbalance = bbalance + {delta} where bid = 1"),
                        format!("insert into history (tid, bid, aid, delta) values ({tid}, 1, {aid}, {delta})"),
                        "commit".to_string(),
                    ];
                    total.fetch_add(1, Ordering::Relaxed);
                    for sql in &script {
                        if let Err(e) = s.run(&db, sql, &[]) {
                            eprintln!("{sql}: {} {}", e.code, e.message);
                            failed.fetch_add(1, Ordering::Relaxed);
                            let _ = s.run(&db, "rollback", &[]);
                            break;
                        }
                    }
                }
            });
        }
    });
    assert_eq!(total.load(Ordering::Relaxed), 320);
    assert_eq!(failed.load(Ordering::Relaxed), 0, "no transaction fails");
    let mut s = Session::new(MAIN);
    let sums = q(
        &db,
        &mut s,
        "select (select sum(abalance) from accounts), (select sum(tbalance) from tellers), \
         (select sum(bbalance) from branches), (select sum(delta) from history), (select count(*) from history)",
    );
    let parts: Vec<&str> = sums[0].split('|').collect();
    assert!(parts[..4].iter().all(|p| *p == parts[0]), "balances agree: {sums:?}");
    assert_eq!(parts[4], "320");
}

/// INSERT ... ON CONFLICT from eight transactions at once, on keys none of them found there: as in Postgres, the
/// second to want a key waits for the first to end and then updates the row (or leaves it), and nobody fails.
#[test]
fn upserts_of_one_key_from_eight_threads_lose_nothing() {
    let db = shared("create table hits (id int primary key, n int not null, who text)");
    let errors = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|sc| {
        for w in 0..8 {
            let (db, errors) = (db.clone(), errors.clone());
            sc.spawn(move || {
                let mut s = Session::new(MAIN);
                for i in 0..15 {
                    let key = i % 5; // five keys, each new to the first to reach it
                    let script = [
                        "begin".to_string(),
                        format!(
                            "insert into hits values ({key}, 1, 'w{w}') on conflict (id) do update set n = hits.n + 1"
                        ),
                        format!("insert into hits values ({}, 1, 'w{w}') on conflict (id) do nothing", key + 100),
                        "commit".to_string(),
                    ];
                    for sql in &script {
                        if let Err(e) = s.run(&db, sql, &[]) {
                            eprintln!("{sql}: {} {}", e.code, e.message);
                            errors.fetch_add(1, Ordering::Relaxed);
                            let _ = s.run(&db, "rollback", &[]);
                            break;
                        }
                    }
                }
            });
        }
    });
    assert_eq!(errors.load(Ordering::Relaxed), 0);
    let mut s = Session::new(MAIN);
    assert_eq!(q(&db, &mut s, "select count(*), sum(n) from hits where id < 100"), ["5|120"]);
    assert_eq!(q(&db, &mut s, "select count(*), sum(n) from hits where id >= 100"), ["5|5"]);
}

/// A transaction that read anew at each statement (and so was rebased onto what others committed), with a savepoint
/// among them, is in the log as it was: the database that is opened again holds the same rows, and the log of a
/// world other than main (whose writes are batched) replays the same.
#[test]
fn a_database_reopens_with_what_read_committed_transactions_did() {
    let dir = std::env::temp_dir().join(format!("chronos-read-committed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let want = {
        let db = Db::open(&dir).unwrap();
        let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
        q(&db, &mut a, "create table jq (id int primary key, state text not null, n int not null default 0)");
        q(&db, &mut a, "insert into jq select g, 'new', 0 from generate_series(1, 20) g");
        for world in [MAIN, "w1"] {
            if world != MAIN {
                q(&db, &mut a, "create world w1");
                q(&db, &mut a, "use world w1");
                q(&db, &mut b, "use world w1");
                q(&db, &mut a, "update jq set n = n + 1000 where id = 20");
            }
            for i in 1..=6 {
                q(&db, &mut a, "begin");
                q(&db, &mut a, &format!("update jq set n = n + 1 where id = {i}"));
                q(&db, &mut b, &format!("update jq set n = n + 10 where id = {}", i + 6)); // commits meanwhile
                q(&db, &mut a, "savepoint s");
                q(&db, &mut a, &format!("update jq set state = 'done' where id = {}", i + 6));
                q(&db, &mut b, &format!("update jq set n = n + 100 where id = {}", i + 12));
                q(&db, &mut a, "update jq set n = n + 1 where id = 1");
                if i % 2 == 0 {
                    q(&db, &mut a, "rollback to s");
                }
                q(&db, &mut a, "release s");
                q(&db, &mut a, "commit");
            }
        }
        q(&db, &mut a, "use world main");
        let out = (q(&db, &mut a, "select id, state, n from jq order by id"), {
            q(&db, &mut a, "use world w1");
            q(&db, &mut a, "select id, state, n from jq order by id")
        });
        db.close().unwrap();
        out
    };
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(q(&db, &mut s, "select id, state, n from jq order by id"), want.0);
    q(&db, &mut s, "use world w1");
    assert_eq!(q(&db, &mut s, "select id, state, n from jq order by id"), want.1);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A transaction (and its savepoint) open while another session commits more rows than it changed itself (see
/// `Core::rebase_tx`): its next statement sees them all, with its own changes on top, as Postgres's does.
#[test]
fn a_big_commit_meanwhile_is_seen_under_the_transactions_own_changes() {
    same_as_postgres(
        "create table big (id int primary key, v int not null); insert into big values (1, 1), (2, 2), (3, 3)",
        &[
            (1, "begin"),
            (1, "update big set v = 10 where id = 1"),
            (1, "savepoint s"),
            (1, "update big set v = 20 where id = 2"),
            (0, "insert into big select g, g from generate_series(4, 3003) g"),
            (1, "select count(*), sum(v) from big"),
            (1, "rollback to s"),
            (1, "select v from big where id in (1, 2, 3003) order by id"),
            (0, "update big set v = 0 where id = 3003"),
            (1, "update big set v = v + 1 where id in (3, 3003) returning v"),
            (1, "commit"),
            (0, "select count(*), sum(v) from big"),
        ],
        &["", "", "", "", "", "3003|4510533", "", "10;2;3003", "", "4;1", "", "3003|4507514"],
    );
}

/// The same on disk, with a checkpoint while the transaction is open: what it did is there after a reopen that
/// replays the log, as it was before.
#[test]
fn a_transaction_open_across_a_big_commit_and_a_checkpoint_replays_the_same() {
    let dir = std::env::temp_dir().join(format!("chronos-rc-big-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sum = |a: i64, b: i64| (a..=b).sum::<i64>();
    let want = {
        let db = Db::open(&dir).unwrap();
        let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
        q(&db, &mut b, "create table big (id int primary key, v int not null)");
        q(&db, &mut b, "insert into big select g, g from generate_series(1, 10) g");
        q(&db, &mut a, "begin");
        q(&db, &mut a, "update big set v = -1 where id = 1");
        q(&db, &mut a, "savepoint s");
        q(&db, &mut a, "update big set v = -2 where id = 2");
        q(&db, &mut b, "insert into big select g, g from generate_series(11, 3010) g");
        assert_eq!(q(&db, &mut a, "select count(*), sum(v) from big"), [format!("3010|{}", sum(3, 3010) - 3)]);
        db.checkpoint().unwrap();
        q(&db, &mut b, "update big set v = 0 where id = 3010");
        assert_eq!(q(&db, &mut a, "select v from big where id in (1, 2, 3010) order by id"), ["-1", "-2", "0"]);
        q(&db, &mut a, "rollback to s");
        q(&db, &mut a, "update big set v = v + 100 where id = 5");
        q(&db, &mut b, "insert into big select g, g from generate_series(3011, 5010) g");
        assert_eq!(
            q(&db, &mut a, "select v from big where id in (1, 2, 5, 5010) order by id"),
            ["-1", "2", "105", "5010"]
        );
        q(&db, &mut a, "commit");
        let out = q(&db, &mut a, "select count(*), sum(v) from big");
        assert_eq!(out, [format!("5010|{}", sum(1, 5010) - 2 + 100 - 3010)]);
        out
    }; // (dropped without a close: the reopen replays the log since the checkpoint)
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(q(&db, &mut s, "select count(*), sum(v) from big"), want);
    assert_eq!(q(&db, &mut s, "select v from big where id in (1, 2, 5, 3010) order by id"), ["-1", "2", "105", "0"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
