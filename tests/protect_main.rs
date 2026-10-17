//! ALTER SYSTEM SET protect_main: main's rows change only by merging a world, for everyone, the
//! database's own user included. A mistaken DELETE on main is refused, not an incident; the ways
//! back (RESTORE, UNDO) and definitions still work directly.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn count(db: &Db, world: &str) -> String {
    run(db, world, "select count(*) from t", &[]).unwrap()[0].rows[0][0].to_text()
}

#[test]
fn a_protected_main_changes_only_by_merging() {
    let dir = std::env::temp_dir().join(format!("chronos-protect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "create table t (id int primary key, v text); insert into t values (1, 'a'), (2, 'b')", &[])
        .unwrap();
    assert_eq!(run(&db, MAIN, "show protect_main", &[]).unwrap()[0].rows[0][0].to_text(), "off");
    run(&db, MAIN, "alter system set protect_main = on", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "show protect_main", &[]).unwrap()[0].rows[0][0].to_text(), "on");

    // every way of changing main's rows directly is refused, and nothing changes
    for q in [
        "delete from t",
        "insert into t values (3, 'c')",
        "update t set v = 'x' where id = 1",
        "truncate t",
        "insert into t select g, 'g' from generate_series(10, 20000) g",
    ] {
        let e = run(&db, MAIN, q, &[]).unwrap_err();
        assert_eq!(e.code, "42501", "{q}: {}", e.message);
        assert!(e.message.contains("main is protected"), "{q}: {}", e.message);
    }
    let mut s = Session::new(MAIN);
    let e = s.run(&db, "begin; delete from t where id = 1; commit", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "a transaction's COMMIT: {}", e.message);
    let _ = s.run(&db, "rollback", &[]);
    assert!(db.put(MAIN, "t/9", r#"{"id": 9, "v": "kv"}"#).is_err(), "the row API too");
    assert_eq!(count(&db, MAIN), "2");

    // definitions, and the database's own rows, still change directly
    run(&db, MAIN, "create table u (id int primary key); create index t_v on t (v)", &[]).unwrap();
    run(&db, MAIN, "create merge policy careful with (max_deletes = 0); notify on merge email, 'hi'", &[]).unwrap();

    // a merge is the way in
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(&db, "w", "delete from t where id = 1", &[]).unwrap();
    run(&db, MAIN, "merge world w", &[]).unwrap();
    assert_eq!(count(&db, MAIN), "1");

    // and the ways back still work: undoing that merge puts the row back
    run(&db, MAIN, "undo merge of world w", &[]).unwrap();
    assert_eq!(count(&db, MAIN), "2");

    // it's kept across a restart; turned off, main is writable again
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(run(&db, MAIN, "delete from t", &[]).unwrap_err().code, "42501");
    run(&db, MAIN, "alter system set protect_main = off", &[]).unwrap();
    run(&db, MAIN, "delete from t where id = 2", &[]).unwrap();
    assert_eq!(count(&db, MAIN), "1");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_agent_cant_turn_it_off() {
    let db = Db::new();
    run(&db, MAIN, "alter system set protect_main = on", &[]).unwrap();
    run(&db, MAIN, "create agent bot with (can = 'read,fork,write_own,write_main,merge_own')", &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    assert_eq!(bot.run(&db, "alter system set protect_main = off", &[]).unwrap_err().code, "42501");
    assert!(run(&db, MAIN, "alter system set protect_main = maybe", &[]).is_err());
}
