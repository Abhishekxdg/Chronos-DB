//! COMMENT ON: what people write about their tables and columns, kept as rows of the world
//! (`_sonos_comments/<kind>/<name>`, the text as the value), so a fork, a merge, `DIFF` and `AS OF`
//! carry them as they carry the table. `obj_description`, `col_description` and `pg_description`
//! read them back (see `catalog`), `pg_dump` and psql's `\d+` show them.
//!
//! Keys, with names as the catalog stores them (`app.orders` outside public):
//! `table/<name>`, `view/<name>`, `materialized view/<name>`, `sequence/<name>`, `index/<name>`,
//! `column/<table>/<column>`, `constraint/<table>/<constraint>`, `schema/<name>`, `type/<name>` (pg_description
//! shows an enum type's) and `function/<name(argument types)>`.
//!
//! A comment goes with its object: dropping the table, column or constraint drops it (`after`
//! removes the rows whose object is gone), and renaming a table, a column or an index moves it.

use super::catalog::{Catalog, Obj};
use super::{Alter, Db, Result, Stmt, View, need_table, qualify, schema_part};
use std::collections::HashMap;

pub(super) const COMMENTS: &str = "_sonos_comments";

/// The world row a comment is kept in.
pub(super) fn row_key(key: &str) -> String {
    format!("{COMMENTS}/{key}")
}

/// A comment's text as its row holds it: a JSON string.
fn stored(text: &str) -> String {
    serde_json::Value::String(text.into()).to_string()
}

/// Every comment of the world, by key (see the module docs).
pub(super) fn load(v: &View) -> Result<HashMap<String, String>> {
    let prefix = format!("{COMMENTS}/");
    let mut out = HashMap::new();
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        let text = serde_json::from_str::<String>(&raw).unwrap_or_else(|_| raw.to_string());
        out.insert(k[prefix.len()..].to_string(), text);
    }
    Ok(out)
}

/// Sets, or with None takes away, the comment `key`.
pub(super) fn set(db: &Db, branch: &str, key: &str, text: Option<&str>) -> Result<()> {
    db.batch_with::<(), super::SqlError>(branch, |v| {
        let row = row_key(key);
        let write = match text {
            Some(t) => Some(stored(t)),
            None if v.get(&row)?.is_some() => None,
            None => return Ok((vec![], ())),
        };
        Ok((vec![(row, write)], ()))
    })?;
    Ok(())
}

/// The comments a statement is about to move, as (from, to) keys: worked out before it runs,
/// while the names it changes are still there.
pub(super) fn moves(v: &View, st: &Stmt) -> Result<Vec<(String, String)>> {
    let mut out = vec![];
    match st {
        Stmt::Alter { table, changes, .. } => {
            // (a view isn't a table: renamed through ALTER TABLE, it is under the name given)
            let found = need_table(v, table).ok();
            let from = found.as_ref().map_or_else(|| table.clone(), |t| t.name.clone());
            for change in changes {
                match change {
                    Alter::RenameTable(to) => {
                        let to = qualify(schema_part(&from), to);
                        for kind in ["table", "view", "materialized view", "sequence"] {
                            out.push((format!("{kind}/{from}"), format!("{kind}/{to}")));
                        }
                        for k in load(v)?.into_keys() {
                            for kind in ["column", "constraint"] {
                                if let Some(rest) = k.strip_prefix(&format!("{kind}/{from}/")) {
                                    out.push((k.clone(), format!("{kind}/{to}/{rest}")));
                                }
                            }
                        }
                    }
                    Alter::RenameColumn(a, b) => {
                        out.push((format!("column/{from}/{a}"), format!("column/{from}/{b}")));
                    }
                    Alter::RenameConstraint(a, b) => {
                        out.push((format!("constraint/{from}/{a}"), format!("constraint/{from}/{b}")));
                        // a unique or primary key constraint's index is renamed with it
                        if found.as_ref().is_some_and(|t| {
                            (t.has_key() && t.pkey_name() == *a) || t.uniques.iter().any(|u| u.name == *a)
                        }) {
                            let schema = schema_part(&from);
                            out.push((
                                format!("index/{}", qualify(schema, a)),
                                format!("index/{}", qualify(schema, b)),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        Stmt::RenameView { name, to, kind, .. } => {
            let from = need_table(v, name).map(|t| t.name).unwrap_or_else(|_| name.clone());
            out.push((format!("{kind}/{from}"), format!("{kind}/{}", qualify(schema_part(&from), to))));
        }
        Stmt::RenameIndex { name, to, .. } => {
            let cat = Catalog::of(v.core, &v.branch)?;
            if let Some(Obj::Index(r, x)) = cat.named(name)?.and_then(|o| cat.find(o)) {
                let schema = schema_part(&cat.rels[r].name);
                out.push((format!("index/{}", qualify(schema, &x.name)), format!("index/{}", qualify(schema, to))));
            }
        }
        _ => {}
    }
    Ok(out)
}

/// After a statement that may have dropped or renamed what comments are on: moves the ones it
/// renamed (`moves`, from before) and drops those whose object is gone.
pub(super) fn after(db: &Db, branch: &str, moves: Vec<(String, String)>) -> Result<()> {
    let all = load(&db.core.view_tx(branch)?)?;
    if all.is_empty() {
        return Ok(());
    }
    let mut now = all.clone();
    for (from, to) in moves {
        if let Some(text) = now.remove(&from) {
            now.insert(to, text);
        }
    }
    let cat = Catalog::of(&db.core, branch)?;
    now.retain(|k, _| cat.has_comment_target(k));
    let mut writes: Vec<(String, Option<String>)> = vec![];
    writes.extend(all.keys().filter(|k| !now.contains_key(*k)).map(|k| (row_key(k), None)));
    writes.extend(now.iter().filter(|(k, t)| all.get(*k) != Some(*t)).map(|(k, t)| (row_key(k), Some(stored(t)))));
    if writes.is_empty() {
        return Ok(());
    }
    db.batch_with::<(), super::SqlError>(branch, |_| Ok((writes, ())))?;
    Ok(())
}

/// The statement that puts a comment (key `kind/name`, see the module docs) as it is now: `text`,
/// or none.
pub(super) fn sql(key: &str, text: Option<&str>) -> Option<String> {
    use super::{ident, ident_rel};
    let (kind, rest) = key.split_once('/')?;
    let on = match kind {
        "table" | "view" | "materialized view" | "sequence" | "index" => format!("{kind} {}", ident_rel(rest)),
        "schema" => format!("schema {}", ident(rest)),
        "type" => format!("type {}", ident_rel(rest)),
        "function" => format!("function {rest}"),
        // (`column/<table>/<column>` and `constraint/<table>/<name>`: the table's name ends at
        // the last `/`, the column's or constraint's begins after it)
        "column" => {
            let (table, column) = rest.rsplit_once('/')?;
            format!("column {}.{}", ident_rel(table), ident(column))
        }
        "constraint" => {
            let (table, name) = rest.rsplit_once('/')?;
            format!("constraint {} on {}", ident(name), ident_rel(table))
        }
        _ => return None,
    };
    let text = text.map_or("null".to_string(), |t| format!("'{}'", t.replace('\'', "''")));
    Some(format!("comment on {on} is {text};"))
}
