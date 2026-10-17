//! Named time zones with daylight saving time, read from the system's tz database (src/sql/tz.rs): timestamptz shown
//! and read in them, `AT TIME ZONE`, `date_trunc(unit, ts, zone)`, `generate_series(.., zone)`, intervals that count days
//! on the zone's calendar, abbreviations (`EST`, `PST`), and the pg_timezone_* views. Postgres's answers are compared in
//! tests/pgdiff.rs (`time_zones_with_daylight_saving_time`, ...); these run without one, and are skipped where the
//! machine has no tz database.

fn have_tzdata() -> bool {
    std::path::Path::new("/usr/share/zoneinfo/America/New_York").exists()
}

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

/// What a query gives back, a row a string, its columns joined with `|` and booleans as `t`/`f`
/// the way psql prints them.
fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    let text = |v: &Value| match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Value::Bytea(b) => format!("\\x{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        v => v.to_text(),
    };
    out.last().unwrap().rows.iter().map(|r| r.iter().map(text).collect::<Vec<_>>().join("|")).collect()
}

/// Statements run in order in one session: a query's rows, or the SQLSTATE of the statement
/// Postgres refuses.
fn script(steps: &[(&str, Result<&[&str], &str>)]) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, want) in steps {
        match want {
            Ok(rows_want) => assert_eq!(&rows(&mut s, &db, q), rows_want, "{q}"),
            Err(code) => {
                let e = s.run(&db, q, &[]).expect_err(q);
                assert_eq!(&e.code, code, "{q}: {}", e.message);
            }
        }
    }
}

#[test]
fn dst_zones_show_read_and_convert_as_postgres_does() {
    if !have_tzdata() {
        return;
    }
    script(&[
        (r"set time zone 'America/New_York'", Ok(&[])),
        (r"show timezone", Ok(&[r"America/New_York"])),
        (r"select timestamptz '2024-03-15 12:00:00+00'", Ok(&[r"2024-03-15 08:00:00-04"])),
        (r"select timestamptz '2024-01-15 12:00:00+00'", Ok(&[r"2024-01-15 07:00:00-05"])),
        (
            r"select timestamptz '2024-03-10 06:59:59+00', timestamptz '2024-03-10 07:00:00+00'",
            Ok(&[r"2024-03-10 01:59:59-05|2024-03-10 03:00:00-04"]),
        ),
        (
            r"select timestamptz '2024-11-03 05:59:59+00', timestamptz '2024-11-03 06:00:00+00'",
            Ok(&[r"2024-11-03 01:59:59-04|2024-11-03 01:00:00-05"]),
        ),
        (
            r"select timestamptz '2024-03-10 02:30:00', timestamptz '2024-03-10 03:30:00'",
            Ok(&[r"2024-03-10 03:30:00-04|2024-03-10 03:30:00-04"]),
        ),
        (r"select timestamptz '2024-11-03 01:30:00'", Ok(&[r"2024-11-03 01:30:00-05"])),
        (
            r"select timestamptz '2024-03-09 12:00' + interval '1 day', timestamptz '2024-03-09 12:00' + interval '24 hours'",
            Ok(&[r"2024-03-10 12:00:00-04|2024-03-10 13:00:00-04"]),
        ),
        (r"select timestamptz '2024-03-11 00:00' - timestamptz '2024-03-10 00:00'", Ok(&[r"23:00:00"])),
        (
            r"select timestamptz '2024-11-02 12:00' + interval '1 day', timestamptz '2024-11-02 12:00' + interval '1 month', timestamptz '2024-03-10 12:00' - interval '1 day'",
            Ok(&[r"2024-11-03 12:00:00-05|2024-12-02 12:00:00-05|2024-03-09 12:00:00-05"]),
        ),
        (
            r"select (timestamp '2024-03-10 02:30:00' at time zone 'America/New_York')::text, (timestamp '2024-11-03 01:30:00' at time zone 'America/New_York')::text",
            Ok(&[r"2024-03-10 03:30:00-04|2024-11-03 01:30:00-05"]),
        ),
        (
            r"select timestamptz '2024-03-15 12:00+00' at time zone 'Asia/Tokyo', timestamptz '2024-07-15 12:00+00' at time zone 'Europe/Berlin', timestamptz '2024-01-15 12:00+00' at time zone 'Europe/Berlin'",
            Ok(&[r"2024-03-15 21:00:00|2024-07-15 14:00:00|2024-01-15 13:00:00"]),
        ),
        (
            r"select timestamptz '2024-03-15 12:00+00' at time zone 'PST', timestamptz '2024-07-15 12:00+00' at time zone 'EST', timestamptz '2024-07-15 12:00+00' at time zone 'CEST'",
            Ok(&[r"2024-03-15 04:00:00|2024-07-15 07:00:00|2024-07-15 14:00:00"]),
        ),
        (
            r"select timestamp '2024-07-15 12:00' at time zone 'Australia/Sydney', timestamp '2024-01-15 12:00' at time zone 'Australia/Sydney'",
            Ok(&[r"2024-07-14 22:00:00-04|2024-01-14 20:00:00-05"]),
        ),
        (
            r"select timestamptz '2024-10-06 01:59+00' at time zone 'Australia/Lord_Howe', timestamptz '2024-04-06 15:29+00' at time zone 'Australia/Lord_Howe', timestamptz '2024-04-06 15:31+00' at time zone 'Australia/Lord_Howe'",
            Ok(&[r"2024-10-06 12:59:00|2024-04-07 01:59:00|2024-04-07 02:01:00"]),
        ),
        (
            r"select timestamptz '2011-12-29 12:00+00' at time zone 'Pacific/Apia', timestamptz '2011-12-30 12:00+00' at time zone 'Pacific/Apia', timestamptz '2011-12-31 12:00+00' at time zone 'Pacific/Apia'",
            Ok(&[r"2011-12-29 02:00:00|2011-12-31 02:00:00|2012-01-01 02:00:00"]),
        ),
        (
            r"select timestamptz '1900-06-15 12:00+00' at time zone 'America/New_York', timestamptz '1850-06-15 12:00+00' at time zone 'America/New_York', timestamptz '1800-01-01 00:00+00' at time zone 'Europe/London'",
            Ok(&[r"1900-06-15 07:00:00|1850-06-15 07:03:58|1799-12-31 23:58:45"]),
        ),
        (
            r"select timestamptz '2100-07-15 12:00+00' at time zone 'America/New_York', timestamptz '2100-01-15 12:00+00' at time zone 'America/New_York', timestamptz '2500-07-15 12:00+00' at time zone 'Europe/Paris'",
            Ok(&[r"2100-07-15 08:00:00|2100-01-15 07:00:00|2500-07-15 14:00:00"]),
        ),
        (
            r"select timestamptz '2024-07-15 12:00+00' at time zone 'America/Sao_Paulo', timestamptz '2017-12-15 12:00+00' at time zone 'America/Sao_Paulo', timestamptz '2020-01-15 12:00+00' at time zone 'America/Sao_Paulo'",
            Ok(&[r"2024-07-15 09:00:00|2017-12-15 10:00:00|2020-01-15 09:00:00"]),
        ),
        (
            r"select timestamptz '2024-03-15 12:00:00 America/Chicago', timestamptz '2024-07-15 12:00:00 Europe/Paris', timestamptz '2024-03-15 12:00:00 EST', timestamptz '2024-03-15 12:00:00 PDT'",
            Ok(&[r"2024-03-15 13:00:00-04|2024-07-15 06:00:00-04|2024-03-15 13:00:00-04|2024-03-15 15:00:00-04"]),
        ),
        (
            r"select timestamptz '2024-03-31 02:30 Europe/Berlin', timestamptz '2024-10-27 02:30 Europe/Berlin'",
            Ok(&[r"2024-03-30 21:30:00-04|2024-10-26 21:30:00-04"]),
        ),
        (
            r"select date_trunc('day', timestamptz '2024-03-10 12:00'), date_trunc('day', timestamptz '2024-03-11 03:00+00'), date_trunc('month', timestamptz '2024-03-31 12:00'), date_trunc('hour', timestamptz '2024-11-03 06:30+00')",
            Ok(&[r"2024-03-10 00:00:00-05|2024-03-10 00:00:00-05|2024-03-01 00:00:00-05|2024-11-03 01:00:00-05"]),
        ),
        (
            r"select extract(hour from timestamptz '2024-07-01 12:00+00'), extract(timezone from timestamptz '2024-07-01 12:00+00'), extract(timezone from timestamptz '2024-01-01 12:00+00'), extract(timezone_hour from timestamptz '2024-01-01 12:00+00')",
            Ok(&[r"8|-14400|-18000|-5"]),
        ),
        (
            r"select extract(epoch from timestamptz '2024-03-10 02:30'), extract(epoch from timestamptz '2024-11-03 01:30')",
            Ok(&[r"1710055800.000000|1730615400.000000"]),
        ),
        (
            r"select timestamptz '2024-03-10 12:00'::date, timestamptz '2024-03-10 03:30+00'::date, date '2024-03-10'::timestamptz, timestamp '2024-03-10 12:00'::timestamptz, timestamptz '2024-07-10 12:00'::timestamp",
            Ok(&[r"2024-03-10|2024-03-09|2024-03-10 00:00:00-05|2024-03-10 12:00:00-04|2024-07-10 12:00:00"]),
        ),
        (
            r"select date '2024-03-10' < timestamptz '2024-03-10 00:00:01-04', timestamp '2024-11-03 01:30' = timestamptz '2024-11-03 06:30+00', date '2024-03-10' = timestamptz '2024-03-10 05:00+00'",
            Ok(&[r"f|t|t"]),
        ),
        (
            r"select to_char(timestamptz '2024-07-15 12:00+00', 'YYYY-MM-DD HH24:MI TZ OF TZH TZM'), to_char(timestamptz '2024-01-15 12:00+00', 'tz'), to_char(timestamp '2024-01-15 12:00', 'TZ')",
            Ok(&[r"2024-07-15 08:00 EDT -04 -04 00|est|"]),
        ),
        (
            r"select make_timestamptz(2024, 3, 10, 2, 30, 0), make_timestamptz(2024, 11, 3, 1, 30, 0), make_timestamptz(2024, 7, 1, 12, 0, 0, 'Europe/Paris'), make_timestamptz(2024, 1, 1, 12, 0, 0, 'Asia/Kolkata')",
            Ok(&[r"2024-03-10 03:30:00-04|2024-11-03 01:30:00-05|2024-07-01 06:00:00-04|2024-01-01 01:30:00-05"]),
        ),
        (
            r"select to_timestamp('2024-03-10 02:30', 'YYYY-MM-DD HH24:MI'), to_timestamp('2024-07-01 12:00', 'YYYY-MM-DD HH24:MI')",
            Ok(&[r"2024-03-10 03:30:00-04|2024-07-01 12:00:00-04"]),
        ),
        (
            r"select age(timestamptz '2024-03-11 00:00', timestamptz '2024-03-10 00:00'), timestamptz '2024-03-11' - timestamptz '2024-03-09'",
            Ok(&[r"1 day|1 day 23:00:00"]),
        ),
        (
            r"select generate_series(timestamptz '2024-03-09 12:00', timestamptz '2024-03-12 12:00', interval '1 day')",
            Ok(&[
                r"2024-03-09 12:00:00-05",
                r"2024-03-10 12:00:00-04",
                r"2024-03-11 12:00:00-04",
                r"2024-03-12 12:00:00-04",
            ]),
        ),
        (
            r"select generate_series(timestamptz '2024-03-09 12:00', timestamptz '2024-03-10 12:00', interval '12 hours')",
            Ok(&[r"2024-03-09 12:00:00-05", r"2024-03-10 00:00:00-05"]),
        ),
        (r"set time zone 'Europe/London'", Ok(&[])),
        (
            r"select timestamptz '2024-06-15 12:00+00', timestamptz '2024-12-15 12:00+00', timestamptz '1970-06-15 12:00+00', timestamptz '1969-06-15 12:00+00'",
            Ok(&[r"2024-06-15 13:00:00+01|2024-12-15 12:00:00+00|1970-06-15 13:00:00+01|1969-06-15 13:00:00+01"]),
        ),
        (r"set time zone 'Asia/Kolkata'", Ok(&[])),
        (
            r"select timestamptz '2024-06-15 12:00+00', timestamptz '1940-06-15 12:00+00', timestamptz '1900-01-01 00:00+00'",
            Ok(&[r"2024-06-15 17:30:00+05:30|1940-06-15 17:30:00+05:30|1900-01-01 05:21:10+05:21:10"]),
        ),
        (r"set time zone 'Australia/Lord_Howe'", Ok(&[])),
        (
            r"select timestamptz '2024-01-15 12:00+00', timestamptz '2024-07-15 12:00+00', timestamptz '2024-10-06 01:59+00', timestamptz '2024-10-06 02:00+00', timestamptz '2024-10-06 02:15:00'",
            Ok(&[
                r"2024-01-15 23:00:00+11|2024-07-15 22:30:00+10:30|2024-10-06 12:59:00+11|2024-10-06 13:00:00+11|2024-10-06 02:45:00+11",
            ]),
        ),
        (r"set time zone 'utc'", Ok(&[])),
        (r"set time zone 'Nowhere/City'", Err(r"22023")),
        (r"set time zone 'america/los_angeles'", Ok(&[])),
        (r"show timezone", Ok(&[r"America/Los_Angeles"])),
        (
            r"select now() at time zone 'America/New_York' is not null, current_setting('timezone')",
            Ok(&[r"t|America/Los_Angeles"]),
        ),
        (r"reset timezone", Ok(&[])),
        (r"select timestamptz '2024-03-15 12:00:00' at time zone 'Foo/Bar'", Err(r"22023")),
        (
            r"select (timestamptz '2024-01-01 12:00+00' at time zone 'EST5EDT')::text, (timestamptz '2024-07-01 12:00+00' at time zone 'EST5EDT')::text, (timestamptz '2024-07-01 12:00+00' at time zone 'Etc/GMT+5')::text, (timestamptz '2024-07-01 12:00+00' at time zone 'PST8PDT')::text",
            Ok(&[r"2024-01-01 07:00:00|2024-07-01 08:00:00|2024-07-01 07:00:00|2024-07-01 05:00:00"]),
        ),
    ]);
}

#[test]
fn dst_zone_functions_answer_as_postgres_does() {
    if !have_tzdata() {
        return;
    }
    script(&[
        (
            r"select date_trunc('day', timestamptz '2024-03-15 23:30+00', 'Asia/Tokyo'), date_trunc('day', timestamptz '2024-03-10 12:00+00', 'America/New_York'), date_trunc('hour', timestamptz '2024-11-03 06:30+00', 'America/New_York')",
            Ok(&[r"2024-03-15 15:00:00+00|2024-03-10 05:00:00+00|2024-11-03 06:00:00+00"]),
        ),
        (
            r"select date_trunc('month', timestamptz '2024-03-31 23:30+00', 'Europe/Berlin'), date_trunc('week', timestamptz '2024-03-31 23:30+00', 'Europe/Berlin'), date_trunc('year', timestamptz '2024-06-30 12:00+00', 'PST')",
            Ok(&[r"2024-03-31 22:00:00+00|2024-03-31 22:00:00+00|2024-01-01 08:00:00+00"]),
        ),
        (r"select date_trunc('day', timestamptz '2024-03-15 23:30+00', 'Nowhere/City')", Err(r"22023")),
        (r"select date_trunc('day', timestamptz '2024-03-15 23:30+00', null)", Ok(&[r""])),
        (
            r"select date_trunc('day', timestamp '2024-03-15 23:30', 'Asia/Tokyo'), date_trunc('day', date '2024-03-15', 'Asia/Tokyo')",
            Ok(&[r"2024-03-15 15:00:00+00|2024-03-14 15:00:00+00"]),
        ),
        (
            r"select generate_series(timestamptz '2024-03-09 12:00+00', timestamptz '2024-03-12 12:00+00', interval '1 day', 'America/New_York')",
            Ok(&[
                r"2024-03-09 12:00:00+00",
                r"2024-03-10 11:00:00+00",
                r"2024-03-11 11:00:00+00",
                r"2024-03-12 11:00:00+00",
            ]),
        ),
        (
            r"select generate_series(timestamptz '2024-10-26 12:00+00', timestamptz '2024-10-29 12:00+00', interval '1 day', 'Europe/Berlin')",
            Ok(&[r"2024-10-26 12:00:00+00", r"2024-10-27 13:00:00+00", r"2024-10-28 13:00:00+00"]),
        ),
        (
            r"select generate_series(timestamptz '2024-01-31 12:00+00', timestamptz '2024-05-31 12:00+00', interval '1 month', 'America/New_York')",
            Ok(&[
                r"2024-01-31 12:00:00+00",
                r"2024-02-29 12:00:00+00",
                r"2024-03-29 11:00:00+00",
                r"2024-04-29 11:00:00+00",
                r"2024-05-29 11:00:00+00",
            ]),
        ),
        (
            r"select timezone('America/New_York', timestamptz '2024-07-15 12:00+00'), timezone('Europe/Berlin', timestamp '2024-07-15 12:00')",
            Ok(&[r"2024-07-15 08:00:00|2024-07-15 10:00:00+00"]),
        ),
        (r"begin", Ok(&[])),
        (r"set local time zone 'Asia/Tokyo'", Ok(&[])),
        (r"select timestamptz '2024-07-15 12:00+00'", Ok(&[r"2024-07-15 21:00:00+09"])),
        (r"commit", Ok(&[])),
        (r"select timestamptz '2024-07-15 12:00+00'", Ok(&[r"2024-07-15 12:00:00+00"])),
        (r"set time zone 'America/New_York'", Ok(&[])),
        (r"prepare q(timestamptz) as select $1", Ok(&[])),
        (r"execute q('2024-03-10 02:30')", Ok(&[r"2024-03-10 03:30:00-04"])),
        (r"execute q('2024-07-01 12:00')", Ok(&[r"2024-07-01 12:00:00-04"])),
        (r"create table ev (id int, at timestamptz)", Ok(&[])),
        (
            r"insert into ev values (1, '2024-03-10 02:30'), (2, '2024-03-10 12:00'), (3, '2024-11-03 01:30'), (4, '2024-07-04 09:00:00+02')",
            Ok(&[]),
        ),
        (
            r"select id, at from ev order by at",
            Ok(&[
                r"1|2024-03-10 03:30:00-04",
                r"2|2024-03-10 12:00:00-04",
                r"4|2024-07-04 03:00:00-04",
                r"3|2024-11-03 01:30:00-05",
            ]),
        ),
        (
            r"select id, at::date, at::time, date_trunc('day', at) from ev order by id",
            Ok(&[
                r"1|2024-03-10|03:30:00|2024-03-10 00:00:00-05",
                r"2|2024-03-10|12:00:00|2024-03-10 00:00:00-05",
                r"3|2024-11-03|01:30:00|2024-11-03 00:00:00-04",
                r"4|2024-07-04|03:00:00|2024-07-04 00:00:00-04",
            ]),
        ),
        (
            r"select id, at at time zone 'UTC' from ev order by id",
            Ok(&[
                r"1|2024-03-10 07:30:00",
                r"2|2024-03-10 16:00:00",
                r"3|2024-11-03 06:30:00",
                r"4|2024-07-04 07:00:00",
            ]),
        ),
        (r"select count(*) from ev where at >= date '2024-03-10' and at < date '2024-03-11'", Ok(&[r"2"])),
        (
            r"select at::date, count(*) from ev group by 1 order by 1",
            Ok(&[r"2024-03-10|2", r"2024-07-04|1", r"2024-11-03|1"]),
        ),
        (r"select id from ev where at::date = '2024-03-10'", Ok(&[r"1", r"2"])),
        (
            r"select extract(hour from at), extract(dow from at), extract(epoch from at) from ev where id = 2",
            Ok(&[r"12|0|1710086400.000000"]),
        ),
        (
            r"select at + interval '1 day' from ev where id in (1, 2) order by id",
            Ok(&[r"2024-03-11 03:30:00-04", r"2024-03-11 12:00:00-04"]),
        ),
        (
            r"select at - interval '1 month' from ev where id in (1, 3) order by id",
            Ok(&[r"2024-02-10 03:30:00-05", r"2024-10-03 01:30:00-04"]),
        ),
        (r"select (at + interval '1 day') - at from ev where id = 2", Ok(&[r"1 day"])),
        (
            r"select to_char(at, 'Dy YYYY-MM-DD HH24:MI:SS TZ'), to_char(at, 'FMHH:MI OF') from ev order by id",
            Ok(&[
                r"Sun 2024-03-10 03:30:00 EDT|3:30 -04",
                r"Sun 2024-03-10 12:00:00 EDT|12:00 -04",
                r"Sun 2024-11-03 01:30:00 EST|1:30 -05",
                r"Thu 2024-07-04 03:00:00 EDT|3:00 -04",
            ]),
        ),
        (
            r"select now() > timestamptz '2020-01-01', current_date is not null, localtimestamp is not null",
            Ok(&[r"t|t|t"]),
        ),
        (r"select age(timestamptz '2024-07-01', timestamptz '2024-01-01')", Ok(&[r"6 mons"])),
        (r"select timestamptz '2024-07-01 00:00' - timestamptz '2024-01-01 00:00'", Ok(&[r"181 days 23:00:00"])),
        (r"select justify_hours(timestamptz '2024-03-11' - timestamptz '2024-03-10')", Ok(&[r"23:00:00"])),
    ]);
}
