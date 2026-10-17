//! Merge checks: named SQL queries that must find nothing in what a merge would make.
//!
//! A check (`_sonos_checks/<name>` on main) is one SELECT that changes nothing. A merge (any but a
//! transaction's commit) runs the checks whose tables it changes on the world it merges into as
//! the merge would leave it: inside the merge, under its locks, before anything is written, so
//! what's checked is what merges. Rows found refuse the merge, with them as the reason; for an
//! agent keeping to a merge policy they're a broken rule instead, and the merge is queued for a
//! person (see `agents`). No one merges past a check: an exception is made by changing the check,
//! which only an admin may do.
//!
//!   merge ─> rows it applies ─> the result, off to the side ─> each check, read only, as
//!                                                              `world@merge-<n>`, within its timeout
//!                                                   rows found ─> refused (23514), or queued

use crate::{Core, Error, MAIN, Result, State, disk, sql};
use serde_json::{Value as Json, json};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const CHECKS: &str = "_sonos_checks";
/// The rows of a failing check a reason shows.
const SHOWN: usize = 10;
/// How long a check may run unless it says, in ms.
const TIMEOUT: u64 = 1000;

/// A query a merge's result must find nothing in (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct MergeCheck {
    pub name: String,
    /// the SELECT, as written
    pub query: String,
    /// the tables whose changes it runs for (empty: every merge)
    pub tables: Vec<String>,
    /// how long it may run, in ms; a merge it doesn't finish in is refused
    pub timeout: u64,
    pub created: u64,
}

impl MergeCheck {
    fn to_json(&self) -> String {
        json!({ "query": self.query, "tables": self.tables, "timeout": self.timeout, "created": self.created })
            .to_string()
    }

    fn from_json(name: &str, raw: &str) -> Option<MergeCheck> {
        let j: Json = serde_json::from_str(raw).ok()?;
        Some(MergeCheck {
            name: name.into(),
            query: j["query"].as_str()?.into(),
            tables: j["tables"].as_array()?.iter().filter_map(|t| t.as_str().map(String::from)).collect(),
            timeout: j["timeout"].as_u64().unwrap_or(TIMEOUT),
            created: j["created"].as_u64().unwrap_or(0),
        })
    }
}

/// The error a merge that breaks merge checks gets, when its agent keeps to no merge policy.
pub(crate) fn broken(b: &str, reasons: &[String]) -> Error {
    Error::Constraint {
        code: "23514", // Postgres's check_violation
        message: format!("merging {b} {}; nothing was merged", reasons.join("; it ")),
    }
}

fn bad(message: String) -> Error {
    Error::Constraint { code: "22023", message }
}

/// The table a row belongs to, for a check's ON TABLES: its own rows, or its schema. None for the
/// database's other rows.
fn table_of(key: &str) -> Option<&str> {
    match key.split_once('/')? {
        ("_sonos_tables", t) => Some(t),
        (t, _) if crate::reserved_table(t) => None,
        (t, _) => Some(t),
    }
}

/// A check's rows as a reason shows them: `(list = 3, name = 'vip'), ...`.
fn shown(out: &sql::Output) -> String {
    let cols = out.columns.as_deref().unwrap_or_default();
    let rows: Vec<String> = out
        .rows
        .iter()
        .take(SHOWN)
        .map(|r| {
            let vals = r.iter().enumerate().map(|(i, v)| {
                let v = match v {
                    sql::Value::Null => "null".into(),
                    sql::Value::Text(t) => format!("'{t}'"),
                    v => v.to_text(),
                };
                match cols.get(i) {
                    Some((c, _)) => format!("{c} = {v}"),
                    None => v,
                }
            });
            format!("({})", vals.collect::<Vec<_>>().join(", "))
        })
        .collect();
    let more = if out.rows.len() > SHOWN { ", and more" } else { "" };
    format!("{}{more}", rows.join(", "))
}

impl Core {
    /// Creates merge check `name`, or with `replace` puts it in place of the one of that name. The
    /// query must be one SELECT that changes nothing. It's run once on main as it is now, so one
    /// that can't run (a table misspelled) is refused here, not at every merge after.
    pub fn create_check(
        &self,
        name: &str,
        query: &str,
        tables: &[String],
        timeout: Option<u64>,
        replace: bool,
    ) -> Result<MergeCheck> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(bad(format!("'{name}' can't name a merge check: use letters, digits, _ and -")));
        }
        let timeout = timeout.unwrap_or(TIMEOUT);
        if timeout == 0 {
            return Err(bad("a merge check needs a timeout: merges wait for it, holding their locks".into()));
        }
        let c = MergeCheck {
            name: name.into(),
            query: query.trim().into(),
            tables: tables.to_vec(),
            timeout,
            created: disk::now_ms(),
        };
        let key = format!("{CHECKS}/{name}");
        // checked and installed under main's write lock, on main as it is then: a table dropped
        // meanwhile can't leave a check that fails every merge after
        self.batch_with(MAIN, |v| {
            if !replace && v.get(&key)?.is_some() {
                return Err(Error::Exists(format!("merge check {name}")));
            }
            // a scope naming no table would never run: a misspelling mustn't switch the check off
            for t in tables {
                let rows = v.prefix(&format!("{t}/"))?.next().is_some();
                if !rows && v.get(&format!("_sonos_tables/{t}"))?.is_none() {
                    return Err(Error::Constraint {
                        code: "42P01", // Postgres's undefined_table
                        message: format!("merge check {name}: ON TABLES names {t}, which isn't a table on main"),
                    });
                }
            }
            self.run_check(&c, MAIN, &v.s).map_err(|e| Error::Constraint {
                code: e.code,
                message: format!("merge check {name}: {}", e.message),
            })?;
            Ok((vec![(key.clone(), Some(c.to_json()))], ()))
        })?;
        Ok(c)
    }

    /// Removes merge check `name`.
    pub fn drop_check(&self, name: &str) -> Result<()> {
        let key = format!("{CHECKS}/{name}");
        self.batch_with(MAIN, |v| match v.get(&key)? {
            None => Err(Error::NoBranch(format!("merge check {name}"))),
            Some(_) => Ok((vec![(key.clone(), None)], ())),
        })?;
        Ok(())
    }

    pub fn checks(&self) -> Result<Vec<MergeCheck>> {
        let prefix = format!("{CHECKS}/");
        let rows = self.view(MAIN)?.prefix(&prefix)?.collect::<Result<Vec<_>>>()?;
        Ok(rows.into_iter().filter_map(|(k, v)| MergeCheck::from_json(&k[prefix.len()..], &v)).collect())
    }

    /// Check `c`'s rows (SHOWN + 1 at most) in `state`, read as world `world` would be. It runs as
    /// the database's own, not the merging agent: an admin wrote it, and it only reads.
    fn run_check(&self, c: &MergeCheck, world: &str, state: &State) -> sql::Result<sql::Output> {
        static RUNS: AtomicU64 = AtomicU64::new(0);
        let Some(db) = self.db() else {
            return Ok(sql::Output { typmods: vec![], columns: None, rows: vec![], tag: String::new() });
        };
        // a name only this run reads the state by; `@` makes it read only
        let at = format!("{world}@merge-{}", RUNS.fetch_add(1, Ordering::Relaxed));
        self.merging.lock().unwrap().insert(at.clone(), state.clone());
        let out = {
            let _nobody = disk::acting_as("");
            sql::merge_check(&db, &at, &c.query, c.timeout, SHOWN + 1)
        };
        self.merging.lock().unwrap().remove(&at);
        out
    }

    /// The merge checks `state`, world `world` as a merge would leave it, breaks, in words (none:
    /// the merge may go through). `keys`: the rows the merge changes, whose tables say which
    /// checks run. A check that fails to run, or to finish in its timeout, counts as broken.
    pub(crate) fn failed_checks(&self, world: &str, state: &State, keys: &[String]) -> Result<Vec<String>> {
        if self.me.get().is_none() {
            return Ok(vec![]); // replaying the log: the merge was checked when it happened
        }
        let changed: std::collections::HashSet<&str> = keys.iter().filter_map(|k| table_of(k)).collect();
        let mut out = vec![];
        for c in self.checks()? {
            if !c.tables.is_empty() && !c.tables.iter().any(|t| changed.contains(t.as_str())) {
                continue;
            }
            match self.run_check(&c, world, state) {
                Ok(o) if o.rows.is_empty() => {}
                Ok(o) => out.push(format!("breaks merge check {}, which finds {}", c.name, shown(&o))),
                Err(e) if e.code == "57014" => {
                    out.push(format!("breaks merge check {}: it didn't finish within {} ms", c.name, c.timeout))
                }
                Err(e) => out.push(format!("breaks merge check {}: it failed ({})", c.name, e.message)),
            }
        }
        Ok(out)
    }
}
