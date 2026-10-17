//! `UPDATE t SET a[i] = v` and `SET a[lo:hi] = v`. Postgres's answers are compared in tests/pgdiff.rs
//! (`update_sets_an_array_element_or_slice`); these run without one. Also assignment through fields and chains
//! (`SET r.f = v`, `INSERT INTO t (a[1], r.f)`, a jsonb's `j['k'][0] = v`) and the subscripts and fields
//! expressions read (`$1.f`, `(r).f.g`, `p[0]` of a point, `(now())[1]` refused), with Postgres 16's answers.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

/// What a query gives back, a row a string, its columns joined with `|` and booleans as `t`/`f`
/// the way psql prints them.
fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    let text = |v: &Value| match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Value::Bytea(b) => format!("\\x{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        v => v.to_text(),
    };
    out.last().unwrap().rows.iter().map(|r| r.iter().map(text).collect::<Vec<_>>().join("|")).collect()
}

/// Statements run in order in one session: a query's rows, or the SQLSTATE of the statement
/// Postgres refuses.
fn script(steps: &[(&str, Result<&[&str], &str>)]) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, want) in steps {
        match want {
            Ok(rows_want) => assert_eq!(&rows(&mut s, &db, q), rows_want, "{q}"),
            Err(code) => {
                let e = s.run(&db, q, &[]).expect_err(q);
                assert_eq!(&e.code, code, "{q}: {}", e.message);
            }
        }
    }
}

#[test]
fn update_sets_an_array_element_or_slice() {
    script(&[
        (r"create table ar (id int primary key, a int[], t text[])", Ok(&[])),
        (r"insert into ar values (1, '{1,2,3}', '{x,y}'), (2, null, null), (3, '{}', '{}')", Ok(&[])),
        (r"update ar set a[2] = 20 where id = 1", Ok(&[])),
        (r"select a from ar where id=1", Ok(&[r"{1,20,3}"])),
        (r"update ar set a[5] = 9 where id = 1", Ok(&[])),
        (r"select a from ar where id=1", Ok(&[r"{1,20,3,NULL,9}"])),
        (r"update ar set t[1] = 'z', a[1] = a[1] + 1 where id = 1", Ok(&[])),
        (r"select a, t from ar where id=1", Ok(&[r"{2,20,3,NULL,9}|{z,y}"])),
        (r"update ar set a[1:2] = '{7,8}' where id = 1", Ok(&[])),
        (r"select a from ar where id=1", Ok(&[r"{7,8,3,NULL,9}"])),
        (r"update ar set a[1:3] = '{1}' where id = 1", Err(r"2202E")),
        (r"update ar set a[2:3] = array[7,8,9] where id = 1", Ok(&[])),
        (r"select a from ar where id=1", Ok(&[r"{7,7,8,NULL,9}"])),
        (r"update ar set a[0] = 1 where id = 1", Ok(&[])),
        (r"select a, array_lower(a,1), array_upper(a,1) from ar where id=1", Ok(&[r"[0:5]={1,7,7,8,NULL,9}|0|5"])),
        (r"update ar set a[2] = 5 where id = 2", Ok(&[])),
        (r"select a, array_lower(a,1) from ar where id=2", Ok(&[r"[2:2]={5}|2"])),
        (r"update ar set a[3] = 5 where id = 3", Ok(&[])),
        (r"select a, array_lower(a,1) from ar where id=3", Ok(&[r"[3:3]={5}|3"])),
        (r"update ar set a[-2] = 5 where id = 1", Ok(&[])),
        (r"select a, array_lower(a,1) from ar where id=1", Ok(&[r"[-2:5]={5,NULL,1,7,7,8,NULL,9}|-2"])),
        (r"update ar set a[null] = 5 where id = 1", Err(r"22004")),
        (r"update ar set a[1] = null where id = 1", Ok(&[])),
        (r"select a from ar where id=1", Ok(&[r"[-2:5]={5,NULL,1,NULL,7,8,NULL,9}"])),
        (r"update ar set a[3:1] = '{1,2}' where id = 1", Err(r"2202E")),
        (r"update ar set a[2:3] = null where id = 1", Ok(&[])),
        (r"select a from ar where id=1", Ok(&[r"[-2:5]={5,NULL,1,NULL,7,8,NULL,9}"])),
        (r"update ar set a[1] = 1, a[2] = 2 where id = 1", Ok(&[])),
        (r"select a from ar where id=1", Ok(&[r"[-2:5]={5,NULL,1,1,2,8,NULL,9}"])),
        (r"select id, a[1], a[2] from ar order by id", Ok(&[r"1|1|2", r"2||5", r"3||"])),
        (r"update ar set a[1] = 1, a = '{}' where id = 1", Err(r"42601")),
        (r"update ar set a = '{}', a[1] = 1 where id = 1", Err(r"42601")),
        (r"update ar set a[1] = 5 where id = 2 returning id, a", Ok(&[r"2|{5,5}"])),
        (r"update ar set t[2] = 'q' where id = 3 returning t", Ok(&[r"[2:2]={q}"])),
    ]);
}

#[test]
fn update_and_insert_assign_through_fields_and_chains() {
    script(&[
        (r"create type aa_cx as (r float8, i float8)", Ok(&[])),
        (r"create type aa_quad as (c1 aa_cx, c2 aa_cx)", Ok(&[])),
        (r"create table qt (f1 int primary key, q aa_quad)", Ok(&[])),
        (r#"insert into qt values (1, '("(1,2)","(3,4)")')"#, Ok(&[])),
        // a field's field, read
        (r"select (q).c1.i, ((q).c2).r from qt", Ok(&[r"2|3"])),
        (r"update qt set q.c1.r = 12 where f1 = 1", Ok(&[])),
        (r"select (q).c1 from qt", Ok(&[r"(12,2)"])),
        // two assignments to one column: the second changes what the first made
        (r"update qt set q.c2.i = 5, q.c2.r = 6", Ok(&[])),
        (r"select q from qt", Ok(&[r#"("(12,2)","(6,5)")"#])),
        (r"update qt set q.c1 = default", Err(r"0A000")),
        (r"update qt set q.c1.r = 1, q = null", Err(r"42601")),
        // INSERT's column list
        (r"create type aa_itype as (if1 int, if2 text[])", Ok(&[])),
        (r"create table it (id serial primary key, f2 int[], f3 aa_itype)", Ok(&[])),
        (r"insert into it (f2[1], f2[2]) values (1, 2), (3, 4)", Ok(&[])),
        (r"insert into it (f2[1], f2[2]) select 7, 8", Ok(&[])),
        (r"insert into it (f3.if1, f3.if2) values (1, '{foo}')", Ok(&[])),
        (r"insert into it (f3.if2[1], f3.if2[2]) values ('foo', 'bar')", Ok(&[])),
        (r"insert into it (f3.if1, f3.if2) select 3, '{baz,quux}'", Ok(&[])),
        (
            r"select f2, f3 from it order by id",
            Ok(&[r"{1,2}|", r"{3,4}|", r"{7,8}|", r"|(1,{foo})", r#"|(,"{foo,bar}")"#, r#"|(3,"{baz,quux}")"#]),
        ),
        (r"insert into it (f2[1], f2[2]) values (1, default)", Err(r"0A000")),
        (r"insert into it (f3.if1, f3.if2) values (1, default)", Err(r"0A000")),
        (r"insert into it (f2, f2[1]) values ('{1}', 2)", Err(r"42701")),
        (r"insert into it (f2[1], f2[2]) values (1)", Err(r"42601")),
        (r"update it set f2[1] = 10, f2[2] = 20 where id = 1 returning f2", Ok(&[r"{10,20}"])),
    ]);
}

#[test]
fn jsonb_subscripts_assign_and_make_their_path() {
    script(&[
        (r"create table js (id int primary key, j jsonb)", Ok(&[])),
        (r#"insert into js values (1, '{}'), (2, '[0]'), (3, null), (4, '{"a": 1}')"#, Ok(&[])),
        (r"update js set j['a'] = '1' where id = 1", Ok(&[])),
        (r#"update js set j['b'][0]['c'] = '"x"' where id = 1"#, Ok(&[])),
        (r"select j from js where id = 1", Ok(&[r#"{"a": 1, "b": [{"c": "x"}]}"#])),
        (r"update js set j[3] = '1' where id = 2", Ok(&[])),
        (r"select j from js where id = 2", Ok(&[r"[0, null, null, 1]"])),
        (r"update js set j[-2] = '5' where id = 2", Ok(&[])),
        (r"select j from js where id = 2", Ok(&[r"[0, null, 5, 1]"])),
        (r"update js set j[-8] = '1' where id = 2", Err(r"22023")),
        (r"update js set j[0] = '5' where id = 3", Ok(&[])),
        (r"select j from js where id = 3", Ok(&[r"[5]"])),
        (r"update js set j['a']['x'] = '1' where id = 4", Err(r"22023")),
        (r"update js set j[null] = '1'", Err(r"22004")),
        (r"select (j)[1:2] from js", Err(r"42804")),
    ]);
}

#[test]
fn fields_of_composite_parameters_and_subscripts_of_any_value() {
    script(&[
        (r"create type aa_fullname as (first text, last text)", Ok(&[])),
        (r"create table aa_people (fn aa_fullname)", Ok(&[])),
        (r"insert into aa_people values ('(Ada,Lovelace)')", Ok(&[])),
        (
            r"create function aa_longname(aa_fullname) returns text language sql as $$select $1.first || ' ' || $1.last$$",
            Ok(&[]),
        ),
        (r"create function aa_first(aa_fullname) returns text language sql strict as 'select $1.first'", Ok(&[])),
        (
            r"create function aa_last(aa_fullname) returns text language plpgsql as $$begin return $1.last; end$$",
            Ok(&[]),
        ),
        (r"create function aa_named(p aa_fullname) returns text language sql strict as 'select (p).last'", Ok(&[])),
        (
            r"select aa_longname(fn), aa_first(fn), aa_last(fn), aa_named(fn) from aa_people",
            Ok(&[r"Ada Lovelace|Ada|Lovelace|Lovelace"]),
        ),
        // Postgres 16: cannot subscript type timestamp with time zone because it does not support subscripting
        (r"select (now())[1]", Err(r"42804")),
        (r"select (1)[1]", Err(r"42804")),
        (r"select (now())[1:2]", Err(r"42804")),
        // a point and a line read as arrays of floats from 0, a box and an lseg as arrays of points; null past the end
        (r"select ('(1,2)'::point)[0], ('(1,2)'::point)[1], ('(1,2)'::point)[2]", Ok(&[r"1|2|"])),
        (r"select ('(3,4),(1,2)'::box)[0], ('(1,2),(3,4)'::box)[1]", Ok(&[r"(3,4)|(1,2)"])),
        (r"select (('(3,4),(1,2)'::box)[1])[0]", Ok(&[r"1"])),
        (r"select ('[(1,2),(3,4)]'::lseg)[1]", Ok(&[r"(3,4)"])),
        (r"select ('{1,2,3}'::line)[2]", Ok(&[r"3"])),
    ]);
}
