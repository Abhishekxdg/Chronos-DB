//! Durability: reopen keeps data, one process per folder, and the crash suite.
//!
//! Crash model: a crash leaves some byte prefix of the log on disk, possibly followed by garbage
//! (a torn write). Recovery must rebuild exactly the state after the ops whose records fully
//! survived, applied in log order; never anything else, never less than what was synced. Writes
//! to a world other than main are logged in batches, later than they were made (but before
//! anything that depends on them), so log order is op order with those writes moved later.
//! White-box on purpose: the test parses log frames ([u32 len][u32 crc][payload]) to know where
//! each record ends and what it holds. CRASH_RUNS=10000 runs the Phase 2 exit gate (default 1000).

use chronos::{Db, Resolve};
use std::fs;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

/// Everything observable about a database, as one comparable string.
fn fingerprint(db: &Db) -> String {
    let mut out = String::new();
    for (b, parent) in db.branches() {
        let rows = db.scan(&b, "", usize::MAX).unwrap();
        let diff = db.diff(&b).unwrap();
        out += &format!("{b} <- {parent:?}\n  {rows:?}\n  {diff:?}\n");
    }
    out
}

#[test]
fn reopen_keeps_main_and_forks() {
    let dir = tmp("reopen");
    {
        let db = Db::open(&dir).unwrap();
        db.put("main", "a", "1").unwrap();
        db.fork("main", "x").unwrap();
        db.put("x", "a", "2").unwrap();
        db.put("x", "b", "3").unwrap();
        db.fork("main", "y").unwrap();
        db.merge("y", Resolve::Fail).unwrap();
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.get("main", "a").unwrap().as_deref(), Some("1"));
    assert_eq!(db.get("x", "a").unwrap().as_deref(), Some("2"), "clean shutdown keeps fork work");
    assert_eq!(db.diff("x").unwrap().len(), 2, "fork point survives, so diff and merge still work");
    db.merge("x", Resolve::Fail).unwrap();
    assert_eq!(db.get("main", "b").unwrap().as_deref(), Some("3"));
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn checkpoint_then_reopen() {
    let dir = tmp("ckpt");
    let want = {
        let db = Db::open(&dir).unwrap();
        db.set_retention(0).unwrap(); // no time travel: nothing older than the checkpoint is kept
        for i in 0..100 {
            db.put("main", &format!("k{i}"), "v").unwrap();
        }
        db.fork("main", "x").unwrap();
        db.put("main", "k1", "main-after-fork").unwrap(); // fork base now differs from parent
        db.put("x", "k2", "fork").unwrap();
        db.del("x", "k3").unwrap();
        db.checkpoint().unwrap();
        db.put("main", "k4", "after-checkpoint").unwrap();
        fingerprint(&db)
    };
    let segs = fs::read_dir(&dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_str().unwrap().starts_with("wal-"))
        .count();
    assert_eq!(segs, 1, "checkpoint drops old log segments");
    let db = Db::open(&dir).unwrap();
    assert_eq!(fingerprint(&db), want);
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn one_process_per_folder() {
    let dir = tmp("lock");
    let _db = Db::open(&dir).unwrap();
    let err = Db::open(&dir).err().expect("second open must fail").to_string();
    assert!(err.contains("already open"), "{err}");
    drop(_db);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn corrupt_manifest_refuses_to_open() {
    let dir = tmp("badsnap");
    {
        let db = Db::open(&dir).unwrap();
        db.put("main", "a", "1").unwrap();
        db.checkpoint().unwrap();
    }
    let p = dir.join("manifest");
    let mut b = fs::read(&p).unwrap();
    b[12] ^= 0xff;
    fs::write(&p, b).unwrap();
    let err = Db::open(&dir).err().expect("must refuse").to_string();
    assert!(err.contains("corrupt"), "{err}");
    fs::remove_dir_all(&dir).unwrap();
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// An op that succeeded, and so was logged.
#[derive(Debug)]
enum Step {
    Write(String, Vec<(String, Option<String>)>),
    Fork(String, String),
    Merge(String, Resolve),
    Discard(String),
}

fn apply(db: &Db, s: &Step) {
    let ok = match s {
        Step::Write(b, rows) => {
            db.batch(b, &rows.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect::<Vec<_>>()).is_ok()
        }
        Step::Fork(from, name) => db.fork(from, name).is_ok(),
        Step::Merge(b, how) => db.merge(b, *how).is_ok(),
        Step::Discard(b) => db.discard(b).is_ok(),
    };
    assert!(ok, "{s:?} fails replayed in log order");
}

/// The steps each record holds, record by record: writes by the world they went to (in order,
/// `n` of them for a batch), the rest in the order they were made.
fn in_log_order<'a>(steps: &'a [Step], recs: &[(u8, String, usize, usize)]) -> Vec<Vec<&'a Step>> {
    let mut taken = vec![false; steps.len()];
    let mut out = vec![];
    for (tag, world, n, _) in recs {
        let mut held = vec![];
        let want = |s: &Step| match (tag, s) {
            (1 | 17, Step::Write(b, _)) => b == world,
            (1 | 17, _) | (_, Step::Write(..)) => false,
            (3 | 8, Step::Fork(..)) | (4 | 11 | 13, Step::Merge(..)) | (5, Step::Discard(_)) => true,
            _ => panic!("a record with tag {tag} where {s:?} is next"),
        };
        for (i, s) in steps.iter().enumerate() {
            if held.len() < *n && !taken[i] && want(s) {
                taken[i] = true;
                held.push(s);
            }
        }
        assert_eq!(held.len(), *n, "record tag {tag} ({world}) holds {n} steps");
        out.push(held);
    }
    assert!(taken.iter().all(|t| *t), "every step is in the log");
    out
}

/// Runs random ops on `db`, adding each one that succeeded (and so was logged) to `steps`.
/// `forked` lists the worlds forked so far, oldest first (a name comes back once it's gone).
fn random_ops(db: &Db, reference: &Db, r: &mut Rng, n: usize, steps: &mut Vec<Step>, forked: &mut Vec<String>) {
    let mut names = 0;
    for _ in 0..n {
        let branches: Vec<String> = db.branches().into_iter().map(|(b, _)| b).collect();
        let b = branches[r.below(branches.len())].clone();
        let key = format!("k{}", r.below(8));
        let val = format!("v{}", r.below(1000));
        let how = [Resolve::Fail, Resolve::Ours, Resolve::Theirs][r.below(3)];
        let op = r.below(12);
        if op == 11 {
            // several at once: logged one by one, deepest first then the later fork, as the reference
            // drops them (names are reused once gone, so fork order is what `forked` says)
            let mut some: Vec<String> = branches.iter().filter(|_| r.below(3) == 0).cloned().collect();
            let got: std::collections::HashMap<String, bool> =
                some.iter().cloned().zip(db.discard_many(&some).iter().map(|o| o.is_ok())).collect();
            some.sort_by_key(|n| {
                let depth = reference.world(n).map_or(0, |w| w.depth);
                std::cmp::Reverse((depth, forked.iter().rposition(|f| f == n)))
            });
            for n in &some {
                let ok = reference.discard(n).is_ok();
                assert_eq!(got[n], ok, "{n}: discard_many and discard must agree");
                if ok {
                    steps.push(Step::Discard(n.clone()));
                }
            }
            continue;
        }
        let rows: Vec<(String, Option<String>)> =
            (0..1 + r.below(3)).map(|i| (format!("k{}", r.below(8)), (i != 1).then(|| format!("b{i}")))).collect();
        let rows: Vec<(&str, Option<&str>)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
        let run = |d: &Db| match op {
            10 => d.batch(&b, &rows).is_ok(),
            0..=3 => d.put(&b, &key, &val).is_ok(),
            4 => d.del(&b, &key).is_ok(),
            5 | 6 => d.fork(&b, &format!("f{names}")).is_ok(),
            7 | 8 => d.merge(&b, how).is_ok(),
            _ => d.discard(&b).is_ok(),
        };
        let ok = run(db);
        assert_eq!(ok, run(reference), "durable and in-memory engines must agree");
        if op == 5 || op == 6 {
            if ok {
                forked.push(format!("f{names}"));
            }
            names += 1;
        }
        if ok {
            let one = |v: Option<&str>| vec![(key.clone(), v.map(String::from))];
            steps.push(match op {
                10 => Step::Write(b, rows.iter().map(|(k, v)| (k.to_string(), v.map(String::from))).collect()),
                0..=3 => Step::Write(b, one(Some(&val))),
                4 => Step::Write(b, one(None)),
                5 | 6 => Step::Fork(b, format!("f{}", names - 1)),
                7 | 8 => Step::Merge(b, how),
                _ => Step::Discard(b),
            });
        }
    }
}

/// Each record in a log segment that changes state: (tag, the world a write or batch of writes
/// (tag 17) went to, how many ops it holds, where it ends). Close, Flag, Meta, Clock, Actor and
/// Synced records (tags 6, 7, 9, 10, 12, 16) change nothing a fingerprint sees (here a Meta is a
/// refused merge counted in the world's metadata).
fn records(seg: &Path) -> Vec<(u8, String, usize, usize)> {
    let b = fs::read(seg).unwrap();
    let (mut off, mut out) = (0, vec![]);
    let u32_at = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
    while off + 8 <= b.len() {
        let (p, tag) = (off + 8, b[off + 8]);
        off += 8 + u32_at(off);
        if [6, 7, 9, 10, 12, 16].contains(&tag) {
            continue;
        }
        let (world, n) = match tag {
            1 | 17 => {
                let len = u32_at(p + 1);
                let world = String::from_utf8(b[p + 5..p + 5 + len].to_vec()).unwrap();
                (world, if tag == 17 { u32_at(p + 5 + len) } else { 1 })
            }
            _ => (String::new(), 1),
        };
        out.push((tag, world, n, off));
    }
    assert_eq!(off, b.len(), "clean shutdown leaves only whole frames");
    out
}

/// Every frame in a log segment as (tag, end offset), for failure reports.
fn frame_tags(seg: &Path) -> Vec<(u8, usize)> {
    let b = fs::read(seg).unwrap();
    let (mut off, mut out) = (0, vec![]);
    while off + 8 <= b.len() {
        let tag = b[off + 8];
        off += 8 + u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
        out.push((tag, off));
    }
    out
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dst = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dst);
        } else {
            fs::copy(&p, dst).unwrap();
        }
    }
}

fn last_segment(dir: &Path) -> PathBuf {
    let mut segs: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("wal-"))
        .collect();
    segs.sort();
    segs.pop().unwrap()
}

#[test]
fn crash_suite() {
    let runs: usize = std::env::var("CRASH_RUNS").ok().and_then(|s| s.parse().ok()).unwrap_or(1000);
    let per_seed = 20;
    let (dir, crash) = (tmp("crash-src"), tmp("crash-dst"));
    for seed in 0..runs.div_ceil(per_seed) as u64 {
        let _ = fs::remove_dir_all(&dir);
        let mut r = Rng(seed * 2654435761 + 1);
        let reference = Db::new();
        let (mut steps, mut forked) = (vec![], vec![]);
        let before_ckpt;
        {
            let db = Db::open(&dir).unwrap();
            random_ops(&db, &reference, &mut r, 40, &mut steps, &mut forked);
            let checkpointed = r.below(2) == 0;
            if checkpointed {
                db.checkpoint().unwrap();
                if r.below(2) == 0 {
                    db.gc().unwrap(); // recovery must still find everything the manifest names
                }
            }
            before_ckpt = if checkpointed { steps.len() } else { 0 };
            random_ops(&db, &reference, &mut r, 40, &mut steps, &mut forked);
        }
        let seg = last_segment(&dir);
        // the state after each record of the last segment, from the checkpoint's (all of it is
        // logged before the checkpoint's cut) or from empty
        let recs = records(&seg);
        let ends: Vec<usize> = recs.iter().map(|r| r.3).collect();
        let replayed = Db::new();
        for s in &steps[..before_ckpt] {
            apply(&replayed, s);
        }
        let mut states = vec![fingerprint(&replayed)];
        for s in in_log_order(&steps[before_ckpt..], &recs) {
            for s in s {
                apply(&replayed, s);
            }
            states.push(fingerprint(&replayed));
        }
        assert_eq!(fingerprint(&replayed), fingerprint(&reference), "seed {seed}: log order ends where op order does");
        let len = fs::metadata(&seg).unwrap().len() as usize;
        for i in 0..per_seed {
            // cut anywhere, including exactly on frame boundaries, empty, and not at all
            let cut = match i {
                0 => 0,
                1 => len,
                2 => ends.get(r.below(ends.len().max(1))).copied().unwrap_or(0),
                _ => r.below(len + 1),
            };
            let _ = fs::remove_dir_all(&crash);
            copy_dir(&dir, &crash);
            let whole = fs::read(&seg).unwrap();
            let mut bytes = whole[..cut].to_vec();
            if r.below(2) == 0 {
                bytes.extend((0..r.below(40)).map(|_| r.next() as u8)); // torn write garbage
                // garbage whose first byte is the one cut off can rebuild the cut frame exactly (a
                // 1 in 256 chance): then the write did land, and recovery rightly keeps it
                if let (Some(g), Some(&was)) = (bytes.get_mut(cut), whole.get(cut))
                    && *g == was
                {
                    *g ^= 0xff;
                }
            }
            fs::write(crash.join(seg.file_name().unwrap()), bytes).unwrap();

            let survived = ends.iter().filter(|&&e| e <= cut).count();
            let want = &states[survived];
            let got = fingerprint(&Db::open(&crash).unwrap());
            if &got != want {
                // say enough to tell a misaligned expectation from a recovery that lost or invented a write
                let matches: Vec<usize> =
                    states.iter().enumerate().filter(|(_, s)| **s == got).map(|(i, _)| i).collect();
                let segs: Vec<String> = fs::read_dir(&dir)
                    .unwrap()
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|n| n.starts_with("wal-") || n == "manifest")
                    .collect();
                panic!(
                    "seed {seed} cut {cut}/{len}: recovered state is not the synced prefix\n\
                     expected state {survived} (before_ckpt {before_ckpt}, survived {survived} of {} op frames, {} states); \
                     the recovered state equals state(s) {matches:?}\nframes (tag@end): {:?}\nfiles: {segs:?}\n\
                     got:  {got}\nwant: {want}",
                    ends.len(),
                    states.len(),
                    frame_tags(&seg),
                );
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&crash);
}

/// Log order must be a valid replay order even when agents race: reopen must rebuild exactly
/// the live state. Threads fork main while others merge into it and write it directly.
#[test]
fn concurrent_history_replays_exactly() {
    let dir = tmp("concurrent");
    for round in 0..5u64 {
        let _ = fs::remove_dir_all(&dir);
        let live = {
            let db = Db::open(&dir).unwrap();
            let done = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|s| {
                // checkpoints race the agents: each one rebases every live branch mid-flight
                s.spawn(|| {
                    while !done.load(std::sync::atomic::Ordering::Relaxed) {
                        db.checkpoint().unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(3));
                    }
                });
                let agents: Vec<_> = (0..16u64)
                    .map(|t| {
                        let db = &db;
                        s.spawn(move || {
                            let mut r = Rng((round * 97 + t) * 2654435761 + 7);
                            for i in 0..60 {
                                let b = format!("t{t}-{i}");
                                if db.fork("main", &b).is_err() {
                                    continue;
                                }
                                for _ in 0..r.below(4) {
                                    db.put(&b, &format!("k{}", r.below(6)), &format!("t{t}")).unwrap();
                                }
                                if r.below(3) == 0 {
                                    db.put("main", &format!("k{}", r.below(6)), &format!("m{t}")).unwrap();
                                }
                                if r.below(5) == 0 {
                                    db.batch(&b, &[("k0", Some("batch")), ("k1", None)]).unwrap();
                                }
                                let how = [Resolve::Fail, Resolve::Ours, Resolve::Theirs][r.below(3)];
                                if r.below(4) == 0 || db.merge(&b, how).is_err() {
                                    db.discard(&b).unwrap();
                                }
                            }
                        })
                    })
                    .collect();
                for a in agents {
                    a.join().unwrap();
                }
                done.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            fingerprint(&db)
        };
        let reopened = fingerprint(&Db::open(&dir).unwrap());
        assert_eq!(reopened, live, "round {round}: replaying the log did not rebuild the live state");
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// Many forks taken while main keeps changing: the snapshot chains their bases, and reopening must
/// rebuild every fork point exactly (diff and merge depend on it).
#[test]
fn checkpoint_chains_many_fork_points() {
    let dir = tmp("chain");
    let want = {
        let db = Db::open(&dir).unwrap();
        for i in 0..30 {
            db.put("main", &format!("k{}", i % 7), &format!("v{i}")).unwrap();
            db.fork("main", &format!("f{i}")).unwrap();
            db.put(&format!("f{i}"), "mine", &format!("{i}")).unwrap();
            if i % 5 == 0 {
                db.fork(&format!("f{i}"), &format!("g{i}")).unwrap();
                db.del(&format!("g{i}"), "k0").unwrap();
            }
        }
        db.merge("f29", Resolve::Fail).unwrap();
        db.checkpoint().unwrap();
        fingerprint(&db)
    };
    let db = Db::open(&dir).unwrap();
    assert_eq!(fingerprint(&db), want);
    db.fork("main", "after-reopen").unwrap();
    db.checkpoint().unwrap(); // chaining must also work on a reloaded fork order
    let again = fingerprint(&db);
    drop(db);
    assert_eq!(fingerprint(&Db::open(&dir).unwrap()), again);
    fs::remove_dir_all(&dir).unwrap();
}

/// Rows that live in on-disk trees: deletes hide them, forks and merges span checkpoints, and
/// everything survives reopening.
#[test]
fn trees_and_overlays_across_checkpoints() {
    let dir = tmp("trees");
    let db = Db::open(&dir).unwrap();
    let rows: Vec<(String, String)> = (0..5000).map(|i| (format!("k{i:05}"), format!("v{i}"))).collect();
    db.batch("main", &rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
    db.fork("main", "old").unwrap(); // forked before the checkpoint, worked on after it
    db.checkpoint().unwrap(); // everything now lives in trees

    db.del("main", "k00010").unwrap(); // hides a tree row
    assert_eq!(db.get("main", "k00010").unwrap(), None);
    assert!(db.scan("main", "k00009", 2).unwrap().iter().all(|(k, _)| k != "k00010"));
    db.put("old", "k00020", "changed").unwrap();
    db.put("old", "new", "row").unwrap();
    db.del("old", "k00030").unwrap();
    let d = db.diff("old").unwrap();
    assert_eq!(d.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), ["k00020", "k00030", "new"]);
    db.checkpoint().unwrap();
    assert_eq!(db.diff("old").unwrap(), d, "diff is the same whether the changes sit in trees or overlays");
    db.merge("old", Resolve::Fail).unwrap();
    assert_eq!(db.get("main", "k00020").unwrap().as_deref(), Some("changed"));
    assert_eq!(db.prefix("main", "k").unwrap().len(), 5000 - 2);
    let want = fingerprint(&db);
    drop(db);
    let db = Db::open(&dir).unwrap();
    assert_eq!(fingerprint(&db), want);
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

fn folder_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            if p.is_dir() { folder_bytes(&p) } else { fs::metadata(&p).unwrap().len() }
        })
        .sum()
}

/// Rewriting everything several times leaves dead pages behind; cleanup frees them, keeps every
/// row of main and of a fork that still points at the old data, and the result survives reopening.
#[test]
fn gc_frees_dead_pages_and_keeps_live_ones() {
    let dir = tmp("gc");
    let db = Db::open(&dir).unwrap();
    db.set_retention(0).unwrap(); // no history to keep old versions alive
    let write_all = |tag: &str| {
        let rows: Vec<(String, String)> =
            (0..20_000).map(|i| (format!("k{i:05}"), format!("{tag}-{i}-{}", "x".repeat(200)))).collect();
        db.batch("main", &rows.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
        db.checkpoint().unwrap();
    };
    write_all("v1");
    db.fork("main", "old").unwrap(); // pins v1
    for tag in ["v2", "v3", "v4", "v5"] {
        write_all(tag);
    }
    let before = folder_bytes(&dir);
    let (pages, freed) = db.gc().unwrap();
    let after = folder_bytes(&dir);
    assert!(pages > 0 && freed > 0, "{pages} pages, {freed} bytes");
    assert!(after < before * 2 / 3, "folder {before} -> {after} bytes");

    assert!(db.get("main", "k12345").unwrap().unwrap().starts_with("v5-12345-"));
    assert!(db.get("old", "k12345").unwrap().unwrap().starts_with("v1-12345-"), "the fork pins v1");
    assert_eq!(db.diff("old").unwrap().len(), 0);
    let want = fingerprint(&db);
    drop(db);
    assert_eq!(fingerprint(&Db::open(&dir).unwrap()), want);
    fs::remove_dir_all(&dir).unwrap();
}

/// Readers hold on to the trees they are reading, so cleanup running under them never pulls a
/// page away mid-scan.
#[test]
fn gc_never_breaks_concurrent_readers() {
    let dir = tmp("gc-readers");
    let db = Db::open(&dir).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let rows = |tag: u32| -> Vec<(String, String)> {
        (0..3000).map(|i| (format!("k{i:05}"), format!("{tag}-{}", "y".repeat(100)))).collect()
    };
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let got = db.scan("main", "", usize::MAX).unwrap();
                    // every scan sees one whole version: 3000 rows, all with the same tag
                    if !got.is_empty() {
                        assert_eq!(got.len(), 3000);
                        let tag = got[0].1.split('-').next().unwrap().to_string();
                        assert!(got.iter().all(|(_, v)| v.starts_with(&format!("{tag}-"))), "torn read");
                    }
                }
            });
        }
        for round in 0..40 {
            let r = rows(round);
            db.batch("main", &r.iter().map(|(k, v)| (k.as_str(), Some(v.as_str()))).collect::<Vec<_>>()).unwrap();
            db.checkpoint().unwrap();
            db.gc().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

/// A branch's unsynced writes can die in a crash. The merge must then fail loudly, never merge
/// less than the agent wrote: with the version its last write returned, or (without one) because
/// every branch open during a crash is flagged until a merge confirms it.
#[test]
fn crash_lost_writes_make_merge_fail_loudly() {
    let (dir, crashed) = (tmp("lost"), tmp("lost-copy"));
    let db = Db::open(&dir).unwrap();
    db.fork("main", "agent").unwrap();
    db.put("main", "sync", "point").unwrap(); // fsyncs the log: the fork is durable
    let v = db.put("agent", "work", "done").unwrap(); // buffered, not synced
    assert_eq!(v, 1);
    copy_dir(&dir, &crashed); // "kill -9" now: the buffered write never reached the file
    drop(db);

    let db = Db::open(&crashed).unwrap();
    assert_eq!(db.get("agent", "work").unwrap(), None, "the unsynced write is gone");
    let stale = |r: chronos::Result<usize>| matches!(r, Err(chronos::Error::Stale { .. }));
    assert!(stale(db.merge_at("agent", Resolve::Fail, Some(1))), "version 1 was lost");
    assert!(stale(db.merge("agent", Resolve::Fail)), "flagged: a plain merge must not succeed");
    let plan = db.merge_preview("agent", &Default::default()).unwrap();
    assert!(plan.blocked.is_some_and(|b| b.contains("crash")), "a dry run says why the merge will refuse");
    drop(db);
    let db = Db::open(&crashed).unwrap(); // clean restart: the flag stays
    assert!(stale(db.merge("agent", Resolve::Fail)));
    db.checkpoint().unwrap(); // and survives checkpoints
    drop(db);
    let db = Db::open(&crashed).unwrap();
    assert!(stale(db.merge("agent", Resolve::Fail)));
    assert_eq!(db.merge_at("agent", Resolve::Fail, Some(db.version("agent").unwrap())).unwrap(), 0, "confirmed");
    drop(db);
    for d in [dir, crashed] {
        fs::remove_dir_all(d).unwrap();
    }
}

/// synchronous_commit: set from SQL, kept across reopen, and every mode still loses nothing on a
/// clean close.
#[test]
fn sync_commit_setting_persists() {
    use chronos::SyncCommit;
    use chronos::sql::Session;
    let dir = tmp("sync-commit");
    {
        let db = Db::open(&dir).unwrap();
        assert_eq!(db.sync_commit(), SyncCommit::Full, "full by default");
        let mut s = Session::new("main");
        s.run(&db, "alter system set synchronous_commit = off", &[]).unwrap();
        assert_eq!(db.sync_commit(), SyncCommit::Off);
        s.run(&db, "create table t (id int primary key); insert into t values (1)", &[]).unwrap();
        s.run(&db, "alter system set synchronous_commit to 'normal'", &[]).unwrap();
        s.run(&db, "insert into t values (2)", &[]).unwrap();
        assert!(s.run(&db, "alter system set synchronous_commit = sometimes", &[]).is_err());
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.sync_commit(), SyncCommit::Normal, "kept across reopen");
    let mut s = Session::new("main");
    let out = s.run(&db, "show synchronous_commit", &[]).unwrap().pop().unwrap();
    assert_eq!(out.rows[0][0].to_text(), "normal");
    let out = s.run(&db, "select count(*) from t", &[]).unwrap().pop().unwrap();
    assert_eq!(out.rows[0][0].to_text(), "2");
    assert_eq!(Db::new().sync_commit(), SyncCommit::Off, "in memory: nothing reaches disk");
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}

/// The live log segment is kept ahead of its records in zeros (so commits don't grow the file):
/// a clean close cuts them off, and zeros a crash leaves behind are read as the log's end.
#[test]
fn log_zeros_ahead_are_not_records() {
    let dir = tmp("zeros-ahead");
    let last_seg = |dir: &Path| {
        let mut w: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("wal-"))
            .collect();
        w.sort();
        w.pop().unwrap()
    };
    {
        let db = Db::open(&dir).unwrap();
        db.put("main", "a", "1").unwrap();
        let len = fs::metadata(last_seg(&dir)).unwrap().len();
        assert_eq!(len % (4 << 20), 0, "padded ahead while open: {len}");
    }
    let wal = last_seg(&dir);
    let clean = fs::metadata(&wal).unwrap().len();
    assert!(clean < 4 << 20 && clean > 0, "a clean close trims the zeros: {clean}");
    // what a crash leaves: records, then zeros
    let mut f = fs::OpenOptions::new().append(true).open(&wal).unwrap();
    std::io::Write::write_all(&mut f, &vec![0; 1 << 20]).unwrap();
    drop(f);
    {
        let db = Db::open(&dir).unwrap();
        assert_eq!(db.get("main", "a").unwrap().as_deref(), Some("1"));
        db.put("main", "b", "2").unwrap();
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(db.get("main", "b").unwrap().as_deref(), Some("2"), "writes after the zeros were cut survive");
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}
