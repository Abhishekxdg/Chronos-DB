//! Storage durability regressions: a checkpoint whose next log segment can't be started, a
//! checkpoint failing on a writer's thread or under a big statement, a manifest older than its
//! log and, with `--features s3`, a remote attached to a folder that already has sealed packs and
//! a folder that fell behind its remote. (Page-pack failures are unit tests in `src/pages.rs`.)

use chronos::sql::run;
use chronos::{Db, MAIN};
use std::fs;
use std::path::{Path, PathBuf};

const KEY: [u8; 32] = [9; 32];

fn tmp(name: &str) -> PathBuf {
    static ENV: std::sync::Once = std::sync::Once::new();
    // before anything reads it (once per process): a big statement spills in parts
    ENV.call_once(|| unsafe { std::env::set_var("CHRONOS_WORK_MEM", "256kB") });
    let d = std::env::temp_dir().join(format!("chronos-review-storage-{}-{name}", std::process::id()));
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

/// Puts a folder where the next log segment's file goes, so starting it fails.
fn block_next_segment(dir: &Path) -> PathBuf {
    let last = segments(dir).pop().unwrap();
    let n: u64 = last.file_name().unwrap().to_str().unwrap()["wal-".len()..].parse().unwrap();
    let block = dir.join(format!("wal-{:016}", n + 1));
    fs::create_dir(&block).unwrap();
    block
}

fn one(db: &Db, q: &str) -> String {
    let out = run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    out.rows[0][0].to_text()
}

/// Encrypted records are sealed to their segment: when the next segment can't be started, the
/// log goes on in the one it has, under that segment's number, and the folder still opens.
#[test]
fn a_segment_that_cannot_start_leaves_the_log_readable() {
    let dir = tmp("rotate");
    let db = Db::open_encrypted(&dir, &KEY).unwrap();
    db.put(MAIN, "a", "1").unwrap();
    let block = block_next_segment(&dir);
    assert!(db.checkpoint().is_err(), "the next segment can't be created");
    db.put(MAIN, "b", "2").unwrap();
    drop(db);
    fs::remove_dir(&block).unwrap();
    let db = Db::open_encrypted(&dir, &KEY).unwrap();
    assert_eq!(db.get(MAIN, "b").unwrap().as_deref(), Some("2"));
    db.checkpoint().unwrap();
    db.put(MAIN, "c", "3").unwrap();
    drop(db);
    let db = Db::open_encrypted(&dir, &KEY).unwrap();
    for (k, v) in [("a", "1"), ("b", "2"), ("c", "3")] {
        assert_eq!(db.get(MAIN, k).unwrap().as_deref(), Some(v));
    }
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

/// Past 256 MB of log a writer checkpoints on its own thread. When that fails, its write (already
/// logged) still succeeds, and checkpoints work again once the cause is gone: no panic, no
/// poisoned lock.
#[test]
fn a_failing_checkpoint_does_not_panic_writers() {
    let dir = tmp("far-behind");
    let db = Db::open(&dir).unwrap();
    let block = block_next_segment(&dir);
    let big = "x".repeat(1 << 20);
    for i in 0..280 {
        db.put(MAIN, &format!("k{i:03}"), &big).unwrap();
    }
    fs::remove_dir(&block).unwrap();
    db.checkpoint().unwrap();
    db.gc().unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.get(MAIN, "k279").unwrap().map(|v| v.len()), Some(1 << 20));
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

/// A statement too big to hold commits through a checkpoint. If the checkpoint fails before its
/// commit is durable, nobody sees the statement: not readers now, not the folder reopened.
#[test]
fn a_big_statement_whose_checkpoint_fails_is_not_seen() {
    let dir = tmp("bulk");
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "create table t (id int primary key, body text)", &[]).unwrap();
    let block = block_next_segment(&dir);
    let q = "insert into t select i, repeat(md5(i::text), 6) from generate_series(1, 6000) i";
    assert!(run(&db, MAIN, q, &[]).is_err(), "its checkpoint can't start a segment");
    assert_eq!(one(&db, "select count(*) from t"), "0");
    fs::remove_dir(&block).unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(one(&db, "select count(*) from t"), "0");
    run(&db, MAIN, q, &[]).unwrap();
    assert_eq!(one(&db, "select count(*) from t"), "6000");
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

/// An older manifest put back (a file restored or swapped in) names a log segment that's gone:
/// replaying the newer log onto it would be silently wrong, so the folder refuses to open.
#[test]
fn a_manifest_older_than_its_log_is_refused() {
    let dir = tmp("old-manifest");
    let db = Db::open(&dir).unwrap();
    db.set_retention(0).unwrap(); // no history keeps the old segments
    db.put(MAIN, "a", "1").unwrap();
    db.checkpoint().unwrap();
    let old = fs::read(dir.join("manifest")).unwrap();
    for v in ["2", "3"] {
        db.put(MAIN, "a", v).unwrap();
        db.checkpoint().unwrap();
    }
    drop(db);
    fs::write(dir.join("manifest"), old).unwrap();
    let err = Db::open(&dir).err().expect("an older manifest than the log");
    assert!(err.to_string().contains("missing"), "{err}");
    fs::remove_dir_all(&dir).unwrap();
}

#[cfg(feature = "s3")]
mod remote {
    use super::*;

    fn rows(db: &Db) -> Vec<(String, String)> {
        db.scan(MAIN, "", usize::MAX).unwrap().into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// A folder with packs sealed before a remote was attached: they go up at the next checkpoint,
    /// and evicting local copies never drops one the remote doesn't have.
    #[test]
    fn attaching_a_remote_uploads_packs_sealed_before() {
        let (dir, bucket, fresh) = (tmp("attach"), tmp("attach-bucket"), tmp("attach-fresh"));
        let url = format!("file://{}", bucket.display());
        let want = {
            // over 64 MB of pages that don't compress: the first pack is sealed
            let db = Db::open(&dir).unwrap();
            let mut r = 0x9e3779b97f4a7c15u64;
            for part in 0..10 {
                let batch: Vec<(String, String)> = (0..10_000)
                    .map(|i| {
                        let v: String = (0..64)
                            .map(|_| {
                                r ^= r << 13;
                                r ^= r >> 7;
                                r ^= r << 17;
                                format!("{r:016x}")
                            })
                            .collect();
                        (format!("k{part}-{i:05}"), v)
                    })
                    .collect();
                db.batch(MAIN, &batch.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
            }
            db.checkpoint().unwrap();
            rows(&db)
        };
        let packs = fs::read_dir(dir.join("pages"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_str().unwrap().ends_with(".idx"))
            .count();
        assert!(packs >= 1, "a pack was sealed before the remote came");
        let db = Db::open_remote(&dir, &url).unwrap();
        db.evict_local().unwrap(); // nothing uploaded yet: nothing may go
        assert!(rows(&db) == want, "every page still readable");
        db.checkpoint().unwrap(); // uploads them
        assert!(db.evict_local().unwrap() > 0);
        assert!(rows(&db) == want, "read back from the remote");
        drop(db);
        let db = Db::open_remote(&fresh, &url).unwrap();
        assert!(rows(&db) == want, "a new folder has every page");
        drop(db);
        for d in [dir, bucket, fresh] {
            fs::remove_dir_all(d).unwrap();
        }
    }

    /// A folder whose remote moved on without it (another folder wrote since) must not reopen its
    /// own older state and publish it over the newer one. It's refused; with takeover, it's set
    /// aside and the remote's opens.
    #[test]
    fn a_folder_behind_its_remote_is_not_reopened_over_it() {
        let (a, b, c, bucket) = (tmp("behind-a"), tmp("behind-b"), tmp("behind-c"), tmp("behind-bucket"));
        let url = format!("file://{}", bucket.display());
        {
            let db = Db::open_remote(&a, &url).unwrap();
            db.put(MAIN, "k", "a").unwrap();
        } // publishes and releases
        {
            let db = Db::open_remote(&b, &url).unwrap();
            assert_eq!(db.get(MAIN, "k").unwrap().as_deref(), Some("a"));
            db.put(MAIN, "k", "b").unwrap();
            db.put(MAIN, "from-b", "1").unwrap();
        }
        let err = Db::open_remote(&a, &url).err().expect("a is behind the remote");
        assert!(err.to_string().contains("hasn't seen"), "{err}");
        {
            let db = Db::open_remote_takeover(&a, &url).unwrap();
            assert_eq!(db.get(MAIN, "k").unwrap().as_deref(), Some("b"), "the remote's state, not a's old one");
            db.put(MAIN, "from-a", "2").unwrap();
        }
        let aside = fs::read_dir(&a)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_str().unwrap().starts_with("stale-"))
            .count();
        assert_eq!(aside, 1, "a's old database is kept aside, not deleted");
        {
            let db = Db::open_remote(&a, &url).unwrap(); // current now: reopens as before
            assert_eq!(db.get(MAIN, "from-a").unwrap().as_deref(), Some("2"));
        }
        let db = Db::open_remote(&c, &url).unwrap();
        for (k, v) in [("k", "b"), ("from-b", "1"), ("from-a", "2")] {
            assert_eq!(db.get(MAIN, k).unwrap().as_deref(), Some(v), "{k}");
        }
        drop(db);
        for d in [a, b, c, bucket] {
            fs::remove_dir_all(d).unwrap();
        }
    }
}
