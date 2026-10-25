//! The catalog's system columns, checked against Postgres 16.15 on the real server: each value here is what Postgres
//! gives (or the same thing where Chronos's rows differ, such as the page a row is on). A `*` of a catalog table, and
//! whatever reads one (a CTE, a subquery, a view, a whole-row value, CREATE TABLE AS), has no `ctid` or `tableoid`; a
//! ctid orders as a tid; pg_proc's rows keep their places when a query narrows them by oid or name; a name given to
//! has_table_privilege folds as an identifier; and a parameter or variable named as a system column meets the column.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

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

fn columns(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().columns.clone().unwrap_or_default().into_iter().map(|(n, _)| n).collect()
}

fn fresh() -> (Db, Session) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int); insert into t values (1)");
    (db, s)
}

#[test]
fn ctid_orders_as_a_tid() {
    let (db, mut s) = fresh();
    let r = run(&db, &mut s, "select ctid from pg_operator order by ctid offset 290 limit 2");
    assert_eq!(r, [["(0,291)"], ["(1,1)"]]);
    assert_eq!(one(&db, &mut s, "select pg_typeof(ctid)::text from pg_operator limit 1"), "tid");
    assert_eq!(one(&db, &mut s, "select min(ctid) from pg_operator"), "(0,1)");
    assert_eq!(run(&db, &mut s, "select oprname from pg_operator where ctid = '(0,1)'").len(), 1);
}

#[test]
fn a_star_of_a_catalog_table_has_no_system_columns_downstream() {
    let (db, mut s) = fresh();
    for q in
        ["with c as (select * from pg_operator) select ctid from c", "select ctid from (select * from pg_operator) s"]
    {
        assert_eq!(fails(&db, &mut s, q).0, "42703", "{q}");
    }
    assert_eq!(one(&db, &mut s, "select s.ctid from (select ctid, oprname from pg_operator) s limit 1"), "(0,1)");
    run(&db, &mut s, "create view vop as select * from pg_operator");
    assert_eq!(fails(&db, &mut s, "select ctid from vop").0, "42703");
    let json =
        "select row_to_json(o)::text like '%ctid%' or row_to_json(o)::text like '%tableoid%' from pg_am o limit 1";
    assert_eq!(one(&db, &mut s, json), "false");
    assert_eq!(
        one(&db, &mut s, "select o::text from pg_am o where amname = 'heap'"),
        "(2,heap,heap_tableam_handler,t)"
    );
    assert_eq!(
        one(&db, &mut s, "select count(*) = (select count(*) from pg_am) from pg_am a natural join pg_am b"),
        "true"
    );
    run(&db, &mut s, "create table p1 as select * from pg_proc limit 0");
    assert_eq!(columns(&db, &mut s, "select * from p1").len(), 30);
    run(&db, &mut s, "create table r1 as select * from pg_roles limit 0");
    assert_eq!(columns(&db, &mut s, "select * from r1").len(), 13);
    run(&db, &mut s, "create table s1 as select * from pg_settings limit 0");
    assert_eq!(columns(&db, &mut s, "select * from s1").len(), 17);
}

#[test]
fn tableoid_of_catalog_tables() {
    let (db, mut s) = fresh();
    assert_eq!(run(&db, &mut s, "select distinct tableoid::regclass::text from pg_type"), [["pg_type"]]);
    assert_eq!(one(&db, &mut s, "select tableoid::regclass::text from pg_namespace limit 1"), "pg_namespace");
    assert_eq!(fails(&db, &mut s, "select ctid from pg_am, pg_opclass").0, "42702");
    let (code, message) = fails(&db, &mut s, "select tableoid from pg_roles");
    assert_eq!((code, message.as_str()), ("42703", "column \"tableoid\" does not exist"));
    assert_eq!(fails(&db, &mut s, "select ctid from information_schema.tables limit 1").0, "42703");
}

#[test]
fn alias_lists() {
    let (db, mut s) = fresh();
    assert_eq!(one(&db, &mut s, "select count(*) from (select * from pg_roles r(a, b)) s where false"), "0");
    // (Postgres's message and code)
    let (code, message) = fails(&db, &mut s, "select * from pg_am x(a,b,c,d,e)");
    assert_eq!((code, message.as_str()), ("42P10", "table \"x\" has 4 columns available but 5 columns specified"));
}

#[test]
fn alias_lists_system_column_still_there() {
    let (db, mut s) = fresh();
    assert_eq!(one(&db, &mut s, "select x.ctid is not null from pg_am x(a, b) limit 1"), "true");
    assert_eq!(one(&db, &mut s, "select s.c is not null from pg_am a, lateral (select a.ctid as c) s limit 1"), "true");
}

#[test]
fn sql_function_parameter_named_ctid_loses_to_the_column() {
    let (db, mut s) = fresh();
    run(&db, &mut s, "create function f_ctid(ctid tid) returns tid language sql as 'select ctid from t limit 1'");
    assert_eq!(one(&db, &mut s, "select f_ctid('(9,9)')"), "(0,1)");
}

#[test]
fn plpgsql_variable_named_tableoid_is_ambiguous_beside_a_table() {
    let (db, mut s) = fresh();
    run(
        &db,
        &mut s,
        "create function f_vpl(tableoid oid) returns oid language plpgsql as $$ declare r oid; begin select tableoid into r from t limit 1; return r; end $$",
    );
    let (code, message) = fails(&db, &mut s, "select f_vpl(5)");
    assert_eq!((code, message.as_str()), ("42702", "column reference \"tableoid\" is ambiguous"));
}

#[test]
fn privileges_by_name_fold_as_identifiers() {
    let (db, mut s) = fresh();
    run(&db, &mut s, "create role ctid_rv");
    run(&db, &mut s, "set role ctid_rv");
    let r = run(
        &db,
        &mut s,
        "select has_table_privilege('pg_roles', 'SELECT'), has_table_privilege('pg_user','SELECT'),
                has_table_privilege('pg_authid','SELECT'),
                has_table_privilege('PG_ROLES','SELECT'), has_table_privilege('\"pg_roles\"', 'SELECT'),
                has_table_privilege('pg_catalog.PG_ROLES', 'SELECT'), has_table_privilege(' pg_user ', 'SELECT')",
    );
    assert_eq!(r, [["true", "true", "false", "true", "true", "true", "true"]]);
}

#[test]
fn column_privileges_of_pg_roles_and_pg_authid() {
    let (db, mut s) = fresh();
    run(&db, &mut s, "create role ctid_rv");
    run(&db, &mut s, "set role ctid_rv");
    let r = run(
        &db,
        &mut s,
        "select has_column_privilege('pg_roles', 'rolname', 'SELECT'), has_column_privilege('pg_authid', 'rolname', 'SELECT'),
                has_any_column_privilege('pg_roles', 'SELECT'), has_any_column_privilege('pg_authid', 'SELECT'),
                has_table_privilege('pg_authid'::regclass, 'SELECT')",
    );
    assert_eq!(r, [["true", "false", "true", "false", "false"]]);
}

#[test]
fn pg_proc_narrowed_keeps_each_rows_place() {
    let (db, mut s) = fresh();
    run(&db, &mut s, "create function mine(int) returns int language sql as 'select 1'");
    for name in ["int4pl", "mine"] {
        let narrowed = one(&db, &mut s, &format!("select ctid from pg_proc where proname = '{name}'"));
        let whole = one(
            &db,
            &mut s,
            &format!("select p.ctid from pg_proc p, pg_proc q where p.proname = '{name}' and q.oid = p.oid"),
        );
        assert_eq!(narrowed, whole, "{name}");
        let by_oid = one(&db, &mut s, &format!("select ctid from pg_proc where oid = '{name}'::regproc"));
        assert_eq!(by_oid, whole, "{name}");
    }
    let all = run(&db, &mut s, "select ctid from pg_proc");
    let distinct = run(&db, &mut s, "select count(distinct ctid) from pg_proc");
    assert_eq!(distinct[0][0], all.len().to_string());
}

#[test]
fn ctid_and_star_together() {
    let (db, mut s) = fresh();
    let cols = columns(&db, &mut s, "select ctid, * from pg_am limit 1");
    assert_eq!(cols, ["ctid", "oid", "amname", "amhandler", "amtype"]);
    let cols = columns(&db, &mut s, "select o.*, o.tableoid from pg_am o limit 1");
    assert_eq!(cols, ["oid", "amname", "amhandler", "amtype", "tableoid"]);
}
