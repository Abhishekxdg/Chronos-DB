//! World names MERGE would misread: `MERGE WORLD ours` reads `ours` as MERGE's own word, so a
//! world named like one of them couldn't be merged unquoted. Such names are refused when a world
//! is made, and a MERGE that names no world on main says why and how to quote one.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

#[test]
fn a_world_cant_be_named_like_one_of_merges_words() {
    let db = Db::new();
    for w in ["ours", "Theirs", "by", "into", "only", "dry", "using", "confirm", "resolve"] {
        let e = run(&db, MAIN, &format!("create world \"{w}\""), &[]).unwrap_err();
        assert!(e.message.contains("would read it as MERGE's own word"), "{w}: {}", e.message);
    }
    assert!(db.fork(MAIN, "ours").is_err(), "the API too");
    assert!(db.fork_with(MAIN, "theirs", serde_json::Value::Null).is_err());
    // names that only start like one are fine
    run(&db, MAIN, "create world ours2; create world bygone", &[]).unwrap();
    run(&db, MAIN, "merge world ours2; merge world bygone", &[]).unwrap();
}

#[test]
fn merging_no_world_on_main_says_why() {
    let db = Db::new();
    for sql in ["merge world ours", "merge world theirs dry run", "merge world"] {
        let e = run(&db, MAIN, sql, &[]).unwrap_err();
        assert_eq!(e.code, "0A000", "{sql}: {}", e.message);
        assert!(e.message.contains("quote it: MERGE WORLD \"ours\""), "{sql}: {}", e.message);
    }
    let mut s = Session::new(MAIN);
    let e = s.run(&db, "merge world ours", &[]).unwrap_err();
    assert!(e.message.contains("names no world here"), "{}", e.message);
    // in a world, naming none merges that world, as before
    s.run(&db, "create world w; use world w; merge world", &[]).unwrap();
    assert!(db.world("w").is_err(), "merged");
}
