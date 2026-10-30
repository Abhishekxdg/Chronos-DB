//! set_config() of the settings a session keeps itself, TimeZone, search_path and statement_timeout, sets them as
//! SET does, as Django's `SELECT set_config('TimeZone', %s, false)` on connecting expects: its datetimes are read in
//! that zone after.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn the_time_zone_is_the_sessions() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(run(&db, &mut s, "SELECT set_config('TimeZone', 'Africa/Nairobi', false)"), [["Africa/Nairobi"]]);
    assert_eq!(run(&db, &mut s, "SHOW TIME ZONE"), [["Africa/Nairobi"]]);
    assert_eq!(run(&db, &mut s, "SELECT '2011-09-01 10:20:30+00'::timestamptz::timestamp"), [["2011-09-01 13:20:30"]]);
    // in a transaction that commits, as Django connects with autocommit off
    run(&db, &mut s, "BEGIN; SELECT set_config('TimeZone', 'Europe/Paris', false); COMMIT");
    assert_eq!(run(&db, &mut s, "SHOW TIME ZONE"), [["Europe/Paris"]]);
    // LOCAL: until the transaction ends
    run(&db, &mut s, "BEGIN; SELECT set_config('TimeZone', 'Asia/Tokyo', true)");
    assert_eq!(run(&db, &mut s, "SHOW TIME ZONE"), [["Asia/Tokyo"]]);
    run(&db, &mut s, "COMMIT");
    assert_eq!(run(&db, &mut s, "SHOW TIME ZONE"), [["Europe/Paris"]]);
    // a statement that fails sets nothing
    assert!(s.run(&db, "SELECT set_config('TimeZone', 'Asia/Tokyo', false), 1 / 0", &[]).is_err());
    assert_eq!(run(&db, &mut s, "SHOW TIME ZONE"), [["Europe/Paris"]]);
    // NULL is the default
    run(&db, &mut s, "SELECT set_config('TimeZone', NULL, false)");
    assert_eq!(run(&db, &mut s, "SHOW TIME ZONE"), [["UTC"]]);
}

#[test]
fn a_zone_there_isnt_is_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let e = s.run(&db, "SELECT set_config('TimeZone', 'Nowhere/Land', false)", &[]).unwrap_err();
    assert_eq!(e.code, "22023", "{}", e.message);
    assert_eq!(run(&db, &mut s, "SHOW TIME ZONE"), [["UTC"]]);
}

#[test]
fn the_search_path_and_statement_timeout_are_the_sessions() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE SCHEMA a; SELECT set_config('search_path', 'a, public', false)");
    assert_eq!(run(&db, &mut s, "SHOW search_path"), [["a, public"]]);
    run(&db, &mut s, "CREATE TABLE t (x int4)");
    assert_eq!(run(&db, &mut s, "SELECT relnamespace::regnamespace FROM pg_class WHERE relname = 't'"), [["a"]]);
    run(&db, &mut s, "SELECT set_config('statement_timeout', '5s', false)");
    assert_eq!(run(&db, &mut s, "SELECT current_setting('statement_timeout')"), [["5s"]]);
}

#[test]
fn a_bad_value_stops_the_statements_writes() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE t (x int4)");
    let e = s.run(&db, "INSERT INTO t VALUES (1) RETURNING set_config('TimeZone', 'Nowhere/Land', false)", &[]);
    assert_eq!(e.unwrap_err().code, "22023");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM t"), [["0"]]);
    let e = s.run(&db, "INSERT INTO t VALUES (1) RETURNING set_config('statement_timeout', 'soon', false)", &[]);
    assert_eq!(e.unwrap_err().code, "22023");
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM t"), [["0"]]);
}

/// set_config('role', name, true) is SET LOCAL ROLE, as PostgREST runs it at the start of every request's
/// transaction: the role's rights until the transaction ends.
#[test]
fn the_role_is_set_for_the_transaction() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE ROLE web_anon; CREATE TABLE t (a int); INSERT INTO t VALUES (1)");
    run(&db, &mut s, "BEGIN");
    assert_eq!(run(&db, &mut s, "SELECT set_config('role', 'web_anon', true)"), [["web_anon"]]);
    assert_eq!(run(&db, &mut s, "SELECT current_user, current_setting('role')"), [["web_anon", "web_anon"]]);
    let e = s.run(&db, "SELECT * FROM t", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    run(&db, &mut s, "ROLLBACK");
    assert_eq!(run(&db, &mut s, "SELECT current_user, current_setting('role')"), [["chronos", "none"]]);
    // not LOCAL: for the session, until RESET ROLE or set_config('role', 'none', false)
    run(&db, &mut s, "SELECT set_config('role', 'web_anon', false)");
    assert_eq!(run(&db, &mut s, "SELECT current_user"), [["web_anon"]]);
    run(&db, &mut s, "SELECT set_config('role', 'none', false)");
    assert_eq!(run(&db, &mut s, "SELECT current_user"), [["chronos"]]);
    // a role there isn't is refused, and the session stays as it was
    let e = s.run(&db, "SELECT set_config('role', 'nobody', true)", &[]).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("22023", "role \"nobody\" does not exist"));
    assert_eq!(run(&db, &mut s, "SELECT current_user"), [["chronos"]]);
    // `NONE` is a role's name, not `none`: refused while the statement runs, its row not kept
    let e = s.run(&db, "INSERT INTO t VALUES (2) RETURNING set_config('role', 'NONE', false)", &[]).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("22023", "role \"NONE\" does not exist"));
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM t"), [["1"]]);
}

/// set_config('role', ...) of a role the session user may not become is refused while the statement runs, so the
/// statement's own writes go with it, as in Postgres: not after them, once they are kept.
#[test]
fn a_role_the_session_user_may_not_become_is_refused_before_any_write() {
    let (db, mut admin) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut admin,
        "CREATE ROLE writer LOGIN; CREATE ROLE other; CREATE TABLE t (a int); GRANT INSERT, SELECT ON t TO writer",
    );
    let mut s = Session::new(MAIN);
    s.user = Some("writer".into());
    s.log_in(&db, "writer").unwrap();
    let e = s.run(&db, "INSERT INTO t VALUES (1) RETURNING set_config('role', 'other', false)", &[]).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("42501", "permission denied to set role \"other\""));
    assert_eq!(run(&db, &mut admin, "SELECT count(*) FROM t"), [["0"]]);
    assert_eq!(run(&db, &mut s, "SELECT current_user"), [["writer"]]);
    // a role it is a member of, it may
    run(&db, &mut admin, "GRANT other TO writer");
    run(&db, &mut s, "SELECT set_config('role', 'other', false)");
    assert_eq!(run(&db, &mut s, "SELECT current_user"), [["other"]]);
}
