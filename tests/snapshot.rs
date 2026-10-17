//! pg_export_snapshot() and SET TRANSACTION SNAPSHOT: one transaction hands its view of the
//! database to another session (what parallel `pg_dump -j` does), and the importing transaction
//! reads exactly that, writes nothing, and leaves the database as it was when it commits, also
//! after a restart replays the log.

use chronos::sql::{Output, Session, Value};
use chronos::{Db, MAIN};

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn q(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    rows(&s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap())
}

fn fails(db: &Db, s: &mut Session, sql: &str) -> &'static str {
    s.run(db, sql, &[]).expect_err(sql).code
}

/// The exporter's view: rows committed before its BEGIN, not its own writes, not what commits after.
fn exported(db: &Db) -> (Session, String) {
    let mut setup = Session::new(MAIN);
    q(db, &mut setup, "create table t (id int primary key, v text)");
    q(db, &mut setup, "insert into t values (1, 'before')");
    let mut exporter = Session::new(MAIN);
    q(db, &mut exporter, "begin isolation level repeatable read");
    q(db, &mut exporter, "insert into t values (2, 'exporter''s own')");
    let id = q(db, &mut exporter, "select pg_export_snapshot()").remove(0);
    q(db, &mut setup, "insert into t values (3, 'committed after')");
    (exporter, id)
}

#[test]
fn an_imported_snapshot_reads_what_the_exporter_reads() {
    let db = Db::new();
    let (mut exporter, id) = exported(&db);
    let mut importer = Session::new(MAIN);
    q(&db, &mut importer, "begin isolation level repeatable read, read only");
    q(&db, &mut importer, &format!("set transaction snapshot '{id}'"));
    assert_eq!(q(&db, &mut importer, "select id, v from t order by id"), ["1|before"]);
    // a later statement in it reads the same
    assert_eq!(q(&db, &mut importer, "select count(*) from t"), ["1"]);
    // a prepared read runs
    q(&db, &mut importer, "prepare r as select count(*) from t");
    assert_eq!(q(&db, &mut importer, "execute r"), ["1"]);
    // what it exports is the snapshot it imported, not the database it began on
    let again = q(&db, &mut importer, "select pg_export_snapshot()").remove(0);
    let mut third = Session::new(MAIN);
    q(&db, &mut third, "begin");
    q(&db, &mut third, "show search_path"); // (reads a setting, no snapshot)
    q(&db, &mut third, "set constraints all deferred");
    q(&db, &mut third, "listen ch");
    q(&db, &mut third, "prepare r3 as select id from t order by id"); // (prepares no snapshot)
    q(&db, &mut third, &format!("set transaction snapshot '{again}'"));
    // a second import is refused, as in Postgres
    assert_eq!(fails(&db, &mut third, &format!("set transaction snapshot '{again}'")), "25001");
    q(&db, &mut third, "rollback");
    q(&db, &mut third, "begin");
    q(&db, &mut third, &format!("set transaction snapshot '{again}'"));
    assert_eq!(q(&db, &mut third, "execute r3"), ["1"]);
    q(&db, &mut third, "commit");
    q(&db, &mut importer, "commit");
    // its COMMIT changes nothing: what committed meanwhile is still there
    let mut other = Session::new(MAIN);
    assert_eq!(q(&db, &mut other, "select id from t order by id"), ["1", "3"]);
    q(&db, &mut exporter, "commit");
    assert_eq!(q(&db, &mut other, "select id from t order by id"), ["1", "2", "3"]);
}

#[test]
fn an_importing_transaction_writes_nothing() {
    let db = Db::new();
    let (_exporter, id) = exported(&db);
    let mut importer = Session::new(MAIN);
    q(&db, &mut importer, "begin");
    q(&db, &mut importer, &format!("set transaction snapshot '{id}'"));
    assert_eq!(fails(&db, &mut importer, "insert into t values (9, 'x')"), "25006");
    assert_eq!(fails(&db, &mut importer, "select 1"), "25P02"); // the error aborted it
    q(&db, &mut importer, "rollback");
    q(&db, &mut importer, "begin");
    q(&db, &mut importer, &format!("set transaction snapshot '{id}'"));
    assert_eq!(fails(&db, &mut importer, "select nextval('nope')"), "25006");
    q(&db, &mut importer, "rollback");
    q(&db, &mut importer, "begin");
    q(&db, &mut importer, &format!("set transaction snapshot '{id}'"));
    q(&db, &mut importer, "prepare p(int) as select $1");
    assert_eq!(fails(&db, &mut importer, "execute p(nextval('nope'))"), "25006");
    q(&db, &mut importer, "rollback");
    // a write naming a sequence is refused before it reserves values
    let mut other = Session::new(MAIN);
    q(&db, &mut other, "create sequence s");
    q(&db, &mut importer, "begin");
    q(&db, &mut importer, &format!("set transaction snapshot '{id}'"));
    assert_eq!(fails(&db, &mut importer, "insert into t values (nextval('s'), 'x')"), "25006");
    q(&db, &mut importer, "rollback");
    assert_eq!(q(&db, &mut other, "select nextval('s')"), ["1"]);
    assert_eq!(q(&db, &mut other, "select id from t order by id"), ["1", "3"]);
}

#[test]
fn snapshot_errors_are_postgres_s() {
    let db = Db::new();
    let (mut exporter, id) = exported(&db);
    let mut s = Session::new(MAIN);
    // outside a transaction, after a query, an id nobody exported
    assert_eq!(fails(&db, &mut s, &format!("set transaction snapshot '{id}'")), "25P01");
    q(&db, &mut s, "begin");
    q(&db, &mut s, "select 1");
    assert_eq!(fails(&db, &mut s, &format!("set transaction snapshot '{id}'")), "25001");
    q(&db, &mut s, "rollback");
    // after a savepoint: ROLLBACK TO would bring back the database as it is now
    q(&db, &mut s, "begin");
    q(&db, &mut s, "savepoint p");
    assert_eq!(fails(&db, &mut s, &format!("set transaction snapshot '{id}'")), "25001");
    q(&db, &mut s, "rollback");
    q(&db, &mut s, "begin");
    assert_eq!(fails(&db, &mut s, "set transaction snapshot '00000000-00000000-0'"), "22023");
    q(&db, &mut s, "rollback");
    // only a transaction in the database it was exported in imports it
    q(&db, &mut s, "create database other");
    let mut elsewhere = Session::new("other");
    q(&db, &mut elsewhere, "begin");
    assert_eq!(fails(&db, &mut elsewhere, &format!("set transaction snapshot '{id}'")), "22023");
    q(&db, &mut elsewhere, "rollback");
    // once the exporting transaction ends, its snapshot does too
    q(&db, &mut exporter, "commit");
    q(&db, &mut s, "begin");
    assert_eq!(fails(&db, &mut s, &format!("set transaction snapshot '{id}'")), "22023");
    q(&db, &mut s, "rollback");
    // outside a transaction it gives an id, and the snapshot ends with the statement
    let id = q(&db, &mut s, "select pg_export_snapshot()").remove(0);
    q(&db, &mut s, "begin");
    assert_eq!(fails(&db, &mut s, &format!("set transaction snapshot '{id}'")), "22023");
    q(&db, &mut s, "rollback");
}

/// The importing transaction's fork is brought to the snapshot by ordinary logged writes, and its
/// COMMIT drops it: replaying the log after a restart ends with the same database.
#[test]
fn a_restart_after_an_import_keeps_the_database() {
    let dir = std::env::temp_dir().join(format!("chronos-test-{}-snapshot", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let (mut exporter, id) = exported(&db);
        let mut importer = Session::new(MAIN);
        q(&db, &mut importer, "begin");
        q(&db, &mut importer, &format!("set transaction snapshot '{id}'"));
        assert_eq!(q(&db, &mut importer, "select id from t order by id"), ["1"]);
        q(&db, &mut importer, "commit");
        q(&db, &mut exporter, "commit");
    }
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(
        q(&db, &mut s, "select id, v from t order by id"),
        ["1|before", "2|exporter's own", "3|committed after"]
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
