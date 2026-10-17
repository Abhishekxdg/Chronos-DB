//! `find` latency on main and inside a fork, for the PRD targets: p99 under 5 ms and fork search
//! within 10% of main.
//!
//!   cargo run --release --example find          env: N=100000 DIM=64 Q=300 EDITS=500

use chronos::{Db, Query};
use serde_json::json;
use std::time::{Duration, Instant};

fn env(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
    fn vec(&mut self, dim: usize) -> Vec<f32> {
        let v: Vec<f32> = (0..dim).map(|_| self.below(2001) as f32 / 1000.0 - 1.0).collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter().map(|x| x / n).collect()
    }
    fn words(&mut self, n: usize) -> String {
        (0..n).map(|_| format!("w{}", self.below(2000))).collect::<Vec<_>>().join(" ")
    }
}

fn row(r: &mut Rng, dim: usize) -> String {
    let (tenant, color, about) = (r.below(100), r.below(5), r.words(12));
    json!({ "tenant": tenant, "color": color, "about": about, "embedding": r.vec(dim) }).to_string()
}

type MakeQuery = Box<dyn Fn(&mut Rng) -> Query>;

fn pct(v: &mut [Duration], p: f64) -> f64 {
    v.sort();
    v[((v.len() - 1) as f64 * p).round() as usize].as_secs_f64() * 1e3
}

fn main() {
    let (n, dim, nq, edits) = (env("N", 100_000), env("DIM", 64), env("Q", 300), env("EDITS", 500));
    let db = Db::new();
    let mut r = Rng(5);
    let rows: Vec<(String, String)> = (0..n).map(|i| (format!("docs/{i:07}"), row(&mut r, dim))).collect();
    db.batch("main", &rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
    drop(rows);

    let t = Instant::now();
    db.find("main", &Query { table: "docs".into(), k: 1, ..Default::default() }).unwrap(); // builds the index
    eprintln!("index of {n} rows built in {:.2}s", t.elapsed().as_secs_f64());

    db.fork("main", "agent").unwrap();
    for _ in 0..edits {
        let k = format!("docs/{:07}", r.below(n as u64));
        db.put("agent", &k, &row(&mut r, dim)).unwrap();
    }

    let mixes: Vec<(&str, MakeQuery)> = vec![
        // k varies so each query is new: these rows measure the engine, not the answer cache
        (
            "filter (1%)",
            Box::new(|r| Query {
                table: "docs".into(),
                filters: vec![("tenant".into(), json!(r.below(100)))],
                k: 10 + r.below(90) as usize,
                ..Default::default()
            }),
        ),
        ("text", Box::new(|r| Query { table: "docs".into(), text: Some(r.words(2)), k: 10, ..Default::default() })),
        (
            "vector + filter (20%)",
            Box::new(move |r| Query {
                table: "docs".into(),
                filters: vec![("color".into(), json!(r.below(5)))],
                vector: Some(("embedding".into(), r.vec(dim))),
                k: 10,
                ..Default::default()
            }),
        ),
        (
            "vector, no filter",
            Box::new(move |r| Query {
                table: "docs".into(),
                vector: Some(("embedding".into(), r.vec(dim))),
                k: 10,
                ..Default::default()
            }),
        ),
        (
            "hybrid: filter + text + vector",
            Box::new(move |r| Query {
                table: "docs".into(),
                filters: vec![("color".into(), json!(r.below(5)))],
                text: Some(r.words(2)),
                vector: Some(("embedding".into(), r.vec(dim))),
                k: 10,
                offset: 0,
                ef: 0,
            }),
        ),
        (
            "repeated query (answer cache)",
            Box::new(move |_| Query {
                table: "docs".into(),
                text: Some("w1 w2".into()),
                vector: Some(("embedding".into(), vec![0.125; dim])),
                k: 10,
                ..Default::default()
            }),
        ),
    ];
    println!("{n} rows, {dim}-dim vectors, fork with {edits} changed rows, {nq} queries each\n");
    println!("| query | main p50 | main p99 | fork p50 | fork p99 | fork/main p50 |\n|---|---|---|---|---|---|");
    for (name, make) in &mixes {
        let mut res = vec![];
        for b in ["main", "agent"] {
            let mut warm = Rng(7);
            for _ in 0..20 {
                db.find(b, &make(&mut warm)).unwrap(); // unmeasured warm-up
            }
            let mut q = Rng(99); // same queries on both
            let mut lat = vec![];
            for _ in 0..nq {
                let query = make(&mut q);
                let t = Instant::now();
                db.find(b, &query).unwrap();
                lat.push(t.elapsed());
            }
            res.push((pct(&mut lat, 0.5), pct(&mut lat, 0.99)));
        }
        println!(
            "| {name} | {:.3}ms | {:.3}ms | {:.3}ms | {:.3}ms | {:.2}x |",
            res[0].0,
            res[0].1,
            res[1].0,
            res[1].1,
            res[1].0 / res[0].0
        );
    }
}
