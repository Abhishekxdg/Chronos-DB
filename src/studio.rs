//! `chronos studio <folder>`: a web UI for the database. The page (a React app in studio/, built to
//! studio/dist and embedded by build.rs) calls `POST /studio/api/<op>` with the ops of `api::call` plus `tables`,
//! `status` and `whoami`. If another process holds the folder, each call goes to it over the local socket
//! (see `local`, mode "api", and `serve_line`), so Studio runs beside `chronos serve`, `mcp` or the shell.
//!
//! Security, by default: on 127.0.0.1 only, and any local page or process can reach that, so the API
//! needs a random session key. The URL printed and opened carries it after `#` (never sent to the server,
//! so never in logs or a Referer); the page keeps it in sessionStorage and sends it as `X-Studio-Token`. A
//! request is refused (403) unless its Host is exactly 127.0.0.1:PORT (DNS rebinding) and its
//! Origin, if any, is exactly http://127.0.0.1:PORT (other pages); the static files need no
//! token. Every response is no-store, with a CSP allowing only this origin's files and no framing.
//! Studio acts as the database's owner, but it offers no `backup` (a path on this machine).
//!
//! With `--login`, there is no key: a person signs in with a database role and its password, as psql
//! would (`POST /studio/api/login`, see `sign_in`), and the Studio acts as that role (see `call_as_role`).
//! A session is a random id in memory, sent back as an HttpOnly, SameSite=Strict cookie (Secure over
//! HTTPS); it ends when signed out, after `IDLE` unused or `LONGEST` in all, and when its role is dropped
//! or may no longer log in. Wrong passwords wait as the Postgres port's do (see `backoff`). It may listen
//! elsewhere than loopback (`--listen`), with TLS unless told otherwise; the Host must be one of its names
//! (`--host`, and the address it listens on) and the Origin, if any, that same name.

use crate::{api, http, local};
use chronos::Db;
use chronos::sql::{Session, scram};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::net::TcpListener;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const MAX_BODY: usize = 8 << 20;
const HEADERS: &str = "Cache-Control: no-store\r\n\
    Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
    connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
    X-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\
    Cross-Origin-Resource-Policy: same-origin\r\nCross-Origin-Opener-Policy: same-origin\r\n";
const API: &str = "/studio/api/";
/// The page asks this before it has a key or a session: which sign-in the Studio takes, and who is signed in.
const WHOAMI: &str = "/studio/api/whoami";
const LOGIN: &str = "/studio/api/login";
const LOGOUT: &str = "/studio/api/logout";

// (path, content type, bytes) of studio/dist's files, or a page saying how to build them
include!(concat!(env!("OUT_DIR"), "/studio_assets.rs"));

/// Where calls run: on the database this process opened, or in the process holding the folder.
pub enum Backend {
    Here(Arc<Db>),
    Remote(String),
}

impl Backend {
    /// One request (see `serve_line`), run where the database is.
    fn call(&self, line: &Value) -> (u16, Value) {
        match self {
            Backend::Here(db) => serve_line(db, line),
            Backend::Remote(dir) => remote(dir, line),
        }
    }
}

/// How the Studio is reached, and who it acts as.
pub struct Options {
    /// 127.0.0.1's port (0: a free one), with the key in the link
    pub port: u16,
    /// open the page in the default browser
    pub open: bool,
    /// signing in with the database's roles instead (`--login`)
    pub login: Option<Login>,
}

/// `chronos studio --login`: the address to listen on, the names it is reached by, and its TLS.
pub struct Login {
    pub listen: String,
    pub hosts: Vec<String>,
    pub tls: Option<crate::tls::Tls>,
}

/// Serves the Studio until the process ends. `name` is the folder's name, shown in the page's title bar.
pub fn run(backend: Backend, name: &str, opts: Options) -> std::io::Result<()> {
    match opts.login {
        Some(login) => signed(backend, name, login, opts.open),
        None => keyed(backend, name, opts.port, opts.open),
    }
}

fn json_reply(status: u16, body: Value) -> http::Response {
    http::Response { status, content_type: "application/json", body: body.to_string().into(), headers: String::new() }
}

/// The page, or one of its files.
fn file(path: &str) -> http::Response {
    let p = if path == "/" { "index.html" } else { &path[1..] };
    match ASSETS.iter().find(|(f, _, _)| *f == p) {
        Some((_, content_type, body)) => {
            http::Response { status: 200, content_type, body: body.to_vec(), headers: String::new() }
        }
        None => json_reply(404, json!({ "error": "not found" })),
    }
}

/// Whether a request's body is declared JSON.
fn json_body(req: &http::Request) -> bool {
    req.header("Content-Type")
        .is_some_and(|t| t.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"))
}

/// An API call's arguments: its body, JSON (an empty one is `{}`).
fn args(req: &http::Request) -> Result<Value, http::Response> {
    match std::str::from_utf8(&req.body)
        .ok()
        .map(|b| if b.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str::<Value>(b) })
    {
        Some(Ok(a)) => Ok(a),
        _ => Err(json_reply(400, json!({ "error": "the body must be UTF-8 JSON" }))),
    }
}

/// The Studio with the key in the link, on 127.0.0.1:`port` (0: a free one), acting as the database's owner.
fn keyed(backend: Backend, name: &str, port: u16, open: bool) -> std::io::Result<()> {
    let l = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| std::io::Error::other(format!("listen on 127.0.0.1:{port}: {e}")))?;
    let port = l.local_addr()?.port();
    let token = new_token()?;
    let url = format!("http://127.0.0.1:{port}/#t={token}");
    eprintln!("chronos: studio on {url}");
    eprintln!("chronos: the link holds this session's key: don't share it. Ctrl-C stops the studio");
    if open {
        launch(&url);
    }
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let admit = |req: &http::Request| -> Result<(), http::Response> {
        let refuse = |msg: &str| Err(json_reply(403, json!({ "error": msg })));
        if req.header("Host") != Some(host.as_str()) {
            return refuse("the Host must be this studio's own address");
        }
        if req.header("Origin").is_some_and(|o| o != origin) {
            return refuse("requests from other web pages are refused");
        }
        if !req.path.starts_with(API) {
            return Ok(()); // the page itself
        }
        if req.path != WHOAMI && !req.header("X-Studio-Token").is_some_and(|t| crate::same_secret(t, &token)) {
            return refuse("missing or wrong X-Studio-Token: open the link chronos studio printed");
        }
        if !json_body(req) {
            return Err(json_reply(415, json!({ "error": "send the body with Content-Type: application/json" })));
        }
        Ok(())
    };
    let handle = |req: &http::Request, (): ()| -> http::Response {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", p) if !p.starts_with(API) => file(p),
            ("POST", p) if p.starts_with(API) => {
                let op = &p[API.len()..];
                let args = match args(req) {
                    Ok(a) => a,
                    Err(res) => return res,
                };
                // (asked without the key: it tells only that this Studio takes one)
                if op == "whoami" {
                    return json_reply(200, json!({ "mode": "key", "user": null }));
                }
                let (status, mut body) = backend.call(&json!({ "op": op, "args": args }));
                if op == "status" && status == 200 {
                    body["folder"] = json!(name);
                }
                json_reply(status, body)
            }
            (_, p) if p.starts_with(API) => json_reply(405, json!({ "error": "use POST" })),
            _ => json_reply(404, json!({ "error": "not found" })),
        }
    };
    http::serve(&l, None, MAX_BODY, HEADERS, &admit, &handle);
    Ok(())
}

/// The cookie a signed-in session is kept in.
const COOKIE: &str = "chronos_studio";
/// A session ends this long after its last call...
const IDLE: Duration = Duration::from_secs(30 * 60);
/// ...and this long after it signed in, however busy.
const LONGEST: Duration = Duration::from_secs(12 * 60 * 60);
/// Sessions kept at most: past it, the one idle longest ends.
const MOST_SESSIONS: usize = 10_000;
/// The one reply to a sign-in that didn't prove its password, whatever the reason: no such role, no
/// password, a wrong one, past its VALID UNTIL, an agent's name. It doesn't tell which.
const WRONG: &str = "wrong role name or password";
const SIGNED_OUT: &str =
    "signed out: the session ended, or its role was dropped or may no longer log in; sign in again";

/// A signed-in session, as a request names it.
struct Who {
    /// the SHA-256 of its id (what `Sessions` keeps)
    key: [u8; 32],
    role: String,
    oid: i64,
}

/// A signed-in session: its role (whose oid it had then: a role dropped and made again is another), and
/// when it signed in and was last used. Whether the role is a superuser isn't kept: the process holding
/// the database works it out on every call (see `call_as_role`).
struct Signed {
    role: String,
    oid: i64,
    created: Instant,
    used: Instant,
}

/// The live sessions, by the SHA-256 of their id: looking one up takes a time that says nothing of the id.
#[derive(Default)]
struct Sessions(Mutex<HashMap<[u8; 32], Signed>>);

fn sha256(id: &str) -> [u8; 32] {
    let mut k = [0u8; 32];
    k.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, id.as_bytes()).as_ref());
    k
}

impl Sessions {
    fn lock(&self) -> MutexGuard<'_, HashMap<[u8; 32], Signed>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The live session a request's cookies name, if any, used now.
    fn find(&self, cookies: Option<&str>) -> Option<Who> {
        let prefix = format!("{COOKIE}=");
        let id = cookies?.split(';').find_map(|c| c.trim().strip_prefix(prefix.as_str()))?;
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let key = sha256(id);
        let now = Instant::now();
        let mut m = self.lock();
        m.retain(|_, s| now.duration_since(s.used) < IDLE && now.duration_since(s.created) < LONGEST);
        let s = m.get_mut(&key)?;
        s.used = now;
        Some(Who { key, role: s.role.clone(), oid: s.oid })
    }

    fn start(&self, id: &str, role: &str, oid: i64) {
        let mut m = self.lock();
        if m.len() >= MOST_SESSIONS
            && let Some(k) = m.iter().min_by_key(|(_, s)| s.used).map(|(k, _)| *k)
        {
            m.remove(&k);
        }
        let now = Instant::now();
        m.insert(sha256(id), Signed { role: role.to_string(), oid, created: now, used: now });
    }

    fn end(&self, key: &[u8; 32]) {
        self.lock().remove(key);
    }
}

/// A Host (or an Origin's host and port) as compared: lower case, with its port (`port` when it gives none).
fn host_key(h: &str, port: u16) -> String {
    let h = h.trim().to_ascii_lowercase();
    let has_port = match h.rfind(']') {
        Some(i) => h[i..].contains(':'), // [::1]:8443
        None => h.contains(':'),
    };
    if has_port { h } else { format!("{h}:{port}") }
}

/// The Studio with `--login`: people sign in with a database role, and it acts as that role.
fn signed(backend: Backend, name: &str, login: Login, open: bool) -> std::io::Result<()> {
    let l = TcpListener::bind(&login.listen)
        .map_err(|e| std::io::Error::other(format!("listen on {}: {e}", login.listen)))?;
    let addr = l.local_addr()?; // the port the kernel picked, with port 0
    let tls = login.tls.is_some();
    let scheme = if tls { "https" } else { "http" };
    // the names it may be reached by: each --host (with the port listened on unless it gives one), and the
    // address listened on; a browser leaves the scheme's own port out of Host
    let mut hosts: Vec<String> = login.hosts.iter().map(|h| host_key(h, addr.port())).collect();
    hosts.push(host_key(&addr.to_string(), addr.port()));
    let default_port = if tls { 443 } else { 80 };
    let shown = login.hosts.first().map_or_else(|| addr.to_string(), |h| host_key(h, addr.port()));
    let url = format!("{scheme}://{shown}/");
    eprintln!("chronos: studio on {url} (sign in with a database role)");
    eprintln!(
        "chronos: a role with LOGIN and a password signs in, and the Studio acts with its privileges; make one in \
         psql or the shell (chronos <folder>): create role admin login superuser password '...'; Ctrl-C stops the studio"
    );
    if open {
        launch(&url);
    }
    let sessions = Sessions::default();
    let secure = if tls { "; Secure" } else { "" };
    let cookie = |id: &str| format!("Set-Cookie: {COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/{secure}\r\n");
    let gone = format!("Set-Cookie: {COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{secure}\r\n");
    let extra = format!("{HEADERS}{}", if tls { "Strict-Transport-Security: max-age=31536000\r\n" } else { "" });
    let admit = |req: &http::Request| -> Result<Option<Who>, http::Response> {
        let refuse = |msg: &str| Err(json_reply(403, json!({ "error": msg })));
        let Some(host) = req.header("Host").map(|h| host_key(h, default_port)).filter(|h| hosts.contains(h)) else {
            return refuse("the Host must be one of this studio's names: give others with chronos studio --host");
        };
        if let Some(o) = req.header("Origin") {
            let rest = o.get(..scheme.len() + 3).filter(|s| s.eq_ignore_ascii_case(&format!("{scheme}://")));
            if rest.is_none() || host_key(&o[scheme.len() + 3..], default_port) != host {
                return refuse("requests from other web pages are refused");
            }
        }
        if !req.path.starts_with(API) {
            return Ok(None); // the page itself
        }
        if !json_body(req) {
            return Err(json_reply(415, json!({ "error": "send the body with Content-Type: application/json" })));
        }
        let who = sessions.find(req.header("Cookie"));
        if who.is_none() && ![LOGIN, LOGOUT, WHOAMI].contains(&req.path.as_str()) {
            return Err(json_reply(401, json!({ "error": "sign in first: no session, or it ended" })));
        }
        Ok(who)
    };
    let handle = |req: &http::Request, who: Option<Who>| -> http::Response {
        let with = |mut res: http::Response, headers: String| {
            res.headers = headers;
            res
        };
        let (op, args) = match (req.method.as_str(), req.path.as_str()) {
            ("GET", p) if !p.starts_with(API) => return file(p),
            ("POST", p) if p.starts_with(API) => match args(req) {
                Ok(a) => (&p[API.len()..], a),
                Err(res) => return res,
            },
            (_, p) if p.starts_with(API) => return json_reply(405, json!({ "error": "use POST" })),
            _ => return json_reply(404, json!({ "error": "not found" })),
        };
        match (op, who) {
            ("login", who) => {
                // an address that failed too often waits before its next try, told at once (see `backoff`)
                if let Some(left) = crate::backoff::wait(req.peer) {
                    let secs = left.as_secs_f64().ceil() as u64;
                    let error = format!("too many failed sign-ins from this address: try again in {secs} s");
                    return json_reply(429, json!({ "error": error }));
                }
                let s = |k: &str| args.get(k).and_then(Value::as_str);
                let (Some(user), Some(password)) = (s("user"), s("password")) else {
                    return json_reply(400, json!({ "error": "send {\"user\": \"<role>\", \"password\": \"...\"}" }));
                };
                let (status, r) = backend.call(&json!({ "sign_in": { "user": user, "password": password } }));
                if status == 401 {
                    crate::backoff::failed(req.peer);
                }
                let (200, Some(role), Some(oid)) = (status, r["name"].as_str(), r["oid"].as_i64()) else {
                    return json_reply(status, r);
                };
                if let Some(w) = who {
                    sessions.end(&w.key); // signing in again ends the session the browser had
                }
                let Ok(id) = new_token() else {
                    return json_reply(500, json!({ "error": "no random numbers for a session" }));
                };
                sessions.start(&id, role, oid);
                let user = json!({ "name": role, "superuser": r["superuser"] == true });
                with(json_reply(200, json!({ "mode": "login", "user": user })), cookie(&id))
            }
            ("logout", who) => {
                if let Some(w) = who {
                    sessions.end(&w.key);
                }
                with(json_reply(200, json!({ "ok": true })), gone.clone())
            }
            ("whoami", None) => json_reply(200, json!({ "mode": "login", "user": null })),
            (op, None) => json_reply(401, json!({ "error": format!("{op}: sign in first") })),
            (op, Some(w)) => {
                let (status, mut body) =
                    backend.call(&json!({ "op": op, "args": args, "role": w.role, "role_oid": w.oid }));
                match (op, status) {
                    // the role was dropped, or may no longer log in: its session ends with it
                    (_, 401) => {
                        sessions.end(&w.key);
                        let r =
                            if op == "whoami" { (200, json!({ "mode": "login", "user": null })) } else { (401, body) };
                        with(json_reply(r.0, r.1), gone.clone())
                    }
                    ("whoami", 200) => json_reply(200, json!({ "mode": "login", "user": body })),
                    ("status", 200) => {
                        body["folder"] = json!(name);
                        json_reply(status, body)
                    }
                    _ => json_reply(status, body),
                }
            }
        }
    };
    http::serve(&l, login.tls.map(|t| t.cfg), MAX_BODY, &extra, &admit, &handle);
    Ok(())
}

/// The secret a name's mock salt is made with (see `sign_in`): this process's own.
static SECRET: LazyLock<[u8; 32]> = LazyLock::new(|| {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 32];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    b
});

/// One request from a Studio, run on the database: by the process that opened it, for its own Studio
/// or (over the local socket, see `local`) for a Studio beside it. `{"op", "args"}`: as the database's
/// owner (the key in the link); with `"role"` and `"role_oid"`: as that signed-in role (see
/// `call_as_role`); `{"sign_in": {"user", "password"}}`: a sign-in's check (see `sign_in`).
pub fn serve_line(db: &Arc<Db>, r: &Value) -> (u16, Value) {
    if let Some(s) = r.get("sign_in") {
        let s = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or_default();
        return sign_in(db, s("user"), s("password"));
    }
    let (op, args) = (r["op"].as_str().unwrap_or(""), &r["args"]);
    match r.get("role").and_then(Value::as_str) {
        Some(role) => call_as_role(db, op, args, role, r["role_oid"].as_i64()),
        None => call(db, op, args),
    }
}

/// Whether `password` is role `user`'s, and the role may sign in: (200, {name, oid, superuser}), or 401
/// with `WRONG` for any name, password or role it can't be (no such role, none kept, past its VALID
/// UNTIL, an agent's name or the guest's: all hashed as a wrong password is, against a mock salt as the
/// Postgres port's mock verifier, so the time taken says little of which names are roles). With the right
/// password, what Postgres checks after it (LOGIN, CONNECT on main, the CONNECTION LIMIT) is refused (403)
/// as Postgres words it.
fn sign_in(db: &Db, user: &str, password: &str) -> (u16, Value) {
    let agent = user == chronos::GUEST || db.agent(user).is_ok();
    let kept = match Session::role_password(db, user) {
        Ok(k) => k,
        Err(e) => return (500, json!({ "error": e.message })),
    };
    let right = match kept {
        Some((Some(p), false)) if !agent => scram::plain_matches(&p, password, user),
        _ => {
            std::hint::black_box(scram::salted_password(
                password,
                &scram::mock_salt(user, &SECRET[..]),
                scram::ITERATIONS,
            ));
            false
        }
    };
    if !right || password.is_empty() {
        return (401, json!({ "error": WRONG }));
    }
    let mut s = crate::Closing(Session::new(chronos::MAIN), db);
    s.0.user = Some(user.to_string());
    if let Err(e) = s.0.log_in(db, user).and_then(|()| s.0.may_connect(db)) {
        return (403, json!({ "error": { "message": e.message, "code": e.code } }));
    }
    match (s.0.login_role(), s.0.superuser(db)) {
        (Some(oid), Ok(superuser)) => (200, json!({ "name": user, "oid": oid, "superuser": superuser })),
        _ => (401, json!({ "error": WRONG })),
    }
}

/// A Studio call as signed-in role `role`, whose oid was `oid` when it signed in. The role is looked at
/// again first: dropped (or dropped and made again), renamed, NOLOGIN, past its VALID UNTIL or now an
/// agent's name, the session ends (401). `whoami` says who it is. `sql` runs as the role, with its
/// privileges, and `tables` counts rows as it. A superuser role may do all the owner does; any other
/// role only reads: `status`, the worlds (without their change counts: DIFF needs a superuser), the
/// tables, and its own SQL (which may write what its privileges allow).
fn call_as_role(db: &Arc<Db>, op: &str, a: &Value, role: &str, oid: Option<i64>) -> (u16, Value) {
    let superuser = {
        let mut s = crate::Closing(Session::new(chronos::MAIN), db);
        s.0.user = Some(role.to_string());
        match s.0.log_in(db, role).map(|()| s.0.login_role()) {
            Ok(Some(o)) if Some(o) == oid && role != chronos::GUEST && db.agent(role).is_err() => s.0.superuser(db),
            _ => return (401, json!({ "error": SIGNED_OUT })),
        }
    };
    let superuser = match superuser {
        Ok(s) => s,
        Err(e) => return (500, json!({ "error": e.message })),
    };
    let r = match op {
        "whoami" => Ok(json!({ "name": role, "superuser": superuser })),
        "sql" => api::run_sql_as_role(db, a, role),
        "tables" => tables(db, a.get("branch").and_then(Value::as_str).unwrap_or(chronos::MAIN), Some(role)),
        "status" => return call(db, op, a),
        "worlds" | "branches" if !superuser => api::call(db, op, a),
        _ if superuser => return call(db, op, a),
        _ => Err(api::Fail(403, format!("{op} needs a superuser role").into())),
    };
    reply(r)
}

fn reply(r: Result<Value, api::Fail>) -> (u16, Value) {
    match r {
        Ok(v) => (200, v),
        Err(api::Fail(status, error)) => (status, json!({ "error": error })),
    }
}

/// 256 random bits, as hex.
fn new_token() -> std::io::Result<String> {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 32];
    ring::rand::SystemRandom::new().fill(&mut b).map_err(|_| std::io::Error::other("no random numbers"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Opens `url` in the default browser. The token stays out of the command line (which other
/// users can see in the process list): the browser is given a file, readable only by this user,
/// that forwards to the URL.
fn launch(url: &str) {
    let file = local::private_file(&format!("studio-{}.html", std::process::id()));
    let page =
        format!("<!doctype html><meta http-equiv=\"refresh\" content=\"0;url={url}\"><title>Chronos Studio</title>");
    let Some(file) = file.filter(|f| std::fs::write(f, page).is_ok()) else {
        eprintln!("chronos: open the link above in a browser");
        return;
    };
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let started = std::process::Command::new(opener)
        .arg(&file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if started.is_err() {
        eprintln!("chronos: couldn't start {opener}: open the link above in a browser");
    }
    // the browser reads it within moments; it's only a pointer to a key that dies with this process
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(file);
    });
}

/// One request (see `serve_line`), run by the process holding the folder.
fn remote(dir: &str, request: &Value) -> (u16, Value) {
    let gone =
        || (503, json!({ "error": "the chronos process holding this database stopped: restart chronos studio" }));
    let Some((mut from, mut to)) = local::connect(dir, json!({ "mode": "api" })) else { return gone() };
    if writeln!(to, "{request}").is_err() {
        return gone();
    }
    let mut line = String::new();
    if from.read_line(&mut line).unwrap_or(0) == 0 {
        return gone();
    }
    let r: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    (r["status"].as_u64().unwrap_or(500) as u16, r["body"].clone())
}

/// One Studio call on `db`: `status`, `tables`, `sessions`, an import from Postgres (see `studio_import`), or an
/// `api` op. Returns the status and the reply (`{"error": ...}` unless 200).
fn call(db: &Arc<Db>, op: &str, a: &Value) -> (u16, Value) {
    if let Some(r) = crate::studio_import::call(db, op, a) {
        return reply(r);
    }
    let r = match op {
        "status" => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "encrypted": db.encrypted(),
            "remote": db.remote(),
            "counters": db.counters().into_iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<serde_json::Map<_, _>>(),
            // how far back time travel reads: history_retention (ms), and the earliest moment kept (0: the database's
            // start, while it is younger than that; null: none yet)
            "history": { "retention": db.retention(), "from": db.history_from() },
        })),
        "worlds" | "branches" => api::call(db, op, a).map(|v| with_changes(db, v)),
        "tables" => tables(db, a.get("branch").and_then(Value::as_str).unwrap_or(chronos::MAIN), None),
        "sessions" => Ok(sessions(db)),
        // a path on this machine, from a web page: not offered (`chronos <folder> backup <dir>` is)
        "backup" => Err(api::Fail(403, "backups aren't made from Studio: run chronos <folder> backup <dir>".into())),
        "merge" => api::call(db, op, a).map(trim_plan).map_err(|api::Fail(status, mut e)| {
            trim(&mut e, "conflicts");
            api::Fail(status, e)
        }),
        _ => api::call(db, op, a),
    };
    reply(r)
}

/// The Postgres port's connections, as pg_stat_activity shows them (times in ms since 1970), each with what it waits
/// for: the backends holding it up (pg_blocking_pids) and the table of the row it waits for. Cancel and terminate
/// go through SQL (pg_cancel_backend, pg_terminate_backend), with their checks.
fn sessions(db: &Db) -> Value {
    let ms = |us: i64| us / 1000;
    let rows: Vec<Value> = chronos::sql::activity(db)
        .into_iter()
        .map(|(pid, i, wait)| {
            json!({
                "pid": pid, "user": i.user, "application_name": i.app, "world": i.world, "state": i.state,
                "client_addr": i.addr.as_ref().map(|(a, _)| a), "client_port": i.addr.as_ref().map(|(_, p)| p),
                "backend_start": ms(i.started), "xact_start": i.xact_start.map(ms), "query_start": i.query_start.map(ms),
                "state_change": ms(i.state_change), "query": i.query,
                "wait_event_type": wait.as_ref().map(|_| "Lock"), "wait_event": wait.as_ref().map(|w| w.event),
                "blocking_pids": wait.as_ref().map_or(vec![], |w| w.blocking.clone()),
                "waiting_on": wait.and_then(|w| w.table),
            })
        })
        .collect();
    json!({ "sessions": rows, "now": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64) })
}

/// Worlds with how many rows each changed since its fork (the sidebar's badges, the Review button).
// ponytail: a diff count per world on every listing; skipped past 100 worlds
fn with_changes(db: &Db, mut v: Value) -> Value {
    if let Some(ws) = v.get_mut("branches").and_then(Value::as_array_mut).filter(|ws| ws.len() <= 100) {
        for w in ws.iter_mut().filter(|w| !w["parent"].is_null()) {
            let n = w["name"].as_str().and_then(|b| db.diff_count(b, None).ok()).map(|t| t.values().sum::<usize>());
            w["changes"] = json!(n);
        }
    }
    v
}

/// Rows shown of a merge's plan or conflicts: the browser never gets a whole big merge.
const SHOWN: usize = 200;

/// A dry run's plan cut to `SHOWN` rows, conflicts first, with how many rows had each outcome.
fn trim_plan(mut v: Value) -> Value {
    if let Some(rows) = v.get_mut("rows").and_then(Value::as_array_mut) {
        let mut outcomes = std::collections::BTreeMap::<String, u64>::new();
        for r in rows.iter() {
            *outcomes.entry(r["outcome"].as_str().unwrap_or("").to_string()).or_default() += 1;
        }
        rows.sort_by_key(|r| r["outcome"] != "conflict"); // stable: key order otherwise
        v["outcomes"] = json!(outcomes);
    }
    trim(&mut v, "rows");
    v
}

/// `v[list]` cut to `SHOWN` items, with `<list>_total` saying how many there were.
fn trim(v: &mut Value, list: &str) {
    if let Some(items) = v.get_mut(list).and_then(Value::as_array_mut) {
        let total = items.len();
        items.truncate(SHOWN);
        v[format!("{list}_total")] = json!(total);
    }
}

/// A world's tables (the database's own `_sonos` ones left out), with row counts, and whether each
/// has a SQL schema (`CREATE TABLE`) or was made by writing JSON rows; and its views and
/// materialized views, with `kind` saying which (a view's rows aren't counted: that runs its query). With
/// `role` (a signed-in Studio), rows are counted as that role: null where it may not read the table.
// ponytail: counts every row of every table on each call; keep counts if big databases feel slow
fn tables(db: &Db, branch: &str, role: Option<&str>) -> Result<Value, api::Fail> {
    if db.private_world(branch) {
        return Err(api::Fail(404, format!("no world {branch}").into()));
    }
    // tables with a schema (empty ones too), then every table with rows
    let (mut schemas, mut matviews) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
    for (k, v) in db.prefix(branch, "_sonos_tables/")? {
        let t = k["_sonos_tables/".len()..].to_string();
        // a materialized view keeps a table's schema, marked as one
        if serde_json::from_str::<Value>(&v).is_ok_and(|j| j.get("matview").is_some()) {
            matviews.insert(t.clone());
        }
        schemas.insert(t);
    }
    let views: Vec<String> =
        db.prefix(branch, "_sonos_views/")?.into_iter().map(|(k, _)| k["_sonos_views/".len()..].to_string()).collect();
    let mut names = schemas.clone();
    let mut from = String::new();
    while let Some((k, _)) = db.scan(branch, &from, 1)?.pop() {
        let t = k.split('/').next().unwrap_or(&k).to_string();
        from = format!("{t}0"); // '0' sorts right after '/': skips the rest of the table
        if !t.starts_with("_sonos") {
            names.insert(t);
        }
    }
    let mut as_role = match role {
        Some(r) => {
            let mut s = crate::Closing(Session::new(branch), db);
            s.0.user = Some(r.to_string());
            s.0.log_in(db, r).map_err(|e| api::Fail(401, e.message.into()))?;
            Some(s)
        }
        None => None,
    };
    let mut out = vec![];
    for t in names.clone() {
        let quoted: Vec<String> = t.split('.').map(|p| format!("\"{}\"", p.replace('"', "\"\""))).collect();
        let q = format!("select count(*) from {}", quoted.join("."));
        let counted = match &mut as_role {
            Some(s) => s.0.run(db, &q, &[]),
            None => chronos::sql::run(db, branch, &q, &[]),
        };
        let count = counted.ok().and_then(|o| o.first()?.rows.first()?.first().map(|v| v.to_json()));
        let mut one = json!({ "name": t, "rows": count, "schema": schemas.contains(&t) });
        if matviews.contains(&t) {
            one["kind"] = json!("materialized view");
        }
        out.push(one);
    }
    for v in views.into_iter().filter(|v| !names.contains(v)) {
        out.push(json!({ "name": v, "rows": null, "schema": true, "kind": "view" }));
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({ "tables": out }))
}
