//! `chronos studio`: the web UI's listener. It prints a URL carrying a session token, serves the
//! page with strict security headers, refuses API calls without the token, from another page
//! (Origin) or under another name (Host), runs the Studio's flows (worlds, tables, rows, SQL,
//! fork, diff, merge), and works beside a running `chronos serve` on the same folder.

mod common;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Studio {
    child: Child,
    port: u16,
    token: String,
}

impl Drop for Studio {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-review-studio-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("db")
}

/// `chronos studio <db> --no-open --port 0`, once it has printed its URL.
fn studio(db: &Path) -> Studio {
    let child = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["studio", db.to_str().unwrap(), "--no-open", "--port", "0"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut s = Studio { child, port: 0, token: String::new() }; // killed on drop, a failed start included
    let mut err = BufReader::new(s.child.stderr.take().unwrap());
    let mut log = String::new();
    loop {
        let mut line = String::new();
        assert!(err.read_line(&mut line).unwrap_or(0) > 0, "studio didn't start:\n{log}");
        log.push_str(&line);
        // "chronos: studio on http://127.0.0.1:PORT/#t=TOKEN"
        if let Some(url) = line.trim().strip_prefix("chronos: studio on http://127.0.0.1:") {
            let (port, token) = url.split_once("/#t=").expect("the URL carries the token after #t=");
            assert_eq!(token.len(), 64, "a 256-bit token, in hex");
            assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
            std::thread::spawn(move || std::io::copy(&mut err, &mut std::io::sink()));
            (s.port, s.token) = (port.parse().unwrap(), token.to_string());
            return s;
        }
    }
}

/// A raw request; returns (status, headers, body) once the server closes the connection.
fn raw(port: u16, req: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = vec![];
    s.read_to_end(&mut out).unwrap();
    let out = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let status = head.split(' ').nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

/// POST /studio/api/<op> with `extra` headers (each ending in \r\n).
fn post(port: u16, op: &str, extra: &str, body: &Value) -> (u16, Value) {
    let body = body.to_string();
    let req = format!(
        "POST /studio/api/{op} HTTP/1.1\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, body) = raw(port, &req);
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

impl Studio {
    /// A call as the page makes it: the right Host, Origin and token.
    fn call(&self, op: &str, args: Value) -> (u16, Value) {
        let p = self.port;
        post(
            p,
            op,
            &format!("Host: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\nX-Studio-Token: {}\r\n", self.token),
            &args,
        )
    }

    fn ok(&self, op: &str, args: Value) -> Value {
        let (status, v) = self.call(op, args.clone());
        assert_eq!(status, 200, "{op} {args}: {v}");
        v
    }

    fn sql(&self, branch: &str, q: &str) -> Value {
        self.ok("sql", json!({ "branch": branch, "sql": q }))
    }
}

#[test]
fn serves_the_page_with_security_headers() {
    let db = scratch("page");
    let s = studio(&db);
    let (status, head, body) =
        raw(s.port, &format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", s.port));
    assert_eq!(status, 200, "{head}");
    assert!(body.contains("<title>Chronos Studio</title>"), "{body}");
    let head = head.to_ascii_lowercase();
    for h in [
        "cache-control: no-store",
        "content-security-policy: default-src 'none'; script-src 'self'",
        "frame-ancestors 'none'",
        "font-src 'self'",
        "x-frame-options: deny",
        "referrer-policy: no-referrer",
        "x-content-type-options: nosniff",
        "content-type: text/html",
    ] {
        assert!(head.contains(h), "missing {h} in\n{head}");
    }
    // no inline script or style (the CSP would block them): every script and stylesheet is a file
    assert!(!body.contains("<style") && !body.contains(" style="), "{body}");
    for tag in body.split("<script").skip(1) {
        assert!(tag.split('>').next().unwrap().contains(" src="), "an inline script: {body}");
    }
    // the page's own files, embedded from studio/dist (absent: the fallback page, and nothing to fetch)
    let files: Vec<&str> = body
        .split(['"', '\''])
        .filter(|a| a.starts_with("./assets/") && (a.ends_with(".js") || a.ends_with(".css")))
        .collect();
    assert!(!files.is_empty() || body.contains("Studio isn't built into this binary"), "{body}");
    for f in files {
        let kind = if f.ends_with(".js") { "text/javascript" } else { "text/css" };
        let path = &f[1..];
        let (status, head, asset) =
            raw(s.port, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", s.port));
        assert_eq!(status, 200, "{path}");
        assert!(
            head.contains(kind) && head.to_ascii_lowercase().contains("cache-control: no-store") && !asset.is_empty()
        );
        // the bundled font, from the stylesheet (font-src 'self' allows it)
        for font in asset.split(['(', ')', '"', '\'']).filter(|u| u.ends_with(".woff2")) {
            let font = font.trim_start_matches("./").trim_start_matches("/assets/");
            let (status, head, _) = raw(
                s.port,
                &format!("GET /assets/{font} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", s.port),
            );
            assert_eq!(status, 200, "{font}");
            assert!(head.contains("font/woff2"), "{head}");
        }
    }
    // the UI never renders data as HTML (React escapes; nothing bypasses it)
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("studio/src");
    let mut stack = vec![src];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if !p.extension().is_some_and(|x| x == "ts" || x == "tsx" || x == "html") {
                continue; // styles, fonts, licenses
            }
            let code = std::fs::read_to_string(&p).unwrap();
            for bad in ["dangerouslySetInnerHTML", "innerHTML", "insertAdjacentHTML", "document.write", "eval("] {
                assert!(!code.contains(bad), "{bad} in {}", p.display());
            }
        }
    }
    let (status, _, _) =
        raw(s.port, &format!("GET /etc/passwd HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", s.port));
    assert_eq!(status, 404);
}

#[test]
fn the_api_needs_the_token_its_own_origin_and_host() {
    let db = scratch("auth");
    let s = studio(&db);
    let p = s.port;
    let host = format!("Host: 127.0.0.1:{p}\r\n");
    let token = format!("X-Studio-Token: {}\r\n", s.token);
    let args = json!({});
    let refused = |extra: &str, why: &str| {
        let (status, v) = post(p, "worlds", extra, &args);
        assert_eq!(status, 403, "{why}: {v}");
        assert!(v.get("branches").is_none(), "{why}: no data in a refusal");
    };
    refused(&host, "no token");
    let mut wrong = s.token.clone().into_bytes();
    wrong[0] = if wrong[0] == b'a' { b'b' } else { b'a' };
    refused(&format!("{host}X-Studio-Token: {}\r\n", String::from_utf8(wrong).unwrap()), "a wrong token");
    refused(&format!("{host}X-Studio-Token: {}\r\n", &s.token[..32]), "half the token");
    refused(&format!("{host}Authorization: Bearer {}\r\n", s.token), "the token in the wrong header");
    refused(&format!("{host}{token}Origin: http://evil.example\r\n"), "another page's Origin");
    refused(&format!("{host}{token}Origin: http://localhost:{p}\r\n"), "an Origin that only looks local");
    refused(&format!("{host}{token}Origin: null\r\n"), "a sandboxed page");
    refused(&format!("Host: evil.example:{p}\r\n{token}"), "DNS rebinding: another Host");
    refused(&format!("Host: localhost:{p}\r\n{token}"), "a Host other than the printed one");
    refused(&format!("Host: 127.0.0.1:{}\r\n{token}", p.wrapping_add(1)), "another port in Host");
    refused(&token, "no Host");
    let (status, v) = post(p, "worlds", &format!("{host}{token}"), &args);
    assert_eq!(status, 200, "{v}");
    assert!(v["branches"].as_array().unwrap().iter().any(|w| w["name"] == "main"));
    let (status, v) = s.call("worlds", args.clone());
    assert_eq!(status, 200, "with the page's own Origin too: {v}");
    // the page isn't served to another Host either (DNS rebinding reads it)
    let (status, _, _) = raw(p, &format!("GET / HTTP/1.1\r\nHost: evil.example:{p}\r\nConnection: close\r\n\r\n"));
    assert_eq!(status, 403);
    // a body that isn't JSON by its Content-Type is refused (a form post can't be made to look like one)
    let req = format!(
        "POST /studio/api/worlds HTTP/1.1\r\n{host}{token}Content-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    );
    assert_eq!(raw(p, &req).0, 415);
    // no backups to a path from the page
    let (status, v) = s.call("backup", json!({ "to": db.with_file_name("copy").to_str().unwrap() }));
    assert_eq!(status, 403, "{v}");
    assert!(!db.with_file_name("copy").exists());
}

#[test]
fn worlds_tables_rows_sql_fork_diff_and_merge() {
    let db = scratch("flows");
    let s = studio(&db);
    let r = s.sql(
        "main",
        "create table t (id int primary key, name text, tags jsonb); \
         insert into t select g, 'n' || g::text, '{\"a\": 1}' from generate_series(1, 500) g; select count(*) as n from t",
    );
    assert_eq!(r["results"][2]["rows"][0]["n"], 500, "{r}");
    s.ok("put", json!({ "table": "notes", "id": "a", "record": { "text": "<img src=x onerror=alert(1)>" } }));
    s.sql("main", "create table empty (id int primary key)");
    let tables = s.ok("tables", json!({ "branch": "main" }));
    assert_eq!(
        tables["tables"],
        json!([
            { "name": "empty", "rows": 0, "schema": true },
            { "name": "notes", "rows": 1, "schema": false },
            { "name": "t", "rows": 500, "schema": true },
        ]),
        "a table without rows is listed too; a JSON table has no schema"
    );
    // the grid's queries: a page of rows, a count with a filter, the table's columns and key
    let page = s.sql(
        "main",
        "select * from \"t\" where CAST(\"name\" AS text) ILIKE '%n1%' order by \"id\" DESC limit 200 offset 100",
    );
    let rows = page["results"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 11, "111 rows match: 100 skipped, 11 left");
    assert_eq!(rows[0]["tags"], json!({ "a": 1 }), "JSON cells come back as JSON");
    let info = s.sql(
        "main",
        "select column_name, data_type from information_schema.columns where table_schema = 'public' and table_name = 't' order by ordinal_position; \
         select k.column_name from information_schema.table_constraints tc join information_schema.key_column_usage k on k.constraint_schema = tc.constraint_schema and k.constraint_name = tc.constraint_name where tc.table_schema = 'public' and tc.table_name = 't' and tc.constraint_type = 'PRIMARY KEY'",
    );
    assert_eq!(info["results"][0]["rows"].as_array().unwrap().len(), 3, "{info}");
    assert_eq!(info["results"][1]["rows"][0]["column_name"], "id", "{info}");
    let notes = s.sql("main", "select * from \"notes\"");
    assert_eq!(notes["results"][0]["rows"][0]["text"], "<img src=x onerror=alert(1)>", "stored as text, shown as text");
    // SQL errors carry their SQLSTATE
    let (status, e) = s.call("sql", json!({ "sql": "select nope from t" }));
    assert_eq!(status, 400);
    assert_eq!(e["error"]["code"], "42703", "{e}");

    // edit in a new world: fork, edit a row (as the row drawer does), review, merge
    let w = s.ok("fork", json!({ "from": "main", "name": "edit-1" }));
    assert_eq!(w["world"]["parent"], "main");
    s.ok("put", json!({ "branch": "edit-1", "table": "t", "id": "7", "record": { "id": 7, "name": "seven", "tags": { "a": 2 } } }));
    s.sql(
        "edit-1",
        "delete from t where id = 8; insert into t (id, name) values (CAST('501' AS integer), CAST('new' AS text))",
    );
    assert_eq!(
        s.sql("main", "select name from t where id = 7")["results"][0]["rows"][0]["name"],
        "n7",
        "main is untouched"
    );
    let count = s.ok("diff", json!({ "branch": "edit-1", "count": true }));
    assert_eq!(count["total"], 3, "{count}");
    assert_eq!(count["tables"], json!({ "t": 3 }));
    // what in the database reads the changed columns: a row added and one deleted reach t's key
    let impact = s.ok("diff", json!({ "branch": "edit-1", "count": true, "readers": true }));
    assert_eq!(impact["total"], 3, "{impact}");
    let readers = impact["readers"].as_array().unwrap();
    assert!(
        readers.iter().any(|r| r["kind"] == "primary key" && r["reader"] == "t_pkey" && r["table"] == "t"),
        "{impact}"
    );
    assert_eq!(impact["coverage"].as_array().unwrap().len(), 3, "what wasn't traced is said: {impact}");
    let first = s.ok("diff", json!({ "branch": "edit-1", "limit": 2 }));
    assert_eq!(first["changes"].as_array().unwrap().len(), 2);
    let rest = s.ok("diff", json!({ "branch": "edit-1", "limit": 2, "after": first["next"] }));
    assert_eq!(rest["changes"].as_array().unwrap().len(), 1);
    assert!(rest["next"].is_null());
    let all: Vec<&Value> =
        first["changes"].as_array().unwrap().iter().chain(rest["changes"].as_array().unwrap()).collect();
    let mut keys: Vec<&str> = all.iter().map(|c| c["key"].as_str().unwrap()).collect();
    keys.sort();
    assert_eq!(keys, ["t/501", "t/7", "t/8"], "each change once, across the pages");
    let seven = all.iter().find(|c| c["key"] == "t/7").unwrap();
    assert_eq!(seven["columns"], json!(["name", "tags"]), "{seven}");
    assert_eq!(seven["before"]["name"], "n7");
    assert_eq!(seven["after"]["name"], "seven");
    let plan = s.ok("merge", json!({ "branch": "edit-1", "dry_run": true }));
    assert_eq!(plan["conflicts"], 0, "{plan}");
    assert_eq!(plan["rows_total"], 3);
    assert_eq!(plan["outcomes"], json!({ "apply": 3 }));
    assert!(plan["readers"].as_array().is_some_and(|r| !r.is_empty()) && plan["coverage"].is_array(), "{plan}");
    let merged = s.ok("merge", json!({ "branch": "edit-1", "confirm": true }));
    assert_eq!(merged["merged"], 3);
    assert_eq!(s.sql("main", "select name from t where id = 7")["results"][0]["rows"][0]["name"], "seven");
    s.ok("undo_merge", json!({ "branch": "edit-1" }));
    assert_eq!(s.sql("main", "select name from t where id = 7")["results"][0]["rows"][0]["name"], "n7", "undone");

    // a conflict comes back explained
    s.ok("fork", json!({ "from": "main", "name": "c1" }));
    s.sql("c1", "update t set name = 'ours' where id = 1");
    s.sql("main", "update t set name = 'theirs' where id = 1");
    let plan = s.ok("merge", json!({ "branch": "c1", "dry_run": true }));
    assert_eq!(plan["conflicts"], 1);
    assert_eq!(plan["rows"][0]["outcome"], "conflict", "conflicts first: {plan}");
    let (status, e) = s.call("merge", json!({ "branch": "c1", "confirm": true }));
    assert_eq!(status, 409);
    assert!(e["error"]["conflicts"][0]["explain"].as_str().is_some_and(|x| !x.is_empty()), "{e}");
    s.ok("discard", json!({ "branch": "c1" }));

    // history, agents and status
    assert!(!s.ok("history", json!({ "branch": "main" }))["events"].as_array().unwrap().is_empty());
    let a = s.ok("create_agent", json!({ "name": "bot", "can": ["read", "fork", "write_own"], "max_worlds": 3 }));
    assert!(a["token"].as_str().is_some_and(|t| !t.is_empty()));
    let agents = s.ok("agents", json!({}));
    assert!(!agents.to_string().contains(a["token"].as_str().unwrap()), "the token shows only once");
    s.ok("alter_agent", json!({ "name": "bot", "disabled": true }));
    let st = s.ok("status", json!({}));
    assert_eq!(st["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(st["encrypted"], false);
    assert!(st["counters"]["worlds"].as_u64().is_some());
    assert!(s.sql("main", "show metrics")["results"][0]["rows"].as_array().is_some());
}

/// What the grid shows of a real and a numeric(p,s) as Postgres shows them (`0.1`, not float8's
/// `0.10000000149011612`; `15.00` as text, keeping its scale), in SQL results and in the rows of
/// get, find and diff.
#[test]
fn real_and_numeric_values_read_as_postgres_shows_them() {
    let db = scratch("values");
    let s = studio(&db);
    s.sql(
        "main",
        "create table t (id int primary key, score real, price numeric(20,2), big float8); \
         insert into t values (1, 0.1, 15, 0.1), (2, 0.3, 16.5, null), (3, 'NaN', 'NaN', 2.5), (4, 1e-7, 123456789012345678.91, null)",
    );
    let r = s.sql("main", "select * from t order by id");
    let r = &r["results"][0];
    assert_eq!(r["types"], json!(["integer", "real", "numeric", "double precision"]), "{r}");
    let rows = r["rows"].as_array().unwrap();
    let shown: Vec<String> = rows.iter().map(|x| format!("{} {} {}", x["score"], x["price"], x["big"])).collect();
    assert_eq!(
        shown,
        ["0.1 \"15.00\" 0.1", "0.3 \"16.50\" null", "\"NaN\" \"NaN\" 2.5", "0.0000001 \"123456789012345678.91\" null",],
        "{r}"
    );
    // what the API's other row replies give: the same digits
    assert_eq!(s.ok("get", json!({ "table": "t", "id": "1" }))["row"]["score"].to_string(), "0.1");
    let found = s.ok("find", json!({ "table": "t", "where": { "id": 2 } }));
    assert_eq!(found["hits"][0]["row"]["score"].to_string(), "0.3", "{found}");
    s.ok("fork", json!({ "from": "main", "name": "w" }));
    s.sql("w", "update t set score = 0.7 where id = 1");
    let d = s.ok("diff", json!({ "branch": "w" }));
    assert_eq!(d["changes"][0]["before"]["score"].to_string(), "0.1", "{d}");
    assert_eq!(d["changes"][0]["after"]["score"].to_string(), "0.7", "{d}");
    // as of a moment: the same
    let at = s.sql("w", "select score from t as of '-0 seconds' where id = 2");
    assert_eq!(at["results"][0]["rows"][0]["score"].to_string(), "0.3", "{at}");
}

/// History counts a write's table rows, not the entries behind its indexes and constraints; views
/// and materialized views are listed with the tables; status says how far back history goes.
#[test]
fn history_counts_rows_views_are_listed_and_the_history_window_is_given() {
    let db = scratch("history");
    let s = studio(&db);
    s.sql(
        "main",
        "create table t (id int primary key, name text unique, n int); create index on t (n); \
         insert into t select g, 'n' || g, g from generate_series(1, 300) g",
    );
    s.sql("main", "update t set name = name || 'x', n = n + 1 where id <= 200");
    let h = s.ok("history", json!({ "branch": "main", "limit": 5 }));
    let rows: Vec<u64> = h["events"].as_array().unwrap().iter().map(|e| e["rows"].as_u64().unwrap()).collect();
    assert_eq!(rows[..2], [200, 300], "the update's 200 rows, then the insert's 300: {h}");

    s.sql(
        "main",
        "create view big_ones as select * from t where n > 250; create materialized view mv as select id from t where id <= 2",
    );
    let tables = s.ok("tables", json!({ "branch": "main" }));
    assert_eq!(
        tables["tables"],
        json!([
            { "name": "big_ones", "rows": null, "schema": true, "kind": "view" },
            { "name": "mv", "rows": 2, "schema": true, "kind": "materialized view" },
            { "name": "t", "rows": 300, "schema": true },
        ]),
        "{tables}"
    );
    let v = s.sql("main", "select count(*) as n from big_ones");
    assert_eq!(v["results"][0]["rows"][0]["n"], 50, "{v}");

    let st = s.ok("status", json!({}));
    assert_eq!(st["history"]["retention"], 3_600_000, "a new database keeps 1 hour: {st}");
    assert_eq!(st["history"]["from"], 0, "a database younger than its window keeps all it was: {st}");
}

#[test]
fn works_beside_a_running_serve() {
    let db = scratch("beside");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", db.to_str().unwrap()]);
    let mut serve = common::serve(&mut cmd, false);
    let s = studio(&db);
    s.sql("main", "create table t (id int primary key, v text); insert into t values (1, 'from studio')");
    assert_eq!(s.ok("tables", json!({}))["tables"], json!([{ "name": "t", "rows": 1, "schema": true }]));
    s.ok("fork", json!({ "from": "main", "name": "w" }));
    // the server sees what the studio wrote: one database, held by the server
    let body = json!({ "sql": "select v from t", "branch": "main" }).to_string();
    let req = format!(
        "POST /v1/sql HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, _, out) = raw(serve.http, &req);
    assert_eq!(status, 200, "{out}");
    assert!(out.contains("from studio"), "{out}");
    let (_, _, worlds) = raw(
        serve.http,
        "POST /v1/worlds HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    );
    assert!(worlds.contains("\"w\""), "{worlds}");
    // the server's own API still refuses web pages
    let req = format!(
        "POST /v1/sql HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        s.port,
        body.len()
    );
    assert_eq!(raw(serve.http, &req).0, 403);
    // with the server gone, the studio says so instead of failing oddly
    let _ = serve.child.kill();
    let _ = serve.child.wait();
    let (status, v) = s.call("worlds", json!({}));
    assert_eq!(status, 503, "{v}");
}
