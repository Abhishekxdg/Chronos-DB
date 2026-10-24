//! ALTER TYPE of composite types, as Postgres's alter_table, domain, create_type and typed_table regression files
//! use it: attributes added, dropped, retyped and renamed (and the typed tables of the type with them, given
//! CASCADE), what refuses a change, the type's relation in the catalog, and what DROP TYPE says depends on it.
//! (The registry of composite types is the process's: each test names its own.)

use chronos::sql::{Session, Value, take_notices};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> chronos::sql::SqlError {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => e,
    }
}

fn notices() -> Vec<String> {
    take_notices().into_iter().map(|n| format!("{}: {}", n.level, n.message)).collect()
}

/// A composite type's attributes, as pg_attribute lists its relation's columns: name and type.
fn attributes(db: &Db, s: &mut Session, ty: &str) -> Vec<String> {
    run(
        db,
        s,
        &format!(
            "select attname || ' ' || format_type(atttypid, atttypmod) from pg_attribute \
             where attrelid = (select typrelid from pg_type where typname = '{ty}') and attnum > 0 order by attnum"
        ),
    )
    .into_iter()
    .map(|r| r[0].clone())
    .collect()
}

/// A table's columns, as pg_attribute lists them: name and type.
fn columns(db: &Db, s: &mut Session, table: &str) -> Vec<String> {
    run(
        db,
        s,
        &format!(
            "select attname || ' ' || format_type(atttypid, atttypmod) from pg_attribute \
             where attrelid = '{table}'::regclass and attnum > 0 and not attisdropped order by attnum"
        ),
    )
    .into_iter()
    .map(|r| r[0].clone())
    .collect()
}

#[test]
fn attributes_are_added_dropped_retyped_and_renamed() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type cat_t1 as (a int)");
    assert_eq!(attributes(&db, &mut s, "cat_t1"), ["a integer"]);
    let e = fails(&db, &mut s, "alter type cat_nosuch add attribute b text");
    assert_eq!((e.code, e.message.as_str()), ("42P01", "relation \"cat_nosuch\" does not exist"));
    run(&db, &mut s, "alter type cat_t1 add attribute b text");
    assert_eq!(attributes(&db, &mut s, "cat_t1"), ["a integer", "b text"]);
    let e = fails(&db, &mut s, "alter type cat_t1 add attribute b text");
    assert_eq!((e.code, e.message.as_str()), ("42701", "column \"b\" of relation \"cat_t1\" already exists"));
    run(&db, &mut s, "alter type cat_t1 alter attribute b set data type varchar");
    assert_eq!(attributes(&db, &mut s, "cat_t1"), ["a integer", "b character varying"]);
    run(&db, &mut s, "alter type cat_t1 alter attribute b set data type integer");
    assert_eq!(attributes(&db, &mut s, "cat_t1"), ["a integer", "b integer"]);
    run(&db, &mut s, "alter type cat_t1 drop attribute b");
    assert_eq!(attributes(&db, &mut s, "cat_t1"), ["a integer"]);
    let e = fails(&db, &mut s, "alter type cat_t1 drop attribute c");
    assert_eq!((e.code, e.message.as_str()), ("42703", "column \"c\" of relation \"cat_t1\" does not exist"));
    take_notices();
    run(&db, &mut s, "alter type cat_t1 drop attribute if exists c");
    assert_eq!(notices(), ["NOTICE: column \"c\" of relation \"cat_t1\" does not exist, skipping"]);
    run(&db, &mut s, "alter type cat_t1 drop attribute a, add attribute d boolean");
    assert_eq!(attributes(&db, &mut s, "cat_t1"), ["d boolean"]);
    let e = fails(&db, &mut s, "alter type cat_t1 rename attribute a to aa");
    assert_eq!((e.code, e.message.as_str()), ("42703", "column \"a\" does not exist"));
    run(&db, &mut s, "alter type cat_t1 rename attribute d to dd");
    assert_eq!(attributes(&db, &mut s, "cat_t1"), ["dd boolean"]);
    // (and the values of it are read with the attributes it has now)
    assert_eq!(run(&db, &mut s, "select (row(true)::cat_t1).dd"), [["true"]]);
    // an attribute can't hold the type itself
    let e = fails(&db, &mut s, "alter type cat_t1 add attribute me cat_t1");
    assert_eq!((e.code, e.message.as_str()), ("42P16", "composite type cat_t1 cannot be made a member of itself"));
    run(&db, &mut s, "drop type cat_t1");
}

#[test]
fn a_composite_type_is_a_relation_in_the_catalog() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type rel_t1 as (a int, b varchar(5))");
    let row = run(
        &db,
        &mut s,
        "select t.typtype, t.typcategory, c.relkind, c.reltype = t.oid, c.relnatts, t.typinput, t.typoutput \
         from pg_type t join pg_class c on c.oid = t.typrelid where t.typname = 'rel_t1'",
    );
    assert_eq!(row, [["c", "C", "c", "true", "2", "record_in", "record_out"]]);
    assert_eq!(attributes(&db, &mut s, "rel_t1"), ["a integer", "b character varying(5)"]);
    // COMMENT ON COLUMN of it: one of its attributes
    let e = fails(&db, &mut s, "comment on column rel_t1.nope is 'bad comment'");
    assert_eq!((e.code, e.message.as_str()), ("42703", "column \"nope\" of relation \"rel_t1\" does not exist"));
    run(&db, &mut s, "comment on column rel_t1.a is 'good comment'");
    assert_eq!(
        run(&db, &mut s, "select col_description((select typrelid from pg_type where typname = 'rel_t1'), 1)"),
        [["good comment"]]
    );
    run(&db, &mut s, "comment on column rel_t1.a is null");
    // an enum is no relation
    run(&db, &mut s, "create type rel_mood as enum ('a')");
    assert_eq!(run(&db, &mut s, "select typtype, typrelid from pg_type where typname = 'rel_mood'"), [["e", "0"]]);
}

#[test]
fn two_composite_types_of_the_same_attributes_are_two_types() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type same_t1 as (a int, b text)");
    run(&db, &mut s, "create type same_t2 as (a int, b text)");
    run(&db, &mut s, "create table same_tbl (x int, y same_t2)");
    // only the type the column is of is used by it
    run(&db, &mut s, "alter type same_t1 alter attribute b type varchar");
    let e = fails(&db, &mut s, "alter type same_t2 alter attribute b type varchar");
    assert_eq!(
        (e.code, e.message.as_str()),
        ("0A000", "cannot alter type \"same_t2\" because column \"same_tbl.y\" uses it")
    );
    let e = fails(&db, &mut s, "drop type same_t2");
    assert_eq!(e.code, "2BP01");
    run(&db, &mut s, "drop type same_t1");
    // a type dropped is forgotten: one made again of its name is a new one
    run(&db, &mut s, "create type same_t1 as (z bigint)");
    assert_eq!(attributes(&db, &mut s, "same_t1"), ["z bigint"]);
    // an index expression of the type uses it too
    run(&db, &mut s, "create table same_idx (x int, y text)");
    run(&db, &mut s, "create index same_idx_i on same_idx ((row(x, y)::same_t2))");
    run(&db, &mut s, "drop table same_tbl");
    let e = fails(&db, &mut s, "alter type same_t2 alter attribute b type varchar");
    assert_eq!(e.message, "cannot alter type \"same_t2\" because column \"same_idx_i.row\" uses it");
}

#[test]
fn typed_tables_change_with_cascade() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type typed_t2 as (a int, b text)");
    run(&db, &mut s, "create table typed_tbl2 of typed_t2");
    run(&db, &mut s, "create table typed_tbl2_sub () inherits (typed_tbl2)");
    let e = fails(&db, &mut s, "alter type typed_t2 add attribute c text");
    assert_eq!(e.code, "2BP01");
    assert_eq!(e.message, "cannot alter type \"typed_t2\" because it is the type of a typed table");
    assert_eq!(e.info.and_then(|i| i.hint).as_deref(), Some("Use ALTER ... CASCADE to alter the typed tables too."));
    run(&db, &mut s, "alter type typed_t2 add attribute c text cascade");
    assert_eq!(columns(&db, &mut s, "typed_tbl2"), ["a integer", "b text", "c text"]);
    assert_eq!(fails(&db, &mut s, "alter type typed_t2 alter attribute b type varchar").code, "2BP01");
    run(&db, &mut s, "alter type typed_t2 alter attribute b type varchar cascade");
    assert_eq!(columns(&db, &mut s, "typed_tbl2"), ["a integer", "b character varying", "c text"]);
    assert_eq!(fails(&db, &mut s, "alter type typed_t2 drop attribute b").code, "2BP01");
    run(&db, &mut s, "alter type typed_t2 drop attribute b cascade");
    assert_eq!(fails(&db, &mut s, "alter type typed_t2 rename attribute a to aa").code, "2BP01");
    run(&db, &mut s, "alter type typed_t2 rename attribute a to aa cascade");
    assert_eq!(attributes(&db, &mut s, "typed_t2"), ["aa integer", "c text"]);
    assert_eq!(columns(&db, &mut s, "typed_tbl2"), ["aa integer", "c text"]);
    assert_eq!(columns(&db, &mut s, "typed_tbl2_sub"), ["aa integer", "c text"]);
    run(&db, &mut s, "drop table typed_tbl2_sub, typed_tbl2");
    run(&db, &mut s, "drop type typed_t2");
}

#[test]
fn an_attribute_a_check_reads_is_dropped_with_it_only_with_cascade() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type chk_tx as (a int, b text)");
    run(&db, &mut s, "create table chk_tblx (x int, y chk_tx check ((y).a > 0))");
    let e = fails(&db, &mut s, "alter type chk_tx drop attribute a");
    assert_eq!(e.code, "2BP01");
    assert_eq!(e.message, "cannot drop column a of composite type chk_tx because other objects depend on it");
    let info = e.info.unwrap();
    assert_eq!(
        info.detail.as_deref(),
        Some("constraint chk_tblx_y_check on table chk_tblx depends on column a of composite type chk_tx")
    );
    assert_eq!(info.hint.as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    take_notices();
    run(&db, &mut s, "alter type chk_tx drop attribute a cascade");
    assert_eq!(notices(), ["NOTICE: drop cascades to constraint chk_tblx_y_check on table chk_tblx"]);
    assert_eq!(run(&db, &mut s, "select count(*) from pg_constraint where conname = 'chk_tblx_y_check'"), [["0"]]);
    // a value of the type kept in a table loses the attribute's value with it
    run(&db, &mut s, "create type chk_t3 as (a int, b text)");
    run(&db, &mut s, "create table chk_tbl3 (c chk_t3)");
    run(&db, &mut s, "insert into chk_tbl3 values ('(1,x)')");
    run(&db, &mut s, "alter type chk_t3 drop attribute a, add attribute d int");
    assert_eq!(run(&db, &mut s, "select c::text, (c).b from chk_tbl3"), [["(x,)", "x"]]);
}

#[test]
fn a_domain_over_the_type_has_its_checks_bound_again() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type dom_comp as (r float8, i float8)");
    run(&db, &mut s, "create domain dom_dcomp as dom_comp");
    run(&db, &mut s, "alter domain dom_dcomp add constraint c1 check ((value).r > 0)");
    run(&db, &mut s, "comment on constraint c1 on domain dom_dcomp is 'random commentary'");
    let e = fails(&db, &mut s, "alter type dom_comp alter attribute r type varchar");
    assert_eq!(
        (e.code, e.message.as_str()),
        ("42883", "operator does not exist: character varying > double precision")
    );
    run(&db, &mut s, "alter type dom_comp alter attribute r type bigint");
    let e = fails(&db, &mut s, "alter type dom_comp drop attribute r");
    assert_eq!(e.code, "2BP01");
    assert_eq!(e.message, "cannot drop column r of composite type dom_comp because other objects depend on it");
    assert_eq!(
        e.info.and_then(|i| i.detail).as_deref(),
        Some("constraint c1 depends on column r of composite type dom_comp")
    );
    run(&db, &mut s, "alter type dom_comp drop attribute i");
    assert_eq!(
        run(
            &db,
            &mut s,
            "select conname, obj_description(oid, 'pg_constraint') from pg_constraint \
             where contypid = 'dom_dcomp'::regtype"
        ),
        [["c1", "random commentary"]]
    );
}

#[test]
fn drop_type_names_the_functions_of_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type fn_person as (id int, name text)");
    run(&db, &mut s, "create table fn_persons of fn_person");
    run(&db, &mut s, "create function fn_all() returns setof fn_person language sql as $$ select * from fn_persons $$");
    run(
        &db,
        &mut s,
        "create function fn_tab() returns table (p fn_person) language sql as $$ select null::fn_person $$",
    );
    let e = fails(&db, &mut s, "drop type fn_person restrict");
    let detail = e.info.and_then(|i| i.detail).unwrap_or_default();
    assert!(detail.contains("function fn_all() depends on type fn_person"), "{detail}");
    assert!(detail.contains("function fn_tab() depends on type fn_person"), "{detail}");
    assert!(detail.contains("table fn_persons depends on type fn_person"), "{detail}");
    take_notices();
    run(&db, &mut s, "drop type fn_person cascade");
    assert_eq!(take_notices()[0].message, "drop cascades to 3 other objects");
}

#[test]
fn a_composite_type_is_renamed_and_moved_with_what_names_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create schema mv_one");
    run(&db, &mut s, "create schema mv_two");
    run(&db, &mut s, "create type mv_one.ctype as (f1 int, f2 text)");
    run(
        &db,
        &mut s,
        "create function mv_one.same(mv_one.ctype, mv_one.ctype) returns boolean language sql \
         as 'select $1.f1 is not distinct from $2.f1'",
    );
    run(&db, &mut s, "create table mv_tbl of mv_one.ctype");
    // (already there: nothing to do)
    run(&db, &mut s, "alter type mv_one.ctype set schema mv_one");
    run(&db, &mut s, "alter type mv_one.ctype set schema mv_two");
    assert_eq!(
        run(
            &db,
            &mut s,
            "select n.nspname from pg_type t join pg_namespace n on n.oid = t.typnamespace where typname = 'ctype'"
        ),
        [["mv_two"]]
    );
    assert_eq!(
        run(&db, &mut s, "select mv_one.same(row(1, 'a')::mv_two.ctype, row(1, 'b')::mv_two.ctype)"),
        [["true"]]
    );
    run(&db, &mut s, "alter type mv_two.ctype rename to ctype2");
    assert_eq!(attributes(&db, &mut s, "ctype2"), ["f1 integer", "f2 text"]);
    assert_eq!(
        run(&db, &mut s, "select reloftype::regtype::text from pg_class where relname = 'mv_tbl'"),
        [["mv_two.ctype2"]]
    );
    // (an enum, a domain or a range type is no relation, as Postgres finds none of the name; a table is no
    // composite type)
    run(&db, &mut s, "create type mv_mood as enum ('a')");
    let e = fails(&db, &mut s, "alter type mv_mood add attribute x int");
    assert_eq!((e.code, e.message.as_str()), ("42P01", "relation \"mv_mood\" does not exist"));
    run(&db, &mut s, "create domain mv_dom as int");
    let e = fails(&db, &mut s, "alter type mv_dom drop attribute x");
    assert_eq!((e.code, e.message.as_str()), ("42P01", "relation \"mv_dom\" does not exist"));
    let e = fails(&db, &mut s, "alter type mv_tbl add attribute x int");
    assert_eq!((e.code, e.message.as_str()), ("42809", "\"mv_tbl\" is not a composite type"));
}

#[test]
fn defaults_and_missing_values_of_the_type_follow_its_attributes() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type dflt_c3 as (a int, b text)");
    run(&db, &mut s, "create table dflt_t3 (x int, c dflt_c3 default '(1,x)')");
    run(&db, &mut s, "insert into dflt_t3 values (0, '(2,y)')");
    run(&db, &mut s, "alter type dflt_c3 drop attribute a");
    run(&db, &mut s, "insert into dflt_t3 (x) values (1)");
    assert_eq!(run(&db, &mut s, "select x, c::text from dflt_t3 order by x"), [["0", "(y)"], ["1", "(x)"]]);
    // (the catalog, and other types, are read and changed as before)
    run(&db, &mut s, "select count(*) from pg_type");
    run(&db, &mut s, "create type dflt_other as (a int)");
    run(&db, &mut s, "alter type dflt_other add attribute b text");
    run(&db, &mut s, "alter type dflt_c3 add attribute d int");
    run(&db, &mut s, "insert into dflt_t3 (x) values (2)");
    assert_eq!(run(&db, &mut s, "select c::text from dflt_t3 where x = 2"), [["(x,)"]]);
    // the value ALTER TABLE ... ADD COLUMN ... DEFAULT left the rows there before
    run(&db, &mut s, "create type dflt_c4 as (a int, b text)");
    run(&db, &mut s, "create table dflt_t4 (x int)");
    run(&db, &mut s, "insert into dflt_t4 values (1)");
    run(&db, &mut s, "alter table dflt_t4 add column c dflt_c4 default '(1,x)'");
    run(&db, &mut s, "alter type dflt_c4 add attribute d int");
    run(&db, &mut s, "insert into dflt_t4 (x) values (2)");
    assert_eq!(run(&db, &mut s, "select x, c::text from dflt_t4 order by x"), [["1", "(1,x,)"], ["2", "(1,x,)"]]);
    run(&db, &mut s, "alter type dflt_c4 drop attribute a");
    assert_eq!(run(&db, &mut s, "select x, c::text from dflt_t4 order by x"), [["1", "(x,)"], ["2", "(x,)"]]);
    run(&db, &mut s, "drop table dflt_t3, dflt_t4");
    run(&db, &mut s, "drop type dflt_c3, dflt_c4, dflt_other");
}

#[test]
fn the_index_entries_of_values_of_the_type_are_made_again() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type idx_c9 as (a int, b text)");
    run(&db, &mut s, "create table idx_t9 (x int, c idx_c9)");
    run(&db, &mut s, "insert into idx_t9 select g, row(g, 'v' || g)::idx_c9 from generate_series(1, 50) g");
    run(&db, &mut s, "create index idx_t9_b on idx_t9 (((c).b))");
    run(&db, &mut s, "create unique index idx_t9_u on idx_t9 (((c).b))");
    run(&db, &mut s, "alter type idx_c9 drop attribute a");
    assert_eq!(run(&db, &mut s, "select count(*) from idx_t9 where (c).b = 'v7'"), [["1"]]);
    assert_eq!(fails(&db, &mut s, "insert into idx_t9 values (51, '(v7)')").code, "23505");
    run(&db, &mut s, "alter type idx_c9 add attribute d int");
    assert_eq!(run(&db, &mut s, "select x, c::text from idx_t9 where (c).b = 'v8'"), [["8", "(v8,)"]]);
    assert_eq!(fails(&db, &mut s, "insert into idx_t9 values (52, '(v8,1)')").code, "23505");
    run(&db, &mut s, "insert into idx_t9 values (53, '(v53,1)')");
    assert_eq!(run(&db, &mut s, "select count(*) from idx_t9"), [["51"]]);
    run(&db, &mut s, "drop table idx_t9");
    run(&db, &mut s, "drop type idx_c9");
}

#[test]
fn a_view_or_an_index_reading_an_attribute_keeps_it_from_being_dropped() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type vw_c7 as (a int, b text)");
    run(&db, &mut s, "create table vw_t7 (c vw_c7)");
    run(&db, &mut s, "insert into vw_t7 values ('(1,x)')");
    run(&db, &mut s, "create view vw_v7 as select (c).a from vw_t7");
    let e = fails(&db, &mut s, "alter type vw_c7 drop attribute a");
    assert_eq!(e.code, "2BP01");
    assert_eq!(e.message, "cannot drop column a of composite type vw_c7 because other objects depend on it");
    let info = e.info.unwrap();
    assert_eq!(info.detail.as_deref(), Some("view vw_v7 depends on column a of composite type vw_c7"));
    assert_eq!(info.hint.as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    assert_eq!(run(&db, &mut s, "select * from vw_v7"), [["1"]]);
    run(&db, &mut s, "create index vw_t7_i on vw_t7 (((c).a))");
    let e = fails(&db, &mut s, "alter type vw_c7 drop attribute a");
    let detail = e.info.and_then(|i| i.detail).unwrap_or_default();
    assert!(detail.contains("index vw_t7_i depends on column a of composite type vw_c7"), "{detail}");
    // an attribute nothing reads goes, and the values with it
    run(&db, &mut s, "alter type vw_c7 drop attribute b");
    assert_eq!(run(&db, &mut s, "select c::text from vw_t7"), [["(1)"]]);
    take_notices();
    run(&db, &mut s, "alter type vw_c7 drop attribute a cascade");
    let said = take_notices();
    assert_eq!(said[0].message, "drop cascades to 2 other objects");
    let detail = said[0].detail.clone().unwrap_or_default();
    assert!(detail.contains("drop cascades to view vw_v7"), "{detail}");
    assert!(detail.contains("drop cascades to index vw_t7_i"), "{detail}");
    assert_eq!(fails(&db, &mut s, "select * from vw_v7").code, "42P01");
    assert_eq!(run(&db, &mut s, "select count(*) from pg_indexes where indexname = 'vw_t7_i'"), [["0"]]);
    assert_eq!(run(&db, &mut s, "select c::text from vw_t7"), [["()"]]);
    run(&db, &mut s, "drop table vw_t7");
    run(&db, &mut s, "drop type vw_c7");
}

#[test]
fn an_attribute_renamed_is_renamed_where_checks_and_indexes_read_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type rn_c10 as (a int, b text)");
    run(&db, &mut s, "create table rn_t10 (c rn_c10 check ((c).a > 0))");
    run(&db, &mut s, "create index rn_t10_b on rn_t10 (((c).b))");
    run(&db, &mut s, "insert into rn_t10 values ('(1,x)')");
    run(&db, &mut s, "alter type rn_c10 rename attribute a to aa");
    run(&db, &mut s, "insert into rn_t10 values ('(2,y)')");
    assert_eq!(fails(&db, &mut s, "insert into rn_t10 values ('(0,z)')").code, "23514");
    run(&db, &mut s, "alter type rn_c10 rename attribute b to bb");
    run(&db, &mut s, "insert into rn_t10 values ('(3,w)')");
    assert_eq!(run(&db, &mut s, "select (c).aa from rn_t10 where (c).bb = 'y'"), [["2"]]);
    assert_eq!(run(&db, &mut s, "select count(*) from rn_t10"), [["3"]]);
    // a domain's check over the type too
    run(&db, &mut s, "create domain rn_d10 as rn_c10");
    run(&db, &mut s, "alter domain rn_d10 add constraint rn_k check ((value).aa < 100)");
    run(&db, &mut s, "create table rn_dt (d rn_d10)");
    run(&db, &mut s, "alter type rn_c10 rename attribute aa to aaa");
    run(&db, &mut s, "insert into rn_dt values ('(5,q)')");
    assert_eq!(fails(&db, &mut s, "insert into rn_dt values ('(500,q)')").code, "23514");
    assert_eq!(fails(&db, &mut s, "insert into rn_t10 values ('(-1,v)')").code, "23514");
    // a view's query, which names it, isn't written again: refused
    run(&db, &mut s, "create view rn_v10 as select (c).bb from rn_t10");
    assert_eq!(fails(&db, &mut s, "alter type rn_c10 rename attribute bb to b").code, "0A000");
    assert_eq!(attributes(&db, &mut s, "rn_c10"), ["aaa integer", "bb text"]);
}

#[test]
fn values_of_the_type_inside_another_type_are_not_left_in_its_old_layout() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type arr_c4 as (a int, b text)");
    run(&db, &mut s, "create table arr_t4 (c arr_c4[])");
    run(&db, &mut s, "insert into arr_t4 values ('{\"(1,x)\"}')");
    assert_eq!(fails(&db, &mut s, "alter type arr_c4 drop attribute a").code, "0A000");
    assert_eq!(attributes(&db, &mut s, "arr_c4"), ["a integer", "b text"]);
    run(&db, &mut s, "drop table arr_t4");
    // another composite type with an attribute of it
    run(&db, &mut s, "create type arr_w5 as (inr arr_c4)");
    run(&db, &mut s, "create table arr_t5 (w arr_w5)");
    assert_eq!(fails(&db, &mut s, "alter type arr_c4 add attribute d int").code, "0A000");
    // (a rename moves no value)
    run(&db, &mut s, "alter type arr_c4 rename attribute b to bb");
    run(&db, &mut s, "drop table arr_t5");
    run(&db, &mut s, "alter type arr_c4 drop attribute a");
    assert_eq!(attributes(&db, &mut s, "arr_c4"), ["bb text"]);
}

#[test]
fn a_type_left_with_no_attributes_is_no_shell_type() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create type none_c11 as (a int)");
    run(&db, &mut s, "alter type none_c11 drop attribute a");
    assert!(attributes(&db, &mut s, "none_c11").is_empty());
    let e = fails(&db, &mut s, "create type none_c11 as (z int)");
    assert_eq!((e.code, e.message.as_str()), ("42710", "type \"none_c11\" already exists"));
    run(&db, &mut s, "alter type none_c11 add attribute b text");
    assert_eq!(attributes(&db, &mut s, "none_c11"), ["b text"]);
    // nor is one made of none
    run(&db, &mut s, "create type none_c12 as ()");
    let e = fails(&db, &mut s, "create type none_c12 as (z int)");
    assert_eq!((e.code, e.message.as_str()), ("42710", "type \"none_c12\" already exists"));
    // a shell type has no relation to alter
    run(&db, &mut s, "create type none_shell");
    let e = fails(&db, &mut s, "alter type none_shell add attribute a int");
    assert_eq!((e.code, e.message.as_str()), ("42P01", "relation \"none_shell\" does not exist"));
    run(&db, &mut s, "drop type none_c11, none_c12, none_shell");
}
