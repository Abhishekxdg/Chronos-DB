use chronos::{Db, Error, Resolve};

fn v(db: &Db, b: &str, k: &str) -> Option<String> {
    db.get(b, k).unwrap().map(|s| s.to_string())
}

#[test]
fn fork_shares_data_and_isolates_writes() {
    let db = Db::new();
    db.put("main", "a", "1").unwrap();
    db.fork("main", "x").unwrap();
    assert_eq!(v(&db, "x", "a").as_deref(), Some("1"));

    db.put("x", "a", "2").unwrap();
    db.put("x", "b", "3").unwrap();
    assert_eq!(v(&db, "main", "a").as_deref(), Some("1"));
    assert_eq!(v(&db, "main", "b"), None);

    db.put("main", "c", "4").unwrap();
    assert_eq!(v(&db, "x", "c"), None, "fork is a snapshot of the fork point");
}

#[test]
fn diff_reports_puts_and_deletes_vs_fork_point() {
    let db = Db::new();
    db.put("main", "a", "1").unwrap();
    db.put("main", "b", "1").unwrap();
    db.fork("main", "x").unwrap();
    db.put("x", "a", "2").unwrap();
    db.del("x", "b").unwrap();
    db.put("x", "c", "3").unwrap();
    db.put("x", "d", "9").unwrap();
    db.del("x", "d").unwrap(); // added then removed: no net change

    let d: Vec<_> = db
        .diff("x")
        .unwrap()
        .into_iter()
        .map(|c| (c.key, c.before.map(|s| s.to_string()), c.after.map(|s| s.to_string())))
        .collect();
    assert_eq!(
        d,
        vec![
            ("a".into(), Some("1".into()), Some("2".into())),
            ("b".into(), Some("1".into()), None),
            ("c".into(), None, Some("3".into())),
        ]
    );
}

#[test]
fn clean_merge_applies_and_removes_branch() {
    let db = Db::new();
    db.put("main", "a", "1").unwrap();
    db.put("main", "b", "1").unwrap();
    db.fork("main", "x").unwrap();
    db.put("x", "a", "2").unwrap();
    db.del("x", "b").unwrap();
    db.put("main", "z", "parent-side edit").unwrap(); // unrelated parent change is fine

    assert_eq!(db.merge("x", Resolve::Fail).unwrap(), 2);
    assert_eq!(v(&db, "main", "a").as_deref(), Some("2"));
    assert_eq!(v(&db, "main", "b"), None);
    assert_eq!(v(&db, "main", "z").as_deref(), Some("parent-side edit"));
    assert!(matches!(db.get("x", "a"), Err(Error::NoBranch(_))));
}

#[test]
fn conflicting_merge_returns_conflicts_as_data_and_keeps_branch() {
    let db = Db::new();
    db.put("main", "a", "1").unwrap();
    db.fork("main", "x").unwrap();
    db.put("x", "a", "ours").unwrap();
    db.put("x", "b", "new").unwrap();
    db.put("main", "a", "theirs").unwrap();

    let Err(Error::Conflicts(c)) = db.merge("x", Resolve::Fail) else { panic!("expected conflicts") };
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].key, "a");
    assert_eq!(c[0].base.as_deref(), Some("1"));
    assert_eq!(c[0].ours.as_deref(), Some("ours"));
    assert_eq!(c[0].theirs.as_deref(), Some("theirs"));
    // nothing applied, branch still alive
    assert_eq!(v(&db, "main", "b"), None);
    assert_eq!(v(&db, "x", "a").as_deref(), Some("ours"));

    assert_eq!(db.merge("x", Resolve::Theirs).unwrap(), 1);
    assert_eq!(v(&db, "main", "a").as_deref(), Some("theirs"));
    assert_eq!(v(&db, "main", "b").as_deref(), Some("new"));
}

#[test]
fn resolve_ours_overwrites_parent() {
    let db = Db::new();
    db.put("main", "a", "1").unwrap();
    db.fork("main", "x").unwrap();
    db.put("x", "a", "ours").unwrap();
    db.put("main", "a", "theirs").unwrap();
    db.merge("x", Resolve::Ours).unwrap();
    assert_eq!(v(&db, "main", "a").as_deref(), Some("ours"));
}

#[test]
fn identical_change_on_both_sides_is_still_a_conflict() {
    // Both sides wrote the row since the fork. Equal values can be a coincidence: a transfer that
    // debits 100 -> 90 must not be dropped because another transfer also left 90 there.
    let db = Db::new();
    db.put("main", "a", "100").unwrap();
    db.fork("main", "x").unwrap();
    db.put("x", "a", "90").unwrap();
    db.put("main", "a", "90").unwrap();
    assert!(matches!(db.merge("x", Resolve::Fail), Err(Error::Conflicts(_))));
    assert_eq!(db.merge("x", Resolve::Theirs).unwrap(), 0); // trivially resolvable either way
    assert_eq!(v(&db, "main", "a").as_deref(), Some("90"));
}

#[test]
fn discard_drops_branch_without_touching_parent() {
    let db = Db::new();
    db.fork("main", "x").unwrap();
    db.put("x", "a", "1").unwrap();
    db.discard("x").unwrap();
    assert_eq!(v(&db, "main", "a"), None);
    assert!(matches!(db.put("x", "a", "1"), Err(Error::NoBranch(_))));
}

#[test]
fn nested_forks_merge_up_the_chain() {
    let db = Db::new();
    db.fork("main", "x").unwrap();
    db.fork("x", "y").unwrap();
    db.put("y", "a", "1").unwrap();
    assert!(matches!(db.merge("x", Resolve::Fail), Err(Error::HasChildren(_))));
    assert!(matches!(db.discard("x"), Err(Error::HasChildren(_))));
    db.merge("y", Resolve::Fail).unwrap();
    db.merge("x", Resolve::Fail).unwrap();
    assert_eq!(v(&db, "main", "a").as_deref(), Some("1"));
}

#[test]
fn guards_and_scan() {
    let db = Db::new();
    assert!(matches!(db.fork("main", "main"), Err(Error::Exists(_))));
    assert!(matches!(db.fork("nope", "x"), Err(Error::NoBranch(_))));
    assert!(matches!(db.merge("main", Resolve::Fail), Err(Error::IsMain)));
    assert!(matches!(db.discard("main"), Err(Error::IsMain)));
    for k in ["a1", "a2", "a3", "b1"] {
        db.put("main", k, k).unwrap();
    }
    let keys: Vec<_> = db.scan("main", "a2", 10).unwrap().into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["a2", "a3", "b1"]);
    assert_eq!(db.scan("main", "", 2).unwrap().len(), 2);
    // errors tell the agent how to fix the call
    assert!(db.get("nope", "a").unwrap_err().to_string().contains("fork"));
}

#[test]
fn many_agents_fork_write_merge_concurrently() {
    let db = Db::new();
    std::thread::scope(|s| {
        for i in 0..64 {
            let db = &db;
            s.spawn(move || {
                let b = format!("agent{i}");
                db.fork("main", &b).unwrap();
                for j in 0..100 {
                    db.put(&b, &format!("{i}:{j}"), "x").unwrap();
                }
                db.merge(&b, Resolve::Fail).unwrap();
            });
        }
    });
    assert_eq!(db.scan("main", "", usize::MAX).unwrap().len(), 64 * 100);
}

#[test]
fn batch_writes_and_deletes_together() {
    let db = Db::new();
    db.put("main", "gone", "x").unwrap();
    db.batch("main", &[("a", Some("1")), ("b", Some("2")), ("gone", None)]).unwrap();
    assert_eq!(v(&db, "main", "a").as_deref(), Some("1"));
    assert_eq!(v(&db, "main", "b").as_deref(), Some("2"));
    assert_eq!(v(&db, "main", "gone"), None);
    assert!(matches!(db.batch("nope", &[("a", None)]), Err(Error::NoBranch(_))));
}

#[test]
fn writes_return_versions_and_merge_can_require_one() {
    let db = Db::new();
    db.fork("main", "x").unwrap();
    assert_eq!(db.put("x", "a", "1").unwrap(), 1);
    assert_eq!(db.batch("x", &[("b", Some("2")), ("c", Some("3"))]).unwrap(), 2);
    assert_eq!(db.del("x", "a").unwrap(), 3);
    assert_eq!(db.version("x").unwrap(), 3);
    let stale = db.merge_at("x", Resolve::Fail, Some(2)).unwrap_err();
    assert!(matches!(stale, Error::Stale { expected: Some(2), actual: 3, .. }), "{stale}");
    assert_eq!(db.merge_at("x", Resolve::Fail, Some(3)).unwrap(), 2);
}
