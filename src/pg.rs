//! `chronos serve` also speaks the Postgres protocol (default 127.0.0.1:5433), so psql, GUI tools
//! and Postgres drivers in any language can connect. The database name is the branch:
//!
//!   psql postgres://127.0.0.1:5433/main
//!
//! Simple and extended queries, text and binary values.
//! Logging in (see `Conn::startup`): an agent proves with SCRAM-SHA-256 (with channel binding over
//! TLS) that it knows its token, against the verifier made with it; one made before agents had
//! verifiers gets one when it first uses its token over the HTTP API (with a server token it is
//! asked for SCRAM as every name is; on loopback without one it may send its token in the clear
//! once, which gives it one too). A role made with CREATE ROLE
//! that has a password proves it knows it with SCRAM, or MD5 when it is an md5 hash, on loopback
//! too. Any other name proves with SCRAM that it knows the server's token (or the admin token);
//! without a server token (loopback only) it needs no password. With a server token, every name
//! gets a SCRAM salt of its own, so the exchange doesn't tell which names exist. A client that
//! isn't an agent and proved the server's token (or, with an admin token configured, nothing) is
//! the guest agent (see `chronos::GUEST`, `crate::Mode`); one that proved the admin token is the
//! database's own user, as is every client of a server with no token at all, or with `--unsafe`
//! one that proved the server's token. A role that proved its own password is that role, never
//! the guest.
//! psql's \d, \dt and \l work: their catalog queries are recognised and answered from Chronos's
//! tables and branches (see `catalog`).
//! With `--tls-cert` and `--tls-key` every connection must use TLS (psql's default, sslmode=prefer,
//! does); a client that doesn't is refused.
//! Cancel requests (psql's Ctrl-C) stop the statement running on the connection they name.
//! COPY ... FROM STDIN and TO STDOUT use the copy sub-protocol (psql's \copy, drivers' copy).
//! A connection's session holds its prepared statements (Parse and SQL PREPARE share names) and
//! its temporary tables.
//! LISTEN / NOTIFY: notifications go out before ReadyForQuery, and a connection waiting for its
//! client wakes to send them at once (see `Conn::wait`).
//! ponytail: no general pg_catalog, and Execute returns every row (its row limit is ignored).

use chronos::Db;
use chronos::sql::scram::{self, Login, Verifier};
use chronos::sql::{self, Output, Prepared, Reg, Session, SqlError, Statement, Type, Value};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::{ServerConnection, StreamOwned};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak, mpsc};
use std::time::Instant;

const MAX_MESSAGE: usize = 64 << 20;
/// How long a COPY FROM STDIN waits for its client's next data before dropping it
/// (CHRONOS_COPY_IDLE, seconds): it holds its world's writes meanwhile.
static COPY_IDLE: LazyLock<std::time::Duration> =
    LazyLock::new(|| std::time::Duration::from_secs(crate::http::env("CHRONOS_COPY_IDLE", 60).max(1)));
/// Before a client has logged in: its messages' size (as Postgres's), and how long it may take.
const STARTUP_MESSAGE: usize = 10_000;
/// A password message's length, at most (Postgres's PG_MAX_AUTH_TOKEN_LENGTH): past it, the
/// connection closes.
const PASSWORD_MESSAGE: usize = 65_535;
/// A SASL message's length, at most (Postgres's PG_MAX_SASL_MESSAGE_LENGTH): past it, 28P01.
const SASL_MESSAGE: usize = 1024;
static STARTUP_TIME: LazyLock<std::time::Duration> =
    LazyLock::new(|| std::time::Duration::from_secs(crate::http::env("CHRONOS_LOGIN_TIMEOUT", 60)));

/// Drops a client still logging in CHRONOS_LOGIN_TIMEOUT after it connected, however it spends that
/// time (a byte a minute, SSLRequest after SSLRequest, a stalled TLS handshake): one thread shuts
/// down the sockets of those not done. Dropping `login` (once logged in, or gone) takes it off.
fn login_deadline(login: &Arc<TcpStream>) {
    type Entry = (Instant, Weak<TcpStream>);
    static TX: LazyLock<mpsc::Sender<Entry>> = LazyLock::new(|| {
        let (tx, rx) = mpsc::channel::<Entry>();
        std::thread::spawn(move || {
            // every deadline is the same time after its accept, so they arrive in order
            let mut q = std::collections::VecDeque::<Entry>::new();
            loop {
                let next = match q.front() {
                    Some((t, _)) => rx.recv_timeout(t.saturating_duration_since(Instant::now())).ok(),
                    None => rx.recv().ok(),
                };
                q.extend(next);
                while q.front().is_some_and(|(t, _)| *t <= Instant::now()) {
                    if let Some(s) = q.pop_front().and_then(|(_, w)| w.upgrade()) {
                        let _ = s.shutdown(Shutdown::Both);
                    }
                }
            }
        });
        tx
    });
    // 0 means no login deadline, as does one too far off to represent
    let Some(t) = Instant::now().checked_add(*STARTUP_TIME).filter(|_| !STARTUP_TIME.is_zero()) else { return };
    let _ = TX.send((t, Arc::downgrade(login)));
}

/// Open connections by backend key (an ID, and a secret that proves a cancel request comes from
/// the client that was given it): psql's Ctrl-C opens a new connection that names the one to stop.
static KEYS: LazyLock<Mutex<HashMap<i32, Key>>> = LazyLock::new(Default::default);

/// A connection's secret, and the flag that stops its running statement.
type Key = (i32, Arc<AtomicBool>);

/// Whether the client at the other end of `c` has closed its connection: a peek that neither
/// waits nor takes bytes the connection's reader hasn't read yet.
fn peer_gone(c: Arc<TcpStream>) -> impl Fn() -> bool + Send + Sync {
    use std::os::fd::AsRawFd;
    // the connection's own socket, shared (not a clone: a descriptor each, a connection's share of
    // `http::max_connections`): the connection swaps its streams for TLS, closing theirs
    move || {
        let mut b = [0u8; 1];
        // SAFETY: a one-byte peek into a buffer we own, on a socket `c` keeps open
        let n = unsafe { libc::recv(c.as_raw_fd(), b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
        let again =
            || matches!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted);
        n == 0 || (n < 0 && !again())
    }
}

/// The table a partition's copy of trigger `name` comes from, its parent being `parent`: up the partition tree to
/// the first whose trigger is its own (what psql's `ON TABLE` names), as regclass shows it.
fn trigger_root(tables: &[sql::TableInfo], parent: &str, name: &str) -> String {
    let mut at = parent.to_string();
    for _ in 0..64 {
        let Some(t) = tables.iter().find(|t| t.name == at) else { break };
        let k = t.triggers.iter().position(|x| x.0 == name);
        match (k.and_then(|k| t.trigger_links.get(k)).is_some_and(|l| l.2), &t.hier.of) {
            (true, Some((p, ..))) => at = p.clone(),
            _ => break,
        }
    }
    sql::shown_name(&at)
}

/// Now, in µs since 1970.
fn micros() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as i64)
}

/// A new backend key: the next ID, and a secret from the system's CSPRNG (as Postgres's
/// pg_strong_random makes it), so a cancel request can't be forged by guessing it.
fn backend_key() -> io::Result<(i32, i32)> {
    static NEXT: AtomicI32 = AtomicI32::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut secret = [0u8; 4];
    SystemRandom::new().fill(&mut secret).map_err(|_| io::Error::other("no random numbers for a backend key"))?;
    Ok((id, i32::from_be_bytes(secret)))
}

/// What SCRAM logins are checked against besides roles' and agents' own verifiers: the server's and
/// admin tokens, and a secret drawn when the server starts that each name's mock salt is made from
/// (`scram::mock_salt`, as Postgres's mock authentication makes it). With a server token, a name
/// that is no role or agent proves it knows a token against verifiers made for that name with its
/// mock salt (one proof is checked against both), and a login that can't succeed is checked against
/// a mock verifier with the same salt: every name gets a salt of its own, the same each time, and
/// the exchange doesn't tell which names exist.
///
/// A name's token verifiers cost a PBKDF2 per token, so they are made only at the last step, once
/// the client has sent its proof, a few at a time (`permits`), and never kept. And every SCRAM login
/// under a server token takes that step in the same time, `pad` at least, whether it makes them or
/// not: the time a login takes tells neither which names are roles or agents nor which logged in lately.
struct Tokens {
    admin: Option<String>,
    server: Option<String>,
    secret: [u8; 32],
    /// how long the last step of a SCRAM login takes at least, with a server token: twice what
    /// making a name's token verifiers took when the server started
    pad: std::time::Duration,
    /// token verifiers being made now, at most `permits` at once
    making: Mutex<usize>,
    made: std::sync::Condvar,
    permits: usize,
    /// how many times token verifiers were made (for the tests)
    makes: std::sync::atomic::AtomicUsize,
    /// the most made at once so far (for the tests)
    most: std::sync::atomic::AtomicUsize,
}

impl Tokens {
    fn new(auth: &crate::Auth) -> io::Result<Tokens> {
        let mut secret = [0u8; 32];
        SystemRandom::new().fill(&mut secret).map_err(|_| io::Error::other("no random numbers for a SCRAM secret"))?;
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let mut t = Tokens {
            admin: auth.admin.clone(),
            server: auth.token.clone(),
            secret,
            pad: std::time::Duration::ZERO,
            making: Mutex::new(0),
            made: std::sync::Condvar::new(),
            permits: (cores / 2).clamp(1, 8),
            makes: Default::default(),
            most: Default::default(),
        };
        if t.server.is_some() {
            let began = Instant::now();
            t.derive("");
            t.pad = began.elapsed() * 2;
        }
        Ok(t)
    }

    /// The tokens' verifiers for a login as `name`, with its mock salt (the admin token's first):
    /// made now, while fewer than `permits` others are being made.
    fn derive(&self, name: &str) -> Vec<Verifier> {
        struct Permit<'a>(&'a Tokens);
        impl Drop for Permit<'_> {
            fn drop(&mut self) {
                *self.0.making.lock().unwrap_or_else(|p| p.into_inner()) -= 1;
                self.0.made.notify_one();
            }
        }
        let mut making = self.making.lock().unwrap_or_else(|p| p.into_inner());
        while *making >= self.permits {
            making = self.made.wait(making).unwrap_or_else(|p| p.into_inner());
        }
        *making += 1;
        self.most.fetch_max(*making, Ordering::Relaxed);
        drop(making);
        let _permit = Permit(self);
        self.makes.fetch_add(1, Ordering::Relaxed);
        let salt = scram::mock_salt(name, &self.secret);
        let made = |t: &str| scram::parse(&scram::verifier(t, &salt, scram::ITERATIONS));
        self.admin.iter().chain(&self.server).filter_map(|t| made(t)).collect()
    }

    /// The last step of a SCRAM login as `name` with a server token, once the client has sent its
    /// proof: the tokens' verifiers for it if `tokens` (it logs in with a token), made now, and in
    /// any case `pad` at least since the step began, so that it takes the same time for every name.
    fn last_step(&self, name: &str, tokens: bool) -> Option<Vec<Verifier>> {
        let began = Instant::now();
        let made = tokens.then(|| self.derive(name));
        std::thread::sleep(self.pad.saturating_sub(began.elapsed()));
        made
    }

    /// A verifier no password matches, with `name`'s mock salt: what a login that can't succeed is
    /// checked against, so that it looks like a wrong password (Postgres's mock authentication).
    fn mock(&self, name: &str) -> Verifier {
        let salt = scram::mock_salt(name, &self.secret);
        Verifier { iterations: scram::ITERATIONS, salt, stored_key: vec![0; 32], server_key: vec![0; 32] }
    }
}

/// How a client logging in proves who it is (see `Conn::startup`).
enum Proof {
    /// nothing: loopback without a server token
    Trust,
    /// a password in the clear: the token of an agent made before agents had SCRAM verifiers, or
    /// with only an admin token, anything
    Clear,
    /// Postgres's MD5 challenge, for a role whose password is this md5 hash
    Md5(String),
    /// SCRAM-SHA-256 against these verifiers (one salt); `doomed`: refused however it ends;
    /// `tokens`: against the tokens' verifiers for the name instead, made at the last step (the
    /// first is the admin token's when there is one)
    Scram { secrets: Vec<Verifier>, doomed: bool, tokens: bool },
}

pub fn serve(db: &Db, listen: &str, auth: &crate::Auth, tls: Option<crate::tls::Tls>) -> io::Result<()> {
    let l = TcpListener::bind(listen).map_err(|e| io::Error::other(format!("postgres listen on {listen}: {e}")))?;
    let tokens = &Tokens::new(auth)?;
    let addr = l.local_addr()?; // the port the kernel picked, with --pg 127.0.0.1:0
    eprintln!("chronos: postgres protocol on {addr} (psql postgres://{addr}/main)");
    let refuse = |c| too_many(c, tls.clone());
    crate::http::connections(&l, refuse, |c| {
        let _ = c.set_nodelay(true);
        // until it has logged in, a client gets a minute, or CHRONOS_LOGIN_TIMEOUT s (see also STARTUP_MESSAGE)
        let _ = c.set_read_timeout(Some(*STARTUP_TIME).filter(|t| !t.is_zero()));
        // one descriptor each for the reader and TLS; the rest (peer_gone, the registry, waiting
        // for NOTIFY) share one: a connection holds about six, so ulimit -n bounds how many fit
        let (Ok(r), Ok(tcp), Ok(sock), Ok(login)) = (c.try_clone(), c.try_clone(), c.try_clone(), c.try_clone()) else {
            return;
        };
        let sock = Arc::new(sock);
        let login = Arc::new(login);
        login_deadline(&login); // and the whole login gets that long too
        let mut session = Session::new(chronos::MAIN);
        session.remote = true; // a client of the server: main's protection stays on for it
        session.gone = Some(Arc::new(peer_gone(sock.clone())));
        let wake = wake_pipe().map(|(r, w)| {
            session.inbox.on_notify(move || {
                // SAFETY: one byte into a pipe the closure owns (non-blocking: a full pipe
                // already wakes the reader)
                unsafe { libc::write(w.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
            });
            r
        });
        let mut conn = Conn {
            db,
            r: BufReader::new(Box::new(r)),
            w: BufWriter::new(Box::new(c)),
            tls: tls.clone().map(|t| (t, tcp)),
            binding: None,
            session,
            portals: HashMap::new(),
            key: None,
            info: None,
            sock,
            wake,
            tls_stream: None,
            zone: "UTC".into(),
            at_ready: false,
        };
        // the connection is closed and its session cleaned up even if serving it panics
        crate::http::guarded(|| {
            let ok = matches!(conn.startup(auth, tokens), Ok(true));
            drop(login); // off the login deadline's list
            if ok && conn.sock.set_read_timeout(None).is_ok() {
                let _ = conn.serve();
            }
        });
        if let Some(id) = conn.key {
            KEYS.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
            sql::forget_backend(id);
        }
        conn.session.end(db); // a transaction left open is rolled back, and its advisory locks go
    });
    Ok(())
}

/// Postgres's refusal of a login that didn't prove its password.
fn failed(user: &str) -> SqlError {
    SqlError { code: "28P01", message: format!("password authentication failed for user \"{user}\""), info: None }
}

/// A client past `limits::max_connections()` (CHRONOS_MAX_CONNECTIONS, or what the descriptors hold): refused once it
/// has sent its startup message, as Postgres's ProcessStartupPacket refuses it (FATAL 53300), its SSLRequest or
/// GSSENCRequest answered first as any connection's is, so a client that asked for TLS hears why, and a cancel request
/// served. A few at a time, each on a thread of its own with a short deadline (the accept loop never waits on one);
/// past that many the refusal goes out at once, before the client says anything.
fn too_many(c: TcpStream, tls: Option<crate::tls::Tls>) {
    static REFUSING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    const AT_ONCE: usize = 16;
    if REFUSING.fetch_add(1, Ordering::SeqCst) >= AT_ONCE {
        REFUSING.fetch_sub(1, Ordering::SeqCst);
        let mut c = c;
        let _ = c.write_all(&too_many_message());
        return;
    }
    let Ok(early) = c.try_clone() else {
        REFUSING.fetch_sub(1, Ordering::SeqCst);
        return;
    };
    let spawned = std::thread::Builder::new().stack_size(256 << 10).spawn(move || {
        let _ = refuse_after_startup(c, tls);
        REFUSING.fetch_sub(1, Ordering::SeqCst);
    });
    if spawned.is_err() {
        REFUSING.fetch_sub(1, Ordering::SeqCst);
        let mut early = early;
        let _ = early.write_all(&too_many_message());
    }
}

/// Postgres's ErrorResponse for a client past max_connections.
fn too_many_message() -> Vec<u8> {
    let mut m = vec![];
    for (k, v) in [(b'S', "FATAL"), (b'V', "FATAL"), (b'C', "53300"), (b'M', "sorry, too many clients already")] {
        m.push(k);
        m.extend_from_slice(v.as_bytes());
        m.push(0);
    }
    m.push(0);
    let mut msg = vec![b'E'];
    msg.extend_from_slice(&(m.len() as i32 + 4).to_be_bytes());
    msg.extend_from_slice(&m);
    msg
}

/// A stream to read and write: the socket, or TLS over it.
trait Duplex: Read + Write {}
impl<T: Read + Write> Duplex for T {}

/// See `too_many`: the client's packets up to its startup message, then the refusal.
fn refuse_after_startup(c: TcpStream, mut tls: Option<crate::tls::Tls>) -> io::Result<()> {
    let wait = std::time::Duration::from_secs(10);
    c.set_read_timeout(Some(wait))?;
    c.set_write_timeout(Some(wait))?;
    let tcp = c.try_clone()?;
    let mut s: Box<dyn Duplex> = Box::new(c);
    for _ in 0..3 {
        let body = body(&mut s, 8, STARTUP_MESSAGE)?;
        match i32::from_be_bytes(body[..4].try_into().unwrap()) {
            80877103 if tls.is_some() => {
                s.write_all(b"S")?;
                s.flush()?;
                let t = tls.take().unwrap();
                let conn = ServerConnection::new(t.cfg).map_err(io::Error::other)?;
                let tcp = tcp.try_clone()?;
                s = Box::new(StreamOwned::new(conn, tcp));
            }
            80877103 | 80877104 => {
                s.write_all(b"N")?;
                s.flush()?;
            }
            80877102 => {
                cancel_request(&body);
                return Ok(());
            }
            196608 => {
                s.write_all(&too_many_message())?;
                return s.flush();
            }
            _ => return Ok(()),
        }
    }
    Ok(())
}

/// A cancel request (its body: the code, then the backend's id and secret): stops the statement running on the
/// connection it names, when the secret is that connection's.
fn cancel_request(body: &[u8]) {
    if let (Some(id), Some(secret)) = (body.get(4..8), body.get(8..12)) {
        let id = i32::from_be_bytes(id.try_into().unwrap());
        let secret = i32::from_be_bytes(secret.try_into().unwrap());
        if let Some((s, stop)) = KEYS.lock().unwrap_or_else(|p| p.into_inner()).get(&id)
            && *s == secret
        {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

enum Fail {
    Sql(SqlError),
    Io(io::Error),
}

impl From<SqlError> for Fail {
    fn from(e: SqlError) -> Self {
        Fail::Sql(e)
    }
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Io(e)
    }
}

fn protocol(msg: impl Into<String>) -> Fail {
    Fail::Sql(SqlError { code: "08P01", message: msg.into(), info: None })
}

/// `n` bytes, grown as they arrive: a length alone doesn't get its size allocated.
fn read_n(r: &mut impl Read, n: usize) -> io::Result<Vec<u8>> {
    if n <= 64 << 10 {
        let mut b = vec![0; n]; // most messages: small, read in one go
        r.read_exact(&mut b)?;
        return Ok(b);
    }
    let mut b = vec![];
    if r.by_ref().take(n as u64).read_to_end(&mut b)? < n {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(b)
}

/// A length-prefixed message body (the length counts itself), of at most `max` bytes.
fn body(r: &mut impl Read, min: usize, max: usize) -> io::Result<Vec<u8>> {
    let n = i32::from_be_bytes(read_n(r, 4)?.try_into().unwrap());
    match usize::try_from(n) {
        Ok(n) if (min..=max).contains(&n) => read_n(r, n - 4),
        _ => Err(io::Error::other("bad message length")),
    }
}

/// What `message` found.
enum Msg {
    Body(Vec<u8>),
    /// A message of this many bytes (its length word, counting itself), past what is taken: its
    /// body was read and dropped, so the connection is at the next message.
    Big(usize),
    /// A length no message has (Postgres: FATAL 08P01 `invalid message length`): the stream is lost.
    Invalid,
}

/// Postgres's longest message (`PQ_LARGE_MESSAGE_LIMIT`, a 1 GB allocation less one).
const MAX_POSSIBLE: usize = 0x3fff_fffe;

/// The next message's body, of `min..=max` bytes counting its length word; one longer than `max`
/// but within what Postgres accepts is read and dropped (the client may be sent an error for it
/// and go on), not left in the stream.
fn message(r: &mut impl Read, min: usize, max: usize) -> io::Result<Msg> {
    let n = i32::from_be_bytes(read_n(r, 4)?.try_into().unwrap());
    match usize::try_from(n) {
        Ok(n) if (min..=max).contains(&n) => Ok(Msg::Body(read_n(r, n - 4)?)),
        Ok(n) if n > max && n <= MAX_POSSIBLE => {
            let want = (n - 4) as u64;
            if io::copy(&mut r.by_ref().take(want), &mut io::sink())? < want {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            Ok(Msg::Big(n))
        }
        _ => Ok(Msg::Invalid),
    }
}

/// Reads from `r` until `until`: a read from the socket that would go past it fails (TimedOut or
/// WouldBlock), the socket's read timeout being set to what is left before each one. What `r`
/// has buffered is read at once.
struct Until<'a> {
    r: &'a mut BufReader<Box<dyn Read>>,
    sock: &'a TcpStream,
    until: Instant,
}

impl Read for Until<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.r.buffer().is_empty() {
            let left = self.until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.sock.set_read_timeout(Some(left))?;
        }
        self.r.read(buf)
    }
}

/// The error for a message of `n` bytes past `MAX_MESSAGE` (Postgres takes up to 1 GB; this
/// server's own limit is lower, which the message says).
fn too_big(n: usize) -> SqlError {
    SqlError {
        code: "54000",
        message: format!(
            "message of {n} bytes is too large: this server accepts messages of at most {MAX_MESSAGE} bytes (64 MB); \
             send the data in smaller messages (COPY, or fewer rows per statement)"
        ),
        info: None,
    }
}

/// Reads a message body.
struct Cur<'a>(&'a [u8]);

impl Cur<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Fail> {
        if self.0.len() < n {
            return Err(protocol("message too short"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn i16(&mut self) -> Result<i16, Fail> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, Fail> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn str(&mut self) -> Result<String, Fail> {
        let end = self.0.iter().position(|&b| b == 0).ok_or_else(|| protocol("missing string terminator"))?;
        let s = String::from_utf8(self.0[..end].to_vec()).map_err(|_| protocol("strings must be UTF-8"))?;
        self.0 = &self.0[end + 1..];
        Ok(s)
    }
    fn i16s(&mut self) -> Result<Vec<i16>, Fail> {
        (0..self.i16()?).map(|_| self.i16()).collect()
    }
}

fn cstr(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(s.as_bytes());
    b.push(0);
}

/// The format (0 text, 1 binary) of value `i`: one code for all, or one each.
fn format(codes: &[i16], i: usize) -> i16 {
    match codes {
        [] => 0,
        [one] => *one,
        many => many.get(i).copied().unwrap_or(0),
    }
}

#[derive(Clone)]
struct Portal {
    p: Prepared,
    params: Vec<Value>,
    formats: Vec<i16>,
    /// what an Execute with a row limit left unsent (PortalSuspended): the result and how many of
    /// its rows the client has, for the next Execute to go on from
    rest: RefCell<Option<(Output, usize)>>,
    /// it has run to its end: Postgres won't run it again
    done: std::cell::Cell<bool>,
}

/// A TLS connection, read and written through two handles: the protocol takes turns on one thread.
struct Tls(Rc<RefCell<StreamOwned<ServerConnection, TcpStream>>>);

impl Read for Tls {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.borrow_mut().read(b)
    }
}

impl Write for Tls {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

struct Conn<'a> {
    db: &'a Db,
    r: BufReader<Box<dyn Read>>,
    w: BufWriter<Box<dyn Write>>,
    tls: Option<(crate::tls::Tls, TcpStream)>, // TLS is required, and not started yet
    binding: Option<Vec<u8>>,                  // in TLS: the certificate's hash a SCRAM-SHA-256-PLUS login binds to
    session: Session,                          // with its prepared statements
    portals: HashMap<String, Rc<Portal>>,      // shared, so each Execute doesn't copy its parameters
    key: Option<i32>,                          // its backend key's ID, while it's in KEYS
    info: Option<Arc<Mutex<sql::BackendInfo>>>, // what pg_stat_activity shows of it
    sock: Arc<TcpStream>,                      // the client's socket, to wait on (see `wait`)
    wake: Option<OwnedFd>,                     // readable when a NOTIFY reaches the session
    tls_stream: Option<Rc<RefCell<StreamOwned<ServerConnection, TcpStream>>>>,
    zone: String,   // the TimeZone the client was last told (ParameterStatus)
    at_ready: bool, // the last message sent was ReadyForQuery: the client is idle (see `idle_deadline`)
}

/// A non-blocking pipe: (read end, write end).
fn wake_pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe fills in two new descriptors, which we then own
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    for fd in fds {
        // SAFETY: descriptors we own
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
    }
    // SAFETY: fresh descriptors, owned by nothing else
    Some(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

impl Conn<'_> {
    fn send(&mut self, tag: u8, body: &[u8]) -> io::Result<()> {
        self.w.write_all(&[tag])?;
        self.w.write_all(&(body.len() as i32 + 4).to_be_bytes())?;
        self.w.write_all(body)
    }

    fn error(&mut self, e: &SqlError) -> io::Result<()> {
        self.report("ERROR", e)
    }

    /// Refuses a client logging in: the error as FATAL, as Postgres sends it; false (drop it).
    fn refuse(&mut self, e: &SqlError) -> io::Result<bool> {
        self.report("FATAL", e)?;
        self.w.flush()?;
        Ok(false)
    }

    fn report(&mut self, severity: &str, e: &SqlError) -> io::Result<()> {
        let mut b = vec![];
        for (k, v) in [(b'S', severity), (b'V', severity), (b'C', e.code), (b'M', e.message.as_str())] {
            b.push(k);
            cstr(&mut b, v);
        }
        // a constraint violation says what it broke, in the order the protocol lists the fields
        if let Some(i) = &e.info {
            let position = i.position.map(|p| p.to_string());
            let fields = [
                (b'D', &i.detail),
                (b'H', &i.hint),
                (b'P', &position),
                (b'W', &i.context),
                (b's', &i.schema),
                (b't', &i.table),
                (b'c', &i.column),
                (b'n', &i.constraint),
            ];
            for (k, v) in fields {
                if let Some(v) = v {
                    b.push(k);
                    cstr(&mut b, v);
                }
            }
        }
        b.push(0);
        self.send(b'E', &b)
    }

    /// NoticeResponse for each notice the last statement sent (RAISE NOTICE, ...).
    fn notices(&mut self) -> io::Result<()> {
        self.send_notices(sql::take_notices())
    }

    fn send_notices(&mut self, notices: Vec<sql::Notice>) -> io::Result<()> {
        for n in notices {
            if !self.session.sends(n.level) {
                continue; // SET client_min_messages = warning, as pg_dump's scripts do
            }
            let mut b = vec![];
            for (k, v) in [(b'S', n.level), (b'V', n.level), (b'C', n.code), (b'M', n.message.as_str())] {
                b.push(k);
                cstr(&mut b, v);
            }
            if let Some(d) = &n.detail {
                b.push(b'D');
                cstr(&mut b, d);
            }
            if let Some(h) = &n.hint {
                b.push(b'H');
                cstr(&mut b, h);
            }
            b.push(0);
            self.send(b'N', &b)?;
        }
        Ok(())
    }

    /// Keeps what pg_stat_activity shows of this connection: it is running `query`, or (None) it is
    /// waiting for the client, in a transaction or not.
    fn activity(&mut self, query: Option<&str>) {
        let Some(info) = &self.info else { return };
        let (now, status) = (micros(), self.session.status());
        let mut i = info.lock().unwrap_or_else(|p| p.into_inner());
        (i.world, i.app) = (self.session.branch.clone(), self.session.var("application_name").unwrap_or_default());
        match query {
            Some(q) => {
                // (Postgres keeps the first 1024 bytes of it)
                let end = (0..=q.len().min(1024)).rev().find(|n| q.is_char_boundary(*n)).unwrap_or(0);
                (i.state, i.query, i.query_start, i.state_change) = ("active", q[..end].to_string(), Some(now), now);
                i.xact_start.get_or_insert(now);
            }
            None if i.state != "active" => {}
            None => {
                let (state, in_tx) = match status {
                    b'T' => ("idle in transaction", true),
                    b'E' => ("idle in transaction (aborted)", true),
                    _ => ("idle", false),
                };
                (i.state, i.state_change) = (state, now);
                if !in_tx {
                    i.xact_start = None;
                }
            }
        }
    }

    fn ready(&mut self) -> io::Result<()> {
        self.activity(None);
        // a SET TIME ZONE tells the client, as Postgres does
        let zone = self.session.zone.as_ref().map_or("UTC", |z| z.name.as_str());
        if zone != self.zone {
            self.zone = zone.to_string();
            let mut b = vec![];
            cstr(&mut b, "TimeZone");
            cstr(&mut b, &self.zone);
            self.send(b'S', &b)?;
        }
        let status = self.session.status();
        if status == b'I' {
            self.notifications()?;
        }
        self.send(b'Z', &[status])?;
        self.at_ready = true;
        self.w.flush()
    }

    /// When the client's idleness runs out, if the session has a limit on it (see
    /// `Session::idle_limits`): from the ReadyForQuery just sent, as Postgres starts the timers
    /// there (not between the messages of one pipeline). And the FATAL error that ends it then.
    fn idle_deadline(&self) -> Option<(Instant, SqlError)> {
        if !self.at_ready {
            return None;
        }
        let (mut in_tx, out_of_tx) = self.session.idle_limits();
        // an agent's max_idle_ms caps how long its transaction may wait on it (its model thinking): past it, the
        // connection goes, the transaction is rolled back and its locks let go
        let cap = self.session.agent.as_deref().and_then(|a| self.db.agent(a).ok()).map_or(0, |a| a.max_idle_ms);
        if cap > 0 && (in_tx == 0 || in_tx > cap) {
            in_tx = cap;
        }
        let (ms, code, what) = match self.session.status() {
            b'I' => (out_of_tx, "57P05", "idle-session"),
            _ => (in_tx, "25P03", "idle-in-transaction"),
        };
        (ms > 0).then(|| {
            let e = SqlError { code, message: format!("terminating connection due to {what} timeout"), info: None };
            (Instant::now() + std::time::Duration::from_millis(ms), e)
        })
    }

    /// The NOTIFYs that reached this session, as NotificationResponse messages.
    fn notifications(&mut self) -> io::Result<()> {
        for n in self.session.inbox.take() {
            let mut b = n.pid.to_be_bytes().to_vec();
            cstr(&mut b, &n.channel);
            cstr(&mut b, &n.payload);
            self.send(b'A', &b)?;
        }
        Ok(())
    }

    /// Waits for the client's next message. While the session LISTENs outside a transaction,
    /// notifications that arrive meanwhile go out at once, as Postgres sends them to idle clients.
    /// With a `deadline`, a wait that reaches it is an error of kind TimedOut.
    fn wait(&mut self, deadline: Option<Instant>) -> io::Result<()> {
        let Some(wake) = self.wake.as_ref().map(|w| w.as_raw_fd()) else { return Ok(()) };
        loop {
            // TLS may hold a message it has decrypted: then there's no waiting to do
            let decrypted = || {
                self.tls_stream.as_ref().is_some_and(|s| {
                    s.borrow_mut().conn.process_new_packets().map_or(true, |st| st.plaintext_bytes_to_read() > 0)
                })
            };
            if !self.r.buffer().is_empty() || !self.session.listening() || self.session.status() != b'I' || decrypted()
            {
                return Ok(());
            }
            let mut fds = [
                libc::pollfd { fd: self.sock.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: wake, events: libc::POLLIN, revents: 0 },
            ];
            let ms = deadline.map_or(-1, |d| {
                // (rounded up: not woken a moment early to wait again)
                i32::try_from(d.saturating_duration_since(Instant::now()).as_millis() + 1).unwrap_or(i32::MAX)
            });
            // SAFETY: two pollfds we own, for descriptors that stay open
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 && deadline.is_some_and(|d| d <= Instant::now()) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            if fds[1].revents != 0 {
                let mut b = [0u8; 64];
                // SAFETY: reads into a buffer we own, from our non-blocking pipe
                while unsafe { libc::read(wake, b.as_mut_ptr().cast(), b.len()) } > 0 {}
                self.notifications()?;
                self.w.flush()?;
            }
            if fds[0].revents != 0 {
                return Ok(());
            }
        }
    }

    fn body(&mut self, min: usize, max: usize) -> io::Result<Vec<u8>> {
        body(&mut self.r, min, max)
    }

    /// The startup handshake; false if the client should be dropped.
    fn startup(&mut self, auth: &crate::Auth, tokens: &Tokens) -> io::Result<bool> {
        let (token, admin_token) = (auth.token.as_deref(), auth.admin.as_deref());
        let mut asks = 0; // encryption requests: libpq sends GSSENCRequest then SSLRequest at most
        let body = loop {
            let body = self.body(8, STARTUP_MESSAGE)?;
            let code = i32::from_be_bytes(body[..4].try_into().unwrap());
            if matches!(code, 80877103 | 80877104) {
                asks += 1;
                if asks > 2 {
                    return Ok(false);
                }
            }
            match code {
                80877103 if self.tls.is_some() => {
                    self.w.write_all(b"S")?;
                    self.w.flush()?;
                    if !self.r.buffer().is_empty() {
                        return Ok(false); // sent before the handshake, so not protected by it
                    }
                    let (t, tcp) = self.tls.take().unwrap();
                    let conn = ServerConnection::new(t.cfg).map_err(io::Error::other)?;
                    self.binding = t.end_point;
                    let s = Rc::new(RefCell::new(StreamOwned::new(conn, tcp)));
                    self.tls_stream = Some(s.clone());
                    self.r = BufReader::new(Box::new(Tls(s.clone())));
                    self.w = BufWriter::new(Box::new(Tls(s)));
                }
                196608 if self.tls.is_some() => {
                    let e = SqlError {
                        code: "28000",
                        message: "this server requires TLS: connect with sslmode=require".into(),
                        info: None,
                    };
                    return self.refuse(&e);
                }
                80877103 | 80877104 => {
                    // TLS or GSS encryption: not offered, the client carries on in plain text
                    self.w.write_all(b"N")?;
                    self.w.flush()?;
                }
                196608 => break body,
                80877102 => {
                    cancel_request(&body); // stop the statement running on the connection it names
                    return Ok(false);
                }
                _ => return Ok(false), // old protocol versions
            }
        };
        let mut c = Cur(&body[4..]);
        let mut opts = HashMap::new();
        loop {
            let Ok(k) = c.str() else { return Ok(false) };
            if k.is_empty() {
                break;
            }
            let Ok(v) = c.str() else { return Ok(false) };
            opts.insert(k, v);
        }
        // How the client proves who it is, chosen from the user name before the client says
        // anything (a SCRAM verifier's salt goes first): an agent proves with SCRAM that it knows
        // its token, against its verifier (one made before agents had verifiers sends its token in
        // the clear, once: see `Core::upgrade_agent`); a role with a password proves it knows it with
        // SCRAM, or MD5 when it is an md5 hash, on loopback too; any other name proves with SCRAM
        // that it knows the admin or the server's token. A role without a password where the server
        // has a token goes through SCRAM doomed, as Postgres does: 28P01. Without a server token
        // (loopback only) other names need no password, and with only an admin token any cleartext
        // password goes, the admin token alone making the client the database's own user (see
        // `crate::Mode`). An agent with a role of its name logs in
        // as the agent (the role lends it its privileges, see `roles::who`, not its password).
        let user = opts.get("user").cloned().unwrap_or_default();
        let agent = if user == chronos::GUEST { None } else { self.db.agent(&user).ok() };
        let is_agent = agent.is_some();
        // (both looked up for every name, so the time it takes says less of which it is)
        let kept = match Session::role_password(self.db, &user) {
            Ok(k) => k.filter(|_| !is_agent),
            Err(e) => return self.refuse(&e),
        };
        let mock = tokens.mock(&user);
        let doomed = || Proof::Scram { secrets: vec![mock.clone()], doomed: true, tokens: false };
        let expired = !is_agent && matches!(kept, Some((_, true)));
        // a role that proves its own password is that role, with the privileges it was given, never the guest: its
        // password is a credential the database's own user made on purpose (an agent, or a name that proves a token,
        // is what the guest stands in for)
        let role_login = !is_agent && matches!(&kept, Some((Some(_), false)));
        let proof = match (&agent, kept) {
            // a disabled agent's exchange is doomed, as a wrong token's
            (Some(a), _) => match a.verifier() {
                Some(v) => Proof::Scram { secrets: vec![v], doomed: a.disabled, tokens: false },
                // one made before agents had verifiers, where every other name gets SCRAM (a server token): SCRAM
                // too, which it can't pass (its token is kept only hashed), so the exchange doesn't tell an agent
                // from any other name; its first use of its token over the HTTP API makes it a verifier (see
                // `api::admit`). Elsewhere (loopback) it sends its token in the clear once, as before.
                None if token.is_some() => doomed(),
                None => Proof::Clear,
            },
            // a password past its VALID UNTIL is checked against nothing, as Postgres does, so that
            // the exchange doesn't tell whether it was right (and never MD5)
            (_, Some((Some(_), true))) => doomed(),
            (_, Some((Some(p), false))) if scram::is_md5(&p) => Proof::Md5(p),
            (_, Some((Some(p), false))) => match scram::parse(&p) {
                Some(v) => Proof::Scram { secrets: vec![v], doomed: false, tokens: false },
                None => doomed(), // a password no login can use
            },
            (_, Some((None, _))) if token.is_some() => doomed(),
            // (the mock verifier gives the salt and iteration count; the tokens' come at the last step)
            _ if token.is_some() => Proof::Scram { secrets: vec![mock.clone()], doomed: false, tokens: true },
            _ if admin_token.is_some() => Proof::Clear,
            _ => Proof::Trust,
        };
        let mut admin = false;
        // an address that failed to log in too often waits before its next try (see `backoff`), told at once
        let peer = self.sock.peer_addr().ok().map(|a| a.ip());
        if !matches!(proof, Proof::Trust)
            && let Some(left) = crate::backoff::wait(peer)
        {
            let message = format!(
                "too many failed logins from this address: try again in {} s",
                left.as_secs_f64().ceil() as u64
            );
            return self.refuse(&SqlError { code: "28P01", message, info: None });
        }
        match proof {
            Proof::Trust => {}
            Proof::Md5(hash) => match self.md5(&user, &hash) {
                Ok(()) => {}
                Err(Fail::Sql(e)) => {
                    crate::backoff::failed(peer);
                    return self.refuse(&e);
                }
                Err(Fail::Io(e)) => return Err(e),
            },
            Proof::Scram { secrets, doomed, tokens: of_tokens } => {
                // with a server token, every SCRAM login's last step takes the same time
                let last_step = token.is_some().then_some((tokens, of_tokens));
                match self.sasl(&user, secrets, doomed, last_step) {
                    // an agent's proof matched its own verifier: the session acts as it
                    Ok(_) if is_agent => self.session.agent = Some(user.clone()),
                    Ok(i) => admin = of_tokens && admin_token.is_some() && i == 0,
                    Err(Fail::Sql(e)) => {
                        crate::backoff::failed(peer);
                        return self.refuse(&e);
                    }
                    Err(Fail::Io(e)) => return Err(e),
                }
            }
            Proof::Clear => {
                // the server's token is the database's own user's password; an agent (the user
                // name) logs in with its own token only and gets a session that acts as it (see
                // `chronos::agents`)
                self.send(b'R', &3i32.to_be_bytes())?; // cleartext password
                self.w.flush()?;
                let mut tag = [0];
                self.r.read_exact(&mut tag)?;
                let body = self.body(4, PASSWORD_MESSAGE)?;
                let given = body.split(|&b| b == 0).next().unwrap_or_default();
                let given = std::str::from_utf8(given).unwrap_or_default();
                let p = tag[0] == b'p';
                admin = p && !is_agent && admin_token.is_some_and(|t| crate::same_secret(given, t));
                let server = p && !is_agent && token.is_some_and(|t| crate::same_secret(given, t));
                let agent = p && is_agent && self.db.agent_by_token(given).is_ok_and(|a| a.name == user);
                // without a server token (loopback only), a password that's neither is no password
                if !admin && !server && !agent && (token.is_some() || is_agent) {
                    crate::backoff::failed(peer);
                    let e = SqlError {
                        code: "28P01",
                        message: "wrong password: use the server's token, or the agent's".into(),
                        info: None,
                    };
                    return self.refuse(&e);
                }
                if agent {
                    self.session.agent = Some(user.clone());
                    // its token, at last: keep a SCRAM verifier of it, so it never sends it in the
                    // clear again (a database that can't be written now just asks again next time)
                    let _ = self.db.upgrade_agent(&user, given);
                }
            }
        }
        // a role past its VALID UNTIL asked for no password (loopback without a token) is refused too
        if expired {
            return self.refuse(&failed(&user));
        }
        // authenticated: as in Postgres, what is checked from here on (NOLOGIN, the database, CONNECT,
        // the connection limit, the time zone) is refused after AuthenticationOk
        self.send(b'R', &0i32.to_be_bytes())?;
        // who reads what, for a change's observed readers: the role, and the program if it says
        let role = if user.is_empty() { "postgres" } else { user.as_str() };
        if !user.is_empty() {
            self.session.user = Some(user.clone()); // current_user, session_user
        }
        // a role made with CREATE ROLE logs in with its own rights (a name that is no role is a
        // superuser, as every login was before roles); one that may not log in now is refused
        if !user.is_empty()
            && !is_agent
            && let Err(e) = self.session.log_in(self.db, &user)
        {
            return self.refuse(&e);
        }
        if let Some(app) = opts.get("application_name") {
            self.session.set_var("application_name", app);
        }
        self.session.client = Some(match opts.get("application_name").filter(|a| !a.is_empty() && *a != role) {
            Some(app) => format!("{role} ({app})"),
            None => role.to_string(),
        });
        if self.session.agent.is_none() && !role_login && auth.guest(admin) {
            self.session.agent = Some(chronos::GUEST.into()); // the guest's rights (see `crate::Mode`)
        }
        // psql connects to a database named after the user unless told otherwise: that means main
        self.session.branch = match opts.get("database").cloned().unwrap_or_default() {
            d if d.is_empty() || d == user => chronos::MAIN.into(),
            // a database made with CREATE DATABASE is a world; `postgres` and `template1`, where tools
            // connect to make it, are main unless a world is named so
            d => sql::database_world(self.db, &d).unwrap_or(d),
        };
        self.session.login = self.session.branch.clone(); // where DISCARD ALL puts it back
        // a world, or a world as it was (`main@2026-09-20 10:00`, read only)
        // (a session's transaction world is that session's own: never one to connect to)
        let world = self.session.branch.split_once('@').map_or(self.session.branch.as_str(), |(w, _)| w);
        if self.db.view(&self.session.branch).is_err() || self.db.private_world(world) {
            let message = format!("no branch \"{}\": connect to main or an existing branch", self.session.branch);
            return self.refuse(&SqlError { code: "3D000", message, info: None });
        }
        // a role may connect to it (CONNECT), and isn't past its CONNECTION LIMIT
        if let Err(e) = self.session.may_connect(self.db) {
            return self.refuse(&e);
        }
        // the database's ALTER DATABASE ... SET settings, now that it is known (under the role's, which `log_in` took)
        if !is_agent && let Err(e) = self.session.take_database_settings(self.db) {
            return self.refuse(&e);
        }
        // TimeZone, as a startup parameter or in options (`-c TimeZone=Asia/Kolkata`), as Postgres
        // takes it: a zone this can't honour refuses the connection rather than serve it in UTC
        let options = opts.get("options").map_or(vec![], |o| {
            let mut kv = vec![];
            let mut words = o.split_whitespace();
            while let Some(w) = words.next() {
                let w = if w == "-c" { words.next().unwrap_or_default() } else { w };
                let w = w.strip_prefix("--").or_else(|| w.strip_prefix("-c")).unwrap_or(w);
                if let Some((k, v)) = w.split_once('=') {
                    kv.push((k.to_string(), v.to_string()));
                }
            }
            kv
        });
        let asked: Vec<(String, String)> = opts.iter().map(|(k, v)| (k.clone(), v.clone())).chain(options).collect();
        if let Some((_, ds)) = asked.iter().rfind(|(k, _)| k.eq_ignore_ascii_case("datestyle")) {
            let q = format!("set datestyle = '{}'", ds.replace('\'', "''"));
            if let Err(e) = self.session.run(self.db, &q, &[]) {
                return self.refuse(&e);
            }
            self.session.keep_as_startup("datestyle");
        }
        if let Some((_, is)) = asked.iter().rfind(|(k, _)| k.eq_ignore_ascii_case("intervalstyle")) {
            let q = format!("set intervalstyle = '{}'", is.replace('\'', "''"));
            if let Err(e) = self.session.run(self.db, &q, &[]) {
                return self.refuse(&e);
            }
            self.session.keep_as_startup("intervalstyle");
        }
        if let Some((_, z)) = asked.into_iter().rfind(|(k, _)| k.eq_ignore_ascii_case("timezone")) {
            match chronos::sql::zone_named(&z) {
                Ok(z) => self.session.zone = Some(z),
                Err(message) => return self.refuse(&SqlError { code: "22023", message, info: None }),
            }
        }
        self.session.keep_zone_as_startup();
        self.zone = self.session.zone.as_ref().map_or("UTC".into(), |z| z.name.clone());
        let zone = self.zone.clone();
        for (k, v) in [
            ("server_version", chronos::sql::SERVER_VERSION),
            ("server_encoding", "UTF8"),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("TimeZone", zone.as_str()),
            ("integer_datetimes", "on"),
            ("standard_conforming_strings", "on"),
        ] {
            let mut b = vec![];
            cstr(&mut b, k);
            cstr(&mut b, v);
            self.send(b'S', &b)?;
        }
        let (id, secret) = backend_key()?;
        self.session.pid = id;
        KEYS.lock().unwrap_or_else(|p| p.into_inner()).insert(id, (secret, self.session.cancel.clone()));
        // pg_cancel_backend(id), pg_terminate_backend(id) and pg_stat_activity
        let now = micros();
        let info = sql::BackendInfo {
            user: if user.is_empty() { "postgres".into() } else { user.clone() },
            role: self.session.login_role(),
            world: self.session.branch.clone(),
            app: opts.get("application_name").cloned().unwrap_or_default(),
            addr: self.sock.peer_addr().ok().map(|a| (a.ip().to_string(), i32::from(a.port()))),
            started: now,
            state: "idle",
            state_change: now,
            ..Default::default()
        };
        self.info = Some(sql::register_backend(id, &self.session, Some(self.sock.clone()), info));
        sql::count_connection(self.db, &self.session.branch);
        self.key = Some(id);
        self.send(b'K', &[id.to_be_bytes(), secret.to_be_bytes()].concat())?;
        self.ready()?;
        Ok(true)
    }

    /// The client's next message while it logs in as `user`, which must be its password or SASL
    /// response. A password message too long (or short) to be one closes the connection; a SASL
    /// message is 1 KB at most, and one that isn't is 28P01, as in Postgres.
    fn auth_message(&mut self, sasl: bool, user: &str) -> Result<Vec<u8>, Fail> {
        let mut tag = [0];
        self.r.read_exact(&mut tag)?;
        if tag[0] != b'p' {
            let what = if sasl { "SASL" } else { "password" };
            return Err(protocol(format!("expected {what} response, got message type {}", tag[0])));
        }
        if !sasl {
            return Ok(self.body(4, PASSWORD_MESSAGE)?);
        }
        let n = i32::from_be_bytes(read_n(&mut self.r, 4)?.try_into().unwrap());
        match usize::try_from(n) {
            Ok(n) if (4..=SASL_MESSAGE).contains(&n) => Ok(read_n(&mut self.r, n - 4)?),
            _ => Err(failed(user).into()),
        }
    }

    /// A SCRAM-SHA-256 login (SASL; see `scram::Login`): which of `secrets` the client's proof matched.
    /// `last_step`, with a server token: its last step is `Tokens::last_step`, and with `true` the
    /// proof is checked against the tokens' verifiers it makes instead.
    fn sasl(
        &mut self,
        user: &str,
        secrets: Vec<Verifier>,
        doomed: bool,
        last_step: Option<(&Tokens, bool)>,
    ) -> Result<usize, Fail> {
        let mut b = 10i32.to_be_bytes().to_vec(); // AuthenticationSASL
        b.extend(scram::mechanisms(self.binding.is_some()));
        self.send(b'R', &b)?;
        self.w.flush()?;
        // SASLInitialResponse: the mechanism chosen, then the length of its first message (-1:
        // none, so an empty challenge asks for it) and the message
        let msg = self.auth_message(true, user)?;
        let mut c = Cur(&msg);
        let end = msg.iter().position(|&b| b == 0).ok_or_else(|| protocol("invalid string in message"))?;
        let mechanism = &c.take(end + 1)?[..end];
        let mut login = Login::start(mechanism, secrets, doomed, self.binding.as_deref())?;
        let short = || protocol("insufficient data left in message");
        let first = match c.i32().map_err(|_| short())? {
            -1 => {
                self.send(b'R', &11i32.to_be_bytes())?;
                self.w.flush()?;
                self.auth_message(true, user)?
            }
            n => {
                let n = usize::try_from(n).map_err(|_| short())?;
                let first = c.take(n).map_err(|_| short())?.to_vec();
                if !c.0.is_empty() {
                    return Err(protocol("invalid message format"));
                }
                first
            }
        };
        let server_first = login.first(&first)?;
        self.send(b'R', &[&11i32.to_be_bytes()[..], &server_first].concat())?; // AuthenticationSASLContinue
        self.w.flush()?;
        let last = self.auth_message(true, user)?;
        if let Some((tokens, of_tokens)) = last_step
            && let Some(made) = tokens.last_step(user, of_tokens)
        {
            login.secrets(made);
        }
        let (i, server_final) = login.last(&last, user)?;
        self.send(b'R', &[&12i32.to_be_bytes()[..], &server_final].concat())?; // AuthenticationSASLFinal
        Ok(i)
    }

    /// Postgres's MD5 challenge, for a role whose password is an md5 hash: a random 4-byte salt,
    /// answered with `md5` and the hex md5 of the hash and the salt.
    fn md5(&mut self, user: &str, hash: &str) -> Result<(), Fail> {
        let mut salt = [0u8; 4];
        SystemRandom::new().fill(&mut salt).map_err(|_| io::Error::other("no random numbers for an MD5 salt"))?;
        self.send(b'R', &[&5i32.to_be_bytes()[..], &salt].concat())?; // AuthenticationMD5Password
        self.w.flush()?;
        let msg = self.auth_message(false, user)?;
        // a C string
        let Some((0, given)) = msg.split_last().map(|(z, g)| (*z, g)).filter(|(_, g)| !g.contains(&0)) else {
            return Err(protocol("invalid password packet size"));
        };
        if given.is_empty() {
            return Err(
                SqlError { code: "28P01", message: "empty password returned by client".into(), info: None }.into()
            );
        }
        let want = scram::md5_salted(hash, &salt);
        if !std::str::from_utf8(given).is_ok_and(|g| crate::same_secret(g, &want)) {
            return Err(failed(user).into());
        }
        Ok(())
    }

    fn serve(&mut self) -> io::Result<()> {
        let mut failed = false; // after an error, the extended protocol skips to the next Sync
        // the last message was the Sync that ended that skip, or a CopyFail right after it (see b'f')
        let (mut synced, mut aborted) = (false, false);
        loop {
            // idle_in_transaction_session_timeout and idle_session_timeout: the client that sends
            // nothing for that long is told (FATAL) and dropped, its transaction rolled back
            let idle = self.idle_deadline();
            let deadline = idle.as_ref().map(|(d, _)| *d);
            let mut tag = [0];
            let got = match self.wait(deadline) {
                Ok(()) => {
                    // (a message already buffered is read at once, whatever the time)
                    let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                    let timed = left.is_some_and(|l| !l.is_zero() && self.r.buffer().is_empty());
                    if left.is_some_and(|l| l.is_zero()) && self.r.buffer().is_empty() {
                        Err(io::ErrorKind::TimedOut.into())
                    } else {
                        if timed {
                            let _ = self.sock.set_read_timeout(left);
                        }
                        let got = self.r.read_exact(&mut tag);
                        if timed {
                            let _ = self.sock.set_read_timeout(None);
                        }
                        got
                    }
                }
                Err(e) => Err(e),
            };
            if let Err(e) = got {
                let timeout = matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock);
                if let (true, Some((_, fatal))) = (timeout, idle) {
                    self.report("FATAL", &fatal)?;
                    self.w.flush()?;
                }
                return Ok(());
            }
            self.at_ready = false;
            // (the idle timers run until the whole message is in, as Postgres's ReadCommand: a
            // client that sends a tag and then stalls is dropped all the same)
            let read = match deadline {
                Some(until) => message(&mut Until { r: &mut self.r, sock: &self.sock, until }, 4, MAX_MESSAGE),
                None => message(&mut self.r, 4, MAX_MESSAGE),
            };
            if deadline.is_some() {
                let _ = self.sock.set_read_timeout(None);
            }
            let read = match (read, &idle) {
                (Err(e), Some((_, fatal)))
                    if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) =>
                {
                    self.report("FATAL", fatal)?;
                    self.w.flush()?;
                    return Ok(());
                }
                (read, _) => read?,
            };
            let body = match read {
                Msg::Body(b) => b,
                Msg::Invalid => {
                    let e = SqlError { code: "08P01", message: "invalid message length".into(), info: None };
                    self.report("FATAL", &e)?;
                    self.w.flush()?;
                    return Ok(());
                }
                // dropped unread, and answered as any failed message is: a simple query gets its
                // error and ReadyForQuery, an extended one skips to the next Sync (CopyData after a
                // failed COPY is dropped, as its small ones are)
                Msg::Big(n) => {
                    if failed || matches!(tag[0], b'd' | b'c' | b'f') {
                        continue;
                    }
                    self.error(&too_big(n))?;
                    self.session.abort();
                    if matches!(tag[0], b'Q' | b'F') {
                        self.ready()?;
                    } else {
                        self.w.flush()?;
                        failed = true;
                    }
                    continue;
                }
            };
            if failed && tag[0] != b'S' {
                continue;
            }
            let (after_sync, after_abort) = (std::mem::take(&mut synced), std::mem::take(&mut aborted));
            let mut c = Cur(&body);
            let r = match tag[0] {
                b'Q' => {
                    match c.str() {
                        Ok(sql) => self.simple(&sql)?,
                        Err(_) => {
                            self.error(&SqlError { code: "08P01", message: "bad query message".into(), info: None })?
                        }
                    }
                    self.ready()?;
                    continue;
                }
                b'S' => {
                    synced = failed;
                    failed = false;
                    self.portals.remove("");
                    // outside a transaction block the Sync ends the implicit one, and with it a portal
                    // left part-read (inside one, it can be read on)
                    if self.session.status() == b'I' {
                        self.portals.retain(|_, p| p.rest.borrow().is_none());
                    }
                    if !after_abort {
                        self.ready()?;
                    }
                    continue;
                }
                b'X' => return Ok(()),
                // a COPY FROM STDIN that failed before its CopyInResponse (an unknown column, say): the
                // postgres crate sent it as Bind, Execute, Sync, and ends it with CopyFail + Sync after
                // the error, counting one ReadyForQuery for all of it, the first Sync's. A second one
                // (Postgres answers every Sync) would answer its next statement in that one's place,
                // so the Sync after this CopyFail gets none
                b'f' if after_sync => {
                    aborted = true;
                    continue;
                }
                // the rest of a COPY's data after it failed: dropped, as Postgres drops it
                b'd' | b'c' | b'f' => continue,
                b'H' => {
                    self.w.flush()?;
                    continue;
                }
                b'F' => {
                    match self.fastpath(&mut c) {
                        Ok(()) => {}
                        Err(Fail::Io(e)) => return Err(e),
                        Err(Fail::Sql(e)) => {
                            self.error(&e)?;
                            self.session.abort();
                        }
                    }
                    self.ready()?;
                    continue;
                }
                b'P' => self.parse(&mut c),
                b'B' => self.bind(&mut c),
                b'D' => self.describe(&mut c),
                b'E' => self.execute(&mut c),
                b'C' => self.close(&mut c),
                t => Err(protocol(format!("unsupported message type '{}'", t as char))),
            };
            match r {
                Ok(()) => {}
                Err(Fail::Io(e)) => return Err(e),
                Err(Fail::Sql(e)) => {
                    self.error(&e)?;
                    // (at once: a client that sent Flush, as asyncpg does after a Parse, waits for it, and
                    // the messages up to the next Sync are skipped, its Flush too)
                    self.w.flush()?;
                    self.session.abort();
                    failed = true;
                }
            }
        }
    }

    /// A FunctionCall message (the "fast path" libpq's lo_* functions use): the large-object function with this oid,
    /// run as a query, and its result as a FunctionCallResponse.
    fn fastpath(&mut self, c: &mut Cur) -> Result<(), Fail> {
        let oid = c.i32()?;
        let formats = c.i16s()?;
        let n = c.i16()?;
        let mut args: Vec<(i16, Option<Vec<u8>>)> = vec![];
        for i in 0..n.max(0) as usize {
            let len = c.i32()?;
            let fmt = format(&formats, i);
            args.push((fmt, if len < 0 { None } else { Some(c.take(len as usize)?.to_vec()) }));
        }
        let _result_format = c.i16()?;
        // (oid, name, argument kinds: i integer, b bytes, result is 8 bytes)
        const FUNCS: &[(i32, &str, &str, usize)] = &[
            (952, "lo_open", "ii", 4),
            (953, "lo_close", "i", 4),
            (954, "loread", "ii", 0),
            (955, "lowrite", "ib", 4),
            (956, "lo_lseek", "iii", 4),
            (957, "lo_creat", "i", 4),
            (958, "lo_tell", "i", 4),
            (964, "lo_unlink", "i", 4),
            (715, "lo_create", "i", 4),
            (1004, "lo_truncate", "ii", 4),
            (3170, "lo_lseek64", "iii", 8),
            (3171, "lo_tell64", "i", 8),
            (3172, "lo_truncate64", "ii", 4),
        ];
        let Some((_, name, kinds, width)) = FUNCS.iter().find(|f| f.0 == oid) else {
            return Err(Fail::Sql(SqlError {
                code: "42883",
                message: format!("function with OID {oid} does not exist"),
                info: None,
            }));
        };
        if args.len() != kinds.len() {
            return Err(Fail::Sql(SqlError {
                code: "08P01",
                message: format!(
                    "function call message contains {} arguments but function requires {}",
                    args.len(),
                    kinds.len()
                ),
                info: None,
            }));
        }
        // the arguments as values, never as SQL text: an integer is parsed here (a text one as Postgres's int8in reads
        // it, 22P02 if it isn't one; a binary one must be 4 or 8 bytes, 22P03), and bytes go as bytea
        let mut parts = vec![];
        for (i, ((fmt, v), kind)) in args.iter().zip(kinds.chars()).enumerate() {
            parts.push(match (v, kind) {
                (None, _) => "null".to_string(),
                (Some(b), 'b') => format!("'\\x{}'::bytea", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
                (Some(b), _) if *fmt == 1 && b.len() == 4 => i32::from_be_bytes(b[..4].try_into().unwrap()).to_string(),
                (Some(b), _) if *fmt == 1 && b.len() == 8 => i64::from_be_bytes(b[..8].try_into().unwrap()).to_string(),
                (Some(_), _) if *fmt == 1 => {
                    return Err(Fail::Sql(SqlError {
                        code: "22P03",
                        message: format!("incorrect binary data format in function argument {}", i + 1),
                        info: None,
                    }));
                }
                (Some(b), _) => {
                    let t = String::from_utf8_lossy(b);
                    match t.trim().parse::<i64>() {
                        Ok(n) => n.to_string(),
                        Err(_) => {
                            return Err(Fail::Sql(SqlError {
                                code: "22P02",
                                message: format!("invalid input syntax for type bigint: \"{t}\""),
                                info: None,
                            }));
                        }
                    }
                }
            });
        }
        let q = format!("select {name}({})", parts.join(", "));
        let outs = self.session.run(self.db, &q, &[]).map_err(Fail::Sql)?;
        let value = outs.last().and_then(|o| o.rows.first()).and_then(|r| r.first()).cloned();
        let mut body = vec![];
        match value {
            None | Some(Value::Null) => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(Value::Bytea(b)) => {
                body.extend_from_slice(&(b.len() as i32).to_be_bytes());
                body.extend_from_slice(&b);
            }
            Some(v) => {
                let n = match v {
                    Value::Int(n) => n,
                    other => other.to_text().parse().unwrap_or(0),
                };
                if *width == 8 {
                    body.extend_from_slice(&8i32.to_be_bytes());
                    body.extend_from_slice(&n.to_be_bytes());
                } else {
                    body.extend_from_slice(&4i32.to_be_bytes());
                    body.extend_from_slice(&(n as i32).to_be_bytes());
                }
            }
        }
        self.send(b'V', &body)?;
        Ok(())
    }

    /// Answers psql's catalog queries (\d, \dt, \dv, \dm, \ds, \l) from Chronos's own tables,
    /// views, sequences and branches, for the queries psql 16 sends; None if `q` isn't one of
    /// them (the SQL engine then answers it from its catalog tables, see `sql::tables`). A table's oid is worked
    /// out from its name, as the catalog's is (see `sql::relation_oids`).
    fn catalog(&self, q: &str) -> Option<sql::Result<Output>> {
        if !q.contains("pg_catalog.") {
            return None;
        }
        let text = |s: &str| Value::Text(s.into());
        let bool = |b: bool| Value::Text(if b { "t" } else { "f" }.into());
        let out = |cols: &[&str], rows: Vec<Vec<Value>>| Output {
            typmods: vec![],
            columns: Some(cols.iter().map(|c| (c.to_string(), Type::Text)).collect()),
            tag: format!("SELECT {}", rows.len()),
            rows,
        };
        // psql's name patterns, the regular expressions it compares a name and a schema with: `c.relname
        // OPERATOR(pg_catalog.~) '^(users.*)$'`, or E'...' with each backslash doubled when the pattern has one
        // (`"no.such.table"` is `E'^(no\\.such\\.table)$'`)
        let pattern = |col: &str| -> Option<String> {
            let rest = q.split(&format!("{col} OPERATOR(pg_catalog.~) ")).nth(1)?;
            let (escaped, rest) = match rest.strip_prefix("E'") {
                Some(r) => (true, r),
                None => (false, rest.strip_prefix('\'')?),
            };
            let (mut p, mut chars) = (String::new(), rest.chars().peekable());
            while let Some(c) = chars.next() {
                match c {
                    '\'' if chars.peek() == Some(&'\'') => {
                        chars.next();
                        p.push('\'');
                    }
                    '\'' => break,
                    '\\' if escaped => p.extend(chars.next()),
                    c => p.push(c),
                }
            }
            Some(p)
        };
        let (names, schemas) = (pattern("c.relname"), pattern("n.nspname"));
        // without a schema, psql lists what the search_path shows
        let visible = q.contains("pg_catalog.pg_table_is_visible(c.oid)");
        let matches = |name: &str| {
            names.as_ref().is_none_or(|p| sql::regex_match(sql::rel_part(name), p))
                && schemas.as_ref().is_none_or(|p| sql::regex_match(sql::schema_part(name), p))
                && (!visible || self.session.in_path(self.db, || sql::shown_name(name) == sql::rel_part(name)))
        };
        // the table an oid in the query names (psql's per-table queries name it as '16385'; a query that names it
        // another way, 'customers'::regclass, is the engine's to answer from its catalog)
        let oid = q.split('\'').skip(1).step_by(2).find_map(|n| n.parse::<usize>().ok());
        let answer = (|| -> sql::Result<Option<Output>> {
            let tables = sql::tables(self.db, self.session.target())?;
            let comments = sql::comments(self.db, self.session.target())?;
            let oids = sql::relation_oids(&tables.iter().map(|t| t.name.clone()).collect::<Vec<_>>());
            let by_oid = || oid.and_then(|o| oids.iter().position(|x| *x == o as i64)).and_then(|i| tables.get(i));
            // a per-relation query of one of `tables`; another's (an index's, a composite type's) is the engine's to
            // answer from its catalog, which has the same oids
            let rel = by_oid().is_some();
            // (\dn and \dn+ are the engine's to answer, from pg_namespace: owners and privileges)
            Ok(Some(if q.contains("FROM pg_catalog.pg_database d") && !q.contains("\"Size\"") {
                let cols = [
                    "Name",
                    "Owner",
                    "Encoding",
                    "Locale Provider",
                    "Collate",
                    "Ctype",
                    "ICU Locale",
                    "ICU Rules",
                    "Access privileges",
                ];
                let rows = self
                    .db
                    .branches()
                    .into_iter()
                    .filter(|(b, _)| !b.starts_with("_tx_"))
                    .map(|(b, _)| {
                        vec![
                            text(&b),
                            text("chronos"),
                            text("UTF8"),
                            text("libc"),
                            text("C"),
                            text("C"),
                            Value::Null,
                            Value::Null,
                            Value::Null,
                        ]
                    })
                    .collect();
                out(&cols, rows)
            } else if q.contains("as \"Owner\"")
                && q.contains("FROM pg_catalog.pg_class c")
                && !q.contains("\"Size\"")
                && !q.contains("pg_partition_tree")
            {
                // the kinds asked for: c.relkind IN ('r','p','v','m','S','f','')
                let kinds = q.split("c.relkind IN (").nth(1).and_then(|r| r.split(')').next()).unwrap_or_default();
                let has = |k: char| kinds.contains(&format!("'{k}'"));
                let owner = |t: &sql::TableInfo| sql::owner_name(self.db, self.session.target(), &t.name);
                // an index matches as a relation of its table's schema does; it is visible where that schema is on
                // the search_path
                let index_matches = |table: &str, index: &str| {
                    names.as_ref().is_none_or(|p| sql::regex_match(index, p))
                        && schemas.as_ref().is_none_or(|p| sql::regex_match(sql::schema_part(table), p))
                        && (!visible
                            || self.session.in_path(self.db, || sql::shown_index(table, index))
                                == sql::quote_ident(index))
                };
                if !has('r') && !has('i') && (has('p') || has('I')) {
                    // \dP: the partitioned tables and indexes; the columns psql asked for, after Schema, Name
                    // and Owner: Type (both kinds), Parent name (with a pattern, or n) and Table (indexes)
                    let (mixed, parents, of_table) =
                        (q.contains("as \"Type\""), q.contains("as \"Parent name\""), q.contains("as \"Table\""));
                    // (without n or a pattern, a partition that is itself partitioned isn't listed)
                    let top = q.contains("NOT c.relispartition");
                    // (schema, type, parent, name, owner, table)
                    type Part = (String, &'static str, Option<String>, String, String, Option<String>);
                    let mut found: Vec<Part> = vec![];
                    for t in tables.iter().filter(|t| t.kind == 'p') {
                        let schema = sql::schema_part(&t.name).to_string();
                        let parent = t.hier.of.as_ref().map(|(p, ..)| p.clone());
                        if has('p') && matches(&t.name) && !(top && parent.is_some()) {
                            let shown = parent.map(|p| self.session.in_path(self.db, || sql::shown_name(&p)));
                            let name = sql::rel_part(&t.name).to_string();
                            found.push((schema.clone(), "partitioned table", shown, name, owner(t), None));
                        }
                        if !has('I') {
                            continue;
                        }
                        for x in t.indexes.iter().filter(|x| index_matches(&t.name, &x.name)) {
                            if top && x.given.is_some() {
                                continue;
                            }
                            // (the index it is a partition of is on the table this one is a partition of)
                            let up = t.hier.of.as_ref().map_or(t.name.as_str(), |(p, ..)| p.as_str());
                            let shown =
                                x.given.as_ref().map(|g| self.session.in_path(self.db, || sql::shown_index(up, g)));
                            let table = self.session.in_path(self.db, || sql::shown_name(&t.name));
                            found.push((
                                schema.clone(),
                                "partitioned index",
                                shown,
                                x.name.clone(),
                                owner(t),
                                Some(table),
                            ));
                        }
                    }
                    // ORDER BY "Schema", "Type" DESC (when both kinds), "Parent name" NULLS FIRST (when shown), "Name"
                    found.sort_by(|a, b| {
                        a.0.cmp(&b.0)
                            .then_with(|| if mixed { b.1.cmp(a.1) } else { std::cmp::Ordering::Equal })
                            .then_with(|| if parents { a.2.cmp(&b.2) } else { std::cmp::Ordering::Equal })
                            .then_with(|| a.3.cmp(&b.3))
                    });
                    let opt = |s: &Option<String>| s.as_deref().map_or(Value::Null, text);
                    let rows = found
                        .iter()
                        .map(|(schema, kind, parent, name, own, table)| {
                            let mut r = vec![text(schema), text(name), text(own)];
                            if mixed {
                                r.push(text(kind));
                            }
                            if parents {
                                r.push(opt(parent));
                            }
                            if of_table {
                                r.push(opt(table));
                            }
                            r
                        })
                        .collect();
                    let mut cols = vec!["Schema", "Name", "Owner"];
                    if mixed {
                        cols.push("Type");
                    }
                    if parents {
                        cols.push("Parent name");
                    }
                    if of_table {
                        cols.push("Table");
                    }
                    out(&cols, rows)
                } else {
                    // \d and \dt, \di, \dv, \dm, \ds, \dE: (schema, name, type, owner, an index's table)
                    let mut found: Vec<(String, String, &'static str, String, Option<String>)> = tables
                        .iter()
                        .filter(|t| has(t.kind) && matches(&t.name))
                        .map(|t| {
                            let (s, n) = (sql::schema_part(&t.name), sql::rel_part(&t.name));
                            (s.to_string(), n.to_string(), kind_name(t.kind), owner(t), None)
                        })
                        .collect();
                    for t in &tables {
                        let kind = if t.kind == 'p' { 'I' } else { 'i' };
                        if !has(kind) {
                            continue;
                        }
                        for x in t.indexes.iter().filter(|x| index_matches(&t.name, &x.name)) {
                            let table = sql::rel_part(&t.name).to_string();
                            let schema = sql::schema_part(&t.name).to_string();
                            found.push((schema, x.name.clone(), kind_name(kind), owner(t), Some(table)));
                        }
                    }
                    // ORDER BY 1, 2
                    found.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
                    let of_table = q.contains("as \"Table\"");
                    let rows = found
                        .into_iter()
                        .map(|(s, n, k, o, table)| {
                            let mut r = vec![text(&s), text(&n), text(k), text(&o)];
                            if of_table {
                                r.push(table.as_deref().map_or(Value::Null, text));
                            }
                            r
                        })
                        .collect();
                    let mut cols = vec!["Schema", "Name", "Type", "Owner"];
                    if of_table {
                        cols.push("Table");
                    }
                    out(&cols, rows)
                }
            } else if q.contains("SELECT c.oid,") && q.contains("n.nspname,") {
                // \d's lookup of what a pattern names: every relation pg_class has (indexes, partitioned indexes and
                // composite types too), as the engine answers it
                return Ok(None);
            } else if rel && q.contains("c.relchecks, c.relkind") {
                let rows = by_oid()
                    .map(|t| {
                        let indexed = !t.indexes.is_empty();
                        // psql then asks for them, and for triggers
                        let linked = !t.fks.is_empty() || !t.referenced_by.is_empty() || !t.triggers.is_empty();
                        let checks = t.constraints.iter().filter(|c| c.kind == 'c').count();
                        let mut r = vec![text(&checks.to_string()), text(&t.kind.to_string()), bool(indexed)];
                        r.extend([
                            bool(false),
                            bool(linked),
                            bool(t.row_security.0),
                            bool(t.row_security.1),
                            bool(false),
                        ]);
                        // (pg_class.relreplident: what REPLICA IDENTITY set, `d` unless it said otherwise)
                        let ident = if t.hier.replident == '\0' { 'd' } else { t.hier.replident };
                        // (its reloptions, `a=b, c=d`: a view's options)
                        let opts = t.reloptions.iter().map(|(o, v)| format!("{o}={v}")).collect::<Vec<_>>().join(", ");
                        r.extend([
                            bool(t.hier.of.is_some()),
                            text(&opts),
                            // (its tablespace, 0 for the database's default: psql's "Tablespace:" footer)
                            text(&t.hier.tablespace.to_string()),
                            // (a typed table's composite type, as regtype names it)
                            text(&t.hier.of_type.as_deref().map(sql::shown_type).unwrap_or_default()),
                            text(if t.unlogged { "u" } else { "p" }),
                            text(&ident.to_string()),
                        ]);
                        // (a partitioned table and a foreign table have no access method; a table's is heap, or the
                        // one CREATE TABLE ... USING gave it)
                        r.push(match t.kind {
                            'p' | 'f' => Value::Null,
                            _ => text(t.hier.am.as_deref().unwrap_or("heap")),
                        });
                        r
                    })
                    .into_iter()
                    .collect();
                let cols = [
                    "relchecks",
                    "relkind",
                    "relhasindex",
                    "relhasrules",
                    "relhastriggers",
                    "relrowsecurity",
                    "relforcerowsecurity",
                    "relhasoids",
                    "relispartition",
                    "?column?",
                    "reltablespace",
                    "reloftype",
                    "relpersistence",
                    "relreplident",
                    "amname",
                ];
                out(&cols, rows)
            } else if rel && q.contains("FROM pg_catalog.pg_attribute a") && q.contains("WHERE a.attrelid = '") {
                let verbose = q.contains("a.attstorage"); // \d+ asks for storage and descriptions too
                // (psql 14 and later also ask for the compression method and the statistics target)
                let compressed = q.contains("a.attcompression");
                // (psql hides the compression column on request, and still asks for the statistics target)
                let stats = q.contains("attstattarget");
                // (a foreign table's columns' options, for its wrapper)
                let fdw = q.contains("attfdwoptions");
                let rows = by_oid()
                    .map(|t| {
                        t.columns
                            .iter()
                            .enumerate()
                            .map(|(i, (name, ty, not_null, default))| {
                                // (as pg_get_expr(adbin, adrelid, true) writes it: pretty)
                                let d = default.as_deref().map_or(Value::Null, |d| text(&sql::pretty_default(t, d)));
                                // (varchar(n), numeric(p, s) and smallint keep what they were made with)
                                let (own, typmod) = t.typmods.get(i).copied().unwrap_or((0, -1));
                                let shown = sql::sized_type(own, typmod).unwrap_or_else(|| ty.pg_name().into_owned());
                                let coll = t.collation_name(i).map_or(Value::Null, |c| text(&c));
                                // (an identity column's kind, `a` or `d`, and a generated one's `s`: what psql shows
                                // as its default)
                                let identity = t.identity.get(i).filter(|c| **c != ' ').map(char::to_string);
                                let generated = if t.generated.get(i).is_some_and(Option::is_some) { "s" } else { "" };
                                // (psql writes a generated column's expression in its own parentheses)
                                let d = match (&d, generated) {
                                    (Value::Text(e), "s") => text(sql::without_outer_parens(e)),
                                    _ => d,
                                };
                                let mut r = vec![
                                    text(name),
                                    text(&shown),
                                    d,
                                    bool(*not_null),
                                    coll,
                                    text(&identity.unwrap_or_default()),
                                    text(generated),
                                ];
                                if fdw {
                                    r.push(text(&t.fdw_column_options(i).unwrap_or_default()));
                                }
                                if verbose {
                                    let fixed = ty.whole()
                                        || matches!(ty, Type::Float | Type::Float4 | Type::Bool | Type::Date);
                                    let comment = comments.get(&format!("column/{}/{name}", t.name));
                                    // (a mac address is plain, like the fixed-size types; an inet, a cidr and a numeric are main)
                                    let storage = match ty {
                                        Type::MacAddr | Type::MacAddr8 => "p",
                                        Type::Inet | Type::Cidr | Type::Numeric => "m",
                                        _ if fixed => "p",
                                        _ => "x",
                                    };
                                    // (what SET STORAGE, SET COMPRESSION and SET STATISTICS left, where the table
                                    // says)
                                    let opts = t.attopts.get(i);
                                    r.push(text(&opts.map_or_else(|| storage.to_string(), |o| o.0.to_string())));
                                    if compressed {
                                        r.push(text(&opts.and_then(|o| o.2).map(String::from).unwrap_or_default()));
                                    }
                                    if stats {
                                        r.push(match opts.map(|o| o.1) {
                                            Some(n) if n != -1 => text(&n.to_string()),
                                            _ => Value::Null,
                                        });
                                    }
                                    r.push(comment.map_or(Value::Null, |c| text(c)));
                                }
                                r
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let mut cols = vec![
                    "attname",
                    "format_type",
                    "pg_get_expr",
                    "attnotnull",
                    "attcollation",
                    "attidentity",
                    "attgenerated",
                ];
                if fdw {
                    cols.push("attfdwoptions");
                }
                if verbose {
                    cols.push("attstorage");
                    if compressed {
                        cols.push("attcompression");
                    }
                    if stats {
                        cols.push("attstattarget");
                    }
                    cols.push("col_description");
                }
                out(&cols, rows)
            } else if rel && q.contains("pg_catalog.pg_class c2, pg_catalog.pg_index i") {
                // the primary key first, then the others by name
                let mut rows = vec![];
                if let Some(t) = by_oid() {
                    let mut all: Vec<&sql::Idx> = t.indexes.iter().collect();
                    all.sort_by(|a, b| b.primary.cmp(&a.primary).then_with(|| a.name.cmp(&b.name)));
                    for x in all {
                        let con = t.constraints.iter().find(|c| c.name == x.name && matches!(c.kind, 'p' | 'u'));
                        let mut def = format!(
                            "CREATE {}INDEX {} ON {}.{} USING {} {}",
                            if x.unique { "UNIQUE " } else { "" },
                            x.name,
                            sql::schema_part(&t.name),
                            sql::rel_part(&t.name),
                            sql::quote_ident(x.am_name()),
                            // (pg_get_indexdef(oid, 0, true): pretty)
                            x.columns_def(true)
                        );
                        if x.nnd {
                            def.push_str(" NULLS NOT DISTINCT");
                        }
                        if let Some(p) = &x.pred_pretty {
                            def.push_str(&format!(" WHERE {p}"));
                        }
                        rows.push(vec![
                            text(&x.name),
                            bool(x.primary),
                            bool(x.unique),
                            bool(x.clustered),
                            bool(x.valid),
                            text(&def),
                            con.map_or(Value::Null, |c| text(&c.pretty)),
                            con.map_or(Value::Null, |c| text(&c.kind.to_string())),
                            con.map_or(Value::Null, |c| bool(c.defer.0)),
                            con.map_or(Value::Null, |c| bool(c.defer.1)),
                            bool(x.replident),
                            // (each index's tablespace: `, tablespace "t"` after it)
                            text(&x.tablespace.to_string()),
                        ]);
                    }
                }
                let cols = [
                    "relname",
                    "indisprimary",
                    "indisunique",
                    "indisclustered",
                    "indisvalid",
                    "pg_get_indexdef",
                    "pg_get_constraintdef",
                    "contype",
                    "condeferrable",
                    "condeferred",
                    "indisreplident",
                    "reltablespace",
                ];
                out(&cols, rows)
            } else if rel && q.contains("r.contype = 'f'") && q.contains("r.conrelid = '") {
                let rows = by_oid()
                    .map(|t| t.fks.iter().map(|(n, def)| vec![bool(true), text(n), text(def), text(&t.name)]).collect())
                    .unwrap_or_default();
                out(&["sametable", "conname", "condef", "ontable"], rows)
            // (a partition's foreign keys are its own and those of the tables it is a partition of)
            } else if rel && q.contains("conrelid = relid") && q.contains("pg_partition_ancestors") {
                let mut rows: Vec<(bool, String, String, String)> = vec![];
                if let Some(t) = by_oid() {
                    let mut chain = vec![t];
                    while let Some(p) = chain.last().and_then(|x| x.hier.of.as_ref()).map(|(p, ..)| p.clone()) {
                        match tables.iter().find(|x| x.name == p) {
                            Some(x) if !chain.iter().any(|c| c.name == x.name) => chain.push(x),
                            _ => break,
                        }
                    }
                    for (n, x) in chain.iter().enumerate() {
                        for c in x.constraints.iter().filter(|c| c.kind == 'f' && c.given.is_none()) {
                            rows.push((n == 0, c.name.clone(), c.pretty.clone(), sql::shown_name(&x.name)));
                        }
                    }
                }
                rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                let rows = rows.into_iter().map(|(s, n, d, o)| vec![bool(s), text(&n), text(&d), text(&o)]).collect();
                out(&["sametable", "conname", "condef", "ontable"], rows)
            } else if rel && q.contains("confrelid IN") && q.contains("pg_partition_ancestors") {
                let mut rows: Vec<(String, String, String)> = vec![];
                if let Some(t) = by_oid() {
                    // the tables whose foreign keys are on this one: it and those it is a partition of
                    let mut names = vec![t.name.clone()];
                    while let Some(p) = names
                        .last()
                        .and_then(|n| tables.iter().find(|x| x.name == *n))
                        .and_then(|x| x.hier.of.as_ref())
                        .map(|(p, ..)| p.clone())
                    {
                        if names.contains(&p) {
                            break;
                        }
                        names.push(p);
                    }
                    for child in &tables {
                        for c in child.constraints.iter().filter(|c| c.kind == 'f' && c.given.is_none()) {
                            if c.parent.as_ref().is_some_and(|(p, ..)| names.contains(p)) {
                                rows.push((c.name.clone(), sql::shown_name(&child.name), c.pretty.clone()));
                            }
                        }
                    }
                }
                rows.sort();
                let rows = rows.into_iter().map(|(n, o, d)| vec![text(&n), text(&o), text(&d)]).collect();
                out(&["conname", "ontable", "condef"], rows)
            } else if rel && q.contains("FROM pg_catalog.pg_sequence") && q.contains("AS \"Type\"") {
                let rows: Vec<Vec<Value>> = by_oid()
                    .and_then(|t| t.sequence.clone())
                    .map(|(ty, start, min, max, inc, cycle)| {
                        let n = Value::Int;
                        vec![text(&ty), n(start), n(min), n(max), n(inc), text(if cycle { "yes" } else { "no" }), n(1)]
                    })
                    .into_iter()
                    .collect();
                // (pg_sequence's numbers are bigints, which psql lines up on the right)
                let cols = ["Type", "Start", "Minimum", "Maximum", "Increment", "Cycles?", "Cache"];
                let types =
                    [Type::Text, Type::BigInt, Type::BigInt, Type::BigInt, Type::BigInt, Type::Text, Type::BigInt];
                Output {
                    typmods: vec![],
                    columns: Some(cols.iter().zip(types).map(|(c, t)| (c.to_string(), t)).collect()),
                    tag: format!("SELECT {}", rows.len()),
                    rows,
                }
            } else if rel && q.contains("pg_catalog.pg_depend d") {
                // a sequence's owning column (OWNED BY, a serial's: `a`; an identity column's: `i`), as
                // schema.table.column with each part as quote_ident writes it
                let rows = by_oid()
                    .and_then(|t| t.owner.clone())
                    .map(|(table, column, deptype)| {
                        let (s, r) = (sql::schema_part(&table), sql::rel_part(&table));
                        let owned =
                            format!("{}.{}.{}", sql::quote_ident(s), sql::quote_ident(r), sql::quote_ident(&column));
                        vec![text(&owned), text(&deptype.to_string())]
                    })
                    .into_iter()
                    .collect();
                out(&["?column?", "deptype"], rows)
            } else if rel && q.contains("FROM pg_catalog.pg_rewrite r") {
                out(&["rulename", "btrim"], vec![])
            } else if rel && q.contains("FROM pg_catalog.pg_trigger t") {
                let rows = by_oid()
                    .map(|t| {
                        // (a partition's copy of its parent's trigger says so: `, ON TABLE parent`)
                        t.triggers
                            .iter()
                            .enumerate()
                            .map(|(k, (n, def, on))| {
                                let cloned = t.trigger_links.get(k).is_some_and(|l| l.2);
                                let parent = match (cloned, &t.hier.of) {
                                    (true, Some((p, ..))) => text(&trigger_root(&tables, p, n)),
                                    _ => Value::Null,
                                };
                                vec![text(n), text(def), text(&on.to_string()), bool(false), parent]
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out(&["tgname", "pg_get_triggerdef", "tgenabled", "tgisinternal", "parent"], rows)
            } else if rel && q.contains("pg_catalog.pg_policy") {
                let opt = |s: &Option<String>| s.as_deref().map_or(Value::Null, text);
                let rows = by_oid()
                    .map(|t| {
                        t.policies
                            .iter()
                            .map(|p| {
                                let roles = (p.roles != [0]).then(|| p.role_names.join(","));
                                let cmd = (p.cmd != '*').then(|| p.cmd_name().to_string());
                                vec![
                                    text(&p.name),
                                    bool(p.permissive),
                                    opt(&roles),
                                    opt(&p.using),
                                    opt(&p.check),
                                    opt(&cmd),
                                ]
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out(&["polname", "polpermissive", "array_to_string", "pg_get_expr", "pg_get_expr", "cmd"], rows)
            } else if rel && q.contains("pg_catalog.pg_statistic_ext") && q.contains("WHERE stxrelid = '") {
                let rows = match by_oid() {
                    Some(t) => sql::table_statistics(self.db, self.session.target(), &t.name)?,
                    None => vec![],
                };
                out(
                    &[
                        "oid",
                        "stxrelid",
                        "nsp",
                        "stxname",
                        "columns",
                        "ndist_enabled",
                        "deps_enabled",
                        "mcv_enabled",
                        "stxstattarget",
                    ],
                    rows,
                )
            } else if rel && q.contains("FROM pg_catalog.pg_publication p") && q.contains("pg_relation_is_publishable")
            {
                // \d's "Publications:": the name, the table's WHERE and its column list
                let found = match by_oid() {
                    Some(t) => sql::table_publications(self.db, self.session.target(), &t.name)?,
                    None => vec![],
                };
                let opt = |s: Option<String>| s.map_or(Value::Null, |s| text(&s));
                let rows = found.into_iter().map(|(n, f, c)| vec![text(&n), opt(f), opt(c)]).collect();
                out(&["pubname", "?column?", "?column?"], rows)
            } else if let Some(o) =
                oid.filter(|_| q.contains("pg_catalog.pg_publication_rel pr") && q.contains("pr.prpubid = '"))
            {
                // \dRp+'s tables of a publication
                let opt = |s: Option<String>| s.map_or(Value::Null, |s| text(&s));
                let rows = sql::publication_tables(self.db, self.session.target(), o as i64)?
                    .into_iter()
                    .map(|(s, n, f, c)| vec![text(&s), text(&n), opt(f), opt(c)])
                    .collect();
                out(&["nspname", "relname", "pg_get_expr", "case"], rows)
            } else if let Some(o) =
                oid.filter(|_| q.contains("pg_catalog.pg_publication_namespace pn") && q.contains("pn.pnpubid = '"))
            {
                // \dRp+'s schemas of a publication
                let rows = sql::publication_schemas(self.db, self.session.target(), o as i64)?
                    .into_iter()
                    .map(|s| vec![text(&s)])
                    .collect();
                out(&["nspname"], rows)
            } else if oid.is_some() && q.contains("pg_catalog.pg_publication") {
                out(&["pubname", "?column?", "?column?"], vec![])
            } else {
                return Ok(None);
            }))
        })();
        answer.transpose()
    }

    /// A simple query: one or more statements, text results, each committed as it runs.
    fn simple(&mut self, sql: &str) -> io::Result<()> {
        self.activity(Some(sql));
        // psql's catalog queries are answered here only once the checks any statement gets pass (an agent's rights,
        // a role's privileges); one they refuse goes to the engine, which refuses it as it refuses any statement
        let vetted = sql.contains("pg_catalog.") && self.session.vet_catalog(self.db, sql).unwrap_or(false);
        if let Some(r) = vetted.then(|| self.catalog(sql)).flatten() {
            return match r {
                Ok(out) => {
                    self.row_description(out.columns.as_deref().unwrap_or_default(), &out.typmods, &[])?;
                    self.rows(&out, &[])
                }
                Err(e) => self.error(&e),
            };
        }
        let script = match self.session.script(self.db, sql) {
            Ok(s) => s,
            Err(e) => {
                // what reading it warned of goes first (CREATE AGGREGATE's attributes it doesn't know)
                self.notices()?;
                // (a statement that can't be read fails its transaction, as one that fails as it runs does; one Chronos
                // refuses as not supported here leaves it as it was)
                if e.code != "0A000" {
                    self.session.abort();
                }
                return self.error(&e);
            }
        };
        // what reading it warned of, before what it does (the statements' own notices start afresh)
        self.notices()?;
        if script.is_empty() {
            return self.send(b'I', &[]);
        }
        for i in 0..script.len() {
            let s = &match self.session.next_statement(self.db, &script, i) {
                Ok(s) => s,
                Err(e) => {
                    if e.code != "0A000" {
                        self.session.abort();
                    }
                    return self.error(&e);
                }
            };
            if s.copy_direction().is_some() {
                match self.copy(s) {
                    Ok(()) => continue,
                    Err(Fail::Sql(e)) => return self.error(&e),
                    Err(Fail::Io(e)) => return Err(e),
                }
            }
            let out = self.session.execute(self.db, s, &[], &[]);
            self.notices()?;
            match out {
                Ok(mut out) => {
                    self.session.show_regs(self.db, &mut out, &|_| true);
                    if let Some(cols) = &out.columns {
                        self.row_description(cols, &out.typmods, &[])?;
                    }
                    self.rows(&out, &[])?;
                }
                Err(e) => return self.error(&script.locate(i, e)),
            }
        }
        Ok(())
    }

    fn parse(&mut self, c: &mut Cur) -> Result<(), Fail> {
        let (name, query) = (c.str()?, c.str()?);
        let looked_up = type_lookup(&query);
        let query = looked_up.clone().unwrap_or(query);
        let mut given: Vec<Option<Type>> =
            (0..c.i16()?).map(|_| c.i32().map(|o| Type::from_oid(o as u32))).collect::<Result<_, _>>()?;
        // (asyncpg's introspection sends its list of type ids as an oid[], which it has a codec for)
        if looked_up.is_some_and(|q| q.contains("any($1)")) {
            given = vec![Some(Type::Array(26))];
        }
        let mut stmts = self.session.parse(self.db, &query)?;
        let stmt = match stmts.len() {
            0 => None,
            1 => stmts.pop(),
            _ => {
                return Err(SqlError {
                    code: "42601",
                    message: "cannot insert multiple commands into a prepared statement".into(),
                    info: None,
                }
                .into());
            }
        };
        let types = match &stmt {
            Some(s) => sql::declared(self.session.params(self.db, s, &given)?, &given),
            None => vec![],
        };
        // (the unnamed statement isn't listed in pg_prepared_statements, so its result types aren't worked out)
        let result = match &stmt {
            Some(s) if !name.is_empty() => sql::result_types(self.session.columns(self.db, s, &types)),
            _ => None,
        };
        let text: Arc<str> = Arc::from(query.as_str());
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as i64);
        let stmt = stmt.map(|s| s.with_proc_params(&types).map(Arc::new)).transpose()?;
        let p = Prepared { stmt, types, result, text, own_text: None, from_sql: false, at };
        Arc::make_mut(&mut self.session.prepared).insert(name, p);
        Ok(self.send(b'1', &[])?)
    }

    fn prepared(&self, name: &str) -> Result<Prepared, Fail> {
        self.session.prepared.get(name).cloned().ok_or_else(|| {
            SqlError { code: "26000", message: format!("prepared statement \"{name}\" does not exist"), info: None }
                .into()
        })
    }

    fn bind(&mut self, c: &mut Cur) -> Result<(), Fail> {
        let (portal, name) = (c.str()?, c.str()?);
        let p = self.prepared(&name)?;
        let pformats = c.i16s()?;
        let n = c.i16()? as usize;
        if n != p.types.len() {
            return Err(protocol(format!(
                "bind message supplies {n} parameters, but prepared statement \"{name}\" requires {}",
                p.types.len()
            )));
        }
        let mut params = vec![];
        self.session.use_zone(); // text timestamps without a zone are the session's
        for (i, ty) in p.types.iter().enumerate() {
            let len = c.i32()?;
            let (b, f) = if len < 0 { (None, 0) } else { (Some(c.take(len as usize)?), format(&pformats, i)) };
            params.push(match b {
                None => Value::Null,
                // a reg value's name (a regproc's, a regclass's) is found in this session's world, where an object made
                // here is
                Some(b) if f == 0 && matches!(ty, Type::Reg(_)) => {
                    self.session.in_session(self.db, || decode(b, f, *ty))?
                }
                Some(b) => decode(b, f, *ty)?,
            });
        }
        let formats = c.i16s()?;
        self.portals
            .insert(portal, Rc::new(Portal { p, params, formats, rest: RefCell::new(None), done: Default::default() }));
        Ok(self.send(b'2', &[])?)
    }

    fn describe(&mut self, c: &mut Cur) -> Result<(), Fail> {
        let (kind, name) = (c.take(1)?[0], c.str()?);
        let (p, formats) = match kind {
            b'S' => {
                let p = self.prepared(&name)?;
                let mut b = (p.types.len() as i16).to_be_bytes().to_vec();
                // (a parameter that is a varchar's or a char(n)'s value is of that type)
                let oids = match &p.stmt {
                    Some(s) => self.session.param_oids(self.db, s, &p.types),
                    None => p.types.iter().map(|t| t.oid()).collect(),
                };
                for oid in &oids {
                    b.extend_from_slice(&oid.to_be_bytes());
                }
                self.send(b't', &b)?;
                (p, vec![])
            }
            _ => {
                let portal = self.portals.get(&name).cloned();
                // a cursor DECLAREd in SQL is a portal of its name, as in Postgres: psycopg's server-side cursors
                // describe theirs so (DECLARE "c" CURSOR FOR ..., then Describe portal "c", then FETCH)
                if portal.is_none()
                    && let Some((cols, typmods)) = self.session.cursor_columns(&name)
                {
                    self.row_description(&cols, &typmods, &[])?;
                    return Ok(());
                }
                let p = portal.ok_or_else(|| SqlError {
                    code: "34000",
                    message: format!("portal \"{name}\" does not exist"),
                    info: None,
                })?;
                (p.p.clone(), p.formats.clone())
            }
        };
        let cols = match &p.stmt {
            Some(s) => self.session.columns_typed(self.db, s, &p.types)?,
            None => None,
        };
        match cols {
            Some((cols, typmods)) => self.row_description(&cols, &typmods, &formats)?,
            None => self.send(b'n', &[])?,
        }
        Ok(())
    }

    fn execute(&mut self, c: &mut Cur) -> Result<(), Fail> {
        let name = c.str()?;
        let max = c.i32()?; // the most rows to send now (0: all of them)
        let p = self.portals.get(&name).cloned();
        let p = p.ok_or_else(|| SqlError {
            code: "34000",
            message: format!("portal \"{name}\" does not exist"),
            info: None,
        })?;
        if p.done.get() {
            return Err(
                SqlError { code: "55000", message: format!("portal \"{name}\" cannot be run"), info: None }.into()
            );
        }
        self.activity(Some(&p.p.text));
        match &p.p.stmt {
            None => self.send(b'I', &[])?,
            Some(s) if s.copy_direction().is_some() => self.copy(s)?,
            Some(s) => {
                // the rest of a portal an earlier Execute stopped part-way, or the statement run now
                let resumed = p.rest.borrow_mut().take();
                let (out, sent) = match resumed {
                    Some(r) => r,
                    None => {
                        self.session.received(p.p.text.clone());
                        let out = self.session.execute(self.db, s, &p.params, &p.p.types);
                        // (its notices go first, even when it failed)
                        self.notices()?;
                        let mut out = out?;
                        // (a reg value read in binary is its oid)
                        self.session.show_regs(self.db, &mut out, &|i| format(&p.formats, i) == 0);
                        (out, 0)
                    }
                };
                let left = out.rows.len() - sent;
                // (exactly as many rows as asked for is still "maybe more": the executor stopped at the limit)
                if max > 0 && left >= max as usize {
                    self.rows_of(&out, &p.formats, sent..sent + max as usize)?;
                    self.send(b's', &[])?; // PortalSuspended: Execute again for more
                    *p.rest.borrow_mut() = Some((out, sent + max as usize));
                } else {
                    let rest = sent..out.rows.len();
                    let mut out = out;
                    // a SELECT's tag counts the rows of the Execute that finished it
                    if out.tag.starts_with("SELECT ") {
                        out.tag = format!("SELECT {}", rest.len());
                    }
                    self.rows_of(&out, &p.formats, rest)?;
                    self.complete(&out.tag)?;
                    p.done.set(true);
                }
            }
        }
        Ok(())
    }

    fn close(&mut self, c: &mut Cur) -> Result<(), Fail> {
        let (kind, name) = (c.take(1)?[0], c.str()?);
        if kind == b'S' {
            Arc::make_mut(&mut self.session.prepared).remove(&name);
        } else {
            self.portals.remove(&name);
        }
        Ok(self.send(b'3', &[])?)
    }

    /// A COPY: rows in from the client (CopyInResponse, then its CopyData until CopyDone or
    /// CopyFail), or out to it (CopyOutResponse, CopyData, CopyDone); then the command tag.
    fn copy(&mut self, s: &Statement) -> Result<(), Fail> {
        let header = |n: usize, binary: bool| {
            let mut b = vec![binary as u8];
            b.extend((n as i16).to_be_bytes());
            for _ in 0..n {
                b.extend((binary as i16).to_be_bytes());
            }
            b
        };
        if let Some(path) = s.copy_file() {
            let fail = |e: io::Error, verb: &str| SqlError {
                code: "58P01",
                message: format!("could not open file \"{path}\" for {verb}: {e}"),
                info: None,
            };
            let mut b = vec![];
            if s.copy_direction() == Some(false) {
                let out = self.session.copy_out(self.db, s)?;
                std::fs::write(path, out.data.concat()).map_err(|e| fail(e, "writing"))?;
                cstr(&mut b, &out.tag);
                return Ok(self.send(b'C', &b)?);
            }
            // (whether the session may read it is asked before the file is opened)
            let copy = self.session.copy_in(self.db, s)?;
            let data = std::fs::read(path).map_err(|e| fail(e, "reading"))?;
            let mut once = Some(data);
            let out = copy.run(&mut self.session, self.db, Box::new(move || Ok(once.take())));
            // (what its triggers said, before its tag or its error)
            self.notices()?;
            cstr(&mut b, &out?.tag);
            return Ok(self.send(b'C', &b)?);
        }
        if s.copy_direction() == Some(false) {
            let out = self.session.copy_out(self.db, s);
            // a COPY of a data-changing statement: what its BEFORE triggers said goes before its
            // rows, and what its AFTER triggers said after them
            let (before, after) = sql::take_notices_split();
            self.send_notices(before)?;
            let out = match out {
                Ok(out) => out,
                Err(e) => {
                    self.send_notices(after)?;
                    return Err(e.into());
                }
            };
            self.send(b'H', &header(out.columns, false))?;
            for d in &out.data {
                self.send(b'd', d)?;
            }
            self.send_notices(after)?;
            self.send(b'c', &[])?;
            let mut b = vec![];
            cstr(&mut b, &out.tag);
            return Ok(self.send(b'C', &b)?);
        }
        let mut copy = self.session.copy_in(self.db, s)?;
        copy.decode_with(|b, ty| {
            decode(b, 1, ty).map_err(|e| match e {
                Fail::Sql(e) => SqlError { code: "22P03", message: e.message, info: None },
                Fail::Io(e) => SqlError { code: "22P03", message: e.to_string(), info: None },
            })
        });
        self.send(b'G', &header(copy.columns(), copy.binary()))?;
        self.w.flush()?;
        // the COPY reads the data itself, a part of the rows at a time (see `CopyIn::run`): it
        // borrows the connection's reader, and a lost connection is kept for after
        let r = Rc::new(RefCell::new(std::mem::replace(&mut self.r, BufReader::new(Box::new(io::empty())))));
        let lost = Rc::new(RefCell::new(None));
        let (reader, gone) = (r.clone(), lost.clone());
        let next = Box::new(move || {
            loop {
                let mut r = reader.borrow_mut();
                let mut tag = [0];
                let read = r.read_exact(&mut tag).and_then(|_| message(&mut *r, 4, MAX_MESSAGE));
                let body = match read {
                    Ok(Msg::Body(b)) => b,
                    // too large for the server: dropped, and the COPY fails (its rest is dropped
                    // up to the Sync, as after any COPY error)
                    Ok(Msg::Big(n)) => return Err(too_big(n)),
                    Ok(Msg::Invalid) => {
                        let e = io::Error::other("invalid message length");
                        *gone.borrow_mut() = Some(e);
                        return Err(SqlError { code: "08P01", message: "invalid message length".into(), info: None });
                    }
                    Err(e) => {
                        *gone.borrow_mut() = Some(e);
                        return Err(SqlError {
                            code: "08006",
                            message: "the connection was lost during COPY".into(),
                            info: None,
                        });
                    }
                };
                return match tag[0] {
                    b'd' => Ok(Some(body)),
                    b'c' => Ok(None),
                    b'f' => {
                        let why = Cur(&body).str().unwrap_or_default();
                        Err(SqlError { code: "57014", message: format!("COPY from stdin failed: {why}"), info: None })
                    }
                    b'H' | b'S' => continue, // Flush and Sync mean nothing during a COPY
                    t => Err(SqlError {
                        code: "08P01",
                        message: format!("unexpected message type 0x{t:02X} during COPY from stdin"),
                        info: None,
                    }),
                };
            }
        });
        // the COPY holds its world's writes while it reads: a client that sends nothing for
        // COPY_IDLE is dropped, which lets them go (statement_timeout and cancels are checked as
        // data arrives, so they can't end a read that waits)
        let timed = self.sock.set_read_timeout(Some(*COPY_IDLE)).is_ok();
        let out = copy.run(&mut self.session, self.db, next);
        if timed {
            self.sock.set_read_timeout(None)?;
        }
        self.r = Rc::try_unwrap(r).map_err(|_| Fail::Io(io::Error::other("COPY kept the connection")))?.into_inner();
        if let Some(e) = lost.take() {
            return Err(Fail::Io(e));
        }
        // (what its triggers said, before its tag or its error)
        self.notices()?;
        let mut b = vec![];
        cstr(&mut b, &out?.tag);
        Ok(self.send(b'C', &b)?)
    }

    /// `typmods`: the columns' type oids and typmods where their `Type`s don't say them (a varchar's, a char(n)'s:
    /// see `Output::typmods`).
    fn row_description(
        &mut self,
        cols: &[(String, Type)],
        typmods: &[Option<(u32, i32)>],
        formats: &[i16],
    ) -> io::Result<()> {
        let mut b = (cols.len() as i16).to_be_bytes().to_vec();
        for (i, (name, ty)) in cols.iter().enumerate() {
            let (oid, typmod) = typmods.get(i).copied().flatten().unwrap_or((ty.oid(), -1));
            cstr(&mut b, name);
            b.extend_from_slice(&0i32.to_be_bytes()); // table
            b.extend_from_slice(&0i16.to_be_bytes()); // column number
            b.extend_from_slice(&oid.to_be_bytes());
            let size: i16 = match ty {
                Type::SmallInt => 2,
                Type::Int | Type::Oid => 4,
                Type::BigInt | Type::Float => 8,
                Type::Float4 => 4,
                Type::Bool | Type::Letter => 1,
                Type::Date => 4,
                Type::Timestamp | Type::TimestampTz | Type::Time => 8,
                Type::Interval | Type::Uuid => 16,
                Type::Reg(_) | Type::Void | Type::Poly(_) => 4,
                // (a base type made with CREATE TYPE: its representation's size, which it was made LIKE)
                Type::Base(_) => -1,
                Type::MacAddr => 6,
                Type::MacAddr8 => 8,
                Type::Inet
                | Type::Cidr
                | Type::Text
                | Type::Json
                | Type::JsonText
                | Type::Numeric
                | Type::Array(_)
                | Type::Vector
                | Type::TsVector
                | Type::TsQuery
                | Type::Geometry
                | Type::Geography
                | Type::Point
                | Type::Lseg
                | Type::Path
                | Type::Box
                | Type::Polygon
                | Type::Circle
                | Type::Line
                | Type::Money
                | Type::PgLsn
                | Type::Tid
                | Type::Xid
                | Type::Xml
                | Type::JsonPath
                | Type::Bit
                | Type::VarBit
                | Type::TimeTz
                | Type::Hstore
                | Type::Ltree
                | Type::Lquery
                | Type::Name
                | Type::Xid8
                | Type::PgSnapshot
                | Type::Bytea
                | Type::Enum(_)
                | Type::Range(_)
                | Type::Multirange(_)
                | Type::Record(_) => -1,
            };
            b.extend_from_slice(&size.to_be_bytes());
            b.extend_from_slice(&typmod.to_be_bytes()); // type modifier
            b.extend_from_slice(&format(formats, i).to_be_bytes());
        }
        self.send(b'T', &b)
    }

    /// The rows, then the command tag.
    fn rows(&mut self, out: &Output, formats: &[i16]) -> io::Result<()> {
        self.rows_of(out, formats, 0..out.rows.len())?;
        self.complete(&out.tag)
    }

    /// DataRow messages for some of `out`'s rows.
    fn rows_of(&mut self, out: &Output, formats: &[i16], which: std::ops::Range<usize>) -> io::Result<()> {
        let cols = out.columns.as_deref().unwrap_or_default();
        let mut b = Vec::with_capacity(128); // (one buffer for every row's message)
        for row in &out.rows[which] {
            b.clear();
            b.extend_from_slice(&(row.len() as i16).to_be_bytes());
            for (i, v) in row.iter().enumerate() {
                let ty = cols.get(i).map_or(Type::Text, |c| c.1);
                match encode(v, ty, format(formats, i), &|k, v| self.session.reg_array_oids(self.db, k, v)) {
                    None => b.extend_from_slice(&(-1i32).to_be_bytes()),
                    Some(mut v) => {
                        // (a varchar[]'s or char(n)[]'s elements, in binary, are of their own type, as its header says)
                        let elem = match out.typmods.get(i).copied().flatten() {
                            Some((1015, _)) => Some(1043u32),
                            Some((1014, _)) => Some(1042),
                            _ => None,
                        };
                        if let (Some(e), 1, true) =
                            (elem, format(formats, i), v.get(8..12) == Some(&25u32.to_be_bytes()[..]))
                        {
                            v[8..12].copy_from_slice(&e.to_be_bytes());
                        }
                        b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                        b.extend_from_slice(&v);
                    }
                }
            }
            self.send(b'D', &b)?;
        }
        Ok(())
    }

    /// CommandComplete.
    fn complete(&mut self, tag: &str) -> io::Result<()> {
        let mut b = vec![];
        cstr(&mut b, tag);
        self.send(b'C', &b)
    }
}

/// A relation kind (pg_class.relkind) as psql's lists name it.
fn kind_name(kind: char) -> &'static str {
    match kind {
        'v' => "view",
        'm' => "materialized view",
        'S' => "sequence",
        'p' => "partitioned table",
        'f' => "foreign table",
        'i' => "index",
        'I' => "partitioned index",
        _ => "table",
    }
}

/// psycopg's type lookup (`TypeInfo.fetch`, which pgvector's `register_vector` runs for `vector`
/// and `bit` so it can send vectors in binary), answered for the types Chronos has; any other name
/// finds nothing, which clients take as "no such type".
fn type_lookup(q: &str) -> Option<String> {
    if q.contains("FROM pg_type t") && q.contains("typarray AS array_oid") {
        return Some(
            "select 'vector' as name, 90000 as oid, 0 as array_oid, 'vector' as regtype, ',' as delimiter where $1 = 'vector' \
             union all select 'bit', 1560, 1561, 'bit', ',' where $1 = 'bit'"
                .into(),
        );
    }
    // asyncpg asks about every array type id it sees, for the element's type and delimiter (its query is a
    // recursive one over pg_type); the array types are the ones it needs answered
    if q.contains("typeinfo_tree") {
        let arrays: Vec<String> = sql::array_types()
            .iter()
            .map(|(oid, name, elem, elem_name)| format!("({oid}, '{name}', {elem}, '{elem_name}')"))
            .collect();
        return Some(format!(
            "select t.oid as oid, 'pg_catalog' as ns, t.name as name, 'b' as kind, null as basetype, t.elem as elemtype, \
             ',' as elemdelim, null as range_subtype, null as attrtypoids, null as attrnames, 0 as depth, \
             null as basetype_name, t.elem_name as elemtype_name, null as range_subtype_name \
             from (values {}) as t(oid, name, elem, elem_name) where t.oid = any($1)",
            arrays.join(", ")
        ));
    }
    None
}

/// Postgres's binary dates count from 2000-01-01, not 1970.
const EPOCH_DAYS: i64 = 10_957;
const EPOCH_MICROS: i64 = EPOCH_DAYS * 86_400_000_000;

/// A parameter from the wire. Text is parsed as the parameter's type; binary is read by size.
fn decode(b: &[u8], format: i16, ty: Type) -> Result<Value, Fail> {
    let text = || String::from_utf8(b.to_vec()).map_err(|_| protocol("text parameters must be UTF-8"));
    if format == 0 {
        return Ok(sql::coerce(Value::Text(text()?), ty)?);
    }
    let bad = || protocol(format!("can't read a {}-byte binary {} parameter", b.len(), ty.name()));
    Ok(match ty {
        // (a smallint's range is checked: one sent wider holds what fits)
        Type::SmallInt | Type::Int | Type::BigInt => sql::coerce(
            Value::Int(match b.len() {
                2 => i16::from_be_bytes(b.try_into().unwrap()) as i64,
                4 => i32::from_be_bytes(b.try_into().unwrap()) as i64,
                8 => i64::from_be_bytes(b.try_into().unwrap()),
                _ => return Err(bad()),
            }),
            ty,
        )?,
        // an oid's binary form is its four unsigned bytes
        Type::Oid if b.len() == 4 => Value::Int(i64::from(u32::from_be_bytes(b.try_into().unwrap()))),
        Type::Oid if b.len() == 8 => sql::coerce(Value::Int(i64::from_be_bytes(b.try_into().unwrap())), ty)?,
        Type::Oid => return Err(bad()),
        Type::Float | Type::Float4 => Value::Float(match b.len() {
            4 => f32::from_be_bytes(b.try_into().unwrap()) as f64,
            8 => f64::from_be_bytes(b.try_into().unwrap()),
            _ => return Err(bad()),
        }),
        Type::Bool if b.len() == 1 => Value::Bool(b[0] != 0),
        Type::Bool => return Err(bad()),
        Type::Text => Value::Text(text()?),
        // void's binary form is nothing
        Type::Void | Type::Poly(_) => Value::Text(String::new()),
        // (a base type made with CREATE TYPE is sent as text)
        Type::Base(_) => return Err(bad()),
        // jsonb's binary form is a version byte, 1, then the text
        Type::Json => sql::coerce(Value::Text(String::from_utf8_lossy(b.strip_prefix(&[1]).unwrap_or(b)).into()), ty)?,
        // (infinity and -infinity are the largest and smallest values, on the wire as in memory)
        Type::Date if b.len() == 4 => {
            let d = i32::from_be_bytes(b.try_into().unwrap());
            Value::Date(if d == i32::MAX || d == i32::MIN { d } else { (d as i64 + EPOCH_DAYS) as i32 })
        }
        Type::Timestamp | Type::TimestampTz if b.len() == 8 => {
            let t = i64::from_be_bytes(b.try_into().unwrap());
            let t = if t == i64::MAX || t == i64::MIN { t } else { t + EPOCH_MICROS };
            if ty == Type::Timestamp { Value::Timestamp(t) } else { Value::TimestampTz(t) }
        }
        Type::Interval if b.len() == 16 => Value::Interval(sql::Interval {
            micros: i64::from_be_bytes(b[..8].try_into().unwrap()),
            days: i32::from_be_bytes(b[8..12].try_into().unwrap()),
            months: i32::from_be_bytes(b[12..].try_into().unwrap()),
        }),
        Type::Numeric => Value::Numeric(numeric_from_wire(b).ok_or_else(bad)?),
        Type::Array(_) => Value::Text(array_from_wire(b)?.ok_or_else(bad)?),
        Type::Vector => sql::vector_of(vector_from_wire(b).ok_or_else(bad)?)?,
        Type::TsVector | Type::TsQuery => return Err(protocol(format!("send {} parameters as text", ty.name()))),
        // an hstore in binary is its pairs, counted; an ltree or an lquery, a version byte (1) and then its text
        Type::Hstore => sql::coerce(Value::Text(sql::hstore_from_wire(b).ok_or_else(bad)?), ty)?,
        Type::Ltree | Type::Lquery => match b.split_first() {
            Some((1, rest)) => sql::coerce(Value::Text(String::from_utf8(rest.to_vec()).map_err(|_| bad())?), ty)?,
            _ => return Err(bad()),
        },
        // geometric types, geometry, geography, enums, records: binary form is text
        Type::Geometry
        | Type::Geography
        | Type::Point
        | Type::Lseg
        | Type::Path
        | Type::Box
        | Type::Polygon
        | Type::Circle
        | Type::Line
        | Type::Money
        | Type::PgLsn
        | Type::Tid
        | Type::Xid
        | Type::Xml
        | Type::JsonPath
        | Type::JsonText
        | Type::Bit
        | Type::VarBit
        | Type::TimeTz
        | Type::Name
        | Type::Letter
        | Type::Xid8
        | Type::PgSnapshot
        | Type::Enum(_)
        | Type::Range(_)
        | Type::Multirange(_)
        | Type::Record(_) => sql::coerce(Value::Text(text()?), ty)?,
        // bytea's binary form is its bytes, uuid's its 16
        Type::Bytea => Value::Bytea(b.to_vec()),
        Type::Uuid if b.len() == 16 => {
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            Value::Text(format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))
        }
        Type::Uuid => return Err(bad()),
        // a reg value's binary form is its object's oid
        Type::Reg(r) if b.len() == 4 => Value::Reg(i64::from(u32::from_be_bytes(b.try_into().unwrap())), r),
        Type::Reg(_) => return Err(bad()),
        // inet, cidr, macaddr and macaddr8: as their _send functions write them
        Type::Inet | Type::Cidr | Type::MacAddr | Type::MacAddr8 => sql::net_from_wire(ty, b).ok_or_else(bad)??,
        // time: microseconds since midnight
        Type::Time if b.len() == 8 => Value::Time(i64::from_be_bytes(b.try_into().unwrap())),
        Type::Date | Type::Timestamp | Type::TimestampTz | Type::Interval | Type::Time => return Err(bad()),
    })
}

/// A vector's binary form: dimensions, a zero, then float4s.
fn vector_from_wire(b: &[u8]) -> Option<Vec<f32>> {
    let dim = u16::from_be_bytes(b.get(..2)?.try_into().ok()?) as usize;
    let xs = b.get(4..)?;
    (xs.len() == dim * 4).then(|| xs.chunks_exact(4).map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect())
}

/// numeric's binary form: digit count, weight (of the first base-10000 group), sign, display
/// scale, then the groups; NaN and the infinities are their sign word alone (0xC000, 0xD000, 0xF000).
fn numeric_to_wire(d: &sql::decimal::Dec) -> Vec<u8> {
    let (groups, weight, sign, scale) = match d.groups() {
        Some((groups, weight)) => {
            let sign: u16 = if d.signum() < 0 { 0x4000 } else { 0 };
            (groups, weight as i16, sign, d.scale() as i16)
        }
        None if d.is_nan() => (vec![], 0, 0xC000, 0),
        None if d.signum() > 0 => (vec![], 0, 0xD000, 0),
        None => (vec![], 0, 0xF000, 0),
    };
    let mut out = vec![];
    for x in [groups.len() as i16, weight, sign as i16, scale].into_iter().chain(groups) {
        out.extend(x.to_be_bytes());
    }
    out
}

fn numeric_from_wire(b: &[u8]) -> Option<sql::decimal::Dec> {
    let word = |i: usize| b.get(i * 2..i * 2 + 2).map(|w| i16::from_be_bytes([w[0], w[1]]));
    let (n, weight, sign, scale) = (word(0)?, word(1)?, word(2)? as u16, word(3)?);
    match sign {
        0xC000 => return Some(sql::decimal::Dec::NAN),
        0xD000 => return Some(sql::decimal::Dec::INF),
        0xF000 => return Some(sql::decimal::Dec::NEG_INF),
        0 | 0x4000 => {}
        _ => return None,
    }
    let groups = (0..n.max(0) as usize).map(|i| word(4 + i)).collect::<Option<Vec<i16>>>()?;
    sql::decimal::Dec::from_groups(&groups, i32::from(weight), sign == 0x4000, scale.max(0) as u32)
}

/// A binary array parameter (one dimension) as Postgres array text: dimensions, a null flag, the
/// element type, each dimension's length and lower bound, then each element, length first.
/// None for a malformed array.
fn array_from_wire(b: &[u8]) -> Result<Option<String>, Fail> {
    let int = |i: usize| b.get(i..i + 4).map(|w| i32::from_be_bytes(w.try_into().unwrap()));
    let (Some(ndim), Some(elem)) = (int(0), int(8)) else { return Ok(None) };
    if ndim == 0 {
        return Ok(Some("{}".into()));
    }
    if ndim != 1 {
        return Ok(None); // ponytail: one dimension, as x = ANY($1) takes
    }
    let Some(ty) = Type::from_oid(elem as u32) else { return Ok(None) };
    // an element that is itself an array would recurse once per level the client sends
    if matches!(ty, Type::Array(_)) {
        return Err(Fail::Sql(SqlError {
            code: "22P03",
            message: "binary array elements can't be arrays".into(),
            info: None,
        }));
    }
    let (Some(n), mut at) = (int(12), 20) else { return Ok(None) };
    let mut vals = vec![];
    for _ in 0..n {
        let Some(len) = int(at) else { return Ok(None) };
        at += 4;
        if len < 0 {
            vals.push(Value::Null);
            continue;
        }
        let Some(x) = b.get(at..at + len as usize) else { return Ok(None) };
        let Ok(v) = decode(x, 1, ty) else { return Ok(None) };
        // (a reg array's elements are kept as their oids)
        vals.push(match v {
            Value::Reg(o, _) => Value::Int(o),
            v => v,
        });
        at += len as usize;
    }
    Ok(Some(sql::array_literal(&vals)))
}

/// A one-dimension array result in binary, as Postgres sends it: the dimensions (0 for an empty
/// array), a flag for nulls, the element type, the length and lower bound of the one dimension,
/// then each element after its length (-1 for a null). None for what can't be: an element that is
/// an array, or a text that isn't one.
fn array_to_wire(text: &str, elem_oid: u32) -> Option<Vec<u8>> {
    // an int2vector is an array of int2 and an oidvector one of oid, each counted from 0
    let (elem_oid, lower) = match elem_oid {
        22 => (21, 0),
        30 => (26, 0),
        o => (o, 1),
    };
    // (a reg array comes here with any names made oids, see `encode`; a "char" is its letter)
    let elem = match elem_oid {
        18 => Type::Text,
        o => Type::from_oid(o)?,
    };
    if matches!(elem, Type::Array(_)) {
        return None;
    }
    let items = sql::array_items(Value::Text(text.into())).ok()??;
    let (mut body, mut has_null) = (vec![], 0i32);
    for item in &items {
        match item {
            Value::Null => {
                has_null = 1;
                body.extend((-1i32).to_be_bytes());
            }
            v => {
                // the elements are text: each becomes its type's value before it's written
                let mut bytes = encode(&sql::coerce(v.clone(), elem).ok()?, elem, 1, &|_, _| None)?;
                // (an int2 is two bytes, where the integer they are read as is four; an oid is four
                // where a bigint is eight, a float4 four where a double is eight)
                match elem_oid {
                    21 => bytes = bytes.split_off(bytes.len().saturating_sub(2)),
                    26 => bytes = bytes.split_off(bytes.len().saturating_sub(4)),
                    700 => {
                        let f = f64::from_be_bytes(bytes.as_slice().try_into().ok()?);
                        bytes = (f as f32).to_be_bytes().to_vec();
                    }
                    _ => {}
                }
                body.extend((bytes.len() as i32).to_be_bytes());
                body.extend(bytes);
            }
        }
    }
    let head: &[i32] = if items.is_empty() {
        &[0, 0, elem_oid as i32]
    } else {
        &[1, has_null, elem_oid as i32, items.len() as i32, lower]
    };
    Some(head.iter().flat_map(|x| x.to_be_bytes()).chain(body).collect())
}

/// A result value for the wire, or None for NULL. `regs` gives a reg array's elements as the oids its binary form
/// holds, any names among them read (see `Session::reg_array_oids`).
fn encode(v: &Value, ty: Type, format: i16, regs: &dyn Fn(Reg, &Value) -> Option<Value>) -> Option<Vec<u8>> {
    Some(match (v, format) {
        (Value::Null, _) => return None,
        // a record, in binary, as Postgres sends a composite: the number of fields, then for each its type id and
        // its length (-1: null) and bytes; the fields are read out of the composite text
        (Value::Text(t), 1) if matches!(ty, Type::Record(_)) => {
            let fields = ty.fields()?;
            let texts = sql::composite_fields(t)?;
            let mut out = (fields.len() as i32).to_be_bytes().to_vec();
            for ((_, fty), text) in fields.iter().zip(texts) {
                out.extend(fty.oid().to_be_bytes());
                let bytes =
                    text.and_then(|x| sql::coerce(Value::Text(x), *fty).ok()).and_then(|v| encode(&v, *fty, 1, regs));
                match bytes {
                    Some(b) => {
                        out.extend((b.len() as i32).to_be_bytes());
                        out.extend(b);
                    }
                    None => out.extend((-1i32).to_be_bytes()),
                }
            }
            out
        }
        // a reg array, in binary: its oids, never its names' text (a value that isn't an array goes as a null)
        (Value::Text(_), 1) if ty.reg_array().is_some() => {
            let kind = ty.reg_array()?;
            array_to_wire(&regs(kind, v)?.to_text(), kind.oid())?
        }
        // a reg value, in binary: its object's oid
        (Value::Int(o), 1) if matches!(ty, Type::Reg(_)) => (*o as u32).to_be_bytes().to_vec(),
        (Value::Reg(o, _), 1) => (*o as u32).to_be_bytes().to_vec(),
        // an array, in binary: arrays are text values here, so its elements are read out of that
        (Value::Text(t), 1) if matches!(ty, Type::Array(_)) => {
            let Type::Array(elem) = ty else { unreachable!() };
            array_to_wire(t, elem).unwrap_or_else(|| t.clone().into_bytes())
        }
        // vectors in binary as pgvector sends them: dimensions, a zero, then float4s
        (Value::Vector(v), 1) => {
            let mut out = [(v.len() as i16).to_be_bytes(), 0i16.to_be_bytes()].concat();
            for x in v.iter() {
                out.extend(x.to_be_bytes());
            }
            out
        }
        (Value::Bool(b), 0) => (if *b { "t" } else { "f" }).into(),
        (Value::Bytea(b), 1) => b.clone(),
        (Value::Net(_), 1) => sql::net_to_wire(v)?,
        (Value::Text(t), 1) if ty == Type::Uuid => {
            let hex: Vec<u8> = t.bytes().filter(|c| *c != b'-').collect();
            let nibble = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            hex.chunks(2).map(|p| nibble(p[0]) << 4 | p.get(1).map_or(0, |c| nibble(*c))).collect()
        }
        (Value::Float(f), 0) if ty == Type::Float4 => sql::fmt_float4(*f).into_bytes(),
        (Value::Text(t), 0) if ty == Type::Xml => sql::xml_out(t).into_bytes(),
        (v, 0) => v.to_text().into_bytes(),
        (Value::Int(n), _) if ty == Type::Int => (*n as i32).to_be_bytes().to_vec(),
        (Value::Int(n), _) if ty == Type::SmallInt => (*n as i16).to_be_bytes().to_vec(),
        (Value::Int(n), _) if ty == Type::Oid => (*n as u32).to_be_bytes().to_vec(),
        (Value::Int(n), _) => n.to_be_bytes().to_vec(),
        (Value::Float(f), _) if ty == Type::Float4 => (*f as f32).to_be_bytes().to_vec(),
        (Value::Float(f), _) => f.to_be_bytes().to_vec(),
        (Value::Bool(b), _) => vec![*b as u8],
        // (json's binary form is its text; jsonb's a version byte, 1, then the text)
        (Value::Json(_), _) if ty == Type::JsonText => v.to_text().into_bytes(),
        (Value::Json(j), _) => [&[1u8][..], j.to_string().as_bytes()].concat(),
        (Value::Date(d), _) if *d == i32::MAX || *d == i32::MIN => d.to_be_bytes().to_vec(),
        (Value::Date(d), _) => ((*d as i64 - EPOCH_DAYS) as i32).to_be_bytes().to_vec(),
        (Value::Timestamp(t) | Value::TimestampTz(t), _) if *t == i64::MAX || *t == i64::MIN => {
            t.to_be_bytes().to_vec()
        }
        (Value::Timestamp(t) | Value::TimestampTz(t), _) => (t - EPOCH_MICROS).to_be_bytes().to_vec(),
        (Value::Time(t), _) => t.to_be_bytes().to_vec(),
        (Value::Numeric(d), _) => numeric_to_wire(d),
        (Value::Text(t), 1) if ty == Type::Hstore => sql::hstore_to_wire(t)?,
        (Value::Text(t), 1) if matches!(ty, Type::Ltree | Type::Lquery) => [&[1u8][..], t.as_bytes()].concat(),
        (Value::Text(t), _) if ty == Type::TsVector => sql::tsearch::TsVector::parse(t).ok()?.to_wire(),
        (Value::Text(t), _) if ty == Type::TsQuery => {
            sql::tsearch::Query::to_wire(&sql::tsearch::parse_tsquery(t).unwrap_or(None))
        }
        (Value::Interval(i), _) => {
            [&i.micros.to_be_bytes()[..], &i.days.to_be_bytes(), &i.months.to_be_bytes()].concat()
        }
        (v, _) => v.to_text().into_bytes(),
    })
}

#[cfg(test)]
mod numeric_wire {
    use chronos::sql::decimal::Dec;

    #[test]
    fn round_trips_as_postgres_sends_it() {
        for s in ["0", "1", "-1.5", "12345.6789", "0.001", "100000000", "-0.00005", "99999999999999999999.99", "3.00"] {
            let d = Dec::parse(s).unwrap();
            let back = super::numeric_from_wire(&super::numeric_to_wire(&d)).unwrap();
            assert_eq!(back.to_string(), d.to_string(), "{s}");
        }
        // 12345.6789 as Postgres writes it: 3 groups, weight 1, positive, scale 4: 1 2345 6789
        let wire = super::numeric_to_wire(&Dec::parse("12345.6789").unwrap());
        let words: Vec<i16> = wire.chunks(2).map(|w| i16::from_be_bytes([w[0], w[1]])).collect();
        assert_eq!(words, [3, 1, 0, 4, 1, 2345, 6789]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(server: Option<&str>, admin: Option<&str>) -> Tokens {
        let auth = crate::Auth {
            token: server.map(String::from),
            admin: admin.map(String::from),
            mode: crate::Mode::Default,
            studio_api: false,
        };
        Tokens::new(&auth).unwrap()
    }

    /// A name's token verifiers are made each time it logs in with a token, never kept (a kept one
    /// would make the login quicker, telling who logged in lately), and the last step takes `pad`
    /// at least whether it makes them or not.
    #[test]
    fn the_last_step_takes_the_same_time_for_every_name() {
        let t = tokens(Some("s3cret"), Some("ADMIN"));
        assert!(t.pad > std::time::Duration::ZERO);
        let made = |n: usize| t.makes.load(Ordering::Relaxed) == n;
        assert!(made(1), "once when the server starts, to time it");
        for (i, name) in ["alice", "alice", "bob"].into_iter().enumerate() {
            let began = Instant::now();
            let v = t.last_step(name, true).unwrap();
            assert!(began.elapsed() >= t.pad);
            assert!(made(2 + i), "made again for {name}");
            let salt = scram::mock_salt(name, &t.secret);
            let want =
                ["ADMIN", "s3cret"].map(|k| scram::parse(&scram::verifier(k, &salt, scram::ITERATIONS)).unwrap());
            assert_eq!(v, want);
        }
        // a name that doesn't log in with a token makes none, and waits as long
        let began = Instant::now();
        assert_eq!(t.last_step("carol", false), None);
        assert!(began.elapsed() >= t.pad && made(4));
        // without a server token there is nothing to wait for
        assert_eq!(tokens(None, Some("ADMIN")).pad, std::time::Duration::ZERO);
    }

    /// Clients trying many names make the server derive a few at a time, not one per client at once.
    #[test]
    fn few_token_verifiers_are_made_at_once() {
        let t = std::sync::Arc::new(tokens(Some("s3cret"), Some("ADMIN")));
        let clients: Vec<_> = (0..t.permits * 3 + 2)
            .map(|i| {
                let t = t.clone();
                std::thread::spawn(move || t.derive(&format!("name{i}")).len())
            })
            .collect();
        for c in clients {
            assert_eq!(c.join().unwrap(), 2);
        }
        // counted where they start, not watched from a thread: on a busy machine a watcher missed them all
        let most = t.most.load(Ordering::Relaxed);
        assert!((1..=t.permits).contains(&most), "{most} at once, {} allowed", t.permits);
    }
}
