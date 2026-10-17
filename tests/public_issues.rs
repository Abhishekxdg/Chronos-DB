//! What the public repository's issues from running a Drizzle/Next.js app on 0.1.3 asked for and is now done, kept
//! working: DO blocks that make constraints and indexes (#14), ON CONFLICT on a partial unique index (#11), and
//! errors that say what Postgres's say, with the row they are about in the HINT (#22).

use chronos::Db;
use chronos::sql::{Session, SqlError};
use std::io::Write;
use std::process::{Command, Stdio};

fn ok(db: &Db, s: &mut Session, sql: &str) {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn fails(db: &Db, s: &mut Session, sql: &str) -> SqlError {
    s.run(db, sql, &[]).expect_err(sql)
}

fn one(db: &Db, s: &mut Session, sql: &str) -> String {
    let out = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    out.rows[0][0].to_text()
}

#[test]
fn do_blocks_make_unique_constraints_and_indexes() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    ok(&db, &mut s, "create table a (id int primary key, k text, n int)");
    ok(&db, &mut s, "insert into a values (1, 'x', 1)");
    // what drizzle-kit and hand-written migrations write: idempotent, so run twice
    for _ in 0..2 {
        ok(
            &db,
            &mut s,
            "do $$ begin alter table a add constraint a_k_uq unique (k); \
             exception when duplicate_object or duplicate_table then null; end $$",
        );
        ok(
            &db,
            &mut s,
            "do $$ begin create index if not exists a_n_idx on a (n); \
             create unique index if not exists a_kn on a (k, n); end $$",
        );
    }
    assert_eq!(fails(&db, &mut s, "insert into a values (2, 'x', 2)").code, "23505");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_indexes where tablename = 'a'"), "4");
    // a block that fails after making one leaves nothing made
    let e =
        fails(&db, &mut s, "do $$ begin alter table a add constraint a_n_uq unique (n); raise exception 'no'; end $$");
    assert_eq!(e.code, "P0001");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_constraint where conname = 'a_n_uq'"), "0");
    ok(&db, &mut s, "insert into a values (3, 'y', 1)");
}

#[test]
fn on_conflict_names_a_partial_unique_index_by_its_predicate() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    for sql in [
        "create table t (id int primary key, n text, v int, st text)",
        "create unique index t_live on t (n) where st = 'a'",
        "insert into t values (1, 'x', 1, 'a')",
        // the one live 'x' is there: nothing
        "insert into t values (2, 'x', 2, 'a') on conflict (n) where st = 'a' do nothing",
        // not live, so not under the index: it goes in
        "insert into t values (3, 'x', 3, 'b') on conflict (n) where st = 'a' do nothing",
        "insert into t values (4, 'x', 4, 'a') on conflict (n) where st = 'a' do update set v = excluded.v",
    ] {
        ok(&db, &mut s, sql);
    }
    assert_eq!(one(&db, &mut s, "select string_agg(id || ':' || v, ',' order by id) from t"), "1:4,3:3");
}

#[test]
fn constraint_errors_say_what_postgres_says() {
    let db = Db::new();
    let mut s = Session::new(chronos::MAIN);
    ok(
        &db,
        &mut s,
        "create table q (id int primary key, k text, r int references q (id), constraint q_k_uq unique (k))",
    );
    ok(&db, &mut s, "insert into q values (1, 'a', null)");
    let info = |e: &SqlError| e.info.clone().unwrap();

    let e = fails(&db, &mut s, "insert into q values (2, 'a', null)");
    assert_eq!((e.code, e.message.as_str()), ("23505", "duplicate key value violates unique constraint \"q_k_uq\""));
    let i = info(&e);
    assert_eq!((i.constraint.as_deref(), i.table.as_deref()), (Some("q_k_uq"), Some("q")));
    assert_eq!(i.detail.as_deref(), Some("Key (k)=(a) already exists."));
    assert_eq!(i.hint.as_deref(), Some("Row q/1 already has this key."), "the row holding it, Chronos's own");

    let e = fails(&db, &mut s, "insert into q values (3, 'b', 99)");
    assert_eq!(e.code, "23503");
    assert_eq!(e.message, "insert or update on table \"q\" violates foreign key constraint \"q_r_fkey\"");
    let i = info(&e);
    assert_eq!(i.constraint.as_deref(), Some("q_r_fkey"));
    assert_eq!(i.detail.as_deref(), Some("Key (r)=(99) is not present in table \"q\"."));
    assert!(i.hint.as_deref().is_some_and(|h| h.contains("isn't a row of q")), "{:?}", i.hint);

    ok(&db, &mut s, "insert into q values (4, 'c', 1)");
    let e = fails(&db, &mut s, "delete from q where id = 1");
    assert_eq!(e.code, "23503");
    assert_eq!(
        e.message,
        "update or delete on table \"q\" violates foreign key constraint \"q_r_fkey\" on table \"q\""
    );
    let hint = info(&e).hint;
    assert!(hint.as_deref().is_some_and(|h| h.contains("is still referenced from q/4")), "{hint:?}");
}

#[test]
fn the_shell_runs_do_blocks_as_sql() {
    let dir = std::env::temp_dir().join(format!("chronos-public-issues-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(b"do $$ begin perform 1; end $$;\n").unwrap();
    let o = c.wait_with_output().unwrap();
    let (out, err) = (String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("DO") && !err.contains("unknown command"), "{out}\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
