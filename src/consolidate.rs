//! Negotiable transactions: a coordinator world `p` whose children propose changes, combined rather than raced.
//!
//!   column merge semantics (`SET MERGE AS DELTA [ESCROW (MIN n, MAX m)]`, see `sql::MergeDelta`) ─> a row both sides
//!     changed only in delta columns (and in other columns one side alone) merges as parent + (ours - base)
//!     (`Core::delta_settle`, called from the merge's per-row decision); a bound it breaks refuses it (23514)
//!   FORECAST CONFLICTS [FOR WORLD w] ─> each pair of live worlds of one parent, and each world against what its
//!     parent changed since its fork: rows both changed (write-write, `combinable` when only delta columns
//!     overlap), and rows a world that checks its reads read that the other changed (read-write). Nothing changes
//!   CONSOLIDATE PREVIEW p ─> p's children, in the order they were forked, merged in memory one after another onto
//!     p as it is: per row applied / combined-delta / conflict / escrow-violation. Nothing changes
//!   CONSOLIDATE p [ONLY CHILDREN (c1, ...)] [USING ARBITRATION] ─> a scratch fork of p (`SCRATCH` + p); each child
//!     merged into it (MERGE ... INTO, so the child stays) in fork order, at the version it had when the
//!     consolidation began ─> any one refused: the scratch is discarded, nothing merged, every child kept
//!     ─> all merged: the scratch merges into p (one merge), then the children are discarded
//!
//! With USING ARBITRATION a row two children both changed (not one the parent changed meanwhile) is settled by the
//! acting agent's merge policy's `arbitrate` rule (`RULES`; none: `priority`, the world owners' agent priority, which
//! only an admin sets, the older world winning a tie), and the losing value is recorded in p's metadata
//! (`ARBITRATION`: key, winner_world, loser_world, loser_value_hash, rule), never lost silently.
//!
//! The children are held from before they're copied until they're discarded (see `Hold`): nothing changes them
//! meanwhile. A crash before the last merge leaves p and every child as they were, and maybe the scratch world, which
//! the next CONSOLIDATE of p discards first. Just before the last merge each child is marked with the scratch world's
//! id (`CONSOLIDATED`); that merge retires the scratch world in its own log record, so a crash after it leaves marked
//! children whose scratch world is gone: their rows are in p, and the next open (or CONSOLIDATE of p) discards them,
//! and no merge of them goes through meanwhile.

use crate::{Core, Error, MergeOptions, Pick, Result, State, Val, View, json_object, reserved_table, shown, sql};
use serde_json::{Map, Value as Json, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

/// The rules `arbitrate` takes (see `MergePolicy::arbitrate`).
pub(crate) const RULES: [&str; 4] = ["priority", "newest", "oldest", "fail"];

/// Metadata key of a coordinator world: the rows arbitration settled in its consolidations, each with the losing value's
/// hash (see the module docs). Only the database writes it.
pub(crate) const ARBITRATION: &str = "_sonos_arbitration";

/// The scratch world a consolidation of `p` merges its children into first: this, then p's name.
const SCRATCH: &str = "_consolidate_";

/// Entries `ARBITRATION` keeps, newest last.
const KEPT: usize = 200;

/// A row two worlds may both change (see `Core::forecast_conflicts`).
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    pub world_a: String,
    pub world_b: String,
    /// `write-write` or `read-write` (world_a read it, world_b changed it)
    pub kind: &'static str,
    /// the row's key, `table/id`
    pub key: String,
    /// both changed it only in delta columns (see `sql::MergeDelta`): their merges combine
    pub combinable: bool,
}

/// What consolidating one child would do to one row (see `Core::consolidate_preview`).
#[derive(Debug, Clone, PartialEq)]
pub struct Consolidated {
    pub world: String,
    pub key: String,
    /// `applied`, `combined-delta`, `conflict` or `escrow-violation`
    pub outcome: &'static str,
    pub detail: String,
}

/// `theirs` with `ours`'s changes since `base`, for a row both sides changed where every column both changed merges as
/// a delta (`deltas`): those as theirs + ours - base, the columns only ours changed as ours has them. Without
/// `by_columns` the two sides mustn't both have changed other columns too (as a row merges whole). None: not settled
/// so; an escrow bound the result breaks: 23514, naming the row and the column.
pub(crate) fn settle_deltas(
    key: &str,
    base: &Map<String, Json>,
    ours: &Map<String, Json>,
    theirs: &Map<String, Json>,
    deltas: &[sql::DeltaColumn],
    by_columns: bool,
) -> Result<Option<Map<String, Json>>> {
    let delta = |k: &str| deltas.iter().find(|(c, _)| c == k).map(|(_, d)| *d);
    let cols: BTreeSet<&String> = base.keys().chain(ours.keys()).chain(theirs.keys()).collect();
    let mine: Vec<&String> = cols.iter().copied().filter(|k| ours.get(*k) != base.get(*k)).collect();
    let others: Vec<&String> = cols.iter().copied().filter(|k| theirs.get(*k) != base.get(*k)).collect();
    let both: Vec<&String> = mine.iter().copied().filter(|k| others.contains(k)).collect();
    if both.is_empty() || both.iter().any(|k| delta(k).is_none()) {
        return Ok(None);
    }
    let rest = |side: &[&String]| side.iter().any(|k| delta(k).is_none());
    if !by_columns && rest(&mine) && rest(&others) {
        return Ok(None);
    }
    let mut row = theirs.clone();
    for k in mine {
        let Some(d) = delta(k).filter(|_| both.contains(&k)) else {
            match ours.get(k) {
                Some(v) => row.insert(k.clone(), v.clone()),
                None => row.remove(k),
            };
            continue;
        };
        let (Some(b), Some(o), Some(t)) = (base.get(k), ours.get(k), theirs.get(k)) else { return Ok(None) };
        let Some(sum) = crate::writeback::add_deltas(b, o, t) else { return Ok(None) };
        let Some(n) = crate::writeback::decimal(&sum) else { return Ok(None) };
        let past = |bound: Option<(i128, u32)>, wrong: std::cmp::Ordering| {
            bound.filter(|b| crate::writeback::cmp_decimal(n, *b) == Some(wrong))
        };
        let broken = match (past(d.min, std::cmp::Ordering::Less), past(d.max, std::cmp::Ordering::Greater)) {
            (Some(m), _) => Some(("MIN", m)),
            (None, Some(m)) => Some(("MAX", m)),
            (None, None) => None,
        };
        if let Some((which, bound)) = broken {
            let show = crate::writeback::show_decimal;
            return Err(Error::Constraint {
                code: "23514",
                message: format!(
                    "{} can't be merged: its delta column {k} would be {} ({} here, {} in the world merged into, from {}), \
                     past its escrow {which} {}; nothing was merged",
                    sql::show_key(key),
                    show(n),
                    o,
                    t,
                    b,
                    show(bound)
                ),
            });
        }
        row.insert(k.clone(), sum);
    }
    Ok(Some(row))
}

/// Whose change wins a row two children both changed, by `rule`: true for the later-forked child.
fn later_wins(rule: &str, earlier: (u64, u8), later: (u64, u8)) -> Option<bool> {
    match rule {
        "newest" => Some(true),
        "oldest" => Some(false),
        "priority" => Some(later.1 > earlier.1), // a tie: the older world
        _ => None,
    }
}

impl Core {
    /// A row both sides of a merge changed (`vals`: base, ours, theirs, in `states`), settled by its table's delta
    /// columns (`deltas`, see `settle_deltas`) and stored as `into` (the world merged into, with the definitions the
    /// merge brings) stores it. None where they don't settle it; an escrow bound broken: 23514.
    pub(crate) fn delta_settle(
        &self,
        key: &str,
        states: [&State; 3],
        vals: [&Option<Val>; 3],
        by_columns: bool,
        deltas: &[sql::DeltaColumn],
        into: &View,
    ) -> Result<Option<Val>> {
        let [Some(b), Some(o), Some(t)] = vals else { return Ok(None) }; // an add or a delete: a conflict
        let obj = |s: &State, v: &Val| json_object(&self.show(s, key, v.clone(), &mut HashMap::new()).ok()?);
        let (Some(bo), Some(oo), Some(to)) = (obj(states[0], b), obj(states[1], o), obj(states[2], t)) else {
            return Ok(None);
        };
        let Some(mut row) = settle_deltas(key, &bo, &oo, &to, deltas, by_columns)? else { return Ok(None) };
        if let Some(cols) = sql::stored_columns(into, key.split_once('/').map_or(key, |(t, _)| t))? {
            row.retain(|k, _| cols.contains(k));
        }
        sql::stored(into, key, &Json::Object(row).to_string()).map(Some)
    }

    /// World `p`'s live children (no transactions, no scratch world), in the order they were forked; with `only`,
    /// those alone, each of which must be one (22023).
    fn children_of(&self, p: &str, only: &[String]) -> Result<Vec<(String, Arc<crate::Branch>)>> {
        let m = self.branches.read().unwrap();
        if !m.contains_key(p) {
            return Err(Error::NoBranch(p.into()));
        }
        let mut out: Vec<(String, Arc<crate::Branch>)> = m
            .iter()
            .filter(|(n, e)| e.parent.as_deref() == Some(p) && !n.starts_with("_tx_") && !n.starts_with(SCRATCH))
            .filter(|(n, _)| only.is_empty() || only.iter().any(|o| o == *n))
            .map(|(n, e)| (n.clone(), e.br.clone()))
            .collect();
        if let Some(stray) = only.iter().find(|o| !out.iter().any(|(n, _)| n == *o)) {
            return Err(Error::Constraint {
                code: "22023",
                message: format!("{stray} isn't a live world forked from {p}: CONSOLIDATE merges only {p}'s children"),
            });
        }
        out.sort_by_key(|(_, br)| br.born);
        Ok(out)
    }

    /// The rows of world `b` (keys as `table/id`) it changed since its fork, as (before, after).
    fn rows_changed(&self, base: &State, now: &State) -> Result<BTreeMap<String, crate::Change>> {
        Ok(self
            .state_diff(base, now)?
            .into_iter()
            .filter(|c| shown(&c.key) && !sql::internal(&c.key) && !reserved_table(crate::table_of(&c.key)))
            .map(|c| (c.key.clone(), c))
            .collect())
    }

    /// `FORECAST CONFLICTS [FOR WORLD w]` (see the module docs): every live world's, or `w`'s alone. Nothing changes.
    pub fn forecast_conflicts(&self, w: Option<&str>) -> Result<Vec<Forecast>> {
        let live: Vec<(String, String, Arc<crate::Branch>)> = {
            let m = self.branches.read().unwrap();
            if let Some(w) = w
                && !m.contains_key(w)
            {
                return Err(Error::NoBranch(w.into()));
            }
            let mut v: Vec<_> = m
                .iter()
                .filter(|(n, _)| !n.starts_with("_tx_") && !n.starts_with(SCRATCH))
                .filter_map(|(n, e)| Some((n.clone(), e.parent.clone()?, e.br.clone())))
                .collect();
            v.sort_by_key(|(_, _, br)| br.born);
            v
        };
        let w_parent = w.and_then(|w| live.iter().find(|(n, _, _)| n == w).map(|(_, p, _)| p.clone()));
        // the worlds that matter: w and its siblings, or all
        let worlds: Vec<&(String, String, Arc<crate::Branch>)> = match (w, &w_parent) {
            (Some(_), Some(p)) => live.iter().filter(|(_, wp, _)| wp == p).collect(),
            (Some(_), None) => vec![], // main, or a database's root: no parent, no siblings
            (None, _) => live.iter().collect(),
        };
        let mut changed: HashMap<&str, BTreeMap<String, crate::Change>> = HashMap::new();
        for (n, _, br) in &worlds {
            changed.insert(n.as_str(), self.rows_changed(&br.base(), &br.snapshot())?);
        }
        let mut deltas: HashMap<String, Vec<sql::DeltaColumn>> = HashMap::new();
        let mut out = vec![];
        let row = |out: &mut Vec<Forecast>, a: &str, b: &str, kind: &'static str, key: &str, combinable: bool| {
            if w.is_none_or(|w| w == a || w == b) {
                out.push(Forecast { world_a: a.into(), world_b: b.into(), kind, key: key.into(), combinable });
            }
        };
        // whether a row both changed combines: base and ours one side's, theirs the other's
        let combines = |deltas: &mut HashMap<String, Vec<sql::DeltaColumn>>,
                        parent: &str,
                        key: &str,
                        (s1, c1): (&State, &crate::Change),
                        (s2, c2): (&State, &crate::Change)|
         -> Result<bool> {
            let table = crate::table_of(key);
            let pbr = self.branch(parent)?;
            let pv = View { core: self, born: pbr.born, s: pbr.snapshot(), branch: String::new(), reads: None };
            let ds = deltas.entry(table.to_string()).or_insert_with(|| sql::merge_deltas(&pv, table));
            if ds.is_empty() {
                return Ok(false);
            }
            let obj = |s: &State, v: &Option<Val>| {
                v.as_ref().and_then(|v| json_object(&self.show(s, key, v.clone(), &mut HashMap::new()).ok()?))
            };
            let (Some(b), Some(o), Some(t)) = (obj(s1, &c1.before), obj(s1, &c1.after), obj(s2, &c2.after)) else {
                return Ok(false);
            };
            // (from the same base: a sibling's base row may be another, if the parent changed it between the forks)
            if c1.before != c2.before {
                return Ok(false);
            }
            Ok(matches!(settle_deltas(key, &b, &o, &t, ds, false), Ok(Some(_))))
        };
        for (i, (a, pa, abr)) in worlds.iter().enumerate() {
            let ca = &changed[a.as_str()];
            // against siblings forked after it (each pair once), and w against every sibling
            for (b, pb, bbr) in worlds.iter().skip(i + 1) {
                if pa != pb {
                    continue;
                }
                let cb = &changed[b.as_str()];
                // w first when it was named
                let (x, y, cx, cy, sx, sy) = match w == Some(b.as_str()) {
                    true => (b, a, cb, ca, bbr.snapshot(), abr.snapshot()),
                    false => (a, b, ca, cb, abr.snapshot(), bbr.snapshot()),
                };
                for (k, chx) in cx {
                    if let Some(chy) = cy.get(k) {
                        let ok = combines(&mut deltas, pa, k, (&sx, chx), (&sy, chy))?;
                        row(&mut out, x, y, "write-write", k, ok);
                    }
                }
                self.read_write(abr, cb, &mut |k| row(&mut out, a, b, "read-write", k, false))?;
                self.read_write(bbr, ca, &mut |k| row(&mut out, b, a, "read-write", k, false))?;
            }
            // against what the parent changed since the fork
            let Ok(pbr) = self.branch(pa) else { continue };
            let pnow = pbr.snapshot();
            let theirs = self.rows_changed(&abr.base(), &pnow)?;
            let snap = abr.snapshot();
            for (k, cha) in ca {
                if let Some(chp) = theirs.get(k) {
                    let ok = combines(&mut deltas, pa, k, (&snap, cha), (&pnow, chp))?;
                    row(&mut out, a, pa, "write-write", k, ok);
                }
            }
            self.read_write(abr, &theirs, &mut |k| row(&mut out, a, pa, "read-write", k, false))?;
        }
        out.sort_by(|x, y| (&x.world_a, &x.world_b, &x.key, x.kind).cmp(&(&y.world_a, &y.world_b, &y.key, y.kind)));
        out.dedup();
        Ok(out)
    }

    /// For world `br`, if it checks its reads: each row of `writes` (another world's changes) it read and didn't
    /// change itself, to `found`.
    fn read_write(
        &self,
        br: &crate::Branch,
        writes: &BTreeMap<String, crate::Change>,
        found: &mut dyn FnMut(&str),
    ) -> Result<()> {
        if writes.is_empty() || !br.info.read().unwrap().meta.contains_key(crate::stale::CHECK_READS) {
            return Ok(());
        }
        let Some(rec) = br.recorder() else { return Ok(()) };
        let own = self.rows_changed(&br.base(), &br.snapshot())?;
        let set = rec.lock().unwrap();
        for k in writes.keys().filter(|k| !own.contains_key(*k)) {
            if set.may_have_read_key(k) {
                found(k);
            }
        }
        Ok(())
    }

    /// `CONSOLIDATE PREVIEW p [ONLY CHILDREN (...)]` (see the module docs): per child, in fork order, per row it
    /// changed, what consolidating would do. Nothing changes.
    pub fn consolidate_preview(&self, p: &str, only: &[String]) -> Result<Vec<Consolidated>> {
        crate::not_tx(p)?;
        let pbr = self.branch(p)?;
        let kids = self.children_of(p, only)?;
        let mut acc = pbr.snapshot();
        let view = |s: &State| View { core: self, born: pbr.born, s: s.clone(), branch: String::new(), reads: None };
        let mut deltas: HashMap<String, Vec<sql::DeltaColumn>> = HashMap::new();
        let mut out = vec![];
        for (c, br) in kids {
            let (base, ours) = (br.base(), br.snapshot());
            for ch in self.state_diff(&base, &ours)? {
                let theirs = self.val(&acc, &ch.key)?;
                let row = shown(&ch.key) && !sql::internal(&ch.key) && !reserved_table(crate::table_of(&ch.key));
                if theirs == ch.before {
                    acc.set(&ch.key, ch.after.clone());
                    if row {
                        out.push(Consolidated {
                            world: c.clone(),
                            key: ch.key,
                            outcome: "applied",
                            detail: String::new(),
                        });
                    }
                    continue;
                }
                if !row {
                    continue; // (definitions and what's behind constraints follow the rows; the real merge judges them)
                }
                let table = crate::table_of(&ch.key).to_string();
                let ds = deltas.entry(table.clone()).or_insert_with(|| sql::merge_deltas(&view(&acc), &table));
                let shown_as = |s: &State, v: &Option<Val>| {
                    v.as_ref().and_then(|v| self.show(s, &ch.key, v.clone(), &mut HashMap::new()).ok())
                };
                let why = crate::explain(
                    shown_as(&base, &ch.before).as_deref(),
                    shown_as(&ours, &ch.after).as_deref(),
                    shown_as(&acc, &theirs).as_deref(),
                );
                let settled = match ds.is_empty() {
                    true => Ok(None),
                    false => {
                        let into = view(&acc);
                        self.delta_settle(
                            &ch.key,
                            [&base, &ours, &acc],
                            [&ch.before, &ch.after, &theirs],
                            false,
                            ds,
                            &into,
                        )
                    }
                };
                let (outcome, detail) = match settled {
                    Ok(Some(v)) => {
                        acc.set(&ch.key, Some(v));
                        ("combined-delta", "both changes to its delta columns, added".to_string())
                    }
                    Ok(None) => ("conflict", why),
                    Err(Error::Constraint { code: "23514", message }) => ("escrow-violation", message),
                    Err(e) => return Err(e),
                };
                out.push(Consolidated { world: c.clone(), key: ch.key, outcome, detail });
            }
        }
        Ok(out)
    }

    /// `CONSOLIDATE p [ONLY CHILDREN (...)] [USING ARBITRATION]` (see the module docs): p's children (or those named)
    /// merged into p as one merge, then discarded; any one refused, and nothing merges. Returns how many merged.
    pub fn consolidate(&self, p: &str, only: &[String], arbitrate: bool) -> Result<usize> {
        crate::not_tx(p)?;
        // by its name from here on, given by name or id: the scratch world is named after it, and found by that name
        // in `Running` (see `consolidating`)
        let name = self.world(p)?.name;
        let p = name.as_str();
        let _one = Running::take(self.uid, p)?;
        // children a consolidation cut short by a crash had already merged: gone now, before they're picked again
        self.finish_consolidations(Some(p));
        let kids = self.children_of(p, only)?;
        if kids.is_empty() {
            return Ok(0);
        }
        // a child with worlds of its own couldn't be discarded after: refused before anything
        {
            let m = self.branches.read().unwrap();
            if let Some((c, _)) = kids.iter().find(|(c, _)| m.get(c).is_some_and(|e| e.kids > 0)) {
                return Err(Error::HasChildren(c.clone()));
            }
        }
        let rule = match arbitrate {
            false => "fail".to_string(),
            true => self.acting_policy()?.and_then(|p| p.arbitrate).unwrap_or_else(|| "priority".into()),
        };
        // the children are held from before they're copied until they're discarded: no write to them, fork of them,
        // or merge of them or into them (a transaction's commit) goes through meanwhile, so none is left out of the
        // copy and lost when they're discarded, and none is left behind by a fork. Held first, then each one's writer
        // lock taken once (under the gate, which a big statement's commit holds): a change that saw it unheld has
        // finished, and the version read here has it
        let hold = Hold::new(kids.iter().map(|(c, br)| (c.clone(), br.clone())).collect())?;
        let versions: Vec<u64> = {
            let _g = self.gate();
            let mut v = vec![];
            for (c, br) in &kids {
                if *crate::lock(&br.writer) {
                    return Err(Error::NoBranch(c.clone())); // merged or discarded meanwhile
                }
                v.push(br.version.load(std::sync::atomic::Ordering::Relaxed));
            }
            v
        };
        {
            let m = self.branches.read().unwrap();
            if let Some((c, _)) = kids.iter().find(|(c, _)| m.get(c).is_some_and(|e| e.kids > 0)) {
                return Err(Error::HasChildren(c.clone())); // forked from just now
            }
        }
        let scratch = format!("{SCRATCH}{p}");
        if self.branch(&scratch).is_ok() {
            self.discard(&scratch)?; // left by a consolidation a crash cut short
        }
        self.fork_internal(p, &scratch)?; // the database's own: an acting agent was checked for the statement
        let mut settled: Vec<Json> = vec![];
        if let Err(e) = self.consolidate_into(p, &scratch, &kids, &versions, &rule, &mut settled) {
            // (the children unmarked first: a crash between leaves them unmarked, and the scratch world, which the
            // next CONSOLIDATE of p discards)
            for (c, br) in &kids {
                if br.info.read().unwrap().meta.contains_key(CONSOLIDATED) {
                    let _ = self.set_reserved(c, CONSOLIDATED, None);
                }
            }
            if self.branch(&scratch).is_ok() {
                let _ = self.discard(&scratch);
            }
            return Err(e);
        }
        if !settled.is_empty() {
            self.set_reserved_with(p, ARBITRATION, |was| {
                let mut list: Vec<Json> = was.and_then(|v| v.as_array().cloned()).unwrap_or_default();
                list.extend(settled);
                let over = list.len().saturating_sub(KEPT);
                Json::Array(list.split_off(over))
            })?;
        }
        // (their rows are in p now, and they're held until they're gone: see `Hold`. One that isn't discarded here is
        // marked as merged still: no merge of it goes through, and the next CONSOLIDATE of p, or open, discards it)
        let names: Vec<&str> = kids.iter().map(|(c, _)| c.as_str()).collect();
        let _ = self.discard_many(&names);
        drop(hold);
        Ok(kids.len())
    }

    /// Children a consolidation marked as merged (`CONSOLIDATED`) once its scratch world is gone: that scratch merged
    /// into p (its merge and its retirement are one log record), so they are in p, and are discarded. A mark whose
    /// scratch world is live still is a consolidation that never got to its last merge: taken off. At open (every
    /// world), and before each CONSOLIDATE of p (p's children).
    pub(crate) fn finish_consolidations(&self, p: Option<&str>) {
        let (marked, live): (Vec<(String, String)>, BTreeSet<String>) = {
            let m = self.branches.read().unwrap();
            let live = m.values().map(|e| e.br.info.read().unwrap().id.clone()).collect();
            let marked = m
                .iter()
                .filter(|(_, e)| p.is_none_or(|p| e.parent.as_deref() == Some(p)))
                .filter_map(|(n, e)| {
                    let info = e.br.info.read().unwrap();
                    Some((n.clone(), info.meta.get(CONSOLIDATED)?["scratch"].as_str()?.to_string()))
                })
                .collect();
            (marked, live)
        };
        let mut merged = vec![];
        for (c, scratch) in marked {
            match live.contains(&scratch) {
                true => {
                    let _ = self.set_reserved(&c, CONSOLIDATED, None);
                }
                false => merged.push(c),
            }
        }
        let _ = self.discard_many(&merged);
    }

    /// Refuses a merge of world `b` that a consolidation already merged into its parent (see `CONSOLIDATED`): its
    /// rows are there, and would be merged twice.
    pub(crate) fn check_consolidated(&self, b: &str, br: &crate::Branch) -> Result<()> {
        if self.replaying() || COPYING.with_borrow(|c| c.iter().any(|n| n == b)) {
            return Ok(());
        }
        let into = br.info.read().unwrap().meta.get(CONSOLIDATED).map(|m| m["into"].as_str().unwrap_or("").to_string());
        match into {
            None => Ok(()),
            Some(p) => Err(Error::Constraint {
                code: "55000",
                message: format!(
                    "{b} was consolidated into {p} already: its rows are there. It is discarded at the next \
                     CONSOLIDATE {p} (or DROP WORLD {b})"
                ),
            }),
        }
    }

    /// The steps of `consolidate` between the scratch world's fork and its merge into p, that one included.
    fn consolidate_into(
        &self,
        p: &str,
        scratch: &str,
        kids: &[(String, Arc<crate::Branch>)],
        versions: &[u64],
        rule: &str,
        settled: &mut Vec<Json>,
    ) -> Result<()> {
        let refused = |c: &str, e: Error| -> Error {
            let head = format!("consolidating {p}: {c}'s merge was refused, so nothing was merged");
            match e {
                Error::Constraint { code, message } => {
                    Error::Constraint { code, message: format!("{head}: {message}") }
                }
                e @ Error::Conflicts(_) => Error::Constraint { code: "40001", message: format!("{head}: {e}") },
                e => e,
            }
        };
        // which child put each row into the scratch world, with its fork order and owner's priority
        let mut put: HashMap<String, (String, (u64, u8))> = HashMap::new();
        for ((c, br), v) in kids.iter().zip(versions) {
            let me = (br.born, self.owner_priority(c));
            let mut o = MergeOptions { into: Some(scratch.into()), expected: Some(*v), ..Default::default() };
            if rule != "fail" {
                let plan = {
                    let _copying = Copying::of(c);
                    self.merge_preview(c, &o).map_err(|e| refused(c, e))?
                };
                for r in plan.rows.iter().filter(|r| r.outcome == "conflict" && shown(&r.key)) {
                    let Some((other, them)) = put.get(&r.key) else { continue }; // the parent's own change: refused
                    let Some(later) = later_wins(rule, *them, me) else { continue };
                    let (winner, loser, lost) = match later {
                        true => (c.as_str(), other.as_str(), &r.theirs),
                        false => (other.as_str(), c.as_str(), &r.ours),
                    };
                    o.picks.insert(r.key.clone(), if later { Pick::Ours } else { Pick::Theirs });
                    let hash =
                        lost.as_deref().map_or_else(String::new, |v| blake3::hash(v.as_bytes()).to_hex().to_string());
                    settled.push(json!({
                        "key": sql::show_key(&r.key), "winner_world": winner, "loser_world": loser,
                        "loser_value_hash": hash, "rule": rule, "at": crate::disk::now_ms(),
                    }));
                }
            }
            {
                let _copying = Copying::of(c);
                self.merge_with(c, &o).map_err(|e| refused(c, e))?;
            }
            COPIED.with_borrow_mut(|f| {
                if let (Some(f), Some(db)) = (f.as_mut(), self.db()) {
                    f(&db, c);
                }
            });
            for k in self.rows_changed(&br.base(), &br.snapshot())?.into_keys() {
                if !matches!(o.picks.get(&k), Some(Pick::Theirs)) {
                    put.insert(k, (c.clone(), me));
                }
            }
        }
        // held since before they were copied (see `consolidate`): nothing changed them since. Checked again all the
        // same, under the gate, before their rows go into p
        {
            let _g = self.gate();
            for ((c, br), v) in kids.iter().zip(versions) {
                let gone = *crate::lock(&br.writer);
                // a child written to meanwhile would lose those writes when it's discarded
                if gone || br.version.load(std::sync::atomic::Ordering::Relaxed) != *v {
                    return Err(Error::Constraint {
                        code: "40001",
                        message: format!(
                            "consolidating {p}: {c} was written to, merged or discarded meanwhile, so nothing was merged; try again"
                        ),
                    });
                }
            }
        }
        // and none was forked from meanwhile: it couldn't be discarded after
        {
            let m = self.branches.read().unwrap();
            if let Some((c, _)) = kids.iter().find(|(c, _)| m.get(c).is_some_and(|e| e.kids > 0)) {
                return Err(Error::Constraint {
                    code: "40001",
                    message: format!(
                        "consolidating {p}: {c} was forked from meanwhile, so nothing was merged; try again"
                    ),
                });
            }
        }
        // each child marked as merged by this scratch world before its last merge: that merge retires the scratch
        // world in its own log record, so after a crash a mark whose scratch world is gone says the child's rows are
        // in p (it's discarded then), and one whose scratch world is still there says they aren't (the mark goes).
        // The marks are logged before the merge's record: on disk with it or before it
        let id = self.world(scratch)?.id;
        for (c, _) in kids {
            self.set_reserved(c, CONSOLIDATED, Some(json!({ "into": p, "scratch": id })))?;
        }
        self.merge_with(scratch, &MergeOptions::default()).map_err(|e| refused(scratch, e))?;
        PAUSE.with_borrow_mut(|f| {
            if let (Some(f), Some(db)) = (f.as_mut(), self.db()) {
                f(&db, &kids.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>());
            }
        });
        Ok(())
    }

    /// The agent priority of world `w`'s owner, as a number (higher wins): an admin's world, or one whose owner is
    /// gone, as a normal agent's.
    fn owner_priority(&self, w: &str) -> u8 {
        let owner = self.world(w).ok().and_then(|w| w.owner);
        match owner.and_then(|a| self.agent(&a).ok()).map(|a| a.priority) {
            Some(crate::Priority::Low) => 0,
            _ => 1,
        }
    }

    /// Sets reserved metadata key `key` of world `b` to what `f` makes of its value now.
    fn set_reserved_with(&self, b: &str, key: &str, f: impl FnOnce(Option<&Json>) -> Json) -> Result<()> {
        let b = self.world(b)?.name;
        self.put_meta(&b, |meta| {
            let v = f(meta.get(key));
            meta.insert(key.into(), v);
            Ok(())
        })
    }
}

/// Children a CONSOLIDATE merges, held from before they're copied into its scratch world until they're discarded: a
/// write to one, a fork of one, and a merge of one or into one (a transaction's commit) are refused (55000) while
/// held, checked under the locks those take (`Core::check_held`); the consolidation's own copies of them alone go
/// through (`Copying`). One child held by another consolidation already: refused. Let go when dropped.
pub(crate) struct Hold(Vec<Arc<crate::Branch>>);

impl Hold {
    fn new(kids: Vec<(String, Arc<crate::Branch>)>) -> Result<Hold> {
        let mut hold = Hold(vec![]);
        for (c, br) in kids {
            let was = br.held.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            hold.0.push(br);
            if was > 0 {
                return Err(Error::Constraint {
                    code: "55000",
                    message: format!("{c} is being consolidated already: wait for that CONSOLIDATE to end"),
                });
            }
        }
        Ok(hold)
    }
}

/// Metadata key of a child a consolidation merged into its scratch world, set just before that world's last merge
/// into p: `{into: p, scratch: <the scratch world's id>}` (see `Core::finish_consolidations`). Only the database
/// writes it.
pub(crate) const CONSOLIDATED: &str = "_sonos_consolidated";

thread_local! {
    /// The child this thread's consolidation is copying into its scratch world: that merge alone isn't held.
    static COPYING: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(vec![]) };
}

/// A consolidation's copy of child `c` into its scratch world, under way on this thread (see `Hold`).
struct Copying;

impl Copying {
    fn of(c: &str) -> Copying {
        COPYING.with_borrow_mut(|v| v.push(c.to_string()));
        Copying
    }
}

impl Drop for Copying {
    fn drop(&mut self) {
        COPYING.with_borrow_mut(|v| {
            v.pop();
        });
    }
}

/// The coordinator worlds being consolidated now (by database, `Core::uid`), each with the thread doing it: one
/// CONSOLIDATE of a world at a time, so none discards the scratch world of another under way; and no one else drops
/// a scratch world while its consolidation runs (`Core::consolidating`).
static RUNNING: std::sync::Mutex<BTreeMap<(u64, String), std::thread::ThreadId>> =
    std::sync::Mutex::new(BTreeMap::new());

struct Running(u64, String);

impl Running {
    fn take(uid: u64, p: &str) -> Result<Running> {
        let mut running = RUNNING.lock().unwrap();
        if running.contains_key(&(uid, p.to_string())) {
            return Err(Error::Constraint {
                code: "55000",
                message: format!("{p} is being consolidated already: wait for that CONSOLIDATE to end"),
            });
        }
        running.insert((uid, p.to_string()), std::thread::current().id());
        Ok(Running(uid, p.to_string()))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.lock().unwrap().remove(&(self.0, std::mem::take(&mut self.1)));
    }
}

impl Core {
    /// Whether `b` is the scratch world of a consolidation running on another thread: dropping it then (by hand, a
    /// cascade, or its time running out) is refused. Between the children's marks and the scratch world's merge into
    /// p, a drop followed by a crash would leave marks whose scratch world is gone, and the next open would discard
    /// children whose rows never reached p (see `finish_consolidations`).
    pub(crate) fn consolidating(&self, b: &str) -> bool {
        let Some(p) = b.strip_prefix(SCRATCH) else { return false };
        RUNNING.lock().unwrap().get(&(self.uid, p.to_string())).is_some_and(|t| *t != std::thread::current().id())
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        for br in &self.0 {
            br.held.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

impl Core {
    /// Refuses a change to branch `b` (a write, a fork of it, a merge of it or into it) while a consolidation holds it
    /// (see `Hold`).
    pub(crate) fn check_held(&self, b: &str, br: &crate::Branch) -> Result<()> {
        if br.held.load(std::sync::atomic::Ordering::SeqCst) == 0
            || self.replaying()
            || COPYING.with_borrow(|c| c.iter().any(|n| n == b))
        {
            return Ok(());
        }
        Err(Error::Constraint {
            code: "55000",
            message: format!(
                "{b} is being consolidated into its parent: it can't be written to, forked or merged until that's done"
            ),
        })
    }
}

type Pause = Box<dyn FnMut(&crate::Db, &[String])>;

thread_local! {
    /// Tests only: run on this thread by a consolidation between its last merge and discarding the children.
    static PAUSE: std::cell::RefCell<Option<Pause>> = const { std::cell::RefCell::new(None) };
}

/// For tests: `f` runs on this thread whenever a consolidation has merged its scratch world into p and is about to
/// discard the children (given their names): what another session could do in that moment. None: no more.
#[doc(hidden)]
pub fn test_consolidate_pause(f: Option<Pause>) {
    PAUSE.set(f);
}

type Copied = Box<dyn FnMut(&crate::Db, &str)>;

thread_local! {
    /// Tests only: run on this thread by a consolidation each time it has copied a child into its scratch world.
    static COPIED: std::cell::RefCell<Option<Copied>> = const { std::cell::RefCell::new(None) };
}

/// For tests: `f` runs on this thread whenever a consolidation has copied a child into its scratch world (given its
/// name), before the next: what another session could do in that moment. None: no more.
#[doc(hidden)]
pub fn test_consolidate_copied(f: Option<Copied>) {
    COPIED.set(f);
}
