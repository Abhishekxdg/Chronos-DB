//! Certified merges: evidence attached to a world, a merge proposed and bound to a certificate, approvals bound to
//! that certificate, and the merge made as the proposing agent once approved. Also the memory of decisions that
//! didn't land (tombstones) and the provenance of those that did.
//!
//!   ATTACH EVIDENCE TO WORLD w (kind = 'ticket', ...) ─> w's `_sonos_evidence` (fixed once proposed)
//!   PROPOSE MERGE WORLD w ─> `_sonos_proposals/<id>` on main: certificate = blake3(canonical {
//!         diff digest (every row the world changed, the database's own included), read set, base,
//!         evidence, scope, policy name + blake3 of its row }), the dry run's verdict, status open;
//!         w frozen for agents (writes, forks, merges past it: 55000) while open or approved
//!   APPROVE | REJECT PROPOSAL 'id' ─> an approval naming the certificate it saw (derived again: one that differs
//!         is refused, 40001)
//!   MERGE PROPOSAL 'id' ─> the certificate derived again (differs: 40001), then merged as the proposing agent: its
//!         policy judged again, with only the rules the approval covers lifted, and only for that diff (checked
//!         again under the merge's locks, see `Core::merge_locked`)
//!
//! Proposal and tombstone rows are rows of main of the database's own. A proposal merged into main, in a folder whose
//! log holds merges physically (`disk::Op::Applied`), has its `merged` row written in the merge's own record, beside
//! its ledger row (see `merged_row`): a crash leaves the merge with the proposal merged, or neither. Every other step
//! (proposed, approved, rejected, withdrawn, tombstones, and `merged` for a merge into another world or logged as
//! before `Op::Applied`) goes through the ordinary write path, after the step it records: a crash between a merge
//! into another world and its proposal's `merged` leaves the proposal approved with its world gone.

use crate::{Change, Core, Error, MAIN, MergeOptions, Result, World, agents, disk, scope};
use serde_json::{Value as Json, json};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

/// World metadata key: the evidence attached to it (see `Core::attach_evidence`).
pub(crate) const EVIDENCE: &str = "_sonos_evidence";
/// World metadata key: the id of its latest proposal (see `Core::propose_merge`).
pub(crate) const PROPOSAL: &str = "_sonos_proposal";
/// Main's rows of proposals, by id.
pub(crate) const PROPOSALS: &str = "_sonos_proposals";
/// Main's rows of worlds that didn't make it (discarded, a merge refused, a proposal rejected).
pub(crate) const TOMBSTONES: &str = "_sonos_tombstones";
/// How long after a strict proposal a person may approve it, ms, when its policy doesn't say (`strict_window`).
const STRICT_WINDOW: u64 = 15 * 60_000;
/// The rule that supersedes a strict proposal: its target took a commit since it was proposed (E7).
pub(crate) const STRICT_TARGET: &str = "strict_target";
/// The settings evidence takes.
const EVIDENCE_KEYS: [&str; 4] = ["kind", "uri", "sha256", "note"];
/// Most evidence one world may carry.
const EVIDENCE_MAX: usize = 100;

/// An approval a merge in progress on this thread carries (see `Core::merge_proposal`): the world, the digest of
/// the rows it was approved for, and whether a person approved (who lifts every rule, not only the reviewer's).
#[derive(Clone)]
pub(crate) struct Approved {
    pub world: String,
    pub digest: String,
    pub person: bool,
    /// the proposal's row (key, and the proposal as it will be once merged, short of its `rows` and `at`): written
    /// in the merge's own log record where it can be (see `merged_row`)
    pub merged: Option<(String, Json)>,
    /// a strict proposal's (rule E7): the version its target must still be at, checked under the merge's locks
    pub strict_version: Option<u64>,
}

/// Held while a proposal's status is read, judged and written (decided, withdrawn, merged): two at once would each
/// read it open, and the second's write would undo the first (a rejected proposal approved, a merged one withdrawn).
static DECIDING: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    static APPROVED: RefCell<Option<Approved>> = const { RefCell::new(None) };
    /// The merge of the approval on this thread wrote its proposal's `merged` row itself (see `merged_row`).
    static CARRIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The approval the merge of world `b` on this thread carries, if any.
pub(crate) fn approval_for(b: &str) -> Option<Approved> {
    APPROVED.with_borrow(|a| a.clone().filter(|a| a.world == b))
}

/// The `merged` row of the proposal whose approved merge of world `b` is being published on this thread, `rows` rows
/// merged at `at`: for `Core::publish` to write in the merge's own `Op::Applied` record, beside its ledger row, so the
/// proposal is merged exactly when its merge is (see the module docs). Taking it marks it written: `merge_proposal`
/// then doesn't write it again.
pub(crate) fn merged_row(b: &str, rows: usize, at: u64) -> Option<(String, Option<String>)> {
    let (key, mut p) = approval_for(b)?.merged?;
    p["merged"]["rows"] = json!(rows);
    p["merged"]["at"] = json!(at);
    CARRIED.set(true);
    Some((key, Some(p.to_string())))
}

struct Carrying;

impl Drop for Carrying {
    fn drop(&mut self) {
        APPROVED.set(None);
        CARRIED.set(false);
    }
}

/// Rules an approval never lifts: merge checks (no one merges past one), reads that changed since the fork (the
/// decision rests on what's gone), and evidence a policy requires (attach it).
pub(crate) fn never_lifted(rule: &str) -> bool {
    matches!(rule, "checks" | "stale_reads" | "require_evidence")
}

/// A digest of `diff`, every row of it (the database's own too): key, before and after, length-prefixed, in key order.
pub(crate) fn diff_digest(diff: &[Change]) -> String {
    let mut rows: Vec<&Change> = diff.iter().collect();
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    let mut h = blake3::Hasher::new();
    h.update(b"chronos-diff-v1");
    let mut put = |b: &[u8]| {
        h.update(&(b.len() as u64).to_le_bytes());
        h.update(b);
    };
    for c in rows {
        put(c.key.as_bytes());
        for v in [&c.before, &c.after] {
            match v {
                None => put(b"\0none"),
                Some(v) => put(format!("\x01{v}").as_bytes()),
            }
        }
    }
    h.finalize().to_hex().to_string()
}

/// `j` with every object's keys in order: the same value, the same text.
fn canonical(j: &Json) -> Json {
    match j {
        Json::Object(m) => {
            let sorted: std::collections::BTreeMap<&String, Json> = m.iter().map(|(k, v)| (k, canonical(v))).collect();
            Json::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Json::Array(a) => Json::Array(a.iter().map(canonical).collect()),
        v => v.clone(),
    }
}

fn hash_text(prefix: &str, text: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(prefix.as_bytes());
    h.update(text.as_bytes());
    h.finalize().to_hex().to_string()
}

fn new_id(prefix: &str, seed: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let h = hash_text("chronos-id", &format!("{seed}/{}/{n}/{}", disk::now_ms(), std::process::id()));
    format!("{prefix}{}", &h[..16])
}

fn bad(message: String) -> Error {
    Error::Constraint { code: "22023", message }
}

fn state(message: String) -> Error {
    Error::Constraint { code: "55000", message } // Postgres's object_not_in_prerequisite_state
}

fn refused(message: String) -> Error {
    Error::Constraint { code: "42501", message }
}

fn changed(message: String) -> Error {
    Error::Constraint { code: "40001", message }
}

fn no_proposal(id: &str) -> Error {
    Error::Constraint { code: "42704", message: format!("proposal '{id}' does not exist (SHOW PROPOSALS lists them)") }
}

/// A strict proposal superseded: its target took a commit since it was proposed (rule E7).
fn superseded(id: &str, target: &str) -> Error {
    changed(format!(
        "proposal '{id}' is superseded ({STRICT_TARGET}): {target} took a commit since it was proposed, and a strict \
         proposal (one queueing an effect on a strict channel) is bound to {target} as it was; propose again"
    ))
}

/// Whether a proposal in `status` holds its world: open, or approved and not merged yet.
fn holding(status: &str) -> bool {
    matches!(status, "open" | "approved")
}

fn texts(j: &Json) -> Vec<String> {
    j.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect()
}

impl Core {
    fn proposal_row(&self, id: &str) -> Result<Json> {
        let raw = self.view(MAIN)?.get(&format!("{PROPOSALS}/{id}"))?.ok_or_else(|| no_proposal(id))?;
        serde_json::from_str(&raw).map_err(|e| bad(format!("proposal '{id}' can't be read: {e}")))
    }

    /// Writes rows of the database's own to main, as no one (the steps of a proposal, tombstones): the actor who
    /// caused them is in the rows, and the audit trail shows only what agents did themselves.
    fn put_own(&self, rows: Vec<(String, Option<String>)>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let _nobody = disk::acting_as("");
        self.batch_with(MAIN, |_| Ok::<_, Error>((rows, ())))?;
        Ok(())
    }

    fn put_proposal(&self, p: &Json) -> Result<()> {
        let id = p["id"].as_str().unwrap_or_default();
        self.put_own(vec![(format!("{PROPOSALS}/{id}"), Some(p.to_string()))])
    }

    /// Every proposal, oldest first.
    pub fn proposals(&self) -> Result<Vec<Json>> {
        let prefix = format!("{PROPOSALS}/");
        let mut out: Vec<Json> = self
            .view(MAIN)?
            .prefix(&prefix)?
            .filter_map(|r| r.ok().and_then(|(_, v)| serde_json::from_str::<Json>(&v).ok()))
            .collect();
        out.sort_by_key(|p| p["created"].as_u64().unwrap_or(0));
        Ok(out)
    }

    /// World `w`'s latest proposal, if it has one.
    fn proposal_of(&self, w: &World) -> Option<Json> {
        let id = w.meta.get(PROPOSAL)?.as_str()?;
        self.proposal_row(id).ok()
    }

    /// Refuses a change to world `w` while a proposal of its merge is open or approved: what was proposed is what
    /// merges (55000). For agents' writes, forks and merges (see `Acting`).
    pub(crate) fn frozen(&self, w: &str) -> Result<()> {
        let Ok(world) = self.world(w) else { return Ok(()) };
        match self.proposal_of(&world) {
            // its own approved merge, under way on this thread (see `merge_proposal`)
            Some(p) if approval_for(&world.name).is_some() && p["status"] == "approved" => Ok(()),
            Some(p) if holding(p["status"].as_str().unwrap_or_default()) => Err(state(format!(
                "world {} has a proposal open ({}, {}): it stays as proposed until it's merged, rejected or withdrawn \
                 (WITHDRAW PROPOSAL '{}' to change it)",
                world.name,
                p["id"].as_str().unwrap_or_default(),
                p["status"].as_str().unwrap_or_default(),
                p["id"].as_str().unwrap_or_default(),
            ))),
            _ => Ok(()),
        }
    }

    /// Attaches evidence to world `w` (a ticket, an order snapshot: what a decision rests on), kept in its metadata:
    /// `kind` (required), `uri`, `sha256` and `note`, with an id, when and by whom (the acting agent, or `person`).
    /// The content stays where the URI points; the hash is what lasts. Refused once a merge of the world was proposed
    /// (until it's withdrawn or rejected) and after one merged. Returns the item.
    pub fn attach_evidence(&self, w: &str, spec: &Json) -> Result<Json> {
        let world = self.world(w)?;
        if world.name == MAIN {
            return Err(bad("evidence is attached to a world before its merge, not to main".into()));
        }
        let m = spec.as_object().ok_or_else(|| bad("evidence is (kind = '...', uri = '...', ...)".into()))?;
        if let Some(k) = m.keys().find(|k| !EVIDENCE_KEYS.contains(&k.as_str())) {
            return Err(bad(format!("evidence has no setting {k} (kind, uri, sha256, note)")));
        }
        let text = |k: &str| -> Result<Option<String>> {
            match m.get(k) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(bad(format!("evidence's {k} must be text"))),
            }
        };
        let kind = text("kind")?.filter(|k| !k.trim().is_empty()).ok_or_else(|| {
            bad("evidence needs a kind (kind = 'ticket', 'order_snapshot', ...): policies ask for kinds".into())
        })?;
        let sha = text("sha256")?;
        if let Some(s) = &sha
            && !(s.len() <= 128 && s.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Err(bad("evidence's sha256 must be a hash in hex".into()));
        }
        if let Some(p) = self.proposal_of(&world) {
            let status = p["status"].as_str().unwrap_or_default();
            if holding(status) || status == "merged" {
                return Err(state(format!(
                    "world {}'s evidence is fixed: its merge was proposed ({}, {status})",
                    world.name,
                    p["id"].as_str().unwrap_or_default()
                )));
            }
        }
        let actor = disk::actor();
        let item = json!({
            "id": new_id("e", &world.id),
            "kind": kind.trim(),
            "uri": text("uri")?,
            "sha256": sha.map(|s| s.to_ascii_lowercase()),
            "note": text("note")?,
            "at": disk::now_ms(),
            "by": if actor.is_empty() { "person".to_string() } else { actor },
        });
        let it = item.clone();
        self.put_meta(&world.name, move |meta| {
            let list = meta.entry(EVIDENCE).or_insert_with(|| json!([]));
            let Some(a) = list.as_array_mut() else { return Err(bad("evidence can't be read".into())) };
            if a.len() >= EVIDENCE_MAX {
                return Err(bad(format!("a world carries at most {EVIDENCE_MAX} pieces of evidence")));
            }
            a.push(it);
            Ok(())
        })?;
        Ok(item)
    }

    /// Who a merge of `w` proposed by the acting agent (or a person) is made as: the agent proposing it, or for a
    /// person, the world's owner if an agent owns it (empty: no one, the database's own users).
    pub(crate) fn merge_as(&self, w: &World) -> String {
        let actor = disk::actor();
        if !actor.is_empty() {
            return actor;
        }
        w.owner.clone().filter(|o| self.agent(o).is_ok()).unwrap_or_default()
    }

    /// World `w`'s read set now, as its certificate takes it (null: it doesn't keep one).
    pub(crate) fn reads_of(&self, w: &str) -> Json {
        match self.recorder(w) {
            Some(r) => serde_json::to_value(r.lock().unwrap().by_table()).unwrap_or(Json::Null),
            None => Json::Null,
        }
    }

    /// The certificate of world `w`'s merge into `into` (None: its parent) as `merge_as` would make it, with `reads`
    /// (its read set, as kept at the proposal): its hash and what it's over.
    pub(crate) fn certificate(
        &self,
        w: &str,
        into: Option<&str>,
        merge_as: &str,
        reads: &Json,
    ) -> Result<(String, Json)> {
        let world = self.world(w)?;
        let br = self.branch(&world.name)?;
        let diff = self.state_diff(&br.base(), &br.snapshot())?;
        let (rows, deletes, effects) = agents::counts(&diff);
        let target = match into {
            Some(t) => self.world(t)?,
            None => self.world(world.parent.as_deref().unwrap_or(MAIN))?,
        };
        let meta = br.info.read().unwrap().meta.clone();
        let policy = match self.agent(merge_as).ok().and_then(|a| a.policy) {
            Some(name) => {
                let raw = self.view(MAIN)?.get(&format!("_sonos_policies/{name}"))?;
                json!({ "name": name, "hash": raw.map(|r| hash_text("chronos-policy-v1", &r)) })
            }
            None => Json::Null,
        };
        let parts = json!({
            "version": "chronos-certificate-v1",
            "diff": diff_digest(&diff),
            "counts": { "rows": rows, "deletes": deletes, "effects": effects },
            "reads": if reads.is_null() { Json::Null } else { json!(hash_text("chronos-reads-v1", &canonical(reads).to_string())) },
            "base": { "world": world.id, "rebased": meta.get(crate::REBASED).cloned().unwrap_or(Json::Null) },
            "target": { "name": target.name, "world": target.id },
            "evidence": meta.get(EVIDENCE).cloned().unwrap_or(json!([])),
            "scope": meta.get(scope::SCOPE).cloned().unwrap_or(Json::Null),
            "merge_as": merge_as,
            "policy": policy,
        });
        let hash = hash_text("chronos-certificate-v1", &canonical(&parts).to_string());
        Ok((hash, parts))
    }

    /// A dry run of world `w`'s merge into `into` as `merge_as` would make it (its policy judging it).
    pub(crate) fn preview_as(&self, w: &str, into: Option<&str>, merge_as: &str) -> Result<crate::MergePlan> {
        let _as = disk::acting_as(merge_as);
        self.merge_preview(w, &MergeOptions { into: into.map(String::from), ..Default::default() })
    }

    /// What a dry run says of a merge: `passes`, `held` (its policy sends it to a review), `conflicts` or `refused`,
    /// with the reasons.
    pub(crate) fn verdict(plan: &crate::MergePlan) -> (&'static str, Vec<String>) {
        let mut reasons: Vec<String> = plan.reasons.iter().map(|r| agents::for_people(r)).collect();
        let verdict = if plan.conflicts > 0 {
            reasons.push(format!("{} rows both sides changed", plan.conflicts));
            "conflicts"
        } else if plan.policy.is_some() && !plan.reasons.is_empty() {
            "held"
        } else if let Some(b) = &plan.blocked {
            if reasons.is_empty() {
                reasons.push(agents::for_people(b));
            }
            "refused"
        } else {
            "passes"
        };
        (verdict, reasons)
    }

    /// Proposes world `w`'s merge into `into` (None: its parent): a row on main with the certificate of what it
    /// would merge, the dry run's verdict and reasons as the proposing agent's merge, status open. The world is then
    /// fixed for agents until the proposal is merged, rejected or withdrawn. Returns the row.
    pub fn propose_merge(&self, w: &str, into: Option<&str>) -> Result<Json> {
        let world = self.world(w)?;
        if world.name == MAIN {
            return Err(Error::IsMain);
        }
        if let Some(p) = self.proposal_of(&world)
            && holding(p["status"].as_str().unwrap_or_default())
        {
            return Err(state(format!(
                "world {} has a proposal open already ({})",
                world.name,
                p["id"].as_str().unwrap_or_default()
            )));
        }
        let actor = disk::actor();
        let merge_as = self.merge_as(&world);
        let reads = self.reads_of(&world.name);
        let (cert, parts) = self.certificate(&world.name, into, &merge_as, &reads)?;
        let plan = self.preview_as(&world.name, into, &merge_as)?;
        let (verdict, reasons) = Self::verdict(&plan);
        // strict (rules E7 and E8): its effects go out on a payment-like channel. Only the whole world, into its parent
        let (strict, window) = self.strict_effects(&world.name, &merge_as)?;
        if !strict.is_empty() && parts["target"]["name"].as_str() != world.parent.as_deref() {
            return Err(bad(format!(
                "world {} queues effects on {} (strict channels): its proposal merges into its parent, not into {}",
                world.name,
                strict.join(", "),
                parts["target"]["name"].as_str().unwrap_or_default()
            )));
        }
        let now = disk::now_ms();
        let id = new_id("p", &cert);
        let mut p = json!({
            "id": id,
            "world": world.name,
            "world_id": world.id,
            "into": into,
            "target": parts["target"]["name"],
            "proposer": if actor.is_empty() { "person".to_string() } else { actor },
            "merge_as": merge_as,
            "owner": world.owner,
            "version": world.version,
            "certificate": cert,
            "parts": parts,
            "reads": reads,
            "status": "open",
            "verdict": verdict,
            "reasons": reasons,
            "policy": plan.policy,
            "approvals": [],
            "created": now,
            "run": world.meta.as_object().and_then(agents::run_of),
            "parent": world.parent,
            "born": world.created,
            "depth": world.depth,
            "strict": if strict.is_empty() { Json::Null } else { json!({ "channels": strict, "until": now + window }) },
        });
        match p["strict"].is_object() {
            true => self.put_bound(&mut p, None)?,
            false => self.put_proposal(&p)?,
        }
        self.set_reserved(&world.name, PROPOSAL, Some(json!(id)))?;
        Ok(p)
    }

    /// The certificate of proposal `p`'s world now, derived as at the proposal (its read set as kept then), and
    /// refused (40001) when the world was written to or the certificate differs.
    fn still_certified(&self, p: &Json, doing: &str) -> Result<World> {
        let id = p["id"].as_str().unwrap_or_default();
        let world = self
            .world(p["world_id"].as_str().unwrap_or_default())
            .map_err(|_| state(format!("proposal '{id}': its world is gone (merged or discarded past it)")))?;
        let gone = |why: String| {
            changed(format!(
                "world {} changed since proposal '{id}' ({why}): {doing} it would cover what wasn't proposed; \
                 withdraw it and propose again",
                world.name
            ))
        };
        let version = p["version"].as_u64().unwrap_or(0);
        if world.version != version {
            return Err(gone(format!("version {version}, now {}", world.version)));
        }
        let (cert, _) =
            self.certificate(&world.name, p["into"].as_str(), p["merge_as"].as_str().unwrap_or_default(), &p["reads"])?;
        if cert != p["certificate"].as_str().unwrap_or_default() {
            return Err(gone("its certificate differs: rows, evidence, scope or policy".into()));
        }
        Ok(world)
    }

    /// The strict channels (see `outbox::strict`) world `w`'s merge as `merge_as` would queue effects on, once each,
    /// and how long a person has to approve it (its policy's `strict_window`).
    pub(crate) fn strict_effects(&self, w: &str, merge_as: &str) -> Result<(Vec<String>, u64)> {
        let br = self.branch(w)?;
        let policy = self.agent(merge_as).ok().and_then(|a| a.policy).and_then(|n| self.policy(&n).ok());
        let mut channels: Vec<String> = self
            .state_diff(&br.base(), &br.snapshot())?
            .into_iter()
            .filter(|c| crate::outbox::entry(&c.key) && c.before.is_none())
            .filter_map(|c| crate::outbox::channel_of(c.after.as_deref()?))
            .filter(|ch| crate::outbox::strict(ch, policy.as_ref()))
            .collect();
        channels.sort();
        channels.dedup();
        Ok((channels, policy.and_then(|p| p.strict_window).unwrap_or(STRICT_WINDOW)))
    }

    /// Writes strict proposal `p`'s row bound to its target's version (rule E7): `expect` is the version the target
    /// must be at (None: proposing, nothing to check yet), read under main's writer lock, which merges into main take
    /// too; the row keeps the version the target is at once the write lands (the row is on main: a target of main
    /// moves with it). A commit landing meanwhile can only make a later check fail, never pass one.
    fn put_bound(&self, p: &mut Json, expect: Option<u64>) -> Result<()> {
        let id = p["id"].as_str().unwrap_or_default().to_string();
        let target = p["target"].as_str().unwrap_or(MAIN).to_string();
        let br = self.branch(&target).map_err(|_| superseded(&id, &target))?;
        let own = u64::from(target == MAIN);
        let _nobody = disk::acting_as("");
        self.batch_with(MAIN, |_| {
            let now = br.version.load(Ordering::Relaxed);
            if expect.is_some_and(|e| e != now) {
                return Err(superseded(&id, &target));
            }
            p["strict"]["version"] = json!(now + own);
            Ok((vec![(format!("{PROPOSALS}/{id}"), Some(p.to_string()))], ()))
        })?;
        Ok(())
    }

    /// Ends strict proposal `p` (as it was before this step) as `status` (`expired`, `superseded`) by `rule`, freeing
    /// its world, and gives back `e`, the refusal that says so.
    fn end_strict(&self, mut p: Json, status: &str, rule: &str, e: Error) -> Error {
        p["status"] = json!(status);
        p["ended"] = json!({ "at": disk::now_ms(), "rule": rule });
        let _ = self.put_proposal(&p);
        self.unfreeze(&p);
        e
    }

    /// Withdraws proposal `id`: the proposing agent, an admin agent, or a person. Its world is the agent's to change
    /// again.
    pub fn withdraw_proposal(&self, id: &str) -> Result<()> {
        let _one = DECIDING.lock().unwrap_or_else(|e| e.into_inner());
        let mut p = self.proposal_row(id)?;
        let status = p["status"].as_str().unwrap_or_default().to_string();
        if !holding(&status) {
            return Err(state(format!("proposal '{id}' is {status}: only an open one is withdrawn")));
        }
        let actor = disk::actor();
        if !actor.is_empty()
            && p["proposer"].as_str() != Some(actor.as_str())
            && !self.agent(&actor).is_ok_and(|a| a.may(agents::Can::Admin))
        {
            return Err(refused(format!("agent {actor} didn't propose '{id}', so it can't withdraw it")));
        }
        p["status"] = json!("withdrawn");
        p["ended"] = json!({ "at": disk::now_ms(), "by": if actor.is_empty() { "person".into() } else { actor } });
        self.put_proposal(&p)?;
        self.unfreeze(&p);
        Ok(())
    }

    /// Takes proposal `p`'s mark off its world, if it's still there and still its.
    fn unfreeze(&self, p: &Json) {
        if let Ok(w) = self.world(p["world_id"].as_str().unwrap_or_default())
            && w.meta.get(PROPOSAL).and_then(Json::as_str) == p["id"].as_str()
        {
            let _ = self.set_reserved(&w.name, PROPOSAL, None);
        }
    }

    /// Approves (or rejects) proposal `id`, with `reason`: a person (`role`: the session's user), the proposing
    /// agent's merge policy's reviewer, or an admin agent; never the agent that proposed it or owns the world. An
    /// approval names the certificate it saw: one derived now that differs from the proposal's is refused. A rejected
    /// proposal frees its world and leaves a tombstone.
    pub fn decide_proposal(&self, id: &str, approve: bool, reason: &str, role: Option<&str>) -> Result<()> {
        let _one = DECIDING.lock().unwrap_or_else(|e| e.into_inner());
        let mut p = self.proposal_row(id)?;
        let status = p["status"].as_str().unwrap_or_default().to_string();
        if !holding(&status) {
            return Err(state(format!("proposal '{id}' is {status}: only an open one is decided")));
        }
        let actor = disk::actor();
        let merge_as = p["merge_as"].as_str().unwrap_or_default().to_string();
        if !actor.is_empty() {
            if p["proposer"].as_str() == Some(actor.as_str())
                || merge_as == actor
                || p["owner"].as_str() == Some(actor.as_str())
            {
                return Err(refused(format!(
                    "agent {actor} can't decide its own proposal '{id}': a person or its merge policy's reviewer does"
                )));
            }
            let reviewer =
                self.agent(&merge_as).ok().and_then(|a| a.policy).and_then(|n| self.policy(&n).ok()?.reviewer);
            let admin = self.agent(&actor).is_ok_and(|a| a.may(agents::Can::Admin));
            if reviewer.as_deref() != Some(actor.as_str()) && !admin {
                return Err(refused(format!(
                    "agent {actor} isn't the reviewer of {merge_as}'s merge policy, so it can't decide proposal '{id}'"
                )));
            }
        }
        // strict (rules E7 and E8): a person approves it, within its window, while its target is as it was
        let strict = p["strict"].is_object() && approve;
        if strict {
            let channels = texts(&p["strict"]["channels"]).join(", ");
            if !actor.is_empty() {
                return Err(refused(format!(
                    "proposal '{id}' is strict (it queues effects on {channels}): only a person approves it, not agent \
                     {actor}"
                )));
            }
            if disk::now_ms() > p["strict"]["until"].as_u64().unwrap_or(0) {
                let e = state(format!(
                    "proposal '{id}' expired: a strict proposal (effects on {channels}) is approved within its \
                     policy's strict_window of being proposed; propose again"
                ));
                return Err(self.end_strict(p, "expired", "strict_window", e));
            }
        }
        if approve {
            self.still_certified(&p, "approving")?;
        }
        let before = p.clone();
        let person = actor.is_empty();
        let decision = json!({
            "by": if person { "person".to_string() } else { actor },
            "role": if person { role.map(String::from) } else { None },
            "person": person,
            "verdict": if approve { "approved" } else { "rejected" },
            "reason": reason,
            "certificate": p["certificate"],
            "at": disk::now_ms(),
        });
        if let Some(a) = p["approvals"].as_array_mut() {
            a.push(decision);
        } else {
            p["approvals"] = json!([decision]);
        }
        p["status"] = json!(if approve { "approved" } else { "rejected" });
        if !approve {
            // remembered: what was proposed, and why it was turned down
            if let Ok(w) = self.world(p["world_id"].as_str().unwrap_or_default()) {
                let why = match reason.is_empty() {
                    true => format!("proposal {id} rejected"),
                    false => format!("proposal {id} rejected: {reason}"),
                };
                if let Some(t) = self.tombstone_row(&w, "rejected", &[why]) {
                    let _ = self.put_own(vec![t]);
                }
            }
        }
        if strict {
            let expect = p["strict"]["version"].as_u64();
            if let Err(e) = self.put_bound(&mut p, expect) {
                return Err(self.end_strict(before, "superseded", STRICT_TARGET, e));
            }
        } else {
            self.put_proposal(&p)?;
        }
        if !approve {
            self.unfreeze(&p);
        }
        Ok(())
    }

    /// Merges approved proposal `id`: its certificate derived again (40001 if it differs), then the merge made as the
    /// proposing agent, at the version proposed, its merge policy judging it again: the rules its approvals cover are
    /// lifted (a person's: every rule but merge checks, changed reads and required evidence; a reviewer's: those not
    /// `person_only`), for exactly the rows proposed (checked again under the merge's locks). Returns the rows merged.
    pub fn merge_proposal(&self, id: &str) -> Result<usize> {
        let _one = DECIDING.lock().unwrap_or_else(|e| e.into_inner());
        let mut p = self.proposal_row(id)?;
        let status = p["status"].as_str().unwrap_or_default().to_string();
        match status.as_str() {
            "approved" => {}
            "open" => return Err(state(format!("proposal '{id}' isn't approved yet (APPROVE PROPOSAL '{id}')"))),
            s => return Err(state(format!("proposal '{id}' is {s}"))),
        }
        let actor = disk::actor();
        let merge_as = p["merge_as"].as_str().unwrap_or_default().to_string();
        if !actor.is_empty()
            && actor != merge_as
            && p["proposer"].as_str() != Some(actor.as_str())
            && !self.agent(&actor).is_ok_and(|a| a.may(agents::Can::Admin))
        {
            return Err(refused(format!("agent {actor} didn't propose '{id}', so it can't merge it")));
        }
        let world = self.still_certified(&p, "merging")?;
        // strict (rule E7): its target as it was when it was approved; checked again under the merge's locks
        let strict_version = p["strict"]["version"].as_u64().filter(|_| p["strict"].is_object());
        let before = p.clone();
        if let Some(v) = strict_version {
            let target = p["target"].as_str().unwrap_or(MAIN).to_string();
            if self.branch(&target).ok().map(|b| b.version.load(Ordering::Relaxed)) != Some(v) {
                return Err(self.end_strict(before, "superseded", STRICT_TARGET, superseded(id, &target)));
            }
        }
        let cert = p["certificate"].as_str().unwrap_or_default().to_string();
        let covering: Vec<&Json> = p["approvals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["verdict"] == "approved" && a["certificate"].as_str() == Some(cert.as_str()))
            .collect();
        if covering.is_empty() {
            return Err(state(format!("proposal '{id}' has no approval of its certificate")));
        }
        let person = covering.iter().any(|a| a["person"] == true);
        let digest = p["parts"]["diff"].as_str().unwrap_or_default().to_string();
        let o = MergeOptions {
            into: p["into"].as_str().map(String::from),
            expected: p["version"].as_u64(),
            ..Default::default()
        };
        // the proposal as merged: written in the merge's own record when it goes into main (see `merged_row`), else
        // once it went through
        p["status"] = json!("merged");
        p["merged"] = json!({
            "at": disk::now_ms(),
            "by": if actor.is_empty() { "person".to_string() } else { actor },
            "as": merge_as,
            "rows": 0,
        });
        let proposer = p["proposer"].as_str().unwrap_or_default().to_string();
        let row = (format!("{PROPOSALS}/{id}"), p.clone());
        let (n, carried) = {
            let _as = disk::acting_as(&merge_as);
            APPROVED.set(Some(Approved {
                world: world.name.clone(),
                digest,
                person,
                merged: Some(row),
                strict_version,
            }));
            CARRIED.set(false);
            let _carrying = Carrying;
            // an agent's proposal: its rights as they are now, not as they were when it proposed: revoked, disabled,
            // or the world transferred away from it (its lease lost, 55P03), and the merge is refused. Only this
            // proposal's own freeze is lifted (see `frozen`). (A person's proposal is the person's to make: the agent
            // it merges as is whose policy judges it)
            if !merge_as.is_empty() && proposer == merge_as && self.agent(&merge_as).is_ok() {
                let acting = self.as_agent(&merge_as)?;
                match o.into.as_deref() {
                    Some(into) => acting.may_merge_into(&world.name, into)?,
                    None => acting.may_merge(&world.name)?,
                }
            }
            let n = match self.merge_with(&world.name, &o) {
                Err(Error::Constraint { code: "40001", message })
                    if strict_version.is_some() && message.contains(STRICT_TARGET) =>
                {
                    let e = Error::Constraint { code: "40001", message };
                    return Err(self.end_strict(before, "superseded", STRICT_TARGET, e));
                }
                r => r?,
            };
            (n, CARRIED.get())
        };
        if !carried {
            p["merged"]["rows"] = json!(n);
            self.put_proposal(&p)?;
        }
        Ok(n)
    }

    /// A tombstone of world `w` (an agent's: none for others), of `kind` (discarded, refused, rejected): what it
    /// changed (a digest and counts, not the rows), its run, where it sat (parent, when it was made, depth), why.
    pub(crate) fn tombstone_row(&self, w: &World, kind: &str, reasons: &[String]) -> Option<(String, Option<String>)> {
        let owner = w.owner.clone()?;
        if w.name.starts_with("_tx_") || w.meta.get(crate::SIM).is_some() {
            return None;
        }
        let br = self.branch(&w.name).ok()?;
        let diff = self.state_diff(&br.base(), &br.snapshot()).ok()?;
        let (rows, deletes, effects) = agents::counts(&diff);
        let at = disk::now_ms();
        let t = json!({
            "world": w.name,
            "world_id": w.id,
            "owner": owner,
            "kind": kind,
            "run_id": w.meta.get("run_id"),
            "run": w.meta.as_object().and_then(agents::run_of),
            "parent": w.parent,
            "born": w.created,
            "depth": w.depth,
            "digest": diff_digest(&diff),
            "counts": { "rows": rows, "deletes": deletes, "effects": effects },
            "reasons": reasons,
            "at": at,
        });
        let id = format!("{at:013}-{}-{kind}", w.id);
        Some((format!("{TOMBSTONES}/{id}"), Some(t.to_string())))
    }

    /// Writes tombstones (see `tombstone_row`); a failure to is ignored: the world's fate stands either way.
    pub(crate) fn bury(&self, rows: Vec<(String, Option<String>)>) {
        let _ = self.put_own(rows);
    }

    /// Tombstones, oldest first (only `agent`'s, if given), each with its id.
    pub fn tombstones(&self, agent: Option<&str>) -> Result<Vec<Json>> {
        let prefix = format!("{TOMBSTONES}/");
        Ok(self
            .view(MAIN)?
            .prefix(&prefix)?
            .filter_map(|r| {
                let (k, v) = r.ok()?;
                let mut j: Json = serde_json::from_str(&v).ok()?;
                j["id"] = json!(&k[prefix.len()..]);
                Some(j)
            })
            .filter(|j| agent.is_none_or(|a| j["owner"].as_str() == Some(a)))
            .collect())
    }

    /// How a merge came to be decided: proposal `id`, or with `world` the latest proposal of a world by that name (or
    /// id), or the world as it is when none was proposed. Each step as (step, when, by whom, what): the proposal, its
    /// certificate, verdict, evidence, scope, run, policy, approvals, how it ended, and what agents did in the world
    /// (the audit trail, as far back as history is kept: older steps may be missing, which a last row says).
    #[allow(clippy::type_complexity)]
    pub fn provenance(
        &self,
        id: Option<&str>,
        world: Option<&str>,
    ) -> Result<Vec<(String, Option<u64>, String, String)>> {
        let p = match (id, world) {
            (Some(id), _) => Some(self.proposal_row(id)?),
            (None, Some(w)) => {
                let live = self.world(w).ok();
                self.proposals()?.into_iter().rev().find(|p| {
                    p["world"].as_str() == Some(w)
                        || p["world_id"].as_str() == Some(w)
                        || live.as_ref().is_some_and(|l| p["world_id"].as_str() == Some(l.id.as_str()))
                })
            }
            (None, None) => None,
        };
        let live = match &p {
            Some(p) => self.world(p["world_id"].as_str().unwrap_or_default()).ok(),
            None => Some(self.world(world.unwrap_or_default()).map_err(|_| no_proposal(world.unwrap_or_default()))?),
        };
        let mut out: Vec<(String, Option<u64>, String, String)> = vec![];
        let s = |j: &Json| {
            j.as_str().map(String::from).unwrap_or_else(|| if j.is_null() { String::new() } else { j.to_string() })
        };
        let name =
            p.as_ref().map(|p| s(&p["world"])).or_else(|| live.as_ref().map(|w| w.name.clone())).unwrap_or_default();
        if let Some(p) = &p {
            let at = p["created"].as_u64();
            out.push((
                "proposal".into(),
                at,
                s(&p["proposer"]),
                format!(
                    "{} {} ({}) into {} at version {}, to merge as {}",
                    s(&p["id"]),
                    s(&p["world"]),
                    s(&p["world_id"]),
                    s(&p["target"]),
                    p["version"],
                    match s(&p["merge_as"]) {
                        m if m.is_empty() => "a person".into(),
                        m => m,
                    }
                ),
            ));
            out.push(("certificate".into(), at, String::new(), s(&p["certificate"])));
            out.push((
                "verdict".into(),
                at,
                String::new(),
                match texts(&p["reasons"]) {
                    r if r.is_empty() => s(&p["verdict"]),
                    r => format!("{}: {}", s(&p["verdict"]), r.join("; ")),
                },
            ));
            for e in p["parts"]["evidence"].as_array().into_iter().flatten() {
                out.push(evidence_step(e));
            }
            if !p["parts"]["scope"].is_null() {
                out.push(("scope".into(), at, String::new(), p["parts"]["scope"].to_string()));
            }
            if !p["run"].is_null() {
                out.push(("run".into(), at, String::new(), p["run"].to_string()));
            }
            let pol = &p["parts"]["policy"];
            if !pol.is_null() {
                out.push((
                    "policy".into(),
                    at,
                    String::new(),
                    format!("{} (blake3 {})", s(&pol["name"]), s(&pol["hash"])),
                ));
            }
            for a in p["approvals"].as_array().into_iter().flatten() {
                let by = match a["role"].as_str() {
                    Some(r) => format!("{} ({r})", s(&a["by"])),
                    None => s(&a["by"]),
                };
                let reason = s(&a["reason"]);
                out.push((
                    "approval".into(),
                    a["at"].as_u64(),
                    by,
                    format!(
                        "{}{} (certificate {})",
                        s(&a["verdict"]),
                        if reason.is_empty() { String::new() } else { format!(": {reason}") },
                        s(&a["certificate"])
                    ),
                ));
            }
            if let Some(m) = p.get("merged").filter(|m| !m.is_null()) {
                out.push((
                    "merge".into(),
                    m["at"].as_u64(),
                    s(&m["by"]),
                    format!("merged {} rows into {} as {}", m["rows"], s(&p["target"]), s(&m["as"])),
                ));
            }
            if let Some(e) = p.get("ended").filter(|e| !e.is_null()) {
                out.push((s(&p["status"]), e["at"].as_u64(), s(&e["by"]), String::new()));
            }
        } else if let Some(w) = &live {
            out.push((
                "world".into(),
                Some(w.created),
                w.owner.clone().unwrap_or_default(),
                format!("{} ({}), not proposed", w.name, w.id),
            ));
            if let Some(m) = w.meta.as_object() {
                for e in m.get(EVIDENCE).and_then(Json::as_array).into_iter().flatten() {
                    out.push(evidence_step(e));
                }
                if let Some(r) = agents::run_of(m) {
                    out.push(("run".into(), None, String::new(), r.to_string()));
                }
            }
            if let Some(sc) = &w.scope {
                out.push(("scope".into(), None, String::new(), sc.to_string()));
            }
        }
        let mut audit = self.audit(None, usize::MAX)?;
        audit.retain(|a| a.world == name);
        audit.reverse(); // oldest first
        for a in audit {
            let rows = if a.rows > 0 { format!(" ({} rows)", a.rows) } else { String::new() };
            out.push(("audit".into(), Some(a.at), a.agent, format!("{}{rows}", a.action)));
        }
        let since = self.window().ok().flatten().map(|w| w.cut);
        out.push((
            "retention".into(),
            since,
            String::new(),
            "history before the retention window may be missing: steps older than it aren't shown (ALTER SYSTEM SET \
             history_retention keeps more)"
                .into(),
        ));
        Ok(out)
    }
}

fn evidence_step(e: &Json) -> (String, Option<u64>, String, String) {
    let s = |k: &str| e[k].as_str().map(String::from);
    let mut d = s("kind").unwrap_or_default();
    for (k, label) in [("uri", ""), ("sha256", "sha256 "), ("note", "")] {
        if let Some(v) = s(k) {
            d.push_str(&format!(" {label}{v}"));
        }
    }
    ("evidence".into(), e["at"].as_u64(), s("by").unwrap_or_default(), d)
}
