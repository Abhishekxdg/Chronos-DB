//! `chronos attach`: a folder beside a Postgres database, against a real one. CHRONOS_TEST_PG is a URL to a server
//! where tests may create databases (CI runs one; skipped without it). Attach copies the tables named and refuses
//! what it can't hold as Postgres does; a merge into main is written to Postgres in one transaction, a row changed
//! there meanwhile comes back as a conflict with nothing written, rows a trigger sets come back into main, and
//! main itself takes no direct writes. A crash on either side of COMMIT is settled at the next merge.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};
use std::process::Command;
use std::sync::Mutex;

#[path = "common/write_matrix.rs"]
mod write_matrix;

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
    force_drop(&mut admin, name);
}

/// `drop database ... with (force)`, which Postgres 12 hasn't: its sessions ended first.
/// (Tried again a few times: a pooler in front, as Supabase's, may open a session to it between the two.)
fn force_drop(admin: &mut postgres::Client, name: &str) {
    // `with (force)` (13+) ends the sessions and drops in one step, before a pooler like Supavisor can reconnect
    if admin.batch_execute(&format!("drop database if exists {name} with (force)")).is_ok() {
        return;
    }
    for left in (0..10).rev() {
        admin
            .batch_execute(&format!(
                "select pg_terminate_backend(pid) from pg_stat_activity where datname = '{name}' and pid <> pg_backend_pid()"
            ))
            .unwrap();
        match admin.batch_execute(&format!("drop database if exists {name}")) {
            Ok(()) => return,
            Err(e) if left == 0 => panic!("drop database {name}: {e}"),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
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

/// Postgres 11 or older is refused before anything is read or made there, saying so (rather than "column
/// attgenerated does not exist"); 12 or later attaches. (Run with CHRONOS_TEST_PG on an 11 to see the refusal.)
#[test]
fn refuses_postgres_older_than_12() {
    let Some((mut pg, name, _url, _turn)) = throwaway("old") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    let num: u32 = pg_cell(&mut pg, "show server_version_num").parse().unwrap();
    pg.batch_execute("create table ok (id int primary key, v text); insert into ok values (1, 'a')").unwrap();
    let dir = folder("old");
    let (ok, out) = attach(&dir, &["--tables", "ok"]);
    if num < 120000 {
        assert!(!ok && out.contains("attach needs Postgres 12 or later"), "{out}");
        assert!(!dir.exists(), "nothing is left behind");
        assert_eq!(pg_cell(&mut pg, "select to_regclass('chronos.merges') is null"), "t", "nothing made in Postgres");
    } else {
        assert!(ok, "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
    drop(pg);
    drop_db(&name);
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
         create table parted (id int, primary key (id)) partition by range (id);
         create view v as select * from ok;",
    )
    .unwrap();
    let dir = folder("refuse");
    // (row-level security that hides rows from the role: `tests/attach_compat.rs`, as a role that isn't a superuser)
    let (ok, out) = attach(&dir, &["--tables", "ok,nokey,parted,v"]);
    assert!(!ok, "{out}");
    for why in ["public.nokey: no primary key", "public.v: a view"] {
        assert!(out.contains(why), "{why}: {out}");
    }
    assert!(!out.contains("public.ok:") && !out.contains("public.parted:"), "{out}");
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
    assert_eq!(db.attached().unwrap().kind().as_str(), "postgres");
    assert!(db.attached().unwrap().capabilities().follow && db.attached().unwrap().capabilities().act_as);
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

    // the ways straight into main are refused too (an undo goes back through Postgres: see undo_of_a_merge_...)
    let e = run(&db, MAIN, "restore world main to '-1 second'", &[]).unwrap_err();
    assert!(e.message.contains("attached to Postgres"), "{}", e.message);

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
    force_drop(&mut admin, &other);

    // more rows in one merge than CHRONOS_ATTACH_MAX_ROWS: refused, said how
    unsafe { std::env::set_var("CHRONOS_ATTACH_MAX_ROWS", "10000") };
    run(&db, MAIN, "create world big", &[]).unwrap();
    run(&db, "big", "insert into accounts select 'a' || g, 'n' from generate_series(1, 10001) g", &[]).unwrap();
    let e = run(&db, MAIN, "merge world big", &[]).unwrap_err();
    unsafe { std::env::remove_var("CHRONOS_ATTACH_MAX_ROWS") };
    assert!(e.message.contains("more than 10000"), "{}", e.message);
    run(&db, MAIN, "drop world big", &[]).unwrap();

    // a column added in Postgres: the merge stops until a refresh takes it
    pg.batch_execute("alter table accounts add column region text").unwrap();
    let e = change(&db, "w14", "update accounts set name = 'r' where id = 'acme'").unwrap();
    assert!(e.message.contains("a refresh takes them") && e.message.contains("region"), "{}", e.message);
    // a column retyped: the merge stops and names it until a refresh takes it
    pg.batch_execute("alter table accounts alter column name type varchar(80)").unwrap();
    let e = change(&db, "w15", "update accounts set name = 'r' where id = 'acme'").unwrap();
    assert!(
        e.message.contains("a refresh takes it") && e.message.contains("column name now character varying(80)"),
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

/// A world that runs its reads again (`rerun_reads`), attached: rows it read that changed in Postgres since its fork
/// hold its merge only when its SELECTs, run again on Postgres's rows, give another answer; the reason says what they
/// gave and give there. Issue #38.
#[test]
fn reads_run_again_on_postgres_hold_only_a_changed_answer() {
    let Some((mut pg, name, _url, _turn)) = throwaway("rerun") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table orders (id int primary key, status text, amount int);
         create table tasks (id int primary key, done bool);
         insert into orders values (1, 'open', 10), (2, 'open', 20), (3, 'closed', 30);
         insert into tasks values (1, false), (2, false);",
    )
    .unwrap();
    let dir = folder("rerun");
    let (ok, out) = attach(&dir, &["--tables", "orders,tasks"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let sum = "select sum(amount) from orders where status = 'open'";

    // the sum came out the same in Postgres: merged, and written there
    run(&db, MAIN, "create world same with (rerun_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "same", sum), "30");
    run(&db, "same", "update tasks set done = true where id = 1", &[]).unwrap();
    pg.batch_execute("update orders set amount = 15 where id in (1, 2)").unwrap();
    run(&db, MAIN, "merge world same", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select done from tasks where id = 1"), "t");

    // another answer there: held, saying what it gave and gives in Postgres, and nothing written
    run(&db, MAIN, "create world other with (rerun_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "other", sum), "30");
    run(&db, "other", "update tasks set done = true where id = 2", &[]).unwrap();
    pg.batch_execute("update orders set amount = 16 where id = 1").unwrap();
    let e = run(&db, MAIN, "merge world other", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in Postgres since it forked"), "{}", e.message);
    assert!(e.message.contains("which gave 30 then and gives 31 in Postgres now"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select done from tasks where id = 2"), "f", "nothing written");

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

#[test]
fn a_tenant_read_of_a_big_table_is_checked_by_the_rows_it_can_match() {
    let Some((mut pg, name, _url, _turn)) = throwaway("tenantread") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    // more rows than a read can be checked by reading the table whole (100,000); tenant 42 has 110, 73 open
    pg.batch_execute(
        "create table leads (id int primary key, org_id int not null, status text not null, name text);
         create index on leads (org_id);
         insert into leads select g, g % 1000, case when g % 3 = 0 then 'won' else 'open' end, 'lead ' || g
             from generate_series(1, 110000) g;",
    )
    .unwrap();
    let dir = folder("tenantread");
    let (ok, out) = attach(&dir, &["--tables", "leads", "--yes"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let read = "select count(*) from leads where org_id = 42 and status = 'open'";

    // the app keeps writing other tenants' rows, and a row of tenant 42 the read couldn't match: no hold
    run(&db, MAIN, "create world w with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "w", read), "73");
    run(&db, "w", "update leads set name = 'called' where id = 1042", &[]).unwrap();
    pg.batch_execute(
        "update leads set name = 'app' where org_id = 7; update leads set name = 'app' where id = 3042;
         insert into leads values (200007, 7, 'open', 'new');",
    )
    .unwrap();
    run(&db, MAIN, "merge world w", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 1042"), "called");

    // a row the read matched, changed in Postgres since: held, nothing written
    run(&db, MAIN, "create world changed with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "changed", read), "73");
    run(&db, "changed", "update leads set name = 'again' where id = 1042", &[]).unwrap();
    pg.batch_execute("update leads set status = 'won' where id = 2042").unwrap();
    let e = run(&db, MAIN, "merge world changed", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in Postgres since it forked"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 1042"), "called", "nothing written");
    run(&db, MAIN, "drop world changed", &[]).unwrap();

    // a row the read would match now, added in Postgres since: held too
    run(&db, MAIN, "create world added with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "added", read), "73");
    run(&db, "added", "update leads set name = 'again' where id = 1042", &[]).unwrap();
    pg.batch_execute("insert into leads values (200042, 42, 'open', 'new')").unwrap();
    let e = run(&db, MAIN, "merge world added", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in Postgres since it forked"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 1042"), "called", "nothing written");
    run(&db, MAIN, "drop world added", &[]).unwrap();

    // a key looked up that found no row, added in Postgres since with values the scan's filter doesn't match: held
    // too (the table is read whole for it; this one has no more than 100,000 rows the check can read)
    pg.batch_execute("delete from leads where id > 90000").unwrap();
    chronos::attach::refresh(&db).unwrap();
    run(&db, MAIN, "create world absent with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "absent", read), "59");
    assert_eq!(cell(&db, "absent", "select count(*) from leads where id = 95000"), "0");
    run(&db, "absent", "update leads set name = 'again' where id = 1042", &[]).unwrap();
    pg.batch_execute("insert into leads values (95000, 7, 'won', 'new')").unwrap();
    let e = run(&db, MAIN, "merge world absent", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in Postgres since it forked"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 1042"), "called", "nothing written");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// A tenant agent's read of a child table of more than 100,000 rows (held to its tenant through a foreign key) is
/// checked against its tenant's rows alone, in Chronos and in Postgres: the app writing other tenants' rows doesn't
/// hold its merge, a row of its own changed or added in Postgres does. A person merges it, as one approves a held run:
/// the reads stay the tenant's.
#[test]
fn a_tenants_read_of_a_big_child_table_is_checked_by_its_tenants_rows() {
    let Some((mut pg, name, _url, _turn)) = throwaway("tenantchild") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    // tenant 42 has 20 of the 2,000 leads, and 1,200 of the 120,000 notes
    pg.batch_execute(
        "create table leads (id int primary key, org_id int not null, name text);
         create table notes (id int primary key, lead_id int not null references leads (id), body text);
         insert into leads select g, g % 100, 'lead ' || g from generate_series(1, 2000) g;
         insert into notes select g, g % 2000 + 1, 'note ' || g from generate_series(1, 120000) g;",
    )
    .unwrap();
    let dir = folder("tenantchild");
    let (ok, out) = attach(&dir, &["--tables", "leads,notes", "--yes"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent run42 with (can = '{can}', tenant = 'org_id = 42')"), &[]).unwrap();
    let agent = || {
        let mut s = Session::new(MAIN);
        s.agent = Some("run42".into());
        s
    };
    let work = |w: &str| {
        let sql = format!(
            "create world {w} with (check_reads = true); use world {w}; select count(*) from notes;
             update leads set name = '{w}' where id = 42; use world main"
        );
        let out = agent().run(&db, &sql, &[]).unwrap();
        assert_eq!(out[2].rows[0][0].to_text(), "1200", "its tenant's notes");
    };
    let held = |pg: &mut postgres::Client, w: &str| {
        let e = run(&db, MAIN, &format!("merge world {w}"), &[]).unwrap_err();
        assert_eq!(e.code, "40001", "{}", e.message);
        assert!(e.message.contains("changed in Postgres since it forked"), "{}", e.message);
        assert!(!e.message.contains("more broadly"), "{}", e.message);
        assert_ne!(pg_cell(pg, "select name from leads where id = 42"), w, "nothing written");
        run(&db, MAIN, &format!("drop world {w}"), &[]).unwrap();
    };

    // the app changes and adds other tenants' notes: not held
    work("others");
    pg.batch_execute("update notes set body = 'app' where lead_id = 7; insert into notes values (200000, 7, 'new');")
        .unwrap();
    run(&db, MAIN, "merge world others", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 42"), "others");

    // every note of its own changed at once, as a bulk run does, while the app changes another tenant's: not held
    let sql = "create world bulk with (check_reads = true); use world bulk;
               update notes set body = body || ' (reviewed)'; use world main";
    let out = agent().run(&db, sql, &[]).unwrap();
    assert_eq!(out[2].tag, "UPDATE 1200");
    pg.batch_execute("update notes set body = 'app again' where lead_id = 7").unwrap();
    run(&db, MAIN, "merge world bulk", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select count(*) from notes where body like '% (reviewed)'"), "1200");

    // a note of its own changed in Postgres, or added there under its lead: held
    work("changed");
    pg.batch_execute("update notes set body = 'app' where id = 41").unwrap(); // lead 42's
    held(&mut pg, "changed");
    work("added");
    pg.batch_execute("insert into notes values (200042, 142, 'new')").unwrap();
    held(&mut pg, "added");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// An agent given an end user's Postgres role and claims (`pg_role`, `pg_settings`) has its worlds written back as
/// that role: Postgres's own row-level security judges each write with the claims, so a row of the end user's is
/// written, and one of another user's (an update the policy hides, an insert it refuses) refuses the merge, saying
/// whose rights it was written with, and nothing is written. It is the same when a person merges the world. A role
/// that reads past row-level security (a superuser) is refused, as it would judge nothing.
#[test]
fn an_agent_writes_back_as_the_end_users_role() {
    let Some((mut pg, name, _url, _turn)) = throwaway("actas") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    // roles are the server's, not the database's: made once, kept
    pg.batch_execute(
        "do $$ begin create role chronos_test_end_user nologin; exception when duplicate_object then null; end $$;
         create table leads (id int primary key, owner text not null, name text);
         insert into leads values (1, 'u-17', 'mine'), (2, 'u-99', 'theirs');
         grant chronos_test_end_user to current_user;
         grant select, insert, update, delete on leads to chronos_test_end_user;
         alter table leads enable row level security;
         create policy own on leads to chronos_test_end_user
             using (owner = current_setting('request.jwt.claims', true)::json ->> 'sub')
             with check (owner = current_setting('request.jwt.claims', true)::json ->> 'sub');",
    )
    .unwrap();
    let dir = folder("actas");
    let (ok, out) = attach(&dir, &["--tables", "leads", "--yes"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let sql = r#"create agent run17 with (can = 'read,fork,write_own,merge_own', pg_role = 'chronos_test_end_user',
                 pg_settings = '{"request.jwt.claims": {"sub": "u-17"}}')"#;
    run(&db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{}", e.message));
    let agent = || {
        let mut s = Session::new(MAIN);
        s.agent = Some("run17".into());
        s
    };
    let work = |w: &str, sql: &str| {
        agent().run(&db, &format!("create world {w}; use world {w}; {sql}; use world main"), &[]).unwrap();
    };
    let refused = |pg: &mut postgres::Client, w: &str| {
        let e = run(&db, MAIN, &format!("merge world {w}"), &[]).unwrap_err();
        assert_eq!(e.code, "42501", "{}", e.message);
        assert!(
            e.message.contains("written as Postgres role chronos_test_end_user with request.jwt.claims"),
            "{}",
            e.message
        );
        assert!(!e.message.contains("u-17"), "the claims aren't shown: {}", e.message);
        assert_eq!(
            pg_cell(pg, "select string_agg(name, ',' order by id) from leads"),
            "called,theirs",
            "nothing written"
        );
        assert_eq!(pg_cell(pg, "select count(*) from leads"), "2");
        run(&db, MAIN, &format!("drop world {w}"), &[]).unwrap();
    };

    // its end user's row: written (merged by a person, as one approves a held run: still as the end user)
    work("mine", "update leads set name = 'called' where id = 1");
    run(&db, MAIN, "merge world mine", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 1"), "called");

    // another user's row: the policy hides it from the update, which writes no row
    work("others", "update leads set name = 'taken' where id = 2");
    refused(&mut pg, "others");
    // a row for another user: the policy refuses the insert
    work("planted", "insert into leads values (3, 'u-99', 'planted')");
    refused(&mut pg, "planted");
    // and a delete of another user's row, among one of its own: nothing written
    work("both", "update leads set name = 'again' where id = 1; delete from leads where id = 2");
    refused(&mut pg, "both");

    // a world forked before the agent changes is written as whom it was forked for
    work("kept", "update leads set name = 'kept' where id = 2");
    run(&db, MAIN, "alter agent run17 set (pg_role = '')", &[]).unwrap();
    refused(&mut pg, "kept");

    // a role that reads past row-level security judges nothing: refused (where the test's role is one)
    let me = pg_cell(&mut pg, "select current_user");
    if pg_cell(&mut pg, "select rolsuper or rolbypassrls from pg_roles where rolname = current_user") == "t" {
        run(&db, MAIN, &format!("alter agent run17 set (pg_role = '{me}')"), &[]).unwrap();
        work("past", "update leads set name = 'past' where id = 2");
        let e = run(&db, MAIN, "merge world past", &[]).unwrap_err();
        assert!(e.message.contains("superuser or has BYPASSRLS"), "{}", e.message);
        assert_eq!(pg_cell(&mut pg, "select name from leads where id = 2"), "theirs");
        run(&db, MAIN, "drop world past", &[]).unwrap();
        run(&db, MAIN, "alter agent run17 set (pg_role = '')", &[]).unwrap();
    }

    // without a role, the attach role's, as before
    work("plain", "update leads set name = 'plain' where id = 2");
    run(&db, MAIN, "merge world plain", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 2"), "plain");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// Write-back rules (`writeback.json` in the folder) shape how merges reach Postgres, table by table: a delta column
/// is written as an increment and never conflicts, an ignored column isn't compared, a check holds a merge whose rows
/// break it, a procedure applies a table's changes through the app's own function, an outbox records each change in
/// the same transaction, observe mode runs the write-back and rolls it back, and a file that isn't rules refuses
/// merges. A world that changes many rows merges IN CHUNKS OF n, each chunk its own transaction.
#[test]
fn write_back_rules_shape_each_tables_writes() {
    let Some((mut pg, name, _url, _turn)) = throwaway("rules") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table inventory (id int primary key, sku text not null, stock int not null, seen text);
         insert into inventory select g, 'sku ' || g, 10, 'then' from generate_series(1, 30) g;
         create table notes (id int primary key, body text not null, source text);
         create table deals (id int primary key, stage text not null, amount int not null);
         insert into deals values (1, 'open', 100);
         create function settle_deal(main jsonb, world jsonb, live jsonb) returns jsonb language sql as
             $$ select live || jsonb_build_object('amount', world->>'amount') $$;
         create table chronos_outbox (merge_id text, world text, table_name text, op text, key jsonb, before jsonb,
                                      after jsonb, at timestamptz);
         create function apply_notes(changes jsonb) returns bigint language plpgsql as $$
         declare c jsonb; n bigint := 0;
         begin
           for c in select * from jsonb_array_elements(changes) loop
             if c->>'op' = 'insert' then
               insert into notes values ((c->'after'->>'id')::int, c->'after'->>'body', 'apply_notes');
             elsif c->>'op' = 'update' then
               update notes set body = c->'after'->>'body', source = 'apply_notes' where id = (c->'key'->>'id')::int;
             else
               delete from notes where id = (c->'key'->>'id')::int;
             end if;
             n := n + 1;
           end loop;
           return n;
         end $$;",
    )
    .unwrap();
    let dir = folder("rules");
    let (ok, out) = attach(&dir, &["--tables", "inventory,notes,deals", "--yes"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let rules = |r: &str| std::fs::write(dir.join("writeback.json"), r).unwrap();
    rules(
        r#"{"tables": {
             "inventory": {"delta": ["stock"], "ignore": ["seen"], "check": "stock >= 0", "writable": "sku <> 'gone'"},
             "notes": {"procedure": "apply_notes", "outbox": "chronos_outbox"},
             "deals": {"resolver": "settle_deal"}}}"#,
    );

    // the app sells 2 while the world sells 3: both count, no conflict
    run(&db, MAIN, "create world sell", &[]).unwrap();
    run(&db, "sell", "update inventory set stock = stock - 3, seen = 'world' where id = 1", &[]).unwrap();
    pg.batch_execute("update inventory set stock = stock - 2, seen = 'app' where id = 1").unwrap();
    run(&db, MAIN, "merge world sell", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(pg_cell(&mut pg, "select stock || ' ' || seen from inventory where id = 1"), "5 world");
    assert_eq!(cell(&db, MAIN, "select stock from inventory where id = 1"), "5", "main takes Postgres's count");

    // the same, with main already holding the app's sale (a refresh): combined inside Chronos as theirs + ours - base
    run(&db, MAIN, "create world again", &[]).unwrap();
    run(&db, "again", "update inventory set stock = stock - 1 where id = 2", &[]).unwrap();
    pg.batch_execute("update inventory set stock = stock - 4 where id = 2").unwrap();
    chronos::attach::refresh(&db).unwrap();
    run(&db, MAIN, "merge world again", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(pg_cell(&mut pg, "select stock from inventory where id = 2"), "5");

    // selling more than is left breaks the check, in Postgres: held, nothing written
    run(&db, MAIN, "create world oversell", &[]).unwrap();
    run(&db, "oversell", "update inventory set stock = stock - 4 where id = 1", &[]).unwrap();
    pg.batch_execute("update inventory set stock = stock - 3 where id = 1").unwrap();
    let e = run(&db, MAIN, "merge world oversell", &[]).unwrap_err();
    assert!(e.message.contains("tables.inventory.check"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select stock from inventory where id = 1"), "2", "nothing written");
    run(&db, MAIN, "drop world oversell", &[]).unwrap();

    // a row the rules say is settled (writable): refused before anything is written
    pg.batch_execute("update inventory set sku = 'gone' where id = 4").unwrap();
    chronos::attach::refresh(&db).unwrap(); // (main holds it too: no conflict, the rule alone refuses)
    run(&db, MAIN, "create world touch", &[]).unwrap();
    run(&db, "touch", "update inventory set stock = 9 where id = 4", &[]).unwrap();
    let e = run(&db, MAIN, "merge world touch", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("tables.inventory.writable"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select stock from inventory where id = 4"), "10");
    run(&db, MAIN, "drop world touch", &[]).unwrap();

    // both sides changed a deal's stage: the resolver keeps Postgres's stage and the world's amount, main takes it
    run(&db, MAIN, "create world deal", &[]).unwrap();
    run(&db, "deal", "update deals set stage = 'won', amount = 200 where id = 1", &[]).unwrap();
    pg.batch_execute("update deals set stage = 'lost' where id = 1").unwrap();
    run(&db, MAIN, "merge world deal", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(pg_cell(&mut pg, "select stage || ' ' || amount from deals where id = 1"), "lost 200");
    assert_eq!(cell(&db, MAIN, "select stage || ' ' || amount from deals where id = 1"), "lost 200");

    // notes go through apply_notes, and each change is in the outbox, in the same transaction
    run(&db, MAIN, "create world note", &[]).unwrap();
    run(&db, "note", "insert into notes (id, body) values (1, 'call back'), (2, 'sent quote')", &[]).unwrap();
    run(&db, MAIN, "merge world note", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(pg_cell(&mut pg, "select count(*) from notes where source = 'apply_notes'"), "2");
    assert_eq!(cell(&db, MAIN, "select source from notes where id = 1"), "apply_notes", "main takes the table");
    assert_eq!(
        pg_cell(&mut pg, "select string_agg(op || ' ' || (key->>'id'), ',' order by key->>'id') from chronos_outbox"),
        "insert 1,insert 2"
    );

    // a scope: a row the merge would leave outside it is refused, nothing written
    rules(r#"{"tables": {"deals": {"scope": "id < 100"}}}"#);
    run(&db, MAIN, "create world far", &[]).unwrap();
    run(&db, "far", "insert into deals values (500, 'open', 1)", &[]).unwrap();
    let e = run(&db, MAIN, "merge world far", &[]).unwrap_err();
    assert!(e.message.contains("tables.deals.check or .scope (id < 100)"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select count(*) from deals where id = 500"), "0");
    run(&db, MAIN, "drop world far", &[]).unwrap();

    // observe mode: run whole, rolled back, said so
    rules(r#"{"mode": "observe", "tables": {"inventory": {"delta": ["stock"]}}}"#);
    run(&db, MAIN, "create world watch", &[]).unwrap();
    run(&db, "watch", "update inventory set stock = 0 where id = 3", &[]).unwrap();
    let e = run(&db, MAIN, "merge world watch", &[]).unwrap_err();
    assert!(e.message.contains("observe mode") && e.message.contains("inventory: 1 updated"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select stock from inventory where id = 3"), "10");

    // a file that isn't rules, or a rule naming a column the table hasn't: refused, nothing written
    rules(r#"{"tables": {"inventory": {"delta": "stock"}}}"#);
    let e = run(&db, MAIN, "merge world watch", &[]).unwrap_err();
    assert!(e.message.contains("write-back rules"), "{}", e.message);
    rules(r#"{"tables": {"inventory": {"delta": ["stok"]}}}"#);
    let e = run(&db, MAIN, "merge world watch", &[]).unwrap_err();
    assert!(e.message.contains("names column stok"), "{}", e.message);
    run(&db, MAIN, "drop world watch", &[]).unwrap();
    std::fs::remove_file(dir.join("writeback.json")).unwrap();

    // 25 rows in chunks of 10: three merges, every row written
    run(&db, MAIN, "create world restock", &[]).unwrap();
    run(&db, "restock", "update inventory set stock = 100 where id > 5", &[]).unwrap();
    let out = run(&db, MAIN, "merge world restock in chunks of 10", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(out[0].tag, "MERGE 25");
    assert_eq!(pg_cell(&mut pg, "select count(*) from inventory where stock = 100"), "25");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// `chronos attach --lazy-tenant org_id`: tables with the column (and their children through a foreign key) start
/// empty, and main takes a tenant's rows the first time a world is forked for it (a person's WITH (tenant), an agent
/// held to the tenant, or chronos refresh --tenant); refreshes then read only the tenants loaded. Tables without the
/// column are copied whole; one two foreign keys down is refused, as is a column no table has.
#[test]
fn a_lazy_tenant_attach_loads_a_tenant_at_its_first_fork() {
    let Some((mut pg, name, _url, _turn)) = throwaway("lazy") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    // 50 tenants of 6 leads each, a note per lead, and plans shared by all
    pg.batch_execute(
        "create table plans (id int primary key, name text not null);
         insert into plans values (1, 'free'), (2, 'team');
         create table leads (id int primary key, org_id int not null, name text);
         create index on leads (org_id);
         insert into leads select g, g % 50, 'lead ' || g from generate_series(1, 300) g;
         create table notes (id int primary key, lead_id int not null references leads (id), body text);
         insert into notes select g, g, 'note ' || g from generate_series(1, 300) g;
         create table tags (id int primary key, note_id int not null references notes (id));",
    )
    .unwrap();
    let dir = folder("lazy");
    let (ok, out) = attach(&dir, &["--tables", "plans,leads,notes", "--lazy-tenant", "org_id"]);
    assert!(ok, "{out}");
    assert!(out.contains("a tenant at a time"), "{out}");
    let db = Db::open(&dir).unwrap();
    assert_eq!(cell(&db, MAIN, "select count(*) from plans"), "2", "copied whole");
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "0");
    assert_eq!(cell(&db, MAIN, "select count(*) from notes"), "0");

    // a person's world for tenant 42: main takes its leads and their notes first, and the world starts with them
    run(&db, MAIN, "create world w with (tenant = 'org_id = 42')", &[]).unwrap();
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "6");
    assert_eq!(cell(&db, MAIN, "select count(*) from notes"), "6");
    assert_eq!(cell(&db, "w", "select count(*) from leads where org_id = 42"), "6");
    run(&db, "w", "update leads set name = 'called' where id = 42; update notes set body = 'n' where id = 92", &[])
        .unwrap();
    run(&db, MAIN, "merge world w", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 42"), "called");
    assert_eq!(pg_cell(&mut pg, "select body from notes where id = 92"), "n");

    // a refresh reads only the tenants loaded: tenant 42's new lead comes over, tenant 7's doesn't
    pg.batch_execute(
        "insert into leads values (1042, 42, 'new'), (1007, 7, 'new');
         insert into notes values (1042, 1042, 'new'), (1007, 1007, 'new');
         update leads set name = 'app' where id = 7;",
    )
    .unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "7", "{r:?}");
    assert_eq!(cell(&db, MAIN, "select count(*) from notes"), "7", "{r:?}");
    assert_eq!(cell(&db, MAIN, "select count(*) from leads where org_id = 7"), "0");

    // an agent held to tenant 7: its first fork loads it, as Postgres has it now
    run(&db, MAIN, "create agent bot with (can = 'read,fork,write_own,merge_own', tenant = 'org_id = 7')", &[])
        .unwrap();
    let mut s = chronos::sql::Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(&db, "create world mine", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(cell(&db, MAIN, "select count(*) from leads where org_id = 7"), "7");
    assert_eq!(cell(&db, MAIN, "select name from leads where id = 7"), "app");
    assert_eq!(cell(&db, MAIN, "select count(*) from notes"), "14", "its tenant's notes, through the foreign key");
    assert_eq!(
        cell(&db, MAIN, "select count(*) from notes n join leads l on l.id = n.lead_id where l.org_id = 7"),
        "7",
        "tenant 7's notes came with its leads"
    );

    // loaded once: a second load takes nothing; and the folder remembers which tenants it holds
    assert!(chronos::attach::load_tenant(&db, "9").unwrap() > 0);
    assert_eq!(chronos::attach::load_tenant(&db, "9").unwrap(), 0);
    drop(s);
    drop(db);
    let db = Db::open(&dir).unwrap();
    let loaded = db.attached().unwrap().lazy().unwrap().loaded();
    assert_eq!(loaded, vec!["42".to_string(), "7".into(), "9".into()]);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();

    // two foreign keys down, and a column no table has: refused, nothing left behind
    let (ok, out) = attach(&dir, &["--tables", "leads,notes,tags", "--lazy-tenant", "org_id"]);
    assert!(!ok && out.contains("two levels down"), "{out}");
    assert!(!dir.exists());
    let (ok, out) = attach(&dir, &["--tables", "plans", "--lazy-tenant", "org_id"]);
    assert!(!ok && out.contains("no table attached has a column"), "{out}");
    assert!(!dir.exists());

    drop(pg);
    drop_db(&name);
}

#[test]
fn refresh_brings_main_up_to_postgres() {
    let Some((mut pg, name, _url, _turn)) = throwaway("refresh") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, org_id int not null, name text, score numeric(10, 2),
                             seen timestamptz);
         insert into leads select g, g % 3, 'lead ' || g, g * 1.5, '2026-10-01 10:00:00+00'
             from generate_series(1, 4500) g;",
    )
    .unwrap();
    let dir = folder("refresh");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // nothing changed: nothing to take (a row nobody changed compares equal, numeric and timestamptz included)
    let r = chronos::attach::refresh(&db).unwrap();
    assert_eq!(r.changed(), 0, "{:?}", r);
    assert!(db.attached().unwrap().refreshed_at() > 0);

    // Postgres changes straight: an update, an insert and a delete
    pg.batch_execute(
        "update leads set name = 'renamed' where id = 7;
         insert into leads values (9001, 1, 'new', 1, now());
         delete from leads where id = 4000;",
    )
    .unwrap();
    // a world that changed one of those rows too
    run(&db, MAIN, "create world w; use world w; update leads set name = 'mine' where id = 7", &[]).unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert_eq!(r.tables, vec![("leads".to_string(), 1, 1, 1)]);
    assert_eq!(cell(&db, MAIN, "select name from leads where id = 7"), "renamed");
    assert_eq!(cell(&db, MAIN, "select count(*) from leads where id = 9001"), "1");
    assert_eq!(cell(&db, MAIN, "select count(*) from leads where id = 4000"), "0");
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "4500");
    // the world's change of the same row is now a conflict, settled as any
    let e = run(&db, MAIN, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    run(&db, MAIN, "merge world w using ours", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 7"), "mine");

    // a table whose key changed in Postgres is left as it was, and said
    pg.batch_execute("alter table leads drop constraint leads_pkey, add primary key (id, org_id)").unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert_eq!(r.skipped.len(), 1, "{:?}", r);
    assert!(r.skipped[0].contains("changed in Postgres since the attach"), "{:?}", r.skipped);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn columns_added_in_postgres_come_over_at_refresh() {
    let Some((mut pg, name, _url, _turn)) = throwaway("widen") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, name text);
         insert into leads select g, 'lead ' || g from generate_series(1, 20) g;",
    )
    .unwrap();
    let dir = folder("widen");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // a world forked before Postgres has the columns
    run(
        &db,
        MAIN,
        "create world old; use world old; update leads set name = 'old 5' where id = 5;
                    update leads set name = 'old 6' where id = 6; insert into leads (id, name) values (200, 'old new')",
        &[],
    )
    .unwrap();
    // the app's migration, and its first writes to the new columns
    pg.batch_execute(
        "alter table leads add column note text, add column status text not null default 'new';
         update leads set status = 'won', note = 'called' where id = 5;",
    )
    .unwrap();
    // until a refresh takes the columns, a merge touching the table is refused, and nothing is written
    let e = run(&db, MAIN, "merge world old", &[]).unwrap_err();
    assert!(e.message.contains("a refresh takes them"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 6"), "lead 6");

    let r = chronos::attach::refresh(&db).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r);
    assert_eq!(r.added, vec!["leads.note text".to_string(), "leads.status text".to_string()]);
    // main took the columns (with Postgres's default), then the values Postgres has
    assert_eq!(r.tables, vec![("leads".to_string(), 0, 1, 0)]);
    assert_eq!(cell(&db, MAIN, "select status || ' ' || note from leads where id = 5"), "won called");
    assert_eq!(cell(&db, MAIN, "select status || ' ' || coalesce(note, '-') from leads where id = 6"), "new -");
    // the record keeps them: reopened, the folder still writes them back
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0);

    // a world forked now sees the columns, and its writes of them reach Postgres
    run(
        &db,
        MAIN,
        "create world fresh; use world fresh; update leads set note = 'emailed' where id = 7;
                    insert into leads (id, name) values (100, 'new lead')",
        &[],
    )
    .unwrap();
    run(&db, MAIN, "merge world fresh", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select note from leads where id = 7"), "emailed");
    assert_eq!(pg_cell(&mut pg, "select status from leads where id = 100"), "new");

    // the world forked before never saw the columns: its writes leave them as Postgres has them
    // (row 5: main took the columns' values from Postgres, the world changed its name: a conflict, settled here by
    // keeping the world's row, which even so keeps what Postgres holds in the columns the world never had)
    let e = run(&db, MAIN, "merge world old", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    run(&db, MAIN, "merge world old using ours", &[]).unwrap();
    assert_eq!(
        pg_cell(&mut pg, "select name || ' ' || status || ' ' || note from leads where id = 5"),
        "old 5 won called"
    );
    assert_eq!(
        pg_cell(&mut pg, "select name || ' ' || status || ' ' || coalesce(note, '-') from leads where id = 6"),
        "old 6 new -"
    );
    assert_eq!(pg_cell(&mut pg, "select status from leads where id = 200"), "new");
    // and main holds what Postgres does
    assert_eq!(
        cell(&db, MAIN, "select name || ' ' || status || ' ' || note from leads where id = 5"),
        "old 5 won called"
    );
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn columns_dropped_renamed_retyped_in_postgres_come_over_at_refresh() {
    let Some((mut pg, name, _url, _turn)) = throwaway("reshape") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, name text, score int, seen text, note text);
         insert into leads select g, 'lead ' || g, g, 'x', 'n' || g from generate_series(1, 20) g;",
    )
    .unwrap();
    let dir = folder("reshape");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // a world forked before Postgres changes the table
    run(&db, MAIN, "create world old; use world old; update leads set name = 'old 6' where id = 6", &[]).unwrap();
    // the app's migration, and its first writes in the new shape
    pg.batch_execute(
        "alter table leads drop column seen, alter column score type bigint, add column tag text;
         alter table leads rename column note to memo;
         update leads set memo = 'm5', score = 5000000000 where id = 5;",
    )
    .unwrap();
    // until a refresh takes the change, a merge touching the table is refused, and nothing is written
    let e = run(&db, MAIN, "merge world old", &[]).unwrap_err();
    assert!(
        e.message.contains("a refresh takes it") && e.message.contains("column note renamed memo"),
        "{}",
        e.message
    );
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 6"), "lead 6");

    let r = chronos::attach::refresh(&db).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r);
    assert_eq!(
        r.altered,
        vec![
            "leads: column seen dropped".to_string(),
            "leads: column note renamed memo".to_string(),
            "leads: column score now bigint".to_string()
        ]
    );
    assert_eq!(r.added, vec!["leads.tag text".to_string()]);
    assert_eq!(r.tables, vec![("leads".to_string(), 0, 1, 0)]);
    assert_eq!(cell(&db, MAIN, "select memo || ' ' || score from leads where id = 5"), "m5 5000000000");
    assert_eq!(cell(&db, MAIN, "select memo from leads where id = 7"), "n7");
    assert!(run(&db, MAIN, "select seen from leads", &[]).is_err());
    // the record keeps the new shape: reopened, nothing more to take
    drop(db);
    let db = Db::open(&dir).unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert!(r.changed() == 0 && r.altered.is_empty() && r.skipped.is_empty(), "{:?}", r);

    // the world forked before made its rows in the old shape: refused, and nothing written
    let e = run(&db, MAIN, "merge world old", &[]).unwrap_err();
    assert!(e.message.contains("fork a new world from main"), "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 6"), "lead 6");
    run(&db, MAIN, "drop world old", &[]).unwrap();

    // a world forked now writes the new shape back
    run(
        &db,
        MAIN,
        "create world fresh; use world fresh; update leads set memo = 'called', score = 6000000000 where id = 7;
                    insert into leads (id, name, memo, tag) values (100, 'new lead', 'x', 'hot')",
        &[],
    )
    .unwrap();
    run(&db, MAIN, "merge world fresh", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select memo || ' ' || score from leads where id = 7"), "called 6000000000");
    assert_eq!(pg_cell(&mut pg, "select memo || ' ' || tag from leads where id = 100"), "x hot");
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn columns_masked_on_main_read_masked_to_agents() {
    let Some((mut pg, name, _url, _turn)) = throwaway("masks") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table customers (id int primary key, email text, tier text);
         insert into customers values (1, 'ann@example.com', 'free'), (2, 'bob@example.com', 'free');",
    )
    .unwrap();
    let dir = folder("masks");
    let (ok, out) = attach(&dir, &["--tables", "customers"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // masked on main: it changes only what agents read here, never Postgres
    run(&db, MAIN, "alter table customers alter column email set masked", &[]).unwrap();
    let t = db.attached().unwrap().tables()[0].clone();
    assert_eq!(chronos::attach::masks(&db, &t), vec![("email".to_string(), "hash")]);
    // not in a world (its merge would change the table's definition), and no other change of shape on main
    run(&db, MAIN, "create world m", &[]).unwrap();
    let e = run(&db, "m", "alter table customers alter column tier set masked", &[]).unwrap_err();
    assert!(e.message.contains("is done on main"), "{}", e.message);
    run(&db, MAIN, "drop world m", &[]).unwrap();
    let e = run(&db, MAIN, "alter table customers add column x int", &[]).unwrap_err();
    assert_eq!(e.code, "0A000", "{}", e.message);

    // an agent reads a mask; its change of another column reaches Postgres, the masked value untouched
    run(&db, MAIN, "create agent bot with (can = 'read,fork,write_own,merge_own')", &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    let read = |bot: &mut Session| {
        let out = bot.run(&db, "select email from customers where id = 1", &[]).unwrap();
        out.last().unwrap().rows[0][0].to_text()
    };
    let masked = read(&mut bot);
    assert!(masked.starts_with("m_"), "{masked}");
    bot.run(&db, "create world b; use world b; update customers set tier = 'gold' where id = 1; use world main", &[])
        .unwrap();
    run(&db, MAIN, "merge world b", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select email || ' ' || tier from customers where id = 1"), "ann@example.com gold");

    // Postgres changes the masked column: a refresh takes it, and the mask stays
    pg.batch_execute("update customers set email = 'ann@new.example' where id = 1").unwrap();
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 1);
    let now = read(&mut bot);
    assert!(now.starts_with("m_") && now != masked, "{now}");
    assert_eq!(cell(&db, MAIN, "select email from customers where id = 1"), "ann@new.example");
    // and SET MASKED is undone on main the same way
    run(&db, MAIN, "alter table customers alter column email drop masked", &[]).unwrap();
    assert!(chronos::attach::masks(&db, &t).is_empty());
    assert_eq!(read(&mut bot), "ann@new.example");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn enum_labels_added_in_postgres_come_over_at_refresh() {
    let Some((mut pg, name, _url, _turn)) = throwaway("labels") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create type mood as enum ('sad', 'happy');
         create table leads (id int primary key, m mood);
         insert into leads values (1, 'sad'), (2, 'happy');",
    )
    .unwrap();
    let dir = folder("labels");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // the app's migration: a label between two, one at the end, and a row holding it
    pg.batch_execute("alter type mood add value 'ok' before 'happy'").unwrap();
    pg.batch_execute("alter type mood add value 'meh'").unwrap();
    pg.batch_execute("update leads set m = 'meh' where id = 1").unwrap();
    // until a refresh, main's type hasn't the labels: a world can't write them
    run(&db, MAIN, "create world early", &[]).unwrap();
    let e = run(&db, "early", "update leads set m = 'ok' where id = 2", &[]).unwrap_err();
    assert!(e.message.contains("invalid input value for enum"), "{}", e.message);
    run(&db, MAIN, "drop world early", &[]).unwrap();

    let r = chronos::attach::refresh(&db).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r);
    assert_eq!(r.labels, vec!["mood 'ok'".to_string(), "mood 'meh'".to_string()]);
    // in Postgres's order, and the row holding one came in after them
    assert_eq!(cell(&db, MAIN, "select 'sad'::mood < 'ok'::mood and 'ok'::mood < 'happy'::mood"), "true");
    assert_eq!(cell(&db, MAIN, "select 'happy'::mood < 'meh'::mood"), "true");
    assert_eq!(cell(&db, MAIN, "select m::text from leads where id = 1"), "meh");
    // taken once: kept when reopened, nothing to take again
    drop(db);
    let db = Db::open(&dir).unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert!(r.labels.is_empty() && r.skipped.is_empty(), "{:?}", r);

    // a world forked now writes them, and the merge reaches Postgres
    if let Some(e) = change(&db, "fresh", "update leads set m = 'ok' where id = 2; insert into leads values (3, 'meh')")
    {
        panic!("{}", e.message);
    }
    assert_eq!(pg_cell(&mut pg, "select string_agg(m::text, ',' order by id) from leads"), "meh,ok,meh");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn widening_keeps_not_null_and_survives_a_stop_before_its_record_is_saved() {
    let Some((mut pg, name, _url, _turn)) = throwaway("widen_nn") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, name text);
         insert into leads select g, 'lead ' || g from generate_series(1, 10) g;",
    )
    .unwrap();
    let dir = folder("widen_nn");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    // a column added NULL, filled, then made NOT NULL: Postgres keeps no default for it
    pg.batch_execute(
        "alter table leads add column owner text; update leads set owner = 'bo';
         alter table leads alter column owner set not null;",
    )
    .unwrap();
    let record = std::fs::read(dir.join("attached")).unwrap();
    let db = Db::open(&dir).unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r);
    assert_eq!(r.added, vec!["leads.owner text".to_string()]);
    assert_eq!(cell(&db, MAIN, "select count(*) from leads where owner = 'bo'"), "10");
    // NOT NULL here too, once main had the values: a world can't store a NULL Postgres would refuse at merge
    run(&db, MAIN, "create world w", &[]).unwrap();
    let e = run(&db, "w", "update leads set owner = null where id = 3", &[]).unwrap_err();
    assert_eq!(e.code, "23502", "{}", e.message);

    // stopped after main took the column, before the folder's record was saved: opened again with the old record,
    // the next refresh finds the column again, adds nothing main has, and saves
    drop(db);
    std::fs::write(dir.join("attached"), &record).unwrap();
    let db = Db::open(&dir).unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r);
    assert_eq!(r.added, vec!["leads.owner text".to_string()]);
    assert_eq!(r.changed(), 0);
    run(&db, MAIN, "create world w2; use world w2; update leads set owner = 'ada' where id = 4", &[]).unwrap();
    run(&db, MAIN, "merge world w2", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select owner from leads where id = 4"), "ada");
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert!(chronos::attach::refresh(&db).unwrap().added.is_empty());

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn a_column_added_with_a_default_worked_out_per_row_comes_over() {
    let Some((mut pg, name, _url, _turn)) = throwaway("widen_volatile") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, name text);
         insert into leads select g, 'lead ' || g from generate_series(1, 10) g;",
    )
    .unwrap();
    let dir = folder("widen_volatile");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // Postgres gives every row its own value; Chronos won't fill rows it has with such a default
    // (gen_random_uuid is pgcrypto's before Postgres 13)
    pg.batch_execute("create extension if not exists pgcrypto").unwrap();
    pg.batch_execute("alter table leads add column ref uuid not null default gen_random_uuid()").unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r);
    assert_eq!(r.added, vec!["leads.ref uuid".to_string()]);
    // the column came bare, took Postgres's values, then its default and NOT NULL
    assert_eq!(r.tables, vec![("leads".to_string(), 0, 10, 0)]);
    assert_eq!(
        cell(&db, MAIN, "select ref::text from leads where id = 3"),
        pg_cell(&mut pg, "select ref::text from leads where id = 3")
    );
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0);
    run(&db, MAIN, "create world w", &[]).unwrap();
    let e = run(&db, "w", "update leads set ref = null where id = 3", &[]).unwrap_err();
    assert_eq!(e.code, "23502", "{}", e.message);
    // a row a world inserts gets its own value from the default, written as it is to Postgres
    run(&db, "w", "insert into leads (id, name) values (50, 'new')", &[]).unwrap();
    let mine = cell(&db, "w", "select ref::text from leads where id = 50");
    assert_eq!(mine.len(), 36, "{mine}");
    run(&db, MAIN, "merge world w", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select ref::text from leads where id = 50"), mine);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn reads_of_a_world_forked_before_columns_were_added_are_checked_as_it_saw_them() {
    let Some((mut pg, name, _url, _turn)) = throwaway("widen_reads") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, name text, status text);
         insert into leads select g, 'lead ' || g, 'new' from generate_series(1, 10) g;",
    )
    .unwrap();
    let dir = folder("widen_reads");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // worlds that check their reads, forked before Postgres has the column: one reads a row by key, one a range
    for w in ["by_key", "by_range"] {
        run(&db, MAIN, &format!("create world {w} with (check_reads = true)"), &[]).unwrap();
    }
    assert_eq!(cell(&db, "by_key", "select name from leads where id = 2"), "lead 2");
    run(&db, "by_key", "update leads set status = 'ready' where id = 7", &[]).unwrap();
    assert_eq!(cell(&db, "by_range", "select count(*) from leads where id < 5"), "4");
    run(&db, "by_range", "update leads set status = 'ready' where id = 6", &[]).unwrap();
    // the migration, a write to the new column of a row they read, and the refresh that takes it
    pg.batch_execute("alter table leads add column owner text; update leads set owner = 'ada' where id = 2;").unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert_eq!(r.added, vec!["leads.owner text".to_string()], "{:?}", r);
    // what they read is as they saw it: the column they never had doesn't count, and they merge
    run(&db, MAIN, "merge world by_key", &[]).unwrap();
    run(&db, MAIN, "merge world by_range", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select status || ' ' || owner from leads where id = 2"), "new ada");
    assert_eq!(pg_cell(&mut pg, "select count(*) from leads where status = 'ready'"), "2");
    // and a column they did see, changed in Postgres, still holds such a world
    run(&db, MAIN, "create world later with (check_reads = true)", &[]).unwrap();
    pg.batch_execute("alter table leads add column score int").unwrap();
    assert_eq!(cell(&db, "later", "select name from leads where id = 4"), "lead 4");
    run(&db, "later", "update leads set status = 'ready' where id = 5", &[]).unwrap();
    pg.batch_execute("update leads set name = 'renamed', score = 1 where id = 4").unwrap();
    chronos::attach::refresh(&db).unwrap();
    let e = run(&db, MAIN, "merge world later", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("since it forked"), "{}", e.message);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn rows_triggers_write_to_other_tables_come_into_main_with_the_merge() {
    let Some((mut pg, name, _url, _turn)) = throwaway("triggers") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table accounts (id int primary key, name text, lead_count int not null default 0);
         create table leads (id int primary key, account_id int references accounts, name text);
         create table notes (id int primary key, lead_id int, body text);
         create table audit (id int primary key, what text);
         create sequence audit_ids;
         create function leads_changed() returns trigger language plpgsql as $$
         begin
           if tg_op = 'INSERT' then
             update accounts set lead_count = lead_count + 1 where id = new.account_id;
             insert into audit values (nextval('audit_ids'), 'added lead ' || new.id);
             return new;
           else
             update accounts set lead_count = lead_count - 1 where id = old.account_id;
             delete from notes where lead_id = old.id;
             return old;
           end if;
         end $$;
         create trigger leads_changed after insert or delete on leads for each row execute function leads_changed();
         insert into accounts values (1, 'acme', 0), (2, 'globex', 0);
         insert into leads values (1, 1, 'ada'), (2, 1, 'bo');
         update accounts set lead_count = 2 where id = 1;
         insert into notes values (1, 2, 'call back'), (2, 1, 'hot');",
    )
    .unwrap();
    let dir = folder("triggers");
    let (ok, out) = attach(&dir, &["--tables", "accounts,leads,notes,audit"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();

    // an insert: the trigger counts it on its account and writes an audit row; main has both at once
    assert!(change(&db, "w1", "insert into leads values (3, 1, 'cy')").is_none());
    assert_eq!(pg_cell(&mut pg, "select lead_count from accounts where id = 1"), "3");
    assert_eq!(cell(&db, MAIN, "select lead_count from accounts where id = 1"), "3");
    assert_eq!(
        cell(&db, MAIN, "select string_agg(what, ', ' order by id) from audit"),
        "added lead 1, added lead 2, added lead 3"
    );
    // a delete: the trigger takes the lead's notes with it; main loses them too
    assert!(change(&db, "w2", "delete from leads where id = 2").is_none());
    assert_eq!(pg_cell(&mut pg, "select count(*) from notes"), "1");
    assert_eq!(cell(&db, MAIN, "select count(*) from notes"), "1");
    assert_eq!(cell(&db, MAIN, "select lead_count from accounts where id = 1"), "2");
    // main is as Postgres is: a world changing the account the triggers wrote merges with no conflict
    assert!(change(&db, "w3", "update accounts set name = 'acme inc' where id = 1").is_none());
    assert_eq!(pg_cell(&mut pg, "select name || ' ' || lead_count from accounts where id = 1"), "acme inc 2");
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// Postgres's counts of rows written hold a connection's earlier commits for up to a second: merges in quick
/// succession aren't taken for triggers writing besides, so main doesn't refresh the table after each.
#[test]
fn merges_in_quick_succession_are_not_taken_for_trigger_writes() {
    let Some((mut pg, name, _url, _turn)) = throwaway("quick") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, owner text);
         insert into leads values (1, 'bo'), (2, 'bo');",
    )
    .unwrap();
    let dir = folder("quick");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();

    // a row changed in Postgres alone, that nothing here says to take
    pg.batch_execute("update leads set owner = 'zed' where id = 2").unwrap();
    for i in 0..6 {
        assert!(change(&db, &format!("w{i}"), &format!("update leads set owner = 'o{i}' where id = 1")).is_none());
        assert_eq!(cell(&db, MAIN, "select owner from leads where id = 2"), "bo", "after merge {i}");
    }
    assert_eq!(pg_cell(&mut pg, "select owner from leads where id = 1"), "o5");
    // a refresh still takes it
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 1);
    assert_eq!(cell(&db, MAIN, "select owner from leads where id = 2"), "zed");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn deferred_trigger_writes_and_a_crash_after_commit_still_reach_main() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: crash points are in debug builds only");
        return;
    }
    let Some((mut pg, name, _url, _turn)) = throwaway("triggers_late") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table accounts (id int primary key, lead_count int not null default 0);
         create table leads (id int primary key, account_id int, name text);
         create function count_lead() returns trigger language plpgsql as $$
         begin
           update accounts set lead_count = lead_count + 1 where id = new.account_id;
           return new;
         end $$;
         create constraint trigger count_lead after insert on leads deferrable initially deferred
           for each row execute function count_lead();
         create table audit (id int primary key, what text);
         create sequence audit_ids;
         create function audit_lead() returns trigger language plpgsql as $$
         begin
           begin
             insert into audit values (nextval('audit_ids'), 'added lead ' || new.id);
           exception when others then null;
           end;
           return new;
         end $$;
         create trigger audit_lead after insert on leads for each row execute function audit_lead();
         insert into accounts values (1, 0);",
    )
    .unwrap();
    let dir = folder("triggers_late");
    let (ok, out) = attach(&dir, &["--tables", "accounts,leads,audit"]);
    assert!(ok, "{out}");
    // a trigger deferred to COMMIT, and one writing in a block with EXCEPTION (a subtransaction of its own): both
    // reach main with the merge
    let mut db = Db::open(&dir).unwrap();
    assert!(change(&db, "w1", "insert into leads values (1, 1, 'ada')").is_none());
    assert_eq!(pg_cell(&mut pg, "select lead_count from accounts where id = 1"), "1");
    assert_eq!(cell(&db, MAIN, "select lead_count from accounts where id = 1"), "1");
    assert_eq!(cell(&db, MAIN, "select string_agg(what, ', ' order by id) from audit"), "added lead 1");
    assert!(!dir.join("attached.pending").exists());
    // a crash right after Postgres committed, or after main made the merge but before it took what the triggers
    // wrote: the pending record stays, and recovery takes it
    for (n, at) in [(2, "after_commit"), (3, "before_trigger_refresh")] {
        run(&db, MAIN, &format!("create world c{n}"), &[]).unwrap();
        run(&db, &format!("c{n}"), &format!("insert into leads values ({n}, 1, 'lead {n}')"), &[]).unwrap();
        drop(db);
        let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
            .args(["-b", &format!("c{n}"), dir.to_str().unwrap(), "merge"])
            .env("CHRONOS_ATTACH_CRASH", at)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{at}: it crashed");
        assert!(dir.join("attached.pending").exists(), "{at}");
        assert_eq!(pg_cell(&mut pg, "select lead_count from accounts where id = 1"), n.to_string());
        let db2 = Db::open(&dir).unwrap();
        let said = chronos::attach::resolve(&db2).unwrap().unwrap_or_default();
        assert!(
            said.contains("its triggers wrote in") && said.contains("accounts") && said.contains("audit"),
            "{at}: {said}"
        );
        assert!(!dir.join("attached.pending").exists(), "{at}");
        assert_eq!(cell(&db2, MAIN, "select lead_count from accounts where id = 1"), n.to_string(), "{at}");
        assert_eq!(cell(&db2, MAIN, "select count(*) from audit"), n.to_string(), "{at}");
        assert_eq!(cell(&db2, MAIN, "select count(*) from leads"), n.to_string(), "{at}");
        drop(db2);
        db = Db::open(&dir).unwrap();
    }

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn a_row_a_world_forked_before_a_column_inserts_gets_postgress_default() {
    let Some((mut pg, name, _url, _turn)) = throwaway("widen_insert") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, name text);
         insert into leads select g, 'lead ' || g from generate_series(1, 5) g;",
    )
    .unwrap();
    let dir = folder("widen_insert");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // forked before the columns exist, and inserting after they do
    run(&db, MAIN, "create world old", &[]).unwrap();
    pg.batch_execute(
        "alter table leads add column r float8 default random(), add column seen timestamptz not null default now()",
    )
    .unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert!(r.skipped.is_empty(), "{:?}", r);
    std::thread::sleep(std::time::Duration::from_millis(50));
    run(&db, "old", "insert into leads (id, name) values (50, 'new')", &[]).unwrap();
    run(&db, MAIN, "merge world old", &[]).unwrap();
    // Postgres filled them as it fills its own rows (a value of its own, the insert's time), and main holds the same
    assert_eq!(
        pg_cell(
            &mut pg,
            "select r is not null and seen > (select max(seen) from leads where id < 50) from leads where id = 50"
        ),
        "t"
    );
    // all of the float's digits, as main prints it, whatever the server's own default (Supabase's is 0)
    pg.batch_execute("set extra_float_digits = 1").unwrap();
    let pg_row = pg_cell(&mut pg, "select r::text || ' ' || seen::text from leads where id = 50");
    let ours = cell(&db, MAIN, "select r::text from leads where id = 50");
    assert!(pg_row.starts_with(&ours), "main {ours}, Postgres {pg_row}");
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0, "main is as Postgres is");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn undo_of_a_merge_into_main_goes_back_through_postgres() {
    let Some((mut pg, name, _url, _turn)) = throwaway("undo") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, owner text);
         insert into leads values (1, 'bo'), (2, 'bo'), (3, 'eve');",
    )
    .unwrap();
    let dir = folder("undo");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    assert!(
        change(&db, "w", "update leads set owner = 'ada' where owner = 'bo'; delete from leads where id = 3").is_none()
    );
    assert_eq!(pg_cell(&mut pg, "select string_agg(owner, ',' order by id) from leads"), "ada,ada");
    run(&db, MAIN, "undo merge of world w", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select string_agg(owner, ',' order by id) from leads"), "bo,bo,eve");
    assert_eq!(cell(&db, MAIN, "select string_agg(owner, ',' order by id) from leads"), "bo,bo,eve");

    // a row changed in Postgres since the merge: the undo is a conflict, and nothing is written
    assert!(change(&db, "w2", "update leads set owner = 'zed' where id = 1").is_none());
    pg.batch_execute("update leads set owner = 'pat' where id = 1").unwrap();
    let e = run(&db, MAIN, "undo merge of world w2", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select owner from leads where id = 1"), "pat");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// Forks `w`, runs `sql` in it, merges it: the merge's id in the database.
fn merge2(db: &Db, w: &str, sql: &str) -> String {
    run(db, MAIN, &format!("create world {w}"), &[]).unwrap();
    run(db, w, sql, &[]).unwrap();
    let _ = chronos::attach::take_merge_id();
    run(db, MAIN, &format!("merge world {w}"), &[]).unwrap();
    chronos::attach::take_merge_id().expect("the merge's id")
}

/// A merge's rows are kept in Postgres with the merge (`chronos.merge_rows`), and undo it by its id: long after the
/// folder's history has gone, several merges at once, a row changed since left (or the whole undo refused).
#[test]
fn a_merge_is_undone_by_its_id() {
    let Some((mut pg, name, _url, _turn)) = throwaway("undoid") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id serial primary key, owner text, score numeric(6,2), meta jsonb, seen timestamptz);
         insert into leads (owner, score, meta, seen) values ('bo', 1.50, '{\"a\": 1}', '2026-10-01 10:00:00+00'),
                                                             ('bo', 2.00, null, null), ('eve', 3.25, '[1, 2]', null);
         create table notes (id serial primary key, lead_id int references leads (id), body text);
         insert into notes (lead_id, body) values (3, 'call eve');",
    )
    .unwrap();
    let dir = folder("undoid");
    let (ok, out) = attach(&dir, &["--tables", "leads,notes"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let merge = |w: &str, sql: &str| -> String {
        run(&db, MAIN, &format!("create world {w}"), &[]).unwrap();
        run(&db, w, sql, &[]).unwrap();
        let _ = chronos::attach::take_merge_id();
        run(&db, MAIN, &format!("merge world {w}"), &[]).unwrap();
        chronos::attach::take_merge_id().expect("the merge's id")
    };
    let state = |pg: &mut postgres::Client| {
        pg_cell(
            pg,
            "select (select string_agg(id || ':' || owner || ':' || score, ',' order by id) from leads) || ' / ' ||
                    coalesce((select string_agg(lead_id || ':' || body, ',' order by id) from notes), '')",
        )
    };
    let was = state(&mut pg);

    // one merge of each kind of change, in two tables
    let first = merge(
        "w",
        "update leads set owner = 'ada', score = 9.75 where owner = 'bo';
         delete from notes where lead_id = 3; delete from leads where id = 3;
         insert into leads (owner, score) values ('new', 4)",
    );
    assert!(first.starts_with("merge:"), "{first}");
    assert_ne!(state(&mut pg), was);
    assert_eq!(pg_cell(&mut pg, &format!("select count(*) from chronos.merge_rows where merge_id = '{first}'")), "5");
    // a dry run says what would happen and writes nothing
    let now = state(&mut pg);
    let plan = db
        .undo_merges(
            std::slice::from_ref(&first),
            chronos::attach::UndoOptions { skip_changed: false, dry_run: true, ..Default::default() },
        )
        .unwrap();
    assert!(plan.dry_run && plan.merge.is_none(), "{plan:?}");
    assert_eq!((plan.undone, plan.skipped, plan.rows.len()), (5, 0, 5), "{plan:?}");
    assert_eq!(state(&mut pg), now);
    // the undo: Postgres and main as they were, ids and values too
    let done = db
        .undo_merges(
            std::slice::from_ref(&first),
            chronos::attach::UndoOptions { skip_changed: false, dry_run: false, ..Default::default() },
        )
        .unwrap();
    assert_eq!((done.undone, done.skipped), (5, 0), "{done:?}");
    assert!(done.merge.as_deref().is_some_and(|m| m.starts_with("merge:") && m != first), "{done:?}");
    assert_eq!(state(&mut pg), was);
    // the instant read in UTC: `seen::text` prints in the server's TimeZone (Asia/Kolkata gave +05:30)
    assert_eq!(
        pg_cell(&mut pg, "select meta::text || (seen at time zone 'utc')::text from leads where id = 1"),
        "{\"a\": 1}2026-10-01 10:00:00"
    );
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "3");
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0, "main is Postgres's");
    // once undone, its rows are as they were before it: a second undo finds nothing to do, and writes nothing
    let again = db
        .undo_merges(
            std::slice::from_ref(&first),
            chronos::attach::UndoOptions { skip_changed: false, dry_run: false, ..Default::default() },
        )
        .unwrap();
    assert_eq!((again.undone, again.skipped, again.merge.is_none()), (0, 0, true), "{again:?}");
    assert!(again.already > 0 && again.rows.iter().all(|r| r.status == "already"), "{again:?}");
    assert_eq!(state(&mut pg), was);

    // two merges undone at once, the later first: the row both wrote goes back to how it was before the first
    let a = merge("a", "update leads set owner = 'one' where id = 1");
    let b = merge("b", "update leads set owner = 'two' where id = 1; update leads set score = 0 where id = 2");
    let done = db
        .undo_merges(
            &[a.clone(), b.clone()],
            chronos::attach::UndoOptions { skip_changed: false, dry_run: false, ..Default::default() },
        )
        .unwrap();
    assert_eq!((done.undone, done.skipped), (3, 0), "{done:?}");
    assert_eq!(state(&mut pg), was);

    // a row changed since, by a later merge and straight in Postgres: named and nothing undone, or left with
    // skip_changed while the rest goes back
    let c = merge("c", "update leads set owner = 'cy' where id in (1, 2, 3)");
    merge("d", "update leads set owner = 'later' where id = 1");
    pg.batch_execute("update leads set owner = 'pat' where id = 2").unwrap();
    let e = db
        .undo_merges(
            std::slice::from_ref(&c),
            chronos::attach::UndoOptions { skip_changed: false, dry_run: false, ..Default::default() },
        )
        .unwrap_err();
    assert!(e.to_string().contains("changed again since") || e.to_string().contains("changed in Postgres"), "{e}");
    assert_eq!(pg_cell(&mut pg, "select string_agg(owner, ',' order by id) from leads"), "later,pat,cy");
    let done = db
        .undo_merges(
            std::slice::from_ref(&c),
            chronos::attach::UndoOptions { skip_changed: true, dry_run: false, ..Default::default() },
        )
        .unwrap();
    assert_eq!((done.undone, done.skipped), (1, 2), "{done:?}");
    assert_eq!(pg_cell(&mut pg, "select string_agg(owner, ',' order by id) from leads"), "later,pat,eve");
    let changed: Vec<_> = done.rows.iter().filter(|r| r.status == "changed").collect();
    assert!(changed.iter().any(|r| r.now["owner"] == "pat"), "{done:?}");
    // undone in part: the row put back isn't "changed since" to a second undo, which the two changed rows still
    // refuse whole; once one holds what the merge wrote again, it goes back with skip_changed
    let plan = db
        .undo_merges(
            std::slice::from_ref(&c),
            chronos::attach::UndoOptions { skip_changed: false, dry_run: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!((plan.undone, plan.skipped, plan.already), (0, 2, 1), "{plan:?}");
    let e = db
        .undo_merges(
            std::slice::from_ref(&c),
            chronos::attach::UndoOptions { skip_changed: false, dry_run: false, ..Default::default() },
        )
        .unwrap_err();
    assert!(e.to_string().contains("2 rows the merge changed have changed again since"), "{e}");
    merge("e", "update leads set owner = 'cy' where id = 1");
    let done = db
        .undo_merges(
            std::slice::from_ref(&c),
            chronos::attach::UndoOptions { skip_changed: true, dry_run: false, ..Default::default() },
        )
        .unwrap();
    assert_eq!((done.undone, done.skipped, done.already), (1, 1, 1), "{done:?}");
    assert_eq!(pg_cell(&mut pg, "select owner from leads where id = 2"), "pat");
    assert_ne!(pg_cell(&mut pg, "select owner from leads where id = 1"), "cy", "back as it was before the merge");

    // a cascade: the undo removes a lead the merge added, and Postgres would delete the tasks added under it since.
    // The dry run names the table and writes nothing; the undo is refused until told ALLOW CASCADE
    pg.batch_execute(
        "create table tasks (id serial primary key, lead_id int references leads (id) on delete cascade, what text)",
    )
    .unwrap();
    drop(db);
    let dir2 = folder("undoid2");
    let (ok, out) = attach(&dir2, &["--tables", "leads,tasks"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir2).unwrap();
    run(&db, MAIN, "create world n", &[]).unwrap();
    run(&db, "n", "insert into leads (owner, score) values ('fresh', 1)", &[]).unwrap();
    let _ = chronos::attach::take_merge_id();
    run(&db, MAIN, "merge world n", &[]).unwrap();
    let n = chronos::attach::take_merge_id().expect("the merge's id");
    let fresh = pg_cell(&mut pg, "select id from leads where owner = 'fresh'");
    pg.batch_execute(&format!("insert into tasks (lead_id, what) values ({fresh}, 'call'), ({fresh}, 'mail')"))
        .unwrap();
    let dry = chronos::attach::UndoOptions { dry_run: true, ..Default::default() };
    let plan = db.undo_merges(std::slice::from_ref(&n), dry).unwrap();
    assert_eq!(plan.cascades.as_deref(), Some(&["tasks".to_string()][..]), "{plan:?}");
    assert_eq!(pg_cell(&mut pg, "select count(*) from tasks"), "2", "a dry run writes nothing");
    assert_eq!(pg_cell(&mut pg, "select count(*) from leads where owner = 'fresh'"), "1");
    let e = db.undo_merges(std::slice::from_ref(&n), Default::default()).unwrap_err();
    assert!(matches!(&e, chronos::Error::Constraint { code, .. } if *code == chronos::attach::CASCADE), "{e}");
    assert_eq!(pg_cell(&mut pg, "select count(*) from tasks"), "2", "refused: nothing written");
    // by the statement, allowed: the lead goes, and its tasks with it
    let out = run(&db, MAIN, &format!("undo merge id '{n}' dry run"), &[]).unwrap();
    assert!(format!("{:?}", out.last().unwrap().tag).contains("DRY RUN 1"), "{out:?}");
    run(&db, MAIN, &format!("undo merge id '{n}' allow cascade"), &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select count(*) from leads where owner = 'fresh'"), "0");
    assert_eq!(pg_cell(&mut pg, "select count(*) from tasks"), "0");
    // a dry run with no cascade says none, and finds a row changed straight in Postgres
    let p = merge2(&db, "p", "update leads set owner = 'pp' where id in (1, 2)");
    pg.batch_execute("update leads set owner = 'straight' where id = 1").unwrap();
    let plan = db.undo_merges(std::slice::from_ref(&p), dry).unwrap();
    assert_eq!(plan.cascades.as_deref(), Some(&[][..]), "{plan:?}");
    assert_eq!((plan.undone, plan.skipped), (1, 1), "{plan:?}");
    assert_eq!(pg_cell(&mut pg, "select owner from leads where id = 2"), "pp", "a dry run writes nothing");
    // effects the merge brought into main aren't undone: listed with where each is (one sent, one still queued)
    run(&db, MAIN, "create world fx", &[]).unwrap();
    run(
        &db,
        "fx",
        "update leads set score = 7 where id = 3; notify on merge sync, 'lead 3'; notify on merge mail, 'hello'",
        &[],
    )
    .unwrap();
    let _ = chronos::attach::take_merge_id();
    run(&db, MAIN, "merge world fx", &[]).unwrap();
    let fx = chronos::attach::take_merge_id().expect("the merge's id");
    let sync = db.outbox(MAIN).unwrap().into_iter().find(|e| e.channel == "sync").unwrap();
    db.ack_outbox(std::slice::from_ref(&sync.id)).unwrap();
    let done = db.undo_merges(std::slice::from_ref(&fx), Default::default()).unwrap();
    let mut effects: Vec<(String, &str)> = done.effects.iter().map(|e| (e.channel.clone(), e.state)).collect();
    effects.sort();
    assert_eq!(effects, [("mail".to_string(), "queued"), ("sync".to_string(), "sent")], "{done:?}");
    assert_eq!(done.undone, 1, "{done:?}");
    std::fs::remove_dir_all(&dir).unwrap();
    let dir = dir2;

    // an id that isn't a merge's
    let e = db
        .undo_merges(
            &["merge:nope".into()],
            chronos::attach::UndoOptions { skip_changed: false, dry_run: false, ..Default::default() },
        )
        .unwrap_err();
    assert!(e.to_string().contains("no undo record"), "{e}");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn a_merge_bigger_than_one_part_is_written_in_one_transaction() {
    let Some((mut pg, name, _url, _turn)) = throwaway("parts") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table items (id int primary key, n int);
         insert into items select g, 0 from generate_series(1, 12000) g;",
    )
    .unwrap();
    let dir = folder("parts");
    let (ok, out) = attach(&dir, &["--tables", "items", "--yes"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    assert!(
        change(&db, "w", "update items set n = 1 where id <= 11000; insert into items select g, 2 from generate_series(12001, 12100) g")
            .is_none()
    );
    assert_eq!(pg_cell(&mut pg, "select count(*) from items where n = 1"), "11000");
    assert_eq!(pg_cell(&mut pg, "select count(*) from items"), "12100");
    // a conflict in the last part: nothing of any part is written
    run(&db, MAIN, "create world v; use world v; update items set n = 3", &[]).unwrap();
    pg.batch_execute("update items set n = 9 where id = 12100").unwrap();
    let e = run(&db, MAIN, "merge world v", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select count(*) from items where n = 3"), "0");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// The pids of the connections to database `name` that say they are `chronos attach`'s, once there are `n` of them
/// (a process that just ended may take a moment to let its go), or as they are after 10 s.
fn backends(pg: &mut postgres::Client, name: &str, n: usize) -> Vec<String> {
    let sql = format!(
        "select pid from pg_stat_activity where datname = '{name}' and application_name = 'chronos attach' order by pid"
    );
    let mut got = vec![];
    for _ in 0..100 {
        got = pg
            .simple_query(&sql)
            .unwrap()
            .iter()
            .filter_map(|m| match m {
                postgres::SimpleQueryMessage::Row(r) => r.get(0).map(String::from),
                _ => None,
            })
            .collect();
        if got.len() == n {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    got
}

/// Merges into main keep one connection to Postgres between them (one backend, the same pid), and a refresh and a dry
/// run use it too: no connect, TLS handshake and login per merge. One the server closed meanwhile
/// (`pg_terminate_backend`) fails at the merge's first statement, before anything is written, and the merge makes a
/// new one and goes through, written once.
#[test]
fn merges_keep_one_connection_and_make_a_new_one_when_it_is_gone() {
    let Some((mut pg, name, url, _turn)) = throwaway("keep") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    // a pooler in front (as Supabase's Supavisor) names the sessions itself: there's no `chronos attach` to count
    let sep = if url.contains('?') { '&' } else { '?' };
    let mut probe =
        postgres::Client::connect(&format!("{url}{sep}application_name=chronos_probe"), postgres::NoTls).unwrap();
    if pg_cell(&mut probe, "select application_name from pg_stat_activity where pid = pg_backend_pid()")
        != "chronos_probe"
    {
        eprintln!("skipped: a pooler in front renames the sessions");
        drop(probe);
        drop_db(&name);
        return;
    }
    drop(probe);
    pg.batch_execute("create table t (id int primary key, v int); insert into t values (1, 0), (2, 0)").unwrap();
    let dir = folder("keep");
    let (ok, out) = attach(&dir, &["--tables", "t"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    assert!(change(&db, "a", "update t set v = 1 where id = 1").is_none());
    let first = backends(&mut pg, &name, 1);
    assert_eq!(first.len(), 1, "one connection after a merge: {first:?}");
    let pid = first[0].clone();
    let e = change(&db, "b", "update t set v = 2 where id = 1");
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(backends(&mut pg, &name, 1), vec![pid.clone()], "the second merge used the first's connection");
    chronos::attach::refresh(&db).unwrap();
    run(&db, MAIN, "create world d", &[]).unwrap();
    run(&db, "d", "update t set v = 9 where id = 2", &[]).unwrap();
    run(&db, MAIN, "merge world d dry run", &[]).unwrap();
    assert_eq!(backends(&mut pg, &name, 1), vec![pid.clone()], "a refresh and a dry run use it too");
    assert_eq!(pg_cell(&mut pg, "select v from t where id = 2"), "0", "the dry run wrote nothing");

    // the server ends it between merges: the next merge connects again, and is written once
    assert_eq!(pg_cell(&mut pg, &format!("select pg_terminate_backend({pid})")), "t");
    assert!(backends(&mut pg, &name, 0).is_empty(), "the connection is gone");
    let e = change(&db, "c", "update t set v = 3 where id = 1");
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(pg_cell(&mut pg, "select v from t where id = 1"), "3");
    assert_eq!(cell(&db, MAIN, "select v from t where id = 1"), "3");
    assert_eq!(pg_cell(&mut pg, "select count(*) from chronos.merges where id like 'merge:%'"), "3");
    let now = backends(&mut pg, &name, 1);
    assert!(now.len() == 1 && now[0] != pid, "a new connection, kept in turn: {now:?} (was {pid})");
    // and kept again for the merge after
    let e = change(&db, "e", "update t set v = 4 where id = 1");
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(backends(&mut pg, &name, 1), now);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// UNDO MERGE ... SKIP CHANGED in an attached folder leaves a row changed straight in Postgres (one main hasn't heard
/// of yet) and puts the rest back there; the effects the merge sent aren't undone, and are listed with where they are.
/// UNDO AGENT on main goes back through Postgres too. Issue 2090-labs/Chronos-DB#48.
#[test]
fn undo_skips_rows_changed_in_postgres_and_lists_the_effects_it_cant_undo() {
    let Some((mut pg, name, _url, _turn)) = throwaway("undoskip") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, status text);
         insert into leads values (1, 'active'), (2, 'active'), (3, 'active');",
    )
    .unwrap();
    let dir = folder("undoskip");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    // the run that hurt: records marked inactive, and a webhook told of it
    let e = change(
        &db,
        "w",
        "update leads set status = 'inactive'; notify on merge sync, 'deactivated 3'; notify on merge mail, 'bye'",
    );
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(pg_cell(&mut pg, "select string_agg(status, ',' order by id) from leads"), "inactive,inactive,inactive");
    // a worker sent the webhook; the email waits
    let sync = db.outbox(MAIN).unwrap().into_iter().find(|e| e.channel == "sync").unwrap();
    db.ack_outbox(std::slice::from_ref(&sync.id)).unwrap();
    // someone fixes row 2 by hand in Postgres; main hasn't seen it
    pg.batch_execute("update leads set status = 'archived' where id = 2").unwrap();
    assert_eq!(cell(&db, MAIN, "select status from leads where id = 2"), "inactive");

    let u = db.undo_merge("w", true).unwrap();
    assert_eq!((u.rows, u.skipped), (2, 1), "{u:?}");
    assert_eq!(pg_cell(&mut pg, "select string_agg(status, ',' order by id) from leads"), "active,archived,active");
    assert_eq!(cell(&db, MAIN, "select string_agg(status, ',' order by id) from leads"), "active,archived,active");
    let mut effects: Vec<(String, &str)> = u.effects.iter().map(|e| (e.channel.clone(), e.state)).collect();
    effects.sort();
    assert_eq!(effects, [("mail".to_string(), "queued"), ("sync".to_string(), "sent")]);
    assert_eq!(db.outbox(MAIN).unwrap().len(), 1, "the email is still queued: the undo leaves effects alone");

    // UNDO AGENT on main: through Postgres as well, conflicts and all
    run(&db, MAIN, "create merge policy p with (max_rows = 100)", &[]).unwrap();
    run(&db, MAIN, "create agent bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'p')", &[])
        .unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(
        &db,
        "create world b; use world b; update leads set status = 'gone' where id in (1, 3); use world main",
        &[],
    )
    .unwrap();
    bot.run(&db, "merge world b", &[]).unwrap();
    assert_eq!(pg_cell(&mut pg, "select string_agg(status, ',' order by id) from leads"), "gone,archived,gone");
    pg.batch_execute("update leads set status = 'kept' where id = 3").unwrap();
    let e = run(&db, MAIN, "undo agent bot since '-1 hour'", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select string_agg(status, ',' order by id) from leads"), "gone,archived,kept");
    let out = run(&db, MAIN, "undo agent bot since '-1 hour' skip changed", &[]).unwrap();
    assert_eq!(out[0].tag, "UNDO AGENT 1");
    assert_eq!(pg_cell(&mut pg, "select string_agg(status, ',' order by id) from leads"), "active,archived,kept");
    assert_eq!(cell(&db, MAIN, "select string_agg(status, ',' order by id) from leads"), "active,archived,kept");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// A rehearsal's lines: (outcome, detail) of each line of its plan.
fn plan_lines(db: &Db, sql: &str) -> Vec<(String, String)> {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap();
    out.rows.iter().map(|r| (r[2].to_text(), r[3].to_text())).collect()
}

fn line<'a>(lines: &'a [(String, String)], outcome: &str) -> Vec<&'a str> {
    lines.iter().filter(|(o, _)| o == outcome).map(|(_, d)| d.as_str()).collect()
}

/// `MERGE WORLD w REHEARSE [FAIL AT n]`: the write-back run in Postgres (triggers, constraints, deferred ones too),
/// then rolled back, Postgres checked unchanged after, and the effects the merge would send listed, unsent. Issue
/// 2090-labs/Chronos-DB#48.
#[test]
fn a_rehearsal_runs_the_write_back_in_postgres_and_rolls_it_back() {
    let Some((mut pg, name, _url, _turn)) = throwaway("rehearse") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table accounts (id int primary key, name text);
         create table leads (id int primary key, account int references accounts (id) deferrable initially deferred,
             status text, touched text);
         create function touch() returns trigger language plpgsql as
             $$ begin new.touched := 'by trigger: ' || new.status; return new; end $$;
         create trigger leads_touch before insert or update on leads for each row execute function touch();
         insert into accounts values (1, 'acme'), (2, 'globex');
         insert into leads values (1, 1, 'active', null), (2, 2, 'active', null);",
    )
    .unwrap();
    let dir = folder("rehearse");
    let (ok, out) = attach(&dir, &["--tables", "accounts,leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let before = "select string_agg(id || ':' || status || ':' || coalesce(touched, '-'), ',' order by id) from leads";
    let was = pg_cell(&mut pg, before);

    // to the end: each statement, what the trigger set, rolled back, Postgres as it was, the effect unsent
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(
        &db,
        "w",
        "update accounts set name = 'Acme' where id = 1; update leads set status = 'inactive'; \
         notify on merge sync, 'deactivated'",
        &[],
    )
    .unwrap();
    let l = plan_lines(&db, "merge world w rehearse");
    let steps = line(&l, "rehearsed");
    assert_eq!(steps.len(), 2, "{l:?}");
    assert!(steps[0].contains("update accounts: 1 rows") && steps[1].contains("update leads: 2 rows"), "{steps:?}");
    assert_eq!(line(&l, "postgres set").len(), 2, "{l:?}");
    assert!(line(&l, "postgres set")[0].contains("by trigger: inactive"), "{l:?}");
    assert!(line(&l, "rolled back")[0].contains("as it did before"), "{l:?}");
    assert!(line(&l, "stopped").is_empty(), "{l:?}");
    assert!(line(&l, "postgres")[0].contains("rehearsed in Postgres"), "{l:?}");
    assert!(line(&l, "effect")[0].contains("sync") && line(&l, "effect")[0].contains("sent nothing"), "{l:?}");
    assert_eq!(pg_cell(&mut pg, before), was);
    assert_eq!(pg_cell(&mut pg, "select name from accounts where id = 1"), "acme");
    assert!(db.outbox(MAIN).unwrap().is_empty(), "nothing sent, nothing merged");
    assert_eq!(cell(&db, "w", "select status from leads where id = 1"), "inactive", "the world is as it was");

    // FAIL AT 1: the first statement runs, then the rollback, on the real rows
    let l = plan_lines(&db, "merge world w rehearse fail at 1");
    assert_eq!(line(&l, "rehearsed").len(), 1, "{l:?}");
    assert!(line(&l, "stopped")[0].contains("FAIL AT"), "{l:?}");
    assert!(line(&l, "rolled back")[0].contains("as it did before"), "{l:?}");
    assert_eq!(pg_cell(&mut pg, "select name from accounts where id = 1"), "acme");

    // a constraint Postgres has that Chronos doesn't: the statement that breaks it, and where
    pg.batch_execute("alter table leads add constraint known check (status in ('active', 'archived'))").unwrap();
    let l = plan_lines(&db, "merge world w rehearse");
    let stopped = line(&l, "stopped");
    assert!(stopped[0].contains("statement 2 (update leads)") && stopped[0].contains("known"), "{l:?}");
    assert_eq!(pg_cell(&mut pg, before), was);
    pg.batch_execute("alter table leads drop constraint known").unwrap();

    // a deferred constraint trigger, run at SET CONSTRAINTS ALL IMMEDIATE as the merge runs it, not at COMMIT
    pg.batch_execute(
        "create function no_moving() returns trigger language plpgsql as
             $$ begin if new.status = 'moved' then raise exception 'no moving leads'; end if; return null; end $$;
         create constraint trigger leads_no_moving after update on leads deferrable initially deferred
             for each row execute function no_moving();",
    )
    .unwrap();
    run(&db, MAIN, "create world d", &[]).unwrap();
    run(&db, "d", "update leads set status = 'moved' where id = 2", &[]).unwrap();
    let l = plan_lines(&db, "merge world d rehearse");
    assert_eq!(line(&l, "rehearsed").len(), 1, "the statement itself went through: {l:?}");
    let stopped = line(&l, "stopped");
    assert!(stopped[0].contains("SET CONSTRAINTS") && stopped[0].contains("no moving leads"), "{l:?}");
    assert_eq!(pg_cell(&mut pg, before), was);

    // a row changed in Postgres since main's copy: said, and main doesn't take it (a rehearsal changes nothing)
    pg.batch_execute("update accounts set name = 'ACME' where id = 1").unwrap();
    let l = plan_lines(&db, "merge world w rehearse");
    assert!(line(&l, "changed in postgres").len() == 1 && line(&l, "rehearsed").is_empty(), "{l:?}");
    assert!(line(&l, "stopped")[0].contains("changed in Postgres"), "{l:?}");
    assert_eq!(cell(&db, MAIN, "select name from accounts where id = 1"), "acme");
    assert!(!line(&l, "blocked").is_empty(), "{l:?}");

    // a merge into another world doesn't write to Postgres: nothing to rehearse
    run(&db, MAIN, "create world x; create world y", &[]).unwrap();
    let e = run(&db, MAIN, "merge world x into y rehearse", &[]).unwrap_err();
    assert_eq!(e.code, "0A000", "{}", e.message);

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

#[test]
fn a_rehearsal_is_refused_where_nothing_is_attached() {
    let dir = folder("rehearse-none");
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "create table t (id int primary key); create world w", &[]).unwrap();
    run(&db, "w", "insert into t values (1)", &[]).unwrap();
    let e = run(&db, MAIN, "merge world w rehearse", &[]).unwrap_err();
    assert!(e.code == "0A000" && e.message.contains("DRY RUN"), "{}", e.message);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Drops the replication slots left on database `name` (a database with one can't be dropped), however the test ends.
struct Slots(String);

impl Drop for Slots {
    fn drop(&mut self) {
        let admin_url = std::env::var("CHRONOS_TEST_PG").unwrap();
        let mut admin = postgres::Client::connect(&admin_url, postgres::NoTls).unwrap();
        let sql = format!(
            "select pg_drop_replication_slot(slot_name) from pg_replication_slots where database = '{}'",
            self.0
        );
        admin.batch_execute(&sql).unwrap();
    }
}

#[test]
fn follow_takes_postgres_changes_as_they_happen() {
    let Some((mut pg, name, _url, _turn)) = throwaway("follow") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    if pg_cell(&mut pg, "show wal_level") != "logical" {
        eprintln!("skipped: the test Postgres's wal_level isn't logical");
        drop(pg);
        drop_db(&name);
        return;
    }
    let slots = Slots(name.clone());
    pg.batch_execute(
        "create table leads (id int primary key, name text, score numeric(10, 2));
         insert into leads select g, 'lead ' || g, g * 1.5 from generate_series(1, 300) g;
         create table notes (id int primary key, body text);
         alter table notes replica identity nothing;
         insert into notes values (1, 'a'), (2, 'b');
         create table events (id int, at date, what text, primary key (id, at)) partition by range (at);
         create table events_2026 partition of events for values from ('2026-01-01') to ('2027-01-01');
         insert into events values (1, '2026-03-01', 'x');
         create table other (id int primary key);",
    )
    .unwrap();
    let dir = folder("follow");
    let (ok, out) = attach(&dir, &["--tables", "leads,notes,events"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let tables = |r: &chronos::attach::Refreshed, t: &str| r.tables.iter().find(|x| x.0 == t).map(|x| (x.1, x.2, x.3));

    // the first follow makes the slot, and refreshes every table (changes before it aren't in the slot)
    let r = chronos::attach::follow(&db).unwrap();
    let slot = r.slot.clone().expect("the slot made");
    assert_eq!(r.changed(), 0, "{r:?}");
    let count = format!("select count(*) from pg_replication_slots where slot_name = '{slot}'");
    assert_eq!(pg_cell(&mut pg, &count), "1");
    let r = chronos::attach::follow(&db).unwrap();
    assert!(r.slot.is_none() && r.tables.is_empty(), "nothing changed, nothing read: {r:?}");

    // Postgres changes straight: main takes the rows they name, read by key
    pg.batch_execute(
        "update leads set name = 'renamed' where id = 7;
         insert into leads values (9001, 'new', 1);
         delete from leads where id = 40;
         update leads set id = 5000 where id = 8;
         insert into leads values (9002, 'gone', 1);
         delete from leads where id = 9002;
         insert into other values (1);
         delete from notes where id = 2;
         update events set what = 'y' where id = 1;",
    )
    .unwrap();
    let r = chronos::attach::follow(&db).unwrap();
    assert_eq!(tables(&r, "leads"), Some((2, 1, 2)), "{r:?}");
    // (a delete under REPLICA IDENTITY NOTHING doesn't say its row: the table is refreshed whole)
    assert_eq!(tables(&r, "notes"), Some((0, 0, 1)), "{r:?}");
    // (a partition's row, as its table's)
    assert_eq!(tables(&r, "events"), Some((0, 1, 0)), "{r:?}");
    assert_eq!(cell(&db, MAIN, "select name from leads where id = 7"), "renamed");
    assert_eq!(cell(&db, MAIN, "select count(*) from leads where id in (8, 40, 9002)"), "0");
    assert_eq!(cell(&db, MAIN, "select count(*) from leads where id in (5000, 9001)"), "2");
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "300");
    assert_eq!(cell(&db, MAIN, "select what from events where id = 1"), "y");
    assert_eq!(cell(&db, MAIN, "select count(*) from notes"), "1");

    // a merge's own write, coming back through the slot, takes nothing
    assert!(change(&db, "w", "update leads set name = 'mine' where id = 7").is_none());
    let r = chronos::attach::follow(&db).unwrap();
    assert_eq!(r.changed(), 0, "{r:?}");
    assert_eq!(cell(&db, MAIN, "select name from leads where id = 7"), "mine");

    // a TRUNCATE: the table refreshed whole
    pg.batch_execute("truncate notes; insert into notes values (3, 'c')").unwrap();
    let r = chronos::attach::follow(&db).unwrap();
    assert_eq!(tables(&r, "notes"), Some((1, 0, 1)), "{r:?}");
    assert_eq!(cell(&db, MAIN, "select body from notes"), "c");

    // the slot moved past what main took: nothing is read twice
    let r = chronos::attach::follow(&db).unwrap();
    assert!(r.tables.is_empty(), "{r:?}");
    assert!(db.attached().unwrap().refreshed_at() > 0);

    // unfollow drops the slot: Postgres keeps no WAL for the folder after
    assert!(chronos::attach::unfollow(&db).unwrap());
    assert_eq!(pg_cell(&mut pg, &count), "0");
    assert!(!chronos::attach::unfollow(&db).unwrap());

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop(slots);
    drop_db(&name);
}

#[test]
fn a_merge_by_columns_meeting_postgres_changes_to_other_columns_goes_through() {
    let Some((mut pg, name, _url, _turn)) = throwaway("bycols") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table leads (id int primary key, name text, score int);
         insert into leads select g, 'lead ' || g, g from generate_series(1, 5) g;",
    )
    .unwrap();
    let dir = folder("bycols");
    let (ok, out) = attach(&dir, &["--tables", "leads"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    let world = |w: &str, sql: &str| {
        run(&db, MAIN, &format!("create world {w}"), &[]).unwrap();
        run(&db, w, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    };

    // the world changes the name, Postgres the score, of the same row: by columns, both are kept, in one merge
    world("w1", "update leads set name = 'mine' where id = 1");
    pg.batch_execute("update leads set score = 100 where id = 1").unwrap();
    run(&db, MAIN, "merge world w1 by columns", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(pg_cell(&mut pg, "select name || ' ' || score from leads where id = 1"), "mine 100");
    assert_eq!(cell(&db, MAIN, "select name || ' ' || score from leads where id = 1"), "mine 100");

    // the same column changed on both sides: still a conflict, nothing written, main holds Postgres's value
    world("w2", "update leads set name = 'mine' where id = 2");
    pg.batch_execute("update leads set name = 'theirs' where id = 2").unwrap();
    let e = run(&db, MAIN, "merge world w2 by columns", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 2"), "theirs");
    assert_eq!(cell(&db, MAIN, "select name from leads where id = 2"), "theirs");

    // a merge not by columns stops at the first conflict, as before
    world("w3", "update leads set name = 'mine' where id = 3");
    pg.batch_execute("update leads set score = 300 where id = 3").unwrap();
    let e = run(&db, MAIN, "merge world w3", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert_eq!(pg_cell(&mut pg, "select name from leads where id = 3"), "lead 3");
    // and by columns after, it goes through
    run(&db, MAIN, "merge world w3 by columns", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(pg_cell(&mut pg, "select name || ' ' || score from leads where id = 3"), "mine 300");

    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// Gate S (CHRONOS-PLAN-2026-11-17-r3.md section 5, "No cross-tenant production writes"): the cases of
/// `tenant_isolation::cross_tenant_write_matrix`, against an attached Postgres. Every way a tenant-42 agent can send a
/// change ends with tenant 7's rows unchanged in Postgres itself, and in Chronos's main.
#[test]
fn cross_tenant_write_matrix_postgres() {
    if std::env::var("CHRONOS_TEST_PG").is_err() {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    }
    let shown = |db: &Db, s: &mut Session, sql: &str| match s.run(db, sql, &[]) {
        Ok(out) => out
            .iter()
            .flat_map(|r| r.rows.iter().map(|row| row.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => format!("ERROR {}: {}", e.code, e.message),
    };
    let pg_rows = |pg: &mut postgres::Client, sql: &str| -> String {
        pg.simple_query(sql)
            .unwrap()
            .iter()
            .filter_map(|m| match m {
                postgres::SimpleQueryMessage::Row(r) => {
                    Some((0..r.len()).map(|i| r.get(i).unwrap_or("NULL")).collect::<Vec<_>>().join("|"))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    const T7_LEADS: &str = "select id, org_id, name, owner from leads where org_id = 7 or id in (3, 4) order by id";
    const T7_NOTES: &str = "select id, org_id, lead_id, body from notes where org_id = 7 order by id";
    for (i, (what, steps)) in write_matrix::CASES.iter().enumerate() {
        let (mut pg, name, _url, turn) = throwaway(&format!("xtw{i}")).unwrap();
        pg.batch_execute(
            "create table leads (id int primary key, org_id int not null, name text, owner text);
             create table notes (id int primary key, org_id int not null, lead_id int, body text);
             create table plans (id int primary key, title text);
             insert into leads values (1, 42, 'acme', 'bo'), (2, 42, 'initech', 'bo'), (3, 7, 'globex', 'eve'),
                                      (4, 7, 'umbrella', 'eve');
             insert into notes values (1, 42, 1, 'call'), (2, 7, 3, 'globex renewal');
             insert into plans values (1, 'free'), (2, 'pro');",
        )
        .unwrap();
        let dir = folder(&format!("xtw{i}"));
        let (ok, out) = attach(&dir, &["--tables", "leads,notes,plans", "--yes"]);
        assert!(ok, "{what}: {out}");
        let db = Db::open(&dir).unwrap();
        // every right a tenant agent can hold short of admin, and a merge policy of its own that lets its merges write
        // to Postgres without a person (without one, each would only be queued, and the tenant checks never reached)
        run(
            &db,
            MAIN,
            "create merge policy open with (max_rows = 100000, max_deletes = 100000);
             create agent all42 with (can = 'read,fork,write_own,write,write_main,merge_own,merge,restore,outbox', \
             tenant = 'org_id = 42', policy = 'open')",
            &[],
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let pg_leads = pg_rows(&mut pg, T7_LEADS);
        let pg_notes = pg_rows(&mut pg, T7_NOTES);
        assert_eq!(pg_leads, "3|7|globex|eve\n4|7|umbrella|eve");
        let main_leads = shown(&db, &mut Session::new(MAIN), T7_LEADS);
        let mut s = Session::new(MAIN);
        s.agent = Some("all42".into());
        for step in steps.iter() {
            let said = shown(&db, &mut s, step);
            for secret in ["globex", "umbrella", "eve"] {
                let l = said.to_lowercase();
                assert!(
                    !l.split(|c: char| !c.is_alphanumeric()).any(|w| w == secret),
                    "{what}: {step} shows tenant 7's {secret:?}:\n{said}"
                );
            }
        }
        // a person then merges every world the agent left, as one approving its runs would: what the agent could
        // put in a world is its tenant's rows alone
        for (w, _) in db.branches().into_iter().filter(|(w, _)| w != MAIN) {
            let _ = run(&db, MAIN, &format!("merge world {w}"), &[]);
        }
        assert_eq!(pg_rows(&mut pg, T7_LEADS), pg_leads, "{what}: tenant 7's leads in Postgres");
        assert_eq!(pg_rows(&mut pg, T7_NOTES), pg_notes, "{what}: tenant 7's notes in Postgres");
        assert_eq!(shown(&db, &mut Session::new(MAIN), T7_LEADS), main_leads, "{what}: tenant 7's leads in main");
        // and the control: the same agent's change to its own tenant's row does reach Postgres
        let ctl = format!("ctl{i}");
        let said = shown(
            &db,
            &mut s,
            &format!(
                "use world main; create world ctl; use world ctl; insert into leads values (100, 42, 'own', '{ctl}'); \
                      use world main; merge world ctl"
            ),
        );
        assert!(!said.starts_with("ERROR"), "{what}: control merge: {said}");
        assert_eq!(
            pg_cell(&mut pg, "select owner from leads where id = 100"),
            ctl,
            "{what}: control merge in Postgres"
        );
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
        drop(pg);
        drop_db(&name);
        drop(turn);
    }
}

/// A crash after Postgres committed a partial merge (`ONLY KEYS`) of a durable world (one the crash doesn't flag):
/// recovery makes only the merge Postgres committed, not the rest of the world, nor what was written to it after the
/// crash. Main and Postgres agree on every row, and merging the rest later writes it to Postgres. Debug builds only.
#[test]
fn partial_merge_not_applied_twice() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: crash points are in debug builds only");
        return;
    }
    let Some((mut pg, name, _url, _turn)) = throwaway("partial_twice") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    let dir = folder("partial_twice");
    for late in [false, true] {
        pg.batch_execute(
            "drop table if exists t; create table t (id int primary key, v text, u text unique);
             insert into t values (1, 'a', 'a'), (2, 'b', 'b'), (3, 'c', 'c')",
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let (ok, out) = attach(&dir, &["--tables", "t"]);
        assert!(ok, "{out}");
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, "create world p", &[]).unwrap();
        run(&db, MAIN, "alter world p set (durable = true)", &[]).unwrap();
        // (u is UNIQUE: the merge's keys hold its index rows too, which recovery must not name)
        run(&db, "p", "update t set v = 'p', u = 'p' || id where id in (1, 2)", &[]).unwrap();
        drop(db);
        let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
            .args([dir.to_str().unwrap(), "merge world p only keys ('t/1');"])
            .env("CHRONOS_ATTACH_CRASH", "after_commit")
            .output()
            .unwrap();
        assert!(!out.status.success(), "it crashed: {}", String::from_utf8_lossy(&out.stdout));
        assert!(dir.join("attached.pending").exists());
        let all = "select string_agg(v, ',' order by id) from t";
        assert_eq!(pg_cell(&mut pg, all), "p,b,c", "Postgres has row 1 only");
        let db = Db::open(&dir).unwrap();
        if late {
            // work in the world goes on before anything merges into main
            run(&db, "p", "update t set v = 'late' where id = 3", &[]).unwrap();
        }
        let said = chronos::attach::resolve(&db).unwrap().unwrap_or_default();
        assert!(!dir.join("attached.pending").exists(), "{said}");
        assert_eq!(pg_cell(&mut pg, all), "p,b,c", "late {late}");
        assert_eq!(cell(&db, MAIN, all), "p,b,c", "late {late}: main and Postgres agree ({said})");
        assert!(db.world("p").is_ok(), "the partial merge leaves p open");
        if !late {
            // recovery made the partial merge: one row (2) is left to merge
            let left = db.diff("p").unwrap().into_iter().filter(|c| c.key.starts_with("t/")).count();
            assert_eq!(left, 1, "{said}");
        }
        let rest = if late { "p,p,late" } else { "p,p,c" };
        assert_eq!(cell(&db, "p", all), rest, "p keeps what is left");
        // the rest, merged later, reaches Postgres (after the late write, recovery took row 1 from Postgres, which p
        // changed too: settled as p has it)
        run(&db, MAIN, "merge world p using ours", &[]).unwrap();
        assert_eq!(pg_cell(&mut pg, all), rest);
        assert_eq!(cell(&db, MAIN, all), rest);
        drop(db);
    }
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// kill -9 at every step of a write-back, with a trigger writing a second table: after recovery, main and Postgres
/// agree on both tables, the world is merged exactly when Postgres committed, and each committed merge is in
/// Postgres's `chronos.merges` and main's merge ledger once. Recovery run again finds nothing to do. Debug builds only.
#[test]
fn crash_matrix_postgres() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: crash points are in debug builds only");
        return;
    }
    let Some((mut pg, name, _url, _turn)) = throwaway("crash_matrix") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table t (id int primary key, v text);
         create table seen (id int primary key, n int not null);
         insert into seen values (1, 0);
         create function count_write() returns trigger language plpgsql as $$
         begin update seen set n = n + 1 where id = 1; return new; end $$;
         create trigger t_count after insert or update on t for each row execute function count_write();
         insert into t values (1, 'a');",
    )
    .unwrap();
    let dir = folder("crash_matrix");
    let (ok, out) = attach(&dir, &["--tables", "t,seen"]);
    assert!(ok, "{out}");
    let ledger = |db: &Db| db.ledger_rows().unwrap().len();
    let base = {
        let db = Db::open(&dir).unwrap();
        ledger(&db)
    };
    let points = ["before_commit", "after_commit", "after_publish", "before_trigger_refresh", "before_forget"];
    let mut committed = 0;
    for (i, at) in points.iter().enumerate() {
        let (w, id) = (format!("m{i}"), 10 + i);
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, &format!("create world {w}"), &[]).unwrap();
        run(&db, &w, &format!("insert into t values ({id}, '{at}')"), &[]).unwrap();
        drop(db);
        let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
            .args(["-b", &w, dir.to_str().unwrap(), "merge"])
            .env("CHRONOS_ATTACH_CRASH", at)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{at}: it crashed: {}", String::from_utf8_lossy(&out.stdout));
        assert!(dir.join("attached.pending").exists(), "{at}: the outcome is left to settle");
        let did = *at != "before_commit";
        committed += usize::from(did);
        let db = Db::open(&dir).unwrap();
        let said = chronos::attach::resolve(&db).unwrap().unwrap_or_default();
        assert!(!dir.join("attached.pending").exists(), "{at}: {said}");
        assert_eq!(chronos::attach::resolve(&db).unwrap(), None, "{at}: settled once");
        for q in ["select string_agg(id || '=' || v, ',' order by id) from t", "select n::text from seen"] {
            assert_eq!(cell(&db, MAIN, q), pg_cell(&mut pg, q), "{at}: main and Postgres agree: {q} ({said})");
        }
        assert_eq!(pg_cell(&mut pg, &format!("select count(*) from t where id = {id}")), if did { "1" } else { "0" });
        assert_eq!(db.world(&w).is_err(), did, "{at}: merged exactly when Postgres committed ({said})");
        assert_eq!(
            pg_cell(&mut pg, "select count(*) from chronos.merges where id like 'merge:%'"),
            committed.to_string(),
            "{at}"
        );
        assert_eq!(ledger(&db) - base, committed, "{at}: each merge in main's ledger once ({said})");
        if !did {
            run(&db, MAIN, &format!("drop world {w}"), &[]).unwrap();
        }
        drop(db);
        // and once more after a clean open: the same
        let db = Db::open(&dir).unwrap();
        assert_eq!(ledger(&db) - base, committed, "{at}");
        drop(db);
    }
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// A Postgres set to lose commits it acknowledged (`synchronous_commit = off`) refuses merges into main, nothing
/// written; with CHRONOS_ATTACH_UNSAFE_SOURCE=1, or once it is set back, they go.
#[test]
fn unsafe_durability_settings_refused() {
    let Some((mut pg, name, _url, _turn)) = throwaway("unsafe") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute("create table t (id int primary key, v text); insert into t values (1, 'a')").unwrap();
    let dir = folder("unsafe");
    let (ok, out) = attach(&dir, &["--tables", "t"]);
    assert!(ok, "{out}");
    pg.batch_execute(&format!("alter database {name} set synchronous_commit = off")).unwrap();
    let db = Db::open(&dir).unwrap();
    let e = change(&db, "w1", "update t set v = 'x' where id = 1").expect("refused");
    assert!(
        e.message.contains("synchronous_commit = off") && e.message.contains("CHRONOS_ATTACH_UNSAFE_SOURCE"),
        "{}",
        e.message
    );
    assert_eq!(pg_cell(&mut pg, "select v from t where id = 1"), "a", "nothing written");
    assert_eq!(cell(&db, MAIN, "select v from t where id = 1"), "a");
    unsafe { std::env::set_var("CHRONOS_ATTACH_UNSAFE_SOURCE", "1") };
    let e = change(&db, "w2", "update t set v = 'y' where id = 1");
    unsafe { std::env::remove_var("CHRONOS_ATTACH_UNSAFE_SOURCE") };
    assert!(e.is_none(), "{:?}", e.map(|e| e.message));
    assert_eq!(pg_cell(&mut pg, "select v from t where id = 1"), "y");
    drop(db);
    pg.batch_execute(&format!("alter database {name} reset synchronous_commit")).unwrap();
    let db = Db::open(&dir).unwrap();
    assert!(change(&db, "w3", "update t set v = 'z' where id = 1").is_none());
    assert_eq!(pg_cell(&mut pg, "select v from t where id = 1"), "z");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// The reconciliation scan (`chronos refresh --check`, `attach::reconcile`): rows changed straight in Postgres, a row
/// added and one deleted there, and a column added, are reported per table with nothing taken into main; exit 1.
/// After a refresh it reports agreement; exit 0.
#[test]
fn reconciliation_scan_reports_divergence() {
    let Some((mut pg, name, _url, _turn)) = throwaway("reconcile") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute(
        "create table t (id int primary key, v text); insert into t values (1, 'a'), (2, 'b'), (3, 'c');
         create table u (id int primary key, v text); insert into u values (1, 'a')",
    )
    .unwrap();
    let dir = folder("reconcile");
    let (ok, out) = attach(&dir, &["--tables", "t,u"]);
    assert!(ok, "{out}");
    let scan = || {
        let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
            .args(["refresh", dir.to_str().unwrap(), "--check"])
            .output()
            .unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
    };
    let (code, text) = scan();
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("main agrees with"), "{text}");
    // --check changes nothing: with --unfollow or --tenant, which would, it is refused
    for other in [&["--unfollow"][..], &["--tenant", "1"][..]] {
        let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
            .args(["refresh", dir.to_str().unwrap(), "--check"])
            .args(other)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{other:?}");
    }
    pg.batch_execute(
        "update t set v = 'changed' where id = 1; delete from t where id = 2; insert into t values (4, 'd');
         alter table u add column note text",
    )
    .unwrap();
    let db = Db::open(&dir).unwrap();
    let r = chronos::attach::reconcile(&db).unwrap();
    let t = r.tables.iter().find(|x| x.0 == "t").unwrap();
    assert_eq!((t.1, t.2, t.3), (1, 1, 1), "one only in Postgres, one differing, one only in main: {:?}", r.tables);
    assert!(r.skipped.iter().any(|s| s.starts_with("u:") && s.contains("note")), "{:?}", r.skipped);
    // nothing taken
    assert_eq!(cell(&db, MAIN, "select string_agg(id || '=' || v, ',' order by id) from t"), "1=a,2=b,3=c");
    drop(db);
    let (code, text) = scan();
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("t: 1 only in") && text.contains("1 differ, 1 only in main"), "{text}");
    let out = Command::new(env!("CARGO_BIN_EXE_chronos")).args(["refresh", dir.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (code, text) = scan();
    assert_eq!(code, Some(0), "after a refresh: {text}");
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// A merge Postgres committed before a crash cut it short is made in Chronos as it was decided: a merge check made
/// since, which the merged result breaks, doesn't refuse it, so the world is merged and main agrees with Postgres. An
/// ordinary merge afterwards is still judged by that check. Debug builds only.
#[test]
fn recovery_applies_intent_payload_without_validation() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: crash points are in debug builds only");
        return;
    }
    let Some((mut pg, name, _url, _turn)) = throwaway("recorded") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute("create table t (id int primary key, v text); insert into t values (1, 'a')").unwrap();
    let dir = folder("recorded");
    let (ok, out) = attach(&dir, &["--tables", "t"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "create world p", &[]).unwrap();
    run(&db, MAIN, "alter world p set (durable = true)", &[]).unwrap();
    run(&db, "p", "update t set v = 'p' where id = 1", &[]).unwrap();
    drop(db);
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args([dir.to_str().unwrap(), "merge world p;"])
        .env("CHRONOS_ATTACH_CRASH", "after_commit")
        .output()
        .unwrap();
    assert!(!out.status.success(), "it crashed: {}", String::from_utf8_lossy(&out.stdout));
    assert!(dir.join("attached.pending").exists());
    assert_eq!(pg_cell(&mut pg, "select v from t where id = 1"), "p", "Postgres committed it");
    let db = Db::open(&dir).unwrap();
    // decided before the crash; a rule made since would refuse it now
    run(&db, MAIN, "create merge check no_p on tables (t) as select id from t where v = 'p'", &[]).unwrap();
    let said = chronos::attach::resolve(&db).unwrap().unwrap_or_default();
    assert!(!dir.join("attached.pending").exists(), "{said}");
    assert!(db.world("p").is_err(), "p is merged: {said}");
    assert_eq!(cell(&db, MAIN, "select v from t where id = 1"), "p", "main agrees with Postgres ({said})");
    // the check still judges what comes next
    run(&db, MAIN, "create world q", &[]).unwrap();
    run(&db, "q", "insert into t values (2, 'p')", &[]).unwrap();
    assert!(run(&db, MAIN, "merge world q", &[]).is_err(), "the merge check refuses a new merge");
    assert_eq!(pg_cell(&mut pg, "select count(*) from t where id = 2"), "0", "nothing written");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}

/// A Postgres that may lose commits it acknowledged is said at `chronos attach` and when `serve` opens the folder
/// (`attach::check`), not only at the first merge; not with CHRONOS_ATTACH_UNSAFE_SOURCE=1, nor once it is set back.
#[test]
fn unsafe_source_is_said_at_attach_and_serve_start() {
    let Some((mut pg, name, _url, _turn)) = throwaway("unsafe_said") else {
        eprintln!("skipped: set CHRONOS_TEST_PG to a Postgres URL");
        return;
    };
    pg.batch_execute("create table t (id int primary key, v text); insert into t values (1, 'a')").unwrap();
    pg.batch_execute(&format!("alter database {name} set synchronous_commit = off")).unwrap();
    let dir = folder("unsafe_said");
    let (ok, out) = attach(&dir, &["--tables", "t"]);
    assert!(ok, "{out}");
    assert!(out.contains("may lose commits") && out.contains("synchronous_commit = off"), "{out}");
    let db = Db::open(&dir).unwrap();
    let line = chronos::attach::check(&db).unwrap();
    assert!(line.contains("warning:") && line.contains("synchronous_commit = off"), "{line}");
    unsafe { std::env::set_var("CHRONOS_ATTACH_UNSAFE_SOURCE", "1") };
    let line = chronos::attach::check(&db);
    unsafe { std::env::remove_var("CHRONOS_ATTACH_UNSAFE_SOURCE") };
    assert!(!line.unwrap().contains("warning:"), "overridden");
    drop(db);
    pg.batch_execute(&format!("alter database {name} reset synchronous_commit")).unwrap();
    let db = Db::open(&dir).unwrap();
    let line = chronos::attach::check(&db).unwrap();
    assert!(!line.contains("warning:"), "{line}");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
    drop(pg);
    drop_db(&name);
}
