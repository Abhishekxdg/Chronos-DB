//! Correlated EXISTS, NOT EXISTS and IN answered from a hash table (see `sql::semi`): the answers
//! are the ones a subquery per outer row gives, nulls included. Each query's answer is checked
//! against a brute-force reading of its SQL over the same rows, with the subquery's column
//! indexed and not (an indexed one keeps running per outer row).

use chronos::sql::{Value, run};
use chronos::{Db, MAIN};

#[derive(Clone, Debug)]
struct R {
    id: i64,
    g: Option<i64>,
    h: Option<String>,
    v: Option<i64>,
    d: Option<i64>, // days after 2026-01-01
}

struct Lcg(u64);
impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
    fn maybe<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> Option<T> {
        if self.below(7) == 0 { None } else { Some(f(self)) }
    }
}

fn rows(n: i64) -> Vec<R> {
    let mut r = Lcg(7);
    (1..=n)
        .map(|id| R {
            id,
            g: r.maybe(|r| r.below(12) as i64),
            h: r.maybe(|r| ["Ab", "aB", "cd", "CD", "ef"][r.below(5) as usize].to_string()),
            v: r.maybe(|r| r.below(9) as i64),
            d: r.maybe(|r| r.below(20) as i64),
        })
        .collect()
}

fn sql_of<T: ToString>(v: &Option<T>, quote: bool) -> String {
    match v {
        None => "null".into(),
        Some(x) if quote => format!("'{}'", x.to_string()),
        Some(x) => x.to_string(),
    }
}

fn load(rows: &[R], index: bool) -> Db {
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, g int, h text, v int, d date)", &[]).unwrap();
    if index {
        run(&db, MAIN, "create index t_g on t (g)", &[]).unwrap();
        run(&db, MAIN, "create index t_h on t (h)", &[]).unwrap();
        run(&db, MAIN, "create index t_lh on t (lower(h))", &[]).unwrap();
    }
    for chunk in rows.chunks(50) {
        let values: Vec<String> = chunk
            .iter()
            .map(|r| {
                let d = r.d.map_or("null".to_string(), |d| format!("date '2026-01-01' + {d}"));
                format!("({}, {}, {}, {}, {d})", r.id, sql_of(&r.g, false), sql_of(&r.h, true), sql_of(&r.v, false))
            })
            .collect();
        let sql = format!("insert into t values {}", values.join(", "));
        run(&db, MAIN, &sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    }
    db
}

fn ids(db: &Db, sql: &str) -> Vec<i64> {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap();
    let mut v: Vec<i64> = out
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Int(n) => *n,
            other => panic!("{sql}: {other:?}"),
        })
        .collect();
    v.sort();
    v
}

/// SQL's three-valued AND, equality and ordering over nulls.
fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}
fn eq<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> Option<bool> {
    Some(a.as_ref()? == b.as_ref()?)
}
fn gt<T: PartialOrd>(a: &Option<T>, b: &Option<T>) -> Option<bool> {
    Some(a.as_ref()? > b.as_ref()?)
}
fn lower(s: &Option<String>) -> Option<String> {
    s.as_ref().map(|s| s.to_lowercase())
}

/// The ids of rows `a` for which some row `b` makes `cond` true (`want`), or none does (`!want`).
fn exists(rows: &[R], want: bool, cond: impl Fn(&R, &R) -> Option<bool>) -> Vec<i64> {
    rows.iter().filter(|a| rows.iter().any(|b| cond(a, b) == Some(true)) == want).map(|a| a.id).collect()
}

/// `a.v [NOT] IN (select b.v from t b where cond)`: true only when it's true, not null.
fn in_list(rows: &[R], negated: bool, cond: impl Fn(&R, &R) -> Option<bool>) -> Vec<i64> {
    rows.iter()
        .filter(|a| {
            let list: Vec<&Option<i64>> = rows.iter().filter(|b| cond(a, b) == Some(true)).map(|b| &b.v).collect();
            let found = match &a.v {
                None => {
                    if list.is_empty() {
                        Some(false)
                    } else {
                        None
                    }
                }
                Some(x) => {
                    if list.iter().any(|y| **y == Some(*x)) {
                        Some(true)
                    } else if list.iter().any(|y| y.is_none()) {
                        None
                    } else {
                        Some(false)
                    }
                }
            };
            found.map(|f| f != negated) == Some(true)
        })
        .map(|a| a.id)
        .collect()
}

#[test]
fn correlated_subqueries_answer_as_one_per_outer_row_does() {
    let data = rows(300);
    for index in [false, true] {
        let db = load(&data, index);
        let q = |sql: &str, want: Vec<i64>| {
            let got = ids(&db, sql);
            assert_eq!(got, want, "{sql} (index: {index})");
            // a volatile condition keeps the subquery running per outer row: the same answer
            let per_row = sql.replace(" from t b where ", " from t b where random() >= 0 and ");
            assert_ne!(per_row, sql);
            assert_eq!(ids(&db, &per_row), want, "{per_row} (index: {index})");
            want.len()
        };
        // each is also a real answer: not all rows, not none
        let mut sizes = vec![];
        // an equality alone: a row is its own match, so every row with a g
        sizes.push(q(
            "select id from t a where exists (select 1 from t b where b.g = a.g)",
            exists(&data, true, |a, b| eq(&a.g, &b.g)),
        ));
        // reversed, and with a condition on the outer row's other columns
        sizes.push(q(
            "select id from t a where exists (select 1 from t b where a.g = b.g and b.id <> a.id)",
            exists(&data, true, |a, b| and(eq(&a.g, &b.g), Some(a.id != b.id))),
        ));
        // an inequality over the outer row too
        sizes.push(q(
            "select id from t a where exists (select 1 from t b where b.g = a.g and b.v > a.v)",
            exists(&data, true, |a, b| and(eq(&a.g, &b.g), gt(&b.v, &a.v))),
        ));
        // NOT EXISTS: a null g equals nothing, so those rows stay
        sizes.push(q(
            "select id from t a where not exists (select 1 from t b where b.g = a.g and b.v > a.v)",
            exists(&data, false, |a, b| and(eq(&a.g, &b.g), gt(&b.v, &a.v))),
        ));
        // the newest row per g, ties broken by id
        sizes.push(q(
            "select id from t a where d is not null and not exists (select 1 from t b where b.g = a.g \
             and (b.d > a.d or (b.d = a.d and b.id > a.id)))",
            exists(&data, false, |a, b| {
                and(eq(&a.g, &b.g), or(gt(&b.d, &a.d), and(eq(&b.d, &a.d), Some(b.id > a.id))))
            })
            .into_iter()
            .filter(|id| data[*id as usize - 1].d.is_some())
            .collect(),
        ));
        // two equalities, one of them over an expression of each side
        sizes.push(q(
            "select id from t a where exists (select 1 from t b where b.g = a.g and lower(b.h) = lower(a.h) and b.id <> a.id)",
            exists(&data, true, |a, b| and(and(eq(&a.g, &b.g), eq(&lower(&b.h), &lower(&a.h))), Some(a.id != b.id))),
        ));
        sizes.push(q(
            "select id from t a where exists (select 1 from t b where b.g = a.g + 1)",
            exists(&data, true, |a, b| eq(&b.g, &a.g.map(|g| g + 1))),
        ));
        // a condition on the subquery's own rows alone goes inside, once
        sizes.push(q(
            "select id from t a where exists (select 1 from t b where b.g = a.g and b.h = 'cd' and b.v >= 4)",
            exists(&data, true, |a, b| {
                and(and(eq(&a.g, &b.g), eq(&b.h, &Some("cd".to_string()))), b.v.map(|v| v >= 4))
            }),
        ));
        // dates, and a number against a float
        sizes.push(q(
            "select id from t a where exists (select 1 from t b where b.d = a.d and b.id < a.id)",
            exists(&data, true, |a, b| and(eq(&a.d, &b.d), Some(b.id < a.id))),
        ));
        sizes.push(q(
            "select id from t a where exists (select 1 from t b where b.g = a.g * 1.0 and b.id < a.id)",
            exists(&data, true, |a, b| and(eq(&a.g, &b.g), Some(b.id < a.id))),
        ));
        // IN: the subquery's rows by the outer row's g; nulls in the list make NOT IN unknown
        sizes.push(q(
            "select id from t a where a.v in (select b.v from t b where b.g = a.g and b.id <> a.id)",
            in_list(&data, false, |a, b| and(eq(&a.g, &b.g), Some(a.id != b.id))),
        ));
        sizes.push(q(
            "select id from t a where a.v not in (select b.v from t b where b.g = a.g and b.id <> a.id)",
            in_list(&data, true, |a, b| and(eq(&a.g, &b.g), Some(a.id != b.id))),
        ));
        assert!(
            sizes.iter().all(|n| *n > 0 && *n < data.len()),
            "{sizes:?}: a query that picks every row or none proves little"
        );
    }
}

fn or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

#[test]
fn a_delete_and_an_update_read_the_table_as_it_was_before_them() {
    let data = rows(200);
    let db = load(&data, false);
    // every row that has a later row in its group goes (the group's last row stays)
    run(&db, MAIN, "delete from t a where exists (select 1 from t b where b.g = a.g and b.id > a.id)", &[]).unwrap();
    let want: Vec<i64> = exists(&data, false, |a, b| and(eq(&a.g, &b.g), Some(b.id > a.id)));
    assert_eq!(ids(&db, "select id from t"), want);

    let db = load(&data, false);
    run(&db, MAIN, "update t a set v = 100 where exists (select 1 from t b where b.g = a.g and b.v > a.v)", &[])
        .unwrap();
    // v was below 9, so the rows at 100 are the rows the update changed
    assert_eq!(
        ids(&db, "select id from t where v = 100"),
        exists(&data, true, |a, b| and(eq(&a.g, &b.g), gt(&b.v, &a.v)))
    );
}

#[test]
fn shapes_that_need_a_subquery_per_outer_row_still_answer() {
    let data = rows(120);
    let db = load(&data, false);
    let group = |a: &R| data.iter().filter(|b| a.g.is_some() && b.g == a.g).count();
    let all: Vec<i64> = data.iter().map(|r| r.id).collect();
    let by = |keep: &dyn Fn(&R) -> bool| data.iter().filter(|r| keep(r)).map(|r| r.id).collect::<Vec<_>>();
    // a LIMIT and OFFSET, an aggregate with no group (always one row), HAVING, DISTINCT, a subquery
    // inside, an outer value in ON, no equality, an OR: each as written
    for (sql, want) in [
        (
            "select id from t a where exists (select 1 from t b where b.g = a.g order by b.id limit 1 offset 2)",
            by(&|a| group(a) >= 3),
        ),
        ("select id from t a where exists (select count(*) from t b where b.g = a.g and b.v > 100)", all.clone()),
        (
            "select id from t a where exists (select 1 from t b where b.g = a.g group by b.g having count(*) > 8)",
            by(&|a| group(a) > 8),
        ),
        (
            "select id from t a where exists (select distinct b.g from t b where b.g = a.g and b.id <> a.id)",
            by(&|a| group(a) >= 2),
        ),
        ("select id from t a where exists (select 1 from t b where b.g = a.g order by count(*))", all.clone()),
        (
            "select id from t a where exists (select distinct on (b.g) b.id from t b where b.g = a.g and b.v > a.v)",
            exists(&data, true, |a, b| and(eq(&a.g, &b.g), gt(&b.v, &a.v))),
        ),
        (
            "select id from t a where exists (select 1 from (select b.id from t b where b.g = a.g) d)",
            by(&|a| group(a) >= 1),
        ),
        (
            "select id from t a where exists (select 1 from t b where b.g = a.g \
             and exists (select 1 from t c where c.g = b.g and c.id > b.id + 40))",
            by(&|a| {
                data.iter().any(|b| {
                    a.g.is_some() && b.g == a.g && data.iter().any(|c| b.g.is_some() && c.g == b.g && c.id > b.id + 40)
                })
            }),
        ),
        (
            "select id from t a where exists (select 1 from t b join t c on c.g = a.g and c.id = b.id where b.g = a.g)",
            by(&|a| group(a) >= 1),
        ),
        (
            "select id from t a where exists (select 1 from t b where b.v > a.v)",
            exists(&data, true, |a, b| gt(&b.v, &a.v)),
        ),
        (
            "select id from t a where exists (select 1 from t b where b.g = a.g or b.v = a.v)",
            exists(&data, true, |a, b| or(eq(&a.g, &b.g), eq(&b.v, &a.v))),
        ),
    ] {
        assert_eq!(ids(&db, sql), want, "{sql}");
    }
}

#[test]
fn a_long_in_list_answers_as_one_compared_value_by_value_does() {
    let data = rows(300);
    let db = load(&data, false);
    let q = |sql: &str, want: Vec<i64>| {
        assert_eq!(ids(&db, sql), want, "{sql}");
        assert!(!want.is_empty() && want.len() < data.len(), "{sql}: {} of {}", want.len(), data.len());
    };
    // the lists are over 16 rows, so they're hashed
    let low = |b: &R| Some(b.v.is_none_or(|v| v < 4)); // with nulls
    let low_only = |b: &R| Some(b.v.is_some_and(|v| v < 4)); // without
    q(
        "select id from t a where a.v in (select b.v from t b where b.v < 4 or b.v is null)",
        in_list(&data, false, |_, b| low(b)),
    );
    // a null in the list makes NOT IN unknown for every row it doesn't find: none left
    let none: Vec<i64> = vec![];
    assert_eq!(
        ids(&db, "select id from t a where a.v not in (select b.v from t b where b.v < 4 or b.v is null)"),
        none
    );
    assert_eq!(in_list(&data, true, |_, b| low(b)), none);
    q("select id from t a where a.v in (select b.v from t b where b.v < 4)", in_list(&data, false, |_, b| low_only(b)));
    q(
        "select id from t a where a.v not in (select b.v from t b where b.v < 4)",
        in_list(&data, true, |_, b| low_only(b)),
    );
    // a number against a list of other number types, and text, and dates
    q(
        "select id from t a where a.v * 1.0 in (select b.v from t b where b.v < 4)",
        in_list(&data, false, |_, b| low_only(b)),
    );
    q("select id from t a where lower(a.h) in (select lower(b.h) from t b where b.id < 60 and b.h <> 'ef')", {
        let list: Vec<String> = data
            .iter()
            .filter(|b| b.id < 60 && b.h.as_deref().is_some_and(|h| h != "ef"))
            .map(|b| b.h.clone().unwrap().to_lowercase())
            .collect();
        data.iter().filter(|a| a.h.as_ref().is_some_and(|h| list.contains(&h.to_lowercase()))).map(|a| a.id).collect()
    });
    q("select id from t a where a.d in (select b.d from t b where b.id < 90 and b.d < date '2026-01-11')", {
        let list: Vec<i64> =
            data.iter().filter(|b| b.id < 90 && b.d.is_some_and(|d| d < 10)).map(|b| b.d.unwrap()).collect();
        data.iter().filter(|a| a.d.is_some_and(|d| list.contains(&d))).map(|a| a.id).collect()
    });
}

fn pairs(db: &Db, sql: &str) -> Vec<(i64, i64)> {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap();
    let int = |v: &Value| match v {
        Value::Int(n) => *n,
        Value::Null => 0,
        other => panic!("{sql}: {other:?}"),
    };
    let mut v: Vec<(i64, i64)> = out.rows.iter().map(|r| (int(&r[0]), int(&r[1]))).collect();
    v.sort();
    v
}

/// Every (a, b) the condition holds for; with `left`, a row of `a` with no match once, against 0.
fn joined(rows: &[R], left: bool, cond: impl Fn(&R, &R) -> Option<bool>) -> Vec<(i64, i64)> {
    let mut out = vec![];
    for a in rows {
        let mut any = false;
        for b in rows {
            if cond(a, b) == Some(true) {
                out.push((a.id, b.id));
                any = true;
            }
        }
        if left && !any {
            out.push((a.id, 0));
        }
    }
    out.sort();
    out
}

#[test]
fn a_join_on_an_expression_is_hashed_and_answers_the_same() {
    let data = rows(250);
    let db = load(&data, false);
    let q = |sql: &str, want: Vec<(i64, i64)>| {
        assert_eq!(pairs(&db, sql), want, "{sql}");
        assert!(!want.is_empty(), "{sql}");
    };
    // each side an expression of its own table (null keys match nothing)
    q(
        "select a.id, b.id from t a join t b on lower(a.h) = lower(b.h) and a.id < b.id",
        joined(&data, false, |a, b| and(eq(&lower(&a.h), &lower(&b.h)), Some(a.id < b.id))),
    );
    // in WHERE, written the other way round, with a condition on one table alone
    q(
        "select a.id, b.id from t a, t b where lower(b.h) = lower(a.h) and a.v > 5 and b.id <> a.id",
        joined(&data, false, |a, b| and(and(eq(&lower(&a.h), &lower(&b.h)), a.v.map(|v| v > 5)), Some(a.id != b.id))),
    );
    // numbers: an int against an expression over a float
    q(
        "select a.id, b.id from t a join t b on a.g + 1 = b.g * 1.0 and b.id > a.id",
        joined(&data, false, |a, b| and(eq(&a.g.map(|g| g + 1), &b.g), Some(b.id > a.id))),
    );
    // a LEFT JOIN keeps the rows that match nothing, with null
    q(
        "select a.id, b.id from t a left join t b on lower(a.h) = lower(b.h) and b.id > a.id + 200",
        joined(&data, true, |a, b| and(eq(&lower(&a.h), &lower(&b.h)), Some(b.id > a.id + 200))),
    );
    // a volatile expression is evaluated as written (here: it holds for every row)
    q(
        "select a.id, b.id from t a join t b on lower(a.h) = lower(b.h) and random() >= 0 and a.id < b.id",
        joined(&data, false, |a, b| and(eq(&lower(&a.h), &lower(&b.h)), Some(a.id < b.id))),
    );
}

/// Random correlated subqueries, and joins on expressions, answered the way they were before: a
/// volatile condition in the subquery (`random() >= 0`), or an equality wrapped in `coalesce`
/// (a function, not a bare `=`), keeps a query on the old path, so each pair must agree.
#[test]
fn random_queries_agree_with_the_per_row_path() {
    let data = rows(240);
    let mut r = Lcg(99);
    let pick = |r: &mut Lcg, of: &[&'static str]| of[r.below(of.len() as u64) as usize];
    for index in [false, true] {
        let db = load(&data, index);
        for round in 0..150 {
            // equalities between the subquery's rows (b) and the outer row (a), either way round
            let eqs = [
                ("b.g", "a.g"),
                ("b.h", "a.h"),
                ("lower(b.h)", "lower(a.h)"),
                ("b.v", "a.v"),
                ("b.d", "a.d"),
                ("b.g", "a.g + 1"),
                ("b.v + 1", "a.v"),
                ("b.id % 7", "a.v"),
            ];
            let mut conds: Vec<String> = vec![];
            for _ in 0..1 + r.below(2) {
                let (i, o) = eqs[r.below(eqs.len() as u64) as usize];
                conds.push(if r.below(2) == 0 { format!("{i} = {o}") } else { format!("{o} = {i}") });
            }
            for _ in 0..r.below(3) {
                conds.push(
                    pick(
                        &mut r,
                        &[
                            "b.id <> a.id",
                            "b.id < a.id",
                            "b.v > a.v",
                            "b.d >= a.d",
                            "(b.v <> a.v or b.g is null)",
                            "a.v is not null",
                            "b.h <> a.h",
                            "b.v + a.v > 6",
                        ],
                    )
                    .to_string(),
                );
            }
            for _ in 0..r.below(2) {
                conds.push(pick(&mut r, &["b.v < 5", "b.h = 'cd'", "b.g is not null", "b.id % 2 = 0"]).to_string());
            }
            let outer = pick(&mut r, &["", " and a.id % 3 <> 0", " and a.g is not null", " and a.h <> 'ef'"]);
            let (head, tail) = match r.below(4) {
                0 => ("exists", ""),
                1 => ("not exists", ""),
                2 => ("a.v in", ""),
                _ => ("a.v not in", ""),
            };
            let select = if head.ends_with("in") { "b.v" } else { "1" };
            let sub = |extra: &str| format!("(select {select} from t b where {}{extra}{tail})", conds.join(" and "));
            let query = |extra: &str| format!("select id from t a where {head} {}{outer}", sub(extra));
            let (fast, slow) = (query(""), query(" and random() >= 0"));
            // a volatile condition last: it must not change which rows the others pick
            assert_eq!(ids(&db, &fast), ids(&db, &slow), "round {round} (index: {index}): {fast}");
        }
    }
}

#[test]
fn random_joins_on_expressions_agree_with_nested_loops() {
    let data = rows(160);
    let db = load(&data, false);
    let mut r = Lcg(5);
    let keys = [
        ("lower(a.h)", "lower(b.h)"),
        ("upper(a.h)", "lower(b.h) || ''"),
        ("a.g + 1", "b.g"),
        ("a.g + 1", "b.g * 1"),
        ("a.v", "b.id % 9"),
        ("a.id % 5", "b.v + 1"),
        ("length(a.h)", "b.v"),
    ];
    for round in 0..60 {
        let (x, y) = keys[r.below(keys.len() as u64) as usize];
        let eq = if r.below(2) == 0 { format!("{x} = {y}") } else { format!("{y} = {x}") };
        let wrapped = format!("coalesce({eq}, false)");
        let more =
            ["", " and a.id < b.id", " and b.v > 2", " and a.h <> b.h", " and a.id % 4 = 0"][r.below(5) as usize];
        let kind = ["join", "left join"][r.below(2) as usize];
        let q = |cond: &str| match kind {
            "join" => format!("select a.id, b.id from t a join t b on {cond}{more}"),
            _ => format!("select a.id, b.id from t a left join t b on {cond}{more}"),
        };
        let (fast, slow) = (q(&eq), q(&wrapped));
        assert_eq!(pairs(&db, &fast), pairs(&db, &slow), "round {round}: {fast}");
        // the same in WHERE
        let w = |cond: &str| format!("select a.id, b.id from t a, t b where {cond}{more}");
        assert_eq!(pairs(&db, &w(&eq)), pairs(&db, &w(&wrapped)), "round {round} where: {}", w(&eq));
    }
}

#[test]
fn a_subquery_inside_a_per_row_subquery_that_reads_the_outermost_row_answers_the_same() {
    // the inner subquery's text changes with each outer row (its value is filled in), so each
    // gets a table of its own; the answers must still be the per-row ones
    let data = rows(150);
    for index in [false, true] {
        let db = load(&data, index);
        let q = |extra: &str| {
            format!(
                "select id from t a where exists (select 1 from t b where b.g = a.g and exists \
                 (select 1 from t c where c.h = b.h and c.v > a.v{extra} and c.id <> b.id))"
            )
        };
        let want = {
            let hits: Vec<i64> = data
                .iter()
                .filter(|a| {
                    data.iter().any(|b| {
                        a.g.is_some()
                            && b.g == a.g
                            && data.iter().any(|c| {
                                b.h.is_some() && c.h == b.h && c.v.zip(a.v).is_some_and(|(x, y)| x > y) && c.id != b.id
                            })
                    })
                })
                .map(|a| a.id)
                .collect();
            hits
        };
        assert!(!want.is_empty() && want.len() < data.len());
        assert_eq!(ids(&db, &q("")), want, "index: {index}");
        assert_eq!(ids(&db, &q(" and random() >= 0")), want, "index: {index}");
    }
}

#[test]
fn a_condition_that_fails_on_rows_the_equality_would_skip_still_isnt_reached() {
    // `i.s::int` fails on 'abc', a row no outer row's k picks: the per-row subquery never meets it
    for index in [false, true] {
        let db = Db::new();
        let sql = |q: &str| run(&db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {}", e.message)).pop().unwrap();
        sql("create table o (id int, k int)");
        sql("create table i (k int, s text)");
        if index {
            sql("create index i_k on i (k)");
        }
        sql("insert into o values (1, 1)");
        sql("insert into i values (1, '5'), (2, 'abc')");
        let got = sql("select id from o where exists (select 1 from i where i.k = o.k and i.s::int > 0)");
        assert_eq!(got.rows, vec![vec![Value::Int(1)]], "index: {index}");
        // a join on an expression, the same
        let got = sql("select o.id from o join i on i.k = o.k and o.id = i.s::int - 4");
        assert_eq!(got.rows, vec![vec![Value::Int(1)]], "index: {index}");
        let got = sql("select o.id from o join i on o.k = i.s::int - 4 where i.k = o.k and i.k < 2");
        assert_eq!(got.rows, vec![vec![Value::Int(1)]], "index: {index}");
    }
}

#[test]
fn floats_and_integers_compare_through_f64_as_before() {
    // 2^53 + 1 and 2^53 as a double are equal to `=`: hashing them apart would answer differently
    let db = Db::new();
    let sql = |q: &str| run(&db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {}", e.message)).pop().unwrap();
    sql("create table a (id int, i bigint)");
    sql("create table b (id int, f float8)");
    sql("insert into a values (1, 9007199254740993), (2, 5)");
    sql("insert into b values (1, 9007199254740992.0), (2, 7.0)");
    let one = vec![vec![Value::Int(1)]];
    assert_eq!(sql("select id from a where exists (select 1 from b where b.f = a.i)").rows, one);
    assert_eq!(sql("select a.id from a join b on b.f * 1 = a.i").rows, one);
    let many: Vec<String> = (0..20).map(|n| format!("{n}.5")).chain(["9007199254740992.0".to_string()]).collect();
    sql("create table l (f float8)");
    sql(&format!("insert into l values ({})", many.join("), (")));
    assert_eq!(sql("select id from a where a.i in (select f from l)").rows, one);
}
