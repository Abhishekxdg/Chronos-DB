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

// ---- WAL shipping (docs/designs/wal-shipping.md) ----

use chronos::{Open, Shipping};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

const SHIP_CHILD: &str = "CHRONOS_SHIP_CHILD";

/// The child process of the shipping tests (a no-op otherwise): opens `dir` on `url`, ships in
/// `mode` with a 100 ms delay, and puts rows `r00000`.. into main, saying ACK after each returns.
#[test]
fn ship_child() {
    let Ok(spec) = std::env::var(SHIP_CHILD) else { return };
    let v: Vec<&str> = spec.split('\n').collect();
    let (dir, url, mode, n) = (v[0], v[1], Shipping::parse(v[2]).unwrap(), v[3].parse::<usize>().unwrap());
    let db = Db::open_with(dir, &Open { remote: Some(url.into()), ..Default::default() }).unwrap();
    db.set_wal_shipping(mode, 100).unwrap();
    for i in 0..n {
        db.put("main", &format!("r{i:05}"), &format!("v{i}")).unwrap();
        println!("ACK {i}");
    }
    println!("DONE");
    std::thread::sleep(std::time::Duration::from_secs(120));
}

/// Runs `ship_child`; returns it once `stop_at` rows were acknowledged (or it said DONE), with how
/// many were.
fn ship_child_until(dir: &Path, url: &str, mode: &str, n: usize, stop_at: usize) -> (Child, usize) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ship_child", "--nocapture", "--test-threads=1"])
        .env(SHIP_CHILD, format!("{}\n{url}\n{mode}\n{n}", dir.display()))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut acked = 0;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if line.contains("DONE") {
            break;
        }
        // (the harness's "test ship_child ... " may come first on a line)
        if let Some((_, i)) = line.split_once("ACK ") {
            acked = i.trim().parse::<usize>().unwrap() + 1;
            if acked >= stop_at {
                break;
            }
        }
    }
    (child, acked)
}

fn kill(mut child: Child) {
    child.kill().unwrap(); // as kill -9: no close, no checkpoint, no flush
    child.wait().unwrap();
}

fn has_rows(db: &Db, n: usize) {
    for i in 0..n {
        assert_eq!(
            db.get("main", &format!("r{i:05}")).unwrap().as_deref(),
            Some(format!("v{i}").as_str()),
            "row {i} of {n}"
        );
    }
}

/// `sync`: the machine is lost mid-stream (killed, its folder gone), and a takeover from an empty
/// folder has every row whose put returned.
#[test]
fn sync_shipping_loses_no_acknowledged_write_with_the_machine() {
    let (bucket, a, b) = (tmp("ship-sync-bucket"), tmp("ship-sync-a"), tmp("ship-sync-b"));
    let url = format!("file://{}", bucket.display());
    let (child, acked) = ship_child_until(&a, &url, "sync", 1000, 60);
    kill(child);
    assert!(acked >= 60);
    fs::remove_dir_all(&a).unwrap();
    let db = Db::open_remote_takeover(&b, &url).unwrap();
    has_rows(&db, acked);
    drop(db);
    for d in [bucket, b] {
        let _ = fs::remove_dir_all(d);
    }
}

/// `async`: what was written more than the delay before the machine was lost is in the remote.
#[test]
fn async_shipping_ships_within_the_delay() {
    let (bucket, a, b) = (tmp("ship-async-bucket"), tmp("ship-async-a"), tmp("ship-async-b"));
    let url = format!("file://{}", bucket.display());
    let (child, _) = ship_child_until(&a, &url, "async", 40, usize::MAX);
    std::thread::sleep(std::time::Duration::from_millis(1500)); // the delay is 100 ms
    kill(child);
    fs::remove_dir_all(&a).unwrap();
    let db = Db::open_remote_takeover(&b, &url).unwrap();
    has_rows(&db, 40);
    drop(db);
    for d in [bucket, b] {
        let _ = fs::remove_dir_all(d);
    }
}

/// The same folder after a crash: still the lease holder, its log matches the remote's, and what it
/// writes next goes on from there (an empty folder later sees all of it).
#[test]
fn shipping_goes_on_in_the_same_folder_after_a_crash() {
    let (bucket, a, c) = (tmp("ship-same-bucket"), tmp("ship-same-a"), tmp("ship-same-c"));
    let url = format!("file://{}", bucket.display());
    let (child, acked) = ship_child_until(&a, &url, "sync", 30, usize::MAX);
    kill(child);
    assert_eq!(acked, 30);
    let db = Db::open_remote(&a, &url).unwrap();
    has_rows(&db, 30);
    db.put("main", "after", "the crash").unwrap();
    drop(db);
    let db = Db::open_remote(&c, &url).unwrap();
    has_rows(&db, 30);
    assert_eq!(db.get("main", "after").unwrap().as_deref(), Some("the crash"));
    drop(db);
    for d in [bucket, a, c] {
        let _ = fs::remove_dir_all(d);
    }
}

fn wal_objects(bucket: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(bucket)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("wal-"))
        .collect();
    v.sort();
    v
}

/// A takeover fences the old writer: its later writes never reach the remote, and its folder,
/// taken over again, catches up with what the writer in between shipped.
#[test]
fn takeover_fences_the_old_writer_and_a_stale_folder_catches_up() {
    let (bucket, a, b) = (tmp("ship-fence-bucket"), tmp("ship-fence-a"), tmp("ship-fence-b"));
    let url = format!("file://{}", bucket.display());
    let db_a = Db::open_remote(&a, &url).unwrap();
    db_a.set_wal_shipping(Shipping::Sync, 100).unwrap();
    db_a.put("main", "x", "a1").unwrap();
    db_a.set_wal_shipping(Shipping::Async, 50).unwrap(); // a fenced `sync` writer would stop the process

    let db_b = Db::open_remote_takeover(&b, &url).unwrap();
    assert_eq!(db_b.get("main", "x").unwrap().as_deref(), Some("a1"), "shipped, not checkpointed");
    db_b.set_wal_shipping(Shipping::Sync, 100).unwrap();
    db_b.put("main", "y", "b1").unwrap();

    db_a.put("main", "x", "a2-after-the-takeover").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500)); // A's shipper meets the fence
    let err = db_a.checkpoint().expect_err("fenced");
    assert!(err.to_string().contains("lost the lease") || err.to_string().contains("taken over"), "{err}");
    drop(db_a);
    assert!(
        wal_objects(&bucket)
            .iter()
            .all(|n| !fs::read(bucket.join(n)).unwrap().windows(21).any(|w| w == b"a2-after-the-takeover")),
        "nothing A wrote after the takeover is in the remote"
    );

    // A's folder, taken over again while B still runs: B's shipped rows, not A's late one
    let db = Db::open_remote_takeover(&a, &url).unwrap();
    assert_eq!(db.get("main", "x").unwrap().as_deref(), Some("a1"));
    assert_eq!(db.get("main", "y").unwrap().as_deref(), Some("b1"));
    assert!(
        fs::read_dir(&a).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("stale-log-")),
        "A's own log kept aside"
    );
    db.put("main", "z", "a3").unwrap();
    drop(db_b); // fenced in turn: its closing checkpoint fails, and it publishes nothing
    drop(db);
    let fresh = tmp("ship-fence-fresh");
    let db = Db::open_remote(&fresh, &url).unwrap();
    assert_eq!(db.get("main", "y").unwrap().as_deref(), Some("b1"));
    assert_eq!(db.get("main", "z").unwrap().as_deref(), Some("a3"));
    drop(db);
    for d in [bucket, a, b, fresh] {
        let _ = fs::remove_dir_all(d);
    }
}

/// Checkpoints delete the shipped log the remote no longer needs, keeping one checkpoint's worth.
#[test]
fn checkpoints_collect_shipped_log() {
    let (bucket, dir) = (tmp("ship-gc-bucket"), tmp("ship-gc"));
    let url = format!("file://{}", bucket.display());
    let db = Db::open_remote(&dir, &url).unwrap();
    db.set_retention(0).unwrap();
    db.set_wal_shipping(Shipping::Sync, 100).unwrap();
    for tag in ["v1", "v2", "v3", "v4", "v5"] {
        write_rows(&db, tag);
        db.checkpoint().unwrap();
    }
    let segs: std::collections::BTreeSet<String> = wal_objects(&bucket).iter().map(|n| n[4..20].to_string()).collect();
    assert!(segs.len() <= 3, "shipped log of {} segments left: {segs:?}", segs.len());
    drop(db);
    let fresh = tmp("ship-gc-fresh");
    let db = Db::open_remote(&fresh, &url).unwrap();
    assert_eq!(db.get("main", "k04999").unwrap().as_deref(), Some("v5-4999"));
    drop(db);
    for d in [bucket, dir, fresh] {
        let _ = fs::remove_dir_all(d);
    }
}

/// A chunk missing from the middle of the shipped log: an open that would replay past it refuses,
/// as a gap in the local log does.
#[test]
fn a_gap_in_the_shipped_log_refuses_to_open() {
    let (bucket, a, b) = (tmp("ship-gap-bucket"), tmp("ship-gap-a"), tmp("ship-gap-b"));
    let url = format!("file://{}", bucket.display());
    let (child, _) = ship_child_until(&a, &url, "sync", 5, usize::MAX);
    kill(child);
    let chunks = wal_objects(&bucket);
    assert!(chunks.len() >= 3, "{chunks:?}");
    fs::remove_file(bucket.join(&chunks[1])).unwrap();
    let err = Db::open_remote_takeover(&b, &url).err().expect("a gap");
    assert!(err.to_string().contains("gap"), "{err}");
    for d in [bucket, a, b] {
        let _ = fs::remove_dir_all(d);
    }
}

/// `wal_shipping` from SQL: set, shown, kept across reopen.
#[test]
fn wal_shipping_setting_from_sql() {
    use chronos::sql::Session;
    let (bucket, dir) = (tmp("ship-sql-bucket"), tmp("ship-sql"));
    let url = format!("file://{}", bucket.display());
    {
        let db = Db::open_remote(&dir, &url).unwrap();
        assert_eq!(db.wal_shipping(), (Shipping::Async, 1000), "async, 1 s, by default with a remote");
        let mut s = Session::new("main");
        s.run(&db, "alter system set wal_shipping = sync", &[]).unwrap();
        s.run(&db, "alter system set wal_shipping_delay = 250", &[]).unwrap();
        assert!(s.run(&db, "alter system set wal_shipping = sometimes", &[]).is_err());
        s.run(&db, "create table t (id int primary key); insert into t values (1)", &[]).unwrap();
    }
    let db = Db::open_remote(&dir, &url).unwrap();
    assert_eq!(db.wal_shipping(), (Shipping::Sync, 250), "kept across reopen");
    let mut s = Session::new("main");
    assert_eq!(s.run(&db, "show wal_shipping", &[]).unwrap().pop().unwrap().rows[0][0].to_text(), "sync");
    assert_eq!(s.run(&db, "show wal_shipping_delay", &[]).unwrap().pop().unwrap().rows[0][0].to_text(), "250ms");
    drop(db);
    for d in [bucket, dir] {
        let _ = fs::remove_dir_all(d);
    }
}

/// `sync` with the remote unreachable: a commit waits for it, new writes are refused (58030) once
/// uploads have failed 10 s, reads work, and everything goes on when the remote is back.
#[test]
fn sync_shipping_pauses_writes_while_the_remote_is_down() {
    use std::os::unix::fs::PermissionsExt;
    let (bucket, dir) = (tmp("ship-down-bucket"), tmp("ship-down"));
    let url = format!("file://{}", bucket.display());
    let db = std::sync::Arc::new(Db::open_remote(&dir, &url).unwrap());
    db.set_wal_shipping(Shipping::Sync, 100).unwrap();
    db.put("main", "before", "1").unwrap();
    let uploads = |db: &Db| db.counters().iter().find(|c| c.0 == "wal_shipping_uploads").unwrap().1;
    assert!(uploads(&db) > 0);

    fs::set_permissions(&bucket, fs::Permissions::from_mode(0o555)).unwrap(); // nothing can be created
    let waiting = {
        let db = db.clone();
        std::thread::spawn(move || db.put("main", "during", "2").unwrap())
    };
    std::thread::sleep(std::time::Duration::from_secs(11));
    assert!(!waiting.is_finished(), "the commit waits for the remote");
    let err = db.put("main", "refused", "3").expect_err("writes are paused");
    assert!(err.to_string().contains("object storage unreachable"), "{err}");
    assert_eq!(db.get("main", "before").unwrap().as_deref(), Some("1"), "reads work");
    let failures = db.counters().iter().find(|c| c.0 == "wal_shipping_upload_failures").unwrap().1;
    assert!(failures > 0);

    fs::set_permissions(&bucket, fs::Permissions::from_mode(0o755)).unwrap();
    waiting.join().unwrap();
    db.put("main", "after", "4").unwrap();
    drop(std::sync::Arc::into_inner(db).unwrap());
    let fresh = tmp("ship-down-fresh");
    let db = Db::open_remote(&fresh, &url).unwrap();
    for k in ["before", "during", "after"] {
        assert!(db.get("main", k).unwrap().is_some(), "{k}");
    }
    assert!(db.get("main", "refused").unwrap().is_none());
    drop(db);
    for d in [bucket, dir, fresh] {
        let _ = fs::remove_dir_all(d);
    }
}

/// A database opened in another folder keeps the settings ALTER SYSTEM put in its old one.
#[test]
fn settings_move_with_the_database() {
    use chronos::sql::Session;
    let (bucket, a, b) = (tmp("settings-bucket"), tmp("settings-a"), tmp("settings-b"));
    let url = format!("file://{}", bucket.display());
    {
        let db = Db::open_remote(&a, &url).unwrap();
        let mut s = Session::new("main");
        s.run(&db, "alter system set max_worlds = 7", &[]).unwrap();
        s.run(&db, "alter system set synchronous_commit = normal", &[]).unwrap();
        s.run(&db, "alter system set wal_shipping = sync", &[]).unwrap();
    }
    let db = Db::open_remote(&b, &url).unwrap();
    let mut s = Session::new("main");
    assert_eq!(s.run(&db, "show max_worlds", &[]).unwrap().pop().unwrap().rows[0][0].to_text(), "7");
    assert_eq!(db.sync_commit(), chronos::SyncCommit::Normal);
    assert_eq!(db.wal_shipping().0, Shipping::Sync);
    drop(db);
    for d in [bucket, a, b] {
        let _ = fs::remove_dir_all(d);
    }
}
