//! Object storage (`--features s3`). Runs against a local folder standing in for a bucket, and
//! against a real S3-compatible server when CHRONOS_TEST_S3=s3://bucket/prefix and the AWS_*
//! variables are set (e.g. a local MinIO).
#![cfg(feature = "s3")]

use chronos::{Db, Resolve};
use std::fs;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-remote-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn fingerprint(db: &Db) -> String {
    let mut out = String::new();
    for (b, parent) in db.branches() {
        let rows = db.scan(&b, "", usize::MAX).unwrap();
        out += &format!("{b} <- {parent:?}\n  {rows:?}\n  {:?}\n", db.diff(&b).unwrap());
    }
    out
}

fn packs_on_disk(dir: &Path) -> usize {
    fs::read_dir(dir.join("pages"))
        .unwrap()
        .filter(|e| {
            let n = e.as_ref().unwrap().file_name().into_string().unwrap();
            n.starts_with("pack-") && !n.ends_with(".idx")
        })
        .filter(|e| e.as_ref().unwrap().metadata().unwrap().len() > 0)
        .count()
}

fn write_rows(db: &Db, tag: &str) {
    let rows: Vec<(String, String)> = (0..5000).map(|i| (format!("k{i:05}"), format!("{tag}-{i}"))).collect();
    db.batch("main", &rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
}

/// A database written on one "machine" opens on another from the remote alone.
fn roundtrip(url: &str, name: &str) {
    let (a, b) = (tmp(&format!("{name}-a")), tmp(&format!("{name}-b")));
    let want = {
        let db = Db::open_remote(&a, url).unwrap();
        write_rows(&db, "v1");
        db.fork("main", "agent").unwrap();
        db.put("agent", "k00001", "agent-edit").unwrap();
        db.checkpoint().unwrap();
        db.put("main", "after", "checkpoint").unwrap(); // published by the checkpoint on close
        fingerprint(&db)
    };

    // a second machine: an empty folder attached to the same remote
    let db = Db::open_remote(&b, url).unwrap();
    assert_eq!(packs_on_disk(&b), 0, "opening downloads indexes and the manifest, not pages");
    assert_eq!(fingerprint(&db), want);
    assert!(packs_on_disk(&b) > 0, "pages came down as they were read");

    // scale to zero: drop the local copies, reads fetch them again
    assert!(db.evict_local().unwrap() > 0);
    assert_eq!(packs_on_disk(&b), 0);
    assert_eq!(db.get("agent", "k00001").unwrap().as_deref(), Some("agent-edit"));
    db.merge("agent", Resolve::Fail).unwrap();
    drop(db);

    // and back on the first machine, attached for good: it sees the second machine's merge
    fs::remove_dir_all(&a).unwrap();
    let db = Db::open_remote(&a, url).unwrap();
    assert_eq!(db.get("main", "k00001").unwrap().as_deref(), Some("agent-edit"));
    assert!(db.branches().iter().all(|(n, _)| n != "agent"));
    drop(db);
    let err = Db::open_remote(&a, "file:///somewhere/else").err().expect("attached to another remote");
    assert!(err.to_string().contains("is attached to"), "{err}");
    for d in [a, b] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn folder_as_bucket() {
    let bucket = tmp("bucket");
    roundtrip(&format!("file://{}", bucket.display()), "file");
    fs::remove_dir_all(bucket).unwrap();
}

#[test]
fn gc_deletes_dead_packs_from_the_remote() {
    let (bucket, dir) = (tmp("gc-bucket"), tmp("gc"));
    let url = format!("file://{}", bucket.display());
    let objects = || fs::read_dir(&bucket).unwrap().count();
    let db = Db::open_remote(&dir, &url).unwrap();
    db.set_retention(0).unwrap(); // no history holds old pages (a new database keeps them a day)
    for tag in ["v1", "v2", "v3", "v4"] {
        write_rows(&db, tag);
        db.checkpoint().unwrap();
    }
    let before = objects();
    db.gc().unwrap();
    assert!(objects() < before, "{before} -> {} objects", objects());
    drop(db);
    let fresh = tmp("gc-fresh");
    let db = Db::open_remote(&fresh, &url).unwrap();
    assert_eq!(db.get("main", "k04999").unwrap().as_deref(), Some("v4-4999"));
    drop(db);
    for d in [bucket, dir, fresh] {
        fs::remove_dir_all(d).unwrap();
    }
}

#[test]
fn real_s3_when_configured() {
    let Ok(url) = std::env::var("CHRONOS_TEST_S3") else {
        eprintln!("skipped: set CHRONOS_TEST_S3=s3://bucket/prefix and AWS_* to run against real S3");
        return;
    };
    let run = format!("{}/run-{}", url.trim_end_matches('/'), std::process::id());
    roundtrip(&run, "s3");
    one_writer(&format!("{run}-lease"), "s3");
}

/// One writer per remote: a second folder is refused, takeover works when the first is gone, and
/// the first is fenced off from then on without damaging what the new owner wrote.
#[test]
fn one_writer_per_remote() {
    let bucket = tmp("lease-bucket");
    one_writer(&format!("file://{}", bucket.display()), "file");
    fs::remove_dir_all(bucket).unwrap();
}

fn one_writer(url: &str, name: &str) {
    let (a, b, c) = (tmp(&format!("{name}-lease-a")), tmp(&format!("{name}-lease-b")), tmp(&format!("{name}-lease-c")));
    let url = url.to_string();
    let db_a = Db::open_remote(&a, &url).unwrap();
    write_rows(&db_a, "a");
    db_a.fork("main", "a-work").unwrap();
    db_a.put("a-work", "k00001", "unfinished").unwrap();
    db_a.checkpoint().unwrap(); // a mid-run manifest: A may write more after it

    let err = Db::open_remote(&b, &url).err().expect("the remote is in use");
    assert!(err.to_string().contains("is open elsewhere"), "{err}");

    // pretend A's machine died: B takes over and writes enough to seal packs of its own
    let db_b = Db::open_remote_takeover(&b, &url).unwrap();
    assert_eq!(db_b.get("main", "k00007").unwrap().as_deref(), Some("a-7"));
    // A's open branch came from a manifest taken while A was still running: it may be missing
    // later writes, so it's flagged like after a crash
    assert!(matches!(db_b.merge("a-work", Resolve::Fail), Err(chronos::Error::Stale { .. })));
    db_b.discard("a-work").unwrap();
    write_rows(&db_b, "b");
    db_b.checkpoint().unwrap();

    // A comes back to life: fenced, and nothing it tries reaches the remote
    write_rows(&db_a, "stale");
    let err = db_a.checkpoint().expect_err("fenced");
    assert!(err.to_string().contains("lost the lease"), "{err}");
    drop(db_a);

    drop(db_b); // publishes and releases
    let db_c = Db::open_remote(&c, &url).unwrap();
    assert_eq!(db_c.get("main", "k00007").unwrap().as_deref(), Some("b-7"), "B's data, untouched by A");
    drop(db_c);
    for d in [a, b, c] {
        let _ = fs::remove_dir_all(d);
    }
}
