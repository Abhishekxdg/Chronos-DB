//! Every world's tenant identity (CHRONOS-PLAN-2026-11-17-r3.md section 3, r3.1 section 1). One of:
//!
//! - `Tenant({"column": c, "value": v})`: the world belongs to that tenant; its merge is held to that tenant's
//!   rows (the `tenant` of its declared scope, `scope::SCOPE`).
//! - `Global`: explicitly no tenant. Main always; and a world made by a person or an agent with no tenant, without
//!   declaring one, from a `Global` world (marked by the reserved key `GLOBAL`, shown to no one as metadata).
//! - `Unassigned`: no identity recorded. Only worlds that existed before the security release, or forked from one
//!   by a person since. `quarantined` when its owner is an agent held to a tenant: the kept SIMULATE / REPLAY worlds
//!   of finding B.
//!
//! Identity is derived from metadata already logged, so opening a folder rewrites nothing and 0.5.5 can still read
//! it. It is set once, when a world is made (`inherit` then `mark`), and never changed afterwards except by
//! `assign`, which takes an `Unassigned` world to a tenant (or to `Global`) once every check passes.

use crate::{Core, Error, MAIN, Result, agents, scope};
use serde_json::{Map, Value as Json, json};

/// Held through an ASSIGN WORLD, its checks and its writes.
static ASSIGNING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Reserved metadata key marking a world `Global` (see the module's comment).
pub(crate) const GLOBAL: &str = "_sonos_tenant_global";
/// Reserved metadata key recording an `assign`: who, when, why, and to what.
pub(crate) const ASSIGNED: &str = "_sonos_tenant_assigned";

#[derive(Clone, Debug, PartialEq)]
pub enum Identity {
    Tenant(Json),
    Global,
    Unassigned { quarantined: bool },
}

impl Identity {
    /// As `SHOW WORLDS` shows it (its `tenant_status`).
    pub fn status(&self) -> &'static str {
        match self {
            Identity::Tenant(_) => "tenant",
            Identity::Global => "global",
            Identity::Unassigned { quarantined: true } => "quarantined",
            Identity::Unassigned { quarantined: false } => "unassigned",
        }
    }

    /// In words, for messages.
    pub fn words(&self) -> String {
        match self {
            Identity::Tenant(t) => format!("tenant {}", tenant_sql(t)),
            Identity::Global => "shared (no tenant)".into(),
            Identity::Unassigned { quarantined: true } => {
                "quarantined: made by an agent held to a tenant, before worlds recorded their tenant".into()
            }
            Identity::Unassigned { quarantined: false } => "unassigned: made before worlds recorded a tenant".into(),
        }
    }
}

/// `{"column": c, "value": v}` as SQL: `c = v`.
pub(crate) fn tenant_sql(t: &Json) -> String {
    format!("{} = {}", t["column"].as_str().unwrap_or_default(), scope::lit(&t["value"]))
}

fn refused(message: String) -> Error {
    Error::Constraint { code: "42501", message }
}

/// Two tenants are one when their column is and their values are the same (`scope::same`: 42 and '42').
pub(crate) fn same_tenant(a: &Json, b: &Json) -> bool {
    a["column"] == b["column"] && scope::same(&a["value"], &b["value"])
}

/// The identity of a world named `name` with metadata `meta` (see the module's comment).
pub(crate) fn of_meta(core: &Core, name: &str, meta: &Map<String, Json>) -> Identity {
    if name == MAIN {
        return Identity::Global;
    }
    if let Some(t) = meta.get(scope::SCOPE).and_then(|s| s.get("tenant")).filter(|t| !t.is_null()) {
        return Identity::Tenant(t.clone());
    }
    if meta.get(GLOBAL) == Some(&Json::Bool(true)) {
        return Identity::Global;
    }
    let quarantined =
        meta.get(agents::OWNER).and_then(Json::as_str).is_some_and(|o| core.agent(o).is_ok_and(|a| a.tenant.is_some()));
    Identity::Unassigned { quarantined }
}

impl Core {
    /// World `w`'s tenant identity (a name or id; `name@when` is the world's; a session's transaction world is the
    /// world it was begun on).
    pub fn identity(&self, w: &str) -> Result<Identity> {
        let mut name = w.split_once('@').map_or(w, |(n, _)| n).to_string();
        loop {
            let (parent, meta) = {
                let m = self.branches.read().unwrap();
                let key = match m.contains_key(&name) {
                    true => name.clone(),
                    false => m.ids.get(&name).cloned().ok_or_else(|| Error::NoBranch(name.clone()))?,
                };
                let e = &m[key.as_str()];
                name = key;
                (e.parent.clone(), e.br.info.read().unwrap().meta.clone())
            };
            match parent {
                Some(p) if name.starts_with("_tx_") => name = p,
                _ => return Ok(of_meta(self, &name, &meta)),
            }
        }
    }

    /// Before a world is made from `from` with metadata `meta` (as given, scope keys not yet resolved) by an agent
    /// held to `creator` (None: a person, or an agent with no tenant): rule T1. A world forked from a tenant's world
    /// is that tenant's (declaring another, or none, is refused); one forked from an unassigned world can't be made
    /// by a tenant agent, nor declared a tenant's (assign the world first). Returns the parent's identity, for `mark`.
    pub(crate) fn inherit(&self, from: &str, meta: &mut Map<String, Json>, creator: Option<&Json>) -> Result<Identity> {
        let parent = self.identity(from)?;
        let said = meta.get("tenant").cloned();
        let said = match &said {
            None => None,
            Some(Json::Null) => Some(None),
            Some(v) => Some(Some(scope::tenant(v)?)),
        };
        match &parent {
            Identity::Tenant(t) => match said {
                None => {
                    let col = t["column"].as_str().unwrap_or_default().to_string();
                    meta.insert("tenant".into(), json!({ col: t["value"].clone() }));
                }
                Some(Some(s)) if same_tenant(&s, t) => {}
                Some(_) => {
                    return Err(refused(format!(
                        "world {from} belongs to tenant {}: a world forked from it is that tenant's too, and can't be \
                         declared another's or none",
                        tenant_sql(t)
                    )));
                }
            },
            // main is every tenant's and no one's: a world forked from it may be declared a tenant's. Any other world
            // with no tenant may hold any tenant's changes: its forks stay without one (assign it, or fork main)
            Identity::Global
                if matches!(said, Some(Some(_)))
                    && self.world(from.split_once('@').map_or(from, |(n, _)| n)).is_ok_and(|w| w.name != MAIN) =>
            {
                return Err(refused(format!(
                    "world {from} has no tenant (it may hold any tenant's changes): a world forked from it can't be \
                     declared a tenant's; fork main for that"
                )));
            }
            Identity::Global => {}
            Identity::Unassigned { .. } => {
                if creator.is_some() {
                    return Err(refused(format!(
                        "world {from} is {}: no agent held to a tenant may fork it",
                        parent.words()
                    )));
                }
                if matches!(said, Some(Some(_))) {
                    return Err(refused(format!(
                        "world {from} is {}: a world forked from it can't be declared a tenant's; ASSIGN WORLD {from} \
                         first",
                        parent.words()
                    )));
                }
            }
        }
        Ok(parent)
    }
}

/// After `inherit` and the scope's resolution (`scope::from_meta`): a world with no tenant forked from a `Global`
/// world is marked `Global`. One forked from an unassigned world stays unassigned (made by a person: see
/// `inherit`).
pub(crate) fn mark(parent: &Identity, meta: &mut Map<String, Json>) {
    let tenant = meta.get(scope::SCOPE).and_then(|s| s.get("tenant")).is_some_and(|t| !t.is_null());
    if !tenant && *parent == Identity::Global {
        meta.insert(GLOBAL.into(), Json::Bool(true));
    }
}

/// ASSIGN WORLD's target: a tenant, or no tenant (`Global`).
pub enum AssignTo {
    Tenant(Json),
    Global,
}

impl Core {
    /// `ASSIGN WORLD w TO TENANT 'col = v' | TO GLOBAL [INCLUDING UNTENANTED TABLES] REASON '...'` (rule T3, the
    /// checks of section 3.3): an unassigned world, and every world forked from it, become that tenant's (or no
    /// tenant's) once each passes every check; else nothing changes. A person or an admin only. Returns the worlds
    /// assigned.
    pub fn assign_world(&self, w: &str, to: AssignTo, including_untenanted: bool, reason: &str) -> Result<Vec<String>> {
        let actor = crate::disk::actor();
        if !actor.is_empty() && !self.agent(&actor).is_ok_and(|a| a.may(agents::Can::Admin)) {
            return Err(refused(format!(
                "agent {actor} may not assign a world's tenant: only a person or an admin may"
            )));
        }
        if reason.trim().is_empty() {
            return Err(crate::bad_meta("ASSIGN WORLD needs a REASON: why its rows are the tenant's".into()));
        }
        // one ASSIGN WORLD at a time, checks through writes: two at once could each find a world unassigned
        let _one = ASSIGNING.lock().unwrap_or_else(|e| e.into_inner());
        let name = self.world(w)?.name;
        let mut tree = vec![name.clone()];
        tree.extend(self.descendants(&name).into_iter().filter(|d| !d.starts_with("_tx_")));
        // 1. every one of them unassigned
        for x in &tree {
            let id = self.identity(x)?;
            if !matches!(id, Identity::Unassigned { .. }) {
                return Err(refused(format!(
                    "world {x} is {}: only a world made before worlds recorded a tenant can be assigned one",
                    id.words()
                )));
            }
        }
        // changes are counted from where the line of unassigned worlds left an assigned one (main, a tenant's or
        // a global world): a world forked from an unassigned world inherits what that world changed, and its own
        // diff doesn't show it
        let mut since = name.clone();
        while let Some(p) = self.world(&since)?.parent
            && matches!(self.identity(&p)?, Identity::Unassigned { .. })
        {
            since = p;
        }
        if let AssignTo::Tenant(t) = &to {
            let only = json!({ "tenant": t });
            for x in &tree {
                // 2-4. every row it changed since its fork is the tenant's, before and after; no definition, grant,
                // owner, policy, rule or check changed (all of them are outside any scope: `scope::outside`)
                let mut reasons = self.strays(x, &since, &only, including_untenanted)?;
                let privileges = self.privilege_changes(x, &since)?;
                if !privileges.is_empty() {
                    reasons.push(format!(
                        "changes grants, owners, roles, row security, policies or checks ({})",
                        privileges.len()
                    ));
                }
                // 6. a simulation from a past moment: its rows put back as they were are in the diff, so 2-4 cover
                // them; said so in the reason
                if !reasons.is_empty() {
                    let past = self
                        .world(x)?
                        .meta
                        .get("sim")
                        .and_then(|s| s["base"].as_str())
                        .is_some_and(|b| b.contains('@'));
                    return Err(refused(format!(
                        "world {x} can't be assigned to tenant {}{}: {}",
                        tenant_sql(t),
                        if past { " (simulated from a past moment: every tenant's rows as they were)" } else { "" },
                        reasons.join("; ")
                    )));
                }
            }
        }
        // all pass: record it, each in its own metadata write (logged as any other)
        let by = if actor.is_empty() { "owner".to_string() } else { actor.clone() };
        let record = json!({ "by": by, "at": crate::disk::now_ms(), "reason": reason, "to": match &to {
            AssignTo::Tenant(t) => t.clone(),
            AssignTo::Global => Json::Null,
        } });
        let assign = |meta: &mut Map<String, Json>| -> Result<()> {
            // still unassigned as it is written (`identity` from the world's own record)
            let tenant = meta.get(scope::SCOPE).and_then(|s| s.get("tenant")).is_some_and(|t| !t.is_null());
            if tenant || meta.contains_key(GLOBAL) || meta.contains_key(ASSIGNED) {
                return Err(refused("the world was given a tenant, or none, while it was being assigned".into()));
            }
            match &to {
                AssignTo::Tenant(t) => {
                    let mut s = meta.get(scope::SCOPE).and_then(Json::as_object).cloned().unwrap_or_default();
                    s.insert("tenant".into(), t.clone());
                    meta.insert(scope::SCOPE.into(), Json::Object(s));
                }
                AssignTo::Global => {
                    meta.insert(GLOBAL.into(), Json::Bool(true));
                }
            }
            meta.insert(ASSIGNED.into(), record.clone());
            Ok(())
        };
        // every world's new metadata within its limit before any is written: none of the tree, or all of it
        for x in &tree {
            // as stored, internal keys too (`World::meta` leaves out the scope, owner and the like, which count)
            let mut meta = {
                let m = self.branches.read().unwrap();
                let e = m.get(x).ok_or_else(|| Error::NoBranch(x.clone()))?;
                e.br.info.read().unwrap().meta.clone()
            };
            assign(&mut meta)?;
            if Json::Object(meta).to_string().len() > crate::META_MAX {
                return Err(crate::bad_meta(format!(
                    "world {x}'s metadata would go over {} bytes as JSON with the assignment recorded: nothing \
                     was assigned",
                    crate::META_MAX
                )));
            }
        }
        for x in &tree {
            self.put_meta(x, |m| assign(m))?;
            let to_words = match &to {
                AssignTo::Tenant(t) => format!("tenant {}", tenant_sql(t)),
                AssignTo::Global => "no tenant".into(),
            };
            self.record_security("assign world", &format!("{x} to {to_words}: {reason}"));
        }
        Ok(tree)
    }
}
