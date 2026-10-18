//! Stage 1 durability: one write-ahead log plus checkpoint snapshots, in a database folder.
//!
//! Folder layout:
//!   LOCK                   held (flock) while a process has the database open
//!   manifest               last checkpoint: every branch's tree roots, CRC-protected, replaced atomically
//!   pages/                 the trees themselves (see `pages` and `prolly`)
//!   wal-<16 digit seq>     log segments; replayed in order on open, starting at the snapshot's segment
//!   history/<seq>-<ms>     earlier checkpoints' manifests, kept with their log segments for the
//!                          retention window (`retention`, ms; default 30 days): the past states
//!                          `Core::past` rebuilds for time travel
//!
//! Log frame: [u32 payload len][u32 crc32 of payload][payload]. A bad frame in the last segment, with
//! no whole record after it, is a torn tail from a crash: recovery stops there and truncates. A bad
//! frame anywhere else is corruption and refuses to open, because silently dropping data is worse
//! than not starting.

use crate::Resolve;
use crate::crypt::Crypt;
use crate::pages::Hash;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::{Mutex, MutexGuard};
use std::borrow::Cow;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Once the live log segment passes this, the next write checkpoints and starts a new segment.
pub(crate) const CHECKPOINT_BYTES: u64 = 64 << 20;
const FLUSH_BYTES: usize = 1 << 20;
/// A segment is kept this far ahead of its records in zeros, so a commit's fsync overwrites file
/// blocks in place: one that also grows the file must make its new size durable too, which on
/// ext4 is a journal commit and more than doubled the cost of a commit (Postgres fills its WAL
/// segments with zeros for the same reason).
const PREALLOC: u64 = 4 << 20;
/// An open world's writes wait to be logged together (see `Batch`) until this many bytes of them
/// are waiting; a write this big alone is logged on its own.
pub(crate) const BATCH_BYTES: usize = 64 << 10;
/// A record's first byte for a `Batch`; an `Op`'s is its variant's.
const BATCH: u8 = 17;
const MAN_MAGIC: &[u8] = b"SONOSMAN"; // then the format's version, one digit
const MAN_VERSION: u8 = b'7'; // 6: logs without Op::Bulk; 5: without counters; 4: without deltas; 3: without world IDs and metadata. All still read.

pub(crate) enum Op<S> {
    /// Rows written to one branch atomically; `None` deletes.
    Write(S, Vec<(S, Option<S>)>),
    Fork(S, S),
    /// The expected branch version, when the caller gave one (it is checked again on replay).
    Merge(S, Resolve, Option<u64>),
    Discard(S),
    /// Written last by a clean shutdown: its absence at recovery means a crash.
    Close,
    /// After a crash: this branch may have lost unsynced writes; merging needs a confirmation.
    Flag(S),
    /// A fork with its world's identity: (from, name, id, created in ms since 1970, metadata JSON).
    World(S, S, S, u64, S),
    /// A world's metadata (a JSON object) replaced.
    Meta(S, S),
    /// The time (ms since 1970) from here on: written when it moves on, and first in each segment.
    Clock(u64),
    /// A merge that settled some rows itself: (branch, how for the rest, expected version, and per
    /// row: 0 ours, 1 theirs, 2 this stored value (none deletes)).
    MergeWith(S, Resolve, Option<u64>, Vec<(S, u8, Option<S>)>),
    /// Who does what follows: an agent's name, or empty for the database's own users. Written
    /// when it changes, so the audit trail can say who did each thing.
    Actor(S),
    /// A merge that leaves the branch open: only some of its rows (`tables`, `keys`; empty: all),
    /// or `into` a world other than its parent (empty: the parent). Settled rows as in `MergeWith`.
    MergeSome {
        branch: S,
        into: S,
        how: Resolve,
        expected: Option<u64>,
        tables: Vec<S>,
        keys: Vec<S>,
        rows: Vec<(S, u8, Option<S>)>,
    },
    /// A sequence's counter, which belongs to no world (see `Core::next_values`): (name, the
    /// highest value reserved, where it counts from). Its whole state, so replay just sets it.
    Seq(S, i64, Option<S>),
    /// A statement too big to hold committed: (branch, rows it wrote, the branch's tree now, with
    /// nothing on top, and its version). The tree's pages are on disk before this is written,
    /// first in the segment of the checkpoint that holds it (see `Core::commit_bulk`).
    Bulk(S, u64, Hash, u64),
    /// The segment is on stable storage up to this offset: written after a commit's fsync, so
    /// recovery can tell damage in acknowledged records (corruption) from a crash's torn tail.
    Synced(u64),
    /// A world's fork point moved up to its parent as it was then, its own changes kept on top
    /// (see `Core::rebase`): (branch, how for rows both changed, expected version, settled rows
    /// as in `MergeWith`).
    Rebase(S, Resolve, Option<u64>, Vec<(S, u8, Option<S>)>),
    /// A READ COMMITTED transaction (and the savepoints above it) brought up to what its world has committed
    /// since it last was (see `Core::rebase_tx`): replay does the same on the same states.
    RebaseTx(S),
}

impl<S: AsRef<str>> Op<S> {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut b = vec![];
        match self {
            Op::Write(br, rows) => {
                // sized up front: a big batch would otherwise be copied as the buffer doubles
                b.reserve_exact(
                    9 + br.as_ref().len()
                        + rows
                            .iter()
                            .map(|(k, v)| 9 + k.as_ref().len() + v.as_ref().map_or(0, |v| v.as_ref().len()))
                            .sum::<usize>(),
                );
                b.push(1);
                put_str(&mut b, br.as_ref());
                b.extend((rows.len() as u32).to_le_bytes());
                for (k, v) in rows {
                    put_str(&mut b, k.as_ref());
                    put_opt(&mut b, v.as_ref().map(|v| v.as_ref()));
                }
            }
            Op::Fork(from, name) => {
                b.push(3);
                put_str(&mut b, from.as_ref());
                put_str(&mut b, name.as_ref());
            }
            Op::Merge(br, how, expect) => {
                b.push(4);
                put_str(&mut b, br.as_ref());
                b.push(*how as u8);
                match expect {
                    Some(v) => {
                        b.push(1);
                        b.extend(v.to_le_bytes());
                    }
                    None => b.push(0),
                }
            }
            Op::Discard(br) => {
                b.push(5);
                put_str(&mut b, br.as_ref());
            }
            Op::Close => b.push(6),
            Op::Flag(br) => {
                b.push(7);
                put_str(&mut b, br.as_ref());
            }
            Op::World(from, name, id, created, meta) => {
                b.push(8);
                for s in [from, name, id] {
                    put_str(&mut b, s.as_ref());
                }
                b.extend(created.to_le_bytes());
                put_str(&mut b, meta.as_ref());
            }
            Op::Meta(br, meta) => {
                b.push(9);
                put_str(&mut b, br.as_ref());
                put_str(&mut b, meta.as_ref());
            }
            Op::Clock(ms) => {
                b.push(10);
                b.extend(ms.to_le_bytes());
            }
            Op::Actor(name) => {
                b.push(12);
                put_str(&mut b, name.as_ref());
            }
            Op::MergeWith(br, how, expect, rows) | Op::Rebase(br, how, expect, rows) => {
                b.push(if matches!(self, Op::Rebase(..)) { 18 } else { 11 });
                put_str(&mut b, br.as_ref());
                b.push(*how as u8);
                match expect {
                    Some(v) => {
                        b.push(1);
                        b.extend(v.to_le_bytes());
                    }
                    None => b.push(0),
                }
                b.extend((rows.len() as u32).to_le_bytes());
                for (k, kind, v) in rows {
                    put_str(&mut b, k.as_ref());
                    b.push(*kind);
                    put_opt(&mut b, v.as_ref().map(|v| v.as_ref()));
                }
            }
            Op::MergeSome { branch, into, how, expected, tables, keys, rows } => {
                b.push(13);
                put_str(&mut b, branch.as_ref());
                put_str(&mut b, into.as_ref());
                b.push(*how as u8);
                match expected {
                    Some(v) => {
                        b.push(1);
                        b.extend(v.to_le_bytes());
                    }
                    None => b.push(0),
                }
                for names in [tables, keys] {
                    b.extend((names.len() as u32).to_le_bytes());
                    for n in names {
                        put_str(&mut b, n.as_ref());
                    }
                }
                b.extend((rows.len() as u32).to_le_bytes());
                for (k, kind, v) in rows {
                    put_str(&mut b, k.as_ref());
                    b.push(*kind);
                    put_opt(&mut b, v.as_ref().map(|v| v.as_ref()));
                }
            }
            Op::Seq(name, high, origin) => {
                b.push(14);
                put_str(&mut b, name.as_ref());
                b.extend(high.to_le_bytes());
                put_opt(&mut b, origin.as_ref().map(|o| o.as_ref()));
            }
            Op::Synced(off) => {
                b.push(16);
                b.extend(off.to_le_bytes());
            }
            Op::RebaseTx(br) => {
                b.push(19);
                put_str(&mut b, br.as_ref());
            }
            Op::Bulk(br, rows, root, version) => {
                b.push(15);
                put_str(&mut b, br.as_ref());
                b.extend(rows.to_le_bytes());
                b.extend(root);
                b.extend(version.to_le_bytes());
            }
        }
        b
    }
}

/// The ops a record holds: its one op or, for a `Batch`, the Clock and Write each of its writes
/// would have been logged as.
fn decode_all(p: &[u8]) -> Option<Vec<Op<String>>> {
    if p.first() != Some(&BATCH) {
        return decode(p).map(|op| vec![op]);
    }
    let mut r = Reader(&p[1..]);
    let br = r.str()?;
    let mut ops = vec![];
    for _ in 0..r.u32()? {
        ops.push(Op::Clock(r.u64()?));
        let rows = (0..r.u32()?).map(|_| Some((r.str()?, r.opt()?))).collect::<Option<_>>()?;
        ops.push(Op::Write(br.clone(), rows));
    }
    r.0.is_empty().then_some(ops)
}

/// Writes to one open world not in the log yet (see `Core::log_write`): logged later as one
/// record, each write in it as `[u64 ms][u32 rows][rows]` (rows as `Op::Write` has them), so
/// readers get the very ops, times included, that logging each write alone would have given.
#[derive(Default)]
pub(crate) struct Batch {
    branch: String,
    actor: String, // who made them all
    writes: u32,
    last: u64, // when the last one was made (ms)
    body: Vec<u8>,
}

impl Batch {
    pub(crate) fn is_empty(&self) -> bool {
        self.writes == 0
    }

    /// Whether `rec`, the `Op::Write` record of a write this thread makes, may join.
    pub(crate) fn takes(&self, rec: &[u8]) -> bool {
        self.is_empty() || (self.body.len() + rec.len() <= BATCH_BYTES && ACTOR.with(|a| *a.borrow() == self.actor))
    }

    /// Adds a write to `branch` made now by this thread, given as its `Op::Write` record.
    pub(crate) fn push(&mut self, branch: &str, rec: &[u8]) {
        if self.is_empty() {
            (self.branch, self.actor) = (branch.into(), actor());
        }
        self.last = self.last.max(now_ms());
        self.body.extend(self.last.to_le_bytes());
        self.body.extend(&rec[1 + 4 + branch.len()..]); // past its tag and branch: [u32 rows][rows]
        self.writes += 1;
    }
}

fn decode(p: &[u8]) -> Option<Op<String>> {
    let mut r = Reader(p);
    let op = match r.u8()? {
        1 => {
            let br = r.str()?;
            let rows = (0..r.u32()?).map(|_| Some((r.str()?, r.opt()?))).collect::<Option<_>>()?;
            Op::Write(br, rows)
        }
        3 => Op::Fork(r.str()?, r.str()?),
        4 => Op::Merge(
            r.str()?,
            match r.u8()? {
                0 => Resolve::Fail,
                1 => Resolve::Ours,
                2 => Resolve::Theirs,
                _ => return None,
            },
            match r.u8()? {
                0 => None,
                1 => Some(r.u64()?),
                _ => return None,
            },
        ),
        5 => Op::Discard(r.str()?),
        6 => Op::Close,
        7 => Op::Flag(r.str()?),
        8 => Op::World(r.str()?, r.str()?, r.str()?, r.u64()?, r.str()?),
        9 => Op::Meta(r.str()?, r.str()?),
        10 => Op::Clock(r.u64()?),
        12 => Op::Actor(r.str()?),
        11 | 18 => {
            let rebase = p[0] == 18;
            let br = r.str()?;
            let how = match r.u8()? {
                0 => Resolve::Fail,
                1 => Resolve::Ours,
                2 => Resolve::Theirs,
                _ => return None,
            };
            let expect = match r.u8()? {
                0 => None,
                1 => Some(r.u64()?),
                _ => return None,
            };
            let rows = (0..r.u32()?).map(|_| Some((r.str()?, r.u8()?, r.opt()?))).collect::<Option<_>>()?;
            match rebase {
                true => Op::Rebase(br, how, expect, rows),
                false => Op::MergeWith(br, how, expect, rows),
            }
        }
        13 => {
            let (branch, into) = (r.str()?, r.str()?);
            let how = match r.u8()? {
                0 => Resolve::Fail,
                1 => Resolve::Ours,
                2 => Resolve::Theirs,
                _ => return None,
            };
            let expected = match r.u8()? {
                0 => None,
                1 => Some(r.u64()?),
                _ => return None,
            };
            let tables = (0..r.u32()?).map(|_| r.str()).collect::<Option<_>>()?;
            let keys = (0..r.u32()?).map(|_| r.str()).collect::<Option<_>>()?;
            let rows = (0..r.u32()?).map(|_| Some((r.str()?, r.u8()?, r.opt()?))).collect::<Option<_>>()?;
            Op::MergeSome { branch, into, how, expected, tables, keys, rows }
        }
        14 => Op::Seq(r.str()?, r.u64()? as i64, r.opt()?),
        15 => Op::Bulk(r.str()?, r.u64()?, r.take(32)?.try_into().ok()?, r.u64()?),
        16 => Op::Synced(r.u64()?),
        19 => Op::RebaseTx(r.str()?),
        _ => return None,
    };
    r.0.is_empty().then_some(op)
}

pub(crate) struct Log {
    pub(crate) dir: PathBuf,
    pub(crate) inner: Mutex<Inner>, // held only to append a record
    synced: Mutex<u64>,             // log position known durable (group commit)
    mode: AtomicU64,                // SyncCommit as u64
    seg_bytes: AtomicU64,           // the live segment's length: where its next record goes
    /// Log bytes since the cut of the last checkpoint that got its manifest out: a checkpoint that
    /// fails still starts a new segment, and must not look like it caught up.
    unpublished: AtomicU64,
    /// After a background checkpoint failed: when to try again (see `retry_due`).
    retry_at: Mutex<Option<std::time::Instant>>,
    pub(crate) ckpt: Mutex<()>,
    pub(crate) crypt: Option<Arc<Crypt>>,
    /// Rollback detection, when asked for (see `check_anchor`).
    pub(crate) anchor: Option<Anchor>,
    /// Set once the open succeeded: only then does dropping the log close it. A failed open must
    /// leave the log as it found it (a Close would mark a crash clean, and move the anchor on).
    pub(crate) live: std::sync::atomic::AtomicBool,
    lock: File,
}

pub(crate) struct Inner {
    file: File,
    pub(crate) seg: u64,
    pending: Vec<u8>,
    end: u64,      // bytes ever appended by this process: the position `sync` waits for
    clock: u64,    // the last Clock written (ms)
    actor: String, // the last Actor written
    off: u64,      // where the segment's records end in the file
    alloc: u64,    // where the zeros written ahead of them end (see PREALLOC)
}

impl Inner {
    fn flush(&mut self) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        if self.pending.is_empty() {
            return Ok(());
        }
        self.file.write_all_at(&self.pending, self.off)?;
        self.off += self.pending.len() as u64;
        self.pending.clear();
        self.ahead()
    }

    /// Writes zeros ahead of the records once they pass the last ones (see PREALLOC).
    fn ahead(&mut self) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        if self.off > self.alloc {
            let to = (self.off / PREALLOC + 1) * PREALLOC;
            self.file.write_all_at(&vec![0; (to - self.off) as usize], self.off)?;
            self.alloc = to;
        }
        Ok(())
    }

    /// Writes a frame straight to the file after what's buffered: a big record isn't copied
    /// into the buffer (which would also keep that much memory from then on).
    fn write_frame(&mut self, head: &[u8], payload: &[u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.flush()?;
        self.file.write_all_at(head, self.off)?;
        self.file.write_all_at(payload, self.off + head.len() as u64)?;
        self.off += (head.len() + payload.len()) as u64;
        self.ahead()
    }

    /// Writes buffered records and drops the zeros after them: a segment that's done ends where
    /// its records do (only a crash leaves zeros, and recovery cuts them off with the torn tail).
    fn trim(&mut self) -> io::Result<()> {
        self.flush()?;
        if self.alloc > self.off {
            self.file.set_len(self.off)?;
            self.alloc = self.off;
        }
        Ok(())
    }
}

/// Opens segment `seg` to add records after those it has.
fn open_seg(dir: &Path, seg: u64) -> io::Result<(File, u64)> {
    let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(seg_path(dir, seg))?;
    let len = file.metadata()?.len();
    Ok((file, len))
}

thread_local! {
    /// The agent this thread acts for (empty: the database's own users); see `acting_as`.
    static ACTOR: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Makes this thread act for agent `name` until the guard is dropped: what it writes is logged as
/// the agent's (see `Op::Actor`).
pub fn acting_as(name: &str) -> ActingAs {
    let before = ACTOR.with(|a| std::mem::replace(&mut *a.borrow_mut(), name.to_string()));
    ActingAs(before)
}

pub struct ActingAs(String);

/// The agent this thread acts for, or empty.
pub(crate) fn actor() -> String {
    ACTOR.with(|a| a.borrow().clone())
}

impl Drop for ActingAs {
    fn drop(&mut self) {
        ACTOR.with(|a| *a.borrow_mut() = std::mem::take(&mut self.0));
    }
}

/// For crash tests (tests/crash.rs), in debug builds: with CHRONOS_CRASH_AT=n, the process aborts
/// at the n-th crash point it passes, leaving its files as a kill -9 at that step would. Crash
/// points sit between the file steps of checkpoints and cleanup.
pub(crate) fn crash_point() {
    #[cfg(debug_assertions)]
    {
        static AT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        static PASSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let at = *AT.get_or_init(|| std::env::var("CHRONOS_CRASH_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
        if at > 0 && PASSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 == at {
            std::process::abort();
        }
    }
}

/// For tests (tests/disk_full.rs), in debug builds: with CHRONOS_FAIL_PAGE_WRITES=n,k the n-th
/// page record written and the k-1 after it fail part way through, as on a full disk.
pub(crate) fn page_write_fails() -> bool {
    #[cfg(debug_assertions)]
    {
        static AT: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (at, k) = *AT.get_or_init(|| {
            let v = std::env::var("CHRONOS_FAIL_PAGE_WRITES").unwrap_or_default();
            let mut n = v.split(',').map(|x| x.trim().parse::<u64>().unwrap_or(0));
            (n.next().unwrap_or(0), n.next().unwrap_or(1))
        });
        if at > 0 {
            let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            return n >= at && n < at + k;
        }
    }
    false
}

/// For crash tests, in debug builds: sleeps for the milliseconds the environment variable `var`
/// says, so a test can hold one thread at a step while another passes it.
pub(crate) fn stall_for_tests(var: &str) {
    #[cfg(debug_assertions)]
    {
        if let Some(ms) = std::env::var(var).ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = var;
}

// An fsync or write that fails leaves the on-disk state unknown; like Postgres, the only safe
// response is to stop and let recovery replay the log on the next open. The whole process: a
// panic would end one thread, poison its locks and leave the rest serving state that isn't durable
// (and a later fsync can report success for pages the failed one dropped).
pub(crate) fn fatal(e: io::Error) -> ! {
    eprintln!("chronos: a write or fsync failed ({e}); stopping so the next open recovers from the log");
    std::process::abort()
}

impl Log {
    fn open(dir: &Path, seg: u64, lock: File, crypt: Option<Arc<Crypt>>) -> io::Result<Log> {
        let (file, len) = open_seg(dir, seg)?;
        sync_dir(dir)?;
        Ok(Log {
            dir: dir.into(),
            inner: Mutex::new(Inner {
                file,
                seg,
                pending: vec![],
                end: 0,
                clock: 0,
                actor: String::new(),
                off: len,
                alloc: len,
            }),
            synced: Mutex::new(0),
            mode: AtomicU64::new(sync_commit(dir) as u64),
            seg_bytes: AtomicU64::new(len),
            unpublished: AtomicU64::new(len),
            retry_at: Mutex::new(None),
            ckpt: Mutex::new(()),
            crypt,
            anchor: None,
            live: Default::default(),
            lock,
        })
    }

    /// Frames and buffers one record; returns the position to pass to `sync` for durability.
    /// A Clock record goes first when the time has moved on, so replay can stop at a moment.
    pub(crate) fn append(&self, g: &mut Inner, payload: &[u8]) -> u64 {
        let now = now_ms();
        if now > g.clock {
            g.clock = now;
            self.frame(g, &Op::<&str>::Clock(now).encode());
        }
        ACTOR.with(|a| {
            let a = a.borrow();
            if *a != g.actor {
                g.actor.clone_from(&a);
                self.frame(g, &Op::Actor(a.as_str()).encode());
            }
        });
        self.frame(g, payload)
    }

    /// Logs `w`'s writes as one record, as the agent that made them, and empties it. Their own
    /// times go with them, so the clock steps back to them if the log has passed them: the next
    /// record's Clock moves it on again.
    pub(crate) fn append_batch(&self, g: &mut Inner, w: &mut Batch) -> u64 {
        if w.actor != g.actor {
            g.actor.clone_from(&w.actor);
            self.frame(g, &Op::Actor(w.actor.as_str()).encode());
        }
        let mut rec = Vec::with_capacity(9 + w.branch.len() + w.body.len());
        rec.push(BATCH);
        put_str(&mut rec, &w.branch);
        rec.extend(w.writes.to_le_bytes());
        rec.extend(&w.body);
        g.clock = w.last;
        *w = Batch::default();
        self.frame(g, &rec)
    }

    fn frame(&self, g: &mut Inner, payload: &[u8]) -> u64 {
        let n = if payload.len() < FLUSH_BYTES {
            let framed = framed(self.crypt.as_deref(), g.seg, self.seg_bytes.load(Ordering::Relaxed), payload);
            g.pending.extend(&framed);
            framed.len()
        } else {
            let p = match self.crypt.as_deref() {
                Some(c) => Cow::Owned(c.seal(&place(g.seg, self.seg_bytes.load(Ordering::Relaxed)), payload)),
                None => Cow::Borrowed(payload),
            };
            let mut head = (p.len() as u32).to_le_bytes().to_vec();
            head.extend(crc32fast::hash(&p).to_le_bytes());
            g.write_frame(&head, &p).unwrap_or_else(|e| fatal(e));
            head.len() + p.len()
        };
        g.end += n as u64;
        self.seg_bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.unpublished.fetch_add(n as u64, Ordering::Relaxed);
        if g.pending.len() >= FLUSH_BYTES {
            g.flush().unwrap_or_else(|e| fatal(e));
        }
        g.end
    }

    /// Blocks until everything up to `upto` is as durable as the `SyncCommit` setting asks: on
    /// stable storage (`Full`), handed to the drive (`Normal`) or to the OS (`Off`). Concurrent
    /// callers share one fsync, and the fsync runs outside `inner`, so writers on other branches
    /// never wait on disk.
    pub(crate) fn sync(&self, upto: u64) {
        let mut synced = self.synced.lock().unwrap();
        if *synced >= upto {
            return;
        }
        let (file, end, seg, off) = {
            let mut g = self.inner.lock().unwrap();
            g.flush().unwrap_or_else(|e| fatal(e));
            (g.file.try_clone().unwrap_or_else(|e| fatal(e)), g.end, g.seg, g.off)
        };
        match self.sync_commit() {
            SyncCommit::Full => file.sync_data().unwrap_or_else(|e| fatal(e)),
            SyncCommit::Normal => plain_fsync(&file).unwrap_or_else(|e| fatal(e)),
            // not durable yet, so `synced` stays put: the next Full or Normal sync covers it
            SyncCommit::Off => return,
        }
        *synced = end;
        // buffered, so it costs no write of its own: the next flush takes it
        let mut g = self.inner.lock().unwrap();
        if g.seg == seg {
            self.frame(&mut g, &Op::<&str>::Synced(off).encode());
        }
    }

    pub(crate) fn sync_commit(&self) -> SyncCommit {
        SyncCommit::from_u64(self.mode.load(Ordering::Relaxed))
    }

    pub(crate) fn set_sync_commit(&self, m: SyncCommit) -> io::Result<()> {
        set_sync_commit(&self.dir, m)?;
        self.mode.store(m as u64, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn needs_checkpoint(&self) -> bool {
        self.unpublished.load(Ordering::Relaxed) > CHECKPOINT_BYTES
    }

    /// Log bytes since the last checkpoint that got its manifest out.
    pub(crate) fn since_checkpoint(&self) -> u64 {
        self.unpublished.load(Ordering::Relaxed)
    }

    /// The live segment's length, its records only (not the zeros kept ahead of them).
    pub(crate) fn seg_len(&self) -> u64 {
        self.seg_bytes.load(Ordering::Relaxed)
    }

    /// So far past the checkpoint point that writers should wait for one.
    pub(crate) fn far_behind(&self) -> bool {
        self.unpublished.load(Ordering::Relaxed) > 4 * CHECKPOINT_BYTES
    }

    /// So far past the checkpoint point (checkpoints keep failing) that writers must wait until
    /// one succeeds, rather than grow the log without limit.
    pub(crate) fn over_ceiling(&self) -> bool {
        self.unpublished.load(Ordering::Relaxed) > 16 * CHECKPOINT_BYTES
    }

    /// A checkpoint whose cut came `cut` bytes of log after the last one's (from `rotate`) is
    /// published: only what came after its cut is left to checkpoint.
    pub(crate) fn published(&self, cut: u64) {
        self.unpublished.fetch_sub(cut, Ordering::Relaxed);
        *self.retry_at.lock().unwrap() = None;
    }

    /// Whether a checkpoint may be tried now: not while waiting out one that failed (each try
    /// starts a log segment, and the cause is rarely gone in less time).
    pub(crate) fn retry_due(&self) -> bool {
        self.retry_at.lock().unwrap().is_none_or(|t| std::time::Instant::now() >= t)
    }

    /// A checkpoint failed: say so, and hold off the next try for a while.
    pub(crate) fn failed(&self, e: &dyn std::fmt::Display) {
        eprintln!("chronos: checkpoint failed ({e}); the log grows until one succeeds; trying again in 10 s");
        *self.retry_at.lock().unwrap() = Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
    }

    /// Makes the current segment durable and starts the next one. Called at a checkpoint's cut.
    /// Returns the time the new segment starts at (ms), and the log bytes before the cut not yet
    /// checkpointed (for `published`).
    pub(crate) fn rotate(&self, g: &mut Inner) -> io::Result<(u64, u64)> {
        g.trim().and_then(|()| g.file.sync_data()).unwrap_or_else(|e| fatal(e));
        crash_point();
        // the segment number moves on only with the file: records are sealed to their segment
        let (file, len) = open_seg(&self.dir, g.seg + 1)?;
        (g.file, g.seg, g.off, g.alloc) = (file, g.seg + 1, len, len);
        sync_dir(&self.dir).unwrap_or_else(|e| fatal(e));
        crash_point();
        self.seg_bytes.store(0, Ordering::Relaxed);
        let cut = self.unpublished.load(Ordering::Relaxed); // `g` held: no record in between
        g.clock = g.clock.max(now_ms());
        self.frame(g, &Op::<&str>::Clock(g.clock).encode()); // when this segment starts
        g.actor.clear(); // and who: nobody, until an Actor record says (readers may start here)
        Ok((g.clock, cut))
    }

    /// Writes buffered records to the file (not to stable storage): for readers of the log.
    pub(crate) fn flush(&self) {
        self.inner.lock().unwrap().flush().unwrap_or_else(|e| fatal(e));
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    /// Ends the log as a clean shutdown does (a Close record, on disk), for a process exiting
    /// without dropping it. Nothing may be appended after.
    pub(crate) fn close(&self) -> io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let close = close_frame(self.crypt.as_deref(), g.seg, self.seg_bytes.load(Ordering::Relaxed));
        g.pending.extend(close);
        g.trim()?;
        g.file.sync_data()?;
        self.move_anchor(g.seg, g.off)
    }

    /// Records in the anchor, if there is one, that the log is durable up to `off` in `seg`.
    pub(crate) fn move_anchor(&self, seg: u64, off: u64) -> io::Result<()> {
        match &self.anchor {
            Some(a) => a.write(self.crypt.as_deref(), seg, off),
            None => Ok(()),
        }
    }

    /// Writes buffered records to the file and returns the live segment and its length: every
    /// record so far ends at or before it.
    pub(crate) fn cut(&self) -> io::Result<(u64, u64)> {
        let mut g = self.inner.lock().unwrap();
        g.flush()?;
        Ok((g.seg, g.off))
    }
}

/// A record as it goes in the log at `off` in segment `seg`: [u32 len][u32 crc][payload], the
/// payload encrypted (and bound to its place) when the database is.
fn framed(c: Option<&Crypt>, seg: u64, off: u64, payload: &[u8]) -> Vec<u8> {
    let p = match c {
        Some(c) => Cow::Owned(c.seal(&place(seg, off), payload)),
        None => Cow::Borrowed(payload),
    };
    let mut b = (p.len() as u32).to_le_bytes().to_vec();
    b.extend(crc32fast::hash(&p).to_le_bytes());
    b.extend(p.iter());
    b
}

fn place(seg: u64, off: u64) -> [u8; 16] {
    let mut b = [0; 16];
    b[..8].copy_from_slice(&seg.to_le_bytes());
    b[8..].copy_from_slice(&off.to_le_bytes());
    b
}

/// The record at `off` in segment `seg`'s bytes: its payload (decrypted) and where the next one
/// starts. None: no whole frame there (a torn tail, the zeros written ahead of the records, or
/// corruption). Err: whole, but it doesn't decrypt (tampered with, or another key's).
fn frame_at<'a>(buf: &'a [u8], seg: u64, off: usize, c: Option<&Crypt>) -> Option<io::Result<(Cow<'a, [u8]>, usize)>> {
    let h = buf.get(off..off + 8)?;
    let len = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
    if len == 0 {
        return None; // no record is empty; zeros are space written ahead (the crc of nothing is 0)
    }
    let crc = u32::from_le_bytes(h[4..].try_into().unwrap());
    let p = buf.get(off + 8..off + 8 + len).filter(|p| crc32fast::hash(p) == crc)?;
    let next = off + 8 + len;
    Some(match c {
        Some(c) => c.open(&place(seg, off as u64), p, "a log record").map(|p| (Cow::Owned(p), next)),
        None => Ok((Cow::Borrowed(p), next)),
    })
}

/// A Close record, framed at the end of segment `seg` (`off` bytes long): the log of a clean
/// shutdown ends with one.
fn close_frame(c: Option<&Crypt>, seg: u64, off: u64) -> Vec<u8> {
    framed(c, seg, off, &Op::<&str>::Close.encode())
}

impl Drop for Log {
    // Clean shutdown keeps unmerged fork work too; only a crash can lose it.
    fn drop(&mut self) {
        if self.live.load(Ordering::Relaxed)
            && let Ok(mut g) = self.inner.lock()
        {
            let close = close_frame(self.crypt.as_deref(), g.seg, self.seg_bytes.load(Ordering::Relaxed));
            g.pending.extend(close);
            if g.trim().and_then(|_| g.file.sync_data()).is_ok() {
                let _ = self.move_anchor(g.seg, g.off);
            }
        }
        // Unlocked here, not left to closing the file: a child process forked meanwhile (by any
        // thread spawning a program) holds a copy of this file until it execs, and a flock lasts
        // while any copy is open, so the folder would stay locked a moment after it's closed.
        let _ = self.lock.unlock();
    }
}

/// A branch in the manifest: its fork point and current state, each a prolly-tree root.
pub(crate) struct ManBranch {
    pub name: String,
    pub parent: Option<String>,
    pub born: u64,
    pub base: Hash,
    pub data: Hash,
    pub version: u64,
    pub flagged: bool,
    pub id: String,   // the world's ID; empty from a version 3 manifest (the loader makes one)
    pub created: u64, // ms since 1970; 0 if unknown
    pub meta: String, // JSON object
    /// Rows changed on top of the `data` tree: a small branch keeps its changes here rather than
    /// in new pages of its own (then `data` is its fork point's tree).
    pub delta: Vec<(String, Option<String>)>,
}

/// Sequence counters, which belong to no world (see `Op::Seq`): name, highest reserved, origin.
pub(crate) type Counters = Vec<(String, i64, Option<String>)>;

/// Creates the folder if needed and takes the one-process lock. Take it before touching any file.
pub(crate) fn lock(dir: &Path) -> io::Result<File> {
    fs::create_dir_all(dir)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("LOCK"))?;
    lock.try_lock().map_err(|_| {
        io::Error::other(format!("{} is already open in another process; close it first", dir.display()))
    })?;
    Ok(lock)
}

/// Loads the manifest, replays the log after it and opens the log for appends. Returns the log
/// and whether the last shutdown was clean (the log ends with a Close record, or the database is
/// new). `apply` replays one logged op; it must not log (the caller has no log attached yet).
pub(crate) fn recover(
    dir: &Path,
    lock: File,
    crypt: Option<Arc<Crypt>>,
    mut load: impl FnMut(Vec<ManBranch>, Option<Counters>),
    mut apply: impl FnMut(Op<String>),
) -> io::Result<(Log, bool)> {
    // with no log of its own to judge by, a folder is clean if it's new or its manifest was closed
    let (first, clean_if_no_log) = match read_manifest(&dir.join("manifest"), crypt.as_deref())? {
        Some((first, closed, branches, counters)) => {
            load(branches, counters);
            (first, closed)
        }
        None => (1, true),
    };
    let mut last_close = None;
    let mut apply = |op: Op<String>| {
        last_close = Some(matches!(op, Op::Close));
        apply(op)
    };
    let segs = segments(dir)?;
    // older segments are history (see `prune`), or leftovers it will clear at the next checkpoint
    let live: Vec<u64> = segs.iter().copied().filter(|&s| s >= first).collect();
    // a checkpoint's segment is on disk before its manifest, and kept while that manifest is the
    // latest: without it (or with a gap after it) this is an older manifest, or log is missing
    if !segs.is_empty() && (live.is_empty() || live.iter().zip(first..).any(|(&s, want)| s != want)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: the manifest starts the log at segment {first}, which is missing or has a gap after it (an older manifest, or lost log); refusing to guess",
                dir.display()
            ),
        ));
    }
    for (i, &s) in live.iter().enumerate() {
        replay(&seg_path(dir, s), s, i + 1 == live.len(), crypt.as_deref(), &mut apply)?;
    }
    let clean = last_close.unwrap_or(clean_if_no_log);
    let log = Log::open(dir, live.last().copied().unwrap_or(first), lock, crypt)?;
    // every segment since the manifest waits for a checkpoint (checkpoints that failed left several)
    let mut behind = 0;
    for &s in &live {
        behind += fs::metadata(seg_path(dir, s))?.len();
    }
    log.unpublished.store(behind, Ordering::Relaxed);
    Ok((log, clean))
}

fn replay(path: &Path, seg: u64, last: bool, c: Option<&Crypt>, apply: &mut impl FnMut(Op<String>)) -> io::Result<()> {
    let buf = fs::read(path)?;
    let mut off = 0;
    while off < buf.len() {
        let Some(frame) = frame_at(&buf, seg, off, c) else {
            // a crash's torn tail is the end of what was written: whole records after the damage
            // mean it's corruption inside the log, and truncating would drop them silently
            if !last || synced_after(&buf, seg, off, c) {
                return Err(corrupt(path, off));
            }
            let f = OpenOptions::new().write(true).open(path)?;
            f.set_len(off as u64)?; // torn tail from a crash: everything after it was never synced
            f.sync_data()?;
            return Ok(());
        };
        let (p, next) = frame.map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        for op in decode_all(&p).ok_or_else(|| corrupt(path, off))? {
            match op {
                Op::Synced(_) => {}
                op => apply(op),
            }
        }
        off = next;
    }
    Ok(())
}

/// Whether damage at `off` lies in what a commit's fsync made durable: a Synced record after it
/// says the segment was on stable storage past `off`. Otherwise it's a crash's torn tail, which
/// may leave zeros or torn bytes before whole records (unsynced writes reach the disk in any
/// order), but none of it was acknowledged. Only frames of a Synced record's size are checked, so
/// the scan hashes a few bytes per offset however big the torn records were.
fn synced_after(buf: &[u8], seg: u64, off: usize, c: Option<&Crypt>) -> bool {
    let want = framed(c, seg, 0, &Op::<&str>::Synced(0).encode()).len() - 8;
    (off + 1..buf.len().saturating_sub(8)).any(|at| {
        u32::from_le_bytes(buf[at..at + 4].try_into().unwrap()) as usize == want
            && matches!(frame_at(buf, seg, at, c), Some(Ok((p, _))) if matches!(decode(&p), Some(Op::Synced(x)) if x > off as u64))
    })
}

/// Publishes a checkpoint: every branch's roots, and the log segment replay starts from.
/// The pages it names must already be durable. Replaced atomically; then older log segments go.
/// `closed`: written by a clean shutdown, so nothing happened after it. A folder with no log of its
/// own (fresh from a remote) trusts it as clean; otherwise its branches get the crash flag.
pub(crate) fn write_manifest(
    dir: &Path,
    c: Option<&Crypt>,
    first_seg: u64,
    branches: &[ManBranch],
    counters: &Counters,
    closed: bool,
    cut_ms: u64,
) -> io::Result<()> {
    let b = sealed(c, manifest_bytes(first_seg, branches, counters, closed));
    let tmp = dir.join("manifest.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(&b)?;
    f.sync_all()?;
    crash_point();
    fs::rename(&tmp, dir.join("manifest"))?;
    sync_dir(dir)?;
    crash_point();
    // the same, kept as history: the state at `cut_ms`, from which its log segments replay
    keep_history(dir, first_seg, &b, cut_ms)?;
    crash_point();
    prune(dir, first_seg, now_ms().saturating_sub(retention(dir)))
}

fn manifest_bytes(first_seg: u64, branches: &[ManBranch], counters: &Counters, closed: bool) -> Vec<u8> {
    let mut b = MAN_MAGIC.to_vec();
    b.push(MAN_VERSION);
    b.extend(first_seg.to_le_bytes());
    b.push(u8::from(closed));
    b.extend((branches.len() as u32).to_le_bytes());
    for br in branches {
        put_str(&mut b, &br.name);
        put_opt(&mut b, br.parent.as_deref());
        b.extend(br.born.to_le_bytes());
        b.extend(br.base);
        b.extend(br.data);
        b.extend(br.version.to_le_bytes());
        b.push(u8::from(br.flagged));
        put_str(&mut b, &br.id);
        b.extend(br.created.to_le_bytes());
        put_str(&mut b, &br.meta);
        b.extend((br.delta.len() as u32).to_le_bytes());
        for (k, v) in &br.delta {
            put_str(&mut b, k);
            put_opt(&mut b, v.as_deref());
        }
    }
    b.extend((counters.len() as u32).to_le_bytes());
    for (name, high, origin) in counters {
        put_str(&mut b, name);
        b.extend(high.to_le_bytes());
        put_opt(&mut b, origin.as_deref());
    }
    b.extend(crc32fast::hash(&b).to_le_bytes());
    b
}

fn keep_history(dir: &Path, first_seg: u64, bytes: &[u8], cut_ms: u64) -> io::Result<()> {
    let hist = dir.join("history");
    fs::create_dir_all(&hist)?;
    let tmp = hist.join("next.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?; // before the rename: never a history file that names nothing after a power cut
    fs::rename(&tmp, hist.join(format!("{first_seg:016}-{cut_ms}")))?;
    sync_dir(&hist)
}

/// A database's first state (`branches`, before any log), as history from the start of time:
/// time travel before the first checkpoint replays the log from there.
pub(crate) fn start_history(dir: &Path, c: Option<&Crypt>, branches: &[ManBranch]) -> io::Result<()> {
    keep_history(dir, 1, &sealed(c, manifest_bytes(1, branches, &vec![], false)), 0)
}

/// A manifest as stored: encrypted when the database is.
fn sealed(c: Option<&Crypt>, b: Vec<u8>) -> Vec<u8> {
    match c {
        Some(c) => c.seal_file(&b),
        None => b,
    }
}

/// Past checkpoints kept as history, oldest first: (first log segment, time of the cut, file).
pub(crate) fn history(dir: &Path) -> io::Result<Vec<(u64, u64, PathBuf)>> {
    let mut v: Vec<(u64, u64, PathBuf)> = match fs::read_dir(dir.join("history")) {
        Ok(rd) => rd
            .filter_map(|e| {
                let e = e.ok()?;
                let name = e.file_name().into_string().ok()?;
                let (seg, ms) = name.split_once('-')?;
                Some((seg.parse().ok()?, ms.parse().ok()?, e.path()))
            })
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e),
    };
    v.sort();
    Ok(v)
}

/// Drops history no longer needed to rebuild any moment since `since` (ms), and the log segments
/// only it needed. Keeps the newest checkpoint from before `since`: moments just after it start there.
fn prune(dir: &Path, first_seg: u64, since: u64) -> io::Result<()> {
    let hist = history(dir)?;
    let base = hist.iter().rposition(|h| h.1 <= since).unwrap_or(0);
    for (_, _, path) in &hist[..base] {
        fs::remove_file(path)?;
        crash_point();
    }
    let keep_from = hist.get(base).map_or(first_seg, |h| h.0).min(first_seg);
    for s in segments(dir)?.into_iter().filter(|&s| s < keep_from) {
        fs::remove_file(seg_path(dir, s))?;
        crash_point();
    }
    Ok(())
}

/// How long history is kept, in ms (`retention` in the folder; 30 days if unset, which is what a folder made before
/// `NEW_RETENTION_MS` was written keeps: every change an agent makes can be undone that long, see `Core::undo_agent`).
pub(crate) fn retention(dir: &Path) -> u64 {
    fs::read_to_string(dir.join("retention")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(30 * DAY_MS)
}

pub(crate) fn set_retention(dir: &Path, ms: u64) -> io::Result<()> {
    write_file(&dir.join("retention"), ms.to_string().as_bytes())
}

const DAY_MS: u64 = 24 * 3600 * 1000;

/// The retention a new database is made with: one hour. History holds old pages and log for the whole window, so a
/// busy database's disk grows with it (soaks grew 10 to 29 GB in 91 minutes for 2.2 GB of live data at 30 days, and
/// 5 to 35 GB in 77 minutes for 8.3 GB at one day);
/// `alter system set history_retention` raises it. Folders made before keep the 30 days they had (no `retention`
/// file), so an upgrade never drops history, undo or the audit trail anyone relied on.
pub(crate) const NEW_RETENTION_MS: u64 = HOUR_MS;

const HOUR_MS: u64 = 3600 * 1000;

/// Database-wide settings (`settings` in the folder: a JSON object of name to number; see
/// `Setting`). Missing ones are 0; a file that isn't such an object is an error, not no settings
/// (that would quietly drop them all, and the next change would write the loss back).
pub(crate) fn settings(dir: &Path) -> io::Result<serde_json::Map<String, serde_json::Value>> {
    let path = dir.join("settings");
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{} is corrupt ({e}); fix or remove it", path.display()))
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e),
    }
}

/// Writes every entry of `m`, numbers or not (entries this Chronos doesn't know stay as they were).
pub(crate) fn set_settings(dir: &Path, m: serde_json::Map<String, serde_json::Value>) -> io::Result<()> {
    write_file(&dir.join("settings"), serde_json::Value::Object(m).to_string().as_bytes())
}

/// How far a commit to `main` pushes the log before it returns (`sync` in the folder; `full` if
/// unset). Only commits differ: checkpoints, segment rotation and close always sync fully.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncCommit {
    /// Through the drive's own cache to stable storage (`F_FULLFSYNC` on macOS, `fdatasync`
    /// elsewhere). Survives power loss.
    Full = 0,
    /// A plain `fsync`: on macOS the drive may still hold it in its cache, which is what Postgres
    /// does there by default. Elsewhere the same as `Full`.
    Normal = 1,
    /// Handed to the OS only. Survives the process crashing, not the machine.
    Off = 2,
}

impl SyncCommit {
    fn from_u64(v: u64) -> SyncCommit {
        match v {
            1 => SyncCommit::Normal,
            2 => SyncCommit::Off,
            _ => SyncCommit::Full,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SyncCommit::Full => "full",
            SyncCommit::Normal => "normal",
            SyncCommit::Off => "off",
        }
    }

    /// `full`/`on`, `normal`, or `off` (any case).
    pub fn parse(s: &str) -> Option<SyncCommit> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" | "on" => Some(SyncCommit::Full),
            "normal" => Some(SyncCommit::Normal),
            "off" => Some(SyncCommit::Off),
            _ => None,
        }
    }
}

fn sync_commit(dir: &Path) -> SyncCommit {
    fs::read_to_string(dir.join("sync")).ok().and_then(|s| SyncCommit::parse(&s)).unwrap_or(SyncCommit::Full)
}

fn set_sync_commit(dir: &Path, m: SyncCommit) -> io::Result<()> {
    write_file(&dir.join("sync"), m.name().as_bytes())
}

#[cfg(target_vendor = "apple")]
fn plain_fsync(f: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // std's sync_data is F_FULLFSYNC here; fsync(2) stops at the drive's cache
    if unsafe { libc::fsync(f.as_raw_fd()) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(target_vendor = "apple"))]
fn plain_fsync(f: &File) -> io::Result<()> {
    f.sync_data()
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The log's records from segment `from` on, in order, with their segment, until `f` returns false. Stops quietly at
/// a bad frame: the live segment's tail may be mid-write.
pub(crate) fn read_log(
    dir: &Path,
    c: Option<&Crypt>,
    from: u64,
    mut f: impl FnMut(u64, Op<String>) -> bool,
) -> io::Result<()> {
    for s in segments(dir)?.into_iter().filter(|&s| s >= from) {
        let buf = match fs::read(seg_path(dir, s)) {
            Ok(b) => b,
            // cleaned up while it was read: what comes after it can't be replayed without it
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("log segment {s} was cleaned up while it was being read; try again"),
                ));
            }
            Err(e) => return Err(e),
        };
        let mut off = 0;
        while let Some(Ok((p, next))) = frame_at(&buf, s, off, c) {
            let Some(ops) = decode_all(&p) else { break };
            for op in ops {
                if !f(s, op) {
                    return Ok(());
                }
            }
            off = next;
        }
    }
    Ok(())
}

/// Copies the database in `dir` to the new folder `to`: the manifest, pages, history, and the log.
/// With `cut` (from `Log::cut`: the database is open), the log only up to it, closed as a clean
/// shutdown closes it; the caller holds off checkpoints, so nothing but the live segment and pack
/// change meanwhile, and both only grow. Without, all of it (the database is closed). Returns
/// (files, bytes) copied.
pub(crate) fn copy_database(
    dir: &Path,
    to: &Path,
    c: Option<&Crypt>,
    cut: Option<(u64, u64)>,
) -> io::Result<(usize, u64)> {
    let (mut files, mut bytes) = (0, 0);
    let mut copy = |from: &Path, to: &Path, len: Option<u64>, tail: &[u8]| -> io::Result<()> {
        let mut src = File::open(from)?;
        let mut dst = File::create(to)?;
        bytes += match len {
            Some(n) => io::copy(&mut io::Read::take(&mut src, n), &mut dst)?,
            None => io::copy(&mut src, &mut dst)?,
        };
        dst.write_all(tail)?;
        dst.sync_all()?;
        files += 1;
        Ok(())
    };
    fs::create_dir_all(to.join("history"))?;
    fs::create_dir_all(to.join("pages"))?;
    for name in ["encryption", "manifest", "retention", "sync", "settings", "attached"] {
        if dir.join(name).exists() {
            copy(&dir.join(name), &to.join(name), None, &[])?;
        }
    }
    for (_, _, path) in history(dir)? {
        copy(&path, &to.join("history").join(path.file_name().unwrap()), None, &[])?;
    }
    for e in fs::read_dir(dir.join("pages"))? {
        let e = e?;
        if e.file_name().to_str().is_some_and(|n| n.starts_with("pack-")) {
            copy(&e.path(), &to.join("pages").join(e.file_name()), None, &[])?;
        }
    }
    for seg in segments(dir)?.into_iter().filter(|&s| cut.is_none_or(|(live, _)| s <= live)) {
        let path = seg_path(dir, seg);
        match cut {
            Some((live, len)) if seg == live => copy(&path, &seg_path(to, seg), Some(len), &close_frame(c, seg, len))?,
            _ => copy(&path, &seg_path(to, seg), None, &[])?,
        }
    }
    for d in [to.join("history"), to.join("pages"), to.to_path_buf()] {
        sync_dir(&d)?;
    }
    Ok((files, bytes))
}

/// Reads every log segment, history's included; a bad record anywhere but the live segment's tail
/// is corruption. Returns the records read.
pub(crate) fn check_log(dir: &Path, c: Option<&Crypt>) -> io::Result<u64> {
    let segs = segments(dir)?;
    let mut records = 0;
    for (i, &s) in segs.iter().enumerate() {
        let path = seg_path(dir, s);
        let buf = fs::read(&path)?;
        let mut off = 0;
        while off < buf.len() {
            match frame_at(&buf, s, off, c) {
                Some(Ok((p, next))) if decode_all(&p).is_some() => off = next,
                None if i + 1 == segs.len() => break, // being written
                Some(Err(e)) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
                _ => return Err(corrupt(&path, off)),
            }
            records += 1;
        }
    }
    Ok(records)
}

pub(crate) fn read_manifest_at(path: &Path, c: Option<&Crypt>) -> io::Result<Option<(u64, bool, Vec<ManBranch>)>> {
    Ok(read_manifest(path, c)?.map(|(first, closed, branches, _)| (first, closed, branches)))
}

/// A manifest: its first log segment, whether it was closed, its branches, and the sequence
/// counters at its cut (None before format 6, when counters were rows of main).
type Manifest = (u64, bool, Vec<ManBranch>, Option<Counters>);

fn read_manifest(path: &Path, c: Option<&Crypt>) -> io::Result<Option<Manifest>> {
    let path = path.to_path_buf();
    let buf = match fs::read(&path) {
        Ok(b) => crate::crypt::open_file(c, b, &path.display().to_string())?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let bad =
        || io::Error::new(io::ErrorKind::InvalidData, format!("{} is corrupt (checksum mismatch)", path.display()));
    let (body, crc) = buf.split_at_checked(buf.len().saturating_sub(4)).ok_or_else(bad)?;
    if crc.len() != 4 || crc32fast::hash(body) != u32::from_le_bytes(crc.try_into().unwrap()) {
        return Err(bad());
    }
    let (version, rest) = body.strip_prefix(MAN_MAGIC).and_then(|b| b.split_first()).ok_or_else(bad)?;
    if !(b'3'..=MAN_VERSION).contains(version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is from a newer Chronos (format {})", path.display(), *version as char),
        ));
    }
    let (mut r, v4, v5, v6) = (Reader(rest), *version >= b'4', *version >= b'5', *version >= b'6');
    let parse = |r: &mut Reader| -> Option<Manifest> {
        let first = r.u64()?;
        let closed = r.u8()? == 1;
        let out = (0..r.u32()?)
            .map(|_| {
                Some(ManBranch {
                    name: r.str()?,
                    parent: r.opt()?,
                    born: r.u64()?,
                    base: r.take(32)?.try_into().ok()?,
                    data: r.take(32)?.try_into().ok()?,
                    version: r.u64()?,
                    flagged: r.u8()? == 1,
                    id: if v4 { r.str()? } else { String::new() },
                    created: if v4 { r.u64()? } else { 0 },
                    meta: if v4 { r.str()? } else { "{}".into() },
                    delta: if v5 {
                        (0..r.u32()?).map(|_| Some((r.str()?, r.opt()?))).collect::<Option<_>>()?
                    } else {
                        vec![]
                    },
                })
            })
            .collect::<Option<_>>()?;
        let counters = match v6 {
            true => Some((0..r.u32()?).map(|_| Some((r.str()?, r.u64()? as i64, r.opt()?))).collect::<Option<_>>()?),
            false => None,
        };
        Some((first, closed, out, counters))
    };
    parse(&mut r).filter(|_| r.0.is_empty()).map(Some).ok_or_else(bad)
}

pub(crate) fn segments(dir: &Path) -> io::Result<Vec<u64>> {
    let mut v: Vec<u64> =
        fs::read_dir(dir)?.filter_map(|e| e.ok()?.file_name().to_str()?.strip_prefix("wal-")?.parse().ok()).collect();
    v.sort();
    Ok(v)
}

pub(crate) fn seg_path(dir: &Path, seg: u64) -> PathBuf {
    dir.join(format!("wal-{seg:016}"))
}

pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// A file outside the database folder (it must be, to help) that says how far the folder has got:
/// its identity (`remote::owner`) and a log position known durable, moved on at each checkpoint
/// (after the manifest) and clean close. An open that finds the folder behind it refuses: someone
/// put older files back (see SECURITY.md). Ahead is fine (a crash before the anchor moved), and
/// moves it on. Sealed with the key when the database is encrypted, so it can't be forged
/// without the key; otherwise only its location's permissions protect it.
/// ponytail: commits since the last checkpoint or close aren't in it, so a folder cut back to
/// within that stretch of log passes; anchoring each commit would cost an fsync of its own.
pub(crate) struct Anchor {
    path: PathBuf,
    id: String,
}

const ANCHOR_MAGIC: &[u8] = b"SONOSANC1";

impl Anchor {
    fn write(&self, c: Option<&Crypt>, seg: u64, off: u64) -> io::Result<()> {
        let mut b = ANCHOR_MAGIC.to_vec();
        put_str(&mut b, &self.id);
        b.extend(seg.to_le_bytes());
        b.extend(off.to_le_bytes());
        b.extend(crc32fast::hash(&b).to_le_bytes());
        write_file(&self.path, &sealed(c, b))
    }

    /// The anchor's identity and position; None if there is no anchor file.
    fn read(&self, c: Option<&Crypt>) -> io::Result<Option<(String, u64, u64)>> {
        let what = format!("anchor {}", self.path.display());
        let buf = match fs::read(&self.path) {
            Ok(b) => crate::crypt::open_file(c, b, &what)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("{what} is corrupt"));
        let (body, crc) = buf.split_at_checked(buf.len().saturating_sub(4)).ok_or_else(bad)?;
        if crc.len() != 4 || crc32fast::hash(body) != u32::from_le_bytes(crc.try_into().unwrap()) {
            return Err(bad());
        }
        let mut r = Reader(body.strip_prefix(ANCHOR_MAGIC).ok_or_else(bad)?);
        let got = (|| Some((r.str()?, r.u64()?, r.u64()?)))().ok_or_else(bad)?;
        r.0.is_empty().then_some(Some(got)).ok_or_else(bad)
    }
}

/// With an anchor configured (`path`): refuses a folder behind it, or one that isn't the
/// database it anchors, or (unless `fresh`, a new folder with nothing to roll back) an anchor
/// that's missing, since deleting it would otherwise turn the check off. `reset` accepts the
/// folder as it is, for a deliberate restore from a backup. Then points the anchor at the folder
/// and attaches it to `log`, which moves it on from here.
pub(crate) fn check_anchor(log: &mut Log, path: &Path, fresh: bool, reset: bool) -> io::Result<()> {
    let (seg, off) = log.cut()?;
    let a = Anchor { path: path.into(), id: crate::remote::owner(&log.dir)? };
    let refuse = |why: String| {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: {why} (XX0A1); if you restored it from a backup on purpose, open it once with --anchor-reset (CHRONOS_ANCHOR_RESET=1)",
                log.dir.display()
            ),
        ))
    };
    if !reset {
        match a.read(log.crypt.as_deref()) {
            Ok(None) if fresh => {}
            Ok(None) => {
                return refuse(format!(
                    "anchor {} is missing; refusing, since deleting it would skip the rollback check",
                    path.display()
                ));
            }
            Ok(Some((id, ..))) if id != a.id => {
                return refuse(format!("anchor {} belongs to another database folder", path.display()));
            }
            Ok(Some((_, s, o))) if (seg, off) < (s, o) => {
                return refuse(format!("database folder is older than its anchor {}: rolled back?", path.display()));
            }
            Ok(Some(_)) => {}
            Err(e) => return refuse(format!("{e}")),
        }
    }
    a.write(log.crypt.as_deref(), seg, off)?;
    log.anchor = Some(a);
    Ok(())
}

/// Replaces a small file atomically and durably: a synced temp file renamed over it, then its
/// folder synced (so neither a power cut nor a crash leaves it empty or half written).
pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // its own temp name: `remote` and `remote.seen` must not share `remote.tmp`
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    sync_dir(path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new(".")))
}

fn corrupt(path: &Path, off: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} is corrupt at byte {off}; this is not a crash tail, so refusing to guess", path.display()),
    )
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend((s.len() as u32).to_le_bytes());
    b.extend(s.as_bytes());
}

fn put_opt(b: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(s) => {
            b.push(1);
            put_str(b, s);
        }
        None => b.push(0),
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (a, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    fn opt(&mut self) -> Option<Option<String>> {
        match self.u8()? {
            0 => Some(None),
            1 => Some(Some(self.str()?)),
            _ => None,
        }
    }
}

#[cfg(all(test, unix))]
mod lock_tests {
    use std::os::fd::AsRawFd;

    /// A copy of the lock's file (as a forked child holds until it execs) doesn't keep the
    /// folder locked once the log unlocks it.
    #[test]
    fn unlocking_frees_the_folder_while_a_copy_is_open() {
        let dir = std::env::temp_dir().join(format!("chronos-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lock = super::lock(&dir).unwrap();
        let copy = unsafe { libc::dup(lock.as_raw_fd()) }; // what a fork would inherit
        assert!(copy >= 0);
        assert!(super::lock(&dir).is_err(), "held");
        lock.unlock().unwrap();
        drop(lock);
        assert!(super::lock(&dir).is_ok(), "free although a copy is still open");
        unsafe { libc::close(copy) };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
