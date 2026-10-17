//! Jepsen-style: real process crashes under concurrent clients. Bank transfers run the agent way
//! (fork, read, write both balances + a ledger row, merge; a conflicting merge is discarded) while
//! the server is killed with SIGKILL and restarted. Afterwards:
//!   1. every transfer whose merge the server acknowledged is in the ledger (no lost commits),
//!   2. every balance is exactly what the ledger implies (merges are atomic, nothing phantom),
//!   3. the total never changed.
//!
//! JEPSEN_KILLS=20 for a longer run (default 4). A failing run keeps its database folder.

mod common;

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ACCOUNTS: u64 = 8;
const START: i64 = 100;

/// One HTTP request on a fresh connection; Err if the server is down or the reply is cut off.
fn post(port: u16, op: &str, body: &Value) -> Result<(u16, Value), String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?; // fails if the server died just now
    let body = body.to_string();
    let req = format!(
        "POST /v1/{op} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let (head, json) = raw.split_once("\r\n\r\n").ok_or("cut off")?;
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).ok_or("bad status")?;
    Ok((status, serde_json::from_str(json).map_err(|_| "cut off body")?))
}

struct Server {
    child: Option<Child>,
    dir: std::path::PathBuf,
    port: u16,
}

impl Server {
    fn start(&mut self) {
        if self.port == 0 {
            // first start: on a port the kernel picks, kept for the restarts
            let s = common::serve(
                Command::new(env!("CARGO_BIN_EXE_chronos")).args(["serve", self.dir.to_str().unwrap()]),
                false,
            );
            (self.child, self.port) = (Some(s.child), s.http);
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut child = Command::new(env!("CARGO_BIN_EXE_chronos"))
                .args([
                    "serve",
                    self.dir.to_str().unwrap(),
                    "--listen",
                    &format!("127.0.0.1:{}", self.port),
                    "--pg",
                    "off",
                ])
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            // up once it answers; a port still held from the killed process just means retry
            let mut up = false;
            while Instant::now() < deadline && child.try_wait().unwrap().is_none() {
                if matches!(post(self.port, "branches", &json!({})), Ok((200, _))) {
                    up = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if up {
                self.child = Some(child);
                return;
            }
            let _ = child.kill();
            let _ = child.wait();
            assert!(Instant::now() < deadline, "server didn't come back");
        }
    }

    fn kill9(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().unwrap(); // SIGKILL: no clean shutdown, no final flush
            c.wait().unwrap();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill9();
        if std::thread::panicking() {
            eprintln!("kept the database for inspection: {}", self.dir.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn acct(i: u64) -> String {
    format!("{i}")
}

/// One transfer, the agent way. Some(id) only if the merge was acknowledged.
fn transfer(port: u16, id: &str, from: u64, to: u64, amount: i64) -> Result<bool, String> {
    let b = format!("t-{id}");
    let ok = |r: Result<(u16, Value), String>| match r {
        Ok((200, v)) => Ok(v),
        Ok((s, v)) => Err(format!("{s} {v}")),
        Err(e) => Err(e),
    };
    ok(post(port, "fork", &json!({ "name": b })))?;
    let bal = |i: u64| -> Result<i64, String> {
        let v = ok(post(port, "get", &json!({ "table": "acct", "id": acct(i), "branch": b })))?;
        Ok(v["row"]["balance"].as_i64().unwrap())
    };
    let (fb, tb) = (bal(from)?, bal(to)?);
    let written = ok(post(
        port,
        "batch",
        &json!({ "branch": b, "rows": [
            { "table": "acct", "id": acct(from), "record": { "balance": fb - amount } },
            { "table": "acct", "id": acct(to), "record": { "balance": tb + amount } },
            { "table": "ledger", "id": id, "record": { "from": from, "to": to, "amount": amount } },
        ]}),
    ))?;
    // the version our write returned: if a crash rolled the branch back, the merge is refused
    let version = written["version"].as_u64().unwrap();
    match post(port, "merge", &json!({ "branch": b, "version": version })) {
        Ok((200, _)) => Ok(true),
        Ok((409, _)) => {
            let _ = post(port, "discard", &json!({ "branch": b }));
            Ok(false) // lost the race, or our writes died in a crash: nothing applied
        }
        Ok((s, v)) => Err(format!("{s} {v}")),
        Err(e) => Err(e), // unknown outcome: the server died mid-request
    }
}

#[test]
fn killed_server_keeps_every_acknowledged_transfer() {
    let kills: usize = std::env::var("JEPSEN_KILLS").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    let dir = std::env::temp_dir().join(format!("chronos-jepsen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut server = Server { child: None, dir, port: 0 };
    server.start();
    let port = server.port;
    let rows: Vec<Value> =
        (0..ACCOUNTS).map(|i| json!({ "table": "acct", "id": acct(i), "record": { "balance": START } })).collect();
    assert_eq!(post(port, "batch", &json!({ "rows": rows })).unwrap().0, 200);

    let acked = Arc::new(Mutex::new(vec![]));
    let stop = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..6)
        .map(|w| {
            let (acked, stop) = (acked.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut x: u64 = 0x9E37_79B9 + w;
                for n in 0.. {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let (from, to) = (x % ACCOUNTS, (x / 7 + 1 + x % (ACCOUNTS - 1)) % ACCOUNTS);
                    if from == to {
                        continue;
                    }
                    let id = format!("{w}-{n}");
                    match transfer(port, &id, from, to, (x % 20) as i64 + 1) {
                        Ok(true) => acked.lock().unwrap().push(id),
                        Ok(false) => {}
                        Err(_) => std::thread::sleep(Duration::from_millis(30)), // down: try again later
                    }
                }
            })
        })
        .collect();

    for k in 0..kills {
        std::thread::sleep(Duration::from_millis(250 + 150 * (k as u64 % 3)));
        server.kill9();
        server.start();
    }
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    for w in workers {
        w.join().unwrap();
    }
    server.kill9(); // one last crash before checking
    server.start();

    let list = |table: &str| -> Vec<Value> {
        let (s, v) = post(port, "find", &json!({ "table": table, "limit": 1_000_000 })).unwrap();
        assert_eq!(s, 200, "{v}");
        v["hits"].as_array().unwrap().clone()
    };
    let ledger: HashMap<String, Value> =
        list("ledger").into_iter().map(|h| (h["id"].as_str().unwrap().to_string(), h["row"].clone())).collect();
    let acked = acked.lock().unwrap();
    assert!(acked.len() > 20, "too few transfers to mean anything: {}", acked.len());
    let lost: Vec<&String> = acked.iter().filter(|id| !ledger.contains_key(*id)).collect();
    assert!(lost.is_empty(), "acknowledged transfers lost after kill -9: {lost:?}");

    let mut expect: HashMap<u64, i64> = (0..ACCOUNTS).map(|i| (i, START)).collect();
    for t in ledger.values() {
        let (f, to, amt) = (t["from"].as_u64().unwrap(), t["to"].as_u64().unwrap(), t["amount"].as_i64().unwrap());
        *expect.get_mut(&f).unwrap() -= amt;
        *expect.get_mut(&to).unwrap() += amt;
    }
    let balances: HashMap<u64, i64> = list("acct")
        .into_iter()
        .map(|h| (h["id"].as_str().unwrap().parse().unwrap(), h["row"]["balance"].as_i64().unwrap()))
        .collect();
    assert_eq!(balances, expect, "balances don't match the ledger: a merge was applied partially");
    assert_eq!(balances.values().sum::<i64>(), START * ACCOUNTS as i64);
    eprintln!(
        "{} acknowledged transfers across {kills} kill -9s, {} in the ledger: all accounted for",
        acked.len(),
        ledger.len()
    );
}
