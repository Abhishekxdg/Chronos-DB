//! Executor bugs from the full review: each test is what Postgres 17 does.

use chronos::sql::{Output, Value, run};
use chronos::{Db, MAIN};

fn sql(db: &Db, q: &str) -> Output {
    run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap()
}

fn code(db: &Db, q: &str) -> &'static str {
    run(db, MAIN, q, &[]).expect_err(q).code
}

/// Rows as text; NULL is "null".
fn rows(db: &Db, q: &str) -> Vec<String> {
    sql(db, q)
        .rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn setup(setup: &str) -> Db {
    let db = Db::new();
    run(&db, MAIN, setup, &[]).unwrap();
    db
}

#[test]
fn window_offsets_past_i64_dont_panic() {
    let db = setup("create table t (v bigint)");
    let frame = "over (order by x rows between current row and unbounded following) from generate_series(1, 3) x";
    sql(&db, &format!("insert into t select nth_value(x, 9223372036854775807) {frame}"));
    assert_eq!(rows(&db, "select count(*), count(v) from t"), ["3|0"]);
    sql(&db, "insert into t values (1)"); // the branch still takes writes
    assert_eq!(rows(&db, &format!("select nth_value(x, 2) {frame}")), ["2", "3", "null"]);
    assert_eq!(
        rows(
            &db,
            "select lag(x, -9223372036854775807, 0) over (order by x), lead(x, 9223372036854775807, 0) over (order by x) from generate_series(1, 2) x"
        ),
        ["0|0", "0|0"]
    );
}

#[test]
fn where_isnt_pushed_below_right_and_full_joins() {
    let db = setup(
        "create table a (id int, x int); create table c (id int);
                 insert into a values (1, 1); insert into c values (1), (2)",
    );
    assert_eq!(rows(&db, "select c.id from a right join c on a.id = c.id where a.x = 1"), ["1"]);
    assert_eq!(rows(&db, "select c.id from a right join c on a.id = c.id where a.id is null"), ["2"]);
    assert!(rows(&db, "select * from a right join c on a.id = c.id where false").is_empty());
    assert_eq!(rows(&db, "select c.id from a full join c on a.id = c.id where a.x = 1"), ["1"]);
    assert_eq!(
        rows(&db, "select c.id from a join a b on a.id = b.id right join c on c.id = a.id where b.x = 1"),
        ["1"]
    );
}

#[test]
fn on_conflict_counts_only_its_arbiter() {
    let db = setup("create table u (id int primary key, email text unique, n int); insert into u values (1, 'a', 0)");
    assert_eq!(code(&db, "insert into u values (1, 'b', 5) on conflict (email) do update set n = excluded.n"), "23505");
    assert_eq!(code(&db, "insert into u values (1, 'b', 5) on conflict (email) do nothing"), "23505");
    sql(&db, "insert into u values (2, 'a', 7) on conflict (email) do update set n = excluded.n");
    sql(&db, "insert into u values (1, 'z', 9) on conflict do nothing");
    assert_eq!(rows(&db, "select * from u"), ["1|a|7"]);
}

#[test]
fn http_rows_need_their_keys_canonical() {
    let db = setup("create table t (id int primary key, v int)");
    assert!(db.put(MAIN, "t/007", r#"{"v":1}"#).is_err());
    db.put(MAIN, "t/7", r#"{"v":1}"#).unwrap();
    assert_eq!(code(&db, "insert into t values (7, 2)"), "23505");
    let db2 = chronos::Db::new();
    run(&db2, MAIN, "create table p (a int, b text, primary key (a, b))", &[]).unwrap();
    sql(&db2, "insert into p values (1, 'x')");
    assert_eq!(rows(&db2, "select * from p"), ["1|x"]);
}

#[test]
fn non_ascii_text_keys_after_char_1() {
    let db = setup("create table t (id text primary key)");
    sql(&db, "insert into t values (chr(1) || 'é')");
    assert_eq!(code(&db, "insert into t values (chr(1) || 'é')"), "23505");
}

#[test]
fn set_null_respects_not_null() {
    let db = setup(
        "create table p (id int primary key);
                 create table c (id int primary key, pid int not null references p on delete set null);
                 insert into p values (1); insert into c values (1, 1)",
    );
    assert_eq!(code(&db, "delete from p"), "23502");
    assert_eq!(rows(&db, "select * from c"), ["1|1"]);
}

#[test]
fn numeric_unique_is_exact() {
    let db = setup("create table t (x numeric unique)");
    sql(&db, "insert into t values (1)");
    assert_eq!(code(&db, "insert into t values (1.0)"), "23505");
    sql(&db, "insert into t values (1.00000000000000000001), (1.00000000000000000002)");
}

#[test]
fn null_in_empty_subquery() {
    let db = Db::new();
    assert_eq!(
        rows(
            &db,
            "select null::int not in (select 1 where false), null::int in (select 1 where false), null::int not in (select 1)"
        ),
        ["true|false|null"]
    );
    assert_eq!(
        rows(&db, "select null::int <> all (select 1 where false), null::int = any (select 1 where false)"),
        ["true|false"]
    );
}

#[test]
fn negative_zero_and_nan_floats() {
    let db = setup("create table f (x float8); insert into f values ('-0'), ('0'), ('NaN'), ('NaN'), ('Infinity')");
    assert_eq!(
        rows(&db, "select '-0'::float8 = 0, '-0'::float8 = '0'::float8, 'NaN'::float8 = 'NaN'::float8"),
        ["true|true|true"]
    );
    assert_eq!(rows(&db, "select count(distinct x) from f"), ["3"]);
    assert_eq!(rows(&db, "select count(*) from f group by x order by x"), ["2", "1", "2"]);
    assert_eq!(rows(&db, "select x from f order by x desc limit 1"), ["NaN"]);
}

#[test]
fn date_and_interval_overflow() {
    let db = Db::new();
    assert_eq!(code(&db, "select '2024-01-01'::date + 4294967296"), "22008");
    assert_eq!(code(&db, "select '2024-01-01'::date + 2147483647"), "22008");
    assert_eq!(code(&db, "select '2024-01-01'::date - 2147483647"), "22008");
    assert_eq!(code(&db, "select interval '1 day' * 4294967297"), "22015");
    assert_eq!(code(&db, "select interval '2147483647 days' + interval '1 day'"), "22015");
    assert_eq!(rows(&db, "select '2024-01-01'::date + 31"), ["2024-02-01"]);
}

#[test]
fn second_index_column_range_is_exact() {
    let db = setup(
        "create table t (id int primary key, y int, x bigint); create index on t (y, x);
                 insert into t values (1, 0, 18014398509481986), (2, 0, 18014398509481985)",
    );
    assert_eq!(rows(&db, "select id from t where y >= 0 and x > 18014398509481985"), ["1"]);
    assert_eq!(rows(&db, "select id from t where y >= 0 and x <= 18014398509481985"), ["2"]);
}

#[test]
fn alter_sequence_keeps_bounds() {
    let db = setup("create sequence s minvalue 100 maxvalue 200 cycle; alter sequence s increment 2");
    assert_eq!(rows(&db, "select nextval('s'), nextval('s')"), ["100|102"]);
    assert_eq!(
        rows(&db, "select min_value, max_value, increment_by from pg_sequences where sequencename = 's'"),
        ["100|200|2"]
    );
}

#[test]
fn set_operations_sort_in_the_output_type() {
    let db = Db::new();
    let part = "select '10' union all select '9' union all select 9.5";
    let q = format!("{} order by 1", [part; 12].join(" union all "));
    let got = rows(&db, &q); // numeric, as Postgres reads the literals, and sorted as one type
    assert_eq!(got.len(), 36);
    let n: Vec<f64> = got.iter().map(|s| s.parse().unwrap()).collect();
    assert!(n.windows(2).all(|w| w[0] <= w[1]), "{got:?}");
    // a schemaless table's mixed values sort without panicking
    let db = Db::new();
    for i in 0..40 {
        db.put(
            MAIN,
            &format!("j/{i}"),
            &if i % 3 == 0 { format!(r#"{{"v":"{i}"}}"#) } else { format!(r#"{{"v":{i}.5}}"#) },
        )
        .unwrap();
    }
    assert_eq!(rows(&db, "select count(*) from (select v from j order by v) s"), ["40"]);
}
