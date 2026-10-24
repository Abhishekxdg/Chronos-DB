//! Forms Postgres's regression suite uses that Chronos took differently: a call named after a type as that type's
//! cast, EXPLAIN and SET in PL/pgSQL, a table's row type as a function's parameter or result, the regression
//! library's small C functions, and a permanent table refused as a child of a temporary one.

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

#[test]
fn a_call_named_after_a_type_is_its_cast() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(one(&db, &mut s, "select box('4,4,4,4')"), "(4,4),(4,4)");
    assert_eq!(one(&db, &mut s, "select point('1, 2')"), "(1,2)");
    assert_eq!(one(&db, &mut s, "select tsquery('bbb & ddd')"), "'bbb' & 'ddd'");
    // (not of a value that isn't a quoted literal: Postgres has no tsquery(integer))
    let (code, _) = fails(&db, &mut s, "select tsquery(42)");
    assert_eq!(code, "42883");
    // (another function of the name, taking other arguments, doesn't stop it)
    run(&db, &mut s, "create function tsquery(int, int) returns int language sql as $$ select $1 + $2 $$");
    assert_eq!(one(&db, &mut s, "select tsquery('bbb & ddd')"), "'bbb' & 'ddd'");
    assert_eq!(one(&db, &mut s, "select tsquery(1, 2)"), "3");
}

#[test]
fn explain_and_set_run_in_plpgsql() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int); insert into t values (1), (2)");
    run(
        &db,
        &mut s,
        "create function plan() returns setof text language plpgsql as $$
         declare ln text;
         begin
           for ln in explain (analyze, timing off, summary off, costs off) select * from t loop
             return next ln;
           end loop;
         end $$",
    );
    let lines = run(&db, &mut s, "select * from plan()");
    assert!(lines.iter().any(|l| l[0].contains("Seq Scan on t")), "{lines:?}");
    run(
        &db,
        &mut s,
        "create function wide() returns text language plpgsql as $$
         begin set work_mem = '7MB'; return current_setting('work_mem'); end $$",
    );
    assert_eq!(one(&db, &mut s, "select wide()"), "7MB");
}

#[test]
fn a_table_row_type_is_a_parameter_and_a_result() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table pair (q1 bigint, q2 bigint); insert into pair values (1, 2)");
    run(&db, &mut s, "create function second(p pair) returns bigint language sql as $$ select p.q2 $$");
    assert_eq!(one(&db, &mut s, "select second(t) from pair t"), "2");
    run(&db, &mut s, "create function mk(bigint, bigint) returns pair language sql as $$ select row($1, $2)::pair $$");
    assert_eq!(run(&db, &mut s, "select * from mk(3, 4)"), vec![vec!["3".to_string(), "4".to_string()]]);
    run(&db, &mut s, "create table one_col (f1 int)");
    run(&db, &mut s, "create function mk1(int) returns one_col language sql as $$ select row($1)::one_col $$");
    assert_eq!(one(&db, &mut s, "select * from mk1(42)"), "42");
    // beside another function of the name and as many arguments, the whole row picks the row-type one
    run(&db, &mut s, "create function second(p int) returns bigint language sql as $$ select -1::bigint $$");
    assert_eq!(one(&db, &mut s, "select second(t) from pair t"), "2");
    assert_eq!(one(&db, &mut s, "select second(7)"), "-1");
    // (a column of a table's row type is still refused, as before: its values aren't stored as rows yet)
    let (code, _) = fails(&db, &mut s, "create table holder (x pair)");
    assert_eq!(code, "42704");
}

#[test]
fn the_regression_librarys_small_functions() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let lib = "/x/regress.so";
    run(&db, &mut s, &format!("create function reverse_name(name) returns name as '{lib}' language c strict"));
    assert_eq!(one(&db, &mut s, "select reverse_name('abc')"), "cba");
    run(&db, &mut s, "create table emp (name text, salary int); insert into emp values ('a', 700), ('b', 600)");
    run(&db, &mut s, &format!("create function overpaid(emp) returns bool as '{lib}' language c strict"));
    assert_eq!(
        run(&db, &mut s, "select name, overpaid(emp.*) from emp order by name"),
        vec![vec!["a".to_string(), "true".to_string()], vec!["b".to_string(), "false".to_string()]]
    );
    run(&db, &mut s, &format!("create function trigger_return_old() returns trigger as '{lib}' language c"));
    run(&db, &mut s, "create table trig (f1 int, f2 text)");
    run(
        &db,
        &mut s,
        "create trigger keep before insert or update or delete on trig for each row execute procedure trigger_return_old()",
    );
    run(&db, &mut s, "insert into trig values (1, 'foo')");
    run(&db, &mut s, "update trig set f2 = f2 || 'bar'");
    // (INSERT keeps the new row; UPDATE gives back the old one, so nothing changes)
    assert_eq!(run(&db, &mut s, "select * from trig"), vec![vec!["1".to_string(), "foo".to_string()]]);
}

#[test]
fn a_temporary_parent_named_plainly_or_in_pg_temp_is_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table parent (a int)");
    run(&db, &mut s, "create temp table parent (a int)");
    // (the temporary one is meant, plain or as pg_temp.parent, as Postgres's search_path has pg_temp first)
    let (code, _) = fails(&db, &mut s, "create table child1 () inherits (parent)");
    assert_eq!(code, "42809");
    let (code, _) = fails(&db, &mut s, "create table child3 () inherits (pg_temp.parent)");
    assert_eq!(code, "42809");
    run(&db, &mut s, "create temp table parted (a int) partition by list (a)");
    let (code, _) = fails(&db, &mut s, "create table part2 partition of pg_temp.parted for values in (2)");
    assert_eq!(code, "42809");
}

#[test]
fn a_permanent_table_is_no_child_of_a_temporary_one() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create temp table temp_parted (a int) partition by list (a)");
    let (code, msg) = fails(&db, &mut s, "create table perm_part partition of temp_parted default");
    assert_eq!(code, "42809");
    assert_eq!(msg, "cannot create a permanent relation as partition of temporary relation \"temp_parted\"");
    run(&db, &mut s, "create temp table temp_parent (a int)");
    let (code, msg) = fails(&db, &mut s, "create table perm_child () inherits (temp_parent)");
    assert_eq!(code, "42809");
    assert_eq!(msg, "cannot inherit from temporary relation \"temp_parent\"");
    // and the session goes on: the refused table left nothing behind
    run(&db, &mut s, "drop table temp_parted cascade");
    run(&db, &mut s, "create table after_it (a int)");
    assert_eq!(one(&db, &mut s, "select count(*) from after_it"), "0");
}

#[test]
fn a_function_of_a_table_row_type_depends_on_the_table() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int); insert into t values (5)");
    run(&db, &mut s, "create function f(x t) returns int language sql as $$ select x.a $$");
    let (code, msg) = fails(&db, &mut s, "drop table t");
    assert_eq!(code, "2BP01");
    assert_eq!(msg, "cannot drop table t because other objects depend on it");
    // renamed, the function follows it
    run(&db, &mut s, "alter table t rename to u");
    assert_eq!(one(&db, &mut s, "select f(u) from u"), "5");
    run(&db, &mut s, "create function rows_of_u() returns setof u language sql as $$ select * from u $$");
    run(&db, &mut s, "drop table u cascade");
    let (code, _) = fails(&db, &mut s, "select f(null)");
    assert_eq!(code, "42883");
    let (code, _) = fails(&db, &mut s, "select * from rows_of_u()");
    assert_eq!(code, "42883");
}

#[test]
fn a_one_text_column_row_type_is_told_by_its_type() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table tx (x text)");
    run(&db, &mut s, "create function mk(text) returns tx language sql as $$ select row($1)::tx $$");
    assert_eq!(one(&db, &mut s, "select * from mk('abc')"), "abc");
    // (a text value that looks like a record stays as it is)
    run(&db, &mut s, "create function plain(text) returns tx language sql as $$ select $1 $$");
    assert_eq!(one(&db, &mut s, "select * from plain('(abc)')"), "(abc)");
}

#[test]
fn a_qualified_name_of_another_schema_is_no_type_cast() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create schema app");
    let (code, _) = fails(&db, &mut s, "select app.tsquery('a & b')");
    assert_eq!(code, "42883");
}
