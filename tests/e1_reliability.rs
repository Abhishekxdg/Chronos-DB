//! Reliability limits of `chronos serve` and the library: messages past the server's size limit,
//! idle sessions (`idle_in_transaction_session_timeout`, `idle_session_timeout`), the descriptor
//! limit behind `max_connections`, and leftovers of interrupted backups.

mod common;

use chronos::Db;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    dir: PathBuf,
    pg: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A server on free ports with `env` set; `ulimit` is a shell command run first (`ulimit -n 512`).
fn start(name: &str, env: &[(&str, &str)], ulimit: Option<&str>) -> Server {
    let dir = std::env::temp_dir().join(format!("chronos-e1-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let exe = env!("CARGO_BIN_EXE_chronos");
    let mut cmd = match ulimit {
        Some(u) => {
            let mut c = Command::new("sh");
            c.args(["-c", &format!("{u}; exec \"$0\" \"$@\""), exe]);
            c
        }
        None => Command::new(exe),
    };
    cmd.args(["serve", dir.join("db").to_str().unwrap()]).envs(env.iter().copied());
    let s = common::serve(&mut cmd, true);
    Server { child: s.child, dir, pg: s.pg }
}

/// One message from the server: its tag and body.
fn message(s: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    s.read_exact(&mut head).ok()?;
    let n = i32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    let mut body = vec![0; n - 4];
    s.read_exact(&mut body).ok()?;
    Some((head[0], body))
}

type Fatal = (String, String, String);

/// An ErrorResponse's severity, code and message.
fn error_of(body: &[u8]) -> Fatal {
    let field = |t: u8| {
        let mut rest = body;
        while let Some((&k, tail)) = rest.split_first() {
            if k == 0 {
                break;
            }
            let end = tail.iter().position(|&b| b == 0).unwrap();
            if k == t {
                return String::from_utf8_lossy(&tail[..end]).into_owned();
            }
            rest = &tail[end + 1..];
        }
        String::new()
    };
    (field(b'S'), field(b'C'), field(b'M'))
}

/// A connection that has logged in (trust, on loopback), up to its first ReadyForQuery. Err: the
/// server's error (a refusal).
fn connect(port: u16) -> Result<TcpStream, Fatal> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut body = 196608i32.to_be_bytes().to_vec();
    body.extend(b"user\0tester\0database\0main\0\0");
    // (a refused client's server has closed by now: its writes may fail, its error is there to read)
    let _ = s.write_all(&(body.len() as i32 + 4).to_be_bytes());
    let _ = s.write_all(&body);
    loop {
        match message(&mut s) {
            Some((b'Z', _)) => return Ok(s),
            Some((b'E', b)) => return Err(error_of(&b)),
            Some(_) => {}
            None => return Err(("closed".into(), String::new(), String::new())),
        }
    }
}

/// Runs a simple query; the messages up to ReadyForQuery (or the end of the connection).
fn query(s: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    let mut m = vec![b'Q'];
    m.extend((sql.len() as i32 + 5).to_be_bytes());
    m.extend(sql.as_bytes());
    m.push(0);
    s.write_all(&m).unwrap();
    let mut out = vec![];
    while let Some((t, b)) = message(s) {
        out.push((t, b));
        if t == b'Z' {
            break;
        }
    }
    out
}

fn first_error(msgs: &[(u8, Vec<u8>)]) -> Option<Fatal> {
    msgs.iter().find(|(t, _)| *t == b'E').map(|(_, b)| error_of(b))
}

/// Sends a simple Query of `n` bytes of text, and reads up to ReadyForQuery.
fn send_big_query(c: &mut TcpStream, n: usize) -> Vec<(u8, Vec<u8>)> {
    let mut head = vec![b'Q'];
    head.extend(((n + 4) as i32).to_be_bytes());
    c.write_all(&head).unwrap();
    let chunk = vec![b'x'; 1 << 20];
    for _ in 0..n >> 20 {
        c.write_all(&chunk).unwrap();
    }
    let mut got = vec![];
    while let Some((t, b)) = message(c) {
        got.push((t, b));
        if t == b'Z' {
            break;
        }
    }
    got
}

/// A message past the server's limit is answered with an error (54000), as a statement that
/// failed is, and the connection goes on; a length no message has ends it with 08P01 as Postgres's.
#[test]
fn oversize_messages_are_answered() {
    let s = start("big", &[], None);
    let mut c = connect(s.pg).unwrap();
    // a simple query of 70 MB (the limit is 64 MB): its text is never read as one
    let big = 70usize << 20;
    let got = send_big_query(&mut c, big);
    let (sev, code, msg) = first_error(&got).expect("an ErrorResponse");
    assert_eq!((sev.as_str(), code.as_str()), ("ERROR", "54000"), "{msg}");
    assert!(msg.contains("too large") && msg.contains("64 MB"), "{msg}");
    assert_eq!(got.last().map(|m| m.0), Some(b'Z'), "ReadyForQuery follows");
    // the connection is where it was
    let ok = query(&mut c, "select 40 + 2");
    assert!(ok.iter().any(|(t, b)| *t == b'D' && b.ends_with(b"42")), "{ok:?}");
    // inside a transaction the transaction fails, as after any error
    query(&mut c, "begin");
    let got = send_big_query(&mut c, big);
    assert_eq!(got.last().map(|m| (m.0, m.1.clone())), Some((b'Z', b"E".to_vec())));
    query(&mut c, "rollback");
    // a length past what Postgres takes: FATAL, closed
    let mut bad = vec![b'Q'];
    bad.extend(0x7fff_0000i32.to_be_bytes());
    c.write_all(&bad).unwrap();
    let (sev, code, msg) = match message(&mut c) {
        Some((b'E', b)) => error_of(&b),
        other => panic!("{other:?}"),
    };
    assert_eq!((sev.as_str(), code.as_str(), msg.as_str()), ("FATAL", "08P01", "invalid message length"));
    assert!(message(&mut c).is_none(), "closed");
}

/// `idle_in_transaction_session_timeout` ends a session idle inside a transaction (FATAL 25P03);
/// `idle_session_timeout` one idle outside it (57P05); neither runs while the client is working,
/// and the first leaves a session outside a transaction alone.
#[test]
fn idle_sessions_are_ended() {
    let s = start("idle", &[], None);
    // idle in a transaction
    let mut c = connect(s.pg).unwrap();
    query(&mut c, "set idle_in_transaction_session_timeout = '400ms'");
    query(&mut c, "create table idle_t (a int)");
    std::thread::sleep(Duration::from_millis(900)); // outside a transaction: no limit
    assert!(first_error(&query(&mut c, "select 1")).is_none());
    query(&mut c, "begin");
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(200)); // each statement starts the wait again
        assert!(first_error(&query(&mut c, "insert into idle_t values (1)")).is_none());
    }
    let t = Instant::now();
    let (sev, code, msg) = match message(&mut c) {
        Some((b'E', b)) => error_of(&b),
        other => panic!("{other:?}"),
    };
    assert_eq!((sev.as_str(), code.as_str()), ("FATAL", "25P03"), "{msg}");
    assert_eq!(msg, "terminating connection due to idle-in-transaction timeout");
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(message(&mut c).is_none(), "closed");
    // its transaction was rolled back
    let mut d = connect(s.pg).unwrap();
    let r = query(&mut d, "select count(*) from idle_t");
    assert!(r.iter().any(|(t, b)| *t == b'D' && b.ends_with(b"0")), "{r:?}");
    // an aborted transaction is idle in a transaction too
    query(&mut d, "set idle_in_transaction_session_timeout = '300ms'");
    query(&mut d, "begin");
    assert!(first_error(&query(&mut d, "select nonesuch")).is_some());
    match message(&mut d) {
        Some((b'E', b)) => assert_eq!(error_of(&b).1, "25P03"),
        other => panic!("{other:?}"),
    }
    // idle outside a transaction
    let mut e = connect(s.pg).unwrap();
    query(&mut e, "set idle_session_timeout = '300ms'");
    query(&mut e, "begin"); // in a transaction it is the other limit's: none set
    std::thread::sleep(Duration::from_millis(800));
    assert!(first_error(&query(&mut e, "select 1")).is_none());
    query(&mut e, "commit");
    let (sev, code, msg) = match message(&mut e) {
        Some((b'E', b)) => error_of(&b),
        other => panic!("{other:?}"),
    };
    assert_eq!((sev.as_str(), code.as_str()), ("FATAL", "57P05"), "{msg}");
    assert_eq!(msg, "terminating connection due to idle-session timeout");
    // 0 turns it off again
    let mut f = connect(s.pg).unwrap();
    query(&mut f, "set idle_session_timeout = '200ms'");
    query(&mut f, "set idle_session_timeout = 0");
    std::thread::sleep(Duration::from_millis(600));
    assert!(first_error(&query(&mut f, "select 1")).is_none());
    // a client that sends a message's tag and half its length, then nothing, is ended all the
    // same: the timer runs until the whole message is in
    let mut g = connect(s.pg).unwrap();
    query(&mut g, "set idle_session_timeout = '300ms'");
    g.write_all(b"Q\0\0").unwrap();
    let t = Instant::now();
    match message(&mut g) {
        Some((b'E', b)) => assert_eq!(error_of(&b).1, "57P05"),
        other => panic!("{other:?}"),
    }
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(message(&mut g).is_none(), "closed");
}

/// Connections past what the process's descriptors hold are refused with 53300, never dropped, and
/// SHOW max_connections says what is enforced; with a low soft limit and room above it, the
/// server raises it and takes far more connections than the limit it started with held.
#[test]
fn max_connections_is_what_the_descriptors_hold() {
    // a hard limit of 512 descriptors: (512 - 256) / 8 connections
    let s = start("fds-hard", &[], Some("ulimit -n 512"));
    let mut first = connect(s.pg).unwrap();
    let shown = query(&mut first, "show max_connections");
    let text = |m: &(u8, Vec<u8>)| String::from_utf8_lossy(&m.1[6..]).into_owned();
    let n: usize = shown.iter().find(|m| m.0 == b'D').map(text).unwrap().parse().unwrap();
    assert!((16..=40).contains(&n), "max_connections {n}");
    let mut held = vec![first];
    for i in 1..n {
        held.push(connect(s.pg).unwrap_or_else(|e| panic!("connection {i} of {n}: {e:?}")));
    }
    let e = connect(s.pg).expect_err("one past the limit is refused");
    assert_eq!((e.0.as_str(), e.1.as_str(), e.2.as_str()), ("FATAL", "53300", "sorry, too many clients already"));
    // those admitted work
    for c in &mut held {
        assert!(first_error(&query(c, "select 1")).is_none());
    }
    drop(held);
    drop(s);

    // a soft limit of 128 with a hard one far above it: raised, so 300 connections are served
    let s = start("fds-soft", &[], Some("ulimit -S -n 128"));
    let held: Vec<_> = (0..300).map(|i| connect(s.pg).unwrap_or_else(|e| panic!("connection {i}: {e:?}"))).collect();
    assert_eq!(held.len(), 300);
}

/// A backup removes the leftovers of interrupted ones beside it (`.NAME.partial`, marked as
/// Chronos's), but not one that a backup still holds, nor a user's folder that only looks alike.
#[test]
fn interrupted_backups_are_cleaned_up() {
    use std::os::fd::AsRawFd;
    let root = std::env::temp_dir().join(format!("chronos-e1-{}-backups", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = Db::open(root.join("db")).unwrap();
    db.put("main", "k", "{}").unwrap();
    let stale = root.join(".old.partial");
    std::fs::create_dir_all(stale.join("pages")).unwrap();
    std::fs::write(stale.join("pages").join("pack-1"), b"half a pack").unwrap();
    std::fs::write(stale.join(".chronos-backup-partial"), b"").unwrap();
    // one a live backup holds (a lock on its folder, as a backup takes)
    let live = root.join(".busy.partial");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(live.join(".chronos-backup-partial"), b"").unwrap();
    let held = std::fs::File::open(&live).unwrap();
    // SAFETY: flock on a descriptor `held` keeps open
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    // other things that merely look alike stay
    let file = root.join(".file.partial");
    std::fs::write(&file, b"not a folder").unwrap();
    let plain = root.join("partial-data");
    std::fs::create_dir_all(&plain).unwrap();
    // a user's own folder named like a backup's: no Chronos mark, so it is never removed
    let theirs = root.join(".upload.partial");
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(theirs.join("data"), b"keep me").unwrap();

    db.backup(root.join("b1")).unwrap();
    assert!(!stale.exists(), "the interrupted backup's folder is removed");
    assert!(live.exists(), "a backup that is running keeps its folder");
    assert!(file.exists() && plain.exists());
    assert_eq!(std::fs::read(theirs.join("data")).unwrap(), b"keep me");
    assert!(!root.join("b1").join(".chronos-backup-partial").exists(), "a finished backup is unmarked");
    // a backup whose temporary name is the user's folder is refused, and the folder kept
    let err = db.backup(root.join("upload")).unwrap_err().to_string();
    assert!(err.contains("not made by a Chronos backup"), "{err}");
    assert_eq!(std::fs::read(theirs.join("data")).unwrap(), b"keep me");
    assert!(root.join("b1").join("manifest").exists());
    assert!(!root.join(".b1.partial").exists());
    // a backup into a name a live one is using is refused, not mixed with it
    let busy = root.join("busy");
    let err = db.backup(&busy).unwrap_err().to_string();
    assert!(err.contains("already running"), "{err}");
    assert!(live.exists());
    drop(held);
    db.backup(&busy).unwrap();
    assert!(!live.exists(), "once its holder is gone, it is a leftover and the backup took its name");
    // a backup killed between making its folder and marking it leaves it empty: once that is a minute old,
    // the next backup takes it over; a just-made empty one (another process's, about to be marked) is left
    let crashed = root.join(".crashed.partial");
    std::fs::create_dir_all(&crashed).unwrap();
    let ago = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    std::fs::File::open(&crashed).unwrap().set_modified(ago).unwrap();
    let fresh = root.join(".fresh.partial");
    std::fs::create_dir_all(&fresh).unwrap();
    db.backup(root.join("crashed")).unwrap();
    assert!(root.join("crashed").join("manifest").exists() && !crashed.exists());
    assert!(fresh.exists(), "an empty folder made a moment ago is left alone");
    let _ = std::fs::remove_dir_all(&root);
}
