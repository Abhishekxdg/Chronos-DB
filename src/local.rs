//! One process holds a folder's database (its LOCK). So that an agent's `chronos mcp` and the person
//! approving its work can use the same folder at once, the first long-lived `chronos` on a folder
//! (`mcp`, the interactive shell, `serve`) also listens on a Unix socket only its user can reach.
//! A later `chronos mcp`, shell command or `import` on that folder connects and runs there, as a
//! session of its own, instead of failing with "already open".
//!
//!   client ── hello {mode, branch, agent, allow_merge | table, data, schema} ──> owner
//!             owner: a new Shell (own branch, own SQL session) on the shared database
//!     mcp     JSON-RPC lines both ways, exactly as over stdio
//!     shell   {"cmd"} per line      <── {"ok", "out", "branch", "tx"} per command
//!     import  one request           <── {"ok", "out"}
//!     api     {"op", "args", "role"?} or {"sign_in"} per line <── {"status", "body"} per request (`chronos studio`)
//! Every reply to the hello is {"ok", "out"} first: ok false means refused, with the reason.
//!
//! One-shot commands never listen (they'd make a client wait on them); a process that finds the
//! folder open and nobody listening retries for a few seconds, which covers them.

use crate::{Shell, mcp};
use chronos::Db;
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The socket for a folder: in a directory of the user's own in the temp directory (a folder path
/// can be too long for a socket name), named by a hash of the folder's full path.
pub fn path(dir: &str) -> io::Result<PathBuf> {
    let full = std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
    let h = blake3::hash(full.to_string_lossy().as_bytes()).to_hex();
    Ok(private_dir()?.join(format!("chronos-{}.sock", &h[..16])))
}

/// A file named `name` in this user's private directory (see `private_dir`).
pub fn private_file(name: &str) -> Option<PathBuf> {
    private_dir().ok().map(|d| d.join(name))
}

/// temp/chronos-<uid>: made mode 0700 if missing, and refused unless it's a real directory that
/// only this user can reach (in a shared /tmp, anyone could have made it first).
fn private_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    // SAFETY: geteuid has no preconditions and can't fail
    let uid = unsafe { libc::geteuid() };
    let d = std::env::temp_dir().join(format!("chronos-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&d) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let m = std::fs::symlink_metadata(&d)?;
    if !m.file_type().is_dir() || m.uid() != uid || m.mode() & 0o077 != 0 {
        return Err(io::Error::other(format!("{} isn't a directory only this user can use", d.display())));
    }
    Ok(d)
}

/// Whether the process at the other end of `s` runs as this user.
fn same_user(s: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: geteuid has no preconditions and can't fail
    let me = unsafe { libc::geteuid() };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: SO_PEERCRED fills the ucred we own, of the length we pass
        let r = unsafe {
            libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len)
        };
        r == 0 && cred.uid == me
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: getpeereid writes the two ids we own
        let r = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
        r == 0 && uid == me
    }
}

/// A folder's socket, served by this process while it has the database open.
pub struct Server {
    path: PathBuf,
    live: Arc<AtomicUsize>, // sessions connected now
}

/// Starts serving `db` to other `chronos` processes on `dir`'s socket, in background threads.
pub fn listen(db: Arc<Db>, dir: &str) -> io::Result<Server> {
    let path = path(dir)?;
    // we hold the folder's lock, so a socket file left here is from a process that died
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let live = Arc::new(AtomicUsize::new(0));
    let l = live.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else {
                std::thread::sleep(Duration::from_millis(100)); // out of file descriptors, say
                continue;
            };
            if !same_user(&conn) {
                continue;
            }
            let (db, live) = (db.clone(), l.clone());
            live.fetch_add(1, Ordering::SeqCst);
            let run = live.clone();
            let spawned = std::thread::Builder::new().stack_size(crate::http::STACK).spawn(move || {
                let _slot = crate::http::Release(&run);
                crate::http::guarded(|| {
                    let _ = session(db, conn);
                });
            });
            if spawned.is_err() {
                live.fetch_sub(1, Ordering::SeqCst);
            }
        }
    });
    Ok(Server { path, live })
}

impl Server {
    /// Stops taking new sessions and waits for connected ones to end, so the database stays open
    /// for them; then this process may close it and exit.
    pub fn drain(self) {
        let _ = std::fs::remove_file(&self.path); // new clients now wait for the lock instead
        let mut told = false;
        while self.live.load(Ordering::SeqCst) > 0 {
            if !told {
                eprintln!(
                    "chronos: waiting for {} other session(s) on this database to finish",
                    self.live.load(Ordering::SeqCst)
                );
                told = true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// One client's session, on the shared database.
fn session(db: Arc<Db>, conn: UnixStream) -> io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut out = conn;
    let mut hello = String::new();
    reader.read_line(&mut hello)?;
    let hello: Value = serde_json::from_str(&hello).map_err(io::Error::other)?;
    let branch = hello["branch"].as_str().unwrap_or(chronos::MAIN).to_string();
    let reply = |out: &mut UnixStream, ok: bool, text: String| writeln!(out, "{}", json!({"ok": ok, "out": text}));
    if db.branches().iter().all(|(b, _)| *b != branch) {
        return reply(&mut out, false, format!("no branch '{branch}'"));
    }
    let mut sh = Shell { db: db.clone(), branch: branch.clone(), sql: chronos::sql::Session::new(&branch) };
    match hello["mode"].as_str().unwrap_or("") {
        "mcp" => {
            let guard = match hello["agent"].as_str() {
                Some(name) => match db.as_agent(name) {
                    Ok(_) => mcp::Guard::agent(name),
                    Err(e) => return reply(&mut out, false, e.to_string()),
                },
                None => mcp::Guard::new(
                    hello["allow_merge"].as_bool() == Some(true),
                    hello["unmask"].as_bool() == Some(true),
                ),
            };
            reply(&mut out, true, String::new())?;
            mcp::serve_io(sh, guard, reader, out);
        }
        "shell" => {
            reply(&mut out, true, String::new())?;
            // a transaction the client leaves open is rolled back as `sh` drops, however this ends
            for line in reader.lines() {
                let cmd: Value = serde_json::from_str(&line?).map_err(io::Error::other)?;
                let (ok, text) = match sh.exec(cmd["cmd"].as_str().unwrap_or("")) {
                    Ok(t) => (true, t),
                    Err(e) => (false, e),
                };
                let tx = sh.sql.status() != b'I';
                writeln!(out, "{}", json!({"ok": ok, "out": text, "branch": sh.branch, "tx": tx}))?;
            }
        }
        "import" => {
            let s = |k: &str| hello[k].as_str().unwrap_or("").to_string();
            reply(&mut out, true, String::new())?;
            let r = crate::import::run(&db, &branch, &s("table"), &s("data"), &s("schema"));
            let (ok, text) = match r {
                Ok(t) => (true, t),
                Err(e) => (false, e),
            };
            reply(&mut out, ok, text)?;
        }
        "api" => {
            // `chronos studio` on a folder another process holds: {"op", "args"} per line, as the database's
            // owner or with "role" as a role signed in to it (checked again here), or {"sign_in"} (see
            // `studio::serve_line`); the peer is this user (see `same_user`), as for every mode
            reply(&mut out, true, String::new())?;
            for line in reader.lines() {
                let r: Value = serde_json::from_str(&line?).map_err(io::Error::other)?;
                let (status, body) = crate::studio::serve_line(&db, &r);
                writeln!(out, "{}", json!({"status": status, "body": body}))?;
            }
        }
        m => reply(&mut out, false, format!("unknown mode '{m}'"))?,
    }
    Ok(())
}

/// A connection to the process that has `dir` open, after the hello: None if nobody serves it.
pub fn connect(dir: &str, hello: Value) -> Option<(BufReader<UnixStream>, UnixStream)> {
    let conn = UnixStream::connect(path(dir).ok()?).ok()?;
    if !same_user(&conn) {
        return None; // someone else's socket: don't send it our work
    }
    let mut out = conn.try_clone().ok()?;
    writeln!(out, "{hello}").ok()?;
    let mut reader = BufReader::new(conn);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let r: Value = serde_json::from_str(&first).ok()?;
    if r["ok"] != true {
        eprintln!("chronos: {}", r["out"].as_str().unwrap_or("refused"));
        std::process::exit(1);
    }
    Some((reader, out))
}

/// Opens `dir`, or, if another process has it open, connects to that process with `hello`.
/// Waits a few seconds for a process that has it open without serving it (a one-shot command).
pub fn open_or_connect<T>(
    dir: &str,
    mut open: impl FnMut() -> Result<T, String>,
    hello: Option<Value>,
) -> Result<Opened<T>, String> {
    let start = Instant::now();
    loop {
        if let Some(h) = &hello
            && let Some(c) = connect(dir, h.clone())
        {
            return Ok(Opened::Remote(c.0, c.1));
        }
        match open() {
            Ok(db) => return Ok(Opened::Here(db)),
            Err(e) if e.contains("already open in another process") && start.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(e) => return Err(e),
        }
    }
}

pub enum Opened<T> {
    Here(T),
    Remote(BufReader<UnixStream>, UnixStream),
}

/// Relays stdin to the owner and its replies to stdout, line by line: `chronos mcp` as a client.
pub fn pipe(mut from: BufReader<UnixStream>, to: UnixStream) {
    let mut to_owner = to.try_clone().expect("socket");
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if writeln!(to_owner, "{line}").is_err() {
                break;
            }
        }
        let _ = to_owner.shutdown(std::net::Shutdown::Write);
    });
    let mut stdout = io::stdout().lock();
    let mut line = String::new();
    while from.read_line(&mut line).unwrap_or(0) > 0 {
        if stdout.write_all(line.as_bytes()).and_then(|_| stdout.flush()).is_err() {
            break;
        }
        line.clear();
    }
    let _ = to.shutdown(std::net::Shutdown::Both);
}

/// A shell session run by the owner: sends one command, returns (ok, output, branch, in a tx).
pub struct Remote {
    from: BufReader<UnixStream>,
    to: UnixStream,
}

impl Remote {
    pub fn new(from: BufReader<UnixStream>, to: UnixStream) -> Remote {
        Remote { from, to }
    }

    pub fn run(&mut self, cmd: &str) -> io::Result<(bool, String, String, bool)> {
        writeln!(self.to, "{}", json!({"cmd": cmd}))?;
        let mut line = String::new();
        if self.from.read_line(&mut line)? == 0 {
            return Err(io::Error::other("the chronos process holding this database stopped"));
        }
        let r: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
        let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
        Ok((r["ok"] == true, s("out"), s("branch"), r["tx"] == true))
    }
}

/// `path` made absolute against this process's working directory, for the owner to open.
pub fn absolute(p: &str) -> String {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) }
        .to_string_lossy()
        .into_owned()
}
