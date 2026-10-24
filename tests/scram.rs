//! Logging in over the Postgres protocol as Postgres 16 does it: SCRAM-SHA-256 for a role with a
//! password (SCRAM-SHA-256-PLUS, bound to the TLS certificate, where TLS is on), MD5 for a role
//! whose password is an md5 hash, SCRAM against an agent's verifier for an agent (one made before
//! agents had verifiers gets one from its token over HTTP, or in the clear once on loopback), SCRAM against the server's and admin tokens
//! for other names, and a mock exchange with a salt of its own for a name that can't log in.
//! Checked with the `postgres` crate (tokio-postgres's SCRAM and its verifiers), psql (libpq's,
//! `\password` included), raw messages, and verifiers made by the reference Postgres (host=/tmp
//! on PGPORT; skipped without one unless CHRONOS_REQUIRE_ORACLE is set). Role and agent names
//! start `r4_`/`r4` and `r3c_`.

mod common;

use postgres::{Client, Config, NoTls};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

struct Server {
    child: Child,
    dir: PathBuf,
    pg: u16,
    http: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A fresh folder for a server (and its certificate).
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chronos-scram-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Starts a server in `dir` with these arguments (a token, TLS files). These tests are about how a
/// login proves what it knows, so most pass `--unsafe`: the server token is then the database's
/// own user, as it was before 0.4 (`tokens_and_the_guest` and tests/safe.rs cover the guest).
fn start(dir: PathBuf, args: &[&str]) -> Server {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.arg("serve").arg(dir.join("db")).args(args);
    let s = common::serve(&mut cmd, true);
    Server { child: s.child, dir, pg: s.pg, http: s.http }
}

fn config(port: u16, user: &str, password: Option<&str>) -> Config {
    let mut c = Config::new();
    c.host("127.0.0.1").port(port).user(user).dbname("main");
    if let Some(p) = password {
        c.password(p);
    }
    c
}

fn connect(port: u16, user: &str, password: Option<&str>) -> Result<Client, postgres::Error> {
    config(port, user, password).connect(NoTls)
}

/// A refused login's SQLSTATE and message.
fn refusal(r: Result<Client, postgres::Error>) -> (String, String) {
    let e = r.err().expect("the login is refused");
    match e.as_db_error() {
        Some(d) => (d.code().code().into(), d.message().into()),
        None => (String::new(), e.to_string()),
    }
}

fn failed(user: &str) -> (String, String) {
    ("28P01".into(), format!("password authentication failed for user \"{user}\""))
}

fn text(c: &mut Client, q: &str) -> String {
    c.query_one(q, &[]).unwrap().get(0)
}

fn startup(user: &str) -> Vec<u8> {
    let mut b = 196608i32.to_be_bytes().to_vec();
    for s in ["user", user, "database", "main"] {
        b.extend(s.as_bytes());
        b.push(0);
    }
    b.push(0);
    [((b.len() + 4) as i32).to_be_bytes().to_vec(), b].concat()
}

/// The server's next message: its tag and body (tag 0: the connection closed).
fn message(s: &mut impl Read) -> (u8, Vec<u8>) {
    let mut head = [0u8; 5];
    if s.read_exact(&mut head).is_err() {
        return (0, vec![]);
    }
    let n = i32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    let mut body = vec![0; n - 4];
    s.read_exact(&mut body).unwrap();
    (head[0], body)
}

fn send(s: &mut impl Write, tag: u8, body: &[u8]) {
    let mut m = vec![tag];
    m.extend(((body.len() + 4) as i32).to_be_bytes());
    m.extend(body);
    let _ = s.write_all(&m);
}

/// An ErrorResponse's severity, SQLSTATE and message.
fn error_fields(body: &[u8]) -> (String, String, String) {
    let (mut sev, mut code, mut msg) = (String::new(), String::new(), String::new());
    for f in body.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        let v = String::from_utf8_lossy(&f[1..]).into_owned();
        match f[0] {
            b'V' => sev = v,
            b'C' => code = v,
            b'M' => msg = v,
            _ => {}
        }
    }
    (sev, code, msg)
}

/// What the server asks a client logging in as `user` to prove before it says anything: the
/// authentication request (0 nothing, 3 a cleartext password, 5 MD5, 10 SASL) and, for SASL,
/// the mechanisms offered.
fn asks(port: u16, user: &str) -> (i32, Vec<String>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(&startup(user)).unwrap();
    let (tag, body) = message(&mut s);
    assert_eq!(tag, b'R', "{:?}", error_fields(&body));
    let code = i32::from_be_bytes(body[..4].try_into().unwrap());
    let mechanisms = match code {
        10 => {
            body[4..].split(|&b| b == 0).filter(|m| !m.is_empty()).map(|m| String::from_utf8_lossy(m).into()).collect()
        }
        _ => vec![],
    };
    (code, mechanisms)
}

fn scram() -> (i32, Vec<String>) {
    (10, vec!["SCRAM-SHA-256".into()])
}

#[test]
fn roles_log_in_with_scram_or_md5() {
    let s = start(folder("roles"), &["--token", "s3cret-s3cret-s3cret-s3cret", "--unsafe"]);
    let p = s.pg;
    // a name that is no role proves with SCRAM that it knows the token, which never crosses the wire
    assert_eq!(asks(p, "admin"), scram());
    let mut admin = connect(p, "admin", Some("s3cret-s3cret-s3cret-s3cret")).unwrap();
    assert_eq!(refusal(connect(p, "admin", Some("wrong"))), failed("admin"));
    assert!(connect(p, "admin", None).is_err());

    // a role with a password: SCRAM against its verifier
    admin.batch_execute("create role r4_alice login password 'wonderland'; create role r4_nopw login").unwrap();
    assert_eq!(asks(p, "r4_alice"), scram());
    let mut alice = connect(p, "r4_alice", Some("wonderland")).unwrap();
    assert_eq!(text(&mut alice, "select current_user::text"), "r4_alice");
    assert_eq!(refusal(connect(p, "r4_alice", Some("wonder"))), failed("r4_alice"));
    assert_eq!(
        refusal(connect(p, "r4_alice", Some("s3cret-s3cret-s3cret-s3cret"))),
        failed("r4_alice"),
        "the token isn't its password"
    );
    // ALTER ROLE ... PASSWORD, by a superuser and by the role itself: the next login takes the new one
    admin.batch_execute("alter role r4_alice password 'looking-glass'").unwrap();
    assert_eq!(refusal(connect(p, "r4_alice", Some("wonderland"))), failed("r4_alice"));
    let mut alice = connect(p, "r4_alice", Some("looking-glass")).unwrap();
    alice.batch_execute("alter role r4_alice password 'queen'").unwrap();
    connect(p, "r4_alice", Some("queen")).unwrap();

    // a verifier made by the client (tokio-postgres's, with its own salt) is kept as given
    let verifier = postgres_protocol::password::scram_sha_256(b"cheshire");
    admin.batch_execute(&format!("alter role r4_alice password '{verifier}'")).unwrap();
    assert_eq!(text(&mut admin, "select rolpassword from pg_authid where rolname = 'r4_alice'"), verifier);
    connect(p, "r4_alice", Some("cheshire")).unwrap();

    // password_encryption = md5: Postgres's md5 hash, and the MD5 challenge to log in
    admin.batch_execute("set password_encryption = 'md5'; create role r4_hatter login password 'tea-party'").unwrap();
    assert_eq!(text(&mut admin, "show password_encryption"), "md5");
    assert_eq!(
        text(&mut admin, "select rolpassword from pg_authid where rolname = 'r4_hatter'"),
        postgres_protocol::password::md5(b"tea-party", "r4_hatter")
    );
    assert_eq!(asks(p, "r4_hatter"), (5, vec![]));
    let mut hatter = connect(p, "r4_hatter", Some("tea-party")).unwrap();
    assert_eq!(text(&mut hatter, "select session_user::text"), "r4_hatter");
    assert_eq!(refusal(connect(p, "r4_hatter", Some("tea"))), failed("r4_hatter"));
    // a client-made md5 hash too
    let md5 = postgres_protocol::password::md5(b"march", "r4_hare");
    admin.batch_execute(&format!("create role r4_hare login password '{md5}'")).unwrap();
    connect(p, "r4_hare", Some("march")).unwrap();
    // and back: a new password is a verifier again, and the login SCRAM
    admin.batch_execute("reset password_encryption; alter role r4_hatter password 'tea-party'").unwrap();
    assert_eq!(asks(p, "r4_hatter"), scram());
    connect(p, "r4_hatter", Some("tea-party")).unwrap();

    // a role without a password, where the server needs one: refused as a wrong password, after SCRAM;
    // a predefined role (NOLOGIN, no password) too, never logging in with the token
    assert_eq!(asks(p, "r4_nopw"), scram());
    assert_eq!(refusal(connect(p, "r4_nopw", Some("s3cret-s3cret-s3cret-s3cret"))), failed("r4_nopw"));
    assert_eq!(refusal(connect(p, "pg_monitor", Some("s3cret-s3cret-s3cret-s3cret"))), failed("pg_monitor"));
    // NOLOGIN is told after the password checks out, and after AuthenticationOk, as in Postgres
    admin
        .batch_execute(
            "create role r4_nologin password 'x'; create role r4_old login password 'x' valid until '2001-01-01';
             set password_encryption = 'md5'; create role r4_old_md5 login password 'x' valid until '2001-01-01'",
        )
        .unwrap();
    assert_eq!(refusal(connect(p, "r4_nologin", Some("y"))), failed("r4_nologin"));
    assert_eq!(
        refusal(connect(p, "r4_nologin", Some("x"))),
        ("28000".into(), "role \"r4_nologin\" is not permitted to log in".into())
    );
    assert_eq!(by_hand(p, "r4_nologin", "x"), ["R10", "R11", "R12", "R0", "E28000"]);
    // a password past VALID UNTIL goes through a doomed exchange: the server never proves it knows
    // the password (no SASLFinal), right or wrong, so the exchange doesn't tell which; never MD5
    assert_eq!(refusal(connect(p, "r4_old", Some("x"))), failed("r4_old"));
    assert_eq!(by_hand(p, "r4_old", "x"), ["R10", "R11", "E28P01"]);
    assert_eq!(by_hand(p, "r4_old", "y"), ["R10", "R11", "E28P01"]);
    assert_eq!(asks(p, "r4_old_md5"), scram());
    assert_eq!(by_hand(p, "r4_old_md5", "x"), ["R10", "R11", "E28P01"]);
    assert_eq!(by_hand(p, "r4_alice", "cheshire"), ["R10", "R11", "R12", "R0"]);

    // an agent proves it knows its token with SCRAM, against the verifier made with the token
    let token: String = admin.query_one("create agent r4bot", &[]).unwrap().get("token");
    assert_eq!(asks(p, "r4bot"), scram());
    let mut bot = connect(p, "r4bot", Some(&token)).unwrap();
    assert_eq!(text(&mut bot, "select current_user::text"), "r4bot");
    assert!(connect(p, "r4bot", Some("nope")).is_err());
    // and may not read the passwords roles keep (with an md5 hash it could answer the MD5
    // challenge as that role), however it asks
    admin.batch_execute("create role r4_boss superuser login password 'boss'").unwrap();
    for q in [
        "select rolpassword from pg_authid where rolname = 'r4_boss'",
        "select passwd from pg_shadow where usename = 'r4_boss'",
        "select p from (select rolpassword p from pg_authid) s where p is not null",
        "with s as (select * from pg_shadow) select count(*) from s",
        "select rolname from pg_catalog.pg_authid",
    ] {
        let e = bot.query(q, &[]).unwrap_err();
        assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{q}: {e}");
    }
    assert_eq!(text(&mut bot, "select rolpassword from pg_roles where rolname = 'r4_boss'"), "********");
    // the database's own user still may
    assert!(text(&mut admin, "select rolpassword from pg_authid where rolname = 'r4_boss'").starts_with("md5"));
}

/// A SCRAM login by hand, with tokio-postgres's client side: the authentication requests the
/// server sent (`R10`, ...), then an error's SQLSTATE (`E28P01`), until it is ready or gone. A
/// SASLFinal is checked: the server must know the password.
fn by_hand(port: u16, user: &str, password: &str) -> Vec<String> {
    use postgres_protocol::authentication::sasl::{ChannelBinding, ScramSha256};
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.write_all(&startup(user)).unwrap();
    let mut scram = ScramSha256::new(password.as_bytes(), ChannelBinding::unsupported());
    let mut seen = vec![];
    loop {
        let (tag, body) = message(&mut c);
        match tag {
            b'R' => {
                let code = i32::from_be_bytes(body[..4].try_into().unwrap());
                seen.push(format!("R{code}"));
                match code {
                    10 => {
                        let mut b = b"SCRAM-SHA-256\0".to_vec();
                        b.extend((scram.message().len() as i32).to_be_bytes());
                        b.extend(scram.message());
                        send(&mut c, b'p', &b);
                    }
                    11 => {
                        scram.update(&body[4..]).unwrap();
                        send(&mut c, b'p', scram.message());
                    }
                    12 => scram.finish(&body[4..]).expect("the server's signature"),
                    _ => {}
                }
            }
            b'E' => {
                seen.push(format!("E{}", error_fields(&body).1));
                return seen;
            }
            0 | b'Z' => return seen,
            _ => {}
        }
    }
}

#[test]
fn loopback_without_a_token() {
    let s = start(folder("loopback"), &[]);
    let p = s.pg;
    // no password for a name that is no role, or a role without one, as before
    assert_eq!(asks(p, "tester"), (0, vec![]));
    let mut admin = connect(p, "tester", None).unwrap();
    admin
        .batch_execute(
            "create role r4_free login; create role r4_kept login password 'pw';
             set password_encryption = md5; create role r4_md5 login password 'pw'",
        )
        .unwrap();
    assert_eq!(asks(p, "r4_free"), (0, vec![]));
    connect(p, "r4_free", None).unwrap();
    assert_eq!(
        refusal(connect(p, "pg_monitor", None)),
        ("28000".into(), "role \"pg_monitor\" is not permitted to log in".into())
    );
    // a role with a password needs it, on loopback too
    assert_eq!(asks(p, "r4_kept"), scram());
    assert!(connect(p, "r4_kept", None).is_err());
    assert_eq!(refusal(connect(p, "r4_kept", Some("wrong"))), failed("r4_kept"));
    connect(p, "r4_kept", Some("pw")).unwrap();
    assert_eq!(asks(p, "r4_md5"), (5, vec![]));
    assert_eq!(refusal(connect(p, "r4_md5", Some("wrong"))), failed("r4_md5"));
    connect(p, "r4_md5", Some("pw")).unwrap();
    // but a role's password guards only its own name: without a token, loopback is trust for
    // every other name, which is a superuser and may become any role (see docs/security.md)
    let mut other = connect(p, "nobody", None).unwrap();
    other.batch_execute("set session authorization r4_kept").unwrap();
    assert_eq!(text(&mut other, "select current_user::text"), "r4_kept");
}

/// `--safe` with both tokens: the admin token is the database's own user, the server's the guest.
#[test]
fn tokens_and_the_guest() {
    let s = start(
        folder("safe"),
        &["--safe", "--token", "s3cret-s3cret-s3cret-s3cret", "--admin-token", "ADMIN-ADMIN-ADMIN-ADMIN-0"],
    );
    let p = s.pg;
    assert_eq!(asks(p, "tester"), scram());
    let mut admin = connect(p, "tester", Some("ADMIN-ADMIN-ADMIN-ADMIN-0")).unwrap();
    admin.batch_execute("create table g (id int primary key); insert into g values (1)").unwrap();
    let mut guest = connect(p, "tester", Some("s3cret-s3cret-s3cret-s3cret")).unwrap();
    let e = guest.batch_execute("insert into g values (2)").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");
    assert_eq!(guest.query_one("select count(*) from g", &[]).unwrap().get::<_, i64>(0), 1);
    assert_eq!(refusal(connect(p, "tester", Some("other"))), failed("tester"));
    // the guest may not read the passwords roles keep either
    admin.batch_execute("create role r4_kept login password 'pw'").unwrap();
    for q in ["select rolpassword from pg_authid", "select passwd from pg_shadow"] {
        let e = guest.query(q, &[]).unwrap_err();
        assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{q}: {e}");
    }
    // with only an admin token, any password is still taken (the guest), in the clear
    let s = start(folder("admin-only"), &["--safe", "--admin-token", "ADMIN-ADMIN-ADMIN-ADMIN-0"]);
    assert_eq!(asks(s.pg, "tester"), (3, vec![]));
}

/// A self-signed certificate for localhost: `key` is openssl's -newkey argument, `hash` the
/// signature's digest (`-sha384`, ...; none for Ed25519).
fn cert(dir: &Path, key: &[&str], hash: &str) {
    let ok = Command::new("openssl")
        .args(["req", "-x509", "-nodes", "-days", "1"])
        .args(Some(hash).filter(|h| !h.is_empty()))
        .arg("-newkey")
        .args(key)
        .args(["-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"])
        .arg("-keyout")
        .arg(dir.join("key.pem"))
        .arg("-out")
        .arg(dir.join("cert.pem"))
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "openssl could not make a certificate");
}

fn tls_server(name: &str, key: &[&str], hash: &str) -> Server {
    let dir = folder(name);
    cert(&dir, key, hash);
    let (c, k) = (dir.join("cert.pem"), dir.join("key.pem"));
    start(
        dir,
        &[
            "--token",
            "s3cret-s3cret-s3cret-s3cret",
            "--unsafe",
            "--tls-cert",
            c.to_str().unwrap(),
            "--tls-key",
            k.to_str().unwrap(),
        ],
    )
}

/// rustls for the `postgres` crate, with tls-server-end-point channel binding: the SHA-256 of the
/// server's certificate (these tests' certificates are signed with ECDSA and SHA-256), or as
/// `lie` says, to play a server in the middle.
mod tls {
    use postgres::Socket;
    use postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    #[derive(Clone)]
    pub struct Rustls(pub Arc<rustls::ClientConfig>, pub Option<Vec<u8>>);

    /// Any certificate, as libpq's sslmode=require takes it (the handshake's signatures are still checked).
    #[derive(Debug)]
    struct Unchecked(Arc<CryptoProvider>);

    impl ServerCertVerifier for Unchecked {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    impl Rustls {
        /// sslmode=require: TLS, whoever's certificate the server has.
        pub fn new(lie: Option<Vec<u8>>) -> Rustls {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Unchecked(provider)))
                .with_no_client_auth();
            Rustls(Arc::new(cfg), lie)
        }
    }

    pub struct Stream(tokio_rustls::client::TlsStream<Socket>, Option<Vec<u8>>);

    impl MakeTlsConnect<Socket> for Rustls {
        type Stream = Stream;
        type TlsConnect = Rustls;
        type Error = io::Error;
        fn make_tls_connect(&mut self, _: &str) -> io::Result<Rustls> {
            Ok(self.clone())
        }
    }

    impl TlsConnect<Socket> for Rustls {
        type Stream = Stream;
        type Error = io::Error;
        type Future = Pin<Box<dyn Future<Output = io::Result<Stream>> + Send>>;
        fn connect(self, s: Socket) -> Self::Future {
            let c = tokio_rustls::TlsConnector::from(self.0);
            let name = ServerName::try_from("localhost").unwrap().to_owned();
            Box::pin(async move { Ok(Stream(c.connect(name, s).await?, self.1)) })
        }
    }

    impl TlsStream for Stream {
        fn channel_binding(&self) -> ChannelBinding {
            let cert = self.0.get_ref().1.peer_certificates().and_then(|c| c.first());
            match (&self.1, cert) {
                (Some(lie), _) => ChannelBinding::tls_server_end_point(lie.clone()),
                (None, Some(c)) => ChannelBinding::tls_server_end_point(
                    ring::digest::digest(&ring::digest::SHA256, c).as_ref().to_vec(),
                ),
                (None, None) => ChannelBinding::none(),
            }
        }
    }

    impl AsyncRead for Stream {
        fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }
}

#[test]
fn channel_binding_over_tls() {
    let s = tls_server("tls", &["ec", "-pkeyopt", "ec_paramgen_curve:prime256v1"], "-sha256");
    let p = s.pg;
    let login = |user: &str, password: &str, binding: &str, lie: Option<Vec<u8>>| {
        let mut c = config(p, user, Some(password));
        c.ssl_mode(postgres::config::SslMode::Require);
        c.channel_binding(match binding {
            "require" => postgres::config::ChannelBinding::Require,
            "prefer" => postgres::config::ChannelBinding::Prefer,
            _ => postgres::config::ChannelBinding::Disable,
        });
        c.connect(tls::Rustls::new(lie))
    };
    // channel_binding=require needs SCRAM-SHA-256-PLUS: the token's login and a role's
    let mut admin = login("admin", "s3cret-s3cret-s3cret-s3cret", "require", None).unwrap();
    admin.batch_execute("create role r4_tls login password 'bound'").unwrap();
    login("r4_tls", "bound", "require", None).unwrap();
    assert_eq!(refusal(login("r4_tls", "unbound", "require", None)), failed("r4_tls"));
    // prefer (tokio-postgres's default) binds too; disable logs in with plain SCRAM
    login("r4_tls", "bound", "prefer", None).unwrap();
    login("r4_tls", "bound", "disable", None).unwrap();
    // a client that saw another certificate (a server in the middle) is refused
    let lie = Some(vec![7; 32]);
    assert_eq!(
        refusal(login("r4_tls", "bound", "prefer", lie)),
        ("28000".into(), "SCRAM channel binding check failed".into())
    );

    // a client that could bind but thinks the server can't (`y`), where the server offered PLUS
    let tcp = TcpStream::connect(("127.0.0.1", p)).unwrap();
    let mut tcp2 = tcp.try_clone().unwrap();
    tcp2.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).unwrap(); // SSLRequest
    let mut yes = [0];
    tcp2.read_exact(&mut yes).unwrap();
    assert_eq!(yes[0], b'S');
    let tls::Rustls(cfg, _) = tls::Rustls::new(None);
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap().to_owned();
    let mut t = rustls::StreamOwned::new(rustls::ClientConnection::new(cfg, name).unwrap(), tcp);
    t.write_all(&startup("r4_tls")).unwrap();
    let (tag, body) = message(&mut t);
    assert_eq!((tag, &body[4..]), (b'R', &b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0"[..]));
    let first = b"y,,n=,r=0123456789abcdef";
    let mut b = b"SCRAM-SHA-256\0".to_vec();
    b.extend((first.len() as i32).to_be_bytes());
    b.extend(first);
    send(&mut t, b'p', &b);
    let (tag, body) = message(&mut t);
    assert_eq!(tag, b'E');
    assert_eq!(error_fields(&body), ("FATAL".into(), "28000".into(), "SCRAM channel binding negotiation error".into()));
}

fn have_psql() -> bool {
    let ok = Command::new("psql").arg("--version").output().is_ok();
    if !ok {
        eprintln!("skipped: psql not installed");
    }
    ok
}

/// psql's `\password` (after `before`, in the same session), its new password read twice from
/// stdin: psql reads it from the terminal when there is one, so it runs in a session of its own.
fn psql_password(url: &str, before: &str, user: &str, password: &str) -> std::process::Output {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("psql");
    cmd.args([url, "-X", "-v", "ON_ERROR_STOP=1", "-c", before, "-c", &format!("\\password {user}")]);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe and touches nothing of this process
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(format!("{password}\n{password}\n").as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn psql(url: &str, password: &str, sql: &str) -> Result<String, String> {
    let out = Command::new("psql")
        .args([url, "-X", "-A", "-t", "-v", "ON_ERROR_STOP=1", "-c", sql])
        .env("PGPASSWORD", password)
        .output()
        .unwrap();
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        false => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
    }
}

/// libpq's SCRAM (and MD5), and `\password`: it asks `password_encryption`, hashes the password
/// itself and sends `ALTER USER ... PASSWORD '<verifier>'`.
#[test]
fn psql_logs_in_and_sets_passwords() {
    if !have_psql() {
        return;
    }
    let s = start(folder("psql"), &["--token", "s3cret-s3cret-s3cret-s3cret", "--unsafe"]);
    let url = |user: &str| format!("postgres://{user}@127.0.0.1:{}/main", s.pg);
    psql(&url("admin"), "s3cret-s3cret-s3cret-s3cret", "create role r4_psql login password 'first'").unwrap();
    assert_eq!(psql(&url("r4_psql"), "first", "select current_user").unwrap(), "r4_psql");
    let e = psql(&url("r4_psql"), "wrong", "select 1").unwrap_err();
    assert!(e.contains("FATAL:  password authentication failed for user \"r4_psql\""), "{e}");

    // \password as the role itself, then the new password logs in
    let mut admin = connect(s.pg, "admin", Some("s3cret-s3cret-s3cret-s3cret")).unwrap();
    let out =
        psql_password(&format!("postgres://r4_psql:first@127.0.0.1:{}/main", s.pg), "select 1", "r4_psql", "second");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let kept = text(&mut admin, "select rolpassword from pg_authid where rolname = 'r4_psql'");
    assert!(kept.starts_with("SCRAM-SHA-256$4096:"), "{kept}");
    assert_eq!(psql(&url("r4_psql"), "second", "select current_user").unwrap(), "r4_psql");
    assert!(psql(&url("r4_psql"), "first", "select 1").is_err());

    // with password_encryption = md5 (\password asks it with SHOW), an md5 hash
    let url_second = format!("postgres://r4_psql:second@127.0.0.1:{}/main", s.pg);
    let out = psql_password(&url_second, "set password_encryption = 'md5'", "r4_psql", "third");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let kept = text(&mut admin, "select rolpassword from pg_authid where rolname = 'r4_psql'");
    assert_eq!(kept, postgres_protocol::password::md5(b"third", "r4_psql"));
    assert_eq!(psql(&url("r4_psql"), "third", "select current_user").unwrap(), "r4_psql");
}

/// libpq binds to the certificate with its own reading of its signature's hash: SHA-256 for a
/// SHA-1 signature, SHA-384 for a SHA-384 one, SHA-224 (which `ring` hasn't) for a SHA-224 one. A
/// certificate whose signature has no hash of its own (Ed25519) gets plain SCRAM, which libpq's
/// `y` (it could bind) logs in with.
#[test]
fn psql_binds_to_the_certificate() {
    if !have_psql() {
        return;
    }
    let rsa = ["rsa:2048"];
    for (name, key, hash) in [
        ("ec256", &["ec", "-pkeyopt", "ec_paramgen_curve:prime256v1"][..], "-sha256"),
        ("rsa384", &rsa[..], "-sha384"),
        ("rsa1", &rsa[..], "-sha1"),
        ("rsa224", &rsa[..], "-sha224"),
        ("ed25519", &["ed25519"][..], ""),
    ] {
        let s = tls_server(name, key, hash);
        let url = |binding: &str| {
            format!("postgres://admin@127.0.0.1:{}/main?sslmode=require&channel_binding={binding}", s.pg)
        };
        let bound = psql(&url("require"), "s3cret-s3cret-s3cret-s3cret", "select 1");
        if name == "ed25519" {
            let e = bound.unwrap_err();
            assert!(e.contains("channel binding"), "{e}");
            assert_eq!(psql(&url("prefer"), "s3cret-s3cret-s3cret-s3cret", "select 1").unwrap(), "1");
        } else {
            assert_eq!(bound.unwrap_or_else(|e| panic!("{name}: {e}")), "1");
        }
        assert!(psql(&url("require"), "wrong", "select 1").is_err());
    }
}

/// A verifier (and an md5 hash) made by Postgres itself logs in here when given as the password.
#[test]
fn verifiers_made_by_postgres_log_in() {
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    let Ok(mut pg) = Client::connect(&format!("host=/tmp port={port} dbname=postgres"), NoTls) else {
        assert!(std::env::var_os("CHRONOS_REQUIRE_ORACLE").is_none(), "no Postgres at host=/tmp");
        eprintln!("skipped: no Postgres at host=/tmp");
        return;
    };
    let roles = ["r4_oracle", "r4_oracle_md5"];
    let drop = |pg: &mut Client| {
        for r in roles {
            pg.batch_execute(&format!("drop role if exists {r}")).unwrap();
        }
    };
    drop(&mut pg);
    pg.batch_execute(
        "create role r4_oracle login password 'from-postgres';
         set password_encryption = 'md5'; create role r4_oracle_md5 login password 'md5-from-postgres'",
    )
    .unwrap();
    let kept: Vec<String> = roles
        .iter()
        .map(|r| text(&mut pg, &format!("select rolpassword from pg_authid where rolname = '{r}'")))
        .collect();
    drop(&mut pg);
    assert!(kept[0].starts_with("SCRAM-SHA-256$") && kept[1].starts_with("md5"), "{kept:?}");

    let s = start(folder("oracle"), &[]);
    let mut admin = connect(s.pg, "admin", None).unwrap();
    for (r, k) in roles.iter().zip(&kept) {
        admin.batch_execute(&format!("create role {r} login; alter role {r} password '{k}'")).unwrap();
        assert_eq!(&text(&mut admin, &format!("select rolpassword from pg_authid where rolname = '{r}'")), k);
    }
    connect(s.pg, "r4_oracle", Some("from-postgres")).unwrap();
    assert_eq!(refusal(connect(s.pg, "r4_oracle", Some("from-chronos"))), failed("r4_oracle"));
    connect(s.pg, "r4_oracle_md5", Some("md5-from-postgres")).unwrap();
    assert_eq!(refusal(connect(s.pg, "r4_oracle_md5", Some("from-chronos"))), failed("r4_oracle_md5"));
}

/// Hostile SASL messages: each is refused with Postgres's error (FATAL, then the connection
/// closes), and the server keeps serving.
#[test]
fn garbage_sasl_messages_are_refused() {
    let s = start(folder("garbage"), &[]);
    let p = s.pg;
    connect(p, "tester", None).unwrap().batch_execute("create role r4_fuzz login password 'pw'").unwrap();
    // (SASLInitialResponse, SASLResponse if the first gets that far): the SQLSTATE refused with
    let initial = |mech: &[u8], data: Option<&[u8]>| {
        let mut b = mech.to_vec();
        b.push(0);
        match data {
            Some(d) => {
                b.extend((d.len() as i32).to_be_bytes());
                b.extend(d);
            }
            None => b.extend((-1i32).to_be_bytes()),
        }
        b
    };
    let try_login = |first: Vec<u8>, last: Option<&[u8]>| -> String {
        let mut c = TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&startup("r4_fuzz")).unwrap();
        assert_eq!(message(&mut c).0, b'R');
        send(&mut c, b'p', &first);
        let (mut tag, mut body) = message(&mut c);
        if tag == b'R'
            && let Some(last) = last
        {
            send(&mut c, b'p', last);
            (tag, body) = message(&mut c);
        }
        match tag {
            b'E' => {
                let (sev, code, _) = error_fields(&body);
                assert_eq!(sev, "FATAL");
                assert_eq!(message(&mut c).0, 0, "the connection closes after a refusal");
                code
            }
            t => format!("{}", t as char),
        }
    };
    let good = b"n,,n=,r=fyko+d2lbbFgONRv9qkxdawL";
    assert_eq!(try_login(initial(b"SCRAM-SHA-1", Some(good)), None), "08P01");
    assert_eq!(try_login(initial(b"SCRAM-SHA-256-PLUS", Some(good)), None), "08P01", "not offered without TLS");
    assert_eq!(try_login(b"SCRAM-SHA-256".to_vec(), None), "08P01", "no terminator");
    assert_eq!(try_login(b"SCRAM-SHA-256\0\0\0".to_vec(), None), "08P01", "a short length");
    let mut long = initial(b"SCRAM-SHA-256", Some(good));
    long.push(b'x');
    assert_eq!(try_login(long, None), "08P01", "more than its length says");
    let mut over = b"SCRAM-SHA-256\0".to_vec();
    over.extend(1000i32.to_be_bytes());
    over.extend(good);
    assert_eq!(try_login(over, None), "08P01", "less than its length says");
    let mut negative = b"SCRAM-SHA-256\0".to_vec();
    negative.extend((-7i32).to_be_bytes());
    assert_eq!(try_login(negative, None), "08P01");
    assert_eq!(try_login(initial(b"SCRAM-SHA-256", Some(b"")), None), "08P01");
    assert_eq!(try_login(initial(b"SCRAM-SHA-256", Some(b"q,,n=,r=a")), None), "08P01");
    assert_eq!(try_login(initial(b"SCRAM-SHA-256", Some(b"n,a=x,n=,r=a")), None), "0A000");
    // a SASL message is 1 KB at most, its length word included: one longer (only its length is
    // sent, so the connection closes cleanly), or under 4, is 28P01, as Postgres refuses it
    let fits = initial(b"SCRAM-SHA-256", Some(&[b'n'; 1020 - 18]));
    assert_eq!(fits.len() + 4, 1024);
    assert_eq!(try_login(fits, None), "08P01", "1,024 bytes: read, and malformed");
    let raw = |first: bool, length: i32| {
        let mut c = TcpStream::connect(("127.0.0.1", p)).unwrap();
        c.write_all(&startup("r4_fuzz")).unwrap();
        message(&mut c);
        if first {
            send(&mut c, b'p', &initial(b"SCRAM-SHA-256", Some(good)));
            assert_eq!(message(&mut c).0, b'R');
        }
        c.write_all(&[&[b'p'][..], &length.to_be_bytes()].concat()).unwrap();
        let (tag, body) = message(&mut c);
        (tag, error_fields(&body).1)
    };
    for length in [1025, 2004, 60_004, i32::MAX, 0, 3, -1] {
        assert_eq!(raw(false, length), (b'E', "28P01".to_string()), "a first message of length {length}");
    }
    assert_eq!(raw(true, 70_004), (b'E', "28P01".to_string()), "a final message of 70,000 bytes");
    // no initial response: an empty challenge, then the first message
    assert_eq!(try_login(initial(b"SCRAM-SHA-256", None), Some(b"garbage")), "08P01");
    // a good first message, then a bad final one
    for last in [&b""[..], b"c=biws", b"c=biws,r=fyko,p=AAAA", b"c=eSws,r=x,p=y", &[0xff; 1000], b"c=biws,r=,p=,,,,"] {
        assert_eq!(try_login(initial(b"SCRAM-SHA-256", Some(good)), Some(last)), "08P01", "{last:?}");
    }
    // random bytes
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..200 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let n = (seed % 300) as usize;
        let junk: Vec<u8> = (0..n).map(|i| (seed >> (i % 56)) as u8 ^ i as u8).collect();
        let code = try_login(junk.clone(), Some(&junk));
        assert!(["08P01", "0A000", "28000", "28P01"].contains(&code.as_str()), "{code} for {junk:?}");
    }
    // a password message where SASL was asked: Postgres's protocol violation
    let mut c = TcpStream::connect(("127.0.0.1", p)).unwrap();
    c.write_all(&startup("r4_fuzz")).unwrap();
    message(&mut c);
    send(&mut c, b'Q', b"select 1\0");
    let (tag, body) = message(&mut c);
    assert_eq!((tag, error_fields(&body).1.as_str()), (b'E', "08P01"));
    // an MD5 password message too long to be one just closes the connection, as in Postgres
    connect(p, "tester", None)
        .unwrap()
        .batch_execute("set password_encryption = 'md5'; create role r4_fuzz_md5 login password 'pw'")
        .unwrap();
    let mut c = TcpStream::connect(("127.0.0.1", p)).unwrap();
    c.write_all(&startup("r4_fuzz_md5")).unwrap();
    let (tag, body) = message(&mut c);
    assert_eq!((tag, &body[..4]), (b'R', &5i32.to_be_bytes()[..]));
    c.write_all(&[&[b'p'][..], &70_004i32.to_be_bytes()].concat()).unwrap();
    assert_eq!(message(&mut c).0, 0);
    // still serving
    connect(p, "r4_fuzz", Some("pw")).unwrap();
    connect(p, "r4_fuzz_md5", Some("pw")).unwrap();
}

/// `chronos import`'s own client answers the MD5 challenge (here, of a Chronos server).
#[test]
fn import_logs_in_with_md5() {
    let s = start(folder("import"), &[]);
    connect(s.pg, "tester", None)
        .unwrap()
        .batch_execute(
            "create table t (id int primary key, v text); insert into t values (1, 'a');
             set password_encryption = 'md5'; create role r4_import login superuser password 'pw'",
        )
        .unwrap();
    let import = |password: &str| {
        let to = s.dir.join(format!("to-{password}"));
        let url = format!("postgres://r4_import:{password}@127.0.0.1:{}/main?sslmode=disable", s.pg);
        // (MD5 over a connection that isn't TLS goes only when asked for: see tests/pg_import.rs)
        Command::new(env!("CARGO_BIN_EXE_chronos"))
            .arg("import")
            .arg(to)
            .args([&url, "--dry-run", "--allow-plaintext-password"])
            .output()
            .unwrap()
    };
    let out = import("pw");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(said.contains("would import 1 of 1 tables"), "{said}");
    let out = import("bad");
    assert!(String::from_utf8_lossy(&out.stderr).contains("password authentication failed for user \"r4_import\""));
}

/// What a client that doesn't know the password sees of a login as `user`: the authentication
/// request and the mechanisms offered, then, for SASL, the server-first-message's salt, iteration
/// count and nonce length (what the server adds to the client's), and how a proof that proves
/// nothing ends (severity, SQLSTATE, message).
#[derive(Debug, Default)]
struct Seen {
    asks: i32,
    mechanisms: Vec<String>,
    salt: String,
    iterations: u32,
    nonce: usize,
    end: (String, String, String),
}

fn exchange(port: u16, user: &str) -> Seen {
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.write_all(&startup(user)).unwrap();
    let (tag, body) = message(&mut c);
    assert_eq!(tag, b'R', "{user}: {:?}", error_fields(&body));
    let mut seen = Seen { asks: i32::from_be_bytes(body[..4].try_into().unwrap()), ..Seen::default() };
    if seen.asks != 10 {
        return seen;
    }
    seen.mechanisms =
        body[4..].split(|&b| b == 0).filter(|m| !m.is_empty()).map(|m| String::from_utf8_lossy(m).into()).collect();
    let client_nonce = "r3cNonceOfTheClient";
    let first = format!("n,,n=,r={client_nonce}");
    let mut b = b"SCRAM-SHA-256\0".to_vec();
    b.extend((first.len() as i32).to_be_bytes());
    b.extend(first.as_bytes());
    send(&mut c, b'p', &b);
    let (tag, body) = message(&mut c);
    assert_eq!((tag, &body[..4]), (b'R', &11i32.to_be_bytes()[..]), "{user}: {:?}", error_fields(&body));
    let server_first = String::from_utf8(body[4..].to_vec()).unwrap();
    let attr = |k: &str| server_first.split(',').find_map(|a| a.strip_prefix(k)).unwrap().to_string();
    let nonce = attr("r=");
    assert!(nonce.starts_with(client_nonce), "{server_first}");
    (seen.nonce, seen.salt, seen.iterations) =
        (nonce.len() - client_nonce.len(), attr("s="), attr("i=").parse().unwrap());
    // a proof of the right length (32 bytes) that proves nothing
    send(&mut c, b'p', format!("c=biws,r={nonce},p={}=", "A".repeat(43)).as_bytes());
    let (tag, body) = message(&mut c);
    assert_eq!(tag, b'E', "{user}");
    seen.end = error_fields(&body);
    seen
}

/// An agent proves it knows its token with SCRAM, against a verifier made when the token was
/// issued, so the token never crosses the wire. A wrong token, the server's token, or the right one
/// of a disabled agent are 28P01, the last with no SASLFinal. The verifier has the iterations
/// `scram_iterations` gave it. An agent with a role of its name logs in as the agent, with its
/// token, as before (the role lends it privileges, not a password).
#[test]
fn agents_log_in_with_scram() {
    let s = start(folder("agents"), &["--token", "s3cret-s3cret-s3cret-s3cret", "--unsafe"]);
    let p = s.pg;
    let mut admin = connect(p, "admin", Some("s3cret-s3cret-s3cret-s3cret")).unwrap();
    admin.batch_execute("create table t (id int primary key); insert into t values (1)").unwrap();
    let token: String = admin.query_one("create agent r3c_bot", &[]).unwrap().get("token");
    assert_eq!(asks(p, "r3c_bot"), scram());
    let mut bot = connect(p, "r3c_bot", Some(&token)).unwrap();
    assert_eq!(text(&mut bot, "select current_user::text"), "r3c_bot");
    let e = bot.batch_execute("insert into t values (2)").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "it acts as the agent: {e}");
    assert_eq!(by_hand(p, "r3c_bot", &token), ["R10", "R11", "R12", "R0"]);
    assert_eq!(refusal(connect(p, "r3c_bot", Some("chronos_wrong"))), failed("r3c_bot"));
    assert_eq!(
        refusal(connect(p, "r3c_bot", Some("s3cret-s3cret-s3cret-s3cret"))),
        failed("r3c_bot"),
        "the server's token isn't its"
    );
    admin.batch_execute("alter agent r3c_bot set (disabled = true)").unwrap();
    assert_eq!(by_hand(p, "r3c_bot", &token), ["R10", "R11", "E28P01"]);
    admin.batch_execute("alter agent r3c_bot set (disabled = false)").unwrap();
    connect(p, "r3c_bot", Some(&token)).unwrap();

    // scram_iterations, as for a role's password
    admin.batch_execute("set scram_iterations = 2000").unwrap();
    let token2: String = admin.query_one("create agent r3c_bot2", &[]).unwrap().get("token");
    assert_eq!(exchange(p, "r3c_bot2").iterations, 2000);
    assert_eq!(exchange(p, "r3c_bot").iterations, 4096);
    connect(p, "r3c_bot2", Some(&token2)).unwrap();
    if have_psql() {
        let url = format!("postgres://r3c_bot2@127.0.0.1:{p}/main");
        assert_eq!(psql(&url, &token2, "select current_user").unwrap(), "r3c_bot2");
        assert!(psql(&url, "chronos_wrong", "select 1").unwrap_err().contains("password authentication failed"));
    }

    // an agent with a role of its name: its token logs in, the role's password doesn't
    let token3: String = admin.query_one("create agent r3c_dual", &[]).unwrap().get("token");
    admin.batch_execute("create role r3c_dual login password 'rolepw'").unwrap();
    let mut dual = connect(p, "r3c_dual", Some(&token3)).unwrap();
    assert_eq!(text(&mut dual, "select current_user::text"), "r3c_dual");
    assert_eq!(refusal(connect(p, "r3c_dual", Some("rolepw"))), failed("r3c_dual"));

    // without a server token (loopback), an agent still needs its token, with SCRAM
    let s = start(folder("agents-loopback"), &[]);
    let token: String =
        connect(s.pg, "tester", None).unwrap().query_one("create agent r3c_lo", &[]).unwrap().get("token");
    assert_eq!(asks(s.pg, "r3c_lo"), scram());
    assert!(connect(s.pg, "r3c_lo", None).is_err());
    assert_eq!(refusal(connect(s.pg, "r3c_lo", Some("chronos_wrong"))), failed("r3c_lo"));
    connect(s.pg, "r3c_lo", Some(&token)).unwrap();
}

/// Over TLS an agent's SCRAM is bound to the certificate (SCRAM-SHA-256-PLUS): channel_binding=require
/// logs it in, and a client that saw another certificate is refused.
#[test]
fn agents_bind_to_the_certificate() {
    let s = tls_server("tls-agent", &["ec", "-pkeyopt", "ec_paramgen_curve:prime256v1"], "-sha256");
    let p = s.pg;
    let login = |user: &str, password: &str, lie: Option<Vec<u8>>| {
        let mut c = config(p, user, Some(password));
        c.ssl_mode(postgres::config::SslMode::Require);
        c.channel_binding(postgres::config::ChannelBinding::Require);
        c.connect(tls::Rustls::new(lie))
    };
    let mut admin = login("admin", "s3cret-s3cret-s3cret-s3cret", None).unwrap();
    let token: String = admin.query_one("create agent r3c_tls", &[]).unwrap().get("token");
    let mut bot = login("r3c_tls", &token, None).unwrap();
    assert_eq!(text(&mut bot, "select current_user::text"), "r3c_tls");
    assert_eq!(refusal(login("r3c_tls", "chronos_wrong", None)), failed("r3c_tls"));
    assert_eq!(
        refusal(login("r3c_tls", &token, Some(vec![7; 32]))),
        ("28000".into(), "SCRAM channel binding check failed".into())
    );
    if have_psql() {
        let url = format!("postgres://r3c_tls@127.0.0.1:{p}/main?sslmode=require&channel_binding=require");
        assert_eq!(psql(&url, &token, "select current_user").unwrap(), "r3c_tls");
    }
}

/// The old agent's database (tests/fixtures/old-agent, written by the binary before agents had
/// verifiers) copied into a fresh folder.
fn old_agent_folder(name: &str) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/old-agent");
    let dir = folder(name);
    for f in ["wal-0000000000000001", "history/0000000000000001-0", "pages/pack-00000000"] {
        let to = dir.join("db").join(f);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(from.join(f), to).unwrap();
    }
    dir
}

/// An HTTP API request's status line, with `token` as its bearer token.
fn http_status(port: u16, token: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let req = format!(
        "POST /v1/branches HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\
         Connection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.lines().next().unwrap_or_default().to_string()
}

/// An agent made before agents had SCRAM verifiers (its row has no verifier; its token is kept only
/// hashed). With a server token every name is asked for SCRAM, so it is too, which tells nobody it
/// is an agent but which it can't pass yet: its first use of its token over the HTTP API makes it a
/// verifier, and from then on, across restarts too, it logs in with SCRAM. A wrong token keeps
/// nothing. Without a server token (loopback) it sends its token in the clear once instead.
#[test]
fn agents_from_before_verifiers_get_one_at_their_first_login() {
    const TOKEN: &str = "chronos_c4c4f34372d6ef6d721d9bc4bd37657991229257da8aefa91c3a6e3803516734";
    let dir = old_agent_folder("old-agent");
    let mut s =
        start(dir.clone(), &["--token", "s3cret-s3cret-s3cret-s3cret", "--admin-token", "ADMIN-ADMIN-ADMIN-ADMIN-0"]);
    let p = s.pg;
    assert_eq!(asks(p, "r3c_oldbot"), scram(), "asked as any name is");
    assert_eq!(asks(p, "r3c_oldbot").1, asks(p, "r3c_nobody_at_all").1);
    assert_eq!(by_hand(p, "r3c_oldbot", TOKEN), ["R10", "R11", "E28P01"], "no verifier yet");
    for other in ["chronos_wrong", "s3cret-s3cret-s3cret-s3cret", "ADMIN-ADMIN-ADMIN-ADMIN-0"] {
        assert_eq!(refusal(connect(p, "r3c_oldbot", Some(other))), failed("r3c_oldbot"), "{other}");
    }
    assert!(http_status(s.http, "chronos_wrong").contains("401"));
    assert_eq!(by_hand(p, "r3c_oldbot", TOKEN), ["R10", "R11", "E28P01"], "a wrong token keeps nothing");
    // its token over HTTP: a verifier
    let status = http_status(s.http, TOKEN);
    assert!(!status.contains("401"), "{status}");
    let mut bot = connect(p, "r3c_oldbot", Some(TOKEN)).unwrap();
    assert_eq!(text(&mut bot, "select current_user::text"), "r3c_oldbot");
    let e = bot.batch_execute("create table t (id int)").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "its rights are as they were: {e}");
    assert_eq!(asks(p, "r3c_oldbot"), scram());
    assert_eq!(by_hand(p, "r3c_oldbot", TOKEN), ["R10", "R11", "R12", "R0"]);
    assert_eq!(refusal(connect(p, "r3c_oldbot", Some("chronos_wrong"))), failed("r3c_oldbot"));
    drop(bot);
    // after a restart
    let _ = s.child.kill();
    let _ = s.child.wait();
    let s2 = start(dir, &["--token", "s3cret-s3cret-s3cret-s3cret", "--admin-token", "ADMIN-ADMIN-ADMIN-ADMIN-0"]);
    assert_eq!(asks(s2.pg, "r3c_oldbot"), scram());
    assert_eq!(by_hand(s2.pg, "r3c_oldbot", TOKEN), ["R10", "R11", "R12", "R0"]);
    s.dir = PathBuf::new(); // s2 removes the folder

    // loopback without a server token: its token in the clear, once, then SCRAM
    let lo = start(old_agent_folder("old-agent-lo"), &[]);
    assert_eq!(asks(lo.pg, "r3c_oldbot"), (3, vec![]));
    assert!(connect(lo.pg, "r3c_oldbot", Some("chronos_wrong")).is_err());
    connect(lo.pg, "r3c_oldbot", Some(TOKEN)).unwrap();
    assert_eq!(asks(lo.pg, "r3c_oldbot"), scram());
}

/// With `--token`, the exchange a name gets before it proves anything doesn't tell what it is: a
/// role with a password, one without, one past its VALID UNTIL, an agent, a disabled one, the
/// database's own user and names nobody has all get SCRAM with Postgres's iteration count and a
/// 16-byte salt of their own, the same each time (a name with no verifier gets one made from a
/// server secret and the name, as Postgres's mock authentication makes it), and a proof that
/// proves nothing is 28P01 for each.
#[test]
fn names_are_not_told_apart() {
    let s = start(folder("names"), &["--token", "s3cret-s3cret-s3cret-s3cret", "--unsafe"]);
    let p = s.pg;
    let mut admin = connect(p, "admin", Some("s3cret-s3cret-s3cret-s3cret")).unwrap();
    admin
        .batch_execute(
            "create role r3c_pw login password 'pw'; create role r3c_nopw login;
             create role r3c_old login password 'pw' valid until '2001-01-01'; create role r3c_nologin password 'pw'",
        )
        .unwrap();
    admin.query_one("create agent r3c_agent", &[]).unwrap();
    admin.query_one("create agent r3c_off", &[]).unwrap();
    admin.batch_execute("alter agent r3c_off set (disabled = true)").unwrap();
    let names = [
        "r3c_pw",
        "r3c_nopw",
        "r3c_old",
        "r3c_nologin",
        "r3c_agent",
        "r3c_off",
        "r3c_nobody",
        "r3c_nobody2",
        "admin",
        "postgres",
        "pg_monitor",
    ];
    let mut salts = std::collections::HashMap::new();
    for n in names {
        let seen = exchange(p, n);
        assert_eq!((seen.asks, seen.mechanisms.as_slice()), (10, &["SCRAM-SHA-256".to_string()][..]), "{n}");
        assert_eq!((seen.iterations, seen.nonce), (4096, 24), "{n}");
        let b64 = |c: char| c.is_ascii_alphanumeric() || c == '+' || c == '/';
        assert!(
            seen.salt.len() == 24 && seen.salt.ends_with("==") && seen.salt[..22].chars().all(b64),
            "{n}: {seen:?}"
        );
        let refused = ("FATAL".into(), "28P01".into(), format!("password authentication failed for user \"{n}\""));
        assert_eq!(seen.end, refused, "{n}");
        if let Some(other) = salts.insert(seen.salt.clone(), n) {
            panic!("{n} has {other}'s salt");
        }
        assert_eq!(exchange(p, n).salt, seen.salt, "{n}: the same salt each time");
    }
    // the database's own user still logs in with the token under every such name
    connect(p, "r3c_nobody", Some("s3cret-s3cret-s3cret-s3cret")).unwrap();
    connect(p, "postgres", Some("s3cret-s3cret-s3cret-s3cret")).unwrap();
}

/// Wrong passwords from one address, whatever names they try: ten are refused as any wrong one is,
/// then the address waits before its next try (28P01 at once, saying how long, the right password
/// too), a wait that doubles with each failure after it; once it has waited, it logs in.
#[test]
fn wrong_passwords_back_off_per_address() {
    let dir = folder("backoff");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.arg("serve").arg(dir.join("db")).args(["--token", "s3cret-s3cret-s3cret-s3cret", "--unsafe"]);
    cmd.env("CHRONOS_LOGIN_FREE_TRIES", "10"); // (the default; the tests' servers get more)
    let s = common::serve(&mut cmd, true);
    let s = Server { child: s.child, dir, pg: s.pg, http: s.http };
    let token = Some("s3cret-s3cret-s3cret-s3cret");
    connect(s.pg, "admin", token).unwrap().batch_execute("create role r3c_bo login password 'pw'").unwrap();
    for i in 0..10 {
        let user = if i % 2 == 0 { "admin" } else { "r3c_bo" };
        assert_eq!(refusal(connect(s.pg, user, Some("wrong"))), failed(user));
    }
    connect(s.pg, "r3c_bo", Some("pw")).unwrap(); // (ten are free)
    assert_eq!(refusal(connect(s.pg, "r3c_bo", Some("wrong"))), failed("r3c_bo"));
    let (code, message) = refusal(connect(s.pg, "admin", token));
    assert_eq!(code, "28P01");
    assert!(message.contains("too many failed logins from this address"), "{message}");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    connect(s.pg, "admin", token).unwrap();
    assert_eq!(refusal(connect(s.pg, "r3c_bo", Some("wrong"))), failed("r3c_bo"));
    assert!(refusal(connect(s.pg, "r3c_bo", Some("pw"))).1.contains("too many failed logins"), "the wait doubled");
}

/// Passwords outside ASCII are kept as Postgres keeps them: after SASLprep (RFC 4013, as Postgres
/// runs it), or as typed where SASLprep prohibits them. The verifier made here with the reference
/// Postgres's salt is the one it made, and a role given each password logs in with libpq's SCRAM
/// (psql), and with tokio-postgres's where its SASLprep agrees with Postgres's.
#[test]
fn passwords_outside_ascii_are_prepared_as_postgres_prepares_them() {
    let long = "e\u{301}".repeat(400);
    // (the password, whether tokio-postgres's SASLprep makes of it what Postgres's does)
    let passwords: Vec<(&str, bool)> = vec![
        ("I\u{AD}X", true),           // mapped to nothing: IX
        ("\u{2168}", true),           // NFKC: IX
        ("a\u{A0}b", true),           // a space outside ASCII: "a b"
        ("\u{FF50}\u{FF41}ss", true), // NFKC: pass
        ("e\u{301}t\u{E9}", true),    // NFKC composes: été
        ("\u{627}1\u{628}", true),    // right to left, first and last: as it is
        ("\u{627}1", true),           // right to left but not last: prohibited, so as typed
        ("\u{FF21}\u{627}", true),    // left to right with right to left: as typed
        ("\u{7}x", true),             // ASCII with a control character: as typed
        ("e\u{301}\u{7}", true),      // a control character outside ASCII: as typed (not composed)
        ("\u{FF41}\u{E000}", true),   // private use: as typed
        ("\u{AD}", false),            // nothing left once mapped: as typed (tokio-postgres: empty)
        ("\u{2150}", false),          // unassigned in Unicode 3.2: as typed (tokio-postgres: 1/7)
        ("a\u{340}", false),          // a deprecated character, checked before NFKC: as typed
        (long.as_str(), true),        // long
    ];
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    let Ok(mut pg) = Client::connect(&format!("host=/tmp port={port} dbname=postgres"), NoTls) else {
        assert!(std::env::var_os("CHRONOS_REQUIRE_ORACLE").is_none(), "no Postgres at host=/tmp");
        eprintln!("skipped: no Postgres at host=/tmp");
        return;
    };
    let literal = |p: &str| format!("'{}'", p.replace('\'', "''"));
    for (pw, _) in &passwords {
        pg.batch_execute(&format!("drop role if exists r3c_prep; create role r3c_prep password {}", literal(pw)))
            .unwrap();
        let kept = text(&mut pg, "select rolpassword from pg_authid where rolname = 'r3c_prep'");
        pg.batch_execute("drop role r3c_prep").unwrap();
        let v = chronos::sql::scram::parse(&kept).unwrap();
        assert_eq!(chronos::sql::scram::verifier(pw, &v.salt, v.iterations), kept, "{pw:?}");
    }

    let s = start(folder("saslprep"), &["--token", "s3cret-s3cret-s3cret-s3cret", "--unsafe"]);
    let mut admin = connect(s.pg, "admin", Some("s3cret-s3cret-s3cret-s3cret")).unwrap();
    for (i, (pw, agrees)) in passwords.iter().enumerate() {
        let role = format!("r3c_prep{i}");
        admin.batch_execute(&format!("create role {role} login password {}", literal(pw))).unwrap();
        if *agrees {
            connect(s.pg, &role, Some(pw)).unwrap_or_else(|e| panic!("{pw:?}: {e}"));
        }
        if have_psql() {
            let url = format!("postgres://{role}@127.0.0.1:{}/main", s.pg);
            assert_eq!(psql(&url, pw, "select current_user").unwrap_or_else(|e| panic!("{pw:?}: {e}")), role);
        }
    }
    // the prepared form is the password too, where SASLprep changes it
    connect(s.pg, "r3c_prep0", Some("IX")).unwrap();
    connect(s.pg, "r3c_prep3", Some("pass")).unwrap();
    assert_eq!(refusal(connect(s.pg, "r3c_prep6", Some("\u{627}1 "))), failed("r3c_prep6"));
}

/// `chronos import`'s own client binds its SCRAM login to the server's certificate over TLS
/// (SCRAM-SHA-256-PLUS), as libpq's channel_binding=prefer does.
#[test]
fn import_binds_to_the_certificate() {
    let s = tls_server("import-tls", &["ec", "-pkeyopt", "ec_paramgen_curve:prime256v1"], "-sha384");
    let mut c = config(s.pg, "admin", Some("s3cret-s3cret-s3cret-s3cret"));
    c.ssl_mode(postgres::config::SslMode::Require);
    c.channel_binding(postgres::config::ChannelBinding::Disable); // (the test client hashes with SHA-256)
    c.connect(tls::Rustls::new(None))
        .unwrap()
        .batch_execute(
            "create table t (id int primary key, v text); insert into t values (1, 'a');
             create role r3c_import login superuser password 'pw'",
        )
        .unwrap();
    let import = |user: &str, password: &str| {
        let to = s.dir.join(format!("to-{user}-{password}"));
        let url = format!("postgres://{user}:{password}@127.0.0.1:{}/main?sslmode=require", s.pg);
        Command::new(env!("CARGO_BIN_EXE_chronos")).arg("import").arg(to).args([&url, "--dry-run"]).output().unwrap()
    };
    for (user, password) in [("r3c_import", "pw"), ("admin", "s3cret-s3cret-s3cret-s3cret")] {
        let out = import(user, password);
        let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && said.contains("would import 1 of 1 tables"), "{user}: {said}");
    }
    let out = import("r3c_import", "bad");
    assert!(String::from_utf8_lossy(&out.stderr).contains("password authentication failed for user \"r3c_import\""));
}

/// `scram_iterations` may be set as high as two billion, which makes hashing a password take many
/// minutes: as in Postgres, the hashing stops for statement_timeout, a cancel request and
/// pg_terminate_backend, for a role's password and an agent's token alike.
#[test]
fn hashing_with_many_iterations_stops_when_asked() {
    let s = start(folder("iterations"), &[]);
    let p = s.pg;
    let mut admin = connect(p, "tester", None).unwrap();
    admin.batch_execute("create role r3c_slow login password 'x'").unwrap();
    // (a statement that would hash for minutes, on its own thread: how it ended, and how long it took)
    let slow = |mut c: Client, sql: &'static str| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let began = std::time::Instant::now();
            let r = c.batch_execute(sql).map_err(|e| e.code().map(|c| c.code().to_string()).unwrap_or(e.to_string()));
            let _ = tx.send((r, began.elapsed()));
        });
        rx
    };
    let ended = |rx: std::sync::mpsc::Receiver<_>| -> (Result<(), String>, std::time::Duration) {
        rx.recv_timeout(std::time::Duration::from_secs(30)).expect("it stops")
    };
    let me = connect(p, "r3c_slow", Some("x")).unwrap();
    let (r, took) = ended(slow(
        me,
        "set statement_timeout = 500; set scram_iterations = 2147483647; alter role r3c_slow password 'y'",
    ));
    assert_eq!(r, Err("57014".into()));
    assert!(took < std::time::Duration::from_secs(10), "{took:?}");

    let me = connect(p, "r3c_slow", Some("x")).unwrap();
    let cancel = me.cancel_token();
    let rx = slow(me, "set scram_iterations = 2147483647; alter role r3c_slow password 'y'");
    std::thread::sleep(std::time::Duration::from_millis(300));
    cancel.cancel_query(NoTls).unwrap();
    assert_eq!(ended(rx).0, Err("57014".into()));

    let mut other = connect(p, "tester", None).unwrap();
    let pid: i32 = other.query_one("select pg_backend_pid()", &[]).unwrap().get(0);
    let rx = slow(other, "set scram_iterations = 2147483647; create agent r3c_slowbot");
    std::thread::sleep(std::time::Duration::from_millis(300));
    admin.query_one("select pg_terminate_backend($1)", &[&pid]).unwrap();
    assert!(ended(rx).0.is_err());
    // the password, and the agents, are as they were
    connect(p, "r3c_slow", Some("x")).unwrap();
    assert!(admin.query("show agents", &[]).unwrap().is_empty());
}
