//! Views over joins as Postgres keeps them (create_view's): a view's names keep finding the columns they found when
//! a table it reads gains, loses or renames a column (a column added whose name another takes is shown by another,
//! `tt11 tt11(x, y, z_1)`), a join with an alias shows its column alias list once its sides' columns changed, a
//! JOIN binds tighter than a comma, and a join written back says CROSS JOIN, ON TRUE and USING (...) AS x as
//! Postgres's ruleutils.c says them.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN, Resolve};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

/// pg_get_viewdef(v, true).
fn def(db: &Db, s: &mut Session, v: &str) -> String {
    run(db, s, &format!("select pg_get_viewdef('{v}', true)"))[0][0].clone()
}

/// A definition as the lines Postgres writes it.
fn lines(l: &[&str]) -> String {
    l.join("\n")
}

/// The SQLSTATE and message `q` fails with.
fn fails(db: &Db, s: &mut Session, q: &str) -> (String, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: ran"),
        Err(e) => (e.code.to_string(), e.message),
    }
}

#[test]
fn a_join_alias_list_follows_its_sides_columns() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table tt2 (a int, b int, c int);
         create table tt3 (ax int8, b int2, c numeric);
         create table tt4 (ay int, b int, q int);
         create view v1 as select * from tt2 natural join tt3;
         create view v1a as select * from (tt2 natural join tt3) j;
         create view v2a as select * from (tt2 join tt3 using (b,c) join tt4 using (b)) j;
         alter table tt2 add column d int;
         alter table tt2 add column e int",
    );
    // (columns added that take no other's name: no alias list)
    assert_eq!(
        def(&db, &mut s, "v1a"),
        lines(&[" SELECT j.b,", "    j.c,", "    j.a,", "    j.ax", "   FROM (tt2", "     JOIN tt3 USING (b, c)) j;"])
    );
    run(&db, &mut s, "alter table tt3 rename c to d");
    run(&db, &mut s, "alter table tt3 add column c int");
    run(&db, &mut s, "alter table tt3 add column e int");
    assert_eq!(
        def(&db, &mut s, "v1"),
        lines(&[
            " SELECT tt2.b,",
            "    tt3.c,",
            "    tt2.a,",
            "    tt3.ax",
            "   FROM tt2",
            "     JOIN tt3 tt3(ax, b, c, c_1, e) USING (b, c);",
        ])
    );
    assert_eq!(
        def(&db, &mut s, "v1a"),
        lines(&[
            " SELECT j.b,",
            "    j.c,",
            "    j.a,",
            "    j.ax",
            "   FROM (tt2",
            "     JOIN tt3 tt3(ax, b, c, c_1, e) USING (b, c)) j(b, c, a, d, e, ax, c_1, e_1);",
        ])
    );
    assert_eq!(
        def(&db, &mut s, "v2a"),
        lines(&[
            " SELECT j.b,",
            "    j.c,",
            "    j.a,",
            "    j.ax,",
            "    j.ay,",
            "    j.q",
            "   FROM (tt2",
            "     JOIN tt3 tt3(ax, b, c, c_1, e) USING (b, c)",
            "     JOIN tt4 USING (b)) j(b, c, a, d, e, ax, c_1, e_1, ay, q);",
        ])
    );
    run(&db, &mut s, "alter table tt2 drop column d");
    assert_eq!(
        def(&db, &mut s, "v1a"),
        lines(&[
            " SELECT j.b,",
            "    j.c,",
            "    j.a,",
            "    j.ax",
            "   FROM (tt2",
            "     JOIN tt3 tt3(ax, b, c, c_1, e) USING (b, c)) j(b, c, a, e, ax, c_1, e_1);",
        ])
    );
    assert_eq!(
        def(&db, &mut s, "v2a"),
        lines(&[
            " SELECT j.b,",
            "    j.c,",
            "    j.a,",
            "    j.ax,",
            "    j.ay,",
            "    j.q",
            "   FROM (tt2",
            "     JOIN tt3 tt3(ax, b, c, c_1, e) USING (b, c)",
            "     JOIN tt4 USING (b)) j(b, c, a, e, ax, c_1, e_1, ay, q);",
        ])
    );
    // the views still read as they did
    run(&db, &mut s, "insert into tt2 values (1, 2, 3, 4); insert into tt3 values (10, 2, 3, 5, 6)");
    assert_eq!(run(&db, &mut s, "select * from v1a"), vec![vec!["2", "3", "1", "10"]]);
    // a view over a view whose column is renamed follows it (`x AS a`)
    run(&db, &mut s, "create view v4 as select * from v1");
    run(&db, &mut s, "alter view v1 rename column a to x");
    assert_eq!(
        def(&db, &mut s, "v1"),
        lines(&[
            " SELECT tt2.b,",
            "    tt3.c,",
            "    tt2.a AS x,",
            "    tt3.ax",
            "   FROM tt2",
            "     JOIN tt3 tt3(ax, b, c, c_1, e) USING (b, c);",
        ])
    );
    assert_eq!(def(&db, &mut s, "v4"), lines(&[" SELECT b,", "    c,", "    x AS a,", "    ax", "   FROM v1;"]));
    assert_eq!(run(&db, &mut s, "select a from v4"), vec![vec!["1"]]);
}

#[test]
fn a_named_join_s_new_columns_go_after_their_side_s() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table tt5 (a int, b int);
         create table tt6 (c int, d int);
         create view vv1 as select * from (tt5 cross join tt6) j(aa,bb,cc,dd)",
    );
    let shown = |from: &str| lines(&[" SELECT j.aa,", "    j.bb,", "    j.cc,", "    j.dd", "   FROM (tt5", from]);
    assert_eq!(def(&db, &mut s, "vv1"), shown("     CROSS JOIN tt6) j(aa, bb, cc, dd);"));
    run(&db, &mut s, "alter table tt5 add column c int");
    assert_eq!(def(&db, &mut s, "vv1"), shown("     CROSS JOIN tt6) j(aa, bb, c, cc, dd);"));
    run(&db, &mut s, "alter table tt5 add column cc int");
    assert_eq!(def(&db, &mut s, "vv1"), shown("     CROSS JOIN tt6) j(aa, bb, c, cc_1, cc, dd);"));
    run(&db, &mut s, "alter table tt5 drop column c");
    assert_eq!(def(&db, &mut s, "vv1"), shown("     CROSS JOIN tt6) j(aa, bb, cc_1, cc, dd);"));
    // (j.cc is still tt6's c)
    run(&db, &mut s, "insert into tt5 values (1, 2, 3); insert into tt6 values (4, 5)");
    assert_eq!(run(&db, &mut s, "select cc from vv1"), vec![vec!["4"]]);
}

#[test]
fn a_column_added_under_a_using_keeps_the_using_column() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table tt11 (x int, y int);
         create table tt12 (x int, z int);
         create table tt13 (z int, q int);
         create view vv6 as select x,y,z,q from (tt11 join tt12 using(x)) join tt13 using(z);
         alter table tt11 add column z int",
    );
    assert_eq!(
        def(&db, &mut s, "vv6"),
        lines(&[
            " SELECT tt11.x,",
            "    tt11.y,",
            "    tt12.z,",
            "    tt13.q",
            "   FROM tt11 tt11(x, y, z_1)",
            "     JOIN tt12 USING (x)",
            "     JOIN tt13 USING (z);",
        ])
    );
    run(
        &db,
        &mut s,
        "insert into tt11 values (1, 2, 99); insert into tt12 values (1, 3); insert into tt13 values (3, 4)",
    );
    assert_eq!(run(&db, &mut s, "select * from vv6"), vec![vec!["1", "2", "3", "4"]]);
}

#[test]
fn a_join_binds_tighter_than_a_comma() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table tt7 (x int, xx int, y int);
         alter table tt7 drop column xx;
         create table tt8 (x int, z int);
         create view vv2 as
         select * from (values(1,2,3,4,5)) v(a,b,c,d,e)
         union all
         select * from tt7 full join tt8 using (x), tt8 tt8x;
         create view vv3 as
         select * from (values(1,2,3,4,5,6)) v(a,b,c,x,e,f)
         union all
         select * from
           tt7 full join tt8 using (x),
           tt7 tt7x full join tt8 tt8x using (x)",
    );
    let head = [
        " SELECT v.a,",
        "    v.b,",
        "    v.c,",
        "    v.x,",
        "    v.e,",
        "    v.f",
        "   FROM ( VALUES (1,2,3,4,5,6)) v(a, b, c, x, e, f)",
        "UNION ALL",
        " SELECT x AS a,",
        "    tt7.y AS b,",
        "    tt8.z AS c,",
        "    x_1 AS x,",
        "    tt7x.y AS e,",
        "    tt8x.z AS f",
        "   FROM tt7",
        "     FULL JOIN tt8 USING (x),",
    ];
    let with = |tail: &[&str]| lines(&head.iter().chain(tail).copied().collect::<Vec<_>>());
    assert_eq!(
        def(&db, &mut s, "vv3"),
        with(&["    tt7 tt7x(x_1, y)", "     FULL JOIN tt8 tt8x(x_1, z) USING (x_1);"])
    );
    // (the USING column of a FULL JOIN is shown by its bare name: every other column of its name is renamed)
    assert!(def(&db, &mut s, "vv2").contains("    tt8x.x_1 AS d,"), "{}", def(&db, &mut s, "vv2"));
    run(&db, &mut s, "alter table tt7 add column zz int");
    run(&db, &mut s, "alter table tt7 add column z int");
    run(&db, &mut s, "alter table tt7 drop column zz");
    run(&db, &mut s, "alter table tt8 add column z2 int");
    assert_eq!(
        def(&db, &mut s, "vv3"),
        with(&["    tt7 tt7x(x_1, y, z)", "     FULL JOIN tt8 tt8x(x_1, z, z2) USING (x_1);"])
    );
    assert!(def(&db, &mut s, "vv2").ends_with("    tt8 tt8x(x_1, z, z2);"), "{}", def(&db, &mut s, "vv2"));
    // a query: the second item's USING sees its own left side alone
    run(&db, &mut s, "create table p (x int, a int); create table q (x int, b int); create table r (x int, c int)");
    run(
        &db,
        &mut s,
        "insert into p values (1, 10); insert into q values (1, 20), (2, 30); insert into r values (2, 40)",
    );
    assert_eq!(run(&db, &mut s, "select * from p, q join r using (x)"), vec![vec!["1", "10", "2", "30", "40"]]);
}

#[test]
fn joins_without_columns_in_common_and_using_aliases_are_written_back() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table tt5 (a int, b int, cc int);
         create table tt6 (c int, d int);
         create view tt21v as select * from tt5 natural inner join tt6;
         create view tt22v as select * from tt5 natural left join tt6;
         create table tbl1 (a int, b int);
         create table tbl1a (a int, c int);
         create view v2b as select * from tbl1 join tbl1a using (a) as x;
         create view v2d as select * from (tbl1 join tbl1a using (a) as x) as y",
    );
    let cols = [" SELECT tt5.a,", "    tt5.b,", "    tt5.cc,", "    tt6.c,", "    tt6.d", "   FROM tt5"];
    let with = |last: &str| lines(&cols.iter().copied().chain([last]).collect::<Vec<_>>());
    assert_eq!(def(&db, &mut s, "tt21v"), with("     CROSS JOIN tt6;"));
    assert_eq!(def(&db, &mut s, "tt22v"), with("     LEFT JOIN tt6 ON TRUE;"));
    assert_eq!(
        def(&db, &mut s, "v2b"),
        lines(&[" SELECT tbl1.a,", "    tbl1.b,", "    tbl1a.c", "   FROM tbl1", "     JOIN tbl1a USING (a) AS x;"])
    );
    assert_eq!(
        def(&db, &mut s, "v2d"),
        lines(&[" SELECT y.a,", "    y.b,", "    y.c", "   FROM (tbl1", "     JOIN tbl1a USING (a) AS x) y;"])
    );
    // (a comma is a comma, a CROSS JOIN a join)
    run(&db, &mut s, "create view commas as select tt5.a, tt6.c from tt5, tt6");
    assert_eq!(def(&db, &mut s, "commas"), lines(&[" SELECT tt5.a,", "    tt6.c", "   FROM tt5,", "    tt6;"]));
    run(&db, &mut s, "insert into tt5 values (1, 2, 3); insert into tt6 values (4, 5)");
    assert_eq!(run(&db, &mut s, "select * from tt22v"), vec![vec!["1", "2", "3", "4", "5"]]);
}

#[test]
fn an_alias_a_renamed_table_takes_is_shown_as_written_again() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table tt1 (f1 int, f2 int, f3 text);
         create table tx1 (x1 int, x2 int, x3 text);
         create view aliased_view_3 as select * from tt1 where exists (select 1 from tx1 a2 where tt1.f1 = a2.x1)",
    );
    let shown = |outer: &str, inner: &str, cond: &str| {
        lines(&[
            " SELECT f1,",
            "    f2,",
            "    f3",
            &format!("   FROM {outer}"),
            "  WHERE (EXISTS ( SELECT 1",
            &format!("           FROM {inner}"),
            &format!("          WHERE {cond}));"),
        ])
    };
    run(&db, &mut s, "alter table tx1 rename to a1");
    run(&db, &mut s, "alter table tt1 rename to a2");
    assert_eq!(def(&db, &mut s, "aliased_view_3"), shown("a2", "a1 a2_1", "a2.f1 = a2_1.x1"));
    run(&db, &mut s, "alter table a1 rename to tt1");
    assert_eq!(def(&db, &mut s, "aliased_view_3"), shown("a2", "tt1 a2_1", "a2.f1 = a2_1.x1"));
    // (the table that took the alias's name renamed again: the alias is shown as it was written)
    run(&db, &mut s, "alter table a2 rename to tx1");
    assert_eq!(def(&db, &mut s, "aliased_view_3"), shown("tx1", "tt1 a2", "tx1.f1 = a2.x1"));
    run(&db, &mut s, "insert into tx1 values (1, 2, 'a'); insert into tt1 values (1, 0, 'b')");
    assert_eq!(run(&db, &mut s, "select f3 from aliased_view_3"), vec![vec!["a"]]);
}

#[test]
fn a_change_a_view_does_not_see_leaves_its_row_as_it_is() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table base (id int);
         create table other (id int);
         create view s1 as select id from base union all select id from other except select 3;
         create view s2 as select id from base except select id from other union all select 3;
         create table m1 (id int, a int);
         create table m2 (id int, b int);
         create view mv as select m1.a, m2.b from m1 join m2 using (id);
         create view mv2 as select a from mv",
    );
    // (a set operation's parenthesized part, written back and read again, would not read as it did)
    let (s1, s2) = (def(&db, &mut s, "s1"), def(&db, &mut s, "s2"));
    run(&db, &mut s, "alter table base add column extra int");
    assert_eq!(def(&db, &mut s, "s1"), s1);
    assert_eq!(def(&db, &mut s, "s2"), s2);
    run(&db, &mut s, "alter table base rename column extra to more");
    assert_eq!(def(&db, &mut s, "s1"), s1);
    // (nor does a column of a view it doesn't read renamed)
    let mv2 = def(&db, &mut s, "mv2");
    run(&db, &mut s, "alter view mv rename column b to bb");
    assert_eq!(def(&db, &mut s, "mv2"), mv2);
    // two worlds, each adding a column to one of the tables a view reads: neither changes the view's row, so both
    // merge
    db.fork(MAIN, "w1").unwrap();
    db.fork(MAIN, "w2").unwrap();
    run(&db, &mut Session::new("w1"), "alter table m1 add column c int");
    run(&db, &mut Session::new("w2"), "alter table m2 add column d int");
    db.merge("w1", Resolve::Fail).unwrap();
    db.merge("w2", Resolve::Fail).unwrap();
    run(&db, &mut s, "insert into m1 values (1, 2, 3); insert into m2 values (1, 4, 5)");
    assert_eq!(run(&db, &mut s, "select * from mv"), vec![vec!["2", "4"]]);
}

#[test]
fn a_right_or_full_join_after_a_comma_joins_its_own_item() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table p (x int, a int);
         create table q (x int, b int);
         create table r (x int, c int);
         insert into p values (1, 10), (2, 20);
         insert into q values (1, 100);
         insert into r values (1, 1000), (3, 3000)",
    );
    // p with each row of q FULL JOIN r: four rows, none without p's columns
    let all = vec![
        vec!["1", "10", "1", "100", "1000"],
        vec!["1", "10", "3", "", "3000"],
        vec!["2", "20", "1", "100", "1000"],
        vec!["2", "20", "3", "", "3000"],
    ];
    assert_eq!(run(&db, &mut s, "select * from p, q full join r using (x) order by 1, 3"), all);
    assert_eq!(run(&db, &mut s, "select count(*) from p, q right join r using (x)"), vec![vec!["4"]]);
    assert_eq!(run(&db, &mut s, "select count(*) from p, q full join r on q.x = r.x"), vec![vec!["4"]]);
    // (its sides' columns by their tables' names)
    let named = "select p.a, coalesce(q.b, 0), r.c from p, q full join r using (x) order by 1, 3";
    let rows =
        vec![vec!["10", "100", "1000"], vec!["10", "0", "3000"], vec!["20", "100", "1000"], vec!["20", "0", "3000"]];
    assert_eq!(run(&db, &mut s, named), rows);
    // a view of one, written back as it was written, and read as it was after a column is added
    run(&db, &mut s, "create view pqr as select a, b, c from p, q full join r using (x)");
    assert!(def(&db, &mut s, "pqr").ends_with("     FULL JOIN r USING (x);"), "{}", def(&db, &mut s, "pqr"));
    run(&db, &mut s, "alter table r add column z int");
    assert_eq!(run(&db, &mut s, "select count(*) from pqr where c = 3000"), vec![vec!["2"]]);
}

#[test]
fn a_bare_using_column_is_ambiguous_where_another_item_has_its_name() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table p (x int, a int);
         create table q (x int, b int);
         create table r (x int, c int);
         insert into p values (1, 10);
         insert into q values (1, 100);
         insert into r values (1, 1000)",
    );
    for q in [
        "select x from p, q join r using (x)",
        "select a from p, q join r using (x) where x = 1",
        "select x from p, q full join r using (x)",
        "select x from q join r using (x), q q2 join r r2 using (x)",
    ] {
        let (code, message) = fails(&db, &mut s, q);
        assert_eq!(code, "42702", "{q}: {message}");
        assert!(message.starts_with("column reference \"x\" is ambiguous"), "{q}: {message}");
    }
    // (where no other item has it, it is the join's)
    assert_eq!(run(&db, &mut s, "select x, a from p join q using (x), r r2(y, c2)"), vec![vec!["1", "10"]]);
    assert_eq!(run(&db, &mut s, "select x from p p2(y, a), q join r using (x)"), vec![vec!["1"]]);
}
