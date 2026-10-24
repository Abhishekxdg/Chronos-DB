//! The C trigger functions of the regression tests' libraries (regress.c's ttdummy, contrib/spi's autoinc and refint's
//! check_primary_key and check_foreign_key) are known by how they were made, LANGUAGE C, never by their body: a
//! PL/pgSQL function with the body one is kept with runs that body, and a C one stays C through reopening the database,
//! a world forked and merged, ALTER FUNCTION ... RENAME, CREATE OR REPLACE and the SQL that makes it again
//! (pg_get_functiondef, DIFF ... AS SQL), with pg_proc showing it as Postgres does. As a C function it fails as its C
//! does when called other than by a row trigger, costs 1, isn't counted by track_functions = 'pl', and `chronos
//! import` from a Chronos server brings it over.

mod common;

use chronos::sql::{Session, SqlError, Value};
use chronos::{Db, MAIN};
use std::process::Command;

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> SqlError {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => e,
    }
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chronos-c-trigger-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The body a C trigger function `symbol` is kept with.
fn c_body(symbol: &str) -> String {
    format!("begin raise exception '{symbol}: not fired by trigger manager'; end")
}

/// What pg_proc shows of function `name`: its language, prosrc and probin.
fn proc_row(db: &Db, s: &mut Session, name: &str) -> Vec<String> {
    run(
        db,
        s,
        &format!(
            "select l.lanname, p.prosrc, coalesce(p.probin, '-') from pg_proc p join pg_language l on l.oid = p.prolang
             where p.proname = '{name}'"
        ),
    )
    .remove(0)
}

/// The rows of table t autoinc numbered (their v, in order): one it left unnumbered is missing. (Its sequence
/// skips ahead on reopening, as Postgres's does after a crash, so the numbers themselves aren't compared.)
fn numbered_rows(db: &Db, s: &mut Session) -> Vec<String> {
    run(db, s, "select v from t where id > 0 order by v").into_iter().map(|r| r[0].clone()).collect()
}

/// autoinc's table: t, its id numbered from sequence seq by trigger `num` running function `f`.
fn numbered_table(db: &Db, s: &mut Session, f: &str) {
    run(
        db,
        s,
        &format!(
            "create sequence seq; create table t (id int4, v text);
             create trigger num before insert on t for each row execute procedure {f} (id, seq)"
        ),
    );
}

#[test]
fn a_plpgsql_function_with_a_c_triggers_body_runs_its_body() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for symbol in ["ttdummy", "autoinc", "check_primary_key", "check_foreign_key"] {
        run(
            &db,
            &mut s,
            &format!("create function {symbol}() returns trigger language plpgsql as $${}$$", c_body(symbol)),
        );
        run(
            &db,
            &mut s,
            &format!(
                "create table {symbol}_t (id int4, b int4);
                 create trigger tr before insert or update or delete on {symbol}_t for each row
                   execute procedure {symbol} ()"
            ),
        );
        let e = fails(&db, &mut s, &format!("insert into {symbol}_t values (1, 1)"));
        assert_eq!((e.code, e.message.as_str()), ("P0001", format!("{symbol}: not fired by trigger manager").as_str()));
        assert_eq!(proc_row(&db, &mut s, symbol), ["plpgsql", &c_body(symbol), "-"]);
    }
    // one given what autoinc takes raises, and the row isn't numbered and written
    numbered_table(&db, &mut s, "autoinc");
    let e = fails(&db, &mut s, "insert into t values (null, 'a')");
    assert_eq!((e.code, e.message.as_str()), ("P0001", "autoinc: not fired by trigger manager"));
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "0");
}

#[test]
fn a_c_trigger_stays_c_through_reopening_renaming_and_replacing() {
    let dir = temp("reopen");
    {
        let (db, mut s) = (Db::open(&dir).unwrap(), Session::new(MAIN));
        run(&db, &mut s, "CREATE FUNCTION autoinc () RETURNS trigger AS '/usr/lib/postgresql/autoinc.so' LANGUAGE C");
        numbered_table(&db, &mut s, "autoinc");
        run(&db, &mut s, "insert into t values (null, 'a')");
        assert_eq!(numbered_rows(&db, &mut s), ["a"]);
        // pg_proc has it as Postgres does: language c, its symbol and its library
        assert_eq!(proc_row(&db, &mut s, "autoinc"), ["c", "autoinc", "/usr/lib/postgresql/autoinc.so"]);
    }
    let (db, mut s) = (Db::open(&dir).unwrap(), Session::new(MAIN));
    run(&db, &mut s, "insert into t values (null, 'b')");
    assert_eq!(numbered_rows(&db, &mut s), ["a", "b"]);
    // renamed, it is still autoinc's C (and its trigger runs it under its new name)
    run(&db, &mut s, "alter function autoinc() rename to numbered");
    run(&db, &mut s, "insert into t values (null, 'c')");
    assert_eq!(numbered_rows(&db, &mut s), ["a", "b", "c"]);
    assert_eq!(proc_row(&db, &mut s, "numbered"), ["c", "autoinc", "/usr/lib/postgresql/autoinc.so"]);
    // replaced with a PL/pgSQL body (the very one it is kept with), it runs that body
    run(
        &db,
        &mut s,
        &format!("create or replace function numbered() returns trigger language plpgsql as $${}$$", c_body("autoinc")),
    );
    let e = fails(&db, &mut s, "insert into t values (null, 'd')");
    assert_eq!((e.code, e.message.as_str()), ("P0001", "autoinc: not fired by trigger manager"));
    assert_eq!(proc_row(&db, &mut s, "numbered"), ["plpgsql", &c_body("autoinc"), "-"]);
    // and replaced with the C again, it is the C again
    run(
        &db,
        &mut s,
        "create or replace function numbered() returns trigger as '/usr/lib/postgresql/autoinc.so', 'autoinc' language c",
    );
    run(&db, &mut s, "insert into t values (null, 'd')");
    assert_eq!(numbered_rows(&db, &mut s), ["a", "b", "c", "d"]);
    drop(db);
    let (db, mut s) = (Db::open(&dir).unwrap(), Session::new(MAIN));
    run(&db, &mut s, "insert into t values (null, 'e')");
    assert_eq!(numbered_rows(&db, &mut s), ["a", "b", "c", "d", "e"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_c_trigger_stays_c_through_a_world_and_the_sql_that_makes_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table k (id int4); insert into k values (1); create world w");
    let mut w = Session::new("w");
    run(
        &db,
        &mut w,
        "CREATE FUNCTION check_primary_key () RETURNS trigger AS '/usr/lib/postgresql/refint.so' LANGUAGE C;
         create table r (kid int4);
         create trigger has_k after insert on r for each row execute procedure check_primary_key ('kid', 'k', 'id')",
    );
    let missing = |db: &Db, s: &mut Session| {
        run(db, s, "insert into r values (1)");
        let e = fails(db, s, "insert into r values (2)");
        assert_eq!((e.code, e.message.as_str()), ("09000", "tuple references non-existent key"));
    };
    missing(&db, &mut w);
    // the SQL DIFF gives makes it again as the C, as does pg_get_functiondef's (what pg_dump writes from pg_proc)
    let diff = run(&db, &mut w, "diff world w as sql").concat().join("\n");
    assert!(diff.contains("language c volatile as '/usr/lib/postgresql/refint.so', 'check_primary_key'"), "{diff}");
    let def = one(&db, &mut w, "select pg_get_functiondef('check_primary_key'::regproc)");
    assert_eq!(
        def,
        "CREATE OR REPLACE FUNCTION public.check_primary_key()\n RETURNS trigger\n LANGUAGE c\n\
         AS '/usr/lib/postgresql/refint.so', $function$check_primary_key$function$\n"
    );
    run(&db, &mut s, "merge world w");
    missing(&db, &mut s);
    assert_eq!(proc_row(&db, &mut s, "check_primary_key"), ["c", "check_primary_key", "/usr/lib/postgresql/refint.so"]);
    for made in [def, diff.lines().find(|l| l.contains("create or replace function")).unwrap().to_string()] {
        let (db, mut s) = (Db::new(), Session::new(MAIN));
        run(&db, &mut s, "create table k (id int4); insert into k values (1)");
        run(&db, &mut s, &made);
        run(
            &db,
            &mut s,
            "create table r (kid int4);
             create trigger has_k after insert on r for each row execute procedure check_primary_key ('kid', 'k', 'id')",
        );
        missing(&db, &mut s);
    }
}

/// Called other than by a row trigger (in an expression, by CALL, as an event trigger), a C trigger function fails as
/// its C does: elog(ERROR), an internal error, with the C's message (autoinc's names no function) and no PL/pgSQL line
/// in its CONTEXT.
#[test]
fn a_c_trigger_called_otherwise_fails_as_its_c_does() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create function ti() returns int as 'regress', 'ttdummy' language c;
         create function ai() returns int as 'autoinc', 'autoinc' language c;
         create procedure check_foreign_key() as 'refint' language c;
         create function et() returns event_trigger as 'regress', 'ttdummy' language c",
    );
    let c_error = |e: SqlError, message: &str| {
        assert_eq!((e.code, e.message.as_str()), ("XX000", message));
        assert_eq!(e.info.and_then(|i| i.context), None, "{message}");
    };
    c_error(fails(&db, &mut s, "select ti()"), "ttdummy: not fired by trigger manager");
    c_error(fails(&db, &mut s, "select ai()"), "not fired by trigger manager");
    c_error(fails(&db, &mut s, "call check_foreign_key()"), "check_foreign_key: not fired by trigger manager");
    run(&db, &mut s, "create event trigger e on ddl_command_start execute function et()");
    let e = fails(&db, &mut s, "create table x (a int)");
    assert_eq!((e.code, e.message.as_str()), ("XX000", "ttdummy: not fired by trigger manager"));
}

/// A C trigger function's procost is Postgres's default for a C function, 1, and pg_get_functiondef (as pg_dump) writes
/// COST only where it isn't that.
#[test]
fn a_c_triggers_cost_is_a_c_functions() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create function autoinc() returns trigger as 'autoinc' language c;
         create function dear() returns trigger as 'autoinc', 'autoinc' language c cost 100",
    );
    assert_eq!(one(&db, &mut s, "select procost from pg_proc where proname = 'autoinc'"), "1");
    assert_eq!(one(&db, &mut s, "select procost from pg_proc where proname = 'dear'"), "100");
    assert_eq!(
        one(&db, &mut s, "select pg_get_functiondef('autoinc'::regproc)"),
        "CREATE OR REPLACE FUNCTION public.autoinc()\n RETURNS trigger\n LANGUAGE c\nAS 'autoinc', $function$autoinc$function$\n"
    );
    assert_eq!(
        one(&db, &mut s, "select pg_get_functiondef('dear'::regproc)"),
        "CREATE OR REPLACE FUNCTION public.dear()\n RETURNS trigger\n LANGUAGE c\n COST 100\n\
         AS 'autoinc', $function$autoinc$function$\n"
    );
}

/// track_functions = 'pl' counts PL/pgSQL functions' calls, not a C function's, which only 'all' counts (Postgres's
/// fmgr tracks a C function as it does a SQL one).
#[test]
fn track_functions_counts_a_c_trigger_as_c() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create function autoinc() returns trigger as 'autoinc' language c");
    numbered_table(&db, &mut s, "autoinc");
    run(&db, &mut s, "set track_functions = 'pl'; insert into t values (null, 'a'); insert into t values (null, 'b')");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_stat_user_functions"), "0");
    run(&db, &mut s, "set track_functions = 'all'; insert into t values (null, 'c')");
    assert_eq!(run(&db, &mut s, "select funcname, calls from pg_stat_user_functions"), [["autoinc", "1"]]);
}

/// `chronos import` from a Chronos server brings a C trigger function over as the C it is (pg_get_functiondef's
/// LANGUAGE c ... AS 'file', 'symbol', which Chronos makes), with the trigger that runs it.
#[test]
fn chronos_import_brings_a_c_trigger_over() {
    let dir = temp("import");
    let token = "admin-admin-admin-admin-0";
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.join("src").to_str().unwrap(), "--admin-token", token]);
    let mut served = common::serve(&mut cmd, true);
    let port = served.pg;
    let mut pg = postgres::Client::connect(
        &format!("host=127.0.0.1 port={port} user=tester password={token} dbname=main"),
        postgres::NoTls,
    )
    .unwrap();
    pg.batch_execute(
        "create function autoinc() returns trigger as '$libdir/autoinc' language c;
         create sequence seq; create table t (id int4, v text);
         create trigger num before insert on t for each row execute procedure autoinc (id, seq)",
    )
    .unwrap();
    let to = dir.join("to");
    let url = format!("postgres://tester:{token}@127.0.0.1:{port}/main");
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["import", to.to_str().unwrap(), &url, "--allow-plaintext-password"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let _ = served.child.kill();
    let _ = served.child.wait();
    assert!(out.status.success(), "{said}");
    // (what the report says it left out, if anything)
    let left_out = said.split_once("not imported").map_or("", |(_, r)| r);
    assert!(!left_out.contains("autoinc") && !left_out.contains("num on t"), "{said}");
    let (db, mut s) = (Db::open(&to).unwrap(), Session::new(MAIN));
    run(&db, &mut s, "insert into t values (null, 'a')");
    assert_eq!(numbered_rows(&db, &mut s), ["a"]);
    assert_eq!(proc_row(&db, &mut s, "autoinc"), ["c", "autoinc", "$libdir/autoinc"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
