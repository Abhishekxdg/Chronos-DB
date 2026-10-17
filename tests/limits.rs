//! Resource limits and world cleanup: agents' max_query_ms, max_concurrent and max_memory_mb;
//! the database's statement_timeout, max_worlds and world_idle_ttl; storage given back after
//! worlds are dropped; DROP WORLD ... CASCADE.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN, Setting};
use std::time::{Duration, Instant};

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-limits-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A table `t` of `n` rows (id, k = id % `groups`, v text).
fn table(db: &Db, t: &str, n: usize, groups: usize) {
    run(db, MAIN, &format!("create table {t} (id int primary key, k int, v text)"), &[]).unwrap();
    for chunk in (0..n).collect::<Vec<_>>().chunks(1000) {
        let rows: Vec<String> = chunk.iter().map(|i| format!("({i}, {}, 'value {i}')", i % groups)).collect();
        run(db, MAIN, &format!("insert into {t} values {}", rows.join(", ")), &[]).unwrap();
    }
}

fn session(agent: &str) -> Session {
    let mut s = Session::new(MAIN);
    s.agent = Some(agent.into());
    s
}

fn code(r: chronos::sql::Result<Vec<chronos::sql::Output>>) -> &'static str {
    r.map(|_| "ok").unwrap_or_else(|e| e.code)
}

/// Pairs of rows that never match: seconds of work, little memory.
const SLOW: &str = "select count(*) from t a, t b where a.v || b.v = 'nope'";

#[test]
fn runaway_queries_stop_at_the_agents_max_query_ms() {
    let db = Db::new();
    table(&db, "t", 4000, 10);
    let spec = AgentSpec { can: Some(vec![Can::Read]), max_query_ms: Some(200), ..Default::default() };
    db.create_agent("slow", &spec).unwrap();
    let mut s = session("slow");
    // it may set its own timeout, but not past its limit
    s.run(&db, "set statement_timeout = '1h'", &[]).unwrap();
    assert_eq!(s.run(&db, "show statement_timeout", &[]).unwrap()[0].rows[0][0].to_text(), "200ms");
    let t = Instant::now();
    let e = s.run(&db, SLOW, &[]).unwrap_err();
    assert_eq!(e.code, "57014", "{}", e.message);
    assert!(t.elapsed() < Duration::from_secs(3), "stopped near 200 ms, took {:?}", t.elapsed());
    // a lower one of its own wins
    s.run(&db, "set statement_timeout = 50", &[]).unwrap();
    assert_eq!(s.run(&db, "show statement_timeout", &[]).unwrap()[0].rows[0][0].to_text(), "50ms");
    assert_eq!(code(s.run(&db, "select count(*) from t", &[])), "ok");
    let shown = run(&db, MAIN, "show agents", &[]).unwrap();
    let cols: Vec<&str> = shown[0].columns.as_ref().unwrap().iter().map(|(n, _)| n.as_str()).collect();
    let at = cols.iter().position(|c| *c == "max_query_ms").unwrap();
    assert_eq!(shown[0].rows[0][at].to_text(), "200");
}

#[test]
fn statement_timeout_for_every_session_persists() {
    let d = dir("timeout");
    {
        let db = Db::open(&d).unwrap();
        table(&db, "t", 4000, 10);
        run(&db, MAIN, "alter system set statement_timeout = '150ms'", &[]).unwrap();
        let e = run(&db, MAIN, SLOW, &[]).unwrap_err();
        assert_eq!(e.code, "57014");
    }
    let db = Db::open(&d).unwrap();
    assert_eq!(db.setting(Setting::StatementTimeout), 150);
    let mut s = Session::new(MAIN);
    assert_eq!(s.run(&db, "show statement_timeout", &[]).unwrap()[0].rows[0][0].to_text(), "150ms");
    s.run(&db, "set statement_timeout = 0", &[]).unwrap(); // a session may turn it off, as in Postgres
    assert_eq!(s.run(&db, "show statement_timeout", &[]).unwrap()[0].rows[0][0].to_text(), "0");
    s.run(&db, "reset statement_timeout", &[]).unwrap();
    assert_eq!(s.run(&db, "show statement_timeout", &[]).unwrap()[0].rows[0][0].to_text(), "150ms");
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn max_concurrent_refuses_the_extra_statement() {
    let db = Db::new();
    table(&db, "t", 4000, 10);
    let spec = AgentSpec { can: Some(vec![Can::Read]), max_concurrent: Some(1), ..Default::default() };
    db.create_agent("one", &spec).unwrap();
    let mut long = session("one");
    let cancel = long.cancel.clone();
    std::thread::scope(|sc| {
        let running = sc.spawn(|| {
            loop {
                match long.run(&db, SLOW, &[]) {
                    Err(e) if e.code == "53300" => continue, // the other one got there first
                    r => return r.map(|_| ()).map_err(|e| e.code),
                }
            }
        });
        // the second statement is refused while the first runs
        let mut other = session("one");
        let start = Instant::now();
        loop {
            match other.run(&db, "select 1", &[]) {
                Err(e) => {
                    assert_eq!(e.code, "53300", "{}", e.message);
                    break;
                }
                Ok(_) => assert!(start.elapsed() < Duration::from_secs(10), "the long statement never started"),
            }
        }
        while !running.is_finished() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed); // until it's seen: a statement clears it as it starts
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(running.join().unwrap(), Err("57014"));
        other.run(&db, "select 1", &[]).unwrap(); // its place is free again
    });
}

#[test]
fn max_memory_mb_refuses_a_big_join_while_another_agents_query_completes() {
    let db = Db::new();
    table(&db, "a", 2000, 20);
    table(&db, "b", 2000, 20);
    let read = || Some(vec![Can::Read]);
    db.create_agent("small", &AgentSpec { can: read(), max_memory_mb: Some(8), ..Default::default() }).unwrap();
    db.create_agent("big", &AgentSpec { can: read(), ..Default::default() }).unwrap();
    // 200,000 joined rows: far past 8 MB held
    let join = "select a.id, b.id, a.v from a join b on a.k = b.k";
    std::thread::scope(|sc| {
        let small =
            sc.spawn(|| session("small").run(&db, join, &[]).map(|o| o[0].rows.len()).map_err(|e| (e.code, e.message)));
        let big = sc.spawn(|| session("big").run(&db, join, &[]).map(|o| o[0].rows.len()).map_err(|e| e.code));
        let (code, message) = small.join().unwrap().unwrap_err();
        assert_eq!(code, "53200", "{message}");
        assert!(message.contains("max_memory_mb"), "{message}");
        assert_eq!(big.join().unwrap(), Ok(200_000));
    });
    // a query within its limit runs; so does the next one (what the refused one held is let go)
    let mut s = session("small");
    assert_eq!(
        s.run(&db, "select count(*) from a join b on a.k = b.k", &[]).unwrap()[0].rows[0][0].to_text(),
        "200000"
    );
    assert_eq!(s.run(&db, "select id from a where k = 3 order by id limit 2", &[]).unwrap()[0].rows.len(), 2);
}

#[test]
fn max_worlds_counts_worlds_but_not_transactions() {
    let d = dir("max-worlds");
    {
        let db = Db::open(&d).unwrap();
        run(&db, MAIN, "create table t (id int primary key)", &[]).unwrap();
        run(&db, MAIN, "alter system set max_worlds = 2", &[]).unwrap();
        db.fork(MAIN, "a").unwrap();
        db.fork("a", "b").unwrap();
        match db.fork(MAIN, "c").unwrap_err() {
            chronos::Error::Constraint { code, message } => {
                assert!(code == "53400" && message.contains("max_worlds"), "{message}")
            }
            e => panic!("{e}"),
        }
        // a transaction's own world doesn't count
        let mut s = Session::new(MAIN);
        s.run(&db, "begin; insert into t values (1); savepoint x; insert into t values (2); commit", &[]).unwrap();
        db.discard("b").unwrap();
        db.fork(MAIN, "c").unwrap(); // room again
    }
    let db = Db::open(&d).unwrap();
    assert_eq!(run(&db, MAIN, "show max_worlds", &[]).unwrap()[0].rows[0][0].to_text(), "2");
    assert!(db.fork(MAIN, "d").is_err(), "kept across restarts");
    run(&db, MAIN, "alter system set max_worlds = 0", &[]).unwrap();
    db.fork(MAIN, "d").unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn idle_worlds_are_discarded_unless_pinned_or_forked_and_activity_survives_restarts() {
    let d = dir("idle");
    let before;
    {
        let db = Db::open(&d).unwrap();
        run(&db, MAIN, "alter system set world_idle_ttl = '7 days'", &[]).unwrap();
        assert_eq!(run(&db, MAIN, "show world_idle_ttl", &[]).unwrap()[0].rows[0][0].to_text(), "7 days");
        for w in ["old", "kept", "parent"] {
            db.fork(MAIN, w).unwrap();
            db.put(w, "t/1", "{}").unwrap();
        }
        db.fork("parent", "kid").unwrap();
        run(&db, MAIN, "alter world kept set pinned", &[]).unwrap();
        assert!(db.world("kept").unwrap().pinned);
        db.set_setting(Setting::WorldIdleTtl, 300).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        db.put("kid", "t/2", "{}").unwrap(); // the kid is busy; its parent isn't, but has a fork
        assert_eq!(db.expire_worlds().unwrap(), 1);
        let names: Vec<String> = db.worlds().into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["kept", "kid", "main", "parent"]);
        // the database did it, and says so
        let audit = db.audit(None, 10).unwrap();
        assert_eq!(
            (audit[0].agent.as_str(), audit[0].world.as_str(), audit[0].action.as_str()),
            ("system", "old", "discard")
        );

        // last activity survives restarts: from the checkpoint, and from the log after it
        db.checkpoint().unwrap();
        db.put("kid", "t/3", "{}").unwrap();
        before = [db.world("parent").unwrap().active, db.world("kid").unwrap().active];
    }
    std::thread::sleep(Duration::from_millis(100));
    let db = Db::open(&d).unwrap();
    let after = [db.world("parent").unwrap().active, db.world("kid").unwrap().active];
    for (b, a) in before.iter().zip(after) {
        assert!(a >= *b && a < b + 50, "{a} vs {b}: not reset by the restart");
    }
    assert_eq!(db.setting(Setting::WorldIdleTtl), 300);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(db.expire_worlds().unwrap(), 2, "the kid, then its parent");
    let names: Vec<String> = db.worlds().into_iter().map(|w| w.name).collect();
    assert_eq!(names, ["kept", "main"]);
    run(&db, MAIN, "alter world kept reset pinned", &[]).unwrap();
    assert_eq!(db.expire_worlds().unwrap(), 1);
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

fn folder_bytes(d: &std::path::Path) -> u64 {
    std::fs::read_dir(d)
        .unwrap()
        .flatten()
        .map(|e| if e.path().is_dir() { folder_bytes(&e.path()) } else { e.metadata().map_or(0, |m| m.len()) })
        .sum()
}

#[test]
fn dropped_worlds_give_their_storage_back_on_their_own() {
    let d = dir("reclaim");
    let db = Db::open(&d).unwrap();
    db.set_retention(0).unwrap();
    db.put(MAIN, "t/0", "{}").unwrap();
    let mut seed = 7u64;
    let mut noise = || {
        // text that doesn't compress away
        (0..25)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                format!("{:08x}", seed >> 32)
            })
            .collect::<String>()
    };
    for w in 0..25 {
        let name = format!("w{w}");
        db.fork(MAIN, &name).unwrap();
        let rows: Vec<(String, String)> =
            (0..5000).map(|i| (format!("t/{w}-{i}"), format!(r#"{{"text": "{}"}}"#, noise()))).collect();
        let rows: Vec<(&str, Option<&str>)> = rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect();
        db.batch(&name, &rows).unwrap();
    }
    db.checkpoint().unwrap();
    let full = folder_bytes(&d);
    let disk = run(&db, MAIN, "show disk", &[]).unwrap();
    let live = disk[0].rows.iter().find(|r| r[0].to_text() == "live").unwrap()[1].to_text().parse::<u64>().unwrap();
    assert!(live > 1 << 20, "the worlds' pages are in use: {live}");
    for w in 0..25 {
        db.discard(&format!("w{w}")).unwrap(); // 125,000 rows' worth: the worker checkpoints and cleans up
    }
    let start = Instant::now();
    while folder_bytes(&d) > full / 4 {
        assert!(start.elapsed() < Duration::from_secs(30), "still {} of {full} bytes", folder_bytes(&d));
        std::thread::sleep(Duration::from_millis(50));
    }
    let u = db.disk_usage().unwrap();
    assert!(u.live < live / 4 && u.history < live / 4, "{u:?}");
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn drop_world_cascade() {
    let d = dir("cascade");
    let db = Db::open(&d).unwrap(); // on disk: the audit trail is the log
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let mut s = session("bot");
    for q in ["create world a", "fork world a as b", "fork world b as c", "fork world a as d"] {
        s.run(&db, q, &[]).unwrap();
    }
    let e = s.run(&db, "drop world a", &[]).unwrap_err();
    assert!(e.code == "2BP01" && e.message.contains("CASCADE"), "{}", e.message);
    // someone else's fork under it (a person's: agents may fork only worlds they may change):
    // the bot may not drop that one
    db.fork("d", "e").unwrap();
    assert_eq!(s.run(&db, "drop world a cascade", &[]).unwrap_err().code, "42501");
    db.discard("e").unwrap();

    s.branch = "c".into();
    assert_eq!(s.run(&db, "drop world a cascade", &[]).unwrap()[0].tag, "DROP WORLD");
    assert_eq!(s.branch, MAIN, "its world was in the tree: it carries on from the tree's parent");
    let names: Vec<String> = db.worlds().into_iter().map(|w| w.name).collect();
    assert_eq!(names, ["main"]);
    // one audit entry each, deepest first
    let gone: Vec<String> =
        db.audit(Some("bot"), 10).unwrap().into_iter().filter(|e| e.action == "discard").map(|e| e.world).collect();
    assert_eq!(gone.len(), 4);
    assert_eq!(gone.last().unwrap(), "c", "the deepest went first");
    assert_eq!(gone.first().unwrap(), "a", "the root went last");
    assert!(run(&db, MAIN, "drop world if exists a cascade", &[]).is_ok());
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}
