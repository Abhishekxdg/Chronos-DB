//! Whole-table rewrites in bounded memory.
//!
//! ALTER TABLE's changes that write every row again (ADD PRIMARY KEY, which moves each row to its key; a column's new
//! type, whose values are converted, a key's rows moving to their keys as the new type writes them; a generated
//! column added, which every row gets the value of) and CLUSTER of a table without a key (its rows get new keys in the
//! index's order) would hold the whole table, its index rows and its new keys in one write. Past an eighth of
//! `CHRONOS_WORK_MEM`, in a database on disk, they go on as a big INSERT does (see `bulk`):
//!
//! - **Rows** are read in key order, and each row's new version goes into a part; a part past a quarter of
//!   `CHRONOS_WORK_MEM` gets its index rows (`enforce_in`, as the table now is, against the world as the statement
//!   found it) and goes to a sorted run on disk. The table's old index rows are deleted in runs first, so every index
//!   and constraint row is made again from the rows as they now are, as a rewrite in memory makes them.
//! - **Keys.** Every row's new key is marked (`bulk::NEW`), so two rows taking one key clash when the runs are merged
//!   at commit: 23505, `could not create unique index "t_pkey"` with Postgres's DETAIL, and nothing is written.
//! - **Commit.** The merged changes go into a new tree built beside the world's, and one checkpoint makes it the
//!   world's state (`Core::commit_bulk`): a crash before that leaves the table as it was, after it the whole change,
//!   never a mix. Other writes to the world wait for it (it holds the world's long lock), reads don't.
//!
//! Memory holds a part, a block of each run being merged and a batch of changes, whatever the table's size.

use super::*;

/// What a whole-table ALTER TABLE does to each row of a big table: what `Exec::alter_writes` leaves for
/// `Exec::alter_big` to do in parts, instead of doing it in memory.
#[derive(Debug)]
pub(super) enum Redo {
    /// ADD PRIMARY KEY on the columns at `at` (`names`: as the statement names them): every row moves to its key
    Rekey { at: Vec<usize>, names: String },
    /// ALTER COLUMN TYPE of the column at `i`: each row's value worked out again, by `using`, or converted (by the
    /// casts `via`, for a regproc, then as an assignment) and kept to `mods`; `widens`: every value holds as it is.
    /// A row whose value reads differently is written again; a one-column key's rows move to their keys.
    Convert { i: usize, ty: Type, mods: TypeMods, via: Vec<Type>, using: Option<Expr>, proc: bool, widens: bool },
    /// a generated column added: every row written again with its value
    Generate,
}

/// Whether ALTER TABLE or CLUSTER may rewrite table `t` in parts: one on its own (no partitions, parents or
/// children), with no EXCLUDE constraint (checked against the table's other rows) and no foreign key to itself, kept
/// in the world.
pub(super) fn eligible(t: &Table) -> bool {
    t.schema
        && t.mem.is_none()
        && t.hier.foreign.is_none()
        && t.hier.of.is_none()
        && t.hier.part.is_none()
        && !t.hier.is_parent()
        && t.hier.all_parents().is_empty()
        && t.excludes.is_empty()
        && !t.fks.iter().any(|f| f.table == t.name)
}

/// Whether table `t`'s rows in `v` pass an eighth of `CHRONOS_WORK_MEM`, which a rewrite in memory holds several times
/// over (it reads no more than that).
pub(super) fn big(v: &View, t: &Table) -> Result<bool> {
    let (prefix, cap) = (format!("{}/", t.name), spill::work_mem() / 8);
    let mut bytes = 0;
    v.each(&prefix, None, None, |k, raw| -> Result<bool> {
        bytes += k.len() + raw.len();
        Ok(bytes <= cap)
    })?;
    Ok(bytes > cap)
}

/// A whole-table rewrite on its way to disk (see the module's comment).
pub(super) struct Rewriter<'a, 'v> {
    db: &'a crate::Core,
    v: &'a View<'v>,
    /// the table's schema as the rewrite leaves it, which its rows' index rows are made for
    schema: (String, Option<String>),
    spilled: bulk::Spilled,
    writes: Vec<(String, Option<String>)>,
    marks: Vec<(String, u8)>,
    /// the rows of the part so far, by their keys now
    keys: Vec<String>,
    bytes: usize,
    /// whether the table has constraints or indexes (before or after), whose rows are made again
    checked: bool,
    /// whether rows move to new keys that may clash
    rekey: bool,
}

impl<'a, 'v> Rewriter<'a, 'v> {
    /// A rewrite of table `before`, which becomes `after`; `rekey`: rows take keys two of them may share. The rows
    /// kept behind the table's constraints and indexes go first (deleted; the parts make them again).
    pub fn new(db: &'a crate::Core, v: &'a View<'v>, before: &Table, after: &Table, rekey: bool) -> Result<Self> {
        let Some(spilled) = bulk::Spilled::new(db) else {
            return err("XX000", "a table is rewritten in parts only in a database on disk");
        };
        let has =
            |t: &Table| !t.uniques.is_empty() || !t.fks.is_empty() || !t.indexes.is_empty() || !t.checks.is_empty();
        let mut w = Rewriter {
            db,
            v,
            schema: (schema_key(&before.name), Some(schema_json(after))),
            spilled,
            writes: vec![],
            marks: vec![],
            keys: vec![],
            bytes: 0,
            checked: has(before) || has(after),
            rekey,
        };
        if w.checked {
            let name = &before.name;
            for p in [format!("{UNIQ}/{name}/"), format!("{REFS}/{name}/"), format!("{}/{name}/", btree::IDX)] {
                for r in v.prefix(&p)? {
                    let k = r?.0;
                    w.bytes += k.len() + 64;
                    w.writes.push((k, None));
                    if w.bytes > Self::budget() {
                        w.plain()?;
                    }
                }
            }
            w.plain()?;
        }
        Ok(w)
    }

    /// The bytes of rows a part holds: a quarter of `CHRONOS_WORK_MEM` (they take about three times that with their
    /// index rows), as a big INSERT's parts.
    fn budget() -> usize {
        (spill::work_mem() / 4).max(64 << 10)
    }

    /// Writes with no rows to check (the old index rows' deletes) to a run of their own.
    fn plain(&mut self) -> Result<()> {
        let writes = std::mem::take(&mut self.writes);
        self.bytes = 0;
        if writes.is_empty() {
            return Ok(());
        }
        self.spilled.add(self.db, self.v, writes, vec![])
    }

    /// A row, under key `from` before; `to`: its key now, and `raw` what it holds now (None: as it is, under the same
    /// key).
    pub fn row(&mut self, from: &str, to: String, raw: Option<String>) -> Result<()> {
        self.bytes += from.len() + to.len() + raw.as_ref().map_or(0, String::len) + 64;
        if self.rekey {
            self.marks.push((to.clone(), bulk::NEW)); // two rows taking it clash, from any part
        }
        if to != from {
            self.writes.push((from.to_string(), None));
        }
        if let Some(raw) = raw {
            self.writes.push((to.clone(), Some(raw)));
        }
        self.keys.push(to);
        if self.bytes > Self::budget() {
            self.part()?;
        }
        Ok(())
    }

    /// The part so far, with its rows' index rows, to a run.
    fn part(&mut self) -> Result<()> {
        tick()?;
        let mut writes = std::mem::take(&mut self.writes);
        let keys = std::mem::take(&mut self.keys);
        let mut marks = std::mem::take(&mut self.marks);
        self.bytes = 0;
        if self.checked && !keys.is_empty() {
            // (the schema as it becomes is what the index rows are made for: written once, at the end)
            writes.push(self.schema.clone());
            enforce_in(self.v, &mut writes, &keys, &[], Some(&mut marks))?;
            writes.retain(|(k, _)| *k != self.schema.0);
        }
        if writes.is_empty() && marks.is_empty() {
            return Ok(());
        }
        self.spilled.add(self.db, self.v, writes, marks)
    }

    /// The last part, then `rest` (the schema and what else the statement writes), committed all at once (see
    /// `bulk::Spilled::commit`; `clash` says what's wrong with a key two rows take).
    pub fn finish(
        mut self,
        rest: Vec<(String, Option<String>)>,
        clash: &dyn Fn(&str, &[String]) -> SqlError,
    ) -> Result<u64> {
        self.part()?;
        self.spilled.add(self.db, self.v, rest, vec![])?;
        self.spilled.commit(self.db, self.v, clash)
    }
}

impl Exec<'_> {
    /// Whether ALTER TABLE is working out changes to table `table` for `alter_big`, which does what they do to its
    /// rows itself (see `Redo`).
    pub(super) fn big_alter(&self, table: &str) -> bool {
        self.nest.big.borrow().as_deref() == Some(table)
    }

    /// ALTER TABLE `table` with one change that rewrites every row (ADD PRIMARY KEY, ALTER COLUMN ... TYPE, ADD COLUMN
    /// of a generated column), on a big table, in parts (see the module's comment). None: not such a change, or not a
    /// table this does (it's done as any ALTER TABLE then).
    pub(super) fn alter_big(&self, table: &str, changes: &[Alter], only: bool) -> Result<Option<Output>> {
        let [change] = changes else { return Ok(None) };
        let rewrites = match change {
            Alter::AddPrimaryKey(..) | Alter::SetType(..) => true,
            Alter::AddColumn(d, _) => d.generated.is_some() && !d.primary,
            _ => false,
        };
        // (a statement of a function or a trigger, or inside another's write, joins that write)
        if !rewrites
            || self.readonly
            || self.nest.depth.get() > 0
            || self.nest.state.borrow().is_some()
            || self.db.spill_dir().is_none()
        {
            return Ok(None);
        }
        {
            let v = self.db.view_tx(self.branch)?;
            let Some(t) = load(&v, table)? else { return Ok(None) };
            if !eligible(&t) || t.matview.is_some() || !big(&v, &t)? {
                return Ok(None);
            }
        }
        self.db.batch_long(self.branch, spill::work_mem(), |v, _| {
            let before = need_table(v, table)?;
            if !eligible(&before) || before.matview.is_some() {
                // (changed since it was looked at: as any ALTER TABLE)
                return Ok((self.alter_hier(v, table, changes, only)?, ()));
            }
            *self.nest.big.borrow_mut() = Some(table.to_string());
            let writes = self.alter_hier(v, table, changes, only);
            self.nest.big.take();
            let redo = self.nest.redo.take();
            let writes = writes?;
            let Some(redo) = redo else { return Ok((writes, ())) };
            let Some(Some(raw)) = writes.iter().find(|(k, _)| *k == schema_key(table)).map(|(_, x)| x.clone()) else {
                return err("XX000", format!("ALTER TABLE of \"{table}\" in parts left no schema"));
            };
            let mut after = schema_of(&Val::from(raw.as_str()))?;
            after.name = table.to_string();
            for c in &mut after.cols {
                c.table = table.to_string();
            }
            self.rewrite_rows(v, table, &before, &after, &redo, writes)?;
            Ok::<_, SqlError>((vec![], ()))
        })?;
        Ok(Some(Output::tag("ALTER TABLE")))
    }

    /// Every row of `before` (in `v`) written again as `redo` says, for table `after`, and `rest` (the schema and what
    /// else ALTER TABLE writes), committed at once.
    fn rewrite_rows(
        &self,
        v: &View,
        table: &str,
        before: &Table,
        after: &Table,
        redo: &Redo,
        rest: Vec<(String, Option<String>)>,
    ) -> Result<u64> {
        let rekey = match redo {
            Redo::Rekey { .. } => true,
            Redo::Convert { i, ty, .. } => after.key == Some(*i) && before.cols[*i].ty != *ty,
            Redo::Generate => false,
        };
        let mut w = Rewriter::new(self.db, v, before, after, rekey)?;
        let prefix = format!("{}/", before.name);
        // (a column given new values isn't read through the new type: its old value may not be one of it)
        let need: Vec<bool> = match redo {
            Redo::Convert { i, .. } => (0..after.cols.len()).map(|c| c != *i).collect(),
            _ => vec![true; after.cols.len()],
        };
        v.each(&prefix, None, None, |k, raw| -> Result<bool> {
            let id = &k[prefix.len()..];
            match redo {
                Redo::Rekey { at, .. } => {
                    // every row moves to its key: never null (unique is checked as the parts are merged)
                    let mut vals = decode_raw(before, id, raw, None)?;
                    for &i in at {
                        if vals[i] == Value::Null {
                            let c = &before.cols[i].name;
                            return err(
                                NOT_NULL,
                                format!("column \"{c}\" of relation \"{}\" contains null values", rel_part(table)),
                            );
                        }
                        vals[i] = coerce(std::mem::take(&mut vals[i]), after.cols[i].ty)?;
                    }
                    let to = row_key(table, &after.row_id(&vals)?);
                    let moved = (to != k).then(|| raw.to_string());
                    w.row(k, to, moved)?;
                }
                Redo::Generate => {
                    // (read through the new layout, so the new column is null)
                    let mut vals = decode_raw(after, id, raw, None)?;
                    generate(after, &mut vals)?;
                    w.row(k, k.to_string(), Some(pack(&vals, after.layout())))?;
                }
                Redo::Convert { i, ty, mods, via, using, proc, widens } => {
                    let i = *i;
                    let old = decode_raw(before, id, raw, None)?;
                    let at_row =
                        |e: SqlError| SqlError { message: format!("row {table}/{}: {}", show_id(id), e.message), ..e };
                    // (as `alter_writes` works them out: USING's value, made the new type; else each step as an
                    // assignment, a regproc's casts first)
                    let new = match using {
                        Some(e) => {
                            let env = Env { cols: &before.cols, row: &old, params: &[], aggs: &[], sub: None };
                            Some(
                                eval(e, &env)
                                    .and_then(|x| coerce(x, *ty))
                                    .and_then(|y| fit_mods(y, *ty, mods))
                                    .map_err(at_row)?,
                            )
                        }
                        None if *widens && !*proc => None,
                        None => {
                            let via: &[Type] = if *proc { via } else { &[] };
                            let from = before.cols[i].ty;
                            let y = via
                                .iter()
                                .chain([ty])
                                .try_fold((old[i].clone(), from), |(x, f), t| Ok((assigned(x, Some(f), *t)?, *t)))
                                .map(|(y, _)| y)
                                .and_then(|y| fit_mods(y, *ty, mods))
                                .map_err(at_row)?;
                            Some(y)
                        }
                    };
                    if rekey {
                        // a key's rows move to their keys as the new type writes them ('05' and '5' are both 5)
                        let mut vals = old;
                        vals[i] = coerce(std::mem::take(&mut vals[i]), *ty)?;
                        let to = row_key(table, &after.row_id(&vals)?);
                        let moved = (to != k).then(|| raw.to_string());
                        w.row(k, to, moved)?;
                        return Ok(true);
                    }
                    let changed = new.filter(|y| using.is_some() || *proc || y.to_text() != old[i].to_text());
                    match changed {
                        Some(y) => {
                            let mut vals = decode_raw(after, id, raw, Some(&need[..]))?;
                            vals[i] = y;
                            w.row(k, k.to_string(), Some(pack(&vals, after.layout())))?;
                        }
                        None => w.row(k, k.to_string(), None)?,
                    }
                }
            }
            Ok(true)
        })?;
        let (pkey, names, at) = match redo {
            Redo::Rekey { at, names } => (after.pkey_name(), names.clone(), at.clone()),
            Redo::Convert { i, .. } => (after.pkey_name(), after.cols[*i].name.clone(), vec![*i]),
            Redo::Generate => (String::new(), String::new(), vec![]),
        };
        // two rows taking one key: as Postgres's index build says it, with the value they share
        let clash = |key: &str, holders: &[String]| -> SqlError {
            let Some(id) = key.strip_prefix(&prefix).filter(|_| rekey) else { return super::clash(v, key, holders) };
            let shown = holders
                .first()
                .and_then(|raw| decode_raw(after, id, raw, None).ok())
                .map(|vals| at.iter().map(|&i| vals[i].to_text()).collect::<Vec<_>>().join(", "))
                .or_else(|| composite_values(id).map(|x| x.iter().map(Value::to_text).collect::<Vec<_>>().join(", ")))
                .unwrap_or_else(|| show_id(id).into_owned());
            index_duplicated(&pkey, &names, &shown)
        };
        w.finish(rest, &clash)
    }
}

/// CLUSTER of table `name` (on index `using`, or the one it was clustered on) when it is a big table without a key:
/// its rows get new keys in the index's order in parts (see the module's comment). None: not one this does.
pub(super) fn cluster_big(db: &Db, branch: &str, name: &str, using: Option<&str>) -> Result<Option<Output>> {
    {
        let v = db.view_tx(branch)?;
        let Some(t) = load(&v, name)? else { return Ok(None) };
        if !eligible(&t) || t.has_key() && t.matview.is_none() || !big(&v, &t)? {
            return Ok(None);
        }
    }
    db.core.batch_long(branch, spill::work_mem(), |v, _| {
        let t = need_table(v, name)?;
        let ix = maintain::cluster_index(&t, using)?;
        maintain::clusterable(v, &t, &ix)?;
        if !eligible(&t) {
            return Ok((maintain::cluster_writes(v, &t, &ix)?, ()));
        }
        let mut after = t.clone();
        let mut rest = vec![];
        if after.hier.clustered.as_deref() != Some(ix.as_str()) {
            after.hier.clustered = Some(ix.clone());
            rest.push((schema_key(&t.name), Some(schema_json(&after))));
        }
        let unpopulated = t.matview.as_ref().is_some_and(|m| m["populated"] == false);
        if unpopulated || (t.has_key() && t.matview.is_none()) {
            return Ok((rest, ()));
        }
        // each row by its place in the index's order (one the index has no entry for, one holding JSON, last), then
        // its key: in order already, nothing moves
        let prefix = format!("{}/", t.name);
        let mut places = spill::Pairs::new(spill::work_mem() / 2);
        let (mut last, mut ordered, mut n) = (None::<String>, true, 0usize);
        v.each(&prefix, None, None, |k, raw| -> Result<bool> {
            let vals = decode_raw(&t, &k[prefix.len()..], raw, None)?;
            let place = maintain::cluster_key(&t, &ix, &vals, &k[prefix.len()..])
                .map_or_else(|| "1".to_string(), |p| format!("0{p}"));
            if last.as_ref().is_some_and(|l| place < *l) {
                ordered = false;
            }
            last = Some(place.clone());
            places.push(place, format!("{k}\0{raw}"))?;
            n += 1;
            Ok(true)
        })?;
        if ordered {
            return Ok((rest, ()));
        }
        // a materialized view's rows are keyed by their place; a table's get new made-up keys, in the order made
        let mut ids = new_row_ids(n);
        let mut place = 0usize;
        let mut w = Rewriter::new(&db.core, v, &t, &after, false)?;
        places.each(|_, row| {
            let (k, raw) = row.split_once('\0').unwrap_or((row, ""));
            place += 1;
            let id = if t.matview.is_some() { format!("{place:010}") } else { ids.next().unwrap_or_default() };
            w.row(k, row_key(&t.name, &id), Some(raw.to_string()))
        })?;
        drop(places);
        w.finish(rest, &|key, holders| clash(v, key, holders))?;
        Ok::<_, SqlError>((vec![], ()))
    })?;
    Ok(Some(Output::tag("CLUSTER")))
}
