//! A hash join whose other side outgrows CHRONOS_WORK_MEM goes to disk (a grace join): both sides
//! are split into files by the join key and each pair of files joined alone. On a join over
//! (w, d, id) the files and their hash tables are keyed on all three columns, not on w alone,
//! which put every row of a warehouse into one file and paired each with all of them there: at
//! TPC-C's five warehouses, CH Q3 and Q18 took 17 minutes instead of a second.

use chronos::sql::{Value, run};
use chronos::{Db, MAIN};
use std::time::{Duration, Instant};

fn count(db: &Db, q: &str) -> i64 {
    let o = run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    match &o.rows[0][0] {
        Value::Int(n) => *n,
        v => panic!("{q}: {v:?}"),
    }
}

#[test]
fn spilled_joins_key_on_every_equality() {
    static ENV: std::sync::Once = std::sync::Once::new();
    // set before anything reads it (once per process)
    ENV.call_once(|| unsafe { std::env::set_var("CHRONOS_WORK_MEM", "1MB") });
    let dir = std::env::temp_dir().join(format!("chronos-join-spill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let q = |s: &str| {
        run(&db, MAIN, s, &[]).unwrap_or_else(|e| panic!("{s}: {e}"));
    };
    q("create table customer (w int, d int, id int, pad text, primary key (w, d, id))");
    q("create table orders (w int, d int, id int, c int, primary key (w, d, id))");
    // one warehouse, two districts: 10,000 customers of 300 bytes each (3 MB, past the 1 MB budget)
    q("insert into customer select 1, d, i, repeat('x', 300) from generate_series(1, 2) d, generate_series(1, 5000) i");
    // each customer's one order, and 1,000 orders of nobody
    q("insert into orders select 1, d, i, i from generate_series(1, 2) d, generate_series(1, 5500) i");
    let t = Instant::now();
    let on = "c.w = o.w and c.d = o.d and c.id = o.c";
    assert_eq!(count(&db, &format!("select count(*) from orders o join customer c on {on}")), 10_000);
    assert_eq!(count(&db, &format!("select count(*) from orders o left join customer c on {on}")), 11_000);
    assert_eq!(
        count(&db, &format!("select count(*) from orders o left join customer c on {on} where c.id is null")),
        1_000
    );
    // keyed on w alone: 110 million pairs; on all three, well under a second
    assert!(t.elapsed() < Duration::from_secs(30), "{:?}", t.elapsed());
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
