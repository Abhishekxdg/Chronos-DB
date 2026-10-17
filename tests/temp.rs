//! Temporary tables are the session's own: in memory, never logged, merged or seen by another
//! session; and every table is in the one schema, public.

use chronos::Db;
use chronos::sql::Session;

fn count(db: &Db, s: &mut Session, q: &str) -> Result<String, &'static str> {
    s.run(db, q, &[]).map(|mut o| o.pop().unwrap().rows[0][0].to_text()).map_err(|e| e.code)
}

#[test]
fn temporary_tables_are_the_sessions_own() {
    let dir = std::env::temp_dir().join(format!("chronos-temp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let (a, b) = (&mut Session::new(chronos::MAIN), &mut Session::new(chronos::MAIN));
        a.run(&db, "create table keep (id int primary key)", &[]).unwrap();
        a.run(&db, "create type mood as enum ('sad', 'ok'); create temp table feel (m mood)", &[]).unwrap();
        assert_eq!(a.run(&db, "insert into feel values ('meh')", &[]).unwrap_err().code, "22P02");
        a.run(&db, "insert into feel values ('ok'), ('sad')", &[]).unwrap();
        assert_eq!(count(&db, a, "select m from feel order by m limit 1"), Ok("sad".into()), "a world's enum");
        a.run(&db, "create temp table scratch (id int primary key, v text); insert into scratch values (1, 'x')", &[])
            .unwrap();
        assert_eq!(count(&db, a, "select count(*) from scratch"), Ok("1".into()));
        assert_eq!(count(&db, b, "select count(*) from scratch"), Err("42P01"), "another session doesn't see it");
        // a temp table in a world: not in its diff, and not merged
        a.run(&db, "create world w; use world w; create temp table t2 (id int); insert into t2 values (1); insert into keep values (7)", &[]).unwrap();
        let diff = a.run(&db, "diff", &[]).unwrap().pop().unwrap();
        assert!(diff.rows.iter().all(|r| r[0].to_text() == "keep"), "{diff:?}");
        a.run(&db, "merge world w", &[]).unwrap();
        assert_eq!(count(&db, b, "select count(*) from keep"), Ok("1".into()));
        assert_eq!(count(&db, b, "select count(*) from t2"), Err("42P01"));
        assert!(
            db.scan(chronos::MAIN, "", 1000)
                .unwrap()
                .iter()
                .all(|(k, _)| !k.starts_with("scratch") && !k.starts_with("t2"))
        );
        a.close(&db);
        db.close().unwrap();
    }
    // nothing of them was logged: reopened, the database has only its own table
    let db = Db::open(&dir).unwrap();
    let s = &mut Session::new(chronos::MAIN);
    assert_eq!(count(&db, s, "select count(*) from scratch"), Err("42P01"));
    assert_eq!(count(&db, s, "select count(*) from keep"), Ok("1".into()));
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// other schemas: tests/schemas.rs
#[test]
fn public_schema_names() {
    let db = Db::new();
    let s = &mut Session::new(chronos::MAIN);
    assert!(s.run(&db, "create schema if not exists public", &[]).is_ok());
    assert_eq!(s.run(&db, "create table app.t (id int)", &[]).unwrap_err().code, "3F000");
    assert_eq!(s.run(&db, "select * from app.t", &[]).unwrap_err().code, "42P01");
    assert_eq!(s.run(&db, "drop schema app", &[]).unwrap_err().code, "3F000");
    assert!(s.run(&db, "drop schema if exists app", &[]).is_ok());
    s.run(&db, "create table public.t (id int primary key); insert into public.t values (1)", &[]).unwrap();
    assert_eq!(count(&db, s, "select count(*) from public.t"), Ok("1".into()));
}
