//! `chronos serve` over the Postgres protocol: a real Postgres driver (the `postgres` crate, which
//! uses the extended protocol with binary values) and psql (simple protocol, text), if installed.

mod common;

use postgres::error::SqlState;
use postgres::{Client, NoTls};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

/// Starts a server; returns it with its Postgres port.
fn start(name: &str, token: Option<&str>) -> (Server, u16) {
    let dir = std::env::temp_dir().join(format!("chronos-pg-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    if let Some(t) = token {
        cmd.args(["--token", t]);
    }
    let s = common::serve(&mut cmd, true);
    (Server(s.child, dir), s.pg)
}

fn connect(port: u16, extra: &str) -> Result<Client, postgres::Error> {
    Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=main {extra}"), NoTls)
}

#[test]
fn postgres_driver() {
    let (_server, port) = start("driver", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute(
        "create table users (id bigint primary key, name text not null, age integer, score double precision, admin boolean);
         insert into users values (1, 'Ada', 36, 9.5, true), (2, 'Alan', 41, null, false);",
    )
    .unwrap();

    // parameters and results in binary, typed both ways
    let n = c.execute("insert into users (id, name, age) values ($1, $2, $3)", &[&3i64, &"Grace", &85i32]).unwrap();
    assert_eq!(n, 1);
    let rows =
        c.query("select id, name, age, score, admin from users where age > $1 order by age desc", &[&40i32]).unwrap();
    type User = (i64, String, i32, Option<f64>, Option<bool>);
    let got: Vec<User> = rows.iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4))).collect();
    assert_eq!(got, [(3, "Grace".into(), 85, None, None), (2, "Alan".into(), 41, None, Some(false))]);

    // a prepared statement, run several times
    let by_id = c.prepare("select name, score from users where id = $1").unwrap();
    let r = c.query_one(&by_id, &[&1i64]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, f64>(1)), ("Ada".into(), 9.5));
    assert!(c.query_opt(&by_id, &[&99i64]).unwrap().is_none());

    let r = c.query_one("update users set age = age + 1 where id = $1 returning age", &[&1i64]).unwrap();
    assert_eq!(r.get::<_, i32>(0), 37);
    assert_eq!(c.execute("delete from users where admin = $1", &[&false]).unwrap(), 1);

    // errors carry Postgres codes, and the connection keeps working after one
    let e = c.execute("insert into users (id, name) values (1, 'dup')", &[]).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::UNIQUE_VIOLATION));
    let e = c.query("select * from nowhere", &[]).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::UNDEFINED_TABLE));
    let e = c.batch_execute("selec 1").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::SYNTAX_ERROR));
    let r = c.query_one("select count_me from (select 1)", &[]);
    assert!(r.is_err());
    assert_eq!(c.query("select id from users order by id", &[]).unwrap().len(), 2);

    // other connections see committed writes at once
    let mut other = connect(port, "").unwrap();
    assert_eq!(other.query_one("select name from users where id = 3", &[]).unwrap().get::<_, String>(0), "Grace");

    // the database name picks the branch
    let e = connect(port, "dbname=nope").map(|_| ()).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INVALID_CATALOG_NAME));
}

#[test]
fn transactions_and_branches_over_the_wire() {
    let (_server, port) = start("tx", None);
    let (mut a, mut b) = (connect(port, "").unwrap(), connect(port, "").unwrap());
    a.batch_execute("create table kv (k text primary key, v bigint)").unwrap();

    // the driver's transaction API sends BEGIN / COMMIT / ROLLBACK
    let mut t = a.transaction().unwrap();
    t.execute("insert into kv values ($1, $2)", &[&"x", &1i64]).unwrap();
    assert!(b.query_opt("select v from kv where k = 'x'", &[]).unwrap().is_none());
    t.commit().unwrap();
    assert_eq!(b.query_one("select v from kv where k = 'x'", &[]).unwrap().get::<_, i64>(0), 1);
    let mut t = a.transaction().unwrap();
    t.execute("delete from kv", &[]).unwrap();
    drop(t); // rolls back
    assert_eq!(b.query("select k from kv", &[]).unwrap().len(), 1);

    // conflicting writes at REPEATABLE READ: the loser gets a serialization failure at its write, as drivers expect
    let mut ta = a.transaction().unwrap();
    b.batch_execute("begin isolation level repeatable read; select 1").unwrap();
    ta.execute("update kv set v = v + 1 where k = 'x'", &[]).unwrap();
    ta.commit().unwrap();
    let e = b.batch_execute("update kv set v = v + 10 where k = 'x'").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::T_R_SERIALIZATION_FAILURE));
    b.batch_execute("rollback").unwrap();

    // subqueries and UNION with typed parameters inside them
    let r = a
        .query(
            "select k from kv where v in (select v from kv where v > $1) union select 'none' where $2",
            &[&0i64, &false],
        )
        .unwrap();
    assert_eq!(r.len(), 1);

    // a branch is a database: connect to it by name
    a.batch_execute("create branch trial; use branch trial; update kv set v = 99").unwrap();
    let mut on_branch = connect(port, "dbname=trial").unwrap();
    assert_eq!(on_branch.query_one("select v from kv", &[]).unwrap().get::<_, i64>(0), 99);
    assert_eq!(b.query_one("select v from kv", &[]).unwrap().get::<_, i64>(0), 2);
    let d = a.query("diff", &[]).unwrap();
    assert_eq!((d.len(), d[0].get::<_, String>("change")), (1, "update".into()));
    a.batch_execute("merge branch trial").unwrap();
    assert_eq!(b.query_one("select v from kv", &[]).unwrap().get::<_, i64>(0), 99);
}

#[test]
fn dates_over_the_wire() {
    let (_server, port) = start("dates", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute("create table ev (id int primary key, at timestamptz, day date, lasts interval)").unwrap();
    // binary parameters and results: the driver sends and reads SystemTime as timestamptz
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_micros(1_710_079_509_250_000);
    c.execute("insert into ev (id, at, day, lasts) values ($1, $2, '2024-03-10', '1 day 02:00')", &[&1i32, &t])
        .unwrap();
    let r = c.query_one("select at, at::text, day::text, lasts::text from ev where at = $1", &[&t]).unwrap();
    assert_eq!(r.get::<_, std::time::SystemTime>(0), t);
    assert_eq!(
        (r.get::<_, String>(1), r.get::<_, String>(2), r.get::<_, String>(3)),
        ("2024-03-10 14:05:09.25+00".into(), "2024-03-10".into(), "1 day 02:00:00".into())
    );
}

#[test]
fn token_is_the_password() {
    let (_server, port) = start("auth", Some("s3cret-s3cret-s3cret-s3cret"));
    assert!(connect(port, "password=wrong").is_err());
    assert!(connect(port, "").is_err());
    let mut c = connect(port, "password=s3cret-s3cret-s3cret-s3cret").unwrap();
    assert_eq!(c.query_one("select 1 + 1", &[]).unwrap().get::<_, i32>(0), 2);
}

#[test]
fn psql() {
    if Command::new("psql").arg("--version").output().is_err() {
        eprintln!("skipped: psql not installed");
        return;
    }
    let (_server, port) = start("psql", None);
    let url = format!("postgres://127.0.0.1:{port}/main");
    let psql = |sql: &str| {
        let out = Command::new("psql")
            .args([url.as_str(), "-X", "-A", "-t", "-v", "ON_ERROR_STOP=1", "-c", sql])
            .output()
            .unwrap();
        assert!(out.status.success(), "psql -c {sql}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    psql("create table t (id int primary key, note text); insert into t values (1, 'one'), (2, 'two')");
    assert_eq!(psql("select id, note from t where id >= 1 order by id desc"), "2|two\n1|one\n");
    assert_eq!(psql("select 'it''s', true, null is null"), "it's|t|t\n");
    // FETCH_COUNT: psql reads the answer through a cursor (DECLARE, FETCH FORWARD n, CLOSE)
    let by_cursor = Command::new("psql")
        .args([url.as_str(), "-X", "-A", "-t", "-v", "FETCH_COUNT=1", "-c", "select id, note from t order by id"])
        .output()
        .unwrap();
    assert!(by_cursor.status.success(), "{}", String::from_utf8_lossy(&by_cursor.stderr));
    assert_eq!(String::from_utf8(by_cursor.stdout).unwrap(), "1|one\n2|two\n");
    // COMMENT ON, in \dt+ (obj_description) and \d+ (col_description; psql 16 asks for 11 columns)
    psql("comment on table t is 'the notes'; comment on column t.note is 'free text'");
    let dt = psql("\\dt+ t");
    assert!(dt.contains("the notes"), "{dt}");
    let d = Command::new("psql").args([url.as_str(), "-X", "-c", "\\d+ t"]).output().unwrap();
    let d = format!("{}{}", String::from_utf8_lossy(&d.stdout), String::from_utf8_lossy(&d.stderr));
    assert!(d.contains("free text") && d.contains("Description") && !d.contains("out of range"), "{d}");

    // psql's own commands, answered from Chronos's tables and branches
    psql(
        "create table users (id serial primary key, email text not null, joined timestamptz default now()); create branch trial",
    );
    assert_eq!(psql("\\dt"), "public|t|table|chronos\npublic|users|table|chronos\n");
    let d = psql("\\d users");
    assert!(d.contains("id|integer||not null|nextval('users_id_seq'::regclass)"), "{d}");
    assert!(d.contains("joined|timestamp with time zone|||now()"), "{d}");
    let full = Command::new("psql").args([url.as_str(), "-X", "-c", "\\d users"]).output().unwrap();
    let full = String::from_utf8(full.stdout).unwrap();
    assert!(
        full.contains("Table \"public.users\"") && full.contains("\"users_pkey\" PRIMARY KEY, btree (id)"),
        "{full}"
    );
    psql("create table posts (id int primary key, author int references users on delete cascade, slug text unique)");
    let full = Command::new("psql").args([url.as_str(), "-X", "-c", "\\d posts"]).output().unwrap();
    let full = String::from_utf8(full.stdout).unwrap();
    assert!(full.contains("\"posts_slug_key\" UNIQUE CONSTRAINT, btree (slug)"), "{full}");
    assert!(
        full.contains("\"posts_author_fkey\" FOREIGN KEY (author) REFERENCES users(id) ON DELETE CASCADE"),
        "{full}"
    );
    let users = Command::new("psql").args([url.as_str(), "-X", "-c", "\\d users"]).output().unwrap();
    assert!(String::from_utf8(users.stdout).unwrap().contains("TABLE \"posts\" CONSTRAINT \"posts_author_fkey\""));
    let l = psql("\\l");
    assert!(l.contains("main|chronos|UTF8") && l.contains("trial|chronos|UTF8"), "{l}");
    // a column shows the type it was made with: its width, length and precision
    psql("create table sized (a smallint, b varchar(5), c char(3), d numeric(10, 2), e numeric, f int)");
    let d = psql("\\d sized");
    for want in
        ["a|smallint|", "b|character varying(5)|", "c|character(3)|", "d|numeric(10,2)|", "e|numeric|", "f|integer|"]
    {
        assert!(d.contains(want), "{want} in {d}");
    }
    // and its collation, where it isn't its type's
    psql("create table collated (a text collate \"sv-x-icu\", b varchar(9) collate \"C\", c text)");
    let d = psql("\\d collated");
    for want in ["a|text|sv-x-icu||", "b|character varying(9)|C||", "c|text|||"] {
        assert!(d.contains(want), "{want} in {d}");
    }
}

#[test]
fn agents_log_in_with_their_token() {
    let (_server, port) = start("agents", Some("s3cret-s3cret-s3cret-s3cret"));
    let mut admin = connect(port, "password=s3cret-s3cret-s3cret-s3cret").unwrap();
    admin.batch_execute("create table t (id int primary key, v text); insert into t values (1, 'one')").unwrap();
    let token: String = admin.query_one("create agent bot", &[]).unwrap().get("token");
    assert!(Client::connect(&format!("host=127.0.0.1 port={port} user=bot dbname=main password=nope"), NoTls).is_err());
    let mut bot =
        Client::connect(&format!("host=127.0.0.1 port={port} user=bot dbname=main password={token}"), NoTls).unwrap();
    assert_eq!(bot.query_one("select count(*) from t", &[]).unwrap().get::<_, i64>(0), 1);
    let e = bot.batch_execute("insert into t values (2, 'two')").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    bot.batch_execute("create world botwork; use world botwork; insert into t values (2, 'two')").unwrap();
    let audit = admin.query("show audit for agent bot", &[]).unwrap();
    assert!(audit.iter().any(|r| r.get::<_, String>("action") == "write" && r.get::<_, String>("world") == "botwork"));
}

/// psql's catalog queries (\dt, \l), which the Postgres port answers itself, get the checks every
/// statement gets first: an agent that may not read its world is refused them, as it is refused
/// the same query run by the engine; one that may read gets the answer.
/// An agent's `max_idle_ms`: a connection of its left idle inside a transaction (its model thinking) past it is
/// dropped, the transaction rolled back and its locks let go; SET can lower it, not raise it. Issue #42.
#[test]
fn an_agent_idle_in_a_transaction_is_dropped() {
    let (_server, port) = start("idle_agent", Some("s3cret-s3cret-s3cret-s3cret"));
    let mut admin = connect(port, "password=s3cret-s3cret-s3cret-s3cret").unwrap();
    admin.batch_execute("create table t (id int primary key, v text); insert into t values (1, 'one')").unwrap();
    let token: String = admin
        .query_one("create agent bot with (can = 'read,fork,write_own,write_main', max_idle_ms = '300ms')", &[])
        .unwrap()
        .get("token");
    let shown = admin.query_one("show agents", &[]).unwrap();
    assert_eq!(shown.get::<_, i64>("max_idle_ms"), 300);
    let login = format!("host=127.0.0.1 port={port} user=bot dbname=main password={token}");
    let mut bot = Client::connect(&login, NoTls).unwrap();
    // outside a transaction it holds nothing: no limit
    std::thread::sleep(Duration::from_millis(700));
    bot.batch_execute("select 1").unwrap();
    // a higher timeout of its own doesn't raise its limit
    bot.batch_execute("set idle_in_transaction_session_timeout = '1min'; begin; update t set v = 'held' where id = 1")
        .unwrap();
    std::thread::sleep(Duration::from_millis(900));
    let e = bot.batch_execute("commit").unwrap_err();
    assert!(
        bot.is_closed() || e.code() == Some(&SqlState::IDLE_IN_TRANSACTION_SESSION_TIMEOUT) || e.is_closed(),
        "{e}"
    );
    // rolled back, and the row isn't held
    assert_eq!(admin.query_one("select v from t where id = 1", &[]).unwrap().get::<_, String>(0), "one");
    // the database's own user has no such limit
    admin.batch_execute("begin").unwrap();
    std::thread::sleep(Duration::from_millis(700));
    admin.batch_execute("commit").unwrap();
}

#[test]
fn catalog_answers_take_the_same_checks() {
    let (_server, port) = start("catalog-checks", None);
    let mut admin = connect(port, "").unwrap();
    admin.batch_execute("create table secret_plans (id int primary key)").unwrap();
    let blind: String = admin.query_one("create agent blind with (can = 'fork')", &[]).unwrap().get("token");
    let reader: String = admin.query_one("create agent reader", &[]).unwrap().get("token");
    let dt = "SELECT n.nspname as \"Schema\",\n  c.relname as \"Name\",\n  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' \
              THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN \
              'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned \
              index' END as \"Type\",\n  pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\"\nFROM pg_catalog.pg_class c\n     \
              LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n     LEFT JOIN pg_catalog.pg_am am ON am.oid = \
              c.relam\nWHERE c.relkind IN ('r','p','')\n      AND n.nspname <> 'pg_catalog'\n      AND n.nspname !~ \
              '^pg_toast'\n      AND n.nspname <> 'information_schema'\n  AND pg_catalog.pg_table_is_visible(c.oid)\nORDER \
              BY 1,2;";
    let names = |c: &mut Client| -> Result<Vec<String>, postgres::Error> {
        Ok(c.simple_query(dt)?
            .iter()
            .filter_map(|m| match m {
                postgres::SimpleQueryMessage::Row(r) => r.get(1).map(String::from),
                _ => None,
            })
            .collect())
    };
    assert_eq!(names(&mut admin).unwrap(), ["secret_plans"]);
    let login = |user: &str, token: &str| {
        Client::connect(&format!("host=127.0.0.1 port={port} user={user} dbname=main password={token}"), NoTls).unwrap()
    };
    assert_eq!(names(&mut login("reader", &reader)).unwrap(), ["secret_plans"]);
    let e = names(&mut login("blind", &blind)).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
}

/// Drivers send lists for `= ANY($1)` as typed arrays (binary int8[], text[]); numeric and time
/// travel in their binary forms.
#[test]
fn arrays_numeric_and_time_over_the_wire() {
    let (_server, port) = start("arrays", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute(
        "create table t (id bigint primary key, name text, price numeric(10, 2), at time);
         insert into t values (1, 'a', 9.99, '09:30'), (2, 'b', 0.1, '12:00'), (3, 'c', 100, '23:59:59')",
    )
    .unwrap();
    let ids = |rows: Vec<postgres::Row>| rows.iter().map(|r| r.get::<_, i64>(0)).collect::<Vec<_>>();
    let rows = c.query("select id from t where id = any($1) order by id", &[&vec![1i64, 3]]).unwrap();
    assert_eq!(ids(rows), [1, 3]);
    let rows = c.query("select id from t where name = any($1) order by id", &[&vec!["b", "c", "zz"]]).unwrap();
    assert_eq!(ids(rows), [2, 3]);
    let rows = c.query("select id from t where id <> all($1) order by id", &[&vec![2i64]]).unwrap();
    assert_eq!(ids(rows), [1, 3]);
    let empty: Vec<i64> = vec![];
    assert!(c.query("select id from t where id = any($1)", &[&empty]).unwrap().is_empty());
    // numeric and time come back exact as text
    let r = c.simple_query("select price, at, price * 3 from t where id = 2").unwrap();
    let row = r.iter().find_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { Some(r) } else { None }).unwrap();
    assert_eq!((row.get(0), row.get(1), row.get(2)), (Some("0.10"), Some("12:00:00"), Some("0.30")));
}

/// A runaway query stops at its statement_timeout, or when the client cancels it (the driver's
/// cancel token is what psql's Ctrl-C sends), and the connection goes on.
#[test]
fn timeouts_and_cancel_requests_stop_a_query() {
    let (_server, port) = start("cancel", None);
    let runaway = "select count(*) from generate_series(1, 30000) a, generate_series(1, 30000) b";
    let mut c = connect(port, "").unwrap();
    c.batch_execute("set statement_timeout = 300").unwrap();
    let t = Instant::now();
    let e = c.simple_query(runaway).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::QUERY_CANCELED), "{e}");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    c.batch_execute("reset statement_timeout").unwrap();

    let cancel = c.cancel_token();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        cancel.cancel_query(NoTls).unwrap();
    });
    let t = Instant::now();
    let e = c.simple_query(runaway).unwrap_err();
    canceller.join().unwrap();
    assert_eq!(e.code(), Some(&SqlState::QUERY_CANCELED), "{e}");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    let n: i32 = c.query_one("select 1 + 1", &[]).unwrap().get(0);
    assert_eq!(n, 2, "the connection goes on");
}

/// A statement whose client went away (psql killed mid-query) stops instead of running on.
#[test]
fn a_query_stops_when_its_client_disconnects() {
    if Command::new("psql").arg("--version").output().is_err() {
        eprintln!("skipped: psql not installed");
        return;
    }
    let (_server, port) = start("gone", None);
    let url = format!("postgres://127.0.0.1:{port}/main");
    let mut c = connect(port, "").unwrap();
    let errors = |c: &mut Client| -> i64 {
        let rows = c.query("show metrics", &[]).unwrap();
        rows.iter().find(|r| r.get::<_, String>(0) == "errors").map_or(0, |r| r.get::<_, i64>(1))
    };
    let before = errors(&mut c);
    let mut psql = Command::new("psql")
        .args([
            url.as_str(),
            "-X",
            "-c",
            "select count(*) from generate_series(1, 30000) a, generate_series(1, 30000) b",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    psql.kill().unwrap();
    psql.wait().unwrap();
    let t = Instant::now();
    while errors(&mut c) == before {
        assert!(t.elapsed() < Duration::from_secs(10), "the abandoned query is still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn listen_and_notify_over_the_wire() {
    use postgres::fallible_iterator::FallibleIterator;
    let (_server, port) = start("notify", None);
    let (mut listener, mut talker) = (connect(port, "").unwrap(), connect(port, "").unwrap());
    // the first to arrive within `wait` ms, then any already arrived with it
    let heard = |c: &mut Client, wait: u64| -> Vec<String> {
        let mut n = c.notifications();
        let first = n.timeout_iter(Duration::from_millis(wait)).next().unwrap();
        let rest: Vec<_> = n.iter().collect().unwrap();
        first.into_iter().chain(rest).map(|n| format!("{}:{}", n.channel(), n.payload())).collect()
    };
    listener.batch_execute("listen jobs").unwrap();
    // an idle listener hears a NOTIFY made outside a transaction, without asking
    talker.batch_execute("notify jobs, 'one'").unwrap();
    let start = Instant::now();
    let mut got = heard(&mut listener, 3000);
    assert_eq!(got, ["jobs:one"]);
    assert!(start.elapsed() < Duration::from_millis(1500), "delivered at once, not at a timeout");
    // in a transaction: at COMMIT, not before, and not at all after ROLLBACK
    talker.batch_execute("begin; notify jobs, 'two'; notify jobs, 'two'").unwrap();
    assert!(heard(&mut listener, 300).is_empty());
    talker.batch_execute("commit").unwrap();
    got = heard(&mut listener, 3000);
    assert_eq!(got, ["jobs:two"]);
    talker.batch_execute("begin; notify jobs, 'never'; rollback; select pg_notify('jobs', 'three')").unwrap();
    assert_eq!(heard(&mut listener, 1000), ["jobs:three"]);
    // a session hears its own, before its query's result is done
    listener.batch_execute("notify jobs, 'self'").unwrap();
    assert_eq!(listener.notifications().len(), 1);
    // a listener in another world doesn't hear main's
    let mut elsewhere = connect(port, "").unwrap();
    elsewhere.batch_execute("create world w; switch world w; listen jobs").unwrap();
    talker.batch_execute("notify jobs, 'main only'").unwrap();
    assert!(heard(&mut elsewhere, 300).is_empty());
}

#[test]
fn psql_shows_views_and_sequences() {
    if Command::new("psql").arg("--version").output().is_err() {
        eprintln!("skipped: psql not installed");
        return;
    }
    let (_server, port) = start("psql-views", None);
    let url = format!("postgres://127.0.0.1:{port}/main");
    let psql = |sql: &str| {
        let out = Command::new("psql").args([url.as_str(), "-X", "-v", "ON_ERROR_STOP=1", "-c", sql]).output().unwrap();
        assert!(out.status.success(), "psql -c {sql}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    psql("create table t (id int primary key, name text); create view v as select id, name from t where id > 1");
    psql("create materialized view m as select id from t; create sequence s start 5");
    let all = psql("\\d");
    for row in [
        "| m    | materialized view |",
        "| s    | sequence          |",
        "| t    | table             |",
        "| v    | view              |",
    ] {
        assert!(all.contains(row), "{row}\n{all}");
    }
    assert!(psql("\\dv").contains("| v    | view |"));
    assert!(psql("\\dm").contains("| m    | materialized view |"));
    assert!(psql("\\ds").contains("| s    | sequence |"));
    let v = psql("\\d+ v");
    assert!(v.contains("View \"public.v\"") && v.contains("View definition:") && v.contains("WHERE id > 1"), "{v}");
    let s = psql("\\d s");
    // (pg_sequence's numbers are bigints, which psql lines up on the right)
    assert!(s.contains("Sequence \"public.s\"") && s.contains(" bigint |     5 |"), "{s}");
    assert!(psql("\\d m").contains("Materialized view \"public.m\""));
    // the + forms ask for sizes: pg_size_pretty(pg_table_size(oid)), pg_size_pretty(pg_database_size(name))
    psql("insert into t select g, repeat('x', 200) from generate_series(1, 500) g");
    let plus = psql("\\dt+");
    assert!(plus.contains("Persistence") && plus.contains("| permanent") && plus.contains(" kB "), "{plus}");
    assert!(psql("\\di+").contains("| t_pkey"), "the index has a size too");
    let dbs = psql("\\l+");
    assert!(dbs.contains("Size") && dbs.contains("pg_default"), "{dbs}");
}

#[test]
fn psql_lists_indexes_partitions_and_owned_sequences() {
    if Command::new("psql").arg("--version").output().is_err() {
        eprintln!("skipped: psql not installed");
        return;
    }
    let (_server, port) = start("psql-parts", None);
    let url = format!("postgres://127.0.0.1:{port}/main");
    let psql = |sql: &str| {
        let out = Command::new("psql").args([url.as_str(), "-X", "-A", "-t", "-c", sql]).output().unwrap();
        assert!(out.status.success(), "psql -c {sql}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    psql("create table st (id serial primary key, note text)");
    // a serial's sequence: "Owned by" its column (psql's pg_depend query)
    let seq = Command::new("psql").args([url.as_str(), "-X", "-c", "\\d st_id_seq"]).output().unwrap();
    let seq = String::from_utf8(seq.stdout).unwrap();
    assert!(seq.contains("Owned by: public.st.id"), "{seq}");
    // \di: the indexes, with the table each is on
    assert_eq!(psql("\\di"), "public|st_pkey|index|chronos|st\n");
    // a quoted name with dots is a regular expression psql writes as E'...': nothing has it
    assert_eq!(psql("\\dt \"no.such.table\""), "");
    // \dP: the partitioned tables and their indexes, a partition that is partitioned under its parent
    psql("create table pt (id int) partition by range (id); create index pt_idx on pt (id)");
    psql("create table pt_1 partition of pt for values from (0) to (10) partition by range (id)");
    assert_eq!(psql("\\dP"), "public|pt|chronos|partitioned table|\npublic|pt_idx|chronos|partitioned index|pt\n");
    assert_eq!(psql("\\dPt"), "public|pt|chronos\n");
    assert_eq!(psql("\\dPtn"), "public|pt|chronos|\npublic|pt_1|chronos|pt\n");
}

#[test]
fn time_zone_from_the_connection_and_set() {
    let (_server, port) = start("zone", None);
    let text = |c: &mut Client, q: &str| {
        let msgs = c.simple_query(q).unwrap();
        msgs.iter()
            .find_map(|m| match m {
                postgres::SimpleQueryMessage::Row(r) => Some(r.get(0).unwrap().to_string()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{q}: no row"))
    };
    let at = "select '2026-09-29 00:00Z'::timestamptz::text";
    let mut c = connect(port, "options='-c TimeZone=Asia/Kolkata'").unwrap();
    assert_eq!(text(&mut c, at), "2026-09-29 05:30:00+05:30");
    assert_eq!(text(&mut c, "show timezone"), "Asia/Kolkata");
    c.batch_execute("set time zone 'Asia/Tokyo'").unwrap();
    assert_eq!(text(&mut c, at), "2026-09-29 09:00:00+09");
    // a timestamptz itself, not cast to text: shown when its row is sent, in the zone too
    assert_eq!(text(&mut c, "select '2026-09-29 00:00Z'::timestamptz"), "2026-09-29 09:00:00+09");
    // SET and the query in one message: the query's rows are shown in the new zone
    let both = "set time zone 'Asia/Kolkata'; select '2026-09-29 00:00Z'::timestamptz";
    assert_eq!(text(&mut c, both), "2026-09-29 05:30:00+05:30");
    let mut plain = connect(port, "").unwrap();
    assert_eq!(text(&mut plain, at), "2026-09-29 00:00:00+00");
    // a zone the tz database doesn't have: refused, not served in UTC
    let Err(e) = connect(port, "options='-c TimeZone=Mars/Olympus_Mons'") else { panic!("connected") };
    assert_eq!(e.code(), Some(&SqlState::INVALID_PARAMETER_VALUE), "{e}");
    // one with daylight saving time keeps it (given a tz database to read it from)
    if std::path::Path::new("/usr/share/zoneinfo/America/New_York").exists() {
        let mut c = connect(port, "options='-c TimeZone=America/New_York'").unwrap();
        assert_eq!(text(&mut c, "show timezone"), "America/New_York");
        assert_eq!(text(&mut c, "select '2026-09-29 00:00Z'::timestamptz::text"), "2026-09-28 20:00:00-04");
        assert_eq!(text(&mut c, "select '2026-01-29 00:00Z'::timestamptz::text"), "2026-01-28 19:00:00-05");
    }
}

/// A constraint violation names what it broke in the ErrorResponse fields (`constraint`, `table`,
/// `column`, `schema`, `detail`), which drivers read as `err.constraint` and so on.
#[test]
fn constraint_violations_carry_their_fields() {
    let (_server, port) = start("violations", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute(
        "create table q (id int primary key, k text, n int not null default 0, constraint q_k_uq unique (k), constraint q_n_pos check (n >= 0));
         create table kid (id int primary key, q int, constraint kid_q_fk foreign key (q) references q (id));
         insert into q values (1, 'a', 1);",
    )
    .unwrap();
    let db = |c: &mut Client, sql: &str| -> postgres::error::DbError {
        c.batch_execute(sql).unwrap_err().as_db_error().expect("a database error").clone()
    };

    // Postgres's message, which apps match on; the row that has the key, which Chronos knows, in the HINT
    let e = db(&mut c, "insert into q values (2, 'a', 1)");
    assert_eq!(e.code(), &SqlState::UNIQUE_VIOLATION);
    assert_eq!(e.message(), "duplicate key value violates unique constraint \"q_k_uq\"");
    assert_eq!((e.constraint(), e.table(), e.schema()), (Some("q_k_uq"), Some("q"), Some("public")));
    assert_eq!(e.detail(), Some("Key (k)=(a) already exists."));
    assert_eq!(e.hint(), Some("Row q/1 already has this key."));

    let e = db(&mut c, "insert into q values (1, 'b', 1)");
    assert_eq!((e.constraint(), e.table()), (Some("q_pkey"), Some("q")));
    assert_eq!(e.detail(), Some("Key (id)=(1) already exists."));

    let e = db(&mut c, "insert into q values (3, 'c', null)");
    assert_eq!(e.code(), &SqlState::NOT_NULL_VIOLATION);
    assert_eq!((e.table(), e.column()), (Some("q"), Some("n")));

    let e = db(&mut c, "insert into q values (3, 'c', -1)");
    assert_eq!(e.code(), &SqlState::CHECK_VIOLATION);
    assert_eq!((e.constraint(), e.table()), (Some("q_n_pos"), Some("q")));

    let e = db(&mut c, "insert into kid values (1, 99)");
    assert_eq!(e.code(), &SqlState::FOREIGN_KEY_VIOLATION);
    assert_eq!(e.message(), "insert or update on table \"kid\" violates foreign key constraint \"kid_q_fk\"");
    assert_eq!((e.constraint(), e.table()), (Some("kid_q_fk"), Some("kid")));
    assert_eq!(e.detail(), Some("Key (q)=(99) is not present in table \"q\"."));
    assert_eq!(e.hint(), Some("kid/1: q = 99 isn't a row of q."));

    // (the table named is the one with the foreign key, as in Postgres)
    c.batch_execute("insert into kid values (1, 1)").unwrap();
    let e = db(&mut c, "delete from q where id = 1");
    assert_eq!(e.code(), &SqlState::FOREIGN_KEY_VIOLATION);
    assert_eq!(
        e.message(),
        "update or delete on table \"q\" violates foreign key constraint \"kid_q_fk\" on table \"kid\""
    );
    assert_eq!((e.constraint(), e.table()), (Some("kid_q_fk"), Some("kid")));
    assert_eq!(e.detail(), Some("Key (id)=(1) is still referenced from table \"kid\"."));
    assert_eq!(e.hint(), Some("q/1 is still referenced from kid/1: delete or change those rows first."));
}

/// A driver that asks for binary results reads arrays back as typed arrays: what `array_agg`,
/// `ARRAY[...]` and array columns give, with nulls, empty arrays and the element types it knows.
#[test]
fn array_results_come_back_in_binary() {
    let (_server, port) = start("array-results", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute(
        "create table t (id int primary key, n int[], b bigint[], s text[], f double precision[], flags boolean[]);
         insert into t values (1, '{1,2,3}', '{10000000000,-5}', '{a,\"b c\",NULL}', '{1.5,-2.25}', '{t,f}'),
                              (2, '{}', '{}', '{}', '{}', '{}'),
                              (3, '{7,NULL}', NULL, NULL, NULL, NULL)",
    )
    .unwrap();
    let rows = c.query("select n, b, s, f, flags from t order by id", &[]).unwrap();
    assert_eq!(rows[0].get::<_, Vec<i32>>(0), [1, 2, 3]);
    assert_eq!(rows[0].get::<_, Vec<i64>>(1), [10_000_000_000, -5]);
    assert_eq!(rows[0].get::<_, Vec<Option<String>>>(2), [Some("a".into()), Some("b c".into()), None]);
    assert_eq!(rows[0].get::<_, Vec<f64>>(3), [1.5, -2.25]);
    assert_eq!(rows[0].get::<_, Vec<bool>>(4), [true, false]);
    assert!(rows[1].get::<_, Vec<i32>>(0).is_empty() && rows[1].get::<_, Vec<String>>(2).is_empty());
    assert_eq!(rows[2].get::<_, Vec<Option<i32>>>(0), [Some(7), None]);
    assert_eq!(rows[2].get::<_, Option<Vec<i64>>>(1), None, "a null array is null, not empty");

    // the same through aggregates and constructors, and what went in comes back out
    let agg: Vec<i32> = c.query_one("select array_agg(id order by id desc) from t", &[]).unwrap().get(0);
    assert_eq!(agg, [3, 2, 1]);
    let made: Vec<i32> = c.query_one("select array(select id from t where id > 1 order by id)", &[]).unwrap().get(0);
    assert_eq!(made, [2, 3]);
    let lits: Vec<i32> = c.query_one("select array[3, 1, 2]", &[]).unwrap().get(0);
    assert_eq!(lits, [3, 1, 2]);
    let back: Vec<i64> = c.query_one("select $1::bigint[]", &[&vec![4i64, 5, 6]]).unwrap().get(0);
    assert_eq!(back, [4, 5, 6]);
    let names: Vec<String> = c.query_one("select string_to_array($1::text, ',')", &[&"x,y,z"]).unwrap().get(0);
    assert_eq!(names, ["x", "y", "z"]);
    let mut t = c.prepare("select n from t where id = $1").unwrap();
    assert_eq!(c.query_one(&t, &[&1i32]).unwrap().get::<_, Vec<i32>>(0), [1, 2, 3]);
    t = c.prepare("select n from t where id = $1").unwrap(); // (and a statement made again)
    assert_eq!(c.query_one(&t, &[&3i32]).unwrap().get::<_, Vec<Option<i32>>>(0), [Some(7), None]);
}

#[test]
fn databases_are_created_and_connected_to_by_name() {
    let (_server, port) = start("databases", None);
    // where Rails, Prisma and Django connect to make the others
    let mut admin = Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=postgres"), NoTls).unwrap();
    admin.batch_execute("create table on_main (id int)").unwrap();
    admin.batch_execute("CREATE DATABASE \"test_app\" ENCODING = 'unicode'").unwrap();
    let found: i64 =
        admin.query_one("select count(*) from pg_database where datname = 'test_app'", &[]).unwrap().get(0);
    assert_eq!(found, 1);

    let mut app = Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=test_app"), NoTls).unwrap();
    let tables: i64 = app.query_one("select count(*) from information_schema.tables", &[]).unwrap().get(0);
    assert_eq!(tables, 0, "empty, not a copy of main");
    app.batch_execute("create table on_main (id int, extra text)").unwrap();
    let me: String = app.query_one("select current_database()", &[]).unwrap().get(0);
    assert_eq!(me, "test_app");

    // it can't drop itself, and drops from another connection
    let e = app.batch_execute("drop database test_app").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("55006"));
    // the extended protocol too (Prisma, JDBC): a statement with no rows to describe
    admin.execute("create database viaextended", &[]).unwrap();
    admin.execute("drop database viaextended", &[]).unwrap();
    admin.batch_execute("drop database test_app").unwrap();
    assert!(Client::connect(&format!("host=127.0.0.1 port={port} user=tester dbname=test_app"), NoTls).is_err());
    let gone: i64 = admin.query_one("select count(*) from pg_database where datname = 'test_app'", &[]).unwrap().get(0);
    assert_eq!(gone, 0);
}

#[test]
fn deferred_keys_over_the_wire() {
    let (_server, port) = start("deferred", None);
    let mut c = connect(port, "").unwrap();
    // what Django emits for every foreign key, and what its loaddata does: children before parents
    c.batch_execute(
        "create table author (id bigint generated by default as identity primary key, name text not null);
         create table book (id bigint generated by default as identity primary key, author_id bigint not null
             constraint book_author_fk references author(id) deferrable initially deferred)",
    )
    .unwrap();
    let mut tx = c.transaction().unwrap();
    tx.execute("insert into book (author_id) values ($1)", &[&7i64]).unwrap();
    tx.execute("insert into author (id, name) values ($1, $2)", &[&7i64, &"Ada"]).unwrap();
    tx.commit().unwrap();
    let n: i64 = c.query_one("select count(*) from book", &[]).unwrap().get(0);
    assert_eq!(n, 1);

    // SET CONSTRAINTS through the extended protocol, and a COMMIT that fails on the wire
    let mut tx = c.transaction().unwrap();
    tx.execute("set constraints all deferred", &[]).unwrap();
    tx.execute("insert into book (author_id) values ($1)", &[&8i64]).unwrap();
    let e = tx.commit().unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("23503"));
    assert_eq!(e.as_db_error().and_then(|d| d.constraint()), Some("book_author_fk"), "{e:?}");
    let n: i64 = c.query_one("select count(*) from book", &[]).unwrap().get(0);
    assert_eq!(n, 1, "the transaction was rolled back");
}

#[test]
fn client_min_messages_hides_notices_below_it() {
    let (_server, port) = start("min-messages", None);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    let mut cfg = postgres::Config::new();
    cfg.host("127.0.0.1").port(port).user("tester").dbname("main");
    cfg.notice_callback(move |n| sink.lock().unwrap().push(format!("{}: {}", n.severity(), n.message())));
    let mut c = cfg.connect(NoTls).unwrap();
    let said = || std::mem::take(&mut *seen.lock().unwrap());

    // a notice by default
    c.batch_execute("create schema if not exists public").unwrap();
    let first = said();
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].starts_with("NOTICE: schema \"public\" already exists"), "{first:?}");
    // what pg_dump's scripts set: no notice, and the setting reads back
    c.batch_execute("SET client_min_messages = warning").unwrap();
    let shown: String = c.query_one("show client_min_messages", &[]).unwrap().get(0);
    assert_eq!(shown, "warning");
    c.batch_execute("create schema if not exists public; drop table if exists nothing_here").unwrap();
    assert_eq!(said(), Vec::<String>::new());
    // a warning still comes, and 'notice' brings notices back
    c.batch_execute("commit").unwrap();
    assert!(said().iter().all(|m| m.starts_with("WARNING")));
    c.batch_execute("SET client_min_messages TO notice").unwrap();
    c.batch_execute("drop table if exists nothing_here").unwrap();
    assert_eq!(said(), ["NOTICE: table \"nothing_here\" does not exist, skipping"]);
    let e = c.batch_execute("SET client_min_messages = loud").unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("22023"));
}

/// A statement that raises notices and then fails sends the notices before its error over the extended
/// protocol too, as Postgres does, so a driver's `query` sees them.
#[test]
fn notices_before_an_error_reach_an_extended_query() {
    let (_server, port) = start("notice-error", None);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    let mut cfg = postgres::Config::new();
    cfg.host("127.0.0.1").port(port).user("tester").dbname("main");
    cfg.notice_callback(move |n| sink.lock().unwrap().push(format!("{}: {}", n.severity(), n.message())));
    let mut c = cfg.connect(NoTls).unwrap();
    c.batch_execute(
        "create function noisy() returns int language plpgsql as $$ begin raise notice 'hello'; raise exception 'boom'; end $$",
    )
    .unwrap();
    let e = c.query("select noisy()", &[]).unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("P0001"));
    assert_eq!(*seen.lock().unwrap(), ["NOTICE: hello"]);
    // and the next statement starts with none of them
    c.query("select 1", &[]).unwrap();
    assert_eq!(*seen.lock().unwrap(), ["NOTICE: hello"]);
}

/// A syntax error says where it is (the ErrorResponse's position, which psql draws its caret from),
/// in Postgres's words, with what was expected as the hint.
#[test]
fn syntax_errors_carry_their_position() {
    let (_server, port) = start("position", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute("create table t (a int, b text)").unwrap();
    for (sql, near, at) in [
        ("select * from t where;", "syntax error at or near \";\"", 22),
        ("selec 1", "syntax error at or near \"selec\"", 1),
        ("select a,, b from t", "syntax error at or near \",\"", 10),
        ("select 1 from", "syntax error at end of input", 14),
        ("select 1\nfrom t\nwhere a = ;", "syntax error at or near \";\"", 27),
        ("select 'é' from t where;", "syntax error at or near \";\"", 24),
    ] {
        let e = c.simple_query(sql).unwrap_err();
        let db = e.as_db_error().unwrap_or_else(|| panic!("{sql}: {e}"));
        assert_eq!(db.code(), &SqlState::SYNTAX_ERROR, "{sql}");
        assert_eq!(db.message(), near, "{sql}");
        assert_eq!(db.position(), Some(&postgres::error::ErrorPosition::Original(at)), "{sql}");
        assert!(db.hint().is_some_and(|h| !h.is_empty()), "{sql}: says what was expected");
    }
    // psql draws its caret from it
    if Command::new("psql").arg("--version").output().is_ok() {
        let out = Command::new("psql")
            .args([
                format!("postgres://127.0.0.1:{port}/main"),
                "-X".into(),
                "-c".into(),
                "select * from t where;".into(),
            ])
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(err.contains("LINE 1: select * from t where;\n") && err.contains("^"), "{err}");
    }
}

/// What Postgres adds to an error where a client can use it: a DETAIL naming what depends on the
/// object, and the HINT that says what to do.
#[test]
fn errors_that_say_what_to_do_carry_a_hint() {
    let (_server, port) = start("hint", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute(
        "create table parent (id int primary key); create table child (id int primary key, p int references parent);
         create table base (id int primary key); create view over_base as select id from base",
    )
    .unwrap();
    for (sql, message, detail) in [
        (
            "drop table parent",
            "cannot drop table parent because other objects depend on it",
            "constraint child_p_fkey on table child depends on table parent",
        ),
        (
            "drop table base",
            "cannot drop table base because other objects depend on it",
            "view over_base depends on table base",
        ),
    ] {
        let e = c.batch_execute(sql).unwrap_err();
        let db = e.as_db_error().unwrap_or_else(|| panic!("{sql}: {e}"));
        assert_eq!(db.code(), &SqlState::DEPENDENT_OBJECTS_STILL_EXIST, "{sql}");
        assert_eq!((db.message(), db.detail()), (message, Some(detail)), "{sql}");
        assert_eq!(db.hint(), Some("Use DROP ... CASCADE to drop the dependent objects too."), "{sql}");
    }
    // and the cursor that can only go forward
    c.batch_execute("begin; declare k no scroll cursor for select id from parent").unwrap();
    let e = c.batch_execute("fetch prior from k").unwrap_err();
    assert_eq!(e.as_db_error().and_then(|d| d.hint()), Some("Declare it with SCROLL option to enable backward scan."));
}

/// A uuid as a driver has it: 16 bytes, read and written only for the uuid type (what sqlx's `Uuid` and
/// pgx's `pgtype.UUID` do, which is why a column that says `text` instead breaks them).
#[derive(Debug, PartialEq)]
struct Uuid([u8; 16]);

impl<'a> postgres::types::FromSql<'a> for Uuid {
    fn from_sql(_: &postgres::types::Type, raw: &'a [u8]) -> Result<Uuid, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Uuid(raw.try_into()?))
    }
    fn accepts(ty: &postgres::types::Type) -> bool {
        *ty == postgres::types::Type::UUID
    }
}

impl postgres::types::ToSql for Uuid {
    fn to_sql(
        &self,
        _: &postgres::types::Type,
        out: &mut postgres::types::private::BytesMut,
    ) -> Result<postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        out.extend_from_slice(&self.0);
        Ok(postgres::types::IsNull::No)
    }
    fn accepts(ty: &postgres::types::Type) -> bool {
        *ty == postgres::types::Type::UUID
    }
    postgres::types::to_sql_checked!();
}

/// uuid over the wire: its own type id (2950), 16 bytes in binary both ways, canonical text otherwise.
#[test]
fn uuid_over_the_wire() {
    let (_server, port) = start("uuid", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute("create table owners (id uuid primary key default gen_random_uuid(), name text)").unwrap();
    let a = Uuid(*b"\xa0\xee\xbc\x99\x9c\x0b\x4e\xf8\xbb\x6d\x6b\xb9\xbd\x38\x0a\x11");
    assert_eq!(c.execute("insert into owners values ($1, $2)", &[&a, &"ada"]).unwrap(), 1);
    c.batch_execute("insert into owners (name) values ('made here')").unwrap();
    // the column is a uuid to the driver, and reads back byte for byte
    let stmt = c.prepare("select id, name from owners where id = $1").unwrap();
    assert_eq!(stmt.columns()[0].type_(), &postgres::types::Type::UUID);
    assert_eq!(stmt.params(), [postgres::types::Type::UUID]);
    let r = c.query_one(&stmt, &[&a]).unwrap();
    assert_eq!((r.get::<_, Uuid>(0), r.get::<_, String>(1)), (a, "ada".to_string()));
    // one the server made
    let r = c.query_one("select id from owners where name = 'made here'", &[]).unwrap();
    assert_eq!(r.columns()[0].type_(), &postgres::types::Type::UUID);
    assert_ne!(r.get::<_, Uuid>(0), Uuid([0; 16]));
    // in text (the simple protocol, psql): lowercase with hyphens, whatever it was written as
    c.batch_execute("insert into owners values ('B0EEBC999C0B4EF8BB6D6BB9BD380A12', 'upper')").unwrap();
    let rows = c.simple_query("select id::text, id from owners where name = 'upper'").unwrap();
    let texts: Vec<Option<String>> = rows
        .iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some(vec![r.get(0).map(String::from), r.get(1).map(String::from)]),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        texts,
        [Some("b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a12".into()), Some("b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a12".into())]
    );
    // a value that isn't one is 22P02, in text and in a typed parameter's text
    let e = c.batch_execute("insert into owners (id) values ('nope')").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INVALID_TEXT_REPRESENTATION), "{e}");
}

/// A setting Postgres would honor and Chronos does not is refused (0A000) rather than accepted and
/// ignored: a session that asked for `DateStyle = German` would read dates in the wrong style
/// without a word. The values Chronos does implement are taken, and the rest of Postgres's
/// settings are kept as set and read back.
#[test]
fn settings_that_change_answers_take_only_what_chronos_does() {
    let (_server, port) = start("settings", None);
    let mut c = connect(port, "").unwrap();
    let code = |c: &mut Client, q: &str| c.batch_execute(q).unwrap_err().code().map(|c| c.code().to_string());
    let show = |c: &mut Client, name: &str| -> String {
        c.query_one(&format!("select current_setting('{name}')"), &[]).unwrap().get(0)
    };
    for (q, want) in [
        ("set intervalstyle = 'bogus'", "22023"),
        ("set standard_conforming_strings = off", "0A000"),
        ("set bytea_output = 'base64'", "22023"),
        ("set extra_float_digits = 4", "22023"),
        ("set client_encoding = 'LATIN1'", "0A000"),
        ("set session_replication_role = replica", "0A000"),
        // (default_transaction_read_only is taken now: READ ONLY transactions are enforced)
        ("set default_transaction_read_only = maybe", "22023"),
        ("set array_nulls = off", "0A000"),
        ("set quote_all_identifiers = on", "0A000"),
        ("set lc_numeric = 'de_DE.UTF-8'", "0A000"),
        // (and what Postgres itself refuses)
        ("set datestyle = 'nonsense'", "22023"),
        ("set work_mem = 'abc'", "22023"),
        ("set max_connections = 10", "55P02"),
        ("set nonexistent_setting = 1", "42704"),
    ] {
        assert_eq!(code(&mut c, q).as_deref(), Some(want), "{q}");
    }
    // what is refused is not half-applied
    assert_eq!(show(&mut c, "DateStyle"), "ISO, MDY");
    assert_eq!(show(&mut c, "standard_conforming_strings"), "on");
    // what Chronos does is taken, in Postgres's spelling
    c.batch_execute("set datestyle = 'iso,mdy'; set extra_float_digits = 3; set client_encoding = 'utf-8'").unwrap();
    assert_eq!(show(&mut c, "datestyle"), "ISO, MDY");
    assert_eq!(show(&mut c, "extra_float_digits"), "3");
    // 0 and below write floats as `%.15g` and its kin do, from 15 digits down
    assert_eq!(c.query_one("select (0.1::float8 * 3)::text", &[]).unwrap().get::<_, String>(0), "0.30000000000000004");
    c.batch_execute("set extra_float_digits = 0").unwrap();
    assert_eq!(c.query_one("select (0.1::float8 * 3)::text", &[]).unwrap().get::<_, String>(0), "0.3");
    c.batch_execute("set extra_float_digits = -5").unwrap();
    assert_eq!(c.query_one("select (1.0::float8 / 3)::text", &[]).unwrap().get::<_, String>(0), "0.3333333333");
    c.batch_execute("set extra_float_digits = 3").unwrap();
    assert_eq!(show(&mut c, "client_encoding"), "UTF8");
    // settings with no effect here are kept and read back, in the unit Postgres shows
    c.batch_execute("set work_mem = '64MB'; set enable_seqscan = off; set jit = off").unwrap();
    assert_eq!(show(&mut c, "work_mem"), "64MB");
    assert_eq!(show(&mut c, "enable_seqscan"), "off");
    let r = c.query_one("select setting, unit, source from pg_settings where name = 'work_mem'", &[]).unwrap();
    assert_eq!(
        (r.get::<_, String>(0), r.get::<_, String>(1), r.get::<_, String>(2)),
        ("65536".to_string(), "kB".to_string(), "session".to_string())
    );
    // a rolled-back SET is undone, and RESET puts the default back
    c.batch_execute("begin; set work_mem = '1GB'; rollback").unwrap();
    assert_eq!(show(&mut c, "work_mem"), "64MB");
    c.batch_execute("reset work_mem").unwrap();
    assert_eq!(show(&mut c, "work_mem"), "4MB");
    // the settings list is Postgres's, all of it
    let n: i64 = c.query_one("select count(*) from pg_settings where name not like '%.%'", &[]).unwrap().get(0);
    assert_eq!(n, 363);
}

/// A value's bytes as they came, whatever its type.
#[derive(Debug, PartialEq)]
struct Raw(Vec<u8>);

impl<'a> postgres::types::FromSql<'a> for Raw {
    fn from_sql(_: &postgres::types::Type, raw: &'a [u8]) -> Result<Raw, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(raw.to_vec()))
    }
    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
}

/// `pg_index.indkey`, `indclass` and `indoption` are int2vector and oidvector: `1 2` in text, an
/// array counted from 0 in binary. They were declared int2[] over text like `1 2`, which no array
/// parser reads (psycopg's failed with "malformed array", so SQLAlchemy could not reflect indexes).
#[test]
fn int2vector_and_oidvector_over_the_wire() {
    let (_server, port) = start("vectors", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute("create table iv (a int, b int, primary key (a, b))").unwrap();
    let q = "select indkey, indclass, indoption from pg_index where indrelid = 'iv'::regclass";
    let stmt = c.prepare(q).unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [22, 30, 22], "int2vector, oidvector, int2vector");
    // in binary: one dimension of int2s (oids, for an oidvector) whose lower bound is 0
    let r = c.query_one(&stmt, &[]).unwrap();
    let int2s: Vec<u8> =
        [1i32, 0, 21, 2, 0].iter().flat_map(|x| x.to_be_bytes()).chain([0, 0, 0, 2, 0, 1, 0, 0, 0, 2, 0, 2]).collect();
    assert_eq!(r.get::<_, Raw>(0), Raw(int2s));
    let head: Vec<u8> = [1i32, 0, 26, 2, 0].iter().flat_map(|x| x.to_be_bytes()).collect();
    assert_eq!(&r.get::<_, Raw>(1).0[..20], &head[..], "an oidvector's elements are oids");
    // in text: the numbers with spaces between them
    let rows = c.simple_query(q).unwrap();
    let row =
        rows.iter().find_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { Some(r) } else { None }).unwrap();
    assert_eq!((row.get(0), row.get(2)), (Some("1 2"), Some("0 0")));
    // what reads them as arrays still does (a number's text, since a smallint reaches a driver as an integer)
    let r = c
        .query_one(
            "select pg_typeof(indkey)::text, indkey::int2[]::text, indkey[0]::text, array_length(indkey, 1) \
             from pg_index where indrelid = 'iv'::regclass",
            &[],
        )
        .unwrap();
    let got: Vec<String> = (0..3).map(|i| r.get(i)).collect();
    assert_eq!(got, ["int2vector", "[0:1]={1,2}", "1"]);
    assert_eq!(r.get::<_, i32>(3), 2);
    let rows = c
        .query("select x::text from pg_index, unnest(indkey) as x where indrelid = 'iv'::regclass order by 1", &[])
        .unwrap();
    assert_eq!(rows.iter().map(|r| r.get::<_, String>(0)).collect::<Vec<_>>(), ["1", "2"]);
}

/// A regproc and a regprocedure reach a driver as Postgres sends them: typed 24 and 2202, the function's name in
/// text and its oid in binary (a driver that reads `typinput` in binary got the name's bytes as text).
#[test]
fn regproc_over_the_wire() {
    let (_server, port) = start("regproc", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute("create function rp_mine() returns int language sql as 'select 1'").unwrap();
    let q = "select typinput, 'now'::regproc, 'int4in(cstring)'::regprocedure, 'rp_mine'::regproc, \
             'rp_mine'::regproc::oid from pg_type where typname = 'int4'";
    let stmt = c.prepare(q).unwrap();
    let ids: Vec<u32> = stmt.columns()[..4].iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [24, 24, 2202, 24], "regproc, regproc, regprocedure, regproc");
    let r = c.query_one(&stmt, &[]).unwrap();
    let oid = |n: u32| Raw(n.to_be_bytes().to_vec());
    assert_eq!((r.get::<_, Raw>(0), r.get::<_, Raw>(1), r.get::<_, Raw>(2)), (oid(42), oid(1299), oid(42)));
    // (`::oid` is an oid, four unsigned bytes)
    let mine: u32 = r.get(4);
    assert_eq!(r.get::<_, Raw>(3), oid(mine), "a function made here: its own oid");
    let rows = c.simple_query(q).unwrap();
    let row =
        rows.iter().find_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { Some(r) } else { None }).unwrap();
    let got: Vec<Option<&str>> = (0..4).map(|i| row.get(i)).collect();
    assert_eq!(got, [Some("int4in"), Some("now"), Some("int4in(cstring)"), Some("rp_mine")]);
}

/// A parameter sent in text format, whatever its type.
#[derive(Debug)]
struct Text(&'static str);

impl postgres::types::ToSql for Text {
    fn to_sql(
        &self,
        _: &postgres::types::Type,
        out: &mut postgres::types::private::BytesMut,
    ) -> Result<postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        out.extend_from_slice(self.0.as_bytes());
        Ok(postgres::types::IsNull::No)
    }
    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
    fn encode_format(&self, _: &postgres::types::Type) -> postgres::types::Format {
        postgres::types::Format::Text
    }
    postgres::types::to_sql_checked!();
}

/// A text parameter typed regproc or regprocedure in a Bind names a function as the session finds it: one made here
/// too, not only a built-in (once 42883), as Postgres reads it.
#[test]
fn a_text_regproc_parameter_names_a_function_made_here() {
    let (_server, port) = start("regproc_param", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute("create function rp_param() returns int language sql as 'select 1'").unwrap();
    let regproc = postgres::types::Type::from_oid(24).unwrap();
    let regprocedure = postgres::types::Type::from_oid(2202).unwrap();
    let stmt = c.prepare_typed("select $1::text, $2::text", &[regproc, regprocedure]).unwrap();
    let r = c.query_one(&stmt, &[&Text("rp_param"), &Text("rp_param()")]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("rp_param".into(), "rp_param()".into()));
    let r = c.query_one(&stmt, &[&Text("now"), &Text("int4in(cstring)")]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("now".into(), "int4in(cstring)".into()));
}

/// A result of many regprocs goes out in binary as fast as in text: each value is its function's oid already, where
/// once every one read the whole catalog again to find it.
#[test]
fn many_regprocs_over_the_wire() {
    let (_server, port) = start("regprocs", None);
    let mut c = connect(port, "").unwrap();
    let tables: String = (0..100).map(|i| format!("create table rpw{i} (a int, b text);")).collect();
    c.batch_execute(&tables).unwrap();
    let q = "select typinput, typoutput, typreceive, typsend, typinput::oid, typoutput::oid, typreceive::oid, \
             typsend::oid from pg_type, generate_series(1, 20)";
    let t = std::time::Instant::now();
    let rows = c.query(q, &[]).unwrap();
    let took = t.elapsed();
    assert!(rows.len() >= 1000, "{}", rows.len());
    for r in &rows {
        for i in 0..4 {
            let Raw(b) = r.get(i);
            assert_eq!(u32::from_be_bytes(b.try_into().unwrap()), r.get::<_, u32>(i + 4));
        }
    }
    assert!(took < std::time::Duration::from_secs(5), "{took:?} for {} rows", rows.len());
}

/// A one-dimension binary array as Postgres sends it: its element type, lower bound and elements (None: null).
fn wire_array(b: &[u8]) -> (u32, i32, Vec<Option<Vec<u8>>>) {
    let int = |i: usize| i32::from_be_bytes(b[i..i + 4].try_into().unwrap());
    let elem = int(8) as u32;
    if int(0) == 0 {
        return (elem, 1, vec![]);
    }
    let (n, lower, mut at, mut items) = (int(12), int(16), 20, vec![]);
    for _ in 0..n {
        let len = int(at);
        at += 4;
        if len < 0 {
            items.push(None);
        } else {
            items.push(Some(b[at..at + len as usize].to_vec()));
            at += len as usize;
        }
    }
    (elem, lower, items)
}

/// regproc[] and regprocedure[] reach a driver as Postgres sends them: typed 1008 and 2207, the functions' names in
/// text, and in binary an array of their oids (element type 24 and 2202); a parameter of either type names functions.
/// The catalog's array columns go as arrays: `proargnames` as text[] (1009), `proallargtypes` as oid[] (1028).
#[test]
fn regproc_arrays_over_the_wire() {
    let (_server, port) = start("regproc_arrays", None);
    let mut c = connect(port, "").unwrap();
    let q = "select array['now'::regproc, 'int4in'], array['abs(int)'::regprocedure], \
             array_agg(typinput order by typinput) from pg_type where typname in ('int4', 'bool')";
    let stmt = c.prepare(q).unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [1008, 2207, 1008], "regproc[], regprocedure[], regproc[]");
    let r = c.query_one(&stmt, &[]).unwrap();
    let oids = |b: Raw| {
        let (elem, lower, items) = wire_array(&b.0);
        let items: Vec<u32> = items.into_iter().map(|i| u32::from_be_bytes(i.unwrap().try_into().unwrap())).collect();
        (elem, lower, items)
    };
    assert_eq!(oids(r.get(0)), (24, 1, vec![1299, 42]));
    assert_eq!(oids(r.get(1)), (2202, 1, vec![1397]));
    assert_eq!(oids(r.get(2)), (24, 1, vec![42, 1242]));
    let rows = c.simple_query(q).unwrap();
    let row =
        rows.iter().find_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { Some(r) } else { None }).unwrap();
    let got: Vec<Option<&str>> = (0..3).map(|i| row.get(i)).collect();
    assert_eq!(got, [Some("{now,int4in}"), Some("{abs(integer)}"), Some("{int4in,boolin}")]);

    // a parameter, in text
    let regprocs = postgres::types::Type::from_oid(1008).unwrap();
    let stmt = c.prepare_typed("select $1::oid[]::text, $1::text", &[regprocs]).unwrap();
    let r = c.query_one(&stmt, &[&Text("{now,int4in}")]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("{1299,42}".into(), "{now,int4in}".into()));

    // the catalog's arrays
    let q = "select proargnames, proallargtypes, proargtypes from pg_proc where oid = 'aclexplode'::regproc";
    let stmt = c.prepare(q).unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [1009, 1028, 30], "text[], oid[], oidvector");
    let r = c.query_one(&stmt, &[]).unwrap();
    let names: Vec<String> = r.get(0);
    assert_eq!(names, ["acl", "grantor", "grantee", "privilege_type", "is_grantable"]);
    assert_eq!(oids(r.get(1)), (26, 1, vec![1034, 26, 26, 25, 16]));
}

/// oid and smallint reach a driver as their own types: oid as 26, four unsigned bytes in binary (4294967295 is
/// `ff ff ff ff`, where a bigint's eight bytes were sent), smallint as 21, two bytes; as parameters too, and the
/// catalog's oid and int2 columns the same.
#[test]
fn oid_and_smallint_over_the_wire() {
    let (_server, port) = start("oid_int2", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute(
        "create table wi (id int primary key, o oid, a smallint);
         insert into wi values (1, 4294967295, -16384), (2, 42, 7);",
    )
    .unwrap();
    let stmt = c.prepare("select o, a, a + a, 1::int2, o::int8 from wi order by id").unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [26, 21, 21, 21, 20], "oid, smallint, smallint, smallint, bigint");
    let rows = c.query(&stmt, &[]).unwrap();
    assert_eq!(rows[0].get::<_, Raw>(0).0, vec![0xff, 0xff, 0xff, 0xff]);
    let got: Vec<(u32, i16, i16, i16, i64)> =
        rows.iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4))).collect();
    assert_eq!(got, [(4294967295, -16384, -32768, 1, 4294967295), (42, 7, 14, 1, 42)]);
    // parameters of both types, in binary
    let r = c.query_one("select $1::oid, $2::smallint + 1::smallint", &[&4294967295u32, &5i16]).unwrap();
    assert_eq!((r.get::<_, u32>(0), r.get::<_, i16>(1)), (4294967295, 6));
    let r = c.query_one("select n from (values (4294967295::oid, 'max')) v(o, n) where o = $1", &[&4294967295u32]);
    assert_eq!(r.unwrap().get::<_, String>(0), "max");
    // in the simple protocol, as text
    let rows = c.simple_query("select o, a from wi where id = 1").unwrap();
    let row =
        rows.iter().find_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { Some(r) } else { None }).unwrap();
    assert_eq!((row.get(0), row.get(1)), (Some("4294967295"), Some("-16384")));
    // the catalog's
    let stmt = c.prepare("select c.oid, c.relnatts, a.attnum, a.attlen from pg_class c join pg_attribute a on a.attrelid = c.oid where c.relname = 'wi' and a.attnum = 1").unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [26, 21, 21, 21], "oid, int2, int2, int2");
    let r = c.query_one(&stmt, &[]).unwrap();
    assert_eq!((r.get::<_, i16>(1), r.get::<_, i16>(2), r.get::<_, i16>(3)), (3, 1, 4));
}

/// ARRAY[smallint, integer] is an integer[] in binary (1007, four bytes an element), an oid[] written as text goes
/// out as the oids it reads as, and an int4 parameter compared with an oid is one (-1 is 4294967295), an int8's
/// -1 22003.
#[test]
fn widened_arrays_and_oid_parameters_over_the_wire() {
    let (_server, port) = start("int_arrays", None);
    let mut c = connect(port, "").unwrap();
    let stmt = c.prepare("select array[1::int2, 70000], '{-1,0x10}'::oid[], array[1::oid, -1]").unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [1007, 1028, 1028], "integer[], oid[], oid[]");
    let r = c.query_one(&stmt, &[]).unwrap();
    assert_eq!(r.get::<_, Vec<i32>>(0), vec![1, 70000]);
    assert_eq!(r.get::<_, Vec<u32>>(1), vec![4294967295, 16]);
    assert_eq!(r.get::<_, Vec<u32>>(2), vec![1, 4294967295]);
    c.batch_execute("create table wo (id int primary key, o oid); insert into wo values (1, 4294967295), (2, 5);")
        .unwrap();
    let stmt = c.prepare_typed("select id from wo where o = $1", &[postgres::types::Type::INT4]).unwrap();
    let ids: Vec<i32> = c.query(&stmt, &[&-1i32]).unwrap().iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, [1]);
    let stmt = c.prepare_typed("select id from wo where o = $1", &[postgres::types::Type::INT8]).unwrap();
    let e = c.query(&stmt, &[&-1i64]).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::NUMERIC_VALUE_OUT_OF_RANGE), "{e}");
}

/// The reg types reach a driver as Postgres sends them: each its own type id (regclass 2205, regtype 2206,
/// regnamespace 4089, regrole 4096, regoper 2203, regoperator 2204, regconfig 3734, regdictionary 3769,
/// regcollation 4191), its name in text and its object's oid in binary; their arrays as arrays of oids, and a
/// parameter of one, in text or binary, read as one; `pg_prepared_statements`' type lists as regtype[].
#[test]
fn reg_types_over_the_wire() {
    let (_server, port) = start("reg_types", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute("create table rw_t (id int primary key)").unwrap();
    let q = "select 'pg_class'::regclass, 'int4'::regtype, 'pg_catalog'::regnamespace, 'chronos'::regrole, \
             '||/'::regoper, '+(int4,int4)'::regoperator, 'english'::regconfig, 'simple'::regdictionary, \
             '\"C\"'::regcollation, 'rw_t'::regclass, 'rw_t'::regclass::oid";
    let stmt = c.prepare(q).unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [2205, 2206, 4089, 4096, 2203, 2204, 3734, 3769, 4191, 2205, 26]);
    let r = c.query_one(&stmt, &[]).unwrap();
    let oid = |n: u32| Raw(n.to_be_bytes().to_vec());
    let got: Vec<Raw> = (0..9).map(|i| r.get::<_, Raw>(i)).collect();
    assert_eq!(got, [1259, 23, 11, 10, 597, 551, 13639, 3765, 950].map(oid));
    let mine: u32 = r.get(10);
    assert_eq!(r.get::<_, Raw>(9), oid(mine), "a table made here: its own oid");
    let rows = c.simple_query(q).unwrap();
    let row =
        rows.iter().find_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { Some(r) } else { None }).unwrap();
    let got: Vec<Option<&str>> = (0..10).map(|i| row.get(i)).collect();
    let want = ["pg_class", "integer", "pg_catalog", "chronos", "||/", "+(integer,integer)", "english", "simple"];
    assert_eq!(got[..8], want.map(Some));
    assert_eq!(got[8..], [Some("\"C\""), Some("rw_t")]);

    // arrays: typed as Postgres's, oids in binary
    let q = "select '{pg_class,pg_type}'::regclass[], '{int4,text}'::regtype[], array['public'::regnamespace]";
    let stmt = c.prepare(q).unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [2210, 2211, 4090]);
    let r = c.query_one(&stmt, &[]).unwrap();
    let oids = |b: Raw| {
        let (elem, lower, items) = wire_array(&b.0);
        let items: Vec<u32> = items.into_iter().map(|i| u32::from_be_bytes(i.unwrap().try_into().unwrap())).collect();
        (elem, lower, items)
    };
    assert_eq!(oids(r.get(0)), (2205, 1, vec![1259, 1247]));
    assert_eq!(oids(r.get(1)), (2206, 1, vec![23, 25]));
    assert_eq!(oids(r.get(2)), (4089, 1, vec![2200]));

    // parameters: a name in text, an oid in binary
    let regclass = postgres::types::Type::from_oid(2205).unwrap();
    let regtype = postgres::types::Type::from_oid(2206).unwrap();
    let stmt = c.prepare_typed("select $1::oid, $2::text, $1::text", &[regclass.clone(), regtype.clone()]).unwrap();
    let r = c.query_one(&stmt, &[&Text("rw_t"), &Text("int8")]).unwrap();
    assert_eq!(
        (r.get::<_, u32>(0), r.get::<_, String>(1), r.get::<_, String>(2)),
        (mine, "bigint".into(), "rw_t".into())
    );
    let stmt = c.prepare_typed("select $1::text, $2::text", &[regclass, regtype]).unwrap();
    let r = c.query_one(&stmt, &[&RawOid(1259), &RawOid(25)]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("pg_class".into(), "text".into()));

    // pg_prepared_statements' type lists: regtype[], the types' oids in binary
    c.batch_execute("prepare rw_p (int, text, regclass) as select 1").unwrap();
    let stmt = c.prepare("select parameter_types from pg_prepared_statements where name = 'rw_p'").unwrap();
    assert_eq!(stmt.columns()[0].type_().oid(), 2211);
    let r = c.query_one(&stmt, &[]).unwrap();
    assert_eq!(oids(r.get(0)), (2206, 1, vec![23, 25, 2205]));
}

/// An oid parameter sent in binary as whatever type the statement declared (a reg type's binary form is its oid's).
#[derive(Debug)]
struct RawOid(u32);

impl postgres::types::ToSql for RawOid {
    fn to_sql(
        &self,
        _: &postgres::types::Type,
        out: &mut postgres::types::private::BytesMut,
    ) -> Result<postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        out.extend_from_slice(&self.0.to_be_bytes());
        Ok(postgres::types::IsNull::No)
    }
    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
    postgres::types::to_sql_checked!();
}

/// bytea over the wire: parameters and results as bytes in binary (a driver's `Vec<u8>`), as `\x`
/// hex text in the simple protocol and psql, and through COPY.
#[test]
fn bytea_over_the_wire() {
    let (_server, port) = start("bytea", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute("create table blobs (id int primary key, data bytea, tag text)").unwrap();
    let all: Vec<u8> = (0..=255).collect();
    c.execute("insert into blobs values ($1, $2, $3)", &[&1i32, &all, &"all"]).unwrap();
    c.execute("insert into blobs values ($1, $2, $3)", &[&2i32, &Vec::<u8>::new(), &"empty"]).unwrap();
    c.execute("insert into blobs values ($1, $2, $3)", &[&3i32, &None::<Vec<u8>>, &"null"]).unwrap();
    // in binary, byte for byte
    let rows = c.query("select id, data from blobs order by id", &[]).unwrap();
    assert_eq!(rows[0].get::<_, Vec<u8>>(1), all);
    assert_eq!(rows[1].get::<_, Vec<u8>>(1), Vec::<u8>::new());
    assert_eq!(rows[2].get::<_, Option<Vec<u8>>>(1), None);
    // a parameter compared, and a function of one
    let r = c.query_one("select id from blobs where data = $1", &[&all]).unwrap();
    assert_eq!(r.get::<_, i32>(0), 1);
    let r = c.query_one("select length(data), encode(data, 'hex') from blobs where id = 1", &[]).unwrap();
    assert_eq!(r.get::<_, i32>(0), 256);
    assert!(r.get::<_, String>(1).starts_with("000102"));
    let r = c.query_one("select $1::bytea || '\\x02'::bytea", &[&vec![1u8]]).unwrap();
    assert_eq!(r.get::<_, Vec<u8>>(0), vec![1u8, 2]);
    // the simple protocol says it in text
    let rows = c.simple_query("select data from blobs where id = 1 or id = 2 order by id").unwrap();
    let texts: Vec<String> = rows
        .iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => r.get(0).map(String::from),
            _ => None,
        })
        .collect();
    assert_eq!(texts[0].len(), 2 + 512);
    assert!(texts[0].starts_with("\\x000102") && texts[1] == "\\x", "{texts:?}");
    // COPY writes and reads the same text
    let mut w = c.copy_in("COPY blobs (id, data) FROM STDIN").unwrap();
    std::io::Write::write_all(&mut w, b"10\t\\\\xdeadbeef\n11\t\\\\x\n").unwrap();
    assert_eq!(w.finish().unwrap(), 2);
    let mut out = String::new();
    std::io::Read::read_to_string(
        &mut c.copy_out("COPY (select id, data from blobs where id >= 10 order by id) TO STDOUT").unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(out, "10\t\\\\xdeadbeef\n11\t\\\\x\n");
    let r = c.query_one("select data from blobs where id = 10", &[]).unwrap();
    assert_eq!(r.get::<_, Vec<u8>>(0), vec![0xde, 0xad, 0xbe, 0xef]);
    // the column's type as a client sees it
    let stmt = c.prepare("select data from blobs").unwrap();
    assert_eq!(stmt.columns()[0].type_().name(), "bytea");
}

/// A record in binary, as Postgres sends a composite: the number of fields, then each one's type id, length
/// (-1 for null) and bytes. `_pg_expandarray` gives records; the `postgres` crate asks for binary results.
#[test]
fn a_record_over_the_wire_in_binary() {
    let (_server, port) = start("record_binary", None);
    let mut c = connect(port, "").unwrap();
    let be = |n: i32| n.to_be_bytes().to_vec();
    // ints: (10,1)
    let row = c.query_one("select information_schema._pg_expandarray(array[10,20]) limit 1", &[]).unwrap();
    let got: Raw = row.get(0);
    assert_eq!(got.0, [be(2), be(23), be(4), be(10), be(23), be(4), be(1)].concat());
    // text: a field with a space and a comma, in text type 25
    let row = c.query_one("select information_schema._pg_expandarray(array['a b','c,d']) limit 1", &[]).unwrap();
    let got: Raw = row.get(0);
    assert_eq!(got.0, [be(2), be(25), be(3), b"a b".to_vec(), be(23), be(4), be(1)].concat());
    // a jsonb element in FROM is a typed value in binary, not its text
    let row =
        c.query_one("select x from information_schema._pg_expandarray(array['{\"a\": 1}'::jsonb]) as t", &[]).unwrap();
    let got: Raw = row.get(0);
    assert_eq!(got.0[0], 1, "jsonb's version byte");
    assert_eq!(String::from_utf8_lossy(&got.0[1..]).replace(' ', ""), r#"{"a":1}"#);
    // the same through the text protocol
    let rows = c.simple_query("select information_schema._pg_expandarray(array['a b','c,d'])").unwrap();
    let texts: Vec<&str> = rows
        .iter()
        .filter_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { r.get(0) } else { None })
        .collect();
    assert_eq!(texts, ["(\"a b\",1)", "(\"c,d\",2)"]);
}

/// A function returning void reaches a driver as Postgres sends it: typed void (2278), its value empty and not null,
/// in text and in binary; a SQL-language one's is null, as Postgres's is.
#[test]
fn void_over_the_wire() {
    let (_server, port) = start("void", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute(
        "create function vw_pl() returns void language plpgsql as 'begin end'; \
         create function vw_sql() returns void language sql as 'select 1'",
    )
    .unwrap();
    let q = "select pg_sleep(0), pg_notify('vw', 'x'), vw_pl(), vw_sql()";
    let stmt = c.prepare(q).unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [2278; 4]);
    let r = c.query_one(&stmt, &[]).unwrap();
    let got: Vec<Option<Raw>> = (0..4).map(|i| r.get(i)).collect();
    assert_eq!(got, [Some(Raw(vec![])), Some(Raw(vec![])), Some(Raw(vec![])), None]);
    let rows = c.simple_query(q).unwrap();
    let row =
        rows.iter().find_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { Some(r) } else { None }).unwrap();
    let got: Vec<Option<&str>> = (0..4).map(|i| row.get(i)).collect();
    assert_eq!(got, [Some(""), Some(""), Some(""), None]);
    let stmt = c.prepare("select * from vw_pl()").unwrap();
    assert_eq!(stmt.columns()[0].type_().oid(), 2278);
    // the built-in ones: the advisory locks that wait, pg_advisory_unlock_all, setseed
    let q = "select pg_advisory_lock(1), pg_advisory_unlock_all(), setseed(0.5)";
    let stmt = c.prepare(q).unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [2278; 3]);
    let r = c.query_one(&stmt, &[]).unwrap();
    let got: Vec<Option<Raw>> = (0..3).map(|i| r.get(i)).collect();
    assert_eq!(got, [Some(Raw(vec![])), Some(Raw(vec![])), Some(Raw(vec![]))]);
}

/// A parameter a driver sends no type for is of unknown type to the overload a call of it chooses, as in Postgres
/// (`m3(1, $1)` takes `m3(int, int)`), on Parse, Describe and Execute alike.
#[test]
fn untyped_parameters_choose_overloads_over_the_wire() {
    let (_server, port) = start("untyped", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute(
        "create function m3(a int, d int) returns text language sql as $$select 'int,int'$$; \
         create function m3(a text, d int) returns text language sql as $$select 'text,int'$$; \
         create function v(date) returns text language sql as $$select 'date'$$; \
         create function v(timestamptz) returns text language sql as $$select 'tstz'$$",
    )
    .unwrap();
    let stmt = c.prepare("select m3(1, $1)").unwrap();
    let r = c.query_one(&stmt, &[&"5"]).unwrap();
    assert_eq!(r.get::<_, String>(0), "int,int");
    let rows = c.query("select m3(1, $1) union all select v($2)", &[&"5", &"2020-01-01"]).unwrap();
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["int,int", "tstz"]);
}

/// A simple query's RowDescription: each column's name, type oid and typmod.
fn row_description(port: u16, sql: &str) -> Vec<(String, u32, i32)> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let body = [&196608i32.to_be_bytes()[..], b"user\0tester\0database\0main\0\0"].concat();
    s.write_all(&[&(body.len() as i32 + 4).to_be_bytes()[..], &body].concat()).unwrap();
    let read = |s: &mut std::net::TcpStream| {
        let mut head = [0u8; 5];
        s.read_exact(&mut head).unwrap();
        let mut body = vec![0; i32::from_be_bytes(head[1..].try_into().unwrap()) as usize - 4];
        s.read_exact(&mut body).unwrap();
        (head[0], body)
    };
    while read(&mut s).0 != b'Z' {}
    let q = [sql.as_bytes(), b"\0"].concat();
    s.write_all(&[&[b'Q'][..], &(q.len() as i32 + 4).to_be_bytes(), &q].concat()).unwrap();
    let mut out = vec![];
    loop {
        match read(&mut s) {
            (b'T', b) => {
                let mut at = 2;
                for _ in 0..i16::from_be_bytes([b[0], b[1]]) {
                    let end = at + b[at..].iter().position(|&c| c == 0).unwrap();
                    let name = String::from_utf8_lossy(&b[at..end]).into_owned();
                    let f = &b[end + 1..];
                    let oid = u32::from_be_bytes(f[6..10].try_into().unwrap());
                    let typmod = i32::from_be_bytes(f[12..16].try_into().unwrap());
                    out.push((name, oid, typmod));
                    at = end + 1 + 18;
                }
            }
            (b'E', b) => panic!("{sql}: {}", String::from_utf8_lossy(&b)),
            (b'Z', _) => return out,
            _ => {}
        }
    }
}

/// varchar, varchar(n), char(n) and their arrays reach a client as Postgres sends them: RowDescription's type oid
/// (1043, 1042, 1015, 1014) and typmod (n + 4, -1 for a varchar of no length), in a simple query, a Describe, a
/// RETURNING and a cursor's FETCH; their values text's (an array's elements, in binary, of the array's element type).
/// A parameter written into a varchar or char(n) column, or compared with a char(n) one, is described as of its type.
#[test]
fn varchar_and_char_types_over_the_wire() {
    let (_server, port) = start("varchar_types", None);
    let mut c = connect(port, "").unwrap();
    c.batch_execute(
        "create table vw (id int primary key, a varchar, b varchar(10), c char(3), d varchar(5)[], e char(2)[], f text);
         insert into vw values (1, 'x', 'y', 'z', '{q}', '{r}', 't')",
    )
    .unwrap();
    let typed = |sql: &str| -> Vec<(u32, i32)> { row_description(port, sql).into_iter().map(|c| (c.1, c.2)).collect() };
    assert_eq!(
        typed("select a, b, c, d, e, f, b || 'x', coalesce(b, b), b::varchar(4), f::char(2), f::varchar from vw"),
        [
            (1043, -1),
            (1043, 14),
            (1042, 7),
            (1015, 9),
            (1014, 6),
            (25, -1),
            (25, -1),
            (1043, 14),
            (1043, 8),
            (1042, 6),
            (1043, -1)
        ]
    );
    assert_eq!(typed("select b from vw union select b from vw"), [(1043, 14)]);
    assert_eq!(typed("select x from (select c from vw) s(x)"), [(1042, 7)]);
    assert_eq!(typed("update vw set a = a returning a, b, c, f"), [(1043, -1), (1043, 14), (1042, 7), (25, -1)]);
    assert_eq!(
        typed("begin; declare k cursor for select b, f from vw; fetch k"),
        [(1043, 14), (25, -1)],
        "a cursor's rows"
    );
    // a Describe (the driver's prepare) says the same
    let stmt = c.prepare("select a, b, c, d, e, f from vw").unwrap();
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [1043, 1043, 1042, 1015, 1014, 25]);
    let r = c.query_one(&stmt, &[]).unwrap();
    let got: (String, String, String, Vec<String>, Vec<String>) = (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4));
    assert_eq!(got, ("x".into(), "y".into(), "z  ".into(), vec!["q".to_string()], vec!["r ".to_string()]));
    // parameters
    let stmt = c.prepare("insert into vw (id, a, b, c, f) values ($1, $2, $3, $4, $5) returning a, c").unwrap();
    let params: Vec<u32> = stmt.params().iter().map(|t| t.oid()).collect();
    assert_eq!(params, [23, 1043, 1043, 1042, 25]);
    let ids: Vec<u32> = stmt.columns().iter().map(|c| c.type_().oid()).collect();
    assert_eq!(ids, [1043, 1042]);
    let r = c.query_one(&stmt, &[&2i32, &"p", &"q", &"r", &"s"]).unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, String>(1)), ("p".into(), "r  ".into()));
    let stmt = c.prepare("select id from vw where c = $1 and a = $2 and b = $3").unwrap();
    let params: Vec<u32> = stmt.params().iter().map(|t| t.oid()).collect();
    assert_eq!(params, [1042, 25, 25], "bpchar = bpchar; varchar = text, as texteq takes it");
    let ids: Vec<i32> = c.query(&stmt, &[&"r", &"p", &"q"]).unwrap().iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, [2]);
    let stmt = c.prepare("update vw set b = $1 where id = $2").unwrap();
    assert_eq!(stmt.params().iter().map(|t| t.oid()).collect::<Vec<_>>(), [1043, 23]);
}
