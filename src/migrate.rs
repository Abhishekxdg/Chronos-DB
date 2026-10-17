//! Versioned migrations: a folder of `.sql` files, applied in name order (`0001_users.sql`,
//! `0002_orders.sql`, ...). Each file runs in its own transaction, so it applies whole or not at
//! all, and the same transaction records it in `_sonos_migrations/<file>` with a checksum:
//! applied files are skipped, and one edited after it was applied is refused. Two runs at once
//! can't both apply a file: the second's commit conflicts on that record and rolls back.

use crate::sql::{Session, SqlError};
use crate::{Db, Val};
use std::path::Path;

const MIGRATIONS: &str = "_sonos_migrations";

/// A migration file, and whether this run applied it (else it already was).
pub struct Migration {
    pub file: String,
    pub applied: bool,
}

fn fail(message: String) -> SqlError {
    SqlError { code: "XX000", message, info: None }
}

impl Db {
    /// Applies `dir`'s pending `.sql` files to `branch`, in name order; stops at the first that
    /// fails (it rolls back; the ones before it stay applied).
    pub fn migrate(&self, branch: &str, dir: impl AsRef<Path>) -> Result<Vec<Migration>, SqlError> {
        let dir = dir.as_ref();
        let mut files: Vec<String> = std::fs::read_dir(dir)
            .map_err(|e| fail(format!("{}: {e}", dir.display())))?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        files.sort();
        let done: std::collections::HashMap<String, Val> = self
            .prefix(branch, &format!("{MIGRATIONS}/"))?
            .into_iter()
            .map(|(k, v)| (k[MIGRATIONS.len() + 1..].to_string(), v))
            .collect();
        let mut todo = vec![];
        for file in files {
            let sql = std::fs::read_to_string(dir.join(&file)).map_err(|e| fail(format!("{file}: {e}")))?;
            let sum = blake3::hash(sql.as_bytes()).to_hex().to_string();
            match done.get(&file) {
                Some(v) if !v.contains(&sum) => {
                    return Err(fail(format!(
                        "{file} changed after it was applied; add a new migration instead of editing an applied one"
                    )));
                }
                Some(_) => todo.push((file, None)),
                None => todo.push((file, Some((sql, sum)))),
            }
        }
        let mut out = vec![];
        for (file, pending) in todo {
            let Some((sql, sum)) = pending else {
                out.push(Migration { file, applied: false });
                continue;
            };
            if crate::sql::parse(&sql)
                .map_err(|e| SqlError { code: e.code, message: format!("{file}: {}", e.message), info: None })?
                .iter()
                .any(|s| s.ends_tx())
            {
                return Err(fail(format!(
                    "{file}: leave out BEGIN/COMMIT/ROLLBACK, each file runs in one transaction"
                )));
            }
            let mut s = Session::new(branch);
            let applied = s.run(self, "BEGIN", &[]).and_then(|_| s.run(self, &sql, &[])).and_then(|_| {
                let record = format!(r#"{{"checksum":"{sum}","applied_at":{}}}"#, crate::now_ms());
                self.write(s.target(), &[(&format!("{MIGRATIONS}/{file}"), Some(&record))])?;
                s.run(self, "COMMIT", &[])
            });
            s.close(self);
            applied.map_err(|e| SqlError { code: e.code, message: format!("{file}: {}", e.message), info: None })?;
            out.push(Migration { file, applied: true });
        }
        Ok(out)
    }
}
