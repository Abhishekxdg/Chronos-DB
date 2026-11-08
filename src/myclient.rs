//! A MySQL client, just enough for `chronos attach` beside a MySQL database (see `attach::mysql`): connect over TCP
//! (TLS as MySQL's `ssl-mode` says), log in (`mysql_native_password`, `caching_sha2_password`, and over TLS the
//! plugins that send the password itself), and run statements with text results, several in one round trip
//! (`CLIENT_MULTI_STATEMENTS`), each statement's rows and affected-row count given back.
//!
//!   connect ─> Handshake ─> [SSLRequest ─> TLS] ─> HandshakeResponse ─> (AuthSwitch | AuthMoreData)* ─> OK
//!   query ───> COM_QUERY ─> per statement: OK | ERR | column count, column definitions, rows, OK
//!
//! Values are never bound: the SQL carries them, as `lit` writes them (hex, so no quote or backslash in a value can
//! end it, whatever `sql_mode` says).

use crate::pgclient::{self, Stream};
use ring::digest;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::sync::Arc;

/// Where to connect, from `mysql://user:password@host:port/database?ssl-mode=...&ssl-ca=...`.
#[derive(Clone, Debug, PartialEq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub database: String,
    /// as Postgres's sslmode names them (see `parse`): disable, prefer (the default), require, verify-ca, verify-full
    pub sslmode: String,
    pub ssl_ca: Option<String>,
    pub connect_timeout: u64,
}

/// A server's ERR packet.
#[derive(Debug)]
pub struct MyError {
    pub code: u16,
    pub state: String,
    pub message: String,
}

impl std::fmt::Display for MyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (MySQL error {}, SQLSTATE {})", self.message, self.code, self.state)
    }
}

impl std::error::Error for MyError {}

/// The server's own error inside `e`, if it is one (else the connection failed).
pub fn my_error(e: &io::Error) -> Option<&MyError> {
    e.get_ref().and_then(|i| i.downcast_ref::<MyError>())
}

pub type Rows = Vec<Vec<Option<String>>>;

/// One statement's outcome: its rows (none for one that returns none), and the rows it changed (matched, for an
/// UPDATE: the connection asks for `CLIENT_FOUND_ROWS`).
#[derive(Debug, Default)]
pub struct Done {
    pub rows: Rows,
    pub affected: u64,
}

const CONNECT_TIMEOUT: u64 = 30;
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// The longest payload taken: MySQL's own max_allowed_packet ceiling (1 GB), and some framing.
const MAX_PAYLOAD: usize = (1 << 30) + (1 << 20);

const LONG_PASSWORD: u32 = 1;
const FOUND_ROWS: u32 = 2;
const LONG_FLAG: u32 = 4;
const CONNECT_WITH_DB: u32 = 8;
const PROTOCOL_41: u32 = 0x200;
const SSL: u32 = 0x800;
const TRANSACTIONS: u32 = 0x2000;
const SECURE_CONNECTION: u32 = 0x8000;
const MULTI_STATEMENTS: u32 = 0x1_0000;
const MULTI_RESULTS: u32 = 0x2_0000;
const PLUGIN_AUTH: u32 = 0x8_0000;
const PLUGIN_AUTH_LENENC: u32 = 0x20_0000;
const DEPRECATE_EOF: u32 = 0x100_0000;

const STATUS_IN_TRANS: u16 = 1;
const MORE_RESULTS: u16 = 8;

/// utf8mb4_general_ci: there in MySQL 5.5 on and every MariaDB
const UTF8MB4: u8 = 45;

fn err(m: impl Into<String>) -> io::Error {
    io::Error::other(m.into())
}

fn unescape(s: &str) -> Result<String, String> {
    let (mut out, b) = (vec![], s.as_bytes());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
            out.push(hex.ok_or_else(|| format!("bad %-escape in {s}"))?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("{s} isn't UTF-8"))
}

/// Whether `url` names a MySQL database (`mysql://` or `mariadb://`).
pub fn is_mysql(url: &str) -> bool {
    url.starts_with("mysql://") || url.starts_with("mariadb://")
}

pub fn parse(url: &str) -> Result<Url, String> {
    let rest = url
        .strip_prefix("mysql://")
        .or_else(|| url.strip_prefix("mariadb://"))
        .ok_or("the URL must start with mysql:// or mariadb://")?;
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (auth, rest) = rest.rsplit_once('@').map_or(("", rest), |(a, r)| (a, r));
    let (hostport, database) = rest.split_once('/').unwrap_or((rest, ""));
    let (user, password) = match auth.split_once(':') {
        Some((u, p)) => (unescape(u)?, Some(unescape(p)?)),
        None => (unescape(auth)?, None),
    };
    let (host, port) = match hostport.rsplit_once(':').filter(|(h, _)| !h.ends_with(']') || h.starts_with('[')) {
        Some((h, p)) if !p.contains(']') => (h, p.parse().map_err(|_| format!("bad port {p}"))?),
        _ => (hostport, 3306),
    };
    let mut u = Url {
        host: host.trim_start_matches('[').trim_end_matches(']').to_string(),
        port,
        user: if user.is_empty() { "root".into() } else { user },
        password: password.or_else(|| std::env::var("MYSQL_PWD").ok()),
        database: unescape(database)?,
        sslmode: "prefer".into(),
        ssl_ca: None,
        connect_timeout: CONNECT_TIMEOUT,
    };
    if u.host.is_empty() {
        u.host = "localhost".into();
    }
    if u.database.is_empty() {
        return Err("the URL must name the database: mysql://user@host:3306/<database>".into());
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = unescape(v)?;
        match k.to_ascii_lowercase().replace('_', "-").as_str() {
            "ssl-mode" | "sslmode" => {
                u.sslmode = match v.to_ascii_uppercase().replace('-', "_").as_str() {
                    "DISABLED" | "DISABLE" => "disable",
                    "PREFERRED" | "PREFER" => "prefer",
                    "REQUIRED" | "REQUIRE" => "require",
                    "VERIFY_CA" => "verify-ca",
                    "VERIFY_IDENTITY" | "VERIFY_FULL" => "verify-full",
                    _ => {
                        return Err(format!(
                            "ssl-mode {v} isn't one of DISABLED, PREFERRED, REQUIRED, VERIFY_CA, VERIFY_IDENTITY"
                        ));
                    }
                }
                .into()
            }
            "ssl-ca" | "sslrootcert" => u.ssl_ca = Some(v),
            "connect-timeout" => u.connect_timeout = v.parse().map_err(|_| format!("bad connect-timeout {v}"))?,
            "password" => u.password = Some(v),
            "user" => u.user = v,
            _ => {}
        }
    }
    Ok(u)
}

/// A value as MySQL's SQL writes it here: NULL, or its UTF-8 bytes in hex made a string of the connection's
/// character set, which MySQL converts to the column's type where it is stored or compared.
pub fn lit(v: Option<&str>) -> String {
    match v {
        None => "NULL".into(),
        Some("") => "''".into(),
        Some(s) => {
            let mut hex = String::with_capacity(s.len() * 2 + 32);
            hex.push_str("convert(X'");
            for b in s.bytes() {
                hex.push_str(&format!("{b:02X}"));
            }
            hex.push_str("' using utf8mb4)");
            hex
        }
    }
}

/// A name as MySQL's SQL writes it.
pub fn q(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MySQL connection ({})", self.version)
    }
}

/// Both halves of one stream.
struct Shared(Arc<std::sync::Mutex<Box<dyn Stream + Send>>>);

impl Read for Shared {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.lock().unwrap().read(b)
    }
}

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// A connection, logged in and ready for statements.
pub struct Client {
    r: BufReader<Box<dyn Read + Send>>,
    w: BufWriter<Box<dyn Write + Send>>,
    seq: u8,
    caps: u32,
    tls: bool,
    /// the server's version, as its handshake says (`8.0.36`, `10.11.6-MariaDB`)
    pub version: String,
    /// a transaction is open, as the server's last status said
    pub in_tx: bool,
    /// a statement failed other than by the server's answer: the connection is good for nothing more
    pub broken: bool,
}

/// A packet's length-encoded integer at `*at`.
fn lenenc(b: &[u8], at: &mut usize) -> io::Result<u64> {
    let bad = || err("the server sent a malformed packet");
    let first = *b.get(*at).ok_or_else(bad)?;
    *at += 1;
    let n = match first {
        0..=0xfa => return Ok(first as u64),
        0xfc => 2,
        0xfd => 3,
        0xfe => 8,
        _ => return Err(bad()),
    };
    let bytes = b.get(*at..*at + n).ok_or_else(bad)?;
    *at += n;
    let mut v = 0u64;
    for (i, x) in bytes.iter().enumerate() {
        v |= (*x as u64) << (8 * i);
    }
    Ok(v)
}

fn put_lenenc(out: &mut Vec<u8>, n: u64) {
    match n {
        0..=0xfa => out.push(n as u8),
        0xfb..=0xffff => {
            out.push(0xfc);
            out.extend(&(n as u16).to_le_bytes());
        }
        0x1_0000..=0xff_ffff => {
            out.push(0xfd);
            out.extend(&(n as u32).to_le_bytes()[..3]);
        }
        _ => {
            out.push(0xfe);
            out.extend(n.to_le_bytes());
        }
    }
}

fn sha1(parts: &[&[u8]]) -> Vec<u8> {
    let mut c = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);
    for p in parts {
        c.update(p);
    }
    c.finish().as_ref().to_vec()
}

fn sha256(parts: &[&[u8]]) -> Vec<u8> {
    let mut c = digest::Context::new(&digest::SHA256);
    for p in parts {
        c.update(p);
    }
    c.finish().as_ref().to_vec()
}

/// What the login plugin `plugin` answers the server's `nonce` with, for `password`. None: a plugin that sends the
/// password itself (over TLS only), or one this doesn't do.
fn scramble(plugin: &str, password: &str, nonce: &[u8]) -> Option<Vec<u8>> {
    if password.is_empty() {
        return Some(vec![]);
    }
    let p = password.as_bytes();
    match plugin {
        // SHA1(password) XOR SHA1(nonce, SHA1(SHA1(password)))
        "mysql_native_password" => {
            let h1 = sha1(&[p]);
            let h2 = sha1(&[&h1]);
            let h3 = sha1(&[nonce, &h2]);
            Some(h1.iter().zip(h3).map(|(a, b)| a ^ b).collect())
        }
        // SHA256(password) XOR SHA256(SHA256(SHA256(password)), nonce)
        "caching_sha2_password" => {
            let h1 = sha256(&[p]);
            let h2 = sha256(&[&h1]);
            let h3 = sha256(&[&h2, nonce]);
            Some(h1.iter().zip(h3).map(|(a, b)| a ^ b).collect())
        }
        _ => None,
    }
}

impl Client {
    pub fn connect(url: &Url) -> io::Result<Client> {
        let addr = format!("{}:{}", url.host, url.port);
        let mut tcp =
            pgclient::connect_tcp(&addr, url.connect_timeout).map_err(|e| err(format!("connecting to {addr}: {e}")))?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(READ_TIMEOUT))?;
        tcp.set_write_timeout(Some(READ_TIMEOUT))?;
        // the handshake, read off the bare socket
        let (seq, hello) = read_raw(&mut tcp)?;
        if hello.first() == Some(&0xff) {
            return Err(io::Error::other(packet_error(&hello)));
        }
        let bad = || err("the server's handshake is malformed (is it MySQL?)");
        if hello.first() != Some(&10) {
            return Err(err("the server doesn't speak MySQL's protocol 10"));
        }
        let mut at = 1;
        let nul = hello[at..].iter().position(|b| *b == 0).ok_or_else(bad)?;
        let version = String::from_utf8_lossy(&hello[at..at + nul]).into_owned();
        at += nul + 1 + 4; // version, connection id
        let mut nonce = hello.get(at..at + 8).ok_or_else(bad)?.to_vec();
        at += 9; // and a filler
        let lo = hello.get(at..at + 2).ok_or_else(bad)?;
        let mut server_caps = u16::from_le_bytes([lo[0], lo[1]]) as u32;
        at += 2 + 1 + 2; // charset, status
        let mut plugin = "mysql_native_password".to_string();
        if let Some(hi) = hello.get(at..at + 2) {
            server_caps |= (u16::from_le_bytes([hi[0], hi[1]]) as u32) << 16;
            at += 2;
            let data_len = *hello.get(at).unwrap_or(&0) as usize;
            at += 1 + 10;
            if server_caps & SECURE_CONNECTION != 0 {
                let n = data_len.saturating_sub(8).max(13);
                if let Some(more) = hello.get(at..at + n) {
                    nonce.extend(more.iter().take_while(|b| **b != 0));
                }
                at += n;
            }
            if server_caps & PLUGIN_AUTH != 0
                && let Some(rest) = hello.get(at..)
            {
                let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
                plugin = String::from_utf8_lossy(&rest[..end]).into_owned();
            }
        }
        if server_caps & PROTOCOL_41 == 0 || server_caps & SECURE_CONNECTION == 0 {
            return Err(err(format!("MySQL {version} is too old: it doesn't speak protocol 4.1")));
        }
        let wanted = LONG_PASSWORD
            | FOUND_ROWS
            | LONG_FLAG
            | CONNECT_WITH_DB
            | PROTOCOL_41
            | TRANSACTIONS
            | SECURE_CONNECTION
            | MULTI_STATEMENTS
            | MULTI_RESULTS
            | PLUGIN_AUTH
            | PLUGIN_AUTH_LENENC
            | DEPRECATE_EOF;
        // (FOUND_ROWS and the MULTI_ flags are the client's to ask: servers take them whatever they advertise)
        let mut caps = wanted & (server_caps | FOUND_ROWS | MULTI_STATEMENTS | MULTI_RESULTS);
        let tls_offered = server_caps & SSL != 0;
        let want_tls = url.sslmode != "disable";
        if want_tls && !tls_offered && url.sslmode != "prefer" {
            return Err(err(format!("{addr} doesn't offer TLS, and ssl-mode needs it")));
        }
        let mut seq = seq.wrapping_add(1);
        let stream: Box<dyn Stream + Send> = if want_tls && tls_offered {
            caps |= SSL;
            let mut req = Vec::with_capacity(32);
            req.extend(caps.to_le_bytes());
            req.extend((1u32 << 24).to_le_bytes());
            req.push(UTF8MB4);
            req.extend([0; 23]);
            write_raw(&mut tcp, seq, &req)?;
            seq = seq.wrapping_add(1);
            let pg_url = pgclient::Url {
                host: url.host.clone(),
                port: url.port,
                user: String::new(),
                password: None,
                database: String::new(),
                sslmode: url.sslmode.clone(),
                sslrootcert: url.ssl_ca.clone(),
                channel_binding: "disable".into(),
                connect_timeout: url.connect_timeout,
                plain_password: false,
            };
            pgclient::tls(&pg_url, tcp)?.0
        } else {
            Box::new(tcp)
        };
        let shared = Arc::new(std::sync::Mutex::new(stream));
        let mut c = Client {
            r: BufReader::new(Box::new(Shared(shared.clone()))),
            w: BufWriter::new(Box::new(Shared(shared))),
            seq,
            caps,
            tls: caps & SSL != 0,
            version,
            in_tx: false,
            broken: false,
        };
        let password = url.password.clone().unwrap_or_default();
        let first = scramble(&plugin, &password, &nonce).unwrap_or_default();
        let mut resp = Vec::with_capacity(128);
        resp.extend(caps.to_le_bytes());
        resp.extend((1u32 << 24).to_le_bytes());
        resp.push(UTF8MB4);
        resp.extend([0; 23]);
        resp.extend(url.user.as_bytes());
        resp.push(0);
        if caps & PLUGIN_AUTH_LENENC != 0 {
            put_lenenc(&mut resp, first.len() as u64);
        } else {
            resp.push(first.len() as u8);
        }
        resp.extend(&first);
        resp.extend(url.database.as_bytes());
        resp.push(0);
        if caps & PLUGIN_AUTH != 0 {
            resp.extend(plugin.as_bytes());
            resp.push(0);
        }
        c.send_packet(&resp)?;
        c.w.flush()?;
        c.login(&password, plugin)?;
        Ok(c)
    }

    /// The rest of the login, after the HandshakeResponse: an OK, an error, or the server asking for more.
    fn login(&mut self, password: &str, mut plugin: String) -> io::Result<()> {
        loop {
            let p = self.read_packet()?;
            match p.first() {
                Some(0x00) => return Ok(()),
                Some(0xff) => return Err(io::Error::other(packet_error(&p))),
                // AuthSwitchRequest: another plugin, and its nonce
                Some(0xfe) => {
                    let rest = &p[1..];
                    let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
                    plugin = String::from_utf8_lossy(&rest[..end]).into_owned();
                    let mut nonce = rest.get(end + 1..).unwrap_or_default().to_vec();
                    if nonce.last() == Some(&0) {
                        nonce.pop();
                    }
                    let answer = match scramble(&plugin, password, &nonce) {
                        Some(a) => a,
                        None => self.clear(password, &plugin)?,
                    };
                    self.send_packet(&answer)?;
                    self.w.flush()?;
                }
                // AuthMoreData: caching_sha2_password's fast login done (3), or the password itself wanted (4)
                Some(0x01) => match (plugin.as_str(), p.get(1)) {
                    ("caching_sha2_password", Some(3)) => {}
                    ("caching_sha2_password", Some(4)) => {
                        let answer = self.clear(password, &plugin)?;
                        self.send_packet(&answer)?;
                        self.w.flush()?;
                    }
                    _ => return Err(err(format!("the login plugin {plugin} asks for what chronos doesn't do"))),
                },
                _ => return Err(err("the server's answer to the login is malformed")),
            }
        }
    }

    /// The password itself, for a plugin that takes it so: over TLS only, as anyone on the way could read it.
    fn clear(&self, password: &str, plugin: &str) -> io::Result<Vec<u8>> {
        if !self.tls {
            return Err(err(format!(
                "the server's login ({plugin}) wants the password itself, which goes only over TLS: connect with \
                 ssl-mode=REQUIRED (or VERIFY_IDENTITY), or give the user mysql_native_password"
            )));
        }
        let mut b = password.as_bytes().to_vec();
        b.push(0);
        Ok(b)
    }

    fn send_packet(&mut self, payload: &[u8]) -> io::Result<()> {
        // (a payload of 16 MB or more goes in parts; one that is a multiple of it ends with an empty part)
        let mut chunks = payload.chunks(0xff_ffff).peekable();
        if payload.is_empty() {
            write_header(&mut self.w, self.seq, 0)?;
            self.seq = self.seq.wrapping_add(1);
            return Ok(());
        }
        while let Some(c) = chunks.next() {
            write_header(&mut self.w, self.seq, c.len())?;
            self.w.write_all(c)?;
            self.seq = self.seq.wrapping_add(1);
            if chunks.peek().is_none() && c.len() == 0xff_ffff {
                write_header(&mut self.w, self.seq, 0)?;
                self.seq = self.seq.wrapping_add(1);
            }
        }
        Ok(())
    }

    fn read_packet(&mut self) -> io::Result<Vec<u8>> {
        let mut out = vec![];
        loop {
            let mut head = [0; 4];
            self.r.read_exact(&mut head)?;
            let n = u32::from_le_bytes([head[0], head[1], head[2], 0]) as usize;
            self.seq = head[3].wrapping_add(1);
            if out.len() + n > MAX_PAYLOAD {
                return Err(err("the server sent a packet longer than any value"));
            }
            let start = out.len();
            out.resize(start + n, 0);
            self.r.read_exact(&mut out[start..])?;
            if n < 0xff_ffff {
                return Ok(out);
            }
        }
    }

    /// Notes `out`'s failure if it isn't the server's answer (see `broken`).
    fn note<T>(&mut self, out: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &out
            && my_error(e).is_none()
        {
            self.broken = true;
        }
        out
    }

    /// Runs `sql`, one statement or several (`;` between them), in one round trip: each statement's outcome, in
    /// order. A statement that fails ends it (MySQL runs none after it): the outcomes of those before it, and the
    /// error with its place.
    #[allow(clippy::type_complexity)]
    pub fn query_all(&mut self, sql: &str) -> io::Result<(Vec<Done>, Option<(usize, io::Error)>)> {
        let out = self.run(sql, &mut |_| Ok(()), false);
        self.note(out)
    }

    /// `query_all`, Err on the first failure, for statements whose outcomes are wanted only if all ran.
    pub fn query(&mut self, sql: &str) -> io::Result<Vec<Done>> {
        match self.query_all(sql)? {
            (done, None) => Ok(done),
            (_, Some((_, e))) => Err(e),
        }
    }

    /// Runs one statement, giving `each` its rows as they come rather than keeping them (a whole table's read).
    pub fn query_each(
        &mut self,
        sql: &str,
        each: &mut dyn FnMut(Vec<Option<String>>) -> io::Result<()>,
    ) -> io::Result<()> {
        let out = self.run(sql, each, true).and_then(|(_, bad)| bad.map_or(Ok(()), |(_, e)| Err(e)));
        self.note(out)
    }

    #[allow(clippy::type_complexity)]
    fn run(
        &mut self,
        sql: &str,
        each: &mut dyn FnMut(Vec<Option<String>>) -> io::Result<()>,
        streamed: bool,
    ) -> io::Result<(Vec<Done>, Option<(usize, io::Error)>)> {
        self.seq = 0;
        let mut cmd = Vec::with_capacity(sql.len() + 1);
        cmd.push(0x03); // COM_QUERY
        cmd.extend(sql.as_bytes());
        self.send_packet(&cmd)?;
        self.w.flush()?;
        let mut all = vec![];
        let mut stopped: Option<io::Error> = None;
        loop {
            let p = self.read_packet()?;
            let more = match p.first() {
                Some(0x00) => {
                    let (affected, status) = ok_packet(&p)?;
                    self.in_tx = status & STATUS_IN_TRANS != 0;
                    all.push(Done { rows: vec![], affected });
                    status & MORE_RESULTS != 0
                }
                Some(0xff) => {
                    // (no statement runs after a failed one)
                    stopped = Some(io::Error::other(packet_error(&p)));
                    false
                }
                Some(0xfb) => return Err(err("the server asks for a local file, which chronos never sends")),
                _ => {
                    let mut at = 0;
                    let n = lenenc(&p, &mut at)? as usize;
                    for _ in 0..n {
                        self.read_packet()?; // column definitions: the SQL says what the columns are
                    }
                    if self.caps & DEPRECATE_EOF == 0 {
                        self.read_packet()?; // their EOF
                    }
                    let mut rows = vec![];
                    let status;
                    loop {
                        let r = self.read_packet()?;
                        match r.first() {
                            Some(0xfe) if r.len() < 0xff_ffff && (self.caps & DEPRECATE_EOF != 0 || r.len() < 9) => {
                                status = match self.caps & DEPRECATE_EOF != 0 {
                                    true => ok_packet(&r)?.1,
                                    false => r.get(3..5).map_or(0, |s| u16::from_le_bytes([s[0], s[1]])),
                                };
                                break;
                            }
                            Some(0xff) => {
                                stopped = Some(io::Error::other(packet_error(&r)));
                                status = 0;
                                break;
                            }
                            _ => {
                                let row = text_row(&r, n)?;
                                match streamed {
                                    true => each(row)?,
                                    false => rows.push(row),
                                }
                            }
                        }
                    }
                    self.in_tx = status & STATUS_IN_TRANS != 0;
                    if stopped.is_none() {
                        all.push(Done { rows, affected: 0 });
                    }
                    stopped.is_none() && status & MORE_RESULTS != 0
                }
            };
            if !more {
                let at = all.len();
                return Ok((all, stopped.map(|e| (at, e))));
            }
        }
    }
}

fn write_header(w: &mut impl Write, seq: u8, n: usize) -> io::Result<()> {
    let b = (n as u32).to_le_bytes();
    w.write_all(&[b[0], b[1], b[2], seq])
}

fn write_raw(w: &mut impl Write, seq: u8, payload: &[u8]) -> io::Result<()> {
    write_header(w, seq, payload.len())?;
    w.write_all(payload)?;
    w.flush()
}

fn read_raw(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0; 4];
    r.read_exact(&mut head)?;
    let n = u32::from_le_bytes([head[0], head[1], head[2], 0]) as usize;
    let mut body = vec![0; n];
    r.read_exact(&mut body)?;
    Ok((head[3], body))
}

/// An OK packet's affected rows and status flags.
fn ok_packet(p: &[u8]) -> io::Result<(u64, u16)> {
    let mut at = 1;
    let affected = lenenc(p, &mut at)?;
    lenenc(p, &mut at)?; // last insert id
    let status = p.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]));
    Ok((affected, status))
}

fn packet_error(p: &[u8]) -> MyError {
    let code = p.get(1..3).map_or(0, |c| u16::from_le_bytes([c[0], c[1]]));
    let (state, msg) = match p.get(3) {
        Some(b'#') => (String::from_utf8_lossy(p.get(4..9).unwrap_or_default()).into_owned(), p.get(9..)),
        _ => ("HY000".to_string(), p.get(3..)),
    };
    MyError { code, state, message: String::from_utf8_lossy(msg.unwrap_or_default()).into_owned() }
}

/// A text-protocol row of `n` values (None: NULL).
fn text_row(p: &[u8], n: usize) -> io::Result<Vec<Option<String>>> {
    let mut at = 0;
    let mut row = Vec::with_capacity(n);
    for _ in 0..n {
        if p.get(at) == Some(&0xfb) {
            at += 1;
            row.push(None);
            continue;
        }
        let len = lenenc(p, &mut at)? as usize;
        let v = p.get(at..at + len).ok_or_else(|| err("the server sent a malformed row"))?;
        row.push(Some(String::from_utf8_lossy(v).into_owned()));
        at += len;
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_literals() {
        let u = parse("mysql://app:p%40ss@db.example.com:3307/shop?ssl-mode=VERIFY_IDENTITY&ssl-ca=/ca.pem").unwrap();
        assert_eq!((u.user.as_str(), u.password.as_deref(), u.port), ("app", Some("p@ss"), 3307));
        assert_eq!(
            (u.database.as_str(), u.sslmode.as_str(), u.ssl_ca.as_deref()),
            ("shop", "verify-full", Some("/ca.pem"))
        );
        assert_eq!(parse("mysql://u@h/d").unwrap().port, 3306);
        assert!(parse("mysql://u@h").is_err(), "a database is needed");
        assert!(parse("mysql://u@h/d?ssl-mode=SOMETIMES").is_err());
        assert!(is_mysql("mariadb://x/y") && !is_mysql("postgres://x/y"));
        assert_eq!(lit(None), "NULL");
        assert_eq!(lit(Some("")), "''");
        assert_eq!(lit(Some("a'\\")), "convert(X'61275C' using utf8mb4)");
        assert_eq!(q("we`ird"), "`we``ird`");
    }

    #[test]
    fn scrambles_as_mysql_does() {
        // as MySQL's own clients compute them (empty password: empty answer)
        assert!(scramble("mysql_native_password", "", b"x").unwrap().is_empty());
        let nonce = b"abcdefghijklmnopqrst";
        let a = scramble("mysql_native_password", "secret", nonce).unwrap();
        assert_eq!(a.len(), 20);
        let b = scramble("caching_sha2_password", "secret", nonce).unwrap();
        assert_eq!(b.len(), 32);
        // XOR undone with the stored hash gives SHA1(password) back: what the server checks
        let h2 = sha1(&[&sha1(&[b"secret"])]);
        let h3 = sha1(&[nonce, &h2]);
        let back: Vec<u8> = a.iter().zip(&h3).map(|(x, y)| x ^ y).collect();
        assert_eq!(back, sha1(&[b"secret"]));
        assert!(scramble("sha256_password", "secret", nonce).is_none());
    }

    #[test]
    fn length_encoded_integers_round_trip() {
        for n in [0u64, 250, 251, 65535, 65536, 0xff_ffff, 0x100_0000, u64::MAX] {
            let mut b = vec![];
            put_lenenc(&mut b, n);
            let mut at = 0;
            assert_eq!(lenenc(&b, &mut at).unwrap(), n);
            assert_eq!(at, b.len());
        }
    }
}
