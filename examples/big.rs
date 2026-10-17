//! A database several times bigger than the page cache: builds it once, reopens it cold in a
//! fresh process, and reports each query's time and the process's true memory footprint after it.
//!
//!   CHRONOS_CACHE_MB=32 MB=1000 cargo run --release --example big
//!
//! MB is about how much row data to write (default 1000; bodies are hex hashes, which barely
//! compress). DIR keeps the database (default a temporary folder, built once and reused while it
//! exists; KEEP=1 leaves it for the next run). The footprint is `top`'s MEM on macOS (RSS
//! under-reports once pages swap), the resident size elsewhere.
use chronos::sql::{Output, Value};
use chronos::{Db, MAIN};
use std::process::Command;
use std::time::Instant;

const BODY: usize = 256; // hex characters per row body: incompressible, like real text is not

fn body(i: u64) -> String {
    let mut s = String::with_capacity(BODY);
    let mut h = blake3::hash(&i.to_le_bytes());
    while s.len() < BODY {
        s.push_str(&h.to_hex());
        h = blake3::hash(h.as_bytes());
    }
    s.truncate(BODY);
    s
}

/// This process's memory footprint, as `top` reports it (e.g. `87M`).
fn footprint() -> String {
    let pid = std::process::id().to_string();
    let top = Command::new("top").args(["-l", "1", "-pid", &pid, "-stats", "mem"]).output();
    if let Ok(o) = top
        && o.status.success()
    {
        return String::from_utf8_lossy(&o.stdout).lines().last().unwrap_or("?").trim().to_string();
    }
    let ps = Command::new("ps").args(["-o", "rss=", "-p", &pid]).output();
    ps.map_or("?".into(), |o| {
        format!("{}M", String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().unwrap_or(0) >> 10)
    })
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p) as usize]
}

fn run(db: &Db, b: &str, q: &str) -> Vec<Output> {
    chronos::sql::run(db, b, q, &[]).unwrap_or_else(|e| panic!("{q}: {e:?}"))
}

fn one(db: &Db, b: &str, q: &str) -> String {
    let out = run(db, b, q);
    let rows = &out.last().unwrap().rows;
    rows.first().map_or("no rows".into(), |r| r.iter().map(Value::to_text).collect::<Vec<_>>().join(" "))
}

fn build(db: &Db, rows: u64) {
    let t = Instant::now();
    run(
        db,
        MAIN,
        "create table big (id bigint primary key, email text unique, grp bigint, cat integer, tag text, body text)",
    );
    run(db, MAIN, "create index big_grp on big (grp)");
    run(db, MAIN, "create table docs (k text primary key, n bigint, body text)");
    run(db, MAIN, "create table small (id bigint primary key, label text)");
    let v: Vec<String> = (0..50).map(|i| format!("({i}, 'label {}')", i % 10)).collect();
    run(db, MAIN, &format!("insert into small values {}", v.join(",")));
    let docs = rows / 4; // a quarter of the rows go to the text-keyed table
    for c in (0..rows).collect::<Vec<_>>().chunks(2000) {
        let v: Vec<String> = c
            .iter()
            .map(|&i| {
                let grp = i * 7919 % rows;
                format!("({i}, 'user{i}@example.com', {grp}, {}, 'tag-{}', '{}')", i % 50, i % 1000, body(i))
            })
            .collect();
        run(db, MAIN, &format!("insert into big values {}", v.join(",")));
        if c[0] < docs {
            let v: Vec<String> = c
                .iter()
                .filter(|&&i| i < docs)
                .map(|&i| {
                    format!(
                        "('{}', {i}, '{}')",
                        &blake3::hash(&(i ^ 1 << 40).to_le_bytes()).to_hex()[..16],
                        body(i ^ 1 << 41)
                    )
                })
                .collect();
            run(db, MAIN, &format!("insert into docs values {}", v.join(",")));
        }
        if c[0] % 200_000 == 0 {
            eprintln!("  {} rows, {:.0} s, footprint {}", c[0], t.elapsed().as_secs_f64(), footprint());
        }
    }
    db.checkpoint().unwrap();
    eprintln!("built {rows} + {docs} rows in {:.0} s", t.elapsed().as_secs_f64());
}

/// Runs `q` once cold and once warm; prints both times and the footprint after.
fn step(db: &Db, b: &str, name: &str, q: &str) {
    let t = Instant::now();
    let answer = one(db, b, q);
    let cold = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    one(db, b, q);
    let warm = t.elapsed().as_secs_f64() * 1e3;
    println!("{name:<34} {cold:>9.1} {warm:>9.1} {:>9}  {answer}", footprint());
}

/// `n` lookups, first cold then the same ones warm: p50 and p99 of each, in ms.
fn lookups(db: &Db, name: &str, n: u64, q: impl Fn(u64) -> String) {
    let mut times = [vec![], vec![]];
    for pass in &mut times {
        for i in 0..n {
            let q = q(i);
            let t = Instant::now();
            let out = run(db, MAIN, &q);
            pass.push(t.elapsed().as_secs_f64() * 1e3);
            assert_eq!(out[0].rows.len(), 1, "{q}");
        }
    }
    let [c, w] = &mut times;
    println!(
        "{name:<34} {:>9} {:>9} {:>9}  p50 {:.3}/{:.3} ms, p99 {:.3}/{:.3} ms (cold/warm)",
        "",
        "",
        footprint(),
        pct(c, 0.5),
        pct(w, 0.5),
        pct(c, 0.99),
        pct(w, 0.99)
    );
}

fn query(dir: &str, rows: u64) {
    println!(
        "cache {} MB, footprint before opening {}",
        std::env::var("CHRONOS_CACHE_MB").unwrap_or("256 (default)".into()),
        footprint()
    );
    let t = Instant::now();
    let db = Db::open(dir).unwrap();
    println!("opened in {:.0} ms, footprint {}", t.elapsed().as_secs_f64() * 1e3, footprint());
    println!("{:<34} {:>9} {:>9} {:>9}  answer", "", "cold ms", "warm ms", "footprint");
    let pick = |i: u64| i * 2_654_435_761 % rows; // spread over the whole table
    lookups(&db, "point lookup by key (1000)", 1000, |i| format!("select cat from big where id = {}", pick(i)));
    lookups(&db, "point lookup by B-tree index (1000)", 1000, |i| {
        format!("select id from big where grp = {}", pick(i))
    });
    let mid = rows / 2;
    step(
        &db,
        MAIN,
        "pk range (1000 keys)",
        &format!("select count(*), max(length(body)) from big where id between {mid} and {}", mid + 999),
    );
    step(&db, MAIN, "text-key range (1/256)", "select count(*), max(n) from docs where k >= 'ab' and k < 'ac'");
    step(&db, MAIN, "full aggregate", "select count(*), sum(cat), max(length(body)) from big");
    step(
        &db,
        MAIN,
        "GROUP BY (50 groups)",
        "select cat, count(*), max(grp) from big group by cat order by cat limit 1",
    );
    step(
        &db,
        MAIN,
        "join + GROUP BY",
        "select s.label, count(*) from big b join small s on s.id = b.cat group by s.label order by 1 limit 1",
    );
    step(&db, MAIN, "equality, unindexed column", "select count(*) from big where tag = 'tag-123'");
    let _ = db.discard("f"); // a kept database's from an earlier run
    db.fork(MAIN, "f").unwrap();
    run(&db, "f", "update big set tag = 'changed' where id between 1000 and 1099");
    step(&db, "f", "same in a fork with changes", "select count(*) from big where tag = 'changed'");
    step(
        &db,
        MAIN,
        "ON CONFLICT (unique) do nothing",
        &format!("insert into big values ({rows}, 'user7@example.com', 0, 0, 'x', 'x') on conflict (email) do nothing"),
    );
    step(
        &db,
        MAIN,
        "ON CONFLICT (unique) do update",
        &format!(
            "insert into big values ({}, 'user9@example.com', 0, 0, 'x', 'x') on conflict (email) do update set tag = 'upserted'",
            rows + 1
        ),
    );
    db.discard("f").unwrap();
    for (k, v) in db.counters().iter().filter(|(k, _)| k.starts_with("cache") || *k == "pages_bytes") {
        println!("{k} {v}");
    }
}

fn main() {
    let mb: u64 = std::env::var("MB").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let rows = mb * (1 << 20) / 400; // about 400 bytes a row with its key, tag, email and index entries
    let dir = std::env::var("DIR")
        .unwrap_or_else(|_| std::env::temp_dir().join(format!("chronos-big-{mb}")).display().to_string());
    if std::env::args().nth(1).as_deref() == Some("query") {
        return query(&dir, rows);
    }
    if !std::path::Path::new(&dir).exists() {
        let db = Db::open(&dir).unwrap();
        build(&db, rows);
        db.close().unwrap();
    }
    let size: u64 = std::fs::read_dir(format!("{dir}/pages"))
        .unwrap()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    println!("{dir}: {} MB of pages, {rows} + {} rows", size >> 20, rows / 4);
    // a fresh process, so nothing the build allocated counts
    let status = Command::new(std::env::current_exe().unwrap()).arg("query").status().unwrap();
    assert!(status.success());
    if std::env::var("KEEP").is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}
