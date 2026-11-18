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

/// Shuttle's tasks run on stacks of their own, 32 KiB unless told: too small for a merge's frames (a merge policy's
/// judging), which overflowed them into SIGBUS. An OS thread gets 2 MiB or more, so 512 KiB still finds no more room
/// than a real thread has.
fn runner<S: shuttle::scheduler::Scheduler + 'static>(s: S) -> shuttle::Runner<S> {
    let mut config = shuttle::Config::new();
    config.stack_size = 0x80000;
    shuttle::Runner::new(s, config)
}

#[test]
fn random_schedules() {
    runner(shuttle::scheduler::RandomScheduler::new(iters())).run(scenario);
}

/// PCT picks a few priority-change points per run: good at bugs that need a handful of
/// specific context switches, which purely random schedules rarely hit.
#[test]
fn pct_schedules() {
    runner(shuttle::scheduler::PctScheduler::new(3, iters())).run(scenario);
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

/// Gate C (plan r3 section 5, approval validation): an approved proposal's merge races a person's write to its
/// world. Whatever the schedule, main ends with exactly the approved rows, or none of them: never the write made
/// after the approval.
fn approve_vs_write() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    run(
        &db,
        "main",
        "create table t (id int primary key, v text);
         create merge policy small with (max_rows = 1);
         create agent bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'small')",
        &[],
    )
    .unwrap();
    let mut bot = Session::new("main");
    bot.agent = Some("bot".into());
    bot.run(
        &db,
        "create world w; use world w; insert into t values (1, 'approved'), (2, 'approved'); use world main",
        &[],
    )
    .unwrap();
    let out = bot.run(&db, "propose merge world w", &[]).unwrap();
    let o = out.last().unwrap();
    let at = o.columns.as_ref().unwrap().iter().position(|(c, _)| c == "id").unwrap();
    let id = o.rows[0][at].to_text();
    run(&db, "main", &format!("approve proposal '{id}'"), &[]).unwrap();
    let merged = std::sync::Mutex::new(None);
    shuttle::thread::scope(|s| {
        s.spawn(|| *merged.lock().unwrap() = Some(run(&db, "main", &format!("merge proposal '{id}'"), &[]).is_ok()));
        s.spawn(|| {
            let _ = run(&db, "w", "update t set v = 'late' where id = 1", &[]); // fails once w is merged
        });
    });
    let rows = run(&db, "main", "select id, v from t order by id", &[]).unwrap().pop().unwrap().rows;
    let rows: Vec<String> = rows.iter().map(|r| format!("{}={}", r[0].to_text(), r[1].to_text())).collect();
    if merged.into_inner().unwrap() == Some(true) {
        assert_eq!(rows, ["1=approved", "2=approved"], "merged: exactly what was approved");
    } else {
        assert!(rows.is_empty(), "refused: nothing merged, {rows:?}");
    }
}

/// Rule E7 under every schedule: an approved strict proposal (it queues a payout) merges while a commit lands on main,
/// in a table it doesn't touch. The merge goes through only with no commit before it; refused, the proposal is
/// superseded and nothing is sent. Never both a payout and a superseded proposal, nor a refusal with no commit.
fn strict_merge_vs_commit() {
    use chronos::sql::{Session, run};
    let db = Db::new();
    run(
        &db,
        "main",
        "create table t (id int primary key, v text);
         create table other (id int primary key);
         create agent bot with (can = 'read,fork,write_own,write_main,merge_own')",
        &[],
    )
    .unwrap();
    let mut bot = Session::new("main");
    bot.agent = Some("bot".into());
    bot.run(&db, "create world w; use world w; insert into t values (1, 'paid'); notify on merge payout, 'pay 1'", &[])
        .unwrap();
    bot.run(&db, "use world main", &[]).unwrap();
    let column = |sql: &str, col: &str| -> String {
        let o = run(&db, "main", sql, &[]).unwrap().pop().unwrap();
        let at = o.columns.as_ref().unwrap().iter().position(|(c, _)| c == col).unwrap();
        o.rows[0][at].to_text()
    };
    let o = bot.run(&db, "propose merge world w", &[]).unwrap().pop().unwrap();
    let at = o.columns.as_ref().unwrap().iter().position(|(c, _)| c == "id").unwrap();
    let id = o.rows[0][at].to_text();
    run(&db, "main", &format!("approve proposal '{id}'"), &[]).unwrap();
    let (merged, committed) = (std::sync::Mutex::new(None), std::sync::Mutex::new(false));
    shuttle::thread::scope(|s| {
        s.spawn(|| {
            let r = run(&db, "main", &format!("merge proposal '{id}'"), &[]);
            *merged.lock().unwrap() = Some(r.map(|_| ()).map_err(|e| e.message));
        });
        s.spawn(|| *committed.lock().unwrap() = run(&db, "main", "insert into other values (1)", &[]).is_ok());
    });
    let sent = run(&db, "main", "show outbox", &[]).unwrap().pop().unwrap().rows.len();
    let status = column("show proposals", "status");
    match merged.into_inner().unwrap().unwrap() {
        Ok(()) => {
            assert_eq!(sent, 1, "merged: the payout queued");
            assert_eq!(status, "merged");
        }
        Err(m) => {
            assert!(m.contains("strict_target"), "refused only for a commit to main: {m}");
            assert!(*committed.lock().unwrap(), "a refusal with no commit to main");
            assert_eq!(sent, 0, "superseded: nothing sent");
            assert_eq!(status, "superseded");
        }
    }
}

#[test]
fn strict_merge_vs_commit_schedules() {
    big_runner(shuttle::scheduler::RandomScheduler::new(iters())).run(strict_merge_vs_commit);
    big_runner(shuttle::scheduler::PctScheduler::new(3, iters())).run(strict_merge_vs_commit);
}

/// `runner` with 8 MiB a task, as a main thread has: SQL statements' frames, in a debug build, need more than 512 KiB.
fn big_runner<S: shuttle::scheduler::Scheduler + 'static>(s: S) -> shuttle::Runner<S> {
    let mut config = shuttle::Config::new();
    config.stack_size = 0x800000;
    shuttle::Runner::new(s, config)
}

#[test]
fn approve_vs_write_schedules() {
    big_runner(shuttle::scheduler::RandomScheduler::new(iters())).run(approve_vs_write);
    big_runner(shuttle::scheduler::PctScheduler::new(3, iters())).run(approve_vs_write);
}
