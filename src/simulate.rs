//! Simulations: fork many worlds from one moment of a base world, run the same SQL script in each
//! (in parallel), score each with a query, keep the best and discard the rest. A world's random()
//! and gen_random_uuid() are seeded from the simulation's seed and the world's index, and its
//! clock stands at the base moment, so the same simulation gives the same worlds and scores every
//! time, on any number of threads. Kept worlds record what made them (metadata `sim`), and
//! `replay` makes one again from history to check it still comes out the same.
//!
//!   pin the base (moment t) ─> per batch: fork ─> [rows the base changed since t, written back]
//!     ─> script ─> score ─> keep the best so far, discard the rest ─> next batch
//!   ─> `sim` metadata on the kept worlds

use crate::sql::{self, Reach, Session, Value};
use crate::{Db, Error, Info, Result, State, Val, check_name, metrics, shown};
use serde_json::json;
use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Instant;

/// Worlds forked at once: live worlds stay near `keep` plus this, and losers go early.
const BATCH: usize = 1024;
/// The script and score query are kept in each kept world's metadata (limited to 64 KB), as JSON
/// strings: this bounds their escaped length.
const SCRIPT_MAX: usize = 32 << 10;
/// Worlds one simulation may run: each is reported (and named) up front, so this bounds its memory.
const WORLDS_MAX: usize = 1 << 20;

/// A simulation (see `Db::simulate`); `Simulate::new` fills in the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Simulate {
    /// the world to fork: a name or ID, or a moment of one (`name@when`)
    pub base: String,
    /// how many worlds
    pub worlds: usize,
    /// world i is named `<prefix>_<i>`
    pub prefix: String,
    /// SQL run in each world: $1 is the world's index (0, 1, ...), $2 its seed
    pub script: String,
    /// one SELECT run in each world after the script, giving one number: its score
    pub score: String,
    /// lower scores are better (default: higher)
    pub ascending: bool,
    /// how many worlds to keep, best first; None keeps all (and only then failed ones too)
    pub keep: Option<usize>,
    pub seed: i64,
    /// threads to run worlds on, the caller's included; 0: as many as the shared budget has free
    pub threads: usize,
    /// the agent running it: its rights and quotas apply, and the worlds are its own
    pub agent: Option<String>,
}

impl Simulate {
    pub fn new(base: &str, worlds: usize, prefix: &str, script: &str, score: &str) -> Simulate {
        Simulate {
            base: base.into(),
            worlds,
            prefix: prefix.into(),
            script: script.into(),
            score: score.into(),
            ascending: false,
            keep: None,
            seed: 0,
            threads: 0,
            agent: None,
        }
    }
}

/// One world of a simulation.
#[derive(Debug, Clone, PartialEq)]
pub struct SimWorld {
    pub name: String,
    pub id: String,
    pub index: usize,
    pub seed: i64, // $2: what its random() was seeded with
    pub score: Option<f64>,
    pub error: Option<String>, // why its script or score failed
    pub kept: bool,
}

/// What a simulation did: every world, best first (then failed ones, by index), and timings.
#[derive(Debug, Clone, PartialEq)]
pub struct Simulated {
    pub worlds: Vec<SimWorld>,
    pub base: String,
    pub at: u64, // the base moment (ms since 1970): replay forks the base as it was then
    pub fork_ms: f64,
    pub run_ms: f64, // scripts and scores
    pub discard_ms: f64,
    pub total_ms: f64,
}

/// A world made again from its simulation's inputs (see `Db::replay`).
#[derive(Debug, Clone, PartialEq)]
pub struct Replayed {
    pub world: String,
    pub replay: Option<String>, // the new world, when kept (`as`)
    pub identical: bool,        // same rows, and the same score (or error)
    pub rows_differing: usize,
    pub score: Option<f64>,
    pub recorded: Option<f64>, // the score the simulation gave
    pub error: Option<String>,
}

fn bad(message: String) -> Error {
    Error::Constraint { code: "22023", message }
}

fn sql_error(e: sql::SqlError) -> Error {
    Error::Constraint { code: e.code, message: e.message }
}

/// World `index`'s seed: $2, and what its random() starts from.
fn world_seed(seed: i64, index: usize) -> i64 {
    sql::mix(seed as u64 ^ sql::mix(index as u64)) as i64
}

/// Best first: scores (by `ascending`), ties and failed worlds (last) by index.
fn rank(a: &SimWorld, b: &SimWorld, ascending: bool) -> Ordering {
    let by = match (a.score, b.score) {
        (Some(x), Some(y)) if ascending => x.total_cmp(&y),
        (Some(x), Some(y)) => y.total_cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    by.then(a.index.cmp(&b.index))
}

/// What a script is refused for: statements that leave the world it runs in.
/// `replay` checks a recorded script again the same way: it runs whatever its metadata says.
fn check_sql(spec: &Simulate) -> Result<()> {
    // as kept in the `sim` metadata: escaped, so the metadata fits whatever the script holds
    let json = |s: &str| serde_json::to_string(s).map_or(usize::MAX, |t| t.len());
    if json(&spec.script).saturating_add(json(&spec.score)) > SCRIPT_MAX {
        return Err(bad(format!(
            "a simulation's script and score query are limited to {SCRIPT_MAX} bytes together (as JSON strings)"
        )));
    }
    let script = sql::parse(&spec.script).map_err(sql_error)?;
    if script.iter().any(|s| s.access() == Reach::World) {
        return Err(Error::Constraint {
            code: "0A000",
            message: "a simulation's script runs in its own world: it can't fork, merge, restore, switch or drop \
                      worlds, or change settings"
                .into(),
        });
    }
    let score = sql::parse(&spec.score).map_err(sql_error)?;
    if score.len() != 1 || score[0].access() != Reach::Read {
        return Err(bad(
            "the score query must be one SELECT giving a number, like select sum(total) from orders".into()
        ));
    }
    Ok(())
}

/// The number a score query gave: its first row's first value.
fn score_of(out: Vec<sql::Output>) -> sql::Result<f64> {
    let v = out.into_iter().last().and_then(|o| o.rows.into_iter().next()).and_then(|r| r.into_iter().next());
    match v {
        Some(Value::Float(f)) => Ok(f),
        Some(v) => v.to_json().as_f64().ok_or_else(|| sql::SqlError {
            code: "22023",
            message: format!(
                "the score query gave {}, not a number",
                if v == Value::Null { "null".into() } else { v.to_text() }
            ),
            info: None,
        }),
        None => Err(sql::SqlError { code: "22023", message: "the score query gave no rows".into(), info: None }),
    }
}

/// What a world runs: its script and score query, with its index and seed.
struct Run<'a> {
    script: &'a str,
    score: &'a str,
    seed: i64,
    agent: Option<&'a str>,
    fix: &'a [(String, Option<Val>)], // rows the base changed since the moment, written back first
    clock: i64,                       // now(), in µs: the base moment
    fixed: bool,                      // under `crate::fixed_counters` (MCP safe mode)
}

impl Db {
    /// Forks `spec.worlds` worlds from one moment of `spec.base`, runs `spec.script` in each on
    /// every core (or `spec.threads`), scores each with `spec.score`, and keeps the best
    /// `spec.keep` (discarding the rest). A world whose script or score fails is reported with its
    /// error; the others carry on. Returns every world, best first.
    ///
    /// The same simulation gives the same worlds and scores every time: each world's random() and
    /// gen_random_uuid() are seeded from `spec.seed` and its index, and now() is the base moment.
    /// (Numbers from `serial` columns come from the database's counters, which all worlds share
    /// so merges never collide, so they vary.) Kept
    /// worlds hold what made them in their metadata (`sim`), for `replay`.
    pub fn simulate(&self, spec: &Simulate) -> Result<Simulated> {
        let _t = self.metrics.time(metrics::Op::Simulate);
        let start = Instant::now();
        let n = spec.worlds;
        if n == 0 {
            return Err(bad("a simulation needs at least one world".into()));
        }
        if n > WORLDS_MAX {
            return Err(Error::Constraint {
                code: "54000", // program_limit_exceeded
                message: format!("a simulation may run at most {WORLDS_MAX} worlds, not {n}"),
            });
        }
        check_name(&spec.prefix)?;
        check_sql(spec)?;
        let no_moves = crate::counters_fixed(); // this thread's: the worlds run on others
        let me = spec.agent.as_deref().map(|a| self.as_agent(a)).transpose()?; // what it writes is the agent's
        if let Some(me) = &me {
            me.may_read(&spec.base)?;
            if spec.base.contains('@') && self.branch(&spec.base).is_err() {
                // worlds as the base was: a restore, once merged
                me.need(crate::Can::Restore, "simulate from a past moment")?;
            }
        }
        let (base, base_id, fixed, at) = self.base_of(&spec.base)?;
        if let Some(me) = &me {
            me.may_fork(&base)?;
        }
        let names: Vec<String> = (0..n).map(|i| format!("{}_{i}", spec.prefix)).collect();
        {
            let m = self.branches.read().unwrap();
            if let Some(taken) = names.iter().find(|w| m.contains_key(*w)) {
                return Err(Error::Constraint {
                    code: "42710",
                    message: format!("world {taken} already exists; pick another prefix or drop it"),
                });
            }
        }
        let keep = spec.keep.map_or(n, |k| k.min(n));
        let mut batch = BATCH;
        if let Some(me) = me.as_ref().filter(|me| me.agent.max_worlds > 0) {
            let (most, owned) = (me.agent.max_worlds as usize, me.owned() as usize);
            let room = most.saturating_sub(owned);
            let (need, why) = match spec.keep {
                None => (n, format!("keeping all {n} worlds needs {n}; keep fewer (the rest run in batches)")),
                Some(_) => (keep + 1, format!("keeping {keep} and running more needs {}; keep fewer", keep + 1)),
            };
            if room < need {
                return Err(Error::Constraint {
                    code: "42501",
                    message: format!(
                        "agent {} may own at most {most} worlds at once and owns {owned}: {why}, or raise its max_worlds",
                        me.agent.name
                    ),
                });
            }
            batch = batch.min(room - if spec.keep.is_some() { keep } else { 0 });
        }
        let most = self.setting(crate::Setting::MaxWorlds) as usize;
        if most > 0 {
            let live =
                self.branches.read().unwrap().keys().filter(|w| *w != crate::MAIN && !w.starts_with("_tx_")).count();
            let room = most.saturating_sub(live);
            let need = if spec.keep.is_some() { keep + 1 } else { n };
            if room < need {
                return Err(Error::Constraint {
                    code: "53400", // configuration_limit_exceeded
                    message: format!(
                        "the database may have at most {most} worlds besides main (max_worlds) and has {live}: this \
                         simulation needs {need} at once; keep fewer, or raise max_worlds"
                    ),
                });
            }
            batch = batch.min(room - if spec.keep.is_some() { keep } else { 0 });
        }
        let threads = if spec.threads == 0 { usize::MAX } else { spec.threads };
        let clock = at as i64 * 1000;
        let (mut out, mut alive) = (Vec::with_capacity(n), vec![]);
        let (mut fork_ms, mut run_ms, mut discard_ms) = (0.0, 0.0, 0.0);
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        let mut batches = || -> Result<()> {
            for from in (0..n).step_by(batch) {
                let to = (from + batch).min(n);
                let t = Instant::now();
                let meta = match &me {
                    Some(me) => me.forks(to - from)?,
                    None => Default::default(),
                };
                let infos =
                    (from..to).map(|i| (names[i].clone(), Info { meta: meta.clone(), ..Info::new() })).collect();
                let forked = self.fork_all(&base, infos)?;
                alive.extend(from..to);
                let fix = if forked.same(&fixed) { vec![] } else { self.undo(&forked, &fixed)? };
                fork_ms += ms(t);
                let t = Instant::now();
                let results = sql::par_jobs(to - from, threads, |j, gone| {
                    let i = from + j;
                    let seed = world_seed(spec.seed, i);
                    let run = Run {
                        script: &spec.script,
                        score: &spec.score,
                        seed,
                        agent: spec.agent.as_deref(),
                        fix: &fix,
                        clock,
                        fixed: no_moves,
                    };
                    let (score, error) = self.run_world(&names[i], i, &run, gone);
                    let id = self.branch(&names[i]).map(|b| b.info.read().unwrap().id.clone()).unwrap_or_default();
                    SimWorld { name: names[i].clone(), id, index: i, seed, score, error, kept: false }
                })
                .map_err(sql_error)?;
                out.extend(results);
                run_ms += ms(t);
                if spec.keep.is_some() {
                    // the best `keep` so far stay; the rest (failed worlds too) go
                    let t = Instant::now();
                    let mut scored: Vec<&SimWorld> = out.iter().filter(|w| w.score.is_some()).collect();
                    scored.sort_by(|a, b| rank(a, b, spec.ascending));
                    let best: std::collections::HashSet<usize> = scored.iter().take(keep).map(|w| w.index).collect();
                    let losers: Vec<usize> = alive.iter().copied().filter(|i| !best.contains(i)).collect();
                    let out = self.discard_many(&losers.iter().map(|i| &names[*i]).collect::<Vec<_>>());
                    let stay: std::collections::HashSet<usize> =
                        losers.iter().zip(out).filter(|(_, r)| r.is_err()).map(|(i, _)| *i).collect();
                    alive.retain(|i| best.contains(i) || stay.contains(i));
                    discard_ms += ms(t);
                }
            }
            Ok(())
        };
        if let Err(e) = batches() {
            let alive: Vec<&String> = alive.iter().map(|i| &names[*i]).collect();
            let _ = self.discard_many(&alive); // nothing of a simulation that stopped stays
            return Err(e);
        }
        for &i in &alive {
            let w = &mut out[i];
            w.kept = true;
            let sim = json!({
                "seed": spec.seed, "index": i, "script": spec.script, "score_query": spec.score,
                "score": w.score, "error": w.error, "base": base, "base_id": base_id, "at": at,
            });
            if let Err(e) = self.set_reserved(&w.name, crate::SIM, Some(sim)) {
                let alive: Vec<&String> = alive.iter().map(|i| &names[*i]).collect();
                let _ = self.discard_many(&alive); // a kept world without its inputs can't be replayed
                return Err(e);
            }
        }
        out.sort_by(|a, b| rank(a, b, spec.ascending));
        Ok(Simulated { worlds: out, base, at, fork_ms, run_ms, discard_ms, total_ms: ms(start) })
    }

    /// The base to fork (a live world's name), its ID, its version to simulate from, and when.
    fn base_of(&self, base: &str) -> Result<(String, String, State, u64)> {
        if let Some((name, when)) = base.split_once('@')
            && self.branch(base).is_err()
        {
            let t = sql::moment(when).ok_or_else(|| bad(format!("'{when}' is not a time or a time ago")))?;
            let past = self.past(t)?;
            let (s, id) = past
                .worlds
                .get(name)
                .or_else(|| past.worlds.values().find(|(_, id)| id == name))
                .ok_or_else(|| Error::NoBranch(base.into()))?;
            let live = self.world(id).or_else(|_| self.world(name))?.name; // forked from, then put back as it was
            return Ok((live, id.clone(), s.clone(), t));
        }
        let w = self.world(base)?;
        let (s, t) = self.pinned(&w.name)?;
        Ok((w.name, w.id, s, t))
    }

    /// Runs one world's script and score: its score, or why it failed.
    fn run_world(
        &self,
        name: &str,
        index: usize,
        run: &Run,
        gone: &Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> (Option<f64>, Option<String>) {
        let _as = run.agent.map(crate::acting_as);
        let _fixed = run.fixed.then(crate::fixed_counters);
        // its statements run within the simulation's slot (see `Core::slot`): taking more could
        // fail some worlds and not others, and it'd no longer come out the same every time
        let _within = crate::agents::InSimulation::enter();
        let r = (|| -> sql::Result<f64> {
            if !run.fix.is_empty() {
                let rows: Vec<(&str, Option<&str>)> = run.fix.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
                self.write(name, &rows)?;
            }
            let mut s = Session::new(name);
            s.agent = run.agent.map(String::from);
            s.seed = Some(sql::seeded(run.seed as u64));
            s.clock = Some(run.clock);
            s.gone = Some(gone.clone());
            let params = [Value::Int(index as i64), Value::Int(run.seed)];
            let out = s.run(self, run.script, &params).and_then(|_| s.run(self, run.score, &params));
            s.close(self);
            score_of(out?)
        })();
        match r {
            Ok(score) => (Some(score), None),
            Err(e) => (None, Some(format!("{} (SQLSTATE {})", e.message, e.code))),
        }
    }

    /// Makes world `world` of a simulation again: forks its base as it was at the simulation's
    /// moment (from history), reruns its script and score with the same index and seed, and says
    /// whether the result is identical. The new world is kept as `name`, if given. Needs history
    /// back to that moment: not in memory, nor past the retention window.
    pub fn replay(&self, world: &str, name: Option<&str>, agent: Option<&str>) -> Result<Replayed> {
        let _t = self.metrics.time(metrics::Op::Simulate);
        let w = self.world(world)?;
        let sim = w.meta.get("sim").filter(|s| s.is_object()).cloned();
        let sim =
            sim.ok_or_else(|| bad(format!("world {} wasn't made by SIMULATE: its metadata has no sim", w.name)))?;
        let incomplete = || bad(format!("world {}'s sim metadata is incomplete, so it can't be replayed", w.name));
        let text = |k: &str| sim[k].as_str().map(String::from).ok_or_else(incomplete);
        let (script, score, base, base_id) = (text("script")?, text("score_query")?, text("base")?, text("base_id")?);
        let (Some(seed), Some(index), Some(at)) = (sim["seed"].as_i64(), sim["index"].as_u64(), sim["at"].as_u64())
        else {
            return Err(incomplete());
        };
        // checked as a new simulation's script is: it runs as SIMULATE would run it
        check_sql(&Simulate::new(&base, 1, &w.name, &script, &score))?;
        let me = agent.map(|a| self.as_agent(a)).transpose()?;
        if let Some(me) = &me {
            me.may_read(&w.name)?;
        }
        let past = self.view_at(&base_id, at).map_err(|e| {
            bad(format!(
                "{} can't be replayed: its base {base} as it was at {} can't be read ({e})",
                w.name,
                crate::show_ms(at)
            ))
        })?;
        let from = self.world(&base_id).or_else(|_| self.world(&base)).map_err(|_| {
            bad(format!("{} can't be replayed: its base world {base} is gone, so there's nothing to fork", w.name))
        })?;
        if let Some(me) = &me {
            me.may_fork(&from.name)?;
            // the base as it was, not as it is: a restore, once merged, as simulate() checks
            let now = self.view(&from.name)?.s;
            if !now.same(&past.s) && !self.undo(&now, &past.s)?.is_empty() {
                me.need(crate::Can::Restore, "replay a world from a past moment")?;
            }
        }
        let target = match name {
            Some(n) => {
                check_name(n)?;
                n.to_string()
            }
            None => format!("_replay_{}", &crate::new_world_id()[1..]),
        };
        let meta = match &me {
            Some(me) => me.forks(1)?,
            None => Default::default(),
        };
        let forked = self.fork_all(&from.name, vec![(target.clone(), Info { meta, ..Info::new() })])?;
        let fix = if forked.same(&past.s) { vec![] } else { self.undo(&forked, &past.s)? };
        let seed = world_seed(seed, index as usize);
        let fixed = crate::counters_fixed();
        let run = Run { script: &script, score: &score, seed, agent, fix: &fix, clock: at as i64 * 1000, fixed };
        let ran = sql::par_jobs(1, 1, |_, gone| self.run_world(&target, index as usize, &run, gone)).map_err(sql_error);
        let compared = ran.and_then(|mut r| {
            let (now, again) = (self.branch(&w.name)?.snapshot(), self.branch(&target)?.snapshot());
            let rows = self.state_diff(&now, &again)?.into_iter().filter(|c| shown(&c.key)).count();
            Ok((r.pop().unwrap_or_default(), rows))
        });
        let ((got, error), rows) = match compared {
            Ok(c) => c,
            Err(e) => {
                let _ = self.discard(&target);
                return Err(e);
            }
        };
        let recorded = sim["score"].as_f64();
        let same_score = got.map(f64::to_bits) == recorded.map(f64::to_bits);
        let identical = rows == 0 && same_score && error.as_deref() == sim["error"].as_str();
        match name {
            None => {
                let _ = self.discard(&target);
            }
            Some(_) => {
                let mut again = sim.clone();
                again["score"] = json!(got);
                again["error"] = json!(error);
                self.set_reserved(&target, crate::SIM, Some(again))?;
            }
        }
        Ok(Replayed {
            world: w.name,
            replay: name.map(String::from),
            identical,
            rows_differing: rows,
            score: got,
            recorded,
            error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_and_ranks() {
        assert_ne!(world_seed(42, 0), world_seed(42, 1));
        assert_ne!(world_seed(42, 0), world_seed(43, 0));
        assert_eq!(world_seed(42, 7), world_seed(42, 7));
        let w = |index, score| SimWorld {
            name: String::new(),
            id: String::new(),
            index,
            seed: 0,
            score,
            error: None,
            kept: false,
        };
        let mut v = [w(0, None), w(1, Some(1.0)), w(2, Some(3.0)), w(3, Some(3.0))];
        v.sort_by(|a, b| rank(a, b, false));
        assert_eq!(v.iter().map(|w| w.index).collect::<Vec<_>>(), [2, 3, 1, 0]);
        v.sort_by(|a, b| rank(a, b, true));
        assert_eq!(v.iter().map(|w| w.index).collect::<Vec<_>>(), [1, 2, 3, 0]);
    }
}
