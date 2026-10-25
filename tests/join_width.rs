//! A hash join's other side is held with only the columns the query reads after that table's own conditions: the
//! rest were nulls in every row, but each took a value's room, so a wide table went to disk (a grace join) long
//! before its rows needed to. CH Q3 joins order_line, 10 columns of which it reads 4: at five warehouses its 1.5
//! million rows went to disk at 256 MB, and the query took 3 s instead of 0.75 s.

use chronos::sql::{Value, run};
use chronos::{Db, MAIN};

fn one(db: &Db, q: &str) -> i64 {
    let o = run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    match &o.rows[0][0] {
        Value::Int(n) => *n,
        v => panic!("{q}: {v:?}"),
    }
}

fn spilled(db: &Db) -> u64 {
    db.counters().into_iter().find(|(n, _)| *n == "spilled_bytes").map_or(0, |(_, v)| v)
}

#[test]
fn a_join_holds_only_the_columns_it_reads() {
    static ENV: std::sync::Once = std::sync::Once::new();
    // set before anything reads it (once per process)
    ENV.call_once(|| unsafe { std::env::set_var("CHRONOS_WORK_MEM", "4MB") });
    let dir = std::env::temp_dir().join(format!("chronos-join-width-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let q = |s: &str| {
        run(&db, MAIN, s, &[]).unwrap_or_else(|e| panic!("{s}: {e}"));
    };
    q(
        "create table customer (w int, d int, id int, pad text, x1 int, x2 int, x3 int, x4 int, x5 int, x6 int, x7 int, \
       x8 int, primary key (w, d, id))",
    );
    q("create table orders (w int, d int, id int, c int, primary key (w, d, id))");
    // 20,000 customers (enough to be read in parts): 12 columns of 32 bytes or more each, 7.7 MB, past the 4 MB budget;
    // the 3 a count reads, 1.9 MB
    q("insert into customer select 1, d, i, repeat('x', 300), i, i, i, i, i * 5, i, i, i \
       from generate_series(1, 2) d, generate_series(1, 10000) i");
    // each customer's one order, and 2,000 orders of nobody
    q("insert into orders select 1, d, i, i from generate_series(1, 2) d, generate_series(1, 11000) i");
    let on = "c.w = o.w and c.d = o.d and c.id = o.c";

    // the keys alone, and a column in the middle: held in memory
    let before = spilled(&db);
    assert_eq!(one(&db, &format!("select count(*) from orders o join customer c on {on}")), 20_000);
    assert_eq!(one(&db, &format!("select sum(c.x5) from orders o join customer c on {on}")), 2 * 5 * 10000 * 10001 / 2);
    assert_eq!(
        one(&db, &format!("select count(*) from orders o left join customer c on {on} where c.x5 is null")),
        2_000
    );
    assert_eq!(spilled(&db), before, "nothing went to disk");

    // the wide column too: past the budget, on disk, and each column still in its place
    let wide = format!("select sum(length(c.pad) + c.x5) from orders o join customer c on {on}");
    assert_eq!(one(&db, &wide), 2 * (300 * 10000 + 5 * 10000 * 10001 / 2));
    assert!(spilled(&db) > before, "a join on disk");
    assert_eq!(
        one(&db, &format!("select count(*) from orders o left join customer c on {on} where c.pad is null")),
        2_000
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
