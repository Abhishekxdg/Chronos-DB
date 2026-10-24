//! Every path by which an agent could change main must go through its capabilities and merge
//! policy — none may skip them. An agent under a merge policy changes main only by merging a world
//! of its own; direct DML, writable CTEs, SQL MERGE, transaction commits, RESTORE and UNDO into
//! main are all refused, and main is left untouched. This holds even for an agent that also has
//! `write_main`: the policy still forces the merge path.
//!
//! (The one path that used to skip the checks — a statement wrapped in a dynamic EXECUTE / function
//! / trigger body — is closed separately, with its own adversarial suite.)

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create merge policy p with (max_deletes = 0);
         create table t (id int primary key, v text);
         insert into t values (1, 'one'), (2, 'two')",
        &[],
    )
    .unwrap();
}

fn main_rows(db: &Db) -> String {
    run(db, MAIN, "select count(*) from t", &[]).unwrap()[0].rows[0][0].to_text()
}

/// A session acting as a policy agent with the given capabilities, on main.
fn agent_session(db: &Db, can: Vec<Can>) -> Session {
    db.create_agent("bot", &AgentSpec { can: Some(can), policy: Some("p".into()), ..Default::default() }).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    s
}

#[test]
fn a_policy_agent_cannot_reach_main_by_any_write_path() {
    let db = Db::new();
    setup(&db);
    // the fullest hand short of merging: it may write anywhere but main, and merge its own worlds
    let mut s =
        agent_session(&db, vec![Can::Read, Can::Fork, Can::WriteOwn, Can::Write, Can::WriteMain, Can::MergeOwn]);
    for q in [
        "insert into t values (3, 'three')",
        "update t set v = 'x' where id = 1",
        "delete from t where id = 1",
        "truncate t",
        "merge into t using (select 3 as id, 'm' as v) s on t.id = s.id when not matched then insert (id, v) values (s.id, s.v)",
        "with ins as (insert into t values (4, 'cte') returning id) select * from ins",
        "restore world main to '-1 minute'",
    ] {
        let e = s.run(&db, q, &[]).unwrap_err();
        assert_eq!(e.code, "42501", "expected refusal for `{q}`, got: {}", e.message);
    }
    assert_eq!(main_rows(&db), "2", "main is untouched by every refused path");
}

#[test]
fn a_transaction_commit_cannot_slip_changes_into_main() {
    let db = Db::new();
    setup(&db);
    let mut s = agent_session(&db, vec![Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain, Can::MergeOwn]);
    // begin/insert/commit on main: the commit would merge into main, which the policy routes away
    // from a silent write — the sequence is refused and main is unchanged
    assert_eq!(s.run(&db, "begin; insert into t values (5, 'tx'); commit", &[]).unwrap_err().code, "42501");
    let _ = s.run(&db, "rollback", &[]);
    assert_eq!(main_rows(&db), "2");
}

#[test]
fn a_policy_agent_cannot_undo_into_main() {
    let db = Db::new();
    setup(&db);
    // give it undo-ish rights outright; the policy still forbids undo (it writes past the rules)
    let mut s = agent_session(&db, vec![Can::Read, Can::Fork, Can::WriteOwn, Can::Restore, Can::Merge, Can::WriteMain]);
    assert_eq!(s.run(&db, "undo agent bot since '-1 hour'", &[]).unwrap_err().code, "42501");
    assert_eq!(s.run(&db, "restore world main to '-1 minute'", &[]).unwrap_err().code, "42501");
    assert_eq!(main_rows(&db), "2");
}

#[test]
fn the_sanctioned_path_still_works_and_the_policy_still_bites() {
    let db = Db::new();
    setup(&db);
    let mut s = agent_session(&db, vec![Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain, Can::MergeOwn]);
    // fork, change the world (allowed), merge — within the policy (an insert, no deletes) it lands
    s.run(&db, "create world w; use world w; insert into t values (3, 'three')", &[]).unwrap();
    s.run(&db, "use world main; merge world w", &[]).unwrap();
    assert_eq!(main_rows(&db), "3", "an in-policy merge is the one way in");

    // a delete breaks max_deletes = 0: the merge is held for review, not applied
    s.run(&db, "create world d; use world d; delete from t where id = 1", &[]).unwrap();
    let e = s.run(&db, "use world main; merge world d", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("review") || e.message.contains("queued"), "{}", e.message);
    assert_eq!(main_rows(&db), "3", "the delete did not reach main");
}
