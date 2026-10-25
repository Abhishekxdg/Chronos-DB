//! Postgres 16's join search for a query whose joins are all nested loops: the dynamic programming of
//! standard_join_search over levels of joinrels (join_search_one_level, with make_rels_by_clause_joins and
//! make_rels_by_clauseless_joins), the joinrels' sizes and clause lists (build_join_rel), the nested-loop paths
//! match_unsorted_outer tries (each outer path with the cheapest inner, and with it materialized), and
//! add_path's tournament with its fuzz factors (1.01, then 1.0000000001 to break near-ties).
//!
//! The query it is given has only inner joins and no clause a hash or merge join could use, every relation is
//! a plain table read by a sequential scan, and nothing asks for a fast start (no LIMIT): so every path of a
//! rel has the same rows, no pathkeys and no parameterization, and the paths differ only in cost, as here.
//!
//! Ported from PostgreSQL 16 (src/backend/optimizer/path/allpaths.c, joinrels.c, joinpath.c,
//! src/backend/optimizer/util/pathnode.c and relnode.c), under the PostgreSQL License:
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

use std::collections::HashMap;
use std::rc::Rc;

use super::Relids;
use super::clausesel::{self, Node, Rels};
use super::cost::{self, Input, QualCost, Rescan, Settings, clamp_row_est};

/// STD_FUZZ_FACTOR (pathnode.c:52).
const STD_FUZZ_FACTOR: f64 = 1.01;
/// add_path's last tie-break (pathnode.c:540).
const TINY_FUZZ_FACTOR: f64 = 1.0000000001;
/// The bit of `Var::needed` for the query's own output (attr_needed's relid 0).
pub(crate) const FINAL: Relids = 1 << 31;

/// A RestrictInfo: a clause, the base rels it reads (required_relids) and what it costs to check once
/// (eval_cost, as cost_qual_eval_walker counts it).
#[derive(Clone, Debug)]
pub(crate) struct Restrict {
    pub node: Node,
    pub relids: Relids,
    pub cost: QualCost,
}

/// A base relation (RelOptInfo, RELOPT_BASEREL), as get_relation_info and the query give it.
#[derive(Clone, Debug)]
pub(crate) struct BaseRel {
    pub pages: f64,
    pub tuples: f64,
    /// baserestrictinfo, in the order the quals were distributed
    pub restrict: Vec<Restrict>,
    /// its tablespace's seq_page_cost
    pub spc_seq_page_cost: f64,
}

/// A Var some rel's targetlist holds: its rel, its width (attr_widths) and where it is needed (attr_needed, with
/// FINAL for the query's output).
#[derive(Clone, Debug)]
pub(crate) struct Var {
    pub rel: usize,
    pub width: i32,
    pub needed: Relids,
}

/// What the join search is given.
#[derive(Clone, Debug)]
pub(crate) struct Query {
    /// in the joinlist's order (the FROM items', left to right)
    pub rels: Vec<BaseRel>,
    /// the join clauses (those reading more than one rel), in the order they were distributed
    pub joins: Vec<Restrict>,
    pub vars: Vec<Var>,
    pub settings: Settings,
}

/// A path: how a rel's rows are made, its row count, costs and target width.
#[derive(Debug)]
pub(crate) struct Path {
    pub node: PathNode,
    pub rows: f64,
    pub startup: f64,
    pub total: f64,
    pub width: i32,
}

#[derive(Debug)]
pub(crate) enum PathNode {
    SeqScan(usize),
    Material(Rc<Path>),
    NestLoop { outer: Rc<Path>, inner: Rc<Path> },
}

impl Path {
    /// The base rels in the order the plan returns their rows' combinations: a nested loop runs its inner side
    /// for each outer row, so its output is ordered by the outer side's rels, then the inner side's.
    pub(crate) fn leaves(&self, out: &mut Vec<usize>) {
        match &self.node {
            PathNode::SeqScan(r) => out.push(*r),
            PathNode::Material(p) => p.leaves(out),
            PathNode::NestLoop { outer, inner } => {
                outer.leaves(out);
                inner.leaves(out);
            }
        }
    }

    fn rescan(&self) -> Rescan {
        match self.node {
            PathNode::Material(_) => Rescan::Material { rows: self.rows, width: self.width },
            _ => Rescan::Plain { startup: self.startup, total: self.total },
        }
    }
}

/// A RelOptInfo, base or join.
struct Rel {
    relids: Relids,
    rows: f64,
    tuples: f64,
    /// reltarget: the Vars it emits, and their summed width
    target: Vec<usize>,
    width: i32,
    /// joininfo: the join clauses that read it and some rel outside it
    joininfo: Vec<usize>,
    pathlist: Vec<Rc<Path>>,
    cheapest_total: Option<Rc<Path>>,
}

struct Planner<'q> {
    q: &'q Query,
    rels: Vec<Rel>,
    by_relids: HashMap<Relids, usize>,
    levels: Vec<Vec<usize>>,
    cur_level: usize,
}

impl Rels for Planner<'_> {
    fn tuples(&self, rel: usize) -> f64 {
        self.rels[rel].tuples
    }
}

/// The cheapest path Postgres would find for `q`, or None where a clause's estimate isn't known.
pub(crate) fn best_path(q: &Query) -> Option<Rc<Path>> {
    let mut p = Planner { q, rels: vec![], by_relids: HashMap::new(), levels: vec![vec![]], cur_level: 1 };
    // set_base_rel_sizes and set_base_rel_pathlists (allpaths.c)
    for (r, base) in q.rels.iter().enumerate() {
        let target: Vec<usize> =
            (0..q.vars.len()).filter(|&v| q.vars[v].rel == r && q.vars[v].needed & !(1 << r) != 0).collect();
        let width = target.iter().map(|&v| q.vars[v].width).sum();
        let joininfo = (0..q.joins.len()).filter(|&c| q.joins[c].relids & (1 << r) != 0).collect();
        p.rels.push(Rel {
            relids: 1 << r,
            rows: 0.0,
            tuples: base.tuples,
            target,
            width,
            joininfo,
            pathlist: vec![],
            cheapest_total: None,
        });
        p.by_relids.insert(1 << r, r);
    }
    for (r, base) in q.rels.iter().enumerate() {
        // set_baserel_size_estimates (costsize.c:5014)
        let clauses: Vec<&Node> = base.restrict.iter().map(|c| &c.node).collect();
        let sel = clausesel::clauselist_selectivity(&p, &clauses)?;
        p.rels[r].rows = clamp_row_est(base.tuples * sel);
        let baserestrictcost = cost_qual_eval(&base.restrict.iter().collect::<Vec<_>>());
        // create_seqscan_path (pathnode.c:934)
        let (startup, total) =
            cost::cost_seqscan(&q.settings, base.pages, base.tuples, baserestrictcost, base.spc_seq_page_cost);
        let path =
            Rc::new(Path { node: PathNode::SeqScan(r), rows: p.rels[r].rows, startup, total, width: p.rels[r].width });
        add_path(&mut p.rels[r], path);
        set_cheapest(&mut p.rels[r]);
    }
    // standard_join_search (allpaths.c)
    let levels_needed = q.rels.len();
    p.levels.push((0..levels_needed).collect());
    for lev in 2..=levels_needed {
        p.levels.push(vec![]);
        p.cur_level = lev;
        p.join_search_one_level(lev)?;
        for i in p.levels[lev].clone() {
            set_cheapest(&mut p.rels[i]);
        }
    }
    let top = *p.levels[levels_needed].first()?;
    p.rels[top].cheapest_total.clone()
}

/// cost_qual_eval (costsize.c:4385) of a list of RestrictInfos: their eval costs summed in order.
fn cost_qual_eval(quals: &[&Restrict]) -> QualCost {
    let mut total = QualCost::default();
    for q in quals {
        total.startup += q.cost.startup;
        total.per_tuple += q.cost.per_tuple;
    }
    total
}

impl Planner<'_> {
    /// join_search_one_level (joinrels.c:72).
    fn join_search_one_level(&mut self, level: usize) -> Option<()> {
        // left-sided and right-sided plans: rels of level-1 members joined to initial rels
        let prev = self.levels[level - 1].clone();
        for (at, &old) in prev.iter().enumerate() {
            if !self.rels[old].joininfo.is_empty() {
                // at level 2 the condition is symmetric: only the initial rels after this one
                let others: Vec<usize> =
                    if level == 2 { self.levels[1][at + 1..].to_vec() } else { self.levels[1].clone() };
                self.make_rels_by_clause_joins(old, &others)?;
            } else {
                let others = self.levels[1].clone();
                self.make_rels_by_clauseless_joins(old, &others)?;
            }
        }
        // bushy plans: rels of k initial rels joined to rels of level-k, 2 <= k <= level-2, where a join clause
        // relates them
        let mut k = 2;
        loop {
            let other_level = level - k;
            if k > other_level {
                break;
            }
            let olds = self.levels[k].clone();
            for (at, &old) in olds.iter().enumerate() {
                if self.rels[old].joininfo.is_empty() {
                    continue;
                }
                let others: Vec<usize> =
                    if k == other_level { olds[at + 1..].to_vec() } else { self.levels[other_level].clone() };
                for new in others {
                    if self.rels[old].relids & self.rels[new].relids == 0 && self.have_relevant_joinclause(old, new) {
                        self.make_join_rel(old, new)?;
                    }
                }
            }
            k += 1;
        }
        // last-ditch effort: cartesian-product joins of every level-1 rel
        if self.levels[level].is_empty() {
            for old in prev {
                let others = self.levels[1].clone();
                self.make_rels_by_clauseless_joins(old, &others)?;
            }
            if self.levels[level].is_empty() {
                return None; // "failed to build any %d-way joins"
            }
        }
        Some(())
    }

    /// make_rels_by_clause_joins (joinrels.c:299).
    fn make_rels_by_clause_joins(&mut self, old: usize, others: &[usize]) -> Option<()> {
        for &other in others {
            if self.rels[old].relids & self.rels[other].relids == 0 && self.have_relevant_joinclause(old, other) {
                self.make_join_rel(old, other)?;
            }
        }
        Some(())
    }

    /// make_rels_by_clauseless_joins (joinrels.c:333).
    fn make_rels_by_clauseless_joins(&mut self, old: usize, others: &[usize]) -> Option<()> {
        for &other in others {
            if self.rels[old].relids & self.rels[other].relids == 0 {
                self.make_join_rel(old, other)?;
            }
        }
        Some(())
    }

    /// have_relevant_joinclause (joininfo.c): a clause in the shorter joininfo list that reads the other rel.
    fn have_relevant_joinclause(&self, rel1: usize, rel2: usize) -> bool {
        let (a, b) = (&self.rels[rel1], &self.rels[rel2]);
        let (joininfo, other) =
            if a.joininfo.len() <= b.joininfo.len() { (&a.joininfo, b.relids) } else { (&b.joininfo, a.relids) };
        joininfo.iter().any(|&c| self.q.joins[c].relids & other != 0)
    }

    /// make_join_rel (joinrels.c:689) for an inner join, which is always legal.
    fn make_join_rel(&mut self, rel1: usize, rel2: usize) -> Option<()> {
        // (a search of many tables can be long: a statement canceled or past its statement_timeout stops it, and
        // keeps the executor's order, whose next check stops the statement)
        super::super::tick().ok()?;
        let joinrelids = self.rels[rel1].relids | self.rels[rel2].relids;
        let (joinrel, restrictlist) = self.build_join_rel(joinrelids, rel1, rel2)?;
        // populate_joinrel_with_paths (joinrels.c:894), JOIN_INNER: both ways round
        self.add_paths_to_joinrel(joinrel, rel1, rel2, &restrictlist);
        self.add_paths_to_joinrel(joinrel, rel2, rel1, &restrictlist);
        Some(())
    }

    /// build_join_rel (relnode.c:645): the joinrel of these relids (made if new, with its size from this pair),
    /// and this pair's restrictlist.
    fn build_join_rel(&mut self, relids: Relids, outer: usize, inner: usize) -> Option<(usize, Vec<usize>)> {
        if let Some(&j) = self.by_relids.get(&relids) {
            return Some((j, self.build_joinrel_restrictlist(relids, outer, inner)));
        }
        // the Vars needed above this join, the outer rel's then the inner's (build_joinrel_tlist)
        let mut target = vec![];
        let mut width = 0;
        for &v in self.rels[outer].target.iter().chain(&self.rels[inner].target) {
            if self.q.vars[v].needed & !relids != 0 {
                target.push(v);
                width += self.q.vars[v].width;
            }
        }
        let restrictlist = self.build_joinrel_restrictlist(relids, outer, inner);
        // build_joinrel_joinlist: the clauses still reading rels outside it, each once
        let mut joininfo: Vec<usize> = vec![];
        for &c in self.rels[outer].joininfo.iter().chain(&self.rels[inner].joininfo) {
            if self.q.joins[c].relids & !relids != 0 && !joininfo.contains(&c) {
                joininfo.push(c);
            }
        }
        // set_joinrel_size_estimates (costsize.c:5093) and calc_joinrel_size_estimate (costsize.c:5166), inner
        // join, no foreign keys matched (fkselec 1)
        let clauses: Vec<&Node> = restrictlist.iter().map(|&c| &self.q.joins[c].node).collect();
        let jselec = clausesel::clauselist_selectivity(self, &clauses)?;
        let fkselec = 1.0;
        let rows = clamp_row_est(self.rels[outer].rows * self.rels[inner].rows * fkselec * jselec);
        let j = self.rels.len();
        self.rels.push(Rel {
            relids,
            rows,
            tuples: 0.0,
            target,
            width,
            joininfo,
            pathlist: vec![],
            cheapest_total: None,
        });
        self.by_relids.insert(relids, j);
        self.levels[self.cur_level].push(j);
        Some((j, restrictlist))
    }

    /// build_joinrel_restrictlist (relnode.c:1272): the clauses of the two inputs' joininfo lists that read
    /// nothing outside the joinrel, the outer's first, each once.
    fn build_joinrel_restrictlist(&self, relids: Relids, outer: usize, inner: usize) -> Vec<usize> {
        let mut out: Vec<usize> = vec![];
        for &c in self.rels[outer].joininfo.iter().chain(&self.rels[inner].joininfo) {
            if self.q.joins[c].relids & !relids == 0 && !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    /// add_paths_to_joinrel (joinpath.c) with no merge or hash clauses: match_unsorted_outer's nested loops.
    fn add_paths_to_joinrel(&mut self, joinrel: usize, outerrel: usize, innerrel: usize, restrictlist: &[usize]) {
        let s = &self.q.settings;
        let Some(inner_cheapest_total) = self.rels[innerrel].cheapest_total.clone() else { return };
        // materialize the cheapest inner path, unless enable_material is off (a SeqScan or a NestLoop doesn't
        // materialize its output)
        let matpath = s.enable_material.then(|| {
            let (startup, total) = cost::cost_material(
                s,
                inner_cheapest_total.startup,
                inner_cheapest_total.total,
                inner_cheapest_total.rows,
                inner_cheapest_total.width,
            );
            Rc::new(Path {
                node: PathNode::Material(inner_cheapest_total.clone()),
                rows: inner_cheapest_total.rows,
                startup,
                total,
                width: self.rels[innerrel].width,
            })
        });
        let restrict: Vec<&Restrict> = restrictlist.iter().map(|&c| &self.q.joins[c]).collect();
        let restrict_cost = cost_qual_eval(&restrict);
        let outers = self.rels[outerrel].pathlist.clone();
        // the inner rel's cheapest_parameterized_paths: its cheapest-total path alone
        let inners = [inner_cheapest_total];
        for outerpath in &outers {
            for innerpath in &inners {
                self.try_nestloop_path(joinrel, outerpath, innerpath, restrict_cost);
            }
            if let Some(m) = &matpath {
                self.try_nestloop_path(joinrel, outerpath, m, restrict_cost);
            }
        }
    }

    /// try_nestloop_path (joinpath.c).
    fn try_nestloop_path(&mut self, joinrel: usize, outer: &Rc<Path>, inner: &Rc<Path>, restrict: QualCost) {
        let s = &self.q.settings;
        let (outer_rescan, inner_rescan) = (outer.rescan(), inner.rescan());
        let workspace = cost::initial_cost_nestloop(
            s,
            &Input { rows: outer.rows, startup: outer.startup, total: outer.total, rescan: &outer_rescan },
            &Input { rows: inner.rows, startup: inner.startup, total: inner.total, rescan: &inner_rescan },
        );
        if !add_path_precheck(&self.rels[joinrel], workspace.startup_cost, workspace.total_cost) {
            return;
        }
        // create_nestloop_path (pathnode.c:2430)
        let (startup, total) = cost::final_cost_nestloop(s, &workspace, outer.rows, inner.rows, restrict);
        let rel = &mut self.rels[joinrel];
        let path = Rc::new(Path {
            node: PathNode::NestLoop { outer: outer.clone(), inner: inner.clone() },
            rows: rel.rows,
            startup,
            total,
            width: rel.width,
        });
        add_path(rel, path);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Costs {
    Equal,
    Better1,
    Better2,
}

/// compare_path_costs_fuzzily (pathnode.c:171) for two paths of a rel whose startup cost isn't of interest
/// (consider_startup false: no LIMIT), so never COSTS_DIFFERENT.
fn compare_path_costs_fuzzily(path1: &Path, path2: &Path, fuzz_factor: f64) -> Costs {
    // total cost first, since it's more likely to be different
    if path1.total > path2.total * fuzz_factor {
        return Costs::Better2;
    }
    if path2.total > path1.total * fuzz_factor {
        return Costs::Better1;
    }
    // fuzzily the same on total cost: startup decides
    if path1.startup > path2.startup * fuzz_factor {
        return Costs::Better2;
    }
    if path2.startup > path1.startup * fuzz_factor {
        return Costs::Better1;
    }
    Costs::Equal
}

/// add_path (pathnode.c:427): every path here has no pathkeys and no parameterization, and paths of a rel are
/// alike in parallel safety.
fn add_path(parent: &mut Rel, new_path: Rc<Path>) {
    let mut accept_new = true;
    let mut insert_at = 0;
    let mut i = 0;
    while i < parent.pathlist.len() {
        let old_path = parent.pathlist[i].clone();
        let mut remove_old = false;
        // keyscmp is PATHKEYS_EQUAL and outercmp BMS_EQUAL for every pair
        match compare_path_costs_fuzzily(&new_path, &old_path, STD_FUZZ_FACTOR) {
            Costs::Equal => {
                if new_path.rows < old_path.rows {
                    remove_old = true;
                } else if new_path.rows > old_path.rows {
                    accept_new = false;
                } else if compare_path_costs_fuzzily(&new_path, &old_path, TINY_FUZZ_FACTOR) == Costs::Better1 {
                    remove_old = true;
                } else {
                    accept_new = false; // old equals or dominates new
                }
            }
            Costs::Better1 => {
                if new_path.rows <= old_path.rows {
                    remove_old = true;
                }
            }
            Costs::Better2 => {
                if new_path.rows >= old_path.rows {
                    accept_new = false;
                }
            }
        }
        if remove_old {
            parent.pathlist.remove(i);
        } else {
            // new belongs after this old path if it has cost >= old's
            if new_path.total >= old_path.total {
                insert_at = i + 1;
            }
            i += 1;
        }
        if !accept_new {
            break;
        }
    }
    if accept_new {
        parent.pathlist.insert(insert_at, new_path);
    }
}

/// add_path_precheck (pathnode.c:649): false if an old path dominates any path of these costs (lower bounds).
fn add_path_precheck(parent: &Rel, startup_cost: f64, total_cost: f64) -> bool {
    let consider_startup = false;
    for old_path in &parent.pathlist {
        if total_cost > old_path.total * STD_FUZZ_FACTOR {
            // new path can win on startup cost only if consider_startup
            if startup_cost > old_path.startup * STD_FUZZ_FACTOR || !consider_startup {
                // the same pathkeys and parameterization: found an old path that dominates the new one
                return false;
            }
        } else {
            // the pathlist is sorted by total_cost: none further on is cheaper
            break;
        }
    }
    true
}

/// set_cheapest (pathnode.c:249): the cheapest-total path, by exact total cost, then startup cost.
fn set_cheapest(parent: &mut Rel) {
    let mut cheapest: Option<Rc<Path>> = None;
    for path in &parent.pathlist {
        match &cheapest {
            None => cheapest = Some(path.clone()),
            Some(c) => {
                let cmp = if c.total != path.total { c.total > path.total } else { c.startup > path.startup };
                if cmp {
                    cheapest = Some(path.clone());
                }
            }
        }
    }
    parent.cheapest_total = cheapest;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(pages: f64, tuples: f64) -> BaseRel {
        BaseRel { pages, tuples, restrict: vec![], spc_seq_page_cost: 1.0 }
    }

    fn order(q: &Query) -> Vec<usize> {
        let mut out = vec![];
        best_path(q).unwrap().leaves(&mut out);
        out
    }

    /// join.sql: J1_TBL (1,200 rows by the 10-page rule, width 40) CROSS JOIN J2_TBL (2,260, width 8): Postgres
    /// puts J2 outer; with a second J2_TBL, J2 a outer of J1, and b inner of both.
    #[test]
    fn cross_joins_go_in_postgres_order() {
        let var = |rel, width| Var { rel, width, needed: FINAL };
        let q = Query {
            rels: vec![rel(10.0, 1200.0), rel(10.0, 2260.0)],
            joins: vec![],
            vars: vec![var(0, 4), var(0, 4), var(0, 32), var(1, 4), var(1, 4)],
            settings: Settings::default(),
        };
        let p = best_path(&q).unwrap();
        assert_eq!(format!("{:.2}", p.total), "33957.60");
        assert_eq!(order(&q), vec![1, 0]);
        let q3 = Query {
            rels: vec![rel(10.0, 1200.0), rel(10.0, 2260.0), rel(10.0, 2260.0)],
            joins: vec![],
            vars: vec![var(0, 4), var(0, 4), var(0, 32), var(1, 4), var(1, 4), var(2, 4), var(2, 4)],
            settings: Settings::default(),
        };
        assert_eq!(format!("{:.2}", best_path(&q3).unwrap().total), "76647995.85");
        assert_eq!(order(&q3), vec![1, 0, 2]);
    }
}
