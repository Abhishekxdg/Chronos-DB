//! date_trunc(unit, interval), and of a time of day, which Postgres cuts as the interval it casts to: Django
//! truncates TimeFields (`DATE_TRUNC('hour', t)::time`) and DurationFields so.

use chronos::sql::Session;
use chronos::{Db, MAIN};

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows[0][0].to_text()
}

#[test]
fn an_interval_is_cut_to_its_unit() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let i = "'2345 years 7 months 12 days 13:47:29.123456'::interval";
    for (unit, want) in [
        ("microseconds", "2345 years 7 mons 12 days 13:47:29.123456"),
        ("milliseconds", "2345 years 7 mons 12 days 13:47:29.123"),
        ("second", "2345 years 7 mons 12 days 13:47:29"),
        ("minute", "2345 years 7 mons 12 days 13:47:00"),
        ("hour", "2345 years 7 mons 12 days 13:00:00"),
        ("day", "2345 years 7 mons 12 days"),
        ("month", "2345 years 7 mons"),
        ("quarter", "2345 years 6 mons"),
        ("year", "2345 years"),
        ("decade", "2340 years"),
        ("century", "2300 years"),
        ("millennium", "2000 years"),
    ] {
        assert_eq!(one(&db, &mut s, &format!("SELECT date_trunc('{unit}', {i})")), want, "{unit}");
    }
    // a negative one toward zero
    assert_eq!(one(&db, &mut s, "SELECT date_trunc('hour', '-1 day -13:47:29'::interval)"), "-1 days -13:00:00");
    let e = s.run(&db, "SELECT date_trunc('week', '1 day'::interval)", &[]).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("0A000", "unit \"week\" not supported for type interval"));
    assert_eq!(s.run(&db, "SELECT date_trunc('nope', '1 day'::interval)", &[]).unwrap_err().code, "22023");
    // the zoned form is a timestamptz's alone
    let e = s.run(&db, "SELECT date_trunc('hour', '1 day 02:30'::interval, 'UTC')", &[]).unwrap_err();
    assert_eq!(e.code, "42883");
}

#[test]
fn a_time_is_cut_as_an_interval() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(one(&db, &mut s, "SELECT date_trunc('hour', '13:47:29.5'::time)"), "13:00:00");
    assert_eq!(one(&db, &mut s, "SELECT pg_typeof(date_trunc('hour', '13:47:29.5'::time))::text"), "interval");
    assert_eq!(one(&db, &mut s, "SELECT date_trunc('second', '13:47:29.5'::time)::time"), "13:47:29");
    in_a_table(&db, &mut s);
}

fn in_a_table(db: &Db, s: &mut Session) {
    s.run(db, "CREATE TABLE t (at time, took interval); INSERT INTO t VALUES ('08:30:15', '1 day 02:03:04.5')", &[])
        .unwrap();
    assert_eq!(one(db, s, "SELECT DATE_TRUNC('minute', at)::time FROM t"), "08:30:00");
    assert_eq!(one(db, s, "SELECT EXTRACT(second FROM DATE_TRUNC('second', took)) FROM t"), "4.000000");
}
