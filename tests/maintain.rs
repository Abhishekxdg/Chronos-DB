//! VACUUM, ANALYZE, REINDEX, CLUSTER, DISCARD, COMMENT ON, LOCK TABLE and the extensions apps
//! create: what these do here (their answers are checked against Postgres by `pgdiff.rs`).

use chronos::sql::{Session, take_notices};
use chronos::{Db, MAIN};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn tag(db: &Db, s: &mut Session, sql: &str) -> String {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap().tag
}

fn text(db: &Db, s: &mut Session, sql: &str) -> String {
    let o = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    o.rows
        .first()
        .map_or("(no rows)".into(), |r| if r[0] == chronos::sql::Value::Null { "NULL".into() } else { r[0].to_text() })
}

fn code(db: &Db, s: &mut Session, sql: &str) -> &'static str {
    s.run(db, sql, &[]).expect_err(sql).code
}

fn messages() -> Vec<String> {
    take_notices().into_iter().map(|n| format!("{}: {}", n.level, n.message)).collect()
}

#[test]
fn maintenance_says_what_it_did() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "create table t (id int primary key, v int)");
    tag(&db, &mut s, "create index t_v on t (v)");
    tag(&db, &mut s, "create view tv as select * from t");
    for (sql, want) in [
        ("vacuum", "VACUUM"),
        ("vacuum (verbose, analyze) t", "VACUUM"),
        ("analyze t (v)", "ANALYZE"),
        ("analyse", "ANALYZE"),
        ("reindex table t", "REINDEX"),
        ("reindex index t_v", "REINDEX"),
        ("reindex database main", "REINDEX"),
        ("cluster t using t_pkey", "CLUSTER"),
        ("cluster", "CLUSTER"),
    ] {
        assert_eq!(tag(&db, &mut s, sql), want, "{sql}");
    }
    assert!(messages().is_empty(), "these say nothing when they work");

    // a view is skipped, as Postgres does: with a warning
    assert_eq!(tag(&db, &mut s, "vacuum tv"), "VACUUM");
    assert_eq!(messages(), ["WARNING: skipping \"tv\" --- cannot vacuum non-tables or special system tables"]);

    // the column list of a table in a schema, and a session's temporary table
    tag(&db, &mut s, "create schema s1");
    tag(&db, &mut s, "create table s1.u (a int)");
    assert_eq!(tag(&db, &mut s, "analyze s1.u (a)"), "ANALYZE");
    assert_eq!(code(&db, &mut s, "analyze s1.u (b)"), "42703");
    tag(&db, &mut s, "create temp table tmp (a int)");
    assert_eq!(tag(&db, &mut s, "vacuum tmp"), "VACUUM");
    assert_eq!(tag(&db, &mut s, "analyze tmp (a)"), "ANALYZE");
    messages();

    // COMMENT ON is checked and kept (tests/comments.rs, pgdiff `comments`); the objects whose
    // comments aren't kept, and a temporary table, say so
    assert_eq!(tag(&db, &mut s, "comment on table t is 'orders'"), "COMMENT");
    assert!(messages().is_empty());
    assert_eq!(text(&db, &mut s, "select obj_description('t'::regclass, 'pg_class')"), "orders");
    assert_eq!(code(&db, &mut s, "comment on table nope is 'x'"), "42P01");
    for on in ["extension plpgsql", "table tmp"] {
        assert_eq!(tag(&db, &mut s, &format!("comment on {on} is 'x'")), "COMMENT");
        let said = messages();
        assert!(said.len() == 1 && said[0].starts_with("NOTICE: the comment isn't kept"), "{on}: {said:?}");
    }
}

#[test]
fn extensions_apps_create() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    // (an extension only in part says which part)
    assert_eq!(tag(&db, &mut s, "create extension \"uuid-ossp\""), "CREATE EXTENSION");
    assert_eq!(messages(), ["NOTICE: uuid-ossp: only uuid_generate_v4() is available, and it is built in"]);
    // (pgcrypto is all there: tests/pgcrypto.rs)
    assert_eq!(tag(&db, &mut s, "create extension if not exists pgcrypto"), "CREATE EXTENSION");
    assert!(messages().is_empty());
    assert_eq!(tag(&db, &mut s, "create extension if not exists vector"), "CREATE EXTENSION");
    assert!(messages().is_empty());
    assert_eq!(text(&db, &mut s, "select length(uuid_generate_v4()::text)"), "36");

    // plpgsql is in every database
    assert_eq!(tag(&db, &mut s, "create extension if not exists plpgsql"), "CREATE EXTENSION");
    assert_eq!(messages(), ["NOTICE: extension \"plpgsql\" already exists, skipping"]);
    assert_eq!(code(&db, &mut s, "create extension plpgsql"), "42710");
    // one that changes what SQL means isn't taken for one that doesn't
    assert_eq!(code(&db, &mut s, "create extension plv8"), "0A000");
    // (citext's type is built in: see tests/pgdiff.rs, `citext_compares_by_lower_case`)
    s.run(&db, "create extension citext", &[]).unwrap();
    // so are hstore's and ltree's types, operators and functions (tests/pgdiff.rs, `hstore_type` and `ltree_type`),
    // by the names pg_dump writes too: the type and the functions in the extension's schema
    s.run(&db, "create extension if not exists hstore with schema public", &[]).unwrap();
    s.run(&db, "create extension ltree", &[]).unwrap();
    assert!(messages().is_empty());
    tag(
        &db,
        &mut s,
        "create table ext (id int primary key, attrs public.hstore default ''::public.hstore, path public.ltree)",
    );
    tag(&db, &mut s, "create index ext_attrs on ext using gin (attrs)");
    tag(&db, &mut s, "create index ext_path on ext using gist (path public.gist_ltree_ops (siglen='8'))");
    tag(&db, &mut s, "insert into ext values (1, public.hstore('k', 'v'), 'top.sci')");
    assert_eq!(
        text(&db, &mut s, "select attrs operator(public.->) 'k' from ext where path operator(public.<@) 'top'"),
        "v"
    );
    assert_eq!(text(&db, &mut s, "select public.akeys(attrs)::text || public.nlevel(path)::text from ext"), "{k}2");
    // pg_stat_statements: each statement by its text with its constants taken out, how often it ran, its rows
    s.run(&db, "create extension pg_stat_statements", &[]).unwrap();
    s.run(&db, "select pg_stat_statements_reset()", &[]).unwrap();
    for q in ["select 1 + 1", "select 2 + 40", "SELECT 3 + 4;"] {
        s.run(&db, q, &[]).unwrap();
    }
    let of_it = |what: &str| format!("select {what} from pg_stat_statements where query = 'select $1 + $2'");
    assert_eq!(text(&db, &mut s, &of_it("calls")), "3");
    assert_eq!(text(&db, &mut s, &of_it("rows")), "3");
    assert_eq!(text(&db, &mut s, &of_it("total_exec_time >= max_exec_time")), "true");
    // (one row for the three: other tests' statements, run at the same time in this process, are kept too)
    assert_eq!(
        text(
            &db,
            &mut s,
            "select count(*) from pg_stat_statements where query = 'select $1 + $2' and queryid is not null"
        ),
        "1"
    );
    // a script of several statements: each by its own text
    s.run(&db, "select 5 * 6; select 7 - 8", &[]).unwrap();
    for q in ["select $1 * $2", "select $1 - $2"] {
        let calls = format!("select calls from pg_stat_statements where query = '{q}'");
        assert_eq!(text(&db, &mut s, &calls), "1", "{q}");
    }
    // (and columns by schema, table and name, as an ORM writes them)
    assert_eq!(
        text(&db, &mut s, "select \"public\".\"ext\".\"id\" from \"public\".\"ext\" where public.ext.id = 1"),
        "1"
    );
    assert_eq!(
        text(
            &db,
            &mut s,
            "select string_agg(udt_name, ',' order by ordinal_position) from information_schema.columns where table_name = 'ext'"
        ),
        "int4,hstore,ltree"
    );

    assert_eq!(tag(&db, &mut s, "drop extension \"uuid-ossp\""), "DROP EXTENSION");
    assert_eq!(tag(&db, &mut s, "drop extension if exists pgcrypto, vector cascade"), "DROP EXTENSION");
    assert_eq!(tag(&db, &mut s, "drop extension if exists nope"), "DROP EXTENSION");
    assert_eq!(code(&db, &mut s, "drop extension nope"), "42704");
}

/// pg_stat_statements keeps statements apart by user and database, as the extension keys them (userid, dbid,
/// queryid): a role without pg_read_all_stats's privileges sees the others' as `<insufficient privilege>`, with no
/// queryid; only a superuser resets them, all or a user's. A top-level statement counts once: an EXECUTE by its
/// PREPARE's text (PREPARE itself not at all), a DECLARE's query not apart from the DECLARE.
#[test]
fn pg_stat_statements_by_user() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "create extension pg_stat_statements");
    tag(&db, &mut s, "create role pss_alice");
    tag(&db, &mut s, "create role pss_bob");
    tag(&db, &mut s, "grant pg_read_all_stats to pss_bob");
    tag(&db, &mut s, "select pg_stat_statements_reset()");
    tag(&db, &mut s, "select 1 + 1");
    tag(&db, &mut s, "prepare pss_q(int) as select $1 + 100");
    tag(&db, &mut s, "execute pss_q(1)");
    tag(&db, &mut s, "execute pss_q(2)");
    tag(&db, &mut s, "begin");
    tag(&db, &mut s, "declare pss_c cursor for select 7 + 8");
    tag(&db, &mut s, "fetch pss_c");
    tag(&db, &mut s, "commit");
    let count = |s: &mut Session, filter: &str| {
        text(&db, s, &format!("select count(*) from pg_stat_statements where {filter}"))
    };
    assert_eq!(text(&db, &mut s, "select calls from pg_stat_statements where query like 'prepare pss_q%'"), "2");
    assert_eq!(count(&mut s, "query like 'execute%'"), "0");
    assert_eq!(text(&db, &mut s, "select calls from pg_stat_statements where query like 'declare pss_c%'"), "1");
    assert_eq!(text(&db, &mut s, "select calls from pg_stat_statements where query = 'select $1 + $2'"), "1");
    let here = "(select oid from pg_database where datname = current_database())";
    assert_eq!(count(&mut s, &format!("query = 'select $1 + $2' and dbid = {here} and toplevel")), "1");

    tag(&db, &mut s, "set role pss_alice");
    tag(&db, &mut s, "select 2 + 2");
    // its own statements by their text, the superuser's hidden
    assert_eq!(
        text(&db, &mut s, "select userid::regrole::text from pg_stat_statements where query = 'select $1 + $2'"),
        "pss_alice"
    );
    assert_eq!(
        text(&db, &mut s, "select calls from pg_stat_statements where query like 'prepare pss_q%'"),
        "(no rows)"
    );
    assert_ne!(count(&mut s, "query = '<insufficient privilege>' and queryid is null"), "0");
    assert_eq!(count(&mut s, "query = '<insufficient privilege>' and userid::regrole::text = 'pss_alice'"), "0");
    assert_eq!(code(&db, &mut s, "select pg_stat_statements_reset()"), "42501");
    tag(&db, &mut s, "reset role");
    // a member of pg_read_all_stats sees them all
    tag(&db, &mut s, "set role pss_bob");
    assert_eq!(count(&mut s, "query = 'select $1 + $2'"), "2");
    assert_eq!(count(&mut s, "query = '<insufficient privilege>'"), "0");
    tag(&db, &mut s, "reset role");
    // a user's statements reset, the others' kept
    tag(&db, &mut s, "select pg_stat_statements_reset('pss_alice'::regrole)");
    assert_eq!(count(&mut s, "query = 'select $1 + $2'"), "1");
    assert_eq!(count(&mut s, "userid::regrole::text = 'pss_alice'"), "0");
    assert_ne!(count(&mut s, "userid::regrole::text = 'pss_bob'"), "0");
    tag(&db, &mut s, "select pg_stat_statements_reset(0, 0, 0)");
    assert_eq!(count(&mut s, "query = 'select $1 + $2'"), "0");
    assert_eq!(code(&db, &mut s, "select pg_stat_statements_reset(0, 0, 0, false)"), "42883");

    // another database's statements are its own
    let other = Db::new();
    let mut o = Session::new(MAIN);
    o.run(&other, "select 3 + 3", &[]).unwrap();
    assert_eq!(count(&mut s, "query = 'select $1 + $2'"), "0");

    // a role renamed keeps its statements: they are its oid's, as Postgres keys them
    tag(&db, &mut s, "set role pss_alice");
    tag(&db, &mut s, "select 4 + 4");
    tag(&db, &mut s, "reset role");
    tag(&db, &mut s, "alter role pss_alice rename to pss_carol");
    tag(&db, &mut s, "set role pss_carol");
    assert_eq!(count(&mut s, "query = 'select $1 + $2' and userid::regrole::text = 'pss_carol'"), "1");
    assert_eq!(count(&mut s, "query = '<insufficient privilege>' and userid::regrole::text = 'pss_carol'"), "0");
    tag(&db, &mut s, "reset role");

    // a world dropped or merged takes its statements along: one made again by its name starts with none
    tag(&db, &mut s, "select pg_stat_statements_reset(0, 0, 0)");
    for end in ["drop world pss_w", "merge world pss_w"] {
        tag(&db, &mut s, "create world pss_w");
        tag(&db, &mut s, "switch world pss_w");
        tag(&db, &mut s, "select 5 + 5");
        tag(&db, &mut s, &format!("switch world {MAIN}"));
        assert_eq!(count(&mut s, "query = 'select $1 + $2'"), "1", "{end}");
        tag(&db, &mut s, end);
        tag(&db, &mut s, "create world pss_w");
        assert_eq!(count(&mut s, "query = 'select $1 + $2'"), "0", "{end}");
        tag(&db, &mut s, "drop world pss_w");
    }

    // a database server that closed leaves nothing behind for one made after it
    drop(o);
    drop(other);
    let next = Db::new();
    let mut n = Session::new(MAIN);
    tag(&next, &mut n, "create extension pg_stat_statements");
    assert_eq!(text(&next, &mut n, "select count(*) from pg_stat_statements where query = 'select $1 + $2'"), "0");
}

#[test]
fn discard_all_puts_a_session_back_as_it_connected() {
    let db = Arc::new(Db::new());
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "create table t (id serial primary key, v int)");
    tag(&db, &mut s, "create sequence sq");
    let mut other = Session::new(MAIN);

    // everything a client can leave behind
    tag(&db, &mut s, "set app.org_id = '42'");
    tag(&db, &mut s, "set statement_timeout = 5000");
    tag(&db, &mut s, "set time zone 'Asia/Kolkata'");
    tag(&db, &mut s, "set search_path = pg_catalog");
    tag(&db, &mut s, "prepare q as select 1");
    tag(&db, &mut s, "listen jobs");
    tag(&db, &mut s, "create temp table scratch (a int)");
    tag(&db, &mut s, "create world trial");
    tag(&db, &mut s, "switch world trial");
    tag(&db, &mut s, "select nextval('sq')");
    tag(&db, &mut s, "select pg_advisory_lock(7)");
    assert_eq!(text(&db, &mut s, "select current_database()"), "trial");
    assert_eq!(text(&db, &mut other, "select pg_try_advisory_lock(7)"), "false");
    assert!(s.listening());

    assert_eq!(tag(&db, &mut s, "discard all"), "DISCARD ALL");

    // (a custom setting, once made, stays as an empty one, as in Postgres)
    assert_eq!(text(&db, &mut s, "select current_setting('app.org_id', true)"), "");
    assert_eq!(text(&db, &mut s, "show statement_timeout"), "0");
    assert_eq!(text(&db, &mut s, "show timezone"), "UTC");
    assert_eq!(text(&db, &mut s, "show search_path"), "\"$user\", public");
    assert_eq!(code(&db, &mut s, "execute q"), "26000");
    assert!(!s.listening());
    assert_eq!(code(&db, &mut s, "select * from scratch"), "42P01");
    assert_eq!(text(&db, &mut s, "select current_database()"), "main", "back in the world it connected to");
    assert_eq!(code(&db, &mut s, "select currval('sq')"), "55000");
    assert_eq!(text(&db, &mut other, "select pg_try_advisory_lock(7)"), "true", "its advisory locks are let go");

    // and only a session outside a transaction: a pool sends it between clients, not inside one
    tag(&db, &mut s, "begin");
    assert_eq!(tag(&db, &mut s, "discard sequences"), "DISCARD SEQUENCES");
    assert_eq!(tag(&db, &mut s, "discard plans"), "DISCARD PLANS");
    assert_eq!(code(&db, &mut s, "discard all"), "25001");
    assert_eq!(code(&db, &mut s, "select 1"), "25P02", "an error aborts the transaction, as in Postgres");
    tag(&db, &mut s, "rollback");

    // DISCARD TEMP drops the temporary tables alone
    tag(&db, &mut s, "create temp table again (a int)");
    tag(&db, &mut s, "set app.org_id = '9'");
    assert_eq!(tag(&db, &mut s, "discard temp"), "DISCARD TEMP");
    assert_eq!(code(&db, &mut s, "select * from again"), "42P01");
    assert_eq!(text(&db, &mut s, "select current_setting('app.org_id')"), "9");
}

/// pg_stat_statements keeps up to 5000 statements for each database server: one that runs many distinct statements
/// pushes out only its own least-run ones, never another server's.
#[test]
fn pg_stat_statements_limit_is_each_servers() {
    let quiet = Db::new();
    let mut q = Session::new(MAIN);
    tag(&quiet, &mut q, "create extension pg_stat_statements");
    tag(&quiet, &mut q, "select 9 + 9 as pss_quiet");
    let busy = Db::new();
    let mut b = Session::new(MAIN);
    for i in 0..5200 {
        tag(&busy, &mut b, &format!("select 1 as pss_busy_{i}"));
    }
    assert_eq!(text(&quiet, &mut q, "select calls from pg_stat_statements where query like '%pss_quiet'"), "1");
}

/// A world the session had made isn't lost by DISCARD ALL, only left.
#[test]
fn discard_all_leaves_worlds_alone() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "create table t (id int primary key)");
    tag(&db, &mut s, "create world w");
    tag(&db, &mut s, "switch world w");
    tag(&db, &mut s, "insert into t values (1)");
    tag(&db, &mut s, "discard all");
    assert_eq!(text(&db, &mut s, "select count(*) from t"), "0");
    tag(&db, &mut s, "switch world w");
    assert_eq!(text(&db, &mut s, "select count(*) from t"), "1");
}

/// LOCK TABLE is taken by no writer, so it's a lock other LOCK TABLEs wait for.
#[test]
fn lock_table_holds_off_other_lockers() {
    let db = Arc::new(Db::new());
    let mut a = Session::new(MAIN);
    tag(&db, &mut a, "create table t (id int primary key)");
    tag(&db, &mut a, "create table u (id int primary key)");
    tag(&db, &mut a, "begin");
    tag(&db, &mut a, "lock table t in access exclusive mode");
    assert_eq!(messages().len(), 1, "it says what it does and doesn't hold off");

    // another session's lock on the same table waits for this transaction to end
    let waiting = {
        let db = db.clone();
        std::thread::spawn(move || {
            let mut b = Session::new(MAIN);
            b.run(&db, "begin", &[]).unwrap();
            let t = Instant::now();
            let out = b.run(&db, "lock table t in share row exclusive mode", &[]).map(|_| ()).map_err(|e| e.code);
            b.run(&db, "rollback", &[]).unwrap();
            (out, t.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    assert!(!waiting.is_finished(), "it waits");

    // the modes that only conflict with the strong ones, and other tables, don't wait
    let mut c = Session::new(MAIN);
    tag(&db, &mut c, "begin");
    let t = Instant::now();
    tag(&db, &mut c, "lock table t in access share mode");
    tag(&db, &mut c, "lock table t in row exclusive mode");
    tag(&db, &mut c, "lock table u in access exclusive mode");
    assert!(t.elapsed() < Duration::from_millis(250), "{:?}", t.elapsed());
    assert_eq!(code(&db, &mut c, "lock table t in exclusive mode nowait"), "55P03");
    tag(&db, &mut c, "rollback");

    tag(&db, &mut a, "commit");
    let (out, took) = waiting.join().unwrap();
    assert_eq!(out, Ok(()));
    assert!(took >= Duration::from_millis(250), "{took:?}");

    // once the holder is gone it can be taken again, and a statement timeout ends a wait
    tag(&db, &mut a, "begin");
    tag(&db, &mut a, "lock table t");
    tag(&db, &mut c, "set statement_timeout = 200");
    tag(&db, &mut c, "begin");
    let t = Instant::now();
    assert_eq!(code(&db, &mut c, "lock table t"), "57014");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    tag(&db, &mut c, "rollback");
    tag(&db, &mut a, "rollback");
}

/// Two transactions that each lock what the other wants: one is told, as Postgres tells it.
#[test]
fn lock_table_finds_a_deadlock() {
    let db = Arc::new(Db::new());
    let mut a = Session::new(MAIN);
    tag(&db, &mut a, "create table t (id int primary key)");
    tag(&db, &mut a, "create table u (id int primary key)");
    tag(&db, &mut a, "begin");
    tag(&db, &mut a, "lock table t in exclusive mode");
    let other = {
        let db = db.clone();
        std::thread::spawn(move || {
            let mut b = Session::new(MAIN);
            b.run(&db, "begin", &[]).unwrap();
            b.run(&db, "lock table u in exclusive mode", &[]).unwrap();
            let out = b.run(&db, "lock table t in exclusive mode", &[]).map(|_| ()).map_err(|e| e.code);
            b.run(&db, "rollback", &[]).unwrap();
            out
        })
    };
    std::thread::sleep(Duration::from_millis(300)); // b holds u, and waits for t
    let mine = a.run(&db, "lock table u in exclusive mode", &[]).map(|_| ()).map_err(|e| e.code);
    tag(&db, &mut a, "rollback");
    let theirs = other.join().unwrap();
    assert!(
        matches!((mine, theirs), (Err("40P01"), Ok(())) | (Ok(()), Err("40P01"))),
        "exactly one of the two is told about the deadlock"
    );
}
