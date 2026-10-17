//! Simulations: many worlds forked from one moment, the same script run in each in parallel,
//! scored, the best kept; the same inputs give the same worlds on any number of threads, and a
//! kept world can be replayed from history to check it.

use chronos::sql::{self, Value};
use chronos::{AgentSpec, Db, MAIN, Simulate};

fn code(e: chronos::Error) -> &'static str {
    match e {
        chronos::Error::Constraint { code, .. } => code,
        e => panic!("not a constraint error: {e}"),
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chronos-sim-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `rows` accounts with a balance of 100 each.
fn accounts(db: &Db, rows: usize) {
    sql::run(db, MAIN, "create table accounts (id bigint primary key, bal double precision)", &[]).unwrap();
    sql::run(db, MAIN, &format!("insert into accounts select g, 100 from generate_series(1, {rows}) g"), &[]).unwrap();
}

/// Every account's balance moves by a random amount; a log row gets a random UUID and the time.
const SCRIPT: &str = "update accounts set bal = bal + (random() - 0.5) * 10;
    create table if not exists runs (id text primary key, idx bigint, at timestamptz);
    insert into runs values (gen_random_uuid(), $1, now())";
const SCORE: &str = "select sum(bal) from accounts";

fn scores(r: &chronos::Simulated) -> Vec<(usize, Option<f64>)> {
    let mut v: Vec<_> = r.worlds.iter().map(|w| (w.index, w.score)).collect();
    v.sort_by_key(|(i, _)| *i);
    v
}

#[test]
fn same_seed_same_worlds_on_any_number_of_threads() {
    let dir = tmp("threads");
    let db = Db::open(&dir).unwrap();
    accounts(&db, 10_000); // past the size where scans split across threads
    let run = |base: &str, prefix: &str, seed: i64, threads: usize| {
        let mut s = Simulate::new(base, 6, prefix, SCRIPT, SCORE);
        (s.seed, s.threads) = (seed, threads);
        db.simulate(&s).unwrap()
    };
    let a = run(MAIN, "a", 42, 1);
    // from the same moment of main, so now() is the same too
    let then = sql::run(&db, MAIN, "select to_timestamp($1 / 1000.0)", &[Value::Int(a.at as i64)]).unwrap();
    let base = format!("main@{}", then[0].rows[0][0].to_text());
    let b = run(&base, "b", 42, 8);
    let c = run(&base, "c", 7, 0);
    assert_eq!((b.at, b.base.as_str()), (a.at, MAIN));
    assert!(a.worlds.iter().all(|w| w.error.is_none() && w.kept), "{:?}", a.worlds);
    assert_eq!(scores(&a), scores(&b), "same seed, 1 thread or 8");
    assert_ne!(scores(&a), scores(&c), "another seed");
    let distinct: std::collections::HashSet<u64> = a.worlds.iter().map(|w| w.score.unwrap().to_bits()).collect();
    assert_eq!(distinct.len(), 6, "each world has its own numbers");
    for i in 0..6 {
        let d = db.diff_between(&format!("a_{i}"), &format!("b_{i}")).unwrap();
        assert!(d.is_empty(), "world {i}: rows, UUIDs and times are the same: {d:?}");
    }
    assert!(!db.diff_between("a_0", "c_0").unwrap().is_empty());
    // now() is the base moment in every world
    let at = sql::run(&db, "a_0", "select extract(epoch from at) * 1000 from runs", &[]).unwrap();
    // (extract gives a numeric, as Postgres 14+ does, with the six decimals an epoch has)
    assert_eq!(at[0].rows[0][0].to_text(), format!("{}.000000", a.at), "{:?}", at[0].rows);
    // $1 is the world's index
    let idx = sql::run(&db, "a_4", "select idx from runs", &[]).unwrap();
    assert_eq!(idx[0].rows, vec![vec![Value::Int(4)]]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn keeps_the_best_and_reports_failures() {
    let db = Db::new();
    accounts(&db, 50);
    // world 3 fails part way; the others carry on
    let script = format!("{SCRIPT}; select 1 / ($1 - 3)");
    let mut s = Simulate::new(MAIN, 20, "k", &script, SCORE);
    (s.keep, s.seed) = (Some(3), 5);
    let r = db.simulate(&s).unwrap();
    assert_eq!(r.worlds.len(), 20);
    let failed = r.worlds.iter().find(|w| w.index == 3).unwrap();
    assert!(failed.score.is_none() && failed.error.as_deref().unwrap().contains("22012"), "{failed:?}");
    assert_eq!(r.worlds.last().unwrap().index, 3, "failed worlds come last");
    let kept: Vec<_> = r.worlds.iter().filter(|w| w.kept).collect();
    assert_eq!(kept.len(), 3);
    assert_eq!(
        kept.iter().map(|w| w.index).collect::<Vec<_>>(),
        r.worlds[..3].iter().map(|w| w.index).collect::<Vec<_>>()
    );
    let best: Vec<f64> = r.worlds.iter().filter_map(|w| w.score).collect();
    assert!(best.windows(2).all(|p| p[0] >= p[1]), "best (highest) first");
    let live: Vec<String> = db.worlds().into_iter().map(|w| w.name).filter(|n| n.starts_with("k_")).collect();
    assert_eq!(live.len(), 3, "exactly the kept worlds are left: {live:?}");
    let sim = &db.world(&kept[0].name).unwrap().meta["sim"];
    assert_eq!((sim["seed"].as_i64(), sim["index"].as_u64()), (Some(5), Some(kept[0].index as u64)));
    assert_eq!(sim["score"].as_f64(), kept[0].score);

    // ascending: the lowest first; keep all keeps failed worlds too
    let mut s = Simulate::new(MAIN, 8, "lo", &script, SCORE);
    s.ascending = true;
    let r = db.simulate(&s).unwrap();
    let low: Vec<f64> = r.worlds.iter().filter_map(|w| w.score).collect();
    assert!(low.windows(2).all(|p| p[0] <= p[1]));
    assert!(r.worlds.iter().all(|w| w.kept) && db.world("lo_3").is_ok());

    // names in use, world statements in the script and scores that aren't one query are refused
    assert_eq!(code(db.simulate(&Simulate::new(MAIN, 2, "lo", SCRIPT, SCORE)).unwrap_err()), "42710");
    assert_eq!(code(db.simulate(&Simulate::new(MAIN, 2, "x", "merge world main", SCORE)).unwrap_err()), "0A000");
    assert_eq!(code(db.simulate(&Simulate::new(MAIN, 2, "x", SCRIPT, "delete from accounts")).unwrap_err()), "22023");
    assert!(db.world("x_0").is_err(), "nothing forked");
    // a score that isn't a number is that world's error
    let r = db.simulate(&Simulate::new(MAIN, 1, "t", SCRIPT, "select 'high'")).unwrap();
    assert!(r.worlds[0].error.as_deref().unwrap().contains("not a number"), "{:?}", r.worlds[0]);
}

#[test]
fn replay_rebuilds_a_world_from_history() {
    let dir = tmp("replay");
    let db = Db::open(&dir).unwrap();
    accounts(&db, 100);
    let mut s = Simulate::new(MAIN, 10, "r", SCRIPT, SCORE);
    (s.keep, s.seed) = (Some(2), 99);
    let r = db.simulate(&s).unwrap();
    let best = r.worlds[0].name.clone();
    // main moves on; the replay still forks it as it was
    sql::run(&db, MAIN, "update accounts set bal = 0 where id < 50", &[]).unwrap();
    let again = db.replay(&best, None, None).unwrap();
    assert!(again.identical, "{again:?}");
    assert_eq!((again.rows_differing, again.score, again.recorded), (0, r.worlds[0].score, r.worlds[0].score));
    assert_eq!(db.worlds().iter().filter(|w| w.name.starts_with("_replay")).count(), 0, "nothing left behind");

    // SQL, keeping the replay
    let out = sql::run(&db, MAIN, &format!("replay world {best} as {best}_again"), &[]).unwrap();
    assert_eq!(out[0].rows[0][2], Value::Bool(true), "{:?}", out[0].rows);
    assert!(db.diff_between(&best, &format!("{best}_again")).unwrap().is_empty());

    // a world changed since isn't what its inputs give
    db.put(&best, "accounts/1", r#"{"id": 1, "bal": 1}"#).unwrap();
    let changed = db.replay(&best, None, None).unwrap();
    assert!(!changed.identical && changed.rows_differing == 1, "{changed:?}");
    // and its inputs are the simulation's alone: replay runs them, so no one else sets them
    let other = r.worlds[1].name.clone();
    let mut sim = db.world(&other).unwrap().meta["sim"].clone();
    sim["seed"] = 100.into();
    assert!(db.set_meta(&other, serde_json::json!({ "sim": sim })).is_err());
    assert!(db.replay(&other, None, None).unwrap().identical);

    // across a restart and a checkpoint too
    drop(db);
    let db = Db::open(&dir).unwrap();
    db.checkpoint().unwrap();
    assert!(db.replay(&format!("{best}_again"), None, None).unwrap().identical);

    // a world that no simulation made, and a database with no history
    assert_eq!(code(db.replay(MAIN, None, None).unwrap_err()), "22023");
    let mem = Db::new();
    accounts(&mem, 10);
    mem.simulate(&Simulate::new(MAIN, 1, "m", SCRIPT, SCORE)).unwrap();
    let e = mem.replay("m_0", None, None).unwrap_err().to_string();
    assert!(e.contains("can't be replayed") && e.contains("in memory"), "{e}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn from_a_past_moment() {
    let dir = tmp("past");
    let db = Db::open(&dir).unwrap();
    accounts(&db, 20);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let then = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    std::thread::sleep(std::time::Duration::from_millis(5));
    sql::run(&db, MAIN, "delete from accounts where id > 10", &[]).unwrap();
    let when =
        sql::run(&db, MAIN, "select to_timestamp($1 / 1000.0)", &[Value::Int(then)]).unwrap()[0].rows[0][0].to_text();
    let base = format!("main@{when}");
    let r = db.simulate(&Simulate::new(&base, 3, "p", SCRIPT, "select count(*) from accounts")).unwrap();
    assert!(r.worlds.iter().all(|w| w.score == Some(20.0)), "the 20 accounts of then: {:?}", r.worlds);
    assert_eq!(db.world("p_0").unwrap().parent.as_deref(), Some(MAIN));
    assert!(db.replay("p_1", None, None).unwrap().identical);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sql_simulate() {
    let db = Db::new();
    accounts(&db, 30);
    let q = "simulate 12 worlds from main as trial run $$update accounts set bal = bal + random() * $1$$ \
             score $$select sum(bal) from accounts$$ keep 4 seed 42 threads 3";
    let a = sql::run(&db, MAIN, q, &[]).unwrap();
    let names: Vec<&str> = a[0].columns.as_ref().unwrap().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["world", "id", "index", "seed", "score", "error", "kept"]);
    assert_eq!(a[0].rows.len(), 12);
    assert_eq!(a[0].rows.iter().filter(|r| r[6] == Value::Bool(true)).count(), 4);
    assert_eq!(a[0].tag, "SIMULATE 12");
    // the same statement again (other names) gives the same scores
    let b = sql::run(&db, MAIN, &q.replace("as trial", "as trial2"), &[]).unwrap();
    let s = |o: &sql::Output| o.rows.iter().map(|r| (r[2].clone(), r[4].clone())).collect::<Vec<_>>();
    assert_eq!(s(&a[0]), s(&b[0]));
    // FROM defaults to the session's world; not inside a transaction
    let mut session = sql::Session::new(MAIN);
    session.run(&db, "simulate 2 worlds as d run 'select 1' score 'select 1' keep all", &[]).unwrap();
    assert!(db.world("d_1").is_ok());
    session.run(&db, "begin", &[]).unwrap();
    let e = session.run(&db, "simulate 2 worlds as e run 'select 1' score 'select 1'", &[]).unwrap_err();
    assert_eq!(e.code, "25001");
    session.close(&db);
    let m = sql::run(&db, MAIN, "show metrics", &[]).unwrap();
    assert!(
        m[0].rows.iter().any(|r| r[0] == Value::Text("simulate".into()) && r[1] == Value::Int(3)),
        "{:?}",
        m[0].rows
    );
}

#[test]
fn setseed_makes_random_repeatable() {
    let db = Db::new();
    let draw = |seed: &str| {
        let mut s = sql::Session::new(MAIN);
        let out =
            s.run(&db, &format!("select setseed({seed}); select random(), random(), gen_random_uuid()"), &[]).unwrap();
        // it lasts for the session, across statements
        let next = s.run(&db, "select random()", &[]).unwrap();
        (out[1].rows[0].clone(), next[0].rows[0].clone())
    };
    assert_eq!(draw("0.5"), draw("0.5"));
    assert_ne!(draw("0.5"), draw("-0.25"));
    let (row, _) = draw("0.5");
    assert!(matches!(row[0], Value::Float(f) if (0.0..1.0).contains(&f)) && row[0] != row[1]);
    let e = sql::run(&db, MAIN, "select setseed(2)", &[]).unwrap_err();
    assert_eq!(e.code, "22003");
    // unseeded, random() differs run to run
    let r = || sql::run(&db, MAIN, "select random()", &[]).unwrap()[0].rows[0][0].clone();
    assert_ne!(r(), r());
    // a big seeded UPDATE gives the same rows each time, whatever the machine's cores
    accounts(&db, 10_000);
    let upd = |w: &str| {
        db.fork(MAIN, w).unwrap();
        let mut s = sql::Session::new(w);
        s.run(&db, "select setseed(0.1); update accounts set bal = random()", &[]).unwrap();
    };
    upd("s1");
    upd("s2");
    assert!(db.diff_between("s1", "s2").unwrap().is_empty());
}

#[test]
fn agents_rights_and_quotas() {
    let db = Db::new();
    accounts(&db, 10);
    let spec = AgentSpec { max_worlds: Some(5), ..Default::default() };
    db.create_agent("bot", &spec).unwrap();
    db.as_agent("bot").unwrap().fork(MAIN, "mine", serde_json::Value::Null).unwrap(); // owns 1 already
    let as_bot = |n: usize, keep: Option<usize>| {
        let mut s =
            Simulate::new(MAIN, n, &format!("b{n}{}", keep.map_or("all".into(), |k| k.to_string())), SCRIPT, SCORE);
        (s.keep, s.agent) = (keep, Some("bot".into()));
        db.simulate(&s)
    };
    // 20 worlds, keeping 2: run in batches of 2 (5 - 1 owned - 2 kept)
    let r = as_bot(20, Some(2)).unwrap();
    assert_eq!(r.worlds.iter().filter(|w| w.score.is_some()).count(), 20);
    let kept: Vec<_> = db.worlds().into_iter().filter(|w| w.name.starts_with("b202_")).collect();
    assert_eq!(kept.len(), 2);
    assert!(kept.iter().all(|w| w.owner.as_deref() == Some("bot")), "its worlds are the agent's");
    // keeping all 20, or 2 more (2 + 1 run needs 3 of the 5 - 3 owned), is refused before anything runs
    let e = as_bot(20, None).unwrap_err();
    assert!(e.to_string().contains("at most 5 worlds"), "{e}");
    assert_eq!(code(e), "42501");
    assert_eq!(code(as_bot(4, Some(2)).unwrap_err()), "42501");
    assert!(db.world("b42_0").is_err());
    // a row quota applies in each world: the script's writes fail there, and are reported
    db.alter_agent("bot", &AgentSpec { max_changes: Some(3), max_worlds: Some(0), ..Default::default() }).unwrap();
    let r = as_bot(3, None).unwrap();
    assert!(r.worlds.iter().all(|w| w.error.as_deref().is_some_and(|e| e.contains("42501"))), "{:?}", r.worlds);
    // an agent that may not fork can't simulate
    db.create_agent("reader", &AgentSpec { can: Some(vec![chronos::Can::Read]), ..Default::default() }).unwrap();
    let mut s = Simulate::new(MAIN, 2, "rd", SCRIPT, SCORE);
    s.agent = Some("reader".into());
    assert_eq!(code(db.simulate(&s).unwrap_err()), "42501");
    // over SQL, as the agent
    let mut session = sql::Session::new(MAIN);
    session.agent = Some("bot".into());
    session.run(&db, "simulate 2 worlds as sq run 'select 1' score 'select 2' keep 1", &[]).unwrap();
    assert_eq!(db.world("sq_0").unwrap().owner.as_deref(), Some("bot"));
    assert!(db.world("sq_1").is_err());
}

#[test]
fn two_thousand_worlds() {
    let db = Db::new();
    sql::run(&db, MAIN, "create table t (id bigint primary key, v double precision)", &[]).unwrap();
    let t = std::time::Instant::now();
    let mut s = Simulate::new(MAIN, 2000, "big", "insert into t values ($1, random())", "select max(v) from t");
    s.keep = Some(5);
    let r = db.simulate(&s).unwrap();
    eprintln!(
        "2,000 worlds: {:.0} ms (fork {:.0}, run {:.0}, discard {:.0})",
        r.total_ms, r.fork_ms, r.run_ms, r.discard_ms
    );
    assert_eq!(r.worlds.len(), 2000);
    assert!(r.worlds.iter().all(|w| w.error.is_none()));
    assert_eq!(db.worlds().iter().filter(|w| w.name.starts_with("big_")).count(), 5);
    assert!(t.elapsed().as_secs() < 120);
}

/// `cargo test --release --test simulate ten_thousand -- --ignored --nocapture`: time for 10,000
/// worlds (measure memory with /usr/bin/time -l).
#[test]
#[ignore]
fn ten_thousand_worlds() {
    let db = Db::new();
    accounts(&db, 1000);
    let mut s = Simulate::new(MAIN, 10_000, "ten", SCRIPT, SCORE);
    s.keep = Some(10);
    let r = db.simulate(&s).unwrap();
    eprintln!(
        "10,000 worlds, 1,000 rows updated in each: {:.0} ms (fork {:.0}, run {:.0}, discard {:.0})",
        r.total_ms, r.fork_ms, r.run_ms, r.discard_ms
    );
    assert!(r.worlds.iter().all(|w| w.error.is_none()));
    sql::run(&db, MAIN, "create table t (id bigint primary key, v double precision)", &[]).unwrap();
    let mut s = Simulate::new(MAIN, 10_000, "one", "insert into t values ($1, random())", "select max(v) from t");
    s.keep = Some(10);
    let r = db.simulate(&s).unwrap();
    eprintln!(
        "10,000 worlds, a row inserted in each: {:.0} ms (fork {:.0}, run {:.0}, discard {:.0})",
        r.total_ms, r.fork_ms, r.run_ms, r.discard_ms
    );
}
