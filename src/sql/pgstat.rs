//! The cumulative statistics system, as Postgres 16's `pgstat` keeps it: what each table, index and
//! function was counted doing, for each database (world) of each Chronos database, from when the process
//! opened it or the counters were last reset.
//!
//! A session counts what its statements do: the rows they insert, update and delete in each table, the
//! sequential and index reads they make, and with `track_functions` the calls of its functions. Rows
//! written are counted by (sub)transaction as Postgres counts them, so a rolled-back insert ends as a
//! dead row and a TRUNCATE forgets what its transaction did before it. When the transaction ends the
//! counts are added to the database's, as Postgres flushes them (Chronos does so at every transaction's
//! end, which is what `pg_stat_force_next_flush()` asks of Postgres). Reads honour
//! `stats_fetch_consistency` (`none`, `cache`, `snapshot`) within a transaction.
//!
//! Chronos has no pages, buffers, WAL, SLRUs, archiver or vacuum: those counters read 0 (or NULL where
//! Postgres has none to show), and their reset times are the server's start, as a fresh cluster's are.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use super::catalog::Catalog;
use super::{Ctx, Result, SessState, SqlError, Type, Value, Violation, err, rel_part, schema_part, time};

/// A table's (or an index's) counters: pending ones (what a session counted, not yet added in) and kept
/// ones (the database's), as Postgres's PgStat_TableCounts and PgStat_StatTabEntry hold them.
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Counts {
    /// sequential scans of a table, scans of an index
    numscans: i64,
    /// rows a table's sequential scans read; entries an index's scans gave
    returned: i64,
    /// rows an index's scans fetched from its table
    fetched: i64,
    inserted: i64,
    updated: i64,
    deleted: i64,
    /// pending: what the live and dead rows changed by; kept: how many there are
    live: i64,
    dead: i64,
    /// rows changed since the table was last analyzed (it never is)
    changed: i64,
    ins_since_vacuum: i64,
    /// kept: when it was last scanned (µs since 1970), 0 for never
    lastscan: i64,
    /// pending: the transaction that ended emptied or dropped it (its live and dead rows start over)
    truncdropped: bool,
}

/// What a (sub)transaction wrote to a table, as Postgres's PgStat_TableXactStatus: the counts before a
/// TRUNCATE (or DROP) in it, which a rollback of it puts back.
#[derive(Clone, Copy, Default, Debug)]
struct Trans {
    ins: i64,
    upd: i64,
    del: i64,
    truncdropped: bool,
    pre: (i64, i64, i64),
}

impl Trans {
    /// save_truncdrop_counters: the counts as they were before the first TRUNCATE (any DROP).
    fn save(&mut self, drop: bool) {
        if !self.truncdropped || drop {
            self.pre = (self.ins, self.upd, self.del);
            self.truncdropped = true;
        }
    }

    /// restore_truncdrop_counters: what a rolled-back TRUNCATE or DROP had forgotten.
    fn restore(&mut self) {
        if self.truncdropped {
            (self.ins, self.upd, self.del) = self.pre;
        }
    }

    /// TRUNCATE (and DROP): the rows written so far are gone.
    fn empty(&mut self, drop: bool) {
        self.save(drop);
        (self.ins, self.upd, self.del) = (0, 0, 0);
    }
}

/// A function's calls, and the time they took (µs) in all and in themselves (without the functions they called).
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Calls {
    calls: i64,
    total_us: i64,
    self_us: i64,
}

/// A database's own counters (pg_stat_database).
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct DbCounts {
    commits: i64,
    rollbacks: i64,
    returned: i64,
    fetched: i64,
    inserted: i64,
    updated: i64,
    deleted: i64,
    sessions: i64,
    /// when its counters were last reset (µs since 1970), 0 for never
    reset: i64,
}

/// A database's statistics: by table (its stored name), by index (its table's and its own name), by function oid.
#[derive(Clone, Default, Debug)]
pub(super) struct DbStats {
    tables: HashMap<String, Counts>,
    indexes: HashMap<(String, String), Counts>,
    funcs: HashMap<i64, Calls>,
    db: DbCounts,
}

/// The SLRUs Postgres 16 counts, in pg_stat_slru's order.
const SLRUS: [&str; 8] =
    ["CommitTs", "MultiXactMember", "MultiXactOffset", "Notify", "Serial", "Subtrans", "Xact", "other"];

/// The cluster's fixed-numbered statistics: when each was last reset (µs since 1970), and the checkpoints
/// CHECKPOINT asked for.
#[derive(Clone, Debug)]
pub(super) struct Fixed {
    archiver: i64,
    bgwriter: i64,
    io: i64,
    prefetch: i64,
    wal: i64,
    slru: [i64; 8],
    requested: i64,
}

impl Fixed {
    /// A fresh cluster's: reset when it started.
    fn fresh() -> Fixed {
        let t = time::started();
        Fixed { archiver: t, bgwriter: t, io: t, prefetch: t, wal: t, slru: [t; 8], requested: 0 }
    }
}

impl Default for Fixed {
    fn default() -> Fixed {
        Fixed::fresh()
    }
}

/// One Chronos database's statistics: each world's, the shared objects' (datid 0), and the fixed ones.
#[derive(Default)]
struct Cluster {
    dbs: HashMap<String, DbStats>,
    shared: DbCounts,
    fixed: Fixed,
}

/// The statistics of every Chronos database the process has open, by `Core::uid`.
static CLUSTERS: LazyLock<Mutex<HashMap<u64, Cluster>>> = LazyLock::new(Default::default);

fn clusters() -> MutexGuard<'static, HashMap<u64, Cluster>> {
    CLUSTERS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Forgets a database that closed.
pub(super) fn forget(uid: u64) {
    clusters().remove(&uid);
}

/// What a read of the statistics sees: its own database's, every database's own counters, the shared
/// objects' and the fixed ones.
#[derive(Clone, Default, Debug)]
pub(super) struct Seen {
    here: DbStats,
    dbs: HashMap<String, DbCounts>,
    shared: DbCounts,
    fixed: Fixed,
}

/// The statistics as they are now, for database `home` of `core`.
fn build(core: &crate::Core, home: &str) -> Seen {
    let all = clusters();
    let Some(cl) = all.get(&core.uid) else { return Seen::default() };
    Seen {
        here: cl.dbs.get(home).cloned().unwrap_or_default(),
        dbs: cl.dbs.iter().map(|(w, d)| (w.clone(), d.db)).collect(),
        shared: cl.shared,
        fixed: cl.fixed.clone(),
    }
}

/// stats_fetch_consistency, as a snapshot of the statistics is: none taken (or `none`), entries cached
/// as they're read (`cache`), or all of them at once (`snapshot`).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
enum Mode {
    #[default]
    None,
    Cache,
    Snapshot,
}

/// The transaction's snapshot of the statistics, as `Mode` says it was taken, and when; `clear`: the
/// setting changed since, so the next read takes a new one (Postgres's force_stats_snapshot_clear).
#[derive(Clone, Default, Debug)]
struct Snap {
    mode: Mode,
    at: i64,
    seen: Option<Arc<Seen>>,
    clear: bool,
}

/// What a session has counted and not yet added to its database's statistics, its open transaction's
/// rows written by (sub)transaction level, and its snapshot of the statistics.
#[derive(Clone, Default, Debug)]
pub(super) struct Pending {
    tables: HashMap<String, Counts>,
    indexes: HashMap<(String, String), Counts>,
    funcs: HashMap<i64, Calls>,
    /// the transaction's levels: its own, then one for each open savepoint
    levels: Vec<HashMap<String, Trans>>,
    db: DbCounts,
    /// a statement of the transaction made, dropped or renamed objects: what the statistics keep of
    /// objects gone is dropped when it ends
    ddl: bool,
    snap: Snap,
}

impl Pending {
    /// The current level's counts for `table`.
    fn trans(&mut self, table: &str) -> &mut Trans {
        if self.levels.is_empty() {
            self.levels.push(HashMap::new());
        }
        let n = self.levels.len() - 1;
        self.levels[n].entry(table.to_string()).or_default()
    }

    /// AtEOSubXact_PgStat_Relations: a savepoint's level released (`commit`) or rolled back.
    fn sub_end(&mut self, commit: bool) {
        let Some(top) = self.levels.pop() else { return };
        for (name, mut t) in top {
            match self.levels.last_mut() {
                Some(up) if commit => match up.get_mut(&name) {
                    Some(u) if t.truncdropped => {
                        u.save(false);
                        (u.ins, u.upd, u.del) = (t.ins, t.upd, t.del);
                    }
                    Some(u) => {
                        u.ins += t.ins;
                        u.upd += t.upd;
                        u.del += t.del;
                    }
                    None => {
                        up.insert(name, t);
                    }
                },
                _ => {
                    // the attempted writes count all the same; what it inserted is dead
                    t.restore();
                    let c = self.tables.entry(name).or_default();
                    c.inserted += t.ins;
                    c.updated += t.upd;
                    c.deleted += t.del;
                    c.dead += t.ins + t.upd;
                }
            }
        }
    }

    /// AtEOXact_PgStat_Relations: the transaction committed or rolled back (its open savepoints with it).
    fn end_xact(&mut self, commit: bool) {
        while self.levels.len() > 1 {
            self.sub_end(commit);
        }
        let Some(top) = self.levels.pop() else { return };
        for (name, mut t) in top {
            if !commit {
                t.restore();
            }
            let c = self.tables.entry(name).or_default();
            c.inserted += t.ins;
            c.updated += t.upd;
            c.deleted += t.del;
            if commit {
                c.truncdropped = t.truncdropped;
                if t.truncdropped {
                    (c.live, c.dead) = (0, 0);
                }
                // an insert adds a live row and a delete takes one; an update or delete leaves a dead one
                c.live += t.ins - t.del;
                c.dead += t.upd + t.del;
                c.changed += t.ins + t.upd + t.del;
            } else {
                c.dead += t.ins + t.upd;
            }
        }
    }

    /// The snapshot, unless the setting changed since it was taken.
    fn prep(&mut self) {
        if self.snap.clear {
            self.snap = Snap::default();
        }
    }

    /// The rows this session wrote to `table` in its open transaction and before (not yet added in).
    fn written(&self, table: &str) -> (i64, i64, i64) {
        let c = self.tables.get(table).copied().unwrap_or_default();
        let (mut i, mut u, mut d) = (c.inserted, c.updated, c.deleted);
        for t in self.levels.iter().filter_map(|l| l.get(table)) {
            i += t.ins;
            u += t.upd;
            d += t.del;
        }
        (i, u, d)
    }

    /// Whether this session has counts for `table` it hasn't added in.
    fn has_table(&self, table: &str) -> bool {
        self.tables.contains_key(table) || self.levels.iter().any(|l| l.contains_key(table))
    }
}

/// The running statement's session, if it has one.
fn session() -> Option<Arc<Ctx>> {
    super::STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone()))
}

/// `f` on the running session's pending counts. Skipped (nothing counted) where the session's state is
/// held: a count is never worth a wait or a deadlock.
fn pending<R>(f: impl FnOnce(&mut Pending) -> R) -> Option<R> {
    let ctx = session()?;
    let mut st = ctx.state.try_lock().ok()?;
    Some(f(&mut st.stats))
}

/// A setting's value for the session, as SET or SET LOCAL left it.
fn setting<'a>(st: &'a SessState, name: &str) -> Option<&'a str> {
    st.local.get(name).or_else(|| st.settings.get(name)).map(String::as_str)
}

fn consistency(st: &SessState) -> Mode {
    match setting(st, "stats_fetch_consistency") {
        Some("none") => Mode::None,
        Some("snapshot") => Mode::Snapshot,
        _ => Mode::Cache,
    }
}

/// SET stats_fetch_consistency changed it: the next read takes a new snapshot (as Postgres's assign hook says).
pub(super) fn consistency_changed(st: &mut SessState) {
    st.stats.snap.clear = true;
}

// ---------------------------------------------------------------- counting

/// Rows written to table `table` (its stored name): inserted, updated, deleted.
pub(super) fn wrote(table: &str, ins: i64, upd: i64, del: i64) {
    if ins == 0 && upd == 0 && del == 0 {
        return;
    }
    pending(|p| {
        let t = p.trans(table);
        t.ins += ins;
        t.upd += upd;
        t.del += del;
    });
}

/// TRUNCATE emptied `table` (pgstat_count_truncate).
pub(super) fn truncated(table: &str) {
    pending(|p| p.trans(table).empty(false));
}

/// An INSERT's rows by the table they went to (a partitioned table's are its partitions'): inserted, and
/// updated by ON CONFLICT DO UPDATE. Counted when it's dropped, at the statement's end (or its error).
#[derive(Default)]
pub(super) struct Tally(Option<(String, i64, i64)>);

impl Tally {
    pub(super) fn add(&mut self, table: &str, ins: i64, upd: i64) {
        if let Some((n, i, u)) = &mut self.0
            && n.as_str() == table
        {
            *i += ins;
            *u += upd;
            return;
        }
        self.flush();
        self.0 = Some((table.to_string(), ins, upd));
    }

    fn flush(&mut self) {
        if let Some((n, i, u)) = self.0.take() {
            wrote(&n, i, u, 0);
        }
    }
}

impl Drop for Tally {
    fn drop(&mut self) {
        self.flush();
    }
}

/// How a table's rows are read: a sequential scan, or through an index (by its name).
#[derive(Clone, Debug)]
pub(super) enum Read {
    Seq,
    Index(String),
}

thread_local! {
    /// How each table's rows are about to be read, as the planner decided last (see `decided`).
    static DECIDED: RefCell<HashMap<String, Read>> = RefCell::new(HashMap::new());
    /// Time the functions called by the function running now took, µs (for its self time).
    static CHILD_US: Cell<i64> = const { Cell::new(0) };
}

/// The planner chose `how` to read `table`'s rows: what the read that follows counts (see `taken`).
pub(super) fn decided(table: &str, how: Read) {
    DECIDED.with_borrow_mut(|d| {
        d.insert(table.to_string(), how);
    });
}

/// How `table`'s rows are being read, as last decided (once: a decision counts one read).
pub(super) fn taken(table: &str) -> Option<Read> {
    DECIDED.with_borrow_mut(|d| d.remove(table))
}

/// A read of `table` ended: `asked` rows or entries looked at, `found` of them there.
pub(super) fn read(table: &str, how: &Read, asked: i64, found: i64) {
    pending(|p| match how {
        Read::Seq => {
            let c = p.tables.entry(table.to_string()).or_default();
            c.numscans += 1;
            c.returned += found;
        }
        Read::Index(ix) => {
            let c = p.indexes.entry((table.to_string(), ix.clone())).or_default();
            c.numscans += 1;
            c.returned += asked;
            c.fetched += found;
        }
    });
}

/// A call of a function under way, timed for `track_functions` (see `calling`).
pub(super) struct Timing {
    oid: i64,
    at: std::time::Instant,
    outer: i64,
}

/// A call of function `f` starting: timed when `track_functions` counts it (`pl` counts PL/pgSQL's,
/// `all` SQL and C functions' too, as Postgres's fmgr has them).
pub(super) fn calling(f: &super::functions::Function) -> Option<Timing> {
    let ctx = session()?;
    let counted = {
        let st = ctx.state.try_lock().ok()?;
        match setting(&st, "track_functions") {
            Some("all") => true,
            Some("pl") => f.lang_name() == "plpgsql",
            _ => false,
        }
    };
    if !counted {
        return None;
    }
    let oid = super::catalog_objects::function_oid(f);
    Some(Timing { oid, at: std::time::Instant::now(), outer: CHILD_US.replace(0) })
}

/// The call ended (`ok`: it returned, rather than failing, which Postgres doesn't count).
pub(super) fn called(t: Timing, ok: bool) {
    let total = i64::try_from(t.at.elapsed().as_micros()).unwrap_or(i64::MAX);
    let children = CHILD_US.replace(t.outer.saturating_add(total));
    if ok {
        pending(|p| {
            let c = p.funcs.entry(t.oid).or_default();
            c.calls += 1;
            c.total_us += total;
            c.self_us += (total - children).max(0);
        });
    }
}

/// A connection to world `world` began (pg_stat_database.sessions).
pub(super) fn connected(core: &crate::Core, world: &str) {
    let mut all = clusters();
    all.entry(core.uid).or_default().dbs.entry(world.to_string()).or_default().db.sessions += 1;
}

/// CHECKPOINT ran (pg_stat_bgwriter.checkpoints_req).
pub(super) fn checkpointed(core: &crate::Core) {
    clusters().entry(core.uid).or_default().fixed.requested += 1;
}

// ---------------------------------------------------------------- statements and transactions

/// Where a session's transaction stood before a statement, and its pending counts then (what a
/// statement run again starts from: see `again`).
pub(super) struct Before {
    in_tx: bool,
    failed: bool,
    kept: Pending,
}

/// A statement is about to run in a session whose transaction is open (`in_tx`, `failed` once a statement failed).
pub(super) fn before(state: &Mutex<SessState>, in_tx: bool, failed: bool) -> Before {
    DECIDED.with_borrow_mut(HashMap::clear);
    let kept = state.lock().map(|st| st.stats.clone()).unwrap_or_default();
    Before { in_tx, failed, kept }
}

/// The statement runs again (it ran out of sequence values): what it counted the first time is forgotten.
pub(super) fn again(state: &Mutex<SessState>, b: &Before) {
    DECIDED.with_borrow_mut(HashMap::clear);
    if let Ok(mut st) = state.lock() {
        let snap = st.stats.snap.clone();
        st.stats = b.kept.clone();
        st.stats.snap = snap;
    }
}

/// What a statement did to its session's transaction (see `stepped`).
pub(super) struct After {
    pub(super) ok: bool,
    /// whether a transaction is open after it, and its savepoints
    pub(super) in_tx: bool,
    pub(super) saves: usize,
    /// ROLLBACK, or ROLLBACK TO a savepoint
    pub(super) rollback: bool,
    pub(super) rollback_to: bool,
    /// it made, dropped or renamed objects
    pub(super) ddl: bool,
    /// of those, it may have dropped or renamed some (not only made them)
    pub(super) removes: bool,
}

/// A statement of the session ran, in world `world` (its transaction's) of database `home`: its counts
/// go to its transaction's level; savepoints it made, released or rolled back to change the levels;
/// and a transaction that ended (the statement's own, outside a transaction block) adds its counts to
/// the database's. `temp`: whether a name is one of the session's temporary tables.
pub(super) fn stepped(
    state: &Mutex<SessState>,
    core: &crate::Core,
    home: &str,
    world: &str,
    b: &Before,
    a: After,
    temp: &dyn Fn(&str) -> bool,
) {
    DECIDED.with_borrow_mut(HashMap::clear);
    let Ok(mut st) = state.lock() else { return };
    let p = &mut st.stats;
    // (what a transaction block made goes if it rolls back: its end looks the counts over; a statement of its own
    // that only made objects leaves none gone)
    p.ddl |= if a.in_tx || b.in_tx { a.ddl } else { a.removes };
    // a table dropped in the transaction: what this level wrote to it reads 0 (pgstat_drop_relation)
    if a.removes
        && a.ok
        && a.in_tx
        && let Some(level) = p.levels.last_mut()
        && !level.is_empty()
        && let Ok(names) = super::relation_names(core, world)
    {
        let names: HashSet<String> = names.into_iter().collect();
        for (n, t) in level.iter_mut() {
            if !names.contains(n) && !temp(n.as_str()) {
                t.empty(true);
            }
        }
    }
    if a.in_tx {
        // BEGIN opened a level; SAVEPOINT another; RELEASE commits those above the one it names, and
        // ROLLBACK TO rolls back those and its own, and opens its own again
        let want = 1 + a.saves;
        if a.rollback_to && a.ok {
            while p.levels.len() > a.saves {
                p.sub_end(false);
            }
        }
        while p.levels.len() > want {
            p.sub_end(true);
        }
        while p.levels.len() < want {
            p.levels.push(HashMap::new());
        }
        return;
    }
    // the transaction ended: COMMIT of one that failed rolls it back, as ROLLBACK does
    let commit = a.ok && !a.rollback && !(b.in_tx && b.failed);
    p.end_xact(commit);
    if commit {
        p.db.commits += 1;
    } else {
        p.db.rollbacks += 1;
    }
    flush(p, core, home);
    // its snapshot of the statistics ends with it
    p.snap = Snap::default();
    if std::mem::take(&mut p.ddl) {
        drop(st);
        reconcile(core, home, temp);
    }
}

/// The session ends with its transaction open: it rolls back.
pub(super) fn closed(state: &Mutex<SessState>, core: &crate::Core, home: &str) {
    let Ok(mut st) = state.lock() else { return };
    let p = &mut st.stats;
    if !p.levels.is_empty() {
        p.end_xact(false);
        p.db.rollbacks += 1;
    }
    flush(p, core, home);
    p.snap = Snap::default();
}

/// Adds the session's pending counts to database `home`'s (pgstat_relation_flush_cb and the others).
fn flush(p: &mut Pending, core: &crate::Core, home: &str) {
    if p.tables.is_empty() && p.indexes.is_empty() && p.funcs.is_empty() && p.db.commits + p.db.rollbacks == 0 {
        return;
    }
    let now = time::real_now();
    let mut all = clusters();
    let d = all.entry(core.uid).or_default().dbs.entry(home.to_string()).or_default();
    for (name, c) in p.tables.drain() {
        let e = d.tables.entry(name).or_default();
        e.numscans += c.numscans;
        if c.numscans > 0 && now > e.lastscan {
            e.lastscan = now;
        }
        e.returned += c.returned;
        e.fetched += c.fetched;
        e.inserted += c.inserted;
        e.updated += c.updated;
        e.deleted += c.deleted;
        if c.truncdropped {
            (e.live, e.dead, e.ins_since_vacuum) = (0, 0, 0);
        }
        e.live = (e.live + c.live).max(0);
        e.dead = (e.dead + c.dead).max(0);
        e.changed += c.changed;
        e.ins_since_vacuum += c.inserted;
        d.db.returned += c.returned;
        d.db.fetched += c.fetched;
        d.db.inserted += c.inserted;
        d.db.updated += c.updated;
        d.db.deleted += c.deleted;
    }
    for (key, c) in p.indexes.drain() {
        let e = d.indexes.entry(key).or_default();
        e.numscans += c.numscans;
        if c.numscans > 0 && now > e.lastscan {
            e.lastscan = now;
        }
        e.returned += c.returned;
        e.fetched += c.fetched;
        d.db.returned += c.returned;
        d.db.fetched += c.fetched;
    }
    for (oid, c) in p.funcs.drain() {
        let e = d.funcs.entry(oid).or_default();
        e.calls += c.calls;
        e.total_us += c.total_us;
        e.self_us += c.self_us;
    }
    d.db.commits += p.db.commits;
    d.db.rollbacks += p.db.rollbacks;
    p.db = DbCounts::default();
}

/// Drops the statistics of objects database `home` no longer has: tables, indexes and functions dropped
/// (or renamed) by a transaction that committed, or made by one that rolled back (Postgres drops them by
/// oid as the transaction ends).
fn reconcile(core: &crate::Core, home: &str, temp: &dyn Fn(&str) -> bool) {
    let Ok(names) = super::relation_names(core, home) else { return };
    let names: HashSet<String> = names.into_iter().collect();
    let (indexed, funcs): (HashSet<String>, bool) = {
        let all = clusters();
        let Some(d) = all.get(&core.uid).and_then(|c| c.dbs.get(home)) else { return };
        (d.indexes.keys().map(|(t, _)| t.clone()).collect(), !d.funcs.is_empty())
    };
    let Ok(v) = core.view_tx(home) else { return };
    // each indexed table's indexes, by name (its primary key's and its unique constraints' too)
    let mut indexes: HashMap<String, HashSet<String>> = HashMap::new();
    for t in indexed.iter().filter(|t| names.contains(*t)) {
        if let Ok(Some(t)) = super::load(&v, t) {
            let mut have: HashSet<String> = t.indexes.iter().map(|x| x.name.clone()).collect();
            have.extend(t.uniques.iter().map(|u| u.name.clone()));
            if t.has_key() {
                have.insert(t.pkey_name());
            }
            indexes.insert(t.name.clone(), have);
        }
    }
    let oids: Option<HashSet<i64>> = funcs.then(|| {
        let mut fs = vec![];
        if let Ok(rows) = v.prefix(&format!("{}/", super::functions::FUNCTIONS)) {
            for r in rows.flatten() {
                if let Ok(f) = super::functions::Function::from_raw(&r.1) {
                    fs.push(f);
                }
            }
        }
        super::catalog_objects::func_oids(&fs).into_iter().collect()
    });
    let mut all = clusters();
    let Some(d) = all.get_mut(&core.uid).and_then(|c| c.dbs.get_mut(home)) else { return };
    d.tables.retain(|n, _| names.contains(n) || temp(n.as_str()));
    d.indexes.retain(|(t, x), _| indexes.get(t).is_some_and(|h| h.contains(x)) || temp(t.as_str()));
    if let Some(oids) = oids {
        d.funcs.retain(|o, _| oids.contains(o));
    }
}

// ---------------------------------------------------------------- reading

/// The statistics as the session's reads see them now: as they are (`none`), or from the snapshot its
/// transaction took (`snapshot`, all at once; `cache`, as they were at its first read).
fn seen(ctx: &Ctx) -> Arc<Seen> {
    let Ok(mut st) = ctx.state.lock() else { return Arc::new(build(&ctx.core, &ctx.home)) };
    let mode = consistency(&st);
    let p = &mut st.stats;
    p.prep();
    match mode {
        Mode::None => {
            (p.snap.mode, p.snap.seen) = (Mode::None, None);
            Arc::new(build(&ctx.core, &ctx.home))
        }
        Mode::Snapshot => match (&p.snap.seen, p.snap.mode) {
            (Some(s), Mode::Snapshot) => s.clone(),
            _ => {
                let s = Arc::new(build(&ctx.core, &ctx.home));
                p.snap = Snap { mode: Mode::Snapshot, at: time::real_now(), seen: Some(s.clone()), clear: false };
                s
            }
        },
        Mode::Cache => match &p.snap.seen {
            Some(s) => s.clone(),
            None => {
                let s = Arc::new(build(&ctx.core, &ctx.home));
                (p.snap.mode, p.snap.seen) = (Mode::Cache, Some(s.clone()));
                s
            }
        },
    }
}

/// The session's pending counts, as they are now.
fn mine(ctx: &Ctx) -> Pending {
    let Ok(st) = ctx.state.lock() else { return Pending::default() };
    let mut p = st.stats.clone();
    p.snap = Snap::default();
    p
}

/// What an oid names among the objects statistics are kept of.
enum Key {
    Table(String),
    Index(String, String),
}

/// The relation (a table, or an index) `oid` names: in the running statement's world, else (one dropped in
/// it since) among those the statistics know, by the oid its name has.
fn rel_key(oid: i64, seen: &Seen, p: &Pending) -> Option<Key> {
    if let Some((t, x)) = super::catalog::stored_rel(oid) {
        return Some(match x {
            Some(x) => Key::Index(t, x),
            None => Key::Table(t),
        });
    }
    let tables = seen.here.tables.keys().chain(p.tables.keys()).chain(p.levels.iter().flat_map(|l| l.keys()));
    for n in tables {
        if super::catalog::lone_rel_oid(n) == oid {
            return Some(Key::Table(n.clone()));
        }
    }
    for (t, x) in seen.here.indexes.keys().chain(p.indexes.keys()) {
        if super::catalog::lone_index_oid(t, x) == oid {
            return Some(Key::Index(t.clone(), x.clone()));
        }
    }
    None
}

/// An oid argument: an integer, a reg value, or digits.
fn oid_of(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        Value::Int(o) | Value::Reg(o, _) => Some(*o),
        v => v.to_text().trim().parse().ok(),
    }
}

/// The world database oid `oid` is (16384 up, as pg_database numbers them), 0 for the shared objects'.
fn world_of(oid: i64) -> Option<String> {
    let cat = super::catalog::current_catalog().ok()?;
    usize::try_from(oid - 16384).ok().and_then(|i| cat.worlds.get(i)).cloned()
}

/// The oid of world `home`, as pg_database numbers it.
fn world_oid(cat: &Catalog, home: &str) -> Option<i64> {
    cat.worlds.iter().position(|w| w == home).map(|k| 16384 + k as i64)
}

/// The shared catalog tables' oids (IsSharedRelation): their statistics are datid 0's.
fn shared_rel(oid: i64) -> bool {
    matches!(oid, 1260 | 1261 | 1262 | 1213 | 1214 | 2396 | 2964 | 3592 | 6000 | 6100 | 6243)
}

const B: Type = Type::BigInt;
const F: Type = Type::Float;
const TS: Type = Type::TimestampTz;
const V: Type = Type::Void;

/// The statistics functions: name, how many arguments, what they give.
const FUNCS: &[(&str, usize, Type)] = &[
    ("pg_stat_get_snapshot_timestamp", 0, TS),
    ("pg_stat_clear_snapshot", 0, V),
    ("pg_stat_force_next_flush", 0, V),
    ("pg_stat_reset", 0, V),
    ("pg_stat_reset_shared", 1, V),
    ("pg_stat_reset_single_table_counters", 1, V),
    ("pg_stat_reset_single_function_counters", 1, V),
    ("pg_stat_reset_slru", 1, V),
    ("pg_stat_reset_replication_slot", 1, V),
    ("pg_stat_reset_subscription_stats", 1, V),
    ("pg_stat_have_stats", 3, Type::Bool),
    ("pg_stat_get_replication_slot", 1, Type::Text),
    ("pg_stat_get_subscription_stats", 1, Type::Text),
    ("pg_stat_get_numscans", 1, B),
    ("pg_stat_get_tuples_returned", 1, B),
    ("pg_stat_get_tuples_fetched", 1, B),
    ("pg_stat_get_tuples_inserted", 1, B),
    ("pg_stat_get_tuples_updated", 1, B),
    ("pg_stat_get_tuples_deleted", 1, B),
    ("pg_stat_get_tuples_hot_updated", 1, B),
    ("pg_stat_get_tuples_newpage_updated", 1, B),
    ("pg_stat_get_live_tuples", 1, B),
    ("pg_stat_get_dead_tuples", 1, B),
    ("pg_stat_get_mod_since_analyze", 1, B),
    ("pg_stat_get_ins_since_vacuum", 1, B),
    ("pg_stat_get_blocks_fetched", 1, B),
    ("pg_stat_get_blocks_hit", 1, B),
    ("pg_stat_get_vacuum_count", 1, B),
    ("pg_stat_get_autovacuum_count", 1, B),
    ("pg_stat_get_analyze_count", 1, B),
    ("pg_stat_get_autoanalyze_count", 1, B),
    ("pg_stat_get_lastscan", 1, TS),
    ("pg_stat_get_last_vacuum_time", 1, TS),
    ("pg_stat_get_last_autovacuum_time", 1, TS),
    ("pg_stat_get_last_analyze_time", 1, TS),
    ("pg_stat_get_last_autoanalyze_time", 1, TS),
    ("pg_stat_get_xact_numscans", 1, B),
    ("pg_stat_get_xact_tuples_returned", 1, B),
    ("pg_stat_get_xact_tuples_fetched", 1, B),
    ("pg_stat_get_xact_tuples_inserted", 1, B),
    ("pg_stat_get_xact_tuples_updated", 1, B),
    ("pg_stat_get_xact_tuples_deleted", 1, B),
    ("pg_stat_get_xact_tuples_hot_updated", 1, B),
    ("pg_stat_get_xact_tuples_newpage_updated", 1, B),
    ("pg_stat_get_xact_blocks_fetched", 1, B),
    ("pg_stat_get_xact_blocks_hit", 1, B),
    ("pg_stat_get_function_calls", 1, B),
    ("pg_stat_get_function_total_time", 1, F),
    ("pg_stat_get_function_self_time", 1, F),
    ("pg_stat_get_xact_function_calls", 1, B),
    ("pg_stat_get_xact_function_total_time", 1, F),
    ("pg_stat_get_xact_function_self_time", 1, F),
    ("pg_stat_get_db_numbackends", 1, Type::Int),
    ("pg_stat_get_db_xact_commit", 1, B),
    ("pg_stat_get_db_xact_rollback", 1, B),
    ("pg_stat_get_db_blocks_fetched", 1, B),
    ("pg_stat_get_db_blocks_hit", 1, B),
    ("pg_stat_get_db_tuples_returned", 1, B),
    ("pg_stat_get_db_tuples_fetched", 1, B),
    ("pg_stat_get_db_tuples_inserted", 1, B),
    ("pg_stat_get_db_tuples_updated", 1, B),
    ("pg_stat_get_db_tuples_deleted", 1, B),
    ("pg_stat_get_db_conflict_tablespace", 1, B),
    ("pg_stat_get_db_conflict_lock", 1, B),
    ("pg_stat_get_db_conflict_snapshot", 1, B),
    ("pg_stat_get_db_conflict_bufferpin", 1, B),
    ("pg_stat_get_db_conflict_startup_deadlock", 1, B),
    ("pg_stat_get_db_conflict_logicalslot", 1, B),
    ("pg_stat_get_db_conflict_all", 1, B),
    ("pg_stat_get_db_deadlocks", 1, B),
    ("pg_stat_get_db_temp_files", 1, B),
    ("pg_stat_get_db_temp_bytes", 1, B),
    ("pg_stat_get_db_sessions", 1, B),
    ("pg_stat_get_db_sessions_abandoned", 1, B),
    ("pg_stat_get_db_sessions_fatal", 1, B),
    ("pg_stat_get_db_sessions_killed", 1, B),
    ("pg_stat_get_db_checksum_failures", 1, B),
    ("pg_stat_get_db_checksum_last_failure", 1, TS),
    ("pg_stat_get_db_blk_read_time", 1, F),
    ("pg_stat_get_db_blk_write_time", 1, F),
    ("pg_stat_get_db_session_time", 1, F),
    ("pg_stat_get_db_active_time", 1, F),
    ("pg_stat_get_db_idle_in_transaction_time", 1, F),
    ("pg_stat_get_db_stat_reset_time", 1, TS),
    ("pg_stat_get_bgwriter_timed_checkpoints", 0, B),
    ("pg_stat_get_bgwriter_requested_checkpoints", 0, B),
    ("pg_stat_get_bgwriter_buf_written_checkpoints", 0, B),
    ("pg_stat_get_bgwriter_buf_written_clean", 0, B),
    ("pg_stat_get_bgwriter_maxwritten_clean", 0, B),
    ("pg_stat_get_buf_written_backend", 0, B),
    ("pg_stat_get_buf_fsync_backend", 0, B),
    ("pg_stat_get_buf_alloc", 0, B),
    ("pg_stat_get_checkpoint_write_time", 0, F),
    ("pg_stat_get_checkpoint_sync_time", 0, F),
    ("pg_stat_get_bgwriter_stat_reset_time", 0, TS),
];

/// The statistics function a name calls, and how many arguments it takes.
pub(super) fn lookup(name: &str) -> Option<(&'static str, std::ops::RangeInclusive<usize>)> {
    FUNCS.iter().find(|(n, ..)| *n == name).map(|(n, k, _)| (*n, *k..=*k))
}

/// The type a statistics function gives.
pub(super) fn result(name: &str) -> Type {
    FUNCS.iter().find(|(n, ..)| *n == name).map_or(Type::Text, |(_, _, t)| *t)
}

/// The functions initdb revokes EXECUTE on from PUBLIC: a superuser's here.
fn privileged(name: &str) -> Result<()> {
    if super::roles::checked().is_some() {
        return err("42501", format!("permission denied for function {name}"));
    }
    Ok(())
}

/// A call of statistics function `name` (see `lookup`) with arguments `a`.
pub(super) fn call(ctx: &Ctx, name: &'static str, a: &[Value]) -> Result<Value> {
    use Value::{Bool, Float, Int, Null};
    let void = || Ok(Value::Text(String::new()));
    let ts = |t: i64| if t == 0 { Null } else { Value::TimestampTz(t) };
    // (all are strict but the ones that reset what a NULL argument says is everything)
    let lenient =
        matches!(name, "pg_stat_reset_slru" | "pg_stat_reset_replication_slot" | "pg_stat_reset_subscription_stats");
    if !lenient && a.contains(&Null) {
        return Ok(Null);
    }
    if name.starts_with("pg_stat_reset") || name == "pg_stat_have_stats" {
        privileged(name)?;
    }
    match name {
        "pg_stat_get_snapshot_timestamp" => {
            let Ok(mut st) = ctx.state.lock() else { return Ok(Null) };
            st.stats.prep();
            let s = &st.stats.snap;
            return Ok(if s.mode == Mode::Snapshot { Value::TimestampTz(s.at) } else { Null });
        }
        "pg_stat_clear_snapshot" => {
            if let Ok(mut st) = ctx.state.lock() {
                st.stats.snap = Snap::default();
            }
            return void();
        }
        // (the counts are added in at every transaction's end: there is nothing to hurry)
        "pg_stat_force_next_flush" => return void(),
        "pg_stat_have_stats" => return have_stats(ctx, a).map(Bool),
        // no replication slot or subscription has statistics: an unknown one's are zeros, as in Postgres
        "pg_stat_get_replication_slot" => {
            let mut vals = vec![a[0].clone()];
            vals.extend(std::iter::repeat_n(Int(0), 8));
            vals.push(Null);
            return Ok(Value::Text(super::composite_text(&vals)));
        }
        "pg_stat_get_subscription_stats" => {
            return Ok(Value::Text(super::composite_text(&[a[0].clone(), Int(0), Int(0), Null])));
        }
        _ => {}
    }
    if name.starts_with("pg_stat_reset") {
        return reset(ctx, name, a).and_then(|_| void());
    }
    if let Some(stat) = name.strip_prefix("pg_stat_get_xact_function_") {
        let mine = mine(ctx);
        let Some(c) = oid_of(&a[0]).and_then(|o| mine.funcs.get(&o).copied()) else { return Ok(Null) };
        return Ok(match stat {
            "calls" => Int(c.calls),
            "total_time" => Float(c.total_us as f64 / 1000.0),
            _ => Float(c.self_us as f64 / 1000.0),
        });
    }
    if let Some(stat) = name.strip_prefix("pg_stat_get_function_") {
        let seen = seen(ctx);
        let Some(c) = oid_of(&a[0]).and_then(|o| seen.here.funcs.get(&o).copied()) else { return Ok(Null) };
        return Ok(match stat {
            "calls" => Int(c.calls),
            "total_time" => Float(c.total_us as f64 / 1000.0),
            _ => Float(c.self_us as f64 / 1000.0),
        });
    }
    if let Some(stat) = name.strip_prefix("pg_stat_get_xact_") {
        let mine = mine(ctx);
        let Some(o) = oid_of(&a[0]) else { return Ok(Null) };
        let seen = build(&ctx.core, &ctx.home);
        let c = match rel_key(o, &seen, &mine) {
            Some(Key::Table(t)) => {
                let mut c = mine.tables.get(&t).copied().unwrap_or_default();
                (c.inserted, c.updated, c.deleted) = mine.written(&t);
                c
            }
            Some(Key::Index(t, x)) => mine.indexes.get(&(t, x)).copied().unwrap_or_default(),
            None => Counts::default(),
        };
        return Ok(Int(match stat {
            "numscans" => c.numscans,
            "tuples_returned" => c.returned,
            "tuples_fetched" => c.fetched,
            "tuples_inserted" => c.inserted,
            "tuples_updated" => c.updated,
            "tuples_deleted" => c.deleted,
            // (no update is HOT, nor moves a row to a new page; there are no blocks)
            _ => 0,
        }));
    }
    if let Some(stat) = name.strip_prefix("pg_stat_get_db_") {
        return db_call(ctx, stat, &a[0]);
    }
    if name.starts_with("pg_stat_get_bgwriter_")
        || name.starts_with("pg_stat_get_buf_")
        || name.starts_with("pg_stat_get_checkpoint_")
    {
        let seen = seen(ctx);
        return Ok(match name {
            "pg_stat_get_bgwriter_requested_checkpoints" => Int(seen.fixed.requested),
            "pg_stat_get_bgwriter_stat_reset_time" => ts(seen.fixed.bgwriter),
            "pg_stat_get_checkpoint_write_time" | "pg_stat_get_checkpoint_sync_time" => Float(0.0),
            _ => Int(0),
        });
    }
    // a table's or an index's
    let seen = seen(ctx);
    let mine = mine(ctx);
    let Some(o) = oid_of(&a[0]) else { return Ok(Null) };
    let c = match rel_key(o, &seen, &mine) {
        Some(Key::Table(t)) => seen.here.tables.get(&t).copied(),
        Some(Key::Index(t, x)) => seen.here.indexes.get(&(t, x)).copied(),
        None => None,
    }
    .unwrap_or_default();
    Ok(match name.trim_start_matches("pg_stat_get_") {
        "numscans" => Int(c.numscans),
        "tuples_returned" => Int(c.returned),
        "tuples_fetched" => Int(c.fetched),
        "tuples_inserted" => Int(c.inserted),
        "tuples_updated" => Int(c.updated),
        "tuples_deleted" => Int(c.deleted),
        "live_tuples" => Int(c.live),
        "dead_tuples" => Int(c.dead),
        "mod_since_analyze" => Int(c.changed),
        "ins_since_vacuum" => Int(c.ins_since_vacuum),
        "lastscan" => ts(c.lastscan),
        // never vacuumed or analyzed
        "last_vacuum_time" | "last_autovacuum_time" | "last_analyze_time" | "last_autoanalyze_time" => Null,
        _ => Int(0),
    })
}

/// pg_stat_get_db_*(oid): a database's counters (0 for none counted, as in Postgres).
fn db_call(ctx: &Ctx, stat: &str, arg: &Value) -> Result<Value> {
    use Value::{Float, Int, Null};
    let Some(o) = oid_of(arg) else { return Ok(Null) };
    let seen = seen(ctx);
    let world = world_of(o);
    let c = match (o, &world) {
        (0, _) => seen.shared,
        (_, Some(w)) => seen.dbs.get(w).copied().unwrap_or_default(),
        _ => DbCounts::default(),
    };
    Ok(match stat {
        "numbackends" => {
            let n = world.map_or(0, |w| super::backends().iter().filter(|(_, b)| b.world == w).count());
            Int(n as i64)
        }
        "xact_commit" => Int(c.commits),
        "xact_rollback" => Int(c.rollbacks),
        "tuples_returned" => Int(c.returned),
        "tuples_fetched" => Int(c.fetched),
        "tuples_inserted" => Int(c.inserted),
        "tuples_updated" => Int(c.updated),
        "tuples_deleted" => Int(c.deleted),
        "sessions" => Int(c.sessions),
        // (data checksums are off: no failures are counted)
        "checksum_failures" | "checksum_last_failure" => Null,
        "stat_reset_time" if c.reset == 0 => Null,
        "stat_reset_time" => Value::TimestampTz(c.reset),
        "blk_read_time" | "blk_write_time" | "session_time" | "active_time" | "idle_in_transaction_time" => Float(0.0),
        _ => Int(0),
    })
}

/// pg_stat_reset() and its kin.
fn reset(ctx: &Ctx, name: &str, a: &[Value]) -> Result<()> {
    let now = time::real_now();
    match name {
        // every counter of this database's
        "pg_stat_reset" => {
            let mut all = clusters();
            let d = all.entry(ctx.core.uid).or_default().dbs.entry(ctx.home.clone()).or_default();
            d.tables.values_mut().for_each(|c| *c = Counts::default());
            d.indexes.values_mut().for_each(|c| *c = Counts::default());
            d.funcs.values_mut().for_each(|c| *c = Calls::default());
            d.db = DbCounts { reset: now, ..DbCounts::default() };
        }
        "pg_stat_reset_shared" => {
            let target = a[0].to_text();
            let mut all = clusters();
            let f = &mut all.entry(ctx.core.uid).or_default().fixed;
            match target.as_str() {
                "archiver" => f.archiver = now,
                // (the checkpointer's are reset with the background writer's, as Postgres 16 does)
                "bgwriter" => (f.bgwriter, f.requested) = (now, 0),
                "io" => f.io = now,
                "recovery_prefetch" => f.prefetch = now,
                "wal" => f.wal = now,
                _ => {
                    return Err(SqlError {
                        code: "22023",
                        message: format!("unrecognized reset target: \"{target}\""),
                        info: Some(Box::new(Violation {
                            hint: Some(
                                "Target must be \"archiver\", \"bgwriter\", \"io\", \"recovery_prefetch\", or \"wal\"."
                                    .into(),
                            ),
                            ..Violation::default()
                        })),
                    });
                }
            }
        }
        "pg_stat_reset_slru" => {
            let mut all = clusters();
            let f = &mut all.entry(ctx.core.uid).or_default().fixed;
            match &a[0] {
                Value::Null => f.slru = [now; 8],
                // (a name that's no SLRU's is "other"'s, as pgstat_get_slru_index has it)
                v => {
                    let n = v.to_text();
                    let i = SLRUS.iter().position(|s| *s == n).unwrap_or(SLRUS.len() - 1);
                    f.slru[i] = now;
                }
            }
        }
        "pg_stat_reset_single_table_counters" => {
            let o = oid_of(&a[0]).unwrap_or(0);
            let key = rel_key(o, &build(&ctx.core, &ctx.home), &mine(ctx));
            let mut all = clusters();
            let cl = all.entry(ctx.core.uid).or_default();
            if shared_rel(o) {
                cl.shared.reset = now;
                return Ok(());
            }
            let d = cl.dbs.entry(ctx.home.clone()).or_default();
            match key {
                Some(Key::Table(t)) => {
                    if let Some(c) = d.tables.get_mut(&t) {
                        *c = Counts::default();
                    }
                }
                Some(Key::Index(t, x)) => {
                    if let Some(c) = d.indexes.get_mut(&(t, x)) {
                        *c = Counts::default();
                    }
                }
                None => {}
            }
            d.db.reset = now;
        }
        "pg_stat_reset_single_function_counters" => {
            let o = oid_of(&a[0]).unwrap_or(0);
            let mut all = clusters();
            let d = all.entry(ctx.core.uid).or_default().dbs.entry(ctx.home.clone()).or_default();
            if let Some(c) = d.funcs.get_mut(&o) {
                *c = Calls::default();
            }
            d.db.reset = now;
        }
        "pg_stat_reset_replication_slot" => {
            if a[0] != Value::Null {
                return err("42704", format!("replication slot \"{}\" does not exist", a[0].to_text()));
            }
        }
        // pg_stat_reset_subscription_stats
        _ => {
            if oid_of(&a[0]) == Some(0) {
                return err("22023", "invalid subscription OID 0");
            }
        }
    }
    Ok(())
}

/// pg_stat_have_stats(kind, database oid, object oid): whether statistics are kept of the object.
fn have_stats(ctx: &Ctx, a: &[Value]) -> Result<bool> {
    let kind = a[0].to_text();
    let (db, obj) = (oid_of(&a[1]).unwrap_or(-1), oid_of(&a[2]).unwrap_or(-1));
    let here = super::catalog::current_catalog().ok().and_then(|c| world_oid(&c, &ctx.home));
    Ok(match kind.to_lowercase().as_str() {
        // the fixed-numbered ones always are
        "archiver" | "bgwriter" | "checkpointer" | "io" | "slru" | "wal" => true,
        "database" => obj == 0 && world_of(db).is_some(),
        "relation" => {
            let seen = build(&ctx.core, &ctx.home);
            let mine = mine(ctx);
            Some(db) == here
                && match rel_key(obj, &seen, &mine) {
                    Some(Key::Table(t)) => seen.here.tables.contains_key(&t) || mine.has_table(&t),
                    Some(Key::Index(t, x)) => {
                        let k = (t, x);
                        seen.here.indexes.contains_key(&k) || mine.indexes.contains_key(&k)
                    }
                    None => false,
                }
        }
        "function" => {
            let seen = build(&ctx.core, &ctx.home);
            Some(db) == here && (seen.here.funcs.contains_key(&obj) || mine(ctx).funcs.contains_key(&obj))
        }
        "replslot" | "subscription" => false,
        _ => return err("22023", format!("invalid statistics kind: \"{kind}\"")),
    })
}

// ---------------------------------------------------------------- the views

/// Whether `name` is one of the statistics views made here.
pub(super) fn view(name: &str) -> bool {
    matches!(
        name,
        "pg_stat_all_tables"
            | "pg_stat_user_tables"
            | "pg_stat_xact_all_tables"
            | "pg_stat_xact_user_tables"
            | "pg_statio_all_tables"
            | "pg_statio_user_tables"
            | "pg_stat_all_indexes"
            | "pg_stat_user_indexes"
            | "pg_statio_all_indexes"
            | "pg_statio_user_indexes"
            | "pg_statio_all_sequences"
            | "pg_statio_user_sequences"
            | "pg_stat_user_functions"
            | "pg_stat_xact_user_functions"
            | "pg_stat_database"
            | "pg_stat_bgwriter"
            | "pg_stat_wal"
            | "pg_stat_archiver"
            | "pg_stat_recovery_prefetch"
            | "pg_stat_slru"
            | "pg_stat_io"
    )
}

/// The rows of statistics view `name` (see `view`) for the world `cat` is of, in its columns' order.
pub(super) fn view_rows(name: &str, cat: &Catalog) -> Vec<Vec<Value>> {
    use Value::{Float, Int, Null};
    let ctx = session();
    let seen = ctx.as_deref().map_or_else(|| Arc::new(Seen::default()), seen);
    let mine = ctx.as_deref().map(mine).unwrap_or_default();
    let cols = super::catalog_schema::columns(name);
    let t = |s: &str| Value::Text(s.into());
    let ts = |at: i64| if at == 0 { Null } else { Value::TimestampTz(at) };
    let row = |f: &dyn Fn(&str) -> Value| cols.iter().map(|(c, _)| f(c)).collect::<Vec<Value>>();
    match name {
        "pg_stat_all_tables"
        | "pg_stat_user_tables"
        | "pg_stat_xact_all_tables"
        | "pg_stat_xact_user_tables"
        | "pg_statio_all_tables"
        | "pg_statio_user_tables" => {
            let xact = name.starts_with("pg_stat_xact");
            cat.rels
                .iter()
                .enumerate()
                .filter(|(_, r)| matches!(r.kind, 'r' | 'm' | 'p'))
                .map(|(i, r)| {
                    let keys: Vec<(String, String)> = cat
                        .idx
                        .iter()
                        .filter(|(j, _)| *j == i)
                        .map(|(_, x)| (r.name.clone(), x.name.clone()))
                        .collect();
                    let indexed = !keys.is_empty();
                    let (c, ix): (Counts, Vec<Counts>) = if xact {
                        let mut c = mine.tables.get(&r.name).copied().unwrap_or_default();
                        (c.inserted, c.updated, c.deleted) = mine.written(&r.name);
                        (c, keys.iter().map(|k| mine.indexes.get(k).copied().unwrap_or_default()).collect())
                    } else {
                        (
                            seen.here.tables.get(&r.name).copied().unwrap_or_default(),
                            keys.iter().map(|k| seen.here.indexes.get(k).copied().unwrap_or_default()).collect(),
                        )
                    };
                    // (a table's index columns are its indexes' sums: null for a table with none)
                    let over = |f: &dyn Fn(&Counts) -> i64| if indexed { Int(ix.iter().map(f).sum()) } else { Null };
                    row(&|col| match col {
                        "relid" => Int(cat.rel_oid(i)),
                        "schemaname" => t(schema_part(&r.name)),
                        "relname" => t(rel_part(&r.name)),
                        "seq_scan" => Int(c.numscans),
                        "last_seq_scan" => ts(c.lastscan),
                        "seq_tup_read" => Int(c.returned),
                        "idx_scan" => over(&|x| x.numscans),
                        "last_idx_scan" => ts(ix.iter().map(|x| x.lastscan).max().unwrap_or(0)),
                        "idx_tup_fetch" if indexed => Int(ix.iter().map(|x| x.fetched).sum::<i64>() + c.fetched),
                        "idx_tup_fetch" => Null,
                        "n_tup_ins" => Int(c.inserted),
                        "n_tup_upd" => Int(c.updated),
                        "n_tup_del" => Int(c.deleted),
                        "n_live_tup" => Int(c.live),
                        "n_dead_tup" => Int(c.dead),
                        "n_mod_since_analyze" => Int(c.changed),
                        "n_ins_since_vacuum" => Int(c.ins_since_vacuum),
                        // (no pages, so no blocks read or hit; no TOAST tables)
                        "idx_blks_read" | "idx_blks_hit" => over(&|_| 0),
                        "toast_blks_read" | "toast_blks_hit" | "tidx_blks_read" | "tidx_blks_hit" => Null,
                        "last_vacuum" | "last_autovacuum" | "last_analyze" | "last_autoanalyze" => Null,
                        _ => Int(0),
                    })
                })
                .collect()
        }
        "pg_stat_all_indexes" | "pg_stat_user_indexes" | "pg_statio_all_indexes" | "pg_statio_user_indexes" => cat
            .idx
            .iter()
            .enumerate()
            .filter(|(_, (i, _))| matches!(cat.rels[*i].kind, 'r' | 'm'))
            .map(|(k, (i, x))| {
                let r = &cat.rels[*i];
                let c = seen.here.indexes.get(&(r.name.clone(), x.name.clone())).copied().unwrap_or_default();
                row(&|col| match col {
                    "relid" => Int(cat.rel_oid(*i)),
                    "indexrelid" => Int(cat.idx_oid(k)),
                    "schemaname" => t(schema_part(&r.name)),
                    "relname" => t(rel_part(&r.name)),
                    "indexrelname" => t(x.name.as_str()),
                    "idx_scan" => Int(c.numscans),
                    "last_idx_scan" => ts(c.lastscan),
                    "idx_tup_read" => Int(c.returned),
                    "idx_tup_fetch" => Int(c.fetched),
                    _ => Int(0),
                })
            })
            .collect(),
        "pg_statio_all_sequences" | "pg_statio_user_sequences" => cat
            .rels
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == 'S')
            .map(|(i, r)| {
                row(&|col| match col {
                    "relid" => Int(cat.rel_oid(i)),
                    "schemaname" => t(schema_part(&r.name)),
                    "relname" => t(rel_part(&r.name)),
                    _ => Int(0),
                })
            })
            .collect(),
        // the functions with calls counted (the transaction's own, for the xact view)
        "pg_stat_user_functions" | "pg_stat_xact_user_functions" => {
            let calls = if name == "pg_stat_user_functions" { &seen.here.funcs } else { &mine.funcs };
            cat.funcs
                .iter()
                .zip(&cat.func_oids)
                .filter_map(|(f, o)| calls.get(o).map(|c| (f, *o, *c)))
                .map(|(f, o, c)| {
                    row(&|col| match col {
                        "funcid" => Int(o),
                        "schemaname" => t(schema_part(&f.name)),
                        "funcname" => t(rel_part(&f.name)),
                        "calls" => Int(c.calls),
                        "total_time" => Float(c.total_us as f64 / 1000.0),
                        "self_time" => Float(c.self_us as f64 / 1000.0),
                        _ => Null,
                    })
                })
                .collect()
        }
        // the shared objects' row (datid 0), then each database's, as pg_database lists them
        "pg_stat_database" => {
            let open = super::backends();
            let mut dbs: Vec<(i64, Option<String>)> = vec![(0, None)];
            dbs.extend(
                [("template1", 1), ("template0", 4), ("postgres", 5)]
                    .into_iter()
                    .filter(|(n, _)| !cat.worlds.iter().any(|w| w == n))
                    .map(|(n, o)| (o, Some(n.to_string()))),
            );
            dbs.extend(cat.worlds.iter().enumerate().map(|(k, w)| (16384 + k as i64, Some(w.clone()))));
            dbs.into_iter()
                .map(|(oid, w)| {
                    let c = match &w {
                        None => seen.shared,
                        Some(w) => seen.dbs.get(w).copied().unwrap_or_default(),
                    };
                    let n = w.as_ref().map_or(0, |w| open.iter().filter(|(_, b)| b.world == *w).count());
                    row(&|col| match col {
                        "datid" => Int(oid),
                        "datname" => w.as_deref().map_or(Null, t),
                        "numbackends" => Int(n as i64),
                        "xact_commit" => Int(c.commits),
                        "xact_rollback" => Int(c.rollbacks),
                        "tup_returned" => Int(c.returned),
                        "tup_fetched" => Int(c.fetched),
                        "tup_inserted" => Int(c.inserted),
                        "tup_updated" => Int(c.updated),
                        "tup_deleted" => Int(c.deleted),
                        "sessions" => Int(c.sessions),
                        // (data checksums are off)
                        "checksum_failures" | "checksum_last_failure" => Null,
                        "stats_reset" => ts(c.reset),
                        "blk_read_time"
                        | "blk_write_time"
                        | "session_time"
                        | "active_time"
                        | "idle_in_transaction_time" => Float(0.0),
                        _ => Int(0),
                    })
                })
                .collect()
        }
        "pg_stat_bgwriter" => vec![row(&|col| match col {
            "checkpoints_req" => Int(seen.fixed.requested),
            "checkpoint_write_time" | "checkpoint_sync_time" => Float(0.0),
            "stats_reset" => ts(seen.fixed.bgwriter),
            _ => Int(0),
        })],
        "pg_stat_wal" => vec![row(&|col| match col {
            "wal_write_time" | "wal_sync_time" => Float(0.0),
            "stats_reset" => ts(seen.fixed.wal),
            _ => Int(0),
        })],
        "pg_stat_archiver" => vec![row(&|col| match col {
            "archived_count" | "failed_count" => Int(0),
            "stats_reset" => ts(seen.fixed.archiver),
            _ => Null,
        })],
        // (never in recovery: nothing prefetched)
        "pg_stat_recovery_prefetch" => vec![row(&|col| match col {
            "stats_reset" => ts(seen.fixed.prefetch),
            _ => Int(0),
        })],
        "pg_stat_slru" => SLRUS
            .iter()
            .zip(seen.fixed.slru)
            .map(|(n, at)| {
                row(&|col| match col {
                    "name" => t(n),
                    "stats_reset" => ts(at),
                    _ => Int(0),
                })
            })
            .collect(),
        // pg_stat_io: the rows Postgres 16 shows, each I/O it doesn't track null (see `io_tracked`)
        _ => {
            let mut rows = vec![];
            for (bk, desc) in IO_BACKENDS {
                for obj in 0..2u8 {
                    for cx in 0..4u8 {
                        if !io_object(bk, obj, cx) {
                            continue;
                        }
                        let op = |o: IoOp| io_op(bk, obj, cx, o);
                        rows.push(row(&|col| {
                            let (o, timed) = match col {
                                "backend_type" => return t(desc),
                                "object" => return t(if obj == 0 { "relation" } else { "temp relation" }),
                                "context" => return t(["bulkread", "bulkwrite", "normal", "vacuum"][cx as usize]),
                                "op_bytes" => return Int(8192),
                                "stats_reset" => return ts(seen.fixed.io),
                                "reads" => (IoOp::Read, false),
                                "read_time" => (IoOp::Read, true),
                                "writes" => (IoOp::Write, false),
                                "write_time" => (IoOp::Write, true),
                                "writebacks" => (IoOp::Writeback, false),
                                "writeback_time" => (IoOp::Writeback, true),
                                "extends" => (IoOp::Extend, false),
                                "extend_time" => (IoOp::Extend, true),
                                "hits" => (IoOp::Hit, false),
                                "evictions" => (IoOp::Evict, false),
                                "reuses" => (IoOp::Reuse, false),
                                "fsyncs" => (IoOp::Fsync, false),
                                "fsync_time" => (IoOp::Fsync, true),
                                _ => return Null,
                            };
                            match (op(o), timed) {
                                (false, _) => Null,
                                (true, true) => Float(0.0),
                                (true, false) => Int(0),
                            }
                        }));
                    }
                }
            }
            rows
        }
    }
}

/// The backend types pg_stat_io has rows for (pgstat_tracks_io_bktype), by their BackendType number and as
/// it names them.
const IO_BACKENDS: [(u8, &str); 9] = [
    (2, "autovacuum launcher"),
    (3, "autovacuum worker"),
    (4, "client backend"),
    (5, "background worker"),
    (6, "background writer"),
    (7, "checkpointer"),
    (9, "standalone backend"),
    (10, "startup"),
    (12, "walsender"),
];

const AV_LAUNCHER: u8 = 2;
const AV_WORKER: u8 = 3;
const BG_WRITER: u8 = 6;
const CHECKPOINTER: u8 = 7;
const STANDALONE: u8 = 9;
const STARTUP: u8 = 10;

/// I/O contexts, by pg_stat_io's numbering.
const BULKREAD: u8 = 0;
const BULKWRITE: u8 = 1;
const NORMAL: u8 = 2;
const VACUUM: u8 = 3;

#[derive(Clone, Copy, PartialEq)]
enum IoOp {
    Evict,
    Extend,
    Fsync,
    Hit,
    Read,
    Reuse,
    Write,
    Writeback,
}

/// pgstat_tracks_io_object: whether backend type `bk` does I/O on object `obj` (0 a relation, 1 a temporary
/// one) in context `ctx`.
fn io_object(bk: u8, obj: u8, ctx: u8) -> bool {
    if ctx != NORMAL && obj == 1 {
        return false;
    }
    let no_temp = matches!(bk, AV_LAUNCHER | BG_WRITER | CHECKPOINTER | AV_WORKER | STANDALONE | STARTUP);
    if no_temp && ctx == NORMAL && obj == 1 {
        return false;
    }
    if matches!(bk, CHECKPOINTER | BG_WRITER) && matches!(ctx, BULKREAD | BULKWRITE | VACUUM) {
        return false;
    }
    if bk == AV_LAUNCHER && ctx == VACUUM {
        return false;
    }
    !(matches!(bk, AV_WORKER | AV_LAUNCHER) && ctx == BULKWRITE)
}

/// pgstat_tracks_io_op: whether that I/O is one of operation `op`'s.
fn io_op(bk: u8, obj: u8, ctx: u8, op: IoOp) -> bool {
    if !io_object(bk, obj, ctx) {
        return false;
    }
    if matches!(bk, BG_WRITER | CHECKPOINTER) && matches!(op, IoOp::Read | IoOp::Evict | IoOp::Hit) {
        return false;
    }
    if matches!(bk, AV_LAUNCHER | BG_WRITER | CHECKPOINTER) && op == IoOp::Extend {
        return false;
    }
    if obj == 1 && matches!(op, IoOp::Fsync | IoOp::Writeback) {
        return false;
    }
    if ctx == BULKREAD && op == IoOp::Extend {
        return false;
    }
    let strategy = matches!(ctx, BULKREAD | BULKWRITE | VACUUM);
    if !strategy && op == IoOp::Reuse {
        return false;
    }
    !(strategy && op == IoOp::Fsync)
}
