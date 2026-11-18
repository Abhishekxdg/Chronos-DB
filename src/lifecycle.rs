//! A world's life, held to at every use rather than only by the background sweeper:
//!
//! - **Expiry.** A world whose time is up (`_sonos_expires`) is read-only at once: no write, merge, fork, or merge into
//!   it, and an open transaction in it can't commit what it wrote. It still reads until the sweeper discards it, with
//!   the worlds forked from it (the deepest first). A fork never outlives its parent: its expiry is capped by the
//!   parent's when it's forked or its TTL is set, and shortening a parent's shortens its forks'.
//! - **Nesting.** `max_depth` and `max_children`, for the database (ALTER SYSTEM) and per agent: a fork past either is
//!   refused (54000). Transactions' own worlds count toward neither.
//! - **Grants on one world.** `GRANT WRITE, MERGE ON WORLD w TO agent [UNTIL ...]` lets an agent do that in w and the
//!   worlds forked from it, never in main (`_sonos_grants`). A grantor grants only what it holds there; an expired
//!   grant counts for nothing.
//! - **Transfer.** `TRANSFER WORLD w TO agent` gives w a new owner and moves its lease epoch (`_sonos_epoch`) on by
//!   one; the old owner is fenced (`_sonos_fenced`): its writes and merges there get 55P03.
//! - **Budgets.** `budget_rows` and `budget_statements` (`_sonos_budget`): rows written and statements that wrote, in
//!   the world and the worlds forked from it, counted as they're written; over, 53400. A fork's own budget is at most
//!   what its parent has left.

use crate::agents::{self, Agent, Can};
use crate::sync::Arc;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::{Branch, Core, Error, MAIN, Result, World, disk};
use serde_json::{Map, Value as Json, json};

/// Metadata keys only the database sets: grants on the world, by agent then right (`{"until": ms or null, "by":
/// grantor}`); its lease epoch (a count of transfers); the agents it was transferred away from; its budget.
pub(crate) const GRANTS: &str = "_sonos_grants";
pub const EPOCH: &str = "_sonos_epoch";
pub(crate) const FENCED: &str = "_sonos_fenced";
pub(crate) const BUDGET: &str = "_sonos_budget";

/// The rights a grant on a world gives.
pub const RIGHTS: [&str; 4] = ["read", "write", "merge", "fork"];

/// The keys a budget is declared with (CREATE WORLD ... WITH, ALTER WORLD ... SET).
const BUDGET_KEYS: [&str; 2] = ["budget_rows", "budget_statements"];

/// Some branch in this process has a budget: until then, writes skip looking for one (std's, as a static outlives a
/// simulated run).
static BUDGETED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What a branch keeps of its metadata to check every write against without reading it (see `Branch::life`): when
/// it expires, and its budget with what it has used of it since this process opened the database.
#[derive(Default)]
pub(crate) struct Life {
    /// ms since 1970, 0: never. A transaction's is its world's, copied when it begins and when the world's changes.
    pub(crate) expires: AtomicU64,
    rows_max: AtomicU64,
    statements_max: AtomicU64,
    rows: AtomicU64,
    statements: AtomicU64,
}

impl Life {
    pub(crate) fn of(meta: &Map<String, Json>) -> Life {
        let l = Life::default();
        l.cache(meta);
        l
    }

    /// Takes what it keeps from `meta`, the world's metadata as just set.
    pub(crate) fn cache(&self, meta: &Map<String, Json>) {
        self.expires.store(meta.get(agents::EXPIRES).and_then(Json::as_u64).unwrap_or(0), Ordering::Relaxed);
        let b = meta.get(BUDGET);
        let (rows, statements) = (budget_of(b, "rows"), budget_of(b, "statements"));
        if rows > 0 || statements > 0 {
            BUDGETED.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.rows_max.store(rows, Ordering::Relaxed);
        self.statements_max.store(statements, Ordering::Relaxed);
    }

    fn budgeted(&self) -> bool {
        self.rows_max.load(Ordering::Relaxed) > 0 || self.statements_max.load(Ordering::Relaxed) > 0
    }

    /// What's left of its budget: (rows, statements), None where it has no limit.
    fn left(&self) -> (Option<u64>, Option<u64>) {
        let left = |max: &AtomicU64, used: &AtomicU64| match max.load(Ordering::Relaxed) {
            0 => None,
            m => Some(m.saturating_sub(used.load(Ordering::Relaxed))),
        };
        (left(&self.rows_max, &self.rows), left(&self.statements_max, &self.statements))
    }
}

fn budget_of(b: Option<&Json>, k: &str) -> u64 {
    b.and_then(|b| b.get(k)).and_then(Json::as_u64).unwrap_or(0)
}

/// Whether a row written counts toward a budget: a table's row, not an index's entry or the database's own.
fn counted(key: &str) -> bool {
    !key.starts_with("_sonos_") && !crate::sql::internal(key) && !crate::sql::btree_entry(key)
}

fn expired(w: &str, at: u64) -> Error {
    Error::Constraint {
        code: "55000", // object_not_in_prerequisite_state
        message: format!(
            "world {w} expired {} ago: it reads until it's discarded, but nothing more is written to it, merged from or \
             into it, or forked from it",
            ago(disk::now_ms().saturating_sub(at))
        ),
    }
}

fn ago(ms: u64) -> String {
    match ms {
        ..1000 => format!("{ms} ms"),
        1000..120_000 => format!("{} s", ms / 1000),
        120_000..7_200_000 => format!("{} min", ms / 60_000),
        _ => format!("{} h", ms / 3_600_000),
    }
}

fn too_many(message: String) -> Error {
    Error::Constraint { code: "54000", message } // program_limit_exceeded
}

fn over_budget(message: String) -> Error {
    Error::Constraint { code: "53400", message } // configuration_limit_exceeded
}

fn refused(message: String) -> Error {
    Error::Constraint { code: "42501", message }
}

fn bad(message: String) -> Error {
    Error::Constraint { code: "22023", message }
}

/// The error a fenced agent's change gets (see `Acting::fenced`).
pub(crate) fn lease_lost(w: &World, agent: &str) -> Error {
    Error::Constraint {
        code: "55P03", // lock_not_available: its lease on the world is gone
        message: format!(
            "lease lost: world {} was transferred to {} (epoch {}), so agent {agent} may no longer change or merge it",
            w.name,
            w.owner.as_deref().unwrap_or("the database's own users"),
            epoch_of(w)
        ),
    }
}

fn epoch_of(w: &World) -> u64 {
    w.meta.get(EPOCH).and_then(Json::as_u64).unwrap_or(0)
}

/// Whether `agent` lost world `w` to a transfer and hasn't been given it back.
pub(crate) fn fenced(w: &World, agent: &str) -> bool {
    w.owner.as_deref() != Some(agent)
        && w.meta.get(FENCED).and_then(Json::as_array).is_some_and(|a| a.iter().any(|n| n.as_str() == Some(agent)))
}

impl Core {
    /// Refuses a change to branch `b` (a world, or a transaction's in one) once its time is up. Replaying the log, what
    /// happened stands.
    pub(crate) fn check_alive(&self, b: &str, br: &Branch) -> Result<()> {
        let at = br.life.expires.load(Ordering::Relaxed);
        if at == 0 || at > disk::now_ms() || self.replaying() {
            return Ok(());
        }
        Err(expired(&self.intent_world(b), at))
    }

    /// Refuses merging branch `b` into `into` once either's time is up; a transaction that wrote nothing commits (it
    /// only read).
    pub(crate) fn merge_alive(&self, b: &str, into: Option<&str>) -> Result<()> {
        let br = self.branch(b)?;
        if b.starts_with("_tx_") && br.version.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        self.check_alive(b, &br)?;
        match into {
            Some(t) => {
                let tb = self.branch(t)?;
                self.check_alive(t, &tb)
            }
            None => Ok(()),
        }
    }

    /// Before a fork of `from` (not a transaction's): refused if `from`'s time is up, and the new world's expiry (in
    /// `meta`) capped at `from`'s, so it never outlives it.
    pub(crate) fn fork_expiry(&self, from: &str, meta: &mut Map<String, Json>) -> Result<()> {
        if self.replaying() {
            return Ok(()); // replaying: as it was
        }
        let src = self.branch(from)?;
        self.check_alive(from, &src)?;
        let cap = src.life.expires.load(Ordering::Relaxed);
        if cap > 0 && meta.get(agents::EXPIRES).and_then(Json::as_u64).is_none_or(|e| e > cap) {
            meta.insert(agents::EXPIRES.into(), json!(cap));
        }
        Ok(())
    }

    /// Whether a fork of `from` goes deeper (`depth` its depth) or wider than the database allows (`max_depth`,
    /// `max_children`); main's forks are `max_worlds`'s. Called holding the branch map, so forks racing count.
    pub(crate) fn fork_room(&self, from: &str, depth: usize, children: impl FnOnce() -> usize) -> Result<()> {
        if self.replaying() {
            return Ok(());
        }
        let deepest = self.setting(crate::Setting::MaxDepth);
        if deepest > 0 && depth as u64 + 1 > deepest {
            return Err(too_many(format!(
                "a fork of {from} would be {} worlds deep, and the database allows {deepest} (ALTER SYSTEM SET max_depth)",
                depth + 1
            )));
        }
        let widest = self.setting(crate::Setting::MaxChildren);
        if widest > 0 && from != MAIN && children() as u64 >= widest {
            return Err(too_many(format!(
                "{from} has {widest} forks already, as many as the database allows (ALTER SYSTEM SET max_children)"
            )));
        }
        Ok(())
    }

    /// Agent `a`'s own `max_depth` and `max_children`, for a fork of `from`.
    pub(crate) fn agent_fork_room(&self, a: &Agent, from: &str) -> Result<()> {
        if a.max_depth == 0 && a.max_children == 0 {
            return Ok(());
        }
        let w = self.world(from)?;
        if a.max_depth > 0 && w.depth as u64 + 1 > a.max_depth {
            return Err(too_many(format!(
                "agent {} may fork worlds at most {} deep; a fork of {} would be {}",
                a.name,
                a.max_depth,
                w.name,
                w.depth + 1
            )));
        }
        if a.max_children > 0 && w.name != MAIN {
            let n = self.children(&w.name);
            if n as u64 >= a.max_children {
                return Err(too_many(format!(
                    "agent {} may fork a world at most {} times while its forks live; {} has {n}",
                    a.name, a.max_children, w.name
                )));
            }
        }
        Ok(())
    }

    /// Live worlds forked from `w` (not transactions').
    fn children(&self, w: &str) -> usize {
        let m = self.branches.read().unwrap();
        m.values().filter(|e| e.parent.as_deref() == Some(w)).count()
            - m.iter().filter(|(n, e)| n.starts_with("_tx_") && e.parent.as_deref() == Some(w)).count()
    }

    /// Sets when world `b` expires (None: never), capped at its parent's, and shortens its forks' that would outlive it;
    /// transactions open in it and in them expire with their worlds.
    pub(crate) fn set_expiry(&self, b: &str, at: Option<u64>) -> Result<()> {
        let parent = self.world(b)?.parent.filter(|p| p != MAIN);
        let cap = match parent {
            Some(p) => Some(self.branch(&p)?.life.expires.load(Ordering::Relaxed)).filter(|e| *e > 0),
            None => None,
        };
        let at = match (at, cap) {
            (Some(a), Some(c)) => Some(a.min(c)),
            (a, c) => a.or(c),
        };
        self.set_reserved(b, agents::EXPIRES, at.map(|t| json!(t)))?;
        // parents before their forks
        for d in self.descendants(b).into_iter().rev() {
            let (br, parent) = {
                let m = self.branches.read().unwrap();
                let Some(e) = m.get(&d) else { continue };
                (e.br.clone(), e.parent.clone())
            };
            let up = match parent.map(|p| self.branch(&p)) {
                Some(Ok(p)) => p.life.expires.load(Ordering::Relaxed),
                _ => continue,
            };
            if d.starts_with("_tx_") {
                br.life.expires.store(up, Ordering::Relaxed);
            } else {
                let own = br.life.expires.load(Ordering::Relaxed);
                if up > 0 && (own == 0 || own > up) {
                    self.set_reserved(&d, agents::EXPIRES, Some(json!(up)))?;
                }
            }
        }
        Ok(())
    }

    /// Worlds whose time is up, with every world forked from them, transactions' too (their commits are refused
    /// already): what the sweeper discards.
    pub(crate) fn expired_trees(&self, now: u64) -> Vec<String> {
        let due: Vec<String> = {
            let m = self.branches.read().unwrap();
            m.iter()
                .filter(|(n, e)| {
                    let at = e.br.life.expires.load(Ordering::Relaxed);
                    *n != MAIN && !n.starts_with("_tx_") && at > 0 && at <= now
                })
                .map(|(n, _)| n.clone())
                .collect()
        };
        let mut all: Vec<String> = vec![];
        let mut seen = std::collections::HashSet::new();
        for w in due {
            for n in self.descendants(&w).into_iter().chain([w]) {
                if seen.insert(n.clone()) {
                    all.push(n);
                }
            }
        }
        all
    }

    /// Counts `keys` (one statement's rows, about to be written to branch `b`) against the budgets of `b` and every
    /// world it was forked from; refused, counting nothing, if any would go over. Replaying the log: nothing.
    pub(crate) fn charge<'k>(&self, b: &str, keys: impl Iterator<Item = &'k str>) -> Result<()> {
        if !BUDGETED.load(std::sync::atomic::Ordering::Relaxed) || self.replaying() {
            return Ok(());
        }
        let chain: Vec<(String, Arc<Branch>)> = {
            let m = self.branches.read().unwrap();
            let (mut out, mut at) = (vec![], Some(b.to_string()));
            while let Some(n) = at {
                let Some(e) = m.get(&n) else { break };
                if e.br.life.budgeted() {
                    out.push((n.clone(), e.br.clone()));
                }
                at = e.parent.clone();
            }
            out
        };
        if chain.is_empty() {
            return Ok(());
        }
        let rows = keys.filter(|k| counted(k)).count() as u64;
        if rows == 0 {
            return Ok(()); // the database's own rows only (statistics, a sequence's): no statement of the budget's
        }
        for (i, (w, br)) in chain.iter().enumerate() {
            let l = &br.life;
            let r = l.rows.fetch_add(rows, Ordering::Relaxed) + rows;
            let s = l.statements.fetch_add(1, Ordering::Relaxed) + 1;
            let (rmax, smax) = (l.rows_max.load(Ordering::Relaxed), l.statements_max.load(Ordering::Relaxed));
            let over = match () {
                _ if rmax > 0 && r > rmax => Some(format!("{} rows written, of budget_rows = {rmax}", r)),
                _ if smax > 0 && s > smax => Some(format!("{s} statements, of budget_statements = {smax}")),
                _ => None,
            };
            if let Some(over) = over {
                for (_, br) in &chain[..=i] {
                    br.life.rows.fetch_sub(rows, Ordering::Relaxed);
                    br.life.statements.fetch_sub(1, Ordering::Relaxed);
                }
                let w = if w.starts_with("_tx_") { self.intent_world(w) } else { w.clone() };
                return Err(over_budget(format!(
                    "world {w}'s budget is used up: this would make {over}; nothing was written (ALTER WORLD {w} SET \
                     (budget_rows = n, budget_statements = n) raises it)"
                )));
            }
        }
        Ok(())
    }

    /// Takes a budget declared for a fork of `from` out of `meta` (`budget_rows`, `budget_statements`) and puts it in
    /// as the database keeps it: at most what `from` (and each world it was forked from) has left.
    pub(crate) fn budget_from_meta(&self, from: &str, meta: &mut Map<String, Json>) -> Result<()> {
        let Some(new) = self.declared_budget(meta, None)? else { return Ok(()) };
        self.within_budget(from, &new, &format!("a fork of {from}"))?;
        meta.insert(BUDGET.into(), new);
        Ok(())
    }

    /// The budget `patch` declares (on top of `old`), taken out of it; None if it declares none.
    fn declared_budget(&self, patch: &mut Map<String, Json>, old: Option<&Json>) -> Result<Option<Json>> {
        if !BUDGET_KEYS.iter().any(|k| patch.contains_key(*k)) {
            return Ok(None);
        }
        let mut b = old.and_then(Json::as_object).cloned().unwrap_or_default();
        for (key, short) in BUDGET_KEYS.into_iter().zip(["rows", "statements"]) {
            match patch.remove(key) {
                None => {}
                Some(Json::Null) => {
                    b.remove(short);
                }
                Some(v) => match v.as_u64().filter(|n| *n > 0) {
                    Some(n) => {
                        b.insert(short.into(), json!(n));
                    }
                    None => return Err(bad(format!("{key} must be a whole number above 0, or null for none"))),
                },
            }
        }
        Ok(Some(Json::Object(b)))
    }

    /// Refuses budget `new` for a world under `under` if it's more than `under`, or any world it was forked from, has
    /// left.
    fn within_budget(&self, under: &str, new: &Json, what: &str) -> Result<()> {
        let mut at = Some(self.world(under)?.name);
        while let Some(n) = at {
            let br = self.branch(&n)?;
            let (rows, statements) = br.life.left();
            for (k, left) in [("rows", rows), ("statements", statements)] {
                if let (Some(want), Some(left)) = (new.get(k).and_then(Json::as_u64), left)
                    && want > left
                {
                    return Err(over_budget(format!(
                        "{what} can't have budget_{k} = {want}: world {n} has {left} left, and a fork's budget is at \
                         most its parent's"
                    )));
                }
            }
            at = self.world(&n)?.parent;
        }
        Ok(())
    }

    /// ALTER WORLD b SET (budget_rows = n, ...): the budget keys taken out of `patch` and set, by the database's own
    /// users or an admin (an agent raising its own world's budget would make it none).
    pub(crate) fn rebudget(&self, b: &str, patch: &mut Map<String, Json>) -> Result<()> {
        let old = self.world(b)?.meta.get(BUDGET).cloned();
        let Some(new) = self.declared_budget(patch, old.as_ref())? else { return Ok(()) };
        let a = disk::actor();
        if !a.is_empty() && !self.agent(&a).is_ok_and(|x| x.may(Can::Admin)) {
            return Err(refused(format!("agent {a} may not change {b}'s budget: only an admin may")));
        }
        if let Some(p) = self.world(b)?.parent.filter(|p| p != MAIN) {
            self.within_budget(&p, &new, b)?;
        }
        let new = Some(new).filter(|n| n.as_object().is_some_and(|o| !o.is_empty()));
        self.set_reserved(b, BUDGET, new)
    }

    /// World `w`'s lease epoch: how many times it was transferred (see `transfer_world`). 0 for one never transferred.
    pub fn world_epoch(&self, w: &str) -> Result<u64> {
        Ok(epoch_of(&self.world(w)?))
    }

    /// Whether `agent` holds `right` on world `w` by a grant on it or a world it was forked from, unexpired: Some(when
    /// the grant ends, 0 for never).
    pub(crate) fn granted(&self, w: &World, agent: &str, right: &str) -> Option<u64> {
        let now = disk::now_ms();
        let mut cur = std::borrow::Cow::Borrowed(w);
        loop {
            if cur.name == MAIN {
                return None;
            }
            if let Some(g) = cur.meta.get(GRANTS).and_then(|g| g.get(agent)).and_then(|g| g.get(right)) {
                match g["until"].as_u64() {
                    None => return Some(0),
                    Some(u) if u > now => return Some(u),
                    Some(_) => {}
                }
            }
            cur = std::borrow::Cow::Owned(self.world(cur.parent.as_deref()?).ok()?);
        }
    }

    /// GRANT `rights` ON WORLD `w` TO agent `to` [UNTIL `until`]: as the acting agent, only rights it holds on `w`
    /// (and no longer than it holds them by a grant of its own). Never on main.
    pub fn grant_world(&self, w: &str, to: &str, rights: &[String], until: Option<u64>) -> Result<World> {
        let world = self.world(w)?;
        let w = world.name.clone();
        if w == MAIN {
            return Err(refused(
                "grants on worlds are for worlds, not main: what an agent may do in main is its own rights (write_main)"
                    .into(),
            ));
        }
        self.agent(to)
            .map_err(|_| Error::Constraint { code: "42704", message: format!("agent {to} does not exist") })?;
        if until.is_some_and(|u| u <= disk::now_ms()) {
            return Err(bad("UNTIL is already past: a grant must end later than now".into()));
        }
        let rights = known_rights(rights)?;
        let actor = disk::actor();
        let mut ends: Vec<(String, Option<u64>)> = vec![];
        for r in &rights {
            let mut end = until;
            if !actor.is_empty() {
                let me = self.as_agent(&actor)?;
                if !me.agent.may(Can::Admin) {
                    me.holds(&world, r).map_err(|e| {
                        refused(format!(
                            "agent {actor} may not grant {r} on world {w}: it doesn't hold it there ({})",
                            e
                        ))
                    })?;
                    // what it holds only by a grant, it hands on no longer than that grant lasts
                    if let Some(u) = self.granted(&world, &actor, r).filter(|u| *u > 0) {
                        end = Some(end.map_or(u, |e| e.min(u)));
                    }
                }
            }
            ends.push((r.clone(), end));
        }
        self.put_world_meta(&w, |meta| {
            let mut grants = meta.get(GRANTS).and_then(Json::as_object).cloned().unwrap_or_default();
            let mine = grants.entry(to.to_string()).or_insert_with(|| json!({}));
            for (r, end) in &ends {
                mine[r.as_str()] = json!({"until": end, "by": actor});
            }
            meta.insert(GRANTS.into(), Json::Object(grants));
        })?;
        let till = until.map(|u| format!(" until {u}")).unwrap_or_default();
        self.record_security("grant on world", &format!("{} on world {w} to {to}{till}", rights.join(", ")));
        self.world(&w)
    }

    /// REVOKE `rights` ON WORLD `w` FROM agent `from`: by its owner, the agent that granted them, or an admin.
    pub fn revoke_world(&self, w: &str, from: &str, rights: &[String]) -> Result<World> {
        let world = self.world(w)?;
        let w = world.name.clone();
        let rights = known_rights(rights)?;
        let actor = disk::actor();
        if !actor.is_empty() && world.owner.as_deref() != Some(actor.as_str()) {
            let me = self.as_agent(&actor)?;
            let by_me = |r: &String| {
                world
                    .meta
                    .get(GRANTS)
                    .and_then(|g| g.get(from))
                    .and_then(|g| g.get(r.as_str()))
                    .is_none_or(|g| g["by"] == actor.as_str())
            };
            if !me.agent.may(Can::Admin) && !rights.iter().all(by_me) {
                return Err(refused(format!(
                    "agent {actor} may not revoke rights on world {w} it didn't grant: its owner or an admin may"
                )));
            }
        }
        self.put_world_meta(&w, |meta| {
            let mut grants = meta.get(GRANTS).and_then(Json::as_object).cloned().unwrap_or_default();
            if let Some(Json::Object(mine)) = grants.get_mut(from) {
                for r in &rights {
                    mine.remove(r);
                }
                if mine.is_empty() {
                    grants.remove(from);
                }
            }
            match grants.is_empty() {
                true => meta.remove(GRANTS),
                false => meta.insert(GRANTS.into(), Json::Object(grants)),
            };
        })?;
        self.record_security("revoke on world", &format!("{} on world {w} from {from}", rights.join(", ")));
        self.world(&w)
    }

    /// TRANSFER WORLD `w` TO agent `to`: its owner becomes `to` and its lease epoch goes up by one; the old owner is
    /// fenced from it (see `fenced`). By its owner or an admin, to an agent whose rights cover the world: one that
    /// may change the worlds it owns, of the world's tenant if it's held to one, with room for one more world.
    pub fn transfer_world(&self, w: &str, to: &str) -> Result<World> {
        let world = self.world(w)?;
        let w = world.name.clone();
        if w == MAIN || world.parent.is_none() {
            return Err(refused(format!("{w} has no owner to transfer: it's not a world forked from another")));
        }
        let actor = disk::actor();
        if !actor.is_empty()
            && world.owner.as_deref() != Some(actor.as_str())
            && !self.agent(&actor).is_ok_and(|a| a.may(Can::Admin))
        {
            return Err(refused(format!("agent {actor} may not transfer world {w}: only its owner or an admin may")));
        }
        let new = self
            .agent(to)
            .map_err(|_| Error::Constraint { code: "42704", message: format!("agent {to} does not exist") })?;
        let cant = |why: &str| refused(format!("world {w} can't go to agent {to}: {why}"));
        if new.disabled || new.expired() {
            return Err(cant("it's disabled or its token expired"));
        }
        if !(new.may(Can::WriteOwn) || new.may(Can::Write)) {
            return Err(cant("it may not change the worlds it owns (it needs 'write_own')"));
        }
        if let Some(t) = &new.tenant {
            let theirs = world.scope.as_ref().and_then(|s| s.get("tenant")).and_then(crate::scope::tenant_key);
            if theirs != crate::scope::tenant_key(t) {
                return Err(cant(&format!(
                    "it works for one tenant ({}), and the world isn't declared that tenant's",
                    new.tenant_sql().unwrap_or_default()
                )));
            }
        }
        if new.max_worlds > 0 && world.owner.as_deref() != Some(to) {
            let owned = self.worlds().iter().filter(|x| x.owner.as_deref() == Some(to)).count() as u64;
            if owned >= new.max_worlds {
                return Err(cant(&format!("it owns {owned} worlds, as many as it may (max_worlds)")));
            }
        }
        let was = world.owner.clone();
        let mut epoch = 0;
        self.put_world_meta(&w, |meta| {
            epoch = meta.get(EPOCH).and_then(Json::as_u64).unwrap_or(0) + 1;
            let mut fenced: Vec<Json> = meta.get(FENCED).and_then(Json::as_array).cloned().unwrap_or_default();
            fenced.retain(|n| n.as_str() != Some(to));
            if let Some(old) = was.as_deref().filter(|o| *o != to)
                && !fenced.iter().any(|n| n.as_str() == Some(old))
            {
                fenced.push(json!(old));
            }
            meta.insert(agents::OWNER.into(), json!(to));
            meta.insert(EPOCH.into(), json!(epoch));
            match fenced.is_empty() {
                true => meta.remove(FENCED),
                false => meta.insert(FENCED.into(), Json::Array(fenced)),
            };
            // written back to Postgres as its new owner's end user, never the old one's
            match &new.act_as {
                Some(who) => meta.insert(agents::ACT_AS.into(), who.clone()),
                None => meta.remove(agents::ACT_AS),
            };
        })?;
        let from = was.as_deref().unwrap_or("the database's own users");
        self.record_security("transfer world", &format!("{w} from {from} to {to} (epoch {epoch})"));
        self.world(&w)
    }

    /// Changes world `w`'s metadata, its reserved keys too, in one logged write.
    fn put_world_meta(&self, w: &str, edit: impl FnOnce(&mut Map<String, Json>)) -> Result<()> {
        crate::not_tx(w)?;
        self.put_meta(w, |m| {
            edit(m);
            Ok(())
        })
    }
}

/// `rights` checked against `RIGHTS`, lowercased, without repeats.
fn known_rights(rights: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = vec![];
    for r in rights {
        let r = r.trim().to_ascii_lowercase();
        if !RIGHTS.contains(&r.as_str()) {
            return Err(bad(format!("{r} isn't a right on a world: READ, WRITE, MERGE or FORK")));
        }
        if !out.contains(&r) {
            out.push(r);
        }
    }
    if out.is_empty() {
        return Err(bad("name at least one right: READ, WRITE, MERGE or FORK".into()));
    }
    Ok(out)
}
