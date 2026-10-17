//! What the October 2026 security review found, each kept fixed: the server refuses what it must.

use chronos::Db;
use chronos::sql::Session;

/// Runs `sql` in `s`, and its error's SQLSTATE and message if it fails.
fn fails(db: &Db, s: &mut Session, sql: &str) -> (String, String) {
    let e = s.run(db, sql, &[]).expect_err(sql);
    (e.code.to_string(), e.message)
}

#[test]
fn dictionary_file_names_are_names_not_paths() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for name in ["../x", "/tmp/x", "sub/x", "Upper", "a.b", ""] {
        for opt in ["stopwords", "synonyms"] {
            let template = if opt == "stopwords" { "simple" } else { "synonym" };
            let sql = format!("create text search dictionary d_{opt} (template = {template}, {opt} = '{name}')");
            let (code, message) = fails(&db, &mut s, &sql);
            assert_eq!(code, "22023", "{sql}");
            assert_eq!(message, format!("invalid text search configuration file name \"{name}\""), "{sql}");
        }
    }
}

/// Runs `sql` in `s`, failing the test if it fails.
fn ok(db: &Db, s: &mut Session, sql: &str) {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The one value `sql` gives.
fn one(db: &Db, s: &mut Session, sql: &str) -> String {
    let out = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    out.rows[0][0].to_text()
}

#[test]
fn large_object_functions_that_write_need_the_right_to_write() {
    let db = Db::new();
    let mut owner = Session::new(chronos::MAIN);
    ok(&db, &mut owner, "select lo_from_bytea(5001, 'kept')");
    ok(&db, &mut owner, "create agent reader with (can = 'read')");
    for who in ["reader", chronos::GUEST] {
        let mut s = Session::new(chronos::MAIN);
        s.agent = Some(who.into());
        // reading one is a read
        assert_eq!(one(&db, &mut s, "select convert_from(lo_get(5001), 'utf8')"), "kept");
        for sql in [
            "select lo_from_bytea(5002, 'new')",
            "select lo_create(5003)",
            "select lo_creat(-1)",
            "select lo_put(5001, 0, 'gone')",
            "select lo_unlink(5001)",
        ] {
            assert_eq!(fails(&db, &mut s, sql).0, "42501", "{who}: {sql}");
        }
    }
    // main is as it was: 5001 as it was, and none made
    assert_eq!(one(&db, &mut owner, "select convert_from(lo_get(5001), 'utf8')"), "kept");
    for oid in [5002, 5003] {
        assert_eq!(fails(&db, &mut owner, &format!("select lo_get({oid})")).0, "42704", "{oid}");
    }
}

#[test]
fn an_agent_sets_session_authorization_only_to_its_own_role() {
    let db = Db::new();
    let mut owner = Session::new(chronos::MAIN);
    ok(&db, &mut owner, "create table t (id int primary key)");
    ok(&db, &mut owner, "create role boss superuser");
    ok(&db, &mut owner, "create agent bot with (can = 'read,fork,write_own,write,write_main')");
    ok(&db, &mut owner, "create role bot");
    ok(&db, &mut owner, "grant select on t to bot");
    ok(&db, &mut owner, "create agent keeper with (can = 'read,admin')");
    let mut bot = Session::new(chronos::MAIN);
    bot.agent = Some("bot".into());
    assert_eq!(fails(&db, &mut bot, "set session authorization boss").0, "42501");
    // its own role, and back, are allowed; its role's privileges still apply
    ok(&db, &mut bot, "set session authorization bot");
    ok(&db, &mut bot, "set session authorization default");
    assert_eq!(fails(&db, &mut bot, "insert into t values (1)").0, "42501");
    // an agent that may administer the database may, as the database's own user may
    let mut keeper = Session::new(chronos::MAIN);
    keeper.agent = Some("keeper".into());
    ok(&db, &mut keeper, "set session authorization boss");
}

#[test]
fn a_functions_own_search_path_holds_for_its_calls() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for sql in [
        "create schema app",
        "create table app.t (v text)",
        "insert into app.t values ('app')",
        "create schema mine",
        "create table mine.t (v text)",
        "insert into mine.t values ('mine')",
        "create function app.get_sql() returns text language sql security definer \
         set search_path = app, pg_temp as $$ select v from t $$",
        "create function app.get_pl() returns text language plpgsql security definer \
         set search_path to app, pg_temp as $$ begin return (select v from t); end $$",
    ] {
        ok(&db, &mut s, sql);
    }
    // the caller's search_path doesn't reach into the call
    ok(&db, &mut s, "set search_path = mine, public, app");
    assert_eq!(one(&db, &mut s, "select v from t"), "mine");
    assert_eq!(one(&db, &mut s, "select app.get_sql()"), "app");
    assert_eq!(one(&db, &mut s, "select app.get_pl()"), "app");
    // nor, with pg_temp named after app, do its temporary tables
    ok(&db, &mut s, "create temp table t (v text)");
    ok(&db, &mut s, "insert into t values ('temp')");
    assert_eq!(one(&db, &mut s, "select v from t"), "temp");
    assert_eq!(one(&db, &mut s, "select app.get_sql()"), "app");
    assert_eq!(one(&db, &mut s, "select app.get_pl()"), "app");
    // and the definition says so, as Postgres's does
    let def = one(&db, &mut s, "select pg_get_functiondef('app.get_sql'::regproc)");
    assert!(def.contains(" SECURITY DEFINER\n SET search_path TO 'app', 'pg_temp'\n"), "{def}");
}

#[test]
fn an_agent_with_no_role_makes_no_security_definer_function() {
    let db = Db::new();
    let mut owner = Session::new(chronos::MAIN);
    ok(&db, &mut owner, "create agent bot with (can = 'read,fork,write_own,write,write_main')");
    ok(&db, &mut owner, "create agent keeper with (can = 'read,write,write_main,admin')");
    let mut bot = Session::new(chronos::MAIN);
    bot.agent = Some("bot".into());
    let definer = "create function f_def() returns int language sql security definer as $$ select 1 $$";
    assert_eq!(fails(&db, &mut bot, definer).0, "42501");
    ok(&db, &mut bot, "create function f_plain() returns int language sql as $$ select 1 $$");
    assert_eq!(fails(&db, &mut bot, "alter function f_plain() security definer").0, "42501");
    ok(&db, &mut bot, "alter function f_plain() security invoker");
    // with a role of its name, what it makes is that role's, and runs as it
    ok(&db, &mut owner, "create role bot");
    ok(&db, &mut owner, "grant create on schema public to bot");
    ok(&db, &mut bot, definer);
    // an agent that may administer the database may, as the database's own user may
    let mut keeper = Session::new(chronos::MAIN);
    keeper.agent = Some("keeper".into());
    ok(&db, &mut keeper, "create function f_keeper() returns int language sql security definer as $$ select 1 $$");
}

#[test]
fn a_revoke_holds_for_reads_of_the_past() {
    let dir = std::env::temp_dir().join(format!("chronos-hardening-past-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(chronos::MAIN);
    for sql in [
        "create table t (id int primary key, v text)",
        "insert into t values (1, 'a')",
        "create table gone (id int primary key)",
        "insert into gone values (1)",
        "create table moved (id int primary key)",
        "insert into moved values (1)",
        "create table remade (id int primary key)",
        "insert into remade values (1)",
        "create role reader",
        "grant select on t, gone, moved, remade to reader",
        "grant create on schema public to reader",
    ] {
        ok(&db, &mut s, sql);
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    let then = one(&db, &mut s, "select now()::text");
    std::thread::sleep(std::time::Duration::from_millis(20));
    ok(&db, &mut s, "revoke select on t from reader");
    ok(&db, &mut s, "drop table gone");
    ok(&db, &mut s, "alter table moved rename to secret");
    ok(&db, &mut s, "revoke select on secret from reader");
    ok(&db, &mut s, "drop table remade");
    ok(&db, &mut s, "set role reader");
    ok(&db, &mut s, "create table remade (id int primary key)");
    ok(&db, &mut s, "reset role");
    let mut past = Session::new(&format!("main@{then}"));
    ok(&db, &mut past, "set role reader");
    // revoked since: refused, then as now
    assert_eq!(fails(&db, &mut past, "select v from t").0, "42501");
    // renamed (and revoked) or dropped since: no relation of its name now has privileges to check, so refused
    assert_eq!(fails(&db, &mut past, "select count(*) from moved").0, "42501");
    assert_eq!(fails(&db, &mut past, "select count(*) from gone").0, "42501");
    // dropped and made again since, by reader: another relation of that name, not the one reader may no longer read
    assert_eq!(fails(&db, &mut past, "select count(*) from remade").0, "42501");
    // a superuser reads them all there
    ok(&db, &mut past, "reset role");
    for table in ["t", "gone", "moved", "remade"] {
        assert_eq!(one(&db, &mut past, &format!("select count(*) from {table}")), "1", "{table}");
    }
    drop(past);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_agents_function_body_makes_no_security_definer_function_either() {
    let db = Db::new();
    let mut owner = Session::new(chronos::MAIN);
    ok(&db, &mut owner, "create agent bot with (can = 'read,fork,write_own,write,write_main')");
    let mut bot = Session::new(chronos::MAIN);
    bot.agent = Some("bot".into());
    ok(&db, &mut bot, "create function f_plain() returns int language sql as $$ select 1 $$");
    for sql in [
        "do $$ begin execute 'create function f_dyn() returns int language sql security definer as ''select 1'''; end $$",
        "do $$ begin execute 'alter function f_plain() security definer'; end $$",
        "create schema s_def create function s_def.f_el() returns int language sql security definer as $$ select 1 $$",
    ] {
        assert_eq!(fails(&db, &mut bot, sql).0, "42501", "{sql}");
    }
    assert_eq!(fails(&db, &mut owner, "select f_dyn()").0, "42883", "never made");
}

#[test]
fn a_security_definer_functions_user_schema_is_never_its_callers() {
    let dir = std::env::temp_dir().join(format!("chronos-hardening-user-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(chronos::MAIN);
        for sql in [
            "create table t (v text)",
            "insert into t values ('public')",
            "create role alice",
            "create schema alice authorization alice",
            "create table alice.t (v text)",
            "alter table alice.t owner to alice",
            "insert into alice.t values ('alice')",
            "create function f() returns text language sql security definer \
             set search_path = \"$user\", public as $$ select v from t $$",
        ] {
            ok(&db, &mut s, sql);
        }
    }
    // after a restart, the body is first bound in alice's statement: still never alice's schema
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "set role alice");
    assert_eq!(one(&db, &mut s, "select v from t"), "alice");
    assert_eq!(one(&db, &mut s, "select f()"), "public");
    drop(s);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_security_definer_functions_path_is_never_cut_to_its_callers_rights() {
    let dir = std::env::temp_dir().join(format!("chronos-hardening-usage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(chronos::MAIN);
        for sql in [
            "create role reader",
            "create schema private",
            "create table private.t (v text)",
            "insert into private.t values ('private')",
            "create schema shared",
            "grant usage on schema shared to reader",
            "create table shared.t (v text)",
            "insert into shared.t values ('shared')",
            "grant select on shared.t to reader",
            "create function f() returns text language sql security definer \
             set search_path = private, shared as $$ select v from t $$",
        ] {
            ok(&db, &mut s, sql);
        }
    }
    // after a restart the body is first bound in reader's statement, who may not use private: still private's t
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "set role reader");
    assert_eq!(one(&db, &mut s, "select f()"), "private");
    drop(s);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_functions_search_path_reads_as_set_search_path_does() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for sql in [
        "create table t (v text)",
        "insert into t values ('public')",
        "create schema app",
        "create table app.t (v text)",
        "insert into app.t values ('app')",
        "create function from_string() returns text language sql set search_path = 'app, public' as $$ select v from t $$",
        "create function from_default() returns text language sql set search_path = default as $$ select v from t $$",
    ] {
        ok(&db, &mut s, sql);
    }
    ok(&db, &mut s, "set search_path = app, public");
    assert_eq!(one(&db, &mut s, "select public.from_string()"), "app");
    assert_eq!(one(&db, &mut s, "select public.from_default()"), "public");
    let def = one(&db, &mut s, "select pg_get_functiondef('public.from_string'::regproc)");
    assert!(def.contains(" SET search_path TO 'app', 'public'\n"), "{def}");
}

#[test]
fn an_invoker_functions_body_is_never_bound_to_its_first_callers_rights() {
    let dir = std::env::temp_dir().join(format!("chronos-hardening-invoker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(chronos::MAIN);
        for sql in [
            "create role reader",
            "create schema private",
            "create table private.t (v text)",
            "insert into private.t values ('private')",
            "create schema shared",
            "grant usage on schema shared to reader",
            "create table shared.t (v text)",
            "insert into shared.t values ('shared')",
            "grant select on shared.t to reader",
            "create function f() returns text language sql set search_path = private, shared as $$ select v from t $$",
        ] {
            ok(&db, &mut s, sql);
        }
    }
    // after a restart reader, who may not use private, loads it first; the database's own user then calls it
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "set role reader");
    assert_eq!(one(&db, &mut s, "select f()"), "shared", "reader may not use private: its path skips it");
    ok(&db, &mut s, "reset role");
    assert_eq!(one(&db, &mut s, "select f()"), "private");
    ok(&db, &mut s, "set role reader");
    assert_eq!(one(&db, &mut s, "select f()"), "shared", "each role's own, in turn");
    drop(s);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_function_search_path_is_written_back() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "create function e() returns int language sql set search_path = '' as $$ select 1 $$");
    let def = one(&db, &mut s, "select pg_get_functiondef('e'::regproc)");
    assert!(def.contains(" SET search_path TO ''\n"), "{def}");
    // and what it writes makes it again
    ok(&db, &mut s, "drop function e()");
    ok(&db, &mut s, &def);
    assert_eq!(one(&db, &mut s, "select e()"), "1");
}

#[test]
fn an_invoker_functions_user_schema_is_its_callers() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for sql in [
        "create table t (v text)",
        "insert into t values ('public')",
        "grant select on t to public",
        "create role alice",
        "create schema alice authorization alice",
        "create table alice.t (v text)",
        "alter table alice.t owner to alice",
        "insert into alice.t values ('alice')",
        "create function f() returns text language sql set search_path = \"$user\", public as $$ select v from t $$",
    ] {
        ok(&db, &mut s, sql);
    }
    assert_eq!(one(&db, &mut s, "select f()"), "public");
    ok(&db, &mut s, "set role alice");
    assert_eq!(one(&db, &mut s, "select f()"), "alice");
    ok(&db, &mut s, "reset role");
    assert_eq!(one(&db, &mut s, "select f()"), "public");
}

#[test]
fn a_quoted_schema_in_a_string_search_path_keeps_its_commas() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for sql in [
        "create table t (v text)",
        "insert into t values ('public')",
        "create schema \"tenant,west\"",
        "create table \"tenant,west\".t (v text)",
        "insert into \"tenant,west\".t values ('tenant')",
        "create function f() returns text language sql set search_path = '\"tenant,west\", public' \
         as $$ select v from t $$",
    ] {
        ok(&db, &mut s, sql);
    }
    assert_eq!(one(&db, &mut s, "select f()"), "tenant");
    let def = one(&db, &mut s, "select pg_get_functiondef('f'::regproc)");
    assert!(def.contains(" SET search_path TO '\"tenant,west\"', 'public'\n"), "{def}");
    // which makes it again as it was
    ok(&db, &mut s, "drop function f()");
    ok(&db, &mut s, &def);
    assert_eq!(one(&db, &mut s, "select f()"), "tenant");
    // and SET search_path reads it so too
    ok(&db, &mut s, "set search_path = '\"tenant,west\", public'");
    assert_eq!(one(&db, &mut s, "select v from t"), "tenant");
}

#[test]
fn a_functions_body_is_bound_in_each_world_apart() {
    let db = Db::new();
    let mut a = Session::new(chronos::MAIN);
    for sql in [
        "create schema s1",
        "create table s1.t (v text)",
        "insert into s1.t values ('main')",
        "create schema s2",
        "create function f() returns text language sql set search_path = s1, s2 as $$ select v from t $$",
        "create world other",
    ] {
        ok(&db, &mut a, sql);
    }
    let mut b = Session::new("other");
    for sql in ["drop table s1.t", "create table s2.t (v text)", "insert into s2.t values ('other')"] {
        ok(&db, &mut b, sql);
    }
    // the same function, bound first in main: in other its names are other's
    assert_eq!(one(&db, &mut a, "select f()"), "main");
    assert_eq!(one(&db, &mut b, "select f()"), "other");
    assert_eq!(one(&db, &mut a, "select f()"), "main");
}

#[test]
fn temporary_tables_are_found_where_pg_temp_stands() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for sql in [
        "create table t (v text)",
        "insert into t values ('public')",
        "create schema private",
        "create schema app",
        "create table app.t (v text)",
        "insert into app.t values ('app')",
        "create function mid() returns text language sql set search_path = private, pg_temp, public \
         as $$ select v from t $$",
        "create function app_first() returns text language sql set search_path = app, pg_temp as $$ select v from t $$",
        "create temp table t (v text)",
        "insert into t values ('temp')",
    ] {
        ok(&db, &mut s, sql);
    }
    // private has no t: the temporary one, before public's
    assert_eq!(one(&db, &mut s, "select mid()"), "temp");
    // app's comes before pg_temp
    assert_eq!(one(&db, &mut s, "select app_first()"), "app");
    // and so along a session's search_path
    ok(&db, &mut s, "set search_path = private, pg_temp, public");
    assert_eq!(one(&db, &mut s, "select v from t"), "temp");
    ok(&db, &mut s, "set search_path = app, pg_temp, public");
    assert_eq!(one(&db, &mut s, "select v from t"), "app");
}

#[test]
fn the_past_of_a_world_dropped_since_is_a_superusers_only() {
    let dir = std::env::temp_dir().join(format!("chronos-hardening-dropped-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "create role reader");
    ok(&db, &mut s, "create world w");
    let mut w = Session::new("w");
    for sql in ["create table wt (v text)", "insert into wt values ('kept')", "grant select on wt to reader"] {
        ok(&db, &mut w, sql);
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    let then = one(&db, &mut w, "select now()::text");
    std::thread::sleep(std::time::Duration::from_millis(20));
    drop(w);
    ok(&db, &mut s, "drop world w");
    let mut past = Session::new(&format!("w@{then}"));
    ok(&db, &mut past, "set role reader");
    assert_eq!(fails(&db, &mut past, "select v from wt").0, "42501");
    drop(past);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_column_dropped_since_is_refused_in_the_past() {
    let dir = std::env::temp_dir().join(format!("chronos-hardening-column-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(chronos::MAIN);
    for sql in [
        "create table t (id int primary key, open text, secret text)",
        "insert into t values (1, 'a', 's')",
        "create role reader",
        "grant select (id, open, secret) on t to reader",
    ] {
        ok(&db, &mut s, sql);
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    let then = one(&db, &mut s, "select now()::text");
    std::thread::sleep(std::time::Duration::from_millis(20));
    ok(&db, &mut s, "revoke select (secret) on t from reader");
    ok(&db, &mut s, "alter table t drop column secret");
    let mut past = Session::new(&format!("main@{then}"));
    ok(&db, &mut past, "set role reader");
    // a column it may still read, then as now
    assert_eq!(one(&db, &mut past, "select open from t"), "a");
    // the dropped one: its privileges now can't be checked
    assert_eq!(fails(&db, &mut past, "select secret from t").0, "42501");
    // a superuser reads it there
    ok(&db, &mut past, "reset role");
    assert_eq!(one(&db, &mut past, "select secret from t"), "s");
    drop(past);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
