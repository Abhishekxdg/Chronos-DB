//! What checking a world's reads costs (`WITH (check_reads = true)`, see src/stale.rs): the same agent work in a
//! world that records what it reads and in one that doesn't, on ROWS rows. Three ways of reading:
//!
//!   keys    KEYS point lookups by primary key (under the 10,000-key cap a table's keys are kept one by one)
//!   many    ROWS point lookups (past the cap: the table counts as read whole)
//!   filters SCANS filtered scans (`where region = ...`), each kept as its filter, not its rows
//!
//! Then main changes CHANGED rows the world didn't read, the world writes one row, and it merges: the merge's
//! time, and whether the check held it. Reads are timed per statement, through a SQL session, as an agent's are.
//!
//!   ROWS=100000 KEYS=9000 SCANS=1000 CHANGED=1000 cargo run --release --example readsets
use chronos::sql::Session;
use chronos::{Db, MAIN};
use std::time::Instant;

fn env(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// Regions rows are spread over; scans read the first half, main changes rows of the second.
const REGIONS: usize = 50;

struct Run {
    reads: usize,
    read_s: f64,
    merge_ms: f64,
    held: Option<String>,
}

fn run(rows: usize, changed: usize, check: bool, how: &str, n: usize) -> Run {
    let dir = std::env::temp_dir().join(format!("chronos-readsets-{}-{how}-{check}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let sql = |q: &str| chronos::sql::run(&db, MAIN, q, &[]).unwrap();
    sql("create table t (id bigint primary key, region text not null, v int not null)");
    for from in (0..rows).step_by(10_000) {
        let v: Vec<String> =
            (from..(from + 10_000).min(rows)).map(|i| format!("({i}, 'R{}', 0)", i % REGIONS)).collect();
        sql(&format!("insert into t values {}", v.join(",")));
    }
    let with = if check { " with (check_reads = true)" } else { "" };
    sql(&format!("create world w{with}"));
    let mut s = Session::new("w");
    // the agent's reads
    let t = Instant::now();
    let reads = match how {
        "filters" => {
            for i in 0..n {
                s.run(&db, &format!("select count(*) from t where region = 'R{}'", i % (REGIONS / 2)), &[]).unwrap();
            }
            n
        }
        _ => {
            for i in 0..n {
                s.run(&db, &format!("select v from t where id = {}", i % rows), &[]).unwrap();
            }
            n
        }
    };
    let read_s = t.elapsed().as_secs_f64();
    // meanwhile, main changes rows the world didn't read: the last CHANGED ids in the regions scans don't read
    let last: Vec<String> = (0..rows)
        .rev()
        .filter(|i| i % REGIONS >= REGIONS / 2 && (how != "keys" || *i >= n))
        .take(changed)
        .map(|i| i.to_string())
        .collect();
    sql(&format!("update t set v = v + 1 where id in ({})", last.join(",")));
    // the agent's decision, one row it read, then its merge
    s.run(&db, "update t set v = 42 where id = 0", &[]).unwrap();
    drop(s);
    let t = Instant::now();
    let held = chronos::sql::run(&db, MAIN, "merge world w", &[]).err().map(|e| e.message);
    let merge_ms = t.elapsed().as_secs_f64() * 1000.0;
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Run { reads, read_s, merge_ms, held }
}

fn main() {
    let (rows, keys, scans, changed) =
        (env("ROWS", 100_000), env("KEYS", 9_000), env("SCANS", 1_000), env("CHANGED", 1_000));
    println!("{rows} rows; main changes {changed} rows the world didn't read before it merges");
    println!("| reads | check_reads | per read | all reads | merge | held |");
    println!("|---|---|---:|---:|---:|---|");
    for (how, n, label) in [
        ("keys", keys, format!("{keys} point lookups")),
        ("many", rows, format!("{rows} point lookups")),
        ("filters", scans, format!("{scans} filtered scans")),
    ] {
        for check in [false, true] {
            let r = run(rows, changed, check, how, n);
            let held = match &r.held {
                None => "no".to_string(),
                Some(m) => format!("yes: {}", m.chars().take(90).collect::<String>()),
            };
            println!(
                "| {label} | {} | {:.1} µs | {:.2} s | {:.1} ms | {held} |",
                if check { "on" } else { "off" },
                r.read_s * 1e6 / r.reads as f64,
                r.read_s,
                r.merge_ms
            );
        }
    }
}
