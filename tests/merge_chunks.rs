//! MERGE WORLD ... IN CHUNKS OF n: a world's rows merged n at a time, each chunk a merge of its own, until none are
//! left; for people and admins only (an agent's chunks would each pass its policy's max_rows).

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

#[test]
fn a_world_merges_in_chunks() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table t (id int primary key, v int); insert into t select g, 0 from generate_series(1, 25) g",
        &[],
    )
    .unwrap();
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(&db, "w", "update t set v = 1", &[]).unwrap();
    let out = run(&db, MAIN, "merge world w in chunks of 10", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(out[0].tag, "MERGE 25");
    let n = run(&db, MAIN, "select count(*) from t where v = 1", &[]).unwrap().pop().unwrap().rows[0][0].to_text();
    assert_eq!(n, "25");
    assert!(db.world("w").is_err(), "the last chunk merges the world itself");

    // what can't be chunked, said so
    run(&db, MAIN, "create world x; use world x; update t set v = 2 where id = 1; use world main", &[]).unwrap();
    for bad in ["merge world x in chunks of 0", "merge world x in chunks of ten", "merge world x in chunks 10"] {
        assert_eq!(run(&db, MAIN, bad, &[]).unwrap_err().code, "42601", "{bad}");
    }
    let e = run(&db, MAIN, "merge world x in chunks of 10 dry run", &[]).unwrap_err();
    assert!(e.message.contains("IN CHUNKS OF"), "{}", e.message);

    // an agent may not split its merges, even one that may merge into main
    run(&db, MAIN, "create agent bot with (can = 'read,fork,write_own,merge')", &[]).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(&db, "create world mine; use world mine; update t set v = 3 where id = 2; use world main", &[]).unwrap();
    let e = s.run(&db, "merge world mine in chunks of 1", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    s.run(&db, "merge world mine", &[]).unwrap();
}
