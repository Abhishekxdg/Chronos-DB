//! Search spike benchmark: filtered top-10 vector search, Chronos (embedded) vs Postgres + pgvector (HNSW).
//!
//!   cargo run --release --example search
//!   env: N=rows (1000000) DIM=384 Q=queries per mix (500) OVERSAMPLE=candidates per result (30)
//!        ONLY=chronos|postgres  EF=pgvector hnsw.ef_search (default: pgvector's 40)
//!
//! Data is synthetic and deterministic: 1,000 Gaussian clusters, unit-normalized, with two tags:
//! tenant (100 values) and kind (10 values). Recall@10 is measured against exact brute force.
//! Postgres tuning pass (documented): hnsw.iterative_scan = relaxed_order so filtered queries fill k.

use chronos::search::VecIndex;
use pgvector::Vector;
use postgres::binary_copy::BinaryCopyInWriter;
use postgres::{Client, NoTls};
use std::time::{Duration, Instant};

const K: usize = 10;
const CLUSTERS: usize = 1000;

fn env(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    fn gauss(&mut self) -> f32 {
        let (u, v) = (self.unit().max(1e-7), self.unit());
        (-2.0 * u.ln()).sqrt() * (std::f32::consts::TAU * v).cos()
    }
}

struct Gen {
    dim: usize,
    centroids: Vec<Vec<f32>>,
}

impl Gen {
    fn new(dim: usize) -> Self {
        let mut r = Rng(1);
        Gen { dim, centroids: (0..CLUSTERS).map(|_| (0..dim).map(|_| r.gauss()).collect()).collect() }
    }
    fn point(&self, r: &mut Rng) -> Vec<f32> {
        let c = &self.centroids[r.next() as usize % CLUSTERS];
        let mut v: Vec<f32> = (0..self.dim).map(|i| c[i] + 0.7 * r.gauss()).collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= n);
        v
    }
    /// Deterministic rows: (vector, tenant, kind).
    fn rows(&self, n: usize) -> impl Iterator<Item = (Vec<f32>, i32, i32)> + '_ {
        let mut r = Rng(2);
        (0..n).map(move |_| {
            let v = self.point(&mut r);
            (v, (r.next() % 100) as i32, (r.next() % 10) as i32)
        })
    }
}

/// (label, tenant filter, kind filter)
const MIXES: [(&str, bool, bool); 4] = [
    ("no filter", false, false),
    ("kind (10%)", false, true),
    ("tenant (1%)", true, false),
    ("tenant+kind (0.1%)", true, true),
];

struct Query {
    v: Vec<f32>,
    tenant: i32,
    kind: i32,
}

fn pct(v: &mut [Duration], p: f64) -> f64 {
    v.sort();
    v[((v.len() - 1) as f64 * p).round() as usize].as_secs_f64() * 1e3
}

fn recall(got: &[u32], truth: &[u32]) -> f64 {
    got.iter().filter(|id| truth.contains(id)).count() as f64 / truth.len().max(1) as f64
}

fn report(system: &str, mix: &str, lat: &mut [Duration], rec: f64) {
    println!("| {system} | {mix} | {:.3}ms | {:.3}ms | {:.1}% |", pct(lat, 0.5), pct(lat, 0.99), rec * 100.0);
}

fn pg_setup(c: &mut Client, data: &Gen, n: usize) {
    let exists: i64 = c
        .query_one("SELECT count(*) FROM pg_indexes WHERE tablename = 'items' AND indexname = 'items_emb_idx'", &[])
        .unwrap()
        .get(0);
    if exists == 1 {
        eprintln!("postgres: reusing loaded + indexed table");
        return;
    }
    let t = Instant::now();
    c.batch_execute(&format!(
        "CREATE EXTENSION IF NOT EXISTS vector;
         DROP TABLE IF EXISTS items;
         CREATE TABLE items (id int4 PRIMARY KEY, tenant int4, kind int4, emb vector({}))",
        data.dim
    ))
    .unwrap();
    let vt = c.prepare("SELECT $1::vector").unwrap().params()[0].clone();
    let w = c.copy_in("COPY items (id, tenant, kind, emb) FROM STDIN WITH (FORMAT BINARY)").unwrap();
    let mut w = BinaryCopyInWriter::new(
        w,
        &[postgres::types::Type::INT4, postgres::types::Type::INT4, postgres::types::Type::INT4, vt],
    );
    for (id, (v, tenant, kind)) in data.rows(n).enumerate() {
        w.write(&[&(id as i32), &tenant, &kind, &Vector::from(v)]).unwrap();
    }
    w.finish().unwrap();
    eprintln!("postgres: loaded {n} rows in {:.0}s, building indexes...", t.elapsed().as_secs_f64());
    let t = Instant::now();
    c.batch_execute(
        "SET maintenance_work_mem = '2GB';
         SET max_parallel_maintenance_workers = 7;
         CREATE INDEX ON items (tenant);
         CREATE INDEX ON items (kind);
         CREATE INDEX items_emb_idx ON items USING hnsw (emb vector_ip_ops);
         ANALYZE items",
    )
    .unwrap();
    eprintln!("postgres: indexes built in {:.0}s", t.elapsed().as_secs_f64());
}

fn main() {
    let (n, dim, q, os) = (env("N", 1_000_000), env("DIM", 384), env("Q", 500), env("OVERSAMPLE", 30));
    let only = std::env::var("ONLY").unwrap_or_default();
    let on = |s: &str| only.is_empty() || only == s;
    let data = Gen::new(dim);
    let mut r = Rng(3);
    let queries: Vec<Query> = (0..q)
        .map(|_| Query { v: data.point(&mut r), tenant: (r.next() % 100) as i32, kind: (r.next() % 10) as i32 })
        .collect();

    // Postgres loads first so its build and our index never compete for RAM.
    let mut pg = on("postgres")
        .then(|| Client::connect(&format!("host=/tmp dbname=chronos_vec_{n}_{dim}"), NoTls).ok())
        .flatten()
        .or_else(|| {
            let mut admin = Client::connect("host=/tmp dbname=postgres", NoTls).ok()?;
            on("postgres").then_some(())?;
            admin.batch_execute(&format!("CREATE DATABASE chronos_vec_{n}_{dim}")).ok()?;
            Client::connect(&format!("host=/tmp dbname=chronos_vec_{n}_{dim}"), NoTls).ok()
        });
    if let Some(c) = pg.as_mut() {
        pg_setup(c, &data, n);
    }

    let t = Instant::now();
    let mut ix = VecIndex::new(dim);
    for (v, tenant, kind) in data.rows(n) {
        ix.add(&v, &[("tenant", &tenant.to_string()), ("kind", &kind.to_string())]);
    }
    eprintln!("chronos: indexed {n} rows in {:.1}s", t.elapsed().as_secs_f64());
    if std::env::var("CHRONOS_VECTOR").map_or(true, |v| v != "flat") {
        let t = Instant::now();
        ix.build_graph();
        eprintln!(
            "chronos: built the HNSW graph in {:.1}s (tuned beam {}, measured recall {:.1}%)",
            t.elapsed().as_secs_f64(),
            ix.tuned_ef().unwrap_or(0),
            ix.graph_recall().unwrap_or(0.0) * 100.0
        );
    }

    // EFS=64,128,256: rerun the unfiltered queries at each graph beam width, on the one graph
    if let Ok(efs) = std::env::var("EFS") {
        let truth: Vec<Vec<u32>> =
            queries.iter().map(|qq| ix.exact(&qq.v, &[], K).into_iter().map(|(id, _)| id).collect()).collect();
        println!("| beam | p50 | p99 | recall@{K} |\n|---|---|---|---|");
        for ef in efs.split(',').filter_map(|e| e.parse().ok()) {
            ix.set_ef(ef);
            let (mut lat, mut rec) = (vec![], 0.0);
            for (qq, tr) in queries.iter().zip(&truth) {
                let t = Instant::now();
                let got = ix.search(&qq.v, &[], K, os);
                lat.push(t.elapsed());
                rec += recall(&got.iter().map(|(id, _)| *id).collect::<Vec<_>>(), tr);
            }
            println!(
                "| {ef} | {:.3}ms | {:.3}ms | {:.1}% |",
                pct(&mut lat, 0.5),
                pct(&mut lat, 0.99),
                rec * 100.0 / q as f64
            );
        }
        return;
    }
    println!("{n} rows, {dim} dims, top-{K}, {q} queries per filter mix, chronos oversample {os}\n");
    println!("| system | filter | p50 | p99 | recall@{K} |\n|---|---|---|---|---|");
    for (mix, use_t, use_k) in MIXES {
        let filters = |qq: &Query| -> Vec<(&'static str, String)> {
            let mut f = vec![];
            if use_t {
                f.push(("tenant", qq.tenant.to_string()));
            }
            if use_k {
                f.push(("kind", qq.kind.to_string()));
            }
            f
        };
        let truth: Vec<Vec<u32>> = queries
            .iter()
            .map(|qq| {
                let f = filters(qq);
                let f: Vec<_> = f.iter().map(|(a, b)| (*a, b.as_str())).collect();
                ix.exact(&qq.v, &f, K).into_iter().map(|(id, _)| id).collect()
            })
            .collect();

        if on("chronos") {
            let (mut lat, mut rec) = (vec![], 0.0);
            for (qq, tr) in queries.iter().zip(&truth) {
                let f = filters(qq);
                let f: Vec<_> = f.iter().map(|(a, b)| (*a, b.as_str())).collect();
                let t = Instant::now();
                let got = ix.search(&qq.v, &f, K, os);
                lat.push(t.elapsed());
                rec += recall(&got.iter().map(|(id, _)| *id).collect::<Vec<_>>(), tr);
            }
            report("chronos", mix, &mut lat, rec / q as f64);
        }

        if let Some(c) = pg.as_mut() {
            let mut sql = String::from("SELECT id FROM items WHERE true");
            if use_t {
                sql += " AND tenant = $2";
            }
            if use_k {
                sql += if use_t { " AND kind = $3" } else { " AND kind = $2" };
            }
            sql += &format!(" ORDER BY emb <#> $1 LIMIT {K}");
            c.batch_execute("SET hnsw.iterative_scan = relaxed_order").unwrap();
            if let Ok(ef) = std::env::var("EF") {
                c.batch_execute(&format!("SET hnsw.ef_search = {}", ef.parse::<u32>().expect("EF is a number")))
                    .unwrap();
            }
            let st = c.prepare(&sql).unwrap();
            let (mut lat, mut rec) = (vec![], 0.0);
            for (qq, tr) in queries.iter().zip(&truth) {
                let v = Vector::from(qq.v.clone());
                let mut p: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&v];
                if use_t {
                    p.push(&qq.tenant);
                }
                if use_k {
                    p.push(&qq.kind);
                }
                let t = Instant::now();
                let rows = c.query(&st, &p).unwrap();
                lat.push(t.elapsed());
                let got: Vec<u32> = rows.iter().map(|r| r.get::<_, i32>(0) as u32).collect();
                rec += recall(&got, tr);
            }
            report("postgres+pgvector", mix, &mut lat, rec / q as f64);
        }
    }
}
