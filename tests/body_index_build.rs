//! A UNIQUE constraint or an index a DO block or function makes is there for what the body does next, as in Postgres:
//! GoTrue's migration adds `refresh_tokens_token_unique` and then a foreign key to it in one DO block.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

#[test]
fn a_foreign_key_to_a_unique_made_just_before() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE rt (id int4 PRIMARY KEY, token varchar(255), parent varchar(255));
         INSERT INTO rt VALUES (1, 'a', NULL);
         DO $$
         BEGIN
           IF NOT EXISTS (SELECT * FROM information_schema.constraint_column_usage
                          WHERE table_name = 'rt' AND constraint_name = 'rt_token_unique') THEN
             ALTER TABLE rt ADD CONSTRAINT rt_token_unique UNIQUE (token);
           END IF;
           ALTER TABLE rt ADD CONSTRAINT rt_parent_fkey FOREIGN KEY (parent) REFERENCES rt (token);
           CREATE UNIQUE INDEX rt_id_token ON rt (id, token);
           RAISE NOTICE '%', (SELECT count(*) FROM pg_class WHERE relname = 'rt_id_token');
         END $$",
    );
    assert_eq!(
        run(&db, &mut s, "SELECT conname FROM pg_constraint WHERE conrelid = 'rt'::regclass ORDER BY 1"),
        [["rt_parent_fkey"], ["rt_pkey"], ["rt_token_unique"]]
    );
    assert_eq!(s.run(&db, "INSERT INTO rt VALUES (2, 'b', 'zz')", &[]).unwrap_err().code, "23503");
    assert_eq!(s.run(&db, "INSERT INTO rt VALUES (2, 'a', NULL)", &[]).unwrap_err().code, "23505");
    run(&db, &mut s, "INSERT INTO rt VALUES (2, 'b', 'a')");
}

#[test]
fn rows_the_body_wrote_still_count() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE t (a int4)");
    // the body's own rows are in the build: a duplicate among them refuses the constraint
    let e = s
        .run(&db, "DO $$ BEGIN INSERT INTO t VALUES (1), (1); ALTER TABLE t ADD CONSTRAINT t_a UNIQUE (a); END $$", &[])
        .unwrap_err();
    assert_eq!(e.code, "23505");
    run(&db, &mut s, "DO $$ BEGIN INSERT INTO t VALUES (1), (2); ALTER TABLE t ADD CONSTRAINT t_a UNIQUE (a); END $$");
    assert_eq!(s.run(&db, "INSERT INTO t VALUES (2)", &[]).unwrap_err().code, "23505");
}

#[test]
fn a_partitioned_tables_index_counts_its_partitions_rows() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE p (a int4) PARTITION BY RANGE (a); CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10)",
    );
    let e = s
        .run(&db, "DO $$ BEGIN INSERT INTO p VALUES (1), (1); CREATE UNIQUE INDEX p_a ON p (a); END $$", &[])
        .unwrap_err();
    assert_eq!(e.code, "23505");
    run(&db, &mut s, "DO $$ BEGIN INSERT INTO p VALUES (2); CREATE UNIQUE INDEX p_a ON p (a); END $$");
    assert_eq!(s.run(&db, "INSERT INTO p VALUES (2)", &[]).unwrap_err().code, "23505");
}

#[test]
fn a_partitions_unique_after_an_index_on_its_parent() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "CREATE TABLE p (id int4, k int4) PARTITION BY RANGE (id);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (100);
         DO $$
         BEGIN
           CREATE INDEX p_k ON p (k);
           ALTER TABLE p1 ADD CONSTRAINT p1_k_key UNIQUE (k);
         END $$",
    );
    // both are there after the block: the partition's index and its UNIQUE
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_indexes WHERE tablename = 'p1'"), [["2"]]);
    run(&db, &mut s, "INSERT INTO p VALUES (1, 7)");
    assert_eq!(s.run(&db, "INSERT INTO p VALUES (2, 7)", &[]).unwrap_err().code, "23505");
}

#[test]
fn an_index_name_pending_on_another_table_is_taken() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE a (x int4); CREATE TABLE b (x int4)");
    let e = s
        .run(
            &db,
            "DO $$ BEGIN INSERT INTO a VALUES (1); CREATE INDEX shared_idx ON a (x); CREATE INDEX shared_idx ON b (x); END $$",
            &[],
        )
        .unwrap_err();
    assert_eq!(e.code, "42P07", "{}", e.message);
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_indexes WHERE indexname = 'shared_idx'"), [["0"]]);
}

#[test]
fn a_block_rolled_back_takes_its_unique_with_it() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "CREATE TABLE a (id int4 PRIMARY KEY, n int4); INSERT INTO a VALUES (1, 1)");
    // an EXCEPTION handler's block: what it built is undone, what the body did before stays
    run(
        &db,
        &mut s,
        "DO $$ BEGIN
           CREATE INDEX a_id_n ON a (id, n);
           BEGIN
             ALTER TABLE a ADD CONSTRAINT a_n_uq UNIQUE (n);
             RAISE EXCEPTION 'no';
           EXCEPTION WHEN OTHERS THEN NULL;
           END;
         END $$",
    );
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_constraint WHERE conname = 'a_n_uq'"), [["0"]]);
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_indexes WHERE indexname = 'a_id_n'"), [["1"]]);
    run(&db, &mut s, "INSERT INTO a VALUES (2, 1)");
    // a statement that fails: none of it
    assert!(s.run(&db, "DO $$ BEGIN CREATE UNIQUE INDEX a_n2 ON a (id); RAISE EXCEPTION 'no'; END $$", &[]).is_err());
    assert_eq!(run(&db, &mut s, "SELECT count(*) FROM pg_indexes WHERE indexname = 'a_n2'"), [["0"]]);
}
