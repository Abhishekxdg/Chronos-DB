//! Merging part of a world (ONLY TABLES / ONLY KEYS) and merging a world into another world
//! (INTO): the world stays open, rows already merged aren't merged again, real conflicts still
//! are, constraints hold, a restart replays it all exactly, and agents need the rights to both.

use chronos::sql::{Output, Session, Value};
use chronos::{AgentSpec, Can, Db, MAIN};

fn q(db: &Db, world: &str, sql: &str) -> Output {
    chronos::sql::run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap()
}

fn fails(db: &Db, world: &str, sql: &str) -> chronos::sql::SqlError {
    chronos::sql::run(db, world, sql, &[]).err().unwrap_or_else(|| panic!("{sql} should fail"))
}

/// A dry run's rows without the line per table it leads with.
fn plan_rows(o: &Output) -> Vec<String> {
    rows(o).into_iter().filter(|r| !r.contains("|summary|")).collect()
}

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn diff_keys(db: &Db, w: &str) -> Vec<String> {
    db.diff(w).unwrap().into_iter().map(|c| chronos::sql::show_key(&c.key).into_owned()).collect()
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chronos-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Tables t and u on main; world x changes rows of both.
fn setup(db: &Db) {
    q(db, MAIN, "create table t (id int primary key, v text)");
    q(db, MAIN, "create table u (id int primary key, v text)");
    q(db, MAIN, "insert into t values (1, 'one'), (2, 'two'); insert into u values (1, 'uno')");
    db.fork(MAIN, "x").unwrap();
    q(
        db,
        "x",
        "update t set v = 'ONE' where id = 1; update t set v = 'TWO' where id = 2; update u set v = 'UNO' where id = 1",
    );
}

#[test]
fn part_then_the_rest_without_false_conflicts() {
    let db = Db::new();
    setup(&db);
    let mut s = Session::new("x");
    assert_eq!(s.run(&db, "merge world x only tables (t)", &[]).unwrap().pop().unwrap().tag, "MERGE 2");
    assert_eq!(s.branch, "x", "a partial merge leaves the world open, and the session on it");
    assert_eq!(rows(&q(&db, MAIN, "select v from t order by id")), ["ONE", "TWO"]);
    assert_eq!(rows(&q(&db, MAIN, "select v from u")), ["uno"], "u not merged yet");
    assert_eq!(diff_keys(&db, "x"), ["u/1"], "what's left to merge");

    // main now holds x's values for t: they aren't conflicts, they're merged
    q(&db, MAIN, "insert into t values (3, 'three')");
    assert_eq!(plan_rows(&q(&db, MAIN, "merge world x dry run")).len(), 1);
    assert_eq!(q(&db, MAIN, "merge world x").tag, "MERGE 1");
    assert!(db.world("x").is_err(), "merging the rest finishes it");
    assert_eq!(rows(&q(&db, MAIN, "select v from u")), ["UNO"]);

    // by key, in steps
    setup_again(&db);
    assert_eq!(q(&db, MAIN, "merge world x only keys ('t/2')").tag, "MERGE 1");
    assert_eq!(rows(&q(&db, MAIN, "select id, v from t order by id")), ["1|one", "2|TWO", "3|three"]);
    assert_eq!(q(&db, MAIN, "merge world x only keys ('t/1', 'u/1')").tag, "MERGE 2");
    assert_eq!(q(&db, MAIN, "merge world x").tag, "MERGE 0");
}

/// x again, from main as it is now.
fn setup_again(db: &Db) {
    q(db, MAIN, "update t set v = 'one' where id = 1; update t set v = 'two' where id = 2; update u set v = 'uno'");
    db.fork(MAIN, "x").unwrap();
    q(
        db,
        "x",
        "update t set v = 'ONE' where id = 1; update t set v = 'TWO' where id = 2; update u set v = 'UNO' where id = 1",
    );
}

#[test]
fn a_real_change_after_a_partial_merge_still_conflicts() {
    let db = Db::new();
    setup(&db);
    q(&db, MAIN, "merge world x only keys ('t/1')");
    // main changes t/1 after the merge; x doesn't touch it again: x's work on it is done
    q(&db, MAIN, "update t set v = 'main' where id = 1");
    assert_eq!(rows(&q(&db, MAIN, "merge world x dry run")).iter().filter(|r| r.contains("conflict")).count(), 0);
    // but if x changes it again, it computed that from its own value, which main no longer has
    q(&db, "x", "update t set v = 'x again' where id = 1");
    let e = fails(&db, MAIN, "merge world x");
    assert!(e.code == "40001" && e.message.contains("t/1") && !e.message.contains("t/2"), "{}", e.message);
    // and a row main changed that x never merged conflicts as always
    q(&db, MAIN, "update t set v = 'main 2' where id = 2");
    let e = fails(&db, MAIN, "merge world x only keys ('t/2')");
    assert!(e.code == "40001" && e.message.contains("t/2"), "{}", e.message);
    assert_eq!(q(&db, MAIN, "merge world x only keys ('t/2') using theirs").tag, "MERGE 0");
    assert_eq!(diff_keys(&db, "x"), ["t/1", "u/1"], "settled with theirs: t/2 is done");
    assert_eq!(q(&db, MAIN, "merge world x resolve ('t/1' = ours)").tag, "MERGE 2");
    assert_eq!(rows(&q(&db, MAIN, "select v from t order by id")), ["x again", "main 2"]);
}

#[test]
fn into_another_world() {
    let db = Db::new();
    setup(&db);
    db.fork(MAIN, "y").unwrap();
    q(&db, "y", "update u set v = 'y' where id = 1");
    // y changed u/1 since x forked: a conflict; t merges cleanly
    let plan = plan_rows(&q(&db, MAIN, "merge world x into y dry run"));
    assert_eq!(plan.len(), 3, "{plan:?}");
    assert!(plan[2].starts_with("u|1|conflict|both changed v"), "{plan:?}");
    let e = fails(&db, MAIN, "merge world x into y");
    assert!(e.code == "40001" && e.message.contains("u/1"), "{}", e.message);
    assert_eq!(q(&db, MAIN, "merge world x into y only tables (t)").tag, "MERGE 2");
    assert_eq!(rows(&q(&db, "y", "select v from t order by id")), ["ONE", "TWO"]);
    assert_eq!(rows(&q(&db, MAIN, "select v from t order by id")), ["one", "two"], "main untouched");
    assert_eq!(diff_keys(&db, "x"), ["t/1", "t/2", "u/1"], "x still has all of it to merge into its parent");
    assert_eq!(q(&db, MAIN, "merge world x into y using theirs").tag, "MERGE 0");
    assert_eq!(rows(&q(&db, "y", "select v from u")), ["y"]);

    // a fork of x, merged into x's sibling, and x itself merged into main afterwards
    db.fork("x", "x2").unwrap();
    q(&db, "x2", "insert into t values (9, 'nine')");
    db.fork(MAIN, "z").unwrap();
    assert_eq!(q(&db, MAIN, "merge world x2 into z").tag, "MERGE 1");
    assert_eq!(rows(&q(&db, "z", "select id from t order by id")), ["1", "2", "9"]);
    q(&db, MAIN, "merge world x2");
    assert_eq!(q(&db, MAIN, "merge world x").tag, "MERGE 4");

    // targets that make no sense
    let e = fails(&db, MAIN, "merge world y into y");
    assert!(e.code == "22023" && e.message.contains("into itself"), "{}", e.message);
    assert_eq!(fails(&db, MAIN, "merge world y into nowhere").code, "3D000");
    assert_eq!(fails(&db, MAIN, "merge world main into y").code, "0A000");
    // into its own parent is the ordinary merge: it finishes the world
    db.fork(MAIN, "w").unwrap();
    q(&db, "w", "insert into t values (10, 'ten')");
    assert_eq!(q(&db, MAIN, "merge world w into main").tag, "MERGE 1");
    assert!(db.world("w").is_err());
}

#[test]
fn only_keys_keeps_constraints_whole() {
    let db = Db::new();
    q(&db, MAIN, "create table p (id int primary key, email text unique)");
    q(&db, MAIN, "insert into p values (1, 'a@x'), (2, 'b@x')");
    db.fork(MAIN, "x").unwrap();
    q(&db, "x", "update p set email = 'c@x' where id = 1");
    q(&db, "x", "update p set email = 'a@x' where id = 2"); // takes the value row 1 gave up
    let e = fails(&db, MAIN, "merge world x only keys ('p/2')");
    assert!(e.code == "22023" && e.message.contains("add p/1 to ONLY KEYS"), "{}", e.message);
    assert_eq!(q(&db, MAIN, "merge world x only keys ('p/1', 'p/2')").tag, "MERGE 5");
    assert!(fails(&db, MAIN, "insert into p values (3, 'a@x')").code == "23505");
    q(&db, MAIN, "insert into p values (3, 'b@x')"); // free again

    // a table whose schema changed goes whole, or not at all
    db.fork(MAIN, "y").unwrap();
    q(&db, "y", "alter table p add column n int default 7");
    q(&db, "y", "update p set n = 8 where id = 1");
    let e = fails(&db, MAIN, "merge world y only keys ('p/1')");
    assert!(e.code == "22023" && e.message.contains("ONLY TABLES (p)"), "{}", e.message);
    assert_eq!(fails(&db, MAIN, "merge world y only keys ('_sonos_tables/p')").code, "22023");
    q(&db, MAIN, "merge world y only tables (p)");
    assert_eq!(rows(&q(&db, MAIN, "select id, n from p order by id")), ["1|8", "2|7", "3|7"]);
}

#[test]
fn only_tables_with_foreign_keys_and_indexes() {
    let db = Db::new();
    q(&db, MAIN, "create table customers (id int primary key, email text unique)");
    q(&db, MAIN, "create table orders (id int primary key, customer int references customers (id), status text)");
    q(&db, MAIN, "create index orders_status on orders (status)");
    q(&db, MAIN, "insert into customers values (1, 'a@x'); insert into orders values (1, 1, 'new'), (2, 1, 'new')");
    db.fork(MAIN, "x").unwrap();
    q(&db, "x", "insert into customers values (2, 'b@x')");
    q(&db, "x", "insert into orders values (3, 2, 'new'); update orders set status = 'paid' where id = 1");

    // orders alone would reference a customer main doesn't have
    let plan = rows(&q(&db, MAIN, "merge world x only tables (orders) dry run"));
    assert!(plan.iter().all(|r| r.starts_with("orders|") || r.contains("blocked")), "{plan:?}");
    assert!(plan.last().unwrap().contains("blocked"), "{plan:?}");
    assert_eq!(fails(&db, MAIN, "merge world x only tables (orders)").code, "23503");
    assert_eq!(rows(&q(&db, MAIN, "select count(*) from orders where status = 'paid'")), ["0"], "nothing merged");

    assert_eq!(q(&db, MAIN, "merge world x only tables (customers)").tag, "MERGE 2", "the row and its unique row");
    assert_eq!(fails(&db, MAIN, "insert into customers values (3, 'b@x')").code, "23505");
    q(&db, MAIN, "merge world x only tables (orders)");
    assert_eq!(rows(&q(&db, MAIN, "select id from orders where status = 'paid'")), ["1"]);
    assert_eq!(rows(&q(&db, MAIN, "select id from orders where status = 'new' order by id")), ["2", "3"]);
    assert_eq!(fails(&db, MAIN, "delete from customers where id = 2").code, "23503", "order 3 references it");
    assert_eq!(q(&db, MAIN, "merge world x").tag, "MERGE 0", "nothing left");
}

#[test]
fn replay_and_checkpoints_give_the_same_state() {
    let dir = temp("merge-select-replay");
    let state = |db: &Db| {
        let mut out = rows(&q(db, MAIN, "select id, v from t order by id"));
        out.extend(rows(&q(db, "y", "select id, v from t order by id")));
        out.extend(diff_keys(db, "x"));
        out
    };
    let want = {
        let db = Db::open(&dir).unwrap();
        setup(&db);
        db.fork(MAIN, "y").unwrap();
        q(&db, "y", "update t set v = 'y' where id = 2");
        q(&db, MAIN, "merge world x only keys ('t/1')");
        // t/1 is merged for x: INTO takes what x hasn't merged into its parent yet
        q(&db, MAIN, "merge world x into y only tables (t) resolve ('t/2' = '{\"id\": 2, \"v\": \"picked\"}')");
        state(&db)
    };
    assert_eq!(want, ["1|ONE", "2|two", "1|one", "2|picked", "t/2", "u/1"]);
    // no checkpoint: all of it comes back from the log
    let db = Db::open(&dir).unwrap();
    assert_eq!(state(&db), want);
    // x's fork point, moved on by a partial merge, is kept by a checkpoint too
    q(&db, MAIN, "merge world x only keys ('u/1')");
    db.checkpoint().unwrap();
    let want = state(&db);
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(state(&db), want);
    assert_eq!(diff_keys(&db, "x"), ["t/2"]);
    assert_eq!(q(&db, MAIN, "merge world x").tag, "MERGE 1");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn history_and_undo_of_partial_and_into_merges() {
    let dir = temp("merge-select-undo");
    let db = Db::open(&dir).unwrap();
    setup(&db);
    db.fork(MAIN, "y").unwrap();
    q(&db, MAIN, "merge world x only tables (t)");
    q(&db, MAIN, "merge world x into y only keys ('u/1')");
    let events = |w: &str| {
        rows(&q(&db, MAIN, &format!("show history for world {w} limit 2")))
            .into_iter()
            .map(|r| r.split('|').nth(2).unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(events("x"), ["merged part into y", "merged part into main"]);
    assert_eq!(events(MAIN)[0], "merged part of x");
    assert_eq!(events("y")[0], "merged part of x");

    // the latest merge of x went into y: undone there
    let u = db.undo_merge("x", false).unwrap();
    assert_eq!((u.parent.as_str(), u.rows), ("y", 1));
    assert_eq!(rows(&q(&db, "y", "select v from u")), ["uno"]);
    // undoing a partial merge puts main back; for x those rows stay merged, not reopened
    q(&db, MAIN, "merge world x only keys ('u/1')");
    assert_eq!(q(&db, MAIN, "undo merge of world x").tag, "UNDO MERGE 1");
    assert_eq!(rows(&q(&db, MAIN, "select v from u")), ["uno"]);
    assert_eq!(rows(&q(&db, MAIN, "select v from t order by id")), ["ONE", "TWO"], "the earlier merge stays");
    assert!(db.world("x").is_ok() && diff_keys(&db, "x").is_empty());
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agents_need_the_right_to_change_the_target() {
    let db = Db::new();
    setup(&db);
    let can = vec![Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain, Can::MergeOwn];
    db.create_agent("bot", &AgentSpec { can: Some(can), ..Default::default() }).unwrap();
    let me = db.as_agent("bot").unwrap();
    me.fork(MAIN, "mine", serde_json::Value::Null).unwrap();
    me.fork(MAIN, "scratch", serde_json::Value::Null).unwrap();
    drop(me);
    q(&db, "mine", "insert into t values (5, 'five')");
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    let run = |s: &mut Session, sql: &str| s.run(&db, sql, &[]).map(|mut o| o.pop().unwrap().tag);
    // x isn't its own, and y (main's other world) isn't either
    assert_eq!(run(&mut s, "merge world x into mine").unwrap_err().code, "42501");
    db.fork(MAIN, "y").unwrap();
    let e = run(&mut s, "merge world mine into y").unwrap_err();
    assert!(e.code == "42501" && e.message.contains("change y"), "{}", e.message);
    // its own world into its own world: merge_own and write_own are enough
    assert_eq!(run(&mut s, "merge world mine into scratch").unwrap(), "MERGE 1");
    assert_eq!(
        run(&mut s, "merge world mine only keys ('t/5') dry run").unwrap(),
        "SELECT 2",
        "a summary line and the row"
    );
    assert_eq!(run(&mut s, "merge world mine only keys ('t/5')").unwrap(), "MERGE 1");
    assert_eq!(rows(&q(&db, MAIN, "select v from t where id = 5")), ["five"]);
    assert!(db.world("mine").is_ok());
    // quotas count the rows a merge that leaves the world open brings
    db.alter_agent("bot", &AgentSpec { max_changes: Some(1), ..Default::default() }).unwrap();
    q(&db, "mine", "insert into t values (6, 'six'), (7, 'seven')");
    assert_eq!(run(&mut s, "merge world mine into scratch").unwrap_err().code, "42501");
}
