//! CH-benCHmark Q3 and Q18 (as BranchBench's spine runs them) over one TPC-C warehouse: Chronos in
//! process, on disk, and the same tables and queries in Postgres over local TCP.
//!
//!   cargo run --release --example ch
//!
//! The rows are made here with generate_series, the same statements on both sides: 30,000
//! customers, 30,000 orders, 9,000 new orders and about 300,000 order lines, at TPC-C's widths
//! (a customer row is about 600 bytes). They aren't chBenchmark's own generator's rows, so times
//! compare within a run of this example, not with BranchBench's.
//!
//! PG overrides the Postgres connection string; PG=off skips it. CHRONOS_THREADS=1 shows Chronos on
//! one core; LOOPQ=q3 or q18 runs one query forever, for profilers. Each query's answer is printed
//! as a hash, to compare two builds.
use chronos::{Db, MAIN};
use postgres::{Client, NoTls};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Instant;

const SCHEMA: [&str; 4] = [
    "create table customer (c_id int not null, c_d_id smallint not null, c_w_id smallint not null, \
     c_first varchar(16), c_middle char(2), c_last varchar(16), c_street_1 varchar(20), \
     c_street_2 varchar(20), c_city varchar(20), c_state char(2), c_zip char(9), c_phone char(16), \
     c_since timestamp, c_credit char(2), c_credit_lim decimal(12, 2), c_discount decimal(4, 4), \
     c_balance decimal(12, 2), c_ytd_payment float, c_payment_cnt smallint, c_delivery_cnt smallint, \
     c_data varchar(500), c_n_nationkey integer, primary key (c_w_id, c_d_id, c_id))",
    "create table orders (o_id int not null, o_d_id smallint not null, o_w_id smallint not null, \
     o_c_id int, o_entry_d timestamp, o_carrier_id smallint, o_ol_cnt smallint, o_all_local smallint, \
     primary key (o_w_id, o_d_id, o_id))",
    "create table new_order (no_o_id int not null, no_d_id smallint not null, no_w_id smallint not null, \
     primary key (no_w_id, no_d_id, no_o_id))",
    "create table order_line (ol_o_id int not null, ol_d_id smallint not null, ol_w_id smallint not null, \
     ol_number smallint not null, ol_i_id int, ol_supply_w_id smallint, ol_delivery_d timestamp, \
     ol_quantity smallint, ol_amount decimal(6, 2), ol_dist_info char(24), \
     primary key (ol_w_id, ol_d_id, ol_o_id, ol_number))",
];

const QUERIES: [(&str, &str); 2] = [
    (
        "q3",
        "select ol.ol_o_id, ol.ol_w_id, ol.ol_d_id, sum(ol.ol_amount) as revenue, o.o_entry_d \
         from customer c join new_order no on no.no_w_id = c.c_w_id and no.no_d_id = c.c_d_id \
         join orders o on o.o_w_id = no.no_w_id and o.o_d_id = no.no_d_id and o.o_id = no.no_o_id \
         and o.o_c_id = c.c_id \
         join order_line ol on ol.ol_w_id = o.o_w_id and ol.ol_d_id = o.o_d_id and ol.ol_o_id = o.o_id \
         where c.c_state >= 'A' and o.o_entry_d > timestamp '2026-10-25 00:00:00' \
         group by ol.ol_o_id, ol.ol_w_id, ol.ol_d_id, o.o_entry_d \
         order by revenue desc, o.o_entry_d limit 100",
    ),
    (
        "q18",
        "select c.c_last, c.c_id, o.o_id, o.o_entry_d, o.o_ol_cnt, sum(ol.ol_amount) as total \
         from customer c join orders o on o.o_w_id = c.c_w_id and o.o_d_id = c.c_d_id and o.o_c_id = c.c_id \
         join order_line ol on ol.ol_w_id = o.o_w_id and ol.ol_d_id = o.o_d_id and ol.ol_o_id = o.o_id \
         where o.o_entry_d > timestamp '2026-10-25 00:00:00' \
         group by c.c_last, c.c_id, o.o_id, o.o_entry_d, o.o_ol_cnt \
         having sum(ol.ol_amount) > 200 order by total desc, o.o_entry_d limit 100",
    ),
];

/// One warehouse, a district per statement. Every order is after the queries' date, as in a
/// freshly loaded TPC-C; each customer has one order; the last 900 orders of a district are new.
fn load(mut exec: impl FnMut(&str)) {
    for s in SCHEMA {
        exec(s);
    }
    for d in 1..=10 {
        exec(&format!(
            "insert into customer select c, {d}, 1, 'first' || c, 'OE', 'LAST' || (c * 7 % 1000), \
             'street one ' || c, 'street two ' || c, 'city ' || (c % 100), \
             chr(65 + c % 26) || chr(65 + (c / 26) % 26), '123456789', '0123456789012345', \
             timestamp '2026-01-01 00:00:00', 'GC', 50000.00, 0.1000, -10.00, 10.0, 1, 0, \
             repeat('x', 300 + c % 200), c % 62 from generate_series(1, 3000) c"
        ));
        exec(&format!(
            "insert into orders select o, {d}, 1, (o * 7919 + {d}) % 3000 + 1, \
             timestamp '2026-11-01 00:00:00' + (o * 10 + {d}) * interval '1 second', \
             case when o <= 2100 then o % 10 + 1 end, 5 + (o * 13 + {d}) % 11, 1 \
             from generate_series(1, 3000) o"
        ));
        exec(&format!("insert into new_order select o, {d}, 1 from generate_series(2101, 3000) o"));
        exec(&format!(
            "insert into order_line select o, {d}, 1, n, (o * n * 37 + {d}) % 100000 + 1, 1, \
             case when o <= 2100 then timestamp '2026-11-02 00:00:00' end, 5, \
             cast(((o * 31 + n * 17 + {d} * 7) % 999999 + 1) / 100.0 as decimal(6, 2)), \
             'dist-info-dist-info-dist' \
             from generate_series(1, 3000) o, generate_series(1, 15) n where n <= 5 + (o * 13 + {d}) % 11"
        ));
    }
}

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

fn main() {
    let dir = std::env::temp_dir().join(format!("chronos-ch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let run = |q: &str| chronos::sql::run(&db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
    load(|q| {
        run(q);
    });
    db.checkpoint().unwrap();
    if let Ok(name) = std::env::var("LOOPQ") {
        let q = QUERIES.iter().find(|q| q.0 == name).expect("LOOPQ is q3 or q18").1;
        loop {
            run(q);
        }
    }

    let url = std::env::var("PG").unwrap_or("host=127.0.0.1 port=5432 dbname=postgres".into());
    let mut pg = (url != "off").then(|| Client::connect(&url, NoTls).unwrap_or_else(|e| panic!("postgres: {e}")));
    const DROP: &str = "drop table if exists order_line, new_order, orders, customer";
    if let Some(c) = &mut pg {
        c.batch_execute(DROP).unwrap();
        load(|q| c.batch_execute(q).unwrap());
        c.batch_execute("vacuum analyze").unwrap();
    }

    println!("one TPC-C warehouse; ms per query");
    println!("{:>8} {:>8}", "chronos", if pg.is_some() { "postgres" } else { "" });
    for (name, q) in QUERIES {
        let out = run(q).pop().unwrap();
        let mut h = DefaultHasher::new();
        format!("{:?}", out.rows).hash(&mut h);
        let s = time(|| {
            run(q);
        });
        let p = pg.as_mut().map(|c| {
            assert_eq!(c.query(q, &[]).unwrap().len(), out.rows.len(), "{name}: rows");
            time(|| drop(c.query(q, &[]).unwrap()))
        });
        let p = p.map_or(String::new(), |p| format!("{p:.1}"));
        println!("{s:>8.1} {p:>8}  {name}: {} rows, answer {:016x}", out.rows.len(), h.finish());
    }
    if let Some(c) = &mut pg {
        c.batch_execute(DROP).unwrap();
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
