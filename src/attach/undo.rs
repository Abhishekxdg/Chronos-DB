//! Undo of a merge written to the source, by the merge's id.
//!
//! A write-back keeps, in the source and in its own transaction, each row it wrote: the row as main had it (which it
//! checked the source had too, under lock) and the row as the source stored it. `chronos.merge_rows` in Postgres,
//! `chronos_merge_rows` in MySQL: they commit or roll back with the write itself, outlive the folder, and are the
//! customer's own values in the customer's own database. They are kept for `window_days`, apart from the folder's
//! history (which holds every page of main, and is kept an hour).
//!
//! `Core::undo_merges` reads them back, forks a world from main, puts each row there as it was before the merge, and
//! merges that world as any world is merged: every row locked and compared in the source, in one transaction. A row
//! main no longer holds as the merge left it was changed since: named, with nothing undone, or left alone with
//! `skip_changed`. An undo is a merge too, with an id and records of its own.
//!
//! Where the table isn't there (a folder attached before it was, and a role that can't make it), merges go on
//! without records and say so in `attached`; `setup_statements` has what an administrator runs.

use super::*;

/// Postgres's table of the rows each merge wrote.
const ROWS: &str = "chronos.merge_rows";
/// Records deleted after one write-back, at most.
const PRUNE: usize = 5000;

thread_local! {
    /// This thread merges an undo: rows go back under the ids they had (see `check_ids`).
    static RESTORING: Cell<bool> = const { Cell::new(false) };
    /// The id of the merge this thread last wrote to the source.
    static LAST: RefCell<Option<String>> = const { RefCell::new(None) };
    /// This thread's undo may write rows besides its own (the caller said ALLOW CASCADE).
    static ALLOW: Cell<bool> = const { Cell::new(false) };
    /// This thread's merge is an undo's dry run: written to the source, then rolled back before COMMIT.
    static PROBE: Cell<bool> = const { Cell::new(false) };
    /// What the dry run's write-back found: the tables written besides the undo's own.
    static PROBED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    /// The effects (outbox entries: id, channel) this thread's merge brings into main, for its records.
    static EFFECTS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

/// What refuses an undo that would write rows besides its own.
pub const CASCADE: &str = "2BP01";

/// Says which effects the merge about to be written brings into main (kept with its records: an undo lists them).
pub(super) fn effects(apply: &[crate::Change]) {
    let prefix = format!("{}/", crate::outbox::OUTBOX);
    let found = apply
        .iter()
        .filter(|c| c.before.is_none())
        .filter_map(|c| {
            let id = c.key.strip_prefix(&prefix)?;
            Some((id.to_string(), crate::outbox::channel_of(c.after.as_deref()?).unwrap_or_default()))
        })
        .collect();
    EFFECTS.set(found);
}

/// Called by a write-back once its writes are in and before anything is recorded or committed, with the tables the
/// source wrote besides the merge's own rows (`others`: a cascade, a trigger). An undo's dry run stops here, to be
/// rolled back, having learned them. An undo that would write such rows is refused unless told ALLOW CASCADE: rows
/// other people added under a row it removes would go with it.
pub(super) fn before_commit(besides: &[String]) -> Result<()> {
    if PROBE.get() {
        PROBED.set(Some(besides.to_vec()));
        return Err(refused("an undo's dry run: rolled back, nothing written"));
    }
    if RESTORING.get() && !besides.is_empty() && !ALLOW.get() {
        return Err(Error::Constraint {
            code: CASCADE,
            message: format!(
                "the undo would also change rows of {} that the merge didn't write (a cascade or a trigger in the \
                 database: rows added under a row the undo removes go with it). A dry run shows this; ALLOW CASCADE \
                 undoes anyway; nothing was undone",
                besides.join(", ")
            ),
        });
    }
    Ok(())
}

/// Whether this thread's merge puts rows back as they were (an undo): a row's sequence-fed column takes the value it
/// had, which the source gave it once.
pub(super) fn restoring() -> bool {
    RESTORING.get()
}

/// Says the write-back `id` committed.
pub(super) fn merged(id: &str) {
    LAST.set(Some(id.to_string()));
}

/// The id of the merge this thread last wrote to the source, once: what `undo_merges` takes.
pub fn take_merge_id() -> Option<String> {
    LAST.take()
}

/// How long a merge's rows are kept to undo it by: CHRONOS_UNDO_WINDOW_DAYS, 30 days when unset.
pub fn window_days() -> u64 {
    std::env::var("CHRONOS_UNDO_WINDOW_DAYS").ok().and_then(|v| v.trim().parse().ok()).filter(|d| *d > 0).unwrap_or(30)
}

/// What makes Postgres's table (also in `setup_statements`).
pub(super) const CREATE: &str = "create table if not exists chronos.merge_rows (merge_id text not null, n int not null, \
     table_name text not null, op text not null, key jsonb, before jsonb, after jsonb, at timestamptz not null \
     default now(), primary key (merge_id, n)); \
     create index if not exists merge_rows_at on chronos.merge_rows (at)";

/// Whether write-backs of the folder at `dir` keep undo records: the table is there and the role may insert into it.
/// Asked of the source once it says yes, and again a minute after a no (an administrator may have run the setup).
pub(super) fn ready(att: &Attached) -> bool {
    static KNOWN: Mutex<Option<HashMap<PathBuf, (bool, u64)>>> = Mutex::new(None);
    let now = crate::now_ms();
    let mut known = KNOWN.lock().unwrap_or_else(|p| p.into_inner());
    let known = known.get_or_insert_with(HashMap::new);
    match known.get(&att.dir) {
        Some(&(true, _)) => return true,
        Some(&(false, at)) if now.saturating_sub(at) < 60_000 => return false,
        _ => {}
    }
    let yes = match att.kind() {
        SourceKind::MySql | SourceKind::MariaDb => mysql::undo_ready(),
        #[cfg(feature = "mongo")]
        SourceKind::Mongo => mongo::undo_ready(),
        #[cfg(not(feature = "mongo"))]
        SourceKind::Mongo => false,
        SourceKind::Postgres => pg_ready(),
    };
    known.insert(att.dir.clone(), (yes, now));
    yes
}

fn pg_ready() -> bool {
    let Ok(mut pg) = connect() else { return false };
    let _ = pg.query(CREATE); // (a role that can't: an administrator runs `setup_statements`)
    if pg.status != b'I' {
        let _ = pg.query("rollback");
    }
    pg.query(&format!("select has_table_privilege('{ROWS}', 'insert')"))
        .ok()
        .and_then(|mut r| r.pop())
        .and_then(|r| r.first().cloned().flatten())
        .is_some_and(|v| v == "t")
}

/// A row's columns by name, as text.
fn named(t: &Table, r: &[Option<String>]) -> Json {
    let o: serde_json::Map<String, Json> =
        t.cols.iter().zip(r).map(|(c, v)| (c.name.clone(), v.clone().map_or(Json::Null, Json::String))).collect();
    Json::Object(o)
}

/// The records of a write-back: per row, in order, its table, operation, key, and the row before and after.
/// `landed` says the row as the source stored it (None: as the merge sent it).
pub(super) fn records(tables: &[Table], rows: &[Row], landed: &dyn Fn(&Row) -> Option<Texts>) -> Vec<Json> {
    let mut out = vec![];
    for r in rows {
        let t = &tables[r.table];
        let after = r.after.as_ref().map(|a| landed(r).unwrap_or_else(|| a.clone()));
        let op = match (&r.before, &after) {
            (None, _) => "insert",
            (_, None) => "delete",
            _ => "update",
        };
        let of = after.as_ref().or(r.before.as_ref()).expect("a row on one side at least");
        let key: serde_json::Map<String, Json> =
            t.key.iter().map(|&i| (t.cols[i].name.clone(), of[i].clone().map_or(Json::Null, Json::String))).collect();
        out.push(json!({
            "n": out.len(), "table": t.name, "op": op, "key": key,
            "before": r.before.as_ref().map_or(Json::Null, |b| named(t, b)),
            "after": after.as_ref().map_or(Json::Null, |a| named(t, a)),
        }));
    }
    // (and the effects it brings into main: not rows of the source's, kept so an undo can say they aren't undone)
    for (id, channel) in EFFECTS.take() {
        out.push(json!({
            "n": out.len(), "table": "", "op": "effect", "key": {"id": id, "channel": channel},
            "before": Json::Null, "after": Json::Null,
        }));
    }
    out
}

/// Writes a write-back's records in its transaction (Postgres).
pub(super) fn pg_record(
    pg: &mut Client,
    tables: &[Table],
    rows: &[Row],
    sent_back: &[Stored],
    merge_id: &str,
) -> Result<()> {
    let stored: HashMap<(usize, &[Option<String>]), &Texts> =
        sent_back.iter().filter_map(|s| Some(((s.table, s.key.as_slice()), s.row.as_ref()?))).collect();
    let landed = |r: &Row| {
        let a = r.after.as_ref()?;
        let key: Vec<Option<String>> = tables[r.table].key.iter().map(|&i| a[i].clone()).collect();
        stored.get(&(r.table, key.as_slice())).map(|t| (*t).clone())
    };
    let sql = format!(
        "insert into {ROWS} (merge_id, n, table_name, op, key, before, after) select $1, (x->>'n')::int, x->>'table', \
         x->>'op', x->'key', x->'before', x->'after' from jsonb_array_elements($2::jsonb) x"
    );
    for part in records(tables, rows, &landed).chunks(CHUNK) {
        let params = [merge_id.to_string(), Json::Array(part.to_vec()).to_string()];
        run(pg, "recording the merge's rows for undo", &sql, &params)?;
    }
    Ok(())
}

/// Drops records older than the window, a batch at a time, after a write-back committed. A role that may not delete
/// them leaves them: an administrator's to prune.
pub(super) fn pg_prune(pg: &mut Client) {
    let sql = format!(
        "delete from {ROWS} where ctid in (select ctid from {ROWS} where at < now() - interval '{} days' limit {PRUNE})",
        window_days()
    );
    let _ = pg.query(&sql);
    if pg.status != b'I' {
        let _ = pg.query("rollback");
    }
}

/// One row a merge wrote, read back from the source's records.
#[derive(Debug, Clone)]
pub struct Record {
    pub merge: String,
    pub table: usize,
    pub op: String,
    /// by position in the table's `cols`
    pub before: Option<Texts>,
    pub after: Option<Texts>,
}

/// A record's row (`{column: text}`) in the table's column order. Err: a column the table hasn't any more.
fn row_of(t: &Table, j: &Json) -> std::result::Result<Option<Texts>, String> {
    let Some(o) = j.as_object() else { return Ok(None) };
    t.cols
        .iter()
        .map(|c| match o.get(&c.name) {
            Some(Json::Null) => Ok(None),
            Some(Json::String(s)) => Ok(Some(s.clone())),
            Some(v) => Ok(Some(v.to_string())),
            None => Err(format!(
                "table {} has a column {} the merge's record hasn't: its columns changed since, and the merge can't \
                 be undone here",
                t.name, c.name
            )),
        })
        .collect::<std::result::Result<Texts, String>>()
        .map(Some)
}

/// A record from the source's row of it: (table name, operation, before, after), the last two JSON as text.
pub(super) fn record(
    att: &Attached,
    merge: &str,
    table: &str,
    op: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> std::result::Result<Record, String> {
    let tables = att.tables();
    let ti = att.table(table).ok_or_else(|| format!("merge {merge} wrote table {table}, which isn't attached now"))?;
    let parse = |v: Option<&str>| v.and_then(|v| serde_json::from_str::<Json>(v).ok()).unwrap_or(Json::Null);
    Ok(Record {
        merge: merge.to_string(),
        table: ti,
        op: op.to_string(),
        before: row_of(&tables[ti], &parse(before))?,
        after: row_of(&tables[ti], &parse(after))?,
    })
}

/// A record as the source holds it: table name, operation, key, before, after (the last three JSON as text).
pub(super) type Raw = [Option<String>; 5];

/// What merge `id` recorded: when it was (ms since 1970), its rows in the order it wrote them, and the effects (id,
/// channel) it brought into main.
#[allow(clippy::type_complexity)]
fn read(att: &Attached, id: &str) -> std::result::Result<(u64, Vec<Record>, Vec<(String, String)>), String> {
    let (at, raw): (u64, Vec<Raw>) = match att.kind() {
        SourceKind::MySql | SourceKind::MariaDb => mysql::undo_read(id)?,
        #[cfg(feature = "mongo")]
        SourceKind::Mongo => mongo::undo_read(id)?,
        #[cfg(not(feature = "mongo"))]
        SourceKind::Mongo => return Err("this chronos was built without MongoDB".into()),
        SourceKind::Postgres => {
            let mut pg = connect()?;
            let rows = pg
                .query_params(
                    &format!(
                        "select table_name, op, key::text, before::text, after::text, (extract(epoch from at) * \
                         1000)::bigint::text from {ROWS} where merge_id = $1 order by n"
                    ),
                    &[Some(id)],
                )
                .map_err(|e| format!("reading {ROWS}: {e}"))?;
            let at = rows.first().and_then(|r| r[5].as_deref()).and_then(|v| v.parse().ok()).unwrap_or(0);
            (
                at,
                rows.into_iter()
                    .map(|r| [r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone(), r[4].clone()])
                    .collect(),
            )
        }
    };
    let (mut recs, mut effects) = (vec![], vec![]);
    for r in &raw {
        let s = |i: usize| r[i].as_deref();
        if s(1) == Some("effect") {
            let key: Json = s(2).and_then(|k| serde_json::from_str(k).ok()).unwrap_or(Json::Null);
            let text = |k: &str| key[k].as_str().unwrap_or_default().to_string();
            effects.push((text("id"), text("channel")));
            continue;
        }
        recs.push(record(att, id, s(0).unwrap_or_default(), s(1).unwrap_or_default(), s(3), s(4))?);
    }
    Ok((at, recs, effects))
}

/// One row of an undo: what the merge did to it, and what the undo does.
#[derive(Debug, Clone)]
pub struct UndoRow {
    pub merge: String,
    pub table: String,
    /// what the merge did: `insert`, `update` or `delete`
    pub op: String,
    pub key: Json,
    /// the row before the merge, and as the merge left it (`{column: text}`; null: no row)
    pub before: Json,
    pub after: Json,
    /// `put_back`; `changed`: main no longer holds it as the merge left it, so it stays as it is; or `already`:
    /// main holds it as it was before the merge (an earlier undo put it back, or someone did), so there is nothing
    /// to undo
    pub status: &'static str,
    /// a changed row as main holds it now (null: no row)
    pub now: Json,
}

/// What `Core::undo_merges` did, or (a dry run) would do.
#[derive(Debug, Clone, Default)]
pub struct UndoneMerges {
    pub rows: Vec<UndoRow>,
    pub undone: usize,
    pub skipped: usize,
    /// rows main holds as they were before the merge already: neither undone nor skipped
    pub already: usize,
    /// the undo's own merge id (None: a dry run, or nothing to put back)
    pub merge: Option<String>,
    pub dry_run: bool,
    /// a dry run's: the tables the source would write rows of besides the undo's own (a cascade, a trigger). None:
    /// the source wasn't asked (nothing to put back), or can't say (MySQL)
    pub cascades: Option<Vec<String>>,
    /// the effects the merges brought into main: never undone (an email sent stays sent), listed with where each is
    pub effects: Vec<crate::UndoneEffect>,
}

/// How `Core::undo_merges` goes.
#[derive(Debug, Clone, Copy, Default)]
pub struct UndoOptions {
    /// leave rows changed since their merge, and undo the rest
    pub skip_changed: bool,
    /// write nothing: say what each row's undo would be, and what else the source would write
    pub dry_run: bool,
    /// undo though the source writes rows besides the undo's own
    pub allow_cascade: bool,
}

fn sql_err(e: crate::sql::SqlError) -> Error {
    Error::Constraint { code: e.code, message: e.message }
}

/// `where` for the row of `t` whose key is in `row`.
fn by_key(t: &Table, row: &[Option<String>]) -> String {
    let conds: Vec<String> = t
        .key
        .iter()
        .map(|&i| match &row[i] {
            Some(v) => format!("{} = {}", q(&t.cols[i].name), lit(v)),
            None => format!("{} is null", q(&t.cols[i].name)),
        })
        .collect();
    conds.join(" and ")
}

/// `t`'s columns an undo compares and writes: not those the database works out itself.
fn written(t: &Table) -> Vec<usize> {
    plain(t).into_iter().filter(|&i| !t.cols[i].computed).collect()
}

impl Core {
    /// Undoes the merges `ids` (as the merge answered them, `merge:...`) of a folder attached to a database: every row
    /// they wrote goes back as it was before, through the source as any merge does, in one transaction. The newest
    /// merge is undone first. A row main no longer holds as its merge left it was changed since: with `skip_changed`
    /// it is left and the rest undone, otherwise all are named in a 40001 and nothing is undone. A dry run does
    /// neither: it says what each row's undo would be.
    ///
    /// The source may write rows besides the undo's own (a foreign key's cascade under a row the undo removes, a
    /// trigger): that refuses the undo (`CASCADE`) unless `allow_cascade`, and a dry run names the tables, having
    /// run the undo's writes in the source and rolled them back. (Postgres counts them; MySQL can't say.) The effects
    /// the merges brought into main aren't undone: each is listed with where it is now, and those still delayed are
    /// taken back unsent.
    pub fn undo_merges(&self, ids: &[String], o: UndoOptions) -> Result<UndoneMerges> {
        let UndoOptions { skip_changed, dry_run, allow_cascade } = o;
        let Some(att) = self.attached.as_deref() else {
            return Err(refused("undo by merge id is for a folder attached to a database (chronos attach)"));
        };
        self.may_undo_into(MAIN)?;
        if ids.is_empty() {
            return Err(refused("name the merges to undo (the ids their merges answered)"));
        }
        let mut merges = vec![];
        let mut effects = vec![];
        for id in ids {
            let (at, recs, with) = read(att, id).map_err(refused)?;
            effects.extend(with);
            if recs.is_empty() {
                return Err(refused(format!(
                    "no undo record of merge {id}: it is older than the undo window ({} days), was made before \
                     records were kept, or isn't a merge of this database",
                    window_days()
                )));
            }
            merges.push((at, recs));
        }
        merges.sort_by_key(|m| std::cmp::Reverse(m.0));
        // each merge's rows: those it changed or deleted put back first, parents first; then those it added removed,
        // children first (the tables' order is the write-back's: parents first)
        let recs: Vec<Record> = merges
            .into_iter()
            .flat_map(|(_, mut r)| {
                r.sort_by_key(|rec| match rec.before.is_some() {
                    true => (0, rec.table as i64),
                    false => (1, -(rec.table as i64)),
                });
                r
            })
            .collect();
        let db = self.db().ok_or_else(|| refused("no database"))?;
        let tables = att.tables();
        for again in [false, true] {
            let w = format!("undo-{}{}", crate::disk::now_ms(), if again { "-again" } else { "" });
            self.fork(MAIN, &w)?;
            let done = self.undo_in(&db, &w, &tables, &recs, skip_changed, dry_run);
            let mut out = match done {
                Ok(out) if out.undone == 0 => {
                    let _ = self.discard(&w);
                    return Ok(UndoneMerges { dry_run, effects: self.effects_now(&effects, false)?, ..out });
                }
                Ok(out) => out,
                Err(e) => {
                    let _ = self.discard(&w);
                    return Err(e);
                }
            };
            let _ = take_merge_id();
            let _ = PROBED.take();
            RESTORING.set(true);
            ALLOW.set(allow_cascade);
            PROBE.set(dry_run);
            let merged = self.merge_with(&w, &crate::MergeOptions::default());
            RESTORING.set(false);
            ALLOW.set(false);
            PROBE.set(false);
            // a dry run's write-back stopped before COMMIT, having learned what else the source would write
            if let (true, Some(besides)) = (dry_run, PROBED.take()) {
                let _ = self.discard(&w);
                out.cascades = (att.kind() == SourceKind::Postgres).then_some(besides);
                out.dry_run = true;
                out.effects = self.effects_now(&effects, false)?;
                return Ok(out);
            }
            match merged {
                Ok(_) => {
                    out.merge = take_merge_id();
                    out.effects = self.effects_now(&effects, true)?;
                    return Ok(out);
                }
                Err(e) => {
                    let _ = self.discard(&w);
                    // a row changed in the source since main last had it: main has taken the source's row (see
                    // `after_failed`), and the undo, run once more, finds it changed and leaves it
                    // (a dry run asks again too: its second answer says those rows changed)
                    if again || !(skip_changed || dry_run) || !is_conflict(&e) {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!("the second time returns")
    }

    /// Puts `recs`' rows back in world `w` (a fork of main), each checked first: the world must hold the row as its
    /// merge left it.
    fn undo_in(
        &self,
        db: &crate::Db,
        w: &str,
        tables: &[Table],
        recs: &[Record],
        skip_changed: bool,
        dry_run: bool,
    ) -> Result<UndoneMerges> {
        let cell = |sql: &str| -> Result<Vec<Option<String>>> {
            let mut got = crate::sql::run(db, w, sql, &[]).map_err(sql_err)?;
            let rows = got.pop().map(|r| r.rows).unwrap_or_default();
            Ok(rows
                .first()
                .map(|r| r.iter().map(|v| (!matches!(v, crate::sql::Value::Null)).then(|| v.to_text())).collect())
                .unwrap_or_default())
        };
        let mut out = UndoneMerges::default();
        let mut writes: Vec<String> = vec![];
        // (a row several of the merges wrote: undone newest first, each from what the one after it left)
        for rec in recs {
            let t = &tables[rec.table];
            let cols = written(t);
            let of = rec.after.as_ref().or(rec.before.as_ref()).expect("a row on one side at least");
            let key = by_key(t, of);
            // the row as its merge left it, compared by type (a literal takes the column's)
            let same = match &rec.after {
                None => format!("select count(*) from {} where {key}", t.sql),
                Some(a) => {
                    let conds: Vec<String> = cols
                        .iter()
                        .map(|&i| match &a[i] {
                            Some(v) => format!("{} is not distinct from {}", q(&t.cols[i].name), lit(v)),
                            None => format!("{} is null", q(&t.cols[i].name)),
                        })
                        .collect();
                    format!("select count(*) from {} where {}", t.sql, conds.join(" and "))
                }
            };
            // (written so far in this world: what later records of the same row are checked against)
            for sql in writes.drain(..) {
                crate::sql::run(db, w, &sql, &[]).map_err(sql_err)?;
            }
            let found = cell(&same)?.first().cloned().flatten().unwrap_or_default();
            let as_left = found == if rec.after.is_some() { "1" } else { "0" };
            let mut row = UndoRow {
                merge: rec.merge.clone(),
                table: t.name.clone(),
                op: rec.op.clone(),
                key: Json::Object(
                    t.key
                        .iter()
                        .map(|&i| (t.cols[i].name.clone(), of[i].clone().map_or(Json::Null, Json::String)))
                        .collect(),
                ),
                before: rec.before.as_ref().map_or(Json::Null, |b| named(t, b)),
                after: rec.after.as_ref().map_or(Json::Null, |a| named(t, a)),
                status: "put_back",
                now: Json::Null,
            };
            if !as_left {
                // as it was before the merge already (an undo that left other rows put this one back, or it was
                // changed back by hand): nothing to undo, and nothing that stops the rest. (A row several of the
                // merges wrote is judged merge by merge, so one put back whole reads as changed for the later ones.)
                // Judged by main, as every row of an undo is: nothing is written for it, so the source isn't asked.
                // A row put back and then changed straight in the source, which main hasn't taken yet, is that
                // later change's and not this merge's to undo.
                let before = match &rec.before {
                    None => (format!("select count(*) from {} where {key}", t.sql), "0"),
                    Some(b) => {
                        let conds: Vec<String> = cols
                            .iter()
                            .map(|&i| match &b[i] {
                                Some(v) => format!("{} is not distinct from {}", q(&t.cols[i].name), lit(v)),
                                None => format!("{} is null", q(&t.cols[i].name)),
                            })
                            .collect();
                        (format!("select count(*) from {} where {}", t.sql, conds.join(" and ")), "1")
                    }
                };
                if cell(&before.0)?.first().cloned().flatten().unwrap_or_default() == before.1 {
                    row.status = "already";
                    out.already += 1;
                    out.rows.push(row);
                    continue;
                }
                let names: Vec<String> = t.cols.iter().map(|c| format!("{}::text", q(&c.name))).collect();
                let now = cell(&format!("select {} from {} where {key}", names.join(", "), t.sql))?;
                row.status = "changed";
                row.now = if now.is_empty() { Json::Null } else { named(t, &now) };
                out.skipped += 1;
                out.rows.push(row);
                continue;
            }
            out.undone += 1;
            out.rows.push(row);
            let vals = |r: &Texts, i: usize| r[i].as_deref().map_or("null".to_string(), lit);
            writes.push(match (&rec.before, &rec.after) {
                (None, _) => format!("delete from {} where {key}", t.sql),
                (Some(b), None) => {
                    let names: Vec<String> = cols.iter().map(|&i| q(&t.cols[i].name)).collect();
                    let overriding = if t.cols.iter().any(|c| c.always) { " overriding system value" } else { "" };
                    let row: Vec<String> = cols.iter().map(|&i| vals(b, i)).collect();
                    format!("insert into {} ({}){overriding} values ({})", t.sql, names.join(", "), row.join(", "))
                }
                (Some(b), Some(_)) => {
                    let sets: Vec<String> = cols
                        .iter()
                        .filter(|i| !t.key.contains(i) && !t.cols[**i].always)
                        .map(|&i| format!("{} = {}", q(&t.cols[i].name), vals(b, i)))
                        .collect();
                    match sets.is_empty() {
                        true => continue,
                        false => format!("update {} set {} where {key}", t.sql, sets.join(", ")),
                    }
                }
            });
        }
        if out.skipped > 0 && !skip_changed && !dry_run {
            let some: Vec<String> = out
                .rows
                .iter()
                .filter(|r| r.status == "changed")
                .take(10)
                .map(|r| format!("{} {}", r.table, r.key))
                .collect();
            return Err(Error::Constraint {
                code: "40001",
                message: format!(
                    "{} the merge changed have changed again since ({}); skip_changed undoes the rest; nothing was \
                     undone",
                    crate::agents::n_rows(out.skipped as u64),
                    some.join(", ")
                ),
            });
        }
        // (a dry run's too: its world is merged as far as COMMIT, then rolled back)
        for sql in writes {
            crate::sql::run(db, w, &sql, &[]).map_err(sql_err)?;
        }
        Ok(out)
    }

    /// `effects` (id, channel) with where each is now: `sent` once a worker acknowledged it (gone from main's
    /// outbox), else its state there. With `cancel`, those still delayed are taken back unsent first.
    fn effects_now(&self, effects: &[(String, String)], cancel: bool) -> Result<Vec<crate::UndoneEffect>> {
        if effects.is_empty() {
            return Ok(vec![]);
        }
        let now: HashMap<String, &'static str> = self.outbox(MAIN)?.into_iter().map(|e| (e.id, e.state)).collect();
        let delayed: Vec<String> =
            effects.iter().filter(|(id, _)| now.get(id) == Some(&"delayed")).map(|(id, _)| id.clone()).collect();
        let cancelled = match cancel && !delayed.is_empty() {
            true => self.cancel_delayed(&delayed)?,
            false => Default::default(),
        };
        Ok(effects
            .iter()
            .map(|(id, channel)| crate::UndoneEffect {
                id: id.clone(),
                channel: channel.clone(),
                state: match cancelled.contains(id) {
                    true => "cancelled",
                    false => now.get(id).copied().unwrap_or("sent"),
                },
            })
            .collect())
    }
}
