//! Hash joins on several columns: a join on (w_id, d_id, o_id), where w_id has one value and
//! d_id ten, keys its hash table on all three, not on w_id alone (which probed every row of the
//! warehouse for each row: minutes for a TPC-C-sized join that takes milliseconds). The answers
//! are checked against a count worked out here, nulls and LEFT JOIN padding included.

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
fn joins_key_on_every_equality() {
    let db = Db::new();
    let q = |s: &str| {
        run(&db, MAIN, s, &[]).unwrap_or_else(|e| panic!("{s}: {e}"));
    };
    q("create table orders (w int, d int, id int, carrier int, primary key (w, d, id))");
    q("create table lines (w int, d int, o int, n int, delivered int, primary key (w, d, o, n))");
    // nullable copies of a key: nulls match nothing
    q("create table loose (w int, d int, o int)");
    // 10 districts x 5,000 orders, one warehouse; every 3rd order delivered (carrier set)
    q("insert into orders select 1, d, i, case when i % 3 = 0 then 1 end
       from generate_series(1, 10) d, generate_series(1, 5000) i");
    // 2 lines per order of the first 4,000 of each district, delivered as the order is
    q("insert into lines select 1, d, i, n, case when i % 3 = 0 then 1 end
       from generate_series(1, 10) d, generate_series(1, 4000) i, generate_series(1, 2) n");
    q("insert into loose select case when i % 10 = 0 then null else 1 end, d, i
       from generate_series(1, 10) d, generate_series(1, 2000) i");

    let t = Instant::now();
    let on = "o.w = l.w and o.d = l.d and o.id = l.o";
    assert_eq!(count(&db, &format!("select count(*) from lines l join orders o on {on}")), 80_000);
    assert_eq!(count(&db, &format!("select count(*) from orders o left join lines l on {on}")), 90_000);
    assert_eq!(
        count(&db, &format!("select count(*) from orders o left join lines l on {on} where l.o is null")),
        10_000,
        "orders 4001..5000 of each district have no lines"
    );
    // the consistency check BranchBench runs: a line is delivered exactly when its order is
    assert_eq!(
        count(
            &db,
            &format!(
                "select count(*) from lines l join orders o on {on} \
                 where (l.delivered is null and o.carrier is not null) or (l.delivered is not null and o.carrier is null)"
            )
        ),
        0
    );
    // the same columns the other way round, and in another order
    assert_eq!(
        count(&db, "select count(*) from orders o join lines l on l.o = o.id and l.w = o.w and l.d = o.d"),
        80_000
    );
    // a null in any key column matches nothing: 1 in 10 rows of `loose` has a null w
    assert_eq!(
        count(&db, "select count(*) from loose x join orders o on o.w = x.w and o.d = x.d and o.id = x.o"),
        18_000
    );
    assert_eq!(
        count(&db, "select count(*) from loose x left join orders o on o.w = x.w and o.d = x.d and o.id = x.o"),
        20_000
    );
    // keyed on w alone, these took minutes; all of them together, well under a second
    assert!(t.elapsed() < Duration::from_secs(30), "{:?}", t.elapsed());
}

/// TPC-C's CH Q3 shape: `customer JOIN new_order ON (w, d) JOIN orders ON (w, d, o) AND
/// o.c = c.id`. Taken in the order written, every new_order row of a district meets every
/// customer of it (millions of rows held at once); the equalities the ON clauses imply
/// (`o.w = c.w`, `o.d = c.d`) let the join take orders, linked to customer by three columns,
/// before new_order. Implied equalities hold only between columns of one type and collation.
#[test]
fn implied_equalities_choose_the_join_order() {
    let db = Db::new();
    let q = |s: &str| {
        run(&db, MAIN, s, &[]).unwrap_or_else(|e| panic!("{s}: {e}"));
    };
    q("create table customer (w int, d int, id int, state text, primary key (w, d, id))");
    q("create table new_order (w int, d int, o int, primary key (w, d, o))");
    q("create table orders (w int, d int, id int, c int, primary key (w, d, id))");
    // one district: 3,000 customers, 3,000 orders (order i by customer i), 2,000 of them new
    q("insert into customer select 1, 1, i, 'A' from generate_series(1, 3000) i");
    q("insert into orders select 1, 1, i, i from generate_series(1, 3000) i");
    q("insert into new_order select 1, 1, i from generate_series(1, 2000) i");
    let t = Instant::now();
    let q3 = "select count(*) from customer c join new_order n on n.w = c.w and n.d = c.d \
              join orders o on o.w = n.w and o.d = n.d and o.id = n.o and o.c = c.id where c.state >= 'A'";
    assert_eq!(count(&db, q3), 2_000);
    // written the other way round: the same answer
    assert_eq!(
        count(
            &db,
            "select count(*) from orders o join new_order n on o.w = n.w and o.d = n.d and o.id = n.o \
             join customer c on o.c = c.id and n.w = c.w and n.d = c.d"
        ),
        2_000
    );
    // 6 million rows held in the order written; a fraction of a second taken right
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}
