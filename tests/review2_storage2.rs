//! Second storage review: a checkpoint that fails after the log moved on is retried, the log
//! corrupt inside its last segment refuses to open instead of losing what follows, a corrupt
//! settings file is an error rather than no settings, and (with `--features s3`) a lease taken by
//! an open that fails goes back. Pages, remote listing and old-format unique entries are unit
//! tests in `src/pages.rs`, `src/remote.rs` and `src/sql.rs`.

use chronos::{Db, MAIN, Setting};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-review2-storage2-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("wal-"))
        .collect();
    v.sort();
    v
}

fn wait_for(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// A background checkpoint that fails after starting the next log segment (here: its manifest
/// can't be written) must still count as owed: the worker tries again, instead of the log growing
/// with nothing ever published.
#[test]
fn a_checkpoint_failing_after_the_cut_is_retried() {
    let dir = tmp("retry");
    let db = Db::open(&dir).unwrap();
    let block = dir.join("manifest.tmp");
    fs::create_dir(&block).unwrap(); // the manifest can't be written
    let big = "x".repeat(1 << 20);
    for i in 0..66 {
        db.put(MAIN, &format!("k{i:03}"), &big).unwrap(); // past 64 MB: the worker checkpoints
    }
    wait_for("the failed checkpoint's cut", 60, || segments(&dir).len() >= 2);
    assert!(!dir.join("manifest").exists());
    fs::remove_dir(&block).unwrap();
    // writes keep asking; after the pause a failure earns, the next try goes through
    wait_for("a retried checkpoint", 60, || {
        db.put(MAIN, "small", "1").unwrap();
        dir.join("manifest").exists()
    });
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.get(MAIN, "k065").unwrap().map(|v| v.len()), Some(1 << 20));
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

/// Damage inside the last log segment with whole records after it is corruption, not a crash's
/// torn tail: opening must refuse rather than truncate the commits after it. A torn tail (the
/// damage at the very end) still recovers.
#[test]
fn corruption_inside_the_last_segment_is_refused() {
    let dir = tmp("mid");
    let db = Db::open(&dir).unwrap();
    for k in ["first", "second", "third"] {
        db.put(MAIN, k, &format!("value-of-{k}")).unwrap();
    }
    drop(db);
    let seg = segments(&dir).pop().unwrap();
    let good = fs::read(&seg).unwrap();
    let at = good.windows(15).position(|w| w == b"value-of-second").unwrap();
    let mut bad = good.clone();
    bad[at] ^= 0xff;
    fs::write(&seg, &bad).unwrap();
    let err = Db::open(&dir).err().expect("the log is corrupt in the middle");
    assert!(err.to_string().contains("corrupt"), "{err}");
    assert_eq!(fs::read(&seg).unwrap(), bad, "nothing was truncated");
    // the same damage as a torn tail: cut right after it
    fs::write(&seg, &bad[..at + 1]).unwrap();
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.get(MAIN, "first").unwrap().as_deref(), Some("value-of-first"));
    assert_eq!(db.get(MAIN, "third").unwrap(), None);
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

/// A settings file that isn't valid JSON is an error at open, not an empty set of settings
/// (which the next ALTER SYSTEM would write back, dropping every other one).
#[test]
fn a_corrupt_settings_file_is_an_error() {
    let dir = tmp("settings");
    let db = Db::open(&dir).unwrap();
    db.set_setting(Setting::StatementTimeout, 5000).unwrap();
    db.set_setting(Setting::MaxWorlds, 7).unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!((db.setting(Setting::StatementTimeout), db.setting(Setting::MaxWorlds)), (5000, 7));
    drop(db);
    fs::write(dir.join("settings"), "{\"statement_timeout\":50").unwrap();
    let err = Db::open(&dir).err().expect("a corrupt settings file");
    assert!(err.to_string().contains("corrupt"), "{err}");
    fs::remove_dir_all(&dir).unwrap();
}

#[cfg(feature = "s3")]
mod remote {
    use super::*;

    /// An open that fails after taking the remote's lease (here: its manifest names a log segment
    /// that's gone) gives it back, so another folder can open without takeover.
    #[test]
    fn a_failed_open_releases_the_lease() {
        let (a, b, bucket) = (tmp("lease-a"), tmp("lease-b"), tmp("lease-bucket"));
        let url = format!("file://{}", bucket.display());
        {
            let db = Db::open_remote(&a, &url).unwrap();
            db.put(MAIN, "k", "1").unwrap();
        } // publishes and releases
        let seg = segments(&a).pop().unwrap();
        let n: u64 = seg.file_name().unwrap().to_str().unwrap()["wal-".len()..].parse().unwrap();
        fs::rename(&seg, a.join(format!("wal-{:016}", n + 5))).unwrap(); // the log has a gap
        let err = Db::open_remote(&a, &url).err().expect("the manifest's log segment is missing");
        assert!(err.to_string().contains("missing"), "{err}");
        let db = Db::open_remote(&b, &url).unwrap_or_else(|e| panic!("the lease is free again: {e}"));
        assert_eq!(db.get(MAIN, "k").unwrap().as_deref(), Some("1"));
        drop(db);
        for d in [a, b, bucket] {
            fs::remove_dir_all(d).unwrap();
        }
    }
}
