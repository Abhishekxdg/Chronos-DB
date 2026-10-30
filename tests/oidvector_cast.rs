//! An array cast to an oidvector (or an int2vector) is one, written as one (`25 1043`), so it compares with
//! pg_proc.proargtypes: Rails asks whether a column's type has a lower() that way before it validates uniqueness
//! case-insensitively.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn arrays_cast_to_vectors() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    assert_eq!(
        run(
            &db,
            &mut s,
            "SELECT ARRAY[25::oid, 1043]::oidvector, ARRAY['text'::regtype]::oidvector = '25'::oidvector,
                 ARRAY[1, 2]::int2vector, '{}'::oid[]::oidvector = ''::oidvector"
        ),
        [["25 1043", "true", "1 2", "true"]]
    );
}

#[test]
fn rails_finds_lower_for_a_column_type() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let q = |t: &str| {
        format!(
            "SELECT exists(
                 SELECT * FROM pg_proc WHERE proname = 'lower' AND proargtypes = ARRAY['{t}'::regtype]::oidvector
             ) OR exists(
                 SELECT * FROM pg_proc INNER JOIN pg_cast ON ARRAY[casttarget]::oidvector = proargtypes
                 WHERE proname = 'lower' AND castsource = '{t}'::regtype
             )"
        )
    };
    assert_eq!(run(&db, &mut s, &q("text")), [["true"]]);
    assert_eq!(run(&db, &mut s, &q("character varying")), [["true"]]);
    assert_eq!(run(&db, &mut s, &q("integer")), [["false"]]);
}
