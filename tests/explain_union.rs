//! EXPLAIN of a statement that opens with `(`, as Django writes a union (`EXPLAIN (SELECT ...) UNION (SELECT ...)`):
//! the `(` is the query's, not an option list's, since no option is named SELECT.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn plan(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q).into_iter().map(|r| r.join(" ")).collect::<Vec<_>>().join("\n")
}

#[test]
fn a_parenthesized_union_is_the_statement() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE tag (id int4 PRIMARY KEY, name text)");
    let p =
        plan(&db, &mut s, "EXPLAIN (SELECT id, name FROM tag) UNION (SELECT id, name FROM tag WHERE name = 'test')");
    assert!(p.contains("Append") || p.contains("Unique"), "{p}");
    // with options before it, and in another format
    let j =
        plan(&db, &mut s, "EXPLAIN (FORMAT JSON) (SELECT id FROM tag) UNION (SELECT id FROM tag WHERE name = 'test')");
    assert!(serde_json::from_str::<serde_json::Value>(&j).is_ok(), "{j}");
    // a query in two parentheses, VALUES and WITH
    plan(&db, &mut s, "EXPLAIN ((SELECT 1))");
    plan(&db, &mut s, "EXPLAIN (VALUES (1)) UNION (VALUES (2))");
    plan(&db, &mut s, "EXPLAIN (WITH x AS (SELECT 1) SELECT * FROM x)");
    // an option list is still one
    assert!(!plan(&db, &mut s, "EXPLAIN (COSTS OFF) SELECT 1").contains("cost="));
    assert_eq!(s.run(&db, "EXPLAIN (NOPE) SELECT 1", &[]).unwrap_err().code, "42601");
}

#[test]
fn a_union_of_many_shows_each() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE tag (id int4 PRIMARY KEY, name text)");
    // (Django's test_multi_page_text_explain: a plan of 100 lines and more)
    let arms = vec!["(SELECT id, name FROM tag)"; 100].join(" UNION ");
    let p = plan(
        &db,
        &mut s,
        &format!("EXPLAIN (FORMAT TEXT) (SELECT id, name FROM tag WHERE name = 'test') UNION {arms}"),
    );
    assert!(p.lines().count() >= 100, "{p}");
}

#[test]
fn a_union_run_once_per_outer_row_is_still_its_parts() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE inner_t (id int4 PRIMARY KEY); CREATE TABLE outer_t (id int4);
         INSERT INTO inner_t SELECT g FROM generate_series(1, 50) g; INSERT INTO outer_t SELECT g FROM generate_series(1, 50) g",
    );
    let q = "EXPLAIN (ANALYZE, COSTS OFF) SELECT (SELECT id FROM inner_t WHERE id = o.id UNION SELECT id FROM inner_t WHERE id = o.id) FROM outer_t o";
    // (the plan doesn't grow with the outer rows: each part's reads add up as loops)
    let p = plan(&db, &mut s, q);
    assert!(p.lines().count() < 15, "{p}");
}

#[test]
fn nested_unions_parts_are_each_their_own() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE tag (id int4 PRIMARY KEY, name text)");
    // a 32-part union beside a 2-part one: 34 scans
    let big = vec!["SELECT id FROM tag"; 32].join(" UNION ALL ");
    let p = plan(
        &db,
        &mut s,
        &format!("EXPLAIN (COSTS OFF) ({big}) UNION ALL (SELECT id FROM tag UNION ALL SELECT id FROM tag)"),
    );
    assert_eq!(p.matches("Seq Scan on tag").count(), 34, "{p}");
}
