//! Selectivity of WHERE and JOIN clauses as Postgres 16's planner estimates it without statistics:
//! clauselist_selectivity's product with its range pairs (`x > a AND x < b` estimated together), AND, OR and
//! IS NULL, and the operators' restriction (oprrest) and join (oprjoin) estimators on the paths they take when
//! no pg_statistic row describes a column. A clause whose estimate would read a statistics row, or whose
//! estimator isn't one of these, has no answer here (`None`), and the planner's choice isn't followed.
//!
//! Ported from PostgreSQL 16 (src/backend/optimizer/path/clausesel.c, src/backend/utils/adt/selfuncs.c and
//! geo_selfuncs.c, src/include/utils/selfuncs.h), under the PostgreSQL License:
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

use super::Relids;
use super::cost::clamp_row_est;

/// selfuncs.h's defaults.
pub(crate) const DEFAULT_EQ_SEL: f64 = 0.005;
pub(crate) const DEFAULT_INEQ_SEL: f64 = 0.3333333333333333;
pub(crate) const DEFAULT_RANGE_INEQ_SEL: f64 = 0.005;
pub(crate) const DEFAULT_MATCHING_SEL: f64 = 0.010;
pub(crate) const DEFAULT_NUM_DISTINCT: f64 = 200.0;
pub(crate) const DEFAULT_UNK_SEL: f64 = 0.005;
pub(crate) const DEFAULT_NOT_UNK_SEL: f64 = 1.0 - DEFAULT_UNK_SEL;

/// BOOLOID (pg_type.dat).
const BOOLOID: i64 = 16;

/// A clause, as the planner holds it after eval_const_expressions and canonicalize_qual.
#[derive(Clone, Debug)]
pub(crate) enum Node {
    /// an OpExpr of two arguments
    Op(OpClause),
    Or(Vec<Node>),
    /// NullTest: `arg IS NULL` (or with `null` false, IS NOT NULL)
    NullTest {
        arg: Arg,
        null: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct OpClause {
    pub op: Oper,
    pub args: [Arg; 2],
}

/// An operator's pg_operator facts the planner reads.
#[derive(Clone, Debug)]
pub(crate) struct Oper {
    /// oprrest's and oprjoin's function names ("" for none)
    pub rest: &'static str,
    pub join: &'static str,
    /// oprnegate's and oprcom's (0 for none)
    pub negate: i64,
    pub com: i64,
    /// oprcode's oid
    pub code: i64,
    /// oprcanhash or oprcanmerge: a hash or merge join could use it
    pub joinable: bool,
}

/// An operator's argument, as examine_variable (selfuncs.c:4988) finds it.
#[derive(Clone, Debug)]
pub(crate) struct Arg {
    /// the base rels it reads (pull_varnos)
    pub relids: Relids,
    /// a pg_statistic row would describe it (a column of an analyzed table that had rows)
    pub stats: bool,
    /// a Const (after estimate_expression_value), and whether it is null
    pub konst: Option<bool>,
    /// its type's oid (exprType)
    pub vartype: i64,
    /// equal()'s test: two arguments with the same text are the same expression
    pub key: String,
}

/// What the estimators read of the query: each base rel's tuples (RelOptInfo.tuples).
pub(crate) trait Rels {
    fn tuples(&self, rel: usize) -> f64;
}

/// CLAMP_PROBABILITY (selfuncs.h).
fn clamp_probability(p: f64) -> f64 {
    p.clamp(0.0, 1.0)
}

fn num_rels(r: Relids) -> u32 {
    r.count_ones()
}

/// clauselist_selectivity (clausesel.c:101) of a list of clauses (RestrictInfos), with varRelid 0 and the
/// inner join's dummy SpecialJoinInfo; no extended statistics (the tables have none: see `opt`).
pub(crate) fn clauselist_selectivity(rels: &dyn Rels, clauses: &[&Node]) -> Option<f64> {
    // exactly one clause: clause_selectivity_ext directly
    if clauses.len() == 1 {
        return clause_selectivity(rels, clauses[0]);
    }
    let mut s1 = 1.0;
    // the range-query pairs, newest first as addRangeClause links them
    let mut rqlist: Vec<RangeQueryClause> = vec![];
    for clause in clauses {
        let s2 = clause_selectivity(rels, clause)?;
        // a restriction clause with a pseudoconstant on one side
        if let Node::Op(op) = clause {
            let relids = op.args[0].relids | op.args[1].relids;
            let pseudo = |a: &Arg| a.relids == 0;
            let mut varonleft = true;
            let ok = num_rels(relids) == 1
                && (pseudo(&op.args[1]) || {
                    varonleft = false;
                    pseudo(&op.args[0])
                });
            if ok {
                match op.op.rest {
                    "scalarltsel" | "scalarlesel" => add_range_clause(&mut rqlist, op, varonleft, true, s2),
                    "scalargtsel" | "scalargesel" => add_range_clause(&mut rqlist, op, varonleft, false, s2),
                    _ => s1 *= s2,
                }
                continue;
            }
        }
        s1 *= s2;
    }
    // now scan the rangequery pair list (newest first)
    for rq in rqlist.iter().rev() {
        match (rq.lobound, rq.hibound) {
            (Some(lo), Some(hi)) => {
                // exact equality to the default value probably means the selectivity function punted
                let s2 = if hi == DEFAULT_INEQ_SEL || lo == DEFAULT_INEQ_SEL {
                    DEFAULT_RANGE_INEQ_SEL
                } else {
                    let mut s2 = hi + lo - 1.0;
                    // adjust for double-exclusion of NULLs
                    s2 += nulltestsel(rq.stats, true)?;
                    // a zero or slightly negative s2 is roundoff; a very negative one, defaults not recognized
                    if s2 <= 0.0 {
                        s2 = if s2 < -0.01 { DEFAULT_RANGE_INEQ_SEL } else { 1.0e-10 };
                    }
                    s2
                };
                s1 *= s2;
            }
            (Some(lo), None) => s1 *= lo,
            (None, Some(hi)) => s1 *= hi,
            (None, None) => {}
        }
    }
    Some(s1)
}

/// RangeQueryClause (clausesel.c:31).
struct RangeQueryClause {
    var: String,
    /// whether a pg_statistic row describes the variable
    stats: bool,
    lobound: Option<f64>,
    hibound: Option<f64>,
}

/// addRangeClause (clausesel.c:427).
fn add_range_clause(rqlist: &mut Vec<RangeQueryClause>, op: &OpClause, varonleft: bool, is_lt_sel: bool, s2: f64) {
    let (arg, is_lobound) = if varonleft {
        // x < something is high bound
        (&op.args[0], !is_lt_sel)
    } else {
        // something < x is low bound
        (&op.args[1], is_lt_sel)
    };
    let var = &arg.key;
    // (the list is searched from its head, the newest entry)
    if let Some(rq) = rqlist.iter_mut().rev().find(|rq| rq.var == *var) {
        // two similar clauses, such as x < y AND x <= z: keep only the more restrictive one
        let bound = if is_lobound { &mut rq.lobound } else { &mut rq.hibound };
        match bound {
            Some(b) if *b > s2 => *b = s2,
            Some(_) => {}
            None => *bound = Some(s2),
        }
        return;
    }
    rqlist.push(RangeQueryClause {
        var: var.clone(),
        stats: arg.stats,
        lobound: is_lobound.then_some(s2),
        hibound: (!is_lobound).then_some(s2),
    });
}

/// clauselist_selectivity_or (clausesel.c:357).
fn clauselist_selectivity_or(rels: &dyn Rels, clauses: &[Node]) -> Option<f64> {
    let mut s1 = 0.0;
    for c in clauses {
        let s2 = clause_selectivity(rels, c)?;
        s1 = s1 + s2 - s1 * s2;
    }
    Some(s1)
}

/// clause_selectivity_ext (clausesel.c:686), varRelid 0, inner join.
pub(crate) fn clause_selectivity(rels: &dyn Rels, clause: &Node) -> Option<f64> {
    match clause {
        Node::Or(args) => clauselist_selectivity_or(rels, args),
        Node::Op(op) => {
            // treat_as_join_clause: more than one base rel
            let relids = op.args[0].relids | op.args[1].relids;
            if num_rels(relids) > 1 { join_selectivity(rels, op) } else { restriction_selectivity(rels, op) }
        }
        Node::NullTest { arg, null } => nulltestsel(arg.stats, *null),
    }
}

/// restriction_selectivity (plancat.c), by the operator's oprrest.
fn restriction_selectivity(rels: &dyn Rels, op: &OpClause) -> Option<f64> {
    let s = match op.op.rest {
        // no estimator: plancat.c's default
        "" => 0.5,
        "eqsel" => eqsel_internal(rels, op, false)?,
        "neqsel" => eqsel_internal(rels, op, true)?,
        "scalarltsel" | "scalarlesel" | "scalargtsel" | "scalargesel" => scalarineqsel_wrapper(op)?,
        // geo_selfuncs.c
        "areasel" => 0.005,
        "positionsel" => 0.1,
        "contsel" => 0.001,
        _ => return None,
    };
    Some(s)
}

/// join_selectivity (plancat.c), by the operator's oprjoin, for an inner join.
fn join_selectivity(rels: &dyn Rels, op: &OpClause) -> Option<f64> {
    let s = match op.op.join {
        "" => 0.5,
        "eqjoinsel" => eqjoinsel(rels, op)?,
        // neqjoinsel (selfuncs.c:2790): 1 - eqjoinsel with the negator, which reads the same here
        "neqjoinsel" => 1.0 - if op.op.negate != 0 { eqjoinsel(rels, op)? } else { DEFAULT_EQ_SEL },
        // scalarltjoinsel and the like (selfuncs.c:2868)
        "scalarltjoinsel" | "scalarlejoinsel" | "scalargtjoinsel" | "scalargejoinsel" => DEFAULT_INEQ_SEL,
        // geo_selfuncs.c
        "areajoinsel" => 0.005,
        "positionjoinsel" => 0.1,
        "contjoinsel" => 0.001,
        // matchingjoinsel (selfuncs.c:3246)
        "matchingjoinsel" => DEFAULT_MATCHING_SEL,
        _ => return None,
    };
    Some(s)
}

/// get_restriction_variable (selfuncs.c:4859): the variable side (None when both or neither read a rel), whether
/// it is on the left, and the other side.
fn get_restriction_variable(op: &OpClause) -> Option<(&Arg, bool, &Arg)> {
    let [l, r] = &op.args;
    match (l.relids != 0, r.relids != 0) {
        (true, false) => Some((l, true, r)),
        (false, true) => Some((r, false, l)),
        _ => None,
    }
}

/// eqsel_internal (selfuncs.c:236).
fn eqsel_internal(rels: &dyn Rels, op: &OpClause, negate: bool) -> Option<f64> {
    if negate && op.op.negate == 0 {
        return Some(1.0 - DEFAULT_EQ_SEL);
    }
    let Some((var, _, other)) = get_restriction_variable(op) else {
        return Some(if negate { 1.0 - DEFAULT_EQ_SEL } else { DEFAULT_EQ_SEL });
    };
    // var_eq_const (selfuncs.c:295) and var_eq_non_const (selfuncs.c:466) take the same no-statistics path; a
    // null constant is never equal
    if other.konst == Some(true) {
        return Some(0.0);
    }
    if var.stats {
        return None;
    }
    // no ANALYZE stats available: equally common values, as many as get_variable_numdistinct guesses
    let mut selec = 1.0 / get_variable_numdistinct(rels, var).0;
    let nullfrac = 0.0;
    if negate {
        selec = 1.0 - selec - nullfrac;
    }
    Some(clamp_probability(selec))
}

/// scalarineqsel_wrapper (selfuncs.c:1400) with no statistics: DEFAULT_INEQ_SEL however it gets there, or 0
/// against a null constant.
fn scalarineqsel_wrapper(op: &OpClause) -> Option<f64> {
    let Some((var, varonleft, other)) = get_restriction_variable(op) else { return Some(DEFAULT_INEQ_SEL) };
    let Some(null) = other.konst else { return Some(DEFAULT_INEQ_SEL) };
    if null {
        return Some(0.0);
    }
    if !varonleft && op.op.com == 0 {
        return Some(DEFAULT_INEQ_SEL);
    }
    // scalarineqsel: with no statistics (and not a ctid), the default
    if var.stats {
        return None;
    }
    Some(DEFAULT_INEQ_SEL)
}

/// eqjoinsel (selfuncs.c:2240) for an inner join without statistics: eqjoinsel_inner's estimate from the
/// numbers of distinct values alone.
fn eqjoinsel(rels: &dyn Rels, op: &OpClause) -> Option<f64> {
    let [v1, v2] = &op.args;
    if v1.stats || v2.stats {
        return None;
    }
    let (nd1, _) = get_variable_numdistinct(rels, v1);
    let (nd2, _) = get_variable_numdistinct(rels, v2);
    // eqjoinsel_inner (selfuncs.c:2405), no MCV lists
    let (nullfrac1, nullfrac2) = (0.0, 0.0);
    let mut selec = (1.0 - nullfrac1) * (1.0 - nullfrac2);
    if nd1 > nd2 {
        selec /= nd1;
    } else {
        selec /= nd2;
    }
    Some(clamp_probability(selec))
}

/// get_variable_numdistinct (selfuncs.c:5712) without a pg_statistic row: (the estimate, whether it is a
/// default).
fn get_variable_numdistinct(rels: &dyn Rels, a: &Arg) -> (f64, bool) {
    // a boolean: presumably two distinct values
    let stadistinct = if a.vartype == BOOLOID { 2.0 } else { 0.0 };
    if stadistinct > 0.0 {
        return (clamp_row_est(stadistinct), false);
    }
    // the relation's size: a base rel's; none for a constant or for an expression over several rels (a joinrel's
    // tuples are 0)
    let rel = (num_rels(a.relids) == 1).then(|| a.relids.trailing_zeros() as usize);
    let Some(rel) = rel else { return (DEFAULT_NUM_DISTINCT, true) };
    let ntuples = rels.tuples(rel);
    if ntuples <= 0.0 {
        return (DEFAULT_NUM_DISTINCT, true);
    }
    // with no data, ndistinct = ntuples if the table is small, else the default
    if ntuples < DEFAULT_NUM_DISTINCT {
        return (clamp_row_est(ntuples), false);
    }
    (DEFAULT_NUM_DISTINCT, true)
}

/// nulltestsel (selfuncs.c:1698) without statistics (`stats`: a pg_statistic row describes the argument, which
/// this doesn't read).
fn nulltestsel(stats: bool, null: bool) -> Option<f64> {
    if stats {
        return None;
    }
    Some(clamp_probability(if null { DEFAULT_UNK_SEL } else { DEFAULT_NOT_UNK_SEL }))
}
