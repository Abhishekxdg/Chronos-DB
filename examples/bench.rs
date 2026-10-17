//! Phase 0/1 harness: N agents each fork main, write ROWS rows, merge back.
//! Chronos vs SQLite (branch = file copy) vs Postgres (branch = CREATE DATABASE ... TEMPLATE).
//!
//!   cargo run --release --example bench -- 10 100 1000
//!   env: SEED=rows in main (default 100000), ONLY=chronos,chronos-disk,sqlite,postgres,
//!        WORKERS=cap on Chronos threads (default: one per agent; 25 matches the Postgres run),
//!        TIMINGS=1 prints the database's own timings (merges, fsyncs, checkpoints) after each run
//!
//! Baselines are deliberately generous: SQLite and Postgres merges get batched writes, and the
//! Postgres merge replays the agent's known writes instead of diffing two databases.

use chronos::{Db, Resolve};
use postgres::{Client, NoTls};
use rusqlite::Connection;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const ROWS: usize = 1000;
const PG: &str = "host=/tmp";

fn seed() -> usize {
    std::env::var("SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(100_000)
}
fn val(i: usize) -> String {
    format!("{i:0>100}")
}

struct Run {
    workers: usize,
    forks: Vec<Duration>,
    merges: Vec<Duration>,
    wall: Duration,
}

/// Runs `n` agents on `workers` threads; each agent returns (fork time, merge time).
fn run(n: usize, workers: usize, agent: impl Fn(usize) -> (Duration, Duration) + Sync) -> Run {
    let next = AtomicUsize::new(0);
    let t = Instant::now();
    let out: Vec<_> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut v = vec![];
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break v;
                        }
                        v.push(agent(i));
                    }
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let wall = t.elapsed();
    let (mut forks, mut merges): (Vec<_>, Vec<_>) = out.into_iter().unzip();
    forks.sort();
    merges.sort();
    Run { workers, forks, merges, wall }
}

fn chronos(n: usize) -> Run {
    chronos_on(n, Db::new())
}

/// Same workload on a database folder: every merge is fsynced (group commit).
fn chronos_disk(n: usize) -> Run {
    let dir = std::env::temp_dir().join(format!("chronos-bench-disk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let r = chronos_on(n, Db::open(&dir).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
    r
}

fn chronos_on(n: usize, db: Db) -> Run {
    let rows: Vec<(String, String)> = (0..seed()).map(|i| (format!("seed:{i:08}"), val(i))).collect();
    db.batch("main", &rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
    let workers = std::env::var("WORKERS").ok().and_then(|s| s.parse().ok()).map_or(n, |w: usize| n.min(w));
    let r = run(n, workers, |i| {
        let b = format!("a{i}");
        let t = Instant::now();
        db.fork("main", &b).unwrap();
        let fork = t.elapsed();
        for j in 0..ROWS {
            db.put(&b, &format!("a{i}:{j}"), &val(j)).unwrap();
        }
        let t = Instant::now();
        db.merge(&b, Resolve::Fail).unwrap();
        (fork, t.elapsed())
    });
    if std::env::var("TIMINGS").is_ok() {
        for t in db.metrics().timings() {
            eprintln!(
                "  {:<10} {:>7} calls, total {:>9.1} ms, p50 {:.3} p99 {:.3} max {:.1} ms",
                t.op, t.count, t.total_ms, t.p50_ms, t.p99_ms, t.max_ms
            );
        }
    }
    r
}

fn sqlite(n: usize) -> Run {
    let dir = std::env::temp_dir().join(format!("chronos-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("main.db");
    {
        let mut c = Connection::open(&main).unwrap();
        c.execute_batch("CREATE TABLE kv(k TEXT PRIMARY KEY, v TEXT) WITHOUT ROWID").unwrap();
        let tx = c.transaction().unwrap();
        {
            let mut st = tx.prepare("INSERT INTO kv VALUES(?1, ?2)").unwrap();
            for i in 0..seed() {
                st.execute((format!("seed:{i:08}"), val(i))).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    // Copies must not see a half-written main: forks share the lock, merges take it exclusively.
    let lock = RwLock::new(());
    // ponytail: capped at 64 workers — 1000 open DB files blows macOS's default fd limit.
    let r = run(n, n.min(64), |i| {
        let f = dir.join(format!("a{i}.db"));
        let t = Instant::now();
        {
            let _g = lock.read().unwrap();
            std::fs::copy(&main, &f).unwrap();
        }
        let fork = t.elapsed();
        {
            let mut c = Connection::open(&f).unwrap();
            let tx = c.transaction().unwrap();
            {
                let mut st = tx.prepare("INSERT OR REPLACE INTO kv VALUES(?1, ?2)").unwrap();
                for j in 0..ROWS {
                    st.execute((format!("a{i}:{j}"), val(j))).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let t = Instant::now();
        {
            let _g = lock.write().unwrap();
            let m = Connection::open(&main).unwrap();
            m.execute("ATTACH ?1 AS a", [f.to_str().unwrap()]).unwrap();
            // no change tracking in a file copy, so merge = diff the whole table
            m.execute_batch("INSERT OR REPLACE INTO main.kv SELECT k, v FROM a.kv EXCEPT SELECT k, v FROM main.kv")
                .unwrap();
        }
        let merge = t.elapsed();
        std::fs::remove_file(&f).unwrap();
        (fork, merge)
    });
    std::fs::remove_dir_all(&dir).unwrap();
    r
}

fn pg(db: &str) -> Client {
    Client::connect(&format!("{PG} dbname={db}"), NoTls).unwrap()
}

fn postgres(n: usize) -> Option<Run> {
    let mut admin = Client::connect(&format!("{PG} dbname=postgres"), NoTls).ok()?;
    for d in ["chronos_main", "chronos_tmpl"] {
        admin.batch_execute(&format!("DROP DATABASE IF EXISTS {d}")).unwrap();
    }
    admin.batch_execute("CREATE DATABASE chronos_tmpl").unwrap();
    pg("chronos_tmpl")
        .batch_execute(&format!(
            "CREATE TABLE kv(k TEXT PRIMARY KEY, v TEXT);
             INSERT INTO kv SELECT 'seed:' || lpad(i::text, 8, '0'), lpad(i::text, 100, '0')
             FROM generate_series(0, {}) i",
            seed() - 1
        ))
        .unwrap();
    // The template must have no connections, so agents fork a frozen copy of main, not live main.
    admin.batch_execute("CREATE DATABASE chronos_main TEMPLATE chronos_tmpl").unwrap();
    let upsert = "INSERT INTO kv VALUES($1, $2) ON CONFLICT (k) DO UPDATE SET v = excluded.v";
    let write = |c: &mut Client, i: usize| {
        let mut tx = c.transaction().unwrap();
        let st = tx.prepare(upsert).unwrap();
        for j in 0..ROWS {
            tx.execute(&st, &[&format!("a{i}:{j}"), &val(j)]).unwrap();
        }
        tx.commit().unwrap();
    };
    // ponytail: capped at 25 workers — each agent holds up to 3 connections, max_connections is 100.
    let r = run(n, n.min(25), |i| {
        let mut admin = pg("postgres");
        let db = format!("chronos_a{i}");
        let t = Instant::now();
        admin.batch_execute(&format!("CREATE DATABASE {db} TEMPLATE chronos_tmpl")).unwrap();
        let fork = t.elapsed();
        write(&mut pg(&db), i);
        let t = Instant::now();
        write(&mut pg("chronos_main"), i);
        let merge = t.elapsed();
        admin.batch_execute(&format!("DROP DATABASE {db}")).unwrap();
        (fork, merge)
    });
    for d in ["chronos_main", "chronos_tmpl"] {
        admin.batch_execute(&format!("DROP DATABASE {d}")).unwrap();
    }
    Some(r)
}

fn pct(v: &[Duration], p: f64) -> String {
    let d = v[((v.len() - 1) as f64 * p).round() as usize];
    format!("{:.3}ms", d.as_secs_f64() * 1e3)
}

fn main() {
    let ns: Vec<usize> = std::env::args().skip(1).map(|a| a.parse().expect("agent counts, e.g. 10 100 1000")).collect();
    let ns = if ns.is_empty() { vec![10, 100, 1000] } else { ns };
    println!("seed rows in main: {}, rows written per agent: {ROWS}\n", seed());
    println!(
        "| system | agents | workers | fork p50 | fork p99 | merge p50 | merge p99 | wall | agent rows/s |\n|---|---|---|---|---|---|---|---|---|"
    );
    for &n in &ns {
        let only = std::env::var("ONLY").unwrap_or_default();
        let on = |s: &str| only.is_empty() || only.split(',').any(|o| o == s);
        let runs = [
            ("chronos", on("chronos").then(|| chronos(n))),
            ("chronos-disk", on("chronos-disk").then(|| chronos_disk(n))),
            ("sqlite", on("sqlite").then(|| sqlite(n))),
            ("postgres", if on("postgres") { postgres(n) } else { None }),
        ];
        for (name, r) in runs.into_iter().filter(|(name, _)| on(name)) {
            let Some(r) = r else {
                println!("| {name} | {n} | skipped: no server at {PG} |");
                continue;
            };
            println!(
                "| {name} | {n} | {} | {} | {} | {} | {} | {:.2}s | {:.0} |",
                r.workers,
                pct(&r.forks, 0.5),
                pct(&r.forks, 0.99),
                pct(&r.merges, 0.5),
                pct(&r.merges, 0.99),
                r.wall.as_secs_f64(),
                (n * ROWS) as f64 / r.wall.as_secs_f64()
            );
        }
    }
}
