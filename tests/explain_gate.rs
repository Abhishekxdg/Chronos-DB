//! chronos.explain: EXPLAIN (COSTS OFF) shows how Chronos runs a statement unless the session asks for the plan
//! PostgreSQL 16 would choose (`postgres`), which then comes with a NOTICE that Chronos may run it differently. The
//! regression suite's exact mode (CHRONOS_PG_EXACT) has `postgres` as its default; these tests run without it.

use chronos::sql::{Session, Value, take_notices};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code, e.message),
    }
}

const Q: &str = "explain (costs off) select * from pl where a = 1";

/// The plan's lines, and whether the NOTICE that it is PostgreSQL's came with them.
fn plan(db: &Db, s: &mut Session, q: &str) -> (Vec<String>, bool) {
    take_notices();
    let lines = run(db, s, q).into_iter().map(|r| r[0].clone()).collect();
    let said = take_notices().iter().any(|n| n.message == "this is the plan PostgreSQL 16 would choose");
    (lines, said)
}

fn setup() -> (Db, Session) {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table pl (a int, b text)");
    (db, s)
}

#[test]
fn chronos_steps_by_default_and_postgres_plan_when_asked() {
    let (db, mut s) = setup();
    assert_eq!(one(&db, &mut s, "show chronos.explain"), "chronos");
    let (own, said) = plan(&db, &mut s, Q);
    assert!(!said, "{own:?}");
    run(&db, &mut s, "set chronos.explain = postgres");
    assert_eq!(one(&db, &mut s, "show chronos.explain"), "postgres");
    take_notices();
    let pg: Vec<String> = run(&db, &mut s, Q).into_iter().map(|r| r[0].clone()).collect();
    assert_eq!(pg, vec!["Seq Scan on pl".to_string(), "  Filter: (a = 1)".to_string()]);
    assert_ne!(own, pg, "the default shows Chronos's steps, not PostgreSQL's plan");
    let notices = take_notices();
    let n = notices.iter().find(|n| n.message == "this is the plan PostgreSQL 16 would choose").expect("the NOTICE");
    assert_eq!(n.level, "NOTICE");
    assert_eq!(n.detail.as_deref(), Some("Chronos may run the query differently."));
    assert_eq!(n.hint.as_deref(), Some("SET chronos.explain = chronos shows how Chronos runs it."));
    // (only EXPLAIN (COSTS OFF) without ANALYZE shows it: the others are Chronos's steps, with no NOTICE)
    assert!(!plan(&db, &mut s, "explain select * from pl where a = 1").1);
    assert!(!plan(&db, &mut s, "explain (analyze, costs off) select * from pl where a = 1").1);
    run(&db, &mut s, "reset chronos.explain");
    assert_eq!(one(&db, &mut s, "show chronos.explain"), "chronos");
    assert_eq!(plan(&db, &mut s, Q), (own, false));
}

#[test]
fn it_is_a_setting_as_postgres_has_them() {
    let (db, mut s) = setup();
    let own = plan(&db, &mut s, Q).0;
    // SET LOCAL, undone by ROLLBACK
    run(&db, &mut s, "begin");
    run(&db, &mut s, "set local chronos.explain = postgres");
    assert!(plan(&db, &mut s, Q).1);
    run(&db, &mut s, "rollback");
    assert_eq!(plan(&db, &mut s, Q), (own.clone(), false));
    // set_config, and DISCARD ALL putting it back
    run(&db, &mut s, "select set_config('chronos.explain', 'postgres', false)");
    assert!(plan(&db, &mut s, Q).1);
    run(&db, &mut s, "discard all");
    assert_eq!(plan(&db, &mut s, Q), (own, false));
    // any case of its name is it, and its values are checked
    run(&db, &mut s, "set \"Chronos.Explain\" = POSTGRES");
    assert_eq!(one(&db, &mut s, "show chronos.explain"), "postgres");
    for q in ["set chronos.explain = mysql", "select set_config('chronos.explain', 'mysql', false)"] {
        let (code, msg) = fails(&db, &mut s, q);
        assert_eq!(code, "22023", "{q}");
        assert_eq!(msg, "invalid value for parameter \"chronos.explain\": \"mysql\"", "{q}");
    }
}

#[test]
fn show_all_and_pg_settings_list_it_set_or_not() {
    let (db, mut s) = setup();
    let listed = |db: &Db, s: &mut Session| -> Vec<Vec<String>> {
        run(db, s, "show all").into_iter().filter(|r| r[0] == "chronos.explain").collect()
    };
    let rows = listed(&db, &mut s);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0][1], "chronos");
    assert!(rows[0][2].starts_with("Which plan EXPLAIN (COSTS OFF) shows"), "{rows:?}");
    assert_eq!(
        run(&db, &mut s, "select setting, source from pg_settings where name = 'chronos.explain'"),
        vec![vec!["chronos".to_string(), "default".to_string()]]
    );
    run(&db, &mut s, "set chronos.explain = postgres");
    assert_eq!(listed(&db, &mut s).len(), 1, "listed once when set");
    assert_eq!(
        run(&db, &mut s, "select setting, source from pg_settings where name = 'chronos.explain'"),
        vec![vec!["postgres".to_string(), "session".to_string()]]
    );
}
