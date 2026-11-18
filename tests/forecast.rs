//! `FORECAST CONFLICTS [FOR WORLD w]`: before anyone merges, which rows two live worlds of one parent both changed
//! (write-write), which rows a world's parent changed since it forked that the world changed too, and, for a world
//! that checks its reads, rows it read that a sibling or the parent changed (read-write). A row both changed only in
//! delta columns (`SET MERGE AS DELTA`) is marked combinable: its merges won't conflict.

use chronos::sql::{Output, Value};
use chronos::{Db, MAIN};

fn q(db: &Db, world: &str, sql: &str) -> Output {
    chronos::sql::run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap()
}

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn setup(db: &Db) {
    q(db, MAIN, "create table inv (sku int primary key, qty int, note text)");
    q(db, MAIN, "insert into inv values (1, 100, 'a'), (2, 50, 'b'), (3, 10, 'c')");
}

#[test]
fn forecast_reports_write_write_between_siblings() {
    let db = Db::new();
    setup(&db);
    db.fork(MAIN, "a").unwrap();
    db.fork(MAIN, "b").unwrap();
    db.fork(MAIN, "c").unwrap();
    q(&db, "a", "update inv set note = 'from a' where sku = 1");
    q(&db, "a", "update inv set note = 'from a' where sku = 3");
    q(&db, "b", "update inv set note = 'from b' where sku = 1");
    q(&db, "c", "update inv set note = 'from c' where sku = 2"); // nobody else's row
    let out = q(&db, MAIN, "forecast conflicts");
    let names: Vec<&str> = out.columns.as_ref().unwrap().iter().map(|c| c.0.as_str()).collect();
    assert_eq!(names, ["world_a", "world_b", "kind", "table", "key", "combinable"]);
    assert_eq!(rows(&out), ["a|b|write-write|inv|1|false"]);
    // the parent changing a row a world changed since its fork shows too
    q(&db, MAIN, "update inv set note = 'main' where sku = 3");
    assert_eq!(
        rows(&q(&db, MAIN, "forecast conflicts for world a")),
        ["a|b|write-write|inv|1|false", "a|main|write-write|inv|3|false"]
    );
    // only a's are shown for a; c's alone has none
    assert!(rows(&q(&db, MAIN, "forecast conflicts for world c")).is_empty());
    // nothing changed by forecasting: a and b still merge as before (b then conflicts, as forecast)
    q(&db, MAIN, "merge world b");
    assert_eq!(chronos::sql::run(&db, MAIN, "merge world a", &[]).unwrap_err().code, "40001");
}

#[test]
fn forecast_marks_delta_only_overlap_combinable() {
    let db = Db::new();
    setup(&db);
    q(&db, MAIN, "alter table inv alter column qty set merge as delta");
    db.fork(MAIN, "a").unwrap();
    db.fork(MAIN, "b").unwrap();
    q(&db, "a", "update inv set qty = qty - 1 where sku = 1");
    q(&db, "b", "update inv set qty = qty - 2 where sku = 1");
    q(&db, "a", "update inv set qty = qty - 1, note = 'x' where sku = 2");
    q(&db, "b", "update inv set qty = qty - 2, note = 'y' where sku = 2");
    assert_eq!(
        rows(&q(&db, MAIN, "forecast conflicts")),
        ["a|b|write-write|inv|1|true", "a|b|write-write|inv|2|false"]
    );
}

#[test]
fn forecast_reports_reads_a_sibling_changed() {
    let db = Db::new();
    setup(&db);
    q(&db, MAIN, "create world reader with (check_reads = true)");
    db.fork(MAIN, "writer").unwrap();
    q(&db, "reader", "select qty from inv where sku = 3");
    q(&db, "reader", "update inv set note = 'seen' where sku = 1");
    q(&db, "writer", "update inv set qty = 0 where sku = 3");
    assert_eq!(rows(&q(&db, MAIN, "forecast conflicts for world reader")), ["reader|writer|read-write|inv|3|false"]);
}
