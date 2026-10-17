//! `ORDER BY x USING < | >` and `unnest(a, b, ...)` in FROM. Postgres's answers are compared in
//! tests/pgdiff.rs (`order_by_using_and_unnest_of_several_arrays`); these run without one.

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
fn order_by_using_and_unnest_of_several_arrays() {
    script(&[
        (r"create table ob (id int, n text)", Ok(&[])),
        (r"insert into ob values (1,'b'),(2,null),(3,'a'),(4,'c')", Ok(&[])),
        (r"select id from ob order by n using <", Ok(&[r"3", r"1", r"4", r"2"])),
        (r"select id from ob order by n using >", Ok(&[r"2", r"4", r"1", r"3"])),
        (r"select id from ob order by n using > nulls last", Ok(&[r"4", r"1", r"3", r"2"])),
        (r"select id from ob order by n using < nulls first", Ok(&[r"2", r"3", r"1", r"4"])),
        (r"select id from ob order by n using <=", Err(r"42809")),
        (r"select id from ob order by n using =", Err(r"42809")),
        (r"select id from (values (1),(2),(3)) v(id) order by id using >", Ok(&[r"3", r"2", r"1"])),
        (r"select n, count(*) from ob group by n order by n using >", Ok(&[r"|1", r"c|1", r"b|1", r"a|1"])),
        (r"select array_agg(id order by n using >) from ob", Ok(&[r"{2,4,1,3}"])),
        (r"select id from ob order by id using >, n using <", Ok(&[r"4", r"3", r"2", r"1"])),
        (r"select string_agg(n, ',' order by n using >) from ob", Ok(&[r"c,b,a"])),
        (r"select unnest(array[1,2], array['a','b','c'])", Err(r"42883")),
        (r"select * from unnest(array[1,2], array['a','b','c']) as t(x,y)", Ok(&[r"1|a", r"2|b", r"|c"])),
        (r"select * from unnest(array[1,2,3]) with ordinality", Ok(&[r"1|1", r"2|2", r"3|3"])),
        (r"select * from unnest(array[1,2], array[3]) as t(a,b) where b is null", Ok(&[r"2|"])),
        (r"select * from unnest(array[1,2], array[3,4], array[5,6]) as t(a,b,c)", Ok(&[r"1|3|5", r"2|4|6"])),
        (r"select * from unnest(null::int[], array[1]) as t(a,b)", Ok(&[r"|1"])),
        (r"select a + b from unnest(array[1,2], array[10,20]) as t(a,b)", Ok(&[r"11", r"22"])),
        (r"select * from unnest(array[1,2], array['x','y']) with ordinality as t(a,b,n)", Ok(&[r"1|x|1", r"2|y|2"])),
        (r"select * from unnest('{1,2}'::int[], '{a,b}'::text[])", Ok(&[r"1|a", r"2|b"])),
    ]);
}
