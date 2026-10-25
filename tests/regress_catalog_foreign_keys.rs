//! The system columns of the catalog tables, as Postgres's oidjoins, opr_sanity and drop_operator regression files read
//! them: every catalog table has a `ctid` (a check of a catalog key names the rows it finds by it) and a `tableoid`,
//! which a `*` leaves out.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

/// The names of the columns `q` gives.
fn columns(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().columns.clone().unwrap_or_default().into_iter().map(|(n, _)| n).collect()
}

#[test]
fn a_catalog_table_has_a_ctid_and_a_tableoid_a_star_leaves_out() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table t (a int primary key); create function f(int) returns int language sql as 'select 1'",
    );
    // (Postgres's own rows, of a function's; the world's; and pg_proc's, which are both)
    for table in ["pg_operator", "pg_cast", "pg_aggregate", "pg_class", "pg_attribute", "pg_type", "pg_proc"] {
        let star = columns(&db, &mut s, &format!("select * from {table} limit 1"));
        assert!(!star.iter().any(|c| c == "ctid" || c == "tableoid"), "{table}: {star:?}");
        let system = run(&db, &mut s, &format!("select ctid, tableoid::regclass from {table} limit 1"));
        assert_eq!(system[0][1], table);
        assert!(system[0][0].starts_with("(0,"), "{table}: {system:?}");
    }
    assert_eq!(one(&db, &mut s, "select ctid from pg_operator offset 2 limit 1"), "(0,3)");
    // (a function of the world's comes after the built-in ones)
    let at = one(&db, &mut s, "select ctid from pg_proc where proname = 'f'");
    let place: Vec<usize> = at.trim_matches(|c| c == '(' || c == ')').split(',').map(|n| n.parse().unwrap()).collect();
    assert!(place[0] * 291 + place[1] > 3000, "{at}");
    // read through a subquery, as a check of a key whose last column is an array reads it
    let found = run(
        &db,
        &mut s,
        "select ctid, a from (select ctid, unnest(proargtypes) as a from pg_proc where proname = 'f') fk
         where not exists (select 1 from pg_type pk where pk.oid = fk.a)",
    );
    assert!(found.is_empty(), "{found:?}");
    let found = run(
        &db,
        &mut s,
        "select ctid, adrelid, adnum from pg_attrdef fk
         where not exists (select 1 from pg_attribute pk where pk.attrelid = fk.adrelid and pk.attnum = fk.adnum)",
    );
    assert!(found.is_empty(), "{found:?}");
    // (the table, not one of the views of it)
    assert_eq!(one(&db, &mut s, "select 1260::regclass"), "pg_authid");
}
