//! `CREATE VIEW ... WITH [LOCAL | CASCADED] CHECK OPTION`: a row written through the view (INSERT or UPDATE) has to show in
//! it (44000 otherwise), LOCAL checking only the view's own condition and the ones of views below it that have an
//! option, CASCADED all of them. Postgres's answers are compared in tests/pgdiff.rs (`view_check_option`); these run
//! without one.

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
fn views_with_check_option_answer_as_postgres_does() {
    script(&[
        (r"create table vb (id int primary key, a int, v text)", Ok(&[])),
        (r"insert into vb values (1, 10, 'x'), (2, 200, 'y')", Ok(&[])),
        (r"create view v_plain as select * from vb where a < 100", Ok(&[])),
        (r"create view v_local as select * from vb where a < 100 with local check option", Ok(&[])),
        (r"create view v_casc as select * from vb where a < 100 with cascaded check option", Ok(&[])),
        (r"create view v_def as select * from vb where a < 100 with check option", Ok(&[])),
        (r"insert into v_plain values (3, 500, 'z')", Ok(&[])),
        (r"insert into v_local values (4, 50, 'ok')", Ok(&[])),
        (r"insert into v_local values (5, 500, 'bad')", Err(r"44000")),
        (r"insert into v_casc values (6, 500, 'bad')", Err(r"44000")),
        (r"insert into v_def values (7, 500, 'bad')", Err(r"44000")),
        (r"insert into v_casc values (8, 60, 'ok')", Ok(&[])),
        (r"update v_local set a = 999 where id = 4", Err(r"44000")),
        (r"update v_local set a = 55 where id = 4", Ok(&[])),
        (r"update v_casc set a = 1000 where id = 8", Err(r"44000")),
        (r"update v_plain set a = 1000 where id = 8", Ok(&[])),
        (r"select id, a from vb order by id", Ok(&[r"1|10", r"2|200", r"3|500", r"4|55", r"8|1000"])),
        (r"insert into v_local values (9, 99, 'edge'), (10, 100, 'edge2')", Err(r"44000")),
        (r"select id from vb where id in (9, 10) order by id", Ok(&[])),
        (r"create view v_outer_local as select * from v_local where a > 5 with local check option", Ok(&[])),
        (r"create view v_outer_casc as select * from v_plain where a > 5 with cascaded check option", Ok(&[])),
        (r"create view v_outer_none_over_casc as select * from v_casc where a > 5", Ok(&[])),
        (r"insert into v_outer_local values (11, 3, 'low')", Err(r"44000")),
        (r"insert into v_outer_local values (12, 500, 'hi')", Err(r"44000")),
        (r"insert into v_outer_casc values (13, 500, 'hi')", Err(r"44000")),
        (r"insert into v_outer_casc values (14, 3, 'lo')", Err(r"44000")),
        (r"insert into v_outer_none_over_casc values (15, 500, 'x')", Err(r"44000")),
        (r"insert into v_outer_none_over_casc values (16, 3, 'x')", Ok(&[])),
        (r"insert into v_outer_none_over_casc values (17, 50, 'fine')", Ok(&[])),
        (r"select id from vb where id >= 11 order by id", Ok(&[r"16", r"17"])),
        (r"create materialized view mv_chk as select * from vb with check option", Err(r"42601")),
        (r"create view v_agg as select a, count(*) from vb group by a with check option", Err(r"0A000")),
        (
            r"select table_name, check_option, is_updatable from information_schema.views where table_name like 'v\_%' order by 1",
            Ok(&[
                r"v_casc|CASCADED|YES",
                r"v_def|CASCADED|YES",
                r"v_local|LOCAL|YES",
                r"v_outer_casc|CASCADED|YES",
                r"v_outer_local|LOCAL|YES",
                r"v_outer_none_over_casc|NONE|YES",
                r"v_plain|NONE|YES",
            ]),
        ),
        (r"insert into v_local (id, a) values (20, null)", Err(r"44000")),
        (r"delete from v_local where id = 4", Ok(&[])),
        (r"insert into v_local values (21, 10, 'r') returning id, a", Ok(&[r"21|10"])),
        (r"create or replace view v_local as select * from vb where a < 50 with local check option", Ok(&[])),
        (r"insert into v_local values (22, 60, 'bad2')", Err(r"44000")),
    ]);
}
