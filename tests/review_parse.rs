//! Parser limits from the review: nesting depth, WITH copies, parameter numbers, type names,
//! set operation precedence, index names with '/', and WITH queries in ORDER BY / LIMIT.

use chronos::sql::run;
use chronos::{Db, MAIN};

fn code(db: &Db, sql: &str) -> &'static str {
    match run(db, MAIN, sql, &[]) {
        Ok(_) => "ok",
        Err(e) => e.code,
    }
}

/// The first column of the last statement's rows, as text, sorted.
fn col(db: &Db, sql: &str) -> Vec<String> {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    let mut v: Vec<String> = out.last().unwrap().rows.iter().map(|r| r[0].to_text()).collect();
    v.sort();
    v
}

/// `f` on a thread with the 2 MB stack connection threads get.
fn small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(2 << 20).spawn(f).unwrap().join().unwrap();
}

fn nested(open: &str, mid: &str, close: &str, n: usize) -> String {
    format!("{}{mid}{}", open.repeat(n), close.repeat(n))
}

#[test]
fn deep_nesting_is_refused_not_a_crash() {
    small_stack(|| {
        let db = Db::new();
        for sql in [
            format!("select {}", nested("(", "1", ")", 200_000)),
            format!("select {}true", "not ".repeat(200_000)),
            format!("select {}1", "- ".repeat(200_000)),
            format!("select {}", vec!["1"; 1_000_000].join("+")),
            format!("select 1{}", "::int".repeat(200_000)),
            format!("select {}", nested("(select ", "1", ")", 100_000)),
            format!("select * from {}", nested("(select * from ", "(select 1) y", ") x", 100_000)),
            format!("select {}", nested("array[", "1", "]", 200_000)),
            format!("select {}", nested("abs(", "1", ")", 200_000)),
            format!("{}select 1", "explain ".repeat(200_000)),
            format!("select 1 where {}", nested("(1 = 1 or ", "true", ")", 200_000)),
            format!("select 1 where {}", nested("(true and ", "true", ")", 200_000)),
        ] {
            assert_eq!(code(&db, &sql), "54001", "{}", &sql[..40]);
        }
    });
}

#[cfg_attr(debug_assertions, ignore = "measures release-build stack use; the VM runs it in release")]
#[test]
fn nesting_just_inside_the_limit_runs_on_a_small_stack() {
    small_stack(|| {
        let db = Db::new();
        run(&db, MAIN, "create table t (a int); insert into t values (1)", &[]).unwrap();
        for sql in [
            format!("select {} from t", nested("(", "a", ")", 100)),
            format!("select {}true from t", "not not ".repeat(190)),
            format!("select {}a from t", "- - ".repeat(190)),
            format!("select {} from t", vec!["a"; 380].join("+")),
            format!("select {} from t", nested("coalesce(", "a", ", 1)", 100)),
            format!("select {}", nested("(select ", "1", ")", 33)),
            format!("select * from {}", nested("(select * from ", "t", ") x", 35)),
            format!("select 1 where exists {}", nested("(select 1 where exists ", "(select 1)", ")", 33)),
            // AND and OR chains nest by their log: a million terms are fine
            format!("select 1 where {}", vec!["1=1"; 1_000_000].join(" and ")),
            format!("select 1 from t where {} or a=1", vec!["a=2"; 1_000_000].join(" or ")),
            // an OR in each parenthesis, as generated queries nest them (sqllogictest's go 37 deep)
            format!("select 1 from t where {}", nested("(a = 1 or ", "a = 2", ")", 60)),
            format!("select 1 from t where {}", nested("(a = 1 and a < 3 or ", "a = 2", ")", 50)),
        ] {
            assert_eq!(code(&db, &sql), "ok", "{}", &sql[..40]);
        }
        assert_eq!(col(&db, &format!("select count(*) from t where {}", vec!["a=1"; 5000].join(" and "))), ["1"]);
        assert_eq!(col(&db, &format!("select count(*) from t where {} or a=1", vec!["a=2"; 5000].join(" or "))), ["1"]);
        let ctes: Vec<String> = (1..30).map(|i| format!("c{i} as (select * from c{})", i - 1)).collect();
        let sql = format!("with c0 as (select 1 a), {} select * from c29", ctes.join(", "));
        assert_eq!(col(&db, &sql), ["1"]);
        let ctes: Vec<String> = (1..45).map(|i| format!("c{i} as (select * from c{})", i - 1)).collect();
        let sql = format!("with c0 as (select 1 a), {} select * from c44", ctes.join(", "));
        assert_eq!(code(&db, &sql), "54001", "WITH queries nest as deep as they use each other");
    });
}

#[test]
fn with_queries_that_double_are_refused() {
    let db = Db::new();
    let ctes: Vec<String> = (1..=40).map(|i| format!("a{i} as (select * from a{p}, a{p} b)", p = i - 1)).collect();
    let sql = format!("with a0 as (select 1), {} select 1", ctes.join(", "));
    assert_eq!(code(&db, &sql), "54001");
    // BETWEEN copies its left side: nested, it doubled too
    let deep = (0..60).fold("1".to_string(), |e, _| format!("({e}) between 0 and 2"));
    assert_eq!(code(&db, &format!("select {deep}")), "54001");
    // a few copies are fine
    assert_eq!(col(&db, "with a as (select 1 x), b as (select * from a, a y) select count(*) from b, b z"), ["1"]);
}

#[test]
fn parameter_numbers_stop_at_65535() {
    let db = Db::new();
    for p in ["$1000000000000", "$18446744073709551615", "$99999999999999999999999", "$65536"] {
        assert_eq!(code(&db, &format!("select {p}")), "42P02", "{p}");
    }
    assert_eq!(code(&db, "select $0"), "42P02"); // as Postgres: "there is no parameter $0"
    assert_ne!(code(&db, "select $65535"), "ok", "a parameter not given");
}

#[test]
fn type_names_are_at_most_63_bytes() {
    let db = Db::new();
    let long = "t".repeat(64);
    assert_eq!(code(&db, &format!("select null::\"{long}\"")), "42622");
    assert_eq!(code(&db, &format!("select null::{long}")), "42622");
    assert_eq!(code(&db, &format!("create type {long} as enum ('a')")), "42622");
    assert_eq!(code(&db, &format!("select null::{}", &long[1..])), "42704");
    run(&db, MAIN, "create type mood as enum ('sad', 'ok')", &[]).unwrap();
    assert_eq!(col(&db, "select 'ok'::mood"), ["ok"]);
}

#[test]
fn intersect_binds_tighter_than_union_and_except() {
    let db = Db::new();
    assert_eq!(col(&db, "select 1 union select 2 intersect select 2"), ["1", "2"]);
    assert_eq!(col(&db, "select 1 except select 1 intersect select 2"), ["1"]);
    assert_eq!(col(&db, "select 2 intersect select 2 union select 1"), ["1", "2"]);
    assert_eq!(col(&db, "select 1 union select 2 intersect select 3 union select 4"), ["1", "4"]);
    assert_eq!(col(&db, "select 1 union (select 2 union select 3 order by 1 limit 1) intersect select 2"), ["1", "2"]);
}

#[test]
fn index_and_constraint_names_cant_have_a_slash() {
    let db = Db::new();
    run(&db, MAIN, "create table t (a int, b int)", &[]).unwrap();
    run(&db, MAIN, "create unique index u on t (a)", &[]).unwrap();
    assert_eq!(code(&db, "create unique index \"u/x\" on t (b)"), "42601");
    assert_eq!(code(&db, "alter table t add constraint \"u/y\" unique (b)"), "42601");
    assert_eq!(code(&db, "create table s (a int, constraint \"k/1\" unique (a))"), "42601");
    // what's left still holds
    run(&db, MAIN, "create unique index v on t (b); insert into t values (1, 1)", &[]).unwrap();
    run(&db, MAIN, "drop index u", &[]).unwrap();
    assert_eq!(code(&db, "insert into t values (2, 1)"), "23505");
}

#[test]
fn with_queries_reach_order_by_group_by_limit_and_offset() {
    let db = Db::new();
    run(&db, MAIN, "create table c (v int); insert into c values (2)", &[]).unwrap();
    let rows = |sql: &str| run(&db, MAIN, sql, &[]).unwrap()[0].rows.len();
    let with = "with c as (select 5 v) select x from generate_series(1, 10) x";
    assert_eq!(rows(&format!("{with} limit (select v from c)")), 5);
    assert_eq!(rows(&format!("{with} offset (select v from c)")), 5);
    assert_eq!(rows(&format!("{with} order by x % (select v from c), x limit 1")), 1);
    assert_eq!(col(&db, &format!("{with} order by x % (select v from c), x limit 1")), ["5"]);
    let grouped = "with c as (select 5 v) select count(*) from generate_series(1, 10) x group by x % (select v from c)";
    assert_eq!(rows(grouped), 5);
}
