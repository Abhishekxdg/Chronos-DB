//! The degree functions (`sind` ... `atan2d`), `asinh`/`acosh`/`atanh`, and `scale`/`min_scale`/`trim_scale`.
//! Postgres's answers are compared in tests/pgdiff.rs (`degree_functions_and_numeric_scale`); these run without one.

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
fn degree_functions_and_numeric_scale() {
    script(&[
        (r"select sind(0), cosd(0), tand(0), cotd(0)", Ok(&[r"0|1|0|Infinity"])),
        (
            r"select sind(1), cosd(1), tand(1), cotd(1)",
            Ok(&[r"0.017452406437283515|0.9998476951563913|0.01745506492821759|57.28996163075942"]),
        ),
        (
            r"select sind(15), cosd(15), tand(15), cotd(15)",
            Ok(&[r"0.2588190451025208|0.9659258262890683|0.26794919243112275|3.7320508075688767"]),
        ),
        (
            r"select sind(30), cosd(30), tand(30), cotd(30)",
            Ok(&[r"0.5|0.8660254037844387|0.5773502691896257|1.7320508075688774"]),
        ),
        (r"select sind(45), cosd(45), tand(45), cotd(45)", Ok(&[r"0.7071067811865475|0.7071067811865475|1|1"])),
        (
            r"select sind(60), cosd(60), tand(60), cotd(60)",
            Ok(&[r"0.8660254037844387|0.5|1.7320508075688774|0.5773502691896257"]),
        ),
        (
            r"select sind(75), cosd(75), tand(75), cotd(75)",
            Ok(&[r"0.9659258262890683|0.2588190451025208|3.7320508075688767|0.26794919243112275"]),
        ),
        (r"select sind(90), cosd(90), tand(90), cotd(90)", Ok(&[r"1|0|Infinity|0"])),
        (
            r"select sind(120), cosd(120), tand(120), cotd(120)",
            Ok(&[r"0.8660254037844387|-0.5|-1.7320508075688774|-0.5773502691896257"]),
        ),
        (r"select sind(135), cosd(135), tand(135), cotd(135)", Ok(&[r"0.7071067811865475|-0.7071067811865475|-1|-1"])),
        (
            r"select sind(150), cosd(150), tand(150), cotd(150)",
            Ok(&[r"0.5|-0.8660254037844387|-0.5773502691896257|-1.7320508075688774"]),
        ),
        (r"select sind(180), cosd(180), tand(180), cotd(180)", Ok(&[r"0|-1|0|-Infinity"])),
        (
            r"select sind(210), cosd(210), tand(210), cotd(210)",
            Ok(&[r"-0.5|-0.8660254037844387|0.5773502691896257|1.7320508075688774"]),
        ),
        (r"select sind(225), cosd(225), tand(225), cotd(225)", Ok(&[r"-0.7071067811865475|-0.7071067811865475|1|1"])),
        (r"select sind(270), cosd(270), tand(270), cotd(270)", Ok(&[r"-1|0|-Infinity|0"])),
        (
            r"select sind(300), cosd(300), tand(300), cotd(300)",
            Ok(&[r"-0.8660254037844387|0.5|-1.7320508075688774|-0.5773502691896257"]),
        ),
        (r"select sind(315), cosd(315), tand(315), cotd(315)", Ok(&[r"-0.7071067811865475|0.7071067811865475|-1|-1"])),
        (
            r"select sind(330), cosd(330), tand(330), cotd(330)",
            Ok(&[r"-0.5|0.8660254037844387|-0.5773502691896257|-1.7320508075688774"]),
        ),
        (r"select sind(360), cosd(360), tand(360), cotd(360)", Ok(&[r"0|1|0|Infinity"])),
        (
            r"select sind(390), cosd(390), tand(390), cotd(390)",
            Ok(&[r"0.5|0.8660254037844387|0.5773502691896257|1.7320508075688774"]),
        ),
        (
            r"select sind(-30), cosd(-30), tand(-30), cotd(-30)",
            Ok(&[r"-0.5|0.8660254037844387|-0.5773502691896257|-1.7320508075688774"]),
        ),
        (r"select sind(-45), cosd(-45), tand(-45), cotd(-45)", Ok(&[r"-0.7071067811865475|0.7071067811865475|-1|-1"])),
        (r"select sind(-90), cosd(-90), tand(-90), cotd(-90)", Ok(&[r"-1|0|-Infinity|0"])),
        (r"select sind(-180), cosd(-180), tand(-180), cotd(-180)", Ok(&[r"-0|-1|0|Infinity"])),
        (r"select sind(-270), cosd(-270), tand(-270), cotd(-270)", Ok(&[r"1|0|Infinity|0"])),
        (r"select sind(720), cosd(720), tand(720), cotd(720)", Ok(&[r"0|1|0|Infinity"])),
        (
            r"select sind(1000000.0), cosd(1000000.0), tand(1000000.0), cotd(1000000.0)",
            Ok(&[r"-0.984807753012208|0.17364817766693036|-5.671281819617709|-0.176326980708465"]),
        ),
        (
            r"select sind(0.5), cosd(0.5), tand(0.5), cotd(0.5)",
            Ok(&[r"0.008726535498373936|0.9999619230641713|0.008726867790758791|114.58865012930958"]),
        ),
        (
            r"select sind(89.999), cosd(89.999), tand(89.999), cotd(89.999)",
            Ok(&[r"0.9999999998476913|1.745329251914054e-05|57295.77950699097|1.7453292521798827e-05"]),
        ),
        (
            r"select sind(30.0000001), cosd(30.0000001), tand(30.0000001), cotd(30.0000001)",
            Ok(&[r"0.5000000015114994|0.866025402911774|0.5773502715167314|1.7320508005875606"]),
        ),
        (
            r"select sind('NaN'::float8), cosd('NaN'::float8), tand('NaN'::float8), cotd('NaN'::float8)",
            Ok(&[r"NaN|NaN|NaN|NaN"]),
        ),
        (
            r"select sind('Infinity'::float8), cosd('Infinity'::float8), tand('Infinity'::float8), cotd('Infinity'::float8)",
            Err(r"22003"),
        ),
        (
            r"select sind('-Infinity'::float8), cosd('-Infinity'::float8), tand('-Infinity'::float8), cotd('-Infinity'::float8)",
            Err(r"22003"),
        ),
        (r"select asind(-2), acosd(-2), atand(-2)", Err(r"22003")),
        (r"select asind(-1), acosd(-1), atand(-1)", Ok(&[r"-90|180|-45"])),
        (
            r"select asind(-0.9), acosd(-0.9), atand(-0.9)",
            Ok(&[r"-64.15806723683288|154.1580672368329|-41.98721249581666"]),
        ),
        (r"select asind(-0.5), acosd(-0.5), atand(-0.5)", Ok(&[r"-30|120|-26.565051177077986"])),
        (
            r"select asind(-0.25), acosd(-0.25), atand(-0.25)",
            Ok(&[r"-14.477512185929923|104.47751218592992|-14.036243467926479"]),
        ),
        (r"select asind(0), acosd(0), atand(0)", Ok(&[r"0|90|0"])),
        (r"select asind(0.1), acosd(0.1), atand(0.1)", Ok(&[r"5.739170477266786|84.26082952273322|5.710593137499643"])),
        (
            r"select asind(0.25), acosd(0.25), atand(0.25)",
            Ok(&[r"14.477512185929923|75.52248781407008|14.036243467926479"]),
        ),
        (r"select asind(0.5), acosd(0.5), atand(0.5)", Ok(&[r"30|60|26.565051177077986"])),
        (
            r"select asind(0.75), acosd(0.75), atand(0.75)",
            Ok(&[r"48.59037789072914|41.40962210927086|36.86989764584402"]),
        ),
        (
            r"select asind(0.9), acosd(0.9), atand(0.9)",
            Ok(&[r"64.15806723683288|25.841932763167122|41.98721249581666"]),
        ),
        (r"select asind(1), acosd(1), atand(1)", Ok(&[r"90|0|45"])),
        (r"select asind(1.5), acosd(1.5), atand(1.5)", Err(r"22003")),
        (r"select atan2d(1,1)", Ok(&[r"45"])),
        (r"select atan2d(1,0)", Ok(&[r"90"])),
        (r"select atan2d(0,1)", Ok(&[r"0"])),
        (r"select atan2d(-1,-1)", Ok(&[r"-135"])),
        (r"select atan2d(0,0)", Ok(&[r"0"])),
        (r"select atan2d(1,-1)", Ok(&[r"135"])),
        (r"select atan2d(-1,0)", Ok(&[r"-90"])),
        (r"select asinh(0), acosh(1), atanh(0.0)", Ok(&[r"0|0|0"])),
        (r"select asinh(1), acosh(2), atanh(0.5)", Ok(&[r"0.881373587019543|1.3169578969248166|0.5493061443340548"])),
        (r"select asinh(2), acosh(3), atanh(0.5)", Ok(&[r"1.4436354751788103|1.762747174039086|0.5493061443340548"])),
        (
            r"select asinh(-3), acosh(4), atanh(0.5)",
            Ok(&[r"-1.8184464592320668|2.0634370688955608|0.5493061443340548"]),
        ),
        (
            r"select asinh(0.5), acosh(1.5), atanh(0.25)",
            Ok(&[r"0.48121182505960347|0.9624236501192069|0.25541281188299536"]),
        ),
        (r"select acosh(0.5)", Err(r"22003")),
        (r"select atanh(1)", Ok(&[r"Infinity"])),
        (r"select atanh(-1)", Ok(&[r"-Infinity"])),
        (r"select atanh(2)", Err(r"22003")),
        (r"select asind(2)", Err(r"22003")),
        (r"select acosd(-1.01)", Err(r"22003")),
        (
            r"select scale(1.50), scale(0), scale(100), scale(-0.001), scale(1e3::numeric), scale(null), min_scale(1.500), min_scale(0.0), min_scale(100), min_scale(1.23), min_scale(0.00), trim_scale(1.500), trim_scale(100), trim_scale(0.000), trim_scale(-2.50), trim_scale(10.10)",
            Ok(&[r"2|0|0|3|0||1|0|0|2|0|1.5|100|0|-2.5|10.1"]),
        ),
        (r"select scale(1.5::float8)", Err(r"42883")),
    ]);
}
