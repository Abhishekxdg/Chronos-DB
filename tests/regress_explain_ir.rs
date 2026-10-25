//! EXPLAIN's plan tree and printer (src/sql/explain.rs): the plan PostgreSQL 16 would choose (chronos.explain =
//! postgres) in every format, with the model's costs, VERBOSE's target lists, ANALYZE's actuals where Chronos's
//! executor ran the node, and the option errors Postgres gives. The expected text is PostgreSQL 16.15's for the same
//! statements on a fresh database (tables never vacuumed: the 10-page rule).

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn lines(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    run(db, s, q).into_iter().map(|r| r[0].clone()).collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code, e.message),
    }
}

fn pg() -> (Db, Session) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "set chronos.explain = postgres");
    run(&db, &mut s, "create table xt (a int, b text)");
    (db, s)
}

#[test]
fn costs_of_a_table_never_vacuumed() {
    let (db, mut s) = pg();
    assert_eq!(
        lines(&db, &mut s, "explain select * from xt"),
        ["Seq Scan on xt  (cost=0.00..22.70 rows=1270 width=36)"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select * from xt where a = 1"),
        ["Seq Scan on xt  (cost=0.00..25.88 rows=6 width=36)", "  Filter: (a = 1)"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select a from xt where a > 1 and a < 5"),
        ["Seq Scan on xt  (cost=0.00..29.05 rows=6 width=4)", "  Filter: ((a > 1) AND (a < 5))"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select * from xt where b is null"),
        ["Seq Scan on xt  (cost=0.00..22.70 rows=6 width=36)", "  Filter: (b IS NULL)"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select count(*) from xt"),
        ["Aggregate  (cost=25.88..25.89 rows=1 width=8)", "  ->  Seq Scan on xt  (cost=0.00..22.70 rows=1270 width=0)"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select * from xt order by a"),
        [
            "Sort  (cost=88.17..91.35 rows=1270 width=36)",
            "  Sort Key: a",
            "  ->  Seq Scan on xt  (cost=0.00..22.70 rows=1270 width=36)"
        ]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select * from xt order by a limit 3"),
        [
            "Limit  (cost=39.11..39.12 rows=3 width=36)",
            "  ->  Sort  (cost=39.11..42.29 rows=1270 width=36)",
            "        Sort Key: a",
            "        ->  Seq Scan on xt  (cost=0.00..22.70 rows=1270 width=36)"
        ]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select * from xt limit 10 offset 2"),
        ["Limit  (cost=0.04..0.21 rows=10 width=36)", "  ->  Seq Scan on xt  (cost=0.00..22.70 rows=1270 width=36)"]
    );
    assert_eq!(lines(&db, &mut s, "explain select 1"), ["Result  (cost=0.00..0.01 rows=1 width=4)"]);
    // (the widths of types with a modifier: get_typavgwidth)
    run(&db, &mut s, "create table xw (a varchar(10), b numeric(10,2), c char(4), d bigint, e bool)");
    assert_eq!(
        lines(&db, &mut s, "explain select * from xw"),
        ["Seq Scan on xw  (cost=0.00..17.30 rows=730 width=83)"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select * from xw where e"),
        ["Seq Scan on xw  (cost=0.00..17.30 rows=365 width=83)", "  Filter: e"]
    );
}

#[test]
fn costs_follow_the_rows_an_emulated_heap_holds() {
    let (db, mut s) = pg();
    run(&db, &mut s, "insert into xt select g, 'row ' || g from generate_series(1, 5000) g");
    // (5000 rows of 37 bytes: 28 pages; 127 rows a page by the columns' widths)
    assert_eq!(
        lines(&db, &mut s, "explain select * from xt"),
        ["Seq Scan on xt  (cost=0.00..63.56 rows=3556 width=36)"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select * from xt where a = 1"),
        ["Seq Scan on xt  (cost=0.00..72.45 rows=18 width=36)", "  Filter: (a = 1)"]
    );
}

#[test]
fn partitions_cost_as_an_append() {
    let (db, mut s) = pg();
    run(&db, &mut s, "create table xp (a int, b int) partition by range (a)");
    run(&db, &mut s, "create table xp1 partition of xp for values from (0) to (10)");
    run(&db, &mut s, "create table xp2 partition of xp for values from (10) to (20)");
    assert_eq!(
        lines(&db, &mut s, "explain select * from xp"),
        [
            "Append  (cost=0.00..87.80 rows=4520 width=8)",
            "  ->  Seq Scan on xp1 xp_1  (cost=0.00..32.60 rows=2260 width=8)",
            "  ->  Seq Scan on xp2 xp_2  (cost=0.00..32.60 rows=2260 width=8)"
        ]
    );
    assert_eq!(
        lines(&db, &mut s, "explain select * from xp where a = 5"),
        ["Seq Scan on xp1 xp  (cost=0.00..38.25 rows=11 width=8)", "  Filter: (a = 5)"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain (format json, costs off) select * from xp")[0],
        r#"[
  {
    "Plan": {
      "Node Type": "Append",
      "Parallel Aware": false,
      "Async Capable": false,
      "Subplans Removed": 0,
      "Plans": [
        {
          "Node Type": "Seq Scan",
          "Parent Relationship": "Member",
          "Parallel Aware": false,
          "Async Capable": false,
          "Relation Name": "xp1",
          "Alias": "xp_1"
        },
        {
          "Node Type": "Seq Scan",
          "Parent Relationship": "Member",
          "Parallel Aware": false,
          "Async Capable": false,
          "Relation Name": "xp2",
          "Alias": "xp_2"
        }
      ]
    }
  }
]"#
    );
}

#[test]
fn formats_json_yaml_xml() {
    let (db, mut s) = pg();
    run(&db, &mut s, "insert into xt select g, 'row ' || g from generate_series(1, 5000) g");
    assert_eq!(
        lines(&db, &mut s, "explain (format json) select * from xt"),
        [r#"[
  {
    "Plan": {
      "Node Type": "Seq Scan",
      "Parallel Aware": false,
      "Async Capable": false,
      "Relation Name": "xt",
      "Alias": "xt",
      "Startup Cost": 0.00,
      "Total Cost": 63.56,
      "Plan Rows": 3556,
      "Plan Width": 36
    }
  }
]"#]
    );
    // (YAML's first line ends in a space: the group's label, before its members' lines)
    assert_eq!(
        lines(&db, &mut s, "explain (format yaml) select * from xt"),
        [concat!(
            "- Plan: \n",
            "    Node Type: \"Seq Scan\"\n",
            "    Parallel Aware: false\n",
            "    Async Capable: false\n",
            "    Relation Name: \"xt\"\n",
            "    Alias: \"xt\"\n",
            "    Startup Cost: 0.00\n",
            "    Total Cost: 63.56\n",
            "    Plan Rows: 3556\n",
            "    Plan Width: 36"
        )]
    );
    assert_eq!(
        lines(&db, &mut s, "explain (format xml, costs off) select * from xt where a = 3"),
        [r#"<explain xmlns="http://www.postgresql.org/2009/explain">
  <Query>
    <Plan>
      <Node-Type>Seq Scan</Node-Type>
      <Parallel-Aware>false</Parallel-Aware>
      <Async-Capable>false</Async-Capable>
      <Relation-Name>xt</Relation-Name>
      <Alias>xt</Alias>
      <Filter>(a = 3)</Filter>
    </Plan>
  </Query>
</explain>"#]
    );
    // the result is of type json and xml, as Postgres types it
    let out = s.run(&db, "explain (format json) select * from xt", &[]).unwrap();
    assert_eq!(out.last().unwrap().columns.as_ref().unwrap()[0].1.name(), "json");
    // BUFFERS: Chronos reads no shared buffers, so its counts are 0 (shown in the formats that always show them)
    let json = lines(&db, &mut s, "explain (buffers, format json, costs off) select * from xt").remove(0);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[0]["Plan"]["Shared Hit Blocks"], 0);
    assert_eq!(v[0]["Planning"]["Temp Written Blocks"], 0);
    assert_eq!(lines(&db, &mut s, "explain (buffers, costs off) select * from xt"), ["Seq Scan on xt"]);
}

#[test]
fn verbose_shows_target_lists() {
    let (db, mut s) = pg();
    assert_eq!(
        lines(&db, &mut s, "explain (verbose) select * from xt"),
        ["Seq Scan on public.xt  (cost=0.00..22.70 rows=1270 width=36)", "  Output: a, b"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain (verbose, costs off) select b, a from xt order by a"),
        ["Sort", "  Output: b, a", "  Sort Key: xt.a", "  ->  Seq Scan on public.xt", "        Output: b, a"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain (verbose, costs off) select count(*) from xt"),
        ["Aggregate", "  Output: count(*)", "  ->  Seq Scan on public.xt", "        Output: a, b"]
    );
    // (a row value's casts the planner folds: not written here, so no Output, and Chronos's steps)
    assert!(
        !lines(&db, &mut s, "explain (verbose, costs off) select row(a, b) from xt")
            .iter()
            .any(|l| l.contains("Output"))
    );
    // with compute_query_id on, the query identifier pg_stat_statements keeps the statement by
    run(&db, &mut s, "set compute_query_id = on");
    let l = lines(&db, &mut s, "explain (verbose, costs off) select * from xt");
    assert_eq!(l[..2], ["Seq Scan on public.xt", "  Output: a, b"]);
    assert!(
        l[2].starts_with("Query Identifier: ") && l[2]["Query Identifier: ".len()..].parse::<i64>().is_ok(),
        "{l:?}"
    );
    run(&db, &mut s, "set compute_query_id = regress");
    assert_eq!(lines(&db, &mut s, "explain (verbose, costs off) select * from xt").len(), 2);
}

#[test]
fn analyze_shows_what_chronos_ran() {
    let (db, mut s) = pg();
    run(&db, &mut s, "insert into xt select g, 'row ' || g from generate_series(1, 5000) g");
    assert_eq!(
        lines(&db, &mut s, "explain (analyze, costs off, timing off, summary off) select * from xt where a < 3"),
        ["Seq Scan on xt (actual rows=2 loops=1)", "  Filter: (a < 3)", "  Rows Removed by Filter: 4998"]
    );
    let l = lines(&db, &mut s, "explain (analyze) select * from xt");
    assert!(l[0].starts_with("Seq Scan on xt  (cost=0.00..63.56 rows=3556 width=36) (actual time="), "{l:?}");
    assert!(l[0].ends_with(" rows=5000 loops=1)"), "{l:?}");
    assert!(l[1].starts_with("Planning Time: ") && l[2].starts_with("Execution Time: "), "{l:?}");
    assert_eq!(l.len(), 3);
}

#[test]
fn settings_lists_what_changes_planning() {
    let (db, mut s) = pg();
    assert_eq!(lines(&db, &mut s, "explain (settings, costs off) select * from xt"), ["Seq Scan on xt"]);
    run(&db, &mut s, "set work_mem = '64kB'");
    assert_eq!(
        lines(&db, &mut s, "explain (settings, costs off) select * from xt"),
        ["Seq Scan on xt", "Settings: work_mem = '64kB'"]
    );
    let json = lines(&db, &mut s, "explain (settings, format json, costs off) select * from xt").remove(0);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[0]["Settings"]["work_mem"], "64kB");
}

#[test]
fn option_errors_are_postgres_s() {
    let (db, mut s) = pg();
    assert_eq!(fails(&db, &mut s, "explain (wal) select 1"), ("22023", "EXPLAIN option WAL requires ANALYZE".into()));
    assert_eq!(
        fails(&db, &mut s, "explain (timing) select 1"),
        ("22023", "EXPLAIN option TIMING requires ANALYZE".into())
    );
    assert_eq!(fails(&db, &mut s, "explain (foo) select 1"), ("42601", "unrecognized EXPLAIN option \"foo\"".into()));
    assert_eq!(
        fails(&db, &mut s, "explain (format foo) select 1"),
        ("22023", "unrecognized value for EXPLAIN option \"format\": \"foo\"".into())
    );
    assert_eq!(
        fails(&db, &mut s, "explain (analyze 2) select 1"),
        ("42601", "analyze requires a Boolean value".into())
    );
    assert_eq!(
        fails(&db, &mut s, "explain (analyze, generic_plan) select 1"),
        ("22023", "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together".into())
    );
    // (a value written for an option: ANALYZE false is no ANALYZE)
    assert_eq!(lines(&db, &mut s, "explain (analyze false, costs off) select * from xt"), ["Seq Scan on xt"]);
}

#[test]
fn chronos_steps_in_every_format() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table xt (a int, b text); insert into xt values (1, 'x')");
    let text = lines(&db, &mut s, "explain (costs off) select * from xt");
    assert_eq!(text, ["Seq Scan on xt"]);
    let text = lines(&db, &mut s, "explain analyze select * from xt");
    assert_eq!(text[0], "Seq Scan on xt  (actual rows=1)");
    assert!(text[1].starts_with("Execution Time: "), "{text:?}");
    let json = lines(&db, &mut s, "explain (format json) select * from xt").remove(0);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[0]["Plan"]["Node Type"], "Seq Scan");
    assert_eq!(v[0]["Plan"]["Relation Name"], "xt");
}

#[test]
fn a_data_error_doesnt_stop_explain() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table xs (x text); insert into xs values ('abc')");
    // (Postgres's EXPLAIN never runs the query, so never meets the cast's error)
    let l = lines(&db, &mut s, "explain (costs off) select x::int from xs");
    assert!(l.iter().any(|x| x.contains("xs")), "{l:?}");
    assert_eq!(fails(&db, &mut s, "select x::int from xs").0, "22P02");
}

/// `explain (costs off) q`'s lines.
fn plan(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    lines(db, s, &format!("explain (costs off) {q}"))
}

#[test]
fn pruning_steps_as_partprune_makes_them() {
    let (db, mut s) = pg();
    for q in [
        "create table rlp (a int, b varchar) partition by range (a)",
        "create table rlp1 partition of rlp for values from (minvalue) to (1)",
        "create table rlp2 partition of rlp for values from (1) to (10)",
        "create table rlp5 partition of rlp for values from (31) to (maxvalue) partition by range (a)",
        "create table rlp5_default partition of rlp5 default",
        "create table rlp5_1 partition of rlp5 for values from (31) to (40)",
    ] {
        run(&db, &mut s, q);
    }
    // (a sub-partitioned table's own bound prunes its default partition: the steps intersect)
    assert_eq!(
        plan(&db, &mut s, "select * from rlp where a <= 31"),
        [
            "Append",
            "  ->  Seq Scan on rlp1 rlp_1",
            "        Filter: (a <= 31)",
            "  ->  Seq Scan on rlp2 rlp_2",
            "        Filter: (a <= 31)",
            "  ->  Seq Scan on rlp5_1 rlp_3",
            "        Filter: (a <= 31)"
        ]
    );
    // (an integer key compared with a numeric: no operator of its family, so no pruning)
    assert_eq!(plan(&db, &mut s, "select * from rlp where a = 1::numeric").len(), 9);
    // (no contradiction found among an OR's arms: each equality is a step)
    assert_eq!(
        plan(&db, &mut s, "select * from rlp where (a = 1 and a = 3) or (a > 1 and a = 15)"),
        ["Seq Scan on rlp2 rlp", "  Filter: (((a = 1) AND (a = 3)) OR ((a > 1) AND (a = 15)))"]
    );
    for q in [
        "create table boolpart (a bool) partition by list (a)",
        "create table boolpart_default partition of boolpart default",
        "create table boolpart_t partition of boolpart for values in ('true')",
        "create table boolpart_f partition of boolpart for values in ('false')",
    ] {
        run(&db, &mut s, q);
    }
    // (IS UNKNOWN prunes nothing, and is written back as it was)
    assert_eq!(
        plan(&db, &mut s, "select * from boolpart where a is unknown"),
        [
            "Append",
            "  ->  Seq Scan on boolpart_f boolpart_1",
            "        Filter: (a IS UNKNOWN)",
            "  ->  Seq Scan on boolpart_t boolpart_2",
            "        Filter: (a IS UNKNOWN)",
            "  ->  Seq Scan on boolpart_default boolpart_3",
            "        Filter: (a IS UNKNOWN)"
        ]
    );
    assert_eq!(
        plan(&db, &mut s, "select * from boolpart where a is not true"),
        [
            "Append",
            "  ->  Seq Scan on boolpart_f boolpart_1",
            "        Filter: (a IS NOT TRUE)",
            "  ->  Seq Scan on boolpart_default boolpart_2",
            "        Filter: (a IS NOT TRUE)"
        ]
    );
    assert_eq!(
        plan(&db, &mut s, "select * from boolpart where not a = false"),
        ["Seq Scan on boolpart_t boolpart", "  Filter: a"]
    );
}

#[test]
fn arrays_collations_and_cross_type_keys() {
    let (db, mut s) = pg();
    for q in [
        "create table coercepart (a varchar) partition by list (a)",
        "create table coercepart_ab partition of coercepart for values in ('ab')",
        "create table coercepart_bc partition of coercepart for values in ('bc')",
        "create table coercepart_cd partition of coercepart for values in ('cd')",
        "create table lparted_by_int2 (a smallint) partition by list (a)",
        "create table lparted_by_int2_1 partition of lparted_by_int2 for values in (1)",
        "create table lparted_by_int2_16384 partition of lparted_by_int2 for values in (16384)",
        "create table coll_pruning (a text collate \"C\") partition by list (a)",
        "create table coll_pruning_a partition of coll_pruning for values in ('a')",
        "create table coll_pruning_b partition of coll_pruning for values in ('b')",
        "create table coll_pruning_def partition of coll_pruning default",
    ] {
        run(&db, &mut s, q);
    }
    assert_eq!(
        plan(&db, &mut s, "select * from coercepart where a = any ('{ab,null}')"),
        ["Seq Scan on coercepart_ab coercepart", "  Filter: ((a)::text = ANY ('{ab,NULL}'::text[]))"]
    );
    assert_eq!(
        plan(&db, &mut s, "select * from coercepart where a = all ('{ab,bc}')"),
        ["Result", "  One-Time Filter: false"]
    );
    let f = "        Filter: ((a)::text = ANY ((ARRAY['ab'::character varying, (to_char(125, '999'::text))::character varying])::text[]))";
    assert_eq!(
        plan(&db, &mut s, "select * from coercepart where a in ('ab', to_char(125, '999'))"),
        [
            "Append",
            "  ->  Seq Scan on coercepart_ab coercepart_1",
            f,
            "  ->  Seq Scan on coercepart_bc coercepart_2",
            f,
            "  ->  Seq Scan on coercepart_cd coercepart_3",
            f
        ]
    );
    // (an int2 key compared with an int8 out of its range: integer_ops compare across widths)
    assert_eq!(
        plan(&db, &mut s, "select * from lparted_by_int2 where a = 100000000000000"),
        ["Result", "  One-Time Filter: false"]
    );
    // (COLLATE: folded into a constant, nothing on a column of that collation, pruning only in the key's)
    assert_eq!(
        plan(&db, &mut s, "select * from coll_pruning where a collate \"C\" = 'a' collate \"C\""),
        ["Seq Scan on coll_pruning_a coll_pruning", "  Filter: (a = 'a'::text COLLATE \"C\")"]
    );
    let f = "        Filter: ((a)::text = 'a'::text COLLATE \"POSIX\")";
    assert_eq!(
        plan(&db, &mut s, "select * from coll_pruning where a collate \"POSIX\" = 'a' collate \"POSIX\""),
        [
            "Append",
            "  ->  Seq Scan on coll_pruning_a coll_pruning_1",
            f,
            "  ->  Seq Scan on coll_pruning_b coll_pruning_2",
            f,
            "  ->  Seq Scan on coll_pruning_def coll_pruning_3",
            f
        ]
    );
}

#[test]
fn a_hash_operator_class_routes_and_prunes() {
    let (db, mut s) = pg();
    for q in [
        "create function hashint4_noop(int4, int8) returns int8 as $$ select $1 + $2; $$ language sql strict immutable",
        "create operator class test_int4_ops for type int4 using hash as operator 1 =, function 2 hashint4_noop(int4, int8)",
        "create table hp (a int) partition by hash (a test_int4_ops)",
        "create table hp0 partition of hp for values with (modulus 4, remainder 0)",
        "create table hp1 partition of hp for values with (modulus 4, remainder 1)",
        "create table hp2 partition of hp for values with (modulus 4, remainder 2)",
        "create table hp3 partition of hp for values with (modulus 4, remainder 3)",
        "insert into hp values (1), (2), (3), (4), (5)",
    ] {
        run(&db, &mut s, q);
    }
    let rows = run(&db, &mut s, "select tableoid::regclass, a from hp order by a");
    let want = [["hp1", "1"], ["hp2", "2"], ["hp3", "3"], ["hp0", "4"], ["hp1", "5"]];
    assert_eq!(rows, want.map(|r| r.map(String::from).to_vec()).to_vec());
    assert_eq!(plan(&db, &mut s, "select * from hp where a = 3"), ["Seq Scan on hp3 hp", "  Filter: (a = 3)"]);
    assert_eq!(run(&db, &mut s, "select pg_get_partkeydef('hp'::regclass)")[0][0], "HASH (a test_int4_ops)");
}

#[test]
fn a_hash_operator_class_of_a_built_in_function_routes_as_the_type_does() {
    // (hashint4extended is int4's own extended hash: the class routes every row where the type's own would)
    let (db, mut s) = pg();
    for q in [
        "create operator class int4_ext for type int4 using hash as operator 1 =, function 2 hashint4extended(int4, int8)",
        "create table hb (a int) partition by hash (a int4_ext)",
        "create table hb0 partition of hb for values with (modulus 2, remainder 0)",
        "create table hb1 partition of hb for values with (modulus 2, remainder 1)",
        "create table hn (a int) partition by hash (a)",
        "create table hn0 partition of hn for values with (modulus 2, remainder 0)",
        "create table hn1 partition of hn for values with (modulus 2, remainder 1)",
        "insert into hb values (1), (2), (3), (4), (5)",
        "insert into hn values (1), (2), (3), (4), (5)",
    ] {
        run(&db, &mut s, q);
    }
    let placed = |s: &mut Session, t: &str| {
        run(&db, s, &format!("select right(tableoid::regclass::text, 1), a from {t} order by a"))
    };
    assert_eq!(placed(&mut s, "hb"), placed(&mut s, "hn"));
}

#[test]
fn a_hash_operator_class_a_partition_key_uses_is_not_dropped_alone() {
    // (the table depends on its key's class: findDependentObjects)
    let (db, mut s) = pg();
    for q in [
        "create function hashint4_noop(int4, int8) returns int8 as $$ select $1 + $2; $$ language sql strict immutable",
        "create operator class test_int4_ops for type int4 using hash as operator 1 =, function 2 hashint4_noop(int4, int8)",
        "create table hp (a int) partition by hash (a test_int4_ops)",
        "create table hp0 partition of hp for values with (modulus 2, remainder 0)",
        "create table hp1 partition of hp for values with (modulus 2, remainder 1)",
        "insert into hp values (1), (2)",
    ] {
        run(&db, &mut s, q);
    }
    assert_eq!(fails(&db, &mut s, "drop operator class test_int4_ops using hash").0, "2BP01");
    assert_eq!(run(&db, &mut s, "select count(*) from hp")[0][0], "2");
    run(&db, &mut s, "drop operator class test_int4_ops using hash cascade");
    assert_eq!(fails(&db, &mut s, "select * from hp").0, "42P01");
}

#[test]
fn a_hash_operator_classs_function_is_found_as_written_and_on_the_search_path() {
    // (public.f is kept as public's functions are, bare; a plain name as the search_path finds it)
    let (db, mut s) = pg();
    for q in [
        "create function hashint4_noop(int4, int8) returns int8 as $$ select $1 + $2; $$ language sql strict immutable",
        "create schema hs",
        "create function hs.hashint4_shift(int4, int8) returns int8 as $$ select $1 + $2; $$ language sql strict immutable",
        "create operator class q_ops for type int4 using hash as operator 1 =, function 2 public.hashint4_noop(int4, int8)",
        "set search_path = hs, public",
        "create operator class p_ops for type int4 using hash as operator 1 =, function 2 hashint4_shift(int4, int8)",
        "reset search_path",
    ] {
        run(&db, &mut s, q);
    }
    for (t, ops) in [("hq", "q_ops"), ("hpath", "hs.p_ops")] {
        run(&db, &mut s, &format!("create table {t} (a int) partition by hash (a {ops})"));
        for r in 0..4 {
            run(
                &db,
                &mut s,
                &format!("create table {t}{r} partition of {t} for values with (modulus 4, remainder {r})"),
            );
        }
        run(&db, &mut s, &format!("insert into {t} values (1), (2), (3), (4), (5)"));
        let rows = run(&db, &mut s, &format!("select right(tableoid::regclass::text, 1), a from {t} order by a"));
        let want = [["1", "1"], ["2", "2"], ["3", "3"], ["0", "4"], ["1", "5"]];
        assert_eq!(rows, want.map(|r| r.map(String::from).to_vec()).to_vec(), "{t}");
    }
}

#[test]
fn a_hash_operator_classs_support_function_is_not_dropped_alone() {
    // (the class depends on its support functions; a table hashing with the class is dropped by its own DROP)
    let (db, mut s) = pg();
    for q in [
        "create function hashint4_noop(int4, int8) returns int8 as $$ select $1 + $2; $$ language sql strict immutable",
        "create operator class test_int4_ops for type int4 using hash as operator 1 =, function 2 hashint4_noop(int4, int8)",
    ] {
        run(&db, &mut s, q);
    }
    assert_eq!(fails(&db, &mut s, "drop function hashint4_noop(int4, int8)").0, "2BP01");
    run(&db, &mut s, "create table hp (a int) partition by hash (a test_int4_ops)");
    assert_eq!(fails(&db, &mut s, "drop function hashint4_noop(int4, int8) cascade").0, "0A000");
    run(&db, &mut s, "drop table hp");
    run(&db, &mut s, "drop function hashint4_noop(int4, int8) cascade");
    // (the class went with it)
    fails(&db, &mut s, "drop operator class test_int4_ops using hash");
}

#[test]
fn an_overload_of_a_support_function_drops_and_a_cascade_needs_the_tables_owner() {
    let (db, mut s) = pg();
    for q in [
        "create function hashint4_noop(int4, int8) returns int8 as $$ select $1 + $2; $$ language sql strict immutable",
        "create function hashint4_noop(text) returns int8 as $$ select 0::int8; $$ language sql strict immutable",
        "create operator class test_int4_ops for type int4 using hash as operator 1 =, function 2 hashint4_noop(int4, int8)",
        "create table hp (a int) partition by hash (a test_int4_ops)",
        "create role stranger",
    ] {
        run(&db, &mut s, q);
    }
    // (the class's function is the one of its argument types, not another of the name)
    run(&db, &mut s, "drop function hashint4_noop(text)");
    // (dropping the class with CASCADE drops hp: only by a role that may drop hp)
    run(&db, &mut s, "set role stranger");
    assert_eq!(fails(&db, &mut s, "drop operator class test_int4_ops using hash cascade").0, "42501");
    run(&db, &mut s, "reset role");
    run(&db, &mut s, "drop operator class test_int4_ops using hash cascade");
    assert_eq!(fails(&db, &mut s, "select * from hp").0, "42P01");
}

#[test]
fn a_built_in_support_function_stays_the_built_in() {
    // (a function made later of its name, in public, isn't taken for the one the class named)
    let (db, mut s) = pg();
    for q in [
        "create operator class int4_ext for type int4 using hash as operator 1 =, function 2 pg_catalog.hashint4extended(int4, int8)",
        "create table hb (a int) partition by hash (a int4_ext)",
        "create table hb0 partition of hb for values with (modulus 2, remainder 0)",
        "create table hb1 partition of hb for values with (modulus 2, remainder 1)",
        "create table hn (a int) partition by hash (a)",
        "create table hn0 partition of hn for values with (modulus 2, remainder 0)",
        "create table hn1 partition of hn for values with (modulus 2, remainder 1)",
        "create function public.hashint4extended(int4, int8) returns int8 as $$ select 0::int8; $$ language sql immutable",
        "insert into hb values (1), (2), (3), (4), (5)",
        "insert into hn values (1), (2), (3), (4), (5)",
    ] {
        run(&db, &mut s, q);
    }
    let placed = |s: &mut Session, t: &str| {
        run(&db, s, &format!("select right(tableoid::regclass::text, 1), a from {t} order by a"))
    };
    assert_eq!(placed(&mut s, "hb"), placed(&mut s, "hn"));
}

#[test]
fn a_quoted_hash_class_name_keeps_its_period() {
    // ("custom.hash_ops" is one name, not a schema and a class)
    let (db, mut s) = pg();
    for q in [
        "create function hashint4_noop(int4, int8) returns int8 as $$ select $1 + $2; $$ language sql strict immutable",
        "create operator class \"custom.hash_ops\" for type int4 using hash as operator 1 =, function 2 hashint4_noop(int4, int8)",
        "create table hq (a int) partition by hash (a \"custom.hash_ops\")",
        "create table hq0 partition of hq for values with (modulus 4, remainder 0)",
        "create table hq1 partition of hq for values with (modulus 4, remainder 1)",
        "create table hq2 partition of hq for values with (modulus 4, remainder 2)",
        "create table hq3 partition of hq for values with (modulus 4, remainder 3)",
        "insert into hq values (1), (2), (3), (4), (5)",
    ] {
        run(&db, &mut s, q);
    }
    let rows = run(&db, &mut s, "select tableoid::regclass, a from hq order by a");
    let want = [["hq1", "1"], ["hq2", "2"], ["hq3", "3"], ["hq0", "4"], ["hq1", "5"]];
    assert_eq!(rows, want.map(|r| r.map(String::from).to_vec()).to_vec());
}

#[test]
fn a_temporary_tables_hash_operator_class_routes_rows() {
    // (a temporary table is kept in the session's own database; the class is the world's)
    let (db, mut s) = pg();
    for q in [
        "create function hashint4_noop(int4, int8) returns int8 as $$ select $1 + $2; $$ language sql strict immutable",
        "create operator class test_int4_ops for type int4 using hash as operator 1 =, function 2 hashint4_noop(int4, int8)",
        "create temp table thp (a int) partition by hash (a test_int4_ops)",
        "create temp table thp0 partition of thp for values with (modulus 4, remainder 0)",
        "create temp table thp1 partition of thp for values with (modulus 4, remainder 1)",
        "create temp table thp2 partition of thp for values with (modulus 4, remainder 2)",
        "create temp table thp3 partition of thp for values with (modulus 4, remainder 3)",
        "insert into thp values (1), (2), (3), (4), (5)",
    ] {
        run(&db, &mut s, q);
    }
    let rows = run(&db, &mut s, "select tableoid::regclass, a from thp order by a");
    let want = [["thp1", "1"], ["thp2", "2"], ["thp3", "3"], ["thp0", "4"], ["thp1", "5"]];
    assert_eq!(rows, want.map(|r| r.map(String::from).to_vec()).to_vec());
    // (the temporary table depends on its key's class as a table of the world does)
    assert_eq!(fails(&db, &mut s, "drop operator class test_int4_ops using hash").0, "2BP01");
    run(&db, &mut s, "drop operator class test_int4_ops using hash cascade");
    assert_eq!(fails(&db, &mut s, "select * from thp").0, "42P01");
}

#[test]
fn constraint_exclusion_and_tableoid() {
    let (db, mut s) = pg();
    for q in [
        "create table inh (a int)",
        "create table inh_c1 (check (a < 10)) inherits (inh)",
        "create table inh_c2 (check (a >= 10)) inherits (inh)",
        "create table part (a int, b int) partition by list (a)",
        "create table part_p1 partition of part for values in (1)",
        "create table part_p2 partition of part for values in (2)",
    ] {
        run(&db, &mut s, q);
    }
    // (a child whose CHECK the WHERE refutes isn't read: relation_excluded_by_constraints)
    assert_eq!(
        plan(&db, &mut s, "select * from inh where a = 5"),
        [
            "Append",
            "  ->  Seq Scan on inh inh_1",
            "        Filter: (a = 5)",
            "  ->  Seq Scan on inh_c1 inh_2",
            "        Filter: (a = 5)"
        ]
    );
    assert_eq!(
        plan(&db, &mut s, "select tableoid::regclass, * from part where a = 2 order by 1, 2, 3"),
        [
            "Sort",
            "  Sort Key: ((part.tableoid)::regclass), part.b",
            "  ->  Seq Scan on part_p2 part",
            "        Filter: (a = 2)"
        ]
    );
}

#[test]
fn a_generic_plan_and_lists_of_arrays() {
    let (db, mut s) = pg();
    for q in [
        "create table gen_part (key1 integer not null, key2 integer not null) partition by list (key1)",
        "create table gen_part_1 partition of gen_part for values in (1) partition by range (key2)",
        "create table gen_part_1_1 partition of gen_part_1 for values from (1) to (2)",
        "create table gen_part_1_2 partition of gen_part_1 for values from (2) to (3)",
        "create table gen_part_2 partition of gen_part for values in (2)",
        "create table pp_arrpart (a int[]) partition by list (a)",
        "create table pp_arrpart1 partition of pp_arrpart for values in ('{1}')",
        "create table pp_arrpart2 partition of pp_arrpart for values in ('{2, 3}', '{4, 5}')",
    ] {
        run(&db, &mut s, q);
    }
    // (a parameter prunes nothing when the plan is made, and is its own constant of an equivalence class)
    let q = "select key1, key2 from gen_part where key1 = 1 and key2 = $1";
    assert_eq!(
        lines(&db, &mut s, &format!("explain (generic_plan, costs off) {q}")),
        [
            "Append",
            "  ->  Seq Scan on gen_part_1_1 gen_part_1",
            "        Filter: ((key1 = 1) AND (key2 = $1))",
            "  ->  Seq Scan on gen_part_1_2 gen_part_2",
            "        Filter: ((key1 = 1) AND (key2 = $1))"
        ]
    );
    assert_eq!(
        lines(&db, &mut s, &format!("explain (generic_plan) {q}")),
        [
            "Append  (cost=0.00..87.81 rows=2 width=8)",
            "  ->  Seq Scan on gen_part_1_1 gen_part_1  (cost=0.00..43.90 rows=1 width=8)",
            "        Filter: ((key1 = 1) AND (key2 = $1))",
            "  ->  Seq Scan on gen_part_1_2 gen_part_2  (cost=0.00..43.90 rows=1 width=8)",
            "        Filter: ((key1 = 1) AND (key2 = $1))"
        ]
    );
    // (an IN list of arrays: an OR of equalities, there being no array of arrays to make)
    let f = "        Filter: ((a = '{4,5}'::integer[]) OR (a = '{1}'::integer[]))";
    assert_eq!(
        plan(&db, &mut s, "select * from pp_arrpart where a in ('{4, 5}', '{1}')"),
        ["Append", "  ->  Seq Scan on pp_arrpart1 pp_arrpart_1", f, "  ->  Seq Scan on pp_arrpart2 pp_arrpart_2", f]
    );
}

#[test]
fn constants_are_what_the_planner_folds_them_to() {
    let (db, mut s) = pg();
    // (eval_const_expressions, clauses.c:2236, and evaluate_function, :4406: an immutable function of constants is
    // its value)
    for (cond, filter) in [
        ("a = 1 + 1", "(a = 2)"),
        ("b = 'x' || 'y'", "(b = 'xy'::text)"),
        ("a = abs(-3)", "(a = 3)"),
        ("a > 2 - 5", "(a > '-3'::integer)"),
        ("a in (1 + 1, 3)", "(a = ANY ('{2,3}'::integer[]))"),
        ("a = 1 and 2 = 2", "(a = 1)"),
    ] {
        assert_eq!(
            plan(&db, &mut s, &format!("select * from xt where {cond}")),
            ["Seq Scan on xt", &format!("  Filter: {filter}")]
        );
    }
    // (a strict operator of a null constant is null (:4454), and a null condition no row passes: a dummy relation)
    for cond in ["a = null", "a = null and b = 'x'", "not (a = null)", "a < null::int"] {
        assert_eq!(
            plan(&db, &mut s, &format!("select * from xt where {cond}")),
            ["Result", "  One-Time Filter: false"],
            "{cond}"
        );
    }
    // (in a WHERE's OR a null is false: find_duplicate_ors, prepqual.c)
    assert_eq!(
        plan(&db, &mut s, "select * from xt where a = null or b = 'x'"),
        ["Seq Scan on xt", "  Filter: (b = 'x'::text)"]
    );
    // (in a select list it stays a null constant after the other arms, which isn't written here: Chronos's steps)
    let q = "explain (verbose, costs off) select a = null or b = 'x' from xt";
    let shown = lines(&db, &mut s, q);
    assert_eq!(shown, own_steps(&db, &mut s, q));
    assert_eq!(plan(&db, &mut s, "select 2 * 3"), ["Result"]);
}

#[test]
fn what_planning_meets_fails_explain_too() {
    for mode in ["chronos", "postgres"] {
        let (db, mut s) = (Db::new(), Session::new(MAIN));
        run(&db, &mut s, &format!("set chronos.explain = {mode}"));
        run(&db, &mut s, "create table xt (a int, b text); insert into xt values (1, 'x')");
        for (q, code) in [
            ("select 1/0", "22012"),
            ("select 'abc'::int", "22P02"),
            ("select * from xt where a = 1/0", "22012"),
            ("select * from xt where a > 0 or 1/0 = 1", "22012"),
            ("select a from xt order by a limit 1/0", "22012"),
            ("update xt set a = 1/0", "22012"),
            ("insert into xt values (1/0, 'y')", "22012"),
        ] {
            assert_eq!(fails(&db, &mut s, &format!("explain (costs off) {q}")).0, code, "{mode}: {q}");
        }
        // (what the planner doesn't work out it never meets: a CASE's arm after a WHEN that is true, a COALESCE's
        // after a constant, an AND's after a false, a row's value)
        for q in [
            "select case when true then 1 else 1/0 end",
            "select coalesce(a, 1, 1/0) from xt",
            "select * from xt where false and 1/0 = 1",
            "select * from xt where a / (a - 1) = 1",
            "select b::int from xt",
        ] {
            let q = format!("explain (costs off) {q}");
            if let Err(e) = s.run(&db, &q, &[]) {
                panic!("{mode}: {q}: {} {}", e.code, e.message);
            }
        }
    }
}

#[test]
fn sort_keys_show_their_collation() {
    let (db, mut s) = pg();
    run(&db, &mut s, "create table tc (a text collate \"C\", b text, c int)");
    // (show_sortorder_options, explain.c:2640: a collation other than the type's default)
    assert_eq!(
        plan(&db, &mut s, "select * from tc order by a"),
        ["Sort", "  Sort Key: a COLLATE \"C\"", "  ->  Seq Scan on tc"]
    );
    assert_eq!(
        plan(&db, &mut s, "select * from tc order by b collate \"C\""),
        ["Sort", "  Sort Key: b COLLATE \"C\"", "  ->  Seq Scan on tc"]
    );
    assert_eq!(
        plan(&db, &mut s, "select * from tc order by b, c desc"),
        ["Sort", "  Sort Key: b, c DESC", "  ->  Seq Scan on tc"]
    );
}

#[test]
fn casts_relabels_and_boolean_tests() {
    let (db, mut s) = pg();
    for q in [
        "create table cp (a int) partition by range (a)",
        "create table cp1 partition of cp for values from (0) to (10)",
        "create table cp2 partition of cp for values from (10) to (20)",
        "create table vp (v varchar) partition by list (v)",
        "create table vp1 partition of vp for values in ('a')",
        "create table vp2 partition of vp for values in ('b')",
        "create table bp (a bool) partition by list (a)",
        "create table bp_t partition of bp for values in (true)",
        "create table bp_f partition of bp for values in (false)",
        "create table vc (a varchar, b int)",
    ] {
        run(&db, &mut s, q);
    }
    // (int4 to text is a CoerceViaIO, which match_clause_to_partition_key doesn't look through: nothing pruned)
    let f = "        Filter: ((a)::text = '5'::text)";
    assert_eq!(
        plan(&db, &mut s, "select * from cp where a::text = '5'"),
        ["Append", "  ->  Seq Scan on cp1 cp_1", f, "  ->  Seq Scan on cp2 cp_2", f]
    );
    // (varchar to text is a RelabelType, which it does)
    assert_eq!(
        plan(&db, &mut s, "select * from vp where v::text = 'a'"),
        ["Seq Scan on vp1 vp", "  Filter: ((v)::text = 'a'::text)"]
    );
    // (and which costs nothing: the conditions keep the order they are written in)
    assert_eq!(
        plan(&db, &mut s, "select * from vc where a::text < 'x' and b < 1"),
        ["Seq Scan on vc", "  Filter: (((a)::text < 'x'::text) AND (b < 1))"]
    );
    // (IS NOT DISTINCT FROM true is a DistinctExpr, not the BooleanTest IS TRUE is)
    let f = "        Filter: (NOT (a IS DISTINCT FROM true))";
    assert_eq!(
        plan(&db, &mut s, "select * from bp where a is not distinct from true"),
        ["Append", "  ->  Seq Scan on bp_f bp_1", f, "  ->  Seq Scan on bp_t bp_2", f]
    );
    assert_eq!(plan(&db, &mut s, "select * from bp where a is true"), ["Seq Scan on bp_t bp", "  Filter: (a IS TRUE)"]);
    run(&db, &mut s, "create view bv as select a is not distinct from true as x, a is true as y from bp");
    assert_eq!(
        run(&db, &mut s, "select pg_get_viewdef('bv')")[0][0],
        " SELECT (NOT (a IS DISTINCT FROM true)) AS x,\n    (a IS TRUE) AS y\n   FROM bp;"
    );
}

/// Chronos's own steps for `q` (chronos.explain = chronos), the session set back to postgres after.
fn own_steps(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    run(db, s, "set chronos.explain = chronos");
    let own = lines(db, s, q);
    run(db, s, "set chronos.explain = postgres");
    own
}

#[test]
fn target_lists_parallel_scans_and_projections() {
    let (db, mut s) = pg();
    // (a system column read: no physical target list, use_physical_tlist, createplan.c:896)
    assert_eq!(
        lines(&db, &mut s, "explain (verbose, costs off) select count(tableoid) from xt"),
        ["Aggregate", "  Output: count(tableoid)", "  ->  Seq Scan on public.xt", "        Output: tableoid"]
    );
    assert_eq!(
        lines(&db, &mut s, "explain (verbose, costs off) select count(b) from xt"),
        ["Aggregate", "  Output: count(b)", "  ->  Seq Scan on public.xt", "        Output: a, b"]
    );
    // (a table of min_parallel_table_scan_size or more gets partial paths, which aren't modelled: Chronos's steps;
    // with max_parallel_workers_per_gather 0 there are none)
    run(&db, &mut s, "insert into xt select g, repeat('x', 100) from generate_series(1, 1000) g");
    run(&db, &mut s, "set min_parallel_table_scan_size = '64kB'");
    let q = "explain (costs off) select * from xt where a = 1";
    let shown = lines(&db, &mut s, q);
    assert_eq!(shown, own_steps(&db, &mut s, q));
    run(&db, &mut s, "set max_parallel_workers_per_gather = 0");
    assert_eq!(lines(&db, &mut s, q), ["Seq Scan on xt", "  Filter: (a = 1)"]);
    run(&db, &mut s, "reset max_parallel_workers_per_gather");
    run(&db, &mut s, "reset min_parallel_table_scan_size");
    // (an inheritance tree's Append doesn't project: a column twice needs a Result over it, which isn't made here)
    run(&db, &mut s, "create table inhp (a int, b int)");
    run(&db, &mut s, "create table inhc (c int) inherits (inhp)");
    let q = "explain (costs off) select a, a from inhp";
    let shown = lines(&db, &mut s, q);
    assert_eq!(shown, own_steps(&db, &mut s, q));
    assert_eq!(
        lines(&db, &mut s, "explain (costs off) select b, a from inhp"),
        ["Append", "  ->  Seq Scan on inhp inhp_1", "  ->  Seq Scan on inhc inhp_2"]
    );
}
