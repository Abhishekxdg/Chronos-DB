//! The system columns of the catalog, as Postgres has them: a catalog table's `ctid` numbers its rows on pages of at
//! most 291 (MaxHeapTuplesPerPage), a catalog view (pg_roles, pg_tables, pg_settings, ...) has no `ctid` or
//! `tableoid` and an oid of its own, and pg_roles and pg_user may be read by everyone, pg_authid not. A column of a
//! relation's own, or its alias list's, wins over a system column of the name; an alias list that names too many is
//! 42P10; a SQL function's parameter named as any system column loses to the column. Each value as Postgres 16.15
//! gives it.

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

/// The names of the columns `q` gives.
fn columns(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().columns.clone().unwrap_or_default().into_iter().map(|(n, _)| n).collect()
}

/// A ctid's page and item.
fn place(ctid: &str) -> (usize, usize) {
    let (p, i) = ctid.trim_matches(|c| c == '(' || c == ')').split_once(',').unwrap();
    (p.parse().unwrap(), i.parse().unwrap())
}

#[test]
fn pg_roles_and_pg_user_may_be_read_by_everyone_pg_authid_not() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create role cc_reader");
    // (asked of another role, by name)
    let asked = run(
        &db,
        &mut s,
        "select has_table_privilege('cc_reader', 'pg_roles', 'SELECT'), has_table_privilege('cc_reader', 'pg_user', 'SELECT'),
                has_table_privilege('cc_reader', 'pg_authid', 'SELECT'), has_table_privilege('cc_reader', 'pg_class', 'SELECT')",
    );
    assert_eq!(asked, [["true", "true", "false", "true"]]);
    run(&db, &mut s, "set role cc_reader");
    let mine = run(
        &db,
        &mut s,
        "select has_table_privilege('pg_roles', 'SELECT'), has_table_privilege('pg_user', 'SELECT'),
                has_table_privilege('pg_authid', 'SELECT'), has_table_privilege('pg_catalog.pg_roles', 'SELECT'),
                has_table_privilege('PG_AUTHID', 'SELECT'), has_table_privilege('pg_catalog.pg_authid', 'SELECT')",
    );
    assert_eq!(mine, [["true", "true", "false", "true", "false", "false"]]);
    // (by oid, 1260 is pg_authid, the table)
    assert_eq!(one(&db, &mut s, "select has_table_privilege(1260::oid, 'SELECT')"), "false");
    // and the views read as such
    assert_eq!(one(&db, &mut s, "select count(*) > 0 from pg_roles where rolname = 'cc_reader'"), "true");
    assert_eq!(one(&db, &mut s, "select count(*) >= 0 from pg_user"), "true");
    run(&db, &mut s, "reset role");
}

#[test]
fn a_catalog_view_has_no_system_columns() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int); insert into t values (1)");
    for view in ["pg_roles", "pg_user", "pg_tables", "pg_views", "pg_indexes", "pg_settings", "pg_stat_activity"] {
        for col in ["ctid", "tableoid"] {
            let (code, message) = fails(&db, &mut s, &format!("select {col} from {view}"));
            assert_eq!(code, "42703", "{view}.{col}: {message}");
        }
        let star = columns(&db, &mut s, &format!("select * from {view} limit 1"));
        assert!(!star.iter().any(|c| c == "ctid" || c == "tableoid"), "{view}: {star:?}");
    }
    // a ctid beside one is the other table's
    assert_eq!(one(&db, &mut s, "select ctid from t, pg_roles r where r.rolname = current_user"), "(0,1)");
    assert_eq!(one(&db, &mut s, "select t.ctid from t join pg_roles r on true limit 1"), "(0,1)");
    assert_eq!(one(&db, &mut s, "select t.tableoid = 't'::regclass from t, pg_settings limit 1"), "true");
    let (code, _) = fails(&db, &mut s, "select r.ctid from t join pg_roles r on true");
    assert_eq!(code, "42703");
    // (a table of the catalog keeps them)
    assert_eq!(one(&db, &mut s, "select tableoid::regclass from pg_authid limit 1"), "pg_authid");
    assert_eq!(one(&db, &mut s, "select ctid from pg_class limit 1"), "(0,1)");
}

#[test]
fn a_catalog_tables_ctid_has_at_most_291_items_to_a_page() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(one(&db, &mut s, "select ctid from pg_operator offset 290 limit 1"), "(0,291)");
    assert_eq!(one(&db, &mut s, "select ctid from pg_operator offset 291 limit 1"), "(1,1)");
    // every row of pg_proc in its place, one after another
    let all = run(&db, &mut s, "select ctid from pg_proc");
    assert!(all.len() > 3000, "{}", all.len());
    for (k, r) in all.iter().enumerate() {
        assert_eq!(place(&r[0]), (k / 291, k % 291 + 1), "row {k}");
    }
}

#[test]
fn system_columns_stay_out_of_an_alias_list_and_a_suggestion() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let n = columns(&db, &mut s, "select * from pg_operator limit 0").len();
    let names: Vec<String> = (0..=n).map(|i| format!("c{i}")).collect();
    let (code, message) = fails(&db, &mut s, &format!("select * from pg_operator x({}) limit 0", names.join(", ")));
    assert_eq!(code, "42P10");
    assert_eq!(message, format!("table \"x\" has {n} columns available but {} columns specified", n + 1));
    let named = columns(&db, &mut s, &format!("select * from pg_operator x({}) limit 0", names[..n].join(", ")));
    assert_eq!(named.len(), n);
    // (the system column stays as it is, by its own name)
    assert_eq!(
        one(&db, &mut s, &format!("select x.ctid from pg_operator x({}) limit 1", names[..2].join(", "))),
        "(0,1)"
    );
    for wrong in ["ctix", "tableoi"] {
        let (code, message) = fails(&db, &mut s, &format!("select {wrong} from pg_class"));
        assert_eq!(code, "42703");
        assert!(!message.contains("ctid") && !message.contains("tableoid"), "{message}");
    }
}

#[test]
fn a_sql_functions_parameter_named_tableoid_is_the_column_of_a_table_not_of_a_view() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int); insert into t values (1)");
    run(
        &db,
        &mut s,
        "create function from_table(tableoid oid) returns oid language sql as 'select tableoid from pg_operator limit 1';
         create function from_view(tableoid oid) returns oid language sql as 'select tableoid from pg_roles limit 1';
         create function from_mine(tableoid oid) returns oid language sql as 'select tableoid from t limit 1'",
    );
    assert_eq!(one(&db, &mut s, "select from_table(5)"), "2617");
    assert_eq!(one(&db, &mut s, "select from_view(5)"), "5");
    assert_eq!(one(&db, &mut s, "select from_mine(5) = 't'::regclass"), "true");
}

#[test]
fn an_alias_list_naming_too_many_columns_is_42p10() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int, b int); insert into t values (1, 2)");
    for (q, message) in [
        ("select * from pg_am x(a,b,c,d,e)", "table \"x\" has 4 columns available but 5 columns specified"),
        ("select * from t x(a,b,c)", "table \"x\" has 2 columns available but 3 columns specified"),
        ("select * from generate_series(1,2) x(a,b)", "table \"x\" has 1 columns available but 2 columns specified"),
        ("select * from (select 1, 2) x(a,b,c)", "table \"x\" has 2 columns available but 3 columns specified"),
    ] {
        let (code, got) = fails(&db, &mut s, q);
        assert_eq!((code, got.as_str()), ("42P10", message), "{q}");
    }
}

#[test]
fn a_column_of_the_relation_wins_over_a_system_column_of_the_name() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int, b int); insert into t values (1, 2)");
    // (the alias list's `ctid` is pg_am's oid, heap's 2 the first; t's is its column a)
    assert_eq!(one(&db, &mut s, "select ctid from pg_am x(ctid) order by 1 limit 1"), "2");
    assert_eq!(one(&db, &mut s, "select x.ctid from pg_am x(ctid) order by 1 limit 1"), "2");
    assert_eq!(one(&db, &mut s, "select ctid from t x(ctid)"), "1");
    assert_eq!(one(&db, &mut s, "select tableoid from t x(tableoid)"), "1");
    assert_eq!(one(&db, &mut s, "select x.tableoid from t x(tableoid)"), "1");
    // a name two relations have stays ambiguous, a system column of each, or a column of one and the other's system one
    for q in ["select ctid from pg_am, pg_opclass limit 1", "select ctid from t x(ctid), pg_am limit 1"] {
        let (code, message) = fails(&db, &mut s, q);
        assert_eq!(code, "42702", "{q}: {message}");
        assert!(message.starts_with("column reference \"ctid\" is ambiguous"), "{message}");
    }
}

#[test]
fn a_sql_functions_parameter_named_as_any_system_column_is_the_column_of_a_table() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int); insert into t values (1)");
    // (cmin and cmax are of type cid, which Chronos hasn't as a type of its own: their parameters are text)
    for (col, ty) in [("xmin", "xid"), ("xmax", "xid"), ("cmin", "text"), ("cmax", "text")] {
        run(
            &db,
            &mut s,
            &format!(
                "create function f_{col}({col} {ty}) returns text language sql as 'select {col}::text from t limit 1';
                 create function c_{col}({col} {ty}) returns text language sql as 'select {col}::text from pg_am limit 1';
                 create function v_{col}({col} {ty}) returns text language sql as 'select {col}::text from pg_roles limit 1'"
            ),
        );
        // (a table's, and a catalog table's, is the column; a view has none, so it is the parameter)
        assert_eq!(one(&db, &mut s, &format!("select f_{col}('5') = '5'")), "false", "{col}");
        assert_eq!(one(&db, &mut s, &format!("select c_{col}('5') = '5'")), "false", "{col}");
        assert_eq!(one(&db, &mut s, &format!("select v_{col}('5')")), "5", "{col}");
    }
    // and a PL/pgSQL variable of the name is ambiguous beside a table
    run(
        &db,
        &mut s,
        "create function p_xmin(xmin xid) returns xid language plpgsql as $$ declare r xid; begin select xmin into r from t limit 1; return r; end $$",
    );
    let (code, message) = fails(&db, &mut s, "select p_xmin('5')");
    assert_eq!((code, message.as_str()), ("42702", "column reference \"xmin\" is ambiguous"));
}

#[test]
fn a_catalog_view_has_an_oid_of_its_own() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let shown = run(
        &db,
        &mut s,
        "select 'pg_roles'::regclass::text, 'pg_user'::regclass::text, 'pg_authid'::regclass::text, 1260::regclass::text,
                'pg_tables'::regclass::text, 1259::regclass::text, 12104::regclass::text",
    );
    assert_eq!(shown, [["pg_roles", "pg_user", "pg_authid", "pg_authid", "pg_tables", "pg_class", "pg_settings"]]);
    let oids = run(
        &db,
        &mut s,
        "select 'pg_roles'::regclass::oid, 'pg_shadow'::regclass::oid, 'pg_user'::regclass::oid, 'pg_views'::regclass::oid,
                'pg_tables'::regclass::oid, 'pg_matviews'::regclass::oid, 'pg_indexes'::regclass::oid,
                'pg_sequences'::regclass::oid, 'pg_settings'::regclass::oid",
    );
    assert_eq!(oids, [["12000", "12005", "12014", "12028", "12033", "12038", "12043", "12048", "12104"]]);
    // so a regclass or an oid of one says which it is, for any role; pg_shadow is the superuser's only
    assert_eq!(one(&db, &mut s, "select has_table_privilege('pg_shadow', 'SELECT')"), "true");
    run(&db, &mut s, "create role cc_oids");
    run(&db, &mut s, "set role cc_oids");
    let mine = run(
        &db,
        &mut s,
        "select has_table_privilege('pg_roles'::regclass, 'SELECT'), has_table_privilege('pg_user'::regclass, 'SELECT'),
                has_table_privilege('pg_authid'::regclass, 'SELECT'), has_table_privilege('pg_roles'::regclass::oid, 'SELECT'),
                has_table_privilege('pg_shadow', 'SELECT'), has_table_privilege(12000::oid, 'SELECT')",
    );
    assert_eq!(mine, [["true", "true", "false", "true", "false", "true"]]);
    run(&db, &mut s, "reset role");
    // (an extension's view has no system columns either)
    run(&db, &mut s, "create extension if not exists pg_stat_statements");
    for col in ["ctid", "tableoid"] {
        assert_eq!(fails(&db, &mut s, &format!("select {col} from pg_stat_statements")).0, "42703", "{col}");
    }
}
