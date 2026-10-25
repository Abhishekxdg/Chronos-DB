//! A port of the part of Postgres 16's planner that decides the order of a query's joins when they can only be
//! nested loops: tables joined with no condition (a cross join) or by conditions no hash or merge join can use
//! (`a.x < b.y`, geometric operators). Postgres chooses which table is the outer one by cost, from the sizes
//! its statistics give (see `cost`, `clausesel`, `joinsearch`); the order of the rows such a join returns
//! follows from that choice, which is where the regression suite's planner-chosen row order comes from.
//!
//! When `chronos.follow_pg_plan` is on (by default in exact mode, CHRONOS_PG_EXACT), the executor joins such a
//! query's tables in that order (see `Exec::join`). Whichever order it takes, every row of every table is read;
//! the order changes which rows come out first, and, for three tables or more, how many pairs of rows the
//! nested loops check and hold (each step's inner side is held, as Material holds it). It is all or nothing:
//! anything the port doesn't model exactly (an index, a clause a hash join could use, an estimate that would
//! read a pg_statistic row, a LIMIT, an outer join, a table whose rows the heap model can't lay out or whose
//! heap Postgres would have made otherwise, a setting it doesn't follow) leaves the executor's own order, never
//! a guess.
//!
//! The heap model counts a table's pages as Postgres's heap would have them had its rows been inserted one by
//! one into it and kept (see `heap`). A table whose heap a statement made another way is marked so and left
//! alone (`stats::RelStats::heap_unknown`): CREATE TABLE AS's bulk fill, a rewrite by ALTER TABLE or VACUUM FULL.
//! What isn't marked is what row changes leave in Postgres's heap: the dead tuples of a DELETE or UPDATE (MERGE,
//! ON CONFLICT DO UPDATE, a foreign key's action), of an INSERT that failed part way or was rolled back, whose
//! space VACUUM keeps for the free space map rather than giving it back, and the empty pages COPY's bulk extension
//! adds. Marking those would be a write to a row of the table's by every such statement, which two transactions
//! changing different rows of one table would then both write and conflict on, as Postgres's never do; so a join
//! of a table with such a past can come out in another order than Postgres's, as `FOLLOW_SETTING_ABOUT` says.
//!
//! The query is put in Postgres's terms here as deconstruct_jointree and distribute_qual_to_rels
//! (initsplan.c) would: the FROM items in order as the joinlist, each WHERE and ON condition a RestrictInfo of
//! the base rel it reads or a join clause, in the order they are distributed (ON conditions first, as the
//! JoinExprs below the FROM list are processed before its WHERE).
//!
//! Portions ported from PostgreSQL 16 (src/backend/optimizer/plan/initsplan.c, src/backend/optimizer/util/
//! plancat.c, src/backend/optimizer/path/costsize.c's cost_qual_eval_walker), under the PostgreSQL License:
//!
//! Portions Copyright (c) 1996-2023, PostgreSQL Global Development Group
//! Portions Copyright (c) 1994, The Regents of the University of California
//!
//! Permission to use, copy, modify, and distribute this software and its documentation for any purpose, without
//! fee, and without a written agreement is hereby granted, provided that the above copyright notice and this
//! paragraph and the following two paragraphs appear in all copies.
//!
//! IN NO EVENT SHALL THE UNIVERSITY OF CALIFORNIA BE LIABLE TO ANY PARTY FOR DIRECT, INDIRECT, SPECIAL,
//! INCIDENTAL, OR CONSEQUENTIAL DAMAGES, INCLUDING LOST PROFITS, ARISING OUT OF THE USE OF THIS SOFTWARE AND ITS
//! DOCUMENTATION, EVEN IF THE UNIVERSITY OF CALIFORNIA HAS BEEN ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//!
//! THE UNIVERSITY OF CALIFORNIA SPECIFICALLY DISCLAIMS ANY WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
//! WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE. THE SOFTWARE PROVIDED HEREUNDER IS ON AN "AS
//! IS" BASIS, AND THE UNIVERSITY OF CALIFORNIA HAS NO OBLIGATIONS TO PROVIDE MAINTENANCE, SUPPORT, UPDATES,
//! ENHANCEMENTS, OR MODIFICATIONS.

pub(crate) mod clausesel;
pub(crate) mod cost;
pub(crate) mod heap;
pub(crate) mod joinsearch;
pub(crate) mod stats;

use std::cell::RefCell;
use std::sync::LazyLock;

use super::{
    Col, Expr, Func, JoinKind, Op, Plan, Select, SelectDepth, Table, Type, Value, View, catalog_schema, col_typmod,
    funcs, op_symbol, refs, settings, ty_in,
};
use clausesel::{Arg, Node, OpClause, Oper};
use cost::{QualCost, RelSize, Settings};
use joinsearch::{BaseRel, FINAL, Query, Restrict, Var};
use stats::RelStats;

/// A set of base rels, by their place in the FROM list (bit k: the kth).
pub(crate) type Relids = u32;

/// The most rows the executor reads again to count a table's pages for the model: past it, its own order.
const MAX_ROWS: usize = 200_000;

/// chronos.follow_pg_plan's description, as SHOW ALL and pg_settings give it.
pub(crate) const FOLLOW_SETTING_ABOUT: &str = "Joins tables in the order PostgreSQL 16 would choose where its plan can only be nested loops (cross joins and joins on conditions no hash or merge join uses), so the rows come out in its order; the same rows are read in any order, though the work done can differ. A table loaded by COPY, or with rows deleted or updated (or inserts rolled back), takes pages in PostgreSQL that Chronos doesn't count, and may be ordered differently.";

/// A setting's value as SHOW writes it, for the running session (its default outside one).
fn guc(name: &str) -> Option<String> {
    super::session_setting(name).or_else(|| settings::find(name).map(|d| d.default_text()))
}

fn guc_bool(name: &str) -> Option<bool> {
    settings::parse_bool(&guc(name)?)
}

fn guc_f64(name: &str) -> Option<f64> {
    guc(name)?.trim().parse().ok()
}

pub(crate) fn guc_i64(name: &str) -> Option<i64> {
    guc(name)?.trim().parse().ok()
}

/// A memory setting in kB (SHOW writes `4MB`).
fn guc_kb(name: &str) -> Option<i64> {
    let v = guc(name)?;
    let v = v.trim();
    let (n, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
    let n: i64 = n.parse().ok()?;
    Some(match unit.trim() {
        "" | "kB" => n,
        "MB" => n * 1024,
        "GB" => n * 1024 * 1024,
        "TB" => n * 1024 * 1024 * 1024,
        _ => return None,
    })
}

/// Whether the executor follows Postgres's join order (chronos.follow_pg_plan: on by default in exact mode).
pub(crate) fn following() -> bool {
    match super::session_setting("chronos.follow_pg_plan").as_deref().and_then(settings::parse_bool) {
        Some(on) => on,
        None => super::pg_exact(),
    }
}

/// The settings the model reads; None when one it doesn't follow is set away from Postgres's default.
/// The pages from which a table gets a parallel scan (compute_parallel_worker, allpaths.c: none below
/// min_parallel_table_scan_size); with max_parallel_workers_per_gather 0, none ever (u64::MAX).
fn parallel_pages() -> Option<u64> {
    if guc_i64("max_parallel_workers_per_gather")? <= 0 {
        return Some(u64::MAX);
    }
    Some((guc_kb("min_parallel_table_scan_size")? / 8).max(0) as u64)
}

fn planner_settings(n: usize) -> Option<Settings> {
    // join_collapse_limit and from_collapse_limit cut a longer FROM list into sub-problems (deconstruct_recurse,
    // initsplan.c), though two tables are one list however they are split; GEQO searches randomly from
    // geqo_threshold relations on
    if n > 2 && n as i64 > guc_i64("join_collapse_limit")?.min(guc_i64("from_collapse_limit")?) {
        return None;
    }
    if guc_bool("geqo")? && n as i64 >= guc_i64("geqo_threshold")? {
        return None;
    }
    Some(Settings {
        seq_page_cost: guc_f64("seq_page_cost")?,
        cpu_tuple_cost: guc_f64("cpu_tuple_cost")?,
        cpu_operator_cost: guc_f64("cpu_operator_cost")?,
        work_mem: guc_kb("work_mem")?,
        enable_seqscan: guc_bool("enable_seqscan")?,
        enable_material: guc_bool("enable_material")?,
        enable_nestloop: guc_bool("enable_nestloop")?,
    })
}

// ---------------------------------------------------------------- pg_operator and pg_proc

/// A built-in operator's planner facts (see `builtin_opers.tsv`).
#[derive(Clone, Debug)]
struct OperRow {
    rest: &'static str,
    join: &'static str,
    canhash: bool,
    canmerge: bool,
    code: i64,
    negate: i64,
    com: i64,
}

/// pg_operator's rows by (name, left type, right type).
static OPERS: LazyLock<std::collections::HashMap<(&'static str, i64, i64), OperRow>> = LazyLock::new(|| {
    let lines = include_str!("../builtin_opers.tsv").lines().filter(|l| !l.starts_with('#') && !l.is_empty());
    lines
        .filter_map(|l| {
            let f: Vec<&'static str> = l.split('\t').collect();
            let int = |i: usize| f.get(i).and_then(|x| x.parse::<i64>().ok());
            let name = |i: usize| f.get(i).map(|x| if *x == "-" { "" } else { *x });
            let row = OperRow {
                rest: name(6)?,
                join: name(7)?,
                canhash: *f.get(8)? == "t",
                canmerge: *f.get(9)? == "t",
                code: int(10)?,
                negate: int(11)?,
                com: int(12)?,
            };
            Some(((*f.get(1)?, int(2)?, int(3)?), row))
        })
        .collect()
});

/// pg_proc.procost of built-in function `oid` (see `builtin_procs.tsv`).
fn procost(oid: i64) -> Option<f64> {
    catalog_schema::builtin_proc_cost(oid)
}

// ---------------------------------------------------------------- pulled-up subqueries

thread_local! {
    /// The SELECT depths whose query Postgres would pull up into the statement's own join tree (see
    /// `PulledUp`): their joins are planned as the statement's are.
    static PULLED: RefCell<Vec<u32>> = const { RefCell::new(vec![]) };
}

/// While a subquery in FROM that Postgres would pull up runs (see `pulls_up`): its SELECT's depth is planned as
/// the statement's own.
pub(crate) struct PulledUp(bool);

impl PulledUp {
    pub(crate) fn enter(on: bool) -> PulledUp {
        if on {
            PULLED.with_borrow_mut(|p| p.push(SelectDepth::current() + 1));
        }
        PulledUp(on)
    }
}

impl Drop for PulledUp {
    fn drop(&mut self) {
        if self.0 {
            PULLED.with_borrow_mut(|p| {
                p.pop();
            });
        }
    }
}

/// Whether the running SELECT is planned as the statement's own: the statement's top query, or a subquery
/// pulled up into it.
fn planned_whole() -> bool {
    let depth = SelectDepth::current();
    depth <= 1 || PULLED.with_borrow(|p| p.contains(&depth))
}

/// Whether Postgres would pull subquery `q`, the only FROM item of `s`, up into `s` (pull_up_subqueries,
/// prepjointree.c's is_simple_subquery) with nothing of `s`'s own to plan with it: `s` is planned whole and
/// only reads it, and `q` is a plain join of tables with no grouping, sorting, limit or set operation.
pub(crate) fn pulls_up(s: &Select, q: &Select) -> bool {
    planned_whole()
        && s.joins.is_empty()
        && s.filter.is_none()
        && s.group.is_empty()
        && s.having.is_none()
        && !s.distinct
        && s.distinct_on.is_empty()
        && s.set.is_empty()
        && s.limit.is_none()
        && s.offset.is_none()
        && q.set.is_empty()
        && q.group.is_empty()
        && q.having.is_none()
        && !q.distinct
        && q.distinct_on.is_empty()
        && q.order.is_empty()
        && q.limit.is_none()
        && q.offset.is_none()
}

// ---------------------------------------------------------------- the query, in Postgres's terms

/// What the join step knows of a statement's FROM tables.
pub(crate) struct Join<'a, 'v> {
    pub s: &'a Select,
    pub p: &'a Plan,
    pub tables: &'a [Table],
    pub views: &'a [View<'v>],
    /// the WHERE's conditions, and each JOIN's ON's
    pub conds: &'a [&'a Expr],
    pub ons: &'a [Vec<&'a Expr>],
    pub types: &'a [Option<Type>],
}

/// The order Postgres 16 would join `j`'s tables in (their places in the FROM list, outermost first), where its
/// plan can only be nested loops and the model has every input; None otherwise.
pub(crate) fn join_order(j: &Join<'_, '_>) -> super::Result<Option<Vec<usize>>> {
    let n = j.tables.len();
    if !(2..=31).contains(&n) || !planned_whole() {
        return Ok(None);
    }
    let Some(settings) = planner_settings(n) else { return Ok(None) };
    // inner joins of plain tables, with nothing asking for a fast start
    let s = j.s;
    if s.limit.is_some() || s.offset.is_some() {
        return Ok(None);
    }
    if s.joins.iter().any(|x| !matches!(x.kind, JoinKind::Inner | JoinKind::Cross)) {
        return Ok(None);
    }
    let srcs: Vec<&super::Source> = s.from.iter().chain(s.joins.iter().map(|x| &x.src)).collect();
    for src in &srcs {
        if src.sub.is_some()
            || src.rec.is_some()
            || src.working
            || src.at.is_some()
            || src.lateral
            || src.srf.is_some()
            || src.hier
            || src.cte != 0
        {
            return Ok(None);
        }
    }
    // the conditions, as distribute_qual_to_rels sees them: the ON's (the JoinExprs below the FROM list are
    // processed first), then the WHERE's
    let quals: Vec<&Expr> = j.ons.iter().flatten().copied().chain(j.conds.iter().copied()).collect();
    if !super::no_subs(quals.iter().copied()) {
        return Ok(None);
    }
    let conv = Conv { j };
    // the tables, as far as their definitions and statistics say (their rows are read last, below)
    let mut facts: Vec<Facts> = vec![];
    for (k, t) in j.tables.iter().enumerate() {
        let Some(f) = conv.facts(k, t)? else { return Ok(None) };
        facts.push(f);
    }
    let rel_stats: Vec<Option<RelStats>> = facts.iter().map(|f| f.stats.clone()).collect();
    let mut restricts: Vec<Vec<Restrict>> = vec![vec![]; n];
    let mut joins: Vec<Restrict> = vec![];
    for e in &quals {
        let relids = conv.relids(e);
        let Some(node) = conv.node(e, &rel_stats) else { return Ok(None) };
        // a clause a hash or merge join could use (an equivalence class's, too) isn't modelled
        if let Node::Op(op) = &node
            && op.op.joinable
        {
            return Ok(None);
        }
        let mut per_tuple = 0.0;
        if conv.cost(e, &settings, &mut per_tuple).is_none() {
            return Ok(None);
        }
        let r = Restrict { node, relids, cost: QualCost { startup: 0.0, per_tuple } };
        match relids.count_ones() {
            0 => return Ok(None), // a gating qual
            1 => restricts[relids.trailing_zeros() as usize].push(r),
            _ => joins.push(r),
        }
    }
    // the Vars each rel emits (add_vars_to_targetlist): those the query's output reads, needed at the top, and
    // those a join clause reads, needed where it is checked
    let mut after = vec![false; j.p.cols.len()];
    j.p.mark_after(&mut after);
    let mut needed: Vec<Relids> = after.iter().map(|&a| if a { FINAL } else { 0 }).collect();
    for e in &quals {
        let relids = conv.relids(e);
        if relids.count_ones() > 1 {
            super::each_idx(e, &mut |i| needed[i] |= relids);
        }
    }
    let mut vars = vec![];
    for (i, &need) in needed.iter().enumerate() {
        if need == 0 {
            continue;
        }
        let k = j.p.starts.iter().rposition(|&st| st <= i).unwrap_or(0);
        let col = &j.p.cols[i];
        let Some(width) = attr_width(col, rel_stats[k].as_ref()) else { return Ok(None) };
        vars.push(Var { rel: k, width: width as i32, needed: need });
    }
    // estimate_rel_size (plancat.c): what the statistics say, and the pages each table has now
    let Some(parallel_pages) = parallel_pages() else { return Ok(None) };
    let mut rels: Vec<BaseRel> = vec![];
    for ((k, t), (f, restrict)) in j.tables.iter().enumerate().zip(facts.iter().zip(restricts)) {
        let Some(size) = conv.size(k, t, f)? else { return Ok(None) };
        let (pages, tuples, _) = cost::estimate_rel_size(&size);
        // a table big enough for a parallel scan has partial paths and Gather paths too (create_plain_partial_paths,
        // compute_parallel_worker), which this doesn't model
        if pages >= parallel_pages as f64 {
            return Ok(None);
        }
        rels.push(BaseRel { pages, tuples, restrict, spc_seq_page_cost: settings.seq_page_cost });
    }
    let q = Query { rels, joins, vars, settings };
    let Some(best) = joinsearch::best_path(&q) else { return Ok(None) };
    let mut order = vec![];
    best.leaves(&mut order);
    Ok((order.len() == n).then_some(order))
}

/// set_rel_width's width of a column (costsize.c:5874): stawidth where ANALYZE found it, else get_typavgwidth's
/// guess from its type; None when ANALYZE sampled the table (its stawidth not known here).
fn attr_width(c: &Col, stats: Option<&RelStats>) -> Option<i64> {
    match stats.and_then(|s| s.width(c.id)) {
        Some(-1) => None,
        Some(w) => Some(w),
        None => {
            let (oid, typmod) = pg_type(c);
            let typlen = match c.ty {
                Type::Enum(_) => 4,
                _ => catalog_schema::type_facts(oid)?.0,
            };
            Some(cost::get_typavgwidth(oid, typlen, typmod))
        }
    }
}

/// Whether Postgres's ALTER TABLE `changes` rewrite table `before` into a new heap (ATRewriteTables, tablecmds.c:5534)
/// to make it `after`: Some(true) where it surely does, Some(false) where it surely doesn't, None where this doesn't
/// follow its rules far enough to say (an ADD COLUMN, whose default may be volatile, SET LOGGED or UNLOGGED, a
/// type change through casts, a domain's or an array's).
pub(crate) fn rewrites(before: &Table, after: &Table, changes: &[super::Alter]) -> Option<bool> {
    use super::Alter;
    let mut out = Some(false);
    for c in changes {
        let this = match c {
            Alter::AddColumn(..) | Alter::Unlogged(_) => None,
            Alter::SetType(name, _, _, via, using) => {
                let old = before.cols.iter().find(|x| x.name == *name)?;
                let new = after.cols.iter().find(|x| x.id == old.id)?;
                retypes(pg_type(old), pg_type(new), !via.is_empty(), using.is_some())
            }
            _ => Some(false),
        };
        out = match (out, this) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (None, _) | (_, None) => None,
            _ => Some(false),
        };
    }
    out
}

/// Whether a column's change of type from `old` to `new` (oid and typmod) needs a rewrite, as ATPrepAlterColumnType
/// works it out: the column coerced to the new type (`cast`: through a USING's casts; `using`: by another USING
/// expression), simplified (the length coercions' support functions, varchar_support and numeric_support), and
/// ATColumnChangeRequiresRewrite (tablecmds.c:12728) asked of what is left: none for the column itself or a
/// RelabelType of it, one for any function or I/O conversion.
fn retypes((o1, m1): (i64, i64), (o2, m2): (i64, i64), cast: bool, using: bool) -> Option<bool> {
    if using {
        return Some(true);
    }
    if cast {
        return None;
    }
    if (o1, m1) == (o2, m2) {
        return Some(false);
    }
    if !super::catalog::builtin_scalar(o1) || !super::catalog::builtin_scalar(o2) {
        return None;
    }
    // numeric's typmod: ((precision << 16) | scale) + VARHDRSZ, valid from VARHDRSZ (numeric.c)
    let valid = |m: i64| m >= 4;
    let precision = |m: i64| ((m - 4) >> 16) & 0xffff;
    let scale = |m: i64| (((m - 4) & 0x7ff) ^ 1024) - 1024;
    match (o1 == o2, o2) {
        // varchar_support (varchar.c:566)
        (true, 1043) => Some(!(m2 < 0 || (m1 >= 0 && m1 <= m2))),
        // numeric_support (numeric.c:1193)
        (true, 1700) => Some(!(!valid(m2) || (valid(m1) && scale(m2) == scale(m1) && precision(m2) >= precision(m1)))),
        // bpchar's length coercion has no support function: it stays a call
        (true, 1042) => Some(true),
        (true, _) => None,
        // timestamp to timestamptz and back: none when the time zone is UTC (TimestampTimestampTzRequiresRewrite)
        (false, 1114 | 1184) if matches!(o1, 1114 | 1184) => None,
        // a relabeling, unless the new typmod needs a length coercion on top of it (whose source's typmod, the
        // relabeled value's, is -1, which no support function takes away)
        (false, _) if super::catalog::relabels(o1, o2) => Some(m2 >= 0),
        (false, _) => Some(true),
    }
}

/// A column's Postgres type oid and typmod.
fn pg_type(c: &Col) -> (i64, i64) {
    let (own, typmod) = col_typmod(c);
    (if own != 0 { own } else { i64::from(c.ty.oid()) }, typmod)
}

/// A table the model follows: its row layout and its statistics (None: never vacuumed or analyzed).
struct Facts {
    layout: heap::Layout,
    stats: Option<RelStats>,
}

/// Turns Chronos's bound expressions into the planner's clauses.
struct Conv<'a, 'v> {
    j: &'a Join<'a, 'v>,
}

impl Conv<'_, '_> {
    fn relids(&self, e: &Expr) -> Relids {
        refs(e, &self.j.p.starts).into_iter().fold(0, |r, k| r | (1 << k))
    }

    /// What table `k`'s definition and statistics say; None where the model doesn't follow it.
    fn facts(&self, k: usize, t: &Table) -> super::Result<Option<Facts>> {
        let v = &self.j.views[k];
        // a plain heap table: no index of any kind (an index path, an index-only scan, a unique index's
        // estimates), no row security, no inheritance, no statistics objects, the default tablespace
        // (a temporary table is read into memory from the session's own database, where it is kept under its own
        // name, without the pg_temp schema the session reads it by)
        let own = t.name.strip_prefix("pg_temp.").unwrap_or(&t.name);
        let temp = t.mem.is_some() && super::has_temp(own);
        if !t.schema
            || t.past.is_some()
            || (t.mem.is_some() && !temp)
            || t.matview.is_some()
            || t.key.is_some()
            || !t.pk.is_empty()
            || !t.uniques.is_empty()
            || !t.indexes.is_empty()
            || !t.excludes.is_empty()
            || !t.ann.is_empty()
            || t.hier.part.is_some()
            || t.hier.of.is_some()
            || t.hier.kids
            || !t.hier.parents.is_empty()
            || t.hier.foreign.is_some()
            || t.hier.tablespace != 0
            // (parallel_workers asks for a parallel scan of a table of any size)
            || t.hier.reloptions.iter().any(|o| o.starts_with("parallel_workers="))
            || super::rls::flags(v, &t.name)?.0
            || super::statistics::load(v)?.iter().any(|o| o.table == t.name)
        {
            return Ok(None);
        }
        let Some(layout) = heap::Layout::of(t) else { return Ok(None) };
        // a temporary table's statistics are in the session's own database, with it
        let stats = if temp {
            match super::session_temp_view(|tv| stats::load(tv, own))? {
                Some(s) => s,
                None => return Ok(None),
            }
        } else {
            stats::load(v, &t.name)?
        };
        if stats.as_ref().is_some_and(|s| !s.modelled || s.heap_unknown) {
            return Ok(None);
        }
        Ok(Some(Facts { layout, stats }))
    }

    /// Table `k`'s size, as estimate_rel_size reads it: its statistics, and the pages it has now
    /// (RelationGetNumberOfBlocks), counted from its rows; None where they can't be.
    fn size(&self, k: usize, t: &Table, f: &Facts) -> super::Result<Option<RelSize>> {
        let (v, layout, stats) = (&self.j.views[k], &f.layout, &f.stats);
        let mut count = heap::Count::new(layout);
        let mut ok = true;
        let mut rows = 0usize;
        let mut add = |vals: &[Value]| {
            rows += 1;
            ok = ok && rows <= MAX_ROWS && count.add(layout, vals, false);
            ok
        };
        match &t.mem {
            Some(mem) => {
                for r in mem.iter() {
                    if !add(r) {
                        break;
                    }
                }
            }
            None => {
                let mut t2 = t.clone();
                super::scan(v, &mut t2, super::Access::All, None, |_, _, vals| Ok(add(vals)))?;
            }
        }
        if !ok {
            return Ok(None);
        }
        let curpages = count.pages.pages as i64;
        let st = stats.clone().unwrap_or_default();
        // get_rel_data_width (plancat.c:1162), which the density is worked out from when the statistics give
        // none: each attribute's width, stawidth where ANALYZE found one
        let mut data_width = 0;
        if st.reltuples < 0.0 || st.relpages <= 0 {
            for &jc in layout.columns() {
                match attr_width(&t.cols[jc], stats.as_ref()) {
                    Some(w) => data_width += w,
                    None => return Ok(None),
                }
            }
        }
        Ok(Some(RelSize {
            relpages: st.relpages,
            reltuples: st.reltuples,
            relallvisible: st.relallvisible,
            hassubclass: false,
            curpages,
            data_width,
        }))
    }

    /// The column an Idx is, and its table.
    fn column(&self, i: usize) -> (usize, &Col) {
        let k = self.j.p.starts.iter().rposition(|&st| st <= i).unwrap_or(0);
        (k, &self.j.p.cols[i])
    }

    /// Expression `e`'s Postgres type oid (exprType).
    fn type_of(&self, e: &Expr) -> Option<i64> {
        if let Expr::Idx(i) = e {
            return Some(pg_type(self.column(*i).1).0);
        }
        let t = ty_in(e, &self.j.p.cols, self.j.types, &[]).ok()?;
        Some(i64::from(t.oid()))
    }

    /// A clause, as eval_const_expressions and canonicalize_qual leave it: None for one the estimators don't
    /// follow.
    fn node(&self, e: &Expr, stats: &[Option<RelStats>]) -> Option<Node> {
        match e {
            // (a top-level AND is split into its conditions before this; one inside an OR is canonicalize_qual's
            // to rearrange, which this doesn't follow)
            Expr::Bin(_, Op::And, _) => None,
            Expr::Bin(_, Op::Or, _) => {
                let mut arms = vec![];
                let mut stack = vec![e];
                while let Some(x) = stack.pop() {
                    match x {
                        Expr::Bin(a, Op::Or, b) => {
                            stack.push(b);
                            stack.push(a);
                        }
                        x => arms.push(self.node(x, stats)?),
                    }
                }
                // an OR over several rels: extract_restriction_or_clauses may add restrictions it implies
                if self.relids(e).count_ones() > 1 {
                    return None;
                }
                Some(Node::Or(arms))
            }
            Expr::IsNull(x, negated) => Some(Node::NullTest { arg: self.arg(x, stats)?, null: !*negated }),
            Expr::Bin(a, op, b) => {
                let sym = op_symbol(*op)?;
                if !matches!(sym, "=" | "<>" | "<" | "<=" | ">" | ">=") {
                    return None;
                }
                let op = self.oper(sym, a, b)?;
                Some(Node::Op(OpClause { op, args: [self.arg(a, stats)?, self.arg(b, stats)?] }))
            }
            Expr::Func(Func::Scalar(funcs::Scalar::GeoOp), args) if args.len() == 5 => {
                let op = self.geo_oper(args)?;
                Some(Node::Op(OpClause { op, args: [self.arg(&args[0], stats)?, self.arg(&args[1], stats)?] }))
            }
            _ => None,
        }
    }

    /// The operator `a sym b` resolves to, as the parser's oper() would pick it among the built-in ones: the one
    /// for both types, or with a constant on one side, the other side's type for both (the constant is coerced
    /// and folded).
    fn oper(&self, sym: &str, a: &Expr, b: &Expr) -> Option<Oper> {
        let (ta, tb) = (self.type_of(a)?, self.type_of(b)?);
        // varchar reads as text for its operators (a binary-compatible relabeling)
        let relabel = |t: i64| if t == 1043 { 25 } else { t };
        let (ta, tb) = (relabel(ta), relabel(tb));
        let found = OPERS.get(&(sym, ta, tb)).or_else(|| match (is_const(a), is_const(b)) {
            (false, true) if lower(tb, ta) => OPERS.get(&(sym, ta, ta)),
            (true, false) if lower(ta, tb) => OPERS.get(&(sym, tb, tb)),
            _ => None,
        })?;
        Some(oper_of(found))
    }

    /// A geometric operator, by its symbol and operand types (`GeoOp`'s arguments).
    fn geo_oper(&self, args: &[Expr]) -> Option<Oper> {
        let (Expr::Lit(Value::Text(sym)), Expr::Lit(Value::Int(ta)), Expr::Lit(Value::Int(tb))) =
            (&args[2], &args[3], &args[4])
        else {
            return None;
        };
        let found = OPERS.iter().find(|((n, l, r), _)| *n == sym.as_str() && l == ta && r == tb)?;
        Some(oper_of(found.1))
    }

    /// An operator's argument, as examine_variable finds it.
    fn arg(&self, e: &Expr, stats: &[Option<RelStats>]) -> Option<Arg> {
        let relids = self.relids(e);
        // a parameter or anything volatile isn't modelled, nor a system column (ctid, tableoid, xmin: selfuncs.c's
        // scalarineqsel and get_variable_numdistinct have paths of their own for them)
        let mut odd = false;
        super::walk(e, &mut |x| {
            odd |= matches!(x, Expr::Param(_))
                || super::node_varies(x, &|_| true)
                || matches!(x, Expr::Idx(i) if self.j.p.cols.get(*i).is_none_or(|c| c.hidden))
        });
        if odd {
            return None;
        }
        let has_stats = match e {
            Expr::Idx(i) => {
                let (k, c) = self.column(*i);
                stats[k].as_ref().is_some_and(|s| s.has_stats(c.id))
            }
            _ => false,
        };
        let konst = is_const(e).then(|| const_null(e));
        Some(Arg { relids, stats: has_stats, konst, vartype: self.type_of(e)?, key: format!("{e:?}") })
    }

    /// cost_qual_eval_walker (costsize.c:4425) over `e`: each operator's procost times cpu_operator_cost,
    /// in the order the walker meets them (an operator before its arguments). None for a node it doesn't follow.
    fn cost(&self, e: &Expr, s: &Settings, acc: &mut f64) -> Option<()> {
        match e {
            // Vars and Consts cost nothing; nor do AND and OR, nor a constant folded at plan time
            Expr::Idx(_) | Expr::Lit(_) => Some(()),
            _ if is_const(e) => Some(()),
            Expr::Bin(a, Op::And | Op::Or, b) => {
                self.cost(a, s, acc)?;
                self.cost(b, s, acc)
            }
            Expr::Bin(a, op, b) => {
                let o = self.oper(op_symbol(*op)?, a, b)?;
                *acc += procost(o.code)? * s.cpu_operator_cost;
                self.cost(a, s, acc)?;
                self.cost(b, s, acc)
            }
            Expr::Func(Func::Scalar(funcs::Scalar::GeoOp | funcs::Scalar::GeoOpF | funcs::Scalar::GeoOpV), args)
                if args.len() == 5 =>
            {
                let o = self.geo_oper(args)?;
                *acc += procost(o.code)? * s.cpu_operator_cost;
                self.cost(&args[0], s, acc)?;
                self.cost(&args[1], s, acc)
            }
            // a subscript (an array's element, a point's coordinate, a geometric value's point, whose type's id the
            // binder adds third): a SubscriptingRef, which costs nothing of its own
            Expr::Func(Func::Scalar(funcs::Scalar::ArrayAt), args) if matches!(args.len(), 2 | 3) => {
                self.cost(&args[0], s, acc)?;
                self.cost(&args[1], s, acc)
            }
            // (a geometric subscript's value as the binder types it: the point's coordinate already is a float8)
            Expr::Cast(x, _) if geo_subscript(x) => self.cost(x, s, acc),
            // a NullTest costs what its argument does
            Expr::IsNull(x, _) => self.cost(x, s, acc),
            _ => None,
        }
    }
}

fn oper_of(r: &OperRow) -> Oper {
    Oper { rest: r.rest, join: r.join, negate: r.negate, com: r.com, code: r.code, joinable: r.canhash || r.canmerge }
}

/// Whether `e` is a subscript of a geometric value (`p[0]`, a point's coordinate), which the binder gives its
/// value's type to with a cast (see `geom::subscript`): Postgres's SubscriptingRef has that type itself.
fn geo_subscript(e: &Expr) -> bool {
    matches!(e, Expr::Func(Func::Scalar(funcs::Scalar::ArrayAt), args) if args.len() == 3)
}

/// A constant once eval_const_expressions has folded it: a literal, or a cast or negation of one.
fn is_const(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) => true,
        Expr::Cast(x, _) | Expr::Neg(x) => is_const(x),
        _ => false,
    }
}

fn const_null(e: &Expr) -> bool {
    match e {
        Expr::Lit(v) => matches!(v, Value::Null),
        Expr::Cast(x, _) | Expr::Neg(x) => const_null(x),
        _ => false,
    }
}

/// Whether a constant of type `c` is coerced to type `t` (an implicit cast up the numeric types: int2, int4,
/// int8, numeric, float4, float8), rather than `t`'s side to `c`'s, when no operator takes both.
fn lower(c: i64, t: i64) -> bool {
    let rank = |o: i64| [21, 23, 20, 1700, 700, 701].iter().position(|&x| x == o);
    matches!((rank(c), rank(t)), (Some(a), Some(b)) if a < b)
}
