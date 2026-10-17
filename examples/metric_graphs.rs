//! What a graph per distance buys (issue #32): cosine and L2 nearest-neighbour search over vectors that aren't of
//! unit length, by an exact scan (what such queries did before) and through the distance's own graph
//! (`VecIndex::for_metric`). Clustered vectors of mixed lengths, as embeddings of mixed models are.
//!
//!   N=200000 DIM=384 cargo run --release --example metric_graphs
use chronos::search::{Metric, VecIndex, for_query};
use roaring::RoaringBitmap;
use std::time::Instant;

fn env(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn main() {
    let (n, dim, queries) = (env("N", 200_000), env("DIM", 384), 200);
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    let mut r = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f32 / (1u64 << 53) as f32 - 0.5
    };
    let sub = 24;
    let basis: Vec<Vec<f32>> = (0..sub).map(|_| (0..dim).map(|_| r()).collect()).collect();
    let mut point = |scale: f32| -> Vec<f32> {
        let z: Vec<f32> = (0..sub).map(|_| r()).collect();
        (0..dim).map(|i| scale * ((0..sub).map(|j| z[j] * basis[j][i]).sum::<f32>() + 0.02 * r())).collect()
    };
    let mut ix = VecIndex::new(dim);
    for i in 0..n {
        let scale = 0.2 + 3.0 * ((i * 7919 % 1000) as f32 / 1000.0);
        ix.add(&point(scale), &[]);
    }
    let qs: Vec<Vec<f32>> = (0..queries).map(|_| point(1.0)).collect();
    let all: RoaringBitmap = (0..n as u32).collect();
    println!("{n} vectors x {dim}, lengths 0.2 to 3.2, {queries} queries, top 10");
    println!("| distance | exact scan p50 | graph p50 | recall@10 | graph build |");
    println!("|---|---:|---:|---:|---:|");
    for m in [Metric::Cosine, Metric::L2] {
        let t = Instant::now();
        let d = ix.for_metric(m).expect("vectors that aren't of unit length get a copy");
        d.build_graph();
        let build = t.elapsed().as_secs_f64();
        let (mut exact_ms, mut graph_ms, mut hit) = (vec![], vec![], 0);
        for q in &qs {
            let qx = for_query(m, q);
            let t = Instant::now();
            let truth: Vec<u32> = ix.exact_by(&qx, &all, 10, m).into_iter().map(|(id, _)| id).collect();
            exact_ms.push(t.elapsed().as_secs_f64() * 1e3);
            let t = Instant::now();
            let got: Vec<u32> = d
                .graph_only(&qx, &all, 10, 0, m == Metric::Cosine)
                .unwrap_or_default()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            graph_ms.push(t.elapsed().as_secs_f64() * 1e3);
            hit += truth.iter().filter(|x| got.contains(x)).count();
        }
        let p50 = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.total_cmp(b));
            v[v.len() / 2]
        };
        println!(
            "| {m:?} | {:.2} ms | {:.3} ms | {:.1}% | {build:.1} s |",
            p50(&mut exact_ms),
            p50(&mut graph_ms),
            100.0 * hit as f64 / (10 * queries) as f64
        );
    }
}
