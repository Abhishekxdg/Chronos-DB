//! Adversarial: an agent must not escape its capability checks by wrapping a privileged
//! statement in a dynamic EXECUTE, a DO block, a function body or a trigger body. The
//! front-door `agent_check` only sees the outer statement, so the nested executor has to
//! re-apply the gate. Each case runs the privileged statement through a nested path and
//! asserts it is refused (42501) and that no state changed; positive controls make sure the
//! gate isn't over-broad.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};

/// A world-owning agent held to a merge policy, with only read/fork/write_own, already in its
/// own world. Returns a session that acts as the agent.
fn restricted_agent(db: &Db) -> Session {
    run(
        db,
        MAIN,
        "create merge policy agents with (max_deletes = 0, max_rows = 500);
         create table t (id int primary key, v text);
         insert into t values (1, 'one')",
        &[],
    )
    .unwrap();
    db.create_agent(
        "bot",
        &AgentSpec {
            can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn]),
            policy: Some("agents".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(db, "create world w; use world w", &[]).unwrap();
    s
}

#[test]
fn agent_can_write_through_a_body_inside_a_transaction() {
    // regression: the nested check must not re-run `may_write` on the branch, which inside a
    // transaction is the private `_tx_*` world that `may_write` rejects — a legitimate write in a
    // DO body inside BEGIN/COMMIT would wrongly fail. The outer statement already authorized the
    // write for this world; only world/system statements are stopped here.
    let db = Db::new();
    let mut s = restricted_agent(&db); // acting in its own world `w`
    s.run(&db, "begin", &[]).unwrap();
    s.run(&db, "do $$ begin execute 'insert into t values (5, ''tx-body'')'; end $$", &[]).unwrap();
    s.run(&db, "commit", &[]).unwrap();
    assert_eq!(run(&db, "w", "select count(*) from t", &[]).unwrap()[0].rows[0][0].to_text(), "2");

    // but escalation from a body inside a transaction is still refused
    s.run(&db, "begin", &[]).unwrap();
    assert_eq!(
        s.run(&db, "do $$ begin execute 'alter agent bot set (can = ''admin'')'; end $$", &[]).unwrap_err().code,
        "42501"
    );
    let _ = s.run(&db, "rollback", &[]);
    assert!(!db.agent("bot").unwrap().may(Can::Admin));
}

/// The number of merge policies, read as a person.
fn policy_count(db: &Db) -> usize {
    run(db, MAIN, "show merge policies", &[]).unwrap()[0].rows.len()
}

/// SQL-escape a single-quoted string for embedding inside another single-quoted literal.
fn q(sql: &str) -> String {
    sql.replace('\'', "''")
}

/// Run `inner` three ways as the agent — direct, inside a DO block, inside a PL/pgSQL
/// function body invoked by SELECT — and assert every one is refused with 42501.
fn refused_every_path(db: &Db, s: &mut Session, inner: &str) {
    let direct = s.run(db, inner, &[]);
    assert_eq!(direct.unwrap_err().code, "42501", "direct: {inner}");

    let do_block = format!("do $$ begin execute '{}'; end $$", q(inner));
    assert_eq!(s.run(db, &do_block, &[]).unwrap_err().code, "42501", "DO block: {inner}");

    let func =
        format!("create function esc() returns void language plpgsql as $$ begin execute '{}'; end $$;", q(inner));
    s.run(db, &func, &[]).unwrap();
    assert_eq!(s.run(db, "select esc()", &[]).unwrap_err().code, "42501", "function body: {inner}");
    s.run(db, "drop function esc", &[]).ok();
}

#[test]
fn agent_cannot_escalate_capabilities_through_any_body() {
    let db = Db::new();
    let mut s = restricted_agent(&db);
    refused_every_path(&db, &mut s, "alter agent bot set (can = 'admin')");
    assert!(!db.agent("bot").unwrap().may(Can::Admin), "bot must not have gained admin");
    assert!(!db.agent("bot").unwrap().may(Can::WriteMain), "bot must not have gained write_main");
}

#[test]
fn agent_cannot_create_an_agent_through_any_body() {
    let db = Db::new();
    let mut s = restricted_agent(&db);
    refused_every_path(&db, &mut s, "create agent evil with (can = 'admin')");
    assert!(db.as_agent("evil").is_err(), "no new agent may exist");
}

#[test]
fn agent_cannot_change_the_system_through_any_body() {
    let db = Db::new();
    let mut s = restricted_agent(&db);
    refused_every_path(&db, &mut s, "alter system set history_retention = '0'");
}

#[test]
fn agent_cannot_change_or_drop_a_policy_through_any_body() {
    let db = Db::new();
    let mut s = restricted_agent(&db);
    let before = policy_count(&db);
    refused_every_path(&db, &mut s, "drop merge policy agents");
    refused_every_path(&db, &mut s, "alter merge policy agents set (max_deletes = 1000)");
    assert_eq!(policy_count(&db), before, "the policy must be untouched");
}

#[test]
fn agent_cannot_escalate_through_a_trigger_body() {
    let db = Db::new();
    let mut s = restricted_agent(&db);
    s.run(
        &db,
        "create function sneak() returns trigger language plpgsql as \
         $$ begin execute 'alter agent bot set (can = ''admin'')'; return new; end $$;",
        &[],
    )
    .unwrap();
    s.run(&db, "create trigger tr before insert on t for each row execute function sneak()", &[]).unwrap();
    // the escalation fires from inside the trigger; the insert must fail and bot stay un-escalated
    assert_eq!(s.run(&db, "insert into t values (2, 'two')", &[]).unwrap_err().code, "42501");
    assert!(!db.agent("bot").unwrap().may(Can::Admin), "bot must not have gained admin via trigger");
}

#[test]
fn agent_can_still_do_ordinary_work_through_a_body() {
    let db = Db::new();
    let mut s = restricted_agent(&db);
    // DML in its own world via EXECUTE: a Reach::Write statement, must still work
    s.run(&db, "do $$ begin execute 'insert into t values (2, ''two'')'; end $$", &[]).unwrap();
    assert_eq!(run(&db, "w", "select count(*) from t", &[]).unwrap()[0].rows[0][0].to_text(), "2");
    // a schema change in its own world via EXECUTE: also Reach::Write, must still work
    s.run(&db, "do $$ begin execute 'create table t2 (id int primary key)'; end $$", &[]).unwrap();
    assert!(run(&db, "w", "select count(*) from t2", &[]).is_ok(), "t2 should exist in the agent's world");
    // a pure read through a function body: must still work
    s.run(
        &db,
        "create function cnt() returns bigint language plpgsql as $$ begin return (select count(*) from t); end $$",
        &[],
    )
    .unwrap();
    assert_eq!(s.run(&db, "select cnt()", &[]).unwrap()[0].rows[0][0].to_text(), "2");
}
