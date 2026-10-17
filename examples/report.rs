//! Whole-table reports (scans, GROUP BY, hash joins) on N rows per table (default 200,000):
//! Chronos in process, on disk, and the same tables and queries in Postgres over local TCP.
//!
//!   N=200000 cargo run --release --example report
//!
//! PG overrides the Postgres connection string; PG=off skips it. CHRONOS_THREADS=1 shows Chronos on
//! one core; LOOPQ="<query>" runs one query forever, for profilers.
use chronos::{Db, MAIN};
use postgres::{Client, NoTls};
use std::time::Instant;

const QUERIES: [&str; 5] = [
    "select count(*) from rpt_orders",
    "select count(*) from rpt_orders where amount > 500",
    "select age, count(*), max(name) from rpt_users group by age",
    "select b.age, sum(o.amount) from rpt_orders o join rpt_users b on b.id = o.ref group by b.age",
    "select count(*) from rpt_orders o join rpt_users b on b.id = o.ref where b.name like 'user 1%'",
];

/// Milliseconds per run of `f`, best of 3 rounds of 5 after a warm-up.
fn time(mut f: impl FnMut()) -> f64 {
    f();
    (0..3)
        .map(|_| {
            let t = Instant::now();
            (0..5).for_each(|_| f());
            t.elapsed().as_secs_f64() * 200.0
        })
        .fold(f64::MAX, f64::min)
}

fn load(n: i64, mut exec: impl FnMut(&str)) {
    exec("create table rpt_users (id bigint primary key, name text not null, age integer)");
    exec("create table rpt_orders (id bigint primary key, ref bigint not null, amount integer)");
    for c in (0..n).collect::<Vec<_>>().chunks(5000) {
        let v: Vec<String> = c.iter().map(|i| format!("({i}, 'user {i}', {})", i % 90)).collect();
        exec(&format!("insert into rpt_users values {}", v.join(",")));
        let v: Vec<String> = c.iter().map(|i| format!("({i}, {}, {})", i * 31 % n, i % 1000)).collect();
        exec(&format!("insert into rpt_orders values {}", v.join(",")));
    }
}

fn main() {
    let n: i64 = std::env::var("N").ok().and_then(|v| v.parse().ok()).unwrap_or(200_000);
    let dir = std::env::temp_dir().join(format!("chronos-report-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let run = |q: &str| chronos::sql::run(&db, MAIN, q, &[]).unwrap();
    load(n, |q| {
        run(q);
    });
    db.checkpoint().unwrap();
    if let Ok(q) = std::env::var("LOOPQ") {
        loop {
            run(&q);
        }
    }

    let url = std::env::var("PG").unwrap_or("host=127.0.0.1 port=5432 dbname=postgres".into());
    let mut pg = (url != "off").then(|| Client::connect(&url, NoTls).unwrap_or_else(|e| panic!("postgres: {e}")));
    if let Some(c) = &mut pg {
        c.batch_execute("drop table if exists rpt_orders; drop table if exists rpt_users").unwrap();
        load(n, |q| c.batch_execute(q).unwrap());
        c.batch_execute("vacuum analyze rpt_users").unwrap();
        c.batch_execute("vacuum analyze rpt_orders").unwrap();
    }

    println!("{n} rows per table; ms per query");
    println!("{:>8} {:>8}", "chronos", if pg.is_some() { "postgres" } else { "" });
    for q in QUERIES {
        let s = time(|| {
            run(q);
        });
        let p = pg.as_mut().map(|c| time(|| drop(c.query(q, &[]).unwrap())));
        println!("{s:>8.1} {:>8}  {q}", p.map_or(String::new(), |p| format!("{p:.1}")));
    }
    if let Some(c) = &mut pg {
        c.batch_execute("drop table rpt_orders; drop table rpt_users").unwrap();
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
