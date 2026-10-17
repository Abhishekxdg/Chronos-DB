//! Second review, parser and expressions: trees deeper than the parser counted (and parallel
//! scans evaluating them), type modifiers running past a copied expression, type names other
//! statements could make a prepared statement forget, enum label limits, jsonb order, and
//! bigint %, LIKE's escape and interval negation.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn code(db: &Db, sql: &str) -> &'static str {
    match run(db, MAIN, sql, &[]) {
        Ok(_) => "ok",
        Err(e) => e.code,
    }
}

/// The first value of the last statement's first row, as text.
fn one(db: &Db, sql: &str) -> String {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{}: {}", e.message, &sql[..sql.len().min(80)]));
    out.last().unwrap().rows[0][0].to_text()
}

/// `f` on a thread with a 2 MB stack.
fn small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().stack_size(2 << 20).spawn(f).unwrap().join().unwrap();
}

/// `n` levels of `b NOT BETWEEN (...) AND true`: the parser counts a level per parenthesis, the
/// tree has three (Not, And, >=). True where b is false.
fn between(n: usize) -> String {
    let mut e = "true".to_string();
    for _ in 0..n {
        e = format!("b not between ({e}) and true");
    }
    e
}

#[cfg_attr(debug_assertions, ignore = "measures release-build stack use; the VM runs it in release")]
#[test]
fn trees_deeper_than_parsed_are_refused_on_a_small_stack() {
    small_stack(|| {
        let db = Db::new();
        run(&db, MAIN, "create table s (x int, b bool); insert into s values (1, false), (2, true), (3, false)", &[])
            .unwrap();
        // ~1170 levels of tree, ~390 parsed: past what a 2 MB stack evaluates without the check
        assert_eq!(code(&db, &format!("select count(*) from s where {}", between(390))), "54001");
        assert_eq!(code(&db, &format!("select count(*) from s where {}", between(150))), "54001");
        assert_eq!(code(&db, &format!("create table c (b bool check ({}))", between(150))), "54001");
        // near the limit: runs, right
        assert_eq!(one(&db, &format!("select count(*) from s where {}", between(90))), "2");
        let casts = format!("x{}", "::numeric(10,2)".repeat(190));
        assert_eq!(one(&db, &format!("select count(*) from s where {casts} > 1")), "2");
        let casts = format!("x{}", "::numeric(10,2)".repeat(390));
        assert_eq!(code(&db, &format!("select {casts} from s")), "54001");
    });
}

#[cfg_attr(debug_assertions, ignore = "measures release-build stack use; the VM runs it in release")]
#[test]
fn parallel_scans_evaluate_deep_conditions_safely() {
    small_stack(|| {
        let db = Db::new();
        run(&db, MAIN, "create table big (x int, b bool)", &[]).unwrap();
        run(&db, MAIN, "insert into big select g, g % 2 = 0 from generate_series(1, 12000) g", &[]).unwrap();
        assert_eq!(one(&db, &format!("select count(*) from big where {}", between(90))), "6000");
        let casts = format!("x{}", "::numeric(10,2)".repeat(190));
        assert_eq!(one(&db, &format!("select count(*) from big where {casts} > 1")), "11999");
        assert_eq!(code(&db, &format!("select * from big where {}", between(390))), "54001");
    });
}

#[test]
fn type_modifiers_take_numbers_only() {
    let db = Db::new();
    run(&db, MAIN, "create table t (a text)", &[]).unwrap();
    for sql in [
        "create index on t ((a::varchar(])))",
        "create table t2 (a text check (a::varchar(]) <> ''))",
        "create view v as select 'a'::varchar(;) x",
        "select 'a'::varchar(a)",
        "select now()::timestamp(])",
        "create index on t (a) where a::varchar(]) <> ''",
    ] {
        assert_eq!(code(&db, sql), "42601", "{sql}");
    }
    for sql in [
        "select 'a'::varchar(20), 'b'::character varying(3), 1.5::numeric(10, 2), now()::timestamp(3)",
        "create table g (p geography(Point, 4326), q varchar(10))",
        "create index on t ((a::varchar(10)))",
        "create view v as select 'a'::varchar(5) x",
    ] {
        assert_eq!(code(&db, sql), "ok", "{sql}");
    }
}

#[test]
fn other_statements_type_names_dont_evict_a_prepared_statements() {
    let db = Db::new();
    run(&db, MAIN, "create type role2 as enum ('admin', 'user')", &[]).unwrap();
    let mut s = Session::new(MAIN);
    s.run(&db, "prepare p as select cast($1 as \"role2\")", &[]).unwrap();
    s.run(&db, "prepare q as select 'user'::role2", &[]).unwrap();
    // another client names many types that don't exist
    let names: Vec<String> = (0..5000).map(|i| format!("1::zz{i}")).collect();
    assert_eq!(code(&db, &format!("select {}", names.join(", "))), "42704");
    for i in 0..5000 {
        let _ = code(&db, &format!("select null::yy{i}"));
    }
    assert_eq!(s.run(&db, "execute p('admin')", &[]).unwrap()[0].rows[0][0].to_text(), "admin");
    assert_eq!(s.run(&db, "execute q", &[]).unwrap()[0].rows[0][0].to_text(), "user");
    assert_eq!(s.run(&db, "execute p('nope')", &[]).unwrap_err().code, "22P02");
}

#[test]
fn enum_labels_are_capped() {
    let db = Db::new();
    let labels = |n: usize| (0..n).map(|i| format!("'l{i}'")).collect::<Vec<_>>().join(",");
    assert_eq!(code(&db, &format!("create type big as enum ({})", labels(65537))), "54000");
    assert_eq!(code(&db, &format!("create type wide as enum ({})", labels(65536))), "ok");
    assert_eq!(code(&db, "alter type wide add value 'one_more'"), "54000");
}

#[test]
fn jsonb_compares_numbers_by_value_in_postgres_order() {
    let db = Db::new();
    assert_eq!(one(&db, r#"select '{"a":1.0}'::jsonb = '{"a":1}'::jsonb"#), "true");
    assert_eq!(one(&db, "select '10'::jsonb > '9'::jsonb"), "true");
    assert_eq!(one(&db, "select '[1, 2.50]'::jsonb = '[1.0, 2.5]'::jsonb"), "true");
    assert_eq!(one(&db, "select '-0.0'::jsonb = '0'::jsonb"), "true");
    assert_eq!(one(&db, "select '1.5'::jsonb < '2'::jsonb"), "true");
    let out = run(
        &db,
        MAIN,
        r#"select j::text from (select '{}'::jsonb j union all select '[1]' union all select 'true' union all select '1' union all select '"a"' union all select 'null' union all select '[]' union all select '{"a":1,"b":2}' union all select '[1,2]') v order by v.j"#,
        &[],
    )
    .unwrap();
    let got: Vec<String> = out[0].rows.iter().map(|r| r[0].to_text()).collect();
    assert_eq!(got, ["[]", "null", "\"a\"", "1", "true", "[1]", "[1, 2]", "{}", "{\"a\": 1, \"b\": 2}"]);
    assert_eq!(
        one(
            &db,
            "select count(distinct j) from (select '1'::jsonb j union all select '1.0' union all select '1.00') v"
        ),
        "1"
    );
}

#[test]
fn bigint_mod_like_escape_and_interval_negation() {
    let db = Db::new();
    assert_eq!(one(&db, "select (-9223372036854775807 - 1) % -1"), "0");
    assert_eq!(code(&db, r"select 'a' like 'a\'"), "22025");
    assert_eq!(one(&db, r"select 'a\' like 'a\\'"), "true");
    assert_eq!(one(&db, "select -interval '1 day 2 hours'"), one(&db, "select interval '-1 day -2 hours'"));
    assert_eq!(one(&db, "select now() - -interval '1 day' > now()"), "true");
}
