//! Persistent autonomous transactions: durable worlds (each acknowledged write is on disk, and a
//! crash doesn't flag them), durable read sets (the stale-read check survives a crash), pinned
//! checkpoints (a label that outlives history's retention), and RESUME / SHOW WORLD STATE.
//!
//! Crash model as in tests/durability.rs: copying the folder while the database is open is a
//! `kill -9` at that moment; what was written to the files is kept, what waited in memory (an open
//! world's batch, the log's buffer) is gone.

use chronos::sql::{Output, run};
use chronos::{Db, Resolve};
use std::fs;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-durable-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn copy_dir(from: &Path, to: &Path) {
    let _ = fs::remove_dir_all(to);
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dst = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dst);
        } else {
            fs::copy(&p, dst).unwrap();
        }
    }
}

fn q(db: &Db, world: &str, sql: &str) -> Vec<Output> {
    run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn text(db: &Db, world: &str, sql: &str) -> Vec<String> {
    q(db, world, sql).pop().unwrap().rows.iter().map(|r| r[0].to_text()).collect()
}

/// The one row SHOW WORLD STATE / RESUME gives, by column name.
fn state(db: &Db, sql: &str) -> std::collections::HashMap<String, String> {
    let o = q(db, "main", sql).pop().unwrap();
    assert_eq!(o.rows.len(), 1, "{sql}");
    o.columns.as_ref().unwrap().iter().map(|c| c.0.clone()).zip(o.rows[0].iter().map(|v| v.to_text())).collect()
}

fn setup(dir: &Path) -> Db {
    let db = Db::open(dir).unwrap();
    q(&db, "main", "create table t (id int primary key, v text); insert into t values (1, 'one'), (2, 'two')");
    db
}

#[test]
fn durable_world_write_survives_kill() {
    let (dir, crashed) = (tmp("survives"), tmp("survives-copy"));
    let db = setup(&dir);
    q(&db, "main", "create world w with (durable)");
    q(&db, "main", "create world plain");
    q(&db, "w", "insert into t values (3, 'three')");
    q(&db, "plain", "insert into t values (3, 'three')");
    copy_dir(&dir, &crashed); // kill -9 now
    drop(db);
    let db = Db::open(&crashed).unwrap();
    assert_eq!(text(&db, "w", "select v from t where id = 3"), ["three"], "acknowledged, so on disk");
    assert!(text(&db, "plain", "select v from t where id = 3").is_empty(), "a plain world's write waited in memory");
    assert!(db.durable("w").unwrap() && !db.durable("plain").unwrap());
    drop(db);
    for d in [dir, crashed] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn a_world_made_plain_again_is_flagged_after_a_crash() {
    let (dir, crashed) = (tmp("unset"), tmp("unset-copy"));
    let db = setup(&dir);
    q(&db, "main", "create world w with (durable)");
    q(&db, "main", "alter world w set (durable = false)");
    q(&db, "w", "insert into t values (3, 'lost')"); // acknowledged, and waits in memory
    copy_dir(&dir, &crashed);
    drop(db);
    let db = Db::open(&crashed).unwrap();
    assert!(!db.durable("w").unwrap(), "it was made plain before the write");
    assert!(db.world("w").unwrap().flagged, "a write it acknowledged may be lost: its merge asks first");
    drop(db);
    for d in [dir, crashed] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn durable_world_not_flagged_after_crash() {
    let (dir, crashed) = (tmp("unflagged"), tmp("unflagged-copy"));
    let db = setup(&dir);
    q(&db, "main", "create world w");
    q(&db, "w", "insert into t values (3, 'before')"); // waits in w's batch
    q(&db, "main", "alter world w set (durable = true)"); // ... and is on disk once w is durable
    q(&db, "w", "update t set v = 'after' where id = 1");
    copy_dir(&dir, &crashed);
    drop(db);
    let db = Db::open(&crashed).unwrap();
    assert!(!db.world("w").unwrap().flagged, "every acknowledged write of a durable world is on disk");
    assert_eq!(text(&db, "w", "select v from t order by id"), ["after", "two", "before"]);
    db.merge("w", Resolve::Fail).unwrap(); // no confirmation needed
    assert_eq!(text(&db, "main", "select v from t order by id"), ["after", "two", "before"]);
    // a clean reopen keeps it so
    drop(db);
    let db = Db::open(&crashed).unwrap();
    assert_eq!(text(&db, "main", "select count(*) from t"), ["3"]);
    drop(db);
    for d in [dir, crashed] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn non_durable_world_still_flagged() {
    let (dir, crashed) = (tmp("flagged"), tmp("flagged-copy"));
    let db = setup(&dir);
    q(&db, "main", "create world w with (durable)");
    q(&db, "main", "create world v");
    q(&db, "v", "insert into t values (3, 'three')");
    q(&db, "main", "alter world w set (durable = false)"); // and back: an ordinary world again
    q(&db, "w", "insert into t values (4, 'four')");
    q(&db, "main", "insert into t values (9, 'sync')"); // syncs the log: the worlds exist on disk
    copy_dir(&dir, &crashed);
    drop(db);
    let db = Db::open(&crashed).unwrap();
    assert!(db.world("v").unwrap().flagged, "a plain world open in a crash may have lost writes");
    assert!(db.world("w").unwrap().flagged, "no longer durable: flagged like any other");
    assert!(matches!(db.merge("v", Resolve::Fail), Err(chronos::Error::Stale { .. })));
    drop(db);
    for d in [dir, crashed] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn durable_read_set_survives_crash_and_merge_checks_normally() {
    let (dir, crashed) = (tmp("reads"), tmp("reads-copy"));
    let db = setup(&dir);
    q(&db, "main", "create table u (id int primary key)");
    q(&db, "main", "create world calm with (durable, check_reads = true)");
    q(&db, "main", "create world moved with (durable, check_reads = true)");
    q(&db, "main", "create world old with (check_reads = true)");
    for (i, w) in ["calm", "moved", "old"].iter().enumerate() {
        q(&db, w, "select v from t where id = 1");
        q(&db, w, &format!("insert into u values ({i})"));
    }
    // read after the world's last write: kept too
    q(&db, "moved", "select v from t where id = 2");
    copy_dir(&dir, &crashed);
    drop(db);
    let db = Db::open(&crashed).unwrap();
    q(&db, "main", "update t set v = 'changed' where id = 2");
    // what it read didn't change: merges, rather than being held for reads lost in the crash
    db.merge("calm", Resolve::Fail).unwrap();
    // what it read did change: held for that row, as without a crash
    let e = db.merge("moved", Resolve::Fail).unwrap_err().to_string();
    assert!(e.contains("read rows that changed") && !e.contains("restarted"), "{e}");
    // a world that isn't durable: its reads are unknown after the crash, its merge held for that
    let e = db.merge("old", Resolve::Fail).unwrap_err().to_string();
    assert!(e.contains("restarted") || e.contains("crash"), "{e}");
    drop(db);
    for d in [dir, crashed] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn durable_read_set_survives_checkpoint_then_crash() {
    let (dir, crashed) = (tmp("reads-ckpt"), tmp("reads-ckpt-copy"));
    let db = setup(&dir);
    q(&db, "main", "create table u (id int primary key)");
    q(&db, "main", "create world w with (durable, check_reads = true)");
    q(&db, "w", "select v from t where id = 1");
    q(&db, "w", "insert into u values (1)");
    db.checkpoint().unwrap(); // the segment holding its read set is left behind
    copy_dir(&dir, &crashed);
    drop(db);
    let db = Db::open(&crashed).unwrap();
    q(&db, "main", "update t set v = 'changed' where id = 1");
    let e = db.merge("w", Resolve::Fail).unwrap_err().to_string();
    assert!(e.contains("read rows that changed") && !e.contains("restarted"), "{e}");
    drop(db);
    for d in [dir, crashed] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn pinned_checkpoint_outlives_retention() {
    let dir = tmp("pinned");
    {
        let db = setup(&dir);
        q(&db, "main", "create world w");
        q(&db, "w", "update t set v = 'kept' where id = 1");
        q(&db, "main", "checkpoint world w as kept pin '1 day'");
        q(&db, "main", "checkpoint world w as plain");
        q(&db, "w", "update t set v = 'later' where id = 1");
        q(&db, "w", "delete from t where id = 2");
    }
    let db = Db::open(&dir).unwrap(); // the pin is on disk
    db.set_retention(0).unwrap();
    db.checkpoint().unwrap(); // history before now is gone
    db.gc().unwrap(); // and its pages
    let e = run(&db, "main", "restore world w to checkpoint plain", &[]).unwrap_err();
    assert!(e.message.contains("history starts at"), "an unpinned label goes with history: {}", e.message);
    q(&db, "main", "restore world w to checkpoint kept");
    assert_eq!(text(&db, "w", "select v from t order by id"), ["kept", "two"]);
    drop(db);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn rollback_to_pinned_checkpoint() {
    let dir = tmp("rollback");
    let db = setup(&dir);
    q(&db, "main", "create world w with (durable)");
    q(&db, "w", "insert into t values (3, 'three')");
    q(&db, "main", "checkpoint world w as step1 pin '1 hour'");
    q(&db, "w", "update t set v = 'x' where id = 1; delete from t where id = 3; insert into t values (4, 'four')");
    let tag = q(&db, "main", "rollback world w to checkpoint step1").pop().unwrap().tag;
    assert!(tag.starts_with("ROLLBACK WORLD "), "{tag}");
    assert_eq!(text(&db, "w", "select v from t order by id"), ["one", "two", "three"]);
    assert!(run(&db, "main", "rollback world w to checkpoint nope", &[]).is_err());
    // relabelled: the newest state under the name
    q(&db, "w", "insert into t values (5, 'five')");
    q(&db, "main", "checkpoint world w as step1 pin '1 hour'");
    q(&db, "w", "delete from t");
    q(&db, "main", "rollback world w to checkpoint step1");
    assert_eq!(text(&db, "w", "select count(*) from t"), ["4"]);
    // a pin that has run out is a plain label again: history decides
    q(&db, "main", "checkpoint world w as brief pin '1 millisecond'");
    std::thread::sleep(std::time::Duration::from_millis(5));
    db.gc().unwrap();
    q(&db, "main", "rollback world w to checkpoint brief"); // still in history's window
    drop(db);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn resume_token_validates_version() {
    let dir = tmp("resume");
    let db = setup(&dir);
    q(&db, "main", "create world w with (durable)");
    q(&db, "w", "insert into t values (3, 'three')");
    let s = state(&db, "show world state w");
    assert_eq!((s["version"].as_str(), s["epoch"].as_str(), s["durable"].as_str()), ("1", "0", "true"));
    let token = s["token"].clone();
    assert_eq!(token, format!("{}:1:0", db.world("w").unwrap().id));
    let r = state(&db, &format!("resume world w token '{token}'"));
    assert_eq!(r["token"], token, "nothing happened since: the same token");
    q(&db, "w", "insert into t values (4, 'four')");
    let e = run(&db, "main", &format!("resume world w token '{token}'"), &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    let id = db.world("w").unwrap().id;
    let e = run(&db, "main", &format!("resume world w token '{id}:2:1'"), &[]).unwrap_err();
    assert_eq!(e.code, "55P03", "another epoch: the lease was handed on: {}", e.message);
    let e = run(&db, "main", "resume world w token 'wnope:2:0'", &[]).unwrap_err();
    assert_eq!(e.code, "22023", "{}", e.message);
    let e = run(&db, "main", "resume world w token 'garbage'", &[]).unwrap_err();
    assert_eq!(e.code, "22023", "{}", e.message);
    state(&db, &format!("resume world w token '{id}:2:0'"));
    state(&db, "resume world w"); // no token: nothing to check
    drop(db);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn resume_reports_stale_reads() {
    let dir = tmp("resume-stale");
    let db = setup(&dir);
    q(&db, "main", "create world w with (durable, check_reads = true)");
    q(&db, "w", "select v from t where id = 1");
    let s = state(&db, "show world state w");
    assert_eq!((s["stale_reads"].as_str(), s["parent_changes"].as_str()), ("", "0"));
    q(&db, "main", "update t set v = 'uno' where id = 1; insert into t values (7, 'seven')");
    let s = state(&db, "resume world w");
    assert!(s["stale_reads"].contains("t(1)") || s["stale_reads"].contains("changed"), "{s:?}");
    assert_eq!(s["parent_changes"], "2");
    assert_eq!(s["check_reads"], "true");
    drop(db);
    let _ = fs::remove_dir_all(dir);
}

/// A label taken again without PIN names the newer moment: the pin it had kept the older one, and goes with it, so
/// ROLLBACK gets back to the newer state, before and after a restart.
#[test]
fn checkpoint_without_pin_replaces_a_pinned_label() {
    let dir = tmp("relabel");
    let db = setup(&dir);
    q(&db, "main", "create world w with (durable)");
    q(&db, "w", "update t set v = '1' where id = 1");
    q(&db, "main", "checkpoint world w as step pin '1 hour'");
    q(&db, "w", "update t set v = '2' where id = 1");
    q(&db, "main", "checkpoint world w as step");
    q(&db, "w", "update t set v = '3' where id = 1");
    q(&db, "main", "rollback world w to checkpoint step");
    assert_eq!(text(&db, "w", "select v from t where id = 1"), ["2"]);
    drop(db);
    let db = Db::open(&dir).unwrap();
    q(&db, "w", "update t set v = '4' where id = 1");
    q(&db, "main", "rollback world w to checkpoint step");
    assert_eq!(text(&db, "w", "select v from t where id = 1"), ["2"]);
    drop(db);
    let _ = fs::remove_dir_all(dir);
}

/// A resume token is refused (40001) once the world changed by any way in, not only its own writes: a child merged
/// into it, a transaction committed into it. A transaction that only read changes nothing. The token stays good across
/// a restart.
#[test]
fn resume_token_sees_merges_and_commits_into_the_world() {
    let dir = tmp("resume-merges");
    let db = setup(&dir);
    q(&db, "main", "create world w with (durable)");
    q(&db, "w", "insert into t values (3, 'three')");
    let token = state(&db, "show world state w")["token"].clone();
    q(&db, "main", "create world c from w");
    q(&db, "c", "insert into t values (4, 'four')");
    q(&db, "main", "merge world c");
    let e = run(&db, "main", &format!("resume world w token '{token}'"), &[]).unwrap_err();
    assert_eq!(e.code, "40001", "a merge into it moved it: {}", e.message);
    let token = state(&db, "show world state w")["token"].clone();
    state(&db, &format!("resume world w token '{token}'"));
    let mut s = chronos::sql::Session::new("w");
    s.run(&db, "begin; insert into t values (5, 'five'); commit", &[]).unwrap();
    let e = run(&db, "main", &format!("resume world w token '{token}'"), &[]).unwrap_err();
    assert_eq!(e.code, "40001", "a commit into it moved it: {}", e.message);
    let token = state(&db, "show world state w")["token"].clone();
    s.run(&db, "begin; select count(*) from t; commit", &[]).unwrap();
    state(&db, &format!("resume world w token '{token}'")); // only read: the same
    s.close(&db);
    drop(db);
    let db = Db::open(&dir).unwrap();
    state(&db, &format!("resume world w token '{token}'")); // a restart moves nothing
    drop(db);
    let _ = fs::remove_dir_all(dir);
}
