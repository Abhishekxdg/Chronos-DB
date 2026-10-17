//! Second review, sequences: counters every world shares can't be moved back by an agent that
//! may not change main, a transaction's restarts wait for its COMMIT, and what a view calls
//! (setval, nextval, functions that write) counts when a statement is checked.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};
use std::io::Write;
use std::process::{Command, Stdio};

fn agent(db: &Db, name: &str, can: Vec<Can>) -> Session {
    db.create_agent(name, &AgentSpec { can: Some(can), ..Default::default() }).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some(name.into());
    s
}

fn code(s: &mut Session, db: &Db, sql: &str) -> &'static str {
    match s.run(db, sql, &[]) {
        Ok(_) => "ok",
        Err(e) => e.code,
    }
}

fn first(db: &Db, sql: &str) -> String {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    out.last().unwrap().rows[0][0].to_text()
}

#[test]
fn a_forked_agent_cant_move_mains_counters_back() {
    let db = Db::new();
    run(&db, MAIN, "create table orders (id serial primary key, v text); create sequence s", &[]).unwrap();
    run(&db, MAIN, "insert into orders (v) values ('a'), ('b'), ('c'); select nextval('s')", &[]).unwrap();
    let mut bot = agent(&db, "bot", vec![Can::Read, Can::Fork, Can::WriteOwn]);
    bot.run(&db, "fork world main as w; use world w", &[]).unwrap();
    assert_eq!(code(&mut bot, &db, "select setval('s', 1)"), "42501");
    assert_eq!(code(&mut bot, &db, "alter sequence s restart"), "42501");
    assert_eq!(code(&mut bot, &db, "create view back as select setval('s', 1) as x"), "ok");
    assert_eq!(code(&mut bot, &db, "select * from back"), "42501");
    // on only: past every value reserved, so no world gets one twice
    assert_eq!(code(&mut bot, &db, "select setval('s', 1000)"), "ok");
    // TRUNCATE ... RESTART IDENTITY empties its world, and the counter keeps counting
    assert_eq!(code(&mut bot, &db, "truncate orders restart identity"), "ok");
    assert_eq!(first(&db, "insert into orders (v) values ('d') returning id"), "4");
    assert_eq!(first(&db, "select count(*) from orders"), "4");
    assert_eq!(first(&db, "select nextval('s')"), "1001");
    // an agent that may change main may, as the database's own users may
    let mut boss = agent(&db, "boss", vec![Can::Read, Can::WriteMain]);
    assert_eq!(code(&mut boss, &db, "select setval('s', 5)"), "ok");
    assert_eq!(first(&db, "select nextval('s')"), "6");
}

#[test]
fn rollback_undoes_a_restart_and_commit_keeps_it() {
    let db = Db::new();
    run(&db, MAIN, "create table t (id serial primary key, v text); create sequence q", &[]).unwrap();
    run(&db, MAIN, "insert into t (v) values ('a'), ('b'); select nextval('q'), nextval('q')", &[]).unwrap();
    let mut s = Session::new(MAIN);
    s.run(&db, "begin; truncate t restart identity; alter sequence q restart; rollback", &[]).unwrap();
    assert_eq!(first(&db, "insert into t (v) values ('c') returning id"), "3");
    assert_eq!(first(&db, "select count(*) from t"), "3");
    assert_eq!(first(&db, "select nextval('q')"), "3");
    s.run(&db, "begin; savepoint a; alter sequence q restart; rollback to a; commit", &[]).unwrap();
    assert_eq!(first(&db, "select nextval('q')"), "4");
    s.run(&db, "begin; truncate t restart identity; alter sequence q restart; commit", &[]).unwrap();
    assert_eq!(first(&db, "insert into t (v) values ('d') returning id"), "1");
    assert_eq!(first(&db, "select nextval('q')"), "1");
}

#[test]
fn a_read_only_agent_cant_write_through_a_view() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table t (id int primary key); create sequence s;
         create function bump() returns int language sql as 'insert into t values (1) returning id';
         create view v1 as select setval('s', 50) as x;
         create view v2 as select bump() as x;
         create view v3 as select nextval('s') as x;
         create view v4 as select * from v2;
         create view fine as select id from t",
        &[],
    )
    .unwrap();
    let mut reader = agent(&db, "reader", vec![Can::Read]);
    for v in ["v1", "v2", "v3", "v4"] {
        assert_eq!(code(&mut reader, &db, &format!("select * from {v}")), "42501", "{v}");
        assert_eq!(code(&mut reader, &db, &format!("select 1 where exists (select * from {v})")), "42501", "{v}");
    }
    assert_eq!(code(&mut reader, &db, "select * from fine"), "ok");
    assert_eq!(first(&db, "select count(*) from t"), "0");
    assert_eq!(first(&db, "select nextval('s')"), "1");
    // the database's own users still write through them
    assert_eq!(first(&db, "select * from v2"), "1");
    assert_eq!(first(&db, "select count(*) from t"), "1");
}

#[test]
fn nextval_across_worlds_still_gives_unique_ids() {
    let db = Db::new();
    run(&db, MAIN, "create table t (id serial primary key, w text); create sequence s", &[]).unwrap();
    let mut seen = std::collections::HashSet::new();
    for w in ["a", "b"] {
        run(&db, MAIN, &format!("fork world main as {w}"), &[]).unwrap();
    }
    for i in 0..100 {
        for w in [MAIN, "a", "b"] {
            let out =
                run(&db, w, &format!("insert into t (w) values ('{w}{i}') returning id; select nextval('s')"), &[])
                    .unwrap();
            assert!(seen.insert(("t", out[0].rows[0][0].to_text())), "{w}: id again");
            assert!(seen.insert(("s", out[1].rows[0][0].to_text())), "{w}: nextval again");
        }
    }
    for w in ["a", "b"] {
        run(&db, MAIN, &format!("merge world {w}"), &[]).unwrap();
    }
    assert_eq!(first(&db, "select count(*) from t"), "300");
}

/// MCP's safe mode: a sequence's counter is every branch's, and a view can hide a write.
#[test]
fn mcp_safe_mode_refuses_counters_and_views_that_write() {
    let dir = std::env::temp_dir().join(format!("chronos-review2-seq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        run(
            &db,
            MAIN,
            "create table t (id int primary key); create sequence s;
             create function bump() returns int language sql as 'insert into t values (1) returning id';
             create view v as select bump() as x; create view n as select setval('s', 1) as x",
            &[],
        )
        .unwrap();
    }
    let call = |id: u32, args: &str| {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"sql","arguments":{args}}}}}"#)
    };
    let fork = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fork","arguments":{"name":"mine"}}}"#;
    let script = [
        fork.to_string(),
        call(2, r#"{"sql":"select * from v"}"#),
        call(3, r#"{"sql":"select setval('s', 1)","branch":"mine"}"#),
        call(4, r#"{"sql":"select * from n","branch":"mine"}"#),
        call(5, r#"{"sql":"alter sequence s restart","branch":"mine"}"#),
        call(6, r#"{"sql":"truncate t restart identity","branch":"mine"}"#),
        call(7, r#"{"sql":"select * from v","branch":"mine"}"#),
        call(8, r#"{"sql":"select count(*) from t"}"#),
    ];
    let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["mcp", dir.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(script.join("\n").as_bytes()).unwrap();
    let out = String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap();
    let r: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let text = |id: usize| r[id - 1]["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let refused = |id: usize, why: &str| {
        assert_eq!(r[id - 1]["result"]["isError"], true, "call {id} should be refused: {}", text(id));
        assert!(text(id).contains(why), "call {id}: {}", text(id));
    };
    refused(2, "changes need branch=");
    for id in 3..=6 {
        refused(id, "every branch shares their counters");
    }
    assert_eq!(r[6]["result"]["isError"], false, "{}", text(7)); // its own branch
    assert!(text(8).contains(" 0\n"), "main is unchanged: {}", text(8));
    std::fs::remove_dir_all(&dir).unwrap();
}
