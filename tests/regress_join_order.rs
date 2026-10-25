//! PostgreSQL 16's join order for cross joins and nested-loop-only joins (see `sql::opt`), which the executor
//! follows under `chronos.follow_pg_plan`, and the statistics it is computed from: VACUUM's and ANALYZE's
//! relpages, reltuples and relallvisible in pg_class, from a model of Postgres's heap. Every expected order and
//! number here is what a real Postgres 16.15 gives (EXPLAIN, pg_class) for the same statements.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code, e.message),
    }
}

/// join.sql's two tables, never vacuumed.
fn j_tables(db: &Db, s: &mut Session) {
    run(db, s, "create table j1_tbl (i integer, j integer, t text); create table j2_tbl (i integer, k integer)");
    run(
        db,
        s,
        "insert into j1_tbl values (1, 4, 'one'), (2, 3, 'two'), (3, 2, 'three'), (4, 1, 'four'), (5, 0, 'five'),
         (6, 6, 'six'), (7, 7, 'seven'), (8, 8, 'eight'), (0, null, 'zero'), (null, null, 'null'), (null, 0, 'zero')",
    );
    run(
        db,
        s,
        "insert into j2_tbl values (1, -1), (2, 2), (3, -3), (2, 4), (5, -5), (5, -5), (0, null), (null, null),
         (null, 0)",
    );
}

/// The first `n` rows' values of columns `cols`, joined with commas.
fn firsts(rows: &[Vec<String>], n: usize, cols: &[usize]) -> Vec<String> {
    rows.iter().take(n).map(|r| cols.iter().map(|&c| r[c].clone()).collect::<Vec<_>>().join(",")).collect()
}

#[test]
fn a_cross_join_returns_its_rows_in_postgres_order() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    j_tables(&db, &mut s);
    // J1_TBL is 1,200 rows by the 10-page rule, J2_TBL 2,260: a nested loop with J2 outer costs 33957.60, with J1
    // outer 33960.25, so Postgres puts J2 outer, J1 (materialized) inner
    run(&db, &mut s, "set chronos.follow_pg_plan = on");
    let rows = run(&db, &mut s, "select * from j1_tbl cross join j2_tbl");
    assert_eq!(rows.len(), 99);
    assert_eq!(firsts(&rows, 3, &[0, 3, 4]), ["1,1,-1", "2,1,-1", "3,1,-1"]);
    assert_eq!(firsts(&rows[11..], 1, &[0, 3, 4]), ["1,2,2"]);
    // (the same in comma form, and with the tables the other way round)
    assert_eq!(run(&db, &mut s, "select * from j1_tbl, j2_tbl"), rows);
    let swapped = run(&db, &mut s, "select j1_tbl.*, j2_tbl.* from j2_tbl, j1_tbl");
    assert_eq!(swapped, rows);
    // three tables: J2 a outer of J1, and J2 b innermost (EXPLAIN: Nested Loop (Nested Loop (a, J1), b))
    let rows = run(&db, &mut s, "select * from j1_tbl cross join j2_tbl a cross join j2_tbl b");
    assert_eq!(rows.len(), 891);
    assert_eq!(firsts(&rows, 2, &[0, 3, 5]), ["1,1,1", "1,1,2"]);
    assert_eq!(firsts(&rows[9..], 1, &[0, 3, 5]), ["2,1,1"]);
    assert_eq!(firsts(&rows[99..], 1, &[0, 3, 5]), ["1,2,1"]);
    // a join written as a subquery Postgres pulls up is ordered as the query's own
    let rows = run(&db, &mut s, "select ii, tt, kk from (j1_tbl cross join j2_tbl) as tx (ii, jj, tt, ii2, kk)");
    assert_eq!(firsts(&rows, 2, &[0, 2]), ["1,-1", "2,-1"]);
    // off: the executor's own order (the first table outer)
    run(&db, &mut s, "set chronos.follow_pg_plan = off");
    let rows = run(&db, &mut s, "select * from j1_tbl cross join j2_tbl");
    assert_eq!(firsts(&rows, 2, &[0, 3, 4]), ["1,1,-1", "1,2,2"]);
}

#[test]
fn a_nested_loop_join_on_a_condition_follows_postgres() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    j_tables(&db, &mut s);
    run(&db, &mut s, "set chronos.follow_pg_plan = on");
    // `<=` on integers no hash or merge join takes: Nested Loop, J2 outer (40737.60)
    let rows = run(&db, &mut s, "select * from j1_tbl join j2_tbl on (j1_tbl.i <= j2_tbl.k)");
    assert_eq!(firsts(&rows, 3, &[0, 3, 4]), ["1,2,2", "2,2,2", "0,2,2"]);
    // a condition a hash join takes isn't modelled: the executor's own order
    let own = run(&db, &mut s, "select * from j1_tbl join j2_tbl on (j1_tbl.i = j2_tbl.i)");
    run(&db, &mut s, "set chronos.follow_pg_plan = off");
    assert_eq!(run(&db, &mut s, "select * from j1_tbl join j2_tbl on (j1_tbl.i = j2_tbl.i)"), own);
}

#[test]
fn what_the_model_does_not_follow_keeps_the_executors_order() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    j_tables(&db, &mut s);
    run(&db, &mut s, "set chronos.follow_pg_plan = on");
    // (J1 first, as the executor's own order has it: Postgres's would put J2 first)
    let own = ["1,1,-1", "1,2,2"];
    // a LIMIT (a fast start counts), or more tables than join_collapse_limit
    let rows = run(&db, &mut s, "select * from j1_tbl cross join j2_tbl limit 5");
    assert_eq!(firsts(&rows, 2, &[0, 3, 4]), own);
    run(&db, &mut s, "set join_collapse_limit = 2");
    let rows = run(&db, &mut s, "select * from j1_tbl cross join j2_tbl a cross join j2_tbl b");
    assert_eq!(firsts(&rows, 2, &[0, 3, 5]), ["1,1,1", "1,1,2"]);
    assert_eq!(firsts(&rows[9..], 1, &[0, 3, 5]), ["1,2,1"]);
    // (two tables are one problem whatever the limit: J2 outer, as Postgres has it)
    run(&db, &mut s, "set join_collapse_limit = 1");
    let rows = run(&db, &mut s, "select * from j1_tbl cross join j2_tbl");
    assert_eq!(firsts(&rows, 2, &[0, 3, 4]), ["1,1,-1", "2,1,-1"]);
    run(&db, &mut s, "reset join_collapse_limit");
    // a cost setting it follows: with no Material node, rescanning J2 (32.60 a time) beats rescanning J1 (22.00)
    // fewer times, so J1 is outer (EXPLAIN: 66262.00)
    run(&db, &mut s, "set enable_material = off");
    let rows = run(&db, &mut s, "select * from j1_tbl cross join j2_tbl");
    assert_eq!(firsts(&rows, 2, &[0, 3, 4]), own);
    run(&db, &mut s, "reset enable_material");
    // an index on a table (an index-only scan is a path Postgres would weigh)
    run(&db, &mut s, "create index on j2_tbl (i)");
    let rows = run(&db, &mut s, "select * from j1_tbl cross join j2_tbl");
    assert_eq!(firsts(&rows, 2, &[0, 3, 4]), own);
}

#[test]
fn geometry_joins_follow_postgres() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "set chronos.follow_pg_plan = on");
    run(&db, &mut s, "create table point_tbl (f1 point)");
    run(
        &db,
        &mut s,
        "insert into point_tbl values ('(0.0,0.0)'), ('(-10.0,0.0)'), ('(-3.0,4.0)'), ('(5.1, 34.5)'),
         ('(-5.0,-12.0)'), ('(1e-300,-1e-300)'), ('(1e+300,Inf)'), ('(Inf,1e+300)'), (' ( Nan , NaN ) '), ('10.0,10.0')",
    );
    run(&db, &mut s, "create table lseg_tbl (s lseg); insert into lseg_tbl values ('[(1,2),(3,4)]'), ('(0,0),(6,6)')");
    run(&db, &mut s, "create table line_tbl (s line); insert into line_tbl values ('{0,-1,5}'), ('{1,0,5}')");
    // LSEG_TBL 1,360 rows, LINE_TBL 1,570: the line outer (32080.70)
    let rows = run(&db, &mut s, "select l.s, l1.s from lseg_tbl l, line_tbl l1");
    assert_eq!(firsts(&rows, 2, &[0, 1]), ["[(1,2),(3,4)],{0,-1,5}", "[(0,0),(6,6)],{0,-1,5}"]);
    // p1 restricted to 9 rows by BETWEEN's range (DEFAULT_RANGE_INEQ_SEL) and materialized inside p2 (316.02)
    let rows =
        run(&db, &mut s, "select p1.f1, p2.f1 from point_tbl p1, point_tbl p2 where p1.f1[0] between 1 and 1000");
    assert_eq!(firsts(&rows, 3, &[0, 1]), ["(5.1,34.5),(0,0)", "(10,10),(0,0)", "(5.1,34.5),(-10,0)"]);
    // p2 restricted by ~= (eqsel: 1 / 1850 distinct values): p1 outer (311.40)
    // (two points are ~= (0,0): it and (1e-300,-1e-300))
    let rows = run(&db, &mut s, "select p1.f1, p2.f1 from point_tbl p1, point_tbl p2 where p2.f1 ~= '(0,0)'::point");
    assert_eq!(firsts(&rows, 3, &[0, 1]), ["(0,0),(0,0)", "(0,0),(1e-300,-1e-300)", "(-10,0),(0,0)"]);
    // a join filter (<@, contjoinsel): point outer (37795.50)
    let rows = run(&db, &mut s, "select p.f1, l.s from point_tbl p, lseg_tbl l where p.f1 <@ l.s");
    assert_eq!(firsts(&rows, 1, &[0, 1]), ["(0,0),[(0,0),(6,6)]"]);
}

#[test]
fn vacuum_and_analyze_record_statistics() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    j_tables(&db, &mut s);
    let stats = |s: &mut Session, t: &str| {
        run(&db, s, &format!("select relpages, reltuples, relallvisible from pg_class where relname = '{t}'"))[0]
            .join(",")
    };
    assert_eq!(stats(&mut s, "j1_tbl"), "0,-1,0");
    run(&db, &mut s, "vacuum j2_tbl");
    assert_eq!(stats(&mut s, "j2_tbl"), "1,9,1");
    run(&db, &mut s, "analyze j1_tbl");
    assert_eq!(stats(&mut s, "j1_tbl"), "1,11,0");
    // with both measured, J1 (11 rows) is outer of J2 (9), materialized (EXPLAIN: 3.46)
    run(&db, &mut s, "set chronos.follow_pg_plan = on");
    let rows = run(&db, &mut s, "select * from j1_tbl cross join j2_tbl");
    assert_eq!(firsts(&rows, 2, &[0, 3, 4]), ["1,1,-1", "1,2,2"]);
    // TRUNCATE: a new heap's
    run(&db, &mut s, "truncate j2_tbl");
    assert_eq!(stats(&mut s, "j2_tbl"), "0,-1,0");
    // UPDATE pg_class SET reltuples, as tests and tuning scripts steer plans with
    assert_eq!(
        run(&db, &mut s, "update pg_class set reltuples = 1000 where relname = 'j1_tbl'"),
        Vec::<Vec<String>>::new()
    );
    assert_eq!(stats(&mut s, "j1_tbl"), "1,1000,0");
    // a partitioned table: its partitions' rows, no pages of its own
    run(
        &db,
        &mut s,
        "create table pp (i int) partition by list (i); create table pp1 partition of pp for values in (1)",
    );
    run(&db, &mut s, "insert into pp values (1), (1); analyze pp");
    assert_eq!(stats(&mut s, "pp"), "-1,2,0");
    assert_eq!(stats(&mut s, "pp1"), "1,2,0");
    // dropped with its table; a renamed table keeps its own
    run(&db, &mut s, "alter table j1_tbl rename to j1_renamed");
    assert_eq!(stats(&mut s, "j1_renamed"), "1,1000,0");
    run(&db, &mut s, "drop table j1_renamed; create table j1_renamed (a int)");
    assert_eq!(stats(&mut s, "j1_renamed"), "0,-1,0");
}

#[test]
fn heap_pages_are_postgres_pages() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let q = [
        "create table h1 (a int)",
        "insert into h1 select generate_series(1, 1000)",
        "create table h2 (a int, b text, c numeric)",
        "insert into h2 select i, repeat('x', i % 200), i * 1.5 from generate_series(1, 3000) i",
        "create table h3 (a int8, b int2, c text, d bool)",
        "insert into h3 select i, (i % 100)::int2, case when i % 3 = 0 then null else 'row ' || i end, i % 2 = 0 \
         from generate_series(1, 2000) i",
        "create table h4 (p path, q polygon, n name)",
        "insert into h4 select ('[(0,0),(' || i || ',1),(2,3)]')::path, ('((0,0),(1,' || i || '),(3,3),(4,4))')::polygon, \
         'n' || i from generate_series(1, 500) i",
        "create table h5 (a char(10), b varchar(20), c numeric(10,2)) with (fillfactor = 70)",
        "insert into h5 select 'c' || i, 'v' || i, i / 7.0 from generate_series(1, 2500) i",
        "create table h6 (a numeric)",
        "insert into h6 values (0), (0.00012), (123456789.123456789), (-5), ('NaN'), (1e30), (1e-30)",
        "insert into h6 select i * 1000000.0001 from generate_series(1, 3000) i",
        "create table h7 (a text)",
        "insert into h7 select repeat('y', 100 + i % 50) from generate_series(1, 1500) i",
        "vacuum h1, h2, h3, h4, h5, h6, h7",
    ];
    for x in q {
        run(&db, &mut s, x);
    }
    let rows = run(
        &db,
        &mut s,
        "select relname, relpages, reltuples from pg_class where relname in ('h1','h2','h3','h4','h5','h6','h7') \
         order by relname",
    );
    let got: Vec<String> = rows.iter().map(|r| r.join(",")).collect();
    assert_eq!(got, ["h1,5,1000", "h2,53,3000", "h3,13,2000", "h4,17,500", "h5,23,2500", "h6,17,3007", "h7,30,1500"]);
}

#[test]
fn the_setting_is_checked_and_listed() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(run(&db, &mut s, "show chronos.follow_pg_plan"), [["off"]]);
    run(&db, &mut s, "set Chronos.Follow_PG_Plan = true");
    assert_eq!(run(&db, &mut s, "show chronos.follow_pg_plan"), [["on"]]);
    let (code, message) = fails(&db, &mut s, "set chronos.follow_pg_plan = maybe");
    assert_eq!((code, message.as_str()), ("22023", "parameter \"chronos.follow_pg_plan\" requires a Boolean value"));
    let all = run(&db, &mut s, "show all");
    let listed =
        all.iter().any(|r| r[0] == "chronos.follow_pg_plan" && r[1] == "on" && r[2].contains("work done can differ"));
    assert!(listed, "{all:?}");
    run(&db, &mut s, "reset chronos.follow_pg_plan");
    assert_eq!(run(&db, &mut s, "show chronos.follow_pg_plan"), [["off"]]);
}

fn stats(db: &Db, s: &mut Session, t: &str) -> String {
    run(db, s, &format!("select relpages, reltuples, relallvisible from pg_class where relname = '{t}'"))[0].join(",")
}

/// CREATE TABLE AS fills its table in bulk, which extends Postgres's heap by more pages at a time as it grows: 20,000
/// ints take 89 pages of rows in 128 (Postgres 16: pg_relation_size), where the model counts 89. The table's joins
/// keep the executor's order, which here is Postgres's (ct outer), where the model's 89 pages would put it inner.
#[test]
fn a_table_create_table_as_filled_keeps_the_executors_order() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table ct as select g as a from generate_series(1, 20000) g");
    run(&db, &mut s, "create table it2 (a int); insert into it2 select g from generate_series(1, 22600) g");
    assert_eq!(stats(&db, &mut s, "ct"), "0,-1,0");
    run(&db, &mut s, "set chronos.follow_pg_plan = on");
    let rows = run(&db, &mut s, "select * from ct, it2 where ct.a < 3 and it2.a < 3");
    assert_eq!(rows.iter().map(|r| r.join(",")).collect::<Vec<_>>(), ["1,1", "1,2", "2,1", "2,2"]);
}

/// VACUUM FULL rewrites the heap: its pages and tuples, and none all-visible (Postgres 16: 14, 3000, 0).
#[test]
fn vacuum_full_records_the_rewritten_heap() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table vf (a int); insert into vf select g from generate_series(1, 3000) g; vacuum vf");
    assert_eq!(stats(&db, &mut s, "vf"), "14,3000,14");
    run(&db, &mut s, "vacuum full vf");
    assert_eq!(stats(&db, &mut s, "vf"), "14,3000,0");
}

/// A column's type changed without a rewrite (a varchar made longer, or text; a numeric given more precision at
/// the same scale) keeps the statistics; one that needs its values converted (to a shorter varchar, another scale)
/// is a new heap's, as Postgres 16 shows them.
#[test]
fn only_a_rewrite_resets_the_statistics() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table vc (a varchar(10)); insert into vc select g::text from generate_series(1, 3000) g");
    run(&db, &mut s, "vacuum vc");
    assert_eq!(stats(&db, &mut s, "vc"), "14,3000,14");
    run(&db, &mut s, "alter table vc alter column a type varchar(20)");
    assert_eq!(stats(&db, &mut s, "vc"), "14,3000,14");
    run(&db, &mut s, "alter table vc alter column a type text");
    assert_eq!(stats(&db, &mut s, "vc"), "14,3000,14");
    run(&db, &mut s, "alter table vc alter column a type varchar(5)");
    assert_eq!(stats(&db, &mut s, "vc"), "0,-1,0");
    run(&db, &mut s, "create table nm (a numeric(10,2)); insert into nm select g from generate_series(1, 3000) g");
    run(&db, &mut s, "vacuum nm");
    let vacuumed = stats(&db, &mut s, "nm");
    run(&db, &mut s, "alter table nm alter column a type numeric(12,2)");
    assert_eq!(stats(&db, &mut s, "nm"), vacuumed);
    run(&db, &mut s, "alter table nm alter column a type numeric(12,3)");
    assert_eq!(stats(&db, &mut s, "nm"), "0,-1,0");
}

/// A rewrite of a table with an index builds the index again on the new heap, which is filled in bulk: 3,000
/// bigints take 14 pages of rows in 16 (Postgres 16: 16, 3000, 0).
#[test]
fn a_rewritten_table_with_an_index_counts_its_new_heap() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table ri (a int); insert into ri select g from generate_series(1, 3000) g");
    run(&db, &mut s, "create index on ri (a); vacuum ri");
    assert_eq!(stats(&db, &mut s, "ri"), "14,3000,14");
    run(&db, &mut s, "alter table ri alter column a type bigint");
    assert_eq!(stats(&db, &mut s, "ri"), "16,3000,0");
}

/// A primary key added to a table with rows builds its index, which counts the table (Postgres 16: 14, 3000, 0);
/// an index on a partitioned table is built on each partition, which each count theirs (9, 1999 and 5, 1001), the
/// partitioned table having none.
#[test]
fn constraints_and_partitioned_indexes_record_statistics() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table pk (a int); insert into pk select g from generate_series(1, 3000) g");
    run(&db, &mut s, "alter table pk add primary key (a)");
    assert_eq!(stats(&db, &mut s, "pk"), "14,3000,0");
    run(
        &db,
        &mut s,
        "create table pt (a int) partition by range (a);
         create table pt1 partition of pt for values from (1) to (2000);
         create table pt2 partition of pt for values from (2000) to (4000);
         insert into pt select g from generate_series(1, 3000) g",
    );
    run(&db, &mut s, "create index on pt (a)");
    assert_eq!(stats(&db, &mut s, "pt"), "0,-1,0");
    assert_eq!(stats(&db, &mut s, "pt1"), "9,1999,0");
    assert_eq!(stats(&db, &mut s, "pt2"), "5,1001,0");
}
