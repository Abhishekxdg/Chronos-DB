//! Memory of a big UPDATE in a world (see tests/review_memwrite.rs).
//!
//!   memwrite build <dir> <rows>     a table `leads` of <rows> rows (WIDTH x 32 bytes of notes each, default 24), checkpointed
//!   memwrite update <dir> <rows>    in a fresh world: UPDATE the first <rows> rows, then discard;
//!                                   prints time, heap and RSS (Linux) at each step
//!
//! Heap is counted by a wrapping allocator (live and peak bytes); RSS is VmRSS / VmHWM.

use chronos::sql::run;
use chronos::{Db, MAIN};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

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

fn status(field: &str) -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with(field)).map(|l| l[field.len()..].trim().to_string()))
        .unwrap_or_else(|| "?".into())
}

fn report(step: &str, t: Instant) {
    println!(
        "{step:<22} {:>7.1} s  heap {:>6} MB (peak {:>6} MB)  rss {}  hwm {}",
        t.elapsed().as_secs_f64(),
        NOW.load(Relaxed) >> 20,
        PEAK.load(Relaxed) >> 20,
        status("VmRSS:"),
        status("VmHWM:")
    );
}

fn sql(db: &Db, b: &str, q: &str) -> String {
    let o = run(db, b, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    o.rows.first().map_or(o.tag, |r| r[0].to_text())
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (dir, n): (&str, u64) = (&a[2], a[3].parse().unwrap());
    let t = Instant::now();
    let db = Db::open(dir).unwrap();
    report("open", t);
    match a[1].as_str() {
        "build" => {
            sql(&db, MAIN, "create table leads (id int primary key, score int, name text, notes text)");
            sql(&db, MAIN, "create index leads_score on leads (score)");
            let step = 100_000;
            let w: usize = std::env::var("WIDTH").ok().and_then(|w| w.parse().ok()).unwrap_or(24);
            for s in (0..n).step_by(step as usize) {
                let e = (s + step).min(n);
                sql(
                    &db,
                    MAIN,
                    &format!(
                        "insert into leads select i, i % 100, 'lead ' || i, repeat(md5(i::text), {w}) from generate_series({s}, {}) i",
                        e - 1
                    ),
                );
            }
            db.checkpoint().unwrap();
            report("built", t);
        }
        "update" => {
            db.fork(MAIN, "w").unwrap();
            report("forked", t);
            let t0 = Instant::now();
            let tag = sql(&db, "w", &format!("update leads set score = (score + 7) % 100 where id < {n}"));
            report(&format!("updated ({tag})"), t);
            let spilled = db.counters().into_iter().find(|(n, _)| *n == "spilled_bytes").map_or(0, |(_, v)| v);
            println!("update took {:.1} s, spilled {} MB", t0.elapsed().as_secs_f64(), spilled >> 20);
            sql(&db, "w", "select count(*) from leads where score >= 0");
            report("read world", t);
            db.checkpoint().unwrap();
            report("checkpointed", t);
            db.discard("w").unwrap();
            db.checkpoint().unwrap();
            report("discarded", t);
        }
        _ => panic!("build | update"),
    }
}
