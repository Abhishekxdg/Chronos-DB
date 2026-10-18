//! A full disk while pages are written. A page record that fails part way through is cut off the
//! pack, so the pages written after it in the same process land, and are read, where the index
//! says; the checkpoint that hit it fails and a later one succeeds. Afterwards, and after
//! reopening, every row is there and `verify database` passes.
//!
//! `injected_page_write_failures` makes page writes fail on purpose (debug builds: see
//! `disk::page_write_fails`). `a_real_full_disk_*` fill a real small filesystem, when
//! CHRONOS_TEST_FULL_FS names a folder on one (a tmpfs mounted with `size=`, say), which they fill
//! up and free again; otherwise they are skipped.

use chronos::Db;
use chronos::sql::Session;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-diskfull-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn run(db: &Db, s: &mut Session, q: &str) -> chronos::sql::Output {
    s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap()
}

/// The rows of `t`: how many, and a hash of them all.
fn fingerprint(db: &Db) -> String {
    let s = &mut Session::new(chronos::MAIN);
    let r = run(db, s, "select count(*), md5(coalesce(string_agg(id || ':' || body, ',' order by id), '')) from t");
    r.rows[0].iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")
}

/// Inserts rows `from..from + n` into `t`: bodies that differ and don't compress, so
/// pages differ and take space.
fn insert(db: &Db, s: &mut Session, from: u64, n: u64) -> Result<(), String> {
    let q = format!(
        "insert into t select i, md5(i::text) || md5((i * 7)::text) || md5((i * 13)::text) || md5((i * 31)::text) from generate_series({from}, {}) i",
        from + n - 1
    );
    s.run(db, &q, &[]).map(drop).map_err(|e| e.to_string())
}

fn verify(db: &Db, what: &str) {
    let s = &mut Session::new(chronos::MAIN);
    s.run(db, "verify database", &[]).unwrap_or_else(|e| panic!("{what}: verify database: {e}"));
}

const CHILD: &str = "CHRONOS_DISKFULL_CHILD";

/// The child process of `injected_page_write_failures` (a no-op otherwise): page writes 3 to 6
/// fail, so the first checkpoints fail after writing some pages, and the next one writes the rest
/// into the same pack.
#[test]
fn child() {
    let Ok(dir) = std::env::var(CHILD) else { return };
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(chronos::MAIN);
    run(&db, s, "create table t (id bigint primary key, body text)");
    insert(&db, s, 0, 3000).unwrap();
    let mut failed = 0;
    while let Err(e) = db.checkpoint() {
        assert!(e.to_string().contains("injected"), "{e}");
        failed += 1;
        assert!(failed < 10, "checkpoints keep failing");
    }
    assert!(failed > 0, "no page write failed: is CHRONOS_FAIL_PAGE_WRITES wired in?");
    insert(&db, s, 3000, 2000).unwrap();
    db.checkpoint().unwrap();
    verify(&db, "in the process that hit the failures");
    println!("\nfingerprint {}", fingerprint(&db));
    drop(db);
}

#[test]
fn injected_page_write_failures() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: the failure hook is compiled into debug builds only");
        return;
    }
    let dir = tmp("injected");
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, &dir)
        .env("CHRONOS_FAIL_PAGE_WRITES", "3,4")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "child failed:\n{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    let want =
        stdout.lines().find_map(|l| l.strip_prefix("fingerprint ")).expect("the child's fingerprint").to_string();
    assert!(want.starts_with("5000|"), "{want}");

    let db = Db::open(&dir).unwrap();
    assert_eq!(fingerprint(&db), want, "rows after reopening");
    verify(&db, "after reopening");
    let s = &mut Session::new(chronos::MAIN);
    insert(&db, s, 5000, 1000).unwrap();
    db.checkpoint().unwrap();
    let want = fingerprint(&db);
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(fingerprint(&db), want, "rows written after reopening");
    verify(&db, "after writing more and reopening again");
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}

/// A folder on the small filesystem CHRONOS_TEST_FULL_FS names, removed when dropped; None (the
/// test is skipped) without one.
struct FullFs(PathBuf);

impl FullFs {
    fn new(name: &str) -> Option<FullFs> {
        let Ok(root) = std::env::var("CHRONOS_TEST_FULL_FS") else {
            eprintln!("skipped: set CHRONOS_TEST_FULL_FS to a folder on a small filesystem");
            return None;
        };
        let root = PathBuf::from(root).join(format!("diskfull-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Some(FullFs(root))
    }
}

impl Drop for FullFs {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Fills the filesystem under `dir`, then frees `room` bytes of it.
fn fill(dir: &Path, room: u64) -> PathBuf {
    let path = dir.join("filler");
    let mut f = fs::File::create(&path).unwrap();
    let chunk = vec![0x5a; 1 << 20];
    let mut len = 0u64;
    loop {
        match f.write(&chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => len += n as u64,
        }
    }
    f.sync_all().ok();
    f.set_len(len.saturating_sub(room)).unwrap();
    f.sync_all().unwrap();
    path
}

fn spawn(child: &str, var: &str, dir: &Path) -> (std::process::ExitStatus, String) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", child, "--nocapture", "--test-threads=1"])
        .env(var, dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!(
        "{child}: {}; last lines:\n{}\nstderr:\n{}",
        out.status,
        stdout.lines().rev().take(4).collect::<Vec<_>>().join("\n"),
        String::from_utf8_lossy(&out.stderr).lines().rev().take(6).collect::<Vec<_>>().join("\n"),
    );
    (out.status, stdout)
}

/// With space again: the folder opens with the rows `check` accepts, verifies, takes more rows
/// and a checkpoint, and opens and verifies again.
fn reopens_whole(dir: &Path, check: impl Fn(u64)) {
    let db = Db::open(dir).unwrap();
    let s = &mut Session::new(chronos::MAIN);
    let n: u64 = run(&db, s, "select count(*) from t").rows[0][0].to_text().parse().unwrap();
    check(n);
    verify(&db, "after freeing space and reopening");
    insert(&db, s, n, 3000).unwrap();
    db.checkpoint().unwrap();
    verify(&db, "after writing more");
    let want = fingerprint(&db);
    drop(db);
    let db = Db::open(dir).unwrap();
    assert_eq!(fingerprint(&db), want, "rows after reopening again");
    verify(&db, "after reopening again");
}

const LOG_CHILD: &str = "CHRONOS_DISKFULL_LOG_CHILD";

/// The child process of `a_real_full_disk_while_logging` (a no-op otherwise): inserts and
/// checkpoints until the disk is full, printing each insert that committed. A log write that
/// fails ends the process (see `disk::fatal`).
#[test]
fn log_child() {
    let Ok(dir) = std::env::var(LOG_CHILD) else { return };
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(chronos::MAIN);
    run(&db, s, "create table t (id bigint primary key, body text)");
    let mut from = 0;
    loop {
        if let Err(e) = insert(&db, s, from, 500) {
            println!("\ninsert failed: {e}");
            break;
        }
        from += 500;
        println!("\ncommitted {from}");
        if let Err(e) = db.checkpoint() {
            println!("\ncheckpoint failed: {e}");
            break;
        }
    }
}

#[test]
fn a_real_full_disk_while_logging() {
    let Some(root) = FullFs::new("log") else { return };
    let dir = root.0.join("db");
    drop(Db::open(&dir).unwrap());
    let filler = fill(&root.0, 6 << 20);
    let (status, stdout) = spawn("log_child", LOG_CHILD, &dir);
    assert!(!status.success() || stdout.contains("failed"), "the disk never filled");
    let committed: u64 = stdout.lines().filter_map(|l| l.strip_prefix("committed ")?.parse().ok()).max().unwrap_or(0);
    assert!(committed > 0, "nothing committed before the disk filled");
    fs::remove_file(&filler).unwrap();
    reopens_whole(&dir, |n| assert!(n >= committed && n.is_multiple_of(500), "{n} rows, {committed} committed"));
}

const CKPT_CHILD: &str = "CHRONOS_DISKFULL_CKPT_CHILD";

/// The child process of `a_real_full_disk_during_a_checkpoint` (a no-op otherwise): logs rows,
/// fills the disk to just short of full, and checkpoints: page writes run out of space part way.
/// Then it frees the space and goes on in the same process: the pages written after the failed
/// one must land where the index says.
#[test]
fn ckpt_child() {
    let Ok(dir) = std::env::var(CKPT_CHILD) else { return };
    let dir = PathBuf::from(dir);
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(chronos::MAIN);
    run(&db, s, "create table t (id bigint primary key, body text)");
    for from in (0..30_000).step_by(3000) {
        insert(&db, s, from, 3000).unwrap();
    }
    let filler = fill(dir.parent().unwrap(), 64 << 10);
    let e = db.checkpoint().expect_err("the checkpoint ran out of space");
    println!("\ncheckpoint failed: {e}");
    assert_eq!(e.kind(), std::io::ErrorKind::StorageFull, "{e}");
    assert_eq!(fingerprint(&db).split('|').next(), Some("30000"), "rows read with the disk full");
    fs::remove_file(&filler).unwrap();
    verify(&db, "after the failed checkpoint");
    db.checkpoint().unwrap();
    insert(&db, s, 30_000, 5000).unwrap();
    db.checkpoint().unwrap();
    verify(&db, "in the process that ran out of space");
    println!("\nfingerprint {}", fingerprint(&db));
}

#[test]
fn a_real_full_disk_during_a_checkpoint() {
    let Some(root) = FullFs::new("ckpt") else { return };
    let dir = root.0.join("db");
    let (status, stdout) = spawn("ckpt_child", CKPT_CHILD, &dir);
    assert!(status.success() && stdout.contains("checkpoint failed"), "child failed:\n{stdout}");
    let want = stdout.lines().find_map(|l| l.strip_prefix("fingerprint ")).expect("the child's fingerprint");
    assert!(want.starts_with("35000|"), "{want}");
    let db = Db::open(&dir).unwrap();
    assert_eq!(fingerprint(&db), want, "rows after reopening");
    drop(db);
    reopens_whole(&dir, |n| assert_eq!(n, 35_000));
}
