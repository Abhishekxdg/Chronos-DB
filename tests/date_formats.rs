//! to_date and to_timestamp with a template, and make_date / make_time / make_timestamp[tz].
//! Postgres's answers are compared in tests/pgdiff.rs (`to_date_and_to_timestamp_read_a_template`,
//! `make_dates_and_times`); these run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn first(s: &mut Session, db: &Db, q: &str) -> String {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows[0].iter().map(Value::to_text).collect::<Vec<_>>().join("|")
}

fn code(s: &mut Session, db: &Db, q: &str) -> &'static str {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => e.code,
    }
}

#[test]
fn a_template_reads_a_date_and_time() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let want = [
        ("select to_date('2024-03-15', 'YYYY-MM-DD'), to_date('15/03/2024', 'DD/MM/YYYY')", "2024-03-15|2024-03-15"),
        ("select to_date('20240315', 'YYYYMMDD'), to_date('2024 3 5', 'YYYY MM DD')", "2024-03-15|2024-03-05"),
        (
            "select to_date('March 5 2024', 'Month DD YYYY'), to_date('mar 5 2024', 'mon dd yyyy')",
            "2024-03-05|2024-03-05",
        ),
        (
            "select to_date('24', 'YY'), to_date('70', 'YY'), to_date('2024-060', 'YYYY-DDD')",
            "2024-01-01|1970-01-01|2024-02-29",
        ),
        ("select to_timestamp('2024-03-15 10:30:45', 'YYYY-MM-DD HH24:MI:SS')", "2024-03-15 10:30:45+00"),
        ("select to_timestamp('2024-03-15 10:30 PM', 'YYYY-MM-DD HH:MI AM')", "2024-03-15 22:30:00+00"),
        ("select to_timestamp('2024-03-15 10:30:45.5', 'YYYY-MM-DD HH24:MI:SS.MS')", "2024-03-15 10:30:45.5+00"),
        ("select to_timestamp('2024-03-15 10:30 +05:30', 'YYYY-MM-DD HH24:MI TZH:TZM')", "2024-03-15 05:00:00+00"),
        // nothing given is year 0 (1 BC), January, the 1st
        ("select to_date('05', 'DD'), to_date('', 'YYYY')", "0001-01-05 BC|0001-01-01 BC"),
    ];
    for (q, out) in want {
        assert_eq!(first(&mut s, &db, q), out, "{q}");
    }
    assert_eq!(code(&mut s, &db, "select to_date('2024-02-30', 'YYYY-MM-DD')"), "22008");
    assert_eq!(code(&mut s, &db, "select to_date('abcd', 'YYYY')"), "22007");
    assert_eq!(code(&mut s, &db, "select to_timestamp('13:30 PM', 'HH12:MI AM')"), "22008");
    assert_eq!(code(&mut s, &db, "select to_date('2024 EST', 'YYYY TZ')"), "0A000");
}

#[test]
fn make_functions_build_dates_and_times() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let q = "select make_date(2024, 3, 15), make_time(10, 30, 45.5), make_timestamp(2024, 3, 15, 10, 30, 45.5), make_timestamptz(2024, 3, 15, 10, 30, 45, 'Asia/Kolkata')";
    assert_eq!(first(&mut s, &db, q), "2024-03-15|10:30:45.5|2024-03-15 10:30:45.5|2024-03-15 05:00:45+00");
    assert_eq!(first(&mut s, &db, "select make_date(-44, 3, 15)"), "0044-03-15 BC");
    assert_eq!(code(&mut s, &db, "select make_date(2023, 2, 29)"), "22008");
    assert_eq!(code(&mut s, &db, "select make_time(24, 0, 1)"), "22008");
}
