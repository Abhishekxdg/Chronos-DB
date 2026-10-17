//! Supply-chain optimization over many worlds: the "ultimate test".
//!
//! Main holds the real state: warehouses, products, stock, a seasonal noisy demand forecast,
//! lead times, costs, and the reorder policy in use today. Round 1 forks WORLDS worlds from main,
//! gives each a different reorder policy (from its index and SEED), simulates DAYS days of demand
//! in SQL inside the world (orders placed, stock moved), scores it in SQL and keeps the best KEEP
//! percent. Each later round forks children from every winner (worlds of worlds), mutates the
//! winner's policy around what worked, simulates the next DAYS days from the winner's own state,
//! and selects again. Finally the best policy is merged into main for real.
//!
//!   cargo run --release --example simulate                  # 10,000 worlds, 3 rounds of 30 days
//!   ULTIMATE=1 cargo run --release --example simulate       # 100,000 worlds, smaller per world
//!   env: WORLDS, ROUNDS, DAYS, WAREHOUSES, PRODUCTS, KEEP (percent), SEED, THREADS
use chronos::sql::Value;
use chronos::{Db, MAIN, Resolve};
use std::time::Instant;

#[path = "stats/mod.rs"]
mod stats;
use stats::{Rng, cpu_s, dir_size, load, mem_mb, ms};

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// A reorder policy: safety stock in days of demand (fast and slow movers), and order size in
/// days of demand.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Policy {
    safety_fast: f64,
    safety_slow: f64,
    cover: f64,
}

impl Policy {
    fn random(r: &mut Rng) -> Policy {
        Policy { safety_fast: r.f() * 6.0, safety_slow: r.f() * 6.0, cover: 1.0 + r.f() * 20.0 }
    }
    /// Near this one: each knob moves by up to `step` of its range, both ways.
    fn mutate(&self, r: &mut Rng, step: f64) -> Policy {
        let mut m = |x: f64, lo: f64, hi: f64| (x + (r.f() * 2.0 - 1.0) * step * (hi - lo)).clamp(lo, hi);
        Policy {
            safety_fast: m(self.safety_fast, 0.0, 6.0),
            safety_slow: m(self.safety_slow, 0.0, 6.0),
            cover: m(self.cover, 1.0, 21.0),
        }
    }
    /// Rewrites the world's policy table from this policy, in one statement.
    fn sql(&self) -> String {
        format!(
            "update policy set rp = round(mu * (lead + case when fast then {:.3} else {:.3} end))::int, \
             qty = greatest(round(mu * {:.3}), 1)::int",
            self.safety_fast, self.safety_slow, self.cover
        )
    }
    fn show(&self) -> String {
        format!("safety {:.2}/{:.2} days, order {:.2} days", self.safety_fast, self.safety_slow, self.cover)
    }
}

/// Simulates days `d0..d1` from the world's current stock with its current policy, as one
/// recursive query: a row per SKU per day. Each morning, if stock plus pipeline (`pos`) is at or
/// below the reorder point, an order of `qty` is placed, arriving after the product's lead time
/// (1-5 days); what arrives today (p1) is added, demand is sold or lost, and the pipeline moves
/// up a day. sold, lost, held (unit-days) and n (orders) are running totals since day 0. Stock is
/// kept as a snapshot per SKU at the end of each round (id = day * 1000 + sku), so a world's
/// stock rows are its history.
fn sim(d0: usize, d1: usize) -> String {
    let order = "s.pos <= s.rp";
    let slot = |k: usize| format!("case when s.lead = {k} and {order} then s.qty else 0 end");
    format!(
        "with recursive sim(sku, day, wh, product, lead, rp, qty, on_hand, p1, p2, p3, p4, p5, pos, sold, lost, held, n, ord) as (
           select s.sku, {d0}, s.wh, s.product, p.lead, p.rp, p.qty, s.on_hand, s.p1, s.p2, s.p3, s.p4, s.p5,
             s.on_hand + s.p1 + s.p2 + s.p3 + s.p4 + s.p5, s.sold, s.lost, s.held, s.n, 0
           from stock s join policy p on p.id = s.sku where s.day = {d0}
           union all
           select s.sku, s.day + 1, s.wh, s.product, s.lead, s.rp, s.qty, greatest(s.on_hand + s.p1 - f.demand, 0),
             s.p2 + {}, s.p3 + {}, s.p4 + {}, s.p5 + {}, {},
             s.pos - least(s.on_hand + s.p1, f.demand) + case when {order} then s.qty else 0 end,
             s.sold + least(s.on_hand + s.p1, f.demand), s.lost + greatest(f.demand - s.on_hand - s.p1, 0),
             s.held + greatest(s.on_hand + s.p1 - f.demand, 0), s.n + case when {order} then 1 else 0 end,
             case when {order} then s.qty else 0 end
           from sim s join forecast f on f.id = s.day * 1000 + s.sku
           where s.day < {d1})",
        slot(1),
        slot(2),
        slot(3),
        slot(4),
        slot(5)
    )
}

/// The simulation's writes: the orders placed, then the stock as it stands on day `d1`. (Each
/// statement runs the recursive query again: an `UPDATE ... FROM` a recursive `WITH` took
/// seconds for 40 rows, so the stock is appended as snapshots instead of updated.)
fn run_days(d0: usize, d1: usize) -> String {
    let s = sim(d0, d1);
    format!(
        "{s} insert into orders select day * 1000 + sku, day, sku, ord from sim where ord > 0;
         {s} insert into stock select day * 1000 + sku, day, sku, wh, product, on_hand, p1, p2, p3, p4, p5, sold, lost, held, n
           from sim where day = {d1}"
    )
}

/// Profit in cents from day 0 to `day`: margin on units sold, minus lost sales, holding and ordering.
fn score(db: &Db, w: &str, day: usize) -> i64 {
    let q = format!(
        "select sum(s.sold * (p.price - p.cost) - s.lost * p.penalty - s.held * w.hold - s.n * p.order_cost)
         from stock s join products p on p.id = s.product join warehouses w on w.id = s.wh where s.day = {day}"
    );
    match &chronos::sql::run(db, w, &q, &[]).unwrap()[0].rows[0][0] {
        Value::Int(v) => *v,
        v => panic!("score: {v:?}"),
    }
}

/// The real state, in main.
// the demand curves use 2π rounded (6.28, 6.2832): the results in BENCHMARKS.md were made with them
#[allow(clippy::approx_constant)]
fn load_main(db: &Db, seed: u64, warehouses: usize, products: usize, days: usize) {
    let sql = |q: &str| chronos::sql::run(db, MAIN, q, &[]).unwrap();
    sql("create table warehouses (id int primary key, name text, hold int)");
    sql(
        "create table products (id int primary key, name text, price int, cost int, penalty int, order_cost int, lead int)",
    );
    sql("create table stock (id int primary key, day int, sku int, wh int, product int, on_hand int, p1 int, p2 int, p3 int, p4 int, p5 int,
         sold int, lost int, held int, n int)");
    sql("create table policy (id int primary key, mu float, lead int, fast boolean, rp int, qty int)");
    sql("create table forecast (id int primary key, day int, sku int, demand int)");
    sql("create table orders (id int primary key, day int, sku int, qty int)");
    let mut r = Rng(seed);
    let wh: Vec<String> = (0..warehouses).map(|w| format!("({w}, 'wh-{w}', {})", 1 + r.below(3))).collect();
    sql(&format!("insert into warehouses values {}", wh.join(",")));
    let mut prod = vec![];
    let mut base = vec![];
    for p in 0..products {
        let cost = 200 + r.below(4800) as i64; // $2 - $50
        let price = cost + cost * (20 + r.below(80) as i64) / 100;
        let lead = 1 + r.below(5);
        prod.push(format!(
            "({p}, 'product-{p}', {price}, {cost}, {}, {}, {lead})",
            (price - cost) * 2,
            500 + r.below(2000)
        ));
        base.push((lead, 1.0 + r.f().powi(2) * 40.0)); // mean daily demand, a few fast movers
    }
    sql(&format!("insert into products values {}", prod.join(",")));
    let (mut stock, mut policy, mut fc) = (vec![], vec![], vec![]);
    for w in 0..warehouses {
        let scale = 0.5 + r.f();
        for (p, &(lead, mu)) in base.iter().enumerate() {
            let sku = w * products + p;
            let mu = mu * scale;
            let phase = r.f() * 6.28;
            let mut sum = 0.0;
            for d in 0..days {
                // seasonal (a 28-day cycle), trending slightly, with noise
                let season = 1.0 + 0.4 * (d as f64 * 6.2832 / 28.0 + phase).sin() + 0.002 * d as f64;
                let noise = (-2.0 * (1.0 - r.f()).ln()).sqrt() * (6.2832 * r.f()).cos(); // normal
                let demand = (mu * season * (1.0 + 0.35 * noise)).round().max(0.0) as i64;
                sum += demand as f64;
                fc.push(format!("({}, {d}, {sku}, {demand})", d * 1000 + sku));
            }
            let mu = sum / days as f64;
            let on_hand = (mu * 10.0).round() as i64;
            stock.push(format!("({sku}, 0, {sku}, {w}, {p}, {on_hand}, 0, 0, 0, 0, 0, 0, 0, 0, 0)"));
            // today's policy: reorder at lead-time demand, order a week's worth
            let (rp, qty) = ((mu * lead as f64).round() as i64, ((mu * 7.0).round() as i64).max(1));
            policy.push(format!("({sku}, {mu:.4}, {lead}, {}, {rp}, {qty})", mu > 15.0));
        }
    }
    sql(&format!("insert into stock values {}", stock.join(",")));
    sql(&format!("insert into policy values {}", policy.join(",")));
    for c in fc.chunks(5000) {
        sql(&format!("insert into forecast values {}", c.join(",")));
    }
}

/// Runs `f` on every item across `threads` threads, keeping order.
fn par<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let chunk = items.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let hs: Vec<_> = items.chunks(chunk).map(|c| s.spawn(|| c.iter().map(&f).collect::<Vec<_>>())).collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    })
}

struct Cand {
    name: String,
    parent: Option<usize>, // index into the previous round's winners
    policy: Policy,
    score: i64,
}

fn main() {
    let ultimate = std::env::var("ULTIMATE").is_ok();
    let worlds = env("WORLDS", if ultimate { 100_000 } else { 10_000 });
    let rounds = env("ROUNDS", 3);
    let days = env("DAYS", if ultimate { 10 } else { 30 });
    let (whs, prods) =
        (env("WAREHOUSES", if ultimate { 3 } else { 4 }), env("PRODUCTS", if ultimate { 4 } else { 10 }));
    let keep = (worlds * env("KEEP", 1) / 100).max(1);
    let seed = env("SEED", 42) as u64;
    let threads = env("THREADS", std::thread::available_parallelism().map_or(8, |n| n.get()));
    let skus = whs * prods;
    let horizon = rounds * days;

    let dir = std::env::temp_dir().join(format!("chronos-simulate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    db.set_retention(0).unwrap(); // no time travel needed: let checkpoints drop the log
    let t = Instant::now();
    load_main(&db, seed, whs, prods, horizon);
    db.checkpoint().unwrap();
    println!(
        "main: {whs} warehouses x {prods} products = {skus} SKUs, {horizon} days of forecast ({} rows), loaded in {:.0} ms",
        skus * horizon,
        ms(t)
    );
    println!(
        "{worlds} worlds per round, {rounds} rounds of {days} days, keep {keep}, {threads} threads, seed {seed}, load {:.2}",
        load()
    );
    let (mem0, disk0) = (mem_mb(), dir_size(&dir));
    println!("memory {mem0:.0} MB, disk {:.1} MB before any world", disk0 as f64 / 1e6);

    let mut winners: Vec<Cand> = vec![];
    let mut alive_parents: Vec<Vec<String>> = vec![]; // winners of each earlier round still alive
    let mut first_best = None;
    let mut rows = vec![];
    let mut peak_mem = 0f64;
    for round in 1..=rounds {
        let (d0, d1) = ((round - 1) * days, round * days);
        // this round's worlds: random policies from main, or children of each winner
        let mut cands: Vec<Cand> = if round == 1 {
            (0..worlds)
                .map(|i| {
                    let mut r = Rng(seed ^ (i as u64).wrapping_mul(0x9E37_79B9));
                    Cand { name: format!("r1_{i}"), parent: None, policy: Policy::random(&mut r), score: 0 }
                })
                .collect()
        } else {
            let per = worlds / winners.len();
            let step = 0.25 / round as f64;
            winners
                .iter()
                .enumerate()
                .flat_map(|(wi, w)| {
                    (0..per).map(move |j| {
                        let mut r = Rng(seed ^ ((round * 1_000_003 + wi * 1009 + j) as u64).wrapping_mul(0x9E37_79B9));
                        // child 0 keeps the winner's policy as it is
                        let policy = if j == 0 { w.policy } else { w.policy.mutate(&mut r, step) };
                        Cand { name: format!("r{round}_{wi}_{j}"), parent: Some(wi), policy, score: 0 }
                    })
                })
                .collect()
        };
        let n = cands.len();
        let (c0, t) = (cpu_s(), Instant::now());
        par(&cands, threads, |c| {
            let from = c.parent.map_or(MAIN, |p| winners[p].name.as_str());
            db.fork(from, &c.name).unwrap();
        });
        let (fork_ms, fork_cpu) = (ms(t), cpu_s() - c0);
        let (c0, t) = (cpu_s(), Instant::now());
        let sim = run_days(d0, d1);
        par(&cands, threads, |c| {
            chronos::sql::run(&db, &c.name, &format!("{}; {sim}", c.policy.sql()), &[]).unwrap();
        });
        let (compute_ms, compute_cpu) = (ms(t), cpu_s() - c0);
        let mem = mem_mb();
        peak_mem = peak_mem.max(mem);
        let (c0, t) = (cpu_s(), Instant::now());
        let scores = par(&cands, threads, |c| score(&db, &c.name, d1));
        let (eval_ms, eval_cpu) = (ms(t), cpu_s() - c0);
        for (c, s) in cands.iter_mut().zip(scores) {
            c.score = s;
        }
        // best first; ties by name, so the same seed picks the same worlds
        cands.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
        let median = cands[n / 2].score;
        let losers: Vec<String> = cands.drain(keep.min(n)..).map(|c| c.name).collect();
        // discard bottom-up: this round's losers, then earlier winners left with no children
        let (c0, t) = (cpu_s(), Instant::now());
        db.discard_many(&losers).into_iter().for_each(|r| r.unwrap());
        let used: std::collections::HashSet<usize> = cands.iter().filter_map(|c| c.parent).collect();
        let mut dropped = losers.len();
        if round > 1 {
            let mut keep_names: Vec<String> =
                cands.iter().filter_map(|c| c.parent).map(|p| winners[p].name.clone()).collect();
            keep_names.sort();
            keep_names.dedup();
            let unused: Vec<&String> =
                winners.iter().enumerate().filter(|(i, _)| !used.contains(i)).map(|(_, w)| &w.name).collect();
            db.discard_many(&unused).into_iter().for_each(|r| r.unwrap());
            dropped += unused.len();
            // and so on up the chain: an ancestor with no living descendant goes too
            for level in alive_parents.iter_mut().rev() {
                let live: std::collections::HashSet<String> =
                    db.branches().into_iter().filter_map(|(_, p)| p).collect();
                level.retain(|name| {
                    if live.contains(name) {
                        return true;
                    }
                    db.discard(name).unwrap();
                    dropped += 1;
                    false
                });
            }
            alive_parents.push(keep_names);
        }
        let (discard_ms, discard_cpu) = (ms(t), cpu_s() - c0);
        let alive = db.branches().len() - 1;
        let disk = dir_size(&dir);
        let best = &cands[0];
        if round == 1 {
            first_best = Some(best.policy);
        }
        println!(
            "round {round} (days {d0}-{d1}): {n} worlds, best {} ({}), median {}",
            best.score,
            best.policy.show(),
            median
        );
        let r = format!(
            "| {round} | {n} | {alive} | {fork_ms:.0} | {compute_ms:.0} | {eval_ms:.0} | {discard_ms:.0} ({dropped}) | {:.1} | {mem:.0} | {:.0} | {:.1} | {} | {median} | {:.2} |",
            fork_cpu + compute_cpu + eval_cpu + discard_cpu,
            mem_mb(),
            disk as f64 / 1e6,
            best.score,
            load()
        );
        println!("{r}");
        rows.push(r);
        winners = cands;
    }

    // the winning lineage, and what its final policy would have made from day 0
    let best = &winners[0];
    let chain = {
        let mut c = vec![best.name.clone()];
        let m: std::collections::HashMap<String, Option<String>> = db.branches().into_iter().collect();
        while let Some(Some(p)) = m.get(c.last().unwrap()) {
            c.push(p.clone());
        }
        c.reverse();
        c
    };
    println!(
        "\nbest world {} (depth {}): {} over {horizon} days, lineage {}",
        best.name,
        chain.len() - 1,
        best.score,
        chain.join(" > ")
    );
    let fixed = |name: &str, p: Option<Policy>| {
        db.fork(MAIN, name).unwrap();
        let set = p.map_or(String::new(), |p| format!("{};", p.sql()));
        chronos::sql::run(&db, name, &format!("{set} {}", run_days(0, horizon)), &[]).unwrap();
        let s = score(&db, name, horizon);
        db.discard(name).unwrap();
        s
    };
    let today = fixed("eval_main", None);
    let r1 = fixed("eval_round1", first_best);
    let last = fixed("eval_final", Some(best.policy));
    let pct = |a: i64, b: i64| (a - b) as f64 * 100.0 / b.abs() as f64;
    println!("over all {horizon} days, from main's real state:");
    println!("  main's policy today:                 {today}");
    println!("  round 1's best policy, kept:         {r1}  ({:+.1}% vs today)", pct(r1, today));
    println!(
        "  the final policy, from day 0:        {last}  ({:+.1}% vs today, {:+.1}% vs round 1)",
        pct(last, today),
        pct(last, r1)
    );
    println!(
        "  the winning lineage (policy by round): {}  ({:+.1}% vs today, {:+.1}% vs round 1)",
        best.score,
        pct(best.score, today),
        pct(best.score, r1)
    );

    // merge the winning policy into main: only the policy table, through a world of its own
    // (the winner itself also holds its simulated stock and orders, which must not reach main)
    let t = Instant::now();
    let out =
        chronos::sql::run(&db, &best.name, "select id, lead, fast, rp, qty, mu from policy order by id", &[]).unwrap();
    let vals: Vec<String> = out[0]
        .rows
        .iter()
        .map(|r| match r.as_slice() {
            [Value::Int(id), Value::Int(lead), Value::Bool(f), Value::Int(rp), Value::Int(q), Value::Float(mu)] => {
                format!("({id}, {mu}, {lead}, {f}, {rp}, {q})")
            }
            r => panic!("policy row {r:?}"),
        })
        .collect();
    db.fork(MAIN, "adopt").unwrap();
    chronos::sql::run(
        &db,
        "adopt",
        &format!(
            "insert into policy values {} on conflict (id) do update set rp = excluded.rp, qty = excluded.qty",
            vals.join(",")
        ),
        &[],
    )
    .unwrap();
    let diff = db.diff("adopt").unwrap().len();
    let merged = db.merge("adopt", Resolve::Fail).unwrap();
    let check =
        |b: &str| chronos::sql::run(&db, b, "select id, rp, qty from policy order by id", &[]).unwrap()[0].rows.clone();
    assert_eq!(check(MAIN), check(&best.name));
    println!(
        "merged into main: {diff} policy rows changed of {skus} (diff), {merged} applied, in {:.1} ms; main's stock, orders and forecast untouched",
        ms(t)
    );
    let mut h = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&format!("{:?}", check(MAIN)), &mut h);
    println!("main's policy fingerprint: {:016x}", std::hash::Hasher::finish(&h));

    // clean up bottom-up, then measure what's left on disk
    let t = Instant::now();
    loop {
        let bs = db.branches();
        let parents: std::collections::HashSet<String> = bs.iter().filter_map(|(_, p)| p.clone()).collect();
        let leaves: Vec<String> =
            bs.into_iter().map(|(n, _)| n).filter(|n| n != MAIN && !parents.contains(n)).collect();
        if leaves.is_empty() {
            break;
        }
        leaves.iter().for_each(|l| db.discard(l).unwrap());
    }
    db.checkpoint().unwrap();
    println!(
        "dropped the rest and checkpointed in {:.0} ms; disk {:.1} MB, peak memory {peak_mem:.0} MB",
        ms(t),
        dir_size(&dir) as f64 / 1e6
    );
    let t = Instant::now();
    let (pages, bytes) = db.gc().unwrap();
    println!(
        "gc: {pages} pages, {:.1} MB freed in {:.0} ms; disk {:.1} MB",
        bytes as f64 / 1e6,
        ms(t),
        dir_size(&dir) as f64 / 1e6
    );

    println!(
        "\n| round | worlds | alive after | fork ms | compute ms | eval ms | discard ms (worlds) | CPU s | mem MB after compute | mem MB after discard | disk MB | best | median | load |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    rows.iter().for_each(|r| println!("{r}"));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
