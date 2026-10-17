//! ICU collations (`CREATE COLLATION`, `COLLATE`) where Chronos differs from Postgres on purpose or where there is
//! nothing in Postgres to compare with: what is refused, which index a query reads, and collations in worlds. The
//! orders themselves are compared with Postgres in tests/pgdiff.rs (`icu_collations_order_text` and the tests after
//! it).

use chronos::sql::{Session, Value, run};
use chronos::{Db, MAIN};

/// A query's rows, each a string with its columns joined by `|`.
fn rows(s: &mut Session, db: &Db, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    let text = |v: &Value| match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_string(),
        v => v.to_text(),
    };
    out.last().unwrap().rows.iter().map(|r| r.iter().map(text).collect::<Vec<_>>().join("|")).collect()
}

/// The SQLSTATE a statement fails with, and its message.
fn fails(s: &mut Session, db: &Db, q: &str) -> (String, String) {
    let e = s.run(db, q, &[]).expect_err(q);
    (e.code.to_string(), e.message)
}

/// In a nondeterministic collation (`deterministic = false`) the strings ICU ties are equal: comparisons, IN,
/// CASE, unique constraints and joins go by that (compared with Postgres in tests/pgdiff.rs,
/// `nondeterministic_collations`). What would still tell such strings apart by their bytes is refused with 0A000
/// rather than done wrong: a key, a subquery's rows, an aggregate's DISTINCT other than count's. GROUP BY,
/// DISTINCT, DISTINCT ON, set operations, count(DISTINCT) and PARTITION BY go by the keys too
/// (`pgdiff::groups_and_distinct_in_nondeterministic_collations`).
#[test]
fn a_nondeterministic_collation_refuses_what_it_cant_do_by_its_keys() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(&db, "create collation ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false)", &[]).unwrap();
    s.run(&db, "create table n (id int primary key, name text collate ci, tag text)", &[]).unwrap();
    s.run(&db, "insert into n values (1, 'Ada', 'x'), (2, 'ADA', 'y'), (3, 'bob', 'z')", &[]).unwrap();
    assert_eq!(rows(&mut s, &db, "select id from n where name = 'ada' order by id"), ["1", "2"]);
    assert_eq!(rows(&mut s, &db, "select count(*) from n a join n b on a.name = b.name"), ["5"]);
    assert_eq!(rows(&mut s, &db, "select count(*) from n group by name order by 1"), ["1", "2"]);
    assert_eq!(rows(&mut s, &db, "select count(*) from (select distinct name from n) d"), ["2"]);
    assert_eq!(
        rows(&mut s, &db, "select id from (select distinct on (name) name, id from n order by name, id) d order by id"),
        ["1", "3"]
    );
    for q in [
        "select array[name] from n group by array[name]",
        "select string_agg(distinct name, ',') from n",
        "select id from n where name in (select name from n)",
        "select id from n where name = any (select name from n)",
        "select id from n where array[name] = array['ada']",
        "create table k (name text collate ci primary key)",
        "create table k (name text collate ci, n int, primary key (name, n))",
        "create table p (name text collate ci unique)",
        "create table c (name text collate ci references p (name))",
        "create table c2 (name text references p (name))",
        "create table h (name text collate ci) partition by hash (name)",
        "create table l (name text collate ci) partition by list (name)",
    ] {
        match s.run(&db, q, &[]) {
            Err(e) if q.starts_with("create table p ") => panic!("{q}: {} {}", e.code, e.message),
            Err(e) => assert_eq!(e.code, "0A000", "{q}: {}", e.message),
            Ok(_) => assert!(q.starts_with("create table p "), "{q} should be refused"),
        }
    }
    // (UNION ALL tells nothing apart)
    assert_eq!(rows(&mut s, &db, "select count(*) from (select name from n union all select name from n) x"), ["6"]);
    // like Postgres 16: no LIKE in one
    assert_eq!(fails(&mut s, &db, "select id from n where name like 'a%'").0, "0A000");
    // a column of it in a world: forked, written and merged as any other
    s.run(&db, "create unique index n_name on n (tag collate ci)", &[]).unwrap();
    assert_eq!(fails(&mut s, &db, "insert into n values (4, 'zed', 'X')").0, "23505");
}

/// Postgres has custom rules and ICU locales whose rules ICU4X doesn't carry; each is
/// refused with 0A000 and says why, rather than giving a different order. (libc locales are
/// the ICU collations of their locales: see `libc_locales_are_icu_ones`.)
#[test]
fn what_icu4x_cant_match_is_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (q, says) in [
        ("create collation r (provider = icu, locale = 'und', rules = '&a < b')", "rules"),
        ("create collation se (provider = icu, locale = 'se-NO')", "se"),
        ("create collation k (provider = icu, locale = 'und-u-kr-grek')", "kr"),
        ("select 'a' < 'b' collate \"se-x-icu\"", "se-x-icu"),
        ("create table t (a text collate \"yi-x-icu\")", "yi-x-icu"),
        ("select initcap('o''neil' collate \"en-x-icu\")", "initcap"),
        ("create table r (a text collate \"de-x-icu\") partition by range (a)", "partition"),
    ] {
        let (code, message) = fails(&mut s, &db, q);
        assert_eq!(code, "0A000", "{q}: {message}");
        assert!(message.contains(says), "{q}: {message}");
    }
    // libc's C and POSIX are bytes, which is what Chronos does anyway
    rows(&mut s, &db, "create collation bytes (provider = libc, locale = 'C')");
    assert_eq!(rows(&mut s, &db, "select 'B' < 'a' collate bytes"), ["t"]);
}

/// A libc locale's name (`en_US`, `de_DE.UTF-8`), as a COLLATE or in CREATE COLLATION, is the
/// ICU collation of that locale: Chronos has no libc locales.
#[test]
fn libc_locales_are_icu_ones() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    // linguistic order, not bytes: 'a' before 'B'
    assert_eq!(rows(&mut s, &db, "select 'a' < 'B' collate \"en_US\", 'a' < 'B' collate \"en_US.utf8\""), ["t|t"]);
    rows(&mut s, &db, "create collation l (provider = libc, locale = 'de_DE.UTF-8')");
    rows(&mut s, &db, "create collation l2 (lc_collate = 'en_US.UTF-8', lc_ctype = 'en_US.UTF-8')");
    assert_eq!(rows(&mut s, &db, "select 'a' < 'B' collate l, 'a' < 'B' collate l2"), ["t|t"]);
    rows(&mut s, &db, "create table t (a text collate \"sv_SE\")");
    rows(&mut s, &db, "insert into t values ('B'), ('a')");
    assert_eq!(rows(&mut s, &db, "select a from t order by a"), ["a", "B"]);
    // a locale ICU4X has no rules for is still refused
    assert_eq!(fails(&mut s, &db, "select 'a' < 'b' collate \"se_NO\"").0, "0A000");
}

/// An index on a collated column keeps that collation's order: ORDER BY ... LIMIT and range conditions read it when
/// the query wants that collation, and an index of another collation is left alone.
#[test]
fn indexes_are_read_only_for_their_own_collation() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    rows(&mut s, &db, "create table ix (id int primary key, s text collate \"sv-x-icu\", t text)");
    let vals: Vec<String> = (0..3000).map(|i| format!("({i}, 'w{}ä{i}', 'v{}')", i % 50, i % 40)).collect();
    rows(&mut s, &db, &format!("insert into ix values {}", vals.join(", ")));
    rows(&mut s, &db, "create index ix_s on ix (s)");
    rows(&mut s, &db, "create index ix_t_de on ix (t collate \"de-x-icu\")");
    let plan = |s: &mut Session, q: &str| rows(s, &db, &format!("explain {q}")).join("\n");
    let p = plan(&mut s, "select id from ix order by s limit 3");
    assert!(p.contains("Index Scan using ix_s"), "{p}");
    let p = plan(&mut s, "select id from ix order by s collate \"de-x-icu\" limit 3");
    assert!(!p.contains("ix_s"), "sv's index doesn't keep de's order: {p}");
    let p = plan(&mut s, "select id from ix order by t collate \"de-x-icu\" desc limit 3");
    assert!(p.contains("Index Scan Backward using ix_t_de"), "{p}");
    let p = plan(&mut s, "select id from ix order by t limit 3");
    assert!(!p.contains("ix_t_de"), "t sorts by bytes, which de's index doesn't keep: {p}");
    let p = plan(&mut s, "select id from ix where s between 'w1ä' and 'w1å'");
    assert!(p.contains("Index Scan using ix_s"), "{p}");
    let p = plan(&mut s, "select id from ix where s collate \"C\" between 'w1ä' and 'w1å'");
    assert!(!p.contains("ix_s"), "{p}");
    let p = plan(&mut s, "select id from ix where t = 'v7'");
    assert!(p.contains("Index Scan using ix_t_de"), "equality is the same in every collation here: {p}");
    // the rows an index read gives are the ones a sort gives
    let by_index = rows(&mut s, &db, "select id from ix order by s limit 20");
    let sorted = rows(&mut s, &db, "select id from (select id, s from ix offset 0) x order by s, id limit 20");
    assert_eq!(by_index, sorted);
}

/// A collation is an object of its world: made in a fork, it isn't in main until the fork merges, DIFF shows it (and
/// AS SQL writes it), and a column of it merges with it.
#[test]
fn collations_fork_merge_and_diff_with_their_world() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    rows(&mut s, &db, "create world wc");
    let mut w = Session::new("wc");
    rows(&mut w, &db, "create collation phone (provider = icu, locale = 'de-u-co-phonebk')");
    rows(&mut w, &db, "create table names (id int primary key, n text collate phone)");
    rows(&mut w, &db, "insert into names values (1, 'Müller'), (2, 'Mueller'), (3, 'Mull')");
    assert_eq!(rows(&mut w, &db, "select id from names order by n"), ["2", "1", "3"]);
    let (code, _) = fails(&mut s, &db, "select 'a' collate phone");
    assert_eq!(code, "42704", "not in main yet");
    let diff = run(&db, MAIN, "diff world wc", &[]).unwrap().pop().unwrap();
    let changes: Vec<String> =
        diff.rows.iter().map(|r| r.iter().map(Value::to_text).collect::<Vec<_>>().join("|")).collect();
    assert!(changes.iter().any(|c| c.contains("create collation") && c.contains("phone")), "{changes:?}");
    let sql = rows(&mut w, &db, "diff world wc as sql");
    assert!(
        sql.iter().any(|l| l.contains("create collation phone (provider = icu, locale = 'de-u-co-phonebk')")),
        "{sql:?}"
    );
    assert!(sql.iter().any(|l| l.contains("n text collate phone")), "{sql:?}");
    rows(&mut s, &db, "merge world wc");
    assert_eq!(rows(&mut s, &db, "select id from names order by n"), ["2", "1", "3"]);
    assert_eq!(
        rows(&mut s, &db, "select colliculocale from pg_collation where collname = 'phone'"),
        ["de-u-co-phonebk"]
    );
    // replayed into another database, the same order
    let (other, mut o) = (Db::new(), Session::new(MAIN));
    for stmt in &sql {
        o.run(&other, stmt, &[]).unwrap_or_else(|e| panic!("{stmt}: {} {}", e.code, e.message));
    }
    assert_eq!(rows(&mut o, &other, "select id from names order by n"), ["2", "1", "3"]);
}

/// A table's collated column keeps its collation when the database is opened again.
#[test]
fn a_column_keeps_its_collation_on_disk() {
    let dir = std::env::temp_dir().join(format!("chronos-collations-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(MAIN);
        rows(&mut s, &db, "create collation num (provider = icu, locale = 'und-u-kn-true')");
        rows(&mut s, &db, "create table v (id int primary key, tag text collate num, sv text collate \"sv-x-icu\")");
        rows(&mut s, &db, "insert into v values (1, 'x10', 'ö'), (2, 'x9', 'z'), (3, 'x100', 'å')");
        rows(&mut s, &db, "create index v_sv on v (sv)");
    }
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(rows(&mut s, &db, "select tag from v order by tag"), ["x9", "x10", "x100"]);
    assert_eq!(rows(&mut s, &db, "select sv from v order by sv limit 2"), ["z", "å"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The two errors pgdiff can't tell apart (it compares SQLSTATE classes): two explicit collations are 42P21, two
/// implicit ones are 42P21 where an order is needed and 42P22 where a comparison runs.
#[test]
fn derivation_errors_have_postgres_codes() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&mut s, &db, "create table e (a text collate \"de-x-icu\", b text collate \"sv-x-icu\")");
    rows(&mut s, &db, "insert into e values ('x', 'y')");
    for (q, want) in [
        ("select 'a' collate \"de-x-icu\" < 'b' collate \"sv-x-icu\"", "42P21"),
        ("select * from e order by a || b", "42P21"),
        ("select a from e union select b from e", "42P21"),
        ("select * from e where a < b", "42P22"),
        ("select * from e where a = b", "42P22"),
        ("select greatest(a, b) from e", "42P22"),
        ("select a from e union all select b from e order by 1", "42P22"),
        ("create table e2 as select a || b as ab from e", "42P22"),
        ("select 1 collate \"C\"", "42804"),
        ("select 'a' collate nope", "42704"),
    ] {
        assert_eq!(fails(&mut s, &db, q).0, want, "{q}");
    }
}

/// CREATE COLLATION takes only what ICU4X is checked to order as ICU4C does: the locales Postgres makes collations
/// of (and region aliases checked, `zh-TW`), and collation types other than `search`; a keyword's value ICU doesn't
/// know is XX000, as in Postgres.
#[test]
fn locales_icu4x_orders_otherwise_are_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    let make = |s: &mut Session, locale: &str| {
        let q = format!(
            "create collation c_{} (provider = icu, locale = '{locale}')",
            locale.replace(['-', '@', '='], "_")
        );
        s.run(&db, &q, &[]).map(|_| ()).map_err(|e| e.code)
    };
    for locale in [
        "ko-u-co-search",
        "ko-u-co-searchjl",
        "de-u-co-search",
        "zh-u-co-standard",
        "sr-ME",
        "uz-AF",
        "pa-PK",
        "az-IR",
        "mn-CN",
        "ky-CN",
        "ug-KZ",
        "sh",
        "zh-Latn",
        "ja-Latn",
        "mr",
        "kok-IN",
        "de-u-co-bogus",
        "en-u-kr-grek",
    ] {
        assert_eq!(make(&mut s, locale), Err("0A000"), "{locale}");
    }
    for locale in ["en-u-kf-xyz", "en-u-kc-maybe", "en-u-kn-maybe", "en@colStrength=bogus", "ja-u-co-private-kana"] {
        assert_eq!(make(&mut s, locale), Err("XX000"), "{locale}");
    }
    for locale in ["zh-TW", "sr_RS", "de-DE", "und-u-kn-true", "es-u-co-trad", "en-u-ks-level2"] {
        assert_eq!(make(&mut s, locale), Ok(()), "{locale}");
    }
    assert_eq!(fails(&mut s, &db, "select 'a' collate \"mr-x-icu\"").0, "0A000");
}

/// Where ICU order isn't carried through yet, the query is refused rather than answered by bytes: text against a
/// subquery's rows, and arrays and rows of collated text ordered.
#[test]
fn orders_not_carried_through_are_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&mut s, &db, "create table sq (id int primary key, x text collate \"en-x-icu\")");
    rows(&mut s, &db, "insert into sq values (1, 'a'), (2, 'B'), (3, 'b')");
    for q in [
        "select id from sq where x > all (select 'B')",
        "select id from sq where x < all (values ('C'), ('c'))",
        "select id from sq where x <= any (select x from sq)",
        "select (x, 1) < (select 'B', 1) from sq",
        "select id from sq order by array[x]",
        "select id from sq order by (x, id)",
        "select max(array[x]) from sq",
        "select array[x] > array['b'] from sq",
        "select greatest(array[x], array['b']) from sq",
        "select id, rank() over (order by array[x]) from sq",
        "select string_agg(x, ',' order by (x, id)) from sq",
    ] {
        assert_eq!(fails(&mut s, &db, q).0, "0A000", "{q}");
    }
    // equality is bytes in every collation, and stays
    assert_eq!(rows(&mut s, &db, "select id from sq where x = any (select 'B')"), ["2"]);
    assert_eq!(rows(&mut s, &db, "select id from sq order by array[x collate \"C\"]"), ["2", "1", "3"]);
}

/// DROP COLLATION reads what names a collation by its tokens: text that isn't a name (a string, characters whose
/// lowercase is longer) neither counts nor breaks it. A default naming one is refused (Chronos keeps defaults as
/// values), CASCADE doesn't drop a materialized view yet, and RENAME is refused while SQL kept names it.
#[test]
fn dependencies_are_found_by_tokens() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&mut s, &db, "create collation german (provider = icu, locale = 'de-u-co-phonebk')");
    rows(&mut s, &db, "create table d (id int primary key, a text)");
    rows(&mut s, &db, "create view dv as select 'İİİİİİİİİİİİİİİİ collate german' as s, a collate german as a from d");
    assert_eq!(fails(&mut s, &db, "alter collation german rename to deutsch").0, "0A000");
    assert_eq!(fails(&mut s, &db, "drop collation german").0, "2BP01");
    rows(&mut s, &db, "drop view dv");
    rows(&mut s, &db, "create view dv as select 'İİİİİ collate german' as s from d");
    rows(&mut s, &db, "alter collation german rename to deutsch");
    assert_eq!(fails(&mut s, &db, "create table dd (a text default 'x' collate deutsch)").0, "0A000");
    rows(&mut s, &db, "create table dc (id int primary key, a text, check (a collate deutsch > 'a'))");
    assert_eq!(fails(&mut s, &db, "alter collation deutsch rename to german").0, "0A000");
    rows(&mut s, &db, "drop table dc");
    rows(&mut s, &db, "create materialized view dm as select a collate deutsch as a from d");
    assert_eq!(fails(&mut s, &db, "drop collation deutsch cascade").0, "0A000");
    rows(&mut s, &db, "drop materialized view dm");
    rows(&mut s, &db, "drop collation deutsch");
    assert_eq!(rows(&mut s, &db, "select s from dv"), Vec::<String>::new());
}

/// A COLLATE name that names nothing isn't kept: sessions sending new names don't grow what the server holds.
#[test]
fn unknown_collation_names_are_not_kept() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&mut s, &db, "select 'a' collate \"de-x-icu\"");
    let before = chronos::sql::collations_in_use();
    for i in 0..2000 {
        let q = format!("select 'a' collate \"nope_{i}\"");
        assert_eq!(fails(&mut s, &db, &q).0, "42704");
    }
    // (tests running beside this one keep the few collations they make)
    let after = chronos::sql::collations_in_use();
    assert!(after < before + 100, "{after} after 2000 unknown names, {before} before");
}

/// The names main read as collations of bytes, any case (`collate c`, `posix`, `"C.utf8"`), still read: SQL it kept
/// in views, CHECKs and functions opens and runs after an upgrade.
#[test]
fn byte_collation_names_main_read_still_read() {
    let dir = std::env::temp_dir().join(format!("chronos-lenient-collations-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(MAIN);
        rows(&mut s, &db, "create table l (id int primary key, a text, check (a collate c >= ''))");
        rows(&mut s, &db, "insert into l values (1, 'b'), (2, 'B')");
        rows(&mut s, &db, "create view lv as select a collate posix as a from l");
        rows(
            &mut s,
            &db,
            "create function lf(x text) returns bool language sql as $$ select x collate \"C.utf8\" < 'a' $$",
        );
    }
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    rows(&mut s, &db, "insert into l values (3, 'a')");
    assert_eq!(rows(&mut s, &db, "select a from lv order by a"), ["B", "a", "b"]);
    assert_eq!(rows(&mut s, &db, "select lf('B'), lf('b')"), ["t|f"]);
    assert_eq!(rows(&mut s, &db, "select 'a' collate \"C.UTF-8\" < 'B', 'a' collate \"c.UTF8\" < 'B'"), ["f|f"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// DIFF AS SQL of a column's new collation keeps its type's length; a merge that would leave a column naming a
/// collation the other side dropped is refused.
#[test]
fn worlds_keep_column_types_and_collations_whole() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    rows(&mut s, &db, "create collation phone (provider = icu, locale = 'de-u-co-phonebk')");
    rows(&mut s, &db, "create collation phone2 (provider = icu, locale = 'de-u-co-phonebk')");
    rows(&mut s, &db, "create table v (id int primary key, n varchar(5) collate phone)");
    rows(&mut s, &db, "create world wv");
    let mut w = Session::new("wv");
    rows(&mut w, &db, "alter table v alter column n type varchar(5) collate phone2");
    let sql = rows(&mut w, &db, "diff world wv as sql");
    assert!(sql.iter().any(|l| l.contains("type varchar(5) collate phone2")), "{sql:?}");
    // one side drops a collation the other gives a column
    rows(&mut s, &db, "create world wd");
    let mut d = Session::new("wd");
    rows(&mut d, &db, "drop collation phone2");
    rows(&mut s, &db, "create table u (id int primary key, n text collate phone2)");
    let e = s.run(&db, "merge world wd", &[]).expect_err("merge");
    assert_eq!(e.code, "2BP01", "{}", e.message);
}

/// A child's column and its parent's must have the same collation: each conflict is 42P21 with Postgres 16's
/// message and detail (a column written without COLLATE has the default).
#[test]
fn inherited_collation_conflicts_have_postgres_messages() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for q in [
        "create table hde (s text collate \"de-x-icu\", n int)",
        "create table hsv (s text collate \"sv-x-icu\")",
        "create table hp (n int)",
        "create table hk (s text collate \"sv-x-icu\") inherits (hp)",
        "create table hc (s text collate \"sv-x-icu\", n int)",
        "create table hpl (s text collate \"de-x-icu\", n int) partition by list (n)",
        "create table hpa (s text, n int)",
    ] {
        rows(&mut s, &db, q);
    }
    for (q, message, detail) in [
        (
            "create table h1 (s text collate \"sv-x-icu\") inherits (hde)",
            "column \"s\" has a collation conflict",
            Some("\"de-x-icu\" versus \"sv-x-icu\""),
        ),
        (
            "create table h2 (s text) inherits (hde)",
            "column \"s\" has a collation conflict",
            Some("\"de-x-icu\" versus \"default\""),
        ),
        (
            "create table h3 () inherits (hde, hsv)",
            "inherited column \"s\" has a collation conflict",
            Some("\"de-x-icu\" versus \"sv-x-icu\""),
        ),
        (
            "alter table hp add column s text collate \"de-x-icu\"",
            "child table \"hk\" has different collation for column \"s\"",
            Some("\"de-x-icu\" versus \"sv-x-icu\""),
        ),
        ("alter table hc inherit hde", "child table \"hc\" has different collation for column \"s\"", None),
        (
            "alter table hpl attach partition hpa for values in (1)",
            "child table \"hpa\" has different collation for column \"s\"",
            None,
        ),
    ] {
        let e = s.run(&db, q, &[]).expect_err(q);
        assert_eq!((e.code, e.message.as_str()), ("42P21", message), "{q}");
        assert_eq!(e.info.and_then(|i| i.detail).as_deref(), detail, "{q}");
    }
    // a partition's columns are its parent's: a COLLATE written for one is taken and the parent's kept
    rows(&mut s, &db, "create table hp1 partition of hpl (s collate \"sv-x-icu\") for values in (2)");
    assert_eq!(
        rows(
            &mut s,
            &db,
            "select attcollation::regcollation from pg_attribute where attrelid = 'hp1'::regclass and attname = 's'"
        ),
        ["\"de-x-icu\""]
    );
}

/// DROP COLLATION ... CASCADE drops the columns of the collation, but a primary key's column can't be dropped here
/// (rows are kept by their key): that is refused, saying so, and nothing is dropped.
#[test]
fn drop_collation_cascade_to_a_key_column_is_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    rows(&mut s, &db, "create collation pkc (provider = icu, locale = 'de')");
    rows(&mut s, &db, "create table pk (s text collate pkc primary key, n int)");
    rows(&mut s, &db, "insert into pk values ('x', 1)");
    let (code, message) = fails(&mut s, &db, "drop collation pkc cascade");
    assert_eq!(code, "0A000", "{message}");
    assert!(message.contains("primary key") && message.contains("pk"), "{message}");
    assert_eq!(rows(&mut s, &db, "select count(*) from pg_collation where collname = 'pkc'"), ["1"]);
    assert_eq!(rows(&mut s, &db, "select * from pk"), ["x|1"]);
}
