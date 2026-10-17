//! Third review, sequences: an agent can't move main's counters back by redefining a sequence in
//! its own world, nor use them up; safe mode refuses counter moves wherever they run; a
//! transaction's queued restart never hands out an id twice; replay from the past needs restore.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN, Simulate};

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

fn forked_bot(db: &Db) -> Session {
    run(db, MAIN, "create sequence s; select nextval('s'), nextval('s'), nextval('s')", &[]).unwrap();
    let mut bot = agent(db, "bot", vec![Can::Read, Can::Fork, Can::WriteOwn]);
    bot.run(db, "fork world main as w; use world w", &[]).unwrap();
    bot
}

#[test]
fn redefining_a_sequence_in_a_fork_cant_rewind_mains_counter() {
    let db = Db::new();
    let mut bot = forked_bot(&db);
    assert_eq!(code(&mut bot, &db, "alter sequence s increment by -1 restart with 1"), "42501");
    assert_eq!(code(&mut bot, &db, "alter sequence s increment by -1"), "42501");
    assert_eq!(code(&mut bot, &db, "alter sequence s cycle"), "42501");
    assert_eq!(code(&mut bot, &db, "select setval('s', 1)"), "42501");
    // a bigger step the same way is fine, and still only moves on
    assert_eq!(code(&mut bot, &db, "alter sequence s increment by 5"), "ok");
    assert_eq!(code(&mut bot, &db, "select setval('s', 2)"), "42501");
    assert_eq!(first(&db, "select nextval('s')"), "4");
}

#[test]
fn a_fork_cant_use_up_mains_counter() {
    let db = Db::new();
    let mut bot = forked_bot(&db);
    assert_eq!(code(&mut bot, &db, "select setval('s', 9223372036854775807)"), "42501");
    assert_eq!(code(&mut bot, &db, "alter sequence s restart with 9223372036854775807"), "42501");
    assert_eq!(code(&mut bot, &db, "select setval('s', 100000000)"), "42501");
    assert_eq!(code(&mut bot, &db, "select setval('s', 1000)"), "ok");
    assert_eq!(first(&db, "select nextval('s')"), "1001");
}

#[test]
fn safe_mode_refuses_counter_moves_wherever_they_run() {
    let db = Db::new();
    run(&db, MAIN, "create sequence s; select nextval('s'), nextval('s'); create table t (id serial primary key)", &[])
        .unwrap();
    let _fixed = chronos::fixed_counters();
    for sql in [
        "select setval('s', 1)",
        "prepare p as select setval('s', 1); execute p",
        "create function f() returns bigint language sql as 'select setval(''s'', 1)'; select f()",
        "create view v as select setval('s', 1) as x; select * from v",
        "alter sequence s restart",
        "begin; alter sequence s restart; commit",
    ] {
        let e = run(&db, MAIN, sql, &[]).expect_err(sql);
        assert_eq!(e.code, "42501", "{sql}: {}", e.message);
    }
    // a simulation's worlds too
    let r = db.simulate(&Simulate::new(MAIN, 1, "sm", "select setval('s', 1)", "select 1")).unwrap();
    assert!(r.worlds[0].error.as_deref().is_some_and(|e| e.contains("42501")), "{:?}", r.worlds);
    drop(_fixed);
    assert_eq!(first(&db, "select nextval('s')"), "3");
}

#[test]
fn a_queued_restart_never_hands_out_an_id_twice() {
    let db = Db::new();
    run(&db, MAIN, "create table t (id serial primary key, v text); create sequence q", &[]).unwrap();
    run(&db, MAIN, "insert into t (v) values ('a'), ('b'), ('c'); select nextval('q'), nextval('q')", &[]).unwrap();
    let mut s = Session::new(MAIN);
    let out = s
        .run(&db, "begin; truncate t restart identity; insert into t (v) values ('x') returning id; commit", &[])
        .unwrap();
    let taken = out[2].rows[0][0].to_text();
    for _ in 0..5 {
        let id = first(&db, "insert into t (v) values ('y') returning id");
        assert_ne!(id, taken);
    }
    assert_eq!(first(&db, "select count(distinct id) from t"), "6");
    let out = s.run(&db, "begin; alter sequence q restart; select nextval('q'); commit", &[]).unwrap();
    let got = out[2].rows[0][0].to_text();
    assert_ne!(first(&db, "select nextval('q')"), got);
    // with nothing taken in between, COMMIT still restarts
    s.run(&db, "begin; alter sequence q restart; commit", &[]).unwrap();
    assert_eq!(first(&db, "select nextval('q')"), "1");
}

#[test]
fn replay_from_the_past_needs_restore() {
    let dir = std::env::temp_dir().join(format!("chronos-r3seq3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "create table a (id bigint primary key, v bigint); insert into a values (1, 1)", &[]).unwrap();
    db.create_agent("sim", &AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn]), ..Default::default() })
        .unwrap();
    let mut spec = Simulate::new(MAIN, 1, "rp", "update a set v = v + 1", "select sum(v) from a");
    (spec.keep, spec.agent) = (Some(1), Some("sim".into()));
    let w = db.simulate(&spec).unwrap().worlds[0].name.clone();
    // main hasn't changed: nothing of the past to see
    db.replay(&w, None, Some("sim")).unwrap_or_else(|e| panic!("{e}"));
    run(&db, MAIN, "update a set v = 50", &[]).unwrap();
    let e = db.replay(&w, None, Some("sim")).unwrap_err();
    assert!(e.to_string().contains("restore"), "{e}");
    assert!(db.replay(&w, None, None).is_ok());
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
