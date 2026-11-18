//! Foreign keys to a UNIQUE column: kept across worlds and merges (two worlds each fine alone can
//! leave a row pointing at a value the other deleted) and after reopening, and written back by
//! `diff as sql`. Against Postgres, the rules are in tests/pgdiff.rs (`foreign_key_to_a_unique_column`).

use chronos::Db;
use chronos::sql::{Session, Value};

fn rows(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    out.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> chronos::sql::SqlError {
    s.run(db, q, &[]).err().unwrap_or_else(|| panic!("{q} should fail"))
}

#[test]
fn worlds_merges_and_reopening_keep_a_reference_to_a_unique_column() {
    let dir = std::env::temp_dir().join(format!("chronos-fk-unique-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let s = &mut Session::new(chronos::MAIN);
        for q in [
            "create table org (id int primary key, slug text unique)",
            "create table member (id int primary key, org text references org (slug) on update cascade)",
            "insert into org values (1, 'acme'), (2, 'zeta'), (3, 'old')",
            "insert into member values (10, 'acme')",
        ] {
            s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
        }
        // a world adds a member of 'old'; main deletes 'old' meanwhile: each is fine alone
        s.run(&db, "create world w; use world w; insert into member values (11, 'old'); use world main", &[]).unwrap();
        s.run(&db, "delete from org where slug = 'old'", &[]).unwrap();
        let e = fails(&db, s, "merge world w");
        assert_eq!(e.code, "23503", "{e:?}");
        assert_eq!(rows(&db, s, "select id from member order by id"), ["10"], "a refused merge changes nothing");
        // the other way: main adds 'new' and the world adds a member of it
        s.run(&db, "create world w2; use world w2; insert into org values (4, 'new'); insert into member values (12, 'new'); use world main", &[]).unwrap();
        s.run(&db, "insert into org values (5, 'other')", &[]).unwrap();
        s.run(&db, "merge world w2", &[]).unwrap();
        assert_eq!(rows(&db, s, "select id, org from member order by id"), ["10|acme", "12|new"]);
        // the diff replays into the same schema, with the referenced column named
        s.run(
            &db,
            "create world w3; use world w3; create table extra (id int primary key, o text references org (slug))",
            &[],
        )
        .unwrap();
        let sql = s.run(&db, "diff as sql", &[]).unwrap().pop().unwrap();
        let sql: Vec<String> = sql.rows.iter().map(|r| r[0].to_text()).collect();
        assert!(sql.iter().any(|q| q.contains("references org (slug)")), "{sql:?}");
        s.run(&db, "use world main", &[]).unwrap();
        db.close().unwrap();
    }
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(chronos::MAIN);
    assert_eq!(fails(&db, s, "insert into member values (13, 'nowhere')").code, "23503");
    assert_eq!(
        fails(
            &db,
            s,
            "update org set slug = 'x' where slug = 'zeta' returning id; delete from org where slug = 'acme'"
        )
        .code,
        "23503"
    );
    s.run(&db, "update org set slug = 'acme2' where slug = 'acme'", &[]).unwrap();
    assert_eq!(rows(&db, s, "select org from member where id = 10"), ["acme2"], "ON UPDATE CASCADE after reopening");
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// H1: a world deletes (or rekeys) a parent row that main's new child references through a UNIQUE column, not the
/// primary key: each side is fine alone, the merge would leave the child pointing at nothing, so it is refused.
#[test]
fn delete_parent_referenced_unique_refused() {
    let db = Db::new();
    let s = &mut Session::new(chronos::MAIN);
    for q in [
        "create table org (id int primary key, slug text unique)",
        "create table member (id int primary key, org text references org (slug))",
        "insert into org values (1, 'acme'), (2, 'old')",
    ] {
        s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    s.run(&db, "create world w; use world w; delete from org where slug = 'old'; use world main", &[]).unwrap();
    s.run(&db, "insert into member values (11, 'old')", &[]).unwrap();
    let e = fails(&db, s, "merge world w");
    assert_eq!(e.code, "23503", "{e:?}");
    assert_eq!(rows(&db, s, "select slug from org order by id"), ["acme", "old"], "a refused merge changes nothing");
    // the referenced value changed rather than deleted: the same
    s.run(&db, "create world w2; use world w2; update org set slug = 'ACME' where id = 1; use world main", &[])
        .unwrap();
    s.run(&db, "insert into member values (12, 'acme')", &[]).unwrap();
    let e = fails(&db, s, "merge world w2");
    assert_eq!(e.code, "23503", "{e:?}");
    assert_eq!(rows(&db, s, "select slug from org order by id"), ["acme", "old"]);
    // a value nobody references still goes
    s.run(&db, "insert into org values (3, 'spare')", &[]).unwrap();
    s.run(&db, "create world w3; use world w3; delete from org where slug = 'spare'; use world main", &[]).unwrap();
    s.run(&db, "insert into member values (13, 'acme')", &[]).unwrap();
    s.run(&db, "merge world w3", &[]).unwrap();
    assert_eq!(rows(&db, s, "select slug from org order by id"), ["acme", "old"]);
}
