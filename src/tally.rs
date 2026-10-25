//! What each agent's merges brought into main in the last hour: rows changed, rows deleted and
//! effects queued, for merge policies' `rows_per_hour`, `deletes_per_hour` and `effects_per_hour`. `max_rows` and `max_deletes`
//! judge one merge, so a job split across several worlds stays under them; these judge them
//! together. An agent made `WITH (tenant = ...)` is counted with its tenant: every agent made
//! for that tenant shares one count, so a product that makes an agent per run (as the agent
//! gateway does) can't start the hour again with each run.
//!
//!   an agent's merge into main, judged by its policy ─> its rows and deletes, with the time
//!   next merge ─> the last hour's, plus this one's, against the limits ─> over: a person looks
//!
//! Kept beside the log, not in it (saved by the background worker and at close), as `reads` is:
//! a restart keeps what was saved.

use crate::{Core, disk};
use serde_json::{Value as Json, json};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::Path;

/// The window the limits count over, in ms.
pub(crate) const HOUR: u64 = 60 * 60 * 1000;

/// Each agent's merges into main in the window: (when, rows, deletes, effects), oldest first.
#[derive(Default)]
pub(crate) struct Tallies {
    by: HashMap<String, VecDeque<(u64, u64, u64, u64)>>,
    /// changed since last saved
    dirty: bool,
}

impl Tallies {
    fn prune(&mut self, now: u64) {
        for merges in self.by.values_mut() {
            while merges.front().is_some_and(|m| m.0 + HOUR <= now) {
                merges.pop_front();
            }
        }
        self.by.retain(|_, m| !m.is_empty());
    }
}

/// The folder's saved tallies; none if there are none. A damaged file starts over, said on
/// stderr.
pub(crate) fn load(dir: &Path, crypt: Option<&crate::crypt::Crypt>) -> Tallies {
    let path = dir.join("tallies");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Tallies::default(),
        Err(e) => {
            crate::obs::log_line(
                "WARN",
                "tally_load_failed",
                &[
                    ("path", path.display().to_string()),
                    ("error", format!("{e}")),
                    ("detail", "hourly merge counts start over".into()),
                ],
            );
            return Tallies::default();
        }
    };
    let parsed = crate::crypt::open_file(crypt, bytes, &path.display().to_string())
        .and_then(|b| serde_json::from_slice::<Json>(&b).map_err(io::Error::other));
    let j = match parsed {
        Ok(j) => j,
        Err(e) => {
            crate::obs::log_line(
                "WARN",
                "tally_damaged",
                &[
                    ("path", path.display().to_string()),
                    ("error", format!("{e}")),
                    ("detail", "hourly merge counts start over".into()),
                ],
            );
            return Tallies::default();
        }
    };
    let by = j
        .as_object()
        .into_iter()
        .flatten()
        .map(|(agent, merges)| {
            let merges = merges
                .as_array()
                .into_iter()
                .flatten()
                // (effects came later: a file from before has none)
                .filter_map(|m| Some((m[0].as_u64()?, m[1].as_u64()?, m[2].as_u64()?, m[3].as_u64().unwrap_or(0))))
                .collect();
            (agent.clone(), merges)
        })
        .collect();
    let mut t = Tallies { by, dirty: false };
    t.prune(disk::now_ms());
    t
}

impl Core {
    /// Whose count agent `agent`'s merges go into: its tenant's, for an agent made `WITH (tenant
    /// = ...)` (see the module docs), else its own. The tally's key, and the tenant in words.
    pub(crate) fn tally_of(&self, agent: &str) -> (String, Option<String>) {
        let a = self.agent(agent).ok();
        let tenant =
            a.as_ref().and_then(|a| Some((a.tenant.as_ref().and_then(crate::scope::tenant_key)?, a.tenant_sql()?)));
        match tenant {
            // the key as the tenant's scope compares it (42 and '42' are one tenant); no agent's name has a space,
            // so it is never an agent's
            Some((key, words)) => (format!("tenant {key}"), Some(words)),
            None => (agent.to_string(), None),
        }
    }

    /// Counts kept under a tenant agent's own name (saved before agents shared their tenant's count, see the module
    /// docs) go into its tenant's, once, as the database opens: an agent that used its hour just before an upgrade
    /// keeps it used. The agents are read from main, so this runs after the log is replayed.
    pub(crate) fn migrate_tallies(&self) {
        let keys: Vec<String> =
            self.tallies.lock().unwrap().by.keys().filter(|k| !k.starts_with("tenant ")).cloned().collect();
        let moves: Vec<(String, String)> = keys
            .into_iter()
            .filter_map(|k| {
                let (to, tenant) = self.tally_of(&k);
                tenant.map(|_| (k, to))
            })
            .collect();
        if moves.is_empty() {
            return;
        }
        let mut t = self.tallies.lock().unwrap();
        for (from, to) in moves {
            let Some(merges) = t.by.remove(&from) else { continue };
            let bucket = t.by.entry(to).or_default();
            let mut all: Vec<_> = bucket.drain(..).chain(merges).collect();
            all.sort_by_key(|m| m.0);
            bucket.extend(all);
        }
        t.dirty = true;
    }

    /// Rows, deletes and effects agent `agent`'s merges (or its tenant's: see `tally_of`) brought
    /// into main in the last hour.
    pub(crate) fn hourly(&self, agent: &str) -> (u64, u64, u64) {
        let now = disk::now_ms();
        let key = self.tally_of(agent).0;
        let t = self.tallies.lock().unwrap();
        t.by.get(&key)
            .into_iter()
            .flatten()
            .filter(|m| m.0 + HOUR > now)
            .fold((0, 0, 0), |a, m| (a.0 + m.1, a.1 + m.2, a.2 + m.3))
    }

    /// Counts a merge into main under tally `key` (an agent's, or its tenant's: see `tally_of`,
    /// which the merge reads before taking main's locks, as this runs under them): `rows`
    /// changed, `deletes` of them deletes, `effects` queued.
    pub(crate) fn count_merge(&self, key: &str, rows: u64, deletes: u64, effects: u64) {
        if rows == 0 && effects == 0 {
            return;
        }
        let now = disk::now_ms();
        let mut t = self.tallies.lock().unwrap();
        t.prune(now);
        t.by.entry(key.to_string()).or_default().push_back((now, rows, deletes, effects));
        t.dirty = true;
    }

    /// Writes the tallies to the folder if they changed (an in-memory database keeps them only
    /// in memory). Called by the background worker and at close.
    pub(crate) fn save_tallies(&self) -> io::Result<()> {
        let Some(log) = &self.log else { return Ok(()) };
        let mut t = self.tallies.lock().unwrap(); // held while the file is written: one writer at a time
        if !t.dirty {
            return Ok(());
        }
        t.prune(disk::now_ms());
        let j: serde_json::Map<String, Json> =
            t.by.iter()
                .map(|(a, ms)| (a.clone(), Json::Array(ms.iter().map(|m| json!([m.0, m.1, m.2, m.3])).collect())))
                .collect();
        let bytes = Json::Object(j).to_string().into_bytes();
        let bytes = match &log.crypt {
            Some(c) => c.seal_file(&bytes),
            None => bytes,
        };
        disk::write_file(&log.dir.join("tallies"), &bytes)?;
        t.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_leave_the_count_after_an_hour() {
        let mut t = Tallies::default();
        t.by.insert("bot".into(), VecDeque::from([(1_000, 5, 1, 0), (1_000 + HOUR - 1, 7, 2, 3)]));
        t.prune(1_000 + HOUR); // the first is an hour old: out
        assert_eq!(t.by["bot"], VecDeque::from([(1_000 + HOUR - 1, 7, 2, 3)]));
        t.prune(1_000 + 2 * HOUR);
        assert!(t.by.is_empty(), "an agent with no merges in the window isn't kept");
    }
}
