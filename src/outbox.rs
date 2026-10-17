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
