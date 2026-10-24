//! Function signatures that name relations and columns, as Postgres's regression suite writes them: RETURNS [SETOF]
//! a table found through the search_path, RETURNS SETOF table.column%TYPE, the NOTICE a `%TYPE` reference gives, and
//! a view's dependency on the columns of a table it reads through a function's rows of it.

use chronos::sql::{self, Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

/// The error's code, message, DETAIL and HINT.
fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String, Option<String>, Option<String>) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => {
            let (detail, hint) = e.info.map(|i| (i.detail.clone(), i.hint.clone())).unwrap_or_default();
            (e.code, e.message, detail, hint)
        }
    }
}

/// The NOTICEs statement `q` sends.
fn notices(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    let _ = sql::take_notices();
    run(db, s, q);
    sql::take_notices().into_iter().map(|n| format!("{} {}", n.level, n.message)).collect()
}

fn rows(r: &[&[&str]]) -> Vec<Vec<String>> {
    r.iter().map(|x| x.iter().map(|c| c.to_string()).collect()).collect()
}

#[test]
fn a_table_the_search_path_finds_is_a_functions_rows() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create schema rls_schema; set search_path = rls_schema");
    run(&db, &mut s, "create table rls_t (c text); insert into rls_t values ('invisible to bob')");
    run(&db, &mut s, "create function rls_f() returns setof rls_t stable language sql as $$ select * from rls_t $$");
    assert_eq!(run(&db, &mut s, "select * from rls_f()"), rows(&[&["invisible to bob"]]));
    // (OR REPLACE with the same result: the same rows type)
    run(
        &db,
        &mut s,
        "create or replace function rls_f() returns setof rls_t stable language sql
         as $$ with cte as (select * from rls_t) select * from cte $$",
    );
    assert_eq!(one(&db, &mut s, "select c from rls_f()"), "invisible to bob");
    // one row of the table's row type, not SETOF
    run(&db, &mut s, "create function rls_one() returns rls_t language sql as $$ select * from rls_t $$");
    assert_eq!(one(&db, &mut s, "select c from rls_one()"), "invisible to bob");
    // (a name the search_path doesn't find is still no type)
    let (code, message, ..) =
        fails(&db, &mut s, "create function nope() returns setof no_such_t language sql as $$ select 1 $$");
    assert_eq!((code, message.as_str()), ("42704", "type \"no_such_t\" does not exist"));
    // dropping the table's schema takes the functions of its rows with it
    run(&db, &mut s, "reset search_path");
    run(&db, &mut s, "drop schema rls_schema cascade");
    let (code, ..) = fails(&db, &mut s, "select * from rls_schema.rls_f()");
    assert!(matches!(code, "42883" | "3F000"), "{code}");
}

#[test]
fn a_view_depends_on_the_columns_it_reads_through_a_functions_rows() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create schema testviewschm2; set search_path to testviewschm2, public");
    run(&db, &mut s, "create table tt14t (f1 text, f2 text, f3 text, f4 text)");
    run(&db, &mut s, "insert into tt14t values ('foo', 'bar', 'baz', '42')");
    run(&db, &mut s, "alter table tt14t drop column f2");
    run(
        &db,
        &mut s,
        "create function tt14f() returns setof tt14t as
         $$
         declare
             rec1 record;
         begin
             for rec1 in select * from tt14t
             loop
                 return next rec1;
             end loop;
         end;
         $$
         language plpgsql",
    );
    run(&db, &mut s, "create view tt14v as select t.* from tt14f() t");
    assert_eq!(
        one(&db, &mut s, "select pg_get_viewdef('tt14v', true)"),
        " SELECT f1,\n    f3,\n    f4\n   FROM tt14f() t(f1, f3, f4);"
    );
    assert_eq!(run(&db, &mut s, "select * from tt14v"), rows(&[&["foo", "baz", "42"]]));
    let (code, message, detail, hint) = fails(&db, &mut s, "alter table tt14t drop column f3");
    assert_eq!(code, "2BP01");
    assert_eq!(message, "cannot drop column f3 of table tt14t because other objects depend on it");
    assert_eq!(detail.as_deref(), Some("view tt14v depends on column f3 of table tt14t"));
    assert_eq!(hint.as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    let (code, message, detail, _) =
        fails(&db, &mut s, "alter table tt14t alter column f4 type integer using f4::integer");
    assert_eq!(code, "0A000");
    assert_eq!(message, "cannot alter type of a column used by a view or rule");
    assert_eq!(detail.as_deref(), Some("rule _RETURN on view tt14v depends on column \"f4\""));
    // a view reading only some of the columns depends on those alone
    run(&db, &mut s, "drop view tt14v");
    run(&db, &mut s, "create view tt14v as select t.f1, t.f4 from tt14f() t");
    assert_eq!(
        one(&db, &mut s, "select pg_get_viewdef('tt14v', true)"),
        " SELECT f1,\n    f4\n   FROM tt14f() t(f1, f3, f4);"
    );
    assert_eq!(run(&db, &mut s, "select * from tt14v"), rows(&[&["foo", "42"]]));
    run(&db, &mut s, "alter table tt14t drop column f3");
    assert_eq!(
        one(&db, &mut s, "select pg_get_viewdef('tt14v', true)"),
        " SELECT f1,\n    f4\n   FROM tt14f() t(f1, f4);"
    );
    assert_eq!(run(&db, &mut s, "select * from tt14v"), rows(&[&["foo", "42"]]));
    let (code, ..) = fails(&db, &mut s, "alter table tt14t alter column f1 type varchar(10)");
    assert_eq!(code, "0A000");
}

/// A view over a function's rows of a table that reads some of its columns, or none (`count(*)`), keeps the call
/// as it was written: a column it doesn't read can go, as in Postgres, and the view still reads the rest.
#[test]
fn a_view_over_a_functions_rows_outlives_a_column_it_does_not_read() {
    for schema in [None, Some("testviewschm2")] {
        let (db, mut s) = (Db::new(), Session::new(MAIN));
        if let Some(sc) = schema {
            run(&db, &mut s, &format!("create schema {sc}; set search_path to {sc}, public"));
        }
        run(&db, &mut s, "create table tt14t (f1 text, f3 text, f4 text)");
        run(&db, &mut s, "insert into tt14t values ('foo', 'baz', '42')");
        run(&db, &mut s, "create table other (o1 int)");
        run(&db, &mut s, "insert into other values (7)");
        run(&db, &mut s, "create function tt14f() returns setof tt14t language sql as $$ select * from tt14t $$");
        run(&db, &mut s, "create view vc as select count(*) from tt14f() t");
        run(&db, &mut s, "create view vo as select t.f1, o.* from tt14f() t, other o");
        run(&db, &mut s, "alter table tt14t drop column f3");
        assert_eq!(run(&db, &mut s, "select * from vc"), rows(&[&["1"]]), "{schema:?}");
        assert_eq!(
            one(&db, &mut s, "select pg_get_viewdef('vc', true)"),
            " SELECT count(*) AS count\n   FROM tt14f() t(f1, f4);",
            "{schema:?}"
        );
        assert_eq!(run(&db, &mut s, "select * from vo"), rows(&[&["foo", "7"]]), "{schema:?}");
        run(&db, &mut s, "alter table tt14t drop column f4");
        assert_eq!(run(&db, &mut s, "select * from vo"), rows(&[&["foo", "7"]]), "{schema:?}");
        assert_eq!(
            one(&db, &mut s, "select pg_get_viewdef('vo', true)"),
            " SELECT t.f1,\n    o.o1\n   FROM tt14f() t(f1),\n    other o;",
            "{schema:?}"
        );
        assert_eq!(run(&db, &mut s, "select * from vc"), rows(&[&["1"]]), "{schema:?}");
    }
}

#[test]
fn setof_a_columns_type_and_the_notices_of_type_references() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table partitioned_table (a int, b text) partition by list (a)");
    run(&db, &mut s, "create table pt_part1 partition of partitioned_table for values in (1)");
    run(&db, &mut s, "create table pt_part2 partition of partitioned_table for values in (2)");
    run(&db, &mut s, "insert into partitioned_table values (1, 'Row 1'), (2, 'Row 2')");
    let said = notices(
        &db,
        &mut s,
        "create or replace function list_partitioned_table()
         returns setof partitioned_table.a%TYPE as $$
         declare
             row partitioned_table%ROWTYPE;
             a_val partitioned_table.a%TYPE;
         begin
             for row in select * from partitioned_table order by a loop
                 a_val := row.a;
                 return next a_val;
             end loop;
             return;
         end; $$ language plpgsql",
    );
    assert_eq!(said, ["NOTICE type reference partitioned_table.a%TYPE converted to integer"]);
    assert_eq!(run(&db, &mut s, "select * from list_partitioned_table() as t"), rows(&[&["1"], &["2"]]));
    // a parameter's first, then the result's; the reference as written, the type by its SQL name
    run(&db, &mut s, "create table hobbies_r (name text, person varchar(20))");
    run(&db, &mut s, "insert into hobbies_r values ('posthacking', 'mike')");
    let said = notices(
        &db,
        &mut s,
        "create function hobbies_by_name(hobbies_r.name%type) returns hobbies_r.person%TYPE
         as 'select person from hobbies_r where name = $1' language sql",
    );
    assert_eq!(
        said,
        [
            "NOTICE type reference hobbies_r.name%TYPE converted to text",
            "NOTICE type reference hobbies_r.person%TYPE converted to character varying",
        ]
    );
    assert_eq!(one(&db, &mut s, "select hobbies_by_name('posthacking')"), "mike");
    // an array's type with its `[]`, varchar(n)[] and char(n)[] too
    run(&db, &mut s, "create table nt (v varchar(5)[], c char(3)[], i int[])");
    let said = notices(
        &db,
        &mut s,
        "create function n1(nt.v%type, nt.c%type, nt.i%type) returns int language sql as $$ select 1 $$",
    );
    assert_eq!(
        said,
        [
            "NOTICE type reference nt.v%TYPE converted to character varying[]",
            "NOTICE type reference nt.c%TYPE converted to character[]",
            "NOTICE type reference nt.i%TYPE converted to integer[]",
        ]
    );
    // (a function without one says nothing)
    assert!(notices(&db, &mut s, "create function plain_f() returns int language sql as $$ select 1 $$").is_empty());
}
