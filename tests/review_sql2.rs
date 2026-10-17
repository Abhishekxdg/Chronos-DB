//! Leftovers from the SQL review: unknown type names, date field errors, UNION's literal
//! types, and COPY under an agent's max_query_ms. Each matches Postgres 17.

use chronos::sql::{Session, parse, run};
use chronos::{AgentSpec, Can, Db, MAIN};

fn code(db: &Db, sql: &str) -> &'static str {
    match run(db, MAIN, sql, &[]) {
        Ok(_) => "ok",
        Err(e) => e.code,
    }
}

/// The first column of the last statement's rows, as text, and its type's name.
fn one(db: &Db, sql: &str) -> (Vec<String>, String) {
    let out = run(db, MAIN, sql, &[]).unwrap_or_else(|e| panic!("{sql:.100}: {}", e.message));
    let last = out.last().unwrap();
    (last.rows.iter().map(|r| r[0].to_text()).collect(), last.columns.as_ref().unwrap()[0].1.name().into_owned())
}

#[test]
fn unknown_type_names_are_42704_and_real_ones_still_work() {
    let db = Db::new();
    run(&db, MAIN, "create type mood as enum ('ok', 'sad'); create table t (m mood)", &[]).unwrap();
    for i in 0..10_000 {
        assert_eq!(code(&db, &format!("select null::t{i}")), "42704");
    }
    assert_eq!(one(&db, "select 'sad'::mood").0, ["sad"]);
    assert_eq!(code(&db, "insert into t values ('meh')"), "22P02");
    assert_eq!(code(&db, "insert into t values ('ok')"), "ok");
    // made after its name was seen unresolved
    assert_eq!(code(&db, "select 'x'::later"), "42704");
    run(&db, MAIN, "create type later as enum ('x')", &[]).unwrap();
    assert_eq!(one(&db, "select 'x'::later").0, ["x"]);
    // a prepared statement names its type when it runs
    let mut s = Session::new(MAIN);
    s.run(&db, "prepare p as select 'ok'::mood", &[]).unwrap();
    assert_eq!(s.run(&db, "execute p", &[]).unwrap()[0].rows[0][0].to_text(), "ok");
}

#[test]
fn date_fields_out_of_range_are_22008_and_bad_formats_22007() {
    let db = Db::new();
    assert_eq!(code(&db, "select '2024-02-30'::date"), "22008");
    assert_eq!(code(&db, "select '2024-13-01'::date"), "22008");
    assert_eq!(code(&db, "select '2024-01-01 25:00'::timestamp"), "22008");
    assert_eq!(code(&db, "select 'tomorrow-ish'::date"), "22007");
    assert_eq!(code(&db, "select 'x'::timestamptz"), "22007");
    assert_eq!(code(&db, "select '2024-02-29'::date"), "ok");
    let e = run(&db, MAIN, "select '2023-02-29'::date", &[]).unwrap_err();
    assert_eq!(e.message, "date/time field value out of range: \"2023-02-29\"");
}

#[test]
fn set_operations_give_string_literals_the_other_sides_type() {
    let db = Db::new();
    assert_eq!(
        one(&db, "select '10' union select 9.5 order by 1"),
        (vec!["9.5".into(), "10".into()], "numeric".into())
    );
    assert_eq!(one(&db, "select 1 union all select '2' order by 1").1, "integer");
    assert_eq!(one(&db, "select null union select 3").1, "integer");
    assert_eq!(
        one(&db, "select '2024-01-01' intersect select date '2024-01-01'"),
        (vec!["2024-01-01".into()], "date".into())
    );
    assert_eq!(one(&db, "select '1.0' except select 1.00").0, Vec::<String>::new());
    assert_eq!(one(&db, "select 'a' union select 'b' order by 1"), (vec!["a".into(), "b".into()], "text".into()));
    assert_eq!(code(&db, "select 'x' union select 9.5"), "22P02");
    // three parts: a typed one anywhere decides
    assert_eq!(one(&db, "select '1' union select null union select 2.5 order by 1").1, "numeric");
}

#[test]
fn copy_from_stops_at_the_agents_max_query_ms() {
    let db = Db::new();
    run(&db, MAIN, "create table t (id int primary key, v text)", &[]).unwrap();
    let spec = AgentSpec { can: Some(vec![Can::Read, Can::WriteMain]), max_query_ms: Some(100), ..Default::default() };
    db.create_agent("loader", &spec).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("loader".into());
    let stmt = parse("copy t from stdin csv").unwrap().pop().unwrap();
    let copy = s.copy_in(&db, &stmt).unwrap();
    let mut first = true;
    let next = move || {
        if !std::mem::take(&mut first) {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        Ok(Some(b"1,a\n".to_vec()))
    };
    let e = copy.run(&mut s, &db, Box::new(next)).unwrap_err();
    assert_eq!(e.code, "57014", "{}", e.message);
}
