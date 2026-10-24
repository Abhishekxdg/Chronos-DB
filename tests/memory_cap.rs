//! A soft, process-wide resident-memory cap (CHRONOS_MAX_MEMORY_BYTES): once the process holds
//! more memory than the cap, new statements are refused with 53200 until it frees up. A statement
//! already running is not interrupted; with no cap (0, the default) nothing is refused. The cap is
//! held per database here so these tests stay isolated from each other.

use chronos::sql::run;
use chronos::{Db, MAIN, limits};

#[test]
fn resident_size_is_readable_and_nonzero() {
    let rss = limits::process_rss_bytes().expect("this platform should report RSS");
    assert!(rss > 0, "a running process holds some resident memory");
}

#[test]
fn no_cap_refuses_nothing() {
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key)", &[]).unwrap();
    // the default cap is 0 (off): a statement runs however much memory the process holds
    run(&db, MAIN, "select 1", &[]).unwrap();
}

#[test]
fn over_the_cap_refuses_new_statements() {
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, v text)", &[]).unwrap();
    // one byte: the process is always over this, so the next statement is refused
    db.set_max_memory_bytes(1);
    let e = run(&db, MAIN, "select 1", &[]).unwrap_err();
    assert_eq!(e.code, "53200", "{}", e.message);
    assert!(e.message.contains("cap"), "{}", e.message);

    // lift the cap: statements run again, so the gate is a live check, not a latch
    db.set_max_memory_bytes(0);
    run(&db, MAIN, "select 1", &[]).unwrap();
}

#[test]
fn a_cap_above_current_usage_allows_statements() {
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key)", &[]).unwrap();
    // far above anything this process holds: nothing is refused
    db.set_max_memory_bytes(1 << 40); // 1 TiB
    run(&db, MAIN, "select 1", &[]).unwrap();
}
