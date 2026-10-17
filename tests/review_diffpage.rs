//! A diff can be counted, or read a page at a time by key cursor, without building every change:
//! a 200k-row update's full diff took 8.8 s and 350 MB just to show its size and first rows.

use chronos::sql::Session;
use chronos::{Change, Db, MAIN};

fn run(db: &Db, b: &str, q: &str) {
    Session::new(b).run(db, q, &[]).unwrap();
}

/// Every page of `b`'s diff (to `to`), `limit` at a time, concatenated.
fn pages(db: &Db, b: &str, to: Option<&str>, limit: usize) -> Vec<Change> {
    let (mut all, mut after) = (vec![], None::<String>);
    loop {
        let (page, next) = db.diff_page(b, to, after.as_deref(), limit).unwrap();
        assert!(page.len() <= limit);
        all.extend(page);
        match next {
            Some(n) => after = Some(n),
            None => return all,
        }
    }
}

fn check(db: &Db, b: &str, to: Option<&str>) -> usize {
    let full = match to {
        Some(t) => db.diff_between(b, t).unwrap(),
        None => db.diff(b).unwrap(),
    };
    let tables = db.diff_count(b, to).unwrap();
    assert_eq!(tables.values().sum::<usize>(), full.len(), "count matches the diff");
    for (t, n) in &tables {
        assert_eq!(*n, full.iter().filter(|c| c.key.starts_with(&format!("{t}/"))).count(), "per-table count of {t}");
    }
    for limit in [1, 3, 7, 50, full.len() + 10] {
        assert_eq!(pages(db, b, to, limit), full, "pages of {limit} concatenate to the diff, in order");
    }
    full.len()
}

fn tables(db: &Db, n: usize) {
    run(db, MAIN, "create table leads (id int primary key, name text, score int)");
    run(db, MAIN, "create table deals (id int primary key, amount int)");
    run(db, MAIN, &format!("insert into leads select g, 'n' || g, 0 from generate_series(1, {n}) g"));
    run(db, MAIN, &format!("insert into deals select g, g from generate_series(1, {n}) g"));
}

/// Inserts, updates and deletes in two tables.
fn change(db: &Db, w: &str, n: usize) {
    run(db, w, &format!("update leads set score = 1 where id % 3 = 0 and id <= {n}"));
    run(db, w, "delete from leads where id % 7 = 1");
    run(db, w, &format!("insert into leads values ({}, 'new', 5), ({}, 'newer', 6)", n + 1, n + 2));
    run(db, w, "update deals set amount = amount * 2 where id % 5 = 0");
    run(db, w, "delete from deals where id = 2");
    run(db, w, &format!("insert into deals values ({}, 9)", n + 1));
}

#[test]
fn pages_and_counts_match_the_diff_in_memory() {
    let db = Db::new();
    tables(&db, 200);
    db.fork(MAIN, "w").unwrap();
    change(&db, "w", 200);
    let n = check(&db, "w", None);
    assert!(n > 100, "{n}");
    assert_eq!(db.diff_count("w", None).unwrap().len(), 2, "leads and deals");
    check(&db, MAIN, Some("w"));
}

/// Changes in on-disk trees (checkpointed on both sides) and in the overlays over them.
#[test]
fn pages_and_counts_match_the_diff_across_trees() {
    let dir = std::env::temp_dir().join(format!("chronos-diffpage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    tables(&db, 5000);
    db.checkpoint().unwrap();
    db.fork(MAIN, "w").unwrap();
    change(&db, "w", 5000);
    db.checkpoint().unwrap();
    run(&db, "w", "update leads set name = 'late' where id between 100 and 140");
    run(&db, "w", "delete from deals where id between 4000 and 4010");
    let n = check(&db, "w", None);
    assert!(n > 2000, "{n}");
    check(&db, MAIN, Some("w"));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_diff_and_a_limit_past_the_end() {
    let db = Db::new();
    tables(&db, 10);
    db.fork(MAIN, "w").unwrap();
    assert!(db.diff_count("w", None).unwrap().is_empty());
    assert_eq!(db.diff_page("w", None, None, 10).unwrap(), (vec![], None));
    run(&db, "w", "update leads set score = 9 where id <= 3");
    let (page, next) = db.diff_page("w", None, None, 1000).unwrap();
    assert_eq!((page.len(), next), (3, None), "one page, no cursor");
    assert_eq!(page, db.diff("w").unwrap());
    // a cursor past every change: an empty last page
    let (page, next) = db.diff_page("w", None, Some("zzz"), 5).unwrap();
    assert_eq!((page.len(), next), (0, None));
}

#[test]
fn sql_diff_limit() {
    let db = Db::new();
    tables(&db, 50);
    db.fork(MAIN, "w").unwrap();
    run(&db, "w", "update leads set score = 2");
    let out = Session::new(MAIN).run(&db, "diff world w limit 7", &[]).unwrap().pop().unwrap();
    assert_eq!(out.rows.len(), 7);
    let all = Session::new(MAIN).run(&db, "diff world w", &[]).unwrap().pop().unwrap();
    assert_eq!(all.rows.len(), 50);
    assert_eq!(out.rows[..], all.rows[..7], "the first rows of the full diff");
}
