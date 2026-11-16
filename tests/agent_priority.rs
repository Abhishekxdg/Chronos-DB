//! An agent's priority. Alone in this file, so alone in its process: whether "a statement of the database's own
//! users is running" is one count for the whole process (`BUSY`), and beside other tests their statements were
//! that. With tests/limits.rs's others running at once, the low-priority agent yielded to them where this test
//! says nobody else is there, and it failed every time in a release build.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};
use std::time::Duration;

/// A table `t` of `n` rows (id, k = id % `groups`, v text).
fn table(db: &Db, t: &str, n: usize, groups: usize) {
    run(db, MAIN, &format!("create table {t} (id int primary key, k int, v text)"), &[]).unwrap();
    for chunk in (0..n).collect::<Vec<_>>().chunks(1000) {
        let rows: Vec<String> = chunk.iter().map(|i| format!("({i}, {}, 'value {i}')", i % groups)).collect();
        run(db, MAIN, &format!("insert into {t} values {}", rows.join(", ")), &[]).unwrap();
    }
}

fn session(agent: &str) -> Session {
    let mut s = Session::new(MAIN);
    s.agent = Some(agent.into());
    s
}

fn code(r: chronos::sql::Result<Vec<chronos::sql::Output>>) -> &'static str {
    r.map(|_| "ok").unwrap_or_else(|e| e.code)
}

/// Pairs of rows that never match: seconds of work, little memory.
const SLOW: &str = "select count(*) from t a, t b where a.v || b.v = 'nope'";

/// An agent's priority (issue #43): `low` makes its statements yield to the database's own users while theirs run
/// (`agent_yield_ms` counts the time), and never otherwise; `normal`, the default, is as before.
#[test]
fn a_low_priority_agent_yields_to_the_databases_own_users() {
    let db = Db::new();
    table(&db, "t", 1000, 10);
    run(
        &db,
        MAIN,
        "create agent batch with (can = 'read', priority = 'low'); create agent helper with (can = 'read')",
        &[],
    )
    .unwrap();
    let priorities = || {
        let shown = run(&db, MAIN, "show agents", &[]).unwrap();
        let cols: Vec<&str> = shown[0].columns.as_ref().unwrap().iter().map(|(n, _)| n.as_str()).collect();
        let at = cols.iter().position(|c| *c == "priority").unwrap();
        let mut out: Vec<(String, String)> = shown[0].rows.iter().map(|r| (r[0].to_text(), r[at].to_text())).collect();
        out.sort();
        out
    };
    let want = |b: &str, h: &str| vec![("batch".to_string(), b.to_string()), ("helper".to_string(), h.to_string())];
    assert_eq!(priorities(), want("low", "normal"));
    run(&db, MAIN, "alter agent helper set (priority = 'LOW')", &[]).unwrap();
    assert_eq!(priorities(), want("low", "low"));
    run(&db, MAIN, "alter agent helper set (priority = 'normal')", &[]).unwrap();
    assert_eq!(priorities(), want("low", "normal"));
    let e = run(&db, MAIN, "alter agent helper set (priority = 'urgent')", &[]).unwrap_err();
    assert!(e.message.contains("priority must be 'normal' or 'low'"), "{}", e.message);

    let yielded = || db.counters().into_iter().find(|(n, _)| *n == "agent_yield_ms").map(|(_, v)| v).unwrap();
    // nobody else's statement running: it doesn't yield
    assert_eq!(code(session("batch").run(&db, SLOW, &[])), "ok");
    assert_eq!(yielded(), 0);
    // a person's statements running, one after another
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|sc| {
        let person = sc.spawn(|| {
            let mut s = Session::new(MAIN);
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                s.run(&db, SLOW, &[]).unwrap();
            }
        });
        std::thread::sleep(Duration::from_millis(100));
        // a normal agent's statement doesn't yield to them
        assert_eq!(code(session("helper").run(&db, SLOW, &[])), "ok");
        assert_eq!(yielded(), 0);
        // a low-priority one's does
        for _ in 0..20 {
            assert_eq!(code(session("batch").run(&db, SLOW, &[])), "ok");
            if yielded() > 0 {
                break;
            }
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        person.join().unwrap();
    });
    assert!(yielded() > 0, "it yielded while a person's statements ran");
    // and its result is the same
    let n = session("batch").run(&db, "select count(*) from t", &[]).unwrap();
    assert_eq!(n[0].rows[0][0].to_text(), "1000");
}
