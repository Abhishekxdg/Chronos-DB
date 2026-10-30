//! The stale-read check: a world that checks its reads remembers what it read, and its merge
//! is held when the world it merges into has changed any of that since the fork. An agent that
//! read a row, decided on it, and wrote something else would otherwise merge a decision made on
//! state that's gone; its own writes to the row already conflict, but its reads didn't.
//!
//!   read through a world's View (get, a scan's range, a search) ─> its read set: keys, key
//!     ranges, and whole tables (a search, or past MOST_KEYS / MOST_RANGES for one table)
//!   a SQL scan of a whole table with `column op constant` conditions ─> the rows they can
//!     match (`sql::RowFilter`), not the table: rows it couldn't have seen may change
//!   a plain SELECT (no function made with CREATE FUNCTION, no row security) ─> the columns it
//!     reads of each table (`USING`), kept per table: a row changed since only in columns no read
//!     used doesn't count (an insert or a delete always does: it changes what a read finds)
//!   merge ─> each read, fork point vs the world merged into now ─> changed: held
//!
//! A hold found that way may be a false one: the rows changed, but what the agent got from them didn't (a sum that
//! came out the same, a row its read found again as it was). A world forked `WITH (rerun_reads = true)` (which checks
//! its reads too), or by an agent whose merge policy has `rerun_reads`, keeps each plain SELECT it reads through (see
//! `sql::Session::replayable`) with a hash of what it gave (at most MOST_KEPT, of MOST_ROWS rows each), and what it
//! read any other way (a statement of another kind, a row read over HTTP) apart, as `outside`. A merge that would be
//! held, for rows only the kept SELECTs read, runs them again: on the world it merges into as it is, and on the fork
//! point. Each gives what it gave then on both: not held. One gives something else now: held, the reasons saying what
//! it gave then and gives now too. One that fails, runs past TIMEOUT, or gives something else on the fork point (a read
//! of the world's own writes, of the clock, of random()): held as found. In a folder attached to Postgres they run on
//! Postgres's rows as well (see `attach::check_reads`).
//!
//!   held ─> each kept SELECT, as its session ran it ─> now: as then? ─> fork point: as then? ─> not held
//!
//! Opt-in: a world forked `WITH (check_reads = true)`, or any world an agent keeping to a merge
//! policy with `check_reads` forks (`_sonos_check_reads` in its metadata, which only the database
//! sets). A transaction's reads count for its world. The read set is kept in memory, and written
//! to the folder (`readsets`) as the database closes cleanly; the next open takes it back and
//! deletes the file. After a crash there's no file: what a world read before is unknown, and its
//! merge is held for that.
//!
//!   clean close ─> each open world's read set ─> `readsets` (sealed, if the folder is)
//!   open after a clean close ─> read sets back, complete ─> file deleted
//!   open after a crash ─> file deleted, unread: read sets incomplete, merges held

use crate::{Core, Error, Result, State};
use serde_json::{Map, Value as Json, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::path::Path;
use std::sync::Arc;

/// The columns of each table the statement running reads, by the table's name as rows are keyed
/// (see `sql`'s `read_columns`): what's read meanwhile is kept as read for those columns alone. A
/// table it doesn't name (one a view or a constraint reaches), or no statement: every column.
pub(crate) type Using = Arc<HashMap<String, BTreeSet<String>>>;

thread_local! {
    pub(crate) static USING: RefCell<Option<Using>> = const { RefCell::new(None) };
    /// A plain SELECT that is kept to run again at the merge is running (see `sql::Session::replayable`): what it
    /// reads meanwhile is read by it. Anything read while it isn't set wasn't, and can't be run again.
    pub(crate) static REPLAY: Cell<bool> = const { Cell::new(false) };
}

/// Metadata key marking a world whose reads are checked at merge.
pub(crate) const CHECK_READS: &str = "_sonos_check_reads";
/// Metadata key marking a world whose SELECTs are run again at a merge their reads would hold.
pub(crate) const RERUN_READS: &str = "_sonos_rerun_reads";
/// Keys one world remembers before a table's reads count as the whole table.
const MOST_KEYS: usize = 10_000;
/// Key ranges likewise.
const MOST_RANGES: usize = 1_000;
/// Filtered scans likewise.
const MOST_FILTERS: usize = 1_000;
/// Changed keys a reason names before it says how many more.
const SHOWN: usize = 10;
/// SELECTs one world keeps to run again at its merge; past it, its reads can't all be run again.
const MOST_KEPT: usize = 100;
/// Rows a SELECT may give to be kept.
const MOST_ROWS: usize = 10_000;
/// How long each kept SELECT may take when run again, in ms: as long as a merge check, which runs under the same locks.
const TIMEOUT: u64 = 1000;
/// How long a merge spends running them again in all, in ms; past it, held as found.
const ALL_TIMEOUT: u64 = 5000;
/// Characters of a kept SELECT a reason shows.
const TEXT_SHOWN: usize = 100;

/// A SELECT kept to run again at the merge (see the module docs): how, and what it gave.
#[derive(Clone)]
pub(crate) struct Kept {
    run: crate::sql::Replay,
    digest: [u8; 32],
    /// what it gave, as a reason shows it
    shown: String,
}

/// What a world read (see the module docs).
#[derive(Default)]
pub(crate) struct ReadSet {
    /// recorded since the fork; false when the database restarted since, so earlier reads are unknown
    pub(crate) complete: bool,
    keys: BTreeSet<String>,
    /// (prefix, after, upto): keys under prefix after `after` (exclusive) up to `upto` (inclusive)
    ranges: BTreeSet<(String, Option<String>, Option<String>)>,
    /// every key of these groups (a table's rows, `t/`, or an index's entries, `_sonos_idx/t/`)
    groups: BTreeSet<String>,
    /// a table's rows (its group) that these filters match: the rows a scan could have returned
    filters: Vec<(String, crate::sql::RowFilter)>,
    /// the filters kept, as `RowFilter::to_json` writes them with their group: a scan run again with the same
    /// conditions (an agent's loop) adds nothing, and doesn't bring its table nearer `MOST_FILTERS`
    seen: HashSet<String>,
    /// per table read (its group), the columns its reads used (None: any); a table not here, every column
    columns: BTreeMap<String, Option<BTreeSet<String>>>,
    /// its SELECTs are kept to run again at its merge (see the module docs)
    rerun: bool,
    /// the SELECTs it read through, to run again at its merge
    kept: Vec<Kept>,
    /// what it read otherwise, which can't be run again: a hold for it stays
    outside: Option<Box<ReadSet>>,
    /// a SELECT read and wasn't kept (it failed, or gave too many rows; too many were kept), or what was read before a
    /// restart isn't known: a hold can't be lifted by running the kept ones again
    loose: bool,
}

/// The group a key's read falls in: a table's rows (`orders/`) or, for the rows SQL keeps behind
/// indexes and constraints, those of one table (`_sonos_idx/orders/`); None for definitions and
/// counters, which aren't read decisions.
fn group(key: &str) -> Option<String> {
    let (first, rest) = key.split_once('/')?;
    if !crate::reserved_table(first) {
        return Some(format!("{first}/"));
    }
    let behind = crate::sql::internal(key) && first != "_sonos_seq";
    behind.then(|| format!("{first}/{}/", rest.split('/').next().unwrap_or_default()))
}

impl ReadSet {
    pub(crate) fn complete() -> ReadSet {
        ReadSet { complete: true, ..Default::default() }
    }

    /// For a world whose metadata is `meta`: kept since its fork (`complete`) or not.
    pub(crate) fn of(meta: &Map<String, Json>, complete: bool) -> ReadSet {
        ReadSet { complete, rerun: meta.contains_key(RERUN_READS), ..Default::default() }
    }

    /// Whether its SELECTs are kept to run again (see the module docs).
    pub(crate) fn reruns(&self) -> bool {
        self.rerun && !self.loose
    }

    /// Where what's read now goes besides: `outside`, unless a kept SELECT reads it.
    fn apart(&mut self) -> Option<&mut ReadSet> {
        match self.reruns() && !REPLAY.get() {
            true => Some(self.outside.get_or_insert_with(|| Box::new(ReadSet::complete()))),
            false => None,
        }
    }

    /// Table group `g` read by the statement running: the columns it uses join those its reads used.
    fn used(&mut self, g: &str) {
        let Some(table) = g.strip_suffix('/').filter(|t| !t.contains('/')) else { return }; // a table's own rows
        let cols = USING.with_borrow(|u| u.as_ref().and_then(|m| m.get(table).cloned()));
        match (self.columns.get_mut(g), cols) {
            (Some(None), _) => {}
            (Some(Some(had)), Some(c)) => had.extend(c),
            (Some(slot), None) => *slot = None,
            (None, c) => {
                self.columns.insert(g.to_string(), c);
            }
        }
    }

    pub(crate) fn key(&mut self, k: &str) {
        if let Some(o) = self.apart() {
            o.key(k);
        }
        let Some(g) = group(k) else { return };
        self.used(&g);
        if self.groups.contains(&g) {
            return;
        }
        if self.keys.len() >= MOST_KEYS {
            return self.whole(g);
        }
        self.keys.insert(k.to_string());
    }

    pub(crate) fn range(&mut self, prefix: &str, after: Option<&str>, upto: Option<&str>) {
        if let Some(o) = self.apart() {
            o.range(prefix, after, upto);
        }
        let Some(g) = group(prefix).or_else(|| group(&format!("{prefix}/"))) else { return };
        self.used(&g);
        if self.groups.contains(&g) {
            return;
        }
        if (prefix == g && after.is_none() && upto.is_none()) || self.ranges.len() >= MOST_RANGES {
            return self.whole(g);
        }
        self.ranges.insert((prefix.to_string(), after.map(String::from), upto.map(String::from)));
    }

    /// A search: it may have read any row of `table`.
    pub(crate) fn table(&mut self, table: &str) {
        if let Some(o) = self.apart() {
            o.table(table);
        }
        if let Some(g) = group(&format!("{table}/")) {
            self.columns.insert(g.clone(), None); // a search may rank by any column
            self.whole(g);
        }
    }

    /// A scan of `table` whose rows `f` matches: any of them it may have read.
    pub(crate) fn filtered(&mut self, table: &str, f: crate::sql::RowFilter) {
        if let Some(o) = self.apart() {
            o.filtered(table, f.clone());
        }
        let Some(g) = group(&format!("{table}/")) else { return };
        self.used(&g);
        if self.groups.contains(&g) {
            return;
        }
        // (a filter that can't be written out, a value kept only in memory, is kept each time: rare)
        if let Some(j) = f.to_json()
            && !self.seen.insert(format!("{g}\u{0}{j}"))
        {
            return; // the same conditions on the same table: already kept
        }
        if self.filters.len() >= MOST_FILTERS {
            return self.whole(g);
        }
        self.filters.push((g, f));
    }

    /// A SELECT read and can't be run again (it failed): no hold is lifted by running the kept ones again.
    pub(crate) fn loosen(&mut self) {
        self.loose = true;
        self.kept.clear();
        self.outside = None;
    }

    /// Plain SELECT `run` gave `out` (see `sql::Session::replayable`): kept to run again at the merge. Too many, or
    /// too many rows: its reads can't all be run again.
    pub(crate) fn keep(&mut self, run: crate::sql::Replay, out: &crate::sql::Output) {
        if !self.reruns() {
            return;
        }
        if out.rows.len() > MOST_ROWS || self.kept.len() >= MOST_KEPT {
            return self.loosen();
        }
        let digest = run.digest(out);
        // (the same statement giving the same again, an agent's loop: kept once)
        let same = |k: &Kept| k.digest == digest && k.run.same(&run);
        if !self.kept.iter().any(same) {
            self.kept.push(Kept { shown: crate::sql::result_summary(out), run, digest });
        }
    }

    /// Per table read, by its name as rows are keyed: the row keys read one by one, and whether more of it was read
    /// than keys (a range, a filtered scan, the whole table, or what's behind its indexes and constraints), for which
    /// any of its rows counts. For checking reads against the Postgres an attached folder copies (see `attach`).
    pub(crate) fn by_table(&self) -> BTreeMap<String, (Vec<String>, bool)> {
        let table = |g: &str| -> Option<String> {
            let parts: Vec<&str> = g.trim_end_matches('/').split('/').collect();
            match parts.as_slice() {
                [t] => Some(t.to_string()),
                [_, t] => Some(t.to_string()),
                _ => None,
            }
        };
        let mut out: BTreeMap<String, (Vec<String>, bool)> = BTreeMap::new();
        for k in &self.keys {
            let Some(g) = group(k) else { continue };
            let Some(t) = table(&g) else { continue };
            let e = out.entry(t).or_default();
            match g.matches('/').count() > 1 {
                true => e.1 = true, // an index's or constraint's entry: found through any row
                false => e.0.push(k.clone()),
            }
        }
        let wider = self.ranges.iter().filter_map(|(p, ..)| group(p).or_else(|| group(&format!("{p}/"))));
        for g in wider.chain(self.groups.iter().cloned()).chain(self.filters.iter().map(|(g, _)| g.clone())) {
            if let Some(t) = table(&g) {
                out.entry(t).or_default().1 = true;
            }
        }
        out
    }

    /// The filtered scans of `table` (by its name as rows are keyed), when they and rows read one by one are all
    /// that was read of it: no key range, not the whole table, nothing behind its indexes or constraints. For
    /// checking reads against the Postgres an attached folder copies (see `attach`), which can then be asked for the
    /// rows they match rather than the table. None otherwise, and when no scan of it was filtered.
    pub(crate) fn only_filtered(&self, table: &str) -> Option<Vec<crate::sql::RowFilter>> {
        let g = format!("{table}/");
        let of_table = |x: &str| {
            x == g
                || x.strip_suffix(&g)
                    .is_some_and(|first| first.ends_with('/') && !first[..first.len() - 1].contains('/'))
        };
        let wider = self.ranges.iter().filter_map(|(p, ..)| group(p).or_else(|| group(&format!("{p}/"))));
        if self.groups.iter().cloned().chain(wider).any(|x| of_table(&x))
            || self.keys.iter().filter_map(|k| group(k)).any(|x| x != g && of_table(&x))
        {
            return None;
        }
        let fs: Vec<crate::sql::RowFilter> =
            self.filters.iter().filter(|(f, _)| *f == g).map(|(_, f)| f.clone()).collect();
        (!fs.is_empty()).then_some(fs)
    }

    fn whole(&mut self, g: String) {
        self.keys.retain(|k| !k.starts_with(&g));
        self.ranges.retain(|(p, ..)| !p.starts_with(&g));
        self.filters.retain(|(f, _)| *f != g);
        self.seen.retain(|s| !s.starts_with(&format!("{g}\u{0}")));
        self.groups.insert(g);
    }

    /// As `readsets` keeps it. A filter that can't be kept as it is (a value of a type kept only
    /// in memory, a text range in a collation) is kept as its whole table: more, never less.
    fn to_json(&self) -> Json {
        let mut groups = self.groups.clone();
        let mut filters = vec![];
        for (g, f) in &self.filters {
            match f.to_json() {
                Some(j) if !groups.contains(g) => filters.push(json!([g, j])),
                Some(_) => {}
                None => {
                    groups.insert(g.clone());
                }
            }
        }
        filters.retain(|f| f[0].as_str().is_some_and(|g| !groups.contains(g)));
        json!({
            "keys": self.keys,
            "ranges": self.ranges.iter().map(|(p, a, u)| json!([p, a, u])).collect::<Vec<_>>(),
            "groups": groups,
            "filters": filters,
            "columns": self.columns,
            // (the SELECTs kept to run again aren't written out: after a restart a hold stays as found)
            "rerun": self.rerun,
        })
    }

    /// Back from `to_json`, complete: None if it doesn't read back whole (then the world's reads
    /// are unknown, as after a crash).
    fn from_json(j: &Json) -> Option<ReadSet> {
        let strs = |v: &Json| -> Option<BTreeSet<String>> {
            v.as_array()?.iter().map(|x| x.as_str().map(String::from)).collect()
        };
        let opt = |v: &Json| -> Option<Option<String>> {
            match v {
                Json::Null => Some(None),
                v => Some(Some(v.as_str()?.to_string())),
            }
        };
        let ranges = j["ranges"]
            .as_array()?
            .iter()
            .map(|r| Some((r[0].as_str()?.to_string(), opt(&r[1])?, opt(&r[2])?)))
            .collect::<Option<_>>()?;
        let filters = j["filters"]
            .as_array()?
            .iter()
            .map(|f| Some((f[0].as_str()?.to_string(), crate::sql::RowFilter::from_json(&f[1])?)))
            .collect::<Option<_>>()?;
        let filters: Vec<(String, crate::sql::RowFilter)> = filters;
        let seen = filters.iter().filter_map(|(g, f)| Some(format!("{g}\u{0}{}", f.to_json()?))).collect();
        // (kept by an older version: none, and every column counts)
        let columns = match &j["columns"] {
            Json::Null => BTreeMap::new(),
            c => serde_json::from_value(c.clone()).ok()?,
        };
        Some(ReadSet {
            complete: true,
            keys: strs(&j["keys"])?,
            ranges,
            groups: strs(&j["groups"])?,
            filters,
            seen,
            columns,
            rerun: j["rerun"].as_bool().unwrap_or(false),
            kept: vec![],
            outside: None,
            loose: true,
        })
    }
}

/// A group as a reason names it: `orders`, or `orders (an index or constraint of it)`.
fn named(g: &str) -> String {
    let parts: Vec<&str> = g.trim_end_matches('/').split('/').collect();
    match parts.as_slice() {
        [t] => crate::sql::shown_name(t),
        [_, t] => format!("{} (an index or constraint of it)", crate::sql::shown_name(t)),
        _ => g.to_string(),
    }
}

/// `check_reads` in a new world's metadata: true becomes the database's own key; false, or none,
/// leaves nothing.
/// `rerun_reads = true` likewise, and it checks its reads too.
pub(crate) fn from_meta(meta: &mut Map<String, Json>) -> Result<()> {
    let flag = |meta: &mut Map<String, Json>, k: &str| match meta.remove(k) {
        None | Some(Json::Bool(false)) => Ok(false),
        Some(Json::Bool(true)) => Ok(true),
        Some(_) => Err(crate::bad_meta(format!("{k} must be true or false"))),
    };
    let (check, rerun) = (flag(meta, "check_reads")?, flag(meta, "rerun_reads")?);
    if check || rerun {
        meta.insert(CHECK_READS.into(), Json::Bool(true));
    }
    if rerun {
        meta.insert(RERUN_READS.into(), Json::Bool(true));
    }
    Ok(())
}

/// How the error `stale` gives ends: what tells it from the other refusals that share its SQLSTATE.
const STALE_END: &str = ": redo the work in a new world forked from now";

/// Whether `e` is the error `stale` gives.
pub(crate) fn is_stale(e: &Error) -> bool {
    matches!(e, Error::Constraint { code: "40001", message } if message.ends_with(STALE_END))
}

/// The error a merge (or a rebase: `doing`) gets when what it read changed (and its agent keeps
/// to no merge policy).
pub(crate) fn stale(b: &str, doing: &str, reasons: &[String]) -> Error {
    let after = match doing {
        "merging" => "nothing was merged",
        _ => "nothing changed",
    };
    Error::Constraint {
        code: "40001", // Postgres's serialization_failure: the same kind of refusal
        message: format!("{doing} {b} {}; {after}{STALE_END}", reasons.join("; it ")),
    }
}

/// The file the read sets are kept in between a clean close and the next open.
const FILE: &str = "readsets";

/// Takes back the read sets kept at the last close into the worlds of `db` (just recovered from
/// `dir`), if it closed cleanly (`clean`), and deletes the file either way: a later crash must
/// never find it. A world that isn't there any more, or reads back wrong, is skipped (its reads
/// unknown, its merge held, as after a crash).
pub(crate) fn load(db: &mut Core, dir: &Path, crypt: Option<&crate::crypt::Crypt>, clean: bool) -> io::Result<()> {
    let path = dir.join(FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    std::fs::remove_file(&path)?;
    if !clean {
        return Ok(()); // a crash came after it: what was read since is lost
    }
    let sets: Json = match crate::crypt::open_file(crypt, bytes, &path.display().to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(io::Error::other))
    {
        Ok(j) => j,
        Err(e) => {
            crate::obs::log_line(
                "WARN",
                "reads_damaged",
                &[
                    ("path", path.display().to_string()),
                    ("error", format!("{e}")),
                    ("detail", "worlds that check their reads will have their merges held".into()),
                ],
            );
            return Ok(());
        }
    };
    let m = db.branches.get_mut().unwrap();
    let by_id: HashMap<String, &crate::sync::Arc<crate::Branch>> =
        m.values().map(|e| (e.br.info.read().unwrap().id.clone(), &e.br)).collect();
    for (id, j) in sets.as_object().into_iter().flatten() {
        if let (Some(br), Some(set)) = (by_id.get(id), ReadSet::from_json(j)) {
            *br.reads.write().unwrap() = Some(crate::sync::Arc::new(crate::sync::Mutex::new(set)));
        }
    }
    Ok(())
}

impl Core {
    /// Keeps the read sets of the open worlds that check their reads, by world ID, for the next
    /// open (see `load`). Called as the database closes, when nothing reads any more.
    pub(crate) fn save_read_sets(&self) -> io::Result<()> {
        let Some(log) = &self.log else { return Ok(()) };
        let m = self.branches.read().unwrap();
        let mut sets = Map::new();
        for e in m.values() {
            let Some(r) = e.br.reads.read().unwrap().clone() else { continue };
            let set = r.lock().unwrap();
            let id = e.br.info.read().unwrap().id.clone();
            if set.complete && !id.is_empty() {
                sets.insert(id, set.to_json());
            }
        }
        if sets.is_empty() {
            return Ok(());
        }
        let bytes = Json::Object(sets).to_string().into_bytes();
        let bytes = match &log.crypt {
            Some(c) => c.seal_file(&bytes),
            None => bytes,
        };
        crate::disk::write_file(&log.dir.join(FILE), &bytes)
    }

    /// Why a merge is held for reading what changed, given the merged world's read set `set`:
    /// `base` its fork point, `now` world `into`, the one it merges into, as it is, `wrote` the keys
    /// it changed itself (those conflict, or settle, on their own). When what it read came only from
    /// SELECTs kept to run again, and they give what they gave, nothing (see the module docs).
    pub(crate) fn stale_reasons(
        &self,
        set: &crate::Recorder,
        into: &str,
        base: &State,
        now: &State,
        wrote: &HashSet<String>,
    ) -> Result<Vec<String>> {
        let set = set.lock().unwrap();
        let out = self.reasons(&set, base, now, wrote)?;
        // (rows changed that something other than a kept SELECT read: held, whatever they give)
        if out.is_empty() || !set.complete || !set.reruns() || set.kept.is_empty() {
            return Ok(out);
        }
        if let Some(o) = &set.outside
            && !self.reasons(o, base, now, wrote)?.is_empty()
        {
            return Ok(out);
        }
        let kept = set.kept.clone();
        drop(set);
        self.run_again(into, base, now, &kept, out)
    }

    /// `stale_reasons` for read set `set`, as read.
    fn reasons(&self, set: &ReadSet, base: &State, now: &State, wrote: &HashSet<String>) -> Result<Vec<String>> {
        let mut out = vec![];
        if !set.complete {
            out.push(
                "relies on reads made before the database restarted, which weren't kept; redo its work in a new world, or turn check_reads off for it".into(),
            );
        }
        let mut changed: Vec<String> = vec![];
        let mut whole: Vec<String> = vec![];
        for k in &set.keys {
            if !wrote.contains(k.as_str()) && self.val(base, k)? != self.val(now, k)? {
                changed.push(k.clone());
            }
        }
        // a range or a whole group: what changed in it between the fork point and now
        let in_range = |prefix: &str, after: Option<&str>, upto: Option<&str>| -> Result<Vec<String>> {
            let start = after.filter(|a| *a > prefix).unwrap_or(prefix);
            let mut found = vec![];
            for c in self.state_diff_iter(base, now, Some(start))? {
                let c = c?;
                if !c.key.starts_with(prefix) || upto.is_some_and(|u| c.key.as_str() > u) {
                    break;
                }
                if !wrote.contains(c.key.as_str()) {
                    found.push(c.key);
                }
            }
            Ok(found)
        };
        for (prefix, after, upto) in &set.ranges {
            changed.extend(in_range(prefix, after.as_deref(), upto.as_deref())?);
        }
        for g in &set.groups {
            let found = in_range(g, None, None)?;
            if !found.is_empty() && !self.unused(set, base, now, found)?.is_empty() {
                whole.push(g.clone());
            }
        }
        // filtered scans: a changed row counts if a filter matched it as it was, or as it is
        if !set.filters.is_empty() {
            let views = [self.past_view(base), self.past_view(now)];
            let mut tables: BTreeSet<&str> = set.filters.iter().map(|(g, _)| g.as_str()).collect();
            tables.retain(|g| !set.groups.contains(*g));
            for g in tables {
                let fs: Vec<&crate::sql::RowFilter> =
                    set.filters.iter().filter(|(f, _)| f == g).map(|(_, f)| f).collect();
                // the table as each state has it, read once; each changed row decoded once per state
                let sides = [crate::sql::Filters::new(&views[0], &fs), crate::sql::Filters::new(&views[1], &fs)];
                for c in self.state_diff_iter(base, now, Some(g))? {
                    let c = c?;
                    if !c.key.starts_with(g) {
                        break;
                    }
                    if wrote.contains(c.key.as_str()) {
                        continue;
                    }
                    let hit = |i: usize, v: &Option<crate::Val>| v.as_ref().is_some_and(|v| sides[i].any(&c.key, v));
                    if hit(0, &c.before) || hit(1, &c.after) {
                        changed.push(c.key);
                    }
                }
            }
        }
        changed.sort();
        changed.dedup();
        let changed = self.unused(set, base, now, changed)?;
        // rows first, then what's behind indexes and constraints, which a row read usually covers
        let (rows, behind): (Vec<String>, Vec<String>) =
            changed.into_iter().partition(|k| group(k).is_some_and(|g| !crate::reserved_table(&g[..g.len() - 1])));
        if !rows.is_empty() {
            let shown: Vec<String> = rows.iter().take(SHOWN).map(|k| crate::sql::show_key(k).into_owned()).collect();
            let more = if rows.len() > SHOWN { format!(" and {} more", rows.len() - SHOWN) } else { String::new() };
            out.push(format!("read rows that changed since it forked: {}{more}", shown.join(", ")));
        }
        let mut groups: BTreeSet<String> = behind.iter().filter_map(|k| group(k)).collect();
        groups.extend(whole);
        for g in groups {
            out.push(format!("read {}, which changed since it forked", named(&g)));
        }
        Ok(out)
    }

    /// The reasons `held` a merge into `into` is held for, given that the rows changed were read only by the SELECTs
    /// `kept`: none when each gives what it gave, on `base` and on `now`; else `held`, and for each that gives
    /// something else now, what it gave and gives (`held` alone when one can't be run again, or gives something else
    /// on `base`).
    fn run_again(
        &self,
        into: &str,
        base: &State,
        now: &State,
        kept: &[Kept],
        held: Vec<String>,
    ) -> Result<Vec<String>> {
        static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let Some(db) = self.db() else { return Ok(held) };
        let started = std::time::Instant::now();
        let run = |k: &Kept, s: &State| -> Option<crate::sql::Output> {
            let left = ALL_TIMEOUT.saturating_sub(started.elapsed().as_millis() as u64);
            if left == 0 {
                return None;
            }
            // a name only this run reads the state by; `@` makes it read only (see `checks`)
            let at = format!("{into}@reads-{}", RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            self.merging.lock().unwrap().insert(at.clone(), s.clone());
            let out = k.run.run(&db, &at, TIMEOUT.min(left));
            self.merging.lock().unwrap().remove(&at);
            out.ok()
        };
        let mut changed = vec![];
        for k in kept {
            // as then on the fork point: what it gave came from the rows (not its own writes, the clock, chance)
            match run(k, base) {
                Some(b) if k.run.digest(&b) == k.digest => {}
                _ => return Ok(held),
            }
            let Some(o) = run(k, now) else { return Ok(held) };
            if k.run.digest(&o) != k.digest {
                let mut text: String = k.run.text.chars().take(TEXT_SHOWN).collect();
                if text.len() < k.run.text.len() {
                    text.push_str("...");
                }
                let now_shown = match crate::sql::result_summary(&o) {
                    s if s == k.shown => format!("{s}, not the same"),
                    s => s,
                };
                changed.push(format!("read `{text}`, which gave {} then and gives {now_shown} now", k.shown));
            }
        }
        if changed.is_empty() {
            return Ok(changed);
        }
        let mut held = held;
        held.extend(changed);
        Ok(held)
    }

    /// Of `keys`, changed between `base` and `now`, those that count against what was read: all
    /// but the rows updated only in columns no read of their table used, or only in columns the
    /// table didn't have at `base` (added since, as an attached folder's refresh adds Postgres's:
    /// nothing read them). A column read that the table doesn't have (a whole-row reference, a
    /// name a query made) counts as every column.
    fn unused(&self, set: &ReadSet, base: &State, now: &State, keys: Vec<String>) -> Result<Vec<String>> {
        let mut out = Vec::with_capacity(keys.len());
        for k in keys {
            let cols = group(&k).and_then(|g| set.columns.get(&g).cloned().flatten());
            let c = crate::Change { key: k.clone(), before: self.val(base, &k)?, after: self.val(now, &k)? };
            let c = self.show_change(base, now, c)?;
            let counts = match crate::sql::describe(std::slice::from_ref(&c)).pop() {
                Some(d) if d.change == "update" => {
                    let had = |name: &String| d.before.as_ref().is_some_and(|r| r.get(name).is_some());
                    let seen: Vec<&String> = d.columns.iter().filter(|x| had(x)).collect();
                    match &cols {
                        _ if seen.is_empty() => false,
                        None => true,
                        Some(cols) => {
                            let has = |name: &String| {
                                [&d.before, &d.after].iter().all(|r| r.as_ref().is_some_and(|r| r.get(name).is_some()))
                            };
                            !cols.iter().all(has) || seen.iter().any(|x| cols.contains(*x))
                        }
                    }
                }
                _ => true,
            };
            if counts {
                out.push(k);
            }
        }
        Ok(out)
    }
}
