//! Column merge semantics: `ALTER TABLE t ALTER COLUMN c SET MERGE AS DELTA [ESCROW (MIN n [, MAX m])]`. Two worlds
//! that each changed a delta column of one row combine at the merge as the parent's value plus the world's own change
//! (parent + (ours - base)), so two agents each taking stock both land; an escrow bound refuses a combined value past
//! it (23514), naming the row. Only numbers merge so (0A000 for any other column), and the setting is part of the
//! table's definition: it forks and merges with it, and `SET MERGE AS DEFAULT` takes it away.

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

fn fails(db: &Db, world: &str, sql: &str) -> chronos::sql::SqlError {
    match chronos::sql::run(db, world, sql, &[]) {
        Ok(_) => panic!("{sql}: went through"),
        Err(e) => e,
    }
}

fn setup(db: &Db) {
    q(db, MAIN, "create table inv (sku int primary key, qty int, price numeric(10, 2), note text)");
    q(db, MAIN, "insert into inv values (1, 100, 10.00, 'a'), (2, 50, 5.50, 'b')");
}

#[test]
fn delta_columns_combine_without_conflict() {
    let db = Db::new();
    setup(&db);
    q(&db, MAIN, "alter table inv alter column qty set merge as delta");
    q(&db, MAIN, "alter table inv alter column price set merge as delta");
    for w in ["a", "b", "c"] {
        db.fork(MAIN, w).unwrap();
    }
    q(&db, "a", "update inv set qty = qty - 5, price = price + 0.25 where sku = 1");
    // b also changes a column that isn't delta, which a alone didn't touch: it still combines
    q(&db, "b", "update inv set qty = qty - 7, note = 'counted' where sku = 1");
    q(&db, "c", "update inv set qty = qty + 30, price = price - 1.10 where sku = 1");
    q(&db, MAIN, "merge world a");
    q(&db, MAIN, "merge world b");
    q(&db, MAIN, "merge world c");
    assert_eq!(rows(&q(&db, MAIN, "select qty, price, note from inv where sku = 1")), ["118|9.15|counted"]);

    // a column both changed that isn't delta: a conflict as before, and nothing merged
    db.fork(MAIN, "d").unwrap();
    db.fork(MAIN, "e").unwrap();
    q(&db, "d", "update inv set qty = qty - 1, note = 'd' where sku = 2");
    q(&db, "e", "update inv set qty = qty - 1, note = 'e' where sku = 2");
    q(&db, MAIN, "merge world d");
    let e = fails(&db, MAIN, "merge world e");
    assert_eq!(e.code, "40001", "{}", e.message);
    assert_eq!(rows(&q(&db, MAIN, "select qty, note from inv where sku = 2")), ["49|d"]);

    // an insert, or a delete, of the same row on both sides still conflicts
    db.fork(MAIN, "f").unwrap();
    db.fork(MAIN, "g").unwrap();
    q(&db, "f", "delete from inv where sku = 1");
    q(&db, "g", "update inv set qty = qty - 1 where sku = 1");
    q(&db, MAIN, "merge world f");
    assert_eq!(fails(&db, MAIN, "merge world g").code, "40001");
}

#[test]
fn escrow_min_refuses_overdraw() {
    let db = Db::new();
    setup(&db);
    q(&db, MAIN, "alter table inv alter column qty set merge as delta escrow (min 0)");
    q(&db, MAIN, "update inv set qty = 10 where sku = 1");
    db.fork(MAIN, "a").unwrap();
    db.fork(MAIN, "b").unwrap();
    db.fork(MAIN, "c").unwrap();
    q(&db, "a", "update inv set qty = qty - 6 where sku = 1");
    q(&db, "b", "update inv set qty = qty - 6 where sku = 1");
    q(&db, "c", "update inv set qty = qty - 4 where sku = 1");
    q(&db, MAIN, "merge world a");
    let e = fails(&db, MAIN, "merge world b");
    assert_eq!(e.code, "23514", "{}", e.message);
    assert!(e.message.contains("inv/1") && e.message.contains("qty"), "{}", e.message);
    assert_eq!(rows(&q(&db, MAIN, "select qty from inv where sku = 1")), ["4"]);
    // b is still there, unmerged; c's -4 lands exactly on the bound
    assert!(db.worlds().iter().any(|w| w.name == "b"));
    q(&db, MAIN, "merge world c");
    assert_eq!(rows(&q(&db, MAIN, "select qty from inv where sku = 1")), ["0"]);

    // and a MAX bound
    q(&db, MAIN, "alter table inv alter column qty set merge as delta escrow (min 0, max 20)");
    db.fork(MAIN, "d").unwrap();
    db.fork(MAIN, "e").unwrap();
    q(&db, "d", "update inv set qty = qty + 15 where sku = 1");
    q(&db, "e", "update inv set qty = qty + 15 where sku = 1");
    q(&db, MAIN, "merge world d");
    assert_eq!(fails(&db, MAIN, "merge world e").code, "23514");
    assert_eq!(rows(&q(&db, MAIN, "select qty from inv where sku = 1")), ["15"]);
}

#[test]
fn delta_on_text_column_refused() {
    let db = Db::new();
    setup(&db);
    let e = fails(&db, MAIN, "alter table inv alter column note set merge as delta");
    assert_eq!(e.code, "0A000", "{}", e.message);
    assert!(e.message.contains("note"), "{}", e.message);
    // a bound the wrong way round, or on a column that isn't one
    assert_eq!(
        fails(&db, MAIN, "alter table inv alter column qty set merge as delta escrow (min 5, max 1)").code,
        "22023"
    );
    assert_eq!(fails(&db, MAIN, "alter table inv alter column nope set merge as delta").code, "42703");
    // a number column takes it, and DEFAULT takes it away: both changing it conflicts again
    q(&db, MAIN, "alter table inv alter column qty set merge as delta");
    q(&db, MAIN, "alter table inv alter column qty set merge as default");
    db.fork(MAIN, "a").unwrap();
    db.fork(MAIN, "b").unwrap();
    q(&db, "a", "update inv set qty = qty - 1 where sku = 1");
    q(&db, "b", "update inv set qty = qty - 1 where sku = 1");
    q(&db, MAIN, "merge world a");
    assert_eq!(fails(&db, MAIN, "merge world b").code, "40001");
}

#[test]
fn delta_definition_forks_and_merges_with_schema() {
    let db = Db::new();
    setup(&db);
    // declared in a world: main doesn't have it until the world merges
    db.fork(MAIN, "schema").unwrap();
    q(&db, "schema", "alter table inv alter column qty set merge as delta escrow (min 0)");
    db.fork(MAIN, "a").unwrap();
    db.fork(MAIN, "b").unwrap();
    q(&db, "a", "update inv set qty = qty - 1 where sku = 2");
    q(&db, "b", "update inv set qty = qty - 1 where sku = 2");
    q(&db, MAIN, "merge world a");
    assert_eq!(fails(&db, MAIN, "merge world b").code, "40001", "main has no delta column yet");
    db.discard("b").unwrap();
    q(&db, MAIN, "merge world schema");
    // worlds forked since have it, and so do their forks
    db.fork(MAIN, "c").unwrap();
    db.fork("c", "c1").unwrap();
    db.fork("c", "c2").unwrap();
    q(&db, "c1", "update inv set qty = qty - 10 where sku = 2");
    q(&db, "c2", "update inv set qty = qty - 20 where sku = 2");
    q(&db, "c", "merge world c1");
    q(&db, "c", "merge world c2");
    assert_eq!(rows(&q(&db, "c", "select qty from inv where sku = 2")), ["19"]);
    q(&db, MAIN, "merge world c");
    assert_eq!(rows(&q(&db, MAIN, "select qty from inv where sku = 2")), ["19"]);
    // and its escrow bound came with it
    db.fork(MAIN, "d").unwrap();
    db.fork(MAIN, "e").unwrap();
    q(&db, "d", "update inv set qty = qty - 15 where sku = 2");
    q(&db, "e", "update inv set qty = qty - 15 where sku = 2");
    q(&db, MAIN, "merge world d");
    assert_eq!(fails(&db, MAIN, "merge world e").code, "23514");
}
