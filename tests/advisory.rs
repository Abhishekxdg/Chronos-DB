//! Advisory locks (pg_advisory_lock and friends), hashtext, and IS [NOT] DISTINCT FROM.

use chronos::sql::{Output, Session, Value};
use chronos::{Db, MAIN};
use std::sync::Arc;
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

fn db() -> Arc<Db> {
    let db = Arc::new(Db::new());
    let mut s = Session::new(MAIN);
    q(&db, &mut s, "create table steps (id int primary key, name text not null)");
    db
}

/// Runs `sql` in a new session on another thread, holding a transaction open until it's told to
/// commit: what it got back (rows, or the error's code) and how long the statement took.
fn in_thread(
    db: &Arc<Db>,
    sql: &'static str,
) -> std::thread::JoinHandle<(Result<Vec<String>, &'static str>, Duration)> {
    let db = db.clone();
    std::thread::spawn(move || {
        let mut s = Session::new(MAIN);
        let t = Instant::now();
        let out = s.run(&db, sql, &[]).map(|mut o| rows(&o.pop().unwrap())).map_err(|e| e.code);
        (out, t.elapsed())
    })
}

#[test]
fn is_distinct_from() {
    let db = db();
    let mut s = Session::new(MAIN);
    let cases = [
        ("1 is distinct from 1", "false"),
        ("1 is distinct from 2", "true"),
        ("1 is distinct from null", "true"),
        ("null is distinct from null", "false"),
        ("1 is not distinct from 1", "true"),
        ("null is not distinct from null", "true"),
        ("'a' is not distinct from null", "false"),
        ("1.0 is distinct from 1", "false"),
        ("(1 + 1) is distinct from 2 and true", "false"),
    ];
    for (e, want) in cases {
        assert_eq!(q(&db, &mut s, &format!("select {e}")), [want], "{e}");
    }
    q(&db, &mut s, "insert into steps values (1, 'a'), (2, 'b')");
    q(&db, &mut s, "create table notes (id int primary key, body text)");
    q(&db, &mut s, "insert into notes values (1, null), (2, 'x')");
    assert_eq!(q(&db, &mut s, "select id from notes where body is distinct from 'x'"), ["1"]);
    assert_eq!(q(&db, &mut s, "select id from notes where body is not distinct from null"), ["1"]);
    let got = s.run(&db, "select id from notes where body is distinct from $1 order by id", &[Value::Null]).unwrap();
    assert_eq!(rows(&got[0]), ["2"]);
}

#[test]
fn a_ledger_trigger_with_is_distinct_from() {
    // payouts can't be rewritten or deleted: the trigger acrd-crm's ledger has
    let db = db();
    let mut s = Session::new(MAIN);
    q(
        &db,
        &mut s,
        "create table payouts (id int primary key, amount numeric not null, note text);
         create function ledger_guard() returns trigger language plpgsql as $$
         begin
           if tg_op = 'DELETE' then raise exception 'payouts cannot be deleted'; end if;
           if new.amount is distinct from old.amount then raise exception 'payout amounts cannot change'; end if;
           return new;
         end $$;
         create trigger payouts_guard before update or delete on payouts for each row execute function ledger_guard();
         insert into payouts values (1, 500, null);",
    );
    q(&db, &mut s, "update payouts set note = 'paid by bank' where id = 1");
    let (_, message) = fails(&db, &mut s, "update payouts set amount = 900 where id = 1");
    assert!(message.contains("cannot change"), "{message}");
    let (_, message) = fails(&db, &mut s, "delete from payouts where id = 1");
    assert!(message.contains("cannot be deleted"), "{message}");
    assert_eq!(q(&db, &mut s, "select amount, note from payouts"), ["500|paid by bank"]);
}

#[test]
fn hashtext_is_stable_and_an_int4() {
    let db = db();
    let mut s = Session::new(MAIN);
    let texts = [
        "",
        "a",
        "abc",
        "setup:org-42",
        "exactly12chr",
        "thirteen chars",
        "a much longer key than twelve bytes, twice",
    ];
    let mut ours = vec![];
    for t in texts {
        let v = q(&db, &mut s, &format!("select hashtext('{t}')")).pop().unwrap();
        let n: i64 = v.parse().unwrap();
        assert!(i32::try_from(n).is_ok(), "{t}: {n}");
        assert_eq!(q(&db, &mut s, &format!("select hashtext('{t}')")), std::slice::from_ref(&v), "the same each time");
        ours.push(v);
    }
    assert_eq!(q(&db, &mut s, "select hashtext(null)"), ["null"]);
    // and Postgres's own, where there's one to ask
    let Ok(url) = std::env::var("CHRONOS_TEST_PG") else { return };
    let mut pg = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    for (t, ours) in texts.iter().zip(ours) {
        let theirs: i32 = pg.query_one("select hashtext($1)", &[t]).unwrap().get(0);
        assert_eq!(ours, theirs.to_string(), "hashtext('{t}')");
    }
}

#[test]
fn a_transaction_lock_waits_and_then_sees_what_the_holder_committed() {
    let db = db();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select pg_advisory_xact_lock(hashtext('setup:org-42'))");
    q(&db, &mut a, "insert into steps values (1, 'seeded')");
    // B begins before A commits, reads, then waits for the lock
    let b = {
        let db = db.clone();
        std::thread::spawn(move || {
            let mut b = Session::new(MAIN);
            q(&db, &mut b, "begin");
            let before = q(&db, &mut b, "select count(*) from steps");
            let t = Instant::now();
            q(&db, &mut b, "select pg_advisory_xact_lock(hashtext('setup:org-42'))");
            let waited = t.elapsed();
            // after the wait it reads the database as A left it, so it doesn't seed again
            let after = q(&db, &mut b, "select count(*) from steps");
            q(&db, &mut b, "commit");
            (before, waited, after)
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    q(&db, &mut a, "commit");
    let (before, waited, after) = b.join().unwrap();
    assert_eq!((before, after), (vec!["0".to_string()], vec!["1".to_string()]));
    assert!(waited >= Duration::from_millis(200), "waited {waited:?}");
}

#[test]
fn a_transaction_that_wrote_before_the_lock_fails_rather_than_read_stale() {
    let db = db();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    // (one that keeps its snapshot: REPEATABLE READ; a READ COMMITTED one reads anew at each statement anyway, below)
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select pg_advisory_xact_lock(7)");
    q(&db, &mut b, "begin isolation level repeatable read");
    q(&db, &mut b, "insert into steps values (9, 'early')");
    q(&db, &mut a, "insert into steps values (1, 'a')");
    q(&db, &mut a, "commit");
    let (code, message) = fails(&db, &mut b, "select pg_advisory_xact_lock(7)");
    assert_eq!(code, "40001", "{message}");
    q(&db, &mut b, "rollback");
    // the lock was given back: it's free
    let mut c = Session::new(MAIN);
    assert_eq!(q(&db, &mut c, "select pg_try_advisory_lock(7)"), ["true"]);
    q(&db, &mut c, "select pg_advisory_unlock(7)");
}

#[test]
fn a_read_committed_transaction_that_wrote_reads_what_the_lock_holder_committed() {
    let db = db();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select pg_advisory_xact_lock(8)");
    q(&db, &mut b, "begin");
    q(&db, &mut b, "insert into steps values (9, 'early')");
    q(&db, &mut a, "insert into steps values (1, 'a')");
    q(&db, &mut a, "commit");
    // it wrote before the lock, and reads the holder's rows after it, as Postgres's READ COMMITTED does
    q(&db, &mut b, "select pg_advisory_xact_lock(8)");
    assert_eq!(q(&db, &mut b, "select id from steps order by id"), ["1", "9"]);
    q(&db, &mut b, "commit");
}

#[test]
fn try_locks_session_locks_and_unlock() {
    let db = db();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    assert_eq!(q(&db, &mut a, "select pg_try_advisory_lock(1)"), ["true"]);
    assert_eq!(q(&db, &mut a, "select pg_try_advisory_lock(1)"), ["true"], "a session takes its own lock again");
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_lock(1)"), ["false"]);
    // the two-integer key is another lock than the one-bigint key
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_lock(0, 1)"), ["true"]);
    // a session lock outlives a transaction
    q(&db, &mut a, "begin; commit");
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_lock(1)"), ["false"]);
    assert_eq!(q(&db, &mut a, "select pg_advisory_unlock(1)"), ["true"]);
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_lock(1)"), ["false"], "taken twice, given back once");
    assert_eq!(q(&db, &mut a, "select pg_advisory_unlock(1)"), ["true"]);
    assert_eq!(q(&db, &mut a, "select pg_advisory_unlock(1)"), ["false"], "not held any more");
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_lock(1)"), ["true"]);
    q(&db, &mut b, "select pg_advisory_unlock_all()");
    assert_eq!(q(&db, &mut a, "select pg_try_advisory_lock(1), pg_try_advisory_lock(0, 1)"), ["true|true"]);
    // a session that ends lets go
    drop(a);
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_lock(1)"), ["true"]);
}

#[test]
fn transaction_locks_go_at_commit_rollback_or_the_statements_end() {
    let db = db();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    for end in ["commit", "rollback"] {
        q(&db, &mut a, "begin");
        assert_eq!(q(&db, &mut a, "select pg_try_advisory_xact_lock(3)"), ["true"]);
        assert_eq!(q(&db, &mut b, "select pg_try_advisory_xact_lock(3)"), ["false"]);
        q(&db, &mut a, end);
        assert_eq!(q(&db, &mut b, "select pg_try_advisory_xact_lock(3)"), ["true"], "{end}");
    }
    // outside a transaction: held for its statement only
    q(&db, &mut a, "select pg_advisory_xact_lock(4)");
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_xact_lock(4)"), ["true"]);
}

#[test]
fn waits_end_with_the_statement_timeout_and_deadlocks_are_found() {
    let db = db();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "select pg_advisory_lock(10)");
    let mut b = Session::new(MAIN);
    q(&db, &mut b, "set statement_timeout = 200");
    let t = Instant::now();
    assert_eq!(fails(&db, &mut b, "select pg_advisory_lock(10)").0, "57014");
    assert!(t.elapsed() < Duration::from_secs(5));

    // a holds 10 and b 11; b waits for 10, then a asks for 11: a cycle
    let db2 = db.clone();
    let waiter = std::thread::spawn(move || {
        let mut b = Session::new(MAIN);
        q(&db2, &mut b, "select pg_advisory_lock(11)");
        let out = b.run(&db2, "select pg_advisory_lock(10)", &[]).map(|_| ()).map_err(|e| e.code);
        (b, out)
    });
    std::thread::sleep(Duration::from_millis(300));
    let (code, message) = fails(&db, &mut a, "select pg_advisory_lock(11)");
    assert_eq!(code, "40P01", "{message}");
    q(&db, &mut a, "select pg_advisory_unlock(10)");
    let (_b, out) = waiter.join().unwrap();
    assert_eq!(out, Ok(()), "b got 10 once a let go");
}

#[test]
fn what_advisory_locks_refuse() {
    let db = db();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    assert_eq!(fails(&db, &mut a, "select pg_advisory_lock_shared(1)").0, "0A000");
    assert_eq!(q(&db, &mut a, "select pg_advisory_lock(null)"), ["null"]);
    assert_eq!(fails(&db, &mut a, "select pg_advisory_lock(1, 5000000000)").0, "22003");
    // a statement that writes doesn't wait: it fails when another session holds the lock
    q(&db, &mut a, "select pg_advisory_lock(20)");
    let (code, _) = fails(&db, &mut b, "insert into steps select 1, 'x' from (select pg_advisory_xact_lock(20)) l");
    assert_eq!(code, "55P03");
    let (out, _) = in_thread(&db, "select pg_try_advisory_lock(20)").join().unwrap();
    assert_eq!(out, Ok(vec!["false".to_string()]));
}

#[test]
fn rollback_to_a_savepoint_lets_go_of_the_locks_taken_since() {
    let db = db();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select pg_advisory_xact_lock(30)");
    q(&db, &mut a, "savepoint s");
    q(&db, &mut a, "select pg_advisory_xact_lock(31)");
    q(&db, &mut a, "rollback to savepoint s");
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_xact_lock(31)"), ["true"], "taken after the savepoint: gone");
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_xact_lock(30)"), ["false"], "taken before it: still held");
    // a released savepoint's locks belong to the transaction
    q(&db, &mut a, "savepoint t");
    q(&db, &mut a, "select pg_advisory_xact_lock(32)");
    q(&db, &mut a, "release savepoint t");
    q(&db, &mut a, "savepoint u");
    q(&db, &mut a, "rollback to savepoint u");
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_xact_lock(32)"), ["false"]);
    q(&db, &mut a, "commit");
    assert_eq!(q(&db, &mut b, "select pg_try_advisory_xact_lock(30), pg_try_advisory_xact_lock(32)"), ["true|true"]);
}

#[test]
fn a_transaction_holding_row_locks_isnt_refreshed_away_from_them() {
    let db = db();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "insert into steps values (1, 'one')");
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select pg_advisory_xact_lock(40)");
    q(&db, &mut b, "begin");
    q(&db, &mut b, "select * from steps where id = 1 for update");
    q(&db, &mut a, "commit");
    // reading anew keeps b's row lock: another session still waits for the row
    q(&db, &mut b, "select pg_advisory_xact_lock(40)");
    let mut c = Session::new(MAIN);
    q(&db, &mut c, "set lock_timeout = '100ms'");
    assert_eq!(fails(&db, &mut c, "update steps set name = 'x' where id = 1").0, "55P03");
    q(&db, &mut b, "commit");
    q(&db, &mut c, "update steps set name = 'x' where id = 1");
}

/// pg_locks shows the locks of the database it is asked in, not those of another database the
/// same program opened (advisory locks are kept for the whole process).
#[test]
fn pg_locks_shows_only_its_own_databases_locks() {
    let (one, two) = (db(), db());
    let (mut a, mut b, mut c) = (Session::new(MAIN), Session::new(MAIN), Session::new(MAIN));
    q(&one, &mut a, "select pg_advisory_lock(31337)");
    q(&two, &mut b, "select pg_advisory_lock(0, 7)");
    let locks = "select classid, objid, objsubid, granted, pid = pg_backend_pid() from pg_locks order by objsubid";
    assert_eq!(q(&one, &mut a, locks), ["0|31337|1|true|true"]);
    assert_eq!(q(&two, &mut b, locks), ["0|7|2|true|true"]);
    // another session of the same database sees the lock (an embedded session has no pid to show)
    assert_eq!(q(&one, &mut c, "select objid, pid from pg_locks"), ["31337|null"]);
    q(&one, &mut a, "select pg_advisory_unlock_all()");
    assert_eq!(q(&one, &mut c, "select count(*) from pg_locks"), ["0"]);
    assert_eq!(q(&two, &mut c, "select count(*) from pg_locks"), ["1"]);
}

/// An error in a transaction lets go of its transaction-level advisory locks at once, as Postgres's AbortTransaction
/// does, not at its ROLLBACK; inside a savepoint, of those taken since it (session-level ones stay: they outlive a
/// rollback).
#[test]
fn an_error_lets_go_of_the_transactions_advisory_locks() {
    let db = db();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select pg_advisory_xact_lock(51), pg_advisory_lock(52)");
    q(&db, &mut a, "savepoint s");
    q(&db, &mut a, "select pg_advisory_xact_lock(53)");
    assert_eq!(fails(&db, &mut a, "select 1/0").0, "22012");
    let held = "select pg_try_advisory_xact_lock(51), pg_try_advisory_xact_lock(52), pg_try_advisory_xact_lock(53)";
    assert_eq!(q(&db, &mut b, held), ["false|false|true"], "the savepoint's part went");
    q(&db, &mut a, "rollback to s");
    q(&db, &mut a, "release s");
    assert_eq!(fails(&db, &mut a, "select 1/0").0, "22012");
    assert_eq!(q(&db, &mut b, held), ["true|false|true"], "an error outside a savepoint lets go of them all");
    q(&db, &mut a, "rollback");
    q(&db, &mut a, "select pg_advisory_unlock(52)");
}
