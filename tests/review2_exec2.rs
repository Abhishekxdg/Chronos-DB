//! Second review, executor: window functions that ignored timeouts, state that outgrew
//! CHRONOS_WORK_MEM, equal numbers that hashed apart, and savepoint bookkeeping. One test per fix.
//! This file's process runs with a 1 MB CHRONOS_WORK_MEM, so state past 4 MB is refused.

use chronos::sql::Session;
use chronos::{AgentSpec, Can, Db, MAIN};

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // set before anything reads it (it's read once per process)
    ONCE.call_once(|| unsafe { std::env::set_var("CHRONOS_WORK_MEM", "1MB") });
}

fn db() -> (Db, Session) {
    setup();
    (Db::new(), Session::new(MAIN))
}

fn try_all(db: &Db, s: &mut Session, q: &str) -> Result<Vec<String>, (&'static str, String)> {
    match s.run(db, q, &[]) {
        Ok(out) => Ok(out.last().unwrap().rows.iter().map(|r| r[0].to_text()).collect()),
        Err(e) => Err((e.code, e.message)),
    }
}

fn all(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    try_all(db, s, q).unwrap_or_else(|(c, m)| panic!("{q}: {c} {m}"))
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    all(db, s, q).into_iter().next().unwrap_or_default()
}

fn code(db: &Db, s: &mut Session, q: &str) -> &'static str {
    match try_all(db, s, q) {
        Ok(v) => panic!("{q}: should fail, gave {v:.100?}"),
        Err((c, _)) => c,
    }
}

#[test]
fn window_frames_slide_and_stop_at_the_statement_timeout() {
    let (db, mut s) = db();
    // sliding frames give the same answers as starting over
    let q = "select sum(g) over (order by g rows between 1 preceding and 1 following) from generate_series(1, 5) g";
    assert_eq!(all(&db, &mut s, q), ["3", "6", "9", "12", "9"]);
    let q = "select sum(g) over (order by g rows between 2 following and 3 following) from generate_series(1, 5) g";
    assert_eq!(all(&db, &mut s, q), vec!["7".to_string(), "9".into(), "5".into(), sql_null(), sql_null()]);
    let q = "select count(x) over (order by g rows between current row and 1 following) \
             from (select g, nullif(g, 2) x from generate_series(1, 4) g) t";
    assert_eq!(all(&db, &mut s, q), ["1", "1", "2", "1"]);
    let q = "select sum(x) over (order by x range between 1 preceding and 1 following) from (select 1 x union all select 2 union all select 2 union all select 5) v";
    assert_eq!(all(&db, &mut s, q), ["5", "5", "5", "5"]);
    // a sum from each row to the end: linear now
    one(&db, &mut s, "set statement_timeout = 20000");
    let q = "select sum(g) over (order by g rows between current row and unbounded following) from generate_series(1, 40000) g";
    assert_eq!(one(&db, &mut s, q), "800020000");
    // max can't slide: quadratic, but the timeout stops it
    one(&db, &mut s, "set statement_timeout = 200");
    let q = "select max(g) over (order by g rows between current row and unbounded following) from generate_series(1, 40000) g";
    assert_eq!(code(&db, &mut s, q), "57014");
}

fn sql_null() -> String {
    chronos::sql::Value::Null.to_text()
}

#[test]
fn recursive_query_past_work_mem_is_a_clean_error() {
    let (db, mut s) = db();
    let q = "with recursive r(n, s) as (select 1, repeat('x', 100000) union all select n + 1, s from r where n < 200000) \
             select count(*) from r";
    assert_eq!(code(&db, &mut s, q), "54000");
    let q = "with recursive r(n) as (select 1 union all select n + 1 from r where n < 1000) select count(*) from r";
    assert_eq!(one(&db, &mut s, q), "1000");
}

fn big(db: &Db, s: &mut Session) {
    all(db, s, "create table big(id int primary key, s text)");
    all(db, s, "insert into big select g, repeat('x', 1000) from generate_series(1, 5000) g");
    all(db, s, "create table small(id int primary key)");
    all(db, s, "insert into small values (1), (2)");
}

#[test]
fn right_join_past_work_mem_is_a_clean_error() {
    let (db, mut s) = db();
    big(&db, &mut s);
    assert_eq!(code(&db, &mut s, "select count(big.s) from small right join big on small.id = big.id"), "54000");
    assert_eq!(code(&db, &mut s, "select count(big.s) from small full join big on small.id = big.id"), "54000");
    assert_eq!(one(&db, &mut s, "select count(big.id) from big right join small on small.id = big.id"), "2");
}

#[test]
fn subquery_cache_evicts_and_stays_correct() {
    let (db, mut s) = db();
    big(&db, &mut s);
    // 5000 distinct outer values, 1 kB each: more than the cache keeps
    let q = "select count(*) from big a where (select b.s || a.id from big b where b.id = a.id) = a.s || a.id";
    assert_eq!(one(&db, &mut s, q), "5000");
}

#[test]
fn window_rows_past_work_mem_are_a_clean_error() {
    let (db, mut s) = db();
    big(&db, &mut s);
    assert_eq!(code(&db, &mut s, "select s, count(*) over () from big"), "54000");
    assert_eq!(one(&db, &mut s, "select count(*) over () from small"), "2");
}

#[test]
fn equal_numeric_and_float_hash_together() {
    let (db, mut s) = db();
    all(&db, &mut s, "create table a(n numeric); create table b(f float8)");
    all(&db, &mut s, "insert into a select g + 0.5 from generate_series(1, 5000) g");
    all(&db, &mut s, "insert into b select g + 0.25 + 0.25 from generate_series(1, 5000) g");
    assert_eq!(one(&db, &mut s, "select count(*) from a join b on a.n = b.f"), "5000");
    assert_eq!(one(&db, &mut s, "select count(*) from a where n in (select f from b)"), "5000");
    // floats without a numeric twin still group apart
    assert_eq!(
        one(
            &db,
            &mut s,
            "select count(distinct x) from (select '1e-300'::float8 x union all select 0::float8 union all select '2e-300'::float8) v"
        ),
        "3"
    );
}

#[test]
fn changing_a_text_key_to_int_rekeys_its_rows() {
    let (db, mut s) = db();
    all(&db, &mut s, "create table t(id text primary key); insert into t values ('05'), ('5')");
    assert_eq!(code(&db, &mut s, "alter table t alter id type int"), "23505");
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "2");
    all(&db, &mut s, "create table u(id text primary key, v int); insert into u values ('07', 1), ('8', 2)");
    all(&db, &mut s, "alter table u alter id type int");
    assert_eq!(one(&db, &mut s, "select v from u where id = 7"), "1");
    assert_eq!(all(&db, &mut s, "select id from u order by id"), ["7", "8"]);
    assert_eq!(code(&db, &mut s, "insert into u values (7, 3)"), "23505");
}

#[test]
fn order_by_mixed_int_and_float_is_exact() {
    let (db, mut s) = db();
    // 2^53 + 1 is no float; through f64 it would tie with 2^53, and 2^53 + 0.5 too
    let q = "select g from generate_series(1, 4) g order by case g when 1 then 9007199254740993 \
             when 2 then 9007199254740992::float8 when 3 then 9007199254740991 else 9007199254740992.5::numeric end desc";
    assert_eq!(all(&db, &mut s, q), ["1", "4", "2", "3"]);
    // many mixed values sort without a panic
    let q = "select count(*) from (select g from generate_series(1, 9000) g \
             order by case g % 3 when 0 then g * 3 when 1 then g * 2.5::float8 else g * 1.5::numeric end) w";
    assert_eq!(one(&db, &mut s, q), "9000");
}

#[test]
fn dates_past_the_timestamp_range_error_or_compare_exactly() {
    let (db, mut s) = db();
    assert_eq!(code(&db, &mut s, "select '300000-01-01'::date::timestamp"), "22008");
    assert_eq!(code(&db, &mut s, "select '300000-01-01'::date + '10:00'::time"), "22008");
    assert_eq!(code(&db, &mut s, "select '300000-01-01'::date + interval '1 day'"), "22008");
    assert_eq!(one(&db, &mut s, "select '300000-01-01'::date > '2000-01-01'::timestamp"), "true");
    let q = "select count(distinct d) from (select '300000-01-01'::date d union all select '300001-01-01'::date \
             union all select '300000-01-01'::date) v";
    assert_eq!(one(&db, &mut s, q), "2");
    assert_eq!(one(&db, &mut s, "select '2000-01-01'::date::timestamp"), "2000-01-01 00:00:00");
}

#[test]
fn notify_from_a_released_savepoint_survives_a_later_rollback_to() {
    let db = Db::new();
    let (a, b) = (&mut Session::new(MAIN), &mut Session::new(MAIN));
    b.run(&db, "listen c", &[]).unwrap();
    a.run(
        &db,
        "begin; savepoint a; notify c, 'kept'; release a; savepoint b; notify c, 'dropped'; rollback to b; commit",
        &[],
    )
    .unwrap();
    assert_eq!(b.inbox.take().iter().map(|n| n.payload.as_str()).collect::<Vec<_>>(), ["kept"]);
}

#[test]
fn rollback_to_undoes_temp_table_changes() {
    let (db, mut s) = db();
    all(&db, &mut s, "create temp table tt(id int primary key)");
    all(&db, &mut s, "begin; insert into tt values (1); savepoint s; insert into tt values (2); rollback to s; commit");
    assert_eq!(all(&db, &mut s, "select id from tt order by id"), ["1"]);
    all(
        &db,
        &mut s,
        "begin; savepoint s; insert into tt values (3); release s; savepoint t; insert into tt values (4); rollback to t; commit",
    );
    assert_eq!(all(&db, &mut s, "select id from tt order by id"), ["1", "3"]);
    all(&db, &mut s, "begin; savepoint s; insert into tt values (5); rollback to s; insert into tt values (6); commit");
    assert_eq!(all(&db, &mut s, "select id from tt order by id"), ["1", "3", "6"]);
    // temporary tables first made inside the savepoint go with it
    let mut s2 = Session::new(MAIN);
    all(&db, &mut s2, "begin; savepoint s; create temp table t2(id int); insert into t2 values (1); rollback to s");
    assert_eq!(code(&db, &mut s2, "select * from t2"), "42P01");
    all(&db, &mut s2, "commit");
}

#[test]
fn execute_runs_in_its_own_slot() {
    let (db, mut s) = db();
    let spec = AgentSpec { can: Some(vec![Can::Admin]), max_concurrent: Some(1), ..Default::default() };
    db.create_agent("one", &spec).unwrap();
    s.agent = Some("one".into());
    all(&db, &mut s, "prepare p as select 41 + 1");
    assert_eq!(one(&db, &mut s, "execute p"), "42");
    assert_eq!(one(&db, &mut s, "execute p"), "42", "the slot is given back");
}
