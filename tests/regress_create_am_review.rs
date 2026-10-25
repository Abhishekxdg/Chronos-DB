//! Review of CREATE ACCESS METHOD and DROP ACCESS METHOD (tests/regress_create_am.rs): who may drop one, the handler
//! names Postgres refuses, the order DROP lists what depends on an index method in, default_table_access_method's
//! value as Postgres takes it (a name, truncated first), the relations SET ACCESS METHOD refuses, and what is made
//! surviving a reopen, a transaction's ROLLBACK and a world's merge. Each expected value is what Postgres 16.15 gives.

use chronos::sql::{Session, Value, take_notices};
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

fn detail(db: &Db, s: &mut Session, q: &str) -> Option<String> {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => e.info.and_then(|i| i.detail.clone()),
    }
}

/// Relation `name`'s access method, as pg_class.relam names it (empty for none).
fn relam(db: &Db, s: &mut Session, name: &str) -> String {
    one(
        db,
        s,
        &format!(
            "select coalesce(a.amname, '') from pg_class c left join pg_am a on a.oid = c.relam where c.relname = '{name}'"
        ),
    )
}

/// gist2 over gisthandler, with the default class for boxes create_am.sql makes for it.
fn gist2(db: &Db, s: &mut Session) {
    run(db, s, "create access method gist2 type index handler gisthandler");
    run(
        db,
        s,
        "create operator class box_ops default for type box using gist2 as
           operator 3 &&, operator 8 <@,
           function 1 gist_box_consistent(internal, box, smallint, oid, internal),
           function 2 gist_box_union(internal, internal),
           function 5 gist_box_penalty(internal, internal, internal),
           function 6 gist_box_picksplit(internal, internal),
           function 7 gist_box_same(box, box, internal)",
    );
}

#[test]
fn only_a_superuser_drops_an_access_method() {
    // (RemoveObjects' check_object_ownership: an access method is a superuser's, objectaddress.c:2574)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
    run(&db, &mut s, "create table t (a int) using heap2");
    run(&db, &mut s, "create role regress_am_plain");
    run(&db, &mut s, "set role regress_am_plain");
    for q in ["drop access method heap2", "drop access method if exists heap2", "drop access method heap2 cascade"] {
        assert_eq!(fails(&db, &mut s, q), ("42501", "must be superuser".into()), "{q}");
    }
    // (one that isn't there is skipped before anyone is asked)
    take_notices();
    run(&db, &mut s, "drop access method if exists nosuch");
    run(&db, &mut s, "reset role");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_am where amname = 'heap2'"), "1");
    assert_eq!(relam(&db, &mut s, "t"), "heap2");
}

#[test]
fn a_handler_named_with_a_database_is_refused() {
    // (LookupFuncName's DeconstructQualifiedName, namespace.c:3008)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(
        fails(&db, &mut s, "create access method h5 type table handler a.b.heap_tableam_handler"),
        ("0A000", "cross-database references are not implemented: a.b.heap_tableam_handler".into())
    );
    assert_eq!(
        fails(&db, &mut s, "create access method h6 type table handler a.b.c.heap_tableam_handler"),
        ("42601", "improper qualified name (too many dotted names): a.b.c.heap_tableam_handler".into())
    );
    assert_eq!(one(&db, &mut s, "select count(*) from pg_am where amname in ('h5', 'h6')"), "0");
}

#[test]
fn what_depends_on_an_index_method_is_listed_in_the_order_it_was_made() {
    // (findDependentObjects reads pg_depend's rows on the operator class in the order they were written)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    gist2(&db, &mut s);
    run(&db, &mut s, "create table zz (b box)");
    run(&db, &mut s, "create index zz_i on zz using gist2 (b)");
    run(&db, &mut s, "create table aa (b box)");
    run(&db, &mut s, "create index aa_i on aa using gist2 (b)");
    assert_eq!(
        detail(&db, &mut s, "drop access method gist2").as_deref(),
        Some(
            "index zz_i depends on operator class box_ops for access method gist2\n\
             index aa_i depends on operator class box_ops for access method gist2"
        )
    );
    take_notices();
    run(&db, &mut s, "drop access method gist2 cascade");
    let said = take_notices();
    assert_eq!(said[0].message, "drop cascades to 2 other objects");
    assert_eq!(said[0].detail.as_deref(), Some("drop cascades to index zz_i\ndrop cascades to index aa_i"));
}

#[test]
fn default_table_access_method_is_a_name_truncated_before_it_is_checked() {
    // (GUC_IS_NAME: parse_and_validate_value truncates it, guc.c:3170, so check_default_table_access_method never
    // sees one too long)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let long = "abcdefghij".repeat(6) + "abcd";
    let cut = &long[..63];
    take_notices();
    let (code, message) = fails(&db, &mut s, &format!("set default_table_access_method = '{long}'"));
    assert_eq!(
        take_notices().into_iter().map(|n| n.message).collect::<Vec<_>>(),
        [format!("identifier \"{long}\" will be truncated to \"{cut}\"")]
    );
    assert_eq!(
        (code, message),
        ("22023", format!("invalid value for parameter \"default_table_access_method\": \"{cut}\""))
    );
    assert_eq!(
        detail(&db, &mut s, &format!("set default_table_access_method = '{long}'")),
        Some(format!("Table access method \"{cut}\" does not exist."))
    );
}

#[test]
fn heap_has_no_operator_classes() {
    // (get_index_am_oid: heap is a table method, amcmds.c:139)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(
        fails(&db, &mut s, "create operator class x_ops for type int4 using heap as operator 1 <"),
        ("55000", "access method \"heap\" is not of type INDEX".into())
    );
    assert_eq!(
        fails(&db, &mut s, "create operator family x_fam using heap"),
        ("55000", "access method \"heap\" is not of type INDEX".into())
    );
}

#[test]
fn set_access_method_is_refused_for_an_index_and_a_sequence() {
    // (ATSimplePermissions: ATT_TABLE | ATT_MATVIEW only, tablecmds.c:4866)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
    run(&db, &mut s, "create table t (a int)");
    run(&db, &mut s, "create index t_a on t (a)");
    run(&db, &mut s, "create sequence sq");
    assert_eq!(
        fails(&db, &mut s, "alter index t_a set access method heap2"),
        ("42809", "ALTER action SET ACCESS METHOD cannot be performed on relation \"t_a\"".into())
    );
    assert_eq!(
        detail(&db, &mut s, "alter index t_a set access method heap2").as_deref(),
        Some("This operation is not supported for indexes.")
    );
    assert_eq!(
        fails(&db, &mut s, "alter table sq set access method heap2"),
        ("42809", "ALTER action SET ACCESS METHOD cannot be performed on relation \"sq\"".into())
    );
    assert_eq!(
        detail(&db, &mut s, "alter table sq set access method heap2").as_deref(),
        Some("This operation is not supported for sequences.")
    );
}

#[test]
fn made_methods_and_what_uses_them_survive_a_reopen() {
    let dir = std::env::temp_dir().join(format!("chronos-create-am-review-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let shown = "select amname, amtype, oid::int8 from pg_am where amname in ('gist2', 'heap2') order by 1";
    let before = {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(MAIN);
        gist2(&db, &mut s);
        run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
        run(&db, &mut s, "comment on access method heap2 is 'kept'");
        run(&db, &mut s, "create table bx (id int, b box) using heap2");
        run(&db, &mut s, "insert into bx values (1, '(1,1),(0,0)'), (2, '(5,5),(4,4)'), (3, null)");
        run(&db, &mut s, "create index bx_i on bx using gist2 (b)");
        db.checkpoint().unwrap();
        run(&db, &mut s, shown)
    };
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(run(&db, &mut s, shown), before);
    assert_eq!(relam(&db, &mut s, "bx"), "heap2");
    assert_eq!(relam(&db, &mut s, "bx_i"), "gist2");
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('bx_i'::regclass)"),
        "CREATE INDEX bx_i ON public.bx USING gist2 (b)"
    );
    assert_eq!(one(&db, &mut s, "select pg_index_has_property('bx_i'::regclass, 'clusterable')"), "true");
    assert_eq!(one(&db, &mut s, "select count(*) from bx where b && '(2,2),(0,0)'::box"), "1");
    assert_eq!(one(&db, &mut s, "select obj_description(oid, 'pg_am') from pg_am where amname = 'heap2'"), "kept");
    assert_eq!(
        detail(&db, &mut s, "drop access method gist2").as_deref(),
        Some("index bx_i depends on operator class box_ops for access method gist2")
    );
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_method_made_in_a_transaction_goes_with_its_rollback() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "begin");
    gist2(&db, &mut s);
    run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
    run(&db, &mut s, "create table bx (b box)");
    run(&db, &mut s, "create index bx_i on bx using gist2 (b)");
    run(&db, &mut s, "set local default_table_access_method = heap2");
    run(&db, &mut s, "create table in_tx (a int)");
    assert_eq!(relam(&db, &mut s, "bx_i"), "gist2");
    assert_eq!(relam(&db, &mut s, "in_tx"), "heap2");
    run(&db, &mut s, "rollback");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_am where amname in ('gist2', 'heap2')"), "0");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_class where relname in ('bx', 'bx_i', 'in_tx')"), "0");
    assert_eq!(one(&db, &mut s, "show default_table_access_method"), "heap");
    // (and one committed stays)
    run(&db, &mut s, "begin");
    run(&db, &mut s, "create access method heap3 type table handler heap_tableam_handler");
    run(&db, &mut s, "create table t3 (a int) using heap3");
    run(&db, &mut s, "commit");
    assert_eq!(relam(&db, &mut s, "t3"), "heap3");
}

#[test]
fn set_access_method_with_another_change_and_on_a_partition() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
    run(&db, &mut s, "create table c1 (a int)");
    run(&db, &mut s, "alter table c1 set access method heap2, add column b int");
    assert_eq!(relam(&db, &mut s, "c1"), "heap2");
    assert_eq!(
        one(&db, &mut s, "select count(*) from pg_attribute where attrelid = 'c1'::regclass and attnum > 0"),
        "2"
    );
    // (a change that fails changes neither)
    assert_eq!(fails(&db, &mut s, "alter table c1 set access method heap, add column a int").0, "42701");
    assert_eq!(relam(&db, &mut s, "c1"), "heap2");
    // a partition has one of its own; a table's children don't get its
    run(&db, &mut s, "create table pt (a int) partition by range (a)");
    run(&db, &mut s, "create table pt1 partition of pt for values from (0) to (10)");
    run(&db, &mut s, "alter table pt1 set access method heap2");
    run(&db, &mut s, "create table par (a int)");
    run(&db, &mut s, "create table chi () inherits (par)");
    run(&db, &mut s, "alter table par set access method heap2");
    assert_eq!(
        run(
            &db,
            &mut s,
            "select c.relname, coalesce(a.amname, '') from pg_class c left join pg_am a on a.oid = c.relam
             where c.relname in ('pt', 'pt1', 'par', 'chi') order by 1"
        ),
        [["chi", "heap"], ["par", "heap2"], ["pt", ""], ["pt1", "heap2"]]
            .map(|r| r.map(String::from).to_vec())
            .to_vec()
    );
}

#[test]
fn an_index_method_made_in_a_world_merges_with_its_indexes() {
    let db = Db::new();
    let mut m = Session::new(MAIN);
    run(&db, &mut m, "create table bx (b box)");
    db.fork(MAIN, "w").unwrap();
    let mut w = Session::new("w");
    gist2(&db, &mut w);
    run(&db, &mut w, "create index bx_i on bx using gist2 (b)");
    assert_eq!(one(&db, &mut m, "select count(*) from pg_am where amname = 'gist2'"), "0");
    db.merge("w", chronos::Resolve::Fail).unwrap();
    assert_eq!(relam(&db, &mut m, "bx_i"), "gist2");
    assert_eq!(
        one(&db, &mut m, "select pg_get_indexdef('bx_i'::regclass)"),
        "CREATE INDEX bx_i ON public.bx USING gist2 (b)"
    );
    assert_eq!(
        detail(&db, &mut m, "drop access method gist2").as_deref(),
        Some("index bx_i depends on operator class box_ops for access method gist2")
    );
    // (one of a name made in two worlds is a conflict)
    db.fork(MAIN, "x").unwrap();
    let mut x = Session::new("x");
    run(&db, &mut x, "create access method heap2 type table handler heap_tableam_handler");
    run(&db, &mut m, "create access method heap2 type table handler heap_tableam_handler");
    assert!(db.merge("x", chronos::Resolve::Fail).is_err());
}

#[test]
fn a_temporary_table_of_a_method_stops_its_drop_or_goes_with_it() {
    // (the session's temporary tables are kept in a database of their own; DROP looks there too, as Postgres's
    // pg_depend has their rows)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
    run(&db, &mut s, "create temp table tt (a int) using heap2");
    assert_eq!(fails(&db, &mut s, "drop access method heap2").0, "2BP01");
    assert_eq!(
        detail(&db, &mut s, "drop access method heap2").as_deref(),
        Some("table tt depends on access method heap2")
    );
    take_notices();
    run(&db, &mut s, "drop access method heap2 cascade");
    let said = take_notices();
    assert_eq!(said[0].message, "drop cascades to table tt");
    assert_eq!(fails(&db, &mut s, "select * from tt").0, "42P01");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_am where amname = 'heap2'"), "0");
}

#[test]
fn a_foreign_key_on_a_unique_index_of_a_method_goes_with_its_cascade() {
    // (the foreign key depends on the index it references; DROP ... CASCADE takes it before the index)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create access method btree2 type index handler bthandler");
    run(
        &db,
        &mut s,
        "create operator class int4_ops default for type int4 using btree2 as
           operator 1 <, operator 2 <=, operator 3 =, operator 4 >=, operator 5 >,
           function 1 btint4cmp(int4, int4)",
    );
    run(&db, &mut s, "create table p (a int)");
    run(&db, &mut s, "create unique index p_a on p using btree2 (a)");
    run(&db, &mut s, "create table c (a int references p (a))");
    assert_eq!(
        detail(&db, &mut s, "drop access method btree2").as_deref(),
        Some(
            "index p_a depends on operator class int4_ops for access method btree2\n\
             constraint c_a_fkey on table c depends on index p_a"
        )
    );
    take_notices();
    run(&db, &mut s, "drop access method btree2 cascade");
    let said = take_notices();
    assert_eq!(said[0].message, "drop cascades to 2 other objects");
    assert_eq!(
        said[0].detail.as_deref(),
        Some("drop cascades to index p_a\ndrop cascades to constraint c_a_fkey on table c")
    );
    // (no key left to check against)
    run(&db, &mut s, "insert into c values (5)");
}

/// An index method `am` (as written: quoted or not) over bthandler, with a default class for int4 made for it.
fn btree_like(db: &Db, s: &mut Session, am: &str) {
    run(db, s, &format!("create access method {am} type index handler bthandler"));
    run(
        db,
        s,
        &format!(
            "create operator class int4_ops default for type int4 using {am} as
               operator 1 <, operator 2 <=, operator 3 =, operator 4 >=, operator 5 >,
               function 1 btint4cmp(int4, int4)"
        ),
    );
}

#[test]
fn a_temporary_tables_index_finds_the_classes_made_for_its_method() {
    // (a temporary table is kept in the session's own database; the classes are the world's)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    btree_like(&db, &mut s, "btree2");
    run(&db, &mut s, "create temp table tt (a int)");
    run(&db, &mut s, "create index ti on tt using btree2 (a)");
    run(&db, &mut s, "create index ti2 on tt using btree2 (a int4_ops)");
    assert_eq!(relam(&db, &mut s, "ti"), "btree2");
}

#[test]
fn a_quoted_method_name_keeps_its_case() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    btree_like(&db, &mut s, "\"Btree2\"");
    run(&db, &mut s, "create table q (a int)");
    run(&db, &mut s, "create index qi on q using \"Btree2\" (a)");
    assert_eq!(relam(&db, &mut s, "qi"), "Btree2");
    // (its definition quotes it, so it makes the index again)
    let def = one(&db, &mut s, "select pg_get_indexdef('qi'::regclass)");
    assert!(def.contains("USING \"Btree2\""), "{def}");
    run(&db, &mut s, "drop index qi");
    run(&db, &mut s, &def);
    assert_eq!(
        fails(&db, &mut s, "create index qj on q using btree2 (a)"),
        ("42704", "access method \"btree2\" does not exist".into())
    );
}

#[test]
fn an_index_names_a_made_class_of_any_name() {
    // (opt_class is any name: one CREATE OPERATOR CLASS made needn't end in _ops, and may be quoted)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    btree_like(&db, &mut s, "btree2");
    for c in ["custom_int4", "\"Int4 Ops\""] {
        run(
            &db,
            &mut s,
            &format!(
                "create operator class {c} for type int4 using btree2 as
                   operator 1 <, operator 2 <=, operator 3 =, operator 4 >=, operator 5 >,
                   function 1 btint4cmp(int4, int4)"
            ),
        );
    }
    run(&db, &mut s, "create table k (a int)");
    run(&db, &mut s, "create index k1 on k using btree2 (a custom_int4)");
    run(&db, &mut s, "create index k2 on k using btree2 (a \"Int4 Ops\" desc)");
    assert_eq!(fails(&db, &mut s, "create index k3 on k using btree2 (a no_such_class)").0, "42704");
}

#[test]
fn an_index_using_a_made_class_stops_its_drop_or_goes_with_it() {
    // (an index depends on its parts' classes: index_create)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    btree_like(&db, &mut s, "btree2");
    run(
        &db,
        &mut s,
        "create operator class custom_int4 for type int4 using btree2 as
           operator 1 <, operator 2 <=, operator 3 =, operator 4 >=, operator 5 >,
           function 1 btint4cmp(int4, int4)",
    );
    run(&db, &mut s, "create table k (a int)");
    run(&db, &mut s, "create index ki on k using btree2 (a custom_int4)");
    assert_eq!(
        detail(&db, &mut s, "drop operator class custom_int4 using btree2").as_deref(),
        Some("index ki depends on operator class custom_int4 for access method btree2")
    );
    run(&db, &mut s, "drop operator class custom_int4 using btree2 cascade");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_indexes where indexname = 'ki'"), "0");
}
