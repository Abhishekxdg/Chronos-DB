//! Chronos Cloud from the command line (the API is in cloud/CONTRACTS.md; the guide is docs/cloud.md):
//!
//!   chronos init                     in a project folder: sign in if needed, find or make the project's
//!                                    database, and write .chronos/project.json, .mcp.json, .env and the
//!                                    CLAUDE.md (and AGENTS.md) block. Safe to run again
//!   chronos cloud login | logout     sign in with GitHub (device flow), or forget the API key
//!   chronos cloud list               your org's projects and their databases
//!   chronos cloud usage              this folder's project: disk, worlds, egress, awake time, against the plan
//!   chronos cloud production on|off  this folder's project in production: agents' worlds wait for a person to
//!                                    merge them (its agents lose write_main and merge_own); off gives them back
//!
//! The API key is kept in ~/.config/chronos/cloud.toml (mode 0600) with the API's address it came from;
//! CHRONOS_CLOUD_API overrides that address. Files in the project are merged into, never replaced: other MCP
//! servers, other .env lines and the rest of CLAUDE.md stay as they are.
//!
//! HTTP is a small HTTP/1.1 client (one request per connection) over rustls with the system's CA
//! certificates, the same TLS stack `chronos import` uses, so the binary takes no new dependency.

use chronos::pgclient;
use serde_json::{Map, Value, json};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_API: &str = "https://api.chronos.cloud";
const BEGIN: &str = "<!-- chronos:begin -->";
const END: &str = "<!-- chronos:end -->";
/// The longest answer read from the API: its answers are small JSON objects.
const MAX_REPLY: usize = 16 << 20;

pub(crate) fn help() -> &'static str {
    "usage: chronos init [--no-open]        in a project folder: sign in if needed, find or make its Chronos Cloud database,
                                      and write .chronos/project.json, .mcp.json, .env and the CLAUDE.md block
       chronos cloud login [--no-open]   sign in with GitHub (prints a code to enter in the browser)
       chronos cloud logout              forget the API key kept in ~/.config/chronos/cloud.toml
       chronos cloud list                your org's projects and their databases
       chronos cloud usage               this folder's project: disk, worlds, egress, awake time, against the plan
       chronos cloud production on|off   agents' merges into main of this folder's project wait for a person (off: they merge)
CHRONOS_CLOUD_API overrides the API's address (https://api.chronos.cloud)."
}

/// `chronos init ...` or `chronos cloud ...` (`args` from the command's name on); the process's exit code.
pub(crate) fn run(args: &[String], no_open: bool) -> i32 {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let done = match words.as_slice() {
        ["init"] => init(no_open),
        ["cloud", "login"] => Config::load().and_then(|mut c| login(&mut c, no_open)).map(|_| ()),
        ["cloud", "logout"] => logout(),
        ["cloud", "list"] => list(),
        ["cloud", "usage"] => usage(),
        ["cloud", "production", on @ ("on" | "off")] => production(*on == "on"),
        _ => {
            eprintln!("{}", help());
            return 2;
        }
    };
    match done {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("chronos: {e}");
            1
        }
    }
}

// ---- the config file -------------------------------------------------------------------------

/// ~/.config/chronos/cloud.toml: `api_key = "ck_..."` and `api = "https://..."`, other lines kept as they are.
struct Config {
    path: PathBuf,
    text: String,
}

impl Config {
    fn load() -> Result<Config, String> {
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(x) => PathBuf::from(x),
            None => match std::env::var_os("HOME").filter(|v| !v.is_empty()) {
                Some(h) => PathBuf::from(h).join(".config"),
                None => return Err("HOME is not set: can't find ~/.config/chronos/cloud.toml".into()),
            },
        };
        let path = base.join("chronos").join("cloud.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Ok(Config { path, text })
    }

    fn get(&self, key: &str) -> Option<String> {
        self.text.lines().find_map(|l| toml_line(l).filter(|(k, _)| k == key).map(|(_, v)| v)).filter(|v| !v.is_empty())
    }

    /// The API's base: CHRONOS_CLOUD_API, else the one the key came from, else api.chronos.cloud.
    fn api(&self) -> String {
        let env = std::env::var("CHRONOS_CLOUD_API").ok().filter(|v| !v.is_empty());
        env.or_else(|| self.get("api")).unwrap_or_else(|| DEFAULT_API.into()).trim_end_matches('/').to_string()
    }

    /// Writes the file with `key` set to `value` (None: removed), only its owner able to read it.
    fn set(&mut self, changes: &[(&str, Option<&str>)]) -> Result<(), String> {
        for (key, value) in changes {
            self.text = toml_set(&self.text, key, *value);
        }
        let dir = self.path.parent().expect("the config file is in a folder");
        let fail = |e: io::Error| format!("{}: {e}", self.path.display());
        std::fs::create_dir_all(dir).map_err(fail)?;
        write_private(&self.path, &self.text).map_err(fail)
    }
}

/// `key = "value"` (or a bare value) as its key and unescaped value; None for anything else.
fn toml_line(line: &str) -> Option<(String, String)> {
    let (k, v) = line.split_once('=')?;
    let (k, v) = (k.trim(), v.trim());
    if k.is_empty() || k.starts_with('#') {
        return None;
    }
    let v = match v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => {
            let mut out = String::new();
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                out.push(if c == '\\' { chars.next()? } else { c });
            }
            out
        }
        None => v.to_string(),
    };
    Some((k.to_string(), v))
}

/// `text` with `key`'s line replaced by `key = "value"` (appended if it has none; removed for None).
fn toml_set(text: &str, key: &str, value: Option<&str>) -> String {
    let line = value.map(|v| format!("{key} = \"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")));
    let mut out = vec![];
    let mut placed = false;
    for l in text.lines() {
        if toml_line(l).is_some_and(|(k, _)| k == key) {
            if let (Some(line), false) = (&line, placed) {
                out.push(line.clone());
            }
            placed = true;
        } else {
            out.push(l.to_string());
        }
    }
    if let (Some(line), false) = (line, placed) {
        out.push(line);
    }
    let mut s = out.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

/// Writes `path` readable by its owner only (0600), replacing it through a rename so it is never half written.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

// ---- HTTP ------------------------------------------------------------------------------------

/// `scheme://host[:port]/path` as (TLS?, host as written, host for SNI, port, path).
fn split_url(url: &str) -> Result<(bool, String, String, u16, String), String> {
    let bad = || format!("{url}: expected http(s)://host[:port][/path]");
    let (tls, rest) = match url.split_once("://") {
        Some(("https", r)) => (true, r),
        Some(("http", r)) => (false, r),
        _ => return Err(bad()),
    };
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (host, port) = if let Some(after) = authority.strip_prefix('[') {
        let (h, tail) = after.split_once(']').ok_or_else(bad)?;
        (format!("[{h}]"), tail.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (authority.to_string(), None),
        }
    };
    let port = match port {
        Some(p) => p.parse().map_err(|_| bad())?,
        None if tls => 443,
        None => 80,
    };
    if host.is_empty() || host.contains('@') {
        return Err(bad());
    }
    let sni = host.trim_start_matches('[').trim_end_matches(']').to_string();
    Ok((tls, host, sni, port, path.to_string()))
}

/// Sends one request and returns the status and body.
fn send(method: &str, url: &str, key: Option<&str>, body: Option<&Value>) -> io::Result<(u16, Vec<u8>)> {
    let (tls, host, sni, port, path) = split_url(url).map_err(io::Error::other)?;
    let addr = format!("{host}:{port}");
    let tcp = pgclient::connect_tcp(&addr, 10)?;
    tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))?;
    let default_port = if tls { 443 } else { 80 };
    let host_header = if port == default_port { host.clone() } else { addr.clone() };
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: chronos/{}\r\nAccept: application/json\r\nConnection: close\r\n",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(k) = key {
        req.push_str(&format!("Authorization: Bearer {k}\r\n"));
    }
    let body = body.map(Value::to_string);
    if let Some(b) = &body {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    req.push_str(body.as_deref().unwrap_or(""));
    let raw = if tls {
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let roots = pgclient::roots(std::env::var("SSL_CERT_FILE").ok().as_deref()).map_err(|e| {
            io::Error::other(format!("TLS: no CA certificates ({e}); SSL_CERT_FILE=<bundle.pem> names them"))
        })?;
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| io::Error::other(format!("TLS: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(sni).map_err(|e| io::Error::other(format!("TLS: {e}")))?;
        let conn = rustls::ClientConnection::new(std::sync::Arc::new(cfg), name)
            .map_err(|e| io::Error::other(format!("TLS: {e}")))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        s.write_all(req.as_bytes())?;
        s.flush()?;
        read_all(&mut s)?
    } else {
        let mut tcp = tcp;
        tcp.write_all(req.as_bytes())?;
        read_all(&mut tcp)?
    };
    parse_reply(&raw)
}

/// Everything the server sends until it closes. A TLS server that closes without close_notify has
/// still sent a whole answer if it sent any (parse_reply checks its length).
fn read_all(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut out = vec![];
    let mut buf = [0u8; 16 << 10];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !out.is_empty() => return Ok(out),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
        if out.len() > MAX_REPLY {
            return Err(io::Error::other("the answer is too long"));
        }
    }
}

/// An HTTP/1.1 answer's status and body (Content-Length or chunked, or up to the close).
fn parse_reply(raw: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    let bad = |why: &str| io::Error::other(format!("a broken HTTP answer: {why}"));
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| bad("no end of headers"))?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut lines = head.split("\r\n");
    let status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse::<u16>().ok());
    let status = status.ok_or_else(|| bad("no status"))?;
    let (mut length, mut chunked) = (None, false);
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = v.trim().parse::<usize>().ok(),
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    let rest = &raw[end + 4..];
    let body = if chunked {
        let mut body = vec![];
        let mut at = 0;
        loop {
            let nl = rest[at..].windows(2).position(|w| w == b"\r\n").ok_or_else(|| bad("a cut chunk"))?;
            let size = String::from_utf8_lossy(&rest[at..at + nl]);
            let size = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| bad("a chunk size"))?;
            at += nl + 2;
            if size == 0 {
                break body;
            }
            let chunk = rest.get(at..at + size).ok_or_else(|| bad("a cut chunk"))?;
            body.extend_from_slice(chunk);
            at += size + 2;
        }
    } else if let Some(n) = length {
        rest.get(..n).ok_or_else(|| bad("shorter than its Content-Length"))?.to_vec()
    } else {
        rest.to_vec()
    };
    Ok((status, body))
}

/// The control plane's API at `base`, with an API key or without (signing in).
struct Api {
    base: String,
    key: Option<String>,
}

impl Api {
    /// The answer's status and JSON body (Null if empty); Err only when the API couldn't be reached.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), String> {
        let url = format!("{}{path}", self.base);
        let (status, raw) = send(method, &url, self.key.as_deref(), body).map_err(|e| {
            format!(
                "can't reach the Chronos Cloud API at {}: {e} (check the network; CHRONOS_CLOUD_API sets another address)",
                self.base
            )
        })?;
        let body = match raw.is_empty() {
            true => Value::Null,
            false => {
                serde_json::from_slice(&raw).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&raw).into()))
            }
        };
        Ok((status, body))
    }

    /// `call`, a status other than 2xx made an error.
    fn ok(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let (status, body) = self.call(method, path, body)?;
        match status {
            200..=299 => Ok(body),
            _ => Err(api_error(status, &body)),
        }
    }
}

/// An error answer (`{"error","message"}`) as what to tell the user.
fn api_error(status: u16, body: &Value) -> String {
    let code = body.get("error").and_then(Value::as_str);
    let message = body.get("message").and_then(Value::as_str).or(body.as_str()).filter(|m| !m.trim().is_empty());
    match (status, code) {
        (401, _) => "not signed in to Chronos Cloud, or the API key was revoked: run `chronos cloud login`".into(),
        (409, Some("plan_limit")) => format!(
            "your plan's limit is reached{}: delete a project you no longer need (chronos cloud list shows them), or upgrade the plan",
            message.map(|m| format!(" ({m})")).unwrap_or_default()
        ),
        _ => {
            let mut s = format!("the Chronos Cloud API answered {status}");
            if let Some(c) = code {
                s.push_str(&format!(" {c}"));
            }
            if let Some(m) = message {
                s.push_str(&format!(": {}", m.chars().take(500).collect::<String>()));
            }
            s
        }
    }
}

/// The API, signed in: the saved key (`chronos cloud login` saves it).
fn signed_in(config: &Config) -> Result<Api, String> {
    match config.get("api_key") {
        Some(key) => Ok(Api { base: config.api(), key: Some(key) }),
        None => Err("not signed in to Chronos Cloud: run `chronos cloud login` (or `chronos init`)".into()),
    }
}

// ---- signing in --------------------------------------------------------------------------------

/// GitHub's device flow through the API: print the code, open the browser, poll until it's entered.
/// Saves the API key and the API's address it came from; returns the key.
fn login(config: &mut Config, no_open: bool) -> Result<String, String> {
    let api = Api { base: config.api(), key: None };
    let start = api.ok("POST", "/v1/auth/device", Some(&json!({})))?;
    let field = |k: &str| {
        start
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("the API's sign-in answer has no {k}"))
    };
    let (device_code, user_code, uri) = (field("device_code")?, field("user_code")?, field("verification_uri")?);
    let mut interval = start.get("interval").and_then(Value::as_u64).unwrap_or(5);
    let expires = start.get("expires_in").and_then(Value::as_u64).unwrap_or(900);
    eprintln!("chronos: to sign in to Chronos Cloud, open {uri} and enter the code {user_code}");
    if !no_open {
        open_browser(&uri);
    }
    let until = Instant::now() + Duration::from_secs(expires);
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        let (status, body) = api.call("POST", "/v1/auth/device/poll", Some(&json!({ "device_code": device_code })))?;
        match status {
            200 => {
                let key = body.get("api_key").and_then(Value::as_str).unwrap_or("");
                if key.is_empty() || key.chars().any(|c| c.is_control() || c.is_whitespace()) {
                    return Err("the API's sign-in answer has no usable api_key".into());
                }
                let who = body.pointer("/user/login").and_then(Value::as_str).unwrap_or("?");
                let base = api.base.clone();
                config.set(&[("api_key", Some(key)), ("api", Some(&base))])?;
                eprintln!("chronos: signed in as {who} (key saved in {})", config.path.display());
                return Ok(key.to_string());
            }
            428 => {}
            429 => interval += 5, // slow_down
            _ => return Err(api_error(status, &body)),
        }
        if Instant::now() >= until {
            return Err("the sign-in code expired before it was entered: run the command again".into());
        }
    }
}

/// Opens `url` in the browser (macOS `open`, Linux `xdg-open`); elsewhere the printed URL is the way.
fn open_browser(url: &str) {
    // only a web address: the API's answer must not name a file or an app for `open` to start
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        return;
    };
    let null = std::process::Stdio::null;
    let _ = std::process::Command::new(opener).arg(url).stdin(null()).stdout(null()).stderr(null()).spawn();
}

fn logout() -> Result<(), String> {
    let mut config = Config::load()?;
    if config.get("api_key").is_none() {
        println!("chronos: not signed in");
        return Ok(());
    }
    config.set(&[("api_key", None)])?;
    println!("chronos: signed out (the key is removed from {})", config.path.display());
    Ok(())
}

// ---- chronos init ------------------------------------------------------------------------------

/// What `POST /v1/projects` (and `GET /v1/projects/{id}`) answers.
struct Project {
    id: String,
    name: String,
    database: String,
    state: String,
    region: String,
    database_url: String,
    mcp_url: String,
}

impl Project {
    fn from(v: &Value) -> Result<Project, String> {
        let s = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
        let p = Project {
            id: s("/project/id"),
            name: s("/project/name"),
            database: s("/project/database/id"),
            state: s("/project/database/state"),
            region: s("/project/database/region"),
            database_url: s("/database_url"),
            mcp_url: s("/mcp_url"),
        };
        for (what, value) in [("project.id", &p.id), ("database_url", &p.database_url), ("mcp_url", &p.mcp_url)] {
            if value.is_empty() {
                return Err(format!("the API's project answer has no {what}"));
            }
        }
        if !id_ok(&p.id) {
            return Err(format!("the API answered a project id that isn't one: {:?}", p.id));
        }
        Ok(p)
    }
}

/// A project id is letters, digits, `_` and `-` (it goes into URLs as it is).
fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The folder's `.chronos/project.json` (`{"project_id","name"}`), if it has one.
fn linked(dir: &Path) -> Result<Option<(String, Option<String>)>, String> {
    let path = dir.join(".chronos").join("project.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let v: Value = serde_json::from_str(&text).map_err(|e| format!(".chronos/project.json: {e}"))?;
    match v.get("project_id").and_then(Value::as_str) {
        Some(id) if id_ok(id) => Ok(Some((id.into(), v.get("name").and_then(Value::as_str).map(Into::into)))),
        _ => Err(".chronos/project.json has no valid project_id: fix it, or remove it to link the folder again".into()),
    }
}

/// The folder's project id, from `.chronos/project.json`, for the commands that act on it.
fn this_project() -> Result<String, String> {
    let dir = std::env::current_dir().map_err(|e| e.to_string())?;
    match linked(&dir)? {
        Some((id, _)) => Ok(id),
        None => Err("this folder has no .chronos/project.json: run `chronos init` here first".into()),
    }
}

/// `git remote get-url origin` in `dir` as `host/owner/repo` (no scheme, user, password or `.git`).
fn git_remote(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(dir)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    normalize_remote(String::from_utf8_lossy(&out.stdout).trim())
}

/// `https://user:token@github.com/x/y.git`, `git@github.com:x/y.git`, `ssh://git@github.com/x/y` all as
/// `github.com/x/y`, so the same repository is matched however it was cloned (and no credential is sent).
fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest.to_string(),
        // scp-like: [user@]host:path
        None => match url.split_once(':') {
            Some((host, path)) if !host.contains('/') => format!("{host}/{}", path.trim_start_matches('/')),
            _ => url.to_string(),
        },
    };
    let rest = match rest.find('/') {
        Some(slash) => match rest[..slash].rfind('@') {
            Some(at) => rest[at + 1..].to_string(),
            None => rest,
        },
        None => rest,
    };
    let (host, path) = rest.split_once('/')?;
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase(); // an ssh port isn't the repository
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}"))
}

/// `postgres://app:secret@host/main` as `postgres://app:****@host/main`.
fn mask(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let Some(at) = rest[..end].rfind('@') else { return url.to_string() };
    match rest[..at].split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:****{}", &rest[at..]),
        None => url.to_string(),
    }
}

/// `.chronos/project.json` with the project's id and name set; other keys kept.
fn merge_project_json(existing: Option<&str>, id: &str, name: &str) -> Result<String, String> {
    let mut obj = match existing {
        Some(t) if !t.trim().is_empty() => match serde_json::from_str::<Value>(t) {
            Ok(Value::Object(m)) => m,
            _ => return Err(".chronos/project.json isn't a JSON object: fix or remove it".into()),
        },
        _ => Map::new(),
    };
    obj.insert("project_id".into(), json!(id));
    obj.insert("name".into(), json!(name));
    Ok(pretty(&Value::Object(obj)))
}

/// `.mcp.json` with `mcpServers.chronos` set to the project's MCP endpoint; other servers and keys kept.
fn merge_mcp_json(existing: Option<&str>, url: &str) -> Result<String, String> {
    let mut obj = match existing {
        Some(t) if !t.trim().is_empty() => match serde_json::from_str::<Value>(t) {
            Ok(Value::Object(m)) => m,
            _ => return Err(".mcp.json isn't a JSON object: fix it, then run chronos init again".into()),
        },
        _ => Map::new(),
    };
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        return Err(".mcp.json's mcpServers isn't an object: fix it, then run chronos init again".into());
    };
    servers.insert("chronos".into(), json!({ "type": "http", "url": url }));
    Ok(pretty(&Value::Object(obj)))
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("JSON values serialize");
    s.push('\n');
    s
}

/// The project's MCP URL: the one endpoint, told the project in the query (cloud/CONTRACTS.md, mcp.<domain>).
fn mcp_project_url(mcp_url: &str, project: &str) -> String {
    format!("{}/?project={project}", mcp_url.trim_end_matches('/'))
}

/// `.env` with its `DATABASE_URL=` line (or `export DATABASE_URL=`) set to `url`, else one appended.
fn merge_env(existing: Option<&str>, url: &str) -> String {
    let value = match url.contains(|c: char| c.is_whitespace() || c == '#' || c == '"' || c == '\'') {
        true => format!("\"{}\"", url.replace('\\', "\\\\").replace('"', "\\\"")),
        false => url.to_string(),
    };
    let text = existing.unwrap_or("");
    let mut out = vec![];
    let mut placed = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let export = trimmed.starts_with("export ");
        let bare = trimmed.strip_prefix("export ").unwrap_or(trimmed).trim_start();
        if bare.strip_prefix("DATABASE_URL").is_some_and(|r| r.trim_start().starts_with('=')) {
            if !placed {
                out.push(format!("{}DATABASE_URL={value}", if export { "export " } else { "" }));
            }
            placed = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !placed {
        out.push(format!("DATABASE_URL={value}"));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// `.gitignore` with `.env` in it, or None when it already ignores `.env`.
fn merge_gitignore(existing: Option<&str>) -> Option<String> {
    let text = existing.unwrap_or("");
    if text.lines().map(str::trim).any(|l| matches!(l, ".env" | "/.env" | ".env*" | "/.env*" | "*.env")) {
        return None;
    }
    let mut s = text.to_string();
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(".env\n");
    Some(s)
}

/// The instructions for agents, between the markers.
fn agent_block() -> String {
    format!(
        "{BEGIN}
## Database: Chronos Cloud

This project's database is on Chronos Cloud: the `chronos` MCP server in `.mcp.json`, the project in
`.chronos/project.json`. `DATABASE_URL` in `.env` points at `main`, the live data: don't change it directly.

To change the schema or the data:
1. Fork a world (an instant copy of `main`) and make the change there.
2. Run the app's tests against the world: `DATABASE_URL` with the world's name in place of `/main`.
3. Show the world's diff (what changed, row by row).
4. Merge the world into `main` (or drop it to throw the change away).
{END}"
    )
}

/// `text` with the block between the markers replaced (or appended if it has none).
fn merge_block(existing: Option<&str>, block: &str) -> Result<String, String> {
    let text = existing.unwrap_or("");
    if let Some(b) = text.find(BEGIN) {
        let Some(e) = text[b..].find(END) else {
            return Err(format!("it has {BEGIN} without {END} after it: fix it by hand, then run chronos init again"));
        };
        let e = b + e + END.len();
        return Ok(format!("{}{block}{}", &text[..b], &text[e..]));
    }
    let mut s = text.to_string();
    if !s.is_empty() {
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s.push('\n');
    }
    s.push_str(block);
    s.push('\n');
    Ok(s)
}

fn read_opt(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Writes `text` to `path` if it differs; true if it was written. A new file is owner-only if `private`.
fn write_changed(path: &Path, old: Option<&str>, text: &str, private: bool) -> Result<bool, String> {
    if old == Some(text) {
        return Ok(false);
    }
    let fail = |e: io::Error| format!("{}: {e}", path.display());
    match (old, private) {
        (None, true) => write_private(path, text).map_err(fail)?,
        _ => std::fs::write(path, text).map_err(fail)?,
    }
    Ok(true)
}

/// Every file `chronos init` writes in `dir` for `p`; the names of those it changed.
fn write_files(dir: &Path, p: &Project) -> Result<Vec<String>, String> {
    let mut changed = vec![];
    let mut put = |name: &str, old: Option<String>, text: String, private: bool| -> Result<(), String> {
        if write_changed(&dir.join(name), old.as_deref(), &text, private)? {
            changed.push(name.to_string());
        }
        Ok(())
    };
    let chronos_dir = dir.join(".chronos");
    std::fs::create_dir_all(&chronos_dir).map_err(|e| format!("{}: {e}", chronos_dir.display()))?;
    let old = read_opt(&dir.join(".chronos/project.json"))?;
    put(".chronos/project.json", old.clone(), merge_project_json(old.as_deref(), &p.id, &p.name)?, false)?;
    let old = read_opt(&dir.join(".mcp.json"))?;
    put(".mcp.json", old.clone(), merge_mcp_json(old.as_deref(), &mcp_project_url(&p.mcp_url, &p.id))?, false)?;
    let old = read_opt(&dir.join(".env"))?;
    put(".env", old.clone(), merge_env(old.as_deref(), &p.database_url), true)?;
    let old = read_opt(&dir.join(".gitignore"))?;
    if let Some(text) = merge_gitignore(old.as_deref()) {
        put(".gitignore", old, text, false)?;
    }
    let block = agent_block();
    for (name, always) in [("CLAUDE.md", true), ("AGENTS.md", false)] {
        let old = read_opt(&dir.join(name))?;
        if old.is_none() && !always {
            continue;
        }
        let text = merge_block(old.as_deref(), &block).map_err(|e| format!("{name}: {e}"))?;
        put(name, old, text, false)?;
    }
    Ok(changed)
}

fn init(no_open: bool) -> Result<(), String> {
    let dir = std::env::current_dir().map_err(|e| format!("the current folder: {e}"))?;
    let mut config = Config::load()?;
    let key = match config.get("api_key") {
        Some(k) => k,
        None => login(&mut config, no_open)?,
    };
    let api = Api { base: config.api(), key: Some(key) };
    let folder = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
    let create =
        |name: &str| api.ok("POST", "/v1/projects", Some(&json!({ "name": name, "git_remote": git_remote(&dir) })));
    let answer = match linked(&dir)? {
        None => create(&folder)?,
        // a clone of a linked project: the same database
        Some((id, name)) => match api.call("GET", &format!("/v1/projects/{id}"), None)? {
            (200, body) if body.get("database_url").is_some() => body,
            // an answer without the connection details: POST /v1/projects finds the project and gives them
            (200, _) => create(name.as_deref().unwrap_or(&folder))?,
            (404, _) => {
                return Err(format!(
                    ".chronos/project.json names project {id}, which isn't one of your org's (deleted, or another \
                     org's): sign in as a member of its org, or remove .chronos/project.json to make a new project"
                ));
            }
            (status, body) => return Err(api_error(status, &body)),
        },
    };
    let p = Project::from(&answer)?;
    if let Some((id, _)) = linked(&dir)?
        && id != p.id
    {
        return Err(format!(
            ".chronos/project.json names project {id}, but the API answered {}: remove .chronos/project.json to link \
             this folder to {} instead",
            p.id, p.id
        ));
    }
    let changed = write_files(&dir, &p)?;
    let mut db = p.database.clone();
    let about: Vec<&str> = [p.state.as_str(), p.region.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
    if !about.is_empty() {
        db.push_str(&format!(" ({})", about.join(", ")));
    }
    println!("chronos: project {} ({}), database {db}", p.name, p.id);
    println!("chronos: DATABASE_URL={} (in .env)", mask(&p.database_url));
    println!("chronos: MCP {}", mcp_project_url(&p.mcp_url, &p.id));
    match changed.is_empty() {
        true => println!("chronos: files already up to date"),
        false => println!("chronos: wrote {}", changed.join(", ")),
    }
    println!("Open Claude Code here and ask it to build.");
    Ok(())
}

// ---- the other commands ------------------------------------------------------------------------

fn list() -> Result<(), String> {
    let api = signed_in(&Config::load()?)?;
    let answer = api.ok("GET", "/v1/projects", None)?;
    let projects = answer.get("projects").unwrap_or(&answer).as_array().cloned().unwrap_or_default();
    if projects.is_empty() {
        println!("no projects yet: run `chronos init` in a project folder");
        return Ok(());
    }
    for p in &projects {
        // each as POST /v1/projects's `project`, or that whole answer
        let p = p.get("project").unwrap_or(p);
        let s = |k: &str| p.pointer(k).and_then(Value::as_str).unwrap_or("-").to_string();
        let production = if p.get("production").and_then(Value::as_bool) == Some(true) { "  production" } else { "" };
        println!(
            "{}  {}  {} ({}, {}){production}",
            s("/id"),
            s("/name"),
            s("/database/id"),
            s("/database/state"),
            s("/database/region")
        );
    }
    Ok(())
}

/// Bytes as `12.3 MB`.
fn bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < units.len() {
        v /= 1024.0;
        u += 1;
    }
    match u {
        0 => format!("{n} B"),
        _ => format!("{v:.1} {}", units[u]),
    }
}

fn usage() -> Result<(), String> {
    let id = this_project()?;
    let api = signed_in(&Config::load()?)?;
    let u = api.ok("GET", &format!("/v1/projects/{id}/usage"), None)?;
    let n = |p: &str| u.pointer(p).and_then(Value::as_u64);
    let limit = |p: &str, f: &dyn Fn(u64) -> String| match u.pointer(p) {
        Some(Value::Null) => " (no limit)".to_string(),
        Some(v) => v.as_u64().map(|l| format!(" of {}", f(l))).unwrap_or_default(),
        None => String::new(),
    };
    let hours = |s: u64| format!("{:.1} h", s as f64 / 3600.0);
    let count = |c: u64| c.to_string();
    let disk_limit = n("/disk_limit_bytes").or(n("/limits/disk_bytes")).map(|l| format!(" of {}", bytes(l)));
    println!("project  {id}");
    println!("disk     {}{}", n("/disk_bytes").map(bytes).unwrap_or("-".into()), disk_limit.unwrap_or_default());
    println!("worlds   {}{}", n("/worlds").map(count).unwrap_or("-".into()), limit("/limits/max_worlds", &count));
    println!(
        "egress   {}{} this month",
        n("/egress_bytes_month").map(bytes).unwrap_or("-".into()),
        limit("/limits/egress_bytes_month", &bytes)
    );
    println!(
        "awake    {}{} this month",
        n("/awake_seconds_month").map(hours).unwrap_or("-".into()),
        limit("/limits/awake_seconds_month", &hours)
    );
    Ok(())
}

fn production(on: bool) -> Result<(), String> {
    let id = this_project()?;
    let api = signed_in(&Config::load()?)?;
    api.ok("POST", &format!("/v1/projects/{id}/production"), Some(&json!({ "production": on })))?;
    match on {
        true => println!("chronos: {id} is in production: agents' worlds wait for a person to merge them into main"),
        false => println!("chronos: {id} is no longer in production: agents merge their worlds into main themselves"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_json_keeps_other_servers() {
        let old = r#"{"mcpServers":{"github":{"command":"gh-mcp","args":["--x"]},"chronos":{"type":"http","url":"old"}},"other":1}"#;
        let new = merge_mcp_json(Some(old), "https://mcp.chronos.cloud/?project=prj_1").unwrap();
        let v: Value = serde_json::from_str(&new).unwrap();
        assert_eq!(v["mcpServers"]["github"], json!({"command":"gh-mcp","args":["--x"]}));
        assert_eq!(v["mcpServers"]["chronos"], json!({"type":"http","url":"https://mcp.chronos.cloud/?project=prj_1"}));
        assert_eq!(v["other"], json!(1));
        // again: the same text
        assert_eq!(merge_mcp_json(Some(&new), "https://mcp.chronos.cloud/?project=prj_1").unwrap(), new);
        // none yet, an empty file, and one that isn't JSON (left alone)
        let fresh = merge_mcp_json(None, "u").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&fresh).unwrap(),
            json!({"mcpServers":{"chronos":{"type":"http","url":"u"}}})
        );
        assert_eq!(merge_mcp_json(Some("  \n"), "u").unwrap(), fresh);
        assert!(merge_mcp_json(Some("{not json"), "u").is_err());
        assert!(merge_mcp_json(Some(r#"{"mcpServers":[1]}"#), "u").is_err());
    }

    #[test]
    fn mcp_url_names_the_project() {
        assert_eq!(mcp_project_url("https://mcp.chronos.cloud", "prj_a"), "https://mcp.chronos.cloud/?project=prj_a");
        assert_eq!(mcp_project_url("https://mcp.chronos.cloud/", "prj_a"), "https://mcp.chronos.cloud/?project=prj_a");
    }

    #[test]
    fn env_line_replaced_or_appended() {
        let url = "postgres://app:pw@db_x.pg.chronos.cloud/main?sslmode=require";
        let old = "# keys\nSTRIPE_KEY=sk_test\nDATABASE_URL=postgres://old\nPORT=3000";
        let new = merge_env(Some(old), url);
        assert_eq!(new, format!("# keys\nSTRIPE_KEY=sk_test\nDATABASE_URL={url}\nPORT=3000\n"));
        assert_eq!(merge_env(Some(&new), url), new);
        // export kept, a second one dropped, spaces around = understood
        let new = merge_env(Some("export DATABASE_URL = x\nDATABASE_URL=y\nA=1\n"), url);
        assert_eq!(new, format!("export DATABASE_URL={url}\nA=1\n"));
        // appended; a name that only starts the same isn't it
        assert_eq!(merge_env(Some("DATABASE_URL_RO=z"), url), format!("DATABASE_URL_RO=z\nDATABASE_URL={url}\n"));
        assert_eq!(merge_env(None, url), format!("DATABASE_URL={url}\n"));
        // a value .env would split is quoted
        assert_eq!(merge_env(None, "postgres://a:b#c@h/main"), "DATABASE_URL=\"postgres://a:b#c@h/main\"\n");
    }

    #[test]
    fn gitignore_gets_env_once() {
        assert_eq!(merge_gitignore(None).as_deref(), Some(".env\n"));
        assert_eq!(merge_gitignore(Some("target")).as_deref(), Some("target\n.env\n"));
        assert_eq!(merge_gitignore(Some("node_modules/\n")).as_deref(), Some("node_modules/\n.env\n"));
        for has in ["a\n.env\n", "/.env", ".env*\n", "  .env  \n"] {
            assert_eq!(merge_gitignore(Some(has)), None, "{has:?}");
        }
        // .env.local isn't .env
        assert!(merge_gitignore(Some(".env.local\n")).is_some());
    }

    #[test]
    fn claude_md_block() {
        let block = agent_block();
        assert!(block.starts_with(BEGIN) && block.ends_with(END));
        let new = merge_block(None, &block).unwrap();
        assert_eq!(new, format!("{block}\n"));
        let mine = "# My app\n\nUse pnpm.";
        let new = merge_block(Some(mine), &block).unwrap();
        assert_eq!(new, format!("# My app\n\nUse pnpm.\n\n{block}\n"));
        assert_eq!(merge_block(Some(&new), &block).unwrap(), new);
        // an old block in the middle is replaced in place, the rest kept
        let old = format!("# My app\n{BEGIN}\nold words\n{END}\n\n## Tests\nnpm test\n");
        let new = merge_block(Some(&old), &block).unwrap();
        assert_eq!(new, format!("# My app\n{block}\n\n## Tests\nnpm test\n"));
        assert!(!new.contains("old words"));
        assert_eq!(merge_block(Some(&new), &block).unwrap(), new);
        // a begin without an end: left alone
        assert!(merge_block(Some(&format!("x\n{BEGIN}\nhalf")), &block).is_err());
    }

    #[test]
    fn project_json_keeps_other_keys() {
        let new = merge_project_json(Some(r#"{"project_id":"prj_old","note":"keep"}"#), "prj_1", "myapp").unwrap();
        let v: Value = serde_json::from_str(&new).unwrap();
        assert_eq!(v, json!({"project_id":"prj_1","name":"myapp","note":"keep"}));
        assert!(merge_project_json(Some("[]"), "prj_1", "x").is_err());
    }

    #[test]
    fn remotes_normalized() {
        for (url, want) in [
            ("https://github.com/x/y.git", Some("github.com/x/y")),
            ("https://user:ghp_secret@github.com/x/y", Some("github.com/x/y")),
            ("git@github.com:x/y.git", Some("github.com/x/y")),
            ("ssh://git@github.com:22/x/y.git/", Some("github.com/x/y")),
            ("GitHub.com/x/y", Some("github.com/x/y")),
            ("", None),
            ("github.com", None),
        ] {
            assert_eq!(normalize_remote(url).as_deref(), want, "{url}");
        }
    }

    #[test]
    fn folder_without_a_remote() {
        let dir = std::env::temp_dir().join(format!("chronos-cloud-noremote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // not a git repository (or git missing): no remote, not an error
        assert_eq!(git_remote(&dir), None);
        if std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).status().is_ok_and(|s| s.success())
        {
            assert_eq!(git_remote(&dir), None); // a repository without origin
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn password_masked() {
        assert_eq!(
            mask("postgres://app:s3cr3t@db_a.pg.chronos.cloud/main?sslmode=require"),
            "postgres://app:****@db_a.pg.chronos.cloud/main?sslmode=require"
        );
        assert_eq!(mask("postgres://app@h/main"), "postgres://app@h/main");
        assert_eq!(mask("postgres://h/main?x=a:b@c"), "postgres://h/main?x=a:b@c");
    }

    #[test]
    fn config_lines() {
        let t = toml_set("# mine\napi = \"https://x\"\n", "api_key", Some("ck_\"q\\"));
        assert_eq!(t, "# mine\napi = \"https://x\"\napi_key = \"ck_\\\"q\\\\\"\n");
        let line = t.lines().nth(2).unwrap();
        assert_eq!(toml_line(line), Some(("api_key".into(), "ck_\"q\\".into())));
        let t = toml_set(&t, "api_key", Some("ck_2"));
        assert_eq!(t, "# mine\napi = \"https://x\"\napi_key = \"ck_2\"\n");
        assert_eq!(toml_set(&t, "api_key", None), "# mine\napi = \"https://x\"\n");
    }

    #[test]
    fn replies_parsed() {
        let r = b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}extra";
        assert_eq!(parse_reply(r).unwrap(), (201, b"{}".to_vec()));
        let r = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4;x=y\r\n\":1}\r\n0\r\n\r\n";
        assert_eq!(parse_reply(r).unwrap(), (200, b"{\"a\":1}".to_vec()));
        assert!(parse_reply(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{}").is_err());
        assert_eq!(
            split_url("http://127.0.0.1:8080").unwrap(),
            (false, "127.0.0.1".into(), "127.0.0.1".into(), 8080, "/".into())
        );
        assert_eq!(split_url("https://[::1]/v").unwrap(), (true, "[::1]".into(), "::1".into(), 443, "/v".into()));
        assert!(split_url("ftp://x").is_err());
    }

    #[test]
    fn errors_say_what_to_do() {
        assert!(api_error(401, &json!({"error":"unauthorized"})).contains("chronos cloud login"));
        let e = api_error(409, &json!({"error":"plan_limit","message":"2 databases on free"}));
        assert!(e.contains("limit") && e.contains("2 databases on free"), "{e}");
        assert_eq!(
            api_error(500, &json!({"error":"internal","message":"boom"})),
            "the Chronos Cloud API answered 500 internal: boom"
        );
    }
}
