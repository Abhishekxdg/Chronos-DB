//! CREATE INDEX over a table many times CHRONOS_WORK_MEM: entries are sorted with spilling and
//! written a part at a time with checkpoints between, so memory stays bounded, and the index is
//! the one row-by-row maintenance makes. This file's process runs with a 1 MB work budget and an
//! 8 MB page cache, and counts its heap.

use chronos::sql::run;
use chronos::{Db, MAIN};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Counting;
static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            PEAK.fetch_max(NOW.fetch_add(l.size(), Relaxed) + l.size(), Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        NOW.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, size) };
        if !q.is_null() {
            NOW.fetch_sub(l.size(), Relaxed);
            PEAK.fetch_max(NOW.fetch_add(size, Relaxed) + size, Relaxed);
        }
        q
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

/// One test at a time: they measure the whole process's heap.
static SERIAL: Mutex<()> = Mutex::new(());

fn setup() -> std::sync::MutexGuard<'static, ()> {
    // set before anything reads them (each is read once per process)
    unsafe {
        std::env::set_var("CHRONOS_WORK_MEM", "1MB");
        std::env::set_var("CHRONOS_CACHE_MB", "8");
    }
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-ixbuild-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn sql(db: &Db, q: &str) -> Vec<Vec<String>> {
    let o = run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    o.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect()).collect()
}

fn fails(db: &Db, q: &str) -> String {
    match run(db, MAIN, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => {
            let detail = e.info.as_ref().and_then(|i| i.detail.clone()).unwrap_or_default();
            format!("{}: {} DETAIL: {detail}", e.code, e.message)
        }
    }
}

/// `n` rows of ~420 bytes (id, g, s): s starts with a hash, so it sorts in no order of id.
fn fill(db: &Db, table: &str, n: usize) {
    for at in (0..n).step_by(20_000) {
        let to = (at + 20_000).min(n);
        sql(
            db,
            &format!(
                "insert into {table} select i, i % 1000, md5(i::text) || repeat('y', 380) from generate_series({}, {to}) i",
                at + 1
            ),
        );
    }
}

/// An index's rows (entries, or a UNIQUE constraint's rows), without the table and index names.
fn entries(db: &Db, kind: &str, table: &str, index: &str) -> Vec<(String, String)> {
    let p = format!("_sonos_{kind}/{table}/{index}/");
    db.prefix(MAIN, &p).unwrap().into_iter().map(|(k, v)| (k[p.len()..].to_string(), v.to_string())).collect()
}

#[test]
fn a_big_build_stays_within_its_memory_and_matches_row_by_row_maintenance() {
    let _one = setup();
    let dir = tmp("big");
    let db = Db::open(&dir).unwrap();
    db.set_retention(0).unwrap();
    let n = 200_000;
    sql(&db, "create table t (id int primary key, g int, s text)");
    fill(&db, "t", n);
    // the same rows, with the same indexes made before them: kept row by row as rows come
    sql(&db, "create table u (id int primary key, g int, s text)");
    sql(&db, "create index u_s on u (s)");
    sql(&db, "create unique index u_gid on u (g, id)");
    sql(&db, "create index u_low on u (upper(s)) where g < 500");
    sql(&db, "alter table u add constraint u_idg unique (id, g)");
    fill(&db, "u", n);
    db.checkpoint().unwrap();

    let base = NOW.load(Relaxed);
    PEAK.store(base, Relaxed);
    sql(&db, "create index t_s on t (s)");
    let grew = PEAK.load(Relaxed) - base;
    let bytes: usize = entries(&db, "idx", "t", "t_s").iter().map(|(k, v)| k.len() + v.len()).sum();
    eprintln!("create index: {} MB of entries, heap grew {:.1} MB at most", bytes >> 20, grew as f64 / 1048576.0);
    assert!(bytes > 80 << 20, "the index is big: {bytes} bytes");
    assert!(grew < 40 << 20, "the build held {grew} bytes; its entries are {bytes}");

    let base = NOW.load(Relaxed);
    PEAK.store(base, Relaxed);
    sql(&db, "create unique index t_gid on t (g, id)");
    sql(&db, "create index t_low on t (upper(s)) where g < 500");
    sql(&db, "alter table t add constraint t_idg unique (id, g)");
    let grew = PEAK.load(Relaxed) - base;
    eprintln!("three more: heap grew {:.1} MB at most", grew as f64 / 1048576.0);
    assert!(grew < 40 << 20, "the builds held {grew} bytes");

    for (kind, a, b) in [
        ("idx", "t_s", "u_s"),
        ("idx", "t_gid", "u_gid"),
        ("uniq", "t_gid", "u_gid"),
        ("idx", "t_low", "u_low"),
        ("uniq", "t_idg", "u_idg"),
    ] {
        let (x, y) = (entries(&db, kind, "t", a), entries(&db, kind, "u", b));
        assert!(!x.is_empty(), "{kind} {a}");
        assert!(x == y, "{kind} {a}: {} rows, maintained: {}", x.len(), y.len());
    }
    assert_eq!(entries(&db, "idx", "t", "t_low").len(), n / 2);
    // the indexes answer as the maintained ones do
    for (q, r) in [
        ("select count(*), min(id), max(id) from t where s between 'a' and 'b'", "u"),
        ("select id from t where g = 7 and id > 190000 order by id", "u"),
        ("select count(*) from t where upper(s) < 'B' and g < 500", "u"),
    ] {
        assert_eq!(sql(&db, q), sql(&db, &q.replace(" t ", &format!(" {r} "))), "{q}");
    }
    assert!(fails(&db, "insert into t values (1, 1, 'x')").contains("23505"));
    drop(db);
    // and after reopening
    let db = Db::open(&dir).unwrap();
    assert_eq!(entries(&db, "idx", "t", "t_s"), entries(&db, "idx", "u", "u_s"));
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_duplicate_found_mid_build_fails_it_cleanly() {
    let _one = setup();
    let dir = tmp("dup");
    let db = Db::open(&dir).unwrap();
    sql(&db, "create table t (id int primary key, g int, s text)");
    fill(&db, "t", 50_000);
    // g = id % 1000 repeats; (g, s) doesn't, except for the one row made to clash, far apart
    sql(&db, "update t set s = (select s from t where id = 3) where id = 40003");
    let e = fails(&db, "create unique index t_gs on t (g, s)");
    // (as Postgres says it: the index in the message, the key twice in the DETAIL)
    assert!(e.contains("23505") && e.contains("could not create unique index \"t_gs\" DETAIL: Key (g, s)=(3, "), "{e}");
    assert!(e.ends_with(") is duplicated."), "{e}");
    let e = fails(&db, "alter table t add constraint t_gs2 unique (g, s)");
    assert!(e.contains("23505") && e.contains("could not create unique index \"t_gs2\""), "{e}");
    assert!(e.contains("DETAIL: Key (g, s)=(3, ") && e.ends_with(") is duplicated."), "{e}");
    assert!(entries(&db, "idx", "t", "t_gs").is_empty() && entries(&db, "uniq", "t", "t_gs").is_empty());
    assert!(entries(&db, "uniq", "t", "t_gs2").is_empty());
    // nothing of it is left: the name is free, and rows may still repeat
    sql(&db, "insert into t values (50001, 3, (select s from t where id = 3))");
    sql(&db, "delete from t where id in (40003, 50001)");
    sql(&db, "create unique index t_gs on t (g, s)");
    assert_eq!(entries(&db, "uniq", "t", "t_gs").len(), 49_999);
    assert!(fails(&db, "insert into t values (50002, 3, (select s from t where id = 3))").contains("t_gs"));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

const CHILD: &str = "CHRONOS_IXBUILD_CHILD";

/// The child process of `a_crash_mid_build_leaves_no_index` (a no-op otherwise).
#[test]
fn child() {
    let Ok(dir) = std::env::var(CHILD) else { return };
    let db = Db::open(&dir).unwrap();
    sql(&db, "create unique index t_s on t (s)");
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

#[test]
fn a_crash_mid_build_leaves_no_index() {
    let _one = setup();
    let (base, dir) = (tmp("crash-base"), tmp("crash"));
    let want = {
        let db = Db::open(&base).unwrap();
        sql(&db, "create table t (id int primary key, g int, s text)");
        fill(&db, "t", 100_000);
        let fresh = tmp("crash-ref");
        copy_dir(&base, &fresh);
        drop(db);
        let db = Db::open(&fresh).unwrap();
        sql(&db, "create unique index t_s on t (s)");
        let want = (entries(&db, "idx", "t", "t_s"), entries(&db, "uniq", "t", "t_s"));
        drop(db);
        let _ = std::fs::remove_dir_all(&fresh);
        want
    };
    assert_eq!(want.0.len(), 100_000);
    let exe = std::env::current_exe().unwrap();
    let (mut mid, mut ms) = (0, 50);
    loop {
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&base, &dir);
        let mut child = Command::new(&exe)
            .args(["--exact", "child", "--nocapture", "--test-threads=1"])
            .env(CHILD, &dir)
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(ms));
        let finished = child.try_wait().unwrap().is_some_and(|s| s.success());
        let _ = child.kill(); // SIGKILL
        child.wait().unwrap();
        let db = Db::open(&dir).unwrap_or_else(|e| panic!("killed after {ms} ms: reopening fails: {e}"));
        db.verify().unwrap_or_else(|e| panic!("killed after {ms} ms: {e}"));
        assert_eq!(sql(&db, "select count(*) from t"), [["100000"]]);
        let left = entries(&db, "idx", "t", "t_s").len() + entries(&db, "uniq", "t", "t_s").len();
        match run(&db, MAIN, "create unique index t_s on t (s)", &[]) {
            // there was no index: whatever the build had written is replaced
            Ok(_) => {
                mid += (left > 0) as usize;
                eprintln!("killed after {ms} ms: no index, {left} of its rows written; built again");
            }
            Err(e) => {
                assert!(e.message.contains("already exists"), "{e}");
                eprintln!("killed after {ms} ms: the index was made");
            }
        }
        assert!(entries(&db, "idx", "t", "t_s") == want.0, "killed after {ms} ms: entries");
        assert!(entries(&db, "uniq", "t", "t_s") == want.1, "killed after {ms} ms: unique rows");
        assert!(fails(&db, "insert into t values (0, 0, (select s from t where id = 5))").contains("t_s"));
        drop(db);
        if finished {
            break;
        }
        ms = ms * 3 / 2;
        assert!(ms < 600_000, "the build never finished");
    }
    assert!(mid >= 1, "no kill came while the build was writing");
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&dir);
}
