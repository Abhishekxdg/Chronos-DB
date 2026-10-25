//! Operational server logs go through the structured `obs` sink (one `key=value` line per event),
//! not free-form `eprintln!`. This exercises one migrated path end to end: a damaged `reads` file
//! makes recovery emit a structured `reads_damaged` line, captured via `obs::capture`.

use chronos::obs::capture;
use chronos::{Db, MAIN};

#[test]
fn a_damaged_reads_file_logs_a_structured_line() {
    let dir = std::env::temp_dir().join(format!("chronos-structlog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        chronos::sql::run(&db, MAIN, "create table t (id int primary key)", &[]).unwrap();
    }
    // corrupt the recorded-reads file, then reopen and capture what recovery logs
    std::fs::write(dir.join("reads"), b"this is not json").unwrap();
    let (_db, lines) = capture(|| Db::open(&dir).unwrap());

    let line = lines.iter().find(|l| l.contains("event=reads_damaged")).unwrap_or_else(|| {
        panic!("expected a structured reads_damaged line, got: {lines:?}");
    });
    assert!(line.starts_with("level=WARN"), "{line}");
    assert!(line.contains("path="), "{line}");
    assert!(line.contains("detail="), "{line}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_intact_database_opens_without_operational_warnings() {
    let dir = std::env::temp_dir().join(format!("chronos-structlog-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        chronos::sql::run(&db, MAIN, "create table t (id int primary key)", &[]).unwrap();
    }
    let (_db, lines) = capture(|| Db::open(&dir).unwrap());
    assert!(
        !lines.iter().any(|l| l.contains("_damaged") || l.contains("_failed")),
        "a clean reopen should log no operational warnings: {lines:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
