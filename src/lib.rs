//! Chronos DB: copy-on-write branches for agents.
//!
//! A branch's state is a prolly tree on disk (immutable, content-addressed pages) plus an in-memory
//! overlay of changes since the last checkpoint. Forking copies two pointers: O(1) at any size.
//! Each branch has its own lock, so agents on different branches never wait on each other; they
//! only meet at merge. A checkpoint folds overlays into new trees, writing only changed pages, so
//! memory holds recent changes rather than the whole database.
//!
//! `Db::open` is durable (see `disk`): main writes and merges into main are fsynced before returning
//! (as far as `SyncCommit` says); a fork's
//! unmerged writes survive clean shutdown but may be lost in a crash. `Db::new` is in memory only.

mod agents;
pub mod attach;
mod backup;
mod checks;
pub mod crypt;
mod disk;
mod hnsw;
mod index;
pub mod limits;
pub mod metrics;
mod migrate;
mod outbox;
mod pages;
pub mod pgclient;
mod prolly;
mod reads;
mod remote;
mod scope;
pub mod search;
mod simulate;
pub mod sql;
mod stale;
mod sync;
mod tally;
mod traced;

use disk::Op;
use imbl::{OrdMap, ordmap::DiffItem};
use pages::{Hash, Pages};
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::iter::Peekable;
use std::path::Path;
use sync::atomic::{AtomicBool, AtomicU64, Ordering};
use sync::mpsc::{SyncSender, sync_channel};
use sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, Weak};

pub use agents::{Acting, Agent, AgentSpec, Audit, Can, GUEST, MergePolicy, Outcome, Outcomes, Review, SYSTEM, Slot};
pub use backup::{Backup, Verified};
pub use checks::MergeCheck;
/// Makes this thread act for an agent until the guard is dropped: what it writes is logged as the
/// agent's (for servers that check rights with `Core::as_agent` and then run the operation).
pub use disk::{ActingAs, SyncCommit, acting_as};
pub use outbox::Effect;
pub use reads::Read;

thread_local! {
    /// The branches this thread holds the long lock of (see `batch_long`), each with its database's address: a write
    /// to one from inside it, by a function the statement runs, doesn't take the lock again. (A session's temporary
    /// tables are a database of their own, whose branches share names with the world's: `main`, `tx`.)
    static HELD_LONG: std::cell::RefCell<Vec<(usize, String)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Set while MCP safe mode runs a call: nothing it runs may move a sequence's counter.
    static FIXED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The branches this thread looked up last, most recent first: (their database's `uid`, name,
    /// branch). See `branch`.
    static LAST: std::cell::RefCell<Vec<(u64, String, Weak<Branch>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Until the guard is dropped, nothing this thread runs (functions, triggers, EXECUTE and
/// simulation worlds included) may move a sequence's counter, which every world shares:
/// setval, ALTER SEQUENCE ... RESTART and TRUNCATE ... RESTART IDENTITY fail with 42501.
pub fn fixed_counters() -> FixedCounters {
    FixedCounters(FIXED.replace(true))
}

pub struct FixedCounters(bool);

impl Drop for FixedCounters {
    fn drop(&mut self) {
        FIXED.set(self.0);
    }
}

/// Whether `fixed_counters` holds on this thread.
pub(crate) fn counters_fixed() -> bool {
    FIXED.get()
}
pub use index::{Hit, Query};
pub use metrics::{Metrics, Timing};
pub use migrate::Migration;
pub use simulate::{Replayed, SimWorld, Simulate, Simulated};

pub type Val = Arc<str>;

/// Whether `table` is one of the database's own (schemas, constraint indexes, agents, ...), which
/// no interface lets anyone read or write as rows.
pub fn reserved_table(table: &str) -> bool {
    table.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("_sonos"))
}

/// Whether a diff shows row `key`: the database's own rows (agents, constraint indexes, ...)
/// aren't shown, except tables' schemas and functions, which SQL's DIFF turns into CREATE and
/// ALTER TABLE and CREATE FUNCTION.
fn shown(key: &str) -> bool {
    !reserved_table(key) || key.starts_with("_sonos_tables/") || sql::schema_object(key)
}

/// How `Db::open_with` opens a folder.
#[derive(Default, Clone)]
pub struct Open {
    /// Object storage behind the folder (see `open_remote`).
    pub remote: Option<String>,
    /// Take the remote's lease from another folder (see `open_remote_takeover`).
    pub takeover: bool,
    /// Encrypt with this key (see `open_encrypted`).
    pub key: Option<[u8; 32]>,
    /// Rollback detection: a file outside the folder recording how far the database has got. An
    /// open that finds the folder older than it refuses (see SECURITY.md and docs/operations.md).
    pub anchor: Option<std::path::PathBuf>,
    /// Accept the folder as it is and re-anchor it: after a deliberate restore from a backup, or
    /// to start anchoring an existing database.
    pub anchor_reset: bool,
}
pub const MAIN: &str = "main";

/// Changes on top of a tree; `None` hides a row the tree has.
/// Changes read lazily, in key order (see `Core::state_diff_iter`).
type DiffRows<'a> = Box<dyn Iterator<Item = Result<Change>> + 'a>;
type Over = OrdMap<Val, Option<Val>>; // keys shared, not copied, when a node is copied on write

/// Overlay keys from `from` on (the overlay's keys are `Val`s, looked up as `str`).
fn from_key(from: &str) -> (std::ops::Bound<&str>, std::ops::Bound<&str>) {
    (std::ops::Bound::Included(from), std::ops::Bound::Unbounded)
}

/// A tree root in use. Every state and every reader holding one keeps a clone, and the
/// database keeps a weak handle to each, so cleanup can see exactly which trees are still read.
#[derive(Clone)]
pub(crate) struct Tree(Arc<Hash>);

impl PartialEq for Tree {
    fn eq(&self, o: &Self) -> bool {
        *self.0 == *o.0
    }
}

/// Indexed states of one table, oldest first: a search uses the newest one it is close to.
type Anchors = Vec<(State, Arc<index::TableIndex>)>;

/// A search's answer: rows passing the filters, and the hits.
type Found = (usize, Vec<Hit>);
/// How many rows match, and the best keys with their scores.
type Ranked = (usize, Vec<(String, f32)>);

/// A branch version, the index it searches and its delta from that index.
type Delta = (State, Arc<index::TableIndex>, Arc<index::Delta>);

/// A branch's contents at one moment. Cloning is O(1).
#[derive(Clone)]
struct State {
    tree: Option<Tree>, // None only for in-memory databases, where the overlay is everything
    over: Over,
}

/// Which version of a world a piece of work was made in, kept to tell whether the world has been written to since
/// (`State::is`, `Core::still`). It holds no tree: a tree is named by the hash of its root page, so the same name
/// is the same rows whether or not cleanup has freed the pages meanwhile, and holding the tree itself would keep
/// pages cleanup could free for as long as the work is kept (a prepared statement lives as long as its connection).
/// The overlay, the changes not in a tree yet, is held by clone: that is memory, no pages, and it keeps an
/// address from being taken for another map's after it is freed.
#[derive(Clone)]
struct Stamp {
    tree: Option<Hash>,
    over: Over,
}

impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "State({} overlaid rows)", self.over.len())
    }
}

impl State {
    fn empty(tree: Option<Tree>) -> State {
        State { tree, over: Over::new() }
    }

    /// The same version: equal trees and the very same overlay (O(1)).
    fn same(&self, o: &State) -> bool {
        self.tree == o.tree && self.over.ptr_eq(&o.over)
    }

    /// Which version this is, to tell it again later without holding it (see `Stamp`).
    fn stamp(&self) -> Stamp {
        Stamp { tree: self.tree.as_ref().map(|t| *t.0), over: self.over.clone() }
    }

    /// Whether this is the version `s` was taken of (O(1)).
    fn is(&self, s: &Stamp) -> bool {
        self.tree.as_ref().map(|t| *t.0) == s.tree && self.over.ptr_eq(&s.over)
    }

    fn set(&mut self, k: &str, v: Option<Val>) {
        match (v, &self.tree) {
            (None, None) => {
                self.over.remove(k); // no tree underneath: nothing to hide
            }
            (v, _) => {
                self.over.insert(k.into(), v);
            }
        }
    }
}

struct Branch {
    born: u64,           // fork order; checkpoints chain sibling fork points newest-first by it
    base: RwLock<State>, // the fork point, for diff and three-way merge; a partial merge moves it on for its rows
    snap: RwLock<State>, // committed state; readers clone it in O(1) and never block writers
    writer: Mutex<bool>, // one writer per branch; true once merged or discarded
    long: Mutex<()>,     // held by a write done in several batches (`batch_long`); other writes wait on it
    version: AtomicU64,  // writes since the fork: a merge can insist on the version it last saw
    /// moves on with every change to its rows, its own writes or merges into it (see `stamp`): a
    /// merge settled against this world as a preview saw it insists on it (`MergeOptions::target`)
    stamp: AtomicU64,
    flagged: AtomicBool, // open during a crash, so its last writes may be gone: merge must confirm
    info: RwLock<Info>,
    changed: AtomicU64,        // rows written since the fork (for quotas; see `changed`)
    counted: AtomicBool,       // `changed` includes rows from before this process opened the database
    active: AtomicU64,         // last write, fork, merge into it or read (ms since 1970): see `world_idle_ttl`
    batch: Mutex<disk::Batch>, // its writes not logged yet (see `log_write`)
    retired: AtomicBool,       // taken out of the branch map (merged, discarded or reloaded)
    /// what it read, when it checks its reads (see `stale`); a transaction shares its world's
    reads: RwLock<Option<Recorder>>,
    /// rows it locked with SELECT ... FOR ..., by key, with the strongest mode (see `RowLock`):
    /// its merge fails if another merge changed or locked one since `start`; nothing fills it now: SELECT ... FOR UPDATE takes row intents (see `Core::claim`), which make others wait instead
    locked: Mutex<std::collections::BTreeMap<String, u8>>,
    /// `Core::lock_clock` when it forked: row locks merged after it are other transactions'
    start: AtomicU64,
    /// the world's state this one was last brought up to (its fork point, then each `Core::rebase_tx`): what a
    /// READ COMMITTED transaction has seen of its world's commits
    synced: Mutex<State>,
    /// its world's `changed` as `synced` was last set: how many rows the world has written since is at most the
    /// difference (merges count theirs a moment after they land), which `Core::rebase_tx` goes by
    synced_rows: AtomicU64,
    /// brought up to its world at least once (see `Core::rebase_tx`): its fork point is then what the world had later,
    /// not what its earlier statements were checked against, so its commit checks its rows on the world it goes into
    /// even when that is still its fork point (see `merge_locked`)
    rebased: AtomicBool,
}

/// A world's read set, shared by the views of it that record into it (see `stale`).
pub(crate) type Recorder = Arc<Mutex<stale::ReadSet>>;

/// Who a branch is apart from its data: its world's ID, when it was made, and what it's for.
#[derive(Clone)]
struct Info {
    id: String,
    created: u64,                                     // ms since 1970
    meta: serde_json::Map<String, serde_json::Value>, // owner, description, tags, ... as callers like
}

impl Info {
    fn new() -> Info {
        Info { id: new_world_id(), created: now_ms(), meta: Default::default() }
    }
}

/// A world's ID: random, so never reused, unlike names (and fork order, after a restart).
fn new_world_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    // std's, never shuttle's: a static outlives one simulated run and is shared by the test
    // threads running simulations side by side, and shuttle's atomic is a RefCell (tests/sim.rs)
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher(); // randomly seeded
    h.write_u64(N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    h.write_u64(now_ms());
    format!("w{:016x}", h.finish())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn main_info() -> Info {
    Info { id: MAIN.into(), created: 0, meta: Default::default() }
}

const META_MAX: usize = 64 << 10;
/// A simulated world's metadata key: what made it (see `simulate`).
pub(crate) const SIM: &str = "sim";

fn bad_meta(message: String) -> Error {
    Error::Constraint { code: "22023", message }
}

/// Refuses metadata keys only the database sets (an agent's world's owner, its expiry, ...).
fn reserved_keys(m: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    // `sim` holds what `replay` reruns: only a simulation sets it
    match m.keys().find(|k| k.starts_with("_sonos") || *k == SIM) {
        Some(k) => Err(bad_meta(format!("metadata key {k} is kept by the database; pick another name"))),
        None => Ok(()),
    }
}

fn meta_object(v: serde_json::Value) -> Result<serde_json::Map<String, serde_json::Value>> {
    match v {
        serde_json::Value::Object(m) => Ok(m),
        serde_json::Value::Null => Ok(Default::default()),
        v => Err(bad_meta(format!("world metadata must be a JSON object, not {v}"))),
    }
}

fn world_of(name: &str, e: &Entry) -> World {
    let info = e.br.info.read().unwrap();
    // keys only the database sets (see `agents`) come out as fields of their own
    let (mut meta, mut owner, mut expires, mut checkpoints, mut pinned, mut declared) =
        (serde_json::Map::new(), None, None, vec![], false, None);
    for (k, v) in &info.meta {
        match k.as_str() {
            agents::OWNER => owner = v.as_str().map(String::from),
            agents::EXPIRES => expires = v.as_u64(),
            agents::PINNED => pinned = v.as_bool() == Some(true),
            scope::SCOPE => declared = Some(v.clone()),
            agents::CHECKPOINTS => {
                checkpoints =
                    v.as_object().into_iter().flatten().filter_map(|(n, t)| Some((n.clone(), t.as_u64()?))).collect()
            }
            _ => {
                meta.insert(k.clone(), v.clone());
            }
        }
    }
    checkpoints.sort_by_key(|c: &(String, u64)| c.1);
    World {
        name: name.into(),
        id: info.id.clone(),
        parent: e.parent.clone(),
        depth: e.depth,
        created: info.created,
        version: e.br.version.load(Ordering::Relaxed),
        flagged: e.br.flagged.load(Ordering::Relaxed),
        meta: serde_json::Value::Object(meta),
        owner,
        expires,
        checkpoints,
        active: if e.parent.is_none() { 0 } else { e.br.active.load(Ordering::Relaxed) }, // main is never idle
        pinned,
        scope: declared,
    }
}

/// What a world costs on its own (see `Core::usage`).
#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    pub world: String,
    pub rows: Option<usize>, // rows changed since its fork; None for main
    pub pages: usize,        // tree pages only it holds
    pub bytes: u64,          // those pages as stored, plus its changed rows held outside pages
}

/// A world (a branch) as listed.
#[derive(Debug, Clone, PartialEq)]
pub struct World {
    pub name: String,
    pub id: String,
    pub parent: Option<String>,
    pub depth: usize,                    // forks between it and main
    pub created: u64,                    // ms since 1970
    pub version: u64,                    // writes since its fork
    pub flagged: bool,                   // open during a crash: its merge must confirm a version
    pub meta: serde_json::Value,         // a JSON object
    pub owner: Option<String>,           // the agent that forked it (see `agents`)
    pub expires: Option<u64>,            // when it will be discarded (ms since 1970), if ever
    pub checkpoints: Vec<(String, u64)>, // named moments, oldest first (see `checkpoint_world`)
    pub active: u64,                     // its last write, fork, merge into it or read (ms since 1970; 0 for main)
    pub pinned: bool,                    // never discarded for being idle (see `pin`)
    /// what it was forked to change (see `scope`): `may_change`, `tenant`, `intent`, `run`
    pub scope: Option<serde_json::Value>,
}

impl Branch {
    fn new(born: u64, base: State, data: State, info: Info) -> Arc<Self> {
        let synced = base.clone();
        Arc::new(Branch {
            born,
            base: RwLock::new(base),
            snap: RwLock::new(data),
            writer: Mutex::new(false),
            long: Mutex::new(()),
            version: AtomicU64::new(0),
            stamp: AtomicU64::new(stamp()),
            flagged: AtomicBool::new(false),
            changed: AtomicU64::new(0),
            counted: AtomicBool::new(true),
            active: AtomicU64::new(info.created),
            batch: Mutex::new(Default::default()),
            retired: AtomicBool::new(false),
            info: RwLock::new(info),
            reads: RwLock::new(None),
            locked: Mutex::new(Default::default()),
            start: AtomicU64::new(0),
            synced: Mutex::new(synced),
            synced_rows: AtomicU64::new(0),
            rebased: AtomicBool::new(false),
        })
    }

    /// Its read set, if it checks its reads. One made here (a world from before this process
    /// opened the database) doesn't know what was read before.
    fn recorder(&self) -> Option<Recorder> {
        if let Some(r) = self.reads.read().unwrap().as_ref() {
            return Some(r.clone());
        }
        if !self.info.read().unwrap().meta.contains_key(stale::CHECK_READS) {
            return None;
        }
        Some(self.reads.write().unwrap().get_or_insert_with(|| Arc::new(Mutex::new(stale::ReadSet::default()))).clone())
    }
    fn snapshot(&self) -> State {
        self.snap.read().unwrap().clone()
    }
    fn base(&self) -> State {
        self.base.read().unwrap().clone()
    }
}

/// Rows to write, in key order: `None` deletes.
pub(crate) type Changes = Vec<(String, Option<Val>)>;

/// A big statement's tree, for its commit (see `Core::commit_bulk`).
struct Bulk {
    branch: String,
    born: u64,
    state: State,
    rows: u64,
    /// of them, the tables' own rows: what the log's note (and so history and the audit trail) counts
    shown: u64,
    /// the branch's version once its note is in the log: committed from then, whatever fails after
    logged: std::cell::Cell<Option<u64>>,
}

struct Entry {
    parent: Option<String>,
    br: Arc<Branch>,
    kids: usize,  // live worlds forked from this one: it can't go while it has any
    depth: usize, // forks between it and main
}

/// The live branches by name, and their names by world ID. Changed only through `add` and
/// `take`, which keep the forks counted.
#[derive(Default)]
struct Branches {
    by_name: HashMap<String, Entry>,
    ids: HashMap<String, String>,
}

impl std::ops::Deref for Branches {
    type Target = HashMap<String, Entry>;
    fn deref(&self) -> &Self::Target {
        &self.by_name
    }
}

impl Branches {
    /// Adds `name`, forked from `parent` (None: main).
    fn add(&mut self, name: String, parent: Option<String>, br: Arc<Branch>) {
        let depth = match parent.as_deref().and_then(|p| self.by_name.get_mut(p)) {
            Some(p) => {
                p.kids += 1;
                p.depth + 1
            }
            None => 0,
        };
        self.ids.insert(br.info.read().unwrap().id.clone(), name.clone());
        self.by_name.insert(name, Entry { parent, br, kids: 0, depth });
    }

    /// Drops `name`, which must have no forks.
    fn take(&mut self, name: &str) -> Option<Entry> {
        let e = self.by_name.remove(name)?;
        e.br.retired.store(true, Ordering::Release);
        debug_assert_eq!(e.kids, 0, "{name} still has forks");
        if let Some(p) = e.parent.as_deref().and_then(|p| self.by_name.get_mut(p)) {
            p.kids -= 1;
        }
        let id = e.br.info.read().unwrap().id.clone();
        if self.ids.get(&id).is_some_and(|n| n == name) {
            self.ids.remove(&id);
        }
        Some(e)
    }
}

/// Lock order, always: long locks (the later fork first, each at most once), gate, writer locks
/// (the later fork first: a child before its parent), the branch map, snap locks, batches (one at
/// a time), the log. Long locks are taken only by `batch_long` (one, for its whole write),
/// `write`, `batch_with` from outside a write (one each) and merges (the world and where it
/// goes: `into`, else its parent), never while holding the gate; checkpoints take no long lock.
///
/// Log order must be a valid replay order. Each mutation appends its record while still holding
/// the locks it changed state under, and any two mutations that affect each other share one of
/// those locks, so their records land in the order they happened. Unrelated mutations commute.
/// Writes to a world other than main wait in its batch instead (see `log_write`), logged before
/// anything that depends on them: under the same locks, so the same holds.
/// The database. Share it by reference across threads; operations are reached through `Core`.
/// An on-disk database runs a background thread that does checkpoints, uploads and cleanup, so
/// agents never do that work inline.
pub struct Db {
    core: Arc<Core>,
    worker: Option<Worker>,
}

struct Worker {
    stop: Arc<AtomicBool>,
    kick: SyncSender<()>,
    thread: sync::thread::JoinHandle<()>,
}

impl std::ops::Deref for Db {
    type Target = Core;
    fn deref(&self) -> &Core {
        &self.core
    }
}

/// The operations of a `Db` (fork, get, put, merge, ...). Not constructed directly.
pub struct Core {
    kick: Option<SyncSender<()>>,                           // wakes the background worker
    owner: Option<String>,                                  // holder of the remote's lease
    indexes: Mutex<HashMap<String, Anchors>>,               // search indexes per table
    deltas: Mutex<HashMap<(String, u64), Delta>>,           // per (table, branch): its delta, while unchanged
    answers: Mutex<HashMap<(u64, String), (State, Found)>>, // per (branch, query): its answer, while unchanged
    seqs: Mutex<HashMap<String, (i64, i64)>>,               // sequence -> (next value, end of reserved block)
    listeners: sql::Listeners,                              // LISTEN: sessions waiting for NOTIFY, by world and channel
    sql: AtomicBool,        // a SQL table has existed: JSON writes must then check for them (see `batch`)
    views: AtomicBool,      // a view has existed: statements must then look for the ones they read
    sequences: AtomicBool,  // a sequence made with CREATE SEQUENCE has existed: a name read as a table may be one
    inherits: AtomicBool, // a table has had a child (partition or inheritor): reads of a table must then look for them
    privileges: AtomicBool, // a role or an object's owner or grants have existed: sessions must then look for them
    row_security: AtomicBool, // a table has had row security or a policy: reads and writes must then look for them
    probed: AtomicBool,   // the flags above cover checkpointed trees too (see `probe_catalog`)
    branches: RwLock<Branches>,
    log: Option<disk::Log>,
    pages: Option<Arc<Pages>>,
    past: Mutex<Vec<Arc<Past>>>, // worlds rebuilt at past moments, newest use first (see `past`)
    rates: Mutex<HashMap<String, (f64, u64)>>, // agents' write allowances: (writes left, as of ms)
    empty: Option<Hash>,         // root of the empty tree: lookups skip it
    trees: Mutex<Vec<Weak<Hash>>>, // every tree handed out; cleanup keeps the ones still alive
    published: Mutex<Vec<Hash>>, // roots in the manifest on disk: recovery needs them
    gate: RwLock<()>,            // mutations hold it shared; a checkpoint takes it exclusively for a consistent cut
    forks: AtomicU64,
    /// merges that brought row locks, counted (see `keep_locks`)
    lock_clock: AtomicU64,
    /// world -> row key -> `lock_clock` of the last merges into it that locked the row FOR UPDATE,
    /// and FOR NO KEY UPDATE
    row_locks: Mutex<HashMap<String, RowLocks>>,
    /// the rows open transactions' statements have written or locked, until they end (see `claim`)
    intents: Mutex<Intents>,
    /// bumped, and waiters woken, whenever intents are let go (std's, not shuttle's: nothing in the simulations waits)
    intent_wake: (std::sync::Mutex<u64>, std::sync::Condvar),
    metrics: Metrics,
    settings: Mutex<[u64; 5]>, // by `Setting`: loaded from the folder after replay, so replay never applies them
    replay_clock: AtomicU64,   // while replaying the log: the time it has reached (0 otherwise)
    running: Mutex<HashMap<String, u64>>, // agents' statements in flight (see `slot`)
    dropped: (AtomicU64, AtomicU64), // worlds discarded since the last cleanup, and the rows they changed
    /// sequence -> (highest value reserved, origin): the database's, in no world (see `next_values`)
    counters: Mutex<HashMap<String, (i64, Option<String>)>>,
    /// counter restarts so far, and sequence -> the count its last restart made (see `restarted_since`)
    restarts: (AtomicU64, Mutex<HashMap<String, u64>>),
    /// the log position of the latest counter record, and the highest one known durable: a value
    /// is handed to a client only once the record that covers it is (see `next_values_in`)
    counter_at: (AtomicU64, AtomicU64),
    /// the `Db` this is the core of, once it has one: merge checks run SQL, which needs it. A core
    /// replaying its log (at recovery, or rebuilding the past) has none, so it runs no checks
    me: std::sync::OnceLock<Weak<Core>>,
    /// this database among all in the process, never reused (a thread's last lookup; see `branch`)
    uid: u64,
    /// states merge checks read, by the name they read them as (`world@merge-n`; see `checks`)
    merging: Mutex<HashMap<String, State>>,
    /// the columns clients' SQL read, and when (see `reads`)
    reads: Mutex<reads::Log>,
    /// where effects that landed in main came from, and those discarded with their worlds (see `outbox::Ledger`)
    ledger: Mutex<outbox::Ledger>,
    tallies: Mutex<tally::Tallies>, // agents' merges into main in the last hour (see `tally`)
    /// the folder is attached to a Postgres database (see `attach`): its main changes only by merges written there
    attached: Option<Arc<attach::Attached>>,
}

/// A database-wide setting (`ALTER SYSTEM SET`), kept in the folder across restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    /// The statement_timeout every SQL session starts with, in ms (0: none).
    StatementTimeout = 0,
    /// Live worlds at most, besides main and transactions' own (0: no limit).
    MaxWorlds = 1,
    /// How long a world may go unused before the background sweeper discards it, in ms (0: never).
    WorldIdleTtl = 2,
    /// 1: main's rows change only by merging a world (see `Core::guard_main`); 0: directly too.
    ProtectMain = 3,
    /// 1: an effect discarded with its world keeps its payload in the outbox ledger; 0: only its SHA-256 (a payload
    /// can hold personal data). See `outbox::Discarded`.
    OutboxKeepPayloads = 4,
}

impl Setting {
    pub const ALL: [Setting; 5] = [
        Setting::StatementTimeout,
        Setting::MaxWorlds,
        Setting::WorldIdleTtl,
        Setting::ProtectMain,
        Setting::OutboxKeepPayloads,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Setting::StatementTimeout => "statement_timeout",
            Setting::MaxWorlds => "max_worlds",
            Setting::WorldIdleTtl => "world_idle_ttl",
            Setting::ProtectMain => "protect_main",
            Setting::OutboxKeepPayloads => "outbox_keep_payloads",
        }
    }

    pub fn parse(s: &str) -> Option<Setting> {
        Setting::ALL.into_iter().find(|x| x.name() == s)
    }
}

/// What the page files on disk hold, in bytes as stored (see `Core::disk_usage`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiskUsage {
    pub live: u64,        // pages some world (or a reader) uses now
    pub history: u64,     // pages only history keeps, for time travel within the retention window
    pub reclaimable: u64, // pages nothing needs: cleanup frees the files mostly made of them
    pub log: u64,         // the log, and the checkpoints history keeps
}

/// Discards since the last cleanup that make the background worker run one (checkpoint, then
/// cleanup): this many worlds, or worlds that changed this many rows.
const RECLAIM_WORLDS: u64 = 1000;
const RECLAIM_ROWS: u64 = 100_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub key: String,
    pub before: Option<Val>,
    pub after: Option<Val>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub key: String,
    pub base: Option<Val>,
    pub ours: Option<Val>,
    pub theirs: Option<Val>,
}

impl Conflict {
    /// Why this row conflicts, in words: which side did what, and which columns each changed.
    pub fn explain(&self) -> String {
        explain(self.base.as_deref(), self.ours.as_deref(), self.theirs.as_deref())
    }

    /// Whether the two sides changed different columns of the row (so BY COLUMNS combines it).
    pub fn columns_differ(&self) -> bool {
        self.explain().ends_with(COMBINES)
    }
}

/// How one row that both sides changed is settled in a merge (see `Core::merge_with`).
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    Ours,
    Theirs,
    /// this row, as a JSON object; None deletes it
    Row(Option<String>),
}

/// How to merge (see `Core::merge_with`).
#[derive(Debug, Clone, Default)]
pub struct MergeOptions {
    /// for rows both sides changed and nothing else settles: fail (list them), ours or theirs
    pub how: Resolve,
    /// merge only if the branch is at this version (see `merge_at`)
    pub expected: Option<u64>,
    /// rows settled one by one, by key (`table/id`)
    pub picks: HashMap<String, Pick>,
    /// rows where the two sides changed different columns are combined, not conflicts
    pub columns: bool,
    /// merge only these tables: their rows, schemas, constraint and index rows. The branch stays
    /// open with the rest, and rows merged into its parent aren't merged again
    pub only_tables: Vec<String>,
    /// merge only these rows (`table/id`), with their unique and reference rows; as `only_tables`
    pub only_keys: Vec<String>,
    /// merge into this world instead of the parent: three-way against the branch's fork point.
    /// The branch stays open, its changes still to merge into its parent
    pub into: Option<String>,
    /// merge only if the world merged into is as a preview that settled rows (OURS, THEIRS, BY
    /// COLUMNS, picks) saw it: its `MergePlan::target`
    pub target: Option<u64>,
    /// the rows the merge deletes, as the person merging read them: asked for when the world's merge policy has
    /// `confirm_deletes_over` and the merge deletes more (see `ConfirmDeletes`); a wrong count is refused
    pub confirm_deletes: Option<u64>,
}

/// What a merge would do, row by row (see `Core::merge_preview`).
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub key: String,
    /// apply, conflict, by columns, picked ours, picked theirs, picked row, kept ours, kept theirs
    pub outcome: &'static str,
    pub detail: String,
    pub base: Option<Val>,
    pub ours: Option<Val>,
    pub theirs: Option<Val>,
    /// the row after the merge (for a conflict: none, as nothing would be merged)
    pub result: Option<Val>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MergePlan {
    pub rows: Vec<Planned>,
    pub conflicts: usize,
    /// why the merge would fail even with no conflicts (a constraint the two sides break together)
    pub blocked: Option<String>,
    /// the branch's version the plan is for: merging at it (`MergeOptions::expected`) merges only
    /// what the plan showed, refused if the branch was written to or partly merged since
    pub version: u64,
    /// the world merged into as the plan saw it (its `stamp`, opaque): rows it settles (OURS,
    /// THEIRS, BY COLUMNS, picks) are settled against that world as it was, so a merge pinned to
    /// it too (`MergeOptions::target`) is refused if that world changed since
    pub target: u64,
    /// in a folder attached to Postgres, a merge into main: the rows it would write checked against Postgres as it
    /// is now (see `attach::dry_run`)
    pub live: Option<LiveCheck>,
    /// a person merging it must give the count of rows it deletes (`MergeOptions::confirm_deletes`)
    pub confirm_deletes: Option<ConfirmDeletes>,
}

/// A merge that deletes more rows than its world's merge policy lets a person approve without giving the count
/// (`confirm_deletes_over`): counted from the merge's own plan, never from what the agent says.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfirmDeletes {
    pub deletes: u64,
    pub over: u64,
    pub policy: String,
}

/// A dry run's check of a merge into main against the Postgres an attached folder copies (see `attach::dry_run`):
/// read in one read-only transaction with no locks, rolled back, so a snapshot of Postgres as it was then.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveCheck {
    /// the database, as said at attach (no secret)
    pub source: String,
    /// when Postgres was read (ms since 1970; 0: it wasn't)
    pub at: u64,
    /// each row the merge would write, compared with Postgres's
    pub rows: Vec<LiveRow>,
    /// the user triggers on the tables it would write (`orders: audit_row`): not run, so what they'd set isn't
    /// shown
    pub triggers: Vec<String>,
    /// why Postgres wasn't checked (unreachable, no URL given, another database, a refusal the write-back makes)
    pub error: Option<String>,
    /// for a world that checks its reads: what it read that Postgres changed since its fork (see `attach::live_reads`),
    /// which would hold the merge
    pub stale_reads: Vec<String>,
}

/// A row a merge would write to Postgres, as the dry run found it there.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRow {
    pub key: String,
    /// changed in Postgres since main's copy (or added, or gone): the merge would be refused for it
    pub changed: bool,
    /// Postgres's row now, as JSON of each column's text (None: no row there)
    pub postgres: Option<String>,
}

/// A row settled inside a merge: by a pick, by columns, or (replaying the log) as it was then.
enum Settle {
    Ours,
    Theirs,
    Json(Option<String>),
    Stored(Option<Val>),
}

/// Why a row conflicts, from its shown (JSON) versions.
/// How `explain` ends for a row whose sides changed different columns.
const COMBINES: &str = "(different columns: merge by columns combines them)";

fn explain(base: Option<&str>, ours: Option<&str>, theirs: Option<&str>) -> String {
    let obj = |v: Option<&str>| match v.map(serde_json::from_str::<serde_json::Value>) {
        Some(Ok(serde_json::Value::Object(o))) => Some(o),
        _ => None,
    };
    let changed = |from: &Option<serde_json::Map<String, serde_json::Value>>,
                   to: &Option<serde_json::Map<String, serde_json::Value>>| {
        let (f, t) = (from.clone().unwrap_or_default(), to.clone().unwrap_or_default());
        let mut c: Vec<String> = t.iter().filter(|(k, v)| f.get(*k) != Some(v)).map(|(k, _)| k.clone()).collect();
        c.extend(f.keys().filter(|k| !t.contains_key(*k)).cloned());
        c.sort();
        c
    };
    match (base, ours, theirs) {
        (None, Some(_), Some(_)) => "both sides added this row".into(),
        (_, None, None) => "both sides deleted this row".into(),
        (_, None, Some(_)) => "deleted here, changed by the parent".into(),
        (_, Some(_), None) => "changed here, deleted by the parent".into(),
        (_, Some(o), Some(t)) if o == t => {
            "both sides made the same change (still a conflict: first merge wins)".into()
        }
        _ => {
            let (b, o, t) = (obj(base), obj(ours), obj(theirs));
            let (co, ct) = (changed(&b, &o), changed(&b, &t));
            let both: Vec<&String> = co.iter().filter(|c| ct.contains(c)).collect();
            if both.is_empty() {
                format!("here: {}; the parent: {} {COMBINES}", co.join(", "), ct.join(", "))
            } else {
                format!("both changed {}", both.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Resolve {
    #[default]
    Fail,
    Ours,
    Theirs,
}

#[derive(Debug)]
pub enum Error {
    NoBranch(String),
    Exists(String),
    HasChildren(String),
    IsMain,
    Conflicts(Vec<Conflict>),
    /// The branch isn't at the version the merge expected, or it was open during a crash and the
    /// merge didn't confirm a version: writes to it may have been lost.
    Stale {
        branch: String,
        expected: Option<u64>,
        actual: u64,
    },
    /// Reading a page failed: the storage is damaged or unavailable.
    Io(io::Error),
    /// The merge would break a SQL constraint (a duplicate UNIQUE value, or a foreign key to a
    /// row that is gone), though each side was valid alone; nothing was merged.
    Constraint {
        code: &'static str,
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NoBranch(b) => write!(f, "branch '{b}' does not exist; create it with fork(\"main\", \"{b}\")"),
            Error::Exists(b) => write!(f, "branch '{b}' already exists; pick another name or discard it first"),
            Error::HasChildren(b) => {
                write!(f, "branch '{b}' has live forks; merge or discard them first, or drop it with CASCADE")
            }
            Error::IsMain => write!(f, "main has no parent; fork it and work on the fork"),
            Error::Conflicts(c) => write!(
                f,
                "{} conflicting rows (see conflicts); retry merge with Resolve::Ours or Resolve::Theirs",
                c.len()
            ),
            Error::Stale { branch, expected: Some(e), actual } => write!(
                f,
                "branch '{branch}' is at version {actual}, not {e}: it changed since (written to, or partly merged), \
                 or writes to it were lost in a crash; check its diff, then merge at version {actual}, or discard it"
            ),
            Error::Stale { branch, expected: None, actual } => write!(
                f,
                "branch '{branch}' was open when the database crashed, so its last writes may be lost; \
                 check its diff, then merge with version {actual} to confirm, or discard it"
            ),
            Error::Io(e) => write!(f, "storage error: {e}"),
            Error::Constraint { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl Default for Db {
    fn default() -> Self {
        Self::new()
    }
}

impl Db {
    /// An in-memory database: nothing is saved.
    pub fn new() -> Self {
        Db::of(Arc::new(Core::new()), None)
    }

    fn of(core: Arc<Core>, worker: Option<Worker>) -> Db {
        sql::note_start();
        let _ = core.me.set(Arc::downgrade(&core));
        Db { core, worker }
    }

    /// Opens (or creates) a database folder, recovering from the last checkpoint and log.
    /// Only one process can have a folder open at a time. A folder attached to a remote with
    /// `open_remote` stays attached.
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Db> {
        Db::open_with(dir, &Open::default())
    }

    /// `open`, encrypted with `key` (see `crypt`): a new folder is encrypted from its first byte;
    /// an existing one opens only with the key it was made with.
    pub fn open_encrypted(dir: impl AsRef<Path>, key: &[u8; 32]) -> io::Result<Db> {
        Db::open_with(dir, &Open { key: Some(*key), ..Default::default() })
    }

    /// Opens with every choice spelled out: object storage, taking its lease over, a key.
    pub fn open_with(dir: impl AsRef<Path>, o: &Open) -> io::Result<Db> {
        Core::open_at(dir.as_ref(), o).map(Db::start)
    }

    /// Opens a database folder backed by object storage (`s3://bucket/prefix` or `file:///dir`;
    /// needs the `s3` feature). Checkpoints upload pages and the manifest, so a new, empty folder
    /// attached to the same remote opens the database as of its last checkpoint and downloads
    /// pages as they are read. Closing the database checkpoints. One writer per remote at a time.
    pub fn open_remote(dir: impl AsRef<Path>, url: &str) -> io::Result<Db> {
        Db::open_with(dir, &Open { remote: Some(url.into()), ..Default::default() })
    }

    /// Like `open_remote`, but takes the remote's lease away from another folder. Only for when
    /// that folder's process is gone: its unpublished changes are lost, and it is fenced off.
    /// Also for a folder that fell behind its remote (another folder wrote since), which
    /// `open_remote` refuses: its own database is set aside in `stale-<ms>` and the remote's opens.
    pub fn open_remote_takeover(dir: impl AsRef<Path>, url: &str) -> io::Result<Db> {
        Db::open_with(dir, &Open { remote: Some(url.into()), takeover: true, ..Default::default() })
    }

    /// `Core::get`, with a SQL table's reg columns (regclass, regtype, ...) shown as their objects' names, as SQL
    /// shows them in world `b` (see `sql::rows_named`): what the HTTP and MCP row API and the CLI read.
    pub fn get(&self, b: &str, k: &str) -> Result<Option<Val>> {
        let Some(v) = self.core.get(b, k)? else { return Ok(None) };
        Ok(sql::rows_named(&self.core, b, vec![(k.to_string(), v)])?.pop().map(|(_, v)| v))
    }

    /// `Core::put`, with a SQL table's reg columns given as names read as SQL reads them in world `b` (see
    /// `sql::row_written`).
    pub fn put(&self, b: &str, k: &str, v: &str) -> Result<u64> {
        match sql::row_written(&self.core, b, k, v)? {
            Some(v) => self.core.put(b, k, &v),
            None => self.core.put(b, k, v),
        }
    }

    /// `Core::batch`, with reg columns read as `put` reads them.
    pub fn batch(&self, b: &str, rows: &[(&str, Option<&str>)]) -> Result<u64> {
        let read: Vec<Option<String>> = rows
            .iter()
            .map(|(k, v)| v.map_or(Ok(None), |v| sql::row_written(&self.core, b, k, v)))
            .collect::<Result<_>>()?;
        let rows: Vec<(&str, Option<&str>)> =
            rows.iter().zip(&read).map(|((k, v), r)| (*k, r.as_deref().or(*v))).collect();
        self.core.batch(b, &rows)
    }

    /// `Core::find`, with reg columns shown as `get` shows them.
    pub fn find(&self, b: &str, q: &Query) -> Result<(usize, Vec<Hit>)> {
        let (total, hits) = self.core.find(b, q)?;
        let rows = sql::rows_named(&self.core, b, hits.iter().map(|h| (h.key.clone(), h.row.clone())).collect())?;
        Ok((total, hits.into_iter().zip(rows).map(|(h, (_, row))| Hit { row, ..h }).collect()))
    }

    /// `Core::scan`, with reg columns shown as `get` shows them.
    pub fn scan(&self, b: &str, from: &str, limit: usize) -> Result<Vec<(String, Val)>> {
        sql::rows_named(&self.core, b, self.core.scan(b, from, limit)?)
    }

    fn start(mut core: Core) -> Db {
        core.metrics = Metrics::default(); // counted from here: replaying the log is not work done
        for e in core.branches.read().unwrap().values() {
            e.br.counted.store(false, Ordering::Relaxed); // replay saw only the log's part (see `changed`)
        }
        let (kick, wake) = sync_channel(1); // one pending wake-up is enough: requests coalesce
        core.kick = Some(kick.clone());
        let core = Arc::new(core);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (core, stop) = (core.clone(), stop.clone());
            sync::thread::spawn(move || {
                // wakes to checkpoint when asked, and every half minute to discard expired and idle
                // worlds; after enough discards, a checkpoint and cleanup give their space back
                #[cfg(not(feature = "shuttle"))]
                let next = || {
                    !matches!(
                        wake.recv_timeout(std::time::Duration::from_secs(30)),
                        Err(sync::mpsc::RecvTimeoutError::Disconnected)
                    )
                };
                #[cfg(feature = "shuttle")]
                let next = || wake.recv().is_ok();
                // a checkpoint that fails leaves nothing the next one can't redo (a failed fsync
                // aborts instead, see `disk::fatal`): say so, and try again in a while
                while next() && !stop.load(Ordering::Relaxed) {
                    let _ = core.expire_worlds();
                    let _ = core.announce_ambiguous(); // an effect whose lease ran out: said on its channel
                    if let Err(e) = core.save_reads() {
                        eprintln!("chronos: could not save the reads recorded: {e}");
                    }
                    if let Err(e) = core.save_tallies() {
                        eprintln!("chronos: could not save agents' hourly merge counts: {e}");
                    }
                    if let (Some(log), Some(pages)) = (&core.log, &core.pages) {
                        let reclaim = core.reclaim_due();
                        if (log.needs_checkpoint() || reclaim) && log.retry_due() {
                            let _one = log.ckpt.lock().unwrap();
                            let done = core.checkpoint_now(log, false).and_then(|()| match core.reclaim_due() {
                                true => core.gc_now(pages).map(|_| ()),
                                false => Ok(()),
                            });
                            if let Err(e) = done {
                                log.failed(&e);
                            }
                        }
                    }
                }
            })
        };
        Db::of(core, Some(Worker { stop, kick, thread }))
    }
}

impl Drop for Db {
    /// Stops the worker; with a remote, then publishes a final checkpoint, so nothing is left
    /// only on this machine.
    fn drop(&mut self) {
        // a `Db` without a worker is in memory, or lent to a merge check (see `Core::db`)
        let Some(w) = self.worker.take() else { return };
        w.stop.store(true, Ordering::Relaxed);
        let _ = w.kick.send(()); // waits out a pending wake-up, then this one stops the loop
        let _ = w.thread.join();
        if let Err(e) = self.save_reads() {
            eprintln!("chronos: could not save the reads recorded: {e}");
        }
        if let Err(e) = self.save_tallies() {
            eprintln!("chronos: could not save agents' hourly merge counts: {e}");
        }
        if let (Some(r), Some(owner)) = (self.pages.as_ref().and_then(|p| p.remote()), &self.owner) {
            match self.checkpoint_closing() {
                Ok(()) => {
                    if let Err(e) = remote::release(r, owner) {
                        eprintln!("chronos: could not release the lease on {}: {e}", r.url());
                    }
                }
                Err(e) => eprintln!("chronos: final checkpoint to {} failed: {e}", r.url()),
            }
        }
    }
}

impl Drop for Core {
    /// Logs the writes waiting in batches before the log closes (its fields drop after this).
    fn drop(&mut self) {
        sql::forget_stats(self.uid); // (its cumulative statistics are the process's: see `sql::pgstat`)
        // what worlds read, kept for the next open: last, as nothing reads any more (see `stale`)
        if let Err(e) = self.save_read_sets() {
            eprintln!("chronos: could not keep what worlds read; their merges will be held after the restart: {e}");
        }
        let (Some(log), Ok(m)) = (&self.log, self.branches.get_mut()) else { return };
        for e in m.values() {
            // not after a panic mid-write, which may have left a batch half made
            if let Ok(mut w) = e.br.batch.lock()
                && !w.is_empty()
            {
                log.append_batch(&mut log.lock(), &mut w);
            }
        }
    }
}

/// A session waiting for a row: its id, the sessions holding it up, and (world, table) of the row it waits on.
pub(crate) type RowWait = (u64, Vec<u64>, Option<(String, String)>);

impl Core {
    fn new() -> Self {
        let mut branches = Branches::default();
        branches.add(MAIN.into(), None, Branch::new(0, State::empty(None), State::empty(None), main_info()));
        Core {
            kick: None,
            owner: None,
            indexes: Mutex::new(HashMap::new()),
            deltas: Mutex::new(HashMap::new()),
            answers: Mutex::new(HashMap::new()),
            seqs: Mutex::new(HashMap::new()),
            counters: Mutex::new(HashMap::new()),
            restarts: (AtomicU64::new(0), Mutex::new(HashMap::new())),
            counter_at: (AtomicU64::new(0), AtomicU64::new(0)),
            listeners: Default::default(),
            sql: AtomicBool::new(false),
            views: AtomicBool::new(false),
            sequences: AtomicBool::new(false),
            inherits: AtomicBool::new(false),
            privileges: AtomicBool::new(false),
            row_security: AtomicBool::new(false),
            probed: AtomicBool::new(false),
            branches: RwLock::new(branches),
            log: None,
            pages: None,
            empty: None,
            past: Mutex::new(vec![]),
            rates: Mutex::new(HashMap::new()),
            trees: Mutex::new(vec![]),
            published: Mutex::new(vec![]),
            gate: RwLock::new(()),
            forks: AtomicU64::new(1),
            lock_clock: AtomicU64::new(0),
            row_locks: Mutex::new(HashMap::new()),
            intents: Mutex::new(Intents::default()),
            intent_wake: (std::sync::Mutex::new(0), std::sync::Condvar::new()),
            metrics: Metrics::default(),
            settings: Mutex::new([0; 5]),
            replay_clock: AtomicU64::new(0),
            running: Mutex::new(HashMap::new()),
            dropped: (AtomicU64::new(0), AtomicU64::new(0)),
            me: std::sync::OnceLock::new(),
            uid: {
                static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            merging: Mutex::new(HashMap::new()),
            reads: Mutex::new(reads::Log::new()),
            ledger: Mutex::new(outbox::Ledger::default()),
            tallies: Default::default(),
            attached: None,
        }
    }

    /// The `Db` this is the core of, to run SQL with; None while replaying the log (see `me`).
    /// Dropping it changes nothing: only the `Db` that opened the database stops its worker.
    pub(crate) fn db(&self) -> Option<Db> {
        self.me.get()?.upgrade().map(|core| Db { core, worker: None })
    }

    fn open_at(dir: &Path, o: &Open) -> io::Result<Core> {
        let mut taken = None;
        Core::open_leased(dir, o, &mut taken).inspect_err(|_| {
            // a lease this open took goes back when it fails, or the next open would need takeover
            // (one this folder held already, from before a crash, stays: its log isn't published)
            if let Some((url, owner)) = taken
                && let Ok(r) = remote::Remote::new(&url)
            {
                drop(remote::release(&r, &owner));
            }
        })
    }

    /// `open_at`; `taken` gets the remote and owner of a lease it took, once it has.
    fn open_leased(dir: &Path, o: &Open, taken: &mut Option<(String, String)>) -> io::Result<Core> {
        let (url, takeover, key) = (o.remote.as_deref(), o.takeover, o.key.as_ref());
        let lock = disk::lock(dir)?;
        // what big statements under way when the last process stopped had spilled (see `spill_dir`)
        let _ = std::fs::remove_dir_all(dir.join("spill"));
        // new: nothing written here yet (a manifest fetched from a remote is checked by the key)
        let fresh = disk::segments(dir)?.is_empty()
            && std::fs::read_dir(dir.join("pages")).map_or(true, |mut d| d.next().is_none());
        let crypt = crypt::setup(dir, key, fresh)?.map(Arc::new);
        let attached = dir.join("remote");
        let url = match (url, std::fs::read_to_string(&attached).ok()) {
            (Some(u), Some(had)) if u != had.trim() => {
                return Err(io::Error::other(format!("{} is attached to {}, not {u}", dir.display(), had.trim())));
            }
            (Some(u), None) => {
                disk::write_file(&attached, u.as_bytes())?;
                Some(u.to_string())
            }
            (u, had) => u.map(String::from).or(had.map(|h| h.trim().to_string())),
        };
        // a new database of its own (not one fetched from a remote, which has its own setting) keeps an hour of history
        if fresh && url.is_none() && !dir.join("retention").exists() {
            disk::set_retention(dir, disk::NEW_RETENTION_MS)?;
        }
        let remote = url.as_deref().map(remote::Remote::new).transpose()?;
        let mut set_aside = false;
        let owner = match &remote {
            Some(r) => {
                let owner = remote::owner(dir)?;
                if !remote::acquire(r, &owner, takeover)? {
                    *taken = Some((r.url().to_string(), owner.clone()));
                }
                // a fresh folder starts from the remote's manifest; a stale one doesn't hold the lease
                set_aside = remote::adopt(r, dir, takeover)?;
                Some(owner)
            }
            None => None,
        };
        let pages = Pages::open(dir.join("pages"), remote.zip(owner.clone()), crypt.clone())?;
        let empty = prolly::build(&pages, [])?;
        let mut db = Core::new();
        let t = db.track(empty);
        db.branches.get_mut().unwrap().by_name.get_mut(MAIN).unwrap().br =
            Branch::new(0, State::empty(Some(t.clone())), State::empty(Some(t)), main_info());
        db.pages = Some(Arc::new(pages));
        db.empty = Some(empty);
        db.owner = owner;
        let mut old_counters = true; // no manifest: the log's rows, if any, are in memory
        let (mut log, clean) = disk::recover(
            dir,
            lock,
            crypt.clone(),
            |man, counters| {
                db.load(man);
                old_counters = counters.is_none();
                let mut all = db.counters.lock().unwrap();
                all.extend(counters.into_iter().flatten().map(|(name, high, origin)| (name, (high, origin))));
            },
            |op| assert!(db.replay(op), "chronos: log replay diverged; the log only holds ops that succeeded"),
        )?;
        if let Some(anchor) = &o.anchor {
            // a takeover that set this folder's own database aside serves the remote's, not
            // anything left here, so there's nothing local to have rolled back: anchor it anew.
            // Only then: a takeover that adopted nothing is checked like any open
            disk::check_anchor(&mut log, anchor, fresh, o.anchor_reset || set_aside)?;
        }
        db.log = Some(log);
        db.replay_clock.store(0, Ordering::Relaxed);
        // after replay: the log holds what was accepted then, whatever the limits are now
        *db.reads.get_mut().unwrap() = reads::load(dir, crypt.as_deref());
        stale::load(&mut db, dir, crypt.as_deref(), clean)?; // what worlds read, if it closed cleanly
        *db.tallies.get_mut().unwrap() = tally::load(dir, crypt.as_deref());
        *db.ledger.get_mut().unwrap() = outbox::load_ledger(dir, crypt.as_deref());
        db.attached = attach::Attached::load(dir)?.map(Arc::new);
        let saved = disk::settings(dir)?;
        for s in Setting::ALL {
            db.settings.get_mut().unwrap()[s as usize] = saved.get(s.name()).and_then(|v| v.as_u64()).unwrap_or(0);
        }
        if !dir.join("manifest").exists() && disk::history(dir)?.is_empty() {
            // no checkpoint yet: the log starts from an empty database, which is history too
            let main = disk::ManBranch {
                name: MAIN.into(),
                parent: None,
                born: 0,
                base: empty,
                data: empty,
                version: 0,
                flagged: false,
                id: MAIN.into(),
                created: 0,
                meta: "{}".into(),
                delta: vec![],
            };
            disk::start_history(dir, crypt.as_deref(), &[main])?;
        }
        // only a manifest from before counters (format 6) can have them as rows of main; reading
        // those rows costs a page fetch, whole packs from a remote
        if old_counters {
            db.adopt_counters().map_err(io::Error::other)?;
        }
        if !clean {
            // Crashed: any branch may have lost writes that were never synced. Say so on merge
            // instead of merging less than the agent wrote. Logged, so it survives restarts.
            let names: Vec<String> = db.branches().into_iter().map(|(n, _)| n).filter(|n| n != MAIN).collect();
            let mut pos = 0;
            for n in names {
                db.branch(&n).unwrap().flagged.store(true, Ordering::Relaxed);
                pos = db.append(db.record(|| Op::Flag(n.as_str())));
            }
            if let Some(log) = &db.log {
                log.sync(pos);
            }
        }
        if let Some(log) = &db.log {
            log.live.store(true, Ordering::Relaxed); // opened: from here, dropping it closes it
        }
        Ok(db)
    }

    /// Applies one logged op again, without logging it: at recovery, and to rebuild the past.
    fn replay(&self, op: Op<String>) -> bool {
        // as nobody: these were accepted when they happened, so no agent's rights or quota apply
        // now (an agent rebuilding the past, for time travel or an undo, must get it exactly)
        let _nobody = disk::acting_as("");
        match op {
            Op::Write(b, rows) => {
                let rows: Vec<_> = rows.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
                self.write(&b, &rows).is_ok() // as accepted then: no rules again
            }
            Op::Fork(from, name) => self.fork_info(&from, &name, Info::new()).is_ok(),
            Op::World(from, name, id, created, meta) => {
                let meta = serde_json::from_str(&meta).unwrap_or_default();
                let info = Info { id, created, meta };
                match from.is_empty() {
                    true => self.empty_world(&name, info).is_ok(), // no source: an empty world
                    false => self.fork_info(&from, &name, info).is_ok(),
                }
            }
            Op::Meta(b, meta) => self
                .put_meta(&b, |m| {
                    *m = serde_json::from_str(&meta).unwrap_or_default();
                    Ok(())
                })
                .is_ok(),
            Op::Merge(b, how, expect) => self.merge_at(&b, how, expect).is_ok(),
            Op::MergeWith(b, how, expected, rows) => {
                let parent = self.branches.read().unwrap().get(&b).and_then(|e| e.parent.clone());
                let _g = self.gate();
                let o = MergeOptions { how, expected, ..Default::default() };
                let ok = self.merge_locked(b.as_str(), &o, &stored_picks(rows), Mode::Merge).is_ok();
                parent.inspect(|p| self.touch(p));
                ok
            }
            Op::Rebase(b, how, expected, rows) => {
                let _g = self.gate();
                let o = MergeOptions { how, expected, ..Default::default() };
                self.merge_locked(b.as_str(), &o, &stored_picks(rows), Mode::Rebase).is_ok()
            }
            Op::MergeSome { branch, into, how, expected, tables, keys, rows } => {
                let parent = self.branches.read().unwrap().get(&branch).and_then(|e| e.parent.clone());
                let _g = self.gate();
                let into = (!into.is_empty()).then_some(into);
                let target = into.clone().or(parent);
                let o =
                    MergeOptions { how, expected, only_tables: tables, only_keys: keys, into, ..Default::default() };
                let ok = self.merge_locked(&branch, &o, &stored_picks(rows), Mode::Merge).is_ok();
                target.inspect(|t| self.touch(t));
                ok
            }
            Op::Discard(b) => self.discard(&b).is_ok(),
            Op::Clock(c) => {
                self.replay_clock.store(c, Ordering::Relaxed); // what replayed ops count as their time
                true
            }
            Op::Close | Op::Actor(_) | Op::Synced(_) => true,
            Op::RebaseTx(b) => self.rebase_tx(&b).is_ok(),
            Op::Bulk(b, _, root, version) => match self.branch(&b) {
                // the statement's tree, whole (after its checkpoint's manifest, this changes nothing)
                Ok(br) => {
                    *br.snap.write().unwrap() = State { tree: Some(self.track(root)), over: Over::new() };
                    br.version.store(version, Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            },
            Op::Seq(name, high, origin) => {
                self.counters.lock().unwrap().insert(name, (high, origin));
                true
            }
            Op::Flag(b) => {
                if let Ok(br) = self.branch(&b) {
                    br.flagged.store(true, Ordering::Relaxed);
                }
                true
            }
        }
    }

    /// Every world as it was at `t` (ms since 1970): the newest checkpoint kept as history from
    /// before then, with the log replayed up to then, in a scratch database. Cached.
    // ponytail: replays up to a checkpoint's worth of log per new moment (up to 64 MB); keep more
    // checkpoints, or snapshots between them, if time travel queries need to start faster
    fn past(&self, t: u64) -> Result<Arc<Past>> {
        let Some(log) = &self.log else {
            return Err(no_history("time travel needs a database on disk; this one is in memory".into()));
        };
        if let Some(p) = self.past.lock().unwrap().iter().find(|p| p.at == t) {
            return Ok(p.clone());
        }
        let now = disk::now_ms();
        if t > now {
            return Err(no_history(format!("{} is in the future", show_ms(t))));
        }
        let hist = disk::history(&log.dir)?;
        let Some((seg, _, path)) = hist.iter().rev().find(|h| h.1 <= t) else {
            let from = hist.first().map_or(now, |h| h.1);
            return Err(no_history(format!("history starts at {}; see the retention setting", show_ms(from))));
        };
        let (_, _, man) = disk::read_manifest_at(path, log.crypt.as_deref())?
            .ok_or_else(|| no_history("history is missing".into()))?;
        let mut scratch = Core::new();
        scratch.pages = self.pages.clone();
        scratch.empty = self.empty;
        scratch.load(man);
        self.flush_log(log);
        // up to `t`; an open world's writes are logged in batches with the times they were made,
        // after records from later, but in the segment live when they were made (see `log_write`):
        // past `t`, read on to that segment's end for them
        let (mut after, mut passed) = (false, None);
        disk::read_log(&log.dir, log.crypt.as_deref(), *seg, |s, op| {
            if passed.is_some_and(|p| p != s) {
                return false;
            }
            if let Op::Clock(c) = &op {
                after = *c > t;
                if after {
                    passed.get_or_insert(s);
                }
            }
            if !after {
                scratch.replay(op);
            }
            true
        })?;
        let m = std::mem::take(scratch.branches.get_mut().unwrap());
        let worlds = m
            .by_name
            .into_iter()
            .map(|(n, e)| {
                // this database's own tree handles, so cleanup keeps their pages while cached
                let s = e.br.snapshot();
                let s = State { tree: s.tree.map(|t| self.track(*t.0)), over: s.over };
                (n, (s, e.br.info.read().unwrap().id.clone()))
            })
            .collect();
        let p = Arc::new(Past { at: t, worlds });
        let mut cache = self.past.lock().unwrap();
        cache.insert(0, p.clone());
        cache.truncate(PAST_CACHED);
        Ok(p)
    }

    /// How long history is kept for time travel, in ms.
    pub fn retention(&self) -> u64 {
        self.log.as_ref().map_or(0, |l| disk::retention(&l.dir))
    }

    /// The earliest moment time travel can read (ms since 1970): the oldest checkpoint kept as history. None in
    /// memory, or before the first checkpoint (see `past`).
    pub fn history_from(&self) -> Option<u64> {
        let log = self.log.as_ref()?;
        disk::history(&log.dir).ok()?.first().map(|h| h.1)
    }

    /// Sets how long history is kept for time travel (ms). Shorter settings free space at the
    /// next checkpoint; 0 keeps none.
    pub fn set_retention(&self, ms: u64) -> Result<()> {
        let Some(log) = &self.log else { return Err(no_history("an in-memory database keeps no history".into())) };
        disk::set_retention(&log.dir, ms)?;
        self.past.lock().unwrap().clear();
        Ok(())
    }

    /// A database-wide setting's value (see `Setting`).
    pub fn setting(&self, s: Setting) -> u64 {
        self.settings.lock().unwrap()[s as usize]
    }

    /// Changes a database-wide setting; on disk it's kept in the folder across restarts.
    pub fn set_setting(&self, s: Setting, v: u64) -> Result<()> {
        let mut all = self.settings.lock().unwrap(); // held while the file is written: one writer at a time
        let mut next = *all;
        next[s as usize] = v;
        if let Some(log) = &self.log {
            // the file's other entries (an older Chronos's `uniq_format`, or not numbers) stay
            let mut m = disk::settings(&log.dir)?;
            for s in Setting::ALL {
                m.insert(s.name().into(), next[s as usize].into());
            }
            disk::set_settings(&log.dir, m)?;
        }
        *all = next;
        Ok(())
    }

    /// Refuses a change to rows of `b`'s tables (`keys`) made straight into main while main is
    /// protected (`protect_main`): main's rows then change only by merging a world, so a
    /// mistaken `DELETE` there is a diff someone sees before it lands, never an incident.
    /// Definitions (CREATE TABLE, CREATE INDEX) and the database's own rows (agents, policies, the
    /// outbox) still change directly; so do the ways back (RESTORE, UNDO), and replaying the log.
    fn guard_main<'k>(&self, b: &str, mut keys: impl Iterator<Item = &'k str>) -> Result<()> {
        let protected = self.setting(Setting::ProtectMain) != 0 || self.attached.is_some();
        if b != MAIN || !protected || self.me.get().is_none() || RECOVERING.get() {
            return Ok(());
        }
        let row = |k: &&str| !reserved_table(k.split('/').next().unwrap_or(k)); // not the database's own
        match keys.find(row) {
            None => Ok(()),
            // an attached folder's main holds what Postgres has: it changes only by a merge written there first
            Some(k) if self.attached.is_some() => Err(Error::Constraint {
                code: "42501",
                message: format!(
                    "main is attached to Postgres ({}): its rows change only by merging a world, which writes to \
                     Postgres first, and this changes {}; fork a world, make the change there, and merge it",
                    self.attached.as_deref().map_or(String::new(), |a| a.source()),
                    sql::show_key(k)
                ),
            }),
            Some(k) => Err(Error::Constraint {
                code: "42501",
                message: format!(
                    "main is protected (protect_main): its rows change only by merging a world, and this changes {}; \
                     fork a world, make the change there, and merge it (or ALTER SYSTEM SET protect_main = off)",
                    sql::show_key(k)
                ),
            }),
        }
    }

    /// The Postgres database this folder is attached to, if it is (see `attach`).
    pub fn attached(&self) -> Option<&attach::Attached> {
        self.attached.as_deref()
    }

    /// Notes world `b` as in use now (see `Setting::WorldIdleTtl`).
    fn touch(&self, b: &str) {
        if let Ok(br) = self.branch(b) {
            br.active.store(self.now(), Ordering::Relaxed);
        }
    }

    /// The time for what's happening now: while replaying the log, the time it has reached.
    fn now(&self) -> u64 {
        match self.replay_clock.load(Ordering::Relaxed) {
            0 => disk::now_ms(),
            t => t,
        }
    }

    /// How far a commit to `main` pushes the log before it returns (see `SyncCommit`). An
    /// in-memory database writes nothing to disk: `Off`.
    pub fn sync_commit(&self) -> SyncCommit {
        self.log.as_ref().map_or(SyncCommit::Off, |l| l.sync_commit())
    }

    /// Sets how far commits to `main` push the log; kept in the folder across restarts.
    pub fn set_sync_commit(&self, m: SyncCommit) -> Result<()> {
        let Some(log) = &self.log else { return Err(no_history("an in-memory database has no log to sync".into())) };
        log.set_sync_commit(m)?;
        Ok(())
    }

    /// Folds every branch's recent changes into its tree, publishes the roots and drops the log
    /// before them. Runs on its own when the log passes 64 MB; call it to make reopening fast.
    pub fn checkpoint(&self) -> io::Result<()> {
        let Some(log) = &self.log else { return Ok(()) };
        let _one = log.ckpt.lock().unwrap();
        self.checkpoint_now(log, false)
    }

    /// The last checkpoint before closing: its manifest says nothing came after it.
    fn checkpoint_closing(&self) -> io::Result<()> {
        let Some(log) = &self.log else { return Ok(()) };
        let _one = log.ckpt.lock().unwrap();
        self.checkpoint_now(log, true)
    }

    fn checkpoint_now(&self, log: &disk::Log, closing: bool) -> io::Result<()> {
        self.checkpoint_with(log, closing, None).map(|_| ()).map_err(|e| match e {
            Error::Io(e) => e,
            e => io::Error::other(e.to_string()),
        })
    }

    /// A checkpoint; with `commit`, one that also installs a big statement's tree for its branch
    /// at the cut (see `commit_bulk`), returning the branch's new version.
    fn checkpoint_with(&self, log: &disk::Log, closing: bool, commit: Option<&Bulk>) -> Result<Option<u64>> {
        let _t = self.metrics.time(metrics::Op::Checkpoint);
        let pages = self.pages.as_ref().expect("a logged database has pages");
        let mut version = None;
        // The cut: with the gate held exclusively no mutation is mid-flight, and clones are O(1).
        // version, crash flag, world info and last activity per branch, at the cut
        let marks: HashMap<String, (u64, bool, Info, u64)>;
        let counters: disk::Counters;
        let (cut, first_seg) = {
            let _quiet = self.gate.write().unwrap();
            // a big statement's tree becomes its branch's state here, with the gate held, so
            // readers see all of it or none; no write to the branch can be under way (it holds
            // the branch's long lock), so the tree has everything the branch had
            let target = match commit {
                Some(c) => {
                    let br = self.branch(&c.branch)?;
                    if br.born != c.born || *br.writer.lock().unwrap() {
                        return Err(Error::NoBranch(c.branch.clone()));
                    }
                    Some((c, br))
                }
                None => None,
            };
            let m = self.branches.read().unwrap();
            // writes made while a segment is live are logged in it (time travel counts on it),
            // and the cut holds them: they must not be replayed on top of it
            for e in m.values() {
                self.flush_batch(&e.br);
            }
            let mut g = log.lock();
            // the log moves on first: if it can't, nothing has changed that readers could see
            let (at, behind) = log.rotate(&mut g)?;
            let installed = match target {
                Some((c, br)) => {
                    *br.snap.write().unwrap() = c.state.clone();
                    br.stamp.store(stamp(), Ordering::Relaxed);
                    br.changed.fetch_add(c.rows, Ordering::Relaxed);
                    br.active.store(self.now(), Ordering::Relaxed);
                    self.metrics.rows_written.fetch_add(c.rows, Ordering::Relaxed);
                    let v = br.version.fetch_add(1, Ordering::Relaxed) + 1;
                    version = Some(v);
                    Some((c, v))
                }
                None => None,
            };
            let cut: HashMap<String, (Option<String>, u64, State, State)> = m
                .iter()
                .map(|(n, e)| (n.clone(), (e.parent.clone(), e.br.born, e.br.base(), e.br.snapshot())))
                .collect();
            marks = m
                .iter()
                .map(|(n, e)| {
                    let info = e.br.info.read().unwrap().clone();
                    let (version, flagged) =
                        (e.br.version.load(Ordering::Relaxed), e.br.flagged.load(Ordering::Relaxed));
                    (n.clone(), (version, flagged, info, e.br.active.load(Ordering::Relaxed)))
                })
                .collect();
            // sequence counters are in no tree: the manifest holds them as they are at the cut
            counters = (self.counters.lock().unwrap().iter()).map(|(n, (h, o))| (n.clone(), *h, o.clone())).collect();
            // its note starts the new segment: with the tree's pages already on disk, it commits
            // the statement even if a crash comes before the manifest
            let note = installed.map(|(c, v)| {
                let root = *c.state.tree.as_ref().expect("an on-disk state has a tree").0;
                log.append(&mut g, &Op::Bulk(c.branch.as_str(), c.shown, root, v).encode())
            });
            (cut, (g.seg, at, note, behind))
        };
        let (first_seg, cut_ms, note, behind) = first_seg;
        if let Some(pos) = note {
            log.sync(pos); // readers see the statement now: so does the log, whatever comes next
            if let (Some(c), Some(v)) = (commit, version) {
                c.logged.set(Some(v));
            }
        }
        let mut kids: HashMap<&str, Vec<&str>> = HashMap::new();
        for (n, (p, ..)) in &cut {
            if let Some(p) = p {
                kids.entry(p.as_str()).or_default().push(n);
            }
        }
        for v in kids.values_mut() {
            v.sort_by_key(|n| std::cmp::Reverse(cut[*n].1)); // newest first
        }
        // main's tree first, then the trees of the databases with no parent (`create_empty`)
        let mut roots_of: Vec<&str> =
            cut.iter().filter(|(n, (p, ..))| p.is_none() && n.as_str() != MAIN).map(|(n, _)| n.as_str()).collect();
        roots_of.sort_by_key(|n| cut[*n].1);
        let (mut order, mut i) = (vec![MAIN], 0);
        order.extend(roots_of);
        while i < order.len() {
            order.extend(kids.get(order[i]).into_iter().flatten());
            i += 1;
        }
        // Fold each state relative to the nearest already-folded state on the same tree: a fork
        // point against the next-newer sibling's (or the parent's current state), a branch's data
        // against its own fork point. Neighbors differ by little, so this costs the size of the
        // changes, not branches x changes.
        //
        // A branch with few changes since its fork keeps them as a list in the manifest instead
        // of new pages: then its data is its fork point's tree plus that list (see `delta_of`).
        let mut roots: HashMap<&str, (Hash, Hash)> = HashMap::new();
        let mut deltas: HashMap<&str, Vec<(String, Option<Val>)>> = HashMap::new();
        let mut last_kid: HashMap<&str, &str> = HashMap::new();
        for &n in &order {
            let (parent, _, base, data) = &cut[n];
            let near = parent.as_deref().map(|p| match last_kid.insert(p, n) {
                Some(sib) => (&cut[sib].2, roots[sib].0),
                // the parent's data as folded; if it kept a list, its fork point is what's folded
                None if deltas.contains_key(p) => (&cut[p].2, roots[p].0),
                None => (&cut[p].3, roots[p].1),
            });
            let base_root = fold(pages, base, near)?;
            match delta_of(base, data) {
                Some(d) if n != MAIN => {
                    deltas.insert(n, d);
                    roots.insert(n, (base_root, base_root));
                }
                _ => {
                    let data_root = fold(pages, data, Some((base, base_root)))?;
                    roots.insert(n, (base_root, data_root));
                }
            }
        }
        pages.sync()?; // pages before the manifest that names them
        disk::crash_point();
        let man: Vec<disk::ManBranch> = order
            .iter()
            .map(|&n| disk::ManBranch {
                name: n.into(),
                parent: cut[n].0.clone(),
                born: cut[n].1,
                base: roots[n].0,
                data: roots[n].1,
                version: marks[n].0,
                flagged: marks[n].1,
                id: marks[n].2.id.clone(),
                created: marks[n].2.created,
                meta: {
                    // last activity rides along in the metadata on disk, unless it's the fork (see `load`)
                    let mut meta = marks[n].2.meta.clone();
                    if n != MAIN && marks[n].3 != marks[n].2.created {
                        meta.insert(agents::ACTIVE.into(), serde_json::json!(marks[n].3));
                    }
                    serde_json::Value::Object(meta).to_string()
                },
                delta: deltas
                    .get(n)
                    .map(|d| d.iter().map(|(k, v)| (k.clone(), v.as_deref().map(String::from))).collect())
                    .unwrap_or_default(),
            })
            .collect();
        disk::write_manifest(&log.dir, log.crypt.as_deref(), first_seg, &man, &counters, closing, cut_ms)?;
        log.move_anchor(first_seg, 0)?; // after the manifest: a crash between leaves the folder ahead, which opens
        self.past.lock().unwrap().clear(); // history may have been dropped: rebuild on demand
        if pages.remote().is_some() {
            let bytes = std::fs::read(log.dir.join("manifest"))?;
            pages.publish("manifest", bytes.clone())?; // after its pages; fenced
            remote::saw(&log.dir, &bytes)?;
        }
        log.published(behind);
        *self.published.lock().unwrap() = man.iter().flat_map(|b| [b.base, b.data]).collect();

        // Rebase live branches onto the new roots, keeping only changes made since the cut.
        let _quiet = self.gate.write().unwrap();
        for (n, e) in self.branches.read().unwrap().iter() {
            let Some(&(base_root, data_root)) = roots.get(n.as_str()) else { continue };
            if e.br.born != cut[n].1 {
                continue; // retired and re-forked under the same name since the cut
            }
            // a partial merge since the cut moved the fork point on for its rows: kept on top. A transaction
            // brought up to its world since (see `Core::rebase_own`) is on its world's state now, not on what was
            // folded: it stays there
            // (the state then the fork point, as writers take them)
            let mut s = e.br.snap.write().unwrap();
            let mut base = e.br.base.write().unwrap();
            // what's left on top of the new tree: changes since the cut, or with a list, since the fork
            let from = if deltas.contains_key(n.as_str()) { &cut[n].2 } else { &cut[n].3 };
            let (Some(base_over), Some(over)) = (grown(&cut[n].2, &base), grown(from, &s)) else { continue };
            *base = State { tree: Some(self.track(base_root)), over: base_over };
            *s = State { tree: Some(self.track(data_root)), over };
        }
        // what a transaction was last brought up to (see `rebase_tx`), when that was its world at the cut: the same
        // rows under the world's new tree, so that a world that wrote nothing since is still the state it saw (else
        // each next statement would bring the transaction up to the very rows it has, its own all read again)
        for (n, e) in self.branches.read().unwrap().iter() {
            let Some(p) = e.parent.as_deref() else { continue };
            let (Some(c), Some(pc), Some(&(_, p_root))) = (cut.get(n), cut.get(p), roots.get(p)) else { continue };
            if e.br.born != c.1 || deltas.contains_key(p) {
                continue;
            }
            let mut synced = lock(&e.br.synced);
            if synced.same(&pc.3) {
                *synced = State { tree: Some(self.track(p_root)), over: Over::new() };
            }
        }
        drop(_quiet);
        if pages.should_gc() {
            self.gc_now(pages)?;
        }
        // overlays folded into pages, and worlds discarded since the last one, leave memory free
        // that glibc's malloc keeps for itself otherwise (100,000 worlds discarded: 1 GB of 1.05)
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        unsafe {
            libc::malloc_trim(0);
        }
        Ok(version)
    }

    /// Where a statement too big to hold keeps its writes until it commits (see `sql::bulk`), and
    /// the key they're sealed with, if the database has one. None in memory. Emptied at open.
    pub(crate) fn spill_dir(&self) -> Option<(std::path::PathBuf, Option<Arc<crypt::Crypt>>)> {
        self.log.as_ref().map(|l| (l.dir.join("spill"), l.crypt.clone()))
    }

    /// Commits a statement too big to hold in memory to branch `b` (see `sql::bulk`): `start` is
    /// the branch as the statement found it, and `next` hands out the statement's changes in key
    /// order, a batch at a time. The caller holds the branch's long lock, so nothing else has
    /// written it since `start`. Each batch goes into a new tree built beside the branch's, its
    /// pages written but seen by no one; then a checkpoint makes that tree the branch's state
    /// (and its manifest the commit record, with an `Op::Bulk` note ahead of it). Memory holds a
    /// batch, and nothing waits in the overlay afterwards. Returns the branch's version.
    pub(crate) fn commit_bulk(
        &self,
        start: &View,
        rows: u64,
        shown: u64,
        from: Option<Tree>,
        next: &mut dyn FnMut() -> Result<Option<Changes>>,
    ) -> Result<u64> {
        let (log, pages) = self.bulk_store()?;
        let b = start.branch.as_str();
        if let Some(q) = self.quota(b)? {
            q.check(b, self.branch(b)?.changed.load(Ordering::Relaxed), rows)?;
        }
        // pages are written under the checkpoint lock, as a checkpoint's are, so cleanup never
        // runs between writing them and holding the tree they make
        let mut tree = match from {
            Some(t) => t,
            None => {
                let _one = log.ckpt.lock().unwrap();
                self.track(fold(pages, &start.s, None)?)
            }
        };
        while let Some(batch) = next()? {
            if batch.is_empty() {
                continue;
            }
            self.guard_main(b, batch.iter().map(|(k, _)| k.as_str()))?;
            let _one = log.ckpt.lock().unwrap();
            tree = self.track(prolly::apply(pages, &tree.0, &batch)?);
        }
        pages.sync()?; // before the note that names the tree
        let commit = Bulk {
            branch: b.to_string(),
            born: start.born,
            state: State { tree: Some(tree), over: Over::new() },
            rows,
            shown,
            logged: Default::default(),
        };
        let _one = log.ckpt.lock().unwrap();
        match self.checkpoint_with(log, false, Some(&commit)) {
            Ok(version) => Ok(version.expect("a commit gives a version")),
            // committed by its note, and readers see it: the rest of the checkpoint (a manifest,
            // an upload) is retried as a failed background one is, not the statement
            Err(e) => match commit.logged.get() {
                Some(version) => {
                    log.failed(&e);
                    Ok(version)
                }
                None => Err(e),
            },
        }
    }

    /// A big statement's part applied to the tree it's building beside `start`'s (`start`'s own
    /// tree for the first): `batch` is sorted, and its pages are written now (see `sql::bulk`).
    pub(crate) fn bulk_apply(&self, start: &View, tree: Option<Tree>, batch: &Changes) -> Result<Tree> {
        let (log, pages) = self.bulk_store()?;
        let _one = log.ckpt.lock().unwrap();
        let root = match &tree {
            Some(t) => *t.0,
            None => fold(pages, &start.s, None)?,
        };
        Ok(self.track(prolly::apply(pages, &root, batch)?))
    }

    /// A key in a tree `bulk_apply` made.
    pub(crate) fn bulk_get(&self, tree: &Tree, key: &str) -> Result<Option<Val>> {
        let (_, pages) = self.bulk_store()?;
        Ok(prolly::get(pages, &tree.0, key)?)
    }

    fn bulk_store(&self) -> Result<(&disk::Log, &Pages)> {
        match (&self.log, &self.pages) {
            (Some(log), Some(pages)) => Ok((log, pages)),
            _ => Err(Error::Constraint {
                code: "XX000",
                message: "a statement spilled in a database with no log".into(),
            }),
        }
    }

    /// Deletes pages that no branch, no in-flight reader and not the checkpoint on disk still
    /// needs. Runs on its own after a checkpoint once the page files have doubled since the last
    /// cleanup. Returns (pages dropped, bytes freed).
    pub fn gc(&self) -> io::Result<(usize, u64)> {
        let (Some(log), Some(pages)) = (&self.log, &self.pages) else { return Ok((0, 0)) };
        let _one = log.ckpt.lock().unwrap(); // checkpoints are the only thing that makes new trees
        self.gc_now(pages)
    }

    fn gc_now(&self, pages: &Pages) -> io::Result<(usize, u64)> {
        let _t = self.metrics.time(metrics::Op::Gc);
        self.dropped.0.store(0, Ordering::Relaxed); // what was discarded so far is dead from here on
        self.dropped.1.store(0, Ordering::Relaxed);
        let (mut roots, history) = self.roots()?;
        roots.extend(history);
        let live = prolly::reachable(pages, &roots)?;
        pages.compact(&live)
    }

    /// Every tree root still needed: the checkpoint's and every tree in use, then history's.
    fn roots(&self) -> io::Result<(Vec<Hash>, Vec<Hash>)> {
        let mut roots = self.published.lock().unwrap().clone();
        let mut history = vec![];
        if let Some(log) = &self.log {
            // checkpoints kept as history: time travel starts from them
            for (_, _, path) in disk::history(&log.dir)? {
                if let Some((_, _, man)) = disk::read_manifest_at(&path, log.crypt.as_deref())? {
                    history.extend(man.iter().flat_map(|b| [b.base, b.data]));
                }
            }
        }
        {
            let mut trees = self.trees.lock().unwrap();
            trees.retain(|t| t.strong_count() > 0);
            roots.extend(trees.iter().filter_map(Weak::upgrade).map(|h| *h));
        }
        roots.extend(self.empty);
        Ok((roots, history))
    }

    /// What the pages on disk hold: pages in use, pages only history keeps (for its retention
    /// window), and pages nothing needs, which cleanup frees; and the log's bytes. Reads every
    /// tree's inner pages, as cleanup does.
    pub fn disk_usage(&self) -> io::Result<DiskUsage> {
        let (Some(log), Some(pages)) = (&self.log, &self.pages) else { return Ok(DiskUsage::default()) };
        let _one = log.ckpt.lock().unwrap(); // no checkpoint or cleanup changes the pages meanwhile
        let (roots, history) = self.roots()?;
        let live = prolly::reachable(pages, &roots)?;
        let kept = prolly::reachable(pages, &history)?;
        let (live, history, total) = pages.split(&live, &kept);
        let size = |p: &Path| -> u64 {
            std::fs::read_dir(p).map_or(0, |d| {
                d.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
            })
        };
        let wal: u64 = disk::segments(&log.dir)?
            .into_iter()
            .map(|s| disk::seg_path(&log.dir, s).metadata().map_or(0, |m| m.len()))
            .sum();
        Ok(DiskUsage {
            live,
            history,
            reclaimable: total.saturating_sub(live + history),
            log: wal + size(&log.dir.join("history")),
        })
    }

    /// The object storage URL this folder is attached to, if any.
    pub fn remote(&self) -> Option<&str> {
        self.pages.as_ref()?.remote().map(|r| r.url())
    }

    /// Whether the folder is encrypted (opened with a key; see `crypt`).
    pub fn encrypted(&self) -> bool {
        self.log.as_ref().is_some_and(|l| l.crypt.is_some())
    }

    /// Deletes local copies of pages the remote already holds; they are downloaded again when
    /// read. Returns bytes freed. Scale to zero: checkpoint, evict, and the folder is just a cache.
    pub fn evict_local(&self) -> io::Result<u64> {
        match &self.pages {
            Some(p) => p.evict_local(),
            None => Ok(0),
        }
    }

    fn track(&self, h: Hash) -> Tree {
        let t = Tree(Arc::new(h));
        let mut trees = self.trees.lock().unwrap();
        if trees.len() == trees.capacity() {
            trees.retain(|t| t.strong_count() > 0); // before growing: dead handles still hold memory
        }
        trees.push(Arc::downgrade(&t.0));
        t
    }

    /// The tree under `s`, unless it is the empty tree, which lookups can skip.
    fn tree<'a>(&self, s: &'a State) -> Option<&'a Hash> {
        s.tree.as_ref().map(|t| &*t.0).filter(|h| Some(**h) != self.empty)
    }

    fn load(&self, mut man: Vec<disk::ManBranch>) {
        let mut m = self.branches.write().unwrap();
        for e in m.values() {
            e.br.retired.store(true, Ordering::Release);
        }
        *m = Branches::default();
        let mut next = 1;
        man.sort_by_key(|b| b.born); // parents before their forks, so each is counted
        for b in man {
            next = next.max(b.born + 1);
            self.published.lock().unwrap().extend([b.base, b.data]);
            let mut meta: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&b.meta).unwrap_or_default();
            let active = meta.remove(agents::ACTIVE).and_then(|v| v.as_u64());
            let info = match b.id.is_empty() {
                true if b.name == MAIN => main_info(),
                true => Info::new(), // from before worlds had IDs
                false => Info { id: b.id, created: b.created, meta },
            };
            let base = State::empty(Some(self.track(b.base)));
            let over: Over = b.delta.into_iter().map(|(k, v)| (k, v.map(Val::from))).collect();
            let data = State { tree: Some(self.track(b.data)), over };
            let br = Branch::new(b.born, base, data, info);
            br.version.store(b.version, Ordering::Relaxed);
            br.flagged.store(b.flagged, Ordering::Relaxed);
            if let Some(t) = active {
                br.active.store(t, Ordering::Relaxed);
            }
            m.add(b.name, b.parent, br);
        }
        self.forks.store(next, Ordering::Relaxed);
    }

    /// Branch names with their parents, sorted by name.
    pub fn branches(&self) -> Vec<(String, Option<String>)> {
        let m = self.branches.read().unwrap();
        let mut v: Vec<_> = m.iter().map(|(n, e)| (n.clone(), e.parent.clone())).collect();
        v.sort();
        v
    }

    /// Every world with its ID, lineage and metadata, sorted by name.
    pub fn worlds(&self) -> Vec<World> {
        let m = self.branches.read().unwrap();
        let mut v: Vec<World> = m.iter().map(|(n, e)| world_of(n, e)).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Whether `name_or_id` is a session's transaction world, which only that session may reach.
    pub fn private_world(&self, name_or_id: &str) -> bool {
        let m = self.branches.read().unwrap();
        let name =
            if m.contains_key(name_or_id) { name_or_id } else { m.ids.get(name_or_id).map_or("", |n| n.as_str()) };
        name.starts_with("_tx_")
    }

    /// One world, by name or by ID.
    pub fn world(&self, name_or_id: &str) -> Result<World> {
        let m = self.branches.read().unwrap();
        let name = match m.contains_key(name_or_id) {
            true => name_or_id,
            false => m.ids.get(name_or_id).ok_or_else(|| Error::NoBranch(name_or_id.into()))?,
        };
        not_tx(name).map_err(|_| Error::NoBranch(name_or_id.into()))?;
        Ok(world_of(name, &m[name]))
    }

    /// Changes a world's metadata: each key in `patch` (a JSON object) is set, or removed when
    /// null. Returns the world.
    pub fn set_meta(&self, b: &str, patch: serde_json::Value) -> Result<World> {
        let mut patch = meta_object(patch)?;
        reserved_keys(&patch)?;
        let br = self.branch(b)?; // a past state (`name@when`) says so
        not_tx(b)?;
        if let Some(on) = patch.remove("check_reads") {
            self.check_reads(b, &br, on)?;
        }
        // the scope and the rest in one write: all of it, or (if any of it is refused) none. A
        // scope change waits for a merge of the world in progress (merges hold its writer lock
        // from reading the scope to committing), so none commits under a scope already changed
        let rescoped = match scope::declared(&patch) {
            true => Some(self.rescope(b, &mut patch)?),
            false => None,
        };
        let _g = self.gate(); // the gate, then the world's writer: the order merges take them in
        let writer = rescoped.as_ref().map(|_| lock(&br.writer));
        if writer.as_ref().is_some_and(|w| **w) {
            return Err(Error::NoBranch(b.into())); // merged or discarded meanwhile
        }
        let pos = self.put_meta_in(b, false, |meta| {
            if let Some((was, new)) = rescoped {
                if meta.get(scope::SCOPE) != was.as_ref() {
                    return Err(Error::Constraint {
                        code: "40001", // Postgres's serialization_failure: try again
                        message: format!("{b}'s scope was changed while this changed it; nothing was changed"),
                    });
                }
                match new {
                    serde_json::Value::Null => meta.remove(scope::SCOPE),
                    v => meta.insert(scope::SCOPE.into(), v),
                };
            }
            for (k, v) in patch {
                match v {
                    serde_json::Value::Null => meta.remove(&k),
                    v => meta.insert(k, v),
                };
            }
            Ok(())
        })?;
        drop((writer, _g));
        self.after(pos, false);
        self.world(b)
    }

    /// World `b`'s declared scope with the scope keys in `patch` (taken out of it) applied: the
    /// scope it had, and the one to write (null: none). Only the database's own users and admins
    /// may change it, as the scope is what its merge is held to.
    fn rescope(
        &self,
        b: &str,
        patch: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Option<serde_json::Value>, serde_json::Value)> {
        let a = disk::actor();
        if !a.is_empty() && !self.agent(&a).is_ok_and(|x| x.may(agents::Can::Admin)) {
            return Err(Error::Constraint {
                code: "42501",
                message: format!(
                    "agent {a} may not change {b}'s scope: it's what the world's merge is held to, so only an admin may"
                ),
            });
        }
        let given: serde_json::Map<String, serde_json::Value> = ["may_change", "tenant", "intent", "run"]
            .iter()
            .filter_map(|k| Some((k.to_string(), patch.remove(*k)?)))
            .collect();
        let was = self.world(b)?.scope;
        let old = was.as_ref().and_then(|s| s.as_object().cloned()).unwrap_or_default();
        let new = scope::resolve(&self.view(b)?, &old, given, b)?;
        Ok((was, new))
    }

    /// Turns checking world `b`'s reads on (only before it changes anything: what it read before
    /// isn't known) or off (not by an agent whose merge policy checks its reads).
    fn check_reads(&self, b: &str, br: &Branch, on: serde_json::Value) -> Result<()> {
        match on {
            serde_json::Value::Bool(true) => {
                if br.version.load(Ordering::Relaxed) > 0 {
                    return Err(bad_meta(format!(
                        "{b} has changed since its fork, and what it read before isn't known: set check_reads when forking (CREATE WORLD w WITH (check_reads = true))"
                    )));
                }
                self.set_reserved(b, stale::CHECK_READS, Some(serde_json::Value::Bool(true)))?;
                *br.reads.write().unwrap() = Some(Arc::new(Mutex::new(stale::ReadSet::complete())));
            }
            serde_json::Value::Bool(false) => {
                if let Some(p) = self.acting_policy()?.filter(|p| p.check_reads) {
                    return Err(Error::Constraint {
                        code: "42501",
                        message: format!(
                            "merge policy {} checks its agents' reads; a person can turn check_reads off",
                            p.name
                        ),
                    });
                }
                self.set_reserved(b, stale::CHECK_READS, None)?;
                *br.reads.write().unwrap() = None;
            }
            _ => return Err(bad_meta("check_reads must be true or false".into())),
        }
        Ok(())
    }

    /// Discards world `b` at `at` (ms since 1970), or never with None. The background worker
    /// discards it once the time is up, unless it has forks of its own then.
    pub fn expire(&self, b: &str, at: Option<u64>) -> Result<World> {
        if b == MAIN {
            return Err(bad_meta("main can't expire".into()));
        }
        self.set_reserved(b, agents::EXPIRES, at.map(|t| serde_json::json!(t)))?;
        self.world(b)
    }

    /// Pins world `b` so it's never discarded for being idle (see `Setting::WorldIdleTtl`), or
    /// unpins it. Kept in its metadata, so it survives restarts.
    pub fn pin(&self, b: &str, pinned: bool) -> Result<World> {
        if b == MAIN {
            return Err(bad_meta("main is never discarded, so it needs no pin".into()));
        }
        self.set_reserved(b, agents::PINNED, pinned.then_some(serde_json::Value::Bool(true)))?;
        self.world(b)
    }

    /// Sets (or with None removes) a metadata key only the database sets (see `agents`).
    pub(crate) fn set_reserved(&self, b: &str, key: &str, v: Option<serde_json::Value>) -> Result<()> {
        let b = self.world(b)?.name;
        self.put_meta(&b, |meta| {
            match v {
                Some(v) => meta.insert(key.into(), v),
                None => meta.remove(key),
            };
            Ok(())
        })
    }

    /// The fork counter now: a world forked before this moment has a lower `born` (see
    /// `sql::advisory`).
    pub(crate) fn fork_clock(&self) -> u64 {
        self.forks.load(Ordering::SeqCst)
    }

    /// World `b`'s place in fork order, and whether anything was written to it since its fork,
    /// or any row locked in it (SELECT ... FOR UPDATE): what a fresh fork of it would lose.
    pub(crate) fn born_written(&self, b: &str) -> Result<(u64, bool)> {
        let br = self.branch(b)?;
        Ok((br.born, br.version.load(Ordering::Relaxed) != 0 || !lock(&br.locked).is_empty()))
    }

    /// The fork order of the oldest open transaction, if any.
    pub(crate) fn oldest_tx(&self) -> Option<u64> {
        self.branches.read().unwrap().iter().filter(|(n, _)| n.starts_with("_tx_")).map(|(_, e)| e.br.born).min()
    }

    /// Rows written to world `b` since its fork (as counted since the database opened).
    /// Rows written to `b` since its fork. For a world from before this process opened the
    /// database, the first call adds what it differs from its fork by (so a restart doesn't reset
    /// agents' quotas); rewrites of one row before then count once.
    pub(crate) fn changed(&self, b: &str) -> Result<u64> {
        let br = self.branch(b)?;
        if !br.counted.load(Ordering::Relaxed) {
            let n = self.state_diff(&br.base(), &br.snapshot())?.len() as u64;
            br.changed.fetch_max(n, Ordering::Relaxed);
            br.counted.store(true, Ordering::Relaxed);
        }
        Ok(br.changed.load(Ordering::Relaxed))
    }

    /// Takes one write from agent `name`'s allowance of `per_minute`; false if it's used up.
    pub(crate) fn take_write(&self, name: &str, per_minute: u64) -> bool {
        let now = disk::now_ms();
        let mut rates = lock(&self.rates);
        let (tokens, last) = rates.entry(name.into()).or_insert((per_minute as f64, now));
        // a clock that went back refills nothing
        *tokens = (*tokens + now.saturating_sub(*last) as f64 * per_minute as f64 / 60_000.0).min(per_minute as f64);
        *last = now;
        if *tokens < 1.0 {
            return false;
        }
        *tokens -= 1.0;
        true
    }

    /// Forks with metadata that may hold the database's own keys (an agent's world: its owner).
    pub(crate) fn fork_owned(
        &self,
        from: &str,
        name: &str,
        meta: serde_json::Map<String, serde_json::Value>,
    ) -> Result<World> {
        check_name(name)?;
        let from = self.world(from)?.name;
        self.fork_info(&from, name, Info { meta, ..Info::new() })?;
        self.world(name)
    }

    /// Changes a world's metadata with `edit`, and logs it: read, changed and written under the
    /// world's lock, so concurrent changes never lose each other's keys. Nothing is written if
    /// `edit` fails.
    fn put_meta(
        &self,
        b: &str,
        edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<()>,
    ) -> Result<()> {
        let pos = self.put_meta_in(b, true, edit)?;
        self.after(pos, false);
        Ok(())
    }

    /// `put_meta` without the `after`, which the caller calls once it holds no locks; `gated`:
    /// false when the caller holds the gate already. Returns the log position.
    fn put_meta_in(
        &self,
        b: &str,
        gated: bool,
        edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<()>,
    ) -> Result<u64> {
        let pos = {
            let _g = gated.then(|| self.gate());
            // the log, then the map (the order forks and retires take them in); the map held through
            // the append: orders us against retires
            let mut g = self.log.as_ref().map(|l| l.lock());
            let m = self.branches.read().unwrap();
            let e = m.get(b).ok_or_else(|| Error::NoBranch(b.into()))?;
            let mut info = e.br.info.write().unwrap();
            let mut meta = info.meta.clone();
            edit(&mut meta)?;
            let text = serde_json::Value::Object(meta.clone()).to_string();
            if text.len() > META_MAX {
                return Err(bad_meta(format!("a world's metadata is limited to {META_MAX} bytes as JSON")));
            }
            let rec = self.record(|| Op::Meta(b, text.as_str()));
            info.meta = meta;
            self.append_in(&mut g, rec)
        };
        Ok(pos)
    }

    /// The live branch named `b`. A thread's last few come from a cache without the branch map's
    /// lock: with a thousand agents each writing its own world, the lock's readers queued behind
    /// every fork and merge taking it to write. An agent goes between its world and main (fork,
    /// write, merge), so a few are kept, not one. Cached is as good as in the map: a branch leaves
    /// the map only through `Branches::take` or `load`, which mark it retired first.
    fn branch(&self, b: &str) -> Result<Arc<Branch>> {
        /// Branches a thread keeps.
        const KEPT: usize = 4;
        // out of the cell before the flag is read: under shuttle that read lets other threads run
        let hit = LAST.with(|l| {
            l.borrow().iter().find(|(uid, name, _)| *uid == self.uid && name == b).map(|(.., br)| br.clone())
        });
        if let Some(br) = hit.and_then(|w| w.upgrade()).filter(|br| !br.retired.load(Ordering::Acquire)) {
            return Ok(br);
        }
        let m = self.branches.read().unwrap();
        if let Some(e) = m.get(b) {
            LAST.with(|l| {
                let mut l = l.borrow_mut();
                l.retain(|(uid, name, _)| !(*uid == self.uid && name == b)); // a retired one of the name
                l.insert(0, (self.uid, b.to_string(), Arc::downgrade(&e.br)));
                l.truncate(KEPT);
            });
            return Ok(e.br.clone());
        }
        Err(match b.split_once('@') {
            // `name@when` is the past: read with `view`, never changed
            Some((name, _)) if m.contains_key(name) => Error::Constraint {
                code: "25006",
                message: format!("{b} is a past state of {name}: it can be read, not changed"),
            },
            _ => Error::NoBranch(b.into()),
        })
    }

    /// Held (shared) for the whole of a mutation when the database is on disk.
    fn gate(&self) -> Option<RwLockReadGuard<'_, ()>> {
        self.log.as_ref().map(|_| self.gate.read().unwrap())
    }

    /// Encodes a log record, but only when there is a log. Call before taking locks.
    fn record<S: AsRef<str>>(&self, op: impl FnOnce() -> Op<S>) -> Option<Vec<u8>> {
        self.log.as_ref().map(|_| op().encode())
    }

    /// Appends a record; the caller must still hold the locks it changed state under.
    /// Returns the log position to sync to.
    fn append(&self, rec: Option<Vec<u8>>) -> u64 {
        match (&self.log, rec) {
            (Some(log), Some(rec)) => log.append(&mut log.lock(), &rec),
            _ => 0,
        }
    }

    /// `append`, with the log's lock held already (`g`, taken from `self.log`).
    fn append_in(&self, g: &mut Option<MutexGuard<'_, disk::Inner>>, rec: Option<Vec<u8>>) -> u64 {
        match (&self.log, g, rec) {
            (Some(log), Some(g), Some(rec)) => log.append(g, &rec),
            _ => 0,
        }
    }

    /// `flush_batch`, with the log's lock held already; take `br`'s batch lock before it.
    fn flush_batch_in(&self, g: &mut Option<MutexGuard<'_, disk::Inner>>, w: &mut disk::Batch) {
        if let (Some(log), Some(g)) = (&self.log, g)
            && !w.is_empty()
        {
            log.append_batch(g, w);
        }
    }

    /// Logs a write to branch `b`; call holding its writer lock and snap lock (for writing). A
    /// write to main is logged now. Another world's waits in its batch with its others, to go
    /// in the log as one record, times and all (a log shared by a thousand agents writing row by
    /// row spent most of its time handing its lock over): at the latest when something that
    /// depends on them is logged (a fork from the world, a merge of or into it, a discard), a
    /// write by another agent, a checkpoint's cut, a reader of the log, or closing. Returns the
    /// log position to sync to (0 while it waits: worlds become durable when they reach main).
    fn log_write(&self, b: &str, br: &Branch, rec: Option<Vec<u8>>) -> u64 {
        let (Some(log), Some(rec)) = (&self.log, rec) else { return 0 };
        if b == MAIN {
            return log.append(&mut log.lock(), &rec);
        }
        let mut w = lock(&br.batch);
        if !w.takes(&rec) {
            log.append_batch(&mut log.lock(), &mut w);
        }
        if rec.len() > disk::BATCH_BYTES {
            return log.append(&mut log.lock(), &rec); // big: on its own, as it came
        }
        w.push(b, &rec);
        0
    }

    /// Logs the writes waiting in `br`'s batch (see `log_write`).
    fn flush_batch(&self, br: &Branch) {
        if let Some(log) = &self.log {
            let mut w = lock(&br.batch);
            if !w.is_empty() {
                log.append_batch(&mut log.lock(), &mut w);
            }
        }
    }

    /// Logs every world's waiting writes; hold no branch lock.
    pub(crate) fn flush_batches(&self) {
        let all: Vec<Arc<Branch>> = self.branches.read().unwrap().values().map(|e| e.br.clone()).collect();
        for br in all {
            self.flush_batch(&br);
        }
    }

    /// Makes the log whole in its file, every world's waiting writes too: for readers of it.
    pub(crate) fn flush_log(&self, log: &disk::Log) {
        self.flush_batches();
        log.flush();
    }

    /// Call with no locks held: fsyncs if `durable`; if the log has grown, asks the background
    /// worker for a checkpoint. Writers that get far ahead of it wait for one (backpressure).
    fn after(&self, pos: u64, durable: bool) {
        let Some(log) = &self.log else { return };
        if durable {
            let _t = self.metrics.time(metrics::Op::Sync);
            log.sync(pos);
        }
        if !log.needs_checkpoint() {
            return;
        }
        match &self.kick {
            Some(k) if !log.far_behind() => {
                let _ = k.try_send(());
            }
            _ => {
                let _one = log.ckpt.lock().unwrap();
                // this writer's record is logged already, so its commit stands: a checkpoint
                // that fails is said, and a write after the wait (see `retry_due`) tries again.
                // Past the ceiling (checkpoints keep failing: remote down, disk full) writers wait
                // here for one to succeed, so the log can't grow without limit.
                loop {
                    if log.needs_checkpoint() && log.retry_due() {
                        match self.checkpoint_now(log, false) {
                            Ok(_) => break,
                            Err(e) => log.failed(&e),
                        }
                    }
                    if !log.over_ceiling() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }

    /// The value of `k` in `s`: the overlay if it has the key, else the tree.
    fn val(&self, s: &State, k: &str) -> Result<Option<Val>> {
        if let Some(v) = s.over.get(k) {
            return Ok(v.clone());
        }
        match (self.tree(s), &self.pages) {
            (Some(t), Some(p)) => Ok(prolly::get(p, t, k)?),
            _ => Ok(None),
        }
    }

    /// Rows of `s` with key >= `from`, in key order, merging the overlay over the tree lazily.
    fn rows<'a>(&'a self, s: &'a State, from: &str) -> Result<Rows<'a>> {
        let tree = match (self.tree(s), &self.pages) {
            (Some(t), Some(p)) => Some(prolly::range(p, t, from)?.peekable()),
            _ => None,
        };
        let over: OverIter = Box::new(s.over.range::<_, str>(from_key(from)));
        Ok(Rows { over: over.peekable(), tree })
    }

    /// Row-level differences from `a` to `b`, in key order (see `state_diff_iter`).
    fn state_diff(&self, a: &State, b: &State) -> Result<Vec<Change>> {
        self.state_diff_iter(a, b, None)?.collect()
    }

    /// Row-level differences from `a` to `b` with key > `after`, in key order, read lazily. Only
    /// keys the overlays disagree on, or that sit in tree pages the two trees don't share, can
    /// differ, so that's all this reads; it holds no more than one change at a time.
    // ponytail: `after` skips the overlays' diff by walking it (no rows kept); the tree side prunes
    fn state_diff_iter<'a>(&'a self, a: &'a State, b: &'a State, after: Option<&'a str>) -> Result<DiffRows<'a>> {
        let past = move |k: &str| after.is_some_and(|x| k <= x);
        let dkey = |d: &DiffItem<'a, 'a, Val, Option<Val>>| -> &'a Val {
            match *d {
                DiffItem::Add(k, _) | DiffItem::Remove(k, _) | DiffItem::Update { new: (k, _), .. } => k,
            }
        };
        if a.tree == b.tree {
            // one tree under both (always in memory): the overlays' diff, in key order, holds both
            // values, except where a side has no overlay entry and so reads the tree
            let under = move |s: &State, k: &str| -> Result<Option<Val>> {
                match (self.tree(s), &self.pages) {
                    (Some(t), Some(p)) => Ok(prolly::get(p, t, k)?),
                    _ => Ok(None),
                }
            };
            let it = a.over.diff(&b.over).skip_while(move |d| past(dkey(d))).filter_map(move |d| {
                let r = (|| {
                    let (key, before, after) = match d {
                        DiffItem::Update { old: (k, x), new: (_, y) } => (k, x.clone(), y.clone()),
                        DiffItem::Add(k, y) => (k, under(a, k)?, y.clone()),
                        DiffItem::Remove(k, x) => (k, x.clone(), under(b, k)?),
                    };
                    Ok((before != after).then(|| Change { key: key.to_string(), before, after }))
                })();
                r.transpose()
            });
            return Ok(Box::new(it));
        }
        let mut over = a.over.diff(&b.over).map(move |d| dkey(&d)).skip_while(move |k| past(k)).peekable();
        let mut tree = match (&a.tree, &b.tree, &self.pages) {
            (Some(ta), Some(tb), Some(p)) => Some(prolly::diff_from(p, &ta.0, &tb.0, after)?),
            _ => None,
        };
        let mut held: Option<String> = None; // the tree side's next key
        // both sides' keys, merged in order without repeats
        let keys = std::iter::from_fn(move || -> Option<Result<String>> {
            if held.is_none()
                && let Some(t) = tree.as_mut()
            {
                match t.next() {
                    Some(Ok(c)) => held = Some(c.key),
                    Some(Err(e)) => return Some(Err(e.into())),
                    None => tree = None,
                }
            }
            match (over.peek(), &held) {
                (None, None) => None,
                (Some(k), Some(t)) if k.as_ref() == t.as_str() => {
                    over.next();
                    held.take().map(Ok)
                }
                (Some(k), t) if t.as_ref().is_none_or(|t| k.as_ref() < t.as_str()) => {
                    over.next().map(|k| Ok(k.to_string()))
                }
                _ => held.take().map(Ok),
            }
        });
        let it = keys.filter_map(move |key| {
            let r = key.and_then(|key| {
                let (before, after) = (self.val(a, &key)?, self.val(b, &key)?);
                Ok((before != after).then_some(Change { key, before, after }))
            });
            r.transpose()
        });
        Ok(Box::new(it))
    }

    /// Writes rows to a branch atomically: all or none survive a crash. `None` deletes.
    /// One log record and at most one fsync, however many rows. Returns the branch's new version:
    /// pass it to `merge_at` to be sure none of these writes was lost in a crash.
    ///
    /// Rows of SQL tables (made with CREATE TABLE) must fit their table, and are checked against
    /// its UNIQUE and foreign-key constraints like an INSERT (see `sql::json_writes`).
    pub fn batch(&self, b: &str, rows: &[(&str, Option<&str>)]) -> Result<u64> {
        if let Some((k, _)) = rows.iter().find(|(k, _)| k.starts_with("_sonos_")) {
            return Err(Error::Constraint {
                code: "42501",
                message: format!("{k}: keys starting with _sonos_ are SQL's own bookkeeping; change them with SQL"),
            });
        }
        // an attached folder has Postgres's tables only: a row of any other would make a table of its own here
        if let Some(att) = self.attached.as_deref()
            && let Some((k, _)) =
                rows.iter().find(|(k, _)| att.table(k.split_once('/').map_or(*k, |(t, _)| t)).is_none())
        {
            return Err(Error::Constraint {
                code: "0A000",
                message: format!(
                    "{k}: not a row of a table attached to Postgres ({}); an attached folder has no tables of its own",
                    att.source()
                ),
            });
        }
        // keys typed as `orders/5` for tables whose integer keys are in number order
        let typed: Vec<(String, Option<&str>)> = if rows
            .iter()
            .any(|(k, _)| k.rsplit('/').next().is_some_and(|id| id.parse::<i64>().is_ok() || sql::may_be_encoded(id)))
        {
            let s = self.view_tx(b)?.s;
            rows.iter().map(|(k, v)| Ok((self.typed(&s, k)?.into_owned(), *v))).collect::<Result<_>>()?
        } else {
            vec![]
        };
        let typed: Vec<(&str, Option<&str>)> = typed.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let rows = if typed.is_empty() { rows } else { &typed[..] };
        if let Some(version) = sql::json_writes(self, b, rows)? {
            return Ok(version);
        }
        self.write(b, rows)
    }

    /// `batch` without SQL's rules: what log replay uses.
    fn write(&self, b: &str, rows: &[(&str, Option<&str>)]) -> Result<u64> {
        self.guard_main(b, rows.iter().map(|(k, _)| *k))?;
        self.note_sql(rows.iter().map(|(k, _)| *k));
        let rec = self.record(|| Op::Write(b, rows.to_vec()));
        let quota = self.quota(b)?;
        // one lookup for both locks: a world discarded and made again meanwhile is `gone` here,
        // rather than written without its batched writer's lock
        let br = self.branch(b)?;
        let _long = lock(&br.long);
        let pos = {
            let _g = self.gate();
            let gone = lock(&br.writer);
            if *gone {
                return Err(Error::NoBranch(b.into()));
            }
            if let Some(q) = &quota {
                q.check(b, br.changed.load(Ordering::Relaxed), rows.len() as u64)?;
            }
            let mut s = br.snap.write().unwrap();
            br.stamp.store(stamp(), Ordering::Relaxed);
            for (k, v) in rows {
                s.set(k, v.map(Val::from));
            }
            br.changed.fetch_add(rows.len() as u64, Ordering::Relaxed);
            br.active.store(self.now(), Ordering::Relaxed);
            self.metrics.rows_written.fetch_add(rows.len() as u64, Ordering::Relaxed);
            (br.version.fetch_add(1, Ordering::Relaxed) + 1, self.log_write(b, &br, rec))
        };
        // (the next writer goes on while this one waits for the disk: its record is in the log after this one's, so
        // its own wait covers this one too, and the waits of writers in between share one fsync, as Postgres's
        // commits share a WAL flush)
        drop(_long);
        self.after(pos.1, b == MAIN);
        Ok(pos.0)
    }

    /// Whether this thread holds the long lock of branch `b` of this database (see `batch_long`).
    fn holds_long(&self, b: &str) -> bool {
        let me = self as *const Core as usize;
        HELD_LONG.with_borrow(|h| h.iter().any(|(c, x)| *c == me && x == b))
    }

    /// `batch`, with the rows computed by `f` from the branch as it is and no other write to the
    /// branch in between: read-modify-write, like SQL's UPDATE. Nothing is written if `f` fails
    /// or returns no rows. Returns the branch's version and what `f` returned.
    // ponytail: holds the checkpoint gate while `f` runs, so a long scan delays checkpoints.
    pub fn batch_with<T, E: From<Error>>(
        &self,
        b: &str,
        f: impl FnOnce(&View) -> std::result::Result<(Vec<(String, Option<String>)>, T), E>,
    ) -> std::result::Result<(u64, T), E> {
        let held = self.holds_long(b);
        self.batch_in(b, !held, f)
    }

    /// `batch_with`; `gated`: false only from inside another write, which holds the gate.
    pub(crate) fn batch_in<T, E: From<Error>>(
        &self,
        b: &str,
        gated: bool,
        f: impl FnOnce(&View) -> std::result::Result<(Vec<(String, Option<String>)>, T), E>,
    ) -> std::result::Result<(u64, T), E> {
        let quota = self.quota(b)?;
        let br = self.branch(b)?; // as in `write`, one lookup for both locks
        let _long = gated.then(|| lock(&br.long)); // inside a write: it holds the gate
        let (out, pos) = {
            let _g = if gated { self.gate() } else { None };
            let gone = lock(&br.writer);
            if *gone {
                return Err(Error::NoBranch(b.into()).into());
            }
            let (rows, out) = f(&View { core: self, born: br.born, s: br.snapshot(), branch: b.into(), reads: None })?;
            if rows.is_empty() {
                return Ok((br.version.load(Ordering::Relaxed), out));
            }
            let (version, pos) = self.put_rows(b, &br, &quota, &rows)?;
            ((version, out), pos)
        };
        drop(_long); // (the next writer goes on while this one waits for the disk: see `write`)
        self.after(pos, b == MAIN);
        Ok(out)
    }

    /// Writes `rows` to branch `b` as one logged batch; call holding the gate and `br`'s writer
    /// lock. Returns the branch's new version and the log position.
    fn put_rows(
        &self,
        b: &str,
        br: &Branch,
        quota: &Option<agents::Quota>,
        rows: &[(String, Option<String>)],
    ) -> Result<(u64, u64)> {
        if let Some(q) = quota {
            q.check(b, br.changed.load(Ordering::Relaxed), rows.len() as u64)?;
        }
        self.guard_main(b, rows.iter().map(|(k, _)| k.as_str()))?;
        let rows: Vec<(&str, Option<&str>)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
        self.note_sql(rows.iter().map(|(k, _)| *k));
        let rec = self.record(|| Op::Write(b, rows.clone()));
        let mut s = br.snap.write().unwrap();
        br.stamp.store(stamp(), Ordering::Relaxed);
        for (k, v) in &rows {
            s.set(k, v.map(Val::from));
        }
        br.changed.fetch_add(rows.len() as u64, Ordering::Relaxed);
        br.active.store(self.now(), Ordering::Relaxed);
        self.metrics.rows_written.fetch_add(rows.len() as u64, Ordering::Relaxed);
        Ok((br.version.fetch_add(1, Ordering::Relaxed) + 1, self.log_write(b, br, rec)))
    }

    /// `batch_with` for a change too big to hold at once, like building an index over a big
    /// table. `f` gets the branch as it is and `flush`, which writes rows straight away as a batch
    /// of their own: memory holds one batch, and checkpoints fold the rest into pages as usual.
    /// The rows `f` returns are written last. Other writes and merges to the branch wait until
    /// it's done (forks and reads don't). A checkpoint runs whenever `every` bytes have been
    /// flushed since the last one, or the log needs one.
    ///
    /// Each batch is logged on its own, so a crash can keep some flushed batches without the last:
    /// `f` must flush only rows nothing reads until the last batch lands (the entries of an index
    /// the table's schema doesn't have yet), and clear what an earlier, unfinished run left.
    pub(crate) fn batch_long<T, E: From<Error>>(
        &self,
        b: &str,
        every: usize,
        f: impl FnOnce(
            &View,
            &mut dyn FnMut(Vec<(String, Option<String>)>) -> Result<()>,
        ) -> std::result::Result<(Vec<(String, Option<String>)>, T), E>,
    ) -> std::result::Result<(u64, T), E> {
        // (a write inside this thread's own long write would wait for itself: a function or trigger of a statement
        // that changes the branch can't run a change that needs it whole)
        if self.holds_long(b) {
            return Err(Error::Constraint {
                code: "55006",
                message: "cannot make this change because the table is being used by active queries in this session"
                    .into(),
            }
            .into());
        }
        let quota = self.quota(b)?;
        let long = self.branch(b)?;
        let _long = lock(&long.long);
        let v = View { core: self, born: long.born, s: long.snapshot(), branch: b.into(), reads: None };
        let write = |rows: &[(String, Option<String>)]| -> Result<(u64, u64)> {
            let _g = self.gate();
            let br = self.branch(b)?;
            let gone = lock(&br.writer);
            if *gone || br.born != long.born {
                return Err(Error::NoBranch(b.into()));
            }
            self.put_rows(b, &br, &quota, rows)
        };
        let mut since = 0;
        let mut flush = |rows: Vec<(String, Option<String>)>| -> Result<()> {
            if !rows.is_empty() {
                write(&rows)?; // the last batch syncs the lot
                since += rows.iter().map(|(k, v)| k.len() + v.as_ref().map_or(0, String::len)).sum::<usize>();
                // fold what's written into pages before writing more, so memory holds `every`
                // bytes of it (the background worker would let the log reach 256 MB)
                if let Some(log) = &self.log
                    && (since > every || log.needs_checkpoint())
                {
                    let _one = log.ckpt.lock().unwrap();
                    self.checkpoint_now(log, false)?;
                    since = 0;
                }
            }
            Ok(())
        };
        struct Held;
        impl Drop for Held {
            fn drop(&mut self) {
                HELD_LONG.with_borrow_mut(|h| {
                    h.pop();
                });
            }
        }
        HELD_LONG.with_borrow_mut(|h| h.push((self as *const Core as usize, b.to_string())));
        let _held = Held;
        let (rows, out) = f(&v, &mut flush)?;
        drop(_held);
        drop(v);
        if rows.is_empty() {
            return Ok((long.version.load(Ordering::Relaxed), out));
        }
        let (version, pos) = write(&rows)?;
        drop(_long); // (the next writer goes on while this one waits for the disk: see `write`)
        self.after(pos, b == MAIN);
        Ok((version, out))
    }

    /// The next `n` values of sequence `name`, the same series on every branch, as a Postgres
    /// sequence behaves: never handed out twice, not taken back by a rollback, and a crash may
    /// skip some. The counter belongs to the database, not to any world: taking values writes no
    /// world's rows and moves no world's version, so a world's inserts leave main untouched (main
    /// may see gaps where a world took numbers). Its high-water mark is logged (`Op::Seq`),
    /// reserved 64 values at a time, so most calls touch nothing but memory.
    pub fn next_values(&self, name: &str, n: usize) -> Result<Vec<i64>> {
        self.next_values_in(name, n, true)
    }

    fn next_values_in(&self, name: &str, n: usize, gated: bool) -> Result<Vec<i64>> {
        let mut reserved = false;
        let (out, pos) = {
            // the gate before `seqs`, as every caller inside a write holds it: one lock order
            let _g = if gated { self.gate() } else { None };
            let mut seqs = self.seqs.lock().unwrap();
            let mut out = Vec::with_capacity(n);
            while out.len() < n {
                let (next, end) = seqs.get(name).copied().unwrap_or((1, 0));
                if next <= end {
                    let take = ((end - next + 1) as usize).min(n - out.len());
                    out.extend(next..next + take as i64);
                    seqs.insert(name.into(), (next + take as i64, end));
                    continue;
                }
                let want = (n - out.len()).max(64) as i64;
                let (high, _) = self.counter(name, |c| {
                    c.0 += want;
                    c.0
                });
                seqs.insert(name.into(), (high - want + 1, high));
                reserved = true;
            }
            // What these values rest on is the counter record that reserved their block, which
            // may be another caller's, still on its way to the disk: Postgres hands out nothing
            // its log doesn't cover (it logs a counter ahead, and flushes that record first). The
            // log is durable as a prefix, so the position of the latest counter record, read under
            // `seqs` after the one that reserved the block was appended, covers it.
            (out, self.counter_at.0.load(Ordering::Acquire))
        };
        if reserved {
            disk::stall_for_tests("CHRONOS_SEQ_STALL_MS");
        }
        if pos > self.counter_at.1.load(Ordering::Acquire) {
            self.counted(pos, gated);
        }
        Ok(out)
    }

    /// Changes sequence `name`'s counter with `f`, logged; returns the log position to make
    /// durable (see `counted`). The caller holds the gate.
    fn counter<T>(&self, name: &str, f: impl FnOnce(&mut (i64, Option<String>)) -> T) -> (T, u64) {
        let mut all = self.counters.lock().unwrap();
        let c = all.entry(name.into()).or_insert((0, None));
        let out = f(c);
        let rec = self.record(|| Op::Seq(name, c.0, c.1.as_deref()));
        let pos = self.append(rec);
        self.counter_at.0.fetch_max(pos, Ordering::Release);
        (out, pos)
    }

    /// Makes counters logged up to `pos` durable, with no lock held but the gate of a write that
    /// holds it already (`gated` false), which a checkpoint would wait for: then only sync.
    fn counted(&self, pos: u64, gated: bool) {
        match &self.log {
            _ if pos == 0 => return,
            Some(log) if !gated => log.sync(pos),
            _ => self.after(pos, true),
        }
        // as durable as `sync` asks of a commit (with `sync = off` that is the OS's, which a
        // killed process leaves be)
        self.counter_at.1.fetch_max(pos, Ordering::Release);
    }

    /// Starts sequence `name` (see `next_values`) over: its next value is step 1 again, with
    /// `origin` (if any) kept beside it (see `sequence_origin`) for the caller to count from.
    /// `ahead` says, from the counter as it is (its highest step reserved, its origin), whether
    /// that only moves it on. Moving it back would hand every world (main too) values it has
    /// handed out already, so only those who may change main may (see `may_rewind`).
    pub(crate) fn restart_sequence(
        &self,
        name: &str,
        origin: Option<String>,
        rewind: bool,
        ahead: impl FnOnce(i64, Option<&str>) -> bool,
    ) -> Result<()> {
        let (_, pos) = {
            let _g = self.gate();
            // `seqs` held from the check to the restart: no values are reserved in between
            let mut seqs = self.seqs.lock().unwrap();
            let (high, from) = self.sequence_counter(name);
            if !rewind && !ahead(high, from.as_deref()) {
                return Err(Error::Constraint {
                    code: "42501",
                    message: format!(
                        "agent {} may not move sequence counter {name} back: every world shares it, so that needs 'write_main'",
                        disk::actor()
                    ),
                });
            }
            seqs.remove(name);
            // counted before `seqs` is let go, so values reserved after it are counted after it
            let mut last = self.restarts.1.lock().unwrap();
            last.insert(name.into(), self.restarts.0.fetch_add(1, Ordering::AcqRel) + 1);
            drop(last);
            self.counter(name, |c| *c = (0, origin))
        };
        self.counted(pos, true);
        Ok(())
    }

    /// Counter restarts so far (see `restarted_since`).
    pub(crate) fn sequence_restarts(&self) -> u64 {
        self.restarts.0.load(Ordering::Acquire)
    }

    /// Whether sequence `name`'s counter is unrestarted since `sequence_restarts` said `seen`, so
    /// values reserved after that still continue its count: then the count now, which says the
    /// same of it; None once it has restarted.
    pub(crate) fn restarted_since(&self, name: &str, seen: u64) -> Option<u64> {
        if self.sequence_restarts() == seen {
            return Some(seen);
        }
        let last = self.restarts.1.lock().unwrap();
        let now = self.restarts.0.load(Ordering::Acquire); // (restarts count under the lock)
        last.get(name).is_none_or(|&n| n <= seen).then_some(now)
    }

    /// Whether this thread may move a sequence's counter back (see `restart_sequence`): the
    /// database's own users may; an agent needs 'write_main'.
    /// Never under `fixed_counters` (MCP safe mode).
    pub(crate) fn may_rewind(&self) -> bool {
        let a = disk::actor();
        !counters_fixed() && (a.is_empty() || self.agent(&a).is_ok_and(|a| a.may(Can::WriteMain)))
    }

    /// Sequence `name`'s counter: its highest step reserved, and its origin.
    pub(crate) fn sequence_counter(&self, name: &str) -> (i64, Option<String>) {
        self.counters.lock().unwrap().get(name).cloned().unwrap_or((0, None))
    }

    /// Where sequence `name` is: its counter and the next value of the block this process
    /// reserved, so it changes whenever a value is handed out.
    pub(crate) fn sequence_mark(&self, name: &str) -> SequenceMark {
        let seqs = self.seqs.lock().unwrap(); // before `counters`, as `next_values_in` locks them
        (seqs.get(name).copied(), self.sequence_counter(name))
    }

    /// Where sequence `name` counts from, as `restart_sequence` last set it.
    pub(crate) fn sequence_origin(&self, name: &str) -> Option<String> {
        self.counters.lock().unwrap().get(name).and_then(|c| c.1.clone())
    }

    /// Takes over counters an older Chronos kept as rows of main (`_sonos_seq/<name>` and
    /// `_sonos_seq/<name>/origin`), once, logged: those rows are never read or written again.
    fn adopt_counters(&self) -> Result<()> {
        let mut found: HashMap<String, (i64, Option<String>)> = HashMap::new();
        let v = self.view(MAIN)?;
        for r in v.prefix("_sonos_seq/")? {
            let (k, val) = r?;
            let k = &k["_sonos_seq/".len()..];
            match k.strip_suffix("/origin") {
                Some(name) => found.entry(name.into()).or_default().1 = Some(val.to_string()),
                None => found.entry(k.into()).or_default().0 = val.parse().unwrap_or(0),
            }
        }
        let mut pos = 0;
        let mut all = self.counters.lock().unwrap();
        for (name, c) in found {
            if let std::collections::hash_map::Entry::Vacant(e) = all.entry(name) {
                pos = self.append(self.record(|| Op::Seq(e.key().as_str(), c.0, c.1.as_deref())));
                e.insert(c);
            }
        }
        drop(all);
        if let Some(log) = &self.log {
            log.sync(pos);
        }
        Ok(())
    }

    /// Remembers that a SQL table exists once one is written.
    fn note_sql<'k>(&self, keys: impl Iterator<Item = &'k str>) {
        let seen = |f: &AtomicBool| f.load(Ordering::Relaxed);
        if seen(&self.sql)
            && seen(&self.views)
            && seen(&self.sequences)
            && seen(&self.inherits)
            && seen(&self.privileges)
            && seen(&self.row_security)
        {
            return;
        }
        for k in keys {
            if k.starts_with("_sonos_tables/") {
                self.sql.store(true, Ordering::Relaxed);
            } else if k.starts_with("_sonos_views/") {
                self.views.store(true, Ordering::Relaxed);
            } else if k.starts_with("_sonos_sequences/") {
                self.sequences.store(true, Ordering::Relaxed);
            } else if k.starts_with("_sonos_inh/") {
                self.inherits.store(true, Ordering::Relaxed);
            } else if k.starts_with("_sonos_roles/") || k.starts_with("_sonos_acl/") {
                self.privileges.store(true, Ordering::Relaxed);
            } else if k.starts_with("_sonos_rls/") || k.starts_with("_sonos_policy/") {
                self.row_security.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Looks for SQL tables and views from before (checkpointed, so not in the replayed log), on
    /// first use rather than at open: it reads pages, whole packs from a remote. Worlds mostly
    /// share trees, so each tree is looked in once, and each world's own changes after that.
    fn probe_catalog(&self) {
        if self.probed.load(Ordering::Acquire) {
            return;
        }
        // the worlds first, then their states: a state is read with the map let go, as `fork` reads
        // a state and then takes the map to write (held across the reads it deadlocked with a
        // write waiting on that state, a fork holding it, and this waiting on the map)
        let worlds: Vec<Arc<Branch>> = self.branches.read().unwrap().values().map(|e| e.br.clone()).collect();
        let states: Vec<State> = worlds.iter().map(|b| b.snapshot()).collect();
        let look = |prefix: &str| -> io::Result<bool> {
            let mut trees = std::collections::HashSet::new();
            for s in &states {
                let ours = |k: &str| k.starts_with(prefix);
                if s.over.range::<_, str>(from_key(prefix)).next().is_some_and(|(k, _)| ours(k)) {
                    return Ok(true);
                }
                if let (Some(t), Some(p)) = (self.tree(s), &self.pages)
                    && trees.insert(*t)
                    && prolly::range(p, t, prefix)?.next().transpose()?.is_some_and(|(k, _)| ours(&k))
                {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        for (prefix, flag) in [
            ("_sonos_tables/", &self.sql),
            ("_sonos_views/", &self.views),
            ("_sonos_sequences/", &self.sequences),
            ("_sonos_inh/", &self.inherits),
            ("_sonos_roles/", &self.privileges),
            ("_sonos_acl/", &self.privileges),
            ("_sonos_rls/", &self.row_security),
            ("_sonos_policy/", &self.row_security),
        ] {
            // a page that can't be read now: assume there are some, which only costs lookups
            if look(prefix).unwrap_or(true) {
                flag.store(true, Ordering::Relaxed);
            }
        }
        self.probed.store(true, Ordering::Release);
    }

    /// Whether any role, or any object's owner or grants, has existed here (sessions look for who
    /// they are and objects for their privileges only then; see `sql::roles`).
    pub(crate) fn has_privileges(&self) -> bool {
        self.probe_catalog();
        self.privileges.load(Ordering::Relaxed)
    }

    /// Whether any table has had row security or a policy here (reads and writes look for their
    /// policies only then; see `sql::rls`).
    pub(crate) fn has_row_security(&self) -> bool {
        self.probe_catalog();
        self.row_security.load(Ordering::Relaxed)
    }

    /// Whether any view has existed here (statements look for the views they read only then).
    pub(crate) fn has_views(&self) -> bool {
        self.probe_catalog();
        self.views.load(Ordering::Relaxed)
    }

    /// Whether any sequence made with CREATE SEQUENCE has existed here (a table name may then be one).
    pub(crate) fn has_sequences(&self) -> bool {
        self.probe_catalog();
        self.sequences.load(Ordering::Relaxed)
    }

    /// Whether any table has had a partition or a child here (a read of a table then looks for them).
    pub(crate) fn has_inherits(&self) -> bool {
        self.probe_catalog();
        self.inherits.load(Ordering::Relaxed)
    }

    /// Whether any SQL table has existed here (JSON writes skip SQL's checks until one has).
    pub(crate) fn has_sql(&self) -> bool {
        self.probe_catalog();
        self.sql.load(Ordering::Relaxed)
    }

    /// The branch as it is now. O(1); later writes don't show up in it.
    /// A world as it is now, or as it was: `name@when`, where `when` is a time
    /// (`2026-09-20 10:00`, UTC unless it says) or a time ago (`-5 minutes`). Past views read only.
    /// A session's transaction world (`_tx_…`) is refused: sessions reach theirs with `view_tx`.
    pub fn view(&self, b: &str) -> Result<View<'_>> {
        if self.private_world(b) {
            return Err(Error::NoBranch(b.into()));
        }
        self.view_tx(b)
    }

    /// `view`, including a session's own transaction world: for the session's own paths, never
    /// for a name someone gave (public for the server binary's Postgres sessions).
    #[doc(hidden)]
    pub fn view_tx(&self, b: &str) -> Result<View<'_>> {
        if let Some((name, when)) = b.split_once('@')
            && self.branch(b).is_err()
        {
            // what a merge would make, read by a merge check (see `checks`)
            if let Some(s) = self.merging.lock().unwrap().get(b) {
                return Ok(View { core: self, born: PAST, s: s.clone(), branch: name.into(), reads: None });
            }
            let t = sql::moment(when).ok_or_else(|| no_history(format!("'{when}' is not a time or a time ago")))?;
            return self.view_at(name, t);
        }
        let br = self.branch(b)?;
        br.active.store(disk::now_ms(), Ordering::Relaxed); // a world being read is in use
        Ok(View { core: self, born: br.born, s: br.snapshot(), branch: b.into(), reads: br.recorder() })
    }

    /// World `b`'s committed state now, for `still` to tell later whether anything was written since: None for a
    /// world that can't be found (or is read in the past) and for one that records its reads (see `stale`: what is
    /// read through a view of it goes in its read set, which work answered from a kept state wouldn't do).
    pub(crate) fn snapshot_of(&self, b: &str) -> Option<Stamp> {
        let br = self.branch(b).ok()?;
        br.recorder().is_none().then(|| br.snap.read().unwrap().stamp())
    }

    /// Whether world `b` is, right now, in the very state `s` came from `snapshot_of` (O(1), see `Stamp`): nothing
    /// was written to it since.
    pub(crate) fn still(&self, b: &str, s: &Stamp) -> bool {
        self.branch(b).is_ok_and(|br| br.recorder().is_none() && br.snap.read().unwrap().is(s))
    }

    /// World `b`'s read set, if it checks its reads (see `stale`): for views made from a
    /// session's own state (see `sql`'s `Exec`).
    pub(crate) fn recorder(&self, b: &str) -> Option<Recorder> {
        self.branch(b).ok()?.recorder()
    }

    /// A view of state `s`, of no world: for reading a state a merge compares (see `stale`).
    pub(crate) fn past_view(&self, s: &State) -> View<'_> {
        View { core: self, born: PAST, s: s.clone(), branch: String::new(), reads: None }
    }

    /// A world as it was at `t` (ms since 1970), by name (or ID) then.
    pub fn view_at(&self, name: &str, t: u64) -> Result<View<'_>> {
        let past = self.past(t)?;
        let found = past.worlds.get_key_value(name).or_else(|| past.worlds.iter().find(|(_, (_, id))| id == name));
        let found = found.filter(|(n, _)| !n.starts_with("_tx_")).map(|(_, w)| w); // a transaction's: private
        let (s, _) = found.ok_or_else(|| Error::NoBranch(format!("{name}@{}", show_ms(t))))?;
        Ok(View { core: self, born: PAST, s: s.clone(), branch: name.into(), reads: None })
    }

    /// Row-level differences from world `a` to world `b`, either of them now or in the past
    /// (`name@when`): what it takes to turn `a` into `b`.
    pub fn diff_between(&self, a: &str, b: &str) -> Result<Vec<Change>> {
        not_tx(a).and(not_tx(b))?;
        let _t = self.metrics.time(metrics::Op::Diff);
        let (a, b) = (self.view(a)?.s, self.view_tx(b)?.s);
        self.state_diff(&a, &b)?.into_iter().filter(|c| shown(&c.key)).map(|c| self.show_change(&a, &b, c)).collect()
    }

    /// Puts world `b` back as it was at `t` (ms since 1970): its rows are written back, as an
    /// ordinary write (so it merges, and history keeps what it replaced). Returns rows written.
    pub fn restore(&self, b: &str, t: u64) -> Result<usize> {
        if b == MAIN {
            attach::refuse_main(self, "RESTORE")?;
        }
        let _back = Recovering::enter(); // a way back: main's protection lets it through
        // a world rebased since: its rows then sat on another fork point, and put back on this one
        // they'd undo, at its merge, what its parent changed in between
        let since_rebase = || -> Result<()> {
            let rebased = self.branch(b)?.info.read().unwrap().meta.get(REBASED).and_then(|v| v.as_u64());
            match rebased.filter(|at| t < *at) {
                Some(at) => Err(Error::Constraint {
                    code: "55000",
                    message: format!(
                        "{b} was rebased at {}, after that time: it can go back only to a time since (or fork its parent as it was then)",
                        show_ms(at)
                    ),
                }),
                None => Ok(()),
            }
        };
        since_rebase()?; // before reading the past, which may not be there to read
        let past = self.view_at(b, t)?.s;
        // and again under the world's writer lock, which a rebase holds while it moves the fork
        // point: none comes between the check and the write
        let (_, n) = self.batch_with(b, |now| {
            since_rebase()?;
            let rows: Vec<(String, Option<String>)> =
                self.undo(&now.s, &past)?.into_iter().map(|(k, v)| (k, v.map(|v| v.to_string()))).collect();
            let n = rows.len();
            Ok::<_, Error>((rows, n))
        })?;
        Ok(n)
    }

    /// World `b` as it was forked: the database a transaction there reads, without its own
    /// writes (what `pg_export_snapshot` hands another session).
    pub(crate) fn fork_state(&self, b: &str) -> Result<State> {
        Ok(self.branch(b)?.base())
    }

    /// Writes into world `b` what turns it into `past` (a state of this database), as ordinary
    /// logged writes, so a replay of the log ends the same: `SET TRANSACTION SNAPSHOT`'s
    /// transaction reads what the exporting one does. Agents' rows aren't brought back (see `undo`).
    pub(crate) fn rewind(&self, b: &str, past: &State) -> Result<usize> {
        let rows = self.undo(&self.branch(b)?.snapshot(), past)?;
        let refs: Vec<(&str, Option<&str>)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
        if !refs.is_empty() {
            self.write(b, &refs)?;
        }
        Ok(rows.len())
    }

    /// A new world forked from `from` that holds `from` as it was at `t`: diff it to see what
    /// changed since, merge it to restore `from`, or just read it.
    pub fn fork_at(&self, from: &str, t: u64, name: &str, meta: serde_json::Value) -> Result<World> {
        let past = self.view_at(from, t)?.s;
        let w = self.fork_with(from, name, meta)?;
        let rows = self.undo(&self.branch(name)?.base(), &past)?;
        let refs: Vec<(&str, Option<&str>)> = rows.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
        if !refs.is_empty() {
            self.write(name, &refs)?;
        }
        Ok(w)
    }

    /// What happened to world `b` within the retention window, newest first: forks, writes,
    /// merges (commits of transactions on it, too), discards and metadata changes.
    // ponytail: reads the whole retained log per call; index it by world if history is asked often
    pub fn history(&self, b: &str, limit: usize) -> Result<Vec<Event>> {
        not_tx(b)?;
        let Some(log) = &self.log else {
            return Err(no_history("an in-memory database keeps no history".into()));
        };
        let hist = disk::history(&log.dir)?;
        let Some((seg, cut, path)) = hist.first() else { return Ok(vec![]) };
        let (_, _, man) = disk::read_manifest_at(path, log.crypt.as_deref())?
            .ok_or_else(|| no_history("history is missing".into()))?;
        let mut parents: HashMap<String, Option<String>> = man.into_iter().map(|m| (m.name, m.parent)).collect();
        let mut written: HashMap<String, usize> = HashMap::new(); // rows each branch wrote, for its merge
        let (mut at, mut out) = (*cut, vec![]);
        self.flush_log(log);
        disk::read_log(&log.dir, log.crypt.as_deref(), *seg, |_, op| {
            let mut ev = |what: String, rows: usize| out.push(Event { at, world: b.into(), what, rows });
            match op {
                Op::Clock(c) => at = c,
                Op::Write(w, rows) => {
                    // the tables' rows: not the entries behind their indexes and constraints, nor the database's own
                    let rows = rows.iter().filter(|(k, _)| !reserved_table(k)).count();
                    *written.entry(w.clone()).or_default() += rows;
                    if w == b {
                        ev("write".into(), rows);
                    }
                }
                Op::Bulk(w, rows, ..) => {
                    *written.entry(w.clone()).or_default() += rows as usize;
                    if w == b {
                        ev("write".into(), rows as usize);
                    }
                }
                Op::Fork(from, name) | Op::World(from, name, ..) => {
                    if name == b {
                        ev(if from.is_empty() { "created empty".into() } else { format!("forked from {from}") }, 0);
                    } else if from == b && !name.starts_with("_tx_") {
                        ev(format!("forked {name}"), 0);
                    }
                    written.remove(&name);
                    parents.insert(name, (!from.is_empty()).then_some(from));
                }
                Op::Merge(w, ..) | Op::MergeWith(w, ..) => {
                    let parent = parents.remove(&w).flatten().unwrap_or_default();
                    let rows = written.remove(&w).unwrap_or(0);
                    if w == b {
                        ev(format!("merged into {parent}"), rows);
                    } else if parent == b {
                        ev(if w.starts_with("_tx_") { "commit".into() } else { format!("merged {w}") }, rows);
                    }
                }
                Op::MergeSome { branch: w, into, tables, keys, .. } => {
                    // the world stays open: its rows written so far, merged in part or elsewhere
                    let into = match into.is_empty() {
                        true => parents.get(&w).cloned().flatten().unwrap_or_default(),
                        false => into,
                    };
                    let (part, rows) = (!tables.is_empty() || !keys.is_empty(), written.get(&w).copied().unwrap_or(0));
                    if w == b {
                        ev(format!("merged {}into {into}", if part { "part " } else { "" }), rows);
                    } else if into == b {
                        ev(format!("merged {}{w}", if part { "part of " } else { "" }), rows);
                    }
                }
                Op::Rebase(w, ..) if w == b => {
                    // the world stays open, on a new fork point
                    let parent = parents.get(&w).cloned().flatten().unwrap_or_default();
                    ev(format!("rebased onto {parent}"), 0);
                }
                Op::Discard(w) => {
                    if w == b {
                        ev("discarded".into(), 0);
                    }
                    parents.remove(&w);
                    written.remove(&w);
                }
                Op::Meta(w, _) if w == b => ev("metadata changed".into(), 0),
                _ => {}
            }
            true
        })?;
        out.sort_by_key(|e| e.at); // a batch of writes is logged after records from later
        out.reverse();
        out.truncate(limit);
        Ok(out)
    }

    /// Undoes the latest merge of world `child` (within the history window): puts back, in the
    /// world it merged into, every row the merge changed, as an ordinary write. Rows changed since
    /// the merge are left alone with `skip_changed`; otherwise their names come back as an error
    /// and nothing changes.
    pub fn undo_merge(&self, child: &str, skip_changed: bool) -> Result<Undone> {
        let _back = Recovering::enter(); // a way back: main's protection lets it through
        not_tx(child)?; // a transaction's commit: undone by a statement, not by its hidden name
        let Some(log) = &self.log else {
            return Err(no_history("an in-memory database keeps no history".into()));
        };
        let hist = disk::history(&log.dir)?;
        let Some((first, cut, path)) = hist.first() else { return Err(no_history("no history kept".into())) };
        let (_, _, man) = disk::read_manifest_at(path, log.crypt.as_deref())?
            .ok_or_else(|| no_history("history is missing".into()))?;
        let mut parents: HashMap<String, Option<String>> = man.into_iter().map(|m| (m.name, m.parent)).collect();
        self.flush_log(log);
        // the merge: which record (segment, place in it), when, and into which world
        let (mut at, mut place, mut found) = (*cut, (0, 0), None);
        disk::read_log(&log.dir, log.crypt.as_deref(), *first, |seg, op| {
            place = if place.0 == seg { (seg, place.1 + 1) } else { (seg, 0) };
            match op {
                Op::Clock(c) => at = c,
                Op::Fork(from, name) | Op::World(from, name, ..) => {
                    parents.insert(name, (!from.is_empty()).then_some(from));
                }
                Op::Merge(w, ..) | Op::MergeWith(w, ..) => {
                    let parent = parents.remove(&w).flatten();
                    if w == child {
                        found = Some((place, at, parent));
                    }
                }
                Op::MergeSome { branch: w, into, .. } if w == child => {
                    let into = if into.is_empty() { parents.get(&w).cloned().flatten() } else { Some(into) };
                    found = Some((place, at, into));
                }
                Op::Discard(w) => {
                    parents.remove(&w);
                }
                _ => {}
            }
            true
        })?;
        let Some((place, when, Some(parent))) = found else {
            return Err(no_history(format!("no merge of {child} within the history window")));
        };
        if parent == MAIN {
            attach::refuse_main(self, "UNDO MERGE")?;
        }
        self.may_undo_into(&parent)?;
        // the world it merged into, just before and just after that record
        let (base_seg, _, base) = hist.iter().rev().find(|h| h.0 <= place.0).expect("the first history is older");
        let (_, _, man) = disk::read_manifest_at(base, log.crypt.as_deref())?
            .ok_or_else(|| no_history("history is missing".into()))?;
        let mut scratch = Core::new();
        scratch.pages = self.pages.clone();
        scratch.empty = self.empty;
        scratch.load(man);
        let (mut now, mut states) = ((0, 0), None);
        disk::read_log(&log.dir, log.crypt.as_deref(), *base_seg, |seg, op| {
            now = if now.0 == seg { (seg, now.1 + 1) } else { (seg, 0) };
            if now == place {
                let before = scratch.branch(&parent).map(|b| b.snapshot());
                scratch.replay(op);
                states = before.ok().zip(scratch.branch(&parent).ok().map(|b| b.snapshot()));
                return false;
            }
            scratch.replay(op);
            true
        })?;
        let (before, after) = states.ok_or_else(|| no_history(format!("the merge of {child} can't be rebuilt")))?;
        let changes = self.state_diff(&before, &after)?;
        let (_, (rows, skipped)) = self.batch_with(&parent, |v| {
            let (mut rows, mut changed) = (vec![], vec![]);
            for c in changes.iter().filter(|c| !c.key.starts_with("_sonos_seq/")) {
                if self.val(&v.s, &c.key)? == c.after {
                    rows.push((c.key.clone(), c.before.as_deref().map(String::from)));
                } else if !sql::internal(&c.key) && !sql::btree_entry(&c.key) {
                    changed.push(c.key.clone());
                }
            }
            if !changed.is_empty() && !skip_changed {
                let some: Vec<std::borrow::Cow<str>> = changed.iter().take(10).map(|k| sql::show_key(k)).collect();
                return Err(Error::Constraint {
                    code: "40001",
                    message: format!(
                        "{} rows the merge changed have changed again since ({}); UNDO MERGE ... SKIP CHANGED undoes the rest; nothing was undone",
                        changed.len(),
                        some.join(", ")
                    ),
                });
            }
            let n = rows.iter().filter(|(k, _)| !sql::internal(k) && !sql::btree_entry(k) && !k.starts_with("_sonos_")).count();
            Ok((rows, (n, changed.len())))
        })?;
        Ok(Undone { parent, at: when, rows, skipped })
    }

    /// Undoes what agent `agent` changed in world `b` since `since` (ms since 1970, within the
    /// history window): its own writes, its commits and the merges it ran into `b`. Each row it
    /// changed goes back to how it was before the agent first changed it, as an ordinary write,
    /// and constraints are checked as in a merge. Rows someone else changed after the agent did
    /// are left alone with `skip_changed`; otherwise their names come back as an error and nothing
    /// changes. Merges a person ran of an agent's world are the person's: see `undo_merge`.
    // ponytail: replays all history since the checkpoint before `since` in memory on every call;
    // index changes by agent if undoing weeks back gets slow
    pub fn undo_agent(&self, agent: &str, b: &str, since: u64, skip_changed: bool) -> Result<Undone> {
        if b == MAIN {
            attach::refuse_main(self, "UNDO AGENT")?;
        }
        let _back = Recovering::enter(); // a way back: main's protection lets it through
        self.may_undo_into(b)?;
        let Some(log) = &self.log else {
            return Err(no_history("an in-memory database keeps no history".into()));
        };
        let hist = disk::history(&log.dir)?;
        let Some((first, cut, path)) = hist.iter().rev().find(|h| h.1 <= since) else {
            let from = hist.first().map_or(disk::now_ms(), |h| h.1);
            return Err(no_history(format!("history starts at {}; see the retention setting", show_ms(from))));
        };
        let (_, _, man) = disk::read_manifest_at(path, log.crypt.as_deref())?
            .ok_or_else(|| no_history("history is missing".into()))?;
        let mut parents: HashMap<String, Option<String>> =
            man.iter().map(|m| (m.name.clone(), m.parent.clone())).collect();
        let mut scratch = Core::new();
        scratch.pages = self.pages.clone();
        scratch.empty = self.empty;
        scratch.load(man);
        // each row the agent changed: before its first change, after its last, and whether
        // anyone else changed it after the agent first did
        let mut rows: std::collections::BTreeMap<String, (Option<Val>, Option<Val>, bool)> = Default::default();
        let (mut at, mut who, mut failed) = (*cut, String::new(), None);
        self.flush_log(log);
        disk::read_log(&log.dir, log.crypt.as_deref(), *first, |_, op| {
            let into = match &op {
                Op::Clock(c) => {
                    at = *c;
                    None
                }
                Op::Actor(a) => {
                    who.clone_from(a);
                    None
                }
                Op::Write(w, _) | Op::Bulk(w, ..) => Some(w.clone()),
                Op::Merge(w, ..) | Op::MergeWith(w, ..) => parents.remove(w).flatten(),
                Op::MergeSome { branch: w, into, .. } if into.is_empty() => parents.get(w).cloned().flatten(),
                Op::MergeSome { into, .. } => Some(into.clone()),
                Op::Fork(from, name) | Op::World(from, name, ..) => {
                    parents.insert(name.clone(), (!from.is_empty()).then(|| from.clone()));
                    None
                }
                Op::Discard(w) => {
                    parents.remove(w);
                    None
                }
                _ => None,
            };
            let before = (at >= since && into.as_deref() == Some(b))
                .then(|| scratch.branch(b).map(|br| br.snapshot()).ok())
                .flatten();
            let step = || -> Result<()> {
                let Some(s) = before else {
                    scratch.replay(op);
                    return Ok(());
                };
                let changes = match &op {
                    Op::Write(_, recs) => {
                        let mut out = vec![];
                        for (k, v) in recs {
                            out.push(Change {
                                key: k.clone(),
                                before: scratch.val(&s, k)?,
                                after: v.as_deref().map(Val::from),
                            });
                        }
                        scratch.replay(op);
                        out
                    }
                    _ => {
                        scratch.replay(op);
                        scratch.state_diff(&s, &scratch.branch(b)?.snapshot())?
                    }
                };
                for c in changes {
                    if who == agent {
                        rows.entry(c.key).or_insert_with(|| (c.before, None, false)).1 = c.after;
                    } else if let Some(r) = rows.get_mut(&c.key) {
                        r.2 = true;
                    }
                }
                Ok(())
            };
            match step() {
                Ok(()) => true,
                Err(e) => {
                    failed = Some(e);
                    false
                }
            }
        })?;
        if let Some(e) = failed {
            return Err(e);
        }
        let (_, (n, skipped)) = self.batch_with(b, |v| {
            // rows first; index entries are rebuilt below for the rows put back, as merges do
            let (mut put, mut changed) = (std::collections::BTreeMap::new(), vec![]);
            let mut owned = vec![]; // unique and reference rows: back only with their rows
            for (k, (before, after, others)) in &rows {
                if k.starts_with("_sonos_seq/") || sql::btree_entry(k) || agents::agent_row(k) {
                    continue;
                }
                let now = self.val(&v.s, k)?;
                if sql::internal(k) {
                    if !others && now == *after {
                        owned.push((k, before));
                    }
                } else if !others && now == *after {
                    if now != *before {
                        put.insert(k.clone(), before.clone());
                    }
                } else {
                    changed.push(k.as_str());
                }
            }
            if !changed.is_empty() && !skip_changed {
                return Err(Error::Constraint {
                    code: "40001",
                    message: format!(
                        "{} rows agent {agent} changed have changed again since ({}); UNDO AGENT ... SKIP CHANGED undoes the rest; nothing was undone",
                        changed.len(),
                        changed[..changed.len().min(10)].iter().map(|k| sql::show_key(k)).collect::<Vec<_>>().join(", ")
                    ),
                });
            }
            let n = put.len();
            for (k, before) in owned {
                let after = rows[k].1.as_deref();
                if sql::owners(k, [before.as_deref(), after]).iter().all(|o| put.contains_key(o)) {
                    put.insert(k.clone(), before.clone());
                }
            }
            let keys: Vec<String> = put.keys().cloned().collect();
            let mut next = v.s.clone();
            for (k, val) in &put {
                next.set(k, val.clone());
            }
            let undone = |e: sql::SqlError| Error::Constraint { code: e.code, message: format!("{}; nothing was undone", e.message) };
            for (k, val) in sql::reindex(&View { s: next.clone(), ..v.clone() }, v, &keys).map_err(undone)? {
                next.set(&k, val.clone());
                put.insert(k, val);
            }
            sql::check_merge(&View { s: next, ..v.clone() }, v, &keys).map_err(undone)?;
            let out = put.into_iter().map(|(k, v)| (k, v.as_deref().map(String::from))).collect();
            Ok((out, (n, changed.len())))
        })?;
        Ok(Undone { parent: b.into(), at: since, rows: n, skipped })
    }

    /// The raw writes that turn state `from` into state `to`, but for agents' rows: restoring or
    /// forking the past (restores, `fork_at`, simulations) never brings back an agent, a token or
    /// a right that was dropped since.
    fn undo(&self, from: &State, to: &State) -> Result<Vec<(String, Option<Val>)>> {
        let mut out = vec![];
        for c in self.state_diff(from, to)?.into_iter().filter(|c| !agents::agent_row(&c.key)) {
            out.push((c.key.clone(), self.val(to, &c.key)?));
        }
        Ok(out)
    }

    /// Closes the database as dropping it would (a closing checkpoint, the log ended cleanly, all
    /// on disk), for a process about to exit while other threads still hold it: a server told to
    /// stop. Writes started after it wait forever, so call it last, then exit.
    pub fn close(&self) -> io::Result<()> {
        let Some(log) = &self.log else { return Ok(()) };
        self.checkpoint_closing()?;
        std::mem::forget(self.gate.write().unwrap()); // no more writes: they'd come after the Close
        self.flush_batches(); // those since the checkpoint
        log.close()
    }

    /// What this database has done since it was opened: operation counts and latencies.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Counters and sizes, by name: rows written and errors since opening, worlds now, bytes on
    /// disk now (pages, the log with the history it keeps, history manifests).
    pub fn counters(&self) -> Vec<(&'static str, u64)> {
        let m = &self.metrics;
        let worlds = self.branches.read().unwrap().keys().filter(|b| !b.starts_with("_tx_")).count();
        let mut out = vec![
            ("uptime_seconds", m.uptime_secs() as u64),
            ("rows_written", m.rows_written.load(Ordering::Relaxed)),
            ("errors", m.errors.load(Ordering::Relaxed)),
            ("conflicts", m.conflicts.load(Ordering::Relaxed)),
            ("worlds", worlds as u64),
            // 1 while an HNSW graph builds in the background (searches scan until it's done)
            ("search_graphs_building", u64::from(!self.search_graphs_ready())),
            // bytes queries wrote to disk because their working state outgrew CHRONOS_WORK_MEM
            ("spilled_bytes", sql::spilled_bytes()),
        ];
        if let Some(p) = &self.pages {
            out.extend(p.cache_stats()); // cache_bytes (now), cache_hits, cache_misses
        }
        if let Some(log) = &self.log {
            let size = |p: &Path| {
                std::fs::read_dir(p).map_or(0, |d| {
                    d.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
                })
            };
            out.push(("pages_bytes", size(&log.dir.join("pages"))));
            out.push(("log_bytes", size(&log.dir)));
            out.push(("history_bytes", size(&log.dir.join("history"))));
        }
        out
    }

    /// Writes since the branch was forked (see `batch` and `merge_at`).
    pub fn version(&self, b: &str) -> Result<u64> {
        Ok(self.branch(b)?.version.load(Ordering::Relaxed))
    }

    /// World `b` as it is now, as a merge's `target` names it (see `MergePlan::target`): it moves
    /// on with every change to its rows, merges into it included.
    pub fn stamp(&self, b: &str) -> Result<u64> {
        Ok(self.branch(b)?.stamp.load(Ordering::Relaxed))
    }

    pub fn get(&self, b: &str, k: &str) -> Result<Option<Val>> {
        let s = self.view_tx(b)?.s;
        let k = self.typed(&s, k)?;
        self.val(&s, &k)?.map(|v| self.show(&s, &k, v, &mut HashMap::new())).transpose()
    }

    /// A row key as people type it (`orders/5`) as its table stores it (see `sql::typed_key`).
    fn typed<'k>(&self, s: &State, k: &'k str) -> Result<std::borrow::Cow<'k, str>> {
        match k.split_once('/') {
            Some((t, id)) if id.parse::<i64>().is_ok() || sql::may_be_encoded(id) => {
                Ok(sql::typed_key(self.val(s, &format!("{}/{t}", sql::SCHEMAS))?.as_ref(), k))
            }
            _ => Ok(std::borrow::Cow::Borrowed(k)),
        }
    }

    /// A value as the world outside SQL sees it: SQL tables store rows packed (see `sql`), and
    /// those come out as JSON objects, like every other row. `schemas` caches table schemas.
    fn show(&self, s: &State, key: &str, v: Val, schemas: &mut HashMap<String, Option<Val>>) -> Result<Val> {
        if !sql::is_packed(&v) {
            return Ok(v);
        }
        let table = key.split('/').next().unwrap_or_default();
        if !schemas.contains_key(table) {
            schemas.insert(table.into(), self.val(s, &format!("{}/{table}", sql::SCHEMAS))?);
        }
        Ok(Val::from(sql::packed_json(schemas[table].as_ref(), &v)))
    }

    fn show_all(&self, s: &State, rows: Vec<(String, Val)>) -> Result<Vec<(String, Val)>> {
        let mut schemas = HashMap::new();
        rows.into_iter().map(|(k, v)| Ok((k.clone(), self.show(s, &k, v, &mut schemas)?))).collect()
    }

    fn show_change(&self, before: &State, after: &State, c: Change) -> Result<Change> {
        let show = |s: &State, v: Option<Val>| v.map(|v| self.show(s, &c.key, v, &mut HashMap::new())).transpose();
        Ok(Change { before: show(before, c.before.clone())?, after: show(after, c.after.clone())?, key: c.key })
    }

    /// Returns the branch's new version (see `batch`).
    pub fn put(&self, b: &str, k: &str, v: &str) -> Result<u64> {
        self.batch(b, &[(k, Some(v))])
    }

    /// Returns the branch's new version (see `batch`).
    pub fn del(&self, b: &str, k: &str) -> Result<u64> {
        self.batch(b, &[(k, None)])
    }

    /// Hybrid search over one table of a branch (see `Query`). Returns how many rows pass the
    /// filters, and the best `q.k` of them. The table's index is shared: a branch that has changed
    /// little since an indexed state searches that index plus its own changes.
    pub fn find(&self, b: &str, q: &Query) -> Result<(usize, Vec<Hit>)> {
        if reserved_table(&q.table) {
            return Err(Error::Constraint {
                code: "42501",
                message: format!("{}: tables starting with _sonos are the database's own", q.table),
            });
        }
        let _t = self.metrics.time(metrics::Op::Search);
        let View { born, s, .. } = self.view_tx(b)?;
        let (total, hits) = self.find_at(born, &s, q)?;
        let mut schemas = HashMap::new();
        let hits = hits
            .into_iter()
            .map(|h| Ok(Hit { row: self.show(&s, &h.key, h.row, &mut schemas)?, ..h }))
            .collect::<Result<_>>()?;
        Ok((total, hits))
    }

    fn find_at(&self, born: u64, s: &State, q: &Query) -> Result<(usize, Vec<Hit>)> {
        Ok(self.find_within(born, s, q, usize::MAX)?.unwrap_or_default())
    }

    /// `find_at`, or None when the table has no index close to `s` and building one would read
    /// more than `budget` bytes of rows.
    fn find_within(&self, born: u64, s: &State, q: &Query, budget: usize) -> Result<Option<Found>> {
        // agents repeat searches: an identical query on an unchanged branch is answered from memory
        let asked = (born, format!("{q:?}"));
        if let Some((v, found)) = self.answers.lock().unwrap().get(&asked)
            && v.same(s)
        {
            return Ok(Some(found.clone()));
        }
        let Some(found) = self.find_in(born, s, q, budget)? else { return Ok(None) };
        let mut a = self.answers.lock().unwrap();
        if a.len() >= 1024 {
            a.clear(); // ponytail: forget everything rather than track recency
        }
        a.insert(asked, (s.clone(), found.clone()));
        Ok(Some(found))
    }

    /// `find` without the rows: how many rows pass the filters, and the best keys with their
    /// scores. For callers that fetch the rows themselves (SQL's `ORDER BY <distance> LIMIT k`).
    pub fn find_keys(&self, b: &str, q: &Query) -> Result<(usize, Vec<(String, f32)>)> {
        self.find_keys_by(b, q, search::Metric::Dot)
    }

    /// `find_keys`, with vectors ranked by `m` (SQL's `<->` and `<=>`).
    pub fn find_keys_by(&self, b: &str, q: &Query, m: search::Metric) -> Result<(usize, Vec<(String, f32)>)> {
        let _t = self.metrics.time(metrics::Op::Search);
        let View { born, s, .. } = self.view_tx(b)?;
        Ok(self.rank(born, &s, q, usize::MAX, m)?.unwrap_or_default())
    }

    fn find_in(&self, born: u64, s: &State, q: &Query, budget: usize) -> Result<Option<Found>> {
        let Some((total, hits)) = self.rank(born, s, q, budget, search::Metric::Dot)? else { return Ok(None) };
        let hits = hits
            .into_iter()
            .filter_map(|(key, score)| match self.val(s, &key) {
                Ok(Some(row)) => Some(Ok(Hit { key, score, row })),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<_>>()?;
        Ok(Some((total, hits)))
    }

    /// The matches' count and the page of keys `q` asks for, best first; None past `budget` (see
    /// `index_for`).
    fn rank(&self, born: u64, s: &State, q: &Query, budget: usize, m: search::Metric) -> Result<Option<Ranked>> {
        if let Some((_, v)) = &q.vector
            && v.iter().any(|x| !x.is_finite())
        {
            return Err(Error::Constraint {
                code: "22023",
                message: "a search vector's numbers must be finite".into(),
            });
        }
        let s = s.clone();
        let key = (q.table.clone(), born);
        let cached = self.deltas.lock().unwrap().get(&key).filter(|(v, ..)| v.same(&s)).cloned();
        let (_, ix, delta) = match cached {
            Some(c) => c,
            None => {
                let Some((ix, changes)) = self.index_for(&s, &q.table, budget)? else { return Ok(None) };
                let entry = (s.clone(), ix.clone(), Arc::new(ix.delta(changes)));
                let mut d = self.deltas.lock().unwrap();
                if d.len() >= 1024 {
                    d.clear(); // ponytail: forget them all rather than track retired branches
                }
                d.insert(key, entry.clone());
                entry
            }
        };
        if let Some((f, v)) = &q.vector
            && let Some(dim) = ix.dim(f).filter(|d| *d != v.len())
        {
            // as pgvector says it
            return Err(Error::Constraint {
                code: "22000",
                message: format!("different vector dimensions {dim} and {}", v.len()),
            });
        }
        ix.start_graphs(); // big vector fields get an HNSW graph, built in the background
        let upto = Query { k: q.offset.saturating_add(q.k), ..q.clone() }; // rank through the end of this page
        // a row's stored vector, for ranking the last candidates exactly (see `TableIndex::search`)
        let reader = match &q.vector {
            Some(_) => Some(sql::SearchRows::of_schema(self.val(&s, &format!("{}/{}", sql::SCHEMAS, q.table))?)),
            None => None,
        };
        let field = q.vector.as_ref().map(|(f, _)| f.as_str());
        let exact = |key: &str| -> Result<Option<Arc<[f32]>>> {
            let Some(raw) = self.val(&s, key)? else { return Ok(None) };
            Ok(reader.as_ref().zip(field).and_then(|(r, f)| r.vector_of(&raw, f)))
        };
        let (total, mut hits) = ix.search(&upto, &delta, m, &exact)?;
        hits.drain(..q.offset.min(hits.len()));
        Ok(Some((total, hits)))
    }

    /// Whether every cached search index has finished building its vector graphs (they build in
    /// the background; searches scan until then). For tests and benchmarks.
    pub fn search_graphs_ready(&self) -> bool {
        self.indexes.lock().unwrap().values().flatten().all(|(_, ix)| ix.graphs_ready())
    }

    /// The newest cached index of `table` that `s` is close to, with `s`'s changes since it;
    /// otherwise a fresh index of `s`, or None if that would read more than `budget` bytes of rows
    /// (the index holds them all in memory, and several times their size).
    // ponytail: builds inline and keeps 4 anchors per table; build in the background and keep
    // anchors per branch lineage if large tables or many diverged branches make this slow.
    fn index_for(
        &self,
        s: &State,
        table: &str,
        budget: usize,
    ) -> Result<Option<(Arc<index::TableIndex>, Vec<Change>)>> {
        let prefix = format!("{table}/");
        // the distances its pgvector indexes are for: an index built for others (made before CREATE INDEX ...
        // vector_cosine_ops, say) is built again
        let metrics = sql::ann_metrics(self.val(s, &format!("{}/{table}", sql::SCHEMAS))?.as_ref());
        let cached = self.indexes.lock().unwrap().get(table).cloned().unwrap_or_default();
        for (anchor, ix) in cached.iter().rev().filter(|(_, ix)| ix.built_for(&metrics)) {
            let delta: Vec<Change> =
                self.state_diff(anchor, s)?.into_iter().filter(|c| c.key.starts_with(&prefix)).collect();
            if delta.len() <= 256.max(ix.len() / 20) {
                let delta = delta.into_iter().map(|c| self.show_change(anchor, s, c)).collect::<Result<_>>()?;
                return Ok(Some((ix.clone(), delta)));
            }
        }
        let (mut rows, mut bytes) = (vec![], 0);
        for r in self.rows(s, &prefix)? {
            let (k, v) = r?;
            if !k.starts_with(&prefix) {
                break;
            }
            bytes += k.len() + v.len();
            if bytes > budget {
                return Ok(None);
            }
            rows.push((k, v));
        }
        let schema = self.val(s, &format!("{}/{table}", sql::SCHEMAS))?;
        let rows = sql::search_rows(schema, rows);
        let ix = Arc::new(index::TableIndex::build_for(rows, &metrics));
        let mut c = self.indexes.lock().unwrap();
        let anchors = c.entry(table.to_string()).or_default();
        anchors.push((s.clone(), ix.clone()));
        if anchors.len() > 4 {
            anchors.remove(0);
        }
        Ok(Some((ix, vec![])))
    }

    /// Up to `limit` rows with key >= `from`, in key order.
    pub fn scan(&self, b: &str, from: &str, limit: usize) -> Result<Vec<(String, Val)>> {
        let s = self.view_tx(b)?.s;
        let rows = self.rows(&s, from)?.take(limit).collect::<Result<Vec<_>>>()?;
        self.show_all(&s, rows)
    }

    /// How many rows have a key starting with `prefix`, without holding them.
    pub fn prefix_len(&self, b: &str, prefix: &str) -> Result<usize> {
        let s = self.view_tx(b)?.s;
        let mut n = 0;
        for r in self.rows(&s, prefix)? {
            if !r?.0.starts_with(prefix) {
                break;
            }
            n += 1;
        }
        Ok(n)
    }

    /// All rows whose key starts with `prefix`, in key order.
    pub fn prefix(&self, b: &str, prefix: &str) -> Result<Vec<(String, Val)>> {
        let s = self.view_tx(b)?.s;
        let rows = self.rows(&s, prefix)?;
        let rows =
            rows.take_while(|r| r.as_ref().map_or(true, |(k, _)| k.starts_with(prefix))).collect::<Result<_>>()?;
        self.show_all(&s, rows)
    }

    /// O(1) regardless of data size, and never waits on writers of `from`.
    pub fn fork(&self, from: &str, name: &str) -> Result<()> {
        check_name(name)?;
        self.fork_info(from, name, Info::new())
    }

    /// `fork`, with metadata for the new world (a JSON object: owner, description, tags, ...).
    /// Returns the new world.
    pub fn fork_with(&self, from: &str, name: &str, meta: serde_json::Value) -> Result<World> {
        check_name(name)?;
        let mut meta = meta_object(meta)?;
        reserved_keys(&meta)?;
        stale::from_meta(&mut meta)?; // `check_reads = true`: its reads are checked at merge
        scope::from_meta(self, from, &mut meta)?; // what it may change, held to at its merge
        let info = Info { meta, ..Info::new() };
        self.fork_info(from, name, info)?;
        self.world(name)
    }

    /// A world with no parent and nothing in it: what `CREATE DATABASE` makes. It merges into
    /// nothing, so it is dropped rather than merged, and it is never discarded for being idle.
    pub fn create_empty(&self, name: &str, meta: serde_json::Value) -> Result<World> {
        check_name(name)?;
        let meta = meta_object(meta)?;
        reserved_keys(&meta)?;
        self.empty_world(name, Info { meta, ..Info::new() })?;
        self.world(name)
    }

    /// `create_empty`; also how the log's record of one (a `World` op with no source) replays.
    fn empty_world(&self, name: &str, info: Info) -> Result<()> {
        let _t = self.metrics.time(metrics::Op::Fork);
        let most = self.setting(Setting::MaxWorlds);
        let rec = self.record(|| {
            let meta = serde_json::Value::Object(info.meta.clone()).to_string();
            Op::World(String::new(), name.to_string(), info.id.clone(), info.created, meta)
        });
        let pos = {
            let _g = self.gate();
            let mut g = self.log.as_ref().map(|l| l.lock());
            let mut m = self.branches.write().unwrap();
            if m.contains_key(name) {
                return Err(Error::Exists(name.into()));
            }
            if most > 0 && m.keys().filter(|n| *n != MAIN && !n.starts_with("_tx_")).count() as u64 >= most {
                return Err(max_worlds(most));
            }
            let born = self.forks.fetch_add(1, Ordering::Relaxed);
            let nothing = || State::empty(self.empty.map(|h| self.track(h)));
            let br = Branch::new(born, nothing(), nothing(), info);
            br.start.store(self.lock_clock.load(Ordering::SeqCst), Ordering::SeqCst);
            m.add(name.into(), None, br);
            self.append_in(&mut g, rec)
        };
        self.after(pos, false);
        Ok(())
    }

    /// Forks `from` once for each world named, all from one version of it, which it returns.
    fn fork_all(&self, from: &str, worlds: Vec<(String, Info)>) -> Result<State> {
        worlds.iter().try_for_each(|(name, _)| check_name(name))?; // generated names too (`<prefix>_<i>`)
        let _t = self.metrics.time(metrics::Op::Fork);
        let most = self.setting(Setting::MaxWorlds);
        let recs: Vec<_> = worlds
            .iter()
            .map(|(name, info)| {
                self.record(|| {
                    let meta = serde_json::Value::Object(info.meta.clone()).to_string();
                    Op::World(from.to_string(), name.clone(), info.id.clone(), info.created, meta)
                })
            })
            .collect();
        let (pos, forked) = {
            let _g = self.gate();
            let src = self.branch(from)?;
            // `from` as it is, its batch, the log, then the map, as `fork_info` takes them
            let data = src.snap.read().unwrap(); // held through the appends: orders us against writes to `from`
            let mut batch = lock(&src.batch);
            let mut g = self.log.as_ref().map(|l| l.lock());
            let mut m = self.branches.write().unwrap();
            if !m.contains_key(from) {
                return Err(Error::NoBranch(from.into())); // merged or discarded meanwhile
            }
            if let Some((name, _)) = worlds.iter().find(|(n, _)| m.contains_key(n)) {
                return Err(Error::Exists(name.clone()));
            }
            if most > 0
                && m.keys().filter(|n| *n != MAIN && !n.starts_with("_tx_")).count() as u64 + worlds.len() as u64 > most
            {
                return Err(max_worlds(most));
            }
            self.flush_batch_in(&mut g, &mut batch); // the forks start from them
            let mut pos = 0;
            for ((name, info), rec) in worlds.into_iter().zip(recs) {
                let born = self.forks.fetch_add(1, Ordering::Relaxed);
                m.add(name, Some(from.into()), Branch::new(born, data.clone(), data.clone(), info));
                pos = self.append_in(&mut g, rec);
            }
            (pos, data.clone())
        };
        self.after(pos, false);
        Ok(forked)
    }

    /// World `b` now, and a moment (ms since 1970) that time travel reads it at exactly: writes
    /// to `b` wait (up to a millisecond) until the clock has passed it.
    fn pinned(&self, b: &str) -> Result<(State, u64)> {
        let br = self.branch(b)?;
        let s = br.snap.read().unwrap();
        let t = now_ms();
        while now_ms() <= t {
            std::hint::spin_loop();
        }
        Ok((s.clone(), t))
    }

    fn fork_info(&self, from: &str, name: &str, info: Info) -> Result<()> {
        let _t = (!name.starts_with("_tx_")).then(|| self.metrics.time(metrics::Op::Fork)); // not BEGIN's
        let most = if name.starts_with("_tx_") { 0 } else { self.setting(Setting::MaxWorlds) };
        let rec = self.record(|| {
            let meta = serde_json::Value::Object(info.meta.clone()).to_string();
            Op::World(from.to_string(), name.to_string(), info.id.clone(), info.created, meta)
        });
        let pos = {
            let _g = self.gate();
            let src = self.branch(from)?;
            // `from` as it is, its batch, the log, then the map: the map is held only to add the
            // world and buffer its record, not while this waits its turn at the log behind every
            // agent's writes (lookups that miss the thread's cache wait on the map)
            let data = src.snap.read().unwrap(); // held through the append: orders us against writes to `from`
            let mut batch = lock(&src.batch);
            let mut g = self.log.as_ref().map(|l| l.lock());
            let mut m = self.branches.write().unwrap();
            if !m.contains_key(from) {
                return Err(Error::NoBranch(from.into())); // merged or discarded meanwhile
            }
            if m.contains_key(name) {
                return Err(Error::Exists(name.into()));
            }
            // ponytail: counts every world per fork while max_worlds is set; keep a count if 100k+ worlds
            if most > 0 && m.keys().filter(|n| *n != MAIN && !n.starts_with("_tx_")).count() as u64 >= most {
                return Err(max_worlds(most));
            }
            src.active.store(self.now(), Ordering::Relaxed);
            self.flush_batch_in(&mut g, &mut batch); // the fork starts from them
            let born = self.forks.fetch_add(1, Ordering::Relaxed);
            // what it reads is kept from now if it checks its reads (a transaction's go in its
            // world's); replaying the log, a world's earlier reads aren't known (see `stale`)
            let reads = match name.starts_with("_tx_") {
                true => src.recorder(),
                false if info.meta.contains_key(stale::CHECK_READS) && self.me.get().is_some() => {
                    Some(Arc::new(Mutex::new(stale::ReadSet::complete())))
                }
                false => None,
            };
            let br = Branch::new(born, data.clone(), data.clone(), info);
            // (the rows `from` has written so far, which `rebase_tx` counts the world's writes since from)
            br.synced_rows.store(src.changed.load(Ordering::Relaxed), Ordering::Relaxed);
            *br.reads.write().unwrap() = reads;
            // read with `data` held, as a merge publishes and moves the clock on with it held
            br.start.store(self.lock_clock.load(Ordering::SeqCst), Ordering::SeqCst);
            m.add(name.into(), Some(from.into()), br);
            self.append_in(&mut g, rec)
        };
        self.after(pos, false);
        Ok(())
    }

    /// What a world costs on its own: the rows it changed since its fork, and the pages and bytes
    /// only it holds (its changed rows kept in memory and the checkpoint count as bytes too).
    /// For main, its whole tree (and `rows` is None).
    pub fn usage(&self, b: &str) -> Result<Usage> {
        let br = self.branch(b)?;
        let (base, now) = (br.base(), br.snapshot());
        let main = b == MAIN;
        let mut over = 0;
        let mut add = |k: &str, v: &Option<Val>| over += (k.len() + v.as_ref().map_or(0, |v| v.len())) as u64;
        if main {
            now.over.iter().for_each(|(k, v)| add(k, v));
        } else {
            for d in base.over.diff(&now.over) {
                match d {
                    DiffItem::Add(k, v) | DiffItem::Update { new: (k, v), .. } => add(k, v),
                    DiffItem::Remove(..) => {}
                }
            }
        }
        let (mut pages, mut bytes) = (0, over);
        if let (Some(p), Some(t)) = (&self.pages, &now.tree) {
            let (n, size) = match (&base.tree, main) {
                (Some(b), false) => prolly::fresh(p, &b.0, &t.0)?,
                _ => {
                    let all = prolly::reachable(p, &[*t.0])?;
                    (all.len(), all.iter().map(|h| p.size(h).unwrap_or(0)).sum())
                }
            };
            (pages, bytes) = (n, bytes + size);
        }
        let rows =
            if main { None } else { Some(self.state_diff(&base, &now)?.into_iter().filter(|c| shown(&c.key)).count()) };
        Ok(Usage { world: b.into(), rows, pages, bytes })
    }

    /// Changes per table (the key's part before '/') from `b`'s fork point to `b` now, or from
    /// world `b` to world `to` (as `diff_between`), counted without keeping any change.
    pub fn diff_count(&self, b: &str, to: Option<&str>) -> Result<std::collections::BTreeMap<String, usize>> {
        let _t = self.metrics.time(metrics::Op::Diff);
        let (x, y) = self.diff_ends(b, to)?;
        let mut tables = std::collections::BTreeMap::new();
        for c in self.state_diff_iter(&x, &y, None)? {
            let c = c?;
            if shown(&c.key) {
                *tables.entry(c.key.split_once('/').map_or(c.key.as_str(), |t| t.0).to_string()).or_insert(0) += 1;
            }
        }
        Ok(tables)
    }

    /// `diff_count`, split per table into rows deleted, updated and inserted (see `sql::summary`): what a review
    /// leads with, deletes first.
    pub fn diff_kinds(&self, b: &str, to: Option<&str>) -> Result<std::collections::BTreeMap<String, sql::Counts>> {
        let _t = self.metrics.time(metrics::Op::Diff);
        let (x, y) = self.diff_ends(b, to)?;
        let mut tables: std::collections::BTreeMap<String, sql::Counts> = std::collections::BTreeMap::new();
        for c in self.state_diff_iter(&x, &y, None)? {
            let c = c?;
            if shown(&c.key) {
                let t = c.key.split_once('/').map_or(c.key.as_str(), |t| t.0);
                tables.entry(t.to_string()).or_default().add(c.before.is_some(), c.after.is_some());
            }
        }
        Ok(tables)
    }

    /// One page of `diff` (or `diff_between` with `to`): up to `limit` changes with key > `after`,
    /// in key order, and the key to pass as `after` for the next page (None on the last). Reads
    /// only as far as the page, so memory follows the page, not the diff.
    pub fn diff_page(
        &self,
        b: &str,
        to: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<Change>, Option<String>)> {
        self.diff_page_of(b, to, after, limit, None)
    }

    /// `diff_page` of only the rows deleted (`deletes` true) or only the others (false): a review lists the deletes
    /// first, a page at a time, then the rest.
    pub fn diff_page_of(
        &self,
        b: &str,
        to: Option<&str>,
        after: Option<&str>,
        limit: usize,
        deletes: Option<bool>,
    ) -> Result<(Vec<Change>, Option<String>)> {
        let _t = self.metrics.time(metrics::Op::Diff);
        let (x, y) = self.diff_ends(b, to)?;
        let kind = |c: &Change| deletes.is_none_or(|d| d == (c.before.is_some() && c.after.is_none()));
        let mut it =
            self.state_diff_iter(&x, &y, after)?.filter(|c| c.as_ref().map_or(true, |c| shown(&c.key) && kind(c)));
        let mut out = vec![];
        for c in it.by_ref().take(limit) {
            out.push(self.show_change(&x, &y, c?)?);
        }
        let more = it.next().transpose()?.is_some();
        let next = if more { out.last().map(|c| c.key.clone()) } else { None };
        Ok((out, next))
    }

    /// The two states a diff compares: `b`'s fork point and `b`, or world `b` and world `to`.
    fn diff_ends(&self, b: &str, to: Option<&str>) -> Result<(State, State)> {
        not_tx(b)?;
        match to {
            Some(to) => {
                not_tx(to)?;
                Ok((self.view(b)?.s, self.view_tx(to)?.s))
            }
            None => {
                let br = self.branch(b)?;
                Ok((br.base(), br.snapshot()))
            }
        }
    }

    /// What changed in `b` since it was forked, in key order.
    pub fn diff(&self, b: &str) -> Result<Vec<Change>> {
        not_tx(b)?;
        let _t = self.metrics.time(metrics::Op::Diff);
        let br = self.branch(b)?;
        let (base, now) = (br.base(), br.snapshot());
        let changes = self.state_diff(&base, &now)?.into_iter().filter(|c| shown(&c.key));
        changes.map(|c| self.show_change(&base, &now, c)).collect()
    }

    /// Three-way merge into the parent, then drop the branch. Returns rows applied.
    /// With `Resolve::Fail`, conflicts come back as data and nothing changes.
    /// The new parent version is built off to the side and swapped in, so readers never wait.
    /// Durable when it returns.
    pub fn merge(&self, b: &str, how: Resolve) -> Result<usize> {
        self.merge_at(b, how, None)
    }

    /// `merge`, but only if the branch is at `expected`, the version its last write returned. That
    /// catches writes lost in a crash, which a plain merge can only suspect: after a crash, merging
    /// a branch that was open needs its version (check `diff` first).
    pub fn merge_at(&self, b: &str, how: Resolve, expected: Option<u64>) -> Result<usize> {
        self.merge_with(b, &MergeOptions { how, expected, ..Default::default() })
    }

    /// Three-way merge into the parent, then drop the branch, settling rows both sides changed
    /// as `o` says: row by row (picks), by columns, then by `o.how`. Returns rows applied.
    /// With `o.only_tables` / `o.only_keys` only those rows merge, and with `o.into` they go to
    /// another world; either way the branch stays open.
    pub fn merge_with(&self, b: &str, o: &MergeOptions) -> Result<usize> {
        self.merge_kept(b, o).map(|(n, _)| n)
    }

    /// `merge_with`, and for a branch that stays open (merged in part, or into another world) its
    /// version as the merge left it, read under the merge's locks: what the caller merges at next,
    /// with no other writer's change in it.
    pub fn merge_kept(&self, b: &str, o: &MergeOptions) -> Result<(usize, Option<u64>)> {
        let _t = self.metrics.time(if b.starts_with("_tx_") { metrics::Op::Commit } else { metrics::Op::Merge });
        // the rows a merge brings count toward the parent's changes, and its agent's quota there
        let parent = {
            let m = self.branches.read().unwrap();
            let e = m.get(b).ok_or_else(|| Error::NoBranch(b.into()))?;
            let p = e.parent.clone().ok_or(Error::IsMain)?;
            m.get(&p).map(|pe| (p, pe.br.clone()))
        };
        let into = o.into.as_deref().filter(|y| parent.as_ref().is_none_or(|(p, _)| p != y));
        let whole = into.is_none() && o.only_tables.is_empty() && o.only_keys.is_empty();
        // a merge that leaves the branch open counts the rows it applies (in `merge_locked`)
        let brought = if whole { self.changed(b)? } else { 0 };
        if whole
            && let Some((p, pbr)) = &parent
            && let Some(q) = self.quota(p)?
        {
            q.check(p, pbr.changed.load(Ordering::Relaxed), brought)?;
        }
        let picks = settles(o);
        // a write done in batches to either side finishes first (see `batch_long`)
        let target = into.map(String::from).or_else(|| parent.as_ref().map(|(p, _)| p.clone()));
        // into main of a folder attached to Postgres: one write-back at a time, through main taking what it sent back
        let _attached = attach::before_merge(self, target.as_deref() == Some(MAIN) && !b.starts_with("_tx_"))?;
        let mut sides: Vec<Arc<Branch>> =
            [Some(b.to_string()), target].iter().flatten().filter_map(|x| self.branch(x).ok()).collect();
        sides.sort_by_key(|x| std::cmp::Reverse(x.born));
        sides.dedup_by(|a, b| Arc::ptr_eq(a, b)); // `merge world y into y`: its error comes later, not a lock taken twice
        let longs: Vec<_> = sides.iter().map(|x| lock(&x.long)).collect();
        // a transaction too big to merge row by row is first brought up to what its world committed since it last was,
        // as a READ COMMITTED statement is (see `rebase_tx`), so that its own state is the merge's result (see
        // `merge_published`); a row both changed is left to the merge, which refuses it as before
        if b.starts_with("_tx_")
            && whole
            && parent.as_ref().is_some_and(|(p, _)| !p.starts_with("_tx_"))
            && let Ok(br) = self.branch(b)
            && big_tx(&br.base(), &br.snapshot())
        {
            let _ = self.rebase_tx(b);
        }
        outbox::LANDED.take(); // (a merge that failed after `landing` left its own)
        let merged = {
            let _g = self.gate();
            self.merge_locked(b, o, &picks, Mode::Merge)
        };
        drop(longs);
        // a refusal is counted once the merge's locks are let go; a merge that goes through does nothing more
        let merged = match merged {
            Ok(m) => m,
            Err(e) => {
                self.note_refusal(b, &e);
                attach::after_failed(self); // a conflict with Postgres: main takes its rows
                return Err(e);
            }
        };
        let (n, pos, notes, kept) = match merged {
            Merged::Done(n, pos, notes, kept) => (n, pos, notes, kept),
            Merged::Plan(..) => unreachable!("only a dry run plans"),
            Merged::Review(p, reasons, version) => {
                self.ask_review(b, &p, &reasons, version)?;
                return Err(agents::needs_review(b, &p, &reasons));
            }
        };
        // like `write`, only a merge into main waits for the disk: worlds and transactions
        // inside one become durable when they reach main
        let durable = into.map_or(parent.as_ref().is_some_and(|(p, _)| p == MAIN), |y| y == MAIN);
        self.touch(into.or(parent.as_ref().map(|(p, _)| p.as_str())).unwrap_or(MAIN)); // where the rows went
        if let Some((_, pbr)) = parent {
            pbr.changed.fetch_add(brought, Ordering::Relaxed);
        }
        self.after(pos, durable);
        // written back to Postgres: main takes the rows Postgres changed as it wrote them
        attach::after_merge(self);
        self.keep_landed(); // the effects it brought into main, and who let them through

        // effects it brought into main happen now: their NOTIFYs, as the rows are there
        sql::deliver(self, MAIN, 0, &notes);
        if !whole && into.is_none() {
            self.partly_merged(b)?; // a queued world, partly approved: its entry says so
        }
        Ok((n, kept))
    }

    /// What `merge_with(b, o)` would do, row by row, without doing it: conflicts are listed, not
    /// errors. Nothing changes.
    pub fn merge_preview(&self, b: &str, o: &MergeOptions) -> Result<MergePlan> {
        let picks = settles(o);
        let (mut plan, dry) = {
            let _g = self.gate();
            match self.merge_locked(b, o, &picks, Mode::Dry)? {
                Merged::Plan(plan) => *plan,
                Merged::Done(..) | Merged::Review(..) => unreachable!("a dry run only plans"),
            }
        };
        if plan.blocked.is_none() && self.branch(b)?.flagged.load(Ordering::Relaxed) {
            plan.blocked = Some(format!(
                "{b} was open during a crash, so its last writes may be lost: check its diff, then merge with CONFIRM"
            ));
        }
        // into main of a folder attached to Postgres: checked there too, now that the merge's locks are let go
        if let (Some(dry), Some(att)) = (dry, self.attached.as_deref()) {
            let live = attach::dry_run(self, att, dry);
            plan.blocked = plan.blocked.or_else(|| attach::dry_blocked(&live));
            plan.live = Some(live);
        }
        Ok(plan)
    }

    /// Moves world `b`'s fork point up to its parent as it is now, its own changes on top: what a
    /// merge would leave, settled as `o` says (rows both changed: picks, by columns, then
    /// `o.how`), kept in the world instead of the parent. The parent doesn't change, and no merge
    /// policy judges it (the merge that follows is judged). Refused, with nothing changed, on a
    /// conflict nothing settles, on constraints the two sides break together, and, for a world
    /// that checks its reads, when a row it read changed in the parent since the fork: the
    /// decision it made on it needs making again, which a rebase can't do for it. Returns how many
    /// rows of the parent's it took in. A long-running world rebases to pick up what changed
    /// under it, and settle conflicts as they come rather than all at its merge.
    pub fn rebase(&self, b: &str, o: &MergeOptions) -> Result<usize> {
        not_tx(b)?;
        if o.into.is_some() || !o.only_tables.is_empty() || !o.only_keys.is_empty() {
            return Err(Error::Constraint {
                code: "22023",
                message: "a rebase takes its parent whole: no INTO, ONLY TABLES or ONLY KEYS".into(),
            });
        }
        let picks = settles(o);
        let parent = {
            let m = self.branches.read().unwrap();
            let e = m.get(b).ok_or_else(|| Error::NoBranch(b.into()))?;
            e.parent.clone().ok_or(Error::IsMain)?
        };
        // a write done in batches to either side finishes first (see `batch_long`), as for a merge
        let mut sides: Vec<Arc<Branch>> = [b, parent.as_str()].iter().filter_map(|x| self.branch(x).ok()).collect();
        sides.sort_by_key(|x| std::cmp::Reverse(x.born));
        let longs: Vec<_> = sides.iter().map(|x| lock(&x.long)).collect();
        let done = {
            let _g = self.gate();
            self.merge_locked(b, o, &picks, Mode::Rebase)?
        };
        drop(longs);
        let Merged::Done(n, pos, ..) = done else { unreachable!("a rebase is done or refused") };
        self.touch(b);
        self.after(pos, false);
        Ok(n)
    }

    /// A row both sides changed, merged by columns: the parent's row with the columns this branch
    /// changed, if the two changed different columns. Stored as the parent stores it.
    fn by_columns(&self, key: &str, states: [&State; 3], vals: [&Option<Val>; 3]) -> Option<Val> {
        let [Some(b), Some(o), Some(t)] = vals else { return None }; // an add or delete: not by columns
        let obj = |s: &State, v: &Val| match self
            .show(s, key, v.clone(), &mut HashMap::new())
            .ok()
            .map(|j| serde_json::from_str(&j))
        {
            Some(Ok(serde_json::Value::Object(o))) => Some(o),
            _ => None,
        };
        let (bo, oo, mut to) = (obj(states[0], b)?, obj(states[1], o)?, obj(states[2], t)?);
        let differs = |x: &serde_json::Map<String, serde_json::Value>, k: &String| x.get(k) != bo.get(k);
        let keys: std::collections::BTreeSet<String> = bo.keys().chain(oo.keys()).chain(to.keys()).cloned().collect();
        let mine: Vec<&String> = keys.iter().filter(|k| differs(&oo, k)).collect();
        if mine.iter().any(|k| differs(&to, k)) {
            return None; // both changed a column
        }
        for k in mine {
            match oo.get(k) {
                Some(v) => to.insert(k.clone(), v.clone()),
                None => to.remove(k),
            };
        }
        let parent = View { core: self, born: PAST, s: states[2].clone(), branch: String::new(), reads: None };
        sql::stored(&parent, key, &serde_json::Value::Object(to).to_string()).ok()
    }

    /// The merge itself (see `merge_with`), or with `dry` its plan. `o.picks` is ignored: rows
    /// settled one by one come in `picks` (replay gives them as they were stored).
    fn merge_locked(&self, b: &str, o: &MergeOptions, picks: &HashMap<String, Settle>, mode: Mode) -> Result<Merged> {
        let (how, expected, columns) = (o.how, o.expected, o.columns);
        let (dry, rebase) = (mode == Mode::Dry, mode == Mode::Rebase);
        // `parent` is the world merged into: the branch's parent, or `into`
        let (child, parent, name, into) = {
            let m = self.branches.read().unwrap();
            let e = m.get(b).ok_or_else(|| Error::NoBranch(b.into()))?;
            let p = e.parent.as_ref().ok_or(Error::IsMain)?;
            match o.into.as_deref().filter(|y| y != p) {
                // a parent can't go away while it has children
                None => (e.br.clone(), m[p].br.clone(), p.clone(), None),
                Some(y) if y == b => {
                    return Err(Error::Constraint { code: "22023", message: format!("can't merge {b} into itself") });
                }
                Some(y) => {
                    let t = m.get(y).ok_or_else(|| Error::NoBranch(y.into()))?;
                    (e.br.clone(), t.br.clone(), y.to_string(), Some(y))
                }
            }
        };
        // the branch stays open: only some of its rows merge, or they go to another world
        let stays = into.is_some() || !o.only_tables.is_empty() || !o.only_keys.is_empty();
        let (mut gone, parent_gone) = if parent.born > child.born {
            let p = lock(&parent.writer);
            (lock(&child.writer), p)
        } else {
            let c = lock(&child.writer);
            (c, lock(&parent.writer))
        };
        if *gone {
            return Err(Error::NoBranch(b.into()));
        }
        if *parent_gone {
            return Err(Error::NoBranch(name)); // `into` merged or discarded meanwhile
        }
        let actual = child.version.load(Ordering::Relaxed);
        if !dry
            && (expected.is_some_and(|e| e != actual) || (expected.is_none() && child.flagged.load(Ordering::Relaxed)))
        {
            return Err(Error::Stale { branch: b.into(), expected, actual });
        }
        // and the world merged into, when the caller settled rows against it as a preview saw it
        let target_now = parent.stamp.load(Ordering::Relaxed);
        if o.target.is_some_and(|t| !dry && t != target_now) {
            return Err(Error::Constraint {
                code: "55000",
                message: format!(
                    "{name} changed since the preview this merge settles rows by (its target): preview again; nothing was merged"
                ),
            });
        }
        let (base_state, ours_state) = (child.base(), child.snapshot());
        // a transaction (or savepoint) too big to merge row by row in memory: its own state is the result, when the
        // world it goes into has nothing it hasn't seen (see `merge_published`); a transaction's world it was brought up
        // to at last (see `merge_with`), a savepoint's transaction changes only so
        if !dry
            && !stays
            && b.starts_with("_tx_")
            && picks.is_empty()
            && !(name == MAIN && self.attached.is_some() && !RECOVERING.get()) // (main's guard below refuses it)
            && big_tx(&base_state, &ours_state)
            && lock(&child.locked).is_empty()
        {
            let theirs_state = parent.snapshot();
            // (a checkpoint since may have put the same rows under another tree)
            let seen = name.starts_with("_tx_") || base_state.same(&theirs_state) || {
                let synced = lock(&child.synced).clone();
                synced.same(&theirs_state) || self.state_diff_iter(&synced, &theirs_state, None)?.next().is_none()
            };
            let rebased = child.rebased.load(Ordering::Relaxed);
            if seen
                && let Some(next) =
                    self.merge_published(&base_state, &ours_state, &theirs_state, parent.born, rebased)?
            {
                let notes = if name == MAIN { self.landed(&theirs_state, &next)? } else { vec![] };
                let rec = self.record(|| Op::Merge(b.to_string(), how, expected));
                self.flush_batch(&child); // replay merges them
                self.flush_batch(&parent);
                let pos = self.retire(b, rec, &parent, next, || {})?;
                *gone = true;
                return Ok(Merged::Done(child.changed.load(Ordering::Relaxed) as usize, pos, notes, None));
            }
        }
        let mut diff = only(self.state_diff(&base_state, &ours_state)?, o)?;
        // a transaction's row picked as its own that it left as it forked, while the parent
        // changed it (an ALTER SEQUENCE setting back what another commit changed): its own stands
        if b.starts_with("_tx_") {
            for (k, _) in picks.iter().filter(|(_, s)| matches!(s, Settle::Ours)) {
                if let Err(at) = diff.binary_search_by(|c| c.key.as_str().cmp(k)) {
                    let v = self.val(&ours_state, k)?;
                    diff.insert(at, Change { key: k.clone(), before: v.clone(), after: v });
                }
            }
        }
        // a world that checks its reads: what it read must be as it was when it forked (see `stale`)
        // (replaying the log: a merge that happened passed then, and earlier reads aren't known now)
        let read_set = if b.starts_with("_tx_") || self.me.get().is_none() { None } else { child.recorder() };
        let wrote: std::collections::HashSet<String> = match &read_set {
            Some(_) => diff.iter().map(|c| c.key.clone()).collect(),
            None => Default::default(),
        };
        // rows merged into the parent on their own are settled for this branch: its fork point
        // moves on to them, so a later merge takes only what changed since
        let settled: Vec<(String, Option<Val>)> = match stays && into.is_none() && !dry {
            true => diff.iter().map(|c| (c.key.clone(), c.after.clone())).collect(),
            false => vec![],
        };
        let mut next = parent.snapshot();
        let theirs_state = next.clone();
        let parent_view =
            View { core: self, born: parent.born, s: theirs_state.clone(), branch: String::new(), reads: None };

        let (mut apply, mut conflicts) = (vec![], vec![]);
        let mut schema_clash = vec![];
        let mut outcomes: HashMap<String, (&'static str, Option<Val>)> = HashMap::new(); // for the plan
        let mut logged: Vec<(String, u8, Option<String>)> = vec![]; // rows settled here, for the log
        // the acting agent's merge policy, checked below on the rows about to merge, under the
        // locks: what's checked is what merges; merge checks likewise, on the merged result (a
        // session's transaction commits into its own world: a write, not a merge)
        let tx = b.starts_with("_tx_");
        let policy = match tx || rebase {
            true => None, // a rebase changes only the world: its merge is judged when it merges
            false => self.acting_policy()?,
        };
        // a folder attached to Postgres: an agent's merge into main, which writes there, goes to a person unless the
        // agent has a merge policy of its own to judge it
        let attached_review = !tx
            && !rebase
            && name == MAIN
            && policy.is_none()
            && self.attached.is_some()
            && !disk::actor().is_empty()
            && !RECOVERING.get();
        let mut overwrote = 0; // rows the parent changed since the fork that this merge overwrites
        let mut by_table: HashMap<String, bool> = HashMap::new(); // tables' merge_by_columns
        // what the world was forked to change (see `scope`); not replaying the log: it was held then
        let declared = match tx || rebase || self.me.get().is_none() {
            true => None,
            false => child.info.read().unwrap().meta.get(scope::SCOPE).cloned(),
        };
        let columns_reviewed = policy.as_ref().is_some_and(|p| !p.review_columns.is_empty());
        for ch in diff {
            let theirs = self.val(&next, &ch.key)?;
            // Apply only where the parent still holds the value this branch forked from. If the
            // parent changed the row too, that's a conflict even when both sides wrote the same
            // value: the branch computed its write from a value that is gone (first merge wins).
            if theirs == ch.before {
                apply.push(ch);
                continue;
            }
            if (ch.key.starts_with("_sonos_schemas/") || ch.key.starts_with("_sonos_extensions/")) && theirs == ch.after
            {
                continue; // both sides made the same schema (or extension): it merges as one
            }
            if ch.key.starts_with("_sonos_tables/") {
                // both changed a table: its rows follow each side's own column layout, so picking
                // either schema would misread the other side's rows
                if theirs != ch.after {
                    schema_clash.push(ch.key.clone());
                    conflicts.push(Conflict { key: ch.key, base: ch.before, ours: ch.after, theirs });
                }
                continue;
            }
            // BY COLUMNS for this merge, or for this table (ALTER TABLE ... SET (merge_by_columns)),
            // as the world merged into has it
            let combine = columns || {
                let t = ch.key.split_once('/').map_or("", |(t, _)| t);
                !reserved_table(t)
                    && *by_table.entry(t.to_string()).or_insert_with(|| sql::merges_by_columns(&parent_view, t))
            };
            let settled = if sql::internal(&ch.key) {
                None // rows behind constraints follow the rows they index
            } else {
                match picks.get(&ch.key) {
                    Some(Settle::Json(j)) => {
                        let v = j.as_deref().map(|j| sql::stored(&parent_view, &ch.key, j)).transpose()?;
                        Some(("picked row", Settle::Stored(v)))
                    }
                    Some(Settle::Ours) => Some(("picked ours", Settle::Ours)),
                    Some(Settle::Theirs) => Some(("picked theirs", Settle::Theirs)),
                    Some(Settle::Stored(v)) => Some(("picked row", Settle::Stored(v.clone()))),
                    None if combine => self
                        .by_columns(
                            &ch.key,
                            [&base_state, &ours_state, &theirs_state],
                            [&ch.before, &ch.after, &theirs],
                        )
                        .map(|v| ("by columns", Settle::Stored(Some(v)))),
                    None => None,
                }
            };
            let settled = settled.or(match how {
                Resolve::Ours => Some(("kept ours", Settle::Ours)),
                Resolve::Theirs => Some(("kept theirs", Settle::Theirs)),
                Resolve::Fail => None,
            });
            match settled {
                Some((what, s)) => {
                    let kind = match (&s, what) {
                        (_, "kept ours" | "kept theirs") => None, // `how` says it, in the log too
                        (Settle::Ours, _) => Some((0, None)),
                        (Settle::Theirs, _) => Some((1, None)),
                        (Settle::Stored(v), _) => Some((2, v.as_deref().map(String::from))),
                        (Settle::Json(_), _) => unreachable!("stored above"),
                    };
                    if let Some((k, v)) = kind {
                        logged.push((ch.key.clone(), k, v));
                    }
                    let result = match s {
                        Settle::Ours => ch.after.clone(),
                        Settle::Theirs => theirs.clone(),
                        Settle::Stored(v) => v,
                        Settle::Json(_) => unreachable!(),
                    };
                    outcomes.insert(ch.key.clone(), (what, result.clone()));
                    overwrote += (result != theirs && what != "by columns" && shown(&ch.key)) as usize;
                    if result != theirs {
                        apply.push(Change { key: ch.key, before: ch.before, after: result });
                    }
                }
                None => conflicts.push(Conflict { key: ch.key, base: ch.before, ours: ch.after, theirs }),
            }
        }
        // what the merge does to the parent, as JSON, for the scope and the policy's columns: each
        // row it applies, from the parent's value now to what it gets (a row combined by columns
        // changes only the world's columns; a picked row, or OURS over the parent's change,
        // changes what it changes), and each conflicting row as the world changed it
        let merge_rows = match declared.is_some() || columns_reviewed {
            true => {
                let mut rows = vec![];
                for c in apply.iter().filter(|c| shown(&c.key)) {
                    let now = self.val(&theirs_state, &c.key)?;
                    // a row picked as JSON, or combined by columns, is stored in the parent's
                    // layout; the rest are the world's own
                    let made = outcomes.get(&c.key).is_some_and(|o| matches!(o.0, "picked row" | "by columns"));
                    let after_state = if made { &theirs_state } else { &ours_state };
                    let c = Change { key: c.key.clone(), before: now, after: c.after.clone() };
                    rows.extend(self.show_change(&theirs_state, after_state, c).ok());
                }
                for c in conflicts.iter().filter(|c| shown(&c.key)) {
                    let c = Change { key: c.key.clone(), before: c.base.clone(), after: c.ours.clone() };
                    rows.extend(self.show_change(&base_state, &ours_state, c).ok());
                }
                sql::describe(&rows)
            }
            false => vec![],
        };
        let strayed = declared.as_ref().map_or(vec![], |s| scope::outside(s, &merge_rows));
        // outside its scope, merged by anyone keeping to no merge policy: refused as it is
        if let (Some(s), None, false) = (&declared, &policy, strayed.is_empty() || dry) {
            return Err(scope::refused(b, s, &strayed));
        }
        let mut broken = vec![];
        if let Some(p) = &policy {
            // the rows the merge would change in the parent: not those settled as the parent has them
            broken = p.broken(&apply, overwrote);
            broken.extend(strayed.iter().map(|r| format!("{r}, outside its declared scope")));
            broken.extend(p.column_reviews(&merge_rows));
            // a world made some other way than a fork that declared one (simulate keeps worlds;
            // one forked before its policy asked): its merge says so, and a person looks
            let scoped = declared.as_ref().is_some_and(|s| s.get("may_change").is_some() || s.get("tenant").is_some());
            if p.require_scope && !tx && !scoped {
                broken.push(format!(
                    "declares no scope (merge policy {} has its agents say what a world may change)",
                    p.name
                ));
            }
            // the world's age: the longer it's open, the older what its merge rests on
            broken.extend(p.age_broken(child.info.read().unwrap().created, disk::now_ms()));
            // and with the agent's other merges into main in the last hour (see `tally`)
            if name == MAIN {
                broken.extend(p.hourly_broken(self, &disk::actor(), &apply));
            }
            // columns a reader marked critical reads (see `sql::impact`): a person looks first.
            // Readers are traced over rows as JSON, as diffs show them, and only once some reader
            // is marked
            if !p.critical && parent_view.prefix("_sonos_critical/")?.next().is_some() {
                let rows: Vec<Change> = apply
                    .iter()
                    .filter(|c| shown(&c.key))
                    .filter_map(|c| self.show_change(&base_state, &ours_state, c.clone()).ok())
                    .collect();
                let why = sql::critical_reasons(self, &parent_view, &rows)
                    .map_err(|e| Error::Constraint { code: e.code, message: e.message })?;
                broken.extend(why);
            }
        }
        // a merge its policy sends to a person is queued even if it couldn't merge as it is: with
        // the merge checks it breaks too when there's a result to check them on (see below)
        if attached_review {
            broken.push(format!(
                "writes to Postgres ({}), which an agent's merge does only under a merge policy of its own",
                self.attached.as_deref().map_or(String::new(), |a| a.source())
            ));
        }
        let review = |why: Vec<String>| match (&policy, attached_review) {
            (Some(p), _) if !dry && !why.is_empty() => Some(Merged::Review(p.name.clone(), why, actual)),
            (None, true) if !dry && !why.is_empty() => Some(Merged::Review(ATTACHED_REVIEW.into(), why, actual)),
            _ => None,
        };
        // B-tree index entries are never merged themselves: a row can win while its entries would
        // lose, so they are rebuilt below from the rows the merge changed
        apply.retain(|c: &Change| !sql::btree_entry(&c.key));
        conflicts.retain(|c| !sql::btree_entry(&c.key));
        // conflicts: no result to check, and the merge fails as it is
        if (!conflicts.is_empty() || !schema_clash.is_empty())
            && let Some(r) = review(broken.clone())
        {
            return Ok(r);
        }
        // rows SQL keeps behind UNIQUE and foreign keys: two sides writing the same one is a
        // constraint broken, not a row to pick a side for
        if let Some(c) = conflicts.iter().find(|c| sql::internal(&c.key))
            && conflicts.iter().all(|c| sql::internal(&c.key))
        {
            let what = c.key.splitn(4, '/').skip(1).collect::<Vec<_>>().join(" ");
            return Err(Error::Constraint {
                code: "23505",
                message: format!("both sides added rows with the same unique values ({what}); nothing was merged"),
            });
        }
        conflicts.retain(|c| !sql::internal(&c.key)); // resolved along with the rows they index
        if dry {
            let (mut p, merged) = self.plan(
                &apply,
                &conflicts,
                &outcomes,
                &schema_clash,
                [&base_state, &ours_state, &theirs_state],
                parent.born,
                next,
            )?;
            p.version = actual;
            p.target = target_now;
            if !tx && !rebase {
                p.confirm_deletes = self.deletes_to_confirm(b, &apply)?;
            }
            // with conflicts nothing would merge, so there's no result to check yet
            if !tx && p.blocked.is_none() && p.conflicts == 0 {
                let keys: Vec<String> = apply.iter().map(|c| c.key.clone()).collect();
                broken.extend(self.failed_checks(&name, &merged, &keys)?);
            }
            if let Some(set) = &read_set {
                broken.extend(self.stale_reasons(set, &base_state, &theirs_state, &wrote)?);
            }
            let refused = match (&policy, &declared) {
                (None, Some(s)) if !strayed.is_empty() => Some(scope::refused(b, s, &strayed)),
                _ if broken.is_empty() => None,
                (Some(p), _) => Some(agents::needs_review(b, &p.name, &broken)),
                (None, _) if attached_review => Some(agents::needs_review(b, ATTACHED_REVIEW, &broken)),
                (None, _) => Some(checks::broken(b, &broken)),
            };
            p.blocked = p.blocked.or(refused.map(|e| e.to_string()));
            // into main of a folder attached to Postgres: the rows it would write there, to check against Postgres
            // once the locks are let go (see `merge_preview`)
            let dry = match self.attached.as_deref().filter(|_| attach::writes_back(self, &name, tx)) {
                Some(att) => {
                    let mut written: Vec<Change> = Vec::with_capacity(apply.len());
                    for c in &apply {
                        written.push(Change {
                            key: c.key.clone(),
                            before: self.val(&theirs_state, &c.key)?,
                            after: self.val(&merged, &c.key)?,
                        });
                    }
                    let reads = read_set.as_ref().map(|set| (set.clone(), base_state.clone(), wrote.clone()));
                    Some(attach::Dry::new(self, att, b, &theirs_state, &ours_state, &written, reads))
                }
                None => None,
            };
            return Ok(Merged::Plan(Box::new((p, dry))));
        }
        if let Some(k) = schema_clash.first() {
            let t = k.strip_prefix("_sonos_tables/").unwrap_or(k);
            return Err(Error::Constraint {
                code: "40001",
                message: format!(
                    "both sides changed table {t} itself (its columns or constraints); merge one side, then redo the other's change on the result; nothing was merged"
                ),
            });
        }
        if !conflicts.is_empty() {
            let (base, ours) = (child.base(), child.snapshot());
            let mut schemas = [HashMap::new(), HashMap::new(), HashMap::new()];
            let mut show = |i: usize, s: &State, k: &str, v: Option<Val>| {
                v.map(|v| self.show(s, k, v, &mut schemas[i])).transpose()
            };
            let conflicts = conflicts
                .into_iter()
                .map(|c| {
                    Ok(Conflict {
                        base: show(0, &base, &c.key, c.base)?,
                        ours: show(1, &ours, &c.key, c.ours)?,
                        theirs: show(2, &next, &c.key, c.theirs)?,
                        key: c.key,
                    })
                })
                .collect::<Result<_>>()?;
            return Err(Error::Conflicts(conflicts));
        }
        // a person approving a merge that deletes more rows than its policy lets go unread gives the count: before
        // anything is written (to Postgres too, in an attached folder). An agent's merge is held by `broken` instead
        if !tx
            && !rebase
            && disk::actor().is_empty()
            && self.me.get().is_some()
            && !RECOVERING.get()
            && let Some(c) = self.deletes_to_confirm(b, &apply)?
            && o.confirm_deletes != Some(c.deletes)
        {
            return Err(agents::count_refused(b, c.deletes, &c.policy, c.over, o.confirm_deletes));
        }
        let n = apply.len();
        let keys: Vec<String> = apply.iter().map(|c| c.key.clone()).collect();
        if tx {
            // a transaction's COMMIT on main is a change made straight into it
            self.guard_main(&name, keys.iter().map(String::as_str))?;
        }
        let notes = if name == MAIN { outbox::notes(&apply) } else { vec![] };
        // the effects it lands in main, and who lets them through: kept once the merge is done (see `keep_landed`)
        if name == MAIN && !tx && !rebase && !notes.is_empty() {
            let info = child.info.read().unwrap();
            let review = info.meta.get(agents::REVIEW).and_then(|r| r["policy"].as_str());
            self.landing(b, &info.id, review, policy.as_ref().map(|p| p.name.as_str()), &apply);
        }
        // an agent's merge into main counts toward its policy's hourly limits once it's done
        let counted = (name == MAIN && policy.is_some()).then(|| agents::counts(&apply));
        for ch in apply {
            next.set(&ch.key, ch.after);
        }
        let before = View { core: self, born: parent.born, s: parent.snapshot(), branch: String::new(), reads: None };
        let after = View { core: self, born: parent.born, s: next.clone(), branch: String::new(), reads: None };
        let entries = match sql::reindex(&after, &before, &keys) {
            Ok(entries) => entries,
            Err(e) => {
                if let Some(r) = review(broken) {
                    return Ok(r);
                }
                return Err(Error::Constraint { code: e.code, message: format!("{}; nothing was merged", e.message) });
            }
        };
        for (k, v) in entries {
            next.set(&k, v);
        }
        // both sides may be valid alone yet break a SQL constraint together; a parent left as the
        // branch forked from it, all of whose rows merge, has no side of its own: the result is the
        // branch's, which its statements checked (a RELEASE SAVEPOINT, whose foreign keys put off
        // until COMMIT may not hold yet, as in Postgres)
        let unchanged = !stays && base_state.same(&theirs_state) && !child.rebased.load(Ordering::Relaxed);
        let merged = View { core: self, born: parent.born, s: next.clone(), branch: String::new(), reads: None };
        if let Err(e) = if unchanged { Ok(()) } else { sql::check_merge(&merged, &before, &keys) } {
            if let Some(r) = review(broken) {
                return Ok(r);
            }
            return Err(Error::Constraint { code: e.code, message: format!("{}; nothing was merged", e.message) });
        }
        // and the merge checks: queries that must find nothing on the merged result; what they
        // find joins the policy's broken rules
        let failed = if tx || rebase { vec![] } else { self.failed_checks(&name, &next, &keys)? };
        let stale = match &read_set {
            Some(set) => self.stale_reasons(set, &base_state, &theirs_state, &wrote)?,
            None => vec![],
        };
        broken.extend(failed.iter().chain(&stale).cloned());
        if let Some(r) = review(broken.clone()) {
            return Ok(r);
        }
        if !failed.is_empty() {
            return Err(checks::broken(b, &broken));
        }
        if !stale.is_empty() {
            return Err(stale::stale(b, if rebase { "rebasing" } else { "merging" }, &stale));
        }
        if rebase {
            // keeping its own rows over the parent's changes would make them look unchanged since at
            // its merge: an agent whose policy has a person look at overwrites can't rebase past one
            if overwrote > 0
                && let Some(p) = self.acting_policy()?.filter(|p| !p.overwrite)
            {
                return Err(Error::Constraint {
                    code: "42501",
                    message: format!(
                        "rebasing {b} would overwrite {overwrote} rows its parent changed since the fork, which merge policy {} leaves to a person (overwrite): settle them as theirs, or merge and let a person review it; nothing changed",
                        p.name
                    ),
                });
            }
            // the world as if forked now: its fork point the parent as it is, its own changes on
            // top as the merge would leave them. What it read stays as it was (unchanged, as just
            // checked), so a later merge checks those reads from the new fork point
            let taken = self
                .state_diff_iter(&base_state, &theirs_state, None)?
                .filter(|c| c.as_ref().map_or(true, |c| shown(&c.key)))
                .count();
            // both sides' writes before this, in the log before it: replay takes in what it took in
            self.flush_batch(&child);
            self.flush_batch(&parent);
            let rec = self.record(|| {
                Op::Rebase(
                    b,
                    how,
                    expected,
                    logged.iter().map(|(k, kind, v)| (k.as_str(), *kind, v.as_deref())).collect(),
                )
            });
            let mut snap = child.snap.write().unwrap(); // held through the append, as `write` does
            *snap = next;
            child.stamp.store(stamp(), Ordering::Relaxed);
            *child.base.write().unwrap() = theirs_state;
            child.version.fetch_add(1, Ordering::Relaxed);
            child.active.store(self.now(), Ordering::Relaxed);
            // its fork point moved: going back past now would put back rows as they sat on the old
            // one (see `restore`). Set with the rebase, under its locks, and again by its replay
            child.info.write().unwrap().meta.insert(REBASED.into(), serde_json::json!(self.now()));
            let pos = self.append(rec);
            return Ok(Merged::Done(taken, pos, vec![], None));
        }
        // rows it locked: refused if changed, or locked FOR UPDATE by a merge, since it forked
        let locked = match stays || rebase {
            true => Default::default(), // only a whole merge (a commit) brings its locks
            false => std::mem::take(&mut *lock(&child.locked)),
        };
        if !locked.is_empty() {
            let start = child.start.load(Ordering::SeqCst);
            let taken = self.row_locks.lock().unwrap();
            let taken = taken.get(&name);
            let wrote: std::collections::HashSet<&String> = keys.iter().collect();
            let mut lost = vec![];
            for (k, mode) in &locked {
                // as Postgres's lock modes conflict: FOR KEY SHARE only with FOR UPDATE, the
                // others with FOR UPDATE and FOR NO KEY UPDATE
                let relocked = taken
                    .and_then(|t| t.get(k))
                    .is_some_and(|&(update, no_key)| update > start || (*mode > RowLock::KEY_SHARE && no_key > start));
                // a row it wrote was checked above
                if relocked || (!wrote.contains(k) && self.val(&theirs_state, k)? != self.val(&base_state, k)?) {
                    lost.push(k.clone());
                }
            }
            if !lost.is_empty() {
                *lock(&child.locked) = locked; // unchanged, should the caller keep the world
                return Err(locks_lost(&lost));
            }
        }
        if stays && let Some(q) = self.quota(&name)? {
            q.check(&name, parent.changed.load(Ordering::Relaxed), n as u64)?;
        }
        // a folder attached to Postgres: everything here passed, so the merge is written there now, in one
        // transaction, before anything is published here (see `attach`); a failure there merges nothing
        if attach::writes_back(self, &name, tx)
            && let Some(att) = self.attached.as_deref()
        {
            let world_id = child.info.read().unwrap().id.clone();
            let mut written: Vec<Change> = Vec::with_capacity(keys.len());
            for k in &keys {
                written.push(Change {
                    key: k.clone(),
                    before: self.val(&theirs_state, k)?,
                    after: self.val(&next, k)?,
                });
            }
            // a world that checks its reads: what it read of the attached tables, checked against Postgres too
            let reads = read_set.as_ref().map(|set| attach::Reads { set, base: &base_state, wrote: &wrote });
            let written =
                attach::write_back(self, att, b, &world_id, &theirs_state, &ours_state, &written, reads.as_ref());
            match written {
                Ok(stale) if stale.is_empty() => {}
                out => {
                    if !locked.is_empty() {
                        *lock(&child.locked) = locked; // unchanged, should the caller keep the world
                    }
                    // read rows Postgres changed since the fork: held as any stale read (nothing was written there)
                    let why = out?;
                    if let Some(r) = review(why.clone()) {
                        return Ok(r);
                    }
                    return Err(stale::stale(b, "merging", &why));
                }
            }
        }
        if stays {
            self.flush_batch(&child); // the merge reads them
            self.flush_batch(&parent);
            let rec = self.record(|| Op::MergeSome {
                branch: b,
                into: into.unwrap_or(""),
                how,
                expected,
                tables: o.only_tables.iter().map(String::as_str).collect(),
                keys: o.only_keys.iter().map(String::as_str).collect(),
                rows: logged.iter().map(|(k, kind, v)| (k.as_str(), *kind, v.as_deref())).collect(),
            });
            let mut snap = parent.snap.write().unwrap(); // held through the append, as `write` does
            *snap = next;
            parent.stamp.store(stamp(), Ordering::Relaxed);
            let mut base = child.base.write().unwrap();
            if !settled.is_empty() {
                // what's left to merge changed: a merge at the version a person reviewed is refused
                child.version.fetch_add(1, Ordering::Relaxed);
            }
            for (k, v) in settled {
                base.set(&k, v);
            }
            let kept = child.version.load(Ordering::Relaxed); // under its writer lock: no write since
            parent.changed.fetch_add(n as u64, Ordering::Relaxed);
            let pos = self.append(rec);
            if let Some((rows, deletes, effects)) = counted {
                self.count_merge(&disk::actor(), rows, deletes, effects);
            }
            return Ok(Merged::Done(n, pos, notes, Some(kept)));
        }
        // rows settled here are logged as the values they got, so replay settles them the same
        let rec = self.record(|| match logged.is_empty() {
            true => Op::Merge(b.to_string(), how, expected),
            false => Op::MergeWith(b.to_string(), how, expected, logged),
        });
        self.flush_batch(&child); // the merge reads them
        self.flush_batch(&parent);
        let mut at = 0;
        // the parent's state is published, and the lock clock moved on with it, under its state
        // lock, which `retire` takes before the log and the map (a fork reads both at once)
        let pos = self.retire(b, rec, &parent, next, || {
            if !locked.is_empty() {
                at = self.lock_clock.fetch_add(1, Ordering::SeqCst) + 1;
            }
        })?;
        *gone = true;
        if !locked.is_empty() {
            self.keep_locks(&name, &parent, locked, at);
        }
        if let Some((rows, deletes, effects)) = counted {
            self.count_merge(&disk::actor(), rows, deletes, effects);
        }
        Ok(Merged::Done(n, pos, notes, None))
    }

    /// The merge of a transaction too big to merge row by row in memory (see `big_tx`), into `theirs`, its world as it
    /// is now. Where that world is still as the transaction forked from it, and the
    /// transaction was never brought up to it since (`rebased`), the result is the transaction's own state
    /// (`ours`): the world takes it as it is, one state, and none of its rows is held or read. Otherwise the rows where
    /// the two differ are read in key order, a part at a time: each must be one the world still has as the transaction
    /// last saw it (`base`), so that the transaction's state is the merge's result still (a READ COMMITTED transaction
    /// is brought up to its world before it commits: see `merge_with`), and the constraints and index entries those
    /// rows need on the result are checked and made as a merge's are (`sql::check_published`). None when a row isn't
    /// so: the merge then goes row by row, as any merge does, to refuse what both sides changed.
    fn merge_published(
        &self,
        base: &State,
        ours: &State,
        theirs: &State,
        born: u64,
        rebased: bool,
    ) -> Result<Option<State>> {
        // (a transaction brought up to its world (`rebased`) has the world as its fork point, and its rows still to
        // check on it: its statements before were checked on what the world had then)
        if base.same(theirs) && !rebased {
            return Ok(Some(ours.clone()));
        }
        let view = |s: &State| View { core: self, born, s: s.clone(), branch: String::new(), reads: None };
        let (before, merged) = (view(theirs), view(ours));
        let mut fixes: Vec<(String, Option<Val>)> = vec![];
        let mut check = |part: &mut Vec<String>| -> Result<()> {
            let entries = sql::check_published(&merged, &before, part).map_err(|e| Error::Constraint {
                code: e.code,
                message: format!("{}; nothing was merged", e.message),
            })?;
            for (k, v) in entries {
                if self.val(ours, &k)? != v {
                    fixes.push((k, v));
                }
            }
            part.clear();
            Ok(())
        };
        let (mut part, mut bytes, part_bytes) = (vec![], 0, sql::published_part());
        for c in self.state_diff_iter(theirs, ours, None)? {
            let c = c?;
            if self.val(base, &c.key)? != c.before {
                return Ok(None);
            }
            if sql::btree_entry(&c.key) {
                continue; // made from the rows, as a merge makes them (see `sql::reindex`)
            }
            bytes += c.key.len() + 64;
            part.push(c.key);
            if bytes >= part_bytes {
                check(&mut part)?;
                bytes = 0;
            }
        }
        check(&mut part)?;
        let mut next = ours.clone();
        for (k, v) in fixes {
            next.set(&k, v);
        }
        Ok(Some(next))
    }

    /// The NOTIFYs a merge that leaves its world at `next` (from `theirs`) sends there: those of the effects it lands
    /// (see `outbox`), read from their own rows alone.
    fn landed(&self, theirs: &State, next: &State) -> Result<Vec<(String, String)>> {
        let mut landed = vec![];
        for c in self.state_diff_iter(theirs, next, Some(outbox::OUTBOX))? {
            let c = c?;
            if !c.key.starts_with(outbox::OUTBOX) {
                break;
            }
            landed.push(c);
        }
        Ok(outbox::notes(&landed))
    }

    /// Whether merging world `b`, which changes `apply`, needs the person merging it to give the count of rows it
    /// deletes (see `ConfirmDeletes`), counted from `apply` itself.
    fn deletes_to_confirm(&self, b: &str, apply: &[Change]) -> Result<Option<ConfirmDeletes>> {
        let Some((policy, over)) = self.confirm_deletes(b)? else { return Ok(None) };
        let (_, deletes, _) = agents::counts(apply);
        Ok((deletes > over).then_some(ConfirmDeletes { deletes, over, policy }))
    }

    /// A dry run's plan (see `merge_preview`): every row the branch changed and what the merge
    /// would do with it, shown as JSON, and whether constraints would block it; and the parent
    /// as the merge would leave it.
    #[allow(clippy::too_many_arguments)]
    fn plan(
        &self,
        apply: &[Change],
        conflicts: &[Conflict],
        outcomes: &HashMap<String, (&'static str, Option<Val>)>,
        schema_clash: &[String],
        states: [&State; 3],
        born: u64,
        mut next: State,
    ) -> Result<(MergePlan, State)> {
        let mut schemas = [HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new()];
        let mut show =
            |i: usize, s: &State, k: &str, v: Option<Val>| v.map(|v| self.show(s, k, v, &mut schemas[i])).transpose();
        let mut rows = vec![];
        let keys: Vec<String> = apply.iter().map(|c| c.key.clone()).collect();
        let before = View { core: self, born, s: states[2].clone(), branch: String::new(), reads: None };
        for ch in apply {
            next.set(&ch.key, ch.after.clone());
        }
        let blocked = match sql::reindex(&View { s: next.clone(), ..before.clone() }, &before, &keys) {
            Err(e) => Some(e.message),
            Ok(entries) => {
                for (k, v) in entries {
                    next.set(&k, v);
                }
                sql::check_merge(&View { s: next.clone(), ..before.clone() }, &before, &keys).err().map(|e| e.message)
            }
        };
        for ch in apply.iter().filter(|c| !sql::internal(&c.key) && !sql::btree_entry(&c.key)) {
            let outcome = outcomes.get(&ch.key).map_or("apply", |o| o.0);
            let theirs = self.val(states[2], &ch.key)?;
            rows.push(Planned {
                key: ch.key.clone(),
                outcome,
                detail: if outcome == "apply" { String::new() } else { "both sides changed this row".into() },
                base: show(0, states[0], &ch.key, ch.before.clone())?,
                ours: show(1, states[1], &ch.key, self.val(states[1], &ch.key)?)?,
                theirs: show(2, states[2], &ch.key, theirs)?,
                result: show(3, &next, &ch.key, ch.after.clone())?,
            });
        }
        // settled rows that end as the parent already has them (kept theirs, picked theirs)
        for (k, (outcome, v)) in outcomes.iter().filter(|(k, _)| !apply.iter().any(|c| &c.key == *k)) {
            rows.push(Planned {
                key: k.clone(),
                outcome,
                detail: "both sides changed this row".into(),
                base: show(0, states[0], k, self.val(states[0], k)?)?,
                ours: show(1, states[1], k, self.val(states[1], k)?)?,
                theirs: show(2, states[2], k, v.clone())?,
                result: show(3, &next, k, v.clone())?,
            });
        }
        for c in conflicts {
            let (base, ours, theirs) = (
                show(0, states[0], &c.key, c.base.clone())?,
                show(1, states[1], &c.key, c.ours.clone())?,
                show(2, states[2], &c.key, c.theirs.clone())?,
            );
            let detail = if schema_clash.contains(&c.key) {
                "both sides changed this table itself: merge one side, then redo the other's change".into()
            } else {
                explain(base.as_deref(), ours.as_deref(), theirs.as_deref())
            };
            rows.push(Planned { key: c.key.clone(), outcome: "conflict", detail, base, ours, theirs, result: None });
        }
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        Ok((
            MergePlan {
                conflicts: conflicts.len(),
                rows,
                blocked,
                version: 0,
                target: 0,
                live: None,
                confirm_deletes: None,
            },
            next,
        ))
    }

    /// Drops world `b` and what it changed; refused while it has forks of its own.
    pub fn discard(&self, b: &str) -> Result<()> {
        self.discard_many(&[b]).pop().expect("an outcome per world")
    }

    /// Discards each world named, as `discard` does, all under one hold of the branch map and the
    /// log: dropping N worlds costs O(N) however many are left. The deepest go first (then the later
    /// fork), so forks go before their parents whatever the order given; a world with forks not in
    /// the list stays (`HasChildren`).
    /// Returns each world's outcome, in the order given.
    pub fn discard_many<S: AsRef<str>>(&self, names: &[S]) -> Vec<Result<()>> {
        let mut out = Vec::with_capacity(names.len());
        let mut todo: Vec<(usize, Arc<Branch>)> = vec![];
        let mut named = std::collections::HashSet::new();
        for (i, b) in names.iter().map(AsRef::as_ref).enumerate() {
            out.push(match self.branch(b) {
                _ if b == MAIN => Err(Error::IsMain),
                Ok(_) if !named.insert(b) => Err(Error::NoBranch(b.into())), // gone by the second time
                Ok(br) => {
                    todo.push((i, br));
                    Ok(())
                }
                Err(e) => Err(e),
            });
        }
        todo.sort_by_key(|(_, br)| std::cmp::Reverse(br.born)); // the lock order: the later fork first
        let mut recs: Vec<_> = todo.iter().map(|(i, _)| self.record(|| Op::Discard(names[*i].as_ref()))).collect();
        let (mut borns, mut worlds) = (vec![], vec![]);
        let mut effects = vec![]; // what each world held to send, recorded once it's gone (see `discarding`)
        let pos = {
            let _g = self.gate();
            let mut gone: Vec<_> = todo.iter().map(|(_, br)| lock(&br.writer)).collect();
            for (_, br) in &todo {
                self.flush_batch(br); // logged before it goes, for history (replay needs none of it)
            }
            let mut log = self.log.as_ref().map(|l| (l, l.lock())); // the log, then the map, as forks take them
            let mut m = self.branches.write().unwrap();
            let mut pos = 0;
            // deepest first, as DROP WORLD ... CASCADE always went (a fork is deeper than its parent)
            let mut order: Vec<usize> = (0..todo.len()).collect();
            order.sort_by_key(|&j| std::cmp::Reverse(m.get(names[todo[j].0].as_ref()).map_or(0, |e| e.depth)));
            for j in order {
                let ((i, br), gone, rec) = (&todo[j], &mut gone[j], recs[j].take());
                let b = names[*i].as_ref();
                if **gone {
                    out[*i] = Err(Error::NoBranch(b.into())); // merged or discarded meanwhile
                    continue;
                }
                if m[b].kids > 0 {
                    out[*i] = Err(Error::HasChildren(b.into()));
                    continue;
                }
                m.take(b);
                self.row_locks.lock().unwrap().remove(b);
                **gone = true;
                if let (Some((l, g)), Some(rec)) = (&mut log, rec) {
                    pos = l.append(g, &rec); // still under the map lock: orders us against forks
                }
                borns.push(br.born);
                if !b.starts_with("_tx_") {
                    if self.me.get().is_some() && !RECOVERING.get() {
                        let held = self.effects_of(br).unwrap_or_default();
                        if !held.is_empty() {
                            let info = br.info.read().unwrap();
                            effects.push((b, info.id.clone(), info.meta.clone(), held));
                        }
                    }
                    worlds.push(b);
                    self.dropped.0.fetch_add(1, Ordering::Relaxed);
                    self.dropped.1.fetch_add(br.changed.load(Ordering::Relaxed), Ordering::Relaxed);
                }
            }
            pos
        };
        for (b, id, meta, held) in effects {
            self.discarding(b, &id, &meta, held);
        }
        self.forget(&borns);
        sql::forget_world_stats(self.uid, &worlds); // a world made again by that name starts with none
        if self.reclaim_due()
            && let Some(k) = &self.kick
        {
            let _ = k.try_send(()); // the background worker checkpoints and cleans up
        }
        self.after(pos, false);
        out
    }

    /// Drops cached search results of retired branches, so nothing keeps their states alive.
    fn forget(&self, borns: &[u64]) {
        if borns.is_empty() {
            return;
        }
        let gone: std::collections::HashSet<u64> = borns.iter().copied().collect();
        self.answers.lock().unwrap().retain(|(b, _), _| !gone.contains(b));
        self.deltas.lock().unwrap().retain(|(_, b), _| !gone.contains(b));
    }

    /// Whether enough was discarded since the last cleanup to run one (see `RECLAIM_WORLDS`).
    fn reclaim_due(&self) -> bool {
        self.dropped.0.load(Ordering::Relaxed) >= RECLAIM_WORLDS
            || self.dropped.1.load(Ordering::Relaxed) >= RECLAIM_ROWS
    }

    /// Worlds forked from `b`, from those, and so on: deepest first, so each can be discarded
    /// before its parent.
    pub fn descendants(&self, b: &str) -> Vec<String> {
        let m = self.branches.read().unwrap();
        let mut kids: HashMap<&str, Vec<&str>> = HashMap::new();
        for (n, e) in m.iter() {
            if let Some(p) = &e.parent {
                kids.entry(p.as_str()).or_default().push(n);
            }
        }
        let mut out: Vec<String> = vec![];
        let mut next: Vec<&str> = kids.get(b).cloned().unwrap_or_default();
        while let Some(n) = next.pop() {
            next.extend(kids.get(n).into_iter().flatten());
            out.push(n.into());
        }
        out.sort_by_key(|n| std::cmp::Reverse(m[n.as_str()].depth));
        out
    }

    /// Discards world `b` and every world forked from it, forks first, each logged on its own
    /// (DROP WORLD ... CASCADE), in one `discard_many`. Returns how many worlds went.
    pub fn discard_tree(&self, b: &str) -> Result<usize> {
        if b == MAIN {
            return Err(Error::IsMain);
        }
        let mut n = 0;
        loop {
            let mut tree = self.descendants(b);
            tree.push(b.into());
            let mut out = self.discard_many(&tree);
            let top = out.pop().expect("an outcome per world");
            for r in out {
                match r {
                    Ok(()) => n += 1,
                    Err(Error::NoBranch(_) | Error::HasChildren(_)) => {} // gone, or forked meanwhile: next round
                    Err(e) => return Err(e),
                }
            }
            match top {
                Ok(()) => return Ok(n + 1),
                Err(Error::HasChildren(_)) => continue, // forked meanwhile
                Err(e) => return Err(e),
            }
        }
    }

    /// The world whose rows branch `b` writes: a transaction's (or its savepoint's) is the world it forked from,
    /// anything else is itself.
    pub(crate) fn intent_world(&self, b: &str) -> String {
        if !b.starts_with("_tx_") {
            return b.to_string();
        }
        let top = b.split("_sp").next().unwrap_or(b);
        let m = self.branches.read().unwrap();
        m.get(top).and_then(|e| e.parent.clone()).unwrap_or_else(|| b.to_string())
    }

    /// Takes row intents for `owner` (a session) in `world`: `keys` with their lock modes, held until `release_intents`.
    /// What Postgres's row locks are to its writers: an UPDATE, DELETE, INSERT or SELECT ... FOR UPDATE takes them
    /// for the rows it writes or locks, and one that meets a row another session holds waits for that session to end
    /// its transaction (the caller waits: see `wait_intents`; nothing here blocks). `tx` is the open transaction
    /// they are taken for, if any: a row that changed in its world since the transaction was last brought up to date
    /// (see `rebase_tx`) is `Claim::Stale`, which READ COMMITTED answers with a fresh look at the row, and
    /// REPEATABLE READ with 40001. Nothing is taken unless every key is free and fresh.
    /// `level` is the number of savepoints open when they are taken: a ROLLBACK TO lets go of those taken since
    /// (see `release_intents_above`).
    ///
    /// `soft` keys (rows a statement adds, which nobody else can hold unless adding them too) are waited for like
    /// the others, but not held once the session holds as many rows as it may: they are no reason to hold a table.
    ///
    /// A session that holds more than `ROW_INTENTS` rows holds the tables they are in instead (a statement that big
    /// keeps its memory flat, as it does elsewhere): other sessions then wait for any row of those tables, which is
    /// more than Postgres makes them wait for, never less.
    pub(crate) fn claim(
        &self,
        owner: u64,
        world: &str,
        tx: Option<&str>,
        keys: &[(&str, u8)],
        level: usize,
        soft: bool,
    ) -> Result<Claim> {
        let mut g = lock(&self.intents);
        let t = &mut *g;
        // the holders, each with how many transactions of its session had ended as it was seen holding
        let mut blockers: Vec<(u64, u64)> = vec![];
        let ended = |t: &Intents, o: &u64| t.ended.get(o).copied().unwrap_or(0);
        let mut block = |t: &Intents, o: u64| {
            if !blockers.iter().any(|(b, _)| *b == o) {
                blockers.push((o, ended(t, &o)));
            }
        };
        let mut tables_seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let w = world.to_string();
        // the table of the first row found held (what pg_stat_activity's waits name)
        let mut at: Option<&str> = None;
        for (k, mode) in keys {
            let table = table_of(k);
            if let Some(hs) = t.held.get(&(w.clone(), k.to_string())) {
                for (o, m) in hs {
                    if *o != owner && RowLock::conflicts(*mode, *m) {
                        block(t, *o);
                        at.get_or_insert(table);
                    }
                }
            }
            if tables_seen.insert(table) {
                // a session that holds the whole table (see below) holds this row
                if let Some(hs) = t.tables.get(&(w.clone(), table.to_string())) {
                    for (o, m) in hs {
                        if *o != owner && RowLock::conflicts(*mode, *m) {
                            block(t, *o);
                            at.get_or_insert(table);
                        }
                    }
                }
            }
        }
        // too many for rows: the tables they are in
        let mine = t.by_owner.get(&owner).map_or(0, |o| o.rows.len());
        let whole = mine + keys.len() > ROW_INTENTS;
        if whole && !soft {
            for table in &tables_seen {
                if let Some(counts) = t.rows_in.get(&(w.clone(), table.to_string())) {
                    for (o, n) in counts {
                        if *o != owner && *n > 0 {
                            block(t, *o);
                            at.get_or_insert(*table);
                        }
                    }
                }
            }
        }
        if !blockers.is_empty() {
            if let Some(table) = at {
                t.blocked_on.insert(owner, (w.clone(), table.to_string()));
            }
            return Ok(Claim::Blocked(blockers));
        }
        if let Some(top) = tx {
            let br = self.branch(top)?;
            let parent = {
                let m = self.branches.read().unwrap();
                m.get(top).and_then(|e| e.parent.clone()).and_then(|p| m.get(&p).map(|e| e.br.clone()))
            };
            if let Some(parent) = parent {
                let now = parent.snapshot();
                let seen = lock(&br.synced).clone();
                if !now.same(&seen) {
                    for (k, _) in keys {
                        if self.val(&now, k)? != self.val(&seen, k)? {
                            return Ok(Claim::Stale(k.to_string()));
                        }
                    }
                }
            }
        }
        if whole && soft {
            return Ok(Claim::Got); // (free and fresh, and not held)
        }
        if whole {
            let strongest = keys.iter().map(|(_, m)| *m).max().unwrap_or(RowLock::NO_KEY_UPDATE);
            for table in tables_seen {
                let held = t.tables.entry((w.clone(), table.to_string())).or_default();
                match held.iter_mut().find(|(o, _)| *o == owner) {
                    Some((_, m)) => *m = (*m).max(strongest),
                    None => {
                        held.push((owner, strongest));
                        t.by_owner.entry(owner).or_default().tables.push((w.clone(), table.to_string(), level));
                    }
                }
                // its rows of the table are held by the table now
                if let Some(o) = t.by_owner.get_mut(&owner) {
                    let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut o.rows)
                        .into_iter()
                        .partition(|(rw, k, _)| *rw == w && table_of(k) == table);
                    o.rows = kept;
                    for (rw, k, _) in gone {
                        Intents::forget_row(&mut t.held, &mut t.rows_in, owner, &rw, &k);
                    }
                }
            }
            return Ok(Claim::Got);
        }
        for (k, mode) in keys {
            let held = t.held.entry((w.clone(), k.to_string())).or_default();
            match held.iter_mut().find(|(o, _)| *o == owner) {
                Some((_, m)) => *m = (*m).max(*mode),
                None => {
                    held.push((owner, *mode));
                    t.by_owner.entry(owner).or_default().rows.push((w.clone(), k.to_string(), level));
                    *t.rows_in.entry((w.clone(), table_of(k).to_string())).or_default().entry(owner).or_default() += 1;
                }
            }
        }
        Ok(Claim::Got)
    }

    /// The rows (and the whole tables) other sessions hold intents on in `world`, by the strongest mode each holds:
    /// what SELECT ... FOR UPDATE NOWAIT and SKIP LOCKED look at.
    pub(crate) fn intents_of_others(&self, owner: u64, world: &str) -> Held {
        let t = lock(&self.intents);
        let mut out = Held::default();
        for ((w, k), hs) in t.held.iter().filter(|((w, _), _)| w == world) {
            let _ = w;
            for (o, m) in hs.iter().filter(|(o, _)| *o != owner) {
                let _ = o;
                let e = out.rows.entry(k.clone()).or_insert(*m);
                *e = (*e).max(*m);
            }
        }
        for ((_, k), hs) in t.tables.iter().filter(|((w, _), _)| w == world) {
            for (_, m) in hs.iter().filter(|(o, _)| *o != owner) {
                let e = out.tables.entry(k.clone()).or_insert(*m);
                *e = (*e).max(*m);
            }
        }
        out
    }

    /// Lets go of everything `owner` holds: its transaction ended (or its statement, outside one).
    pub(crate) fn release_intents(&self, owner: u64) {
        let mut g = lock(&self.intents);
        let t = &mut *g;
        let Some(owned) = t.by_owner.remove(&owner) else { return };
        for (w, k, _) in owned.rows {
            Intents::forget_row(&mut t.held, &mut t.rows_in, owner, &w, &k);
        }
        for (w, table, _) in owned.tables {
            Intents::forget_table(&mut t.tables, owner, &w, &table);
        }
        *t.ended.entry(owner).or_default() += 1;
        if t.ended.len() > 10_000 {
            let live: std::collections::HashSet<u64> = t.waiting.keys().copied().collect();
            t.ended.retain(|o, _| live.contains(o));
        }
        drop(g);
        let (m, cv) = &self.intent_wake;
        *m.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        cv.notify_all();
    }

    /// ROLLBACK TO: lets go of what `owner` took with more than `level` savepoints open, as Postgres lets go of the
    /// row locks of a subtransaction it rolls back. The rows it had before stay, whatever it did to them since.
    pub(crate) fn release_intents_above(&self, owner: u64, level: usize) {
        let mut g = lock(&self.intents);
        let t = &mut *g;
        let Some(owned) = t.by_owner.get_mut(&owner) else { return };
        let (gone_rows, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut owned.rows).into_iter().partition(|r| r.2 > level);
        owned.rows = kept;
        let (gone_tables, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut owned.tables).into_iter().partition(|r| r.2 > level);
        owned.tables = kept;
        if gone_rows.is_empty() && gone_tables.is_empty() {
            return;
        }
        for (w, k, _) in gone_rows {
            Intents::forget_row(&mut t.held, &mut t.rows_in, owner, &w, &k);
        }
        for (w, table, _) in gone_tables {
            Intents::forget_table(&mut t.tables, owner, &w, &table);
        }
        if t.by_owner.get(&owner).is_some_and(|o| o.rows.is_empty() && o.tables.is_empty()) {
            t.by_owner.remove(&owner);
        }
        // (a waiter for one of them finds it free: it is woken as for an end, and asks again)
        *t.ended.entry(owner).or_default() += 1;
        drop(g);
        let (m, cv) = &self.intent_wake;
        *m.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        cv.notify_all();
    }

    /// RELEASE SAVEPOINT: what `owner` took with more than `level` savepoints open is held at `level` from now on, as
    /// a released subtransaction's row locks pass to its parent: a later ROLLBACK TO a savepoint made after the
    /// release, or an error past one, leaves them held.
    pub(crate) fn keep_intents_at(&self, owner: u64, level: usize) {
        let mut t = lock(&self.intents);
        if let Some(o) = t.by_owner.get_mut(&owner) {
            for r in o.rows.iter_mut().filter(|r| r.2 > level) {
                r.2 = level;
            }
            for r in o.tables.iter_mut().filter(|r| r.2 > level) {
                r.2 = level;
            }
        }
    }

    /// Waits until one of `holders` (sessions) lets go of its intents, as a statement that met a row they hold
    /// waits for their transactions to end. `stop` is asked between waits, and ends the wait with its error
    /// (a cancel, a timeout); a wait that would close a cycle of sessions waiting for each other fails with 40P01,
    /// as Postgres's deadlock check does. `others` are the waits of other kinds (advisory locks, sequences).
    pub(crate) fn wait_intents(
        &self,
        owner: u64,
        seen: &[(u64, u64)],
        others: &dyn Fn() -> HashMap<u64, Vec<u64>>,
        stop: &mut dyn FnMut() -> std::result::Result<(), Error>,
    ) -> std::result::Result<(), Error> {
        let holders: Vec<u64> = seen.iter().map(|(h, _)| *h).collect();
        lock(&self.intents).waiting.insert(owner, seen.to_vec());
        let done = |this: &Core| {
            let mut t = lock(&this.intents);
            t.waiting.remove(&owner);
            t.blocked_on.remove(&owner);
        };
        loop {
            // a cycle: a holder waits (through others) for something this session holds
            let cycle = {
                // (whether a holder is through and the graph are read under one lock: read apart, a holder could end
                // its transaction between them, begin another and wait for a row this session holds, and the
                // check would follow it as if it still held the row waited for: a cycle that never was)
                let t = lock(&self.intents);
                let finished =
                    seen.iter().any(|(h, n)| t.ended.get(h).copied().unwrap_or(0) != *n || !t.by_owner.contains_key(h));
                if finished {
                    drop(t);
                    done(self);
                    return Ok(());
                }
                // (an edge counts while its holder still holds what it was seen holding: a session that ended its
                // transaction and began another is not the one waited for)
                let mut graph: HashMap<u64, Vec<u64>> = t
                    .waiting
                    .iter()
                    .map(|(w, hs)| {
                        let live = hs
                            .iter()
                            .filter(|(h, n)| t.ended.get(h).copied().unwrap_or(0) == *n && t.by_owner.contains_key(h));
                        (*w, live.map(|(h, _)| *h).collect())
                    })
                    .collect();
                drop(t);
                for (k, v) in others() {
                    graph.entry(k).or_default().extend(v);
                }
                let mut at: Vec<u64> = holders.to_vec();
                let mut seen_o = std::collections::HashSet::new();
                let mut found = false;
                while let Some(o) = at.pop() {
                    if o == owner {
                        found = true;
                        break;
                    }
                    if seen_o.insert(o) {
                        at.extend(graph.get(&o).into_iter().flatten().copied());
                    }
                }
                found
            };
            if cycle {
                done(self);
                return Err(Error::Constraint { code: "40P01", message: "deadlock detected".into() });
            }
            if let Err(e) = stop() {
                done(self);
                return Err(e);
            }
            let (m, cv) = &self.intent_wake;
            let g = m.lock().unwrap_or_else(|e| e.into_inner());
            let _ = cv.wait_timeout(g, std::time::Duration::from_millis(10));
        }
    }

    /// The sessions waiting for a row another transaction holds (see `claim`): each with the sessions it waits for
    /// that still hold what they were seen holding, and the (world, table) of the row. What pg_blocking_pids() and
    /// pg_stat_activity's wait_event show of row locks.
    pub(crate) fn row_waits(&self) -> Vec<RowWait> {
        let t = lock(&self.intents);
        t.waiting
            .iter()
            .map(|(w, hs)| {
                let live = hs
                    .iter()
                    .filter(|(h, n)| t.ended.get(h).copied().unwrap_or(0) == *n && t.by_owner.contains_key(h))
                    .map(|(h, _)| *h);
                (*w, live.collect(), t.blocked_on.get(w).cloned())
            })
            .collect()
    }

    /// READ COMMITTED: brings transaction `top` (and the savepoints open above it) up to what its world has committed
    /// since it last was: the rows others committed since appear in it, with its own changes on top, as if it read
    /// the world as it is now. Each statement of a READ COMMITTED transaction starts so. The transaction's own
    /// changes stay, so a row both changed is refused (see `Rebased::Conflict`): the row intents (see `claim`) keep
    /// others off rows a transaction wrote, so that happens only to writers that take none (a merged world, a write
    /// through the API, a table's definition both altered) and to a constraint's rows of two inserts of one value.
    ///
    /// Logged as `Op::RebaseTx`: replaying it does the same to the same states.
    pub(crate) fn rebase_tx(&self, top: &str) -> Result<Rebased> {
        // (the common case first, with no locks: its world hasn't moved since)
        {
            // (the map's lock isn't held while a state's is: forks and merges take them the other way round)
            let (mine, parent) = {
                let m = self.branches.read().unwrap();
                let e = m.get(top).ok_or_else(|| Error::NoBranch(top.into()))?;
                let p = e.parent.as_ref().ok_or(Error::IsMain)?;
                let parent = m.get(p).ok_or_else(|| Error::NoBranch(p.clone()))?;
                (e.br.clone(), parent.br.clone())
            };
            let seen = lock(&mine.synced).clone();
            if seen.same(&parent.snap.read().unwrap()) {
                return Ok(Rebased::Same);
            }
        }
        let (chain, parent): (Vec<(String, Arc<Branch>)>, Arc<Branch>) = {
            let m = self.branches.read().unwrap();
            let e = m.get(top).ok_or_else(|| Error::NoBranch(top.into()))?;
            let p = e.parent.clone().ok_or(Error::IsMain)?;
            let parent = m.get(&p).map(|pe| pe.br.clone()).ok_or_else(|| Error::NoBranch(p.clone()))?;
            let mut chain = vec![(top.to_string(), e.br.clone())];
            let mut more: Vec<(&String, &Entry)> = m
                .iter()
                .filter(|(n, _)| n.starts_with(top) && n.as_str() != top && n[top.len()..].starts_with("_sp"))
                .collect();
            more.sort_by_key(|(_, e)| e.depth);
            chain.extend(more.into_iter().map(|(n, e)| (n.clone(), e.br.clone())));
            (chain, parent)
        };
        let rec = self.record(|| Op::RebaseTx(top));
        let pos = {
            let _g = self.gate();
            // the later forks' writer locks first, as merges take them
            let mut order: Vec<&Arc<Branch>> = chain.iter().map(|(_, b)| b).collect();
            order.sort_by_key(|b| std::cmp::Reverse(b.born));
            let guards: Vec<_> = order.iter().map(|b| lock(&b.writer)).collect();
            if guards.iter().any(|g| **g) {
                return Err(Error::NoBranch(top.into()));
            }
            for b in order.iter() {
                self.flush_batch(b); // logged before the rebase, which replay does on them
            }
            self.flush_batch(&parent);
            let now = parent.snap.read().unwrap(); // held through the append: orders us against writes to it
            let top_br = &chain[0].1;
            let seen = lock(&top_br.synced).clone();
            if seen.same(&now) {
                return Ok(Rebased::Same);
            }
            // the world's changes since are brought in under the transaction's own, unless they are more than the
            // transaction changed itself (other sessions' commits, an index built, a big load): then its own changes
            // are put on top of the world as it is now instead, which costs what the transaction changed, not what the
            // world did (each open transaction would otherwise copy all of that into its own state). A checkpoint
            // since, which folded the world's changes into a tree of their own, is one of those: telling which rows
            // of tree pages the two don't share changed reads all of them. Either way gives the same rows
            // (the world's own count says when it is more, with no diff to read: other sessions' commits, mostly)
            // A transaction too big to hold in memory (see `big_tx`) never is: its own changes, in pages of their own,
            // would all be read and laid on the world's overlay; the world's changes come in under them instead
            let own: u64 = chain.iter().map(|(_, b)| b.changed.load(Ordering::Relaxed)).sum();
            let big = chain.iter().any(|(_, b)| big_tx(&b.base(), &b.snapshot()));
            let most = if big { usize::MAX } else { (own as usize).min(REBASE_ROWS) };
            let rows = parent.changed.load(Ordering::Relaxed);
            let since = rows.saturating_sub(top_br.synced_rows.load(Ordering::Relaxed));
            let mut changes = vec![];
            let mut many = !big && (seen.tree != now.tree || since as usize > most);
            if !many {
                for c in self.state_diff_iter(&seen, &now, None)? {
                    changes.push(c?);
                    if changes.len() > most {
                        many = true;
                        break;
                    }
                }
            }
            if many {
                drop(changes);
                if let Some(conflict) = self.rebase_own(&chain, &seen, &now)? {
                    return Ok(conflict);
                }
            } else {
                // a row it changed that the world changed too can't be both (see `Rebased::Conflict`); a table both
                // altered is such a row (its definition's), as at COMMIT. A sequence's row is the exception: one the
                // transaction altered stays as it altered it, whoever altered it meanwhile (its fork point takes the
                // world's, so the commit brings it over theirs, as it always has)
                // (each level's own value of those, by key)
                let mut mine: Vec<HashMap<&str, Option<Val>>> = vec![HashMap::new(); chain.len()];
                for (i, (_, br)) in chain.iter().enumerate() {
                    let (base, snap) = (br.base(), br.snapshot());
                    for c in &changes {
                        let (was, is) = (self.val(&base, &c.key)?, self.val(&snap, &c.key)?);
                        if is == was {
                            continue;
                        }
                        if c.key.starts_with("_sonos_seq/") {
                            mine[i].insert(c.key.as_str(), is);
                            continue;
                        }
                        return Ok(Rebased::Conflict(c.key.clone(), was.is_none() && c.before.is_none()));
                    }
                }
                let own: std::collections::HashSet<&str> = mine.iter().flat_map(|m| m.keys().copied()).collect();
                // what the level above ends with, for the rows a level changed: the next one forks from it
                let mut above: HashMap<&str, Option<Val>> = HashMap::new();
                for (i, (_, br)) in chain.iter().enumerate() {
                    let mut snap = br.snap.write().unwrap();
                    let mut base = br.base.write().unwrap();
                    for c in &changes {
                        let k = c.key.as_str();
                        if !own.contains(k) {
                            snap.set(k, c.after.clone());
                            base.set(k, c.after.clone());
                            continue;
                        }
                        let fork = above.get(k).cloned().unwrap_or_else(|| c.after.clone());
                        base.set(k, fork.clone());
                        match mine[i].get(k) {
                            Some(v) => {
                                above.insert(k, v.clone());
                            }
                            None => {
                                snap.set(k, fork.clone());
                                above.insert(k, fork);
                            }
                        }
                    }
                    br.active.store(self.now(), Ordering::Relaxed);
                }
            }
            *lock(&top_br.synced) = now.clone();
            top_br.synced_rows.store(rows, Ordering::Relaxed);
            for (_, br) in &chain {
                br.rebased.store(true, Ordering::Relaxed);
            }
            // the row locks merged since it began are not its to lose: it is as if it began now
            top_br.start.store(self.lock_clock.load(Ordering::SeqCst), Ordering::SeqCst);
            // what its world was given before `now` is in the log before this (as `fork_info` has it): replay
            // finds the world as it is here
            let mut batch = lock(&parent.batch);
            let mut g = self.log.as_ref().map(|l| l.lock());
            self.flush_batch_in(&mut g, &mut batch);
            self.append_in(&mut g, rec)
        };
        self.after(pos, false);
        Ok(Rebased::Moved)
    }

    /// `rebase_tx` when its world committed many rows since `seen` (it is at `now`): each level of the transaction
    /// (`chain`, the transaction then its savepoints) is made its own changes on top of the level under it as it is
    /// now, the world's for the transaction, which is what bringing the world's changes in under them gives, row for
    /// row (and what replay gives, whichever way it goes). Some(Conflict) if a row it changed changed there too,
    /// with nothing changed. Call with what `rebase_tx` holds.
    fn rebase_own(&self, chain: &[(String, Arc<Branch>)], seen: &State, now: &State) -> Result<Option<Rebased>> {
        let mut owns: Vec<Vec<Change>> = Vec::with_capacity(chain.len());
        for (_, br) in chain {
            let own = self.state_diff(&br.base(), &br.snapshot())?;
            for c in own.iter().filter(|c| !c.key.starts_with("_sonos_seq/")) {
                let before = self.val(seen, &c.key)?;
                if before != self.val(now, &c.key)? {
                    return Ok(Some(Rebased::Conflict(c.key.clone(), c.before.is_none() && before.is_none())));
                }
            }
            owns.push(own);
        }
        let mut under = now.clone();
        for ((_, br), own) in chain.iter().zip(owns) {
            let mut snap = br.snap.write().unwrap();
            let mut base = br.base.write().unwrap();
            let mut next = under.clone();
            for c in own {
                next.set(&c.key, c.after);
            }
            *base = under;
            *snap = next.clone();
            under = next;
            br.active.store(self.now(), Ordering::Relaxed);
        }
        Ok(None)
    }

    /// After a merge of `locked` into `name` at `at`: a savepoint's locks join
    /// its transaction's; a transaction's commit leaves its FOR UPDATE and FOR NO KEY UPDATE
    /// locks to hold against later commits into the world that began before it. Like Postgres's,
    /// the locks themselves end with the transaction. Locks no open transaction forked before
    /// are forgotten.
    fn keep_locks(&self, name: &str, parent: &Branch, locked: std::collections::BTreeMap<String, u8>, at: u64) {
        if name.starts_with("_tx_") {
            // no other transaction commits into a transaction's world
            let mut kept = lock(&parent.locked);
            for (k, mode) in locked {
                let m = kept.entry(k).or_insert(mode);
                *m = (*m).max(mode);
            }
            return;
        }
        let mut all = self.row_locks.lock().unwrap();
        let taken = all.entry(name.to_string()).or_default();
        for (k, mode) in locked {
            let e = taken.entry(k).or_default();
            match mode {
                RowLock::UPDATE => e.0 = at,
                RowLock::NO_KEY_UPDATE => e.1 = at,
                _ => {} // shared: they hold against no later lock
            }
        }
        if taken.len() > 10_000 {
            // ponytail: a walk of every world, once per 10k locks; keep the oldest start if it shows.
            // Only transactions lock rows (see `sql`), so only theirs count
            let m = self.branches.read().unwrap();
            let oldest =
                m.iter().filter(|(n, _)| n.starts_with("_tx_")).map(|(_, e)| e.br.start.load(Ordering::SeqCst)).min();
            drop(m);
            taken.retain(|_, (u, n)| oldest.is_some_and(|o| *u.max(n) > o));
        }
    }

    /// Atomically (vs fork): refuse if `b` has live forks, else make `next` its parent's state,
    /// call `published` with that state held, drop `b` and log it. Takes the parent's state, the
    /// log, then the map, as forks do.
    fn retire(
        &self,
        b: &str,
        rec: Option<Vec<u8>>,
        parent: &Branch,
        next: State,
        published: impl FnOnce(),
    ) -> Result<u64> {
        let (pos, born) = {
            let mut snap = parent.snap.write().unwrap();
            let mut g = self.log.as_ref().map(|l| l.lock());
            let mut m = self.branches.write().unwrap();
            if m.get(b).is_some_and(|e| e.kids > 0) {
                return Err(Error::HasChildren(b.into()));
            }
            *snap = next;
            parent.stamp.store(stamp(), Ordering::Relaxed);
            published(); // moved on with the state published: a fork reads both at once (see `fork_info`)
            let born = m.take(b).map(|e| e.br.born);
            self.row_locks.lock().unwrap().remove(b); // what was locked in it goes with it
            (self.append_in(&mut g, rec), born) // still under the map lock: orders us against forks and other retires
        };
        self.forget(&Vec::from_iter(born));
        if !b.starts_with("_tx_") {
            sql::forget_world_stats(self.uid, &[b]); // a world made again by that name starts with none
        }
        Ok(pos)
    }
}

/// A merge undone (see `Core::undo_merge`).
#[derive(Debug, Clone, PartialEq)]
pub struct Undone {
    pub parent: String, // the world the merge went into, now put back
    pub at: u64,        // when the merge was (ms since 1970)
    pub rows: usize,    // rows put back
    pub skipped: usize, // rows left as they are: changed since the merge
}

/// One thing that happened to a world (see `Core::history`).
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub at: u64, // ms since 1970
    pub world: String,
    pub what: String, // "write", "forked from main", "merged into main", "commit", "discarded", ...
    pub rows: usize,  // rows written (for a merge or commit: rows the merged world wrote)
}

/// What `merge_locked` did: merged (rows applied, log position), or only planned (dry run).
enum Merged {
    /// rows applied, the log position, the notifications of effects it brought into main, and
    /// the branch's version if it stays open
    Done(usize, u64, Vec<(String, String)>, Option<u64>),
    /// with, into main of a folder attached to Postgres, what to check there once the locks are let go
    Plan(Box<(MergePlan, Option<attach::Dry>)>),
    /// refused by the acting agent's merge policy: its name, the rules (and merge checks) broken,
    /// and the branch's version they were checked at
    Review(String, Vec<String>, u64),
}

/// What `merge_locked` does with what it works out.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// merges
    Merge,
    /// only plans: nothing changes
    Dry,
    /// moves the branch's fork point up to its parent as it is (see `rebase`): the parent doesn't change
    Rebase,
}

/// Rows a logged merge settled (see `Op::MergeWith`), to settle them the same on replay.
fn stored_picks(rows: Vec<(String, u8, Option<String>)>) -> HashMap<String, Settle> {
    let settle = |kind, v: Option<String>| match kind {
        0 => Settle::Ours,
        1 => Settle::Theirs,
        _ => Settle::Stored(v.map(Val::from)),
    };
    rows.into_iter().map(|(k, kind, v)| (k, settle(kind, v))).collect()
}

fn settles(o: &MergeOptions) -> HashMap<String, Settle> {
    o.picks
        .iter()
        .map(|(k, p)| {
            let s = match p {
                Pick::Ours => Settle::Ours,
                Pick::Theirs => Settle::Theirs,
                Pick::Row(j) => Settle::Json(j.clone()),
            };
            (k.clone(), s)
        })
        .collect()
}

/// The table a key belongs to: a row's, or the one a schema, constraint or index row is for.
fn table_of(key: &str) -> &str {
    let k = ["_sonos_tables/", "_sonos_uniq/", "_sonos_ref/", "_sonos_idx/"].iter().find_map(|p| key.strip_prefix(p));
    let k = k.unwrap_or(key);
    k.split('/').next().unwrap_or(k)
}

/// The changes a partial merge takes (see `MergeOptions::only_tables`): whole tables, and rows by
/// key with the unique and reference rows only they own. A constraint row shared with a row left
/// out, or a table whose schema changed, would leave the target inconsistent: refused.
fn only(diff: Vec<Change>, o: &MergeOptions) -> Result<Vec<Change>> {
    if o.only_tables.is_empty() && o.only_keys.is_empty() {
        return Ok(diff);
    }
    let bad = |message: String| Error::Constraint { code: "22023", message };
    if let Some(k) = o.only_keys.iter().find(|k| reserved_table(k) || !k.contains('/')) {
        return Err(bad(format!("{k} isn't a row: ONLY KEYS takes rows as 'table/id'")));
    }
    let keys: std::collections::HashSet<&str> = o.only_keys.iter().map(String::as_str).collect();
    let mut out = vec![];
    for c in diff {
        if o.only_tables.iter().any(|t| t == table_of(&c.key)) {
            out.push(c);
        } else if keys.is_empty() || sql::btree_entry(&c.key) {
            // index entries aren't merged: they're rebuilt for the rows merged
        } else if let Some(t) = c.key.strip_prefix("_sonos_tables/") {
            if keys.iter().any(|k| table_of(k) == t) {
                return Err(bad(format!(
                    "the world changed table {t} itself (its columns or constraints), so its rows can't merge \
                     without it: use ONLY TABLES ({t})"
                )));
            }
        } else if sql::internal(&c.key) {
            let owners = sql::owners(&c.key, [c.before.as_deref(), c.after.as_deref()]);
            if let Some(mine) = owners.iter().find(|r| keys.contains(r.as_str())) {
                if let Some(other) = owners.iter().find(|r| !keys.contains(r.as_str())) {
                    let (mine, other) = (sql::show_key(mine), sql::show_key(other));
                    return Err(bad(format!(
                        "{mine} and {other} changed one unique value or reference together; add {other} to ONLY KEYS"
                    )));
                }
                out.push(c);
            }
        } else if keys.contains(c.key.as_str()) {
            out.push(c);
        }
    }
    Ok(out)
}

/// Worlds as they were at one moment (see `Core::past`): each one's state and ID.
struct Past {
    at: u64,
    worlds: HashMap<String, (State, String)>,
}

const PAST_CACHED: usize = 4;
/// The `born` of past views: never a live branch's, so their search caches stay apart.
const PAST: u64 = u64::MAX;

/// World names can't be empty, `@` means "as it was at" (see `Core::view`), and `_tx_` starts
/// the names of transactions' own worlds, which go uncounted and are discarded on opening (see
/// `sql::cleanup`): only `fork_info` makes those.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('@') {
        return Err(Error::Constraint {
            code: "42602",
            message: format!("'{name}' can't name a world: it's empty or has an @"),
        });
    }
    if name.starts_with("_tx_") {
        return Err(Error::Constraint {
            code: "42939",
            message: format!("'{name}' can't name a world: names starting with _tx_ are transactions'"),
        });
    }
    // `MERGE WORLD ours` reads the word as MERGE's own, so a world named so couldn't be merged
    // without quotes. Worlds made before this rule came in still load (replay doesn't check).
    if let Some(w) = MERGE_WORDS.iter().find(|w| name.eq_ignore_ascii_case(w)) {
        return Err(Error::Constraint {
            code: "42939",
            message: format!(
                "'{name}' can't name a world: MERGE WORLD {w} would read it as MERGE's own word; pick another name"
            ),
        });
    }
    Ok(())
}

/// A stamp for a world's change (see `Branch::stamp`): one counter for the process, starting from
/// the time in nanoseconds, so a stamp from before a restart never comes again.
fn stamp() -> u64 {
    // std's, never shuttle's: a static outlives one simulated run (see `tests/sim.rs`)
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static START: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    START.get_or_init(|| disk::now_ms().saturating_mul(1_000_000))
        + NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    /// This thread is putting rows back (RESTORE, UNDO MERGE, UNDO AGENT): main's protection
    /// lets it through (see `Core::guard_main`).
    static RECOVERING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks this thread as putting rows back until dropped (see `RECOVERING`).
struct Recovering(bool);

impl Recovering {
    fn enter() -> Recovering {
        Recovering(RECOVERING.replace(true))
    }
}

impl Drop for Recovering {
    fn drop(&mut self) {
        RECOVERING.set(self.0);
    }
}

/// The merge policy named for an agent's merge into an attached folder's main that no policy of its own judges.
const ATTACHED_REVIEW: &str = "attached";

/// Metadata key: when a world was last rebased (see `Core::rebase`), which only the database sets.
pub(crate) const REBASED: &str = "_sonos_rebased";

/// Words MERGE WORLD reads after WORLD as its own, not as a world's name (see `check_name`).
pub(crate) const MERGE_WORDS: [&str; 11] =
    ["using", "ours", "theirs", "confirm", "by", "resolve", "dry", "into", "only", "at", "target"];

/// Refuses a transaction's own world (see `check_name`): private to the session that made it,
/// which reaches it through its own paths, never by a name given to the API or in SQL.
fn not_tx(name: &str) -> Result<()> {
    match name.starts_with("_tx_") {
        true => Err(Error::NoBranch(name.into())),
        false => Ok(()),
    }
}

/// Rows open transactions' statements wrote or locked, by (world, key): who holds each, and in which mode (see
/// `Core::claim`); and the tables whole sessions hold instead, once they hold too many rows.
#[derive(Default)]
pub(crate) struct Intents {
    held: HashMap<(String, String), Vec<(u64, u8)>>,
    /// (world, table) -> the sessions holding all of it
    tables: HashMap<(String, String), Vec<(u64, u8)>>,
    /// (world, table) -> each session holding rows of it, and how many
    rows_in: HashMap<(String, String), HashMap<u64, u64>>,
    by_owner: HashMap<u64, Owned>,
    /// each session waiting for a row: the sessions it waits for, each with how many of its transactions had ended
    /// when it was seen holding (it is the same holder while that is still so)
    waiting: HashMap<u64, Vec<(u64, u64)>>,
    /// how many times each session let go of its intents, for a waiter to see one did
    ended: HashMap<u64, u64>,
    /// each session waiting for a row: the (world, table) of the row (see `Core::row_waits`)
    blocked_on: HashMap<u64, (String, String)>,
}

/// What a session holds: (world, key, savepoint level) of its rows, and (world, table, level) of its tables.
#[derive(Default)]
struct Owned {
    rows: Vec<(String, String, usize)>,
    tables: Vec<(String, String, usize)>,
}

/// More than this many rows held by one session are held as their tables (see `Core::claim`).
const ROW_INTENTS: usize = 10_000;

/// More than this many rows committed to its world since a READ COMMITTED transaction last looked (or more than it
/// wrote itself), and its own changes are put on top of the world instead of the world's changes under them (see
/// `Core::rebase_tx`).
const REBASE_ROWS: usize = 1024;

impl Intents {
    fn forget_row(
        held: &mut HashMap<(String, String), Vec<(u64, u8)>>,
        rows_in: &mut HashMap<(String, String), HashMap<u64, u64>>,
        owner: u64,
        world: &str,
        key: &str,
    ) {
        let id = (world.to_string(), key.to_string());
        if let Some(hs) = held.get_mut(&id) {
            hs.retain(|(o, _)| *o != owner);
            if hs.is_empty() {
                held.remove(&id);
            }
        }
        let table = (world.to_string(), table_of(key).to_string());
        if let Some(counts) = rows_in.get_mut(&table) {
            if let Some(n) = counts.get_mut(&owner) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    counts.remove(&owner);
                }
            }
            if counts.is_empty() {
                rows_in.remove(&table);
            }
        }
    }

    fn forget_table(tables: &mut HashMap<(String, String), Vec<(u64, u8)>>, owner: u64, world: &str, table: &str) {
        let id = (world.to_string(), table.to_string());
        if let Some(hs) = tables.get_mut(&id) {
            hs.retain(|(o, _)| *o != owner);
            if hs.is_empty() {
                tables.remove(&id);
            }
        }
    }
}

/// The rows and tables other sessions hold, by the strongest mode (see `Core::intents_of_others`).
#[derive(Default)]
pub(crate) struct Held {
    pub(crate) rows: HashMap<String, u8>,
    pub(crate) tables: HashMap<String, u8>,
}

impl Held {
    /// The mode `key`'s row is held in by others: its own, or its table's.
    pub(crate) fn of(&self, key: &str) -> Option<u8> {
        match (self.rows.get(key), self.tables.get(table_of(key))) {
            (Some(a), Some(b)) => Some(*a.max(b)),
            (a, b) => a.or(b).copied(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len() + self.tables.len()
    }
}

/// What `Core::claim` found.
pub(crate) enum Claim {
    /// taken
    Got,
    /// held by these sessions (each with how many of its transactions had ended) in a mode that conflicts: wait for
    /// one of them to end
    Blocked(Vec<(u64, u64)>),
    /// this row changed in the world since the transaction last read it (its key)
    Stale(String),
}

/// What `Core::rebase_tx` did.
pub(crate) enum Rebased {
    /// the world had not changed
    Same,
    Moved,
    /// a row (key) the transaction changed that the world changed too, and whether the transaction added it
    Conflict(String, bool),
}

/// A world's row locks left by commits: row key -> (`lock_clock` of the last FOR UPDATE, of the
/// last FOR NO KEY UPDATE).
type RowLocks = HashMap<String, (u64, u64)>;

/// SELECT ... FOR `mode`: Postgres's four row lock modes, weakest first.
pub(crate) struct RowLock;

impl RowLock {
    pub(crate) const KEY_SHARE: u8 = 0;
    pub(crate) const SHARE: u8 = 1;
    pub(crate) const NO_KEY_UPDATE: u8 = 2;
    pub(crate) const UPDATE: u8 = 3;

    /// Whether taking a lock of mode `want` conflicts with one of mode `held` on the row, as Postgres's lock modes
    /// do: FOR KEY SHARE only with FOR UPDATE, FOR SHARE with FOR NO KEY UPDATE and FOR UPDATE, FOR NO KEY UPDATE
    /// with all but FOR KEY SHARE, FOR UPDATE with every one.
    pub(crate) fn conflicts(want: u8, held: u8) -> bool {
        match want {
            Self::KEY_SHARE => held == Self::UPDATE,
            Self::SHARE => held >= Self::NO_KEY_UPDATE,
            Self::NO_KEY_UPDATE => held >= Self::SHARE,
            _ => true,
        }
    }
}

/// A merge refused for rows it locked that another merge changed or
/// locked first.
fn locks_lost(keys: &[String]) -> Error {
    const SHOWN: usize = 5;
    let mut rows = keys.iter().take(SHOWN).map(|k| sql::show_key(k)).collect::<Vec<_>>().join(", ");
    if keys.len() > SHOWN {
        rows += &format!(" and {} more", keys.len() - SHOWN);
    }
    Error::Constraint {
        code: "40001", // Postgres's serialization_failure: try again
        message: format!(
            "rows locked with FOR UPDATE or FOR SHARE were changed or locked by another commit since this began ({rows})"
        ),
    }
}

fn max_worlds(most: u64) -> Error {
    Error::Constraint {
        code: "53400", // configuration_limit_exceeded
        message: format!(
            "the database may have at most {most} worlds besides main (max_worlds); merge or drop some, \
             or raise it with ALTER SYSTEM SET max_worlds"
        ),
    }
}

/// Takes a lock whose data stays whole even if a holder panicked (a flag, a counter, `()`), so
/// one panic doesn't make every later caller panic too.
fn lock<T>(m: &Mutex<T>) -> sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn no_history(message: String) -> Error {
    Error::Constraint { code: "22023", message }
}

/// ms since 1970 as a UTC time, for messages.
fn show_ms(ms: u64) -> String {
    sql::show_moment(ms)
}

/// Rows a branch may keep as a list at a checkpoint instead of in pages of its own.
// ponytail: fixed limits, and the manifest is rewritten whole each checkpoint; write it
// incrementally if millions of worlds with lists make checkpoints slow
const DELTA_ROWS: usize = 32;
const DELTA_BYTES: usize = 4096;

/// Rows a transaction's changes may hold in memory before its commit is one too big to merge row by row there (see
/// `Core::merge_published`).
const BIG_TX_ROWS: usize = 10_000;

/// Whether a transaction (or savepoint) whose fork point is `base` and state `ours` is too big to merge row by row in
/// memory: its changes went into pages of their own (a statement that spilled, a checkpoint), or add many rows to the
/// ones its fork point holds in memory.
fn big_tx(base: &State, ours: &State) -> bool {
    ours.tree != base.tree || ours.over.len().saturating_sub(base.over.len()) >= BIG_TX_ROWS
}

/// A branch's changes since its fork, if few enough to keep as a list: its data must still sit
/// on its fork point's tree.
fn delta_of(base: &State, data: &State) -> Option<Vec<(String, Option<Val>)>> {
    if base.tree != data.tree {
        return None;
    }
    let (mut out, mut bytes) = (vec![], 0);
    for d in base.over.diff(&data.over) {
        let (DiffItem::Add(k, v) | DiffItem::Update { new: (k, v), .. }) = d else { return None };
        bytes += k.len() + v.as_ref().map_or(0, |v| v.len());
        if out.len() == DELTA_ROWS || bytes > DELTA_BYTES {
            return None;
        }
        out.push((k.to_string(), v.clone()));
    }
    Some(out)
}

/// What `is` has on top of `was`, when it is `was` with entries added to its overlay (or changed) and nothing else:
/// put on `was` folded into a tree, they give `is`'s rows. None if it is any other state.
fn grown(was: &State, is: &State) -> Option<Over> {
    if was.tree != is.tree {
        return None;
    }
    let mut over = Over::new();
    for d in was.over.diff(&is.over) {
        match d {
            DiffItem::Add(k, v) | DiffItem::Update { new: (k, v), .. } => {
                over.insert(k.clone(), v.clone());
            }
            DiffItem::Remove(..) => return None,
        }
    }
    Some(over)
}

/// Folds a state's overlay into a new tree. With a `near` state on the same tree that is already
/// folded, only the overlay entries where the two disagree are applied, on top of `near`'s root.
fn fold(pages: &Pages, s: &State, near: Option<(&State, Hash)>) -> io::Result<Hash> {
    let tree = *s.tree.as_ref().expect("on-disk states always have a tree").0;
    let (root, changes): (Hash, Vec<(String, Option<Val>)>) = match near {
        Some((n, n_root)) if n.tree == s.tree => {
            let mut changes = vec![];
            for d in n.over.diff(&s.over) {
                match d {
                    DiffItem::Add(k, v) | DiffItem::Update { new: (k, v), .. } => {
                        changes.push((k.to_string(), v.clone()))
                    }
                    // `s` doesn't override this key, so it has the tree's value
                    DiffItem::Remove(k, _) => changes.push((k.to_string(), prolly::get(pages, &tree, k)?)),
                }
            }
            (n_root, changes)
        }
        _ => (tree, s.over.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()),
    };
    if changes.is_empty() {
        return Ok(root);
    }
    prolly::apply(pages, &root, &changes)
}

/// A branch at one moment (see `Core::view`, `Core::batch_with`).
#[derive(Clone)]
pub struct View<'a> {
    core: &'a Core,
    born: u64,
    s: State,
    branch: String, // the world's name (without any `@when`)
    /// the world's read set, when it checks its reads: what's read through this view goes in it
    reads: Option<Recorder>,
}

impl View<'_> {
    /// Whether what's read through this view goes in a world's read set (see `stale`).
    pub(crate) fn recording(&self) -> bool {
        self.reads.is_some()
    }

    /// This view, read past the world's read set: for a scan that recorded what it may read
    /// itself (`filtered`).
    pub(crate) fn unrecorded(&self) -> Self {
        View { reads: None, ..self.clone() }
    }

    /// Records in the world's read set that a scan of `table` read the rows `f` matches.
    pub(crate) fn filtered(&self, table: &str, f: sql::RowFilter) {
        if let Some(r) = &self.reads {
            r.lock().unwrap().filtered(table, f);
        }
    }

    pub fn get(&self, k: &str) -> Result<Option<Val>> {
        if let Some(r) = &self.reads {
            r.lock().unwrap().key(k);
        }
        let v = self.core.val(&self.s, k)?;
        if traced::on() {
            traced::key(self, k, &v);
        }
        Ok(v)
    }

    /// `Core::find` on this view.
    pub fn find(&self, q: &Query) -> Result<(usize, Vec<Hit>)> {
        if let Some(r) = &self.reads {
            r.lock().unwrap().table(&q.table);
        }
        if traced::on() {
            traced::spoil();
        }
        self.core.find_at(self.born, &self.s, q)
    }

    /// `find`, or None when the table has no search index yet and building one would read more
    /// than `budget` bytes of rows: for callers that can scan instead (SQL's `column = value`).
    pub fn find_within(&self, q: &Query, budget: usize) -> Result<Option<(usize, Vec<Hit>)>> {
        if let Some(r) = &self.reads {
            r.lock().unwrap().table(&q.table);
        }
        if traced::on() {
            traced::spoil();
        }
        self.core.find_within(self.born, &self.s, q, budget)
    }

    /// Rows whose key starts with `prefix`, in key order, read lazily.
    pub fn prefix<'b>(&'b self, prefix: &'b str) -> Result<impl Iterator<Item = Result<(String, Val)>> + 'b> {
        if let Some(r) = &self.reads {
            r.lock().unwrap().range(prefix, None, None);
        }
        let rows = self.core.rows(&self.s, prefix)?;
        let rows = rows.take_while(move |r| r.as_ref().map_or(true, |(k, _)| k.starts_with(prefix)));
        Ok(traced::prefix(self, prefix, rows))
    }
}

impl View<'_> {
    /// Rows in each run `marks` cuts, about: the size scans split tables into (sql's PAR_MIN / 2),
    /// so a part stays small however big its table.
    pub(crate) const MARK_ROWS: usize = 4096;

    /// Keys that cut the rows under `prefix` into runs of about `rows` rows each (the overlay's
    /// rows: every 1,024), each with about how many rows its run (up to and including it) holds,
    /// in key order; and about how many rows there are in all. For splitting a scan across
    /// threads: it reads no leaf pages.
    pub fn marks(&self, prefix: &str, rows: usize) -> Result<(Vec<(String, usize)>, usize)> {
        if traced::on() {
            traced::spoil();
        }
        let upto = format!("{prefix}\u{10ffff}");
        let mut marks = match (self.core.tree(&self.s), &self.core.pages) {
            (Some(t), Some(p)) => prolly::marks(p, t, prefix, &upto, rows)?,
            _ => vec![],
        };
        // the runs, plus one more for the rows after the last mark
        let mut total: usize = marks.iter().map(|m| m.1).sum::<usize>() + marks.first().map_or(0, |m| m.1);
        let mut n = 0;
        for (k, _) in
            self.s.over.range::<_, str>((std::ops::Bound::Included(prefix), std::ops::Bound::Excluded(upto.as_str())))
        {
            n += 1;
            if n % 1024 == 0 {
                marks.push((k.to_string(), 1024));
            }
        }
        total += n;
        marks.sort_unstable();
        Ok((marks, total))
    }

    /// Calls `f` on the rows under `prefix` after key `after` up to and including key `upto`
    /// (None: no bound), in key order, until it returns false. Lends each row: for scans, which
    /// would otherwise spend much of their time copying keys.
    pub fn each<E: From<Error>>(
        &self,
        prefix: &str,
        after: Option<&str>,
        upto: Option<&str>,
        mut f: impl FnMut(&str, &Val) -> std::result::Result<bool, E>,
    ) -> std::result::Result<(), E> {
        if traced::on() {
            traced::spoil();
        }
        // what a scan read: up to where it stopped, if it stopped early (a LIMIT)
        let mut last: Option<String> = None;
        let mut stopped = false;
        let tracked = self.reads.is_some();
        let mut f = |k: &str, v: &Val| -> std::result::Result<bool, E> {
            let go = f(k, v)?;
            if tracked {
                last = Some(k.to_string());
                stopped = !go;
            }
            Ok(go)
        };
        let out = self.each_all(prefix, after, upto, &mut f);
        if let Some(r) = &self.reads {
            let upto = if stopped { last.as_deref() } else { upto };
            r.lock().unwrap().range(prefix, after, upto);
        }
        out
    }

    fn each_all<E: From<Error>>(
        &self,
        prefix: &str,
        after: Option<&str>,
        upto: Option<&str>,
        mut f: impl FnMut(&str, &Val) -> std::result::Result<bool, E>,
    ) -> std::result::Result<(), E> {
        let from = after.map_or_else(|| prefix.to_string(), |a| format!("{a}\0")); // the next key after `a`
        let within = |k: &str| k.starts_with(prefix) && upto.is_none_or(|u| k <= u);
        let f: &mut dyn FnMut(&str, &Val) -> std::result::Result<bool, E> = &mut f;
        /// `f` on the overlay's rows before key `k` (None: all that are left); false once past
        /// the run or when `f` says stop.
        fn before<'o, E>(
            over: &mut Peekable<impl Iterator<Item = (&'o Val, &'o Option<Val>)>>,
            k: Option<&str>,
            within: &dyn Fn(&str) -> bool,
            f: &mut dyn FnMut(&str, &Val) -> std::result::Result<bool, E>,
        ) -> std::result::Result<bool, E> {
            while let Some((ok, ov)) = over.next_if(|(ok, _)| k.is_none_or(|k| &***ok < k)) {
                if !within(ok) {
                    return Ok(false);
                }
                if let Some(v) = ov
                    && !f(ok, v)?
                {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        let mut over = self.s.over.range::<_, str>(from_key(&from)).peekable();
        let mut ended = false;
        if let (Some(t), Some(p)) = (self.core.tree(&self.s), &self.core.pages) {
            let mut failed = None; // `f`'s error, carried out of the tree walk
            let walk = prolly::visit(p, t, &from, &mut |k, v| {
                let mut step = || -> std::result::Result<bool, E> {
                    if !before(&mut over, Some(k), &within, f)? || !within(k) {
                        return Ok(false);
                    }
                    match over.next_if(|(ok, _)| &***ok == k) {
                        Some((_, None)) => Ok(true), // deleted since the checkpoint
                        Some((ok, Some(ov))) => f(ok, ov),
                        None => f(k, v),
                    }
                };
                let go = step().map_err(|e| {
                    failed = Some(e);
                    std::io::Error::other("stopped")
                })?;
                ended = !go;
                Ok::<_, std::io::Error>(go)
            });
            if let Some(e) = failed {
                return Err(e);
            }
            walk.map_err(|e| E::from(Error::from(e)))?;
        }
        if !ended {
            before(&mut over, None, &within, f)?;
        }
        Ok(())
    }
}

impl View<'_> {
    /// `each` backwards: the rows under `prefix` after key `after` (exclusive) up to and including
    /// key `upto`, in descending key order, until `f` returns false.
    pub fn each_rev<E: From<Error>>(
        &self,
        prefix: &str,
        after: Option<&str>,
        upto: Option<&str>,
        mut f: impl FnMut(&str, &Val) -> std::result::Result<bool, E>,
    ) -> std::result::Result<(), E> {
        if let Some(r) = &self.reads {
            r.lock().unwrap().range(prefix, after, upto); // all it could reach: stopping early goes unnoticed
        }
        if traced::on() {
            traced::spoil();
        }
        let start = upto.map_or_else(|| format!("{prefix}\u{10ffff}"), String::from);
        let within = |k: &str| k.starts_with(prefix) && after.is_none_or(|a| k > a);
        let f: &mut dyn FnMut(&str, &Val) -> std::result::Result<bool, E> = &mut f;
        /// `f` on the overlay's rows after key `k` (None: all that are left), going down.
        fn above<'o, E>(
            over: &mut Peekable<impl Iterator<Item = (&'o Val, &'o Option<Val>)>>,
            k: Option<&str>,
            within: &dyn Fn(&str) -> bool,
            f: &mut dyn FnMut(&str, &Val) -> std::result::Result<bool, E>,
        ) -> std::result::Result<bool, E> {
            while let Some((ok, ov)) = over.next_if(|(ok, _)| k.is_none_or(|k| &***ok > k)) {
                if !within(ok) {
                    return Ok(false);
                }
                if let Some(v) = ov
                    && !f(ok, v)?
                {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        let mut over = self
            .s
            .over
            .range::<_, str>((std::ops::Bound::Unbounded, std::ops::Bound::Included(start.as_str())))
            .rev()
            .peekable();
        let mut ended = false;
        if let (Some(t), Some(p)) = (self.core.tree(&self.s), &self.core.pages) {
            let mut failed = None;
            let walk = prolly::visit_rev(p, t, &start, &mut |k, v| {
                let mut step = || -> std::result::Result<bool, E> {
                    if !above(&mut over, Some(k), &within, f)? || !within(k) {
                        return Ok(false);
                    }
                    match over.next_if(|(ok, _)| &***ok == k) {
                        Some((_, None)) => Ok(true), // deleted since the checkpoint
                        Some((ok, Some(ov))) => f(ok, ov),
                        None => f(k, v),
                    }
                };
                let go = step().map_err(|e| {
                    failed = Some(e);
                    std::io::Error::other("stopped")
                })?;
                ended = !go;
                Ok::<_, std::io::Error>(go)
            });
            if let Some(e) = failed {
                return Err(e);
            }
            walk.map_err(|e| E::from(Error::from(e)))?;
        }
        if !ended {
            above(&mut over, None, &within, f)?;
        }
        Ok(())
    }
}

/// An overlay merged over a tree, overlay first; `None` in the overlay hides the tree's row.
type OverIter<'a> = Box<dyn Iterator<Item = (&'a Val, &'a Option<Val>)> + 'a>;

struct Rows<'a> {
    over: Peekable<OverIter<'a>>,
    tree: Option<Peekable<prolly::Iter<'a>>>,
}

impl Iterator for Rows<'_> {
    type Item = Result<(String, Val)>;

    fn next(&mut self) -> Option<Self::Item> {
        enum Next {
            Done,
            Tree,
            Over { shadows_tree: bool },
            Err,
        }
        loop {
            let next = {
                let o: Option<&str> = self.over.peek().map(|(k, _)| &***k);
                let t = match self.tree.as_mut().and_then(|t| t.peek()) {
                    Some(Err(_)) => Some(Err(())),
                    Some(Ok((k, _))) => Some(Ok(k.as_str())),
                    None => None,
                };
                match (o, t) {
                    (_, Some(Err(()))) => Next::Err,
                    (None, None) => Next::Done,
                    (None, Some(Ok(_))) => Next::Tree,
                    (Some(o), Some(Ok(t))) if t < o => Next::Tree,
                    (Some(o), t) => Next::Over { shadows_tree: t == Some(Ok(o)) },
                }
            };
            match next {
                Next::Done => return None,
                Next::Err => {
                    let e = self.tree.take()?.next()?.unwrap_err();
                    return Some(Err(e.into()));
                }
                Next::Tree => return self.tree.as_mut()?.next().map(|r| r.map_err(Error::from)),
                Next::Over { shadows_tree } => {
                    if shadows_tree {
                        self.tree.as_mut()?.next();
                    }
                    let (k, v) = self.over.next()?;
                    if let Some(v) = v {
                        return Some(Ok((k.to_string(), v.clone())));
                    }
                }
            }
        }
    }
}

/// A sequence's reserved block (next value, end) and its shared counter (value, origin).
pub(crate) type SequenceMark = (Option<(i64, i64)>, (i64, Option<String>));

#[cfg(test)]
mod tests {
    use super::*;

    /// A world merged or discarded leaves nothing in the search caches that would keep its
    /// state (and memory) alive.
    #[test]
    fn retired_worlds_leave_the_search_caches() {
        let db = Db::new();
        for i in 0..50 {
            db.put(MAIN, &format!("items/{i}"), &format!("{{\"n\": {}}}", i % 5)).unwrap();
        }
        let filters = vec![("n".into(), serde_json::json!(9))];
        let q = Query { table: "items".into(), filters, k: 10, ..Default::default() };
        let cached = |born: u64| {
            let a = db.answers.lock().unwrap().keys().any(|k| k.0 == born);
            (a, db.deltas.lock().unwrap().keys().any(|k| k.1 == born))
        };
        for (w, merge) in [("gone", false), ("merged", true)] {
            db.fork(MAIN, w).unwrap();
            db.put(w, "items/1", "{\"n\": 9}").unwrap();
            let born = db.branch(w).unwrap().born;
            assert_eq!(db.find(w, &q).unwrap().0, 1);
            assert_eq!(cached(born), (true, true));
            match merge {
                true => drop(db.merge(w, Resolve::Fail).unwrap()),
                false => db.discard(w).unwrap(),
            }
            assert_eq!(cached(born), (false, false), "{w}");
        }
    }
}
