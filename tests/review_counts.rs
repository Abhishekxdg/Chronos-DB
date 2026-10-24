//! The review `count` path derives its per-table totals from the kinds (delete/update/insert) of
//! a single diff walk instead of walking the change a second time with `diff_count`. These tests
//! pin the equivalence that relies on: for every table, the kinds sum to the `diff_count` total,
//! across inserts, updates and deletes.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

/// A world whose diff against main has inserts, updates and deletes across two tables.
fn mixed_world(db: &Db) -> Session {
    run(
        db,
        MAIN,
        "create table a (id int primary key, v text);
         create table b (id int primary key, v text);
         insert into a values (1, 'one'), (2, 'two'), (3, 'three');
         insert into b values (1, 'x'), (2, 'y')",
        &[],
    )
    .unwrap();
    let mut s = Session::new(MAIN);
    s.run(db, "create world w; use world w", &[]).unwrap();
    s.run(
        db,
        "update a set v = 'ONE' where id = 1;   -- update
         delete from a where id = 2;            -- delete
         insert into a values (4, 'four');      -- insert
         insert into b values (3, 'z');         -- insert
         delete from b where id = 1",
        &[],
    )
    .unwrap();
    s
}

#[test]
fn kinds_totals_equal_the_diff_count_per_table() {
    let db = Db::new();
    let _s = mixed_world(&db);
    let counts = db.diff_count("w", None).unwrap(); // per-table totals (the old second walk)
    let kinds = db.diff_kinds("w", None).unwrap(); // per-table delete/update/insert (one walk)

    // every table that changed appears in both, with the kinds summing to the total
    assert_eq!(counts.keys().collect::<Vec<_>>(), kinds.keys().collect::<Vec<_>>());
    for (table, c) in &kinds {
        assert_eq!(c.total(), counts[table], "table {table}: kinds {c:?} must sum to diff_count {}", counts[table]);
    }
    // and the grand totals match
    let from_kinds: usize = kinds.values().map(|c| c.total()).sum();
    let from_count: usize = counts.values().sum();
    assert_eq!(from_kinds, from_count);
}

#[test]
fn the_kinds_are_what_was_done() {
    let db = Db::new();
    let _s = mixed_world(&db);
    let kinds = db.diff_kinds("w", None).unwrap();
    let a = &kinds["a"];
    assert_eq!((a.deleted, a.updated, a.inserted), (1, 1, 1), "a: {a:?}");
    let b = &kinds["b"];
    assert_eq!((b.deleted, b.updated, b.inserted), (1, 0, 1), "b: {b:?}");
    assert_eq!(a.total() + b.total(), db.diff_count("w", None).unwrap().values().sum::<usize>());
}
