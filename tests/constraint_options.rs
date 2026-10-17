//! `UNIQUE NULLS NOT DISTINCT`, `MATCH FULL` foreign keys, and `ON COMMIT DELETE ROWS | DROP` temporary tables.
//! Postgres's answers are compared in tests/pgdiff.rs (`nulls_not_distinct_match_full_and_on_commit`); these run without one.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

/// What a query gives back, a row a string, its columns joined with `|` and booleans as `t`/`f`
/// the way psql prints them.
fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    let text = |v: &Value| match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        Value::Bytea(b) => format!("\\x{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        v => v.to_text(),
    };
    out.last().unwrap().rows.iter().map(|r| r.iter().map(text).collect::<Vec<_>>().join("|")).collect()
}

/// Statements run in order in one session: a query's rows, or the SQLSTATE of the statement
/// Postgres refuses.
fn script(steps: &[(&str, Result<&[&str], &str>)]) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, want) in steps {
        match want {
            Ok(rows_want) => assert_eq!(&rows(&mut s, &db, q), rows_want, "{q}"),
            Err(code) => {
                let e = s.run(&db, q, &[]).expect_err(q);
                assert_eq!(&e.code, code, "{q}: {}", e.message);
            }
        }
    }
}

#[test]
fn nulls_not_distinct_match_full_and_on_commit() {
    script(&[
        (r"create table u2 (id int, a int, unique nulls not distinct (a))", Ok(&[])),
        (r"insert into u2 values (1, null)", Ok(&[])),
        (r"insert into u2 values (2, null)", Err(r"23505")),
        (r"insert into u2 values (3, 5)", Ok(&[])),
        (r"insert into u2 values (4, 5)", Err(r"23505")),
        (r"select id, a from u2 order by id", Ok(&[r"1|", r"3|5"])),
        (r"update u2 set a = null where id = 3", Err(r"23505")),
        (r"delete from u2 where id = 1", Ok(&[])),
        (r"insert into u2 values (5, null)", Ok(&[])),
        (r"select id, a from u2 order by id", Ok(&[r"3|5", r"5|"])),
        (r"create table u3 (a int, b int, unique (a, b) nulls not distinct)", Err(r"42601")),
        (r"insert into u3 values (1, null), (1, null)", Err(r"42P01")),
        (r"insert into u3 values (1, null)", Err(r"42P01")),
        (r"insert into u3 values (null, null), (2, null)", Err(r"42P01")),
        (r"insert into u3 values (null, null)", Err(r"42P01")),
        (r"create table u4 (a int unique nulls not distinct, b int unique nulls distinct, c int unique)", Ok(&[])),
        (r"insert into u4 values (null, null, null), (1, null, null)", Ok(&[])),
        (r"insert into u4 values (null, 1, 1)", Err(r"23505")),
        (r"insert into u4 values (2, null, null)", Ok(&[])),
        (r"create table u5 (a int, b text)", Ok(&[])),
        (r"create unique index u5i on u5 (a) nulls not distinct", Ok(&[])),
        (r"insert into u5 values (null, 'x')", Ok(&[])),
        (r"insert into u5 values (null, 'y')", Err(r"23505")),
        (r"create unique index u5j on u5 (b) nulls distinct", Ok(&[])),
        (r"create index u5k on u5 (a) nulls not distinct", Ok(&[])),
        (r"insert into u2 values (6, null) on conflict (a) do nothing", Ok(&[])),
        (r"select count(*) from u2", Ok(&[r"2"])),
        (r"insert into u2 values (7, null) on conflict (a) do update set id = 70", Ok(&[])),
        (r"select id, a from u2 order by id", Ok(&[r"3|5", r"70|"])),
        (r"create table u6 (a int)", Ok(&[])),
        (r"insert into u6 values (null), (null), (1)", Ok(&[])),
        (r"alter table u6 add constraint u6c unique nulls not distinct (a)", Err(r"23505")),
        (r"alter table u6 add constraint u6c unique nulls not distinct (a)", Err(r"23505")),
        (r"insert into u6 values (null)", Ok(&[])),
        (r"create table u7 (a int, constraint u7c unique nulls not distinct (a))", Ok(&[])),
        (r"insert into u7 values (null), (null)", Err(r"23505")),
        (r"create table u8 (a int, unique nulls distinct (a))", Ok(&[])),
        (r"insert into u8 values (null), (null)", Ok(&[])),
        (r"select count(*) from u8", Ok(&[r"2"])),
        (r"select indnullsnotdistinct from pg_index where indexrelid = 'u5i'::regclass", Ok(&[r"t"])),
        (
            r"select pg_get_indexdef('u5i'::regclass)",
            Ok(&[r"CREATE UNIQUE INDEX u5i ON public.u5 USING btree (a) NULLS NOT DISTINCT"]),
        ),
        (
            r"select conname, pg_get_constraintdef(oid) from pg_constraint where conname = 'u7c'",
            Ok(&[r"u7c|UNIQUE NULLS NOT DISTINCT (a)"]),
        ),
        (r"create table p (a int, b int, primary key (a, b))", Ok(&[])),
        (r"insert into p values (1, 1), (2, 2)", Ok(&[])),
        (
            r"create table c1 (id int primary key, x int, y int, foreign key (x, y) references p (a, b) match full)",
            Ok(&[]),
        ),
        (r"insert into c1 values (1, 1, 1)", Ok(&[])),
        (r"insert into c1 values (2, null, null)", Ok(&[])),
        (r"insert into c1 values (3, 1, null)", Err(r"23503")),
        (r"insert into c1 values (4, null, 2)", Err(r"23503")),
        (r"insert into c1 values (5, 3, 3)", Err(r"23503")),
        (r"update c1 set y = null where id = 1", Err(r"23503")),
        (r"update c1 set x = null, y = null where id = 1", Ok(&[])),
        (r"select id, x, y from c1 order by id", Ok(&[r"1||", r"2||"])),
        (
            r"create table c2 (id int primary key, x int, y int, foreign key (x, y) references p (a, b) match simple)",
            Ok(&[]),
        ),
        (r"insert into c2 values (1, 1, null)", Ok(&[])),
        (r"insert into c2 values (2, null, 7)", Ok(&[])),
        (
            r"create table c3 (id int primary key, x int, y int, foreign key (x, y) references p (a, b) match partial)",
            Err(r"0A000"),
        ),
        (
            r"create table c4 (id int primary key, x int, y int, constraint c4fk foreign key (x, y) references p match full on delete cascade)",
            Ok(&[]),
        ),
        (
            r"select conname, pg_get_constraintdef(oid) from pg_constraint where conname in ('c1_x_y_fkey','c4fk') order by 1",
            Ok(&[
                r"c1_x_y_fkey|FOREIGN KEY (x, y) REFERENCES p(a, b) MATCH FULL",
                r"c4fk|FOREIGN KEY (x, y) REFERENCES p(a, b) MATCH FULL ON DELETE CASCADE",
            ]),
        ),
        (r"alter table c2 add constraint c2f foreign key (x, y) references p (a, b) match full", Err(r"23503")),
        (r"delete from c2", Ok(&[])),
        (r"alter table c2 add constraint c2f foreign key (x, y) references p (a, b) match full", Ok(&[])),
        (r"insert into c2 values (3, 1, null)", Err(r"23503")),
        (r"begin", Ok(&[])),
        (
            r"create table c5 (id int primary key, x int, y int, foreign key (x, y) references p (a, b) match full deferrable initially deferred)",
            Ok(&[]),
        ),
        (r"insert into c5 values (1, 1, null)", Ok(&[])),
        (r"commit", Err(r"23503")),
        (r"delete from p where a = 1", Ok(&[])),
        (r"select count(*) from c1", Ok(&[r"2"])),
        (r"create temp table tt (a int) on commit delete rows", Ok(&[])),
        (r"begin", Ok(&[])),
        (r"insert into tt values (1)", Ok(&[])),
        (r"select count(*) from tt", Ok(&[r"1"])),
        (r"commit", Ok(&[])),
        (r"select count(*) from tt", Ok(&[r"0"])),
        (r"insert into tt values (2)", Ok(&[])),
        (r"select count(*) from tt", Ok(&[r"0"])),
        (r"begin", Ok(&[])),
        (r"insert into tt values (3)", Ok(&[])),
        (r"rollback", Ok(&[])),
        (r"select count(*) from tt", Ok(&[r"0"])),
        (r"begin", Ok(&[])),
        (r"insert into tt values (4), (5)", Ok(&[])),
        (r"savepoint s1", Ok(&[])),
        (r"insert into tt values (6)", Ok(&[])),
        (r"commit", Ok(&[])),
        (r"select count(*) from tt", Ok(&[r"0"])),
        (r"create temp table t2 (a int) on commit drop", Ok(&[])),
        (r"select count(*) from t2", Err(r"42P01")),
        (r"begin", Ok(&[])),
        (r"create temp table t3 (a int) on commit drop", Ok(&[])),
        (r"insert into t3 values (1)", Ok(&[])),
        (r"select count(*) from t3", Ok(&[r"1"])),
        (r"commit", Ok(&[])),
        (r"select count(*) from t3", Err(r"42P01")),
        (r"create temp table t4 (a int) on commit preserve rows", Ok(&[])),
        (r"insert into t4 values (1)", Ok(&[])),
        (r"select count(*) from t4", Ok(&[r"1"])),
        (r"create table notemp (a int) on commit drop", Err(r"42P16")),
        (r"create temp table t5 (a int) on commit bogus", Err(r"42601")),
        (r"begin", Ok(&[])),
        (r"create temp table t6 (a int) on commit delete rows", Ok(&[])),
        (r"insert into t6 values (1)", Ok(&[])),
        (r"rollback", Ok(&[])),
        (r"select count(*) from t6", Err(r"42P01")),
        (r"create temp table t7 (a int)", Ok(&[])),
        (r"insert into t7 values (1), (2)", Ok(&[])),
        (r"truncate table t7", Ok(&[])),
        (r"select count(*) from t7", Ok(&[r"0"])),
        (r"truncate t7, nosuch", Err(r"42P01")),
    ]);
}
