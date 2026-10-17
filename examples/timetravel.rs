//! What time travel costs: rebuilding a past moment (first read), reading it again, a query AS OF
//! against the same query now, and the disk history takes.
//!
//!   BATCHES=2000 cargo run --release --example timetravel
use chronos::{Db, MAIN};
use std::time::Instant;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn main() {
    let batches: usize = std::env::var("BATCHES").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    let dir = std::env::temp_dir().join(format!("chronos-timetravel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let sql = |q: &str| chronos::sql::run(&db, MAIN, q, &[]).unwrap();
    sql("create table t (id bigint primary key, n int, note text)");
    let mut marks = vec![];
    let t = Instant::now();
    for b in 0..batches {
        // 100 rows per statement: new rows, and updates of old ones
        let v: Vec<String> =
            (0..100).map(|i| format!("({}, {b}, 'batch {b}')", (b * 100 + i) % (batches * 50))).collect();
        sql(&format!(
            "insert into t values {} on conflict (id) do update set n = excluded.n, note = excluded.note",
            v.join(",")
        ));
        if b % (batches / 4).max(1) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(2));
            marks.push(chronos::sql::moment("now").unwrap());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    println!(
        "{batches} statements of 100 rows: {:.0} ms ({:.0} rows/s)",
        ms(t),
        batches as f64 * 100.0 / t.elapsed().as_secs_f64()
    );
    for (i, &m) in marks.iter().enumerate() {
        let t = Instant::now();
        let v = db.view_at(MAIN, m).unwrap();
        let first = ms(t);
        let t = Instant::now();
        let _ = db.view_at(MAIN, m).unwrap();
        let again = ms(t);
        drop(v);
        println!("moment {i}: first read {first:.1} ms (rebuilt from the log), again {again:.3} ms");
    }
    // a fixed moment (a time ago would be a new moment on every run, rebuilt each time)
    let at = chronos::sql::run(&db, MAIN, "select (now() - interval '1 millisecond')::text", &[]).unwrap();
    let at = at.last().unwrap().rows[0][0].to_text();
    let q_now = "select count(*), sum(n) from t";
    let q_then = format!("select count(*), sum(n) from t as of '{at}'");
    for q in [q_now, q_then.as_str()] {
        sql(q);
        let t = Instant::now();
        for _ in 0..10 {
            sql(q);
        }
        println!("{:>8.2} ms  {q}", ms(t) / 10.0);
    }
    db.checkpoint().unwrap();
    let size = |p: &std::path::Path| {
        std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| e.metadata().map_or(0, |m| m.len())).sum::<u64>())
            .unwrap_or(0)
    };
    let wal = size(&dir);
    let hist = size(&dir.join("history"));
    let pages = size(&dir.join("pages"));
    println!(
        "on disk: pages {:.1} MB, log kept for history {:.1} MB, history manifests {:.1} KB",
        pages as f64 / 1e6,
        wal as f64 / 1e6,
        hist as f64 / 1e3
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
