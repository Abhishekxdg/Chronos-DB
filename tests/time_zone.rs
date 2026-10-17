//! SET TIME ZONE: timestamptz shown and read in the session's zone, dates and plain timestamps
//! converted in it, current_date its date; zones the tz database doesn't have refused.

use chronos::sql::{Output, Session, Value};
use chronos::{Db, MAIN};

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn q(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    rows(&s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap())
}

fn one(db: &Db, s: &mut Session, sql: &str) -> String {
    q(db, s, sql).pop().unwrap()
}

fn fails(db: &Db, s: &mut Session, sql: &str) -> (&'static str, String) {
    let e = s.run(db, sql, &[]).expect_err(sql);
    (e.code, e.message)
}

fn ist() -> (Db, Session) {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    q(&db, &mut s, "set time zone 'Asia/Kolkata'");
    (db, s)
}

#[test]
fn utc_until_set() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    assert_eq!(one(&db, &mut s, "show timezone"), "UTC");
    assert_eq!(one(&db, &mut s, "select '2026-09-29 00:00+00'::timestamptz::text"), "2026-09-29 00:00:00+00");
    assert_eq!(one(&db, &mut s, "select '2026-09-28 20:00'::timestamptz::date::text"), "2026-09-28");
}

#[test]
fn timestamptz_is_shown_and_read_in_the_zone() {
    let (db, mut s) = ist();
    assert_eq!(one(&db, &mut s, "show time zone"), "Asia/Kolkata");
    assert_eq!(one(&db, &mut s, "select '2026-09-29 00:00+00'::timestamptz::text"), "2026-09-29 05:30:00+05:30");
    // text without a zone is the session's; with one, its own
    assert_eq!(one(&db, &mut s, "select '2026-09-29 10:00'::timestamptz = '2026-09-29 04:30Z'::timestamptz"), "true");
    assert_eq!(one(&db, &mut s, "select '2026-09-29 10:00-02'::timestamptz::text"), "2026-09-29 17:30:00+05:30");
    assert_eq!(
        one(&db, &mut s, "select to_char('2026-09-28 20:00Z'::timestamptz, 'YYYY-MM-DD HH24:MI')"),
        "2026-09-29 01:30"
    );
}

#[test]
fn dates_and_plain_timestamps_convert_in_the_zone() {
    let (db, mut s) = ist();
    // 20:00 UTC on the 28th is 01:30 on the 29th in India
    assert_eq!(one(&db, &mut s, "select '2026-09-28 20:00Z'::timestamptz::date::text"), "2026-09-29");
    assert_eq!(one(&db, &mut s, "select '2026-09-28 20:00Z'::timestamptz::timestamp::text"), "2026-09-29 01:30:00");
    assert_eq!(one(&db, &mut s, "select '2026-09-28 20:00Z'::timestamptz::time::text"), "01:30:00");
    assert_eq!(
        one(&db, &mut s, "select '2026-09-29 10:00'::timestamp::timestamptz::text"),
        "2026-09-29 10:00:00+05:30"
    );
    assert_eq!(one(&db, &mut s, "select date '2026-09-29'::timestamptz::text"), "2026-09-29 00:00:00+05:30");
    assert_eq!(
        one(&db, &mut s, "select date_trunc('day', '2026-09-28 20:00Z'::timestamptz)::text"),
        "2026-09-29 00:00:00+05:30"
    );
    assert_eq!(one(&db, &mut s, "select extract(hour from '2026-09-28 20:00Z'::timestamptz)"), "1");
    assert_eq!(one(&db, &mut s, "select extract(day from '2026-09-28 20:00Z'::timestamptz)"), "29");
    // epoch counts from 1970 UTC, whatever the zone
    // (a numeric, with its six decimals, as in Postgres 14+)
    assert_eq!(one(&db, &mut s, "select extract(epoch from '1970-01-01 00:00:10Z'::timestamptz)"), "10.000000");
    assert_eq!(one(&db, &mut s, "select '2026-09-29 00:30Z'::timestamptz - date '2026-09-29'"), "06:00:00");
}

#[test]
fn today_is_the_zones_date() {
    let (db, mut s) = ist();
    let today = |secs: i64| (secs + 19_800).div_euclid(86_400).to_string();
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let before = today(now());
    let got = one(&db, &mut s, "select current_date - date '1970-01-01'");
    assert!(got == before || got == today(now()), "{got} vs {before}");
    assert_eq!(one(&db, &mut s, "select current_date = now()::date"), "true");
    assert_eq!(one(&db, &mut s, "select localtimestamp::date = current_date"), "true");
}

#[test]
fn a_date_against_a_timestamptz_column_is_the_zones_midnight() {
    let (db, mut s) = ist();
    q(
        &db,
        &mut s,
        "create table visits (id int primary key, at timestamptz not null);
         insert into visits values (1, '2026-09-28 18:00Z'), (2, '2026-09-28 19:00Z'), (3, '2026-09-29 20:00Z')",
    );
    // in India: 23:30 on the 28th, 00:30 on the 29th, 01:30 on the 30th
    let today = "select id from visits where at >= date '2026-09-29' and at < date '2026-09-30' order by id";
    assert_eq!(q(&db, &mut s, today), ["2"]);
    assert_eq!(q(&db, &mut s, "select id from visits where at::date = '2026-09-29' order by id"), ["2"]);
    q(&db, &mut s, "create index visits_at on visits (at)");
    assert_eq!(q(&db, &mut s, today), ["2"]);
    let mut utc = Session::new(MAIN);
    assert_eq!(q(&db, &mut utc, today), ["3"]);
    // what's stored is the moment: another zone shows it as its own time
    assert_eq!(one(&db, &mut utc, "select at::text from visits where id = 2"), "2026-09-28 19:00:00+00");
    q(&db, &mut s, "insert into visits values (4, '2026-09-29 10:00')");
    assert_eq!(one(&db, &mut utc, "select at::text from visits where id = 4"), "2026-09-29 04:30:00+00");
}

#[test]
fn parallel_scans_read_in_the_zone() {
    let (db, mut s) = ist();
    // 40,000 visits a minute apart from 2026-09-28 00:00 UTC: enough rows to scan on every core
    q(
        &db,
        &mut s,
        "create table visits (id int primary key, at timestamptz not null);
         insert into visits select i, '2026-09-28 00:00Z'::timestamptz + (i::text || ' minutes')::interval from generate_series(0, 39999) i",
    );
    // the 29th in India: 2026-09-28 18:30 UTC (minute 1110) up to 2026-09-29 18:30 (minute 2550)
    assert_eq!(one(&db, &mut s, "select count(*) from visits where at::date = date '2026-09-29'"), "1440");
    assert_eq!(one(&db, &mut s, "select min(id) from visits where at::date = date '2026-09-29'"), "1110");
}

#[test]
fn the_ways_to_set_it() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    let cases = [
        ("set time zone interval '+05:30' hour to minute", "<+05:30>-05:30", "05:30:00+05:30"),
        ("set time zone 5.5", "<+05:30>-05:30", "05:30:00+05:30"),
        ("set time zone -3", "<-03>+03", "21:00:00-03"),
        ("set timezone = 'Etc/GMT-5'", "Etc/GMT-5", "05:00:00+05"),
        ("set timezone to 'UTC+2'", "UTC+2", "22:00:00-02"), // POSIX, as Postgres: + is west
        ("set session time zone 'Asia/Kathmandu'", "Asia/Kathmandu", "05:45:00+05:45"),
        ("set time zone utc", "UTC", "00:00:00+00"),
        ("set time zone 'Asia/Kolkata'; set time zone local", "UTC", "00:00:00+00"),
        ("set time zone 'Asia/Kolkata'; set time zone default", "UTC", "00:00:00+00"),
        ("set time zone 'Asia/Kolkata'; reset timezone", "UTC", "00:00:00+00"),
        ("set time zone 'Asia/Kolkata'; reset time zone", "UTC", "00:00:00+00"),
    ];
    for (set, name, shown) in cases {
        s.run(&db, set, &[]).unwrap_or_else(|e| panic!("{set}: {e}"));
        assert_eq!(one(&db, &mut s, "show timezone"), name, "{set}");
        let t = one(&db, &mut s, "select '2026-09-29 00:00Z'::timestamptz::text");
        assert!(t.ends_with(shown), "{set}: {t}");
    }
    // one session's zone isn't another's
    q(&db, &mut s, "set time zone 'Asia/Tokyo'");
    assert_eq!(one(&db, &mut Session::new(MAIN), "show timezone"), "UTC");
}

#[test]
fn zones_it_cant_honour_are_refused() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    // (a zone with daylight saving time is read from the tz database: tests/time_zones_dst.rs)
    for zone in ["Mars/Olympus_Mons", "Nowhere"] {
        let (code, message) = fails(&db, &mut s, &format!("set time zone '{zone}'"));
        assert_eq!(code, "22023", "{zone}");
        assert!(message.contains(zone), "{message}");
    }
    assert_eq!(fails(&db, &mut s, "set time zone interval '1 day'").0, "22023");
    assert_eq!(fails(&db, &mut s, "set time zone 20").0, "22023");
    assert_eq!(one(&db, &mut s, "show timezone"), "UTC", "unchanged");
}

#[test]
fn set_local_time_zone_ends_with_its_transaction() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    q(&db, &mut s, "set time zone 'Asia/Tokyo'");
    q(&db, &mut s, "begin");
    q(&db, &mut s, "set local time zone 'Asia/Kolkata'");
    assert_eq!(one(&db, &mut s, "show timezone"), "Asia/Kolkata");
    q(&db, &mut s, "commit");
    assert_eq!(one(&db, &mut s, "show timezone"), "Asia/Tokyo");
    // and after ROLLBACK
    q(&db, &mut s, "begin; set local time zone 'Asia/Dubai'; rollback");
    assert_eq!(one(&db, &mut s, "show timezone"), "Asia/Tokyo");
    // a plain SET after it in the same transaction is what stays
    q(&db, &mut s, "begin; set local time zone 'Asia/Dubai'; set time zone 'Asia/Seoul'; commit");
    assert_eq!(one(&db, &mut s, "show timezone"), "Asia/Seoul");
    // outside a transaction SET LOCAL does nothing (Postgres warns)
    q(&db, &mut s, "set local time zone 'Asia/Dubai'");
    assert_eq!(one(&db, &mut s, "show timezone"), "Asia/Seoul");
}

#[test]
fn offsets_out_of_range_are_refused_not_wrapped() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for sql in ["set time zone interval '1193046 hours'", "set time zone -10000000000", "set time zone 16"] {
        assert_eq!(fails(&db, &mut s, sql).0, "22023", "{sql}");
    }
    assert_eq!(one(&db, &mut s, "show timezone"), "UTC");
}
