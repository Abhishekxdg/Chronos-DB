//! Object addresses of the objects kept by name only, and of Postgres's own casts and operator family members, as
//! object_address.sql reads them: pg_get_object_address finds each, pg_identify_object names it, and
//! pg_identify_object_as_address gives names that find it again. DROP SCHEMA ... CASCADE drops a schema's aggregates
//! and text search objects with it, listed in the order they were made.

use chronos::sql::{Session, Value, take_notices};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

/// pg_identify_object of the object `type`, `names` and `args` address, with whether it comes back the same through
/// pg_identify_object_as_address.
fn identified(db: &Db, s: &mut Session, ty: &str, names: &str, args: &str) -> Vec<String> {
    let q = format!(
        "SELECT (pg_identify_object(a.classid, a.objid, a.objsubid)).*,
             ROW(pg_identify_object(a.classid, a.objid, a.objsubid)) = ROW(pg_identify_object(b.classid, b.objid, b.objsubid))
         FROM pg_get_object_address('{ty}', '{names}', '{args}') a,
             pg_identify_object_as_address(a.classid, a.objid, a.objsubid) i(typ, nms, args),
             pg_get_object_address(i.typ, i.nms, i.args) b"
    );
    run(db, s, &q).pop().unwrap_or_default()
}

fn setup(db: &Db, s: &mut Session) {
    run(
        db,
        s,
        "CREATE SCHEMA addr_nsp;
         SET search_path TO addr_nsp;
         CREATE TEXT SEARCH DICTIONARY addr_ts_dict (template = simple);
         CREATE TEXT SEARCH CONFIGURATION addr_ts_conf (copy = english);
         CREATE TEXT SEARCH TEMPLATE addr_ts_temp (lexize = dsimple_lexize);
         CREATE TEXT SEARCH PARSER addr_ts_prs
             (start = prsd_start, gettoken = prsd_nexttoken, end = prsd_end, lextypes = prsd_lextype);
         CREATE TYPE addr_nsp.genenum AS ENUM ('one', 'two');
         CREATE AGGREGATE addr_nsp.genaggr(int4) (sfunc = int4pl, stype = int4);
         CREATE DOMAIN addr_nsp.gendomain AS int4 CONSTRAINT domconstr CHECK (value > 0);
         CREATE TRANSFORM FOR int LANGUAGE SQL (
             FROM SQL WITH FUNCTION prsd_lextype(internal), TO SQL WITH FUNCTION int4recv(internal))",
    );
}

#[test]
fn the_objects_kept_by_name_have_addresses() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    let t = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        identified(&db, &mut s, "aggregate", "{addr_nsp,genaggr}", "{int4}"),
        t(&["aggregate", "addr_nsp", "", "addr_nsp.genaggr(integer)", "true"])
    );
    assert_eq!(
        identified(&db, &mut s, "text search dictionary", "{addr_ts_dict}", "{}"),
        t(&["text search dictionary", "addr_nsp", "addr_ts_dict", "addr_nsp.addr_ts_dict", "true"])
    );
    assert_eq!(
        identified(&db, &mut s, "text search parser", "{addr_ts_prs}", "{}"),
        t(&["text search parser", "addr_nsp", "addr_ts_prs", "addr_nsp.addr_ts_prs", "true"])
    );
    assert_eq!(
        identified(&db, &mut s, "transform", "{int}", "{sql}"),
        t(&["transform", "", "", "for integer language sql", "true"])
    );
    assert_eq!(
        identified(&db, &mut s, "domain constraint", "{addr_nsp.gendomain}", "{domconstr}"),
        t(&["domain constraint", "addr_nsp", "", "domconstr on addr_nsp.gendomain", "true"])
    );
    // an aggregate's oid is a function's: pg_get_object_address('function', ...) finds it too
    let agg = run(&db, &mut s, "SELECT objid FROM pg_get_object_address('aggregate', '{addr_nsp,genaggr}', '{int4}')");
    let func = run(&db, &mut s, "SELECT objid FROM pg_get_object_address('function', '{addr_nsp,genaggr}', '{int4}')");
    assert_eq!(agg, func);
}

#[test]
fn postgres_s_own_casts_and_family_members_have_addresses() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let t = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        identified(&db, &mut s, "cast", "{int8}", "{int4}"),
        t(&["cast", "", "", "(bigint AS integer)", "true"])
    );
    assert_eq!(
        identified(&db, &mut s, "operator of access method", "{btree,integer_ops,1}", "{integer,integer}"),
        t(&[
            "operator of access method",
            "",
            "",
            "operator 1 (integer, integer) of pg_catalog.integer_ops USING btree",
            "true"
        ])
    );
    assert_eq!(
        identified(&db, &mut s, "function of access method", "{btree,integer_ops,2}", "{integer,integer}"),
        t(&[
            "function of access method",
            "",
            "",
            "function 2 (integer, integer) of pg_catalog.integer_ops USING btree",
            "true"
        ])
    );
}

#[test]
fn a_subscription_has_an_address_and_warns_when_not_connected() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    take_notices();
    run(
        &db,
        &mut s,
        "CREATE SUBSCRIPTION regress_addr_sub CONNECTION '' PUBLICATION bar WITH (connect = false, slot_name = NONE)",
    );
    let warned: Vec<String> = take_notices().into_iter().map(|n| format!("{}: {}", n.level, n.message)).collect();
    assert_eq!(warned, ["WARNING: subscription was created, but is not connected"]);
    let t = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        identified(&db, &mut s, "subscription", "{regress_addr_sub}", "{}"),
        t(&["subscription", "", "regress_addr_sub", "regress_addr_sub", "true"])
    );
}

#[test]
fn drop_schema_cascade_drops_them_in_the_order_they_were_made() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    setup(&db, &mut s);
    take_notices();
    run(&db, &mut s, "DROP SCHEMA addr_nsp CASCADE");
    let n = take_notices().into_iter().find(|n| n.message.starts_with("drop cascades")).expect("a cascade notice");
    assert_eq!(n.message, "drop cascades to 7 other objects");
    assert_eq!(
        n.detail.as_deref().unwrap_or_default(),
        "drop cascades to text search dictionary addr_ts_dict
drop cascades to text search configuration addr_ts_conf
drop cascades to text search template addr_ts_temp
drop cascades to text search parser addr_ts_prs
drop cascades to type genenum
drop cascades to function genaggr(integer)
drop cascades to type gendomain"
    );
    let e = s.run(&db, "SELECT pg_get_object_address('text search dictionary', '{addr_nsp,addr_ts_dict}', '{}')", &[]);
    assert!(e.is_err());
}

#[test]
fn a_moved_aggregate_a_made_types_transform_and_a_missing_schema_on_the_path() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let t = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    // SET SCHEMA moves it: its address is the new schema's, and DROP SCHEMA of the old one leaves it
    run(
        &db,
        &mut s,
        "CREATE SCHEMA a; CREATE SCHEMA b;
         CREATE AGGREGATE a.f(int4) (sfunc = int4pl, stype = int4);
         ALTER AGGREGATE a.f(int4) SET SCHEMA b;
         DROP SCHEMA a",
    );
    assert_eq!(
        identified(&db, &mut s, "aggregate", "{b,f}", "{int4}"),
        t(&["aggregate", "b", "", "b.f(integer)", "true"])
    );
    // transforms of two enums are two transforms, each found by its type
    run(
        &db,
        &mut s,
        "CREATE TYPE e1 AS ENUM ('x'); CREATE TYPE e2 AS ENUM ('y');
         CREATE TRANSFORM FOR e1 LANGUAGE SQL (FROM SQL WITH FUNCTION prsd_lextype(internal));
         CREATE TRANSFORM FOR e2 LANGUAGE SQL (FROM SQL WITH FUNCTION prsd_lextype(internal))",
    );
    assert_eq!(
        identified(&db, &mut s, "transform", "{e2}", "{sql}"),
        t(&["transform", "", "", "for public.e2 language sql", "true"])
    );
    // a search_path schema that isn't there isn't where a new object goes
    run(&db, &mut s, "SET search_path TO nonexistent, public; CREATE TEXT SEARCH DICTIONARY d2 (template = simple)");
    assert_eq!(
        identified(&db, &mut s, "text search dictionary", "{public,d2}", "{}"),
        t(&["text search dictionary", "public", "d2", "public.d2", "true"])
    );
}

#[test]
fn a_transform_of_a_missing_type_is_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let e = s
        .run(&db, "CREATE TRANSFORM FOR missing_type LANGUAGE SQL (FROM SQL WITH FUNCTION prsd_lextype(internal))", &[])
        .unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("42704", "type \"missing_type\" does not exist"));
}
