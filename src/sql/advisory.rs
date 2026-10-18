//! Postgres's advisory locks: `pg_advisory_lock(key)` and friends, locks on numbers the
//! application picks, held by a session (until `pg_advisory_unlock` or the session ends) or by
//! its transaction (`_xact_`: until it commits or rolls back, or outside one, until the
//! statement ends), exclusive or (the `_shared` kinds) shared. A lock another session holds in a
//! mode that conflicts (exclusive with either, shared with exclusive) waits, as long as the
//! statement may run (its timeout, a cancel), and a wait that would close a cycle of sessions waiting on each other fails at once (40P01), as Postgres's
//! deadlock check does.
//!
//! A transaction reads the database as it was at BEGIN (see `crate::Core`), so after waiting
//! for a lock another transaction held, it wouldn't see what that one committed: the lock would
//! guard nothing. So when a lock was released after the transaction began, the transaction
//! reads the database as it is now from its next statement on (`SessState::refresh`), as
//! Postgres's READ COMMITTED would; one that already wrote can't, and fails with 40001.

use super::{Result, SqlError, Value, err};
use std::collections::HashMap;
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::Duration;

/// A lock's name: the database (its core's address) and the key, one bigint or two integers
/// (Postgres keeps those apart).
pub(crate) type Key = (usize, i64, bool);

/// A lock as held: its key and whether in share mode (the `_shared` functions' ShareLock; else ExclusiveLock). The
/// two modes of a key are two locks, as in Postgres: a session may hold both.
pub(crate) type Held = (Key, bool);

#[derive(Default)]
struct Lock {
    /// the sessions holding it (one at most in exclusive mode): times each took it for the session, and for its
    /// transaction
    holders: HashMap<u64, (u32, u32)>,
    /// `Core::fork_clock` when it was last released: a transaction forked before that may not
    /// see what its holder committed
    released: u64,
    by: u64, // the session that released it
}

#[derive(Default)]
struct Table {
    locks: HashMap<Held, Lock>,
    waiting: HashMap<u64, Held>, // what each waiting session waits for
}

static TABLE: LazyLock<(Mutex<Table>, Condvar)> = LazyLock::new(Default::default);

/// Released locks kept for their release times before those old enough to matter to no one go.
const KEPT: usize = 10_000;

/// The sessions other than `session` holding a lock that a request for `want` waits for: an exclusive lock's
/// holders, and for an exclusive request the share lock's too.
fn blockers(t: &Table, session: u64, (key, share): Held) -> Vec<u64> {
    let mut out = vec![];
    for mode in [false, true] {
        if mode && share {
            continue;
        }
        if let Some(l) = t.locks.get(&(key, mode)) {
            out.extend(l.holders.keys().copied().filter(|s| *s != session));
        }
    }
    out.sort_unstable();
    out
}

/// The locks held and waited for (pg_locks): the key, whether in share mode, the session, and whether it holds it.
pub(crate) fn listed() -> Vec<(Key, bool, u64, bool)> {
    let t = TABLE.0.lock().unwrap_or_else(|p| p.into_inner());
    let mut out: Vec<_> =
        t.locks.iter().flat_map(|((k, m), l)| l.holders.keys().map(move |s| (*k, *m, *s, true))).collect();
    out.extend(t.waiting.iter().map(|(s, (k, m))| (*k, *m, *s, false)));
    out.sort();
    out
}

/// The key of a call's arguments: `(bigint)` or `(int, int)`.
pub(crate) fn key(db: usize, a: &[Value]) -> Result<Option<Key>> {
    let int = |v: &Value| -> Result<Option<i64>> {
        match super::coerce(v.clone(), super::Type::BigInt)? {
            Value::Int(n) => Ok(Some(n)),
            _ => Ok(None),
        }
    };
    Ok(match a {
        [k] => int(k)?.map(|k| (db, k, false)),
        [x, y] => match (int(x)?, int(y)?) {
            (Some(x), Some(y)) => {
                let half = |n: i64| {
                    i32::try_from(n).map_err(|_| SqlError {
                        code: "22003",
                        message: "integer out of range".into(),
                        info: None,
                    })
                };
                Some((db, ((half(x)? as i64) << 32) | (half(y)? as u32 as i64), true))
            }
            _ => None,
        },
        _ => None,
    })
}

/// Takes `key` for `session` (for its transaction when `xact`; in share mode when `share`): waits while another
/// session holds it in a mode that conflicts, unless `try_only` (then false). Returns whether it's taken, and the
/// time it was last released by someone else (0: never).
pub(crate) fn take(session: u64, key: Key, xact: bool, try_only: bool, share: bool) -> Result<(bool, u64)> {
    let (m, cv) = &*TABLE;
    let mut t = m.lock().unwrap();
    loop {
        let holder = blockers(&t, session, (key, share));
        if holder.is_empty() {
            // (released by someone else: of the locks it would have waited for)
            let released = [false, true]
                .into_iter()
                .filter(|mode| !(share && *mode))
                .filter_map(|mode| t.locks.get(&(key, mode)))
                .filter(|l| l.holders.is_empty() && l.by != session)
                .map(|l| l.released)
                .max()
                .unwrap_or(0);
            let l = t.locks.entry((key, share)).or_default();
            let n = l.holders.entry(session).or_default();
            if xact {
                n.1 += 1;
            } else {
                n.0 += 1;
            }
            t.waiting.remove(&session);
            return Ok((true, released));
        }
        if try_only {
            return Ok((false, 0));
        }
        // a cycle: a holder waits (through others, for advisory locks or sequences) for a lock
        // this session holds
        let mut graph = waits(&t);
        graph.extend(super::counter_waits_now().unwrap_or_default());
        if closes_cycle(session, holder, &graph) {
            t.waiting.remove(&session);
            return err("40P01", "deadlock detected: sessions are waiting for each other's locks");
        }
        t.waiting.insert(session, (key, share));
        t = cv.wait_timeout(t, Duration::from_millis(50)).unwrap().0;
        if let Err(e) = super::stopped() {
            t.waiting.remove(&session);
            return Err(e);
        }
    }
}

/// What each session waiting for an advisory lock waits for: its holders.
fn waits(t: &Table) -> HashMap<u64, Vec<u64>> {
    t.waiting.iter().map(|(s, h)| (*s, blockers(t, *s, *h))).filter(|(_, b)| !b.is_empty()).collect()
}

/// `waits`, for another kind of wait's deadlock check (see `super::restarting_wait`); None while
/// another thread has the table, as that check may hold its own (it looks again soon).
/// What each session waiting for an advisory lock waits for, waiting for the table if another
/// thread has it (pg_blocking_pids, pg_stat_activity: see `waits` in sql.rs).
pub(crate) fn waits_all() -> HashMap<u64, Vec<u64>> {
    waits(&TABLE.0.lock().unwrap_or_else(|p| p.into_inner()))
}

pub(crate) fn waits_now() -> Option<HashMap<u64, Vec<u64>>> {
    match TABLE.0.try_lock() {
        Ok(t) => Some(waits(&t)),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(waits(&p.into_inner())),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// Whether `session`, waiting for those in `at`, closes a cycle of sessions waiting for each
/// other: `graph` says what each waiting session waits for.
pub(crate) fn closes_cycle(session: u64, mut at: Vec<u64>, graph: &HashMap<u64, Vec<u64>>) -> bool {
    let mut seen = std::collections::HashSet::new();
    while let Some(o) = at.pop() {
        if o == session {
            return true;
        }
        if seen.insert(o) {
            at.extend(graph.get(&o).into_iter().flatten().copied());
        }
    }
    false
}

/// Lets go of `key` once, as `session`'s session lock (in share mode when `share`); false if it didn't hold it so
/// (Postgres warns, and says false).
pub(crate) fn release(session: u64, key: Key, now: u64, share: bool) -> bool {
    let (m, cv) = &*TABLE;
    let mut t = m.lock().unwrap();
    let Some(l) = t.locks.get_mut(&(key, share)) else { return false };
    let Some(n) = l.holders.get_mut(&session).filter(|n| n.0 > 0) else { return false };
    n.0 -= 1;
    if *n == (0, 0) {
        l.holders.remove(&session);
        (l.released, l.by) = (now, session);
        cv.notify_all();
    }
    true
}

/// Undoes one `take` of `held` (a lock taken that the transaction can't use after all).
pub(crate) fn untake(session: u64, held: Held, xact: bool) {
    let (m, cv) = &*TABLE;
    let mut t = m.lock().unwrap();
    if let Some(l) = t.locks.get_mut(&held)
        && let Some(n) = l.holders.get_mut(&session)
    {
        if xact {
            n.1 = n.1.saturating_sub(1);
        } else {
            n.0 = n.0.saturating_sub(1);
        }
        if *n == (0, 0) {
            l.holders.remove(&session); // as it was: its release time stands
            cv.notify_all();
        }
    }
}

/// Lets go of every lock `session` holds for its transaction (`xact`) or all it holds.
pub(crate) fn release_all(session: u64, xact: bool, now: u64, oldest: impl FnOnce() -> Option<u64>) {
    let_go(session, true, !xact, now, oldest);
}

/// pg_advisory_unlock_all: every lock `session` holds for the session; those of its transaction stay, as in Postgres.
pub(crate) fn release_session_locks(session: u64, now: u64, oldest: impl FnOnce() -> Option<u64>) {
    let_go(session, false, true, now, oldest);
}

/// Lets go of `session`'s transaction locks (`xact`) and its session locks (`sessions`).
fn let_go(session: u64, xact: bool, sessions: bool, now: u64, oldest: impl FnOnce() -> Option<u64>) {
    let (m, cv) = &*TABLE;
    let mut t = m.lock().unwrap();
    let mut freed = false;
    for l in t.locks.values_mut() {
        let Some(n) = l.holders.get_mut(&session) else { continue };
        if xact {
            n.1 = 0;
        }
        if sessions {
            n.0 = 0;
        }
        if *n == (0, 0) {
            l.holders.remove(&session);
            (l.released, l.by) = (now, session);
            freed = true;
        }
    }
    if freed {
        cv.notify_all();
    }
    if t.locks.len() > KEPT {
        // ponytail: a walk of the locks each time past KEPT; keep a heap of release times if it shows
        let oldest = oldest();
        t.locks.retain(|_, l| !l.holders.is_empty() || oldest.is_some_and(|o| l.released > o));
    }
}

/// Postgres's `hashtext`: its `hash_bytes` (Bob Jenkins's lookup3, as Postgres has it) of the
/// text's bytes, as an int4.
pub(crate) fn hashtext(s: &str) -> i32 {
    let k = s.as_bytes();
    let rot = |x: u32, n: u32| x.rotate_left(n);
    let (mut a, mut b, mut c) = {
        let v = 0x9e37_79b9u32.wrapping_add(k.len() as u32).wrapping_add(3_923_095);
        (v, v, v)
    };
    macro_rules! mix {
        () => {
            a = a.wrapping_sub(c);
            a ^= rot(c, 4);
            c = c.wrapping_add(b);
            b = b.wrapping_sub(a);
            b ^= rot(a, 6);
            a = a.wrapping_add(c);
            c = c.wrapping_sub(b);
            c ^= rot(b, 8);
            b = b.wrapping_add(a);
            a = a.wrapping_sub(c);
            a ^= rot(c, 16);
            c = c.wrapping_add(b);
            b = b.wrapping_sub(a);
            b ^= rot(a, 19);
            a = a.wrapping_add(c);
            c = c.wrapping_sub(b);
            c ^= rot(b, 4);
            b = b.wrapping_add(a);
        };
    }
    let word = |w: &[u8]| w.iter().enumerate().fold(0u32, |acc, (i, &x)| acc | (x as u32) << (8 * i));
    let mut rest = k;
    while rest.len() >= 12 {
        a = a.wrapping_add(word(&rest[0..4]));
        b = b.wrapping_add(word(&rest[4..8]));
        c = c.wrapping_add(word(&rest[8..12]));
        mix!();
        rest = &rest[12..];
    }
    // the last bytes; c's lowest byte is left for the length, as lookup3 does
    let n = rest.len();
    if n > 8 {
        c = c.wrapping_add(word(&rest[8..]) << 8);
    }
    if n > 4 {
        b = b.wrapping_add(word(&rest[4..n.min(8)]));
    }
    if n > 0 {
        a = a.wrapping_add(word(&rest[..n.min(4)]));
    }
    // final
    c ^= b;
    c = c.wrapping_sub(rot(b, 14));
    a ^= c;
    a = a.wrapping_sub(rot(c, 11));
    b ^= a;
    b = b.wrapping_sub(rot(a, 25));
    c ^= b;
    c = c.wrapping_sub(rot(b, 16));
    a ^= c;
    a = a.wrapping_sub(rot(c, 4));
    b ^= a;
    b = b.wrapping_sub(rot(a, 14));
    c ^= b;
    c = c.wrapping_sub(rot(b, 24));
    c as i32
}
