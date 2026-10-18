//! `chronos init` and `chronos cloud ...` against a fake control plane (a TcpListener in the test answering
//! cloud/CONTRACTS.md's device flow and project calls with canned JSON): no real network.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const KEY: &str = "ck_test_1";
const DB_URL: &str = "postgres://app:s3cretpw@db_abcdefghijklmnop.pg.localtest.me/main?sslmode=require";

#[derive(Default)]
struct State {
    polls: u32,
    revoked: bool,
    /// (method, path, body) of every request
    seen: Vec<(String, String, String)>,
}

fn project(name: &str) -> Value {
    json!({
        "project": {"id": "prj_1", "name": name, "database": {"id": "db_abcdefghijklmnop", "state": "running", "region": "asia-south1"}},
        "database_url": DB_URL,
        "http_url": "https://db_abcdefghijklmnop.db.localtest.me",
        "mcp_url": "https://mcp.localtest.me"
    })
}

fn answer(state: &Mutex<State>, method: &str, path: &str, auth: Option<&str>, body: &str) -> (u16, Value) {
    let mut s = state.lock().unwrap();
    s.seen.push((method.into(), path.into(), body.into()));
    let req: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match (method, path) {
        ("POST", "/v1/auth/device") => (
            200,
            json!({"device_code": "dev-1", "user_code": "WDJB-MJHT", "verification_uri": "https://github.com/login/device", "interval": 0, "expires_in": 60}),
        ),
        ("POST", "/v1/auth/device/poll") => {
            assert_eq!(req["device_code"], "dev-1");
            s.polls += 1;
            match s.polls {
                1 => (428, json!({"error": "authorization_pending", "message": "not yet"})),
                _ => (200, json!({"api_key": KEY, "user": {"id": "u_1", "login": "ada"}})),
            }
        }
        _ if s.revoked || auth != Some(&format!("Bearer {KEY}")) => {
            (401, json!({"error": "unauthorized", "message": "no such key"}))
        }
        ("POST", "/v1/projects") => match req["name"].as_str() {
            Some("full") => (409, json!({"error": "plan_limit", "message": "the free plan has 2 databases"})),
            Some(name) => (201, project(name)),
            None => (400, json!({"error": "bad_request", "message": "name"})),
        },
        ("GET", "/v1/projects") => (200, json!({"projects": [project("myapp")["project"]]})),
        ("GET", "/v1/projects/prj_1") => (200, project("myapp")),
        ("GET", "/v1/projects/prj_1/usage") => (
            200,
            json!({"disk_bytes": 12_582_912, "disk_limit_bytes": 536_870_912, "worlds": 3, "egress_bytes_month": 1024,
                   "awake_seconds_month": 5400, "limits": {"max_worlds": 100, "egress_bytes_month": 5_368_709_120u64, "awake_seconds_month": 360000}}),
        ),
        ("POST", "/v1/projects/prj_1/production") => (200, json!({"production": req["production"]})),
        _ => (404, json!({"error": "not_found", "message": path})),
    }
}

/// One request per connection, as the client sends them (Connection: close).
fn serve(mut c: TcpStream, state: &Mutex<State>) {
    let mut raw = vec![];
    let mut buf = [0u8; 4096];
    let head_end = loop {
        let n = c.read(&mut buf).unwrap();
        assert!(n > 0, "the client closed mid-request");
        raw.extend_from_slice(&buf[..n]);
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8(raw[..head_end].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap().split(' ');
    let (method, path) = (first.next().unwrap().to_string(), first.next().unwrap().to_string());
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    let length: usize = header("content-length").map_or(0, |l| l.parse().unwrap());
    while raw.len() < head_end + 4 + length {
        let n = c.read(&mut buf).unwrap();
        raw.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8(raw[head_end + 4..head_end + 4 + length].to_vec()).unwrap();
    let (status, reply) = answer(state, &method, &path, header("authorization").as_deref(), &body);
    let reply = reply.to_string();
    let out = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    c.write_all(out.as_bytes()).unwrap();
}

fn fake_api() -> (String, Arc<Mutex<State>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let state = Arc::new(Mutex::new(State::default()));
    let s = state.clone();
    std::thread::spawn(move || {
        for c in l.incoming() {
            serve(c.unwrap(), &s);
        }
    });
    (base, state)
}

/// `chronos <args>` in `dir`, with `home` as HOME and `api` as CHRONOS_CLOUD_API.
fn chronos(dir: &Path, home: &Path, api: &str, args: &[&str]) -> (bool, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env("CHRONOS_CLOUD_API", api)
        .output()
        .unwrap();
    (o.status.success(), String::from_utf8(o.stdout).unwrap(), String::from_utf8(o.stderr).unwrap())
}

fn read(p: PathBuf) -> String {
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git").args(args).current_dir(dir).output().is_ok_and(|o| o.status.success())
}

#[test]
fn init_and_cloud_commands() {
    let root = std::env::temp_dir().join(format!("chronos-cloud-init-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, app) = (root.join("home"), root.join("myapp"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&app).unwrap();
    let has_git = git(&app, &["init", "-q"])
        && git(&app, &["remote", "add", "origin", "https://someone:ghp_secret@github.com/acme/myapp.git"]);
    // a project with its own MCP server, .env and CLAUDE.md, and an AGENTS.md
    std::fs::write(app.join(".mcp.json"), r#"{"mcpServers":{"github":{"command":"gh-mcp"}}}"#).unwrap();
    std::fs::write(app.join(".env"), "PORT=3000\nDATABASE_URL=postgres://old\n").unwrap();
    std::fs::write(app.join(".gitignore"), "node_modules/\n").unwrap();
    std::fs::write(app.join("CLAUDE.md"), "# myapp\n\nUse pnpm.\n").unwrap();
    std::fs::write(app.join("AGENTS.md"), "# Agents\n").unwrap();
    let (api, state) = fake_api();

    // first run: signs in (one pending poll, then the key), makes the project, writes the files
    let (ok, out, err) = chronos(&app, &home, &api, &["init", "--no-open"]);
    assert!(ok, "{out}\n{err}");
    assert!(err.contains("WDJB-MJHT") && err.contains("https://github.com/login/device"), "{err}");
    assert!(out.contains("postgres://app:****@db_abcdefghijklmnop.pg.localtest.me/main"), "{out}");
    assert!(!out.contains("s3cretpw") && !err.contains("s3cretpw"), "the password is printed:\n{out}\n{err}");
    assert!(out.contains("Open Claude Code here and ask it to build."), "{out}");
    assert_eq!(state.lock().unwrap().polls, 2);
    let create = state.lock().unwrap().seen.iter().find(|(m, p, _)| m == "POST" && p == "/v1/projects").cloned();
    let create: Value = serde_json::from_str(&create.expect("POST /v1/projects").2).unwrap();
    assert_eq!(create["name"], "myapp");
    if has_git {
        assert_eq!(create["git_remote"], "github.com/acme/myapp"); // no credential sent
    }

    let config = home.join(".config/chronos/cloud.toml");
    let text = read(config.clone());
    assert!(text.contains(&format!("api_key = \"{KEY}\"")) && text.contains(&format!("api = \"{api}\"")), "{text}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&config).unwrap().permissions().mode() & 0o777, 0o600);
    }

    let pj: Value = serde_json::from_str(&read(app.join(".chronos/project.json"))).unwrap();
    assert_eq!(pj, json!({"project_id": "prj_1", "name": "myapp"}));
    let mcp: Value = serde_json::from_str(&read(app.join(".mcp.json"))).unwrap();
    assert_eq!(
        mcp,
        json!({"mcpServers": {"github": {"command": "gh-mcp"},
                              "chronos": {"type": "http", "url": "https://mcp.localtest.me/?project=prj_1"}}})
    );
    assert_eq!(read(app.join(".env")), format!("PORT=3000\nDATABASE_URL={DB_URL}\n"));
    assert_eq!(read(app.join(".gitignore")), "node_modules/\n.env\n");
    for (f, start) in [
        ("CLAUDE.md", "# myapp\n\nUse pnpm.\n\n<!-- chronos:begin -->"),
        ("AGENTS.md", "# Agents\n\n<!-- chronos:begin -->"),
    ] {
        let t = read(app.join(f));
        assert!(t.starts_with(start) && t.ends_with("<!-- chronos:end -->\n"), "{f}:\n{t}");
        assert!(t.contains("Fork a world") && t.contains("Merge"), "{f}:\n{t}");
    }

    // again: no sign-in, the project from .chronos/project.json, the files as they were
    let files = [".chronos/project.json", ".mcp.json", ".env", ".gitignore", "CLAUDE.md", "AGENTS.md"];
    let before: Vec<String> = files.iter().map(|f| read(app.join(f))).collect();
    let (ok, out, err) = chronos(&app, &home, &api, &["init", "--no-open"]);
    assert!(ok, "{out}\n{err}");
    assert!(out.contains("already up to date"), "{out}");
    assert_eq!(state.lock().unwrap().polls, 2);
    let last = state.lock().unwrap().seen.last().cloned().unwrap();
    assert_eq!((last.0.as_str(), last.1.as_str()), ("GET", "/v1/projects/prj_1"));
    let after: Vec<String> = files.iter().map(|f| read(app.join(f))).collect();
    assert_eq!(before, after);

    // a second clone (the committed project.json, no .env yet) reaches the same database
    let clone = root.join("clone");
    std::fs::create_dir_all(clone.join(".chronos")).unwrap();
    std::fs::copy(app.join(".chronos/project.json"), clone.join(".chronos/project.json")).unwrap();
    let (ok, out, err) = chronos(&clone, &home, &api, &["init"]);
    assert!(ok, "{out}\n{err}");
    assert_eq!(read(clone.join(".env")), format!("DATABASE_URL={DB_URL}\n"));
    assert!(!clone.join("AGENTS.md").exists()); // only where there is one

    // the other commands, from the project's folder
    let (ok, out, err) = chronos(&app, &home, &api, &["cloud", "list"]);
    assert!(ok && out.contains("prj_1") && out.contains("db_abcdefghijklmnop"), "{out}\n{err}");
    let (ok, out, err) = chronos(&app, &home, &api, &["cloud", "usage"]);
    assert!(ok && out.contains("12.0 MB of 512.0 MB") && out.contains("3 of 100"), "{out}\n{err}");
    let (ok, out, err) = chronos(&app, &home, &api, &["cloud", "production", "on"]);
    assert!(ok && out.contains("production"), "{out}\n{err}");
    let last = state.lock().unwrap().seen.last().cloned().unwrap();
    assert_eq!((last.1.as_str(), last.2.as_str()), ("/v1/projects/prj_1/production", r#"{"production":true}"#));
    let (ok, _, err) = chronos(&root, &home, &api, &["cloud", "usage"]);
    assert!(!ok && err.contains("chronos init"), "{err}");
    let (ok, _, err) = chronos(&app, &home, &api, &["cloud", "frobnicate"]);
    assert!(!ok && err.contains("usage: chronos init"), "{err}");

    // a revoked key: told to sign in again
    state.lock().unwrap().revoked = true;
    for args in [&["cloud", "list"][..], &["init"][..]] {
        let (ok, _, err) = chronos(&app, &home, &api, args);
        assert!(!ok && err.contains("run `chronos cloud login`"), "{args:?}: {err}");
    }
    state.lock().unwrap().revoked = false;

    // past the plan
    let full = root.join("full");
    std::fs::create_dir_all(&full).unwrap();
    let (ok, _, err) = chronos(&full, &home, &api, &["init"]);
    assert!(!ok && err.contains("limit is reached") && err.contains("the free plan has 2 databases"), "{err}");
    assert!(!full.join(".env").exists() && !full.join("CLAUDE.md").exists(), "files written for a refused project");

    // the API unreachable
    let (ok, _, err) = chronos(&app, &home, "http://127.0.0.1:1", &["cloud", "list"]);
    assert!(!ok && err.contains("can't reach the Chronos Cloud API at http://127.0.0.1:1"), "{err}");

    // signed out: the key is gone, the rest of the file kept
    let (ok, out, err) = chronos(&app, &home, &api, &["cloud", "logout"]);
    assert!(ok, "{out}\n{err}");
    let text = read(config.clone());
    assert!(!text.contains("api_key") && text.contains("api = "), "{text}");
    let (ok, _, err) = chronos(&app, &home, &api, &["cloud", "list"]);
    assert!(!ok && err.contains("chronos cloud login"), "{err}");

    // and in again (the fake's next poll answers at once)
    let (ok, out, err) = chronos(&app, &home, &api, &["cloud", "login", "--no-open"]);
    assert!(ok && err.contains("signed in as ada"), "{out}\n{err}");
    assert!(read(config).contains(KEY));

    std::fs::remove_dir_all(&root).unwrap();
}
