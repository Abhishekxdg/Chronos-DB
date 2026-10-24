//! Rotating an agent's token: the old token stops working at once, a fresh one is issued, and
//! the agent's identity (name, id, capabilities, owned worlds) is unchanged. Only an admin may
//! rotate; the rotation time is recorded.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};

#[test]
fn rotation_kills_the_old_token_and_keeps_identity() {
    let db = Db::new();
    let (a, tok1) = db
        .create_agent("bot", &AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn]), ..Default::default() })
        .unwrap();
    assert_eq!(db.agent_by_token(&tok1).unwrap().name, "bot");
    assert_eq!(db.agent("bot").unwrap().rotated, 0, "not rotated yet");

    let tok2 = db.rotate_agent_token("bot").unwrap();
    assert_ne!(tok1, tok2, "a fresh token");
    assert!(db.agent_by_token(&tok1).is_err(), "the old token no longer works");
    assert_eq!(db.agent_by_token(&tok2).unwrap().name, "bot", "the new token works");

    let after = db.agent("bot").unwrap();
    assert_eq!(after.id, a.id, "the id is stable across rotation");
    assert_eq!(after.can, a.can, "capabilities are unchanged");
    assert!(after.rotated > 0, "the rotation time is recorded");
}

#[test]
fn rotation_keeps_owned_worlds() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn]), ..Default::default() })
        .unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(&db, "create world w", &[]).unwrap();
    assert_eq!(db.world("w").unwrap().owner.as_deref(), Some("bot"));
    db.rotate_agent_token("bot").unwrap();
    assert_eq!(db.world("w").unwrap().owner.as_deref(), Some("bot"), "it still owns its world after rotation");
}

#[test]
fn rotate_over_sql_shows_the_new_token_once() {
    let db = Db::new();
    let (_a, tok1) = db.create_agent("bot", &AgentSpec::default()).unwrap();
    let out = run(&db, MAIN, "alter agent bot rotate token", &[]).unwrap();
    let row = &out[0].rows[0];
    assert_eq!(row[0].to_text(), "bot");
    let tok2 = row[2].to_text();
    assert!(tok2.starts_with("chronos_"));
    assert!(db.agent_by_token(&tok1).is_err());
    assert_eq!(db.agent_by_token(&tok2).unwrap().name, "bot");
}

#[test]
fn an_agent_cannot_rotate_tokens_itself() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn]), ..Default::default() })
        .unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    // rotating a token manages agents: it needs 'admin', which bot doesn't have
    assert_eq!(s.run(&db, "alter agent bot rotate token", &[]).unwrap_err().code, "42501");
}

#[test]
fn rotating_a_missing_or_reserved_agent_is_refused() {
    let db = Db::new();
    assert!(db.rotate_agent_token("ghost").is_err(), "no such agent");
    assert!(db.rotate_agent_token("guest").is_err(), "the guest has no token");
}
