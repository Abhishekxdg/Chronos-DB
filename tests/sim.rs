//! Deterministic simulation of concurrency (`--features shuttle`). Shuttle replaces the engine's
//! locks, atomics, channels and threads, then runs the scenario under many seeded schedules. It
//! controls exactly which thread runs at every lock and atomic, finds deadlocks, and prints the
//! schedule of any failure so `shuttle::replay` reproduces it exactly.
//!
//!   cargo test --release --features shuttle --test sim           (SIM_ITERS=300 by default)
#![cfg(feature = "shuttle")]

use chronos::{Db, Resolve};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static RUN: AtomicUsize = AtomicUsize::new(0);

fn tmp() -> PathBuf {
    let n = RUN.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("chronos-sim-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn fingerprint(db: &Db) -> String {
    let mut out = String::new();
    for (b, parent) in db.branches() {
        let rows = db.scan(&b, "", usize::MAX).unwrap();
        out += &format!("{b} <- {parent:?}\n  {rows:?}\n  {:?}\n", db.diff(&b).unwrap());
    }
    out
}

fn iters() -> usize {
    std::env::var("SIM_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(300)
}

/// Whatever the interleaving, replaying the log must rebuild exactly the live state.
fn scenario() {
    let dir = tmp();
    let live = {
        let db = Db::open(&dir).unwrap();
        db.put("main", "k0", "seed").unwrap();
        shuttle::thread::scope(|s| {
            // direct writes to main race the forks taken from it
            s.spawn(|| {
                for i in 0..4 {
                    db.put("main", "m0", &format!("direct {i}")).unwrap(); // the key agents delete
                }
            });
            for t in 0..2 {
                let db = &db;
                s.spawn(move || {
                    let b = format!("agent{t}");
                    db.fork("main", &b).unwrap();
                    db.put(&b, "k0", &b).unwrap(); // both edit k0: the second merge conflicts
                    db.batch(&b, &[("m0", None), ("shared", Some(b.as_str()))][..t + 1]).unwrap();
                    let how = [Resolve::Fail, Resolve::Ours][t];
                    if db.merge(&b, how).is_err() {
                        db.discard(&b).unwrap();
                    }
                });
            }
            s.spawn(|| {
                db.checkpoint().unwrap();
                db.history("main", 5).unwrap(); // logs the worlds' waiting writes, racing them
                db.gc().unwrap();
            });
        });
        fingerprint(&db)
    };
    let reopened = fingerprint(&Db::open(&dir).unwrap());
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(reopened, live, "replaying the log did not rebuild the live state");
}

#[test]
fn random_schedules() {
    shuttle::check_random(scenario, iters());
}

/// PCT picks a few priority-change points per run: good at bugs that need a handful of
/// specific context switches, which purely random schedules rarely hit.
#[test]
fn pct_schedules() {
    shuttle::check_pct(scenario, iters(), 3);
}

/// The tests above run side by side on two OS threads, so a `static` of shuttle's types (a
/// `RefCell` underneath) is touched by both at once and panics "RefCell already borrowed" now
/// and then, in whichever task happens to use it. Files that take their locks and atomics from
/// `crate::sync` must spell out std's for statics.
#[test]
fn statics_are_std() {
    let mut bad = vec![];
    let mut dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = dirs.pop() {
        for e in fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let src = fs::read_to_string(&path).unwrap_or_default();
            if !src.contains("crate::sync::") && !src.contains("use sync::") {
                continue;
            }
            for (i, line) in src.lines().enumerate() {
                let Some((_, ty)) = line.trim().split_once("static ").and_then(|(_, r)| r.split_once(':')) else {
                    continue;
                };
                let ty = ty.trim_start();
                if ["Atomic", "Mutex", "RwLock", "mpsc"].iter().any(|t| ty.starts_with(t)) {
                    bad.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                }
            }
        }
    }
    assert!(bad.is_empty(), "statics of shuttle's types (use std::sync:: for statics):\n{}", bad.join("\n"));
}

/// Replays a failing schedule exactly: SIM_REPLAY='<schedule printed by a failed run>'.
#[test]
fn replay_schedule() {
    let Ok(schedule) = std::env::var("SIM_REPLAY") else { return };
    shuttle::replay(scenario, schedule.trim().trim_matches('"'));
}
