//! What views read while a piece of work runs, to tell later whether that work would come out the same: if every
//! key and prefix it read reads the same now, it would (see SQL's rewrite cache, `sql::Statement::catalog`).
//!
//! Only `View::get` and `View::prefix` are kept, with what they found: a key's small value itself and anything
//! larger as a fingerprint (see `Seen`, `Print`); a scan, a search, a read of the past or a trace grown past `MOST`
//! spoils it (such work isn't kept). Nothing is held but keys, fingerprints and values up to `KEEP` bytes (shared
//! with the tree, not copied), and no tree, so a kept trace pins nothing a cleanup would free but those few values.

use crate::{Core, PAST, Val, View};
use std::cell::{Cell, RefCell};
use std::hash::{BuildHasher, Hash, Hasher, RandomState};

/// Where a read was made: the database, by its address (only to tell open databases apart: what was read is
/// read again and compared, so an address used again by another database can't pass for it), and the world.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct At {
    core: usize,
    branch: String,
}

/// One read, kept: a row by its key and what was there, or the rows under a prefix as far as they were read (how
/// many, their fingerprint, and whether that was all of them).
#[derive(Clone, Debug)]
pub(crate) enum Read {
    Key(At, String, Seen),
    Prefix(At, String, usize, Print, bool),
}

/// What a key read found, kept to compare with what it finds now: the value itself when it is small (the row is
/// shared, not copied, and reading the same value again is a pointer comparison, where a fingerprint takes a hash of
/// all of it, twice), else its fingerprint.
#[derive(Clone, Debug)]
pub(crate) enum Seen {
    Value(Option<Val>),
    Print(Print),
}

/// The longest value a key read keeps whole (see `Seen`): longer ones are kept as a fingerprint, so a trace pins
/// little.
const KEEP: usize = 4096;

/// One read as a trace collects it, with what it found (see `Read`).
enum Raw {
    Key(At, String, Option<Val>),
    Prefix(At, String, Vec<(String, Val)>, bool),
}

impl Raw {
    fn kept(&self) -> Read {
        match self {
            Raw::Key(at, k, v) => Read::Key(
                at.clone(),
                k.clone(),
                match v {
                    Some(v) if v.len() > KEEP => Seen::Print(Print::of_key(k, Some(v))),
                    v => Seen::Value(v.clone()),
                },
            ),
            Raw::Prefix(at, p, rows, done) => {
                let mut print = Printer::new();
                for (k, v) in rows {
                    print.add(k, Some(v));
                }
                Read::Prefix(at.clone(), p.clone(), rows.len(), print.done(), *done)
            }
        }
    }
}

/// A fingerprint of rows read: two 64-bit SipHash hashes of their keys and values (each value's absence too), under
/// two keys this process drew at random, so no one can make different rows print alike (and by chance they do
/// once in 2^128), and their length in bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Print(u64, u64, usize);

/// The keys `Print` hashes under.
static KEYS: std::sync::LazyLock<(RandomState, RandomState)> =
    std::sync::LazyLock::new(|| (RandomState::new(), RandomState::new()));

struct Printer(std::hash::DefaultHasher, std::hash::DefaultHasher, usize);

impl Printer {
    fn new() -> Printer {
        Printer(KEYS.0.build_hasher(), KEYS.1.build_hasher(), 0)
    }

    fn add(&mut self, k: &str, v: Option<&str>) {
        (k, v).hash(&mut self.0);
        (k, v).hash(&mut self.1);
        self.2 += k.len() + v.map_or(0, str::len);
    }

    fn done(self) -> Print {
        Print(self.0.finish(), self.1.finish(), self.2)
    }
}

impl Print {
    fn of_key(k: &str, v: Option<&str>) -> Print {
        let mut p = Printer::new();
        p.add(k, v);
        p.done()
    }
}

#[derive(Default)]
struct Trace {
    /// which trace this is (see `prefix`)
    id: u64,
    reads: Vec<Raw>,
    /// how many rows the reads hold (each key one)
    size: usize,
    /// read in a way that can't be read again the same
    spoiled: bool,
}

/// Most rows a trace keeps: past them, reading them all again would cost about what the work did.
const MOST: usize = 256;

thread_local! {
    static TRACE: RefCell<Option<Trace>> = const { RefCell::new(None) };
    /// the last trace's id
    static IDS: Cell<u64> = const { Cell::new(0) };
    /// a trace is running (`TRACE` is Some): what every read checks
    static ON: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f`, keeping what views read meanwhile: None if it read in a way that can't be read again the same. A
/// trace inside another adds its reads to the outer one too (what the inner work read, the outer work did).
pub(crate) fn trace<T>(f: impl FnOnce() -> T) -> (T, Option<Vec<Read>>) {
    /// Puts the outer trace back however `f` ends, with this one's reads added.
    struct Back(Option<Trace>, bool);
    impl Drop for Back {
        fn drop(&mut self) {
            let inner = TRACE.replace(self.0.take());
            ON.set(self.1);
            TRACE.with_borrow_mut(|t| {
                if let (Some(t), Some(inner)) = (t, inner) {
                    t.size += inner.size;
                    t.spoiled |= inner.spoiled || t.size > MOST;
                    t.reads.extend(inner.reads);
                }
            });
        }
    }
    IDS.set(IDS.get() + 1);
    let back = Back(TRACE.replace(Some(Trace { id: IDS.get(), ..Trace::default() })), ON.replace(true));
    let out = f();
    let mine =
        TRACE.with_borrow(|t| t.as_ref().filter(|t| !t.spoiled).map(|t| t.reads.iter().map(Raw::kept).collect()));
    drop(back);
    (out, mine)
}

/// Whether a trace is running: then reads are kept.
pub(crate) fn on() -> bool {
    ON.get()
}

/// Keeps a read through `v`, which `read` makes from where `v` reads.
fn keep(v: &View, rows: usize, read: impl FnOnce(At) -> Raw) {
    TRACE.with_borrow_mut(|t| {
        let Some(t) = t else { return };
        t.size += rows;
        if t.spoiled || v.born == PAST || t.size > MOST {
            t.spoiled = true;
            return;
        }
        t.reads.push(read(At { core: std::ptr::from_ref(v.core) as usize, branch: v.branch.clone() }));
    });
}

/// `View::get`'s read of `k`, which found `val` (once: the same key read again in the same world is kept once).
pub(crate) fn key(v: &View, k: &str, val: &Option<Val>) {
    let core = std::ptr::from_ref(v.core) as usize;
    let again = TRACE.with_borrow(|t| {
        t.as_ref().is_some_and(|t| {
            t.reads.iter().any(|r| matches!(r, Raw::Key(a, x, _) if x == k && a.core == core && a.branch == v.branch))
        })
    });
    if !again {
        keep(v, 1, |at| Raw::Key(at, k.to_string(), val.clone()));
    }
}

/// A read that can't be read again the same (a scan, a search): the trace isn't kept.
pub(crate) fn spoil() {
    TRACE.with_borrow_mut(|t| {
        if let Some(t) = t {
            t.spoiled = true;
        }
    });
}

/// `View::prefix`'s rows under `prefix`, kept as they are read: the iterator `rows`, wrapped.
pub(crate) fn prefix<'a>(
    v: &View,
    prefix: &str,
    mut rows: impl Iterator<Item = crate::Result<(String, Val)>> + 'a,
) -> impl Iterator<Item = crate::Result<(String, Val)>> + 'a {
    // the trace it was made in, and the read's place there, filled in as its rows come (no trace: 0)
    let (mut id, mut at) = (0, None);
    if on() {
        keep(v, 1, |a| Raw::Prefix(a, prefix.to_string(), vec![], false));
        TRACE.with_borrow(|t| {
            if let Some(t) = t.as_ref() {
                id = t.id;
                at = (!t.spoiled).then(|| t.reads.len() - 1);
            }
        });
    }
    std::iter::from_fn(move || {
        let next = rows.next();
        if !on() {
            return next;
        }
        TRACE.with_borrow_mut(|t| {
            let Some(t) = t.as_mut().filter(|t| !t.spoiled) else { return };
            // (rows read in a trace other than the one it was made in: that trace can't say what was read)
            let Some(i) = at.filter(|i| t.id == id && *i < t.reads.len()) else {
                t.spoiled = true;
                return;
            };
            match &next {
                Some(Ok((k, val))) => {
                    t.size += 1;
                    if t.size > MOST {
                        t.spoiled = true;
                    } else if let Raw::Prefix(_, _, found, _) = &mut t.reads[i] {
                        found.push((k.clone(), val.clone()));
                    }
                }
                Some(Err(_)) => t.spoiled = true,
                None => {
                    if let Raw::Prefix(_, _, _, done) = &mut t.reads[i] {
                        *done = true;
                    }
                }
            }
        });
        next
    })
}

/// Whether every one of `reads` was made in world `branch` of `core` (and so is answered by that world's state).
pub(crate) fn only_in(reads: &[Read], core: &Core, branch: &str) -> bool {
    let here = std::ptr::from_ref(core) as usize;
    reads.iter().all(|r| {
        let (Read::Key(at, ..) | Read::Prefix(at, ..)) = r;
        at.core == here && at.branch == branch
    })
}

/// Whether every one of `reads` reads the same now: each in `core` or, a session's temporary tables', `temp`.
pub(crate) fn same(reads: &[Read], core: &Core, temp: Option<&Core>) -> bool {
    same_in(reads, core, temp, None)
}

/// `same`, with what was read in world `moved.0` read in world `moved.1` instead (a transaction's reads, in the
/// next transaction's world).
pub(crate) fn same_in(reads: &[Read], core: &Core, temp: Option<&Core>, moved: Option<(&str, &str)>) -> bool {
    let mut views: Vec<(&At, View)> = vec![];
    for r in reads {
        let (Read::Key(at, ..) | Read::Prefix(at, ..)) = r;
        let i = match views.iter().position(|(a, _)| *a == at) {
            Some(i) => i,
            None => {
                let db = [Some(core), temp].into_iter().flatten().find(|c| std::ptr::from_ref(*c) as usize == at.core);
                let branch = match moved {
                    Some((from, to)) if at.branch == from => to,
                    _ => at.branch.as_str(),
                };
                let Some(Ok(v)) = db.map(|c| c.view_tx(branch)) else { return false };
                views.push((at, v));
                views.len() - 1
            }
        };
        let v = &views[i].1;
        let same = match r {
            Read::Key(_, k, was) => v.get(k).is_ok_and(|now| match was {
                Seen::Value(was) => *was == now,
                Seen::Print(was) => Print::of_key(k, now.as_deref()) == *was,
            }),
            Read::Prefix(_, p, n, was, done) => {
                let Ok(mut now) = v.prefix(p) else { return false };
                let mut print = Printer::new();
                for _ in 0..*n {
                    match now.next() {
                        Some(Ok((k, val))) => print.add(&k, Some(&val)),
                        _ => return false,
                    }
                }
                print.done() == *was && (!*done || now.next().is_none())
            }
        };
        if !same {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, MAIN};

    /// A traced key and prefix read again as they were, then after each change to what they read.
    #[test]
    fn a_trace_reads_the_same_until_what_it_read_changes() {
        let db = Db::new();
        let sql = |q: &str| crate::sql::run(&db, MAIN, q, &[]).unwrap();
        sql("create table t (id int primary key, v text)");
        sql("insert into t values (1, 'a'), (2, 'b')");
        let schema = format!("{}/t", crate::sql::SCHEMAS);
        let traced = || {
            trace(|| {
                let v = db.view_tx(MAIN).unwrap();
                v.get(&schema).unwrap().expect("t's schema");
                v.prefix("t/").unwrap().count()
            })
            .1
            .expect("kept")
        };
        let reads = traced();
        assert!(matches!(&reads[..], [Read::Key(..), Read::Prefix(_, _, 2, _, true)]), "{reads:?}");
        assert!(same(&reads, &db.core, None));
        // what it didn't read
        sql("create table other (id int primary key)");
        sql("insert into other values (1)");
        assert!(same(&reads, &db.core, None));
        // a value back as it was (written as it was) reads the same
        let reads = traced();
        sql("update t set v = 'z' where id = 1");
        sql("update t set v = 'a' where id = 1");
        assert!(same(&reads, &db.core, None));
        // a row's value, a row more, a row less, the key read
        for change in [
            "update t set v = 'c' where id = 2",
            "insert into t values (3, 'x')",
            "delete from t where id = 3",
            "alter table t add column w int",
        ] {
            let reads = traced();
            sql(change);
            assert!(!same(&reads, &db.core, None), "{change}");
        }
        // nothing is kept of a scan
        let (_, reads) = trace(|| {
            let v = db.view_tx(MAIN).unwrap();
            v.each::<crate::Error>("t/", None, None, |_, _| Ok(true)).unwrap();
        });
        assert!(reads.is_none());
    }
}
