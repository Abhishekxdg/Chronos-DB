//! Many worlds and deep worlds, on disk: fork cost, memory per world, checkpoint and reopen time,
//! and queries at depth against main.
//!
//!   WORLDS=100000 DEPTH=1000 ROWS=100000 cargo run --release --example worlds
//!   SWEEP=1 ROWS=100000 cargo run --release --example worlds   # 10 .. 100,000 worlds, one table
//!   (SIZES=10,100,1000 picks the sizes)
//!   DISCARD=one ROWS=10000 cargo run --release --example worlds  # 100,000 worlds, drop 99,000
//!   (DISCARD=one: one by one; many: in one call; expire: as expired worlds in a sweep; KEEP=1000)
use chronos::{Db, MAIN};
use std::time::{Duration, Instant};

#[path = "stats/mod.rs"]
mod stats;
use stats::{cpu_s, dir_size, load, mem_mb, ms, pct};

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    if std::env::var("SWEEP").is_ok() {
        return sweep();
    }
    if let Ok(how) = std::env::var("DISCARD") {
        return discard(&how);
    }
    let (worlds, depth, rows) = (env("WORLDS", 100_000), env("DEPTH", 1000), env("ROWS", 100_000));
    let dir = std::env::temp_dir().join(format!("chronos-worlds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let sql = |b: &str, q: &str| chronos::sql::run(&db, b, q, &[]).unwrap();
    sql(MAIN, "create table t (id bigint primary key, name text, n int)");
    for c in (0..rows).collect::<Vec<_>>().chunks(5000) {
        let v: Vec<String> = c.iter().map(|i| format!("({i}, 'row {i}', {})", i % 100)).collect();
        sql(MAIN, &format!("insert into t values {}", v.join(",")));
    }
    db.checkpoint().unwrap();
    let base = mem_mb();
    println!("{rows} rows in main; {base:.0} MB resident");

    // many worlds, each forked from main and given one write of its own
    let t = Instant::now();
    for i in 0..worlds {
        db.fork(MAIN, &format!("w{i}")).unwrap();
    }
    let fork = ms(t);
    let t = Instant::now();
    for i in 0..worlds {
        db.put(&format!("w{i}"), "t/0", &format!("{{\"id\": 0, \"name\": \"w{i}\", \"n\": 1}}")).unwrap();
    }
    let write = ms(t);
    let mem = mem_mb();
    println!(
        "{worlds} worlds: fork {:.2} µs each, first write {:.2} µs each, {:.2} KB resident per world",
        fork * 1e3 / worlds as f64,
        write * 1e3 / worlds as f64,
        (mem - base) * 1024.0 / worlds as f64
    );

    // a chain: each world forked from the one before, each changing one row
    let t = Instant::now();
    let mut prev = MAIN.to_string();
    for d in 0..depth {
        let name = format!("d{d}");
        db.fork(&prev, &name).unwrap();
        db.put(&name, &format!("t/{d}"), &format!("{{\"id\": {d}, \"name\": \"depth {d}\", \"n\": 7}}")).unwrap();
        prev = name;
    }
    println!("chain of {depth}: {:.2} µs per fork+write", ms(t) * 1e3 / depth as f64);

    let time = |b: &str, q: &str| {
        sql(b, q);
        let t = Instant::now();
        for _ in 0..20 {
            sql(b, q);
        }
        ms(t) / 20.0
    };
    for q in
        ["select name from t where id = 5", "select count(*), sum(n) from t", "select n, count(*) from t group by n"]
    {
        let (m, w, d) = (time(MAIN, q), time("w7", q), time(&prev, q));
        println!("{m:>8.3} ms main  {w:>8.3} ms world  {d:>8.3} ms depth {depth}   {q}");
    }

    let t = Instant::now();
    db.checkpoint().unwrap();
    println!("checkpoint with {} worlds: {:.0} ms", worlds + depth, ms(t));
    let _ = sql(&prev, "select count(*) from t"); // warm
    let q = "select count(*), sum(n) from t";
    println!("{:>8.3} ms depth {depth} after checkpoint   {q}", time(&prev, q));
    let size = dir_size(&dir);
    drop(db);
    let t = Instant::now();
    let db = Db::open(&dir).unwrap();
    println!("reopen: {:.0} ms, {} branches, {:.0} MB on disk", ms(t), db.branches().len(), size as f64 / 1e6);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

fn load_main(dir: &std::path::Path, rows: usize) -> Db {
    let _ = std::fs::remove_dir_all(dir);
    let db = Db::open(dir).unwrap();
    chronos::sql::run(&db, MAIN, "create table t (id bigint primary key, name text, n int)", &[]).unwrap();
    for c in (0..rows).collect::<Vec<_>>().chunks(5000) {
        let v: Vec<String> = c.iter().map(|i| format!("({i}, 'row {i}', {})", i % 100)).collect();
        chronos::sql::run(&db, MAIN, &format!("insert into t values {}", v.join(",")), &[]).unwrap();
    }
    db.checkpoint().unwrap();
    db
}

/// WORLDS worlds forked from main, each writing one row, then all but KEEP discarded (`how`: one
/// by one, in one call, or by the expiry sweep); time and footprint before and after.
fn discard(how: &str) {
    let (worlds, keep, rows) = (env("WORLDS", 100_000), env("KEEP", 1000), env("ROWS", 10_000));
    let dir = std::env::temp_dir().join(format!("chronos-discard-{}", std::process::id()));
    let db = load_main(&dir, rows);
    let mem0 = mem_mb();
    let t = Instant::now();
    for i in 0..worlds {
        db.fork(MAIN, &format!("w{i}")).unwrap();
        db.put(&format!("w{i}"), "t/0", &format!("{{\"id\": 0, \"name\": \"w{i}\", \"n\": 1}}")).unwrap();
    }
    let (fork, mem1) = (ms(t), mem_mb());
    println!("{worlds} worlds forked and written in {fork:.0} ms; memory {mem0:.0} MB before, {mem1:.0} MB after");
    let t = Instant::now();
    let listed = db.worlds().len();
    println!("worlds(): {listed} in {:.0} ms", ms(t));
    let id = db.world(&format!("w{}", worlds / 2)).unwrap().id;
    let t = Instant::now();
    for _ in 0..100 {
        db.world(&id).unwrap();
    }
    println!("world(id): {:.3} ms each", ms(t) / 100.0);
    let t = Instant::now();
    db.checkpoint().unwrap();
    println!("checkpoint with {} worlds: {:.0} ms; memory {:.0} MB", worlds + 1, ms(t), mem_mb());
    let gone: Vec<String> = (keep..worlds).map(|i| format!("w{i}")).collect();
    if how == "expire" {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        gone.iter().for_each(|w| drop(db.expire(w, Some(now)).unwrap()));
    }
    let (t, c0) = (Instant::now(), cpu_s());
    let n = match how {
        "one" => gone.iter().map(|w| db.discard(w).unwrap()).count(),
        "many" => db.discard_many(&gone).into_iter().map(|r| r.unwrap()).count(),
        "expire" => db.expire_worlds().unwrap(),
        _ => panic!("DISCARD=one, many or expire"),
    };
    let (took, cpu) = (ms(t), cpu_s() - c0);
    assert!(n == gone.len() || how == "expire"); // the background sweep may take some first
    assert_eq!(db.branches().len(), keep + 1);
    let mem2 = mem_mb();
    let t = Instant::now();
    db.checkpoint().unwrap();
    let ckpt = ms(t);
    let mem3 = mem_mb();
    // what the allocator still keeps for reuse rather than giving back to the system
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
    println!(
        "discard ({how}) {n} of {worlds}: {took:.0} ms ({:.1} µs each, {cpu:.1} s CPU); memory {mem2:.0} MB after, \
         {mem3:.0} MB after a checkpoint ({ckpt:.0} ms), {:.0} MB after malloc_trim; load {:.1}",
        took * 1e3 / n as f64,
        mem_mb(),
        load()
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// For each size: a fresh database with ROWS rows in main, then that many worlds forked from main,
/// each writing one row of its own. Every size starts from scratch.
fn sweep() {
    let rows = env("ROWS", 100_000);
    let sizes: Vec<usize> = std::env::var("SIZES")
        .unwrap_or("10,100,1000,10000,100000".into())
        .split(',')
        .map(|s| s.trim().parse().unwrap())
        .collect();
    let dir = std::env::temp_dir().join(format!("chronos-sweep-{}", std::process::id()));
    let mut table = vec![];
    for &n in &sizes {
        let db = load_main(&dir, rows);
        let (mem0, disk0, cpu0) = (mem_mb(), dir_size(&dir), cpu_s());
        let t = Instant::now();
        let mut forks: Vec<Duration> = (0..n)
            .map(|i| {
                let t = Instant::now();
                db.fork(MAIN, &format!("w{i}")).unwrap();
                t.elapsed()
            })
            .collect();
        let fork_all = ms(t);
        let mut writes: Vec<Duration> = (0..n)
            .map(|i| {
                let t = Instant::now();
                db.put(&format!("w{i}"), "t/0", &format!("{{\"id\": 0, \"name\": \"w{i}\", \"n\": 1}}")).unwrap();
                t.elapsed()
            })
            .collect();
        let cpu = cpu_s() - cpu0;
        let mem = mem_mb();
        forks.sort();
        writes.sort();
        let time = |b: &str, q: &str| {
            chronos::sql::run(&db, b, q, &[]).unwrap();
            let t = Instant::now();
            for _ in 0..20 {
                chronos::sql::run(&db, b, q, &[]).unwrap();
            }
            ms(t) * 1e3 / 20.0
        };
        let w = format!("w{}", n / 2);
        let (point, agg) = ("select name from t where id = 5", "select n, count(*), sum(id) from t group by n");
        let (pm, pw, am, aw) = (time(MAIN, point), time(&w, point), time(MAIN, agg), time(&w, agg));
        let t = Instant::now();
        db.checkpoint().unwrap();
        let ckpt = ms(t);
        let disk = dir_size(&dir);
        drop(db);
        let t = Instant::now();
        let db = Db::open(&dir).unwrap();
        let reopen = ms(t);
        assert_eq!(db.branches().len(), n + 1);
        drop(db);
        let r = format!(
            "| {n} | {:.2} | {:.2} | {:.1} | {:.2} | {:.2} | {pm:.0} / {pw:.0} | {:.1} / {:.1} | {:.1} | {:.0} | {:.1} | {:.2} | {cpu:.2} | {ckpt:.0} | {reopen:.0} | {:.1} |",
            pct(&forks, 50.0),
            pct(&forks, 99.0),
            fork_all * 1e3 / n as f64,
            pct(&writes, 50.0),
            pct(&writes, 99.0),
            am / 1e3,
            aw / 1e3,
            disk as f64 / 1e6,
            (disk.saturating_sub(disk0)) as f64 / n as f64,
            mem - mem0,
            (mem - mem0) * 1e3 / n as f64,
            load(),
        );
        println!("{r}");
        table.push(r);
    }
    println!("\n{rows} rows in main. fork and write in µs; point query in µs and aggregate in ms, main / world;");
    println!(
        "disk after checkpoint; memory is the process footprint added by the worlds; CPU user+sys for forks and writes"
    );
    println!(
        "| worlds | fork p50 µs | fork p99 µs | fork mean µs | write p50 µs | write p99 µs | point main / world µs | agg main / world ms | disk MB | disk B/world | mem MB | mem KB/world | cpu s | checkpoint ms | reopen ms | load |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    table.iter().for_each(|r| println!("{r}"));
    let _ = std::fs::remove_dir_all(&dir);
}
