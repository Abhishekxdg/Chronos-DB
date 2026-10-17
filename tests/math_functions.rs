//! Integer literals in hex/octal/binary, integer and boolean casts, trigonometry, gcd/lcm/div,
//! width_bucket, bit aggregates. Postgres's answers are compared in tests/pgdiff.rs
//! (`math_functions`); these run without one.

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
fn radix_literals_are_whole_numbers() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(first(&mut s, &db, "select 0x1F, 0o17, 0b101, 0XfF, 0x_1, 0xFFFFFFFF"), "31|15|5|255|1|4294967295");
    for bad in ["select 0x", "select 0b2", "select 1_0x", "select 0x1G"] {
        assert_eq!(code(&mut s, &db, bad), "42601", "{bad}");
    }
}

#[test]
fn integers_and_booleans_cast_to_each_other() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(
        first(&mut s, &db, "select 1::boolean, 0::boolean, 5::boolean, true::int, false::int"),
        "true|false|true|1|0"
    );
    assert_eq!(first(&mut s, &db, "select null::int::boolean is null, pg_typeof(true::int)"), "true|integer");
    assert_eq!(code(&mut s, &db, "select 1::bigint::boolean"), "42804");
}

#[test]
fn the_number_functions() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let want = [
        ("select degrees(pi()), radians(180) = pi(), sin(0), cos(0), atan2(1, 1) * 4 = pi()", "180|true|0|1|true"),
        ("select div(7, 2), div(-7.5, 2), gcd(12, 18), lcm(4, 6), factorial(5)", "3|-3|6|12|120"),
        ("select width_bucket(5.5, 0, 10, 4), width_bucket(15, 0, 10, 4), width_bucket(5, 10, 0, 4)", "3|5|3"),
        ("select to_hex(255), round(2.5::float8), round(3.5::float8), round(-2.5::float8)", "ff|2|4|-2"),
        (
            "select pg_typeof(sign(-3)), pg_typeof(ceil(5)), pg_typeof(div(7, 2))",
            "double precision|double precision|numeric",
        ),
    ];
    for (q, out) in want {
        assert_eq!(first(&mut s, &db, q), out, "{q}");
    }
    assert_eq!(code(&mut s, &db, "select asin(2)"), "22003");
    assert_eq!(code(&mut s, &db, "select div(1, 0)"), "22012");
    assert_eq!(code(&mut s, &db, "select width_bucket(5, 3, 3, 4)"), "2201G");
}

#[test]
fn bit_aggregates_and_any_value() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(&db, "create table b (id int primary key, a int)", &[]).unwrap();
    s.run(&db, "insert into b values (1, 12), (2, 10), (3, NULL)", &[]).unwrap();
    assert_eq!(
        first(&mut s, &db, "select bit_and(a), bit_or(a), bit_xor(a), any_value(a) is not null from b"),
        "8|14|6|true"
    );
    assert_eq!(first(&mut s, &db, "select bit_and(a) is null from b where id = 3"), "true");
    assert_eq!(code(&mut s, &db, "select bit_and('a')"), "42725");
}
