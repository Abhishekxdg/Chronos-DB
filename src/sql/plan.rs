//! EXPLAIN as Postgres's planner makes the plan, when the session asks for it (chronos.explain = postgres, exact
//! mode's default: Chronos may run the statement another way), for the statements whose plan doesn't turn on what
//! things cost: a read of one table (a partitioned or inherited one an Append of the partitions or children it reads,
//! pruned as partprune.c prunes them by the WHERE), under a plain Aggregate, a Sort and a Limit; an UPDATE or DELETE
//! of one. The plan is built as a tree of PostgreSQL's plan nodes over a range table (see `explain`).
//! Conditions are written as ruleutils.c writes them, in the order the planner keeps them (createplan.c's
//! order_qual_clauses: cheapest first, the equalities that make equivalence classes after the rest). Anything else
//! (joins, a condition or order an index could serve, grouping, subqueries, parameters outside GENERIC_PLAN) gives
//! None, and EXPLAIN shows Chronos's own steps.
//!
//! With COSTS each node carries the model's numbers, as PostgreSQL would cost the plan for these rows: estimate_rel_size
//! over an emulated heap (the pages the rows would take; 10 for a table never vacuumed), the selectivities selfuncs.c
//! gives without statistics, and costsize.c's formulas. They are estimates in PostgreSQL's units, not what Chronos
//! spends; a plan whose numbers this can't work out (a condition without a ported selectivity) isn't shown with COSTS.
//! Chronos keeps no statistics (pg_class shows every table never vacuumed), so after a VACUUM or ANALYZE Postgres's
//! numbers would be its statistics' and these are still the never-vacuumed ones.
//!
//! Portions Copyright (c) 1996-2023, PostgreSQL Global Development Group
//! Portions Copyright (c) 1994, The Regents of the University of California
//! (ported in part from PostgreSQL 16's src/backend/optimizer and src/backend/partitioning, under the PostgreSQL
//! License: see the NOTICE file)

use std::cell::RefCell;

use super::explain::{
    AggStrategy, CmdType, Cost, ExplainOpts, Node, PExpr, Plan, PlannedStmt, Rte, SortKey, Target, Var,
};
use super::partition::*;
use super::viewdef::col_ty;
use super::*;

impl Exec<'_> {
    /// The plan Postgres's planner makes for `stmt` (the model's costs worked out where `o` asks for COSTS); None
    /// for a statement this doesn't plan.
    pub(super) fn pg_plan(&self, stmt: &Stmt, o: &ExplainOpts) -> Result<Option<PlannedStmt>> {
        let v = self.view()?;
        let off = |name: &str| {
            session_setting(name).is_some_and(|s| matches!(s.to_lowercase().as_str(), "off" | "false" | "0" | "no"))
        };
        // (where parallel plans are made cheap or forced, a Gather may come over any scan)
        let parallel = ["parallel_setup_cost", "parallel_tuple_cost", "min_parallel_table_scan_size"]
            .iter()
            .any(|n| session_setting(n).is_some_and(|s| s.trim_start_matches('0').is_empty() || s.starts_with("0 ")))
            || !off("debug_parallel_query") && session_setting("debug_parallel_query").is_some();
        if parallel {
            return Ok(None);
        }
        // (a hash key's part hashed by its operator class's support function, where it names one made with CREATE
        // OPERATOR CLASS: partition pruning computes the hash as routing does)
        let hash = |pk: &PartKey, types: &[Type], i: usize, x: &Value| -> Option<Result<u64>> {
            match self.hash_procs(&v, pk) {
                Ok(procs) => self.part_hash(&procs, types, i, x),
                Err(e) => Some(Err(e)),
            }
        };
        // (a part of an expression the planner works out when it makes the plan: see `Planner::fold`)
        let eval = |e: &Expr| self.constant(e);
        // max_parallel_workers_per_gather and min_parallel_table_scan_size (in pages), which say where a scan may get
        // a Gather over it
        let workers = session_setting("max_parallel_workers_per_gather").map_or(Some(2.0), |s| s.trim().parse().ok());
        // (its unit is a page: 8MB, 1024 pages, by default)
        let min_pages = session_setting("min_parallel_table_scan_size")
            .map_or(Some(1024.0), |s| s.trim().parse::<f64>().ok().or_else(|| memory_kb(&s).map(|kb| kb / 8.0)));
        let p = Planner {
            v: &v,
            hash: &hash,
            eval: &eval,
            parallel: match (workers, min_pages) {
                (Some(w), Some(_)) if w <= 0.0 => ParallelScan::Never,
                (Some(_), Some(m)) => ParallelScan::From(m),
                // (a setting this doesn't read: no plan is shown)
                _ => ParallelScan::Unknown,
            },
            exclusion: match session_setting("constraint_exclusion").as_deref() {
                Some("on") => Exclusion::On,
                Some("off") => Exclusion::Off,
                _ => Exclusion::Partition,
            },
            prune: !off("enable_partition_pruning"),
            partitionwise: !off("enable_partitionwise_aggregate")
                && session_setting("enable_partitionwise_aggregate").is_some(),
            generic: o.generic,
            costs: o.costs,
            g: Gucs::now(),
            rtable: RefCell::new(vec![]),
        };
        let node = match stmt {
            Stmt::Select(s) => p.select(s)?,
            Stmt::Update { table, only, sets, filter, from: None, .. } => {
                let exprs: Vec<&Expr> = sets.iter().map(|(_, e)| e).collect();
                p.modify(CmdType::Update, table, *only, filter.as_ref(), &exprs)?
            }
            Stmt::Delete { table, only, filter, from: None, .. } => {
                p.modify(CmdType::Delete, table, *only, filter.as_ref(), &[])?
            }
            _ => None,
        };
        Ok(node.map(|n| PlannedStmt { rtable: p.rtable.into_inner(), roots: vec![n] }))
    }
}

/// The settings the model reads (costsize.c's globals), as the session has them.
struct Gucs {
    seq_page_cost: f64,
    random_page_cost: f64,
    cpu_tuple_cost: f64,
    cpu_operator_cost: f64,
    /// work_mem, in kB
    work_mem: f64,
    enable_seqscan: bool,
    enable_sort: bool,
}

/// costsize.c's disable_cost: what a path a setting turns off costs more.
const DISABLE_COST: f64 = 1.0e10;

impl Gucs {
    fn now() -> Gucs {
        let real = |name: &str, default: f64| {
            session_setting(name)
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|x| x.is_finite())
                .unwrap_or(default)
        };
        let on = |name: &str| {
            !session_setting(name).is_some_and(|s| matches!(s.to_lowercase().as_str(), "off" | "false" | "0" | "no"))
        };
        Gucs {
            seq_page_cost: real("seq_page_cost", 1.0),
            random_page_cost: real("random_page_cost", 4.0),
            cpu_tuple_cost: real("cpu_tuple_cost", 0.01),
            cpu_operator_cost: real("cpu_operator_cost", 0.0025),
            work_mem: session_setting("work_mem").and_then(|s| memory_kb(&s)).unwrap_or(4096.0),
            enable_seqscan: on("enable_seqscan"),
            enable_sort: on("enable_sort"),
        }
    }
}

/// A memory setting as SHOW writes it (`4MB`, `64kB`) in kB.
fn memory_kb(s: &str) -> Option<f64> {
    let s = s.trim();
    let digits = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let n: f64 = s[..digits].parse().ok()?;
    Some(match s[digits..].trim() {
        "" | "kB" => n,
        "MB" => n * 1024.0,
        "GB" => n * 1024.0 * 1024.0,
        "TB" => n * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    })
}

/// constraint_exclusion: off, partition (the default: the tables an inheritance tree or partitioned table reads),
/// or on (every table).
#[derive(Clone, Copy, PartialEq)]
enum Exclusion {
    Off,
    Partition,
    On,
}

/// A key part's hash by its operator class's support function (see `Exec::part_hash`): None for its type's own.
type PartHasher<'a> = &'a dyn Fn(&PartKey, &[Type], usize, &Value) -> Option<Result<u64>>;

/// Where a scan of a table gets partial paths, which a Gather puts together (compute_parallel_worker, allpaths.c:4210,
/// for a base relation): from a table of so many pages (min_parallel_table_scan_size), or never
/// (max_parallel_workers_per_gather 0, where standard_planner makes no parallel plan: planner.c:347); `Unknown` where
/// a setting isn't one this reads.
#[derive(Clone, Copy)]
enum ParallelScan {
    From(f64),
    Never,
    Unknown,
}

struct Planner<'a, 'v> {
    v: &'a View<'v>,
    hash: PartHasher<'a>,
    /// a constant expression's value, as the planner works it out (see `fold`)
    eval: &'a dyn Fn(&Expr) -> Result<Value>,
    parallel: ParallelScan,
    /// constraint_exclusion
    exclusion: Exclusion,
    /// enable_partition_pruning
    prune: bool,
    /// enable_partitionwise_aggregate
    partitionwise: bool,
    /// EXPLAIN (GENERIC_PLAN): the parameters have no values, and prune nothing when the plan is made
    generic: bool,
    /// COSTS: the model's numbers are worked out
    costs: bool,
    g: Gucs,
    /// the range table, as the plan is made
    rtable: RefCell<Vec<Rte>>,
}

/// What a relation's scan is under its WHERE: never true, or its conditions in the planner's order, with the terms
/// an equivalence class makes equal to a constant (an ORDER BY of one needs no sort).
enum Quals {
    False,
    List(Vec<Expr>, Vec<String>),
}

/// A scan as `Planner::scan` makes it: its plan, the range-table entries of the tables it reads (an UPDATE's or
/// DELETE's targets), and whether the range table holds more than the table named (an inheritance tree), which
/// qualifies the columns of the nodes above.
struct Scanned {
    plan: Plan,
    read: Vec<usize>,
    tree: bool,
}

/// The column of `t` named `name` (from 1, among the columns a row has), if it is one.
fn attno(t: &Table, name: &str) -> Option<usize> {
    t.cols.iter().filter(|c| !c.hidden).position(|c| c.name == name).map(|i| i + 1)
}

impl Planner<'_, '_> {
    /// A new entry of the range table for `t`, shown as `refname`, written in the query as `written`: its varno.
    fn rte(&self, t: &Table, refname: &str, written: &str) -> usize {
        let mut rt = self.rtable.borrow_mut();
        rt.push(Rte {
            relname: rel_part(&t.name).to_string(),
            schema: schema_part(&t.name).to_string(),
            refname: refname.to_string(),
            written: written.to_string(),
            // (and after them tableoid, the system column a query may read: its expressions name it)
            cols: t
                .cols
                .iter()
                .filter(|c| !c.hidden)
                .map(col_ty)
                .chain(std::iter::once(("tableoid".to_string(), Type::Oid, (0, -1))))
                .collect(),
            colls: t
                .cols
                .iter()
                .filter(|c| !c.hidden)
                .map(|c| c.coll)
                .chain(std::iter::once(collation::Coll::DEFAULT))
                .collect(),
        });
        rt.len()
    }

    /// A SELECT of one table (or of nothing).
    fn select(&self, s: &Select) -> Result<Option<Plan>> {
        if !s.set.is_empty() || !s.joins.is_empty() || s.distinct || !s.group.is_empty() || s.having.is_some() {
            return Ok(None);
        }
        let mut exprs: Vec<&Expr> = vec![];
        for it in &s.items {
            if let Item::Expr(e, _) = it {
                exprs.push(e);
            }
        }
        exprs
            .extend(s.filter.iter().chain(s.order.iter().map(|(e, _)| e)).chain(s.limit.iter()).chain(s.offset.iter()));
        if exprs.iter().any(|e| self.awkward(e)) {
            return Ok(None);
        }
        let agg = s.items.iter().any(|it| matches!(it, Item::Expr(e, _) if has_agg(e)));
        let Some(src) = &s.from else {
            // (a select list alone)
            let plain = s.filter.is_none() && s.order.is_empty() && s.limit.is_none() && s.offset.is_none() && !agg;
            if !plain {
                return Ok(None);
            }
            let Some(s) = self.folded(s, &[]) else { return Ok(None) };
            let mut r = self.result_of(&s);
            if !s.items.iter().all(|it| matches!(it, Item::Expr(e, _) if plain_target(e))) {
                no_tlist(&mut r);
            }
            return Ok(Some(r));
        };
        if src.sub.is_some()
            || src.srf.is_some()
            || src.rec.is_some()
            || src.at.is_some()
            || src.cte != 0
            || src.working
            || !src.names.is_empty()
            || view_def(self.v, &src.table)?.is_some()
        {
            return Ok(None);
        }
        let Some(t) = load_src(self.v, src)? else { return Ok(None) };
        // (row security puts its policies in the conditions)
        if !t.schema
            || t.matview.is_some()
            || t.hier.foreign.is_some()
            || (t.mem.is_some() && !self.temp(&t)?)
            || rls::flags(self.v, &t.name)?.0
        {
            return Ok(None);
        }
        // (the query as the planner leaves it, its constant parts worked out: see `fold`)
        let Some(folded) = self.folded(s, &t.cols) else { return Ok(None) };
        let s = &folded;
        let alias = match src.alias.is_empty() || src.alias == src.table {
            true => rel_part(&src.table).to_string(),
            false => src.alias.clone(),
        };
        // the select list, `*` its columns: the target list of what reads the table (the sort keys it doesn't have
        // after them), and what the scan's rows must carry (the arguments of aggregates, under one)
        let mut tl: Vec<Expr> = vec![];
        for it in &s.items {
            match it {
                Item::Star => tl.extend(t.cols.iter().filter(|c| !c.hidden).map(|c| Expr::Col(None, c.name.clone()))),
                Item::TableStar(q) if *q == alias || *q == src.table => {
                    tl.extend(t.cols.iter().filter(|c| !c.hidden).map(|c| Expr::Col(None, c.name.clone())))
                }
                Item::TableStar(_) => return Ok(None),
                Item::Expr(e, _) => tl.push(e.clone()),
            }
        }
        // the columns the ORDER BY sorts by (an output column's name or place is what it is), each with its collation
        let mut keys: Vec<(Expr, Dir, Option<collation::Coll>)> = vec![];
        let mut relabeled = false;
        for (e, d) in &s.order {
            let k = match e {
                Expr::Lit(Value::Int(n)) => match usize::try_from(*n).ok().and_then(|n| tl.get(n.checked_sub(1)?)) {
                    Some(x) => x.clone(),
                    None => return Ok(None),
                },
                Expr::Col(None, name) => {
                    let named = s.items.iter().find_map(|it| match it {
                        Item::Expr(x, Some(a)) if a == name => Some(x.clone()),
                        _ => None,
                    });
                    named.unwrap_or_else(|| e.clone())
                }
                e => e.clone(),
            };
            // (a constant orders nothing: the planner makes no pathkey of it, which isn't worked out here)
            if self.awkward(&k) || !has_col(&k) {
                return Ok(None);
            }
            // (the sort's collation is the key's, exprCollation; EXPLAIN shows it where it isn't its type's default:
            // show_sortorder_options, explain.c:2640)
            let coll = match coll_of(&k, &t.cols) {
                Ok(collation::Strength::None) => None,
                Ok(st) if st.coll() == collation::Coll::INDETERMINATE => return Ok(None),
                Ok(st) => Some(st.coll()),
                Err(_) => return Ok(None),
            };
            // (a COLLATE on a column the select list has: that column, sorted in the collation, as
            // find_ec_member_matching_expr (equivclass.c:760) looks through the RelabelType it is; on anything else
            // the sort key is a projected expression this doesn't write)
            let k = match k {
                Expr::Func(Func::Collate(_), args) if args.len() == 1 => match &args[0] {
                    x @ Expr::Col(..) if tl.iter().any(|y| term_key(y) == term_key(x)) => {
                        relabeled = true;
                        x.clone()
                    }
                    _ => return Ok(None),
                },
                k => k,
            };
            keys.push((k, *d, coll));
        }
        let quals = match &s.filter {
            None => Quals::List(vec![], vec![]),
            Some(f) => match scan_quals(f, &t.cols) {
                Some(q) => q,
                None => return Ok(None),
            },
        };
        // a condition, an order or an aggregate an index might serve: what the plan is then turns on costs
        let mut used: Vec<String> = vec![];
        for e in s.filter.iter().chain(keys.iter().map(|(e, ..)| e)) {
            cols_of(e, &mut used);
        }
        if agg {
            for it in &s.items {
                if let Item::Expr(e, _) = it {
                    cols_of(e, &mut used);
                }
            }
        }
        let consts = match &quals {
            Quals::List(_, c) => c.clone(),
            Quals::False => vec![],
        };
        // (a key an equivalence class makes a constant orders nothing; one sorted in another collation than the
        // equality's is of another class, which isn't worked out here)
        if relabeled && keys.iter().any(|(k, ..)| consts.contains(&term_key(k))) {
            return Ok(None);
        }
        keys.retain(|(k, ..)| !consts.contains(&term_key(k)));
        if !agg {
            for (k, ..) in &keys {
                if !tl.iter().any(|x| term_key(x) == term_key(k)) {
                    tl.push(k.clone());
                }
            }
        }
        let carried: Vec<Expr> = if agg {
            let mut vars: Vec<String> = vec![];
            for e in &tl {
                cols_of(e, &mut vars);
            }
            vars.into_iter().map(|n| Expr::Col(None, n)).collect()
        } else {
            tl.clone()
        };
        let Some(sc) = self.scan(&t, src.only, &alias, &quals, None, &used, &carried)? else {
            return Ok(None);
        };
        let tree = sc.tree;
        let mut node = sc.plan;
        // (an inheritance tree's Append doesn't project: where the target list isn't the columns it reads, each once,
        // in the order the select list first names them (the rel's reltarget, build_base_rel_tlists), the scan/join
        // target differs from it and apply_scanjoin_target_to_paths (planner.c:7601) puts a Result over the Append,
        // which isn't made here)
        if tree && t.hier.part.is_none() {
            let mut seen: Vec<&str> = vec![];
            for e in &carried {
                let Expr::Col(_, n) = e else { return Ok(None) };
                if seen.contains(&n.as_str()) {
                    return Ok(None);
                }
                seen.push(n);
            }
        }
        // the upper nodes' target list: the parent's columns, by the name the query gives it
        let top = if tree { 1 } else { sc.read.first().copied().unwrap_or(1) };
        let upper: Vec<Target> = self.targets(&t, top, &tl);
        if agg {
            if !keys.is_empty() || (tree && self.partitionwise) {
                return Ok(None);
            }
            let aggs: Vec<Target> = tl.iter().map(|e| Target::Expr(PExpr { expr: e.clone(), varno: top })).collect();
            let cost = match (self.costs, node.cost) {
                (true, Some(c)) => self.agg_cost(&tl, c, &t),
                _ => None,
            };
            // (an aggregate over a plain table reads its rows whole, the scan's physical target list, unless a
            // system column or the whole row is read: use_physical_tlist, createplan.c:896; then the scan's target
            // list is what it carries)
            let physical = carried.iter().all(|e| matches!(e, Expr::Col(_, n) if attno(&t, n).is_some()));
            if tree || !t.dropped.is_empty() {
                node.tlist = None;
            } else if physical {
                node.tlist = Some(self.targets(
                    &t,
                    top,
                    &t.cols.iter().filter(|c| !c.hidden).map(|c| Expr::Col(None, c.name.clone())).collect::<Vec<_>>(),
                ));
            }
            node = Plan { cost, tlist: Some(aggs), ..Plan::above(Node::Agg { strategy: AggStrategy::Plain }, node) };
        }
        if !keys.is_empty() {
            let sort_keys: Vec<SortKey> = keys
                .iter()
                .map(|(k, d, coll)| SortKey {
                    expr: PExpr { expr: k.clone(), varno: top },
                    desc: d.desc,
                    nulls_first: d.nulls_first,
                    coll: *coll,
                })
                .collect();
            // (a known LIMIT, and no unknown OFFSET: a bounded sort of that many, planner.c:1405)
            let limit_tuples = match (&s.limit, limit_ests(s.limit.as_ref(), s.offset.as_ref())) {
                (Some(_), Some((count, offset))) if count > 0.0 => Some(count + offset),
                _ => None,
            };
            let cost = match (self.costs, node.cost) {
                (true, Some(c)) => Some(self.sort_cost(c, limit_tuples)),
                _ => None,
            };
            node = Plan { cost, tlist: Some(upper.clone()), ..Plan::above(Node::Sort { keys: sort_keys }, node) };
        }
        if s.limit.is_some() || s.offset.is_some() {
            let cost = match (self.costs, node.cost) {
                (true, Some(c)) => limit_cost(c, s.limit.as_ref(), s.offset.as_ref()),
                _ => None,
            };
            let tlist = node.tlist.clone();
            node = Plan { cost, tlist, ..Plan::above(Node::Limit, node) };
        }
        // (a row value or a cast of one in the select list: the planner folds its casts, which isn't written here, so
        // VERBOSE's Output isn't shown; nor where a sort key is a column in another collation, which the planner
        // keeps a resjunk entry of as well)
        if !tl.iter().all(plain_target) || relabeled {
            no_tlist(&mut node);
        }
        Ok(Some(node))
    }

    /// `s` with its select list, WHERE, ORDER BY, LIMIT and OFFSET as the planner leaves them (see `fold`, `fold_qual`;
    /// `cols`: the columns of the table it reads); None where that isn't worked out here, or an ORDER BY of an
    /// expression comes out a constant (a sort key the planner drops, which here would read as a column's place).
    fn folded(&self, s: &Select, cols: &[Col]) -> Option<Select> {
        let mut items = vec![];
        for it in &s.items {
            items.push(match it {
                Item::Expr(e, a) => Item::Expr(self.fold(e, cols)?, a.clone()),
                it => it.clone(),
            });
        }
        let filter = match &s.filter {
            Some(f) => Some(self.fold_qual(f, cols)?),
            None => None,
        };
        let mut order = vec![];
        for (e, d) in &s.order {
            let k = self.fold(e, cols)?;
            if !matches!(e, Expr::Lit(_)) && !has_col(&k) {
                return None;
            }
            order.push((k, *d));
        }
        let limit = match &s.limit {
            Some(e) => Some(self.fold(e, cols)?),
            None => None,
        };
        let offset = match &s.offset {
            Some(e) => Some(self.fold(e, cols)?),
            None => None,
        };
        Some(Select { items, filter, order, limit, offset, ..s.clone() })
    }

    /// `e` as eval_const_expressions (optimizer/util/clauses.c:2422) leaves it for the plan to show: each part that
    /// `folds` says the planner works out its value (evaluate_function, clauses.c:4406), as a constant (`const_expr`);
    /// then a strict operator of a null constant null (see `nulls`). A constant as written is left to the deparser,
    /// which writes it as the planner folds it. None where what the planner leaves isn't worked out here: a part
    /// that fails (EXPLAIN raises its error: see `Exec::planning_errors`), a value of a type this doesn't write, a
    /// null left where more is folded around it.
    fn fold(&self, e: &Expr, cols: &[Col]) -> Option<Expr> {
        let e = self.fold_parts(e, cols)?;
        // (a null an AND or an OR has is kept there, after its other arms: simplify_and_arguments, clauses.c:3877)
        (!null_arm(&e)).then_some(e)
    }

    /// `fold`, a null an AND or an OR has left in it.
    fn fold_parts(&self, e: &Expr, cols: &[Col]) -> Option<Expr> {
        let failed = std::cell::Cell::new(false);
        let out = rewrite(e, &|x| {
            if failed.get() || written_const(x) {
                return Some(x.clone());
            }
            if !folds(x, cols) {
                return None;
            }
            let c = match ((self.eval)(x), ty_in(x, cols, &[], &[])) {
                (Ok(v), Ok(ty)) => const_expr(v, ty),
                _ => None,
            };
            failed.set(c.is_none());
            Some(c.unwrap_or_else(|| x.clone()))
        });
        if failed.get() {
            return None;
        }
        nulls(&out)
    }

    /// A WHERE as the planner leaves it (see `fold`): a null among its ANDs and ORs is false there, as
    /// canonicalize_qual's find_duplicate_ors takes it (prepqual.c:407), which `simplify` then folds away; a
    /// condition that is false no row passes (relation_excluded_by_constraints, plancat.c:1449, takes a constant false
    /// or null restriction for a relation that is empty).
    fn fold_qual(&self, f: &Expr, cols: &[Col]) -> Option<Expr> {
        fn falsed(e: Expr) -> Expr {
            match e {
                e if is_null(&e) => Expr::Lit(Value::Bool(false)),
                Expr::Bin(a, op @ (Op::And | Op::Or), b) => Expr::Bin(Box::new(falsed(*a)), op, Box::new(falsed(*b))),
                e => e,
            }
        }
        let e = falsed(self.fold_parts(f, cols)?);
        (!null_arm(&e)).then_some(e)
    }

    /// A target list of the select list `tl` over the range table's `varno` (`t`'s columns).
    fn targets(&self, t: &Table, varno: usize, tl: &[Expr]) -> Vec<Target> {
        tl.iter()
            .map(|e| match e {
                Expr::Col(_, n) if attno(t, n).is_some() => Target::Var(Var { varno, attno: attno(t, n).unwrap_or(0) }),
                e => Target::Expr(PExpr { expr: e.clone(), varno }),
            })
            .collect()
    }

    /// The Result a SELECT without FROM is: its target list, and with COSTS cost_resultscan's numbers (one row) for a
    /// select list of constants.
    fn result_of(&self, s: &Select) -> Plan {
        let mut tl = vec![];
        let mut width = Some(0);
        for it in &s.items {
            let Item::Expr(e, _) = it else { continue };
            tl.push(Target::Expr(PExpr { expr: e.clone(), varno: 0 }));
            width = match (width, const_of(e)) {
                (Some(w), K::Val(v, decl)) => {
                    let ty = decl.or_else(|| match v {
                        Value::Int(n) if i32::try_from(n).is_ok() => Some(Type::Int),
                        Value::Int(_) => Some(Type::BigInt),
                        Value::Text(_) => Some(Type::Text),
                        Value::Bool(_) => Some(Type::Bool),
                        _ => None,
                    });
                    ty.map(|t| w + typavgwidth(t, (0, -1)))
                }
                _ => None,
            };
        }
        let cost = match (self.costs, width) {
            // (cost_resultscan, costsize.c: a tuple's CPU cost, for its one row)
            (true, Some(w)) => Some(Cost { startup: 0.0, total: self.g.cpu_tuple_cost, rows: 1.0, width: w }),
            _ => None,
        };
        Plan { cost, tlist: Some(tl), ..Plan::new(Node::Result { one_time: None }) }
    }

    /// UPDATE or DELETE (`op`) of `table` without FROM: the ModifyTable over the scan of the rows it changes, which
    /// for a partitioned or inherited table names each table it writes.
    fn modify(
        &self,
        op: CmdType,
        table: &str,
        only: bool,
        filter: Option<&Expr>,
        sets: &[&Expr],
    ) -> Result<Option<Plan>> {
        if filter.is_some_and(|f| self.awkward(f))
            || sets.iter().any(|e| self.awkward(e))
            || view_def(self.v, table)?.is_some()
        {
            return Ok(None);
        }
        let Some(t) = load(self.v, table)? else { return Ok(None) };
        if !t.schema
            || t.matview.is_some()
            || t.hier.foreign.is_some()
            || (t.mem.is_some() && !self.temp(&t)?)
            || rls::flags(self.v, &t.name)?.0
        {
            return Ok(None);
        }
        // (the WHERE as the planner leaves it: see `fold_qual`)
        let quals = match filter {
            None => Quals::List(vec![], vec![]),
            Some(f) => match self.fold_qual(f, &t.cols).and_then(|f| scan_quals(&f, &t.cols)) {
                Some(q) => q,
                None => return Ok(None),
            },
        };
        let mut used = vec![];
        if let Some(f) = filter {
            cols_of(f, &mut used);
        }
        let alias = rel_part(&t.name).to_string();
        let Some(sc) = self.scan(&t, only, &alias, &quals, Some(op), &used, &[])? else { return Ok(None) };
        let mut node = sc.plan;
        // (an UPDATE's or DELETE's subplan carries what it writes and the row's identity: no costs or target list
        // are worked out for it)
        strip(&mut node);
        // (an UPDATE of an inheritance tree's tables projects their rows through a Result)
        if op == CmdType::Update && sc.tree && t.hier.part.is_none() && matches!(node.node, Node::Append) {
            node = Plan::above(Node::Result { one_time: None }, node);
        }
        let nominal = 1;
        let targets = if sc.tree { sc.read.clone() } else { vec![nominal] };
        Ok(Some(Plan::above(Node::ModifyTable { op, nominal, targets }, node)))
    }

    /// The scan of `t`, read as `alias` (ONLY: not its children) under `quals`: a Seq Scan, an Append of the scans of
    /// the partitions or children it reads, or a Result where it reads none. `modify`: the tree is an UPDATE's or
    /// DELETE's target, which numbers its tables even when one is left. None where an index on one of the columns
    /// `used` might serve. `carried`: what the scan's rows carry up (the target list it computes), for its width and
    /// VERBOSE's Output.
    #[allow(clippy::too_many_arguments)]
    fn scan(
        &self,
        t: &Table,
        only: bool,
        alias: &str,
        quals: &Quals,
        modify: Option<CmdType>,
        used: &[String],
        carried: &[Expr],
    ) -> Result<Option<Scanned>> {
        let tree = t.hier.is_parent() && !only;
        if only && t.hier.part.is_some() {
            return Ok(None);
        }
        let filter = match quals {
            Quals::False => {
                let varno = self.rte(t, alias, alias);
                let mut plan =
                    Plan::new(Node::Result { one_time: Some(PExpr { expr: Expr::Lit(Value::Bool(false)), varno }) });
                // (a relation proven empty: a dummy path, costing nothing, of the width of its target)
                if self.costs {
                    plan.cost = Some(Cost { startup: 0.0, total: 0.0, rows: 0.0, width: self.width(t, carried) });
                }
                plan.tlist = Some(self.targets(t, varno, carried));
                return Ok(Some(Scanned { plan, read: vec![], tree }));
            }
            Quals::List(q, _) => q,
        };
        let tables: Vec<Table> = if !tree {
            vec![t.clone()]
        } else if t.hier.part.is_some() {
            let mut out = vec![];
            let pq = self.qual_above(t)?;
            if self.leaves(t, filter, &pq, &mut out)?.is_none() {
                return Ok(None);
            }
            out
        } else {
            let mut out = vec![t.clone()];
            for d in hier::descendants(self.v, t)? {
                if d.hier.part.is_some() || d.hier.foreign.is_some() {
                    return Ok(None);
                }
                out.push(d);
            }
            out
        };
        if tables.iter().any(|x| indexed(x, used)) {
            return Ok(None);
        }
        // (an inheritance parent whose children are gone is read as a plain table)
        let tree = tree && (t.hier.part.is_some() || tables.len() > 1);
        // (constraint exclusion: the tables whose constraints the WHERE refutes aren't read)
        let mut kept = vec![];
        for x in tables {
            if !self.excluded(&x, filter, tree)? {
                kept.push(x);
            }
        }
        let tables = kept;
        // (a table of min_parallel_table_scan_size or more gets partial paths (compute_parallel_worker,
        // allpaths.c:4210; create_plain_partial_paths, :797), and a Gather over them is often the cheaper plan:
        // parallel plans aren't made here. Not for a temporary table, which workers can't read
        // (set_rel_consider_parallel, allpaths.c:622), nor an UPDATE's or a DELETE's, which standard_planner makes
        // no parallel plan of (planner.c:349))
        if modify.is_none() {
            match self.parallel {
                ParallelScan::Never => {}
                ParallelScan::Unknown => return Ok(None),
                ParallelScan::From(min) => {
                    for x in &tables {
                        if !self.temp(x)? && self.pages_reach(x, min)? {
                            return Ok(None);
                        }
                    }
                }
            }
        }
        // the range table: the table named, then (in a tree) each table it reads
        let parent = if tree { Some(self.rte(t, alias, alias)) } else { None };
        if tables.is_empty() {
            let mut plan = Plan::new(Node::Result {
                one_time: Some(PExpr { expr: Expr::Lit(Value::Bool(false)), varno: parent.unwrap_or(1) }),
            });
            if self.costs {
                plan.cost = Some(Cost { startup: 0.0, total: 0.0, rows: 0.0, width: self.width(t, carried) });
            }
            plan.tlist = Some(self.targets(t, parent.unwrap_or(1), carried));
            return Ok(Some(Scanned { plan, read: vec![], tree }));
        }
        let numbered = tree && (tables.len() > 1 || modify.is_some());
        let cond = and_all(filter);
        let (mut scans, mut read) = (vec![], vec![]);
        for (i, x) in tables.iter().enumerate() {
            let shown = if numbered { format!("{alias}_{}", i + 1) } else { alias.to_string() };
            let varno = self.rte(x, &shown, alias);
            let mut n = Plan::new(Node::SeqScan { varno });
            if let Some(c) = &cond {
                n.qual.push(PExpr { expr: c.clone(), varno });
            }
            n.tlist = Some(self.targets(x, varno, carried));
            if self.costs {
                n.cost = self.seqscan_cost(x, filter, carried)?;
            }
            read.push(varno);
            scans.push(n);
        }
        let plan = match scans.len() {
            1 => scans.pop().expect("one scan"),
            _ => {
                let cost = match self.costs {
                    true => append_cost(&scans, self.g.cpu_tuple_cost),
                    false => None,
                };
                // (an Append's target list isn't shown)
                Plan { cost, kids: scans, tlist: Some(vec![]), ..Plan::new(Node::Append) }
            }
        };
        Ok(Some(Scanned { plan, read, tree }))
    }

    /// Whether `t` (whose rows a read holds) is a temporary table: its rows are the session's, not a subquery's.
    fn temp(&self, t: &Table) -> Result<bool> {
        // (not one of a temporary partitioned or inherited tree: its tables are the session's, which the world's
        // view this plans with doesn't hold)
        Ok(t.mem.is_some() && !t.hier.is_parent() && t.hier.of.is_none() && temp_table_named(self.v, &t.name)?)
    }

    /// What this planner doesn't plan in `e` (see `awkward`); a parameter is a value only known when the plan runs,
    /// which a generic plan takes as it is.
    fn awkward(&self, e: &Expr) -> bool {
        if self.generic {
            let mut found = false;
            walk(e, &mut |x| {
                found |= match x {
                    Expr::Param(_) => false,
                    x => awkward_one(x),
                }
            });
            return found;
        }
        awkward(e)
    }

    /// relation_excluded_by_constraints (optimizer/util/plancat.c:1449): whether the scan of `x` under `quals` can be
    /// left out, the conditions contradicting each other or the table's constraints. Where constraint_exclusion is
    /// partition (the default), only for a table an inheritance tree or a partitioned table reads (`member`); where
    /// it is on, for any.
    fn excluded(&self, x: &Table, quals: &[Expr], member: bool) -> Result<bool> {
        match self.exclusion {
            Exclusion::Off => return Ok(false),
            Exclusion::Partition if !member => return Ok(false),
            _ => {}
        }
        if quals.is_empty() {
            return Ok(false);
        }
        // (conditions refuting each other, weakly: those of immutable functions only)
        let safe: Vec<Expr> = quals.iter().filter(|q| !mutable(q)).cloned().collect();
        if !safe.is_empty() && refutes(&and_all(&safe).expect("one"), &and_all(&safe).expect("one"), true, &x.cols) {
            return Ok(true);
        }
        // get_relation_constraints (plancat.c:1281): its valid CHECK constraints (its NO INHERIT ones where only it
        // is read), its NOT NULL columns', and where constraint_exclusion is on and it is a partition named on its
        // own, its partition constraint
        let inh = !member && x.hier.is_parent();
        let mut preds = vec![];
        for c in &x.checks {
            if !c.valid || (c.no_inherit && inh) {
                continue;
            }
            let Ok(e) = check_expr(&c.text) else { continue };
            let e = simplify(&e);
            if !mutable(&e) {
                chain(&e, Op::And, &mut preds);
            }
        }
        if !inh || x.hier.part.is_some() {
            for c in x.cols.iter().filter(|c| c.not_null && !c.hidden) {
                preds.push(Expr::IsNull(Box::new(Expr::Col(None, c.name.clone())), true));
            }
        }
        if self.exclusion == Exclusion::On && !member {
            preds.extend(self.qual_above(x)?);
        }
        Ok(refuted_by(&preds, quals, &x.cols))
    }

    /// The partition constraint of `t` where it is a partition read on its own (RelationGetPartitionQual,
    /// partcache.c:353): its parents' constraints and its own bound's, over their keys, which prune its own default
    /// partition too.
    fn qual_above(&self, t: &Table) -> Result<Vec<Expr>> {
        let mut out = vec![];
        let mut cur = t.clone();
        for _ in 0..64 {
            let Some((parent, _)) = cur.hier.of.clone() else { break };
            let Some(p) = load(self.v, &parent)? else { break };
            let Some(pk) = &p.hier.part else { break };
            let key = Key::of(&p, pk)?;
            let (kids, bounds) = self.bounds_of(&p, &key)?;
            let Some(i) = kids.iter().position(|k| k.name == cur.name) else { break };
            // (generate_partition_qual: the parent's first)
            let mine = partition_qual(&key, &bounds[i], &bounds);
            out.splice(0..0, mine);
            cur = p;
        }
        Ok(out)
    }

    /// The partitions of `t` in bound order, with their bounds typed as its key's columns.
    fn bounds_of(&self, t: &Table, key: &Key) -> Result<(Vec<Table>, Vec<PartBound>)> {
        let kids = hier::kids_in_order(self.v, t)?;
        let mut bounds = vec![];
        for k in &kids {
            bounds.push(match &k.hier.of {
                Some((_, b)) => b.typed(&key.types)?,
                None => PartBound::Default,
            });
        }
        Ok((kids, bounds))
    }

    /// The tables holding rows below partitioned table `t` that a read under `quals` reads, in the order Postgres's
    /// Append has them (bound order, depth first), each level's partitions as prune_append_rel_partitions
    /// (partprune.c:752) keeps them: `pq`, `t`'s partition constraint, prunes its default partition too. None where a
    /// partition is a foreign table, or a key is compared with a value only known when the plan runs (which Postgres
    /// prunes by as the plan starts).
    fn leaves(&self, t: &Table, quals: &[Expr], pq: &[Expr], out: &mut Vec<Table>) -> Result<Option<()>> {
        let Some(pk) = &t.hier.part else {
            out.push(t.clone());
            return Ok(Some(()));
        };
        let key = Key::of(t, pk)?;
        let (kids, bounds) = self.bounds_of(t, &key)?;
        let bi = BoundInfo::new(key.strategy, &bounds);
        let keep: Vec<usize> = if !self.prune || quals.is_empty() || kids.is_empty() {
            (0..kids.len()).collect()
        } else {
            // (with a default partition, the table's own constraint is a clause too: gen_partprune_steps)
            let with_default = bi.default_index.is_some();
            let mut clauses = quals.to_vec();
            if with_default {
                clauses.extend(pq.iter().cloned());
            }
            let mut p = Prune {
                pk,
                hash: self.hash,
                key: &key,
                bi: &bi,
                pq: if with_default { pq } else { &[] },
                cols: &t.cols,
                steps: vec![],
                contradictory: false,
                mutable: false,
                generic: self.generic,
            };
            p.internal(&clauses);
            if p.mutable {
                return Ok(None);
            }
            match p.contradictory {
                true => vec![],
                false => p.matching(kids.len(), &key.types),
            }
        };
        for i in keep {
            let k = &kids[i];
            if k.hier.foreign.is_some() {
                return Ok(None);
            }
            if k.hier.part.is_some() {
                let mut below = pq.to_vec();
                below.extend(partition_qual(&key, &bounds[i], &bounds));
                if self.leaves(k, quals, &below, out)?.is_none() {
                    return Ok(None);
                }
            } else {
                out.push(k.clone());
            }
        }
        Ok(Some(()))
    }

    // ------------------------------------------------------------ the model's costs

    /// estimate_rel_size (optimizer/util/plancat.c:1037) for a heap, table_block_relation_estimate_size
    /// (access/table/tableam.c:663): the pages and rows `t` is estimated at. Its pages are those its rows would take
    /// in a heap (see `heap_pages`), at least 10 while it was never vacuumed and has no children; its rows from the
    /// density VACUUM recorded, or else from the widths of its columns' types.
    fn rel_size(&self, t: &Table) -> Result<(f64, f64)> {
        let (relpages, reltuples) = class_stats(t);
        let mut curpages = heap_pages(self.v, t, None)?;
        // (the 10-page rule: tableam.c:713, for a table never vacuumed, which reltuples < 0 says)
        if curpages < 10.0 && reltuples < 0.0 && !t.hier.kids {
            curpages = 10.0;
        }
        if curpages == 0.0 {
            return Ok((0.0, 0.0));
        }
        let density = if reltuples >= 0.0 && relpages > 0.0 {
            reltuples / relpages
        } else {
            // (the tuple's data, MAXALIGN(SizeofHeapTupleHeader) and an ItemIdData; integer division: tableam.c:745)
            let width = rel_data_width(t) + 24 + 4;
            f64::from((8192 - 24) / width.max(1))
        };
        Ok((curpages, (density * curpages).round_ties_even()))
    }

    /// Whether `t` is estimated at `min` pages or more (estimate_rel_size's, as `rel_size` works them out), reading
    /// its rows only until it is.
    fn pages_reach(&self, t: &Table, min: f64) -> Result<bool> {
        let (_, reltuples) = class_stats(t);
        // (the 10-page rule)
        if min <= 10.0 && reltuples < 0.0 && !t.hier.kids && t.hier.part.is_none() {
            return Ok(true);
        }
        Ok(heap_pages(self.v, t, Some(min))? >= min)
    }

    /// The width of a row carrying `carried` (set_rel_width and set_pathtarget_cost_width, costsize.c): each column's
    /// average width (get_typavgwidth: there are no statistics), each other expression its type's.
    fn width(&self, t: &Table, carried: &[Expr]) -> i32 {
        carried
            .iter()
            .map(|e| match e {
                // (the system column every row has: an oid)
                Expr::Col(_, n) if n == "tableoid" => 4,
                Expr::Col(_, n) => t.cols.iter().find(|c| c.name == *n && !c.hidden).map_or(32, |c| {
                    let (_, ty, m) = col_ty(c);
                    typavgwidth(ty, m)
                }),
                e => ty_in(e, &t.cols, &[], &[]).map_or(32, |ty| typavgwidth(ty, (0, -1))),
            })
            .sum()
    }

    /// cost_seqscan (costsize.c:282) of table `x` under `filter`, its rows carrying `carried`; None where a
    /// condition's selectivity isn't one this works out.
    fn seqscan_cost(&self, x: &Table, filter: &[Expr], carried: &[Expr]) -> Result<Option<Cost>> {
        let (pages, tuples) = self.rel_size(x)?;
        let Some(sel) = clauselist_selectivity(filter, x, tuples) else { return Ok(None) };
        let rows = clamp_row_est(tuples * sel);
        let qual_per_tuple: f64 = filter.iter().map(|e| cost(e, &x.cols)).sum::<f64>() * self.g.cpu_operator_cost;
        let mut startup = 0.0;
        if !self.g.enable_seqscan {
            startup += DISABLE_COST;
        }
        let disk = self.g.seq_page_cost * pages;
        let mut cpu = (self.g.cpu_tuple_cost + qual_per_tuple) * tuples;
        // (the target list's expressions, computed per row the scan gives: costsize.c:318)
        let tl_per_tuple: f64 = carried.iter().map(|e| cost(e, &x.cols)).sum::<f64>() * self.g.cpu_operator_cost;
        cpu += tl_per_tuple * rows;
        Ok(Some(Cost { startup, total: startup + cpu + disk, rows, width: self.width(x, carried) }))
    }

    /// cost_sort (costsize.c:2109) over `input`, a LIMIT of `limit_tuples` above it.
    fn sort_cost(&self, input: Cost, limit_tuples: Option<f64>) -> Cost {
        let (startup, run) = tuplesort_cost(input.rows, input.width, self.g.work_mem, limit_tuples, &self.g);
        let mut startup = startup;
        if !self.g.enable_sort {
            startup += DISABLE_COST;
        }
        startup += input.total;
        Cost { startup, total: startup + run, rows: input.rows, width: input.width }
    }

    /// cost_agg (costsize.c:2623) for AGG_PLAIN over `input`, of the aggregates in `tl`: each transition function's
    /// cost per row and each final function's once (get_agg_clause_costs, prepagg.c); None for an aggregate whose
    /// functions this doesn't know.
    fn agg_cost(&self, tl: &[Expr], input: Cost, t: &Table) -> Option<Cost> {
        let (mut trans, mut fin, mut width) = (0.0, 0.0, 0);
        let mut seen: Vec<String> = vec![];
        for e in tl {
            let Expr::Agg(a, arg, false) = e else { return None };
            let arg_ty = match arg {
                Some(x) => Some(ty_in(x, &t.cols, &[], &[]).ok()?),
                None => None,
            };
            // (the result's type, and whether a final function runs)
            let (out, final_fn): (Type, bool) = match (a, arg_ty) {
                (Agg::Count, _) => (Type::BigInt, false),
                (Agg::Min | Agg::Max, Some(ty)) => (ty, false),
                (Agg::Sum, Some(Type::SmallInt | Type::Int)) => (Type::BigInt, false),
                (Agg::Sum, Some(Type::Float)) => (Type::Float, false),
                _ => return None,
            };
            // (one transition state per distinct aggregate: find_compatible_trans)
            let key = format!("{e:?}");
            if !seen.contains(&key) {
                seen.push(key);
                trans += self.g.cpu_operator_cost;
                if let Some(x) = arg {
                    trans += cost(x, &t.cols) * self.g.cpu_operator_cost;
                }
            }
            if final_fn {
                fin += self.g.cpu_operator_cost;
            }
            width += typavgwidth(out, (0, -1));
        }
        let startup = input.total + trans * input.rows + fin;
        Some(Cost { startup, total: startup + self.g.cpu_tuple_cost, rows: 1.0, width })
    }
}

/// A target list without what an UPDATE's or DELETE's subplan computes: its own (the new values, the row's
/// identity) are not worked out here.
fn strip(p: &mut Plan) {
    p.cost = None;
    p.tlist = None;
    for k in &mut p.kids {
        strip(k);
    }
}

/// A plan without its target lists: VERBOSE can't be shown.
fn no_tlist(p: &mut Plan) {
    p.tlist = None;
    for k in &mut p.kids {
        no_tlist(k);
    }
}

/// Whether a select-list entry is written back as the planner leaves it: not a row value, nor a cast to a row type
/// (convert_rowtype casts the planner folds together).
fn plain_target(e: &Expr) -> bool {
    let mut ok = true;
    walk(e, &mut |x| {
        ok &= !matches!(x, Expr::Func(Func::RowValue, _) | Expr::Cast(_, Type::Record(_)));
    });
    ok
}

/// preprocess_limit's estimates (planner.c:2563) of a LIMIT and an OFFSET, where both are constants: the count (0
/// for none or NULL, at least 1) and the offset (0 for none, NULL or one below 0).
fn limit_ests(limit: Option<&Expr>, offset: Option<&Expr>) -> Option<(f64, f64)> {
    let count_est = match limit {
        None => 0.0,
        Some(e) => match const_of(e) {
            K::Val(Value::Null, _) => 0.0,
            K::Val(Value::Int(n), _) => (n as f64).max(1.0),
            _ => return None,
        },
    };
    let offset_est = match offset {
        None => 0.0,
        Some(e) => match const_of(e) {
            K::Val(Value::Null, _) => 0.0,
            K::Val(Value::Int(n), _) => (n as f64).max(0.0),
            _ => return None,
        },
    };
    Some((count_est, offset_est))
}

/// create_limit_path's cost (adjust_limit_rows_costs, pathnode.c:3909) over `input`: None where LIMIT or OFFSET
/// isn't a constant.
fn limit_cost(input: Cost, limit: Option<&Expr>, offset: Option<&Expr>) -> Option<Cost> {
    let (count_est, offset_est) = limit_ests(limit, offset)?;
    let (input_rows, input_startup, input_total) = (input.rows, input.startup, input.total);
    let (mut rows, mut startup, mut total) = (input.rows, input.startup, input.total);
    if offset_est != 0.0 {
        let offset_rows = offset_est.min(rows);
        if input_rows > 0.0 {
            startup += (input_total - input_startup) * offset_rows / input_rows;
        }
        rows = (rows - offset_rows).max(1.0);
    }
    if count_est != 0.0 {
        let count_rows = count_est.min(rows);
        if input_rows > 0.0 {
            total = startup + (input_total - input_startup) * count_rows / input_rows;
        }
        rows = count_rows.max(1.0);
    }
    Some(Cost { startup, total, rows, width: input.width })
}

/// cost_append (costsize.c:2262) of an unordered, non-parallel Append of `kids`: the first's startup, the sums of
/// their rows and costs, and half a tuple's cost per row (APPEND_CPU_COST_MULTIPLIER); its width the rows' average
/// (set_append_rel_size, allpaths.c:1167).
fn append_cost(kids: &[Plan], cpu_tuple_cost: f64) -> Option<Cost> {
    let costs: Vec<Cost> = kids.iter().map(|k| k.cost).collect::<Option<_>>()?;
    let startup = costs.first().map_or(0.0, |c| c.startup);
    let rows: f64 = costs.iter().map(|c| c.rows).sum();
    let mut total: f64 = costs.iter().map(|c| c.total).sum();
    total += cpu_tuple_cost * 0.5 * rows;
    let size: f64 = costs.iter().map(|c| c.rows * f64::from(c.width)).sum();
    let width = if rows > 0.0 { (size / rows).round_ties_even() as i32 } else { costs.first().map_or(0, |c| c.width) };
    Some(Cost { startup, total, rows, width })
}

/// cost_tuplesort (costsize.c:1968): the startup and run costs of sorting `tuples` rows of `width`, in `work_mem` kB,
/// keeping `limit` of them where a LIMIT says how many.
fn tuplesort_cost(tuples: f64, width: i32, work_mem: f64, limit: Option<f64>, g: &Gucs) -> (f64, f64) {
    let relation_byte_size = |n: f64| n * f64::from(maxalign(width) + 24);
    let input_bytes = relation_byte_size(tuples);
    let sort_mem_bytes = work_mem * 1024.0;
    let tuples = tuples.max(2.0);
    let comparison_cost = 2.0 * g.cpu_operator_cost;
    let (output_tuples, output_bytes) = match limit {
        Some(l) if l > 0.0 && l < tuples => (l, relation_byte_size(l)),
        _ => (tuples, input_bytes),
    };
    let startup = if output_bytes > sort_mem_bytes {
        let npages = (input_bytes / 8192.0).ceil();
        let nruns = input_bytes / sort_mem_bytes;
        // tuplesort_merge_order (tuplesort.c): a tape's buffer and a merge's, MINORDER 6 to MAXORDER 500
        let merge_order = ((sort_mem_bytes / (2.0 * 8192.0 + 32.0 * 8192.0)).floor()).clamp(6.0, 500.0);
        let log_runs = if nruns > merge_order { (nruns.ln() / merge_order.ln()).ceil() } else { 1.0 };
        let page_accesses = 2.0 * npages * log_runs;
        comparison_cost * tuples * tuples.log2() + page_accesses * (g.seq_page_cost * 0.75 + g.random_page_cost * 0.25)
    } else if tuples > 2.0 * output_tuples || input_bytes > sort_mem_bytes {
        comparison_cost * tuples * (2.0 * output_tuples).log2()
    } else {
        comparison_cost * tuples * tuples.log2()
    };
    (startup, g.cpu_operator_cost * tuples)
}

/// MAXALIGN: up to 8 bytes.
fn maxalign(n: i32) -> i32 {
    (n + 7) & !7
}

/// clamp_row_est (costsize.c:198): at least one row, a whole number.
fn clamp_row_est(n: f64) -> f64 {
    if n.is_nan() || n > 1e100 {
        1e100
    } else if n <= 1.0 {
        1.0
    } else {
        n.round_ties_even()
    }
}

/// pg_class's relpages and reltuples for `t`, as estimate_rel_size reads them: what VACUUM or ANALYZE recorded, or
/// (0, -1) for a table never vacuumed, which is what pg_class shows here (see `catalog_objects::class_rows`).
fn class_stats(_t: &Table) -> (f64, f64) {
    (0.0, -1.0)
}

/// Whether the session's temporary tables hold one named `name`.
fn temp_table_named(v: &View, name: &str) -> Result<bool> {
    Ok(temp_table(v, name)?.is_some() && load_world(v, name)?.is_none())
}

// ---------------------------------------------------------------- the emulated heap

/// A type's facts as pg_type has them: its length (-1 a varlena), its alignment in bytes, and whether a value may
/// be stored with a short header (attstorage other than plain).
fn type_layout(ty: Type, m: (i64, i64)) -> (i64, usize, bool) {
    if matches!(ty, Type::Enum(_)) {
        return (4, 4, false);
    }
    let oid = if m.0 != 0 { m.0 } else { i64::from(ty.oid()) };
    match catalog_schema::type_facts(oid) {
        Some((len, _, align, storage, _)) => {
            let a = match align.as_str() {
                "c" => 1,
                "s" => 2,
                "d" => 8,
                _ => 4,
            };
            (len, a, storage != "p")
        }
        None => (-1, 4, true),
    }
}

/// get_typavgwidth (utils/cache/lsyscache.c:2574) of a type and modifier: a fixed length as it is; a varchar's or
/// numeric's from its maximum size (type_maximum_size, format_type.c:408: four bytes a character in UTF8); else 32.
pub(super) fn typavgwidth(ty: Type, m: (i64, i64)) -> i32 {
    let (len, _, _) = type_layout(ty, m);
    if len > 0 {
        return len as i32;
    }
    let (oid, typmod) = (if m.0 != 0 { m.0 } else { i64::from(ty.oid()) }, m.1);
    let maxwidth: i64 = if typmod < 0 {
        -1
    } else {
        match oid {
            1042 | 1043 => (typmod - 4) * 4 + 4,
            // numeric_maximum_size (numeric.c): its precision's NumericDigits and an 8-byte header
            1700 => {
                let precision = ((typmod - 4) >> 16) & 0xffff;
                8 + ((precision + 2 * 3) / 4) * 2
            }
            1560 | 1562 => (typmod + 7) / 8 + 8,
            _ => -1,
        }
    };
    if maxwidth > 0 {
        if oid == 1042 || maxwidth <= 32 {
            return maxwidth as i32;
        }
        if maxwidth < 1000 {
            return (32 + (maxwidth - 32) / 2) as i32;
        }
        return 32 + (1000 - 32) / 2;
    }
    32
}

/// get_rel_data_width (plancat.c:1162): the sum of its columns' average widths (there are no statistics).
fn rel_data_width(t: &Table) -> i32 {
    t.cols
        .iter()
        .filter(|c| !c.hidden)
        .map(|c| {
            let (_, ty, m) = col_ty(c);
            typavgwidth(ty, m)
        })
        .sum()
}

/// The bytes a varlena value's data takes (without its header), as Postgres stores the type.
fn varlena_len(v: &Value, ty: Type, c: &Col) -> usize {
    match v {
        Value::Text(s) => match c.chars {
            // (a char(n) is kept padded to its length)
            Some((n, true)) if n != VARLEN => s.len() + (n as usize).saturating_sub(s.chars().count()),
            _ => s.len(),
        },
        Value::Bytea(b) => b.len(),
        Value::JsonText(s) => s.len(),
        // numeric's short format (numeric.c): a 2-byte header and 2 bytes a base-10000 digit, the long one 4
        Value::Numeric(d) => match d.groups() {
            None => 2,
            Some((g, weight)) => {
                let short = d.scale() <= 63 && (-64..=63).contains(&weight);
                (if short { 2 } else { 4 }) + 2 * g.len()
            }
        },
        Value::Int(n) if ty == Type::Numeric => {
            let digits = n.unsigned_abs().to_string().len();
            2 + 2 * digits.div_ceil(4)
        }
        // inet and cidr: the family, the bits, and the address
        Value::Net(net::Net::Inet(a) | net::Net::Cidr(a)) => 2 + if a.v6 { 16 } else { 4 },
        v => v.to_text().len(),
    }
}

/// The length of a heap tuple holding `vals` (heap_form_tuple, access/common/heaptuple.c:1094): the header, with a
/// null bitmap where a value is null, MAXALIGNed, then each value as heap_compute_data_size lays it out; a tuple
/// over TOAST_TUPLE_THRESHOLD has its largest values moved out of line (an 18-byte pointer each) until it fits (no
/// compression is modelled).
fn tuple_len(t: &Table, vals: &[Value]) -> usize {
    let natts = t.cols.iter().filter(|c| !c.hidden).count() + t.dropped.len();
    let hasnull = vals.iter().any(|v| matches!(v, Value::Null)) || !t.dropped.is_empty();
    let hoff = (23 + if hasnull { natts.div_ceil(8) } else { 0 } + 7) & !7;
    // each value: (fixed length or the varlena's data length, alignment, packable)
    let mut items: Vec<(i64, usize, usize, bool)> = vec![];
    for (c, v) in t.cols.iter().filter(|c| !c.hidden).zip(vals) {
        if matches!(v, Value::Null) {
            continue;
        }
        let (_, ty, m) = col_ty(c);
        let (len, align, packable) = type_layout(ty, m);
        let data = if len > 0 { len as usize } else { varlena_len(v, ty, c) };
        items.push((len, data, align, packable));
    }
    let size = |items: &[(i64, usize, usize, bool)]| {
        let mut off = 0usize;
        for &(len, data, align, packable) in items {
            if len == -1 && packable && data < 127 {
                off += data + 1;
            } else if len == -1 {
                off = off.next_multiple_of(align) + data + 4;
            } else {
                off = off.next_multiple_of(align) + data;
            }
        }
        off
    };
    let mut total = hoff + size(&items);
    // (heap_toast_insert_or_update: the largest varlena out of line, while the tuple is over the threshold)
    while total > 2032 {
        let Some(i) = items
            .iter()
            .enumerate()
            .filter(|(_, x)| x.0 == -1 && x.3 && x.1 > 18)
            .max_by_key(|(_, x)| x.1)
            .map(|(i, _)| i)
        else {
            break;
        };
        items[i].1 = 17;
        total = hoff + size(&items);
    }
    total
}

/// The pages `t`'s rows would take in a heap, filled as inserts fill them (RelationGetBufferForTuple, hio.c, at the
/// default fillfactor): each tuple MAXALIGNed with a 4-byte line pointer, while it fits a page's 8168 bytes after its
/// header, at most MaxHeapTuplesPerPage (291) a page. Only the rows there now are counted: the dead ones an UPDATE or
/// DELETE leaves in Postgres until VACUUM aren't. Past 30,000 rows the pages are taken in proportion to the rows, at
/// as many a page as the first 30,000's full pages held. `upto`: the count stops once the pages reach it.
fn heap_pages(v: &View, t: &Table, upto: Option<f64>) -> Result<f64> {
    if t.hier.part.is_some() {
        return Ok(0.0);
    }
    const SAMPLE: usize = 30_000;
    let reached = |pages: f64| upto.is_some_and(|u| pages >= u);
    let mut heap = Heap::default();
    if let Some(mem) = &t.mem {
        for vals in mem.iter() {
            heap.add(tuple_len(t, vals));
            if reached(heap.pages) {
                break;
            }
        }
        return Ok(heap.pages);
    }
    let mut out = vec![Value::Null; t.cols.len()];
    let (mut rows, mut added, mut per_page) = (0usize, 0usize, 1.0);
    for r in v.prefix(&format!("{}/", t.name))? {
        let (_, raw) = r?;
        rows += 1;
        if rows <= SAMPLE {
            if unpack_row(t, raw.as_ref(), &mut out, None).is_some() {
                let vals: Vec<Value> =
                    t.cols.iter().zip(&out).filter(|(c, _)| !c.hidden).map(|(_, v)| v.clone()).collect();
                heap.add(tuple_len(t, &vals));
                added += 1;
            }
            if reached(heap.pages) {
                return Ok(heap.pages);
            }
            if rows == SAMPLE {
                // (the tuples a full page held: all the pages but the last, which may be partly filled)
                per_page = match heap.pages > 1.0 {
                    true => (added - heap.n) as f64 / (heap.pages - 1.0),
                    false => added.max(1) as f64,
                };
            }
        } else if reached((rows as f64 / per_page).ceil()) {
            break;
        }
    }
    if rows > SAMPLE {
        return Ok((rows as f64 / per_page).ceil());
    }
    Ok(heap.pages)
}

/// Heap pages being filled: how many, and the bytes and line pointers of the last.
#[derive(Default)]
struct Heap {
    pages: f64,
    used: usize,
    n: usize,
}

impl Heap {
    /// A tuple of `len` bytes added: on the last page where it fits (its MAXALIGNed length and a line pointer, in the
    /// 8168 bytes after the page header; at most 291 a page), else on a new one.
    fn add(&mut self, len: usize) {
        let need = (len + 7) & !7;
        if self.pages == 0.0 || self.n >= 291 || 8192 - 24 - self.used < need + 4 {
            self.pages += 1.0;
            (self.used, self.n) = (0, 0);
        }
        self.used += need + 4;
        self.n += 1;
    }
}

// ---------------------------------------------------------------- selectivity

/// selfuncs.h's defaults, for what there are no statistics on
const DEFAULT_INEQ_SEL: f64 = 0.3333333333333333;
const DEFAULT_RANGE_INEQ_SEL: f64 = 0.005;
const DEFAULT_UNK_SEL: f64 = 0.005;
const DEFAULT_NOT_UNK_SEL: f64 = 1.0 - DEFAULT_UNK_SEL;
const DEFAULT_NUM_DISTINCT: f64 = 200.0;

/// clauselist_selectivity (optimizer/path/clausesel.c:100) of conditions AND-ed on table `t` of `tuples` rows,
/// without statistics: each one's own multiplied in, except the `<`/`>` comparisons of one term with a constant,
/// which pair up as a range (clausesel.c:198-260). None where a condition's selectivity function isn't ported.
fn clauselist_selectivity(clauses: &[Expr], t: &Table, tuples: f64) -> Option<f64> {
    if clauses.len() == 1 {
        return clause_selectivity(&clauses[0], t, tuples);
    }
    // (term, low bound's selectivity, high bound's): rqlist
    let mut ranges: Vec<(String, Option<f64>, Option<f64>)> = vec![];
    let mut s1 = 1.0;
    for c in clauses {
        let s2 = clause_selectivity(c, t, tuples)?;
        if let Expr::Bin(a, op @ (Op::Lt | Op::Le | Op::Gt | Op::Ge), b) = c {
            let (term, lt) = match (has_col(a), has_col(b)) {
                (true, false) => (term_key(a), matches!(op, Op::Lt | Op::Le)),
                (false, true) => (term_key(b), matches!(op, Op::Gt | Op::Ge)),
                _ => {
                    s1 *= s2;
                    continue;
                }
            };
            // (x < c is a high bound of x: addRangeClause, clausesel.c:419; the more restrictive of two kept)
            let lo = !lt;
            let at = match ranges.iter().position(|r| r.0 == term) {
                Some(i) => i,
                None => {
                    ranges.insert(0, (term, None, None));
                    0
                }
            };
            let slot = if lo { &mut ranges[at].1 } else { &mut ranges[at].2 };
            *slot = Some(slot.map_or(s2, |x: f64| x.min(s2)));
            continue;
        }
        s1 *= s2;
    }
    for (_, lo, hi) in ranges {
        match (lo, hi) {
            (Some(lo), Some(hi)) => {
                let s2 = if hi == DEFAULT_INEQ_SEL || lo == DEFAULT_INEQ_SEL {
                    DEFAULT_RANGE_INEQ_SEL
                } else {
                    let s = hi + lo - 1.0 + DEFAULT_UNK_SEL;
                    if s <= 0.0 { if s < -0.01 { DEFAULT_RANGE_INEQ_SEL } else { 1.0e-10 } } else { s }
                };
                s1 *= s2;
            }
            (Some(x), None) | (None, Some(x)) => s1 *= x,
            (None, None) => {}
        }
    }
    Some(s1)
}

/// clause_selectivity (clausesel.c:667) of one condition, without statistics; None for one not ported.
fn clause_selectivity(c: &Expr, t: &Table, tuples: f64) -> Option<f64> {
    let clamp = |s: f64| s.clamp(0.0, 1.0);
    Some(match c {
        Expr::Lit(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Expr::Lit(Value::Null) => 0.0,
        Expr::Bin(_, Op::And, _) => {
            let mut parts = vec![];
            chain(c, Op::And, &mut parts);
            clauselist_selectivity(&parts, t, tuples)?
        }
        // (an OR's arms as independent: s1 + s2 - s1 * s2, clausesel.c:740)
        Expr::Bin(_, Op::Or, _) => {
            let mut parts = vec![];
            chain(c, Op::Or, &mut parts);
            let mut s1 = 0.0;
            for p in &parts {
                let s2 = clause_selectivity(p, t, tuples)?;
                s1 = s1 + s2 - s1 * s2;
            }
            s1
        }
        Expr::Not(x) => 1.0 - clause_selectivity(x, t, tuples)?,
        Expr::Bin(a, op @ (Op::Eq | Op::Ne), b) => {
            let (var, other) = match (has_col(a), has_col(b)) {
                (true, false) => (&**a, &**b),
                (false, true) => (&**b, &**a),
                _ => return None,
            };
            // var_eq_const (selfuncs.c:296): a null constant matches nothing; else one value of those it may have
            let eq = match const_of(other) {
                K::Val(Value::Null, _) => 0.0,
                K::Val(..) | K::Later => 1.0 / num_distinct(var, t, tuples)?,
                K::No => return None,
            };
            if *op == Op::Eq { clamp(eq) } else { clamp(1.0 - eq) }
        }
        // scalarineqsel without statistics (selfuncs.c:589): the default
        Expr::Bin(a, Op::Lt | Op::Le | Op::Gt | Op::Ge, b) => match (has_col(a), has_col(b)) {
            (true, false) | (false, true) => {
                let other = if has_col(a) { b } else { a };
                match const_of(other) {
                    K::Val(Value::Null, _) => 0.0,
                    K::Val(..) | K::Later => DEFAULT_INEQ_SEL,
                    K::No => return None,
                }
            }
            _ => return None,
        },
        // nulltestsel without statistics (selfuncs.c:1700)
        Expr::IsNull(x, neg) if has_col(x) => {
            if *neg {
                DEFAULT_NOT_UNK_SEL
            } else {
                DEFAULT_UNK_SEL
            }
        }
        // boolvarsel (selfuncs.c:1514): a boolean column alone is it = true
        Expr::Col(..) => 1.0 / num_distinct(c, t, tuples)?,
        _ => return None,
    })
}

/// get_variable_numdistinct (selfuncs.c:5728) without statistics: a boolean's 2, a column a unique index (or
/// primary key) has alone as many as there are rows, else the rows where under DEFAULT_NUM_DISTINCT, else that.
fn num_distinct(var: &Expr, t: &Table, tuples: f64) -> Option<f64> {
    let col = match var {
        Expr::Col(_, n) => t.cols.iter().find(|c| c.name == *n && !c.hidden),
        _ => None,
    };
    let ty = match col {
        Some(c) => c.ty,
        None => ty_in(var, &t.cols, &[], &[]).ok()?,
    };
    // (has_unique_index, plancat.c: a unique index of that column alone)
    let unique = col.is_some_and(|c| {
        let i = t.cols.iter().position(|x| x.name == c.name);
        (t.key.is_some() && t.key == i)
            || (t.key.is_none() && t.pk.len() == 1 && t.pk.first() == i.as_ref())
            || t.uniques.iter().any(|u| u.cols == [c.id])
    });
    let stadistinct = if ty == Type::Bool { 2.0 } else { 0.0 };
    let stadistinct = if unique { -1.0 } else { stadistinct };
    if stadistinct > 0.0 {
        return Some(clamp_row_est(stadistinct));
    }
    if tuples <= 0.0 {
        return Some(DEFAULT_NUM_DISTINCT);
    }
    if stadistinct < 0.0 {
        return Some(clamp_row_est(-stadistinct * tuples));
    }
    if tuples < DEFAULT_NUM_DISTINCT {
        return Some(clamp_row_est(tuples));
    }
    Some(DEFAULT_NUM_DISTINCT)
}

/// Whether `x` is something `awkward` refuses on its own (not looking inside it).
fn awkward_one(x: &Expr) -> bool {
    match x {
        Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) | Expr::Window(_) | Expr::WinRef(..) => true,
        Expr::Param(_) | Expr::Idx(_) | Expr::AggRef(_) | Expr::Call(_) | Expr::Default => true,
        Expr::Func(f, _) => SRFS.contains(&func_name(f).as_str()),
        Expr::Agg(_, _, distinct) => *distinct,
        _ => false,
    }
}

/// Whether `e` holds what this planner doesn't plan: a subquery, a window function, a parameter, a function made
/// with CREATE FUNCTION (which Postgres may inline, and costs otherwise), a set-returning function.
fn awkward(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| found |= awkward_one(x));
    found
}

/// The names of the columns `e` reads, added to `out`.
fn cols_of(e: &Expr, out: &mut Vec<String>) {
    walk(e, &mut |x| {
        if let Expr::Col(_, n) = x
            && !out.contains(n)
        {
            out.push(n.clone());
        }
    });
}

/// Whether an index of `t` (a primary key's or a unique constraint's too) starts with one of the columns `used`.
fn indexed(t: &Table, used: &[String]) -> bool {
    let name = |id: u32| t.cols.iter().find(|c| c.id == id).map(|c| c.name.clone());
    let mut firsts: Vec<String> = vec![];
    for x in &t.indexes {
        match x.cols.first() {
            Some(&btree::EXPR) => {
                // (an expression index: the columns its first part reads)
                if let Some(e) = x.exprs.first().and_then(|s| check_expr(s).ok()) {
                    cols_of(&e, &mut firsts);
                }
            }
            Some(&id) => firsts.extend(name(id)),
            None => {}
        }
    }
    for u in &t.uniques {
        firsts.extend(u.cols.first().and_then(|&id| name(id)));
    }
    if let Some(&k) = t.key_cols().first() {
        firsts.extend(t.cols.get(k).map(|c| c.name.clone()));
    }
    firsts.iter().any(|f| used.contains(f))
}

/// Conditions AND-ed, left to right; None for none.
fn and_all(conds: &[Expr]) -> Option<Expr> {
    let mut it = conds.iter().cloned();
    let first = it.next()?;
    Some(it.fold(first, |acc, c| bin(acc, Op::And, c)))
}

// ---------------------------------------------------------------- conditions as the planner keeps them

/// The operands of a chain of `op` (AND or OR), however it nests.
fn chain(e: &Expr, op: Op, out: &mut Vec<Expr>) {
    match e {
        Expr::Bin(a, o, b) if *o == op => {
            chain(a, op, out);
            chain(b, op, out);
        }
        e => out.push(e.clone()),
    }
}

/// `a op b` (AND or OR) with constants folded away, as eval_const_expressions leaves it.
fn bool_op(op: Op, a: Expr, b: Expr) -> Expr {
    let (absorb, unit) = if op == Op::And { (false, true) } else { (true, false) };
    let lit = |e: &Expr| match e {
        Expr::Lit(Value::Bool(x)) => Some(*x),
        _ => None,
    };
    let (la, lb) = (lit(&a), lit(&b));
    if la == Some(absorb) || lb == Some(absorb) {
        Expr::Lit(Value::Bool(absorb))
    } else if la == Some(unit) {
        b
    } else if lb == Some(unit) {
        a
    } else {
        bin(a, op, b)
    }
}

/// `e` as eval_const_expressions and canonicalize_qual leave a condition: NOT taken into what it negates, a
/// comparison with a boolean constant made the boolean itself (or its NOT), true and false folded away, and every
/// chain of ANDs or ORs one flat list, left to right.
fn simplify(e: &Expr) -> Expr {
    match e {
        Expr::Bin(_, op @ (Op::And | Op::Or), _) => {
            let mut parts = vec![];
            chain(e, *op, &mut parts);
            let mut it = parts.iter().map(simplify);
            let first = it.next().unwrap_or(Expr::Lit(Value::Bool(*op == Op::And)));
            // (a chain the simplified parts make is flattened into this one)
            let mut flat = vec![];
            chain(&first, *op, &mut flat);
            for p in it {
                chain(&p, *op, &mut flat);
            }
            let mut it = flat.into_iter();
            let first = it.next().unwrap_or(Expr::Lit(Value::Bool(*op == Op::And)));
            it.fold(first, |acc, p| bool_op(*op, acc, p))
        }
        Expr::Not(x) => negate(&simplify(x)),
        Expr::Bin(a, op @ (Op::Eq | Op::Ne), b) => match (&**a, &**b) {
            (x, Expr::Lit(Value::Bool(v))) | (Expr::Lit(Value::Bool(v)), x) if !matches!(x, Expr::Lit(_)) => {
                let x = simplify(x);
                if *v == (*op == Op::Eq) { x } else { negate(&x) }
            }
            _ => e.clone(),
        },
        _ => e.clone(),
    }
}

/// NOT `e`, as negate_clause makes it: an operator its negator, a test its opposite, AND and OR by De Morgan.
fn negate(e: &Expr) -> Expr {
    let neg_op = |op: Op| {
        Some(match op {
            Op::Eq => Op::Ne,
            Op::Ne => Op::Eq,
            Op::Lt => Op::Ge,
            Op::Le => Op::Gt,
            Op::Gt => Op::Le,
            Op::Ge => Op::Lt,
            Op::Regex { ci, neg } => Op::Regex { ci, neg: !neg },
            _ => return None,
        })
    };
    match e {
        Expr::Lit(Value::Bool(b)) => Expr::Lit(Value::Bool(!b)),
        Expr::Not(x) => (**x).clone(),
        Expr::Bin(a, op @ (Op::And | Op::Or), b) => {
            let other = if *op == Op::And { Op::Or } else { Op::And };
            simplify(&bin(negate(a), other, negate(b)))
        }
        Expr::Bin(a, op, b) => match neg_op(*op) {
            Some(n) => Expr::Bin(a.clone(), n, b.clone()),
            None => Expr::Not(Box::new(e.clone())),
        },
        Expr::IsNull(x, n) => Expr::IsNull(x.clone(), !n),
        Expr::In(x, l, n) => Expr::In(x.clone(), l.clone(), !n),
        Expr::Like(a, p, ci, n) => Expr::Like(a.clone(), p.clone(), *ci, !n),
        Expr::Func(Func::Distinct(d), args) if bool_test(args) => Expr::Func(Func::Distinct(!d), args.clone()),
        e => Expr::Not(Box::new(e.clone())),
    }
}

/// Whether a [NOT] DISTINCT FROM's arguments are a BooleanTest's: IS [NOT] TRUE, FALSE or UNKNOWN (a boolean NULL).
fn bool_test(args: &[Expr]) -> bool {
    match args.get(1) {
        Some(Expr::Lit(Value::Bool(_))) => true,
        Some(Expr::Cast(n, Type::Bool)) => matches!(**n, Expr::Lit(Value::Null)),
        _ => false,
    }
}

/// `e` with its columns' qualifiers dropped: what makes two mentions of a column or an expression the same term.
fn unqualified(e: &Expr) -> Expr {
    rewrite(e, &|x| match x {
        Expr::Col(Some(_), n) => Some(Expr::Col(None, n.clone())),
        _ => None,
    })
}

/// A term's identity: a column's name, or the expression's form.
fn term_key(e: &Expr) -> String {
    match e {
        Expr::Col(_, n) => n.clone(),
        e => format!("{:?}", unqualified(e)),
    }
}

/// The two sides of an equality that makes (or joins) an equivalence class: a term equal to a constant or to
/// another term.
fn ec_pair(e: &Expr) -> Option<(Expr, Expr)> {
    let Expr::Bin(a, Op::Eq, b) = e else { return None };
    let null = |x: &Expr| matches!(x, Expr::Lit(Value::Null));
    if null(a) || null(b) || (!has_col(a) && !has_col(b)) {
        return None;
    }
    // (a side that is neither a term nor a plain constant, or a parameter, which is a pseudoconstant: no class)
    let ok = |x: &Expr| has_col(x) || matches!(const_of(x), K::Val(..)) || matches!(x, Expr::Param(_));
    (ok(a) && ok(b)).then(|| ((**a).clone(), (**b).clone()))
}

/// Whether `e` reads a column.
fn has_col(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| found |= matches!(x, Expr::Col(..)));
    found
}

/// A WHERE as the scan of its one relation filters by it (see `Quals`): its conditions, each equality of a term
/// with a constant or another term taken into an equivalence class, which gives them back after the other conditions
/// (the one clause that made it, or a `term = constant` of each of its terms); then all cheapest first. None where
/// two constants of a class can't be compared here.
fn scan_quals(filter: &Expr, cols: &[Col]) -> Option<Quals> {
    struct Ec {
        /// each member's key, the member, and whether it is a constant
        members: Vec<(String, Expr, bool)>,
        sources: Vec<Expr>,
    }
    let mut conds = vec![];
    chain(&simplify(filter), Op::And, &mut conds);
    let mut plain = vec![];
    let mut ecs: Vec<Ec> = vec![];
    for c in conds {
        match &c {
            Expr::Lit(Value::Bool(true)) => continue,
            Expr::Lit(Value::Bool(false) | Value::Null) => return Some(Quals::False),
            _ => {}
        }
        let Some((a, b)) = ec_pair(&c) else {
            plain.push(c);
            continue;
        };
        // (a class is of one collation, the equality's: ec_collation, equivclass.c:300)
        let explicit = |e: &Expr| {
            let mut c = None;
            walk(e, &mut |x| {
                if let Expr::Func(Func::Collate(k), _) = x {
                    c = Some(*k);
                }
            });
            c
        };
        let coll = match (explicit(&a).or_else(|| explicit(&b)), coll_of(&a, cols), coll_of(&b, cols)) {
            (Some(c), ..) => format!("{c:?}"),
            (None, Ok(x), Ok(y)) => {
                collation::merge(x, y).map_or_else(|_| String::new(), |s| format!("{:?}", s.coll()))
            }
            _ => String::new(),
        };
        let (ka, kb) = (format!("{}\u{1}{coll}", term_key(&a)), format!("{}\u{1}{coll}", term_key(&b)));
        if ka == kb {
            // (x = x is x IS NOT NULL)
            plain.push(Expr::IsNull(Box::new(a), true));
            continue;
        }
        let holding = |k: &str| ecs.iter().position(|e| e.members.iter().any(|m| m.0 == k));
        let i = match (holding(&ka), holding(&kb)) {
            (Some(i), Some(j)) if i != j => {
                let (keep, gone) = (i.min(j), i.max(j));
                let other = ecs.remove(gone);
                ecs[keep].members.extend(other.members);
                ecs[keep].sources.extend(other.sources);
                keep
            }
            (Some(i), _) | (_, Some(i)) => i,
            (None, None) => {
                ecs.push(Ec { members: vec![], sources: vec![] });
                ecs.len() - 1
            }
        };
        for (k, x) in [(ka, a), (kb, b)] {
            if !ecs[i].members.iter().any(|m| m.0 == k) {
                let konst = !has_col(&x);
                ecs[i].members.push((k, x, konst));
            }
        }
        ecs[i].sources.push(c);
    }
    let mut out = plain;
    let mut fixed = vec![];
    for ec in ecs {
        let consts: Vec<&Expr> = ec.members.iter().filter(|m| m.2).map(|m| &m.1).collect();
        // two constants in a class: it holds only where they are equal
        for c in consts.iter().skip(1) {
            let (K::Val(x, _), K::Val(y, _)) = (const_of(consts[0]), const_of(c)) else { return None };
            match compare(&x, &y) {
                Ok(Ordering::Equal) => {}
                Ok(_) => return Some(Quals::False),
                Err(_) => return None,
            }
        }
        if !consts.is_empty() {
            fixed.extend(
                ec.members.iter().filter(|m| !m.2).map(|m| m.0.split('\u{1}').next().unwrap_or("").to_string()),
            );
        }
        if ec.members.len() == 2 && ec.sources.len() == 1 {
            out.extend(ec.sources);
            continue;
        }
        match consts.first() {
            Some(c) => {
                for m in ec.members.iter().filter(|m| !m.2) {
                    out.push(bin(m.1.clone(), Op::Eq, (*c).clone()));
                }
            }
            None => {
                for w in ec.members.windows(2) {
                    out.push(bin(w[0].1.clone(), Op::Eq, w[1].1.clone()));
                }
            }
        }
    }
    // (a stable sort: conditions of one cost keep their order)
    out.sort_by(|a, b| cost(a, cols).partial_cmp(&cost(b, cols)).unwrap_or(Ordering::Equal));
    Some(Quals::List(out, fixed))
}

/// What a condition costs to check per row, as cost_qual_eval counts it (in cpu_operator_cost): each operator and
/// function call one, an `= ANY (array)` half one per element, tests of null and of truth nothing, and a cast to text
/// of a varchar or text column of `cols` nothing (a RelabelType, or no node at all).
fn cost(e: &Expr, cols: &[Col]) -> f64 {
    let c = |x: &Expr| cost(x, cols);
    match e {
        Expr::Lit(_) | Expr::Col(..) | Expr::Param(_) => 0.0,
        Expr::Neg(x) if matches!(**x, Expr::Lit(_)) => 0.0,
        Expr::Cast(x, _) if matches!(const_of(x), K::Val(..)) => 0.0,
        Expr::Cast(x, Type::Text) if relabels_to_text(x, cols) => 0.0,
        Expr::Bin(a, Op::And | Op::Or, b) => c(a) + c(b),
        Expr::Bin(a, _, b) | Expr::Like(a, b, ..) => 1.0 + c(a) + c(b),
        Expr::Not(x) | Expr::IsNull(x, _) => c(x),
        Expr::Neg(x) | Expr::Cast(x, _) => 1.0 + c(x),
        Expr::In(x, list, _) => {
            let consts = list.iter().filter(|e| !has_col(e)).count();
            let others: f64 = list.iter().filter(|e| has_col(e)).map(|e| 1.0 + c(e)).sum();
            let array = match consts {
                0 => 0.0,
                1 => 1.0,
                n if n >= 9 => 2.0,
                n => 0.5 * n as f64,
            };
            c(x) * (1 + list.iter().filter(|e| has_col(e)).count()) as f64 + array + others
        }
        Expr::Func(Func::Distinct(_), args) if bool_test(args) => c(&args[0]),
        Expr::Func(Func::Collate(_) | Func::Coalesce | Func::RowValue, args) => args.iter().map(c).sum(),
        Expr::Func(_, args) => 1.0 + args.iter().map(c).sum::<f64>(),
        Expr::Case(o, ws, el) => {
            o.iter().chain(el.iter()).map(|x| c(x)).sum::<f64>() + ws.iter().map(|(w, t)| c(w) + c(t)).sum::<f64>()
        }
        Expr::Call(call) => 100.0 + call.args.iter().map(c).sum::<f64>(),
        _ => 0.0,
    }
}

/// The collation of `e` over the columns `cols`, each column its own (collation::of reads a bound expression's).
fn coll_of(e: &Expr, cols: &[Col]) -> Result<collation::Strength> {
    let bound = rewrite(e, &|x| match x {
        Expr::Col(_, n) => cols.iter().position(|c| c.name == *n && !c.hidden).map(Expr::Idx),
        _ => None,
    });
    collation::of(&bound, cols, &[], &|_| None)
}

/// Whether `x` is a column of `cols` that a cast to text only relabels (a RelabelType): a varchar's or a text's
/// (char(n)'s is a function, text(bpchar); another type's a CoerceViaIO, which match_clause_to_partition_key,
/// partprune.c:1880, and clause_is_strict_for, predtest.c:1381, don't look through).
fn relabels_to_text(x: &Expr, cols: &[Col]) -> bool {
    match x {
        Expr::Col(_, n) => {
            cols.iter().any(|c| c.name == *n && !c.hidden && c.ty == Type::Text && !matches!(c.chars, Some((_, true))))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------- constants as the planner folds them

/// Whether `e` is a constant as the query writes one (`1`, `-1`, `'a'`, `'1'::bigint`, `'a' COLLATE "C"`), which the
/// deparser writes as the planner folds it.
fn written_const(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) => true,
        Expr::Neg(x) => matches!(**x, Expr::Lit(Value::Int(_) | Value::Numeric(_) | Value::Float(_))),
        Expr::Cast(x, _) => written_const(x),
        Expr::Func(Func::Collate(_), args) => args.len() == 1 && written_const(&args[0]),
        _ => false,
    }
}

/// The types whose immutable operators and functions `folds` works out: numbers but floating ones (whose text this
/// doesn't write as Postgres does), text and booleans.
fn scalar_ty(t: Type) -> bool {
    matches!(t, Type::SmallInt | Type::Int | Type::BigInt | Type::Numeric | Type::Text | Type::Bool)
}

/// Whether the planner works `e` out to a constant this writes (see `Planner::fold`): it is `foldable` (no column,
/// parameter or subquery in it, no function that isn't immutable), and made only of the operators and functions of
/// `scalar_ty` types (a time's or a time zone's operators are stable, as is text concatenated with another type's:
/// anytextcat), CASE, COALESCE and the tests of null and truth.
fn folds(e: &Expr, cols: &[Col]) -> bool {
    if !foldable(e) {
        return false;
    }
    let typed = |x: &Expr| ty_in(x, cols, &[], &[]).is_ok_and(scalar_ty);
    let mut ok = true;
    walk(e, &mut |x| {
        ok &= match x {
            Expr::Lit(_) | Expr::Not(_) | Expr::IsNull(..) | Expr::Case(..) => true,
            Expr::Neg(a) => typed(a),
            Expr::Cast(a, t) => scalar_ty(*t) && (matches!(**a, Expr::Lit(_)) || typed(a)),
            Expr::Bin(a, op, b) => match op {
                Op::And | Op::Or => true,
                Op::Concat => [a, b].iter().all(|y| ty_in(y, cols, &[], &[]).is_ok_and(|t| t == Type::Text)),
                Op::Eq
                | Op::Ne
                | Op::Lt
                | Op::Le
                | Op::Gt
                | Op::Ge
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Mod => typed(a) && typed(b),
                _ => false,
            },
            Expr::In(a, ..) => typed(a),
            Expr::Func(Func::Collate(_), _) => true,
            Expr::Func(
                Func::Coalesce
                | Func::Abs
                | Func::Round
                | Func::Lower
                | Func::Upper
                | Func::Length
                | Func::CharLength
                | Func::Scalar(_),
                args,
            ) => args.iter().all(typed) && typed(x),
            _ => false,
        }
    });
    ok
}

/// A value of type `ty` as the constant the planner leaves (a Const, which ruleutils.c's get_const_expr writes: `2`,
/// `'-2'::integer`, `'2'::bigint`, `'xy'::text`, `NULL::integer`); None for a type this doesn't write.
fn const_expr(v: Value, ty: Type) -> Option<Expr> {
    let cast = |x: Expr| Expr::Cast(Box::new(x), ty);
    Some(match (v, ty) {
        (Value::Null, ty) if scalar_ty(ty) => cast(Expr::Lit(Value::Null)),
        (v @ Value::Bool(_), Type::Bool) => Expr::Lit(v),
        (Value::Int(n), Type::Int) if i32::try_from(n).is_ok() => Expr::Lit(Value::Int(n)),
        (v @ Value::Int(_), Type::BigInt | Type::SmallInt) => cast(Expr::Lit(v)),
        (v @ Value::Numeric(_), Type::Numeric) => Expr::Lit(v),
        (v @ Value::Text(_), Type::Text) => cast(Expr::Lit(v)),
        _ => return None,
    })
}

/// Whether `e` is a null constant (of a type or not).
fn is_null(e: &Expr) -> bool {
    match e {
        Expr::Lit(Value::Null) => true,
        Expr::Cast(x, _) => is_null(x),
        _ => false,
    }
}

/// Whether `op`, as an operator of two values, is strict and gives a boolean: a comparison (each a strict function).
fn strict_test(op: Op) -> bool {
    matches!(op, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Regex { .. } | Op::Like { .. })
}

/// Whether an AND or an OR in `e` has a null constant arm.
fn null_arm(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| found |= matches!(x, Expr::Bin(a, Op::And | Op::Or, b) if is_null(a) || is_null(b)));
    found
}

/// Whether a strict operator in `e` has a null constant operand, or a NOT a null argument.
fn has_null_operand(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| {
        found |= match x {
            Expr::Bin(a, op, b) if !matches!(op, Op::And | Op::Or) => is_null(a) || is_null(b),
            Expr::Like(a, b, ..) => is_null(a) || is_null(b),
            Expr::Not(a) | Expr::Neg(a) => is_null(a),
            _ => false,
        }
    });
    found
}

/// `e` (its constant parts worked out) with a comparison of a null constant a null boolean, as simplify_function
/// leaves a strict function with a null input (clauses.c:4454), and NOT of a null null. None where a null is left
/// where eval_const_expressions keeps more than a null (an AND, an OR, a CASE, a function or a test of one), or an
/// operator other than a comparison has one (its result's type isn't worked out here).
fn nulls(e: &Expr) -> Option<Expr> {
    let null_bool = || Expr::Cast(Box::new(Expr::Lit(Value::Null)), Type::Bool);
    Some(match e {
        e if is_null(e) || written_const(e) => e.clone(),
        Expr::Bin(a, op, b) if strict_test(*op) => {
            let (a, b) = (nulls(a)?, nulls(b)?);
            match is_null(&a) || is_null(&b) {
                true => null_bool(),
                false => Expr::Bin(Box::new(a), *op, Box::new(b)),
            }
        }
        Expr::Like(a, b, ci, neg) => {
            let (a, b) = (nulls(a)?, nulls(b)?);
            match is_null(&a) || is_null(&b) {
                true => null_bool(),
                false => Expr::Like(Box::new(a), Box::new(b), *ci, *neg),
            }
        }
        Expr::Not(x) => {
            let x = nulls(x)?;
            match is_null(&x) {
                true => null_bool(),
                false => Expr::Not(Box::new(x)),
            }
        }
        // (an AND's or an OR's arms, each with its nulls: one that is null is kept, see `Planner::fold`)
        Expr::Bin(a, op @ (Op::And | Op::Or), b) => Expr::Bin(Box::new(nulls(a)?), *op, Box::new(nulls(b)?)),
        // COALESCE: its null constants dropped, and what comes after a constant that isn't null (clauses.c:3271)
        Expr::Func(Func::Coalesce, args) => {
            let mut kept = vec![];
            for a in args {
                let a = nulls(a)?;
                if is_null(&a) {
                    continue;
                }
                let konst = written_const(&a);
                if konst && kept.is_empty() {
                    return Some(a);
                }
                kept.push(a);
                if konst {
                    break;
                }
            }
            // (all null: a null of its type)
            if kept.is_empty() {
                return None;
            }
            Expr::Func(Func::Coalesce, kept)
        }
        // CASE: a WHEN that is false or null dropped, one that is true the result (after others, the ELSE), and with
        // no WHEN left the ELSE (clauses.c:3100)
        Expr::Case(None, whens, els) => {
            let (mut kept, mut default, mut ended) = (vec![], None, false);
            for (w, t) in whens {
                let w = nulls(w)?;
                match w {
                    Expr::Lit(Value::Bool(false)) => {}
                    ref x if is_null(x) => {}
                    Expr::Lit(Value::Bool(true)) => {
                        default = Some(nulls(t)?);
                        ended = true;
                        break;
                    }
                    w => kept.push((w, nulls(t)?)),
                }
            }
            if !ended && let Some(x) = els {
                default = Some(nulls(x)?);
            }
            match (kept.is_empty(), default) {
                (true, Some(d)) => d,
                // (a null of the CASE's type)
                (true, None) => return None,
                (false, d) => Expr::Case(None, kept, d.map(Box::new)),
            }
        }
        e if has_null_operand(e) => return None,
        e => e.clone(),
    })
}

// ---------------------------------------------------------------- partition pruning (partitioning/partprune.c)

/// A side of a comparison without columns: a constant (and the type it was cast to), what is only known when the
/// plan runs (a parameter, a stable function's value), or neither.
enum K {
    Val(Value, Option<Type>),
    Later,
    No,
}

fn const_of(e: &Expr) -> K {
    match e {
        Expr::Lit(v) => K::Val(v.clone(), None),
        Expr::Neg(x) => match &**x {
            Expr::Lit(Value::Int(n)) => K::Val(Value::Int(-n), None),
            Expr::Lit(v @ (Value::Numeric(_) | Value::Float(_))) => {
                match coerce(Value::Text(format!("-{}", v.to_text())), Type::Numeric) {
                    Ok(n) => K::Val(n, None),
                    Err(_) => K::No,
                }
            }
            x if has_col(x) => K::No,
            _ => K::Later,
        },
        Expr::Cast(x, t) => match const_of(x) {
            K::Val(v, _) => match coerce(v, *t) {
                Ok(c) => K::Val(c, Some(*t)),
                Err(_) => K::No,
            },
            k => k,
        },
        // (a constant's COLLATE is folded into it: eval_const_expressions, clauses.c:3076)
        Expr::Func(Func::Collate(_), args) if args.len() == 1 => const_of(&args[0]),
        e if has_col(e) => K::No,
        _ => K::Later,
    }
}

/// A partitioned table's key as pruning reads it (PartitionKey): its strategy, and for each part its expression (a
/// column is one too), the term that names it (`term_key`), its type, and its collation (None: of a type that has
/// none).
struct Key {
    strategy: Strategy,
    exprs: Vec<Expr>,
    terms: Vec<String>,
    types: Vec<Type>,
    colls: Vec<Option<collation::Coll>>,
}

impl Key {
    fn of(t: &Table, pk: &PartKey) -> Result<Key> {
        let types = pk.types(t)?;
        let (mut exprs, mut terms, mut colls) = (vec![], vec![], vec![]);
        for (i, (p, ty)) in pk.parts.iter().zip(&types).enumerate() {
            let (e, coll) = match p {
                KeyPart::Col(id) => {
                    let c = t.cols.iter().find(|c| c.id == *id);
                    (Expr::Col(None, c.map_or_else(String::new, |c| c.name.clone())), c.map(|c| c.coll))
                }
                KeyPart::Expr(text) => match check_expr(text) {
                    // (the key's COLLATE: its collation)
                    Ok(Expr::Func(Func::Collate(c), args)) if args.len() == 1 => (args[0].clone(), Some(c)),
                    Ok(e) => {
                        let coll = coll_of(&e, &t.cols).ok().map(|s| s.coll());
                        (e, coll)
                    }
                    Err(_) => (Expr::Lit(Value::Null), None),
                },
            };
            // (a COLLATE written with the part: partcollation)
            let coll = pk.coll(i).and_then(|n| collation::builtin_named(None, n)).or(coll);
            terms.push(term_key(&e));
            colls.push(collation::collatable(*ty).then(|| coll.unwrap_or(collation::Coll::DEFAULT)));
            exprs.push(e);
        }
        Ok(Key { strategy: pk.strategy, exprs, terms, types, colls })
    }
}

/// A constant compared with a key of type `key`, as the key's operator family compares it: an untyped literal read
/// as the key's type; a value of another type of the family (an integer of any width with an integer key, a number
/// with a numeric or floating one) as itself, its cross-type operators comparing them as they are; None for one the
/// family has no operator for (an integer key with a numeric).
fn key_value(v: &Value, decl: Option<Type>, key: Type) -> Option<Value> {
    if *v == Value::Null {
        return Some(Value::Null);
    }
    let from = match (decl, v) {
        (Some(t), _) => t,
        (None, Value::Text(_)) => return coerce(v.clone(), key).ok(),
        (None, v) => v.ty()?,
    };
    let kind = |t: Type| match t {
        t if t.integral() => 1,
        Type::Numeric => 2,
        Type::Float | Type::Float4 => 3,
        Type::Text => 4,
        Type::Date | Type::Timestamp | Type::TimestampTz => 5,
        _ => 0,
    };
    let (f, k) = (kind(from), kind(key));
    // (integer_ops: int2, int4 and int8 compare across widths, so a constant out of the key's range is as it is)
    if f == 1 && k == 1 {
        return Some(v.clone());
    }
    let ok = from == key || (f != 0 && f == k) || (k == 2 && f == 1) || (k == 3 && matches!(f, 1 | 2));
    if ok { coerce(v.clone(), key).ok() } else { None }
}

/// A B-tree strategy (BTLessStrategyNumber ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Strat {
    Lt = 1,
    Le = 2,
    Eq = 3,
    Ge = 4,
    Gt = 5,
}

impl Strat {
    fn of(op: Op) -> Option<Strat> {
        Some(match op {
            Op::Lt => Strat::Lt,
            Op::Le => Strat::Le,
            Op::Eq => Strat::Eq,
            Op::Ge => Strat::Ge,
            Op::Gt => Strat::Gt,
            _ => return None,
        })
    }
}

/// `op` with its operands swapped (get_commutator).
fn commute(op: Op) -> Op {
    match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        op => op,
    }
}

/// partbounds.c's PartitionBoundInfo of a partitioned table's partitions, each partition by its place in
/// `kids_in_order` (Postgres's canonical order: by bound, null-only, default last).
struct BoundInfo {
    strategy: Strategy,
    /// list: the values, in order; range: each distinct bound's edges, in order
    datums: Vec<Vec<Edge>>,
    /// list: the partition of each value; range: the partition each bound is the upper bound of (None for a lower
    /// bound: below it, back to the bound before, no partition holds a row), one more than the bounds; hash: the
    /// partition of each remainder of the greatest modulus
    indexes: Vec<Option<usize>>,
    null_index: Option<usize>,
    default_index: Option<usize>,
}

/// partition_rbound_cmp (partbounds.c:3489): two range bounds, column by column (MINVALUE below every value and
/// MAXVALUE above, and nothing after either counted), an upper bound below a lower one of the same values.
fn rbound_cmp(a: &[Edge], a_lower: bool, b: &[Edge], b_lower: bool) -> Ordering {
    let rank = |e: &Edge| match e {
        Edge::Min => 0,
        Edge::Val(_) => 1,
        Edge::Max => 2,
    };
    let mut o = Ordering::Equal;
    for (x, y) in a.iter().zip(b) {
        match rank(x).cmp(&rank(y)) {
            Ordering::Equal => {}
            other => return other,
        }
        let (Edge::Val(p), Edge::Val(q)) = (x, y) else { break };
        o = compare(p, q).unwrap_or(Ordering::Equal);
        if o != Ordering::Equal {
            return o;
        }
    }
    if o == Ordering::Equal && a_lower != b_lower {
        return if a_lower { Ordering::Greater } else { Ordering::Less };
    }
    o
}

/// partition_rbound_datum_cmp (partbounds.c:3557): a bound against the first `values.len()` key values.
fn rbound_datum_cmp(bound: &[Edge], values: &[Value]) -> Ordering {
    let mut o = Ordering::Less;
    for (e, v) in bound.iter().zip(values) {
        match e {
            Edge::Min => return Ordering::Less,
            Edge::Max => return Ordering::Greater,
            Edge::Val(b) => {
                o = compare(b, v).unwrap_or(Ordering::Equal);
                if o != Ordering::Equal {
                    break;
                }
            }
        }
    }
    o
}

impl BoundInfo {
    /// partition_bounds_create (partbounds.c:300) of `bounds`, the partitions' (typed) in canonical order.
    fn new(strategy: Strategy, bounds: &[PartBound]) -> BoundInfo {
        let mut bi = BoundInfo { strategy, datums: vec![], indexes: vec![], null_index: None, default_index: None };
        match strategy {
            // create_list_bounds (partbounds.c:463)
            Strategy::List => {
                let mut all: Vec<(Value, usize)> = vec![];
                for (i, b) in bounds.iter().enumerate() {
                    match b {
                        PartBound::Default => bi.default_index = Some(i),
                        PartBound::List(vs) => {
                            for v in vs {
                                match v {
                                    Value::Null => bi.null_index = Some(i),
                                    v => all.push((v.clone(), i)),
                                }
                            }
                        }
                        _ => {}
                    }
                }
                all.sort_by(|a, b| compare(&a.0, &b.0).unwrap_or(Ordering::Equal));
                for (v, i) in all {
                    bi.datums.push(vec![Edge::Val(v)]);
                    bi.indexes.push(Some(i));
                }
            }
            // create_range_bounds (partbounds.c:678)
            Strategy::Range => {
                let mut all: Vec<(&[Edge], bool, usize)> = vec![];
                for (i, b) in bounds.iter().enumerate() {
                    match b {
                        PartBound::Default => bi.default_index = Some(i),
                        PartBound::Range(lo, hi) => {
                            all.push((lo, true, i));
                            all.push((hi, false, i));
                        }
                        _ => {}
                    }
                }
                all.sort_by(|a, b| rbound_cmp(a.0, a.1, b.0, b.1));
                let mut prev: Option<&[Edge]> = None;
                for (edges, lower, i) in all {
                    // (only distinct bounds: kinds or values differing before a MINVALUE or MAXVALUE)
                    let distinct = match prev {
                        None => true,
                        Some(p) => {
                            let mut d = false;
                            for (x, y) in edges.iter().zip(p) {
                                match (x, y) {
                                    (Edge::Val(a), Edge::Val(b)) => {
                                        if compare(a, b).ok() != Some(Ordering::Equal) {
                                            d = true;
                                            break;
                                        }
                                    }
                                    (Edge::Min, Edge::Min) | (Edge::Max, Edge::Max) => break,
                                    _ => {
                                        d = true;
                                        break;
                                    }
                                }
                            }
                            d
                        }
                    };
                    if distinct {
                        bi.datums.push(edges.to_vec());
                        bi.indexes.push(if lower { None } else { Some(i) });
                    }
                    prev = Some(edges);
                }
                bi.indexes.push(None);
            }
            // create_hash_bounds (partbounds.c:348)
            Strategy::Hash => {
                let hb: Vec<(u32, u32, usize)> = bounds
                    .iter()
                    .enumerate()
                    .filter_map(|(i, b)| match b {
                        PartBound::Hash(m, r) => Some((*m, *r, i)),
                        _ => None,
                    })
                    .collect();
                let greatest = hb.iter().map(|h| h.0).max().unwrap_or(0) as usize;
                bi.indexes = vec![None; greatest];
                for (m, r, i) in hb {
                    let mut rem = r as usize;
                    while rem < greatest && m > 0 {
                        bi.indexes[rem] = Some(i);
                        rem += m as usize;
                    }
                }
            }
        }
        bi
    }

    /// partition_list_bsearch (partbounds.c:3608): the greatest value at or below `v` (None: all above), and whether
    /// it is equal.
    fn list_bsearch(&self, v: &Value) -> (Option<usize>, bool) {
        let (mut lo, mut hi): (isize, isize) = (-1, self.datums.len() as isize - 1);
        let mut eq = false;
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let Edge::Val(d) = &self.datums[mid as usize][0] else { return (None, false) };
            let c = compare(d, v).unwrap_or(Ordering::Equal);
            if c != Ordering::Greater {
                lo = mid;
                eq = c == Ordering::Equal;
                if eq {
                    break;
                }
            } else {
                hi = mid - 1;
            }
        }
        (usize::try_from(lo).ok(), eq)
    }

    /// partition_range_datum_bsearch (partbounds.c:3696): the greatest bound at or below the values (None: all
    /// above), and whether it is equal.
    fn range_bsearch(&self, values: &[Value]) -> (isize, bool) {
        let (mut lo, mut hi): (isize, isize) = (-1, self.datums.len() as isize - 1);
        let mut eq = false;
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let c = rbound_datum_cmp(&self.datums[mid as usize], values);
            if c != Ordering::Greater {
                lo = mid;
                eq = c == Ordering::Equal;
                if eq {
                    break;
                }
            } else {
                hi = mid - 1;
            }
        }
        (lo, eq)
    }
}

/// A clause matched to a partition key (PartClauseInfo): its key column, strategy, whether it is `<>` (a list's),
/// and the value compared with.
#[derive(Clone, Debug)]
struct PartClause {
    keyno: usize,
    strat: Strat,
    is_ne: bool,
    value: Value,
}

/// A pruning step (PartitionPruneStep): an operator step (the strategy, None for `<>` or for nulls only; the values
/// of a prefix of the key columns, those in `nullkeys` left out), or one combining earlier steps' results.
enum Step {
    Op { strat: Option<Strat>, values: Vec<Value>, nullkeys: Vec<usize> },
    Combine { union: bool, sources: Vec<usize> },
}

/// What a step keeps (PruneStepResult): offsets into the bounds' indexes, and whether the null-accepting and the
/// default partitions are kept.
#[derive(Clone, Debug, Default)]
struct StepResult {
    offsets: std::collections::BTreeSet<usize>,
    scan_null: bool,
    scan_default: bool,
}

/// match_clause_to_partition_key's verdicts (PartClauseMatchStatus).
enum Match {
    Unmatched,
    Clause(PartClause),
    Nullness(bool),
    Steps(Vec<usize>),
    Contradict,
    Unsupported,
}

/// gen_partprune_steps's context (GeneratePruningStepsContext), for one partitioned table: its key and bounds, its
/// partition constraint where it has a default partition, the steps made, and whether the clauses contradict each
/// other. `mutable`: a key was compared with a value only known when the plan runs, which Postgres prunes by as the
/// plan starts (initial pruning), and this doesn't.
struct Prune<'k> {
    pk: &'k PartKey,
    hash: PartHasher<'k>,
    key: &'k Key,
    bi: &'k BoundInfo,
    pq: &'k [Expr],
    cols: &'k [Col],
    steps: Vec<Step>,
    contradictory: bool,
    mutable: bool,
    generic: bool,
}

/// The expression a clause's side is once a COLLATE and a cast to text of a varchar or text column of `cols` are taken
/// off it: what match_clause_to_partition_key (partprune.c:1880) and clause_is_strict_for (predtest.c:1381) see
/// through a RelabelType (see `relabels_to_text`), with the collation given.
fn bare<'e>(e: &'e Expr, cols: &[Col]) -> &'e Expr {
    match e {
        Expr::Func(Func::Collate(_), args) if args.len() == 1 => bare(&args[0], cols),
        Expr::Cast(x, Type::Text) if relabels_to_text(x, cols) => x,
        e => e,
    }
}

impl Prune<'_> {
    fn op_step(&mut self, strat: Option<Strat>, values: Vec<Value>, nullkeys: Vec<usize>) -> usize {
        self.steps.push(Step::Op { strat, values, nullkeys });
        self.steps.len() - 1
    }

    fn combine(&mut self, union: bool, sources: Vec<usize>) -> usize {
        self.steps.push(Step::Combine { union, sources });
        self.steps.len() - 1
    }

    /// The collation a comparison of `a` and `b` takes (its inputcollid): an explicit COLLATE, else a column's, else
    /// the default; None where they aren't text.
    fn input_coll(&self, a: &Expr, b: &Expr) -> Option<collation::Coll> {
        let sa = coll_of(a, self.cols).ok()?;
        let sb = coll_of(b, self.cols).ok()?;
        match collation::merge(sa, sb).ok()? {
            collation::Strength::None => None,
            s => Some(s.coll()),
        }
    }

    /// PartCollMatchesExprColl (partprune.c:1747).
    fn coll_matches(&self, keyno: usize, a: &Expr, b: &Expr) -> bool {
        match self.key.colls[keyno] {
            None => true,
            Some(c) => self.input_coll(a, b).unwrap_or(collation::Coll::DEFAULT) == c,
        }
    }

    /// gen_partprune_steps_internal (partprune.c:962): the steps `clauses` (AND-ed) make; the last, where there are
    /// several, the INTERSECT of the others.
    fn internal(&mut self, clauses: &[Expr]) -> Vec<usize> {
        // (the clauses refuting the table's own partition constraint: none of its partitions is read)
        if self.bi.default_index.is_some() && !self.pq.is_empty() && refuted_by(self.pq, clauses, self.cols) {
            self.contradictory = true;
            return vec![];
        }
        let n = self.key.terms.len();
        let mut keyclauses: Vec<Vec<PartClause>> = vec![vec![]; n];
        let (mut nullkeys, mut notnullkeys) = (vec![false; n], vec![false; n]);
        let mut generate_opsteps = false;
        let mut result = vec![];
        for clause in clauses {
            if matches!(clause, Expr::Lit(Value::Bool(false) | Value::Null)) {
                self.contradictory = true;
                return vec![];
            }
            if let Expr::Bin(_, Op::Or, _) = clause {
                let mut args = vec![];
                chain(clause, Op::Or, &mut args);
                let (mut ids, mut all_contradictory) = (vec![], true);
                for arg in &args {
                    let steps = self.internal(std::slice::from_ref(arg));
                    let contradictory = std::mem::take(&mut self.contradictory);
                    // (an arm that contradicts itself is no way in)
                    if contradictory {
                        continue;
                    }
                    all_contradictory = false;
                    match steps.last() {
                        Some(&last) => ids.push(last),
                        // (an arm no step comes of: no pruning by the OR, a combine step of nothing)
                        None => ids.push(self.combine(true, vec![])),
                    }
                }
                if all_contradictory {
                    self.contradictory = true;
                    return vec![];
                }
                if !ids.is_empty() {
                    result.push(self.combine(true, ids));
                }
                continue;
            }
            if let Expr::Bin(_, Op::And, _) = clause {
                let mut args = vec![];
                chain(clause, Op::And, &mut args);
                let steps = self.internal(&args);
                if self.contradictory {
                    return vec![];
                }
                result.extend(steps.last().copied());
                continue;
            }
            for i in 0..n {
                match self.match_clause(clause, i) {
                    Match::Clause(pc) => {
                        // (operators are strict: an IS NULL of the key contradicts it)
                        if nullkeys[i] {
                            self.contradictory = true;
                            return vec![];
                        }
                        generate_opsteps = true;
                        keyclauses[i].push(pc);
                    }
                    Match::Nullness(not_null) => {
                        if !not_null {
                            if notnullkeys[i] || !keyclauses[i].is_empty() {
                                self.contradictory = true;
                                return vec![];
                            }
                            nullkeys[i] = true;
                        } else {
                            if nullkeys[i] {
                                self.contradictory = true;
                                return vec![];
                            }
                            notnullkeys[i] = true;
                        }
                    }
                    Match::Steps(steps) => result.extend(steps),
                    Match::Contradict => {
                        self.contradictory = true;
                        return vec![];
                    }
                    Match::Unmatched => continue,
                    Match::Unsupported => {}
                }
                break;
            }
        }
        let nnull = nullkeys.iter().filter(|x| **x).count();
        let nulls: Vec<usize> = (0..n).filter(|i| nullkeys[*i]).collect();
        if nnull > 0 && (self.key.strategy != Strategy::Hash || nnull == n) {
            // (strategy 1: a null key is only in the partition taking nulls, or the default)
            result.push(self.op_step(None, vec![], nulls));
        } else if generate_opsteps {
            let steps = self.steps_from_opexps(&keyclauses, &nullkeys);
            result.extend(steps);
        } else if notnullkeys.iter().all(|x| *x) && n > 0 {
            // (strategy 3: every key not null leaves out only the partition taking nulls)
            result.push(self.op_step(None, vec![], vec![]));
        }
        if result.len() > 1 {
            let all = result.clone();
            result.push(self.combine(false, all));
        }
        result
    }

    /// match_boolean_partition_clause (partprune.c:3628): of a boolean key, a clause that is it (or its NOT) or a test
    /// of it IS [NOT] TRUE or FALSE: the value it fixes the key to, and whether it is an IS NOT test (which a null
    /// key also passes). Some(None) where the clause is of another form, but a boolean one (NOMATCH); None where
    /// it isn't (UNSUPPORTED).
    #[allow(clippy::option_option)]
    fn match_bool(&self, clause: &Expr, i: usize) -> Option<Option<(bool, bool)>> {
        if self.key.types[i] != Type::Bool {
            return None;
        }
        let key = &self.key.terms[i];
        if let Expr::Func(Func::Distinct(same), args) = clause {
            let [x, Expr::Lit(Value::Bool(b))] = &args[..] else { return None };
            if term_key(bare(x, self.cols)) != *key {
                return Some(None);
            }
            return Some(Some((*b, !same)));
        }
        let (not, arg) = match clause {
            Expr::Not(x) => (true, &**x),
            x => (false, x),
        };
        if term_key(bare(arg, self.cols)) == *key {
            return Some(Some((!not, false)));
        }
        if term_key(&negate(bare(arg, self.cols))) == *key {
            return Some(Some((not, false)));
        }
        Some(None)
    }

    /// match_clause_to_partition_key (partprune.c:1791).
    fn match_clause(&mut self, clause: &Expr, i: usize) -> Match {
        let boolmatch = self.match_bool(clause, i);
        if let Some(Some((val, noteq))) = boolmatch {
            if noteq {
                // (IS NOT TRUE: IS FALSE or null, as steps of their own)
                let key = self.key.exprs[i].clone();
                // (Func::Distinct(true): IS NOT DISTINCT FROM, so IS)
                let test = Expr::Func(Func::Distinct(true), vec![key.clone(), Expr::Lit(Value::Bool(!val))]);
                let or = bin(test, Op::Or, Expr::IsNull(Box::new(key), false));
                let steps = self.internal(&[or]);
                if self.contradictory {
                    return Match::Contradict;
                }
                return if steps.is_empty() { Match::Unsupported } else { Match::Steps(steps) };
            }
            return Match::Clause(PartClause { keyno: i, strat: Strat::Eq, is_ne: false, value: Value::Bool(val) });
        }
        let key = &self.key.terms[i];
        match clause {
            Expr::Bin(a, op @ (Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge), b) => {
                let (expr, op) = if term_key(bare(a, self.cols)) == *key && has_col(bare(a, self.cols)) {
                    (&**b, *op)
                } else if term_key(bare(b, self.cols)) == *key && has_col(bare(b, self.cols)) {
                    (&**a, commute(*op))
                } else {
                    return Match::Unmatched;
                };
                if !self.coll_matches(i, a, b) {
                    return Match::Unmatched;
                }
                // the operator in the key's operator family: a hash family has only =, `<>` is a list's by its
                // negator
                let (strat, is_ne) = match (op, self.key.strategy) {
                    (Op::Ne, Strategy::List) => (Strat::Eq, true),
                    (Op::Ne, _) => return Match::Unmatched,
                    (Op::Eq, _) => (Strat::Eq, false),
                    (_, Strategy::Hash) => return Match::Unmatched,
                    (op, _) => (Strat::of(op).expect("a comparison"), false),
                };
                self.value_clause(i, strat, is_ne, expr)
            }
            Expr::In(x, list, neg) => {
                let elems: Vec<Expr> = list.iter().cloned().collect();
                // (x IN (...) is x = ANY (ARRAY[...]); NOT IN, <> ALL)
                let (op, use_or) = if *neg { (Op::Ne, false) } else { (Op::Eq, true) };
                self.match_saop(clause, x, op, use_or, Some(elems), i)
            }
            Expr::Func(Func::Quant(op, all), args) if !args.is_empty() => {
                let elems = args[1..].to_vec();
                self.match_saop(clause, &args[0], *op, !all, Some(elems), i)
            }
            Expr::Func(Func::QuantArray(op, all), args) if args.len() == 2 => {
                let elems = match const_of(&args[1]) {
                    K::Val(Value::Null, _) => None,
                    K::Val(v, _) => match array_values(v) {
                        Ok(Some(vs)) => Some(vs.into_iter().map(Expr::Lit).collect()),
                        _ => return Match::Unsupported,
                    },
                    _ => return Match::Unsupported,
                };
                self.match_saop(clause, &args[0], *op, !all, elems, i)
            }
            Expr::IsNull(x, neg) => match term_key(bare(x, self.cols)) == *key {
                true => Match::Nullness(*neg),
                false => Match::Unmatched,
            },
            _ => match boolmatch {
                Some(_) => Match::Unmatched,
                None => Match::Unsupported,
            },
        }
    }

    /// The clause of key `i`, strategy `strat`, compared with `expr` (partprune.c:1950-2080): a constant of the key's
    /// operator family is a match; anything else no pruning when the plan is made.
    fn value_clause(&mut self, i: usize, strat: Strat, is_ne: bool, expr: &Expr) -> Match {
        match const_of(expr) {
            K::Val(v, decl) => match key_value(&v, decl, self.key.types[i]) {
                Some(value) => Match::Clause(PartClause { keyno: i, strat, is_ne, value }),
                // (no operator of the key's family compares the two: no match)
                None => Match::Unmatched,
            },
            K::Later => {
                // (a parameter of a generic plan prunes nothing; a stable value prunes as the plan starts)
                if !(self.generic && matches!(bare(expr, self.cols), Expr::Param(_))) {
                    self.mutable = true;
                }
                Match::Unsupported
            }
            K::No => Match::Unsupported,
        }
    }

    /// A ScalarArrayOpExpr matched to key `i` (partprune.c:2148-2348): `x op ANY` (`use_or`) or `ALL` over `elems`
    /// (None: a null array), each element a clause of its own, OR-ed or AND-ed.
    fn match_saop(
        &mut self,
        clause: &Expr,
        x: &Expr,
        op: Op,
        use_or: bool,
        elems: Option<Vec<Expr>>,
        i: usize,
    ) -> Match {
        let key = &self.key.terms[i];
        if term_key(bare(x, self.cols)) != *key || !has_col(bare(x, self.cols)) {
            return Match::Unmatched;
        }
        let first = elems.as_ref().and_then(|e| e.first()).cloned().unwrap_or(Expr::Lit(Value::Null));
        if !self.coll_matches(i, x, &first) {
            return Match::Unmatched;
        }
        // the operator in the family: `<>` only a list's (by its negator)
        match (op, self.key.strategy) {
            (Op::Ne, Strategy::List) => {}
            (Op::Ne, _) => return Match::Unmatched,
            (Op::Eq, _) => {}
            (_, Strategy::Hash) => return Match::Unmatched,
            (op, _) if Strat::of(op).is_some() => {}
            _ => return Match::Unmatched,
        }
        let Some(elems) = elems else { return Match::Contradict };
        // (an array of constants is one constant; one of other values an ArrayExpr, which a varchar key reads through
        // an ArrayCoerceExpr no step comes of)
        let all_const = elems.iter().all(|e| matches!(const_of(e), K::Val(..)));
        if !all_const && self.key.types[i] == Type::Text && self.cols_varchar(x) {
            return Match::Unsupported;
        }
        let mut elem_clauses = vec![];
        for e in elems {
            if matches!(const_of(&e), K::Val(Value::Null, _)) {
                // (a null element: no match with = ANY, every row false with ALL)
                if use_or {
                    continue;
                }
                return Match::Contradict;
            }
            elem_clauses.push(Expr::Bin(Box::new(bare(x, self.cols).clone()), op, Box::new(e)));
        }
        let _ = clause;
        let clauses = if use_or && elem_clauses.len() > 1 {
            let mut it = elem_clauses.into_iter();
            let first = it.next().expect("two");
            vec![it.fold(first, |acc, c| bin(acc, Op::Or, c))]
        } else {
            elem_clauses
        };
        let steps = self.internal(&clauses);
        if self.contradictory {
            return Match::Contradict;
        }
        if steps.is_empty() { Match::Unsupported } else { Match::Steps(steps) }
    }

    /// Whether `x` is a varchar column (relabeled to text where an operator compares it).
    fn cols_varchar(&self, x: &Expr) -> bool {
        match bare(x, self.cols) {
            Expr::Col(_, n) => self.cols.iter().any(|c| c.name == *n && matches!(c.chars, Some((_, false)))),
            _ => false,
        }
    }

    /// gen_prune_steps_from_opexps (partprune.c:1384): steps from the clauses matched to each key column.
    fn steps_from_opexps(&mut self, keyclauses: &[Vec<PartClause>], nullkeys: &[bool]) -> Vec<usize> {
        let n = keyclauses.len();
        let mut btree: Vec<Vec<PartClause>> = vec![vec![]; 6];
        let mut hash_eq: Vec<PartClause> = vec![];
        for (i, list) in keyclauses.iter().enumerate() {
            let mut next_key = true;
            if self.key.strategy == Strategy::Range && list.is_empty() {
                break;
            }
            if self.key.strategy == Strategy::Hash && list.is_empty() && !nullkeys[i] {
                return vec![];
            }
            for pc in list {
                match self.key.strategy {
                    Strategy::List | Strategy::Range => {
                        btree[pc.strat as usize].push(pc.clone());
                        if matches!(pc.strat, Strat::Lt | Strat::Gt) {
                            next_key = false;
                        }
                    }
                    Strategy::Hash => hash_eq.push(pc.clone()),
                }
            }
            if !next_key {
                break;
            }
        }
        let _ = n;
        let mut opsteps = vec![];
        match self.key.strategy {
            Strategy::List | Strategy::Range => {
                for strat in 1..=5 {
                    for pc in btree[strat].clone() {
                        if pc.keyno == 0 {
                            opsteps.extend(self.using_prefix(Some(pc.strat), pc.is_ne, pc.value.clone(), vec![], &[]));
                            continue;
                        }
                        // the clauses of the key columns before: =, and <= (for < and <=) or >= (for > and >=)
                        let mut prefix: Vec<PartClause> = vec![];
                        let mut valid = true;
                        for keyno in 0..pc.keyno {
                            let mut has = false;
                            for e in btree[Strat::Eq as usize].iter().filter(|c| c.keyno == keyno) {
                                prefix.push(e.clone());
                                has = true;
                            }
                            let side = match strat {
                                1 | 2 => Some(Strat::Le),
                                4 | 5 => Some(Strat::Ge),
                                _ => None,
                            };
                            if let Some(s) = side {
                                for e in btree[s as usize].iter().filter(|c| c.keyno == keyno) {
                                    prefix.push(e.clone());
                                    has = true;
                                }
                            }
                            if !has {
                                valid = false;
                                break;
                            }
                        }
                        if !valid {
                            break;
                        }
                        prefix.sort_by_key(|c| c.keyno);
                        opsteps.extend(self.using_prefix(Some(pc.strat), pc.is_ne, pc.value.clone(), vec![], &prefix));
                    }
                }
            }
            Strategy::Hash => {
                if let Some(last) = hash_eq.last() {
                    let last_keyno = last.keyno;
                    let start = hash_eq.iter().position(|c| c.keyno == last_keyno).unwrap_or(0);
                    let prefix: Vec<PartClause> = hash_eq[..start].to_vec();
                    let nulls: Vec<usize> = (0..nullkeys.len()).filter(|i| nullkeys[*i]).collect();
                    for pc in hash_eq[start..].iter().cloned() {
                        opsteps.extend(self.using_prefix(Some(Strat::Eq), false, pc.value, nulls.clone(), &prefix));
                    }
                }
            }
        }
        opsteps
    }

    /// get_steps_using_prefix (partprune.c:2427): a step for each choice of one clause per key column of `prefix`,
    /// the last value after them.
    fn using_prefix(
        &mut self,
        strat: Option<Strat>,
        is_ne: bool,
        last: Value,
        nullkeys: Vec<usize>,
        prefix: &[PartClause],
    ) -> Vec<usize> {
        let strat = if is_ne { None } else { strat };
        let mut keys: Vec<usize> = prefix.iter().map(|c| c.keyno).collect();
        keys.dedup();
        let mut combos: Vec<Vec<Value>> = vec![vec![]];
        for k in keys {
            let mut next = vec![];
            for c in combos {
                for pc in prefix.iter().filter(|p| p.keyno == k) {
                    let mut v = c.clone();
                    v.push(pc.value.clone());
                    next.push(v);
                }
            }
            combos = next;
        }
        let mut out = vec![];
        for mut values in combos {
            values.push(last.clone());
            out.push(self.op_step(strat, values, nullkeys.clone()));
        }
        out
    }

    // ------------------------------------------------------------ running the steps

    /// get_matching_partitions (partprune.c:818): the partitions (their places) the steps keep; all of them where
    /// there are none.
    fn matching(&self, nparts: usize, key_types: &[Type]) -> Vec<usize> {
        if self.steps.is_empty() {
            return (0..nparts).collect();
        }
        let mut results: Vec<StepResult> = vec![];
        for s in &self.steps {
            let r = match s {
                Step::Op { strat, values, nullkeys } => self.base_step(*strat, values, nullkeys, key_types),
                Step::Combine { union, sources } => self.combine_step(*union, sources, &results),
            };
            results.push(r);
        }
        let last = results.pop().unwrap_or_default();
        let mut scan_default = last.scan_default;
        let mut out = std::collections::BTreeSet::new();
        for i in &last.offsets {
            match self.bi.indexes.get(*i).copied().flatten() {
                Some(p) => {
                    out.insert(p);
                }
                // (a range no partition holds: the default's, if there is one)
                None => scan_default |= self.bi.default_index.is_some(),
            }
        }
        if last.scan_null
            && let Some(p) = self.bi.null_index
        {
            out.insert(p);
        }
        if scan_default && let Some(p) = self.bi.default_index {
            out.insert(p);
        }
        out.into_iter().collect()
    }

    /// perform_pruning_base_step (partprune.c:3405).
    fn base_step(&self, strat: Option<Strat>, values: &[Value], nullkeys: &[usize], key_types: &[Type]) -> StepResult {
        // (a null compared with by a strict operator: nothing matches)
        if values.contains(&Value::Null) {
            return StepResult::default();
        }
        match self.bi.strategy {
            Strategy::Hash => self.hash_bounds(values, nullkeys, key_types),
            Strategy::List => self.list_bounds(strat, values.first(), nullkeys),
            Strategy::Range => self.range_bounds(strat, values, nullkeys),
        }
    }

    /// get_matching_hash_bounds (partprune.c:2652).
    fn hash_bounds(&self, values: &[Value], nullkeys: &[usize], key_types: &[Type]) -> StepResult {
        let mut r = StepResult::default();
        let n = key_types.len();
        let nindexes = self.bi.indexes.len();
        if values.len() + nullkeys.len() == n && nindexes > 0 {
            let mut it = values.iter();
            let row: Vec<Value> = (0..n)
                .map(|k| if nullkeys.contains(&k) { Value::Null } else { it.next().cloned().unwrap_or(Value::Null) })
                .collect();
            match hash_key_with(&row, key_types, &|i, x| (self.hash)(self.pk, key_types, i, x)) {
                Ok(h) => {
                    let off = (h % nindexes as u64) as usize;
                    if self.bi.indexes[off].is_some() {
                        r.offsets.insert(off);
                    }
                }
                Err(_) => r.offsets.extend(0..nindexes),
            }
        } else {
            r.offsets.extend(0..nindexes);
        }
        r
    }

    /// get_matching_list_bounds (partprune.c:2729).
    fn list_bounds(&self, strat: Option<Strat>, value: Option<&Value>, nullkeys: &[usize]) -> StepResult {
        let mut r = StepResult::default();
        let has_default = self.bi.default_index.is_some();
        if !nullkeys.is_empty() {
            if self.bi.null_index.is_some() {
                r.scan_null = true;
            } else {
                r.scan_default = has_default;
            }
            return r;
        }
        let ndatums = self.bi.datums.len();
        if ndatums == 0 {
            r.scan_default = has_default;
            return r;
        }
        let Some(value) = value else {
            r.offsets.extend(0..ndatums);
            r.scan_default = has_default;
            return r;
        };
        let Some(strat) = strat else {
            // (<>: every value but the one)
            r.offsets.extend(0..ndatums);
            if let (Some(off), true) = self.bi.list_bsearch(value) {
                r.offsets.remove(&off);
            }
            r.scan_default = has_default;
            return r;
        };
        if strat != Strat::Eq {
            r.scan_default = has_default;
        }
        let (mut minoff, mut maxoff) = (0usize, ndatums - 1);
        match strat {
            Strat::Eq => {
                match self.bi.list_bsearch(value) {
                    (Some(off), true) => {
                        r.offsets.insert(off);
                    }
                    _ => r.scan_default = has_default,
                }
                return r;
            }
            Strat::Ge | Strat::Gt => {
                let inclusive = strat == Strat::Ge;
                let off = match self.bi.list_bsearch(value) {
                    (Some(off), eq) => {
                        if !eq || !inclusive {
                            off + 1
                        } else {
                            off
                        }
                    }
                    (None, _) => 0,
                };
                if off > ndatums - 1 {
                    return r;
                }
                minoff = off;
            }
            Strat::Le | Strat::Lt => {
                let inclusive = strat == Strat::Le;
                let off: isize = match self.bi.list_bsearch(value) {
                    (Some(off), eq) if eq && !inclusive => off as isize - 1,
                    (Some(off), _) => off as isize,
                    (None, _) => -1,
                };
                if off < 0 {
                    return r;
                }
                maxoff = off as usize;
            }
        }
        r.offsets.extend(minoff..=maxoff);
        r
    }

    /// get_matching_range_bounds (partprune.c:2940).
    fn range_bounds(&self, strat: Option<Strat>, values: &[Value], nullkeys: &[usize]) -> StepResult {
        let mut r = StepResult::default();
        let bi = self.bi;
        let has_default = bi.default_index.is_some();
        let ndatums = bi.datums.len();
        let partnatts = self.key.terms.len();
        if ndatums == 0 || !nullkeys.is_empty() {
            r.scan_default = has_default;
            return r;
        }
        let (mut minoff, mut maxoff) = (0usize, ndatums);
        let nvalues = values.len();
        if nvalues == 0 {
            // (every non-null key: every bound, the ranges no partition covers too, as 16.15 has it
            // (partprune.c:2978): a step this is intersected with may keep those, which are the default's)
            r.scan_default = has_default;
            r.offsets.extend(minoff..=maxoff);
            return r;
        }
        if nvalues < partnatts {
            r.scan_default = has_default;
        }
        let cmp_at = |i: usize| rbound_datum_cmp(&bi.datums[i], values);
        let kind_at = |i: usize, k: usize| bi.datums[i].get(k).cloned();
        let Some(strat) = strat else { return r };
        match strat {
            Strat::Eq => {
                let (off, eq) = bi.range_bsearch(values);
                if off >= 0 && eq {
                    let off = off as usize;
                    if nvalues == partnatts {
                        r.offsets.insert(off + 1);
                        return r;
                    }
                    // (a prefix of the key: the other bounds equal to it too)
                    let mut lo = off;
                    while lo >= 1 && cmp_at(lo - 1) == Ordering::Equal {
                        lo -= 1;
                    }
                    if matches!(kind_at(lo, nvalues), Some(Edge::Min)) {
                        lo += 1;
                    }
                    minoff = lo;
                    let mut hi = off;
                    while hi < ndatums - 1 && cmp_at(hi + 1) == Ordering::Equal {
                        hi += 1;
                    }
                    maxoff = hi + 1;
                    r.offsets.extend(minoff..=maxoff);
                } else {
                    r.offsets.insert((off + 1) as usize);
                }
                return r;
            }
            Strat::Ge | Strat::Gt => {
                let inclusive = strat == Strat::Ge;
                let (off, eq) = bi.range_bsearch(values);
                if off < 0 {
                    minoff = 0;
                } else if eq && nvalues < partnatts {
                    let mut off = off as usize;
                    while off >= 1 && off < ndatums - 1 {
                        let next = if inclusive { off - 1 } else { off + 1 };
                        if cmp_at(next) != Ordering::Equal {
                            break;
                        }
                        off = next;
                    }
                    minoff = if inclusive { off } else { off + 1 };
                } else {
                    minoff = off as usize + 1;
                }
            }
            Strat::Le | Strat::Lt => {
                let inclusive = strat == Strat::Le;
                let (off, eq) = bi.range_bsearch(values);
                if off >= 0 {
                    let mut o = off as usize;
                    if eq && nvalues < partnatts {
                        while o >= 1 && o < ndatums - 1 {
                            let next = if inclusive { o + 1 } else { o - 1 };
                            if cmp_at(next) != Ordering::Equal {
                                break;
                            }
                            o = next;
                        }
                        maxoff = if inclusive { o + 1 } else { o };
                    } else if !eq || inclusive {
                        maxoff = o + 1;
                    } else {
                        maxoff = o;
                    }
                } else {
                    maxoff = 0;
                }
            }
        }
        // (a smallest partition from MINVALUE, a greatest to MAXVALUE: their finite bounds, so the default isn't read
        // by mistake)
        let lastkey = nvalues - 1;
        if minoff < ndatums && bi.indexes[minoff].is_none() && matches!(kind_at(minoff, lastkey), Some(Edge::Min)) {
            minoff += 1;
        }
        if maxoff >= 1 && bi.indexes[maxoff].is_none() && matches!(kind_at(maxoff - 1, lastkey), Some(Edge::Max)) {
            maxoff -= 1;
        }
        if minoff <= maxoff {
            r.offsets.extend(minoff..=maxoff);
        }
        r
    }

    /// perform_pruning_combine_step (partprune.c:3553).
    fn combine_step(&self, union: bool, sources: &[usize], results: &[StepResult]) -> StepResult {
        let mut r = StepResult::default();
        if sources.is_empty() {
            r.offsets.extend(0..self.bi.indexes.len());
            r.scan_default = self.bi.default_index.is_some();
            r.scan_null = self.bi.null_index.is_some();
            return r;
        }
        for (k, s) in sources.iter().enumerate() {
            let Some(x) = results.get(*s) else { continue };
            if union {
                r.offsets.extend(x.offsets.iter().copied());
                r.scan_null |= x.scan_null;
                r.scan_default |= x.scan_default;
            } else if k == 0 {
                r = x.clone();
            } else {
                r.offsets = r.offsets.intersection(&x.offsets).copied().collect();
                r.scan_null &= x.scan_null;
                r.scan_default &= x.scan_default;
            }
        }
        r
    }
}

// ---------------------------------------------------------------- partition constraints (partbounds.c)

/// The partition constraint of a partition of bound `b`, `bounds` its parent's partitions' (typed), over the parent's
/// key: get_qual_for_list (partbounds.c:3963), get_qual_for_range (partbounds.c:4110), as AND-ed clauses. A hash
/// partition's (satisfies_hash_partition) says nothing pruning reads: none.
fn partition_qual(key: &Key, b: &PartBound, bounds: &[PartBound]) -> Vec<Expr> {
    match b {
        PartBound::List(_) => list_qual(key, b, bounds),
        PartBound::Range(..) => range_qual(key, b, false),
        PartBound::Default if key.strategy == Strategy::List => list_qual(key, b, bounds),
        PartBound::Default => {
            // everything no other partition holds (get_qual_for_range, for a default)
            let mut arms = vec![];
            for o in bounds.iter().filter(|o| matches!(o, PartBound::Range(..))) {
                let q = range_qual(key, o, true);
                if let Some(a) = and_all(&q) {
                    arms.push(a);
                }
            }
            if arms.is_empty() {
                return vec![];
            }
            let mut parts: Vec<Expr> = key.exprs.iter().map(|e| Expr::IsNull(Box::new(e.clone()), true)).collect();
            let mut it = arms.into_iter();
            let first = it.next().expect("an arm");
            parts.push(it.fold(first, |acc, a| bin(acc, Op::Or, a)));
            vec![Expr::Not(Box::new(and_all(&parts).expect("parts")))]
        }
        PartBound::Hash(..) => vec![],
    }
}

/// A key part compared with a value: the clause make_partition_op_expr makes.
fn key_cmp(key: &Key, j: usize, op: Op, v: &Value) -> Expr {
    Expr::Bin(Box::new(key.exprs[j].clone()), op, Box::new(Expr::Lit(v.clone())))
}

/// get_qual_for_list (partbounds.c:3963).
fn list_qual(key: &Key, b: &PartBound, bounds: &[PartBound]) -> Vec<Expr> {
    let col = key.exprs[0].clone();
    let (elems, has_null): (Vec<Value>, bool) = match b {
        PartBound::List(vs) => (vs.iter().filter(|v| **v != Value::Null).cloned().collect(), vs.contains(&Value::Null)),
        _ => {
            let mut all = vec![];
            let mut null = false;
            for o in bounds {
                if let PartBound::List(vs) = o {
                    for v in vs {
                        match v {
                            Value::Null => null = true,
                            v => all.push(v.clone()),
                        }
                    }
                }
            }
            (all, null)
        }
    };
    let opexpr = match elems.len() {
        0 => None,
        1 => Some(key_cmp(key, 0, Op::Eq, &elems[0])),
        _ => Some(Expr::In(Box::new(col.clone()), elems.into_iter().map(Expr::Lit).collect::<Vec<_>>().into(), false)),
    };
    let result = if !has_null {
        let nn = Expr::IsNull(Box::new(col), true);
        match opexpr {
            Some(o) => vec![nn, o],
            None => vec![nn],
        }
    } else {
        let isnull = Expr::IsNull(Box::new(col), false);
        match opexpr {
            Some(o) => vec![bin(isnull, Op::Or, o)],
            None => vec![isnull],
        }
    };
    if matches!(b, PartBound::Default) {
        return vec![Expr::Not(Box::new(and_all(&result).expect("a clause")))];
    }
    result
}

/// get_qual_for_range (partbounds.c:4110) of a non-default range partition: its key columns not null (unless
/// `for_default`), each leading column whose ends are equal `= value`, then an OR of arms for the lower end and one for
/// the upper.
fn range_qual(key: &Key, b: &PartBound, for_default: bool) -> Vec<Expr> {
    let PartBound::Range(lo, hi) = b else { return vec![] };
    let n = key.exprs.len();
    let mut result: Vec<Expr> = vec![];
    if !for_default {
        result.extend(key.exprs.iter().map(|e| Expr::IsNull(Box::new(e.clone()), true)));
    }
    let val = |e: Option<&Edge>| match e {
        Some(Edge::Val(v)) => Some(v.clone()),
        _ => None,
    };
    let mut i = 0;
    while i < n {
        let (Some(l), Some(u)) = (val(lo.get(i)), val(hi.get(i))) else { break };
        if compare(&l, &u).ok() != Some(Ordering::Equal) || i == n - 1 {
            break;
        }
        result.push(key_cmp(key, i, Op::Eq, &l));
        i += 1;
    }
    let num_or_arms = n - i;
    let (mut lower_arms, mut upper_arms) = (vec![], vec![]);
    let (mut need_lower, mut need_upper) = (true, true);
    let mut current = 0;
    while current < num_or_arms {
        let (mut lower_args, mut upper_args) = (vec![], vec![]);
        let mut j = i;
        while j < n {
            let (l, u) = (val(lo.get(j)), val(hi.get(j)));
            let (l_next, u_next) = (lo.get(j + 1), hi.get(j + 1));
            if need_lower && let Some(l) = &l {
                let op = if j - i < current {
                    Op::Eq
                } else if j == n - 1 || matches!(l_next, Some(Edge::Min)) {
                    Op::Ge
                } else {
                    Op::Gt
                };
                lower_args.push(key_cmp(key, j, op, l));
            }
            if need_upper && let Some(u) = &u {
                let op = if j - i < current {
                    Op::Eq
                } else if matches!(u_next, Some(Edge::Max)) {
                    Op::Le
                } else {
                    Op::Lt
                };
                upper_args.push(key_cmp(key, j, op, u));
            }
            j += 1;
            if j - i > current {
                if l.is_none() || !matches!(l_next, Some(Edge::Val(_))) {
                    need_lower = false;
                }
                if u.is_none() || !matches!(u_next, Some(Edge::Val(_))) {
                    need_upper = false;
                }
                break;
            }
        }
        if let Some(a) = and_all(&lower_args) {
            lower_arms.push(a);
        }
        if let Some(a) = and_all(&upper_args) {
            upper_arms.push(a);
        }
        if !need_lower && !need_upper {
            break;
        }
        current += 1;
    }
    for arms in [lower_arms, upper_arms] {
        let mut it = arms.into_iter();
        if let Some(first) = it.next() {
            result.push(it.fold(first, |acc, a| bin(acc, Op::Or, a)));
        }
    }
    if result.is_empty() {
        if for_default {
            return key.exprs.iter().map(|e| Expr::IsNull(Box::new(e.clone()), true)).collect();
        }
        return vec![Expr::Lit(Value::Bool(true))];
    }
    result
}

// ---------------------------------------------------------------- proofs (optimizer/util/predtest.c)

/// predicate_refuted_by (predtest.c:222), strong: whether `clauses` (AND-ed) being true makes `predicate` (AND-ed)
/// false.
fn refuted_by(predicate: &[Expr], clauses: &[Expr], cols: &[Col]) -> bool {
    if predicate.is_empty() || clauses.is_empty() {
        return false;
    }
    let p = and_all(predicate).expect("a predicate");
    let c = and_all(clauses).expect("a clause");
    refutes(&c, &p, false, cols)
}

/// contain_mutable_functions (clauses.c:368): whether `e` calls what isn't immutable (a function made with CREATE
/// FUNCTION, a built-in that is stable or volatile) or reads a parameter, which no proof may rest on.
fn mutable(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| {
        found |= match x {
            Expr::Call(_) | Expr::Param(_) | Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) => true,
            Expr::Func(Func::Collate(_) | Func::Distinct(_) | Func::Quant(..) | Func::QuantArray(..), _) => false,
            Expr::Func(f, _) => catalog_schema::builtin_mutable(&func_name(f)),
            _ => false,
        }
    });
    found
}

/// predicate_classify (predtest.c:826): an AND's or an OR's items (an IN list's constants as `x = c` each, ANY's
/// OR-ed and ALL's AND-ed), or an atom.
enum Class {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Atom,
}

fn classify(e: &Expr) -> Class {
    match e {
        Expr::Bin(_, Op::And, _) => {
            let mut v = vec![];
            chain(e, Op::And, &mut v);
            Class::And(v)
        }
        Expr::Bin(_, Op::Or, _) => {
            let mut v = vec![];
            chain(e, Op::Or, &mut v);
            Class::Or(v)
        }
        Expr::In(x, list, neg) if list.len() <= 100 => {
            let op = if *neg { Op::Ne } else { Op::Eq };
            let items = list.iter().map(|c| Expr::Bin(x.clone(), op, Box::new(c.clone()))).collect();
            if *neg { Class::And(items) } else { Class::Or(items) }
        }
        _ => Class::Atom,
    }
}

/// extract_not_arg (predtest.c:1281): a NOT's argument (or of IS NOT TRUE, IS FALSE).
fn not_arg(e: &Expr) -> Option<Expr> {
    match e {
        Expr::Not(x) => Some((**x).clone()),
        // (IS FALSE, IS NOT TRUE, and IS UNKNOWN)
        Expr::Func(Func::Distinct(same), args) => match &args[..] {
            [x, Expr::Lit(Value::Bool(b))] if (*same && !*b) || (!*same && *b) => Some(x.clone()),
            [x, Expr::Cast(n, Type::Bool)] if *same && matches!(**n, Expr::Lit(Value::Null)) => Some(x.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// extract_strong_not_arg (predtest.c:1305): a NOT's argument (or IS FALSE's).
fn strong_not_arg(e: &Expr) -> Option<Expr> {
    match e {
        Expr::Not(x) => Some((**x).clone()),
        Expr::Func(Func::Distinct(true), args) => match &args[..] {
            [x, Expr::Lit(Value::Bool(false))] => Some(x.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// predicate_implied_by_recurse (predtest.c:290).
fn implies(clause: &Expr, pred: &Expr, weak: bool, cols: &[Col]) -> bool {
    match (classify(clause), classify(pred)) {
        (Class::And(_), Class::And(ps)) => ps.iter().all(|p| implies(clause, p, weak, cols)),
        (Class::And(cs), Class::Or(ps)) => {
            ps.iter().any(|p| implies(clause, p, weak, cols)) || cs.iter().any(|c| implies(c, pred, weak, cols))
        }
        (Class::And(cs), Class::Atom) => cs.iter().any(|c| implies(c, pred, weak, cols)),
        (Class::Or(cs), Class::Or(ps)) => cs.iter().all(|c| ps.iter().any(|p| implies(c, p, weak, cols))),
        (Class::Or(cs), _) => cs.iter().all(|c| implies(c, pred, weak, cols)),
        (Class::Atom, Class::And(ps)) => ps.iter().all(|p| implies(clause, p, weak, cols)),
        (Class::Atom, Class::Or(ps)) => ps.iter().any(|p| implies(clause, p, weak, cols)),
        (Class::Atom, Class::Atom) => implied_simple(pred, clause, weak, cols),
    }
}

/// predicate_refuted_by_recurse (predtest.c:531).
fn refutes(clause: &Expr, pred: &Expr, weak: bool, cols: &[Col]) -> bool {
    match classify(clause) {
        Class::And(cs) => match classify(pred) {
            Class::And(ps) => {
                ps.iter().any(|p| refutes(clause, p, weak, cols)) || cs.iter().any(|c| refutes(c, pred, weak, cols))
            }
            Class::Or(ps) => ps.iter().all(|p| refutes(clause, p, weak, cols)),
            Class::Atom => {
                if let Some(a) = not_arg(pred)
                    && implies(clause, &a, false, cols)
                {
                    return true;
                }
                cs.iter().any(|c| refutes(c, pred, weak, cols))
            }
        },
        Class::Or(cs) => match classify(pred) {
            Class::Or(ps) => ps.iter().all(|p| refutes(clause, p, weak, cols)),
            Class::And(ps) => cs.iter().all(|c| ps.iter().any(|p| refutes(c, p, weak, cols))),
            Class::Atom => {
                if let Some(a) = not_arg(pred)
                    && implies(clause, &a, false, cols)
                {
                    return true;
                }
                cs.iter().all(|c| refutes(c, pred, weak, cols))
            }
        },
        Class::Atom => {
            if let Some(a) = strong_not_arg(clause)
                && implies(pred, &a, !weak, cols)
            {
                return true;
            }
            match classify(pred) {
                Class::And(ps) => ps.iter().any(|p| refutes(clause, p, weak, cols)),
                Class::Or(ps) => ps.iter().all(|p| refutes(clause, p, weak, cols)),
                Class::Atom => {
                    if let Some(a) = not_arg(pred)
                        && implies(clause, &a, false, cols)
                    {
                        return true;
                    }
                    refuted_simple(pred, clause, weak, cols)
                }
            }
        }
    }
}

/// clause_is_strict_for (predtest.c:1366): whether `clause` is null (or false, which here goes with it) wherever
/// `sub` is null: it is `sub`, or a strict operator or function of something that is.
fn strict_for(clause: &Expr, sub: &Expr, cols: &[Col]) -> bool {
    if term_key(bare(clause, cols)) == term_key(bare(sub, cols)) {
        return true;
    }
    match clause {
        Expr::Bin(a, op, b) if !matches!(op, Op::And | Op::Or) => strict_for(a, sub, cols) || strict_for(b, sub, cols),
        Expr::Func(Func::Collate(_), args) => args.iter().any(|a| strict_for(a, sub, cols)),
        Expr::Neg(x) | Expr::Cast(x, _) => strict_for(x, sub, cols),
        Expr::Like(a, b, ..) => strict_for(a, sub, cols) || strict_for(b, sub, cols),
        _ => false,
    }
}

/// predicate_implied_by_simple_clause (predtest.c:1124).
fn implied_simple(pred: &Expr, clause: &Expr, weak: bool, cols: &[Col]) -> bool {
    if term_key(pred) == term_key(clause) {
        return true;
    }
    if !weak && let Expr::IsNull(x, true) = pred {
        return strict_for(clause, x, cols);
    }
    operator_proof(pred, clause, false, weak, cols)
}

/// predicate_refuted_by_simple_clause (predtest.c:1223).
fn refuted_simple(pred: &Expr, clause: &Expr, weak: bool, cols: &[Col]) -> bool {
    if let Expr::IsNull(x, false) = pred {
        if strict_for(clause, x, cols) {
            return true;
        }
        if let Expr::IsNull(y, true) = clause
            && term_key(y) == term_key(x)
        {
            return true;
        }
        return false;
    }
    if let Expr::IsNull(x, false) = clause {
        if let Expr::IsNull(y, true) = pred
            && term_key(y) == term_key(x)
        {
            return true;
        }
        if weak && strict_for(pred, x, cols) {
            return true;
        }
        return false;
    }
    operator_proof(pred, clause, true, weak, cols)
}

/// operator_predicate_proof (predtest.c:1686) for two comparisons of one expression with constants, the btree
/// tables' rule (predtest.c:1579-1631): the test the strategies call for, made on the two constants.
fn operator_proof(pred: &Expr, clause: &Expr, refute: bool, weak: bool, cols: &[Col]) -> bool {
    // a comparison with the term on the left and a constant on the right (6: <>)
    let side = |e: &Expr| -> Option<(String, usize, K)> {
        let Expr::Bin(a, op, b) = e else { return None };
        let n = |op: Op| match op {
            Op::Lt => Some(0),
            Op::Le => Some(1),
            Op::Eq => Some(2),
            Op::Ge => Some(3),
            Op::Gt => Some(4),
            Op::Ne => Some(5),
            _ => None,
        };
        if has_col(a) && !has_col(b) {
            Some((term_key(bare(a, cols)), n(*op)?, const_of(b)))
        } else if has_col(b) && !has_col(a) {
            Some((term_key(bare(b, cols)), n(commute(*op))?, const_of(a)))
        } else {
            None
        }
    };
    let (Some((pt, pop, pk)), Some((ct, cop, ck))) = (side(pred), side(clause)) else { return false };
    if pt != ct {
        return false;
    }
    let (K::Val(pv, pd), K::Val(cv, cd)) = (pk, ck) else { return false };
    // (the two operators of one btree family: constants of one family, else the term compared is a cast of it, which
    // equal() doesn't take for the term itself)
    let family = |v: &Value, d: Option<Type>| match (d, v) {
        // (an untyped literal: the other side's)
        (None, Value::Text(_)) => 0,
        _ => match d.or_else(|| v.ty()) {
            Some(t) if t.integral() => 1,
            Some(Type::Numeric) => 2,
            Some(Type::Float | Type::Float4) => 3,
            Some(Type::Date | Type::Timestamp | Type::TimestampTz) => 4,
            Some(t) => 10 + t.oid(),
            None => 0,
        },
    };
    let (pf, cf) = (family(&pv, pd), family(&cv, cd));
    if pv != Value::Null && cv != Value::Null && pf != 0 && cf != 0 && pf != cf {
        return false;
    }
    // (a null constant: btree operators are strict)
    if cv == Value::Null {
        return !weak || refute || pv == Value::Null;
    }
    if pv == Value::Null {
        return weak;
    }
    const NONE: u8 = 0;
    const LT: u8 = 1;
    const LE: u8 = 2;
    const EQ: u8 = 3;
    const GE: u8 = 4;
    const GT: u8 = 5;
    const NE: u8 = 6;
    // BT_implic_table and BT_refute_table: [clause op][predicate op]
    const IMPLIC: [[u8; 6]; 6] = [
        [GE, GE, NONE, NONE, NONE, GE],
        [GT, GE, NONE, NONE, NONE, GT],
        [GT, GE, EQ, LE, LT, NE],
        [NONE, NONE, NONE, LE, LT, LT],
        [NONE, NONE, NONE, LE, LE, LE],
        [NONE, NONE, NONE, NONE, NONE, EQ],
    ];
    const REFUTE: [[u8; 6]; 6] = [
        [NONE, NONE, GE, GE, GE, NONE],
        [NONE, NONE, GT, GT, GE, NONE],
        [LE, LT, NE, GT, GE, EQ],
        [LE, LT, LT, NONE, NONE, NONE],
        [LE, LE, LE, NONE, NONE, NONE],
        [NONE, NONE, EQ, NONE, NONE, NONE],
    ];
    let test = if refute { REFUTE[cop][pop] } else { IMPLIC[cop][pop] };
    // (the test: pred_const test_op clause_const)
    let Ok(o) = compare(&pv, &cv) else { return false };
    match test {
        LT => o == Ordering::Less,
        LE => o != Ordering::Greater,
        EQ => o == Ordering::Equal,
        GE => o != Ordering::Less,
        GT => o == Ordering::Greater,
        NE => o != Ordering::Equal,
        _ => false,
    }
}
