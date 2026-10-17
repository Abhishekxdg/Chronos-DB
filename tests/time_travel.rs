//! Time travel: every world can be read as it was at any moment in the retention window, across
//! checkpoints and restarts; restored; forked from the past; and diffed between two moments.

use chronos::{Db, MAIN, Resolve};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// The moment just after everything so far, before anything next.
fn mark() -> u64 {
    std::thread::sleep(Duration::from_millis(3));
    let t = now();
    std::thread::sleep(Duration::from_millis(3));
    t
}

fn get(db: &Db, b: &str, k: &str) -> Option<String> {
    db.get(b, k).unwrap().map(|v| v.to_string())
}

#[test]
fn read_restore_fork_and_diff_the_past() {
    let dir = std::env::temp_dir().join(format!("chronos-time-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    db.put(MAIN, "t/a", "1").unwrap();
    db.put(MAIN, "t/b", "1").unwrap();
    let t1 = mark();
    db.put(MAIN, "t/a", "2").unwrap();
    db.fork(MAIN, "agent").unwrap();
    db.put("agent", "t/c", "agent").unwrap();
    let t2 = mark();
    db.checkpoint().unwrap(); // history now spans a checkpoint
    db.del(MAIN, "t/b").unwrap();
    let t2b = mark(); // read from the first checkpoint's pages, which only history keeps after the next
    db.put(MAIN, "t/a", "2b").unwrap();
    db.put(MAIN, "t/a", "2").unwrap();
    db.checkpoint().unwrap(); // main's tree from the first checkpoint is now only history's
    db.gc().unwrap(); // which cleanup must keep
    assert_eq!(db.merge("agent", Resolve::Fail).unwrap(), 1);
    let t3 = mark();
    db.put(MAIN, "t/a", "3").unwrap();

    let at = |b: &str, t: u64, k: &str| db.view_at(b, t).unwrap().get(k).unwrap().map(|v| v.to_string());
    assert_eq!(
        (at(MAIN, t1, "t/a"), at(MAIN, t1, "t/b"), at(MAIN, t1, "t/c")),
        (Some("1".into()), Some("1".into()), None)
    );
    assert_eq!((at(MAIN, t2, "t/a"), at(MAIN, t2, "t/b")), (Some("2".into()), Some("1".into())));
    assert_eq!((at(MAIN, t2b, "t/a"), at(MAIN, t2b, "t/b")), (Some("2".into()), None));
    assert_eq!(
        (at(MAIN, t3, "t/a"), at(MAIN, t3, "t/b"), at(MAIN, t3, "t/c")),
        (Some("2".into()), None, Some("agent".into()))
    );
    // a world that has since been merged away can still be read as it was
    assert_eq!(at("agent", t2, "t/c"), Some("agent".into()));
    assert!(db.view_at("agent", t1).is_err(), "it didn't exist yet");
    // the name@when form works wherever a world is read
    assert_eq!(
        get(&db, "main@-1 hour", "t/a"),
        None,
        "an hour ago the database was empty: history starts at its creation"
    );

    // what changed since t1: a from 1 to 3, b deleted, c added
    let d = db.diff_between(&format!("{MAIN}@{}", show(t1)), MAIN).unwrap();
    let keys: Vec<_> = d.iter().map(|c| (c.key.as_str(), c.after.as_deref())).collect();
    assert_eq!(keys, [("t/a", Some("3")), ("t/b", None), ("t/c", Some("agent"))]);

    // fork the past: a world holding main as at t1; merging it would undo everything since
    db.fork_at(MAIN, t1, "undo", serde_json::json!({"why": "look"})).unwrap();
    assert_eq!(get(&db, "undo", "t/a"), Some("1".into()));
    assert_eq!(db.diff("undo").unwrap().len(), 3);
    db.discard("undo").unwrap();

    // restore main to t2: an ordinary write, so the state it replaced stays in history
    let t4 = mark();
    assert_eq!(db.restore(MAIN, t2).unwrap(), 3);
    assert_eq!(
        (get(&db, MAIN, "t/a"), get(&db, MAIN, "t/b"), get(&db, MAIN, "t/c")),
        (Some("2".into()), Some("1".into()), None)
    );
    assert_eq!(at(MAIN, t4, "t/a"), Some("3".into()));
    drop(db);

    // history lives on disk: after reopening, the past reads the same
    let db = Db::open(&dir).unwrap();
    let at = |b: &str, t: u64, k: &str| db.view_at(b, t).unwrap().get(k).unwrap().map(|v| v.to_string());
    assert_eq!(
        (at(MAIN, t1, "t/a"), at(MAIN, t3, "t/c"), at(MAIN, t4, "t/a")),
        (Some("1".into()), Some("agent".into()), Some("3".into()))
    );

    // no retention: the next checkpoint drops history
    db.set_retention(0).unwrap();
    db.checkpoint().unwrap();
    let e = db.view_at(MAIN, t1).map(|_| ()).unwrap_err().to_string();
    assert!(e.contains("history starts at"), "{e}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An open world's writes are logged in batches, after records made later (a batch goes in when
/// something needs it: here, the reads of the past), yet each keeps its own time: the past shows
/// it from the moment it was made, history lists it then, and both hold across a restart.
#[test]
fn an_open_worlds_writes_keep_their_own_times() {
    let dir = std::env::temp_dir().join(format!("chronos-time-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    db.fork(MAIN, "w").unwrap();
    db.put("w", "a", "1").unwrap();
    let t1 = mark();
    db.put(MAIN, "m", "1").unwrap(); // logged now: the log's clock passes w's waiting write
    db.put("w", "b", "2").unwrap();
    let t2 = mark();
    let check = |db: &Db| {
        let at = |b: &str, t: u64, k: &str| db.view_at(b, t).unwrap().get(k).unwrap().map(|v| v.to_string());
        assert_eq!((at("w", t1, "a"), at("w", t1, "b"), at(MAIN, t1, "m")), (Some("1".into()), None, None));
        assert_eq!(
            (at("w", t2, "a"), at("w", t2, "b"), at(MAIN, t2, "m")),
            (Some("1".into()), Some("2".into()), Some("1".into()))
        );
        let writes: Vec<u64> =
            db.history("w", 10).unwrap().iter().filter(|e| e.what == "write").map(|e| e.at).collect();
        assert!(writes.len() == 2 && writes[0] > t1 && writes[1] < t1, "newest first, each at its time: {writes:?}");
    };
    check(&db);
    drop(db);
    check(&Db::open(&dir).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn memory_databases_and_bad_times_say_so() {
    let db = Db::new();
    assert!(db.view("main@-1 minute").map(|_| ()).unwrap_err().to_string().contains("on disk"));
    assert!(db.view("main@yesterday-ish").map(|_| ()).unwrap_err().to_string().contains("not a time"));
    assert!(db.fork(MAIN, "a@b").is_err());
}

fn show(ms: u64) -> String {
    // UTC, as the parser reads it
    let secs = ms / 1000;
    let (days, rest) = (secs / 86400, secs % 86400);
    let (y, m, d) = civil(days as i64);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}", rest / 3600, rest / 60 % 60, rest % 60, ms % 1000)
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Time travel in SQL: AS OF on any table (joins of now and then too), a table's schema as it was,
/// SHOW HISTORY, RESTORE, forking and diffing the past, and sessions on a past world.
#[test]
fn time_travel_in_sql() {
    use chronos::sql::{Session, Value};
    let dir = std::env::temp_dir().join(format!("chronos-time-sql-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    let mut q =
        |sql: &str, params: &[Value]| s.run(&db, sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    let rows = |o: &chronos::sql::Output| -> Vec<String> {
        o.rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| if *v == Value::Null { "null".into() } else { v.to_text() })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    };
    q("create table t (id int primary key, v text); insert into t values (1, 'one'), (2, 'two')", &[]);
    q("create table gone (id int primary key)", &[]);
    q("insert into gone values (7)", &[]);
    let then = Value::Text(q("select now()::text", &[]).rows[0][0].to_text());
    mark();
    q("update t set v = 'uno' where id = 1; delete from t where id = 2; insert into t values (3, 'three')", &[]);
    q("alter table t add column extra int default 5", &[]);
    q("begin; update t set v = 'tres' where id = 3; commit", &[]);
    q("drop table gone", &[]);
    db.checkpoint().unwrap();

    let p = std::slice::from_ref(&then);
    assert_eq!(rows(&q("select id, v from t as of $1 order by id", p)), ["1|one", "2|two"]);
    assert_eq!(
        rows(&q("select * from t for system_time as of $1 order by id", p)),
        ["1|one", "2|two"],
        "no `extra` then"
    );
    assert_eq!(rows(&q("select id from gone as of $1", p)), ["7"], "dropped since");
    // now against then, in one query
    assert_eq!(
        rows(&q(
            "select now.id, now.v, old.v from t now left join t as of $1 old on old.id = now.id order by now.id",
            p
        )),
        ["1|uno|one", "3|tres|null"]
    );
    // bring one row back
    q("insert into t (id, v) select id, v from t as of $1 where id = 2", p);
    assert_eq!(rows(&q("select id, v, extra from t order by id", &[])), ["1|uno|5", "2|two|5", "3|tres|5"]);

    let h = q("show history limit 20", &[]);
    let events: Vec<String> = h.rows.iter().map(|r| r[2].to_text()).collect();
    assert!(events.contains(&"write".to_string()) && events.contains(&"commit".to_string()), "{events:?}");
    assert!(h.rows.windows(2).all(|w| w[0][0].to_text() >= w[1][0].to_text()), "newest first");

    // diff and fork the past
    let past = format!("main@{}", then.to_text());
    let d = q(&format!("diff world '{past}' to main"), &[]);
    assert!(d.rows.iter().any(|r| r[0].to_text() == "t" && r[2].to_text() == "alter table"), "the added column shows");
    let changed: Vec<String> = d
        .rows
        .iter()
        .filter(|r| r[0].to_text() == "t" && r[1] != Value::Null)
        .map(|r| format!("{}:{}", r[1].to_text(), r[2].to_text()))
        .collect();
    assert_eq!(changed, ["1:update", "2:update", "3:insert"], "2 came back as a new version (with extra)");
    q(&format!("fork world '{past}' as undo"), &[]);
    let mut u = Session::new("undo");
    let undo = u.run(&db, "select id, v from t order by id", &[]).unwrap().pop().unwrap();
    assert_eq!(rows(&undo), ["1|one", "2|two"]);

    // restore main, then read the past through a session on it: read only
    let r = q("restore world main to $1", p);
    assert!(r.tag.starts_with("RESTORE "), "{}", r.tag);
    assert_eq!(rows(&q("select id, v from t order by id", &[])), ["1|one", "2|two"]);
    assert_eq!(rows(&q("select id from gone", &[])), ["7"], "the dropped table is back too");
    let mut old = Session::new(&past);
    assert_eq!(rows(&old.run(&db, "select count(*) from t", &[]).unwrap().pop().unwrap()), ["2"]);
    assert_eq!(old.run(&db, "insert into t values (9, 'x')", &[]).unwrap_err().code, "25006");

    q("alter system set history_retention = '7 days'", &[]);
    assert_eq!(rows(&q("show history_retention", &[])), ["7 days"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pages only history still uses survive cleanup: a past read needs them.
#[test]
fn cleanup_keeps_what_history_needs() {
    let dir = std::env::temp_dir().join(format!("chronos-time-gc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    for i in 0..2000 {
        db.put(MAIN, &format!("t/{i:04}"), &format!("v1-{i}-{}", "x".repeat(100))).unwrap();
    }
    db.checkpoint().unwrap();
    let t = mark(); // main is exactly the first checkpoint's tree now
    for i in 0..2000 {
        db.put(MAIN, &format!("t/{i:04}"), &format!("v2-{i}")).unwrap();
    }
    db.checkpoint().unwrap();
    db.checkpoint().unwrap();
    let (_, freed) = db.gc().unwrap();
    let past = db.view_at(MAIN, t).unwrap();
    assert!(past.get("t/1234").unwrap().unwrap().starts_with("v1-1234-"), "freed {freed} bytes");
    drop(past);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A new database keeps an hour of history; a folder made before that default (no `retention` file) keeps the 30
/// days it had, so an upgrade drops nothing.
#[test]
fn a_new_database_keeps_an_hour_and_an_old_one_keeps_its_thirty() {
    const HOUR_MS: u64 = 3600 * 1000;
    const DAY_MS: u64 = 24 * HOUR_MS;
    let dir = std::env::temp_dir().join(format!("chronos-time-retention-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.retention(), HOUR_MS);
    db.put(MAIN, "k", "v").unwrap();
    drop(db);
    // reopened, it keeps its setting
    assert_eq!(Db::open(&dir).unwrap().retention(), HOUR_MS);
    // a folder an older release made has no retention file: 30 days
    std::fs::remove_file(dir.join("retention")).unwrap();
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.retention(), 30 * DAY_MS);
    assert!(!dir.join("retention").exists(), "an old folder is not given the new default");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
