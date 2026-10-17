//! Regressions from the third review of the executor: old-form unique keys stay bounded, a
//! referenced key's type can't change, sliding window sums evaluate each argument once, numbers
//! hash and group as they compare, stored functions keep their enum arguments, and interval
//! negation, jsonb input and upper/lower stay in range.

use chronos::sql::Session;
use chronos::{Db, MAIN};

fn run(s: &mut Session, db: &Db, sql: &str) -> Vec<Vec<String>> {
    s.run(db, sql, &[])
        .unwrap_or_else(|e| panic!("{sql}: {}", e.message))
        .pop()
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(|v| v.to_text()).collect())
        .collect()
}

fn code(s: &mut Session, db: &Db, sql: &str) -> &'static str {
    match s.run(db, sql, &[]) {
        Ok(_) => "ok",
        Err(e) => e.code,
    }
}

/// 1. A 30-column unique key over numerics: one insert once built 2^30 old-form keys.
#[test]
fn a_wide_unique_numeric_key_inserts_quickly() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let cols: Vec<String> = (1..=30).map(|i| format!("c{i}")).collect();
    let defs: Vec<String> = cols.iter().map(|c| format!("{c} numeric")).collect();
    s.run(&db, &format!("create table t (id int primary key, {}, unique ({}))", defs.join(", "), cols.join(", ")), &[])
        .unwrap();
    let zeros = vec!["0"; 30].join(", ");
    let start = std::time::Instant::now();
    s.run(&db, &format!("insert into t values (1, {zeros})"), &[]).unwrap();
    assert_eq!(code(&mut s, &db, &format!("insert into t values (2, {zeros})")), "23505");
    assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
}

/// 2. The type of a key other tables reference can't change: their rows would keep '05'.
#[test]
fn a_referenced_keys_type_cant_change() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(&db, "create table p (id text primary key)", &[]).unwrap();
    s.run(&db, "create table c (id int primary key, p text references p)", &[]).unwrap();
    s.run(&db, "insert into p values ('05'); insert into c values (1, '05')", &[]).unwrap();
    assert_eq!(code(&mut s, &db, "alter table p alter column id type int"), "0A000");
    assert_eq!(code(&mut s, &db, "delete from p"), "23503");
    // unreferenced: the rows move, and their unique index follows
    s.run(&db, "create table q (id text primary key, u int unique)", &[]).unwrap();
    s.run(&db, "insert into q values ('05', 1)", &[]).unwrap();
    s.run(&db, "alter table q alter column id type int", &[]).unwrap();
    assert_eq!(code(&mut s, &db, "insert into q values (6, 1)"), "23505");
    assert_eq!(run(&mut s, &db, "select id from q where u = 1"), [["5"]]);
}

/// 3. A sliding sum takes back out the value it fed, not a second evaluation.
#[test]
fn a_sliding_sum_evaluates_its_argument_once() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(&db, "create table t (i int primary key); insert into t select g from generate_series(1, 50) g", &[])
        .unwrap();
    s.run(&db, "create sequence q", &[]).unwrap();
    let r = run(&mut s, &db, "select sum(nextval('q')) over (order by i rows 2 preceding) from t order by i");
    assert_eq!(r.len(), 50);
    // every row's nextval ran once: 50 values used
    assert_eq!(run(&mut s, &db, "select nextval('q')"), [["51"]]);
    // sum of the last three values fed, exactly
    assert_eq!(r[49][0], (48 + 49 + 50).to_string());
    let r = run(
        &mut s,
        &db,
        "select count(*) from (select i, x, sum(x) over (order by i rows 2 preceding) s, \
         x + coalesce(lag(x) over (order by i), 0) + coalesce(lag(x, 2) over (order by i), 0) e \
         from (select i, random() x from t) a) b where abs(s - e) > 1e-9",
    );
    assert_eq!(r, [["0"]]);
}

/// 4. Numbers hash and group as `=` compares them.
#[test]
fn numbers_group_as_they_compare() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(run(&mut s, &db, "select 1152921504606846976 = '1152921504606846976'::float8"), [["true"]]);
    s.run(&db, "create table n (a numeric); create table f (b float8); create table i (c bigint)", &[]).unwrap();
    s.run(
        &db,
        "insert into n values (1152921504606846976), (0.1); insert into f values ('1152921504606846976'), (0.1); \
        insert into i values (1152921504606846976)",
        &[],
    )
    .unwrap();
    // hash joins key both sides alike
    assert_eq!(run(&mut s, &db, "select count(*) from n join f on a = b"), [["2"]]);
    assert_eq!(run(&mut s, &db, "select count(*) from i join f on c = b"), [["1"]]);
}

/// 5, 6. A stored function with an enum argument keeps its signature after enum names are swept.
#[test]
fn a_functions_enum_argument_survives_a_sweep() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(&db, "create type mood as enum ('ok', 'sad')", &[]).unwrap();
    s.run(&db, "create function f(m mood) returns text language sql as $$ select m::text $$", &[]).unwrap();
    assert_eq!(run(&mut s, &db, "select f('ok')"), [["ok"]]);
    for i in 0..5000 {
        s.run(&db, &format!("select 'a'::no_such_type_{i}"), &[]).ok();
    }
    assert_eq!(run(&mut s, &db, "select f('sad')"), [["sad"]]);
    s.run(&db, "drop function f(mood)", &[]).unwrap();
}

/// 7. Negating the most negative interval part is 22015, not an overflow.
#[test]
fn interval_negation_overflow_is_an_error() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(code(&mut s, &db, "select timestamp '2000-01-01' - interval '-2147483648 days'"), "22015");
}

/// 8. An object's members count as three values each.
#[test]
fn jsonb_objects_count_their_members() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(run(&mut s, &db, r#"select '{"a":1,"b":[2]}'::jsonb ->> 'a'"#), [["1"]]);
}

/// 9. upper/lower check their result's bound before building it ('ɐ' is 2 bytes, its capital 3).
#[test]
fn upper_checks_before_growing() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(code(&mut s, &db, "select length(upper(repeat('ɐ', 400000000)))"), "54000");
    assert_eq!(run(&mut s, &db, "select upper('ab'), lower('AB')"), [["AB", "ab"]]);
}
