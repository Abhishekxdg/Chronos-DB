//! The costs Postgres 16's planner gives the paths of a nested-loop join plan: a sequential scan, a Material
//! node and the nested loop itself, with the relation sizes they start from. Each function is a port of the C
//! one it is named after, in the same float operation order, so that two paths that tie or nearly tie in
//! Postgres tie the same way here.
//!
//! Ported from PostgreSQL 16 (src/backend/optimizer/path/costsize.c, src/backend/access/table/tableam.c,
//! src/backend/access/heap/heapam_handler.c, src/backend/utils/cache/lsyscache.c,
//! src/backend/utils/adt/format_type.c and numeric.c), under the PostgreSQL License:
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

use super::heap::{BLCKSZ, PAGE_HEADER, TUPLE_HEADER, maxalign};

/// disable_cost (costsize.c:146).
pub(crate) const DISABLE_COST: f64 = 1.0e10;
/// MAXIMUM_ROWCOUNT (pathnodes.h).
const MAXIMUM_ROWCOUNT: f64 = 1e100;

/// The settings the costs read (costsize.c's globals, as the session has them).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    pub seq_page_cost: f64,
    pub cpu_tuple_cost: f64,
    pub cpu_operator_cost: f64,
    /// in kB
    pub work_mem: i64,
    pub enable_seqscan: bool,
    pub enable_material: bool,
    pub enable_nestloop: bool,
}

impl Default for Settings {
    /// guc_tables.c's boot values: DEFAULT_SEQ_PAGE_COST, DEFAULT_CPU_TUPLE_COST, DEFAULT_CPU_OPERATOR_COST,
    /// work_mem 4MB.
    fn default() -> Settings {
        Settings {
            seq_page_cost: 1.0,
            cpu_tuple_cost: 0.01,
            cpu_operator_cost: 0.0025,
            work_mem: 4096,
            enable_seqscan: true,
            enable_material: true,
            enable_nestloop: true,
        }
    }
}

/// QualCost (pathnodes.h).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct QualCost {
    pub startup: f64,
    pub per_tuple: f64,
}

/// clamp_row_est (costsize.c:203).
pub(crate) fn clamp_row_est(nrows: f64) -> f64 {
    if nrows > MAXIMUM_ROWCOUNT || nrows.is_nan() {
        MAXIMUM_ROWCOUNT
    } else if nrows <= 1.0 {
        1.0
    } else {
        // rint: round half to even, as C's default rounding mode does
        nrows.round_ties_even()
    }
}

/// relation_byte_size (costsize.c:6119).
pub(crate) fn relation_byte_size(tuples: f64, width: i32) -> f64 {
    tuples * ((maxalign(width.max(0) as usize) + maxalign(TUPLE_HEADER)) as f64)
}

/// cost_seqscan (costsize.c:256), unparameterized and not parallel: (startup, total).
pub(crate) fn cost_seqscan(
    s: &Settings,
    pages: f64,
    tuples: f64,
    baserestrictcost: QualCost,
    spc_seq_page_cost: f64,
) -> (f64, f64) {
    let mut startup_cost = 0.0;
    if !s.enable_seqscan {
        startup_cost += DISABLE_COST;
    }
    // disk costs
    let disk_run_cost = spc_seq_page_cost * pages;
    // CPU costs
    let qpqual_cost = baserestrictcost;
    startup_cost += qpqual_cost.startup;
    let cpu_per_tuple = s.cpu_tuple_cost + qpqual_cost.per_tuple;
    let mut cpu_run_cost = cpu_per_tuple * tuples;
    // tlist eval costs are paid per output row, not per tuple scanned: a base rel's reltarget of Vars costs
    // nothing (set_rel_width)
    startup_cost += 0.0;
    cpu_run_cost += 0.0;
    (startup_cost, startup_cost + cpu_run_cost + disk_run_cost)
}

/// cost_material (costsize.c:2425): (startup, total).
pub(crate) fn cost_material(
    s: &Settings,
    input_startup_cost: f64,
    input_total_cost: f64,
    tuples: f64,
    width: i32,
) -> (f64, f64) {
    let startup_cost = input_startup_cost;
    let mut run_cost = input_total_cost - input_startup_cost;
    let nbytes = relation_byte_size(tuples, width);
    let work_mem_bytes = (s.work_mem * 1024) as f64;
    // whether spilling or not, 2x cpu_operator_cost per tuple
    run_cost += 2.0 * s.cpu_operator_cost * tuples;
    if nbytes > work_mem_bytes {
        let npages = (nbytes / BLCKSZ as f64).ceil();
        run_cost += s.seq_page_cost * npages;
    }
    (startup_cost, startup_cost + run_cost)
}

/// What a path costs to scan again (cost_rescan, costsize.c:4273): (rescan startup, rescan total).
pub(crate) enum Rescan {
    /// T_Material: cpu_operator_cost per tuple, and its pages if it spills
    Material { rows: f64, width: i32 },
    /// anything else here (a scan, a nested loop): as much as the first time
    Plain { startup: f64, total: f64 },
}

pub(crate) fn cost_rescan(s: &Settings, r: &Rescan) -> (f64, f64) {
    match *r {
        Rescan::Material { rows, width } => {
            let mut run_cost = s.cpu_operator_cost * rows;
            let nbytes = relation_byte_size(rows, width);
            let work_mem_bytes = (s.work_mem * 1024) as f64;
            if nbytes > work_mem_bytes {
                let npages = (nbytes / BLCKSZ as f64).ceil();
                run_cost += s.seq_page_cost * npages;
            }
            (0.0, run_cost)
        }
        Rescan::Plain { startup, total } => (startup, total),
    }
}

/// JoinCostWorkspace (pathnodes.h), as initial_cost_nestloop fills it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Workspace {
    pub startup_cost: f64,
    pub total_cost: f64,
    pub run_cost: f64,
}

/// A join input as the nested loop's costs read it.
pub(crate) struct Input<'a> {
    pub rows: f64,
    pub startup: f64,
    pub total: f64,
    pub rescan: &'a Rescan,
}

/// initial_cost_nestloop (costsize.c:2964), for an inner join whose inner side isn't known unique.
pub(crate) fn initial_cost_nestloop(s: &Settings, outer: &Input, inner: &Input) -> Workspace {
    let mut startup_cost = 0.0;
    let mut run_cost = 0.0;
    let outer_path_rows = outer.rows;
    // estimate costs to rescan the inner relation
    let (inner_rescan_start_cost, inner_rescan_total_cost) = cost_rescan(s, inner.rescan);
    // cost of source data
    startup_cost += outer.startup + inner.startup;
    run_cost += outer.total - outer.startup;
    if outer_path_rows > 1.0 {
        run_cost += (outer_path_rows - 1.0) * inner_rescan_start_cost;
    }
    let inner_run_cost = inner.total - inner.startup;
    let inner_rescan_run_cost = inner_rescan_total_cost - inner_rescan_start_cost;
    // normal case: the whole inner rel for each outer row
    run_cost += inner_run_cost;
    if outer_path_rows > 1.0 {
        run_cost += (outer_path_rows - 1.0) * inner_rescan_run_cost;
    }
    Workspace { startup_cost, total_cost: startup_cost + run_cost, run_cost }
}

/// final_cost_nestloop (costsize.c:3039), unparameterized and not parallel, for an inner join whose inner side
/// isn't known unique: (startup, total). `restrict` is cost_qual_eval of the join's restrictlist; `rows` the
/// joinrel's.
pub(crate) fn final_cost_nestloop(
    s: &Settings,
    w: &Workspace,
    outer_rows: f64,
    inner_rows: f64,
    restrict: QualCost,
) -> (f64, f64) {
    let mut startup_cost = w.startup_cost;
    let mut run_cost = w.run_cost;
    // protect some assumptions below that rowcounts aren't zero
    let outer_path_rows = if outer_rows <= 0.0 { 1.0 } else { outer_rows };
    let inner_path_rows = if inner_rows <= 0.0 { 1.0 } else { inner_rows };
    if !s.enable_nestloop {
        startup_cost += DISABLE_COST;
    }
    // compute number of tuples processed (not number emitted!)
    let ntuples = outer_path_rows * inner_path_rows;
    // CPU costs
    startup_cost += restrict.startup;
    let cpu_per_tuple = s.cpu_tuple_cost + restrict.per_tuple;
    run_cost += cpu_per_tuple * ntuples;
    // tlist eval costs: a joinrel's target of Vars costs nothing
    startup_cost += 0.0;
    run_cost += 0.0;
    (startup_cost, startup_cost + run_cost)
}

/// HEAP_OVERHEAD_BYTES_PER_TUPLE (heapam_handler.c:2096): MAXALIGN(SizeofHeapTupleHeader) + sizeof(ItemIdData).
const HEAP_OVERHEAD_BYTES_PER_TUPLE: i64 = maxalign(TUPLE_HEADER) as i64 + 4;
/// HEAP_USABLE_BYTES_PER_PAGE (heapam_handler.c:2098): BLCKSZ - SizeOfPageHeaderData.
const HEAP_USABLE_BYTES_PER_PAGE: i64 = (BLCKSZ - PAGE_HEADER) as i64;

/// A table as estimate_rel_size reads it: pg_class's relpages, reltuples and relallvisible, whether it has
/// inheritance children (relhassubclass), the pages it has now (RelationGetNumberOfBlocks), and the width of
/// its tuples' data as get_rel_data_width (plancat.c:1162) works it out.
#[derive(Clone, Debug)]
pub(crate) struct RelSize {
    pub relpages: i64,
    pub reltuples: f64,
    pub relallvisible: i64,
    pub hassubclass: bool,
    pub curpages: i64,
    pub data_width: i64,
}

/// table_block_relation_estimate_size (tableam.c:663) for a heap: (pages, tuples, allvisfrac).
pub(crate) fn estimate_rel_size(r: &RelSize) -> (f64, f64, f64) {
    let mut curpages = r.curpages;
    // if the relation has never yet been vacuumed, a minimum size estimate of 10 pages (unless it has
    // inheritance children)
    if curpages < 10 && r.reltuples < 0.0 && !r.hassubclass {
        curpages = 10;
    }
    if curpages == 0 {
        return (0.0, 0.0, 0.0);
    }
    // estimate number of tuples from previous tuple density
    let density = if r.reltuples >= 0.0 && r.relpages > 0 {
        r.reltuples / r.relpages as f64
    } else {
        // no data: the tuple width from the attributes' types; integer division is intentional
        let tuple_width = r.data_width + HEAP_OVERHEAD_BYTES_PER_TUPLE;
        (HEAP_USABLE_BYTES_PER_PAGE / tuple_width) as f64
    };
    let tuples = (density * curpages as f64).round_ties_even();
    let allvisfrac = if r.relallvisible == 0 || curpages <= 0 {
        0.0
    } else if r.relallvisible as f64 >= curpages as f64 {
        1.0
    } else {
        r.relallvisible as f64 / curpages as f64
    };
    (curpages as f64, tuples, allvisfrac)
}

/// get_typavgwidth (lsyscache.c:2574): `typlen` the type's (its base type's, for a domain).
pub(crate) fn get_typavgwidth(typid: i64, typlen: i64, typmod: i64) -> i64 {
    if typlen > 0 {
        return typlen;
    }
    let maxwidth = type_maximum_size(typid, typmod);
    if maxwidth > 0 {
        // for BPCHAR, the max width is also the only width
        if typid == 1042 {
            return maxwidth;
        }
        if maxwidth <= 32 {
            return maxwidth;
        }
        if maxwidth < 1000 {
            return 32 + (maxwidth - 32) / 2;
        }
        return 32 + (1000 - 32) / 2;
    }
    32
}

/// type_maximum_size (format_type.c:408), in a UTF8 database (pg_encoding_max_length 4).
fn type_maximum_size(type_oid: i64, typemod: i64) -> i64 {
    const VARHDRSZ: i64 = 4;
    if typemod < 0 {
        return -1;
    }
    match type_oid {
        // bpchar, varchar: typemod includes the varlena header and is in characters
        1042 | 1043 => (typemod - VARHDRSZ) * 4 + VARHDRSZ,
        // numeric_maximum_size (numeric.c:950)
        1700 => {
            if typemod < VARHDRSZ {
                return -1;
            }
            let precision = ((typemod - VARHDRSZ) >> 16) & 0xffff;
            let numeric_digits = (precision + 2 * (4 - 1)) / 4;
            // NUMERIC_HDRSZ: VARHDRSZ + sizeof(uint16) + sizeof(int16)
            (VARHDRSZ + 2 + 2) + numeric_digits * 2
        }
        // varbit, bit: typemod is the (max) number of bits
        1562 | 1560 => (typemod + 7) / 8 + 2 * 4,
        _ => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// join.sql's J1_TBL (int, int, text) and J2_TBL (int, int), never vacuumed: 1,200 and 2,260 rows by the
    /// 10-page rule; with J2 outer the nested loop costs 33957.60, with J1 outer 33960.25 (EXPLAIN on a real
    /// Postgres 16.15).
    #[test]
    fn j1_j2_cross_join_costs_as_postgres_does() {
        let s = Settings::default();
        let size = |w| RelSize {
            relpages: 0,
            reltuples: -1.0,
            relallvisible: 0,
            hassubclass: false,
            curpages: 1,
            data_width: w,
        };
        let (p1, t1, _) = estimate_rel_size(&size(4 + 4 + 32));
        let (p2, t2, _) = estimate_rel_size(&size(4 + 4));
        assert_eq!((p1, t1, p2, t2), (10.0, 1200.0, 10.0, 2260.0));
        let s1 = cost_seqscan(&s, p1, t1, QualCost::default(), 1.0);
        let s2 = cost_seqscan(&s, p2, t2, QualCost::default(), 1.0);
        assert_eq!((s1.1, s2.1), (22.0, 32.6));
        let m1 = cost_material(&s, s1.0, s1.1, t1, 40);
        let m2 = cost_material(&s, s2.0, s2.1, t2, 8);
        let (r1, r2) = (Rescan::Material { rows: t1, width: 40 }, Rescan::Material { rows: t2, width: 8 });
        let plain2 = Rescan::Plain { startup: s2.0, total: s2.1 };
        let plain1 = Rescan::Plain { startup: s1.0, total: s1.1 };
        let j2_outer = initial_cost_nestloop(
            &s,
            &Input { rows: t2, startup: s2.0, total: s2.1, rescan: &plain2 },
            &Input { rows: t1, startup: m1.0, total: m1.1, rescan: &r1 },
        );
        let j1_outer = initial_cost_nestloop(
            &s,
            &Input { rows: t1, startup: s1.0, total: s1.1, rescan: &plain1 },
            &Input { rows: t2, startup: m2.0, total: m2.1, rescan: &r2 },
        );
        let a = final_cost_nestloop(&s, &j2_outer, t2, t1, QualCost::default());
        let b = final_cost_nestloop(&s, &j1_outer, t1, t2, QualCost::default());
        assert_eq!(format!("{:.2} {:.2}", a.1, b.1), "33957.60 33960.25");
    }
}
