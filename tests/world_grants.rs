//! World-scoped grants: GRANT {READ | WRITE | MERGE | FORK}[, ...] ON WORLD w TO agent [UNTIL '<timestamp>'] lets
//! an agent do that in world w and the worlds forked from it, and nowhere else; never in main. A grantor grants only
//! what it holds there; a grant past its UNTIL counts for nothing; REVOKE takes rights back.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create table t (id int primary key, v text); insert into t values (1, 'a'), (2, 'b');
         create agent owner with (can = 'read,fork,write_own,merge_own');
         create agent helper with (can = 'read');
         create agent third with (can = 'read')",
        &[],
    )
    .unwrap();
}

fn as_agent(name: &str, world: &str) -> Session {
    let mut s = Session::new(world);
    s.agent = Some(name.into());
    s
}

/// The SQLSTATE `q` fails with, run as agent `who` in world `w`.
fn refused(db: &Db, who: &str, w: &str, q: &str) -> &'static str {
    match as_agent(who, w).run(db, q, &[]) {
        Ok(_) => panic!("{q} as {who} in {w}: went through"),
        Err(e) => e.code,
    }
}

fn ok(db: &Db, who: &str, w: &str, q: &str) {
    if let Err(e) = as_agent(who, w).run(db, q, &[]) {
        panic!("{q} as {who} in {w}: {} {}", e.code, e.message);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

/// `ms` since 1970 as a timestamp UNTIL takes.
fn stamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // civil from days (Howard Hinnant's algorithm)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}+00", rem / 3600, rem % 3600 / 60, rem % 60, ms % 1000)
}

#[test]
fn grant_on_world_allows_only_that_world() {
    let db = Db::new();
    setup(&db);
    ok(&db, "owner", MAIN, "create world w; create world w2");
    assert_eq!(refused(&db, "helper", "w", "insert into t values (3, 'h')"), "42501");
    ok(&db, "owner", MAIN, "grant write on world w to helper");
    ok(&db, "helper", "w", "insert into t values (3, 'h')");
    assert_eq!(refused(&db, "helper", "w2", "insert into t values (3, 'h')"), "42501", "only that world");
    assert_eq!(refused(&db, "helper", MAIN, "insert into t values (3, 'h')"), "42501", "never main");
    // and the worlds forked from it
    ok(&db, "owner", MAIN, "create world wc from w");
    ok(&db, "helper", "wc", "insert into t values (4, 'h')");
    // merging needs MERGE: a fork of w into w (which it may write), never w into main
    assert_eq!(refused(&db, "helper", MAIN, "merge world wc"), "42501");
    ok(&db, "owner", MAIN, "grant merge, fork on world w to helper");
    ok(&db, "helper", MAIN, "merge world wc");
    assert_eq!(refused(&db, "helper", MAIN, "merge world w"), "42501", "main needs write_main");
    ok(&db, "helper", MAIN, "create world mine from w"); // FORK on w: a world of its own, inside w
    assert_eq!(db.world("mine").unwrap().owner.as_deref(), Some("helper"));
    assert_eq!(refused(&db, "helper", MAIN, "create world nope from w2"), "42501");
    // nothing on main
    assert_eq!(refused(&db, "owner", MAIN, "grant read on world main to helper"), "42501");
    let e = run(&db, MAIN, "grant write on world main to helper", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);

    // REVOKE takes it back
    ok(&db, "owner", MAIN, "revoke write on world w from helper");
    assert_eq!(refused(&db, "helper", "w", "insert into t values (5, 'h')"), "42501");
    let audit: Vec<String> = db.security_events(20).into_iter().map(|e| format!("{} {}", e.action, e.target)).collect();
    assert!(audit.iter().any(|a| a.starts_with("grant on world") && a.contains("helper")), "{audit:?}");
    assert!(audit.iter().any(|a| a.starts_with("revoke on world")), "{audit:?}");
    // grants are kept with the world
    let meta = db.world("w").unwrap().meta;
    assert!(meta["_sonos_grants"]["helper"]["merge"].is_object(), "{meta}");
    assert!(meta["_sonos_grants"]["helper"].get("write").is_none(), "{meta}");
}

#[test]
fn grant_until_expires() {
    let db = Db::new();
    setup(&db);
    ok(&db, "owner", MAIN, "create world w");
    let until = stamp(now_ms() + 600);
    ok(&db, "owner", MAIN, &format!("grant write on world w to helper until '{until}'"));
    ok(&db, "helper", "w", "insert into t values (3, 'h')");
    std::thread::sleep(std::time::Duration::from_millis(800));
    assert_eq!(refused(&db, "helper", "w", "insert into t values (4, 'h')"), "42501", "past its UNTIL");
    // one already past is refused
    let past = stamp(now_ms() - 60_000);
    let e = as_agent("owner", MAIN).run(&db, &format!("grant write on world w to helper until '{past}'"), &[]);
    assert_eq!(e.unwrap_err().code, "22023");
}

#[test]
fn grantor_cannot_grant_more_than_held() {
    let db = Db::new();
    setup(&db);
    ok(&db, "owner", MAIN, "create world w; create world w2");
    // owner holds write, merge and fork on its own world; never another's
    run(&db, MAIN, "create agent stranger with (can = 'read,fork,write_own,merge_own')", &[]).unwrap();
    assert_eq!(refused(&db, "stranger", MAIN, "grant write on world w to helper"), "42501");
    // helper may hand on what it holds (until no later than its own), not more
    let until = stamp(now_ms() + 3_600_000);
    ok(&db, "owner", MAIN, &format!("grant write on world w to helper until '{until}'"));
    assert_eq!(refused(&db, "helper", MAIN, "grant merge on world w to third"), "42501");
    assert_eq!(refused(&db, "helper", MAIN, "grant write on world w2 to third"), "42501");
    ok(&db, "helper", MAIN, "grant write on world w to third");
    ok(&db, "third", "w", "insert into t values (3, 'third')");
    let g = &db.world("w").unwrap().meta["_sonos_grants"]["third"]["write"];
    assert!(g["until"].as_u64().is_some(), "capped at the grantor's own grant: {g}");
    assert_eq!(g["by"], "helper");
    // an agent unknown, or a right unknown
    assert_eq!(refused(&db, "owner", MAIN, "grant write on world w to nobody"), "42704");
    assert!(as_agent("owner", MAIN).run(&db, "grant drive on world w to helper", &[]).is_err());
}
