//! Dates and timestamps before the common era, `infinity` and `-infinity`, and the special words
//! (`epoch`, `today`, ...). Postgres's answers are compared in tests/pgdiff.rs (`infinity_and_bc`);
//! these run without one, and check that no function trips over an infinite value.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn first(s: &mut Session, db: &Db, q: &str) -> String {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows[0].iter().map(Value::to_text).collect::<Vec<_>>().join("|")
}

#[test]
fn infinity_orders_beyond_every_date() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(&db, "create table p (id int primary key, d date, ts timestamptz)", &[]).unwrap();
    s.run(
        &db,
        "insert into p values (1, '2024-03-15', '2024-03-15 10:00+00'), (2, 'infinity', 'infinity'), (3, '-infinity', '-infinity')",
        &[],
    )
    .unwrap();
    assert_eq!(first(&mut s, &db, "select string_agg(id::text, ',' order by d) from p"), "3,1,2");
    assert_eq!(first(&mut s, &db, "select max(d), min(ts) from p"), "infinity|-infinity");
    assert_eq!(first(&mut s, &db, "select count(*) from p where ts > now() and d = 'infinity'"), "1");
    assert_eq!(first(&mut s, &db, "select isfinite(d), isfinite(ts) from p where id = 2"), "false|false");
    assert_eq!(
        first(&mut s, &db, "select 'infinity'::date = 'infinity'::timestamp, 'infinity'::date + 1"),
        "true|infinity"
    );
    let e = s.run(&db, "select 'infinity'::date - date '2024-01-01'", &[]).unwrap_err();
    assert_eq!(e.code, "22008");
}

#[test]
fn years_before_the_common_era() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let q = "select '0044-03-15 BC'::date, date '0001-01-01' - 1, '0044-03-15 10:00:00 BC'::timestamp, '0044-03-15 bc'::date < '0001-01-01'";
    assert_eq!(first(&mut s, &db, q), "0044-03-15 BC|0001-12-31 BC|0044-03-15 10:00:00 BC|true");
    assert_eq!(first(&mut s, &db, "select to_char('0044-03-15 BC'::date, 'YYYY BC')"), "0044 BC");
    // a two-digit year leads only in YMD order (in MDY, Postgres's default, 44 is a month and out of range)
    let e = s.run(&db, "select '44-03-15'::date", &[]).unwrap_err();
    assert_eq!(e.code, "22008");
    s.run(&db, "set datestyle = 'ISO, YMD'", &[]).unwrap();
    assert_eq!(first(&mut s, &db, "select '44-03-15'::date, '70-01-01'::date"), "2044-03-15|1970-01-01");
    s.run(&db, "reset datestyle", &[]).unwrap();
    let e = s.run(&db, "select '0000-01-01'::date", &[]).unwrap_err();
    assert_eq!(e.code, "22008");
}

#[test]
fn no_function_trips_over_an_infinite_value() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let fns = [
        "date_trunc('day', X)",
        "date_trunc('month', X::timestamp)",
        "extract(hour from X)",
        "date_part('year', X)",
        "X::text",
        "X::date",
        "X::timestamp",
        "X::timestamptz",
        "X + interval '1 day'",
        "X - interval '1 month'",
        "X - X",
        "X - now()",
        "now() - X",
        "age(X)",
        "age(X, now())",
        "to_char(X, 'YYYY-MM-DD')",
        "X at time zone 'UTC'",
        "X at time zone 'Asia/Kolkata'",
        "to_jsonb(X)",
        "least(X, now())",
        "greatest(X, now())",
        "coalesce(X, now())",
        "X = X",
        "X between now() and X",
        "(X, X) overlaps (X, X)",
        "array[X]",
        "isfinite(X)",
        "X + time '10:00'",
        "X::date + 1",
        "X::date - 1",
        "X::date - X::date",
        "extract(epoch from X)",
        "generate_series(X, now(), interval '1 day')",
        "make_interval(days => 1) + X",
    ];
    for ty in ["date", "timestamp", "timestamptz"] {
        for word in ["infinity", "-infinity"] {
            for f in fns {
                let x = format!("'{word}'::{ty}");
                let q = format!("select {}", f.replace('X', &x));
                // an error is fine (Postgres has some); a panic is not
                let _ = s.run(&db, &q, &[]);
            }
        }
    }
}
