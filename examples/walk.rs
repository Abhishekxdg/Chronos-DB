//! Vector search paths compared on real vectors, for recall@10 and time per query: the HNSW graph
//! walked on the 8-bit vectors or on 1-bit codes (then rescored), and the 1-bit scan with
//! rescoring that searches use up to `SCAN_WORDS` (see `search`).
//!
//!   cargo run --release --example walk -- <base.f32> <queries.f32> <truth.u32> <dim>
//! Files are raw little-endian: base and queries f32 rows, truth 10 u32 row ids per query.

use chronos::search::{VecIndex, VecRows};
use roaring::RoaringBitmap;
use std::time::Instant;

fn read_f32(p: &str) -> Vec<f32> {
    std::fs::read(p).unwrap().chunks(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dim: usize = a[4].parse().unwrap();
    let (base, queries) = (read_f32(&a[1]), read_f32(&a[2]));
    let truth: Vec<u32> =
        std::fs::read(&a[3]).unwrap().chunks(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect();
    // built as the server's search index builds it, so a big one keeps 8-bit numbers as there
    let mut rows = VecRows::new(dim, base.len() / dim);
    for (i, v) in base.chunks(dim).enumerate() {
        rows.push(i as u32, v);
    }
    let ix = VecIndex::from_rows(rows);
    let t = Instant::now();
    ix.build_graph();
    println!(
        "{} x {dim}: graph built in {:.1} s, {} nodes nothing links to, measured recall {:.1}%",
        ix.len(),
        t.elapsed().as_secs_f64(),
        ix.graph_orphans().unwrap(),
        ix.graph_recall().unwrap() * 100.0
    );
    println!("tuned beam {:?}", ix.tuned_ef());
    // PERF_READY=<file>: tell a profiler the build is done (it attaches to this process), wait for it
    if let Ok(f) = std::env::var("PERF_READY") {
        std::fs::write(&f, std::process::id().to_string()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    if std::env::var("ONLY_BUILD").is_ok() {
        return;
    }
    let all: RoaringBitmap = (0..ix.len() as u32).collect();
    // CPU time of the whole process (every thread), which other programs on the machine barely move
    let cpu_ms = || {
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
        let t = |v: libc::timeval| v.tv_sec as f64 * 1e3 + v.tv_usec as f64 / 1e3;
        t(u.ru_utime) + t(u.ru_stime)
    };
    type Search<'a> = &'a dyn Fn(&[f32]) -> Vec<(u32, f32)>;
    let run = |label: &str, f: Search| {
        let cpu0 = cpu_ms();
        let (mut hit, mut ms) = (0, vec![]);
        for (qi, q) in queries.chunks(dim).enumerate() {
            let t = Instant::now();
            let got = f(q);
            ms.push(t.elapsed().as_secs_f64() * 1e3);
            let want = &truth[qi * 10..][..10];
            hit += got.iter().take(10).filter(|(id, _)| want.contains(id)).count();
        }
        ms.sort_by(f64::total_cmp);
        let n = ms.len();
        let cpu = (cpu_ms() - cpu0) / n as f64;
        println!(
            "{label:<28} recall@10 {:>6.2}%   p50 {:.3} ms   p99 {:.3} ms   cpu {cpu:.3} ms/query",
            hit as f64 / n as f64 * 10.0,
            ms[n / 2],
            ms[n * 99 / 100]
        );
    };
    if std::env::var("ONLY_DEFAULT").is_ok() {
        for _ in 0..20 {
            run("default (what searches do)", &|q| ix.search_in(q, &all, 10, 64));
        }
        return;
    }
    for ef in [128, 256, 512] {
        run(&format!("graph, 8-bit walk ef={ef}"), &|q| ix.walk(q, 10, ef, None, false).unwrap());
    }
    for ef in [128, 256, 400, 512, 1024] {
        run(&format!("graph, 1-bit walk ef={ef}"), &|q| ix.walk(q, 10, ef, None, true).unwrap());
    }
    run("default (what searches do)", &|q| ix.search_in(q, &all, 10, 64));
    for over in [16, 32, 64] {
        run(&format!("1-bit scan + rescore x{over}"), &|q| ix.exact_scan(q, &all, 10, over));
    }
}
