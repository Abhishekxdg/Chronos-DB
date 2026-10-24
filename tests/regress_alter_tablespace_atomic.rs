//! ALTER TABLE ... , SET TABLESPACE t is one statement: the move and the statement's other changes are one write, so
//! either all of it lands or none does, as Postgres's ALTER TABLE is one transaction. A move that fails once the other
//! changes are worked out leaves none of them, and so does a crash (the log is cut wherever the statement's records
//! are). The ordinary cases do what PostgreSQL 16 does: a partitioned table records only its own, the other changes
//! reach its partitions, and a transaction takes all of it back.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};
use std::fs;
use std::path::{Path, PathBuf};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code, e.message),
    }
}

/// The names of the relations in tablespace `space`, in name order.
fn in_space(db: &Db, s: &mut Session, space: &str) -> Vec<String> {
    let q = format!(
        "select c.relname from pg_class c, pg_tablespace t where c.reltablespace = t.oid and t.spcname = '{space}' \
         order by 1"
    );
    run(db, s, &q).into_iter().map(|r| r[0].clone()).collect()
}

fn session(db: &Db) -> Session {
    let mut s = Session::new(MAIN);
    run(db, &mut s, "set allow_in_place_tablespaces = true");
    run(db, &mut s, "create tablespace regress_tblspace location ''");
    s
}

#[test]
fn the_move_and_the_other_changes_are_one_write() {
    let db = Db::new();
    let mut s = session(&db);
    run(&db, &mut s, "create table k (a int)");
    let writes = |s: &mut Session, q: &str| {
        let before = db.version(MAIN).unwrap();
        run(&db, s, q);
        db.version(MAIN).unwrap() - before
    };
    assert_eq!(writes(&mut s, "alter table k add column c int, set tablespace regress_tblspace"), 1);
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["k"]);
    assert_eq!(run(&db, &mut s, "select a, c from k"), Vec::<Vec<String>>::new());
    // with row level security, which is kept in a row of its own
    let q = "alter table k enable row level security, add column d int, set tablespace pg_default";
    assert_eq!(writes(&mut s, q), 1);
    assert!(in_space(&db, &mut s, "regress_tblspace").is_empty());
    assert_eq!(one(&db, &mut s, "select relrowsecurity from pg_class where relname = 'k'"), "true");
    // a unique constraint built with it (its index stays where indexes are made)
    assert_eq!(writes(&mut s, "alter table k add unique (a), set tablespace regress_tblspace"), 1);
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["k"]);
    run(&db, &mut s, "insert into k values (1)");
    assert_eq!(fails(&db, &mut s, "insert into k values (1)").0, "23505");
}

#[test]
fn a_move_that_fails_after_the_other_changes_leaves_none_of_them() {
    let db = Db::new();
    let mut s = session(&db);
    run(&db, &mut s, "create table k (a int)");
    run(&db, &mut s, "insert into k values (1)");
    run(&db, &mut s, "create schema s2");
    // SET SCHEMA among them (which Postgres's grammar doesn't take): checked first, the move is there to make, but
    // once the table has moved to s2 there is none by its old name to move
    let before = db.version(MAIN).unwrap();
    fails(&db, &mut s, "alter table k set schema s2, set tablespace regress_tblspace");
    assert_eq!(db.version(MAIN).unwrap(), before);
    assert_eq!(one(&db, &mut s, "select count(*) from public.k"), "1");
    assert_eq!(fails(&db, &mut s, "select * from s2.k").0, "42P01");
    assert!(in_space(&db, &mut s, "regress_tblspace").is_empty());

    // as Postgres checks them: each SET TABLESPACE's tablespace before the other changes are made (then that there is
    // only one), each change before the move, and where the table may go (not pg_global) as it moves
    let no_space = ("42704", "tablespace \"nosuch\" does not exist".to_string());
    assert_eq!(fails(&db, &mut s, "alter table k add column a int, set tablespace nosuch"), no_space);
    assert_eq!(fails(&db, &mut s, "alter table k set tablespace nosuch, set tablespace pg_default"), no_space);
    assert_eq!(fails(&db, &mut s, "alter table k set tablespace pg_default, set tablespace nosuch"), no_space);
    assert_eq!(
        fails(&db, &mut s, "alter table k set tablespace regress_tblspace, set tablespace pg_default"),
        ("42601", "cannot have multiple SET TABLESPACE subcommands".into())
    );
    let a_twice = ("42701", "column \"a\" of relation \"k\" already exists".to_string());
    assert_eq!(fails(&db, &mut s, "alter table k add column a int, set tablespace regress_tblspace"), a_twice);
    assert_eq!(fails(&db, &mut s, "alter table k add column a int, set tablespace pg_global"), a_twice);
    assert_eq!(
        fails(&db, &mut s, "alter table k drop column nosuch, set tablespace pg_global"),
        ("42703", "column \"nosuch\" of relation \"k\" does not exist".into())
    );
    let global = ("22023", "only shared relations can be placed in pg_global tablespace".to_string());
    assert_eq!(fails(&db, &mut s, "alter table k add column z int, set tablespace pg_global"), global);
    assert_eq!(fails(&db, &mut s, "alter table k add unique (a), set tablespace pg_global"), global);
    assert_eq!(fails(&db, &mut s, "alter table k add primary key (a), set tablespace pg_global"), global);
    run(&db, &mut s, "insert into k values (1)");
    let unique = fails(&db, &mut s, "alter table k add unique (a), set tablespace regress_tblspace");
    assert_eq!(unique, ("23505", "could not create unique index \"k_a_key\"".into()));
    assert_eq!(fails(&db, &mut s, "alter table k add unique (a), set tablespace pg_global"), unique);
    let pkey = ("23505", "could not create unique index \"k_pkey\"".to_string());
    assert_eq!(fails(&db, &mut s, "alter table k add primary key (a), set tablespace pg_global"), pkey);
    assert_eq!(db.version(MAIN).unwrap(), before + 1, "only the INSERT wrote");
    assert_eq!(fails(&db, &mut s, "select z from k").0, "42703");
    assert!(in_space(&db, &mut s, "regress_tblspace").is_empty());
}

#[test]
fn partitions_children_and_transactions() {
    let db = Db::new();
    let mut s = session(&db);
    // a partitioned table records where its partitions are made from then on; the column reaches them all
    run(&db, &mut s, "create table p (a int, b int) partition by range (a)");
    run(&db, &mut s, "create table p1 partition of p for values from (0) to (10)");
    run(&db, &mut s, "alter table p add column c int, set tablespace regress_tblspace");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["p"]);
    run(&db, &mut s, "insert into p values (1, 2, 3)");
    assert_eq!(run(&db, &mut s, "select a, b, c from p1"), [["1", "2", "3"]]);
    run(&db, &mut s, "create table p2 partition of p for values from (10) to (20)");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["p", "p2"]);
    // a child that inherits, and moves, in one statement
    run(&db, &mut s, "create table par (a int)");
    run(&db, &mut s, "create table ch (a int)");
    run(&db, &mut s, "insert into ch values (5)");
    run(&db, &mut s, "alter table ch inherit par, set tablespace regress_tblspace");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["ch", "p", "p2"]);
    assert_eq!(one(&db, &mut s, "select a from par"), "5");
    // a transaction takes back all of it, or keeps all of it
    run(&db, &mut s, "create table k (a int)");
    run(&db, &mut s, "begin");
    run(&db, &mut s, "alter table k add column d int, set tablespace regress_tblspace");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["ch", "k", "p", "p2"]);
    run(&db, &mut s, "rollback");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["ch", "p", "p2"]);
    assert_eq!(fails(&db, &mut s, "select d from k").0, "42703");
    run(&db, &mut s, "begin");
    run(&db, &mut s, "alter table k add column d int, set tablespace regress_tblspace");
    run(&db, &mut s, "commit");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["ch", "k", "p", "p2"]);
    assert_eq!(run(&db, &mut s, "select d from k"), Vec::<Vec<String>>::new());
    // an index moves by itself
    run(&db, &mut s, "create index k_a on k (a)");
    run(&db, &mut s, "alter index k_a set tablespace regress_tblspace");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["ch", "k", "k_a", "p", "p2"]);
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-ts-atomic-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dst = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dst);
        } else {
            fs::copy(&p, dst).unwrap();
        }
    }
}

/// Every world's rows.
fn fingerprint(db: &Db) -> String {
    let mut out = String::new();
    for (b, parent) in db.branches() {
        out += &format!("{b} <- {parent:?}\n  {:?}\n", db.scan(&b, "", usize::MAX).unwrap());
    }
    out
}

/// Where each of a log segment's records ends: frames are [u32 len][u32 crc][payload], and the file may go on in zeros
/// written ahead of them (a zero length ends it).
fn record_ends(seg: &Path) -> Vec<u64> {
    let b = fs::read(seg).unwrap();
    let (mut off, mut ends) = (0, vec![]);
    while let Some(h) = b.get(off..off + 8) {
        let len = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
        if len == 0 || off + 8 + len > b.len() {
            break;
        }
        off += 8 + len;
        ends.push(off as u64);
    }
    ends
}

fn last_segment(dir: &Path) -> PathBuf {
    let mut segs: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("wal-"))
        .collect();
    segs.sort();
    segs.pop().unwrap()
}

#[test]
fn a_crash_keeps_all_of_the_statement_or_none_of_it() {
    let (dir, crash) = (tmp("db"), tmp("crash"));
    let (seg, start, end, before, after);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = session(&db);
        run(&db, &mut s, "create table k (a int primary key, b text)");
        run(&db, &mut s, "insert into k select i, 'b' || i from generate_series(1, 50) i");
        seg = last_segment(&dir);
        start = *record_ends(&seg).last().unwrap();
        before = fingerprint(&db);
        run(&db, &mut s, "alter table k add column c int, set tablespace regress_tblspace");
        // a statement on main is synced before it returns: the log holds all of it by now
        end = *record_ends(&seg).last().unwrap();
        after = fingerprint(&db);
    }
    assert_eq!(last_segment(&dir), seg, "no checkpoint in between: one segment holds the statement");
    assert_ne!(before, after);
    // the log cut at each of the statement's records, and a byte either side
    let cuts: Vec<u64> = record_ends(&seg)
        .into_iter()
        .filter(|&e| e > start && e <= end)
        .flat_map(|e| [e - 1, e, e + 1])
        .chain([start, start + 1])
        .collect();
    for cut in cuts {
        let _ = fs::remove_dir_all(&crash);
        copy_dir(&dir, &crash);
        let mut bytes = fs::read(&seg).unwrap();
        bytes.truncate(cut as usize);
        fs::write(crash.join(seg.file_name().unwrap()), bytes).unwrap();
        let db = Db::open(&crash).unwrap_or_else(|e| panic!("cut {cut} of {start}..{end}: {e}"));
        chronos::sql::cleanup(&db);
        let got = fingerprint(&db);
        let whole = if cut >= end { &after } else { &before };
        assert!(got == *whole, "cut {cut} of {start}..{end}: neither before the statement nor after all of it");
    }
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&crash);
}
