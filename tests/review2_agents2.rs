//! Regressions from the second review of agents, simulations and search: replayed scripts are
//! checked, transactions' worlds are private, generated names are checked, a simulation's worlds
//! share its slot, zero vectors keep the index's fast path, `sim` metadata always fits,
//! concurrent metadata changes all land, and huge world TTLs are refused.

use chronos::search::{Metric, VecIndex};
use chronos::sql::{self, Session};
use chronos::{AgentSpec, Can, Db, MAIN, Simulate};
use serde_json::json;
use std::io::Write;
use std::process::{Command, Stdio};

fn code(e: chronos::Error) -> &'static str {
    match e {
        chronos::Error::Constraint { code, .. } => code,
        e => panic!("not a constraint error: {e}"),
    }
}

fn col(db: &Db, sql: &str) -> Vec<String> {
    sql::run(db, MAIN, sql, &[]).unwrap().pop().unwrap().rows.iter().map(|r| r[0].to_text()).collect()
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chronos-review2-agents2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// 1. `sim` is only a simulation's to set, so a safe-mode MCP client can't plant a script for
///    `replay` to run on main.
#[test]
fn replay_runs_only_what_a_simulation_recorded() {
    let db = Db::new();
    db.fork(MAIN, "w").unwrap();
    let sim = json!({ "sim": { "script": "USE BRANCH main; DELETE FROM users", "score_query": "select 1",
        "seed": 0, "index": 0, "base": "main", "base_id": "main", "at": 1 } });
    assert_eq!(code(db.set_meta("w", sim.clone()).unwrap_err()), "22023");
    assert_eq!(code(db.fork_with(MAIN, "w2", sim.clone()).unwrap_err()), "22023");
    assert_eq!(sql::run(&db, MAIN, "alter world w set (sim = 1)", &[]).unwrap_err().code, "22023");

    // end to end: safe-mode MCP, no agent
    let dir = tmp("mcp");
    let d = Db::open(&dir).unwrap();
    col(&d, "create table users (id int primary key, name text)");
    col(&d, "insert into users values (1, 'Ada')");
    drop(d);
    let call = |id: u32, name: &str, args: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
        )
    };
    let script = [
        call(1, "fork", r#"{"name":"w"}"#),
        call(
            2,
            "set_meta",
            r#"{"branch":"w","meta":{"sim":{"script":"USE BRANCH main; DELETE FROM users","score_query":"select 1","seed":0,"index":0,"base":"main","base_id":"main","at":1}}}"#,
        ),
        call(3, "replay", r#"{"world":"w"}"#),
        call(4, "sql", r#"{"sql":"select count(*) from users"}"#),
    ];
    let r = mcp(&dir, &script);
    let text = |i: usize| r[i]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert_eq!(r[1]["result"]["isError"], true, "{}", text(1));
    assert_eq!(r[2]["result"]["isError"], true, "{}", text(2));
    assert!(text(3).contains('1'), "main keeps its row: {}", text(3));
    let _ = std::fs::remove_dir_all(&dir);
}

fn mcp(dir: &std::path::Path, script: &[String]) -> Vec<serde_json::Value> {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["mcp", dir.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(script.join("\n").as_bytes()).unwrap();
    let out = String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap();
    out.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

/// 2. A session's transaction world is its own: no one else reads, writes, forks, merges, drops,
///    diffs or tags it by name, and the audit never names it.
#[test]
fn transaction_worlds_are_private() {
    let dir = tmp("tx");
    let db = Db::open(&dir).unwrap();
    col(&db, "create table t (id int primary key)");
    let spec = AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn, Can::Write]), ..Default::default() };
    db.create_agent("mallory", &spec).unwrap();
    db.fork(MAIN, "scratch").unwrap();
    let mut w = Session::new("scratch");
    w.agent = Some("mallory".into());
    w.run(&db, "begin; insert into t values (9); commit", &[]).unwrap(); // its own transaction
    let mut b = Session::new(MAIN);
    b.run(&db, "begin; insert into t values (1)", &[]).unwrap();
    let tx = db.branches().into_iter().map(|(n, _)| n).find(|n| n.starts_with("_tx_")).expect("B's transaction");

    let me = db.as_agent("mallory").unwrap();
    assert!(me.may_write(&tx, 1).is_err() && me.may_read(&tx).is_err() && me.may_manage(&tx).is_err());
    assert!(me.may_fork(&tx).is_err() && me.may_merge(&tx).is_err());
    assert!(db.world(&tx).is_err() && db.diff(&tx).is_err() && db.set_meta(&tx, json!({ "a": 1 })).is_err());
    assert!(db.diff_between(MAIN, &tx).is_err() && db.history(&tx, 10).is_err());
    // the HTTP API refuses any op naming it (api::call), whoever calls
    assert!(db.private_world(&tx) && !db.private_world("scratch") && !db.private_world(MAIN));
    let mut m = Session::new(&tx);
    m.agent = Some("mallory".into());
    assert!(m.run(&db, "insert into t values (666)", &[]).is_err());
    assert!(m.run(&db, "select count(*) from t", &[]).is_err());
    let mut m = Session::new(MAIN);
    m.agent = Some("mallory".into());
    for q in [format!("use world '{tx}'"), format!("drop world '{tx}'"), format!("diff world '{tx}'")] {
        assert!(m.run(&db, &q, &[]).is_err(), "{q}");
    }
    assert!(sql::run(&db, MAIN, &format!("drop world '{tx}'"), &[]).is_err());
    assert!(
        !sql::run(&db, MAIN, "show worlds", &[]).unwrap()[0].rows.iter().any(|r| r[0].to_text().starts_with("_tx_"))
    );

    // B's transaction is untouched and commits only its own row
    b.run(&db, "commit", &[]).unwrap();
    assert_eq!(col(&db, "select id from t order by id"), ["1"]);
    let audit = db.audit(None, 1000).unwrap();
    assert!(audit.iter().all(|e| !e.world.starts_with("_tx_") && !e.action.contains("_tx_")), "{audit:?}");
    assert!(audit.iter().any(|e| e.world == "scratch" && e.action == "commit"), "{audit:?}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 3. Every name a simulation makes is checked: a prefix of `_tx` can't make `_tx_0`.
#[test]
fn generated_world_names_are_checked() {
    let db = Db::new();
    let e = db.simulate(&Simulate::new(MAIN, 2, "_tx", "select 1", "select 1")).unwrap_err();
    assert_eq!(code(e), "42939");
    assert_eq!(db.branches().len(), 1, "nothing was made");
}

/// 4. A simulation's worlds run within the slot its caller holds: max_concurrent = 1 fails none.
#[test]
fn simulations_share_their_slot() {
    let db = Db::new();
    col(&db, "create table t (id int primary key)");
    db.create_agent("one", &AgentSpec { max_concurrent: Some(1), ..Default::default() }).unwrap();
    // as the HTTP op does: the request's slot, then the simulation
    let agent = db.agent("one").unwrap();
    let held = db.slot(&agent).unwrap();
    let mut spec = Simulate::new(MAIN, 6, "c", "insert into t values ($1)", "select count(*) from t");
    spec.agent = Some("one".into());
    spec.threads = 4;
    let r = db.simulate(&spec).unwrap();
    assert!(r.worlds.iter().all(|w| w.error.is_none() && w.score == Some(1.0)), "{:?}", r.worlds);
    drop(held);
    // and as SQL does: the SIMULATE statement's slot
    let mut s = Session::new(MAIN);
    s.agent = Some("one".into());
    s.run(&db, "simulate 3 worlds as d run $$insert into t values ($1)$$ score $$select count(*) from t$$", &[])
        .unwrap();
    for i in 0..3 {
        let sim = &db.world(&format!("d_{i}")).unwrap().meta["sim"];
        assert!(sim["error"].is_null() && sim["score"] == 1.0, "{sim}");
    }
    // outside a simulation the limit holds
    let _one = db.slot(&agent).unwrap();
    assert_eq!(code(db.slot(&agent).err().unwrap()), "53300");
}

/// 5. Zero vectors (placeholders) don't take the index off its fast path, and results stay exact.
#[test]
fn zero_vectors_keep_unit_search() {
    let mut ix = VecIndex::new(3);
    ix.add(&[1.0, 0.0, 0.0], &[]);
    ix.add(&[0.0, 0.0, 0.0], &[]);
    assert!(ix.unit(), "a zero vector keeps the index unit");
    ix.add(&[2.0, 0.0, 0.0], &[]);
    assert!(!ix.unit());

    // the exact path, now on every core, gives what comparing each vector does
    let mut ix = VecIndex::new(8);
    let mut x: u64 = 7;
    let mut rnd = || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((x >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };
    for _ in 0..10_000 {
        ix.add(&(0..8).map(|_| rnd() * 3.0).collect::<Vec<_>>(), &[]);
    }
    let q: Vec<f32> = (0..8).map(|_| rnd()).collect();
    let all: roaring::RoaringBitmap = (0..10_000u32).collect();
    for m in [Metric::L2, Metric::Cosine] {
        let mut want: Vec<(u32, f32)> = (0..10_000).map(|id| (id, ix.score_by(id, &q, m))).collect();
        want.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        want.truncate(10);
        assert_eq!(ix.exact_by(&q, &all, 10, m), want, "{m:?}");
    }

    // through SQL: a zero vector ranks where its metric puts it
    let db = Db::new();
    col(&db, "create table e (id int primary key, v vector(3))");
    col(&db, "create index on e using hnsw (v vector_l2_ops)");
    let mut rows = vec!["(0, '[0,0,0]')".to_string()];
    for (i, deg) in [10, 40, 62, 70, 80, 88, 100].iter().enumerate() {
        let r = (*deg as f64).to_radians();
        rows.push(format!("({}, '[{},{},0]')", i + 1, r.cos(), r.sin()));
    }
    col(&db, &format!("insert into e values {}", rows.join(", ")));
    // by distance from [1,0,0]: 10 and 40 degrees are nearer than the zero vector (distance 1), 62+ farther
    assert_eq!(col(&db, "select id from e order by v <-> '[1,0,0]' limit 4"), ["1", "2", "0", "3"]);
    // by angle, a zero vector has none: it comes last
    assert_eq!(col(&db, "select id from e order by v <=> '[1,0,0]' limit 8").last().unwrap(), "0");
}

/// 6. A script whose JSON-escaped form wouldn't fit the `sim` metadata is refused up front,
///    leaving no worlds behind.
#[test]
fn sim_metadata_always_fits() {
    let db = Db::new();
    let script = format!("select 1 -- {}", "\u{1}".repeat(20_000)); // 20 KB, 120 KB as JSON
    let e = db.simulate(&Simulate::new(MAIN, 2, "big", &script, "select 1")).unwrap_err();
    assert_eq!(code(e), "22023");
    assert_eq!(db.branches().len(), 1, "nothing left behind");
}

/// 7. Concurrent metadata changes all land.
#[test]
fn concurrent_metadata_changes_all_land() {
    let db = Db::new();
    db.fork(MAIN, "w").unwrap();
    std::thread::scope(|s| {
        for t in 0..8 {
            let db = &db;
            s.spawn(move || {
                for i in 0..50 {
                    db.set_meta("w", json!({ format!("k{t}_{i}"): i })).unwrap();
                }
            });
        }
    });
    assert_eq!(db.world("w").unwrap().meta.as_object().unwrap().len(), 400);
}

/// 8. A world TTL is at most 100 years, and an expiry never wraps around.
#[test]
fn huge_world_ttls() {
    let db = Db::new();
    let huge = AgentSpec { world_ttl: Some(u64::MAX), ..Default::default() };
    assert_eq!(code(db.create_agent("a", &huge).unwrap_err()), "22023");
    db.create_agent("a", &AgentSpec::default()).unwrap();
    assert_eq!(code(db.alter_agent("a", &huge).unwrap_err()), "22023");
    let long = AgentSpec { world_ttl: Some(36_525 * 86_400_000), ..Default::default() };
    db.alter_agent("a", &long).unwrap();
    let w = db.as_agent("a").unwrap().fork(MAIN, "w", serde_json::Value::Null).unwrap();
    assert!(w.expires.unwrap() > w.created);
    assert_eq!(db.expire_worlds().unwrap(), 0);
}
