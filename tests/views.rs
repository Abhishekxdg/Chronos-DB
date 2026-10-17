//! Views, materialized views, sequences, TRUNCATE and NOTIFY as objects of a world: in forks,
//! through merges, read AS OF a moment, and after the database is reopened.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};
use std::time::Duration;

fn rows(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let o = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    o.rows
        .iter()
        .map(|r| r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect())
        .collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    rows(db, s, q)[0][0].clone()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> String {
    s.run(db, q, &[]).err().unwrap_or_else(|| panic!("{q} should fail")).code.to_string()
}

fn run(db: &Db, s: &mut Session, q: &str) {
    s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-views-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn views_read_write_and_depend() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table orders (id int primary key, customer text, total int, paid boolean)");
    run(&db, s, "insert into orders values (1, 'ada', 10, true), (2, 'bo', 20, false), (3, 'ada', 30, true)");
    run(&db, s, "create view paid as select id, customer, total from orders where paid");
    run(&db, s, "create view big (who, amount) as select customer, total from paid where total > 15");
    assert_eq!(rows(&db, s, "select * from big"), [["ada", "30"]]);
    assert_eq!(one(&db, s, "select sum(p.total) from paid p join orders o using (id)"), "40");
    assert_eq!(one(&db, s, "select count(*) from orders where id in (select id from paid)"), "2");
    // simple views write through to their table, under their WHERE
    run(&db, s, "update paid set total = total + 1 where customer = 'ada'");
    assert_eq!(rows(&db, s, "select id, total from orders order by id"), [["1", "11"], ["2", "20"], ["3", "31"]]);
    run(&db, s, "insert into paid (id, customer, total) values (4, 'cy', 5)");
    assert_eq!(rows(&db, s, "select id, paid from orders where id = 4"), [["4", "null"]]);
    assert_eq!(rows(&db, s, "delete from paid where id = 1 returning *"), [["1", "ada", "11"]]);
    run(&db, s, "update big set amount = amount + 1"); // through two views: big's amount is paid's total
    assert_eq!(one(&db, s, "select total from orders where id = 3"), "32");
    assert_eq!(
        fails(&db, s, "create view calc as select id, total * 2 as dbl from orders; update calc set dbl = 1"),
        "0A000"
    );
    assert_eq!(
        fails(
            &db,
            s,
            "create view counts as select customer, count(*) from orders group by customer; insert into counts values ('x', 1)"
        ),
        "55000"
    );
    // names are shared with tables; dependencies hold
    assert_eq!(fails(&db, s, "create table paid (id int)"), "42P07");
    assert_eq!(fails(&db, s, "drop table orders"), "2BP01");
    assert_eq!(fails(&db, s, "drop view paid"), "2BP01");
    assert_eq!(fails(&db, s, "drop table paid"), "42809");
    assert_eq!(fails(&db, s, "create or replace view paid as select id from orders"), "42P16");
    run(&db, s, "create or replace view paid as select id, customer, total, paid from orders where paid");
    run(&db, s, "drop table orders cascade");
    assert_eq!(fails(&db, s, "select * from big"), "42P01");
    assert_eq!(fails(&db, s, "select * from counts"), "42P01");
}

#[test]
fn views_in_worlds_and_time() {
    let dir = tmp("worlds");
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table t (id int primary key, v int)");
    run(&db, s, "insert into t values (1, 10), (2, 20)");
    run(&db, s, "create view v as select id, v * 2 as twice from t");
    std::thread::sleep(Duration::from_millis(5));
    let then = one(&db, s, "select now()::text");
    std::thread::sleep(Duration::from_millis(5));
    run(&db, s, "create or replace view v as select id, v * 3 as twice from t");
    run(&db, s, "update t set v = 100 where id = 1");
    assert_eq!(one(&db, s, "select twice from v where id = 1"), "300");
    // AS OF reads the view's definition and its table as they were
    assert_eq!(one(&db, s, &format!("select twice from v as of '{then}' where id = 1")), "20");
    // a fork has its own views; a merge brings them back
    run(&db, s, "create world w");
    run(&db, s, "switch world w");
    run(&db, s, "create view high as select id from t where v > 50");
    assert_eq!(rows(&db, s, "select * from high"), [["1"]]);
    run(&db, s, "switch world main");
    assert_eq!(fails(&db, s, "select * from high"), "42P01");
    let diff = rows(&db, s, "diff world w");
    assert_eq!((diff[0][0].as_str(), diff[0][2].as_str()), ("high", "create view"));
    assert!(one(&db, s, "diff world w as sql").starts_with("create or replace view high as select"));
    run(&db, s, "merge world w");
    assert_eq!(rows(&db, s, "select * from high"), [["1"]]);
    drop(db);
    // after reopening
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    assert_eq!(rows(&db, s, "select * from high"), [["1"]]);
    assert_eq!(one(&db, s, "select twice from v where id = 2"), "60");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn materialized_views() {
    let dir = tmp("matviews");
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table t (id int primary key, k text, v int)");
    run(&db, s, "insert into t values (1, 'a', 1), (2, 'a', 2), (3, 'b', 5)");
    assert_eq!(
        s.run(&db, "create materialized view sums as select k, sum(v) as total from t group by k order by k", &[])
            .unwrap()[0]
            .tag,
        "SELECT 2"
    );
    run(&db, s, "insert into t values (4, 'b', 10)");
    assert_eq!(rows(&db, s, "select * from sums"), [["a", "3"], ["b", "5"]], "a snapshot, until REFRESH");
    assert_eq!(fails(&db, s, "insert into sums values ('c', 1)"), "42809");
    assert_eq!(fails(&db, s, "drop table t"), "2BP01");
    run(&db, s, "create index on sums (total)");
    std::thread::sleep(Duration::from_millis(5));
    let then = one(&db, s, "select now()::text");
    std::thread::sleep(Duration::from_millis(5));
    run(&db, s, "create world w");
    run(&db, s, "switch world w");
    run(&db, s, "refresh materialized view sums");
    assert_eq!(rows(&db, s, "select * from sums"), [["a", "3"], ["b", "15"]]);
    assert_eq!(rows(&db, s, "select k from sums where total = 15"), [["b"]], "its index follows a REFRESH");
    assert_eq!(rows(&db, s, "diff as sql"), [["refresh materialized view sums;"]], "only the row that changed");
    run(&db, s, "switch world main");
    assert_eq!(rows(&db, s, "select total from sums where k = 'b'"), [["5"]]);
    run(&db, s, "merge world w");
    assert_eq!(rows(&db, s, "select total from sums where k = 'b'"), [["15"]]);
    assert_eq!(rows(&db, s, &format!("select total from sums as of '{then}' where k = 'b'")), [["5"]]);
    run(&db, s, "create world again");
    run(&db, s, "switch world again");
    run(&db, s, "refresh materialized view sums");
    assert!(rows(&db, s, "diff").is_empty(), "the same rows: nothing changed");
    run(&db, s, "switch world main");
    run(&db, s, "create materialized view later as select id from t with no data");
    assert_eq!(fails(&db, s, "select * from later"), "55000");
    run(&db, s, "refresh materialized view later");
    assert_eq!(one(&db, s, "select count(*) from later"), "4");
    drop(db);
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    assert_eq!(rows(&db, s, "select * from sums"), [["a", "3"], ["b", "15"]]);
    run(&db, s, "drop materialized view later");
    run(&db, s, "drop table t cascade");
    assert_eq!(fails(&db, s, "select * from sums"), "42P01");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sequences_are_shared_by_worlds() {
    let dir = tmp("sequences");
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create sequence ids start 100 increment by 10");
    assert_eq!(one(&db, s, "select nextval('ids')"), "100");
    assert_eq!(one(&db, s, "select nextval('ids')"), "110");
    assert_eq!(one(&db, s, "select currval('ids')"), "110");
    assert_eq!(one(&db, s, "select lastval()"), "110");
    run(&db, s, "create table t (id bigint primary key default nextval('ids'), name text)");
    run(&db, s, "insert into t (name) values ('a'), ('b')");
    run(&db, s, "insert into t values (nextval('ids'), 'c')");
    assert_eq!(rows(&db, s, "select id from t order by id"), [["120"], ["130"], ["140"]]);
    // a fork takes values from the same counter: no clash when it merges
    run(&db, s, "create world w");
    let w = &mut Session::new("w");
    run(&db, w, "insert into t (name) values ('from w')");
    run(&db, s, "insert into t (name) values ('from main')");
    run(&db, s, "merge world w");
    assert_eq!(one(&db, s, "select count(distinct id) from t"), "5");
    // the definition is the world's: AS OF, a session in the past can't take values
    std::thread::sleep(Duration::from_millis(5));
    let then = one(&db, s, "select now()::text");
    std::thread::sleep(Duration::from_millis(5));
    run(&db, s, "create sequence later");
    let past = &mut Session::new(&format!("main@{then}"));
    assert_eq!(fails(&db, past, "select nextval('ids')"), "25006");
    assert_eq!(rows(&db, past, "select sequence_name from information_schema.sequences"), [["ids"]]);
    run(&db, s, "drop sequence later");
    // setval and RESTART move it
    assert_eq!(one(&db, s, "select setval('ids', 500)"), "500");
    assert_eq!(one(&db, s, "select nextval('ids')"), "510");
    run(&db, s, "alter sequence ids restart with 7");
    assert_eq!(one(&db, s, "select nextval('ids')"), "7");
    run(&db, s, "alter sequence ids increment by 1");
    assert_eq!(one(&db, s, "select nextval('ids')"), "8");
    run(&db, s, "create sequence small as smallint maxvalue 3 cycle");
    assert_eq!(rows(&db, s, "select nextval('small') from generate_series(1, 5)"), [["1"], ["2"], ["3"], ["1"], ["2"]]);
    run(&db, s, "create sequence one maxvalue 2 start 2");
    run(&db, s, "select nextval('one')");
    assert_eq!(fails(&db, s, "select nextval('one')"), "2200H");
    assert_eq!(fails(&db, s, "select currval('nope')"), "42P01");
    assert_eq!(fails(&db, &mut Session::new(MAIN), "select currval('ids')"), "55000");
    // many values in one write: the session reserves more as it goes
    run(&db, s, "create table many (id bigint primary key, n int)");
    run(&db, s, "insert into many select nextval('ids'), g from generate_series(1, 300) g");
    assert_eq!(one(&db, s, "select count(distinct id) from many"), "300");
    drop(db);
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    let next: i64 = one(&db, s, "select nextval('ids')").parse().unwrap();
    assert!(next > 308, "after reopening, values go on: {next}");
    run(&db, s, "drop sequence ids, small, one");
    assert_eq!(fails(&db, s, "select nextval('ids')"), "42P01");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn truncate_and_aliases() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table p (id serial primary key, name text)");
    run(&db, s, "create table c (id int primary key, p int references p)");
    run(&db, s, "insert into p (name) values ('a'), ('b')");
    run(&db, s, "insert into c values (1, 1)");
    assert_eq!(fails(&db, s, "truncate p"), "0A000");
    run(&db, s, "truncate table p, c restart identity");
    assert_eq!(one(&db, s, "select count(*) from c"), "0");
    run(&db, s, "insert into p (name) values ('x')");
    assert_eq!(one(&db, s, "select id from p"), "1");
    run(&db, s, "insert into c values (1, 1)");
    run(&db, s, "truncate p cascade");
    assert_eq!(one(&db, s, "select count(*) from c"), "0");
    run(&db, s, "insert into p (name) values ('y'), ('z')");
    run(&db, s, "update p as x set name = x.name || '!' where x.name = 'y'");
    run(&db, s, "update p x set name = upper(x.name) where x.id = (select max(id) from p)");
    assert_eq!(rows(&db, s, "select name from p order by id"), [["y!"], ["Z"]]);
    run(&db, s, "delete from p x where x.name = 'Z'");
    assert_eq!(one(&db, s, "select count(*) from p"), "1");
}

#[test]
fn notify_reaches_listeners_of_the_world() {
    let db = Db::new();
    let (a, b, other) = (&mut Session::new(MAIN), &mut Session::new(MAIN), &mut Session::new(MAIN));
    run(&db, b, "listen jobs");
    run(&db, other, "create world elsewhere");
    run(&db, other, "switch world elsewhere");
    run(&db, other, "listen jobs");
    run(&db, a, "notify jobs, 'one'");
    assert_eq!(b.inbox.take().iter().map(|n| n.payload.as_str()).collect::<Vec<_>>(), ["one"]);
    // in a transaction: at COMMIT, each (channel, payload) once
    run(&db, a, "begin");
    run(&db, a, "notify jobs, 'two'; notify jobs, 'two'; select pg_notify('jobs', 'three')");
    assert!(b.inbox.take().is_empty(), "not before COMMIT");
    run(&db, a, "commit");
    assert_eq!(b.inbox.take().iter().map(|n| n.payload.as_str()).collect::<Vec<_>>(), ["two", "three"]);
    run(&db, a, "begin; notify jobs, 'no'; rollback");
    run(&db, a, "begin; savepoint s; notify jobs, 'dropped'; rollback to s; notify jobs, 'kept'; commit");
    assert_eq!(b.inbox.take().iter().map(|n| n.payload.as_str()).collect::<Vec<_>>(), ["kept"]);
    assert!(other.inbox.take().is_empty(), "another world's listeners don't hear main");
    run(&db, b, "unlisten *");
    run(&db, a, "notify jobs");
    assert!(b.inbox.take().is_empty());
    assert_eq!(fails(&db, a, &format!("notify jobs, '{}'", "x".repeat(8000))), "22023");
}

/// Serial and sequence counters are the database's, in no world: a world taking numbers writes
/// none of main's rows and leaves its version alone, and numbers stay unique across merges.
#[test]
fn counters_belong_to_no_world() {
    let dir = tmp("counters");
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table s (id serial primary key, v text)");
    run(&db, s, "create sequence ids start 100");
    run(&db, s, "insert into s (v) values ('m1')");
    for w in ["w1", "w2", "w3"] {
        run(&db, s, &format!("create world {w}"));
    }
    let before = (db.version(MAIN).unwrap(), rows(&db, s, "select id, v from s order by id"));
    for w in ["w1", "w2", "w3"] {
        let ws = &mut Session::new(w);
        // past a reservation's 64, so the counter is logged again from inside the worlds
        run(&db, ws, "insert into s (v) select 'x' from generate_series(1, 100)");
        run(&db, ws, "select nextval('ids')");
        run(&db, ws, "select setval('ids', 1000)");
    }
    assert_eq!((db.version(MAIN).unwrap(), rows(&db, s, "select id, v from s order by id")), before);
    assert!(db.prefix(MAIN, "_sonos_seq/").unwrap().is_empty(), "no counter rows in main");
    // the audit's case: main's next number skips what the worlds took (a gap, as after a rollback)
    run(&db, s, "insert into s (v) values ('m2')");
    let m2: i64 = one(&db, s, "select id from s where v = 'm2'").parse().unwrap();
    assert!(m2 > 1, "{m2}");
    assert_eq!(one(&db, s, "select nextval('ids')"), "1001", "setval in a world moved the shared counter");
    for w in ["w1", "w2", "w3"] {
        run(&db, s, &format!("merge world {w}"));
    }
    assert_eq!(one(&db, s, "select count(*) from s"), "302");
    assert_eq!(one(&db, s, "select count(distinct id) from s"), "302");
    // RESTART in a world changes the definition there, and the counter for every world
    run(&db, s, "create world r");
    let r = &mut Session::new("r");
    let v = db.version(MAIN).unwrap();
    run(&db, r, "alter sequence ids restart with 5");
    assert_eq!(db.version(MAIN).unwrap(), v);
    assert_eq!(one(&db, s, "select nextval('ids')"), "5");
    run(&db, r, "truncate s restart identity");
    assert_eq!(db.version(MAIN).unwrap(), v);
    assert_eq!(one(&db, r, "insert into s (v) values ('again') returning id"), "1");
    // time travel doesn't rewind counters: main put back as it was still takes new numbers
    run(&db, s, "create table u (id serial primary key, x int)");
    run(&db, s, "insert into u (x) values (0)");
    std::thread::sleep(Duration::from_millis(5));
    let then = one(&db, s, "select now()::text");
    std::thread::sleep(Duration::from_millis(5));
    run(&db, s, "insert into u (x) select g from generate_series(1, 70) g");
    run(&db, s, &format!("restore world main to '{then}'"));
    assert_eq!(one(&db, s, "select count(*) from u"), "1");
    let back = one(&db, s, "insert into u (x) values (0) returning id");
    assert!(back.parse::<i64>().unwrap() > 71, "numbers are never handed out twice: {back}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Many worlds inserting at once, then merged: no two rows got one number.
#[test]
fn concurrent_worlds_never_collide() {
    let db = std::sync::Arc::new(Db::new());
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table t (id bigserial primary key, w int)");
    let v = db.version(MAIN).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let db = db.clone();
            std::thread::spawn(move || {
                let w = format!("w{i}");
                db.fork(MAIN, &w).unwrap();
                let s = &mut Session::new(&w);
                for _ in 0..25 {
                    run(&db, s, &format!("insert into t (w) values ({i}), ({i}), ({i}), ({i}), ({i})"));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(db.version(MAIN).unwrap(), v, "the worlds' numbering never wrote main");
    for i in 0..8 {
        run(&db, s, &format!("merge world w{i}"));
    }
    assert_eq!(one(&db, s, "select count(*) from t"), "1000");
    assert_eq!(one(&db, s, "select count(distinct id) from t"), "1000");
}

/// Counters are logged, so reopening (cleanly, after a crash, or after a checkpoint dropped the
/// log they were in) never hands a number out twice.
#[test]
fn counters_survive_restarts_and_crashes() {
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for f in std::fs::read_dir(from).unwrap() {
            let f = f.unwrap();
            match f.file_type().unwrap().is_dir() {
                true => copy(&f.path(), &to.join(f.file_name())),
                false => drop(std::fs::copy(f.path(), to.join(f.file_name())).unwrap()),
            }
        }
    }
    let (dir, crashed, ckpt, bare) = (tmp("restart"), tmp("restart-crash"), tmp("restart-ckpt"), tmp("restart-bare"));
    let max = |db: &Db, s: &mut Session| one(db, s, "select max(id) from t").parse::<i64>().unwrap();
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table t (id serial primary key, v text)");
    run(&db, s, "create world w");
    run(&db, &mut Session::new("w"), "insert into t (v) values ('w')");
    run(&db, s, "insert into t (v) values ('m')");
    let taken = max(&db, s);
    // "kill -9" now: the world's row may be lost, never the counter it moved
    copy(&dir, &crashed);
    drop(db);
    for d in [&dir, &crashed] {
        let db = Db::open(d).unwrap();
        let s = &mut Session::new(MAIN);
        let id: i64 = one(&db, s, "insert into t (v) values ('next') returning id").parse().unwrap();
        assert!(id > taken, "{id} after {taken}");
        drop(db);
    }
    // after a checkpoint the manifest holds them: replay starts after the log they were written
    // in, and a folder with just the manifest and pages (as a remote keeps) has them too
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(MAIN);
    let taken = max(&db, s);
    db.checkpoint().unwrap();
    copy(&dir, &ckpt);
    copy(&dir.join("pages"), &bare.join("pages"));
    std::fs::copy(dir.join("manifest"), bare.join("manifest")).unwrap();
    drop(db);
    for d in [&dir, &ckpt, &bare] {
        let db = Db::open(d).unwrap();
        let s = &mut Session::new(MAIN);
        let id: i64 = one(&db, s, "insert into t (v) values ('later') returning id").parse().unwrap();
        assert!(id > taken, "{id} after {taken}");
        drop(db);
    }
    for d in [dir, crashed, ckpt, bare] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// A database written by main's binary when counters were rows of main (`_sonos_seq/...`),
/// copied from tests/fixtures: it opens, and numbering carries on past what it handed out.
#[test]
fn counters_from_before_carry_on() {
    let from = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/old-sequences");
    let dir = tmp("old");
    std::fs::create_dir_all(dir.join("history")).unwrap();
    std::fs::create_dir_all(dir.join("pages")).unwrap();
    for f in ["wal-0000000000000001", "history/0000000000000001-0", "pages/pack-00000000"] {
        std::fs::copy(from.join(f), dir.join(f)).unwrap();
    }
    // there: main's 1 and 129, w's 65 and 66; ids at 510 after setval('ids', 500)
    let (mut last, mut last_id) = (129, 510);
    for reopen in [false, true] {
        let db = Db::open(&dir).unwrap();
        let s = &mut Session::new(MAIN);
        let id: i64 = one(&db, s, "insert into s (v) values ('new') returning id").parse().unwrap();
        assert!(id > last, "{id} after {last}");
        let w: i64 = one(&db, &mut Session::new("w"), "insert into s (v) values ('w') returning id").parse().unwrap();
        assert!(w > last && w != id, "{w}");
        let next: i64 = one(&db, s, "select nextval('ids')").parse().unwrap();
        assert!(next > last_id && next % 10 == 0, "{next} after {last_id}");
        run(&db, s, "merge world w");
        assert_eq!(one(&db, s, "select count(*) = count(distinct id) from s"), "true");
        (last, last_id) = (id.max(w), next);
        if reopen {
            db.checkpoint().unwrap();
        } else {
            run(&db, s, "create world w");
        }
        drop(db);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A trigger taking serial numbers inside its statement's write, on main and in a world.
#[test]
fn triggers_take_numbers_in_any_world() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    s.run(
        &db,
        "create table item (id int primary key);
         create table log (id serial primary key, item int);
         create function note() returns trigger language plpgsql as $$
         begin insert into log (item) values (new.id); return new; end $$;
         create trigger item_log after insert on item for each row execute function note();",
        &[],
    )
    .unwrap();
    run(&db, s, "insert into item select g from generate_series(1, 100) g");
    run(&db, s, "create world w");
    let v = db.version(MAIN).unwrap();
    run(&db, &mut Session::new("w"), "insert into item select g from generate_series(101, 250) g");
    assert_eq!(db.version(MAIN).unwrap(), v);
    run(&db, s, "insert into item select g from generate_series(251, 300) g");
    run(&db, s, "merge world w");
    assert_eq!(one(&db, s, "select count(distinct id) from log"), "300");
}

/// The error a statement fails with: its SQLSTATE, message, DETAIL and HINT.
fn refused(db: &Db, s: &mut Session, q: &str) -> (String, String, Option<String>, Option<String>) {
    let e = s.run(db, q, &[]).err().unwrap_or_else(|| panic!("{q} should fail"));
    let (detail, hint) = e.info.map(|i| (i.detail, i.hint)).unwrap_or_default();
    (e.code.to_string(), e.message, detail, hint)
}

/// Views Postgres doesn't write through are refused with its message, DETAIL and HINT; columns that aren't the
/// table's (an expression, a system column) can't be written, nor two view columns of one table column both.
#[test]
fn views_not_written_through_say_why() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table base_tbl (a int primary key, b text default 'Unspecified')");
    run(&db, s, "insert into base_tbl select i, 'Row ' || i from generate_series(-2, 2) g(i)");
    run(&db, s, "create view ro_view1 as select distinct a, b from base_tbl");
    run(&db, s, "create view ro_view4 as select count(*) from base_tbl");
    run(&db, s, "create view ro_view7 as with t as (select a, b from base_tbl) select * from t");
    run(&db, s, "create view ro_view9 as select a, b from base_tbl order by a limit 1");
    run(&db, s, "create view ro_view10 as select 1 as a");
    run(&db, s, "create view ro_view17 as select * from ro_view1");
    run(&db, s, "create view ro_view20 as select a, b, generate_series(1, a) g from base_tbl");
    run(&db, s, "create view rw_view14 as select ctid, a, b from base_tbl");
    run(&db, s, "create view rw_view15 as select a, upper(b) from base_tbl");
    run(&db, s, "create view rw_view16 as select a, b, a as aa from base_tbl");
    let (code, message, detail, hint) = refused(&db, s, "delete from ro_view1");
    assert_eq!(code, "55000");
    assert_eq!(message, "cannot delete from view \"ro_view1\"");
    assert_eq!(detail.as_deref(), Some("Views containing DISTINCT are not automatically updatable."));
    assert_eq!(
        hint.as_deref(),
        Some(
            "To enable deleting from the view, provide an INSTEAD OF DELETE trigger or an unconditional ON DELETE DO \
             INSTEAD rule."
        )
    );
    let detail = |q: &str, s: &mut Session| refused(&db, s, q).2.unwrap_or_default();
    assert_eq!(
        detail("delete from ro_view4", s),
        "Views that return aggregate functions are not automatically updatable."
    );
    assert_eq!(detail("update ro_view7 set a = a + 1", s), "Views containing WITH are not automatically updatable.");
    assert_eq!(
        detail("update ro_view9 set a = a + 1", s),
        "Views containing LIMIT or OFFSET are not automatically updatable."
    );
    let one_table = "Views that do not select from a single table or view are not automatically updatable.";
    assert_eq!(detail("update ro_view10 set a = a + 1", s), one_table);
    assert_eq!(
        detail("update ro_view20 set b = upper(b)", s),
        "Views that return set-returning functions are not automatically updatable."
    );
    // (a view of one that isn't written through: the one below is named)
    let (_, message, detail, _) = refused(&db, s, "insert into ro_view17 values (3, 'ROW 3')");
    assert_eq!(message, "cannot insert into view \"ro_view1\"");
    assert_eq!(detail.as_deref(), Some("Views containing DISTINCT are not automatically updatable."));
    // columns that aren't the table's
    let (code, message, detail, _) = refused(&db, s, "insert into rw_view14 values (null, 3, 'Row 3')");
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("0A000", "cannot insert into column \"ctid\" of view \"rw_view14\"")
    );
    assert_eq!(detail.as_deref(), Some("View columns that refer to system columns are not updatable."));
    run(&db, s, "insert into rw_view14 (a, b) values (3, 'Row 3')");
    run(&db, s, "update rw_view14 set b = 'ROW 3' where a = 3");
    assert_eq!(rows(&db, s, "select a, b from rw_view14 where a = 3"), [["3", "ROW 3"]]);
    run(&db, s, "delete from rw_view14 where a = 3");
    let (_, message, detail, _) = refused(&db, s, "update rw_view15 set upper = 'ROW 3' where a = 3");
    assert_eq!(message, "cannot update column \"upper\" of view \"rw_view15\"");
    assert_eq!(detail.as_deref(), Some("View columns that are not columns of their base relation are not updatable."));
    let (_, message, ..) =
        refused(&db, s, "insert into rw_view15 (a) values (3) on conflict (a) do update set upper = 'x'");
    assert_eq!(message, "cannot insert into column \"upper\" of view \"rw_view15\"");
    let (code, message, ..) = refused(&db, s, "insert into rw_view16 values (3, 'Row 3', 3)");
    assert_eq!((code.as_str(), message.as_str()), ("42601", "multiple assignments to same column \"a\""));
    let (code, ..) = refused(&db, s, "update rw_view16 set a = 3, aa = -3 where a = 3");
    assert_eq!(code, "42601");
    run(&db, s, "update rw_view16 set aa = -3 where a = 2");
    assert_eq!(one(&db, s, "select b from base_tbl where a = -3"), "Row 2");
}

/// information_schema says which views and columns can be written through, as Postgres works it out without
/// triggers, and which events a view has INSTEAD OF triggers for.
#[test]
fn information_schema_says_what_views_take() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table base_tbl (a int primary key, b text)");
    run(&db, s, "create view ro_view1 as select distinct a, b from base_tbl");
    run(&db, s, "create view ro_view17 as select * from ro_view1");
    run(&db, s, "create view rw_view14 as select ctid, a, b from base_tbl");
    run(&db, s, "create view rw_view15 as select a, upper(b) from base_tbl");
    run(&db, s, "create view rw_view2 as select a as aa from rw_view15 where a > 0");
    assert_eq!(
        rows(&db, s, "select table_name, is_insertable_into from information_schema.tables order by table_name"),
        [
            ["base_tbl", "YES"],
            ["ro_view1", "NO"],
            ["ro_view17", "NO"],
            ["rw_view14", "YES"],
            ["rw_view15", "YES"],
            ["rw_view2", "YES"]
        ]
    );
    assert_eq!(
        rows(&db, s, "select table_name, is_updatable from information_schema.views order by table_name"),
        [["ro_view1", "NO"], ["ro_view17", "NO"], ["rw_view14", "YES"], ["rw_view15", "YES"], ["rw_view2", "YES"]]
    );
    assert_eq!(
        rows(
            &db,
            s,
            "select table_name, column_name, is_updatable from information_schema.columns \
             where table_name like 'rw%' order by table_name, ordinal_position"
        ),
        [
            ["rw_view14", "ctid", "NO"],
            ["rw_view14", "a", "YES"],
            ["rw_view14", "b", "YES"],
            ["rw_view15", "a", "YES"],
            ["rw_view15", "upper", "NO"],
            ["rw_view2", "aa", "YES"]
        ]
    );
    run(
        &db,
        s,
        "create function ro_trig() returns trigger language plpgsql as $$ begin return new; end $$;
         create trigger ro_ins instead of insert on ro_view1 for each row execute function ro_trig()",
    );
    assert_eq!(
        rows(
            &db,
            s,
            "select is_insertable_into, is_trigger_updatable, is_trigger_deletable, is_trigger_insertable_into \
             from information_schema.views where table_name = 'ro_view1'"
        ),
        [["NO", "NO", "NO", "YES"]]
    );
}

/// `ctid`: each row's place, `(page,item)`, on the pages Postgres would write the rows to in the order they are
/// read; compared and ordered as tids (by page, then item), not as text, and not in `*`.
#[test]
fn ctid_is_where_a_row_would_be() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    run(&db, s, "create table tidscan (id integer)");
    run(&db, s, "insert into tidscan values (1), (2), (3)");
    assert_eq!(rows(&db, s, "select ctid, * from tidscan"), [["(0,1)", "1"], ["(0,2)", "2"], ["(0,3)", "3"]]);
    assert_eq!(rows(&db, s, "select ctid, * from tidscan where ctid = '(0,1)'"), [["(0,1)", "1"]]);
    assert_eq!(rows(&db, s, "select id from tidscan where '(0,2)' = ctid"), [["2"]]);
    assert_eq!(rows(&db, s, "select id from tidscan where ctid in ('(0,2)', '(0, 3)') order by id"), [["2"], ["3"]]);
    assert_eq!(
        rows(&db, s, "select t1.id, t2.ctid from tidscan t1 join tidscan t2 on t1.ctid = t2.ctid where t1.id = 1"),
        [["1", "(0,1)"]]
    );
    assert_eq!(rows(&db, s, "select * from tidscan where id = 3"), [["3"]]);
    // 58 rows of (int, 100 bytes of text) fill a page, as in Postgres: 4 + 136 bytes each of 8168
    run(&db, s, "create table tidrangescan (id integer, data text)");
    assert_eq!(rows(&db, s, "select ctid from tidrangescan where ctid < '(1, 0)'"), Vec::<Vec<String>>::new());
    run(&db, s, "insert into tidrangescan select i, repeat('x', 100) from generate_series(1, 200) as s(i)");
    assert_eq!(one(&db, s, "select ctid from tidrangescan where id = 58"), "(0,58)");
    assert_eq!(one(&db, s, "select ctid from tidrangescan where id = 59"), "(1,1)");
    assert_eq!(one(&db, s, "select max(id) from tidrangescan where ctid < '(1,0)'"), "58");
    assert_eq!(one(&db, s, "select count(*) from tidrangescan where ctid > '(2,8)'"), "76");
    // (by page, then item: (0,10) is after (0,9))
    assert_eq!(
        rows(&db, s, "select ctid from tidrangescan where ctid between '(0,8)' and '(0,11)' order by ctid"),
        [["(0,8)"], ["(0,9)"], ["(0,10)"], ["(0,11)"]]
    );
    assert_eq!(one(&db, s, "select ctid from tidrangescan order by ctid desc limit 1"), "(3,26)");
    assert_eq!(one(&db, s, "select pg_typeof(ctid)::text from tidscan limit 1"), "tid");
}

/// Type names Postgres has that only name types in its catalog (pseudo-types, int2vector, oidvector), and a
/// table's name as its row type in a cast.
#[test]
fn catalog_and_row_type_names() {
    let db = Db::new();
    let s = &mut Session::new(MAIN);
    assert_eq!(one(&db, s, "select 'internal'::regtype::oid::text"), "2281");
    assert_eq!(one(&db, s, "select 'cstring'::regtype::text"), "cstring");
    assert_eq!(one(&db, s, "select '\"any\"'::regtype::text"), "\"any\"");
    assert_eq!(one(&db, s, "select 'anycompatiblerange'::regtype::oid::text"), "5080");
    assert_eq!(one(&db, s, "select 'index_am_handler'::regtype::oid::text"), "325");
    assert_eq!(one(&db, s, "select pg_typeof('11 22 33'::int2vector)::text"), "int2vector");
    assert_eq!(one(&db, s, "select '1 2'::oidvector::text"), "1 2");
    run(&db, s, "create table compositetable (a text, b text)");
    assert_eq!(one(&db, s, "select '(Joe,Blow)'::compositetable::text"), "(Joe,Blow)");
    assert_eq!(rows(&db, s, "select (null::compositetable).a"), [["null"]]);
}
