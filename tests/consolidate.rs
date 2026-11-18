//! `CONSOLIDATE p`: a coordinator world's children merged into it as one step, in the order they were forked. Each
//! child's merge goes into a scratch fork of p first; only when every one of them went through does the scratch merge
//! into p, and the children merged are discarded. Any one refused (a conflict, an escrow bound) and nothing merges.
//! `CONSOLIDATE PREVIEW p` says per row what would happen, changing nothing. `ONLY CHILDREN (...)` picks which, and
//! `USING ARBITRATION` settles rows two children both changed by the merge policy's `arbitrate` rule (by default the
//! owners' priority), recording the losing value in p's metadata (`_sonos_arbitration`).

use chronos::sql::{Output, Session, Value};
use chronos::{AgentSpec, Can, Db, MAIN, Priority};

fn q(db: &Db, world: &str, sql: &str) -> Output {
    chronos::sql::run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap()
}

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn fails(db: &Db, world: &str, sql: &str) -> chronos::sql::SqlError {
    match chronos::sql::run(db, world, sql, &[]) {
        Ok(_) => panic!("{sql}: went through"),
        Err(e) => e,
    }
}

fn worlds(db: &Db) -> Vec<String> {
    db.worlds().into_iter().map(|w| w.name).collect()
}

/// main's table, coordinator p, and its children c1, c2, c3 (forked in that order).
fn setup(db: &Db) {
    q(db, MAIN, "create table inv (sku int primary key, qty int, note text)");
    q(db, MAIN, "insert into inv values (1, 100, 'a'), (2, 50, 'b'), (3, 10, 'c')");
    q(db, MAIN, "alter table inv alter column qty set merge as delta escrow (min 0)");
    db.fork(MAIN, "p").unwrap();
    for c in ["c1", "c2", "c3"] {
        db.fork("p", c).unwrap();
    }
}

#[test]
fn consolidate_preview_changes_nothing() {
    let db = Db::new();
    setup(&db);
    q(&db, "c1", "update inv set qty = qty - 3 where sku = 1");
    q(&db, "c1", "update inv set note = 'c1' where sku = 2");
    q(&db, "c2", "update inv set qty = qty - 4 where sku = 1");
    q(&db, "c2", "insert into inv values (5, 1, 'new')");
    q(&db, "c3", "update inv set note = 'c3' where sku = 2");
    q(&db, "c3", "update inv set qty = qty - 9 where sku = 3"); // after c1, c2: 10 - 9 stays above 0
    let versions: Vec<u64> = ["p", "c1", "c2", "c3"].iter().map(|w| db.version(w).unwrap()).collect();
    let before = worlds(&db);
    let out = q(&db, MAIN, "consolidate preview p");
    let names: Vec<&str> = out.columns.as_ref().unwrap().iter().map(|c| c.0.as_str()).collect();
    assert_eq!(names, ["world", "table", "key", "outcome", "detail"]);
    let got: Vec<String> = rows(&out).iter().map(|r| r.rsplit_once('|').unwrap().0.to_string()).collect();
    assert_eq!(
        got,
        [
            "c1|inv|1|applied",
            "c1|inv|2|applied",
            "c2|inv|1|combined-delta",
            "c2|inv|5|applied",
            "c3|inv|2|conflict",
            "c3|inv|3|applied",
        ]
    );
    // nothing changed: p's rows, the worlds, their versions
    assert_eq!(rows(&q(&db, "p", "select sku, qty, note from inv order by sku")), ["1|100|a", "2|50|b", "3|10|c"]);
    assert_eq!(worlds(&db), before);
    let after: Vec<u64> = ["p", "c1", "c2", "c3"].iter().map(|w| db.version(w).unwrap()).collect();
    assert_eq!(after, versions);

    // an escrow bound the children break together is named as such
    q(&db, "c1", "update inv set qty = qty - 60 where sku = 1");
    q(&db, "c2", "update inv set qty = qty - 60 where sku = 1");
    let out = rows(&q(&db, MAIN, "consolidate preview p"));
    assert!(out.iter().any(|r| r.starts_with("c2|inv|1|escrow-violation|")), "{out:?}");
}

#[test]
fn consolidate_all_or_nothing() {
    let db = Db::new();
    setup(&db);
    q(&db, "c1", "update inv set qty = qty - 3, note = 'c1' where sku = 1");
    q(&db, "c2", "update inv set qty = qty - 4 where sku = 1");
    q(&db, "c3", "update inv set note = 'c3' where sku = 1"); // c1 changed the note too
    let e = fails(&db, MAIN, "consolidate p");
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("c3"), "{}", e.message);
    assert_eq!(rows(&q(&db, "p", "select qty, note from inv where sku = 1")), ["100|a"]);
    assert_eq!(worlds(&db), ["c1", "c2", "c3", "main", "p"], "no scratch world left, no child gone");

    // an escrow bound: likewise nothing
    db.discard("c3").unwrap();
    q(&db, "c2", "update inv set qty = qty - 200 where sku = 1");
    assert_eq!(fails(&db, MAIN, "consolidate p").code, "23514");
    assert_eq!(rows(&q(&db, "p", "select qty, note from inv where sku = 1")), ["100|a"]);
    assert_eq!(worlds(&db), ["c1", "c2", "main", "p"]);

    // set right, both merge as one, and go
    q(&db, "c2", "update inv set qty = qty + 200 where sku = 1");
    let out = q(&db, MAIN, "consolidate p");
    assert_eq!(out.tag, "CONSOLIDATE 2");
    assert_eq!(rows(&q(&db, "p", "select qty, note from inv where sku = 1")), ["93|c1"]);
    assert_eq!(worlds(&db), ["main", "p"]);
}

#[test]
fn consolidate_only_children_keeps_others() {
    let db = Db::new();
    setup(&db);
    q(&db, "c1", "update inv set qty = qty - 1 where sku = 1");
    q(&db, "c2", "update inv set qty = qty - 2 where sku = 1");
    q(&db, "c3", "update inv set qty = qty - 4 where sku = 1");
    q(&db, MAIN, "consolidate p only children (c1, c3)");
    assert_eq!(rows(&q(&db, "p", "select qty from inv where sku = 1")), ["95"]);
    assert_eq!(worlds(&db), ["c2", "main", "p"]);
    // a world that isn't p's child is refused
    db.fork(MAIN, "other").unwrap();
    assert_eq!(fails(&db, MAIN, "consolidate p only children (c2, other)").code, "22023");
    assert_eq!(worlds(&db), ["c2", "main", "other", "p"]);
    // c2 merges later, as a delta on top
    q(&db, "p", "merge world c2");
    assert_eq!(rows(&q(&db, "p", "select qty from inv where sku = 1")), ["93"]);
}

fn agent(db: &Db, name: &str, priority: Priority) -> Session {
    let can = vec![Can::Read, Can::Fork, Can::WriteOwn, Can::Write, Can::MergeOwn];
    let spec = AgentSpec { can: Some(can), priority: Some(priority), ..Default::default() };
    db.create_agent(name, &spec).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some(name.into());
    s
}

#[test]
fn arbitration_priority_records_loser() {
    let db = Db::new();
    q(&db, MAIN, "create table inv (sku int primary key, qty int, note text)");
    q(&db, MAIN, "insert into inv values (1, 100, 'a'), (2, 50, 'b')");
    db.fork(MAIN, "p").unwrap();
    let mut lo = agent(&db, "lo", Priority::Low);
    let mut hi = agent(&db, "hi", Priority::Normal);
    // the low-priority agent's world comes first, so the default (oldest wins) would keep its row
    lo.run(&db, "create world wl from p; use world wl; update inv set note = 'lo' where sku = 1", &[]).unwrap();
    hi.run(&db, "create world wh from p; use world wh; update inv set note = 'hi' where sku = 1", &[]).unwrap();
    hi.run(&db, "update inv set note = 'hi2' where sku = 2", &[]).unwrap();
    // without arbitration: refused, nothing merged
    assert_eq!(fails(&db, MAIN, "consolidate p").code, "40001");
    assert_eq!(rows(&q(&db, "p", "select note from inv order by sku")), ["a", "b"]);
    q(&db, MAIN, "consolidate p using arbitration");
    assert_eq!(rows(&q(&db, "p", "select note from inv order by sku")), ["hi", "hi2"]);
    assert_eq!(worlds(&db), ["main", "p"]);
    let meta = db.world("p").unwrap().meta;
    let list = meta["_sonos_arbitration"].as_array().unwrap_or_else(|| panic!("{meta}"));
    assert_eq!(list.len(), 1, "{meta}");
    let e = &list[0];
    assert_eq!(e["key"], "inv/1", "{meta}");
    assert_eq!(e["winner_world"], "wh");
    assert_eq!(e["loser_world"], "wl");
    assert_eq!(e["rule"], "priority");
    assert!(e["loser_value_hash"].as_str().is_some_and(|h| h.len() == 64), "{meta}");

    // the merge policy setting: its values, and the full list of settings when it's misspelt
    q(&db, MAIN, "create merge policy arb with (arbitrate = 'newest')");
    let e = fails(&db, MAIN, "create merge policy arb2 with (arbitrate = 'loudest')");
    assert!(e.message.contains("priority, newest, oldest or fail"), "{}", e.message);
    let e = fails(&db, MAIN, "create merge policy arb3 with (arbitrat = 'newest')");
    assert!(e.message.contains("arbitrate"), "{}", e.message);
}

/// Between the last merge (the scratch world into p) and discarding the children, their rows are already in p: a write
/// to a child then, a fork of one, or a transaction committed into one would be lost with it, or leave it behind. The
/// children are held for that window: each is refused (55000), and the consolidation ends with them gone and only
/// their merged rows in p.
#[test]
fn children_held_until_discarded() {
    use std::cell::Cell;
    use std::rc::Rc;
    let db = Db::new();
    setup(&db);
    q(&db, "c1", "update inv set qty = qty - 3 where sku = 1");
    q(&db, "c2", "update inv set note = 'c2' where sku = 2");
    let tried = Rc::new(Cell::new(0));
    let seen = tried.clone();
    chronos::test_consolidate_pause(Some(Box::new(move |db: &Db, kids: &[String]| {
        assert_eq!(kids, ["c1", "c2", "c3"]);
        for c in kids {
            let e = chronos::sql::run(db, c, "update inv set note = 'late' where sku = 3", &[]).unwrap_err();
            assert_eq!(e.code, "55000", "{}", e.message);
            assert!(e.message.contains("consolidated"), "{}", e.message);
            assert!(db.put(c, "inv/late", "x").is_err(), "the API's writes too");
            assert!(db.fork(c, &format!("{c}_late")).is_err(), "no fork of it");
            let mut s = Session::new(c);
            let r = s.run(db, "begin; update inv set note = 'tx' where sku = 3; commit", &[]);
            assert!(r.is_err(), "no transaction committed into it");
            seen.set(seen.get() + 1);
        }
    })));
    let out = chronos::sql::run(&db, MAIN, "consolidate p", &[]);
    chronos::test_consolidate_pause(None);
    assert_eq!(out.unwrap().pop().unwrap().tag, "CONSOLIDATE 3");
    assert_eq!(tried.get(), 3, "the pause ran for every child");
    assert_eq!(worlds(&db), ["main", "p"], "every child gone, none left behind by a fork");
    assert_eq!(rows(&q(&db, "p", "select sku, qty, note from inv order by sku")), ["1|97|a", "2|50|c2", "3|10|c"]);
    // and the next consolidation isn't held by the last one's children
    db.fork("p", "c4").unwrap();
    q(&db, "c4", "update inv set note = 'c4' where sku = 3");
    assert_eq!(q(&db, MAIN, "consolidate p").tag, "CONSOLIDATE 1");
    assert_eq!(rows(&q(&db, "p", "select note from inv where sku = 3")), ["c4"]);
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

/// The lifecycle holds through a consolidation: an expired child refuses it (nothing merged, every child kept, no
/// scratch world left), the scratch world is forked as any fork of p (so not from p once p's time is up).
#[test]
fn consolidation_keeps_expiry() {
    let db = Db::new();
    setup(&db);
    q(&db, "c1", "update inv set qty = qty - 3 where sku = 1");
    q(&db, "c2", "update inv set qty = qty - 4 where sku = 1");
    db.expire("c2", Some(now_ms())).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let e = fails(&db, MAIN, "consolidate p");
    assert_eq!(e.code, "55000", "{}", e.message);
    assert!(e.message.contains("c2"), "{}", e.message);
    assert_eq!(rows(&q(&db, "p", "select qty from inv where sku = 1")), ["100"]);
    assert_eq!(worlds(&db), ["c1", "c2", "c3", "main", "p"]);
    db.discard("c2").unwrap();
    // p with a TTL: consolidates as before
    q(&db, MAIN, "alter world p set ttl '1 hour'");
    assert_eq!(q(&db, MAIN, "consolidate p").tag, "CONSOLIDATE 2");
    assert_eq!(rows(&q(&db, "p", "select qty from inv where sku = 1")), ["97"]);
    // p's time up: no scratch world forks from it, nothing merges into it
    db.fork("p", "c5").unwrap();
    q(&db, "c5", "update inv set note = 'c5' where sku = 2");
    db.expire("p", Some(now_ms())).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    assert_eq!(fails(&db, MAIN, "consolidate p").code, "55000");
    assert_eq!(worlds(&db), ["c5", "main", "p"], "no scratch world left");
}

/// A transaction committed into a child after the consolidation copied it into its scratch world, but before the
/// last merge, would be discarded with the child, its rows never in p. The children are held from before they're
/// copied: the commit is refused (55000), so nothing it was told committed is lost.
#[test]
fn commit_into_copied_child_is_refused() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let db = Db::new();
    setup(&db);
    q(&db, "c1", "update inv set qty = qty - 3 where sku = 1");
    q(&db, "c2", "update inv set note = 'c2' where sku = 2");
    let committed: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(vec![]));
    let seen = committed.clone();
    chronos::test_consolidate_copied(Some(Box::new(move |db: &Db, c: &str| {
        if c != "c1" {
            return;
        }
        let mut s = Session::new("c1");
        let r = s.run(db, "begin; update inv set note = 'tx' where sku = 3; commit", &[]);
        if let Err(e) = &r {
            assert_eq!(e.code, "55000", "{}", e.message);
        }
        seen.borrow_mut().push(r.is_ok());
        s.close(db);
    })));
    let out = chronos::sql::run(&db, MAIN, "consolidate p", &[]);
    chronos::test_consolidate_copied(None);
    assert_eq!(committed.borrow().len(), 1, "the hook ran after c1 was copied");
    let acknowledged = committed.borrow()[0];
    let note = rows(&q(&db, "p", "select note from inv where sku = 3"))[0].clone();
    let kept = db.world("c1").is_ok();
    assert!(
        !acknowledged || note == "tx" || kept,
        "a commit acknowledged into c1 was lost: consolidation {out:?}, p has {note}, c1 kept: {kept}"
    );
    assert!(!acknowledged, "the commit is refused while c1 is held");
    assert_eq!(out.unwrap().pop().unwrap().tag, "CONSOLIDATE 3");
    assert_eq!(rows(&q(&db, "p", "select sku, qty, note from inv order by sku")), ["1|97|a", "2|50|c2", "3|10|c"]);
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dst = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dst);
        } else {
            std::fs::copy(&p, dst).unwrap();
        }
    }
}

/// A crash after the last merge (the scratch world into p, on disk: p is durable) and before the children are
/// discarded leaves the children with their rows already in p. They're marked so before that merge, and the next
/// open discards them: consolidating again merges nothing twice, and no merge of one goes through.
#[test]
fn crash_after_last_merge_merges_nothing_twice() {
    let base = std::env::temp_dir().join(format!("chronos-consolidate-crash-{}", std::process::id()));
    let (dir, crashed) = (base.join("db"), base.join("crashed"));
    let _ = std::fs::remove_dir_all(&base);
    let db = Db::open(&dir).unwrap();
    setup(&db);
    q(&db, MAIN, "alter world p set (durable = true)");
    q(&db, "c1", "update inv set qty = qty - 3 where sku = 1");
    q(&db, "c2", "update inv set qty = qty - 4 where sku = 1");
    q(&db, "c3", "update inv set note = 'c3' where sku = 3");
    db.checkpoint().unwrap();
    let to = crashed.clone();
    chronos::test_consolidate_pause(Some(Box::new(move |_db: &Db, _kids: &[String]| {
        copy_dir(&dir, &to); // kill -9 now
    })));
    let out = chronos::sql::run(&db, MAIN, "consolidate p", &[]);
    chronos::test_consolidate_pause(None);
    assert_eq!(out.unwrap().pop().unwrap().tag, "CONSOLIDATE 3");
    assert_eq!(rows(&q(&db, "p", "select qty from inv where sku = 1")), ["93"]);
    drop(db);

    let db = Db::open(&crashed).unwrap();
    assert_eq!(rows(&q(&db, "p", "select sku, qty, note from inv order by sku")), ["1|93|a", "2|50|b", "3|10|c3"]);
    assert_eq!(worlds(&db), ["main", "p"], "the children merged are discarded at open");
    let n = chronos::sql::run(&db, MAIN, "consolidate p", &[]).unwrap().pop().unwrap().tag;
    assert_eq!(n, "CONSOLIDATE 0");
    assert_eq!(rows(&q(&db, "p", "select qty from inv where sku = 1")), ["93"], "nothing merged twice");
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// An agent consolidating its own coordinator: the scratch world is the database's own, made while the agent acts.
/// The security release refuses worlds made for an agent outside its own fork, but not this one (PR #362 review).
#[test]
fn an_agent_consolidates_its_own_coordinator() {
    let db = Db::new();
    q(&db, MAIN, "create table inv (sku int primary key, qty int, note text)");
    q(&db, MAIN, "insert into inv values (1, 100, 'a')");
    q(&db, MAIN, "alter table inv alter column qty set merge as delta escrow (min 0)");
    db.create_agent(
        "coord",
        &AgentSpec {
            can: Some(vec![Can::ReadOwn, Can::ReadMain, Can::Fork, Can::WriteOwn, Can::MergeOwn]),
            ..Default::default()
        },
    )
    .unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("coord".into());
    let run = |s: &mut Session, sql: &str| {
        s.run(&db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {} {}", e.code, e.message));
    };
    run(&mut s, "create world p; create world c1 from p; create world c2 from p");
    run(&mut s, "use world c1; update inv set qty = qty - 3 where sku = 1");
    run(&mut s, "use world c2; update inv set qty = qty - 4 where sku = 1");
    run(&mut s, "use world main; consolidate p");
    assert_eq!(rows(&q(&db, "p", "select qty from inv where sku = 1")), ["93"]);
    let left = worlds(&db);
    assert!(!left.iter().any(|w| w == "c1" || w == "c2" || w.starts_with("_")), "{left:?}");
}
