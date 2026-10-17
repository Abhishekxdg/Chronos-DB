//! Correlated EXISTS and IN answered from a hash table.
//!
//! `EXISTS (SELECT 1 FROM t l2 WHERE l2.company = l1.company AND ...)` used to run its subquery
//! once for every outer row, with that row's values filled in: a full read of `t` each time
//! when `company` has no index (15,000 outer rows over a 150,000-row table took two minutes).
//! Here the subquery runs once, without the conditions that tie it to the outer row, and its
//! rows are kept by the value of the inner side of each equality (`l2.company`). An outer row
//! then finds its candidates by hashing its own side (`l1.company`) and checks only the other
//! correlated conditions (`l2.created_at > l1.created_at`) on those.
//!
//! The answers are the ones the per-row subquery gave: a condition counts only when it is true
//! (null is not), so a null key on either side matches nothing: EXISTS is false, NOT EXISTS true,
//! and for IN the rows are none, as with the subquery. The keys are hashed as joins hash them
//! (`hkey`: 5 and 5.0 meet). A subquery that isn't of this shape (grouped, limited, with its own
//! subqueries, a volatile function, no equality to hash on, more rows than CHRONOS_WORK_MEM
//! holds) keeps running per outer row; so does one whose equality an index serves, until that
//! has taken half a second (`Slot::Indexed`): a lookup by index is cheap when it picks few rows.

use super::*;

type Place = (usize, Option<String>, String);

/// A correlated EXISTS or IN subquery's rows, by the values its equalities compare with the
/// outer row.
pub(super) struct Semi {
    /// where in the outer row the subquery reads, and how wide that row is: another outer query
    /// with the same subquery text but other columns doesn't use this table
    places: Vec<usize>,
    outer_width: usize,
    /// the outer side of each equality, over the outer row
    keys: Vec<Expr>,
    /// the other correlated conditions, over the outer row followed by `extras`
    resid: Vec<Expr>,
    cols: Vec<Col>,
    /// inner columns `resid` reads, kept after each row's select list
    extras: usize,
    /// the select list's width
    width: usize,
    map: FastMap<Vec<HKey>, Vec<Vec<Value>>>,
    some: SubRows,
    none: SubRows,
    /// about how many bytes the rows and keys take
    bytes: usize,
}

/// Whether `e` reads a column of the outer query (`inner`: of the subquery's own tables).
fn names_outer(e: &Expr, outer: &[Place], inner: bool) -> bool {
    let mut found = false;
    walk(e, &mut |x| {
        if let Expr::Col(q, n) = x {
            let is_outer = outer.iter().any(|(_, oq, on)| oq == q && on == n);
            found |= is_outer != inner;
        }
    });
    found
}

/// Whether an outer value is read anywhere in `e`.
fn reads_outer(e: &Expr, outer: &[Place]) -> bool {
    names_outer(e, outer, false)
}

/// Whether a column of the subquery's own tables is read anywhere in `e`.
fn reads_inner(e: &Expr, outer: &[Place]) -> bool {
    names_outer(e, outer, true)
}

/// The conditions joined by AND, or none.
fn all_of(conds: Vec<&Expr>) -> Option<Expr> {
    conds.into_iter().cloned().reduce(|a, b| bin(a, Op::And, b))
}

/// Whether an index or the primary key serves lookups of this key of the subquery: a column an
/// index starts with, or an expression where a table has an index on an expression (which of
/// them isn't worked out: a wrong guess costs `WATCH`). Then each outer row's lookup is cheap,
/// and a hash would only add a scan.
fn served(tables: &[Table], e: &Expr) -> bool {
    let Expr::Col(qual, name) = e else {
        return tables.iter().any(|t| t.mem.is_none() && t.indexes.iter().any(|x| x.cols.contains(&btree::EXPR)));
    };
    let cols: Vec<Col> = tables.iter().flat_map(|t| t.cols.iter().cloned()).collect();
    let Ok(i) = resolve(&cols, qual.as_deref(), name) else { return false };
    let mut start = 0;
    for t in tables {
        if i < start + t.cols.len() {
            let c = i - start;
            return t.mem.is_none()
                && (t.key == Some(c)
                    || t.pk.first() == Some(&c)
                    || t.indexes.iter().any(|x| x.cols.first() == Some(&t.cols[c].id)));
        }
        start += t.cols.len();
    }
    false
}

/// Types whose equal values hash alike (`hkey`) where `=` agrees: the same type, or exact numbers
/// (integers and numeric). A float against an integer compares through f64, which can call two
/// numbers past 2^53 equal that hashing keeps apart, so those keep their comparison. An oid hashes
/// only with an oid or a reg value (`c.oid = r`, both the object's oid): an integer beside one is
/// converted first (-1 is 4294967295), which a key compared as it is would miss.
pub(super) fn hashes_alike(a: Type, b: Type) -> bool {
    let exact = |t: Type| t.integral() || t == Type::Numeric;
    let oid = |t: Type| matches!(t, Type::Oid | Type::Reg(_));
    a == b || (exact(a) && exact(b)) || (oid(a) && oid(b))
}

/// Errors that end a statement whatever way it's answered: it was canceled, timed out, ran out
/// of memory, or its client is gone. Any other error building a hash table is the table's, not
/// the query's: the rows it read are more than the per-row subquery would have read (a condition
/// that fails on a row the correlated `=` would have skipped), so that runs, and says if it
/// fails.
pub(super) fn fatal(e: &SqlError) -> bool {
    matches!(e.code, CANCELED | OUT_OF_MEMORY | "54000" | "08006")
}

/// What a correlated subquery is answered from.
#[derive(Clone)]
pub(super) enum Slot {
    Hash(std::rc::Rc<Semi>),
    /// not of a shape a hash can answer, or too big to hold: it runs per outer row
    PerRow,
    /// an index serves its equality, so per outer row looks cheap: it runs that way while the
    /// time it takes stays under `WATCH`, then hashes after all (the index may pick few of
    /// the table's rows, but not few enough)
    Indexed,
}

/// Time spent running an index-served subquery per outer row, past which it is hashed.
const WATCH: std::time::Duration = std::time::Duration::from_millis(500);

impl Exec<'_> {
    /// How correlated `sq` is answered: from its hash table (built when first asked), per outer
    /// row, or per outer row under watch. The table serves only an outer row laid out as the one
    /// it was built for.
    pub(super) fn semi(&self, sq: &SubQ, env: &Env, exists: bool) -> Result<Slot> {
        let id = (sq.sig, exists);
        let mut slot = self.subs.borrow_mut().semi.get(&id);
        let mut force = false;
        if matches!(slot, Some(Slot::Indexed)) && self.subs.borrow_mut().watch.get(&id).unwrap_or_default() >= WATCH {
            (slot, force) = (None, true);
        }
        let slot = match slot {
            Some(s) => s,
            None => {
                let s = self.semi_build(sq, env, exists, force)?;
                let bytes = if let Slot::Hash(h) = &s { h.bytes } else { 0 };
                self.subs.borrow_mut().semi.put(id, s.clone(), bytes);
                s
            }
        };
        Ok(match slot {
            Slot::Hash(s)
                if !(s.outer_width == env.cols.len() && s.places.iter().eq(sq.outer.iter().map(|o| &o.0))) =>
            {
                Slot::PerRow
            }
            slot => slot,
        })
    }

    /// The rows of correlated `sq` for the outer row in `env`, from its hash table `s`, or from
    /// the answer already given for the same outer values (as `Runner::rows` keeps them: the
    /// tag tells an EXISTS's answer, which isn't the subquery's rows, from an IN's).
    pub(super) fn semi_rows(&self, s: &Semi, sq: &SubQ, env: &Env, exists: bool) -> Result<SubRows> {
        let tag = usize::MAX - usize::from(exists);
        let key = (sq.sig, tag, sq.outer.iter().map(|(i, ..)| hkey(&env.row[*i])).collect());
        let hit = self.subs.borrow_mut().get(&key);
        if let Some(rows) = hit {
            return Ok(rows);
        }
        let rows = s.probe(env, self.params, exists)?;
        self.subs.borrow_mut().insert(key, rows.clone());
        Ok(rows)
    }

    /// Adds the time one outer row's subquery took (see `Slot::Indexed`).
    pub(super) fn semi_spent(&self, sq: &SubQ, exists: bool, took: std::time::Duration) {
        let id = (sq.sig, exists);
        let mut cache = self.subs.borrow_mut();
        let was = cache.watch.get(&id).unwrap_or_default();
        cache.watch.put(id, was + took, 16);
    }

    fn semi_build(&self, sq: &SubQ, env: &Env, exists: bool, force: bool) -> Result<Slot> {
        let (q, outer) = (&*sq.q, &sq.outer[..]);
        let src_ok = |s: &Source| !s.lateral && s.srf.is_none();
        if !(q.set.is_empty()
            && q.group.is_empty()
            && q.distinct_on.is_empty()
            && q.having.is_none()
            && q.limit.is_none()
            && q.offset.is_none()
            && q.from.as_ref().is_some_and(src_ok)
            && q.joins.iter().all(|j| src_ok(&j.src)))
        {
            return Ok(Slot::PerRow);
        }
        // outer values only in WHERE conditions: not in ON, a FROM subquery, or (for IN) the
        // select list; and nothing there that the answer can't repeat
        let mut elsewhere: Vec<&Expr> = q.joins.iter().filter_map(|j| j.on.as_ref()).collect();
        elsewhere.extend(q.from.iter().chain(q.joins.iter().map(|j| &j.src)).filter_map(|s| s.at.as_ref()));
        if !exists {
            elsewhere.extend(q.items.iter().filter_map(|i| if let Item::Expr(e, _) = i { Some(e) } else { None }));
        }
        nested_exprs(q, &mut elsewhere);
        let filter: Vec<&Expr> = q.filter.as_ref().map(conjuncts).unwrap_or_default();
        let listed = || elsewhere.iter().copied().chain(filter.iter().copied());
        if !no_subs(listed()) || listed().any(per_row) || elsewhere.iter().any(|e| reads_outer(e, outer)) {
            return Ok(Slot::PerRow);
        }
        // an aggregate's (`(SELECT max(x) FROM t WHERE t.k = outer.k)`): its value for each key, from the rows grouped
        // by it, and for a key no row has, its value over no rows
        let aggregated = q.items.iter().any(|i| matches!(i, Item::Expr(e, _) if has_agg(e)));
        // (a column read outside its aggregates is an error per row: left to say so)
        let outside = |e: &Expr| {
            let mut col = false;
            walk(&rewrite(e, &|x| matches!(x, Expr::Agg(..)).then(|| Expr::Lit(Value::Null))), &mut |x| {
                col |= matches!(x, Expr::Col(..) | Expr::Idx(_))
            });
            col
        };
        let grouped = aggregated
            && !exists
            && !q.distinct
            && q.order.is_empty()
            && !q
                .items
                .iter()
                .any(|i| matches!(i, Item::Expr(e, _) if outside(e)) || matches!(i, Item::Star | Item::TableStar(_)));
        if q.items.iter().any(|i| matches!(i, Item::Expr(e, _) if has_window(e)))
            || (aggregated && !grouped)
            || filter.iter().any(|e| has_agg(e) || has_window(e))
            || q.order.iter().any(|(e, _)| has_agg(e) || has_window(e))
        {
            return Ok(Slot::PerRow);
        }

        // the equalities between an inner and an outer expression, the other conditions that
        // read the outer row, and the ones that don't
        let (mut keys, mut resid, mut inner): (Vec<(&Expr, &Expr)>, Vec<&Expr>, Vec<&Expr>) = (vec![], vec![], vec![]);
        for c in filter {
            if !reads_outer(c, outer) {
                inner.push(c);
                continue;
            }
            if let Expr::Bin(a, Op::Eq, b) = c {
                let side = |i: &Expr, o: &Expr| {
                    reads_inner(i, outer) && !reads_outer(i, outer) && reads_outer(o, outer) && !reads_inner(o, outer)
                };
                if side(a, b) {
                    keys.push((a, b));
                    continue;
                }
                if side(b, a) {
                    keys.push((b, a));
                    continue;
                }
            }
            resid.push(c);
        }
        // (a grouped one's other correlated conditions would pick rows within a group)
        if keys.is_empty() || (grouped && !resid.is_empty()) {
            return Ok(Slot::PerRow);
        }
        // an indexed column is looked up per outer row; hashing it would read the table to save that
        let v = self.view()?;
        let Ok(tables) = sources(&v, q, None) else { return Ok(Slot::PerRow) };
        if !force && keys.iter().any(|(i, _)| served(&tables, i)) {
            return Ok(Slot::Indexed);
        }

        // the inner columns the other conditions read
        let mut extras: Vec<(Option<String>, String)> = vec![];
        for r in &resid {
            walk(r, &mut |x| {
                if let Expr::Col(qual, n) = x
                    && !outer.iter().any(|(_, oq, on)| oq == qual && on == n)
                    && !extras.iter().any(|(eq, en)| eq == qual && en == n)
                {
                    extras.push((qual.clone(), n.clone()));
                }
            });
        }
        let mut items: Vec<Item> =
            if exists { vec![Item::Expr(Expr::Lit(Value::Int(1)), None)] } else { q.items.clone() };
        for (i, (inner_key, _)) in keys.iter().enumerate() {
            items.push(Item::Expr((*inner_key).clone(), Some(format!("__k{i}"))));
        }
        for (j, (qual, n)) in extras.iter().enumerate() {
            items.push(Item::Expr(Expr::Col(qual.clone(), n.clone()), Some(format!("__r{j}"))));
        }
        let q2 = Select {
            set: vec![],
            distinct: false,
            distinct_on: vec![],
            items,
            from: q.from.clone(),
            joins: q.joins.clone(),
            filter: all_of(inner),
            group: if grouped { keys.iter().map(|(i, _)| (*i).clone()).collect() } else { vec![] },
            having: None,
            order: vec![],
            limit: None,
            offset: None,
            locking: vec![],
        };
        // what it holds is about the rows, and for each a key and a place in the table: a quarter
        // of CHRONOS_WORK_MEM at most, so a few can be kept (see `Kept`). Few rows are read at
        // first, to see how wide they are and so how many may be read.
        let (nk, ne) = (keys.len(), extras.len());
        let budget = spill::work_mem() / 4;
        let weigh = |rows: &[Vec<Value>]| rows.iter().map(|r| 64 + 32 * nk + spill::size_of(r)).sum::<usize>();
        enum Fetch {
            Rows(Output),
            /// more than the limit (the limit and one more)
            Over(Output),
            Failed,
        }
        let fetch = |limit: usize| -> Result<Fetch> {
            let mut q = q2.clone();
            q.limit = Some(Expr::Lit(Value::Int(limit as i64 + 1)));
            // as the per-row run does: a subquery that reads the outer row gets its own WITH rows
            let held = self.ctes.take();
            let ran = self.select(&q);
            *self.ctes.borrow_mut() = held;
            match ran {
                Ok(out) if out.rows.len() <= limit => Ok(Fetch::Rows(out)),
                Ok(out) => Ok(Fetch::Over(out)),
                Err(e) if fatal(&e) => Err(e),
                Err(_) => Ok(Fetch::Failed),
            }
        };
        const PROBE: usize = 4096;
        let out = match fetch(PROBE)? {
            Fetch::Rows(out) => out,
            Fetch::Failed => return Ok(Slot::PerRow),
            Fetch::Over(few) => {
                let allowed = budget / (weigh(&few.rows) / few.rows.len() + 1);
                if allowed <= PROBE {
                    return Ok(Slot::PerRow);
                }
                match fetch(allowed)? {
                    Fetch::Rows(out) => out,
                    _ => return Ok(Slot::PerRow), // too many to hold: per outer row, as before
                }
            }
        };
        let bytes = weigh(&out.rows);
        if bytes > budget {
            return Ok(Slot::PerRow);
        }
        let Some(types) = out.columns.as_ref().map(|c| c.iter().map(|(_, t)| *t).collect::<Vec<_>>()) else {
            return Ok(Slot::PerRow);
        };
        let Some(width) = types.len().checked_sub(nk + ne).filter(|w| *w >= 1 && (exists || *w == 1)) else {
            return Ok(Slot::PerRow);
        };
        // the outer sides over the outer row, the other conditions over it and the extras
        let mut bound = vec![];
        for (j, (_, o)) in keys.iter().enumerate() {
            let Ok(e) = Binder::plain(env.cols, None).bind(o) else { return Ok(Slot::PerRow) };
            match ty_in(&e, env.cols, self.types, &[]) {
                Ok(t) if hashes_alike(t, types[width + j]) => bound.push(e),
                _ => return Ok(Slot::PerRow),
            }
        }
        let mut cols = env.cols.to_vec();
        for (j, t) in types[width + nk..].iter().enumerate() {
            cols.push(Col::new("", &format!("__r{j}"), *t));
        }
        let mut conds = vec![];
        for r in resid {
            let renamed = rewrite(r, &|x| match x {
                Expr::Col(qual, n) if !outer.iter().any(|(_, oq, on)| oq == qual && on == n) => {
                    let j = extras.iter().position(|(eq, en)| eq == qual && en == n)?;
                    Some(Expr::Col(None, format!("__r{j}")))
                }
                _ => None,
            });
            let Ok(e) = Binder::plain(&cols, None).bind(&renamed) else { return Ok(Slot::PerRow) };
            conds.push(e);
        }

        // (a grouped one's value where no row has the key: the aggregates over no rows, `count(*)` 0)
        let none = match grouped {
            false => std::rc::Rc::new(vec![]),
            true => {
                let nothing = Select {
                    items: q.items.clone(),
                    filter: Some(Expr::Lit(Value::Bool(false))),
                    group: vec![],
                    order: vec![],
                    ..q2.clone()
                };
                let held = self.ctes.take();
                let ran = self.select(&nothing);
                *self.ctes.borrow_mut() = held;
                match ran {
                    Ok(out) if out.rows.len() == 1 => std::rc::Rc::new(out.rows),
                    Err(e) if fatal(&e) => return Err(e),
                    _ => return Ok(Slot::PerRow),
                }
            }
        };
        let exact = conds.is_empty() && exists;
        let mut map: FastMap<Vec<HKey>, Vec<Vec<Value>>> = FastMap::default();
        'rows: for mut row in out.rows {
            tick()?;
            let mut key = Vec::with_capacity(nk);
            for v in &row[width..width + nk] {
                if *v == Value::Null {
                    continue 'rows; // equals nothing
                }
                key.push(hkey(v));
            }
            row.drain(width..width + nk);
            let bucket = map.entry(key).or_default();
            if !exact || bucket.is_empty() {
                bucket.push(row);
            }
        }
        Ok(Slot::Hash(std::rc::Rc::new(Semi {
            places: outer.iter().map(|o| o.0).collect(),
            outer_width: env.cols.len(),
            keys: bound,
            resid: conds,
            cols,
            extras: ne,
            width,
            map,
            some: std::rc::Rc::new(vec![vec![Value::Int(1)]]),
            none,
            bytes,
        })))
    }
}

impl Semi {
    /// The subquery's rows for the outer row in `env`.
    fn probe(&self, env: &Env, params: &[Value], exists: bool) -> Result<SubRows> {
        let mut key = Vec::with_capacity(self.keys.len());
        for k in &self.keys {
            let v = eval(k, env)?;
            if v == Value::Null {
                return Ok(self.none.clone());
            }
            key.push(hkey(&v));
        }
        let Some(bucket) = self.map.get(&key) else { return Ok(self.none.clone()) };
        if self.resid.is_empty() && exists {
            return Ok(self.some.clone());
        }
        let n = env.row.len();
        let mut buf: Vec<Value> = env.row.to_vec();
        buf.resize(n + self.extras, Value::Null);
        let mut found: Vec<Vec<Value>> = vec![];
        for row in bucket {
            tick()?;
            let pass = if self.resid.is_empty() {
                true
            } else {
                buf[n..].clone_from_slice(&row[self.width..]);
                let at = Env { cols: &self.cols, row: &buf, params, aggs: &[], sub: None };
                let mut all = true;
                for c in &self.resid {
                    if truth(ev(c, &at)?.as_ref())? != Some(true) {
                        all = false;
                        break;
                    }
                }
                all
            };
            if pass {
                if exists {
                    return Ok(self.some.clone());
                }
                found.push(row[..self.width].to_vec());
            }
        }
        Ok(if found.is_empty() { self.none.clone() } else { std::rc::Rc::new(found) })
    }
}

/// Fewer rows than this in an IN's list: compared one by one.
pub(super) const SET_MIN: usize = 16;

/// What kind of values `hkey` can compare as `=` does (exact numbers with exact numbers, floats
/// with floats, text with text): none for the rest, which keep their comparisons.
fn kind(v: &Value) -> Option<u8> {
    match v {
        Value::Int(_) | Value::Numeric(_) => Some(1),
        Value::Text(_) => Some(2),
        Value::Date(_) => Some(3),
        Value::Bool(_) => Some(4),
        Value::Float(_) => Some(5),
        _ => None,
    }
}

/// An IN list (an uncorrelated subquery's first column) as a hash set, for the rows of a table
/// to be looked up in it without comparing each with all of them.
pub(super) struct InSet {
    kind: u8,
    null: bool,
    keys: std::collections::HashSet<HKey, std::hash::BuildHasherDefault<Fx>>,
    bytes: usize,
}

impl InSet {
    /// None unless every value in the list is of one kind that hashes as `=` compares.
    pub(super) fn new(rows: &[Vec<Value>]) -> Option<InSet> {
        InSet::of(rows.iter().map(|r| r.first().unwrap_or(&Value::Null)))
    }

    /// `new`, of the values themselves (an IN list's constants, an array's elements).
    pub(super) fn of<'a>(values: impl Iterator<Item = &'a Value>) -> Option<InSet> {
        let mut set = InSet { kind: 0, null: false, keys: Default::default(), bytes: 0 };
        for v in values {
            if *v == Value::Null {
                set.null = true;
                continue;
            }
            let k = kind(v)?;
            if set.kind != 0 && set.kind != k {
                return None;
            }
            set.kind = k;
            set.keys.insert(hkey(v));
            set.bytes += 64 + spill::size_of(std::slice::from_ref(v));
        }
        Some(set)
    }

    /// About how many bytes the set takes.
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Whether `x` is in the list, and whether the list holds a null; None when `x` is of
    /// another kind than the list (which `=` may still equate: those compare one by one).
    pub(super) fn contains(&self, x: &Value) -> Option<(bool, bool)> {
        if self.kind != 0 && kind(x) != Some(self.kind) {
            return None;
        }
        Some((self.keys.contains(&hkey(x)), self.null))
    }

    /// Whether the list has no values at all, not even a null (an empty array's).
    pub(super) fn is_empty(&self) -> bool {
        self.keys.is_empty() && !self.null
    }
}
