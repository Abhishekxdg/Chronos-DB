//! Regressions from the core review: reserved world names, bounded simulations, the past never
//! bringing back agents, merge and fork rights, agent names, lock poisoning, and vector search
//! (huge k, metrics, dimensions, non-finite numbers, calibration, k = 0).

use chronos::search::VecIndex;
use chronos::sql::{self, Session, Value};
use chronos::{AgentSpec, Can, Db, MAIN, Query, Resolve, Simulate};
use serde_json::json;

fn code(e: chronos::Error) -> &'static str {
    match e {
        chronos::Error::Constraint { code, .. } => code,
        e => panic!("not a constraint error: {e}"),
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chronos-review-core-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn col(db: &Db, sql: &str) -> Vec<String> {
    sql::run(db, MAIN, sql, &[]).unwrap().pop().unwrap().rows.iter().map(|r| r[0].to_text()).collect()
}

/// `_tx_` names are transactions' (discarded on opening): no one else may make one.
#[test]
fn tx_names_are_reserved() {
    let db = Db::new();
    assert_eq!(code(db.fork(MAIN, "_tx_mine").unwrap_err()), "42939");
    assert_eq!(code(db.fork_with(MAIN, "_tx_mine", serde_json::Value::Null).unwrap_err()), "42939");
    assert_eq!(sql::run(&db, MAIN, "create world _tx_y", &[]).unwrap_err().code, "42939");
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    assert_eq!(code(db.as_agent("bot").unwrap().fork(MAIN, "_tx_x", serde_json::Value::Null).unwrap_err()), "42939");
    // transactions and savepoints still make theirs
    col(&db, "create table t (id int primary key)");
    sql::run(&db, MAIN, "begin; insert into t values (1); savepoint s; insert into t values (2); commit", &[]).unwrap();
    assert_eq!(col(&db, "select count(*) from t"), ["2"]);
}

/// A simulation's size is checked before anything is allocated for it, and it keeps to max_worlds.
#[test]
fn simulations_are_bounded() {
    let db = Db::new();
    let s = Simulate::new(MAIN, 1 << 40, "p", "select 1", "select 1");
    assert_eq!(code(db.simulate(&s).unwrap_err()), "54000");
    let e = sql::run(&db, MAIN, "simulate 1000000000000000 worlds as p run $$select 1$$ score $$select 1$$", &[])
        .unwrap_err();
    assert_eq!(e.code, "54000", "{}", e.message);

    sql::run(&db, MAIN, "alter system set max_worlds = 3", &[]).unwrap();
    let s = Simulate::new(MAIN, 5, "q", "select 1", "select $1::float8");
    assert_eq!(code(db.simulate(&s).unwrap_err()), "53400", "all 5 kept needs 5 worlds");
    assert_eq!(db.worlds().len(), 1, "nothing was left behind");
    let mut s = Simulate::new(MAIN, 5, "q", "select 1", "select $1::float8");
    s.keep = Some(2); // 2 kept and one running at a time: 3
    let r = db.simulate(&s).unwrap();
    assert_eq!((r.worlds.len(), r.worlds.iter().filter(|w| w.kept).count()), (5, 2));
}

/// Worlds from the past (simulations, restores) never bring back a dropped agent; and agents
/// need 'restore' to simulate from the past at all.
#[test]
fn the_past_brings_back_no_agents() {
    let dir = tmp("past");
    let db = Db::open(&dir).unwrap();
    col(&db, "create table t (id int primary key)");
    db.create_agent("gone", &AgentSpec { can: Some(vec![Can::Admin]), ..Default::default() }).unwrap();
    db.create_agent(
        "bot",
        &AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn, Can::MergeOwn]), ..Default::default() },
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let then = sql::run(&db, MAIN, "select to_timestamp($1 / 1000.0)", &[Value::Int(ms)]).unwrap();
    let base = format!("main@{}", then[0].rows[0][0].to_text());
    std::thread::sleep(std::time::Duration::from_millis(5));
    db.drop_agent("gone").unwrap();

    let mut s = Simulate::new(&base, 1, "p", "insert into t values (1)", "select 1");
    s.agent = Some("bot".into());
    assert_eq!(code(db.simulate(&s).unwrap_err()), "42501", "the past is a restore's");
    s.agent = None;
    db.simulate(&s).unwrap();
    db.merge("p_0", Resolve::Fail).unwrap();
    assert!(db.agent("gone").is_err(), "the merge brought the dropped agent back");
    assert_eq!(col(&db, "select count(*) from t"), ["1"]);
    db.restore(MAIN, ms as u64).unwrap();
    assert!(db.agent("gone").is_err(), "the restore brought the dropped agent back");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 'merge_own' publishes an agent's world into a parent it may change: main only with
/// 'write_main', and not a world it lost (or never had) the right to change.
#[test]
fn merging_needs_the_right_to_change_the_parent() {
    let db = Db::new();
    db.fork(MAIN, "x").unwrap(); // a person's world
    let can = |c: &[Can]| AgentSpec { can: Some(c.to_vec()), ..Default::default() };
    db.create_agent("b", &can(&[Can::Read, Can::Fork, Can::WriteOwn, Can::Write, Can::MergeOwn])).unwrap();
    db.as_agent("b").unwrap().fork("x", "bx", serde_json::Value::Null).unwrap();
    db.alter_agent("b", &can(&[Can::Read, Can::Fork, Can::WriteOwn, Can::MergeOwn])).unwrap();
    let me = db.as_agent("b").unwrap();
    assert_eq!(code(me.may_merge("bx").unwrap_err()), "42501", "x isn't its to change");
    me.fork(MAIN, "bm", serde_json::Value::Null).unwrap();
    assert_eq!(code(me.may_merge("bm").unwrap_err()), "42501", "main needs write_main");
    me.fork("bm", "bm2", serde_json::Value::Null).unwrap();
    me.may_merge("bm2").unwrap(); // into its own world
    drop(me);
    db.alter_agent("b", &can(&[Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain, Can::MergeOwn])).unwrap();
    db.as_agent("b").unwrap().may_merge("bm").unwrap();
}

/// Forks pin a world (it can't be merged or discarded while they live), so an agent may fork only
/// main and worlds it may change: not another agent's.
#[test]
fn forking_needs_the_right_to_change() {
    let db = Db::new();
    db.create_agent("a", &AgentSpec::default()).unwrap();
    db.create_agent("b", &AgentSpec::default()).unwrap();
    db.as_agent("a").unwrap().fork(MAIN, "wa", serde_json::Value::Null).unwrap();
    let b = db.as_agent("b").unwrap();
    assert_eq!(code(b.fork("wa", "pin", serde_json::Value::Null).unwrap_err()), "42501");
    b.fork(MAIN, "wb", serde_json::Value::Null).unwrap();
    b.fork("wb", "wb2", serde_json::Value::Null).unwrap();
    drop(b);
    let mut s = Simulate::new("wa", 2, "p", "select 1", "select 1");
    s.agent = Some("b".into());
    assert_eq!(code(db.simulate(&s).unwrap_err()), "42501");
    let mut session = Session::new(MAIN);
    session.agent = Some("b".into());
    assert_eq!(session.run(&db, "create world pin2 from wa", &[]).unwrap_err().code, "42501");
    db.discard("wa").unwrap(); // nothing pins it
}

/// An agent can't set the database's own metadata keys on a world it forks.
#[test]
fn fork_metadata_keeps_the_databases_keys() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let me = db.as_agent("bot").unwrap();
    for meta in [
        json!({"_sonos_active": 99_999_999_999_999u64}),
        json!({"_sonos_checkpoints": {"x": 1}}),
        json!({"_sonos_owner": "x"}),
    ] {
        assert_eq!(code(me.fork(MAIN, "w", meta).unwrap_err()), "22023");
    }
    assert!(db.world("w").is_err());
}

/// A dropped agent's worlds aren't inherited by a new agent of the same name.
#[test]
fn agent_names_are_not_reused_while_they_own_worlds() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    db.as_agent("bot").unwrap().fork(MAIN, "w", serde_json::Value::Null).unwrap();
    db.drop_agent("bot").unwrap();
    assert_eq!(code(db.create_agent("bot", &AgentSpec::default()).unwrap_err()), "22023");
    db.discard("w").unwrap();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
}

/// A panic inside a write (here the caller's own closure) doesn't make later writes panic.
#[test]
fn a_panic_in_a_write_does_not_poison_the_world() {
    let db = Db::new();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        type Rows = Vec<(String, Option<String>)>;
        let _ = db.batch_with(MAIN, |_| -> Result<(Rows, ()), chronos::Error> { panic!("a bug") });
    }));
    assert!(r.is_err());
    db.put(MAIN, "k/1", "{}").unwrap();
    db.fork(MAIN, "w").unwrap();
    db.put("w", "k/2", "{}").unwrap();
    db.merge("w", Resolve::Fail).unwrap();
}

/// A huge k (or offset) is clamped to the candidates, not allocated for.
#[test]
fn huge_k_and_offset() {
    let dim = 64;
    let mut ix = VecIndex::new(dim);
    let mut x: u64 = 1;
    let mut rnd = || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((x >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };
    for _ in 0..70_000 {
        let v: Vec<f32> = (0..dim).map(|_| rnd()).collect();
        ix.add(&v, &[]);
    }
    let q: Vec<f32> = (0..dim).map(|_| rnd()).collect();
    assert_eq!(ix.search(&q, &[], usize::MAX / 2, 64).len(), 70_000);
    assert!(ix.search(&q, &[], 0, 64).is_empty(), "k = 0 finds nothing");
    assert!(ix.exact(&q, &[], 0).is_empty());

    let db = Db::new();
    for i in 0..20 {
        db.put(MAIN, &format!("items/{i}"), &json!({ "e": [i as f32, 1.0, 0.5] }).to_string()).unwrap();
    }
    let mut q = Query {
        table: "items".into(),
        vector: Some(("e".into(), vec![1.0, 0.0, 0.0])),
        k: usize::MAX / 2,
        ..Default::default()
    };
    assert_eq!(db.find(MAIN, &q).unwrap().1.len(), 20);
    q.offset = usize::MAX;
    assert!(db.find(MAIN, &q).unwrap().1.is_empty());
    col(&db, "create table v (id int primary key, e vector(2))");
    col(&db, "create index on v using hnsw (e vector_l2_ops)");
    col(&db, "insert into v values (1, '[1,0]'), (2, '[0,1]')");
    assert_eq!(col(&db, "select id from v order by e <-> '[1,0]' limit 100000000000 offset 0").len(), 2);
}

/// `<->` and `<=>` rank by their own metric, whatever the vectors' lengths.
#[test]
fn distances_rank_by_their_metric() {
    let db = Db::new();
    col(&db, "create table m (id int primary key, v vector(2))");
    col(&db, "create index on m using hnsw (v vector_l2_ops)");
    col(&db, "insert into m values (1, '[10,10]'), (2, '[1,0.1]'), (3, '[-5,0]')");
    // by dot product 1 is nearest [1,0]; by distance and by angle, 2
    assert_eq!(col(&db, "select id from m order by v <-> '[1,0]' limit 1"), ["2"]);
    assert_eq!(col(&db, "select id from m order by v <=> '[1,0]' limit 1"), ["2"]);
    assert_eq!(col(&db, "select id from m order by v <#> '[1,0]' limit 1"), ["1"]);
    // a query of another dimension is an error, as in pgvector
    assert_eq!(
        sql::run(&db, MAIN, "select id from m order by v <-> '[1,0,0]' limit 1", &[]).unwrap_err().code,
        "22000"
    );
}

/// A JSON field's vectors take their most common length (a stray short array doesn't decide it);
/// numbers too big for an f32 aren't a vector; a query must be finite and of that length.
#[test]
fn json_vectors_dimensions_and_finiteness() {
    let db = Db::new();
    db.put(MAIN, "items/0", r#"{"e": [1, 0]}"#).unwrap();
    for (i, v) in [(1, [1.0, 0.0, 0.0]), (2, [0.0, 1.0, 0.0]), (3, [0.5, 0.5, 0.0])] {
        db.put(MAIN, &format!("items/{i}"), &json!({ "e": v }).to_string()).unwrap();
    }
    db.put(MAIN, "items/4", r#"{"e": [1e39, 0, 0]}"#).unwrap();
    let q = |v: Vec<f32>| Query { table: "items".into(), vector: Some(("e".into(), v)), k: 10, ..Default::default() };
    let keys: Vec<String> = db.find(MAIN, &q(vec![1.0, 0.0, 0.0])).unwrap().1.into_iter().map(|h| h.key).collect();
    assert_eq!(keys, ["items/1", "items/3", "items/2"]);
    assert_eq!(code(db.find(MAIN, &q(vec![1.0, 0.0])).unwrap_err()), "22000");
    assert_eq!(code(db.find(MAIN, &q(vec![f32::INFINITY, 0.0, 0.0])).unwrap_err()), "22023");
}

/// The graph's beam is tuned on real vectors, not on the zeros standing in for rows without one.
#[test]
fn calibration_skips_padding() {
    let dim = 16;
    let mut ix = VecIndex::new(dim);
    ix.pad_to(1000);
    let mut x: u64 = 3;
    let mut rnd = || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((x >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    };
    for _ in 0..300 {
        let v: Vec<f32> = (0..dim).map(|_| rnd()).collect();
        let n = v.iter().map(|a| a * a).sum::<f32>().sqrt();
        ix.add(&v.iter().map(|a| a / n).collect::<Vec<_>>(), &[]);
    }
    assert!(ix.build_graph());
    let r = ix.graph_recall().unwrap();
    // the graph builds on many threads, so recall varies run to run; zero probes measured ~0.17
    assert!(r >= 0.5, "recall {r}: measured on zero probes?");
}
