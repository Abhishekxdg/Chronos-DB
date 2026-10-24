//! The C functions Postgres's regression suite loads that Chronos runs as their C does: regress.so's
//! test_canonicalize_path, binary_coercible, ttdummy and set_ttdummy, and contrib/spi's autoinc and refint
//! (check_primary_key, check_foreign_key), with Postgres's results, messages and SQLSTATEs; and COMMENT ON TRIGGER,
//! which triggers.sql runs on refint's triggers, refusing a trigger that isn't there and one not the user's own.

use chronos::sql::{Session, SqlError, Value, take_notices};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> SqlError {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => e,
    }
}

fn detail(e: &SqlError) -> Option<String> {
    e.info.as_ref().and_then(|i| i.detail.clone())
}

fn context(e: &SqlError) -> Option<String> {
    e.info.as_ref().and_then(|i| i.context.clone())
}

fn notices() -> Vec<String> {
    take_notices().into_iter().map(|n| format!("{}: {}", n.level, n.message)).collect()
}

#[test]
fn test_canonicalize_path_is_postgres_canonicalize_path() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE FUNCTION test_canonicalize_path(text) RETURNS text AS '/usr/lib/postgresql/regress.so'
         LANGUAGE C STRICT IMMUTABLE",
    );
    // misc_functions.out's cases
    let cases = [
        ("/", "/"),
        ("/./abc/def/", "/abc/def"),
        ("/./../abc/def", "/abc/def"),
        ("/./../../abc/def/", "/abc/def"),
        ("/abc/.././def/ghi", "/def/ghi"),
        ("/abc/./../def/ghi//", "/def/ghi"),
        ("/abc/def/../..", "/"),
        ("/abc/def/../../..", "/"),
        ("/abc/def/../../../../ghi/jkl", "/ghi/jkl"),
        (".", "."),
        ("./", "."),
        ("./abc/..", "."),
        ("abc/../", "."),
        ("abc/../def", "def"),
        ("..", ".."),
        ("../abc/def", "../abc/def"),
        ("../abc/..", ".."),
        ("../abc/../def", "../def"),
        ("../abc/../../def/ghi", "../../def/ghi"),
        ("./abc/./def/.", "abc/def"),
        ("./abc/././def/.", "abc/def"),
        ("./abc/./def/.././ghi/../../../jkl/mno", "../jkl/mno"),
    ];
    for (path, canonical) in cases {
        assert_eq!(one(&db, &mut s, &format!("SELECT test_canonicalize_path('{path}')")), canonical, "{path}");
    }
    assert_eq!(one(&db, &mut s, "SELECT test_canonicalize_path(NULL) IS NULL"), "true");
}

#[test]
fn binary_coercible_is_is_binary_coercible() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE FUNCTION binary_coercible(oid, oid) RETURNS bool AS '/usr/lib/postgresql/regress.so',
         'binary_coercible' LANGUAGE C STRICT STABLE PARALLEL SAFE",
    );
    let coercible = |s: &mut Session, from: &str, to: &str| {
        one(&db, s, &format!("SELECT binary_coercible('{from}'::regtype, '{to}'::regtype)"))
    };
    for (from, to, yes) in [
        ("int4", "int4", true),
        ("int4", "int8", false),
        ("varchar", "text", true),
        ("text", "varchar", true),
        ("int4", "any", true),
        ("int4", "anyelement", true),
        ("int4", "anycompatible", true),
        ("int4[]", "anyarray", true),
        ("int4", "anyarray", false),
        ("int4", "anynonarray", true),
        ("int4[]", "anynonarray", false),
        ("int4range", "anyrange", true),
        ("int4", "anyrange", false),
        ("int4multirange", "anymultirange", true),
        ("int4range", "anycompatiblerange", true),
        ("regproc", "oid", true),
        ("oid", "regproc", true),
        ("int4", "oid", true),
        ("int8", "oid", false),
        ("cidr", "inet", true),
    ] {
        assert_eq!(coercible(&mut s, from, to), if yes { "true" } else { "false" }, "{from} -> {to}");
    }
    assert_eq!(one(&db, &mut s, "SELECT binary_coercible(NULL, 23) IS NULL"), "true");
    // an enum is one of anyenum, a domain its base type
    run(&db, &mut s, "CREATE TYPE mood AS ENUM ('sad', 'happy'); CREATE DOMAIN posint AS int4 CHECK (VALUE > 0)");
    assert_eq!(coercible(&mut s, "mood", "anyenum"), "true");
    assert_eq!(coercible(&mut s, "int4", "anyenum"), "false");
    assert_eq!(coercible(&mut s, "posint", "int4"), "true");
    assert_eq!(coercible(&mut s, "posint", "oid"), "true");
    assert_eq!(coercible(&mut s, "posint", "int8"), "false");
    // a table's row type is a record, its array a record[]
    run(&db, &mut s, "CREATE TABLE pair (a int, b int)");
    assert_eq!(coercible(&mut s, "pair", "record"), "true");
    // (record[]: 2287)
    assert_eq!(one(&db, &mut s, "SELECT binary_coercible('pair[]'::regtype, 2287)"), "true");
    assert_eq!(one(&db, &mut s, "SELECT binary_coercible('int4[]'::regtype, 2287)"), "false");
    assert_eq!(coercible(&mut s, "int4", "record"), "false");
    // a composite type made with CREATE TYPE is a record too, a range type one of anyrange, and an enum with no
    // labels one of anyenum
    run(
        &db,
        &mut s,
        "CREATE TYPE cpx AS (r float8, i float8); CREATE TYPE f8r AS RANGE (subtype = float8);
         CREATE TYPE no_labels AS ENUM ()",
    );
    assert_eq!(coercible(&mut s, "cpx", "record"), "true");
    assert_eq!(one(&db, &mut s, "SELECT binary_coercible('cpx[]'::regtype, 2287)"), "true");
    assert_eq!(coercible(&mut s, "cpx", "anyenum"), "false");
    assert_eq!(coercible(&mut s, "f8r", "anyrange"), "true");
    assert_eq!(coercible(&mut s, "f8r", "anycompatiblerange"), "true");
    assert_eq!(coercible(&mut s, "f8r", "record"), "false");
    assert_eq!(coercible(&mut s, "f8r_multirange", "anymultirange"), "true");
    assert_eq!(coercible(&mut s, "f8r_multirange", "anyrange"), "false");
    assert_eq!(coercible(&mut s, "no_labels", "anyenum"), "true");
    // opr_sanity's checks of the built-in catalogs that use it find nothing amiss
    let checks = [
        "SELECT o1.oid FROM pg_operator AS o1, pg_proc AS p1
         WHERE o1.oprcode = p1.oid AND o1.oprkind = 'b' AND
             (p1.pronargs != 2
              OR NOT binary_coercible(p1.prorettype, o1.oprresult)
              OR NOT binary_coercible(o1.oprleft, p1.proargtypes[0])
              OR NOT binary_coercible(o1.oprright, p1.proargtypes[1]))",
        "SELECT o1.oid FROM pg_operator AS o1, pg_proc AS p1
         WHERE o1.oprcode = p1.oid AND o1.oprkind = 'l' AND
             (p1.pronargs != 1
              OR NOT binary_coercible(p1.prorettype, o1.oprresult)
              OR NOT binary_coercible(o1.oprright, p1.proargtypes[0])
              OR o1.oprleft != 0)",
        "SELECT c.oid FROM pg_cast c, pg_proc p
         WHERE c.castfunc = p.oid AND
             (p.pronargs < 1 OR p.pronargs > 3
              OR NOT (binary_coercible(c.castsource, p.proargtypes[0])
                      OR (c.castsource = 'character'::regtype AND
                          p.proargtypes[0] = 'text'::regtype))
              OR NOT binary_coercible(p.prorettype, c.casttarget))",
        "SELECT a.aggfnoid::oid FROM pg_aggregate AS a, pg_proc AS p, pg_proc AS ptr
         WHERE a.aggfnoid = p.oid AND a.aggtransfn = ptr.oid AND
             (ptr.proretset
              OR NOT (ptr.pronargs =
                      CASE WHEN a.aggkind = 'n' THEN p.pronargs + 1
                      ELSE greatest(p.pronargs - a.aggnumdirectargs, 1) + 1 END)
              OR NOT binary_coercible(ptr.prorettype, a.aggtranstype)
              OR NOT binary_coercible(a.aggtranstype, ptr.proargtypes[0])
              OR (p.pronargs > 0 AND NOT binary_coercible(p.proargtypes[0], ptr.proargtypes[1]))
              OR (p.pronargs > 1 AND NOT binary_coercible(p.proargtypes[1], ptr.proargtypes[2]))
              OR (p.pronargs > 2 AND NOT binary_coercible(p.proargtypes[2], ptr.proargtypes[3]))
              OR (p.pronargs > 3 AND NOT binary_coercible(p.proargtypes[3], ptr.proargtypes[4]))
              OR (p.pronargs > 4))",
        "SELECT a.aggfnoid::oid FROM pg_aggregate AS a, pg_proc AS p, pg_proc AS pfn
         WHERE a.aggfnoid = p.oid AND a.aggfinalfn = pfn.oid AND
             (pfn.proretset OR
              NOT binary_coercible(pfn.prorettype, p.prorettype) OR
              NOT binary_coercible(a.aggtranstype, pfn.proargtypes[0]) OR
              CASE WHEN a.aggfinalextra THEN pfn.pronargs != p.pronargs + 1
                   ELSE pfn.pronargs != a.aggnumdirectargs + 1 END
              OR (pfn.pronargs > 1 AND NOT binary_coercible(p.proargtypes[0], pfn.proargtypes[1]))
              OR (pfn.pronargs > 2 AND NOT binary_coercible(p.proargtypes[1], pfn.proargtypes[2]))
              OR (pfn.pronargs > 3 AND NOT binary_coercible(p.proargtypes[2], pfn.proargtypes[3]))
              OR (pfn.pronargs > 4))",
        "SELECT a.aggfnoid::oid FROM pg_aggregate AS a, pg_proc AS p, pg_proc AS ptr
         WHERE a.aggfnoid = p.oid AND a.aggtransfn = ptr.oid AND ptr.proisstrict AND
             a.agginitval IS NULL AND NOT binary_coercible(p.proargtypes[0], a.aggtranstype)",
        "SELECT a.aggfnoid FROM pg_aggregate as a, pg_proc as p
         WHERE a.aggcombinefn = p.oid AND
             (p.pronargs != 2 OR p.prorettype != p.proargtypes[0] OR p.prorettype != p.proargtypes[1] OR
              NOT binary_coercible(a.aggtranstype, p.proargtypes[0]))",
        "SELECT c1.opcname FROM pg_opclass AS c1
         WHERE NOT EXISTS(SELECT 1 FROM pg_amop AS a1
                          WHERE a1.amopfamily = c1.opcfamily
                            AND binary_coercible(c1.opcintype, a1.amoplefttype))",
        "SELECT a1.amopfamily FROM pg_amop AS a1
         WHERE NOT EXISTS(SELECT 1 FROM pg_opclass AS c1
                          WHERE c1.opcfamily = a1.amopfamily
                            AND binary_coercible(c1.opcintype, a1.amoplefttype))",
    ];
    for q in checks {
        assert_eq!(run(&db, &mut s, q), Vec::<Vec<String>>::new(), "{q}");
    }
}

/// triggers.sql's refint tables and triggers, as Postgres 16.15's has them (AFTER ROW triggers).
fn refint_tables(db: &Db, s: &mut Session) {
    run(
        db,
        s,
        "CREATE FUNCTION check_primary_key () RETURNS trigger AS '/usr/lib/postgresql/refint.so' LANGUAGE C;
         CREATE FUNCTION check_foreign_key () RETURNS trigger AS '/usr/lib/postgresql/refint.so' LANGUAGE C;
         create table pkeys (pkey1 int4 not null, pkey2 text not null);
         create table fkeys (fkey1 int4, fkey2 text, fkey3 int);
         create table fkeys2 (fkey21 int4, fkey22 text, pkey23 int not null);
         insert into pkeys values (10, '1'), (20, '2'), (30, '3'), (40, '4'), (50, '5'), (60, '6');
         create unique index pkeys_i on pkeys (pkey1, pkey2);
         create trigger check_fkeys_pkey_exist after insert or update on fkeys for each row
           execute function check_primary_key ('fkey1', 'fkey2', 'pkeys', 'pkey1', 'pkey2');
         create trigger check_fkeys_pkey2_exist after insert or update on fkeys for each row
           execute function check_primary_key ('fkey3', 'fkeys2', 'pkey23');
         create trigger check_fkeys2_pkey_exist after insert or update on fkeys2 for each row
           execute procedure check_primary_key ('fkey21', 'fkey22', 'pkeys', 'pkey1', 'pkey2');
         create trigger check_pkeys_fkey_cascade after delete or update on pkeys for each row
           execute procedure check_foreign_key (2, 'cascade', 'pkey1', 'pkey2',
             'fkeys', 'fkey1', 'fkey2', 'fkeys2', 'fkey21', 'fkey22');
         create trigger check_fkeys2_fkey_restrict after delete or update on fkeys2 for each row
           execute procedure check_foreign_key (1, 'restrict', 'pkey23', 'fkeys', 'fkey3')",
    );
}

#[test]
fn refint_checks_keys_and_cascades_as_triggers_out_says() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    refint_tables(&db, &mut s);
    // a comment on a trigger: the trigger has to be there
    let e = fails(&db, &mut s, "COMMENT ON TRIGGER check_fkeys2_pkey_bad ON fkeys2 IS 'wrong'");
    assert_eq!(
        (e.code, e.message.as_str()),
        ("42704", "trigger \"check_fkeys2_pkey_bad\" for table \"fkeys2\" does not exist")
    );
    run(&db, &mut s, "COMMENT ON TRIGGER check_fkeys2_pkey_exist ON fkeys2 IS 'right'");
    run(&db, &mut s, "COMMENT ON TRIGGER check_fkeys2_pkey_exist ON fkeys2 IS NULL");
    let e = fails(&db, &mut s, "COMMENT ON TRIGGER check_fkeys2_pkey_exist ON nosuch IS 'x'");
    assert_eq!(e.code, "42P01");
    run(&db, &mut s, "insert into fkeys2 values (10, '1', 1), (30, '3', 2), (40, '4', 5), (50, '5', 3)");
    let e = fails(&db, &mut s, "insert into fkeys2 values (70, '5', 3)");
    assert_eq!((e.code, e.message.as_str()), ("09000", "tuple references non-existent key"));
    assert_eq!(
        detail(&e).as_deref(),
        Some("Trigger \"check_fkeys2_pkey_exist\" found tuple referencing non-existent key in \"pkeys\".")
    );
    assert_eq!(context(&e), None);
    run(&db, &mut s, "insert into fkeys values (10, '1', 2), (30, '3', 3), (40, '4', 2), (50, '5', 2)");
    let e = fails(&db, &mut s, "insert into fkeys values (70, '5', 1)");
    assert_eq!(
        detail(&e).as_deref(),
        Some("Trigger \"check_fkeys_pkey_exist\" found tuple referencing non-existent key in \"pkeys\".")
    );
    let e = fails(&db, &mut s, "insert into fkeys values (60, '6', 4)");
    assert_eq!(
        detail(&e).as_deref(),
        Some("Trigger \"check_fkeys_pkey2_exist\" found tuple referencing non-existent key in \"fkeys2\".")
    );
    // a cascaded delete a restrict refuses: the C function's one CONTEXT line, the statement it ran
    notices();
    let e = fails(&db, &mut s, "delete from pkeys where pkey1 = 30 and pkey2 = '3'");
    assert_eq!(notices(), ["NOTICE: check_pkeys_fkey_cascade: 1 tuple(s) of fkeys are deleted"]);
    assert_eq!(
        (e.code, e.message.as_str()),
        ("09000", "\"check_fkeys2_fkey_restrict\": tuple is referenced in \"fkeys\"")
    );
    assert_eq!(context(&e).as_deref(), Some("SQL statement \"delete from fkeys2 where fkey21 = $1 and fkey22 = $2 \""));
    assert_eq!(one(&db, &mut s, "select count(*) from fkeys where fkey1 = 30"), "1");
    run(&db, &mut s, "delete from pkeys where pkey1 = 40 and pkey2 = '4'");
    assert_eq!(
        notices(),
        [
            "NOTICE: check_pkeys_fkey_cascade: 1 tuple(s) of fkeys are deleted",
            "NOTICE: check_pkeys_fkey_cascade: 1 tuple(s) of fkeys2 are deleted"
        ]
    );
    // an UPDATE of the key gives the referencing rows the new key
    run(&db, &mut s, "update pkeys set pkey1 = 7, pkey2 = '70' where pkey1 = 50 and pkey2 = '5'");
    assert_eq!(
        notices(),
        [
            "NOTICE: check_pkeys_fkey_cascade: 1 tuple(s) of fkeys are deleted",
            "NOTICE: check_pkeys_fkey_cascade: 1 tuple(s) of fkeys2 are deleted"
        ]
    );
    assert_eq!(run(&db, &mut s, "select fkey1, fkey2, fkey3 from fkeys where fkey1 = 7"), [["7", "70", "2"]]);
    assert_eq!(run(&db, &mut s, "select * from fkeys2 where fkey21 = 7"), [["7", "70", "3"]]);
    let e = fails(&db, &mut s, "update pkeys set pkey1 = 7, pkey2 = '70' where pkey1 = 10 and pkey2 = '1'");
    assert_eq!(e.code, "23505");
    // an UPDATE that keeps the key does nothing to the others
    run(&db, &mut s, "update fkeys2 set fkey22 = '70' where pkey23 = 3");
    assert_eq!(notices(), Vec::<String>::new());
    let listed = run(
        &db,
        &mut s,
        "SELECT trigger_name, event_manipulation, action_order, action_timing FROM information_schema.triggers
         WHERE event_object_table in ('pkeys', 'fkeys', 'fkeys2') ORDER BY trigger_name COLLATE \"C\", 2",
    );
    assert_eq!(listed.len(), 10, "{listed:?}");
    assert_eq!(listed[3], ["check_fkeys2_pkey_exist", "UPDATE", "2", "AFTER"]);
}

#[test]
fn refint_refuses_what_its_c_refuses() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    refint_tables(&db, &mut s);
    run(
        &db,
        &mut s,
        "create table odd (a int, b int);
         create trigger odd_pk after insert on odd for each row execute procedure check_primary_key ('a', 'pkeys');
         create trigger odd_fk after delete on odd for each row execute procedure check_foreign_key (1, 'x', 'a', 'pkeys', 'pkey1')",
    );
    let e = fails(&db, &mut s, "insert into odd values (1, 1)");
    assert_eq!(
        (e.code, e.message.as_str()),
        ("XX000", "check_primary_key: odd number of arguments should be specified")
    );
    run(&db, &mut s, "drop trigger odd_pk on odd; insert into odd values (1, 1)");
    let e = fails(&db, &mut s, "delete from odd");
    assert_eq!((e.code, e.message.as_str()), ("XX000", "check_foreign_key: invalid action x"));
    run(
        &db,
        &mut s,
        "drop trigger odd_fk on odd;
         create trigger odd_col after insert on odd for each row execute procedure check_primary_key ('c', 'pkeys', 'pkey1')",
    );
    let e = fails(&db, &mut s, "insert into odd values (2, 2)");
    assert_eq!((e.code, e.message.as_str()), ("42703", "there is no attribute \"c\" in relation \"odd\""));
    // a NULL key is not checked
    run(&db, &mut s, "drop trigger odd_col on odd");
    run(
        &db,
        &mut s,
        "create trigger odd_null after insert on odd for each row execute procedure check_primary_key ('a', 'pkeys', 'pkey1')",
    );
    run(&db, &mut s, "insert into odd values (null, 3)");
    // setnull sets the referencing rows' key to NULL
    run(
        &db,
        &mut s,
        "create table parent (id int); create table child (pid int);
         insert into parent values (1), (2); insert into child values (1), (1), (2);
         create trigger parent_setnull after delete on parent for each row
           execute procedure check_foreign_key (1, 'setnull', 'id', 'child', 'pid')",
    );
    notices();
    run(&db, &mut s, "delete from parent where id = 1");
    assert_eq!(notices(), ["NOTICE: parent_setnull: 2 tuple(s) of child are set to null"]);
    assert_eq!(one(&db, &mut s, "select count(*) from child where pid is null"), "2");
    // its statements are prepared, their tables and columns looked up, before it knows whether the key changed: an
    // UPDATE that keeps the key is refused too where one isn't there
    run(
        &db,
        &mut s,
        "create table lone (id int, v int); insert into lone values (1, 1);
         create trigger lone_fk after update on lone for each row
           execute procedure check_foreign_key (1, 'restrict', 'id', 'nosuch', 'ref')",
    );
    let e = fails(&db, &mut s, "update lone set v = 2");
    assert_eq!(e.code, "42P01");
    assert_eq!(context(&e).as_deref(), Some("SQL statement \"select 1 from nosuch where ref = $1 \""));
    run(
        &db,
        &mut s,
        "drop trigger lone_fk on lone;
         create trigger lone_fk after update on lone for each row
           execute procedure check_foreign_key (1, 'cascade', 'id', 'child', 'nocol')",
    );
    let e = fails(&db, &mut s, "update lone set v = 3");
    assert_eq!((e.code, e.message.as_str()), ("42703", "column \"nocol\" does not exist"));
    assert_eq!(context(&e).as_deref(), Some("SQL statement \"update child set  nocol = '1'  where nocol = $1 \""));
    run(
        &db,
        &mut s,
        "drop trigger lone_fk on lone;
         create trigger lone_fk after update on lone for each row
           execute procedure check_foreign_key (1, 'restrict', 'id', 'child', 'PID')",
    );
    run(&db, &mut s, "update lone set v = 4");
    assert_eq!(one(&db, &mut s, "select v from lone"), "4");
}

/// COMMENT ON TRIGGER: of a trigger the table or view has (a view's INSTEAD OF and statement triggers too), and by
/// the table's owner only, as Postgres's check_object_ownership asks (its message calling the table a relation).
#[test]
fn comment_on_trigger_needs_the_trigger_and_the_tables_owner() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table base (a int, b text); create view vw as select a, b from base;
         create function vw_ins() returns trigger language plpgsql as $$ begin insert into base values (new.a, new.b); return new; end $$;
         create function noop() returns trigger language plpgsql as $$ begin return null; end $$;
         create trigger vw_ti instead of insert on vw for each row execute function vw_ins();
         create trigger vw_stmt after insert on vw for each statement execute function noop();
         create trigger base_t after insert on base for each row execute function noop()",
    );
    run(&db, &mut s, "COMMENT ON TRIGGER vw_ti ON vw IS 'instead'");
    run(&db, &mut s, "COMMENT ON TRIGGER vw_stmt ON vw IS 'after'");
    let e = fails(&db, &mut s, "COMMENT ON TRIGGER vw_nope ON vw IS 'x'");
    assert_eq!((e.code, e.message.as_str()), ("42704", "trigger \"vw_nope\" for table \"vw\" does not exist"));
    let e = fails(&db, &mut s, "COMMENT ON TRIGGER base_t ON vw IS 'x'");
    assert_eq!((e.code, e.message.as_str()), ("42704", "trigger \"base_t\" for table \"vw\" does not exist"));
    run(&db, &mut s, "create role ct_other; set role ct_other");
    for (q, rel) in
        [("COMMENT ON TRIGGER base_t ON base IS 'x'", "base"), ("COMMENT ON TRIGGER vw_ti ON vw IS 'x'", "vw")]
    {
        let e = fails(&db, &mut s, q);
        assert_eq!((e.code, e.message), ("42501", format!("must be owner of relation {rel}")), "{q}");
    }
    run(&db, &mut s, "reset role");
    run(&db, &mut s, "COMMENT ON TRIGGER base_t ON base IS 'mine'");
}

#[test]
fn ttdummy_keeps_past_rows_and_autoinc_numbers_them() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE FUNCTION ttdummy () RETURNS trigger AS '/usr/lib/postgresql/regress.so' LANGUAGE C;
         CREATE FUNCTION set_ttdummy (int4) RETURNS int4 AS '/usr/lib/postgresql/regress.so' LANGUAGE C STRICT;
         CREATE FUNCTION autoinc () RETURNS trigger AS '/usr/lib/postgresql/autoinc.so' LANGUAGE C;
         create sequence ttdummy_seq increment 10 start 0 minvalue 0;
         create table tttest (price_id int4, price_val int4, price_on int4, price_off int4 default 999999);
         create trigger ttdummy before delete or update on tttest for each row
           execute procedure ttdummy (price_on, price_off);
         create trigger ttserial before insert or update on tttest for each row
           execute procedure autoinc (price_on, ttdummy_seq);
         insert into tttest values (1, 1, null);
         insert into tttest values (2, 2, null);
         insert into tttest values (3, 3, 0)",
    );
    let all = |s: &mut Session| run(&db, s, "select * from tttest order by price_on, price_off");
    let rows = |r: &[[&str; 4]]| r.iter().map(|x| x.iter().map(|v| v.to_string()).collect()).collect::<Vec<Vec<_>>>();
    assert_eq!(
        all(&mut s),
        rows(&[["1", "1", "10", "999999"], ["2", "2", "20", "999999"], ["3", "3", "30", "999999"]])
    );
    // a DELETE leaves the row, closed
    run(&db, &mut s, "delete from tttest where price_id = 2");
    assert_eq!(all(&mut s), rows(&[["1", "1", "10", "999999"], ["2", "2", "20", "40"], ["3", "3", "30", "999999"]]));
    // an UPDATE closes the row and starts its new version
    run(&db, &mut s, "update tttest set price_val = 30 where price_id = 3");
    assert_eq!(
        all(&mut s),
        rows(&[
            ["1", "1", "10", "999999"],
            ["2", "2", "20", "40"],
            ["3", "3", "30", "50"],
            ["3", "30", "50", "999999"]
        ])
    );
    // a past row isn't changed
    run(&db, &mut s, "update tttest set price_id = 5 where price_id = 3");
    assert_eq!(
        all(&mut s),
        rows(&[
            ["1", "1", "10", "999999"],
            ["2", "2", "20", "40"],
            ["3", "3", "30", "50"],
            ["3", "30", "50", "60"],
            ["5", "30", "60", "999999"]
        ])
    );
    // set_ttdummy says what it was; off, rows pass as they are
    assert_eq!(one(&db, &mut s, "select set_ttdummy(0)"), "1");
    assert_eq!(one(&db, &mut s, "select set_ttdummy(0)"), "0");
    run(&db, &mut s, "delete from tttest where price_id = 5");
    run(&db, &mut s, "update tttest set price_off = 999999 where price_val = 30");
    run(&db, &mut s, "update tttest set price_id = 5 where price_id = 3");
    assert_eq!(
        all(&mut s),
        rows(&[
            ["1", "1", "10", "999999"],
            ["2", "2", "20", "40"],
            ["5", "3", "30", "50"],
            ["5", "30", "50", "999999"]
        ])
    );
    assert_eq!(one(&db, &mut s, "select set_ttdummy(1)"), "0");
    let e = fails(&db, &mut s, "update tttest set price_on = -1 where price_id = 1");
    assert_eq!(e.code, "0A000");
    assert_eq!(e.message, "ttdummy (tttest): you cannot change price_on and/or price_off columns (use set_ttdummy)");
    assert_eq!(context(&e), None);
    assert_eq!(one(&db, &mut s, "select set_ttdummy(0)"), "1");
    run(&db, &mut s, "update tttest set price_on = -1 where price_id = 1");
    assert_eq!(
        run(&db, &mut s, "select * from tttest where price_on <= 35 and price_off > 35 and price_id = 5"),
        rows(&[["5", "3", "30", "50"]])
    );
    // (on again for the rest: the switch is the session's)
    assert_eq!(one(&db, &mut s, "select set_ttdummy(1)"), "0");
    let e = fails(
        &db,
        &mut s,
        "insert into tttest values (9, 9, 0); create trigger bad before insert on tttest for each row execute procedure ttdummy (price_on, price_off); insert into tttest values (8, 8, 0)",
    );
    assert_eq!((e.code, e.message.as_str()), ("XX000", "ttdummy: cannot process INSERT event"));
}

#[test]
fn autoinc_refuses_what_its_c_refuses() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE FUNCTION autoinc () RETURNS trigger AS '/usr/lib/postgresql/autoinc.so' LANGUAGE C;
         create sequence seq; create table t (id int4, big int8, v text);
         create trigger odd before insert on t for each row execute procedure autoinc (id)",
    );
    let e = fails(&db, &mut s, "insert into t values (null, 1, 'a')");
    assert_eq!((e.code, e.message.as_str()), ("XX000", "autoinc (t): even number gt 0 of arguments was expected"));
    run(
        &db,
        &mut s,
        "drop trigger odd on t; create trigger nocol before insert on t for each row execute procedure autoinc (nope, seq)",
    );
    let e = fails(&db, &mut s, "insert into t values (null, 1, 'a')");
    assert_eq!((e.code, e.message.as_str()), ("09000", "\"t\" has no attribute \"nope\""));
    run(
        &db,
        &mut s,
        "drop trigger nocol on t; create trigger wide before insert on t for each row execute procedure autoinc (big, seq)",
    );
    let e = fails(&db, &mut s, "insert into t values (null, 1, 'a')");
    assert_eq!((e.code, e.message.as_str()), ("09000", "attribute \"big\" of \"t\" must be type INT4"));
    run(
        &db,
        &mut s,
        "drop trigger wide on t; create trigger del before delete on t for each row execute procedure autoinc (id, seq)",
    );
    run(&db, &mut s, "insert into t values (null, 1, 'a')");
    let e = fails(&db, &mut s, "delete from t");
    assert_eq!((e.code, e.message.as_str()), ("XX000", "cannot process DELETE events"));
    run(
        &db,
        &mut s,
        "drop trigger del on t; create trigger num before insert or update on t for each row execute procedure autoinc (id, seq)",
    );
    run(&db, &mut s, "insert into t values (null, 2, 'b'), (0, 3, 'c'), (7, 4, 'd')");
    assert_eq!(run(&db, &mut s, "select id from t where big > 1 order by big"), [["1"], ["2"], ["7"]]);
}

#[test]
fn other_c_functions_are_still_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        "CREATE FUNCTION f() RETURNS trigger AS '/usr/lib/postgresql/other.so', 'check_primary_key' LANGUAGE C",
        "CREATE FUNCTION g(int) RETURNS int AS '/usr/lib/postgresql/regress.so', 'nosuchsymbol' LANGUAGE C",
        "CREATE FUNCTION autoinc(int) RETURNS int AS '/usr/lib/postgresql/autoinc.so' LANGUAGE C",
        "CREATE FUNCTION ttdummy() RETURNS trigger AS '/usr/lib/postgresql/refint.so' LANGUAGE C",
    ] {
        let e = fails(&db, &mut s, q);
        assert_eq!(e.code, "0A000", "{q}");
        assert_eq!(e.message, "language \"c\" isn't supported: functions are LANGUAGE sql or plpgsql", "{q}");
    }
    // the ones there were still are
    run(
        &db,
        &mut s,
        "CREATE FUNCTION reverse_name(name) RETURNS name AS '/usr/lib/postgresql/regress.so' LANGUAGE C STRICT",
    );
    assert_eq!(one(&db, &mut s, "select reverse_name('abc')"), "cba");
}
