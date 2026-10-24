//! The same SQL workload over the Postgres protocol against Chronos and Postgres.
//!
//!   CHRONOS_PROTECT_MAIN=off chronos serve /tmp/sqlbench &   # it loads main directly               (Postgres protocol on 127.0.0.1:5433)
//!   N=10000 cargo run --release --example sql
//!
//! Everything goes over TCP. CHRONOS and PG override the connection strings; PG_FULL adds a Postgres
//! started with wal_sync_method=fsync_writethrough, which on macOS flushes the drive's cache on
//! commit as Chronos does (Postgres's default there doesn't, so a power cut can lose commits).

use postgres::{Client, NoTls};
use std::time::Instant;

fn pct(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p) as usize]
}

/// Runs `f` n times; returns per-call microseconds.
fn timed(n: usize, mut f: impl FnMut(usize)) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let t = Instant::now();
            f(i);
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect()
}

fn run(name: &str, url: &str, n: usize) {
    let mut c = Client::connect(url, NoTls).unwrap_or_else(|e| panic!("{name}: {e}"));
    let _ = c.batch_execute("drop table if exists sql_bench");
    c.batch_execute("create table sql_bench (id bigint primary key, name text not null, age integer)").unwrap();

    let ins = c.prepare("insert into sql_bench (id, name, age) values ($1, $2, $3)").unwrap();
    let t = Instant::now();
    for i in 0..n as i64 {
        c.execute(&ins, &[&i, &format!("user {i}"), &((i % 90) as i32)]).unwrap();
    }
    let one_by_one = n as f64 / t.elapsed().as_secs_f64();

    let t = Instant::now();
    for chunk in (n as i64..2 * n as i64).collect::<Vec<_>>().chunks(1000) {
        let values: Vec<String> = chunk.iter().map(|i| format!("({i}, 'user {i}', {})", i % 90)).collect();
        c.batch_execute(&format!("insert into sql_bench (id, name, age) values {}", values.join(","))).unwrap();
    }
    let batched = n as f64 / t.elapsed().as_secs_f64();

    let get = c.prepare("select name, age from sql_bench where id = $1").unwrap();
    let lookups = timed(n, |i| {
        c.query_one(&get, &[&((i * 7919 % (2 * n)) as i64)]).unwrap();
    });
    let upd = c.prepare("update sql_bench set age = age + 1 where id = $1").unwrap();
    let updates = timed(n / 10, |i| {
        c.execute(&upd, &[&(i as i64)]).unwrap();
    });
    let scan = c.prepare("select id from sql_bench where age = $1 and name like $2").unwrap();
    let scans = timed(50, |i| {
        c.query(&scan, &[&((i % 90) as i32), &"user 1%"]).unwrap();
    });

    // a second table to join: orders pointing at sql_bench rows
    let _ = c.batch_execute("drop table if exists sql_orders");
    c.batch_execute("create table sql_orders (id bigint primary key, ref bigint not null, amount integer)").unwrap();
    for chunk in (0..2 * n as i64).collect::<Vec<_>>().chunks(1000) {
        let values: Vec<String> =
            chunk.iter().map(|i| format!("({i}, {}, {})", i * 31 % (2 * n as i64), i % 1000)).collect();
        c.batch_execute(&format!("insert into sql_orders values {}", values.join(","))).unwrap();
    }
    let jk = c
        .prepare("select b.name, o.amount from sql_orders o join sql_bench b on b.id = o.ref where o.id = $1")
        .unwrap();
    let joins_by_key = timed(n / 10, |i| {
        c.query_one(&jk, &[&((i * 7919 % (2 * n)) as i64)]).unwrap();
    });
    let hj = c.prepare("select count(*) from sql_orders o join sql_bench b on b.id = o.ref where b.age = $1").unwrap();
    let hash_joins = timed(20, |i| {
        c.query_one(&hj, &[&((i % 90) as i32)]).unwrap();
    });
    let grp = c.prepare("select age, count(*), max(name) from sql_bench group by age order by age").unwrap();
    let groups = timed(20, |_| {
        c.query(&grp, &[]).unwrap();
    });
    let hjg = c
        .prepare("select b.age, sum(o.amount) from sql_orders o join sql_bench b on b.id = o.ref group by b.age")
        .unwrap();
    let join_groups = timed(10, |_| {
        c.query(&hjg, &[]).unwrap();
    });
    c.batch_execute("drop table sql_bench; drop table sql_orders").unwrap();

    println!(
        "{name:<8}   join by key p50 {:.0}µs  filtered hash join p50 {:.1}ms  group by p50 {:.1}ms  join+group p50 {:.1}ms",
        pct(joins_by_key, 0.5),
        pct(hash_joins, 0.5) / 1000.0,
        pct(groups, 0.5) / 1000.0,
        pct(join_groups, 0.5) / 1000.0,
    );
    println!(
        "{name:<8} {:>12.0} {:>12.0} {:>10.0} {:>10.0} {:>10.0} {:>10.0}",
        one_by_one,
        batched,
        pct(lookups.clone(), 0.5),
        pct(lookups, 0.99),
        pct(updates, 0.5),
        pct(scans, 0.5) / 1000.0,
    );
}

fn main() {
    let n: usize = std::env::var("N").ok().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let chronos = std::env::var("CHRONOS").unwrap_or("host=127.0.0.1 port=5433 user=bench dbname=main".into());
    let pg = std::env::var("PG").unwrap_or("host=127.0.0.1 port=5432 dbname=postgres".into());
    println!("{n} rows, then {n} more in batches of 1000; lookups by key; updates by key; scans of {} rows", 2 * n);
    println!(
        "{:<8} {:>12} {:>12} {:>10} {:>10} {:>10} {:>10}",
        "", "insert/s", "batch row/s", "get p50µs", "get p99µs", "upd p50µs", "scan p50ms"
    );
    run("chronos", &chronos, n);
    run("postgres", &pg, n);
    if let Ok(full) = std::env::var("PG_FULL") {
        run("pg-flush", &full, n);
    }
}
