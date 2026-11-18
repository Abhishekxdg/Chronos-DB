//! Persistent autonomous transactions: a world an agent works in for hours, across restarts and
//! crashes, and comes back to.
//!
//! Durable worlds. A world made `WITH (durable)` (or `ALTER WORLD w SET (durable = true)`) waits
//! for the disk on every write it acknowledges, as main does: its writes skip the batch other
//! worlds' wait in (see `Core::log_write`), and share main's group fsync. So a crash loses none of
//! them, and the world isn't flagged at the next open (every other world open in a crash is: its
//! last writes may be gone). Merges into it, its rebases and its metadata wait for the disk too.
//!
//!   write to a durable world ─> logged now ─> group fsync ─> acknowledged
//!   crash ─> open ─> durable worlds as they were acknowledged, not flagged
//!
//! With `synchronous_commit = off` nothing waits for the disk, main included, and a durable world
//! is as durable as main then: no more.
//!
//! Durable read sets. A durable world that checks its reads (see `stale`) logs its read set
//! (`Op::ReadSet`, whole) at the end of each statement that changed it, before the statement's
//! answer goes out, and again first in each new log segment (a checkpoint drops the old ones), so
//! what the agent was answered is never more than what the log says it read. After a crash the
//! stale-read check runs as usual, rather than holding the merge for reads it can't know.
//!
//!   statement read rows ─> read set changed? ─> Op::ReadSet ─> fsync ─> answer
//!   checkpoint ─> each durable world's read set, first in the new segment
//!
//! Pinned checkpoints. `CHECKPOINT WORLD w AS label PIN '1 day'` folds the world as it is now into
//! pages and keeps the tree's root in its metadata (`PINS`) until the pin runs out: a root of
//! cleanup (see `Core::roots`), so `RESTORE` / `ROLLBACK WORLD w TO CHECKPOINT label` gets back
//! to it after history's retention has dropped that moment.
//!
//! Resume. `SHOW WORLD STATE w` says where a world is: its version, epoch (the lease's, kept by
//! the gateway's TRANSFER as `EPOCH`; 0 without one), whether it is durable or flagged, what it
//! read that changed since, how far its parent moved, and a token `<world id>:<version>:<epoch>`.
//! `RESUME WORLD w TOKEN '...'` gives the same once the token still holds: the same world, no
//! write since (40001; a merge into it that changed it, a transaction's commit included, counts as
//! one: each moves its version on), no other holder since (55P03).

use crate::{Branch, Core, Error, Result, State, disk::Op, pages::Hash};
use serde_json::{Map, Value as Json, json};
use std::sync::atomic::Ordering;

/// Metadata key marking a durable world.
pub(crate) const DURABLE: &str = "_sonos_durable";
/// Metadata key holding a world's pinned checkpoints: label -> {root, at, until}.
pub(crate) const PINS: &str = "_sonos_pins";
/// Metadata key holding a world's lease epoch (set by the gateway's TRANSFER; absent: 0).
pub(crate) const EPOCH: &str = "_sonos_epoch";

/// `durable` in a new world's metadata: true becomes the database's own key; false, or none,
/// leaves nothing.
pub(crate) fn from_meta(meta: &mut Map<String, Json>) -> Result<()> {
    match meta.remove("durable") {
        None | Some(Json::Bool(false)) => Ok(()),
        Some(Json::Bool(true)) => {
            meta.insert(DURABLE.into(), Json::Bool(true));
            Ok(())
        }
        Some(_) => Err(crate::bad_meta("durable must be true or false".into())),
    }
}

/// Whether branch `br` is a durable world.
pub(crate) fn is_durable(br: &Branch) -> bool {
    br.info.read().unwrap().meta.get(DURABLE) == Some(&Json::Bool(true))
}

/// A world's state as `SHOW WORLD STATE` and `RESUME WORLD` give it.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldState {
    pub world: String,
    pub id: String,
    pub version: u64,
    pub epoch: u64,
    pub durable: bool,
    pub flagged: bool,
    pub check_reads: bool,
    /// why its merge would be held for what it read (see `Core::reads_changed`); empty: nothing it read moved
    pub stale_reads: Vec<String>,
    /// rows its parent changed since its fork point
    pub parent_changes: u64,
    /// `<world id>:<version>:<epoch>`, for the next `RESUME`
    pub token: String,
}

/// A 64-character hex root back as a hash.
fn unhex(s: &str) -> Option<Hash> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut h = [0u8; 32];
    for (i, out) in h.iter_mut().enumerate() {
        *out = u8::from_str_radix(std::str::from_utf8(&b[2 * i..2 * i + 2]).ok()?, 16).ok()?;
    }
    Some(h)
}

fn bad(message: String) -> Error {
    Error::Constraint { code: "22023", message }
}

impl Core {
    /// Whether world `b` is durable (see the module docs).
    pub fn durable(&self, b: &str) -> Result<bool> {
        Ok(is_durable(&*self.branch(b)?))
    }

    /// Whether a change to branch `b` waits for the disk: main's, and a durable world's.
    pub(crate) fn syncs(&self, b: &str, br: &Branch) -> bool {
        b == crate::MAIN || is_durable(br)
    }

    /// Waits until everything logged so far is on disk.
    fn sync_all(&self) {
        if let Some(log) = &self.log {
            log.sync(log.appended());
        }
    }

    /// Turns world `b` durable or not (from `set_meta`). Turning it on, its writes waiting in its
    /// batch go to the disk first, and its read set is logged: from here on nothing acknowledged is
    /// only in memory.
    pub(crate) fn set_durable(&self, b: &str, br: &Branch, on: Json) -> Result<()> {
        match on {
            Json::Bool(true) => {
                self.set_reserved(b, DURABLE, Some(Json::Bool(true)))?;
                self.flush_batch(br);
                self.persist_reads(b);
                self.sync_all();
            }
            Json::Bool(false) => {
                self.set_reserved(b, DURABLE, None)?;
                // on disk before a write it no longer waits for: else a crash replays it durable, and unflagged
                self.sync_all();
            }
            _ => return Err(bad("durable must be true or false".into())),
        }
        Ok(())
    }

    /// After world `name` is forked: if durable, it and its (empty) read set are on disk before the fork is answered.
    pub(crate) fn durable_born(&self, name: &str) {
        if self.branch(name).is_ok_and(|br| is_durable(&br)) {
            self.persist_reads(name);
            self.sync_all();
        }
    }

    /// Logs durable world `b`'s read set if it changed, and waits for the disk: at the end of a
    /// statement or API call, before its answer goes out. Any other world: nothing.
    pub fn persist_reads(&self, b: &str) {
        if self.log.is_none() {
            return;
        }
        let Ok(br) = self.branch(b) else { return };
        if !is_durable(&br) {
            return;
        }
        let Some(r) = br.reads.read().unwrap().clone() else { return };
        // `Op::ReadSet` is a record a Chronos from before manifest format 8 can't read: the folder says so first (see
        // `upgrade_log`); until a checkpoint gets that out, the read set stays in memory only, as a world's that isn't
        // durable does
        self.upgrade_log();
        if !self.log.as_ref().is_some_and(|l| l.applied_ok.load(std::sync::atomic::Ordering::Relaxed)) {
            return;
        }
        let pos = {
            // the gate, then the read set, then the log: as merges take them (the gate, then the set to check it);
            // appended under the set's lock, so the log has its snapshots in the order they grew
            let _g = self.gate();
            let mut set = r.lock().unwrap();
            let Some(text) = set.snapshot() else { return };
            self.append(Some(Op::ReadSet(b, text.as_str()).encode()))
        };
        self.after(pos, true);
    }

    /// Logs the read sets of the durable worlds among `worlds` again, whole (their earlier records
    /// may be in segments a checkpoint drops): at open, and first in each new segment (see
    /// `checkpoint_with`). Call holding the log's lock (`g`). Returns the log position to sync to
    /// (0: none).
    pub(crate) fn relog_reads<'a>(
        &self,
        g: &mut crate::disk::Inner,
        worlds: impl Iterator<Item = (&'a String, &'a crate::sync::Arc<Branch>)>,
    ) -> u64 {
        let Some(log) = &self.log else { return 0 };
        let mut pos = 0;
        for (n, br) in worlds {
            if !is_durable(br) {
                continue;
            }
            if let Some(r) = br.reads.read().unwrap().clone() {
                let mut set = r.lock().unwrap();
                set.forget_logged();
                if let Some(text) = set.snapshot() {
                    pos = log.append(g, &Op::ReadSet(n.as_str(), text.as_str()).encode());
                }
            }
        }
        pos
    }

    /// Replays `Op::ReadSet`: world `b`'s read set is the one logged.
    pub(crate) fn replay_reads(&self, b: &str, text: &str) {
        if let Ok(br) = self.branch(b) {
            let meta = br.info.read().unwrap().meta.clone();
            let set = crate::stale::ReadSet::from_snapshot(text, &meta);
            *br.reads.write().unwrap() = Some(crate::sync::Arc::new(crate::sync::Mutex::new(set)));
        }
    }

    /// `CHECKPOINT WORLD w AS label PIN 'for'`: the label, and world `w` as it is now kept for `for_ms`
    /// past history's retention (see the module docs). Returns when the label was taken.
    pub fn pin_checkpoint(&self, w: &str, label: &str, for_ms: u64) -> Result<u64> {
        let (Some(log), Some(pages)) = (&self.log, &self.pages) else {
            return Err(Error::Constraint {
                code: "0A000",
                message: "a pinned checkpoint needs a database on disk; this one is in memory".into(),
            });
        };
        let w = self.world(w)?.name;
        let s = self.branch(&w)?.snapshot(); // its tree is a root of cleanup while `s` holds it
        let at = self.checkpoint_world(&w, label)?;
        let until = crate::disk::now_ms().saturating_add(for_ms);
        let pos = {
            // no checkpoint or cleanup meanwhile: the pages folded here are a root before cleanup next looks
            let _one = log.ckpt.lock().unwrap();
            let root = crate::fold(pages, &s, None)?;
            pages.sync()?; // the pages before the metadata that names them
            self.put_meta_in(&w, true, |meta| {
                let mut pins = meta.get(PINS).and_then(|p| p.as_object().cloned()).unwrap_or_default();
                let now = crate::disk::now_ms();
                pins.retain(|_, p| p["until"].as_u64().is_some_and(|u| u > now)); // run out: let go
                pins.insert(label.into(), json!({ "root": crate::pages::hex(&root), "at": at, "until": until }));
                meta.insert(PINS.into(), Json::Object(pins));
                Ok(())
            })?
        };
        self.after(pos, true);
        Ok(at)
    }

    /// World `w`'s pinned checkpoint `label`, if it is pinned still: its root and when it was taken.
    fn pin_of(&self, w: &str, label: &str) -> Option<(Hash, u64)> {
        let br = self.branch(w).ok()?;
        let info = br.info.read().unwrap();
        let p = info.meta.get(PINS)?.get(label)?;
        if p["until"].as_u64()? <= crate::disk::now_ms() {
            return None;
        }
        let at = p["at"].as_u64()?;
        // the label taken again since, without PIN: that newer moment, not the pinned one
        let label = info.meta.get(crate::agents::CHECKPOINTS).and_then(|c| c.get(label)).and_then(Json::as_u64);
        if label.is_some_and(|t| t != at) {
            return None;
        }
        Some((unhex(p["root"].as_str()?)?, at))
    }

    /// The roots of every pin not run out yet: cleanup keeps their pages (see `roots`).
    pub(crate) fn pinned_roots(&self) -> Vec<Hash> {
        let now = crate::disk::now_ms();
        let m = self.branches.read().unwrap();
        let mut out = vec![];
        for e in m.values() {
            let info = e.br.info.read().unwrap();
            for p in info.meta.get(PINS).and_then(|p| p.as_object()).into_iter().flat_map(|p| p.values()) {
                if p["until"].as_u64().is_some_and(|u| u > now)
                    && let Some(h) = p["root"].as_str().and_then(unhex)
                {
                    out.push(h);
                }
            }
        }
        out
    }

    /// `RESTORE` / `ROLLBACK WORLD w TO CHECKPOINT label`: world `w` back to the label, from its
    /// pinned tree while the pin holds, else from history at the label's time. Returns the rows changed.
    pub fn restore_checkpoint(&self, w: &str, label: &str) -> Result<usize> {
        let w = self.world(w)?.name;
        match self.pin_of(&w, label) {
            Some((root, at)) => {
                self.since_rebase(&w, at)?;
                let past = State { tree: Some(self.track(root)), over: Default::default() };
                self.restore_to(&w, at, &past)
            }
            None => {
                let t = self.checkpoint_time(&w, label)?;
                self.restore(&w, t)
            }
        }
    }

    /// `SHOW WORLD STATE w`, and with `token`, `RESUME WORLD w TOKEN '...'`: where world `b` is now,
    /// once the token (if any) still holds (see the module docs).
    pub fn world_state(&self, b: &str, token: Option<&str>) -> Result<WorldState> {
        let world = self.world(b)?;
        if world.parent.is_none() {
            return Err(Error::IsMain);
        }
        let br = self.branch(&world.name)?;
        let (epoch, check_reads) = {
            let info = br.info.read().unwrap();
            (
                info.meta.get(EPOCH).and_then(|e| e.as_u64()).unwrap_or(0),
                info.meta.contains_key(crate::stale::CHECK_READS),
            )
        };
        let version = br.version.load(Ordering::Relaxed);
        if let Some(t) = token {
            let parts: Vec<&str> = t.rsplitn(3, ':').collect();
            let [ep, ver, id] = parts[..] else {
                return Err(bad(format!("a resume token looks like '<world id>:<version>:<epoch>', not '{t}'")));
            };
            let (Ok(ver), Ok(ep)) = (ver.parse::<u64>(), ep.parse::<u64>()) else {
                return Err(bad(format!("a resume token looks like '<world id>:<version>:<epoch>', not '{t}'")));
            };
            if id != world.id {
                return Err(bad(format!("the resume token is for world {id}, not {} ({})", world.name, world.id)));
            }
            if ep != epoch {
                return Err(Error::Constraint {
                    code: "55P03",
                    message: format!(
                        "world {} was handed on since the token was issued (its epoch is {epoch}, the token's {ep}): this holder's lease is lost",
                        world.name
                    ),
                });
            }
            if ver != version {
                return Err(Error::Constraint {
                    code: "40001",
                    message: format!(
                        "world {} is at version {version}, the token at {ver}: it changed since the token was issued{}; SHOW WORLD STATE {} says where it is",
                        world.name,
                        if br.flagged.load(Ordering::Relaxed) { " (it lost writes in a crash)" } else { "" },
                        world.name
                    ),
                });
            }
        }
        let stale_reads = match check_reads {
            true => self.reads_changed(&world.name)?.into_iter().map(|(_, why)| why).collect(),
            false => vec![],
        };
        let parent = world.parent.clone().unwrap_or_default();
        let (base, now) = (br.base(), self.branch(&parent)?.snapshot());
        let mut parent_changes = 0;
        for c in self.state_diff_iter(&base, &now, None)? {
            c?;
            parent_changes += 1;
        }
        Ok(WorldState {
            token: format!("{}:{version}:{epoch}", world.id),
            world: world.name,
            id: world.id,
            version,
            epoch,
            durable: is_durable(&br),
            flagged: br.flagged.load(Ordering::Relaxed),
            check_reads,
            stale_reads,
            parent_changes,
        })
    }
}
