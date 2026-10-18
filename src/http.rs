//! A small HTTP/1.1 server for `api`: requests with a Content-Length body, keep-alive, and
//! `Expect: 100-continue`, over plain TCP or TLS (see `tls`). One thread per connection, at most
//! `max_connections()` at once. A request is admitted from its headers before its body is read, and must
//! arrive (headers and body) within `REQUEST` of its first byte.
//! CHRONOS_MAX_CONNECTIONS and CHRONOS_REQUEST_TIMEOUT (seconds) change those two.
//! ponytail: no chunked request bodies (clients send Content-Length for JSON).

use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::cell::Cell;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

const MAX_HEAD: usize = 64 << 10;
const IDLE: Duration = Duration::from_secs(60);
/// How long a request's headers and body may take to arrive, from its first byte.
static REQUEST: LazyLock<Duration> = LazyLock::new(|| Duration::from_secs(env("CHRONOS_REQUEST_TIMEOUT", 120)));
/// A number from the environment variable `name`, or `default`.
pub fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
/// A connection thread's stack: deep SQL recurses, and address space is cheap.
pub const STACK: usize = 16 << 20;

pub struct Request {
    pub method: String,
    pub path: String, // without the query string
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// the client's address (what failed logins are counted by: see `crate::backoff`)
    pub peer: Option<std::net::IpAddr>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// header lines of this response alone (a Set-Cookie), each ending in \r\n
    pub headers: String,
}

/// Connections live at once, across every listener (HTTP and Postgres): `max_connections()` is
/// what the process's descriptors hold, so the listeners share it rather than each having it.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Connections open now, across every listener (HTTP and Postgres).
pub fn live() -> usize {
    LIVE.load(Ordering::SeqCst)
}

/// Runs `f` for each connection accepted on `l`, each on its own thread; a connection past
/// `max_connections()` (counted across all listeners) goes to `refuse` instead. Accept errors (out of file descriptors, say) are logged
/// and retried after a pause. Runs until the process ends.
pub fn connections<F: Fn(TcpStream) + Sync>(l: &TcpListener, refuse: impl Fn(TcpStream), f: F) {
    chronos::limits::prepare_descriptors();
    let max = chronos::limits::max_connections();
    let live = &LIVE;
    std::thread::scope(|s| {
        loop {
            let c = match l.accept() {
                Ok((c, _)) => c,
                Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionAborted | io::ErrorKind::Interrupted) => continue,
                Err(e) => {
                    eprintln!("chronos: accepting a connection: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            if live.fetch_add(1, Ordering::SeqCst) >= max {
                live.fetch_sub(1, Ordering::SeqCst);
                refuse(c);
                continue;
            }
            let f = &f;
            let spawned = std::thread::Builder::new().stack_size(STACK).spawn_scoped(s, move || {
                let _slot = Release(live);
                guarded(|| f(c));
            });
            if let Err(e) = spawned {
                live.fetch_sub(1, Ordering::SeqCst); // the connection closed with the closure
                eprintln!("chronos: starting a connection's thread: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    })
}

/// Runs one connection's work: a panic ends only that connection (the panic hook has printed it).
pub fn guarded(f: impl FnOnce()) {
    if std::panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
        // not eprintln!, which panics when stderr is gone
        let _ = writeln!(io::stderr(), "chronos: a connection hit an internal error and was closed");
    }
}

/// A connection's slot in a count of live ones, given back however the connection ends.
pub struct Release<'a>(pub &'a AtomicUsize);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serves connections from `l` forever. `admit` sees each request's headers before its body is
/// read: an error reply is sent and the connection closed; otherwise `handle` gets the request
/// with its body and what `admit` returned. `extra` (header lines, each ending in \r\n) goes on
/// every response, errors included.
pub fn serve<A, G, F>(
    l: &TcpListener,
    tls: Option<Arc<ServerConfig>>,
    max_body: usize,
    extra: &str,
    admit: &G,
    handle: &F,
) where
    G: Fn(&Request) -> Result<A, Response> + Sync,
    F: Fn(&Request, A) -> Response + Sync,
{
    let busy = |mut c: TcpStream| {
        let res = Response {
            status: 503,
            content_type: "application/json",
            body: r#"{"error": "too many connections"}"#.into(),
            headers: String::new(),
        };
        let _ = write(&mut c, &res, extra, false);
    };
    connections(l, busy, |c| {
        let _ = c.set_nodelay(true);
        let _ = c.set_write_timeout(Some(IDLE));
        let peer = c.peer_addr().ok().map(|a| a.ip());
        let deadline = Rc::new(Cell::new(None));
        let c = Timed { s: c, deadline: deadline.clone() };
        let _ = match tls.clone() {
            Some(cfg) => match ServerConnection::new(cfg) {
                Ok(conn) => {
                    // the handshake gets a request's time, from accept: a client that stalls in it is dropped
                    deadline.set(Instant::now().checked_add(*REQUEST)); // too far off to represent: none
                    let mut s = StreamOwned::new(conn, c);
                    while s.conn.is_handshaking() {
                        if s.conn.complete_io(&mut s.sock).is_err() {
                            return;
                        }
                    }
                    connection(BufReader::new(s), &deadline, peer, max_body, extra, admit, handle)
                }
                Err(_) => Ok(()),
            },
            None => connection(BufReader::new(c), &deadline, peer, max_body, extra, admit, handle),
        };
    })
}

/// A socket whose reads wait at most `IDLE`, and fail once the current request's deadline passes.
struct Timed {
    s: TcpStream,
    deadline: Rc<Cell<Option<Instant>>>,
}

impl Read for Timed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let wait = self.deadline.get().map_or(IDLE, |d| d.saturating_duration_since(Instant::now()).min(IDLE));
        if wait.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.s.set_read_timeout(Some(wait))?;
        self.s.read(buf)
    }
}

impl Write for Timed {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.s.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.s.flush()
    }
}

/// Requests on one connection, one after another, until the client closes it or asks to.
fn connection<S: Read + Write, A, G, F>(
    mut r: BufReader<S>,
    deadline: &Cell<Option<Instant>>,
    peer: Option<std::net::IpAddr>,
    max_body: usize,
    extra: &str,
    admit: &G,
    handle: &F,
) -> io::Result<()>
where
    G: Fn(&Request) -> Result<A, Response>,
    F: Fn(&Request, A) -> Response,
{
    loop {
        deadline.set(None); // between requests, the connection may idle
        let (mut req, len, keep) = match read(&mut r, deadline, max_body)? {
            Next::Closed => return Ok(()),
            Next::Bad(status, msg) => {
                let res = Response {
                    status,
                    content_type: "application/json",
                    body: format!("{{\"error\": {msg:?}}}").into(),
                    headers: String::new(),
                };
                return write(r.get_mut(), &res, extra, false);
            }
            Next::Req(req, len, keep) => (Request { peer, ..req }, len, keep),
        };
        // the body isn't read for a request refused on its headers, so the connection can't go on
        let who = match admit(&req) {
            Ok(who) => who,
            Err(res) => return write(r.get_mut(), &res, extra, false),
        };
        if len > 0 && req.header("Expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
            r.get_mut().write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            r.get_mut().flush()?;
        }
        // grows as the bytes arrive: a Content-Length alone doesn't get its size allocated
        if r.by_ref().take(len as u64).read_to_end(&mut req.body)? < len {
            return Ok(()); // the client went away mid-body
        }
        deadline.set(None);
        let res = handle(&req, who);
        write(r.get_mut(), &res, extra, keep)?;
        if !keep {
            return Ok(());
        }
    }
}

enum Next {
    Closed,
    Bad(u16, &'static str),
    Req(Request, usize, bool), // its body's length (still to read), and whether to keep the connection open after it
}

/// A request's headers; its body is left to read.
fn read<S: Read + Write>(r: &mut BufReader<S>, deadline: &Cell<Option<Instant>>, max_body: usize) -> io::Result<Next> {
    let timeout = |e: &io::Error| matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut);
    match r.fill_buf() {
        Ok([]) => return Ok(Next::Closed),
        Ok(_) => deadline.set(Instant::now().checked_add(*REQUEST)), // the request has begun
        Err(e) if timeout(&e) || e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Next::Closed),
        Err(e) => return Err(e),
    }
    let mut head = vec![];
    loop {
        let n = match r.by_ref().take((MAX_HEAD - head.len()) as u64).read_until(b'\n', &mut head) {
            Ok(n) => n,
            Err(e) if timeout(&e) => return Ok(Next::Closed),
            Err(e) if head.is_empty() && e.kind() == io::ErrorKind::UnexpectedEof => return Ok(Next::Closed),
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(if head.is_empty() { Next::Closed } else { Next::Bad(400, "incomplete request") });
        }
        if head.len() >= MAX_HEAD {
            return Ok(Next::Bad(431, "request headers are too large"));
        }
        if head == b"\r\n" || head == b"\n" {
            head.clear(); // blank lines before a request are allowed
        } else if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
    }
    let Ok(head) = String::from_utf8(head) else { return Ok(Next::Bad(400, "headers must be UTF-8")) };
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version)) = (first.next(), first.next(), first.next()) else {
        return Ok(Next::Bad(400, "bad request line"));
    };
    // strict, so a proxy in front can't read the headers differently (request smuggling): no
    // space before the colon, no folded lines, no line without a colon
    let mut headers = vec![];
    for l in lines.filter(|l| !l.is_empty()) {
        match l.split_once(':') {
            Some((k, v)) if !k.is_empty() && !k.ends_with([' ', '\t']) && !l.starts_with([' ', '\t']) => {
                headers.push((k.to_string(), v.trim().to_string()))
            }
            _ => return Ok(Next::Bad(400, "bad header line")),
        }
    }
    let req = Request {
        method: method.to_string(),
        path: target.split('?').next().unwrap_or("").to_string(),
        headers,
        body: vec![],
        peer: None,
    };
    if req.header("Transfer-Encoding").is_some() {
        return Ok(Next::Bad(411, "send the body with a Content-Length"));
    }
    let mut lengths = req.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("Content-Length")).map(|(_, v)| v);
    let first = lengths.next();
    if lengths.any(|v| Some(v) != first) {
        return Ok(Next::Bad(400, "conflicting Content-Length headers"));
    }
    let len = match first {
        None => 0,
        Some(v) if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) => {
            return Ok(Next::Bad(400, "bad Content-Length"));
        }
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n <= max_body => n,
            _ => return Ok(Next::Bad(413, "the body is too large")),
        },
    };
    let conn = req.header("Connection").map(str::to_ascii_lowercase);
    let keep = match version {
        "HTTP/1.1" => conn.as_deref() != Some("close"),
        _ => conn.as_deref() == Some("keep-alive"),
    };
    Ok(Next::Req(req, len, keep))
}

fn write<W: Write>(w: &mut W, res: &Response, extra: &str, keep: bool) -> io::Result<()> {
    let reason = match res.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n{extra}{}\r\n",
        res.status,
        res.content_type,
        res.body.len(),
        if keep { "keep-alive" } else { "close" },
        res.headers
    );
    w.write_all(head.as_bytes())?;
    w.write_all(&res.body)?;
    w.flush()
}

#[cfg(test)]
mod panics {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// A connection that panics is closed, and the others (HTTP's and Postgres's both run on
    /// `connections`) are served on, before and after it.
    #[test]
    fn a_panic_ends_only_its_connection() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            super::connections(&l, drop, |mut c| {
                let mut b = [0u8; 1];
                while c.read_exact(&mut b).is_ok() {
                    assert!(b[0] != b'!', "a bug in serving this connection");
                    c.write_all(&b).unwrap();
                }
            })
        });
        let echo = |c: &mut TcpStream, b: u8| {
            c.write_all(&[b]).unwrap();
            let mut got = [0u8; 1];
            c.read_exact(&mut got).map(|()| got[0])
        };
        let mut before = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert_eq!(echo(&mut before, b'a').unwrap(), b'a');
        let mut bad = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(echo(&mut bad, b'!').is_err(), "the panicking connection is closed");
        assert_eq!(echo(&mut before, b'b').unwrap(), b'b');
        let mut after = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert_eq!(echo(&mut after, b'c').unwrap(), b'c');
    }
}
