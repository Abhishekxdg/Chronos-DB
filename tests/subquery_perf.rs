//! Subquery speed budget at the size of the side-by-side agent demo's CRM, run by CI in release mode:
//!
//!   cargo test --release --test subquery_perf -- --ignored --nocapture
//!
//! `leads` has 150,600 rows (25,000 accounts x 6 leads, 600 duplicates), a primary key and an index
//! on `region` only, as in `demo/vm/crm_tool.py`; region R3 holds 15,000 rows, 2,500 companies x 6.
//! Before the hash semi-join (src/sql/semi.rs) a correlated EXISTS over it read R3's 15,000 rows for
//! each of R3's 15,000 rows: 91 s for the count and 97 s for the DELETE on an M2-class laptop (the
//! demo's 4-vCPU VM took about 9 minutes), and `x IN (subquery)` read the subquery's 2,500 rows
//! for each of the 150,600 outer rows (2.4 s). Since, they take 0.13 s, 0.25 s and 0.16 s. The
//! ceilings leave room for a slow CI machine and stay far below the old times: they catch a lost
//! hash, not drift.

use chronos::sql::{Value, run};
use chronos::{Db, MAIN};
use std::time::{Duration, Instant};

const ACCOUNTS: i64 = 25_000;
const DUPS: i64 = 300; // per region, R5 and R6

/// One lead, as the demo seeds it. `created_at` is a day number from 2025-06-01.
struct Lead {
    id: i64,
    account: i64,
    region: String,
    company: String,
    created: i64,
}

fn leads() -> Vec<Lead> {
    let mut out = vec![];
    let mut x: u64 = 42;
    let mut next = |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n) as i64
    };
    for account in 1..=ACCOUNTS {
        for _ in 0..6 {
            let id = out.len() as i64 + 1;
            out.push(Lead {
                id,
                account,
                region: format!("R{}", account % 10),
                company: format!("Acme-{account}"),
                created: next(301) - 200,
            });
        }
    }
    for region in ["R5", "R6"] {
        let pool: Vec<usize> = (0..out.len()).filter(|&i| out[i].region == region).collect();
        for k in 0..DUPS {
            let o = &out[pool[(k * 7 + 3) as usize % pool.len()]];
            let id = out.len() as i64 + 1;
            let dup = Lead {
                id,
                account: o.account,
                region: o.region.clone(),
                company: o.company.clone(),
                created: o.created - 30,
            };
            out.push(dup);
        }
    }
    out
}

fn load(db: &Db, rows: &[Lead]) {
    run(
        db,
        MAIN,
        "create table leads (id int primary key, account_id int not null, region text not null, \
         email text not null, company text not null, name text not null, status text not null, \
         score int not null, last_contact date not null, created_at date not null)",
        &[],
    )
    .unwrap();
    for chunk in rows.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|r| {
                format!(
                    "({}, {}, '{}', 'lead{}@acme-{}.example', '{}', 'Lead {}', '{}', {}, \
                     date '2025-06-01' + {}, date '2025-06-01' + {})",
                    r.id,
                    r.account,
                    r.region,
                    r.id,
                    r.account,
                    r.company,
                    r.id,
                    ["new", "contacted", "qualified"][(r.id % 3) as usize],
                    r.id % 81,
                    r.id % 458,
                    r.created
                )
            })
            .collect();
        run(db, MAIN, &format!("insert into leads values {}", values.join(", ")), &[]).unwrap();
    }
    run(db, MAIN, "create index leads_region on leads (region)", &[]).unwrap();
}

/// R3's rows that have a later row of the same company in R3: what the demo's statements delete.
fn stale_r3(rows: &[Lead]) -> i64 {
    let mut newest = std::collections::HashMap::new();
    for r in rows.iter().filter(|r| r.region == "R3") {
        let n = newest.entry(&r.company).or_insert(r.created);
        *n = (*n).max(r.created);
    }
    rows.iter().filter(|r| r.region == "R3" && r.created < newest[&r.company]).count() as i64
}

fn timed(what: &str, db: &Db, sql: &str) -> (Duration, chronos::sql::Output) {
    let t = Instant::now();
    let out = run(db, MAIN, sql, &[]).unwrap().pop().unwrap();
    let took = t.elapsed();
    eprintln!("{what}: {took:.2?}");
    (took, out)
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        v => panic!("not an integer: {v:?}"),
    }
}

fn setup() -> (Db, Vec<Lead>) {
    let rows = leads();
    assert_eq!(rows.len(), 150_600);
    let db = Db::new();
    let t = Instant::now();
    load(&db, &rows);
    eprintln!("loaded {} rows in {:.2?}", rows.len(), t.elapsed());
    (db, rows)
}

const EXISTS_NEWER: &str = "from leads l1 where l1.region = 'R3' and exists \
     (select 1 from leads l2 where l2.company = l1.company and l2.region = 'R3' and l2.created_at > l1.created_at)";

#[test]
#[ignore]
fn correlated_exists_count() {
    let (db, rows) = setup();
    let (took, out) =
        timed("count(*) where exists (correlated)", &db, &format!("select count(*) as rows_to_delete {EXISTS_NEWER}"));
    assert_eq!(int(&out.rows[0][0]), stale_r3(&rows));
    assert!(took < Duration::from_secs(20), "{took:?}");
}

#[test]
#[ignore]
fn correlated_exists_delete() {
    let (db, rows) = setup();
    let want = stale_r3(&rows);
    let (took, out) = timed("delete where exists (correlated)", &db, &format!("delete {EXISTS_NEWER}"));
    assert_eq!(out.tag, format!("DELETE {want}"));
    let left = run(&db, MAIN, "select count(*) from leads", &[]).unwrap().pop().unwrap();
    assert_eq!(int(&left.rows[0][0]), rows.len() as i64 - want);
    assert!(took < Duration::from_secs(30), "{took:?}");
}

#[test]
#[ignore]
fn in_subquery_of_grouped_rows() {
    let (db, rows) = setup();
    let sql = "select id, company from leads where company in \
               (select company from leads where region = 'R3' group by company having count(*) > 1)";
    let (took, out) = timed("select where company in (grouped subquery)", &db, sql);
    // every lead of an R3 company: those companies are all in R3, 6 leads each
    assert_eq!(out.rows.len() as i64, rows.iter().filter(|r| r.region == "R3").count() as i64);
    assert!(took < Duration::from_secs(2), "{took:?}"); // 2.4 s reading the list per outer row, 0.16 s since
}

#[test]
#[ignore]
fn in_variants() {
    let (db, rows) = setup();
    let r3 = rows.iter().filter(|r| r.region == "R3").count() as i64;
    let multi = "(select company from leads where region = 'R3' group by company having count(*) > 1)";
    let (_, out) = timed(
        "count(*) where region = R3 and company in (grouped)",
        &db,
        &format!("select count(*) from leads l1 where l1.region = 'R3' and l1.company in {multi}"),
    );
    assert_eq!(int(&out.rows[0][0]), r3);
    let (took, out) = timed(
        "count(*) where company not in (grouped)",
        &db,
        &format!("select count(*) from leads where company not in {multi}"),
    );
    assert_eq!(int(&out.rows[0][0]), rows.len() as i64 - r3);
    assert!(took < Duration::from_secs(2), "{took:?}"); // 4.4 s reading the list per outer row, 0.23 s since
    let (_, out) = timed(
        "count(*) where id in (select id from R3)",
        &db,
        "select count(*) from leads where id in (select id from leads where region = 'R3')",
    );
    assert_eq!(int(&out.rows[0][0]), r3);
    let (_, out) = timed(
        "update where company in (grouped)",
        &db,
        &format!("update leads set status = 'x' where company in {multi}"),
    );
    assert_eq!(out.tag, format!("UPDATE {r3}"));
    let (_, out) =
        timed("delete where company in (grouped)", &db, &format!("delete from leads where company in {multi}"));
    assert_eq!(out.tag, format!("DELETE {r3}"));
}

/// The agents in the demo worked on a world forked from main, not on main.
#[test]
#[ignore]
fn in_on_a_world() {
    let (db, rows) = setup();
    let r3 = rows.iter().filter(|r| r.region == "R3").count() as i64;
    db.fork(MAIN, "agent").unwrap();
    let sql = "select id, company from leads where company in \
               (select company from leads where region = 'R3' group by company having count(*) > 1)";
    let t = Instant::now();
    let out = run(&db, "agent", sql, &[]).unwrap().pop().unwrap();
    eprintln!("select where company in (grouped subquery), on a world: {:.2?}", t.elapsed());
    assert_eq!(out.rows.len() as i64, r3);
    // after the world has changed some rows
    run(&db, "agent", "update leads set status = 'x' where id <= 10", &[]).unwrap();
    let t = Instant::now();
    let out = run(&db, "agent", sql, &[]).unwrap().pop().unwrap();
    eprintln!("the same, after a change in the world: {:.2?}", t.elapsed());
    assert_eq!(out.rows.len() as i64, r3);
}

#[test]
#[ignore]
fn other_shapes() {
    let (db, rows) = setup();
    let r3 = rows.iter().filter(|r| r.region == "R3").count() as i64;
    // NOT EXISTS: the newest lead of each R3 company. This and the scalar subquery below have no
    // ceiling: whichever of them (or of the queries before) asks first builds the table's search
    // index, a few seconds, once, and a slow CI machine would take several times that.
    let (_, out) = timed(
        "count(*) where not exists (correlated)",
        &db,
        "select count(*) from leads l1 where l1.region = 'R3' and not exists \
         (select 1 from leads l2 where l2.company = l1.company and l2.created_at > l1.created_at)",
    );
    assert_eq!(int(&out.rows[0][0]), r3 - stale_r3(&rows));
    // a correlated scalar subquery with an aggregate
    let (_, out) = timed(
        "count(*) where scalar subquery = outer value (correlated)",
        &db,
        "select count(*) from leads l1 where l1.region = 'R3' and l1.created_at = \
         (select max(l2.created_at) from leads l2 where l2.company = l1.company)",
    );
    assert_eq!(int(&out.rows[0][0]), r3 - stale_r3(&rows));
    // a correlated IN
    let (took, out) = timed(
        "count(*) where id in (correlated)",
        &db,
        "select count(*) from leads l1 where l1.region = 'R3' and l1.id in \
         (select l2.id from leads l2 where l2.company = l1.company and l2.region = 'R3')",
    );
    assert_eq!(int(&out.rows[0][0]), r3);
    assert!(took < Duration::from_secs(5), "{took:?}"); // 23 s one subquery per outer row, 0.05 s since
}
