//! Search latency budget, run by CI in release mode:
//!
//!   cargo test --release --test perf -- --ignored
//!
//! 100k rows, p99 per query kind against ceilings about 5x today's numbers on an 8-core laptop.
//! Shared CI machines are too noisy for a 5% rule; this catches real regressions (a lost
//! index, an accidental full scan, a quadratic step), not small drifts. Each kind takes the
//! best p99 of 3 rounds: a regression is slow every round, a noisy neighbour is not (one CI
//! run hit 5.3 ms on main while the same query on the agent took 2.2 ms). Use `examples/find.rs`
//! for real measurements.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN, Query};
use serde_json::json;
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
    fn vec(&mut self) -> Vec<f32> {
        let v: Vec<f32> = (0..64).map(|_| self.below(2001) as f32 / 1000.0 - 1.0).collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter().map(|x| x / n).collect()
    }
    fn words(&mut self, n: usize) -> String {
        (0..n).map(|_| format!("w{}", self.below(2000))).collect::<Vec<_>>().join(" ")
    }
}

type MakeQuery<'a> = &'a dyn Fn(&mut Rng, usize) -> Query;

fn best_p99(db: &Db, b: &str, make: MakeQuery) -> Duration {
    // a fresh seed per round, so no round repeats a query another already put in the answer cache
    // (five: CI runs these on a machine shared with other jobs, and one quiet round is enough)
    (0..5).map(|round| p99(db, b, make, 1000 + round)).min().unwrap()
}

fn p99(db: &Db, b: &str, make: MakeQuery, seed: u64) -> Duration {
    let mut r = Rng(seed);
    for i in 0..20 {
        db.find(b, &make(&mut r, i)).unwrap(); // warm-up
    }
    let mut lat: Vec<Duration> = (0..200)
        .map(|i| {
            let q = make(&mut r, i);
            let t = Instant::now();
            db.find(b, &q).unwrap();
            t.elapsed()
        })
        .collect();
    lat.sort();
    lat[197]
}

#[test]
#[ignore = "latency budget: run in release with --ignored"]
fn search_latency_budget() {
    let db = Db::new();
    let mut r = Rng(5);
    let rows: Vec<(String, String)> = (0..100_000)
        .map(|i| {
            let (tenant, color, about) = (r.below(100), r.below(5), r.words(12));
            (
                format!("docs/{i:06}"),
                json!({ "tenant": tenant, "color": color, "about": about, "embedding": r.vec() }).to_string(),
            )
        })
        .collect();
    db.batch("main", &rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
    db.fork("main", "agent").unwrap();
    for i in 0..500 {
        let v = json!({ "tenant": 1, "color": 1, "about": r.words(12), "embedding": r.vec() }).to_string();
        db.put("agent", &format!("docs/{:06}", i * 197), &v).unwrap();
    }

    // each query is distinct (k varies), so this measures the engine, not the answer cache
    let q = |_: &mut Rng, i: usize| Query { table: "docs".into(), k: 10 + i % 7, ..Default::default() };
    let budgets: [(&str, u64, MakeQuery); 5] = [
        ("filter", 1, &|r, i| Query { filters: vec![("tenant".into(), json!(r.below(100)))], ..q(r, i) }),
        ("text", 3, &|r, i| Query { text: Some(r.words(2)), ..q(r, i) }),
        ("vector+filter", 5, &|r, i| Query {
            filters: vec![("color".into(), json!(r.below(5)))],
            vector: Some(("embedding".into(), r.vec())),
            ..q(r, i)
        }),
        ("vector", 8, &|r, i| Query { vector: Some(("embedding".into(), r.vec())), ..q(r, i) }),
        ("hybrid", 8, &|r, i| Query {
            filters: vec![("color".into(), json!(r.below(5)))],
            text: Some(r.words(2)),
            vector: Some(("embedding".into(), r.vec())),
            ..q(r, i)
        }),
    ];
    let mut over = vec![];
    for (name, ms, make) in budgets {
        for b in ["main", "agent"] {
            let got = best_p99(&db, b, make);
            eprintln!("{name:>14} on {b:<5}: best p99 {:.3} ms (budget {ms} ms)", got.as_secs_f64() * 1e3);
            if got > Duration::from_millis(ms) {
                over.push(format!("{name} on {b}: {:?} > {ms} ms", got));
            }
        }
    }
    assert!(over.is_empty(), "over budget: {over:?}");
}

/// Correlated EXISTS / NOT EXISTS / IN, a join on `lower(a) = lower(b)` and a long IN list over a
/// 60,000-row table (the shape of the CRM demo's leads, at 40% of its size): each ran a full
/// scan per outer row, two minutes and more at 150,000 rows, and a few tenths of a second now.
/// A regression is slow by orders of magnitude, so the ceilings are loose; a statement that
/// takes over 30 s is cancelled and fails the test, not the CI run's patience.
#[test]
#[ignore = "latency budget: run in release with --ignored"]
fn sql_subquery_budget() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    let mut sql = |q: &str| s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {}", e.message)).pop().unwrap();
    sql("set statement_timeout = '30s'");
    sql("create table leads (id int primary key, account_id int not null, region text not null, email text not null, \
         company text not null, name text not null, status text not null, score int not null, \
         last_contact date not null, created_at date not null)");
    // 10,000 accounts of 6 leads; the region is the account's number mod 10 (6,000 leads each)
    sql("insert into leads select i, (i - 1) / 6 + 1, 'R' || (((i - 1) / 6 + 1) % 10), \
         'lead' || i || '@acme-' || ((i - 1) / 6 + 1) || '.example', 'Acme-' || ((i - 1) / 6 + 1), 'Lead ' || i, \
         'new', i % 80, date '2025-06-01' + (i * 7 % 458), date '2025-06-01' + (i * 13 % 300) - 200 \
         from generate_series(1, 60000) i");
    // 1,000 older copies of R5 leads and of R6 leads: the same email, 30 days older, ids above all
    sql("insert into leads select id + 100000, account_id, region, email, company, name, status, score, \
         last_contact, created_at - 30 from leads where region in ('R5', 'R6') and id % 60 in (25, 31)");
    sql("create index leads_region on leads (region)");
    let count = |q: &str, sql: &mut dyn FnMut(&str) -> chronos::sql::Output| match &sql(q).rows[0][0] {
        Value::Int(n) => *n,
        v => panic!("{q}: {v:?}"),
    };

    let mut over = vec![];
    let mut timed =
        |name: &str, ms: u64, q: &str, want: Option<i64>, sql: &mut dyn FnMut(&str) -> chronos::sql::Output| {
            let t = Instant::now();
            let got = count(&format!("select count(*) from ({q}) x"), sql);
            let took = t.elapsed();
            eprintln!("{name:>26}: {:8.1} ms (budget {ms} ms), {got} rows", took.as_secs_f64() * 1e3);
            if took > Duration::from_millis(ms) {
                over.push(format!("{name}: {took:?} > {ms} ms"));
            }
            if let Some(w) = want {
                assert_eq!(got, w, "{name}: {q}");
            }
        };
    // the answers, by queries that never used a subquery per row
    let newest: i64 = count(
        "select count(*) from (select id, row_number() over (partition by company order by created_at desc, id desc) rn \
         from leads where region = 'R3') x where rn = 1",
        &mut sql,
    );
    let older: i64 = count(
        "select count(*) from leads l join (select company, max(created_at) m from leads where region = 'R3' \
         group by company) x on l.company = x.company where l.region = 'R3' and l.created_at < x.m",
        &mut sql,
    );
    let copies = count("select count(*) from leads where region = 'R5' and id > 100000", &mut sql);
    let named = count("select count(distinct company) from leads where region in ('R3', 'R4', 'R5')", &mut sql);

    timed(
        "group by having",
        1000,
        "select company, count(*) from leads where region = 'R3' group by company having count(*) > 1",
        None,
        &mut sql,
    );
    timed(
        "exists (newer)",
        2000,
        "select l1.id from leads l1 where l1.region = 'R3' and exists (select 1 from leads l2 \
           where l2.company = l1.company and l2.region = 'R3' and l2.created_at > l1.created_at)",
        Some(older),
        &mut sql,
    );
    timed(
        "not exists (newest)",
        2000,
        "select id from leads l where region = 'R3' and not exists (select 1 from leads l2 \
           where l2.company = l.company and l2.region = 'R3' and (l2.created_at > l.created_at \
           or (l2.created_at = l.created_at and l2.id > l.id)))",
        Some(newest),
        &mut sql,
    );
    timed(
        "in (join on lower())",
        2000,
        "select id from leads where region = 'R5' and id in (select l1.id from leads l1 \
           inner join leads l2 on lower(l1.email) = lower(l2.email) and l1.id <> l2.id \
           where l1.region = 'R5' and l1.created_at < l2.created_at)",
        Some(copies),
        &mut sql,
    );
    // the subquery's equality is on an indexed column that picks 6,000 rows (a tenth of the table):
    // per outer row until half a second has gone that way, then hashed
    let few = count(
        "select count(*) from leads l1 join (select region, min(id) m from leads where id % 500 = 0 \
         group by region) x on x.region = l1.region where l1.region in ('R3', 'R4', 'R5') and l1.id > x.m",
        &mut sql,
    );
    timed(
        "exists, indexed low-card key",
        2500,
        "select l1.id from leads l1 where l1.region in ('R3', 'R4', 'R5') and exists (select 1 from leads l2 \
           where l2.region = l1.region and l2.id % 500 = 0 and l2.id < l1.id)",
        Some(few),
        &mut sql,
    );
    // few distinct outer values (80 scores) against one bucket of 60,000 rows: each answer is kept
    // for the next outer row that reads the same values
    timed(
        "exists, few outer values",
        2000,
        "select id from leads a where exists (select 1 from leads e where e.status = a.status \
           and e.score > a.score + 1000)",
        Some(0),
        &mut sql,
    );
    timed(
        "long in list",
        1500,
        "select id from leads where company in (select company from leads \
           where region in ('R3', 'R4', 'R5'))",
        None,
        &mut sql,
    );
    timed(
        "long not in list",
        1500,
        "select id from leads where company not in (select company from leads \
           where region in ('R3', 'R4', 'R5'))",
        None,
        &mut sql,
    );
    assert!(named > 0);
    // a delete that reads its own table in a correlated EXISTS, rolled back
    sql("begin");
    let t = Instant::now();
    let deleted = sql("delete from leads l1 where l1.region = 'R3' and exists (select 1 from leads l2 \
                       where l2.company = l1.company and l2.region = 'R3' and l2.created_at > l1.created_at)");
    eprintln!(
        "{:>26}: {:8.1} ms (budget 2000 ms), {}",
        "delete where exists",
        t.elapsed().as_secs_f64() * 1e3,
        deleted.tag
    );
    if t.elapsed() > Duration::from_millis(2000) {
        over.push(format!("delete where exists: {:?} > 2000 ms", t.elapsed()));
    }
    assert_eq!(deleted.tag, format!("DELETE {older}"));
    sql("rollback");
    assert!(over.is_empty(), "over budget: {over:?}");
}
