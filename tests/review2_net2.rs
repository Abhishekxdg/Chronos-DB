//! Second review, network listeners: nested binary arrays, logins and TLS handshakes that never
//! finish, a local shell session that ends badly, and MCP's discard of a world named `cascade`.

mod common;

use postgres::{Client, NoTls};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-review2-net2-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A tokenless server on free ports, with `env` set; with a TLS certificate if `tls`.
fn start(name: &str, env: &[(&str, &str)], tls: bool) -> Server {
    let dir = scratch(name);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.join("db").to_str().unwrap()]).envs(env.iter().copied());
    if tls {
        let ok = Command::new("openssl")
            .args(["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-days", "1"])
            .args(["-subj", "/CN=localhost", "-keyout"])
            .arg(dir.join("key.pem"))
            .arg("-out")
            .arg(dir.join("cert.pem"))
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "openssl could not make a certificate");
        cmd.arg("--tls-cert").arg(dir.join("cert.pem")).arg("--tls-key").arg(dir.join("key.pem"));
    }
    let s = common::serve(&mut cmd, true);
    Server { child: s.child, dir, http: s.http, pg: s.pg }
}

fn msg(tag: u8, body: &[u8]) -> Vec<u8> {
    [&[tag][..], &(body.len() as i32 + 4).to_be_bytes(), body].concat()
}

/// Reads messages until ReadyForQuery; returns the error codes seen.
fn until_ready(c: &mut TcpStream) -> Vec<String> {
    let mut codes = vec![];
    loop {
        let mut head = [0u8; 5];
        c.read_exact(&mut head).expect("the server should answer, not close");
        let mut body = vec![0; i32::from_be_bytes(head[1..].try_into().unwrap()) as usize - 4];
        c.read_exact(&mut body).unwrap();
        match head[0] {
            b'E' => codes.extend(
                body.split(|&b| b == 0)
                    .find_map(|f| f.strip_prefix(b"C"))
                    .map(|c| String::from_utf8_lossy(c).into_owned()),
            ),
            b'Z' => return codes,
            _ => {}
        }
    }
}

fn startup() -> Vec<u8> {
    let body = [&196608i32.to_be_bytes()[..], b"user\0tester\0database\0main\0\0"].concat();
    [&(body.len() as i32 + 4).to_be_bytes()[..], &body].concat()
}

/// Whether the server closed `c` (the read timeout on `c` is the wait).
fn closed(c: &mut TcpStream) -> bool {
    let mut b = [0u8; 64];
    match c.read(&mut b) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
    }
}

/// A binary array whose element is an array, 200k levels deep, is refused, not recursed into
/// until the connection's stack overflows (which took the whole server down).
#[test]
fn nested_binary_arrays_are_refused() {
    let s = start("nested", &[], false);
    let mut c = TcpStream::connect(("127.0.0.1", s.pg)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    c.write_all(&startup()).unwrap();
    assert!(until_ready(&mut c).is_empty());
    // int[] (1007) holding one int[], holding one int[], ..., holding an empty int[]
    const LEVELS: usize = 200_000;
    let mut array = vec![];
    for i in (1..=LEVELS).rev() {
        for x in [1, 0, 1007, 1, 1, 12 + 24 * (i as i32 - 1)] {
            array.extend(x.to_be_bytes());
        }
    }
    array.extend([0i32, 0, 23].iter().flat_map(|x| x.to_be_bytes()));
    let parse = [&b"\0SELECT $1\0"[..], &1i16.to_be_bytes(), &1007i32.to_be_bytes()].concat();
    let bind = [
        &b"\0\0"[..],
        &1i16.to_be_bytes(),
        &1i16.to_be_bytes(), // binary
        &1i16.to_be_bytes(),
        &(array.len() as i32).to_be_bytes(),
        &array,
        &0i16.to_be_bytes(),
    ]
    .concat();
    c.write_all(&[msg(b'P', &parse), msg(b'B', &bind), msg(b'S', &[])].concat()).unwrap();
    assert_eq!(until_ready(&mut c), ["22P03"]);
    let mut ok = Client::connect(&format!("host=127.0.0.1 port={} user=tester dbname=main", s.pg), NoTls).unwrap();
    assert_eq!(ok.query_one("select 1", &[]).unwrap().get::<_, i32>(0), 1, "the server is still up");
}

/// A login gets CHRONOS_LOGIN_TIMEOUT in all, not per read: a startup message sent a byte at a
/// time, or SSLRequest after SSLRequest, is dropped when it's up.
#[test]
fn slow_logins_are_dropped() {
    let s = start("login", &[("CHRONOS_LOGIN_TIMEOUT", "1")], false);
    // a byte every 300 ms: each read is quick, the whole message would take 10 s
    let mut c = TcpStream::connect(("127.0.0.1", s.pg)).unwrap();
    c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    let t = Instant::now();
    let mut gone = false;
    for b in startup() {
        if c.write_all(&[b]).is_err() || closed(&mut c) {
            gone = true;
            break;
        }
    }
    assert!(
        gone && t.elapsed() < Duration::from_secs(4),
        "a trickled startup is dropped at the deadline: {:?}",
        t.elapsed()
    );

    // SSLRequest over and over, each answered "N" (no TLS here)
    let mut c = TcpStream::connect(("127.0.0.1", s.pg)).unwrap();
    c.set_read_timeout(Some(Duration::from_millis(400))).unwrap();
    let ssl = [8i32.to_be_bytes(), 80877103i32.to_be_bytes()].concat();
    let t = Instant::now();
    let mut gone = false;
    while t.elapsed() < Duration::from_secs(10) {
        if c.write_all(&ssl).is_err() {
            gone = true;
            break;
        }
        let mut n = [0u8; 1];
        match c.read(&mut n) {
            Ok(1) => assert_eq!(n[0], b'N'),
            Ok(_) => {
                gone = true;
                break;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => {
                gone = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    assert!(gone && t.elapsed() < Duration::from_secs(4), "an SSLRequest loop is dropped: {:?}", t.elapsed());

    // a real client still logs in
    let mut ok = Client::connect(&format!("host=127.0.0.1 port={} user=tester dbname=main", s.pg), NoTls).unwrap();
    assert_eq!(ok.query_one("select 1", &[]).unwrap().get::<_, i32>(0), 1);
}

/// An HTTPS client that never finishes its TLS handshake is dropped at CHRONOS_REQUEST_TIMEOUT,
/// not held for as long as it likes.
#[test]
fn stalled_tls_handshake_is_dropped() {
    let s = start("tls-stall", &[("CHRONOS_REQUEST_TIMEOUT", "1")], true);
    let mut c = TcpStream::connect(("127.0.0.1", s.http)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let t = Instant::now();
    c.write_all(&[0x16, 0x03, 0x01]).unwrap(); // the start of a ClientHello, then nothing
    assert!(closed(&mut c), "the server should close the connection");
    assert!(t.elapsed() < Duration::from_secs(5), "dropped at the request deadline: {:?}", t.elapsed());
}

/// Runs `chronos <dir>` with `stdin` (through the owner's local socket while one serves it).
fn chronos(dir: &Path, args: &[&str], stdin: &str) -> String {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .arg(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let o = c.wait_with_output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

/// A long-running `chronos mcp <dir>`, the folder's owner, serving it on its local socket.
struct Owner {
    child: Child,
    input: std::process::ChildStdin,
    output: BufReader<std::process::ChildStdout>,
}

impl Owner {
    fn new(dir: &Path) -> Owner {
        let mut child = Command::new(env!("CARGO_BIN_EXE_chronos"))
            .args(["mcp", dir.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let (input, output) = (child.stdin.take().unwrap(), BufReader::new(child.stdout.take().unwrap()));
        Owner { child, input, output }
    }

    fn call(&mut self, name: &str, args: &str) -> serde_json::Value {
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
        );
        writeln!(self.input, "{line}").unwrap();
        let mut reply = String::new();
        self.output.read_line(&mut reply).unwrap();
        serde_json::from_str(&reply).unwrap()
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The owner's socket for `dir`, as `local::path` names it.
fn socket(dir: &Path) -> PathBuf {
    let full = std::fs::canonicalize(dir).unwrap();
    let h = blake3::hash(full.to_string_lossy().as_bytes()).to_hex();
    // SAFETY: geteuid has no preconditions
    std::env::temp_dir()
        .join(format!("chronos-{}", unsafe { libc::geteuid() }))
        .join(format!("chronos-{}.sock", &h[..16]))
}

/// A local shell session that ends mid-transaction on a bad line rolls its transaction back: its
/// `_tx_` world doesn't outlive it.
#[test]
fn local_shell_ending_badly_rolls_back() {
    let dir = scratch("local-tx");
    let db = dir.join("db");
    assert!(chronos(&db, &["put", "users", "1", r#"{"name": "Ada"}"#], "").contains("put users/1"));
    let mut owner = Owner::new(&db);
    assert_eq!(owner.call("branches", "{}")["result"]["isError"], false);
    let conn = UnixStream::connect(socket(&db)).expect("the owner serves the folder");
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut out = conn.try_clone().unwrap();
    let mut lines = BufReader::new(conn).lines();
    writeln!(out, r#"{{"mode": "shell", "branch": "main"}}"#).unwrap();
    assert!(lines.next().unwrap().unwrap().contains(r#""ok":true"#));
    writeln!(out, r#"{{"cmd": "begin"}}"#).unwrap();
    assert!(lines.next().unwrap().unwrap().contains(r#""tx":true"#));
    writeln!(out, "this line is not JSON").unwrap();
    assert!(lines.next().is_none(), "the session ends on a bad line");
    let t = Instant::now();
    loop {
        let branches = chronos(&db, &["branches"], "");
        if !branches.contains("_tx_") {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "the transaction's world is left behind: {branches}");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(owner);
    let _ = std::fs::remove_dir_all(&dir);
}

/// MCP's discard of a world named `cascade` discards that world only, not the forks of it that
/// others made (it went through the shell as `discard cascade`, the keyword).
#[test]
fn mcp_discard_of_cascade_keeps_others_forks() {
    let dir = scratch("cascade");
    let db = dir.join("db");
    assert!(chronos(&db, &["put", "users", "1", r#"{"name": "Ada"}"#], "").contains("put users/1"));
    let mut owner = Owner::new(&db);
    let r = owner.call("fork", r#"{"name":"cascade"}"#);
    assert_eq!(r["result"]["isError"], false, "{r}");
    // someone else forks the agent's world
    let out = chronos(&db, &[], "fork others from cascade\n");
    assert!(out.contains("forked cascade -> others"), "{out}");
    owner.call("discard", r#"{"branch":"cascade"}"#);
    let out = chronos(&db, &["branches"], "");
    assert!(out.contains("others"), "someone else's fork survives: {out}");
    drop(owner);
    let _ = std::fs::remove_dir_all(&dir);
}
