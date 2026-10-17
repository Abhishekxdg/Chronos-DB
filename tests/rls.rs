//! Row-level security where a real connection, psql, pg_dump or a world matters (the SQL is
//! checked against Postgres by `pgdiff.rs` `row_security_*`): tenants logged in as roles, COPY,
//! psql's `\d` and `\dp`, a dump that restores, and what forks, merges and DIFF do with policies.

mod common;

use chronos::Db;
use chronos::sql::{Session, Value};
use postgres::error::SqlState;
use postgres::{Client, NoTls};
use std::io::Read;
use std::process::{Child, Command};

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(name: &str) -> (Server, u16) {
    let dir = std::env::temp_dir().join(format!("chronos-rls-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    let s = common::serve(&mut cmd, true);
    (Server(s.child, dir), s.pg)
}

fn connect(port: u16, user: &str) -> Result<Client, postgres::Error> {
    Client::connect(&format!("host=127.0.0.1 port={port} user={user} dbname=main"), NoTls)
}

/// A server error's message.
fn message(e: &postgres::Error) -> String {
    e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string())
}

fn ids(c: &mut Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).unwrap().iter().map(|r| r.get(0)).collect()
}

#[test]
fn tenants_see_only_their_rows_over_real_connections() {
    let (_server, port) = start("tenants");
    let mut admin = connect(port, "admin").unwrap();
    admin
        .batch_execute(
            "create role r1b_t1 login; create role r1b_t2 login;
             create table notes (id int primary key, tenant text not null default current_user, body text);
             grant select, insert, update, delete on notes to r1b_t1, r1b_t2;
             alter table notes enable row level security;
             create policy tenant on notes using (tenant = current_user);
             insert into notes values (1, 'r1b_t1', 'one'), (2, 'r1b_t2', 'two'), (3, 'r1b_t1', 'three')",
        )
        .unwrap();
    let mut t1 = connect(port, "r1b_t1").unwrap();
    let mut t2 = connect(port, "r1b_t2").unwrap();
    assert_eq!(ids(&mut t1, "select id from notes order by id"), [1, 3]);
    assert_eq!(ids(&mut t2, "select id from notes order by id"), [2]);
    // a prepared statement, run with parameters (the extended protocol)
    let st = t1.prepare("select id from notes where id = $1").unwrap();
    assert!(t1.query(&st, &[&2i32]).unwrap().is_empty());
    assert_eq!(t1.query(&st, &[&1i32]).unwrap().len(), 1);
    // writes: each tenant's own rows only
    t2.execute("insert into notes (id, body) values (4, 'four')", &[]).unwrap();
    let e = t2.execute("insert into notes values (5, 'r1b_t1', 'x')", &[]).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
    assert_eq!(message(&e), "new row violates row-level security policy for table \"notes\"");
    assert_eq!(t2.execute("update notes set body = body || '!'", &[]).unwrap(), 2);
    assert_eq!(t1.execute("delete from notes where id = 2", &[]).unwrap(), 0);
    assert_eq!(ids(&mut t2, "select id from notes order by id"), [2, 4]);
    // COPY TO reads what the policies let it; COPY FROM is refused for a role they apply to
    let mut out = String::new();
    t1.copy_out("copy notes (id, body) to stdout").unwrap().read_to_string(&mut out).unwrap();
    assert_eq!(out, "1\tone\n3\tthree\n");
    let e = t1.copy_in::<_>("copy notes from stdin").err().unwrap();
    assert_eq!(e.code(), Some(&SqlState::FEATURE_NOT_SUPPORTED), "{e}");
    assert_eq!(message(&e), "COPY FROM not supported with row-level security");
    // (a connection whose COPY FROM failed can't run another statement: a client and server issue of its own)
    drop(t1);
    let mut t1 = connect(port, "r1b_t1").unwrap();
    // a superuser sees every row
    assert_eq!(ids(&mut admin, "select id from notes order by id"), [1, 2, 3, 4]);

    // the other common pattern: the tenant is a setting each session makes
    admin
        .batch_execute(
            "create table orders (id int primary key, org int, total int);
             grant select, insert on orders to r1b_t1, r1b_t2;
             alter table orders enable row level security;
             create policy org on orders using (org = current_setting('app.org_id')::int);
             insert into orders values (1, 10, 5), (2, 20, 7), (3, 10, 9)",
        )
        .unwrap();
    t1.batch_execute("set app.org_id = '10'").unwrap();
    t2.batch_execute("set app.org_id = '20'").unwrap();
    assert_eq!(ids(&mut t1, "select id from orders order by id"), [1, 3]);
    assert_eq!(ids(&mut t2, "select id from orders order by id"), [2]);
    assert_eq!(t1.query_one("select sum(total)::int from orders", &[]).unwrap().get::<_, i32>(0), 14);
    let e = t2.execute("insert into orders values (4, 10, 1)", &[]).unwrap_err();
    assert_eq!(message(&e), "new row violates row-level security policy for table \"orders\"");
    t2.execute("insert into orders values (4, 20, 1)", &[]).unwrap();
    assert_eq!(ids(&mut t2, "select id from orders order by id"), [2, 4]);
}

#[test]
fn psql_shows_policies() {
    if Command::new("psql").arg("--version").output().is_err() {
        eprintln!("skipped: psql not installed");
        return;
    }
    let (_server, port) = start("psql");
    let url = format!("postgres://admin@127.0.0.1:{port}/main");
    let psql = |args: &[&str], sql: &str| {
        let out = Command::new("psql").arg(&url).args(["-X"]).args(args).args(["-c", sql]).output().unwrap();
        assert!(out.status.success(), "psql -c {sql}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    psql(
        &[],
        "create role r1b_a; create table t (id int primary key, owner text, v int);
         create policy p_all on t as restrictive to r1b_a using (v > 5);
         create policy p_ins on t for insert to r1b_a with check (owner = current_user);
         create policy p_sel on t for select to r1b_a",
    );
    let d = psql(&[], "\\d t");
    assert!(d.contains("Policies (row security disabled):\n    POLICY \"p_all\" AS RESTRICTIVE\n"), "{d}");
    psql(&[], "alter table t enable row level security");
    let d = psql(&[], "\\d t");
    let policies = "Policies:\n    POLICY \"p_all\" AS RESTRICTIVE\n      TO r1b_a\n      USING ((v > 5))\n    \
                    POLICY \"p_ins\" FOR INSERT\n      TO r1b_a\n      WITH CHECK ((owner = CURRENT_USER))\n    \
                    POLICY \"p_sel\" FOR SELECT\n      TO r1b_a\n";
    assert!(d.contains(policies), "{d}");
    psql(&[], "alter table t force row level security");
    let d = psql(&[], "\\d t");
    assert!(d.contains("Policies (forced row security enabled):\n"), "{d}");
    psql(&[], "create table u (id int); alter table u enable row level security");
    let d = psql(&[], "\\d u");
    assert!(d.contains("Policies (row security enabled): (none)"), "{d}");
    let dp = psql(&["-A", "-t"], "\\dp t");
    for policy in [
        "p_sel (r):\n  to: r1b_a",
        "p_ins (a):\n  (c): (owner = CURRENT_USER)\n  to: r1b_a",
        "p_all (RESTRICTIVE):\n  (u): (v > 5)\n  to: r1b_a",
    ] {
        assert!(dp.contains(policy), "{policy} not in {dp}");
    }
}

#[test]
fn pg_dump_writes_policies_that_restore() {
    if Command::new("pg_dump").arg("--version").output().is_err() {
        eprintln!("skipped: pg_dump not installed");
        return;
    }
    let (_a, pa) = start("dump-a");
    let (_b, pb) = start("dump-b");
    let roles = "create role r1b_app login; create role r1b_ro";
    connect(pa, "admin")
        .unwrap()
        .batch_execute(&format!(
            "{roles}; create schema s; create table s.t (id int primary key, owner text, v int);
             insert into s.t values (1, 'r1b_app', 1);
             alter table s.t enable row level security; alter table s.t force row level security;
             create policy p_all on s.t as restrictive to r1b_app, r1b_ro using (v > 0);
             create policy p_ins on s.t for insert to r1b_app with check (owner = current_user);
             create policy p_sel on s.t for select using (true);
             create table s.u (id int); alter table s.u enable row level security"
        ))
        .unwrap();
    let dump = |port: u16| {
        let out = Command::new("pg_dump")
            .args(["-h", "127.0.0.1", "-p", &port.to_string(), "-U", "postgres", "main"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .filter(|l| !l.starts_with("\\restrict") && !l.starts_with("\\unrestrict"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let first = dump(pa);
    for line in [
        "SET row_security = off;",
        "ALTER TABLE s.t ENABLE ROW LEVEL SECURITY;",
        "ALTER TABLE ONLY s.t FORCE ROW LEVEL SECURITY;",
        "ALTER TABLE s.u ENABLE ROW LEVEL SECURITY;",
        "CREATE POLICY p_all ON s.t AS RESTRICTIVE TO r1b_app, r1b_ro USING ((v > 0));",
        "CREATE POLICY p_ins ON s.t FOR INSERT TO r1b_app WITH CHECK ((owner = CURRENT_USER));",
        "CREATE POLICY p_sel ON s.t FOR SELECT USING (true);",
    ] {
        assert!(first.contains(line), "{line} not in:\n{first}");
    }
    // the roles first (pg_dumpall's part), then the database: the same dump again
    connect(pb, "admin").unwrap().batch_execute(roles).unwrap();
    let file = std::env::temp_dir().join(format!("chronos-rls-{}-dump.sql", std::process::id()));
    std::fs::write(&file, &first).unwrap();
    let out = Command::new("psql")
        .args(["-X", "-q", "-h", "127.0.0.1", "-p", &pb.to_string(), "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-f"])
        .arg(&file)
        .arg("main")
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&file);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(dump(pb), first);
    // and they work there
    let mut b = connect(pb, "admin").unwrap();
    b.batch_execute("grant usage on schema s to r1b_app; grant select on s.t to r1b_app").unwrap();
    let mut app = connect(pb, "r1b_app").unwrap();
    assert_eq!(ids(&mut app, "select id from s.t"), [1]);
    b.batch_execute("update s.t set v = 0").unwrap();
    assert!(ids(&mut app, "select id from s.t").is_empty());
}

fn ok(db: &Db, s: &mut Session, sql: &str) {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Db, s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    let out = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    out.rows
        .iter()
        .map(|r| r.iter().map(|v| if *v == Value::Null { "NULL".into() } else { v.to_text() }).collect())
        .collect()
}

#[test]
fn policies_fork_merge_and_diff_with_their_world() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r1b_a",
        "create table t (id int primary key, owner text)",
        "insert into t values (1, 'r1b_a'), (2, 'x')",
        "grant select on t to r1b_a",
        "create world w",
    ] {
        ok(&db, &mut s, q);
    }
    let count = "select count(*) from t";
    // a world's policies are its own until it merges
    let mut w = Session::new("w");
    ok(&db, &mut w, "alter table t enable row level security");
    ok(&db, &mut w, "create policy own on t using (owner = current_user)");
    ok(&db, &mut w, "set role r1b_a");
    ok(&db, &mut s, "set role r1b_a");
    assert_eq!(rows(&db, &mut w, count), [["1"]]);
    assert_eq!(rows(&db, &mut s, count), [["2"]]);
    ok(&db, &mut w, "reset role");
    ok(&db, &mut s, "reset role");
    // DIFF shows them, as SQL too
    let d = rows(&db, &mut s, "diff world w");
    assert!(d.iter().any(|r| r[0] == "t" && r[2] == "enable row level security"), "{d:?}");
    assert!(d.iter().any(|r| r[0] == "t" && r[2] == "create policy"), "{d:?}");
    let sql: Vec<String> = rows(&db, &mut s, "diff world w as sql").into_iter().map(|r| r[0].clone()).collect();
    assert!(sql.contains(&"alter table t enable row level security;".to_string()), "{sql:?}");
    assert!(sql.contains(&"create policy own on t using ((owner = CURRENT_USER));".to_string()), "{sql:?}");
    // merged, they are main's
    ok(&db, &mut s, "merge world w");
    ok(&db, &mut s, "set role r1b_a");
    assert_eq!(rows(&db, &mut s, count), [["1"]]);
    ok(&db, &mut s, "reset role");
    // two worlds adding a policy each to one table merge without a conflict
    ok(&db, &mut s, "create world w2");
    ok(&db, &mut s, "create world w3");
    ok(&db, &mut Session::new("w2"), "create policy two on t using (id = 2)");
    ok(&db, &mut Session::new("w3"), "create policy three on t for insert with check (true)");
    ok(&db, &mut s, "merge world w2");
    ok(&db, &mut s, "merge world w3");
    assert_eq!(
        rows(&db, &mut s, "select policyname, cmd from pg_policies order by 1"),
        [["own", "ALL"], ["three", "INSERT"], ["two", "ALL"]]
    );
    ok(&db, &mut s, "set role r1b_a");
    assert_eq!(rows(&db, &mut s, count), [["2"]]);
    ok(&db, &mut s, "reset role");
    // a dropped policy, and disabling, show in a DIFF too
    ok(&db, &mut s, "create world w4");
    let mut w4 = Session::new("w4");
    ok(&db, &mut w4, "drop policy two on t");
    ok(&db, &mut w4, "alter table t disable row level security");
    let sql: Vec<String> = rows(&db, &mut s, "diff world w4 as sql").into_iter().map(|r| r[0].clone()).collect();
    assert!(sql.contains(&"drop policy two on t;".to_string()), "{sql:?}");
    assert!(sql.contains(&"alter table t disable row level security;".to_string()), "{sql:?}");
}

fn fails(db: &Db, s: &mut Session, sql: &str) -> (String, String, Option<String>) {
    let e = s.run(db, sql, &[]).expect_err(sql);
    (e.code.to_string(), e.message, e.info.and_then(|i| i.detail))
}

#[test]
fn policy_statements_refuse_as_postgres_does() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r1b_a",
        "create role r1b_c",
        "create table t (id int primary key, owner text, v int)",
        "create view vw as select 1 as x",
        "grant select on t to r1b_a",
        "create policy p1 on t using (owner = current_user)",
        "create policy p6 on t to r1b_c using (true)",
        "create policy p7 on t for select using (true)",
        "create policy p8 on t for insert with check (true)",
    ] {
        ok(&db, &mut s, q);
    }
    let refused = |code: &str, message: &str| (code.to_string(), message.to_string());
    for (q, want) in [
        ("create policy p1 on t using (true)", refused("42710", "policy \"p1\" for table \"t\" already exists")),
        (
            "create policy p9 on t for select using (true) with check (true)",
            refused("42601", "WITH CHECK cannot be applied to SELECT or DELETE"),
        ),
        (
            "create policy p9 on t for insert using (true)",
            refused("42601", "only WITH CHECK expression allowed for INSERT"),
        ),
        (
            "alter policy p7 on t with check (true)",
            refused("42601", "only USING expression allowed for SELECT, DELETE"),
        ),
        ("alter policy p8 on t using (true)", refused("42601", "only WITH CHECK expression allowed for INSERT")),
        ("alter policy nope on t using (true)", refused("42704", "policy \"nope\" for table \"t\" does not exist")),
        ("alter policy nope on t rename to p1", refused("42710", "policy \"p1\" for table \"t\" already exists")),
        ("drop policy nope on t", refused("42704", "policy \"nope\" for table \"t\" does not exist")),
        ("create policy p9 on vw using (true)", refused("42809", "\"vw\" is not a table")),
        (
            "create policy p9 on t using (v)",
            refused("42804", "argument of POLICY must be type boolean, not type integer"),
        ),
        (
            "create policy p9 on t using (count(*) > 1)",
            refused("42803", "aggregate functions are not allowed in policy expressions"),
        ),
        ("create policy p9 on t to r1b_nope using (true)", refused("42704", "role \"r1b_nope\" does not exist")),
    ] {
        let (code, message, _) = fails(&db, &mut s, q);
        assert_eq!((code, message), want, "{q}");
    }
    // only the table's owner may
    ok(&db, &mut s, "set role r1b_a");
    for (q, message) in [
        ("create policy p9 on t using (true)", "must be owner of table t"),
        ("alter policy p1 on t rename to p9", "must be owner of table t"),
        ("drop policy p1 on t", "must be owner of relation t"),
        ("alter table t enable row level security", "must be owner of table t"),
    ] {
        let (code, m, _) = fails(&db, &mut s, q);
        assert_eq!((code.as_str(), m.as_str()), ("42501", message), "{q}");
    }
    ok(&db, &mut s, "reset role");
    // a role a policy is for stays until the policy goes
    let (code, message, detail) = fails(&db, &mut s, "drop role r1b_c");
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("2BP01", "role \"r1b_c\" cannot be dropped because some objects depend on it")
    );
    assert!(detail.is_some_and(|d| d.contains("target of policy p6 on table t")));
    ok(&db, &mut s, "drop policy p6 on t");
    ok(&db, &mut s, "drop role r1b_c");
}

/// A temporary table can't stand in for the table a policy reads: what a policy reads is bound when
/// it is made (Postgres reads the table it was made with; here the read is refused).
#[test]
fn a_temporary_table_cant_stand_in_for_what_a_policy_reads() {
    let (_server, port) = start("temp");
    let mut admin = connect(port, "admin").unwrap();
    admin
        .batch_execute(
            "create role r1b_t1 login;
             create table member (who text, team int);
             insert into member values ('r1b_t1', 1);
             create table task (id int primary key, team int);
             insert into task values (1, 1), (2, 2);
             grant select on member, task to r1b_t1;
             alter table task enable row level security;
             create policy team on task using (team in (select m.team from member m where m.who = current_user))",
        )
        .unwrap();
    let mut t1 = connect(port, "r1b_t1").unwrap();
    assert_eq!(ids(&mut t1, "select id from task order by id"), [1]);
    t1.batch_execute("create temp table member (who text, team int); insert into member values ('r1b_t1', 2)").unwrap();
    // the policy reads the table it was made with, as Postgres does (it binds it by oid)
    assert_eq!(ids(&mut t1, "select id from task order by id"), [1]);
    let st = t1.prepare("select id from task where id = $1").unwrap();
    assert!(t1.query(&st, &[&2i32]).unwrap().is_empty());
    assert_eq!(t1.query(&st, &[&1i32]).unwrap().len(), 1);
    // the session's own reads of the temporary table go on
    assert_eq!(ids(&mut t1, "select team from member"), [2]);
    // another session's reads don't see it
    let mut t1b = connect(port, "r1b_t1").unwrap();
    assert_eq!(ids(&mut t1b, "select id from task order by id"), [1]);
}

/// POSTs `body` to /v1/`op` with `token`: the status and the reply.
fn post(port: u16, token: &str, op: &str, body: &str) -> (u16, serde_json::Value) {
    use std::io::Write;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let req = format!(
        "POST /v1/{op} HTTP/1.0\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, json) = raw.split_once("\r\n\r\n").unwrap();
    (head.split(' ').nth(1).unwrap().parse().unwrap(), serde_json::from_str(json).unwrap_or_default())
}

/// The HTTP and MCP row API works on rows as they are: an agent whose role a policy limits on a table
/// is refused its rows there (403, 42501), and reads them with SQL, which applies the policies.
#[test]
fn the_row_api_refuses_an_agent_its_policies_limit() {
    let dir = std::env::temp_dir().join(format!("chronos-rls-{}-api", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    let s = common::serve(&mut cmd, true);
    // (stopped before MCP opens the folder, which it keeps)
    struct Stop(Child);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let server = Stop(s.child);
    let mut admin = connect(s.pg, "admin").unwrap();
    admin
        .batch_execute(
            "create table notes (id int primary key, tenant text, body text);
             insert into notes values (1, 'r1b_bot', 'mine'), (2, 'r1b_t2', 'theirs');
             create table open (id int primary key, body text);
             insert into open values (1, 'all');
             alter table notes enable row level security;
             create policy tenant on notes using (tenant = current_user)",
        )
        .unwrap();
    let made = admin.simple_query("create agent r1b_bot with (can = 'read,fork,write_own,write,write_main')").unwrap();
    let token = made
        .iter()
        .find_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => r.get(2).map(String::from),
            _ => None,
        })
        .expect("the agent's token");
    // no role of its name: its capabilities alone decide, as before
    assert_eq!(post(s.http, &token, "get", r#"{"table": "notes", "id": "2"}"#).0, 200);
    admin.batch_execute("create role r1b_bot; grant select, insert, update, delete on notes, open to r1b_bot").unwrap();
    for (op, body) in [
        ("get", r#"{"table": "notes", "id": "2"}"#),
        ("find", r#"{"table": "notes"}"#),
        ("put", r#"{"table": "notes", "id": "3", "record": {"tenant": "r1b_t2"}}"#),
        ("delete", r#"{"table": "notes", "id": "2"}"#),
        ("batch", r#"{"rows": [{"table": "notes", "id": "2", "record": null}]}"#),
    ] {
        let (status, reply) = post(s.http, &token, op, body);
        assert_eq!((status, &reply["error"]["code"]), (403, &serde_json::json!("42501")), "{op}: {reply}");
        assert!(reply["error"]["message"].as_str().unwrap().contains("row-level security"), "{op}: {reply}");
    }
    // a table without row security, and SQL
    assert_eq!(post(s.http, &token, "get", r#"{"table": "open", "id": "1"}"#).0, 200);
    let (status, reply) = post(s.http, &token, "sql", r#"{"sql": "select id from notes order by id"}"#);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["results"][0]["rows"], serde_json::json!([{"id": 1}]));
    // a world's diff, where it changed the table
    admin.batch_execute("create world w; use world w; insert into notes values (5, 'r1b_t2', 'x')").unwrap();
    let (status, reply) = post(s.http, &token, "diff", r#"{"branch": "w"}"#);
    assert_eq!(status, 403, "{reply}");
    // over MCP, the same
    drop(admin);
    drop(server);
    let script = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_string(),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get","arguments":{"table":"notes","id":"2"}}}"#
            .to_string(),
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get","arguments":{"table":"open","id":"1"}}}"#
            .to_string(),
    ];
    let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["mcp", dir.to_str().unwrap(), "--agent", "r1b_bot"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        c.stdin.take().unwrap().write_all(script.join("\n").as_bytes()).unwrap();
    }
    let out = String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap();
    let replies: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let text = |id: i64| {
        let r = replies.iter().find(|r| r["id"] == id).unwrap_or_else(|| panic!("no reply {id}: {out}"));
        (r["result"]["isError"] == true, r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string())
    };
    let (refused, said) = text(2);
    assert!(refused && said.contains("row-level security"), "{said}");
    assert!(!text(3).0, "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A table read AS OF a moment has the policies it had then, and those its name has now: renamed or
/// dropped since, it isn't read whole.
#[test]
fn a_table_read_as_of_a_moment_keeps_its_policies() {
    let dir = std::env::temp_dir().join(format!("chronos-rls-{}-asof", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "create role r1b_t1");
    for t in ["notes", "gone"] {
        ok(&db, &mut s, &format!("create table {t} (id int primary key, tenant text)"));
        ok(&db, &mut s, &format!("insert into {t} values (1, 'r1b_t1'), (2, 'r1b_t2'), (3, 'r1b_t1')"));
        ok(&db, &mut s, &format!("grant select on {t} to r1b_t1"));
        ok(&db, &mut s, &format!("alter table {t} enable row level security"));
        ok(&db, &mut s, &format!("create policy tenant on {t} using (tenant = current_user)"));
    }
    let then = Value::Text(rows(&db, &mut s, "select now()::text")[0][0].clone());
    std::thread::sleep(std::time::Duration::from_millis(5));
    ok(&db, &mut s, "alter table notes rename to notes2");
    ok(&db, &mut s, "drop table gone");
    db.checkpoint().unwrap();
    ok(&db, &mut s, "set role r1b_t1");
    let at = |s: &mut Session, sql: &str| -> Vec<String> {
        let out = s.run(&db, sql, std::slice::from_ref(&then)).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
        out.rows.iter().map(|r| r[0].to_text()).collect()
    };
    assert_eq!(at(&mut s, "select id from notes as of $1 order by id"), ["1", "3"]);
    assert_eq!(at(&mut s, "select id from gone as of $1 order by id"), ["1", "3"]);
    assert_eq!(rows(&db, &mut s, "select id from notes2 order by id"), [["1"], ["3"]]);
    drop(s);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hundreds of policies on a table, put together, nest a few levels deep: reading and writing it
/// doesn't run out of stack.
#[test]
fn hundreds_of_policies_dont_overflow_the_stack() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "create role r1b_t1");
    ok(&db, &mut s, "create table t (id int primary key, v int)");
    ok(&db, &mut s, "insert into t values (1, 1), (2, 900)");
    ok(&db, &mut s, "grant select, insert, update on t to r1b_t1");
    ok(&db, &mut s, "alter table t enable row level security");
    let mut sql = String::new();
    for i in 1..=600 {
        sql += &format!("create policy p{i} on t using (v = {i});");
    }
    for i in 1..=600 {
        sql += &format!("create policy r{i} on t as restrictive using (v <> -{i});");
    }
    ok(&db, &mut s, &sql);
    ok(&db, &mut s, "set role r1b_t1");
    assert_eq!(rows(&db, &mut s, "select id from t order by id"), [["1"]]);
    assert_eq!(rows(&db, &mut s, "update t set v = v returning id"), [["1"]]);
    assert_eq!(rows(&db, &mut s, "insert into t values (3, 3) returning id"), [["3"]]);
}

/// A table's policies are its own whatever the names: a table can't move into a schema whose name
/// has `/` (stored as `a/b.t`, its policies' keys would be below table `a`'s, and its rows' keys
/// below `a`'s rows); policies' names may have `/` and `.`; table `b` of schema `a` and table `a` keep
/// apart; a table whose name looks like an escape (`c%2Fd`) keeps its own.
#[test]
fn policies_stay_with_their_table_whatever_the_names() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r1b_t1",
        "create table a (id int primary key, v int)",
        "insert into a values (1, 1), (2, 2)",
        "grant select on a to r1b_t1",
        "alter table a enable row level security",
        "create policy own on a using (v = 1)",
        "create schema \"a/b\"",
        "create table t (id int primary key)",
    ] {
        ok(&db, &mut s, q);
    }
    let (code, message, _) = fails(&db, &mut s, "alter table t set schema \"a/b\"");
    assert_eq!((code.as_str(), message.as_str()), ("42601", "a table name can't be empty or contain '/'"));
    for q in [
        "create schema a",
        "grant usage on schema a to r1b_t1",
        "create table a.b (id int primary key, v int)",
        "insert into a.b values (1, 1), (2, 2)",
        "grant select on a.b to r1b_t1",
        "alter table a.b enable row level security",
        "create policy \"x/y\" on a.b using (v = 2)",
        "create policy \"p.q\" on a using (false)",
        "create table \"c%2Fd\" (id int primary key, v int)",
        "insert into \"c%2Fd\" values (1, 1), (2, 2)",
        "grant select on \"c%2Fd\" to r1b_t1",
        "alter table \"c%2Fd\" enable row level security",
        "create policy \"c/p\" on \"c%2Fd\" using (v = 1)",
    ] {
        ok(&db, &mut s, q);
    }
    assert_eq!(
        rows(&db, &mut s, "select schemaname, tablename, policyname from pg_policies order by 1, 2, 3"),
        [["a", "b", "x/y"], ["public", "a", "own"], ["public", "a", "p.q"], ["public", "c%2Fd", "c/p"]]
    );
    assert_eq!(
        rows(&db, &mut s, "select relname from pg_class where relrowsecurity order by 1"),
        [["a"], ["b"], ["c%2Fd"]]
    );
    ok(&db, &mut s, "set role r1b_t1");
    assert_eq!(rows(&db, &mut s, "select id from a order by id"), [["1"]]);
    assert_eq!(rows(&db, &mut s, "select id from a.b order by id"), [["2"]]);
    assert_eq!(rows(&db, &mut s, "select id from \"c%2Fd\" order by id"), [["1"]]);
    ok(&db, &mut s, "reset role");
    // a world's DIFF names the table as it is
    ok(&db, &mut s, "create world w");
    ok(&db, &mut s, "use world w");
    ok(&db, &mut s, "drop policy \"c/p\" on \"c%2Fd\"");
    ok(&db, &mut s, "use world main");
    let diff = rows(&db, &mut s, "diff world w");
    assert!(diff.iter().any(|r| r.iter().any(|c| c == "c%2Fd")), "{diff:?}");
}

/// Dropping a table or view another table's policy reads is refused with Postgres's words (2BP01, each policy
/// in the detail), and CASCADE drops the policies with its notice; dropped together with that table, it goes.
#[test]
fn dropping_what_a_policy_reads_is_refused_as_postgres_does() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create table member (who text, team int)",
        "create table task (id int primary key, team int)",
        "alter table task enable row level security",
        "create policy team on task using (team in (select m.team from member m where m.who = current_user))",
        "create policy two on task for select using (exists (select 1 from member where member.team = task.team))",
        "create policy own on task using (team = 1)",
    ] {
        ok(&db, &mut s, q);
    }
    let (code, message, detail) = fails(&db, &mut s, "drop table member");
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("2BP01", "cannot drop table member because other objects depend on it")
    );
    assert_eq!(
        detail.as_deref(),
        Some("policy team on table task depends on table member\npolicy two on table task depends on table member")
    );
    let _ = chronos::sql::take_notices();
    ok(&db, &mut s, "drop table member cascade");
    let notices: Vec<(String, Option<String>)> =
        chronos::sql::take_notices().into_iter().map(|n| (n.message, n.detail)).collect();
    assert_eq!(
        notices,
        [(
            "drop cascades to 2 other objects".to_string(),
            Some("drop cascades to policy team on table task\ndrop cascades to policy two on table task".to_string())
        )]
    );
    assert_eq!(rows(&db, &mut s, "select policyname from pg_policies"), [["own"]]);
    // a view a policy reads
    for q in [
        "create table member (who text, team int)",
        "create view teams as select team from member",
        "create policy viaview on task using (team in (select team from teams))",
    ] {
        ok(&db, &mut s, q);
    }
    let (code, message, detail) = fails(&db, &mut s, "drop view teams");
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("2BP01", "cannot drop view teams because other objects depend on it")
    );
    assert_eq!(detail.as_deref(), Some("policy viaview on table task depends on view teams"));
    let _ = chronos::sql::take_notices();
    ok(&db, &mut s, "drop view teams cascade");
    let notices: Vec<String> = chronos::sql::take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(notices, ["drop cascades to policy viaview on table task"]);
    // dropped with the table whose policy reads it
    ok(&db, &mut s, "create policy again on task using (team in (select team from member))");
    ok(&db, &mut s, "drop table task, member");
    assert!(rows(&db, &mut s, "select policyname from pg_policies").is_empty());
}

/// Renaming a column a policy reads, in a subquery or from another table's policy, or a table it reads, renames
/// it in the policy's condition (shown as written: see docs/postgres-compatibility.md).
#[test]
fn renames_are_renamed_in_the_policies_that_read_them() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create table member (who text, team int)",
        "create table task (id int primary key, team int)",
        "alter table task enable row level security",
        "create policy team on task using (team in (select m.team from member m where m.who = current_user))",
        "create policy mine on task for select using (exists (select 1 from member where member.team = task.team and who = current_user))",
        "create table note (id int primary key, task int)",
        "alter table note enable row level security",
        "create policy n on note using (task in (select id from task where team = task))",
    ] {
        ok(&db, &mut s, q);
    }
    for q in [
        "alter table member rename column team to grp",
        "alter table member rename column who to member_name",
        "alter table task rename column id to task_id",
        "alter table task rename column team to team_no",
        "alter table note rename column task to task_ref",
        "alter table member rename to members",
    ] {
        ok(&db, &mut s, q);
    }
    assert_eq!(
        rows(&db, &mut s, "select tablename, policyname, qual from pg_policies order by 1, 2"),
        [
            ["note", "n", "(task_ref in(select task_id as id from task where team_no = task_ref))"],
            [
                "task",
                "mine",
                "(exists(select 1 from members member where member.grp = task.team_no and member_name = current_user))"
            ],
            ["task", "team", "(team_no in(select m.grp as team from members m where m.member_name = current_user))"],
        ]
    );
    // and they still read what they did
    for q in [
        "create role r3b_t1",
        "grant select on members, task, note to r3b_t1",
        "insert into members values ('r3b_t1', 1)",
        "insert into task values (1, 1), (2, 2)",
        "insert into note values (1, 1), (2, 2)",
        "set role r3b_t1",
    ] {
        ok(&db, &mut s, q);
    }
    assert_eq!(rows(&db, &mut s, "select task_id from task order by 1"), [["1"]]);
    assert_eq!(rows(&db, &mut s, "select id from note order by 1"), [["1"]]);
}

/// A table with policies joined to another stays a table read through its index where that's safe (the
/// conditions on it can't show its rows), as the same join without policies is.
#[test]
fn joined_tables_with_policies_keep_their_index_reads() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r3b_t1",
        "create table t (id int primary key, owner text, secret text)",
        "insert into t select g, case when g % 2 = 0 then 'r3b_t1' else 'x' end, 's' || g from generate_series(1, 500) g",
        "create table o (id int primary key, tid int, v text)",
        "insert into o select g, g * 7 % 500 + 1, 'v' from generate_series(1, 50) g",
        "grant select on t, o to r3b_t1",
        "alter table t enable row level security",
        "create policy own on t using (owner = current_user)",
    ] {
        ok(&db, &mut s, q);
    }
    let plan = |s: &mut Session, sql: &str| -> Vec<String> {
        rows(&db, s, &format!("explain {sql}")).into_iter().map(|r| r[0].clone()).collect()
    };
    let join = "select o.id, t.secret from o join t on t.id = o.tid where t.id = 22";
    let comma = "select o.id, t.secret from o, t where t.id = o.tid and t.id = 22";
    let (mine, mine_comma) = (plan(&mut s, join), plan(&mut s, comma));
    assert!(mine.iter().any(|l| l.contains("Index Scan using t_pkey on t")), "{mine:?}");
    ok(&db, &mut s, "set role r3b_t1");
    assert_eq!(plan(&mut s, join), mine);
    assert_eq!(plan(&mut s, comma), mine_comma);
    assert_eq!(rows(&db, &mut s, join), [["3", "s22"]]);
    assert!(rows(&db, &mut s, "select o.id from o join t on t.id = o.tid where t.id = 15").is_empty());
    // a condition that could show a hidden row's values keeps the policy's subquery
    let leaky = "select o.id from o join t on t.id = o.tid where t.id = 22 and length(t.secret) > 0";
    assert!(plan(&mut s, leaky).iter().any(|l| l.contains("Subquery Scan on t")));
}

/// A prepared statement is rewritten (its views and policies put in) once, and again only when what the rewrite
/// read changes or another role runs it: its runs reuse the rewritten statement and its plan.
#[test]
fn a_prepared_statement_is_rewritten_once() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r3b_t1",
        "create role r3b_t2",
        "create table t (id int primary key, owner text)",
        "insert into t values (1, 'r3b_t1'), (2, 'r3b_t2'), (3, 'r3b_t1')",
        "grant select on t to r3b_t1, r3b_t2",
        "alter table t enable row level security",
        "create policy own on t using (owner = current_user)",
        "create table other (id int primary key)",
        "set role r3b_t1",
    ] {
        ok(&db, &mut s, q);
    }
    let st = chronos::sql::parse("select id from t where id = $1").unwrap().pop().unwrap();
    let run = |s: &mut Session, id: i64| -> usize {
        s.execute(&db, &st, &[Value::Int(id)], &[chronos::sql::Type::Int]).unwrap().rows.len()
    };
    let before = chronos::sql::rewrites_on_this_thread();
    for _ in 0..50 {
        assert_eq!(run(&mut s, 1), 1);
        assert_eq!(run(&mut s, 2), 0);
        // (the extended protocol describes it between runs, under no role: kept apart)
        st.columns(&db, chronos::MAIN, &[chronos::sql::Type::Int]).unwrap();
    }
    assert_eq!(chronos::sql::rewrites_on_this_thread() - before, 1);
    // rows written, and a table that isn't read changing, change nothing
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "insert into t values (4, 'r3b_t2')");
    ok(&db, &mut s, "alter table other add column v int");
    ok(&db, &mut s, "set role r3b_t1");
    let before = chronos::sql::rewrites_on_this_thread();
    assert_eq!(run(&mut s, 1), 1);
    assert_eq!(chronos::sql::rewrites_on_this_thread() - before, 0);
    // another role: rewritten for it
    ok(&db, &mut s, "set role r3b_t2");
    assert_eq!(run(&mut s, 2), 1);
    assert_eq!(chronos::sql::rewrites_on_this_thread() - before, 1);
    // a policy changed: rewritten again
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "alter policy own on t using (owner = current_user or id = 1)");
    ok(&db, &mut s, "set role r3b_t2");
    assert_eq!(run(&mut s, 1), 1);
    assert_eq!(chronos::sql::rewrites_on_this_thread() - before, 2);
    assert_eq!(run(&mut s, 1), 1);
    assert_eq!(chronos::sql::rewrites_on_this_thread() - before, 2);
    // a policy made in a transaction applies in it, and is gone after ROLLBACK
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "begin");
    ok(&db, &mut s, "create policy two on t using (id = 2)");
    ok(&db, &mut s, "set role r3b_t1");
    assert_eq!(run(&mut s, 2), 1);
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "rollback");
    ok(&db, &mut s, "set role r3b_t1");
    assert_eq!(run(&mut s, 2), 0);
}

/// DROP SCHEMA of a schema holding a table another table's policy reads: refused with Postgres's detail, each
/// object it holds followed by the policies that read it; with CASCADE one notice lists them all.
#[test]
fn dropping_a_schema_a_policy_reads_lists_what_depends_on_it() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    let setup = [
        "create schema sc",
        "create table sc.member (who text, team int)",
        "create policy pk on tk using (team in (select team from sc.member))",
    ];
    for q in ["create table tk (id int, team int)", "alter table tk enable row level security"].iter().chain(&setup) {
        ok(&db, &mut s, q);
    }
    let e = s.run(&db, "drop schema sc", &[]).expect_err("drop schema sc");
    assert_eq!((e.code, e.message.as_str()), ("2BP01", "cannot drop schema sc because other objects depend on it"));
    let info = e.info.expect("a DETAIL and a HINT");
    assert_eq!(
        info.detail.as_deref(),
        Some("table sc.member depends on schema sc\npolicy pk on table tk depends on table sc.member")
    );
    assert_eq!(info.hint.as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    let _ = chronos::sql::take_notices();
    ok(&db, &mut s, "drop schema sc cascade");
    let notices: Vec<(String, Option<String>)> =
        chronos::sql::take_notices().into_iter().map(|n| (n.message, n.detail)).collect();
    assert_eq!(
        notices,
        [(
            "drop cascades to 2 other objects".to_string(),
            Some("drop cascades to table sc.member\ndrop cascades to policy pk on table tk".to_string())
        )]
    );
    assert!(rows(&db, &mut s, "select policyname from pg_policies").is_empty());
    // more in it: each with what reads it, then the rest
    for q in setup.iter().chain(&["create table sc.other (x int)", "create view sc.v as select 1 as one"]) {
        ok(&db, &mut s, q);
    }
    let e = s.run(&db, "drop schema sc", &[]).expect_err("drop schema sc");
    assert_eq!(
        e.info.and_then(|i| i.detail).as_deref(),
        Some(
            "table sc.member depends on schema sc\npolicy pk on table tk depends on table sc.member\n\
             table sc.other depends on schema sc\nview sc.v depends on schema sc"
        )
    );
    let _ = chronos::sql::take_notices();
    ok(&db, &mut s, "drop schema sc cascade");
    let notices: Vec<(String, Option<String>)> =
        chronos::sql::take_notices().into_iter().map(|n| (n.message, n.detail)).collect();
    assert_eq!(
        notices,
        [(
            "drop cascades to 4 other objects".to_string(),
            Some(
                "drop cascades to table sc.member\ndrop cascades to policy pk on table tk\n\
                 drop cascades to table sc.other\ndrop cascades to view sc.v"
                    .to_string()
            )
        )]
    );
    // one table alone
    ok(&db, &mut s, "create schema sc");
    ok(&db, &mut s, "create table sc.member (who text, team int)");
    let e = s.run(&db, "drop schema sc", &[]).expect_err("drop schema sc");
    assert_eq!(e.info.and_then(|i| i.detail).as_deref(), Some("table sc.member depends on schema sc"));
    let _ = chronos::sql::take_notices();
    ok(&db, &mut s, "drop schema sc cascade");
    let notices: Vec<String> = chronos::sql::take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(notices, ["drop cascades to table sc.member"]);
}
