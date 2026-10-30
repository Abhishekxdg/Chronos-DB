//! Roles and privileges where a real connection, a world or the exact error matters (the SQL is
//! checked against Postgres by `pgdiff.rs`): logging in as a role, COPY, psql's `\du` and `\dp`,
//! agents, and what forks and merges do with grants.

mod common;

use chronos::Db;
use chronos::sql::{Session, Value};
use postgres::error::SqlState;
use postgres::{Client, NoTls};
use std::io::{Read, Write};
use std::process::{Child, Command};

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(name: &str, token: Option<&str>) -> (Server, u16) {
    let dir = std::env::temp_dir().join(format!("chronos-roles-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    if let Some(t) = token {
        cmd.args(["--token", t, "--unsafe"]); // the token as the database's own user, as before 0.4
    }
    let s = common::serve(&mut cmd, true);
    (Server(s.child, dir), s.pg)
}

fn connect(port: u16, user: &str) -> Result<Client, postgres::Error> {
    Client::connect(&format!("host=127.0.0.1 port={port} user={user} dbname=main"), NoTls)
}

fn text(c: &mut Client, q: &str) -> String {
    c.query_one(q, &[]).unwrap().get(0)
}

/// A server error's message.
fn message(e: &postgres::Error) -> String {
    e.as_db_error().map_or_else(|| e.to_string(), |d| d.message().to_string())
}

#[test]
fn a_created_role_logs_in_with_its_rights() {
    let (_server, port) = start("login", None);
    // a name that is no role is a superuser, as every login was before roles
    let mut admin = connect(port, "admin").unwrap();
    admin
        .batch_execute(
            "create table t (id int primary key, v text); insert into t values (1, 'a');
             create role r1_nologin; create role r1_user login; create role r1_old login valid until '2001-01-01';
             create role r1_reader login; grant select on t to r1_reader",
        )
        .unwrap();
    assert_eq!(text(&mut admin, "select current_user"), "admin");
    assert!(
        admin.query_one("select rolsuper from pg_roles where rolname = current_user", &[]).unwrap().get::<_, bool>(0)
    );

    let e = connect(port, "r1_nologin").err().expect("NOLOGIN is refused");
    assert_eq!(e.code(), Some(&SqlState::INVALID_AUTHORIZATION_SPECIFICATION), "{e}");
    assert_eq!(message(&e), "role \"r1_nologin\" is not permitted to log in");
    let e = connect(port, "r1_old").err().expect("an expired role is refused");
    assert_eq!(e.code(), Some(&SqlState::INVALID_PASSWORD), "{e}");

    // a LOGIN role with no privileges: in, and refused what it may not do
    let mut user = connect(port, "r1_user").unwrap();
    assert_eq!(text(&mut user, "select current_user"), "r1_user");
    assert_eq!(text(&mut user, "select session_user"), "r1_user");
    assert!(
        !user.query_one("select rolsuper from pg_roles where rolname = current_user", &[]).unwrap().get::<_, bool>(0)
    );
    for q in ["select * from t", "insert into t values (2, 'b')", "update t set v = 'x'", "delete from t", "truncate t"]
    {
        let e = user.batch_execute(q).unwrap_err();
        assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{q}: {e}");
        assert_eq!(message(&e), "permission denied for table t", "{q}");
    }
    let e = user.batch_execute("create table mine (id int)").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
    let e = user.batch_execute("create role r1_x").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
    let e = user.batch_execute("set role r1_reader").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
    // COPY is a read or a write like any other
    let e = user.copy_out("copy t to stdout").err().unwrap();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
    let e = user.copy_in::<_>("copy t from stdin").err().unwrap();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");

    // granted SELECT: COPY TO works, COPY FROM still doesn't
    let mut reader = connect(port, "r1_reader").unwrap();
    let mut out = String::new();
    reader.copy_out("copy t to stdout").unwrap().read_to_string(&mut out).unwrap();
    assert_eq!(out, "1\ta\n");
    let e = reader.copy_in::<_>("copy t (id, v) from stdin").err().unwrap();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
    admin.batch_execute("grant insert on t to r1_reader").unwrap();
    // (a connection whose COPY FROM failed can't run another: a client and server issue of its own)
    drop(reader);
    let mut reader = connect(port, "r1_reader").unwrap();
    let mut w = reader.copy_in("copy t from stdin").unwrap();
    w.write_all(b"2\tb\n").unwrap();
    w.finish().unwrap();
    assert_eq!(text(&mut admin, "select string_agg(v, ',' order by id) from t"), "a,b");

    // CONNECT on the database (the world), which PUBLIC has until revoked
    admin.batch_execute("revoke connect on database main from public").unwrap();
    let e = connect(port, "r1_user").err().expect("no CONNECT");
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
    assert_eq!(message(&e), "permission denied for database \"main\"");
    admin.batch_execute("grant connect on database main to r1_user").unwrap();
    connect(port, "r1_user").unwrap();
    // `postgres` is main too, as connecting to it is (Supabase's init scripts grant on it)
    admin.batch_execute("revoke connect on database postgres from r1_user").unwrap();
    assert!(connect(port, "r1_user").is_err());
    admin.batch_execute("grant connect on database postgres to r1_user").unwrap();
    connect(port, "r1_user").unwrap();
    // CONNECTION LIMIT: the one it has open counts
    admin.batch_execute("alter role r1_reader connection limit 1").unwrap();
    admin.batch_execute("grant connect on database main to r1_reader").unwrap();
    let e = connect(port, "r1_reader").err().expect("past its limit");
    assert_eq!(e.code(), Some(&SqlState::TOO_MANY_CONNECTIONS), "{e}");
    drop(reader);
    std::thread::sleep(std::time::Duration::from_millis(200));
    connect(port, "r1_reader").unwrap();

    // NOLOGIN later: the next login is refused
    admin.batch_execute("alter role r1_user nologin").unwrap();
    assert!(connect(port, "r1_user").is_err());
}

/// Runs `sql` in `s`, and its error's SQLSTATE, message and detail if it fails.
fn fails(db: &Db, s: &mut Session, sql: &str) -> (String, String, Option<String>) {
    let e = s.run(db, sql, &[]).expect_err(sql);
    (e.code.to_string(), e.message, e.info.and_then(|i| i.detail))
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
fn refusals_say_what_postgres_says() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r1_a",
        "create role r1_m createrole",
        "create table t (id int primary key)",
        "create function f() returns int language sql as $$ select 1 $$",
        "create view v as select 1 as x",
        "create schema s",
        "grant usage on schema s to r1_a",
        "create table s.u (id int)",
    ] {
        ok(&db, &mut s, q);
    }
    ok(&db, &mut s, "set role r1_a");
    let denied = |m: &str| ("42501".to_string(), m.to_string(), None);
    assert_eq!(fails(&db, &mut s, "drop table t"), denied("must be owner of table t"));
    assert_eq!(fails(&db, &mut s, "drop view v"), denied("must be owner of view v"));
    assert_eq!(fails(&db, &mut s, "drop function f()"), denied("must be owner of function f"));
    assert_eq!(fails(&db, &mut s, "alter table t add column x int"), denied("must be owner of table t"));
    assert_eq!(fails(&db, &mut s, "drop schema s"), denied("must be owner of schema s"));
    assert_eq!(fails(&db, &mut s, "create table t2 (id int)"), denied("permission denied for schema public"));
    assert_eq!(fails(&db, &mut s, "create table s.t2 (id int)"), denied("permission denied for schema s"));
    assert_eq!(fails(&db, &mut s, "select * from s.u"), denied("permission denied for table u"));
    // CREATE TABLE AS and SELECT INTO: the query is checked before the schema it makes the table in
    assert_eq!(fails(&db, &mut s, "create table t3 as select * from s.u"), denied("permission denied for table u"));
    assert_eq!(fails(&db, &mut s, "select * into t3 from s.u"), denied("permission denied for table u"));
    assert_eq!(
        fails(&db, &mut s, "create role r1_x"),
        (
            "42501".into(),
            "permission denied to create role".into(),
            Some("Only roles with the CREATEROLE attribute may create roles.".into())
        )
    );
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "set role r1_m");
    assert_eq!(
        fails(&db, &mut s, "create role r1_x superuser"),
        (
            "42501".into(),
            "permission denied to create role".into(),
            Some("Only roles with the SUPERUSER attribute may create roles with the SUPERUSER attribute.".into())
        )
    );
    assert_eq!(
        fails(&db, &mut s, "alter role r1_a login"),
        (
            "42501".into(),
            "permission denied to alter role".into(),
            Some(
                "Only roles with the CREATEROLE attribute and the ADMIN option on role \"r1_a\" may alter this role."
                    .into()
            )
        )
    );
    ok(&db, &mut s, "reset role");
    assert_eq!(fails(&db, &mut s, "grant r1_a to r1_m granted by r1_a").0, "42501");
    ok(&db, &mut s, "grant select on t to r1_a");
    let (code, message, detail) = fails(&db, &mut s, "drop role r1_a");
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("2BP01", "role \"r1_a\" cannot be dropped because some objects depend on it")
    );
    assert_eq!(detail.as_deref(), Some("privileges for schema s\nprivileges for table t"));
    assert_eq!(fails(&db, &mut s, "drop role current_user").0, "22023");
    assert_eq!(fails(&db, &mut s, "create role r1_a").0, "42710");
    // the bootstrap superuser's names, and the session's own login (a superuser), are taken
    let taken = |n: &str| ("42710".to_string(), format!("role \"{n}\" already exists"), None);
    assert_eq!(fails(&db, &mut s, "create role postgres login"), taken("postgres"));
    assert_eq!(fails(&db, &mut s, "create role chronos"), taken("chronos"));
    assert_eq!(fails(&db, &mut s, "alter role r1_m rename to postgres"), taken("postgres"));
    ok(&db, &mut s, "set role r1_m");
    assert_eq!(fails(&db, &mut s, "create role postgres login"), taken("postgres"));
    ok(&db, &mut s, "reset role");
    let mut ada = Session::new(chronos::MAIN);
    ada.user = Some("ada".into());
    assert_eq!(fails(&db, &mut ada, "create role ada login"), taken("ada"));
    assert_eq!(fails(&db, &mut s, "drop role postgres").0, "2BP01");
}

#[test]
fn merge_reads_what_its_conditions_and_sets_read() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r1_ma",
        "create table m1 (id int primary key, a int, b text)",
        "insert into m1 values (1, 10, 'secret')",
        "grant select (id, a), insert (id, a), update (a) on m1 to r1_ma",
        "set role r1_ma",
    ] {
        ok(&db, &mut s, q);
    }
    let denied = ("42501".to_string(), "permission denied for table m1".to_string(), None);
    for q in [
        "merge into m1 t using (select 1 as id) s on t.id = s.id when matched then update set a = length(t.b)",
        "merge into m1 t using (select 1 as id) s on t.id = s.id when matched and t.b = 'x' then update set a = 1",
        "merge into m1 t using (select 'secret' as v) s on t.b = s.v when matched then update set a = 1",
    ] {
        assert_eq!(fails(&db, &mut s, q), denied, "{q}");
    }
    ok(&db, &mut s, "merge into m1 t using (select 1 as id) s on t.id = s.id when matched then update set a = t.a + 1");
    ok(&db, &mut s, "reset role");
    assert_eq!(rows(&db, &mut s, "select a, b from m1"), [["11", "secret"]]);
}

#[test]
fn grants_are_the_worlds_and_merge_with_it() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "create role r1_a");
    ok(&db, &mut s, "create table t (id int primary key)");
    ok(&db, &mut s, "insert into t values (1)");
    ok(&db, &mut s, "create world w");
    // a role is the database's: every world sees it; a grant is the world's, like the table it's on
    let mut w = Session::new("w");
    assert_eq!(rows(&db, &mut w, "select count(*) from pg_roles where rolname = 'r1_a'"), [["1"]]);
    ok(&db, &mut w, "grant select on t to r1_a");
    let acl = "select relacl from pg_class where relname = 't'";
    assert_eq!(rows(&db, &mut w, acl), [["{chronos=arwdDxt/chronos,r1_a=r/chronos}"]]);
    assert_eq!(rows(&db, &mut s, acl), [["NULL"]]);
    ok(&db, &mut w, "set role r1_a");
    assert_eq!(rows(&db, &mut w, "select count(*) from t"), [["1"]]);
    ok(&db, &mut s, "set role r1_a");
    assert_eq!(fails(&db, &mut s, "select count(*) from t").0, "42501");
    ok(&db, &mut s, "reset role");
    ok(&db, &mut w, "reset role");
    // merged, the grant is main's too
    ok(&db, &mut s, "merge world w");
    assert_eq!(rows(&db, &mut s, acl), [["{chronos=arwdDxt/chronos,r1_a=r/chronos}"]]);
    ok(&db, &mut s, "set role r1_a");
    assert_eq!(rows(&db, &mut s, "select count(*) from t"), [["1"]]);
}

#[test]
fn an_agent_keeps_its_capabilities_and_its_roles_privileges() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "create table t (id int primary key)");
    ok(&db, &mut s, "insert into t values (1)");
    ok(&db, &mut s, "create agent bot with (can = 'read,fork,write_own,write,write_main')");
    // no role of its name: its capabilities alone decide
    let mut bot = Session::new(chronos::MAIN);
    bot.agent = Some("bot".into());
    ok(&db, &mut bot, "insert into t values (2)");
    // a role of its name: its privileges apply as well
    ok(&db, &mut s, "create role bot");
    ok(&db, &mut s, "grant select on t to bot");
    assert_eq!(rows(&db, &mut bot, "select count(*) from t"), [["2"]]);
    assert_eq!(fails(&db, &mut bot, "insert into t values (3)").0, "42501");
    // and it can't make roles or grant without the admin capability, superuser or not
    ok(&db, &mut s, "alter role bot superuser");
    assert_eq!(fails(&db, &mut bot, "create role r1_x").0, "42501");
    assert_eq!(fails(&db, &mut bot, "grant bot to r1_a").0, "42501");
}

#[test]
fn psql_lists_roles_and_privileges() {
    if Command::new("psql").arg("--version").output().is_err() {
        eprintln!("skipped: psql not installed");
        return;
    }
    let (_server, port) = start("psql", None);
    let url = format!("postgres://admin@127.0.0.1:{port}/main");
    let psql = |sql: &str| {
        let out = Command::new("psql").args([url.as_str(), "-X", "-A", "-t", "-c", sql]).output().unwrap();
        assert!(out.status.success(), "psql -c {sql}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    psql(
        "create role r1_a login createdb; create table t (id int primary key, v text); grant select on t to r1_a; grant update (v) on t to r1_a",
    );
    let du = psql("\\du");
    assert!(du.lines().any(|l| l.starts_with("r1_a|Create DB")), "{du}");
    let dp = psql("\\dp t");
    assert!(dp.contains("r1_a=r/chronos"), "{dp}");
    assert!(dp.contains("v:") && dp.contains("r1_a=w/chronos"), "{dp}");
    // \dn+ has its owners, privileges and comments; \dt each table's owner
    psql(
        "create schema s; grant usage on schema s to r1_a; comment on schema s is 'stuff'; create table s.x (id int); alter table s.x owner to r1_a",
    );
    let dn = psql("\\dn+ s");
    assert_eq!(dn, "s|chronos|chronos=UC/chronos\nr1_a=U/chronos|stuff\n");
    assert_eq!(psql("\\dt s.*"), "s|x|table|r1_a\n");
}

#[test]
fn pg_dump_writes_grants_that_restore() {
    if Command::new("pg_dump").arg("--version").output().is_err() {
        eprintln!("skipped: pg_dump not installed");
        return;
    }
    let (_a, pa) = start("dump-a", None);
    let (_b, pb) = start("dump-b", None);
    let roles = "create role app login; create role ro";
    connect(pa, "admin")
        .unwrap()
        .batch_execute(&format!(
            "{roles}; create schema s; create table s.t (id int primary key, v text); insert into s.t values (1, 'a');
             grant usage on schema s to app, ro; grant select on s.t to ro with grant option; grant insert (v), update (v) on s.t to app;
             create function s.f() returns int language sql security definer as $$ select 1 $$;
             revoke execute on function s.f() from public; grant execute on function s.f() to app;
             create sequence s.q; grant usage on sequence s.q to app; alter table s.t owner to app"
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
        "GRANT USAGE ON SCHEMA s TO app;",
        "GRANT SELECT ON TABLE s.t TO ro WITH GRANT OPTION;",
        "GRANT INSERT(v),UPDATE(v) ON TABLE s.t TO app;",
        "REVOKE ALL ON FUNCTION s.f() FROM PUBLIC;",
        "ALTER TABLE s.t OWNER TO app;",
    ] {
        assert!(first.contains(line), "{line} not in:\n{first}");
    }
    // the roles first (pg_dumpall's part), then the database: the same dump again
    connect(pb, "admin").unwrap().batch_execute(roles).unwrap();
    let file = std::env::temp_dir().join(format!("chronos-roles-{}-dump.sql", std::process::id()));
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
}

#[test]
fn superuser_logins_are_taken_names_and_signalling_needs_rights() {
    let (_server, port) = start("signal", None);
    let mut admin = connect(port, "admin").unwrap();
    let mut boss = connect(port, "boss").unwrap();
    admin.batch_execute("create role r1_k login createrole; create role r1_j login").unwrap();
    // an open connection that is no role is a superuser of that name: no role may take it over
    for q in ["create role admin", "create role boss login", "alter role r1_j rename to boss"] {
        let e = admin.batch_execute(q).unwrap_err();
        assert_eq!(e.code(), Some(&SqlState::DUPLICATE_OBJECT), "{q}: {e}");
    }
    let mut k = connect(port, "r1_k").unwrap();
    let e = k.batch_execute("create role postgres login").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::DUPLICATE_OBJECT), "{e}");
    let mut j = connect(port, "r1_j").unwrap();
    let pid = |c: &mut Client| -> i32 { c.query_one("select pg_backend_pid()", &[]).unwrap().get(0) };
    let (boss_pid, j_pid) = (pid(&mut boss), pid(&mut j));
    // a superuser's connection: only a superuser may cancel or end it
    let e = k.query_one("select pg_terminate_backend($1)", &[&boss_pid]).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
    assert_eq!(message(&e), "permission denied to terminate process");
    assert_eq!(
        e.as_db_error().and_then(|d| d.detail()),
        Some("Only roles with the SUPERUSER attribute may terminate processes of roles with the SUPERUSER attribute.")
    );
    let e = k.query_one("select pg_cancel_backend($1)", &[&boss_pid]).unwrap_err();
    assert_eq!(message(&e), "permission denied to cancel query");
    // another role's: its privileges, or pg_signal_backend's
    let e = k.query_one("select pg_cancel_backend($1)", &[&j_pid]).unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE), "{e}");
    admin.batch_execute("grant pg_signal_backend to r1_k").unwrap();
    assert!(k.query_one("select pg_cancel_backend($1)", &[&j_pid]).unwrap().get::<_, bool>(0));
    // and its own role's, always
    let mut k2 = connect(port, "r1_k").unwrap();
    let k2_pid = pid(&mut k2);
    admin.batch_execute("revoke pg_signal_backend from r1_k").unwrap();
    assert!(k.query_one("select pg_cancel_backend($1)", &[&k2_pid]).unwrap().get::<_, bool>(0));
    assert!(admin.query_one("select pg_terminate_backend($1)", &[&j_pid]).unwrap().get::<_, bool>(0));
}

#[test]
fn a_roles_settings_apply_when_it_logs_in() {
    let (_server, port) = start("config", None);
    let mut admin = connect(port, "admin").unwrap();
    admin
        .batch_execute(
            "create schema s; create role r1_cfg login;
             alter role r1_cfg set search_path = s, public; alter role r1_cfg set statement_timeout = '7s';
             alter role r1_cfg set timezone = 'Asia/Kolkata'",
        )
        .unwrap();
    let mut c = connect(port, "r1_cfg").unwrap();
    assert_eq!(text(&mut c, "show search_path"), "s, public");
    assert_eq!(text(&mut c, "show statement_timeout"), "7s");
    assert_eq!(text(&mut c, "show timezone"), "Asia/Kolkata");
    // the client's own startup options come after them
    let mut c = Client::connect(
        &format!("host=127.0.0.1 port={port} user=r1_cfg dbname=main options='-c TimeZone=Europe/Paris'"),
        NoTls,
    )
    .unwrap();
    assert_eq!(text(&mut c, "show timezone"), "Europe/Paris");
    assert_eq!(text(&mut c, "show search_path"), "s, public");
    // a login that is no role gets none of them
    assert_eq!(text(&mut admin, "show statement_timeout"), "0");
}

/// ALTER ROLE ... SET of the bootstrap superuser applies to a login that is no role (it is that superuser), and ALTER
/// DATABASE ... SET to every login to that database, under the role's own: what Supabase's setup runs.
#[test]
fn the_superusers_and_a_databases_settings_apply_when_logging_in() {
    let (_server, port) = start("dbconfig", None);
    let mut admin = connect(port, "admin").unwrap();
    admin
        .batch_execute(
            "create schema s; create role r2_cfg login; create database app;
             alter role postgres set statement_timeout = '9s';
             alter database app set search_path = s, public; alter database app set statement_timeout = '3s';
             alter database app set \"app.settings.jwt_secret\" to 'shh';
             alter role r2_cfg set statement_timeout = '7s'",
        )
        .unwrap();
    assert_eq!(text(&mut connect(port, "admin").unwrap(), "show statement_timeout"), "9s");
    let to_app =
        |user: &str| Client::connect(&format!("host=127.0.0.1 port={port} user={user} dbname=app"), NoTls).unwrap();
    let mut c = to_app("r2_cfg");
    assert_eq!(text(&mut c, "show search_path"), "s, public");
    assert_eq!(text(&mut c, "show statement_timeout"), "7s"); // the role's, over the database's
    assert_eq!(text(&mut c, "select current_setting('app.settings.jwt_secret')"), "shh");
    // they are what RESET puts back, as Postgres has it
    c.batch_execute("set \"app.settings.jwt_secret\" = 'other'; reset all").unwrap();
    assert_eq!(text(&mut c, "select current_setting('app.settings.jwt_secret')"), "shh");
    // main has none of app's
    let mut c = connect(port, "r2_cfg").unwrap();
    assert_ne!(text(&mut c, "show search_path"), "s, public");
    // pg_db_role_setting lists both kinds
    let rows: Vec<String> = admin
        .query(
            "select (setdatabase <> 0)::text || '|' || setrole::regrole::text || '|' || setconfig::text
             from pg_db_role_setting order by 1",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        rows,
        [
            "false|chronos|{statement_timeout=9s}",
            "false|r2_cfg|{statement_timeout=7s}",
            "true|-|{\"search_path=s, public\",statement_timeout=3s,app.settings.jwt_secret=shh}",
        ]
    );
    // RESET ALL takes them away; a database there isn't is refused
    admin.batch_execute("alter database app reset all; alter role chronos reset all").unwrap();
    assert_eq!(text(&mut to_app("r2_cfg"), "show search_path"), "\"$user\", public");
    let e = admin.batch_execute("alter database nowhere set search_path = s").unwrap_err();
    assert_eq!(e.code(), Some(&SqlState::UNDEFINED_DATABASE), "{e}");
    // a database dropped and made again under its name has none of the old one's
    admin.batch_execute("alter database app set \"app.settings.jwt_secret\" to 'shh'").unwrap();
    admin.batch_execute("drop database app with (force)").unwrap();
    admin.batch_execute("create database app").unwrap();
    assert_eq!(
        text(&mut to_app("r2_cfg"), "select coalesce(current_setting('app.settings.jwt_secret', true), '')"),
        ""
    );
    assert_eq!(text(&mut admin, "select count(*)::text from pg_db_role_setting where setdatabase <> 0"), "0");
}

/// A predefined role is a role that may not log in: a login as one is refused, never taken for the
/// superuser a name that is no role is.
#[test]
fn predefined_roles_may_not_log_in() {
    let (_server, port) = start("predefined", None);
    let mut admin = connect(port, "admin").unwrap();
    admin.batch_execute("create role r1_someone").unwrap();
    for name in ["pg_monitor", "pg_signal_backend", "r1_someone"] {
        let e = connect(port, name).err().unwrap_or_else(|| panic!("{name} logged in"));
        assert!(message(&e).contains("is not permitted to log in"), "{name}: {}", message(&e));
    }
}

/// "$user" in the default search_path is the role's own schema, when it has one: searched first,
/// and where an unqualified CREATE puts what it makes.
#[test]
fn user_in_the_search_path_is_the_roles_own_schema() {
    let (_server, port) = start("dollar_user", None);
    let mut admin = connect(port, "admin").unwrap();
    admin
        .batch_execute(
            "create role r1_u login; create schema r1_u authorization r1_u;
             create table public.t (v text); insert into public.t values ('public');
             grant select on public.t to r1_u",
        )
        .unwrap();
    let mut c = connect(port, "r1_u").unwrap();
    assert_eq!(text(&mut c, "select v from t"), "public");
    c.batch_execute("create table t (v text); insert into t values ('mine')").unwrap();
    assert_eq!(text(&mut c, "select v from t"), "mine");
    assert_eq!(text(&mut c, "select v from r1_u.t"), "mine");
}

/// USE WORLD needs CONNECT on the world it switches to, as a login there does.
#[test]
fn switching_worlds_needs_connect() {
    let (_server, port) = start("use_world", None);
    let mut admin = connect(port, "admin").unwrap();
    admin
        .batch_execute(
            "create role r1_w login; create world w2; use world w2;
             revoke connect on database w2 from public; use world main",
        )
        .unwrap();
    let mut c = connect(port, "r1_w").unwrap();
    let e = c.batch_execute("use world w2").expect_err("no CONNECT on w2");
    assert!(message(&e).contains("permission denied for database \"w2\""), "{}", message(&e));
    admin.batch_execute("use world w2; grant connect on database w2 to r1_w").unwrap();
    c.batch_execute("use world w2").unwrap();
}

/// Types', domains' and extensions' owners, and SET SCHEMA's CREATE, refused with Postgres's messages
/// (its own, from the oracle; `pgdiff.rs` compares the rest).
#[test]
fn types_extensions_and_set_schema_say_what_postgres_says() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r1_a",
        "create role r1_b",
        "create schema s",
        "create schema s2",
        "grant usage, create on schema s to r1_a, r1_b",
        "grant usage on schema s2 to r1_a, r1_b",
        "set role r1_a",
        "create type s.mood as enum ('a', 'b')",
        "create domain s.pos as int check (value > 0)",
        "create table s.m (id int primary key, f s.mood)",
        "reset role",
        "create extension pgcrypto schema s",
    ] {
        ok(&db, &mut s, q);
    }
    let denied = |m: &str| ("42501".to_string(), m.to_string(), None);
    ok(&db, &mut s, "set role r1_b");
    for q in [
        "drop type s.mood",
        "drop type s.mood cascade",
        "alter type s.mood add value 'c'",
        "alter type s.mood rename value 'a' to 'z'",
        "alter type s.mood rename to mood2",
    ] {
        assert_eq!(fails(&db, &mut s, q), denied("must be owner of type s.mood"), "{q}");
    }
    assert_eq!(fails(&db, &mut s, "drop domain s.pos"), denied("must be owner of type s.pos"));
    assert_eq!(fails(&db, &mut s, "drop type if exists s.pos, s.nosuch"), denied("must be owner of type s.pos"));
    assert_eq!(fails(&db, &mut s, "alter table s.m set schema s2"), denied("must be owner of table m"));
    assert_eq!(fails(&db, &mut s, "drop extension pgcrypto"), denied("must be owner of extension pgcrypto"));
    assert_eq!(fails(&db, &mut s, "alter extension pgcrypto update"), denied("must be owner of extension pgcrypto"));
    // (SET SCHEMA's schema is checked before the extension's owner, as Postgres looks it up first)
    assert_eq!(fails(&db, &mut s, "alter extension pgcrypto set schema s2"), denied("permission denied for schema s2"));
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "set role r1_a");
    assert_eq!(fails(&db, &mut s, "alter table s.m set schema s2"), denied("permission denied for schema s2"));
    assert_eq!(
        fails(&db, &mut s, "alter function f() set schema s2"),
        ("42883".into(), "function f() does not exist".into(), None)
    );
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "drop extension pgcrypto");
    // pgcrypto is trusted: CREATE on the database makes it, and its maker owns it
    ok(&db, &mut s, "set role r1_b");
    let e = s.run(&db, "create extension pgcrypto schema s", &[]).expect_err("no CREATE on the database");
    assert_eq!((e.code, e.message.as_str()), ("42501", "permission denied to create extension \"pgcrypto\""));
    assert_eq!(
        e.info.and_then(|i| i.hint).as_deref(),
        Some("Must have CREATE privilege on current database to create this extension.")
    );
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "grant create on database main to r1_b");
    ok(&db, &mut s, "set role r1_b");
    ok(&db, &mut s, "create extension pgcrypto schema s");
    assert_eq!(
        rows(
            &db,
            &mut s,
            "select r.rolname from pg_extension e join pg_roles r on r.oid = e.extowner where extname = 'pgcrypto'"
        ),
        [["r1_b"]]
    );
    ok(&db, &mut s, "alter extension pgcrypto update");
    assert_eq!(fails(&db, &mut s, "alter extension pgcrypto set schema s2"), denied("permission denied for schema s2"));
    ok(&db, &mut s, "reset role");
    let (code, _, detail) = fails(&db, &mut s, "drop role r1_b");
    assert_eq!(code, "2BP01");
    assert_eq!(
        detail.as_deref(),
        Some(
            "privileges for schema s\nprivileges for schema s2\nowner of extension pgcrypto\nprivileges for database main"
        )
    );
    ok(&db, &mut s, "drop owned by r1_b");
    assert_eq!(rows(&db, &mut s, "select count(*) from pg_extension where extname = 'pgcrypto'"), [["0"]]);
    ok(&db, &mut s, "drop role r1_b");
    // a type dropped leaves nothing of its owner's behind, and a renamed one is still its owner's
    ok(&db, &mut s, "grant create on schema public to r1_a");
    ok(&db, &mut s, "set role r1_a");
    ok(&db, &mut s, "create type tmp as enum ('x')");
    ok(&db, &mut s, "alter type tmp rename to tmp2");
    ok(&db, &mut s, "drop table s.m");
    ok(&db, &mut s, "drop type s.mood");
    ok(&db, &mut s, "drop domain s.pos");
    ok(&db, &mut s, "reset role");
    let (_, _, detail) = fails(&db, &mut s, "drop role r1_a");
    assert_eq!(
        detail.as_deref(),
        Some("privileges for schema public\nprivileges for schema s\nprivileges for schema s2\nowner of type tmp2")
    );
    ok(&db, &mut s, "set role r1_a");
    ok(&db, &mut s, "drop type tmp2");
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "revoke all on schema public, s, s2 from r1_a");
    ok(&db, &mut s, "drop role r1_a");
}

/// CONNECTION LIMIT counts the role's connections by the role, not by the name each logged in
/// with: a session from before a rename still counts.
#[test]
fn connection_limit_counts_a_renamed_roles_sessions() {
    let (_server, port) = start("connlimit", None);
    let mut admin = connect(port, "admin").unwrap();
    admin.batch_execute("create role r1_lim login connection limit 1").unwrap();
    let _first = connect(port, "r1_lim").unwrap();
    admin.batch_execute("alter role r1_lim rename to r1_lim2").unwrap();
    let e = connect(port, "r1_lim2").err().expect("past its limit");
    assert_eq!(e.code(), Some(&SqlState::TOO_MANY_CONNECTIONS), "{e}");
    assert_eq!(message(&e), "too many connections for role \"r1_lim2\"");
}

/// ALTER DEFAULT PRIVILEGES's refusals and DROP ROLE's detail of default privileges, and the type
/// and domain privileges' refusals, with Postgres's messages (its own, from the oracle; `pgdiff.rs`
/// compares the rest).
#[test]
fn default_and_type_privileges_say_what_postgres_says() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r3a_a",
        "create role r3a_b",
        "create role r3a_c",
        "create schema r3a_s",
        "grant usage, create on schema r3a_s to r3a_a",
        "alter default privileges for role r3a_a grant select on tables to r3a_b",
        "alter default privileges for role r3a_a in schema r3a_s grant insert on tables to r3a_c",
        "alter default privileges for role r3a_a revoke execute on functions from public",
        "alter default privileges for role r3a_a grant usage on types to r3a_b",
        "alter default privileges for role r3a_a grant all on sequences to r3a_b",
        "alter default privileges for role r3a_a grant all on schemas to r3a_b",
        "set role r3a_a",
        "create table r3a_s.t (id int)",
        "create type r3a_s.e as enum ('a')",
        "reset role",
    ] {
        ok(&db, &mut s, q);
    }
    let refused = |code: &str, m: &str| (code.to_string(), m.to_string(), None);
    let adp = "alter default privileges for role r3a_a";
    for (q, code, m) in [
        (
            "in schema r3a_s grant usage on schemas to r3a_b",
            "0LP01",
            "cannot use IN SCHEMA clause when using GRANT/REVOKE ON SCHEMAS",
        ),
        ("grant execute on tables to r3a_b", "0LP01", "invalid privilege type EXECUTE for relation"),
        ("grant usage on functions to r3a_b", "0LP01", "invalid privilege type USAGE for function"),
        ("grant insert on sequences to r3a_b", "0LP01", "invalid privilege type INSERT for sequence"),
        ("grant select on types to r3a_b", "0LP01", "invalid privilege type SELECT for type"),
        ("grant select on schemas to r3a_b", "0LP01", "invalid privilege type SELECT for schema"),
        ("grant temp on tables to r3a_b", "0LP01", "invalid privilege type TEMP for relation"),
        ("grant select (id) on tables to r3a_b", "0LP01", "default privileges cannot be set for columns"),
        ("grant all (id) on tables to r3a_b", "0LP01", "default privileges cannot be set for columns"),
        ("grant select on tables to public with grant option", "0LP01", "grant options can only be granted to roles"),
        ("in schema r3a_nope grant select on tables to r3a_b", "3F000", "schema \"r3a_nope\" does not exist"),
        ("for role r3a_b grant select on tables to r3a_c", "42601", "conflicting or redundant options"),
    ] {
        assert_eq!(fails(&db, &mut s, &format!("{adp} {q}")), refused(code, m), "{q}");
    }
    assert_eq!(
        fails(&db, &mut s, "alter default privileges for role r3a_nope grant select on tables to r3a_b"),
        refused("42704", "role \"r3a_nope\" does not exist")
    );
    assert_eq!(
        fails(&db, &mut s, "alter default privileges for role public grant select on tables to r3a_b"),
        refused("42704", "role \"public\" does not exist")
    );
    ok(&db, &mut s, "set role r3a_b");
    assert_eq!(
        fails(&db, &mut s, "alter default privileges for role r3a_a grant select on tables to r3a_b"),
        refused("42501", "permission denied to change default privileges")
    );
    ok(&db, &mut s, "reset role");
    // owning default privileges, or being named in them, keeps a role, as Postgres lists them
    let detail = |role: &str, db: &Db, s: &mut Session| {
        let (code, message, detail) = fails(db, s, &format!("drop role {role}"));
        assert_eq!(code, "2BP01");
        assert_eq!(message, format!("role \"{role}\" cannot be dropped because some objects depend on it"));
        detail.unwrap_or_default()
    };
    assert_eq!(
        detail("r3a_b", &db, &mut s),
        "privileges for default privileges on new relations belonging to role r3a_a\n\
         privileges for default privileges on new types belonging to role r3a_a\n\
         privileges for default privileges on new sequences belonging to role r3a_a\n\
         privileges for default privileges on new schemas belonging to role r3a_a\n\
         privileges for table r3a_s.t\n\
         privileges for type r3a_s.e"
    );
    assert_eq!(
        detail("r3a_a", &db, &mut s),
        "privileges for schema r3a_s\n\
         owner of default privileges on new relations belonging to role r3a_a\n\
         owner of default privileges on new relations belonging to role r3a_a in schema r3a_s\n\
         owner of default privileges on new functions belonging to role r3a_a\n\
         owner of default privileges on new types belonging to role r3a_a\n\
         owner of default privileges on new sequences belonging to role r3a_a\n\
         owner of default privileges on new schemas belonging to role r3a_a\n\
         owner of table r3a_s.t\n\
         owner of type r3a_s.e"
    );
    assert_eq!(
        detail("r3a_c", &db, &mut s),
        "privileges for default privileges on new relations belonging to role r3a_a in schema r3a_s\n\
         privileges for table r3a_s.t"
    );
    // a type's and a domain's USAGE
    for q in [
        "create type r3a_s.mood as enum ('a', 'b')",
        "create domain r3a_s.pos as int",
        "revoke usage on type r3a_s.mood from public",
        "revoke usage on domain r3a_s.pos from public",
    ] {
        ok(&db, &mut s, q);
    }
    ok(&db, &mut s, "set role r3a_a");
    let denied = |m: &str| refused("42501", m);
    for (q, m) in [
        ("create table r3a_s.t1 (m r3a_s.mood)", "permission denied for type r3a_s.mood"),
        ("create table r3a_s.t2 (p r3a_s.pos)", "permission denied for type r3a_s.pos"),
        ("create domain r3a_s.d2 as r3a_s.mood", "permission denied for type r3a_s.mood"),
        (
            "create function r3a_s.f1(x r3a_s.mood) returns int language sql as 'select 1'",
            "permission denied for type r3a_s.mood",
        ),
        (
            "create function r3a_s.f2() returns r3a_s.pos language sql as 'select 1'",
            "permission denied for type r3a_s.pos",
        ),
        ("alter table r3a_s.t add column m r3a_s.mood", "permission denied for type r3a_s.mood"),
        ("grant usage on type r3a_s.mood to r3a_b", "permission denied for type mood"),
    ] {
        assert_eq!(fails(&db, &mut s, q), denied(m), "{q}");
    }
    ok(&db, &mut s, "reset role");
    for (q, code, m) in [
        ("grant select on type r3a_s.mood to r3a_a", "0LP01", "invalid privilege type SELECT for type"),
        ("grant select on domain r3a_s.pos to r3a_a", "0LP01", "invalid privilege type SELECT for domain"),
        ("grant usage on domain r3a_s.mood to r3a_a", "42809", "\"mood\" is not a domain"),
        ("grant usage on type r3a_s.nope to r3a_a", "42704", "type \"r3a_s.nope\" does not exist"),
    ] {
        assert_eq!(fails(&db, &mut s, q), refused(code, m), "{q}");
    }
    let e = s.run(&db, "grant usage on type r3a_s._mood to r3a_a", &[]).expect_err("an array type");
    assert_eq!((e.code, e.message.as_str()), ("0LP01", "cannot set privileges of array types"));
    assert_eq!(e.info.and_then(|i| i.hint).as_deref(), Some("Set the privileges of the element type instead."));
    // without the grant option, a warning
    ok(&db, &mut s, "grant usage on type r3a_s.mood to r3a_a");
    ok(&db, &mut s, "set role r3a_a");
    let _ = chronos::sql::take_notices();
    let out = s.run(&db, "grant usage on type r3a_s.mood to r3a_b", &[]).unwrap().pop().unwrap();
    assert_eq!(out.tag, "GRANT");
    assert_eq!(
        chronos::sql::take_notices().into_iter().map(|n| format!("{} {}", n.level, n.message)).collect::<Vec<_>>(),
        ["WARNING no privileges were granted for \"mood\""]
    );
    ok(&db, &mut s, "reset role");
}

/// psql's `\ddp` lists default privileges, and pg_dump writes them as ALTER DEFAULT PRIVILEGES
/// that restore: the same dump again, the objects made after with the same grants.
#[test]
fn psql_lists_default_privileges_and_pg_dump_restores_them() {
    if Command::new("pg_dump").arg("--version").output().is_err() {
        eprintln!("skipped: pg_dump not installed");
        return;
    }
    let (_a, pa) = start("defacl-a", None);
    let (_b, pb) = start("defacl-b", None);
    let roles = "create role r3a_app login; create role r3a_ro";
    connect(pa, "admin")
        .unwrap()
        .batch_execute(&format!(
            "{roles}; create schema s; grant usage, create on schema s to r3a_app;
             alter default privileges for role r3a_app grant select on tables to r3a_ro;
             alter default privileges for role r3a_app in schema s grant usage on sequences to r3a_ro;
             alter default privileges for role r3a_app in schema s grant insert on tables to public;
             alter default privileges for role r3a_app revoke execute on functions from public;
             alter default privileges for role r3a_app grant usage on types to r3a_ro;
             alter default privileges for role r3a_app grant create on schemas to r3a_ro with grant option;
             set role r3a_app; create table s.t (id serial primary key); reset role;
             create type s.mood as enum ('a'); alter type s.mood owner to r3a_app; revoke usage on type s.mood from public"
        ))
        .unwrap();
    let url = format!("postgres://admin@127.0.0.1:{pa}/main");
    let out = Command::new("psql").args([url.as_str(), "-X", "-A", "-t", "-c", "\\ddp"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "r3a_app|s|sequence|r3a_ro=U/r3a_app\n\
         r3a_app|s|table|=a/r3a_app\n\
         r3a_app||function|r3a_app=X/r3a_app\n\
         r3a_app||schema|r3a_app=UC/r3a_app\nr3a_ro=C*/r3a_app\n\
         r3a_app||table|r3a_app=arwdDxt/r3a_app\nr3a_ro=r/r3a_app\n\
         r3a_app||type|=U/r3a_app\nr3a_app=U/r3a_app\nr3a_ro=U/r3a_app\n"
    );
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
        "ALTER DEFAULT PRIVILEGES FOR ROLE r3a_app IN SCHEMA s GRANT USAGE ON SEQUENCES TO r3a_ro;",
        "ALTER DEFAULT PRIVILEGES FOR ROLE r3a_app IN SCHEMA s GRANT INSERT ON TABLES TO PUBLIC;",
        "ALTER DEFAULT PRIVILEGES FOR ROLE r3a_app GRANT SELECT ON TABLES TO r3a_ro;",
        "ALTER DEFAULT PRIVILEGES FOR ROLE r3a_app REVOKE ALL ON FUNCTIONS FROM PUBLIC;",
        "ALTER DEFAULT PRIVILEGES FOR ROLE r3a_app GRANT ALL ON TYPES TO r3a_ro;",
        "ALTER DEFAULT PRIVILEGES FOR ROLE r3a_app GRANT CREATE ON SCHEMAS TO r3a_ro WITH GRANT OPTION;",
        "GRANT SELECT ON TABLE s.t TO r3a_ro;",
        "GRANT INSERT ON TABLE s.t TO PUBLIC;",
        "GRANT USAGE ON SEQUENCE s.t_id_seq TO r3a_ro;",
        "ALTER TYPE s.mood OWNER TO r3a_app;",
        "REVOKE ALL ON TYPE s.mood FROM PUBLIC;",
    ] {
        assert!(first.contains(line), "{line} not in:\n{first}");
    }
    connect(pb, "admin").unwrap().batch_execute(roles).unwrap();
    let file = std::env::temp_dir().join(format!("chronos-roles-{}-defacl.sql", std::process::id()));
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
    // and they apply on the restored database, where the type is its owner's
    let mut c = connect(pb, "admin").unwrap();
    assert_eq!(text(&mut c, "select typowner::regrole::text from pg_type where typname = 'mood'"), "r3a_app");
    c.batch_execute("set role r3a_app; create table s.u (id int); reset role").unwrap();
    assert_eq!(
        text(&mut c, "select relacl::text from pg_class where oid = 's.u'::regclass"),
        "{=a/r3a_app,r3a_app=arwdDxt/r3a_app,r3a_ro=r/r3a_app}"
    );
}

/// Default privileges are the world's, like grants: a fork has its parent's, sets its own, and a
/// merge brings them over.
#[test]
fn default_privileges_are_the_worlds_and_merge_with_it() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "create role r3a_a");
    ok(&db, &mut s, "create role r3a_b");
    ok(&db, &mut s, "grant create on schema public to r3a_a");
    ok(&db, &mut s, "create world w");
    let mut w = Session::new("w");
    ok(&db, &mut w, "alter default privileges for role r3a_a grant select on tables to r3a_b");
    let defaults = "select defaclobjtype, defaclacl::text from pg_default_acl";
    assert_eq!(rows(&db, &mut w, defaults), [["r", "{r3a_a=arwdDxt/r3a_a,r3a_b=r/r3a_a}"]]);
    assert!(rows(&db, &mut s, defaults).is_empty());
    ok(&db, &mut s, "set role r3a_a");
    ok(&db, &mut s, "create table t1 (id int)");
    ok(&db, &mut s, "reset role");
    assert_eq!(rows(&db, &mut s, "select relacl from pg_class where relname = 't1'"), [["NULL"]]);
    ok(&db, &mut s, "merge world w");
    assert_eq!(rows(&db, &mut s, defaults), [["r", "{r3a_a=arwdDxt/r3a_a,r3a_b=r/r3a_a}"]]);
    ok(&db, &mut s, "set role r3a_a");
    ok(&db, &mut s, "create table t2 (id int)");
    ok(&db, &mut s, "reset role");
    assert_eq!(
        rows(&db, &mut s, "select relacl from pg_class where relname = 't2'"),
        [["{r3a_a=arwdDxt/r3a_a,r3a_b=r/r3a_a}"]]
    );
}

/// FOR ROLE's roles are looked up and checked one at a time, as Postgres does; GRANT ON TYPE of a
/// built-in type, by any of its names, is refused, and of an array type as Postgres refuses it.
#[test]
fn default_privileges_roles_in_turn_and_built_in_types() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in ["create role r3a_a", "create role r3a_b", "create table t (id int)"] {
        ok(&db, &mut s, q);
    }
    ok(&db, &mut s, "set role r3a_b");
    assert_eq!(
        fails(&db, &mut s, "alter default privileges for role r3a_a, r3a_nope grant select on tables to r3a_b"),
        ("42501".into(), "permission denied to change default privileges".into(), None)
    );
    ok(&db, &mut s, "reset role");
    for q in [
        "grant usage on type int4 to r3a_a",
        "grant usage on type pg_catalog.int4 to r3a_a",
        "grant usage on type varchar to r3a_a",
        "grant usage on type integer to r3a_a",
        "revoke usage on type text from public",
        "grant usage on type t to r3a_a",
    ] {
        assert_eq!(fails(&db, &mut s, q).0, "0A000", "{q}");
    }
    let e = s.run(&db, "grant usage on type _int4 to r3a_a", &[]).expect_err("an array type");
    assert_eq!((e.code, e.message.as_str()), ("0LP01", "cannot set privileges of array types"));
}

/// DROP SERVER ... CASCADE by the server's owner drops the foreign tables on it whoever owns them, as Postgres's
/// dependency drop does, in the one statement: the server never goes while a table on it stays.
#[test]
fn drop_server_cascade_takes_other_roles_foreign_tables() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r1_srv",
        "create role r1_tab",
        "create foreign data wrapper r1_w",
        "grant usage on foreign data wrapper r1_w to r1_srv",
        "grant create on schema public to r1_tab",
        "set role r1_srv",
        "create server r1_s foreign data wrapper r1_w",
        "grant usage on foreign server r1_s to r1_tab",
        "reset role",
        "set role r1_tab",
        "create foreign table r1_ft (a int) server r1_s",
        "reset role",
        "set role r1_srv",
    ] {
        ok(&db, &mut s, q);
    }
    assert_eq!(fails(&db, &mut s, "drop foreign table r1_ft").0, "42501", "not its owner");
    ok(&db, &mut s, "drop server r1_s cascade");
    // (only the owner check of the tables it takes is waived: the statement still runs as the caller)
    assert_eq!(rows(&db, &mut s, "select current_user"), [["r1_srv"]]);
    ok(&db, &mut s, "reset role");
    let count = |s: &mut Session, q: &str| rows(&db, s, q)[0][0].clone();
    assert_eq!(count(&mut s, "select count(*) from pg_foreign_server where srvname = 'r1_s'"), "0");
    assert_eq!(count(&mut s, "select count(*) from pg_class where relname = 'r1_ft'"), "0");
}

/// A DROP SERVER ... CASCADE that looked for the server's foreign tables before another session made one on it
/// doesn't take effect over it: its transaction fails to commit, and the new table keeps its server.
#[test]
fn drop_server_cascade_fails_over_a_table_made_meanwhile() {
    let db = Db::new();
    let (mut a, mut b) = (Session::new(chronos::MAIN), Session::new(chronos::MAIN));
    for q in ["create foreign data wrapper r1_w2", "create server r1_s2 foreign data wrapper r1_w2"] {
        ok(&db, &mut a, q);
    }
    ok(&db, &mut a, "create foreign table r1_ft1 (a int) server r1_s2");
    ok(&db, &mut a, "begin");
    ok(&db, &mut a, "drop server r1_s2 cascade");
    // (made while the drop is uncommitted, on the server as it still is)
    ok(&db, &mut b, "create foreign table r1_ft2 (a int) server r1_s2");
    assert!(a.run(&db, "commit", &[]).is_err(), "the drop must not commit over the new table");
    let count = |s: &mut Session, q: &str| rows(&db, s, q)[0][0].clone();
    assert_eq!(count(&mut b, "select count(*) from pg_foreign_server where srvname = 'r1_s2'"), "1");
    assert_eq!(
        count(&mut b, "select count(*) from pg_foreign_table f join pg_foreign_server s on s.oid = f.ftserver"),
        "2",
        "both tables keep their server"
    );
    // and once nothing new is made meanwhile, the drop takes them all
    let _ = a.run(&db, "rollback", &[]); // (in case the failed commit left the transaction open)
    ok(&db, &mut a, "drop server r1_s2 cascade");
    assert_eq!(count(&mut b, "select count(*) from pg_foreign_table"), "0");
}

/// Event triggers are a superuser's to make (and their owners', who are superusers, to change or drop).
#[test]
fn event_triggers_need_a_superuser() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r1_evt",
        "create function r1_evt_note() returns event_trigger language plpgsql as $$ begin end $$",
        "create event trigger r1_evt_t on ddl_command_start execute function r1_evt_note()",
        "set role r1_evt",
    ] {
        ok(&db, &mut s, q);
    }
    let e = s.run(&db, "create event trigger r1_evt_u on ddl_command_start execute function r1_evt_note()", &[]);
    let e = e.expect_err("not a superuser");
    assert_eq!((e.code, e.message.as_str()), ("42501", "permission denied to create event trigger \"r1_evt_u\""));
    assert_eq!(e.info.and_then(|i| i.hint).as_deref(), Some("Must be superuser to create an event trigger."));
    assert_eq!(fails(&db, &mut s, "drop event trigger r1_evt_t").1, "must be owner of event trigger r1_evt_t");
    assert_eq!(fails(&db, &mut s, "alter event trigger r1_evt_t disable").0, "42501");
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "drop event trigger r1_evt_t");
}

/// Statistics objects and publications keep their owners as Postgres does: CREATE STATISTICS needs its schema and
/// CREATE on it; ALTER STATISTICS ... OWNER TO a role the owner can SET ROLE to, which has CREATE on the object's
/// schema; DROP ROLE lists what a role owns of them, DROP OWNED drops them and REASSIGN OWNED gives them away.
#[test]
fn statistics_objects_and_publications_keep_their_owners() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for q in [
        "create role r5s_a",
        "create role r5s_b",
        "create role r5s_c",
        "create schema r5s_s",
        "grant usage, create on schema r5s_s to r5s_a",
        "grant usage on schema r5s_s to r5s_b",
        "create table r5s_s.t (a int, b int)",
        "alter table r5s_s.t owner to r5s_a",
    ] {
        ok(&db, &mut s, q);
    }
    let refused = |code: &str, m: &str| (code.to_string(), m.to_string(), None);
    ok(&db, &mut s, "set role r5s_a");
    assert_eq!(
        fails(&db, &mut s, "create statistics r5s_nope.st on a, b from r5s_s.t"),
        refused("3F000", "schema \"r5s_nope\" does not exist")
    );
    assert_eq!(
        fails(&db, &mut s, "create statistics st on a, b from r5s_s.t"),
        refused("42501", "permission denied for schema public")
    );
    ok(&db, &mut s, "create statistics r5s_s.st on a, b from r5s_s.t");
    assert_eq!(
        fails(&db, &mut s, "alter statistics r5s_s.st owner to r5s_b"),
        refused("42501", "must be able to SET ROLE \"r5s_b\"")
    );
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "grant r5s_b to r5s_a");
    ok(&db, &mut s, "set role r5s_a");
    assert_eq!(
        fails(&db, &mut s, "alter statistics r5s_s.st owner to r5s_b"),
        refused("42501", "permission denied for schema r5s_s")
    );
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "grant create on schema r5s_s to r5s_b");
    ok(&db, &mut s, "set role r5s_a");
    ok(&db, &mut s, "alter statistics r5s_s.st owner to r5s_b");
    ok(&db, &mut s, "reset role");
    // (a role that can't use the schema isn't let find the object in it, in Postgres: USAGE first)
    ok(&db, &mut s, "grant usage on schema r5s_s to r5s_c");
    ok(&db, &mut s, "set role r5s_c");
    // (ALTER ... OWNER TO and RENAME TO name the object by its own name, SET STATISTICS and DROP as written)
    for (q, name) in [
        ("alter statistics r5s_s.st owner to r5s_c", "st"),
        ("alter statistics r5s_s.st rename to st2", "st"),
        ("alter statistics r5s_s.st set statistics 5", "r5s_s.st"),
        ("drop statistics r5s_s.st", "r5s_s.st"),
    ] {
        let m = format!("must be owner of statistics object {name}");
        assert_eq!(fails(&db, &mut s, q), refused("42501", &m), "{q}");
    }
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "revoke usage on schema r5s_s from r5s_c");
    // what a role owns of them keeps it
    ok(&db, &mut s, "create publication r5s_p");
    ok(&db, &mut s, "alter publication r5s_p owner to r5s_c");
    let depends = |role: &str, db: &Db, s: &mut Session| {
        let (code, message, detail) = fails(db, s, &format!("drop role {role}"));
        assert_eq!(code, "2BP01");
        assert_eq!(message, format!("role \"{role}\" cannot be dropped because some objects depend on it"));
        detail.unwrap_or_default()
    };
    assert_eq!(depends("r5s_c", &db, &mut s), "owner of publication r5s_p");
    ok(&db, &mut s, "revoke all on schema r5s_s from r5s_b");
    assert_eq!(depends("r5s_b", &db, &mut s), "owner of statistics object r5s_s.st");
    ok(&db, &mut s, "reassign owned by r5s_c to r5s_a");
    assert_eq!(
        rows(&db, &mut s, "select r.rolname from pg_publication p join pg_roles r on r.oid = p.pubowner"),
        [["r5s_a"]]
    );
    // (a publication's new owner has CREATE on the database)
    ok(&db, &mut s, "set role r5s_a");
    assert_eq!(
        fails(&db, &mut s, "alter publication r5s_p owner to r5s_b"),
        refused("42501", "permission denied for database main")
    );
    ok(&db, &mut s, "reset role");
    ok(&db, &mut s, "drop role r5s_c");
    ok(&db, &mut s, "drop owned by r5s_b");
    assert_eq!(rows(&db, &mut s, "select count(*) from pg_statistic_ext"), [["0"]]);
    ok(&db, &mut s, "drop role r5s_b");
    ok(&db, &mut s, "drop owned by r5s_a");
    assert_eq!(rows(&db, &mut s, "select count(*) from pg_publication"), [["0"]]);
    ok(&db, &mut s, "drop role r5s_a");
}

/// Supabase's migrations grant on built-in functions and hand some to its admin role: GRANT EXECUTE ON FUNCTION
/// pg_catalog.pg_reload_conf(), ALTER FUNCTION pg_catalog.lo_export OWNER TO, by name alone where only one has it.
#[test]
fn built_in_functions_take_grants_and_owners() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "create role r_admin; create role r_other");
    ok(&db, &mut s, "grant execute on function pg_catalog.pg_reload_conf() to r_admin with grant option");
    // every role runs a built-in function here: a REVOKE, which wouldn't stop one, is refused
    assert_eq!(fails(&db, &mut s, "revoke execute on function pg_catalog.lower(text) from public").0, "0A000");
    assert_eq!(fails(&db, &mut s, "revoke execute on function pg_reload_conf() from r_admin").0, "0A000");
    // one made in pg_catalog is a function as any other: its privileges are its own
    ok(&db, &mut s, "create function pg_catalog.custom_fn() returns int language sql as 'select 1'");
    ok(&db, &mut s, "revoke execute on function pg_catalog.custom_fn() from public");
    ok(&db, &mut s, "alter function pg_catalog.lo_export owner to r_admin");
    ok(&db, &mut s, "alter function pg_catalog.lo_import(text) owner to r_admin");
    ok(&db, &mut s, "alter function pg_catalog.lo_import(text, oid) owner to r_admin");
    // kept through other statements' clean-up of what went
    ok(&db, &mut s, "create table unrelated (a int4); drop table unrelated");
    // (its owner, still: one who isn't may not alter it)
    ok(&db, &mut s, "set role r_admin; alter function pg_catalog.lo_export owner to r_admin; reset role");
    assert_eq!(fails(&db, &mut s, "set role r_other; alter function pg_catalog.lo_export owner to r_other").0, "42501");
    ok(&db, &mut s, "reset role");
    // the name alone names several: which one is meant isn't told
    assert_eq!(fails(&db, &mut s, "alter function pg_catalog.lo_import owner to r_admin").0, "42725");
    assert_eq!(fails(&db, &mut s, "grant execute on function no_such_fn() to r_admin").0, "42883");
}
