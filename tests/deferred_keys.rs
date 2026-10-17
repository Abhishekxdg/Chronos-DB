//! DEFERRABLE foreign keys: checked when the transaction ends, or at SET CONSTRAINTS ... IMMEDIATE
//! (their answers are checked against Postgres by `pgdiff.rs`; this is what it can't reach).

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn tag(db: &Db, s: &mut Session, sql: &str) -> String {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap().tag
}

fn code(db: &Db, s: &mut Session, sql: &str) -> &'static str {
    s.run(db, sql, &[]).expect_err(sql).code
}

fn count(db: &Db, sql: &str) -> String {
    run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"))[0].rows[0][0].to_text()
}

#[test]
fn a_deferred_key_shows_in_the_catalogs_and_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!("chronos-deferred-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let defs = |db: &Db| {
        run(
            db,
            MAIN,
            "select conname, condeferrable, condeferred, pg_get_constraintdef(oid) from pg_constraint where contype = 'f' order by 1",
            &[],
        )
        .unwrap()[0]
            .rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
    };
    let want = [
        "b_a_fkey|true|true|FOREIGN KEY (a) REFERENCES a(id) DEFERRABLE INITIALLY DEFERRED",
        "c_a_fkey|true|false|FOREIGN KEY (a) REFERENCES a(id) DEFERRABLE",
        "d_a_fkey|false|false|FOREIGN KEY (a) REFERENCES a(id)",
    ];
    {
        let db = Db::open(&dir).unwrap();
        for q in [
            "create table a (id int primary key)",
            "create table b (id int primary key, a int references a(id) deferrable initially deferred)",
            "create table c (id int primary key, a int references a(id) deferrable)",
            "create table d (id int primary key, a int references a(id) not deferrable)",
        ] {
            run(&db, MAIN, q, &[]).unwrap();
        }
        assert_eq!(defs(&db), want);
        db.close().unwrap();
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(defs(&db), want, "as stored: the deferral is part of the table");
    // and still works: the parent may come second
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "begin");
    tag(&db, &mut s, "insert into b values (1, 5)");
    tag(&db, &mut s, "insert into a values (5)");
    tag(&db, &mut s, "commit");
    assert_eq!(count(&db, "select count(*) from b"), "1");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_commit_leaves_nothing_and_the_session_carries_on() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "create table p (id int primary key)");
    tag(&db, &mut s, "create table c (id int primary key, p int references p(id) deferrable initially deferred)");
    tag(&db, &mut s, "insert into p values (1)");
    tag(&db, &mut s, "begin");
    tag(&db, &mut s, "insert into c values (1, 1)");
    tag(&db, &mut s, "insert into c values (2, 99)");
    assert_eq!(code(&db, &mut s, "commit"), "23503");
    assert_eq!(count(&db, "select count(*) from c"), "0", "the whole transaction went");
    // the session is out of it, and what it put off doesn't follow it into the next
    tag(&db, &mut s, "begin");
    tag(&db, &mut s, "insert into c values (3, 1)");
    tag(&db, &mut s, "commit");
    assert_eq!(count(&db, "select count(*) from c"), "1");

    // a failed statement's notes don't matter: what counts is how the rows end up
    tag(&db, &mut s, "begin");
    tag(&db, &mut s, "insert into c values (4, 77)");
    assert_eq!(code(&db, &mut s, "insert into c values (3, 1)"), "23505");
    assert_eq!(tag(&db, &mut s, "commit"), "ROLLBACK", "a failed transaction can only roll back");
    assert_eq!(count(&db, "select count(*) from c"), "1", "and its put-off row went with it");
}

#[test]
fn set_constraints_reaches_the_keys_it_names_and_ends_with_the_transaction() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    tag(&db, &mut s, "create table p (id int primary key)");
    tag(&db, &mut s, "create table c1 (id int primary key, p int references p(id) deferrable)");
    tag(&db, &mut s, "create table c2 (id int primary key, p int references p(id) deferrable)");
    tag(&db, &mut s, "begin");
    tag(&db, &mut s, "set constraints c1_p_fkey deferred");
    tag(&db, &mut s, "insert into c1 values (1, 9)"); // put off
    assert_eq!(code(&db, &mut s, "insert into c2 values (1, 9)"), "23503", "c2's is still at once");
    tag(&db, &mut s, "rollback");
    // its effect is gone with the transaction
    tag(&db, &mut s, "begin");
    assert_eq!(code(&db, &mut s, "insert into c1 values (1, 9)"), "23503");
    tag(&db, &mut s, "rollback");

    // the last SET CONSTRAINTS wins, ALL included, and IMMEDIATE checks what was put off
    tag(&db, &mut s, "begin");
    tag(&db, &mut s, "set constraints all deferred");
    tag(&db, &mut s, "insert into c1 values (1, 9)");
    tag(&db, &mut s, "insert into c2 values (1, 8)");
    // c2's row is checked now, and 8 isn't a parent; c1's, put off with it, is not
    assert_eq!(code(&db, &mut s, "set constraints c2_p_fkey immediate"), "23503");
    assert_eq!(code(&db, &mut s, "select 1"), "25P02", "an error in a transaction ends it");
    tag(&db, &mut s, "rollback");

    // what a key does when its parent is deleted still holds: RESTRICT isn't put off
    tag(
        &db,
        &mut s,
        "create table r (id int primary key, p int references p(id) on delete restrict deferrable initially deferred)",
    );
    tag(&db, &mut s, "insert into p values (1)");
    tag(&db, &mut s, "insert into r values (1, 1)");
    tag(&db, &mut s, "begin");
    assert_eq!(code(&db, &mut s, "delete from p where id = 1"), "23503");
    tag(&db, &mut s, "rollback");
}
