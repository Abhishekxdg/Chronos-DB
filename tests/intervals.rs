//! Intervals multiplied and divided by numbers, summed and averaged, justified; OVERLAPS.
//! Postgres's answers are compared in tests/pgdiff.rs (`interval_arithmetic`); these run without one.

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
fn a_number_scales_an_interval() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let q = "select interval '1 hour' * 2.5, interval '1 month' * 1.5, 2 * interval '3 hours', interval '1 day' / 2";
    assert_eq!(first(&mut s, &db, q), "02:30:00|1 mon 15 days|06:00:00|12:00:00");
    assert_eq!(
        first(&mut s, &db, "select interval '1 year 2 months 3 days 04:05:06' / 2.5"),
        "5 mons 19 days 06:26:02.4"
    );
    assert_eq!(code(&mut s, &db, "select interval '1 hour' / 0"), "22012");
    assert_eq!(code(&mut s, &db, "select interval '1 day' * 1e10"), "22015");
    assert_eq!(code(&mut s, &db, "select 2 / interval '1 hour'"), "42883");
    assert_eq!(
        first(&mut s, &db, "select interval '0', interval '5', interval '5 days 3'"),
        "00:00:00|00:00:05|5 days 00:00:03"
    );
}

#[test]
fn intervals_sum_average_and_justify() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(&db, "create table t (d interval)", &[]).unwrap();
    s.run(&db, "insert into t values ('1 hour 30 minutes'), ('2 days'), ('45 minutes'), (NULL)", &[]).unwrap();
    assert_eq!(
        first(&mut s, &db, "select sum(d), min(d), max(d), count(d) from t"),
        "2 days 02:15:00|00:45:00|2 days|3"
    );
    assert_eq!(first(&mut s, &db, "select avg(d), pg_typeof(sum(d)) from t"), "16:45:00|interval");
    let q = "select justify_hours(interval '50 hours'), justify_days(interval '40 days'), justify_interval(interval '1 month 40 days 50 hours')";
    assert_eq!(first(&mut s, &db, q), "2 days 02:00:00|1 mon 10 days|2 mons 12 days 02:00:00");
}

#[test]
fn overlaps_puts_periods_in_order_and_follows_the_null_rules() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let q = "select (date '2024-03-15', date '2024-03-20') overlaps (date '2024-03-18', date '2024-03-25'), \
             (date '2024-03-15', date '2024-03-20') overlaps (date '2024-03-20', date '2024-03-25'), \
             (date '2024-03-20', date '2024-03-15') overlaps (date '2024-03-18', date '2024-03-25'), \
             (timestamp '2024-01-01 10:00', interval '2 hours') overlaps (timestamp '2024-01-01 11:00', interval '1 hour')";
    assert_eq!(first(&mut s, &db, q), "true|false|true|true");
    let q = "select (date '2024-03-15', null) overlaps (date '2024-03-18', date '2024-03-25') is null, \
             (date '2024-03-15', null) overlaps (date '2024-03-10', date '2024-03-25')";
    assert_eq!(first(&mut s, &db, q), "true|true");
    assert_eq!(code(&mut s, &db, "select (1, 2) overlaps (2, 3)"), "42883");
}
