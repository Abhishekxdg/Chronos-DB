//! Second review, enum type names: a name is kept only while a type, a statement or a stored
//! function's place needs it, so making and dropping many distinct types keeps memory bounded;
//! a prepared statement and a function naming an older type still work afterwards.

use chronos::sql::{Session, enum_type_names};
use chronos::{Db, MAIN};

fn first(s: &mut Session, db: &Db, sql: &str) -> String {
    let out = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    out.last().unwrap().rows[0][0].to_text()
}

#[test]
fn distinct_type_names_are_not_kept_for_good() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, "create type keep as enum ('a', 'b')", &[]).unwrap();
    s.run(&db, "create function f(x keep) returns text language sql as $$ select x::text $$", &[]).unwrap();
    s.run(&db, "prepare p as select 'b'::keep", &[]).unwrap();
    for n in 0..5000 {
        assert_eq!(first(&mut s, &db, &format!("create type t{n} as enum ('a'); select 'a'::t{n}")), "a");
        s.run(&db, &format!("drop type t{n}"), &[]).unwrap();
    }
    let kept = enum_type_names();
    assert!(kept <= 4096, "{kept} enum type names kept: every distinct name stays");
    assert_eq!(first(&mut s, &db, "execute p"), "b");
    assert_eq!(first(&mut s, &db, "select f('a')"), "a");
    assert_eq!(first(&mut s, &db, "select 'a'::keep"), "a");
}
