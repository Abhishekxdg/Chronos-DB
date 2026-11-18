//! UNDO DIFF puts back what turned one world (or moment) into another, in the session's world, in one statement: a
//! revert done by the server rather than by running `DIFF ... AS SQL`'s statements one by one.

use chronos::sql::{Output, Value};
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

/// main with three rows; `before`, then a change (an update, an insert, a delete, a log row), then `after`.
fn setup(db: &Db) {
    q(db, MAIN, "create table t (id int primary key, v text unique)");
    q(db, MAIN, "create table log (n int primary key)");
    q(db, MAIN, "insert into t values (1, 'a'), (2, 'b'), (3, 'c')");
    db.fork(MAIN, "before").unwrap();
    q(db, MAIN, "update t set v = 'B' where id = 2");
    q(db, MAIN, "insert into t values (4, 'd')");
    q(db, MAIN, "delete from t where id = 3");
    q(db, MAIN, "insert into log values (1)");
    db.fork(MAIN, "after").unwrap();
}

#[test]
fn undoes_a_diff_and_keeps_later_changes() {
    let db = Db::new();
    setup(&db);
    q(&db, MAIN, "insert into t values (5, 'e')"); // after the change: stays
    let o = q(&db, MAIN, "undo diff world before to after except tables (log)");
    assert_eq!(o.tag, "UNDO DIFF 3");
    assert_eq!(rows(&q(&db, MAIN, "select * from t order by id")), ["1|a", "2|b", "3|c", "5|e"]);
    assert_eq!(rows(&q(&db, MAIN, "select * from log")), ["1"], "EXCEPT TABLES leaves the log");
    // the unique index follows the rows put back: 'c' is taken again, 'd' free
    assert!(chronos::sql::run(&db, MAIN, "insert into t values (6, 'c')", &[]).is_err());
    q(&db, MAIN, "insert into t values (6, 'd')");
}

#[test]
fn a_row_changed_since_is_refused_or_skipped() {
    let db = Db::new();
    setup(&db);
    q(&db, MAIN, "update t set v = 'BB' where id = 2");
    let e = chronos::sql::run(&db, MAIN, "undo diff before to after", &[]).unwrap_err();
    assert_eq!(e.code, "40001");
    assert!(e.message.contains("t/2") && e.message.contains("SKIP CHANGED"), "{}", e.message);
    assert_eq!(rows(&q(&db, MAIN, "select count(*) from t")), ["3"], "nothing undone");
    assert_eq!(q(&db, MAIN, "undo diff before to after skip changed").tag, "UNDO DIFF 3"); // 3, 4 and log's row
    assert_eq!(rows(&q(&db, MAIN, "select * from t order by id")), ["1|a", "2|BB", "3|c"]);
    assert_eq!(rows(&q(&db, MAIN, "select count(*) from log")), ["0"]);
}

#[test]
fn into_another_world() {
    let db = Db::new();
    setup(&db);
    db.fork(MAIN, "w").unwrap();
    assert_eq!(q(&db, "w", "undo diff before to after except tables (log)").tag, "UNDO DIFF 3");
    assert_eq!(rows(&q(&db, "w", "select * from t order by id")), ["1|a", "2|b", "3|c"]);
    assert_eq!(rows(&q(&db, MAIN, "select count(*) from t")), ["3"], "main as it was: 1, 2, 4");
    assert_eq!(rows(&q(&db, "w", "diff world w to after")).len(), 3, "w differs from after in those three rows");
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-undo-merge-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A parent row a merge brought, and a child main added after that references it.
fn merged_parent(db: &Db) {
    q(db, MAIN, "create table p (id int primary key)");
    q(db, MAIN, "create table c (id int primary key, pid int references p (id))");
    db.fork(MAIN, "w").unwrap();
    q(db, "w", "insert into p values (1), (2)");
    q(db, MAIN, "merge world w");
    q(db, MAIN, "insert into c values (10, 1)");
}

/// H4: UNDO MERGE is checked as a merge is: undoing it would delete a parent a later child references, so it is
/// refused and nothing changes.
#[test]
fn undo_merge_breaking_reference_refused() {
    let dir = tmp("refused");
    let db = Db::open(&dir).unwrap();
    merged_parent(&db);
    let e = chronos::sql::run(&db, MAIN, "undo merge of world w", &[]).unwrap_err();
    assert_eq!(e.code, "23503", "{e}");
    assert_eq!(rows(&q(&db, MAIN, "select id from p order by id")), ["1", "2"], "nothing undone");
    assert_eq!(rows(&q(&db, MAIN, "select id, pid from c")), ["10|1"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// With SKIP CHANGED, the referenced parent is left as one changed since, and the rest is undone.
#[test]
fn undo_merge_skip_changed_keeps_parent() {
    let dir = tmp("skip");
    let db = Db::open(&dir).unwrap();
    merged_parent(&db);
    q(&db, MAIN, "undo merge of world w skip changed");
    assert_eq!(rows(&q(&db, MAIN, "select id from p order by id")), ["1"], "the parent stays, 2 goes");
    assert_eq!(rows(&q(&db, MAIN, "select id, pid from c")), ["10|1"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
