//! Agents: tokens, what each may do, quotas, worlds they own (and that expire), checkpoints, and
//! the audit trail of what they did.

use chronos::{AgentSpec, Can, Db, MAIN};
use std::time::Duration;

fn code(e: chronos::Error) -> &'static str {
    match e {
        chronos::Error::Constraint { code, .. } => code,
        e => panic!("not a constraint error: {e}"),
    }
}

#[test]
fn tokens_rights_and_owned_worlds() {
    let db = Db::new();
    db.put(MAIN, "t/1", r#"{"v": 1}"#).unwrap();
    let (bot, token) = db.create_agent("bot", &AgentSpec::default()).unwrap();
    assert_eq!(bot.can, Can::DEFAULT);
    assert!(token.starts_with("chronos_") && token.len() > 60);
    assert_eq!(db.agent_by_token(&token).unwrap().name, "bot");
    assert!(db.agent_by_token("chronos_wrong").is_err());
    assert!(!format!("{:?}", db.agents().unwrap()).contains(&token), "the token itself is never kept");

    let me = db.as_agent("bot").unwrap();
    let w = me.fork(MAIN, "bot-work", serde_json::json!({"task": 7})).unwrap();
    assert_eq!((w.owner.as_deref(), &w.meta), (Some("bot"), &serde_json::json!({"task": 7})));
    me.may_write("bot-work", 1).unwrap();
    assert_eq!(code(me.may_write(MAIN, 1).unwrap_err()), "42501", "main needs write_main");
    assert_eq!(code(me.may_merge("bot-work").unwrap_err()), "42501", "merging needs merge_own");
    // another agent's world is off limits, and so is renaming who owns one
    let (_, _) = db.create_agent("other", &AgentSpec::default()).unwrap();
    let them = db.as_agent("other").unwrap();
    assert_eq!(code(them.may_write("bot-work", 1).unwrap_err()), "42501");
    assert_eq!(code(them.may_manage("bot-work").unwrap_err()), "42501");
    assert!(db.set_meta("bot-work", serde_json::json!({"_sonos_owner": "other"})).is_err());
    assert!(db.fork_with(MAIN, "sneaky", serde_json::json!({"_sonos_owner": "bot"})).is_err());
    drop(them);

    // rights can grow: now it may publish its own work, into main only with write_main too
    drop(me);
    db.alter_agent(
        "bot",
        &AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn, Can::MergeOwn]), ..Default::default() },
    )
    .unwrap();
    let me = db.as_agent("bot").unwrap();
    assert_eq!(code(me.may_merge("bot-work").unwrap_err()), "42501", "merging into main needs write_main");
    drop(me);
    let can = vec![Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain, Can::MergeOwn];
    db.alter_agent("bot", &AgentSpec { can: Some(can), ..Default::default() }).unwrap();
    let me = db.as_agent("bot").unwrap();
    me.may_merge("bot-work").unwrap();
    drop(me);
    // disabled agents do nothing, and their tokens stop working
    db.alter_agent("bot", &AgentSpec { disabled: Some(true), ..Default::default() }).unwrap();
    assert!(db.as_agent("bot").is_err() && db.agent_by_token(&token).is_err());
}

#[test]
fn quotas_and_expiry() {
    let db = Db::new();
    let spec = AgentSpec {
        max_worlds: Some(2),
        max_changes: Some(3),
        writes_per_minute: Some(5),
        world_ttl: Some(30),
        ..Default::default()
    };
    db.create_agent("busy", &spec).unwrap();
    let me = db.as_agent("busy").unwrap();
    me.fork(MAIN, "a", serde_json::Value::Null).unwrap();
    me.fork(MAIN, "b", serde_json::Value::Null).unwrap();
    assert_eq!(code(me.fork(MAIN, "c", serde_json::Value::Null).unwrap_err()), "42501", "2 worlds at once");

    // rows each world may change
    me.may_write("a", 2).unwrap();
    db.put("a", "t/1", "{}").unwrap();
    db.put("a", "t/2", "{}").unwrap();
    assert_eq!(code(me.may_write("a", 2).unwrap_err()), "42501", "a has changed 2 of 3 rows");

    // writes a minute: 5 allowed (one went above; a refused one takes none), then slow down
    for _ in 0..4 {
        me.may_write("b", 0).unwrap();
    }
    assert_eq!(code(me.may_write("b", 0).unwrap_err()), "53400");

    // worlds live 30 ms
    let w = db.world("a").unwrap();
    assert!(w.expires.is_some_and(|e| e > w.created));
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(db.expire_worlds().unwrap(), 2);
    assert!(db.world("a").is_err() && db.world("b").is_err());
    me.fork(MAIN, "c", serde_json::Value::Null).unwrap(); // room again
}

#[test]
fn audit_checkpoints_and_restarts() {
    use chronos::sql::run;
    let dir = std::env::temp_dir().join(format!("chronos-agents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, "create table t (id int primary key, v text); insert into t values (1, 'one')", &[]).unwrap();
        let o =
            run(&db, MAIN, "create agent writer with (can = 'read,fork,write_own', world_ttl = '1 day')", &[]).unwrap();
        assert_eq!(o[0].rows[0].len(), 3, "name, id, token");
        {
            let me = db.as_agent("writer").unwrap();
            me.fork(MAIN, "w1", serde_json::Value::Null).unwrap();
            me.may_write("w1", 1).unwrap();
            run(&db, "w1", "update t set v = 'uno' where id = 1", &[]).unwrap();
        }
        run(&db, MAIN, "insert into t values (2, 'two')", &[]).unwrap(); // a person, not an agent

        // named checkpoints: restore a world to one
        run(&db, MAIN, "checkpoint world w1 as 'before'", &[]).unwrap();
        run(&db, "w1", "update t set v = 'oops' where id = 1", &[]).unwrap();
        assert_eq!(run(&db, MAIN, "restore world w1 to checkpoint 'before'", &[]).unwrap()[0].tag, "RESTORE 1");
        let v = run(&db, "w1", "select v from t where id = 1", &[]).unwrap();
        assert_eq!(v[0].rows[0][0].to_text(), "uno");

        let audit = db.audit(None, 50).unwrap();
        let mine: Vec<String> = audit.iter().map(|e| format!("{} {} {}", e.agent, e.world, e.action)).collect();
        assert_eq!(mine, ["writer w1 write", "writer w1 fork from main"], "only the agent's actions, newest first");
        assert!(db.audit(Some("nobody"), 10).unwrap().is_empty());
        let shown = run(&db, MAIN, "show audit for agent writer limit 1", &[]).unwrap();
        assert_eq!(shown[0].rows.len(), 1);
    }
    // agents, owners, expiry and checkpoints survive a restart
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.agent("writer").unwrap().world_ttl, 86_400_000);
    let w = db.world("w1").unwrap();
    assert_eq!((w.owner.as_deref(), w.checkpoints.len()), (Some("writer"), 1));
    assert!(w.expires.is_some());
    assert_eq!(db.audit(Some("writer"), 10).unwrap().len(), 2);
    let shown = chronos::sql::run(&db, MAIN, "show worlds", &[]).unwrap();
    let row = shown[0].rows.iter().find(|r| r[0].to_text() == "w1").unwrap();
    assert_eq!(row[7].to_text(), "writer");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A SQL session acting as an agent: CREATE WORLD makes a world it owns, it changes only that
/// one (transactions too), and merging or managing agents is refused.
#[test]
fn sql_sessions_as_agents() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, v text); insert into t values (1, 'one')", &[]).unwrap();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    let mut q = |sql: &str| s.run(&db, sql, &[]);
    assert_eq!(q("select count(*) from t").unwrap()[0].rows[0][0].to_text(), "1");
    assert_eq!(q("insert into t values (2, 'two')").unwrap_err().code, "42501", "not main");
    assert_eq!(q("create world mine").unwrap()[0].tag, "CREATE WORLD");
    assert_eq!(db.world("mine").unwrap().owner.as_deref(), Some("bot"));
    q("use world mine").unwrap();
    q("insert into t values (2, 'two')").unwrap();
    q("begin; update t set v = 'ONE' where id = 1; commit").unwrap();
    assert_eq!(q("merge world mine").unwrap_err().code, "42501", "a person merges");
    assert!(q("merge world mine dry run").is_ok(), "but it may look");
    assert_eq!(q("create agent evil").unwrap_err().code, "42501");
    assert_eq!(q("alter system set history_retention = '0'").unwrap_err().code, "42501");
    let rows = run(&db, "mine", "select id, v from t order by id", &[]).unwrap();
    assert_eq!(rows[0].rows.len(), 2);
}

/// The database's own tables (`_sonos_agents`, constraint indexes, ...) are out of SQL's reach for
/// everyone: an agent can't grant itself admin by updating its row in a world and getting it merged.
#[test]
fn sql_cannot_reach_the_databases_own_tables() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, v text unique)", &[]).unwrap();
    let can = vec![Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain, Can::MergeOwn];
    db.create_agent("bot", &AgentSpec { can: Some(can), ..Default::default() }).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(&db, "create world w; use world w", &[]).unwrap();
    for sql in [
        r#"update _sonos_agents set can = '["admin"]'::jsonb"#,
        "delete from _sonos_agents",
        "select * from _sonos_agents",
        "insert into _sonos_uniq values ('x')",
        "select * from _SONOS_tables",
        "create table _sonos_mine (id int primary key)",
        "alter table t rename to _sonos_t",
        "drop table _sonos_agents",
    ] {
        let e = s.run(&db, sql, &[]).unwrap_err();
        assert_eq!(e.code, "42501", "{sql}: {}", e.message);
        // nor may the database's own users
        assert_eq!(run(&db, MAIN, sql, &[]).unwrap_err().code, "42501", "{sql}");
    }
    assert!(db.diff("w").unwrap().is_empty(), "nothing was written");
    s.run(&db, "use world main; merge world w", &[]).unwrap();
    assert!(!db.agent("bot").unwrap().may(Can::Admin));
}

/// A row quota counts every row however it's written: SQL statements, transactions, and merges.
#[test]
fn row_quotas_count_sql_transactions_and_merges() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, v int)", &[]).unwrap();
    run(
        &db,
        MAIN,
        &format!("insert into t values {}", (1..=20).map(|i| format!("({i}, 0)")).collect::<Vec<_>>().join(",")),
        &[],
    )
    .unwrap();
    db.create_agent("bot", &AgentSpec { max_changes: Some(5), ..Default::default() }).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    let mut q = |sql: &str| s.run(&db, sql, &[]).map_err(|e| e.code);
    q("create world w; use world w").unwrap();
    assert_eq!(q("update t set v = 1 where id <= 6").unwrap_err(), "42501", "one statement, six rows");
    assert_eq!(
        q("insert into t values (101, 0), (102, 0), (103, 0), (104, 0), (105, 0), (106, 0)").unwrap_err(),
        "42501"
    );
    q("update t set v = 1 where id <= 3").unwrap();
    assert_eq!(
        q("begin; update t set v = 2 where id between 4 and 6; commit").unwrap_err(),
        "42501",
        "a transaction counts at commit"
    );
    assert_eq!(q("rollback").map(|_| ()), Ok(()));
    q("update t set v = 2 where id in (4, 5)").unwrap(); // five rows: the quota
    assert_eq!(q("update t set v = 3 where id = 1").unwrap_err(), "42501");
    // a world forked from w brings its rows when merged into w
    let me = db.as_agent("bot").unwrap();
    me.fork("w", "w2", serde_json::Value::Null).unwrap();
    drop(me);
    run(&db, "w2", "update t set v = 9 where id = 20", &[]).unwrap();
    let _as = chronos::acting_as("bot");
    assert_eq!(code(db.merge("w2", chronos::Resolve::Fail).unwrap_err()), "42501", "w is at its quota");
}

/// Quotas survive restarts and checkpoints: a world's earlier changes still count.
#[test]
fn row_quotas_survive_restarts() {
    let dir = std::env::temp_dir().join(format!("chronos-quota-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        db.create_agent("bot", &AgentSpec { max_changes: Some(5), ..Default::default() }).unwrap();
        let me = db.as_agent("bot").unwrap();
        me.fork(MAIN, "d", serde_json::Value::Null).unwrap();
        for i in 0..5 {
            db.put("d", &format!("t/{i}"), "{}").unwrap(); // acting as bot: counted
        }
        assert_eq!(code(db.put("d", "t/5", "{}").unwrap_err()), "42501");
        drop(me);
        db.checkpoint().unwrap(); // the rows now live in the checkpoint, not the log
    }
    let db = Db::open(&dir).unwrap();
    let _as = chronos::acting_as("bot");
    assert_eq!(code(db.put("d", "t/5", "{}").unwrap_err()), "42501", "still at its quota after a restart");
    drop(_as);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn add_column_keeps_its_constraints() {
    use chronos::sql::run;
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table p (id int primary key); insert into p values (1); create table c (id int primary key)",
        &[],
    )
    .unwrap();
    run(&db, MAIN, "insert into c values (1)", &[]).unwrap();
    run(&db, MAIN, "alter table c add column p_id int references p(id)", &[]).unwrap();
    assert_eq!(
        run(&db, MAIN, "insert into c values (2, 99)", &[]).unwrap_err().code,
        "23503",
        "the foreign key is real"
    );
    run(&db, MAIN, "insert into c values (3, 1)", &[]).unwrap();
    run(&db, MAIN, "alter table c add column code text unique", &[]).unwrap();
    run(&db, MAIN, "update c set code = 'x' where id = 1", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "update c set code = 'x' where id = 3", &[]).unwrap_err().code, "23505");
    let e = run(&db, MAIN, "alter table c add column a text references _sonos_agents", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
}

/// Diffs never show the database's own rows (agents' token hashes, constraint indexes), whatever
/// is diffed: a world since its fork, main, or two moments.
#[test]
fn diffs_hide_the_databases_own_rows() {
    let db = Db::new();
    chronos::sql::run(&db, MAIN, "create table t (id int primary key, v text unique)", &[]).unwrap();
    db.fork(MAIN, "w").unwrap();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    chronos::sql::run(&db, MAIN, "insert into t values (1, 'a')", &[]).unwrap();
    for d in [db.diff(MAIN).unwrap(), db.diff_between("w", MAIN).unwrap()] {
        let keys: Vec<String> = d.iter().map(|c| chronos::sql::show_key(&c.key).into_owned()).collect();
        assert!(
            keys.iter().any(|k| k == "t/1")
                && keys.iter().all(|k| !k.starts_with("_sonos_") || k.starts_with("_sonos_tables/")),
            "{keys:?}"
        );
    }
}

/// Rebuilding the past replays what was accepted then, whoever asks: an agent (with a quota, or
/// none known to the rebuild) gets the same past as anyone.
#[test]
fn agents_see_the_same_past() {
    let dir = std::env::temp_dir().join(format!("chronos-agent-past-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    db.create_agent("bot", &AgentSpec { max_changes: Some(1), ..Default::default() }).unwrap();
    db.fork(MAIN, "x").unwrap();
    for i in 0..3 {
        db.put("x", &format!("t/{i}"), "{}").unwrap(); // by the database's own user: no quota
    }
    std::thread::sleep(Duration::from_millis(5));
    let then = chronos::sql::moment("now").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    db.put("x", "t/9", "{}").unwrap();
    let _as = chronos::acting_as("bot");
    let past = db.view_at("x", then).unwrap();
    assert_eq!(past.prefix("t/").unwrap().count(), 3, "all three rows, though bot may change one");
    drop(past);
    drop(_as);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Everything an agent changed in a world since a moment can be undone, row by row, whether it
/// wrote there directly, committed transactions or merged its own worlds in. Everyone else's
/// An undo writes the world it undoes into, main included: besides `restore` and `merge`, the
/// agent needs the right to write that world, as a restore does.
#[test]
fn undo_needs_the_right_to_write_its_world() {
    use chronos::sql::{Session, run};
    let dir = std::env::temp_dir().join(format!("chronos-undo-rights-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let q = |sql: &str| run(&db, MAIN, sql, &[]).unwrap();
    q("create table t (id int primary key, v text); insert into t values (1, 'one')");
    q("create world x; use world x; update t set v = 'ONE' where id = 1; use world main; merge world x");
    for (name, can) in [
        ("undoer", vec![Can::Read, Can::Restore, Can::Merge]),
        ("writer", vec![Can::Read, Can::Restore, Can::Merge, Can::WriteMain]),
    ] {
        db.create_agent(name, &AgentSpec { can: Some(can), ..Default::default() }).unwrap();
    }
    let mut s = Session::new(MAIN);
    s.agent = Some("undoer".into());
    let e = s.run(&db, "undo merge of world x", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    let e = s.run(&db, "undo agent writer since '-1 hour'", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert_eq!(q("select v from t")[0].rows[0][0].to_text(), "ONE", "nothing undone");
    s.agent = Some("writer".into());
    s.run(&db, "undo merge of world x", &[]).unwrap();
    assert_eq!(q("select v from t")[0].rows[0][0].to_text(), "one");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// changes stay; rows someone changed after the agent are named (or left, with SKIP CHANGED).
#[test]
fn undo_an_agent() {
    use chronos::sql::{Session, run};
    let dir = std::env::temp_dir().join(format!("chronos-undo-agent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let q = |sql: &str| run(&db, MAIN, sql, &[]).unwrap();
    let col = |sql: &str| -> Vec<String> { q(sql)[0].rows.iter().map(|r| r[0].to_text()).collect() };
    q("create table t (id int primary key, v text, code text unique); create index t_v on t (v)");
    q(&format!(
        "insert into t values {}",
        (1..=40).map(|i| format!("({i}, 'v{i}', 'c{i}')")).collect::<Vec<_>>().join(",")
    ));
    let can = vec![Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain, Can::MergeOwn];
    db.create_agent("bot", &AgentSpec { can: Some(can), ..Default::default() }).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(&db, "update t set v = 'early' where id = 1", &[]).unwrap(); // before the moment: stays
    std::thread::sleep(Duration::from_millis(5));
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    std::thread::sleep(Duration::from_millis(5));
    for sql in [
        "update t set v = 'bot' where id between 2 and 20", // straight into main
        "begin; delete from t where id = 21; insert into t values (100, 'new', 'c100'); commit", // a transaction
        "create world w; use world w; update t set code = 'x22' where id = 22; use world main; merge world w",
    ] {
        bot.run(&db, sql, &[]).unwrap();
    }
    q("update t set v = 'person' where id in (30, 31); update t set v = 'late' where id = 5"); // a person, after

    let e = db.undo_agent("bot", MAIN, since, false).unwrap_err().to_string();
    assert!(e.contains("t/5") && e.contains("SKIP CHANGED"), "{e}");
    assert_eq!(col("select v from t where id = 2"), ["bot"], "nothing undone");

    let u = db.undo_agent("bot", MAIN, since, true).unwrap();
    assert_eq!((u.rows, u.skipped), (18 + 1 + 1 + 1, 1)); // 2..20 but 5, 21 back, 100 gone, 22's code
    let want: Vec<String> = (1..=40)
        .map(|i| match i {
            1 => "1|early|c1".to_string(),
            5 => "5|late|c5".into(),
            30 | 31 => format!("{i}|person|c{i}"),
            _ => format!("{i}|v{i}|c{i}"),
        })
        .collect();
    assert_eq!(col("select id || '|' || v || '|' || code from t order by id"), want);
    // indexes and unique values follow the rows back
    assert_eq!(col("select id from t where v = 'v3'"), ["3"]);
    assert!(col("select id from t where v = 'bot'").is_empty());
    assert_eq!(col("select id from t where v = 'late'"), ["5"]);
    q("insert into t values (101, 'x', 'c100'), (102, 'x', 'x22')"); // freed
    assert!(run(&db, MAIN, "insert into t values (103, 'x', 'c22')", &[]).is_err(), "c22 is 22's again");

    // in SQL, from a time ago: what's left of the bot's is row 1; the rest was changed since
    assert_eq!(run(&db, MAIN, "undo agent bot since '-1 hour'", &[]).unwrap_err().code, "40001");
    assert_eq!(q("undo agent bot since '-1 hour' skip changed")[0].tag, "UNDO AGENT 1");
    assert_eq!(col("select v from t where id = 1"), ["v1"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An agent's temporary tables live in its session's own database, which has no agents: writing,
/// savepoints and commits there act for no one, with or without a merge policy or a quota.
#[test]
fn agents_use_temporary_tables_in_transactions() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    run(&db, MAIN, "create merge policy small with (max_rows = 2)", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    for (bot, with) in [("plain", ""), ("ruled", ", policy = 'small'"), ("capped", ", max_changes = 3")] {
        run(&db, MAIN, &format!("create agent {bot} with (can = '{can}'{with})"), &[]).unwrap();
        let mut s = Session::new(MAIN);
        s.agent = Some(bot.into());
        let mut q = |sql: &str| s.run(&db, sql, &[]).unwrap_or_else(|e| panic!("{bot}: {sql}: {}", e.message));
        q(&format!("create world {bot}_w; use world {bot}_w"));
        q("begin; create temp table scratch (id int); insert into scratch values (1); commit");
        q("begin; insert into scratch values (2); savepoint a; insert into scratch values (3); release savepoint a");
        q("savepoint b; insert into scratch values (4); rollback to savepoint b; commit");
        q("begin; insert into scratch values (5), (6), (7), (8); rollback");
        q("update scratch set id = id * 10; delete from scratch where id = 30");
        let rows = q("select id from scratch order by id").pop().unwrap().rows;
        let ids: Vec<String> = rows.iter().map(|r| r[0].to_text()).collect();
        assert_eq!(ids, ["10", "20"], "{bot}");
        q("drop table scratch");
    }
}

/// Merge policies: an agent's merge that keeps to its policy's rules goes through on its own; one
/// that breaks any is refused and queued for a person, who merges it.
#[test]
fn merge_policies_send_big_or_risky_merges_to_a_person() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    run(&db, MAIN, "create table orders (id serial primary key, status text); create table payments (id int primary key, amount int)", &[]).unwrap();
    run(&db, MAIN, "insert into orders (status) values ('new'), ('new'), ('new')", &[]).unwrap();
    run(&db, MAIN, "create merge policy small with (max_rows = 2, max_deletes = 0, review_tables = 'payments')", &[])
        .unwrap();
    let can = "read,fork,write_own,merge_own,write_main";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'small')"), &[]).unwrap();
    assert_eq!(db.agent("bot").unwrap().policy.as_deref(), Some("small"));
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    let mut q = |sql: &str| s.run(&db, sql, &[]);

    // write_main lets it merge into main, but not write there: that would get around the rules
    let e = q("update orders set status = 'direct'").unwrap_err();
    assert!(e.code == "42501" && e.message.contains("changes main only by merging"), "{}", e.message);

    // small, safe: merges on its own (a serial's counter moving isn't a schema change)
    q("create world a; use world a; insert into orders (status) values ('paid'); update orders set status = 'sent' where id = 1").unwrap();
    q("use world main; merge world a").unwrap();
    assert_eq!(run(&db, MAIN, "select count(*) from orders", &[]).unwrap()[0].rows[0][0].to_text(), "4");

    // each rule broken: refused, queued, the world kept
    for (w, sql, why) in [
        ("big", "update orders set status = 'x'", "changes 4 rows (at most 2)"),
        ("del", "delete from orders where id = 2", "deletes 1 rows (at most 0)"),
        ("pay", "insert into payments values (1, 10)", "changes payments (always reviewed)"),
        ("ddl", "alter table orders add column note text", "changes the schema: table orders"),
    ] {
        q(&format!("create world {w}; use world {w}; {sql}; use world main")).unwrap();
        let plan = q(&format!("merge world {w} dry run")).unwrap();
        assert!(plan.iter().any(|o| o.tag.contains("SELECT")), "a dry run still plans");
        assert!(
            db.merge_preview(w, &Default::default()).unwrap().blocked.is_none(),
            "a person's preview isn't blocked"
        );
        let e = q(&format!("merge world {w}")).unwrap_err();
        assert_eq!(e.code, "42501", "{w}: {}", e.message);
        assert!(e.message.contains(why) && e.message.contains("SHOW REVIEWS"), "{w}: {}", e.message);
        assert!(db.world(w).is_ok(), "{w} is kept for review");
    }
    let _as = chronos::acting_as("bot");
    let blocked = db.merge_preview("big", &Default::default()).unwrap().blocked.unwrap_or_default();
    assert!(blocked.contains("needs a person's review"), "the agent's preview says so: {blocked}");
    drop(_as);

    // overwriting what changed in main since the fork needs a person; by columns it doesn't
    q("create world race; use world race; update orders set status = 'mine' where id = 3; use world main").unwrap();
    run(&db, MAIN, "update orders set status = 'theirs' where id = 3", &[]).unwrap();
    assert!(q("merge world race ours").unwrap_err().message.contains("overwrites 1 rows"));
    let _ = q("drop world race");

    // the queue: oldest first, with whether the world changed since the agent asked
    let r = run(&db, MAIN, "show reviews", &[]).unwrap();
    let mut worlds: Vec<String> = r[0].rows.iter().map(|row| row[0].to_text()).collect();
    let asked: Vec<String> = r[0].rows.iter().map(|row| row[4].to_text()).collect();
    assert!(asked.windows(2).all(|w| w[0] <= w[1]), "oldest first: {asked:?}");
    worlds.sort();
    assert_eq!(worlds, ["big", "ddl", "del", "pay"]);
    assert!(r[0].rows.iter().all(|row| row[1].to_text() == "bot" && row[6].to_text() == "false"));
    q("use world big; update orders set status = 'y' where id = 1; use world main").unwrap();
    let r = run(&db, MAIN, "show reviews", &[]).unwrap();
    let big = r[0].rows.iter().find(|row| row[0].to_text() == "big").unwrap();
    assert_eq!(big[6].to_text(), "true", "big changed since");

    // a person approves by merging; the agent can't clear its flag
    assert!(q("alter world pay set (_sonos_review = null)").is_err());
    run(&db, MAIN, "merge world pay", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "select count(*) from payments", &[]).unwrap()[0].rows[0][0].to_text(), "1");
    assert_eq!(db.reviews().len(), 3);

    // policies: shown, changed, and not dropped while an agent keeps to one
    let p = run(&db, MAIN, "alter merge policy small set (max_rows = null, schema = true)", &[]).unwrap();
    assert_eq!((p[0].tag.as_str(), p[0].rows.len()), ("ALTER MERGE POLICY", 1));
    assert_eq!(db.policy("small").unwrap().max_rows, None);
    assert!(db.policy("small").unwrap().schema);
    q("create world c; use world c; update orders set status = 'z' where id <> 2; use world main; merge world c")
        .unwrap(); // no row limit now
    assert_eq!(run(&db, MAIN, "show merge policies", &[]).unwrap()[0].rows.len(), 1);
    assert_eq!(run(&db, MAIN, "drop merge policy small", &[]).unwrap_err().code, "22023");
    assert!(run(&db, MAIN, "alter agent bot set (policy = 'nope')", &[]).is_err());
    assert!(db.agent("bot").unwrap().policy.is_some(), "a missing policy isn't set");
    run(&db, MAIN, "alter agent bot set (policy = null); drop merge policy small", &[]).unwrap();
    assert!(db.policies().unwrap().is_empty());
    q("merge world del").unwrap(); // no policy: its rights alone decide
    q("update orders set status = 'direct' where id = 5").unwrap();
    // agents may read policies and reviews, not make them
    assert!(q("show merge policies").is_ok() && q("show reviews").is_ok());
    assert_eq!(q("create merge policy mine with (max_rows = 100000)").unwrap_err().code, "42501");
}

/// Merge policies, from the PR review: tables named as SQL names them (`"Orders"` isn't
/// `orders`); rules count the rows a merge applies, not rows settled as the parent has them; an
/// agent keeping to a policy can't undo past it; the queue keeps the version it checked.
#[test]
fn merge_policies_name_tables_count_applied_rows_and_bar_undo() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    run(
        &db,
        MAIN,
        r#"create table orders (id int primary key, s text); create table "Orders" (id int primary key, s text)"#,
        &[],
    )
    .unwrap();
    run(&db, MAIN, r#"insert into orders values (1, 'a'), (2, 'a'); insert into "Orders" values (1, 'a')"#, &[])
        .unwrap();
    run(&db, MAIN, "create merge policy only_orders with (tables = 'orders', max_rows = 1)", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own,merge,restore";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'only_orders')"), &[]).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    let mut q = |sql: &str| s.run(&db, sql, &[]);

    // "Orders" is another table than orders
    q(r#"create world upper; use world upper; update "Orders" set s = 'b'; use world main"#).unwrap();
    let e = q("merge world upper").unwrap_err();
    assert!(e.message.contains("changes Orders (only orders)"), "{}", e.message);
    run(&db, MAIN, r#"alter merge policy only_orders set (tables = 'orders,"Orders"')"#, &[]).unwrap();
    assert_eq!(db.policy("only_orders").unwrap().tables, ["orders", "Orders"]);
    let shown = run(&db, MAIN, "show merge policies", &[]).unwrap();
    assert_eq!(shown[0].rows[0][3].to_text(), r#"orders,"Orders""#, "as the setting takes it back");
    q("merge world upper").unwrap();

    // a quoted name may hold a comma: one table, not two
    run(&db, MAIN, r#"create table "payments,archive" (id int primary key)"#, &[]).unwrap();
    run(
        &db,
        MAIN,
        r#"create merge policy money with (review_tables = ' "payments,archive" , "Pay ""x""", orders')"#,
        &[],
    )
    .unwrap();
    assert_eq!(db.policy("money").unwrap().review_tables, ["payments,archive", r#"Pay "x""#, "orders"]);
    let shown = run(&db, MAIN, "show merge policies", &[]).unwrap();
    let money = shown[0].rows.iter().find(|r| r[0].to_text() == "money").unwrap();
    assert_eq!(money[4].to_text(), r#""payments,archive","Pay ""x""",orders"#, "as the setting takes it back");
    assert!(run(&db, MAIN, r#"alter merge policy money set (tables = '"open')"#, &[]).is_err(), "a quote left open");
    assert!(db.alter_policy("money", &serde_json::json!({"tables": ["a,b"]})).is_err(), "one name per entry");
    run(&db, MAIN, "alter agent bot set (policy = 'money')", &[]).unwrap();
    q(r#"create world arch; use world arch; insert into "payments,archive" values (1); use world main"#).unwrap();
    let e = q("merge world arch").unwrap_err();
    assert!(e.message.contains("changes payments,archive (always reviewed)"), "{}", e.message);
    run(&db, MAIN, "alter agent bot set (policy = 'only_orders')", &[]).unwrap();

    // two rows changed, but the parent's value kept for one: one row applied, within max_rows
    q("create world two; use world two; update orders set s = 'mine'; use world main").unwrap();
    run(&db, MAIN, "update orders set s = 'theirs' where id = 2", &[]).unwrap();
    q("merge world two using theirs").unwrap();
    let rows = run(&db, MAIN, "select s from orders order by id", &[]).unwrap();
    assert_eq!(rows[0].rows.iter().map(|r| r[0].to_text()).collect::<Vec<_>>(), ["mine", "theirs"]);

    // undoing writes straight into main, past the rules
    let e = q("undo merge of world two").unwrap_err();
    assert!(e.code == "42501" && e.message.contains("may not undo"), "{}", e.message);
    assert!(q("undo agent bot since '-1 hour'").unwrap_err().message.contains("may not undo"));

    // the queue keeps the version its rules were checked at
    q("create world big; use world big; update orders set s = 'x'; use world main").unwrap();
    let at = db.world("big").unwrap().version;
    assert!(q("merge world big").is_err());
    assert_eq!(db.reviews()[0].version, at);
}

/// Merge policies, from the PR's second review: every definition is a schema change (enum types
/// included), and dropping a policy can't race an agent being given it.
#[test]
fn merge_policies_see_every_definition_and_drop_atomically() {
    use chronos::sql::{Session, run};
    use std::sync::Arc;
    let db = Arc::new(Db::new());
    run(&db, MAIN, "create table t (id int primary key)", &[]).unwrap();
    run(&db, MAIN, "create merge policy p", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'p')"), &[]).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(&db, "create world ty; use world ty; create type mood as enum ('ok', 'meh'); use world main", &[]).unwrap();
    let e = s.run(&db, "merge world ty", &[]).unwrap_err();
    assert!(e.message.contains("changes the schema: type mood"), "{}", e.message);
    run(&db, MAIN, "alter agent bot set (policy = null)", &[]).unwrap();

    // a drop and an agent given the policy, at once: never an agent naming a dropped policy
    for i in 0..200 {
        let name = format!("race{i}");
        db.create_policy(&name, &serde_json::Value::Null).unwrap();
        let (d1, d2) = (db.clone(), db.clone());
        let n2 = name.clone();
        let give = std::thread::spawn(move || {
            d2.alter_agent("bot", &AgentSpec { policy: Some(n2), ..Default::default() }).is_ok()
        });
        let dropped = d1.drop_policy(&name, false).is_ok();
        let given = give.join().unwrap();
        assert!(!(dropped && given), "{name}: both went through");
        if let Some(p) = db.agent("bot").unwrap().policy {
            assert!(db.policy(&p).is_ok(), "bot names {p}, which is gone");
        }
        db.alter_agent("bot", &AgentSpec { policy: Some(String::new()), ..Default::default() }).unwrap();
        let _ = db.drop_policy(&name, false);
    }
}

/// Merge policies, from the PR's fourth review: an agent dropped while it acts can't merge
/// unchecked.
#[test]
fn merge_policies_fail_closed_on_a_dropped_agent() {
    use chronos::sql::run;
    let db = Db::new();
    run(&db, MAIN, "create table orders (id int primary key, s text); create table payments (id int primary key)", &[])
        .unwrap();
    run(&db, MAIN, "insert into orders values (1, 'a')", &[]).unwrap();
    run(&db, MAIN, "create merge policy p with (review_tables = 'payments')", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(
        &db,
        MAIN,
        &format!(
            "create agent bot with (can = '{can}', policy = 'p'); create agent gone with (can = '{can}', policy = 'p')"
        ),
        &[],
    )
    .unwrap();
    // an agent dropped while it acts: refused, not merged unchecked
    let me = db.as_agent("gone").unwrap();
    me.fork(MAIN, "gone-w", serde_json::Value::Null).unwrap();
    db.put("gone-w", "notes/9", "{}").unwrap();
    {
        let _person = chronos::acting_as("");
        db.drop_agent("gone").unwrap();
    }
    let e = db.merge("gone-w", chronos::Resolve::Fail).unwrap_err();
    assert!(e.to_string().contains("no longer exists"), "{e}");
    drop(me);
    assert!(db.world("gone-w").is_ok(), "nothing was merged");
}

/// Lists and the leads in them; every list keeps at least one lead (the rule an agent that deleted
/// every CRM lead list broke).
fn crm(db: &Db) {
    use chronos::sql::run;
    run(db, MAIN, "create table lists (id int primary key, name text); create table leads (id int primary key, list int, who text)", &[])
        .unwrap();
    run(db, MAIN, "insert into lists values (1, 'vip'), (2, 'cold'); insert into leads values (1, 1, 'ada'), (2, 1, 'bo'), (3, 2, 'cy')", &[])
        .unwrap();
    run(
        db,
        MAIN,
        "create merge check lists_keep_leads on tables (leads, lists) as select id from lists where id not in (select list from leads)",
        &[],
    )
    .unwrap();
}

fn count(db: &Db, world: &str, table: &str) -> String {
    chronos::sql::run(db, world, &format!("select count(*) from {table}"), &[]).unwrap()[0].rows[0][0].to_text()
}

/// Merge checks: a query a merge's result must find nothing in. Two changes each fine alone but
/// breaking the rule together are caught; dry runs say so first; partial merges and merges into
/// another world check what they'd make; the rows found are the reason.
#[test]
fn merge_checks_refuse_what_two_fine_changes_break_together() {
    use chronos::sql::run;
    let db = Db::new();
    crm(&db);
    let shown = run(&db, MAIN, "show merge checks", &[]).unwrap();
    let c = &shown[0].rows[0];
    assert_eq!(
        (c[0].to_text(), c[1].to_text(), c[2].to_text()),
        ("lists_keep_leads".into(), "leads,lists".into(), "1000".into())
    );
    assert!(c[3].to_text().starts_with("select id from lists"), "{:?}", c[3]);

    // each takes one of list 1's two leads: fine alone, not together
    for (w, id) in [("a", 1), ("b", 2)] {
        db.fork(MAIN, w).unwrap();
        run(&db, w, &format!("delete from leads where id = {id}"), &[]).unwrap();
    }
    assert_eq!(db.merge_preview("a", &Default::default()).unwrap().blocked, None);
    run(&db, MAIN, "merge world a", &[]).unwrap(); // passes: list 1 still has lead 2
    let blocked = db.merge_preview("b", &Default::default()).unwrap().blocked.unwrap_or_default();
    assert!(blocked.contains("breaks merge check lists_keep_leads, which finds (id = 1)"), "{blocked}");
    let plan = run(&db, MAIN, "merge world b dry run", &[]).unwrap();
    let last = plan[0].rows.last().unwrap();
    assert_eq!(last[2].to_text(), "blocked");
    assert!(last[3].to_text().contains("lists_keep_leads"), "{:?}", last[3]);
    let e = run(&db, MAIN, "merge world b", &[]).unwrap_err();
    assert_eq!(e.code, "23514", "{}", e.message);
    assert!(
        e.message.contains("merging b breaks merge check lists_keep_leads, which finds (id = 1); nothing was merged"),
        "{}",
        e.message
    );
    assert!(db.world("b").is_ok(), "b stays");
    assert_eq!(count(&db, MAIN, "leads"), "2", "nothing was merged");
    assert!(db.reviews().is_empty(), "a person's merge is refused, not queued");

    // part of a world: what that part would make is checked, not the whole world
    db.fork(MAIN, "p").unwrap();
    run(
        &db,
        "p",
        "insert into lists values (3, 'new'); insert into leads values (4, 3, 'di'); delete from leads where id = 3",
        &[],
    )
    .unwrap();
    let e = run(&db, MAIN, "merge world p only tables (lists)", &[]).unwrap_err();
    assert!(e.message.contains("finds (id = 3)"), "list 3 without its lead: {}", e.message);
    let e = run(&db, MAIN, "merge world p only tables (leads)", &[]).unwrap_err();
    assert!(e.message.contains("finds (id = 2)"), "list 2 without lead 3: {}", e.message);
    let blocked = db
        .merge_preview(
            "p",
            &chronos::MergeOptions { only_keys: vec!["lists/3".into(), "leads/4".into()], ..Default::default() },
        )
        .unwrap()
        .blocked;
    assert_eq!(blocked, None);
    run(&db, MAIN, "merge world p only keys ('lists/3', 'leads/4')", &[]).unwrap();
    assert_eq!(count(&db, MAIN, "lists"), "3");

    // into another world: that world's result is checked, and main's rows don't matter
    db.fork(MAIN, "y").unwrap();
    run(&db, "y", "insert into leads values (5, 2, 'ed')", &[]).unwrap();
    db.fork(MAIN, "q").unwrap();
    run(&db, "q", "delete from leads where id = 3", &[]).unwrap();
    let e = run(&db, MAIN, "merge world b into y", &[]).unwrap_err();
    assert!(e.message.contains("finds (id = 1)"), "{}", e.message);
    run(&db, MAIN, "merge world q into y", &[]).unwrap(); // y's list 2 keeps lead 5
    assert!(run(&db, MAIN, "merge world q", &[]).unwrap_err().message.contains("finds (id = 2)"), "not in main");

    // more rows than a reason shows: the first ten, and a word that there are more
    run(&db, MAIN, "insert into lists select g, 'empty' from generate_series(100, 111) g", &[]).unwrap();
    db.fork(MAIN, "more").unwrap();
    run(&db, "more", "update lists set name = 'still empty' where id = 100", &[]).unwrap();
    let e = run(&db, MAIN, "merge world more", &[]).unwrap_err();
    assert!(e.message.contains("(id = 100), ") && e.message.contains(", and more"), "{}", e.message);
    assert!(!e.message.contains("(id = 111)"), "{}", e.message);
    // a LIMIT of its own applies first
    run(
        &db,
        MAIN,
        "create or replace merge check lists_keep_leads on tables (leads, lists) as select id from lists where id not in (select list from leads) order by id limit 2",
        &[],
    )
    .unwrap();
    let e = run(&db, MAIN, "merge world more", &[]).unwrap_err();
    assert!(e.message.ends_with("finds (id = 100), (id = 101); nothing was merged"), "{}", e.message);

    // dropped: merges go through again
    run(&db, MAIN, "drop merge check lists_keep_leads", &[]).unwrap();
    assert!(db.checks().unwrap().is_empty());
    run(&db, MAIN, "merge world b", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "drop merge check lists_keep_leads", &[]).unwrap_err().code, "3D000");
}

/// Merge checks and who merges: an agent keeping to a merge policy is queued for a person, like a
/// rule it broke; anyone else, a person included, is refused. The check applies to people too:
/// the person approves by fixing the world, or by changing the check.
#[test]
fn merge_checks_queue_agents_under_a_policy_and_refuse_everyone_else() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    crm(&db);
    run(&db, MAIN, "create merge policy p", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(
        &db,
        MAIN,
        &format!("create agent bot with (can = '{can}', policy = 'p'); create agent free with (can = '{can}')"),
        &[],
    )
    .unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());

    bot.run(&db, "create world wipe; use world wipe; delete from leads where list = 2; use world main", &[]).unwrap();
    let plan = bot.run(&db, "merge world wipe dry run", &[]).unwrap();
    let why = plan[0].rows.last().unwrap()[3].to_text();
    assert!(why.contains("needs a person's review (merge policy p: it breaks merge check lists_keep_leads"), "{why}");
    let e = bot.run(&db, "merge world wipe", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(
        e.message.contains("breaks merge check lists_keep_leads, which finds (id = 2)")
            && e.message.contains("SHOW REVIEWS")
    );
    let r = db.reviews();
    assert_eq!((r.len(), r[0].world.as_str(), r[0].policy.as_str()), (1, "wipe", "p"));
    assert!(r[0].reasons[0].starts_with("breaks merge check lists_keep_leads"), "{:?}", r[0].reasons);
    // policy rules and checks together: both are reasons
    run(&db, MAIN, "alter merge policy p set (max_deletes = 0)", &[]).unwrap();
    let blocked = {
        let _as = chronos::acting_as("bot");
        db.merge_preview("wipe", &Default::default()).unwrap().blocked.unwrap_or_default()
    };
    assert!(blocked.contains("deletes 1 rows (at most 0); it breaks merge check"), "{blocked}");
    let e = bot.run(&db, "merge world wipe", &[]).unwrap_err();
    assert!(e.message.contains("deletes 1 rows (at most 0); it breaks merge check lists_keep_leads"), "{}", e.message);
    assert_eq!(db.reviews()[0].reasons.len(), 2, "the queue has both: {:?}", db.reviews()[0].reasons);
    run(&db, MAIN, "alter merge policy p set (max_deletes = null)", &[]).unwrap();

    // the person can't merge past the check either: they fix the world, then merge it
    let e = run(&db, MAIN, "merge world wipe", &[]).unwrap_err();
    assert_eq!(e.code, "23514", "{}", e.message);
    run(&db, "wipe", "insert into leads values (9, 2, 'new')", &[]).unwrap();
    run(&db, MAIN, "merge world wipe", &[]).unwrap();
    assert!(db.reviews().is_empty());

    // an agent without a policy: refused, not queued
    let mut free = Session::new(MAIN);
    free.agent = Some("free".into());
    free.run(&db, "create world wipe2; use world wipe2; delete from leads where list = 1; use world main", &[])
        .unwrap();
    let e = free.run(&db, "merge world wipe2", &[]).unwrap_err();
    assert_eq!(e.code, "23514", "{}", e.message);
    assert!(db.reviews().is_empty());

    // agents read checks; only admins make or drop them
    assert!(bot.run(&db, "show merge checks", &[]).is_ok());
    assert_eq!(bot.run(&db, "create merge check mine as select 1", &[]).unwrap_err().code, "42501");
    assert_eq!(free.run(&db, "drop merge check lists_keep_leads", &[]).unwrap_err().code, "42501");
    assert_eq!(db.checks().unwrap().len(), 1);
}

/// Merge checks: which run (ON TABLES), what a check may be (one SELECT that changes nothing),
/// how long it may take, and that they survive a restart without being run again on replay.
#[test]
fn merge_checks_run_for_their_tables_change_nothing_and_stop_in_time() {
    use chronos::sql::run;
    let dir = std::env::temp_dir().join(format!("chronos-merge-checks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    run(
        &db,
        MAIN,
        "create table a (id int primary key); create table b (id int primary key); insert into b values (1)",
        &[],
    )
    .unwrap();
    // it finds a row already, so it refuses every merge it runs for
    run(&db, MAIN, "create merge check b_empty on tables (b) as select id from b", &[]).unwrap();
    db.fork(MAIN, "only_a").unwrap();
    run(&db, "only_a", "insert into a values (1)", &[]).unwrap();
    run(&db, MAIN, "merge world only_a", &[]).unwrap(); // doesn't change b: not run
    db.fork(MAIN, "alter_b").unwrap();
    run(&db, "alter_b", "alter table b add column note text", &[]).unwrap();
    let e = run(&db, MAIN, "merge world alter_b", &[]).unwrap_err();
    assert!(e.message.contains("b_empty, which finds (id = 1)"), "a change to b itself runs it: {}", e.message);
    assert_eq!(run(&db, MAIN, "create merge check b_empty as select 1", &[]).unwrap_err().code, "42710");
    let e = run(&db, MAIN, "create merge check typo on tables (bb) as select id from b", &[]).unwrap_err();
    assert!(e.code == "42P01" && e.message.contains("names bb"), "a misspelled scope would never run: {}", e.message);
    run(&db, MAIN, "create or replace merge check b_empty on tables (b) as select id from b where id > 1", &[])
        .unwrap();
    run(&db, MAIN, "merge world alter_b", &[]).unwrap();

    // a table in another schema is named as it's stored, found along the search_path or written out
    let mut s = chronos::sql::Session::new(MAIN);
    s.run(&db, "create schema app; create table app.t (id int primary key); set search_path to app, public", &[])
        .unwrap();
    s.run(&db, "create merge check app_t on tables (t) as select id from app.t", &[]).unwrap();
    run(&db, MAIN, "create merge check app_t2 on tables (app.t) as select id from app.t", &[]).unwrap();
    let tables = |n: &str| db.checks().unwrap().into_iter().find(|c| c.name == n).unwrap().tables;
    assert_eq!((tables("app_t"), tables("app_t2")), (vec!["app.t".to_string()], vec!["app.t".to_string()]));
    db.fork(MAIN, "in_app").unwrap();
    run(&db, "in_app", "insert into app.t values (7)", &[]).unwrap();
    let e = run(&db, MAIN, "merge world in_app", &[]).unwrap_err();
    assert!(e.message.contains("app_t, which finds (id = 7)"), "{}", e.message);
    run(&db, MAIN, "drop merge check app_t; drop merge check app_t2; merge world in_app", &[]).unwrap();

    // a check changes nothing: not a write, not a counter, not a function that could write
    run(&db, MAIN, "create sequence s; create function f() returns int language sql as 'select 1'", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "create merge check w as delete from a", &[]).unwrap_err().code, "42601");
    for q in ["select nextval('s')", "select setval('s', 5)", "select f()", "select pg_notify('c', 'x')"] {
        let e = run(&db, MAIN, &format!("create merge check w as {q}"), &[]).unwrap_err();
        assert_eq!(e.code, "25006", "{q}: {}", e.message);
    }
    assert_eq!(run(&db, MAIN, "select nextval('s')", &[]).unwrap()[0].rows[0][0].to_text(), "1", "s never moved");
    assert!(run(&db, MAIN, "create merge check w as select id from nowhere", &[]).is_err(), "can't run: refused now");
    assert_eq!(db.checks().unwrap().len(), 1);

    // a check that runs past its timeout refuses the merge: the merge waits for it, holding its locks
    run(&db, MAIN, "create table big (id int primary key)", &[]).unwrap();
    run(
        &db,
        MAIN,
        "create merge check slow on tables (big) with (timeout = '50ms') as select x.id from big x, big y where x.id + y.id < 0",
        &[],
    )
    .unwrap();
    assert_eq!(db.checks().unwrap().iter().find(|c| c.name == "slow").unwrap().timeout, 50);
    db.fork(MAIN, "fill").unwrap();
    run(&db, "fill", "insert into big select g from generate_series(1, 5000) g", &[]).unwrap();
    let t = std::time::Instant::now();
    let e = run(&db, MAIN, "merge world fill", &[]).unwrap_err();
    assert!(e.code == "23514" && e.message.contains("slow: it didn't finish within 50 ms"), "{}", e.message);
    assert!(t.elapsed() < Duration::from_secs(10), "stopped after {:?}", t.elapsed());
    assert_eq!(count(&db, MAIN, "big"), "0");
    assert!(run(&db, MAIN, "create merge check z with (timeout = 0) as select 1", &[]).is_err());
    run(&db, MAIN, "drop merge check slow", &[]).unwrap();
    run(&db, MAIN, "merge world fill", &[]).unwrap();

    // a restart: checks are kept, and replaying merges doesn't run them again
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.checks().unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["b_empty"]);
    assert_eq!((count(&db, MAIN, "a"), count(&db, MAIN, "big")), ("1".into(), "5000".into()));
    assert!(db.world("alter_b").is_err(), "merged before the restart");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Merge policies, deferred from PR #1: changes at once all land and never bring back a dropped
/// policy; DROP ... CASCADE releases its agents in the same step; a partial merge of a queued
/// world shows in SHOW REVIEWS without moving the world's version.
#[test]
fn merge_policies_change_atomically_and_reviews_see_partial_merges() {
    use chronos::sql::{Session, run};
    use std::sync::Arc;
    let db = Arc::new(Db::new());
    for i in 0..100 {
        let name = format!("p{i}");
        db.create_policy(&name, &serde_json::Value::Null).unwrap();
        let (a, b) = (db.clone(), db.clone());
        let (na, nb) = (name.clone(), name.clone());
        let t = std::thread::spawn(move || a.alter_policy(&na, &serde_json::json!({"max_rows": 7})).unwrap());
        b.alter_policy(&nb, &serde_json::json!({"max_deletes": 3})).unwrap();
        t.join().unwrap();
        let p = db.policy(&name).unwrap();
        assert_eq!((p.max_rows, p.max_deletes), (Some(7), Some(3)), "{name}: both changes land");
        // a change racing a drop never writes the policy back
        let (a, n2) = (db.clone(), name.clone());
        let t = std::thread::spawn(move || a.alter_policy(&n2, &serde_json::json!({"max_rows": 1})).is_ok());
        db.drop_policy(&name, false).unwrap();
        let _ = t.join().unwrap();
        assert!(db.policy(&name).is_err(), "{name} came back after its drop");
        // two creates at once: one wins
        let (a, n3) = (db.clone(), name.clone());
        let t = std::thread::spawn(move || a.create_policy(&n3, &serde_json::Value::Null).is_ok());
        let mine = db.create_policy(&name, &serde_json::Value::Null).is_ok();
        assert!(mine ^ t.join().unwrap(), "{name}: exactly one create");
        db.drop_policy(&name, false).unwrap();
    }

    // DROP ... CASCADE: its agents released in the same step
    run(&db, MAIN, "create table t (id int primary key, v text); create table u (id int primary key)", &[]).unwrap();
    run(&db, MAIN, "insert into t values (1, 'a'), (2, 'b')", &[]).unwrap();
    run(&db, MAIN, "create merge policy small with (max_rows = 1)", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'small'); create agent bot2 with (can = '{can}', policy = 'small')"), &[])
        .unwrap();
    assert_eq!(run(&db, MAIN, "drop merge policy small", &[]).unwrap_err().code, "22023");

    // first, a queued world partly approved: its entry says it changed, and its version moves, so
    // a merge at the version a person reviewed is refused (what's left isn't what was reviewed)
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(&db, "create world w; use world w; update t set v = 'x'; insert into u values (1); use world main", &[])
        .unwrap();
    assert!(s.run(&db, "merge world w", &[]).is_err());
    let version = db.world("w").unwrap().version;
    assert!(!db.reviews()[0].changed_since());
    run(&db, MAIN, "merge world w only tables (u)", &[]).unwrap();
    let r = db.reviews();
    assert!(r[0].partly_merged && r[0].changed_since(), "{r:?}");
    assert_eq!(db.world("w").unwrap().version, version + 1, "what's left to merge changed");
    let e = run(&db, MAIN, &format!("merge world w at version {version}"), &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    let shown = run(&db, MAIN, "show reviews", &[]).unwrap().pop().unwrap();
    assert_eq!(shown.rows[0][6].to_text(), "true");

    run(&db, MAIN, "drop merge policy small cascade", &[]).unwrap();
    assert!(db.policy("small").is_err());
    assert!(db.agent("bot").unwrap().policy.is_none() && db.agent("bot2").unwrap().policy.is_none());
}

/// A person approves the version they reviewed: an agent that keeps writing to its world while it
/// waits doesn't get its later writes merged with it, by SQL, the HTTP API's merge, or a dry run's
/// version.
#[test]
fn an_approval_merges_only_the_version_reviewed() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, v text); create merge policy small with (max_rows = 1)", &[])
        .unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'small')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(&db, "create world w; use world w; insert into t values (1, 'a'), (2, 'b'); use world main", &[]).unwrap();
    assert_eq!(bot.run(&db, "merge world w", &[]).unwrap_err().code, "42501");
    let reviewed = run(&db, MAIN, "show reviews", &[]).unwrap().pop().unwrap().rows[0][5].to_text();
    let plan = db.merge_preview("w", &Default::default()).unwrap();
    assert_eq!(plan.version.to_string(), reviewed, "the dry run is for the version the review shows");

    // the agent writes on while the person reads
    bot.run(&db, "use world w; insert into t values (3, 'sneaked in'); use world main", &[]).unwrap();
    let e = run(&db, MAIN, &format!("merge world w at version {reviewed}"), &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    assert!(e.message.contains("it changed since"), "{}", e.message);
    assert!(
        db.world("w").is_ok() && run(&db, MAIN, "select * from t", &[]).unwrap()[0].rows.is_empty(),
        "nothing merged"
    );
    let e = run(&db, MAIN, &format!("merge world w at version {reviewed} confirm"), &[]).unwrap_err();
    assert_eq!(e.code, "42601", "{}", e.message);

    // read again, approve what's there now
    let now = db.world("w").unwrap().version;
    run(&db, MAIN, &format!("merge world w at version {now}"), &[]).unwrap();
    assert_eq!(run(&db, MAIN, "select count(*) from t", &[]).unwrap()[0].rows[0][0].to_text(), "3");
}

/// A preview that settles rows (USING OURS, BY COLUMNS, RESOLVE) settles them against the world
/// merged into as it was: a merge pinned to that (TARGET) is refused if that world changed since,
/// even with the branch unchanged, so it can't overwrite a change nobody saw.
#[test]
fn a_settled_merge_is_pinned_to_the_target_the_preview_saw() {
    use chronos::sql::run;
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, v text); insert into t values (1, 'a')", &[]).unwrap();
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(&db, "w", "update t set v = 'w'", &[]).unwrap();
    run(&db, MAIN, "update t set v = 'm'", &[]).unwrap();
    let o = chronos::MergeOptions { how: chronos::Resolve::Ours, ..Default::default() };
    let plan = db.merge_preview("w", &o).unwrap();
    // main changes the row again after the preview; the branch doesn't
    run(&db, MAIN, "update t set v = 'unseen'", &[]).unwrap();
    let q = format!("merge world w using ours at version {} target {}", plan.version, plan.target);
    let e = run(&db, MAIN, &q, &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    assert!(e.message.contains("changed since the preview"), "{}", e.message);
    let v = run(&db, MAIN, "select v from t", &[]).unwrap()[0].rows[0][0].to_text();
    assert_eq!(v, "unseen", "nothing merged");
    // a merge into it moves it too
    let plan = db.merge_preview("w", &o).unwrap();
    run(&db, MAIN, "create world other; ", &[]).unwrap();
    run(&db, "other", "create table u (id int primary key)", &[]).unwrap();
    run(&db, MAIN, "merge world other", &[]).unwrap();
    let q = format!("merge world w using ours at version {} target {}", plan.version, plan.target);
    assert_eq!(run(&db, MAIN, &q, &[]).unwrap_err().code, "55000");
    // previewed again: it merges
    let plan = db.merge_preview("w", &o).unwrap();
    run(&db, MAIN, &format!("merge world w using ours at version {} target {}", plan.version, plan.target), &[])
        .unwrap();
    assert_eq!(run(&db, MAIN, "select v from t", &[]).unwrap()[0].rows[0][0].to_text(), "w");
}
