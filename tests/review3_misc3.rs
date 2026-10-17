//! Third review, odds and ends: CHRONOS_LOGIN_TIMEOUT=0 and timeouts too big to add to a clock,
//! and a session's transaction world, which no one else may open (not even as a database name).

mod common;

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};
use postgres::{Client, NoTls};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

struct Server {
    child: Child,
    dir: PathBuf,
    http: u16,
    pg: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A tokenless server on free ports, with `env` set.
fn start(name: &str, env: &[(&str, &str)]) -> Server {
    let dir = std::env::temp_dir().join(format!("chronos-review3-misc3-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]).envs(env.iter().copied());
    let s = common::serve(&mut cmd, true);
    Server { child: s.child, dir, http: s.http, pg: s.pg }
}

fn pg(port: u16, db: &str) -> Result<Client, postgres::Error> {
    Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname={db} connect_timeout=10"), NoTls)
}

fn health(port: u16) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut b = String::new();
    let _ = s.read_to_string(&mut b);
    b
}

fn select1(c: &mut Client) -> i32 {
    c.query_one("select 1", &[]).unwrap().get(0)
}

#[test]
fn login_timeout_zero_means_none() {
    let s = start("zero", &[("CHRONOS_LOGIN_TIMEOUT", "0")]);
    let mut c = pg(s.pg, "main").expect("with no login deadline a client logs in");
    assert_eq!(select1(&mut c), 1);
}

#[test]
fn timeouts_too_big_for_the_clock_mean_none() {
    let huge = u64::MAX.to_string();
    let s = start("huge", &[("CHRONOS_LOGIN_TIMEOUT", &huge), ("CHRONOS_REQUEST_TIMEOUT", &huge)]);
    let mut c = pg(s.pg, "main").expect("a huge login timeout logs in");
    assert_eq!(select1(&mut c), 1);
    assert!(health(s.http).starts_with("HTTP/1.1 200"), "an HTTP request is answered");
    assert!(health(s.http).starts_with("HTTP/1.1 200"), "and the server serves on");
    let mut c = pg(s.pg, "main").unwrap();
    assert_eq!(select1(&mut c), 1);
}

/// A transaction world's name, found in the server's files (no API lists it).
fn tx_name(dir: &std::path::Path) -> Option<String> {
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
                continue;
            }
            let b = std::fs::read(&p).unwrap_or_default();
            if let Some(i) = b.windows(4).position(|w| w == b"_tx_") {
                let hex = b[i + 4..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
                if hex >= 40 {
                    return Some(String::from_utf8_lossy(&b[i..i + 44]).into_owned());
                }
            }
        }
    }
    None
}

#[test]
fn postgres_database_name_cannot_open_a_transaction() {
    let s = start("tx", &[]);
    let mut a = pg(s.pg, "main").unwrap();
    a.batch_execute("create table t (x int); begin; insert into t values (1)").unwrap();
    // a commit elsewhere flushes the log, the transaction's fork with it
    pg(s.pg, "main").unwrap().batch_execute("create table u (x int)").unwrap();
    let mut name = None;
    for _ in 0..50 {
        name = tx_name(&s.dir);
        if name.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let name = name.expect("the transaction's world is in the log");
    let e = pg(s.pg, &name).err().expect("a transaction's world is not a database");
    assert_eq!(e.code().map(|c| c.code()), Some("3D000"), "{e}");
    // its owner still uses it
    a.batch_execute("insert into t values (2); commit").unwrap();
    let n: i64 = a.query_one("select count(*) from t", &[]).unwrap().get(0);
    assert_eq!(n, 2);
}

#[test]
fn view_refuses_transaction_worlds() {
    let db = Db::new();
    run(&db, MAIN, "create table t (x int)", &[]).unwrap();
    let mut s = Session::new(MAIN);
    s.run(&db, "begin; insert into t values (1)", &[]).unwrap();
    let (tx, _) = db.branches().into_iter().find(|(b, _)| b.starts_with("_tx_")).expect("a transaction world");
    assert!(db.view(&tx).is_err(), "view by name");
    assert!(db.view(&format!("{tx}@-1 minutes")).is_err(), "a past view by name");
    assert!(db.view_at(&tx, u64::MAX / 2).is_err(), "view_at by name");
    // the session works on in it
    s.run(&db, "insert into t values (2); commit", &[]).unwrap();
    let out = run(&db, MAIN, "select count(*) from t", &[]).unwrap();
    assert_eq!(out.last().unwrap().rows[0][0].to_text(), "2");
}
