//! Attached to MongoDB: `chronos attach <dir> --tables a,b` with CHRONOS_SOURCE_URL `mongodb://...` (the collections
//! of the database the URL names). The same promise as with Postgres and MySQL: main is a copy of the collections; a
//! merge into main is written to MongoDB first, in one multi-document transaction, after checking each document it
//! writes is still as main has it (else a conflict, and nothing is written); a crash around the commit is settled
//! from `chronos_merges`; `chronos refresh` brings main up to MongoDB. Transactions need a replica set (Atlas is one):
//! a standalone server is refused at attach.
//!
//! A collection is held as one table: `_id` (text), the fields named by `--fields` each as a column of its own (a
//! tenant's id, say), and `doc` (jsonb), the rest of the document. Values are MongoDB's relaxed Extended JSON, so an
//! ObjectId is `{"$oid": "..."}` and a date `{"$date": "..."}`; numbers are plain. `_id` is the ObjectId's 24 hex
//! digits, or any other id as JSON (`"abc"`, `7`).
//!
//! An update is sent as `$set` and `$unset` of the fields that changed, by path, never the whole document: the fields
//! it leaves keep their order and their types. A number written to a field that held one keeps that field's type
//! (a double stays a double); in a new field an integer is an int (a long past 32 bits) and a fraction a double.
//! Arrays are one value: a changed array is written whole.
//!
//! Not yet for MongoDB (each refused, saying so): `serve --follow`, rehearsals, merge checks
//! run in the source, write-back rules, `pg_role`, `--lazy-tenant`, sharded clusters, views, time-series and capped
//! collections.

use super::mongo_json::{ext, ext_doc, unext};
use super::*;
use mongodb::bson::{self, Bson, Document, doc, oid::ObjectId};
use mongodb::sync::{Client, ClientSession, Database};

/// The marker and merge collection: `chronos.merges` as MongoDB holds it, in the attached database.
const MERGES: &str = "chronos_merges";
/// The rows each merge wrote, kept to undo it by (see `super::undo`).
const MERGE_ROWS: &str = "chronos_merge_rows";
/// Documents read or written by `_id` in one command, at most.
const IDS: usize = 1000;
/// The largest document attach takes, as Extended JSON text: past it the copy would hold megabytes a row.
const MAX_DOC: usize = 4 << 20;

const CHANGED: &str = super::CHANGED_IN_MONGO;

pub(crate) fn is_mongo(url: &str) -> bool {
    url.starts_with("mongodb://") || url.starts_with("mongodb+srv://")
}

/// The client for the source's URL, kept between uses (it holds the connections and what it knows of the servers).
fn client() -> std::result::Result<(Client, Database), String> {
    static KEPT: Mutex<Option<(String, Client)>> = Mutex::new(None);
    let url = super::source_text()?;
    if !is_mongo(&url) {
        return Err("this folder is attached to MongoDB: CHRONOS_SOURCE_URL must be a mongodb:// URL".into());
    }
    let mut kept = KEPT.lock().unwrap_or_else(|p| p.into_inner());
    let client = match kept.as_ref() {
        Some((u, c)) if *u == url => c.clone(),
        _ => {
            let c = Client::with_uri_str(&url).map_err(|e| format!("MongoDB: {}", plain(&e)))?;
            *kept = Some((url.clone(), c.clone()));
            c
        }
    };
    let db = client.default_database().ok_or(
        "the MongoDB URL names no database: mongodb://user:password@host:27017/<database> (its collections are \
         the ones attached)",
    )?;
    Ok((client, db))
}

/// An error of the driver's without the password a URL in it might show.
fn plain(e: &mongodb::error::Error) -> String {
    let text = e.to_string();
    match (text.find("://"), text.find('@')) {
        (Some(a), Some(b)) if a < b => format!("{}://...{}", &text[..a], &text[b..]),
        _ => text,
    }
}

/// A MongoDB failure as the merge's error, as `super::failed` says a Postgres one.
fn failed(what: &str, e: &mongodb::error::Error) -> Error {
    let retry = e.contains_label("TransientTransactionError") || e.contains_label("UnknownTransactionCommitResult");
    let dup = plain(e).contains("E11000");
    let again = if retry { "; nothing was written: safe to merge again" } else { "; nothing was written" };
    Error::Constraint {
        code: if retry { "40001" } else { "23000" },
        message: format!("MongoDB, {what}: {}{again}", plain(e)),
    }
    .with_code(if dup {
        "23505"
    } else if retry {
        "40001"
    } else {
        "23000"
    })
}

// ---------------------------------------------------------------- documents as rows

/// How a field kept as a column of its own is typed in MongoDB (the column's `ty`).
const TY_STRING: &str = "mongo:string";
const TY_INT: &str = "mongo:int";
const TY_LONG: &str = "mongo:long";
const TY_OID: &str = "mongo:oid";

/// An `_id` as the copy's key: an ObjectId's hex, anything else as JSON.
fn id_text(id: &Bson) -> String {
    match id {
        Bson::ObjectId(o) => o.to_hex(),
        other => ext(other).to_string(),
    }
}

/// The `_id` a key stands for. Err: not one `id_text` writes.
fn id_bson(text: &str) -> std::result::Result<Bson, String> {
    if text.len() == 24 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return ObjectId::parse_str(text).map(Bson::ObjectId).map_err(|e| e.to_string());
    }
    let j: Json = serde_json::from_str(text).map_err(|_| {
        format!(
            "_id {text:?} is neither an ObjectId (24 hex digits) nor JSON: write a string id with its quotes \
             ('\"abc\"'), a number as it is"
        )
    })?;
    to_bson(&j, None).map_err(|e| format!("_id {text:?}: {e}"))
}

/// A field kept as a column, as the column's text (None: absent or null).
fn lifted_text(v: Option<&Bson>) -> Option<String> {
    match v? {
        Bson::Null => None,
        Bson::String(s) => Some(s.clone()),
        Bson::ObjectId(o) => Some(o.to_hex()),
        Bson::Int32(n) => Some(n.to_string()),
        Bson::Int64(n) => Some(n.to_string()),
        other => Some(ext(other).to_string()),
    }
}

/// A column's text as the field's value in MongoDB, by the type the column was attached with.
fn lifted_bson(ty: &str, text: &str) -> std::result::Result<Bson, String> {
    let bad = |what: &str| format!("{text:?} isn't {what}, which this field holds in MongoDB");
    Ok(match ty {
        TY_INT => Bson::Int32(text.trim().parse().map_err(|_| bad("a 32-bit integer"))?),
        TY_LONG => Bson::Int64(text.trim().parse().map_err(|_| bad("an integer"))?),
        TY_OID => Bson::ObjectId(ObjectId::parse_str(text.trim()).map_err(|_| bad("an ObjectId"))?),
        _ => Bson::String(text.to_string()),
    })
}

/// The columns of `t` between `_id` and `doc`: the fields kept as columns.
fn lifted(t: &Table) -> std::ops::Range<usize> {
    1..t.cols.len() - 1
}

/// A document as the copy's row: `_id`, the lifted fields, and the rest as Extended JSON.
fn row_of(t: &Table, mut d: Document) -> Texts {
    let id = d.remove("_id").unwrap_or(Bson::Null);
    let mut row = vec![Some(id_text(&id))];
    for i in lifted(t) {
        row.push(lifted_text(d.remove(&t.cols[i].name).as_ref()));
    }
    row.push(Some(ext_doc(&d).to_string()));
    row
}

/// Whether two JSON values are the same value: numbers by what they count, objects whatever their keys' order.
fn same_json(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => {
            x == y
                || match (x.as_i64(), y.as_i64()) {
                    (Some(p), Some(q)) => p == q,
                    _ => x.as_f64().zip(y.as_f64()).is_some_and(|(p, q)| p == q),
                }
        }
        (Json::Array(x), Json::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same_json(p, q)),
        (Json::Object(x), Json::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, p)| y.get(k).is_some_and(|q| same_json(p, q)))
        }
        _ => a == b,
    }
}

fn parse(text: Option<&str>) -> Json {
    text.and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Json::Null)
}

/// Whether two rows of `t` are the same document.
fn same_row(t: &Table, a: &Texts, b: &Texts) -> bool {
    let last = t.cols.len() - 1;
    a[..last] == b[..last] && same_json(&parse(a[last].as_deref()), &parse(b[last].as_deref()))
}

/// An object that is one Extended JSON value (`{"$oid": ...}`), not a document of fields.
fn typed(o: &serde_json::Map<String, Json>) -> bool {
    o.keys().next().is_some_and(|k| k.starts_with('$'))
}

/// Whether `o`'s fields can each be named by a dotted path (none has a dot, or starts with `$`).
fn pathable(o: &serde_json::Map<String, Json>) -> bool {
    o.keys().all(|k| !k.contains('.') && !k.starts_with('$') && !k.is_empty())
}

/// JSON as the value MongoDB stores, keeping the type of the number it replaces (`was`): see the module's doc.
fn to_bson(new: &Json, was: Option<&Bson>) -> std::result::Result<Bson, String> {
    Ok(match new {
        Json::Null => Bson::Null,
        Json::Bool(b) => Bson::Boolean(*b),
        Json::String(s) => Bson::String(s.clone()),
        Json::Number(n) => {
            let whole =
                n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9e15).map(|f| f as i64));
            match (was, whole) {
                (Some(Bson::Double(_)), _) | (_, None) => {
                    Bson::Double(n.as_f64().ok_or_else(|| format!("{n} is no number MongoDB holds"))?)
                }
                (Some(Bson::Int64(_)), Some(i)) => Bson::Int64(i),
                // (a number written with a fraction part stays a double: 5.0 isn't made an int)
                (_, Some(i)) if n.as_i64().is_none() && !matches!(was, Some(Bson::Int32(_))) => Bson::Double(i as f64),
                (_, Some(i)) => i32::try_from(i).map_or(Bson::Int64(i), Bson::Int32),
            }
        }
        Json::Array(a) => {
            let old = match was {
                Some(Bson::Array(o)) => Some(o),
                _ => None,
            };
            let items: std::result::Result<Vec<Bson>, String> =
                a.iter().enumerate().map(|(i, v)| to_bson(v, old.and_then(|o| o.get(i)))).collect();
            Bson::Array(items?)
        }
        Json::Object(o) if typed(o) => unext(o)?,
        Json::Object(o) => {
            let old = match was {
                Some(Bson::Document(d)) => Some(d),
                _ => None,
            };
            let mut d = Document::new();
            for (k, v) in o {
                d.insert(k.clone(), to_bson(v, old.and_then(|o| o.get(k)))?);
            }
            Bson::Document(d)
        }
    })
}

/// The value at dotted `path` of `d`.
fn at_path<'a>(d: &'a Document, path: &str) -> Option<&'a Bson> {
    let mut parts = path.split('.');
    let mut now = d.get(parts.next()?)?;
    for p in parts {
        now = now.as_document()?.get(p)?;
    }
    Some(now)
}

/// What turns document `old` into `new` (both without `_id` and the lifted fields), field by field: the paths to set
/// (with their values) and the paths to unset. An object whose fields can't be named by a path is set whole.
fn diff(path: &str, old: &Json, new: &Json, sets: &mut Vec<(String, Json)>, unsets: &mut Vec<String>) {
    match (old, new) {
        (Json::Object(o), Json::Object(n))
            if !typed(o) && !typed(n) && pathable(o) && pathable(n) && !path.is_empty() || path.is_empty() =>
        {
            let (Json::Object(o), Json::Object(n)) = (old, new) else { return };
            let at = |k: &str| if path.is_empty() { k.to_string() } else { format!("{path}.{k}") };
            for k in o.keys().filter(|k| !n.contains_key(*k)) {
                unsets.push(at(k));
            }
            for (k, v) in n {
                match o.get(k) {
                    None => sets.push((at(k), v.clone())),
                    Some(was) => diff(&at(k), was, v, sets, unsets),
                }
            }
        }
        _ if same_json(old, new) => {}
        _ => sets.push((path.to_string(), new.clone())),
    }
}

/// The document a row the merge inserts stands for.
fn document(t: &Table, row: &Texts) -> std::result::Result<Document, String> {
    let last = t.cols.len() - 1;
    let mut d = doc! { "_id": id_bson(row[0].as_deref().unwrap_or_default())? };
    for i in lifted(t) {
        if let Some(v) = &row[i] {
            d.insert(t.cols[i].name.clone(), lifted_bson(&t.cols[i].ty, v)?);
        }
    }
    let Json::Object(fields) = parse(row[last].as_deref()) else {
        return Err(format!("{}.doc must be a JSON object (the document's fields)", t.name));
    };
    for (k, v) in &fields {
        if k == "_id" || t.cols[lifted(t)].iter().any(|c| &c.name == k) {
            return Err(format!(
                "{}.doc holds the field {k}, which is a column of its own here: set the column, not the field",
                t.name
            ));
        }
        if k.contains('.') || k.starts_with('$') || k.is_empty() {
            return Err(format!("{}.doc holds a field named {k:?}: MongoDB can't address it by name", t.name));
        }
        d.insert(k.clone(), to_bson(v, None)?);
    }
    Ok(d)
}

/// The `$set` and `$unset` that turn `now` (MongoDB's document, which is main's) into row `after`.
fn update_of(t: &Table, now: &Document, before: &Texts, after: &Texts) -> std::result::Result<Document, String> {
    let last = t.cols.len() - 1;
    let (mut sets, mut unsets) = (vec![], vec![]);
    let new = parse(after[last].as_deref());
    let Some(fields) = new.as_object() else {
        return Err(format!("{}.doc must be a JSON object (the document's fields)", t.name));
    };
    if let Some(k) = fields.keys().find(|k| k.contains('.')) {
        return Err(format!("{}.doc holds a field named {k:?}: MongoDB can't address it by name", t.name));
    }
    diff("", &parse(before[last].as_deref()), &new, &mut sets, &mut unsets);
    let mut set = Document::new();
    let mut unset = Document::new();
    for (path, v) in &sets {
        let top = path.split('.').next().unwrap_or_default();
        if top == "_id" || t.cols[lifted(t)].iter().any(|c| c.name == top) {
            return Err(format!(
                "{}.doc holds the field {top}, which is a column of its own here: set the column, not the field",
                t.name
            ));
        }
        if top.starts_with('$') || top.is_empty() {
            return Err(format!("{}.doc holds a field named {top:?}: MongoDB can't address it by name", t.name));
        }
        set.insert(path.clone(), to_bson(v, at_path(now, path))?);
    }
    for path in unsets {
        unset.insert(path, "");
    }
    for i in lifted(t) {
        match (&before[i], &after[i]) {
            (a, b) if a == b => {}
            (_, Some(v)) => {
                set.insert(t.cols[i].name.clone(), lifted_bson(&t.cols[i].ty, v)?);
            }
            (_, None) => {
                unset.insert(t.cols[i].name.clone(), "");
            }
        }
    }
    let mut update = Document::new();
    if !set.is_empty() {
        update.insert("$set", set);
    }
    if !unset.is_empty() {
        update.insert("$unset", unset);
    }
    Ok(update)
}

/// The documents of `t` whose keys are `keys`, as MongoDB has them (in `session`'s transaction or snapshot).
fn by_ids(
    db: &Database,
    session: &mut ClientSession,
    t: &Table,
    keys: &[String],
) -> std::result::Result<HashMap<String, Document>, mongodb::error::Error> {
    let coll = db.collection::<Document>(&t.pg);
    let mut out = HashMap::new();
    for part in keys.chunks(IDS) {
        let ids: Vec<Bson> = part.iter().filter_map(|k| id_bson(k).ok()).collect();
        let mut cursor = coll.find(doc! { "_id": { "$in": ids } }).session(&mut *session).run()?;
        for d in cursor.iter(&mut *session) {
            let d = d?;
            out.insert(id_text(d.get("_id").unwrap_or(&Bson::Null)), d);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- the write-back

#[allow(clippy::too_many_arguments)]
pub(super) fn write_back(
    core: &Core,
    att: &Attached,
    world: &str,
    world_id: &str,
    rows: &[Row],
    reads: Option<&Reads>,
    checks: &[crate::checks::MergeCheck],
    who: Option<&ActAs>,
    rules: &Rules,
) -> Result<Option<Held>> {
    let no = |what: &str| {
        Err(refused(format!(
            "{what} works with a Postgres source only for now, and this folder is attached to MongoDB ({}): nothing \
             was merged",
            att.source()
        )))
    };
    if who.is_some() {
        return no("writing as the end user's role (pg_role)");
    }
    if rules.observe || rules.serializable || rules.has_tables() {
        return no("write-back rules (writeback.json)");
    }
    if !checks.is_empty() {
        return no("running merge checks in the source");
    }
    let (client, db) = client().map_err(|e| refused(format!("{e}; nothing was merged")))?;
    let mut session = client.start_session().run().map_err(|e| failed("starting a session", &e))?;
    let out = transaction(core, att, &client, &db, &mut session, world, world_id, rows, reads);
    if !matches!(out, Ok(None)) {
        let _ = session.abort_transaction().run();
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn transaction(
    core: &Core,
    att: &Attached,
    client: &Client,
    db: &Database,
    session: &mut ClientSession,
    world: &str,
    world_id: &str,
    rows: &[Row],
    reads: Option<&Reads>,
) -> Result<Option<Held>> {
    let tables = att.tables();
    let by_table = by_table(rows);
    let keeps_undo = super::undo::ready(att);
    session.start_transaction().run().map_err(|e| failed("starting the write-back", &e))?;
    let marker = db
        .collection::<Document>(MERGES)
        .count_documents(doc! { "_id": att.marker() })
        .session(&mut *session)
        .run()
        .map_err(|e| failed("reading chronos_merges", &e))?;
    if marker != 1 {
        return Err(refused(format!(
            "the MongoDB CHRONOS_SOURCE_URL names isn't the one this folder was attached to ({}): its chronos_merges \
             has no document {}; nothing was merged",
            att.source(),
            att.marker()
        )));
    }
    // every document it writes, read in the transaction and compared with main's: any that differs, and nothing is
    // written. (One changed by another writer between this read and the write below fails the write: MongoDB aborts
    // a transaction that writes what another committed after its snapshot.)
    let mut now: HashMap<(usize, String), Document> = HashMap::new();
    let mut conflicts: Vec<(usize, &Row, Option<Texts>)> = vec![];
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let keys: Vec<String> = rs.iter().map(|r| r.key_texts(t)[0].clone().unwrap_or_default()).collect();
        let found = by_ids(db, session, t, &keys).map_err(|e| failed(&format!("reading {}", t.name), &e))?;
        for (r, key) in rs.iter().zip(&keys) {
            let theirs = found.get(key).map(|d| row_of(t, d.clone()));
            let changed = match (&r.before, &theirs) {
                (None, None) => false,
                (Some(b), Some(n)) => !same_row(t, b, n),
                _ => true,
            };
            if changed {
                conflicts.push((ti, r, theirs));
            }
        }
        now.extend(found.into_iter().map(|(k, d)| ((ti, k), d)));
    }
    if !conflicts.is_empty() {
        let _ = session.abort_transaction().run();
        let mut lines = vec![];
        let mut stored = vec![];
        for (ti, row, theirs) in &conflicts {
            let t = &tables[*ti];
            let show = |r: &Option<Texts>| r.as_ref().map_or("no document".to_string(), |r| row_json(t, r));
            if lines.len() < 10 {
                lines.push(format!(
                    "  {}: the world's {}, main's {}, MongoDB's {}",
                    crate::sql::show_key(&row.key),
                    show(&row.after),
                    show(&row.before),
                    show(theirs)
                ));
            }
            stored.push(Stored { table: *ti, key: row.key_texts(t), row: theirs.clone() });
        }
        if conflicts.len() > lines.len() {
            lines.push(format!("  ... and {} more", conflicts.len() - lines.len()));
        }
        AFTER.set(Some(After { rows: stored, committed: false, refresh: vec![] }));
        return Err(Error::Constraint {
            code: "40001",
            message: format!(
                "{} {world} changes {CHANGED} (attached {}); nothing was written to MongoDB and nothing merged. main \
                 now holds MongoDB's values for them: merge again, and settle them as any conflict (OURS, THEIRS, or \
                 row by row).\n{}",
                crate::agents::n_rows(conflicts.len() as u64),
                att.source(),
                lines.join("\n")
            ),
        });
    }
    // what the world read, checked against MongoDB before anything is written (see `super::live_reads`): changed,
    // the merge is held for a person, and nothing is written
    if let Some(r) = reads {
        let stale = super::live_reads(core, &mut MoLive { db, session: &mut *session }, att, r)?;
        if !stale.is_empty() {
            return Ok(Some(Held::Stale(stale)));
        }
    }
    // the writes: inserts and updates in the tables' order, then deletes in the reverse
    let merge_id = format!("merge:{}", random_id());
    let said = |t: &Table, e: String| refused(format!("{}: {e}; nothing was merged", t.name));
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let coll = db.collection::<Document>(&t.pg);
        let new: Vec<Document> = rs
            .iter()
            .filter(|r| r.before.is_none())
            .map(|r| document(t, r.after.as_ref().unwrap()).map_err(|e| said(t, e)))
            .collect::<Result<_>>()?;
        for part in new.chunks(IDS) {
            let done = coll
                .insert_many(part.to_vec())
                .session(&mut *session)
                .run()
                .map_err(|e| failed(&format!("insert into {}", t.name), &e))?;
            if done.inserted_ids.len() != part.len() {
                return Err(refused(format!(
                    "MongoDB inserted {} documents of {} where the merge sent {}: nothing was merged",
                    done.inserted_ids.len(),
                    t.name,
                    part.len()
                )));
            }
        }
        for r in rs.iter().filter(|r| r.before.is_some() && r.after.is_some()) {
            let key = r.key_texts(t)[0].clone().unwrap_or_default();
            let Some(theirs) = now.get(&(ti, key.clone())) else { continue };
            let update =
                update_of(t, theirs, r.before.as_ref().unwrap(), r.after.as_ref().unwrap()).map_err(|e| said(t, e))?;
            if update.is_empty() {
                continue;
            }
            let done = coll
                .update_one(doc! { "_id": id_bson(&key).map_err(|e| said(t, e))? }, update)
                .session(&mut *session)
                .run()
                .map_err(|e| failed(&format!("update {}", t.name), &e))?;
            if done.matched_count != 1 {
                return Err(refused(format!(
                    "MongoDB updated {} documents of {} where the merge sent 1: nothing was merged",
                    done.matched_count, t.name
                )));
            }
        }
    }
    for (&ti, rs) in by_table.iter().rev() {
        let t = &tables[ti];
        let coll = db.collection::<Document>(&t.pg);
        let keys: Vec<String> =
            rs.iter().filter(|r| r.after.is_none()).map(|r| r.key_texts(t)[0].clone().unwrap_or_default()).collect();
        for part in keys.chunks(IDS) {
            let ids: Vec<Bson> = part.iter().map(|k| id_bson(k).map_err(|e| said(t, e))).collect::<Result<_>>()?;
            let done = coll
                .delete_many(doc! { "_id": { "$in": ids } })
                .session(&mut *session)
                .run()
                .map_err(|e| failed(&format!("delete from {}", t.name), &e))?;
            if done.deleted_count as usize != part.len() {
                return Err(refused(format!(
                    "MongoDB deleted {} documents of {} where the merge sent {}: nothing was merged",
                    done.deleted_count,
                    t.name,
                    part.len()
                )));
            }
        }
    }
    // the documents written, read back as MongoDB stored them: main takes those
    let mut sent_back: Vec<Stored> = vec![];
    let mut landed: HashMap<&str, Texts> = HashMap::new();
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let written: Vec<&&Row> = rs.iter().filter(|r| r.after.is_some()).collect();
        let keys: Vec<String> =
            written.iter().map(|r| r.after.as_ref().unwrap()[0].clone().unwrap_or_default()).collect();
        let found = by_ids(db, session, t, &keys).map_err(|e| failed(&format!("reading back {}", t.name), &e))?;
        for (r, key) in written.iter().zip(&keys) {
            let Some(d) = found.get(key) else {
                return Err(refused(format!(
                    "MongoDB doesn't hold the document {key} of {} the merge just wrote: nothing was merged",
                    t.name
                )));
            };
            let row = row_of(t, d.clone());
            landed.insert(r.key.as_str(), row.clone());
            sent_back.push(Stored { table: ti, key: vec![Some(key.clone())], row: Some(row) });
        }
        for r in rs.iter().filter(|r| r.after.is_none()) {
            sent_back.push(Stored { table: ti, key: r.key_texts(t), row: None });
        }
    }
    // (an undo's dry run stops here; MongoDB has no cascades or triggers to count)
    super::undo::before_commit(&[])?;
    if keeps_undo {
        let recs = super::undo::records(&tables, rows, &|r: &Row| landed.get(r.key.as_str()).cloned());
        let at = bson::DateTime::now();
        let docs: Vec<Document> = recs
            .iter()
            .map(|j| {
                let text = |k: &str| match &j[k] {
                    Json::Null => Bson::Null,
                    v => Bson::String(v.to_string()),
                };
                doc! {
                    "merge_id": merge_id.as_str(), "n": j["n"].as_i64().unwrap_or(0),
                    "table_name": j["table"].as_str().unwrap_or_default(), "op": j["op"].as_str().unwrap_or_default(),
                    "key": text("key"), "before": text("before"), "after": text("after"), "at": at,
                }
            })
            .collect();
        for part in docs.chunks(IDS) {
            db.collection::<Document>(MERGE_ROWS)
                .insert_many(part.to_vec())
                .session(&mut *session)
                .run()
                .map_err(|e| failed("recording the merge's rows for undo", &e))?;
        }
    }
    db.collection::<Document>(MERGES)
        .insert_one(doc! { "_id": merge_id.as_str(), "at": bson::DateTime::now() })
        .session(&mut *session)
        .run()
        .map_err(|e| failed("recording the merge", &e))?;
    // what recovery needs should the commit's outcome be lost
    let mut touched: BTreeMap<String, Vec<Vec<Option<String>>>> = BTreeMap::new();
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        touched.entry(t.name.clone()).or_default().extend(rs.iter().map(|r| r.key_texts(t)));
    }
    let pending = json!({"merge": merge_id, "world": world, "world_id": world_id, "rows": touched, "others": []});
    crate::disk::write_file(&att.pending_path(), pending.to_string().as_bytes()).map_err(Error::Io)?;
    crash_point("before_commit");
    let outcome = session.commit_transaction().run();
    crash_point("after_commit");
    if let Err(e) = outcome {
        // (the driver tried the commit again itself; what it couldn't learn is asked of MongoDB from outside)
        let unknown = e.contains_label("UnknownTransactionCommitResult");
        match unknown.then(|| committed(&merge_id)) {
            Some(Ok(true)) => {}
            Some(Err(why)) => {
                return Err(refused(format!(
                    "the connection to MongoDB was lost during the commit ({}), and whether it committed can't be \
                     told yet ({why}): merges into main wait until it can; {world} is as it was",
                    plain(&e)
                )));
            }
            _ => {
                let _ = std::fs::remove_file(att.pending_path());
                return Err(failed("at the commit", &e));
            }
        }
    }
    super::undo::merged(&merge_id);
    if keeps_undo {
        // (records past the undo window go)
        let cutoff = bson::DateTime::from_millis(crate::now_ms() as i64 - super::window_days() as i64 * 86_400_000);
        let _ = db.collection::<Document>(MERGE_ROWS).delete_many(doc! { "at": { "$lt": cutoff } }).run();
    }
    let _ = client;
    AFTER.set(Some(After { rows: sent_back, committed: true, refresh: vec![] }));
    Ok(None)
}

/// Whether the write-back that inserted `merge_id` committed (see `super::committed`): its document inserted again
/// from outside waits for the first transaction to end, then is a duplicate if it committed.
pub(super) fn committed(merge_id: &str) -> std::result::Result<bool, String> {
    let (_, db) = client()?;
    let coll = db.collection::<Document>(MERGES);
    match coll.insert_one(doc! { "_id": merge_id, "probe": true }).run() {
        Ok(_) => {
            let _ = coll.delete_one(doc! { "_id": merge_id, "probe": true }).run();
            Ok(false)
        }
        Err(e) if plain(&e).contains("E11000") => {
            // (its own probe left by a crash between the two statements above isn't a commit)
            let probe = coll.count_documents(doc! { "_id": merge_id, "probe": true }).run().map_err(|e| plain(&e))?;
            if probe == 1 {
                let _ = coll.delete_one(doc! { "_id": merge_id, "probe": true }).run();
            }
            Ok(probe == 0)
        }
        Err(e) => Err(plain(&e)),
    }
}

/// The rows `touched` names (table name: keys), as MongoDB has them now: for `resolve`.
pub(super) fn reread(
    att: &Attached,
    touched: &serde_json::Map<String, Json>,
) -> std::result::Result<Vec<Stored>, String> {
    let (client, db) = client()?;
    let mut session = client.start_session().run().map_err(|e| plain(&e))?;
    let tables = att.tables();
    let mut out = vec![];
    for (name, keys) in touched {
        let Some(ti) = att.table(name) else { continue };
        let t = &tables[ti];
        let keys: Vec<Vec<Option<String>>> = serde_json::from_value(keys.clone()).unwrap_or_default();
        let ids: Vec<String> = keys.iter().filter_map(|k| k.first().cloned().flatten()).collect();
        let found = by_ids(&db, &mut session, t, &ids).map_err(|e| plain(&e))?;
        for id in ids {
            let row = found.get(&id).map(|d| row_of(t, d.clone()));
            out.push(Stored { table: ti, key: vec![Some(id)], row });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- undo records

/// Whether write-backs keep undo records: the collection is there (made now, if the user may).
pub(super) fn undo_ready() -> bool {
    let Ok((_, db)) = client() else { return false };
    let _ = db.create_collection(MERGE_ROWS).run();
    let coll = db.collection::<Document>(MERGE_ROWS);
    let _ = coll.create_index(mongodb::IndexModel::builder().keys(doc! { "merge_id": 1, "n": 1 }).build()).run();
    // (a document written and taken back: whether this user may)
    let probe = doc! { "merge_id": "probe", "n": -1, "table_name": "", "op": "", "at": bson::DateTime::now() };
    match coll.insert_one(probe).run() {
        Ok(_) => coll.delete_many(doc! { "merge_id": "probe" }).run().is_ok(),
        Err(_) => false,
    }
}

/// The records of merge `id` as MongoDB holds them, in the order it wrote them, and when it was (ms since 1970).
pub(super) fn undo_read(id: &str) -> std::result::Result<(u64, Vec<super::undo::Raw>), String> {
    let (_, db) = client()?;
    let coll = db.collection::<Document>(MERGE_ROWS);
    let cursor = coll.find(doc! { "merge_id": id }).sort(doc! { "n": 1 }).run().map_err(|e| plain(&e))?;
    let mut at = 0;
    let mut out = vec![];
    for d in cursor {
        let d = d.map_err(|e| plain(&e))?;
        let s = |k: &str| d.get_str(k).ok().map(String::from);
        at = d.get_datetime("at").map_or(at, |t| t.timestamp_millis().max(0) as u64);
        out.push([s("table_name"), s("op"), s("key"), s("before"), s("after")]);
    }
    Ok((at, out))
}

// ---------------------------------------------------------------- reads checked, and the dry run

/// `super::Live`, of MongoDB in `session`'s transaction or snapshot: documents compared whole (see `same_row`), and
/// a collection read more broadly than by `_id` read whole, for the world's tenant when it read for one and the
/// tenant's field is kept as a column.
struct MoLive<'a> {
    db: &'a Database,
    session: &'a mut ClientSession,
}

impl super::Live for MoLive<'_> {
    fn name(&self) -> &'static str {
        "MongoDB"
    }

    /// (Filters aren't written as MongoDB queries yet: a collection read by a filtered scan is read whole.)
    fn narrow(&self, _filters: &[crate::sql::RowFilter], _types: &[(String, String)]) -> Option<String> {
        None
    }

    /// The tenant's documents, as a query in Extended JSON (`{"org_id": 42}`): only where the tenant's column is a
    /// field kept as a column of the collection itself. (MongoDB compares numbers by value whatever their type.)
    fn tenant_cond(
        &self,
        v: &crate::View,
        of: &mut crate::sql::Tenants,
        att: &Attached,
        name: &str,
        tenant: &Json,
    ) -> Result<Option<String>> {
        let tables = att.tables();
        let Some(t) = att.table(name).map(|i| &tables[i]) else { return Ok(None) };
        let reach = of.reach(v, name).map_err(|e| Error::Constraint { code: e.code, message: e.message })?;
        if !matches!(reach, Some(crate::sql::TenantReach::Column)) {
            return Ok(None);
        }
        let col = tenant["column"].as_str().unwrap_or_default();
        let Some(c) = lifted(t).map(|i| &t.cols[i]).find(|c| c.name == col) else { return Ok(None) };
        let text = match &tenant["value"] {
            Json::Number(n) => n.to_string(),
            Json::String(s) => s.clone(),
            Json::Bool(b) => b.to_string(),
            _ => return Ok(None),
        };
        let value = match c.ty.as_str() {
            TY_INT | TY_LONG => match text.trim().parse::<i64>() {
                Ok(n) => json!(n),
                Err(_) => return Ok(None),
            },
            TY_OID => json!({ "$oid": text }),
            _ => json!(text),
        };
        Ok(Some(json!({ col: value }).to_string()))
    }

    fn changed(&mut self, t: &Table, rs: &[&Row]) -> Result<Vec<(usize, Option<Texts>)>> {
        let keys: Vec<String> = rs.iter().map(|r| r.key_texts(t)[0].clone().unwrap_or_default()).collect();
        let found = by_ids(self.db, self.session, t, &keys).map_err(|e| failed(&format!("reading {}", t.name), &e))?;
        let mut out = vec![];
        for (at, (r, key)) in rs.iter().zip(&keys).enumerate() {
            let theirs = found.get(key).map(|d| row_of(t, d.clone()));
            let same = match (&r.before, &theirs) {
                (Some(b), Some(n)) => same_row(t, b, n),
                (None, None) => true,
                _ => false,
            };
            if !same {
                out.push((at, theirs));
            }
        }
        Ok(out)
    }

    fn added(&mut self, t: &Table, rs: &[&Row], only: &[String]) -> Result<Vec<Texts>> {
        let known: HashSet<String> = rs.iter().map(|r| r.key_texts(t)[0].clone().unwrap_or_default()).collect();
        let mut all = vec![];
        for cond in only {
            let j: Json = serde_json::from_str(cond).map_err(|e| refused(format!("{}: {e}", t.name)))?;
            all.push(to_bson(&j, None).map_err(|e| refused(format!("{}: {e}", t.name)))?);
        }
        let filter = match all.is_empty() {
            true => doc! {},
            false => doc! { "$and": all },
        };
        let what = format!("reading documents of {} the copy hasn't", t.name);
        // (one more than could be told apart from too many, counting those the fork point has)
        let limit = (super::LIVE_ROWS + known.len() + 1) as i64;
        let mut cursor = self
            .db
            .collection::<Document>(&t.pg)
            .find(filter)
            .limit(limit)
            .session(&mut *self.session)
            .run()
            .map_err(|e| failed(&what, &e))?;
        let mut out = vec![];
        for d in cursor.iter(&mut *self.session) {
            let d = d.map_err(|e| failed(&what, &e))?;
            if !known.contains(&id_text(d.get("_id").unwrap_or(&Bson::Null))) {
                out.push(row_of(t, d));
            }
        }
        Ok(out)
    }
}

/// A session whose reads are all of one moment (a snapshot), after checking the server is the one attached.
fn snapshot(att: &Attached) -> std::result::Result<(Database, ClientSession), String> {
    let (client, db) = client()?;
    let marker = db
        .collection::<Document>(MERGES)
        .count_documents(doc! { "_id": att.marker() })
        .run()
        .map_err(|e| format!("MongoDB, reading chronos_merges: {}", plain(&e)))?;
    if marker != 1 {
        return Err(format!(
            "the MongoDB CHRONOS_SOURCE_URL names isn't the one this folder was attached to ({}): its chronos_merges \
             has no document {}",
            att.source(),
            att.marker()
        ));
    }
    let session = client.start_session().snapshot(true).run().map_err(|e| format!("MongoDB: {}", plain(&e)))?;
    Ok((db, session))
}

/// `super::check_reads`, against MongoDB: what the world read, checked as the write-back checks it, in one snapshot,
/// outside any merge.
pub(super) fn check_reads(core: &Core, att: &Attached, r: &Reads) -> std::result::Result<Vec<String>, String> {
    let (db, mut session) = snapshot(att)?;
    super::live_reads(core, &mut MoLive { db: &db, session: &mut session }, att, r).map_err(|e| e.to_string())
}

/// `super::dry_run`'s read of MongoDB: one snapshot, nothing locked or written. Each document the merge would write
/// compared with MongoDB's as the write-back compares it, and the world's reads checked.
pub(super) fn dry_run(
    core: &Core,
    att: &Attached,
    rows: &[Row],
    reads: Option<&Reads>,
    live: &mut crate::LiveCheck,
) -> std::result::Result<(), String> {
    let (db, mut session) = snapshot(att)?;
    live.at = crate::now_ms();
    let mut src = MoLive { db: &db, session: &mut session };
    if let Some(r) = reads {
        live.stale_reads = super::live_reads(core, &mut src, att, r)
            .map_err(|e| e.to_string())?
            .iter()
            .map(|w| crate::agents::for_caller(w))
            .collect();
    }
    let tables = att.tables();
    for (&ti, rs) in &by_table(rows) {
        let t = &tables[ti];
        let changed: HashMap<usize, Option<Texts>> =
            super::Live::changed(&mut src, t, rs).map_err(|e| e.to_string())?.into_iter().collect();
        for (at, r) in rs.iter().enumerate() {
            live.rows.push(crate::LiveRow {
                key: r.key.clone(),
                changed: changed.contains_key(&at),
                // (a document as main has it is MongoDB's, by the comparison)
                postgres: match changed.get(&at) {
                    Some(now) => now.as_ref().map(|n| row_json(t, n)),
                    None => r.before.as_ref().map(|b| row_json(t, b)),
                },
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- refresh

/// `super::refresh` of a MongoDB folder: each collection read whole in one snapshot, compared with main's rows, main
/// taking the differences in one change.
pub(super) fn refresh(core: &Core, att: &Attached, only: Option<&[usize]>) -> std::result::Result<Refreshed, String> {
    let (client, db) = client()?;
    let mut session = client.start_session().snapshot(true).run().map_err(|e| plain(&e))?;
    let marker = db
        .collection::<Document>(MERGES)
        .count_documents(doc! { "_id": att.marker() })
        .run()
        .map_err(|e| format!("MongoDB, reading chronos_merges: {}", plain(&e)))?;
    if marker != 1 {
        return Err(format!(
            "the MongoDB CHRONOS_SOURCE_URL names isn't the one this folder was attached to ({}): nothing was refreshed",
            att.source()
        ));
    }
    let mut out = Refreshed { at: crate::now_ms(), ..Default::default() };
    let mut rows = vec![];
    let tables = att.tables();
    let main = core.view(MAIN).map_err(|e| e.to_string())?;
    for (ti, t) in tables.iter().enumerate() {
        if only.is_some_and(|o| !o.contains(&ti)) {
            continue;
        }
        let mut theirs: HashMap<String, Texts> = HashMap::new();
        let mut cursor = db
            .collection::<Document>(&t.pg)
            .find(doc! {})
            .session(&mut session)
            .run()
            .map_err(|e| format!("MongoDB, reading {}: {}", t.name, plain(&e)))?;
        for d in cursor.iter(&mut session) {
            let row = row_of(t, d.map_err(|e| format!("MongoDB, reading {}: {}", t.name, plain(&e)))?);
            theirs.insert(row[0].clone().unwrap_or_default(), row);
        }
        let (mut ins, mut upd, mut del) = (0, 0, 0);
        let prefix = format!("{}/", t.name);
        for r in core.rows(&main.s, &prefix).map_err(|e| e.to_string())? {
            let (k, raw) = r.map_err(|e| e.to_string())?;
            if !k.starts_with(&prefix) {
                break;
            }
            let ours = texts(core, &main.s, t, &k, &raw).map_err(|e| e.to_string())?;
            let id = ours[0].clone().unwrap_or_default();
            match theirs.remove(&id) {
                None => {
                    del += 1;
                    rows.push(Stored { table: ti, key: vec![Some(id)], row: None });
                }
                Some(n) if !same_row(t, &ours, &n) => {
                    upd += 1;
                    rows.push(Stored { table: ti, key: vec![Some(id)], row: Some(n) });
                }
                Some(_) => {}
            }
        }
        for (id, n) in theirs {
            ins += 1;
            rows.push(Stored { table: ti, key: vec![Some(id)], row: Some(n) });
        }
        out.tables.push((t.name.clone(), ins, upd, del));
    }
    store(core, att, &rows)?;
    if only.is_none() {
        att.refreshed(out.at).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

// ---------------------------------------------------------------- attach

/// `chronos attach <dir>` with a MongoDB CHRONOS_SOURCE_URL: collections of the URL's database, copied in one
/// snapshot into a new folder, then recorded (`chronos_merges`'s marker document and the folder's record). `fields`
/// are top-level fields kept as columns of their own (`--fields org_id`).
pub fn attach(
    dir: &str,
    opts: &crate::Open,
    named: &[String],
    fields: &[String],
    yes: bool,
) -> std::result::Result<(), String> {
    let path = Path::new(dir);
    if path.exists() {
        return Err(format!("{dir} exists: attach makes a new folder (to attach again, give another)"));
    }
    if named.is_empty() {
        return Err("name the collections to attach: --tables a,b (collections of the database the URL names)".into());
    }
    let (client, db) = client()?;
    let hello = db.run_command(doc! { "hello": 1 }).run().map_err(|e| format!("MongoDB: {}", plain(&e)))?;
    if hello.get_str("msg").ok() == Some("isdbgrid") {
        return Err("this is a sharded cluster (mongos): attach works with a replica set only for now".into());
    }
    if hello.get("setName").is_none() {
        return Err(
            "this MongoDB is a standalone server, which has no transactions: a merge couldn't be written all or \
             nothing. Attach needs a replica set (one member is enough: mongod --replSet rs0, then rs.initiate()), \
             or Atlas; nothing was copied"
                .into(),
        );
    }
    let build = db.run_command(doc! { "buildInfo": 1 }).run().map_err(|e| format!("MongoDB: {}", plain(&e)))?;
    let version = build.get_str("version").unwrap_or("0").to_string();
    let major: u32 = version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
    if major < 5 {
        return Err(format!("MongoDB {version} is too old: attach needs 5.0 or later (nothing was copied)"));
    }
    let specs: Vec<mongodb::results::CollectionSpecification> = db
        .list_collections()
        .run()
        .map_err(|e| format!("reading MongoDB: {}", plain(&e)))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| format!("reading MongoDB: {}", plain(&e)))?;
    let mut refused_why = vec![];
    let mut made: Vec<(Table, i64)> = vec![];
    let mut total = 0i64;
    for name in named {
        let Some(spec) = specs.iter().find(|s| &s.name == name) else {
            refused_why.push(format!("  - {name}: no such collection in {}", db.name()));
            continue;
        };
        match spec.collection_type {
            mongodb::results::CollectionType::Collection => {}
            mongodb::results::CollectionType::View => {
                refused_why.push(format!("  - {name}: a view: attach the collections it reads"));
                continue;
            }
            _ => {
                refused_why.push(format!("  - {name}: a time-series collection: not attached yet"));
                continue;
            }
        }
        if spec.options.capped == Some(true) {
            refused_why.push(format!("  - {name}: a capped collection: its documents can't be deleted or grow"));
            continue;
        }
        if name.starts_with("system.") || name == MERGES || name == MERGE_ROWS {
            refused_why.push(format!("  - {name}: not a collection of yours"));
            continue;
        }
        let rows = db.collection::<Document>(name).estimated_document_count().run().unwrap_or(0) as i64;
        total += rows;
        println!("{name}: about {rows} documents");
        let mut cols = vec![Col {
            name: "_id".into(),
            ty: "text".into(),
            always: false,
            generated: false,
            attnum: 1,
            fill: false,
            computed: false,
        }];
        for (i, f) in fields.iter().enumerate() {
            cols.push(Col {
                name: f.clone(),
                ty: String::new(),
                always: false,
                generated: false,
                attnum: i as i16 + 2,
                fill: false,
                computed: false,
            });
        }
        cols.push(Col {
            name: "doc".into(),
            ty: "jsonb".into(),
            always: false,
            generated: false,
            attnum: fields.len() as i16 + 2,
            fill: false,
            computed: false,
        });
        made.push((
            Table { name: name.clone(), sql: q(name), pg: name.clone(), cols, key: vec![0], seqs: vec![] },
            rows,
        ));
    }
    for f in fields {
        if f == "_id" || f == "doc" || f.contains('.') || f.starts_with('$') {
            refused_why.push(format!("  - --fields {f}: a top-level field's name, not _id or doc"));
        }
    }
    if !refused_why.is_empty() {
        return Err(format!("can't attach, nothing was copied:\n{}", refused_why.join("\n")));
    }
    if total > 1_000_000 && !yes {
        return Err(format!(
            "about {total} documents in all (MongoDB's estimate): copying holds one snapshot open on MongoDB for that \
             long. Run again with --yes to go on"
        ));
    }
    db.create_collection(MERGES)
        .run()
        .or_else(|e| if plain(&e).contains("already exists") { Ok(()) } else { Err(e) })
        .map_err(|e| {
            format!(
                "{} can't make the collection {MERGES} in MongoDB ({}): give it readWrite on the database",
                db.name(),
                plain(&e)
            )
        })?;
    // one snapshot of everything from here on
    let mut session =
        client.start_session().snapshot(true).run().map_err(|e| format!("starting the read: {}", plain(&e)))?;
    // the type each field kept as a column holds: one type for all the documents that have it
    for (t, _) in made.iter_mut() {
        for i in lifted(t) {
            let field = t.cols[i].name.clone();
            let types = db
                .collection::<Document>(&t.pg)
                .aggregate([doc! { "$group": { "_id": { "$type": format!("${field}") }, "n": { "$sum": 1 } } }])
                .session(&mut session)
                .run()
                .map_err(|e| format!("reading {}: {}", t.name, plain(&e)))?
                .iter(&mut session)
                .filter_map(|d| d.ok().and_then(|d| d.get_str("_id").ok().map(String::from)))
                .filter(|ty| ty != "missing" && ty != "null")
                .collect::<Vec<_>>();
            t.cols[i].ty = match types.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
                [] | ["string"] => TY_STRING.into(),
                ["int"] => TY_INT.into(),
                ["long"] | ["int", "long"] | ["long", "int"] => TY_LONG.into(),
                ["objectId"] => TY_OID.into(),
                other => {
                    return Err(format!(
                        "can't attach, nothing was copied:\n  - {}.{field}: holds {} in different documents; a field \
                         kept as a column (--fields) must be one of string, int, long or objectId throughout",
                        t.name,
                        other.join(" and ")
                    ));
                }
            };
        }
    }
    let odb = crate::Db::open_with(dir, opts).map_err(|e| format!("{dir}: {e}"))?;
    let done = (|| -> std::result::Result<Attached, String> {
        for (t, _) in &made {
            let mut defs = vec![];
            for i in lifted(t) {
                let ty = if matches!(t.cols[i].ty.as_str(), TY_INT | TY_LONG) { "bigint" } else { "text" };
                defs.push(format!("{} {ty}", q(&t.cols[i].name)));
            }
            let rest = if defs.is_empty() { String::new() } else { format!("{}, ", defs.join(", ")) };
            // a new document's _id here: an ObjectId's 24 hex digits, as MongoDB's drivers make one (time, then random)
            let id = "lpad(to_hex(extract(epoch from now())::bigint), 8, '0') || substr(md5(gen_random_uuid()::text), 1, 16)";
            let with = format!(
                "create table {} (\"_id\" text primary key default ({id}), {rest}\"doc\" jsonb not null default '{{}}')",
                t.sql
            );
            let without = format!(
                "create table {} (\"_id\" text primary key, {rest}\"doc\" jsonb not null default '{{}}')",
                t.sql
            );
            if crate::sql::run(&odb, MAIN, &with, &[]).is_err() {
                crate::sql::run(&odb, MAIN, &without, &[])
                    .map_err(|e| format!("making {} here: {}", t.name, e.message))?;
            }
            let mut batch: Vec<String> = vec![];
            let mut n = 0u64;
            let flush = |batch: &mut Vec<String>| -> std::result::Result<(), String> {
                if batch.is_empty() {
                    return Ok(());
                }
                let sql = format!("insert into {} values {}", t.sql, batch.join(", "));
                batch.clear();
                crate::sql::run(&odb, MAIN, &sql, &[])
                    .map(|_| ())
                    .map_err(|e| format!("copying {}: {}", t.name, e.message))
            };
            let mut cursor = db
                .collection::<Document>(&t.pg)
                .find(doc! {})
                .session(&mut session)
                .run()
                .map_err(|e| format!("reading {} from MongoDB: {}", t.name, plain(&e)))?;
            let mut seen: HashSet<String> = HashSet::new();
            for d in cursor.iter(&mut session) {
                let row = row_of(t, d.map_err(|e| format!("reading {} from MongoDB: {}", t.name, plain(&e)))?);
                let id = row[0].clone().unwrap_or_default();
                if row.last().and_then(|d| d.as_ref()).is_some_and(|d| d.len() > MAX_DOC) {
                    return Err(format!("{}: the document {id} is over {} MB: not attached", t.name, MAX_DOC >> 20));
                }
                if !seen.insert(id.clone()) {
                    return Err(format!(
                        "{}: two documents share the key {id} as it is written here: not attached",
                        t.name
                    ));
                }
                let vals: Vec<String> = row.iter().map(|v| v.as_deref().map_or("null".into(), super::lit)).collect();
                batch.push(format!("({})", vals.join(", ")));
                n += 1;
                if batch.len() >= 500 {
                    flush(&mut batch)?;
                }
            }
            flush(&mut batch)?;
            println!("{}: {n} documents", t.name);
        }
        let (host, port) = host_of(&super::source_text()?);
        let att = Attached::new_mongo(path, &host, port, db.name(), made.into_iter().map(|(t, _)| t).collect());
        db.collection::<Document>(MERGES)
            .insert_one(doc! { "_id": att.marker(), "at": bson::DateTime::now() })
            .run()
            .map_err(|e| format!("recording the attach in {MERGES}: {}", plain(&e)))?;
        att.save().map_err(|e| e.to_string())?;
        Ok(att)
    })();
    drop(odb);
    let att = match done {
        Ok(a) => a,
        Err(e) => {
            let _ = std::fs::remove_dir_all(path);
            return Err(e);
        }
    };
    println!("\nattached {} collections of {} (MongoDB {version}) to {dir}", att.tables().len(), att.source());
    println!(
        "\nnext: chronos serve {dir} (with CHRONOS_SOURCE_URL set), fork a world, change documents there (each is a \
         row: _id, and doc, its fields as jsonb), and MERGE WORLD: the change goes to MongoDB first. Not yet with \
         MongoDB: serve --follow, rehearsals, merge checks, writeback.json and pg_role."
    );
    Ok(())
}

/// The first host and port a MongoDB URL names, for the folder's record (what `source()` says).
fn host_of(url: &str) -> (String, u16) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    let first = rest.split(['/', ',', '?']).next().unwrap_or_default();
    match first.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(27017)),
        None => (first.to_string(), 27017),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let oid = Bson::ObjectId(ObjectId::parse_str("65f1a2b3c4d5e6f708192a3b").unwrap());
        assert_eq!(id_text(&oid), "65f1a2b3c4d5e6f708192a3b");
        assert_eq!(id_bson("65f1a2b3c4d5e6f708192a3b").unwrap(), oid);
        assert_eq!(id_text(&Bson::String("abc".into())), "\"abc\"");
        assert_eq!(id_bson("\"abc\"").unwrap(), Bson::String("abc".into()));
        assert_eq!(id_bson(&id_text(&Bson::Int32(7))).unwrap(), Bson::Int32(7));
        assert!(id_bson("abc").is_err());
    }

    #[test]
    fn numbers_keep_the_type_of_the_field_they_replace() {
        let n = |s: &str| serde_json::from_str::<Json>(s).unwrap();
        assert_eq!(to_bson(&n("5"), Some(&Bson::Double(1.5))).unwrap(), Bson::Double(5.0));
        assert_eq!(to_bson(&n("5"), Some(&Bson::Int64(1))).unwrap(), Bson::Int64(5));
        assert_eq!(to_bson(&n("5"), Some(&Bson::Int32(1))).unwrap(), Bson::Int32(5));
        assert_eq!(to_bson(&n("5"), None).unwrap(), Bson::Int32(5));
        assert_eq!(to_bson(&n("5000000000"), None).unwrap(), Bson::Int64(5_000_000_000));
        assert_eq!(to_bson(&n("5.5"), Some(&Bson::Int32(1))).unwrap(), Bson::Double(5.5));
        assert_eq!(to_bson(&n("5.0"), None).unwrap(), Bson::Double(5.0));
        assert!(matches!(to_bson(&n(r#"{"$oid": "65f1a2b3c4d5e6f708192a3b"}"#), None).unwrap(), Bson::ObjectId(_)));
        assert!(matches!(to_bson(&n(r#"{"$date": "2026-10-01T10:00:00Z"}"#), None).unwrap(), Bson::DateTime(_)));
    }

    #[test]
    fn an_update_names_only_the_fields_that_changed() {
        let j = |s: &str| serde_json::from_str::<Json>(s).unwrap();
        let (mut sets, mut unsets) = (vec![], vec![]);
        diff(
            "",
            &j(r#"{"a": 1, "b": {"x": 1, "y": 2}, "tags": [1, 2], "gone": true, "same": {"k": 1.0}}"#),
            &j(r#"{"a": 1, "b": {"x": 1, "y": 3}, "tags": [1, 2, 3], "new": "n", "same": {"k": 1}}"#),
            &mut sets,
            &mut unsets,
        );
        sets.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(sets, [("b.y".into(), j("3")), ("new".into(), j("\"n\"")), ("tags".into(), j("[1, 2, 3]"))]);
        assert_eq!(unsets, ["gone"]);
    }

    #[test]
    fn documents_compare_by_value() {
        let j = |s: &str| serde_json::from_str::<Json>(s).unwrap();
        assert!(same_json(&j(r#"{"a": 1, "b": [1.0, 2]}"#), &j(r#"{"b": [1, 2.0], "a": 1.0}"#)));
        assert!(!same_json(&j(r#"{"a": 1}"#), &j(r#"{"a": 1, "b": null}"#)));
        assert!(!same_json(&j("[1, 2]"), &j("[2, 1]")));
    }
}
