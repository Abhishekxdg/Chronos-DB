//! Numeric literals with underscores between digits (`1_000`), as in Postgres 16+.

use chronos::sql::run;
use chronos::{Db, MAIN};

fn one(sql: &str) -> String {
    let db = Db::new();
    let out = run(&db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    out.last().unwrap().rows[0][0].to_text()
}

#[test]
fn underscores_group_digits() {
    assert_eq!(one("select 1_000"), "1000");
    assert_eq!(one("select 1_000 as n"), "1000");
    assert_eq!(one("select 1_0_0 + 1"), "101");
    assert_eq!(one("select 1_000.000_5"), "1000.0005");
    assert_eq!(one("select 1_0e1_0::float8 > 1"), "true");
}

#[test]
fn a_stray_underscore_is_never_an_alias() {
    let db = Db::new();
    // `1_` and `1__0` are not numbers: the `_` starts a word, so this is a syntax error, not `1 AS _`.
    for sql in ["select 1_", "select 1__0"] {
        assert!(run(&db, MAIN, sql, &[]).is_err(), "{sql}");
    }
}
