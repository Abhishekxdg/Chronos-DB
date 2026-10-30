//! CREATE EXTENSION of the extensions built in (hstore, citext, pg_trgm, uuid-ossp, vector, ...) is recorded as
//! pgcrypto's is: pg_extension lists each once it's made, at its Postgres 16 version, as Rails' `extensions` and
//! schema dumps and Django's `CreateExtension` read it. Their types and functions are there before it too.

use chronos::sql::{Session, Value, take_notices};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn code(db: &Db, s: &mut Session, q: &str) -> String {
    s.run(db, q, &[]).expect_err(q).code.to_string()
}

#[test]
fn pg_extension_lists_what_was_made() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let listed = "SELECT extname, extversion FROM pg_extension WHERE extname <> 'plpgsql' ORDER BY 1";
    assert!(run(&db, &mut s, listed).is_empty());
    // (built in before it's made)
    assert_eq!(run(&db, &mut s, "SELECT 'a=>1'::hstore -> 'a'"), [["1"]]);
    run(&db, &mut s, "CREATE EXTENSION hstore; CREATE EXTENSION IF NOT EXISTS citext; CREATE EXTENSION vector");
    assert_eq!(run(&db, &mut s, listed), [["citext", "1.6"], ["hstore", "1.8"], ["vector", "0.8.0"]]);
    // each its own oid
    assert_eq!(run(&db, &mut s, "SELECT count(DISTINCT oid) FROM pg_extension"), [["4"]]);
    take_notices();
    run(&db, &mut s, "CREATE EXTENSION IF NOT EXISTS hstore");
    let said: Vec<String> = take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(said, ["extension \"hstore\" already exists, skipping"]);
    assert_eq!(code(&db, &mut s, "CREATE EXTENSION hstore"), "42710");
    assert_eq!(code(&db, &mut s, "CREATE EXTENSION pg_trgm VERSION '1.0'"), "22023");
    run(&db, &mut s, "ALTER EXTENSION hstore UPDATE");
    run(&db, &mut s, "DROP EXTENSION hstore, citext");
    assert_eq!(run(&db, &mut s, listed), [["vector", "0.8.0"]]);
    assert_eq!(code(&db, &mut s, "DROP EXTENSION hstore"), "42704");
    run(&db, &mut s, "DROP EXTENSION IF EXISTS hstore");
    // in a schema of its own, as Rails' enable_extension "customschema.hstore" makes it
    run(&db, &mut s, "CREATE SCHEMA customschema; CREATE EXTENSION hstore SCHEMA customschema");
    assert_eq!(
        run(&db, &mut s, "SELECT extnamespace::regnamespace::text FROM pg_extension WHERE extname = 'hstore'"),
        [["customschema"]]
    );
}

#[test]
fn uuid_ossp_says_what_it_gives() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    take_notices();
    run(&db, &mut s, "CREATE EXTENSION \"uuid-ossp\"");
    let said: Vec<String> = take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(said, ["uuid-ossp: only uuid_generate_v4() is available, and it is built in"]);
    assert_eq!(run(&db, &mut s, "SELECT extversion FROM pg_extension WHERE extname = 'uuid-ossp'"), [["1.1"]]);
    assert_eq!(run(&db, &mut s, "SELECT length(uuid_generate_v4()::text)"), [["36"]]);
}
