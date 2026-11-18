//! The merge ledger: a row in main for every merge into it, written in the merge's own log record (so it survives
//! a restart, and a crash leaves both or neither), chained by hash (VERIFY LEDGER finds a row changed), and
//! operation ids that make a repeated merge request return what the first did.

use chronos::{Db, MAIN, MergeOptions, Resolve};
use std::path::PathBuf;

fn q(db: &Db, world: &str, sql: &str) -> Vec<String> {
    let out = chronos::sql::run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    out.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")).collect()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-ledger-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Forks `w` from main, writes `rows` there and merges it whole.
fn merge_one(db: &Db, w: &str, rows: &[(&str, &str)]) {
    db.fork(MAIN, w).unwrap();
    for (k, v) in rows {
        db.put(w, k, v).unwrap();
    }
    db.merge(w, Resolve::Fail).unwrap();
}

#[test]
fn ledger_row_survives_restart() {
    let dir = tmp("restart");
    let before = {
        let db = Db::open(&dir).unwrap();
        merge_one(&db, "a", &[("k1", "1"), ("k2", "2")]);
        db.fork(MAIN, "p").unwrap();
        db.put("p", "t/3", "3").unwrap();
        db.put("p", "t/4", "4").unwrap();
        db.merge_with("p", &MergeOptions { only_keys: vec!["t/3".into()], ..Default::default() }).unwrap();
        let rows = db.ledger_rows().unwrap();
        assert_eq!(rows.len(), 2, "one row per merge into main, partial merges too: {rows:?}");
        assert_eq!(rows[0]["source"], "a");
        assert_eq!(rows[0]["rows"], 2);
        assert_eq!(rows[0]["seq"], 1);
        assert_eq!(rows[1]["source"], "p");
        assert_eq!(rows[1]["prev_hash"], rows[0]["hash"], "chained");
        rows
    };
    // no checkpoint since the merges: the rows come back from the log, in the merges' own records
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.ledger_rows().unwrap(), before);
    // and the chain goes on from there after the restart
    merge_one(&db, "b", &[("k5", "5")]);
    let rows = db.ledger_rows().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2]["seq"], 3);
    assert_eq!(rows[2]["prev_hash"], rows[1]["hash"]);
    assert!(q(&db, MAIN, "verify ledger").iter().all(|r| r.contains("|true|")), "{:?}", q(&db, MAIN, "verify ledger"));
    // a merge between other worlds writes none; nor does a rebase
    db.fork(MAIN, "x").unwrap();
    db.fork("x", "y").unwrap();
    db.put("y", "z", "1").unwrap();
    db.merge("y", Resolve::Fail).unwrap();
    db.put(MAIN, "k6", "6").unwrap();
    db.rebase("x", &MergeOptions::default()).unwrap();
    assert_eq!(db.ledger_rows().unwrap().len(), 3);
    // and the world's diff never carries main's ledger rows into a later merge
    assert!(db.diff("x").unwrap().iter().all(|c| !c.key.starts_with("_sonos_ledger/")));
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ledger_chain_verifies_and_detects_tamper() {
    let (dir, copy) = (tmp("tamper"), tmp("tamper-copy"));
    let db = Db::open(&dir).unwrap();
    for (i, w) in ["a", "b", "c"].iter().enumerate() {
        merge_one(&db, w, &[(&format!("k{i}"), "v")]);
    }
    assert_eq!(q(&db, MAIN, "verify ledger"), vec!["1|true|", "2|true|", "3|true|"]);
    // nobody writes the ledger but a merge: not SQL, not a write straight into main
    let key = "_sonos_ledger/00000000000000000002";
    assert!(db.put(MAIN, key, "{}").is_err());
    assert!(db.del(MAIN, key).is_err());
    // the folder as a crash would leave it (the merges are in the log, not yet in a checkpoint)
    copy_dir(&dir, &copy);
    drop(db);
    // the second merge's ledger row changed on disk, its frame's checksum made good again: recovery writes it
    // back as logged, and VERIFY LEDGER finds that row no longer hashes to its hash (the rows after it still link)
    let mut done = false;
    for seg in std::fs::read_dir(&copy).unwrap().map(|e| e.unwrap().path()) {
        if !seg.file_name().unwrap().to_str().unwrap().starts_with("wal-") {
            continue;
        }
        let mut b = std::fs::read(&seg).unwrap();
        let (mut off, mut applied) = (0, 0);
        while off + 8 <= b.len() {
            let len = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
            if len == 0 {
                break;
            }
            let p = off + 8;
            if b[p] == 20 {
                applied += 1;
                if applied == 2 {
                    let at = b[p..p + len].windows(9).position(|w| w == b"\"rows\":1,").expect("rows in the row") + p;
                    b[at + 7] = b'9';
                    let crc = crc32fast::hash(&b[p..p + len]);
                    b[off + 4..off + 8].copy_from_slice(&crc.to_le_bytes());
                    done = true;
                }
            }
            off = p + len;
        }
        std::fs::write(&seg, b).unwrap();
    }
    assert!(done, "the second merge's record was found");
    let db = Db::open(&copy).unwrap();
    assert_eq!(db.ledger_rows().unwrap()[1]["rows"], 9);
    let got = q(&db, MAIN, "verify ledger");
    assert_eq!(got[0], "1|true|", "{got:?}");
    assert!(got[1].starts_with("2|false|") && got[1].contains("doesn't match its contents"), "{got:?}");
    assert_eq!(got[2], "3|true|", "{got:?}");
    drop(db);
    for d in [dir, copy] {
        std::fs::remove_dir_all(d).unwrap();
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dst = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dst);
        } else if p.file_name().unwrap() != "LOCK" {
            std::fs::copy(&p, dst).unwrap();
        }
    }
}

#[test]
fn operation_id_repeat_returns_stored_result() {
    let dir = tmp("operation");
    let db = Db::open(&dir).unwrap();
    db.fork(MAIN, "w").unwrap();
    db.put("w", "k1", "1").unwrap();
    db.put("w", "k2", "2").unwrap();
    let o = MergeOptions { operation: Some("op-1".into()), ..Default::default() };
    assert_eq!(db.merge_with("w", &o).unwrap(), 2);
    // the same request again (the world is gone by now): what the first did, nothing merged again
    assert_eq!(db.merge_with("w", &o).unwrap(), 2);
    assert_eq!(db.ledger_rows().unwrap().len(), 1);
    assert_eq!(db.ledger_rows().unwrap()[0]["operation"], "op-1");
    // the same id for another request: refused
    db.fork(MAIN, "v").unwrap();
    db.put("v", "k3", "3").unwrap();
    let e = db.merge_with("v", &o).unwrap_err().to_string();
    assert!(e.contains("op-1") && e.contains("already used"), "{e}");
    assert_eq!(db.get(MAIN, "k3").unwrap(), None);
    // across a restart too
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.merge_with("w", &o).unwrap(), 2);
    assert!(db.merge_with("v", &o).is_err());
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An operation id is bound to how the request settles its rows too: the same id with a row settled the other way (or
/// to another value) is another request (23505), not a repeat of the first.
#[test]
fn operation_id_binds_how_rows_are_settled() {
    use chronos::Pick;
    let dir = tmp("operation-picks");
    let db = Db::open(&dir).unwrap();
    db.put(MAIN, "k", "0").unwrap();
    db.fork(MAIN, "w").unwrap();
    db.put("w", "k", "w").unwrap();
    db.put(MAIN, "k", "main").unwrap();
    let mut o = MergeOptions { operation: Some("op-picks".into()), ..Default::default() };
    o.picks.insert("k".into(), Pick::Ours);
    let n = db.merge_with("w", &o).unwrap();
    assert_eq!(db.get(MAIN, "k").unwrap().as_deref(), Some("w"));
    assert_eq!(db.merge_with("w", &o).unwrap(), n, "the same request: what the first did");
    let refused = |r: chronos::Result<usize>| match r {
        Err(chronos::Error::Constraint { code, .. }) => code == "23505",
        _ => false,
    };
    o.picks.insert("k".into(), Pick::Theirs);
    assert!(refused(db.merge_with("w", &o)), "the row settled the other way: another request");
    o.picks.insert("k".into(), Pick::Row(Some("other".into())));
    assert!(refused(db.merge_with("w", &o)), "settled to another value: another request");
    assert_eq!(db.get(MAIN, "k").unwrap().as_deref(), Some("w"));
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

/// VERIFY LEDGER's rows, each `seq|valid|why`, all valid.
fn verified(db: &Db, n: usize) {
    let got = q(db, MAIN, "verify ledger");
    assert_eq!(got.len(), n, "{got:?}");
    assert!(got.iter().all(|r| r.contains("|true|")), "{got:?}");
}

/// Gate C (plan r3 section 5, durable ledger): a merge's row outlives the world it merged, after a checkpoint and a
/// restart too.
#[test]
fn ledger_row_survives_world_drop() {
    let dir = tmp("drop");
    let db = Db::open(&dir).unwrap();
    merge_one(&db, "a", &[("k1", "1")]);
    merge_one(&db, "b", &[("k2", "2")]);
    // (a merged world is already gone; one forked again under the same name and dropped is another world)
    db.fork(MAIN, "a").unwrap();
    db.put("a", "k3", "3").unwrap();
    db.discard("a").unwrap();
    let before = db.ledger_rows().unwrap();
    assert_eq!(before.len(), 2);
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.ledger_rows().unwrap(), before);
    verified(&db, 2);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Gate C: history kept for no time, checkpointed and cleaned up: every row stays, and the chain still verifies.
#[test]
fn ledger_row_survives_retention_and_gc() {
    let dir = tmp("gc");
    let db = Db::open(&dir).unwrap();
    db.set_retention(0).unwrap();
    for i in 0..5 {
        merge_one(&db, &format!("w{i}"), &[(&format!("k{i}"), "v")]);
        db.checkpoint().unwrap();
        db.gc().unwrap();
    }
    let before = db.ledger_rows().unwrap();
    assert_eq!(before.len(), 5);
    std::thread::sleep(std::time::Duration::from_millis(20));
    db.checkpoint().unwrap();
    db.gc().unwrap();
    assert_eq!(db.ledger_rows().unwrap(), before);
    verified(&db, 5);
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.ledger_rows().unwrap(), before);
    verified(&db, 5);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Gate C: undoing a merge, or restoring main to before it, puts the rows back but not the record that the merge
/// happened: its ledger row stays, and the chain still verifies.
#[test]
fn ledger_row_survives_undo_and_restore() {
    let dir = tmp("undo");
    let db = Db::open(&dir).unwrap();
    merge_one(&db, "a", &[("k1", "1")]);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let t = now_ms();
    std::thread::sleep(std::time::Duration::from_millis(20));
    merge_one(&db, "b", &[("k2", "2")]);
    let before = db.ledger_rows().unwrap();
    assert_eq!(before.len(), 2);

    db.undo_merge("b", false).unwrap();
    assert_eq!(db.get(MAIN, "k2").unwrap(), None, "undone");
    let after_undo = db.ledger_rows().unwrap();
    assert_eq!(&after_undo[..2], &before[..], "the undone merge's row stays");
    verified(&db, after_undo.len());

    db.restore(MAIN, t).unwrap();
    assert_eq!(db.get(MAIN, "k1").unwrap().as_deref(), Some("1"));
    assert_eq!(db.get(MAIN, "k2").unwrap(), None);
    let after_restore = db.ledger_rows().unwrap();
    assert_eq!(&after_restore[..2], &before[..], "restoring main to before a merge keeps its row");
    verified(&db, after_restore.len());

    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.ledger_rows().unwrap(), after_restore);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

const CHILD: &str = "CHRONOS_LEDGER_CRASH_CHILD";

/// The child of `ledger_and_data_all_or_nothing` (a no-op otherwise): merges world `w` into main (the folder's first
/// merge: a checkpoint first, then the merge's record), then checkpoints and cleans up.
#[test]
fn ledger_crash_child() {
    let Ok(dir) = std::env::var(CHILD) else { return };
    let db = Db::open(&dir).unwrap();
    db.merge("w", Resolve::Fail).unwrap();
    db.checkpoint().unwrap();
    db.gc().unwrap();
}

/// Gate C: a merge's ledger row is there if and only if the merge's rows are, wherever a crash cuts it: at every crash
/// point (a debug build's CHRONOS_CRASH_AT, see `disk::crash_point`) through the merge, its checkpoint and cleanup; and
/// with the log cut at every byte of the merge's record.
#[test]
fn ledger_and_data_all_or_nothing() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: crash points are compiled into debug builds only");
        return;
    }
    let (base, dir) = (tmp("crash-base"), tmp("crash"));
    {
        let db = Db::open(&base).unwrap();
        db.put(MAIN, "seed", "0").unwrap();
        db.fork(MAIN, "w").unwrap();
        for i in 0..50 {
            db.put("w", &format!("t/{i:03}"), "v").unwrap();
        }
    }
    // both or neither: the ledger row, and the merge's 50 rows in main
    let check = |db: &Db, at: &str| -> bool {
        let rows = db.scan(MAIN, "t/", usize::MAX).unwrap().iter().filter(|(k, _)| k.starts_with("t/")).count();
        let ledger = db.ledger_rows().unwrap().len();
        assert!((rows, ledger) == (0, 0) || (rows, ledger) == (50, 1), "{at}: {rows} rows, {ledger} ledger rows");
        if ledger == 1 {
            verified(db, 1);
        }
        ledger == 1
    };

    let exe = std::env::current_exe().unwrap();
    let mut at = 1;
    loop {
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&base, &dir);
        let status = std::process::Command::new(&exe)
            .args(["--exact", "ledger_crash_child", "--nocapture", "--test-threads=1"])
            .env(CHILD, &dir)
            .env("CHRONOS_CRASH_AT", at.to_string())
            .output()
            .unwrap()
            .status;
        let db = Db::open(&dir).unwrap_or_else(|e| panic!("crash point {at}: {e}"));
        let merged = check(&db, &format!("crash point {at}"));
        if status.success() {
            assert!(merged, "finished, so it's there");
            break;
        }
        drop(db);
        at += 1;
        assert!(at < 1000, "never finished");
    }

    // the log cut at every byte of the merge's record (kind 20, `Op::Applied`), as a crash mid-write leaves it
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&base, &dir);
    {
        let db = Db::open(&dir).unwrap();
        db.merge("w", Resolve::Fail).unwrap();
        copy_dir(&dir, &base.with_extension("merged"));
    }
    let merged = base.with_extension("merged");
    let mut found = 0;
    for seg in std::fs::read_dir(&merged).unwrap().map(|e| e.unwrap().path()) {
        let name = seg.file_name().unwrap().to_str().unwrap().to_string();
        if !name.starts_with("wal-") {
            continue;
        }
        let b = std::fs::read(&seg).unwrap();
        let mut off = 0;
        while off + 8 <= b.len() {
            let len = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
            if len == 0 {
                break;
            }
            if b[off + 8] == 20 {
                found += 1;
                for cut in off..=off + 8 + len {
                    let _ = std::fs::remove_dir_all(&dir);
                    copy_dir(&merged, &dir);
                    std::fs::write(dir.join(&name), &b[..cut]).unwrap();
                    let db = Db::open(&dir).unwrap_or_else(|e| panic!("log cut at {cut}: {e}"));
                    let whole = check(&db, &format!("log cut at byte {cut}"));
                    assert_eq!(whole, cut == off + 8 + len, "log cut at byte {cut}: only the whole record merges");
                }
            }
            off += 8 + len;
        }
    }
    assert_eq!(found, 1, "the merge's record was found");
    for d in [base, dir, merged] {
        let _ = std::fs::remove_dir_all(d);
    }
}
