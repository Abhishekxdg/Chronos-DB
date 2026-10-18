//! `chronos attach`: a folder beside a Postgres database, against a real one. CHRONOS_TEST_PG is a URL to a server
//! where tests may create databases (CI runs one; skipped without it). Attach copies the tables named and refuses
//! what it can't hold as Postgres does; a merge into main is written to Postgres in one transaction, a row changed
//! there meanwhile comes back as a conflict with nothing written, rows a trigger sets come back into main, and
//! main itself takes no direct writes. A crash on either side of COMMIT is settled at the next merge.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};
use std::process::Command;
use std::sync::Mutex;

/// The source a folder's merges write to is the process's CHRONOS_SOURCE_URL: tests here take turns.
static SOURCE: Mutex<()> = Mutex::new(());

/// A throwaway database on the test server: an admin connection to it, its name, its URL, and the turn.
fn throwaway(tag: &str) -> Option<(postgres::Client, String, String, std::sync::MutexGuard<'static, ()>)> {
    let admin_url = std::env::var("CHRONOS_TEST_PG").ok()?;
    let turn = SOURCE.lock().unwrap_or_else(|p| p.into_inner());
    let name = format!("chronos_attach_{tag}_{}", std::process::id());
    let mut admin = postgres::Client::connect(&admin_url, postgres::NoTls).unwrap();
    admin.batch_execute(&format!("drop database if exists {name}")).unwrap();
    admin.batch_execute(&format!("create database {name}")).unwrap();
    let url = match admin_url.rsplit_once('/') {
        Some((base, _)) => format!("{base}/{name}"),
        None => panic!("CHRONOS_TEST_PG needs a database in its path"),
    };
    let pg = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    // (each test sets the source before anything reads it, under its turn)
    unsafe { std::env::set_var("CHRONOS_SOURCE_URL", &url) };
    Some((pg, name, url, turn))
}

fn drop_db(name: &str) {
    let admin_url = std::env::var("CHRONOS_TEST_PG").unwrap();
    let mut admin = postgres::Client::connect(&admin_url, postgres::NoTls).unwrap();
    admin.batch_execute(&format!("drop database if exists {name} with (force)")).unwrap();
}

fn folder(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chronos-attach-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `chronos attach <dir> <args>`: whether it succeeded, and what it printed.
fn attach(dir: &std::path::Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_chronos")).arg("attach").arg(dir).args(args).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

fn cell(db: &Db, world: &str, sql: &str) -> String {
    run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap().rows[0][0].to_text()
}

fn pg_cell(pg: &mut postgres::Client, sql: &str) -> String {
    let r = pg.simple_query(sql).unwrap();
    r.iter()
        .find_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some(r.get(0).unwrap_or("NULL").to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Forks `w`, runs `sql` in it, and merges it: the merge's error, if any.
fn change(db: &Db, w: &str, sql: &str) -> Option<chronos::sql::SqlError> {
    run(db, MAIN, &format!("create world {w}"), &[]).unwrap();
    run(db, w, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    run(db, MAIN, &format!("merge world {w}"), &[]).err()
}

#[test]
fn refuses_what_it_cant_hold() {
    let Some((mut pg, name, url, _turn)) = throwaway("refuse") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table ok (id int primary key, v text);
         create table nokey (v text);
         create table gen (id int primary key, a int, b int generated always as (a * 2) stored);
         create table secured (id int primary key); alter table secured enable row level security;
         create table parted (id int, primary key (id)) partition by range (id);
         create view v as select * from ok;",
    )
    .unwrap();
    let dir = folder("refuse");
    let (ok, out) = attach(&dir, &["--tables", "ok,nokey,gen,secured,parted,v"]);
    assert!(!ok, "{out}");
    for why in [
        "public.nokey: no primary key",
        "public.gen: column b is generated",
        "public.secured: row-level security",
        "public.parted: a partitioned table",
        "public.v: a view",
    ] {
        assert!(out.contains(why), "{why}: {out}");
    }
    assert!(!out.contains("public.ok:"), "{out}");
    assert!(!dir.exists(), "nothing is left behind");
    // a table that isn't there
    let (ok, out) = attach(&dir, &["--tables", "ok,nothere"]);
    assert!(!ok && out.contains("no table public.nothere"), "{out}");
    // the URL is never an argument: without the environment's, attach says how to give it
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["attach", dir.to_str().unwrap(), "--tables", "ok"])
        .env_remove("CHRONOS_SOURCE_URL")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("CHRONOS_SOURCE_URL"));
    // a folder that exists is refused
    std::fs::create_dir_all(&dir).unwrap();
    let (ok, out) = attach(&dir, &["--tables", "ok"]);
    assert!(!ok && out.contains("exists"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
    let _ = url;
    drop(pg);
    drop_db(&name);
}

#[test]
fn merges_are_written_to_postgres_first() {
    let Some((mut pg, name, url, _turn)) = throwaway("merge") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table accounts (id text primary key, name text not null);
         create table leads (
             id uuid primary key, account text references accounts (id), who text not null, score numeric(10, 2),
             seen timestamptz, tags jsonb, ok boolean, ratio double precision, note varchar(20),
             updated_at timestamptz);
         create function touch() returns trigger language plpgsql as
             $$ begin new.updated_at := clock_timestamp(); return new; end $$;
         create trigger leads_touch before insert or update on leads for each row execute function touch();
         create table notes (id serial primary key, body text);
         create table tickets (id bigint generated always as identity primary key, title text);
         insert into accounts values ('acme', 'Acme'), ('globex', 'Globex');
         insert into leads (id, account, who, score, seen, tags, ok, ratio, note) values
             ('00000000-0000-0000-0000-000000000001', 'acme', 'ada', 99.50, '2026-10-17 10:00:00+05:30',
              '{\"a\": [1, 2]}', true, 0.1, 'x'),
             ('00000000-0000-0000-0000-000000000002', 'acme', 'bo', 10, null, null, false, 1e-300, null),
             ('00000000-0000-0000-0000-000000000003', 'globex', E'tab\\there', null, now(), '[]', null, -0.0, 'q''s'),
             ('00000000-0000-0000-0000-000000000004', 'globex', 'dee', 1, now(), '{}', true, 2.5, null),
             ('00000000-0000-0000-0000-000000000005', 'globex', 'eve', 2, now(), '{}', true, 3.5, null);
         insert into notes (body) values ('one');",
    )
    .unwrap();
    // the role's own settings differ from those attach reads with: the text compared mustn't
    let role = pg_cell(&mut pg, "select current_user");
    pg.batch_execute(&format!(
        "alter role \"{role}\" in database {name} set timezone = 'America/New_York'; \
         alter role \"{role}\" in database {name} set datestyle = 'SQL, DMY'"
    ))
    .unwrap();
    let dir = folder("merge");
    let (ok, out) = attach(&dir, &["--tables", "accounts,leads,notes,tickets"]);
    assert!(ok, "{out}");
    assert!(out.contains("attached 4 tables"), "{out}");
    assert!(out.contains("trigger leads_touch on leads: stays in Postgres"), "{out}");
    assert!(out.contains("\"notes\".\"id\": new values come from Postgres's sequence public.notes_id_seq"), "{out}");
    assert!(out.contains("\"tickets\".\"id\": new values come from Postgres's sequence"), "{out}");
    // no password anywhere in the folder
    let password = url.split_once("://").and_then(|(_, r)| r.split_once('@')).and_then(|(a, _)| a.split_once(':'));
    if let Some((_, pw)) = password {
        for e in walk(&dir) {
            let bytes = std::fs::read(&e).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains(pw), "{} holds the password", e.display());
        }
    }
    assert_eq!(pg_cell(&mut pg, "select count(*) from chronos.merges where id like 'attach:%'"), "1");

    let db = Db::open(&dir).unwrap();
    assert!(db.attached().is_some());
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "5");
    assert_eq!(cell(&db, MAIN, "select score from leads where who = 'ada'"), "99.50");

    // main takes no direct write, and nothing changes shape, in any world
    for q in ["update leads set score = 1", "insert into accounts values ('x', 'X')", "delete from leads"] {
        let e = run(&db, MAIN, q, &[]).unwrap_err();
        assert_eq!(e.code, "42501", "{q}: {}", e.message);
        assert!(e.message.contains("attached to Postgres"), "{q}: {}", e.message);
    }
    run(&db, MAIN, "create world shape", &[]).unwrap();
    for q in [
        "create table extra (id int primary key)",
        "alter table leads add column more text",
        "drop table notes",
        "truncate accounts",
        "create index leads_who on leads (who)",
        "create view vv as select 1",
    ] {
        let e = run(&db, "shape", q, &[]).unwrap_err();
        assert_eq!(e.code, "0A000", "{q}: {}", e.message);
        assert!(e.message.contains("attached to Postgres"), "{q}: {}", e.message);
    }
    run(&db, MAIN, "drop world shape", &[]).unwrap();
    assert!(db.put(MAIN, "other/1", r#"{"id": 1}"#).is_err(), "no table of its own through the row API");

    // an update, an insert and a delete, written to Postgres; the trigger's value comes back into main
    let e = change(
        &db,
        "w1",
        "update leads set score = 42.10, tags = '{\"b\": true}' where who = 'ada';
         insert into leads (id, account, who, score) values ('00000000-0000-0000-0000-0000000000aa', 'globex', 'new', 7);
         delete from leads where who = 'bo'",
    );
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(pg_cell(&mut pg, "select score from leads where who = 'ada'"), "42.10");
    assert_eq!(pg_cell(&mut pg, "select tags::text from leads where who = 'ada'"), "{\"b\": true}");
    assert_eq!(pg_cell(&mut pg, "select count(*) from leads where who = 'new'"), "1");
    assert_eq!(pg_cell(&mut pg, "select count(*) from leads where who = 'bo'"), "0");
    let pg_touched =
        pg_cell(&mut pg, "select (extract(epoch from updated_at) * 1000000)::bigint from leads where who = 'ada'");
    assert_eq!(
        cell(&db, MAIN, "select (extract(epoch from updated_at) * 1000000)::bigint from leads where who = 'ada'"),
        pg_touched
    );
    assert_eq!(cell(&db, MAIN, "select count(*) from leads where who = 'new' and updated_at is not null"), "1");
    assert_eq!(pg_cell(&mut pg, "select count(*) from chronos.merges where id like 'merge:%'"), "1");

    // again on the same row: the trigger's value main took compares equal (no false conflict), twice
    for (w, s) in [("w2", "43"), ("w3", "44")] {
        let e = change(&db, w, &format!("update leads set score = {s} where who = 'ada'"));
        assert!(e.is_none(), "{w}: {:?}", e.map(|e| e.message));
    }
    assert_eq!(pg_cell(&mut pg, "select score from leads where who = 'ada'"), "44.00");
    // every type compares equal as it came: a merge touching only one column of rows with all of them set
    let e = change(&db, "types", "update leads set note = 'n' where who in ('dee', 'eve', E'tab\\there')");
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));

    // a row changed in Postgres meanwhile: a conflict, nothing written, and main takes Postgres's row
    run(&db, MAIN, "create world w4", &[]).unwrap();
    run(&db, "w4", "update leads set score = 5 where who = 'dee'", &[]).unwrap();
    pg.batch_execute("update leads set score = 6 where who = 'dee'").unwrap();
    let e = run(&db, MAIN, "merge world w4", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in Postgres since main last had them"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select score from leads where who = 'dee'"), "6.00");
    assert_eq!(cell(&db, MAIN, "select score from leads where who = 'dee'"), "6.00", "main has Postgres's row");
    // merged again it is an ordinary conflict, settled as one: the world's value goes to Postgres
    let e = run(&db, MAIN, "merge world w4", &[]).unwrap_err();
    assert!(e.message.contains("changed on both sides"), "{}", e.message);
    run(&db, MAIN, "merge world w4 using ours", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select score from leads where who = 'dee'"), "5.00");

    // deleted in Postgres, updated in a world; a key inserted in both; deleted in a world, gone from Postgres
    for (w, world, there) in [
        ("w5", "update leads set score = 8 where who = 'eve'", "delete from leads where who = 'eve'"),
        (
            "w6",
            "insert into leads (id, who) values ('00000000-0000-0000-0000-0000000000bb', 'both')",
            "insert into leads (id, who) values ('00000000-0000-0000-0000-0000000000bb', 'pg')",
        ),
        ("w7", "delete from leads where who = 'new'", "delete from leads where who = 'new'"),
    ] {
        run(&db, MAIN, &format!("create world {w}"), &[]).unwrap();
        run(&db, w, world, &[]).unwrap();
        pg.batch_execute(there).unwrap();
        let before = pg_cell(&mut pg, "select count(*) || ':' || coalesce(sum(score), 0) from leads");
        let e = run(&db, MAIN, &format!("merge world {w}"), &[]).unwrap_err();
        assert_eq!(e.code, "40001", "{w}: {}", e.message);
        assert_eq!(pg_cell(&mut pg, "select count(*) || ':' || coalesce(sum(score), 0) from leads"), before, "{w}");
        run(&db, MAIN, &format!("drop world {w}"), &[]).unwrap();
    }
    assert_eq!(cell(&db, MAIN, "select count(*) from leads where who = 'eve'"), "0", "main took the delete");
    assert_eq!(cell(&db, MAIN, "select who from leads where id = '00000000-0000-0000-0000-0000000000bb'"), "pg");

    // a primary key changed in a world: a delete and an insert there (the trigger's value comes back for the insert)
    let e = change(&db, "w8", "update leads set id = '00000000-0000-0000-0000-0000000000cc' where who = 'dee'");
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(
        pg_cell(&mut pg, "select id::text from leads where who = 'dee'"),
        "00000000-0000-0000-0000-0000000000cc"
    );
    assert_eq!(pg_cell(&mut pg, "select count(*) from leads where id = '00000000-0000-0000-0000-000000000004'"), "0");
    let touched =
        pg_cell(&mut pg, "select (extract(epoch from updated_at) * 1000000)::bigint from leads where who = 'dee'");
    assert_eq!(
        cell(&db, MAIN, "select (extract(epoch from updated_at) * 1000000)::bigint from leads where who = 'dee'"),
        touched
    );

    // inserts into tables keyed by a serial or an identity: ids reserved from Postgres's own sequences, kept at merge
    let e = change(
        &db,
        "w9",
        "insert into notes (body) values ('two'), ('three'); insert into tickets (title) values ('t1')",
    );
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(pg_cell(&mut pg, "select count(*) from notes"), "3");
    for (t, c) in [("notes", "body"), ("tickets", "title")] {
        let pg_ids = pg_cell(&mut pg, &format!("select string_agg(id || '=' || {c}, ',' order by id) from {t}"));
        assert_eq!(
            cell(&db, MAIN, &format!("select string_agg(id || '=' || {c}, ',' order by id) from {t}")),
            pg_ids,
            "{t}"
        );
    }
    // Postgres's own inserts go on past what the folder reserved: no collision
    pg.batch_execute("insert into notes (body) values ('pg'); insert into tickets (title) values ('pg')").unwrap();
    let reserved = cell(&db, MAIN, "select max(id) from notes");
    assert!(pg_cell(&mut pg, "select max(id) from notes").parse::<i64>().unwrap() > reserved.parse::<i64>().unwrap());
    assert!(dir.join("attached.ids").exists(), "the reserved ids are kept in the folder");
    // an id given by hand isn't one Postgres reserved: refused, nothing written
    let e = change(&db, "w9b", "insert into notes (id, body) values (500000, 'mine')").unwrap();
    assert!(e.message.contains("isn't one this folder reserved"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select count(*) from notes where id = 500000"), "0");
    run(&db, MAIN, "drop world w9b", &[]).unwrap();
    // ids taken before a restart still merge after it
    run(&db, MAIN, "create world w9c", &[]).unwrap();
    run(&db, "w9c", "insert into notes (body) values ('before restart')", &[]).unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "merge world w9c", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select count(*) from notes where body = 'before restart'"), "1");
    let e = change(&db, "w10", "update notes set body = 'uno' where body = 'one'");
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(pg_cell(&mut pg, "select body from notes order by id limit 1"), "uno");

    // a constraint Postgres refuses: its message, nothing written
    let e = change(&db, "w11", "update accounts set name = 'z' where id = 'acme'");
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    pg.batch_execute("alter table accounts add constraint short check (length(name) < 10)").unwrap();
    let e = change(&db, "w12", "update accounts set name = 'much too long' where id = 'acme'").unwrap();
    assert!(e.message.contains("short"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select name from accounts where id = 'acme'"), "z");
    run(&db, MAIN, "drop world w12", &[]).unwrap();

    // an agent's merge, with no merge policy of its own, waits for a person and writes nothing
    run(&db, MAIN, "create agent bot with (can = 'read,fork,write_own,write_main,merge_own')", &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(&db, "create world b1; use world b1; update leads set score = 77 where who = 'ada'; use world main", &[])
        .unwrap();
    let e = bot.run(&db, "merge world b1", &[]).unwrap_err();
    assert!(e.message.contains("SHOW REVIEWS") && e.message.contains("writes to Postgres"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select score from leads where who = 'ada'"), "44.00");
    run(&db, MAIN, "merge world b1", &[]).unwrap(); // a person approves by merging it
    assert_eq!(pg_cell(&mut pg, "select score from leads where who = 'ada'"), "77.00");

    // the ways straight into main are refused too
    for q in ["undo merge of world w10", "restore world main to '-1 second'"] {
        let e = run(&db, MAIN, q, &[]).unwrap_err();
        assert!(e.message.contains("attached to Postgres"), "{q}: {}", e.message);
    }

    // a Postgres that isn't the one attached: refused, nothing written
    let other = format!("{name}_other");
    let admin_url = std::env::var("CHRONOS_TEST_PG").unwrap();
    let mut admin = postgres::Client::connect(&admin_url, postgres::NoTls).unwrap();
    admin.batch_execute(&format!("drop database if exists {other}")).unwrap();
    admin.batch_execute(&format!("create database {other} template {name}")).unwrap_or_else(|_| {
        // (a template in use can't be copied: one made by hand)
        admin.batch_execute(&format!("create database {other}")).unwrap();
    });
    let other_url = format!("{}/{other}", url.rsplit_once('/').unwrap().0);
    let mut o = postgres::Client::connect(&other_url, postgres::NoTls).unwrap();
    o.batch_execute("create schema if not exists chronos; create table if not exists chronos.merges (id text primary key, at timestamptz); delete from chronos.merges").unwrap();
    drop(o);
    unsafe { std::env::set_var("CHRONOS_SOURCE_URL", &other_url) };
    let e = change(&db, "w13", "update accounts set name = 'q' where id = 'acme'").unwrap();
    assert!(e.message.contains("isn't the one this folder was attached to"), "{}", e.message);
    unsafe { std::env::set_var("CHRONOS_SOURCE_URL", &url) };
    run(&db, MAIN, "drop world w13", &[]).unwrap();
    admin.batch_execute(&format!("drop database {other} with (force)")).unwrap();

    // more than 10,000 rows in one merge: refused, said how
    run(&db, MAIN, "create world big", &[]).unwrap();
    run(&db, "big", "insert into accounts select 'a' || g, 'n' from generate_series(1, 10001) g", &[]).unwrap();
    let e = run(&db, MAIN, "merge world big", &[]).unwrap_err();
    assert!(e.message.contains("more than 10000"), "{}", e.message);
    run(&db, MAIN, "drop world big", &[]).unwrap();

    // the schema changed in Postgres: the merge stops and names it
    pg.batch_execute("alter table accounts add column region text").unwrap();
    let e = change(&db, "w14", "update accounts set name = 'r' where id = 'acme'").unwrap();
    assert!(
        e.message.contains("changed in Postgres since the attach") && e.message.contains("region"),
        "{}",
        e.message
    );

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// Every file under `dir`.
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// kill -9 on either side of COMMIT: the next merge settles it, and Postgres and main agree. Debug builds only
/// (the crash points are compiled out of release ones).
#[test]
fn a_crash_around_commit_is_settled() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: crash points are in debug builds only");
        return;
    }
    let Some((mut pg, name, _url, _turn)) = throwaway("crash") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute("create table t (id int primary key, v text); insert into t values (1, 'a'), (2, 'b')").unwrap();
    let dir = folder("crash");
    let (ok, out) = attach(&dir, &["--tables", "t"]);
    assert!(ok, "{out}");
    for (at, world, v) in [("before_commit", "c1", "x"), ("after_commit", "c2", "y")] {
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, &format!("create world {world}"), &[]).unwrap();
        run(&db, world, &format!("update t set v = '{v}' where id = 1"), &[]).unwrap();
        drop(db);
        let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
            .args(["-b", world, dir.to_str().unwrap(), "merge"])
            .env("CHRONOS_ATTACH_CRASH", at)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{at}: it crashed");
        assert!(dir.join("attached.pending").exists(), "{at}: the outcome is left to settle");
        let db = Db::open(&dir).unwrap();
        // the next merge into main settles it first
        let e = change(&db, &format!("{world}_next"), "update t set v = 'z' where id = 2");
        assert!(e.is_none(), "{at}: {:?}", e.map(|e| e.message));
        assert!(!dir.join("attached.pending").exists(), "{at}");
        let pg_v = pg_cell(&mut pg, "select v from t where id = 1");
        assert_eq!(cell(&db, MAIN, "select v from t where id = 1"), pg_v, "{at}: Postgres and main agree");
        match at {
            // never committed: Postgres doesn't have it, and the world is still there to merge again
            "before_commit" => {
                assert_eq!(pg_v, "a");
                assert!(db.world(world).is_ok(), "{at}");
                run(&db, MAIN, &format!("drop world {world}"), &[]).unwrap();
            }
            // committed: the merge is made here too
            _ => {
                assert_eq!(pg_v, "y");
                assert!(db.world(world).is_err(), "{at}: merged");
            }
        }
        assert_eq!(pg_cell(&mut pg, "select v from t where id = 2"), "z");
        run(&db, MAIN, "create world reset", &[]).unwrap();
        run(&db, "reset", "update t set v = 'b' where id = 2", &[]).unwrap();
        run(&db, MAIN, "merge world reset", &[]).unwrap();
        drop(db);
    }
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// Two worlds merging into the same attached table at once: one after the other, both written.
#[test]
fn merges_at_once_take_turns() {
    let Some((mut pg, name, _url, _turn)) = throwaway("turns") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table t (id int primary key, v int); insert into t select g, 0 from generate_series(1, 20) g",
    )
    .unwrap();
    let dir = folder("turns");
    let (ok, out) = attach(&dir, &["--tables", "t"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    for i in 1..=8 {
        run(&db, MAIN, &format!("create world m{i}"), &[]).unwrap();
        run(&db, &format!("m{i}"), &format!("update t set v = {i} where id = {i}"), &[]).unwrap();
    }
    std::thread::scope(|s| {
        for i in 1..=8 {
            let db = &db;
            s.spawn(move || run(db, MAIN, &format!("merge world m{i}"), &[]).unwrap());
        }
    });
    assert_eq!(pg_cell(&mut pg, "select sum(v) from t"), "36");
    assert_eq!(cell(&db, MAIN, "select sum(v) from t"), "36");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// `MERGE WORLD x DRY RUN` in an attached folder checks the rows it would write against Postgres as it is now, in a
/// read-only transaction it rolls back: a row changed there since the copy is marked, with Postgres's value, and
/// nothing is written to Postgres or taken into main (as a real merge's conflict would). Issue #26.
#[test]
fn a_dry_run_checks_postgres_without_writing() {
    let Some((mut pg, name, url, _turn)) = throwaway("dry") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table t (id int primary key, v text, touched timestamptz);
         create function touch() returns trigger language plpgsql as
             $$ begin new.touched := clock_timestamp(); return new; end $$;
         -- (on insert only: the test sets a row back later, which must then match the copy again)
         create trigger t_touch before insert on t for each row execute function touch();
         insert into t (id, v) values (1, 'a'), (2, 'b'), (3, 'c');",
    )
    .unwrap();
    let dir = folder("dry");
    let (ok, out) = attach(&dir, &["--tables", "t"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "create world d", &[]).unwrap();
    run(
        &db,
        "d",
        "update t set v = 'x' where id = 1; update t set v = 'y' where id = 2; delete from t where id = 3",
        &[],
    )
    .unwrap();
    // changed in Postgres since the copy
    pg.batch_execute("update t set v = 'pg' where id = 2").unwrap();
    let state = |pg: &mut postgres::Client| pg_cell(pg, "select string_agg(id || '=' || v, ',' order by id) from t");
    let before = state(&mut pg);

    let out = run(&db, MAIN, "merge world d dry run", &[]).unwrap().pop().unwrap();
    let col = |n: &str| {
        out.columns.as_ref().unwrap().iter().position(|(c, _)| c == n).unwrap_or_else(|| panic!("no column {n}"))
    };
    let (id, outcome, detail, postgres) = (col("id"), col("outcome"), col("detail"), col("postgres"));
    let row = |want: &str| {
        out.rows.iter().find(|r| r[id].to_text() == want).unwrap_or_else(|| panic!("no row {want}: {:?}", out.rows))
    };
    // would apply: Postgres's row beside it
    assert_eq!(row("1")[outcome].to_text(), "apply");
    assert!(row("1")[postgres].to_text().contains(r#""v": "a""#), "{}", row("1")[postgres].to_text());
    // changed there: marked, with Postgres's value
    assert_eq!(row("2")[outcome].to_text(), "changed in postgres");
    assert!(row("2")[postgres].to_text().contains(r#""v": "pg""#), "{}", row("2")[postgres].to_text());
    // a delete Postgres still has: applies
    assert_eq!(row("3")[outcome].to_text(), "apply");
    let line = |o: &str| out.rows.iter().find(|r| r[outcome].to_text() == o).map(|r| r[detail].to_text());
    let summary = line("postgres").expect("a line saying what Postgres was checked against");
    assert!(summary.contains("2 rows would apply, 1 changed in Postgres"), "{summary}");
    assert!(summary.contains("no locks") && summary.contains("t_touch"), "{summary}");
    let blocked = line("blocked").expect("blocked: a merge now would be refused");
    assert!(blocked.contains("changed in Postgres since main's copy"), "{blocked}");

    // nothing written to Postgres, no merge recorded there, and main didn't take Postgres's row
    assert_eq!(state(&mut pg), before);
    assert_eq!(pg_cell(&mut pg, "select count(*) from chronos.merges where id like 'merge:%'"), "0");
    assert_eq!(cell(&db, MAIN, "select v from t where id = 2"), "b");
    assert!(db.world("d").is_ok(), "the world stays");

    // the same through the API: each row's Postgres value, and when it was read
    let plan = db.merge_preview("d", &Default::default()).unwrap();
    let live = plan.live.expect("checked against Postgres");
    assert!(live.error.is_none() && live.at > 0, "{live:?}");
    assert_eq!(live.rows.iter().filter(|r| r.changed).count(), 1);
    assert_eq!(live.triggers, ["t: t_touch"]);

    // Postgres out of reach: the plan still comes back, saying it wasn't checked
    unsafe { std::env::set_var("CHRONOS_SOURCE_URL", "postgres://nobody:x@127.0.0.1:1/nowhere") };
    let plan = db.merge_preview("d", &Default::default()).unwrap();
    unsafe { std::env::set_var("CHRONOS_SOURCE_URL", &url) };
    assert!(plan.live.as_ref().is_some_and(|l| l.error.is_some()), "{:?}", plan.live);

    // in step again: the dry run finds nothing changed, and the merge goes through as it said
    pg.batch_execute("update t set v = 'b' where id = 2").unwrap();
    let plan = db.merge_preview("d", &Default::default()).unwrap();
    assert!(plan.blocked.is_none(), "{:?}", plan.blocked);
    assert!(plan.live.unwrap().rows.iter().all(|r| !r.changed));
    run(&db, MAIN, "merge world d", &[]).unwrap();
    assert_eq!(state(&mut pg), "1=x,2=y");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// A world that checks its reads has them checked against Postgres too, at the write-back: an agent finds no row
/// for an email in a suppression list, decides on that, and before its merge someone adds the email straight into
/// Postgres. The merge is held, naming what it read, and nothing is written. A row added there that its read couldn't
/// have found doesn't hold it. Issue #28.
#[test]
fn reads_are_checked_against_postgres() {
    let Some((mut pg, name, _url, _turn)) = throwaway("reads") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, email text, status text);
         create table suppressed (id serial primary key, email text);
         create table optout (email text primary key);
         insert into leads values (1, 'ada@x.com', 'new'), (2, 'bo@x.com', 'new');",
    )
    .unwrap();
    let dir = folder("reads");
    let (ok, out) = attach(&dir, &["--tables", "leads,suppressed,optout"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();

    // a filtered scan that found nothing
    run(&db, MAIN, "create world w with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "w", "select count(*) from suppressed where email = 'ada@x.com'"), "0");
    run(&db, "w", "update leads set status = 'ready_to_contact' where id = 1", &[]).unwrap();
    // a row its read couldn't have found: still goes through (checked in a dry run)
    pg.batch_execute("insert into suppressed (email) values ('someone@else.com')").unwrap();
    let plan = db.merge_preview("w", &Default::default()).unwrap();
    assert!(plan.blocked.is_none(), "{:?}", plan.blocked);
    assert!(plan.live.as_ref().is_some_and(|l| l.stale_reads.is_empty()), "{:?}", plan.live);
    // CHECK READS between steps: nothing it relied on moved yet
    assert_eq!(run(&db, "w", "check reads", &[]).unwrap().pop().unwrap().rows.len(), 0);
    // the email itself, added straight into Postgres: CHECK READS says so before any merge (against Postgres, not
    // main, which never saw it), the world's view stays as it was, and the dry run says it would be held
    pg.batch_execute("insert into suppressed (email) values ('ada@x.com')").unwrap();
    let found = run(&db, MAIN, "check reads of world w", &[]).unwrap().pop().unwrap();
    assert_eq!(found.rows.len(), 1, "{:?}", found.rows);
    assert!(found.rows[0][0].to_text().starts_with("Postgres ("), "{:?}", found.rows);
    assert!(found.rows[0][1].to_text().contains("changed in Postgres since it forked"), "{:?}", found.rows);
    assert_eq!(cell(&db, "w", "select count(*) from suppressed where email = 'ada@x.com'"), "0", "no rebase");
    let plan = db.merge_preview("w", &Default::default()).unwrap();
    let blocked = plan.blocked.unwrap_or_default();
    assert!(blocked.contains("changed in Postgres since it forked"), "{blocked}");
    let e = run(&db, MAIN, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in Postgres since it forked"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select status from leads where id = 1"), "new", "nothing written");
    assert_eq!(pg_cell(&mut pg, "select count(*) from chronos.merges where id like 'merge:%'"), "0");
    assert!(db.world("w").is_ok(), "the world stays, to redo or drop");
    run(&db, MAIN, "drop world w", &[]).unwrap();

    // a key looked up that wasn't there, added in Postgres since
    run(&db, MAIN, "create world k with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "k", "select count(*) from optout where email = 'bo@x.com'"), "0");
    run(&db, "k", "update leads set status = 'ready_to_contact' where id = 2", &[]).unwrap();
    pg.batch_execute("insert into optout values ('bo@x.com')").unwrap();
    let e = run(&db, MAIN, "merge world k", &[]).unwrap_err();
    assert!(e.message.contains("optout"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select status from leads where id = 2"), "new");
    run(&db, MAIN, "drop world k", &[]).unwrap();

    // a row read and then changed in Postgres
    run(&db, MAIN, "create world r with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "r", "select status from leads where id = 2"), "new");
    run(&db, "r", "update leads set status = 'ready_to_contact' where id = 1", &[]).unwrap();
    pg.batch_execute("update leads set status = 'do_not_contact' where id = 2").unwrap();
    let e = run(&db, MAIN, "merge world r", &[]).unwrap_err();
    assert!(e.message.contains("leads"), "{}", e.message);
    run(&db, MAIN, "drop world r", &[]).unwrap();

    // a world that doesn't check its reads merges as before
    let e = change(&db, "plain", "update leads set status = 'ready_to_contact' where id = 1");
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(pg_cell(&mut pg, "select status from leads where id = 1"), "ready_to_contact");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// "Found nothing, then production added it": an agent looks for a lead, finds none, creates one and queues a welcome;
/// Postgres gets a matching lead straight before the merge. The merge is held or refused, with neither the lead nor
/// the welcome landing: through the world's own reads, through a merge check run in Postgres inside the write-back
/// (main, Chronos's copy, never saw the row), and queued for a person under an agent's merge policy. Issue #39.
#[test]
fn a_row_added_in_postgres_after_an_empty_read_holds_the_merge() {
    let Some((mut pg, name, _url, _turn)) = throwaway("absent") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute("create table leads (id int primary key, email text); insert into leads values (1, 'bo@x.com')")
        .unwrap();
    let dir = folder("absent");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // forks `w` (`with`), finds no lead for ada, creates one (id `next`) and queues the welcome; Postgres then gets its
    // own
    let create_if_missing = |pg: &mut postgres::Client, s: &mut Session, w: &str, with: &str, next: i32| {
        s.run(&db, &format!("create world {w}{with}; use world {w}"), &[]).unwrap();
        let found = s.run(&db, "select count(*) from leads where lower(email) = 'ada@x.com'", &[]).unwrap();
        assert_eq!(found[0].rows[0][0].to_text(), "0");
        s.run(
            &db,
            &format!(
                "insert into leads values ({next}, 'ada@x.com'); notify on merge crm, '{{\"welcome\": \"ada@x.com\"}}'; use world main"
            ),
            &[],
        )
        .unwrap();
        pg.batch_execute(&format!("insert into leads values ({}, 'Ada@X.com')", next + 1)).unwrap();
    };
    let nothing_landed = |pg: &mut postgres::Client, db: &Db, w: &str| {
        assert_eq!(pg_cell(pg, "select count(*) from leads where lower(email) = 'ada@x.com'"), "1", "only Postgres's");
        assert_eq!(pg_cell(pg, "select count(*) from chronos.merges where id like 'merge:%'"), "0");
        assert_eq!(run(db, MAIN, "show outbox", &[]).unwrap().pop().unwrap().rows.len(), 0, "no welcome queued");
        assert!(db.world(w).is_ok(), "the world stays, to redo or drop");
        pg.batch_execute("delete from leads where lower(email) = 'ada@x.com'").unwrap();
        run(db, MAIN, &format!("drop world {w}"), &[]).unwrap();
    };
    let mut me = Session::new(MAIN);

    // the world checks its reads: the empty read is checked against Postgres in the write-back
    create_if_missing(&mut pg, &mut me, "r", " with (check_reads = true)", 10);
    let e = run(&db, MAIN, "merge world r", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in Postgres since it forked"), "{}", e.message);
    nothing_landed(&mut pg, &db, "r");

    // it doesn't: a merge check on duplicate emails catches it, run in Postgres as the write-back leaves it
    run(
        &db,
        MAIN,
        "create merge check one_lead_per_email on tables (leads) as \
         select lower(email) as email, count(*) as n from leads group by 1 having count(*) > 1",
        &[],
    )
    .unwrap();
    create_if_missing(&mut pg, &mut me, "c", "", 20);
    let e = run(&db, MAIN, "merge world c", &[]).unwrap_err();
    assert_eq!(e.code, "23514", "{}", e.message);
    assert!(e.message.contains("breaks merge check one_lead_per_email in Postgres"), "{}", e.message);
    assert!(e.message.contains(r#""email":"ada@x.com","n":2"#), "the rows it found: {}", e.message);
    nothing_landed(&mut pg, &db, "c");

    // an agent keeping to a merge policy: queued for a person, not refused, and nothing written
    run(&db, MAIN, "create merge policy p", &[]).unwrap();
    run(&db, MAIN, "create agent bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'p')", &[])
        .unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    create_if_missing(&mut pg, &mut bot, "b", "", 30);
    let e = bot.run(&db, "merge world b", &[]).unwrap_err();
    assert!(e.message.contains("SHOW REVIEWS"), "{}", e.message);
    let reviews = db.reviews();
    assert_eq!(reviews.len(), 1);
    assert!(format!("{:?}", reviews[0]).contains("in Postgres"), "{:?}", reviews[0]);
    nothing_landed(&mut pg, &db, "b");

    // without the duplicate, the same merge goes through, welcome and all
    create_if_missing(&mut pg, &mut me, "ok", "", 40);
    pg.batch_execute("delete from leads where email = 'Ada@X.com'").unwrap();
    run(&db, MAIN, "merge world ok", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select count(*) from leads where lower(email) = 'ada@x.com'"), "1");
    assert_eq!(run(&db, MAIN, "show outbox", &[]).unwrap().pop().unwrap().rows.len(), 1);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}
