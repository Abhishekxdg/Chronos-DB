//! `POST /studio/api/<op>` on `chronos serve --studio-api`: the Studio's calls over the server's port, for a service
//! that hosts the Studio's page itself. Admin token only; off without the flag; no path-taking calls.

mod common;

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command};

const ADMIN: &str = "ADMIN-ADMIN-ADMIN-ADMIN-0";
const SERVER: &str = "SERVER-SERVER-SERVER-SERVER-0";

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(tag: &str, args: &[&str]) -> (Server, u16) {
    let dir = std::env::temp_dir().join(format!("chronos-studio-api-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]).args(args);
    let s = common::serve(&mut cmd, false);
    (Server(s.child, dir), s.http)
}

fn send(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (head.split(' ').nth(1).unwrap().parse().unwrap(), serde_json::from_str(body).unwrap_or(Value::Null))
}

fn studio(port: u16, token: Option<&str>, op: &str, body: Value) -> (u16, Value) {
    send(port, "POST", &format!("/studio/api/{op}"), token, &body.to_string())
}

#[test]
fn the_studio_api_is_off_without_its_flag() {
    let (_s, port) = start("off", &["--admin-token", ADMIN]);
    let (status, r) = studio(port, Some(ADMIN), "tables", json!({}));
    assert_eq!(status, 404, "{r}");
    assert!(r["error"].as_str().unwrap().contains("--studio-api"), "{r}");
}

#[test]
fn the_studio_api_takes_the_admin_token_and_runs_the_studios_calls() {
    let (_s, port) = start("on", &["--studio-api", "--token", SERVER, "--admin-token", ADMIN]);
    // no token, the server's token, a wrong token: refused (the server's token is the database's user, not the host's)
    assert_eq!(studio(port, None, "tables", json!({})).0, 401);
    let (status, r) = studio(port, Some(SERVER), "tables", json!({}));
    assert_eq!(status, 403, "{r}");
    assert!(r["error"].as_str().unwrap().contains("admin token"), "{r}");
    assert_eq!(studio(port, Some("not-the-admin-token-at-all-x"), "tables", json!({})).0, 401);
    // GET is refused
    assert_eq!(send(port, "GET", "/studio/api/tables", Some(ADMIN), "").0, 405);

    // the admin token: SQL, then the Studio's own calls over what it made
    let (status, r) = studio(
        port,
        Some(ADMIN),
        "sql",
        json!({ "sql": "create table t (id int primary key, v text); insert into t values (1, 'a'), (2, 'b')" }),
    );
    assert_eq!(status, 200, "{r}");
    let (status, r) = studio(port, Some(ADMIN), "tables", json!({}));
    assert_eq!(status, 200, "{r}");
    let t = r["tables"].as_array().unwrap().iter().find(|t| t["name"] == "t").unwrap_or_else(|| panic!("{r}"));
    assert_eq!(t["rows"], 2, "{r}");
    let (status, r) = studio(port, Some(ADMIN), "status", json!({}));
    assert_eq!(status, 200, "{r}");
    assert!(r["version"].is_string() && r["history"]["retention"].is_number(), "{r}");
    assert_eq!(studio(port, Some(ADMIN), "sessions", json!({})).0, 200);
    let (status, r) = studio(port, Some(ADMIN), "whoami", json!({}));
    assert_eq!((status, r["mode"].as_str()), (200, Some("key")), "{r}");
    // an /v1 op through it, and the world calls
    assert_eq!(studio(port, Some(ADMIN), "fork", json!({ "name": "w1" })).0, 200);
    let (status, r) = studio(port, Some(ADMIN), "worlds", json!({}));
    assert_eq!(status, 200, "{r}");
    assert!(r["branches"].as_array().unwrap().iter().any(|w| w["name"] == "w1"), "{r}");
    // a path on the server's machine, and a path that isn't a call
    let (status, r) = studio(port, Some(ADMIN), "backup", json!({ "path": "/tmp/x" }));
    assert_eq!(status, 403, "{r}");
    // nor through SQL (a console's statement must not write a copy of the database wherever the process can)
    let target = std::env::temp_dir().join(format!("chronos-studio-api-backup-{}", std::process::id()));
    let (status, r) =
        studio(port, Some(ADMIN), "sql", json!({ "sql": format!("backup database to '{}'", target.display()) }));
    assert_eq!(status, 403, "{r}");
    // (a SQL error comes back as an object with its message and SQLSTATE, other errors as text)
    let msg = r["error"].as_str().or(r["error"]["message"].as_str()).unwrap_or_default();
    assert!(msg.contains("path on the server"), "{r}");
    assert!(!target.exists(), "nothing was written");
    assert_eq!(studio(port, Some(ADMIN), "", json!({})).0, 404);
    assert_eq!(studio(port, Some(ADMIN), "a/b", json!({})).0, 404);
    // a bad body
    let (status, _) = send(port, "POST", "/studio/api/tables", Some(ADMIN), "{not json");
    assert_eq!(status, 400);
}

#[test]
fn an_agents_token_cannot_use_the_studio_api() {
    let (_s, port) = start("agent", &["--studio-api", "--admin-token", ADMIN]);
    let v = send(
        port,
        "POST",
        "/v1/sql",
        Some(ADMIN),
        &json!({ "sql": "create agent bot with (can = 'read,fork')" }).to_string(),
    )
    .1;
    let token = v["results"][0]["rows"][0]["token"].as_str().unwrap().to_string();
    let (status, r) = studio(port, Some(&token), "tables", json!({}));
    assert_eq!(status, 403, "{r}");
}

#[test]
fn studio_sql_cannot_reach_server_files_even_where_the_server_allows_them() {
    let dir = std::env::temp_dir().join(format!("chronos-studio-api-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap(), "--studio-api", "--admin-token", ADMIN])
        .env("CHRONOS_ALLOW_SERVER_COPY", "1");
    let s = common::serve(&mut cmd, false);
    let (_guard, port) = (Server(s.child, dir), s.http);
    let (status, r) = studio(port, Some(ADMIN), "sql", json!({ "sql": "create table t (id int primary key)" }));
    assert_eq!(status, 200, "{r}");
    let out = std::env::temp_dir().join(format!("chronos-studio-api-copy-{}.csv", std::process::id()));
    let (status, r) = studio(port, Some(ADMIN), "sql", json!({ "sql": format!("copy t to '{}'", out.display()) }));
    assert_eq!(status, 403, "{r}");
    assert!(!out.exists(), "nothing was written");
}

#[test]
fn the_studio_api_needs_an_admin_token_to_start() {
    let dir = std::env::temp_dir().join(format!("chronos-studio-api-noadmin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["serve", dir.to_str().unwrap(), "--studio-api", "--listen", "127.0.0.1:0", "--pg", "off"])
        .env_remove("CHRONOS_ADMIN_TOKEN")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--studio-api needs an admin token"), "{out:?}");
}
