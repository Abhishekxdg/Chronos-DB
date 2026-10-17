//! Merge policies' hourly limits: `rows_per_hour` and `deletes_per_hour` count an agent's merges
//! into main together, so a job split across several worlds is judged as a whole. Over the
//! limit, the merge waits for a person; a person's merges don't count, nor do merges between
//! the agent's own worlds.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn bot(db: &Db) -> Session {
    let can = "read,fork,write_own,write_main,merge_own";
    run(db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'hourly')"), &[]).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    s
}

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create table leads (id int primary key, who text);
         insert into leads select g, 'x' from generate_series(1, 20) g;
         create merge policy hourly with (rows_per_hour = 10, deletes_per_hour = 3)",
        &[],
    )
    .unwrap();
}

/// As the agent: forks `w`, runs `sql` in it and merges it; the merge's error, if any.
fn attempt(db: &Db, s: &mut Session, w: &str, sql: &str) -> Option<String> {
    s.run(db, &format!("create world {w}; use world {w}; {sql}; use world main"), &[]).unwrap();
    s.run(db, &format!("merge world {w}"), &[]).err().map(|e| {
        assert_eq!(e.code, "42501", "{}", e.message);
        e.message
    })
}

#[test]
fn an_agents_merges_count_together_over_the_hour() {
    let db = Db::new();
    setup(&db);
    let mut s = bot(&db);
    assert_eq!(attempt(&db, &mut s, "a", "delete from leads where id in (1, 2)"), None);
    // two more deletes: four in the hour
    let e = attempt(&db, &mut s, "b", "delete from leads where id in (3, 4)").unwrap();
    assert!(e.contains("deletes 2 rows, and its merges deleted 2 in the last hour (at most 3 an hour)"), "{e}");
    assert!(e.contains("SHOW REVIEWS"), "{e}");
    // a dry run as the agent says so too (as a person, no policy applies)
    let plan = s.run(&db, "merge world b dry run", &[]).unwrap().pop().unwrap();
    let said = plan.rows.iter().flatten().any(|v| v.to_text().contains("deleted 2 in the last hour"));
    assert!(said, "{:?}", plan.rows);
    // a person's approval isn't the agent's merge: it doesn't count
    run(&db, MAIN, "merge world b", &[]).unwrap();
    assert_eq!(attempt(&db, &mut s, "c", "delete from leads where id = 5"), None); // 3 of 3

    // rows likewise: 3 so far, 8 more is 11
    let e = attempt(&db, &mut s, "d", "insert into leads select g, 'y' from generate_series(21, 28) g").unwrap();
    assert!(e.contains("changes 8 rows, and its merges changed 3 in the last hour (at most 10 an hour)"), "{e}");

    // merges between its own worlds don't count; only reaching main does
    s.run(
        &db,
        "create world e; create world f from e; use world f; update leads set who = 'z' where id in (6, 7)",
        &[],
    )
    .unwrap();
    s.run(&db, "use world main; merge world f", &[]).unwrap();
    s.run(&db, "merge world e", &[]).unwrap(); // 3 + 2 = 5 rows
    assert_eq!(attempt(&db, &mut s, "g", "update leads set who = 'w' where id between 8 and 12"), None); // 10
    assert!(attempt(&db, &mut s, "h", "update leads set who = 'v' where id = 13").is_some());
}

#[test]
fn the_counts_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("chronos-agent-limits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        setup(&db);
        let mut s = bot(&db);
        assert_eq!(attempt(&db, &mut s, "a", "delete from leads where id in (1, 2, 3)"), None);
    }
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    let e = attempt(&db, &mut s, "b", "delete from leads where id = 4").unwrap();
    assert!(e.contains("its merges deleted 3 in the last hour"), "{e}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_world_older_than_max_age_waits_for_a_person() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "alter merge policy hourly set (max_age = '1h')", &[]).unwrap();
    let shown = run(&db, MAIN, "show merge policies", &[]).unwrap().pop().unwrap();
    let col = shown.columns.as_ref().unwrap().iter().position(|(n, _)| n == "max_age").unwrap();
    assert_eq!(shown.rows[0][col].to_text(), "01:00:00");
    assert_eq!(db.policy("hourly").unwrap().max_age, Some(3_600_000));
    let mut s = bot(&db);
    // young: merges on its own
    assert_eq!(attempt(&db, &mut s, "young", "update leads set who = 'y' where id = 1"), None);
    // the same world with a tighter limit, once it's older than that
    run(&db, MAIN, "alter merge policy hourly set (max_age = 1)", &[]).unwrap(); // 1 ms
    s.run(&db, "create world old; use world old; update leads set who = 'o' where id = 2; use world main", &[])
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let e = s.run(&db, "merge world old", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("was forked 0 s ago (at most 0 s)"), "{}", e.message);
    assert_eq!(db.reviews().len(), 1);
    // null lifts it; bad values are refused
    run(&db, MAIN, "alter merge policy hourly set (max_age = null)", &[]).unwrap();
    s.run(&db, "merge world old", &[]).unwrap();
    assert!(run(&db, MAIN, "alter merge policy hourly set (max_age = 'soon')", &[]).is_err());
    assert!(run(&db, MAIN, "alter merge policy hourly set (max_age = 0)", &[]).is_err());
}
