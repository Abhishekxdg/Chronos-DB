//! regprocedure input Postgres rejects: a signature without its closing parenthesis.

use chronos::sql::Session;
use chronos::{Db, MAIN};

#[test]
fn a_signature_missing_its_right_parenthesis_is_refused() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for text in ["now(", "abs(integer", "abs(integer, "] {
        let e = s.run(&db, &format!("select '{text}'::regprocedure"), &[]).expect_err(text);
        assert_eq!((e.code, e.message.as_str()), ("22P02", "expected a right parenthesis"), "{text}");
    }
    assert_eq!(s.run(&db, "select 'now()'::regprocedure::text", &[]).unwrap().pop().unwrap().rows.len(), 1);
}

/// A quoted type name may hold the signature's delimiters (a function's own name can't: CREATE refuses it).
#[test]
fn a_quoted_type_name_may_hold_a_comma_or_parenthesis() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, "create schema qq", &[]).unwrap();
    s.run(&db, r#"create type qq."T,u(" as enum ('x')"#, &[]).unwrap();
    s.run(&db, r#"create function qq.g(a qq."T,u(") returns int language sql as 'select 1'"#, &[]).unwrap();
    let sql = r#"select 'qq.g(qq."T,u(")'::regprocedure::text"#;
    let o = s.run(&db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    assert_eq!(o.rows[0][0].to_text(), r#"qq.g(qq."T,u(")"#);
}

fn rows(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    let o = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    o.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")).collect()
}

/// regproc and regprocedure, and their array types, are rows of pg_type with Postgres's oids (as in Postgres 16).
#[test]
fn regproc_types_and_their_arrays_are_in_pg_type() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    assert_eq!(
        rows(
            &db,
            &mut s,
            "select oid, typname, typarray, typelem from pg_type where oid in (24, 1008, 2202, 2207) order by oid"
        ),
        ["24|regproc|1008|0", "1008|_regproc|0|24", "2202|regprocedure|2207|0", "2207|_regprocedure|0|2202"]
    );
    s.run(&db, "create table rt (p regproc, q regprocedure)", &[]).unwrap();
    assert_eq!(
        rows(
            &db,
            &mut s,
            "select a.attname, t.typname from pg_attribute a join pg_type t on t.oid = a.atttypid \
             where a.attrelid = 'rt'::regclass and a.attnum > 0 order by a.attnum"
        ),
        ["p|regproc", "q|regprocedure"]
    );
}

/// A column of the statement's own named "$1" stays the column when parameter 1 is a regproc.
#[test]
fn a_column_named_like_a_parameter_stays_the_column() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, r#"create table tq ("$1" text)"#, &[]).unwrap();
    s.run(&db, "insert into tq values ('col')", &[]).unwrap();
    s.run(&db, r#"prepare q(regproc) as select "$1" from tq where $1 = 'now'::regproc"#, &[]).unwrap();
    assert_eq!(rows(&db, &mut s, "execute q('now')"), ["col"]);
}

/// The same in a SQL function with a regproc parameter: its body's column named "$1" stays the column.
#[test]
fn a_function_body_column_named_like_a_parameter_stays_the_column() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, r#"create table tq ("$1" text)"#, &[]).unwrap();
    s.run(&db, "insert into tq values ('col')", &[]).unwrap();
    s.run(
        &db,
        r#"create function fq(regproc) returns text language sql as $$ select "$1" from tq where $1 = 'now'::regproc $$"#,
        &[],
    )
    .unwrap();
    assert_eq!(rows(&db, &mut s, "select fq('now')"), ["col"]);
}

/// Arrays of regprocs are types of their own, not a text[] (declaring one was refused, 0A000, until they were): a
/// column of one keeps its functions' oids and shows their names.
#[test]
fn a_regproc_array_type_is_a_type() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for (t, written, shown) in [("regproc[]", "{now,int4in}", "{now,int4in}"), ("regprocedure[]", "{1299}", "{now()}")]
    {
        s.run(&db, &format!("create table ra(p {t})"), &[]).unwrap();
        s.run(&db, &format!("insert into ra values ('{written}')"), &[]).unwrap();
        let got = rows(&db, &mut s, "select p::text || ' ' || p::oid[]::text || ' ' || pg_typeof(p) from ra");
        let oids = if t == "regproc[]" { "{1299,42}" } else { "{1299}" };
        assert_eq!(got, [format!("{shown} {oids} {t}")], "{t}");
        s.run(&db, "drop table ra", &[]).unwrap();
    }
}

/// A regproc hash partition key hashes as its unsigned 32-bit oid (Postgres's hashoid), as the int4 with
/// the same 32 bits does, also for functions made here, whose oids are past 2^31 (hashint8 of one under 2^32 is
/// hashoid of it).
#[test]
fn a_regproc_hash_key_hashes_as_its_32_bit_oid() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    let mut oids = Vec::new();
    for f in 0..8 {
        s.run(&db, &format!("create function f{f}() returns int language sql as 'select 1'"), &[]).unwrap();
        let oid: i64 = rows(&db, &mut s, &format!("select 'f{f}'::regproc::oid"))[0].parse().unwrap();
        assert!(oid > i64::from(i32::MAX), "{oid}");
        oids.push(oid);
    }
    let mut part = Vec::new();
    for t in ["regproc", "int"] {
        s.run(&db, &format!("create table h_{t}(k {t}) partition by hash (k)"), &[]).unwrap();
        for r in 0..8 {
            let sql = format!("create table h_{t}_{r} partition of h_{t} for values with (modulus 8, remainder {r})");
            s.run(&db, &sql, &[]).unwrap();
        }
        for (f, oid) in oids.iter().enumerate() {
            let v = if t == "int" { (oid - (1i64 << 32)).to_string() } else { format!("'f{f}'::regproc") };
            s.run(&db, &format!("insert into h_{t} values ({v})"), &[]).unwrap();
        }
        let sql = format!("select tableoid::regclass::text, k::text from h_{t} order by k::bigint");
        part.push(
            rows(&db, &mut s, &sql).iter().map(|r| r.split('|').next().unwrap().replace(t, "")).collect::<Vec<_>>(),
        );
    }
    assert_eq!(part[0], part[1]);
}

/// A row a CHECK, or a NOT NULL, refuses is shown with its regproc[]'s names in the error's DETAIL, as Postgres shows
/// it (it showed the oids), and a relation and a type of one name is 42710 with Postgres's hint.
#[test]
fn a_refused_row_shows_its_regproc_names() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, "create table rk (id int, ps regproc[] check (cardinality(ps) < 2), n int not null)", &[]).unwrap();
    let detail = |e: chronos::sql::SqlError| e.info.and_then(|i| i.detail);
    let e = s.run(&db, "insert into rk values (1, '{now,int4in}', 1)", &[]).expect_err("check");
    assert_eq!(e.code, "23514");
    assert_eq!(detail(e).as_deref(), Some("Failing row contains (1, {now,int4in}, 1)."));
    let e = s.run(&db, "insert into rk values (2, '{now}', null)", &[]).expect_err("not null");
    assert_eq!(e.code, "23502");
    assert_eq!(detail(e).as_deref(), Some("Failing row contains (2, {now}, null)."));
    s.run(&db, "create type rkt as enum ('a')", &[]).unwrap();
    let e = s.run(&db, "create table rkt (a int)", &[]).expect_err("clash");
    assert_eq!(e.code, "42710");
    assert!(e.info.and_then(|i| i.hint).is_some_and(|h| h.starts_with("A relation has an associated type")));
}
