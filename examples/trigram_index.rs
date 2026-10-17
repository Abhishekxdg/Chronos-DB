//! `%` and `LIKE '%...%'` on a text column with and without a trigram index (`gin_trgm_ops`): the same rows,
//! and how long each takes.
//!
//!   cargo run --release --example trigram_index          env: N=200000 Q=20

use chronos::Db;
use chronos::sql::{Value, run};
use std::time::Instant;

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
    /// A made-up word of 4 to 9 letters, from a small alphabet so words share trigrams as real names do.
    fn word(&mut self) -> String {
        let len = 4 + self.below(6) as usize;
        (0..len).map(|_| b"aeioulnrstmkdp"[self.below(14) as usize] as char).collect()
    }
}

fn main() {
    let (n, q) = (env("N", 200_000), env("Q", 20));
    let db = Db::new();
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let names: Vec<String> = (0..n).map(|_| format!("{} {}", r.word(), r.word())).collect();
    for t in ["a", "b"] {
        run(&db, "main", &format!("create table {t} (id int primary key, name text)"), &[]).unwrap();
        for (k, chunk) in names.chunks(5000).enumerate() {
            let vals: Vec<String> =
                chunk.iter().enumerate().map(|(i, s)| format!("({}, '{s}')", k * 5000 + i)).collect();
            run(&db, "main", &format!("insert into {t} values {}", vals.join(", ")), &[]).unwrap();
        }
    }
    let at = Instant::now();
    run(&db, "main", "create index a_name on a using gin (name gin_trgm_ops)", &[]).unwrap();
    println!("{n} rows; index built in {:.1} s", at.elapsed().as_secs_f64());
    let queries: Vec<(String, String)> = (0..q)
        .flat_map(|i| {
            let w = &names[(i * 7919) % n];
            let word = w.split(' ').next().unwrap().to_string();
            let mid = word[1..word.len().min(5)].to_string();
            [("%".to_string(), word), ("like".to_string(), format!("%{mid}%"))]
        })
        .collect();
    for kind in ["%", "like"] {
        let (mut with, mut without, mut found) = (0.0, 0.0, 0);
        for (k, p) in queries.iter().filter(|(k, _)| k == kind) {
            let sql = |t: &str| format!("select id from {t} where name {k} $1 order by id");
            let p = [Value::Text(p.clone())];
            let at = Instant::now();
            let x = run(&db, "main", &sql("a"), &p).unwrap().pop().unwrap();
            with += at.elapsed().as_secs_f64();
            let at = Instant::now();
            let y = run(&db, "main", &sql("b"), &p).unwrap().pop().unwrap();
            without += at.elapsed().as_secs_f64();
            assert_eq!(x.rows, y.rows, "{k} {p:?}");
            found += x.rows.len();
        }
        let m = queries.iter().filter(|(k, _)| k == kind).count() as f64;
        println!(
            "{kind:>4}: {:.2} ms with the index, {:.2} ms without ({:.0} rows a query on average)",
            with / m * 1000.0,
            without / m * 1000.0,
            found as f64 / m
        );
    }
}
