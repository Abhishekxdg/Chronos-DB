//! TRANSFER WORLD w TO agent: the world gets a new owner and its lease epoch goes up by one; the old owner is fenced
//! (its writes and merges there get 55P03, lease lost). Only the owner or an admin transfers, and only to an agent whose
//! rights cover the world.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create table t (id int primary key, v text); insert into t values (1, 'a');
         create agent a with (can = 'read,fork,write_own,merge_own,write');
         create agent b with (can = 'read,fork,write_own,merge_own');
         create agent reader with (can = 'read');
         create agent tenanted with (can = 'read,fork,write_own', tenant = 'id = 1')",
        &[],
    )
    .unwrap();
}

fn agent(name: &str, world: &str) -> Session {
    let mut s = Session::new(world);
    s.agent = Some(name.into());
    s
}

fn code(r: Result<Vec<chronos::sql::Output>, chronos::sql::SqlError>) -> &'static str {
    match r {
        Ok(_) => panic!("went through"),
        Err(e) => e.code,
    }
}

#[test]
fn transfer_fences_old_owner() {
    let db = Db::new();
    setup(&db);
    let mut a = agent("a", MAIN);
    a.run(&db, "create world w; use world w; insert into t values (2, 'a')", &[]).unwrap();
    assert_eq!(db.world_epoch("w").unwrap(), 0);
    // not to an agent whose rights don't cover it, nor by one that doesn't own it
    assert_eq!(code(a.run(&db, "transfer world w to reader", &[])), "42501");
    assert_eq!(code(a.run(&db, "transfer world w to tenanted", &[])), "42501");
    assert_eq!(code(agent("b", MAIN).run(&db, "transfer world w to b", &[])), "42501");
    let out = a.run(&db, "transfer world w to b", &[]).unwrap();
    assert_eq!(out[0].rows[0][1].to_text(), "b");
    assert_eq!(out[0].rows[0][2].to_text(), "1");
    let w = db.world("w").unwrap();
    assert_eq!((w.owner.as_deref(), db.world_epoch("w").unwrap()), (Some("b"), 1));

    // a, still in w with a session, is fenced though it has 'write'
    assert_eq!(code(a.run(&db, "insert into t values (3, 'a')", &[])), "55P03");
    assert_eq!(code(agent("a", MAIN).run(&db, "merge world w into main", &[])), "55P03");
    assert_eq!(code(agent("a", MAIN).run(&db, "transfer world w to a", &[])), "42501");
    agent("b", "w").run(&db, "insert into t values (4, 'b')", &[]).unwrap();
    let audit: Vec<String> = db.security_events(20).into_iter().map(|e| format!("{} {}", e.action, e.target)).collect();
    assert!(audit.iter().any(|x| x.starts_with("transfer world") && x.contains("epoch 1")), "{audit:?}");

    // back to a by the database's own user: a is no longer fenced, b is
    run(&db, MAIN, "transfer world w to a", &[]).unwrap();
    assert_eq!(db.world_epoch("w").unwrap(), 2);
    agent("a", "w").run(&db, "insert into t values (5, 'a')", &[]).unwrap();
    assert_eq!(code(agent("b", "w").run(&db, "insert into t values (6, 'b')", &[])), "55P03");
    assert_eq!(code(run(&db, MAIN, "transfer world main to a", &[])), "42501");
}

#[test]
fn transfer_survives_restart() {
    let dir = std::env::temp_dir().join(format!("chronos-world-transfer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        setup(&db);
        agent("a", MAIN).run(&db, "create world w", &[]).unwrap();
        agent("a", MAIN).run(&db, "transfer world w to b", &[]).unwrap();
        agent("b", MAIN).run(&db, "grant write on world w to reader", &[]).unwrap();
    }
    let db = Db::open(&dir).unwrap();
    let w = db.world("w").unwrap();
    assert_eq!((w.owner.as_deref(), db.world_epoch("w").unwrap()), (Some("b"), 1));
    assert_eq!(code(agent("a", "w").run(&db, "insert into t values (2, 'a')", &[])), "55P03");
    agent("b", "w").run(&db, "insert into t values (3, 'b')", &[]).unwrap();
    agent("reader", "w").run(&db, "insert into t values (4, 'r')", &[]).unwrap(); // the grant came back too
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
