//! `chronos serve`'s listeners against hostile clients: web pages (CSRF, DNS rebinding), requests
//! that don't parse the same way twice, oversized or unauthenticated payloads, a public address
//! that only looks local, the local socket's directory, and the Python client's error handling.

mod common;

use postgres::{Client, NoTls};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

struct Server {
    child: Child,
    dir: PathBuf,
    http: u16,
    pg: u16,
    /// what the server printed while starting
    log: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-review-net-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `scratch`, where a Unix socket's path fits: macOS's temp directory is already some 50 bytes, and a socket path is
/// limited to 104 (the server's socket is `<tmp>/chronos-<uid>/chronos-<hash>.sock`).
fn short_scratch(name: &str) -> PathBuf {
    let base = if cfg!(target_os = "macos") { PathBuf::from("/tmp") } else { std::env::temp_dir() };
    let d = base.join(format!("crn-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A server on free ports (HTTP and Postgres), with `tmp` as its temp directory.
fn start(name: &str, token: Option<&str>, tmp: Option<&PathBuf>) -> Server {
    let dir = scratch(name);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.join("db").to_str().unwrap()]);
    if let Some(t) = token {
        cmd.args(["--token", t, "--unsafe"]); // the token as the database's own user, as before 0.4
    }
    if let Some(t) = tmp {
        cmd.env("TMPDIR", t);
    }
    let s = common::serve(&mut cmd, true);
    Server { child: s.child, dir, http: s.http, pg: s.pg, log: s.log }
}

/// Sends `req` as is, and reads the reply until the server closes the connection.
fn raw(port: u16, req: &[u8]) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(req).unwrap();
    let mut out = vec![];
    s.read_to_end(&mut out).expect("the server should answer and close the connection");
    String::from_utf8_lossy(&out).into_owned()
}

fn post(port: u16, extra: &str, body: &str) -> String {
    let req =
        format!("POST /v1/sql HTTP/1.1\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    raw(port, req.as_bytes())
}

fn status(reply: &str) -> &str {
    reply.split(' ').nth(1).unwrap_or("")
}

/// A web page can't use a tokenless server through its reader's browser.
#[test]
fn web_pages_are_refused() {
    let s = start("web", None, None);
    let json = "Content-Type: application/json\r\n";
    let local = format!("Host: 127.0.0.1:{}\r\n", s.http);
    let make = r#"{"sql": "create table t (id int primary key)"}"#;
    // fetch() with a text/plain body (no preflight), from another site
    let r = post(s.http, &format!("{local}Origin: http://evil.example\r\nContent-Type: text/plain\r\n"), make);
    assert_eq!(status(&r), "403", "{r}");
    let r = post(s.http, &format!("{local}Origin: http://evil.example\r\n{json}"), make);
    assert_eq!(status(&r), "403", "an Origin is refused whatever the body: {r}");
    // a body that isn't declared JSON (a form, or a Blob), even without an Origin
    assert_eq!(status(&post(s.http, &format!("{local}Content-Type: text/plain\r\n"), make)), "415");
    assert_eq!(status(&post(s.http, &local, make)), "415");
    // DNS rebinding: the page's own name, pointing at 127.0.0.1
    assert_eq!(status(&post(s.http, &format!("Host: evil.example:{}\r\n{json}", s.http), make)), "403");
    assert_eq!(status(&post(s.http, &format!("Host: localhost.evil.example\r\n{json}"), make)), "403");
    // none of those ran
    let r = post(s.http, &format!("{local}{json}"), r#"{"sql": "select * from t"}"#);
    assert_ne!(status(&r), "200", "no table t yet: {r}");
    // a program on this machine still can
    for host in [local.clone(), format!("Host: localhost:{}\r\n", s.http), "Host: [::1]\r\n".into()] {
        let r =
            post(s.http, &format!("{host}Content-Type: application/json; charset=utf-8\r\n"), r#"{"sql": "select 1"}"#);
        assert_eq!(status(&r), "200", "{host}: {r}");
    }
    assert_eq!(status(&post(s.http, &format!("{local}{json}"), make)), "200");
}

/// A server with a token checks it before reading (or allocating) the body.
#[test]
fn auth_comes_before_the_body() {
    let s = start("auth-first", Some("secret-secret-secret-secret"), None);
    let head = "POST /v1/sql HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 30000000\r\n\r\n";
    let r = raw(s.http, head.as_bytes());
    assert_eq!(status(&r), "401", "refused without waiting for 30 MB: {r}");
    // with a token the Host isn't checked (a proxy may name the server anything)
    let r = post(
        s.http,
        "Host: db.example\r\nAuthorization: Bearer secret-secret-secret-secret\r\nContent-Type: application/json\r\n",
        r#"{"sql": "select 1"}"#,
    );
    assert_eq!(status(&r), "200", "{r}");
}

/// Headers a proxy could read differently from the server are refused, and the server survives
/// garbage.
#[test]
fn ambiguous_requests_are_refused() {
    let s = start("smuggle", None, None);
    let json = "Host: 127.0.0.1\r\nContent-Type: application/json\r\n";
    let r = raw(
        s.http,
        format!("POST /v1/sql HTTP/1.1\r\n{json}Content-Length: 2\r\nContent-Length: 40\r\n\r\n{{}}").as_bytes(),
    );
    assert_eq!(status(&r), "400", "conflicting lengths: {r}");
    let r = raw(s.http, format!("POST /v1/sql HTTP/1.1\r\n{json}Content-Length : 2\r\n\r\n{{}}").as_bytes());
    assert_eq!(status(&r), "400", "space before the colon: {r}");
    let r = raw(s.http, format!("POST /v1/sql HTTP/1.1\r\n{json}Content-Length: +2\r\n\r\n{{}}").as_bytes());
    assert_eq!(status(&r), "400", "a signed length: {r}");
    let r =
        raw(s.http, format!("POST /v1/sql HTTP/1.1\r\n{json}Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n").as_bytes());
    assert_eq!(status(&r), "411", "no chunked bodies: {r}");
    raw(s.http, b"\x00\xff garbage \r\n\r\n");
    let r = raw(s.http, b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    assert_eq!(status(&r), "200", "{r}");
}

/// Before login, a Postgres client can't make the server allocate a big message.
#[test]
fn postgres_startup_is_small() {
    let s = start("pg-startup", None, None);
    let mut c = TcpStream::connect(("127.0.0.1", s.pg)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    // a startup message that says it's 1 MB long
    c.write_all(&[&1_000_000i32.to_be_bytes()[..], &196608i32.to_be_bytes()].concat()).unwrap();
    let mut b = [0u8; 16];
    match c.read(&mut b) {
        Ok(n) => assert_eq!(n, 0, "closed at once"),
        Err(e) => {
            assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset, "closed at once, not waiting for the megabyte")
        }
    }
    let mut ok = Client::connect(&format!("host=127.0.0.1 port={} user=tester dbname=main", s.pg), NoTls).unwrap();
    assert_eq!(ok.query_one("select 1", &[]).unwrap().get::<_, i32>(0), 1);
}

/// A FunctionCall (the fast path libpq's lo_* functions use) takes its arguments as values, never as
/// SQL: a text-format integer that isn't one is 22P02 (nothing it says runs), a binary one of the
/// wrong size 22P03, and the wrong number of arguments 08P01, each followed by ReadyForQuery.
#[test]
fn function_call_arguments_are_values_not_sql() {
    let s = start("fastpath", None, None);
    let mut c = TcpStream::connect(("127.0.0.1", s.pg)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut startup = 196608i32.to_be_bytes().to_vec();
    for p in ["user", "tester", "database", "main", ""] {
        startup.extend(p.as_bytes());
        startup.push(0);
    }
    c.write_all(&[&((startup.len() + 4) as i32).to_be_bytes()[..], &startup].concat()).unwrap();
    // the messages up to ReadyForQuery: their tags, and an error's SQLSTATE
    let until_ready = |c: &mut TcpStream| {
        let mut seen = vec![];
        loop {
            let mut head = [0u8; 5];
            c.read_exact(&mut head).unwrap();
            let mut body = vec![0u8; i32::from_be_bytes(head[1..].try_into().unwrap()) as usize - 4];
            c.read_exact(&mut body).unwrap();
            if head[0] == b'E' {
                let code = body.split(|&b| b == 0).find(|f| f.first() == Some(&b'C')).unwrap();
                seen.push(format!("E{}", String::from_utf8_lossy(&code[1..])));
            } else {
                seen.push((head[0] as char).to_string());
            }
            if head[0] == b'Z' {
                return seen;
            }
        }
    };
    let send = |c: &mut TcpStream, tag: u8, body: &[u8]| {
        c.write_all(&[&[tag][..], &((body.len() + 4) as i32).to_be_bytes(), body].concat()).unwrap();
    };
    until_ready(&mut c);
    send(&mut c, b'Q', b"create table fp (id int)\0");
    assert!(!until_ready(&mut c).iter().any(|m| m.starts_with('E')));
    // lo_open (952): (format codes, arguments, result format)
    let call = |args: &[(i16, &[u8])]| {
        let mut b = 952i32.to_be_bytes().to_vec();
        b.extend((args.len() as i16).to_be_bytes());
        for (f, _) in args {
            b.extend(f.to_be_bytes());
        }
        b.extend((args.len() as i16).to_be_bytes());
        for (_, v) in args {
            b.extend((v.len() as i32).to_be_bytes());
            b.extend(*v);
        }
        b.extend(0i16.to_be_bytes());
        b
    };
    let sneak = b"1, 1); drop table fp; select lo_open(1".as_slice();
    send(&mut c, b'F', &call(&[(0, sneak), (0, b"262144")]));
    assert_eq!(until_ready(&mut c), ["E22P02", "Z"]);
    send(&mut c, b'F', &call(&[(1, b"123"), (0, b"262144")]));
    assert_eq!(until_ready(&mut c), ["E22P03", "Z"]);
    send(&mut c, b'F', &call(&[(0, b"1")]));
    assert_eq!(until_ready(&mut c), ["E08P01", "Z"]);
    // a number, padded as libpq may send it, is one: lo_open of an object that isn't there
    send(&mut c, b'F', &call(&[(0, b" 4242 "), (0, b"262144")]));
    assert_eq!(until_ready(&mut c), ["E42704", "Z"]);
    send(&mut c, b'Q', b"select count(*) from fp\0");
    assert_eq!(until_ready(&mut c), ["T", "D", "C", "Z"], "the table is still there");
}

/// A name that merely starts with "localhost" isn't loopback: it needs a token.
#[test]
fn a_public_name_needs_a_token() {
    let dir = scratch("public-name");
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["serve", dir.join("db").to_str().unwrap(), "--listen", "localhost.example.invalid:7070", "--pg", "off"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("without a token"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The local socket lives in a directory only its user can reach, and a directory someone else
/// could have made is refused.
#[test]
fn local_socket_is_private() {
    let tmp = short_scratch("sock-tmp");
    let uid = std::fs::metadata(&tmp).unwrap().uid();
    let s = start("sock", None, Some(&tmp));
    let d = tmp.join(format!("chronos-{uid}"));
    let m = std::fs::symlink_metadata(&d).unwrap();
    assert!(m.is_dir());
    assert_eq!(m.mode() & 0o777, 0o700);
    assert!(std::fs::read_dir(&d).unwrap().flatten().any(|e| e.file_name().to_string_lossy().ends_with(".sock")));
    drop(s);

    let open = short_scratch("sock-open");
    let d = open.join(format!("chronos-{uid}"));
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777)).unwrap();
    let s = start("sock-refused", None, Some(&open));
    assert!(s.log.contains("can't share this folder"), "{}", s.log);
    assert!(std::fs::read_dir(&d).unwrap().next().is_none(), "no socket in a directory others can write");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&open);
}

/// The Python client reports a proxy's non-JSON error with its status, and doesn't follow
/// redirects (which would carry its token elsewhere).
#[test]
fn python_client_errors_and_redirects() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: python3 not installed");
        return;
    }
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let replies = [
        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/html\r\nContent-Length: 12\r\nConnection: close\r\n\r\n<h1>502</h1>",
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ];
    let fake = std::thread::spawn(move || {
        for reply in replies {
            let (mut c, _) = l.accept().unwrap();
            // the whole request, so closing doesn't reset the connection before the reply is read
            c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let mut b = [0u8; 4096];
            while c.read(&mut b).is_ok_and(|n| n > 0) {}
            c.write_all(reply.as_bytes()).unwrap();
        }
    });
    let script = format!(
        r#"
from chronos import Chronos, ChronosError
db = Chronos("http://127.0.0.1:{port}", token="t")
for want in (502, 302):
    try:
        db.branches()
        raise SystemExit("no error")
    except ChronosError as e:
        assert e.status == want, (e.status, str(e))
"#
    );
    let out = Command::new("python3")
        .args(["-c", &script])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/clients/python"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    fake.join().unwrap();
}
