//! An index's access method, its columns' ordering and operator classes and its INCLUDE columns, kept as CREATE
//! INDEX wrote them and shown where Postgres shows them (pg_class.relam, pg_index, pg_get_indexdef, pg_attribute),
//! and the index property functions (pg_indexam_has_property, pg_index_has_property, pg_index_column_has_property)
//! answering as Postgres 16's access methods do (its regression suite's amutils.sql). Each expected value is what
//! Postgres 16 gives.

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

/// Each of `props`' values from `call` (which has `{}` where the property's name goes), one letter each: `t`, `f`,
/// or `-` for NULL.
fn props(db: &Db, s: &mut Session, call: &str, props: &[&str]) -> String {
    props
        .iter()
        .map(|p| match one(db, s, &format!("select {}", call.replace("{}", &format!("'{p}'")))).as_str() {
            "true" => 't',
            "false" => 'f',
            _ => '-',
        })
        .collect()
}

/// A column's properties, in amutils.sql's order.
const COLUMN: [&str; 10] = [
    "asc",
    "desc",
    "nulls_first",
    "nulls_last",
    "orderable",
    "distance_orderable",
    "returnable",
    "search_array",
    "search_nulls",
    "bogus",
];

fn column(db: &Db, s: &mut Session, index: &str, k: i64) -> String {
    props(db, s, &format!("pg_index_column_has_property('{index}'::regclass, {k}, {{}})"), &COLUMN)
}

fn index(db: &Db, s: &mut Session, rel: &str) -> String {
    props(
        db,
        s,
        &format!("pg_index_has_property('{rel}'::regclass, {{}})"),
        &["clusterable", "index_scan", "bitmap_scan", "backward_scan", "can_order", "bogus"],
    )
}

#[test]
fn access_methods_report_what_they_can_do() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let am = |db: &Db, s: &mut Session, name: &str| {
        props(
            db,
            s,
            &format!("pg_indexam_has_property((select oid from pg_am where amname = '{name}'), {{}})"),
            &["can_order", "can_unique", "can_multi_col", "can_exclude", "can_include", "clusterable", "bogus"],
        )
    };
    assert_eq!(am(&db, &mut s, "btree"), "ttttt--");
    assert_eq!(am(&db, &mut s, "hash"), "ffftf--");
    assert_eq!(am(&db, &mut s, "gist"), "ffttt--");
    assert_eq!(am(&db, &mut s, "spgist"), "ffftt--");
    assert_eq!(am(&db, &mut s, "gin"), "fftff--");
    assert_eq!(am(&db, &mut s, "brin"), "fftff--");
    // (a table access method has none of an index's, nor does an oid of no method)
    assert_eq!(am(&db, &mut s, "heap"), "-------");
    assert_eq!(one(&db, &mut s, "select pg_indexam_has_property(0, 'can_order')"), "");
    // (a property's name in any case)
    assert_eq!(one(&db, &mut s, "select pg_indexam_has_property(403, 'CAN_ORDER')"), "true");
}

#[test]
fn btree_columns_and_their_ordering() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create temp table foo (f1 int, f2 int, f3 int, f4 int)");
    // (an index of a temporary table is found by its name, as the table is)
    run(&db, &mut s, "create index fooindex on foo (f1 desc, f2 asc, f3 nulls first, f4 nulls last)");
    assert_eq!(column(&db, &mut s, "fooindex", 1), "fttftfttt-");
    assert_eq!(column(&db, &mut s, "fooindex", 2), "tffttfttt-");
    assert_eq!(column(&db, &mut s, "fooindex", 3), "tftftfttt-");
    assert_eq!(column(&db, &mut s, "fooindex", 4), "tffttfttt-");
    // (past the index's columns, and column 0 or less: NULL)
    assert_eq!(column(&db, &mut s, "fooindex", 5), "----------");
    assert_eq!(column(&db, &mut s, "fooindex", 0), "----------");
    assert_eq!(
        one(&db, &mut s, "select split_part(pg_get_indexdef('fooindex'::regclass), ' USING ', 2)"),
        "btree (f1 DESC, f2, f3 NULLS FIRST, f4)"
    );
    assert_eq!(
        run(
            &db,
            &mut s,
            "select indnatts, indnkeyatts, indkey::text, indoption::text from pg_index \
             where indexrelid = 'fooindex'::regclass"
        ),
        vec![vec!["4", "4", "1 2 3 4", "3 0 2 0"]]
    );
    // the index's own properties, and a table's (NULL: it isn't an index)
    assert_eq!(index(&db, &mut s, "fooindex"), "tttt--");
    assert_eq!(index(&db, &mut s, "foo"), "------");
    // an index with DESC can't be a constraint's
    run(&db, &mut s, "create table cwi (a int, b int)");
    run(&db, &mut s, "create unique index cwi_desc on cwi (a desc)");
    let (code, message) = fails(&db, &mut s, "alter table cwi add unique using index cwi_desc");
    assert_eq!(
        (code, message.as_str()),
        ("42809", "index \"cwi_desc\" column number 1 does not have default sorting behavior")
    );
}

#[test]
fn include_columns_are_kept_and_shown() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table foo (f1 int, f2 int, f3 text, f4 box)");
    run(&db, &mut s, "create index foocover on foo (f1) include (f2, f3)");
    assert_eq!(column(&db, &mut s, "foocover", 1), "tffttfttt-");
    // (an INCLUDE column isn't ordered, but gives its value back)
    assert_eq!(column(&db, &mut s, "foocover", 2), "----fft---");
    assert_eq!(column(&db, &mut s, "foocover", 3), "----fft---");
    assert_eq!(column(&db, &mut s, "foocover", 4), "----------");
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('foocover'::regclass)"),
        "CREATE INDEX foocover ON public.foo USING btree (f1) INCLUDE (f2, f3)"
    );
    assert_eq!(one(&db, &mut s, "select pg_get_indexdef('foocover'::regclass, 3, true)"), "f3");
    assert_eq!(
        run(
            &db,
            &mut s,
            "select indnatts, indnkeyatts, indkey::text, indcollation::text, indclass::text, indoption::text \
             from pg_index where indexrelid = 'foocover'::regclass"
        ),
        vec![vec!["3", "1", "1 2 3", "0", "1978", "0"]]
    );
    // psql's \d of the index: its columns, which are keys, and (INCLUDE's) no collation
    assert_eq!(
        run(
            &db,
            &mut s,
            "select attname, attnum <= (select indnkeyatts from pg_index where indexrelid = 'foocover'::regclass), \
             attcollation from pg_attribute where attrelid = 'foocover'::regclass order by attnum"
        ),
        vec![vec!["f1", "true", "0"], vec!["f2", "false", "0"], vec!["f3", "false", "0"]]
    );
    // a gist index's INCLUDE column: what its operator class would say is NULL
    run(&db, &mut s, "create index foo_gist on foo using gist (f4) include (f1)");
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('foo_gist'::regclass)"),
        "CREATE INDEX foo_gist ON public.foo USING gist (f4) INCLUDE (f1)"
    );
    assert_eq!(column(&db, &mut s, "foo_gist", 2), "----f-----");
    // dropping an INCLUDE column drops the index
    run(&db, &mut s, "alter table foo drop column f3");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_indexes where indexname = 'foocover'"), "0");
    // (a partition's copy of an index keeps them, and is named for all its columns)
    run(&db, &mut s, "create table pt (a int, b int, c text) partition by range (a)");
    run(&db, &mut s, "create table pt1 partition of pt for values from (0) to (10)");
    run(&db, &mut s, "create index pt_idx on pt (a desc, b) include (c)");
    assert_eq!(
        run(&db, &mut s, "select indexdef from pg_indexes where tablename in ('pt', 'pt1') order by indexname"),
        vec![
            vec!["CREATE INDEX pt1_a_b_c_idx ON public.pt1 USING btree (a DESC, b) INCLUDE (c)"],
            vec!["CREATE INDEX pt_idx ON ONLY public.pt USING btree (a DESC, b) INCLUDE (c)"],
        ]
    );
}

#[test]
fn each_access_method_is_kept_and_answers_for_itself() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table g (p point, c circle, t text, n int, a int[], i inet)");
    for q in [
        "create index g_hash on g using hash (n)",
        "create index g_circle on g using gist (c)",
        "create index g_quad on g using spgist (p)",
        "create index g_radix on g using spgist (t)",
        "create index g_gin on g using gin (a)",
        "create index g_brin on g using brin (n, i inet_inclusion_ops)",
    ] {
        run(&db, &mut s, q);
    }
    assert_eq!(
        run(
            &db,
            &mut s,
            "select c.relname, a.amname from pg_class c join pg_am a on a.oid = c.relam \
             where c.relname like 'g\\_%' order by 1"
        ),
        vec![
            vec!["g_brin", "brin"],
            vec!["g_circle", "gist"],
            vec!["g_gin", "gin"],
            vec!["g_hash", "hash"],
            vec!["g_quad", "spgist"],
            vec!["g_radix", "spgist"],
        ]
    );
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('g_hash'::regclass)"),
        "CREATE INDEX g_hash ON public.g USING hash (n)"
    );
    // (inet_inclusion_ops is brin's default for inet: not shown)
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('g_brin'::regclass)"),
        "CREATE INDEX g_brin ON public.g USING brin (n, i)"
    );
    // amutils.sql's comparison of the methods' first columns
    assert_eq!(column(&db, &mut s, "g_hash", 1), "fffffffff-");
    assert_eq!(column(&db, &mut s, "g_circle", 1), "ffffftfft-");
    assert_eq!(column(&db, &mut s, "g_quad", 1), "fffffttft-");
    assert_eq!(column(&db, &mut s, "g_radix", 1), "fffffftft-");
    assert_eq!(column(&db, &mut s, "g_gin", 1), "fffffffff-");
    assert_eq!(column(&db, &mut s, "g_brin", 1), "fffffffft-");
    assert_eq!(index(&db, &mut s, "g_hash"), "fttt--");
    assert_eq!(index(&db, &mut s, "g_circle"), "tttf--");
    assert_eq!(index(&db, &mut s, "g_quad"), "fttf--");
    assert_eq!(index(&db, &mut s, "g_gin"), "fftf--");
    assert_eq!(index(&db, &mut s, "g_brin"), "fftf--");
    // the classes: Postgres's own by oid, and the column's key type where it isn't the column's
    assert_eq!(
        run(
            &db,
            &mut s,
            "select indclass::text from pg_index where indexrelid in \
             ('g_circle'::regclass, 'g_quad'::regclass, 'g_gin'::regclass) order by indexrelid::regclass::text"
        ),
        vec![vec!["10063"], vec!["10064"], vec!["10084"]]
    );
    assert_eq!(
        one(&db, &mut s, "select atttypid::regtype::text from pg_attribute where attrelid = 'g_circle'::regclass"),
        "box"
    );
}

#[test]
fn what_an_access_method_cant_do_is_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table g (p point, b box, n int, t text, a int[])");
    for (q, message) in [
        ("create unique index on g using hash (n)", "access method \"hash\" does not support unique indexes"),
        ("create index on g using hash (n, t)", "access method \"hash\" does not support multicolumn indexes"),
        ("create index on g using gin (a desc)", "access method \"gin\" does not support ASC/DESC options"),
        (
            "create index on g using gist (p nulls first)",
            "access method \"gist\" does not support NULLS FIRST/LAST options",
        ),
        ("create index on g using brin (n) include (t)", "access method \"brin\" does not support included columns"),
        (
            "create unique index on g using gist (p) include (n)",
            "access method \"gist\" does not support unique indexes",
        ),
        ("create index on g using gin (n, t) include (a)", "access method \"gin\" does not support included columns"),
    ] {
        assert_eq!(fails(&db, &mut s, q), ("0A000", message.to_string()), "{q}");
    }
    assert_eq!(
        fails(&db, &mut s, "create index on g (n foo_ops)"),
        ("42704", "operator class \"foo_ops\" does not exist for access method \"btree\"".to_string())
    );
    // rtree is gist, as Postgres substitutes it
    run(&db, &mut s, "create index g_rtree on g using rtree (b)");
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('g_rtree'::regclass)"),
        "CREATE INDEX g_rtree ON public.g USING gist (b)"
    );
}

#[test]
fn operator_classes_show_where_they_arent_the_default() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table ix (v varchar(10), t text, n int, p point)");
    run(
        &db,
        &mut s,
        "create index ix_a on ix (v, t text_pattern_ops desc nulls last, n int4_ops, v varchar_pattern_ops, \
         t collate \"C\" text_ops)",
    );
    run(&db, &mut s, "create index ix_kd on ix using spgist (p kd_point_ops)");
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('ix_a'::regclass)"),
        "CREATE INDEX ix_a ON public.ix USING btree (v, t text_pattern_ops DESC NULLS LAST, n, v varchar_pattern_ops, \
         t COLLATE \"C\")"
    );
    assert_eq!(
        one(
            &db,
            &mut s,
            "select indclass::text || ' / ' || indoption::text from pg_index where indexrelid = 'ix_a'::regclass"
        ),
        "3126 4217 1978 4218 3126 / 0 1 0 0 0"
    );
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('ix_kd'::regclass)"),
        "CREATE INDEX ix_kd ON public.ix USING spgist (p kd_point_ops)"
    );
    // (a column alone, as pg_get_indexdef gives one: without its class or ordering)
    assert_eq!(one(&db, &mut s, "select pg_get_indexdef('ix_a'::regclass, 2, true)"), "t");
}

#[test]
fn a_class_create_operator_class_made_is_one_an_index_names() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table h (i int)");
    run(&db, &mut s, "insert into h select g from generate_series(1, 20) g");
    run(
        &db,
        &mut s,
        "create operator class int4_custom_ops for type int4 using btree as \
         operator 1 < (int4, int4), operator 3 = (int4, int4), function 1 btint4cmp(int4, int4)",
    );
    run(&db, &mut s, "create unique index h_custom on h using btree (i int4_custom_ops)");
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('h_custom'::regclass)"),
        "CREATE UNIQUE INDEX h_custom ON public.h USING btree (i int4_custom_ops)"
    );
    // (one of another access method is no class of this one's)
    assert_eq!(
        fails(&db, &mut s, "create index on h using hash (i int4_custom_ops)"),
        ("42704", "operator class \"int4_custom_ops\" does not exist for access method \"hash\"".to_string())
    );
}

#[test]
fn what_an_index_column_can_be_altered_in() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table tbl (c1 int, c2 int)");
    run(&db, &mut s, "create index tbl_idx on tbl (c1, (c1 + 0)) include (c2)");
    run(&db, &mut s, "alter index tbl_idx alter column 2 set statistics 1000");
    assert_eq!(
        fails(&db, &mut s, "alter index tbl_idx alter column 3 set statistics 1000"),
        ("0A000", "cannot alter statistics on included column \"c2\" of index \"tbl_idx\"".to_string())
    );
    assert_eq!(fails(&db, &mut s, "alter index tbl_idx alter column 4 set statistics 1000").0, "42703");
    // a table isn't clustered on an index of a method that doesn't order
    run(&db, &mut s, "create index tbl_hash on tbl using hash (c2)");
    assert_eq!(
        fails(&db, &mut s, "cluster tbl using tbl_hash"),
        ("0A000", "cannot cluster on index \"tbl_hash\" because access method does not support clustering".to_string())
    );
}

#[test]
fn a_class_is_compared_and_followed_as_postgres_does() {
    // a partition's index matches its parent's where their classes are of one family (CompareIndexInfo compares
    // opfamilies): varchar_ops is text_ops's family, the default for a varchar, and is shown as written
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table pv (v varchar) partition by list (v)");
    run(&db, &mut s, "create table pv1 partition of pv for values in ('a')");
    run(&db, &mut s, "create index pv1_plain on pv1 (v)");
    run(&db, &mut s, "create index pv_x on pv (v varchar_ops)");
    assert_eq!(
        run(
            &db,
            &mut s,
            "select c.relname, coalesce(i.inhparent::regclass::text, '-') from pg_class c \
             left join pg_inherits i on i.inhrelid = c.oid where c.relkind = 'i' and c.relname like 'pv%' order by 1"
        ),
        [["pv1_plain", "pv_x"]]
    );
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('pv_x'::regclass)"),
        "CREATE INDEX pv_x ON ONLY public.pv USING btree (v varchar_ops)"
    );
    // DROP COLUMN ... CASCADE of an INCLUDE column takes the foreign key that depended on its unique index
    run(&db, &mut s, "create table t (a int, b int)");
    run(&db, &mut s, "create unique index t_a_b on t (a) include (b)");
    run(&db, &mut s, "create table x (a int references t (a))");
    let (code, _) = fails(&db, &mut s, "alter table t drop column b");
    assert_eq!(code, "2BP01");
    run(&db, &mut s, "alter table t drop column b cascade");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_constraint where conname = 'x_a_fkey'"), "0");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_class where relname = 't_a_b'"), "0");
    // a type's default class written out is none: ALTER TYPE gives the index the new type's (int2_ops, int4_ops)
    run(&db, &mut s, "create table s5 (n smallint)");
    run(&db, &mut s, "create index s5_n on s5 (n int2_ops)");
    run(&db, &mut s, "alter table s5 alter n type int");
    run(&db, &mut s, "create table s6 (n int)");
    run(&db, &mut s, "create index s6_n on s6 using hash (n int4_ops)");
    run(&db, &mut s, "alter table s6 alter n type bigint");
    assert_eq!(
        run(
            &db,
            &mut s,
            "select pg_get_indexdef(indexrelid), indclass::text from pg_index \
             where indexrelid in ('s5_n'::regclass, 's6_n'::regclass) order by 1"
        ),
        [
            ["CREATE INDEX s5_n ON public.s5 USING btree (n)", "1978"],
            ["CREATE INDEX s6_n ON public.s6 USING hash (n)", "10021"]
        ]
    );
    // and one that isn't the old type's default, but takes the new type, stays (varchar(10) to text)
    run(&db, &mut s, "create table s3 (t varchar(10))");
    run(&db, &mut s, "create index s3_t on s3 (t text_pattern_ops)");
    run(&db, &mut s, "alter table s3 alter t type text");
    assert_eq!(
        one(&db, &mut s, "select pg_get_indexdef('s3_t'::regclass)"),
        "CREATE INDEX s3_t ON public.s3 USING btree (t text_pattern_ops)"
    );
}
