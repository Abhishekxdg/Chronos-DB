//! A join's last table, then GROUP BY: each joined row is grouped as it is made instead of every joined row being
//! kept first (each as wide as all the tables together: CH Q18's 300,000 rows of 43 values were half its time).
//! The groups, their order, their first rows and their aggregates must come out as they did: each query here is
//! run beside a twin whose sum isn't of a bare column, which still keeps the joined rows and groups them after.

use chronos::sql::{Value, run};
use chronos::{Db, MAIN};

fn rows(db: &Db, q: &str) -> Vec<Vec<Value>> {
    run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap().rows
}

fn db(name: &str) -> Db {
    let dir = std::env::temp_dir().join(format!("chronos-join-group-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let q = |s: &str| {
        run(&db, MAIN, s, &[]).unwrap_or_else(|e| panic!("{s}: {e}"));
    };
    q("create table cust (w int, d int, id int, name text, pad text, primary key (w, d, id))");
    q("create table ord (w int, d int, id int, c int, tag int, primary key (w, d, id))");
    q("create table line (w int, d int, o int, n int, amount numeric(10, 2), qty int, primary key (w, d, o, n))");
    // 10,000 customers (enough for a join to split across cores), names of several lengths shared by many
    q("insert into cust select 1, d, i, repeat('n', i % 7) || (i % 700), repeat('x', 50) \
       from generate_series(1, 2) d, generate_series(1, 5000) i");
    // 12,000 orders: one in 11 of nobody (null), some of customers that don't exist (5001 to 5500)
    q("insert into ord select 1, d, i, case when i % 11 = 0 then null else i * 7 % 5500 + 1 end, i % 5 \
       from generate_series(1, 2) d, generate_series(1, 6000) i");
    // three lines an order, amounts with cents
    q("insert into line select 1, d, o, n, cast((o * 3 + n) / 4.0 as numeric(10, 2)), n \
       from generate_series(1, 2) d, generate_series(1, 6000) o, generate_series(1, 3) n");
    db
}

/// `q` with `{sum}` as a sum of the bare column (grouped while joining) and as a sum of an expression (grouped
/// after): the same rows in the same order.
fn same_both_ways(db: &Db, q: &str, col: &str) -> Vec<Vec<Value>> {
    let folded = rows(db, &q.replace("{sum}", &format!("sum({col})")));
    let kept = rows(db, &q.replace("{sum}", &format!("sum({col} + 0)")));
    assert!(!folded.is_empty(), "{q}: no rows");
    assert_eq!(folded, kept, "{q}");
    folded
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        v => panic!("{v:?}"),
    }
}

#[test]
fn three_tables_grouped_as_the_last_is_joined() {
    let db = db("three");
    let on = "from cust c join ord o on o.w = c.w and o.d = c.d and o.c = c.id \
              join line l on l.w = o.w and l.d = o.d and l.o = o.id";
    // groups of text and numbers, in the order they first appear (no ORDER BY)
    let out = same_both_ways(
        &db,
        &format!("select c.name, o.id, {{sum}}, count(*), min(l.qty), max(c.pad) {on} group by c.name, o.id"),
        "l.amount",
    );
    // every order of a customer that exists, three lines each
    let orders = (1..=6000).filter(|i| i % 11 != 0 && i * 7 % 5500 < 5000).count() as i64;
    assert_eq!(out.iter().map(|r| int(&r[3])).sum::<i64>(), 2 * orders * 3);
    assert!(out.iter().all(|r| int(&r[4]) == 1));
    // a WHERE over two tables, HAVING, ORDER BY and LIMIT
    same_both_ways(
        &db,
        &format!(
            "select c.name, {{sum}} as total, avg(l.amount) {on} where o.tag <> 3 and l.qty + c.id > 10 \
             group by c.name having count(*) > 2 order by total desc, c.name limit 50"
        ),
        "l.amount",
    );
    // one group, and no rows at all
    same_both_ways(&db, &format!("select {{sum}}, count(*) {on}"), "l.amount");
    let none = rows(&db, &format!("select sum(l.amount), count(*) {on} where o.tag = 9"));
    assert_eq!(none, vec![vec![Value::Null, Value::Int(0)]]);
    assert!(rows(&db, &format!("select o.tag, sum(l.amount) {on} where o.tag = 9 group by o.tag")).is_empty());
}

#[test]
fn two_tables_grouped_as_they_are_joined() {
    let db = db("two");
    // a LEFT join keeps the orders of nobody; the WHERE reads the padded side
    let out = same_both_ways(
        &db,
        "select o.tag, count(*), {sum}, count(c.id) from ord o \
         left join cust c on c.w = o.w and c.d = o.d and c.id = o.c \
         where c.name is null or c.id % 2 = 0 group by o.tag",
        "c.id",
    );
    let unmatched = (1..=6000).filter(|i| i % 11 == 0 || i * 7 % 5500 >= 5000).count() as i64;
    let even = (1..=6000).filter(|i| i % 11 != 0 && i * 7 % 5500 < 5000 && (i * 7 % 5500 + 1) % 2 == 0).count() as i64;
    assert_eq!(out.iter().map(|r| int(&r[1])).sum::<i64>(), 2 * (unmatched + even));
    assert_eq!(out.iter().map(|r| int(&r[3])).sum::<i64>(), 2 * even);
    // an inner join on text and a number together, numeric sums
    same_both_ways(
        &db,
        "select c.name, {sum}, count(*) from line l join cust c on c.w = l.w and c.d = l.d and c.id = l.o \
         group by c.name",
        "l.amount",
    );
}
