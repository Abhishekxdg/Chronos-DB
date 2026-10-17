//! Regressions from the second review of SQL functions, COPY and spill files: untrusted SQL that
//! could allocate without bound, overflow the stack, spin past a lost client or run for hours now
//! gets a clean error. One test per finding; each trigger would crash or hang without its fix.
//! This file's process runs with a 256 kB CHRONOS_WORK_MEM, so a few thousand rows spill.

use chronos::sql::{self, Session};
use chronos::{Db, MAIN};
use std::sync::Arc;

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // set before anything reads it (it's read once per process)
    ONCE.call_once(|| unsafe { std::env::set_var("CHRONOS_WORK_MEM", "256kB") });
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
    try_one(db, s, q).unwrap_or_else(|(c, m)| panic!("{q:.200}: {c} {m}"))
}

fn code(db: &Db, s: &mut Session, q: &str) -> &'static str {
    match try_one(db, s, q) {
        Ok(v) => panic!("{q:.200}: should fail, gave {v:.100}"),
        Err((c, _)) => c,
    }
}

/// A CSV record of a gigabyte of commas and no newline: refused at the first field past the
/// table's columns, not gathered as a billion fields (32 GB).
#[test]
fn copy_refuses_extra_fields_before_the_newline() {
    let (db, mut s) = db();
    one(&db, &mut s, "create table t (a int, b text)");
    let stmt = sql::parse("copy t from stdin (format csv)").unwrap().pop().unwrap();
    let c = s.copy_in(&db, &stmt).unwrap();
    let mut left = 1024;
    let next = move || {
        left -= 1;
        assert!(left > 0, "still reading after 1 GB");
        Ok(Some(vec![b','; 1 << 20]))
    };
    let e = c.run(&mut s, &db, Box::new(next)).expect_err("a record with more fields than columns is refused");
    assert_eq!(e.code, "22P04", "{}", e.message);
    // a header's extra fields are read and dropped; records of the right width still go in
    let stmt = sql::parse("copy t from stdin (format csv, header)").unwrap().pop().unwrap();
    let c = s.copy_in(&db, &stmt).unwrap();
    let mut data = Some(b"a,b,c,d\n1,x\n2,y\n".to_vec());
    c.run(&mut s, &db, Box::new(move || Ok(data.take()))).unwrap();
    assert_eq!(one(&db, &mut s, "select string_agg(b, '' order by a) from t"), "xy");
}

/// Functions whose output outgrows their input (replace, regexp_replace, string_to_array,
/// concat_ws, regexp_split_to_array, to_char, similar_to_escape) are refused past 1 GB as they
/// build, not after allocating tens of gigabytes.
#[test]
fn growing_string_and_array_functions_are_capped() {
    let (db, mut s) = db();
    for q in [
        "select length(replace(repeat('a', 100000), 'a', repeat('b', 100000)))",
        r"select length(regexp_replace(repeat('a', 100000), '.', repeat('\&', 50000), 'g'))",
        "select cardinality(string_to_array(repeat('a', 500000000), null))",
        "select cardinality(string_to_array(repeat(',', 500000000), ','))",
        "select length(concat_ws(repeat('x', 200000000), 1, 2, 3, 4, 5, 6, 7, 8, 9, 10))",
        "select length(concat(repeat('x', 600000000), repeat('y', 600000000)))",
        "select cardinality(regexp_split_to_array(repeat('a', 30000000), ''))",
        "select length(to_char(now(), repeat('J', 100000000)))",
        "select length(to_char(1, repeat('9', 100000000)))",
    ] {
        assert_eq!(code(&db, &mut s, q), "54000", "{q}");
    }
    assert_eq!(code(&db, &mut s, "select 'a' ~ repeat('a', 2000000)"), "2201B");
    // ordinary sizes still work
    assert_eq!(one(&db, &mut s, "select replace('abcabc', 'b', 'XY')"), "aXYcaXYc");
    assert_eq!(one(&db, &mut s, r"select regexp_replace('abc', '(b)', '[\1\&]', 'g')"), "a[bb]c");
    assert_eq!(one(&db, &mut s, "select string_to_array('abc', null)"), "{a,b,c}");
    assert_eq!(one(&db, &mut s, "select concat_ws('-', 'a', null, 'b')"), "a-b");
    assert_eq!(one(&db, &mut s, "select regexp_split_to_array('a1b22c', '[0-9]+')"), "{a,b,c}");
    assert_eq!(one(&db, &mut s, "select substr('héllo', 2, 3) || left('héllo', -2) || right('héllo', 2)"), "éllhéllo");
}

/// WHEN OTHERS doesn't catch the client being gone (08006): the loop stops at once instead of
/// spinning until the statement_timeout (forever without one).
#[test]
fn when_others_does_not_catch_a_lost_connection() {
    let (db, mut s) = db();
    one(
        &db,
        &mut s,
        "create function spin() returns int language plpgsql as $$
         begin
           for i in 1..100000000 loop
             begin
               perform count(*) from generate_series(1, 100000);
             exception when others then null;
             end;
           end loop;
           return 0;
         end $$",
    );
    one(&db, &mut s, "set statement_timeout = 20000");
    s.gone = Some(Arc::new(|| true));
    let t = std::time::Instant::now();
    assert_eq!(code(&db, &mut s, "select spin()"), "08006");
    assert!(t.elapsed() < std::time::Duration::from_secs(10), "{:?}", t.elapsed());
}

/// Functions nested 40 deep, each evaluating an expression nested 381 deep, run out of a 16 MB
/// server thread's stack without a stack check: now a clean 54001. (Measured on the x86-64 test
/// VM, release build, with the check disabled: f(39) aborts with a stack overflow at this width
/// and at 300; f(20) runs. With it, the check refuses once less than 4 MB is left.)
// a debug build's frames are several times larger, so one call near the parser's limit fills
// the thread: there the check refuses even shallow nesting (it still never crashes)
#[cfg_attr(debug_assertions, ignore = "measures release-build stack use; the VM runs it in release")]
#[test]
fn nested_function_calls_check_the_stack_left() {
    let h = std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(|| {
            let (db, mut s) = db();
            let body = format!("f(n - 1){}", " + 0".repeat(380));
            one(
                &db,
                &mut s,
                &format!(
                    "create function f(n int) returns int language plpgsql as $$
                     begin if n <= 0 then return 0; end if; return {body}; end $$"
                ),
            );
            assert_eq!(code(&db, &mut s, "select f(39)"), "54001");
            assert_eq!(one(&db, &mut s, "select f(3)"), "0");
        })
        .unwrap();
    h.join().unwrap();
}

/// ts_rank over many operands, each with many positions, is quadratic (hours here): it stops
/// at the statement_timeout. And a tsquery made from millions of words is refused.
#[test]
fn ts_rank_stops_at_the_timeout_and_tsqueries_are_bounded() {
    let (db, mut s) = db();
    let positions: String = (1..=256).map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let vector: String = (0..2000).map(|i| format!("w{i}:{positions}")).collect::<Vec<_>>().join(" ");
    let query: String = (0..2000).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" & ");
    one(&db, &mut s, "set statement_timeout = 500");
    let t = std::time::Instant::now();
    let q = format!("select ts_rank('{vector}'::tsvector, '{query}'::tsquery)");
    assert_eq!(code(&db, &mut s, &q), "57014");
    assert!(t.elapsed() < std::time::Duration::from_secs(20), "{:?}", t.elapsed());
    one(&db, &mut s, "set statement_timeout = 0");
    assert_eq!(code(&db, &mut s, "select plainto_tsquery('simple', repeat('cat ', 5000000))"), "54000");
    assert_eq!(
        one(&db, &mut s, "select ts_rank(to_tsvector('fat cats ate rats'), to_tsquery('cat & rat')) > 0"),
        "true"
    );
}

/// Array bounds near i64::MAX: an error (Postgres's bounds are int4), not an overflow. Postgres 16's array_in
/// answers them: past int4 the bounds don't match the elements (22P02), and an upper bound read past int4 is less
/// than the lower (2202E).
#[test]
fn array_bounds_past_int4_are_errors() {
    let (db, mut s) = db();
    let big = "'[9223372036854775807:9223372036854775807]={1,2}'";
    assert_eq!(code(&db, &mut s, &format!("select array_upper({big}, 1)")), "22P02");
    assert_eq!(code(&db, &mut s, &format!("select array_lower({big}, 1)")), "22P02");
    assert_eq!(code(&db, &mut s, "select array_upper('[2147483647:2147483648]={1,2}', 1)"), "2202E");
    // (Postgres 16 refuses a lower bound whose bound plus length leaves int4: "array lower bound is too large")
    assert_eq!(code(&db, &mut s, "select array_upper('[2147483646:2147483647]={1,2}', 1)"), "54000");
    assert_eq!(one(&db, &mut s, "select array_upper('[-3:-2]={1,2}', 1)"), "-2");
    // a subscript far from the lower bound: out of range, so null
    let low = "'[-9223372036854775807:-9223372036854775807]={1}'::text[]";
    assert_eq!(
        one(&db, &mut s, &format!("select (select a[9223372036854775807] from (select {low} a) x) is null")),
        "true"
    );
}

/// Bulk statements' run files are made new with random names, readable by this user only: a
/// link planted where a predictable name would be isn't followed and written through.
#[test]
fn bulk_run_files_are_new_private_and_unpredictable() {
    setup();
    let dir = std::env::temp_dir().join(format!("chronos-review2-funcs2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let spill = dir.join("spill");
    std::fs::create_dir_all(&spill).unwrap();
    let target = dir.join("victim");
    std::fs::write(&target, "keep").unwrap();
    for n in 0..2000 {
        std::os::unix::fs::symlink(&target, spill.join(format!("run-{}-{n}", std::process::id()))).unwrap();
    }
    sql::run(&db, MAIN, "create table t (id int primary key, body text)", &[]).unwrap();
    sql::run(&db, MAIN, "insert into t select i, repeat('x', 200) from generate_series(1, 6000) i", &[]).unwrap();
    let out = sql::run(&db, MAIN, "select count(*) from t", &[]).unwrap();
    assert_eq!(out.last().unwrap().rows[0][0].to_text(), "6000");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep", "a planted link was written through");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// to_char of a date past about 294,276 AD: an error, not microseconds wrapped around.
#[test]
fn to_char_of_a_far_date_is_an_error() {
    let (db, mut s) = db();
    assert_eq!(code(&db, &mut s, "select to_char('5874897-12-31'::date, 'YYYY')"), "22008");
    assert_eq!(one(&db, &mut s, "select to_char('2024-02-29'::date, 'YYYY-MM-DD')"), "2024-02-29");
}
