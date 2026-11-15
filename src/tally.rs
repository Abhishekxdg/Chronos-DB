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
//!
//! Kept with them: the rows tainted by untrusted text (see `Taint`), and the sizes of each merge policy's recent
//! merges (see `Core::usual_rows`), for `max_unusual`.

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
    /// rows tainted by untrusted text, per table (see `Taint`)
    taint: HashMap<String, Taint>,
    /// rows each merge policy's last merges into main changed, oldest first (at most `USUAL_KEPT`)
    sizes: HashMap<String, VecDeque<u64>>,
    /// changed since last saved
    dirty: bool,
}

/// Rows of one table an agent's merge wrote into main after its world read text outsiders write (a merge policy's
/// `untrusted_columns`), or other tainted rows: what they hold may be what planted instructions asked for. A world
/// whose policy lists `untrusted_columns` and reads one is judged as if it read untrusted text. A row leaves when it's
/// deleted, or a person's own merge (no agent's, no policy's hold) writes it.
#[derive(Default, Clone)]
pub(crate) struct Taint {
    /// the rows' keys, at most `MOST_TAINTED`
    keys: std::collections::BTreeSet<String>,
    /// more than `MOST_TAINTED` were: any row of the table counts
    all: bool,
}

/// Tainted rows kept one by one per table, past which the whole table counts.
const MOST_TAINTED: usize = 10_000;
/// Merge sizes kept per policy for `max_unusual`.
pub(crate) const USUAL_KEPT: usize = 50;
/// Merges a policy needs before `max_unusual` judges by them.
pub(crate) const USUAL_FEWEST: usize = 10;

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
    let mut taint = HashMap::new();
    let mut sizes = HashMap::new();
    for (k, v) in j.as_object().into_iter().flatten() {
        if let Some(table) = k.strip_prefix(TAINT_KEY) {
            let keys = v["keys"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from));
            taint.insert(table.to_string(), Taint { keys: keys.collect(), all: v["all"].as_bool().unwrap_or(false) });
        } else if let Some(policy) = k.strip_prefix(SIZES_KEY) {
            sizes.insert(policy.to_string(), v.as_array().into_iter().flatten().filter_map(Json::as_u64).collect());
        }
    }
    let by = j
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| !k.starts_with(TAINT_KEY) && !k.starts_with(SIZES_KEY))
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
    let mut t = Tallies { by, taint, sizes, dirty: false };
    t.prune(disk::now_ms());
    t
}

/// Where the file keeps a table's tainted rows, and a policy's merge sizes: words no agent's name or tally key starts
/// with (no agent's name has a space).
const TAINT_KEY: &str = "taint ";
const SIZES_KEY: &str = "sizes ";

/// The tally every agent keeping to merge policy `policy` shares, for its `all_rows_per_hour`, `all_deletes_per_hour`
/// and `all_effects_per_hour`: one count however many agents, runs or tenants, so a fault every one of them repeats
/// stops at the limit. (No agent's name has a space, and `tenant ` keys are another word: it is never theirs.)
pub(crate) fn policy_key(policy: &str) -> String {
    format!("policy {policy}")
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
        let keys: Vec<String> = self
            .tallies
            .lock()
            .unwrap()
            .by
            .keys()
            .filter(|k| !k.starts_with("tenant ") && !k.starts_with("policy "))
            .cloned()
            .collect();
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
        self.hourly_key(&self.tally_of(agent).0)
    }

    /// Rows, deletes and effects the merges counted under tally `key` brought into main in the last hour.
    pub(crate) fn hourly_key(&self, key: &str) -> (u64, u64, u64) {
        let now = disk::now_ms();
        let t = self.tallies.lock().unwrap();
        t.by.get(key)
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

    /// Rows `wrote` (keys of rows a merge into main wrote) are tainted, and rows `cleared` aren't any more (see
    /// `Taint`).
    pub(crate) fn taint_rows(&self, wrote: &[String], cleared: &[String]) {
        if wrote.is_empty() && cleared.is_empty() {
            return;
        }
        let mut t = self.tallies.lock().unwrap();
        for k in cleared {
            let Some((table, _)) = k.split_once('/') else { continue };
            if let Some(taint) = t.taint.get_mut(table)
                && taint.keys.remove(k)
                && taint.keys.is_empty()
                && !taint.all
            {
                t.taint.remove(table);
            }
        }
        for k in wrote {
            let Some((table, _)) = k.split_once('/') else { continue };
            let taint = t.taint.entry(table.to_string()).or_default();
            if taint.all {
                continue;
            }
            taint.keys.insert(k.clone());
            if taint.keys.len() > MOST_TAINTED {
                *taint = Taint { keys: Default::default(), all: true };
            }
        }
        t.dirty = true;
    }

    /// The tainted rows (see `Taint`) a world that read `set` may have read, as reasons show them: its keys, or the
    /// table's name where it read more of it than rows one by one (a scan) or the table's are past counting.
    pub(crate) fn tainted_read(&self, set: &crate::stale::ReadSet) -> Vec<String> {
        let t = self.tallies.lock().unwrap();
        if t.taint.is_empty() {
            return vec![];
        }
        let read = set.by_table();
        let mut out = vec![];
        for (table, taint) in &t.taint {
            // not all it read is known (a restart since the fork): any tainted table counts
            let Some((keys, wider)) = read.get(table).cloned().or_else(|| (!set.complete).then(Default::default))
            else {
                continue;
            };
            if wider || taint.all || !set.complete {
                out.push(format!("rows of {table}"));
                continue;
            }
            out.extend(keys.iter().filter(|k| taint.keys.contains(*k)).map(|k| crate::agents::shown_key(k)));
        }
        out.sort();
        out
    }

    /// How many rows merge policy `policy`'s merges into main usually change: the median of its last `USUAL_KEPT`,
    /// with how many that is; None before `USUAL_FEWEST`. For `max_unusual`.
    pub(crate) fn usual_rows(&self, policy: &str) -> Option<(u64, usize)> {
        let t = self.tallies.lock().unwrap();
        let sizes = t.sizes.get(policy).filter(|s| s.len() >= USUAL_FEWEST)?;
        let mut sorted: Vec<u64> = sizes.iter().copied().collect();
        sorted.sort_unstable();
        Some((sorted[sorted.len() / 2], sorted.len()))
    }

    /// A merge into main under merge policy `policy` changed `rows` rows: kept for `usual_rows`.
    pub(crate) fn note_size(&self, policy: &str, rows: u64) {
        let mut t = self.tallies.lock().unwrap();
        let sizes = t.sizes.entry(policy.to_string()).or_default();
        sizes.push_back(rows);
        while sizes.len() > USUAL_KEPT {
            sizes.pop_front();
        }
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
        let mut j: serde_json::Map<String, Json> =
            t.by.iter()
                .map(|(a, ms)| (a.clone(), Json::Array(ms.iter().map(|m| json!([m.0, m.1, m.2, m.3])).collect())))
                .collect();
        for (table, taint) in &t.taint {
            j.insert(format!("{TAINT_KEY}{table}"), json!({ "keys": taint.keys, "all": taint.all }));
        }
        for (policy, sizes) in &t.sizes {
            j.insert(format!("{SIZES_KEY}{policy}"), json!(sizes));
        }
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
