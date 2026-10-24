//! `chronos serve --safe`: HTTP and Postgres clients that aren't agents get MCP's safe mode (fork,
//! and change only the worlds they forked), unless they present the admin token.

mod common;

use postgres::{Client, NoTls};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command};

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start() -> (Server, u16, u16) {
    let dir = std::env::temp_dir().join(format!("chronos-safe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap(), "--safe", "--admin-token", "ADMIN-ADMIN-ADMIN-ADMIN-0"]);
    let s = common::serve(&mut cmd, true);
    (Server(s.child, dir), s.http, s.pg)
}

fn post(port: u16, token: Option<&str>, op: &str, body: Value) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let body = body.to_string();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let req = format!(
        "POST /v1/{op} HTTP/1.0\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, json) = raw.split_once("\r\n\r\n").unwrap();
    (head.split(' ').nth(1).unwrap().parse().unwrap(), serde_json::from_str(json).unwrap())
}

fn pg(port: u16, password: &str) -> Client {
    Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main password={password}"), NoTls).unwrap()
}

#[test]
fn safe_mode_over_http_and_postgres() {
    let (_server, http, port) = start();
    let row = json!({"table": "t", "id": "1", "record": {"v": 1}});

    // HTTP without a token: the guest
    let (status, _) = post(http, None, "put", row.clone());
    assert_eq!(status, 403, "no writes to main");
    let (status, body) = post(http, None, "fork", json!({"name": "mine"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["world"]["owner"], "guest");
    let (status, _) = post(http, None, "put", json!({"table": "t", "id": "1", "record": {"v": 2}, "branch": "mine"}));
    assert_eq!(status, 200, "its own world");
    let (status, _) = post(http, None, "merge", json!({"branch": "mine"}));
    assert_eq!(status, 403, "no merges");
    let (status, _) = post(http, None, "sql", json!({"sql": "alter system set max_worlds = 5"}));
    assert_eq!(status, 403, "no system settings");
    let (status, _) = post(http, None, "sql", json!({"sql": "insert into t values (1)"}));
    assert_eq!(status, 403, "no SQL writes to main");
    let (status, _) = post(http, Some("wrong"), "branches", json!({}));
    assert_eq!(status, 401);

    // the admin token: the database's own user
    let (status, body) = post(http, Some("ADMIN-ADMIN-ADMIN-ADMIN-0"), "put", row);
    assert_eq!(status, 200, "{body}");
    let (status, body) =
        post(http, Some("ADMIN-ADMIN-ADMIN-ADMIN-0"), "merge", json!({"branch": "mine", "resolve": "ours"}));
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(http, Some("ADMIN-ADMIN-ADMIN-ADMIN-0"), "fork", json!({"name": "admins"}));
    assert_eq!(status, 200, "{body}");
    let (status, _) = post(http, None, "discard", json!({"branch": "admins"}));
    assert_eq!(status, 403, "not the guest's to drop");

    // Postgres: any other password is the guest
    let mut guest = pg(port, "anything");
    let e = guest.batch_execute("create table g (id int primary key)").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");
    guest.batch_execute("create world scratch").unwrap();
    let e = guest.batch_execute("merge world scratch").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");
    let e = guest.batch_execute("drop world admins").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");
    let e = guest.batch_execute("alter system set world_idle_ttl = '1 day'").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");
    guest.batch_execute("drop world scratch").unwrap(); // its own

    let mut admin = pg(port, "ADMIN-ADMIN-ADMIN-ADMIN-0");
    admin.batch_execute("create table g (id int primary key); insert into g values (1)").unwrap();
    admin.batch_execute("create world w2; alter system set world_idle_ttl = '1 day'").unwrap();
    admin.batch_execute("merge world w2").unwrap();
    admin.batch_execute("drop world admins").unwrap();
    let n: i64 = guest.query_one("select count(*) from g", &[]).unwrap().get(0);
    assert_eq!(n, 1, "the guest reads everything");
}

/// Starts a server with these arguments, in a folder of its own.
fn start_with(tag: &str, args: &[&str]) -> (Server, u16, u16) {
    let dir = std::env::temp_dir().join(format!("chronos-safe-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]).args(args);
    let s = common::serve(&mut cmd, true);
    (Server(s.child, dir), s.http, s.pg)
}

const SERVER: &str = "SERVER-SERVER-SERVER-SERVER-0";
const ADMIN: &str = "ADMIN-ADMIN-ADMIN-ADMIN-0";

/// Since 0.4, without `--safe` or `--unsafe`: the server token lets a client in as the guest, the admin token is the
/// database's own user, and with no token at all (loopback only) every client is the database's own user, as before.
/// `--unsafe` makes the server token the database's own user, as every server ran before 0.4.
#[test]
fn the_server_token_is_the_guest_by_default() {
    // a server token alone: it is the guest's, over HTTP and Postgres; nothing is the database's own user
    let (_s, http, port) = start_with("default", &["--token", SERVER]);
    let row = json!({"table": "t", "id": "1", "record": {"v": 1}});
    assert_eq!(post(http, None, "branches", json!({})).0, 401, "a server with a token wants one");
    let (status, _) = post(http, Some(SERVER), "put", row.clone());
    assert_eq!(status, 403, "the server token doesn't write main");
    let (status, body) = post(http, Some(SERVER), "fork", json!({"name": "mine"}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["world"]["owner"], "guest");
    let (status, _) =
        post(http, Some(SERVER), "put", json!({"table": "t", "id": "1", "record": {"v": 2}, "branch": "mine"}));
    assert_eq!(status, 200, "its own world");
    assert_eq!(post(http, Some(SERVER), "merge", json!({"branch": "mine"})).0, 403, "no merges");
    assert_eq!(post(http, Some(SERVER), "sql", json!({"sql": "create agent bot"})).0, 403, "no agents");
    assert_eq!(post(http, Some(ADMIN), "branches", json!({})).0, 401, "no admin token was configured");
    let mut guest = pg(port, SERVER);
    let e = guest.batch_execute("create table g (id int primary key)").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");
    guest.batch_execute("create world scratch").unwrap();
    let e = guest.batch_execute("merge world scratch").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");
    assert!(
        Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main password=wrong"), NoTls).is_err()
    );

    // both tokens: the admin token is the database's own user, the server token still the guest
    let (_s, http, port) = start_with("both", &["--token", SERVER, "--admin-token", ADMIN]);
    let (status, body) = post(http, Some(ADMIN), "put", row.clone());
    assert_eq!(status, 200, "{body}");
    assert_eq!(post(http, Some(SERVER), "put", row.clone()).0, 403);
    let mut admin = pg(port, ADMIN);
    admin.batch_execute("create table g (id int primary key); insert into g values (1)").unwrap();
    let mut guest = pg(port, SERVER);
    let e = guest.batch_execute("insert into g values (2)").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");
    assert_eq!(guest.query_one("select count(*) from g", &[]).unwrap().get::<_, i64>(0), 1, "the guest reads");
    // a role that logs in with its own password is that role, with its privileges, never the guest
    admin.batch_execute("create role r_app login password 'app-pw'; grant insert, select on g to r_app").unwrap();
    let mut app =
        Client::connect(&format!("host=127.0.0.1 port={port} user=r_app dbname=main password=app-pw"), NoTls).unwrap();
    app.batch_execute("insert into g values (2)").unwrap();
    assert_eq!(app.query_one("select current_user::text", &[]).unwrap().get::<_, String>(0), "r_app");
    let e = app.batch_execute("create table h (id int)").unwrap_err(); // its privileges, not the guest's: 42501 as a role's
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");
    let e = app.batch_execute("insert into g values (2)").unwrap_err(); // (a duplicate: the insert above ran as r_app)
    assert_eq!(e.code().map(|c| c.code()), Some("23505"), "{e}");
    assert!(
        Client::connect(&format!("host=127.0.0.1 port={port} user=r_app dbname=main password=wrong"), NoTls).is_err()
    );

    // --unsafe: the server token is the database's own user, as before 0.4
    let (_s, http, port) = start_with("unsafe", &["--token", SERVER, "--unsafe"]);
    let (status, body) = post(http, Some(SERVER), "put", row);
    assert_eq!(status, 200, "{body}");
    let mut owner = pg(port, SERVER);
    owner.batch_execute("create table g (id int primary key); create world w; merge world w").unwrap();

    // no token at all (loopback): every client is the database's own user, as before
    let (_s, http, port) = start_with("none", &[]);
    assert_eq!(post(http, None, "put", json!({"table": "t", "id": "1", "record": {}})).0, 200);
    let mut owner = Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main"), NoTls).unwrap();
    owner.batch_execute("create table g (id int primary key)").unwrap();
}
