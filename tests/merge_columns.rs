//! Both sides of a merge changing one table's columns: different columns merge, each side's rows
//! read through the combined definition (the world's new columns go after the parent's, with ids
//! past the parent's own); the same column changed by both, or two columns of one name, is still
//! refused. BranchBench's S4: a dev world and the spine each add a column to `customer`, and the
//! dev world moves its column out of the way of the spine's before it rebases.

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

fn names(o: &Output) -> Vec<String> {
    o.columns.as_ref().unwrap().iter().map(|c| c.0.clone()).collect()
}

/// main and world x each add a column and write rows with it.
fn setup(db: &Db) {
    q(db, MAIN, "create table customer (id int primary key, name text)");
    q(db, MAIN, "insert into customer values (1, 'one'), (2, 'two'), (3, 'three')");
    db.fork(MAIN, "x").unwrap();
    q(db, "x", "alter table customer add column loyalty varchar(8) default 'none'");
    q(db, "x", "update customer set loyalty = 'gold' where id = 2");
    q(db, "x", "insert into customer values (5, 'five', 'silver')");
    q(db, MAIN, "alter table customer add column tier int");
    q(db, MAIN, "update customer set tier = 1 where id = 1");
    q(db, MAIN, "insert into customer values (4, 'four', 2)");
}

const MERGED: [&str; 5] =
    ["1|one|1|none", "2|two|null|gold", "3|three|null|none", "4|four|2|none", "5|five|null|silver"];

#[test]
fn different_columns_on_each_side_merge() {
    let db = Db::new();
    setup(&db);
    let plan = rows(&q(&db, MAIN, "merge world x dry run"));
    assert!(!plan.iter().any(|r| r.contains("conflict") || r.contains("blocked")), "{plan:?}");
    q(&db, MAIN, "merge world x");
    let all = q(&db, MAIN, "select * from customer order by id");
    assert_eq!(names(&all), ["id", "name", "tier", "loyalty"]);
    assert_eq!(rows(&all), MERGED);
    // and main goes on with both columns
    q(&db, MAIN, "insert into customer values (6, 'six', 3, 'bronze')");
    q(&db, MAIN, "update customer set loyalty = 'gold+', tier = 9 where id = 5");
    assert_eq!(
        rows(&q(&db, MAIN, "select id, tier, loyalty from customer where id >= 5 order by id")),
        ["5|9|gold+", "6|3|bronze"]
    );
}

#[test]
fn a_rebase_takes_the_parents_column_too() {
    let db = Db::new();
    setup(&db);
    q(&db, MAIN, "rebase world x");
    let all = q(&db, "x", "select * from customer order by id");
    assert_eq!(names(&all), ["id", "name", "tier", "loyalty"]);
    assert_eq!(rows(&all), MERGED);
    // main didn't change; x writes on and merges
    assert_eq!(names(&q(&db, MAIN, "select * from customer")), ["id", "name", "tier"]);
    q(&db, "x", "update customer set tier = 7 where id = 3");
    q(&db, MAIN, "merge world x");
    assert_eq!(rows(&q(&db, MAIN, "select id, tier, loyalty from customer where id = 3")), ["3|7|none"]);
}

#[test]
fn the_same_column_both_ways_is_still_refused() {
    let db = Db::new();
    q(&db, MAIN, "create table t (id int primary key, a text, n int)");
    q(&db, MAIN, "insert into t values (1, 'one', 1)");
    db.fork(MAIN, "x").unwrap();
    q(&db, "x", "alter table t alter column n type bigint");
    q(&db, MAIN, "alter table t drop column n");
    let e = chronos::sql::run(&db, MAIN, "merge world x", &[]).unwrap_err();
    assert!(e.code == "40001" && e.message.contains("both sides changed table t itself"), "{}", e.message);
    // and a column the world dropped, which the parent changed
    db.fork(MAIN, "y").unwrap();
    q(&db, "y", "alter table t drop column a");
    q(&db, MAIN, "alter table t alter column a type varchar(10)");
    let e = chronos::sql::run(&db, MAIN, "merge world y", &[]).unwrap_err();
    assert!(e.code == "40001", "{}", e.message);
}

/// S4's path: the dev world added c_tier as text; the spine adds c_tier as an int, so the rebase
/// is refused; the world moves its column to c_loyalty_tier (add, copy, drop) and rebases again,
/// which now takes the spine's c_tier beside its own column.
#[test]
fn moving_a_column_out_of_the_way_lets_the_rebase_through() {
    let dir = std::env::temp_dir().join(format!("chronos-merge-columns-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        q(&db, MAIN, "create table customer (id int primary key, name text)");
        q(&db, MAIN, "insert into customer values (1, 'one'), (2, 'two')");
        db.checkpoint().unwrap(); // everything after this is replayed from the log at the restart
        db.fork(MAIN, "dev").unwrap();
        q(&db, "dev", "alter table customer add column c_tier varchar(8)");
        q(&db, "dev", "update customer set c_tier = 'gold' where id = 1");
        q(&db, MAIN, "alter table customer add column c_tier int");
        q(&db, MAIN, "update customer set c_tier = 3 where id = 2");
        q(&db, MAIN, "insert into customer values (9, 'nine', 4)");
        let e = chronos::sql::run(&db, MAIN, "rebase world dev", &[]).unwrap_err();
        assert!(e.code == "40001" && e.message.contains("both sides changed table customer itself"), "{}", e.message);
        q(&db, "dev", "alter table customer add column c_loyalty_tier varchar(8)");
        q(&db, "dev", "update customer set c_loyalty_tier = c_tier where c_tier is not null");
        q(&db, "dev", "alter table customer drop column c_tier");
        q(&db, MAIN, "rebase world dev");
        let all = q(&db, "dev", "select * from customer order by id");
        assert_eq!(names(&all), ["id", "name", "c_tier", "c_loyalty_tier"]);
        assert_eq!(rows(&all), ["1|one|null|gold", "2|two|3|null", "9|nine|4|null"]);
        // Postgres's attnums: c_tier 3, the world's dropped column 4, c_loyalty_tier 5
        assert_eq!(
            rows(&q(
                &db,
                "dev",
                "select attname, attnum from pg_attribute where attrelid = 'customer'::regclass and attnum > 0 order by attnum"
            )),
            ["id|1", "name|2", "c_tier|3", "........pg.dropped.4........|4", "c_loyalty_tier|5"]
        );
        q(&db, MAIN, "merge world dev");
        q(&db, MAIN, "insert into customer values (10, 'ten', 5, 'tin')");
    }
    // reopened, the rebase and the merge come back from the log: the rows read the same
    let db = Db::open(&dir).unwrap();
    assert_eq!(
        rows(&q(&db, MAIN, "select * from customer order by id")),
        ["1|one|null|gold", "2|two|3|null", "9|nine|4|null", "10|ten|5|tin"]
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
