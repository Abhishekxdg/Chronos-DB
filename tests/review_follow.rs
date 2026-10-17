//! Follow-ups from the review: limits on what inlined functions and views add to a query, linear
//! copies of DDL text, `(SELECT ...)` starting a statement, depth checks in the JSON aggregates,
//! and dates and timestamps out of range. Each gives the right answer or a clean error.

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
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql:.100}: {}", e.message));
    let mut v: Vec<String> = out.last().unwrap().rows.iter().map(|r| r[0].to_text()).collect();
    v.sort();
    v
}

/// `f` on a thread with a 2 MB stack.
fn small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(2 << 20).spawn(f).unwrap().join().unwrap();
}

fn nested(open: &str, mid: &str, close: &str, n: usize) -> String {
    format!("{}{mid}{}", open.repeat(n), close.repeat(n))
}

#[cfg_attr(debug_assertions, ignore = "measures release-build stack use; the VM runs it in release")]
#[test]
fn sql_functions_inlined_in_their_own_arguments_stay_shallow() {
    small_stack(|| {
        let db = Db::new();
        // a body 300 levels deep, called in its own argument 100 times: 30,000 levels inlined
        let body = format!("select x{}", " + 1".repeat(300));
        run(&db, MAIN, &format!("create function f(x int) returns int language sql as $${body}$$"), &[]).unwrap();
        run(&db, MAIN, "create function g(x int) returns int language sql as 'select x + 1'", &[]).unwrap();
        assert_eq!(col(&db, &format!("select {}", nested("f(", "0", ")", 100))), ["30000"]);
        assert_eq!(col(&db, &format!("select {}", nested("g(", "0", ")", 100))), ["100"]);
    });
}

#[test]
fn views_reading_views_are_bounded() {
    let db = Db::new();
    run(&db, MAIN, "create table t (a int); insert into t values (1)", &[]).unwrap();
    // a chain: refused past the depth the parser allows, not a crash
    run(&db, MAIN, "create view v0 as select a from t", &[]).unwrap();
    let mut last = 0;
    for i in 1..200 {
        match code(&db, &format!("create view v{i} as select a from v{}", i - 1)) {
            "ok" => last = i,
            c => {
                assert_eq!(c, "54001", "v{i}");
                break;
            }
        }
    }
    assert!((30..200).contains(&last), "{last}");
    assert_eq!(col(&db, &format!("select a from v{last}")), ["1"]);
    // each reading the one before twice: refused before it doubles far
    run(&db, MAIN, "create view w0 as select a from t", &[]).unwrap();
    let refused = (1..40)
        .any(|i| code(&db, &format!("create view w{i} as select x.a from w{p} x, w{p} y", p = i - 1)) == "54001");
    assert!(refused);
    // and none can read itself
    assert_eq!(code(&db, "create or replace view v0 as select a from v1"), "42P17");
}

#[test]
fn ddl_text_is_copied_to_its_end_only() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table t (a int, b text, check (a > 0), check (length(b) < 5)); \
         create index i on t ((a + 1), lower(b)) where a > 1; \
         create view v as select a, b from t where a > 2; \
         insert into t values (1, 'x'), (3, 'y')",
        &[],
    )
    .unwrap();
    assert_eq!(code(&db, "insert into t values (0, 'x')"), "23514");
    assert_eq!(code(&db, "insert into t values (5, 'xxxxxx')"), "23514");
    assert_eq!(col(&db, "select a from v"), ["3"]);
    assert_eq!(col(&db, "select b from t where a + 1 = 4"), ["y"]);
    // many statements with such parts, in one script
    let script: String = (0..3000).map(|i| format!("create table c{i} (a int check (a > {i}));")).collect();
    run(&db, MAIN, &script, &[]).unwrap();
    assert_eq!(code(&db, "insert into c2999 values (2999)"), "23514");
}

#[test]
fn a_statement_can_start_with_a_parenthesized_query() {
    let db = Db::new();
    assert_eq!(col(&db, "(select 1) union select 2"), ["1", "2"]);
    assert_eq!(col(&db, "((select 1)) union all select 1"), ["1", "1"]);
    assert_eq!(col(&db, "(select 1)"), ["1"]);
    assert_eq!(col(&db, "(select 1 union select 2) intersect select 2"), ["2"]);
    assert_eq!(col(&db, "(select 3 union select 1 order by 1 limit 1) union select 5"), ["1", "5"]);
    assert_eq!(col(&db, "(select 2 union select 1 order by 1 limit 1)"), ["1"]);
    assert_eq!(col(&db, "(select 1 union select 2) order by 1 desc limit 1"), ["2"]);
    assert_eq!(code(&db, &nested("(", "select 1", ")", 100_000)), "54001");
}

#[test]
fn json_aggregates_stop_at_the_jsonb_depth() {
    let db = Db::new();
    let deep = nested("[", "1", "]", 100);
    assert_eq!(code(&db, &format!("select jsonb_agg(x) from (select '{deep}'::jsonb x) s")), "54000");
    assert_eq!(code(&db, &format!("select jsonb_object_agg('k', x) from (select '{deep}'::jsonb x) s")), "54000");
    assert_eq!(col(&db, "select jsonb_agg(x)::text from (select '[1]'::jsonb x) s"), ["[[1]]"]);
}

#[test]
fn dates_and_timestamps_out_of_range_are_22008() {
    let db = Db::new();
    assert_eq!(code(&db, "select '5874898-01-01'::date"), "22008");
    assert_eq!(code(&db, "select '300000-01-01'::timestamp"), "22008");
    assert_eq!(code(&db, "select '290000-01-01'::timestamp + interval '10000 years'"), "22008");
    assert_eq!(code(&db, "select interval '10000 years' + '290000-01-01'::timestamptz"), "22008");
    assert_eq!(code(&db, "select '2024-01-01'::timestamp - '300000-01-01'::timestamp"), "22008");
    assert_eq!(
        code(
            &db,
            "select * from generate_series('290000-01-01'::timestamp, '292000-01-01'::timestamp, interval '5000 years')"
        ),
        "22008"
    );
    // a field out of range is 22008 too, as in Postgres; not a date at all is 22007
    assert_eq!(code(&db, "select '2024-02-30'::date"), "22008");
    assert_eq!(code(&db, "select 'soon'::date"), "22007");
    assert_eq!(col(&db, "select ('290000-01-01'::timestamp + interval '1 year')::text"), ["290001-01-01 00:00:00"]);
}
