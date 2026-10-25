//! The planner's statistics in pg_class (relpages, reltuples, relallvisible), as VACUUM and ANALYZE record them,
//! and which columns ANALYZE gathered a pg_statistic row for, with the average width it found (stawidth). They
//! are a row of the world per table (`_sonos_relstats/<table>`, JSON), beside its schema and apart from it, as
//! maintenance that changes no definition (see `tablespace::Gens`): a merge keeps the world merged into's.
//!
//! A table never vacuumed or analyzed has no row: pg_class shows 0 pages and -1 tuples, as Postgres does, and
//! the planner uses the 10-page rule (see `cost::estimate_rel_size`). TRUNCATE sets them back (pg_statistic's
//! rows stay, as Postgres's do); DROP TABLE removes the row; a renamed table keeps it. UPDATE pg_class SET
//! reltuples (relpages, relallvisible), which tests and tuning scripts use to steer plans, writes it.
//!
//! Pages are the heap model's (see `heap`): where it can't lay a row out, relpages is 0 and the planner doesn't
//! use the table's statistics (`modelled` false). The model is of a heap the rows were inserted into one by one;
//! a table whose heap a statement made otherwise (`heap_unknown`: CREATE TABLE AS's bulk fill, a rewrite) keeps the
//! model's count in pg_class, but the planner doesn't order joins by it. What other statements leave in Postgres's
//! heap isn't marked (see `opt`'s module comment for why): dead tuples of a DELETE or UPDATE, COPY's bulk extension.
//!
//! reltuples is a float4 in pg_class, as Postgres keeps it: every count and every value UPDATE pg_class sets is
//! rounded to one (see `float4`), so that what the planner reads is what Postgres's reads.

use super::super::*;
use super::heap::{Count, Fill, Layout};

/// Where a table's statistics are kept, by its stored name.
pub(crate) const KEY: &str = "_sonos_relstats/";

pub(crate) fn key(table: &str) -> String {
    format!("{KEY}{table}")
}

/// A table's statistics.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RelStats {
    pub relpages: i64,
    pub reltuples: f64,
    pub relallvisible: i64,
    /// relpages is the heap model's count (false: a row it couldn't lay out)
    pub modelled: bool,
    /// the columns (by id) that have a pg_statistic row, each with its stawidth (-1: ANALYZE sampled the
    /// table, so its width is one this didn't work out)
    pub analyzed: Vec<(u32, i64)>,
    /// the table's heap is not one the model follows: CREATE TABLE AS filled it (in bulk, see `bulk_filled`), or
    /// it was rewritten (VACUUM FULL, an ALTER TABLE that rewrites it: see `altered`). Its pages in Postgres are
    /// not the model's, so the planner leaves its joins alone until TRUNCATE gives it a new, empty heap.
    pub heap_unknown: bool,
}

impl Default for RelStats {
    /// What pg_class has for a table never vacuumed or analyzed.
    fn default() -> RelStats {
        RelStats {
            relpages: 0,
            reltuples: -1.0,
            relallvisible: 0,
            modelled: true,
            analyzed: vec![],
            heap_unknown: false,
        }
    }
}

/// A count as pg_class.reltuples (a float4) keeps it: rounded to 32 bits (123456789 is 123456792), as
/// vac_update_relstats's and index_update_stats's `(float4) num_tuples` round it.
pub(crate) fn float4(n: f64) -> f64 {
    f64::from(n as f32)
}

impl RelStats {
    fn from_raw(raw: &str) -> RelStats {
        let j: Json = serde_json::from_str(raw).unwrap_or_default();
        let analyzed = j["analyzed"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((u32::try_from(p.get(0)?.as_u64()?).ok()?, p.get(1)?.as_i64()?)))
            .collect();
        RelStats {
            relpages: j["relpages"].as_i64().unwrap_or(0),
            reltuples: j["reltuples"].as_f64().unwrap_or(-1.0),
            relallvisible: j["relallvisible"].as_i64().unwrap_or(0),
            modelled: j["modelled"].as_bool().unwrap_or(false),
            analyzed,
            // (a row written before this was kept: none had a heap the model didn't follow marked)
            heap_unknown: j["heap_unknown"].as_bool().unwrap_or(false),
        }
    }

    /// The write that keeps them as table `table`'s (a delete when they are a never-vacuumed table's).
    pub(crate) fn write(&self, table: &str) -> (String, Option<String>) {
        if *self == RelStats::default() {
            return (key(table), None);
        }
        let analyzed: Vec<Json> = self.analyzed.iter().map(|(c, w)| serde_json::json!([c, w])).collect();
        let j = serde_json::json!({
            "relpages": self.relpages,
            "reltuples": self.reltuples,
            "relallvisible": self.relallvisible,
            "modelled": self.modelled,
            "analyzed": analyzed,
            "heap_unknown": self.heap_unknown,
        });
        (key(table), Some(j.to_string()))
    }

    /// Whether column `id` has a pg_statistic row.
    pub(crate) fn has_stats(&self, id: u32) -> bool {
        self.analyzed.iter().any(|(c, _)| *c == id)
    }

    /// Column `id`'s stawidth: None without a pg_statistic row (or with 0, an all-null column's), Some(-1) when
    /// not known.
    pub(crate) fn width(&self, id: u32) -> Option<i64> {
        self.analyzed.iter().find(|(c, _)| *c == id).map(|(_, w)| *w).filter(|&w| w != 0)
    }
}

/// Table `table`'s statistics (None: never vacuumed or analyzed).
pub(crate) fn load(v: &View, table: &str) -> Result<Option<RelStats>> {
    Ok(v.get(&key(table))?.map(|raw| RelStats::from_raw(&raw)))
}

/// Every table's, by stored name: what pg_class shows (see `tables`).
pub(crate) fn all(v: &View) -> Result<HashMap<String, RelStats>> {
    let mut out = HashMap::new();
    for r in v.prefix(KEY)? {
        let (k, raw) = r?;
        out.insert(k[KEY.len()..].to_string(), RelStats::from_raw(&raw));
    }
    Ok(out)
}

/// The write that drops table `table`'s statistics with it, if it has any.
pub(crate) fn dropped(v: &View, table: &str) -> Result<Option<(String, Option<String>)>> {
    Ok(v.get(&key(table))?.map(|_| (key(table), None)))
}

/// The writes that move table `from`'s statistics to its new name `to` (RENAME TO, SET SCHEMA).
pub(crate) fn renamed(v: &View, from: &str, to: &str) -> Result<Vec<(String, Option<String>)>> {
    if from == to {
        return Ok(vec![]);
    }
    Ok(match v.get(&key(from))? {
        Some(raw) => vec![(key(from), None), (key(to), Some(raw.to_string()))],
        None => vec![],
    })
}

/// The write TRUNCATE makes: a new, empty heap's pg_class row (RelationSetNewRelfilenumber: 0 pages, -1 tuples,
/// none all-visible), which the model follows again; the pg_statistic rows stay.
pub(crate) fn truncated(v: &View, table: &str) -> Result<Option<(String, Option<String>)>> {
    let Some(old) = load(v, table)? else { return Ok(None) };
    let new = RelStats { analyzed: old.analyzed, ..RelStats::default() };
    Ok(Some(new.write(table)))
}

/// The write that marks table `table`'s heap as one the model doesn't follow (see `RelStats::heap_unknown`), if it
/// isn't marked already: CREATE TABLE AS (and SELECT INTO) filled it with rows, as intorel_receive does with a bulk
/// insert state, whose heap RelationAddBlocks extends by more pages at a time as it grows (1, 1, 2, 4, ... 64,
/// hio.c:302), the last of them maybe empty. The table is new, its statement's own: no other writes this row.
/// What pg_class shows is unchanged.
pub(crate) fn bulk_filled(v: &View, table: &str) -> Result<Option<(String, Option<String>)>> {
    let old = load(v, table)?.unwrap_or_default();
    if old.heap_unknown {
        return Ok(None);
    }
    Ok(Some(RelStats { heap_unknown: true, ..old }.write(table)))
}

/// index_update_stats (index.c:2811) for an index built on table `t` (CREATE INDEX, a constraint's index, REINDEX):
/// see `indexed`. None where nothing changes.
pub(crate) fn index_built(v: &View, t: &mut Table, old: Option<RelStats>) -> Result<Option<RelStats>> {
    let m = measure(v, t, Fill::Insert, &[], 0)?;
    let old = old.unwrap_or_default();
    let new = indexed(old.clone(), &m);
    Ok((new != old).then_some(new))
}

/// index_update_stats's pg_class counts for a table whose rows are `m`: its pages (RelationGetNumberOfBlocks, the
/// model's count) and live tuples (the build's heap scan), and the pages the visibility map has (taken as what the
/// last VACUUM left, no more than the pages there are). An empty table never vacuumed keeps -1 tuples ("creating
/// an index as part of CREATE TABLE doesn't cause the table to prematurely look like it's been vacuumed",
/// index.c:2827).
fn indexed(st: RelStats, m: &Measure) -> RelStats {
    if m.tuples == 0 && st.reltuples < 0.0 {
        return st;
    }
    let (relpages, modelled) = match m.pages {
        Some(p) => (p, true),
        None => (0, false),
    };
    let relallvisible = st.relallvisible.min(relpages);
    RelStats { relpages, reltuples: float4(m.tuples as f64), relallvisible, modelled, ..st }
}

/// What ALTER TABLE leaves of table `t`'s statistics (`old`), its rows counted after it: `rewrite`, whether its heap
/// was rewritten (see `opt::rewrites`: None where that isn't known, and the model doesn't follow the heap after it);
/// `altered`, the columns whose type changed; `built`, whether it built an index. A rewrite makes a new heap whose
/// pg_class counts are a never-vacuumed table's (make_new_heap, then swap_relation_files swaps them in,
/// cluster.c:1216), without pg_statistic rows for the columns altered (ATExecAlterColumnType's RemoveStatistics),
/// filled in bulk; then every index is built again on it (reindex_relation), and each build sets the counts as
/// index_update_stats does. None where nothing changes.
pub(crate) fn altered(
    v: &View,
    t: &mut Table,
    old: Option<RelStats>,
    rewrite: Option<bool>,
    altered: &[u32],
    built: bool,
) -> Result<Option<RelStats>> {
    if rewrite == Some(false) && !built {
        return Ok(None);
    }
    let indexes = t.has_key() || !t.uniques.is_empty() || !t.indexes.is_empty() || !t.excludes.is_empty();
    let builds = built || (rewrite == Some(true) && indexes);
    // (the rows are counted only for an index build; otherwise whether there are any is all it takes. A rewritten
    // heap is filled in bulk: see `Fill::Bulk`)
    let fill = if rewrite == Some(true) { Fill::Bulk } else { Fill::Insert };
    let m = if builds { Some(measure(v, t, fill, &[], 0)?) } else { None };
    let rows = match &m {
        Some(m) => m.tuples > 0,
        None => v.prefix(&format!("{}/", t.name))?.next().is_some(),
    };
    let old = old.unwrap_or_default();
    let mut st = match rewrite {
        Some(true) => {
            let mut analyzed = old.analyzed.clone();
            analyzed.retain(|(c, _)| !altered.contains(c));
            RelStats { analyzed, heap_unknown: rows, ..RelStats::default() }
        }
        None => RelStats { heap_unknown: old.heap_unknown || rows, ..old.clone() },
        Some(false) => old.clone(),
    };
    if let Some(m) = &m {
        st = indexed(st, m);
    }
    Ok((st != old).then_some(st))
}

/// What a read of a table's rows finds of its heap (see `heap::Count`): live tuples, the pages they'd take
/// (None where a row can't be laid out) and, for `widths`, each column's stawidth.
pub(crate) struct Measure {
    pub tuples: u64,
    pub pages: Option<i64>,
    pub widths: Vec<(u32, i64)>,
}

/// Reads table `t`'s rows once, its heap filled as `fill` says; for its columns `analyze` (by id), stawidth as
/// ANALYZE works it out from a sample of `sample` rows: exact when the table has no more rows than that, else not
/// known (-1).
pub(crate) fn measure(v: &View, t: &mut Table, fill: Fill, analyze: &[u32], sample: u64) -> Result<Measure> {
    let layout = Layout::filled(t, fill);
    let mut count = layout.as_ref().map(|l| Count::filled(l, fill));
    let mut tuples = 0u64;
    scan(v, t, Access::All, None, |_, _, vals| {
        tuples += 1;
        let laid = match (&layout, &mut count) {
            (Some(l), Some(c)) => c.add(l, vals, !analyze.is_empty()),
            _ => true,
        };
        if !laid {
            count = None;
        }
        Ok(true)
    })?;
    let pages = count.as_ref().map(|c| c.pages.blocks() as i64);
    let mut widths = vec![];
    for &id in analyze {
        let w = match (&layout, &count) {
            (Some(l), Some(c)) if tuples <= sample => {
                let at = l.columns().iter().position(|&j| t.cols[j].id == id);
                match at {
                    Some(i) => {
                        let (total, nonnull) = c.widths[i];
                        // stawidth = total_width / nonnull_cnt; an all-null column's is 0 (its typlen if fixed)
                        if nonnull > 0 {
                            (total as f64 / nonnull as f64) as i64
                        } else {
                            l.fixed(i).map_or(0, |n| n as i64)
                        }
                    }
                    None => -1,
                }
            }
            _ => -1,
        };
        widths.push((id, w));
    }
    Ok(Measure { tuples, pages, widths })
}

/// What VACUUM and ANALYZE do to a table's statistics: VACUUM (`vacuum`, `full` for VACUUM FULL, which rewrites
/// the heap), ANALYZE (`analyze`, of columns `cols`, all when empty).
#[derive(Clone, Copy, Debug)]
pub(crate) struct How {
    pub vacuum: bool,
    pub full: bool,
    pub analyze: bool,
}

/// VACUUM's and ANALYZE's statistics (`how`) for table `name`, given what it had (`old`): vac_update_relstats's
/// relpages, reltuples and relallvisible (VACUUM FULL's copy_table_data's, cluster.c:1011), and for ANALYZE the
/// columns given a pg_statistic row. With rows read, the live tuples (for a partitioned parent's total).
pub(crate) fn recorded(
    v: &View,
    name: &str,
    how: How,
    cols: &[String],
    old: Option<RelStats>,
) -> Result<(RelStats, u64)> {
    let analyze = how.analyze;
    // (the world's own table: a temporary table of the name is one of its own database's)
    let Some(mut t) = load_world(v, name)? else { return err(NO_TABLE, format!("relation \"{name}\" does not exist")) };
    // the columns ANALYZE looks at: those named, or all; not one whose statistics target is 0
    // (std_typanalyze: minrows = 300 * attstattarget, the table's sample the largest of them)
    let default_target = super::guc_i64("default_statistics_target").unwrap_or(100);
    let mut todo: Vec<u32> = vec![];
    let mut sample = 0;
    if analyze {
        for c in t.cols.iter().filter(|c| !c.hidden) {
            if !cols.is_empty() && !cols.contains(&c.name) {
                continue;
            }
            let target = c.opts.stats.filter(|&s| s >= 0).unwrap_or(default_target);
            if target == 0 {
                continue;
            }
            todo.push(c.id);
            sample = sample.max(300 * target.max(0) as u64);
        }
    }
    // (VACUUM FULL's new heap is filled by raw_heap_insert: see `Fill::Rewrite`)
    let fill = if how.full { Fill::Rewrite } else { Fill::Insert };
    let m = measure(v, &mut t, fill, &todo, sample)?;
    let old = old.unwrap_or_default();
    let (relpages, modelled) = match m.pages {
        Some(p) => (p, true),
        None => (0, false),
    };
    let mut analyzed = old.analyzed.clone();
    // (an empty table gets no pg_statistic rows: the ones it had stay)
    if analyze && m.tuples > 0 {
        for (id, w) in m.widths {
            analyzed.retain(|(c, _)| *c != id);
            analyzed.push((id, w));
        }
        analyzed.sort_by_key(|(c, _)| *c);
    }
    // a page VACUUM saw is all-visible after it; ANALYZE counts what the visibility map has (taken as what the
    // last VACUUM left, no more than the pages there are); VACUUM FULL's new heap has none set
    let relallvisible = match (how.full, how.vacuum) {
        (true, _) => 0,
        (false, true) => relpages,
        (false, false) => old.relallvisible.min(relpages),
    };
    // (a heap the model doesn't follow stays so; VACUUM FULL's rewrite fills pages its own way, raw_heap_insert's,
    // without the free space map)
    let heap_unknown = old.heap_unknown || how.full;
    let reltuples = float4(m.tuples as f64);
    let stats = RelStats { relpages, reltuples, relallvisible, modelled, analyzed, heap_unknown };
    Ok((stats, m.tuples))
}

/// What a merge keeps of table statistics both sides recorded (see `crate::Core`'s merge): the world merged
/// into's, marked as a heap the model doesn't follow when the other side's was (its rewrite merges in with it).
/// None where the world merged into's stands as it is.
pub(crate) fn merged(theirs: Option<&str>, ours: Option<&str>) -> Option<String> {
    let (theirs, ours) = (RelStats::from_raw(theirs?), RelStats::from_raw(ours?));
    if theirs.heap_unknown || !ours.heap_unknown {
        return None;
    }
    let (_, raw) = RelStats { heap_unknown: true, ..theirs }.write("");
    raw
}
