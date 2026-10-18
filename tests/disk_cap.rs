//! Limits the operator sets where the process starts: the folder's size cap
//! (CHRONOS_MAX_DISK_BYTES, `Open::max_disk_bytes`) and ceilings over the database's own settings
//! (CHRONOS_LIMITS, `Open::ceilings`). A full database refuses writes, forks and spills with 53100
//! (HTTP 507) and can still free itself; ceilings refuse ALTER SYSTEM past them (42501) and clamp a
//! session's statement_timeout.

mod common;

use chronos::limits::Ceilings;
use chronos::sql::{Output, run};
use chronos::{Db, Error, MAIN, Open, Setting};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const CAP: u64 = 2 << 20;
/// What a write let in just under the cap may take past it: its log framing and clock records.
const SLACK: u64 = 64 << 10;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-diskcap-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn capped(d: &Path, cap: u64) -> Db {
    Db::open_with(d, &Open { max_disk_bytes: Some(cap), ..Default::default() }).unwrap()
}

fn sql(db: &Db, q: &str) -> chronos::sql::Result<Vec<Output>> {
    run(db, MAIN, q, &[])
}

fn one(db: &Db, q: &str) -> String {
    sql(db, q).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message))[0].rows[0][0].to_text()
}

/// Text that doesn't compress: a page holds about as much as the row.
fn noise(seed: u64, len: usize) -> String {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            char::from(b'a' + (x % 26) as u8)
        })
        .collect()
}

/// `n` rows of about 1 kB, ids from `from`.
fn insert(db: &Db, from: u64, n: u64) -> chronos::sql::Result<Vec<Output>> {
    let rows: Vec<String> = (from..from + n).map(|i| format!("({i}, '{}')", noise(i, 1000))).collect();
    sql(db, &format!("insert into t values {}", rows.join(", ")))
}

fn disk(db: &Db, what: &str) -> u64 {
    let out = sql(db, "show disk").unwrap();
    out[0].rows.iter().find(|r| r[0].to_text() == what).unwrap()[1].to_text().parse().unwrap()
}

fn is_full(e: &Error) -> bool {
    matches!(e, Error::Constraint { code: "53100", .. })
}

#[test]
fn rows_fill_to_the_cap_and_deletes_free_it() {
    let d = dir("rows");
    let db = capped(&d, CAP);
    sql(&db, "alter system set history_retention = '0'").unwrap();
    sql(&db, "create table t (id bigint primary key, v text)").unwrap();
    let mut n = 0;
    let e = loop {
        match insert(&db, n, 50) {
            Ok(_) => n += 50,
            Err(e) => break e,
        }
        assert!(n < 100_000, "never refused");
    };
    assert_eq!(e.code, "53100", "{}", e.message);
    assert!(e.message.contains("database is over its 2 MB limit"), "{}", e.message);
    assert!(n >= 1000, "refused early, at {n} rows");
    let total = disk(&db, "total");
    assert!(total <= CAP + SLACK && total > CAP / 2, "{total}");
    assert_eq!(disk(&db, "limit"), CAP);
    // the JSON row API too, and an UPDATE
    let row = format!(r#"{{"v": "{}"}}"#, noise(1, 100_000));
    assert!(is_full(&db.batch(MAIN, &[("j/1", Some(row.as_str()))]).unwrap_err()));
    assert_eq!(sql(&db, "update t set v = v || 'x'").unwrap_err().code, "53100");
    // reads, transactions and deletes go on
    assert_eq!(one(&db, "select count(*) from t"), n.to_string());
    sql(&db, "begin").unwrap();
    sql(&db, "rollback").unwrap();
    sql(&db, &format!("delete from t where id >= {}", n / 4)).unwrap();
    // a checkpoint folds the log away (no history kept), cleanup frees the pages
    db.checkpoint().unwrap();
    db.gc().unwrap();
    let total = disk(&db, "total");
    assert!(total < CAP / 2, "{total}");
    insert(&db, n, 50).unwrap();
    assert_eq!(one(&db, "select count(*) from t"), (n / 4 + 50).to_string());
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn worlds_fill_to_the_cap_and_discarding_frees_it() {
    let d = dir("worlds");
    let db = capped(&d, CAP);
    sql(&db, "alter system set history_retention = '0'").unwrap();
    // each world's writes past a batch's 64 kB, so they're logged at once rather than held
    let mut made = vec![];
    let e = loop {
        let w = format!("w{}", made.len());
        if let Err(e) = db.fork(MAIN, &w) {
            break e;
        }
        made.push(w.clone());
        let rows: Vec<(String, String)> = (0..80)
            .map(|i| (format!("t/{i}"), format!(r#"{{"v": "{}"}}"#, noise(made.len() as u64 * 100 + i, 1000))))
            .collect();
        let rows: Vec<(&str, Option<&str>)> = rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect();
        if let Err(e) = db.batch(&w, &rows) {
            break e;
        }
        assert!(made.len() < 10_000, "never refused");
    };
    assert!(is_full(&e), "{e}");
    assert!(made.len() >= 10, "refused early, at {} worlds", made.len());
    for w in &made {
        db.discard(w).unwrap();
    }
    db.checkpoint().unwrap();
    db.gc().unwrap();
    assert!(db.disk_bytes() < CAP / 2, "{}", db.disk_bytes());
    db.fork(MAIN, "again").unwrap();
    db.batch("again", &[("t/1", Some(r#"{"v": 1}"#))]).unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn history_fills_to_the_cap_and_a_shorter_retention_frees_it() {
    let d = dir("history");
    let cap = 4 << 20;
    let db = capped(&d, cap);
    sql(&db, "alter system set history_retention = '1 day'").unwrap();
    sql(&db, "create table t (id bigint primary key, v text)").unwrap();
    insert(&db, 0, 100).unwrap();
    let mut round = 0u64;
    let e = loop {
        round += 1;
        // every row anew: the old versions stay, for time travel
        let q = format!("update t set v = md5(v || '{round}') || substr(v, 33)");
        if let Err(e) = sql(&db, &q) {
            break e;
        }
        db.checkpoint().unwrap();
        assert!(round < 1000, "never refused");
    };
    assert_eq!(e.code, "53100", "{}", e.message);
    assert!(round > 3, "refused early, at round {round}");
    assert!(disk(&db, "history") > 0);
    // the past is still readable, deletes are let in
    assert_eq!(one(&db, "select count(*) from t"), "100");
    sql(&db, "alter system set history_retention = '0'").unwrap();
    db.checkpoint().unwrap();
    db.gc().unwrap();
    assert!(db.disk_bytes() < cap / 2, "{}", db.disk_bytes());
    sql(&db, "update t set v = 'small'").unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ceilings_refuse_alter_system_and_clamp_statement_timeout() {
    let d = dir("ceilings");
    let ceilings = Ceilings {
        max_worlds: 3,
        history_retention_ms: 60_000,
        world_idle_ttl_ms: 3_600_000,
        statement_timeout_ms: 200,
    };
    let open = |d: &Path| Db::open_with(d, &Open { ceilings: Some(ceilings), ..Default::default() }).unwrap();
    let db = open(&d);
    assert_eq!(db.ceilings(), ceilings);
    // unset (0, no limit) is the ceiling; a new database's hour of history is cut to the ceiling's minute
    assert_eq!(one(&db, "show max_worlds"), "3");
    assert_eq!(db.setting(Setting::WorldIdleTtl), 3_600_000);
    assert_eq!(one(&db, "show history_retention"), "00:01:00");
    for (q, name) in [
        ("alter system set max_worlds = 4", "max_worlds"),
        ("alter system set history_retention = '1 hour'", "history_retention"),
        ("alter system set world_idle_ttl = '1 day'", "world_idle_ttl"),
        ("alter system set statement_timeout = '1s'", "statement_timeout"),
    ] {
        let e = sql(&db, q).unwrap_err();
        assert_eq!(e.code, "42501", "{q}: {}", e.message);
        assert!(e.message.starts_with(&format!("{name} may be at most")), "{}", e.message);
        assert!(e.message.contains("CHRONOS_LIMITS"), "{}", e.message);
    }
    // under them is fine
    sql(&db, "alter system set max_worlds = 2").unwrap();
    assert_eq!(one(&db, "show max_worlds"), "2");
    sql(&db, "alter system set max_worlds = 0").unwrap();
    assert_eq!(one(&db, "show max_worlds"), "3");
    sql(&db, "alter system set history_retention = '30s'").unwrap();
    // the ceiling holds forks
    for w in ["a", "b", "c"] {
        db.fork(MAIN, w).unwrap();
    }
    assert!(matches!(db.fork(MAIN, "d"), Err(Error::Constraint { code: "53400", .. })));
    // a session's SET is clamped: above the ceiling, and 0 (none)
    let mut s = chronos::sql::Session::new(MAIN);
    let shown =
        |s: &mut chronos::sql::Session| s.run(&db, "show statement_timeout", &[]).unwrap()[0].rows[0][0].to_text();
    assert_eq!(shown(&mut s), "200ms");
    s.run(&db, "set statement_timeout = '1h'", &[]).unwrap();
    assert_eq!(shown(&mut s), "200ms");
    s.run(&db, "set statement_timeout = 0", &[]).unwrap();
    assert_eq!(shown(&mut s), "200ms");
    s.run(&db, "set statement_timeout = 50", &[]).unwrap();
    assert_eq!(shown(&mut s), "50ms");
    s.run(&db, "set statement_timeout = 0", &[]).unwrap();
    s.run(&db, "create table big (id int primary key, v text)", &[]).unwrap();
    s.run(&db, "insert into big select i, 'value ' || i from generate_series(1, 4000) i", &[]).unwrap();
    let t = Instant::now();
    let e = s.run(&db, "select count(*) from big a, big b where a.v || b.v = 'nope'", &[]).unwrap_err();
    assert_eq!(e.code, "57014", "{}", e.message);
    assert!(t.elapsed() < Duration::from_secs(3), "stopped near 200 ms, took {:?}", t.elapsed());
    drop(db);
    // kept across a restart, and a database opened with no ceilings is free of them
    let db = open(&d);
    assert_eq!(one(&db, "show history_retention"), "00:00:30");
    drop(db);
    let db = Db::open(&d).unwrap();
    assert_eq!(one(&db, "show max_worlds"), "0");
    sql(&db, "alter system set max_worlds = 100").unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ceilings_file_is_read_strictly() {
    assert_eq!(
        Ceilings::parse("max_worlds = 10\nstatement_timeout_ms = 30_000 # half a minute\n").unwrap(),
        Ceilings { max_worlds: 10, statement_timeout_ms: 30_000, ..Default::default() }
    );
    assert!(Ceilings::parse("max_world = 10").is_err());
    assert!(Ceilings::parse("max_worlds = ten").is_err());
}

fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (head.split(' ').nth(1).unwrap().parse().unwrap(), body.to_string())
}

fn post_sql(port: u16, q: &str) -> (u16, Value) {
    let (status, body) = http(port, "POST", "/v1/sql", &json!({ "sql": q }).to_string());
    (status, serde_json::from_str(&body).unwrap_or(Value::String(body)))
}

fn gauge(metrics: &str, name: &str) -> u64 {
    let line = metrics.lines().find(|l| l.starts_with(&format!("{name} "))).unwrap_or_else(|| panic!("no {name}"));
    line.split(' ').nth(1).unwrap().parse().unwrap()
}

/// The server as a host runs it: both limits from the environment, spills inside the folder; full
/// over HTTP (507), its gauges in /metrics, killed at the cap, and whole when opened again.
#[test]
fn a_server_at_its_cap_answers_507_and_survives_kill_9() {
    let d = dir("serve");
    std::fs::create_dir_all(&d).unwrap();
    let limits = d.with_extension("limits.toml");
    std::fs::write(&limits, "max_worlds = 5\nstatement_timeout_ms = 60_000\n").unwrap();
    let exe = env!("CARGO_BIN_EXE_chronos");
    // a bad ceilings file stops the start
    let bad = d.with_extension("bad.toml");
    std::fs::write(&bad, "max_wrlds = 5\n").unwrap();
    let out = Command::new(exe)
        .args(["serve", d.to_str().unwrap(), "--listen", "127.0.0.1:0", "--pg", "off"])
        .env("CHRONOS_LIMITS", &bad)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no limit named max_wrlds"), "{out:?}");

    let cap = 4u64 << 20;
    let mut cmd = Command::new(exe);
    cmd.args(["serve", d.to_str().unwrap()])
        .env("CHRONOS_MAX_DISK_BYTES", cap.to_string())
        .env("CHRONOS_LIMITS", &limits)
        .env("CHRONOS_SPILL_DIR", d.join("spill"))
        .env("CHRONOS_WORK_MEM", "64kB");
    let mut srv = common::serve(&mut cmd, false);
    let port = srv.http;
    let (_, m) = http(port, "GET", "/metrics", "");
    assert_eq!(gauge(&m, "chronos_disk_limit_bytes"), cap);
    assert_eq!(gauge(&m, "chronos_connections_open"), 0, "the one asking isn't counted");
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    assert_eq!(post_sql(port, "alter system set history_retention = '0'").0, 200);
    let (status, e) = post_sql(port, "alter system set max_worlds = 6");
    assert_eq!((status, e["error"]["code"].as_str()), (403, Some("42501")), "{e}");
    assert_eq!(post_sql(port, "create table t (id bigint primary key, v text)").0, 200);
    let rows: Vec<String> = (0..500).map(|i| format!("({i}, '{}')", noise(i, 1000))).collect();
    assert_eq!(post_sql(port, &format!("insert into t values {}", rows.join(", "))).0, 200);
    let mut n = 500;
    loop {
        let rows: Vec<String> = (n..n + 100).map(|i| format!("({i}, '{}')", noise(i, 1000))).collect();
        let (status, e) = post_sql(port, &format!("insert into t values {}", rows.join(", ")));
        if status == 507 {
            assert_eq!(e["error"]["code"], "53100");
            break;
        }
        assert_eq!(status, 200, "{e}");
        n += 100;
        assert!(n < 100_000, "never refused");
    }
    // a query's spill past the cap is refused (its sort spills, being past CHRONOS_WORK_MEM); reads that
    // don't spill go on
    let (status, e) = post_sql(port, "select v || v as w from t order by w");
    let code = e["error"]["code"].as_str().map(String::from);
    assert_eq!((status, code.as_deref()), (507, Some("53100")), "{}", e["error"]);
    assert!(e["error"]["message"].as_str().unwrap().contains("over its 4 MB limit"), "{}", e["error"]);
    assert_eq!(post_sql(port, "select count(*) from t").0, 200);
    let (_, m) = http(port, "GET", "/metrics", "");
    let used = gauge(&m, "chronos_disk_bytes");
    assert!(used > cap / 2 && used <= cap + SLACK, "{used}");
    assert!(gauge(&m, "chronos_last_statement_unix_ms") >= before);
    // killed at the cap: nothing torn
    srv.child.kill().unwrap();
    srv.child.wait().unwrap();
    let db = capped(&d, cap);
    assert_eq!(sql(&db, "verify database").unwrap()[0].tag, "VERIFY");
    assert_eq!(one(&db, "select count(*) from t"), n.to_string());
    assert_eq!(insert(&db, n, 100).unwrap_err().code, "53100");
    sql(&db, "delete from t").unwrap();
    db.checkpoint().unwrap();
    db.gc().unwrap();
    insert(&db, 0, 100).unwrap();
    assert_eq!(sql(&db, "verify database").unwrap()[0].tag, "VERIFY");
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::remove_file(&limits);
    let _ = std::fs::remove_file(&bad);
}
