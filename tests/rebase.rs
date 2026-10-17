//! REBASE WORLD: a long-running world takes in what changed in its parent since its fork, its own
//! changes kept on top, so it settles conflicts as they come instead of all at its merge. The
//! parent doesn't change; nothing does when the rebase is refused.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn one(db: &Db, world: &str, sql: &str) -> String {
    run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap().rows[0][0].to_text()
}

fn shop(db: &Db) {
    run(
        db,
        MAIN,
        "create table t (id int primary key, v text, n int); insert into t values (1, 'a', 1), (2, 'b', 2), (3, 'c', 3);
         create table u (id int primary key, email text unique)",
        &[],
    )
    .unwrap();
}

#[test]
fn a_world_takes_in_its_parents_changes_and_keeps_its_own() {
    let db = Db::new();
    shop(&db);
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(&db, "w", "update t set v = 'mine' where id = 1", &[]).unwrap();
    run(&db, MAIN, "update t set v = 'theirs' where id = 2; insert into t values (4, 'd', 4)", &[]).unwrap();
    let before = db.world("w").unwrap().version;
    assert_eq!(run(&db, MAIN, "rebase world w", &[]).unwrap()[0].tag, "REBASE 2");
    assert_eq!(db.world("w").unwrap().version, before + 1);
    assert_eq!(one(&db, "w", "select string_agg(v, ',' order by id) from t"), "mine,theirs,c,d");
    // its diff is its own change alone, against the parent as it is
    let diff = db.diff("w").unwrap();
    assert_eq!(diff.iter().filter(|c| c.key.starts_with("t/")).count(), 1, "{diff:?}");
    assert_eq!(one(&db, MAIN, "select v from t where id = 1"), "a", "the parent doesn't change");
    run(&db, MAIN, "merge world w", &[]).unwrap();
    assert_eq!(one(&db, MAIN, "select string_agg(v, ',' order by id) from t"), "mine,theirs,c,d");
}

#[test]
fn conflicts_are_refused_or_settled_as_the_rebase_says() {
    let db = Db::new();
    shop(&db);
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(&db, "w", "update t set v = 'w' where id = 1; update t set n = 30 where id = 3", &[]).unwrap();
    run(&db, MAIN, "update t set v = 'm' where id = 1; update t set v = 'cc' where id = 3", &[]).unwrap();
    let version = db.world("w").unwrap().version;
    let e = run(&db, MAIN, "rebase world w", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert_eq!(db.world("w").unwrap().version, version, "nothing changed");
    assert_eq!(one(&db, "w", "select v from t where id = 1"), "w");
    // AT VERSION, as for a merge
    assert_eq!(
        run(&db, MAIN, &format!("rebase world w using theirs at version {}", version + 7), &[]).unwrap_err().code,
        "55000"
    );
    // row 3: different columns, combined; row 1: theirs
    run(&db, MAIN, &format!("rebase world w by columns resolve ('t/1' = theirs) at version {version}"), &[]).unwrap();
    assert_eq!(one(&db, "w", "select v || n from t where id = 1"), "m1");
    assert_eq!(one(&db, "w", "select v || n from t where id = 3"), "cc30");
    run(&db, MAIN, "merge world w", &[]).unwrap(); // no conflicts left: they were settled
    assert_eq!(one(&db, MAIN, "select v || n from t where id = 3"), "cc30");
    // what both sides added under one unique value is refused as a merge would be
    run(&db, MAIN, "create world x", &[]).unwrap();
    run(&db, "x", "insert into u values (1, 'a@x')", &[]).unwrap();
    run(&db, MAIN, "insert into u values (2, 'a@x')", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "rebase world x", &[]).unwrap_err().code, "23505");
    // INTO, ONLY, CONFIRM, DRY RUN aren't a rebase's; nor a transaction
    for bad in [
        "rebase world x into main",
        "rebase world x only tables (t)",
        "rebase world x confirm",
        "rebase world x dry run",
    ] {
        assert_eq!(run(&db, MAIN, bad, &[]).unwrap_err().code, "42601", "{bad}");
    }
    assert!(run(&db, MAIN, "rebase world main", &[]).is_err());
    let mut s = Session::new("x");
    let e = s.run(&db, "begin; rebase world x", &[]).unwrap_err();
    assert_eq!(e.code, "25001", "{}", e.message);
}

#[test]
fn a_rebased_world_cant_go_back_past_its_rebase() {
    let db = Db::new();
    shop(&db);
    run(&db, MAIN, "create world w; checkpoint world w as start", &[]).unwrap();
    run(&db, "w", "update t set v = 'w' where id = 1", &[]).unwrap();
    run(&db, MAIN, "update t set v = 'm' where id = 2", &[]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    run(&db, MAIN, "rebase world w", &[]).unwrap();
    // put back on the new fork point, its old rows would undo the parent's change at its merge
    let e = run(&db, MAIN, "restore world w to checkpoint start", &[]).unwrap_err();
    assert_eq!(e.code, "55000", "{}", e.message);
    assert!(e.message.contains("was rebased"), "{}", e.message);
    run(&db, MAIN, "merge world w", &[]).unwrap();
    assert_eq!(one(&db, MAIN, "select string_agg(v, ',' order by id) from t"), "w,m,c");
}

#[test]
fn an_agent_rebases_its_own_world_within_its_policy() {
    let db = Db::new();
    shop(&db);
    run(&db, MAIN, "create merge policy careful", &[]).unwrap(); // overwrite = false
    let can = "read,fork,write_own,write_main,merge_own";
    run(
        &db,
        MAIN,
        &format!("create agent bot with (can = '{can}', policy = 'careful'); create agent other with (can = '{can}')"),
        &[],
    )
    .unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(&db, "create world w; use world w; update t set v = 'bot' where id = 1", &[]).unwrap();
    run(&db, MAIN, "update t set v = 'person' where id = 1; update t set v = 'x' where id = 2", &[]).unwrap();
    // keeping its row over the parent's change would hide the overwrite from its merge's review
    let e = bot.run(&db, "rebase world w using ours", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("overwrite"), "{}", e.message);
    bot.run(&db, "rebase world w using theirs", &[]).unwrap();
    assert_eq!(one(&db, "w", "select string_agg(v, ',' order by id) from t"), "person,x,c");
    // another agent's world isn't its to rebase
    let mut other = Session::new(MAIN);
    other.agent = Some("other".into());
    assert_eq!(other.run(&db, "rebase world w", &[]).unwrap_err().code, "42501");
}

#[test]
fn a_rebase_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!("chronos-rebase-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        shop(&db);
        run(&db, MAIN, "create world w", &[]).unwrap();
        run(&db, "w", "update t set v = 'w' where id = 1; update t set n = 9 where id = 2", &[]).unwrap();
        run(&db, MAIN, "update t set v = 'm' where id = 2; update t set v = 'm' where id = 1", &[]).unwrap();
        run(&db, MAIN, "checkpoint world w as before", &[]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        run(&db, MAIN, "rebase world w by columns resolve ('t/1' = ours)", &[]).unwrap();
        run(&db, "w", "update t set v = 'after' where id = 3", &[]).unwrap();
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(one(&db, "w", "select string_agg(v || n, ',' order by id) from t"), "w1,m9,after3");
    // in the world's history, the world still open
    let what: Vec<String> = run(&db, MAIN, "show history for world w", &[])
        .unwrap()
        .pop()
        .unwrap()
        .rows
        .iter()
        .map(|r| r[2].to_text())
        .collect();
    assert!(what.iter().any(|w| w == "rebased onto main"), "{what:?}");
    // the rebase's boundary came back with it, from its own log record
    assert_eq!(run(&db, MAIN, "restore world w to checkpoint before", &[]).unwrap_err().code, "55000");
    let keys: Vec<String> = db.diff("w").unwrap().into_iter().map(|c| c.key).filter(|k| k.starts_with("t/")).collect();
    assert_eq!(keys.len(), 3, "{keys:?}");
    run(&db, MAIN, "merge world w", &[]).unwrap();
    assert_eq!(one(&db, MAIN, "select string_agg(v || n, ',' order by id) from t"), "w1,m9,after3");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_agents_rebase_is_in_its_audit_trail() {
    let dir = std::env::temp_dir().join(format!("chronos-rebase-audit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    shop(&db);
    run(&db, MAIN, "create agent bot with (can = 'read,fork,write_own')", &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(&db, "create world w; use world w; update t set v = 'bot' where id = 1", &[]).unwrap();
    run(&db, MAIN, "update t set v = 'x' where id = 2", &[]).unwrap();
    bot.run(&db, "rebase world w", &[]).unwrap();
    let audit = run(&db, MAIN, "show audit for agent bot", &[]).unwrap().pop().unwrap();
    let actions: Vec<(String, String)> = audit.rows.iter().map(|r| (r[2].to_text(), r[3].to_text())).collect();
    assert!(actions.contains(&("w".into(), "rebase onto main".into())), "{actions:?}");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
