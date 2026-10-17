//! How merges end (`SHOW MERGE OUTCOMES`, `Core::merge_outcomes`): worlds by how they ended, and how often their
//! merges were held or refused, by agent and by policy, read back from the log. A known mix of 100 worlds gives
//! exactly its counts, before and after a restart and a kill -9; a retried merge moves attempts, never worlds; a
//! transaction's conflict isn't an agent's; only the database's own users may look.

mod common;

use chronos::sql::{Session, run};
use chronos::{Db, MAIN, Outcome};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-merge-stats-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Leads 1 to 1000, a notes table, a policy holding deletes, and two agents: `bot` keeps to it, `free` to none.
fn setup(db: &Db) -> (Session, Session) {
    run(
        db,
        MAIN,
        "create table leads (id int primary key, status text);
         insert into leads select i, 'new' from generate_series(1, 1000) i;
         create table notes (id int primary key, body text);
         create merge policy careful with (max_deletes = 0);
         create merge check no_bad on tables (leads) as select id from leads where status = 'bad'",
        &[],
    )
    .unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'careful')"), &[]).unwrap();
    run(db, MAIN, &format!("create agent free with (can = '{can}')"), &[]).unwrap();
    let as_agent = |name: &str| {
        let mut s = Session::new(MAIN);
        s.agent = Some(name.into());
        s
    };
    (as_agent("bot"), as_agent("free"))
}

/// As `s`: makes world `w` (`with`, e.g. " with (check_reads = true)"), runs `sql` there, and comes back to main.
fn work(db: &Db, s: &mut Session, w: &str, with: &str, sql: &str) {
    s.run(db, &format!("create world {w}{with}; use world {w}; {sql}; use world main"), &[]).unwrap();
}

fn merge(db: &Db, s: &mut Session, w: &str) -> Result<(), String> {
    s.run(db, &format!("merge world {w}"), &[]).map(|_| ()).map_err(|e| format!("{} {}", e.code, e.message))
}

fn row<'a>(rows: &'a [Outcome], name: &str) -> &'a Outcome {
    rows.iter().find(|o| o.name == name).unwrap_or_else(|| panic!("no row for {name:?} in {rows:#?}"))
}

/// The mix: 60 of bot's worlds, 35 of free's, 5 of the database's own users'.
fn the_mix(db: &Db, bot: &mut Session, free: &mut Session) {
    // bot: 40 merge on their own
    for i in 0..40 {
        work(db, bot, &format!("ok{i}"), "", &format!("update leads set status = 'done' where id = {}", 1 + i));
        merge(db, bot, &format!("ok{i}")).unwrap();
    }
    // bot: 10 delete a row, so they wait; a person merges 6 and drops 4
    for i in 0..10 {
        work(db, bot, &format!("del{i}"), "", &format!("delete from leads where id = {}", 100 + i));
        let e = merge(db, bot, &format!("del{i}")).unwrap_err();
        assert!(e.contains("SHOW REVIEWS"), "{e}");
    }
    for i in 0..6 {
        run(db, MAIN, &format!("merge world del{i}"), &[]).unwrap();
    }
    for i in 6..10 {
        run(db, MAIN, &format!("drop world del{i}"), &[]).unwrap();
    }
    // bot: 5 conflict with main, and bot drops them
    for i in 0..5 {
        work(db, bot, &format!("clash{i}"), "", "update leads set status = 'bot' where id = 200");
        run(db, MAIN, &format!("update leads set status = 'main{i}' where id = 200"), &[]).unwrap();
        let e = merge(db, bot, &format!("clash{i}")).unwrap_err();
        assert!(e.contains("changed on both sides"), "{e}");
        bot.run(db, &format!("drop world clash{i}"), &[]).unwrap();
    }
    // bot: 5 left open
    for i in 0..5 {
        work(db, bot, &format!("idle{i}"), "", &format!("update leads set status = 'later' where id = {}", 300 + i));
    }
    // free: 20 merge (no policy to keep to)
    for i in 0..20 {
        work(db, free, &format!("f{i}"), "", &format!("update leads set status = 'done' where id = {}", 400 + i));
        merge(db, free, &format!("f{i}")).unwrap();
    }
    // free: 5 relied on a row main changed since; each refused twice, then left open
    for i in 0..5 {
        let id = 500 + i;
        work(
            db,
            free,
            &format!("stale{i}"),
            " with (check_reads = true)",
            &format!("select status from leads where id = {id}; insert into notes values ({id}, 'saw it')"),
        );
        run(db, MAIN, &format!("update leads set status = 'moved' where id = {id}"), &[]).unwrap();
        for _ in 0..2 {
            let e = merge(db, free, &format!("stale{i}")).unwrap_err();
            assert!(e.starts_with("40001"), "{e}");
        }
    }
    // free: 5 break the merge check, and free drops them
    for i in 0..5 {
        work(db, free, &format!("bad{i}"), "", &format!("update leads set status = 'bad' where id = {}", 600 + i));
        let e = merge(db, free, &format!("bad{i}")).unwrap_err();
        assert!(e.starts_with("23514"), "{e}");
        free.run(db, &format!("drop world bad{i}"), &[]).unwrap();
    }
    // free: 5 merged in part
    for i in 0..5 {
        work(
            db,
            free,
            &format!("part{i}"),
            "",
            &format!(
                "update leads set status = 'done' where id = {}; insert into notes values ({}, 'kept')",
                700 + i,
                700 + i
            ),
        );
        free.run(db, &format!("merge world part{i} only tables (leads)"), &[]).unwrap();
    }
    // the database's own users: 5 merge
    for i in 0..5 {
        run(db, MAIN, &format!("create world own{i}"), &[]).unwrap();
        run(db, &format!("own{i}"), &format!("update leads set status = 'own' where id = {}", 800 + i), &[]).unwrap();
        run(db, MAIN, &format!("merge world own{i}"), &[]).unwrap();
    }
}

fn check_the_mix(db: &Db) {
    let out = db.merge_outcomes(false).unwrap();
    assert!(out.since > 0);
    assert_eq!(out.rows.len(), 3, "{:#?}", out.rows);
    let bot = row(&out.rows, "bot");
    let want = Outcome {
        name: "bot".into(),
        policy: "careful".into(),
        worlds: 60,
        on_rules: 40,
        approved: 6,
        rejected: 4,
        discarded: 5,
        open: 5,
        conflicts: 5,
        holds: 10,
        ..Default::default()
    };
    assert_eq!(*bot, want);
    assert_eq!(bot.rules_rate(), Some(40.0 / 50.0));
    let free = row(&out.rows, "free");
    let want = Outcome {
        name: "free".into(),
        worlds: 35,
        no_policy: 20,
        discarded: 5,
        open: 5,
        partly_merged: 5,
        stale_reads: 10,
        check_refusals: 5,
        ..Default::default()
    };
    assert_eq!(*free, want);
    assert_eq!(free.rules_rate(), None, "nothing of free's was decided by rules or a person");
    let own = row(&out.rows, "");
    assert_eq!((own.worlds, own.no_policy, own.policy.as_str()), (5, 5, ""));
    assert_eq!(out.rows[0].name, "bot", "most worlds first");

    // by policy: careful's agents, and everyone keeping to none
    let by = db.merge_outcomes(true).unwrap();
    assert_eq!(by.rows.len(), 2, "{:#?}", by.rows);
    let careful = row(&by.rows, "careful");
    assert_eq!((careful.worlds, careful.on_rules, careful.holds, careful.policy.as_str()), (60, 40, 10, ""));
    let none = row(&by.rows, "");
    assert_eq!((none.worlds, none.no_policy, none.stale_reads, none.check_refusals), (40, 25, 10, 5));
}

#[test]
fn a_known_mix_of_merges_gives_exactly_its_counts_and_keeps_them_across_a_restart() {
    let dir = tmp("mix");
    {
        let db = Db::open(&dir).unwrap();
        let (mut bot, mut free) = setup(&db);
        the_mix(&db, &mut bot, &mut free);
        check_the_mix(&db);

        // a transaction that loses to a change made since its snapshot is no agent's merge (one thread: the other
        // change commits first, so nothing waits on a row lock)
        let mut a = Session::new(MAIN);
        a.run(&db, "begin isolation level repeatable read; select status from leads where id = 999", &[]).unwrap();
        run(&db, MAIN, "update leads set status = 'b' where id = 999", &[]).unwrap();
        // refused (Postgres: could not serialize) or not, it's no agent's merge and counts nothing
        let _ = a.run(&db, "update leads set status = 'a' where id = 999; commit", &[]);
        let _ = a.run(&db, "rollback", &[]);
        check_the_mix(&db);

        // the statement: by agent, then by policy
        let out = run(&db, MAIN, "show merge outcomes", &[]).unwrap().pop().unwrap();
        let cols: Vec<&str> = out.columns.as_ref().unwrap().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            cols,
            [
                "since",
                "agent",
                "policy",
                "worlds",
                "on_rules",
                "no_policy",
                "approved",
                "rejected",
                "discarded",
                "open",
                "partly_merged",
                "before_window",
                "conflicts",
                "stale_reads",
                "check_refusals",
                "holds",
                "effects_discarded",
                "rules_rate"
            ]
        );
        assert_eq!(out.rows.len(), 3);
        assert_eq!(out.rows[0][1].to_text(), "bot");
        assert_eq!(out.rows[0][3].to_text(), "60");
        assert_eq!(out.rows[0][17].to_text(), "0.8");
        let own = out.rows.iter().find(|r| r[1].to_text().is_empty()).unwrap();
        assert!(matches!(own[1], chronos::sql::Value::Null), "the database's own users show as NULL");
        let by = run(&db, MAIN, "show merge outcomes by policy", &[]).unwrap().pop().unwrap();
        assert_eq!(by.columns.as_ref().unwrap()[1].0, "policy");
        assert_eq!(by.columns.as_ref().unwrap().len(), 17);
        assert_eq!(run(&db, MAIN, "show merge outcomes by agent", &[]).unwrap().pop().unwrap().rows.len(), 3);
    }
    let db = Db::open(&dir).unwrap();
    check_the_mix(&db);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_merge_retried_fifty_times_counts_fifty_refusals_and_one_world() {
    let dir = tmp("retry");
    let db = Db::open(&dir).unwrap();
    let (_, mut free) = setup(&db);
    work(
        &db,
        &mut free,
        "again",
        " with (check_reads = true)",
        "select status from leads where id = 7; insert into notes values (7, 'x')",
    );
    run(&db, MAIN, "update leads set status = 'moved' where id = 7", &[]).unwrap();
    for _ in 0..50 {
        assert!(merge(&db, &mut free, "again").unwrap_err().starts_with("40001"));
    }
    let out = db.merge_outcomes(false).unwrap();
    let f = row(&out.rows, "free");
    assert_eq!((f.worlds, f.open, f.stale_reads), (1, 1, 50));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_the_database_s_own_users_see_the_outcomes() {
    let dir = tmp("who");
    let db = Db::open(&dir).unwrap();
    let (mut bot, _) = setup(&db);
    let e = bot.run(&db, "show merge outcomes", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    let e = bot.run(&db, "show merge outcomes by policy", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    // nor can anyone write the counts
    work(&db, &mut bot, "w", "", "update leads set status = 'x' where id = 1");
    for s in [&mut bot, &mut Session::new(MAIN)] {
        let e = s.run(&db, "alter world w set (_sonos_refusals = '{\"conflicts\": 0}')", &[]).unwrap_err();
        assert!(e.message.contains("kept by the database"), "{}", e.message);
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_in_memory_database_has_nothing_to_count() {
    let db = Db::new();
    let (mut bot, _) = setup(&db);
    work(&db, &mut bot, "w", "", "update leads set status = 'x' where id = 1");
    merge(&db, &mut bot, "w").unwrap();
    let out = db.merge_outcomes(false).unwrap();
    assert_eq!((out.since, out.rows.len()), (0, 0));
    assert!(run(&db, MAIN, "show merge outcomes", &[]).unwrap().pop().unwrap().rows.is_empty());
}

#[test]
fn an_encrypted_database_counts_the_same() {
    const KEY: [u8; 32] = [7; 32];
    let dir = tmp("crypt");
    {
        let db = Db::open_encrypted(&dir, &KEY).unwrap();
        let (mut bot, _) = setup(&db);
        work(&db, &mut bot, "ok", "", "update leads set status = 'done' where id = 1");
        merge(&db, &mut bot, "ok").unwrap();
        work(&db, &mut bot, "clash", "", "update leads set status = 'bot' where id = 2");
        run(&db, MAIN, "update leads set status = 'main' where id = 2", &[]).unwrap();
        merge(&db, &mut bot, "clash").unwrap_err();
    }
    let db = Db::open_encrypted(&dir, &KEY).unwrap();
    let out = db.merge_outcomes(false).unwrap();
    let b = row(&out.rows, "bot");
    assert_eq!((b.worlds, b.on_rules, b.open, b.conflicts), (2, 1, 1, 1));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_world_forked_before_the_window_is_counted_on_its_own() {
    let dir = tmp("window");
    let db = Db::open(&dir).unwrap();
    let (mut bot, _) = setup(&db);
    db.set_retention(0).unwrap(); // history starts at the next checkpoint
    work(&db, &mut bot, "old", "", "update leads set status = 'old' where id = 1");
    db.checkpoint().unwrap();
    work(&db, &mut bot, "new", "", "update leads set status = 'new2' where id = 2");
    merge(&db, &mut bot, "old").unwrap();
    merge(&db, &mut bot, "new").unwrap();
    let out = db.merge_outcomes(false).unwrap();
    let b = row(&out.rows, "bot");
    assert_eq!((b.worlds, b.before_window, b.on_rules), (2, 1, 1), "{b:#?}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

const KILL_CHILD: &str = "CHRONOS_MERGE_STATS_CHILD";

/// Run by `a_kill_during_merges_loses_no_merge`: bot merges world after world into main, saying each.
#[test]
fn kill_child() {
    let Ok(dir) = std::env::var(KILL_CHILD) else { return };
    let db = Db::open(&dir).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    for i in 0.. {
        work(
            &db,
            &mut bot,
            &format!("k{i}"),
            "",
            &format!("update leads set status = 'k' where id = {}", 1 + i % 1000),
        );
        merge(&db, &mut bot, &format!("k{i}")).unwrap();
        println!("MERGED {i}");
    }
}

/// kill -9 while an agent merges: every merge it was told went through is counted after recovery, and the counts
/// equal what the surviving log says (the audit's merges into main).
#[test]
fn a_kill_during_merges_loses_no_merge() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let dir = tmp("kill");
    {
        let db = Db::open(&dir).unwrap();
        setup(&db);
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "kill_child", "--nocapture", "--test-threads=1"])
        .env(KILL_CHILD, &dir)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut told = 0;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        if line.unwrap().contains("MERGED ") {
            told += 1;
            if told == 40 {
                break;
            }
        }
    }
    child.kill().unwrap(); // as kill -9: no close, no flush
    child.wait().unwrap();
    let db = Db::open(&dir).unwrap();
    let out = db.merge_outcomes(false).unwrap();
    let b = row(&out.rows, "bot");
    assert!(b.on_rules >= told, "{} merges counted, {told} were acknowledged", b.on_rules);
    let logged = db.audit(Some("bot"), usize::MAX).unwrap().iter().filter(|e| e.action == "merge into main").count();
    assert_eq!(b.on_rules, logged as u64, "the counts are what the log says");
    assert!(b.open <= 1, "at most the world being merged when it died is open: {b:#?}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `POST /v1/merge_outcomes`: the server's token gets the counts as JSON; an agent's is refused, as for /metrics.
#[test]
fn the_api_op_is_for_the_server_s_token() {
    use std::io::{Read, Write};
    const SERVER: &str = "SERVER-SERVER-SERVER-SERVER-0";
    let dir = tmp("api");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap(), "--token", SERVER]);
    /// Stops the server however the test ends.
    struct Server(std::process::Child, PathBuf);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    let s = common::serve(&mut cmd, false);
    let port = s.http;
    let _server = Server(s.child, dir.clone());
    let send = |token: &str, op: &str, body: Value| -> (u16, Value) {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let body = body.to_string();
        let req = format!(
            "POST /v1/{op} HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        c.write_all(req.as_bytes()).unwrap();
        let mut raw = String::new();
        c.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        (head.split(' ').nth(1).unwrap().parse().unwrap(), serde_json::from_str(body).unwrap_or(Value::Null))
    };
    let sql = |q: &str| send(SERVER, "sql", json!({ "sql": q }));
    sql(
        "create table t (id int primary key, v text); insert into t values (1, 'a'); create merge policy careful with (max_deletes = 0)",
    );
    let (_, made) = sql("create agent bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'careful')");
    let token = made["results"][0]["rows"][0]["token"].as_str().unwrap().to_string();
    let (st, _) = send(&token, "fork", json!({ "from": "main", "name": "w" }));
    assert_eq!(st, 200);
    let (st, _) = send(&token, "put", json!({ "branch": "w", "table": "t", "id": "1", "record": { "v": "b" } }));
    assert_eq!(st, 200);
    let (st, _) = send(&token, "merge", json!({ "branch": "w" }));
    assert_eq!(st, 200);

    let (st, body) = send(&token, "merge_outcomes", json!({}));
    assert_eq!(st, 403, "{body}");
    let (st, body) = send(SERVER, "merge_outcomes", json!({}));
    assert_eq!(st, 200, "{body}");
    assert!(body["since"].as_u64().unwrap() > 0, "{body}");
    let bot = &body["outcomes"][0];
    assert_eq!((bot["agent"].as_str(), bot["policy"].as_str()), (Some("bot"), Some("careful")), "{body}");
    assert_eq!((bot["worlds"].as_u64(), bot["on_rules"].as_u64()), (Some(1), Some(1)), "{body}");
    assert_eq!(bot["rules_rate"].as_f64(), Some(1.0), "{body}");
    let (st, body) = send(SERVER, "merge_outcomes", json!({ "by": "policy" }));
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["outcomes"][0]["policy"].as_str(), Some("careful"), "{body}");
    assert!(body["outcomes"][0].get("agent").is_none(), "{body}");
}

/// How long the count takes over a long log: worlds forked from a hub, written and merged back (no wait for the disk:
/// the hub isn't main) until the log holds a million records, and what one refusal adds to it. Run on the test VM:
/// `cargo test --release --test merge_stats -- --ignored --nocapture`.
#[test]
#[ignore]
fn the_count_at_a_million_log_records() {
    let dir = tmp("million");
    let db = Db::open(&dir).unwrap();
    let (_, mut free) = setup(&db);
    db.set_retention(365 * 86_400_000).unwrap(); // keep all of it in the window
    db.fork(MAIN, "hub").unwrap();
    let t = std::time::Instant::now();
    let (mut worlds, mut records) = (0u64, 0);
    while records < 1_000_000 {
        for _ in 0..10_000 {
            let name = format!("m{worlds}");
            db.fork("hub", &name).unwrap();
            db.put(&name, "k", &worlds.to_string()).unwrap();
            db.merge(&name, chronos::Resolve::Ours).unwrap();
            worlds += 1;
        }
        records = db.verify().unwrap().log_records;
    }
    eprintln!("{worlds} worlds made {records} log records in {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let out = db.merge_outcomes(false).unwrap();
    eprintln!("SHOW MERGE OUTCOMES over them: {:?}", t.elapsed());
    assert_eq!(row(&out.rows, "").no_policy, worlds);

    // what a refusal adds to the log, retried
    work(
        &db,
        &mut free,
        "again",
        " with (check_reads = true)",
        "select status from leads where id = 7; insert into notes values (7, 'x')",
    );
    run(&db, MAIN, "update leads set status = 'moved' where id = 7", &[]).unwrap();
    for _ in 0..100 {
        merge(&db, &mut free, "again").unwrap_err();
    }
    // each refusal logs the world's whole metadata once (an Op::Meta record)
    let meta = serde_json::to_string(&db.world("again").unwrap().meta).unwrap();
    eprintln!("each refused merge logs about {} bytes of metadata", meta.len());
    // whether those records count toward the agent's writes per minute (a quota would bound a retry loop)
    run(&db, MAIN, "alter agent free set (writes_per_minute = 1)", &[]).unwrap();
    let codes: Vec<String> =
        (0..5).map(|_| merge(&db, &mut free, "again").unwrap_err().split(' ').next().unwrap().to_string()).collect();
    eprintln!("5 more refused merges at writes_per_minute = 1: {codes:?}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
