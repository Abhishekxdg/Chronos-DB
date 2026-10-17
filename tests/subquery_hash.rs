//! Correlated EXISTS, IN and scalar subqueries, and `x IN (subquery)` over a long list, are
//! answered from hash tables (src/sql/semi.rs) instead of one subquery run per outer row. The
//! answers must be the ones the per-row run gives, NULLs included.
//!
//! Each correlated query runs twice: as written, and with `random() < 2.0 and` put first in the
//! subquery's WHERE. That is always true, but a volatile function keeps a subquery on the per-row
//! path, so the second run is the reference. Both must agree. The speed itself is in
//! `subquery_perf.rs`.

use chronos::sql::{Value, run};
use chronos::{Db, MAIN};

fn sql(db: &Db, q: &str) -> chronos::sql::Output {
    run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {}", e.message)).pop().unwrap()
}

fn rows(db: &Db, q: &str) -> Vec<String> {
    sql(db, q).rows.iter().map(|r| format!("{r:?}")).collect()
}

/// `a` and `b` are the same rows; if not, the first rows that differ (not the whole lists).
fn agree(a: &[String], b: &[String], what: &str) {
    if let Some(i) = (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i)) {
        panic!("{what}: row {i} differs: {:?} and {:?} ({} and {} rows)", a.get(i), b.get(i), a.len(), b.len());
    }
}

/// 300 rows with NULLs and repeats in every column, no index but the key's. `k` repeats every
/// 40 rows, `n` every 25, `d` every 30; a multiple of 17, 13, 19, 11 or 23 has a NULL `k`, `n`,
/// `d`, `s` or `f`.
fn table(db: &Db, name: &str) {
    sql(db, &format!("create table {name} (id int primary key, k text, n int, d date, s text, f float)"));
    let v = |null: bool, x: String| if null { "null".to_string() } else { x };
    for chunk in (1..=300).collect::<Vec<i64>>().chunks(100) {
        let values: Vec<String> = chunk
            .iter()
            .map(|&i| {
                format!(
                    "({i}, {}, {}, {}, {}, {})",
                    v(i % 17 == 0, format!("'k{}'", i % 40)),
                    v(i % 13 == 0, (i % 25).to_string()),
                    v(i % 19 == 0, format!("date '2025-01-01' + {}", i % 30)),
                    v(i % 11 == 0, format!("'{}'", i % 25)),
                    // 2.0 and 3.5: equal to an integer column's value, and to none
                    v(i % 23 == 0, if i % 2 == 0 { format!("{}.0", i % 25) } else { format!("{}.5", i % 25) }),
                )
            })
            .collect();
        sql(db, &format!("insert into {name} values {}", values.join(", ")));
    }
}

/// `q` with `random() < 2.0 and` first in the WHERE of the subquery that reads `from` (as in
/// `from t l2`): always true, and volatile, so the subquery runs once for each outer row.
fn per_row(q: &str, from: &str) -> String {
    let at = q.find(from).unwrap_or_else(|| panic!("{from} not in {q}"));
    let w = at + q[at..].find(" where ").unwrap_or_else(|| panic!("no WHERE after {from} in {q}")) + " where ".len();
    format!("{}random() < 2.0 and {}", &q[..w], &q[w..])
}

/// `q` as written and as `per_row` runs it: the same rows. Returns how many.
fn same(db: &Db, q: &str) -> usize {
    let (a, b) = (rows(db, q), rows(db, &per_row(q, "from t l2")));
    agree(&a, &b, q);
    a.len()
}

#[test]
fn correlated_exists_matches_the_per_row_run() {
    let db = Db::new();
    table(&db, "t");
    // the demo's shape: an equality, and a comparison with the outer row. Every row of the first
    // 60 but the three with a NULL key (17, 34, 51: equal to nothing) has a later one with its key.
    let n = same(
        &db,
        "select id from t l1 where l1.id <= 60 and exists (select 1 from t l2 where l2.k = l1.k and l2.id > l1.id) order by id",
    );
    assert_eq!(n, 57);
    let n = same(
        &db,
        "select id from t l1 where l1.id <= 60 and not exists (select 1 from t l2 where l2.k = l1.k and l2.id > l1.id) order by id",
    );
    assert_eq!(n, 3);
    // the equality written the other way round, an unqualified outer column, a constant condition
    same(
        &db,
        "select id from t l1 where l1.id <= 60 and exists (select 1 from t l2 where l1.k = l2.k and l2.n < 5) order by id",
    );
    same(
        &db,
        "select id from t where id <= 60 and exists (select 1 from t l2 where l2.n = t.n and l2.id <> t.id) order by id",
    );
    // two equalities: a composite key
    same(
        &db,
        "select id from t l1 where l1.id <= 60 and exists (select 1 from t l2 where l2.k = l1.k and l2.n = l1.n and l2.id <> l1.id) order by id",
    );
    // a date key (no search index reads those)
    same(
        &db,
        "select id from t l1 where l1.id <= 60 and exists (select 1 from t l2 where l2.d = l1.d and l2.id <> l1.id) order by id",
    );
    // an OR leaves no equality of its own to hash by; two tables or a subquery inside are not of its shape
    same(
        &db,
        "select id from t l1 where l1.id <= 60 and exists (select 1 from t l2 where l2.k = l1.k or l2.id = l1.id) order by id",
    );
    same(
        &db,
        "select id from t l1 where l1.id <= 60 and exists \
         (select 1 from t l2 join t l3 on l3.id = l2.id where l2.k = l1.k and l3.id > l1.id) order by id",
    );
    same(
        &db,
        "select id from t l1 where l1.id <= 60 and exists \
         (select 1 from t l2 where l2.k = l1.k and l2.id in (select id from t where n = 1)) order by id",
    );
}

#[test]
fn correlated_values_match_the_per_row_run() {
    let db = Db::new();
    table(&db, "t");
    // an aggregate over no rows is a row: count 0 for a NULL key, NULL for max
    same(&db, "select id, (select count(*) from t l2 where l2.k = l1.k) from t l1 where id <= 60 order by id");
    same(
        &db,
        "select id, (select max(l2.id) from t l2 where l2.n = l1.n and l2.id < l1.id) from t l1 where id <= 60 order by id",
    );
    same(&db, "select id from t l1 where id <= 60 and n = (select max(l2.n) from t l2 where l2.k = l1.k) order by id");
    // IN and NOT IN over the matching rows: NULLs among them make NOT IN unknown
    same(&db, "select id, n in (select l2.n from t l2 where l2.k = l1.k) from t l1 where id <= 60 order by id");
    same(&db, "select id, n not in (select l2.n from t l2 where l2.k = l1.k) from t l1 where id <= 60 order by id");
    same(
        &db,
        "select id from t l1 where id <= 60 and n in (select l2.n from t l2 where l2.k = l1.k and l2.id <> l1.id) order by id",
    );
    // ORDER BY and LIMIT, DISTINCT inside
    same(
        &db,
        "select id, (select l2.id from t l2 where l2.k = l1.k order by l2.id desc limit 1) from t l1 where id <= 60 order by id",
    );
    same(
        &db,
        "select id, (select count(distinct l2.n) from t l2 where l2.k = l1.k) from t l1 where id <= 60 order by id",
    );
    // rows read in the order a scan gives them (no ORDER BY inside, so string_agg shows it), whether
    // the subquery's rows came from the table, the search index or a hash
    same(
        &db,
        "select id, (select string_agg(l2.id::text, ',') from t l2 where l2.k = l1.k) from t l1 where id <= 60 order by id",
    );
    same(
        &db,
        "select id, (select string_agg(l2.id::text, ',') from t l2 where l2.n = l1.n and l2.id > l1.id) from t l1 where id <= 60 order by id",
    );
}

#[test]
fn a_value_of_another_kind_than_the_column_is_compared_as_sql_does() {
    let db = Db::new();
    table(&db, "t");
    // an integer column against a text column ('5' = 5), against a float column (2.0 = 2); a text
    // column against an integer or a date: kinds a hash of one kind can't answer, which SQL compares
    // its own ways
    same(
        &db,
        "select id from t l1 where id <= 60 and exists (select 1 from t l2 where l2.n = l1.s and l2.id <> l1.id) order by id",
    );
    same(
        &db,
        "select id from t l1 where id <= 60 and exists (select 1 from t l2 where l2.n = l1.f and l2.id <> l1.id) order by id",
    );
    same(
        &db,
        "select id from t l1 where id <= 60 and exists (select 1 from t l2 where l2.s = l1.n and l2.id <> l1.id) order by id",
    );
    same(
        &db,
        "select id from t l1 where id <= 60 and exists (select 1 from t l2 where l2.f = l1.n and l2.id <> l1.id) order by id",
    );
    // what SQL refuses (a text 'k1' is no date) it refuses the same way through a hash
    let q = "select id from t l1 where id <= 60 and exists (select 1 from t l2 where l2.k = l1.d and l2.id <> l1.id)";
    let err = |q: &str| run(&db, MAIN, q, &[]).map(|_| ()).unwrap_err().message;
    assert_eq!(err(q), err(&per_row(q, "from t l2")));
}

#[test]
fn a_key_with_two_values_gives_the_same_rows() {
    let db = Db::new();
    sql(&db, "create table lo (id int primary key, g text, n int)");
    let values: Vec<String> = (1..=300).map(|i| format!("({i}, 'g{}', {})", i % 2, i % 7)).collect();
    sql(&db, &format!("insert into lo values {}", values.join(", ")));
    let q = "select id from lo l1 where id <= 60 and exists (select 1 from lo l2 where l2.g = l1.g and l2.n = l1.n + 1 and l2.id > l1.id) order by id";
    agree(&rows(&db, q), &rows(&db, &per_row(q, "from lo l2")), "two keys");
}

#[test]
fn a_write_reads_the_table_as_the_per_row_run_does() {
    let db = Db::new();
    for t in ["a", "b", "c", "d", "e", "f"] {
        table(&db, t);
    }
    // the demo's DELETE: every row that has a newer one with its key
    let del = |t: &str, per_row_run: bool| {
        let q = format!("delete from {t} l1 where exists (select 1 from {t} l2 where l2.k = l1.k and l2.id > l1.id)");
        sql(&db, &if per_row_run { per_row(&q, &format!("from {t} l2")) } else { q }).tag
    };
    assert_eq!(del("a", false), del("b", true));
    agree(&rows(&db, "select id from a order by id"), &rows(&db, "select id from b order by id"), "delete");
    // one row of each key is left, and every NULL-key row (it equals nothing, so nothing is newer)
    let left = sql(&db, "select count(*) from a").rows[0][0].clone();
    assert_eq!(left, Value::Int(40 + (1..=300).filter(|i| i % 17 == 0).count() as i64));
    // an UPDATE and an INSERT ... SELECT that write the table they read from: each outer row
    // reads the table as the per-row run does, with whatever the statement has written
    for (t, per_row_run) in [("c", false), ("d", true)] {
        let q = format!("update {t} set n = (select max(l2.n) from {t} l2 where l2.k = {t}.k and l2.id <> {t}.id)");
        sql(&db, &if per_row_run { per_row(&q, &format!("from {t} l2")) } else { q });
    }
    agree(&rows(&db, "select id, n from c order by id"), &rows(&db, "select id, n from d order by id"), "update");
    for (t, per_row_run) in [("e", false), ("f", true)] {
        let q = format!(
            "insert into {t} select id + 1000, k, n, d, s, f from {t} l1 \
             where exists (select 1 from {t} l2 where l2.k = l1.k and l2.id > l1.id)"
        );
        sql(&db, &if per_row_run { per_row(&q, &format!("from {t} l2")) } else { q });
    }
    agree(&rows(&db, "select id from e order by id"), &rows(&db, "select id from f order by id"), "insert");
}

/// A BEFORE ROW trigger runs between the SET subqueries of two rows: what it writes to the table
/// they read is there for the next row, and a hash of that table made before it would not have it.
#[test]
fn what_a_trigger_writes_mid_statement_is_read() {
    let db = Db::new();
    for t in ["a", "b"] {
        table(&db, &format!("t_{t}"));
        sql(&db, &format!("create table log_{t} (id int primary key, k text)"));
        sql(&db, &format!("insert into log_{t} select id + 5000, k from t_{t}"));
        sql(
            &db,
            &format!(
                "create function bump_{t}() returns trigger language plpgsql as $$ \
                 begin insert into log_{t} values (new.id, new.k); return new; end $$"
            ),
        );
        sql(&db, &format!("create trigger bump_{t} before update on t_{t} for each row execute function bump_{t}()"));
    }
    // (each row's `id` makes its outer values its own, so no answer is reused from another row)
    let q = "update t_a set n = (select count(*) from log_a l2 where l2.k = t_a.k and l2.id <> t_a.id)";
    sql(&db, q);
    sql(&db, &per_row(&q.replace("t_a", "t_b").replace("log_a", "log_b"), "from log_b l2"));
    agree(&rows(&db, "select id, n from t_a order by id"), &rows(&db, "select id, n from t_b order by id"), "trigger");
    // the counts did grow as rows were updated: row 1 saw its key's 8 seeded rows, row 281 those and the 6 rows before it
    let n = |id: i64| match &sql(&db, &format!("select n from t_a where id = {id}")).rows[0][0] {
        Value::Int(n) => *n,
        v => panic!("{v:?}"),
    };
    assert!(n(281) > n(1), "{} {}", n(281), n(1));
}

#[test]
fn a_new_statement_reads_the_table_as_it_is_then() {
    let db = Db::new();
    table(&db, "t");
    let q = "select count(*) from t l1 where exists (select 1 from t l2 where l2.k = l1.k and l2.id <> l1.id)";
    let before = sql(&db, q).rows[0][0].clone();
    // a lone 'zz' row has no partner, a second one makes two rows that have
    sql(&db, "insert into t values (1001, 'zz', 1, null, null, null)");
    assert_eq!(sql(&db, q).rows[0][0], before);
    sql(&db, "insert into t values (1002, 'zz', 1, null, null, null)");
    let Value::Int(b) = before else { panic!() };
    assert_eq!(sql(&db, q).rows[0][0], Value::Int(b + 2));
    // a row deleted since is gone from the next statement's table
    sql(&db, "delete from t where id in (1001, 1002)");
    assert_eq!(sql(&db, q).rows[0][0], Value::Int(b));
}

#[test]
fn an_indexed_key_gives_the_same_rows() {
    let db = Db::new();
    table(&db, "t");
    sql(&db, "create index t_k on t (k)");
    same(
        &db,
        "select id from t l1 where l1.id <= 60 and exists (select 1 from t l2 where l2.k = l1.k and l2.id > l1.id) order by id",
    );
}

/// `x [NOT] IN (subquery)` as SQL's three values say, for the Rust side to compare with.
fn is_in(x: Option<i64>, list: &[Option<i64>], negated: bool) -> Value {
    let (yes, no) = (Value::Bool(!negated), Value::Bool(negated));
    match x {
        None if list.is_empty() => no,
        None => Value::Null,
        Some(x) if list.contains(&Some(x)) => yes,
        Some(_) if list.contains(&None) => Value::Null,
        Some(_) => no,
    }
}

#[test]
fn in_over_a_long_list_keeps_nulls_unknown() {
    let db = Db::new();
    // two lists of 40: one with NULLs among its values, one without
    for t in ["u", "w", "o"] {
        sql(&db, &format!("create table {t} (id int primary key, v int)"));
    }
    let (mut with_null, mut without) = (vec![], vec![]);
    for i in 1..=40i64 {
        let a = if i % 10 == 0 { None } else { Some(i * 2) };
        with_null.push(a);
        without.push(Some(i * 2));
        sql(&db, &format!("insert into u values ({i}, {})", a.map_or("null".to_string(), |v| v.to_string())));
        sql(&db, &format!("insert into w values ({i}, {})", i * 2));
    }
    // outer values: in the list, not in it, NULL
    let xs: Vec<Option<i64>> =
        vec![Some(2), Some(4), Some(40), Some(80), Some(3), Some(81), Some(1000), None, Some(60), Some(61)];
    for (i, x) in xs.iter().enumerate() {
        sql(&db, &format!("insert into o values ({}, {})", i + 1, x.map_or("null".to_string(), |v| v.to_string())));
    }
    for (table, list) in [("u", &with_null), ("w", &without)] {
        for negated in [false, true] {
            let op = if negated { "not in" } else { "in" };
            let out = sql(&db, &format!("select id, v {op} (select v from {table}) from o order by id"));
            for (r, x) in out.rows.iter().zip(&xs) {
                assert_eq!(r[1], is_in(*x, list, negated), "{x:?} {op} {table}");
            }
            // WHERE: an unknown is not a match, for IN or NOT IN
            let got = sql(&db, &format!("select id from o where v {op} (select v from {table}) order by id")).rows;
            let want: Vec<_> = xs
                .iter()
                .enumerate()
                .filter(|(_, x)| is_in(**x, list, negated) == Value::Bool(true))
                .map(|(i, _)| vec![Value::Int(i as i64 + 1)])
                .collect();
            assert_eq!(got, want, "where v {op} {table}");
        }
    }
    // no rows at all: IN is false and NOT IN true, for NULL too
    let out = sql(
        &db,
        "select id, v in (select v from u where id > 100), v not in (select v from u where id > 100) from o order by id",
    );
    for r in &out.rows {
        assert_eq!((&r[1], &r[2]), (&Value::Bool(false), &Value::Bool(true)));
    }
}

#[test]
fn in_over_a_long_list_of_another_kind_than_x() {
    let db = Db::new();
    sql(&db, "create table u (id int primary key, w text)");
    sql(&db, "create table o (id int primary key, x int, t text, f float)");
    // text values that are numbers in their own way: '04' is 4 as a number
    for i in 1..=30i64 {
        let w = if i % 2 == 0 { format!("{i:02}") } else { i.to_string() };
        sql(&db, &format!("insert into u values ({i}, '{w}')"));
    }
    sql(
        &db,
        "insert into o values (1, 3, '3', 3.0), (2, 4, '4', 4.5), (3, 99, 'zz', 99.0), (4, null, null, null), \
         (5, 30, '30', 30.0), (6, 6, '6', 6.0), (7, 7, '07', 7.0)",
    );
    // a number against a list of text is refused, as in Postgres (integer = text has no operator); cast, the
    // list compares as numbers ('04' is 4); text against text is exact ('07' is not '7')
    for col in ["x", "f"] {
        let e = run(&db, MAIN, &format!("select id from o where {col} in (select w from u)"), &[]).unwrap_err();
        assert_eq!(e.code, "42883", "{col}: {}", e.message);
    }
    let ids = |q: &str| rows(&db, &format!("select id from o where {q} order by id"));
    assert_eq!(ids("x in (select w::int from u)"), ["[Int(1)]", "[Int(2)]", "[Int(5)]", "[Int(6)]", "[Int(7)]"]);
    assert_eq!(ids("t in (select w from u)"), ["[Int(1)]", "[Int(5)]"]);
    assert_eq!(ids("f in (select w::float from u)"), ["[Int(1)]", "[Int(5)]", "[Int(6)]", "[Int(7)]"]);
}
