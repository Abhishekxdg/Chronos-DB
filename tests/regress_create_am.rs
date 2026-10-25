//! CREATE ACCESS METHOD and DROP ACCESS METHOD, as Postgres's create_am.sql uses them: an index method over a
//! built-in one's handler (gist2 over gisthandler) with the operator classes made for it, a table method over
//! heap_tableam_handler (heap2) that tables, CREATE TABLE AS, SELECT INTO and materialized views are made with
//! (USING, default_table_access_method) and ALTER TABLE ... SET ACCESS METHOD changes, shown in pg_am, pg_class.relam
//! and pg_depend, and kept as rows of the world, so a fork has them and a merge brings them. Each expected value is
//! what Postgres 16 gives.

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

/// The error's DETAIL and HINT.
fn detail(db: &Db, s: &mut Session, q: &str) -> (Option<String>, Option<String>) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => e.info.map_or((None, None), |i| (i.detail.clone(), i.hint.clone())),
    }
}

fn notices() -> Vec<String> {
    take_notices().into_iter().map(|n| n.message).collect()
}

#[test]
fn an_index_method_over_a_handler_does_as_its_handlers_does() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table boxes (id int, home_base box)");
    run(&db, &mut s, "insert into boxes values (1, '(1,1),(0,0)'), (2, '(5,5),(4,4)'), (3, null)");
    run(&db, &mut s, "create access method gist2 type index handler gisthandler");
    // (the handler must be a function of internal, of the kind's result type)
    assert_eq!(
        fails(&db, &mut s, "create access method bogus type index handler int4in"),
        ("42883", "function int4in(internal) does not exist".into())
    );
    assert_eq!(
        fails(&db, &mut s, "create access method bogus type index handler heap_tableam_handler"),
        ("42809", "function heap_tableam_handler must return type index_am_handler".into())
    );
    assert_eq!(
        fails(&db, &mut s, "create access method bogus type index handler public.gisthandler"),
        ("42883", "function public.gisthandler(internal) does not exist".into())
    );
    assert_eq!(
        fails(&db, &mut s, "create access method gist2 type index handler gisthandler"),
        ("42710", "access method \"gist2\" already exists".into())
    );
    assert_eq!(
        run(&db, &mut s, "select amname, amhandler::text, amtype from pg_am where amname = 'gist2'"),
        vec![vec!["gist2".to_string(), "gisthandler".into(), "i".into()]]
    );
    // it has no operator classes of its own until they are made for it
    assert_eq!(
        fails(&db, &mut s, "create index boxes_gist2 on boxes using gist2 (home_base)"),
        ("42704", "data type box has no default operator class for access method \"gist2\"".into())
    );
    let (_, hint) = detail(&db, &mut s, "create index boxes_gist2 on boxes using gist2 (home_base)");
    assert_eq!(
        hint.as_deref(),
        Some("You must specify an operator class for the index or define a default operator class for the data type.")
    );
    assert_eq!(
        fails(&db, &mut s, "create operator class box_ops for type box using nosuch as operator 1 <<"),
        ("42704", "access method \"nosuch\" does not exist".into())
    );
    run(
        &db,
        &mut s,
        "create operator class box_ops default for type box using gist2 as
           operator 1 <<, operator 3 &&, operator 8 <@,
           function 1 gist_box_consistent(internal, box, smallint, oid, internal),
           function 2 gist_box_union(internal, internal),
           function 5 gist_box_penalty(internal, internal, internal),
           function 6 gist_box_picksplit(internal, internal),
           function 7 gist_box_same(box, box, internal)",
    );
    run(&db, &mut s, "create index boxes_gist2 on boxes using gist2 (home_base)");
    // shown as its own, and answering as gist does
    assert_eq!(
        one(
            &db,
            &mut s,
            "select a.amname from pg_class c join pg_am a on a.oid = c.relam where c.relname = 'boxes_gist2'"
        ),
        "gist2"
    );
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('boxes_gist2'::regclass)"),
        "CREATE INDEX boxes_gist2 ON public.boxes USING gist2 (home_base)"
    );
    let prop = |db: &Db, s: &mut Session, p: &str| {
        one(db, s, &format!("select pg_indexam_has_property(oid, '{p}') from pg_am where amname = 'gist2'"))
    };
    assert_eq!(prop(&db, &mut s, "can_order"), "false");
    assert_eq!(prop(&db, &mut s, "can_multi_col"), "true");
    assert_eq!(prop(&db, &mut s, "can_include"), "true");
    assert_eq!(one(&db, &mut s, "select pg_index_has_property('boxes_gist2'::regclass, 'clusterable')"), "true");
    assert_eq!(
        fails(&db, &mut s, "create unique index boxes_u on boxes using gist2 (home_base)"),
        ("0A000", "access method \"gist2\" does not support unique indexes".into())
    );
    assert_eq!(one(&db, &mut s, "select count(*) from boxes where home_base && '(1000,1000,0,0)'::box"), "2");
    assert_eq!(one(&db, &mut s, "select count(*) from boxes where home_base is null"), "1");
    // what depends on it stops the drop, and goes with CASCADE
    assert_eq!(
        fails(&db, &mut s, "drop access method gist2"),
        ("2BP01", "cannot drop access method gist2 because other objects depend on it".into())
    );
    let (d, hint) = detail(&db, &mut s, "drop access method gist2 restrict");
    assert_eq!(d.as_deref(), Some("index boxes_gist2 depends on operator class box_ops for access method gist2"));
    assert_eq!(hint.as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    take_notices();
    run(&db, &mut s, "drop access method gist2 cascade");
    assert_eq!(notices(), ["drop cascades to index boxes_gist2"]);
    assert_eq!(one(&db, &mut s, "select count(*) from pg_class where relname = 'boxes_gist2'"), "0");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_am where amname = 'gist2'"), "0");
    // (its operator classes went with it: one made again starts without them)
    run(&db, &mut s, "create access method gist2 type index handler gisthandler");
    assert_eq!(fails(&db, &mut s, "create index boxes_gist2 on boxes using gist2 (home_base)").0, "42704");
    run(&db, &mut s, "drop access method gist2");
    // Postgres's own can't be dropped; one not there is skipped with IF EXISTS
    assert_eq!(
        fails(&db, &mut s, "drop access method btree"),
        ("2BP01", "cannot drop access method btree because it is required by the database system".into())
    );
    assert_eq!(
        fails(&db, &mut s, "drop access method gist2"),
        ("42704", "access method \"gist2\" does not exist".into())
    );
    take_notices();
    run(&db, &mut s, "drop access method if exists gist2");
    assert_eq!(notices(), ["access method \"gist2\" does not exist, skipping"]);
    // an index of a table method's: its handler gives no index routine
    assert_eq!(
        fails(&db, &mut s, "create index on boxes using heap (id)"),
        ("XX000", "index access method handler function 3 did not return an IndexAmRoutine struct".into())
    );
    assert_eq!(
        fails(&db, &mut s, "create index on boxes using nosuch (id)"),
        ("42704", "access method \"nosuch\" does not exist".into())
    );
}

#[test]
fn a_comment_on_an_access_method_is_its_description() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
    run(&db, &mut s, "comment on access method heap2 is 'a heap by another name'");
    assert_eq!(
        one(&db, &mut s, "select obj_description(oid, 'pg_am') from pg_am where amname = 'heap2'"),
        "a heap by another name"
    );
    assert_eq!(
        one(&db, &mut s, "select obj_description(oid, 'pg_am') from pg_am where amname = 'btree'"),
        "b-tree index access method"
    );
    assert_eq!(
        fails(&db, &mut s, "comment on access method nosuch is 'x'"),
        ("42704", "access method \"nosuch\" does not exist".into())
    );
    assert_eq!(
        one(&db, &mut s, "select pg_describe_object('pg_am'::regclass, oid, 0) from pg_am where amname = 'heap2'"),
        "access method heap2"
    );
    run(&db, &mut s, "drop access method heap2");
    assert_eq!(
        one(
            &db,
            &mut s,
            "select count(*) from pg_description where classoid = 'pg_am'::regclass and description like 'a heap%'"
        ),
        "0"
    );
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

#[test]
fn tables_are_made_with_a_table_method() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    // default_table_access_method takes a table method that is there
    assert_eq!(
        fails(&db, &mut s, "set default_table_access_method = ''"),
        ("22023", "invalid value for parameter \"default_table_access_method\": \"\"".into())
    );
    let (d, _) = detail(&db, &mut s, "set default_table_access_method = ''");
    assert_eq!(d.as_deref(), Some("default_table_access_method cannot be empty."));
    let (d, _) = detail(&db, &mut s, "set default_table_access_method = 'I do not exist AM'");
    assert_eq!(d.as_deref(), Some("Table access method \"I do not exist AM\" does not exist."));
    assert_eq!(
        fails(&db, &mut s, "set default_table_access_method = 'btree'"),
        ("55000", "access method \"btree\" is not of type TABLE".into())
    );
    run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
    assert_eq!(
        fails(&db, &mut s, "create access method bogus type table handler bthandler"),
        ("42809", "function bthandler must return type table_am_handler".into())
    );
    assert_eq!(
        run(&db, &mut s, "select amname, amhandler::text, amtype from pg_am where amtype = 't' order by 1, 2"),
        vec![
            vec!["heap".to_string(), "heap_tableam_handler".into(), "t".into()],
            vec!["heap2".to_string(), "heap_tableam_handler".into(), "t".into()]
        ]
    );
    run(&db, &mut s, "create table tbl_heap2 (f1 int) using heap2");
    run(&db, &mut s, "insert into tbl_heap2 values (1)");
    assert_eq!(one(&db, &mut s, "select f1 from tbl_heap2"), "1");
    run(&db, &mut s, "create table tblas_heap2 using heap2 as select * from tbl_heap2");
    run(&db, &mut s, "create materialized view tblmv_heap2 using heap2 as select * from tbl_heap2");
    assert_eq!(one(&db, &mut s, "select f1 from tblmv_heap2"), "1");
    assert_eq!(relam(&db, &mut s, "tbl_heap2"), "heap2");
    assert_eq!(relam(&db, &mut s, "tblas_heap2"), "heap2");
    assert_eq!(relam(&db, &mut s, "tblmv_heap2"), "heap2");
    // a partitioned table has none; its partitions get the default, not its
    assert_eq!(
        fails(&db, &mut s, "create table parted (a text, b int) partition by list (a) using heap2"),
        ("0A000", "specifying a table access method is not supported on a partitioned table".into())
    );
    run(&db, &mut s, "create table parted (a text, b int) partition by list (a)");
    run(&db, &mut s, "set default_table_access_method = 'heap2'");
    run(&db, &mut s, "create table parted_b partition of parted for values in ('b')");
    run(&db, &mut s, "create table parted_c partition of parted for values in ('c') using heap");
    run(&db, &mut s, "select into selinto_heapx from tbl_heap2");
    run(&db, &mut s, "create view view_heapx as select * from tbl_heap2");
    run(&db, &mut s, "create sequence seq_heapx");
    run(&db, &mut s, "reset default_table_access_method");
    assert_eq!(relam(&db, &mut s, "parted"), "");
    assert_eq!(relam(&db, &mut s, "parted_b"), "heap2");
    assert_eq!(relam(&db, &mut s, "parted_c"), "heap");
    assert_eq!(relam(&db, &mut s, "selinto_heapx"), "heap2");
    assert_eq!(relam(&db, &mut s, "view_heapx"), "");
    assert_eq!(relam(&db, &mut s, "seq_heapx"), "");
    run(&db, &mut s, "drop view view_heapx");
    // what can't be one
    assert_eq!(
        fails(&db, &mut s, "create table i_am_a_failure() using i_do_not_exist_am"),
        ("42704", "access method \"i_do_not_exist_am\" does not exist".into())
    );
    assert_eq!(
        fails(&db, &mut s, "create table i_am_a_failure() using btree"),
        ("55000", "access method \"btree\" is not of type TABLE".into())
    );
    assert_eq!(
        fails(&db, &mut s, "create table i_am_a_failure() using \"\""),
        ("42601", "zero-length delimited identifier at or near \"\"\"\"".into())
    );
    let (code, message) = fails(&db, &mut s, "select into x using heap2 from tbl_heap2");
    assert_eq!(code, "42601");
    assert!(message.to_lowercase().starts_with("syntax error at or near \"using\""), "{message}");
    // pg_depend has each on its method (not on heap, which is pinned)
    assert_eq!(
        run(
            &db,
            &mut s,
            "select pg_describe_object(classid, objid, objsubid) from pg_depend
             where refclassid = 'pg_am'::regclass and refobjid = (select oid from pg_am where amname = 'heap2')
             order by 1"
        ),
        vec![
            vec!["materialized view tblmv_heap2".to_string()],
            vec!["table parted_b".into()],
            vec!["table selinto_heapx".into()],
            vec!["table tbl_heap2".into()],
            vec!["table tblas_heap2".into()]
        ]
    );
    // what depends on it stops the drop, each said in the order made
    let (d, _) = detail(&db, &mut s, "drop access method heap2");
    assert_eq!(
        d.as_deref(),
        Some(
            "table tbl_heap2 depends on access method heap2\ntable tblas_heap2 depends on access method heap2\n\
             materialized view tblmv_heap2 depends on access method heap2\ntable parted_b depends on access method \
             heap2\ntable selinto_heapx depends on access method heap2"
        )
    );
}

#[test]
fn alter_table_set_access_method_changes_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
    run(
        &db,
        &mut s,
        "create table heaptable using heap as select a, repeat(a::text, 100) from generate_series(1, 9) as a",
    );
    assert_eq!(relam(&db, &mut s, "heaptable"), "heap");
    run(&db, &mut s, "alter table heaptable set access method heap2");
    assert_eq!(relam(&db, &mut s, "heaptable"), "heap2");
    let deps = "select pg_describe_object(refclassid, refobjid, refobjsubid) from pg_depend
                where classid = 'pg_class'::regclass and objid = 'heaptable'::regclass and refclassid = 'pg_am'::regclass";
    assert_eq!(run(&db, &mut s, deps), vec![vec!["access method heap2".to_string()]]);
    run(&db, &mut s, "alter table heaptable set access method heap");
    assert_eq!(relam(&db, &mut s, "heaptable"), "heap");
    assert!(run(&db, &mut s, deps).is_empty());
    run(&db, &mut s, "alter table heaptable set access method heap2");
    // its rows are as they were
    assert_eq!(
        run(&db, &mut s, "select count(a), count(1) filter (where a = 1) from heaptable"),
        vec![vec!["9".to_string(), "1".into()]]
    );
    run(&db, &mut s, "create materialized view heapmv using heap as select * from heaptable");
    run(&db, &mut s, "alter materialized view heapmv set access method heap2");
    assert_eq!(relam(&db, &mut s, "heapmv"), "heap2");
    assert_eq!(one(&db, &mut s, "select count(a) from heapmv"), "9");
    // one change of it to a statement, a partitioned table none, a view none
    assert_eq!(
        fails(&db, &mut s, "alter table heaptable set access method heap, set access method heap2"),
        ("0A000", "cannot have multiple SET ACCESS METHOD subcommands".into())
    );
    assert_eq!(
        fails(&db, &mut s, "alter materialized view heapmv set access method heap, set access method heap2"),
        ("0A000", "cannot have multiple SET ACCESS METHOD subcommands".into())
    );
    run(&db, &mut s, "create table am_partitioned (x int, y int) partition by hash (x)");
    assert_eq!(
        fails(&db, &mut s, "alter table am_partitioned set access method heap2"),
        ("42809", "cannot change access method of a partitioned table".into())
    );
    run(&db, &mut s, "create view v as select 1 as one");
    assert_eq!(
        fails(&db, &mut s, "alter table v set access method heap2"),
        ("42809", "ALTER action SET ACCESS METHOD cannot be performed on relation \"v\"".into())
    );
    assert_eq!(
        fails(&db, &mut s, "alter table heaptable set access method btree"),
        ("55000", "access method \"btree\" is not of type TABLE".into())
    );
    // CASCADE drops what is of it, and what reads that
    run(&db, &mut s, "create view reads_heaptable as select a from heaptable");
    take_notices();
    run(&db, &mut s, "drop access method heap2 cascade");
    let said = take_notices();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].message, "drop cascades to 3 other objects");
    assert_eq!(
        said[0].detail.as_deref(),
        Some(
            "drop cascades to table heaptable\ndrop cascades to view reads_heaptable\ndrop cascades to materialized view heapmv"
        )
    );
    assert_eq!(
        one(&db, &mut s, "select count(*) from pg_class where relname in ('heaptable', 'heapmv', 'reads_heaptable')"),
        "0"
    );
}

#[test]
fn access_methods_fork_and_merge_with_the_world() {
    let db = Db::new();
    db.fork(MAIN, "w").unwrap();
    let mut w = Session::new("w");
    run(&db, &mut w, "create access method heap2 type table handler heap_tableam_handler");
    run(&db, &mut w, "create table t (a int) using heap2");
    let mut m = Session::new(MAIN);
    assert_eq!(one(&db, &mut m, "select count(*) from pg_am where amname = 'heap2'"), "0");
    db.merge("w", chronos::Resolve::Fail).unwrap();
    assert_eq!(one(&db, &mut m, "select count(*) from pg_am where amname = 'heap2'"), "1");
    assert_eq!(relam(&db, &mut m, "t"), "heap2");
    // (made in two worlds, two have two oids)
    db.fork(MAIN, "x").unwrap();
    let mut x = Session::new("x");
    run(&db, &mut x, "create access method heap3 type table handler heap_tableam_handler");
    run(&db, &mut m, "create access method heap4 type table handler heap_tableam_handler");
    db.merge("x", chronos::Resolve::Fail).unwrap();
    assert_eq!(one(&db, &mut m, "select count(distinct oid) from pg_am where amname in ('heap3', 'heap4')"), "2");
}

#[test]
fn drop_and_set_access_method_refuse_as_postgres_does() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create access method heap2 type table handler heap_tableam_handler");
    // every name is looked up (and its owner checked) before one of Postgres's own is refused
    assert_eq!(
        fails(&db, &mut s, "drop access method btree, nosuch"),
        ("42704", "access method \"nosuch\" does not exist".into())
    );
    take_notices();
    assert_eq!(
        fails(&db, &mut s, "drop access method if exists nosuch, btree"),
        ("2BP01", "cannot drop access method btree because it is required by the database system".into())
    );
    assert_eq!(notices(), ["access method \"nosuch\" does not exist, skipping"]);
    assert_eq!(
        fails(&db, &mut s, "drop access method heap2, btree"),
        ("2BP01", "cannot drop access method btree because it is required by the database system".into())
    );
    assert_eq!(one(&db, &mut s, "select count(*) from pg_am where amname = 'heap2'"), "1");
    run(&db, &mut s, "create role regress_am_p2");
    run(&db, &mut s, "set role regress_am_p2");
    assert_eq!(fails(&db, &mut s, "drop access method btree"), ("42501", "must be superuser".into()));
    assert_eq!(
        fails(&db, &mut s, "drop access method nosuch"),
        ("42704", "access method \"nosuch\" does not exist".into())
    );
    run(&db, &mut s, "reset role");
    // SET ACCESS METHOD of an index, by ALTER INDEX or ALTER TABLE, is refused whatever the method
    run(&db, &mut s, "create table pt (a int) partition by range (a)");
    run(&db, &mut s, "create index pt_i on pt (a)");
    run(&db, &mut s, "create table t (a int)");
    run(&db, &mut s, "create sequence sq");
    for q in ["alter index pt_i set access method heap2", "alter table pt_i set access method nosuch"] {
        assert_eq!(
            fails(&db, &mut s, q),
            ("42809", "ALTER action SET ACCESS METHOD cannot be performed on relation \"pt_i\"".into()),
            "{q}"
        );
        assert_eq!(
            detail(&db, &mut s, q).0.as_deref(),
            Some("This operation is not supported for partitioned indexes.")
        );
    }
    assert_eq!(fails(&db, &mut s, "alter index t set access method heap2"), ("42809", "\"t\" is not an index".into()));
    assert_eq!(
        fails(&db, &mut s, "alter index sq set access method heap2"),
        ("42809", "\"sq\" is not an index".into())
    );
    assert_eq!(
        fails(&db, &mut s, "alter index nosuch set access method heap2"),
        ("42P01", "relation \"nosuch\" does not exist".into())
    );
    take_notices();
    run(&db, &mut s, "alter index if exists nosuch set access method heap2");
    assert_eq!(notices(), ["relation \"nosuch\" does not exist, skipping"]);
    // (heap is a table method: no operator family is of it)
    run(&db, &mut s, "create operator family f1 using hash");
    assert_eq!(
        fails(&db, &mut s, "drop operator family f1 using heap"),
        ("55000", "access method \"heap\" is not of type INDEX".into())
    );
}
