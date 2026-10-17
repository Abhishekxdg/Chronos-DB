//! What undoing an agent costs: ROWS rows in main, an agent changes AGENT of them straight in main
//! (no world, no approval), then a person changes OTHERS of the rest and a few of the agent's.
//! UNDO AGENT puts the agent's rows back and keeps the person's.
//!
//!   ROWS=500000 AGENT=100000 OTHERS=50000 cargo run --release --example undo_agent
use chronos::{AgentSpec, Can, Db, MAIN};
use std::time::Instant;

fn env(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn main() {
    let (rows, agent, others) = (env("ROWS", 500_000), env("AGENT", 100_000), env("OTHERS", 50_000));
    let dir = std::env::temp_dir().join(format!("chronos-undo-agent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let sql = |q: &str| chronos::sql::run(&db, MAIN, q, &[]).unwrap();
    sql("create table t (id bigint primary key, v text, n int); create index t_n on t (n)");
    let t = Instant::now();
    for from in (0..rows).step_by(10_000) {
        let v: Vec<String> =
            (from..(from + 10_000).min(rows)).map(|i| format!("({i}, 'row {i}', {})", i % 1000)).collect();
        sql(&format!("insert into t values {}", v.join(",")));
    }
    println!("loaded {rows} rows in {:.1} s", t.elapsed().as_secs_f64());

    let can = vec![Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain];
    db.create_agent("bot", &AgentSpec { can: Some(can), ..Default::default() }).unwrap();
    let mut bot = chronos::sql::Session::new(MAIN);
    bot.agent = Some("bot".into());
    std::thread::sleep(std::time::Duration::from_millis(5));
    let since = chronos::sql::moment("now").unwrap();
    let t = Instant::now();
    for from in (0..agent).step_by(10_000) {
        let to = (from + 10_000).min(agent);
        bot.run(&db, &format!("update t set v = 'bot', n = n + 1 where id >= {from} and id < {to}"), &[]).unwrap();
    }
    println!("agent changed {agent} rows in {:.1} s", t.elapsed().as_secs_f64());
    // a person, afterwards: rows the agent never touched, and 100 it did
    sql(&format!("update t set v = 'person' where id >= {agent} and id < {}", agent + others));
    sql("update t set v = 'person' where id < 100");

    let t = Instant::now();
    let u = db.undo_agent("bot", MAIN, since, true).unwrap();
    println!(
        "undo agent: {} rows put back, {} left (changed since) in {:.2} s",
        u.rows,
        u.skipped,
        t.elapsed().as_secs_f64()
    );
    let count = |q: &str| sql(q)[0].rows[0][0].to_text();
    assert_eq!(count("select count(*) from t where v = 'bot'"), "0");
    assert_eq!(count("select count(*) from t where v = 'person'"), (others + 100).to_string());
    assert_eq!(
        count(&format!("select count(*) from t where v = 'row ' || id and n = id % 1000 and id < {agent}")),
        (agent - 100).to_string()
    );
    println!("checked: no agent rows left, every person's row kept");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
