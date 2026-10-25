//! Review of the EXPLAIN plan IR (src/sql/explain.rs, src/sql/plan.rs): with chronos.explain = postgres, EXPLAIN
//! shows the plan PostgreSQL 16 would choose or, where that plan isn't made here, Chronos's own steps (all or
//! nothing, never a guess). Each expected plan is PostgreSQL 16.15's for the same statements on a fresh database.

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
        Ok(out) => panic!("{q}: should fail, gave {:?}", out.last().map(|o| &o.rows)),
        Err(e) => (e.code, e.message),
    }
}

fn pg() -> (Db, Session) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "set chronos.explain = postgres");
    (db, s)
}

/// `q`'s plan with chronos.explain = postgres: PostgreSQL's (`want`), or else Chronos's own steps, never another.
fn pg_plan_or_steps(db: &Db, s: &mut Session, q: &str, want: &[&str]) {
    let shown = lines(db, s, q);
    if shown == want {
        return;
    }
    run(db, s, "set chronos.explain = chronos");
    let own = lines(db, s, q);
    run(db, s, "set chronos.explain = postgres");
    assert_eq!(shown, own, "{q}: neither PostgreSQL's plan {want:?} nor Chronos's steps");
}

#[test]
fn a_cast_of_an_integer_key_to_text_prunes_nothing() {
    let (db, mut s) = pg();
    for q in [
        "create table cp (a int) partition by range (a)",
        "create table cp1 partition of cp for values from (0) to (10)",
        "create table cp2 partition of cp for values from (10) to (20)",
    ] {
        run(&db, &mut s, q);
    }
    // (int4 to text is CoerceViaIO, not a RelabelType: match_clause_to_partition_key, partprune.c:1880, doesn't
    // look through it, so the clause doesn't match the key)
    for (cond, filter) in [
        ("a::text < '5'", "((a)::text < '5'::text)"),
        ("a::text = '5'", "((a)::text = '5'::text)"),
        ("a::text is null", "((a)::text IS NULL)"),
    ] {
        let f = format!("        Filter: {filter}");
        pg_plan_or_steps(
            &db,
            &mut s,
            &format!("explain (costs off) select * from cp where {cond}"),
            &["Append", "  ->  Seq Scan on cp1 cp_1", &f, "  ->  Seq Scan on cp2 cp_2", &f],
        );
    }
}

#[test]
fn errors_postgres_meets_planning_still_fail() {
    // (a literal is cast when the statement is analyzed, and a constant expression folded when it is planned:
    // EXPLAIN without ANALYZE meets those errors in Postgres too)
    for mode in ["chronos", "postgres"] {
        let (db, mut s) = (Db::new(), Session::new(MAIN));
        run(&db, &mut s, &format!("set chronos.explain = {mode}"));
        run(&db, &mut s, "create table xt (a int, b text)");
        assert_eq!(fails(&db, &mut s, "explain (costs off) select 'abc'::int").0, "22P02", "{mode}");
        assert_eq!(fails(&db, &mut s, "explain (costs off) select 1/0").0, "22012", "{mode}");
        assert_eq!(fails(&db, &mut s, "explain (costs off) select * from xt where a = 1/0").0, "22012", "{mode}");
        // (a CASE arm whose WHEN is false or null is dropped before its result is folded)
        run(&db, &mut s, "explain (costs off) select case when false then 1/0 else a end from xt");
        run(&db, &mut s, "explain (costs off) select case when null then 1/0 else a end from xt");
        // (a simple CASE's constant operand compared with a constant WHEN: an arm that can't match is dropped too)
        run(&db, &mut s, "explain (costs off) select case 1 when 2 then 1/0 else a end from xt");
        assert_eq!(
            fails(&db, &mut s, "explain (costs off) select case 1 when 1 then 1/0 else a end from xt").0,
            "22012",
            "{mode}"
        );
        assert_eq!(
            fails(&db, &mut s, "explain (costs off) select case when a > 0 then 1/0 else a end from xt").0,
            "22012",
            "{mode}"
        );
    }
}

#[test]
fn a_sort_key_shows_a_collation_other_than_its_types() {
    let (db, mut s) = pg();
    run(&db, &mut s, "create table tc (a text collate \"C\", b text)");
    // (show_sortorder_options, explain.c:2700)
    pg_plan_or_steps(
        &db,
        &mut s,
        "explain (costs off) select * from tc order by a",
        &["Sort", "  Sort Key: a COLLATE \"C\"", "  ->  Seq Scan on tc"],
    );
    pg_plan_or_steps(
        &db,
        &mut s,
        "explain (costs off) select * from tc order by b collate \"C\"",
        &["Sort", "  Sort Key: b COLLATE \"C\"", "  ->  Seq Scan on tc"],
    );
}

#[test]
fn an_inheritance_append_that_must_project_has_a_result() {
    let (db, mut s) = pg();
    run(&db, &mut s, "create table inhp (a int, b int)");
    run(&db, &mut s, "create table inhc (c int) inherits (inhp)");
    pg_plan_or_steps(
        &db,
        &mut s,
        "explain (costs off) select a, a from inhp",
        &["Result", "  ->  Append", "        ->  Seq Scan on inhp inhp_1", "        ->  Seq Scan on inhc inhp_2"],
    );
}

#[test]
fn an_aggregate_over_a_system_column_reads_no_physical_target_list() {
    let (db, mut s) = pg();
    run(&db, &mut s, "create table xt (a int, b text)");
    pg_plan_or_steps(
        &db,
        &mut s,
        "explain (verbose, costs off) select count(tableoid) from xt",
        &["Aggregate", "  Output: count(tableoid)", "  ->  Seq Scan on public.xt", "        Output: tableoid"],
    );
}

#[test]
fn a_relabeling_cast_costs_nothing() {
    let (db, mut s) = pg();
    run(&db, &mut s, "create table vc (a varchar, b int)");
    // (varchar to text is a RelabelType, which cost_qual_eval charges nothing: the two keep their order)
    pg_plan_or_steps(
        &db,
        &mut s,
        "explain (costs off) select * from vc where a::text < 'x' and b < 1",
        &["Seq Scan on vc", "  Filter: (((a)::text < 'x'::text) AND (b < 1))"],
    );
}

#[test]
fn is_not_distinct_from_true_is_no_boolean_test() {
    let (db, mut s) = pg();
    for q in [
        "create table bp (a bool) partition by list (a)",
        "create table bp_t partition of bp for values in (true)",
        "create table bp_f partition of bp for values in (false)",
        "create table bp_d partition of bp default",
    ] {
        run(&db, &mut s, q);
    }
    let f = "        Filter: (NOT (a IS DISTINCT FROM true))";
    pg_plan_or_steps(
        &db,
        &mut s,
        "explain (costs off) select * from bp where a is not distinct from true",
        &[
            "Append",
            "  ->  Seq Scan on bp_f bp_1",
            f,
            "  ->  Seq Scan on bp_t bp_2",
            f,
            "  ->  Seq Scan on bp_d bp_3",
            f,
        ],
    );
}

#[test]
fn a_comparison_with_null_is_false() {
    let (db, mut s) = pg();
    run(&db, &mut s, "create table xt (a int, b text)");
    pg_plan_or_steps(
        &db,
        &mut s,
        "explain (costs off) select * from xt where a = null",
        &["Result", "  One-Time Filter: false"],
    );
}

#[test]
fn a_table_large_enough_for_parallel_workers_gets_a_gather() {
    let (db, mut s) = pg();
    run(&db, &mut s, "create table wide (a int, b text)");
    run(&db, &mut s, "insert into wide select g, repeat('x', 100) from generate_series(1, 200000) g");
    // (3449 pages, over min_parallel_table_scan_size: create_plain_partial_paths, allpaths.c:851, and the Gather
    // is cheaper)
    pg_plan_or_steps(
        &db,
        &mut s,
        "explain (costs off) select * from wide where a = 1",
        &["Gather", "  Workers Planned: 2", "  ->  Parallel Seq Scan on wide", "        Filter: (a = 1)"],
    );
    pg_plan_or_steps(
        &db,
        &mut s,
        "explain (costs off) select count(*) from wide",
        &[
            "Finalize Aggregate",
            "  ->  Gather",
            "        Workers Planned: 2",
            "        ->  Partial Aggregate",
            "              ->  Parallel Seq Scan on wide",
        ],
    );
}

#[test]
fn constants_are_folded_as_the_planner_folds_them() {
    let (db, mut s) = pg();
    run(&db, &mut s, "create table xt (a int, b text)");
    // (eval_const_expressions, clauses.c:2236, and evaluate_function, clauses.c:4406: an immutable function of
    // constants is its value)
    for (cond, filter) in [("a = 1 + 1", "(a = 2)"), ("b = 'x' || 'y'", "(b = 'xy'::text)"), ("a = abs(-3)", "(a = 3)")]
    {
        pg_plan_or_steps(
            &db,
            &mut s,
            &format!("explain (costs off) select * from xt where {cond}"),
            &["Seq Scan on xt", &format!("  Filter: {filter}")],
        );
    }
}
