//! The merge engine: a dry run shows what a merge would do and why rows conflict, changing
//! nothing; rows are settled one by one or combined by columns; a restart replays a settled
//! merge exactly; and two sides changing one table's schema can't be settled by picking a side.

use chronos::sql::{Output, Session, Value};
use chronos::{Db, MAIN};

fn q(db: &Db, world: &str, sql: &str) -> Output {
    chronos::sql::run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap()
}

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

/// main and world x, each changing the same four rows in their own way.
fn setup(db: &Db) {
    q(db, MAIN, "create table t (id text primary key, name text, email text, age int, at timestamptz default now())");
    q(
        db,
        MAIN,
        "insert into t (id, name, email, age) values ('a', 'Ada', 'a@x', 36), ('b', 'Bo', 'b@x', 20), ('c', 'Cy', 'c@x', 30), ('d', 'Di', 'd@x', 40)",
    );
    db.fork(MAIN, "x").unwrap();
    q(db, "x", "update t set name = 'Ada L.' where id = 'a'"); // main changes a's email: different columns
    q(db, "x", "update t set age = 21 where id = 'b'"); //        main changes b's age too
    q(db, "x", "update t set age = 31 where id = 'c'"); //        only here
    q(db, "x", "delete from t where id = 'd'"); //                main changes d
    q(db, MAIN, "update t set email = 'ada@y' where id = 'a'");
    q(db, MAIN, "update t set age = 22 where id = 'b'");
    q(db, MAIN, "update t set name = 'Diana' where id = 'd'");
}

#[test]
fn dry_run_explains_and_changes_nothing() {
    let db = Db::new();
    setup(&db);
    let before = rows(&q(&db, MAIN, "select id, name, email, age from t order by id"));
    let plan = q(&db, MAIN, "merge world x dry run");
    let cols: Vec<String> = plan.columns.clone().unwrap().into_iter().map(|c| c.0).collect();
    assert_eq!(cols, ["table", "id", "outcome", "detail", "base", "ours", "theirs", "result", "postgres"]);
    let got: Vec<String> =
        plan.rows.iter().map(|r| format!("{}: {} ({})", r[1].to_text(), r[2].to_text(), r[3].to_text())).collect();
    assert_eq!(
        got,
        [
            "a: conflict (here: name; the parent: email (different columns: merge by columns combines them))",
            "b: conflict (both changed age)",
            "c: apply ()",
            "d: conflict (deleted here, changed by the parent)",
        ]
    );
    assert_eq!(rows(&q(&db, MAIN, "select id, name, email, age from t order by id")), before, "nothing changed");
    assert!(db.world("x").is_ok(), "x is still there");

    // a merge that fails says why, row by row
    let e = chronos::sql::run(&db, MAIN, "merge world x", &[]).unwrap_err();
    assert_eq!(e.code, "40001");
    assert!(e.message.contains("t/b (both changed age)") && e.message.contains("RESOLVE"), "{}", e.message);

    // by columns, a is combined; b and d still need a choice
    let plan = q(&db, MAIN, "merge world x by columns dry run");
    let a = &plan.rows[0];
    assert_eq!(a[2].to_text(), "by columns");
    let result = a[7].to_text();
    assert!(result.contains("\"name\": \"Ada L.\"") && result.contains("\"email\": \"ada@y\""), "{result}");
    assert_eq!(plan.rows.iter().filter(|r| r[2].to_text() == "conflict").count(), 2);
}

#[test]
fn settle_row_by_row_and_by_columns() {
    let db = Db::new();
    setup(&db);
    let tag = q(&db, MAIN, "merge world x by columns resolve ('t/b' = theirs, 't/d' = '{\"id\": \"d\", \"name\": \"Dee\", \"email\": \"d@z\", \"age\": 41}')").tag;
    assert_eq!(tag, "MERGE 3", "a by columns, c, and d as given (b stays main's)");
    assert_eq!(
        rows(&q(&db, MAIN, "select id, name, email, age from t order by id")),
        ["a|Ada L.|ada@y|36", "b|Bo|b@x|22", "c|Cy|c@x|31", "d|Dee|d@z|41"]
    );
    assert!(db.world("x").is_err(), "merged");
    // a row given with a column left out gets it filled in as by an INSERT
    assert!(q(&db, MAIN, "select at from t where id = 'd'").rows[0][0] != Value::Null);
}

/// A table set to merge by columns: in any merge into a world that has the setting, a row whose
/// sides changed different columns combines; the same column still conflicts. Off (the default),
/// the conflict says the rows would combine and names the setting. The setting is a change to
/// the table, in DIFF and DIFF AS SQL.
#[test]
fn a_table_that_merges_by_columns() {
    let db = Db::new();
    let fails = |sql: &str| chronos::sql::run(&db, MAIN, sql, &[]).err().unwrap_or_else(|| panic!("{sql} should fail"));
    q(&db, MAIN, "create table p (id int primary key, name text, email text, age int)");
    q(&db, MAIN, "insert into p values (1, 'Ada', 'a@x', 36), (2, 'Bo', 'b@x', 20)");
    db.fork(MAIN, "w").unwrap();
    q(&db, "w", "update p set name = 'Ada L.' where id = 1");
    q(&db, MAIN, "update p set email = 'ada@y' where id = 1");
    let e = fails("merge world w");
    assert_eq!(e.code, "40001");
    assert!(
        e.message.contains("The two sides changed different columns of each")
            && e.message.contains("ALTER TABLE p SET (merge_by_columns = true)"),
        "{}",
        e.message
    );

    // on, in the world merged into: the row combines, no BY COLUMNS needed
    q(&db, MAIN, "alter table p set (merge_by_columns = true)");
    assert_eq!(q(&db, MAIN, "merge world w").tag, "MERGE 1");
    assert_eq!(rows(&q(&db, MAIN, "select name, email from p where id = 1")), ["Ada L.|ada@y"]);

    // the same column still conflicts, and the error doesn't offer combining
    db.fork(MAIN, "v").unwrap();
    q(&db, "v", "update p set age = 21 where id = 2");
    q(&db, MAIN, "update p set age = 22 where id = 2");
    let e = fails("merge world v");
    assert!(e.code == "40001" && !e.message.contains("different columns of each"), "{}", e.message);

    // the setting is a change to the table
    db.fork(MAIN, "s").unwrap();
    q(&db, "s", "alter table p reset (merge_by_columns)");
    assert!(rows(&q(&db, MAIN, "diff world s")).iter().any(|r| r.contains("merge: whole rows")));
    let sql = rows(&q(&db, MAIN, "diff world s as sql"));
    assert!(sql.iter().any(|r| r.contains("alter table p set (merge_by_columns = false);")), "{sql:?}");
    assert_eq!(fails("alter table p set (no_such_setting = 70)").code, "0A000"); // (fillfactor and the other storage parameters are accepted: they change nothing here)
}

#[test]
fn settled_merges_replay_exactly() {
    let dir = std::env::temp_dir().join(format!("chronos-merge-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let want = {
        let db = Db::open(&dir).unwrap();
        setup(&db);
        // d is given without `at`: its default is now(), which replay must not compute again
        q(&db, MAIN, "merge world x by columns resolve ('t/b' = ours, 't/d' = '{\"id\": \"d\", \"name\": \"Dee\"}')");
        std::thread::sleep(std::time::Duration::from_millis(20));
        rows(&q(&db, MAIN, "select * from t order by id"))
    };
    let db = Db::open(&dir).unwrap(); // no checkpoint: the merge comes back from the log
    assert_eq!(rows(&q(&db, MAIN, "select * from t order by id")), want);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn both_changing_a_table_cannot_be_settled_by_picking_a_side() {
    let db = Db::new();
    q(&db, MAIN, "create table t (id int primary key, a text)");
    q(&db, MAIN, "insert into t values (1, 'one')");
    db.fork(MAIN, "x").unwrap();
    q(&db, "x", "alter table t add column b int default 1");
    q(&db, "x", "insert into t values (2, 'two', 2)");
    q(&db, MAIN, "alter table t add column c text default 'c'");
    q(&db, MAIN, "insert into t values (3, 'three', 'three')");
    for how in ["", " using ours", " using theirs", " by columns"] {
        let e = chronos::sql::run(&db, MAIN, &format!("merge world x{how}"), &[]).unwrap_err();
        assert!(e.code == "40001" && e.message.contains("both sides changed table t itself"), "{how}: {}", e.message);
    }
    let plan = q(&db, MAIN, "merge world x dry run");
    assert!(
        rows(&plan).iter().any(|r| r.starts_with("t|null|conflict|both sides changed this table itself")),
        "{:?}",
        rows(&plan)
    );
    // the same change on both sides is no conflict
    let mut s = Session::new(MAIN);
    s.run(&db, "drop world x; create world y", &[]).unwrap();
    q(&db, "y", "create index t_a on t (a)");
    q(&db, MAIN, "create index t_a on t (a)");
    assert_eq!(q(&db, MAIN, "merge world y").tag, "MERGE 0");
}

/// A merge can be undone from history: every row it changed goes back, as an ordinary write;
/// rows changed again since are named (or left alone, with SKIP CHANGED).
#[test]
fn undo_a_merge() {
    let dir = std::env::temp_dir().join(format!("chronos-undo-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    q(&db, MAIN, "create table t (id int primary key, v text, code text unique)");
    q(&db, MAIN, "insert into t values (1, 'one', 'c1'), (2, 'two', 'c2')");
    db.fork(MAIN, "x").unwrap();
    q(
        &db,
        "x",
        "update t set v = 'ONE' where id = 1; update t set v = 'TWO' where id = 2; insert into t values (3, 'three', 'c3')",
    );
    q(&db, MAIN, "merge world x");
    q(&db, MAIN, "update t set v = 'two again' where id = 2"); // after the merge

    let e = chronos::sql::run(&db, MAIN, "undo merge of world x", &[]).unwrap_err();
    assert!(e.code == "40001" && e.message.contains("t/2") && e.message.contains("SKIP CHANGED"), "{}", e.message);
    assert_eq!(
        rows(&q(&db, MAIN, "select id, v from t order by id")),
        ["1|ONE", "2|two again", "3|three"],
        "nothing undone"
    );

    assert_eq!(q(&db, MAIN, "undo merge of world x skip changed").tag, "UNDO MERGE 2");
    assert_eq!(rows(&q(&db, MAIN, "select id, v from t order by id")), ["1|one", "2|two again"]);
    // the unique value row 3 held is free again: constraint rows went back with the rows
    q(&db, MAIN, "insert into t values (4, 'four', 'c3')");
    assert!(chronos::sql::run(&db, MAIN, "insert into t values (5, 'five', 'c1')", &[]).is_err(), "c1 is still taken");
    let e = chronos::sql::run(&db, MAIN, "undo merge of world nobody", &[]).unwrap_err();
    assert!(e.message.contains("no merge of nobody"), "{}", e.message);
    drop(db);
    let mem = Db::new();
    assert!(mem.undo_merge("x", false).unwrap_err().to_string().contains("in-memory"));
    let _ = std::fs::remove_dir_all(&dir);
}
