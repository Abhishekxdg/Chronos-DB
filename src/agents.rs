//! Agents: who does what, and what they may do.
//!
//! An agent has a name, an ID, a secret token (only its hash is kept, and a SCRAM-SHA-256 verifier
//! of it for logging in over the Postgres protocol), capabilities and quotas.
//! Agents are rows on main (`_sonos_agents/<name>`), so they're durable, time-travel and can't be
//! written through the JSON or SQL row commands. A world an agent forks is its own: its metadata
//! names the owner (and, with a TTL, when it expires), which no one can change with `set_meta`.
//! Everything an agent writes is logged as its (see `disk::Op::Actor`), and `audit` reads that.
//!
//!   request ─> token ─> agent ─> as_agent(): may it? (capability, owner, quota) ─> the operation,
//!                                             logged as the agent's
//!
//! A merge policy (`_sonos_policies/<name>`) is a set of rules for an agent's merges: a merge that
//! keeps to them goes through on its own; one that breaks any is refused and queued for a person
//! (the world's `_sonos_review` metadata, see `Core::reviews`), who merges or drops it. The rules
//! are checked inside the merge, on the rows it's about to apply, so what was checked is what
//! merges even while the world and its parent keep changing.
//!
//! How those merges end is read back from the log (`merge_outcomes`, beside `audit` on the same walk): a merge
//! refused for a conflict, a stale read or a merge check leaves a count in its world's `_sonos_refusals`
//! metadata (see `Core::note_refusal`), and a hold its `_sonos_review`, so both are in history.

use crate::disk::{self, Op};
use crate::{Core, Error, MAIN, Result, World};
use serde_json::{Value as Json, json};
use std::collections::HashMap;

const AGENTS: &str = "_sonos_agents";
const POLICIES: &str = "_sonos_policies";
/// The longest `world_ttl`: 100 years, in ms (an expiry is now plus it, and shows as an interval).
const WORLD_TTL_MAX: u64 = 36_525 * 86_400_000;

/// Whether `key` is an agent's, a merge policy's, a merge check's or the roles' row (see
/// `Core::undo`: never put back from the past).
pub(crate) fn agent_row(key: &str) -> bool {
    [AGENTS, POLICIES, crate::checks::CHECKS, "_sonos_roles"]
        .iter()
        .any(|p| key.strip_prefix(p).is_some_and(|k| k.starts_with('/')))
}

/// Metadata keys only the database sets (see `Core::set_meta`).
pub(crate) const OWNER: &str = "_sonos_owner";
pub(crate) const EXPIRES: &str = "_sonos_expires";
pub(crate) const CHECKPOINTS: &str = "_sonos_checkpoints";
pub(crate) const PINNED: &str = "_sonos_pinned";
/// A world's last activity, in the metadata on disk only (see `Core::load`).
pub(crate) const ACTIVE: &str = "_sonos_active";
/// Why a world's merge waits for a person (see `Core::reviews`).
pub(crate) const REVIEW: &str = "_sonos_review";
/// How often a world's merge was refused, by kind (see `Core::note_refusal`).
pub(crate) const REFUSALS: &str = "_sonos_refusals";

/// Who `chronos serve --safe` takes a client without an agent's token for: an agent with the
/// default rights (read, fork, change the worlds it forked), shared by all such clients.
pub const GUEST: &str = "guest";
/// Who the database's own background work is logged as: discarding expired and idle worlds.
pub const SYSTEM: &str = "system";

/// Something an agent may be allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Can {
    /// read any world
    Read,
    /// fork worlds (its own from then on)
    Fork,
    /// change the worlds it owns
    WriteOwn,
    /// change any world but main
    Write,
    /// change main
    WriteMain,
    /// merge (publish) the worlds it owns into parents it may change (main needs WriteMain)
    MergeOwn,
    /// merge any world
    Merge,
    /// put a world back as it was (restore, undo a merge)
    Restore,
    /// work main's outbox: claim, acknowledge and retry queued effects (see `outbox`)
    Outbox,
    /// everything, and managing agents
    Admin,
}

impl Can {
    pub const ALL: [Can; 10] = [
        Can::Read,
        Can::Fork,
        Can::WriteOwn,
        Can::Write,
        Can::WriteMain,
        Can::MergeOwn,
        Can::Merge,
        Can::Restore,
        Can::Outbox,
        Can::Admin,
    ];
    /// What a new agent may do unless told otherwise: read, fork, and change its own worlds. A
    /// person reviews its diffs and merges.
    pub const DEFAULT: [Can; 3] = [Can::Read, Can::Fork, Can::WriteOwn];

    pub fn name(self) -> &'static str {
        match self {
            Can::Read => "read",
            Can::Fork => "fork",
            Can::WriteOwn => "write_own",
            Can::Write => "write",
            Can::WriteMain => "write_main",
            Can::MergeOwn => "merge_own",
            Can::Merge => "merge",
            Can::Restore => "restore",
            Can::Outbox => "outbox",
            Can::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Can> {
        Can::ALL.into_iter().find(|c| c.name() == s.trim())
    }
}

/// An agent (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub name: String,
    pub id: String,
    pub can: Vec<Can>,
    /// live worlds it may own at once (0: no limit)
    pub max_worlds: u64,
    /// writes per minute (0: no limit)
    pub writes_per_minute: u64,
    /// rows each of its worlds may change (0: no limit)
    pub max_changes: u64,
    /// how long its worlds live, in ms (0: until merged or discarded)
    pub world_ttl: u64,
    /// the longest any of its statements may run, in ms: its statement_timeout can't go past it (0: no limit)
    pub max_query_ms: u64,
    /// its statements (and HTTP requests) running at once (0: no limit)
    pub max_concurrent: u64,
    /// working memory each of its statements may hold, in MB (0: no limit)
    pub max_memory_mb: u64,
    pub created: u64,
    pub disabled: bool,
    /// the merge policy its merges keep to (see `MergePolicy`); None: its rights alone decide
    pub policy: Option<String>,
    token_hash: String,
    /// its token's SCRAM-SHA-256 verifier (see `sql::scram`), made when the token was issued; None
    /// for an agent made before agents had one, until it logs in with its token (`upgrade_agent`)
    scram: Option<String>,
}

/// What to set when creating or changing an agent: None leaves a field as it is (or at its default).
#[derive(Debug, Clone, Default)]
pub struct AgentSpec {
    pub can: Option<Vec<Can>>,
    pub max_worlds: Option<u64>,
    pub writes_per_minute: Option<u64>,
    pub max_changes: Option<u64>,
    pub world_ttl: Option<u64>,
    pub max_query_ms: Option<u64>,
    pub max_concurrent: Option<u64>,
    pub max_memory_mb: Option<u64>,
    pub disabled: Option<bool>,
    /// a merge policy's name; "" takes the agent's away
    pub policy: Option<String>,
}

impl Agent {
    /// Whether the agent may do `c` (an admin may do anything; a disabled agent nothing).
    pub fn may(&self, c: Can) -> bool {
        !self.disabled && (self.can.contains(&c) || self.can.contains(&Can::Admin))
    }

    fn to_json(&self) -> String {
        json!({
            "id": self.id, "can": self.can.iter().map(|c| c.name()).collect::<Vec<_>>(),
            "max_worlds": self.max_worlds, "writes_per_minute": self.writes_per_minute,
            "max_changes": self.max_changes, "world_ttl": self.world_ttl, "max_query_ms": self.max_query_ms,
            "max_concurrent": self.max_concurrent, "max_memory_mb": self.max_memory_mb, "created": self.created,
            "disabled": self.disabled, "policy": self.policy, "token_hash": self.token_hash, "scram": self.scram,
        })
        .to_string()
    }

    fn from_json(name: &str, raw: &str) -> Option<Agent> {
        let j: Json = serde_json::from_str(raw).ok()?;
        let n = |k: &str| j[k].as_u64().unwrap_or(0);
        Some(Agent {
            name: name.into(),
            id: j["id"].as_str()?.into(),
            can: j["can"].as_array()?.iter().filter_map(|c| Can::parse(c.as_str()?)).collect(),
            max_worlds: n("max_worlds"),
            writes_per_minute: n("writes_per_minute"),
            max_changes: n("max_changes"),
            world_ttl: n("world_ttl"),
            max_query_ms: n("max_query_ms"),
            max_concurrent: n("max_concurrent"),
            max_memory_mb: n("max_memory_mb"),
            created: n("created"),
            disabled: j["disabled"].as_bool().unwrap_or(false),
            policy: j["policy"].as_str().map(String::from),
            token_hash: j["token_hash"].as_str()?.into(),
            scram: j["scram"].as_str().map(String::from),
        })
    }

    /// Safe mode's guest (see `GUEST`): the default rights, no quotas, no token.
    fn guest() -> Agent {
        Agent {
            name: GUEST.into(),
            id: GUEST.into(),
            can: Can::DEFAULT.to_vec(),
            max_worlds: 0,
            writes_per_minute: 0,
            max_changes: 0,
            world_ttl: 0,
            max_query_ms: 0,
            max_concurrent: 0,
            max_memory_mb: 0,
            created: 0,
            disabled: false,
            policy: None,
            token_hash: String::new(), // matches no token
            scram: None,
        }
    }

    /// Its token's SCRAM-SHA-256 verifier, which a Postgres login checks its proof against; None for
    /// an agent made before agents had one (it logs in with its token in the clear, once).
    pub fn verifier(&self) -> Option<crate::sql::scram::Verifier> {
        self.scram.as_deref().and_then(crate::sql::scram::parse)
    }

    fn apply(&mut self, spec: &AgentSpec) -> Result<()> {
        if spec.world_ttl.is_some_and(|t| t > WORLD_TTL_MAX) {
            return Err(bad(format!(
                "world_ttl is at most {WORLD_TTL_MAX} ms (100 years); 0 means worlds never expire"
            )));
        }
        if let Some(c) = &spec.can {
            self.can.clone_from(c);
        }
        self.max_worlds = spec.max_worlds.unwrap_or(self.max_worlds);
        self.writes_per_minute = spec.writes_per_minute.unwrap_or(self.writes_per_minute);
        self.max_changes = spec.max_changes.unwrap_or(self.max_changes);
        self.world_ttl = spec.world_ttl.unwrap_or(self.world_ttl);
        self.max_query_ms = spec.max_query_ms.unwrap_or(self.max_query_ms);
        self.max_concurrent = spec.max_concurrent.unwrap_or(self.max_concurrent);
        self.max_memory_mb = spec.max_memory_mb.unwrap_or(self.max_memory_mb);
        self.disabled = spec.disabled.unwrap_or(self.disabled);
        if let Some(p) = &spec.policy {
            self.policy = (!p.is_empty()).then(|| p.clone());
        }
        Ok(())
    }
}

/// One thing an agent did (see `Core::audit`).
#[derive(Debug, Clone, PartialEq)]
pub struct Audit {
    pub at: u64, // ms since 1970
    pub agent: String,
    pub world: String,
    pub action: String, // write, fork from ..., merge into ..., discard, metadata
    pub rows: usize,
}

/// How one agent's worlds (or one merge policy's agents' worlds) ended within the history window, and how often
/// their merges were held or refused (see `Core::merge_outcomes`). A world counts once, by how it ended.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcome {
    /// the agent (empty: the database's own users), or with `by_policy` the merge policy (empty: none)
    pub name: String,
    /// the agent's merge policy now (by agent only)
    pub policy: String,
    pub worlds: u64,
    /// merged by its owner, who keeps to a merge policy, with no hold
    pub on_rules: u64,
    /// merged by its owner, who keeps to no merge policy
    pub no_policy: u64,
    /// merged by someone else: a person, after a hold or without one
    pub approved: u64,
    /// dropped after a hold
    pub rejected: u64,
    /// dropped with no hold
    pub discarded: u64,
    /// still open, nothing merged
    pub open: u64,
    /// still open, some of it merged (only some rows, or into another world)
    pub partly_merged: u64,
    /// forked before the window: how it ended is counted, not classed
    pub before_window: u64,
    /// merges refused for rows both sides changed
    pub conflicts: u64,
    /// merges refused because rows the world relied on changed since it read them
    pub stale_reads: u64,
    /// merges refused by merge checks or CHECK constraints on the merged result
    pub check_refusals: u64,
    /// merges sent to a person by a merge policy
    pub holds: u64,
}

impl Outcome {
    /// The share of decided worlds that merged on their rules alone, if any were decided by rules or a person.
    pub fn rules_rate(&self) -> Option<f64> {
        let decided = self.on_rules + self.approved + self.rejected;
        (decided > 0).then(|| self.on_rules as f64 / decided as f64)
    }
}

/// `Core::merge_outcomes`: the window it covers and a row per agent or policy.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Outcomes {
    /// when the window starts (ms since 1970): its cut, or the log's first record for a window from the start; 0
    /// with no log (an in-memory database)
    pub since: u64,
    pub rows: Vec<Outcome>,
}

/// One thing done, as `Core::walk` reads it from the log: a transaction's worlds fold into a commit to the
/// world it ran on, and never show.
pub(crate) enum Step {
    /// a transaction committed to this world
    Commit(String),
    /// rows written to a world (the tables' rows, as `Core::history` counts them)
    Write(String, usize),
    /// a world made: forked `from` ("" for an empty one), with its metadata then (JSON; empty if not logged)
    Fork {
        world: String,
        from: String,
        meta: String,
    },
    /// a world merged whole into `into`, and gone
    Merge {
        world: String,
        into: String,
    },
    /// a world merged and kept open: `part` when only some of its rows
    MergeSome {
        world: String,
        into: String,
        part: bool,
    },
    Rebase {
        world: String,
        onto: String,
    },
    Discard(String),
    /// a world's metadata (a JSON object) replaced
    Meta(String, String),
}

/// Where `Core::walk` starts: the oldest retained segment, its time, and the worlds open then.
pub(crate) struct Window {
    seg: u64,
    cut: u64,
    open: Vec<disk::ManBranch>,
}

/// A row quota on a world, for the agent this thread acts for: (agent, most rows).
pub(crate) struct Quota(String, u64);

impl Quota {
    /// Refuses `rows` more changes to a world that has `changed` already.
    pub(crate) fn check(&self, w: &str, changed: u64, rows: u64) -> Result<()> {
        if changed + rows > self.1 {
            return Err(refused(format!("world {w} may change at most {} rows (agent {}'s quota)", self.1, self.0)));
        }
        Ok(())
    }
}

impl Core {
    /// The acting agent's quota on world `w`, if any. Checked where rows are actually written
    /// (`write`, `batch_with`) and merged (`merge_with`), so SQL, restores and transactions count
    /// every row. Main and transactions' own worlds have none: a transaction counts at its commit.
    pub(crate) fn quota(&self, w: &str) -> Result<Option<Quota>> {
        let actor = disk::actor();
        if actor.is_empty() || w == MAIN || w.starts_with("_tx_") {
            return Ok(None);
        }
        let a = self.agent(&actor)?;
        if a.max_changes == 0 {
            return Ok(None);
        }
        self.changed(w)?; // counts what it changed before a restart too
        Ok(Some(Quota(a.name, a.max_changes)))
    }
}

/// The error an agent's merge gets when its merge policy sends it to a person.
pub(crate) fn needs_review(b: &str, policy: &str, reasons: &[String]) -> Error {
    refused(format!(
        "merging {b} needs a person's review (merge policy {policy}: it {}); it's queued (SHOW REVIEWS)",
        reasons.join("; it ")
    ))
}

fn refused(message: String) -> Error {
    Error::Constraint { code: "42501", message } // Postgres's insufficient_privilege
}

fn bad(message: String) -> Error {
    Error::Constraint { code: "22023", message }
}

/// A secret for an agent: 32 random bytes from the operating system, as hex.
fn new_token() -> Result<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(Error::Io)?;
    Ok(format!("chronos_{}", crate::pages::hex(&b)))
}

fn hash(token: &str) -> String {
    crate::pages::hex(blake3::hash(token.as_bytes()).as_bytes())
}

impl Core {
    /// Creates agent `name`; returns it and its token, which is shown only now: keep it.
    pub fn create_agent(&self, name: &str, spec: &AgentSpec) -> Result<(Agent, String)> {
        self.create_agent_with(name, spec, crate::sql::scram::ITERATIONS)
    }

    /// `create_agent`, its token's SCRAM verifier made with `iterations` (a SQL session's
    /// `scram_iterations`).
    pub fn create_agent_with(&self, name: &str, spec: &AgentSpec, iterations: u32) -> Result<(Agent, String)> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(bad(format!("'{name}' can't name an agent: use letters, digits, _ and -")));
        }
        if name == GUEST || name == SYSTEM {
            return Err(bad(format!("'{name}' is a name the database keeps for itself; pick another")));
        }
        if self.agent(name).is_ok() {
            return Err(Error::Exists(format!("agent {name}")));
        }
        // worlds are owned by name: a new agent must not inherit a dropped one's
        let left: Vec<String> =
            self.worlds().into_iter().filter(|w| w.owner.as_deref() == Some(name)).map(|w| w.name).collect();
        if !left.is_empty() {
            return Err(bad(format!(
                "a dropped agent {name} still owns worlds ({}); merge or drop them before reusing the name",
                left.join(", ")
            )));
        }
        let token = new_token()?;
        let scram = crate::sql::scram::fresh(&token, iterations)
            .map_err(|e| Error::Constraint { code: e.code, message: e.message })?;
        let mut a = Agent {
            name: name.into(),
            id: format!("a{}", &crate::pages::hex(blake3::hash(token.as_bytes()).as_bytes())[..16]),
            can: Can::DEFAULT.to_vec(),
            max_worlds: 0,
            writes_per_minute: 0,
            max_changes: 0,
            world_ttl: 0,
            max_query_ms: 0,
            max_concurrent: 0,
            max_memory_mb: 0,
            created: disk::now_ms(),
            disabled: false,
            policy: None,
            token_hash: hash(&token),
            scram: Some(scram),
        };
        a.apply(spec)?;
        self.put_agent(&a)?;
        Ok((a, token))
    }

    /// Gives agent `name` a SCRAM verifier of `token`, once a login has shown `token` is its: an agent
    /// made before agents had verifiers logs in with its token in the clear (it is kept only hashed,
    /// so nothing could be made from it before), and from then on with SCRAM. Nothing changes if it
    /// has one already, or its token isn't `token`. Logged as `SYSTEM`. Whether it was given one.
    pub fn upgrade_agent(&self, name: &str, token: &str) -> Result<bool> {
        let h = hash(token);
        let scram = crate::sql::scram::fresh(token, crate::sql::scram::ITERATIONS)
            .map_err(|e| Error::Constraint { code: e.code, message: e.message })?;
        let _system = disk::acting_as(SYSTEM);
        let key = format!("{AGENTS}/{name}");
        let (_, upgraded) = self.batch_with(MAIN, |v| {
            let a = v.get(&key)?.and_then(|r| Agent::from_json(name, &r));
            let Some(mut a) = a.filter(|a| a.verifier().is_none() && a.token_hash == h) else {
                return Ok((vec![], false));
            };
            a.scram = Some(scram);
            Ok::<_, Error>((vec![(key.clone(), Some(a.to_json()))], true))
        })?;
        Ok(upgraded)
    }

    /// Changes what agent `name` may do; returns it.
    pub fn alter_agent(&self, name: &str, spec: &AgentSpec) -> Result<Agent> {
        let mut a = self.agent(name)?;
        a.apply(spec)?;
        self.put_agent(&a)?;
        Ok(a)
    }

    /// Writes agent `a`'s row, checking in the same step that its merge policy exists: an agent
    /// can't be left naming a policy dropped meanwhile (see `drop_policy`).
    fn put_agent(&self, a: &Agent) -> Result<()> {
        self.batch_with(MAIN, |v| {
            if let Some(p) = &a.policy
                && v.get(&format!("{POLICIES}/{p}"))?.is_none()
            {
                return Err(Error::NoBranch(format!("merge policy {p}")));
            }
            Ok((vec![(format!("{AGENTS}/{}", a.name), Some(a.to_json()))], ()))
        })?;
        Ok(())
    }

    /// Removes agent `name`: its token stops working. Its worlds stay (and keep their owner): the
    /// name can't name a new agent until they're gone (see `create_agent`).
    pub fn drop_agent(&self, name: &str) -> Result<()> {
        self.agent(name)?;
        self.write(MAIN, &[(&format!("{AGENTS}/{name}"), None)])?;
        Ok(())
    }

    /// Agent `name`, or safe mode's guest (see `GUEST`).
    pub fn agent(&self, name: &str) -> Result<Agent> {
        let raw = self.view(MAIN)?.get(&format!("{AGENTS}/{name}"))?;
        match raw.and_then(|r| Agent::from_json(name, &r)) {
            Some(a) => Ok(a),
            None if name == GUEST => Ok(Agent::guest()),
            None => Err(Error::NoBranch(format!("agent {name}"))),
        }
    }

    /// Takes one of agent `a`'s places for a statement or request, held until the `Slot` drops;
    /// refused (53300) while `max_concurrent` of its statements are running. None: no limit.
    pub fn slot(&self, a: &Agent) -> Result<Option<Slot<'_>>> {
        if a.max_concurrent == 0 || IN_SIMULATION.get() {
            return Ok(None);
        }
        let mut running = crate::lock(&self.running);
        let n = running.entry(a.name.clone()).or_default();
        if *n >= a.max_concurrent {
            return Err(Error::Constraint {
                code: "53300", // too_many_connections
                message: format!(
                    "agent {} may run {} statements at once; wait for one to finish",
                    a.name, a.max_concurrent
                ),
            });
        }
        *n += 1;
        Ok(Some(Slot { core: self, agent: a.name.clone() }))
    }

    pub fn agents(&self) -> Result<Vec<Agent>> {
        let prefix = format!("{AGENTS}/");
        let rows = self.view(MAIN)?.prefix(&prefix)?.collect::<Result<Vec<_>>>()?;
        Ok(rows.into_iter().filter_map(|(k, v)| Agent::from_json(&k[prefix.len()..], &v)).collect())
    }

    /// The agent a token belongs to (disabled agents' tokens don't work).
    pub fn agent_by_token(&self, token: &str) -> Result<Agent> {
        let h = hash(token);
        self.agents()?
            .into_iter()
            .find(|a| a.token_hash == h && !a.disabled)
            .ok_or_else(|| refused("no agent has that token".into()))
    }

    /// Acts as agent `name`: what it may do is checked, and what it writes is logged as its.
    /// An undo (`UNDO MERGE`, `UNDO AGENT`) writes world `w` directly: the agent acting, if
    /// any, must also be allowed to write it, as a restore must.
    pub(crate) fn may_undo_into(&self, w: &str) -> Result<()> {
        let actor = disk::actor();
        if actor.is_empty() {
            return Ok(());
        }
        self.as_agent(&actor)?.may_write(w, 0)
    }

    pub fn as_agent(&self, name: &str) -> Result<Acting<'_>> {
        let agent = self.agent(name)?;
        if agent.disabled {
            return Err(refused(format!("agent {name} is disabled")));
        }
        Ok(Acting { core: self, _as: disk::acting_as(name), agent })
    }

    /// Marks world `w` as it is now with a name, to read or restore later (`w@name` doesn't
    /// exist: use `checkpoint_time`, then time travel).
    pub fn checkpoint_world(&self, w: &str, name: &str) -> Result<u64> {
        let at = disk::now_ms();
        std::thread::sleep(std::time::Duration::from_millis(1)); // later writes get a later time
        let mut marks = self.world(w)?.checkpoints;
        marks.retain(|(n, _)| n != name);
        marks.push((name.into(), at));
        let marks: serde_json::Map<String, Json> = marks.into_iter().map(|(n, t)| (n, json!(t))).collect();
        self.set_reserved(w, CHECKPOINTS, Some(Json::Object(marks)))?;
        Ok(at)
    }

    /// When world `w`'s checkpoint `name` was taken (ms since 1970).
    pub fn checkpoint_time(&self, w: &str, name: &str) -> Result<u64> {
        let w = self.world(w)?;
        w.checkpoints
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| *t)
            .ok_or_else(|| bad(format!("world {} has no checkpoint '{name}'", w.name)))
    }

    /// Discards worlds whose time is up (an agent's `world_ttl`, or `expire`), and worlds unused
    /// for longer than `world_idle_ttl` unless pinned; never main, or a world with forks of its
    /// own then. Forks first, so an idle parent can follow its idle forks. Runs in the
    /// background, logged as `SYSTEM`; returns how many went.
    pub fn expire_worlds(&self) -> Result<usize> {
        let _system = disk::acting_as(SYSTEM);
        let (now, idle) = (disk::now_ms(), self.setting(crate::Setting::WorldIdleTtl));
        let due: Vec<String> = self
            .worlds()
            .into_iter()
            .filter(|w| w.name != MAIN && !w.name.starts_with("_tx_"))
            .filter(|w| {
                w.expires.is_some_and(|e| e <= now)
                    || (idle > 0 && !w.pinned && w.parent.is_some() && w.active.saturating_add(idle) <= now)
            })
            .map(|w| w.name)
            .collect();
        // a world with forks is refused, atomically against a fork racing this
        Ok(self.discard_many(&due).iter().filter(|r| r.is_ok()).count())
    }

    /// Where the history window starts, or None with no log or no history yet.
    pub(crate) fn window(&self) -> Result<Option<Window>> {
        let Some(log) = &self.log else { return Ok(None) };
        let hist = disk::history(&log.dir)?;
        let Some((seg, cut, path)) = hist.first() else { return Ok(None) };
        let (_, _, open) =
            disk::read_manifest_at(path, log.crypt.as_deref())?.ok_or_else(|| bad("history is missing".into()))?;
        Ok(Some(Window { seg: *seg, cut: *cut, open }))
    }

    /// Reads the log from `win` on, in order, and hands `visit` each step with when (ms since 1970) and by whom
    /// (an agent, or empty for the database's own users).
    // ponytail: reads the whole retained log per call, and lasts as long as history does
    pub(crate) fn walk(&self, win: &Window, mut visit: impl FnMut(u64, &str, Step)) -> Result<()> {
        let Some(log) = &self.log else { return Ok(()) };
        let mut parents: HashMap<String, Option<String>> =
            win.open.iter().map(|m| (m.name.clone(), m.parent.clone())).collect();
        let (mut at, mut who) = (win.cut, String::new());
        self.flush_log(log);
        disk::read_log(&log.dir, log.crypt.as_deref(), win.seg, |_, op| {
            let step = match op {
                Op::Clock(c) => {
                    at = c;
                    None
                }
                Op::Actor(a) => {
                    who = a;
                    None
                }
                // a session's transaction world is its own: its writes show as a commit to the
                // world it was on, and its name never shows
                Op::Write(w, _) | Op::Bulk(w, ..) | Op::Meta(w, _) if w.starts_with("_tx_") => None,
                Op::Discard(w) if w.starts_with("_tx_") => {
                    parents.remove(&w);
                    None
                }
                Op::Fork(from, name) | Op::World(from, name, ..) if name.starts_with("_tx_") => {
                    parents.insert(name, Some(from));
                    None
                }
                Op::Merge(w, ..) | Op::MergeWith(w, ..) | Op::MergeSome { branch: w, .. } if w.starts_with("_tx_") => {
                    let p = parents.remove(&w).flatten().unwrap_or_default();
                    (!p.starts_with("_tx_")).then_some(Step::Commit(p)) // (a savepoint's release stays inside)
                }
                Op::Write(w, rows) => {
                    let n = rows.iter().filter(|(k, _)| !crate::reserved_table(k)).count();
                    Some(Step::Write(w, n))
                }
                Op::Bulk(w, rows, ..) => Some(Step::Write(w, rows as usize)),
                Op::Fork(from, name) => {
                    parents.insert(name.clone(), (!from.is_empty()).then(|| from.clone()));
                    Some(Step::Fork { world: name, from, meta: String::new() })
                }
                Op::World(from, name, _, _, meta) => {
                    parents.insert(name.clone(), (!from.is_empty()).then(|| from.clone()));
                    Some(Step::Fork { world: name, from, meta })
                }
                Op::Merge(w, ..) | Op::MergeWith(w, ..) => {
                    let into = parents.remove(&w).flatten().unwrap_or_default();
                    Some(Step::Merge { world: w, into })
                }
                Op::MergeSome { branch: w, into, tables, keys, .. } => {
                    let into = match into.is_empty() {
                        true => parents.get(&w).cloned().flatten().unwrap_or_default(),
                        false => into,
                    };
                    let part = !tables.is_empty() || !keys.is_empty();
                    Some(Step::MergeSome { world: w, into, part })
                }
                Op::Rebase(w, ..) => {
                    // the world stays open, on a new fork point
                    let onto = parents.get(&w).cloned().flatten().unwrap_or_default();
                    Some(Step::Rebase { world: w, onto })
                }
                Op::Discard(w) => {
                    parents.remove(&w);
                    Some(Step::Discard(w))
                }
                Op::Meta(w, meta) => Some(Step::Meta(w, meta)),
                _ => None,
            };
            if let Some(step) = step {
                visit(at, &who, step);
            }
            true
        })?;
        Ok(())
    }

    /// What agents did within the history window, newest first (optionally only `agent`'s).
    pub fn audit(&self, agent: Option<&str>, limit: usize) -> Result<Vec<Audit>> {
        let Some(win) = self.window()? else { return Ok(vec![]) };
        let mut out = vec![];
        self.walk(&win, |at, who, step| {
            if who.is_empty() || agent.is_some_and(|a| a != who) {
                return;
            }
            let (world, action, rows) = match step {
                Step::Commit(w) => (w, "commit".into(), 0),
                Step::Write(w, n) => (w, "write".into(), n),
                Step::Fork { world, from, .. } => {
                    (world, if from.is_empty() { "create empty".into() } else { format!("fork from {from}") }, 0)
                }
                Step::Merge { world, into } => (world, format!("merge into {into}"), 0),
                Step::MergeSome { world, into, part } => {
                    (world, format!("merge {}into {into}", if part { "part " } else { "" }), 0)
                }
                Step::Rebase { world, onto } => (world, format!("rebase onto {onto}"), 0),
                Step::Discard(w) => (w, "discard".into(), 0),
                Step::Meta(w, _) => (w, "metadata".into(), 0),
            };
            out.push(Audit { at, agent: who.into(), world, action, rows });
        })?;
        out.sort_by_key(|e| e.at); // a batch of writes is logged after records from later
        out.reverse();
        out.truncate(limit);
        Ok(out)
    }

    /// How worlds ended within the history window, and how often their merges were held or refused: a row per
    /// owner (agent, or empty for the database's own users), or with `by_policy` per owner's merge policy now,
    /// most worlds first. Merges later undone are not told apart (`UNDO MERGE` is logged as an ordinary write).
    // gstack-shortcut: D13 (2026-10-14) leaves undone merges out; revisit when a pass rate is quoted
    // publicly or a tester first uses UNDO MERGE
    pub fn merge_outcomes(&self, by_policy: bool) -> Result<Outcomes> {
        let Some(win) = self.window()? else { return Ok(Outcomes::default()) };
        #[derive(Default)]
        struct Seen {
            owner: String,
            before_window: bool,
            held: bool,
            review_at: Option<u64>,
            holds: u64,
            refusals: [u64; 3], // conflicts, stale reads, checks: as the world's metadata last said
            refused_before: [u64; 3],
            merged_by: Option<String>,
            part: bool,
            dropped: bool,
        }
        let refusals = |meta: &Json| -> [u64; 3] {
            let r = &meta[REFUSALS];
            ["conflicts", "stale_reads", "checks"].map(|k| r[k].as_u64().unwrap_or(0))
        };
        let review_at = |meta: &Json| meta[REVIEW]["at"].as_u64();
        let mut worlds: HashMap<String, Seen> = HashMap::new();
        for m in win.open.iter().filter(|m| m.name != MAIN && !m.name.starts_with("_tx_")) {
            let meta: Json = serde_json::from_str(&m.meta).unwrap_or(Json::Null);
            let r = refusals(&meta);
            let at = review_at(&meta);
            let seen = Seen {
                owner: meta[OWNER].as_str().unwrap_or_default().into(),
                before_window: true,
                held: at.is_some(),
                review_at: at,
                refusals: r,
                refused_before: r,
                ..Default::default()
            };
            worlds.insert(m.name.clone(), seen);
        }
        let mut first = None; // when the log starts, for a window that starts at the database's beginning (cut 0)
        self.walk(&win, |at, who, step| {
            if first.is_none() && at > 0 {
                first = Some(at);
            }
            match step {
                Step::Fork { world, meta, .. } if world != MAIN => {
                    let meta: Json = serde_json::from_str(&meta).unwrap_or(Json::Null);
                    let owner = meta[OWNER].as_str().map_or_else(|| who.to_string(), String::from);
                    worlds.insert(world, Seen { owner, ..Default::default() });
                }
                Step::Meta(w, meta) => {
                    let Some(s) = worlds.get_mut(&w) else { return };
                    let meta: Json = serde_json::from_str(&meta).unwrap_or(Json::Null);
                    if let Some(o) = meta[OWNER].as_str() {
                        s.owner = o.into();
                    }
                    let at = review_at(&meta);
                    if at.is_some() && at != s.review_at {
                        s.holds += 1;
                        s.held = true;
                    }
                    s.review_at = at;
                    for (have, now) in s.refusals.iter_mut().zip(refusals(&meta)) {
                        *have = (*have).max(now);
                    }
                }
                Step::Merge { world, .. } => {
                    if let Some(s) = worlds.get_mut(&world) {
                        s.merged_by = Some(who.into());
                    }
                }
                Step::MergeSome { world, .. } => {
                    if let Some(s) = worlds.get_mut(&world) {
                        s.part = true;
                    }
                }
                Step::Discard(w) => {
                    if let Some(s) = worlds.get_mut(&w) {
                        s.dropped = true;
                    }
                }
                _ => {}
            }
        })?;
        // each owner's policy as it is now (an agent since dropped has none)
        let mut policies: HashMap<String, String> = HashMap::new();
        let mut policy = |owner: &str| -> String {
            if owner.is_empty() {
                return String::new();
            }
            policies
                .entry(owner.into())
                .or_insert_with(|| self.agent(owner).ok().and_then(|a| a.policy).unwrap_or_default())
                .clone()
        };
        let mut rows: HashMap<String, Outcome> = HashMap::new();
        for s in worlds.into_values() {
            let p = policy(&s.owner);
            let key = if by_policy { p.clone() } else { s.owner.clone() };
            let o = rows.entry(key.clone()).or_insert_with(|| Outcome {
                name: key,
                policy: if by_policy { String::new() } else { p.clone() },
                ..Default::default()
            });
            o.worlds += 1;
            match &s.merged_by {
                _ if s.before_window => o.before_window += 1,
                Some(by) if *by != s.owner => o.approved += 1,
                Some(_) if s.held || p.is_empty() => {
                    // a hold the owner merged past (its policy loosened since) is a person's doing too
                    if s.held { o.approved += 1 } else { o.no_policy += 1 }
                }
                Some(_) => o.on_rules += 1,
                None if s.dropped && s.held => o.rejected += 1,
                None if s.dropped => o.discarded += 1,
                None if s.part => o.partly_merged += 1,
                None => o.open += 1,
            }
            o.conflicts += s.refusals[0] - s.refused_before[0].min(s.refusals[0]);
            o.stale_reads += s.refusals[1] - s.refused_before[1].min(s.refusals[1]);
            o.check_refusals += s.refusals[2] - s.refused_before[2].min(s.refusals[2]);
            o.holds += s.holds;
        }
        let mut rows: Vec<Outcome> = rows.into_values().collect();
        rows.sort_by(|a, b| b.worlds.cmp(&a.worlds).then_with(|| a.name.cmp(&b.name)));
        let since = if win.cut > 0 { win.cut } else { first.unwrap_or_else(disk::now_ms) };
        Ok(Outcomes { since, rows })
    }

    /// Counts a refused merge of world `b` in its metadata (`REFUSALS`), by kind: rows both sides changed, rows
    /// it relied on changed since (a stale read), or a merge check or CHECK constraint the result breaks. Not a
    /// transaction's world (its COMMIT conflicting is no agent's merge), and not a hold (`REVIEW` records those).
    /// A count that can't be written is let go: the merge's own error is what the caller gets.
    pub(crate) fn note_refusal(&self, b: &str, e: &Error) {
        let kind = match e {
            _ if b.starts_with("_tx_") => return,
            Error::Conflicts(_) => "conflicts",
            Error::Constraint { code: "40001", message } if message.starts_with("both sides changed table") => {
                "conflicts"
            }
            Error::Constraint { code: "40001", .. } => "stale_reads",
            Error::Constraint { code: "23514", .. } => "checks",
            _ => return,
        };
        let _ = self.put_meta(b, |meta| {
            let r = meta.entry(REFUSALS).or_insert_with(|| json!({}));
            r[kind] = json!(r[kind].as_u64().unwrap_or(0) + 1);
            Ok(())
        });
    }
}

/// Rules for an agent's merges (see the module docs). A limit left out doesn't limit.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MergePolicy {
    pub name: String,
    /// most rows one merge may change
    pub max_rows: Option<u64>,
    /// most rows one merge may delete
    pub max_deletes: Option<u64>,
    /// most rows an agent's merges into main may change in an hour, this one's included (see `tally`)
    pub rows_per_hour: Option<u64>,
    /// most rows an agent's merges into main may delete in an hour, this one's included
    pub deletes_per_hour: Option<u64>,
    /// the oldest a world may be (ms since its fork) and still merge on its own
    pub max_age: Option<u64>,
    /// most effects one merge may queue (NOTIFY ON MERGE: see `outbox`)
    pub max_effects: Option<u64>,
    /// most effects an agent's merges into main may queue in an hour, this one's included
    pub effects_per_hour: Option<u64>,
    /// channels every effect queued on waits for a person
    pub review_channels: Vec<String>,
    /// the only tables a merge may change (empty: any)
    pub tables: Vec<String>,
    /// tables every change to waits for a person
    pub review_tables: Vec<String>,
    /// columns (table, column) every change to waits for a person, even in one row
    pub review_columns: Vec<(String, String)>,
    /// whether a merge may change tables themselves, views, functions, sequences, schemas or types
    pub schema: bool,
    /// whether a merge may overwrite rows its parent changed since the fork (by OURS or picking
    /// rows); rows combined by columns overwrite nothing
    pub overwrite: bool,
    /// whether a merge may change columns a reader marked critical reads (see `MARK READER`)
    pub critical: bool,
    /// whether its agents' worlds keep what they read, and a merge whose reads changed since the
    /// fork waits for a person (see `stale`)
    pub check_reads: bool,
    /// whether its agents must declare what a world may change when forking it (`may_change` or
    /// `tenant`: see `scope`)
    pub require_scope: bool,
    pub created: u64,
}

/// A merge waiting for a person (see `Core::reviews`).
#[derive(Debug, Clone, PartialEq)]
pub struct Review {
    pub world: String,
    pub owner: Option<String>,
    pub policy: String,
    /// the rules the merge broke
    pub reasons: Vec<String>,
    /// when the agent asked (ms since 1970)
    pub at: u64,
    /// the world's version then, and now: a person reviewing it sees whether it changed since
    pub version: u64,
    pub now: u64,
    /// part of it was merged since the agent asked (MERGE ... ONLY TABLES / ONLY KEYS): what's
    /// left isn't what was refused. Kept apart from `version`, which clients merge at
    pub partly_merged: bool,
    /// what the world was forked to change (see `scope`)
    pub scope: Option<Json>,
}

impl Review {
    /// Whether the world isn't as it was when its agent asked: written to, or partly merged.
    pub fn changed_since(&self) -> bool {
        self.now != self.version || self.partly_merged
    }
}

impl MergePolicy {
    /// Sets what `j` says: `max_rows` and `max_deletes` as numbers (null: no limit), `tables` and
    /// `review_tables` (and `review_channels`) as lists (`'a,b'` or an array; a name is folded to
    /// lowercase unless quoted, as in SQL), `schema` and `overwrite` as booleans.
    fn apply(&mut self, j: &Json) -> Result<()> {
        let list = |k: &str, v: &Json| -> Result<Vec<String>> {
            let wrong = || {
                let what = if k == "review_channels" {
                    "channel names like 'billing,email'"
                } else {
                    "table names like 'orders,items'"
                };
                bad(format!("{k} must be a list of {what} (quote a name as SQL does: '\"Orders\"')"))
            };
            match v {
                Json::Null => Ok(vec![]),
                Json::String(s) => table_names(s).ok_or_else(wrong),
                // an entry is one name each
                Json::Array(a) => a
                    .iter()
                    .map(|t| match t.as_str().and_then(table_names).as_deref() {
                        Some([one]) => Ok(one.clone()),
                        _ => Err(wrong()),
                    })
                    .collect(),
                _ => Err(wrong()),
            }
        };
        let m = match j {
            Json::Object(m) => m,
            Json::Null => return Ok(()), // no settings: the defaults
            _ => return Err(bad("a merge policy's settings must be a JSON object".into())),
        };
        for (k, v) in m {
            let limit = || match v {
                Json::Null => Ok(None),
                v => v.as_u64().map(Some).ok_or_else(|| bad(format!("{k} must be a number, or null for no limit"))),
            };
            let flag = || v.as_bool().ok_or_else(|| bad(format!("{k} must be true or false")));
            match k.as_str() {
                "max_rows" => self.max_rows = limit()?,
                "max_deletes" => self.max_deletes = limit()?,
                "rows_per_hour" => self.rows_per_hour = limit()?,
                "deletes_per_hour" => self.deletes_per_hour = limit()?,
                "max_effects" => self.max_effects = limit()?,
                "effects_per_hour" => self.effects_per_hour = limit()?,
                "review_channels" => self.review_channels = list(k, v)?,
                "max_age" => {
                    self.max_age = match v {
                        Json::Null => None,
                        Json::String(s) => Some(crate::sql::duration_ms(s).filter(|ms| *ms > 0).ok_or_else(|| {
                            bad(format!("max_age must be a length of time like '1h' or '30 minutes', not '{s}'"))
                        })?),
                        v => Some(v.as_u64().filter(|ms| *ms > 0).ok_or_else(|| {
                            bad("max_age must be a length of time like '1h', ms as a number, or null".into())
                        })?),
                    }
                }
                "tables" => self.tables = list(k, v)?,
                "review_tables" => self.review_tables = list(k, v)?,
                "review_columns" => self.review_columns = columns(v)?,
                "schema" => self.schema = flag()?,
                "overwrite" => self.overwrite = flag()?,
                "critical" => self.critical = flag()?,
                "check_reads" => self.check_reads = flag()?,
                "require_scope" => self.require_scope = flag()?,
                k => {
                    return Err(bad(format!(
                        "merge policies have no setting {k} (max_rows, max_deletes, rows_per_hour, deletes_per_hour, max_age, max_effects, effects_per_hour, review_channels, tables, review_tables, review_columns, schema, overwrite, critical, check_reads, require_scope)"
                    )));
                }
            }
        }
        Ok(())
    }

    fn to_json(&self) -> String {
        json!({
            "max_rows": self.max_rows, "max_deletes": self.max_deletes, "tables": self.tables,
            "rows_per_hour": self.rows_per_hour, "deletes_per_hour": self.deletes_per_hour, "max_age": self.max_age,
            "max_effects": self.max_effects, "effects_per_hour": self.effects_per_hour,
            "review_channels": self.review_channels,
            "review_tables": self.review_tables, "review_columns": self.review_columns, "schema": self.schema, "overwrite": self.overwrite,
            "critical": self.critical, "check_reads": self.check_reads,
            "require_scope": self.require_scope, "created": self.created,
        })
        .to_string()
    }

    fn from_json(name: &str, raw: &str) -> Option<MergePolicy> {
        let j: Json = serde_json::from_str(raw).ok()?;
        let list =
            |k: &str| j[k].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)).collect();
        Some(MergePolicy {
            name: name.into(),
            max_rows: j["max_rows"].as_u64(),
            max_deletes: j["max_deletes"].as_u64(),
            rows_per_hour: j["rows_per_hour"].as_u64(),
            deletes_per_hour: j["deletes_per_hour"].as_u64(),
            max_age: j["max_age"].as_u64(),
            max_effects: j["max_effects"].as_u64(),
            effects_per_hour: j["effects_per_hour"].as_u64(),
            review_channels: list("review_channels"),
            tables: list("tables"),
            review_tables: list("review_tables"),
            review_columns: j["review_columns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string())))
                .collect(),
            schema: j["schema"].as_bool().unwrap_or(false),
            overwrite: j["overwrite"].as_bool().unwrap_or(false),
            critical: j["critical"].as_bool().unwrap_or(false),
            check_reads: j["check_reads"].as_bool().unwrap_or(false),
            require_scope: j["require_scope"].as_bool().unwrap_or(false),
            created: j["created"].as_u64().unwrap_or(0),
        })
    }

    /// The `review_columns` that `rows` (a world's own changes, as JSON: see `sql::describe`)
    /// change, as one reason naming them and their rows; None when it changes none. A column
    /// changes when an update changes it, or an insert gives it (a delete takes) a value.
    pub(crate) fn column_reviews(&self, rows: &[crate::sql::Described]) -> Option<String> {
        use std::collections::BTreeSet;
        const SHOWN: usize = 5;
        let (mut cols, mut keys) = (BTreeSet::new(), vec![]);
        for d in rows.iter().filter(|d| d.id.is_some()) {
            let has = |row: &Option<Json>, c: &str| row.as_ref().is_some_and(|r| !r[c].is_null());
            let hit: Vec<String> = self
                .review_columns
                .iter()
                .filter(|(t, c)| {
                    *t == d.table
                        && match d.change {
                            "update" => d.columns.contains(c),
                            "insert" => has(&d.after, c),
                            _ => has(&d.before, c),
                        }
                })
                .map(|(t, c)| format!("{t}.{c}"))
                .collect();
            if !hit.is_empty() {
                cols.extend(hit);
                keys.push(crate::sql::show_key(&d.key).into_owned());
            }
        }
        if keys.is_empty() {
            return None;
        }
        let more = if keys.len() > SHOWN { format!(", and {} more", keys.len() - SHOWN) } else { String::new() };
        let cols: Vec<String> = cols.into_iter().collect();
        Some(format!(
            "changes {} (always reviewed): {}{more}",
            cols.join(", "),
            keys[..keys.len().min(SHOWN)].join(", ")
        ))
    }

    /// Whether a world forked at `created` (ms since 1970) is past `max_age` at `now`: its age, in
    /// words, as the reason.
    pub(crate) fn age_broken(&self, created: u64, now: u64) -> Option<String> {
        let most = self.max_age?;
        let age = now.saturating_sub(created);
        (created > 0 && age > most).then(|| format!("was forked {} ago (at most {})", words(age), words(most)))
    }

    /// The hourly limits a merge of `diff` into main by agent `agent` breaks, counted with the
    /// agent's other merges into main in the last hour (see `tally`), in words.
    pub(crate) fn hourly_broken(&self, core: &Core, agent: &str, diff: &[crate::Change]) -> Vec<String> {
        if self.rows_per_hour.is_none() && self.deletes_per_hour.is_none() && self.effects_per_hour.is_none() {
            return vec![];
        }
        let (rows, deletes, effects) = counts(diff);
        let (had_rows, had_deletes, had_effects) = core.hourly(agent);
        let mut out = vec![];
        if let Some(n) = self.rows_per_hour.filter(|n| had_rows + rows > *n) {
            out.push(format!(
                "changes {rows} rows, and its merges changed {had_rows} in the last hour (at most {n} an hour)"
            ));
        }
        if let Some(n) = self.deletes_per_hour.filter(|n| had_deletes + deletes > *n) {
            out.push(format!(
                "deletes {deletes} rows, and its merges deleted {had_deletes} in the last hour (at most {n} an hour)"
            ));
        }
        if let Some(n) = self.effects_per_hour.filter(|n| had_effects + effects > *n) {
            out.push(format!(
                "queues {effects} effects, and its merges queued {had_effects} in the last hour (at most {n} an hour)"
            ));
        }
        out
    }

    /// The rules a merge of `diff` breaks, in words (none: it may go through). `overwrote`: rows
    /// it would overwrite that its parent changed since the fork.
    pub(crate) fn broken(&self, diff: &[crate::Change], overwrote: usize) -> Vec<String> {
        use std::collections::BTreeSet;
        let (mut rows, mut deletes, mut effects, mut taken) = (0u64, 0u64, 0u64, 0u64);
        let (mut outside, mut review, mut schema) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        let mut channels = BTreeSet::new();
        for c in diff {
            let (kind, name) = c.key.split_once('/').unwrap_or((&c.key, ""));
            let table = match kind {
                // the database's own rows behind indexes, constraints and counters follow the
                // rows they're for
                _ if crate::sql::internal(&c.key) => continue,
                // effects queued for main (see `outbox`): no rows or schema of the parent change, but
                // things that happen outside the database once they land, so they're counted too
                _ if crate::outbox::entry(&c.key) => {
                    match (&c.before, &c.after) {
                        (None, Some(v)) => {
                            effects += 1;
                            if let Some(ch) =
                                crate::outbox::channel_of(v).filter(|ch| self.review_channels.contains(ch))
                            {
                                channels.insert(ch);
                            }
                        }
                        // one its parent queued already: taken back or changed, its worker never told
                        _ => taken += 1,
                    }
                    continue;
                }
                "_sonos_tables" => {
                    schema.insert(format!("table {name}"));
                    name
                }
                // any other row of its own defines something: a view, function, sequence, schema,
                // type, and whatever comes later (so a new kind is a schema change until it's said
                // otherwise here)
                k if crate::reserved_table(k) => {
                    schema.insert(format!("{} {name}", k.trim_start_matches("_sonos_").trim_end_matches('s')));
                    continue;
                }
                t => {
                    rows += 1;
                    deletes += c.after.is_none() as u64;
                    t
                }
            };
            let t = table.to_string(); // as stored: `"Orders"` and `orders` are two tables
            if !self.tables.is_empty() && !self.tables.contains(&t) {
                outside.insert(t.clone());
            }
            if self.review_tables.contains(&t) {
                review.insert(t);
            }
        }
        let join = |s: BTreeSet<String>| s.into_iter().collect::<Vec<_>>().join(", ");
        let mut out = vec![];
        if let Some(n) = self.max_rows.filter(|n| rows > *n) {
            out.push(format!("changes {rows} rows (at most {n})"));
        }
        if let Some(n) = self.max_deletes.filter(|n| deletes > *n) {
            out.push(format!("deletes {deletes} rows (at most {n})"));
        }
        if let Some(n) = self.max_effects.filter(|n| effects > *n) {
            out.push(format!("queues {effects} effects (at most {n})"));
        }
        if !channels.is_empty() {
            out.push(format!("queues effects on {} (always reviewed)", join(channels)));
        }
        if taken > 0 {
            out.push(format!("takes back or changes {taken} effects already queued in its parent"));
        }
        if !outside.is_empty() {
            out.push(format!("changes {} (only {})", join(outside), listed(&self.tables)));
        }
        if !review.is_empty() {
            out.push(format!("changes {} (always reviewed)", join(review)));
        }
        if !self.schema && !schema.is_empty() {
            out.push(format!("changes the schema: {}", join(schema)));
        }
        if !self.overwrite && overwrote > 0 {
            out.push(format!("overwrites {overwrote} rows changed in its parent since the fork"));
        }
        out
    }
}

/// A comma-separated list of table names as SQL reads them: unquoted names fold to lowercase;
/// quoted ones keep their case and may hold commas and spaces (`""` is a quote in one). None: a
/// quote left open.
fn table_names(s: &str) -> Option<Vec<String>> {
    let (mut out, mut name, mut quoted) = (vec![], String::new(), false);
    let mut cs = s.chars().peekable();
    while let Some(c) = cs.next() {
        match c {
            '"' if quoted && cs.peek() == Some(&'"') => {
                cs.next();
                name.push('"');
            }
            '"' => quoted = !quoted,
            c if quoted => name.push(c),
            ',' => out.push(std::mem::take(&mut name)),
            c if c.is_whitespace() => {}
            c => name.extend(c.to_lowercase()),
        }
    }
    if quoted {
        return None;
    }
    out.push(name);
    out.retain(|n| !n.is_empty());
    Some(out)
}

/// Table names as a policy's settings take them back: `orders,"Orders"`.
/// `review_columns`' list: text like `'users.email, accounts."Owner"'` (names as SQL reads them),
/// or an array of such names, one each; null for none.
fn columns(v: &Json) -> Result<Vec<(String, String)>> {
    let wrong = || {
        bad("review_columns must be a list of columns like 'users.email,accounts.owner' (table.column, quoted as SQL does)"
            .into())
    };
    let items: Vec<String> = match v {
        Json::Null => return Ok(vec![]),
        Json::String(s) => crate::scope::split(s, ',').ok_or_else(wrong)?,
        Json::Array(a) => a.iter().map(|x| x.as_str().map(String::from)).collect::<Option<_>>().ok_or_else(wrong)?,
        _ => return Err(wrong()),
    };
    let mut out = vec![];
    for item in items.iter().filter(|i| !i.trim().is_empty()) {
        let parts = crate::scope::split(item.trim(), '.').filter(|p| p.len() >= 2).ok_or_else(wrong)?;
        let parts: Vec<String> = parts.iter().map(|p| crate::scope::name(p)).collect();
        // '.owner', 'leads.' or 'leads..owner' name no column: it would never match
        if parts.iter().any(String::is_empty) {
            return Err(wrong());
        }
        let (column, table) = parts.split_last().ok_or_else(wrong)?;
        out.push((table.join("."), column.clone()));
    }
    Ok(out)
}

/// Columns as `review_columns` takes them back: `users.email,"Accounts"."Owner"`.
pub fn listed_columns(cols: &[(String, String)]) -> String {
    cols.iter()
        .map(|(t, c)| format!("{}.{}", listed(std::slice::from_ref(t)), listed(std::slice::from_ref(c))))
        .collect::<Vec<_>>()
        .join(",")
}

/// A length of time in words, to the minute past an hour: `2 h 5 min`, `45 min`, `30 s`.
fn words(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ if s % 3600 < 60 => format!("{} h", s / 3600),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

/// The rows a merge of `diff` changes, how many of them it deletes, and the effects it queues, as
/// policies count them: the database's own rows (indexes, constraints, definitions) don't count.
pub(crate) fn counts(diff: &[crate::Change]) -> (u64, u64, u64) {
    let effects = diff.iter().filter(|c| crate::outbox::entry(&c.key) && c.before.is_none()).count() as u64;
    let (rows, deletes) = diff
        .iter()
        .filter(|c| {
            let table = c.key.split('/').next().unwrap_or_default();
            !crate::sql::internal(&c.key) && !crate::outbox::entry(&c.key) && !crate::reserved_table(table)
        })
        .fold((0, 0), |(r, d), c| (r + 1, d + c.after.is_none() as u64));
    (rows, deletes, effects)
}

pub fn listed(names: &[String]) -> String {
    let one = |t: &String| match t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        true => t.clone(),
        false => format!("\"{}\"", t.replace('"', "\"\"")),
    };
    names.iter().map(one).collect::<Vec<_>>().join(",")
}

impl Core {
    /// Creates merge policy `name` with the settings in `j` (see `MergePolicy::apply`).
    pub fn create_policy(&self, name: &str, j: &Json) -> Result<MergePolicy> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(bad(format!("'{name}' can't name a merge policy: use letters, digits, _ and -")));
        }
        let mut p = MergePolicy { name: name.into(), created: disk::now_ms(), ..Default::default() };
        p.apply(j)?;
        let key = format!("{POLICIES}/{name}");
        // checked and written in one step: two creates at once can't both think they made it
        self.batch_with(MAIN, |v| match v.get(&key)? {
            Some(_) => Err(Error::Exists(format!("merge policy {name}"))),
            None => Ok((vec![(key.clone(), Some(p.to_json()))], ())),
        })?;
        Ok(p)
    }

    /// Changes the settings `j` names of merge policy `name`; the rest stay. Read, changed and
    /// written in one step, so two changes at once both land and a policy dropped meanwhile
    /// isn't written back.
    pub fn alter_policy(&self, name: &str, j: &Json) -> Result<MergePolicy> {
        let key = format!("{POLICIES}/{name}");
        let (_, p) = self.batch_with(MAIN, |v| {
            let gone = || Error::NoBranch(format!("merge policy {name}"));
            let mut p = v.get(&key)?.and_then(|raw| MergePolicy::from_json(name, &raw)).ok_or_else(gone)?;
            p.apply(j)?;
            Ok::<_, Error>((vec![(key.clone(), Some(p.to_json()))], p))
        })?;
        Ok(p)
    }

    /// Removes merge policy `name`. While an agent keeps to it that's refused, unless `release`
    /// (DROP ... CASCADE): then those agents keep to none from now on. Checked, released and
    /// removed in one step, so an agent given the policy meanwhile can't be left naming it (see
    /// `put_agent`), and a release never happens without the drop.
    pub fn drop_policy(&self, name: &str, release: bool) -> Result<Vec<String>> {
        let key = format!("{POLICIES}/{name}");
        let (_, released) = self.batch_with(MAIN, |v| {
            if v.get(&key)?.is_none() {
                return Err(Error::NoBranch(format!("merge policy {name}")));
            }
            let prefix = format!("{AGENTS}/");
            let using: Vec<Agent> = v
                .prefix(&prefix)?
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter_map(|(k, raw)| Agent::from_json(&k[prefix.len()..], &raw))
                .filter(|a| a.policy.as_deref() == Some(name))
                .collect();
            let names: Vec<String> = using.iter().map(|a| a.name.clone()).collect();
            if !using.is_empty() && !release {
                return Err(bad(format!(
                    "agents {} keep to merge policy {name}; give them another (or none) first, or DROP MERGE POLICY {name} CASCADE",
                    names.join(", ")
                )));
            }
            let mut rows = vec![(key.clone(), None)];
            for mut a in using {
                a.policy = None;
                rows.push((format!("{AGENTS}/{}", a.name), Some(a.to_json())));
            }
            Ok((rows, names))
        })?;
        Ok(released)
    }

    pub fn policy(&self, name: &str) -> Result<MergePolicy> {
        let raw = self.view(MAIN)?.get(&format!("{POLICIES}/{name}"))?;
        raw.and_then(|r| MergePolicy::from_json(name, &r))
            .ok_or_else(|| Error::NoBranch(format!("merge policy {name}")))
    }

    pub fn policies(&self) -> Result<Vec<MergePolicy>> {
        let prefix = format!("{POLICIES}/");
        let rows = self.view(MAIN)?.prefix(&prefix)?.collect::<Result<Vec<_>>>()?;
        Ok(rows.into_iter().filter_map(|(k, v)| MergePolicy::from_json(&k[prefix.len()..], &v)).collect())
    }

    /// The rules merging world `b` into its parent would break under merge policy `name` with
    /// `rules` on top (either may be left out), and so send to a person; none: it'd merge on its
    /// own. For trying rules on real work before an agent keeps to them. Rows both sides changed
    /// are `merge_preview`'s (a merge that doesn't settle them fails, policy or not).
    pub fn check_policy(&self, b: &str, name: Option<&str>, rules: &Json) -> Result<Vec<String>> {
        let mut p = name.map(|n| self.policy(n)).transpose()?.unwrap_or_default();
        if !rules.is_null() {
            p.apply(rules)?;
        }
        let br = self.branch(&self.world(b)?.name)?;
        Ok(p.broken(&self.state_diff(&br.base(), &br.snapshot())?, 0))
    }

    /// Agent `a`'s merge policy, if it has one.
    fn policy_of(&self, a: &Agent) -> Result<Option<MergePolicy>> {
        a.policy.as_deref().map(|p| self.policy(p)).transpose()
    }

    /// The merge policy of the agent this thread acts for, if any: checked inside every merge
    /// (see `Core::merge_locked`). An agent dropped while it acts is refused, not let through
    /// unchecked. (A session's temporary tables merge acting for no one: see `sql`'s `temp_end`.)
    pub(crate) fn acting_policy(&self) -> Result<Option<MergePolicy>> {
        let actor = disk::actor();
        if actor.is_empty() {
            return Ok(None);
        }
        match self.agent(&actor) {
            Ok(a) => self.policy_of(&a),
            Err(_) => Err(refused(format!(
                "agent {actor} no longer exists, so its merge can't be checked; nothing was merged"
            ))),
        }
    }

    /// Queues world `b`'s merge for a person: policy `p` refused it for `reasons`, checked at
    /// the world's `version` (a write since then shows as `changed_since`).
    pub(crate) fn ask_review(&self, b: &str, p: &str, reasons: &[String], version: u64) -> Result<()> {
        let v = json!({ "policy": p, "reasons": reasons, "at": disk::now_ms(), "version": version });
        self.set_reserved(b, REVIEW, Some(v))
    }

    /// Notes on world `b`'s queue entry, if it has one, that part of it was merged (see
    /// `Review::partly_merged`).
    pub(crate) fn partly_merged(&self, b: &str) -> Result<()> {
        let Some(mut r) = self.world(b)?.meta.get(REVIEW).cloned() else { return Ok(()) };
        r["partly_merged"] = json!(true);
        self.set_reserved(b, REVIEW, Some(r))
    }

    /// Merges waiting for a person, oldest first: a person merges the world (or drops it).
    pub fn reviews(&self) -> Vec<Review> {
        let mut out: Vec<Review> = self
            .worlds()
            .into_iter()
            .filter_map(|w| {
                let r = w.meta.get(REVIEW)?;
                Some(Review {
                    policy: r["policy"].as_str().unwrap_or_default().into(),
                    reasons: r["reasons"].as_array()?.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                    at: r["at"].as_u64().unwrap_or(0),
                    version: r["version"].as_u64().unwrap_or(0),
                    partly_merged: r["partly_merged"].as_bool().unwrap_or(false),
                    now: w.version,
                    owner: w.owner,
                    scope: w.scope,
                    world: w.name,
                })
            })
            .collect();
        out.sort_by_key(|r| r.at);
        out
    }
}

thread_local! {
    /// This thread runs a world of a simulation: its statements run within the slot the
    /// simulation (its request or statement) holds, so none of them is refused for want of one.
    static IN_SIMULATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks this thread as running a simulation's world until dropped (see `IN_SIMULATION`).
pub(crate) struct InSimulation(bool);

impl InSimulation {
    pub(crate) fn enter() -> InSimulation {
        InSimulation(IN_SIMULATION.replace(true))
    }
}

impl Drop for InSimulation {
    fn drop(&mut self) {
        IN_SIMULATION.set(self.0);
    }
}

/// One of an agent's statements in flight (see `Core::slot`).
pub struct Slot<'a> {
    core: &'a Core,
    agent: String,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut running = crate::lock(&self.core.running); // a panic here, unwinding, would abort
        if let Some(n) = running.get_mut(&self.agent) {
            *n -= 1;
            if *n == 0 {
                running.remove(&self.agent);
            }
        }
    }
}

/// A database acting as one agent (see `Core::as_agent`): each operation checks the agent may do
/// it first, and what it writes is logged as the agent's.
pub struct Acting<'a> {
    core: &'a Core,
    _as: disk::ActingAs,
    pub agent: Agent,
}

impl Acting<'_> {
    pub(crate) fn need(&self, c: Can, doing: &str) -> Result<()> {
        if self.agent.may(c) {
            return Ok(());
        }
        Err(refused(format!("agent {} may not {doing}: it needs '{}'", self.agent.name, c.name())))
    }

    fn owns(&self, w: &World) -> bool {
        w.owner.as_deref() == Some(self.agent.name.as_str())
    }

    /// Whether the agent may read world `w` (a name, ID, or `name@when`).
    pub fn may_read(&self, w: &str) -> Result<()> {
        crate::not_tx(w)?; // a session's transaction is its own
        self.need(Can::Read, "read")
    }

    /// Whether the agent may change world `w` by `rows` more rows now: its right to, its write
    /// rate, and the world's change quota. An agent keeping to a merge policy changes main only
    /// by merging, where the policy is checked: else `write_main` would get around it.
    pub fn may_write(&self, w: &str, rows: usize) -> Result<()> {
        if let Some(p) = &self.agent.policy
            && self.core.world(w)?.name == MAIN
        {
            return Err(refused(format!(
                "agent {} keeps to merge policy {p}, so it changes main only by merging a world of its own: fork one, change it, merge it",
                self.agent.name
            )));
        }
        self.may_write_rows(w, rows)
    }

    /// `may_write` without the merge policy's rule: for merges, which check the policy themselves.
    fn may_write_rows(&self, w: &str, rows: usize) -> Result<()> {
        let world = self.core.world(w)?;
        self.may_change(&world.name)?;
        let a = &self.agent;
        if a.max_changes > 0 && world.name != MAIN && self.core.changed(&world.name)? + rows as u64 > a.max_changes {
            return Err(refused(format!(
                "world {} may change at most {} rows (agent {}'s quota)",
                world.name, a.max_changes, a.name
            )));
        }
        if a.writes_per_minute > 0 && !self.core.take_write(&a.name, a.writes_per_minute) {
            return Err(Error::Constraint {
                code: "53400", // configuration_limit_exceeded
                message: format!("agent {} may write {} times a minute; slow down", a.name, a.writes_per_minute),
            });
        }
        Ok(())
    }

    /// Whether the agent may merge world `w` into its parent: with 'merge', any world; with
    /// 'merge_own', its own, into a parent it may change (main needs 'write_main').
    pub fn may_merge(&self, w: &str) -> Result<()> {
        let world = self.core.world(w)?;
        if self.agent.may(Can::Merge) {
            return Ok(());
        }
        if !(self.owns(&world) && self.agent.may(Can::MergeOwn)) {
            return Err(refused(format!(
                "agent {} may not merge {}: it needs 'merge', or 'merge_own' and to own it",
                self.agent.name, world.name
            )));
        }
        match world.parent.as_deref() {
            Some(p) => self.may_change(p),
            None => Ok(()),
        }
    }

    /// Whether the agent may change world `w` (its right to, not its rate or quota).
    fn may_change(&self, w: &str) -> Result<()> {
        let world = self.core.world(w)?;
        let doing = format!("change {}", world.name);
        match () {
            _ if world.name == MAIN => self.need(Can::WriteMain, &doing),
            _ if self.owns(&world) && self.agent.may(Can::WriteOwn) => Ok(()),
            _ => self.need(Can::Write, &doing),
        }
    }

    /// Whether the agent may fork world `w`. A world can't be merged or discarded while it has
    /// forks, so forking any world but main needs the right to change it: else anyone who may
    /// read a world could pin it.
    pub fn may_fork(&self, w: &str) -> Result<()> {
        self.need(Can::Fork, "fork")?;
        let world = self.core.world(w)?;
        if world.name != MAIN {
            self.may_change(&world.name).map_err(|_| {
                refused(format!(
                    "agent {} may not fork {}: forking a world needs the right to change it",
                    self.agent.name, world.name
                ))
            })?;
        }
        Ok(())
    }

    /// Whether the agent may merge world `w` into world `into`: the right to merge `w`, and,
    /// unless `into` is its parent, to change `into`.
    pub fn may_merge_into(&self, w: &str, into: &str) -> Result<()> {
        self.may_merge(w)?;
        if self.core.world(w)?.parent.as_deref() != Some(self.core.world(into)?.name.as_str()) {
            self.may_write_rows(into, 0)?;
        }
        Ok(())
    }

    /// Whether the agent may discard world `w` or change its metadata: its own, or with 'write'.
    pub fn may_manage(&self, w: &str) -> Result<()> {
        let world = self.core.world(w)?;
        if world.name == MAIN || !(self.owns(&world) || self.agent.may(Can::Write)) {
            return Err(refused(format!(
                "agent {} may not manage world {}: it's not its own",
                self.agent.name, world.name
            )));
        }
        Ok(())
    }

    /// Whether the agent may discard world `w` with every world forked from it (CASCADE).
    pub fn may_manage_tree(&self, w: &str) -> Result<()> {
        self.may_manage(w)?;
        let forks = self.core.descendants(&self.core.world(w)?.name);
        // sessions' transactions on the tree go with it (their commits then fail)
        forks.iter().filter(|d| !d.starts_with("_tx_")).try_for_each(|d| self.may_manage(d))
    }

    /// Whether the agent may undo a merge or an agent's changes (`UNDO MERGE`, `UNDO AGENT`):
    /// `admin`, or `restore` and `merge`, and (checked once the undo knows the world it writes:
    /// `Core::may_undo_into`) the right to write that world. Never an agent keeping to a merge
    /// policy: an undo writes its rows straight into the world, main included, past the rules.
    pub fn may_undo(&self) -> Result<()> {
        if let Some(p) = &self.agent.policy {
            return Err(refused(format!(
                "agent {} keeps to merge policy {p}, so it may not undo merges or agents' changes: they write past the rules",
                self.agent.name
            )));
        }
        if self.agent.may(Can::Restore) && self.agent.may(Can::Merge) {
            return Ok(());
        }
        self.may_admin()
    }

    /// Whether the agent may put world `w` back as it was.
    pub fn may_restore(&self, w: &str) -> Result<()> {
        self.need(Can::Restore, "restore")?;
        self.may_write(w, 0)
    }

    /// Whether the agent may manage agents.
    pub fn may_admin(&self) -> Result<()> {
        self.need(Can::Admin, "manage agents or the database")
    }

    /// Forks `from` as `name`, owned by this agent (expiring after its world TTL, if any).
    pub fn fork(&self, from: &str, name: &str, meta: Json) -> Result<World> {
        self.may_fork(from)?;
        let owner = self.forks(1)?;
        let mut meta = match meta {
            Json::Object(m) => m,
            Json::Null => Default::default(),
            _ => return Err(bad("world metadata must be a JSON object".into())),
        };
        crate::reserved_keys(&meta)?; // only the database sets its own keys (owner, expiry, last use, ...)
        crate::stale::from_meta(&mut meta)?;
        crate::scope::from_meta(self.core, from, &mut meta)?;
        let policy = self.core.policy_of(&self.agent)?;
        if policy.as_ref().is_some_and(|p| p.check_reads) {
            meta.insert(crate::stale::CHECK_READS.into(), Json::Bool(true)); // its policy checks what it reads
        }
        if let Some(p) = policy.filter(|p| p.require_scope)
            && !meta
                .get(crate::scope::SCOPE)
                .is_some_and(|s| s.get("may_change").is_some() || s.get("tenant").is_some())
        {
            return Err(refused(format!(
                "merge policy {} has its agents say what a world may change when forking it: add WITH (may_change = 'table, table.column') or tenant = 'column = value'",
                p.name
            )));
        }
        meta.extend(owner);
        self.core.fork_owned(from, name, meta)
    }

    /// The metadata that makes `n` new worlds this agent's (its owner, its TTL), once it's checked
    /// the agent may fork and has room for `n` more worlds.
    pub(crate) fn forks(&self, n: usize) -> Result<serde_json::Map<String, Json>> {
        self.need(Can::Fork, "fork")?;
        let a = &self.agent;
        if a.max_worlds > 0 {
            let owned = self.owned();
            if owned + n as u64 > a.max_worlds {
                let more = if n > 1 { format!(" (it owns {owned}; this needs {n} more)") } else { String::new() };
                return Err(refused(format!(
                    "agent {} may own at most {} worlds at once{more}; merge or discard some",
                    a.name, a.max_worlds
                )));
            }
        }
        let mut meta = serde_json::Map::new();
        meta.insert(OWNER.into(), json!(a.name));
        if a.world_ttl > 0 {
            meta.insert(EXPIRES.into(), json!(disk::now_ms().saturating_add(a.world_ttl)));
        }
        Ok(meta)
    }

    /// The worlds this agent owns now.
    pub(crate) fn owned(&self) -> u64 {
        self.core.worlds().iter().filter(|w| self.owns(w)).count() as u64
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_agent_row_without_a_verifier_reads() {
        // as agents were kept before they had SCRAM verifiers
        let old = r#"{"can":["read","fork","write_own"],"created":1,"disabled":false,"id":"a1","max_changes":0,"max_concurrent":0,"max_memory_mb":0,"max_query_ms":0,"max_worlds":0,"policy":null,"token_hash":"ab","world_ttl":0,"writes_per_minute":0}"#;
        let mut a = super::Agent::from_json("bot", old).unwrap();
        assert_eq!((a.scram.as_deref(), a.verifier()), (None, None));
        a.scram = Some(crate::sql::scram::fresh("chronos_x", 4096).unwrap());
        let back = super::Agent::from_json("bot", &a.to_json()).unwrap();
        assert_eq!(back, a);
        assert!(back.verifier().is_some());
    }

    #[test]
    fn ages_in_words() {
        assert_eq!(super::words(30_000), "30 s");
        assert_eq!(super::words(45 * 60_000), "45 min");
        assert_eq!(super::words(3_600_000), "1 h");
        assert_eq!(super::words(3_600_000 + 5 * 60_000 + 7_000), "1 h 5 min");
        let p = super::MergePolicy { max_age: Some(3_600_000), ..Default::default() };
        assert_eq!(p.age_broken(1_000, 1_000 + 3_600_000), None, "an hour exactly is still fine");
        assert_eq!(p.age_broken(1_000, 1_000 + 7_500_000).unwrap(), "was forked 2 h 5 min ago (at most 1 h)");
    }

    /// A merge check's definition is a schema change, as every definition is: an agent keeping to
    /// a policy can't bring one in (or take one out) without a person.
    #[test]
    fn a_merge_check_is_a_schema_change() {
        let c = crate::Change { key: "_sonos_checks/keep".into(), before: None, after: Some("{}".into()) };
        assert_eq!(super::MergePolicy::default().broken(&[c], 0), ["changes the schema: check keep"]);
    }
}
