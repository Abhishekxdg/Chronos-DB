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
/// The secret masked columns' hashes are keyed by (see `Core::mask_key`).
const MASK_KEY: &str = "_sonos_masking/key";
/// The longest `world_ttl`: 100 years, in ms (an expiry is now plus it, and shows as an interval).
const WORLD_TTL_MAX: u64 = 36_525 * 86_400_000;

/// Whether `key` is an agent's, a merge policy's, a merge check's or the roles' row (see
/// `Core::undo`: never put back from the past).
pub(crate) fn agent_row(key: &str) -> bool {
    [AGENTS, POLICIES, crate::checks::CHECKS, "_sonos_roles", "_sonos_masking"]
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
/// The channel a merge held for review is said on (LISTEN chronos_review_held): a JSON payload with the world, its
/// policy, the policy's reviewer, the version it was held at, and whether a person must decide it.
pub const REVIEW_CHANNEL: &str = "chronos_review_held";
/// How often a world's merge was refused, by kind (see `Core::note_refusal`).
pub(crate) const REFUSALS: &str = "_sonos_refusals";
/// World metadata key: the Postgres role and settings its owner agent writes as (see `Agent::act_as`), copied from
/// the agent when it forks the world, so the world's write-back is the end user's however the agent changes after.
pub(crate) const ACT_AS: &str = "_sonos_act_as";

/// Who `chronos serve` takes a client with the server token for (every client that isn't an agent
/// or the admin token, with `--safe`): an agent with the default rights (read, fork, change the
/// worlds it forked), shared by all such clients.
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
    /// read masked columns as they are (see `sql::mask`); without it they read masked
    Unmask,
    /// everything, and managing agents
    Admin,
}

impl Can {
    pub const ALL: [Can; 11] = [
        Can::Read,
        Can::Fork,
        Can::WriteOwn,
        Can::Write,
        Can::WriteMain,
        Can::MergeOwn,
        Can::Merge,
        Can::Restore,
        Can::Outbox,
        Can::Unmask,
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
            Can::Unmask => "unmask",
            Can::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Can> {
        Can::ALL.into_iter().find(|c| c.name() == s.trim())
    }
}

/// How an agent's statements share the CPU with the statements of the database's own users (sessions no agent
/// runs): a world isolates data, not CPU, so an agent's heavy scan in its world competes with main's traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    /// as anyone's
    #[default]
    Normal,
    /// while any statement of the database's own users runs, its statements yield: each of its threads runs at most
    /// a quarter of the time, and its parallel scans borrow no helper threads (see `sql`'s `Stop::check`)
    Low,
}

impl Priority {
    pub fn name(self) -> &'static str {
        match self {
            Priority::Normal => "normal",
            Priority::Low => "low",
        }
    }

    pub fn parse(s: &str) -> Option<Priority> {
        match s.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(Priority::Normal),
            "low" => Some(Priority::Low),
            _ => None,
        }
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
    /// how long a Postgres connection of its may sit idle inside a transaction, in ms, before it's
    /// dropped and the transaction rolled back, its locks let go: its
    /// idle_in_transaction_session_timeout can't go past it (0: no limit)
    pub max_idle_ms: u64,
    /// the most threads one of its statements may run on (a parallel scan's parts), its own included, so a heavy
    /// scan leaves the other cores to everyone else (0: as many as the server's pool lends)
    pub max_threads: u64,
    /// how its statements share the CPU with the database's own users (see `Priority`)
    pub priority: Priority,
    pub created: u64,
    pub disabled: bool,
    /// the merge policy its merges keep to (see `MergePolicy`); None: its rights alone decide
    pub policy: Option<String>,
    /// the one tenant it works for, `{"column": c, "value": v}` (see `tenant_sql`): in every world it reads and
    /// changes only rows of tables with that column that hold that value, as if a policy said so whatever its
    /// role, and every world it forks is declared that tenant's (see `scope`). None: any rows its rights reach
    pub tenant: Option<Json>,
    /// when its token stops working (ms since 1970); 0: never
    pub expires: u64,
    /// when its token was last rotated (ms since 1970); 0: never rotated since it was created
    pub rotated: u64,
    /// who it works for, in an attached Postgres: `{"role": r, "settings": {name: value}}`. A world it forks is
    /// written back to Postgres as that role (`SET LOCAL ROLE`), with those settings (`set_config(name, value,
    /// true)`, such as Supabase's `request.jwt.claims`), so Postgres's own grants and row-level security policies
    /// judge its writes as they would the end user's (see `attach`). None: written as the attach role
    pub act_as: Option<Json>,
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
    pub max_idle_ms: Option<u64>,
    pub max_threads: Option<u64>,
    pub priority: Option<Priority>,
    pub disabled: Option<bool>,
    /// a merge policy's name; "" takes the agent's away
    pub policy: Option<String>,
    /// its tenant (text `'org_id = 42'`, or `{"org_id": 42}`); null takes it away
    pub tenant: Option<Json>,
    /// how long from now its token works, in ms; 0: no end
    pub valid_for: Option<u64>,
    /// the Postgres role its worlds are written back as (see `Agent::act_as`); "" takes it (and its settings) away
    pub pg_role: Option<String>,
    /// the settings its worlds are written back with, `{"request.jwt.claims": "..."}` (a JSON object, or its text);
    /// null takes them away
    pub pg_settings: Option<Json>,
}

impl Agent {
    /// Whether the agent may do `c` (an admin may do anything; a disabled or expired agent nothing).
    pub fn may(&self, c: Can) -> bool {
        !self.disabled && !self.expired() && (self.can.contains(&c) || self.can.contains(&Can::Admin))
    }

    /// Whether its token stopped working (see `expires`).
    pub fn expired(&self) -> bool {
        self.expires > 0 && disk::now_ms() >= self.expires
    }

    /// Its tenant's condition as SQL (`"org_id" = 42`), for the rows it may read and change; None without one.
    pub fn tenant_sql(&self) -> Option<String> {
        let t = self.tenant.as_ref()?;
        let col = t["column"].as_str()?;
        Some(format!("\"{}\" = {}", col.replace('"', "\"\""), crate::scope::lit(&t["value"])))
    }

    fn to_json(&self) -> String {
        json!({
            "id": self.id, "can": self.can.iter().map(|c| c.name()).collect::<Vec<_>>(),
            "max_worlds": self.max_worlds, "writes_per_minute": self.writes_per_minute,
            "max_changes": self.max_changes, "world_ttl": self.world_ttl, "max_query_ms": self.max_query_ms,
            "max_concurrent": self.max_concurrent, "max_memory_mb": self.max_memory_mb, "max_idle_ms": self.max_idle_ms, "max_threads": self.max_threads,
            "priority": self.priority.name(),
            "created": self.created,
            "disabled": self.disabled, "policy": self.policy, "token_hash": self.token_hash, "scram": self.scram,
            "tenant": self.tenant, "expires": self.expires, "rotated": self.rotated, "act_as": self.act_as,
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
            max_idle_ms: n("max_idle_ms"),
            max_threads: n("max_threads"),
            // (kept by an older version: none, normal)
            priority: j["priority"].as_str().and_then(Priority::parse).unwrap_or_default(),
            created: n("created"),
            disabled: j["disabled"].as_bool().unwrap_or(false),
            policy: j["policy"].as_str().map(String::from),
            tenant: j.get("tenant").filter(|t| t.is_object()).cloned(),
            expires: n("expires"),
            rotated: n("rotated"),
            act_as: j.get("act_as").filter(|t| t.is_object()).cloned(),
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
            max_idle_ms: 0,
            max_threads: 0,
            priority: Priority::Normal,
            created: 0,
            disabled: false,
            policy: None,
            tenant: None,
            expires: 0,
            rotated: 0,
            act_as: None,
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
        self.max_idle_ms = spec.max_idle_ms.unwrap_or(self.max_idle_ms);
        self.max_threads = spec.max_threads.unwrap_or(self.max_threads);
        self.priority = spec.priority.unwrap_or(self.priority);
        self.disabled = spec.disabled.unwrap_or(self.disabled);
        if let Some(p) = &spec.policy {
            self.policy = (!p.is_empty()).then(|| p.clone());
        }
        match &spec.tenant {
            None => {}
            Some(Json::Null) => self.tenant = None,
            Some(t) => self.tenant = Some(crate::scope::tenant(t)?),
        }
        match spec.valid_for {
            None => {}
            Some(0) => self.expires = 0,
            Some(ms) if ms > WORLD_TTL_MAX => {
                return Err(bad(format!("valid_for is at most {WORLD_TTL_MAX} ms (100 years); 0 means no end")));
            }
            Some(ms) => self.expires = disk::now_ms() + ms,
        }
        self.act_as = act_as(self.act_as.take(), spec)?;
        Ok(())
    }
}

/// The most settings an agent writes back with, and the most bytes of them all.
const ACT_AS_SETTINGS: usize = 32;
const ACT_AS_BYTES: usize = 16 << 10;

/// Agent `was`'s `act_as` changed as `spec` says (see `Agent::act_as`), checked: a role name Postgres could have
/// (1 to 63 bytes, no NUL), and settings that are custom ones only (`prefix.name`: letters, digits and `_`, at least
/// one dot), so none can change how Postgres runs the write-back (`role`, `statement_timeout`, `search_path`, ...).
/// A value that isn't text (`{"sub": "u-17"}`) is kept as its JSON text, which is what Postgres's settings hold.
fn act_as(was: Option<Json>, spec: &AgentSpec) -> Result<Option<Json>> {
    let mut role = was.as_ref().and_then(|a| a["role"].as_str()).map(String::from);
    let mut settings = was.as_ref().and_then(|a| a["settings"].as_object()).cloned().unwrap_or_default();
    if let Some(r) = &spec.pg_role {
        if r.is_empty() {
            role = None;
            settings.clear();
        } else if r.len() > 63 || r.contains('\0') {
            return Err(bad(format!("pg_role {r:?} isn't a Postgres role name (1 to 63 bytes)")));
        } else {
            role = Some(r.clone());
        }
    }
    match &spec.pg_settings {
        None => {}
        Some(Json::Null) => settings.clear(),
        Some(v) => {
            let parsed;
            let v = match v {
                Json::String(s) => {
                    parsed = serde_json::from_str::<Json>(s)
                        .map_err(|e| bad(format!("pg_settings must be a JSON object of settings: {e}")))?;
                    &parsed
                }
                v => v,
            };
            let Some(o) = v.as_object() else {
                return Err(bad("pg_settings must be a JSON object, like {\"request.jwt.claims\": \"...\"}".into()));
            };
            settings.clear();
            for (k, v) in o {
                let custom = k.contains('.')
                    && k.split('.').all(|p| {
                        !p.is_empty()
                            && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                            && !p.as_bytes()[0].is_ascii_digit()
                    });
                if !custom {
                    return Err(bad(format!(
                        "pg_settings: {k:?} isn't a custom setting (prefix.name, as request.jwt.claims or \
                         app.user_id): Postgres's own settings can't be set for the write-back"
                    )));
                }
                let text = match v {
                    Json::String(s) => s.clone(),
                    Json::Null => return Err(bad(format!("pg_settings: {k} must have a value"))),
                    v => v.to_string(),
                };
                if text.contains('\0') {
                    return Err(bad(format!("pg_settings: {k}'s value can't hold a NUL")));
                }
                settings.insert(k.to_ascii_lowercase(), Json::String(text));
            }
            if settings.len() > ACT_AS_SETTINGS {
                return Err(bad(format!("pg_settings: at most {ACT_AS_SETTINGS} settings")));
            }
            if Json::Object(settings.clone()).to_string().len() > ACT_AS_BYTES {
                return Err(bad(format!("pg_settings: at most {ACT_AS_BYTES} bytes")));
            }
        }
    }
    match role {
        Some(r) => Ok(Some(json!({"role": r, "settings": settings}))),
        None if !settings.is_empty() => {
            Err(bad("pg_settings need a pg_role: the role the write-back runs as, with them".into()))
        }
        None => Ok(None),
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
    /// the run that made it, from the world's metadata (see `RUN_KEYS`): None when it names none
    pub run: Option<Json>,
}

/// World metadata keys that say which run made a world's changes: the caller's id for the run, and the prompt,
/// model and tool schema it ran with. Set at the fork (or with ALTER WORLD); shown by SHOW AUDIT and SHOW REVIEWS,
/// and `UNDO AGENT ... WHERE run_id = '...'` puts back one run's changes.
pub const RUN_KEYS: [&str; 4] = ["run_id", "prompt_version", "model", "tool_schema"];

/// The run keys `meta` (a world's metadata object) has, as an object; None when it has none.
pub fn run_of(meta: &serde_json::Map<String, Json>) -> Option<Json> {
    let run: serde_json::Map<String, Json> =
        RUN_KEYS.iter().filter_map(|k| Some((k.to_string(), meta.get(*k)?.clone()))).collect();
    (!run.is_empty()).then_some(Json::Object(run))
}

/// Refuses run keys (see `RUN_KEYS`) in `meta` that aren't text.
pub(crate) fn check_run(meta: &serde_json::Map<String, Json>) -> Result<()> {
    match RUN_KEYS.iter().find(|k| meta.get(**k).is_some_and(|v| !v.is_string() && !v.is_null())) {
        Some(k) => Err(bad(format!("{k} must be text"))),
        None => Ok(()),
    }
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
    /// effects (NOTIFY ON MERGE) discarded with their worlds, never sent: vetoed, dropped, or swept (see
    /// `outbox::Discarded`)
    pub effects_discarded: u64,
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
    pub(crate) cut: u64,
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

/// `1 row`, `2 rows`: a count of rows as a refusal says it.
pub(crate) fn n_rows(n: u64) -> String {
    format!("{n} {}", if n == 1 { "row" } else { "rows" })
}

/// A person's merge of world `b`, which deletes `deletes` rows, refused for want of the count merge policy `policy`
/// asks for above `over` (`given`: the count given, if one was).
pub(crate) fn count_refused(b: &str, deletes: u64, policy: &str, over: u64, given: Option<u64>) -> Error {
    let said = match given {
        Some(g) => format!("CONFIRM DELETES {g} doesn't match: it deletes {deletes}"),
        None => format!("it deletes {}", n_rows(deletes)),
    };
    Error::Constraint {
        code: "55000",
        message: format!(
            "merging {b}: {said}, and merge policy {policy} has a person approve more than {over} deletes only by \
             giving the count: MERGE WORLD {b} CONFIRM DELETES {deletes} (after reading them: DIFF WORLD {b}); \
             nothing was merged"
        ),
    }
}

/// The error an agent's merge gets when its merge policy sends it to a person.
pub(crate) fn needs_review(b: &str, policy: &str, reasons: &[String]) -> Error {
    refused(for_caller(&format!(
        "merging {b} needs a person's review (merge policy {policy}: it {}); it's queued (SHOW REVIEWS)",
        reasons.join("; it ")
    )))
}

/// Metadata key of a world whose merge landed a canary (see `Core::canary`): when, and how many rows.
pub(crate) const CANARY: &str = "_sonos_canary";
/// How long a canary waits when its policy doesn't say: ten minutes.
const CANARY_WAIT: u64 = 10 * 60 * 1000;

/// Marks around a value a reason shows (a row's key, what a merge check found, what a SELECT gave): text an outsider
/// may have written. People see it as it is (`for_people`); an agent, a policy's reviewer among them, sees `…` in its
/// place (`values_free`), so a reason can't carry instructions planted in a row to the model that decides on it.
const VALUE_OPEN: char = '\u{E000}';
const VALUE_CLOSE: char = '\u{E001}';

/// `v`, marked as a value in a reason (see `VALUE_OPEN`).
pub(crate) fn value(v: impl std::fmt::Display) -> String {
    let v = v.to_string().replace([VALUE_OPEN, VALUE_CLOSE], "");
    format!("{VALUE_OPEN}{v}{VALUE_CLOSE}")
}

/// Row key `key` as a reason shows it: readable (see `sql::show_key`), its id marked as a value unless it's only
/// numbers (`leads/2`, `pairs/(1, 2)`), which carry no words.
pub(crate) fn shown_key(key: &str) -> String {
    let shown = crate::sql::show_key(key);
    match shown.split_once('/') {
        Some((_, id)) if id.chars().all(|c| c.is_ascii_digit() || "-.,() ".contains(c)) => shown.into_owned(),
        Some((t, id)) => format!("{t}/{}", value(id)),
        None => value(shown),
    }
}

/// A reason as people read it: its values as they are.
pub fn for_people(reason: &str) -> String {
    reason.replace([VALUE_OPEN, VALUE_CLOSE], "")
}

/// A reason as an agent reads it: each value it shows replaced by `…`, what's around it (the rule, the table, the
/// column, how many rows) kept.
pub fn values_free(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    let mut inside = false;
    for ch in reason.chars() {
        match ch {
            VALUE_OPEN if !inside => {
                inside = true;
                out.push('…');
            }
            VALUE_CLOSE => inside = false,
            _ if inside => {}
            ch => out.push(ch),
        }
    }
    out
}

/// A reason as whoever this thread acts for reads it: an agent, values-free; a person, as it is.
pub(crate) fn for_caller(reason: &str) -> String {
    match disk::actor().is_empty() {
        true => for_people(reason),
        false => values_free(reason),
    }
}

/// An agent (one keeping to no policy: an admin's, say) merging world `b`, held for `person`, rules its policy keeps
/// for a person (see `MergePolicy::person_only`): refused, as the policy's reviewer's approval is.
pub(crate) fn person_only_refused(b: &str, person: &[String]) -> Error {
    refused(for_caller(&format!(
        "{b} waits for a person, not an agent: it {} (its merge policy's person_only rules)",
        person.join("; it ")
    )))
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
            max_idle_ms: 0,
            max_threads: 0,
            priority: Priority::Normal,
            created: disk::now_ms(),
            disabled: false,
            policy: None,
            tenant: None,
            expires: 0,
            rotated: 0,
            act_as: None,
            token_hash: hash(&token),
            scram: Some(scram),
        };
        a.apply(spec)?;
        self.tenant_held(&a)?;
        self.put_agent(&a)?;
        self.record_security("create agent", name);
        Ok((a, token))
    }

    /// Refuses agent `a`'s tenant when no table on main has its column: it is held to its tenant only in tables with
    /// the column, so one no table has (a typo, `{"nosuch": 1}`) would hold it nowhere and let it see every tenant's
    /// rows. Tables without the column are shared lookup tables, read whole, as long as some table has it. Nothing to
    /// check on a database with no tables yet.
    fn tenant_held(&self, a: &Agent) -> Result<()> {
        let Some(col) = a.tenant.as_ref().and_then(|t| t["column"].as_str()) else { return Ok(()) };
        let tables = crate::sql::tables_and_columns(self, MAIN)?;
        if tables.is_empty() || tables.iter().any(|(_, cs)| cs.iter().any(|c| c == col)) {
            return Ok(());
        }
        let seen: Vec<String> = tables.iter().map(|(t, cs)| format!("{t} ({})", cs.join(", "))).collect();
        Err(bad(format!(
            "tenant {}: no table has a column \"{col}\", so agent {} would be held to no tenant and see every row; \
             the tables are {}",
            a.tenant_sql().unwrap_or_default(),
            a.name,
            seen.join("; ")
        )))
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
        if spec.tenant.as_ref().is_some_and(|t| !t.is_null()) {
            self.tenant_held(&a)?;
        }
        self.put_agent(&a)?;
        self.record_security("alter agent", name);
        Ok(a)
    }

    /// Issues agent `name` a fresh token; returns it, shown only now (keep it). The old token
    /// stops working at once. Everything else about the agent — its name, id, capabilities,
    /// quotas, policy and the worlds it owns — is unchanged, so nothing that referred to it
    /// breaks. When it was rotated is recorded (`rotated`, shown by SHOW AGENT PRIVILEGES).
    pub fn rotate_agent_token(&self, name: &str) -> Result<String> {
        self.rotate_agent_token_with(name, crate::sql::scram::ITERATIONS)
    }

    /// `rotate_agent_token`, the new token's SCRAM verifier made with `iterations` (a session's
    /// `scram_iterations`).
    pub fn rotate_agent_token_with(&self, name: &str, iterations: u32) -> Result<String> {
        if name == GUEST || name == SYSTEM {
            return Err(bad(format!("'{name}' has no token to rotate")));
        }
        let mut a = self.agent(name)?; // errors if there's no such agent
        let token = new_token()?;
        let scram = crate::sql::scram::fresh(&token, iterations)
            .map_err(|e| Error::Constraint { code: e.code, message: e.message })?;
        a.token_hash = hash(&token);
        a.scram = Some(scram);
        a.rotated = disk::now_ms();
        self.put_agent(&a)?;
        self.record_security("rotate token", name);
        Ok(token)
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

    /// The secret masked columns' hashes are keyed by (see `sql::mask`): 32 random bytes, made the first time
    /// one is asked for and kept on main, where SQL and the row API don't reach.
    pub(crate) fn mask_key(&self) -> Result<[u8; 32]> {
        let parse = |raw: &str| -> Option<[u8; 32]> {
            let b = raw.as_bytes();
            if b.len() != 64 {
                return None;
            }
            let mut out = [0u8; 32];
            for (i, pair) in b.chunks(2).enumerate() {
                out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
            }
            Some(out)
        };
        if let Some(raw) = self.view(MAIN)?.get(MASK_KEY)?
            && let Some(k) = parse(&raw)
        {
            return Ok(k);
        }
        let fresh = new_token()?;
        let fresh = fresh.trim_start_matches("chronos_").to_string();
        let _system = disk::acting_as(SYSTEM);
        let (_, kept) = self.batch_with(MAIN, |v| match v.get(MASK_KEY)? {
            // (another thread made it first)
            Some(raw) => Ok::<_, Error>((vec![], raw.to_string())),
            None => Ok((vec![(MASK_KEY.to_string(), Some(fresh.clone()))], fresh.clone())),
        })?;
        parse(&kept).ok_or_else(|| bad("the database's mask key is damaged".into()))
    }

    /// Whether a column was ever masked here (the mask key was made): else nothing is masked, and the row API
    /// needn't look.
    pub(crate) fn masks_made(&self) -> bool {
        self.view(MAIN).and_then(|v| v.get(MASK_KEY)).is_ok_and(|k| k.is_some())
    }

    /// Removes agent `name`: its token stops working. Its worlds stay (and keep their owner): the
    /// name can't name a new agent until they're gone (see `create_agent`).
    pub fn drop_agent(&self, name: &str) -> Result<()> {
        self.agent(name)?;
        self.write(MAIN, &[(&format!("{AGENTS}/{name}"), None)])?;
        self.record_security("drop agent", name);
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
            .find(|a| a.token_hash == h && !a.disabled && !a.expired())
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
        if agent.expired() {
            return Err(refused(format!("agent {name}'s token expired (valid_for)")));
        }
        Ok(Acting { core: self, _as: disk::acting_as(name), agent })
    }

    /// Marks world `w` as it is now with a name, to read or restore later (`w@name` doesn't
    /// exist: use `checkpoint_time`, then time travel).
    pub fn checkpoint_world(&self, w: &str, name: &str) -> Result<u64> {
        self.guard_disk(crate::WORLD_BYTES)?; // a mark, kept with the history it names
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
        // each world's run as its metadata says it (see `RUN_KEYS`): as the window opened, then as forks and
        // metadata changes in it say
        let parse = |m: &str| serde_json::from_str::<serde_json::Map<String, Json>>(m).ok().and_then(|m| run_of(&m));
        let mut runs: HashMap<String, Json> =
            win.open.iter().filter_map(|m| Some((m.name.clone(), parse(&m.meta)?))).collect();
        self.walk(&win, |at, who, step| {
            match &step {
                Step::Fork { world, meta, .. } | Step::Meta(world, meta) => match parse(meta) {
                    Some(r) => {
                        runs.insert(world.clone(), r);
                    }
                    None => {
                        runs.remove(world);
                    }
                },
                _ => {}
            }
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
                Step::Meta(w, meta) => {
                    // a reviewer's decision on its held merge (see `decide_review`), or other metadata
                    let d = serde_json::from_str::<Json>(&meta).ok().map(|m| m[REVIEW]["decision"].clone());
                    match d.filter(|d| d["at"].as_u64().is_some_and(|t| t + 1000 >= at)) {
                        Some(d) => {
                            let verdict = d["verdict"].as_str().unwrap_or("decided");
                            let reason = d["reason"].as_str().filter(|r| !r.is_empty());
                            (w, format!("review {verdict}{}", reason.map_or(String::new(), |r| format!(": {r}"))), 0)
                        }
                        None => (w, "metadata".into(), 0),
                    }
                }
            };
            let run = runs.get(&world).cloned();
            out.push(Audit { at, agent: who.into(), world, action, rows, run });
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
        // effects discarded with their worlds, by the world's owner (or its policy then)
        for d in self.discarded_effects(win.cut) {
            let key = if by_policy { d.policy.clone() } else { d.owner.clone() };
            let p = policy(&d.owner);
            rows.entry(key.clone())
                .or_insert_with(|| Outcome {
                    name: key,
                    policy: if by_policy { String::new() } else { p },
                    ..Default::default()
                })
                .effects_discarded += 1;
        }
        let mut rows: Vec<Outcome> = rows.into_values().collect();
        rows.sort_by(|a, b| b.worlds.cmp(&a.worlds).then_with(|| a.name.cmp(&b.name)));
        let since = if win.cut > 0 { win.cut } else { first.unwrap_or_else(disk::now_ms) };
        Ok(Outcomes { since, rows })
    }

    /// Counts a refused merge of world `b` in its metadata (`REFUSALS`), by kind: rows both sides changed (with
    /// main, or in an attached folder with Postgres), rows it relied on changed since (a stale read), or a merge
    /// check or CHECK constraint the result breaks. Not a transaction's world (its COMMIT conflicting is no agent's
    /// merge), not a hold (`REVIEW` records those), and not the other refusals sharing 40001 (a scope changed
    /// meanwhile, Postgres busy). A count that can't be written is let go: the merge's own error is what the caller
    /// gets.
    /// An agent's whole merge of world `b` into main under a policy with `canary` (see `MergePolicy::canary`): one
    /// changing more rows than that lands only that many now, as a merge of its own, and is refused (55000) saying
    /// so; merged again before `canary_wait` has passed, refused; after, it merges as any merge (judged by its
    /// policy). Ok: merge as usual.
    pub(crate) fn canary(&self, b: &str, o: &crate::MergeOptions) -> Result<()> {
        if b.starts_with("_tx_")
            || o.into.is_some()
            || !o.only_tables.is_empty()
            || !o.only_keys.is_empty()
            || disk::actor().is_empty()
            || self.me.get().is_none()
        {
            return Ok(());
        }
        let Some(p) = self.acting_policy()?.filter(|p| p.canary.is_some()) else { return Ok(()) };
        let w = self.world(b)?;
        if w.parent.as_deref() != Some(MAIN) {
            return Ok(());
        }
        let wait = p.canary_wait.unwrap_or(CANARY_WAIT);
        if let Some(c) = w.meta.get(CANARY) {
            let until = c["at"].as_u64().unwrap_or(0).saturating_add(wait);
            let now = disk::now_ms();
            if now < until {
                return Err(Error::Constraint {
                    code: "55000",
                    message: format!(
                        "the rest of {b} waits for its canary ({} rows merged; merge policy {}: canary_wait {}): merge it again in {}",
                        c["rows"].as_u64().unwrap_or(0),
                        p.name,
                        words(wait),
                        words(until - now)
                    ),
                });
            }
            return Ok(());
        }
        let n = p.canary.unwrap_or(0) as usize;
        let keys: Vec<String> = self
            .diff(b)?
            .into_iter()
            .map(|c| c.key)
            .filter(|k| !crate::reserved_table(k.split('/').next().unwrap_or(k)) && !crate::sql::internal(k))
            .collect();
        if keys.len() <= n {
            return Ok(());
        }
        let part = crate::MergeOptions { only_keys: keys[..n].to_vec(), ..o.clone() };
        let (landed, _) = self.merge_once(b, &part)?;
        self.set_reserved(b, CANARY, Some(json!({ "at": disk::now_ms(), "rows": landed, "of": keys.len() })))?;
        Err(Error::Constraint {
            code: "55000",
            message: format!(
                "merged {landed} of {} rows of {b} as a canary (merge policy {}: canary = {n}); the rest merges when {b} is merged again, in {}: check the canary's rows meanwhile (UNDO MERGE {b} takes them back)",
                keys.len(),
                p.name,
                words(wait)
            ),
        })
    }

    /// The columns `untrusted_columns` lists (`'*'` and `table.*` read as the text columns of each table, as `v`
    /// defines it, less `trusted_columns`) a world that read `set` may have read: None when what it read isn't known
    /// (a restart since the fork) and `'*'` asks of every table.
    pub(crate) fn untrusted_seen(
        &self,
        p: &MergePolicy,
        v: &crate::View,
        set: &crate::stale::ReadSet,
    ) -> Option<Vec<(String, String)>> {
        let star = p.untrusted_columns.iter().any(|(t, _)| t == "*");
        if star && !set.complete {
            return None;
        }
        let mut cols: Vec<(String, String)> = vec![];
        let text = |t: &str| -> Vec<(String, String)> {
            crate::sql::text_columns(v, t).into_iter().map(|c| (t.to_string(), c)).collect()
        };
        for (t, c) in &p.untrusted_columns {
            match (t.as_str(), c.as_str()) {
                ("*", _) => cols.extend(set.tables_read().iter().flat_map(|t| text(t))),
                (t, "*") => cols.extend(text(t)),
                _ => cols.push((t.clone(), c.clone())),
            }
        }
        cols.sort();
        cols.dedup();
        cols.retain(|(t, c)| !crate::reserved_table(t) && !p.trusted_columns.iter().any(|(tt, tc)| tt == t && tc == c));
        Some(cols.into_iter().filter(|(t, c)| set.may_have_read(t, c)).collect())
    }

    pub(crate) fn note_refusal(&self, b: &str, e: &Error) {
        let kind = match e {
            _ if b.starts_with("_tx_") => return,
            Error::Conflicts(_) => "conflicts",
            Error::Constraint { code: "40001", message } if message.starts_with("both sides changed table") => {
                "conflicts"
            }
            e if crate::attach::is_conflict(e) => "conflicts",
            e if crate::stale::is_stale(e) => "stale_reads",
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
    /// most rows an agent's merges into main may change in an hour, this one's included (see `tally`; an agent
    /// held to a tenant shares the count with every agent made for that tenant)
    pub rows_per_hour: Option<u64>,
    /// most rows an agent's merges into main may delete in an hour, this one's included (shared as `rows_per_hour`)
    pub deletes_per_hour: Option<u64>,
    /// a merge deleting more rows than this waits for a person, who approves it only by giving the count it deletes
    /// (`MERGE WORLD x CONFIRM DELETES n`): a big delete is approved by someone who read the number
    pub confirm_deletes_over: Option<u64>,
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
    /// whether its agents' worlds keep the SELECTs they read through, and a merge their reads would hold runs them
    /// again: held only if one gives something else now (see `stale`). It checks their reads too
    pub rerun_reads: bool,
    /// whether its agents must declare what a world may change when forking it (`may_change` or
    /// `tenant`: see `scope`)
    pub require_scope: bool,
    /// an agent (a reviewer model's) that looks at its held merges before a person: it approves (the merge goes
    /// through), rejects, or passes one on, with a reason (see `Core::decide_review`)
    pub reviewer: Option<String>,
    /// conditions every row a merge changes in a table must keep, each `(table, condition)`: SQL over the row as it
    /// was (`old.col`) and as the merge leaves it (`new.col`), as a CHECK reads its row (a NULL keeps it); a row that
    /// makes one false waits for a person (see `rowrules`)
    pub row_rules: Vec<(String, String)>,
    /// most rows the merges of every agent keeping to this policy may change in main in an hour, this one's included:
    /// one count for all of them, whatever agent or tenant (see `tally::policy_key`), so a fault every run repeats
    /// (a model update, one trick that works for every customer) stops at the limit
    pub all_rows_per_hour: Option<u64>,
    /// most rows they may delete in an hour, together (as `all_rows_per_hour`)
    pub all_deletes_per_hour: Option<u64>,
    /// most effects they may queue in an hour, together (as `all_rows_per_hour`)
    pub all_effects_per_hour: Option<u64>,
    /// one merge in this many that keeps every rule waits for a person anyway, picked at random: a sample of what
    /// goes through on its own
    pub spot_check: Option<u64>,
    /// most rows one merge may empty a value of (an update setting a column that held something to NULL): a wipe
    /// that counts as an update otherwise
    pub max_nulled: Option<u64>,
    /// columns (table, column) holding text outsiders write (an order's notes, a ticket's body): a world whose agent
    /// read one may have read instructions planted there, so its merge changing an `untrusted_guards` table waits
    /// for a person (see `stale::ReadSet::may_have_read`). `("*", "*")` (set as `'*'`): every text column of every
    /// table, untrusted by default; `(table, "*")`: every text column of one. Rows an agent's merge wrote after it read
    /// such text are tainted (see `tally::Taint`), and reading one counts as reading untrusted text too
    pub untrusted_columns: Vec<(String, String)>,
    /// columns `untrusted_columns` leaves out (those only people or the application write: a status, a name the
    /// application checks), for `'*'` and `table.*`
    pub trusted_columns: Vec<(String, String)>,
    /// tables a world that read an `untrusted_columns` column may not change on its own (empty: any table)
    pub untrusted_guards: Vec<String>,
    /// every merge goes to the reviewer first, not only those the rules hold: it decides each, a person what it
    /// passes on (without a reviewer: a person decides each)
    pub review_every: bool,
    /// the rules whose break only a person may approve past, not the reviewer (see `PERSON_ONLY`); None: the default
    pub person_only: Option<Vec<String>>,
    /// scalar queries (`select sum(amount) from payments`) run on main before an agent's merge and on what it would
    /// leave: one whose result moves more than `max_shift` percent holds it. Judges what a merge does, not only what
    /// it changes
    pub watch_totals: Vec<String>,
    /// how far, in percent, a `watch_totals` result may move in one merge (None: 10)
    pub max_shift: Option<u64>,
    /// a merge changing more rows than this lands only its first this many, then waits `canary_wait` before the rest
    /// may merge: a wrong job shows on a few rows first (see `Core::canary`)
    pub canary: Option<u64>,
    /// how long a canary's rows sit in main before the rest may merge, ms (None: 10 minutes)
    pub canary_wait: Option<u64>,
    /// how long the effects an agent's merge lands wait before a worker may claim them, ms: an UNDO MERGE meanwhile
    /// takes them back unsent (see `outbox`)
    pub effects_delay: Option<u64>,
    /// a merge changing more than this many times the rows the policy's merges usually change (the median of its last
    /// 50) waits for a review: drift, an agent doing something it never did (see `Core::usual_rows`)
    pub max_unusual: Option<u64>,
    pub created: u64,
}

/// The rules a reviewer can't approve a merge past by default (see `MergePolicy::person_only`): deleting, changing
/// the schema, a big delete's count, acting on text outsiders wrote, the limits every agent shares, and the spot
/// checks people sample the reviewer's work with.
pub const PERSON_ONLY: [&str; 9] = [
    "max_deletes",
    "confirm_deletes_over",
    "schema",
    "untrusted",
    "all_rows_per_hour",
    "all_deletes_per_hour",
    "all_effects_per_hour",
    "spot_check",
    "max_shift",
];

/// Every rule a merge can break, by the name `person_only` takes (the settings', and `scope`, `checks` and
/// `stale_reads` for a declared scope, merge checks and changed reads).
pub const RULE_NAMES: [&str; 28] = [
    "max_rows",
    "max_deletes",
    "confirm_deletes_over",
    "rows_per_hour",
    "deletes_per_hour",
    "all_rows_per_hour",
    "all_deletes_per_hour",
    "all_effects_per_hour",
    "max_age",
    "max_effects",
    "effects_per_hour",
    "review_channels",
    "tables",
    "review_tables",
    "review_columns",
    "row_rules",
    "max_nulled",
    "untrusted",
    "schema",
    "overwrite",
    "critical",
    "require_scope",
    "scope",
    "checks",
    "stale_reads",
    "spot_check",
    "max_shift",
    "max_unusual",
];

/// A rule a merge broke: which (by its name in `RULE_NAMES`, or `review_every`), and why, in words.
pub(crate) type Broke = (&'static str, String);

/// What a reviewer decides about a held merge (see `Core::decide_review`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// merged, as held: the version the policy held
    Approve,
    /// not merged; it stays held, with the reason, for a person to see (who may still merge it)
    Reject,
    /// left to a person, with the reason
    Pass,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Approve => "approved",
            Verdict::Reject => "rejected",
            Verdict::Pass => "passed to a person",
        }
    }

    fn parse(s: &str) -> Option<Verdict> {
        [Verdict::Approve, Verdict::Reject, Verdict::Pass].into_iter().find(|v| v.name() == s)
    }
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
    /// the run that made it (see `RUN_KEYS`)
    pub run: Option<Json>,
    /// its policy's reviewer, before a person (see `MergePolicy::reviewer`): who, and what it decided (None: not yet)
    pub reviewer: Option<(String, Option<(Verdict, String)>)>,
    /// the reasons only a person may approve it past (see `MergePolicy::person_only`): a reviewer's approval is
    /// refused while there are any
    pub person: Vec<String>,
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
                "confirm_deletes_over" => self.confirm_deletes_over = limit()?,
                "max_effects" => self.max_effects = limit()?,
                "effects_per_hour" => self.effects_per_hour = limit()?,
                "review_channels" => self.review_channels = list(k, v)?,
                "canary_wait" => self.canary_wait = duration(k, v)?,
                "effects_delay" => self.effects_delay = duration(k, v)?,
                "watch_totals" => self.watch_totals = totals(v)?,
                "max_shift" => self.max_shift = limit()?,
                "canary" => {
                    self.canary = limit()?;
                    if self.canary == Some(0) {
                        return Err(bad("canary is how many rows land first (10: ten), or null for none".into()));
                    }
                }
                "max_unusual" => {
                    self.max_unusual = limit()?;
                    if self.max_unusual == Some(0) {
                        return Err(
                            bad("max_unusual is how many times the usual rows (5: five times), or null".into()),
                        );
                    }
                }
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
                "review_columns" => self.review_columns = columns(k, v)?,
                "row_rules" => self.row_rules = row_rules(v)?,
                "all_rows_per_hour" => self.all_rows_per_hour = limit()?,
                "all_deletes_per_hour" => self.all_deletes_per_hour = limit()?,
                "all_effects_per_hour" => self.all_effects_per_hour = limit()?,
                "spot_check" => {
                    self.spot_check = limit()?;
                    if self.spot_check == Some(0) {
                        return Err(
                            bad("spot_check is one merge in how many (50: one in 50), or null for none".into()),
                        );
                    }
                }
                "max_nulled" => self.max_nulled = limit()?,
                "untrusted_columns" => {
                    self.untrusted_columns = match v {
                        Json::String(s) if s.trim() == "*" => vec![("*".into(), "*".into())],
                        v => columns(k, v)?,
                    }
                }
                "trusted_columns" => self.trusted_columns = columns(k, v)?,
                "untrusted_guards" => self.untrusted_guards = list(k, v)?,
                "review_every" => self.review_every = flag()?,
                "person_only" => self.person_only = person_only(v)?,
                "schema" => self.schema = flag()?,
                "overwrite" => self.overwrite = flag()?,
                "critical" => self.critical = flag()?,
                "check_reads" => self.check_reads = flag()?,
                "rerun_reads" => self.rerun_reads = flag()?,
                "require_scope" => self.require_scope = flag()?,
                "reviewer" => {
                    self.reviewer = match v {
                        Json::Null => None,
                        Json::String(s) if !s.is_empty() => Some(s.clone()),
                        _ => return Err(bad("reviewer must be an agent's name, or null for none".into())),
                    }
                }
                k => {
                    return Err(bad(format!(
                        "merge policies have no setting {k} (max_rows, max_deletes, confirm_deletes_over, rows_per_hour, deletes_per_hour, all_rows_per_hour, all_deletes_per_hour, all_effects_per_hour, max_age, max_effects, effects_per_hour, review_channels, tables, review_tables, review_columns, row_rules, max_nulled, untrusted_columns, trusted_columns, untrusted_guards, spot_check, schema, overwrite, critical, check_reads, rerun_reads, require_scope, reviewer, review_every, person_only, watch_totals, max_shift, canary, canary_wait, effects_delay, max_unusual)"
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
            "confirm_deletes_over": self.confirm_deletes_over,
            "max_effects": self.max_effects, "effects_per_hour": self.effects_per_hour,
            "review_channels": self.review_channels,
            "review_tables": self.review_tables, "review_columns": self.review_columns, "schema": self.schema, "overwrite": self.overwrite,
            "critical": self.critical, "check_reads": self.check_reads, "rerun_reads": self.rerun_reads,
            "require_scope": self.require_scope, "reviewer": self.reviewer, "created": self.created,
            "row_rules": self.row_rules, "all_rows_per_hour": self.all_rows_per_hour,
            "all_deletes_per_hour": self.all_deletes_per_hour, "all_effects_per_hour": self.all_effects_per_hour,
            "spot_check": self.spot_check, "max_nulled": self.max_nulled,
            "untrusted_columns": self.untrusted_columns, "untrusted_guards": self.untrusted_guards,
            "trusted_columns": self.trusted_columns, "watch_totals": self.watch_totals, "max_shift": self.max_shift,
            "canary": self.canary, "canary_wait": self.canary_wait, "effects_delay": self.effects_delay,
            "max_unusual": self.max_unusual,
            "review_every": self.review_every, "person_only": self.person_only,
        })
        .to_string()
    }

    fn from_json(name: &str, raw: &str) -> Option<MergePolicy> {
        let j: Json = serde_json::from_str(raw).ok()?;
        let list =
            |k: &str| j[k].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)).collect();
        // (table, column) and (table, condition) pairs, kept as two-element arrays
        let pairs = |k: &str| -> Vec<(String, String)> {
            j[k].as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string())))
                .collect()
        };
        Some(MergePolicy {
            name: name.into(),
            max_rows: j["max_rows"].as_u64(),
            max_deletes: j["max_deletes"].as_u64(),
            rows_per_hour: j["rows_per_hour"].as_u64(),
            deletes_per_hour: j["deletes_per_hour"].as_u64(),
            confirm_deletes_over: j["confirm_deletes_over"].as_u64(),
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
            rerun_reads: j["rerun_reads"].as_bool().unwrap_or(false),
            require_scope: j["require_scope"].as_bool().unwrap_or(false),
            reviewer: j["reviewer"].as_str().map(String::from),
            row_rules: pairs("row_rules"),
            all_rows_per_hour: j["all_rows_per_hour"].as_u64(),
            all_deletes_per_hour: j["all_deletes_per_hour"].as_u64(),
            all_effects_per_hour: j["all_effects_per_hour"].as_u64(),
            spot_check: j["spot_check"].as_u64().filter(|n| *n > 0),
            max_nulled: j["max_nulled"].as_u64(),
            untrusted_columns: pairs("untrusted_columns"),
            trusted_columns: pairs("trusted_columns"),
            watch_totals: list("watch_totals"),
            max_shift: j["max_shift"].as_u64(),
            canary: j["canary"].as_u64().filter(|n| *n > 0),
            canary_wait: j["canary_wait"].as_u64(),
            effects_delay: j["effects_delay"].as_u64(),
            max_unusual: j["max_unusual"].as_u64().filter(|n| *n > 0),
            untrusted_guards: list("untrusted_guards"),
            review_every: j["review_every"].as_bool().unwrap_or(false),
            person_only: j["person_only"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()),
            created: j["created"].as_u64().unwrap_or(0),
        })
    }

    /// Whether only a person may approve a merge past rule `rule` (see `person_only`, `PERSON_ONLY`).
    pub(crate) fn person_needs(&self, rule: &str) -> bool {
        match &self.person_only {
            Some(rules) => rules.iter().any(|r| r == rule),
            None => PERSON_ONLY.contains(&rule),
        }
    }

    /// The reasons of `broke` only a person may approve past.
    pub(crate) fn for_a_person(&self, broke: &[Broke]) -> Vec<String> {
        broke.iter().filter(|(rule, _)| self.person_needs(rule)).map(|(_, why)| why.clone()).collect()
    }

    /// The `person_only` setting as SHOW MERGE POLICIES gives it: the rules, comma-separated (the default's too).
    pub fn person_only_listed(&self) -> String {
        match &self.person_only {
            Some(rules) => rules.join(","),
            None => PERSON_ONLY.join(","),
        }
    }

    /// The `row_rules` setting as it's written: `payments: new.amount >= -50; prices: ...`.
    pub fn row_rules_listed(&self) -> String {
        self.row_rules
            .iter()
            .map(|(t, e)| format!("{}: {e}", listed(std::slice::from_ref(t))))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// The rows of `rows` (a merge's changes, as JSON: see `sql::describe`) whose update empties a value (a column
    /// that held something set to NULL), past `max_nulled`, as one reason naming the columns and rows.
    pub(crate) fn nulled_broken(&self, rows: &[crate::sql::Described]) -> Option<String> {
        use std::collections::BTreeSet;
        const SHOWN: usize = 5;
        let most = self.max_nulled?;
        let (mut cols, mut keys) = (BTreeSet::new(), vec![]);
        for d in rows.iter().filter(|d| d.id.is_some() && d.change == "update") {
            let (Some(before), Some(after)) = (&d.before, &d.after) else { continue };
            let emptied: Vec<&String> =
                d.columns.iter().filter(|c| !before[c.as_str()].is_null() && after[c.as_str()].is_null()).collect();
            if !emptied.is_empty() {
                cols.extend(emptied.into_iter().map(|c| format!("{}.{c}", d.table)));
                keys.push(shown_key(&d.key));
            }
        }
        if keys.len() as u64 <= most {
            return None;
        }
        let more = if keys.len() > SHOWN { format!(", and {} more", keys.len() - SHOWN) } else { String::new() };
        let cols: Vec<String> = cols.into_iter().collect();
        Some(format!(
            "empties {} in {} (at most {most}): {}{more}",
            cols.join(", "),
            n_rows(keys.len() as u64),
            keys[..keys.len().min(SHOWN)].join(", ")
        ))
    }

    /// Whether a merge changing the tables of `diff` acts on text outsiders wrote: its world read one of
    /// `untrusted_columns` (`read`: a column of them it may have read, as `stale::ReadSet::may_have_read` tells; None
    /// when what it read isn't known, which counts as read), and it changes an `untrusted_guards` table (any, with
    /// none listed). The reason, in words.
    pub(crate) fn untrusted_broken(
        &self,
        read: Option<Vec<(String, String)>>,
        tainted: &[String],
        diff: &[crate::Change],
    ) -> Option<String> {
        use std::collections::BTreeSet;
        if self.untrusted_columns.is_empty() {
            return None;
        }
        let changed: BTreeSet<String> = diff
            .iter()
            .filter(|c| !crate::sql::internal(&c.key) && !crate::outbox::entry(&c.key))
            .filter_map(|c| c.key.split_once('/').map(|(t, _)| t.to_string()))
            .filter(|t| !crate::reserved_table(t))
            .filter(|t| self.untrusted_guards.is_empty() || self.untrusted_guards.contains(t))
            .collect();
        if changed.is_empty() {
            return None;
        }
        let mut what = match read {
            Some(cols) if cols.is_empty() && tainted.is_empty() => return None,
            Some(cols) if cols.is_empty() => String::new(),
            Some(cols) => {
                listed_columns(&cols) + ", which outsiders write"
            }
            None => "rows without a record of which columns (it was forked before its policy listed untrusted_columns, or the database restarted since)".into(),
        };
        if !tainted.is_empty() {
            const SHOWN: usize = 5;
            let more =
                if tainted.len() > SHOWN { format!(", and {} more", tainted.len() - SHOWN) } else { String::new() };
            let rows = format!(
                "rows an agent wrote after reading such text ({}{more})",
                tainted[..tainted.len().min(SHOWN)].join(", ")
            );
            what = match what.is_empty() {
                true => rows,
                false => format!("{what}, and {rows}"),
            };
        }
        let changed: Vec<String> = changed.into_iter().collect();
        Some(format!(
            "read {what}, and changes {} (untrusted reads: a person checks it wasn't told to)",
            listed(&changed)
        ))
    }

    /// The limits every agent keeping to this policy shares (`all_rows_per_hour`, ...) that a merge of `diff` into
    /// main breaks, counted with all their merges into main in the last hour (see `tally::policy_key`).
    pub(crate) fn all_hourly_broken(&self, core: &Core, diff: &[crate::Change]) -> Vec<Broke> {
        if self.all_rows_per_hour.is_none()
            && self.all_deletes_per_hour.is_none()
            && self.all_effects_per_hour.is_none()
        {
            return vec![];
        }
        let (rows, deletes, effects) = counts(diff);
        let (had_rows, had_deletes, had_effects) = core.hourly_key(&crate::tally::policy_key(&self.name));
        let whose = format!("every agent under merge policy {}", self.name);
        let mut out = vec![];
        if let Some(n) = self.all_rows_per_hour.filter(|n| had_rows + rows > *n) {
            out.push((
                "all_rows_per_hour",
                format!(
                    "changes {}, and {whose} changed {had_rows} in the last hour (at most {n} an hour, together)",
                    n_rows(rows)
                ),
            ));
        }
        if let Some(n) = self.all_deletes_per_hour.filter(|n| had_deletes + deletes > *n) {
            out.push((
                "all_deletes_per_hour",
                format!(
                    "deletes {}, and {whose} deleted {had_deletes} in the last hour (at most {n} an hour, together)",
                    n_rows(deletes)
                ),
            ));
        }
        if let Some(n) = self.all_effects_per_hour.filter(|n| had_effects + effects > *n) {
            out.push((
                "all_effects_per_hour",
                format!("queues {effects} effects, and {whose} queued {had_effects} in the last hour (at most {n} an hour, together)"),
            ));
        }
        out
    }

    /// Whether this merge, which keeps every rule, is one `spot_check` picks for a person: one in that many, at random.
    pub(crate) fn spot_picked(&self) -> Option<String> {
        let n = self.spot_check?;
        random().is_multiple_of(n).then(|| format!("picked for a spot check (one merge in {n} that keeps every rule)"))
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
                keys.push(shown_key(&d.key));
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
    pub(crate) fn hourly_broken(&self, core: &Core, agent: &str, diff: &[crate::Change]) -> Vec<Broke> {
        if self.rows_per_hour.is_none() && self.deletes_per_hour.is_none() && self.effects_per_hour.is_none() {
            return vec![];
        }
        let (rows, deletes, effects) = counts(diff);
        let (had_rows, had_deletes, had_effects) = core.hourly(agent);
        // an agent held to a tenant is counted with every agent made for it (see `tally`)
        let whose = match core.tally_of(agent).1 {
            Some(t) => format!("merges for tenant {t}"),
            None => "its merges".to_string(),
        };
        let mut out = vec![];
        if let Some(n) = self.rows_per_hour.filter(|n| had_rows + rows > *n) {
            out.push((
                "rows_per_hour",
                format!(
                    "changes {}, and {whose} changed {had_rows} in the last hour (at most {n} an hour)",
                    n_rows(rows)
                ),
            ));
        }
        if let Some(n) = self.deletes_per_hour.filter(|n| had_deletes + deletes > *n) {
            out.push((
                "deletes_per_hour",
                format!(
                    "deletes {}, and {whose} deleted {had_deletes} in the last hour (at most {n} an hour)",
                    n_rows(deletes)
                ),
            ));
        }
        if let Some(n) = self.effects_per_hour.filter(|n| had_effects + effects > *n) {
            out.push((
                "effects_per_hour",
                format!(
                    "queues {effects} effects, and {whose} queued {had_effects} in the last hour (at most {n} an hour)"
                ),
            ));
        }
        out
    }

    /// The rules a merge of `diff` breaks, in words (none: it may go through). `overwrote`: rows
    /// it would overwrite that its parent changed since the fork.
    pub(crate) fn broken(&self, diff: &[crate::Change], overwrote: usize) -> Vec<String> {
        self.judged(diff, overwrote).into_iter().map(|(_, why)| why).collect()
    }

    /// `broken`, each reason with the rule it's for (see `RULE_NAMES`).
    pub(crate) fn judged(&self, diff: &[crate::Change], overwrote: usize) -> Vec<Broke> {
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
                // (and when an object was made, kept beside it, follows the object)
                _ if crate::sql::made_row(&c.key) => continue,
                // (and so do the planner's statistics VACUUM, ANALYZE and an index build record of a table, which
                // define nothing: see `sql::RELSTATS`)
                _ if c.key.starts_with(crate::sql::RELSTATS) => continue,
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
            out.push(("max_rows", format!("changes {} (at most {n})", n_rows(rows))));
        }
        if let Some(n) = self.max_deletes.filter(|n| deletes > *n) {
            out.push(("max_deletes", format!("deletes {} (at most {n})", n_rows(deletes))));
        }
        // (an agent can't give the count for itself: a person does, approving it)
        if let Some(n) =
            self.confirm_deletes_over.filter(|n| deletes > *n && self.max_deletes.is_none_or(|m| deletes <= m))
        {
            out.push((
                "confirm_deletes_over",
                format!("deletes {} (over {n}: a person approves it by giving the count)", n_rows(deletes)),
            ));
        }
        if let Some(n) = self.max_effects.filter(|n| effects > *n) {
            out.push(("max_effects", format!("queues {effects} effects (at most {n})")));
        }
        if !channels.is_empty() {
            out.push(("review_channels", format!("queues effects on {} (always reviewed)", join(channels))));
        }
        if taken > 0 {
            out.push((
                "review_channels",
                format!("takes back or changes {taken} effects already queued in its parent"),
            ));
        }
        if !outside.is_empty() {
            out.push(("tables", format!("changes {} (only {})", join(outside), listed(&self.tables))));
        }
        if !review.is_empty() {
            out.push(("review_tables", format!("changes {} (always reviewed)", join(review))));
        }
        if !self.schema && !schema.is_empty() {
            out.push(("schema", format!("changes the schema: {}", join(schema))));
        }
        if !self.overwrite && overwrote > 0 {
            out.push(("overwrite", format!("overwrites {overwrote} rows changed in its parent since the fork")));
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
fn columns(k: &str, v: &Json) -> Result<Vec<(String, String)>> {
    let wrong = || {
        bad(format!(
            "{k} must be a list of columns like 'users.email,accounts.owner' (table.column, quoted as SQL does)"
        ))
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

/// `row_rules`' list: text like `'payments: new.amount >= -50; prices: new.price <= old.price * 1.2'` (a table as SQL
/// names it, a colon, and a condition; `;` between them, outside quotes), or an array of `table: condition` each; null
/// for none. Each condition must parse as SQL; whether it binds to its table's columns is known at a merge (a rule
/// that doesn't holds the merge for a person, saying why).
/// A length of time setting (`canary_wait`, `effects_delay`): `'5 minutes'`, ms as a number, or null for none.
fn duration(k: &str, v: &Json) -> Result<Option<u64>> {
    match v {
        Json::Null => Ok(None),
        Json::String(s) => Ok(Some(
            crate::sql::duration_ms(s)
                .ok_or_else(|| bad(format!("{k} must be a length of time like '10 minutes' or '1h', not '{s}'")))?,
        )),
        v => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| bad(format!("{k} must be a length of time like '10 minutes', ms as a number, or null"))),
    }
}

/// `watch_totals`: queries that each give one number, as text kept apart by semicolons, or a JSON array; each checked
/// to be a SELECT.
fn totals(v: &Json) -> Result<Vec<String>> {
    let items: Vec<String> = match v {
        Json::Null => return Ok(vec![]),
        Json::String(s) => split_outside_quotes(s, ';'),
        Json::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(String::from))
            .collect::<Option<_>>()
            .ok_or_else(|| bad("watch_totals must be queries like 'select sum(amount) from payments'".into()))?,
        _ => return Err(bad("watch_totals must be queries like 'select sum(amount) from payments'".into())),
    };
    let items: Vec<String> = items.iter().map(|i| i.trim().to_string()).filter(|i| !i.is_empty()).collect();
    if let Some(q) = items.iter().find(|q| !q.to_ascii_lowercase().starts_with("select")) {
        return Err(bad(format!(
            "watch_totals takes queries that give one number, like 'select count(*) from orders', not '{q}'"
        )));
    }
    Ok(items)
}

/// `row_rules` as a merge policy or a run's scope gives them: (table, condition) pairs, each condition checked to be SQL.
pub(crate) fn parse_row_rules(v: &Json) -> Result<Vec<(String, String)>> {
    row_rules(v)
}

fn row_rules(v: &Json) -> Result<Vec<(String, String)>> {
    let wrong = |why: String| {
        bad(format!(
            "row_rules must be a list like 'payments: new.amount >= -50; prices: new.price <= old.price * 1.2' \
             (a table, a colon, a condition over old.column and new.column){why}"
        ))
    };
    let items: Vec<String> = match v {
        Json::Null => return Ok(vec![]),
        Json::String(s) => split_outside_quotes(s, ';'),
        Json::Array(a) => {
            a.iter().map(|x| x.as_str().map(String::from)).collect::<Option<_>>().ok_or_else(|| wrong(String::new()))?
        }
        _ => return Err(wrong(String::new())),
    };
    let mut out = vec![];
    for item in items.iter().map(|i| i.trim()).filter(|i| !i.is_empty()) {
        let (table, cond) = item.split_once(':').ok_or_else(|| wrong(format!(": '{item}' has no colon")))?;
        let table = match table_names(table.trim()).as_deref() {
            Some([one]) => one.clone(),
            _ => return Err(wrong(format!(": '{}' isn't one table's name", table.trim()))),
        };
        let cond = cond.trim().to_string();
        crate::sql::row_rule_parses(&cond).map_err(|e| wrong(format!(": '{cond}' isn't SQL ({e})")))?;
        out.push((table, cond));
    }
    Ok(out)
}

/// `text` split at `sep` where it isn't inside a quoted string or name (`'...'`, `"..."`).
fn split_outside_quotes(text: &str, sep: char) -> Vec<String> {
    let (mut out, mut cur, mut quote) = (vec![], String::new(), None::<char>);
    for c in text.chars() {
        match (quote, c) {
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.push(c);
            }
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (None, c) if c == sep => out.push(std::mem::take(&mut cur)),
            (_, c) => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// `person_only`'s list: rule names (see `RULE_NAMES`) as text `'max_deletes,schema'` or an array; an empty list
/// lets the reviewer approve past every rule; null: the default (`PERSON_ONLY`).
fn person_only(v: &Json) -> Result<Option<Vec<String>>> {
    let wrong = |what: &str| {
        bad(format!(
            "person_only must be a list of rules like 'max_deletes,schema'{what}, or null for the default ({})",
            PERSON_ONLY.join(", ")
        ))
    };
    let items: Vec<String> = match v {
        Json::Null => return Ok(None),
        // 'none': the reviewer may approve past any rule (as '' does; for tools that read '' as unset)
        Json::String(s) if s.trim().eq_ignore_ascii_case("none") => vec![],
        Json::String(s) => s.split(',').map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()).collect(),
        Json::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(|s| s.trim().to_lowercase()))
            .collect::<Option<_>>()
            .ok_or_else(|| wrong(""))?,
        _ => return Err(wrong("")),
    };
    if let Some(r) = items.iter().find(|r| !RULE_NAMES.contains(&r.as_str())) {
        return Err(wrong(&format!(": there's no rule {r} (the rules: {})", RULE_NAMES.join(", "))));
    }
    Ok(Some(items))
}

/// A number no one can guess ahead: for `spot_check`'s pick.
fn random() -> u64 {
    use std::hash::BuildHasher;
    std::collections::hash_map::RandomState::new().hash_one((disk::now_ms(), std::thread::current().id()))
}

/// Columns as `review_columns` takes them back: `users.email,"Accounts"."Owner"`.
pub fn listed_columns(cols: &[(String, String)]) -> String {
    cols.iter()
        .map(|(t, c)| match (t.as_str(), c.as_str()) {
            ("*", _) => "*".to_string(),
            (_, "*") => format!("{}.*", listed(std::slice::from_ref(t))),
            _ => format!("{}.{}", listed(std::slice::from_ref(t)), listed(std::slice::from_ref(c))),
        })
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
        self.record_security("create merge policy", name);
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
        self.record_security("alter merge policy", name);
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
        self.record_security("drop merge policy", name);
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
        Ok(p.broken(&self.state_diff(&br.base(), &br.snapshot())?, 0).iter().map(|r| for_caller(r)).collect())
    }

    /// The merge policy that asks a person merging world `b` to give the count of rows it deletes, and its
    /// `confirm_deletes_over`: the policy the world's queue entry names (a merge held for review), else its owning
    /// agent's. None: no count is asked.
    pub(crate) fn confirm_deletes(&self, b: &str) -> Result<Option<(String, u64)>> {
        let w = self.world(b)?;
        let queued = w.meta.get(REVIEW).and_then(|r| r["policy"].as_str()).and_then(|p| self.policy(p).ok());
        let owners = || -> Result<Option<MergePolicy>> {
            match w.owner.as_deref().map(|o| self.agent(o)) {
                Some(Ok(a)) => self.policy_of(&a),
                _ => Ok(None),
            }
        };
        let p = match queued {
            Some(p) => Some(p),
            None => owners()?,
        };
        Ok(p.and_then(|p| p.confirm_deletes_over.map(|n| (p.name, n))))
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
    /// `person`: those of `reasons` only a person may approve it past (see `MergePolicy::person_only`).
    pub(crate) fn ask_review(
        &self,
        b: &str,
        p: &str,
        reasons: &[String],
        person: &[String],
        version: u64,
    ) -> Result<()> {
        let v = json!({ "policy": p, "reasons": reasons, "person": person, "at": disk::now_ms(), "version": version });
        self.set_reserved(b, REVIEW, Some(v))?;
        // said on REVIEW_CHANNEL, so a reviewer (or a person's pager) hears of it now rather than at its next look
        let reviewer = self.policy(p).ok().and_then(|p| p.reviewer);
        let note =
            json!({ "world": b, "policy": p, "reviewer": reviewer, "version": version, "person": !person.is_empty() });
        crate::sql::deliver(self, crate::MAIN, 0, &[(REVIEW_CHANNEL.to_string(), note.to_string())]);
        Ok(())
    }

    /// A reviewer's decision on world `b`'s held merge (see `MergePolicy::reviewer`), with its reason: kept with the
    /// hold (SHOW REVIEWS, the audit), and for `Approve` the merge made as the policy held it, at that version, as a
    /// person approving it makes it. Only the policy's reviewer, an admin, or the database's own users decide; an
    /// approval of a world written to (or partly merged) since it was held is refused, as it would merge what wasn't
    /// reviewed. Returns the rows merged (0 unless approved).
    pub fn decide_review(&self, b: &str, verdict: Verdict, reason: &str) -> Result<usize> {
        let w = self.world(b)?;
        let Some(mut r) = w.meta.get(REVIEW).cloned() else {
            return Err(Error::Constraint {
                code: "55000",
                message: format!("{} isn't waiting for review (SHOW REVIEWS lists those that are)", w.name),
            });
        };
        let policy = r["policy"].as_str().unwrap_or_default().to_string();
        let actor = disk::actor();
        if !actor.is_empty() {
            let reviewer = self.policy(&policy).ok().and_then(|p| p.reviewer);
            let admin = self.agent(&actor).is_ok_and(|a| a.may(Can::Admin));
            if reviewer.as_deref() != Some(actor.as_str()) && !admin {
                return Err(refused(format!(
                    "agent {actor} isn't merge policy {policy}'s reviewer, so it can't decide {}'s review",
                    w.name
                )));
            }
        }
        // rules only a person approves a merge past (see `MergePolicy::person_only`): not an agent, a reviewer or an
        // admin agent, however sure it is; it may still reject or pass it on, with its reason
        let person: Vec<String> =
            r["person"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
        if verdict == Verdict::Approve && !actor.is_empty() && !person.is_empty() {
            return Err(refused(format!(
                "{} waits for a person, not a reviewer: it {} (merge policy {policy}'s person_only rules)",
                w.name,
                person.join("; it ")
            )));
        }
        let held = r["version"].as_u64().unwrap_or(0);
        if verdict == Verdict::Approve && (w.version != held || r["partly_merged"].as_bool() == Some(true)) {
            return Err(Error::Constraint {
                code: "40001",
                message: format!(
                    "{} changed since its merge was held (version {held}, now {}): approving it would merge what \
                     wasn't reviewed; it waits for a person",
                    w.name, w.version
                ),
            });
        }
        let by = if actor.is_empty() { Json::Null } else { json!(actor) };
        r["decision"] = json!({ "by": by, "verdict": verdict.name(), "reason": reason, "at": disk::now_ms() });
        self.set_reserved(&w.name, REVIEW, Some(r))?;
        if verdict != Verdict::Approve {
            return Ok(0);
        }
        // past the policy that held it, as a person merging it is; only if it is still as it was held
        let _person = disk::acting_as("");
        self.merge_at(&w.name, crate::Resolve::Fail, Some(held))
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
                    // values-free for an agent asking, a reviewer among them (see `values_free`)
                    reasons: r["reasons"].as_array()?.iter().filter_map(|x| x.as_str().map(for_caller)).collect(),
                    at: r["at"].as_u64().unwrap_or(0),
                    version: r["version"].as_u64().unwrap_or(0),
                    partly_merged: r["partly_merged"].as_bool().unwrap_or(false),
                    now: w.version,
                    owner: w.owner,
                    scope: w.scope,
                    run: w.meta.as_object().and_then(run_of),
                    reviewer: {
                        let decided = r.get("decision").and_then(|d| {
                            Some((Verdict::parse(d["verdict"].as_str()?)?, d["reason"].as_str().unwrap_or("").into()))
                        });
                        let by = r["decision"]["by"].as_str().map(String::from);
                        let named = self.policy(r["policy"].as_str().unwrap_or_default()).ok().and_then(|p| p.reviewer);
                        by.or(named).map(|who| (who, decided))
                    },
                    person: r["person"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|x| x.as_str().map(for_caller))
                        .collect(),
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

    /// Whether the agent may see world `w`'s changes as rows (a diff, its history, a dry run of its merge, what it
    /// read, its review): those show rows as they are, whatever tenant they are, so an agent held to a tenant sees
    /// them only of the worlds it forked itself (its own rows: what it could change). Others: as `may_read`.
    pub fn may_see_changes(&self, w: &str) -> Result<()> {
        self.may_read(w)?;
        if self.agent.tenant.is_none() {
            return Ok(());
        }
        let name = w.split_once('@').map_or(w, |(n, _)| n);
        if self.core.world(name)?.owner.as_deref() == Some(self.agent.name.as_str()) {
            return Ok(());
        }
        Err(refused(format!(
            "agent {} works for one tenant ({}): it sees the changes of the worlds it forked, not {name}'s; read rows \
             with SQL, which shows its tenant's",
            self.agent.name,
            self.agent.tenant_sql().unwrap_or_default()
        )))
    }

    /// For an agent held to a tenant, what lists every world's or agent's doings (SHOW REVIEWS, SHOW AUDIT, SHOW
    /// HISTORY, merge outcomes): row keys and reasons of other tenants' worlds, refused. Others: allowed.
    pub fn may_list_all(&self, what: &str) -> Result<()> {
        match &self.agent.tenant {
            None => Ok(()),
            Some(_) => Err(refused(format!(
                "agent {} works for one tenant ({}): {what} lists other tenants' worlds",
                self.agent.name,
                self.agent.tenant_sql().unwrap_or_default()
            ))),
        }
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
        // an agent held to a tenant forks only that tenant's worlds: declared so, whatever it says
        if let Some(t) = &self.agent.tenant {
            let mine = json!({ t["column"].as_str().unwrap_or_default(): t["value"].clone() });
            if let Some(said) = meta.get("tenant")
                && crate::scope::tenant(said)? != *t
            {
                return Err(refused(format!(
                    "agent {} works for tenant {} only: a world it forks can't be declared another's",
                    self.agent.name,
                    self.agent.tenant_sql().unwrap_or_default()
                )));
            }
            meta.insert("tenant".into(), mine);
        }
        crate::scope::from_meta(self.core, from, &mut meta)?;
        let policy = self.core.policy_of(&self.agent)?;
        if policy.as_ref().is_some_and(|p| p.check_reads || p.rerun_reads) {
            meta.insert(crate::stale::CHECK_READS.into(), Json::Bool(true)); // its policy checks what it reads
        }
        if policy.as_ref().is_some_and(|p| p.rerun_reads) {
            meta.insert(crate::stale::RERUN_READS.into(), Json::Bool(true)); // and runs its SELECTs again
        }
        if policy.as_ref().is_some_and(|p| !p.untrusted_columns.is_empty()) {
            // which columns it reads is kept (not checked against changes, unless its policy checks reads): whether it
            // read text outsiders write is known at its merge (see `MergePolicy::untrusted_broken`)
            meta.insert(crate::stale::WATCH_READS.into(), Json::Bool(true));
        }
        // a run may declare tighter limits than its policy, never looser
        if let (Some(p), Some(s)) = (&policy, meta.get(crate::scope::SCOPE)) {
            let (rows, deletes) = crate::scope::limits(s);
            for (what, mine, theirs) in [("max_rows", rows, p.max_rows), ("max_deletes", deletes, p.max_deletes)] {
                if let (Some(m), Some(t)) = (mine, theirs)
                    && m > t
                {
                    return Err(refused(format!(
                        "{what} = {m} is more than merge policy {} allows ({what} {t}): a run may declare less than its policy, not more",
                        p.name
                    )));
                }
            }
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
        self.core.tenant_first(from, &meta)?;
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
        if let Some(who) = &a.act_as {
            meta.insert(ACT_AS.into(), who.clone());
        }
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

    /// The planner's statistics a VACUUM or ANALYZE in a world records define nothing: no schema change.
    #[test]
    fn table_statistics_are_no_schema_change() {
        let key = format!("{}t", crate::sql::RELSTATS);
        let c = crate::Change { key, before: None, after: Some(r#"{"relpages":1}"#.into()) };
        assert!(super::MergePolicy::default().broken(&[c], 0).is_empty());
    }
}
