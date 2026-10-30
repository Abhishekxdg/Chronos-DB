//! NOTIFY in PL/pgSQL is the utility statement it is: run for its effect, it leaves FOUND as it was, and a SELECT
//! of pg_notify(...) written out is still a query with an INTO.

use chronos::sql::{Session, Value, take_notices};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn notify_leaves_found_alone() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    take_notices();
    run(
        &db,
        &mut s,
        "DO $$
         DECLARE r record; n int;
         BEGIN
           PERFORM 1 WHERE false;
           NOTIFY jobs, 'x';
           RAISE NOTICE 'found %', FOUND;
           PERFORM 1;
           NOTIFY jobs, 'x';
           GET DIAGNOSTICS n = ROW_COUNT;
           RAISE NOTICE 'found % %', FOUND, n;
           SELECT pg_notify('jobs', 'y') INTO r;
           RAISE NOTICE 'found %', FOUND;
         END $$",
    );
    let said: Vec<String> = take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(said, ["found f", "found t 0", "found t"]);
}
