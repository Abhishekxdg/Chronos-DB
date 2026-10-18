//! Masked columns: ALTER TABLE t ALTER COLUMN c SET MASKED [WITH HASH | NULL | PARTIAL] and DROP MASKED.
//!
//! A masked column's values are never shown to an agent without the `unmask` capability (see `on`): it reads
//! a keyed hash of each value (`HASH`, the default: equal values hash the same, so joins, GROUP BY and
//! DISTINCT still work, across tables too), NULL, the value's last four characters (`PARTIAL`), or a keyed
//! stand-in of the same shape (`FORMAT`: each letter a letter, each digit a digit, the rest kept, so an
//! email stays email-shaped and the same length, for agents that validate formats). People, and agents with
//! `unmask`, read the values as they are.
//!
//! The mask is the column's own (pg_attribute's options, see `ColOpts`), so it forks, merges, renames and drops
//! with the column, and a table that inherits from or is a partition of a masked one takes its masks too.
//!
//! How, fail-closed. A read of a masked table in an agent's statement becomes a subquery of it whose masked
//! columns are already masked (`wrap`, from `expand_select`): every WHERE, join, ORDER BY and function above it
//! sees only the hash, so nothing can be learned by filtering on the value. Row security's policies run
//! inside the subquery, on the values as they are. What can't be rewritten that way is refused (42501): an
//! UPDATE, DELETE, ON CONFLICT DO UPDATE or MERGE that reads a masked column of its own target (`target`),
//! changing a masked table's definition (`ddl`), `search()` over one, DIFF AS SQL, and the HTTP and MCP row
//! API (see `agent_refused`). COPY of a table is a SELECT of it, so it reads the table masked. DIFF and
//! MERGE ... DRY RUN show masked values masked (`output`), and a constraint's error doesn't show a masked
//! table's key (see `rls::may_show_key`).
//!
//! The hash is keyed by a secret of the database's (`Core::mask_key`), kept on main where no SQL reaches it,
//! so a hash can't be looked up in a table of hashes of likely values; and the function that makes it has no
//! name SQL can call, so an agent can't hash a guess to compare.

use super::*;
use std::collections::BTreeSet;
use std::sync::Mutex;

/// The alias of the table read inside a masked subquery (see `wrap`): a name SQL can't write, so the
/// subquery's own read of the table isn't wrapped again.
pub(super) const INNER: &str = "\u{1}masked";

/// HASH, NULL and PARTIAL, as `ColOpts::mask` keeps them.
pub(super) const HASH: char = 'h';
pub(super) const NULL: char = 'n';
pub(super) const PARTIAL: char = 'p';
pub(super) const FORMAT: char = 'f';

/// The mode a word of SET MASKED WITH names.
pub(super) fn mode(word: &str) -> Option<char> {
    match word {
        "hash" => Some(HASH),
        "null" => Some(NULL),
        "partial" => Some(PARTIAL),
        "format" => Some(FORMAT),
        _ => None,
    }
}

pub(super) fn mode_name(m: char) -> &'static str {
    match m {
        NULL => "null",
        PARTIAL => "partial",
        FORMAT => "format",
        _ => "hash",
    }
}

/// Whether the running statement is one masks apply to: an agent's (the guest's too) without `unmask`.
pub(crate) fn on() -> bool {
    STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref()).is_some_and(|c| c.masked))
}

/// The masked columns of table `name` in world `v`, with their modes: its own and those of the tables it
/// inherits from or is a partition of (by column name), and the generated columns made from any of them. Empty
/// for anything else.
pub(crate) fn of(v: &View, name: &str) -> Result<Vec<(String, char)>> {
    let mut out: Vec<(String, char)> = vec![];
    let mut tables = vec![];
    let mut todo = vec![(name.to_string(), 0usize)];
    while let Some((n, depth)) = todo.pop() {
        if depth > 64 {
            break; // (a loop of parents: Postgres refuses to make one)
        }
        let Some(t) = load(v, &n)? else { continue };
        for c in t.cols.iter().filter(|c| !c.hidden) {
            if let Some(m) = c.opts.mask
                && !out.iter().any(|(o, _)| *o == c.name)
            {
                out.push((c.name.clone(), m));
            }
        }
        todo.extend(t.hier.of.iter().map(|(p, _)| (p.clone(), depth + 1)));
        todo.extend(t.hier.parents.iter().map(|p| (p.clone(), depth + 1)));
        tables.push(t);
    }
    if out.is_empty() {
        return Ok(out);
    }
    // a generated column made from a masked one holds its value as it is (or one worked out from it), whenever it
    // was added: masked too, NULL if one it reads is, else HASH (the last characters of what it works out could be
    // the value's own)
    for t in &tables {
        for g in t.cols.iter().filter(|c| !c.hidden) {
            let Some(text) = g.generated.as_deref() else { continue };
            if out.iter().any(|(o, _)| *o == g.name) {
                continue;
            }
            let Ok(e) = check_expr(text) else { continue };
            let mut read: Vec<char> = vec![];
            walk(&e, &mut |x| {
                if let Expr::Col(_, n) = x
                    && let Some((_, m)) = out.iter().find(|(o, _)| o == n)
                {
                    read.push(*m);
                }
            });
            if !read.is_empty() {
                out.push((g.name.clone(), if read.contains(&NULL) { NULL } else { HASH }));
            }
        }
    }
    Ok(out)
}

/// Whether `src`, a table read where masks apply, is one `wrap` makes a subquery of: not the table
/// inside such a subquery, a WITH query or its rows so far.
pub(super) fn wraps(src: &Source) -> bool {
    src.alias != INNER && src.sub.is_none() && src.rec.is_none() && !src.working && src.cte == 0 && src.srf.is_none()
}

/// For a read `src` of a table with masked columns (in `v`, and in `past` when it is read AS OF a moment, where
/// the masks then count as well as those now): `SELECT a, <masked b>, ... FROM table`, whose columns are the
/// table's under their names, the masked ones masked; with a last column `ctid` if the query reads it (`ctid`)
/// and the table has one to give (see `ctid_source`), and then true. None if it has none.
pub(super) fn wrap(v: &View, past: Option<&View>, src: &Source, ctid: bool) -> Result<Option<(Select, bool)>> {
    let mut masked = of(v, &src.table)?;
    if let Some(p) = past {
        for (c, m) in of(p, &src.table)? {
            if !masked.iter().any(|(o, _)| *o == c) {
                masked.push((c, m));
            }
        }
    }
    if masked.is_empty() {
        return Ok(None);
    }
    let Some(t) = load_src(past.unwrap_or(v), src)? else { return Ok(None) };
    let key = key_id(v.core)?;
    let mut body = select_of(Expr::Lit(Value::Null));
    body.items = t
        .cols
        .iter()
        .filter(|c| !c.hidden)
        .map(|c| {
            let col = Expr::Col(None, c.name.clone());
            let e = match masked.iter().find(|(n, _)| *n == c.name) {
                Some((_, m)) => masked_expr(col, *m, key),
                None => col,
            };
            Item::Expr(e, Some(c.name.clone()))
        })
        .collect();
    let ctid = ctid
        && src.at.is_none()
        && t.schema
        && !t.cols.iter().any(|c| c.name == "ctid")
        && (src.only || !t.hier.is_parent());
    if ctid {
        body.items.push(Item::Expr(Expr::Col(None, "ctid".into()), Some("ctid".into())));
    }
    body.from = Some(Source {
        table: src.table.clone(),
        alias: INNER.into(),
        sub: None,
        names: vec![],
        rec: None,
        working: false,
        at: src.at.clone(),
        lateral: false,
        srf: None,
        cte: 0,
        only: src.only,
        hier: false,
        policed: src.policed,
        bound: src.bound,
    });
    Ok(Some((body, ctid)))
}

/// The masked subquery `wrap` made of a table read `src`, if it is one: FOR UPDATE locks the table's rows through it.
pub(super) fn wrapped_of(src: &Source) -> Option<&Select> {
    src.sub.as_deref().filter(|b| {
        b.set.is_empty()
            && b.joins.is_empty()
            && b.from.as_ref().is_some_and(|f| f.alias == INNER && f.table == src.table)
    })
}

/// Whether column `name` of a table read through `wrap`'s subquery `body` is masked there.
pub(super) fn masked_in(body: &Select, name: &str) -> bool {
    body.items.iter().any(|i| matches!(i, Item::Expr(e, Some(n)) if n == name && !matches!(e, Expr::Col(..))))
}

/// Column `col` masked by mode `m`.
fn masked_expr(col: Expr, m: char, key: i64) -> Expr {
    Expr::Func(Func::Scalar(funcs::Scalar::Mask(m as u8)), vec![col, Expr::Lit(Value::Int(key))])
}

/// The masks' keys, by the number a masked expression names its key by (see `key_id`): kept for the life of the
/// process, so the expression carries no secret (EXPLAIN shows it) and runs on any thread.
static KEYS: Mutex<Vec<[u8; 32]>> = Mutex::new(Vec::new());

/// `KEYS`, locked (std's lock, as a `static` takes: see `crate::sync`; its data stays whole if a holder panicked).
fn keys() -> std::sync::MutexGuard<'static, Vec<[u8; 32]>> {
    KEYS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The number of database `core`'s mask key (see `KEYS`).
fn key_id(core: &crate::Core) -> Result<i64> {
    let key = core.mask_key()?;
    let mut keys = keys();
    let at = match keys.iter().position(|k| *k == key) {
        Some(i) => i,
        None => {
            keys.push(key);
            keys.len() - 1
        }
    };
    Ok(at as i64)
}

/// A masked value: what Scalar::Mask(mode) gives for `a` (the value, the key's number).
pub(super) fn apply(mode: u8, a: &[Value]) -> Result<Value> {
    let v = match a.first() {
        None | Some(Value::Null) => return Ok(Value::Null),
        Some(v) => v,
    };
    Ok(match mode as char {
        NULL => Value::Null,
        PARTIAL => Value::Text(partial(&v.to_text())),
        m => {
            let Some(Value::Int(id)) = a.get(1) else { return err("XX000", "a masked value without its key") };
            let key = keys().get(*id as usize).copied();
            let Some(key) = key else { return err("XX000", "a masked value's key isn't known") };
            Value::Text(match m {
                FORMAT => shaped(&key, &v.to_text()),
                _ => hashed(&key, &v.to_text()),
            })
        }
    })
}

/// HASH's text: `m_` and 24 hex digits of the keyed hash of the value's text.
fn hashed(key: &[u8; 32], text: &str) -> String {
    let h = blake3::keyed_hash(key, text.as_bytes());
    format!("m_{}", &crate::pages::hex(h.as_bytes())[..24])
}

/// FORMAT's text: the value's shape with keyed stand-ins: each ASCII letter a letter of the same case, each digit
/// a digit, any other letter `x`, and everything else (`@`, `.`, `-`, spaces) kept. The same value gives the same
/// text, so joins and GROUP BY still work; unlike HASH, the shape (and length) shows.
fn shaped(key: &[u8; 32], text: &str) -> String {
    let mut stream = blake3::Hasher::new_keyed(key).update(text.as_bytes()).finalize_xof();
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let mut b = [0u8; 1];
        stream.fill(&mut b);
        let r = b[0];
        out.push(match c {
            'a'..='z' => (b'a' + r % 26) as char,
            'A'..='Z' => (b'A' + r % 26) as char,
            '0'..='9' => (b'0' + r % 10) as char,
            c if c.is_alphanumeric() => 'x',
            c => c,
        });
    }
    out
}

/// PARTIAL's text: the last four characters, the rest `*` (all of them for four or fewer).
fn partial(text: &str) -> String {
    let n = text.chars().count();
    let keep = if n > 4 { 4 } else { 0 };
    let mut out: String = "*".repeat(n - keep);
    out.extend(text.chars().skip(n - keep));
    out
}

/// A JSON value masked by mode `m` (DIFF's and MERGE ... DRY RUN's rows).
fn masked_json(j: &Json, m: char, key: &[u8; 32]) -> Json {
    let text = match j {
        Json::Null => return Json::Null,
        Json::String(s) => s.clone(),
        other => other.to_string(),
    };
    match m {
        NULL => Json::Null,
        PARTIAL => Json::String(partial(&text)),
        FORMAT => Json::String(shaped(key, &text)),
        _ => Json::String(hashed(key, &text)),
    }
}

fn refuse<T>(message: String) -> Result<T> {
    Err(SqlError { code: "42501", message, info: None })
}

/// An UPDATE, DELETE or INSERT ... ON CONFLICT DO UPDATE whose WHERE, SET or RETURNING reads a masked column of the
/// table it writes would read it as it is (only reads in FROM are masked): refused, as is MERGE into a masked
/// table. `st` as `expand_stmt` has it before its reads are masked (writes to views made writes to their tables).
pub(super) fn target(v: &View, st: &Stmt) -> Result<()> {
    // (the rewrite of a statement rewritten already, whose reads are masked: it was checked then, and its masks'
    // own reads of the table would count here)
    if wrapped(st) {
        return Ok(());
    }
    let table = match st {
        Stmt::Update { table, .. } | Stmt::Delete { table, .. } => table,
        Stmt::Insert { table, conflict: Some(_), .. } => table,
        // a plain INSERT reads the target only in its RETURNING (the rows it wrote)
        Stmt::Insert { table, alias, returning: Some(items), .. } => {
            let masked = of(v, table)?;
            if masked.is_empty() {
                return Ok(());
            }
            // the target's whole row (`row_to_json(t)`, `t.*`) is every column, the masked ones too; a column of
            // the name is the column
            let named = |n: &str| n == rel_part(table) || alias.as_deref() == Some(n);
            let cols: Vec<String> = match load(v, table)? {
                Some(t) => t.cols.into_iter().map(|c| c.name).collect(),
                None => vec![],
            };
            let mut read: Option<String> = None;
            for i in items {
                match i {
                    Item::Star | Item::TableStar(_) => read = read.or_else(|| Some(masked[0].0.clone())),
                    Item::Expr(e, _) => walk(e, &mut |x| match x {
                        Expr::Col(Some(q), n) if n == "*" && named(q) => {
                            read.get_or_insert_with(|| masked[0].0.clone());
                        }
                        Expr::Col(None, n) if named(n) && !cols.contains(n) => {
                            read.get_or_insert_with(|| masked[0].0.clone());
                        }
                        Expr::Col(_, n) if masked.iter().any(|(c, _)| c == n) => {
                            read.get_or_insert_with(|| n.clone());
                        }
                        _ => {}
                    }),
                }
            }
            return match read {
                Some(c) => refuse(format!(
                    "column \"{c}\" of table \"{}\" is masked, and RETURNING would show it as it is: return the \
                     other columns",
                    rel_part(table)
                )),
                None => Ok(()),
            };
        }
        Stmt::Merge(m) => {
            if let Some(t) = merge_target(m)
                && !of(v, &t)?.is_empty()
            {
                return refuse(format!(
                    "table \"{}\" has masked columns, and MERGE reads its rows as they are: an agent without \
                     'unmask' writes it with INSERT, UPDATE and DELETE",
                    rel_part(&t)
                ));
            }
            return Ok(());
        }
        _ => return Ok(()),
    };
    let masked = of(v, table)?;
    if masked.is_empty() {
        return Ok(());
    }
    for (rel, cols) in acl::columns_read(v, st)? {
        if rel != *table {
            continue;
        }
        if let Some((c, _)) = masked.iter().find(|(c, _)| cols.contains(c)) {
            return refuse(format!(
                "column \"{c}\" of table \"{}\" is masked, and this statement's WHERE, SET or RETURNING would read \
                 it as it is: an agent without 'unmask' may write the column but not read it here (read it with \
                 SELECT, which shows it masked)",
                rel_part(table)
            ));
        }
    }
    Ok(())
}

/// Whether statement `st` reads a table through a masked subquery (see `wrap`).
fn wrapped(st: &Stmt) -> bool {
    let found = std::cell::Cell::new(false);
    let _ = functions::visit(&mut st.clone(), &mut |_| Ok(()), &mut |q| {
        let mut srcs = q.from.iter().chain(q.joins.iter().map(|j| &j.src));
        found.set(found.get() || srcs.any(|s| s.alias == INNER));
    });
    found.get()
}

fn merge_target(m: &MergeStmt) -> Option<String> {
    Some(m.table.clone())
}

/// Changing the definition of a table with masked columns (ALTER TABLE, its indexes, triggers, rules and
/// policies), which could copy or test its values as they are (a generated column, a trigger, a CHECK whose
/// error tells): refused where masks apply.
pub(super) fn ddl(v: &View, st: &Stmt) -> Result<()> {
    let table = match st {
        Stmt::Alter { table, .. } | Stmt::CreateIndex { table, .. } => table.clone(),
        Stmt::CreateTrigger(_, table, _)
        | Stmt::DropTrigger { table, .. }
        | Stmt::RenameTrigger { table, .. }
        | Stmt::RenameRule { table, .. } => table.clone(),
        // CREATE, DROP and ALTER TABLE ... ENABLE | DISABLE RULE (a rule's action reads OLD and NEW as they are)
        Stmt::UserDef { key, .. } => match rules::table_of(key) {
            Some(t) => t.to_string(),
            None => return Ok(()),
        },
        // (dropping a policy shows the rows it hid)
        Stmt::Policy(p) => match &**p {
            rls::PolicyStmt::Create { table, .. }
            | rls::PolicyStmt::Alter { table, .. }
            | rls::PolicyStmt::Drop { table, .. } => table.clone(),
        },
        _ => return Ok(()),
    };
    if of(v, &table)?.is_empty() {
        return Ok(());
    }
    refuse(format!(
        "table \"{}\" has masked columns: an agent without 'unmask' may not change its definition, its indexes, \
         triggers, rules or policies (they could read the masked values as they are)",
        rel_part(&table)
    ))
}

/// `search()` over a table with masked columns, whose text index holds their values as they are: refused where
/// masks apply.
pub(super) fn search(v: &View, table: &str) -> Result<()> {
    if of(v, table)?.is_empty() {
        return Ok(());
    }
    refuse(format!(
        "table \"{}\" has masked columns, and search() matches their values as they are: search it with SQL \
         (WHERE ... @@, ORDER BY ... <=>), which reads them masked",
        rel_part(table)
    ))
}

/// DIFF's and MERGE ... DRY RUN's rows (a `table` column, then JSON columns of rows) with the masked columns'
/// values masked, for a statement masks apply to; worlds `views` are where the tables' masks are looked up
/// (a table masked in any of them is).
pub(super) fn output(views: &[&View], out: &mut Output) -> Result<()> {
    let Some(cols) = &out.columns else { return Ok(()) };
    let Some(t_at) = cols.iter().position(|c| c.0 == "table") else { return Ok(()) };
    let json_at: Vec<usize> = cols.iter().enumerate().filter(|(_, c)| c.1 == Type::Json).map(|(i, _)| i).collect();
    let mut known: HashMap<String, Vec<(String, char)>> = HashMap::new();
    let mut key: Option<[u8; 32]> = None;
    for row in &mut out.rows {
        let Some(Value::Text(t)) = row.get(t_at) else { continue };
        if !known.contains_key(t) {
            let mut masked: Vec<(String, char)> = vec![];
            for v in views {
                for (c, m) in of(v, t)? {
                    if !masked.iter().any(|(o, _)| *o == c) {
                        masked.push((c, m));
                    }
                }
            }
            known.insert(t.clone(), masked);
        }
        let masked = &known[t];
        if masked.is_empty() {
            continue;
        }
        if key.is_none() {
            key = Some(views[0].core.mask_key()?);
        }
        let k = key.as_ref().expect("set above");
        for &i in &json_at {
            let Some(Value::Json(j)) = row.get_mut(i) else { continue };
            match j {
                Json::Object(o) => {
                    for (c, m) in masked {
                        if let Some(x) = o.get_mut(c) {
                            *x = masked_json(x, *m, k);
                        }
                    }
                }
                // (the columns that changed, by name: no values)
                Json::Array(_) => {}
                // a row that isn't an object of columns: nothing of it is shown
                other if !other.is_null() => *other = Json::String("masked".into()),
                _ => {}
            }
        }
    }
    Ok(())
}

/// DIFF ... AS SQL writes rows' values into statements: refused where masks apply when a table it shows has
/// masked columns.
pub(super) fn diff_sql(v: &View, tables: &BTreeSet<String>) -> Result<()> {
    for t in tables {
        if !of(v, t)?.is_empty() {
            return refuse(format!(
                "table \"{}\" has masked columns, and DIFF ... AS SQL writes their values: use DIFF, which shows \
                 them masked",
                rel_part(t)
            ));
        }
    }
    Ok(())
}

/// For the HTTP and MCP row API, which reads rows as they are, as agent `agent` (one without `unmask`): the message
/// refusing it the rows of `tables` in world `branch` when one has masked columns.
pub(crate) fn agent_refused(db: &Db, agent: &str, branch: &str, tables: &[&str]) -> Result<Option<String>> {
    if tables.is_empty() || !db.core.masks_made() {
        return Ok(None);
    }
    let v = db.view_tx(branch)?;
    for t in tables {
        if !of(&v, t)?.is_empty() {
            return Ok(Some(format!(
                "table \"{}\" has masked columns, and the row API works on rows as they are: read it with SQL (the \
                 sql tool, or POST /v1/sql), which shows them masked ({} hasn't 'unmask')",
                rel_part(t),
                if agent.contains(' ') { agent.to_string() } else { format!("agent {agent}") }
            )));
        }
    }
    Ok(None)
}

/// The masks the columns of a table made from query `q` (CREATE TABLE AS, a materialized view) take, by place
/// (`n` columns), when a person makes it: a copy of a masked column's values is masked too. A column that is a
/// masked column as it is keeps its mode; one worked out from masked columns (or with a subquery that reads one)
/// is masked with NULL if one it reads is masked with NULL, else with HASH. A query too involved to follow column by
/// column (UNION, a subquery or function in FROM) that reads any masked column masks every column with HASH.
pub(super) fn derived(v: &View, q: &Select, n: usize) -> Result<Vec<Option<char>>> {
    // what the whole query reads that's masked: none, and nothing is masked (the usual case)
    let reads = impact::plain_reads(&Stmt::Select(Box::new(q.clone())));
    let mut masks: HashMap<String, Vec<(String, char)>> = HashMap::new();
    for t in reads.iter().map(|(t, _)| t) {
        if !masks.contains_key(t) {
            masks.insert(t.clone(), of(v, t)?);
        }
    }
    let reads_masked = |reads: &impact::Reads| {
        reads
            .iter()
            .any(|(t, c)| masks.get(t).is_some_and(|m| (c == "*" && !m.is_empty()) || m.iter().any(|(x, _)| x == c)))
    };
    if !reads_masked(&reads) {
        return Ok(vec![None; n]);
    }
    let all = vec![Some(HASH); n];
    let sources: Vec<&Source> = q.from.iter().chain(q.joins.iter().map(|j| &j.src)).collect();
    if !q.set.is_empty() || sources.iter().any(|s| s.sub.is_some() || s.rec.is_some() || s.srf.is_some()) {
        return Ok(all);
    }
    // this level's tables: (alias, table, its visible columns, its masks)
    let mut level = vec![];
    for s in &sources {
        let Some(t) = load(v, &s.table)? else { return Ok(all) };
        let cols: Vec<String> = t.cols.iter().filter(|c| !c.hidden).map(|c| c.name.clone()).collect();
        level.push((s.alias.clone(), s.table.clone(), cols, of(v, &s.table)?));
    }
    let mode_of = |table: &(String, String, Vec<String>, Vec<(String, char)>), c: &str| {
        table.3.iter().find(|(x, _)| x == c).map(|(_, m)| *m)
    };
    let mut out = vec![];
    for item in &q.items {
        match item {
            Item::Star => {
                for t in &level {
                    out.extend(t.2.iter().map(|c| mode_of(t, c)));
                }
            }
            Item::TableStar(a) => {
                let Some(t) = level.iter().find(|t| t.0 == *a || t.1 == *a) else { return Ok(all) };
                out.extend(t.2.iter().map(|c| mode_of(t, c)));
            }
            Item::Expr(e, _) => {
                // the masked columns it reads, and whether it is one of them as it is
                let mut found: Vec<char> = vec![];
                let mut sub = false;
                walk(e, &mut |x| match x {
                    Expr::Col(qual, name) => {
                        let at = level.iter().filter(|t| match qual {
                            Some(q) => t.0 == *q || t.1 == *q,
                            None => t.2.iter().any(|c| c == name),
                        });
                        found.extend(at.filter_map(|t| mode_of(t, name)));
                    }
                    Expr::Sub(sq) | Expr::Exists(sq, _) | Expr::InSub(_, sq, _) => {
                        sub |= reads_masked(&impact::plain_reads(&Stmt::Select(sq.q.clone())))
                    }
                    _ => {}
                });
                out.push(match (e, found.first()) {
                    (Expr::Col(..), Some(m)) if found.len() == 1 && !sub => Some(*m),
                    _ if found.contains(&NULL) => Some(NULL),
                    _ if !found.is_empty() || sub => Some(HASH),
                    _ => None,
                });
            }
        }
    }
    Ok(if out.len() == n { out } else { all })
}
