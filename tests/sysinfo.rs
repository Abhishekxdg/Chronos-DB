//! What a server tells a client about itself and its connection: who it is logged in as, its
//! backend id, whether it can stop another connection. (The functions' answers are checked against
//! Postgres by `pgdiff.rs`; this is what needs a real connection.)

mod common;

use postgres::error::SqlState;
use postgres::{Client, NoTls};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(name: &str, token: Option<&str>) -> (Server, u16) {
    let dir = std::env::temp_dir().join(format!("chronos-sysinfo-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    if let Some(t) = token {
        cmd.args(["--token", t]);
    }
    let s = common::serve(&mut cmd, true);
    (Server(s.child, dir), s.pg)
}

fn connect(port: u16, user: &str) -> Client {
    Client::connect(&format!("host=127.0.0.1 port={port} user={user} dbname=main"), NoTls).unwrap()
}

fn text(c: &mut Client, q: &str) -> String {
    c.query_one(q, &[]).unwrap().get(0)
}

#[test]
fn a_session_is_who_it_logged_in_as() {
    let (_server, port) = start("user", None);
    let (mut ada, mut bo) = (connect(port, "ada"), connect(port, "bo"));
    for q in ["select current_user", "select session_user", "select current_role"] {
        assert_eq!(text(&mut ada, q), "ada", "{q}");
        assert_eq!(text(&mut bo, q), "bo", "{q}");
    }
    // the name a pooled or prepared statement sees is the same
    let by_name = ada.prepare("select current_user, session_user").unwrap();
    let r = ada.query_one(&by_name, &[]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("ada".into(), "ada".into()));
    assert!(text(&mut ada, "select version()").starts_with("PostgreSQL 16."));
    assert_eq!(text(&mut ada, "show server_version"), text(&mut ada, "select current_setting('server_version')"));
}

#[test]
fn every_connection_has_its_own_backend_id() {
    let (_server, port) = start("pid", None);
    let (mut a, mut b) = (connect(port, "a"), connect(port, "b"));
    let (pa, pb): (i32, i32) = (
        a.query_one("select pg_backend_pid()", &[]).unwrap().get(0),
        b.query_one("select pg_backend_pid()", &[]).unwrap().get(0),
    );
    assert!(pa > 0 && pb > 0 && pa != pb, "{pa} {pb}");
    assert_eq!(a.query_one("select pg_backend_pid()", &[]).unwrap().get::<_, i32>(0), pa, "stable for the connection");

    // a transaction is one number; the next is another
    let txid = |c: &mut Client| -> i64 { c.query_one("select txid_current()", &[]).unwrap().get(0) };
    a.batch_execute("begin").unwrap();
    let (x, y) = (txid(&mut a), txid(&mut a));
    a.batch_execute("commit").unwrap();
    a.batch_execute("begin").unwrap();
    let z = txid(&mut a);
    a.batch_execute("commit").unwrap();
    assert_eq!(x, y);
    assert!(z > x, "{x} {z}");
}

#[test]
fn pg_sleep_waits_and_can_be_stopped() {
    let (_server, port) = start("sleep", None);
    let mut c = connect(port, "tester");
    let t = Instant::now();
    c.batch_execute("select pg_sleep(0.15)").unwrap();
    assert!(t.elapsed() >= Duration::from_millis(150), "{:?}", t.elapsed());

    c.batch_execute("set statement_timeout = 200").unwrap();
    let t = Instant::now();
    let e = c.batch_execute("select pg_sleep(30)").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::QUERY_CANCELED), "{e}");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

#[test]
fn pg_cancel_backend_stops_another_connections_statement() {
    let (_server, port) = start("cancel", None);
    let (mut sleeper, mut boss) = (connect(port, "sleeper"), connect(port, "boss"));
    let pid: i32 = sleeper.query_one("select pg_backend_pid()", &[]).unwrap().get(0);
    let t = Instant::now();
    let worker = std::thread::spawn(move || {
        let e = sleeper.batch_execute("select pg_sleep(60)").unwrap_err();
        (e.code().cloned(), sleeper)
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(boss.query_one("select pg_cancel_backend($1)", &[&pid]).unwrap().get::<_, bool>(0));
    let (code, mut sleeper) = worker.join().unwrap();
    assert_eq!(code, Some(SqlState::QUERY_CANCELED));
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    assert_eq!(text(&mut sleeper, "select 'still here'"), "still here", "the connection goes on");

    // no such connection: false, and a warning as Postgres gives
    assert!(!boss.query_one("select pg_cancel_backend(2000000000)", &[]).unwrap().get::<_, bool>(0));
    // a closed connection is forgotten
    drop(sleeper);
    std::thread::sleep(Duration::from_millis(200));
    assert!(!boss.query_one("select pg_cancel_backend($1)", &[&pid]).unwrap().get::<_, bool>(0));
}

#[test]
fn an_agent_is_its_name_and_gets_no_privilege_answers() {
    let (_server, port) = start("agent", Some("s3cret-s3cret-s3cret-s3cret"));
    let mut admin = Client::connect(
        &format!("host=127.0.0.1 port={port} user=admin dbname=main password=s3cret-s3cret-s3cret-s3cret"),
        NoTls,
    )
    .unwrap();
    admin.batch_execute("create table t (id int primary key)").unwrap();
    assert_eq!(text(&mut admin, "select current_user"), "admin");
    assert!(admin.query_one("select has_table_privilege('t', 'insert')", &[]).unwrap().get::<_, bool>(0));

    let token: String = admin.query_one("create agent bot", &[]).unwrap().get("token");
    let mut bot =
        Client::connect(&format!("host=127.0.0.1 port={port} user=bot dbname=main password={token}"), NoTls).unwrap();
    assert_eq!(text(&mut bot, "select current_user"), "bot");
    // a yes would be wrong for the world it can't write, so it's refused
    let e = bot.query_one("select has_table_privilege('t', 'insert')", &[]).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::FEATURE_NOT_SUPPORTED), "{e}");
    // and it can't stop other connections
    let e = bot.query_one("select pg_cancel_backend(1)", &[]).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
}

/// What `pg_stat_activity` shows: a row for each connection with who it is, what it is doing and what
/// it last ran, and `pg_terminate_backend` ends one (monitoring tools and exporters read the first;
/// admins use the second).
#[test]
fn pg_stat_activity_shows_connections_and_pg_terminate_backend_ends_one() {
    let (_server, port) = start("activity", None);
    let mut me =
        Client::connect(&format!("host=127.0.0.1 port={port} user=ada dbname=main application_name=cli"), NoTls)
            .unwrap();
    let mut other = connect(port, "bo");
    other.batch_execute("select 1 as marker").unwrap();
    // my own row: active, running this very query
    let q = "select pid, usename, datname, application_name, state, query, backend_type, xact_start is null, backend_start is not null \
             from pg_stat_activity where pid = pg_backend_pid()";
    let r = me.query_one(q, &[]).unwrap();
    let mine = r.get::<_, i32>(0);
    assert_eq!(
        (r.get::<_, String>(1), r.get::<_, String>(2), r.get::<_, String>(3), r.get::<_, String>(4)),
        ("ada".into(), "main".into(), "cli".into(), "active".into())
    );
    assert_eq!(r.get::<_, String>(5), q);
    assert_eq!(
        (r.get::<_, String>(6), r.get::<_, bool>(7), r.get::<_, bool>(8)),
        ("client backend".into(), false, true) // (a running statement is in a transaction: xact_start is set)
    );
    // the other one: idle, and what it ran last
    let r = me.query_one("select state, query, usename from pg_stat_activity where usename = 'bo'", &[]).unwrap();
    assert_eq!(
        (r.get::<_, String>(0), r.get::<_, String>(1), r.get::<_, String>(2)),
        ("idle".into(), "select 1 as marker".into(), "bo".into())
    );
    // in a transaction it says so, and when it began
    other.batch_execute("begin").unwrap();
    let r =
        me.query_one("select state, xact_start is not null from pg_stat_activity where usename = 'bo'", &[]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, bool>(1)), ("idle in transaction".into(), true));
    other.batch_execute("rollback").unwrap();
    // a database's connection count
    let n: i32 = me.query_one("select numbackends from pg_stat_database where datname = 'main'", &[]).unwrap().get(0);
    assert_eq!(n, 2);
    // terminating another connection: true, and it is gone; a pid nobody has is false with a warning
    let bo: i32 = me.query_one("select pid from pg_stat_activity where usename = 'bo'", &[]).unwrap().get(0);
    assert!(me.query_one("select pg_terminate_backend($1)", &[&bo]).unwrap().get::<_, bool>(0));
    assert!(other.batch_execute("select 1").is_err(), "the terminated connection is closed");
    // its row goes when its connection's thread ends, which can be a moment after the client sees the close
    // (Postgres doesn't wait either, unless pg_terminate_backend is given a timeout)
    let started = std::time::Instant::now();
    let gone = loop {
        let n: i64 = me.query_one("select count(*) from pg_stat_activity where usename = 'bo'", &[]).unwrap().get(0);
        if n == 0 || started.elapsed() > std::time::Duration::from_secs(5) {
            break n;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(gone, 0);
    assert!(!me.query_one("select pg_terminate_backend(2147483000)", &[]).unwrap().get::<_, bool>(0));
    assert_ne!(mine, bo);
}

/// pg_locks names the connection holding or waiting for each advisory lock, as pg_stat_activity
/// numbers it, so monitoring can join the two to see who blocks whom.
#[test]
fn pg_locks_shows_every_holders_and_waiters_pid() {
    let (_server, port) = start("locks", None);
    let (mut holder, mut waiter, mut me) = (connect(port, "holder"), connect(port, "waiter"), connect(port, "me"));
    holder.batch_execute("select pg_advisory_lock(4242)").unwrap();
    let blocked = std::thread::spawn(move || {
        waiter.batch_execute("select pg_advisory_lock(4242)").unwrap();
        waiter
    });
    let q = "select a.usename, l.granted from pg_locks l join pg_stat_activity a on a.pid = l.pid \
             where l.locktype = 'advisory' and l.objid = 4242 order by l.granted desc";
    let t = Instant::now();
    let seen = loop {
        let seen: Vec<(String, bool)> = me.query(q, &[]).unwrap().iter().map(|r| (r.get(0), r.get(1))).collect();
        if seen.len() == 2 || t.elapsed() > Duration::from_secs(20) {
            break seen;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(seen, [("holder".to_string(), true), ("waiter".to_string(), false)]);
    let none: i64 = me.query_one("select count(*) from pg_locks where pid is null", &[]).unwrap().get(0);
    assert_eq!(none, 0);
    holder.batch_execute("select pg_advisory_unlock(4242)").unwrap();
    let mut waiter = blocked.join().unwrap();
    let pid: i32 = waiter.query_one("select pg_backend_pid()", &[]).unwrap().get(0);
    let r = me.query_one("select pid, granted from pg_locks where objid = 4242", &[]).unwrap();
    assert_eq!((r.get::<_, i32>(0), r.get::<_, bool>(1)), (pid, true));
}
