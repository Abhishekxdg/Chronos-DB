//! Tablespaces as Postgres's regression suite uses them: kept as records (CREATE, ALTER and DROP TABLESPACE, with
//! Postgres's checks), each relation's tablespace in pg_class (from TABLESPACE, USING INDEX TABLESPACE,
//! default_tablespace and a partition's parent), moved by SET TABLESPACE, ALL IN TABLESPACE and REINDEX
//! (TABLESPACE ...) with a new relfilenode, and the privileges a role needs to put a relation there. Also a view's
//! own options in pg_class.reloptions.

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

/// The names of the relations in tablespace `space`, in name order.
fn in_space(db: &Db, s: &mut Session, space: &str) -> Vec<String> {
    let q = format!(
        "select c.relname from pg_class c, pg_tablespace t where c.reltablespace = t.oid and t.spcname = '{space}' \
         order by 1"
    );
    run(db, s, &q).into_iter().map(|r| r[0].clone()).collect()
}

fn session(db: &Db) -> Session {
    let mut s = Session::new(MAIN);
    run(db, &mut s, "set allow_in_place_tablespaces = true");
    run(db, &mut s, "create tablespace regress_tblspace location ''");
    s
}

#[test]
fn a_tablespace_is_a_kept_record() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(
        fails(&db, &mut s, "create tablespace ts location 'relative'"),
        ("42P17", "tablespace location must be an absolute path".into())
    );
    // (in place only where allow_in_place_tablespaces says)
    assert_eq!(fails(&db, &mut s, "create tablespace ts location ''").0, "42P17");
    assert_eq!(
        fails(&db, &mut s, "create tablespace ts location '/no/such/location'"),
        ("58P01", "directory \"/no/such/location\" does not exist".into())
    );
    run(&db, &mut s, "set allow_in_place_tablespaces = true");
    assert_eq!(
        fails(&db, &mut s, "create tablespace ts location '' with (some_nonexistent_parameter = true)"),
        ("22023", "unrecognized parameter \"some_nonexistent_parameter\"".into())
    );
    run(&db, &mut s, "create tablespace ts location '' with (random_page_cost = 3.0)");
    assert_eq!(one(&db, &mut s, "select spcoptions from pg_tablespace where spcname = 'ts'"), "{random_page_cost=3.0}");
    assert_eq!(
        fails(&db, &mut s, "create tablespace ts location ''"),
        ("42710", "tablespace \"ts\" already exists".into())
    );
    assert_eq!(fails(&db, &mut s, "create tablespace pg_mine location ''").0, "42939");
    // (in place: a directory of the data folder's own)
    let at = one(&db, &mut s, "select pg_tablespace_location(oid) from pg_tablespace where spcname = 'ts'");
    assert!(at.starts_with("pg_tblspc/"), "{at}");
    assert_eq!(
        one(&db, &mut s, "select pg_tablespace_location(oid) from pg_tablespace where spcname = 'pg_default'"),
        ""
    );
    assert_eq!(one(&db, &mut s, "select pg_tablespace_location(0)"), "");

    run(&db, &mut s, "alter tablespace ts set (random_page_cost = 1.0, seq_page_cost = 1.1)");
    assert_eq!(
        one(&db, &mut s, "select spcoptions from pg_tablespace where spcname = 'ts'"),
        "{random_page_cost=1.0,seq_page_cost=1.1}"
    );
    assert_eq!(fails(&db, &mut s, "alter tablespace ts set (some_nonexistent_parameter = true)").0, "22023");
    assert_eq!(
        fails(&db, &mut s, "alter tablespace ts reset (random_page_cost = 2.0)"),
        ("42601", "RESET must not include values for parameters".into())
    );
    run(&db, &mut s, "alter tablespace ts reset (random_page_cost, effective_io_concurrency)");
    assert_eq!(one(&db, &mut s, "select spcoptions from pg_tablespace where spcname = 'ts'"), "{seq_page_cost=1.1}");

    // a rename keeps its oid
    let oid = one(&db, &mut s, "select oid from pg_tablespace where spcname = 'ts'");
    run(&db, &mut s, "alter tablespace ts rename to ts2");
    assert_eq!(one(&db, &mut s, "select oid from pg_tablespace where spcname = 'ts2'"), oid);
    // (not inside a transaction block, as Postgres's would make or remove a directory)
    run(&db, &mut s, "begin");
    assert_eq!(fails(&db, &mut s, "drop tablespace ts2").0, "25001");
    run(&db, &mut s, "rollback");
    run(&db, &mut s, "drop tablespace ts2");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_tablespace"), "2");
    assert_eq!(fails(&db, &mut s, "drop tablespace ts2"), ("42704", "tablespace \"ts2\" does not exist".into()));
    take_notices();
    run(&db, &mut s, "drop tablespace if exists ts2");
    let notes: Vec<String> = take_notices().into_iter().map(|n| n.message).collect();
    assert!(notes.contains(&"tablespace \"ts2\" does not exist, skipping".to_string()), "{notes:?}");
    assert_eq!(fails(&db, &mut s, "drop tablespace pg_default").0, "42501");
}

#[test]
fn a_relation_records_where_it_is_kept() {
    let db = Db::new();
    let mut s = session(&db);
    run(&db, &mut s, "create schema testschema");
    run(&db, &mut s, "create table testschema.foo (i int) tablespace regress_tblspace");
    run(&db, &mut s, "create table testschema.asselect tablespace regress_tblspace as select 1");
    run(&db, &mut s, "create index foo_idx on testschema.foo (i) tablespace regress_tblspace");
    run(&db, &mut s, "create table testschema.plain (i int)");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["asselect", "foo", "foo_idx"]);
    assert_eq!(
        fails(&db, &mut s, "create table bar (i int) tablespace regress_nosuchspace"),
        ("42704", "tablespace \"regress_nosuchspace\" does not exist".into())
    );
    assert_eq!(
        fails(&db, &mut s, "create table bar (i int) tablespace pg_global"),
        ("22023", "only shared relations can be placed in pg_global tablespace".into())
    );

    // a partition with none named is where its parent says, else where default_tablespace says
    run(&db, &mut s, "create table testschema.part (a int) partition by list (a)");
    run(&db, &mut s, "set default_tablespace to pg_global");
    assert_eq!(
        fails(&db, &mut s, "create table testschema.part_1 partition of testschema.part for values in (1)").0,
        "22023"
    );
    run(&db, &mut s, "set default_tablespace to regress_tblspace");
    run(&db, &mut s, "create table testschema.part_2 partition of testschema.part for values in (2)");
    run(&db, &mut s, "set default_tablespace to pg_global");
    run(&db, &mut s, "alter table testschema.part set tablespace regress_tblspace");
    run(&db, &mut s, "create table testschema.part_3 partition of testschema.part for values in (3)");
    run(
        &db,
        &mut s,
        "create table testschema.part_4 partition of testschema.part for values in (4) tablespace pg_default",
    );
    run(&db, &mut s, "reset default_tablespace");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["asselect", "foo", "foo_idx", "part", "part_2", "part_3"]);
    assert_eq!(
        fails(&db, &mut s, "set default_tablespace to regress_nosuchspace"),
        ("22023", "invalid value for parameter \"default_tablespace\": \"regress_nosuchspace\"".into())
    );

    // a partitioned relation can't name the database's default
    let refused = ("0A000", "cannot specify default tablespace for partitioned relations".to_string());
    assert_eq!(
        fails(&db, &mut s, "create table dflt (a int primary key) partition by list (a) tablespace pg_default"),
        refused
    );
    assert_eq!(
        fails(
            &db,
            &mut s,
            "create table dflt (a int primary key using index tablespace pg_default) partition by list (a)"
        ),
        refused
    );
    run(&db, &mut s, "set default_tablespace to 'pg_default'");
    assert_eq!(
        fails(&db, &mut s, "create table dflt (a int primary key) partition by list (a) tablespace regress_tblspace"),
        refused
    );
    run(
        &db,
        &mut s,
        "create table dflt (a int primary key using index tablespace regress_tblspace) partition by list (a) \
         tablespace regress_tblspace",
    );
    run(&db, &mut s, "set default_tablespace to ''");

    // a key's and a unique constraint's index: USING INDEX TABLESPACE, else default_tablespace's
    run(&db, &mut s, "create table testschema.tab (id bigint) tablespace regress_tblspace");
    run(&db, &mut s, "alter table testschema.tab add constraint tab_pkey primary key (id)");
    run(
        &db,
        &mut s,
        "alter table testschema.tab add constraint tab_u unique (id) using index tablespace regress_tblspace",
    );
    run(&db, &mut s, "set default_tablespace to regress_tblspace");
    run(&db, &mut s, "create index tab_d on testschema.tab (id)");
    run(&db, &mut s, "set default_tablespace to ''");
    // (an index rebuilt by ALTER TYPE stays where it was)
    run(&db, &mut s, "alter table testschema.tab alter id type int");
    let tab: Vec<String> =
        in_space(&db, &mut s, "regress_tblspace").into_iter().filter(|n| n.starts_with("tab")).collect();
    assert_eq!(tab, ["tab", "tab_d", "tab_u"]);
}

#[test]
fn a_move_gives_a_new_filenode() {
    let db = Db::new();
    let mut s = session(&db);
    run(&db, &mut s, "create table t (num1 bigint, t text)");
    run(&db, &mut s, "create index t_idx on t (num1)");
    let node = one(&db, &mut s, "select relfilenode from pg_class where relname = 't_idx'");
    // (only shared relations go in pg_global)
    assert_eq!(fails(&db, &mut s, "reindex (tablespace pg_global) index t_idx").0, "22023");
    assert_eq!(
        fails(&db, &mut s, "reindex (tablespace pg_global, concurrently) index t_idx"),
        ("0A000", "cannot move non-shared relation to tablespace \"pg_global\"".into())
    );
    // a REINDEX's move is its transaction's
    run(&db, &mut s, "begin");
    run(&db, &mut s, "reindex (tablespace regress_tblspace) table t");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["t_idx"]);
    run(&db, &mut s, "rollback");
    assert!(in_space(&db, &mut s, "regress_tblspace").is_empty());
    run(&db, &mut s, "reindex (tablespace regress_tblspace) table t");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["t_idx"]);
    assert_ne!(one(&db, &mut s, "select relfilenode from pg_class where relname = 't_idx'"), node);
    // a table moves without its indexes
    run(&db, &mut s, "alter table t set tablespace regress_tblspace");
    run(&db, &mut s, "alter table t set tablespace pg_default");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["t_idx"]);
    run(&db, &mut s, "alter index t_idx set tablespace pg_default");
    assert!(in_space(&db, &mut s, "regress_tblspace").is_empty());
    // each relation's filenode names it again, and a view's is none
    run(&db, &mut s, "create view v as select 1");
    let unmapped = "select count(*) from pg_class where relkind in ('r', 'i', 'S', 'm') \
                    and pg_filenode_relation(reltablespace, pg_relation_filenode(oid)) is distinct from oid";
    assert_eq!(one(&db, &mut s, unmapped), "0");
    assert_eq!(one(&db, &mut s, "select pg_relation_filenode('v') is null"), "true");

    // a catalog's indexes don't move, nor are they rebuilt concurrently
    assert_eq!(
        fails(&db, &mut s, "reindex (tablespace regress_tblspace) table pg_am"),
        ("0A000", "cannot move system relation \"pg_am_name_index\"".into())
    );
    assert_eq!(
        fails(&db, &mut s, "reindex (tablespace regress_tblspace) index pg_toast.pg_toast_1260_index"),
        ("0A000", "cannot move system relation \"pg_toast_1260_index\"".into())
    );
    assert_eq!(
        fails(&db, &mut s, "reindex (tablespace regress_tblspace) table concurrently pg_authid"),
        ("0A000", "cannot reindex system catalogs concurrently".into())
    );
}

#[test]
fn a_partitioned_tree_moves_its_leaves() {
    let db = Db::new();
    let mut s = session(&db);
    run(&db, &mut s, "create table p (c1 int, c2 int) partition by range (c1)");
    run(&db, &mut s, "create table p_0 partition of p for values from (0) to (10)");
    run(&db, &mut s, "create index p_idx on p (c1) tablespace regress_tblspace");
    run(&db, &mut s, "create table p_1 partition of p for values from (10) to (20)");
    // (a partition's index is where its parent's is, made before or after)
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["p_0_c1_idx", "p_1_c1_idx", "p_idx"]);
    // one a partitioned relation names is a dependent, the others its contents
    let (code, message) = fails(&db, &mut s, "drop tablespace regress_tblspace");
    assert_eq!(
        (code, message.as_str()),
        ("2BP01", "tablespace \"regress_tblspace\" cannot be dropped because some objects depend on it")
    );
    run(&db, &mut s, "alter index p_idx set tablespace pg_default");
    assert_eq!(
        fails(&db, &mut s, "drop tablespace regress_tblspace"),
        ("55000", "tablespace \"regress_tblspace\" is not empty".into())
    );
    assert_eq!(fails(&db, &mut s, "alter index p_idx set tablespace pg_global").0, "22023");
    // REINDEX of the tree rebuilds and moves the leaves' indexes, not the partitioned one
    let before = one(&db, &mut s, "select relfilenode from pg_class where relname = 'p_0_c1_idx'");
    run(&db, &mut s, "alter index all in tablespace regress_tblspace set tablespace pg_default");
    assert!(in_space(&db, &mut s, "regress_tblspace").is_empty());
    run(&db, &mut s, "reindex (tablespace regress_tblspace) table p");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["p_0_c1_idx", "p_1_c1_idx"]);
    assert_ne!(one(&db, &mut s, "select relfilenode from pg_class where relname = 'p_0_c1_idx'"), before);
    assert_eq!(one(&db, &mut s, "select relfilenode from pg_class where relname = 'p_idx'"), "0");
    run(&db, &mut s, "alter index all in tablespace regress_tblspace set tablespace pg_default");
    take_notices();
    run(&db, &mut s, "alter table all in tablespace regress_tblspace set tablespace pg_default");
    let notes: Vec<String> = take_notices().into_iter().map(|n| n.message).collect();
    assert!(notes.contains(&"no matching relations in tablespace \"regress_tblspace\" found".to_string()), "{notes:?}");
    run(&db, &mut s, "drop tablespace regress_tblspace");
}

#[test]
fn a_role_needs_create_on_a_tablespace() {
    let db = Db::new();
    let mut s = session(&db);
    run(&db, &mut s, "create role regress_ts_user1 login");
    run(&db, &mut s, "create role regress_ts_user2 login");
    run(&db, &mut s, "grant all on schema public to public");
    run(&db, &mut s, "alter tablespace regress_tblspace owner to regress_ts_user1");
    assert_eq!(
        one(&db, &mut s, "select pg_get_userbyid(spcowner) from pg_tablespace where spcname = 'regress_tblspace'"),
        "regress_ts_user1"
    );
    // (a GRANT is its transaction's)
    run(&db, &mut s, "begin");
    run(&db, &mut s, "grant all on tablespace regress_tblspace to public");
    run(&db, &mut s, "rollback");
    run(&db, &mut s, "set session role regress_ts_user2");
    let denied = ("42501", "permission denied for tablespace regress_tblspace".to_string());
    assert_eq!(fails(&db, &mut s, "create table tablespace_table (i int) tablespace regress_tblspace"), denied);
    assert_eq!(fails(&db, &mut s, "reindex (tablespace regress_tblspace) table tablespace_table"), denied);
    run(&db, &mut s, "reset role");
    run(&db, &mut s, "grant create on tablespace regress_tblspace to regress_ts_user2");
    let acl = one(&db, &mut s, "select spcacl from pg_tablespace where spcname = 'regress_tblspace'");
    assert!(acl.contains("regress_ts_user2=C/regress_ts_user1"), "{acl}");
    run(&db, &mut s, "set session role regress_ts_user2");
    run(&db, &mut s, "create table tablespace_table (i int) tablespace regress_tblspace");
    run(&db, &mut s, "reset role");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["tablespace_table"]);
}

#[test]
fn a_view_shows_its_own_options() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int)");
    run(&db, &mut s, "create view v1 with (security_barrier) as select * from t");
    run(&db, &mut s, "create view v2 with (security_invoker = false, security_barrier = true) as select * from t");
    run(&db, &mut s, "create table t2 (a int) with (fillfactor = 70)");
    assert_eq!(
        run(&db, &mut s, "select relname, reloptions from pg_class where relname in ('v1', 'v2', 't2') order by 1"),
        [
            vec!["t2".to_string(), "{fillfactor=70}".to_string()],
            vec!["v1".to_string(), "{security_barrier=true}".to_string()],
            vec!["v2".to_string(), "{security_invoker=false,security_barrier=true}".to_string()],
        ]
    );
}

#[test]
fn a_reindex_merges_with_a_change_to_its_table() {
    let db = Db::new();
    let mut main = Session::new(MAIN);
    run(&db, &mut main, "create table m (a int)");
    run(&db, &mut main, "create index m_a on m (a)");
    let node = |s: &mut Session| one(&db, s, "select relfilenode from pg_class where relname = 'm_a'");
    let original = node(&mut main);
    // (a rebuild is maintenance: it is written apart from the table's schema, which the world changes)
    for (n, reindex, both) in
        [(0, "reindex table m", false), (1, "reindex schema public", false), (2, "reindex index m_a", true)]
    {
        let mut w = Session::new(MAIN);
        run(&db, &mut w, &format!("fork world main as w{n}"));
        run(&db, &mut w, &format!("use world w{n}"));
        run(&db, &mut w, &format!("alter table m add column b{n} int"));
        if both {
            run(&db, &mut w, reindex);
        }
        run(&db, &mut main, reindex);
        let rebuilt = node(&mut main);
        assert_ne!(rebuilt, original, "{reindex}");
        run(&db, &mut main, &format!("merge world w{n}"));
        assert_eq!(one(&db, &mut main, &format!("select count(b{n}) from m")), "0", "{reindex}");
        // (both rebuilt it: main's storage stands)
        assert_eq!(node(&mut main), rebuilt, "{reindex}");
    }
    // a dropped table's storage goes with it: one made again by its name starts afresh
    run(&db, &mut main, "drop table m");
    run(&db, &mut main, "create table m (a int)");
    run(&db, &mut main, "create index m_a on m (a)");
    assert_eq!(one(&db, &mut main, "select relfilenode = oid from pg_class where relname = 'm_a'"), "true");
}

#[test]
fn only_an_owner_moves_or_rebuilds_an_index() {
    let db = Db::new();
    let mut s = session(&db);
    run(&db, &mut s, "create role regress_ts_other login");
    run(&db, &mut s, "grant all on schema public to public");
    run(&db, &mut s, "grant create on tablespace regress_tblspace to public");
    run(&db, &mut s, "create schema schema_to_reindex");
    run(&db, &mut s, "create table schema_to_reindex.t1 (a int)");
    run(&db, &mut s, "create index t1_a on schema_to_reindex.t1 (a)");
    run(&db, &mut s, "create table t (a int)");
    run(&db, &mut s, "create index t_a on t (a)");
    let node = one(&db, &mut s, "select relfilenode from pg_class where relname = 't_a'");
    run(&db, &mut s, "set session role regress_ts_other");
    let not_owner = ("42501", "must be owner of index t_a".to_string());
    assert_eq!(fails(&db, &mut s, "alter index t_a set tablespace regress_tblspace"), not_owner);
    assert_eq!(fails(&db, &mut s, "reindex index t_a"), not_owner);
    assert_eq!(fails(&db, &mut s, "reindex (tablespace regress_tblspace) index t_a"), not_owner);
    assert_eq!(
        fails(&db, &mut s, "reindex schema schema_to_reindex"),
        ("42501", "must be owner of schema schema_to_reindex".into())
    );
    run(&db, &mut s, "reset role");
    assert!(in_space(&db, &mut s, "regress_tblspace").is_empty());
    assert_eq!(one(&db, &mut s, "select relfilenode from pg_class where relname = 't_a'"), node);
    // an agent that only reads rebuilds nothing either
    run(&db, &mut s, "create agent reader with (can = 'read')");
    let mut agent = Session::new(MAIN);
    agent.agent = Some("reader".into());
    for q in ["reindex table t", "reindex index t_a", "reindex schema public"] {
        assert_eq!(fails(&db, &mut agent, q).0, "42501", "{q}");
    }
    assert_eq!(one(&db, &mut s, "select relfilenode from pg_class where relname = 't_a'"), node);
}

#[test]
fn a_move_is_checked_before_anything_else_changes() {
    let db = Db::new();
    let mut s = session(&db);
    run(&db, &mut s, "create table k (a int)");
    // the whole ALTER TABLE or none of it
    assert_eq!(
        fails(&db, &mut s, "alter table k add column c int, set tablespace nosuch"),
        ("42704", "tablespace \"nosuch\" does not exist".into())
    );
    assert_eq!(fails(&db, &mut s, "select c from k").0, "42703");
    run(&db, &mut s, "alter table k add column c int, set tablespace regress_tblspace");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["k"]);
    run(&db, &mut s, "alter table k set tablespace pg_default");
    // ALTER INDEX IF EXISTS of one that is there moves it
    run(&db, &mut s, "create index k_a on k (a)");
    take_notices();
    run(&db, &mut s, "alter index if exists k_a set tablespace regress_tblspace");
    assert!(take_notices().is_empty());
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["k_a"]);
    run(&db, &mut s, "alter index if exists k_a set tablespace pg_default");
    // an index made a constraint's stays where it is kept
    run(&db, &mut s, "create unique index k_c on k (c) tablespace regress_tblspace");
    run(&db, &mut s, "alter table k add constraint k_pk primary key using index k_c");
    run(&db, &mut s, "alter table k add column d int");
    run(&db, &mut s, "create unique index k_d on k (d) tablespace regress_tblspace");
    run(&db, &mut s, "alter table k add constraint k_d_key unique using index k_d");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["k_d_key", "k_pk"]);
    // set_config checks default_tablespace as SET does
    assert_eq!(
        fails(&db, &mut s, "select set_config('default_tablespace', 'nosuch', false)"),
        ("22023", "invalid value for parameter \"default_tablespace\": \"nosuch\"".into())
    );
    assert_eq!(
        one(&db, &mut s, "select set_config('default_tablespace', 'regress_tblspace', false)"),
        "regress_tblspace"
    );
}

#[test]
fn each_partition_index_is_its_own_parents() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    // (indexing.sql: an index made ON ONLY, which the partitions have none of, isn't the one attaching gives them)
    for q in [
        "create table parted_inval_tab (a int) partition by range (a)",
        "create index parted_inval_idx on parted_inval_tab (a)",
        "create table parted_inval_tab_1 (a int) partition by range (a)",
        "create table parted_inval_tab_1_1 partition of parted_inval_tab_1 for values from (0) to (10)",
        "create table parted_inval_tab_1_2 partition of parted_inval_tab_1 for values from (10) to (20)",
        "create index parted_inval_ixd_1 on only parted_inval_tab_1 (a)",
        "alter table parted_inval_tab attach partition parted_inval_tab_1 for values from (1) to (100)",
    ] {
        run(&db, &mut s, q);
    }
    let rows = run(
        &db,
        &mut s,
        "select indexrelid::regclass::text, indisvalid, indrelid::regclass::text, \
                coalesce(inhparent::regclass::text, '-') \
           from pg_index idx left join pg_inherits inh on (idx.indexrelid = inh.inhrelid) \
          where indexrelid::regclass::text like 'parted_inval%' \
          order by indexrelid::regclass::text collate \"C\"",
    );
    let want = [
        ["parted_inval_idx", "true", "parted_inval_tab", "-"],
        ["parted_inval_ixd_1", "false", "parted_inval_tab_1", "-"],
        ["parted_inval_tab_1_1_a_idx", "true", "parted_inval_tab_1_1", "parted_inval_tab_1_a_idx"],
        ["parted_inval_tab_1_2_a_idx", "true", "parted_inval_tab_1_2", "parted_inval_tab_1_a_idx"],
        ["parted_inval_tab_1_a_idx", "true", "parted_inval_tab_1", "parted_inval_idx"],
    ];
    assert_eq!(rows, want.map(|r| r.map(String::from).to_vec()));
    // two alike indexes of a partitioned table: a copy of each in each partition, made before or after
    for q in [
        "create table dup (a int) partition by range (a)",
        "create table dup_1 partition of dup for values from (0) to (10)",
        "create index dup_i1 on dup (a)",
        "create index dup_i2 on dup (a)",
        "create table dup_2 partition of dup for values from (10) to (20)",
    ] {
        run(&db, &mut s, q);
    }
    let rows = run(
        &db,
        &mut s,
        "select c.relname, i.inhparent::regclass::text from pg_class c join pg_inherits i on i.inhrelid = c.oid \
          where c.relname like 'dup___a_idx%' order by 1",
    );
    let want =
        [["dup_1_a_idx", "dup_i1"], ["dup_1_a_idx1", "dup_i2"], ["dup_2_a_idx", "dup_i1"], ["dup_2_a_idx1", "dup_i2"]];
    assert_eq!(rows, want.map(|r| r.map(String::from).to_vec()));
}

/// Where a temporary table is kept, as Postgres's GetDefaultTablespace chooses for one: temp_tablespaces, not
/// default_tablespace (the database's default where it names none); and a tablespace holding one isn't empty.
#[test]
fn temporary_tables_follow_temp_tablespaces() {
    let db = Db::new();
    let mut s = session(&db);
    let space_of = |s: &mut Session, rel: &str| {
        one(
            &db,
            s,
            &format!(
                "select coalesce(t.spcname, 'default') from pg_class c left join pg_tablespace t \
                 on t.oid = c.reltablespace where c.relname = '{rel}'"
            ),
        )
    };
    run(&db, &mut s, "create role regress_tmp_user login; grant all on schema public to public");
    // default_tablespace names one the role may not create in: a temporary table isn't put there, nor refused
    run(&db, &mut s, "set session role regress_tmp_user; set default_tablespace = regress_tblspace");
    assert_eq!(fails(&db, &mut s, "create table perm (a int)").0, "42501");
    run(&db, &mut s, "create temp table tmp1 (a int); create index tmp1_a on tmp1 (a)");
    assert_eq!(space_of(&mut s, "tmp1"), "default");
    assert_eq!(space_of(&mut s, "tmp1_a"), "default");
    run(&db, &mut s, "reset role; reset default_tablespace");
    // temp_tablespaces: its first that the role may create in
    run(&db, &mut s, "set temp_tablespaces = regress_tblspace");
    run(&db, &mut s, "create temp table tmp2 (a int); create index tmp2_a on tmp2 (a)");
    assert_eq!(space_of(&mut s, "tmp2"), "regress_tblspace");
    assert_eq!(space_of(&mut s, "tmp2_a"), "regress_tblspace");
    run(&db, &mut s, "create table perm2 (a int)");
    assert_eq!(space_of(&mut s, "perm2"), "default");
    run(&db, &mut s, "set session role regress_tmp_user; create temp table tmp3 (a int); reset role");
    assert_eq!(space_of(&mut s, "tmp3"), "default");
    run(&db, &mut s, "reset temp_tablespaces");
    // DROP TABLESPACE: the session's temporary relations are in it too
    let not_empty = ("55000", "tablespace \"regress_tblspace\" is not empty".to_string());
    assert_eq!(fails(&db, &mut s, "drop tablespace regress_tblspace"), not_empty);
    run(&db, &mut s, "drop table tmp2; create temp table tmp4 (a int) tablespace regress_tblspace");
    assert_eq!(fails(&db, &mut s, "drop tablespace regress_tblspace"), not_empty);
    run(&db, &mut s, "drop table tmp4; drop tablespace regress_tblspace");
}

/// REINDEX DATABASE rebuilds the database's own indexes, those of tables and materialized views the role owns (a
/// partitioned table's are its partitions'), moving them with TABLESPACE; outside a transaction block only.
#[test]
fn reindex_database_rebuilds_the_databases_indexes() {
    let db = Db::new();
    let mut s = session(&db);
    run(
        &db,
        &mut s,
        "create role regress_rdb_user login; grant all on schema public to public;
         create table t (a int primary key, b int); create index t_b on t (b);
         create materialized view mv as select a from t; create index mv_a on mv (a);
         create table p (a int) partition by range (a); create table p1 partition of p for values from (0) to (10);
         create index p_a on p (a);
         set session role regress_rdb_user; create table mine (a int); create index mine_a on mine (a); reset role",
    );
    let node =
        |s: &mut Session, rel: &str| one(&db, s, &format!("select relfilenode from pg_class where relname = '{rel}'"));
    let names = ["t_pkey", "t_b", "mv_a", "p1_a_idx", "mine_a"];
    let before: Vec<String> = names.iter().map(|n| node(&mut s, n)).collect();
    run(&db, &mut s, "begin");
    assert_eq!(
        fails(&db, &mut s, "reindex database regression"),
        ("25001", "REINDEX DATABASE cannot run inside a transaction block".into())
    );
    run(&db, &mut s, "rollback");
    // a role rebuilds those it owns
    run(&db, &mut s, "set session role regress_rdb_user; reindex database regression; reset role");
    let after: Vec<String> = names.iter().map(|n| node(&mut s, n)).collect();
    assert_eq!(after[..4], before[..4]);
    assert_ne!(after[4], before[4]);
    // a superuser all of them, and TABLESPACE moves them
    run(&db, &mut s, "reindex (tablespace regress_tblspace) database regression");
    for (n, (b, a)) in names.iter().zip(before.iter().zip(&after)) {
        let now = node(&mut s, n);
        assert!(now != *b && now != *a, "{n}");
    }
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["mine_a", "mv_a", "p1_a_idx", "t_b", "t_pkey"]);
    // (REINDEX SYSTEM rebuilds the catalogs' only)
    run(&db, &mut s, "reindex system regression");
    assert_eq!(in_space(&db, &mut s, "regress_tblspace"), ["mine_a", "mv_a", "p1_a_idx", "t_b", "t_pkey"]);
}
