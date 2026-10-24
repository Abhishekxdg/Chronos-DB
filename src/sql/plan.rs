//! EXPLAIN (COSTS OFF) as Postgres's planner makes the plan, when the session asks for it (chronos.explain = postgres,
//! exact mode's default: Chronos may run the statement another way), for the statements whose plan doesn't turn on
//! what things cost: a read of one table (a partitioned or inherited one an Append of the partitions or children it reads, pruned
//! as partprune.c prunes them by the WHERE), under a plain Aggregate, a Sort and a Limit; an UPDATE or DELETE of one.
//! Conditions are written as ruleutils.c writes them, in the order the planner keeps them (createplan.c's
//! order_qual_clauses: cheapest first, the equalities that make equivalence classes after the rest). Anything else
//! (joins, a condition or order an index could serve, grouping, subqueries, parameters) gives None, and EXPLAIN shows
//! Chronos's own steps.

use super::partition::*;
use super::viewdef::{PlanRel, col_ty, plan_expr};
use super::*;

/// A plan node as EXPLAIN's text format shows it: its line, its detail lines (`Filter: ...`), and its children.
struct PNode {
    title: String,
    props: Vec<String>,
    kids: Vec<PNode>,
}

impl PNode {
    fn leaf(title: impl Into<String>) -> PNode {
        PNode { title: title.into(), props: vec![], kids: vec![] }
    }

    fn above(title: &str, kid: PNode) -> PNode {
        PNode { title: title.into(), props: vec![], kids: vec![kid] }
    }

    /// The Result a relation that can't hold a row is read as.
    fn nothing() -> PNode {
        PNode { title: "Result".into(), props: vec!["One-Time Filter: false".into()], kids: vec![] }
    }
}

/// ExplainNode's layout: a child is `->  ` under its parent, six columns in; detail lines two past the node's name.
fn render(n: &PNode, d: usize, out: &mut Vec<String>) {
    let at = if d == 0 { String::new() } else { format!("{}->  ", " ".repeat(6 * d - 4)) };
    out.push(format!("{at}{}", n.title));
    let pad = " ".repeat(if d == 0 { 2 } else { 6 * d + 2 });
    for p in &n.props {
        out.push(format!("{pad}{p}"));
    }
    for k in &n.kids {
        render(k, d + 1, out);
    }
}

impl Exec<'_> {
    /// EXPLAIN (COSTS OFF)'s lines for `stmt`, as Postgres plans it; None for a statement this doesn't plan.
    pub(super) fn pg_plan(&self, stmt: &Stmt) -> Result<Option<Vec<String>>> {
        let v = self.view()?;
        let off = |name: &str| {
            session_setting(name).is_some_and(|s| matches!(s.to_lowercase().as_str(), "off" | "false" | "0" | "no"))
        };
        // (where parallel plans are made cheap or forced, a Gather may come over any scan)
        let parallel = ["parallel_setup_cost", "parallel_tuple_cost", "min_parallel_table_scan_size"]
            .iter()
            .any(|n| session_setting(n).is_some_and(|s| s.trim_start_matches('0').is_empty() || s.starts_with("0 ")))
            || !off("debug_parallel_query") && session_setting("debug_parallel_query").is_some();
        // (constraint_exclusion = on: any table's CHECK constraints may empty its scan)
        if parallel || session_setting("constraint_exclusion").is_some_and(|s| s == "on") {
            return Ok(None);
        }
        let p = Planner {
            v: &v,
            prune: !off("enable_partition_pruning"),
            partitionwise: !off("enable_partitionwise_aggregate")
                && session_setting("enable_partitionwise_aggregate").is_some(),
        };
        let node = match stmt {
            Stmt::Select(s) => p.select(s)?,
            Stmt::Update { table, only, sets, filter, from: None, .. } => {
                let exprs: Vec<&Expr> = sets.iter().map(|(_, e)| e).collect();
                p.modify("Update", table, *only, filter.as_ref(), &exprs)?
            }
            Stmt::Delete { table, only, filter, from: None, .. } => {
                p.modify("Delete", table, *only, filter.as_ref(), &[])?
            }
            _ => None,
        };
        Ok(node.map(|n| {
            let mut out = vec![];
            render(&n, 0, &mut out);
            out
        }))
    }
}

struct Planner<'a, 'v> {
    v: &'a View<'v>,
    /// enable_partition_pruning
    prune: bool,
    /// enable_partitionwise_aggregate
    partitionwise: bool,
}

/// What a relation's scan is under its WHERE: never true, or its conditions in the planner's order, with the terms
/// an equivalence class makes equal to a constant (an ORDER BY of one needs no sort).
enum Quals {
    False,
    List(Vec<Expr>, Vec<String>),
}

impl Planner<'_, '_> {
    /// A SELECT of one table (or of nothing).
    fn select(&self, s: &Select) -> Result<Option<PNode>> {
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
        if exprs.iter().any(|e| awkward(e)) {
            return Ok(None);
        }
        let agg = s.items.iter().any(|it| matches!(it, Item::Expr(e, _) if has_agg(e)));
        let Some(src) = &s.from else {
            // (a select list alone)
            let plain = s.filter.is_none() && s.order.is_empty() && s.limit.is_none() && s.offset.is_none() && !agg;
            return Ok(plain.then(|| PNode::leaf("Result")));
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
            || t.mem.is_some()
            || rls::flags(self.v, &t.name)?.0
        {
            return Ok(None);
        }
        let alias = match src.alias.is_empty() || src.alias == src.table {
            true => rel_part(&src.table).to_string(),
            false => src.alias.clone(),
        };
        // the columns the ORDER BY sorts by (an output column's name or place is what it is)
        let mut keys: Vec<(Expr, Dir)> = vec![];
        for (e, d) in &s.order {
            let k = match e {
                Expr::Lit(Value::Int(n)) => match usize::try_from(*n).ok().and_then(|n| s.items.get(n.checked_sub(1)?))
                {
                    Some(Item::Expr(x, _)) => x.clone(),
                    _ => return Ok(None),
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
            if awkward(&k) {
                return Ok(None);
            }
            keys.push((k, *d));
        }
        let quals = match &s.filter {
            None => Quals::List(vec![], vec![]),
            Some(f) => match scan_quals(f) {
                Some(q) => q,
                None => return Ok(None),
            },
        };
        // a condition, an order or an aggregate an index might serve: what the plan is then turns on costs
        let mut used: Vec<String> = vec![];
        for e in s.filter.iter().chain(keys.iter().map(|(e, _)| e)) {
            cols_of(e, &mut used);
        }
        if agg {
            for it in &s.items {
                if let Item::Expr(e, _) = it {
                    cols_of(e, &mut used);
                }
            }
        }
        let Some((mut node, _, tree)) = self.scan(&t, src.only, &alias, &quals, false, &used)? else {
            return Ok(None);
        };
        if agg {
            if !keys.is_empty() || (tree && self.partitionwise) {
                return Ok(None);
            }
            node = PNode::above("Aggregate", node);
        }
        // (a key an equivalence class makes a constant orders nothing)
        let consts = match &quals {
            Quals::List(_, c) => c.clone(),
            Quals::False => vec![],
        };
        keys.retain(|(k, _)| !consts.contains(&term_key(k)));
        if !keys.is_empty() {
            let rel =
                PlanRel { shown: alias.clone(), written: alias.clone(), cols: t.cols.iter().map(col_ty).collect() };
            let mut shown = vec![];
            for (k, d) in &keys {
                let Some(text) = plan_expr(k, std::slice::from_ref(&rel), tree, Type::Text) else { return Ok(None) };
                // (an expression is the column of the plan below that computes it)
                let mut text = if matches!(k, Expr::Col(..)) { text } else { format!("({text})") };
                if d.desc {
                    text.push_str(" DESC");
                }
                if d.nulls_first && !d.desc {
                    text.push_str(" NULLS FIRST");
                } else if !d.nulls_first && d.desc {
                    text.push_str(" NULLS LAST");
                }
                shown.push(text);
            }
            node = PNode {
                title: "Sort".into(),
                props: vec![format!("Sort Key: {}", shown.join(", "))],
                kids: vec![node],
            };
        }
        if s.limit.is_some() || s.offset.is_some() {
            node = PNode::above("Limit", node);
        }
        Ok(Some(node))
    }

    /// UPDATE or DELETE (`verb`) of `table` without FROM: the ModifyTable over the scan of the rows it changes, which
    /// for a partitioned or inherited table names each table it writes.
    fn modify(
        &self,
        verb: &str,
        table: &str,
        only: bool,
        filter: Option<&Expr>,
        sets: &[&Expr],
    ) -> Result<Option<PNode>> {
        if filter.is_some_and(awkward) || sets.iter().any(|e| awkward(e)) || view_def(self.v, table)?.is_some() {
            return Ok(None);
        }
        let Some(t) = load(self.v, table)? else { return Ok(None) };
        if !t.schema || t.matview.is_some() || t.hier.foreign.is_some() || rls::flags(self.v, &t.name)?.0 {
            return Ok(None);
        }
        let quals = match filter {
            None => Quals::List(vec![], vec![]),
            Some(f) => match scan_quals(f) {
                Some(q) => q,
                None => return Ok(None),
            },
        };
        let mut used = vec![];
        if let Some(f) = filter {
            cols_of(f, &mut used);
        }
        let alias = rel_part(&t.name).to_string();
        let Some((node, names, tree)) = self.scan(&t, only, &alias, &quals, true, &used)? else { return Ok(None) };
        let mut top = PNode::leaf(format!("{verb} on {}", ident(&alias)));
        if tree {
            top.props = names.iter().map(|n| format!("{verb} on {n}")).collect();
        }
        // (an UPDATE of an inheritance tree's tables projects their rows through a Result)
        let node = if verb == "Update" && tree && t.hier.part.is_none() && node.title == "Append" {
            PNode::above("Result", node)
        } else {
            node
        };
        top.kids.push(node);
        Ok(Some(top))
    }

    /// The scan of `t`, read as `alias` (ONLY: not its children) under `quals`: a Seq Scan, an Append of the scans of
    /// the partitions or children it reads, or a Result where it reads none. With the names each table read is shown
    /// by (`lp_ad lp_1`), and whether the range table holds more than `t` (an inheritance tree), which qualifies the
    /// columns of the nodes above. `modify`: the tree is an UPDATE's or DELETE's target, which numbers its tables even
    /// when one is left. None where an index on one of the columns `used` might serve.
    fn scan(
        &self,
        t: &Table,
        only: bool,
        alias: &str,
        quals: &Quals,
        modify: bool,
        used: &[String],
    ) -> Result<Option<(PNode, Vec<String>, bool)>> {
        let tree = t.hier.is_parent() && !only;
        if only && t.hier.part.is_some() {
            return Ok(None);
        }
        let (filter, _) = match quals {
            Quals::False => return Ok(Some((PNode::nothing(), vec![], tree))),
            Quals::List(q, c) => (q, c),
        };
        let tables: Vec<Table> = if !tree {
            vec![t.clone()]
        } else if t.hier.part.is_some() {
            let f = F::And(filter.iter().map(formula).collect());
            let mut out = vec![];
            let (anc, anc_terms) = self.bounds_above(t)?;
            if self.leaves(t, &f, &anc, &anc_terms, &mut out)?.is_none() {
                return Ok(None);
            }
            out
        } else {
            let mut out = vec![t.clone()];
            for d in hier::descendants(self.v, t)? {
                // (constraint exclusion may leave out a child its CHECK constraints refute)
                if d.hier.part.is_some() || d.hier.foreign.is_some() || (!filter.is_empty() && !d.checks.is_empty()) {
                    return Ok(None);
                }
                out.push(d);
            }
            out
        };
        if tables.iter().any(|x| indexed(x, used)) {
            return Ok(None);
        }
        // (constraint exclusion may leave out a partition its CHECK constraints refute)
        if tree && !filter.is_empty() && tables.iter().any(|x| x.name != t.name && !x.checks.is_empty()) {
            return Ok(None);
        }
        // (an inheritance parent whose children are gone is read as a plain table)
        let tree = tree && (t.hier.part.is_some() || tables.len() > 1);
        if tables.is_empty() {
            return Ok(Some((PNode::nothing(), vec![], tree)));
        }
        let numbered = tree && (tables.len() > 1 || modify);
        let cond = and_all(filter);
        let (mut scans, mut names) = (vec![], vec![]);
        for (i, x) in tables.iter().enumerate() {
            let shown = if numbered { format!("{alias}_{}", i + 1) } else { alias.to_string() };
            let rel = rel_part(&x.name);
            let named = if shown == rel { ident(rel) } else { format!("{} {}", ident(rel), ident(&shown)) };
            let mut n = PNode::leaf(format!("Seq Scan on {named}"));
            if let Some(c) = &cond {
                let r = PlanRel {
                    shown: shown.clone(),
                    written: alias.to_string(),
                    cols: x.cols.iter().map(col_ty).collect(),
                };
                let Some(text) = plan_expr(c, &[r], false, Type::Bool) else { return Ok(None) };
                n.props.push(format!("Filter: {text}"));
            }
            names.push(named);
            scans.push(n);
        }
        let node = match scans.len() {
            1 => scans.pop().expect("one scan"),
            _ => PNode { title: "Append".into(), props: vec![], kids: scans },
        };
        Ok(Some((node, names, tree)))
    }

    /// What the bound of `t` (a partition read on its own) and those of the partitions above it say of their parents'
    /// keys, with those keys' first terms and types: what its own partitions are pruned by too.
    #[allow(clippy::type_complexity)]
    fn bounds_above(&self, t: &Table) -> Result<(Vec<F>, Vec<(String, Type)>)> {
        let (mut fs, mut terms) = (vec![], vec![]);
        let mut cur = t.clone();
        for _ in 0..64 {
            let Some((parent, b)) = cur.hier.of.clone() else { break };
            let Some(p) = load(self.v, &parent)? else { break };
            let Some(pk) = &p.hier.part else { break };
            let key = Key::of(&p, pk)?;
            if let Some(f) = bound_formula(&key, &b.typed(&key.types)?) {
                fs.push(f);
                terms.push((key.terms[0].clone(), key.types[0]));
            }
            cur = p;
        }
        Ok((fs, terms))
    }

    /// The tables holding rows below partitioned table `t` that a read under `f` reads, in the order Postgres's
    /// Append has them (bound order, depth first): each partition kept unless its bound and `f` (with, where the
    /// table has a default partition, `anc`: what its own bound and its parents' say) can't both hold. None where a
    /// partition is a foreign table, or a key is compared with a value only known when the plan runs.
    fn leaves(
        &self,
        t: &Table,
        f: &F,
        anc: &[F],
        anc_terms: &[(String, Type)],
        out: &mut Vec<Table>,
    ) -> Result<Option<()>> {
        let Some(pk) = &t.hier.part else {
            out.push(t.clone());
            return Ok(Some(()));
        };
        let key = Key::of(t, pk)?;
        if later(f, &key.terms) {
            return Ok(None);
        }
        let kids = hier::kids_in_order(self.v, t)?;
        let mut bounds = vec![];
        for k in &kids {
            bounds.push(match &k.hier.of {
                Some((_, b)) => b.typed(&key.types)?,
                None => PartBound::Default,
            });
        }
        // (a partition's partitions are pruned by its own bound too: partition_qual)
        let here = match anc.is_empty() {
            false => F::And(std::iter::once(f.clone()).chain(anc.iter().cloned()).collect()),
            true => f.clone(),
        };
        for (k, b) in kids.iter().zip(&bounds) {
            if self.prune && !survives(&here, &key, anc, anc_terms, b, &bounds) {
                continue;
            }
            if k.hier.foreign.is_some() {
                return Ok(None);
            }
            if k.hier.part.is_some() {
                let (mut below, mut below_terms) = (anc.to_vec(), anc_terms.to_vec());
                if let Some(bf) = bound_formula(&key, b) {
                    below.push(bf);
                    below_terms.push((key.terms[0].clone(), key.types[0]));
                }
                if self.leaves(k, f, &below, &below_terms, out)?.is_none() {
                    return Ok(None);
                }
            } else {
                out.push(k.clone());
            }
        }
        Ok(Some(()))
    }
}

/// Whether `e` holds what this planner doesn't plan: a subquery, a window function, a parameter, a function made
/// with CREATE FUNCTION (which Postgres may inline, and costs otherwise), a set-returning function.
fn awkward(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| {
        found |= match x {
            Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) | Expr::Window(_) | Expr::WinRef(..) => true,
            Expr::Param(_) | Expr::Idx(_) | Expr::AggRef(_) | Expr::Call(_) | Expr::Default => true,
            Expr::Func(f, _) => SRFS.contains(&func_name(f).as_str()),
            Expr::Agg(_, _, distinct) => *distinct,
            _ => false,
        }
    });
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
        Expr::Func(Func::Distinct(d), args) if matches!(args.get(1), Some(Expr::Lit(Value::Bool(_)))) => {
            Expr::Func(Func::Distinct(!d), args.clone())
        }
        e => Expr::Not(Box::new(e.clone())),
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
    // (a side that is neither a term nor a plain constant: no class)
    let ok = |x: &Expr| has_col(x) || matches!(const_of(x), K::Val(..));
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
fn scan_quals(filter: &Expr) -> Option<Quals> {
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
        let (ka, kb) = (term_key(&a), term_key(&b));
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
            fixed.extend(ec.members.iter().filter(|m| !m.2).map(|m| m.0.clone()));
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
    out.sort_by(|a, b| cost(a).partial_cmp(&cost(b)).unwrap_or(Ordering::Equal));
    Some(Quals::List(out, fixed))
}

/// What a condition costs to check per row, as cost_qual_eval counts it (in cpu_operator_cost): each operator and
/// function call one, an `= ANY (array)` half one per element, tests of null and of truth nothing.
fn cost(e: &Expr) -> f64 {
    match e {
        Expr::Lit(_) | Expr::Col(..) | Expr::Param(_) => 0.0,
        Expr::Neg(x) if matches!(**x, Expr::Lit(_)) => 0.0,
        Expr::Cast(x, _) if matches!(const_of(x), K::Val(..)) => 0.0,
        Expr::Bin(a, Op::And | Op::Or, b) => cost(a) + cost(b),
        Expr::Bin(a, _, b) | Expr::Like(a, b, ..) => 1.0 + cost(a) + cost(b),
        Expr::Not(x) | Expr::IsNull(x, _) => cost(x),
        Expr::Neg(x) | Expr::Cast(x, _) => 1.0 + cost(x),
        Expr::In(x, list, _) => {
            let consts = list.iter().filter(|c| !has_col(c)).count();
            let others: f64 = list.iter().filter(|c| has_col(c)).map(|c| 1.0 + cost(c)).sum();
            let array = match consts {
                0 => 0.0,
                1 => 1.0,
                n if n >= 9 => 2.0,
                n => 0.5 * n as f64,
            };
            cost(x) * (1 + list.iter().filter(|c| has_col(c)).count()) as f64 + array + others
        }
        Expr::Func(Func::Distinct(_), args) if matches!(args.get(1), Some(Expr::Lit(Value::Bool(_)))) => cost(&args[0]),
        Expr::Func(Func::Collate(_) | Func::Coalesce | Func::RowValue, args) => args.iter().map(cost).sum(),
        Expr::Func(_, args) => 1.0 + args.iter().map(cost).sum::<f64>(),
        Expr::Case(o, ws, el) => {
            o.iter().chain(el.iter()).map(|x| cost(x)).sum::<f64>()
                + ws.iter().map(|(w, t)| cost(w) + cost(t)).sum::<f64>()
        }
        Expr::Call(c) => 100.0 + c.args.iter().map(cost).sum::<f64>(),
        _ => 0.0,
    }
}

// ---------------------------------------------------------------- partition pruning

/// What a condition says about a row's values, for pruning: atoms that compare a term (a column, or an expression of
/// columns) with a constant, AND-ed and OR-ed.
#[derive(Clone, Debug)]
enum F {
    And(Vec<F>),
    Or(Vec<F>),
    Atom(String, Pred),
    /// a term compared with what has no columns but isn't a constant (a stable function's value): Postgres prunes on
    /// it as the plan starts
    Later(String),
    /// says nothing about any term
    Any,
}

#[derive(Clone, Debug)]
enum Pred {
    /// term op constant, the constant's type if one was written (None: an untyped literal, or a number's own)
    Cmp(Op, Value, Option<Type>),
    /// IS NULL (true) or IS NOT NULL
    Null(bool),
}

/// A side of a comparison without columns: a constant (and the type it was cast to), what is only known when the
/// plan runs, or neither.
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
        e if has_col(e) => K::No,
        _ => K::Later,
    }
}

/// `term op other` as an atom (`op` as it reads with the term on the left).
fn side(t: String, op: Op, other: &Expr) -> F {
    match const_of(other) {
        K::Val(v, ty) => F::Atom(t, Pred::Cmp(op, v, ty)),
        K::Later => F::Later(t),
        K::No => F::Any,
    }
}

/// A condition (as `simplify` leaves it) as what it says about terms.
fn formula(e: &Expr) -> F {
    let flip = |op: Op| match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        op => op,
    };
    let term = |x: &Expr| has_col(x).then(|| term_key(x));
    match e {
        Expr::Bin(_, op @ (Op::And | Op::Or), _) => {
            let mut parts = vec![];
            chain(e, *op, &mut parts);
            let fs = parts.iter().map(formula).collect();
            if *op == Op::And { F::And(fs) } else { F::Or(fs) }
        }
        Expr::Bin(a, op @ (Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge), b) => match (term(a), term(b)) {
            (Some(t), None) => side(t, *op, b),
            (None, Some(t)) => side(t, flip(*op), a),
            _ => F::Any,
        },
        Expr::IsNull(x, neg) => term(x).map_or(F::Any, |t| F::Atom(t, Pred::Null(!neg))),
        Expr::In(x, list, neg) => match term(x) {
            Some(t) => {
                let op = if *neg { Op::Ne } else { Op::Eq };
                let parts: Vec<F> = list.iter().map(|c| side(t.clone(), op, c)).collect();
                if *neg { F::And(parts) } else { F::Or(parts) }
            }
            None => F::Any,
        },
        // a boolean column alone, or its NOT
        Expr::Col(..) => F::Atom(term_key(e), Pred::Cmp(Op::Eq, Value::Bool(true), Some(Type::Bool))),
        Expr::Not(x) if matches!(**x, Expr::Col(..)) => {
            F::Atom(term_key(x), Pred::Cmp(Op::Eq, Value::Bool(false), Some(Type::Bool)))
        }
        // IS [NOT] TRUE | FALSE
        Expr::Func(Func::Distinct(same), args) => match &args[..] {
            [x, Expr::Lit(Value::Bool(b))] => match term(x) {
                Some(t) if *same => F::Atom(t, Pred::Cmp(Op::Eq, Value::Bool(*b), Some(Type::Bool))),
                Some(t) => F::Or(vec![
                    F::Atom(t.clone(), Pred::Cmp(Op::Eq, Value::Bool(!b), Some(Type::Bool))),
                    F::Atom(t, Pred::Null(true)),
                ]),
                None => F::Any,
            },
            _ => F::Any,
        },
        _ => F::Any,
    }
}

/// Whether `f` compares one of `terms` with a value only known when the plan runs.
fn later(f: &F, terms: &[String]) -> bool {
    match f {
        F::And(xs) | F::Or(xs) => xs.iter().any(|x| later(x, terms)),
        F::Later(t) => terms.contains(t),
        _ => false,
    }
}

/// A partitioned table's key as pruning reads it: its strategy, the term each part is, and their types.
struct Key {
    strategy: Strategy,
    terms: Vec<String>,
    types: Vec<Type>,
}

impl Key {
    fn of(t: &Table, pk: &PartKey) -> Result<Key> {
        let types = pk.types(t)?;
        let mut terms = vec![];
        for p in &pk.parts {
            terms.push(match p {
                KeyPart::Col(id) => t.cols.iter().find(|c| c.id == *id).map_or_else(String::new, |c| c.name.clone()),
                KeyPart::Expr(text) => check_expr(text).map_or_else(|_| String::new(), |e| term_key(&e)),
            });
        }
        Ok(Key { strategy: pk.strategy, terms, types })
    }
}

/// A constant compared with a key of type `key`, as the key's operator family takes it: an untyped literal read as
/// the key's type, a constant of the key's kind (an integer of any width for an integer key, a number for a numeric
/// or floating one) converted; None for one the key's operators don't compare (an integer key with a numeric).
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
    let ok = from == key || (f != 0 && f == k) || (k == 2 && f == 1) || (k == 3 && matches!(f, 1 | 2));
    if ok { coerce(v.clone(), key).ok() } else { None }
}

/// What a partition's bound says about its parent's key, as a condition for its own partitions (pruning a default
/// partition by it): a range's ends (of a multi-column one, its first column's), a list's values.
fn bound_formula(key: &Key, b: &PartBound) -> Option<F> {
    let (t, ty) = (key.terms.first()?.clone(), *key.types.first()?);
    match b {
        PartBound::Range(lo, hi) => {
            let last = key.terms.len() == 1;
            let mut parts = vec![];
            if let Some(Edge::Val(v)) = lo.first() {
                parts.push(F::Atom(t.clone(), Pred::Cmp(Op::Ge, v.clone(), Some(ty))));
            }
            if let Some(Edge::Val(v)) = hi.first() {
                let op = if last { Op::Lt } else { Op::Le };
                parts.push(F::Atom(t, Pred::Cmp(op, v.clone(), Some(ty))));
            }
            (!parts.is_empty()).then_some(F::And(parts))
        }
        PartBound::List(vs) => Some(F::Or(
            vs.iter()
                .map(|v| match v {
                    Value::Null => F::Atom(t.clone(), Pred::Null(true)),
                    v => F::Atom(t.clone(), Pred::Cmp(Op::Eq, v.clone(), Some(ty))),
                })
                .collect(),
        )),
        _ => None,
    }
}

/// Whether the partition of bound `b` (one of `all`, its siblings') may hold a row `f` is true of. `anc`: what the
/// bounds of the tables above say (already in `f`), and `anc_terms` their keys' terms and types, which `f` narrows
/// too (an OR's arm their bounds refute is no way in, as partprune.c drops it).
fn survives(f: &F, key: &Key, anc: &[F], anc_terms: &[(String, Type)], b: &PartBound, all: &[PartBound]) -> bool {
    let (mut terms, mut types) = (key.terms.clone(), key.types.clone());
    for (t, ty) in anc_terms {
        if !terms.contains(t) {
            terms.push(t.clone());
            types.push(*ty);
        }
    }
    let q = vec![Iv::default(); terms.len()];
    let n = key.terms.len();
    match key.strategy {
        Strategy::List => {
            let (mut listed, mut null_kid) = (vec![], false);
            for x in all {
                if let PartBound::List(vs) = x {
                    for v in vs {
                        match v {
                            Value::Null => null_kid = true,
                            v => listed.push(v.clone()),
                        }
                    }
                }
            }
            may(f, &terms, &types, &q, &|q: &[Iv]| list_test(b, &q[0], &listed, null_kid))
        }
        Strategy::Range => {
            let sibs: Vec<(&[Edge], &[Edge])> = all
                .iter()
                .filter_map(|x| match x {
                    PartBound::Range(lo, hi) => Some((&lo[..], &hi[..])),
                    _ => None,
                })
                .collect();
            // (what the bounds above say, which refutes an OR's arm)
            let mut base = q.clone();
            for a in anc {
                let mut atoms = vec![];
                flat(std::slice::from_ref(a), &mut atoms);
                for x in atoms {
                    if let F::Atom(t, pred) = x
                        && let Some(j) = terms.iter().position(|k| k == t)
                    {
                        narrow(&mut base[j], pred, types[j]);
                    }
                }
            }
            kept(f, &terms, &types, n, &base, &|q: &[Iv]| range_test(b, &q[..n], &sibs))
        }
        Strategy::Hash => may(f, &terms, &types, &q, &|q: &[Iv]| hash_test(b, &q[..n], &key.types)),
    }
}

/// Whether two values are equal.
fn same(a: &Value, b: &Value) -> bool {
    compare(a, b).ok() == Some(Ordering::Equal)
}

/// Whether a list partition of bound `b` may hold a row whose key is narrowed to `iv` (`listed`: every value its
/// siblings list; `null_kid`: one of them takes nulls): one of its values within, or null where the key may be; the
/// default one a value no sibling lists (any, unless the key is fixed to one), or a null no sibling takes.
fn list_test(b: &PartBound, iv: &Iv, listed: &[Value], null_kid: bool) -> bool {
    let inside = |d: &Value| {
        let lo = match &iv.lo {
            Some((v, incl)) => match compare(d, v) {
                Ok(Ordering::Greater) => true,
                Ok(Ordering::Equal) => *incl,
                Ok(Ordering::Less) => false,
                Err(_) => true,
            },
            None => true,
        };
        let hi = match &iv.hi {
            Some((v, incl)) => match compare(d, v) {
                Ok(Ordering::Less) => true,
                Ok(Ordering::Equal) => *incl,
                Ok(Ordering::Greater) => false,
                Err(_) => true,
            },
            None => true,
        };
        iv.null != Some(true) && lo && hi && !iv.ne.iter().any(|x| same(d, x))
    };
    match b {
        PartBound::List(vs) => vs.iter().any(|v| match v {
            Value::Null => iv.null != Some(false),
            d => inside(d),
        }),
        PartBound::Default => {
            let nulls = !null_kid && iv.null != Some(false);
            let other = iv.null != Some(true)
                && match (&iv.lo, &iv.hi) {
                    (Some((a, true)), Some((c, true))) if same(a, c) => {
                        !listed.iter().any(|x| same(x, a)) && !iv.ne.iter().any(|x| same(x, a))
                    }
                    _ => true,
                };
            nulls || other
        }
        _ => true,
    }
}

/// What the conditions so far say of one key column: its lowest and highest values (each with whether it is
/// included), the values it isn't, and whether it is null (Some(true)), never null (Some(false)), or either.
#[derive(Clone, Debug, Default)]
struct Iv {
    lo: Option<(Value, bool)>,
    hi: Option<(Value, bool)>,
    null: Option<bool>,
    /// values it isn't (`<>`)
    ne: Vec<Value>,
}

/// Narrows `iv` by `pred`; false where nothing is left.
fn narrow(iv: &mut Iv, pred: &Pred, ty: Type) -> bool {
    let (op, c) = match pred {
        Pred::Null(is) => {
            if iv.null == Some(!is) {
                return false;
            }
            iv.null = Some(*is);
            return true;
        }
        Pred::Cmp(op, v, decl) => match key_value(v, *decl, ty) {
            None => return true,
            Some(Value::Null) => return false,
            Some(c) => (*op, c),
        },
    };
    if iv.null == Some(true) {
        return false;
    }
    iv.null = Some(false);
    match op {
        Op::Eq => {
            tighten(&mut iv.lo, &c, true, true);
            tighten(&mut iv.hi, &c, true, false);
        }
        Op::Lt => tighten(&mut iv.hi, &c, false, false),
        Op::Le => tighten(&mut iv.hi, &c, true, false),
        Op::Gt => tighten(&mut iv.lo, &c, false, true),
        Op::Ge => tighten(&mut iv.lo, &c, true, true),
        Op::Ne => iv.ne.push(c),
        _ => {}
    }
    match (&iv.lo, &iv.hi) {
        (Some((a, ai)), Some((b, bi))) => match compare(a, b) {
            Ok(Ordering::Greater) => false,
            Ok(Ordering::Equal) => *ai && *bi,
            _ => true,
        },
        _ => true,
    }
}

/// Moves an end of a range to `c` (included or not) where that narrows it: `up`, the lower end (raised), else the
/// upper (lowered).
fn tighten(end: &mut Option<(Value, bool)>, c: &Value, incl: bool, up: bool) {
    let o = end.as_ref().map(|(v, _)| compare(c, v));
    match o {
        None => *end = Some((c.clone(), incl)),
        Some(Ok(Ordering::Equal)) => {
            if let Some((_, i)) = end {
                *i &= incl;
            }
        }
        Some(Ok(o)) if (o == Ordering::Greater) == up => *end = Some((c.clone(), incl)),
        _ => {}
    }
}

/// The parts of an AND, an AND inside it one with it.
fn flat<'f>(xs: &'f [F], out: &mut Vec<&'f F>) {
    for x in xs {
        match x {
            F::And(ys) => flat(ys, out),
            x => out.push(x),
        }
    }
}

/// Whether partprune.c's steps for a range-partitioned level keep the partition `test` judges: each comparison of a
/// key column (the first `n` of `terms`) with a constant is a step of its own, with each choice of the equalities of
/// the columns before it, and an AND keeps the partitions every step keeps (so `a = 1 AND a = 3` keeps the partition
/// of both); an OR those any of its arms keeps, an arm the bounds above (`base`) refute left out.
fn kept(f: &F, terms: &[String], types: &[Type], n: usize, base: &[Iv], test: &dyn Fn(&[Iv]) -> bool) -> bool {
    match f {
        F::And(xs) => {
            let mut all = vec![];
            flat(xs, &mut all);
            let mut by_col: Vec<Vec<&Pred>> = vec![vec![]; n];
            for x in &all {
                if let F::Atom(t, pred) = x
                    && let Some(j) = terms.iter().position(|k| k == t)
                    && j < n
                {
                    by_col[j].push(pred);
                }
            }
            let fresh = vec![Iv::default(); terms.len()];
            for j in 0..n {
                for pred in &by_col[j] {
                    match pred {
                        Pred::Null(true) => {
                            let mut q = fresh.clone();
                            q[j].null = Some(true);
                            if !test(&q) {
                                return false;
                            }
                        }
                        Pred::Null(false) | Pred::Cmp(Op::Ne, ..) => {}
                        Pred::Cmp(..) => {
                            // (the columns before: each of their equalities, where every one has some)
                            let mut prefixes = vec![fresh.clone()];
                            for i in 0..j {
                                let eqs: Vec<&Pred> =
                                    by_col[i].iter().copied().filter(|p| matches!(p, Pred::Cmp(Op::Eq, ..))).collect();
                                let mut next = vec![];
                                for pre in &prefixes {
                                    for e in &eqs {
                                        let mut q = pre.clone();
                                        if narrow(&mut q[i], e, types[i]) {
                                            next.push(q);
                                        }
                                    }
                                }
                                prefixes = next;
                            }
                            for mut q in prefixes {
                                if !(narrow(&mut q[j], pred, types[j]) && test(&q)) {
                                    return false;
                                }
                            }
                        }
                    }
                }
            }
            all.iter().filter(|x| matches!(x, F::Or(_))).all(|x| kept(x, terms, types, n, base, test))
        }
        F::Or(xs) => xs.iter().any(|x| !refuted(x, terms, types, base) && kept(x, terms, types, n, base, test)),
        F::Atom(..) => kept(&F::And(vec![f.clone()]), terms, types, n, base, test),
        F::Any | F::Later(_) => true,
    }
}

/// Whether what the bounds above say (`base`) refutes an OR's arm `f`: one of its comparisons can't hold within them.
fn refuted(f: &F, terms: &[String], types: &[Type], base: &[Iv]) -> bool {
    let mut atoms = vec![];
    flat(std::slice::from_ref(f), &mut atoms);
    let mut q = base.to_vec();
    atoms.into_iter().any(|x| match x {
        F::Atom(t, pred) => match terms.iter().position(|k| k == t) {
            Some(j) => !narrow(&mut q[j], pred, types[j]),
            None => false,
        },
        _ => false,
    })
}

/// Whether `f` may be true of a row of the partition `test` judges, the columns of `terms` (of `types`) narrowed to
/// `q`: an AND narrows them by its atoms, an OR may be true by any of its parts.
fn may(f: &F, terms: &[String], types: &[Type], q: &[Iv], test: &dyn Fn(&[Iv]) -> bool) -> bool {
    match f {
        F::And(xs) => {
            let mut all = vec![];
            flat(xs, &mut all);
            let mut q2 = q.to_vec();
            for x in &all {
                if let F::Atom(t, pred) = x
                    && let Some(j) = terms.iter().position(|k| k == t)
                    && !narrow(&mut q2[j], pred, types[j])
                {
                    return false;
                }
            }
            let rest: Vec<&F> = all.into_iter().filter(|x| matches!(x, F::Or(_))).collect();
            if rest.is_empty() { test(&q2) } else { rest.iter().all(|x| may(x, terms, types, &q2, test)) }
        }
        F::Or(xs) => xs.iter().any(|x| may(x, terms, types, q, test)),
        F::Atom(..) => may(&F::And(vec![f.clone()]), terms, types, q, test),
        F::Any | F::Later(_) => test(q),
    }
}

/// The edges of the rows narrowed to `q`, as a range bound's: the lowest (and whether it is included), the highest
/// (likewise), and whether every column was narrowed (else the first column not fixed to one value ends them, and the
/// rest are MINVALUE or MAXVALUE).
fn edges_of(q: &[Iv]) -> (Vec<Edge>, bool, Vec<Edge>, bool, bool) {
    let (mut l, mut u) = (vec![], vec![]);
    let (mut li, mut ui) = (true, true);
    for (j, iv) in q.iter().enumerate() {
        if let (Some((a, true)), Some((b, true))) = (&iv.lo, &iv.hi)
            && compare(a, b).ok() == Some(Ordering::Equal)
        {
            l.push(Edge::Val(a.clone()));
            u.push(Edge::Val(b.clone()));
            continue;
        }
        match &iv.lo {
            Some((a, incl)) => {
                l.push(Edge::Val(a.clone()));
                li = *incl;
            }
            None => l.push(Edge::Min),
        }
        match &iv.hi {
            Some((b, incl)) => {
                u.push(Edge::Val(b.clone()));
                ui = *incl;
            }
            None => u.push(Edge::Max),
        }
        let full = j + 1 == q.len();
        for _ in j + 1..q.len() {
            l.push(if li { Edge::Min } else { Edge::Max });
            u.push(if ui { Edge::Max } else { Edge::Min });
        }
        return (l, li, u, ui, full);
    }
    (l, li, u, ui, true)
}

/// Two edges compared; None where their values don't compare.
fn cmp_e(a: &[Edge], b: &[Edge]) -> Option<Ordering> {
    cmp_edges(a, b).ok()
}

/// Whether a range partition of bound `b` may hold a row narrowed to `q` (`sibs`: the ranges of its siblings, in
/// bound order): a range one where the rows' edges meet its own; the default one where a key may be null, where the
/// key isn't narrowed on every column, or where the rows' range has a gap no sibling covers.
fn range_test(b: &PartBound, q: &[Iv], sibs: &[(&[Edge], &[Edge])]) -> bool {
    let (l, li, u, ui, full) = edges_of(q);
    match b {
        PartBound::Range(lo, hi) => {
            if q.iter().any(|iv| iv.null == Some(true)) {
                return false;
            }
            let (Some(c1), Some(c2), Some(c3)) = (cmp_e(&l, hi), cmp_e(lo, &u), cmp_e(&l, &u)) else { return true };
            c1 == Ordering::Less
                && (c2 == Ordering::Less || (c2 == Ordering::Equal && ui))
                && (c3 == Ordering::Less || (c3 == Ordering::Equal && li && ui))
        }
        PartBound::Default => {
            if q.iter().any(|iv| iv.null != Some(false)) || !full {
                return true;
            }
            // from the lowest edge up, partition by partition: covered where the highest is reached without a gap
            let mut cur = l;
            for _ in 0..=sibs.len() {
                let found = sibs.iter().find(|(lo, hi)| {
                    cmp_e(lo, &cur).is_some_and(|o| o != Ordering::Greater) && cmp_e(&cur, hi) == Some(Ordering::Less)
                });
                let Some((_, hi)) = found else { return true };
                match cmp_e(&u, hi) {
                    Some(Ordering::Less) => return false,
                    Some(Ordering::Equal) if !ui => return false,
                    None => return true,
                    _ => {}
                }
                cur = hi.to_vec();
            }
            true
        }
        _ => true,
    }
}

/// Whether a hash partition of bound `b` may hold a row narrowed to `q`: unless every key column is fixed to one
/// value (or null), any may.
fn hash_test(b: &PartBound, q: &[Iv], types: &[Type]) -> bool {
    let PartBound::Hash(m, r) = b else { return true };
    let mut vals = vec![];
    for iv in q {
        if iv.null == Some(true) {
            vals.push(Value::Null);
            continue;
        }
        match (&iv.lo, &iv.hi) {
            (Some((a, true)), Some((c, true))) if compare(a, c).ok() == Some(Ordering::Equal) => vals.push(a.clone()),
            _ => return true,
        }
    }
    match hash_key(&vals, types) {
        Ok(h) => *m == 0 || h % u64::from(*m) == u64::from(*r),
        Err(_) => true,
    }
}
