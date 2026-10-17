//! Two things ORMs write that Postgres accepts: `GROUP BY` a table's primary key while selecting its other columns
//! (they follow from the key), and `RETURNING (xmax = 0) AS inserted` on an upsert, to tell the rows it made from the
//! ones it changed (with `xmin`, `xmax`, `cmin` and `cmax` readable as system columns). Postgres's answers are compared
//! in tests/pgdiff.rs (`group_by_primary_key_covers_the_rest`, `upsert_tells_inserted_from_updated`); these run
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
fn group_by_primary_key_covers_the_rest_as_postgres_does() {
    script(&[
        (r"create table u (id int primary key, name text, age int)", Ok(&[])),
        (r"create table o (id int primary key, uid int, amt int)", Ok(&[])),
        (r"create table k2 (a int, b int, c text, primary key (a, b))", Ok(&[])),
        (r"insert into u values (1,'ann',30),(2,'bo',40),(3,'cy',50)", Ok(&[])),
        (r"insert into o values (1,1,10),(2,1,20),(3,2,5)", Ok(&[])),
        (r"insert into k2 values (1,1,'x'),(1,2,'y'),(2,1,'z')", Ok(&[])),
        (r"select id, name, count(*) from u group by id order by id", Ok(&[r"1|ann|1", r"2|bo|1", r"3|cy|1"])),
        (
            r"select u.id, u.name, u.age, sum(o.amt) from u left join o on o.uid = u.id group by u.id order by u.id",
            Ok(&[r"1|ann|30|30", r"2|bo|40|5", r"3|cy|50|"]),
        ),
        (
            r"select u.name, count(o.id) from u left join o on o.uid = u.id group by u.id order by 1",
            Ok(&[r"ann|2", r"bo|1", r"cy|0"]),
        ),
        (r"select a, b, c, count(*) from k2 group by a, b order by a, b", Ok(&[r"1|1|x|1", r"1|2|y|1", r"2|1|z|1"])),
        (r"select a, c, count(*) from k2 group by a", Err(r"42803")),
        (r"select c, count(*) from k2 group by a, b order by c", Ok(&[r"x|1", r"y|1", r"z|1"])),
        (r"select id, name from u group by name", Err(r"42803")),
        (r"select u.id, o.id from u join o on o.uid = u.id group by u.id", Err(r"42803")),
        (
            r"select u.id, o.id from u join o on o.uid = u.id group by u.id, o.id order by 1, 2",
            Ok(&[r"1|1", r"1|2", r"2|3"]),
        ),
        (r"select name, max(age) from u group by id having max(age) > 35 order by name", Ok(&[r"bo|40", r"cy|50"])),
        (
            r"select id, name, age + 1 as a1, upper(name) from u group by id order by id",
            Ok(&[r"1|ann|31|ANN", r"2|bo|41|BO", r"3|cy|51|CY"]),
        ),
        (r"select id, name, count(*) over () from u group by id order by id", Ok(&[r"1|ann|3", r"2|bo|3", r"3|cy|3"])),
        (
            r"select (select name from u u2 where u2.id = u.id), count(*) from u group by u.id order by 1",
            Ok(&[r"ann|1", r"bo|1", r"cy|1"]),
        ),
        (r"select x.id, x.name, count(*) from (select * from u) x group by x.id", Err(r"42803")),
        (r"select id, name from (select id, name from u) x group by id", Err(r"42803")),
        (r"select id, name, count(*) from u group by 1 order by 1", Ok(&[r"1|ann|1", r"2|bo|1", r"3|cy|1"])),
        (r"select u.*, count(*) from u group by u.id order by u.id", Ok(&[r"1|ann|30|1", r"2|bo|40|1", r"3|cy|50|1"])),
        (r"with c as (select id, name from u) select id, name, count(*) from c group by id", Err(r"42803")),
        (r"select name, count(*) from u group by id, age order by 1", Ok(&[r"ann|1", r"bo|1", r"cy|1"])),
        (r"select coalesce(name, 'z'), count(*) from u group by id order by 1", Ok(&[r"ann|1", r"bo|1", r"cy|1"])),
        (
            r"select id, array_agg(amt order by o.id) from u join o on o.uid = u.id group by u.id order by 1",
            Err(r"42702"),
        ),
    ]);
}

#[test]
fn upsert_xmax_tells_inserted_from_updated_as_postgres_does() {
    script(&[
        (r"create table inv (sku text primary key, qty int, note text)", Ok(&[])),
        (r"insert into inv values ('A', 1, 'x')", Ok(&[])),
        (
            r"insert into inv (sku, qty) values ('A', 5) on conflict (sku) do update set qty = inv.qty + excluded.qty returning sku, qty, (xmax = 0) as inserted",
            Ok(&[r"A|6|f"]),
        ),
        (
            r"insert into inv (sku, qty) values ('B', 7) on conflict (sku) do update set qty = inv.qty + excluded.qty returning sku, qty, (xmax = 0) as inserted",
            Ok(&[r"B|7|t"]),
        ),
        (
            r"insert into inv (sku, qty) values ('A', 1), ('C', 2), ('B', 3) on conflict (sku) do update set qty = excluded.qty returning sku, (xmax = 0) as inserted",
            Ok(&[r"A|f", r"C|t", r"B|f"]),
        ),
        (
            r"insert into inv (sku, qty) values ('A', 1) on conflict (sku) do update set qty = 9 returning *, (xmax = 0) as inserted",
            Ok(&[r"A|9|x|f"]),
        ),
        (
            r"insert into inv (sku, qty) values ('D', 1) on conflict (sku) do update set qty = 9 returning inv.*, xmax::text::bigint = 0 as ins",
            Ok(&[r"D|1||t"]),
        ),
        (
            r"insert into inv (sku, qty) values ('D', 1) on conflict (sku) do nothing returning sku, (xmax = 0) as inserted",
            Ok(&[]),
        ),
        (r"insert into inv (sku, qty) values ('E', 1) returning sku, (xmax = 0) as inserted", Ok(&[r"E|t"])),
        (r"update inv set qty = 0 where sku = 'E' returning sku, (xmax = 0) as fresh", Ok(&[r"E|t"])),
        (r"select case when xmax = 0 then 'new' else 'upd' end from inv where sku = 'E'", Ok(&[r"new"])),
    ]);
}
