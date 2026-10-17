//! A big UPDATE in a world goes to pages early instead of sitting in the world's overlay.
//!
//! An UPDATE counted only its rows' values against CHRONOS_WORK_MEM, about a fifth of what it
//! really held (the rows it makes, their index entries, the log record, the overlay), so on a
//! table of narrow rows a 200,000-row UPDATE never went on in parts: 280 MB sat in the overlay
//! until a checkpoint. This file's process runs with a 256 kB work budget and UPDATEs 400 narrow
//! rows, which the old count put under the spill point and the new one puts over it; then the
//! world must read right, merge, discard (freeing its memory), survive crashes and keep its past.

use chronos::sql::run;
use chronos::{Db, MAIN, Resolve};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Counting;
static NOW: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            NOW.fetch_add(l.size(), Relaxed);
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
            NOW.fetch_add(size, Relaxed);
        }
        q
    }
}

#[global_allocator]
static HEAP: Counting = Counting;

static SERIAL: Mutex<()> = Mutex::new(());

const ROWS: usize = 400;
const CHILD: &str = "CHRONOS_MEMWRITE_CHILD";

fn setup() -> std::sync::MutexGuard<'static, ()> {
    unsafe {
        std::env::set_var("CHRONOS_WORK_MEM", "256kB");
        std::env::set_var("CHRONOS_CACHE_MB", "8");
    }
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-memwrite-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn on(db: &Db, b: &str, q: &str) -> String {
    let o = run(db, b, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    o.rows.first().map_or(o.tag, |r| r[0].to_text())
}

fn spilled(db: &Db) -> u64 {
    db.counters().into_iter().find(|(n, _)| *n == "spilled_bytes").map_or(0, |(_, v)| v)
}

/// Main with `ROWS` narrow rows (g = 0) and an index on g, checkpointed, and a world `w`.
fn base(db: &Db) {
    on(db, MAIN, "create table t (id int primary key, g int, body text)");
    on(db, MAIN, "create index t_g on t (g)");
    on(db, MAIN, &format!("insert into t select i, 0, 'body ' || i from generate_series(1, {ROWS}) i"));
    db.fork(MAIN, "w").unwrap();
    db.checkpoint().unwrap();
}

const UPDATE: &str = "update t set g = g + 1";

/// Rows of `b` by g, as "g:count" pairs.
fn by_g(db: &Db, b: &str) -> String {
    let o = run(db, b, "select g, count(*) from t group by g order by g", &[]).unwrap().pop().unwrap();
    o.rows.iter().map(|r| format!("{}:{}", r[0].to_text(), r[1].to_text())).collect::<Vec<_>>().join(" ")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

#[test]
fn a_big_update_in_a_world_goes_to_pages_and_reads_merges_and_travels_right() {
    let _one = setup();
    let dir = tmp("world");
    let db = Db::open(&dir).unwrap();
    base(&db);
    let t0 = now_ms();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let before = spilled(&db);
    assert_eq!(on(&db, "w", UPDATE), format!("UPDATE {ROWS}"));
    assert!(spilled(&db) > before, "the UPDATE went on in parts, not into the overlay");
    // the world reads its update, through the index too; main doesn't see it
    assert_eq!(by_g(&db, "w"), format!("1:{ROWS}"));
    assert_eq!(on(&db, "w", "select count(*) from t where g = 1"), ROWS.to_string());
    assert_eq!(on(&db, "w", "select body from t where id = 7"), "body 7");
    assert_eq!(by_g(&db, MAIN), format!("0:{ROWS}"));
    // a small write on top, then the past: before the update, every row had g = 0
    on(&db, "w", "update t set g = 5 where id = 1");
    assert_eq!(db.view_at("w", t0).unwrap().prefix("t/").unwrap().count(), ROWS);
    assert_eq!(on(&db, "w", "select count(*) from t where g = 1"), (ROWS - 1).to_string());
    // after reopening, the same
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(by_g(&db, "w"), format!("1:{} 5:1", ROWS - 1));
    assert_eq!(by_g(&db, MAIN), format!("0:{ROWS}"));
    // merged: main has it all, index included
    db.merge("w", Resolve::Fail).unwrap();
    assert_eq!(by_g(&db, MAIN), format!("1:{} 5:1", ROWS - 1));
    assert_eq!(on(&db, MAIN, "select count(*) from t where g = 1"), (ROWS - 1).to_string());
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discarding_the_world_after_it_gives_its_memory_back() {
    let _one = setup();
    let dir = tmp("discard");
    let db = Db::open(&dir).unwrap();
    base(&db);
    on(&db, "w", "select count(*) from t where body <> ''"); // the cache as warm as it gets
    let start = NOW.load(Relaxed);
    for _ in 0..5 {
        db.fork(MAIN, "x").unwrap();
        on(&db, "x", UPDATE);
        assert_eq!(by_g(&db, "x"), format!("1:{ROWS}"));
        db.discard("x").unwrap();
        db.checkpoint().unwrap();
        db.gc().unwrap();
    }
    let end = NOW.load(Relaxed);
    assert!(end < start + (2 << 20), "memory grew from {start} to {end} bytes over five discarded updates");
    assert_eq!(by_g(&db, MAIN), format!("0:{ROWS}"));
    db.verify().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The child of `a_crash_anywhere_in_it_leaves_the_world_before_or_after` (a no-op otherwise).
#[test]
fn child() {
    let Ok(dir) = std::env::var(CHILD) else { return };
    unsafe { std::env::set_var("CHRONOS_WORK_MEM", "256kB") };
    let db = Db::open(&dir).unwrap();
    on(&db, "w", UPDATE);
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

/// At every crash point (a debug build's CHRONOS_CRASH_AT, see `disk::crash_point`) the child's
/// UPDATE passes, the world opens with all of it or none, and main is untouched.
#[test]
fn a_crash_anywhere_in_it_leaves_the_world_before_or_after() {
    let _one = setup();
    let (base_dir, dir) = (tmp("crash-base"), tmp("crash"));
    {
        let db = Db::open(&base_dir).unwrap();
        base(&db);
    }
    let exe = std::env::current_exe().unwrap();
    let mut at = 1;
    loop {
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&base_dir, &dir);
        let status = Command::new(&exe)
            .args(["--exact", "child", "--nocapture", "--test-threads=1"])
            .env(CHILD, &dir)
            .env("CHRONOS_CRASH_AT", at.to_string())
            .output()
            .unwrap()
            .status;
        let db = Db::open(&dir).unwrap_or_else(|e| panic!("crash point {at}: {e}"));
        db.verify().unwrap_or_else(|e| panic!("crash point {at}: {e}"));
        let w = by_g(&db, "w");
        assert!(w == format!("0:{ROWS}") || w == format!("1:{ROWS}"), "crash point {at}: the world has {w}");
        assert_eq!(
            on(&db, "w", "select count(*) from t where g = 1"),
            if w.starts_with('1') { ROWS.to_string() } else { "0".into() }
        );
        assert_eq!(by_g(&db, MAIN), format!("0:{ROWS}"), "crash point {at}");
        if status.success() {
            assert_eq!(w, format!("1:{ROWS}"), "finished, so it's there");
            break;
        }
        drop(db);
        at += 1;
        assert!(at < 1000, "never finished");
    }
    let _ = std::fs::remove_dir_all(&base_dir);
    let _ = std::fs::remove_dir_all(&dir);
}
