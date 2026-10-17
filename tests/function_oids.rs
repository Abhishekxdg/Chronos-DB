//! A function's oid stays its own: a later function whose signature hashes to the same slot can't take it, so a
//! stored regproc keeps naming the function it named, after a restart too.

use chronos::sql::{Output, Session};
use chronos::{Db, MAIN};

fn one(db: &Db, s: &mut Session, sql: &str) -> String {
    let o: Output = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    o.rows[0][0].to_text()
}

#[test]
fn a_colliding_function_made_later_takes_no_existing_oid() {
    let dir = std::env::temp_dir().join(format!("chronos-test-{}-funcoids", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(MAIN);
        // these two signatures (oid_t.hfbaa(), oid_t.doaim()) hash to the same slot, and the second sorts first
        s.run(&db, "create schema oid_t", &[]).unwrap();
        s.run(&db, "create function oid_t.hfbaa() returns int language sql as 'select 1'", &[]).unwrap();
        s.run(&db, "create table keep (p regproc)", &[]).unwrap();
        s.run(&db, "insert into keep values ('oid_t.hfbaa')", &[]).unwrap();
        let oid = one(&db, &mut s, "select p::oid from keep");
        s.run(&db, "create function oid_t.doaim() returns int language sql as 'select 2'", &[]).unwrap();
        assert_eq!(one(&db, &mut s, "select p::text from keep"), "oid_t.hfbaa");
        assert_eq!(one(&db, &mut s, "select 'oid_t.hfbaa'::regproc::oid"), oid);
        assert_ne!(one(&db, &mut s, "select 'oid_t.doaim'::regproc::oid"), oid);
        // OR REPLACE keeps it
        s.run(&db, "create or replace function oid_t.hfbaa() returns int language sql as 'select 3'", &[]).unwrap();
        assert_eq!(one(&db, &mut s, "select 'oid_t.hfbaa'::regproc::oid"), oid);
    }
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(one(&db, &mut s, "select p::text from keep"), "oid_t.hfbaa");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two worlds each make a function and keep its regproc; after both merge, each value still names its own
/// function: the oids come from a counter every world shares, so they never meet.
#[test]
fn functions_made_in_two_worlds_keep_their_oids_after_both_merge() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, "create schema oid_t", &[]).unwrap();
    s.run(&db, "create world w1", &[]).unwrap();
    s.run(&db, "create world w2", &[]).unwrap();
    let mut a = Session::new("w1");
    let mut b = Session::new("w2");
    // the same pair that hashes to one slot
    a.run(&db, "create function oid_t.hfbaa() returns int language sql as 'select 1'", &[]).unwrap();
    a.run(&db, "create table oid_t.ka (p regproc)", &[]).unwrap();
    a.run(&db, "insert into oid_t.ka values ('oid_t.hfbaa')", &[]).unwrap();
    b.run(&db, "create function oid_t.doaim() returns int language sql as 'select 2'", &[]).unwrap();
    b.run(&db, "create table oid_t.kb (p regproc)", &[]).unwrap();
    b.run(&db, "insert into oid_t.kb values ('oid_t.doaim')", &[]).unwrap();
    s.run(&db, "merge world w1", &[]).unwrap();
    s.run(&db, "merge world w2", &[]).unwrap();
    assert_eq!(one(&db, &mut s, "select p::text from oid_t.ka"), "oid_t.hfbaa");
    assert_eq!(one(&db, &mut s, "select p::text from oid_t.kb"), "oid_t.doaim");
}
