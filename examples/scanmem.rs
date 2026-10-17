//! Memory of plain scans as a table grows (the README's "a plain scan's memory grows").
//!
//!   scanmem build <dir> <rows>   a table `t` of <rows> narrow rows (id, score, name), checkpointed
//!   scanmem scan <dir>           on a fresh open: count(*), a filtered count and a sum, each with
//!                                its time and heap (live after, and peak during), and RSS (Linux)
//!
//! Heap is counted by a wrapping allocator; the peak is reset before each query, so each line's
//! peak is what that query alone needed above what was live before it.

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

fn mb(b: usize) -> f64 {
    b as f64 / (1 << 20) as f64
}

fn sql(db: &Db, q: &str) -> String {
    let o = run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    o.rows.first().map_or(o.tag, |r| r[0].to_text())
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = &a[2];
    let t = Instant::now();
    let db = Db::open(dir).unwrap();
    match a[1].as_str() {
        "build" => {
            let n: u64 = a[3].parse().unwrap();
            sql(&db, "create table t (id int primary key, score int, name text)");
            let step = 200_000;
            for s in (0..n).step_by(step as usize) {
                let e = (s + step).min(n) - 1;
                sql(&db, &format!("insert into t select i, i % 100, 'row ' || i from generate_series({s}, {e}) i"));
            }
            db.checkpoint().unwrap();
            println!("built {n} rows in {:.1} s", t.elapsed().as_secs_f64());
        }
        "scan" => {
            println!("open: heap {:.1} MB, rss {}", mb(NOW.load(Relaxed)), status("VmRSS:"));
            for q in [
                "select count(*) from t",
                "select count(*) from t where score >= 0",
                "select sum(score) from t",
                "select count(*) from t",
            ] {
                let before = NOW.load(Relaxed);
                PEAK.store(before, Relaxed);
                let t0 = Instant::now();
                let r = sql(&db, q);
                println!(
                    "{q:<42} = {r:>10}  {:>6.0} ms  heap live {:>6.1} MB, peak {:>6.1} MB (+{:.1} over before)  rss {}  hwm {}",
                    t0.elapsed().as_secs_f64() * 1e3,
                    mb(NOW.load(Relaxed)),
                    mb(PEAK.load(Relaxed)),
                    mb(PEAK.load(Relaxed) - before),
                    status("VmRSS:"),
                    status("VmHWM:"),
                );
            }
        }
        _ => panic!("build | scan"),
    }
}
