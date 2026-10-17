//! `chronos serve`'s limits, set low through the environment so a test can reach them: the cap on
//! connections (HTTP 503, Postgres 53300), the Postgres login timeout and the HTTP request deadline.
//! (A panic ending only its own connection is tested in src/http.rs, where one can be made.)

mod common;

use postgres::error::SqlState;
use postgres::{Client, NoTls};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

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

/// A server on free ports (HTTP and Postgres), with `env` set.
fn start(name: &str, env: &[(&str, &str)]) -> Server {
    let dir = std::env::temp_dir().join(format!("chronos-review-ops2-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.join("db").to_str().unwrap()]).envs(env.iter().copied());
    let s = common::serve(&mut cmd, true);
    Server { child: s.child, dir, http: s.http, pg: s.pg }
}

/// Retries `f` while it's refused (a closed connection gives its place back a moment later).
fn admitted<T>(mut f: impl FnMut() -> Result<T, String>) -> T {
    let start = Instant::now();
    loop {
        match f() {
            Ok(t) => return t,
            Err(e) => assert!(start.elapsed() < Duration::from_secs(10), "never admitted: {e}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An HTTP connection, kept open after one request; the start of the reply.
fn http(port: u16) -> (TcpStream, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut b = [0u8; 256];
    let n = s.read(&mut b).unwrap();
    let reply = String::from_utf8_lossy(&b[..n]).into_owned();
    (s, reply)
}

fn pg(port: u16) -> Result<Client, postgres::Error> {
    Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main"), NoTls)
}

/// With CHRONOS_MAX_CONNECTIONS=2, a third connection is refused, counted across both protocols
/// (the cap is what the process's descriptors hold), and a place freed is taken again.
#[test]
fn connections_are_capped() {
    let s = start("cap", &[("CHRONOS_MAX_CONNECTIONS", "2")]);
    let held: Vec<_> = (0..2)
        .map(|_| {
            admitted(|| Some(http(s.http)).filter(|(_, r)| r.starts_with("HTTP/1.1 200")).ok_or("503".to_string()))
        })
        .collect();
    let (_, r) = http(s.http);
    assert!(r.starts_with("HTTP/1.1 503"), "a third HTTP connection is refused: {r}");
    // the two HTTP connections use the Postgres listener's places too
    let Err(e) = pg(s.pg) else { panic!("a Postgres client past the shared cap is refused") };
    assert_eq!(e.code(), Some(&SqlState::TOO_MANY_CONNECTIONS), "{e}");
    drop(held);
    let mut clients: Vec<_> = (0..2).map(|_| admitted(|| pg(s.pg).map_err(|e| e.to_string()))).collect();
    let Err(e) = pg(s.pg) else { panic!("a third Postgres client is refused") };
    assert_eq!(e.code(), Some(&SqlState::TOO_MANY_CONNECTIONS), "{e}");
    let (_, r) = http(s.http);
    assert!(r.starts_with("HTTP/1.1 503"), "an HTTP connection past the shared cap is refused: {r}");
    // those admitted still work, and a closed one makes room
    assert_eq!(clients[0].query_one("select 1", &[]).unwrap().get::<_, i32>(0), 1);
    drop(clients.pop());
    admitted(|| pg(s.pg).map_err(|e| e.to_string()));
    drop(clients);
    admitted(|| Some(http(s.http)).filter(|(_, r)| r.starts_with("HTTP/1.1 200")).ok_or("503".to_string()));
}

/// How long until the server closes `c`, which sends nothing more.
fn dropped_after(mut c: TcpStream) -> Duration {
    let t = Instant::now();
    c.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut b = [0u8; 64];
    match c.read(&mut b) {
        Ok(n) => assert_eq!(n, 0, "closed without a reply"),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset, "closed, not still waiting: {e}"),
    }
    t.elapsed()
}

/// A Postgres client that never logs in, and an HTTP request that stops halfway, are dropped
/// after CHRONOS_LOGIN_TIMEOUT and CHRONOS_REQUEST_TIMEOUT seconds; the server serves on.
#[test]
fn stalled_clients_are_dropped() {
    let s = start("stall", &[("CHRONOS_LOGIN_TIMEOUT", "1"), ("CHRONOS_REQUEST_TIMEOUT", "1")]);
    let took = dropped_after(TcpStream::connect(("127.0.0.1", s.pg)).unwrap());
    assert!(took >= Duration::from_millis(900) && took < Duration::from_secs(5), "login timeout: {took:?}");
    let mut c = TcpStream::connect(("127.0.0.1", s.http)).unwrap();
    c.write_all(b"GET /v1/health HTTP/1.1\r\nHost: loc").unwrap();
    let took = dropped_after(c);
    assert!(took >= Duration::from_millis(900) && took < Duration::from_secs(5), "request deadline: {took:?}");
    // a client that logged in has no login timeout
    let mut ok = pg(s.pg).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(ok.query_one("select 1", &[]).unwrap().get::<_, i32>(0), 1);
    assert!(http(s.http).1.starts_with("HTTP/1.1 200"));
}

/// A Postgres client past the cap is answered as Postgres answers it: its GSSENCRequest and SSLRequest get their
/// one-byte answers (psql sends SSLRequest first by default, and an error in their place reads as "server sent an
/// error response during SSL exchange"), and its startup message gets FATAL 53300.
#[test]
fn a_client_past_the_cap_is_refused_after_its_startup_message() {
    let s = start("cap-ssl", &[("CHRONOS_MAX_CONNECTIONS", "1")]);
    let _held = admitted(|| pg(s.pg).map_err(|e| e.to_string()));
    let mut c = TcpStream::connect(("127.0.0.1", s.pg)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    for code in [80877104i32, 80877103] {
        c.write_all(&[8i32.to_be_bytes(), code.to_be_bytes()].concat()).unwrap();
        let mut b = [0u8; 1];
        c.read_exact(&mut b).unwrap();
        assert_eq!(b[0], b'N', "answered {code} with {:?}", b[0] as char);
    }
    let mut startup = 196608i32.to_be_bytes().to_vec();
    startup.extend(b"user\0tester\0database\0main\0\0");
    c.write_all(&(startup.len() as i32 + 4).to_be_bytes()).unwrap();
    c.write_all(&startup).unwrap();
    let mut reply = vec![];
    c.read_to_end(&mut reply).unwrap();
    assert_eq!(reply.first(), Some(&b'E'), "{reply:?}");
    let fields: Vec<String> =
        reply[5..].split(|b| *b == 0).filter(|f| !f.is_empty()).map(|f| String::from_utf8_lossy(f).into()).collect();
    for want in ["SFATAL", "C53300", "Msorry, too many clients already"] {
        assert!(fields.iter().any(|f| f == want), "{want} in {fields:?}");
    }
    // and through a driver: the same refusal
    let Err(e) = pg(s.pg) else { panic!("refused") };
    assert_eq!(e.code(), Some(&SqlState::TOO_MANY_CONNECTIONS), "{e}");
}
