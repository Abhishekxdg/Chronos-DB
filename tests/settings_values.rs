//! VALUES lists as queries (in FROM, IN, WITH, on their own), and settings: SHOW,
//! current_setting(), set_config(), SET / SET LOCAL / RESET of custom settings.

use chronos::sql::{Output, Session, Value};
use chronos::{Db, MAIN};

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn run(db: &Db, s: &mut Session, sql: &str) -> Output {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap()
}

fn q(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    rows(&run(db, s, sql))
}

fn fails(db: &Db, s: &mut Session, sql: &str) -> &'static str {
    s.run(db, sql, &[]).expect_err(sql).code
}

fn setup() -> (Db, Session) {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    q(&db, &mut s, "create table steps (id int primary key, name text not null)");
    q(&db, &mut s, "insert into steps values (1, 'one'), (2, 'two')");
    (db, s)
}

#[test]
fn values_as_a_query() {
    let (db, mut s) = setup();
    let got = q(&db, &mut s, "select * from (values (2, 'b'), (1, 'a')) as t(id, name) order by id");
    assert_eq!(got, ["1|a", "2|b"]);
    // unnamed, the columns are column1, column2, ...
    let o = run(&db, &mut s, "select * from (values (1, 'x')) v");
    let names: Vec<&str> = o.columns.as_ref().unwrap().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["column1", "column2"]);
    assert_eq!(q(&db, &mut s, "select column2 from (values (1, 'x')) v"), ["x"]);
    // on its own, with ORDER BY and LIMIT
    assert_eq!(q(&db, &mut s, "values (1, 'a'), (2, 'b') order by 1 desc"), ["2|b", "1|a"]);
    assert_eq!(q(&db, &mut s, "values (1), (2), (3) limit 2"), ["1", "2"]);
    // joined to a table, in IN, in WITH, and feeding an INSERT
    let joined = "select s.name, v.label from steps s join (values (1, 'first'), (2, 'second')) v(id, label) on v.id = s.id order by s.id";
    assert_eq!(q(&db, &mut s, joined), ["one|first", "two|second"]);
    assert_eq!(q(&db, &mut s, "select name from steps where id in (values (2), (3))"), ["two"]);
    assert_eq!(q(&db, &mut s, "with v as (values (1), (2), (3)) select sum(column1) from v"), ["6"]);
    q(&db, &mut s, "insert into steps select * from (values (5, 'five'), (6, 'six')) v");
    assert_eq!(q(&db, &mut s, "select count(*) from steps"), ["4"]);
    // nulls, expressions and parameters in the rows
    assert_eq!(q(&db, &mut s, "select * from (values (1, null), (2, 'b')) v order by 1"), ["1|null", "2|b"]);
    let got = s.run(&db, "select column1 * 2 from (values ($1::int), (1 + 1)) v order by 1", &[Value::Int(5)]).unwrap();
    assert_eq!(rows(&got[0]), ["4", "10"]);
    assert_eq!(fails(&db, &mut s, "values (1, 2), (3)"), "42601");
}

#[test]
fn postgres_settings_show_what_chronos_does() {
    let (db, mut s) = setup();
    for (name, want) in [
        ("server_version", "16.15"),
        ("server_version_num", "160015"),
        ("TimeZone", "UTC"),
        ("transaction_isolation", "read committed"),
        ("lc_collate", "C"),
        ("standard_conforming_strings", "on"),
        ("search_path", "\"$user\", public"),
        ("statement_timeout", "0"),
    ] {
        assert_eq!(q(&db, &mut s, &format!("select current_setting('{name}')")), [want], "{name}");
        assert_eq!(q(&db, &mut s, &format!("show {name}")), [want], "SHOW {name}");
    }
    assert_eq!(q(&db, &mut s, "show transaction isolation level"), ["read committed"]);
    assert_eq!(q(&db, &mut s, "show time zone"), ["UTC"]);
    let o = run(&db, &mut s, "show timezone");
    assert_eq!(o.columns.as_ref().unwrap()[0].0, "TimeZone", "named as Postgres names it");
    // what the session's own statements set
    q(&db, &mut s, "set search_path to x, public; set statement_timeout = 5000");
    assert_eq!(
        q(&db, &mut s, "select current_setting('search_path'), current_setting('statement_timeout')"),
        ["x, public|5s"]
    );
    // a setting that would change answers if Chronos honored it as Postgres does is refused, not
    // accepted and ignored, and SHOW says what's in effect
    assert_eq!(fails(&db, &mut s, "set datestyle = 'nonsense'"), "22023");
    assert_eq!(q(&db, &mut s, "show datestyle"), ["ISO, MDY"]);
    // one it has nothing to do for is kept and read back, as Postgres writes it
    q(&db, &mut s, "set work_mem = '64MB'");
    assert_eq!(q(&db, &mut s, "show work_mem"), ["64MB"]);
    // unknown, read-only
    assert_eq!(fails(&db, &mut s, "select current_setting('no_such_thing')"), "42704");
    assert_eq!(fails(&db, &mut s, "show no_such_thing"), "42704");
    assert_eq!(q(&db, &mut s, "select current_setting('no_such_thing', true)"), ["null"]);
    assert_eq!(fails(&db, &mut s, "set server_version = '1'"), "55P02");
    assert_eq!(fails(&db, &mut s, "select set_config('server_version', '1', false)"), "55P02");
}

#[test]
fn custom_settings_for_the_session_and_the_transaction() {
    let (db, mut s) = setup();
    q(&db, &mut s, "set app.current_rep = 'one'");
    assert_eq!(q(&db, &mut s, "select current_setting('app.current_rep')"), ["one"]);
    assert_eq!(q(&db, &mut s, "show app.current_rep"), ["one"]);
    // the pattern row-level filters use
    assert_eq!(q(&db, &mut s, "select id from steps where name = current_setting('app.current_rep')"), ["1"]);
    assert_eq!(q(&db, &mut s, "select set_config('app.current_rep', 'two', false)"), ["two"]);
    assert_eq!(q(&db, &mut s, "select id from steps where name = current_setting('app.current_rep')"), ["2"]);
    // another session has its own
    let mut other = Session::new(MAIN);
    assert_eq!(q(&db, &mut other, "select current_setting('app.current_rep', true)"), ["null"]);
    assert_eq!(fails(&db, &mut other, "select current_setting('app.current_rep')"), "42704");

    // SET LOCAL and set_config(..., true): until the transaction ends
    q(&db, &mut s, "begin");
    q(&db, &mut s, "set local app.org = 'acme'");
    q(&db, &mut s, "select set_config('app.region', 'south', true)");
    assert_eq!(q(&db, &mut s, "select current_setting('app.org'), current_setting('app.region')"), ["acme|south"]);
    q(&db, &mut s, "commit");
    // (a custom setting, once made, stays as an empty one: Postgres says '', not NULL)
    assert_eq!(q(&db, &mut s, "select current_setting('app.org', true), current_setting('app.region', true)"), ["|"]);
    // outside a transaction SET LOCAL does nothing (Postgres warns), but the name exists after it
    q(&db, &mut s, "set local app.org2 = 'acme'");
    assert_eq!(q(&db, &mut s, "select current_setting('app.org2', true)"), [""]);

    // RESET one, RESET ALL, application_name
    q(&db, &mut s, "set application_name = 'crm-worker'");
    assert_eq!(q(&db, &mut s, "show application_name"), ["crm-worker"]);
    q(&db, &mut s, "reset app.current_rep");
    assert_eq!(q(&db, &mut s, "select current_setting('app.current_rep', true)"), [""]);
    q(&db, &mut s, "set app.a = 1; set search_path to x");
    q(&db, &mut s, "reset all");
    assert_eq!(
        q(
            &db,
            &mut s,
            "select current_setting('app.a', true), current_setting('application_name'), current_setting('search_path')"
        ),
        ["||\"$user\", public"]
    );
}

#[test]
fn reset_puts_the_sessions_own_settings_back() {
    let (db, mut s) = setup();
    q(
        &db,
        &mut s,
        "set time zone 'Asia/Kolkata'; set hnsw.ef_search = 50; set statement_timeout = 5000; set search_path to x",
    );
    q(&db, &mut s, "reset statement_timeout; reset search_path; reset hnsw.ef_search");
    assert_eq!(q(&db, &mut s, "show statement_timeout"), ["0"]);
    assert_eq!(q(&db, &mut s, "show search_path"), ["\"$user\", public"]);
    assert_eq!(q(&db, &mut s, "show timezone"), ["Asia/Kolkata"]);
    q(&db, &mut s, "set app.x = 1; set statement_timeout = 5000");
    q(&db, &mut s, "reset all");
    assert_eq!(q(&db, &mut s, "show timezone"), ["UTC"], "RESET ALL resets the zone too");
    assert_eq!(q(&db, &mut s, "select '2026-09-29 00:00Z'::timestamptz::text"), ["2026-09-29 00:00:00+00"]);
    assert_eq!(q(&db, &mut s, "select current_setting('app.x', true), current_setting('statement_timeout')"), ["|0"]);
}

#[test]
fn values_in_set_operations_and_max_connections() {
    let (db, mut s) = setup();
    assert_eq!(q(&db, &mut s, "values (1) union all values (2) order by 1"), ["1", "2"]);
    assert_eq!(q(&db, &mut s, "select 1 union values (2), (1) order by 1"), ["1", "2"]);
    assert_eq!(q(&db, &mut s, "select id from steps except values (2)"), ["1"]);
    // what the server enforces: the setting, or less when the process's descriptors fit fewer
    let conns: usize = std::env::var("CHRONOS_MAX_CONNECTIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let shown: usize = q(&db, &mut s, "show max_connections")[0].parse().unwrap();
    assert!((1..=conns).contains(&shown), "max_connections {shown} of {conns}");
}
