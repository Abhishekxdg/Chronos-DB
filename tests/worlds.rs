//! Worlds on disk across checkpoints and restarts: a world with few changes keeps them as a list
//! in the manifest instead of pages of its own. Whatever the storage, every world must read,
//! diff and merge exactly the same after a checkpoint and after reopening.

use chronos::{Db, Error, MAIN, Resolve};
use std::collections::{BTreeMap, HashMap};

type Rows = BTreeMap<String, String>;
type Diff = Vec<(String, Option<String>, Option<String>)>;

fn dump(db: &Db, b: &str) -> Rows {
    let rows = db.scan(b, "t/", usize::MAX).unwrap();
    rows.into_iter().filter(|(k, _)| k.starts_with("t/")).map(|(k, v)| (k, v.to_string())).collect()
}

fn diffs(db: &Db, b: &str) -> Diff {
    db.diff(b)
        .unwrap()
        .into_iter()
        .map(|c| (c.key, c.before.map(|v| v.to_string()), c.after.map(|v| v.to_string())))
        .collect()
}

/// Every world's rows and diff.
fn state(db: &Db) -> BTreeMap<String, (Rows, Diff)> {
    db.worlds()
        .into_iter()
        .map(|w| {
            let d = if w.parent.is_some() { diffs(db, &w.name) } else { vec![] };
            (w.name.clone(), (dump(db, &w.name), d))
        })
        .collect()
}

#[test]
fn small_and_big_worlds_survive_checkpoints_and_restarts() {
    let dir = std::env::temp_dir().join(format!("chronos-worlds-delta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    for i in 0..500 {
        db.put(MAIN, &format!("t/{i:04}"), &format!("{{\"v\": {i}}}")).unwrap();
    }
    db.checkpoint().unwrap();

    // small: a few puts and a delete; big: more rows than a list holds
    db.fork(MAIN, "small").unwrap();
    db.put("small", "t/0001", r#"{"v": "small"}"#).unwrap();
    db.put("small", "t/new", r#"{"v": "new"}"#).unwrap();
    db.del("small", "t/0002").unwrap();
    db.fork(MAIN, "big").unwrap();
    for i in 0..100 {
        db.put("big", &format!("t/{i:04}"), r#"{"v": "big"}"#).unwrap();
    }
    // a world forked from a small world, then changed, and main moving on meanwhile
    db.fork("small", "child").unwrap();
    db.put("child", "t/0003", r#"{"v": "child"}"#).unwrap();
    db.put(MAIN, "t/0400", r#"{"v": "main moved"}"#).unwrap();
    db.fork(MAIN, "late").unwrap();
    db.del("late", "t/0400").unwrap();
    let want = state(&db);

    db.checkpoint().unwrap();
    assert_eq!(state(&db), want, "after a checkpoint");
    // more changes after the checkpoint, then another one: lists grow, a small world turns big
    db.put("small", "t/0005", r#"{"v": "later"}"#).unwrap();
    for i in 100..140 {
        db.put("late", &format!("t/{i:04}"), r#"{"v": "late"}"#).unwrap();
    }
    let want = state(&db);
    db.checkpoint().unwrap();
    assert_eq!(state(&db), want, "after a second checkpoint");
    drop(db);

    let db = Db::open(&dir).unwrap();
    assert_eq!(state(&db), want, "after reopening");
    // merges work from lists: child into small, small into main
    assert_eq!(db.merge("child", Resolve::Fail).unwrap(), 1);
    assert_eq!(db.merge("small", Resolve::Fail).unwrap(), 5);
    let main = dump(&db, MAIN);
    assert_eq!(main["t/0001"], r#"{"v": "small"}"#);
    assert_eq!(main["t/0003"], r#"{"v": "child"}"#);
    assert!(!main.contains_key("t/0002"));
    let want = state(&db);
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(state(&db), want, "after merging and reopening");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Many worlds with one change each cost a list entry, not pages: the folder stays small.
#[test]
fn many_small_worlds_take_little_disk() {
    let dir = std::env::temp_dir().join(format!("chronos-worlds-many-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    for i in 0..2000 {
        db.put(MAIN, &format!("t/{i:05}"), &format!("{{\"v\": {i}, \"pad\": \"{}\"}}", "x".repeat(100))).unwrap();
    }
    db.checkpoint().unwrap();
    let pages = |d: &std::path::Path| {
        std::fs::read_dir(d.join("pages")).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum::<u64>()
    };
    let before = pages(&dir);
    for i in 0..2000 {
        let w = format!("w{i}");
        db.fork(MAIN, &w).unwrap();
        db.put(&w, &format!("t/{i:05}"), r#"{"v": "mine"}"#).unwrap();
    }
    db.checkpoint().unwrap();
    let grew = pages(&dir) - before;
    assert!(grew < 64 << 10, "2000 one-row worlds added {grew} bytes of pages");
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.get("w1234", "t/01234").unwrap().as_deref(), Some(r#"{"v": "mine"}"#));
    assert_eq!(db.get("w1234", "t/01235").unwrap().as_deref().map(|v| v.contains("1235")), Some(true));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn storage_counts_what_each_world_holds_on_its_own() {
    let dir = std::env::temp_dir().join(format!("chronos-usage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let rows: Vec<(String, String)> =
        (0..20_000).map(|i| (format!("t/{i:05}"), format!("{{\"v\":\"{}{i}\"}}", "x".repeat(100)))).collect();
    let refs: Vec<(&str, Option<&str>)> = rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect();
    db.batch(MAIN, &refs).unwrap();
    db.checkpoint().unwrap();
    for w in ["idle", "few", "many"] {
        db.fork(MAIN, w).unwrap();
    }
    db.put("few", "t/00001", "{\"v\":\"changed\"}").unwrap();
    let many: Vec<(String, String)> = (0..2_000).map(|i| (format!("t/{i:05}"), "{\"v\":\"changed\"}".into())).collect();
    db.batch("many", &many.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
    db.checkpoint().unwrap();

    let main = db.usage(MAIN).unwrap();
    assert!(main.rows.is_none() && main.pages > 100 && main.bytes > 100_000, "{main:?}");
    let idle = db.usage("idle").unwrap();
    assert_eq!((idle.rows, idle.pages, idle.bytes), (Some(0), 0, 0), "a fresh fork costs nothing");
    let few = db.usage("few").unwrap();
    assert_eq!((few.rows, few.pages), (Some(1), 0), "one row stays a list, not pages: {few:?}");
    assert!(few.bytes > 0 && few.bytes < 100, "{few:?}");
    // 2,000 neighbouring rows of 20,000: its own pages for that tenth of the keys, and no more
    let many = db.usage("many").unwrap();
    assert_eq!(many.rows, Some(2_000));
    assert!(many.pages > 0 && many.pages < main.pages / 4 && many.bytes < main.bytes / 4, "{many:?} vs {main:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn any_world_can_expire() {
    let db = Db::new();
    let sql = |q: &str| chronos::sql::run(&db, MAIN, q, &[]);
    sql("create world scratch; create world keep; create world soon").unwrap();
    sql("alter world scratch set ttl '0 seconds'; alter world keep set ttl '0 seconds'; alter world soon set ttl '1 hour'")
        .unwrap();
    let soon = db.world("soon").unwrap().expires.unwrap();
    assert!(soon > chronos_now() + 3_500_000 && soon < chronos_now() + 3_700_000, "{soon}");
    sql("alter world keep reset ttl").unwrap();
    assert_eq!(db.world("keep").unwrap().expires, None);
    assert_eq!(db.expire_worlds().unwrap(), 1);
    let names: Vec<String> = db.worlds().into_iter().map(|w| w.name).collect();
    assert_eq!(names, ["keep", "main", "soon"]);
    assert_eq!(sql("alter world main set ttl '1 day'").unwrap_err().code, "22023");
    assert_eq!(sql("alter world keep set ttl 'soon'").unwrap_err().code, "42601");

    // an agent can't lift the world_ttl its worlds were given
    sql("create agent bot with (can = 'read,fork,write_own', world_ttl = '1 hour')").unwrap();
    let mut s = chronos::sql::Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(&db, "create world mine", &[]).unwrap();
    assert_eq!(s.run(&db, "alter world mine reset ttl", &[]).unwrap_err().code, "42501");
    assert!(db.world("mine").unwrap().expires.is_some());
}

fn chronos_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-worlds-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Each world counts its forks instead of discard scanning every world for them: the counts must
/// hold through discards (one and many at once), merges, CASCADE, checkpoints and restarts.
#[test]
fn fork_counts_hold_through_discards_merges_and_restarts() {
    let dir = tmp("counts");
    let db = Db::open(&dir).unwrap();
    for (from, name) in [(MAIN, "a"), ("a", "a1"), ("a1", "a11"), ("a", "a2"), (MAIN, "b"), ("b", "b1")] {
        db.fork(from, name).unwrap();
        db.put(name, "t/x", name).unwrap();
    }
    let a1 = db.world("a1").unwrap().id;
    // forks go first whatever the order; a world with a fork not in the list stays
    let out = db.discard_many(&["a1", "a11", "b", "main", "nope", "a11"]);
    assert!(out[0].is_ok() && out[1].is_ok(), "{out:?}");
    assert!(matches!(out[2], Err(Error::HasChildren(_))), "{out:?}"); // b1 isn't listed
    assert!(matches!(out[3], Err(Error::IsMain)));
    assert!(matches!(out[4], Err(Error::NoBranch(_))));
    assert!(matches!(out[5], Err(Error::NoBranch(_)))); // named twice: gone the second time
    assert!(matches!(db.world(&a1), Err(Error::NoBranch(_))), "a discarded world's ID is gone too");
    assert!(matches!(db.discard("a"), Err(Error::HasChildren(_)))); // a2 is left
    db.merge("b1", Resolve::Fail).unwrap(); // a merge takes a fork away too
    let a2 = db.world("a2").unwrap().id;
    db.checkpoint().unwrap();
    drop(db);

    let db = Db::open(&dir).unwrap(); // from the checkpoint
    let depth: HashMap<String, usize> = db.worlds().into_iter().map(|w| (w.name, w.depth)).collect();
    assert_eq!(depth, HashMap::from([("main".into(), 0), ("a".into(), 1), ("a2".into(), 2), ("b".into(), 1)]));
    assert_eq!(db.world(&a2).unwrap().name, "a2");
    assert!(matches!(db.discard("a"), Err(Error::HasChildren(_))));
    db.fork("a2", "a21").unwrap();
    assert_eq!(db.world("a21").unwrap().depth, 3);
    drop(db);

    let db = Db::open(&dir).unwrap(); // from the log
    assert!(matches!(db.discard("a2"), Err(Error::HasChildren(_))));
    assert_eq!(db.descendants("a"), ["a21", "a2"]);
    assert_eq!(db.discard_tree("a").unwrap(), 3);
    db.discard("b").unwrap(); // its fork was merged
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.branches(), [(MAIN.to_string(), None)]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Forks racing discards of their parents: a fork either lands, and its parent then can't go, or
/// finds the parent gone. No fork is orphaned or lost, and a restart replays exactly this.
#[test]
fn concurrent_forks_and_discards_lose_no_children() {
    let dir = tmp("race");
    let db = Db::open(&dir).unwrap();
    let parents: Vec<String> = (0..300).map(|i| format!("p{i}")).collect();
    for p in &parents {
        db.fork(MAIN, p).unwrap();
    }
    std::thread::scope(|s| {
        for t in 0..4 {
            let (db, parents) = (&db, &parents);
            s.spawn(move || {
                for p in parents {
                    match db.fork(p, &format!("{p}_{t}")) {
                        Ok(()) | Err(Error::NoBranch(_)) => {}
                        Err(e) => panic!("{e}"),
                    }
                }
            });
        }
        s.spawn(|| parents.chunks(7).for_each(|c| drop(db.discard_many(c))));
        s.spawn(|| parents.iter().rev().for_each(|p| drop(db.discard(p))));
    });
    let live: HashMap<String, Option<String>> = db.branches().into_iter().collect();
    for (n, p) in &live {
        if let Some(p) = p {
            assert!(live.contains_key(p), "{n} lost its parent {p}");
            if p != MAIN {
                assert!(matches!(db.discard(p), Err(Error::HasChildren(_))), "{p} has {n}");
            }
        }
    }
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.branches().into_iter().collect::<HashMap<_, _>>(), live, "a restart replays the same worlds");
    // with the counts right, everything goes at once, forks first
    let all: Vec<String> = live.into_keys().filter(|n| n != MAIN).collect();
    assert!(db.discard_many(&all).iter().all(|o| o.is_ok()));
    assert_eq!(db.branches().len(), 1);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
