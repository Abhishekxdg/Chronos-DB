//! Review of the index access method change (tests/regress_index_am_props.rs): what an index keeps beyond its
//! entries (its access method, ordering, INCLUDE columns and operator classes) surviving a reopen and a world's
//! merge, and the places that compare two indexes or check what CREATE INDEX said doing so as Postgres 16 does.
//! Each expected value is what Postgres 16.15 gives.

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

/// Each index of a table whose name starts with `prefix`: its name, access method and the index it is a partition
/// of ("-" for none).
fn attached(db: &Db, s: &mut Session, prefix: &str) -> Vec<Vec<String>> {
    run(
        db,
        s,
        &format!(
            "select c.relname, a.amname, coalesce(i.inhparent::regclass::text, '-') from pg_class c \
             join pg_am a on a.oid = c.relam left join pg_inherits i on i.inhrelid = c.oid \
             where c.relkind = 'i' and c.relname like '{prefix}%' order by 1"
        ),
    )
}

fn strs(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect()
}

const DEFS: &str = "select indexdef from pg_indexes where tablename = 'k' order by indexname";

#[test]
fn what_an_index_said_survives_a_reopen() {
    let dir = std::env::temp_dir().join(format!("chronos-am-review-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let before = {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(MAIN);
        run(&db, &mut s, "create table k (a int, b text, c int, p point)");
        run(&db, &mut s, "create index k_desc on k (a desc nulls last, b text_pattern_ops) include (c)");
        run(&db, &mut s, "create index k_hash on k using hash (c)");
        run(&db, &mut s, "create index k_kd on k using spgist (p kd_point_ops)");
        db.checkpoint().unwrap();
        run(&db, &mut s, DEFS)
    };
    assert_eq!(
        before,
        strs(&[
            &["CREATE INDEX k_desc ON public.k USING btree (a DESC NULLS LAST, b text_pattern_ops) INCLUDE (c)"],
            &["CREATE INDEX k_hash ON public.k USING hash (c)"],
            &["CREATE INDEX k_kd ON public.k USING spgist (p kd_point_ops)"],
        ])
    );
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(run(&db, &mut s, DEFS), before);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn what_an_index_said_in_a_world_is_merged() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    run(&db, &mut s, "create table k (a int, b text, c int)");
    db.fork(MAIN, "w").unwrap();
    let mut w = Session::new("w");
    run(&db, &mut w, "create index k_desc on k (a desc, b) include (c)");
    run(&db, &mut w, "create index k_hash on k using hash (c)");
    db.merge("w", chronos::Resolve::Fail).unwrap();
    assert_eq!(
        run(&db, &mut s, DEFS),
        strs(&[
            &["CREATE INDEX k_desc ON public.k USING btree (a DESC, b) INCLUDE (c)"],
            &["CREATE INDEX k_hash ON public.k USING hash (c)"],
        ])
    );
}

#[test]
fn attaching_a_partition_leaves_its_index_of_another_method() {
    // (AttachPartitionEnsureIndexes: CompareIndexInfo, index.c:2555, wants the same access method; the partition
    // gets a btree of its own)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table p (a int, b int) partition by range (a)");
    run(&db, &mut s, "create table p1 (a int, b int)");
    run(&db, &mut s, "create index p1_hash on p1 using hash (a)");
    run(&db, &mut s, "create index p_a on p (a)");
    run(&db, &mut s, "alter table p attach partition p1 for values from (0) to (100)");
    assert_eq!(attached(&db, &mut s, "p1"), strs(&[&["p1_a_idx", "btree", "p_a"], &["p1_hash", "hash", "-"]]));
}

#[test]
fn a_parents_index_isnt_a_partitions_of_another_class_or_include() {
    // (DefineIndex's search of each partition's indexes: CompareIndexInfo compares the operator families and the
    // INCLUDE columns)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table q (a text) partition by range (a)");
    run(&db, &mut s, "create table q1 partition of q for values from ('a') to ('m')");
    run(&db, &mut s, "create index q1_plain on q1 (a)");
    run(&db, &mut s, "create index q_pat on q (a text_pattern_ops)");
    assert_eq!(attached(&db, &mut s, "q1"), strs(&[&["q1_a_idx", "btree", "q_pat"], &["q1_plain", "btree", "-"]]));
    run(&db, &mut s, "create table w (a text, b int) partition by range (a)");
    run(&db, &mut s, "create table w1 partition of w for values from ('a') to ('m')");
    run(&db, &mut s, "create index w1_plain on w1 (a)");
    run(&db, &mut s, "create index w_inc on w (a) include (b)");
    assert_eq!(attached(&db, &mut s, "w1"), strs(&[&["w1_a_b_idx", "btree", "w_inc"], &["w1_plain", "btree", "-"]]));
}

#[test]
fn attach_index_refuses_one_of_another_method() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table r (a int) partition by range (a)");
    run(&db, &mut s, "create table r1 partition of r for values from (0) to (10)");
    run(&db, &mut s, "create index r_a on only r (a)");
    run(&db, &mut s, "create index r1_h on r1 using hash (a)");
    assert_eq!(
        fails(&db, &mut s, "alter index r_a attach partition r1_h"),
        ("42P17", "cannot attach index \"r1_h\" as a partition of index \"r_a\"".to_string())
    );
    // (Postgres 16's CompareIndexInfo doesn't compare indoption: a DESC one is attached)
    run(&db, &mut s, "create index r1_d on r1 (a desc)");
    run(&db, &mut s, "alter index r_a attach partition r1_d");
    assert_eq!(attached(&db, &mut s, "r1"), strs(&[&["r1_d", "btree", "r_a"], &["r1_h", "hash", "-"]]));
}

#[test]
fn a_types_default_class_written_out_follows_the_columns_type() {
    // (Postgres keeps the class's oid, and ALTER TYPE makes the index again from pg_get_indexdef, which leaves a
    // default class out: the new type's default is the index's)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table s (n int)");
    run(&db, &mut s, "create index s_n on s (n int4_ops)");
    run(&db, &mut s, "alter table s alter n type bigint");
    assert_eq!(
        run(
            &db,
            &mut s,
            "select pg_get_indexdef(indexrelid), indclass::text from pg_index where indexrelid = 's_n'::regclass"
        ),
        strs(&[&["CREATE INDEX s_n ON public.s USING btree (n)", "3124"]])
    );
    // (and a class that isn't one of the new type's is refused, as DefineIndex's ResolveOpClass refuses it)
    run(&db, &mut s, "create table s2 (t text)");
    run(&db, &mut s, "create index s2_t on s2 (t text_pattern_ops)");
    assert_eq!(
        fails(&db, &mut s, "alter table s2 alter t type int using 0"),
        ("42804", "operator class \"text_pattern_ops\" does not accept data type integer".to_string())
    );
}

#[test]
fn a_missing_column_is_said_before_its_ordering_or_class() {
    // (ComputeIndexAttrs looks the column up first, indexcmds.c:1890, then its class and ordering)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table s (n int)");
    assert_eq!(fails(&db, &mut s, "create index on s using hash (nope desc)").0, "42703");
    assert_eq!(fails(&db, &mut s, "create index on s using gin (nope nulls first)").0, "42703");
    assert_eq!(fails(&db, &mut s, "create index on s (nope foo_ops)").0, "42703");
}

#[test]
fn include_columns_of_spgist_and_copies() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table sp (p point, n int)");
    run(&db, &mut s, "create index sp_x on sp using spgist (p) include (n)");
    let props: Vec<String> = ["orderable", "distance_orderable", "returnable", "search_nulls"]
        .iter()
        .map(|p| one(&db, &mut s, &format!("select pg_index_column_has_property('sp_x'::regclass, 2, '{p}')")))
        .collect();
    assert_eq!(props, ["false", "", "true", ""]);
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('sp_x'::regclass)"),
        "CREATE INDEX sp_x ON public.sp USING spgist (p) INCLUDE (n)"
    );
    // LIKE ... INCLUDING INDEXES: the copy keeps them, and is named for all its columns
    run(&db, &mut s, "create table l (a int, b int, c text)");
    run(&db, &mut s, "create index l_x on l using btree (a desc, c text_pattern_ops) include (b)");
    run(&db, &mut s, "create table l2 (like l including indexes)");
    assert_eq!(
        one(&db, &mut s, "select indexdef from pg_indexes where tablename = 'l2'"),
        "CREATE INDEX l2_a_c_b_idx ON public.l2 USING btree (a DESC, c text_pattern_ops) INCLUDE (b)"
    );
}

#[test]
fn an_include_column_a_foreign_key_needs_isnt_dropped_alone() {
    // (the foreign key depends on the unique index, which depends on its INCLUDE column: RESTRICT refuses, 2BP01)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int, b int)");
    run(&db, &mut s, "create unique index t_a_b on t (a) include (b)");
    run(&db, &mut s, "create table x (a int references t (a))");
    assert_eq!(fails(&db, &mut s, "alter table t drop column b").0, "2BP01");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_constraint where conname = 'x_a_fkey'"), "1");
    // (the key the foreign key points at stays unique)
    assert_eq!(fails(&db, &mut s, "insert into t values (1, 1), (1, 2)").0, "23505");
}

#[test]
fn diff_as_sql_writes_what_an_index_said() {
    // (replayed into another database, the indexes are the world's: method, ordering, class and INCLUDE)
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create world amw");
    let mut w = Session::new("amw");
    run(&db, &mut w, "create table k (a int, b text, c int, p point)");
    run(&db, &mut w, "create index k_desc on k (a desc nulls last, b text_pattern_ops) include (c)");
    run(&db, &mut w, "create index k_hash on k using hash (c)");
    run(&db, &mut w, "create index k_kd on k using spgist (p kd_point_ops)");
    let want = run(&db, &mut w, DEFS);
    let sql: Vec<String> = run(&db, &mut w, "diff world amw as sql").into_iter().map(|r| r[0].clone()).collect();
    let (other, mut o) = (Db::new(), Session::new(MAIN));
    for stmt in &sql {
        o.run(&other, stmt, &[]).unwrap_or_else(|e| panic!("{stmt}: {} {}", e.code, e.message));
    }
    assert_eq!(run(&other, &mut o, DEFS), want, "{sql:?}");
}
