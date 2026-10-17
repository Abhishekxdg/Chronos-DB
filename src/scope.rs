//! A run's declared scope: what a world was forked to change, said before its first write, and
//! held to at its merge. An agent that may write every table can still overwrite the wrong
//! 5,000 rows; a scope says which ones it meant to touch, so a merge that strays is caught.
//!
//!   CREATE WORLD w WITH (may_change = 'leads.status, lists', tenant = 'org_id = 42',
//!                        intent = 'close stale leads', run = 'planner/3')
//!     ─> `_sonos_scope` in its metadata (only the database sets it; an admin may change it)
//!   merge ─> each row it applies, as JSON ─> outside the tables or columns, or the tenant:
//!              an agent keeping to a merge policy ─> queued for a person, with what strayed
//!              anyone else ─> refused (42501)
//!
//! `may_change` lists whole tables (which may be inserted into, deleted from and altered) and
//! single columns (which may only be updated). `tenant` names a column and a value that every
//! row changed has, before and after, so a table without that column is out of scope. Anything
//! else a merge changes (views, functions, sequences, schemas) is out of any declared scope.
//! `intent` and `run` (a caller's own id for the run, such as a subagent's) are only shown.

use crate::sql::{self, Described};
use crate::{Core, Error, Result, View};
use serde_json::{Map, Value as Json, json};
use std::collections::BTreeMap;

/// Metadata key holding a world's declared scope.
pub(crate) const SCOPE: &str = "_sonos_scope";
/// The metadata keys a scope is declared with.
const KEYS: [&str; 4] = ["may_change", "tenant", "intent", "run"];
/// Rows a reason names before it says how many more.
const SHOWN: usize = 5;

fn bad(message: String) -> Error {
    crate::bad_meta(message)
}

/// Whether `meta` (as given to a fork or ALTER WORLD) declares any part of a scope.
pub(crate) fn declared(meta: &Map<String, Json>) -> bool {
    KEYS.iter().any(|k| meta.contains_key(*k))
}

/// Takes the scope keys out of `meta` (a new world's) and puts the scope they declare, resolved
/// against world `from`'s tables, under `SCOPE`.
pub(crate) fn from_meta(core: &Core, from: &str, meta: &mut Map<String, Json>) -> Result<()> {
    if !declared(meta) {
        return Ok(());
    }
    let given: Map<String, Json> = KEYS.iter().filter_map(|k| Some((k.to_string(), meta.remove(*k)?))).collect();
    let scope = resolve(&core.view(from)?, &Map::new(), given, from)?;
    if !scope.is_null() {
        meta.insert(SCOPE.into(), scope);
    }
    Ok(())
}

/// `old` (a stored scope, or empty) with `patch`'s keys set (null: removed), resolved against
/// view `v` of world `w`. Null when nothing is left declared.
pub(crate) fn resolve(v: &View, old: &Map<String, Json>, patch: Map<String, Json>, w: &str) -> Result<Json> {
    let mut s = old.clone();
    for (k, val) in patch {
        match (k.as_str(), val) {
            (_, Json::Null) => {
                s.remove(&k);
            }
            ("may_change", val) => {
                s.insert(k, Json::Array(may_change(v, &val, w)?));
            }
            ("tenant", val) => {
                s.insert(k, tenant(&val)?);
            }
            ("intent" | "run", Json::String(t)) => {
                s.insert(k, Json::String(t));
            }
            (k, _) => return Err(bad(format!("{k} must be text"))),
        }
    }
    // every table the scope names whole must hold its tenant's column: its rows couldn't be
    // changed otherwise
    if let (Some(t), Some(Json::Array(targets))) = (s.get("tenant"), s.get("may_change")) {
        let col = t["column"].as_str().unwrap_or_default();
        for table in targets.iter().filter_map(|x| x["table"].as_str()) {
            if !sql::table_columns(v, table)?.is_some_and(|cs| cs.iter().any(|c| c == col)) {
                return Err(bad(format!(
                    "tenant {col} = ...: table {table} in may_change has no column {col}, so none of its rows could change"
                )));
            }
        }
    }
    Ok(if s.is_empty() { Json::Null } else { Json::Object(s) })
}

/// `may_change`'s list, as `[{"table": t}, {"table": t, "column": c}, ...]`: a comma-separated
/// text of tables and `table.column`s (names as SQL reads them), or a JSON array of those.
fn may_change(v: &View, val: &Json, w: &str) -> Result<Vec<Json>> {
    let items: Vec<String> = match val {
        Json::String(s) => split(s, ',').ok_or_else(|| bad("may_change: a quote is left open".into()))?,
        Json::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(String::from))
            .collect::<Option<_>>()
            .ok_or_else(|| bad("may_change must be text ('leads, lists.name') or an array of such names".into()))?,
        _ => return Err(bad("may_change must be text ('leads, lists.name') or an array of such names".into())),
    };
    let mut out = vec![];
    for item in items.iter().map(|i| i.trim()).filter(|i| !i.is_empty()) {
        let parts = split(item, '.').ok_or_else(|| bad(format!("may_change: a quote is left open in {item}")))?;
        let parts: Vec<String> = parts.into_iter().map(|p| name(&p)).collect();
        let whole = parts.join(".");
        if sql::table_columns(v, &whole)?.is_some() {
            out.push(json!({ "table": whole }));
            continue;
        }
        let found = match parts.split_last() {
            Some((col, table)) if !table.is_empty() => {
                let table = table.join(".");
                sql::table_columns(v, &table)?.filter(|cs| cs.contains(col)).map(|_| (table, col.clone()))
            }
            _ => None,
        };
        match found {
            Some((table, column)) => out.push(json!({ "table": table, "column": column })),
            None => {
                return Err(Error::Constraint {
                    code: "42P01", // Postgres's undefined_table
                    message: format!("may_change names {item}, which is neither a table nor a table's column in {w}"),
                });
            }
        }
    }
    if out.is_empty() {
        return Err(bad("may_change names nothing: list tables or columns, or leave it out".into()));
    }
    Ok(out)
}

/// A tenant as `{"column": c, "value": v}`: text `column = value` (a number, true or false, or
/// text in single quotes) or a JSON object of one column and its value.
fn tenant(val: &Json) -> Result<Json> {
    let wrong = || bad("tenant must be text like 'org_id = 42' or 'region = ''eu''', or {\"org_id\": 42}".into());
    let (column, value) = match val {
        Json::Object(m) if m.len() == 1 => {
            let (k, v) = m.iter().next().unwrap();
            if !(v.is_string() || v.is_number() || v.is_boolean()) {
                return Err(wrong());
            }
            (k.clone(), v.clone())
        }
        Json::String(s) => {
            let (c, v) = s.split_once('=').ok_or_else(wrong)?;
            let c = split(c.trim(), '.').filter(|p| p.len() == 1).ok_or_else(wrong)?;
            let v = v.trim();
            let value = match v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
                Some(t) if v.len() >= 2 => Json::String(t.replace("''", "'")),
                _ => match v {
                    "true" | "false" => Json::Bool(v == "true"),
                    _ => serde_json::from_str::<serde_json::Number>(v).map(Json::Number).map_err(|_| wrong())?,
                },
            };
            (name(&c[0]), value)
        }
        _ => return Err(wrong()),
    };
    if column.is_empty() {
        return Err(wrong());
    }
    Ok(json!({ "column": column, "value": value }))
}

/// `s` split at each `sep` outside double quotes, quotes kept. None: a quote left open.
pub(crate) fn split(s: &str, sep: char) -> Option<Vec<String>> {
    let (mut out, mut cur, mut quoted) = (vec![], String::new(), false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c == sep && !quoted => out.push(std::mem::take(&mut cur).trim().to_string()),
            c => cur.push(c),
        }
    }
    out.push(cur.trim().to_string());
    (!quoted).then_some(out)
}

/// A name as SQL stores it: quoted keeps its case (`""` a quote in it), unquoted folds to lowercase.
pub(crate) fn name(s: &str) -> String {
    match s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(q) if s.len() >= 2 => q.replace("\"\"", "\""),
        _ => s.to_lowercase(),
    }
}

/// A scope in words, for reasons and people: `may change leads.status, lists; tenant org_id = 42`.
pub fn summary(scope: &Json) -> String {
    let mut parts = vec![];
    if let Some(ts) = scope["may_change"].as_array() {
        let names: Vec<String> = ts
            .iter()
            .map(|t| match t["column"].as_str() {
                Some(c) => format!("{}.{c}", t["table"].as_str().unwrap_or_default()),
                None => t["table"].as_str().unwrap_or_default().to_string(),
            })
            .collect();
        parts.push(format!("may change {}", names.join(", ")));
    }
    if let Some(c) = scope["tenant"]["column"].as_str() {
        parts.push(format!("tenant {c} = {}", lit(&scope["tenant"]["value"])));
    }
    if parts.is_empty() {
        parts.push("no limits on what it changes".into());
    }
    parts.join("; ")
}

/// What `changes` (a merge's rows, as JSON: see `sql::describe`) does outside `scope`, in words
/// (none: all within it).
pub(crate) fn outside(scope: &Json, changes: &[Described]) -> Vec<String> {
    let targets = scope["may_change"].as_array();
    let tenant = scope["tenant"]["column"].as_str().map(|c| (c, &scope["tenant"]["value"]));
    if targets.is_none() && tenant.is_none() {
        return vec![]; // only an intent or a run's id: nothing to hold it to
    }
    let whole = |t: &str| targets.is_none_or(|ts| ts.iter().any(|x| x["table"] == t && x.get("column").is_none()));
    let column =
        |t: &str, c: &str| whole(t) || targets.is_some_and(|ts| ts.iter().any(|x| x["table"] == t && x["column"] == c));
    // what strayed, in words -> the rows' ids (none for a definition)
    let mut strayed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in changes {
        if d.id.is_none() {
            // a table itself, or a view, function, sequence, schema: its definition
            let table = d.key.starts_with("_sonos_tables/");
            if !(table && tenant.is_none() && whole(&d.table)) {
                let (verb, noun) = d.change.split_once(' ').unwrap_or((d.change, ""));
                strayed.entry(format!("{verb}s {noun} {}", d.table)).or_default();
            }
            continue;
        }
        let row = sql::show_key(&d.key).into_owned();
        let mut add = |what: String| strayed.entry(what).or_default().push(row.clone());
        match d.change {
            "update" => {
                let cols: Vec<String> =
                    d.columns.iter().filter(|c| !column(&d.table, c)).map(|c| format!("{}.{c}", d.table)).collect();
                if !cols.is_empty() {
                    add(format!("changes {}", cols.join(", ")));
                }
            }
            "insert" if !whole(&d.table) => add(format!("inserts into {}", d.table)),
            "delete" if !whole(&d.table) => add(format!("deletes from {}", d.table)),
            _ => {}
        }
        if let Some((c, v)) = tenant {
            let of = |row: &Option<Json>| row.as_ref().is_none_or(|r| same(&r[c], v));
            if !of(&d.before) || !of(&d.after) {
                add(format!("changes {} rows outside tenant {c} = {}", d.table, lit(v)));
            }
        }
    }
    strayed
        .into_iter()
        .map(|(what, ids)| match ids.is_empty() {
            true => what,
            false => format!("{what}: {}", shown(&ids)),
        })
        .collect()
}

/// A tenant column's value in a row against the scope's: as JSON, or as exact decimals for a
/// number kept as text (a numeric column's value) or the other way round. Never as floats: two
/// ids past 2^53 would compare equal.
fn same(row: &Json, want: &Json) -> bool {
    row == want
        || match (row, want) {
            (Json::String(s), Json::Number(n)) | (Json::Number(n), Json::String(s)) => {
                decimal(s).is_some_and(|d| decimal(&n.to_string()) == Some(d))
            }
            _ => false,
        }
}

/// A plain decimal (`-012.50`) as one text per value (`-12.5`); None for anything else.
fn decimal(s: &str) -> Option<String> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if (int.is_empty() && frac.is_empty()) || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let int = match int.trim_start_matches('0') {
        "" => "0",
        i => i,
    };
    let frac = frac.trim_end_matches('0');
    let n = if frac.is_empty() { int.to_string() } else { format!("{int}.{frac}") };
    Some(if neg && n != "0" { format!("-{n}") } else { n })
}

/// A tenant's value as SQL writes it: `42`, `'eu'`.
fn lit(v: &Json) -> String {
    match v {
        Json::String(s) => format!("'{}'", s.replace('\'', "''")),
        v => v.to_string(),
    }
}

fn shown(ids: &[String]) -> String {
    let first: Vec<&str> = ids.iter().take(SHOWN).map(String::as_str).collect();
    match ids.len() > SHOWN {
        true => format!("{}, and {} more", first.join(", "), ids.len() - SHOWN),
        false => first.join(", "),
    }
}

/// The error a merge outside its world's declared scope gets (its agent keeps to no merge policy).
pub(crate) fn refused(b: &str, scope: &Json, reasons: &[String]) -> Error {
    Error::Constraint {
        code: "42501",
        message: format!(
            "merging {b} {}, outside the scope it was forked with ({}); nothing was merged: an admin can widen it (ALTER WORLD {b} SET (may_change = ...))",
            reasons.join("; it "),
            summary(scope)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::same;
    use serde_json::json;

    #[test]
    fn tenant_values_compare_exactly() {
        assert!(same(&json!("42"), &json!(42)));
        assert!(same(&json!(12.5), &json!("012.50")));
        assert!(same(&json!("-0"), &json!(0)));
        assert!(!same(&json!(9007199254740992u64), &json!("9007199254740993")), "no floats");
        assert!(!same(&json!("42x"), &json!(42)));
        assert!(!same(&json!("1e2"), &json!(100)), "only plain decimals");
    }
}
