//! CHECK constraints: every write and every merge keeps them, as Postgres does.

use chronos::sql::{Output, Value, run};
use chronos::{Db, MAIN, Resolve};

fn on(db: &Db, branch: &str, q: &str) -> Output {
    run(db, branch, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap()
}

fn sql(db: &Db, q: &str) -> Output {
    on(db, MAIN, q)
}

fn fails(db: &Db, q: &str) -> (&'static str, String) {
    let e = run(db, MAIN, q, &[]).expect_err(q);
    (e.code, e.message)
}

fn count(db: &Db, q: &str) -> i64 {
    match sql(db, q).rows[0][0] {
        Value::Int(n) => n,
        ref v => panic!("{q}: {v:?}"),
    }
}

fn items() -> Db {
    let db = Db::new();
    sql(
        &db,
        "create table items (
            id int primary key,
            name text check (length(name) > 0),
            price int constraint price_positive check (price >= 0),
            status text default 'new',
            lo int, hi int,
            check (lo <= hi),
            check (status in ('new', 'it''s done'))
        )",
    );
    db
}

#[test]
fn rows_must_pass_their_checks() {
    let db = items();
    sql(&db, "insert into items (id, name, price, lo, hi) values (1, 'a', 5, 1, 2)");
    let (code, msg) = fails(&db, "insert into items (id, name, price) values (2, '', 5)");
    assert_eq!(code, "23514");
    assert!(msg.contains("items_name_check"), "{msg}"); // Postgres's default name
    assert!(fails(&db, "insert into items (id, name, price) values (2, 'b', -1)").1.contains("price_positive"));
    assert!(fails(&db, "insert into items (id, lo, hi) values (2, 5, 1)").1.contains("items_check"));
    assert!(fails(&db, "insert into items (id, status) values (2, 'lost')").1.contains("items_status_check")); // a table check that reads one column is named for it, as Postgres names it
    // null makes a check neither true nor false, so it passes
    sql(&db, "insert into items (id, name, price, lo, hi, status) values (3, null, null, null, 1, 'it''s done')");
    assert_eq!(fails(&db, "update items set price = -5 where id = 1").0, "23514");
    assert_eq!(fails(&db, "update items set lo = 9").0, "23514", "one row failing stops the whole statement");
    assert_eq!(count(&db, "select count(*) from items where lo = 9"), 0);
    // JSON writes keep them too
    assert!(db.put(MAIN, "items/7", r#"{"name": "", "price": 1}"#).is_err());
    db.put(MAIN, "items/7", r#"{"name": "x", "price": 1}"#).unwrap();
    // a check with a subquery isn't allowed, as in Postgres
    assert_eq!(fails(&db, "create table bad (id int primary key, n int check (n > (select 1)))").0, "0A000");
    assert_eq!(fails(&db, "create table bad (id int primary key, n int check (nope > 1))").0, "42703");
}

#[test]
fn alter_table_and_checks() {
    let db = items();
    sql(&db, "insert into items (id, name, price, lo, hi) values (1, 'a', 5, 1, 2), (2, 'b', 50, 1, 2)");
    // adding a check tests the rows already there
    assert_eq!(fails(&db, "alter table items add constraint cheap check (price < 10)").0, "23514");
    sql(&db, "delete from items where id = 2");
    sql(&db, "alter table items add constraint cheap check (price < 10)");
    assert_eq!(fails(&db, "insert into items (id, price) values (3, 11)").0, "23514");
    assert_eq!(fails(&db, "alter table items add constraint cheap check (price < 20)").0, "42710");
    sql(&db, "alter table items drop constraint cheap");
    sql(&db, "insert into items (id, price) values (3, 11)");
    // a renamed column is renamed inside its checks
    sql(&db, "alter table items rename column price to cost");
    assert_eq!(fails(&db, "update items set cost = -1 where id = 3").0, "23514");
    // a dropped column takes its checks with it
    sql(&db, "alter table items drop column lo");
    sql(&db, "insert into items (id, hi) values (4, -100)");
    assert_eq!(fails(&db, "alter table items drop constraint items_check").0, "42704", "it went with lo");
}

#[test]
fn merges_keep_checks() {
    let db = items();
    sql(&db, "insert into items (id, price) values (1, 5)");
    // the branch adds a check; meanwhile main adds a row that breaks it
    db.fork(MAIN, "x").unwrap();
    on(&db, "x", "alter table items add constraint small check (price < 100)");
    sql(&db, "insert into items (id, price) values (2, 500)");
    let e = db.merge("x", Resolve::Fail).expect_err("the merged table would break its check");
    assert!(e.to_string().contains("small"), "{e}");
    assert_eq!(count(&db, "select count(*) from items"), 2, "nothing was merged");
    db.discard("x").unwrap();
    // an ordinary merge of rows that pass
    db.fork(MAIN, "y").unwrap();
    on(&db, "y", "insert into items (id, price) values (3, 7)");
    db.merge("y", Resolve::Fail).unwrap();
    assert_eq!(count(&db, "select count(*) from items"), 3);
}

#[test]
fn checks_survive_a_reopen() {
    let dir = std::env::temp_dir().join(format!("chronos-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table t (id int primary key, n int check (n between 1 and 10), s text check (s <> 'a\"b'))");
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(fails(&db, "insert into t values (1, 11, 'x')").0, "23514");
    assert_eq!(fails(&db, "insert into t values (1, 5, 'a\"b')").0, "23514");
    sql(&db, "insert into t values (1, 5, 'x')");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
