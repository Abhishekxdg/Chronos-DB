//! `POST /mcp` on `chronos serve`: the MCP tools over HTTP, as the agent whose token comes with the request (the
//! guest under `--safe`), so an agent in a container needs only its token, never the folder.

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
    let dir = std::env::temp_dir().join(format!("chronos-mcp-http-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]).args(args);
    let s = common::serve(&mut cmd, false);
    (Server(s.child, dir), s.http)
}

/// One request: its status, and its body (empty for none).
fn send(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (head.split(' ').nth(1).unwrap().parse().unwrap(), body.to_string())
}

fn post(port: u16, token: Option<&str>, op: &str, body: Value) -> Value {
    let (status, out) = send(port, "POST", &format!("/v1/{op}"), token, &body.to_string());
    assert_eq!(status, 200, "{out}");
    serde_json::from_str(&out).unwrap()
}

/// A JSON-RPC request to `/mcp`: the status and the reply.
fn rpc(port: u16, token: Option<&str>, method: &str, params: Value) -> (u16, Value) {
    let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let (status, out) = send(port, "POST", "/mcp", token, &msg.to_string());
    (status, serde_json::from_str(&out).unwrap_or(Value::Null))
}

/// A tools/call: its text, and whether it is an error.
fn tool(port: u16, token: Option<&str>, name: &str, args: Value) -> (String, bool) {
    let (status, r) = rpc(port, token, "tools/call", json!({ "name": name, "arguments": args }));
    assert_eq!(status, 200, "{r}");
    let res = &r["result"];
    (res["content"][0]["text"].as_str().unwrap_or("").to_string(), res["isError"] == json!(true))
}

fn agent(port: u16, name: &str, can: &str) -> String {
    let v = post(port, Some(ADMIN), "sql", json!({ "sql": format!("create agent {name} with (can = '{can}')") }));
    v["results"][0]["rows"][0]["token"].as_str().unwrap().to_string()
}

#[test]
fn an_agent_uses_the_tools_over_http_with_its_token() {
    let (_s, port) = start("agent", &["--safe", "--admin-token", ADMIN]);
    post(port, Some(ADMIN), "put", json!({ "table": "t", "id": "1", "record": { "v": 1 } }));
    let bot = agent(port, "bot", "read,fork,write_own");
    let me = Some(bot.as_str());

    // initialize: told a person merges; the tools it can't use aren't listed
    let (status, r) = rpc(port, me, "initialize", json!({ "protocolVersion": "2025-06-18" }));
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["result"]["serverInfo"]["name"], "chronos");
    assert!(r["result"]["instructions"].as_str().unwrap().contains("you can't merge"), "{r}");
    let (_, r) = rpc(port, me, "tools/list", json!({}));
    let names: Vec<&str> =
        r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"fork") && names.contains(&"sql"), "{names:?}");
    assert!(!names.contains(&"merge") && !names.contains(&"restore"), "{names:?}");

    // reads main, changes only what it forked, and the fork is its own across requests
    let (out, err) = tool(port, me, "get", json!({ "table": "t", "id": "1" }));
    assert!(!err && out.contains('1'), "{out}");
    let (out, err) = tool(port, me, "put", json!({ "table": "t", "id": "2", "record": { "v": 2 } }));
    assert!(err, "main isn't the agent's to write: {out}");
    let (out, err) = tool(port, me, "fork", json!({ "name": "bot_w" }));
    assert!(!err, "{out}");
    let (out, err) = tool(port, me, "put", json!({ "branch": "bot_w", "table": "t", "id": "2", "record": { "v": 2 } }));
    assert!(!err, "{out}");
    let (out, err) = tool(port, me, "sql", json!({ "branch": "bot_w", "sql": "select count(*) from t" }));
    assert!(!err && out.contains('2'), "{out}");
    let (out, err) = tool(port, me, "sql", json!({ "branch": "main", "sql": "delete from t" }));
    assert!(err, "{out}");
    let w = post(port, Some(ADMIN), "world", json!({ "branch": "bot_w" }));
    assert_eq!(w["owner"], "bot", "{w}");
    assert!(tool(port, me, "merge", json!({ "branch": "bot_w" })).1, "merging isn't the agent's");

    // a notification, or a response to the server: 202 and no body
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
    assert_eq!(send(port, "POST", "/mcp", me, &note), (202, String::new()));
    let answer = json!({ "jsonrpc": "2.0", "id": 9, "result": {} }).to_string();
    assert_eq!(send(port, "POST", "/mcp", me, &answer).0, 202);
    // not JSON, a batch, a method it doesn't have; no event stream to GET
    let (status, out) = send(port, "POST", "/mcp", me, "{nope");
    assert!(status == 400 && out.contains("-32700"), "{out}");
    let (status, out) = send(port, "POST", "/mcp", me, "[]");
    assert!(status == 400 && out.contains("-32600"), "{out}");
    let (_, r) = rpc(port, me, "resources/list", json!({}));
    assert_eq!(r["error"]["code"], -32601, "{r}");
    assert_eq!(send(port, "GET", "/mcp", me, "").0, 405);

    // a disabled agent's token stops working; a wrong one is refused
    post(port, Some(ADMIN), "sql", json!({ "sql": "alter agent bot set (disabled = true)" }));
    assert_eq!(rpc(port, me, "initialize", json!({})).0, 401);
    assert_eq!(rpc(port, Some("not-a-token-of-anyone-at-all"), "initialize", json!({})).0, 401);
}

#[test]
fn mcp_over_http_is_only_ever_an_agent() {
    // --safe: no token is the guest, which forks and changes only what the guest forked; the admin token is refused
    let (_s, port) = start("guest", &["--safe", "--admin-token", ADMIN]);
    let (status, r) = rpc(port, None, "initialize", json!({}));
    assert_eq!(status, 200, "{r}");
    let (out, err) = tool(port, None, "fork", json!({ "name": "guest_w" }));
    assert!(!err, "{out}");
    assert!(!tool(port, None, "put", json!({ "branch": "guest_w", "table": "t", "id": "1", "record": {} })).1);
    assert!(tool(port, None, "put", json!({ "table": "t", "id": "1", "record": {} })).1, "not main");
    let (status, r) = rpc(port, Some(ADMIN), "initialize", json!({}));
    assert_eq!(status, 403, "{r}");
    assert!(r["error"].as_str().unwrap().contains("agent's token"), "{r}");
    // a web page is refused, as on every other path
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
    let req = format!(
        "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 403"), "{raw}");

    // without --safe: the server's token is the database's own user, which MCP over HTTP isn't
    let (_s2, port) = start("owner", &[]);
    let (status, _) = rpc(port, None, "initialize", json!({}));
    assert_eq!(status, 403, "loopback without a token is the database's own user");
    let (_s3, port) = start("token", &["--token", SERVER]);
    assert_eq!(rpc(port, Some(SERVER), "initialize", json!({})).0, 403);
    assert_eq!(rpc(port, None, "initialize", json!({})).0, 401, "a server with a token wants one");
    let v = post(port, Some(SERVER), "sql", json!({ "sql": "create agent helper" }));
    let helper = v["results"][0]["rows"][0]["token"].as_str().unwrap().to_string();
    assert_eq!(rpc(port, Some(&helper), "initialize", json!({})).0, 200);
}

/// What the review of `/mcp` found it must keep as `/v1/` does: an agent's `max_concurrent` counts every tool call,
/// not only SQL's statements; `describe` shows no rows of a table a row-level security policy limits the agent on;
/// a token disabled between calls stops the next one.
#[test]
fn mcp_over_http_keeps_an_agents_limits() {
    let (_s, port) = start("limits", &["--safe", "--admin-token", ADMIN]);
    let sql = |q: &str| post(port, Some(ADMIN), "sql", json!({ "sql": q }));
    sql("create table notes (id int primary key, tenant text, body text);
         insert into notes values (1, 'lim_bot', 'mine'), (2, 'someone', 'theirs');
         create table open (id int primary key, body text);
         insert into open values (1, 'for all');
         alter table notes enable row level security;
         create policy tenant on notes using (tenant = current_user)");
    let v = sql("create agent lim_bot with (can = 'read,fork,write_own', max_concurrent = 1)");
    let bot = v["results"][0]["rows"][0]["token"].as_str().unwrap().to_string();
    let me = Some(bot.as_str());
    // (policies limit an agent through a role of its name)
    sql("create role lim_bot; grant select on notes, open to lim_bot");

    // describe: the policy's table shows no rows (SQL applies the policy), the open one its sample
    let (out, err) = tool(port, me, "describe", json!({}));
    assert!(!err, "{out}");
    assert!(out.contains("row-level security") && !out.contains("theirs") && !out.contains("mine"), "{out}");
    assert!(out.contains("for all"), "{out}");
    let (out, err) = tool(port, me, "sql", json!({ "sql": "select body from notes order by id" }));
    assert!(!err && out.contains("mine") && !out.contains("theirs"), "{out}");

    // max_concurrent = 1: while a statement of its runs, its other tool calls are refused
    let slow = std::thread::spawn({
        let bot = bot.clone();
        move || tool(port, Some(&bot), "sql", json!({ "sql": "select pg_sleep(2)" }))
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (out, err) = tool(port, me, "branches", json!({}));
    assert!(err && out.contains("at once"), "a second call while one runs: {out}");
    let (out, err) = slow.join().unwrap();
    assert!(!err, "{out}");
    assert!(!tool(port, me, "branches", json!({})).1, "free again");

    // disabled between two calls: the next one is refused
    sql("alter agent lim_bot set (disabled = true)");
    let (status, _) = rpc(port, me, "tools/call", json!({ "name": "branches", "arguments": {} }));
    assert_eq!(status, 401);
}

#[test]
fn the_admin_token_is_refused_without_safe_mode_too() {
    let (_s, port) = start("admin", &["--token", SERVER, "--admin-token", ADMIN]);
    assert_eq!(rpc(port, Some(ADMIN), "initialize", json!({})).0, 403);
}

/// A message past 8 MB is refused from its header (413), before its body is read; `find` and `history` give a model
/// at most 1000 rows, `find` pages up to offset 100000, with a beam up to 4096, and say to use SQL past that.
#[test]
fn mcp_over_http_bounds_messages_and_pages() {
    let (_s, port) = start("bounds", &["--safe", "--admin-token", ADMIN]);
    let bot = agent(port, "page_bot", "read,fork,write_own");
    let me = Some(bot.as_str());
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let head = format!(
        "POST /mcp HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {bot}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n",
        (8 << 20) + 1
    );
    s.write_all(head.as_bytes()).unwrap();
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    assert!(raw.starts_with("HTTP/1.1 413"), "{raw}");

    post(port, Some(ADMIN), "put", json!({ "table": "t", "id": "1", "record": { "v": 1 } }));
    assert!(!tool(port, me, "find", json!({ "table": "t", "limit": 1000, "offset": 100_000, "ef": 4096 })).1);
    for (name, args) in [
        ("find", json!({ "table": "t", "limit": 1001 })),
        ("find", json!({ "table": "t", "offset": 100_001 })),
        ("find", json!({ "table": "t", "ef": 4097 })),
        ("find", json!({ "table": "t", "limit": -1 })),
        ("history", json!({ "branch": "main", "limit": 5000 })),
    ] {
        let (out, err) = tool(port, me, name, args.clone());
        assert!(err && (out.contains("at most") || out.contains("whole number")), "{name} {args}: {out}");
    }
    assert!(!tool(port, me, "history", json!({ "branch": "main", "limit": 1000 })).1);
}
