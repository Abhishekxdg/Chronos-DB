//! What each agent's merges brought into main in the last hour: rows changed, rows deleted and
//! effects queued, for merge policies' `rows_per_hour`, `deletes_per_hour` and `effects_per_hour`. `max_rows` and `max_deletes`
//! judge one merge, so a job split across several worlds stays under them; these judge them
//! together.
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
            eprintln!("chronos: could not read {}: {e}; agents' hourly merge counts start over", path.display());
            return Tallies::default();
        }
    };
    let parsed = crate::crypt::open_file(crypt, bytes, &path.display().to_string())
        .and_then(|b| serde_json::from_slice::<Json>(&b).map_err(io::Error::other));
    let j = match parsed {
        Ok(j) => j,
        Err(e) => {
            eprintln!("chronos: {} is damaged ({e}); agents' hourly merge counts start over", path.display());
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
    /// Rows, deletes and effects agent `agent`'s merges brought into main in the last hour.
    pub(crate) fn hourly(&self, agent: &str) -> (u64, u64, u64) {
        let now = disk::now_ms();
        let t = self.tallies.lock().unwrap();
        t.by.get(agent)
            .into_iter()
            .flatten()
            .filter(|m| m.0 + HOUR > now)
            .fold((0, 0, 0), |a, m| (a.0 + m.1, a.1 + m.2, a.2 + m.3))
    }

    /// Counts a merge of agent `agent`'s into main: `rows` changed, `deletes` of them deletes,
    /// `effects` queued.
    pub(crate) fn count_merge(&self, agent: &str, rows: u64, deletes: u64, effects: u64) {
        if rows == 0 && effects == 0 {
            return;
        }
        let now = disk::now_ms();
        let mut t = self.tallies.lock().unwrap();
        t.prune(now);
        t.by.entry(agent.to_string()).or_default().push_back((now, rows, deletes, effects));
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
