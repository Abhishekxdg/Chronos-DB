//! SELECT ... FOR UPDATE / FOR SHARE, and SERIALIZABLE refused rather than ignored.
//!
//! A row lock is held until its transaction ends, and a statement that meets a row another transaction holds in a
//! mode that conflicts waits for it (`lock_timeout` ends the wait with 55P03), as Postgres's does; NOWAIT refuses
//! such a row, and SKIP LOCKED leaves it out. What the waiting statement reads afterwards is what the holder
//! committed (READ COMMITTED), or 40001 if it reads from a snapshot taken before (REPEATABLE READ).

use chronos::sql::{Output, Session, Value, run};
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

fn q(db: &Db, s: &mut Session, sql: &str) -> Output {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap()
}

fn fails(db: &Db, s: &mut Session, sql: &str) -> (&'static str, String) {
    let e = s.run(db, sql, &[]).expect_err(sql);
    (e.code, e.message)
}

/// Units that can be held, and the holds on them.
fn units() -> Db {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table units (id int primary key, name text not null);
         create table holds (id int primary key, unit_id int not null, rep text not null);
         insert into units values (1, 'A-101'), (2, 'A-102');",
        &[],
    )
    .unwrap();
    db
}

/// "Check the unit is free, then hold it": two reps at once. The row lock makes the second wait for the first, and
/// then see the unit taken, where without it both holds would commit.
fn hold(db: &Db, s: &mut Session, id: i64, rep: &str, lock: &str) {
    q(db, s, "begin");
    let got = q(db, s, &format!("select id, name from units where id = 1 {lock}"));
    assert_eq!(rows(&got), ["1|A-101"]);
    let held = q(db, s, "select count(*) from holds where unit_id = 1");
    assert_eq!(rows(&held), ["0"], "each rep's snapshot shows the unit free");
    q(db, s, &format!("insert into holds values ({id}, 1, '{rep}')"));
}

#[test]
fn the_second_of_two_racing_holds_waits_and_then_sees_the_unit_taken() {
    let db = Arc::new(units());
    let mut a = Session::new(MAIN);
    hold(&db, &mut a, 1, "ravi", "for update");
    let db2 = db.clone();
    let (ready, started) = std::sync::mpsc::channel();
    let second = std::thread::spawn(move || {
        let mut b = Session::new(MAIN);
        q(&db2, &mut b, "begin");
        let t = Instant::now();
        ready.send(()).unwrap();
        let got = q(&db2, &mut b, "select id, name from units where id = 1 for update");
        let waited = t.elapsed();
        let held = q(&db2, &mut b, "select count(*) from holds where unit_id = 1");
        q(&db2, &mut b, "rollback");
        (rows(&got), rows(&held), waited)
    });
    started.recv().unwrap(); // (a loaded machine may start it late)
    std::thread::sleep(Duration::from_millis(300));
    assert!(!second.is_finished(), "it waits for the first rep's hold to end");
    assert_eq!(q(&db, &mut a, "commit").tag, "COMMIT");
    let (got, held, waited) = second.join().unwrap();
    assert_eq!(got, ["1|A-101"]);
    assert_eq!(held, ["1"], "after the wait, the unit is taken");
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert_eq!(rows(&q(&db, &mut a, "select rep from holds")), ["ravi"]);
}

#[test]
fn without_a_lock_both_holds_commit() {
    // snapshot isolation's write skew, which FOR UPDATE is there to stop
    let db = units();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    hold(&db, &mut a, 1, "ravi", "");
    hold(&db, &mut b, 2, "zara", "");
    q(&db, &mut a, "commit");
    q(&db, &mut b, "commit");
    assert_eq!(rows(&q(&db, &mut a, "select count(*) from holds")), ["2"]);
}

#[test]
fn a_locked_row_is_held_against_changes_until_the_lock_ends() {
    let db = units();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select * from units where id = 2 for update");
    // another session's change to it waits (here: gives up, so as not to wait for ever)
    q(&db, &mut b, "set lock_timeout = '100ms'");
    let (code, message) = fails(&db, &mut b, "update units set name = 'B-102' where id = 2");
    assert_eq!(code, "55P03", "{message}");
    q(&db, &mut a, "commit");
    q(&db, &mut b, "update units set name = 'B-102' where id = 2");

    // a lock taken after the change sees it
    q(&db, &mut a, "begin");
    assert_eq!(rows(&q(&db, &mut a, "select name from units where id = 2 for update")), ["B-102"]);
    q(&db, &mut a, "commit");
}

#[test]
fn a_repeatable_read_transaction_cant_lock_a_row_changed_since_its_snapshot() {
    let db = units();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin isolation level repeatable read");
    q(&db, &mut a, "select * from units where id = 2");
    q(&db, &mut b, "update units set name = 'B-102' where id = 2");
    let (code, message) = fails(&db, &mut a, "select * from units where id = 2 for update");
    assert_eq!(code, "40001", "{message}");
    q(&db, &mut a, "rollback");
}

#[test]
fn different_rows_and_shared_locks_dont_clash() {
    let db = units();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    q(&db, &mut a, "select * from units where id = 1 for update");
    q(&db, &mut b, "select * from units where id = 2 for update");
    q(&db, &mut a, "commit");
    q(&db, &mut b, "commit");

    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    q(&db, &mut a, "select * from units for share");
    q(&db, &mut b, "select * from units for key share");
    q(&db, &mut a, "commit");
    q(&db, &mut b, "commit");

    // a shared lock does stop a FOR NO KEY UPDATE
    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    q(&db, &mut a, "select * from units where id = 1 for share");
    let (code, _) = fails(&db, &mut b, "select * from units where id = 1 for no key update nowait");
    assert_eq!(code, "55P03");
    q(&db, &mut b, "rollback");
    q(&db, &mut a, "commit");
}

#[test]
fn of_locks_only_the_tables_it_names() {
    let db = units();
    run(&db, MAIN, "insert into holds values (1, 1, 'ravi')", &[]).unwrap();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    let got = q(&db, &mut a, "select u.name, h.rep from units u join holds h on h.unit_id = u.id for update of h");
    assert_eq!(rows(&got), ["A-101|ravi"]);
    assert_eq!(got.columns.as_ref().map(Vec::len), Some(2), "the key columns it reads aren't shown");
    q(&db, &mut b, "update units set name = 'X' where id = 1"); // not locked
    q(&db, &mut a, "commit");

    q(&db, &mut a, "begin");
    q(&db, &mut a, "select * from units u join holds h on h.unit_id = u.id for update of h");
    q(&db, &mut b, "set lock_timeout = '100ms'");
    assert_eq!(fails(&db, &mut b, "update holds set rep = 'bala' where id = 1").0, "55P03"); // locked
    q(&db, &mut a, "commit");

    // LEFT JOIN: a unit with no hold locks no hold
    q(&db, &mut a, "begin");
    let got =
        q(&db, &mut a, "select u.id, h.id from units u left join holds h on h.unit_id = u.id order by u.id for update");
    assert_eq!(rows(&got), ["1|1", "2|null"]);
    q(&db, &mut a, "commit");
}

#[test]
fn savepoints_keep_or_drop_their_locks() {
    let db = units();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut a, "savepoint s1");
    q(&db, &mut a, "select * from units where id = 1 for update");
    q(&db, &mut a, "rollback to savepoint s1"); // its lock goes with it
    q(&db, &mut b, "begin");
    q(&db, &mut b, "select * from units where id = 1 for update nowait");
    q(&db, &mut b, "commit");
    q(&db, &mut a, "commit");

    q(&db, &mut a, "begin");
    q(&db, &mut a, "savepoint s1");
    q(&db, &mut a, "select * from units where id = 1 for update");
    q(&db, &mut a, "release savepoint s1"); // kept
    q(&db, &mut b, "begin");
    assert_eq!(fails(&db, &mut b, "select * from units where id = 1 for update nowait").0, "55P03");
    q(&db, &mut b, "rollback");
    q(&db, &mut a, "commit");
}

#[test]
fn composite_keys_limit_after_and_outside_a_transaction() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table slots (unit int, day int, taken boolean, primary key (unit, day));
         insert into slots values (1, 1, false), (1, 2, false), (2, 1, false);",
        &[],
    )
    .unwrap();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    // outside a transaction a lock ends with its statement: it reads, and holds nothing
    let got = q(&db, &mut a, "select day from slots where unit = 1 order by day for update limit 1");
    assert_eq!(rows(&got), ["1"]);

    q(&db, &mut a, "begin");
    q(&db, &mut a, "select * from slots where unit = 1 and day = 2 for update nowait");
    q(&db, &mut b, "update slots set taken = true where unit = 1 and day = 1"); // another row
    q(&db, &mut a, "commit");

    q(&db, &mut a, "begin");
    q(&db, &mut a, "select * from slots where unit = 1 and day = 2 for update");
    q(&db, &mut b, "set lock_timeout = '100ms'");
    let (code, message) = fails(&db, &mut b, "update slots set taken = true where unit = 1 and day = 2");
    assert_eq!(code, "55P03", "{message}");
    q(&db, &mut a, "commit");
}

#[test]
fn a_function_can_lock_rows() {
    let db = Arc::new(units());
    run(
        &db,
        MAIN,
        "create function take(u int, who text, hid int) returns text language plpgsql as $$
         declare n text; held int;
         begin
           select name into n from units where id = u for update;
           select count(*) into held from holds where unit_id = u;
           if held > 0 then return 'taken'; end if;
           insert into holds values (hid, u, who); -- ids of their own: only the lock clashes
           return n;
         end $$;",
        &[],
    )
    .unwrap();
    let mut a = Session::new(MAIN);
    q(&db, &mut a, "begin");
    assert_eq!(rows(&q(&db, &mut a, "select take(1, 'ravi', 1)")), ["A-101"]);
    let db2 = db.clone();
    let (ready, started) = std::sync::mpsc::channel();
    let second = std::thread::spawn(move || {
        let mut b = Session::new(MAIN);
        q(&db2, &mut b, "begin");
        ready.send(()).unwrap();
        let out = rows(&q(&db2, &mut b, "select take(1, 'zara', 2)"));
        q(&db2, &mut b, "commit");
        out
    });
    started.recv().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(!second.is_finished(), "the second call waits for the first's lock");
    q(&db, &mut a, "commit");
    assert_eq!(second.join().unwrap(), ["taken"]);
}

#[test]
fn what_for_update_refuses() {
    let db = units();
    run(&db, MAIN, "create table notes (body text)", &[]).unwrap();
    let mut s = Session::new(MAIN);
    let refused = [
        ("select distinct name from units for update", "0A000"),
        ("select count(*) from units for update", "0A000"),
        ("select name from units group by name for update", "0A000"),
        ("select id from units union select id from holds for update", "0A000"),
        ("select * from units for update of holds", "42P01"),
    ];
    for (sql, code) in refused {
        assert_eq!(fails(&db, &mut s, sql).0, code, "{sql}");
    }
    // a subquery in FROM locks nothing, unless named
    q(&db, &mut s, "begin");
    q(&db, &mut s, "select * from (select id from units) x for update");
    assert_eq!(fails(&db, &mut s, "select * from (select id from units) x for update of x").0, "0A000");
}

#[test]
fn serializable_is_refused_not_ignored() {
    let db = units();
    let mut s = Session::new(MAIN);
    for sql in [
        "begin isolation level serializable",
        "begin transaction isolation level serializable, read only",
        "start transaction isolation level serializable",
        "begin; set transaction isolation level serializable",
        "set session characteristics as transaction isolation level serializable",
        "set default_transaction_isolation = 'serializable'",
        "set default_transaction_isolation to serializable",
        "set transaction_isolation = 'SERIALIZABLE'",
    ] {
        let (code, message) = fails(&db, &mut s, sql);
        assert_eq!(code, "0A000", "{sql}");
        assert!(message.contains("FOR UPDATE"), "{message}");
        if s.status() != b'I' {
            q(&db, &mut s, "rollback");
        }
    }
    // what snapshot isolation gives, or more lenient, is accepted
    for sql in [
        "begin isolation level repeatable read; commit",
        "begin isolation level read committed, read write; commit",
        "start transaction isolation level repeatable read; commit",
        "set default_transaction_isolation = 'read committed'",
        "set application_name = 'serializable'",
    ] {
        s.run(&db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn lock_modes_conflict_as_postgres_s_do() {
    let db = units();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    // FOR KEY SHARE and FOR NO KEY UPDATE don't conflict, in either order
    for (first, second) in [("for no key update", "for key share"), ("for key share", "for no key update")] {
        q(&db, &mut a, "begin");
        q(&db, &mut b, "begin");
        q(&db, &mut a, &format!("select * from units where id = 1 {first}"));
        q(&db, &mut b, &format!("select * from units where id = 1 {second} nowait"));
        q(&db, &mut a, "commit");
        q(&db, &mut b, "commit");
    }
    // FOR KEY SHARE does conflict with FOR UPDATE
    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    q(&db, &mut a, "select * from units where id = 1 for key share");
    assert_eq!(fails(&db, &mut b, "select * from units where id = 1 for update nowait").0, "55P03");
    q(&db, &mut b, "rollback");
    q(&db, &mut a, "commit");
}

#[test]
fn a_locking_select_that_calls_a_writing_function_is_one_statement() {
    let db = units();
    run(
        &db,
        MAIN,
        "create function note_hold(u int) returns int language plpgsql as $$
         begin
           insert into holds values (u, u, 'noted');
           if u = 2 then raise exception 'no holds on unit 2'; end if;
           return u;
         end $$;",
        &[],
    )
    .unwrap();
    let mut s = Session::new(MAIN);
    // the second row's call fails: the first row's insert goes too
    fails(&db, &mut s, "select note_hold(id) from units order by id for update");
    assert_eq!(rows(&q(&db, &mut s, "select count(*) from holds")), ["0"]);
}

fn jobs() -> Db {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table jobs (id int primary key, state text not null default 'new');
         insert into jobs (id) select g from generate_series(1, 5) g;",
        &[],
    )
    .unwrap();
    db
}

#[test]
fn skip_locked_leaves_out_rows_other_transactions_hold() {
    let db = jobs();
    let (mut a, mut b, mut c, mut d) = (Session::new(MAIN), Session::new(MAIN), Session::new(MAIN), Session::new(MAIN));
    for s in [&mut a, &mut b, &mut c] {
        q(&db, s, "begin");
    }
    assert_eq!(rows(&q(&db, &mut a, "select id from jobs order by id limit 2 for update skip locked")), ["1", "2"]);
    // the locked rows are left out before LIMIT counts
    assert_eq!(rows(&q(&db, &mut b, "select id from jobs order by id limit 2 for update skip locked")), ["3", "4"]);
    assert_eq!(rows(&q(&db, &mut c, "select id from jobs order by id limit 2 for update skip locked")), ["5"]);
    // outside a transaction, the same statement sees what is held, and holds nothing itself
    assert_eq!(rows(&q(&db, &mut d, "select id from jobs order by id for update skip locked")), Vec::<String>::new());
    q(&db, &mut a, "commit");
    assert_eq!(rows(&q(&db, &mut d, "select id from jobs order by id for update skip locked")), ["1", "2"]);
    q(&db, &mut b, "rollback");
    q(&db, &mut c, "rollback");
    assert_eq!(rows(&q(&db, &mut d, "select id from jobs order by id for update skip locked")).len(), 5);
}

#[test]
fn skip_locked_follows_postgress_lock_conflicts() {
    let db = jobs();
    let (mut a, mut d) = (Session::new(MAIN), Session::new(MAIN));
    let free = |db: &Db, s: &mut Session, mode: &str| {
        rows(&q(db, s, &format!("select id from jobs where id = 1 for {mode} skip locked")))
    };
    // a's FOR NO KEY UPDATE lets a FOR KEY SHARE by, and stops the others
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select id from jobs where id = 1 for no key update");
    assert_eq!(free(&db, &mut d, "key share"), ["1"]);
    for mode in ["share", "no key update", "update"] {
        assert_eq!(free(&db, &mut d, mode), Vec::<String>::new(), "{mode}");
    }
    q(&db, &mut a, "rollback");
    // a's FOR SHARE lets FOR SHARE and FOR KEY SHARE by
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select id from jobs where id = 1 for share");
    for mode in ["key share", "share"] {
        assert_eq!(free(&db, &mut d, mode), ["1"], "{mode}");
    }
    for mode in ["no key update", "update"] {
        assert_eq!(free(&db, &mut d, mode), Vec::<String>::new(), "{mode}");
    }
    q(&db, &mut a, "rollback");
    // FOR UPDATE stops every one
    q(&db, &mut a, "begin");
    q(&db, &mut a, "select id from jobs where id = 1 for update");
    for mode in ["key share", "share", "no key update", "update"] {
        assert_eq!(free(&db, &mut d, mode), Vec::<String>::new(), "{mode}");
    }
    q(&db, &mut a, "rollback");
}

#[test]
fn skip_locked_leaves_out_rows_another_transaction_wrote() {
    let db = jobs();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    q(&db, &mut a, "update jobs set state = 'running' where id = 1");
    q(&db, &mut a, "delete from jobs where id = 2");
    let got = q(&db, &mut b, "select id from jobs where state = 'new' order by id limit 2 for update skip locked");
    assert_eq!(rows(&got), ["3", "4"]);
    // a lock that doesn't conflict with an UPDATE of a non-key column
    assert_eq!(rows(&q(&db, &mut b, "select id from jobs where id = 1 for key share skip locked")), ["1"]);
    q(&db, &mut a, "rollback");
    q(&db, &mut b, "rollback");
}

#[test]
fn nowait_refuses_a_row_another_transaction_holds() {
    let db = jobs();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    q(&db, &mut a, "select id from jobs where id = 2 for update");
    q(&db, &mut b, "savepoint s");
    let (code, message) = fails(&db, &mut b, "select id from jobs order by id for update nowait");
    assert_eq!((code, message.as_str()), ("55P03", "could not obtain lock on row in relation \"jobs\""));
    q(&db, &mut b, "rollback to savepoint s");
    // rows past the LIMIT are never looked at
    assert_eq!(rows(&q(&db, &mut b, "select id from jobs order by id limit 1 for update nowait")), ["1"]);
    assert_eq!(rows(&q(&db, &mut b, "select id from jobs where id = 3 for update nowait")), ["3"]);
    // a plain FOR UPDATE would wait for the holder to end: it does, and gets the row
    q(&db, &mut a, "commit");
    assert_eq!(rows(&q(&db, &mut b, "select id from jobs where id = 2 for update")), ["2"]);
    q(&db, &mut b, "commit");
}

#[test]
fn skip_locked_in_a_subquery_or_with_query_is_a_job_queue() {
    let db = jobs();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    let take = "with next as (select id from jobs where state = 'new' order by id limit 1 for update skip locked) \
                update jobs set state = 'done' from next where jobs.id = next.id returning jobs.id";
    assert_eq!(rows(&q(&db, &mut a, take)), ["1"]);
    assert_eq!(rows(&q(&db, &mut b, take)), ["2"]);
    let sub = "update jobs set state = 'done' where id in \
               (select id from jobs where state = 'new' order by id limit 1 for update skip locked) returning id";
    assert_eq!(rows(&q(&db, &mut a, sub)), ["3"]);
    assert_eq!(rows(&q(&db, &mut b, sub)), ["4"]);
    q(&db, &mut a, "commit");
    q(&db, &mut b, "commit");
    assert_eq!(rows(&q(&db, &mut a, "select id from jobs where state = 'new'")), ["5"]);
}

#[test]
fn for_update_works_on_a_table_without_a_primary_key() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table notes (body text, n int);
         insert into notes values ('a', 1), ('b', 2), ('c', 3);",
        &[],
    )
    .unwrap();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    q(&db, &mut a, "begin");
    q(&db, &mut b, "begin");
    assert_eq!(rows(&q(&db, &mut a, "select body from notes where n < 3 order by n for update")), ["a", "b"]);
    assert_eq!(rows(&q(&db, &mut b, "select body from notes order by n for update skip locked")), ["c"]);
    let (code, _) = fails(&db, &mut b, "select body from notes where n = 1 for update nowait");
    assert_eq!(code, "55P03");
    // the columns it reads to find the rows again aren't shown
    let got = q(&db, &mut a, "select body from notes where n < 3 order by n for update");
    assert_eq!(got.columns.as_ref().map(Vec::len), Some(1));
    q(&db, &mut a, "rollback");
    q(&db, &mut b, "rollback");
}

/// A table without a key: rows of the same values are each a row of their own to lock (two equal rows locked are
/// two locks, and a SKIP LOCKED consumer is never given one row twice), and a row of nulls is a row to lock, not
/// taken for an outer join's missing side.
#[test]
fn rows_of_a_table_without_a_key_are_locked_one_by_one() {
    let db = Db::new();
    run(&db, MAIN, "create table kl (job text); insert into kl values ('x'), ('x'), (null), ('y')", &[]).unwrap();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    // both of two equal rows are locked
    q(&db, &mut a, "begin");
    assert_eq!(rows(&q(&db, &mut a, "select job from kl where job = 'x' for update")), ["x", "x"]);
    q(&db, &mut b, "begin");
    let skip = "select job from kl where job = 'x' for update skip locked";
    assert_eq!(rows(&q(&db, &mut b, skip)), Vec::<String>::new(), "neither is free");
    q(&db, &mut a, "rollback");
    q(&db, &mut b, "rollback");
    // one held: the other is given once
    q(&db, &mut a, "begin");
    assert_eq!(rows(&q(&db, &mut a, "select job from kl where job = 'x' limit 1 for update skip locked")), ["x"]);
    q(&db, &mut b, "begin");
    assert_eq!(rows(&q(&db, &mut b, skip)), ["x"], "the one row left, once");
    q(&db, &mut a, "rollback");
    q(&db, &mut b, "rollback");
    // a row of nulls is locked too
    q(&db, &mut a, "begin");
    assert_eq!(rows(&q(&db, &mut a, "select job from kl where job is null for update")), ["null"]);
    q(&db, &mut b, "begin");
    let nulls = "select job from kl where job is null for update skip locked";
    assert_eq!(rows(&q(&db, &mut b, nulls)), Vec::<String>::new());
    q(&db, &mut b, "set lock_timeout = '100ms'");
    let (code, message) = fails(&db, &mut b, "update kl set job = 'z' where job is null");
    assert_eq!(code, "55P03", "{message}");
    q(&db, &mut a, "rollback");
    q(&db, &mut b, "rollback");
}
