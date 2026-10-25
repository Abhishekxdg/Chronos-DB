//! Review of the join-order lane (`sql::opt`, `chronos.follow_pg_plan`) and the statistics VACUUM and ANALYZE
//! record. Every expected value here is what a real PostgreSQL 16.15 gives for the same statements (pg_class,
//! EXPLAIN, and the rows in the order its plan returns them).

use chronos::sql::{Session, Value, take_notices};
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

fn joined(rows: &[Vec<String>]) -> Vec<String> {
    rows.iter().map(|r| r.join(",")).collect()
}

fn stats(db: &Db, s: &mut Session, t: &str) -> String {
    run(db, s, &format!("select relpages, reltuples, relallvisible from pg_class where relname = '{t}'"))[0].join(",")
}

/// maintain_every.sql: a role that isn't a superuser analyzes its own temporary table, which has no rows: no
/// warning (it may), and reltuples 0. (The full regression run of this branch has maintain_every at 90%, from
/// 93%: a "permission denied to analyze" WARNING the routing to the temporary tables' database brought.)
#[test]
fn a_role_analyzes_its_own_temporary_table() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create role regress_maintain; set role regress_maintain");
    run(&db, &mut s, "create temp table past_inh_db_other (); create temp table past_inh_db_parent ()");
    take_notices();
    run(&db, &mut s, "analyze past_inh_db_parent");
    let notes: Vec<String> = take_notices().into_iter().map(|n| n.message).collect();
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(run(&db, &mut s, "select reltuples from pg_class where oid = 'past_inh_db_parent'::regclass"), [["0"]]);
    run(&db, &mut s, "vacuum past_inh_db_other");
    assert!(take_notices().is_empty());
}

/// `VACUUM (ONLY_DATABASE_STATS)` processes no table (vacuum.c: "We don't process any tables in this case"), so
/// pg_class is as it was; vacuum.sql runs it.
#[test]
fn only_database_stats_records_no_table() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table od (a int); insert into od values (1), (2)");
    run(&db, &mut s, "vacuum (only_database_stats)");
    assert_eq!(stats(&db, &mut s, "od"), "0,-1,0");
}

/// VACUUM and ANALYZE now write rows of the world (`_sonos_relstats/`): an agent that may only read can't make
/// them, as it can't REINDEX (see regress_tablespaces_and_index_describe.rs).
#[test]
fn an_agent_that_only_reads_records_no_statistics() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int); insert into t values (1), (2), (3)");
    run(&db, &mut s, "create agent reader with (can = 'read')");
    let mut agent = Session::new(MAIN);
    agent.agent = Some("reader".into());
    for q in ["analyze t", "vacuum t", "vacuum"] {
        assert_eq!(fails(&db, &mut agent, q).0, "42501", "{q}");
    }
    assert_eq!(stats(&db, &mut s, "t"), "0,-1,0");
}

/// CREATE INDEX on a table with rows sets the table's relpages and reltuples (index_update_stats, index.c), and
/// DROP INDEX leaves them: Postgres then estimates the table from them, not by the 10-page rule.
#[test]
fn create_index_records_the_tables_statistics() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table ci (a int, b int); insert into ci select g, g from generate_series(1, 3000) g");
    run(&db, &mut s, "create index ci_a on ci (a)");
    assert_eq!(stats(&db, &mut s, "ci"), "14,3000,0");
    run(&db, &mut s, "drop index ci_a");
    assert_eq!(stats(&db, &mut s, "ci"), "14,3000,0");
    // u (3,100 rows, vacuumed) restricted to 1,033 rows, ci to 1,000 (3,000 / 3): u outer (EXPLAIN: 13019.25).
    // With ci taken as never vacuumed it would be 1,055 (3,164 / 3) and go outer.
    run(&db, &mut s, "create table u (b int, c int); insert into u select g, g from generate_series(1, 3100) g");
    run(&db, &mut s, "vacuum u; set chronos.follow_pg_plan = on");
    let rows = run(&db, &mut s, "select * from ci, u where ci.a < 5 and u.b < 5");
    assert_eq!(joined(&rows[..5]), ["1,1,1,1", "2,2,1,1", "3,3,1,1", "4,4,1,1", "1,1,2,2"]);
}

/// A table rewrite (ALTER COLUMN ... TYPE) gives the table a new heap whose pg_class row is a never-vacuumed one
/// (make_new_heap, swap_relation_files): 0 pages, -1 tuples.
#[test]
fn a_rewrite_resets_the_statistics() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table rw (a int); insert into rw select g from generate_series(1, 3000) g; vacuum rw");
    assert_eq!(stats(&db, &mut s, "rw"), "14,3000,14");
    run(&db, &mut s, "alter table rw alter column a type bigint");
    assert_eq!(stats(&db, &mut s, "rw"), "0,-1,0");
}

/// pg_class.reltuples is a float4: what UPDATE pg_class SET reltuples keeps is rounded to one, and written as one
/// (the wire's text, as float4out writes it).
#[test]
fn reltuples_is_a_float4() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table od (a int)");
    run(&db, &mut s, "update pg_class set reltuples = 123456789 where relname = 'od'");
    let out = s.run(&db, "select reltuples from pg_class where relname = 'od'", &[]).unwrap().pop().unwrap();
    let ty = out.columns.as_ref().unwrap()[0].1;
    assert_eq!(chronos::sql::wire_text(&out.rows[0][0], ty).as_deref(), Some("1.2345679e+08"));
    assert_eq!(run(&db, &mut s, "select reltuples::float8 from pg_class where relname = 'od'"), [["123456792"]]);
}

/// A database-wide VACUUM records a materialized view too, as Postgres's get_all_vacuum_rels lists it.
#[test]
fn a_database_wide_vacuum_records_a_materialized_view() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create materialized view mv as select 1 as x; create view v as select 1 as y");
    run(&db, &mut s, "create sequence sq; vacuum");
    assert_eq!(stats(&db, &mut s, "mv"), "1,1,1");
}

/// Joins of three and four never-vacuumed tables on conditions no hash or merge join takes, in the order
/// Postgres 16 returns their rows (a 2,550 rows by the 10-page rule, b 1,270, c 2,260, d 850).
#[test]
fn chains_of_nested_loops_go_in_postgres_order() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        "create table a (x int)",
        "create table b (y int, t text)",
        "create table c (x int, y int)",
        "create table d (y int, t text, u text)",
        "insert into a values (1), (5), (3)",
        "insert into b values (2, 'b2'), (6, 'b6'), (4, 'b4'), (0, 'b0')",
        "insert into c values (1, 7), (3, 2)",
        "insert into d values (8, 'd8', 'x'), (2, 'd2', 'y'), (5, 'd5', 'z')",
        "set chronos.follow_pg_plan = on",
    ] {
        run(&db, &mut s, q);
    }
    let rows = run(&db, &mut s, "select a.x, b.y, c.x, c.y from a, b, c where a.x < b.y and b.y < c.y");
    assert_eq!(joined(&rows), ["1,2,1,7", "1,6,1,7", "5,6,1,7", "3,6,1,7", "1,4,1,7", "3,4,1,7"]);
    // Nested Loop (Nested Loop (Nested Loop (a, d), b), c)
    let rows =
        run(&db, &mut s, "select a.x, b.y, c.x, d.y from a, b, c, d where a.x < b.y and c.x < d.y and a.x < d.y");
    assert_eq!(
        joined(&rows),
        [
            "1,2,1,8", "1,2,3,8", "1,6,1,8", "1,6,3,8", "1,4,1,8", "1,4,3,8", "1,2,1,2", "1,6,1,2", "1,4,1,2",
            "1,2,1,5", "1,2,3,5", "1,6,1,5", "1,6,3,5", "1,4,1,5", "1,4,3,5", "5,6,1,8", "5,6,3,8", "3,6,1,8",
            "3,6,3,8", "3,4,1,8", "3,4,3,8", "3,6,1,5", "3,6,3,5", "3,4,1,5", "3,4,3,5",
        ]
    );
    // an OR, IS NOT NULL and <> as restrictions: a 1,417 rows, d 846, b 1,264 (Nested Loop (Nested Loop (a, d), b))
    let rows = run(
        &db,
        &mut s,
        "select a.x, b.y, d.y from a, b, d where (a.x < 2 or a.x > 4) and b.t is not null and d.y <> 5 \
         and a.x <= d.y",
    );
    assert_eq!(
        joined(&rows),
        ["1,2,8", "1,6,8", "1,4,8", "1,0,8", "1,2,2", "1,6,2", "1,4,2", "1,0,2", "5,2,8", "5,6,8", "5,4,8", "5,0,8"]
    );
    // JOIN ... ON: Nested Loop (Nested Loop (c, b), a)
    let rows = run(&db, &mut s, "select a.x, c.x, c.y, b.y from a join c on a.x < c.y join b on b.y > c.x");
    assert_eq!(
        joined(&rows),
        [
            "1,1,7,2", "5,1,7,2", "3,1,7,2", "1,1,7,6", "5,1,7,6", "3,1,7,6", "1,1,7,4", "5,1,7,4", "3,1,7,4",
            "1,3,2,6", "1,3,2,4"
        ]
    );
}
