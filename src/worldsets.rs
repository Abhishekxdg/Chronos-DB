//! World sets: worlds forked from one base world, run, compared and ranked as a group, and the best of them merged
//! into the base (never straight into main). An objective names what "best" means: a query to minimise or maximise,
//! and the merge checks a world must pass.
//!
//!   CREATE WORLD SET s FROM WORLD b ─> FORK WORLD SET s COUNT n RUN $$script$$ SEED k   (or ADOPT WORLD w INTO SET s)
//!     ─> COMPARE WORLDS a, b ─> RANK WORLD SET s BY ($$metric$$ DESC, ...) [WITH CHECKS]
//!     ─> PRUNE WORLD SET s WHERE rank > k OR failed ─> MERGE BEST OF SET s INTO b ─> (b's own merge into main)
//!
//! A set is metadata only: its base world holds `_sonos_set_base` (the sets based on it, with their settings), and
//! each member `_sonos_set` (the set's name), its ranking `_sonos_set_scores`, and, when forked by the set, the
//! `sim` metadata SIMULATE writes (so `REPLAY WORLD` makes it again). Members are forked from the base, so their
//! parent is the base: a member merges into the base and nowhere else.
//!
//! A member forked for an agent is forked as the agent forks any world (`Acting::fork`): its owner, expiry, tenant,
//! and what its merge policy has it record; a member with no scope of its own gets the base's.

use crate::simulate::{Run, check_sql, score_of, world_seed};
use crate::sql::{self, Reach, Session};
use crate::{Core, Db, Error, Info, MAIN, Result, World, check_name, disk, metrics, shown};
use serde_json::{Map, Value as Json, json};
use std::cmp::Ordering;

/// A set member with its kept rank and whether its checks failed.
type Ranked = (World, usize, bool);

/// A base world's metadata key: the sets based on it, by name, each with its settings (`max_live`).
pub(crate) const SET_BASE: &str = "_sonos_set_base";
/// A member's metadata key: its set's name.
pub(crate) const SET: &str = "_sonos_set";
/// A member's metadata key: its ranking (see `rank_world_set`).
pub(crate) const SET_SCORES: &str = "_sonos_set_scores";
/// Objectives: rows `_sonos_objectives/<name>` on main.
pub(crate) const OBJECTIVES: &str = "_sonos_objectives";
/// How long a metric query may run in each world, in ms.
const METRIC_TIMEOUT: u64 = 10_000;
/// Members one FORK WORLD SET may make.
const COUNT_MAX: usize = 1 << 16;

fn bad(message: String) -> Error {
    Error::Constraint { code: "22023", message }
}

fn state(code: &'static str, message: String) -> Error {
    Error::Constraint { code, message }
}

fn sql_error(e: sql::SqlError) -> Error {
    Error::Constraint { code: e.code, message: e.message }
}

/// A member made or found by a set statement.
#[derive(Debug, Clone, PartialEq)]
pub struct SetMember {
    pub name: String,
    pub id: String,
    pub index: Option<usize>,
    pub seed: Option<i64>,
    /// why its script failed, if it did (it stays, and ranks last)
    pub error: Option<String>,
}

/// FORK WORLD SET's inputs (see `Db::fork_world_set`).
#[derive(Debug, Clone, PartialEq)]
pub struct ForkSet {
    pub set: String,
    pub count: usize,
    /// SQL run in each new member: $1 is its index, $2 its seed
    pub script: String,
    pub seed: i64,
    /// threads to run members on; 0: as many as the shared budget has free
    pub threads: usize,
    pub agent: Option<String>,
}

/// One metric of a ranking: a query giving one number in each member, and whether lower is better.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub query: String,
    pub ascending: bool,
}

/// A member's place in a ranking.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedWorld {
    pub world: String,
    /// 1 is the best
    pub rank: usize,
    /// each metric's value (None where its query failed)
    pub values: Vec<Option<f64>>,
    /// its script, a metric query or a merge check failed: it ranks after every member that didn't
    pub failed: bool,
    pub reasons: Vec<String>,
    /// still in the set (OPTIMIZE's KEEP discards the others)
    pub kept: bool,
}

/// A row of COMPARE WORLDS: a row the two worlds hold differently (a value is None where the world hasn't the row).
#[derive(Debug, Clone, PartialEq)]
pub struct Differs {
    pub key: String,
    pub a: Option<String>,
    pub b: Option<String>,
}

/// COMPARE WORLDS a, b: the rows they differ in, key order, and per table how many rows each changed since the fork
/// point they share (None when they share none: then they were compared row by row, not by their changes).
#[derive(Debug, Clone, PartialEq)]
pub struct Compared {
    pub rows: Vec<Differs>,
    pub changed: Option<(Vec<String>, Vec<String>)>,
}

/// What PRUNE WORLD SET discards: members ranked below `rank_gt`, and (with `failed`) the failed ones.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prune {
    pub rank_gt: Option<usize>,
    pub failed: bool,
}

/// An objective: what OPTIMIZE ranks worlds by.
#[derive(Debug, Clone, PartialEq)]
pub struct Objective {
    pub name: String,
    pub query: String,
    pub minimize: bool,
    /// merge checks a world must pass (empty: none)
    pub checks: Vec<String>,
    pub created: u64,
}

impl Objective {
    fn to_json(&self) -> String {
        json!({ "query": self.query, "minimize": self.minimize, "checks": self.checks, "created": self.created })
            .to_string()
    }

    fn from_json(name: &str, raw: &str) -> Option<Objective> {
        let j: Json = serde_json::from_str(raw).ok()?;
        Some(Objective {
            name: name.into(),
            query: j["query"].as_str()?.into(),
            minimize: j["minimize"].as_bool()?,
            checks: j["checks"].as_array()?.iter().filter_map(|c| c.as_str().map(String::from)).collect(),
            created: j["created"].as_u64().unwrap_or(0),
        })
    }
}

/// OPTIMIZE's inputs (see `Db::optimize`).
#[derive(Debug, Clone, PartialEq)]
pub struct Optimize {
    pub objective: String,
    pub base: String,
    /// the set made for the run (default: the objective's name)
    pub set: Option<String>,
    pub script: String,
    pub count: usize,
    pub keep: Option<usize>,
    pub seed: i64,
    pub threads: usize,
    pub agent: Option<String>,
}

/// A member's order before any ranking: forked ones by index, then adopted ones by name.
fn member_order(a: &World, b: &World) -> Ordering {
    let index = |w: &World| w.meta.get(crate::SIM).and_then(|s| s["index"].as_u64());
    match (index(a), index(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
    .then_with(|| a.name.cmp(&b.name))
}

/// A world name quoted for SQL.
fn quoted(w: &str) -> String {
    format!("'{}'", w.replace('\'', "''"))
}

impl Core {
    /// World set `set`'s base world and its members (by index, then name).
    pub fn world_set(&self, set: &str) -> Result<(String, Vec<String>)> {
        let (base, members) = self.set_worlds(set)?;
        Ok((base.name, members.into_iter().map(|w| w.name).collect()))
    }

    /// `world_set`, as worlds.
    fn set_worlds(&self, set: &str) -> Result<(World, Vec<World>)> {
        let all = self.worlds();
        let base = all.iter().find(|w| w.meta.get(SET_BASE).and_then(|s| s.get(set)).is_some()).cloned();
        let base = base.ok_or_else(|| state("42704", format!("world set {set} does not exist")))?;
        let mut members: Vec<World> =
            all.into_iter().filter(|w| w.meta.get(SET).and_then(Json::as_str) == Some(set)).collect();
        members.sort_by(member_order);
        Ok((base, members))
    }

    /// The objectives, by name.
    pub fn objectives(&self) -> Result<Vec<Objective>> {
        let prefix = format!("{OBJECTIVES}/");
        let rows = self.view(MAIN)?.prefix(&prefix)?.collect::<Result<Vec<_>>>()?;
        Ok(rows.into_iter().filter_map(|(k, v)| Objective::from_json(&k[prefix.len()..], &v)).collect())
    }

    /// Creates objective `name` (or with `replace` puts it in place of the one of that name): `query`, one SELECT
    /// giving a number, to minimise or maximise, in worlds that pass merge checks `checks`.
    pub fn create_objective(
        &self,
        name: &str,
        query: &str,
        minimize: bool,
        checks: &[String],
        replace: bool,
    ) -> Result<Objective> {
        check_name(name)?;
        metric_sql(query)?;
        let known = self.checks()?;
        if let Some(c) = checks.iter().find(|c| !known.iter().any(|k| k.name == **c)) {
            return Err(state("42704", format!("objective {name}: merge check {c} does not exist")));
        }
        let o = Objective {
            name: name.into(),
            query: query.trim().into(),
            minimize,
            checks: checks.to_vec(),
            created: disk::now_ms(),
        };
        let key = format!("{OBJECTIVES}/{name}");
        self.batch_with(MAIN, |v| {
            if !replace && v.get(&key)?.is_some() {
                return Err(Error::Exists(format!("objective {name}")));
            }
            Ok((vec![(key.clone(), Some(o.to_json()))], ()))
        })?;
        Ok(o)
    }

    /// Removes objective `name`.
    pub fn drop_objective(&self, name: &str) -> Result<()> {
        let key = format!("{OBJECTIVES}/{name}");
        self.batch_with(MAIN, |v| match v.get(&key)? {
            None => Err(state("42704", format!("objective {name} does not exist"))),
            Some(_) => Ok((vec![(key.clone(), None)], ())),
        })?;
        Ok(())
    }
}

/// What a metric (or objective) query is refused for: it must be one statement that only reads.
fn metric_sql(query: &str) -> Result<()> {
    let q = sql::parse(query).map_err(sql_error)?;
    if q.len() != 1 || q[0].access() != Reach::Read {
        return Err(bad(format!(
            "a metric must be one SELECT giving a number, like select sum(total) from orders; not {query}"
        )));
    }
    Ok(())
}

impl Db {
    /// Makes world set `set` of base world `base`: its settings go in the base's metadata. `max_live`: how many
    /// members it may have at once (0: no limit). A set's base can't be main: its best world merges into the base,
    /// and from there into main through the base's own merge, never straight from a member.
    pub fn create_world_set(&self, set: &str, base: &str, max_live: usize, agent: Option<&str>) -> Result<World> {
        check_name(set)?;
        let b = self.world(base)?;
        if b.name == MAIN {
            return Err(state(
                "0A000",
                format!(
                    "a world set's base can't be main: its best world merges into its base, never straight into \
                     main; fork a world from main (CREATE WORLD {set}_base) and make the set from that"
                ),
            ));
        }
        if b.name.starts_with("_tx_") || base.contains('@') {
            return Err(Error::NoBranch(base.into()));
        }
        if let Some(a) = agent {
            let me = self.as_agent(a)?;
            me.may_fork(&b.name)?;
            me.may_manage(&b.name)?;
        }
        if self.world_set(set).is_ok() {
            return Err(Error::Exists(format!("world set {set}")));
        }
        let mut sets = b.meta.get(SET_BASE).and_then(Json::as_object).cloned().unwrap_or_default();
        let mut cfg = Map::new();
        cfg.insert("max_live".into(), json!(max_live));
        cfg.insert("created".into(), json!(disk::now_ms()));
        if let Some(a) = agent {
            cfg.insert("owner".into(), json!(a));
        }
        sets.insert(set.into(), Json::Object(cfg));
        self.set_reserved(&b.name, SET_BASE, Some(Json::Object(sets)))?;
        self.world(&b.name)
    }

    /// Forks `spec.count` new members of set `spec.set` from its base, and runs `spec.script` in each (on every
    /// core, or `spec.threads`): as SIMULATE runs its worlds, so the same set, count, script and seed give the same
    /// members on any number of threads (each member's random() is seeded from the seed and its index, and now()
    /// stands at the moment of the fork). Every member is kept, failed ones too (with the error). Members are named
    /// `<set>_<index>`, indexes carrying on from the members the set has.
    pub fn fork_world_set(&self, spec: &ForkSet) -> Result<Vec<SetMember>> {
        let _t = self.metrics.time(metrics::Op::Simulate);
        let n = spec.count;
        if n == 0 || n > COUNT_MAX {
            return Err(bad(format!("FORK WORLD SET makes 1 to {COUNT_MAX} worlds, not {n}")));
        }
        let (base, members) = self.set_worlds(&spec.set)?;
        // checked as SIMULATE's script is: it can't leave the world it runs in
        check_sql(&crate::Simulate::new(&base.name, n, &spec.set, &spec.script, "select 0"))?;
        let cfg = base.meta[SET_BASE][&spec.set].clone();
        let most = cfg["max_live"].as_u64().unwrap_or(0) as usize;
        if most > 0 && members.len() + n > most {
            return Err(state(
                "53400", // configuration_limit_exceeded
                format!(
                    "world set {} may have at most {most} worlds at once (max_live) and has {}: {n} more is too many; \
                     prune some",
                    spec.set,
                    members.len()
                ),
            ));
        }
        let me = spec.agent.as_deref().map(|a| self.as_agent(a)).transpose()?;
        if let Some(me) = &me {
            me.may_read(&base.name)?;
            me.may_fork(&base.name)?;
        }
        let first = members
            .iter()
            .filter_map(|w| w.meta.get(crate::SIM).and_then(|s| s["index"].as_u64()))
            .max()
            .map_or(0, |i| i as usize + 1);
        let names: Vec<String> = (first..first + n).map(|i| format!("{}_{i}", spec.set)).collect();
        {
            let m = self.branches.read().unwrap();
            if let Some(taken) = names.iter().find(|w| m.contains_key(*w)) {
                return Err(state(
                    "42710",
                    format!("world {taken} already exists; drop it, or name the set otherwise"),
                ));
            }
        }
        let (_, base_id, fixed, at) = self.base_of(&base.name)?;
        let no_moves = crate::counters_fixed();
        let clock = at as i64 * 1000;
        let mut made: Vec<String> = vec![];
        let forked = (|| -> Result<()> {
            match &me {
                // as the agent forks any world: its owner, tenant, scope and policy's flags
                Some(me) => {
                    for w in &names {
                        me.fork(&base.name, w, Json::Null)?;
                        made.push(w.clone());
                        self.set_reserved(w, SET, Some(json!(spec.set)))?;
                        if self.world(w)?.scope.is_none()
                            && let Some(s) = &base.scope
                        {
                            self.set_reserved(w, crate::scope::SCOPE, Some(s.clone()))?;
                        }
                    }
                }
                None => {
                    let mut meta = Map::new();
                    meta.insert(SET.into(), json!(spec.set));
                    // as any fork: the base's tenant identity and scope (rules T1, T5)
                    self.fork_identity(&base.name, &mut meta)?;
                    let infos = names.iter().map(|w| (w.clone(), Info { meta: meta.clone(), ..Info::new() })).collect();
                    self.fork_all(&base.name, infos)?;
                    made.extend(names.iter().cloned());
                }
            }
            Ok(())
        })();
        let cleanup = |made: &[String]| {
            let _ = self.discard_many(made); // nothing of a fork that stopped stays
        };
        if let Err(e) = forked {
            cleanup(&made);
            return Err(e);
        }
        // the rows the base changed since the moment the members stand at, written back first
        let mut fixes = vec![];
        for w in &names {
            let from = self.branch(w)?.base();
            fixes.push(if from.same(&fixed) { vec![] } else { self.undo(&from, &fixed)? });
        }
        let threads = if spec.threads == 0 { usize::MAX } else { spec.threads };
        let ran = sql::par_jobs(n, threads, |j, gone| {
            let i = first + j;
            let seed = world_seed(spec.seed, i);
            let run = Run {
                script: &spec.script,
                score: "select 0",
                seed,
                agent: spec.agent.as_deref(),
                fix: &fixes[j],
                clock,
                fixed: no_moves,
            };
            let (_, error) = self.run_world(&names[j], i, &run, gone);
            (seed, error)
        })
        .map_err(sql_error);
        let ran = match ran {
            Ok(r) => r,
            Err(e) => {
                cleanup(&made);
                return Err(e);
            }
        };
        let mut out = vec![];
        for (j, (seed, error)) in ran.into_iter().enumerate() {
            let w = &names[j];
            // what made it, as SIMULATE keeps it: REPLAY WORLD makes it again
            let sim = json!({
                "seed": spec.seed, "index": first + j, "script": spec.script, "score_query": "select 0",
                "score": if error.is_none() { Some(0.0) } else { None }, "error": error,
                "base": base.name, "base_id": base_id, "at": at,
            });
            if let Err(e) = self.set_reserved(w, crate::SIM, Some(sim)) {
                cleanup(&made);
                return Err(e);
            }
            let id = self.world(w).map(|x| x.id).unwrap_or_default();
            out.push(SetMember { name: w.clone(), id, index: Some(first + j), seed: Some(seed), error });
        }
        Ok(out)
    }

    /// Puts world `w`, a fork of set `set`'s base, in the set.
    pub fn adopt_world(&self, w: &str, set: &str, agent: Option<&str>) -> Result<World> {
        let (base, _) = self.set_worlds(set)?;
        let world = self.world(w)?;
        if world.parent.as_deref() != Some(base.name.as_str()) {
            return Err(state(
                "55000",
                format!(
                    "world {} can't join world set {set}: a set's worlds are forks of its base, {}, and {} is a fork \
                     of {}",
                    world.name,
                    base.name,
                    world.name,
                    world.parent.as_deref().unwrap_or("nothing")
                ),
            ));
        }
        if let Some(other) = world.meta.get(SET).and_then(Json::as_str) {
            return Err(state("55000", format!("world {} is in world set {other} already", world.name)));
        }
        if let Some(a) = agent {
            self.as_agent(a)?.may_manage(&world.name)?;
        }
        self.set_reserved(&world.name, SET, Some(json!(set)))?;
        self.world(&world.name)
    }

    /// The rows worlds `a` and `b` hold differently. Worlds with a fork point in common (siblings forked from one
    /// state of their parent: a set's members) are compared by what each changed since it, O(changes); others
    /// row by row where their trees differ.
    pub fn compare_worlds(&self, a: &str, b: &str, agent: Option<&str>) -> Result<Compared> {
        crate::not_tx(a).and(crate::not_tx(b))?;
        let _t = self.metrics.time(metrics::Op::Diff);
        if let Some(x) = agent {
            let me = self.as_agent(x)?;
            me.may_see_between(a, b)?;
        }
        let (wa, wb) = (self.world(a)?, self.world(b)?);
        let (ba, bb) = (self.branch(&wa.name)?, self.branch(&wb.name)?);
        let (sa, sb) = (ba.snapshot(), bb.snapshot());
        let shared = wa.parent.is_some() && wa.parent == wb.parent && ba.base().same(&bb.base());
        let show = |c: crate::Change| -> Result<Differs> {
            let c = self.show_change(&sa, &sb, c)?;
            Ok(Differs { key: c.key, a: c.before.map(|v| v.to_string()), b: c.after.map(|v| v.to_string()) })
        };
        if !shared {
            let rows = self.state_diff(&sa, &sb)?.into_iter().filter(|c| shown(&c.key)).map(show);
            return Ok(Compared { rows: rows.collect::<Result<_>>()?, changed: None });
        }
        let r0 = ba.base();
        let (da, db) = (self.state_diff(&r0, &sa)?, self.state_diff(&r0, &sb)?);
        let changed = (
            da.iter().filter(|c| shown(&c.key)).map(|c| c.key.clone()).collect(),
            db.iter().filter(|c| shown(&c.key)).map(|c| c.key.clone()).collect(),
        );
        // merge-join the two deltas by key: a key in one only is the base's value in the other
        let (mut i, mut j, mut rows) = (0, 0, vec![]);
        while i < da.len() || j < db.len() {
            let by = match (da.get(i), db.get(j)) {
                (Some(x), Some(y)) => x.key.cmp(&y.key),
                (Some(_), None) => Ordering::Less,
                (None, _) => Ordering::Greater,
            };
            let c = match by {
                Ordering::Less => {
                    i += 1;
                    crate::Change {
                        key: da[i - 1].key.clone(),
                        before: da[i - 1].after.clone(),
                        after: da[i - 1].before.clone(),
                    }
                }
                Ordering::Greater => {
                    j += 1;
                    crate::Change {
                        key: db[j - 1].key.clone(),
                        before: db[j - 1].before.clone(),
                        after: db[j - 1].after.clone(),
                    }
                }
                Ordering::Equal => {
                    i += 1;
                    j += 1;
                    crate::Change {
                        key: da[i - 1].key.clone(),
                        before: da[i - 1].after.clone(),
                        after: db[j - 1].after.clone(),
                    }
                }
            };
            if c.before != c.after && shown(&c.key) {
                rows.push(show(c)?);
            }
        }
        Ok(Compared { rows, changed: Some(changed) })
    }

    /// Ranks set `set`'s members by `metrics` (each a query giving one number, run read only in each member as
    /// `agent`), the first metric first and the others breaking its ties, then by index. A member whose script
    /// failed, or a metric's query, or (with `checks`: those named, or all when empty) a merge check as a merge of
    /// it into the base would run it, ranks after all that didn't. The ranking is kept in each member's metadata
    /// (`_sonos_set_scores`), with the member's version: PRUNE and MERGE BEST go by it.
    pub fn rank_world_set(
        &self,
        set: &str,
        metrics: &[Metric],
        checks: Option<&[String]>,
        agent: Option<&str>,
    ) -> Result<Vec<RankedWorld>> {
        if metrics.is_empty() {
            return Err(bad("RANK WORLD SET needs a metric at least".into()));
        }
        for m in metrics {
            metric_sql(&m.query)?;
        }
        let (base, members) = self.set_worlds(set)?;
        let me = agent.map(|a| self.as_agent(a)).transpose()?;
        if let Some(me) = &me {
            for w in &members {
                me.may_read(&w.name)?;
                me.may_manage(&w.name)?; // its ranking goes in its metadata
            }
        }
        if let Some(c) = checks {
            let known = self.checks()?;
            if let Some(x) = c.iter().find(|c| !known.iter().any(|k| k.name == **c)) {
                return Err(state("42704", format!("merge check {x} does not exist")));
            }
        }
        let mut out: Vec<(RankedWorld, usize, u64)> = vec![];
        for (order, w) in members.iter().enumerate() {
            let mut reasons = vec![];
            if let Some(e) = w.meta.get(crate::SIM).and_then(|s| s["error"].as_str()) {
                reasons.push(format!("its script failed: {e}"));
            }
            let mut values = vec![];
            for m in metrics {
                match self.metric(&w.name, &m.query, agent) {
                    Ok(v) => values.push(Some(v)),
                    Err(e) => {
                        values.push(None);
                        reasons.push(format!("metric `{}` failed: {} (SQLSTATE {})", m.query, e.message, e.code));
                    }
                }
            }
            if let Some(c) = checks {
                let br = self.branch(&w.name)?;
                let (from, now) = (br.base(), br.snapshot());
                let keys: Vec<String> = self.state_diff(&from, &now)?.into_iter().map(|c| c.key).collect();
                let only = (!c.is_empty()).then_some(c);
                // checks read every tenant's rows: an agent gets their reasons without values, as a merge's
                let failed = self.failed_checks_of(&base.name, &now, &keys, only)?;
                reasons.extend(
                    failed.into_iter().map(|r| if agent.is_some() { crate::agents::values_free(&r) } else { r }),
                );
            }
            let failed = !reasons.is_empty();
            out.push((
                RankedWorld { world: w.name.clone(), rank: 0, values, failed, reasons, kept: true },
                order,
                w.version,
            ));
        }
        out.sort_by(|(a, ia, _), (b, ib, _)| {
            let by = match (a.failed, b.failed) {
                (false, true) => Ordering::Less,
                (true, false) => Ordering::Greater,
                (true, true) => Ordering::Equal,
                (false, false) => metrics.iter().enumerate().fold(Ordering::Equal, |o, (k, m)| {
                    o.then_with(|| {
                        let (x, y) = (a.values[k].unwrap_or(f64::NAN), b.values[k].unwrap_or(f64::NAN));
                        if m.ascending { x.total_cmp(&y) } else { y.total_cmp(&x) }
                    })
                }),
            };
            by.then(ia.cmp(ib))
        });
        let queries: Vec<Json> = metrics.iter().map(|m| json!({ "query": m.query, "asc": m.ascending })).collect();
        let mut ranked = vec![];
        for (k, (mut r, _, version)) in out.into_iter().enumerate() {
            r.rank = k + 1;
            let kept = json!({
                "rank": r.rank, "values": r.values, "failed": r.failed, "reasons": r.reasons,
                "metrics": queries, "version": version, "at": disk::now_ms(),
            });
            self.set_reserved(&r.world, SET_SCORES, Some(kept))?;
            ranked.push(r);
        }
        Ok(ranked)
    }

    /// `query`'s number in world `w`, run read only as `agent`.
    fn metric(&self, w: &str, query: &str, agent: Option<&str>) -> sql::Result<f64> {
        let mut s = Session::new(w);
        s.agent = agent.map(String::from);
        let out = s
            .run(self, &format!("set statement_timeout = {METRIC_TIMEOUT}"), &[])
            .and_then(|_| s.run(self, query, &[]));
        s.close(self);
        score_of(out?)
    }

    /// Set `set`'s members with their kept ranking (rank, failed), refused unless each has one as it is now.
    fn ranking(&self, set: &str) -> Result<(World, Vec<Ranked>)> {
        let (base, members) = self.set_worlds(set)?;
        let mut out = vec![];
        for w in members {
            let s = w.meta.get(SET_SCORES).cloned().unwrap_or(Json::Null);
            let (Some(rank), Some(version)) = (s["rank"].as_u64(), s["version"].as_u64()) else {
                return Err(state(
                    "55000",
                    format!("world {} of world set {set} has no ranking: RANK WORLD SET {set} BY (...) first", w.name),
                ));
            };
            if version != w.version {
                return Err(state(
                    "40001",
                    format!(
                        "world {} of world set {set} has changed since it was ranked: RANK WORLD SET {set} again",
                        w.name
                    ),
                ));
            }
            let failed = s["failed"].as_bool().unwrap_or(true);
            out.push((w, rank as usize, failed));
        }
        Ok((base, out))
    }

    /// Discards set `set`'s members that `rule` picks, by their kept ranking. Returns the worlds discarded.
    pub fn prune_world_set(&self, set: &str, rule: Prune, agent: Option<&str>) -> Result<Vec<String>> {
        let (_, ranked) = self.ranking(set)?;
        let gone: Vec<String> = ranked
            .into_iter()
            .filter(|(_, rank, failed)| (rule.failed && *failed) || rule.rank_gt.is_some_and(|k| *rank > k))
            .map(|(w, ..)| w.name)
            .collect();
        if let Some(a) = agent {
            let me = self.as_agent(a)?;
            gone.iter().try_for_each(|w| me.may_manage(w))?;
        }
        let out = self.discard_many(&gone);
        let mut done = vec![];
        for (w, r) in gone.into_iter().zip(out) {
            r?;
            done.push(w);
        }
        Ok(done)
    }

    /// Merges set `set`'s best member (rank 1 of its kept ranking, which mustn't have failed) into `into`, which
    /// must be the set's base, by an ordinary MERGE WORLD run as `agent` (its rights, its merge policy, the merge
    /// checks), then discards the other members. With `dry`, only says what it would do. Returns each member with
    /// what was done to it (`merged`, `discarded`; dry: `would merge`, `would discard`) and how many rows it changed.
    pub fn merge_best(
        &self,
        set: &str,
        into: &str,
        dry: bool,
        agent: Option<&str>,
    ) -> Result<Vec<(String, String, u64)>> {
        let (base, ranked) = self.ranking(set)?;
        let into = self.world(into)?;
        if into.name != base.name {
            return Err(state(
                "0A000",
                format!(
                    "the best of world set {set} merges into its base, {}, not {}: {} then merges as any world does",
                    base.name, into.name, base.name
                ),
            ));
        }
        let Some((winner, _, _)) = ranked.iter().find(|(_, rank, failed)| *rank == 1 && !failed) else {
            return Err(state("55000", format!("world set {set} has no best world: every world in it failed")));
        };
        let rows = |w: &str| -> Result<u64> {
            let br = self.branch(w)?;
            Ok(self.state_diff(&br.base(), &br.snapshot())?.into_iter().filter(|c| shown(&c.key)).count() as u64)
        };
        let me = agent.map(|a| self.as_agent(a)).transpose()?;
        if let Some(me) = &me {
            me.may_merge_into(&winner.name, &base.name)?;
            for (w, ..) in &ranked {
                if w.name != winner.name {
                    me.may_manage(&w.name)?;
                }
            }
        }
        let mut out =
            vec![(winner.name.clone(), if dry { "would merge" } else { "merged" }.to_string(), rows(&winner.name)?)];
        if !dry {
            let mut s = Session::new(&base.name);
            s.agent = agent.map(String::from);
            let merged = s.run(self, &format!("merge world {}", quoted(&winner.name)), &[]);
            s.close(self);
            merged.map_err(sql_error)?;
        }
        let losers: Vec<&String> =
            ranked.iter().filter(|(w, ..)| w.name != winner.name).map(|(w, ..)| &w.name).collect();
        let counts = losers.iter().map(|w| rows(w)).collect::<Result<Vec<_>>>()?;
        if dry {
            out.extend(losers.into_iter().zip(counts).map(|(w, n)| (w.clone(), "would discard".to_string(), n)));
            return Ok(out);
        }
        let gone = self.discard_many(&losers);
        for ((w, n), r) in losers.into_iter().zip(counts).zip(gone) {
            let what = match r {
                Ok(()) => "discarded".to_string(),
                Err(e) => format!("kept: {e}"),
            };
            out.push((w.clone(), what, n));
        }
        Ok(out)
    }

    /// OPTIMIZE: a world set of `spec.base` (named `spec.set`, else the objective's name), `spec.count` members
    /// forked and run as FORK WORLD SET runs them, ranked by the objective (its merge checks failed ranking last),
    /// and with `spec.keep` only the best that many kept (failed ones never). Deterministic: the same inputs give
    /// the same ranking on any number of threads. Returns the ranking, each member saying whether it was kept.
    pub fn optimize(&self, spec: &Optimize) -> Result<Vec<RankedWorld>> {
        let o = self
            .objectives()?
            .into_iter()
            .find(|o| o.name == spec.objective)
            .ok_or_else(|| state("42704", format!("objective {} does not exist", spec.objective)))?;
        let set = spec.set.clone().unwrap_or_else(|| o.name.clone());
        let agent = spec.agent.as_deref();
        self.create_world_set(&set, &spec.base, 0, agent)?;
        let fork = ForkSet {
            set: set.clone(),
            count: spec.count,
            script: spec.script.clone(),
            seed: spec.seed,
            threads: spec.threads,
            agent: spec.agent.clone(),
        };
        let undo = |e: Error| {
            // nothing of a run that stopped stays: its worlds, and the set
            if let Ok((base, members)) = self.world_set(&set) {
                let _ = self.discard_many(&members);
                let _ = self.drop_world_set(&base, &set);
            }
            e
        };
        self.fork_world_set(&fork).map_err(&undo)?;
        let metric = [Metric { query: o.query.clone(), ascending: o.minimize }];
        // no SUBJECT TO CHECKS: none judged (Some of none would be every check: see `rank_world_set`)
        let checks = (!o.checks.is_empty()).then_some(o.checks.as_slice());
        let mut ranked = self.rank_world_set(&set, &metric, checks, agent).map_err(&undo)?;
        if let Some(k) = spec.keep {
            let gone = self.prune_world_set(&set, Prune { rank_gt: Some(k), failed: true }, agent)?;
            for r in &mut ranked {
                r.kept = !gone.contains(&r.world);
            }
        }
        Ok(ranked)
    }

    /// Takes set `set` off base world `base` (its members stay worlds).
    fn drop_world_set(&self, base: &str, set: &str) -> Result<()> {
        let b = self.world(base)?;
        let mut sets = b.meta.get(SET_BASE).and_then(Json::as_object).cloned().unwrap_or_default();
        sets.remove(set);
        self.set_reserved(base, SET_BASE, (!sets.is_empty()).then_some(Json::Object(sets)))
    }
}

/// A world set statement, as parsed (see `sql`).
#[derive(Debug, Clone, PartialEq)]
pub enum SetStmt {
    /// CREATE WORLD SET s FROM WORLD b [WITH (max_live = n)]
    Create { set: String, base: String, max_live: usize },
    /// FORK WORLD SET s COUNT n RUN $$script$$ [SEED k] [THREADS t]
    Fork(ForkSet),
    /// ADOPT WORLD w INTO SET s
    Adopt { world: String, set: String },
    /// COMPARE WORLDS a, b [SUMMARY]
    Compare { a: String, b: String, summary: bool },
    /// RANK WORLD SET s BY ($$metric$$ [ASC | DESC], ...) [WITH CHECKS [(c, ...)]]
    Rank { set: String, metrics: Vec<Metric>, checks: Option<Vec<String>> },
    /// PRUNE WORLD SET s WHERE rank > k | failed | failed OR rank > k
    Prune { set: String, rule: Prune },
    /// MERGE BEST OF SET s INTO b [DRY RUN]
    MergeBest { set: String, into: String, dry: bool },
    /// CREATE [OR REPLACE] OBJECTIVE o MINIMIZE | MAXIMIZE $$query$$ [SUBJECT TO CHECKS (c, ...)]
    CreateObjective { name: String, query: String, minimize: bool, checks: Vec<String>, replace: bool },
    /// DROP OBJECTIVE o
    DropObjective(String),
    /// OPTIMIZE o FROM WORLD b [AS SET s] RUN $$script$$ COUNT n [KEEP k] [SEED s] [THREADS t]
    Optimize(Optimize),
}
