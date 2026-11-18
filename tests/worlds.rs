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

/// Gate S, batch S3 (CHRONOS-PLAN-2026-11-17-r2.md D4): with an agent acting, the core makes no
/// world without an owner. Every agent path goes through the agents' constructor, which stamps the
/// owner, tenant, scope and read flags; a core call that skips it (a path missed by the audit) is
/// refused rather than making an unstamped world. With no agent acting it works as before.
#[test]
fn core_refuses_ownerless_agent_world() {
    let db = Db::new();
    chronos::sql::run(
        &db,
        MAIN,
        "create table t (id int primary key, org_id int);
         create agent run42 with (can = 'read,fork,write_own', tenant = 'org_id = 42')",
        &[],
    )
    .unwrap();
    {
        let _acting = chronos::acting_as("run42");
        assert!(db.fork(MAIN, "a").is_err(), "fork");
        assert!(db.fork_with(MAIN, "b", serde_json::json!({})).is_err(), "fork_with");
        assert!(db.world("a").is_err() && db.world("b").is_err());
        // through the agent's own constructor it works, stamped
        let w = db.as_agent("run42").unwrap().fork(MAIN, "c", serde_json::Value::Null).unwrap();
        assert_eq!(w.owner.as_deref(), Some("run42"));
    }
    db.fork(MAIN, "d").unwrap();
    db.fork_with(MAIN, "e", serde_json::json!({})).unwrap();
}

/// Runs `q` on world `w` as the database's own user.
fn on(db: &Db, w: &str, q: &str) -> Result<Vec<chronos::sql::Output>, chronos::sql::SqlError> {
    chronos::sql::run(db, w, q, &[])
}

/// The SQLSTATE `q` on world `w` fails with.
fn refused(db: &Db, w: &str, q: &str) -> &'static str {
    match on(db, w, q) {
        Ok(_) => panic!("{q} on {w}: went through"),
        Err(e) => e.code,
    }
}

/// Expires world `w` now and lets the clock pass it.
fn expire_now(db: &Db, w: &str) {
    db.expire(w, Some(chronos_now())).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
}

fn crm(db: &Db) {
    on(db, MAIN, "create table t (id int primary key, v text); insert into t values (1, 'a'), (2, 'b')").unwrap();
}

fn count(db: &Db, w: &str) -> String {
    on(db, w, "select count(*) from t").unwrap()[0].rows[0][0].to_text()
}

/// A world whose time is up is read-only at once: no write (SQL or the API), merge, fork or MERGE INTO from it,
/// rather than everything until the sweeper's next round. It still reads until the sweeper discards it.
#[test]
fn expired_world_refuses_write_merge_fork_at_once() {
    let db = Db::new();
    crm(&db);
    on(&db, MAIN, "create world w; create world other").unwrap();
    on(&db, "w", "insert into t values (3, 'c')").unwrap();
    expire_now(&db, "w");

    assert_eq!(refused(&db, "w", "insert into t values (4, 'd')"), "55000");
    assert_eq!(refused(&db, "w", "update t set v = 'z'"), "55000");
    assert_eq!(refused(&db, "w", "begin; insert into t values (5, 'e'); commit"), "55000");
    match db.put("w", "kv/1", "{}") {
        Err(Error::Constraint { code, .. }) => assert_eq!(code, "55000"),
        r => panic!("an API write to an expired world: {r:?}"),
    }
    assert_eq!(refused(&db, MAIN, "create world child from w"), "55000");
    assert_eq!(refused(&db, MAIN, "merge world w"), "55000");
    assert_eq!(refused(&db, MAIN, "merge world w into other"), "55000");
    // nor does anything merge into it
    on(&db, MAIN, "create world feeder; alter world feeder set (note = 1)").unwrap();
    on(&db, "feeder", "insert into t values (6, 'f')").unwrap();
    assert_eq!(refused(&db, MAIN, "merge world feeder into w"), "55000");

    // reads still work, a read-only transaction too, until it's swept
    assert_eq!(count(&db, "w"), "3");
    on(&db, "w", "begin; select count(*) from t; commit").unwrap();
    assert_eq!(count(&db, MAIN), "2", "nothing reached main");
    assert_eq!(db.expire_worlds().unwrap(), 1);
    assert!(db.world("w").is_err());
}

/// A child never outlives its parent: its expiry is capped by the parent's when forked, when its TTL is set or reset,
/// and for an agent's world_ttl.
#[test]
fn child_ttl_capped_by_parent() {
    let db = Db::new();
    crm(&db);
    on(&db, MAIN, "create world p; alter world p set ttl '1 hour'; create world c from p").unwrap();
    let p = db.world("p").unwrap().expires.unwrap();
    assert_eq!(db.world("c").unwrap().expires, Some(p), "a child with no TTL gets its parent's");
    on(&db, MAIN, "alter world c set ttl '10 days'").unwrap();
    assert_eq!(db.world("c").unwrap().expires, Some(p), "a longer TTL is capped");
    on(&db, MAIN, "alter world c reset ttl").unwrap();
    assert_eq!(db.world("c").unwrap().expires, Some(p), "no TTL is capped too");
    on(&db, MAIN, "alter world c set ttl '1 minute'").unwrap();
    let c = db.world("c").unwrap().expires.unwrap();
    assert!(c < p, "a shorter one stands");
    on(&db, MAIN, "create world g from c").unwrap();
    assert_eq!(db.world("g").unwrap().expires, Some(c));

    // an agent's world_ttl too
    on(&db, MAIN, "create agent bot with (can = 'read,fork,write_own,write', world_ttl = '1 day')").unwrap();
    let mut s = chronos::sql::Session::new(MAIN);
    s.agent = Some("bot".into());
    s.run(&db, "create world mine from p", &[]).unwrap();
    assert_eq!(db.world("mine").unwrap().expires, Some(p));
}

/// When a world's time is up, the worlds forked from it go with it, the deepest first, even ones that were given no
/// TTL of their own before it had one.
#[test]
fn parent_expiry_expires_subtree_deepest_first() {
    let db = Db::new();
    crm(&db);
    on(&db, MAIN, "create world p; create world c from p; create world g from c; create world c2 from p").unwrap();
    on(&db, "g", "insert into t values (9, 'g')").unwrap();
    on(&db, MAIN, "create world bystander").unwrap();
    expire_now(&db, "p");
    for w in ["p", "c", "g", "c2"] {
        assert!(db.world(w).unwrap().expires.is_some(), "{w} expires with its parent");
    }
    assert_eq!(refused(&db, "g", "insert into t values (10, 'h')"), "55000", "a grandchild is expired at once too");
    assert_eq!(db.expire_worlds().unwrap(), 4);
    let names: Vec<String> = db.worlds().into_iter().map(|w| w.name).collect();
    assert_eq!(names, ["bystander", "main"]);
}

/// Shortening a world's TTL shortens its forks' that would outlive it; lengthening it leaves theirs.
#[test]
fn shortening_parent_ttl_shortens_children() {
    let db = Db::new();
    crm(&db);
    on(&db, MAIN, "create world p; alter world p set ttl '10 days'; create world c from p; create world g from c")
        .unwrap();
    on(&db, MAIN, "create world quick from p; alter world quick set ttl '1 minute'").unwrap();
    let quick = db.world("quick").unwrap().expires.unwrap();
    on(&db, MAIN, "alter world p set ttl '1 hour'").unwrap();
    let p = db.world("p").unwrap().expires.unwrap();
    assert_eq!(db.world("c").unwrap().expires, Some(p));
    assert_eq!(db.world("g").unwrap().expires, Some(p));
    assert_eq!(db.world("quick").unwrap().expires, Some(quick), "a shorter one stands");
    on(&db, MAIN, "alter world p set ttl '20 days'").unwrap();
    assert_eq!(db.world("c").unwrap().expires, Some(p), "lengthening the parent's lengthens no child's");
}

/// A transaction open in a world whose time runs out (its own, or its parent's) can't commit: what it wrote never
/// lands. A read-only one commits.
#[test]
fn open_transaction_on_expiring_child_commit_refused() {
    let db = Db::new();
    crm(&db);
    on(&db, MAIN, "create world p; create world c from p").unwrap();
    let mut s = chronos::sql::Session::new("c");
    s.run(&db, "begin; insert into t values (7, 'tx')", &[]).unwrap();
    let mut r = chronos::sql::Session::new("c");
    r.run(&db, "begin; select count(*) from t", &[]).unwrap();
    expire_now(&db, "p");
    assert_eq!(s.run(&db, "commit", &[]).unwrap_err().code, "55000");
    r.run(&db, "commit", &[]).unwrap();
    assert_eq!(count(&db, "c"), "2", "the transaction's row never landed");
    // and one begun after can't write
    let mut late = chronos::sql::Session::new("c");
    late.run(&db, "begin", &[]).unwrap();
    assert_eq!(late.run(&db, "insert into t values (8, 'late')", &[]).unwrap_err().code, "55000");
}

/// How deep worlds nest and how many forks one world has, for the database (ALTER SYSTEM) and per agent; over either,
/// a fork is refused with 54000. Transactions' own worlds count toward neither.
#[test]
fn max_depth_and_max_children_refuse() {
    let db = Db::new();
    crm(&db);
    on(&db, MAIN, "alter system set max_depth = 2").unwrap();
    assert_eq!(on(&db, MAIN, "show max_depth").unwrap()[0].rows[0][0].to_text(), "2");
    on(&db, MAIN, "create world a; create world b from a").unwrap();
    assert_eq!(refused(&db, MAIN, "create world c from b"), "54000");
    on(&db, "b", "begin; insert into t values (5, 'x'); commit").unwrap(); // a transaction is no deeper world

    on(&db, MAIN, "alter system set max_children = 2").unwrap();
    on(&db, MAIN, "create world k1 from a").unwrap(); // a has b and k1
    assert_eq!(refused(&db, MAIN, "create world k2 from a"), "54000");
    on(&db, MAIN, "create world top1; create world top2; create world top3").unwrap(); // main's are max_worlds'
    on(&db, MAIN, "alter system set max_depth = 0; alter system set max_children = 0").unwrap();
    on(&db, MAIN, "create world k2 from a; create world c from b").unwrap();

    // per agent
    on(&db, MAIN, "create agent deep with (can = 'read,fork,write_own', max_depth = 1)").unwrap();
    let mut s = chronos::sql::Session::new(MAIN);
    s.agent = Some("deep".into());
    s.run(&db, "create world d1", &[]).unwrap();
    assert_eq!(s.run(&db, "create world d2 from d1", &[]).unwrap_err().code, "54000");
    on(&db, MAIN, "create agent wide with (can = 'read,fork,write_own', max_children = 1)").unwrap();
    let mut s = chronos::sql::Session::new(MAIN);
    s.agent = Some("wide".into());
    s.run(&db, "create world n1; create world n2 from n1", &[]).unwrap();
    assert_eq!(s.run(&db, "create world n3 from n1", &[]).unwrap_err().code, "54000");
    assert_eq!(db.agent("wide").unwrap().max_children, 1);
    assert_eq!(db.agent("deep").unwrap().max_depth, 1);
}

/// A world's budget of rows written and statements run, counted as they happen; a fork's own budget is at most what
/// its parent has left, and what a fork writes counts toward its parent's budget too. Over, 53400.
#[test]
fn budget_rows_refuses_and_child_capped() {
    let db = Db::new();
    crm(&db);
    on(&db, MAIN, "create world b with (budget_rows = 10, budget_statements = 4)").unwrap();
    on(&db, "b", "insert into t select g, 'x' from generate_series(10, 13) g").unwrap(); // 4 rows, 1 statement
    assert_eq!(refused(&db, MAIN, "create world big from b with (budget_rows = 7)"), "53400");
    on(&db, MAIN, "create world c from b with (budget_rows = 3)").unwrap();
    on(&db, "c", "insert into t values (20, 'c'), (21, 'c'), (22, 'c')").unwrap(); // c: 3, b: 7
    assert_eq!(refused(&db, "c", "insert into t values (23, 'c')"), "53400", "c's own budget");
    assert_eq!(refused(&db, "b", "insert into t select g, 'x' from generate_series(30, 33) g"), "53400", "b's");
    on(&db, "b", "insert into t values (30, 'x'), (31, 'x'), (32, 'x')").unwrap(); // b: 10 rows, 3 statements
    assert_eq!(refused(&db, "b", "insert into t values (40, 'x')"), "53400");
    assert_eq!(count(&db, "b"), "9", "refused statements wrote nothing");
    let budget = &db.world("c").unwrap().meta["_sonos_budget"];
    assert_eq!(budget["rows"], 3);
    assert_eq!(refused(&db, MAIN, "alter world b set (_sonos_budget = 1)"), "22023");

    // statements
    on(&db, MAIN, "create world s with (budget_statements = 2)").unwrap();
    on(&db, "s", "insert into t values (50, 's'); update t set v = 'y' where id = 50").unwrap();
    assert_eq!(refused(&db, "s", "delete from t where id = 50"), "53400");
    on(&db, MAIN, "alter world s set (budget_statements = 5)").unwrap();
    on(&db, "s", "delete from t where id = 50").unwrap();
    // an agent can't raise its own
    on(&db, MAIN, "create agent bot with (can = 'read,fork,write_own')").unwrap();
    let mut a = chronos::sql::Session::new(MAIN);
    a.agent = Some("bot".into());
    a.run(&db, "create world mine with (budget_rows = 1)", &[]).unwrap();
    assert_eq!(a.run(&db, "alter world mine set (budget_rows = 100)", &[]).unwrap_err().code, "42501");
}
