//! Queries whose working state outgrows CHRONOS_WORK_MEM spill to disk and give the same answers.
//! This file's process runs with a 1 MB budget, so modest tables already spill.

use chronos::Db;
use chronos::sql::{Output, run};

fn sql(db: &Db, q: &str) -> Output {
    run(db, chronos::MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap()
}

fn rows(o: &Output) -> Vec<String> {
    o.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")).collect()
}

fn spilled(db: &Db) -> u64 {
    db.counters().into_iter().find(|(n, _)| *n == "spilled_bytes").map_or(0, |(_, v)| v)
}

fn setup() -> Db {
    // set before anything reads it (it's read once per process)
    unsafe { std::env::set_var("CHRONOS_WORK_MEM", "1MB") };
    let db = Db::new();
    sql(&db, "create table t (id int primary key, g int, f float, s text)");
    sql(&db, "insert into t select i, (i * 7) % 50000, i / 4.0, 'r' || i from generate_series(1, 200000) i");
    db
}

#[test]
fn group_by_spills_and_answers_the_same() {
    let db = setup();
    let before = spilled(&db);
    let o = sql(&db, "select g, count(*), sum(id), min(s), max(f), string_agg(id::text, ',') from t group by g");
    assert!(spilled(&db) > before, "50,000 groups don't fit in 1 MB: it spilled");
    assert_eq!(o.rows.len(), 50_000);
    // groups in the order they first appear, rows in scan order (integer keys in number order)
    let ids: Vec<i64> = (1..=200_000).collect();
    let mut order: Vec<i64> = vec![];
    let mut groups: std::collections::HashMap<i64, Vec<i64>> = Default::default();
    for id in ids {
        let g = id * 7 % 50_000;
        let e = groups.entry(g).or_default();
        if e.is_empty() {
            order.push(g);
        }
        e.push(id);
    }
    let want: Vec<String> = order
        .iter()
        .map(|g| {
            let ids = &groups[g];
            let min_s = ids.iter().map(|i| format!("r{i}")).min().unwrap();
            let max_f = *ids.iter().max().unwrap() as f64 / 4.0;
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            format!("{g}|{}|{}|{min_s}|{max_f}|{list}", ids.len(), ids.iter().sum::<i64>())
        })
        .collect();
    assert_eq!(rows(&o), want);
    // aggregates over all of it, and HAVING, ORDER BY and LIMIT on spilled groups
    assert_eq!(rows(&sql(&db, "select count(*) from (select g from t group by g) x")), ["50000"]);
    let mut sums = std::collections::HashMap::<i64, i64>::new();
    for id in 1..=200_000i64 {
        *sums.entry(id * 7 % 50_000).or_default() += id;
    }
    let mut want: Vec<(i64, i64)> = sums.into_iter().filter(|(_, s)| *s > 400_000).collect();
    want.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let want: Vec<String> = want.iter().take(3).map(|(g, s)| format!("{g}|{s}")).collect();
    let o = sql(&db, "select g, sum(id) from t group by g having sum(id) > 400000 order by sum(id) desc, g limit 3");
    assert_eq!(rows(&o), want);
}

#[test]
fn order_by_spills_and_answers_the_same() {
    let db = setup();
    let before = spilled(&db);
    // g repeats: ties come out in scan order (integer keys in number order), as without spilling
    let o = sql(&db, "select id, g from t order by g desc");
    assert!(spilled(&db) > before, "200,000 rows don't fit in 1 MB: it spilled");
    let ids: Vec<i64> = (1..=200_000).collect();
    let mut want: Vec<(i64, i64)> = ids.into_iter().map(|i| (i, i * 7 % 50_000)).collect();
    want.sort_by_key(|w| std::cmp::Reverse(w.1)); // stable: scan order within a g
    let want: Vec<String> = want.iter().map(|(i, g)| format!("{i}|{g}")).collect();
    assert_eq!(rows(&o), want);
    // a LIMIT holds only its best rows; OFFSET, DISTINCT and text keys too
    assert_eq!(
        rows(&sql(&db, "select id from t order by f desc, id limit 3 offset 1")),
        ["199999", "199998", "199997"]
    );
    assert_eq!(rows(&sql(&db, "select distinct g from t order by g limit 3 offset 2")), ["2", "3", "4"]);
    assert_eq!(rows(&sql(&db, "select s from t order by s limit 2")), ["r1", "r10"]);
    assert_eq!(rows(&sql(&db, "select count(*) from (select distinct g from t order by g) x")), ["50000"]);
}

#[test]
fn joins_spill_and_answer_the_same() {
    let db = setup();
    sql(&db, "create table a (id int primary key, k int)");
    sql(&db, "insert into a select i, i % 5000 from generate_series(1, 20000) i");
    sql(&db, "create table b (id int primary key, k int, s text)");
    sql(&db, "insert into b select i, i % 3000, 'b' || i from generate_series(1, 100000) i");
    // scan order is key order: integer keys in number order
    let order = |n: i64| (1..=n).collect::<Vec<i64>>();
    let (a, b) = (order(20_000), order(100_000));
    let mut by_k: std::collections::HashMap<i64, Vec<i64>> = Default::default();
    for &id in &b {
        by_k.entry(id % 3000).or_default().push(id);
    }
    let before = spilled(&db);
    // the join's own order: the first table's, then each row's matches in the other's
    let o = sql(&db, "select a.id, b.id from a join b on a.k = b.k");
    assert!(spilled(&db) > before, "b doesn't fit in 1 MB: the join went to disk");
    let want: Vec<String> =
        a.iter().flat_map(|&x| by_k.get(&(x % 5000)).into_iter().flatten().map(move |y| format!("{x}|{y}"))).collect();
    assert_eq!(rows(&o).len(), want.len());
    assert_eq!(rows(&o), want);
    // LEFT JOIN keeps a's rows with no match, and WHERE applies after it
    let o = sql(&db, "select a.id, b.s from a left join b on a.k = b.k where a.id % 1000 = 0");
    let want: Vec<String> = a
        .iter()
        .filter(|x| *x % 1000 == 0)
        .flat_map(|&x| match by_k.get(&(x % 5000)) {
            Some(ys) => ys.iter().map(|y| format!("{x}|b{y}")).collect::<Vec<_>>(),
            None => vec![format!("{x}|")], // null prints as nothing here
        })
        .collect();
    assert_eq!(rows(&o), want);
    // GROUP BY over it, grouped file by file
    let o = sql(&db, "select a.k % 7, count(*), sum(b.id) from a join b on a.k = b.k group by 1");
    let mut groups: Vec<(i64, i64, i64)> = vec![];
    for &x in &a {
        for &y in by_k.get(&(x % 5000)).into_iter().flatten() {
            let g = (x % 5000) % 7;
            match groups.iter_mut().find(|e| e.0 == g) {
                Some(e) => {
                    e.1 += 1;
                    e.2 += y;
                }
                None => groups.push((g, 1, y)),
            }
        }
    }
    assert_eq!(rows(&o), groups.iter().map(|(g, n, s)| format!("{g}|{n}|{s}")).collect::<Vec<_>>());
    // a later step: the rows so far, joined to a third table too big for memory
    sql(&db, "create table c (id int primary key, v text)");
    sql(&db, "insert into c select i, 'c' || i || repeat('x', 40) from generate_series(1, 50000) i");
    let before = spilled(&db);
    let o = sql(&db, "select a.id, b.id, c.v from a join b on a.k = b.k join c on c.id = b.id where a.id <= 2000");
    assert!(spilled(&db) > before, "c doesn't fit in 1 MB: that step went to disk");
    let want: Vec<String> = a
        .iter()
        .filter(|x| **x <= 2000)
        .flat_map(|&x| {
            by_k.get(&(x % 5000))
                .into_iter()
                .flatten()
                .filter(|y| **y <= 50_000)
                .map(move |y| format!("{x}|{y}|c{y}{}", "x".repeat(40)))
        })
        .collect();
    assert_eq!(rows(&o), want);
    // only that later step too big: b2 fits, the rows it gives are many, c doesn't fit
    sql(&db, "create table b2 (id int primary key, k int)");
    sql(&db, "insert into b2 select i, i - 1 from generate_series(1, 1000) i");
    let before = spilled(&db);
    let o = sql(&db, "select count(*), sum(c.id) from a join b2 on a.k = b2.k join c on c.id = a.id");
    assert!(spilled(&db) > before, "the step joining c went to disk");
    let ids: Vec<i64> = (1..=20_000).filter(|i| i % 5000 < 1000).collect();
    assert_eq!(rows(&o), [format!("{}|{}", ids.len(), ids.iter().sum::<i64>())]);
}

#[test]
fn distinct_aggregates_spill_and_answer_the_same() {
    use std::collections::{HashMap, HashSet};
    let db = setup();
    let ids = 1..=200_000i64;
    let g = |i: i64| i * 7 % 50_000;
    // one argument for every DISTINCT: rows split by it too, so one big group spreads over files
    let before = spilled(&db);
    let o = sql(&db, "select count(distinct s), min(distinct s), max(distinct s), count(*) from t");
    assert!(spilled(&db) > before, "200,000 distinct values don't fit in 1 MB: it spilled");
    assert_eq!(rows(&o), ["200000|r1|r99999|200000"]);
    let gs: HashSet<i64> = ids.clone().map(g).collect();
    let (sum, n) = (gs.iter().sum::<i64>(), gs.len() as f64);
    let o = sql(&db, "select count(distinct g), sum(distinct g), round(avg(distinct g), 6), min(g), max(g) from t");
    assert_eq!(rows(&o), [format!("{}|{sum}|{:.6}|0|49999", gs.len(), sum as f64 / n)]);
    // nulls aren't counted
    let o = sql(&db, "select count(distinct case when id % 3 = 0 then null else g end) from t");
    let want: HashSet<i64> = ids.clone().filter(|i| i % 3 != 0).map(g).collect();
    assert_eq!(rows(&o), [want.len().to_string()]);
    // grouped: each group's distinct values, groups in key order (as Postgres's GroupAggregate gives them for a
    // DISTINCT aggregate, its rows sorted by group)
    let o = sql(&db, "select id % 10, count(distinct g), sum(distinct g), count(*) from t group by id % 10");
    let keys: Vec<i64> = ids.clone().collect(); // the scan order: integer keys in number order
    let mut order: Vec<i64> = vec![];
    let mut by: HashMap<i64, (HashSet<i64>, i64)> = HashMap::new(); // group: its g values, its rows
    for i in keys {
        let e = by.entry(i % 10).or_insert_with(|| {
            order.push(i % 10);
            (HashSet::new(), 0)
        });
        e.0.insert(g(i));
        e.1 += 1;
    }
    order.sort();
    let want: Vec<String> =
        order.iter().map(|b| format!("{b}|{}|{}|{}", by[b].0.len(), by[b].0.iter().sum::<i64>(), by[b].1)).collect();
    assert_eq!(rows(&o), want);
    // different DISTINCT arguments, and a list aggregate: rows split by group only, answers the same
    let o = sql(&db, "select count(distinct g), count(distinct s) from t");
    assert_eq!(rows(&o), [format!("{}|200000", gs.len())]);
    let o =
        sql(&db, "select string_agg(distinct (id % 7)::text, ',' order by (id % 7)::text), count(distinct s) from t");
    assert_eq!(rows(&o), ["0,1,2,3,4,5,6|200000"]);
}
