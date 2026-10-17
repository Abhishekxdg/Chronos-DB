//! The Studio screens of 0.2.0: Sessions (the Postgres port's connections, who waits for whom, cancel and
//! terminate), Import from Postgres (a job the page polls: success, a dry run, a new world for a database that has
//! tables, schemas and tables to include, a refused plaintext password, cancel, and a password never echoed),
//! history retention, and agents made before SCRAM verifiers. pg_blocking_pids() and pg_stat_activity's waits
//! are what Sessions reads. And `chronos studio --login`: signing in with a database role (every wrong sign-in
//! refused alike, then waits), the session cookie, the role's own privileges, what only a superuser role may do,
//! sessions ending with their role, Host and Origin, beside a running serve, and over HTTPS.
//!
//! The imports from a real Postgres need CHRONOS_TEST_PG (a URL with a password, as tests/pg_import.rs); they are
//! skipped without it.

mod common;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Studio {
    child: Child,
    port: u16,
    token: String,
    /// what it printed (a password must never be in it)
    log: Arc<Mutex<String>>,
}

impl Drop for Studio {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-studio-screens-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("db")
}

fn studio(db: &Path) -> Studio {
    let child = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["studio", db.to_str().unwrap(), "--no-open", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let log = Arc::new(Mutex::new(String::new()));
    let mut s = Studio { child, port: 0, token: String::new(), log: log.clone() };
    let mut out = s.child.stdout.take().unwrap();
    let out_log = log.clone();
    std::thread::spawn(move || {
        let mut b = String::new();
        let _ = out.read_to_string(&mut b);
        out_log.lock().unwrap().push_str(&b);
    });
    let mut err = BufReader::new(s.child.stderr.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(err.read_line(&mut line).unwrap_or(0) > 0, "studio didn't start:\n{}", log.lock().unwrap());
        log.lock().unwrap().push_str(&line);
        if let Some(url) = line.trim().strip_prefix("chronos: studio on http://127.0.0.1:") {
            let (port, token) = url.split_once("/#t=").unwrap();
            (s.port, s.token) = (port.parse().unwrap(), token.to_string());
            std::thread::spawn(move || {
                let mut rest = String::new();
                let _ = err.read_to_string(&mut rest);
                log.lock().unwrap().push_str(&rest);
            });
            return s;
        }
    }
}

/// A raw request: (status, body).
fn raw(port: u16, req: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = vec![];
    s.read_to_end(&mut out).unwrap();
    let out = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    (head.split(' ').nth(1).unwrap_or("0").parse().unwrap_or(0), body.to_string())
}

impl Studio {
    /// A call as the page makes it; (status, the reply as text, the reply).
    fn call_text(&self, op: &str, args: &Value) -> (u16, String, Value) {
        let (p, body) = (self.port, args.to_string());
        let req = format!(
            "POST /studio/api/{op} HTTP/1.1\r\nHost: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\nX-Studio-Token: {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.token,
            body.len()
        );
        let (status, text) = raw(p, &req);
        let v = serde_json::from_str(&text).unwrap_or(Value::Null);
        (status, text, v)
    }

    fn call(&self, op: &str, args: Value) -> (u16, Value) {
        let (status, _, v) = self.call_text(op, &args);
        (status, v)
    }

    fn ok(&self, op: &str, args: Value) -> Value {
        let (status, v) = self.call(op, args.clone());
        assert_eq!(status, 200, "{op} {args}: {v}");
        v
    }

    /// The first statement's rows.
    fn rows(&self, world: &str, q: &str) -> Vec<Value> {
        let v = self.ok("sql", json!({ "branch": world, "sql": q }));
        v["results"][0]["rows"].as_array().cloned().unwrap_or_default()
    }

    /// Polls an import until it ends; its last status. `secret` must never be in any reply.
    fn finish(&self, job: &str, secret: &str) -> Value {
        let end = Instant::now() + Duration::from_secs(300);
        loop {
            let (status, text, v) = self.call_text("import_status", &json!({ "job": job }));
            assert_eq!(status, 200, "{text}");
            assert!(secret.is_empty() || !text.contains(secret), "the password came back: {text}");
            if !["running", "canceling"].contains(&v["state"].as_str().unwrap_or("")) {
                return v;
            }
            assert!(Instant::now() < end, "the import didn't end: {v}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// POST /v1/<op> to `chronos serve`'s HTTP API, with `bearer` if any: (status, reply).
fn http(port: u16, op: &str, bearer: Option<&str>, body: &Value) -> (u16, Value) {
    let body = body.to_string();
    let auth = bearer.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let req = format!(
        "POST /v1/{op} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let (status, text) = raw(port, &req);
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn serve(db: &Path, extra: &[&str]) -> common::Serve {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", db.to_str().unwrap()]).args(extra);
    common::serve(&mut cmd, true)
}

fn pg(port: u16, user: &str) -> postgres::Client {
    postgres::Client::connect(&format!("host=127.0.0.1 port={port} user={user} dbname=main"), postgres::NoTls).unwrap()
}

/// A session waiting for a row another holds shows whose it is (pg_blocking_pids) and on which table, in Studio and
/// in pg_stat_activity; Studio's Cancel query stops the waiting statement (57014) and Terminate closes the holder's
/// connection, whose transaction rolls back.
#[test]
fn sessions_show_who_waits_for_whom_and_cancel_and_terminate() {
    let db = scratch("sessions");
    let mut srv = serve(&db, &[]);
    let s = studio(&db);
    let p = srv.pg;
    let mut a = postgres::Client::connect(
        &format!("host=127.0.0.1 port={p} user=alice dbname=main application_name=holder"),
        postgres::NoTls,
    )
    .unwrap();
    a.batch_execute("create table t (id int primary key, v int); insert into t values (1, 0)").unwrap();
    let a_pid: i32 = a.query_one("select pg_backend_pid()", &[]).unwrap().get(0);
    a.batch_execute("begin; update t set v = 1 where id = 1").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let mut b = pg(p, "bob");
        let pid: i32 = b.query_one("select pg_backend_pid()", &[]).unwrap().get(0);
        tx.send(pid).unwrap();
        b.batch_execute("update t set v = 2 where id = 1").map_err(|e| e.code().map(|c| c.code().to_string()))
    });
    let b_pid = rx.recv().unwrap();
    // Studio's view, once bob waits
    let end = Instant::now() + Duration::from_secs(30);
    let bob = loop {
        let v = s.ok("sessions", json!({}));
        let all = v["sessions"].as_array().unwrap().clone();
        if let Some(b) = all.iter().find(|x| x["pid"] == json!(b_pid) && x["blocking_pids"] == json!([a_pid])) {
            let alice = all.iter().find(|x| x["pid"] == json!(a_pid)).expect("alice is listed");
            assert_eq!(alice["state"], "idle in transaction", "{alice}");
            assert!(alice["xact_start"].as_i64().is_some(), "{alice}");
            assert_eq!(alice["application_name"], "holder");
            assert_eq!(alice["user"], "alice");
            assert_eq!(alice["blocking_pids"], json!([]));
            assert!(alice["wait_event"].is_null());
            break b.clone();
        }
        assert!(Instant::now() < end, "bob never showed as waiting for alice: {v}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(bob["state"], "active");
    assert_eq!(bob["wait_event_type"], "Lock");
    assert_eq!(bob["wait_event"], "transactionid");
    assert_eq!(bob["waiting_on"], "t");
    assert!(bob["query"].as_str().unwrap().contains("update t set v = 2"), "{bob}");
    // the same in SQL, for psql
    let mut c = pg(p, "carol");
    let r =
        c.query_one(&format!("select pg_blocking_pids({b_pid})::text, pg_blocking_pids({a_pid})::text"), &[]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), (format!("{{{a_pid}}}"), "{}".to_string()));
    let r = c
        .query_one(&format!("select wait_event_type, wait_event from pg_stat_activity where pid = {b_pid}"), &[])
        .unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("Lock".into(), "transactionid".into()));
    // Cancel query, as the page does it
    let ok = s.rows("main", &format!("select pg_cancel_backend({b_pid}) as ok"));
    assert_eq!(ok[0]["ok"], json!(true));
    assert_eq!(waiter.join().unwrap(), Err(Some("57014".to_string())), "the waiting update was canceled");
    // Terminate: the holder's connection closes, and its update rolls back
    let ok = s.rows("main", &format!("select pg_terminate_backend({a_pid}) as ok"));
    assert_eq!(ok[0]["ok"], json!(true));
    assert!(a.batch_execute("select 1").is_err(), "alice's connection is closed");
    let end = Instant::now() + Duration::from_secs(30);
    while s.ok("sessions", json!({}))["sessions"].as_array().unwrap().iter().any(|x| x["pid"] == json!(a_pid)) {
        assert!(Instant::now() < end, "alice is still listed");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(s.rows("main", "select v from t")[0]["v"], json!(0));
    let _ = srv.child.kill();
    let _ = srv.child.wait();
}

/// An agent may neither cancel nor terminate a connection (42501, over the HTTP API as over SQL), and the Studio's
/// own ops (sessions, the import) aren't the HTTP API's, for an agent or for the server's own user. A role that isn't
/// a superuser may not signal a superuser's connection (Postgres 16's rule).
#[test]
fn agents_and_roles_may_not_signal_what_they_should_not() {
    let db = scratch("signals");
    let mut srv = serve(&db, &[]);
    let s = studio(&db);
    let mut a = pg(srv.pg, "alice"); // no role: a superuser
    let a_pid: i32 = a.query_one("select pg_backend_pid()", &[]).unwrap().get(0);
    let token = s.ok("create_agent", json!({ "name": "k2bot", "can": ["read", "admin"] }))["token"]
        .as_str()
        .unwrap()
        .to_string();
    for f in ["pg_cancel_backend", "pg_terminate_backend"] {
        let (status, v) =
            http(srv.http, "sql", Some(&token), &json!({ "sql": format!("select {f}({a_pid})"), "branch": "main" }));
        assert_eq!(status, 403, "{v}");
        assert_eq!(v["error"]["code"], "42501", "{v}");
    }
    for op in ["sessions", "import_postgres", "import_status", "import_cancel"] {
        let (status, v) = http(srv.http, op, Some(&token), &json!({}));
        assert_eq!(status, 404, "{op} as an agent: {v}");
        let (status, v) = http(srv.http, op, None, &json!({}));
        assert_eq!(status, 404, "{op} as the server's user: {v}");
    }
    a.batch_execute("create role plain login").unwrap();
    let mut plain = pg(srv.pg, "plain");
    for (f, what) in [("pg_cancel_backend", "cancel query"), ("pg_terminate_backend", "terminate process")] {
        let e = plain.batch_execute(&format!("select {f}({a_pid})")).unwrap_err();
        let e = e.as_db_error().unwrap();
        assert_eq!((e.code().code(), e.message()), ("42501", format!("permission denied to {what}").as_str()));
    }
    assert!(a.batch_execute("select 1").is_ok(), "alice is still connected");
    let _ = srv.child.kill();
    let _ = srv.child.wait();
}

/// An agent made before agents had SCRAM verifiers is marked on the Agents page until its first HTTP login gives it
/// one; an agent made now has one.
#[test]
fn agents_from_before_verifiers_are_marked_until_their_http_login() {
    const TOKEN: &str = "chronos_c4c4f34372d6ef6d721d9bc4bd37657991229257da8aefa91c3a6e3803516734";
    let db = scratch("old-agent");
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/old-agent");
    for f in ["wal-0000000000000001", "history/0000000000000001-0", "pages/pack-00000000"] {
        let to = db.join(f);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(from.join(f), to).unwrap();
    }
    let mut srv = serve(&db, &[]);
    let s = studio(&db);
    s.ok("create_agent", json!({ "name": "newbot", "can": ["read"] }));
    let marks = || -> Vec<(String, Value)> {
        let v = s.ok("agents", json!({}));
        let mut m: Vec<_> = v["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (a["name"].as_str().unwrap().to_string(), a["needs_http_login"].clone()))
            .collect();
        m.sort_by(|x, y| x.0.cmp(&y.0));
        m
    };
    assert_eq!(marks(), [("newbot".into(), json!(false)), ("r3c_oldbot".into(), json!(true))]);
    let (status, v) = http(srv.http, "branches", Some(TOKEN), &json!({}));
    assert_eq!(status, 200, "{v}");
    assert_eq!(marks(), [("newbot".into(), json!(false)), ("r3c_oldbot".into(), json!(false))]);
    let _ = srv.child.kill();
    let _ = srv.child.wait();
}

/// History retention reads and changes through SQL, as Settings does it; SHOW DISK splits the disk.
#[test]
fn history_retention_shows_and_changes() {
    let db = scratch("retention");
    let s = studio(&db);
    assert_eq!(s.rows("main", "show history_retention")[0]["history_retention"], "01:00:00", "a new database: 1 hour");
    s.ok("sql", json!({ "branch": "main", "sql": "alter system set history_retention = '7 days'" }));
    assert_eq!(s.rows("main", "show history_retention")[0]["history_retention"], "7 days");
    let disk: Vec<String> =
        s.rows("main", "show disk").iter().map(|r| r["what"].as_str().unwrap().to_string()).collect();
    assert_eq!(disk, ["live", "history", "reclaimable", "log"]);
}

/// A server that asks for the password in the clear over a connection without TLS is refused unless the form allows
/// it, as `chronos import` without --allow-plaintext-password; the password is in no reply and nothing the Studio
/// prints. Chronos is the server here (with only an admin token, it asks other names for the password in the clear).
#[test]
fn imports_refuse_a_plaintext_password_unless_allowed() {
    let src = scratch("plain-src");
    let mut srv = serve(&src, &["--admin-token", "admin-admin-admin-admin-0"]);
    let s = studio(&scratch("plain-to"));
    let secret = "admin-admin-admin-admin-0";
    let form = |allow: bool| {
        json!({
            "host": "127.0.0.1", "port": srv.pg, "database": "main", "user": "tester", "password": secret,
            "sslmode": "prefer", "dry_run": true, "allow_plaintext_password": allow,
        })
    };
    let (status, text, v) = s.call_text("import_postgres", &form(false));
    assert_eq!(status, 200, "{text}");
    assert!(!text.contains(secret));
    let end = s.finish(v["job"].as_str().unwrap(), secret);
    assert_eq!(end["state"], "failed", "{end}");
    assert!(end["error"].as_str().unwrap().contains("over a connection that isn't TLS"), "{end}");
    let (_, _, v) = s.call_text("import_postgres", &form(true));
    let end = s.finish(v["job"].as_str().unwrap(), secret);
    assert!(!end["error"].as_str().unwrap_or("").contains("isn't TLS"), "{end}");
    // a bad form: refused before anything starts
    let (status, v) = s.call("import_postgres", json!({ "host": "/tmp", "user": "x" }));
    assert_eq!(status, 400, "{v}");
    let (status, v) = s.call("import_postgres", json!({ "host": "h", "user": "x", "sslmode": "sometimes" }));
    assert_eq!(status, 400, "{v}");
    let (status, v) = s.call("import_status", json!({ "job": "nope" }));
    assert_eq!(status, 404, "{v}");
    assert!(!s.log.lock().unwrap().contains(secret), "the password was printed");
    let _ = srv.child.kill();
    let _ = srv.child.wait();
}

/// CHRONOS_TEST_PG's parts, as the form takes them, and the URL of a throwaway database made on that server.
struct Source {
    admin: postgres::Client,
    name: String,
    form: Value,
    password: String,
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.admin.batch_execute(&format!("drop database if exists {} with (force)", self.name));
    }
}

fn source(tag: &str, setup: &str) -> Option<Source> {
    let Ok(admin_url) = std::env::var("CHRONOS_TEST_PG") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL with a password");
        return None;
    };
    let name = format!("chronos_studio_{tag}_{}", std::process::id());
    let mut admin = postgres::Client::connect(&admin_url, postgres::NoTls).unwrap();
    admin.batch_execute(&format!("drop database if exists {name}")).unwrap();
    admin.batch_execute(&format!("create database {name}")).unwrap();
    let (base, _) = admin_url.rsplit_once('/').expect("CHRONOS_TEST_PG needs a database in its path");
    let url = format!("{base}/{name}");
    let mut pg = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    pg.batch_execute(setup).unwrap();
    // postgres://user:password@host:port/db
    let rest = admin_url.split_once("://").unwrap().1;
    let (auth, hostpart) = rest.rsplit_once('@').expect("CHRONOS_TEST_PG needs user:password@");
    let (user, password) = auth.split_once(':').expect("CHRONOS_TEST_PG needs a password");
    let hostport = hostpart.split('/').next().unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "5432"));
    let form = json!({ "host": host, "port": port, "database": name, "user": user, "password": password, "sslmode": "prefer" });
    Some(Source { admin, name, form, password: password.to_string() })
}

fn with(form: &Value, extra: Value) -> Value {
    let mut f = form.clone();
    for (k, v) in extra.as_object().unwrap() {
        f[k] = v.clone();
    }
    f
}

const SETUP: &str = "create table lists (id serial primary key, name text not null unique);
     create table leads (id int primary key, list int references lists (id), who text);
     create view named as select id, name from lists;
     create schema app;
     create table app.orders (id int primary key, total numeric);
     insert into lists (name) values ('vip'), ('cold');
     insert into leads values (1, 1, 'ada'), (2, 2, 'bo'), (3, 1, E'tab\\there');
     insert into app.orders values (1, 12.5);
     create sequence spare;
     select setval('spare', 40);
     create function shout() returns trigger language plpgsql as $$ begin new.who := upper(new.who); return new; end $$;
     create trigger leads_shout before insert on leads for each row execute function shout();
     comment on table leads is 'from postgres';
     create function greet() returns text language sql as $$ select 'from postgres' $$;
     create function trimmed() returns trigger language plpgsql as $$ begin new.name := trim(new.name); return new; end $$;
     create trigger lists_trimmed before insert on lists for each row execute function trimmed()";

/// An import into an empty database goes into its world as it is; a dry run writes nothing; the per-table rows and
/// the report come back, never the password.
#[test]
fn imports_from_postgres_with_a_report_and_a_dry_run() {
    let Some(src) = source("basic", SETUP) else { return };
    let s = studio(&scratch("import-basic"));
    // a dry run: the report, nothing written
    let v = s.ok("import_postgres", with(&src.form, json!({ "dry_run": true })));
    assert_eq!((v["forked"].clone(), v["world"].clone()), (json!(false), json!("main")), "{v}");
    let end = s.finish(v["job"].as_str().unwrap(), &src.password);
    assert_eq!(end["state"], "done", "{end}");
    assert_eq!(end["report"]["imported"], 3, "{end}");
    assert_eq!(end["report"]["total"], 3);
    assert!(s.ok("tables", json!({}))["tables"].as_array().unwrap().is_empty(), "a dry run writes nothing");
    // the import itself
    let v = s.ok("import_postgres", src.form.clone());
    assert_eq!(v["forked"], json!(false), "an empty database: into main itself");
    let end = s.finish(v["job"].as_str().unwrap(), &src.password);
    assert_eq!(end["state"], "done", "{end}");
    let r = &end["report"];
    assert_eq!((r["imported"].clone(), r["total"].clone(), r["rows"].clone()), (json!(3), json!(3), json!(6)), "{end}");
    assert!(r["error"].is_null(), "{end}");
    let done: Vec<&str> = r["done"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    assert!(done.iter().any(|d| d.starts_with("view")), "{done:?}");
    assert!(done.iter().any(|d| d.starts_with("foreign key")), "{done:?}");
    let mut tables: Vec<(String, u64)> = end["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["name"].as_str().unwrap().to_string(), t["rows"].as_u64().unwrap()))
        .collect();
    tables.sort();
    assert_eq!(tables, [("\"app\".\"orders\"".into(), 1), ("\"leads\"".into(), 3), ("\"lists\"".into(), 2)]);
    assert_eq!(end["tables_done"], 3);
    assert_eq!(end["rows"], 6);
    assert_eq!(s.rows("main", "select who from leads where id = 3")[0]["who"], "tab\there");
    assert_eq!(s.rows("main", "select count(*) as n from named")[0]["n"], 2);
    s.ok("sql", json!({ "branch": "main", "sql": "insert into leads values (4, 1, 'cy')" }));
    assert_eq!(s.rows("main", "select who from leads where id = 4")[0]["who"], "CY", "its trigger came over");
    assert_eq!(s.rows("main", "select obj_description('leads'::regclass) as d")[0]["d"], "from postgres");
    assert!(!s.log.lock().unwrap().contains(&src.password), "the password was printed");
}

/// A database that has tables gets the import in a new world (forked from the one named), unless told otherwise;
/// a named new world is used as named; `include` brings only the schemas and tables it names.
#[test]
fn imports_into_a_new_world_when_the_database_has_tables() {
    let Some(src) = source("fork", SETUP) else { return };
    let s = studio(&scratch("import-fork"));
    s.ok(
        "sql",
        json!({ "branch": "main", "sql": "create table mine (id int primary key); insert into mine values (1)" }),
    );
    let v = s.ok("import_postgres", src.form.clone());
    assert_eq!(v["forked"], json!(true), "{v}");
    let world = v["world"].as_str().unwrap().to_string();
    assert!(world.starts_with(&format!("import_{}_", src.name)), "{world}");
    let end = s.finish(v["job"].as_str().unwrap(), &src.password);
    assert_eq!(end["state"], "done", "{end}");
    assert_eq!(end["world"], json!(world));
    assert_eq!(s.rows(&world, "select count(*) as n from leads")[0]["n"], 3);
    assert_eq!(s.rows(&world, "select count(*) as n from mine")[0]["n"], 1, "a fork of main: its tables too");
    let (status, _) = s.call("sql", json!({ "branch": "main", "sql": "select * from leads" }));
    assert_eq!(status, 404, "main is untouched until the world is merged");
    let changed = s.ok("diff", json!({ "branch": world }));
    assert!(changed.to_string().contains("leads"), "{changed}");
    // named to the second (with the year), and numbered past a world of that name: a second import never collides
    let at = &world[format!("import_{}_", src.name).len()..];
    assert!(at.len() >= 15 && at.as_bytes()[8] == b'_' && at[..8].bytes().all(|b| b.is_ascii_digit()), "{world}");
    let again = s.ok("import_postgres", src.form.clone());
    let second = again["world"].as_str().unwrap().to_string();
    assert_ne!(second, world);
    assert_eq!(s.finish(again["job"].as_str().unwrap(), &src.password)["state"], "done");
    let third = s.ok("import_postgres", src.form.clone());
    assert!(![world.as_str(), second.as_str()].contains(&third["world"].as_str().unwrap()), "{third}");
    assert_eq!(s.finish(third["job"].as_str().unwrap(), &src.password)["state"], "done");
    // a name of one's own, and only the schema app
    let v = s.ok("import_postgres", with(&src.form, json!({ "new_world": "just_app", "include": "app" })));
    assert_eq!((v["forked"].clone(), v["world"].clone()), (json!(true), json!("just_app")));
    let end = s.finish(v["job"].as_str().unwrap(), &src.password);
    assert_eq!(end["state"], "done", "{end}");
    assert_eq!((end["report"]["imported"].clone(), end["report"]["total"].clone()), (json!(1), json!(1)), "{end}");
    let tables: Vec<String> = s.ok("tables", json!({ "branch": "just_app" }))["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(tables, ["app.orders", "mine"]);
    // one table by name, beside main's own tables when asked: its own sequence comes along, no other
    s.ok(
        "sql",
        json!({ "branch": "main", "sql": "create sequence spare; select setval('spare', 7);
                 create table leads (id int primary key, list int, who text);
                 create function greet() returns text language sql as $$ select 'mine' $$" }),
    );
    let v = s.ok("import_postgres", with(&src.form, json!({ "include": ["public.lists"], "same_world": true })));
    assert_eq!((v["forked"].clone(), v["world"].clone()), (json!(false), json!("main")));
    let end = s.finish(v["job"].as_str().unwrap(), &src.password);
    assert_eq!(end["state"], "done", "{end}");
    assert_eq!(s.rows("main", "select count(*) as n from lists")[0]["n"], 2);
    assert_eq!(s.rows("main", "select nextval('lists_id_seq') as n")[0]["n"], 3, "set to where Postgres had it");
    assert_eq!(s.rows("main", "select last_value as n from spare")[0]["n"], 7, "a sequence not included is left alone");
    // leads wasn't included: main's own table of that name gets none of Postgres's rows, trigger or comment
    s.ok("sql", json!({ "branch": "main", "sql": "insert into leads values (9, 1, 'zed')" }));
    assert_eq!(s.rows("main", "select string_agg(who, ',') as w from leads")[0]["w"], "zed");
    assert_eq!(s.rows("main", "select obj_description('leads'::regclass) as d")[0]["d"], Value::Null);
    // a function only a table left out needs stays out, main's own of that name kept; lists' trigger's came
    assert_eq!(s.rows("main", "select greet() as g")[0]["g"], "mine");
    s.ok("sql", json!({ "branch": "main", "sql": "insert into lists (name) values ('  warm ')" }));
    assert_eq!(s.rows("main", "select name from lists order by id desc limit 1")[0]["name"], "warm");
    let (status, _) = s.call("sql", json!({ "branch": "main", "sql": "select * from named" }));
    assert_eq!(status, 404, "a view of the schema not named isn't brought along");
    let (status, v) = s.call("import_postgres", with(&src.form, json!({ "include": "a.b.c" })));
    assert_eq!(status, 400, "{v}");
}

/// `include` naming a view brings the functions its query calls, though their schema isn't included whole; a function
/// nothing included uses stays out.
#[test]
fn an_included_view_brings_the_functions_it_calls() {
    let Some(src) = source(
        "viewfn",
        "create table t (id int primary key);
         insert into t values (1), (2);
         create function dbl(int) returns int language sql immutable as $$ select $1 * 2 $$;
         create function unused() returns int language sql as $$ select 1 $$;
         create view tv as select id, dbl(id) as d from t",
    ) else {
        return;
    };
    let s = studio(&scratch("import-viewfn"));
    let v = s.ok("import_postgres", with(&src.form, json!({ "include": ["public.t", "public.tv"] })));
    let end = s.finish(v["job"].as_str().unwrap(), &src.password);
    assert_eq!(end["state"], "done", "{end}");
    let world = end["world"].as_str().unwrap().to_string();
    assert_eq!(s.rows(&world, "select sum(d) as s from tv")[0]["s"], 6, "the view and the function it calls came");
    let (status, _) = s.call("sql", json!({ "branch": world, "sql": "select unused()" }));
    assert_ne!(status, 200, "a function nothing included uses stays out");
}

/// A wrong password fails the job with the server's refusal, which never holds the password; Cancel stops an import
/// in the middle of a big table's COPY.
#[test]
fn imports_fail_on_a_wrong_password_and_cancel() {
    let Some(src) = source(
        "cancel",
        "create table big (id int primary key, pad text);
         insert into big select g, repeat('x', 40) from generate_series(1, 3000000) g",
    ) else {
        return;
    };
    let s = studio(&scratch("import-cancel"));
    let wrong = "not-the-password-k2-0451";
    let v = s.ok("import_postgres", with(&src.form, json!({ "password": wrong })));
    let end = s.finish(v["job"].as_str().unwrap(), wrong);
    assert_eq!(end["state"], "failed", "{end}");
    // (the server's message names the user: where the password is the user's name too, as CI's postgres/postgres
    // is, that name isn't the password showing)
    let shown = end.to_string().replace(&format!("user \\\"{}\\\"", src.form["user"].as_str().unwrap()), "user");
    assert!(!shown.contains(wrong) && !shown.contains(&src.password), "{end}");
    let v = s.ok("import_postgres", src.form.clone());
    let job = v["job"].as_str().unwrap().to_string();
    // once rows are coming, cancel
    let end = Instant::now() + Duration::from_secs(120);
    loop {
        let v = s.ok("import_status", json!({ "job": job }));
        if v["rows"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert_eq!(v["state"], "running", "it ended before it could be canceled: {v}");
        assert!(Instant::now() < end, "no rows came: {v}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (status, v) = s.call("import_postgres", src.form.clone());
    assert_eq!(status, 409, "one import at a time: {v}");
    s.ok("import_cancel", json!({ "job": job }));
    let end = s.finish(&job, &src.password);
    assert_eq!(end["state"], "canceled", "{end}");
    assert_eq!(end["error"], "the import was canceled");
    // the table is there, without the rows the canceled COPY was writing
    assert_eq!(s.rows("main", "select count(*) as n from big")[0]["n"], 0);
    assert!(!s.log.lock().unwrap().contains(&src.password));
}

// ---------------------------------------------------------------- chronos studio --login

const ADMIN_PW: &str = "admin-pw-0451";

/// Runs SQL in the folder with the one-shot shell, before anything holds it.
fn shell(db: &Path, sql: &str) {
    let o = Command::new(env!("CARGO_BIN_EXE_chronos")).arg(db).arg(sql).output().unwrap();
    assert!(o.status.success(), "{sql}: {}", String::from_utf8_lossy(&o.stderr));
}

/// A folder with a superuser role `admin` that signs in with `ADMIN_PW`.
fn with_admin(name: &str) -> PathBuf {
    let db = scratch(name);
    shell(&db, &format!("create role admin login superuser password '{ADMIN_PW}';"));
    db
}

/// `chronos studio <db> --login` with `extra` flags (127.0.0.1 and a free port unless --listen is among them),
/// once it listens; wrong sign-ins wait only after `free` of them (see `backoff`).
fn studio_login(db: &Path, extra: &[&str], free: u32) -> Studio {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["studio", db.to_str().unwrap(), "--login", "--no-open"]).args(extra);
    if !extra.contains(&"--listen") {
        cmd.args(["--port", "0"]);
    }
    cmd.env("CHRONOS_LOGIN_FREE_TRIES", free.to_string()).stdout(Stdio::null()).stderr(Stdio::piped());
    let child = cmd.spawn().unwrap();
    let log = Arc::new(Mutex::new(String::new()));
    let mut s = Studio { child, port: 0, token: String::new(), log: log.clone() };
    let mut err = BufReader::new(s.child.stderr.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(err.read_line(&mut line).unwrap_or(0) > 0, "studio didn't start:\n{}", log.lock().unwrap());
        log.lock().unwrap().push_str(&line);
        if let Some(url) = line.trim().strip_prefix("chronos: studio on ") {
            assert!(!url.contains("#t="), "no key in the link: {url}");
            let addr = url.split(' ').next().unwrap().trim_end_matches('/');
            s.port = addr.rsplit(':').next().unwrap().parse().unwrap();
            std::thread::spawn(move || {
                let mut rest = String::new();
                let _ = err.read_to_string(&mut rest);
                log.lock().unwrap().push_str(&rest);
            });
            return s;
        }
    }
}

/// A Studio API call with these header lines (Host among them; each ending in \r\n): (status, the response's
/// head, the reply).
fn post(port: u16, op: &str, headers: &str, args: &Value) -> (u16, String, Value) {
    let body = args.to_string();
    let req = format!(
        "POST /studio/api/{op} HTTP/1.1\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = vec![];
    s.read_to_end(&mut out).unwrap();
    let out = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let status = head.split(' ').nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, head.to_string(), serde_json::from_str(body).unwrap_or(Value::Null))
}

/// A browser on a `--login` Studio: the page's Host and Origin, and the session cookie it was given.
struct Browser {
    port: u16,
    cookie: Option<String>,
}

impl Browser {
    fn new(port: u16) -> Browser {
        Browser { port, cookie: None }
    }

    /// A call as the page makes it: (status, the response's head, the reply). A Set-Cookie replaces the cookie
    /// (an emptied one clears it).
    fn call_head(&mut self, op: &str, args: &Value) -> (u16, String, Value) {
        let p = self.port;
        // (beside a cookie of another page on the same name)
        let cookie = self.cookie.as_ref().map_or(String::new(), |c| format!("Cookie: other=1; chronos_studio={c}\r\n"));
        let r = post(p, op, &format!("Host: 127.0.0.1:{p}\r\nOrigin: http://127.0.0.1:{p}\r\n{cookie}"), args);
        if let Some(line) = r.1.lines().find(|l| l.to_ascii_lowercase().starts_with("set-cookie:")) {
            let pair = line.split_once(':').unwrap().1.trim().split(';').next().unwrap();
            let value = pair.strip_prefix("chronos_studio=").unwrap();
            self.cookie = (!value.is_empty()).then(|| value.to_string());
        }
        r
    }

    fn call(&mut self, op: &str, args: Value) -> (u16, Value) {
        let (status, _, v) = self.call_head(op, &args);
        (status, v)
    }

    fn ok(&mut self, op: &str, args: Value) -> Value {
        let (status, v) = self.call(op, args.clone());
        assert_eq!(status, 200, "{op} {args}: {v}");
        v
    }

    fn sign_in(&mut self, user: &str, password: &str) -> (u16, Value) {
        self.call("login", json!({ "user": user, "password": password }))
    }

    fn sql(&mut self, q: &str) -> Value {
        self.ok("sql", json!({ "sql": q }))
    }

    /// The first statement's rows.
    fn rows(&mut self, q: &str) -> Vec<Value> {
        self.sql(q)["results"][0]["rows"].as_array().cloned().unwrap_or_default()
    }

    /// A statement refused: (status, SQLSTATE).
    fn refused(&mut self, q: &str) -> (u16, String) {
        let (status, v) = self.call("sql", json!({ "sql": q }));
        (status, v["error"]["code"].as_str().unwrap_or_default().to_string())
    }
}

/// Every sign-in that doesn't prove a role's password gets one reply, whatever was wrong: the name, the password,
/// a role without one, one past its VALID UNTIL, an agent's name, the guest. With the right password, what Postgres
/// refuses after it (NOLOGIN, CONNECT) is told as Postgres words it. A role whose password is an md5 hash signs in.
#[test]
fn login_refuses_every_wrong_sign_in_alike() {
    let db = with_admin("login-refusals");
    let s = studio_login(&db, &[], 1_000_000);
    let mut admin = Browser::new(s.port);
    assert_eq!(admin.sign_in("admin", ADMIN_PW).0, 200);
    admin.sql(
        "create role alice login password 'alice-pw'; create role nopw login; \
         create role shut nologin password 'shut-pw'; \
         create role old login password 'old-pw' valid until '2000-01-01'; \
         create role bot login password 'bot-pw'; \
         set password_encryption = 'md5'; create role m5 login password 'm5-pw'",
    );
    admin.ok("create_agent", json!({ "name": "bot", "can": ["read"] }));
    let wrong = json!({ "error": "wrong role name or password" });
    for (user, password, why) in [
        ("alice", "nope", "a wrong password"),
        ("alice", "", "an empty password"),
        ("alice", "alice-pw ", "nearly"),
        ("nobody", "alice-pw", "no such role"),
        ("nopw", "x", "a role without a password"),
        ("nopw", "", "a role without a password, and none given"),
        ("old", "old-pw", "past its VALID UNTIL"),
        ("bot", "bot-pw", "an agent's name"),
        ("guest", "x", "the guest"),
        ("shut", "wrong", "NOLOGIN, with a wrong password"),
        ("", "x", "no name"),
        ("m5", "m5", "an md5 role, wrong"),
    ] {
        let mut b = Browser::new(s.port);
        let (status, v) = b.sign_in(user, password);
        assert_eq!((status, &v), (401, &wrong), "{why}");
        assert!(b.cookie.is_none(), "{why}");
    }
    let (status, v) = Browser::new(s.port).call("login", json!({ "user": "alice" }));
    assert_eq!(status, 400, "{v}");
    let mut m5 = Browser::new(s.port);
    assert_eq!(m5.sign_in("m5", "m5-pw").0, 200, "a role whose password is an md5 hash");
    assert_eq!(m5.rows("select current_user as u")[0]["u"], "m5");
    // the right password: refused after it, as Postgres refuses it
    let (status, v) = Browser::new(s.port).sign_in("shut", "shut-pw");
    assert_eq!(status, 403, "{v}");
    assert_eq!(v["error"], json!({ "message": "role \"shut\" is not permitted to log in", "code": "28000" }));
    admin.sql("revoke connect on database main from public");
    let (status, v) = Browser::new(s.port).sign_in("alice", "alice-pw");
    assert_eq!(status, 403, "{v}");
    assert_eq!(v["error"], json!({ "message": "permission denied for database \"main\"", "code": "42501" }));
    admin.sql("grant connect on database main to alice");
    assert_eq!(Browser::new(s.port).sign_in("alice", "alice-pw").0, 200);
    assert!(!s.log.lock().unwrap().contains("alice-pw"), "no password printed");
}

/// After the free tries, an address that keeps failing waits before its next sign-in, told at once (429), even
/// with the right password; once the wait is over it signs in.
#[test]
fn login_failures_wait_after_the_free_tries() {
    let db = with_admin("login-backoff");
    let s = studio_login(&db, &[], 3);
    let mut b = Browser::new(s.port);
    for i in 0..4 {
        assert_eq!(b.sign_in("admin", "wrong").0, 401, "try {i}");
    }
    for (user, password) in [("admin", ADMIN_PW), ("nobody", "x")] {
        let (status, v) = b.sign_in(user, password);
        assert_eq!(status, 429, "{v}");
        assert!(v["error"].as_str().unwrap().starts_with("too many failed sign-ins from this address"), "{v}");
    }
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(b.sign_in("admin", ADMIN_PW).0, 200);
}

/// A signed-in role gets an HttpOnly, SameSite=Strict cookie; calls without it, or with one nobody was given, are
/// refused (401). Its SQL runs as the role: GRANTs and row-level security apply; `tables` counts rows as it. Beyond
/// status, worlds, tables and SQL it needs a superuser role (403), which may do all the owner may; a superuser made
/// an ordinary role loses that at its next call. Signing out, NOLOGIN, and dropping the role (even if a role of the
/// same name is made again) end the session.
#[test]
fn a_signed_in_role_acts_with_its_own_privileges() {
    let db = with_admin("login-roles");
    let s = studio_login(&db, &[], 1_000_000);
    let mut admin = Browser::new(s.port);
    let (status, head, v) = admin.call_head("login", &json!({ "user": "admin", "password": ADMIN_PW }));
    assert_eq!(status, 200, "{v}");
    assert_eq!(v, json!({ "mode": "login", "user": { "name": "admin", "superuser": true } }));
    let id = admin.cookie.clone().unwrap();
    assert!(id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
    let set = head.lines().find(|l| l.starts_with("Set-Cookie:")).unwrap();
    assert_eq!(
        set,
        format!("Set-Cookie: chronos_studio={id}; HttpOnly; SameSite=Strict; Path=/"),
        "not Secure over HTTP"
    );
    assert_eq!(
        admin.ok("whoami", json!({})),
        json!({ "mode": "login", "user": { "name": "admin", "superuser": true } })
    );
    // without a session, or with one nobody was given
    let mut anon = Browser::new(s.port);
    assert_eq!(anon.ok("whoami", json!({})), json!({ "mode": "login", "user": null }));
    let other = format!("{}{}", &id[..63], if id.ends_with('0') { '1' } else { '0' });
    for cookie in [None, Some("0".repeat(64)), Some("not-a-session".into()), Some(other)] {
        for op in ["sql", "tables", "status", "worlds", "fork", "agents"] {
            anon.cookie = cookie.clone();
            let (status, v) = anon.call(op, json!({ "sql": "select 1", "branch": "main" }));
            assert_eq!(status, 401, "{op} with {cookie:?}: {v}");
        }
    }

    admin.sql(
        "create role alice login password 'alice-pw'; \
         create table t (id int primary key, v text); insert into t values (1, 'one'), (2, 'two'); \
         create table u (id int primary key); insert into u values (1); \
         create table notes (id int primary key, owner text); \
         insert into notes values (1, 'alice'), (2, 'bob'), (3, 'alice'); \
         alter table notes enable row level security; \
         create policy mine on notes using (owner = current_user); grant select on notes to alice",
    );
    admin.ok("fork", json!({ "from": "main", "name": "w1" }));
    admin.ok("put", json!({ "branch": "w1", "table": "j", "id": "9", "record": { "v": "nine" } }));
    let mut alice = Browser::new(s.port);
    assert_eq!(alice.sign_in("alice", "alice-pw").0, 200);
    assert_eq!(alice.ok("whoami", json!({}))["user"], json!({ "name": "alice", "superuser": false }));
    assert_eq!(alice.rows("select current_user as u, session_user as s")[0], json!({ "u": "alice", "s": "alice" }));
    // GRANTs, as over the Postgres port
    assert_eq!(alice.refused("select * from t"), (403, "42501".into()));
    admin.sql("grant select on t to alice");
    assert_eq!(alice.rows("select count(*) as n from t")[0]["n"], 2);
    assert_eq!(alice.refused("insert into t values (3, 'three')"), (403, "42501".into()));
    admin.sql("grant insert on t to alice");
    alice.sql("insert into t values (3, 'three')");
    // row-level security
    let ids: Vec<Value> = alice.rows("select id from notes order by id").iter().map(|r| r["id"].clone()).collect();
    assert_eq!(ids, [json!(1), json!(3)]);
    // tables: rows counted as alice, null where she may not read
    let v = alice.ok("tables", json!({ "branch": "main" }));
    let count = |t: &str| v["tables"].as_array().unwrap().iter().find(|x| x["name"] == t).unwrap()["rows"].clone();
    assert_eq!((count("t"), count("u"), count("notes")), (json!(3), Value::Null, json!(2)));
    assert_eq!(alice.ok("status", json!({}))["folder"], "db");
    // the worlds, without how much each changed (DIFF needs a superuser)
    let w1 = |v: Value| v["branches"].as_array().unwrap().iter().find(|w| w["name"] == "w1").unwrap().clone();
    assert!(w1(alice.ok("worlds", json!({}))).get("changes").is_none());
    assert_eq!(w1(admin.ok("worlds", json!({})))["changes"], 1);
    // everything else needs a superuser role
    for (op, args) in [
        ("fork", json!({ "from": "main", "name": "w2" })),
        ("put", json!({ "table": "t", "id": "8", "record": { "v": "x" } })),
        ("get", json!({ "table": "t", "id": "1" })),
        ("find", json!({ "table": "t" })),
        ("delete", json!({ "table": "t", "id": "1" })),
        ("batch", json!({ "rows": [] })),
        ("history", json!({ "branch": "main" })),
        ("diff", json!({ "branch": "w1" })),
        ("merge", json!({ "branch": "w1", "dry_run": true })),
        ("discard", json!({ "branch": "w1" })),
        ("restore", json!({ "branch": "w1", "at": "-1 minute" })),
        ("checkpoint", json!({ "branch": "main", "name": "c1" })),
        ("set_meta", json!({ "branch": "w1", "meta": {} })),
        ("agents", json!({})),
        ("create_agent", json!({ "name": "x", "can": ["read"] })),
        ("audit", json!({})),
        ("sessions", json!({})),
        ("import_status", json!({ "job": "x" })),
        ("verify", json!({})),
        ("simulate", json!({})),
        ("backup", json!({})),
    ] {
        let (status, v) = alice.call(op, args);
        assert_eq!((status, v["error"].as_str()), (403, Some(format!("{op} needs a superuser role").as_str())), "{op}");
    }
    // the superuser role does what the owner does
    admin.ok("history", json!({ "branch": "main" }));
    admin.ok("agents", json!({}));
    admin.ok("diff", json!({ "branch": "w1" }));
    admin.ok("sessions", json!({}));
    admin.ok("get", json!({ "branch": "w1", "table": "j", "id": "9" }));
    // superuser no more: told at its next call
    admin.sql("create role boss login superuser password 'boss-pw'");
    let mut boss = Browser::new(s.port);
    assert_eq!(boss.sign_in("boss", "boss-pw").1["user"]["superuser"], true);
    boss.ok("agents", json!({}));
    admin.sql("alter role boss nosuperuser");
    assert_eq!(boss.call("agents", json!({})).0, 403);
    assert_eq!(boss.ok("whoami", json!({}))["user"], json!({ "name": "boss", "superuser": false }));

    // signing out clears the cookie, and the session it named is gone
    let kept = alice.cookie.clone();
    let (status, head, _) = alice.call_head("logout", &json!({}));
    assert_eq!(status, 200);
    assert!(head.contains("Set-Cookie: chronos_studio=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"), "{head}");
    assert!(alice.cookie.is_none());
    alice.cookie = kept;
    assert_eq!(alice.call("sql", json!({ "sql": "select 1" })).0, 401);
    // NOLOGIN ends a session at its next call, and LOGIN again doesn't bring it back
    alice.cookie = None;
    assert_eq!(alice.sign_in("alice", "alice-pw").0, 200);
    let kept = alice.cookie.clone();
    admin.sql("alter role alice nologin");
    let (status, v) = alice.call("tables", json!({}));
    assert_eq!(status, 401, "{v}");
    assert!(v["error"].as_str().unwrap().contains("sign in again"), "{v}");
    assert!(alice.cookie.is_none(), "the cookie is cleared");
    admin.sql("alter role alice login");
    alice.cookie = kept;
    assert_eq!(alice.call("sql", json!({ "sql": "select 1" })).0, 401);
    assert_eq!(alice.ok("whoami", json!({}))["user"], Value::Null);
    // dropped; and dropped and made again under its name: another role, not this session's
    for again in [false, true] {
        admin.sql("create role carol login password 'carol-pw'");
        let mut carol = Browser::new(s.port);
        assert_eq!(carol.sign_in("carol", "carol-pw").0, 200);
        carol.sql("select 1");
        admin.sql("drop role carol");
        if again {
            admin.sql("create role carol login password 'carol-pw'");
        }
        assert_eq!(carol.call("sql", json!({ "sql": "select 1" })).0, 401, "made again: {again}");
        admin.sql("drop role if exists carol");
    }
}

/// --login keeps the Host and Origin checks: the Host must be one of its names (--host, the address listened on),
/// the Origin, if any, that same name; a cookie alone gets nothing from another page. Bodies must be JSON.
#[test]
fn login_mode_keeps_the_host_and_origin_checks() {
    let db = with_admin("login-hosts");
    let s = studio_login(&db, &["--host", "studio.example"], 1_000_000);
    let p = s.port;
    let body = json!({ "user": "admin", "password": ADMIN_PW });
    let local = format!("Host: 127.0.0.1:{p}\r\n");
    for (headers, why) in [
        (format!("Host: evil.example:{p}\r\n"), "another Host (DNS rebinding)"),
        (format!("Host: localhost:{p}\r\n"), "a Host other than its names"),
        (format!("Host: studio.example:{}\r\n", p.wrapping_add(1)), "another port"),
        (String::new(), "no Host"),
        (format!("{local}Origin: http://evil.example\r\n"), "another page"),
        (format!("{local}Origin: http://studio.example:{p}\r\n"), "an Origin other than the Host"),
        (format!("{local}Origin: https://127.0.0.1:{p}\r\n"), "https, served over http"),
        (format!("{local}Origin: null\r\n"), "a sandboxed page"),
    ] {
        let (status, _, v) = post(p, "login", &headers, &body);
        assert_eq!(status, 403, "{why}: {v}");
    }
    // its --host name (with the port listened on), in any case, with its own Origin
    let named = format!("Host: Studio.Example:{p}\r\nOrigin: http://studio.example:{p}\r\n");
    let (status, head, v) = post(p, "login", &named, &body);
    assert_eq!(status, 200, "{v}");
    let cookie =
        head.lines().find_map(|l| l.strip_prefix("Set-Cookie: ")).unwrap().split(';').next().unwrap().to_string();
    let (status, _, v) = post(p, "whoami", &format!("{named}Cookie: {cookie}\r\n"), &json!({}));
    assert_eq!((status, v["user"]["name"].as_str()), (200, Some("admin")));
    // the cookie from another page: refused before it is looked at
    let (status, _, _) =
        post(p, "agents", &format!("{local}Origin: http://evil.example\r\nCookie: {cookie}\r\n"), &json!({}));
    assert_eq!(status, 403);
    // a body that isn't JSON
    let form = "user=admin&password=x";
    let (status, _) = raw(
        p,
        &format!(
            "POST /studio/api/login HTTP/1.1\r\n{local}Content-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{form}",
            form.len()
        ),
    );
    assert_eq!(status, 415);
    // the page: under its names only
    let get = |host: &str| raw(p, &format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")).0;
    assert_eq!(get(&format!("studio.example:{p}")), 200);
    assert_eq!(get(&format!("127.0.0.1:{p}")), 200);
    assert_eq!(get(&format!("evil.example:{p}")), 403);
}

/// Beside a running `chronos serve`, sign-ins and calls go to the server, which checks the role on each: its
/// privileges apply, and a role made NOLOGIN there loses its session.
#[test]
fn login_beside_a_running_serve_checks_roles_in_the_server() {
    let db = scratch("login-beside");
    let mut srv = serve(&db, &[]);
    let mut owner = pg(srv.pg, "postgres"); // no role: a superuser
    owner
        .batch_execute(&format!(
            "create role admin login superuser password '{ADMIN_PW}'; create role dora login password 'dora-pw'; \
             create table t (id int primary key); insert into t values (1)"
        ))
        .unwrap();
    let s = studio_login(&db, &[], 1_000_000);
    let mut dora = Browser::new(s.port);
    assert_eq!(dora.sign_in("dora", "wrong"), (401, json!({ "error": "wrong role name or password" })));
    assert_eq!(dora.sign_in("nobody", "dora-pw"), (401, json!({ "error": "wrong role name or password" })));
    assert_eq!(dora.sign_in("dora", "dora-pw").0, 200);
    assert_eq!(dora.refused("select * from t"), (403, "42501".into()));
    owner.batch_execute("grant select on t to dora").unwrap();
    assert_eq!(dora.rows("select count(*) as n from t")[0]["n"], 1);
    assert_eq!(dora.call("agents", json!({})).0, 403);
    let mut admin = Browser::new(s.port);
    assert_eq!(admin.sign_in("admin", ADMIN_PW).0, 200);
    admin.ok("agents", json!({}));
    owner.batch_execute("alter role dora nologin").unwrap();
    assert_eq!(dora.call("sql", json!({ "sql": "select 1" })).0, 401);
    drop(s);
    let _ = srv.child.kill();
    let _ = srv.child.wait();
}

/// The key in the link stays the default, on 127.0.0.1 only: --listen, --host and TLS need --login. The page may
/// ask which sign-in it is without the key, and learns nothing else. --login anywhere but loopback needs TLS unless
/// --insecure-no-tls (then a warning).
#[test]
fn the_key_stays_the_default_and_login_needs_tls_off_loopback() {
    let db = with_admin("login-cli");
    let s = studio(&db);
    let local = format!("Host: 127.0.0.1:{}\r\n", s.port);
    let (status, _, v) = post(s.port, "whoami", &local, &json!({}));
    assert_eq!((status, v), (200, json!({ "mode": "key", "user": null })));
    let (status, _, _) = post(s.port, "login", &local, &json!({ "user": "admin", "password": ADMIN_PW }));
    assert_eq!(status, 403, "no key: refused as before");
    let (status, v) = s.call("login", json!({ "user": "admin", "password": ADMIN_PW }));
    assert_ne!(status, 200, "the key's Studio signs no one in: {v}");
    drop(s);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_chronos")).arg("studio").arg(&db).args(args).arg("--no-open").output().unwrap()
    };
    for args in [
        &["--listen", "127.0.0.1:0"][..],
        &["--host", "studio.example"],
        &["--tls-cert", "c.pem", "--tls-key", "k.pem"],
    ] {
        let o = run(args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains("need --login"), "{args:?}");
    }
    let o = run(&["--login", "--listen", "0.0.0.0:0"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("without TLS"), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(run(&["--login", "--listen", "127.0.0.1:0", "--port", "7"]).status.code(), Some(2));
    let s = studio_login(&db, &["--listen", "0.0.0.0:0", "--insecure-no-tls", "--host", "127.0.0.1"], 1_000_000);
    assert!(s.log.lock().unwrap().contains("WARNING: listening on 0.0.0.0:0 WITHOUT TLS"), "{}", s.log.lock().unwrap());
    let mut b = Browser::new(s.port);
    assert_eq!(b.sign_in("admin", ADMIN_PW).0, 200);
}

/// Over HTTPS (--tls-cert and --tls-key), the session cookie is Secure and HSTS is sent; plain HTTP gets nothing.
#[test]
fn login_over_https_marks_its_cookie_secure() {
    let db = with_admin("login-tls");
    let dir = db.parent().unwrap().to_path_buf();
    let made = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-days", "1"])
        .args(["-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"])
        .arg("-keyout")
        .arg(dir.join("key.pem"))
        .arg("-out")
        .arg(dir.join("cert.pem"))
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(made, "openssl could not make a certificate");
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    let s = studio_login(
        &db,
        &["--tls-cert", cert.to_str().unwrap(), "--tls-key", key.to_str().unwrap(), "--host", "localhost"],
        1_000_000,
    );
    assert!(s.log.lock().unwrap().contains(&format!("studio on https://localhost:{}/", s.port)));
    let p = s.port;
    let curl = |scheme: &str| {
        let out = Command::new("curl")
            .args(["-s", "-i", "--cacert"])
            .arg(&cert)
            .args(["-H", "Content-Type: application/json", "-H", &format!("Origin: https://localhost:{p}")])
            .args(["-d", &json!({ "user": "admin", "password": ADMIN_PW }).to_string()])
            .arg(format!("{scheme}://localhost:{p}/studio/api/login"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let out = curl("https");
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    let set = out.lines().find(|l| l.starts_with("Set-Cookie:")).unwrap();
    assert!(set.ends_with("; HttpOnly; SameSite=Strict; Path=/; Secure"), "{set}");
    assert!(out.contains("Strict-Transport-Security: max-age=31536000"), "{out}");
    assert!(!curl("http").starts_with("HTTP/1.1 200"), "no plain HTTP");
}
