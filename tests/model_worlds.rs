//! Worlds against a model: long seeded runs of fork, put, delete, merge (each of FAIL, OURS and THEIRS), rebase,
//! discard, undo merge and reopening the folder, each checked step by step against a small in-memory model of what
//! the docs say worlds do. After every step, whether it was refused must agree, and every world's rows, its parent
//! and the set of worlds must be exactly the model's.
//!
//! The model (independent of the engine's code):
//! - a world is its parent, its fork point (`base`: the parent's rows when it forked or last rebased) and its rows;
//! - a merge applies each row the world changed since its fork (its value differs from `base`) where the parent
//!   still holds the fork point's value; where the parent changed it too (even to the same value) it is a conflict:
//!   FAIL refuses the whole merge with nothing changed, OURS takes the world's value, THEIRS keeps the parent's;
//!   the world goes away. A world with forks of its own can't merge or be discarded;
//! - a rebase settles the same way, but into the world: its rows become the parent's now with its own changes on
//!   top, and its fork point becomes the parent's rows now. The parent doesn't change;
//! - undoing a world's merge puts back, in the world it merged into, each row the merge changed whose value is still
//!   what the merge left; a row changed since refuses it all, unless SKIP CHANGED leaves that row alone.
//!
//! A failure prints the seed, the step and the steps before it; `CHRONOS_MODEL_SEED=n` reruns that seed alone.
//!
//!     CHRONOS_MODEL_SEEDS=200 CHRONOS_MODEL_STEPS=2000 cargo test --release --test model_worlds

use chronos::{Db, MAIN, MergeOptions, Resolve};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

type Rows = BTreeMap<String, String>;

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

#[derive(Clone)]
struct World {
    parent: Option<String>,
    base: Rows,
    rows: Rows,
}

/// One merge, for undoing it: the world it went into, and each row it changed (before, after).
type Merged = (String, Vec<(String, Option<String>, Option<String>)>);

#[derive(Clone)]
struct Model {
    worlds: BTreeMap<String, World>,
    merges: BTreeMap<String, Merged>,
}

#[derive(Debug, Clone)]
enum Op {
    Put(String, String, String),
    Del(String, String),
    Fork(String, String),
    Merge(String, Resolve),
    Rebase(String, Resolve),
    Discard(String),
    Undo(String, bool),
    Reopen,
}

impl Model {
    fn new() -> Model {
        let main = World { parent: None, base: Rows::new(), rows: Rows::new() };
        Model { worlds: BTreeMap::from([(MAIN.to_string(), main)]), merges: BTreeMap::new() }
    }

    fn kids(&self, w: &str) -> usize {
        self.worlds.values().filter(|x| x.parent.as_deref() == Some(w)).count()
    }

    /// The parent's rows with world `b`'s changes settled by `how` on top, or None for a conflict FAIL refuses.
    fn settle(&self, b: &str, how: Resolve) -> Option<Rows> {
        let w = &self.worlds[b];
        let parent = &self.worlds[w.parent.as_deref().unwrap()].rows;
        let mut out = parent.clone();
        let keys: BTreeSet<&String> = w.base.keys().chain(w.rows.keys()).collect();
        for k in keys {
            let (before, after) = (w.base.get(k), w.rows.get(k));
            if before == after {
                continue; // not changed in the world
            }
            let take = if parent.get(k) == before {
                true
            } else {
                match how {
                    Resolve::Fail => return None,
                    Resolve::Ours => true,
                    Resolve::Theirs => false,
                }
            };
            if take {
                match after {
                    Some(v) => out.insert(k.clone(), v.clone()),
                    None => out.remove(k),
                };
            }
        }
        Some(out)
    }

    /// Applies `op`; false where the engine must refuse it (and the model is left as it was).
    fn apply(&mut self, op: &Op) -> bool {
        match op {
            Op::Put(w, k, v) => match self.worlds.get_mut(w) {
                Some(x) => {
                    x.rows.insert(k.clone(), v.clone());
                    true
                }
                None => false,
            },
            Op::Del(w, k) => match self.worlds.get_mut(w) {
                Some(x) => {
                    x.rows.remove(k);
                    true
                }
                None => false,
            },
            Op::Fork(from, name) => {
                let Some(x) = self.worlds.get(from) else { return false };
                let rows = x.rows.clone();
                self.worlds.insert(name.clone(), World { parent: Some(from.clone()), base: rows.clone(), rows });
                true
            }
            Op::Merge(b, how) => {
                if b == MAIN || !self.worlds.contains_key(b) || self.kids(b) > 0 {
                    return false;
                }
                let Some(next) = self.settle(b, *how) else { return false };
                let p = self.worlds[b].parent.clone().unwrap();
                let now = &self.worlds[&p].rows;
                let keys: BTreeSet<&String> = now.keys().chain(next.keys()).collect();
                let changed = keys
                    .into_iter()
                    .filter(|k| now.get(*k) != next.get(*k))
                    .map(|k| (k.clone(), now.get(k).cloned(), next.get(k).cloned()))
                    .collect();
                self.merges.insert(b.clone(), (p.clone(), changed));
                self.worlds.get_mut(&p).unwrap().rows = next;
                self.worlds.remove(b);
                true
            }
            Op::Rebase(b, how) => {
                if b == MAIN || !self.worlds.contains_key(b) {
                    return false;
                }
                let Some(next) = self.settle(b, *how) else { return false };
                let p = self.worlds[b].parent.clone().unwrap();
                let base = self.worlds[&p].rows.clone();
                let w = self.worlds.get_mut(b).unwrap();
                (w.base, w.rows) = (base, next);
                true
            }
            Op::Discard(b) => {
                if b == MAIN || !self.worlds.contains_key(b) || self.kids(b) > 0 {
                    return false;
                }
                self.worlds.remove(b);
                true
            }
            Op::Undo(child, skip) => {
                let Some((p, changed)) = self.merges.get(child) else { return false };
                let Some(target) = self.worlds.get(p) else { return false };
                let (mut back, mut moved) = (vec![], 0);
                for (k, before, after) in changed {
                    if target.rows.get(k) == after.as_ref() {
                        back.push((k.clone(), before.clone()));
                    } else {
                        moved += 1;
                    }
                }
                if moved > 0 && !skip {
                    return false;
                }
                let rows = &mut self.worlds.get_mut(p).unwrap().rows;
                for (k, v) in back {
                    match v {
                        Some(v) => rows.insert(k, v),
                        None => rows.remove(&k),
                    };
                }
                true
            }
            Op::Reopen => true,
        }
    }
}

fn run(db: &Db, op: &Op) -> bool {
    let o = |how: &Resolve| MergeOptions { how: *how, ..Default::default() };
    match op {
        Op::Put(w, k, v) => db.put(w, k, v).is_ok(),
        Op::Del(w, k) => db.del(w, k).is_ok(),
        Op::Fork(from, name) => db.fork(from, name).is_ok(),
        Op::Merge(b, how) => db.merge_with(b, &o(how)).is_ok(),
        Op::Rebase(b, how) => db.rebase(b, &o(how)).is_ok(),
        Op::Discard(b) => db.discard(b).is_ok(),
        Op::Undo(child, skip) => db.undo_merge(child, *skip).is_ok(),
        Op::Reopen => true,
    }
}

/// What the engine holds, as the model holds it: each world's parent and rows (the engine's own keys left out).
fn observed(db: &Db) -> BTreeMap<String, (Option<String>, Rows)> {
    db.branches()
        .into_iter()
        .filter(|(w, _)| !w.starts_with('_'))
        .map(|(w, parent)| {
            let rows = db
                .scan(&w, "", 100_000)
                .unwrap_or_else(|e| panic!("scan {w}: {e}"))
                .into_iter()
                .filter(|(k, _)| !k.starts_with('_'))
                .map(|(k, v)| (k, v.to_string()))
                .collect();
            (w, (parent, rows))
        })
        .collect()
}

fn expected(m: &Model) -> BTreeMap<String, (Option<String>, Rows)> {
    m.worlds.iter().map(|(w, x)| (w.clone(), (x.parent.clone(), x.rows.clone()))).collect()
}

fn pick(r: &mut Rng, names: &[String]) -> String {
    names[r.below(names.len())].clone()
}

/// The next step for seed `r`: mostly on worlds that exist, now and then on one that's gone.
fn next_op(r: &mut Rng, m: &Model, made: &mut usize, gone: &[String], undo: bool, reopen: bool) -> Op {
    let live: Vec<String> = m.worlds.keys().cloned().collect();
    let world = |r: &mut Rng| match gone.is_empty() || r.below(25) != 0 {
        true => pick(r, &live),
        false => pick(r, gone),
    };
    let key = |r: &mut Rng| format!("k{}", r.below(6));
    let how = |r: &mut Rng| [Resolve::Fail, Resolve::Ours, Resolve::Theirs][r.below(3)];
    loop {
        return match r.below(100) {
            0..=29 => {
                let w = world(r);
                let k = key(r);
                // now and then the value another world holds, so two sides write the same value
                let v = match r.below(4) {
                    0 => m.worlds.values().find_map(|x| x.rows.get(&k).cloned()).unwrap_or_else(|| "v0".into()),
                    _ => format!("v{}", r.below(50)),
                };
                Op::Put(w, k, v)
            }
            30..=39 => Op::Del(world(r), key(r)),
            40..=53 => {
                let from = world(r);
                let depth =
                    std::iter::successors(Some(from.as_str()), |w| m.worlds.get(*w).and_then(|x| x.parent.as_deref()))
                        .count();
                if depth > 4 {
                    continue;
                }
                *made += 1;
                Op::Fork(from, format!("w{made}"))
            }
            54..=71 => Op::Merge(world(r), how(r)),
            72..=81 => {
                let b = world(r);
                if m.kids(&b) > 0 {
                    continue; // (not modelled: a rebase of a world with forks of its own)
                }
                Op::Rebase(b, how(r))
            }
            82..=88 => Op::Discard(world(r)),
            89..=97 if undo => {
                let merged: Vec<String> = m.merges.keys().cloned().collect();
                if merged.is_empty() {
                    continue;
                }
                Op::Undo(pick(r, &merged), r.below(2) == 0)
            }
            98..=99 if reopen => Op::Reopen,
            _ => continue,
        };
    }
}

fn seeds() -> Vec<u64> {
    match std::env::var("CHRONOS_MODEL_SEED") {
        Ok(s) => vec![s.parse().expect("CHRONOS_MODEL_SEED")],
        Err(_) => {
            let n = std::env::var("CHRONOS_MODEL_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(12u64);
            (1..=n).collect()
        }
    }
}

fn steps(default: usize) -> usize {
    std::env::var("CHRONOS_MODEL_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn tmp(seed: u64) -> PathBuf {
    let d = std::env::temp_dir().join(format!("chronos-model-{}-{seed}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Runs seed `seed` for `n` steps; with `dir`, on disk (undo merge and reopening included), else in memory.
fn check(seed: u64, n: usize, dir: Option<&Path>) {
    let mut db = match dir {
        Some(d) => Db::open(d).unwrap(),
        None => Db::new(),
    };
    let (mut r, mut m) = (Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1), Model::new());
    let (mut made, mut gone, mut done): (usize, Vec<String>, Vec<Op>) = (0, vec![], vec![]);
    for step in 0..n {
        let op = next_op(&mut r, &m, &mut made, &gone, dir.is_some(), dir.is_some());
        let before: BTreeSet<String> = m.worlds.keys().cloned().collect();
        let mut after = m.clone();
        let want = after.apply(&op);
        let got = match (&op, dir) {
            (Op::Reopen, Some(d)) => {
                drop(db);
                db = Db::open(d).unwrap_or_else(|e| panic!("seed {seed}, step {step}: reopen: {e}"));
                true
            }
            _ => run(&db, &op),
        };
        let say = |what: String| {
            let last: Vec<String> = done.iter().rev().take(40).rev().map(|o| format!("{o:?}")).collect();
            panic!(
                "seed {seed}, step {step}, {op:?}: {what}\n(rerun: CHRONOS_MODEL_SEED={seed}; the steps before it, \
                 oldest first:\n  {})",
                last.join("\n  ")
            );
        };
        if got != want {
            say(format!("the engine {} it, the model {} it", ok(got), ok(want)));
        }
        if want {
            m = after;
            gone.extend(before.into_iter().filter(|w| !m.worlds.contains_key(w)));
        }
        let (seen, model) = (observed(&db), expected(&m));
        if seen != model {
            let diff: Vec<String> = model
                .keys()
                .chain(seen.keys())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|w| seen.get(*w) != model.get(*w))
                .map(|w| format!("{w}: engine {:?}, model {:?}", seen.get(w), model.get(w)))
                .collect();
            say(format!("worlds differ:\n  {}", diff.join("\n  ")));
        }
        done.push(op);
    }
}

fn ok(b: bool) -> &'static str {
    if b { "did" } else { "refused" }
}

#[test]
fn worlds_in_memory_match_the_model() {
    for seed in seeds() {
        check(seed, steps(600), None);
    }
}

#[test]
fn worlds_on_disk_match_the_model_with_undo_and_reopening() {
    for seed in seeds() {
        let dir = tmp(seed);
        check(seed, steps(300), Some(dir.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
