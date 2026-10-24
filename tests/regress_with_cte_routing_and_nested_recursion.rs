//! WITH forms of Postgres's regression suite (with.sql) Chronos took differently: a WITH RECURSIVE query that changes
//! rows and reads one after it (on temporary tables, run whole in their database), itself or through a query that
//! doesn't change rows, and queries that read one another in a circle (mutual recursion, which Postgres refuses), the
//! outer recursive query read inside a nested WITH RECURSIVE, and a reference to the recursive query in a WITH query
//! that is read twice.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn col(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    run(db, s, q).into_iter().map(|r| r[0].clone()).collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code, e.message),
    }
}

/// The error and its position (1-based, in characters), which must be at `at`'s first place in `q`.
fn fails_at(db: &Db, s: &mut Session, q: &str, at: &str) -> (&'static str, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => {
            let want = q.find(at).map(|k| k as u32 + 1);
            assert_eq!(e.info.as_ref().and_then(|i| i.position), want, "{q}: {}", e.message);
            (e.code, e.message)
        }
    }
}

fn nums(r: std::ops::RangeInclusive<i32>) -> Vec<String> {
    r.map(|n| n.to_string()).collect()
}

#[test]
fn a_forward_reference_on_temporary_tables_runs_in_their_database() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create temporary table y (a integer)");
    run(&db, &mut s, "insert into y select generate_series(11, 21)");
    // (with.sql: t reads t2, written after it; t2 runs first and t inserts what it gave back)
    let got = col(
        &db,
        &mut s,
        "with recursive t as (insert into y select a+5 from t2 where a > 5 returning *),
              t2 as (update y set a=a-11 returning *)
         select * from t union all select * from t2",
    );
    assert_eq!(got, [nums(11..=15), nums(0..=10)].concat());
    assert_eq!(col(&db, &mut s, "select a from y order by a"), nums(0..=15));

    // (the pair that checks the order they run to completion in)
    run(&db, &mut s, "truncate table y");
    run(&db, &mut s, "insert into y select generate_series(1, 3)");
    run(&db, &mut s, "create temporary table yy (a integer)");
    run(
        &db,
        &mut s,
        "with recursive t1 as (insert into y select * from y returning *),
              t2 as (insert into yy select * from t1 returning *)
         select 1",
    );
    assert_eq!(col(&db, &mut s, "select count(*) from y"), ["6"]);
    assert_eq!(col(&db, &mut s, "select a from yy order by a"), ["1", "2", "3"]);
    let one = col(
        &db,
        &mut s,
        "with recursive t1 as (insert into yy select * from t2 returning *),
              t2 as (insert into y select * from y returning *)
         select 1",
    );
    assert_eq!(one, ["1"]);
    assert_eq!(col(&db, &mut s, "select count(*) from y"), ["12"]);
    assert_eq!(col(&db, &mut s, "select count(*) from yy"), ["9"]);
    assert_eq!(col(&db, &mut s, "select a from yy order by a"), ["1", "1", "1", "2", "2", "2", "3", "3", "3"]);

    // one that changes a temporary table and reads a table that isn't one is still refused
    run(&db, &mut s, "create table w (a integer); insert into w values (7)");
    let (code, message) = fails(&db, &mut s, "with t as (insert into y select a from w returning *) select * from t");
    assert_eq!(code, "0A000");
    assert_eq!(message, "WITH queries that change rows can't change a temporary table while they read other tables");
}

#[test]
fn a_forward_reference_on_permanent_tables() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table y (a integer)");
    run(&db, &mut s, "insert into y select generate_series(11, 21)");
    let got = col(
        &db,
        &mut s,
        "with recursive t as (insert into y select a+5 from t2 where a > 5 returning *),
              t2 as (update y set a=a-11 returning *)
         select * from t union all select * from t2",
    );
    assert_eq!(got, [nums(11..=15), nums(0..=10)].concat());
    assert_eq!(col(&db, &mut s, "select a from y order by a"), nums(0..=15));
    // (in a WITH that isn't RECURSIVE, a name after it is no WITH query's)
    let (code, message) = fails(
        &db,
        &mut s,
        "with t as (insert into y select a from t2 returning *), t2 as (select 1 as a) select * from t",
    );
    assert_eq!(code, "42P01");
    assert!(message.contains("\"t2\" does not exist"), "{message}");
}

#[test]
fn the_outer_recursive_query_is_read_in_a_nested_with_recursive() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create temp table department (id integer primary key, parent_department integer references department,
                                       name text)",
    );
    run(
        &db,
        &mut s,
        "insert into department values (0, null, 'ROOT'), (1, 0, 'A'), (2, 1, 'B'), (3, 2, 'C'), (4, 2, 'D'),
                                       (5, 0, 'E'), (6, 4, 'F'), (7, 5, 'G')",
    );
    // (with.sql: x never ends, but the LIMIT takes 32 rows, the first of q's and x's rounds)
    let got = col(
        &db,
        &mut s,
        "with recursive q as (
               select * from department
             union all
               (with recursive x as (
                    select * from department
                  union all
                    (select * from q union all select * from x)
                 )
                select * from x)
             )
         select name from q limit 32",
    );
    let names = ["ROOT", "A", "B", "C", "D", "E", "F", "G"];
    assert_eq!(got, names.repeat(4));
    // the nested query's first part reads the rows so far too
    let got = col(
        &db,
        &mut s,
        "with recursive q(n) as (
               select 1
             union all
               (with recursive x(m) as (select n + 1 from q where n < 4 union all select m from x where false)
                select m from x)
             )
         select n from q order by n",
    );
    assert_eq!(got, ["1", "2", "3", "4"]);
    // and its step, each round of the outer query's
    let got = col(
        &db,
        &mut s,
        "with recursive q(n) as (
               select 1
             union all
               (with recursive x(m) as (select 0 union (select n from q where n < 3 union all select m from x where false))
                select m + 1 from x where m > 0)
             )
         select n from q order by n",
    );
    assert_eq!(got, ["1", "2", "3"]);
}

#[test]
fn a_with_query_read_twice_counts_its_recursive_reference_once() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for op in ["union all", "union"] {
        let q = format!(
            "with recursive
               tab(id_key, link) as (values (1, 17), (2, 17), (3, 17), (4, 17), (6, 17), (5, 17)),
               iter (id_key, row_type, link) as (
                   select 0, 'base', 17
                 {op} (
                   with remaining(id_key, row_type, link, min) as (
                     select tab.id_key, 'true'::text, iter.link, min(tab.id_key) over ()
                     from tab inner join iter using (link)
                     where tab.id_key > iter.id_key
                   ),
                   first_remaining as (
                     select id_key, row_type, link
                     from remaining
                     where id_key=min
                   ),
                   effect as (
                     select tab.id_key, 'new'::text, tab.link
                     from first_remaining e inner join tab on e.id_key=tab.id_key
                     where e.row_type = 'false'
                   )
                   select * from first_remaining
                   union all select * from effect
                 )
               )
             select * from iter"
        );
        let got = run(&db, &mut s, &q);
        let want: Vec<Vec<String>> = std::iter::once(vec!["0".into(), "base".into(), "17".into()])
            .chain((1..=6).map(|k| vec![k.to_string(), "true".into(), "17".into()]))
            .collect();
        assert_eq!(got, want, "{op}");
    }
    // (twice in the step itself is still twice)
    let (code, message) = fails(
        &db,
        &mut s,
        "with recursive foo(i) as (values (1) union all
           (select i+1 from foo where i < 10 union all select i+1 from foo where i < 5))
         select * from foo",
    );
    assert_eq!(code, "42P19");
    assert_eq!(message, "recursive reference to query \"foo\" must not appear more than once");
    // (and a WITH query that reads it twice)
    let (code, message) = fails(
        &db,
        &mut s,
        "with recursive foo(i) as (values (1) union all
           (with w as (select i+1 from foo where i < 10 union all select i+1 from foo where i < 5)
            select * from w))
         select * from foo",
    );
    assert_eq!(code, "42P19");
    assert_eq!(message, "recursive reference to query \"foo\" must not appear more than once");
}

#[test]
fn a_query_that_changes_rows_runs_after_one_it_reads_through_a_query_that_doesnt() {
    for temp in ["", "temporary "] {
        let (db, mut s) = (Db::new(), Session::new(MAIN));
        run(&db, &mut s, &format!("create {temp}table y (v integer)"));
        run(&db, &mut s, &format!("create {temp}table z (v integer)"));
        run(&db, &mut s, "insert into z values (1), (2)");
        // (a reads c, which reads b: b runs first, and a inserts what it deleted)
        let got = col(
            &db,
            &mut s,
            "with recursive a as (insert into y select * from c returning *),
                  b as (delete from z returning *),
                  c as (select * from b)
             select v from a order by v",
        );
        assert_eq!(got, ["1", "2"], "{temp}");
        assert_eq!(col(&db, &mut s, "select v from y order by v"), ["1", "2"], "{temp}");
        assert_eq!(col(&db, &mut s, "select count(*) from z"), ["0"], "{temp}");
    }
}

#[test]
fn queries_that_read_one_another_in_a_circle_are_mutual_recursion() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table y (a integer)");
    run(&db, &mut s, "create table z (a integer)");
    run(&db, &mut s, "insert into y values (1)");
    let refused = [
        // two that change rows, each reading the other
        (
            "with recursive t1 as (insert into y select * from t2 returning *),
                  t2 as (insert into z select * from t1 returning *)
             select 1",
            "t1 as",
        ),
        // one that changes rows and reads itself through a query after it
        ("with recursive t as (insert into y select * from r returning *), r as (select * from t) select 1", "t as"),
        // (and a query before it, which reads it)
        ("with recursive r as (select * from t), t as (insert into y select * from r returning *) select 1", "r as"),
        // Postgres's sort swaps x, which reads nothing, to the first place: the place left is t2's
        (
            "with recursive t1 as (insert into y select * from t2 returning *),
                  t2 as (insert into z select * from t1 returning *),
                  x as (select 1)
             select 1",
            "t2 as",
        ),
        // two recursive queries, each reading the other
        (
            "with recursive a(n) as (select m from b union all select n+1 from a where n < 3),
                  b(m) as (select 1 union select n from a, b where n < 2 and m = n)
             select * from b",
            "a(n)",
        ),
        (
            "with recursive a(n) as (select 1 union all select n+1 from a, b where n < 3),
                  b(m) as (select 1 union all select n from a, b where m < 2)
             select * from b",
            "a(n)",
        ),
    ];
    for (q, at) in refused {
        let (code, message) = fails_at(&db, &mut s, q, at);
        assert_eq!(code, "0A000", "{q}");
        assert_eq!(message, "mutual recursion between WITH items is not implemented", "{q}");
    }
    // nothing ran
    assert_eq!(col(&db, &mut s, "select count(*) from y"), ["1"]);
    assert_eq!(col(&db, &mut s, "select count(*) from z"), ["0"]);
    // (one read before a query that changes rows and gives nothing back)
    let (code, message) = fails(
        &db,
        &mut s,
        "with recursive t as (insert into y select * from t2 returning *), t2 as (insert into z values (1)) select 1",
    );
    assert_eq!(code, "0A000");
    assert_eq!(message, "WITH query \"t2\" does not have a RETURNING clause");
}
