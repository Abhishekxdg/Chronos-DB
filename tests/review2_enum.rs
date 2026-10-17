//! Second review, enum types: each label set a type goes through (ADD VALUE makes one each time)
//! is kept only while something holds it, so memory stays bounded by what's in use; prepared
//! statements, per-world label sets and dropped types still behave.

use chronos::sql::{Session, enum_label_sets, run};
use chronos::{Db, MAIN};

fn first(s: &mut Session, db: &Db, sql: &str) -> String {
    let out = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    out.last().unwrap().rows[0][0].to_text()
}

fn code(s: &mut Session, db: &Db, sql: &str) -> &'static str {
    match s.run(db, sql, &[]) {
        Ok(_) => "ok",
        Err(e) => e.code,
    }
}

#[test]
fn adding_values_one_at_a_time_keeps_few_label_sets() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, "create type grow as enum ('v0')", &[]).unwrap();
    for n in 1..=3000 {
        s.run(&db, &format!("alter type grow add value 'v{n}'"), &[]).unwrap();
        assert_eq!(first(&mut s, &db, &format!("select 'v{n}'::grow")), format!("v{n}"));
    }
    let kept = enum_label_sets("grow");
    assert!(kept <= 70, "{kept} label sets of grow kept: each ADD VALUE's stays");
    assert_eq!(first(&mut s, &db, "select array_length(enum_range(null::grow), 1)"), "3001");
}

#[test]
fn a_prepared_statement_keeps_its_enum_through_many_alters() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, "create type mood as enum ('sad', 'ok'); create table feel (id int primary key, m mood)", &[]).unwrap();
    s.run(&db, "insert into feel values (1, 'ok'), (2, 'sad')", &[]).unwrap();
    s.run(&db, "prepare p as select id from feel where m = $1", &[]).unwrap();
    s.run(&db, "prepare q as select 'sad'::mood < 'ok'::mood", &[]).unwrap();
    assert_eq!(first(&mut s, &db, "execute p('ok')"), "1");
    for n in 0..200 {
        s.run(&db, &format!("alter type mood add value 'm{n}'"), &[]).unwrap();
    }
    assert_eq!(first(&mut s, &db, "execute p('ok')"), "1");
    assert_eq!(first(&mut s, &db, "execute p('sad')"), "2");
    assert_eq!(first(&mut s, &db, "execute q"), "true");
    assert_eq!(first(&mut s, &db, "select min(m)::text from feel"), "sad");
}

#[test]
fn worlds_with_different_labels_each_resolve_their_own() {
    let db = Db::new();
    run(&db, MAIN, "create type hue as enum ('red', 'blue')", &[]).unwrap();
    let mut w = Session::new(MAIN);
    w.run(&db, "fork world main as w; use world w; alter type hue add value 'green'", &[]).unwrap();
    let mut m = Session::new(MAIN);
    m.run(&db, "alter type hue add value 'pink' before 'red'", &[]).unwrap();
    for _ in 0..3 {
        assert_eq!(first(&mut w, &db, "select enum_range(null::hue)"), "{red,blue,green}");
        assert_eq!(first(&mut m, &db, "select enum_range(null::hue)"), "{pink,red,blue}");
        assert_eq!(first(&mut w, &db, "select 'green'::hue::text"), "green");
        assert_eq!(code(&mut m, &db, "select 'green'::hue"), "22P02");
        assert_eq!(code(&mut w, &db, "select 'pink'::hue"), "22P02");
        // make others' label sets in between, so neither world's is only kept for being recent
        let mut c = Session::new(MAIN);
        c.run(&db, "create type hue_churn as enum ('a')", &[]).ok();
        for n in 0..80 {
            c.run(&db, &format!("alter type hue_churn add value 'c{n}'"), &[]).unwrap();
        }
        c.run(&db, "drop type hue_churn", &[]).unwrap();
    }
}

#[test]
fn a_dropped_types_label_sets_go_away() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, "create type gone as enum ('a', 'b'); create table g (x gone)", &[]).unwrap();
    s.run(&db, "insert into g values ('b'); alter type gone add value 'c'", &[]).unwrap();
    assert_eq!(first(&mut s, &db, "select x::text from g"), "b");
    assert!(enum_label_sets("gone") >= 1);
    s.run(&db, "drop table g; drop type gone", &[]).unwrap();
    s.run(&db, "create type gone_churn as enum ('a')", &[]).unwrap();
    for n in 0..80 {
        s.run(&db, &format!("alter type gone_churn add value 'c{n}'"), &[]).unwrap();
    }
    assert_eq!(enum_label_sets("gone"), 0);
    assert_eq!(code(&mut s, &db, "select 'a'::gone"), "42704");
}
