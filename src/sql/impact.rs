//! What a change affects: for each column a diff changes, what inside the database reads it.
//!
//! A reviewer sees the rows that moved; this says what depends on them. Readers are traced from
//! the world's own definitions:
//!
//!   views, materialized views, merge checks ── their queries, column by column (views inlined)
//!   keys, unique, foreign keys, CHECK ──────── the columns they're over (and tables pointing here)
//!   indexes ────────────────────────────────── their columns, expressions and WHERE
//!   triggers ───────────────────────────────── the events that fire them (and UPDATE OF columns)
//!   functions ──────────────────────────────── the table and column names their text uses
//!   registered readers ─────────────────────── REGISTER READER: exports, sync tools, anything outside SQL
//!   clients ────────────────────────────────── the columns their SQL was seen reading (see `crate::reads`)
//!
//! A row inserted or deleted changes every column of its table. A query's `*`, or a name it
//! doesn't qualify that more than one of its tables has, counts for every column it could mean:
//! for what might break, over is the safe side. What isn't traced is said in `COVERAGE`, always,
//! so "no readers" is never read as "nothing reads this".

use super::functions::{Event, FUNCTIONS};
use super::*;
use std::collections::BTreeSet;

/// Something in the database that reads columns a change changed.
#[derive(Clone, Debug, PartialEq)]
pub struct Reader {
    /// the changed table (as stored: `app.orders`)
    pub table: String,
    /// the changed columns it reads, in the table's order
    pub columns: Vec<String>,
    /// view, materialized view, merge check, trigger, function, primary key, unique, foreign
    /// key, check, index or vector index
    pub kind: &'static str,
    pub name: String,
    /// its definition, or how it reads the columns
    pub detail: String,
    /// marked critical (MARK READER name CRITICAL): a merge policy holds merges changing what it reads
    pub critical: bool,
}

/// Registered readers: rows `_sonos_readers/<name>`, {"tables": {table: [columns, or "*"]}}.
pub(super) const READERS: &str = "_sonos_readers";
/// Readers marked critical, by name (any kind: a client's role, a registered reader, a view):
/// rows `_sonos_critical/<name>`, {"by": who marked it, "at": when}. In the world, like a view,
/// so forks carry them and a merge changing one shows in its diff.
pub(super) const CRITICAL: &str = "_sonos_critical";

/// What `readers` doesn't see: said with every answer (with when reads started being recorded).
pub const COVERAGE: [&str; 2] = [
    "traced inside the database: views, materialized views, merge checks, triggers, functions, keys, foreign keys, CHECK constraints and indexes",
    "functions count where their text names the table and a changed column (or any column, for rows added or deleted); a query EXECUTE builds from a string isn't followed",
];

/// A world's readers of what a change changes, and what wasn't traced.
#[derive(Clone, Debug, PartialEq)]
pub struct Impact {
    pub readers: Vec<Reader>,
    pub coverage: Vec<String>,
}

/// What changed in one table: which kinds of change, and which columns.
#[derive(Default)]
struct Changed {
    insert: bool,
    update: bool,
    delete: bool,
    cols: BTreeSet<String>,
}

/// The readers, in world `world` (`name@when` too), of what `changes` changes.
pub fn impact(db: &Db, world: &str, changes: &[crate::Change]) -> Result<Impact> {
    let v = db.view_tx(world)?;
    let (since, observed) = db.reads();
    let readers = readers(&v, &describe(changes), &db.checks()?, &observed)?;
    let mut coverage: Vec<String> = COVERAGE.iter().map(|c| c.to_string()).collect();
    coverage.insert(
        1,
        format!(
            "clients' reads recorded since {}, for SQL over the Postgres protocol (by role, and application_name) and over HTTP (by agent); clients sharing a role and application_name show as one; the JSON API's get and find, and anything reading an export, show only if registered (REGISTER READER)",
            show_moment(since)
        ),
    );
    Ok(Impact { readers, coverage })
}

/// Registers `name` as reading `cols` of `table` (none: every column) in world `world`, beside
/// the tables it reads already.
pub(super) fn register(db: &Db, world: &str, name: &str, table: &str, cols: &[String]) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return err("22023", format!("'{name}' can't name a reader: use letters, digits, _ and -"));
    }
    db.batch_with(world, |v| {
        let Some(t) = load(v, table)? else {
            return err("42P01", format!("relation \"{}\" does not exist", shown_name(table)));
        };
        if let Some(c) = cols.iter().find(|c| !t.cols.iter().any(|x| x.name == **c)) {
            return err("42703", format!("column \"{c}\" of relation \"{}\" does not exist", shown_name(table)));
        }
        let key = format!("{READERS}/{name}");
        let mut j: Json = v
            .get(&key)?
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_else(|| serde_json::json!({ "tables": {}, "created": crate::disk::now_ms() }));
        let cols = if cols.is_empty() { vec!["*".to_string()] } else { cols.to_vec() };
        j["tables"][table] = serde_json::json!(cols);
        Ok((vec![(key, Some(j.to_string()))], ()))
    })?;
    Ok(())
}

/// Removes registered reader `name`, or only its reading of `table`.
pub(super) fn unregister(db: &Db, world: &str, name: &str, table: Option<&str>) -> Result<()> {
    db.batch_with(world, |v| {
        let key = format!("{READERS}/{name}");
        let missing = || err("42704", format!("reader \"{name}\" isn't registered"));
        let Some(mut j) = v.get(&key)?.and_then(|r| serde_json::from_str::<Json>(&r).ok()) else { return missing() };
        let row = match table {
            None => None,
            Some(t) => {
                let Some(m) = j["tables"].as_object_mut() else { return missing() };
                if m.remove(t).is_none() {
                    return err("42704", format!("reader \"{name}\" isn't registered on {}", shown_name(t)));
                }
                (!m.is_empty()).then(|| j.to_string())
            }
        };
        Ok((vec![(key, row)], ()))
    })?;
    Ok(())
}

/// A registered reader: its name, and each table it reads with its columns ("*": every one).
type Registered = (String, Vec<(String, Vec<String>)>);

/// Registered readers in world `v`.
fn registered(v: &View) -> Result<Vec<Registered>> {
    let mut out = vec![];
    for r in v.prefix(&format!("{READERS}/"))? {
        let (k, raw) = r?;
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        let tables = j["tables"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(t, cols)| {
                let cols = cols.as_array().into_iter().flatten().filter_map(|c| c.as_str().map(String::from));
                (t.clone(), cols.collect())
            })
            .collect();
        out.push((k[READERS.len() + 1..].to_string(), tables));
    }
    Ok(out)
}

/// Readers marked critical in world `v`: name -> (who marked it, when).
fn marks(v: &View) -> Result<HashMap<String, (String, u64)>> {
    let mut out = HashMap::new();
    for r in v.prefix(&format!("{CRITICAL}/"))? {
        let (k, raw) = r?;
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        let by = j["by"].as_str().unwrap_or_default().to_string();
        out.insert(k[CRITICAL.len() + 1..].to_string(), (by, j["at"].as_u64().unwrap_or(0)));
    }
    Ok(out)
}

/// Marks reader `name` critical in world `world` (or with `critical` false takes the mark away),
/// recording who did and when. Any name: a reader may be marked before it's first seen.
pub(super) fn mark(db: &Db, world: &str, name: &str, critical: bool) -> Result<()> {
    if name.is_empty() || name.contains('/') {
        return err("22023", format!("'{name}' can't name a reader"));
    }
    let key = format!("{CRITICAL}/{name}");
    db.batch_with(world, |v| {
        let row = match critical {
            true => Some(serde_json::json!({ "by": crate::disk::actor(), "at": crate::disk::now_ms() }).to_string()),
            false if v.get(&key)?.is_none() => return err("42704", format!("reader \"{name}\" isn't marked critical")),
            false => None,
        };
        Ok((vec![(key.clone(), row)], ()))
    })?;
    Ok(())
}

/// SHOW READERS: registered readers in world `world`, the clients seen reading, and readers
/// marked critical that are neither, as rows (reader, kind, table, columns, last read, critical,
/// marked by, marked at); each table on its own row.
pub(super) fn show(db: &Db, world: &str) -> Result<Vec<Vec<Value>>> {
    let v = db.view_tx(world)?;
    let marks = marks(&v)?;
    // reader, kind, table, columns, last read
    type Row = (String, &'static str, Option<String>, Vec<String>, Option<u64>);
    let mut rows: Vec<Row> = vec![];
    for (name, tables) in registered(&v)? {
        rows.extend(tables.into_iter().map(|(t, cols)| (name.clone(), "registered", Some(t), cols, None)));
    }
    let mut seen: BTreeMap<(String, String), (Vec<String>, u64)> = BTreeMap::new();
    for r in db.reads().1 {
        let e = seen.entry((r.client, r.table)).or_default();
        e.0.push(r.column);
        e.1 = e.1.max(r.last);
    }
    for ((client, table), (mut cols, last)) in seen {
        cols.sort();
        rows.push((client, "client", Some(table), cols, Some(last)));
    }
    let listed: HashSet<String> = rows.iter().map(|r| r.0.clone()).collect();
    let mut unlisted: Vec<&String> = marks.keys().filter(|m| !listed.contains(*m)).collect();
    unlisted.sort();
    rows.extend(unlisted.into_iter().map(|m| (m.clone(), "marked", None, vec![], None)));
    let ms = |ms: u64| Value::TimestampTz(ms as i64 * 1000);
    Ok(rows
        .into_iter()
        .map(|(name, kind, table, cols, last)| {
            let mark = marks.get(&name);
            vec![
                Value::Text(name.clone()),
                Value::Text(kind.into()),
                table.map_or(Value::Null, |t| Value::Text(shown_name(&t))),
                if cols.is_empty() { Value::Null } else { Value::Text(cols.join(", ")) },
                last.map_or(Value::Null, ms),
                Value::Bool(mark.is_some()),
                mark.map_or(Value::Null, |(by, _)| {
                    Value::Text(if by.is_empty() { "a person".into() } else { by.clone() })
                }),
                mark.map_or(Value::Null, |(_, at)| ms(*at)),
            ]
        })
        .collect())
}

/// Why a merge policy holds a merge of `changes` into world `v`: each critical reader of a
/// column it changes (none: no reader is marked critical, or none reads what it changes).
pub(crate) fn critical_reasons(db: &crate::Core, v: &View, changes: &[crate::Change]) -> Result<Vec<String>> {
    if v.prefix(&format!("{CRITICAL}/"))?.next().is_none() {
        return Ok(vec![]); // the usual case: no reader is critical, so nothing to trace
    }
    let found = readers(v, &describe(changes), &db.checks()?, &db.reads().1)?;
    Ok(found
        .into_iter()
        .filter(|r| r.critical)
        .map(|r| {
            let t = shown_name(&r.table);
            let cols: Vec<String> = r.columns.iter().map(|c| format!("{t}.{c}")).collect();
            format!("changes {}, which critical {} {} reads", cols.join(", "), r.kind, r.name)
        })
        .collect())
}

/// The column a schema change's word names (see `schema_changes`): `-age`, `+email text`,
/// `name -> full_name`, `age: not null`; None for the table's keys, indexes, checks, triggers.
fn named_column(word: &str) -> Option<&str> {
    let w = word.strip_prefix(['-', '+']).unwrap_or(word);
    let name = w.split([' ', ':']).next().filter(|n| !n.is_empty())?;
    let table_level = ["primary", "index", "check", "trigger", "unique", "foreign"];
    (!table_level.contains(&name) || w == name).then_some(name)
}

/// Who reads the columns `changes` changes, in view `v` (its tables, views, functions, registered
/// readers), the merge checks `checks` and the clients' reads `observed`.
pub(crate) fn readers(
    v: &View,
    changes: &[Described],
    checks: &[crate::MergeCheck],
    observed: &[crate::Read],
) -> Result<Vec<Reader>> {
    let tables: HashMap<String, Table> = schemas(v)?.into_iter().map(|t| (t.name.clone(), t)).collect();
    let mut changed: BTreeMap<String, Changed> = BTreeMap::new();
    for d in changes {
        let (kind, name) = d.key.split_once('/').unwrap_or((&d.key, ""));
        let (table, whole) = match (kind, d.id.is_some()) {
            (SCHEMAS, false) => (name, d.change != "alter table"),
            (k, true) if !crate::reserved_table(k) => (d.table.as_str(), d.change != "update"),
            _ => continue, // views, functions and the rest: their own definitions, not columns
        };
        let Some(t) = tables.get(table) else { continue }; // dropped: nothing left to read it
        let c = changed.entry(table.to_string()).or_default();
        match d.change {
            "insert" => c.insert = true,
            "delete" => c.delete = true,
            _ => c.update = true,
        }
        if whole {
            c.cols.extend(t.cols.iter().map(|c| c.name.clone()));
        } else {
            c.cols.extend(d.columns.iter().filter_map(|w| named_column(w)).map(String::from));
        }
    }
    if changed.is_empty() {
        return Ok(vec![]);
    }
    let names: HashMap<String, Vec<String>> =
        tables.values().map(|t| (t.name.clone(), t.cols.iter().map(|c| c.name.clone()).collect())).collect();
    // the queries that read tables: views, materialized views and merge checks, traced once
    let mut queries: Vec<(&'static str, String, Reads)> = vec![];
    for r in v.prefix(&format!("{VIEWS}/"))? {
        let (k, raw) = r?;
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        let name = k[VIEWS.len() + 1..].to_string();
        let q = view_query(&name, j["sql"].as_str().unwrap_or(""))?;
        queries.push(("view", name, traced(v, &names, &q)));
    }
    for t in tables.values().filter(|t| t.matview.is_some()) {
        let sql = t.matview.as_ref().and_then(|m| m["sql"].as_str()).unwrap_or("");
        queries.push(("materialized view", t.name.clone(), traced(v, &names, &view_query(&t.name, sql)?)));
    }
    for c in checks {
        if let Some(Statement(Stmt::Select(q), ..)) = parse(&c.query)?.pop() {
            queries.push(("merge check", c.name.clone(), traced(v, &names, &q)));
        }
    }
    let functions: Vec<(String, HashSet<String>)> = v
        .prefix(&format!("{FUNCTIONS}/"))?
        .filter_map(|r| r.ok())
        .filter_map(|(_, raw)| {
            let j: Json = serde_json::from_str(&raw).ok()?;
            let words = lex(j["body"].as_str()?).ok()?.into_iter().filter_map(|t| match t {
                Tok::Word(w) | Tok::Ident(w) => Some(w),
                _ => None,
            });
            Some((j["name"].as_str()?.to_string(), words.collect()))
        })
        .collect();
    let registered = registered(v)?;

    let mut out = vec![];
    for (table, c) in &changed {
        let t = &tables[table];
        let names_of = |ids: &[u32]| -> Vec<String> {
            ids.iter().filter_map(|id| t.cols.iter().find(|c| c.id == *id)).map(|c| c.name.clone()).collect()
        };
        // the changed columns among `cols`, in the table's order
        let hit = |cols: &[String]| -> Vec<String> {
            t.cols.iter().map(|x| &x.name).filter(|n| c.cols.contains(*n) && cols.contains(n)).cloned().collect()
        };
        let mut add = |kind: &'static str, name: &str, columns: Vec<String>, detail: String| {
            if !columns.is_empty() {
                out.push(Reader {
                    table: table.clone(),
                    columns,
                    kind,
                    name: name.to_string(),
                    detail,
                    critical: false,
                });
            }
        };
        let (cons, _) = catalog::describe(v, t);
        for con in &cons {
            let cols: Vec<String> = con
                .cols
                .iter()
                .filter_map(|&n| t.cols.get((n as usize).wrapping_sub(1)))
                .map(|c| c.name.clone())
                .collect();
            let kind = match con.kind {
                'p' => "primary key",
                'u' => "unique",
                'f' => "foreign key",
                _ => "check",
            };
            add(kind, &con.name, hit(&cols), con.def.clone());
        }
        // foreign keys pointing here: a key changed, or a row deleted under them
        let keys: Vec<String> = t.key_cols().iter().map(|&i| t.cols[i].name.clone()).collect();
        for o in tables.values() {
            for f in o.fks.iter().filter(|f| f.table == *table) {
                let cols = if c.delete { keys.clone() } else { hit(&keys) };
                add("foreign key", &f.name, cols, format!("{} references {}", shown_name(&o.name), shown_name(table)));
            }
        }
        for x in &t.indexes {
            let mut cols = names_of(&x.cols);
            for text in x.exprs.iter().chain(&x.filter) {
                if let Ok(e) = check_expr(text) {
                    walk(&e, &mut |e| {
                        if let Expr::Col(_, n) = e {
                            cols.push(n.clone());
                        }
                    });
                }
            }
            let kind = if t.uniques.iter().any(|u| u.name == x.name) { "unique" } else { "index" };
            add(kind, &x.name, hit(&cols), String::new());
        }
        for (name, id, _) in &t.ann {
            add("vector index", name, hit(&names_of(&[*id])), String::new());
        }
        for tr in &t.triggers {
            let fires = tr.events.iter().any(|e| match e {
                Event::Insert => c.insert,
                Event::Delete => c.delete,
                Event::Update => c.update,
            });
            let of = names_of(&tr.cols);
            let cols = if of.is_empty() { c.cols.iter().cloned().collect() } else { of };
            if fires {
                let events: Vec<&str> = tr.events.iter().map(|e| e.name()).collect();
                add("trigger", &tr.name, hit(&cols), format!("{} runs {}()", events.join(" OR "), tr.func));
            }
        }
        for (kind, name, reads) in &queries {
            let star = reads.contains(&(table.clone(), "*".into()));
            let cols: Vec<String> = t
                .cols
                .iter()
                .map(|x| x.name.clone())
                .filter(|n| star || reads.contains(&(table.clone(), n.clone())))
                .collect();
            let detail = if star { format!("reads every column of {}", shown_name(table)) } else { String::new() };
            add(kind, name, hit(&cols), detail);
        }
        let rel = rel_part(table);
        for (name, words) in &functions {
            if !(words.contains(rel) || words.contains(table)) {
                continue;
            }
            let all: Vec<String> = c.cols.iter().cloned().collect();
            let named: Vec<String> = all.iter().filter(|n| words.contains(*n)).cloned().collect();
            if !named.is_empty() {
                add("function", name, hit(&named), format!("names {} and these columns", shown_name(table)));
            } else if c.insert || c.delete {
                // it names no changed column, but its queries of the table meet rows added or gone
                add("function", name, hit(&all), format!("names {}, whose rows changed", shown_name(table)));
            }
        }
        let every: Vec<String> = t.cols.iter().map(|x| x.name.clone()).collect();
        for (name, tables) in &registered {
            if let Some((_, cols)) = tables.iter().find(|(x, _)| x == table) {
                let cols = if cols.iter().any(|c| c == "*") { &every } else { cols };
                add("registered reader", name, hit(cols), "registered with REGISTER READER".into());
            }
        }
        // clients seen reading: their columns, and when they last read one of these
        let mut clients: BTreeMap<&str, (Vec<String>, u64)> = BTreeMap::new();
        for r in observed.iter().filter(|r| r.table == *table && (r.column == "*" || c.cols.contains(&r.column))) {
            let e = clients.entry(r.client.as_str()).or_default();
            match r.column == "*" {
                true => e.0.extend(every.iter().cloned()),
                false => e.0.push(r.column.clone()),
            }
            e.1 = e.1.max(r.last);
        }
        for (client, (cols, last)) in clients {
            add("client", client, hit(&cols), format!("last read {}", show_moment(last)));
        }
    }
    // blast radius: critical readers first, then the tables most read, then the readers of the
    // most changed columns
    let marks = marks(v)?;
    let mut per_table: HashMap<String, usize> = HashMap::new();
    for r in &mut out {
        r.critical = marks.contains_key(&r.name);
        *per_table.entry(r.table.clone()).or_default() += 1;
    }
    out.sort_by(|a, b| {
        (b.critical, per_table[&b.table], b.columns.len()).cmp(&(a.critical, per_table[&a.table], a.columns.len()))
    });
    Ok(out)
}

/// The columns a query reads: (table, column), with (table, "*") for all of a table's, and
/// (table, "") for each table it reads at all (its rows, even with none of their columns).
pub(super) type Reads = BTreeSet<(String, String)>;

/// Each table's columns, to tell which of a query's tables a bare name is from; None when the
/// schemas aren't loaded, and a bare name counts for each table it could be from.
type Names<'a> = Option<&'a HashMap<String, Vec<String>>>;

fn stored(names: Names, t: &str) -> bool {
    names.is_none_or(|m| m.contains_key(t))
}

fn has(names: Names, t: &str, c: &str) -> bool {
    names.is_none_or(|m| m.get(t).is_some_and(|cols| cols.iter().any(|x| x == c)))
}

/// `q`'s reads, with the views it reads inlined, so a view reading a view reaches the tables.
fn traced(v: &View, names: &HashMap<String, Vec<String>>, q: &Select) -> Reads {
    let st = Stmt::Select(Box::new(q.clone()));
    let q = match expand_stmt(v, &st, None, true, true) {
        Ok(Some((Stmt::Select(e), _))) => *e,
        _ => q.clone(),
    };
    let mut out = Reads::new();
    trace(Some(names), &q, &mut vec![], &mut out);
    out.retain(|(_, c)| !c.is_empty()); // readers are of columns, not of a table's rows alone
    out
}

/// The columns query `q` (views already inlined) reads in world `v`, each name found in the tables that have it: a
/// bare name a subquery's own tables don't have is an outer query's (a correlated column), as SQL reads it. With
/// (table, "") for each table it reads at all. For the stale-read check (see `stale`), which mustn't miss a column.
pub(super) fn named_reads(v: &View, q: &Select) -> Reads {
    let tables: BTreeSet<String> =
        plain_reads(&Stmt::Select(Box::new(q.clone()))).into_iter().map(|(t, _)| t).collect();
    let mut names: HashMap<String, Vec<String>> = HashMap::new();
    for t in tables {
        if let Ok(Some(table)) = load(v, &t) {
            names.insert(t, table.cols.iter().filter(|c| !c.hidden).map(|c| c.name.clone()).collect());
        }
    }
    let mut out = Reads::new();
    trace(Some(&names), q, &mut vec![], &mut out);
    out
}

/// The columns statement `st` reads in world `v`: its query, an INSERT's SELECT, an UPDATE's or
/// DELETE's WHERE, SET and FROM. Without the tables' schemas, so it costs no lookups (it runs
/// for clients' statements, see `reads`): a bare name counts for each of its level's tables.
/// Views are inlined when `views` (the database has some).
pub(super) fn statement_reads(v: &View, views: bool, st: &Stmt) -> Reads {
    reads_in(Some(v), views, st)
}

/// `statement_reads` where the database has no views: the statement's own, which needs no world to read (so a
/// statement's can be kept with it).
pub(super) fn plain_reads(st: &Stmt) -> Reads {
    reads_in(None, false, st)
}

fn reads_in(v: Option<&View>, views: bool, st: &Stmt) -> Reads {
    let inlined = match (views, v) {
        (true, Some(v)) => expand_stmt(v, st, None, true, true).ok().flatten().map(|(e, _)| e),
        _ => None,
    };
    let mut out = Reads::new();
    let target = |table: &str, e: &mut dyn FnMut(&mut Vec<Level>, &mut Reads)| {
        let level = Level { tables: vec![(rel_part(table).to_string(), table.to_string())], others: vec![] };
        let mut outer = vec![level];
        let mut out = Reads::new();
        e(&mut outer, &mut out);
        out
    };
    match inlined.as_ref().unwrap_or(st) {
        Stmt::Select(q) | Stmt::Insert { query: Some(q), .. } => trace(None, q, &mut vec![], &mut out),
        Stmt::Update { table, sets, filter, from, .. } => {
            out.extend(target(table, &mut |outer, out| {
                for e in sets.iter().map(|(_, e)| e).chain(filter) {
                    exprs(None, e, outer, out);
                }
            }));
            if let Some(q) = from {
                trace(None, q, &mut vec![], &mut out);
            }
        }
        Stmt::Delete { table, filter, from, .. } => {
            out.extend(target(table, &mut |outer, out| {
                if let Some(e) = filter {
                    exprs(None, e, outer, out);
                }
            }));
            if let Some(q) = from {
                trace(None, q, &mut vec![], &mut out);
            }
        }
        // WITH queries that change rows: what each of its statements reads
        Stmt::With(w) => {
            for st in w.dml.iter().map(|(_, s)| s).chain([&w.main]) {
                out.extend(reads_in(v, views, st));
            }
        }
        _ => {}
    }
    out
}

/// One query level's FROM: its stored tables (alias, table), and the aliases of the rest
/// (subqueries, functions, WITH RECURSIVE's rows so far), whose columns aren't stored ones.
#[derive(Default)]
struct Level {
    tables: Vec<(String, String)>,
    others: Vec<String>,
}

fn trace(names: Names, q: &Select, outer: &mut Vec<Level>, out: &mut Reads) {
    let sources: Vec<&Source> = q.from.iter().chain(q.joins.iter().map(|j| &j.src)).collect();
    let mut level = Level::default();
    for s in &sources {
        match s.sub.is_none() && s.rec.is_none() && s.srf.is_none() && stored(names, &s.table) {
            true => level.tables.push((s.alias.clone(), s.table.clone())),
            false => level.others.push(s.alias.clone()),
        }
    }
    let tables = level.tables.clone();
    out.extend(tables.iter().map(|(_, t)| (t.clone(), String::new())));
    outer.push(level);
    for s in &sources {
        if let Some(sub) = &s.sub {
            trace(names, sub, outer, out);
        }
        if let Some(r) = &s.rec {
            trace(names, &r.base, outer, out);
            trace(names, &r.step, outer, out);
        }
        for e in s.srf.iter().flat_map(|f| &f.1).chain(&s.at) {
            exprs(names, e, outer, out);
        }
    }
    let star = |t: &String| (t.clone(), "*".to_string());
    for i in &q.items {
        match i {
            Item::Star => out.extend(tables.iter().map(|(_, t)| star(t))),
            Item::TableStar(a) => out.extend(tables.iter().filter(|(x, t)| x == a || t == a).map(|(_, t)| star(t))),
            Item::Expr(e, _) => exprs(names, e, outer, out),
        }
    }
    for j in &q.joins {
        for u in &j.using {
            out.extend(tables.iter().filter(|(_, t)| has(names, t, u)).map(|(_, t)| (t.clone(), u.clone())));
        }
    }
    let rest = q.joins.iter().filter_map(|j| j.on.as_ref()).chain(&q.filter).chain(&q.group).chain(&q.having);
    let rest = rest.chain(&q.distinct_on);
    for e in rest.chain(q.order.iter().map(|(e, _)| e)).chain(&q.limit).chain(&q.offset) {
        exprs(names, e, outer, out);
    }
    outer.pop();
    for (_, _, p) in &q.set {
        trace(names, p, outer, out);
    }
}

/// The columns expression `e` reads, and those its subqueries read.
fn exprs(names: Names, e: &Expr, outer: &mut Vec<Level>, out: &mut Reads) {
    walk(e, &mut |x| match x {
        Expr::Col(q, n) => column(names, q.as_deref(), n, outer, out),
        Expr::Sub(sq) | Expr::Exists(sq, _) | Expr::InSub(_, sq, _) => trace(names, &sq.q, outer, out),
        _ => {}
    });
}

/// The source of `level` that the name `q` refers to: the one whose alias is `q`, else one with no alias of its own
/// (its alias is its table's name) whose table is `q`: `FROM orders AS hidden` hides `orders`, which may then name
/// another source (`audit AS orders`).
fn shown<'a>(level: &'a Level, q: &str) -> Option<&'a (String, String)> {
    level
        .tables
        .iter()
        .find(|(a, _)| a == q)
        .or_else(|| level.tables.iter().find(|(a, t)| (a == t || a == rel_part(t)) && (t == q || rel_part(t) == q)))
}

/// Where column `n` (of `q`, if qualified) comes from: the nearest level naming it. Unqualified
/// and in several of that level's tables, it counts for each.
fn column(names: Names, q: Option<&str>, n: &str, outer: &[Level], out: &mut Reads) {
    for level in outer.iter().rev() {
        match q {
            Some(q) => {
                if let Some((_, t)) = shown(level, q) {
                    out.insert((t.clone(), n.to_string()));
                    return;
                }
                if level.others.iter().any(|a| a == q) {
                    return; // a subquery's column: what the subquery reads is traced on its own
                }
            }
            None => {
                let from: Vec<&String> = level.tables.iter().map(|(_, t)| t).filter(|t| has(names, t, n)).collect();
                if !from.is_empty() {
                    out.extend(from.into_iter().map(|t| (t.clone(), n.to_string())));
                    return;
                }
                // no column of that name: a table's name as a value is its whole row (`select o from orders o`)
                if let Some((_, t)) = shown(level, n) {
                    out.insert((t.clone(), "*".to_string()));
                    return;
                }
            }
        }
    }
}
