//! The slow-query log: a statement that runs at least as long as `log_min_duration_statement` is
//! emitted as one structured log line; below the threshold, or with it off (-1, the default),
//! nothing is logged. Lines are captured per-thread via `chronos::obs::capture`.

use chronos::obs::capture;
use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn session(db: &Db) -> Session {
    run(db, MAIN, "create table t (id int primary key, v text); insert into t values (1, 'one')", &[]).unwrap();
    Session::new(MAIN)
}

/// The slow-query lines captured while running `sql` as `s`.
fn slow_lines(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    let ((), lines) = capture(|| {
        s.run(db, sql, &[]).unwrap();
    });
    lines.into_iter().filter(|l| l.contains("event=slow_query")).collect()
}

#[test]
fn logs_all_statements_at_zero() {
    let db = Db::new();
    let mut s = session(&db);
    s.run(&db, "set log_min_duration_statement = 0", &[]).unwrap(); // log everything
    let lines = slow_lines(&db, &mut s, "select * from t");
    assert_eq!(lines.len(), 1, "one slow-query line: {lines:?}");
    let l = &lines[0];
    assert!(l.contains("level=WARN"), "{l}");
    assert!(l.contains("event=slow_query"), "{l}");
    assert!(l.contains("ms="), "{l}");
    assert!(l.contains("sql="), "{l}");
    assert!(l.contains("select * from t"), "the statement text is logged: {l}");
}

#[test]
fn off_by_default_logs_nothing() {
    let db = Db::new();
    let mut s = session(&db);
    // never set: the default is -1 (off)
    assert!(slow_lines(&db, &mut s, "select * from t").is_empty());
}

#[test]
fn minus_one_turns_it_off() {
    let db = Db::new();
    let mut s = session(&db);
    s.run(&db, "set log_min_duration_statement = 0", &[]).unwrap();
    assert_eq!(slow_lines(&db, &mut s, "select * from t").len(), 1, "on at 0");
    s.run(&db, "set log_min_duration_statement = -1", &[]).unwrap();
    assert!(slow_lines(&db, &mut s, "select * from t").is_empty(), "off again at -1");
}

#[test]
fn a_high_threshold_skips_fast_statements() {
    let db = Db::new();
    let mut s = session(&db);
    s.run(&db, "set log_min_duration_statement = '1h'", &[]).unwrap();
    assert!(slow_lines(&db, &mut s, "select * from t").is_empty(), "a trivial select is well under an hour");
}
