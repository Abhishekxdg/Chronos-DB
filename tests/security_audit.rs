//! The security audit records privileged actions — agents and merge policies created, changed or
//! dropped, tokens rotated, ALTER SYSTEM — since the server started, with who did each. It is
//! readable only by the database's own user or an admin, through SHOW SECURITY AUDIT.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};

/// (action, target) pairs in the trail, newest first.
fn trail(db: &Db) -> Vec<(String, String)> {
    db.security_events(100).into_iter().map(|e| (e.action, e.target)).collect()
}

#[test]
fn privileged_actions_are_recorded() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    db.alter_agent("bot", &AgentSpec { max_worlds: Some(5), ..Default::default() }).unwrap();
    db.rotate_agent_token("bot").unwrap();
    run(&db, MAIN, "create merge policy p with (max_deletes = 0)", &[]).unwrap();
    run(&db, MAIN, "alter merge policy p set (max_rows = 100)", &[]).unwrap();
    run(&db, MAIN, "alter system set max_worlds = 1000", &[]).unwrap();
    db.drop_agent("bot").unwrap();

    let t = trail(&db);
    // newest first: the drop is first, the create agent last
    assert_eq!(t.first().unwrap().0, "drop agent");
    assert!(t.contains(&("create agent".into(), "bot".into())), "{t:?}");
    assert!(t.contains(&("alter agent".into(), "bot".into())), "{t:?}");
    assert!(t.contains(&("rotate token".into(), "bot".into())), "{t:?}");
    assert!(t.contains(&("create merge policy".into(), "p".into())), "{t:?}");
    assert!(t.contains(&("alter merge policy".into(), "p".into())), "{t:?}");
    assert!(t.iter().any(|(a, tg)| a == "alter system" && tg.starts_with("max_worlds")), "{t:?}");
}

#[test]
fn show_security_audit_lists_them_for_the_owner() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let out = run(&db, MAIN, "show security audit", &[]).unwrap();
    let rows = &out[0].rows;
    assert!(!rows.is_empty());
    // columns: at, actor, action, target
    let top = &rows[0];
    assert_eq!(top[1].to_text(), "owner", "an owner action is attributed to 'owner'");
    assert_eq!(top[2].to_text(), "create agent");
    assert_eq!(top[3].to_text(), "bot");
    assert!(run(&db, MAIN, "show security audit limit 1", &[]).unwrap()[0].rows.len() == 1);
}

#[test]
fn an_admin_agents_action_is_attributed_to_it() {
    let db = Db::new();
    db.create_agent("boss", &AgentSpec { can: Some(vec![Can::Admin]), ..Default::default() }).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("boss".into());
    s.run(&db, "create agent minion", &[]).unwrap();
    let t = db.security_events(100);
    let made = t.iter().find(|e| e.action == "create agent" && e.target == "minion").expect("recorded");
    assert_eq!(made.actor, "boss", "attributed to the admin agent that did it");
}

#[test]
fn a_non_admin_agent_cannot_read_the_security_audit() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn]), ..Default::default() })
        .unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    assert_eq!(s.run(&db, "show security audit", &[]).unwrap_err().code, "42501");
}
