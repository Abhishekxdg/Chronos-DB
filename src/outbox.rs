//! Effects that happen only when a world's changes reach main: an agent working in a world
//! queues a notification (for a webhook, an email, a payment) that nobody hears while the world
//! is being worked on, that's dropped with it when it's discarded, and that's delivered when a
//! merge brings it into main.
//!
//!   NOTIFY ON MERGE channel, 'payload'  (in a world, or a transaction)
//!     ─> an entry `_sonos_outbox/<id>`: a row of the world, rolled back, discarded and merged
//!        with its other rows (a merge into another world carries it along)
//!   a merge (or a transaction's COMMIT) brings it into main
//!     ─> NOTIFY channel, 'payload' to main's listeners, as it lands
//!     ─> kept in main until a worker that did the work says so
//!
//! The database sends nothing out itself: a worker does the work. Each entry in main is in one
//! of three states, so a worker that dies mid-call never makes an effect vanish or happen twice
//! unseen:
//!
//!   queued ─CLAIM OUTBOX─> claimed (a worker, a lease, attempt n) ─ACK OUTBOX─> gone (done)
//!                          claimed ─RETRY OUTBOX 'id' ATTEMPT n─> queued (the worker knows it wasn't sent)
//!                          claimed, lease run out ─> queued again if the claim said IDEMPOTENT
//!                                                    (the receiver drops a second call);
//!                                                 ─> else ambiguous: it may have been sent.
//!                          ambiguous ─ACK or RETRY─> once someone checked the receiver
//!
//! A lease running out never counts as leave to send again to a receiver that isn't idempotent:
//! an ambiguous entry stays in SHOW OUTBOX, and no CLAIM returns it.

use crate::{Core, Error, MAIN, Result, disk};
use serde_json::{Value as Json, json};

/// Where queued effects are kept: `_sonos_outbox/<id>`.
pub(crate) const OUTBOX: &str = "_sonos_outbox";

/// How long a claim lasts unless CLAIM OUTBOX says: five minutes.
pub const LEASE_MS: u64 = 5 * 60 * 1000;

/// An effect waiting in a world (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct Effect {
    /// its name in ACK OUTBOX: the time it was queued, then a random part. Stable from queueing
    /// to acknowledgement, so a receiver can take it as an idempotency key
    pub id: String,
    pub channel: String,
    pub payload: String,
    /// the world it was queued in
    pub world: String,
    /// when it was queued (ms since 1970)
    pub at: u64,
    /// the agent that queued it (empty: the database's own user)
    pub by: String,
    /// `queued`, `claimed` or `ambiguous` (see the module docs)
    pub state: &'static str,
    /// how many times a worker claimed it
    pub attempts: u64,
    /// who claimed it last (empty: nobody, or the database's own user)
    pub worker: String,
    /// when the last claim's lease runs (or ran) out, ms since 1970; None: never claimed, or retried
    pub lease: Option<u64>,
}

fn bad(message: impl Into<String>) -> Error {
    Error::Constraint { code: "22023", message: message.into() }
}

/// A new effect's id: ordered by time, unique across worlds, so two worlds' entries never
/// conflict when both merge.
fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    format!("{:013}-{:016x}", disk::now_ms(), h.finish())
}

/// Whether `key` is an effect's entry.
pub(crate) fn entry(key: &str) -> bool {
    key.starts_with(OUTBOX) && key.as_bytes().get(OUTBOX.len()) == Some(&b'/')
}

/// An entry's channel (see `entry`), from its stored value.
pub(crate) fn channel_of(raw: &str) -> Option<String> {
    let j: Json = serde_json::from_str(raw).ok()?;
    j["channel"].as_str().map(String::from)
}

/// The notifications the effects among `landed` (rows a merge brings into main) send there.
pub(crate) fn notes(landed: &[crate::Change]) -> Vec<(String, String)> {
    landed
        .iter()
        .filter(|c| entry(&c.key) && c.before.is_none()) // new to main: a claim's update isn't news
        .filter_map(|c| {
            let j: Json = serde_json::from_str(c.after.as_deref()?).ok()?;
            Some((j["channel"].as_str()?.to_string(), j["payload"].as_str().unwrap_or_default().to_string()))
        })
        .collect()
}

/// An entry's state at `now`, from its stored value (see the module docs).
fn state(j: &Json, now: u64) -> &'static str {
    match j["state"].as_str() {
        Some("claimed") if j["until"].as_u64().is_some_and(|u| u > now) => "claimed",
        Some("claimed") if j["idempotent"].as_bool() == Some(true) => "queued",
        Some("claimed") => "ambiguous",
        _ => "queued",
    }
}

fn effect(id: &str, j: &Json, now: u64) -> Option<Effect> {
    Some(Effect {
        id: id.to_string(),
        channel: j["channel"].as_str()?.into(),
        payload: j["payload"].as_str().unwrap_or_default().into(),
        world: j["world"].as_str().unwrap_or_default().into(),
        at: j["at"].as_u64().unwrap_or(0),
        by: j["by"].as_str().unwrap_or_default().into(),
        state: state(j, now),
        attempts: j["attempts"].as_u64().unwrap_or(0),
        worker: j["worker"].as_str().unwrap_or_default().into(),
        lease: j["until"].as_u64(),
    })
}

impl Core {
    /// Queues an effect in world `b` (a transaction's world, when in one), queued from `home`,
    /// the world the session is in. Channel and payload are checked as NOTIFY checks them.
    pub fn queue_effect(&self, b: &str, home: &str, channel: &str, payload: &str) -> Result<Effect> {
        if channel.is_empty() {
            return Err(bad("channel name cannot be empty"));
        }
        if channel.len() >= 64 {
            return Err(bad("channel name too long"));
        }
        if payload.len() >= 8000 {
            return Err(bad("payload string too long"));
        }
        let e = Effect {
            id: new_id(),
            channel: channel.into(),
            payload: payload.into(),
            world: home.into(),
            at: disk::now_ms(),
            by: disk::actor(),
            state: "queued",
            attempts: 0,
            worker: String::new(),
            lease: None,
        };
        let v = json!({ "channel": e.channel, "payload": e.payload, "world": e.world, "at": e.at, "by": e.by });
        // the database's own row: past `batch`, which keeps callers off `_sonos_` keys
        let row = (format!("{OUTBOX}/{}", e.id), Some(v.to_string()));
        self.batch_with(b, |_| Ok::<_, Error>((vec![row], ())))?;
        Ok(e)
    }

    /// The effects waiting in world `b`, oldest first: in main, those that landed and weren't
    /// acknowledged; in a world, those it queued (and main's, as they were when it forked).
    pub fn outbox(&self, b: &str) -> Result<Vec<Effect>> {
        let prefix = format!("{OUTBOX}/");
        let rows = self.view(b)?.prefix(&prefix)?.collect::<Result<Vec<_>>>()?;
        let now = disk::now_ms();
        Ok(rows
            .into_iter()
            .filter_map(|(k, v)| effect(&k[prefix.len()..], &serde_json::from_str(&v).ok()?, now))
            .collect())
    }

    /// Whoever acts may work main's outbox: the database's own users, and agents with 'outbox'
    /// or 'admin' (it changes no data, so a merge policy doesn't stop it; an agent that may merge
    /// may not, else it could take back the effects its merges queued).
    fn may_work_outbox(&self, doing: &str) -> Result<()> {
        let a = disk::actor();
        if a.is_empty() || self.agent(&a).is_ok_and(|x| x.may(crate::agents::Can::Outbox)) {
            return Ok(());
        }
        Err(Error::Constraint {
            code: "42501",
            message: format!("agent {a} may not {doing} effects in main's outbox: that needs 'outbox' or 'admin'"),
        })
    }

    /// Claims up to `limit` queued effects in main, oldest first (only those on `channels`, if
    /// any), for `lease_ms`: each is the claimer's until it acknowledges it, retries it, or the
    /// lease runs out (see the module docs; `idempotent`: the receiver drops a call it already
    /// had, so a lease that runs out may queue it again). Two claims never get the same effect.
    pub fn claim_outbox(
        &self,
        channels: &[String],
        limit: usize,
        lease_ms: u64,
        idempotent: bool,
    ) -> Result<Vec<Effect>> {
        self.may_work_outbox("claim")?;
        if lease_ms == 0 {
            return Err(bad("a claim's lease must be longer than 0"));
        }
        let worker = disk::actor();
        let (_, out) = self.batch_with(MAIN, |v| {
            let prefix = format!("{OUTBOX}/");
            let now = disk::now_ms();
            let (mut rows, mut out) = (vec![], vec![]);
            for r in v.prefix(&prefix)? {
                if out.len() >= limit {
                    break;
                }
                let (k, raw) = r?;
                let Ok(mut j) = serde_json::from_str::<Json>(&raw) else { continue };
                let on = j["channel"].as_str().is_some_and(|c| channels.is_empty() || channels.iter().any(|x| x == c));
                if !on || state(&j, now) != "queued" {
                    continue;
                }
                j["state"] = json!("claimed");
                j["worker"] = json!(worker);
                j["until"] = json!(now.saturating_add(lease_ms));
                j["attempts"] = json!(j["attempts"].as_u64().unwrap_or(0) + 1);
                j["idempotent"] = json!(idempotent);
                out.extend(effect(&k[prefix.len()..], &j, now));
                rows.push((k, Some(j.to_string())));
            }
            Ok::<_, Error>((rows, out))
        })?;
        Ok(out)
    }

    /// Acknowledges effects in main as done, whatever their state: they leave the outbox. Ids not
    /// there (done already, or never) are skipped, so a worker that acknowledges twice isn't
    /// refused. Returns how many left.
    pub fn ack_outbox(&self, ids: &[String]) -> Result<usize> {
        self.may_work_outbox("acknowledge")?;
        let (_, n) = self.batch_with(MAIN, |v| {
            let mut gone = vec![];
            for id in ids {
                let k = format!("{OUTBOX}/{id}");
                if v.get(&k)?.is_some() {
                    gone.push((k, None));
                }
            }
            let n = gone.len();
            Ok::<_, Error>((gone, n))
        })?;
        Ok(n)
    }

    /// Queues effects in main again, each named with the attempt it's about (`attempts` as its
    /// claim returned it): a claimed one whose worker knows it wasn't sent, an ambiguous one someone
    /// checked wasn't. Only that attempt is retried: a retry that comes late, or twice, after the
    /// effect was queued again and claimed anew changes nothing, so a newer claim is never taken
    /// from its worker. Returns how many were queued again (others are skipped).
    pub fn retry_outbox(&self, ids: &[(String, u64)]) -> Result<usize> {
        self.may_work_outbox("retry")?;
        let (_, n) = self.batch_with(MAIN, |v| {
            let mut rows = vec![];
            for (id, attempt) in ids {
                let k = format!("{OUTBOX}/{id}");
                let Some(raw) = v.get(&k)? else { continue };
                let Ok(mut j) = serde_json::from_str::<Json>(&raw) else { continue };
                if j["state"].as_str() != Some("claimed") || j["attempts"].as_u64() != Some(*attempt) {
                    continue; // never claimed, queued again already, or claimed since
                }
                if let Some(m) = j.as_object_mut() {
                    m.remove("until");
                    m.remove("idempotent");
                    m.insert("state".into(), json!("queued"));
                }
                rows.push((k, Some(j.to_string())));
            }
            let n = rows.len();
            Ok::<_, Error>((rows, n))
        })?;
        Ok(n)
    }
}

// ---------------------------------------------------------------- the ledger: where effects came from, and those that never went

/// The file the ledger is kept in, beside the log (as `reads` is): not rows of main, which replaying the log must
/// rebuild exactly, while who merged or dropped a world isn't in what replay applies.
const LEDGER: &str = "outbox.ledger";
/// The file beside it holding the key discarded payloads are hashed under (32 random bytes, made when first needed):
/// it never leaves the folder, through SQL, a dump or the journal, so only who holds the database can tell whether a
/// discarded payload was a given text.
const KEY: &str = "outbox.key";
/// Entries the ledger keeps at most of each kind, past the history window's own limit: the oldest go first.
const LEDGER_MOST: usize = 100_000;
/// The channel a NOTIFY goes out on when an effect in main turns ambiguous (see the module docs).
pub const AMBIGUOUS_CHANNEL: &str = "chronos_outbox_ambiguous";

/// An effect that landed in main, and how (issue #30): kept from the merge on, after it's acknowledged too, so
/// whoever approved a merge can see what it sent and whether it went out (`Core::merge_effects`).
#[derive(Debug, Clone, PartialEq)]
pub struct Landed {
    pub id: String,
    pub channel: String,
    /// the world whose merge brought it into main, by name and id
    pub world: String,
    pub world_id: String,
    /// when it landed (ms since 1970)
    pub at: u64,
    /// who merged it (an agent; empty: the database's own user)
    pub by: String,
    /// who let it through, in words: a merge policy's rules, a person after a review, a person, an agent with none
    pub approved: String,
}

/// An effect discarded with its world, never sent (issue #31): kept so that "the agent went quiet" and "the
/// agent's email was vetoed" can be told apart from outside.
#[derive(Debug, Clone, PartialEq)]
pub struct Discarded {
    pub id: String,
    pub channel: String,
    /// HMAC-SHA256 of the payload under the database's own key (see `KEY`), as hex: whether two were the same, and
    /// whether one was a given text for whoever holds the database, without keeping what they said, and without
    /// letting a reader of this record confirm a short, guessable payload by hashing candidates
    pub payload_hmac: String,
    /// the domains of the email addresses in the payload, sorted, comma-separated (empty: none): where it was going,
    /// kept without the addresses themselves, which stay in the payload
    pub recipient_domains: String,
    /// the payload itself, only while `outbox_keep_payloads` is on (a payload can hold personal data)
    pub payload: Option<String>,
    pub world: String,
    pub world_id: String,
    /// who queued it (an agent; empty: the database's own user), and when
    pub queued_by: String,
    pub queued_at: u64,
    /// the world's owner and its merge policy then (for `SHOW MERGE OUTCOMES`)
    pub owner: String,
    pub policy: String,
    /// when it was discarded, by whom (`system`: the sweeper), and why, in words
    pub at: u64,
    pub by: String,
    pub why: String,
}

/// What the ledger holds (see `Landed`, `Discarded`), and the ambiguous effects already told about.
#[derive(Default)]
pub(crate) struct Ledger {
    landed: Vec<Landed>,
    discarded: Vec<Discarded>,
    /// the key payloads are hashed under (see `KEY`), once loaded or made
    key: Option<[u8; 32]>,
    /// (id, attempt) of effects whose turning ambiguous was announced (in memory: after a restart one may be
    /// announced again, which a listener takes as the same news)
    told: std::collections::HashSet<(String, u64)>,
}

impl Landed {
    fn to_json(&self) -> Json {
        json!({"id": self.id, "channel": self.channel, "world": self.world, "world_id": self.world_id, "at": self.at,
               "by": self.by, "approved": self.approved})
    }
    fn from_json(j: &Json) -> Option<Landed> {
        let s = |k: &str| j[k].as_str().map(String::from);
        Some(Landed {
            id: s("id")?,
            channel: s("channel")?,
            world: s("world").unwrap_or_default(),
            world_id: s("world_id").unwrap_or_default(),
            at: j["at"].as_u64().unwrap_or(0),
            by: s("by").unwrap_or_default(),
            approved: s("approved").unwrap_or_default(),
        })
    }
}

impl Discarded {
    fn to_json(&self) -> Json {
        json!({"id": self.id, "channel": self.channel, "payload_hmac": self.payload_hmac,
               "recipient_domains": self.recipient_domains, "payload": self.payload,
               "world": self.world, "world_id": self.world_id, "queued_by": self.queued_by,
               "queued_at": self.queued_at, "owner": self.owner, "policy": self.policy, "at": self.at, "by": self.by,
               "why": self.why})
    }
    fn from_json(j: &Json) -> Option<Discarded> {
        let s = |k: &str| j[k].as_str().map(String::from);
        Some(Discarded {
            id: s("id")?,
            channel: s("channel")?,
            payload_hmac: s("payload_hmac").unwrap_or_default(), // empty for one recorded before 0.3.2: unkeyed
            recipient_domains: s("recipient_domains").unwrap_or_default(),
            payload: s("payload"),
            world: s("world").unwrap_or_default(),
            world_id: s("world_id").unwrap_or_default(),
            queued_by: s("queued_by").unwrap_or_default(),
            queued_at: j["queued_at"].as_u64().unwrap_or(0),
            owner: s("owner").unwrap_or_default(),
            policy: s("policy").unwrap_or_default(),
            at: j["at"].as_u64().unwrap_or(0),
            by: s("by").unwrap_or_default(),
            why: s("why").unwrap_or_default(),
        })
    }
}

/// The folder's ledger; an empty one if there is none. A damaged file starts a new one, said on stderr: what it held
/// is lost, not the database.
pub(crate) fn load_ledger(dir: &std::path::Path, crypt: Option<&crate::crypt::Crypt>) -> Ledger {
    let path = dir.join(LEDGER);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ledger { key: load_key(dir, crypt), ..Ledger::default() };
        }
        Err(e) => {
            crate::obs::log_line(
                "WARN",
                "ledger_load_failed",
                &[
                    ("path", path.display().to_string()),
                    ("error", format!("{e}")),
                    ("detail", "the outbox ledger starts empty".into()),
                ],
            );
            return Ledger::default();
        }
    };
    let parsed = crate::crypt::open_file(crypt, bytes, &path.display().to_string())
        .and_then(|b| serde_json::from_slice::<Json>(&b).map_err(std::io::Error::other));
    match parsed {
        Ok(j) => Ledger {
            landed: j["landed"].as_array().into_iter().flatten().filter_map(Landed::from_json).collect(),
            discarded: j["discarded"].as_array().into_iter().flatten().filter_map(Discarded::from_json).collect(),
            key: load_key(dir, crypt),
            told: Default::default(),
        },
        Err(e) => {
            crate::obs::log_line(
                "WARN",
                "ledger_damaged",
                &[
                    ("path", path.display().to_string()),
                    ("error", format!("{e}")),
                    ("detail", "the outbox ledger starts empty".into()),
                ],
            );
            Ledger { key: load_key(dir, crypt), ..Ledger::default() }
        }
    }
}

thread_local! {
    /// What the merge just made in `merge_locked` landed in main, for `merge_kept` to keep once it's done (see
    /// `Core::keep_landed`).
    pub(crate) static LANDED: std::cell::RefCell<Vec<Landed>> = const { std::cell::RefCell::new(vec![]) };
}

/// The folder's payload key (see `KEY`), if it has one. One that can't be read is said on stderr, and a new one is
/// made when next needed: records hashed under the old key can't be compared with new ones, and nothing else is lost.
fn load_key(dir: &std::path::Path, crypt: Option<&crate::crypt::Crypt>) -> Option<[u8; 32]> {
    let path = dir.join(KEY);
    let bytes = std::fs::read(&path).ok()?;
    match crate::crypt::open_file(crypt, bytes, &path.display().to_string()) {
        Ok(k) if k.len() == 32 => Some(k.try_into().unwrap()),
        Ok(_) => {
            crate::obs::log_line(
                "WARN",
                "outbox_key_damaged",
                &[
                    ("path", path.display().to_string()),
                    ("detail", "discarded payloads are hashed under a new key".into()),
                ],
            );
            None
        }
        Err(e) => {
            crate::obs::log_line(
                "WARN",
                "outbox_key_load_failed",
                &[
                    ("path", path.display().to_string()),
                    ("error", format!("{e}")),
                    ("detail", "discarded payloads are hashed under a new key".into()),
                ],
            );
            None
        }
    }
}

/// HMAC-SHA256 of `s` under `key`, as hex.
fn hmac_hex(key: &[u8; 32], s: &str) -> String {
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    ring::hmac::sign(&k, s.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The domains of the email addresses in `payload` (`ada@Example.com` gives `example.com`), sorted and without
/// repeats, comma-separated. Found in any text, JSON or not: an address is a run of the characters one may hold, with
/// one `@`, something before it, and a dot in what follows.
pub(crate) fn recipient_domains(payload: &str) -> String {
    let address = |c: char| c.is_ascii_alphanumeric() || "._%+-@".contains(c);
    let mut out = std::collections::BTreeSet::new();
    for word in payload.split(|c: char| !address(c)) {
        let Some((local, domain)) = word.split_once('@') else { continue };
        let domain = domain.trim_matches(|c: char| c == '.' || c == '-').to_ascii_lowercase();
        if !local.trim_matches('.').is_empty() && !domain.contains('@') && domain.contains('.') {
            out.insert(domain);
        }
    }
    out.into_iter().collect::<Vec<_>>().join(",")
}

impl Core {
    /// The effects a world queued itself (or took in from worlds merged into it), oldest first: those its merge
    /// would send, not those it saw in its parent when it forked. What a review lists beside the world's rows.
    pub fn world_effects(&self, b: &str) -> Result<Vec<Effect>> {
        crate::not_tx(b)?;
        self.effects_of(&*self.branch(b)?)
    }

    /// `world_effects` of a branch in hand (`discard_many` holds the map's lock, so can't look one up).
    pub(crate) fn effects_of(&self, br: &crate::Branch) -> Result<Vec<Effect>> {
        let (base, now) = (br.base(), br.snapshot());
        let prefix = format!("{OUTBOX}/");
        let t = disk::now_ms();
        let mut out = vec![];
        for c in self.state_diff_iter(&base, &now, Some(&prefix))? {
            let c = c?;
            if !c.key.starts_with(&prefix) {
                break;
            }
            if let (None, Some(v)) = (&c.before, &c.after)
                && let Ok(j) = serde_json::from_str::<Json>(v)
            {
                out.extend(effect(&c.key[prefix.len()..], &j, t));
            }
        }
        Ok(out)
    }

    /// Whether this process records what happens to effects now: a database in use, not one replaying its log
    /// (whose merges and drops were recorded when they happened).
    fn records_effects(&self) -> bool {
        self.me.get().is_some() && !crate::RECOVERING.get()
    }

    /// The effects a merge of world `b` (forked from `base`, now `ours`) into main lands, as `apply` (the rows it
    /// writes there) has them, with who let them through: kept for after the merge (see `keep_landed`), by
    /// `merge_locked` once it can't fail any more. `policy`: the acting agent's merge policy.
    pub(crate) fn landing(
        &self,
        b: &str,
        world_id: &str,
        review: Option<&str>,
        policy: Option<&str>,
        apply: &[crate::Change],
    ) {
        if !self.records_effects() {
            return;
        }
        let by = disk::actor();
        let approved = match (by.is_empty(), review, policy) {
            (true, Some(p), _) => format!("approved by a person after review (merge policy {p})"),
            (true, None, _) => "merged by a person".to_string(),
            (false, _, Some(p)) => format!("on merge policy {p}'s rules"),
            (false, _, None) => format!("merged by agent {by}, which keeps to no merge policy"),
        };
        let at = disk::now_ms();
        let landed: Vec<Landed> = apply
            .iter()
            .filter(|c| entry(&c.key) && c.before.is_none())
            .filter_map(|c| {
                Some(Landed {
                    id: c.key[OUTBOX.len() + 1..].to_string(),
                    channel: channel_of(c.after.as_deref()?)?,
                    world: b.to_string(),
                    world_id: world_id.to_string(),
                    at,
                    by: by.clone(),
                    approved: approved.clone(),
                })
            })
            .collect();
        LANDED.set(landed);
    }

    /// Keeps what the merge just made landed (see `landing`), now that it's done.
    pub(crate) fn keep_landed(&self) {
        let landed = LANDED.take();
        if landed.is_empty() {
            return;
        }
        let mut l = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        l.landed.extend(landed);
        self.save_ledger(&mut l);
    }

    /// Records the effects world `b` held as it's discarded (see `Discarded`), with why: called by `discard_many`
    /// before the world goes. `meta`: its metadata then.
    pub(crate) fn discarding(
        &self,
        b: &str,
        world_id: &str,
        meta: &serde_json::Map<String, Json>,
        effects: Vec<Effect>,
    ) {
        if effects.is_empty() || !self.records_effects() {
            return;
        }
        let by = disk::actor();
        let owner = meta.get(crate::agents::OWNER).and_then(Json::as_str).unwrap_or_default().to_string();
        let policy = self.agent(&owner).ok().and_then(|a| a.policy).unwrap_or_default();
        let review = meta.get(crate::agents::REVIEW).and_then(|r| r["policy"].as_str());
        let who = if by.is_empty() { "a person".to_string() } else { format!("agent {by}") };
        let why = match review {
            _ if by == crate::agents::SYSTEM => "swept: its time ran out, or it went unused".to_string(),
            Some(p) => format!("rejected: dropped by {who} while its merge waited for review (merge policy {p})"),
            None => format!("dropped by {who}"),
        };
        let keep = self.setting(crate::Setting::OutboxKeepPayloads) != 0;
        let at = disk::now_ms();
        let mut l = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        let key = self.payload_key(&mut l);
        for e in effects {
            l.discarded.push(Discarded {
                payload_hmac: hmac_hex(&key, &e.payload),
                recipient_domains: recipient_domains(&e.payload),
                payload: keep.then_some(e.payload),
                id: e.id,
                channel: e.channel,
                world: b.to_string(),
                world_id: world_id.to_string(),
                queued_by: e.by,
                queued_at: e.at,
                owner: owner.clone(),
                policy: policy.clone(),
                at,
                by: by.clone(),
                why: why.clone(),
            });
        }
        self.save_ledger(&mut l);
    }

    /// The key discarded payloads are hashed under (see `KEY`): the folder's, or a new one, saved there (sealed when
    /// the folder is encrypted). A database without a folder keeps one for as long as it's open.
    fn payload_key(&self, l: &mut Ledger) -> [u8; 32] {
        if let Some(k) = l.key {
            return k;
        }
        use ring::rand::SecureRandom;
        let mut k = [0u8; 32];
        ring::rand::SystemRandom::new().fill(&mut k).expect("the system's random numbers");
        if let Some(log) = &self.log {
            let path = log.dir.join(KEY);
            let bytes = match &log.crypt {
                Some(c) => c.seal_file(&k),
                None => k.to_vec(),
            };
            if let Err(e) = disk::write_file(&path, &bytes) {
                crate::obs::log_line(
                    "WARN",
                    "ledger_save_failed",
                    &[("path", path.display().to_string()), ("error", format!("{e}"))],
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
        l.key = Some(k);
        k
    }

    /// Writes the ledger to the folder, after dropping what's older than the history window (and past
    /// `LEDGER_MOST`): what the merge outcomes read is kept as long. A file that can't be written is said on stderr:
    /// the merge or drop it records has happened, and the next one writes it all again.
    fn save_ledger(&self, l: &mut Ledger) {
        let cut = self.window().ok().flatten().map_or(0, |w| w.cut);
        l.landed.retain(|x| x.at >= cut);
        l.discarded.retain(|x| x.at >= cut);
        if l.landed.len() > LEDGER_MOST {
            let n = l.landed.len() - LEDGER_MOST;
            l.landed.drain(..n);
        }
        if l.discarded.len() > LEDGER_MOST {
            let n = l.discarded.len() - LEDGER_MOST;
            l.discarded.drain(..n);
        }
        let Some(log) = &self.log else { return };
        let j = json!({
            "landed": l.landed.iter().map(Landed::to_json).collect::<Vec<_>>(),
            "discarded": l.discarded.iter().map(Discarded::to_json).collect::<Vec<_>>(),
        });
        let bytes = j.to_string().into_bytes();
        let bytes = match &log.crypt {
            Some(c) => c.seal_file(&bytes),
            None => bytes,
        };
        if let Err(e) = disk::write_file(&log.dir.join(LEDGER), &bytes) {
            crate::obs::log_line("WARN", "ledger_save_failed", &[("error", format!("{e}"))]);
        }
    }

    /// Effects discarded with their worlds since `since` (ms since 1970), oldest first.
    pub fn discarded_effects(&self, since: u64) -> Vec<Discarded> {
        let l = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        l.discarded.iter().filter(|d| d.at >= since).cloned().collect()
    }

    /// How each effect that landed in main came there (see `Landed`), by its id.
    pub fn landed_effects(&self) -> std::collections::HashMap<String, Landed> {
        let l = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        l.landed.iter().map(|x| (x.id.clone(), x.clone())).collect()
    }

    /// What the merges of world `w` (by name or id) brought into main, and where each effect is now: `queued`,
    /// `claimed`, `ambiguous`, or `done` once acknowledged (gone from the outbox). Oldest first. What whoever
    /// approved the merge looks at to see whether what it sent went out.
    pub fn merge_effects(&self, w: &str) -> Result<Vec<(Landed, Option<Effect>)>> {
        let now: std::collections::HashMap<String, Effect> =
            self.outbox(MAIN)?.into_iter().map(|e| (e.id.clone(), e)).collect();
        let l = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        Ok(l.landed
            .iter()
            .filter(|x| x.world == w || x.world_id == w)
            .map(|x| (x.clone(), now.get(&x.id).cloned()))
            .collect())
    }

    /// Announces, on `AMBIGUOUS_CHANNEL`, each effect in main that turned ambiguous since it was last looked at (its
    /// lease ran out on a claim that wasn't IDEMPOTENT): so a worker or an alert can page someone, instead of it
    /// waiting unseen in SHOW OUTBOX. The payload is JSON: its id, channel, attempt, worker, and where it came from.
    /// Run by the background worker, and as SHOW OUTBOX and CLAIM OUTBOX look at the outbox.
    pub fn announce_ambiguous(&self) -> Result<usize> {
        let ambiguous: Vec<Effect> = self.outbox(MAIN)?.into_iter().filter(|e| e.state == "ambiguous").collect();
        if ambiguous.is_empty() {
            return Ok(0);
        }
        let notes: Vec<(String, String)> = {
            let mut l = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
            let from: std::collections::HashMap<&str, &Landed> = l.landed.iter().map(|x| (x.id.as_str(), x)).collect();
            let mut out = vec![];
            for e in &ambiguous {
                let origin = from.get(e.id.as_str());
                out.push((
                    (e.id.clone(), e.attempts),
                    json!({"id": e.id, "channel": e.channel, "attempt": e.attempts, "worker": e.worker,
                           "world": origin.map(|o| o.world.as_str()), "approved": origin.map(|o| o.approved.as_str())})
                    .to_string(),
                ));
            }
            out.retain(|(k, _)| !l.told.contains(k));
            for (k, _) in &out {
                l.told.insert(k.clone());
            }
            out.into_iter().map(|(_, p)| (AMBIGUOUS_CHANNEL.to_string(), p)).collect()
        };
        crate::sql::deliver(self, MAIN, 0, &notes);
        Ok(notes.len())
    }
}
