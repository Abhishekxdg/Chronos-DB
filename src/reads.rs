//! Who reads what: the tables and columns each client's SQL has read, and when it last did. A
//! change's observed readers come from here (see `sql::impact`): a sync job reading
//! `leads.status` shows beside the views that do.
//!
//!   SQL session with a client (a Postgres role, `role (application_name)`, an agent, http)
//!     ─> each statement's columns, sent at most once a minute per column (see `Session`)
//!     ─> (client, table, column) -> last seen, capped at MOST, saved in the folder's `reads`
//!
//! It's the database's, not a world's: a read of any world counts. It's kept beside the log,
//! not in it, so recording a read writes nothing a world or its history sees.

use crate::{Core, disk};
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::io;
use std::path::Path;

/// Most (client, table, column) entries kept: past it, the longest unseen go first.
const MOST: usize = 100_000;

/// The recorded reads (see the module docs).
pub(crate) struct Log {
    /// since when reads are recorded (ms since 1970): before it, nothing is known
    since: u64,
    seen: HashMap<(String, String, String), u64>,
    /// changed since last saved
    dirty: bool,
}

impl Log {
    pub(crate) fn new() -> Log {
        Log { since: disk::now_ms(), seen: HashMap::new(), dirty: false }
    }
}

/// A client's reads of a table's column: when it last read it (ms since 1970).
#[derive(Clone, Debug, PartialEq)]
pub struct Read {
    pub client: String,
    pub table: String,
    pub column: String,
    pub last: u64,
}

/// The folder's recorded reads; a new log if there are none. A damaged file starts a new one,
/// said on stderr: what it held is lost, not the database.
pub(crate) fn load(dir: &Path, crypt: Option<&crate::crypt::Crypt>) -> Log {
    let path = dir.join("reads");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Log::new(),
        Err(e) => {
            eprintln!("chronos: could not read {}: {e}; recording reads from now", path.display());
            return Log::new();
        }
    };
    let parsed = crate::crypt::open_file(crypt, bytes, &path.display().to_string())
        .and_then(|b| serde_json::from_slice::<Json>(&b).map_err(io::Error::other));
    let j = match parsed {
        Ok(j) => j,
        Err(e) => {
            eprintln!("chronos: {} is damaged ({e}); recording reads from now", path.display());
            return Log::new();
        }
    };
    let seen = j["seen"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let s = |i: usize| r[i].as_str().map(String::from);
            Some(((s(0)?, s(1)?, s(2)?), r[3].as_u64()?))
        })
        .collect();
    Log { since: j["since"].as_u64().unwrap_or_else(disk::now_ms), seen, dirty: false }
}

impl Core {
    /// Records that `client` just read these (table, column) pairs (`*`: every column).
    pub(crate) fn note_reads(&self, client: &str, reads: &[(String, String)]) {
        let now = disk::now_ms();
        let mut l = self.reads.lock().unwrap();
        for (t, c) in reads {
            l.seen.insert((client.to_string(), t.clone(), c.clone()), now);
        }
        if l.seen.len() > MOST {
            // ponytail: sorts all entries past the cap; a heap if a busy server feels it
            let mut by_age: Vec<(u64, (String, String, String))> =
                l.seen.iter().map(|(k, v)| (*v, k.clone())).collect();
            by_age.sort_unstable();
            for (_, k) in by_age.into_iter().take(l.seen.len() - MOST * 9 / 10) {
                l.seen.remove(&k);
            }
        }
        l.dirty = true;
    }

    /// Since when reads are recorded (ms since 1970), and every read recorded.
    pub fn reads(&self) -> (u64, Vec<Read>) {
        let l = self.reads.lock().unwrap();
        let reads = l
            .seen
            .iter()
            .map(|((client, table, column), last)| Read {
                client: client.clone(),
                table: table.clone(),
                column: column.clone(),
                last: *last,
            })
            .collect();
        (l.since, reads)
    }

    /// Writes the reads to the folder if they changed (an in-memory database keeps them only in
    /// memory). Called by the background worker and at close.
    pub(crate) fn save_reads(&self) -> io::Result<()> {
        let Some(log) = &self.log else { return Ok(()) };
        let mut l = self.reads.lock().unwrap(); // held while the file is written: one writer at a time
        if !l.dirty {
            return Ok(());
        }
        let seen: Vec<Json> = l.seen.iter().map(|((c, t, col), last)| json!([c, t, col, last])).collect();
        let bytes = json!({ "since": l.since, "seen": seen }).to_string().into_bytes();
        let bytes = match &log.crypt {
            Some(c) => c.seal_file(&bytes),
            None => bytes,
        };
        disk::write_file(&log.dir.join("reads"), &bytes)?;
        l.dirty = false;
        Ok(())
    }
}
