//! Limits and edge cases in SQL functions that crashed the process, panicked or gave wrong
//! answers: each now gives the right result or a clean error. One test per finding.
//! This file's process runs with a 1 MB CHRONOS_WORK_MEM, so the spill test spills.

use chronos::sql::{self, Reach, Session, Value};
use chronos::{Db, MAIN};

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // set before anything reads it (it's read once per process)
    ONCE.call_once(|| unsafe { std::env::set_var("CHRONOS_WORK_MEM", "1MB") });
}

fn db() -> (Db, Session) {
    setup();
    (Db::new(), Session::new(MAIN))
}

fn try_one(db: &Db, s: &mut Session, q: &str) -> Result<String, (&'static str, String)> {
    match s.run(db, q, &[]) {
        Ok(out) => Ok(out.last().unwrap().rows.first().map_or(String::new(), |r| r[0].to_text())),
        Err(e) => Err((e.code, e.message)),
    }
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    try_one(db, s, q).unwrap_or_else(|(c, m)| panic!("{q}: {c} {m}"))
}

fn code(db: &Db, s: &mut Session, q: &str) -> &'static str {
    match try_one(db, s, q) {
        Ok(v) => panic!("{q}: should fail, gave {v:.100}"),
        Err((c, _)) => c,
    }
}

#[test]
fn regex_on_long_text_or_deep_pattern_does_not_overflow_the_stack() {
    let (db, mut s) = db();
    assert_eq!(one(&db, &mut s, "select repeat('a', 200000) ~ 'a*'"), "true");
    assert_eq!(one(&db, &mut s, "select repeat('a', 200000) ~ '^[a-z]+$'"), "true");
    assert_eq!(one(&db, &mut s, "select length(regexp_replace(repeat('ab', 100000), 'b*?a', 'x', 'g'))"), "100001");
    assert_eq!(code(&db, &mut s, "select 'x' ~ repeat('(', 100000)"), "2201B");
    // a group repeated over a long text: an answer or a clean error, not a crash
    match try_one(&db, &mut s, "select repeat('ab', 200000) ~ '^(ab)*$'") {
        Ok(v) => assert_eq!(v, "true"),
        Err((c, _)) => assert_eq!(c, "54001"),
    }
    assert_eq!(code(&db, &mut s, "select repeat('a', 100000) ~ repeat('a', 100000)"), "54001");
}

#[test]
fn regex_with_many_matches_stops_at_the_statement_timeout() {
    let (db, mut s) = db();
    one(&db, &mut s, "set statement_timeout = 10");
    assert_eq!(code(&db, &mut s, "select length(regexp_replace(repeat('a', 5000000), 'a', 'b', 'g'))"), "57014");
}

#[test]
fn regex_class_without_its_close_is_an_error() {
    let (db, mut s) = db();
    assert_eq!(code(&db, &mut s, "select 'a' ~ '[[:]'"), "2201B");
    assert_eq!(code(&db, &mut s, "select 'a' ~ '[[:alpha]'"), "2201B");
    assert_eq!(one(&db, &mut s, "select 'a' ~ '^[[:alpha:]]$'"), "true");
}

#[test]
fn big_tsqueries_stay_shallow_or_are_refused() {
    let (db, mut s) = db();
    let q = one(&db, &mut s, "select plainto_tsquery(repeat('cat ', 200000))");
    assert!(q.starts_with("'cat' & 'cat'"), "{q:.40}");
    // printed and read back: a long & chain is parsed shallow too
    assert_eq!(one(&db, &mut s, "select to_tsvector('cat') @@ plainto_tsquery(repeat('cat ', 200000))"), "true");
    assert_eq!(
        one(&db, &mut s, "select to_tsvector('cat dog') @@ websearch_to_tsquery(repeat('cat or dog ', 50000))"),
        "true"
    );
    assert_eq!(code(&db, &mut s, "select to_tsquery(repeat('!', 200000) || 'a')"), "42601");
    assert_eq!(code(&db, &mut s, "select to_tsquery(repeat('(', 200000) || 'a')"), "42601");
    assert_eq!(code(&db, &mut s, "select phraseto_tsquery(repeat('cat ', 5000))"), "54000");
    assert_eq!(code(&db, &mut s, "select to_tsquery(repeat('cat <-> ', 5000) || 'cat')"), "42601");
    assert_eq!(one(&db, &mut s, "select to_tsquery('fat & (rat | !cat)')"), "'fat' & ( 'rat' | !'cat' )");
}

#[test]
fn phrase_distances_add_up_without_overflow() {
    let (db, mut s) = db();
    let q = "select to_tsquery('the <16383> the <16383> the <16383> the <16383> the <16383> x')";
    assert_eq!(one(&db, &mut s, q), "'x'");
    // lexemes without positions: Postgres 16's TS_phrase_execute can't place them in a phrase, so no match
    let q = "select 'b c d e f'::tsvector @@ to_tsquery('simple', 'b <16383> c <16383> d <16383> e <16383> f')";
    assert_eq!(one(&db, &mut s, q), "false");
}

#[test]
fn strings_past_a_gigabyte_are_refused() {
    let (db, mut s) = db();
    for q in [
        "select repeat('x', 200000000000)",
        "select repeat('x', 4611686018427387904)",
        "select repeat('xx', 600000000)",
        "select format('%*s', 100000000000, 'x')",
        "select format('%-100000000000s', 'x')",
        "select lpad('x', 9000000000000)",
        "select rpad('x', 9000000000000, 'ab')",
    ] {
        assert_eq!(code(&db, &mut s, q), "54000", "{q}");
    }
    assert_eq!(one(&db, &mut s, "select lpad('x', 4, 'ab') || repeat('y', 2) || format('%3s', 'z')"), "abaxyy  z");
}

#[test]
fn to_char_of_nan_and_infinity() {
    let (db, mut s) = db();
    assert_eq!(one(&db, &mut s, "select to_char('NaN'::float8, '999.99')"), " NaN");
    assert_eq!(one(&db, &mut s, "select to_char('Infinity'::float8, '999.99')"), " ###.##");
    assert_eq!(one(&db, &mut s, "select to_char(1.5::float8, '999.99')"), "   1.50");
}

#[test]
fn odd_length_hex_geometry_is_an_error() {
    let (db, mut s) = db();
    assert!(try_one(&db, &mut s, "select '0101000000A'::geometry").is_err());
    assert!(try_one(&db, &mut s, "select st_astext('0101000000000000000000F03F0000000000000040'::geometry)").is_ok());
}

#[test]
fn timestamps_with_odd_offsets_are_errors() {
    let (db, mut s) = db();
    assert_eq!(code(&db, &mut s, "select '2024-01-01 00:00+1é1'::timestamptz"), "22007");
    assert_eq!(code(&db, &mut s, "select '2024-01-01 00:00+999999999999999'::timestamptz"), "22009");
    assert_eq!(code(&db, &mut s, "select '2024-01-01 00:00+16'::timestamptz"), "22009");
    assert_eq!(one(&db, &mut s, "select '2024-01-01 02:00+0130'::timestamptz"), "2024-01-01 00:30:00+00");
}

#[test]
fn dates_and_intervals_out_of_range_are_errors() {
    let (db, mut s) = db();
    for q in [
        "select '9999999999-01-01'::date",
        "select '9223372036854775807-01-01'::date",
        "select '5874898-01-01'::date",
        "select '200000000-01-01'::timestamp",
        "select '9223372036854775807 hours'::interval",
        "select '2147483647 days 1 day'::interval",
        "select '1e300 seconds'::interval",
        "select '2147483647 months 1 month'::interval",
    ] {
        assert!(try_one(&db, &mut s, q).is_err(), "{q}");
    }
    assert_eq!(one(&db, &mut s, "select '5874897-12-31'::date"), "5874897-12-31");
    // two far timestamps, nearly an i64 of microseconds apart (a year before 1 BC is written with BC, as in
    // Postgres: a year with a minus sign is refused)
    let d = one(&db, &mut s, "select '290000-01-01'::timestamp - '0001-01-01 BC'::timestamp");
    assert_eq!(d, "105920325 days");
    assert!(try_one(&db, &mut s, "select '-200000-01-01'::timestamp").is_err());
}

#[test]
fn numeric_min_mantissa_divides_without_panic() {
    let (db, mut s) = db();
    let min = "(-9223372036854775808::numeric * 18446744073709551616::numeric)";
    assert_eq!(one(&db, &mut s, &format!("select {min}")), "-170141183460469231731687303715884105728");
    assert_eq!(one(&db, &mut s, &format!("select {min} % -1")), "0");
    // (numeric has no 128-bit bound now: its negation is a numeric too, 2^127, at no places, as Postgres divides it)
    assert_eq!(one(&db, &mut s, &format!("select {min} / -1")), "170141183460469231731687303715884105728");
    one(&db, &mut s, "create table n (x numeric(38, 0))");
    assert_eq!(code(&db, &mut s, &format!("insert into n values ({min})")), "22003");
}

#[test]
fn tiny_negative_numerics_keep_their_sign() {
    let (db, mut s) = db();
    assert!(
        one(&db, &mut s, "select '-0.123456789012345678901234567890123456789'::numeric").starts_with("-0.1234567890")
    );
    assert!(one(&db, &mut s, "select '-1.5e-38'::numeric").starts_with("-0.0000"));
    assert_eq!(one(&db, &mut s, "select '-0.123456789012345678901234567890123456789'::numeric < 0"), "true");
}

#[test]
fn sql_function_parameter_out_of_range_is_an_error() {
    let (db, mut s) = db();
    for (f, n) in [("f", 5), ("g", 0)] {
        // refused when it's made, or when it's called
        let made =
            try_one(&db, &mut s, &format!("create function {f}(a int) returns int language sql as 'select ${n}'"));
        let e = made.and_then(|_| try_one(&db, &mut s, &format!("select {f}(1)"))).expect_err("no such parameter");
        assert_eq!(e.0, "42P02", "{}", e.1);
    }
    one(&db, &mut s, "create function h(a int) returns int language sql as 'select $1 + 1'");
    assert_eq!(one(&db, &mut s, "select h(41)"), "42");
    // quoted, "$1" is a column's name, as in Postgres (42703: there is none here)
    let made = try_one(&db, &mut s, "create function hq(a int) returns int language sql as 'select \"$1\" + 1'");
    let e = made.and_then(|_| try_one(&db, &mut s, "select hq(41)")).expect_err("no column \"$1\"");
    assert_eq!(e.0, "42703", "{}", e.1);
}

#[test]
fn copy_reads_a_long_record_in_small_pieces_and_stops_on_timeout() {
    let (db, mut s) = db();
    one(&db, &mut s, "create table t (id int primary key, body text)");
    let stmt = sql::parse("copy t from stdin csv").unwrap().pop().unwrap();
    let c = s.copy_in(&db, &stmt).unwrap();
    // one 4 MB record in 64-byte pieces: read once, not again with each piece
    let t = std::time::Instant::now();
    let data = format!("1,\"{}\"\n", "x".repeat(4 << 20));
    let mut pieces: Vec<Vec<u8>> = data.as_bytes().chunks(64).map(<[u8]>::to_vec).rev().collect();
    c.run(&mut s, &db, Box::new(move || Ok(pieces.pop()))).unwrap();
    assert!(t.elapsed() < std::time::Duration::from_secs(20), "{:?}", t.elapsed());
    assert_eq!(one(&db, &mut s, "select length(body) from t"), (4 << 20).to_string());
    // a statement_timeout stops a COPY that's still reading
    one(&db, &mut s, "set statement_timeout = 1");
    let c = s.copy_in(&db, &stmt).unwrap();
    let next = || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        Ok(Some(b"2,y\n".to_vec()))
    };
    let e = c.run(&mut s, &db, Box::new(next)).expect_err("past its timeout");
    assert_eq!(e.code, "57014");
}

#[test]
fn deeply_nested_jsonb_is_refused_not_a_crash() {
    let (db, mut s) = db();
    one(
        &db,
        &mut s,
        "create function deep(n int) returns int language plpgsql as $$
         declare j jsonb := '[]';
         begin
           for i in 1..n loop j := jsonb_build_array(j); end loop;
           return 1;
         end $$",
    );
    assert_eq!(one(&db, &mut s, "select deep(50)"), "1");
    assert_eq!(code(&db, &mut s, "select deep(50000)"), "54000");
    one(
        &db,
        &mut s,
        "create function deep_obj(n int) returns int language plpgsql as $$
         declare j jsonb := '{}';
         begin
           for i in 1..n loop j := jsonb_set(jsonb_build_object('a', 1), '{b}', j); end loop;
           return 1;
         end $$",
    );
    assert_eq!(code(&db, &mut s, "select deep_obj(50000)"), "54000");
}

#[test]
fn safe_mode_counts_sequence_and_notify_calls_as_writes() {
    let access = |q: &str| sql::parse(q).unwrap().pop().unwrap().access();
    for q in [
        "select setval('orders_id_seq', 1)",
        "select nextval('orders_id_seq')",
        "select * from t where id = nextval('s')",
        "select (select setval('s', 1, false))",
        "select pg_notify('c', 'x')",
        "notify c, 'x'",
    ] {
        assert_eq!(access(q), Reach::Write, "{q}");
    }
    assert_eq!(access("select currval('s'), lastval()"), Reach::Read);
    assert_eq!(access("listen c"), Reach::Read);
}

#[test]
fn spill_files_are_sealed_and_leave_nothing() {
    setup();
    let dir = std::env::temp_dir().join(format!("chronos-review-spill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open_encrypted(&dir, &[7; 32]).unwrap();
    let q = |sql: &str| sql::run(&db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    q("create table t (id int primary key, s text)");
    q("insert into t select i, 'secret-' || i from generate_series(1, 60000) i");
    let spilled = || db.counters().into_iter().find(|(n, _)| *n == "spilled_bytes").map_or(0, |(_, v)| v);
    let before = spilled();
    let o = q("select s from t order by s desc");
    assert!(spilled() > before, "60,000 rows don't fit in 1 MB: it spilled");
    assert_eq!(o.rows.len(), 60_000);
    assert_eq!(o.rows[0][0], Value::Text("secret-9999".into()));
    let names: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(&format!("sonos-spill-{}-", std::process::id())))
        .collect();
    assert!(names.is_empty(), "{names:?}");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
