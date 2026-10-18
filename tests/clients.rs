//! `chronos serve` end to end: the TypeScript and Python clients run their test scripts against a
//! real server (skipped if node or python3 isn't installed), plus the server's own safety rules.

mod common;

use std::process::{Child, Command};

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(name: &str, token: Option<&str>) -> (Server, String) {
    let dir = std::env::temp_dir().join(format!("chronos-serve-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    if let Some(t) = token {
        cmd.args(["--token", t]);
    }
    let s = common::serve(&mut cmd, false);
    (Server(s.child, dir), format!("http://127.0.0.1:{}", s.http))
}

fn run_client(program: &str, script: &str, dir: &str, url: &str, token: &str) {
    if Command::new(program).arg("--version").output().is_err() {
        eprintln!("skipped: {program} not installed");
        return;
    }
    let root = env!("CARGO_MANIFEST_DIR");
    let out = Command::new(program)
        .arg(script)
        .current_dir(format!("{root}/clients/{dir}"))
        .env("CHRONOS_URL", url)
        .env("CHRONOS_TOKEN", token)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program} {script} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn typescript_client() {
    let (_server, url) = start("ts", Some("t0k3n-t0k3n-t0k3n-t0k3n-t0k3n"));
    run_client("node", "test.ts", "typescript", &url, "t0k3n-t0k3n-t0k3n-t0k3n-t0k3n");
}

#[test]
fn python_client() {
    let (_server, url) = start("py", None);
    run_client("python3", "test_chronos.py", "python", &url, "");
}

#[test]
fn agent_examples_run() {
    let (_server, url) = start("agents", None);
    run_client("node", "../../examples/agents/parallel.ts", "typescript", &url, "");
    let (_server, url) = start("tools", None);
    run_client("python3", "../../examples/agents/tools.py", "python", &url, "");
}

/// The five-minute trial (examples/trial): it checks its own outcomes, so it fails if Chronos
/// stops catching one of the four mistakes it shows.
#[test]
fn trial_runs() {
    let (_server, url) = start("trial", None);
    run_client("python3", "../../examples/trial/trial.py", "python", &url, "");
}

#[test]
fn token_is_enforced_and_open_networks_need_one() {
    let (_server, url) = start("auth", Some("secret-secret-secret-secret"));
    let post = |auth: Option<&str>| {
        let mut c = Command::new("curl");
        c.args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "POST", &format!("{url}/v1/branches")]);
        if let Some(a) = auth {
            c.args(["-H", &format!("Authorization: Bearer {a}")]);
        }
        String::from_utf8(c.output().unwrap().stdout).unwrap()
    };
    assert_eq!(post(None), "401");
    assert_eq!(post(Some("wrong")), "401");
    assert_eq!(post(Some("secret-secret-secret-secret")), "200");

    let dir = std::env::temp_dir().join(format!("chronos-serve-{}-open", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["serve", dir.to_str().unwrap(), "--listen", "0.0.0.0:0", "--pg", "off"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("without a token"));
    let _ = std::fs::remove_dir_all(dir);
}

/// A server token (or admin token) shorter than 24 characters, or the old placeholder `change-me`,
/// stops the server from starting; an address that isn't loopback needs TLS too, unless
/// `--insecure-no-tls`, which starts with a loud warning.
#[test]
fn guessable_tokens_and_remote_listens_without_tls_are_refused() {
    use std::io::BufRead;
    let dir = std::env::temp_dir().join(format!("chronos-serve-{}-weak", std::process::id()));
    let cmd = |listen: &str, pg: &str, args: &[&str], env: Option<&str>| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"));
        c.args(["serve", dir.to_str().unwrap(), "--listen", listen, "--pg", pg]).args(args);
        c.env_remove("CHRONOS_TOKEN");
        if let Some(t) = env {
            c.env("CHRONOS_TOKEN", t);
        }
        c
    };
    let refused = |listen: &str, pg: &str, args: &[&str], env: Option<&str>| {
        let out = cmd(listen, pg, args, env).output().unwrap();
        assert!(!out.status.success(), "{listen} {pg} {args:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let local = "127.0.0.1:0";
    assert!(refused(local, "off", &["--token", "s3cret"], None).contains("shorter than 24 characters"));
    assert!(refused(local, "off", &[], Some("change-me")).contains("placeholder `change-me`"));
    let long = "0123456789abcdef01234567";
    assert!(refused(local, "off", &["--admin-token", "ADMIN"], Some(long)).contains("admin token"));
    let e = refused("0.0.0.0:0", "off", &[], Some(long));
    assert!(e.contains("without TLS") && e.contains("--insecure-no-tls"), "{e}");
    let e = refused(local, "0.0.0.0:0", &[], Some(long));
    assert!(e.contains("without TLS"), "{e}");
    // with --insecure-no-tls it starts, and says loudly what that means
    let mut c = cmd("0.0.0.0:0", "off", &["--insecure-no-tls"], Some(long));
    let mut child = c.stderr(std::process::Stdio::piped()).spawn().unwrap();
    let mut err = std::io::BufReader::new(child.stderr.take().unwrap());
    let _server = Server(child, dir.clone());
    let mut log = String::new();
    while !log.contains("serving on") {
        if err.read_line(&mut log).unwrap_or(0) == 0 {
            panic!("the server didn't start:\n{log}");
        }
    }
    assert!(log.contains("WITHOUT TLS"), "{log}");
}

/// A server other machines can reach, whose token can change main directly (no --safe, protect_main off), says so
/// as it starts; --safe, or protect_main on, and it doesn't. A loopback server doesn't either.
#[test]
fn an_exposed_server_whose_token_can_change_main_warns() {
    use std::io::BufRead;
    let dir = std::env::temp_dir().join(format!("chronos-serve-{}-exposed", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let long = "0123456789abcdef01234567";
    // the log up to "serving on", from a server started with `args`
    let started = |listen: &str, args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"));
        c.args(["serve", dir.to_str().unwrap(), "--listen", listen, "--pg", "off"]).args(args);
        c.env("CHRONOS_TOKEN", long).env_remove("CHRONOS_ADMIN_TOKEN");
        c.env("CHRONOS_PROTECT_MAIN", "off"); // (a folder serve makes is protected: this one starts without)
        let mut child = c.stderr(std::process::Stdio::piped()).spawn().unwrap();
        let mut err = std::io::BufReader::new(child.stderr.take().unwrap());
        let mut log = String::new();
        while !log.contains("serving on") {
            if err.read_line(&mut log).unwrap_or(0) == 0 {
                panic!("the server didn't start:\n{log}");
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        log
    };
    let warned = |log: &str| log.contains("can change main directly");
    let log = started("0.0.0.0:0", &["--insecure-no-tls"]);
    assert!(warned(&log) && log.contains("--safe --admin-token") && log.contains("protect_main = on"), "{log}");
    let log = started("0.0.0.0:0", &["--insecure-no-tls", "--safe", "--admin-token", "abcdef0123456789abcdef01"]);
    assert!(!warned(&log), "--safe: the server token alone is the guest\n{log}");
    assert!(!warned(&started("127.0.0.1:0", &[])), "loopback");
    {
        let db = chronos::Db::open(&dir).unwrap();
        chronos::sql::run(&db, chronos::MAIN, "alter system set protect_main = on", &[]).unwrap();
        db.close().unwrap();
    }
    let log = started("0.0.0.0:0", &["--insecure-no-tls"]);
    assert!(!warned(&log), "protect_main on\n{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Wrong tokens from one address: ten go as any wrong token does (401), then the address waits
/// before its next try, a wait that doubles with each further failure: meanwhile even the right
/// token is told to wait (429), and after it, it is let in. Every reply says not to sniff its type
/// or cache it.
#[test]
fn wrong_tokens_back_off_per_address() {
    let dir = std::env::temp_dir().join(format!("chronos-serve-{}-backoff", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap(), "--token", "backoff-backoff-backoff-1"]);
    cmd.env("CHRONOS_LOGIN_FREE_TRIES", "10"); // (the default; the tests' servers get more)
    let s = common::serve(&mut cmd, false);
    let (_server, url) = (Server(s.child, dir), format!("http://127.0.0.1:{}", s.http));
    let post = |auth: &str| {
        let out = Command::new("curl")
            .args(["-s", "-D", "-", "-o", "/dev/null", "-X", "POST", &format!("{url}/v1/branches")])
            .args(["-H", &format!("Authorization: Bearer {auth}")])
            .output()
            .unwrap();
        let head = String::from_utf8(out.stdout).unwrap();
        assert!(head.contains("X-Content-Type-Options: nosniff") && head.contains("Cache-Control: no-store"), "{head}");
        assert!(!head.contains("Strict-Transport-Security"), "only over TLS: {head}");
        head.split(' ').nth(1).unwrap_or_default().to_string()
    };
    for _ in 0..10 {
        assert_eq!(post("wrong"), "401");
    }
    assert_eq!(post("backoff-backoff-backoff-1"), "200", "ten wrong tries are free");
    assert_eq!(post("wrong"), "401");
    assert_eq!(post("backoff-backoff-backoff-1"), "429", "then the address waits");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(post("backoff-backoff-backoff-1"), "200");
    assert_eq!(post("wrong"), "401");
    assert_eq!(post("wrong"), "429", "the wait doubled");
}

#[test]
fn http_writes_follow_sql_rules() {
    let (_server, url) = start("rules", None);
    let post = |op: &str, body: &str| {
        let out = Command::new("curl")
            .args([
                "-s",
                "-w",
                "\n%{http_code}",
                "-X",
                "POST",
                &format!("{url}/v1/{op}"),
                "-H",
                "Content-Type: application/json",
                "-d",
                body,
            ])
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let (body, code) = text.rsplit_once('\n').unwrap();
        (code.to_string(), body.to_string())
    };
    assert_eq!(
        post("sql", r#"{"sql": "create table users (id int primary key, email text unique not null)"}"#).0,
        "200"
    );
    assert_eq!(post("put", r#"{"table": "users", "id": "1", "record": {"email": "a@x"}}"#).0, "200");
    let (code, body) = post("put", r#"{"table": "users", "id": "2", "record": {"email": "a@x"}}"#);
    assert_eq!(code, "409", "{body}");
    assert!(body.contains("users_email_key"), "{body}");
    let (code, body) = post("put", r#"{"table": "users", "id": "2", "record": {"mail": "b@x"}}"#);
    assert_eq!(code, "400", "{body}");
    assert!(body.contains("no column \\\"mail\\\""), "{body}");
}

/// Without a server token, no header is the database's own user; a token that matches nobody
/// (a typo, or a disabled agent's) is refused rather than taken as the owner.
#[test]
fn unknown_tokens_are_refused_without_a_server_token() {
    let (_server, url) = start("no-token", None);
    let post = |auth: Option<&str>, op: &str, body: &str| {
        let mut c = Command::new("curl");
        c.args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            &format!("{url}/v1/{op}"),
            "-H",
            "Content-Type: application/json",
            "-d",
            body,
        ]);
        if let Some(a) = auth {
            c.args(["-H", &format!("Authorization: Bearer {a}")]);
        }
        String::from_utf8(c.output().unwrap().stdout).unwrap()
    };
    let put = r#"{"table": "t", "id": "1", "record": {"v": 1}}"#;
    assert_eq!(post(None, "put", put), "200");
    assert_eq!(post(Some("made-up"), "put", put), "401");
    let made = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            &format!("{url}/v1/create_agent"),
            "-H",
            "Content-Type: application/json",
            "-d",
            r#"{"name": "bot"}"#,
        ])
        .output()
        .unwrap();
    let made: serde_json::Value = serde_json::from_slice(&made.stdout).unwrap();
    let bot = made["token"].as_str().unwrap().to_string();
    assert_eq!(post(Some(&bot), "put", put), "403", "an agent may not write main");
    assert_eq!(post(None, "alter_agent", r#"{"name": "bot", "disabled": true}"#), "200");
    assert_eq!(post(Some(&bot), "put", put), "401", "a disabled agent's token stops working");
}

/// Agents over HTTP: the server token is the database's own user; an agent's token acts as the
/// agent, which may change only its own worlds and can't merge unless allowed.
#[test]
fn agents_over_http() {
    let (_server, url) = start("agent-tokens", Some("admin-secret-admin-secret-0"));
    let post = |auth: &str, op: &str, body: &str| {
        let out = Command::new("curl")
            .args([
                "-s",
                "-w",
                "\n%{http_code}",
                "-X",
                "POST",
                &format!("{url}/v1/{op}"),
                "-H",
                &format!("Authorization: Bearer {auth}"),
                "-H",
                "Content-Type: application/json",
                "-d",
                body,
            ])
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let (body, code) = text.rsplit_once('\n').unwrap();
        (code.to_string(), serde_json::from_str::<serde_json::Value>(body).unwrap_or_default())
    };
    let admin = "admin-secret-admin-secret-0";
    assert_eq!(post(admin, "put", r#"{"table": "t", "id": "1", "record": {"v": 1}}"#).0, "200");
    let (code, made) = post(admin, "create_agent", r#"{"name": "bot", "max_worlds": 5}"#);
    assert_eq!(code, "200", "{made}");
    let bot = made["token"].as_str().unwrap().to_string();

    let (code, forked) = post(&bot, "fork", r#"{"name": "bot-1"}"#);
    assert_eq!((code.as_str(), forked["world"]["owner"].as_str()), ("200", Some("bot")), "{forked}");
    assert_eq!(post(&bot, "put", r#"{"branch": "bot-1", "table": "t", "id": "2", "record": {"v": 2}}"#).0, "200");
    assert_eq!(post(&bot, "get", r#"{"table": "t", "id": "1"}"#).0, "200", "reads anywhere");
    assert_eq!(post(&bot, "put", r#"{"table": "t", "id": "3", "record": {"v": 3}}"#).0, "403", "not main");
    assert_eq!(post(&bot, "sql", r#"{"branch": "bot-1", "sql": "select count(*) from t"}"#).0, "200");
    assert_eq!(post(&bot, "sql", r#"{"sql": "merge world 'bot-1'"}"#).0, "403", "no world changes through SQL");
    assert_eq!(post(&bot, "merge", r#"{"branch": "bot-1", "dry_run": true}"#).0, "200", "a preview is a read");
    assert_eq!(post(&bot, "merge", r#"{"branch": "bot-1"}"#).0, "403", "a person merges");
    assert_eq!(post(&bot, "agents", "{}").0, "403", "only admins manage agents");
    assert_eq!(post("chronos_not_a_token", "branches", "{}").0, "401");

    let (code, audit) = post(admin, "audit", r#"{"agent": "bot"}"#);
    assert_eq!(code, "200");
    let actions: Vec<&str> =
        audit["events"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["write", "fork from main"], "{audit}");
    assert_eq!(post(admin, "merge", r#"{"branch": "bot-1"}"#).0, "200");
    // a simulation's worlds are the agent's, and count against its max_worlds (5)
    let sim = r#"{"worlds": 6, "prefix": "s", "script": "select 1", "score": "select $1::int", "keep": 2}"#;
    let (code, r) = post(&bot, "simulate", sim);
    assert_eq!(code, "200", "{r}");
    assert_eq!(r["worlds"][0]["world"], "s_5", "{r}");
    assert_eq!(post(admin, "world", r#"{"branch": "s_5"}"#).1["owner"], "bot");
    let (code, r) = post(&bot, "simulate", &sim.replace("\"s\"", "\"s2\"").replace("2}", "\"all\"}"));
    assert_eq!(code, "403", "{r}");
    // the database's own tables are no rows to anyone, the server's token included
    for (op, body) in [
        ("get", r#"{"table": "_sonos_agents", "id": "bot"}"#),
        ("put", r#"{"table": "_sonos_agents", "id": "bot", "record": {"can": ["admin"]}}"#),
        ("find", r#"{"table": "_sonos_agents"}"#),
    ] {
        assert_eq!(post(admin, op, body).0, "403", "{op}");
    }
}

#[test]
fn metrics_for_prometheus() {
    let (_server, url) = start("metrics", Some("admin-secret-admin-secret-0"));
    let curl = |args: &[&str]| {
        let out = Command::new("curl").args(["-s", "-w", "\n%{http_code}"]).args(args).output().unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let (body, code) = text.rsplit_once('\n').unwrap();
        (code.to_string(), body.to_string())
    };
    let auth = "Authorization: Bearer admin-secret-admin-secret-0";
    let sql = format!("{url}/v1/sql");
    curl(&[
        "-X",
        "POST",
        &sql,
        "-H",
        auth,
        "-H",
        "Content-Type: application/json",
        "-d",
        r#"{"sql": "create table t (id int primary key); insert into t values (1)"}"#,
    ]);
    curl(&[
        "-X",
        "POST",
        &sql,
        "-H",
        auth,
        "-H",
        "Content-Type: application/json",
        "-d",
        r#"{"sql": "select * from t"}"#,
    ]);
    curl(&[
        "-X",
        "POST",
        &sql,
        "-H",
        auth,
        "-H",
        "Content-Type: application/json",
        "-d",
        r#"{"sql": "insert into t values (1)"}"#,
    ]); // a duplicate: an error
    assert_eq!(curl(&[&format!("{url}/metrics")]).0, "401", "the server's token is needed");
    let (code, text) = curl(&[&format!("{url}/metrics"), "-H", auth]);
    assert_eq!(code, "200", "{text}");
    for want in [
        "chronos_op_seconds_count{op=\"select\"} 1",
        "chronos_op_seconds_count{op=\"insert\"} 2",
        "chronos_op_seconds_count{op=\"schema\"} 1",
        "chronos_op_seconds{op=\"select\",quantile=\"0.99\"} ",
        "# TYPE chronos_errors_total counter\nchronos_errors_total 1",
        "chronos_rows_written_total ",
        "chronos_worlds 1",
    ] {
        assert!(text.contains(want), "no {want:?} in\n{text}");
    }
}

/// The HTTP server itself: two requests on one connection, then requests it turns away.
#[test]
fn http_keep_alive_and_bad_requests() {
    use std::io::{BufRead, BufReader, Read, Write};
    let (_server, url) = start("raw-http", None);
    let addr = url.trim_start_matches("http://");
    let mut c = std::net::TcpStream::connect(addr).unwrap();
    let mut r = BufReader::new(c.try_clone().unwrap());
    let mut response = || {
        let (mut status, mut len) = (String::new(), 0);
        r.read_line(&mut status).unwrap();
        loop {
            let mut h = String::new();
            r.read_line(&mut h).unwrap();
            if h == "\r\n" {
                break;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).unwrap();
        (status.split(' ').nth(1).unwrap().to_string(), String::from_utf8(body).unwrap())
    };
    let body = r#"{"table": "t", "id": "1", "record": {"v": 1}}"#;
    write!(c, "POST /v1/put HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    assert_eq!(response().0, "200");
    write!(c, "GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
    assert_eq!(response().0, "200", "the same connection serves a second request");
    write!(c, "POST /v1/put HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
    assert_eq!(response().0, "411");

    let mut c = std::net::TcpStream::connect(addr).unwrap();
    let huge = "x".repeat(70_000);
    let _ = write!(c, "GET /v1/health HTTP/1.1\r\nX-Big: {huge}\r\n\r\n");
    let mut out = String::new();
    let _ = c.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 431"), "{out}");
}

/// Stopping the server (SIGTERM, as systemd or `kill` stop it) closes the database cleanly: a
/// world's acknowledged writes, not yet synced, are kept, and nothing is marked as crashed.
#[test]
fn a_stopped_server_keeps_acknowledged_writes() {
    let (mut server, url) = start("sigterm", None);
    let post = |op: &str, body: &str| {
        let out = Command::new("curl")
            .args(["-s", "-X", "POST", &format!("{url}/v1/{op}"), "-H", "Content-Type: application/json", "-d", body])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    post("fork", r#"{"name": "w"}"#);
    for i in 0..20 {
        assert!(
            post("put", &format!(r#"{{"branch": "w", "table": "t", "id": "{i}", "record": {{"v": {i}}}}}"#))
                .contains("ok")
        );
    }
    assert!(Command::new("kill").arg(server.0.id().to_string()).status().unwrap().success());
    let status = server.0.wait().unwrap();
    assert!(status.success(), "a clean exit: {status:?}");
    let db = chronos::Db::open(&server.1).unwrap();
    assert_eq!(db.scan("w", "t/", usize::MAX).unwrap().len(), 20, "every acknowledged write");
    assert!(!db.world("w").unwrap().flagged, "a stop is no crash");
    db.merge("w", chronos::Resolve::Fail).unwrap();
}
