//! A Postgres client, just enough for `chronos import postgres://...` and `chronos attach`: connect over TCP (TLS as
//! libpq's `sslmode` says), log in (trust, a password, MD5, SCRAM-SHA-256, bound to the server's
//! certificate over TLS as libpq's `channel_binding=prefer` binds it), run queries with text
//! results, run one statement with parameters (text, never quoted into the SQL), and read `COPY ... TO STDOUT` a
//! message at a time.
//!
//!   connect ─> [SSLRequest ─> TLS] ─> StartupMessage ─> auth (password | MD5 | SASL) ─> ReadyForQuery
//!   query ───> Query ─> RowDescription, DataRow..., CommandComplete ─> ReadyForQuery
//!   params ──> Parse, Bind, Execute, Sync (unnamed) ─> ParseComplete, BindComplete, DataRow..., CommandComplete ─> ReadyForQuery
//!   copy ────> Query ─> CopyOutResponse, CopyData... (one per `copy_chunk`), CopyDone ─> ReadyForQuery

use ring::{digest, hmac, rand::SecureRandom};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// Where to connect, from `postgres://user:password@host:port/database?sslmode=...`.
#[derive(Debug, PartialEq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub database: String,
    /// disable, prefer (the default), require, verify-ca or verify-full
    pub sslmode: String,
    /// CA certificates (PEM) for verify-ca and verify-full, instead of the system's
    pub sslrootcert: Option<String>,
    /// disable, prefer (the default) or require, as libpq's `channel_binding`
    pub channel_binding: String,
    /// libpq's `connect_timeout`, in seconds (0: wait as long as the system does)
    pub connect_timeout: u64,
    /// a password may go in the clear or as MD5 over a connection that isn't TLS, or whose server's certificate
    /// wasn't checked (sslmode prefer or require: anyone on the way could have answered), `chronos import
    /// --allow-plaintext-password`; else only SCRAM-SHA-256, which never sends it, may there
    pub plain_password: bool,
}

/// How long connecting may take unless the URL's connect_timeout says.
const CONNECT_TIMEOUT: u64 = 30;
/// How long the server may send nothing while a reply is awaited (a big table's COPY streams, so
/// this is between its messages, not for all of it).
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
/// The longest message taken from the server: Postgres's own limit on a value (1 GB), and a row's
/// framing past it. A longer length is a broken (or hostile) server.
const MAX_MESSAGE: usize = (1 << 30) + (1 << 20);

/// `%xx` in a URL part, as its byte.
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

pub fn parse(url: &str) -> Result<Url, String> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or("the URL must start with postgres:// or postgresql://")?;
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (auth, rest) = rest.rsplit_once('@').map_or(("", rest), |(a, r)| (a, r));
    let (hostport, database) = rest.split_once('/').unwrap_or((rest, ""));
    let (user, password) = match auth.split_once(':') {
        Some((u, p)) => (unescape(u)?, Some(unescape(p)?)),
        None => (unescape(auth)?, None),
    };
    let (host, port) = match hostport.rsplit_once(':').filter(|(h, _)| !h.ends_with(']') || h.starts_with('[')) {
        Some((h, p)) if !p.contains(']') => (h, p.parse().map_err(|_| format!("bad port {p}"))?),
        _ => (hostport, 5432),
    };
    let mut u = Url {
        host: host.trim_start_matches('[').trim_end_matches(']').to_string(),
        port,
        user: if user.is_empty() { std::env::var("USER").unwrap_or_else(|_| "postgres".into()) } else { user },
        password: password.or_else(|| std::env::var("PGPASSWORD").ok()),
        database: unescape(database)?,
        sslmode: "prefer".into(),
        sslrootcert: None,
        channel_binding: "prefer".into(),
        connect_timeout: CONNECT_TIMEOUT,
        plain_password: false,
    };
    if u.host.is_empty() {
        u.host = "localhost".into();
    }
    if u.database.is_empty() {
        u.database = u.user.clone();
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = unescape(v)?;
        match k {
            "sslmode"
                if ["disable", "allow", "prefer", "require", "verify-ca", "verify-full"].contains(&v.as_str()) =>
            {
                u.sslmode = v
            }
            "sslmode" => {
                return Err(format!("sslmode {v} isn't one of disable, prefer, require, verify-ca, verify-full"));
            }
            "sslrootcert" => u.sslrootcert = Some(v),
            "channel_binding" if ["disable", "prefer", "require"].contains(&v.as_str()) => u.channel_binding = v,
            "channel_binding" => return Err(format!("channel_binding {v} isn't one of disable, prefer, require")),
            "password" => u.password = Some(v),
            "user" => u.user = v,
            "host" if v.starts_with('/') => {
                return Err("Unix sockets aren't supported: connect over TCP (host:port)".into());
            }
            "host" => u.host = v,
            "port" => u.port = v.parse().map_err(|_| format!("bad port {v}"))?,
            "connect_timeout" => u.connect_timeout = v.parse().map_err(|_| format!("bad connect_timeout {v}"))?,
            _ => {} // application_name, ...: not needed to read a database
        }
    }
    Ok(u)
}

/// `values` as a Postgres array literal of text (`{"a",NULL,"b\\"c"}`), for one `$n::text[]` parameter: every
/// element quoted, a quote or backslash in it escaped, None as NULL.
pub fn text_array<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> String {
    let mut out = String::from("{");
    for (i, v) in values.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match v {
            None => out.push_str("NULL"),
            Some(v) => {
                out.push('"');
                for c in v.chars() {
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
        }
    }
    out.push('}');
    out
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

/// A connection, logged in and ready for queries.
pub struct Client {
    r: BufReader<Box<dyn Stream>>,
    w: BufWriter<Box<dyn Stream>>,
    /// in a COPY ... TO STDOUT, until its CopyDone
    copying: bool,
    /// over TLS: (the certificate's channel binding data, if it can be made)
    tls: Option<Option<Vec<u8>>>,
    /// the last ReadyForQuery's transaction status: `I` idle, `T` in a transaction, `E` in a failed one
    pub status: u8,
}

/// An ErrorResponse from the server: its SQLSTATE, message and detail. `chronos attach` tells a lock timeout or a
/// duplicate key from the rest by the code; the detail names the row a constraint refused.
#[derive(Debug, Clone)]
pub struct PgError {
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
}

impl std::fmt::Display for PgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for PgError {}

/// The server's error inside `e`, if it is one (not a broken connection).
pub fn pg_error(e: &io::Error) -> Option<&PgError> {
    e.get_ref().and_then(|i| i.downcast_ref::<PgError>())
}

/// A reply's rows, as text (None: NULL).
pub type Rows = Vec<Vec<Option<String>>>;

/// Both halves of one stream: reads and writes share it (TLS state included).
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

fn err(m: impl Into<String>) -> io::Error {
    io::Error::other(m.into())
}

/// Accepts any certificate: libpq's sslmode=prefer and require encrypt without checking whose
/// server it is. The handshake's signatures are still checked.
#[derive(Debug)]
struct Unchecked(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for Unchecked {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// sslmode=verify-ca's check: the certificate's chain up to a trusted CA, as verify-full checks it, but not its
/// name, as libpq's verify-ca doesn't (a certificate whose name isn't the host's is accepted once its chain is
/// good). webpki checks the chain before the name, so a name error means the chain passed.
#[derive(Debug)]
struct ChainOnly(Arc<rustls::client::WebPkiServerVerifier>);

impl rustls::client::danger::ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use rustls::CertificateError::{NotValidForName, NotValidForNameContext};
        match self.0.verify_server_cert(cert, intermediates, name, ocsp, now) {
            Err(rustls::Error::InvalidCertificate(NotValidForName | NotValidForNameContext { .. })) => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            other => other,
        }
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(m, c, d)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// CA certificates: `sslrootcert`, else the system's bundle.
pub fn roots(file: Option<&str>) -> io::Result<rustls::RootCertStore> {
    let system = ["/etc/ssl/cert.pem", "/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt"];
    let path = match file {
        Some(f) => f.to_string(),
        None => system.iter().find(|p| std::path::Path::new(p).exists()).map(|p| p.to_string()).ok_or_else(|| {
            err("no CA certificates found for sslmode=verify-ca or verify-full: give them with sslrootcert=<file.pem>")
        })?,
    };
    let mut store = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(&path).map_err(|e| err(format!("{path}: {e}")))? {
        let _ = store.add(c.map_err(|e| err(format!("{path}: {e}")))?);
    }
    Ok(store)
}

/// A stream to the server, and over TLS the tls-server-end-point channel binding data of its
/// certificate (None where its signature has no hash of its own: see `scram::end_point`).
type Connected = (Box<dyn Stream + Send>, Option<Vec<u8>>);

/// A TLS stream to the server, its handshake done, and its certificate's channel binding data.
fn tls(url: &Url, mut tcp: TcpStream) -> io::Result<Connected> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| err(format!("TLS: {e}")))?;
    let cfg = match url.sslmode.as_str() {
        "verify-full" => builder.with_root_certificates(roots(url.sslrootcert.as_deref())?).with_no_client_auth(),
        "verify-ca" => {
            let roots = Arc::new(roots(url.sslrootcert.as_deref())?);
            let chain = rustls::client::WebPkiServerVerifier::builder_with_provider(roots, provider.clone())
                .build()
                .map_err(|e| err(format!("TLS: {e}")))?;
            builder.dangerous().with_custom_certificate_verifier(Arc::new(ChainOnly(chain))).with_no_client_auth()
        }
        _ => builder.dangerous().with_custom_certificate_verifier(Arc::new(Unchecked(provider))).with_no_client_auth(),
    };
    let name = ServerName::try_from(url.host.clone()).map_err(|e| err(format!("TLS: {}: {e}", url.host)))?;
    let mut conn = rustls::ClientConnection::new(Arc::new(cfg), name).map_err(|e| err(format!("TLS: {e}")))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp).map_err(|e| err(format!("TLS: {e}")))?;
    }
    let binding = conn.peer_certificates().and_then(|c| c.first()).and_then(|c| crate::sql::scram::end_point(c));
    Ok((Box::new(rustls::StreamOwned::new(conn, tcp)), binding))
}

fn b64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = vec![];
    for c in s.chunks(4) {
        let mut n = 0u32;
        for (i, b) in c.iter().enumerate() {
            n |= (val(*b)? as u32) << (18 - 6 * i);
        }
        out.extend(&n.to_be_bytes()[1..c.len()]);
    }
    Some(out)
}

fn hmac256(key: &[u8], data: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data).as_ref().to_vec()
}

/// The client's side of a SCRAM-SHA-256 login, as libpq runs it with `channel_binding=prefer` (the
/// default): over TLS, bound to the server's certificate (SCRAM-SHA-256-PLUS, `tls-server-end-point`)
/// where the server offers that, else saying it could bind (`y`, which a server that offered binding
/// refuses, so a server in the middle can't take it away); without TLS, `n`. Where the server offers
/// binding over TLS but the certificate's hash can't be made (EdDSA, whose signature has none), the
/// login fails, as libpq's does: a server in the middle with such a certificate could otherwise
/// relay the real server's offer and have the client not bind. `require` logs in only bound;
/// `disable` never binds.
struct Scram {
    /// the GS2 header, and the channel binding data that goes with it
    gs2: String,
    binding: Vec<u8>,
    first_bare: String,
    /// (the AuthMessage, SaltedPassword), to check the server's proof with
    sent: Option<(String, Vec<u8>)>,
}

impl Scram {
    /// `offered`: AuthenticationSASL's mechanisms; `tls`: the connection is TLS, and `binding` its
    /// certificate's hash, if one can be made. The mechanism chosen, and the client-first-message.
    fn start(
        offered: &[&str],
        mode: &str,
        tls: bool,
        binding: Option<&[u8]>,
        nonce: &str,
    ) -> io::Result<(Scram, &'static str, String)> {
        let plus = tls && mode != "disable" && offered.contains(&"SCRAM-SHA-256-PLUS");
        let (mechanism, gs2, binding) = match binding {
            Some(b) if plus => ("SCRAM-SHA-256-PLUS", "p=tls-server-end-point,,", b.to_vec()),
            None if plus => {
                return Err(err(
                    "the server offers SCRAM channel binding, but its certificate's hash can't be made (a signature \
                     without a hash of its own, such as EdDSA): connect with channel_binding=disable if you trust \
                     the connection",
                ));
            }
            _ if mode == "require" => {
                return Err(err("channel_binding=require, but the server doesn't offer SCRAM-SHA-256-PLUS over TLS"));
            }
            _ if !offered.contains(&"SCRAM-SHA-256") => {
                return Err(err(format!("the server offers no login chronos import knows ({})", offered.join(", "))));
            }
            _ if tls && mode != "disable" => ("SCRAM-SHA-256", "y,,", vec![]),
            _ => ("SCRAM-SHA-256", "n,,", vec![]),
        };
        let first_bare = format!("n=,r={nonce}");
        let first = format!("{gs2}{first_bare}");
        Ok((Scram { gs2: gs2.into(), binding, first_bare, sent: None }, mechanism, first))
    }

    /// The server-first-message, answered with the client-final-message.
    fn answer(&mut self, server_first: &str, password: &str) -> io::Result<String> {
        let attr = |k: &str| server_first.split(',').find_map(|a| a.strip_prefix(k));
        let bad = || err("the server's SCRAM reply is malformed");
        let (nonce, salt, iters) =
            (attr("r=").ok_or_else(bad)?, attr("s=").ok_or_else(bad)?, attr("i=").ok_or_else(bad)?);
        if !nonce.starts_with(self.first_bare.trim_start_matches("n=,r=")) {
            return Err(err("the server's SCRAM nonce doesn't extend ours"));
        }
        let salt = unb64(salt).ok_or_else(bad)?;
        let iters: u32 = iters.parse().ok().filter(|n| *n > 0).ok_or_else(bad)?;
        // (SASLprep'd, as libpq does)
        let salted = crate::sql::scram::salted_password(password, &salt, iters);
        let client_key = hmac256(&salted, b"Client Key");
        let stored = digest::digest(&digest::SHA256, &client_key);
        let without_proof = format!("c={},r={nonce}", b64(&[self.gs2.as_bytes(), &self.binding].concat()));
        let auth = format!("{},{server_first},{without_proof}", self.first_bare);
        let sig = hmac256(stored.as_ref(), auth.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(&sig).map(|(a, b)| a ^ b).collect();
        self.sent = Some((auth, salted));
        Ok(format!("{without_proof},p={}", b64(&proof)))
    }

    /// The server-final-message: the server must prove it knows the password too.
    fn check(&mut self, server_final: &str) -> io::Result<()> {
        let (auth, salted) = self.sent.take().ok_or_else(|| err("SCRAM ended before it began"))?;
        let server_key = hmac256(&salted, b"Server Key");
        let want = b64(&hmac256(&server_key, auth.as_bytes()));
        if server_final.trim_start_matches("v=").trim_end_matches('\0') != want {
            return Err(err("the server couldn't prove it knows the password (SCRAM)"));
        }
        Ok(())
    }
}

/// A DataRow's values as text (None: NULL); a row shorter than its lengths say is an error, not a panic.
fn data_row(body: &[u8]) -> io::Result<Vec<Option<String>>> {
    let bad = || err("the server sent a malformed DataRow");
    let n = body.get(..2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).ok_or_else(bad)?;
    let (mut at, mut row) = (2, Vec::with_capacity(n.min(1600)));
    for _ in 0..n {
        let len = body.get(at..at + 4).map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]])).ok_or_else(bad)?;
        at += 4;
        if len < 0 {
            row.push(None);
        } else {
            let v = body.get(at..at + len as usize).ok_or_else(bad)?;
            row.push(Some(String::from_utf8_lossy(v).into_owned()));
            at += len as usize;
        }
    }
    Ok(row)
}

/// A TCP connection to `addr`, trying each address it resolves to for at most `secs` seconds each
/// (0: as long as the system takes; under 2, 2, as libpq's connect_timeout).
pub fn connect_tcp(addr: &str, secs: u64) -> io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    if secs == 0 {
        return TcpStream::connect(addr);
    }
    let wait = std::time::Duration::from_secs(secs.max(2));
    let mut last = io::Error::other("the host name resolves to no address");
    for a in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, wait) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

impl Client {
    pub fn connect(url: &Url) -> io::Result<Client> {
        Client::connect_as(url, "chronos import")
    }

    /// `connect`, telling the server `app` as the application_name (what pg_stat_activity shows).
    pub fn connect_as(url: &Url, app: &str) -> io::Result<Client> {
        let addr = format!("{}:{}", url.host, url.port);
        let start = std::time::Instant::now();
        let mut tcp = connect_tcp(&addr, url.connect_timeout).map_err(|e| err(format!("connecting to {addr}: {e}")))?;
        tcp.set_nodelay(true)?;
        // a server that stops answering (or never does) doesn't hold the import for ever
        tcp.set_read_timeout(Some(READ_TIMEOUT))?;
        tcp.set_write_timeout(Some(READ_TIMEOUT))?;
        // connect_timeout bounds all of connecting, as libpq's: the TLS handshake and the login too
        let sock = tcp.try_clone()?;
        let until =
            (url.connect_timeout > 0).then(|| start + std::time::Duration::from_secs(url.connect_timeout.max(2)));
        let tick = || -> io::Result<()> {
            let Some(until) = until else { return Ok(()) };
            let left = (until.checked_duration_since(std::time::Instant::now()))
                .filter(|d| !d.is_zero())
                .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, format!("connecting to {addr}: timed out")))?;
            sock.set_read_timeout(Some(left))?;
            sock.set_write_timeout(Some(left))
        };
        tick()?;
        let mut tls_binding = None;
        let stream: Box<dyn Stream + Send> = if url.sslmode == "disable" {
            Box::new(tcp)
        } else {
            // SSLRequest: the server answers S (go on in TLS) or N
            tcp.write_all(&[0, 0, 0, 8, 4, 210, 22, 47])?;
            let mut yes = [0];
            tcp.read_exact(&mut yes)?;
            match (yes[0], url.sslmode.as_str()) {
                (b'S', _) => {
                    tick()?;
                    let (stream, binding) = tls(url, tcp)?;
                    tls_binding = Some(binding);
                    stream
                }
                (_, "prefer" | "allow") => Box::new(tcp),
                _ => return Err(err(format!("{addr} doesn't offer TLS, and sslmode={} needs it", url.sslmode))),
            }
        };
        let shared = Arc::new(std::sync::Mutex::new(stream));
        let mut c = Client {
            r: BufReader::new(Box::new(Shared(shared.clone()))),
            w: BufWriter::new(Box::new(Shared(shared))),
            copying: false,
            tls: tls_binding,
            status: b'I',
        };
        let mut body = vec![0, 3, 0, 0]; // protocol 3.0
        for (k, v) in [("user", url.user.as_str()), ("database", &url.database), ("application_name", app)] {
            body.extend(k.as_bytes());
            body.push(0);
            body.extend(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        tick()?;
        c.w.write_all(&((body.len() + 4) as u32).to_be_bytes())?;
        c.w.write_all(&body)?;
        c.w.flush()?;
        c.login(url, &tick)?;
        // connected: from here only READ_TIMEOUT between messages
        sock.set_read_timeout(Some(READ_TIMEOUT))?;
        sock.set_write_timeout(Some(READ_TIMEOUT))?;
        Ok(c)
    }

    fn send(&mut self, tag: u8, body: &[u8]) -> io::Result<()> {
        self.w.write_all(&[tag])?;
        self.w.write_all(&((body.len() + 4) as u32).to_be_bytes())?;
        self.w.write_all(body)
    }

    fn recv(&mut self) -> io::Result<(u8, Vec<u8>)> {
        let mut head = [0; 5];
        self.r.read_exact(&mut head)?;
        let n = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if !(4..=MAX_MESSAGE).contains(&n) {
            return Err(err(format!("the server sent a message of impossible length {n}")));
        }
        // (grown as the bytes arrive: a length alone doesn't get its size allocated)
        let mut body = Vec::with_capacity((n - 4).min(1 << 16));
        if (&mut self.r).take((n - 4) as u64).read_to_end(&mut body)? < n - 4 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok((head[0], body))
    }

    /// An ErrorResponse as `message (SQLSTATE)`, a `PgError` inside (see `pg_error`).
    fn error(body: &[u8]) -> io::Error {
        let mut e = PgError { code: String::new(), message: String::new(), detail: None };
        for f in body.split(|b| *b == 0).filter(|f| !f.is_empty()) {
            let v = String::from_utf8_lossy(&f[1..]).into_owned();
            match f[0] {
                b'M' => e.message = v,
                b'C' => e.code = v,
                b'D' => e.detail = Some(v),
                _ => {}
            }
        }
        io::Error::other(e)
    }

    /// Logs in; `tick` (see `connect`) before each message awaited keeps it within connect_timeout.
    fn login(&mut self, url: &Url, tick: &dyn Fn() -> io::Result<()>) -> io::Result<()> {
        let password =
            || url.password.clone().ok_or_else(|| err("the server wants a password: put it in the URL or PGPASSWORD"));
        let mut scram: Option<Scram> = None;
        loop {
            tick()?;
            let (tag, body) = self.recv()?;
            match tag {
                b'R' => {
                    let head = body.get(..4).ok_or_else(|| err("bad auth message"))?;
                    let kind = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
                    // channel_binding=require takes only a login bound to the certificate, as libpq's
                    let bound = scram.as_ref().is_some_and(|s| s.gs2.starts_with('p'));
                    if url.channel_binding == "require" && ((kind == 0 && !bound) || kind == 3 || kind == 5) {
                        return Err(err("channel_binding=require, but the server didn't log in with channel binding"));
                    }
                    // a password in the clear (3), or as MD5 (5, a hash anyone who sees it can log in with
                    // elsewhere and guess from), goes only over TLS to the server the URL names, its certificate
                    // checked for that name (sslmode=verify-full: with prefer or require anyone on the way could be
                    // the one asking, and with verify-ca anyone holding a certificate from a trusted CA, for any
                    // name), unless the user said otherwise
                    let checked = self.tls.is_some() && url.sslmode == "verify-full";
                    if matches!(kind, 3 | 5) && !checked && !url.plain_password {
                        let how = if kind == 3 { "in the clear" } else { "as MD5" };
                        let over = match self.tls {
                            None => "a connection that isn't TLS",
                            Some(_) => "TLS whose certificate wasn't checked for the server's name",
                        };
                        return Err(err(format!(
                            "the server asks for the password {how} over {over}: refused, as anyone on the way \
                             could read it. Use sslmode=verify-full, have the server use scram-sha-256, or pass \
                             --allow-plaintext-password if the network is trusted"
                        )));
                    }
                    match kind {
                        0 => {}
                        3 => {
                            let mut p = password()?.into_bytes();
                            p.push(0);
                            self.send(b'p', &p)?;
                            self.w.flush()?;
                        }
                        5 => {
                            // md5(md5(password, user), the server's salt)
                            let salt = body.get(4..8).ok_or_else(|| err("bad auth message"))?;
                            let hash = crate::sql::scram::md5_hash(&password()?, &url.user);
                            let mut p = crate::sql::scram::md5_salted(&hash, salt).into_bytes();
                            p.push(0);
                            self.send(b'p', &p)?;
                            self.w.flush()?;
                        }
                        10 => {
                            let mechs = String::from_utf8_lossy(&body[4..]).into_owned();
                            let offered: Vec<&str> = mechs.split('\0').filter(|m| !m.is_empty()).collect();
                            let mut nonce = [0u8; 18];
                            ring::rand::SystemRandom::new().fill(&mut nonce).map_err(|_| err("no random numbers"))?;
                            let binding = self.tls.as_ref().and_then(|b| b.as_deref());
                            let tls = self.tls.is_some();
                            let (s, mechanism, first) =
                                Scram::start(&offered, &url.channel_binding, tls, binding, &b64(&nonce))?;
                            scram = Some(s);
                            let mut b = format!("{mechanism}\0").into_bytes();
                            b.extend((first.len() as u32).to_be_bytes());
                            b.extend(first.as_bytes());
                            self.send(b'p', &b)?;
                            self.w.flush()?;
                        }
                        11 => {
                            let s = scram.as_mut().ok_or_else(|| err("SCRAM ended before it began"))?;
                            let last = s.answer(&String::from_utf8_lossy(&body[4..]), &password()?)?;
                            self.send(b'p', last.as_bytes())?;
                            self.w.flush()?;
                        }
                        12 => {
                            let s = scram.as_mut().ok_or_else(|| err("SCRAM ended before it began"))?;
                            s.check(&String::from_utf8_lossy(&body[4..]))?;
                        }
                        k => return Err(err(format!("the server asks for a login chronos import doesn't do ({k})"))),
                    }
                }
                b'E' => return Err(Self::error(&body)),
                b'Z' => return Ok(()),
                _ => {} // ParameterStatus, BackendKeyData, notices
            }
        }
    }

    /// Runs `sql` (one statement), and returns its rows as text.
    pub fn query(&mut self, sql: &str) -> io::Result<Rows> {
        let mut q = sql.as_bytes().to_vec();
        q.push(0);
        self.send(b'Q', &q)?;
        self.w.flush()?;
        let (mut rows, mut failed) = (vec![], None);
        loop {
            let (tag, body) = self.recv()?;
            match tag {
                b'D' => rows.push(data_row(&body)?),
                b'E' => failed = Some(Self::error(&body)),
                b'Z' => {
                    self.status = body.first().copied().unwrap_or(b'I');
                    return failed.map_or(Ok(rows), Err);
                }
                _ => {} // RowDescription, CommandComplete, notices
            }
        }
    }

    /// Runs one statement `sql` with `params` (`$1`, `$2`, ...: text, None for NULL, the SQL casting each to its
    /// type), and returns its rows as text: the extended query protocol, an unnamed statement and portal, so no value
    /// is ever quoted into SQL and nothing outlives the call (a pooler in transaction mode may hand the next one to
    /// another server connection).
    pub fn query_params(&mut self, sql: &str, params: &[Option<&str>]) -> io::Result<Rows> {
        if params.len() > u16::MAX as usize {
            return Err(err(format!("{} parameters, more than one statement takes", params.len())));
        }
        let mut parse = vec![0]; // unnamed
        parse.extend(sql.as_bytes());
        parse.extend([0, 0, 0]); // no parameter types given: the SQL casts them
        self.send(b'P', &parse)?;
        let mut bind = vec![0, 0, 0, 0]; // unnamed portal and statement, no format codes (all text)
        bind.extend((params.len() as u16).to_be_bytes());
        for p in params {
            match p {
                None => bind.extend((-1i32).to_be_bytes()),
                Some(v) => {
                    bind.extend((v.len() as i32).to_be_bytes());
                    bind.extend(v.as_bytes());
                }
            }
        }
        bind.extend([0, 0]); // results in text
        self.send(b'B', &bind)?;
        self.send(b'E', &[0, 0, 0, 0, 0])?; // the unnamed portal, all its rows
        self.send(b'S', &[])?;
        self.w.flush()?;
        let (mut rows, mut failed) = (vec![], None);
        loop {
            let (tag, body) = self.recv()?;
            match tag {
                b'D' => rows.push(data_row(&body)?),
                b'E' => failed = Some(Self::error(&body)),
                b'Z' => {
                    self.status = body.first().copied().unwrap_or(b'I');
                    return failed.map_or(Ok(rows), Err);
                }
                _ => {} // ParseComplete, BindComplete, RowDescription, NoData, CommandComplete, notices
            }
        }
    }

    /// Starts `COPY ... TO STDOUT`: then `copy_chunk` until it gives None.
    pub fn copy_out(&mut self, sql: &str) -> io::Result<()> {
        let mut q = sql.as_bytes().to_vec();
        q.push(0);
        self.send(b'Q', &q)?;
        self.w.flush()?;
        loop {
            let (tag, body) = self.recv()?;
            match tag {
                b'H' => {
                    self.copying = true;
                    return Ok(());
                }
                b'E' => {
                    let e = Self::error(&body);
                    loop {
                        let (tag, body) = self.recv()?;
                        if tag == b'Z' {
                            self.status = body.first().copied().unwrap_or(b'I');
                            break;
                        }
                    }
                    return Err(e);
                }
                _ => {}
            }
        }
    }

    /// The COPY's next CopyData; None once it's done (and the connection ready again).
    pub fn copy_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        if !self.copying {
            return Ok(None);
        }
        let mut failed = None;
        loop {
            let (tag, body) = self.recv()?;
            match tag {
                b'd' => return Ok(Some(body)),
                b'E' => failed = Some(Self::error(&body)),
                b'Z' => {
                    self.copying = false;
                    self.status = body.first().copied().unwrap_or(b'I');
                    return failed.map_or(Ok(None), Err);
                }
                _ => {} // CopyDone, CommandComplete
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.send(b'X', &[]); // Terminate
        let _ = self.w.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that takes the connection and then says nothing is given up on at connect_timeout, not READ_TIMEOUT:
    /// the limit holds for all of connecting, the TLS request and the login too.
    #[test]
    fn connect_timeout_bounds_the_login_too() {
        use std::time::{Duration, Instant};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let silent = std::thread::spawn(move || {
            let held: Vec<_> = (0..2).filter_map(|_| l.accept().ok()).collect();
            std::thread::sleep(Duration::from_secs(1));
            drop(held)
        });
        for mode in ["disable", "prefer"] {
            let url = parse(&format!("postgres://u@127.0.0.1:{port}/d?connect_timeout=2&sslmode={mode}")).unwrap();
            let t = Instant::now();
            assert!(Client::connect(&url).is_err(), "{mode}");
            assert!(t.elapsed() < Duration::from_secs(5), "{mode}: {:?}", t.elapsed());
        }
        silent.join().unwrap();
    }

    #[test]
    fn urls_and_base64() {
        let u = parse("postgres://ada:p%40ss@db.example.com:6543/shop?sslmode=require").unwrap();
        assert_eq!((u.user.as_str(), u.password.as_deref(), u.host.as_str()), ("ada", Some("p@ss"), "db.example.com"));
        assert_eq!((u.port, u.database.as_str(), u.sslmode.as_str()), (6543, "shop", "require"));
        let u = parse("postgresql://bo@localhost/").unwrap();
        assert_eq!((u.port, u.database.as_str(), u.sslmode.as_str()), (5432, "bo", "prefer"));
        assert!(parse("mysql://x").is_err() && parse("postgres://h/d?sslmode=maybe").is_err());
        assert_eq!(parse("postgres://h/d?channel_binding=require").unwrap().channel_binding, "require");
        assert!(parse("postgres://h/d?channel_binding=maybe").is_err());
        for s in [&b""[..], b"n,,", b"any carnal pleasure.", &[0xff, 0, 1, 2]] {
            assert_eq!(unb64(&b64(s)).unwrap(), s, "{s:?}");
        }
        assert_eq!(b64(b"n,,"), "biws");
        assert_eq!(parse("postgres://h/d?connect_timeout=5").unwrap().connect_timeout, 5);
        assert!(!parse("postgres://h/d").unwrap().plain_password);
    }

    /// A server asking for the password in the clear (3) or as MD5 (5) gets it only over TLS whose certificate was
    /// checked (or with --allow-plaintext-password): over TLS with sslmode=prefer or require, whoever answered might
    /// not be the server.
    #[test]
    fn a_password_goes_in_the_clear_only_where_the_certificate_was_checked() {
        use std::sync::{Arc, Mutex};
        struct Sent(Arc<Mutex<Vec<u8>>>);
        impl Read for Sent {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        impl Write for Sent {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        // (what the client sends, and the login's error)
        let login = |sslmode: &str, tls: bool, kind: u32, plain: bool| {
            let mut url = parse(&format!("postgres://u:secret@h/d?sslmode={sslmode}")).unwrap();
            url.plain_password = plain;
            // AuthenticationCleartextPassword, or AuthenticationMD5Password with its salt
            let body: Vec<u8> =
                kind.to_be_bytes().into_iter().chain(if kind == 5 { vec![1, 2, 3, 4] } else { vec![] }).collect();
            let mut asks = vec![b'R'];
            asks.extend((body.len() as u32 + 4).to_be_bytes());
            asks.extend(body);
            let sent = Arc::new(Mutex::new(vec![]));
            let mut c = Client {
                r: BufReader::new(Box::new(io::Cursor::new(asks))),
                w: BufWriter::new(Box::new(Sent(sent.clone()))),
                copying: false,
                tls: tls.then_some(None),
                status: b'I',
            };
            let e = c.login(&url, &|| Ok(())).expect_err("the stream ends").to_string();
            let sent = sent.lock().unwrap().clone();
            (!sent.is_empty(), e)
        };
        for kind in [3, 5] {
            let (sent, e) = login("prefer", false, kind, false);
            assert!(!sent && e.contains("a connection that isn't TLS"), "{kind}: {e}");
            for mode in ["prefer", "require"] {
                let (sent, e) = login(mode, true, kind, false);
                assert!(!sent && e.contains("TLS whose certificate wasn't checked"), "{mode} {kind}: {e}");
                assert!(login(mode, true, kind, true).0, "{mode} {kind}: --allow-plaintext-password");
            }
            // verify-ca checks the chain, not the name: any certificate from a trusted CA passes it
            let (sent, e) = login("verify-ca", true, kind, false);
            assert!(!sent && e.contains("TLS whose certificate wasn't checked"), "verify-ca {kind}: {e}");
            assert!(login("verify-full", true, kind, false).0, "verify-full {kind}");
        }
    }

    /// A DataRow shorter than its counts and lengths say is an error, never a panic.
    #[test]
    fn short_data_rows_are_errors() {
        assert_eq!(data_row(&[0, 2, 0, 0, 0, 1, b'x', 255, 255, 255, 255]).unwrap(), [Some("x".into()), None]);
        for short in [&[][..], &[0], &[0, 1], &[0, 1, 0, 0, 0], &[0, 1, 0, 0, 0, 9, b'x'], &[0, 2, 255, 255, 255, 255]]
        {
            assert!(data_row(short).is_err(), "{short:?}");
        }
    }

    /// The client's SCRAM against the server's (`scram::Login`): bound to the certificate's hash
    /// where TLS is on and the server offers SCRAM-SHA-256-PLUS, so a server that has another
    /// certificate (one in the middle) refuses it; `y` over TLS where it isn't offered; `n` without
    /// TLS. Where PLUS is offered over TLS and the certificate's hash can't be made, no login.
    #[test]
    fn scram_binds_to_the_certificate() {
        use crate::sql::scram::{Login, parse, verifier};
        let v = parse(&verifier("pw", b"0123456789abcdef", 4096)).unwrap();
        // (mechanisms offered, TLS, the client's binding, the server's): the mechanism and GS2 flag
        // the client chose, and whether the server took its proof
        let run = |offered: &[&str], tls: bool, mine: Option<&[u8]>, theirs: Option<&[u8]>| {
            let (mut c, mechanism, first) = Scram::start(offered, "prefer", tls, mine, "nOnCe").unwrap();
            let mut l = Login::start(mechanism.as_bytes(), vec![v.clone()], false, theirs).unwrap();
            let server_first = String::from_utf8(l.first(first.as_bytes()).unwrap()).unwrap();
            let last = c.answer(&server_first, "pw").unwrap();
            let ok = match l.last(last.as_bytes(), "u") {
                Ok((_, fin)) => c.check(&String::from_utf8(fin).unwrap()).is_ok(),
                Err(e) => {
                    assert_eq!(e.code, "28000", "{}", e.message);
                    false
                }
            };
            (mechanism, first.as_bytes()[0] as char, ok)
        };
        let both = ["SCRAM-SHA-256-PLUS", "SCRAM-SHA-256"];
        let hash = &b"the certificate's hash"[..];
        assert_eq!(run(&both, true, Some(hash), Some(hash)), ("SCRAM-SHA-256-PLUS", 'p', true));
        assert_eq!(run(&both, true, Some(b"another certificate"), Some(hash)), ("SCRAM-SHA-256-PLUS", 'p', false));
        assert_eq!(run(&both[1..], true, Some(hash), None), ("SCRAM-SHA-256", 'y', true));
        assert_eq!(run(&both[1..], false, None, None), ("SCRAM-SHA-256", 'n', true));
        // a certificate whose hash can't be made (EdDSA, or one in the middle) where PLUS is
        // offered: no login, never `n` over TLS (with channel_binding=disable, `n`)
        let e = Scram::start(&both, "prefer", true, None, "n").err().unwrap();
        assert!(e.to_string().contains("can't be made"), "{e}");
        let (_, mechanism, first) = Scram::start(&both, "disable", true, Some(hash), "n").unwrap();
        assert_eq!((mechanism, &first[..3]), ("SCRAM-SHA-256", "n,,"));
        // require: bound or nothing
        assert_eq!(Scram::start(&both, "require", true, Some(hash), "n").unwrap().1, "SCRAM-SHA-256-PLUS");
        assert!(Scram::start(&both[1..], "require", true, Some(hash), "n").is_err());
        assert!(Scram::start(&both, "require", false, None, "n").is_err());
        assert!(Scram::start(&["SCRAM-SHA-1"], "prefer", false, None, "n").is_err());
    }
}
