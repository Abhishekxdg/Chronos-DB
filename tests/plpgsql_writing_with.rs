//! A WITH whose queries write, run from PL/pgSQL (a DO block or a function), with SELECT ... INTO its result:
//! Supabase Storage's prefix backfill inserts the prefixes of a batch of objects and reads back how many it took.

use chronos::sql::{Session, Value, take_notices};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn a_writing_with_runs_and_gives_its_rows() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE objects (id int4, name text); CREATE TABLE prefixes (name text PRIMARY KEY);
         INSERT INTO objects VALUES (1, 'a/b'), (2, 'c'), (3, 'a/b')",
    );
    take_notices();
    run(
        &db,
        &mut s,
        "DO $$
         DECLARE n int; last text;
         BEGIN
           WITH batch AS (SELECT id, name FROM objects ORDER BY id LIMIT 10),
                batch_count AS (SELECT count(*) AS count FROM batch),
                cur AS (SELECT name FROM batch ORDER BY id DESC LIMIT 1),
                ins AS (INSERT INTO prefixes SELECT DISTINCT name FROM batch ON CONFLICT DO NOTHING)
           SELECT count, cur.name FROM cur, batch_count INTO n, last;
           RAISE NOTICE '% %', n, last;
         END $$",
    );
    let said: Vec<String> = take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(said, ["3 a/b"]);
    assert_eq!(run(&db, &mut s, "SELECT name FROM prefixes ORDER BY 1"), [["a/b"], ["c"]]);
    // and in a function, as its own statement
    run(
        &db,
        &mut s,
        "CREATE FUNCTION add(x text) RETURNS int LANGUAGE plpgsql AS $$
         DECLARE n int;
         BEGIN
           WITH ins AS (INSERT INTO prefixes VALUES (x) RETURNING name) SELECT count(*) FROM ins INTO n;
           RETURN n;
         END $$",
    );
    assert_eq!(run(&db, &mut s, "SELECT add('d')"), [["1"]]);
}

#[test]
fn it_is_what_its_main_statement_is() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE t (id int4)");
    // a SELECT's INTO takes the first of several rows (not STRICT)
    take_notices();
    run(
        &db,
        &mut s,
        "DO $$ DECLARE n int; BEGIN
           WITH ins AS (INSERT INTO t VALUES (1) RETURNING id) SELECT id FROM ins UNION ALL SELECT 2 INTO n;
           RAISE NOTICE '%', n;
         END $$",
    );
    let said: Vec<String> = take_notices().into_iter().map(|n| n.message).collect();
    assert_eq!(said.len(), 1);
    // one without INTO has no destination for its rows
    let e =
        s.run(&db, "DO $$ BEGIN WITH ins AS (INSERT INTO t VALUES (3) RETURNING id) SELECT id FROM ins; END $$", &[]);
    assert_eq!(e.unwrap_err().code, "42601");
    // and one whose main statement is an INSERT runs without one
    run(&db, &mut s, "DO $$ BEGIN WITH x AS (SELECT 4 AS v) INSERT INTO t SELECT v FROM x; END $$");
}

#[test]
fn a_members_write_target_names_its_columns() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE t (id int4); INSERT INTO t VALUES (1), (2)");
    // `id` is the column and a variable: ambiguous, as in an UPDATE of its own
    let e = s
        .run(
            &db,
            "DO $$ DECLARE id int := 99; n int; BEGIN WITH upd AS (UPDATE t SET id = id + 1) SELECT 1 INTO n; END $$",
            &[],
        )
        .unwrap_err();
    assert_eq!(e.code, "42702");
    assert_eq!(run(&db, &mut s, "SELECT id FROM t ORDER BY id"), [["1"], ["2"]]);
}
