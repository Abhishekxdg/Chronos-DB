//! What open-source apps' schemas and migrations need, as they run against Chronos (Django's, Rails', Supabase's): each
//! case one of theirs that failed.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

/// GoTrue's (Supabase Auth's) `adds_confirmed_at` migration: a generated column naming its table's columns with the
/// table's name before them, in a schema of its own.
#[test]
fn a_generated_column_names_its_tables_columns_with_the_table() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE SCHEMA auth;
         CREATE TABLE auth.users (id int4 PRIMARY KEY, email_confirmed_at timestamptz, phone_confirmed_at timestamptz);
         INSERT INTO auth.users VALUES (1, '2026-01-02 00:00+00', '2026-01-01 00:00+00'), (2, NULL, NULL);
         ALTER TABLE auth.users ADD COLUMN IF NOT EXISTS confirmed_at timestamptz
             GENERATED ALWAYS AS (LEAST (users.email_confirmed_at, users.phone_confirmed_at)) STORED;
         INSERT INTO auth.users (id, email_confirmed_at) VALUES (3, '2026-03-01 00:00+00')",
    );
    assert_eq!(
        run(&db, &mut s, "SELECT id, confirmed_at IS NOT NULL FROM auth.users ORDER BY id"),
        [["1", "true"], ["2", "false"], ["3", "true"]]
    );
    // and with the schema too, in CREATE TABLE
    run(
        &db,
        &mut s,
        "CREATE TABLE auth.t (a int4, b int4 GENERATED ALWAYS AS (auth.t.a * 2) STORED); INSERT INTO auth.t VALUES (4)",
    );
    assert_eq!(run(&db, &mut s, "SELECT b FROM auth.t"), [["8"]]);
}

/// Supabase Storage's `prefixes` table: a generated column of an IMMUTABLE STRICT SQL function's, and a CHECK of one,
/// which run where no query does: the functions' bodies are put in their place, as Postgres inlines them.
#[test]
fn generated_columns_and_checks_call_sql_functions() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        r#"CREATE SCHEMA storage;
         CREATE OR REPLACE FUNCTION "storage"."get_level"("name" text) RETURNS int
         AS $func$ SELECT array_length(string_to_array("name", '/'), 1); $func$ LANGUAGE SQL IMMUTABLE STRICT;
         CREATE TABLE "storage"."prefixes" (
             "name" text COLLATE "C",
             "level" int GENERATED ALWAYS AS ("storage"."get_level"("name")) STORED);
         INSERT INTO storage.prefixes (name) VALUES ('a/b/c'), ('a'), (NULL)"#,
    );
    assert_eq!(
        run(&db, &mut s, "SELECT name, level FROM storage.prefixes ORDER BY level NULLS LAST"),
        [["a", "1"], ["a/b/c", "3"], ["", ""]]
    );
    run(
        &db,
        &mut s,
        "CREATE FUNCTION short_enough(t text) RETURNS boolean AS 'SELECT length(t) < 4' LANGUAGE sql IMMUTABLE;
         CREATE TABLE codes (c text CHECK (short_enough(c)));
         INSERT INTO codes VALUES ('ab')",
    );
    let e = s.run(&db, "INSERT INTO codes VALUES ('abcdef')", &[]).unwrap_err();
    assert_eq!(e.code, "23514");
}

/// What inlining a function into a CHECK or a generated column keeps of a call: its named arguments in the function's
/// order, the caller's right to run it, and a STRICT function's argument evaluated once.
#[test]
fn inlined_calls_keep_what_a_call_is() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE FUNCTION minus(a int4, b int4) RETURNS int4 AS 'SELECT a - b' LANGUAGE sql IMMUTABLE;
         CREATE TABLE g (x int4, d int4 GENERATED ALWAYS AS (minus(b => 1, a => x)) STORED);
         INSERT INTO g (x) VALUES (10)",
    );
    assert_eq!(run(&db, &mut s, "SELECT d FROM g"), [["9"]]);
    // one the role may not run isn't run through a table it makes
    run(
        &db,
        &mut s,
        "CREATE FUNCTION secret(x int4) RETURNS boolean AS 'SELECT x > 0' LANGUAGE sql IMMUTABLE;
         REVOKE EXECUTE ON FUNCTION secret(int4) FROM public;
         CREATE ROLE app; GRANT CREATE ON SCHEMA public TO app; SET ROLE app",
    );
    // (refused when the table is made, or else when a row is checked)
    let e = match s.run(&db, "CREATE TABLE h (x int4 CHECK (secret(x)))", &[]) {
        Err(e) => e,
        Ok(_) => s.run(&db, "INSERT INTO h VALUES (1)", &[]).unwrap_err(),
    };
    assert_eq!(e.code, "42501");
}

/// A function a generated column names without a schema is the one where its table is, whatever search_path a row's
/// writer has.
#[test]
fn an_inlined_call_means_what_it_did_where_the_table_is() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE SCHEMA a; CREATE SCHEMA b;
         CREATE FUNCTION a.f(x int4) RETURNS int4 AS 'SELECT x + 1' LANGUAGE sql IMMUTABLE;
         CREATE FUNCTION b.f(x int4) RETURNS int4 AS 'SELECT x + 100' LANGUAGE sql IMMUTABLE;
         SET search_path = a;
         CREATE TABLE a.t (x int4, y int4 GENERATED ALWAYS AS (f(x)) STORED);
         SET search_path = b;
         INSERT INTO a.t (x) VALUES (1)",
    );
    assert_eq!(run(&db, &mut s, "SELECT y FROM a.t"), [["2"]]);
}
