//! Regression tests for bugs found by the 2026-09-24 capability audit.
use chronos::{Db, MAIN, sql};

fn run(db: &Db, q: &str) -> String {
    format!("{:?}", sql::run(db, MAIN, q, &[]).unwrap())
}

#[test]
fn aggregate_inside_an_operator_types_without_panicking() {
    let db = Db::new();
    run(&db, "create table t (id int primary key, v text)");
    run(&db, "insert into t values (1, 'a'), (2, 'b')");
    let out = run(&db, "select count(*) || 'x' from t");
    assert!(out.contains("2x"), "{out}");
}

#[test]
fn nearest_to_null_on_an_indexed_vector_column_is_not_a_crash() {
    let db = Db::new();
    run(&db, "create table emb (id int primary key, e vector(3))");
    run(&db, "insert into emb select i, ('[' || i || ',1,2]')::vector from generate_series(1, 50) i");
    run(&db, "create index on emb using hnsw (e vector_l2_ops)");
    // Postgres: every distance is NULL, so any 3 rows come back
    let rows = sql::run(&db, MAIN, "select id from emb order by e <-> null limit 3", &[]).unwrap();
    assert!(format!("{rows:?}").matches("Int(").count() >= 3, "{rows:?}");
    let p = sql::run(&db, MAIN, "select id from emb order by e <-> $1 limit 3", &[chronos::sql::Value::Null]);
    assert!(p.is_ok(), "{p:?}");
}

#[test]
fn merges_settled_by_columns_are_in_history() {
    let dir = std::env::temp_dir().join(format!("chronos-audit-history-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let q = |b: &str, s: &str| format!("{:?}", sql::run(&db, b, s, &[]).unwrap());
    q(MAIN, "create table h (id int primary key, a text, b text)");
    q(MAIN, "insert into h values (1, 'x', 'x')");
    q(MAIN, "create world hx");
    q("hx", "update h set a = 'hx' where id = 1");
    q(MAIN, "update h set b = 'main' where id = 1");
    q(MAIN, "merge world hx by columns");
    let h = q(MAIN, "show history limit 10");
    assert!(h.contains("merged hx"), "{h}");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn version_and_unknown_options_make_no_database_folder() {
    let dir = std::env::temp_dir().join(format!("chronos-audit-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let chronos =
        |a: &str| std::process::Command::new(env!("CARGO_BIN_EXE_chronos")).arg(a).current_dir(&dir).output().unwrap();
    let v = chronos("--version");
    assert!(v.status.success() && String::from_utf8_lossy(&v.stdout).starts_with("chronos "), "{v:?}");
    assert!(!chronos("--verbose").status.success());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no folder was created");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn integer_key_ranges_read_by_key_and_match_a_scan() {
    let db = Db::new();
    run(&db, "create table t (id bigint primary key, v int)");
    run(&db, "insert into t select i, i % 7 from generate_series(-50, 5000) i");
    let plan = run(&db, "explain select count(*) from t where id between 10 and 20");
    assert!(plan.contains("Index Scan using t_pkey"), "{plan}");
    for (cond, want) in [
        ("id between 10 and 20", 11),
        ("id >= 10 and id < 20", 10),
        ("id > 10 and id <= 20", 10),
        ("id between -5 and 5", 11),
        ("id between 4990 and 6000", 11),
        ("id between 20 and 10", 0),
        ("id between 1 and 200000", 5000), // wider than the lookup limit: a scan, same answer
        ("id > 4995 and id < 9223372036854775807", 5),
    ] {
        let out = run(&db, &format!("select count(*) from t where {cond}"));
        assert!(out.contains(&format!("Int({want})")), "{cond}: {out}");
    }
    let s = run(&db, "select sum(v) from t where id between 100 and 200 and v = 3");
    let scan = run(&db, "select sum(v) from t where id + 0 between 100 and 200 and v = 3");
    assert_eq!(s, scan);
}

#[test]
fn equality_on_a_big_table_scans_instead_of_indexing_it_all_in_memory() {
    let db = Db::new();
    run(&db, "create table small (id int primary key, v text)");
    run(&db, "insert into small select i, 'v' || (i % 10) from generate_series(1, 1000) i");
    let plan = run(&db, "explain select count(*) from small where v = 'v3'");
    assert!(plan.contains("search index"), "{plan}");
    // about 40 MB of rows: over the default budget (CHRONOS_WORK_MEM / 8 = 32 MB)
    run(&db, "create table big (id int primary key, v text)");
    for c in 0..8 {
        let (a, b) = (c * 5000 + 1, c * 5000 + 5000);
        run(&db, &format!("insert into big select i, repeat(md5(i::text), 32) from generate_series({a}, {b}) i"));
    }
    run(&db, "update big set v = 'changed' where id between 1 and 10");
    let plan = run(&db, "explain select count(*) from big where v = 'changed'");
    assert!(plan.contains("Seq Scan"), "{plan}");
    let out = run(&db, "select count(*) from big where v = 'changed'");
    assert!(out.contains("Int(10)"), "{out}");
}

#[test]
fn statement_timeout_stops_a_runaway_query() {
    let db = Db::new();
    let mut s = sql::Session::new(MAIN);
    let mut q = |text: &str| s.run(&db, text, &[]);
    q("set statement_timeout = '200ms'").unwrap();
    assert!(format!("{:?}", q("show statement_timeout").unwrap()).contains("200ms"));
    let t = std::time::Instant::now();
    let e = q("select count(*) from generate_series(1, 20000) a, generate_series(1, 20000) b").unwrap_err();
    assert_eq!(e.code, "57014", "{e:?}");
    assert!(t.elapsed() < std::time::Duration::from_secs(10), "stopped after {:?}", t.elapsed());
    assert!(format!("{:?}", q("select 1 + 1").unwrap()).contains("Int(2)"), "the session goes on");
    q("reset statement_timeout").unwrap();
    assert!(format!("{:?}", q("show statement_timeout").unwrap()).contains("Text(\"0\")"));
    q("set statement_timeout to 5000").unwrap();
    assert!(format!("{:?}", q("show statement_timeout").unwrap()).contains("5s"));
    assert!(q("set statement_timeout = 'soon'").is_err());
}
