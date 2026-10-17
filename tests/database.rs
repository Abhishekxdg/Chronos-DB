//! CREATE DATABASE and DROP DATABASE: a database is a world (see `src/sql/database.rs`). Postgres
//! has no worlds, so what these do is checked here, not against it (`pgdiff.rs`).

use chronos::sql::{Session, run, take_notices};
use chronos::{Db, MAIN};

fn tag(db: &Db, s: &mut Session, sql: &str) -> String {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap().tag
}

fn code(db: &Db, s: &mut Session, sql: &str) -> &'static str {
    s.run(db, sql, &[]).expect_err(sql).code
}

fn count(db: &Db, world: &str, sql: &str) -> String {
    run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"))[0].rows[0][0].to_text()
}

fn messages() -> Vec<String> {
    take_notices().into_iter().map(|n| format!("{}: {}", n.level, n.message)).collect()
}

#[test]
fn a_new_database_is_empty_and_apart_from_main() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "create table t (id int primary key)");
    tag(&db, &mut s, "insert into t values (1)");

    assert_eq!(tag(&db, &mut s, "CREATE DATABASE test_app"), "CREATE DATABASE");
    let w = db.world("test_app").unwrap();
    assert_eq!(w.parent, None, "a database has no parent world");
    // nothing of main's: a migration that makes `t` doesn't find it there already
    assert_eq!(count(&db, "test_app", "select count(*) from information_schema.tables"), "0");
    run(&db, "test_app", "create table t (id int primary key, v text); insert into t values (7, 'x')", &[]).unwrap();
    assert_eq!(count(&db, "test_app", "select id from t"), "7");
    assert_eq!(count(&db, MAIN, "select id from t"), "1", "and main is as it was");

    // it can't be merged (it has nothing to merge into) and idle expiry leaves it alone
    let e = run(&db, "test_app", "merge world test_app", &[]).unwrap_err();
    assert_eq!(e.code, "0A000", "{e}");
    db.set_setting(chronos::Setting::WorldIdleTtl, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    assert_eq!(db.expire_worlds().unwrap(), 0);
    assert!(db.world("test_app").is_ok());
}

#[test]
fn options_are_read_as_postgres_reads_them() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for (sql, quiet) in [
        ("create database a", true),
        ("create database b with owner = chronos encoding = 'UTF8' template template0", true),
        ("create database \"c-d\" ENCODING 'unicode' LC_COLLATE 'C' LC_CTYPE 'C.UTF-8' tablespace pg_default", true),
        ("create database e connection limit -1 allow_connections true is_template false strategy wal_log", true),
        ("create database f locale_provider libc", true),
        ("create database g lc_collate 'en_US.UTF-8'", false),
        ("create database h connection limit 5", false),
    ] {
        assert_eq!(tag(&db, &mut s, sql), "CREATE DATABASE", "{sql}");
        let said = messages();
        assert_eq!(said.is_empty(), quiet, "{sql}: {said:?}");
        assert!(said.iter().all(|m| m.starts_with("WARNING: ") && m.contains("isn't applied")), "{said:?}");
    }
    assert!(db.world("c-d").is_ok());

    assert_eq!(code(&db, &mut s, "create database a"), "42P04", "exists");
    assert_eq!(code(&db, &mut s, "create database postgres"), "42P04");
    assert_eq!(code(&db, &mut s, "create database template0"), "42P04");
    assert_eq!(code(&db, &mut s, "create database main"), "42P04");
    assert_eq!(code(&db, &mut s, "create database x encoding 'latin1'"), "0A000");
    assert_eq!(code(&db, &mut s, "create database x strategy sideways"), "22023");
    assert_eq!(code(&db, &mut s, "create database x tablespace fast"), "42704");
    assert_eq!(code(&db, &mut s, "create database x owner me owner you"), "42601", "redundant");
    assert_eq!(code(&db, &mut s, "create database x bogus 1"), "42601");
    assert_eq!(code(&db, &mut s, "create database x template nowhere"), "3D000");
    assert_eq!(code(&db, &mut s, "create database x encoding 'nosuch'"), "42704");
    assert!(db.world("x").is_err(), "none of the refused ones made a world");
}

#[test]
fn template_forks_the_database_it_names() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "create table t (id int primary key); insert into t values (1), (2)");
    // main is `postgres`, the database tools connect to, so a copy of it is a copy of main
    tag(&db, &mut s, "create database copy1 template postgres");
    tag(&db, &mut s, "create database copy2 template = main");
    assert_eq!(count(&db, "copy1", "select count(*) from t"), "2");
    assert_eq!(db.world("copy2").unwrap().parent.as_deref(), Some(MAIN));
    run(&db, "copy1", "insert into t values (3)", &[]).unwrap();
    assert_eq!(count(&db, MAIN, "select count(*) from t"), "2", "the copy is its own");

    // and can't be dropped from under its copy
    tag(&db, &mut s, "create database copy3 template copy1");
    assert_eq!(code(&db, &mut s, "drop database copy1"), "2BP01");
    tag(&db, &mut s, "drop database copy3");
    tag(&db, &mut s, "drop database copy1");
}

#[test]
fn drop_database_says_what_postgres_says() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "create database gone");
    assert_eq!(tag(&db, &mut s, "drop database gone"), "DROP DATABASE");
    assert!(db.world("gone").is_err());

    assert_eq!(code(&db, &mut s, "drop database gone"), "3D000");
    assert_eq!(tag(&db, &mut s, "drop database if exists gone"), "DROP DATABASE");
    assert_eq!(messages(), ["NOTICE: database \"gone\" does not exist, skipping"]);
    tag(&db, &mut s, "create database g2");
    assert_eq!(tag(&db, &mut s, "drop database g2 with (force)"), "DROP DATABASE");
    tag(&db, &mut s, "create database g3");
    assert_eq!(tag(&db, &mut s, "drop database if exists g3 (force)"), "DROP DATABASE");

    // main is where a fresh connection lands, and template0 isn't a database to drop
    assert_eq!(code(&db, &mut s, "drop database main"), "55006", "the open one");
    assert_eq!(code(&db, &mut s, "drop database postgres"), "55006");
    assert_eq!(code(&db, &mut s, "drop database template0"), "42809");

    // the world a session is in, or connected to, is open: it can't drop it
    tag(&db, &mut s, "create database busy");
    let mut on = Session::new("busy");
    assert_eq!(code(&db, &mut on, "drop database busy"), "55006");
    assert_eq!(tag(&db, &mut s, "drop database busy"), "DROP DATABASE");
}

#[test]
fn neither_runs_in_a_transaction_or_for_an_agent() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for sql in ["create database t1", "drop database t1"] {
        tag(&db, &mut s, "begin");
        assert_eq!(code(&db, &mut s, sql), "25001", "{sql}");
        tag(&db, &mut s, "rollback");
    }
    assert!(db.world("t1").is_err());

    run(&db, MAIN, "create agent bot with (can = 'read,fork,write_own')", &[]).unwrap();
    let mut agent = Session::new(MAIN);
    agent.agent = Some("bot".into());
    assert_eq!(code(&db, &mut agent, "create database mine"), "42501");
    tag(&db, &mut s, "create database other");
    assert_eq!(code(&db, &mut agent, "drop database other"), "42501");
    assert!(db.world("other").is_ok() && db.world("mine").is_err());
}

#[test]
fn a_database_survives_a_restart_and_a_checkpoint() {
    let dir = std::env::temp_dir().join(format!("chronos-database-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, "create table t (id int primary key); insert into t values (1)", &[]).unwrap();
        run(&db, MAIN, "create database app", &[]).unwrap();
        run(&db, "app", "create table u (id int primary key, v text); insert into u values (5, 'five')", &[]).unwrap();
        run(&db, MAIN, "create database later", &[]).unwrap();
        db.close().unwrap(); // a clean shutdown: the next open reads a checkpoint, then the log after it
        drop(db);
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, "create database after_checkpoint", &[]).unwrap();
        run(&db, "after_checkpoint", "create table w (a int)", &[]).unwrap();
    }
    // the log alone (the database above wasn't closed) and the checkpoint before it
    let db = Db::open(&dir).unwrap();
    for name in ["app", "later", "after_checkpoint"] {
        assert_eq!(db.world(name).unwrap().parent, None, "{name}");
    }
    assert_eq!(count(&db, "app", "select v from u where id = 5"), "five");
    assert_eq!(count(&db, "app", "select count(*) from information_schema.tables"), "1");
    assert_eq!(count(&db, "after_checkpoint", "select count(*) from information_schema.tables"), "1");
    assert_eq!(count(&db, MAIN, "select count(*) from t"), "1");
    run(&db, MAIN, "drop database later", &[]).unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert!(db.world("later").is_err() && db.world("app").is_ok());
    // what happened to it reads back as it does for any world
    let history = run(&db, MAIN, "show history for world app", &[]).unwrap();
    let said: Vec<String> =
        history[0].rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join(" ")).collect();
    assert!(said.iter().any(|l| l.contains("created empty")), "{said:?}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
