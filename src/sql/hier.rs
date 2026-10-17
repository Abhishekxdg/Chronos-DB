//! Declarative partitioning and table inheritance (Postgres's `PARTITION BY`, `PARTITION OF`,
//! `ATTACH PARTITION`, `INHERITS`).
//!
//! Every table in a hierarchy is an ordinary table with its own rows, keyed as any table is; what
//! makes it a partition or a child is a few lines of its schema (`Hier`), and a marker row
//! `_sonos_inh/<parent>/<child>` that lets a parent find its children with one prefix read, and
//! that forks, merges and travels in time with the world like every row. A partitioned table holds
//! no rows itself: a read of it is the read of its partitions put together (as a view is read, see
//! `expand_select`), and a write to it goes to the partition each row belongs to.
//!
//! What `tableoid` says is the oid pg_class gives the table (see `Catalog::rel_oid`), which is
//! worked out for a read that names the column.

use super::partition::*;
use super::*;

/// The marker rows: `_sonos_inh/<parent>/<child>` holds the order the child was made in.
pub(super) const INH: &str = "_sonos_inh";

fn inh_key(parent: &str, child: &str) -> String {
    format!("{INH}/{parent}/{child}")
}

/// What makes a table part of a hierarchy, as its schema row keeps it.
#[derive(Clone, Debug, Default)]
pub(super) struct Hier {
    /// PARTITION BY: what the table is partitioned by (it then has no rows of its own)
    pub(super) part: Option<PartKey>,
    /// a partition: its parent, and its bound
    pub(super) of: Option<(String, PartBound)>,
    /// INHERITS: the parents, in the order written (a partition's parent is in `of`)
    pub(super) parents: Vec<String>,
    /// some table inherits from this one (so a read of it is a read of them too)
    pub(super) kids: bool,
    /// constraints, indexes and triggers a parent gave this partition, by name (`*pkey` for the
    /// primary key): they go with the parent's, and can't be dropped on their own
    pub(super) inherited: Vec<String>,
    /// a foreign table: its server and options, and its columns' (see `foreign`)
    pub(super) foreign: Option<Json>,
    /// CLUSTER ... USING and ALTER TABLE ... CLUSTER ON: the index the table was last clustered on (what
    /// pg_index.indisclustered marks, and a CLUSTER without an index uses)
    pub(super) clustered: Option<String>,
    /// ALTER TABLE ... REPLICA IDENTITY other than DEFAULT: `f` FULL, `n` NOTHING, or `i` USING INDEX and the
    /// index (pg_class.relreplident, and pg_index.indisreplident). What logical replication would send of a
    /// changed row; there is none here, and it is kept as Postgres keeps it.
    pub(super) replident: Option<(char, Option<String>)>,
    /// CREATE TABLE ... OF type: the composite type (its stored name) the table is made of (pg_class.reloftype),
    /// which DROP TYPE finds it by
    pub(super) of_type: Option<String>,
}

impl Hier {
    pub(super) fn from_json(j: &Json) -> Hier {
        let strs = |x: &Json| -> Vec<String> {
            x.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
        };
        let of = j["partition_of"]
            .as_object()
            .and_then(|o| Some((o.get("parent")?.as_str()?.to_string(), PartBound::from_json(o.get("bound")?)?)));
        Hier {
            part: PartKey::from_json(&j["partition_by"]),
            of,
            parents: strs(&j["inherits"]),
            kids: j["kids"] == true,
            inherited: strs(&j["inherited"]),
            foreign: j.get("ftable").cloned(),
            clustered: j["clustered"].as_str().map(String::from),
            replident: j["replident"]
                .as_str()
                .and_then(|s| s.chars().next())
                .map(|k| (k, j["replident_index"].as_str().map(String::from))),
            of_type: j["of_type"].as_str().map(String::from),
        }
    }

    pub(super) fn write_json(&self, j: &mut Json) {
        if let Some(p) = &self.part {
            j["partition_by"] = p.to_json();
        }
        if let Some((parent, b)) = &self.of {
            j["partition_of"] = serde_json::json!({ "parent": parent, "bound": b.to_json() });
        }
        if !self.parents.is_empty() {
            j["inherits"] = serde_json::json!(self.parents);
        }
        if self.kids {
            j["kids"] = Json::Bool(true);
        }
        if !self.inherited.is_empty() {
            j["inherited"] = serde_json::json!(self.inherited);
        }
        if let Some(f) = &self.foreign {
            j["ftable"] = f.clone();
        }
        if let Some(x) = &self.clustered {
            j["clustered"] = Json::String(x.clone());
        }
        if let Some(x) = &self.of_type {
            j["of_type"] = Json::String(x.clone());
        }
        if let Some((k, index)) = &self.replident {
            j["replident"] = Json::String(k.to_string());
            if let Some(x) = index {
                j["replident_index"] = Json::String(x.clone());
            }
        }
    }

    /// Whether any other table may be a child of this one.
    pub(super) fn is_parent(&self) -> bool {
        self.part.is_some() || self.kids
    }

    /// Every table this one inherits from, a partition's parent first.
    pub(super) fn all_parents(&self) -> Vec<&str> {
        self.of.iter().map(|(p, _)| p.as_str()).chain(self.parents.iter().map(String::as_str)).collect()
    }
}

// ---------------------------------------------------------------- parsed statements

/// A partition key item as written: a column, or an expression with its text.
#[derive(Clone, Debug)]
pub(super) enum PartItem {
    Col(String),
    Expr(Expr, String),
}

/// One end of a range bound as written.
#[derive(Clone, Debug)]
pub(super) enum EdgeSpec {
    Min,
    Max,
    Val(Expr),
}

/// FOR VALUES ... as written, its values not yet worked out.
#[derive(Clone, Debug)]
pub(super) enum BoundSpec {
    Range(Vec<EdgeSpec>, Vec<EdgeSpec>),
    List(Vec<Expr>),
    /// WITH (MODULUS m, REMAINDER r): each None when not given, each given at most once
    Hash(Option<i64>, Option<i64>),
    Default,
}

/// What CREATE TABLE says about partitions and inheritance.
#[derive(Clone, Debug, Default)]
pub(super) struct CreateHier {
    /// PARTITION BY strategy (items)
    pub(super) by: Option<(Strategy, Vec<PartItem>)>,
    /// PARTITION OF parent FOR VALUES ...
    pub(super) of: Option<(String, BoundSpec)>,
    /// INHERITS (parents)
    pub(super) inherits: Vec<String>,
    /// CREATE FOREIGN TABLE's server and options, and its columns' by name (see `Parser::foreign_table`)
    pub(super) foreign: Option<Json>,
    /// CONSTRAINT name PRIMARY KEY (...): the key's name (not the hierarchy's, but written beside the columns too)
    pub(super) pkey_name: Option<String>,
    /// OF type: the composite type's stored name (see `Hier::of_type`)
    pub(super) of_type: Option<String>,
}

/// How ALTER TABLE ... DETACH PARTITION goes about it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Detach {
    Plain,
    Concurrently,
    Finalize,
}

impl Parser {
    /// After PARTITION BY: `RANGE | LIST | HASH (column | (expression) | function(...) [COLLATE c] [opclass], ...)`.
    pub(super) fn partition_by(&mut self) -> Result<(Strategy, Vec<PartItem>)> {
        let strategy = match self.next() {
            Some(Tok::Word(w) | Tok::Ident(w)) => match Strategy::from_name(&w) {
                Some(s) => s,
                None => return err("22023", format!("unrecognized partitioning strategy \"{w}\"")),
            },
            t => {
                self.put_back(t);
                return self.fail("expected RANGE, LIST or HASH");
            }
        };
        self.expect_sym("(")?;
        let mut items = vec![];
        loop {
            let (start, toks) = (self.i, self.ahead(true));
            let e = self.expr()?;
            let text = tokens_shown(self.since(&toks, start)?);
            // a parenthesized expression or a function call; a bare column
            items.push(match &e {
                Expr::Col(None, n) => PartItem::Col(n.clone()),
                _ => PartItem::Expr(e, text),
            });
            // COLLATE: one of bytes is how keys compare here; ranges in an ICU one would route rows differently
            if self.kw("collate") && !self.collation_name()?.bytes() {
                return err(UNSUPPORTED, "a partition key in an ICU collation isn't supported yet");
            }
            // an operator class, by name: what it is for is the type's default's business here
            if matches!(self.peek(), Some(Tok::Word(_) | Tok::Ident(_))) {
                self.name()?;
            }
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok((strategy, items))
    }

    /// After PARTITION OF parent: `FOR VALUES { IN (..) | FROM (..) TO (..) | WITH (MODULUS m, REMAINDER r) } | DEFAULT`.
    pub(super) fn partition_bound(&mut self) -> Result<BoundSpec> {
        if self.kw("default") {
            return Ok(BoundSpec::Default);
        }
        self.expect_kw("for")?;
        self.expect_kw("values")?;
        if self.kw("in") {
            self.expect_sym("(")?;
            let list = self.list()?;
            self.expect_sym(")")?;
            return Ok(BoundSpec::List(list));
        }
        if self.kw("from") {
            let from = self.range_edges()?;
            self.expect_kw("to")?;
            let to = self.range_edges()?;
            return Ok(BoundSpec::Range(from, to));
        }
        if self.kw("with") {
            self.expect_sym("(")?;
            let (mut modulus, mut remainder) = (None, None);
            loop {
                let which = match self.next() {
                    Some(Tok::Word(w)) if w == "modulus" || w == "remainder" => w,
                    t => {
                        self.put_back(t);
                        return self.fail("expected MODULUS or REMAINDER");
                    }
                };
                let n = match self.next() {
                    Some(Tok::Num(n)) => n.parse::<i64>().map_err(|_| SqlError {
                        code: SYNTAX,
                        message: format!("syntax error at or near \"{n}\""),
                        info: None,
                    })?,
                    t => {
                        self.put_back(t);
                        return self.fail("expected a number");
                    }
                };
                let slot = if which == "modulus" { &mut modulus } else { &mut remainder };
                if slot.is_some() {
                    return err("42710", format!("{which} for hash partition provided more than once"));
                }
                *slot = Some(n);
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            return Ok(BoundSpec::Hash(modulus, remainder));
        }
        self.fail("expected IN, FROM or WITH")
    }

    /// `(value | MINVALUE | MAXVALUE, ...)`
    fn range_edges(&mut self) -> Result<Vec<EdgeSpec>> {
        self.expect_sym("(")?;
        let mut out = vec![];
        loop {
            out.push(if self.kw("minvalue") {
                EdgeSpec::Min
            } else if self.kw("maxvalue") {
                EdgeSpec::Max
            } else {
                EdgeSpec::Val(self.expr()?)
            });
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(out)
    }

    /// CREATE TABLE's clauses after the column list: `INHERITS (parents)` and `PARTITION BY ...`.
    pub(super) fn hier_clause(&mut self, h: &mut CreateHier) -> Result<bool> {
        if self.kw("inherits") {
            self.expect_sym("(")?;
            h.inherits = self.rel_names()?;
            self.expect_sym(")")?;
            return Ok(true);
        }
        if self.kw("partition") {
            self.expect_kw("by")?;
            h.by = Some(self.partition_by()?);
            return Ok(true);
        }
        Ok(false)
    }
}

// ---------------------------------------------------------------- finding a hierarchy

/// The direct children of `name` (its partitions, and the tables inheriting from it), in the
/// order they were made.
pub(super) fn child_names(v: &View, name: &str) -> Result<Vec<String>> {
    let prefix = format!("{INH}/{name}/");
    let mut out: Vec<(u64, String)> = vec![];
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        out.push((raw.parse().unwrap_or(0), k[prefix.len()..].to_string()));
    }
    out.sort();
    Ok(out.into_iter().map(|(_, n)| n).collect())
}

/// The direct children, loaded.
pub(super) fn children(v: &View, name: &str) -> Result<Vec<Table>> {
    child_names(v, name)?.iter().map(|n| need_table(v, n)).collect()
}

/// Every table below `t` (each once, however many paths lead to it), in the order Postgres reads
/// them: a partitioned table's partitions depth first in bound order; inheritance children a level
/// at a time, each table's in the order they were made (as find_all_inheritors walks them).
pub(super) fn descendants(v: &View, t: &Table) -> Result<Vec<Table>> {
    let mut out: Vec<Table> = vec![];
    let mut seen = HashSet::new();
    if t.hier.part.is_none() {
        let mut at = 0;
        let mut cur = t.clone();
        loop {
            for c in kids_in_order(v, &cur)? {
                if c.name != t.name && seen.insert(c.name.clone()) {
                    out.push(c);
                }
            }
            let Some(next) = out.get(at) else { break };
            cur = next.clone();
            at += 1;
        }
        return Ok(out);
    }
    fn go(v: &View, t: &Table, out: &mut Vec<Table>, seen: &mut HashSet<String>) -> Result<()> {
        for c in kids_in_order(v, t)? {
            if seen.insert(c.name.clone()) {
                out.push(c.clone());
                go(v, &c, out, seen)?;
            }
        }
        Ok(())
    }
    go(v, t, &mut out, &mut seen)?;
    Ok(out)
}

/// The children of `t`, in the order a read gives their rows: a partitioned table's by bound
/// (the default one last), the others' as they were made.
pub(super) fn kids_in_order(v: &View, t: &Table) -> Result<Vec<Table>> {
    if !t.hier.is_parent() {
        return Ok(vec![]);
    }
    let kids = children(v, &t.name)?;
    let Some(key) = &t.hier.part else { return Ok(kids) };
    let types = key.types(t)?;
    let mut typed: Vec<(PartBound, Table)> = vec![];
    for k in kids {
        let b = k.hier.of.as_ref().map(|(_, b)| b.typed(&types)).transpose()?.unwrap_or(PartBound::Default);
        typed.push((b, k));
    }
    let mut failed = None;
    typed.sort_by(|(a, x), (b, y)| {
        let rank = |b: &PartBound| match b {
            PartBound::Default => 1,
            _ => 0,
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| match (a, b) {
                (PartBound::Range(f, _), PartBound::Range(g, _)) => cmp_edges(f, g).unwrap_or_else(|e| {
                    failed = Some(e);
                    Ordering::Equal
                }),
                (PartBound::Hash(m, r), PartBound::Hash(n, s)) => (m, r).cmp(&(n, s)),
                (PartBound::List(p), PartBound::List(q)) => {
                    // by the smallest value, nulls last
                    let low = |l: &Vec<Value>| {
                        l.iter()
                            .filter(|v| **v != Value::Null)
                            .cloned()
                            .min_by(|a, b| compare(a, b).unwrap_or(Ordering::Equal))
                    };
                    match (low(p), low(q)) {
                        (Some(a), Some(b)) => compare(&a, &b).unwrap_or(Ordering::Equal),
                        (None, Some(_)) => Ordering::Greater,
                        (Some(_), None) => Ordering::Less,
                        _ => Ordering::Equal,
                    }
                }
                _ => Ordering::Equal,
            })
            .then_with(|| x.name.cmp(&y.name))
    });
    if let Some(e) = failed {
        return Err(e);
    }
    Ok(typed.into_iter().map(|(_, k)| k).collect())
}

/// The tables whose rows a read of `t` gives: `t` itself unless it is partitioned, then each
/// table below it that holds rows (a partitioned one doesn't).
pub(super) fn row_tables(v: &View, t: &Table) -> Result<Vec<Table>> {
    let mut out = vec![];
    if t.hier.part.is_none() {
        out.push(t.clone());
    }
    out.extend(descendants(v, t)?.into_iter().filter(|d| d.hier.part.is_none()));
    Ok(out)
}

// ---------------------------------------------------------------- errors and small helpers

/// The first of `t`'s row triggers with transition tables, which a partition or an inheritance child can't have
/// (Postgres's FindTriggerIncompatibleWithInheritance).
fn transition_row_trigger(t: &Table) -> Option<&str> {
    t.triggers.iter().find(|x| x.row && (x.old_table.is_some() || x.new_table.is_some())).map(|x| x.name.as_str())
}

fn err_detail<T>(code: &'static str, message: impl Into<String>, detail: impl Into<String>) -> Result<T> {
    Err(SqlError {
        code,
        message: message.into(),
        info: Some(Box::new(Violation { detail: Some(detail.into()), ..Violation::default() })),
    })
}

pub(super) fn truncate_only() -> SqlError {
    SqlError {
        code: "42809",
        message: "cannot truncate only a partitioned table".into(),
        info: Some(Box::new(Violation {
            hint: Some("Do not specify the ONLY keyword, or use TRUNCATE ONLY on the partitions directly.".into()),
            ..Violation::default()
        })),
    }
}

/// The tables `t` is a child or a partition of, and theirs in turn: each once.
pub(super) fn ancestors(v: &View, t: &Table) -> Result<Vec<Table>> {
    let mut out: Vec<Table> = vec![];
    let mut todo: Vec<String> = t.hier.all_parents().into_iter().map(String::from).collect();
    while let Some(n) = todo.pop() {
        if out.iter().any(|o| o.name == n) || n == t.name {
            continue;
        }
        let Some(p) = load(v, &n)? else { continue };
        todo.extend(p.hier.all_parents().into_iter().map(String::from));
        out.push(p);
    }
    Ok(out)
}

/// `t`'s name and its ancestors': the names a foreign key to a partitioned table may carry.
pub(super) fn with_ancestors(v: &View, t: &Table) -> Result<Vec<String>> {
    if t.hier.all_parents().is_empty() {
        return Ok(vec![t.name.clone()]);
    }
    let mut out = vec![t.name.clone()];
    out.extend(ancestors(v, t)?.into_iter().map(|a| a.name));
    Ok(out)
}

/// A column's type as a message shows it: `integer`, `numeric(5,2)`, `character varying(10)`.
pub(super) fn type_text(c: &Col) -> String {
    match (c.numeric, c.chars) {
        (Some((p, s)), _) => format!("numeric({p},{s})"),
        (_, Some((n, fixed))) => super::chars_name(n, fixed),
        _ if c.small => "smallint".into(),
        _ => c.ty.pg_name().into_owned(),
    }
}

fn same_type(a: &Col, b: &Col) -> bool {
    a.ty == b.ty && a.numeric == b.numeric && a.chars == b.chars && a.small == b.small && a.dim == b.dim
}

/// 42P21, as Postgres reports a column whose two definitions have different collations: `"a" versus "b"`.
fn coll_conflict<T>(message: String, a: super::collation::Coll, b: super::collation::Coll) -> Result<T> {
    err_detail("42P21", message, format!("\"{}\" versus \"{}\"", a.collname(), b.collname()))
}

/// `vals` of `from`'s columns as `to`'s columns have them (by name; a column `to` has and `from`
/// doesn't is null).
pub(super) fn to_layout(vals: &[Value], from: &Table, to: &Table) -> Vec<Value> {
    if from.cols.len() == to.cols.len() && from.cols.iter().zip(&to.cols).all(|(a, b)| a.name == b.name) {
        return vals.to_vec();
    }
    to.cols
        .iter()
        .map(|c| from.cols.iter().position(|x| x.name == c.name).map_or(Value::Null, |i| vals[i].clone()))
        .collect()
}

/// The copy of a parent's column a child starts from.
fn inherit_col(pc: &Col, table: &str) -> Col {
    let mut c = Col { table: table.into(), pk: false, missing: None, inh: 1, local: false, ..pc.clone() };
    // an identity column's counter isn't inherited, nor is the default it is; the NOT NULL is
    if c.identity.is_some() {
        (c.identity, c.auto, c.seq, c.default, c.default_sql) = (None, None, None, None, None);
        c.not_null = true;
    }
    c
}

/// The columns of a table that inherits from `parents` and writes `local` itself, as Postgres's
/// MergeAttributes makes them: the parents' in order (one two parents share merged into one), then
/// the table's own that no parent has. A column both write takes the table's default.
pub(super) fn merge_columns(table: &str, parents: &[Table], local: Vec<Col>, say: bool) -> Result<Vec<Col>> {
    let mut cols: Vec<Col> = vec![];
    let mut conflicts: Vec<String> = vec![];
    for p in parents {
        for pc in &p.cols {
            let Some(c) = cols.iter_mut().find(|c| c.name == pc.name) else {
                cols.push(inherit_col(pc, table));
                continue;
            };
            if !same_type(c, pc) {
                return err_detail(
                    WRONG_TYPE,
                    format!("inherited column \"{}\" has a type conflict", pc.name),
                    format!("{} versus {}", type_text(c), type_text(pc)),
                );
            }
            if c.coll != pc.coll {
                return coll_conflict(
                    format!("inherited column \"{}\" has a collation conflict", pc.name),
                    c.coll,
                    pc.coll,
                );
            }
            if say {
                notice("NOTICE", format!("merging multiple inherited definitions of column \"{}\"", pc.name));
            }
            // (a default is one value, or SQL worked out for each row)
            let has = |c: &Col| c.default.is_some() || c.default_sql.is_some();
            if has(c) && has(pc) && (c.default != pc.default || c.default_sql != pc.default_sql) {
                conflicts.push(pc.name.clone()); // (the table's own default for it settles this)
            }
            c.inh += 1;
            c.not_null |= pc.not_null;
            if c.default.is_none() && c.auto.is_none() && c.default_sql.is_none() {
                (c.default, c.auto, c.seq) = (pc.default.clone(), pc.auto, pc.seq.clone());
                c.default_sql = pc.default_sql.clone();
            }
        }
    }
    for (n, lc) in local.into_iter().enumerate() {
        let Some(at) = cols.iter().position(|c| c.name == lc.name) else {
            cols.push(lc);
            continue;
        };
        let c = &mut cols[at];
        if say && at == n {
            notice("NOTICE", format!("merging column \"{}\" with inherited definition", lc.name));
        } else if say {
            notice_detail(
                "NOTICE",
                format!("moving and merging column \"{}\" with inherited definition", lc.name),
                "User-specified column moved to the position of the inherited column.".into(),
            );
        }
        if !same_type(c, &lc) {
            return err_detail(
                WRONG_TYPE,
                format!("column \"{}\" has a type conflict", lc.name),
                format!("{} versus {}", type_text(c), type_text(&lc)),
            );
        }
        // (one written without COLLATE has the default, which an inherited collation isn't)
        if c.coll != lc.coll {
            return coll_conflict(format!("column \"{}\" has a collation conflict", lc.name), c.coll, lc.coll);
        }
        c.local = true;
        c.not_null |= lc.not_null;
        if lc.default.is_some() || lc.auto.is_some() || lc.default_sql.is_some() {
            conflicts.retain(|x| *x != lc.name);
            (c.default, c.auto, c.seq) = (lc.default, lc.auto, lc.seq);
            c.default_sql = lc.default_sql;
        }
        if lc.generated.is_some() {
            c.generated = lc.generated;
        }
    }
    if let Some(n) = conflicts.first() {
        return Err(SqlError {
            code: "42611",
            message: format!("column \"{n}\" inherits conflicting default values"),
            info: Some(Box::new(Violation {
                hint: Some("To resolve the conflict, specify a default explicitly.".into()),
                ..Violation::default()
            })),
        });
    }
    for (i, c) in cols.iter_mut().enumerate() {
        c.id = i as u32;
        c.table = table.into();
    }
    Ok(cols)
}

/// Whether two CHECK conditions (as SQL) say the same.
pub(super) fn same_check(a: &str, b: &str) -> bool {
    match (check_expr(a), check_expr(b)) {
        (Ok(x), Ok(y)) => format!("{x:?}") == format!("{y:?}"),
        _ => a == b,
    }
}

/// `child` takes the CHECK constraints of `parents` that can be inherited, as Postgres makes them:
/// the same name, counted once per parent; one the child writes itself under that name is that one.
pub(super) fn inherit_checks(child: &mut Table, parents: &[Table]) -> Result<()> {
    let (short, partition) = (rel_part(&child.name).to_string(), child.hier.of.is_some());
    for p in parents {
        for c in p.checks.iter().filter(|c| !c.no_inherit) {
            match child.checks.iter_mut().find(|x| x.name == c.name) {
                Some(x) if same_check(&x.text, &c.text) => {
                    // (CREATE TABLE ... INHERITS: the table's own of the name is the inherited one, as
                    // Postgres's MergeWithExistingConstraint merges it; a NO INHERIT one can't be)
                    if !partition && x.local && x.inh == 0 {
                        if x.no_inherit {
                            return err(
                                "42P17",
                                format!(
                                    "constraint \"{}\" conflicts with inherited constraint on relation \"{short}\"",
                                    x.name
                                ),
                            );
                        }
                        notice("NOTICE", format!("merging constraint \"{}\" with inherited definition", x.name));
                    }
                    x.inh += 1;
                }
                Some(x) => {
                    return err(
                        "42710",
                        format!(
                            "check constraint name \"{}\" appears multiple times but with different expressions",
                            x.name
                        ),
                    );
                }
                None => child.checks.push(Check {
                    name: c.name.clone(),
                    text: c.text.clone(),
                    inh: 1,
                    local: false,
                    no_inherit: false,
                    valid: true,
                }),
            }
        }
    }
    Ok(())
}

/// `ids` (of `from`'s columns) as the ids of the columns `to` has of those names.
pub(super) fn map_ids(ids: &[u32], from: &Table, to: &Table) -> Option<Vec<u32>> {
    ids.iter()
        .map(|id| {
            if *id == btree::EXPR {
                return Some(*id);
            }
            let name = &from.cols.iter().find(|c| c.id == *id)?.name;
            to.cols.iter().find(|c| c.name == *name).map(|c| c.id)
        })
        .collect()
}

// ---------------------------------------------------------------- partition keys and bounds

/// Whether an expression in a partition key is one Postgres would refuse as not immutable: the
/// clock, a random value, a sequence, and a function of a time with a zone (whose answer depends
/// on the session's TimeZone).
pub(super) fn not_immutable(e: &Expr, cols: &[Col]) -> bool {
    let mut bad = false;
    walk(e, &mut |x| bad |= matches!(x, Expr::Func(Func::Ctx(_), _) | Expr::Call(_)) || mutable_here(x, cols));
    bad
}

/// Whether node `x` (not what is under it) is a call Postgres marks STABLE or VOLATILE: the clock, a random value, a
/// generated uuid, and a function of a time with a zone (whose answer depends on the session's TimeZone), as read
/// over columns `cols`.
pub(super) fn mutable_here(x: &Expr, cols: &[Col]) -> bool {
    use funcs::Scalar::{Random, SetSeed, ToChar};
    let zoned =
        |a: &Expr| matches!(a, Expr::Col(_, n) if cols.iter().any(|c| c.name == *n && c.ty == Type::TimestampTz));
    match x {
        Expr::Func(
            Func::GenRandomUuid
            | Func::Now
            | Func::StatementTs
            | Func::ClockTs
            | Func::CurrentDate
            | Func::Scalar(Random | SetSeed),
            _,
        ) => true,
        Expr::Func(Func::Scalar(f), _) if super::pgcrypto::volatile(*f) => true,
        // (date_trunc of a date is the timestamptz one: only STABLE)
        Expr::Func(Func::DateTrunc, args)
            if args
                .iter()
                .any(|a| matches!(a, Expr::Col(_, n) if cols.iter().any(|c| c.name == *n && c.ty == Type::Date))) =>
        {
            true
        }
        Expr::Func(Func::DateTrunc | Func::Extract | Func::DatePart | Func::Scalar(ToChar), args) => {
            args.iter().any(zoned)
        }
        Expr::Cast(a, Type::Date | Type::Text | Type::Time | Type::Timestamp) => zoned(a),
        _ => false,
    }
}

impl Exec<'_> {
    /// The partition key PARTITION BY's `items` make on table `t` (whose columns are all there is to
    /// read), checked as Postgres checks it.
    pub(super) fn partition_key(&self, t: &Table, strategy: Strategy, items: &[PartItem]) -> Result<PartKey> {
        if strategy == Strategy::List && items.len() > 1 {
            return err("42P17", "cannot use \"list\" partition strategy with more than one column");
        }
        let mut parts = vec![];
        for item in items {
            match item {
                PartItem::Col(n) => {
                    if system_column(n).is_some() || n == "tableoid" || n == "ctid" {
                        return err("42P17", format!("cannot use system column \"{n}\" in partition key"));
                    }
                    let Some(c) = t.cols.iter().find(|c| c.name == *n) else {
                        return err(NO_COLUMN, format!("column \"{n}\" named in partition key does not exist"));
                    };
                    if c.generated.is_some() {
                        return err_detail(
                            "42P17",
                            "cannot use generated column in partition key",
                            format!("Column \"{n}\" is a generated column."),
                        );
                    }
                    // ranges of text in an ICU collation would route rows by its order, which bounds here don't keep
                    if strategy == Strategy::Range && c.coll.icu() {
                        return err(UNSUPPORTED, "range partitioning by text in an ICU collation isn't supported yet");
                    }
                    // (a row finds its partition by its key's bytes)
                    if c.coll.nondeterministic() {
                        return Err(super::collation::nd_refused("partitioning by a key"));
                    }
                    parts.push(KeyPart::Col(c.id));
                }
                PartItem::Expr(e, text) => {
                    let mut sub = false;
                    let mut reads: Vec<String> = vec![];
                    walk(e, &mut |x| match x {
                        Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) => sub = true,
                        Expr::Col(_, n) => reads.push(n.clone()),
                        _ => {}
                    });
                    if sub {
                        return err(UNSUPPORTED, "cannot use subquery in partition key expression");
                    }
                    if let Some(n) =
                        reads.iter().find(|n| system_column(n).is_some() || *n == "tableoid" || *n == "ctid")
                    {
                        return err("42P17", format!("cannot use system column \"{n}\" in partition key"));
                    }
                    let bound = Binder::plain(&t.cols, None).bind(e)?;
                    if strategy == Strategy::Range
                        && super::collation::of(&bound, &t.cols, &[], &|_| None)?.coll().icu()
                    {
                        return err(UNSUPPORTED, "range partitioning by text in an ICU collation isn't supported yet");
                    }
                    if super::collation::nd_in(&bound, &t.cols, &[], &|_| None) {
                        return Err(super::collation::nd_refused("partitioning by a key"));
                    }
                    for n in &reads {
                        if let Some(c) = t.cols.iter().find(|c| c.name == *n && c.generated.is_some()) {
                            return err_detail(
                                "42P17",
                                "cannot use generated column in partition key",
                                format!("Column \"{}\" is a generated column.", c.name),
                            );
                        }
                    }
                    if reads.is_empty() {
                        return err("42P17", "cannot use constant expression as partition key");
                    }
                    if not_immutable(&settle_all(e, &t.cols), &t.cols) {
                        return err("42P17", "functions in partition key expression must be marked IMMUTABLE");
                    }
                    parts.push(KeyPart::Expr(text.clone()));
                }
            }
        }
        let key = PartKey { strategy, parts };
        for (ty, what) in key.types(t)?.into_iter().zip(0..) {
            let c = key.parts.get(what);
            let _ = c;
            if matches!(ty, Type::Geometry | Type::Geography | Type::Vector | Type::TsVector | Type::TsQuery) {
                return Err(SqlError {
                    code: "42704",
                    message: format!("data type {} has no default operator class for access method \"btree\"", ty.name()),
                    info: Some(Box::new(Violation {
                        hint: Some(
                            "You must specify a btree operator class or define a default btree operator class for the data type."
                                .into(),
                        ),
                        ..Violation::default()
                    })),
                });
            }
        }
        Ok(key)
    }

    /// A constant in a partition bound, as a value of the key column's type.
    fn bound_value(&self, e: &Expr, ty: Type, column: &str) -> Result<Value> {
        let v = self.constant(e)?;
        if v == Value::Null {
            return Ok(v);
        }
        coerce(v, ty).map_err(|x| match x.code {
            NO_OPERATOR | WRONG_TYPE => SqlError {
                code: WRONG_TYPE,
                message: format!("specified value cannot be cast to type {} for column \"{column}\"", ty.name()),
                info: None,
            },
            _ => x,
        })
    }

    /// The bound `spec` says for a partition of `parent`, as values of the key's types, and refused
    /// where it doesn't fit the table's strategy or its columns.
    pub(super) fn make_bound(&self, parent: &Table, spec: &BoundSpec) -> Result<PartBound> {
        let Some(key) = &parent.hier.part else {
            return err("42P17", format!("\"{}\" is not partitioned", rel_part(&parent.name)));
        };
        let types = key.types(parent)?;
        let names: Vec<String> = key
            .parts
            .iter()
            .map(|p| match p {
                KeyPart::Col(id) => {
                    parent.cols.iter().find(|c| c.id == *id).map_or_else(String::new, |c| c.name.clone())
                }
                KeyPart::Expr(_) => String::new(),
            })
            .collect();
        let named = |i: usize| if names[i].is_empty() { "?".to_string() } else { names[i].clone() };
        let wrong = |what: &str| err("42P16", format!("invalid bound specification for a {what} partition"));
        match (key.strategy, spec) {
            (Strategy::Hash, BoundSpec::Default) => {
                err("42P16", "a hash-partitioned table may not have a default partition")
            }
            (_, BoundSpec::Default) => Ok(PartBound::Default),
            (Strategy::Range, BoundSpec::Range(from, to)) => {
                let side = |es: &[EdgeSpec], what: &str| -> Result<Vec<Edge>> {
                    if es.len() != types.len() {
                        return err("42P16", format!("{what} must specify exactly one value per partitioning column"));
                    }
                    let mut out: Vec<Edge> = vec![];
                    for (i, e) in es.iter().enumerate() {
                        match e {
                            EdgeSpec::Min | EdgeSpec::Max => {
                                let (want, word) =
                                    if matches!(e, EdgeSpec::Min) { (true, "MINVALUE") } else { (false, "MAXVALUE") };
                                let _ = want;
                                if let Some(prev) = out.last()
                                    && matches!(prev, Edge::Min | Edge::Max)
                                    && (matches!(prev, Edge::Min) != matches!(e, EdgeSpec::Min))
                                {
                                    let w = if matches!(prev, Edge::Min) { "MINVALUE" } else { "MAXVALUE" };
                                    return err(WRONG_TYPE, format!("every bound following {w} must also be {w}"));
                                }
                                let _ = word;
                                out.push(if matches!(e, EdgeSpec::Min) { Edge::Min } else { Edge::Max });
                            }
                            EdgeSpec::Val(x) => {
                                if let Some(prev @ (Edge::Min | Edge::Max)) = out.last() {
                                    let w = if matches!(prev, Edge::Min) { "MINVALUE" } else { "MAXVALUE" };
                                    return err(WRONG_TYPE, format!("every bound following {w} must also be {w}"));
                                }
                                let v = self.bound_value(x, types[i], &named(i))?;
                                if v == Value::Null {
                                    return err(SYNTAX, "cannot specify NULL in range bound");
                                }
                                out.push(Edge::Val(v));
                            }
                        }
                    }
                    Ok(out)
                };
                Ok(PartBound::Range(side(from, "FROM")?, side(to, "TO")?))
            }
            (Strategy::Range, _) => wrong("range"),
            (Strategy::List, BoundSpec::List(vals)) => {
                let mut out = vec![];
                for e in vals {
                    out.push(self.bound_value(e, types[0], &named(0))?);
                }
                Ok(PartBound::List(out))
            }
            (Strategy::List, _) => wrong("list"),
            (Strategy::Hash, BoundSpec::Hash(m, r)) => {
                let Some(m) = *m else { return err(SYNTAX, "modulus for hash partition must be specified") };
                let Some(r) = *r else { return err(SYNTAX, "remainder for hash partition must be specified") };
                if m <= 0 || m > i64::from(i32::MAX) {
                    return err("42P16", "modulus for hash partition must be an integer value greater than zero");
                }
                if r < 0 {
                    return err(
                        "42P16",
                        "remainder for hash partition must be an integer value greater than or equal to zero",
                    );
                }
                if r >= m {
                    return err("42P16", "remainder for hash partition must be less than modulus");
                }
                Ok(PartBound::Hash(m as u32, r as u32))
            }
            (Strategy::Hash, _) => wrong("hash"),
        }
    }
}

/// Refuses bound `new` for partition `name` of `parent` where Postgres does: a range with no rows
/// in it, one that overlaps another partition (or takes the place of the default one), a hash
/// modulus that isn't a factor of the next larger. `skip`: a partition not counted (the one being
/// attached is a child of nothing yet, but a detached-then-attached one may still be listed).
pub(super) fn check_bound(v: &View, parent: &Table, name: &str, new: &PartBound) -> Result<()> {
    let Some(key) = &parent.hier.part else { return Ok(()) };
    let types = key.types(parent)?;
    let shown = key.shows(parent)?;
    let mut sibs: Vec<(String, PartBound)> = vec![];
    for k in kids_in_order(v, parent)? {
        if k.name == name {
            continue;
        }
        if let Some((_, b)) = &k.hier.of {
            sibs.push((k.name.clone(), b.typed(&types)?));
        }
    }
    let short = |n: &str| rel_part(n).to_string();
    let overlap =
        |with: &str| err("42P17", format!("partition \"{}\" would overlap partition \"{}\"", short(name), short(with)));
    match new {
        PartBound::Default => {
            if let Some((d, _)) = sibs.iter().find(|(_, b)| *b == PartBound::Default) {
                return err(
                    "42P17",
                    format!("partition \"{}\" conflicts with existing default partition \"{}\"", short(name), short(d)),
                );
            }
        }
        PartBound::Range(f, t) => {
            if cmp_edges(f, t)? != Ordering::Less {
                return err_detail(
                    "42P17",
                    format!("empty range bound specified for partition \"{}\"", short(name)),
                    format!(
                        "Specified lower bound {} is greater than or equal to upper bound {}.",
                        edges_text(f, &shown),
                        edges_text(t, &shown)
                    ),
                );
            }
            // the first partition (in bound order) that ends after the new one starts
            let mut ranges: Vec<&(String, PartBound)> =
                sibs.iter().filter(|(_, b)| matches!(b, PartBound::Range(..))).collect();
            let mut failed = None;
            ranges.sort_by(|(_, a), (_, b)| match (a, b) {
                (PartBound::Range(x, _), PartBound::Range(y, _)) => cmp_edges(x, y).unwrap_or_else(|e| {
                    failed = Some(e);
                    Ordering::Equal
                }),
                _ => Ordering::Equal,
            });
            if let Some(e) = failed {
                return Err(e);
            }
            for (n, b) in ranges {
                if let PartBound::Range(_, to) = b
                    && cmp_edges(to, f)? == Ordering::Greater
                {
                    if overlaps(new, b)? {
                        return overlap(n);
                    }
                    break;
                }
            }
        }
        PartBound::List(vals) => {
            for val in vals {
                for (n, b) in &sibs {
                    if overlaps(&PartBound::List(vec![val.clone()]), b)? {
                        return overlap(n);
                    }
                }
            }
        }
        PartBound::Hash(m, r) => {
            let mut hashes: Vec<(u32, u32, &str)> = sibs
                .iter()
                .filter_map(|(n, b)| if let PartBound::Hash(m, r) = b { Some((*m, *r, n.as_str())) } else { None })
                .collect();
            hashes.sort();
            if !hashes.is_empty() {
                // every modulus must be a factor of the next larger one
                let at = hashes.iter().rposition(|(hm, hr, _)| (*hm, *hr) <= (*m, *r));
                let mut bad = None;
                match at {
                    None => {
                        let (nm, _, nn) = hashes[0];
                        if nm % m != 0 {
                            bad = Some(format!(
                                "The new modulus {m} is not a factor of {nm}, the modulus of existing partition \"{}\".",
                                short(nn)
                            ));
                        }
                    }
                    Some(i) => {
                        let (pm, _, pn) = hashes[i];
                        if m % pm != 0 {
                            bad = Some(format!(
                                "The new modulus {m} is not divisible by {pm}, the modulus of existing partition \"{}\".",
                                short(pn)
                            ));
                        } else if let Some((nm, _, nn)) = hashes.get(i + 1)
                            && nm % m != 0
                        {
                            bad = Some(format!(
                                "The new modulus {m} is not a factor of {nm}, the modulus of existing partition \"{}\".",
                                short(nn)
                            ));
                        }
                    }
                }
                if let Some(d) = bad {
                    return err_detail(
                        "42P17",
                        "every hash partition modulus must be a factor of the next larger modulus",
                        d,
                    );
                }
                let greatest = hashes.iter().map(|h| h.0).max().unwrap_or(*m).max(*m);
                let mut rem = *r;
                while rem < greatest {
                    if let Some((_, _, n)) = hashes.iter().find(|(hm, hr, _)| rem % hm == *hr) {
                        return overlap(n);
                    }
                    rem += m;
                }
            }
        }
    }
    Ok(())
}

/// Every table below `t` with the table it is a child of, depth first: what DROP ... CASCADE takes.
pub(super) fn dependents_tree(v: &View, t: &Table) -> Result<Vec<(String, String)>> {
    fn go(v: &View, t: &Table, out: &mut Vec<(String, String)>) -> Result<()> {
        for c in kids_in_order(v, t)? {
            if !out.iter().any(|(d, _)| *d == c.name) {
                out.push((c.name.clone(), t.name.clone()));
                go(v, &c, out)?;
            }
        }
        Ok(())
    }
    let mut out = vec![];
    go(v, t, &mut out)?;
    Ok(out)
}

/// 2BP01: dropping `name` without CASCADE, when the tables `below` (with their parents) inherit from it.
pub(super) fn needs_cascade(v: &View, name: &str, below: &[&(String, String)]) -> SqlError {
    let noun = |n: &str| super::foreign::rel_noun(v, n);
    let detail: Vec<String> = below
        .iter()
        .map(|(d, p)| format!("{} {} depends on {} {}", noun(d), rel_part(d), noun(p), rel_part(p)))
        .collect();
    SqlError {
        code: "2BP01",
        message: format!("cannot drop {} {} because other objects depend on it", noun(name), rel_part(name)),
        info: Some(Box::new(Violation {
            detail: Some(detail.join("\n")),
            hint: Some("Use DROP ... CASCADE to drop the dependent objects too.".into()),
            ..Violation::default()
        })),
    }
}

/// The marker rows of table `t` to delete when it goes: those that make it a child of its
/// parents, and those that make others its children.
pub(super) fn unlink_writes(v: &View, t: &Table) -> Result<Vec<(String, Option<String>)>> {
    let mut w: Vec<(String, Option<String>)> = vec![];
    for p in t.hier.all_parents() {
        w.push((inh_key(p, &t.name), None));
    }
    for r in v.prefix(&format!("{INH}/{}/", t.name))? {
        w.push((r?.0, None));
    }
    Ok(w)
}

/// What a new partition or child writes to the world: the marker row that makes it a child of
/// `parent`, and the parent's schema row when it is to know it has children.
pub(super) fn link_writes(v: &View, parent: &Table, child: &str) -> Result<Vec<(String, Option<String>)>> {
    let prefix = format!("{INH}/{}/", parent.name);
    let mut next = 1u64;
    for r in v.prefix(&prefix)? {
        next = next.max(r?.1.parse::<u64>().unwrap_or(0) + 1);
    }
    let mut w = vec![(inh_key(&parent.name, child), Some(next.to_string()))];
    if !parent.hier.kids && parent.hier.part.is_none() {
        let mut p = parent.clone();
        p.hier.kids = true;
        w.push((schema_key(&parent.name), Some(schema_json(&p))));
    }
    Ok(w)
}

fn no_relation<T>(name: &str) -> Result<T> {
    err(NO_TABLE, format!("relation \"{name}\" does not exist"))
}

/// Every row of `t`, as its values.
fn each_row(v: &View, t: &Table, mut f: impl FnMut(&[Value]) -> Result<bool>) -> Result<()> {
    scan(v, &mut t.clone(), Access::All, None, |_, _, vals| f(vals))
}

/// Refuses a new (or attached) partition's bound where the default partition of `parent` holds a
/// row it would take (23514): the rows would have to move.
pub(super) fn default_holds_none(v: &View, parent: &Table, name: &str, new: &PartBound) -> Result<()> {
    let Some(key) = &parent.hier.part else { return Ok(()) };
    if *new == PartBound::Default {
        return Ok(());
    }
    let types = key.types(parent)?;
    let compiled = key.compile(parent)?;
    let new = new.typed(&types)?;
    for k in kids_in_order(v, parent)? {
        if k.name == name || !matches!(&k.hier.of, Some((_, PartBound::Default))) {
            continue;
        }
        let mut tables = vec![];
        if k.hier.part.is_none() {
            tables.push(k.clone());
        }
        tables.extend(descendants(v, &k)?.into_iter().filter(|d| d.hier.part.is_none()));
        for d in tables {
            let mut found = false;
            each_row(v, &d, |vals| {
                let kv = row_key_values(parent, &compiled, &to_layout(vals, &d, parent))?;
                found = new.holds(&kv, &types)?;
                Ok(!found)
            })?;
            if found {
                return err(
                    "23514",
                    format!(
                        "updated partition constraint for default partition \"{}\" would be violated by some row",
                        rel_part(&k.name)
                    ),
                );
            }
        }
    }
    Ok(())
}

/// Refuses a primary key or UNIQUE constraint on a partitioned table that doesn't hold all of
/// its partition key (a row in another partition couldn't be seen to repeat it), as Postgres does.
pub(super) fn check_covers(t: &Table, cols: &[u32], what: &str) -> Result<()> {
    let Some(key) = &t.hier.part else { return Ok(()) };
    if key.parts.iter().any(|p| matches!(p, KeyPart::Expr(_))) {
        return err_detail(
            UNSUPPORTED,
            format!("unsupported {what} constraint with partition key definition"),
            format!("{what} constraints cannot be used when partition keys include expressions."),
        );
    }
    for p in &key.parts {
        if let KeyPart::Col(id) = p
            && !cols.contains(id)
        {
            let name = t.cols.iter().find(|c| c.id == *id).map_or("?", |c| c.name.as_str());
            return err_detail(
                UNSUPPORTED,
                "unique constraint on partitioned table must include all partitioning columns",
                format!(
                    "{what} constraint on table \"{}\" lacks column \"{name}\" which is part of the partition key.",
                    rel_part(&t.name)
                ),
            );
        }
    }
    Ok(())
}

/// `check_covers` for every key and unique constraint of `t`.
pub(super) fn check_all_cover(t: &Table) -> Result<()> {
    if t.hier.part.is_none() {
        return Ok(());
    }
    if t.has_key() {
        let ids: Vec<u32> = t.key_cols().iter().map(|&i| t.cols[i].id).collect();
        check_covers(t, &ids, "PRIMARY KEY")?;
    }
    for u in &t.uniques {
        check_covers(t, &u.cols, "UNIQUE")?;
    }
    Ok(())
}

impl Exec<'_> {
    /// CREATE TABLE's part for a table that inherits or is a partition: loads the parents (refusing
    /// what can't be one) and gives `t` the columns that come of theirs and its own (`overrides`:
    /// what a partition's column list says about its columns). A partition with no primary key of its
    /// own takes its parent's (`key`). The partition's bound is worked out here and set in `t`.
    pub(super) fn hier_parents(
        &self,
        view: &View,
        t: &mut Table,
        h: &CreateHier,
        overrides: &[ColDef],
        key: &mut Vec<String>,
    ) -> Result<Vec<Table>> {
        let mut parents: Vec<Table> = vec![];
        if h.by.is_some() && !h.inherits.is_empty() {
            return err("42P17", "cannot create partitioned table as inheritance child");
        }
        let partition = h.of.as_ref();
        if let Some((p, _)) = partition {
            let parent =
                load(view, p)?.filter(|x| x.schema && x.matview.is_none()).map_or_else(|| no_relation(p), Ok)?;
            if parent.hier.part.is_none() {
                return err("42P17", format!("\"{}\" is not partitioned", rel_part(p)));
            }
            parents.push(parent);
        }
        for n in &h.inherits {
            let p = load(view, n)?.filter(|x| x.schema && x.matview.is_none()).map_or_else(|| no_relation(n), Ok)?;
            if p.hier.part.is_some() {
                return err("42809", format!("cannot inherit from partitioned table \"{}\"", rel_part(n)));
            }
            if p.mem.is_some() {
                return err("42809", format!("cannot inherit from temporary relation \"{}\"", rel_part(n)));
            }
            if p.hier.of.is_some() {
                return err("42809", format!("cannot inherit from partition \"{}\"", rel_part(n)));
            }
            if parents.iter().any(|x| x.name == p.name) {
                return err("42P07", format!("relation \"{}\" would be inherited from more than once", rel_part(n)));
            }
            parents.push(p);
        }
        if parents.is_empty() {
            return Ok(parents);
        }
        let local: Vec<Col> = match partition {
            Some(_) => {
                let parent = &parents[0];
                let mut out = vec![];
                for d in overrides {
                    let Some(pc) = parent.cols.iter().find(|c| c.name == d.name) else {
                        return err(NO_COLUMN, format!("column \"{}\" does not exist", d.name));
                    };
                    let mut c = pc.clone();
                    c.not_null |= d.not_null;
                    if let Some(e) = &d.default {
                        c.default = Some(self.default_value(e, &d.name, pc.ty)?);
                    }
                    // `(b PRIMARY KEY)`: the partition's own key (as PRIMARY KEY (b) after its columns)
                    if d.primary {
                        if !key.is_empty() {
                            return err(
                                "42P16",
                                format!("multiple primary keys for table \"{}\" are not allowed", rel_part(&t.name)),
                            );
                        }
                        *key = vec![d.name.clone()];
                    }
                    out.push(c);
                }
                out
            }
            None => std::mem::take(&mut t.cols),
        };
        t.cols = merge_columns(&t.name, &parents, local, partition.is_none())?;
        t.next_id = t.cols.len() as u32;
        t.hier.parents = h.inherits.clone();
        if let Some((_, spec)) = partition {
            let parent = &parents[0];
            for c in &mut t.cols {
                c.local = false;
            }
            t.hier.of = Some((parent.name.clone(), self.make_bound(parent, spec)?));
            if parent.has_key() {
                // (a key of its own is the parent's, attached to it, or one too many)
                let up: Vec<String> = parent.key_cols().iter().map(|&i| parent.cols[i].name.clone()).collect();
                if key.is_empty() {
                    *key = up;
                } else if *key != up {
                    return err(
                        "42P16",
                        format!("multiple primary keys for table \"{}\" are not allowed", rel_part(&t.name)),
                    );
                }
            }
        }
        Ok(parents)
    }

    /// What CREATE TABLE's closure does for a partition or a child, with the batch's view `v`: the
    /// partition's bound is checked against the others (a default partition that holds rows it would
    /// take is refused), its parent's constraints are copied to it, and the rows that make it a
    /// child are returned.
    pub(super) fn hier_link(
        &self,
        v: &View,
        t: &mut Table,
        parents: &[Table],
    ) -> Result<Vec<(String, Option<String>)>> {
        let mut writes = vec![];
        if let Some((p, bound)) = t.hier.of.clone() {
            let parent = need_table(v, &p)?;
            check_bound(v, &parent, &t.name, &bound)?;
            default_holds_none(v, &parent, &t.name, &bound)?;
            clone_constraints(t, &parent, false, &index_names(v)?, v)?;
            writes.extend(link_writes(v, &parent, &t.name)?);
        } else if !parents.is_empty() {
            for p in parents {
                let fresh = need_table(v, &p.name)?;
                writes.extend(link_writes(v, &fresh, &t.name)?);
            }
            inherit_checks(t, parents)?;
        }
        Ok(writes)
    }
}

// ---------------------------------------------------------------- reading

/// The oid pg_class gives each of these relations (worked out from the names, see `catalog::Named`; 0 for a name
/// that isn't a relation of the world).
pub(super) fn oids(v: &View, names: &[&str]) -> Result<Vec<i64>> {
    let mut all = relation_names(v.core, &v.branch)?;
    let existing = all.clone();
    // (the counters of serial columns are relations too, in name order with the rest: see `tables`)
    for t in schemas(v)? {
        for c in t.cols.iter().filter(|c| c.auto == Some(Auto::Serial)) {
            if c.seq.as_deref().is_some_and(|s| !s.contains('.') || named_sequence(v, s)) {
                continue;
            }
            if c.seq.as_deref().is_some_and(|s| foreign_counter(&existing, &t.name, s).is_some()) {
                continue;
            }
            all.push(implicit_sequence_name(&t.name, &c.name));
        }
    }
    all.sort();
    all.dedup();
    let oids = super::catalog::rel_oids(&all);
    Ok(names.iter().map(|n| all.binary_search_by(|x| x.as_str().cmp(n)).map_or(0, |i| oids[i])).collect())
}

/// Whether the query reads a column called `tableoid` in its own expressions.
pub(super) fn mentions_tableoid(q: &Select) -> bool {
    let mut found = false;
    for e in select_exprs(q) {
        walk(e, &mut |x| found |= matches!(x, Expr::Col(_, n) if n == "tableoid"));
    }
    found
}

/// What `expand_source` made of a source.
pub(super) enum Expanded {
    /// nothing to do with hierarchies
    Skip,
    /// a table that holds its own rows and has no partitions or children
    Plain,
    /// replaced by the rows of the tables it is made of
    Hier,
}

/// A table in FROM that is partitioned or has children is read as all of them: a subquery that
/// puts together the rows of each table that holds some (UNION ALL), its columns the table's and,
/// when the query reads `tableoid` (`mention`), one more that says which table each row is in.
pub(super) fn expand_source(v: &View, src: &mut Source, past: Option<&View>, mention: bool) -> Result<Expanded> {
    if crate::reserved_table(&src.table) {
        return Ok(Expanded::Skip);
    }
    let seen = past.unwrap_or(v);
    let Some(t) = load_src(seen, src)?.filter(|t| t.schema) else { return Ok(Expanded::Skip) };
    let own = |e: Expanded| if mention { e } else { Expanded::Skip };
    if src.only || !t.hier.is_parent() {
        return Ok(own(Expanded::Plain));
    }
    let rows = row_tables(seen, &t)?;
    if rows.is_empty() || (rows.len() == 1 && rows[0].name == t.name) {
        return Ok(own(Expanded::Plain));
    }
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    let oids = if mention { oids(v, &names)? } else { vec![] };
    let mut arms: Vec<Select> = vec![];
    for (i, r) in rows.iter().enumerate() {
        let mut items: Vec<Item> =
            t.cols.iter().map(|c| Item::Expr(Expr::Col(None, c.name.clone()), Some(c.name.clone()))).collect();
        if mention {
            items.push(Item::Expr(oid_lit(oids[i]), Some("tableoid".into())));
        }
        arms.push(Select {
            set: vec![],
            distinct: false,
            distinct_on: vec![],
            items,
            from: Some(Source {
                table: r.name.clone(),
                alias: rel_part(&r.name).to_string(),
                sub: None,
                names: vec![],
                rec: None,
                working: false,
                at: src.at.clone(),
                lateral: false,
                srf: None,
                cte: 0,
                only: true,
                hier: false,
                policed: true,
                bound: src.bound,
            }),
            joins: vec![],
            filter: None,
            group: vec![],
            having: None,
            order: vec![],
            limit: None,
            offset: None,
            locking: vec![],
        });
    }
    let mut first = arms.remove(0);
    first.set = arms.into_iter().map(|a| (SetOp::Union, true, Box::new(a))).collect();
    src.sub = Some(Box::new(first));
    src.at = None;
    src.hier = mention;
    Ok(Expanded::Hier)
}

/// `q`'s reads of `tableoid` (`t.tableoid`, or bare when the query reads one table) as the oid of
/// the plain table they are of, for each (alias, table) of `plain`.
pub(super) fn plain_tableoid(v: &View, q: &mut Select, plain: &[(String, String)]) -> Result<bool> {
    let names: Vec<&str> = plain.iter().map(|(_, t)| t.as_str()).collect();
    let oids = oids(v, &names)?;
    let lone = q.from.is_some() && q.joins.is_empty() && plain.len() == 1;
    let by_alias: HashMap<&str, i64> = plain.iter().map(|(a, _)| a.as_str()).zip(oids.iter().copied()).collect();
    let f = |x: &Expr| match x {
        Expr::Col(Some(a), n) if n == "tableoid" => by_alias.get(a.as_str()).map(|o| oid_lit(*o)),
        Expr::Col(None, n) if n == "tableoid" && lone => Some(oid_lit(oids[0])),
        _ => None,
    };
    let mut changed = false;
    for e in own_exprs_mut(q) {
        let new = rewrite(e, &f);
        changed |= format!("{new:?}") != format!("{e:?}");
        *e = new;
    }
    Ok(changed)
}

/// Every expression of `q` itself (not of its set operations' other parts).
pub(super) fn own_exprs_mut(q: &mut Select) -> Vec<&mut Expr> {
    let items = q.items.iter_mut().filter_map(|i| if let Item::Expr(e, _) = i { Some(e) } else { None });
    items
        .chain(q.joins.iter_mut().filter_map(|j| j.on.as_mut()))
        .chain(q.filter.iter_mut())
        .chain(q.group.iter_mut())
        .chain(q.having.iter_mut())
        .chain(q.distinct_on.iter_mut())
        .chain(q.order.iter_mut().map(|(e, _)| e))
        .collect()
}

// ---------------------------------------------------------------- ALTER TABLE

/// How an ALTER TABLE change reaches a table: the one the statement names (`No`), a table below
/// it, getting what its parent got (`Parent`), or a table below it that is left to keep what it
/// has of the change's own (`Only`: ALTER TABLE ONLY, and NO INHERIT).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Inheriting {
    #[default]
    No,
    Parent,
    Only,
}

pub(super) type Writes = Vec<(String, Option<String>)>;

/// The writes that make `t`, changed in its schema, what the table is: its schema row, the entries
/// of its indexes and constraints made again from its rows, and its constraints held to them.
fn schema_writes(v: &View, t: &Table) -> Result<Writes> {
    let mut writes = vec![(schema_key(&t.name), Some(schema_json(t)))];
    writes.extend(index_rows(v, &t.name)?.into_iter().map(|k| (k, None)));
    let keys: Vec<String> =
        v.prefix(&format!("{}/", t.name))?.map(|r| r.map(|(k, _)| k)).collect::<crate::Result<_>>()?;
    enforce(v, &mut writes, &keys, false)?;
    Ok(writes)
}

/// 42804: what a table lacks, or has too much of, to be a partition or a child.
fn mismatch<T>(message: String) -> Result<T> {
    err("42804", message)
}

/// The name `kid`, a table below `parent`, has for the constraint or index `name` of `parent`
/// (None: it has none).
fn kid_constraint(parent: &Table, kid: &Table, name: &str) -> Option<String> {
    if kid.checks.iter().any(|c| c.name == name && c.inh > 0) {
        return Some(name.to_string());
    }
    let given = |n: &str| kid.hier.inherited.iter().any(|x| x == n);
    if parent.has_key() && name == parent.pkey_name() {
        return kid.hier.inherited.iter().any(|x| x == "*pkey").then(|| kid.pkey_name());
    }
    // (either may have been renamed since: `up` is the parent's name for it then)
    if parent.fks.iter().any(|f| f.name == name) {
        return kid
            .fks
            .iter()
            .find(|k| given(&k.name) && k.up.as_deref().unwrap_or(&k.name) == name)
            .map(|k| k.name.clone());
    }
    if let Some(u) = parent.uniques.iter().find(|u| u.name == name) {
        let mapped = map_ids(&u.cols, parent, kid)?;
        return kid.uniques.iter().find(|k| given(&k.name) && k.cols == mapped).map(|k| k.name.clone());
    }
    let x = parent.indexes.iter().find(|x| x.name == name)?;
    let mapped = map_ids(&x.cols, parent, kid)?;
    kid.indexes
        .iter()
        .find(|k| {
            given(&k.name)
                && k.cols == mapped
                && k.exprs == x.exprs
                && k.filter == x.filter
                && (0..mapped.len()).all(|n| k.coll(n) == x.coll(n))
        })
        .map(|k| k.name.clone())
}

impl Exec<'_> {
    /// ALTER TABLE's writes for a table that is, or may be, in a hierarchy: ATTACH, DETACH and
    /// INHERIT, and for a table with partitions or children, each change on them too (unless ONLY).
    pub(super) fn alter_hier(&self, v: &View, table: &str, changes: &[Alter], only: bool) -> Result<Writes> {
        let Some(t) = load(v, table)?.filter(|t| t.schema && t.matview.is_none()) else {
            return self.alter_writes(v, table, changes);
        };
        if changes.iter().any(|c| matches!(c, Alter::Attach(..) | Alter::Detach(..) | Alter::Inherit(..))) {
            let [one] = changes else {
                return err(UNSUPPORTED, "ATTACH, DETACH and INHERIT can't be combined with other changes yet");
            };
            return match one {
                Alter::Attach(child, spec) => self.attach_partition(v, &t, child, spec),
                Alter::Detach(child, how) => self.detach_partition(v, &t, child, *how),
                Alter::Inherit(parent, true) => self.inherit_table(v, &t, parent),
                Alter::Inherit(parent, false) => self.uninherit_table(v, &t, parent),
                _ => unreachable!(),
            };
        }
        // a table that is renamed or moved to another schema keeps its place in its hierarchy
        let moved_to = changes.iter().find_map(|c| match c {
            Alter::RenameTable(to) => Some(to.clone()),
            Alter::SetSchema(s) => Some(qualify(s, rel_part(table))),
            _ => None,
        });
        if let Some(to) =
            moved_to.filter(|to| *to != t.name && (t.hier.is_parent() || !t.hier.all_parents().is_empty()))
        {
            let mut writes = self.alter_writes(v, table, changes)?;
            writes.extend(rename_links(v, &t, &to)?);
            return Ok(writes);
        }
        // a foreign key of a partitioned table is its partitions', each checked for its rows: not ONLY on
        // it, nor NOT VALID (as Postgres's ATAddForeignKeyConstraint refuses them)
        if t.hier.part.is_some() {
            for c in changes {
                let Alter::AddConstraint(ConstraintDef::Foreign { table: pk, not_valid, .. }) = c else { continue };
                let message = |what: &str| {
                    format!(
                        "{what} foreign key on partitioned table \"{}\" referencing relation \"{}\"",
                        rel_part(&t.name),
                        rel_part(pk)
                    )
                };
                if only {
                    return err("42809", message("cannot use ONLY for"));
                }
                if *not_valid {
                    return Err(SqlError {
                        code: "42809",
                        message: message("cannot add NOT VALID"),
                        info: Some(Box::new(Violation {
                            detail: Some("This feature is not yet supported on partitioned tables.".into()),
                            ..Violation::default()
                        })),
                    });
                }
            }
        }
        if !t.hier.is_parent() {
            return self.alter_writes(v, table, changes);
        }
        let direct = child_names(v, &t.name)?;
        let parted = t.hier.part.is_some();
        // a partitioned table's foreign key is its partitions' too, and holds for their rows from the start (as
        // Postgres's ATAddForeignKeyConstraint has it)
        for c in changes.iter().filter(|_| parted) {
            let Alter::AddConstraint(ConstraintDef::Foreign { table: pk, not_valid, .. }) = c else { continue };
            if only {
                return err(
                    "42809",
                    format!(
                        "cannot use ONLY for foreign key on partitioned table \"{}\" referencing relation \"{}\"",
                        rel_part(&t.name),
                        rel_part(pk)
                    ),
                );
            }
            if *not_valid {
                return err_detail(
                    "42809",
                    format!(
                        "cannot add NOT VALID foreign key on partitioned table \"{}\" referencing relation \"{}\"",
                        rel_part(&t.name),
                        rel_part(pk)
                    ),
                    "This feature is not yet supported on partitioned tables.",
                );
            }
        }
        // (a partitioned table holds no rows: a constraint only on it would hold for none)
        if parted
            && changes.iter().any(|c| matches!(c, Alter::AddConstraint(ConstraintDef::Check { no_inherit: true, .. })))
        {
            return err(
                "42P16",
                format!("cannot add NO INHERIT constraint to partitioned table \"{}\"", rel_part(&t.name)),
            );
        }
        // ONLY, on a table that has children or partitions: what has to reach them too
        if only && !direct.is_empty() {
            let only_hint = |message: String, detail: Option<String>| -> Result<Writes> {
                Err(SqlError {
                    code: "42P16",
                    message,
                    info: Some(Box::new(Violation {
                        detail,
                        hint: Some("Do not specify the ONLY keyword.".into()),
                        ..Violation::default()
                    })),
                })
            };
            for c in changes {
                match c {
                    Alter::DropColumn(n, ..)
                        if parted
                            && t.cols.iter().any(|x| {
                                x.name == *n
                                    && x.inh == 0
                                    && !t.hier.part.as_ref().is_some_and(|k| k.columns(&t).contains(&x.id))
                            }) =>
                    {
                        return only_hint(
                            "cannot drop column from only the partitioned table when partitions exist".into(),
                            None,
                        );
                    }
                    Alter::NotNull(_, false) if parted => {
                        return only_hint(
                            "cannot remove constraint from only the partitioned table when partitions exist".into(),
                            None,
                        );
                    }
                    Alter::DropConstraint(n, ..)
                        if parted && t.checks.iter().any(|x| x.name == *n && !x.no_inherit) =>
                    {
                        return only_hint(
                            "cannot remove constraint from only the partitioned table when partitions exist".into(),
                            None,
                        );
                    }
                    // (SET NOT NULL on only a partitioned table: each partition has to be NOT NULL already)
                    Alter::NotNull(n, true) if parted && t.cols.iter().any(|x| x.name == *n) => {
                        for k in &direct {
                            let Some(kt) = load(v, k)? else { continue };
                            if let Some(i) = kt.cols.iter().position(|x| x.name == *n)
                                && !(kt.cols[i].not_null || kt.in_key(i))
                            {
                                return only_hint(
                                    "constraint must be added to child tables too".into(),
                                    Some(format!(
                                        "Column \"{n}\" of relation \"{}\" is not already NOT NULL.",
                                        rel_part(k)
                                    )),
                                );
                            }
                        }
                    }
                    Alter::AddColumn(d, quiet) if !(*quiet && t.cols.iter().any(|x| x.name == d.name)) => {
                        return err("42P16", "column must be added to child tables too");
                    }
                    Alter::SetType(n, ..) => {
                        return err(
                            "42P16",
                            format!("type of inherited column \"{n}\" must be changed in child tables too"),
                        );
                    }
                    Alter::RenameColumn(n, _) => {
                        return err("42P16", format!("inherited column \"{n}\" must be renamed in child tables too"));
                    }
                    Alter::RenameConstraint(n, _) if t.checks.iter().any(|x| x.name == *n && !x.no_inherit) => {
                        return err(
                            "42P16",
                            format!("inherited constraint \"{n}\" must be renamed in child tables too"),
                        );
                    }
                    Alter::AddConstraint(ConstraintDef::Check { no_inherit: false, .. }) => {
                        return err("42P16", "constraint must be added to child tables too");
                    }
                    _ => {}
                }
            }
        }
        let kids = descendants(v, &t)?;
        let mut writes = self.alter_writes(v, table, changes)?;
        // a column a table below has from a parent outside this hierarchy too can't change its type or
        // name here: that parent's would differ (Postgres compares attinhcount with the parents in the tree)
        let in_tree: HashSet<&str> = kids.iter().map(|k| k.name.as_str()).chain([t.name.as_str()]).collect();
        let parents_in_tree = |kid: &Table| kid.hier.all_parents().into_iter().filter(|p| in_tree.contains(p)).count();
        if !only {
            for kid in &kids {
                let parents = parents_in_tree(kid);
                for c in changes {
                    let (n, rename) = match c {
                        Alter::SetType(n, ..) => (n, false),
                        Alter::RenameColumn(n, _) => (n, true),
                        _ => continue,
                    };
                    if kid.cols.iter().any(|x| x.name == *n && x.inh as usize > parents) {
                        return err(
                            "42P16",
                            if rename {
                                format!("cannot rename inherited column \"{n}\"")
                            } else {
                                format!("cannot alter inherited column \"{n}\" of relation \"{}\"", rel_part(&kid.name))
                            },
                        );
                    }
                }
            }
        }
        let named = named_checks(&t, changes, &writes)?;
        // ADD COLUMN: Postgres goes down from each table that adds the column, not from one that merges it
        // with its own; the tables it reaches, and by how many parents (by column, then table)
        let mut col_paths: HashMap<String, HashMap<String, u32>> = HashMap::new();
        for c in named.iter().filter(|_| !only) {
            let Alter::AddColumn(d, _) = c else { continue };
            let mut adds: HashSet<String> = HashSet::from([t.name.clone()]);
            let mut paths: HashMap<String, u32> = HashMap::new();
            // (to a fixpoint: a table may come before one of its parents in `kids`)
            loop {
                let mut changed = false;
                for kid in &kids {
                    let n = kid.hier.all_parents().into_iter().filter(|p| adds.contains(*p)).count() as u32;
                    if n > 0 && !kid.cols.iter().any(|x| x.name == d.name) && adds.insert(kid.name.clone()) {
                        changed = true;
                    }
                    if paths.insert(kid.name.clone(), n) != Some(n) {
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
            col_paths.insert(d.name.clone(), paths);
        }
        let was = self.inheriting.get();
        self.fk_copies.borrow_mut().clear();
        for kid in &kids {
            let how = match (only, direct.contains(&kid.name)) {
                (false, _) => Inheriting::Parent,
                (true, true) => Inheriting::Only,
                (true, false) => continue,
            };
            // (a column reaches no table below one that had it already)
            let reaches = |col: &str| col_paths.get(col).and_then(|m| m.get(&kid.name)).is_none_or(|&n| n > 0);
            let ch: Vec<Alter> = recursed(&t, &kids, kid, &named, only)
                .into_iter()
                .filter(|c| !matches!(c, Alter::AddColumn(d, _) if !reaches(&d.name)))
                .collect();
            if ch.is_empty() {
                continue;
            }
            self.inheriting.set(how);
            // (Postgres reaches a table by each of its parents in the tree: what is added comes as many times)
            {
                let mut paths = self.paths.borrow_mut();
                paths.clear();
                if how == Inheriting::Parent {
                    paths.insert(String::new(), parents_in_tree(kid) as u32);
                    for (col, m) in &col_paths {
                        paths.insert(col.clone(), m.get(&kid.name).copied().unwrap_or(1));
                    }
                }
            }
            let w = self.alter_writes(v, &kid.name, &ch);
            self.inheriting.set(was);
            self.paths.borrow_mut().clear();
            writes.extend(w?);
        }
        // a partition's copy of a foreign key renamed keeps its name, and knows it by the new one
        for c in changes {
            let Alter::RenameConstraint(a, b) = c else { continue };
            if !t.fks.iter().any(|f| f.name == *a) {
                continue;
            }
            for kid in kids.iter().filter(|k| k.hier.of.as_ref().is_some_and(|(p, _)| *p == t.name)) {
                let Some(n) = kid_constraint(&t, kid, a) else { continue };
                // (as the statement's other changes left it)
                let key = schema_key(&kid.name);
                let mut kid = match writes.iter().rev().find(|(k, _)| *k == key) {
                    Some((_, Some(s))) => {
                        let mut x = parse_schema(&kid.name, s)?;
                        x.name.clone_from(&kid.name);
                        for c in &mut x.cols {
                            c.table.clone_from(&kid.name);
                        }
                        x
                    }
                    _ => kid.clone(),
                };
                for f in kid.fks.iter_mut().filter(|f| f.name == n) {
                    f.up = (*b != n).then(|| b.clone());
                }
                writes.push((key, Some(schema_json(&kid))));
            }
        }
        Ok(writes)
    }

    /// ALTER TABLE parent ATTACH PARTITION child FOR VALUES ...: the table becomes a partition,
    /// once its columns and constraints are the parent's and its rows are the bound's.
    fn attach_partition(&self, v: &View, parent: &Table, child: &str, spec: &BoundSpec) -> Result<Writes> {
        let Some(key) = parent.hier.part.clone() else {
            return err("42P17", format!("table \"{}\" is not partitioned", rel_part(&parent.name)));
        };
        let mut kid =
            load(v, child)?.filter(|t| t.schema && t.matview.is_none()).map_or_else(|| no_relation(child), Ok)?;
        if kid.name == parent.name || descendants(v, &kid)?.iter().any(|d| d.name == parent.name) {
            return err_detail(
                "42P07",
                "circular inheritance not allowed",
                format!("\"{}\" is already a child of \"{}\".", rel_part(&parent.name), rel_part(&kid.name)),
            );
        }
        if kid.hier.of.is_some() {
            return err("42809", format!("\"{}\" is already a partition", rel_part(child)));
        }
        if !kid.hier.parents.is_empty() {
            return err("42809", "cannot attach inheritance child as partition");
        }
        if kid.hier.kids && kid.hier.part.is_none() {
            return err("42809", "cannot attach inheritance parent as partition");
        }
        super::foreign::attachable(parent, &kid)?;
        if let Some(x) = transition_row_trigger(&kid) {
            return err_detail(
                UNSUPPORTED,
                format!("trigger \"{x}\" prevents table \"{}\" from becoming a partition", rel_part(&kid.name)),
                "ROW triggers with transition tables are not supported on partitions.",
            );
        }
        let short = rel_part(&kid.name).to_string();
        for (i, c) in kid.cols.iter().enumerate() {
            let Some(pc) = parent.cols.iter().find(|x| x.name == c.name) else {
                return err_detail(
                    "42804",
                    format!(
                        "table \"{short}\" contains column \"{}\" not found in parent \"{}\"",
                        c.name,
                        rel_part(&parent.name)
                    ),
                    "The new partition may contain only the columns present in parent.",
                );
            };
            if !same_type(c, pc) {
                return mismatch(format!("child table \"{short}\" has different type for column \"{}\"", c.name));
            }
            if c.coll != pc.coll {
                return err(
                    "42P21",
                    format!("child table \"{short}\" has different collation for column \"{}\"", c.name),
                );
            }
            if (pc.not_null || parent.in_key(parent.cols.iter().position(|x| x.id == pc.id).unwrap_or(0)))
                && !(c.not_null || kid.in_key(i))
            {
                return mismatch(format!("column \"{}\" in child table must be marked NOT NULL", c.name));
            }
        }
        for pc in &parent.cols {
            if !kid.cols.iter().any(|c| c.name == pc.name) {
                return mismatch(format!("child table is missing column \"{}\"", pc.name));
            }
        }
        for c in parent.checks.iter().filter(|c| !c.no_inherit) {
            match kid.checks.iter().find(|x| x.name == c.name) {
                Some(x) if same_check(&x.text, &c.text) => {}
                Some(_) => {
                    return err(
                        "42710",
                        format!(
                            "constraint \"{}\" conflicts with non-inherited constraint on child table \"{short}\"",
                            c.name
                        ),
                    );
                }
                None => return mismatch(format!("child table is missing constraint \"{}\"", c.name)),
            }
        }
        let bound = self.make_bound(parent, spec)?;
        check_bound(v, parent, &kid.name, &bound)?;
        default_holds_none(v, parent, &kid.name, &bound)?;
        // (a foreign key of the parent's to the table would reference one of its own partitions: Postgres's
        // CloneFkReferencing refuses it)
        if let Some(f) = parent.fks.iter().find(|f| f.table == kid.name) {
            return err(
                UNSUPPORTED,
                format!(
                    "cannot attach table \"{}\" as a partition because it is referenced by foreign key \"{}\"",
                    rel_part(&kid.name),
                    f.name
                ),
            );
        }
        // its rows have to be the bound's
        let types = key.types(parent)?;
        let compiled = key.compile(parent)?;
        let typed = bound.typed(&types)?;
        let siblings = if bound == PartBound::Default { kids_in_order(v, parent)? } else { vec![] };
        let mut holders = vec![];
        if kid.hier.part.is_none() {
            holders.push(kid.clone());
        }
        holders.extend(descendants(v, &kid)?.into_iter().filter(|d| d.hier.part.is_none()));
        for d in holders {
            let mut bad = false;
            each_row(v, &d, |vals| {
                let kv = row_key_values(parent, &compiled, &to_layout(vals, &d, parent))?;
                bad = if bound == PartBound::Default {
                    let mut taken = false;
                    for s in &siblings {
                        if let Some((_, b)) = &s.hier.of
                            && b.typed(&types)?.holds(&kv, &types)?
                        {
                            taken = true;
                        }
                    }
                    taken
                } else {
                    !typed.holds(&kv, &types)?
                };
                Ok(!bad)
            })?;
            if bad {
                return err("23514", format!("partition constraint of relation \"{short}\" is violated by some row"));
            }
        }
        for c in &mut kid.cols {
            c.inh = 1;
            c.local = false;
        }
        kid.hier.of = Some((parent.name.clone(), bound));
        let mut writes = link_writes(v, parent, &kid.name)?;
        writes.extend(self.adopt_tree(v, &mut kid, parent, true)?);
        Ok(writes)
    }

    /// The writes that give partition `kid` what `parent` has (indexes, keys, foreign keys, triggers)
    /// and the same for the partitions below it, each now a partition of a table that has it.
    fn adopt_tree(&self, v: &View, kid: &mut Table, parent: &Table, top: bool) -> Result<Writes> {
        clone_constraints(kid, parent, true, &index_names(v)?, v)?;
        // (a foreign partition takes none of its parent's indexes)
        if kid.hier.foreign.is_some() {
            let names: Vec<String> = kid.indexes.drain(..).map(|x| x.name).collect();
            kid.hier.inherited.retain(|n| !names.contains(n));
        }
        if top {
            // (what the parent's checks counted for it, it holds as an inheritor, not as its own)
            for c in kid.checks.iter_mut().filter(|c| c.inh > 0) {
                c.local = false;
            }
        }
        let mut writes = schema_writes(v, kid)?;
        for mut sub in children(v, &kid.name)? {
            if sub.hier.of.is_some() {
                writes.extend(self.adopt_tree(v, &mut sub, kid, false)?);
            }
        }
        Ok(writes)
    }

    /// ALTER TABLE parent DETACH PARTITION child [CONCURRENTLY | FINALIZE]: the table is its own again,
    /// with what it was given kept as its own.
    fn detach_partition(&self, v: &View, parent: &Table, child: &str, how: Detach) -> Result<Writes> {
        if parent.hier.part.is_none() {
            return err("42P17", format!("table \"{}\" is not partitioned", rel_part(&parent.name)));
        }
        if how == Detach::Concurrently
            && kids_in_order(v, parent)?.iter().any(|k| matches!(&k.hier.of, Some((_, PartBound::Default))))
        {
            return err("55000", "cannot detach partitions concurrently when a default partition exists");
        }
        let mut kid = load(v, child)?.filter(|t| t.schema).map_or_else(|| no_relation(child), Ok)?;
        if kid.hier.of.as_ref().is_none_or(|(p, _)| *p != parent.name) {
            return err(
                NO_TABLE,
                format!(
                    "relation \"{}\" is not a partition of relation \"{}\"",
                    rel_part(child),
                    rel_part(&parent.name)
                ),
            );
        }
        if how == Detach::Finalize {
            return err_detail(
                "55000",
                format!("cannot complete detaching partition \"{}\"", rel_part(child)),
                "There's no pending concurrent detach.",
            );
        }
        detach_refs_hold(v, parent, &kid)?;
        kid.hier.of = None;
        for c in &mut kid.cols {
            c.inh = 0;
            c.local = true;
        }
        for c in &mut kid.checks {
            if c.inh > 0 {
                c.inh = 0;
                c.local = true;
            }
        }
        // its copies of the parent's row triggers go, and so do its partitions' (as DETACH drops cloned triggers)
        let cloned: Vec<String> =
            kid.hier.inherited.iter().filter_map(|m| m.strip_prefix("trigger:")).map(String::from).collect();
        let below = unspread_triggers(v, &kid, &cloned)?;
        kid.triggers.retain(|x| !cloned.contains(&x.name));
        kid.hier.inherited.clear();
        for f in &mut kid.fks {
            f.up = None;
        }
        let mut writes =
            vec![(inh_key(&parent.name, &kid.name), None), (schema_key(&kid.name), Some(schema_json(&kid)))];
        writes.extend(below);
        Ok(writes)
    }

    /// ALTER TABLE child INHERIT parent.
    fn inherit_table(&self, v: &View, kid: &Table, parent: &str) -> Result<Writes> {
        let p = load(v, parent)?.filter(|t| t.schema && t.matview.is_none()).map_or_else(|| no_relation(parent), Ok)?;
        if kid.hier.of.is_some() {
            return err("42809", "cannot change inheritance of a partition");
        }
        if kid.hier.part.is_some() {
            return err("42809", "cannot change inheritance of partitioned table");
        }
        if p.hier.part.is_some() {
            return err("42809", format!("cannot inherit from partitioned table \"{}\"", rel_part(parent)));
        }
        if p.hier.of.is_some() {
            return err("42809", format!("cannot inherit from partition \"{}\"", rel_part(parent)));
        }
        if p.name == kid.name || descendants(v, kid)?.iter().any(|d| d.name == p.name) {
            return err_detail(
                "42P07",
                "circular inheritance not allowed",
                format!("\"{}\" is already a child of \"{}\".", rel_part(&p.name), rel_part(&kid.name)),
            );
        }
        if kid.hier.parents.contains(&p.name) {
            return err("42P07", format!("relation \"{}\" would be inherited from more than once", rel_part(parent)));
        }
        if let Some(x) = transition_row_trigger(kid) {
            return err_detail(
                UNSUPPORTED,
                format!(
                    "trigger \"{x}\" prevents table \"{}\" from becoming an inheritance child",
                    rel_part(&kid.name)
                ),
                "ROW triggers with transition tables are not supported in inheritance hierarchies.",
            );
        }
        let mut kid = kid.clone();
        let short = rel_part(&kid.name).to_string();
        for pc in &p.cols {
            let Some(c) = kid.cols.iter_mut().find(|x| x.name == pc.name) else {
                return mismatch(format!("child table is missing column \"{}\"", pc.name));
            };
            if !same_type(c, pc) {
                return mismatch(format!("child table \"{short}\" has different type for column \"{}\"", pc.name));
            }
            if c.coll != pc.coll {
                return err(
                    "42P21",
                    format!("child table \"{short}\" has different collation for column \"{}\"", pc.name),
                );
            }
            if pc.not_null && !c.not_null {
                return mismatch(format!("column \"{}\" in child table must be marked NOT NULL", pc.name));
            }
            c.inh += 1;
        }
        for c in p.checks.iter().filter(|c| !c.no_inherit) {
            match kid.checks.iter_mut().find(|x| x.name == c.name) {
                Some(x) if same_check(&x.text, &c.text) => x.inh += 1,
                Some(_) => {
                    return err(
                        "42710",
                        format!(
                            "constraint \"{}\" conflicts with non-inherited constraint on child table \"{short}\"",
                            c.name
                        ),
                    );
                }
                None => return mismatch(format!("child table is missing constraint \"{}\"", c.name)),
            }
        }
        kid.hier.parents.push(p.name.clone());
        let mut writes = link_writes(v, &p, &kid.name)?;
        writes.push((schema_key(&kid.name), Some(schema_json(&kid))));
        Ok(writes)
    }

    /// ALTER TABLE child NO INHERIT parent.
    fn uninherit_table(&self, v: &View, kid: &Table, parent: &str) -> Result<Writes> {
        let p = load(v, parent)?.filter(|t| t.schema && t.matview.is_none()).map_or_else(|| no_relation(parent), Ok)?;
        if kid.hier.of.is_some() {
            return err("42809", "cannot change inheritance of a partition");
        }
        if !kid.hier.parents.contains(&p.name) {
            return err(
                NO_TABLE,
                format!("relation \"{}\" is not a parent of relation \"{}\"", rel_part(parent), rel_part(&kid.name)),
            );
        }
        let mut kid = kid.clone();
        kid.hier.parents.retain(|x| *x != p.name);
        for pc in &p.cols {
            if let Some(c) = kid.cols.iter_mut().find(|x| x.name == pc.name) {
                c.inh = c.inh.saturating_sub(1);
                c.local |= c.inh == 0;
            }
        }
        for pk in p.checks.iter().filter(|c| !c.no_inherit) {
            if let Some(c) = kid.checks.iter_mut().find(|x| x.name == pk.name) {
                c.inh = c.inh.saturating_sub(1);
                c.local |= c.inh == 0;
            }
        }
        Ok(vec![(inh_key(&p.name, &kid.name), None), (schema_key(&kid.name), Some(schema_json(&kid)))])
    }

    /// ALTER INDEX parent ATTACH PARTITION child: the index of a partition becomes its parent's.
    pub(super) fn attach_index(&self, parent: &str, child: &str) -> Result<Output> {
        self.db.batch_with(self.branch, |v| {
            let all = schemas(v)?;
            let owner = |n: &str| {
                all.iter().find(|t| {
                    t.indexes.iter().any(|x| x.name == n)
                        || t.uniques.iter().any(|u| u.name == n)
                        || (t.has_key() && n == t.pkey_name())
                })
            };
            let no_index = |n: &str| err::<()>("42704", format!("relation \"{n}\" does not exist")).unwrap_err();
            // (a table named for either: the parent is refused as ALTER INDEX refuses a table, the child as
            // ATTACH PARTITION's lookup refuses it)
            let table = |n: &str| all.iter().any(|t| rel_part(&t.name) == rel_part(n));
            let (Some(pt), Some(ct)) = (owner(rel_part(parent)), owner(rel_part(child))) else {
                if owner(rel_part(parent)).is_none() {
                    if table(parent) {
                        return err("42809", format!("\"{}\" is not an index", rel_part(parent)));
                    }
                    return Err(no_index(parent));
                }
                if table(child) {
                    return err("42P17", format!("\"{}\" is not an index", rel_part(child)));
                }
                return Err(no_index(child));
            };
            let (pn, cn) = (rel_part(parent), rel_part(child));
            // (55000 for an index in the wrong place, 42P17 for one whose definition differs)
            let refuse = |code: &'static str, why: &str| {
                err_detail::<(Vec<(String, Option<String>)>, ())>(
                    code,
                    format!("cannot attach index \"{cn}\" as a partition of index \"{pn}\""),
                    why.to_string(),
                )
            };
            if pt.hier.part.is_none() {
                return err_detail("42809", format!("\"{pn}\" is not a partitioned index"), "");
            }
            // attached to it already: nothing to do; another of the partition's attached to it: refused
            match kid_constraint(pt, ct, pn) {
                Some(n) if n == cn => return Ok((vec![], ())),
                Some(_) => {
                    return refuse("55000", &format!(
                        "Another index is already attached for partition \"{}\".",
                        rel_part(&ct.name)
                    ));
                }
                None => {}
            }
            if ct.hier.of.as_ref().is_none_or(|(p, _)| *p != pt.name) {
                return refuse("55000", &format!(
                    "Index \"{pn}\" is not an index on any partition of table \"{}\".",
                    rel_part(&ct.name)
                ));
            }
            let key = |t: &Table, n: &str| t.has_key() && n == t.pkey_name();
            // the definitions have to be the same
            let same = if key(pt, pn) {
                key(ct, cn) && {
                    let p: Vec<u32> = pt.key_cols().iter().map(|&i| pt.cols[i].id).collect();
                    let c: Vec<u32> = ct.key_cols().iter().map(|&i| ct.cols[i].id).collect();
                    map_ids(&p, pt, ct) == Some(c)
                }
            } else if let Some(pu) = pt.uniques.iter().find(|u| u.name == pn) {
                let backed = pt.indexes.iter().any(|x| x.name == pn);
                ct.uniques.iter().any(|u| {
                    u.name == cn
                        && u.nnd == pu.nnd
                        && map_ids(&pu.cols, pt, ct).as_ref() == Some(&u.cols)
                        && ct.indexes.iter().any(|x| x.name == cn) == backed
                })
            } else {
                let px = pt.indexes.iter().find(|x| x.name == pn);
                match (px, ct.indexes.iter().find(|x| x.name == cn)) {
                    (Some(px), Some(cx)) => {
                        map_ids(&px.cols, pt, ct).as_ref() == Some(&cx.cols)
                            && px.exprs == cx.exprs
                            && px.filter == cx.filter
                            && !ct.uniques.iter().any(|u| u.name == cn)
                    }
                    _ => false,
                }
            };
            // the parent's is a constraint's index (a primary key's, a unique constraint's) and the partition's a plain
            // unique index on the same columns: the definitions match, but the partition has no constraint for it
            let constraint_missing = !same && {
                let up: Option<Vec<u32>> = if key(pt, pn) {
                    Some(pt.key_cols().iter().map(|&i| pt.cols[i].id).collect())
                } else {
                    pt.uniques
                        .iter()
                        .find(|u| u.name == pn && !u.nnd && !pt.indexes.iter().any(|x| x.name == pn))
                        .map(|u| u.cols.clone())
                };
                up.and_then(|p| map_ids(&p, pt, ct)).is_some_and(|mapped| {
                    ct.indexes.iter().any(|x| x.name == cn && x.exprs.is_empty() && x.filter.is_none())
                        && ct.uniques.iter().any(|u| u.name == cn && !u.nnd && u.cols == mapped)
                })
            };
            if constraint_missing {
                return refuse(
                    "42P17",
                    &format!(
                    "The index \"{pn}\" belongs to a constraint in table \"{}\" but no constraint exists for index \"{cn}\".",
                    rel_part(&pt.name)
                ));
            }
            if !same {
                return refuse("42P17", "The index definitions do not match.");
            }
            let mark = if key(ct, cn) { "*pkey".to_string() } else { cn.to_string() };
            if kid_constraint(pt, ct, pn).as_deref() == Some(cn) {
                return Ok((vec![], ())); // (it is that already)
            }
            if ct.hier.inherited.contains(&mark) {
                return refuse("55000", &format!("Index \"{cn}\" is already attached to another index."));
            }
            let mut ct = ct.clone();
            ct.hier.inherited.push(mark);
            Ok((vec![(schema_key(&ct.name), Some(schema_json(&ct)))], ()))
        })?;
        Ok(Output::tag("ALTER INDEX"))
    }
}

/// The name `kid`, a table anywhere below `t` (one of `kids`), has for `t`'s constraint or index `name`:
/// a partition's, by way of the partitioned table it is a partition of (their names may differ).
fn kid_constraint_below(t: &Table, kids: &[Table], kid: &Table, name: &str) -> Option<String> {
    match kid.hier.of.as_ref().and_then(|(p, _)| kids.iter().find(|k| k.name == *p)) {
        Some(mid) => kid_constraint(mid, kid, &kid_constraint_below(t, kids, mid, name)?),
        None => kid_constraint(t, kid, name),
    }
}

/// `changes` with each CHECK written without a name given the name `t`'s own copy got (`writes`, ALTER
/// TABLE's writes for `t`): the tables below have the parent's constraint under the parent's name
/// (`ac_aa_check` on `bc` too), as Postgres recurses with the constraint once named.
fn named_checks(t: &Table, changes: &[Alter], writes: &Writes) -> Result<Vec<Alter>> {
    let defs = |c: &Alter| -> Vec<ConstraintDef> {
        match c {
            Alter::AddConstraint(d) => vec![d.clone()],
            Alter::AddColumn(d, _) => d.constraints.clone(),
            _ => vec![],
        }
    };
    let unnamed = changes.iter().flat_map(defs).any(|d| matches!(d, ConstraintDef::Check { name: None, .. }));
    let key = schema_key(&t.name);
    let new = match writes.iter().rev().find(|(k, _)| *k == key) {
        Some((_, Some(s))) if unnamed => parse_schema(&t.name, s)?,
        _ => return Ok(changes.to_vec()),
    };
    let given: Vec<String> = changes
        .iter()
        .flat_map(defs)
        .filter_map(|d| match d {
            ConstraintDef::Check { name: Some(n), .. } => Some(n),
            _ => None,
        })
        .collect();
    // (the checks the statement made, in the order it made them, less those it was told the names of)
    let mut fresh = new
        .checks
        .iter()
        .map(|c| c.name.clone())
        .filter(|n| !t.checks.iter().any(|c| c.name == *n) && !given.contains(n))
        .collect::<Vec<_>>()
        .into_iter();
    let mut out = changes.to_vec();
    for c in &mut out {
        let defs: &mut [ConstraintDef] = match c {
            Alter::AddConstraint(d) => std::slice::from_mut(d),
            Alter::AddColumn(d, _) => d.constraints.as_mut_slice(),
            _ => continue,
        };
        for d in defs {
            if let ConstraintDef::Check { name: n @ None, .. } = d {
                *n = fresh.next();
            }
        }
    }
    Ok(out)
}

/// Whether DROP COLUMN `n` of `t` reaches `kid` (one of `kids`, the tables below `t`): a child of `t` is reached,
/// one further down only if each of its parents below `t` drops the column too (one that has it as its own, or from
/// another parent as well, keeps it, and so does what is below it).
fn drops_through(t: &Table, kids: &[Table], kid: &Table, n: &str) -> bool {
    // (a partition's parent is its PARTITION OF's)
    let above: Vec<&String> = kid.hier.parents.iter().chain(kid.hier.of.as_ref().map(|(p, _)| p)).collect();
    if above.contains(&&t.name) {
        return true;
    }
    let mut parents = above.into_iter().filter_map(|p| kids.iter().find(|k| k.name == *p)).peekable();
    parents.peek().is_some()
        && parents.all(|p| {
            p.cols.iter().find(|c| c.name == n).is_some_and(|c| !c.local && c.inh <= 1) && drops_through(t, kids, p, n)
        })
}

/// The changes of `changes` that reach `kid`, a table below `t` (one of `kids`; the changes are ALTER
/// TABLE's on `t`). `only`: the table is a child of one altered ONLY, which keeps what it has of what
/// was dropped.
fn recursed(t: &Table, kids: &[Table], kid: &Table, changes: &[Alter], only: bool) -> Vec<Alter> {
    let mut out = vec![];
    for c in changes {
        match c {
            Alter::DropColumn(n, ..) if t.cols.iter().any(|x| x.name == *n) && drops_through(t, kids, kid, n) => {
                out.push(c.clone())
            }
            Alter::DropConstraint(n, _, cascade) => {
                if let Some(k) = kid_constraint_below(t, kids, kid, n) {
                    out.push(Alter::DropConstraint(k, true, *cascade));
                }
            }
            _ if only => {}
            // ENABLE / DISABLE TRIGGER: a partitioned table's row triggers' copies in its partitions too
            Alter::Trigger(name, mode) if t.hier.part.is_some() => {
                for x in t.triggers.iter().filter(|x| x.row && name.as_ref().is_none_or(|n| x.name == *n)) {
                    if kid.hier.inherited.contains(&format!("trigger:{}", x.name)) {
                        out.push(Alter::Trigger(Some(x.name.clone()), *mode));
                    }
                }
            }
            Alter::AddColumn(d, quiet) if !(*quiet && t.cols.iter().any(|x| x.name == d.name)) => out.push(c.clone()),
            Alter::AddColumn(..) => {}
            // (a key, unique or foreign key constraint a partition has of its parent's keeps its own name)
            Alter::RenameConstraint(n, _) if t.checks.iter().any(|x| x.name == *n && !x.no_inherit) => {
                out.push(c.clone());
            }
            Alter::RenameConstraint(..) => {}
            Alter::RenameColumn(..)
            | Alter::SetType(..)
            | Alter::SetDefault(..)
            | Alter::NotNull(..)
            | Alter::DropExpression(..)
            | Alter::ColOpt(..) => out.push(c.clone()),
            Alter::AddConstraint(def) => match def {
                ConstraintDef::Check { no_inherit: false, .. } => out.push(c.clone()),
                // (a partition names its own unique constraint, as it names its own key)
                ConstraintDef::Unique { cols, nnd, .. } if t.hier.part.is_some() => {
                    out.push(Alter::AddConstraint(ConstraintDef::Unique { name: None, cols: cols.clone(), nnd: *nnd }));
                }
                // (under the name the table gives it: its partitions' copies have that name too)
                ConstraintDef::Foreign { cols, .. } if t.hier.part.is_some() => {
                    let mut def = def.clone();
                    if let ConstraintDef::Foreign { name: n @ None, .. } = &mut def {
                        *n = Some(format!("{}_{}_fkey", rel_part(&t.name), cols.join("_")));
                    }
                    out.push(Alter::AddConstraint(def));
                }
                _ => {}
            },
            Alter::AddPrimaryKey(cols, _) => {
                // (a partition names its own key)
                if t.hier.part.is_some() {
                    out.push(Alter::AddPrimaryKey(cols.clone(), None));
                } else {
                    out.extend(cols.iter().map(|n| Alter::NotNull(n.clone(), true)));
                }
            }
            Alter::Validate(n) if kid.checks.iter().any(|x| x.name == *n) => out.push(c.clone()),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------- UPDATE and DELETE

/// What a statement on partitioned or inherited table `table` reaches: the table, and each table
/// below it that holds rows (the table itself, too, unless it is partitioned).
type Tree = (Table, Vec<Table>);

/// The rows a statement's tag says it changed.
fn count_of(o: &Output) -> usize {
    o.tag.rsplit(' ').next().and_then(|x| x.parse().ok()).unwrap_or(0)
}

/// What the statements of each table give together.
fn gathered(outs: Vec<Output>, verb: &str) -> Output {
    let n: usize = outs.iter().map(count_of).sum();
    let columns = outs.iter().find_map(|o| o.columns.clone());
    Output {
        typmods: vec![],
        tag: format!("{verb} {n}"),
        columns,
        rows: outs.into_iter().flat_map(|o| o.rows).collect(),
    }
}

impl Exec<'_> {
    /// The tables a statement on `table` reaches; None when that is only the table (it has no
    /// partitions or children, or the statement says ONLY).
    fn tree_of(&self, table: &str, only: bool) -> Result<Option<Tree>> {
        if only || !self.db.has_inherits() {
            return Ok(None);
        }
        let v = self.view()?;
        let Some(t) = load(&v, table)?.filter(|t| t.schema && t.hier.is_parent()) else { return Ok(None) };
        let rows = row_tables(&v, &t)?;
        if rows.len() == 1 && rows[0].name == t.name {
            return Ok(None);
        }
        Ok(Some((t, rows)))
    }

    /// `e`, which reads `parent`'s columns, as it reads `leaf`'s: its qualifier, and `tableoid`.
    fn retarget(&self, v: &View, e: &Expr, parent: &Table, leaf: &Table) -> Result<Expr> {
        let e = unalias(e, &parent.name, &leaf.name);
        let mut mentions = false;
        walk(&e, &mut |x| mentions |= matches!(x, Expr::Col(_, n) if n == "tableoid"));
        if !mentions {
            return Ok(e);
        }
        let oid = oids(v, &[leaf.name.as_str()])?[0];
        Ok(rewrite(&e, &|x| match x {
            Expr::Col(_, n) if n == "tableoid" => Some(oid_lit(oid)),
            _ => None,
        }))
    }

    /// A RETURNING list as it reads `leaf`: `*` and `t.*` are the parent's columns, not the ones the
    /// leaf has more.
    fn retarget_items(
        &self,
        v: &View,
        items: &[Item],
        parent: &Table,
        leaf: &Table,
        keep_oid: bool,
    ) -> Result<Vec<Item>> {
        let mut out = vec![];
        for it in items {
            match it {
                Item::Star => out.extend(
                    parent
                        .cols
                        .iter()
                        .map(|c| Item::Expr(Expr::Col(Some(leaf.name.clone()), c.name.clone()), Some(c.name.clone()))),
                ),
                Item::TableStar(t) if *t == parent.name || *t == leaf.name => out.extend(
                    parent
                        .cols
                        .iter()
                        .map(|c| Item::Expr(Expr::Col(Some(leaf.name.clone()), c.name.clone()), Some(c.name.clone()))),
                ),
                // (named for what it was before it was pointed at the leaf)
                // (`tableoid` of a row that moves is the table it moved to: worked out as the row is returned)
                Item::Expr(e, a) => {
                    let e2 = if keep_oid {
                        unalias(e, &parent.name, &leaf.name)
                    } else {
                        self.retarget(v, e, parent, leaf)?
                    };
                    out.push(Item::Expr(e2, a.clone().or_else(|| figure_name(e))))
                }
                it => out.push(it.clone()),
            }
        }
        Ok(out)
    }

    /// UPDATE (see `update_in`): for a table with partitions or children, one UPDATE of each table
    /// that holds rows, all or none of them taking effect; the statement triggers of the table
    /// named fire once.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn update_tree(
        &self,
        table: &str,
        only: bool,
        sets: &[(String, Expr)],
        filter: Option<&Expr>,
        returning: Option<&[Item]>,
        from: Option<&Select>,
        checks: Option<&rls::Checks>,
    ) -> Result<Output> {
        let Some((root, leaves)) = self.tree_of(table, only)? else {
            return self.update_in(table, sets, filter, returning, from, None, checks);
        };
        let set: Vec<u32> =
            sets.iter().filter_map(|(c, _)| root.cols.iter().find(|x| x.name == *c).map(|x| x.id)).collect();
        // (a row leaves its partition only for a partitioned table's UPDATE)
        let moves = root.hier.part.is_some().then_some(root.name.as_str());
        self.write(|v| {
            self.fire_statement(&root, functions::Event::Update, true, &set)?;
            let mut outs = vec![];
            let was = self.quiet.replace(true);
            // (the rows changed below are the parent's transition tables' too)
            let was_root = functions::transition_root(&root, &leaves);
            let back = |was_root| {
                self.quiet.set(was);
                functions::transition_root_back(was_root);
            };
            for leaf in &leaves {
                let made = (|| {
                    let sets: Vec<(String, Expr)> = sets
                        .iter()
                        .map(|(c, e)| Ok((c.clone(), self.retarget(v, e, &root, leaf)?)))
                        .collect::<Result<_>>()?;
                    let filter = filter.map(|f| self.retarget(v, f, &root, leaf)).transpose()?;
                    let items =
                        returning.map(|r| self.retarget_items(v, r, &root, leaf, moves.is_some())).transpose()?;
                    let join = from.map(|q| retarget_join(q, leaf));
                    // (checked by the policies of the table the statement names)
                    self.update_in(&leaf.name, &sets, filter.as_ref(), items.as_deref(), join.as_ref(), moves, checks)
                })();
                match made {
                    Ok(o) => outs.push(o),
                    Err(e) => {
                        back(was_root);
                        return Err(e);
                    }
                }
            }
            back(was_root);
            // (after them all: a row moved to a partition is not one the statement then updates again)
            let moved: Vec<_> = std::mem::take(&mut *self.moves.borrow_mut());
            let mut arrived = 0;
            for (from, rows) in moved {
                arrived += self.relocate(v, &from, moves, &rows)?;
            }
            self.fire_statement(&root, functions::Event::Update, false, &set)?;
            // a row that moved counts once it is in the partition it went to (a BEFORE INSERT trigger there may
            // keep it out)
            let mut out = gathered(outs, "UPDATE");
            out.tag = format!("UPDATE {}", count_of(&out) + arrived);
            Ok(out)
        })
    }

    /// DELETE, as `update_tree`.
    pub(super) fn delete_tree(
        &self,
        table: &str,
        only: bool,
        filter: Option<&Expr>,
        returning: Option<&[Item]>,
        from: Option<&Select>,
        checks: Option<&rls::Checks>,
    ) -> Result<Output> {
        let Some((root, leaves)) = self.tree_of(table, only)? else {
            return self.delete(table, filter, returning, from, checks);
        };
        self.write(|v| {
            self.fire_statement(&root, functions::Event::Delete, true, &[])?;
            let mut outs = vec![];
            let was = self.quiet.replace(true);
            // (the rows deleted below are the parent's transition table's too)
            let was_root = functions::transition_root(&root, &leaves);
            let back = |was_root| {
                self.quiet.set(was);
                functions::transition_root_back(was_root);
            };
            for leaf in &leaves {
                let gone = (|| {
                    let filter = filter.map(|f| self.retarget(v, f, &root, leaf)).transpose()?;
                    let items = returning.map(|r| self.retarget_items(v, r, &root, leaf, false)).transpose()?;
                    let join = from.map(|q| retarget_join(q, leaf));
                    self.delete(&leaf.name, filter.as_ref(), items.as_deref(), join.as_ref(), checks)
                })();
                match gone {
                    Ok(o) => outs.push(o),
                    Err(e) => {
                        back(was_root);
                        return Err(e);
                    }
                }
            }
            back(was_root);
            self.fire_statement(&root, functions::Event::Delete, false, &[])?;
            Ok(gathered(outs, "DELETE"))
        })
    }

    /// Rows that stopped belonging to partition `from` by an UPDATE of the partitioned table `root`
    /// are inserted there, to go to the partition they now belong to: how many arrived.
    pub(super) fn relocate(&self, v: &View, from: &Table, root: Option<&str>, rows: &[Vec<Value>]) -> Result<usize> {
        let Some(root) = root else { return Ok(0) };
        let top = need_table(v, root)?;
        // a partition that is itself partitioned keeps its own bound
        if top.hier.of.is_some() {
            for r in rows {
                if !self.partition_takes(v, &top, &to_layout(r, from, &top))? {
                    return Err(violates_partition(from, r));
                }
            }
        }
        let names: Vec<String> = from.cols.iter().map(|c| c.name.clone()).collect();
        let exprs: Vec<Vec<Expr>> = rows
            .iter()
            .map(|r| {
                r.iter()
                    .zip(&from.cols)
                    .map(|(v, c)| if c.generated.is_some() { Expr::Default } else { Expr::Lit(v.clone()) })
                    .collect()
            })
            .collect();
        let was = self.quiet.replace(true);
        let out = self.insert(root, None, Some(&names), exprs, None, None, None, Some(true), None);
        self.quiet.set(was);
        out.map(|o| count_of(&o))
    }
}

/// The select of UPDATE ... FROM or DELETE ... USING (see `joined_select`) as it reads `leaf`: the
/// target table is the leaf, under the name the statement gave it.
fn retarget_join(q: &Select, leaf: &Table) -> Select {
    let mut q = q.clone();
    if let Some(t) = &mut q.from {
        t.table = leaf.name.clone();
        t.only = true;
    }
    q
}

// ---------------------------------------------------------------- where a row goes

/// One partitioned table with its partitions loaded, for routing rows: built once for a statement.
pub(super) struct Level {
    pub(super) table: Table,
    key: Vec<KeyEval>,
    router: Router,
    pub(super) kids: Vec<Table>,
}

/// The levels a statement has looked at, by table name.
pub(super) type Routes = HashMap<String, std::rc::Rc<Level>>;

/// Values as Postgres lists them in an error's DETAIL: `(1, x, null)`.
/// A row of `cols` as Postgres shows it in a violation's DETAIL: each value as text (a regproc[] as its functions'
/// names), null as `null`.
pub(super) fn shown_row(vals: &[Value], cols: &[Col]) -> String {
    let parts: Vec<String> = vals
        .iter()
        .enumerate()
        .map(|(i, v)| match v {
            Value::Null => "null".into(),
            v => {
                let ty = cols.get(i).map(|c| c.ty);
                proc_text(ty, v.clone()).map_or_else(|_| v.to_text(), |v| v.to_text())
            }
        })
        .collect();
    format!("({})", parts.join(", "))
}

/// The DETAIL of a row of table `table` (its values `vals` of columns `cols`) that a constraint refuses, as Postgres's
/// ExecBuildSlotValueDescription makes it: `Failing row contains (1, x).`, a row of a partition or child the
/// statement wrote through its parent as the parent has the columns. A role without SELECT on the table sees only
/// the columns it may read or is writing, named (`Failing row contains (a, b) = (1, x).`), and nothing (no DETAIL)
/// when that is none of them or row security limits it on the table.
pub(super) fn failing_row(table: &str, vals: &[Value], cols: &[Col]) -> Option<String> {
    let all = |vals: &[Value], cols: &[Col]| Some(format!("Failing row contains {}.", shown_row(vals, cols)));
    let who = roles::current();
    let limited = who.as_ref().is_some_and(|w| !w.superuser);
    let Some(ctx) = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone())) else {
        return all(vals, cols);
    };
    let writing = WRITING.with_borrow(Clone::clone);
    let Ok(v) = ctx.core.view_tx(&ctx.world) else { return if limited { None } else { all(vals, cols) } };
    // the table the statement writes, when `table` is one of its partitions or children: the row as it has them
    // (by its stored name)
    let target: Option<String> = writing.as_ref().map(|w| match load(&v, &w.table) {
        Ok(Some(t)) => t.name,
        _ => w.table.clone(),
    });
    let mut root: Option<Table> = None;
    if let Some(w) = target.as_ref().filter(|w| *w != table)
        && let Ok(Some(t)) = load(&v, table)
        && !t.hier.all_parents().is_empty()
        && let Ok(up) = ancestors(&v, &t)
    {
        root = up.into_iter().find(|a| a.name == *w);
    }
    let rooted: Vec<Value>;
    let (name, vals, cols) = match &root {
        Some(r) => {
            rooted = r
                .cols
                .iter()
                .map(|c| cols.iter().position(|x| x.name == c.name).map_or(Value::Null, |i| vals[i].clone()))
                .collect();
            (r.name.as_str(), &rooted[..], &r.cols[..])
        }
        None => (table, vals, cols),
    };
    let Some(who) = who.filter(|_| limited) else { return all(vals, cols) };
    if !ctx.core.has_privileges() && !ctx.core.has_row_security() {
        return all(vals, cols);
    }
    if rls::limits(&v, name, Some(&*who)).unwrap_or(true) {
        return None;
    }
    if !ctx.core.has_privileges() {
        return all(vals, cols);
    }
    let owner = match acl::rel(&v, name) {
        Ok(Some(o)) if o.has(&who, acl::SELECT) => return all(vals, cols),
        Ok(Some(o)) => o.owner,
        Ok(None) => return all(vals, cols), // (a temporary table: its session's)
        Err(_) => return None,
    };
    // the columns it may read, and those it writes
    let written: Vec<&str> = match writing.as_ref().filter(|_| target.as_deref() == Some(name)) {
        Some(w) => {
            let first = cols.iter().filter(|c| !c.hidden).take(w.first).map(|c| c.name.as_str());
            w.cols.iter().map(String::as_str).chain(first).collect()
        }
        None => vec![],
    };
    let (mut names, mut shown) = (vec![], vec![]);
    for (i, c) in cols.iter().enumerate() {
        if c.hidden {
            continue;
        }
        let ok = written.contains(&c.name.as_str())
            || acl::column_has(&v, name, &c.name, &who, owner, acl::SELECT).unwrap_or(false);
        if ok && let Some(x) = vals.get(i) {
            names.push(c.name.clone());
            shown.push(match x {
                Value::Null => "null".into(),
                x => proc_text(Some(c.ty), x.clone()).map_or_else(|_| x.to_text(), |x| x.to_text()),
            });
        }
    }
    if names.is_empty() {
        return None;
    }
    Some(format!("Failing row contains ({}) = ({}).", names.join(", "), shown.join(", ")))
}

/// A partition's row in the DETAIL of a CHECK it fails, as its root partitioned table has the columns: Postgres
/// shows the row a statement routed there in the order the statement's table has them (`Failing row contains (20,
/// 1, 10)` for a partition whose columns are in another order).
pub(super) fn rooted_detail(by_name: &HashMap<&str, &Table>, t: &Table, vals: &[Value], mut e: SqlError) -> SqlError {
    let Some(root) = partition_chain(by_name, t).last().and_then(|n| by_name.get(n)) else { return e };
    if let Some(i) =
        e.info.as_mut().filter(|i| i.detail.as_deref().is_some_and(|d| d.starts_with("Failing row contains")))
    {
        i.detail = failing_row(&root.name, &to_layout(vals, t, root), &root.cols);
    }
    e
}

impl Exec<'_> {
    /// The partitioned table `name` with its partitions (see `Level`).
    pub(super) fn level(&self, v: &View, name: &str) -> Result<std::rc::Rc<Level>> {
        if let Some(l) = self.routes.borrow().get(name) {
            return Ok(l.clone());
        }
        let table = need_table(v, name)?;
        let Some(key) = table.hier.part.clone() else {
            return err("42P17", format!("\"{}\" is not partitioned", rel_part(name)));
        };
        let types = key.types(&table)?;
        let kids = children(v, name)?;
        let mut of = vec![];
        for k in &kids {
            let b = k.hier.of.as_ref().map(|(_, b)| b.typed(&types)).transpose()?.unwrap_or(PartBound::Default);
            of.push(Kid { bound: b });
        }
        let level = std::rc::Rc::new(Level {
            key: key.compile(&table)?,
            router: Router::new(key.strategy, of, types.clone())?,
            table,
            kids,
        });
        self.routes.borrow_mut().insert(name.to_string(), level.clone());
        Ok(level)
    }

    /// The partition (a table that holds rows) a row of partitioned table `t` goes to, and the row
    /// as its columns have it: 23514 when there is none.
    pub(super) fn route(&self, v: &View, t: &Table, vals: &[Value]) -> Result<(std::rc::Rc<Table>, Vec<Value>)> {
        let mut level = self.level(v, &t.name)?;
        let mut vals = vals.to_vec();
        loop {
            let kv = row_key_values(&level.table, &level.key, &vals)?;
            let Some(i) = level.router.find(&kv)? else {
                return Err(no_partition(&level.table, &kv));
            };
            let kid = &level.kids[i];
            vals = to_layout(&vals, &level.table, kid);
            if kid.hier.part.is_some() {
                let next = self.level(v, &kid.name)?;
                level = next;
                continue;
            }
            return Ok((std::rc::Rc::new(kid.clone()), vals));
        }
    }

    /// Whether a row of partition `t` is one its bound, and its parents' bounds, take.
    pub(super) fn partition_takes(&self, v: &View, t: &Table, vals: &[Value]) -> Result<bool> {
        let (mut cur, mut vals) = (t.clone(), vals.to_vec());
        while let Some((p, _)) = cur.hier.of.clone() {
            let level = self.level(v, &p)?;
            let pv = to_layout(&vals, &cur, &level.table);
            let kv = row_key_values(&level.table, &level.key, &pv)?;
            let mine = level.kids.iter().position(|k| k.name == cur.name);
            if mine.is_none() || level.router.find(&kv)? != mine {
                return Ok(false);
            }
            (cur, vals) = (level.table.clone(), pv);
        }
        Ok(true)
    }
}

/// 23514: no partition of `t` takes a row with this key.
fn no_partition(t: &Table, key: &[Value]) -> SqlError {
    let names: Vec<String> = match &t.hier.part {
        Some(k) => k
            .parts
            .iter()
            .map(|p| match p {
                KeyPart::Col(id) => t.cols.iter().find(|c| c.id == *id).map_or_else(String::new, |c| c.name.clone()),
                // (as Postgres writes an expression back: `(b + 0)`, `abs(a)`)
                KeyPart::Expr(e) => catalog::expr_def(e, &t.cols, true),
            })
            .collect(),
        None => vec![],
    };
    let vals: Vec<String> = key.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect();
    violation(
        "23514",
        format!("no partition of relation \"{}\" found for row", rel_part(&t.name)),
        &t.name,
        None,
        None,
        Some(format!("Partition key of the failing row contains ({}) = ({}).", names.join(", "), vals.join(", "))),
    )
}

/// 0A000: ON CONFLICT DO UPDATE would make a row one its partition doesn't take.
pub(super) fn moves_partition(t: &Table) -> SqlError {
    let mut e = violation(
        UNSUPPORTED,
        "invalid ON UPDATE specification",
        &t.name,
        None,
        None,
        Some("The result tuple would appear in a different partition than the original tuple.".into()),
    );
    if let Some(i) = &mut e.info {
        i.table = None;
        i.schema = None;
    }
    e
}

/// 23514: a row doesn't satisfy the partition constraint of the partition it is written to.
pub(super) fn violates_partition(t: &Table, vals: &[Value]) -> SqlError {
    violation(
        "23514",
        format!("new row for relation \"{}\" violates partition constraint", rel_part(&t.name)),
        &t.name,
        None,
        None,
        failing_row(&t.name, vals, &t.cols),
    )
}

/// Every index and constraint name the world's tables use (they share one namespace).
pub(super) fn index_names(v: &View) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for t in schemas(v)? {
        out.extend(t.indexes.iter().map(|x| x.name.clone()));
        out.extend(t.uniques.iter().map(|x| x.name.clone()));
        out.extend(t.ann.iter().map(|(n, _, _)| n.clone()));
    }
    Ok(out)
}

/// The name Postgres gives an index or unique constraint of a table on these columns: `t_a_b_idx`,
/// `t_a_b_key`, made unique among those `taken`.
pub(super) fn index_name(table: &str, cols: &[String], suffix: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let base = format!("{}_{}_{suffix}", rel_part(table), if cols.is_empty() { "expr".into() } else { cols.join("_") });
    (0..).map(|i| if i == 0 { base.clone() } else { format!("{base}{i}") }).find(|n| !taken(n)).unwrap_or(base)
}

/// The name of `t`'s copy of its parent's foreign key when `t` has a constraint of the parent's name
/// for it: `t_a_b_fkey` for its columns, made unique among the constraint names of the schema, as
/// Postgres names it.
pub(super) fn fk_copy_name(v: &View, t: &Table, cols: &[String]) -> Result<String> {
    let schema = schema_part(&t.name);
    let others: Vec<Table> =
        schemas(v)?.into_iter().filter(|o| o.name != t.name && schema_part(&o.name) == schema).collect();
    let taken = |n: &str| t.has_constraint(n, true) || others.iter().any(|o| o.has_constraint(n, true));
    Ok(index_name(&t.name, cols, "fkey", &taken))
}

/// What Postgres's addFkRecurseReferenced adds for foreign key `fk` of `owner` (the table's own, not a partition's
/// copy of its parent's) when the table it references is partitioned: a constraint of `owner` for each table below
/// that one, depth first in bound order, each referencing that table and under the one made for the table above it
/// (the first level's under `fk`), named `<owner>_<columns>_fkeyN` with the first N no constraint of the schema has.
/// By the table each references: (that table, its name, the name of the one it is under).
pub(super) fn referenced_clones(v: &View, owner: &Table, fk: &Fk) -> Result<Vec<(String, String, String)>> {
    let Some(pk) = load(v, &fk.table)?.filter(|p| p.hier.part.is_some()) else { return Ok(vec![]) };
    let schema = schema_part(&owner.name);
    let others: Vec<Table> = schemas(v)?.into_iter().filter(|o| schema_part(&o.name) == schema).collect();
    let cols: Vec<String> =
        fk.cols.iter().filter_map(|id| owner.cols.iter().find(|c| c.id == *id).map(|c| c.name.clone())).collect();
    let mut made: Vec<(String, String, String)> = vec![];
    referenced_below(v, owner, fk, &cols, &others, &mut made, &pk, &fk.name)?;
    Ok(made)
}

/// `referenced_clones` for the tables below partitioned table `t`, whose constraint is named `above`: each, then
/// those below it (depth first).
#[allow(clippy::too_many_arguments)]
fn referenced_below(
    v: &View,
    owner: &Table,
    fk: &Fk,
    cols: &[String],
    others: &[Table],
    made: &mut Vec<(String, String, String)>,
    t: &Table,
    above: &str,
) -> Result<()> {
    for kid in kids_in_order(v, t)? {
        if kid.hier.of.is_none() {
            continue;
        }
        let name = {
            let taken = |n: &str| {
                n == fk.name || others.iter().any(|o| o.has_constraint(n, true)) || made.iter().any(|m| m.1 == n)
            };
            index_name(&owner.name, cols, "fkey", &taken)
        };
        made.push((kid.name.clone(), name.clone(), above.to_string()));
        if kid.hier.part.is_some() {
            referenced_below(v, owner, fk, cols, others, made, &kid, &name)?;
        }
    }
    Ok(())
}

/// That no row a foreign key to `parent` (or a table above it) references is in `kid`, a partition of it about to be
/// detached (Postgres's ATDetachCheckNoForeignKeyRefs): 23503 naming the constraint made for `kid` (see
/// `referenced_clones`) where one is.
fn detach_refs_hold(v: &View, parent: &Table, kid: &Table) -> Result<()> {
    let tables = schemas(v)?;
    let by_name: HashMap<&str, &Table> = tables.iter().map(|t| (t.name.as_str(), t)).collect();
    let mut above: Vec<&str> = vec![parent.name.as_str()];
    above.extend(partition_chain(&by_name, parent));
    if !tables.iter().any(|c| c.fks.iter().any(|f| above.contains(&f.table.as_str()))) {
        return Ok(());
    }
    let holders = row_tables(v, kid)?;
    for child in &tables {
        for fk in child.fks.iter().filter(|f| above.contains(&f.table.as_str())) {
            for h in &holders {
                let cols = ref_cols(h, fk);
                let mut found: Option<Vec<Value>> = None;
                each_row(v, h, |vals| {
                    let key: Vec<Value> = cols.iter().map(|&i| vals[i].clone()).collect();
                    if key.contains(&Value::Null) {
                        return Ok(true);
                    }
                    if v.prefix(&ref_prefix(child, fk, &key))?.next().is_some() {
                        found = Some(key);
                        return Ok(false);
                    }
                    Ok(true)
                })?;
                let Some(key) = found else { continue };
                let (owner, con) = action_names(v, &by_name, child, fk, &kid.name);
                let names: Vec<&str> = cols.iter().map(|&i| h.cols[i].name.as_str()).collect();
                let shown: Vec<String> = key.iter().map(Value::to_text).collect();
                return err_detail(
                    "23503",
                    format!("removing partition \"{}\" violates foreign key constraint \"{con}\"", rel_part(&kid.name)),
                    format!(
                        "Key ({})=({}) is still referenced from table \"{}\".",
                        names.join(", "),
                        shown.join(", "),
                        rel_part(&owner)
                    ),
                );
            }
        }
    }
    Ok(())
}

/// The table and the constraint Postgres names when a row of `pk` that foreign key `fk` of `child` references can't
/// go: the table that has `fk` as its own (a partition's copy is its parent's, up to the table whose own it is), and
/// that foreign key's name, or, where `pk` is a partition of the partitioned table it references, the name of the
/// one made for `pk` (see `referenced_clones`).
pub(super) fn action_names(
    v: &View,
    by_name: &HashMap<&str, &Table>,
    child: &Table,
    fk: &Fk,
    pk: &str,
) -> (String, String) {
    let (mut t, mut f) = (child.clone(), fk.clone());
    for _ in 0..64 {
        let Some((p, _)) = t.hier.of.clone() else { break };
        if !t.hier.inherited.contains(&f.name) {
            break;
        }
        let up = f.up.clone().unwrap_or_else(|| f.name.clone());
        let Some(pt) = by_name.get(p.as_str()).copied() else { break };
        let Some(pf) = pt.fks.iter().find(|x| x.name == up).cloned() else { break };
        (t, f) = (pt.clone(), pf);
    }
    let mut con = f.name.clone();
    if f.table != pk
        && let Ok(clones) = referenced_clones(v, &t, &f)
        && let Some((_, n, _)) = clones.into_iter().find(|(p, ..)| p == pk)
    {
        con = n;
    }
    (t.name, con)
}

/// Gives partition `child` what its parent has: CHECK constraints, UNIQUE constraints, indexes,
/// foreign keys and row triggers, each marked as inherited. `attaching`: the table existed, and
/// what it has that is the same as what the parent has is kept (and takes the parent's place),
/// not made again. Columns are matched by name.
pub(super) fn clone_constraints(
    child: &mut Table,
    parent: &Table,
    attaching: bool,
    others: &HashSet<String>,
    v: &View,
) -> Result<()> {
    // (a table's checks are counted once for each parent: those of a partition below one being attached
    // are counted already)
    if !attaching || !child.checks.iter().any(|c| c.inh > 0 && parent.checks.iter().any(|p| p.name == c.name)) {
        inherit_checks(child, std::slice::from_ref(parent))?;
    }
    if parent.has_key() {
        if attaching && !child.has_key() {
            // the key the parent has, on the table's columns
            let ids: Vec<u32> = parent.key_cols().iter().map(|&i| parent.cols[i].id).collect();
            if let Some(m) = map_ids(&ids, parent, child) {
                let at: Vec<usize> = m.iter().filter_map(|id| child.cols.iter().position(|c| c.id == *id)).collect();
                match at[..] {
                    [i] => child.key = Some(i),
                    _ => child.pk = at.clone(),
                }
                for i in at {
                    child.cols[i].not_null = true;
                }
            }
        }
        if !child.hier.inherited.iter().any(|x| x == "*pkey") {
            child.hier.inherited.push("*pkey".into());
        }
    }
    for u in &parent.uniques {
        let Some(cols) = map_ids(&u.cols, parent, child) else { continue };
        let backed = parent.indexes.iter().any(|x| x.name == u.name);
        if attaching
            && let Some(x) = child
                .uniques
                .iter()
                .find(|x| x.cols == cols && x.nnd == u.nnd && child.indexes.iter().any(|i| i.name == x.name) == backed)
        {
            let n = x.name.clone();
            if !child.hier.inherited.contains(&n) {
                child.hier.inherited.push(n);
            }
            continue;
        }
        let backing = parent.indexes.iter().find(|x| x.name == u.name);
        let cn = index_words(parent, &u.cols, backing.map_or(&[][..], |x| &x.exprs));
        let taken = |n: &str| {
            child.uniques.iter().any(|x| x.name == n)
                || child.indexes.iter().any(|x| x.name == n)
                || others.contains(n)
                || n == child.pkey_name()
        };
        let name = index_name(&child.name, &cn, if backing.is_some() { "idx" } else { "key" }, &taken);
        child.uniques.push(Unique { name: name.clone(), cols: cols.clone(), nnd: u.nnd, primary: u.primary });
        if let Some(x) = backing {
            child.indexes.push(btree::Index {
                name: name.clone(),
                cols,
                exprs: x.exprs.clone(),
                filter: x.filter.clone(),
                gin: x.gin,
                colls: x.colls.clone(),
            });
        }
        child.hier.inherited.push(name);
    }
    for x in parent.indexes.iter().filter(|x| !parent.uniques.iter().any(|u| u.name == x.name)) {
        let Some(cols) = map_ids(&x.cols, parent, child) else { continue };
        // (one in another collation, or already a partition of another index, is not it)
        let attached = kid_constraint(parent, child, &x.name);
        if attaching
            && let Some(o) = child.indexes.iter().find(|o| {
                o.cols == cols
                    && o.exprs == x.exprs
                    && o.filter == x.filter
                    && (0..cols.len()).all(|k| o.coll(k) == x.coll(k))
                    && (attached.as_deref() == Some(o.name.as_str()) || !child.hier.inherited.contains(&o.name))
                    && !child.uniques.iter().any(|u| u.name == o.name)
                    // (nor is an invalid one: a partitioned table's that its partitions don't all have)
                    && index_valid(v, child, &o.name)
            })
        {
            let n = o.name.clone();
            if !child.hier.inherited.contains(&n) {
                child.hier.inherited.push(n);
            }
            continue;
        }
        let cn = index_words(parent, &x.cols, &x.exprs);
        let taken = |n: &str| {
            child.uniques.iter().any(|u| u.name == n) || child.indexes.iter().any(|i| i.name == n) || others.contains(n)
        };
        let name = index_name(&child.name, &cn, "idx", &taken);
        child.indexes.push(btree::Index {
            name: name.clone(),
            cols,
            exprs: x.exprs.clone(),
            filter: x.filter.clone(),
            gin: x.gin,
            colls: x.colls.clone(),
        });
        child.hier.inherited.push(name);
    }
    for f in &parent.fks {
        let Some(cols) = map_ids(&f.cols, parent, child) else { continue };
        // (a partition below one attached has its copies already)
        if child
            .fks
            .iter()
            .any(|o| child.hier.inherited.contains(&o.name) && o.up.as_ref().unwrap_or(&o.name) == &f.name)
        {
            continue;
        }
        // (one that acts as the parent's does first, when several are on the same columns)
        let same = |o: &Fk| {
            o.cols == cols && o.table == f.table && o.refs == f.refs && !child.hier.inherited.contains(&o.name)
        };
        let acts = |o: &Fk| {
            (o.on_delete, o.on_update, o.deferrable, o.deferred, o.full)
                == (f.on_delete, f.on_update, f.deferrable, f.deferred, f.full)
        };
        let at = child.fks.iter().position(|o| same(o) && acts(o)).or_else(|| child.fks.iter().position(same));
        if attaching && let Some(o) = at.map(|i| &mut child.fks[i]) {
            o.up = (o.name != f.name).then(|| f.name.clone());
            child.hier.inherited.push(o.name.clone());
            continue;
        }
        // (a table attached that has a constraint of the name names its copy as Postgres does; one made
        // a partition can't have it, and names its key after)
        let mut name = f.name.clone();
        if child.has_constraint(&name, attaching) {
            if !attaching {
                return err(
                    "42710",
                    format!("constraint \"{name}\" for relation \"{}\" already exists", rel_part(&child.name)),
                );
            }
            let names: Vec<String> =
                cols.iter().filter_map(|id| child.cols.iter().find(|c| c.id == *id)).map(|c| c.name.clone()).collect();
            name = fk_copy_name(v, child, &names)?;
        }
        // (ON DELETE SET ... (columns) names the partition's own columns)
        let delete_cols = map_ids(&f.delete_cols, parent, child).unwrap_or_default();
        child.fks.push(Fk {
            name: name.clone(),
            cols,
            delete_cols,
            up: (name != f.name).then(|| f.name.clone()),
            ..f.clone()
        });
        child.hier.inherited.push(name);
    }
    for t in parent.triggers.iter().filter(|t| t.row) {
        let mark = format!("trigger:{}", t.name);
        if child.hier.inherited.contains(&mark) {
            continue;
        }
        // (a table attached that has a trigger of the name can't take the parent's)
        if child.triggers.iter().any(|x| x.name == t.name) {
            return err(
                "42710",
                format!("trigger \"{}\" for relation \"{}\" already exists", t.name, rel_part(&child.name)),
            );
        }
        let mut t = t.clone();
        if let Some(cols) = map_ids(&t.cols, parent, child) {
            t.cols = cols;
        }
        child.hier.inherited.push(mark);
        child.triggers.push(t);
    }
    child.triggers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(())
}

// ---------------------------------------------------------------- the catalog

/// What the catalog says of a table's place in a hierarchy (see `TableInfo`).
#[derive(Clone, Debug, Default)]
pub struct HierInfo {
    /// PARTITION BY: pg_partitioned_table's strategy letter, the key's columns (attnums, 0 for an
    /// expression), its expressions (as SQL), `pg_get_partkeydef`'s text and the default partition
    pub part: Option<PartInfo>,
    /// a partition: its parent, its bound as `pg_get_expr(relpartbound, oid)` writes it, and its
    /// partition constraint as `pg_get_partition_constraintdef` writes it
    pub of: Option<(String, String, String)>,
    /// the tables it inherits from (a partition's parent first), in pg_inherits's order
    pub parents: Vec<String>,
    /// the tables below it, each with its bound as text ('' for one that isn't a partition)
    pub children: Vec<(String, String)>,
    /// the tables directly below it in the order they were made (what pg_partition_tree walks)
    pub created: Vec<String>,
    /// per column: attislocal and attinhcount
    pub cols: Vec<(bool, i64)>,
    /// pg_class.relreplident: `d` (DEFAULT), `f` (FULL), `n` (NOTHING) or `i` (USING INDEX); '\0' for what
    /// isn't a table
    pub replident: char,
    /// a typed table's composite type, by stored name (see `Hier::of_type`)
    pub of_type: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PartInfo {
    pub strat: char,
    pub attrs: Vec<i64>,
    pub exprs: Option<String>,
    pub def: String,
    pub default: Option<String>,
}

/// A relation's oid as `tableoid` reads it: an oid.
fn oid_lit(o: i64) -> Expr {
    Expr::Cast(Box::new(Expr::Lit(Value::Int(o))), Type::Oid)
}

/// A constant as a partition constraint shows it, compared with a key part of this type: an integer
/// as it is, a bigint or smallint (or any negative number) in quotes with its type, and so on.
fn qual_lit(v: &Value, ty: Type, col: Option<&Col>) -> String {
    let q = |t: &str| format!("'{}'", t.replace('\'', "''"));
    let chars = col.and_then(|c| c.chars);
    let small = col.is_some_and(|c| c.small);
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) if ty == Type::Int && !small && *n >= 0 => n.to_string(),
        Value::Int(n) if ty == Type::Int && !small => format!("{}::integer", q(&n.to_string())),
        Value::Int(n) if small => format!("{}::smallint", q(&n.to_string())),
        Value::Int(n) if ty == Type::BigInt => format!("{}::bigint", q(&n.to_string())),
        Value::Numeric(_) | Value::Int(_) | Value::Float(_) if ty == Type::Numeric => {
            let n = match (col.and_then(|c| c.numeric), v) {
                (Some((_, s)), v) => {
                    let d = match v {
                        Value::Numeric(d) => d.clone(),
                        v => match coerce(v.clone(), Type::Numeric) {
                            Ok(Value::Numeric(d)) => d,
                            _ => return q(&v.to_text()),
                        },
                    };
                    round_dec(&d, s).map_or_else(|_| d.to_string(), |r| r.to_string())
                }
                (None, v) => v.to_text(),
            };
            match col.and_then(|c| c.numeric) {
                Some((p, s)) => format!("{n}::numeric({p},{s})"),
                None => format!("{n}::numeric"),
            }
        }
        Value::Float(f) => format!("{}::double precision", q(&Value::Float(*f).to_text())),
        Value::Text(s) => match chars {
            Some((n, fixed)) => format!("{}::{}", q(s), super::chars_name(n, fixed)),
            None => format!("{}::{}", q(s), ty.pg_name()),
        },
        v => format!("{}::{}", q(&v.to_text()), ty.pg_name()),
    }
}

/// The condition a partition's bound puts on rows of `parent` (its parts as SQL, to be put together
/// with AND): what `pg_get_partition_constraintdef` writes for the partition. `siblings`: the
/// other partitions' bounds, for a DEFAULT one.
fn bound_items(parent: &Table, key: &PartKey, bound: &PartBound, siblings: &[PartBound]) -> Result<Vec<String>> {
    let types = key.types(parent)?;
    let keys: Vec<(String, Option<&Col>)> = key
        .parts
        .iter()
        .map(|p| match p {
            KeyPart::Col(id) => {
                let c = parent.cols.iter().find(|c| c.id == *id);
                (c.map_or_else(String::new, |c| ident(&c.name)), c)
            }
            KeyPart::Expr(text) => (
                check_expr(text)
                    .ok()
                    .and_then(|e| catalog::deparse_expr(&e, &parent.cols))
                    .map_or_else(|| format!("({text})"), |(d, _)| d),
                None,
            ),
        })
        .collect();
    let lit = |i: usize, v: &Value| qual_lit(v, types[i], keys[i].1);
    // a character column compares as text
    let lhs = |i: usize| match keys[i].1.and_then(|c| c.chars) {
        Some(_) => format!("({})::text", keys[i].0),
        None => keys[i].0.clone(),
    };
    let not_null = |i: usize| format!("({} IS NOT NULL)", keys[i].0);
    Ok(match bound {
        PartBound::Hash(m, r) => {
            let cols: Vec<String> = keys.iter().map(|(k, _)| k.clone()).collect();
            vec![format!("satisfies_hash_partition('@{}@'::oid, {m}, {r}, {})", parent.name, cols.join(", "))]
        }
        PartBound::List(vals) => {
            let nulls = vals.contains(&Value::Null);
            let rest: Vec<&Value> = vals.iter().filter(|v| **v != Value::Null).collect();
            let test = match &rest[..] {
                [] => None,
                [one] => Some(format!("({} = {})", lhs(0), lit(0, one))),
                many => Some(format!(
                    "({} = ANY (ARRAY[{}]))",
                    lhs(0),
                    many.iter().map(|v| lit(0, v)).collect::<Vec<_>>().join(", ")
                )),
            };
            match (nulls, test) {
                (true, None) => vec![format!("({} IS NULL)", keys[0].0)],
                (true, Some(t)) => vec![format!("(({} IS NULL) OR {t})", keys[0].0)],
                (false, Some(t)) => vec![format!("({} IS NOT NULL)", keys[0].0), t],
                (false, None) => vec!["false".into()],
            }
        }
        PartBound::Range(from, to) => {
            let mut items: Vec<String> = (0..keys.len()).map(not_null).collect();
            let mut i = 0;
            while i < keys.len() {
                match (&from[i], &to[i]) {
                    (Edge::Val(a), Edge::Val(b)) if compare(a, b).ok() == Some(Ordering::Equal) => {
                        items.push(format!("({} = {})", keys[i].0, lit(i, a)));
                        i += 1;
                    }
                    _ => break,
                }
            }
            // a side's terms: the columns up to the first MINVALUE or MAXVALUE
            let side = |edges: &[Edge], lower: bool| -> Option<String> {
                let m = edges[i..].iter().take_while(|e| matches!(e, Edge::Val(_))).count();
                if m == 0 {
                    return None;
                }
                let end = i + m;
                let last = match (lower, edges.get(end)) {
                    (true, None | Some(Edge::Min)) => ">=",
                    (true, Some(_)) => ">",
                    (false, None | Some(Edge::Min)) if edges.get(end).is_none() => "<",
                    (false, Some(Edge::Max)) => "<=",
                    (false, _) => "<",
                };
                let (strict, last) = (if lower { ">" } else { "<" }, last);
                let mut text = String::new();
                for j in (i..end).rev() {
                    let Edge::Val(v) = &edges[j] else { return None };
                    text = if j == end - 1 {
                        format!("({} {last} {})", keys[j].0, lit(j, v))
                    } else {
                        format!(
                            "(({} {strict} {}) OR (({} = {}) AND {text}))",
                            keys[j].0,
                            lit(j, v),
                            keys[j].0,
                            lit(j, v)
                        )
                    };
                }
                Some(text)
            };
            items.extend(side(from, true));
            items.extend(side(to, false));
            items
        }
        PartBound::Default => {
            // no other partition takes the row
            let mut taken: Vec<String> = vec![];
            let mut null_taken = false;
            let mut list: Vec<&Value> = vec![];
            let mut ranges: Vec<String> = vec![];
            for s in siblings {
                match s {
                    PartBound::List(vs) => {
                        null_taken |= vs.contains(&Value::Null);
                        list.extend(vs.iter().filter(|v| **v != Value::Null));
                    }
                    PartBound::Range(..) => {
                        let items = bound_items(parent, key, s, &[])?;
                        let body: Vec<String> = items.into_iter().skip(keys.len()).collect();
                        ranges.push(match &body[..] {
                            [one] => one.clone(),
                            many => format!("({})", many.join(" AND ")),
                        });
                    }
                    _ => {}
                }
            }
            if !list.is_empty() {
                let t = match &list[..] {
                    [one] => format!("({} = {})", lhs(0), lit(0, one)),
                    many => format!(
                        "({} = ANY (ARRAY[{}]))",
                        lhs(0),
                        many.iter().map(|v| lit(0, v)).collect::<Vec<_>>().join(", ")
                    ),
                };
                taken.push(if null_taken {
                    format!("(({} IS NULL) OR {t})", keys[0].0)
                } else {
                    format!("(({} IS NOT NULL) AND {t})", keys[0].0)
                });
            } else if null_taken {
                taken.push(format!("({} IS NULL)", keys[0].0));
            }
            if !ranges.is_empty() {
                let any = match &ranges[..] {
                    [one] => one.clone(),
                    many => format!("({})", many.join(" OR ")),
                };
                taken.push(format!("(({} IS NOT NULL) AND {any})", keys[0].0));
            }
            match &taken[..] {
                [] => vec![],
                [one] => vec![format!("(NOT {one})")],
                many => vec![format!("(NOT ({}))", many.join(" OR "))],
            }
        }
    })
}

/// A partition's constraint as its parents and it put it, as SQL (empty: no row is refused).
fn partition_items(v: &View, t: &Table) -> Result<Vec<String>> {
    let Some((p, bound)) = &t.hier.of else { return Ok(vec![]) };
    let parent = need_table(v, p)?;
    let Some(key) = &parent.hier.part else { return Ok(vec![]) };
    let types = key.types(&parent)?;
    let bound = bound.typed(&types)?;
    let mut siblings = vec![];
    if bound == PartBound::Default {
        for k in kids_in_order(v, &parent)? {
            if k.name != t.name
                && let Some((_, b)) = &k.hier.of
            {
                siblings.push(b.typed(&types)?);
            }
        }
    }
    let mut items = partition_items(v, &parent)?;
    items.extend(bound_items(&parent, key, &bound, &siblings)?);
    Ok(items)
}

/// `pg_get_partition_constraintdef` of `t`.
pub(super) fn partition_constraint(v: &View, t: &Table) -> Result<String> {
    let items = partition_items(v, t)?;
    Ok(match &items[..] {
        [] => String::new(),
        [one] => one.clone(),
        many => format!("({})", many.join(" AND ")),
    })
}

/// A table's place in its hierarchy, for the catalog.
pub(super) fn info(v: &View, t: &Table) -> Result<HierInfo> {
    let mut h = HierInfo {
        cols: t.cols.iter().map(|c| (c.local, i64::from(c.inh))).collect(),
        replident: t.hier.replident.as_ref().map_or('d', |r| r.0),
        of_type: t.hier.of_type.clone(),
        ..HierInfo::default()
    };
    h.parents = t.hier.all_parents().into_iter().map(String::from).collect();
    if let Some((p, b)) = &t.hier.of {
        let parent = need_table(v, p)?;
        let text = match &parent.hier.part {
            Some(k) => b.typed(&k.types(&parent)?)?.text(&k.shows(&parent)?),
            None => String::new(),
        };
        h.of = Some((p.clone(), text, partition_constraint(v, t)?));
    }
    if let Some(k) = &t.hier.part {
        let attrs: Vec<i64> = k
            .parts
            .iter()
            .map(|p| match p {
                KeyPart::Col(id) => t.cols.iter().position(|c| c.id == *id).map_or(0, |i| t.attnum(i)),
                KeyPart::Expr(_) => 0,
            })
            .collect();
        let exprs: Vec<String> = k
            .parts
            .iter()
            .filter_map(|p| match p {
                KeyPart::Expr(e) => Some(catalog::expr_def(e, &t.cols, false)),
                KeyPart::Col(_) => None,
            })
            .collect();
        let kids = kids_in_order(v, t)?;
        let default = kids.iter().find(|k| matches!(&k.hier.of, Some((_, PartBound::Default)))).map(|k| k.name.clone());
        h.part = Some(PartInfo {
            strat: k.strategy.letter(),
            attrs,
            exprs: (!exprs.is_empty()).then(|| exprs.join(", ")),
            def: k.def(t),
            default,
        });
    }
    if t.hier.is_parent() {
        h.created = child_names(v, &t.name)?;
        let types = t.hier.part.as_ref().map(|k| k.types(t)).transpose()?;
        let shown = t.hier.part.as_ref().map(|k| k.shows(t)).transpose()?;
        for k in kids_in_order(v, t)? {
            let text = match (&k.hier.of, &types, &shown) {
                (Some((_, b)), Some(ty), Some(sh)) => b.typed(ty)?.text(sh),
                _ => String::new(),
            };
            h.children.push((k.name.clone(), text));
        }
    }
    Ok(h)
}

/// The writes that give each partition of `parent` the index `ix` (and the UNIQUE constraint `u` it
/// is, if it is) that was just made on it, and the partitions below those: an index a partition has
/// of its own that is the same is the parent's from then on.
pub(super) fn spread_index(
    v: &View,
    parent: &Table,
    ix: &btree::Index,
    u: Option<&Unique>,
    flush: &mut dyn FnMut(Writes) -> crate::Result<()>,
) -> Result<Writes> {
    let mut writes = vec![];
    let mut taken = index_names(v)?;
    for mut kid in children(v, &parent.name)? {
        // (a foreign partition has no indexes)
        if kid.hier.of.is_none() || kid.hier.foreign.is_some() {
            continue;
        }
        let Some(cols) = map_ids(&ix.cols, parent, &kid) else { continue };
        // (one in another collation, or already a partition of another index, is not it)
        let attached = kid_constraint(parent, &kid, &ix.name);
        let same = |o: &btree::Index| {
            o.cols == cols
                && o.exprs == ix.exprs
                && o.filter == ix.filter
                && (0..cols.len()).all(|k| o.coll(k) == ix.coll(k))
                && (attached.as_deref() == Some(o.name.as_str()) || !kid.hier.inherited.contains(&o.name))
                && kid.uniques.iter().any(|k| k.name == o.name) == u.is_some()
        };
        // (one the partition has already is attached as it is: the partitions below it keep what they have)
        let fresh = !kid.indexes.iter().any(&same);
        let name = match kid.indexes.iter().find(|o| same(o)) {
            Some(o) => o.name.clone(),
            None => {
                let cn = index_words(&kid, &cols, &ix.exprs);
                let name = index_name(&kid.name, &cn, "idx", &|n: &str| taken.contains(n));
                taken.insert(name.clone());
                kid.indexes.push(btree::Index {
                    name: name.clone(),
                    cols: cols.clone(),
                    exprs: ix.exprs.clone(),
                    filter: ix.filter.clone(),
                    gin: ix.gin,
                    colls: ix.colls.clone(),
                });
                if let Some(u) = u {
                    kid.uniques.push(Unique { name: name.clone(), cols, nnd: u.nnd, primary: u.primary });
                }
                name
            }
        };
        if !kid.hier.inherited.contains(&name) {
            kid.hier.inherited.push(name.clone());
        }
        let kix = kid.indexes.iter().find(|o| o.name == name).cloned().unwrap_or_else(|| ix.clone());
        let ku = kid.uniques.iter().find(|x| x.name == name).cloned();
        writes.extend(build_index(v, &kid, Some(&kix), ku.as_ref(), flush)?);
        writes.push((schema_key(&kid.name), Some(schema_json(&kid))));
        if fresh && kid.hier.part.is_some() {
            writes.extend(spread_index(v, &kid, &kix, ku.as_ref(), flush)?);
        }
    }
    Ok(writes)
}

/// The leaf partitions below partitioned table `t`, each with its index that is `t`'s index `name`'s, in partition
/// order: what CLUSTER of a partitioned table clusters.
pub(super) fn leaf_indexes(v: &View, t: &Table, name: &str) -> Result<Vec<(Table, String)>> {
    let mut out = vec![];
    for kid in kids_in_order(v, t)? {
        let Some(n) = kid_constraint(t, &kid, name).filter(|_| kid.hier.of.is_some()) else { continue };
        if kid.hier.part.is_some() {
            out.extend(leaf_indexes(v, &kid, &n)?);
        } else {
            out.push((kid, n));
        }
    }
    Ok(out)
}

/// The name of the index of partition `t`'s parent that the partition's index `name` is of (None: none).
pub(super) fn parent_index(v: &View, t: &Table, name: &str) -> Result<Option<String>> {
    let Some((p, _)) = &t.hier.of else { return Ok(None) };
    let parent = need_table(v, p)?;
    Ok(kid_constraint_up(&parent, t, name))
}

/// The parent's name for the index or constraint `name` of its partition `kid`.
fn kid_constraint_up(parent: &Table, kid: &Table, name: &str) -> Option<String> {
    let mut names: Vec<&str> = parent.indexes.iter().map(|x| x.name.as_str()).collect();
    names.extend(parent.uniques.iter().map(|x| x.name.as_str()));
    names.into_iter().find(|n| kid_constraint(parent, kid, n).as_deref() == Some(name)).map(String::from)
}

/// The writes that drop, from the partitions below `t`, the indexes that are `t`'s index `name`'s.
pub(super) fn drop_index_below(v: &View, t: &Table, name: &str) -> Result<Writes> {
    let mut writes = vec![];
    if t.hier.part.is_none() {
        return Ok(writes);
    }
    for mut kid in children(v, &t.name)? {
        let Some(n) = kid_constraint(t, &kid, name).filter(|_| kid.hier.of.is_some()) else { continue };
        writes.extend(drop_index_below(v, &kid, &n)?);
        kid.indexes.retain(|x| x.name != n);
        let mut gone = vec![btree::prefix(&kid.name, &n)];
        if kid.uniques.iter().any(|u| u.name == n) {
            kid.uniques.retain(|u| u.name != n);
            gone.push(format!("{UNIQ}/{}/{n}/", kid.name));
        }
        kid.hier.inherited.retain(|x| *x != n);
        writes.push((schema_key(&kid.name), Some(schema_json(&kid))));
        for p in gone {
            for r in v.prefix(&p)? {
                writes.push((r?.0, None));
            }
        }
    }
    Ok(writes)
}

/// The words an index's default name is made of: its columns' names, and an expression's as Postgres's
/// FigureIndexColname has it (`lower(b)` is `lower`, `(a + b)` is `expr`), numbered where one repeats (`a`, `a1`).
pub(super) fn index_words(t: &Table, cols: &[u32], exprs: &[String]) -> Vec<String> {
    let mut ex = exprs.iter();
    let words = cols
        .iter()
        .map(|&c| {
            if c == btree::EXPR {
                ex.next().map_or_else(|| "expr".into(), |e| super::catalog_objects::expr_name(e))
            } else {
                t.cols.iter().find(|x| x.id == c).map(|x| x.name.clone()).unwrap_or_default()
            }
        })
        .collect();
    super::catalog_objects::index_column_names(words)
}

/// Whether the index `name` of `t` is valid: on a partitioned table, every partition has the index that
/// is its (and so on below).
pub(super) fn index_valid(v: &View, t: &Table, name: &str) -> bool {
    if t.hier.part.is_none() {
        return true;
    }
    let Ok(kids) = children(v, &t.name) else { return true };
    kids.iter().filter(|k| k.hier.of.is_some()).all(|k| match kid_constraint(t, k, name) {
        Some(n) => index_valid(v, k, &n),
        None => false,
    })
}

/// The tables above `t` that it is a partition of, nearest first, by name.
pub(super) fn partition_chain<'a>(by_name: &HashMap<&str, &'a Table>, t: &Table) -> Vec<&'a str> {
    let mut out: Vec<&'a str> = vec![];
    let mut cur = t.hier.of.as_ref().map(|(p, _)| p.as_str());
    while let Some(p) = cur {
        let Some(x) = by_name.get(p) else { break };
        if out.contains(&x.name.as_str()) {
            break;
        }
        out.push(&x.name);
        cur = x.hier.of.as_ref().map(|(p, _)| p.as_str());
    }
    out
}

/// The writes that keep `t`, which is renamed `to`, in its hierarchy: the rows that make it a child of its
/// parents and a parent of its children, and what its children say of it.
fn rename_links(v: &View, t: &Table, to: &str) -> Result<Writes> {
    let mut w: Writes = vec![];
    for p in t.hier.all_parents() {
        let raw = v.get(&inh_key(p, &t.name))?.map(|x| x.to_string());
        w.push((inh_key(p, &t.name), None));
        w.push((inh_key(p, to), raw));
    }
    let prefix = format!("{INH}/{}/", t.name);
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        let child = k[prefix.len()..].to_string();
        w.push((k, None));
        w.push((inh_key(to, &child), Some(raw.to_string())));
        let mut c = need_table(v, &child)?;
        if let Some((p, _)) = &mut c.hier.of
            && *p == t.name
        {
            *p = to.to_string();
        }
        for p in &mut c.hier.parents {
            if *p == t.name {
                *p = to.to_string();
            }
        }
        w.push((schema_key(&child), Some(schema_json(&c))));
    }
    Ok(w)
}

/// The writes that give each partition of `parent` its row trigger `x` (the partitions below those too). A
/// partition's own trigger of the name is taken over by OR REPLACE (`replace`), else refused.
pub(super) fn spread_trigger(v: &View, parent: &Table, x: &functions::Trigger, replace: bool) -> Result<Writes> {
    let mut writes = vec![];
    for mut kid in children(v, &parent.name)? {
        if kid.hier.of.is_none() {
            continue;
        }
        let mut y = x.clone();
        if let Some(cols) = map_ids(&x.cols, parent, &kid) {
            y.cols = cols;
        }
        let mark = format!("trigger:{}", y.name);
        match kid.triggers.iter().position(|t| t.name == y.name) {
            Some(_) if !replace && !kid.hier.inherited.contains(&mark) => {
                return err(
                    "42710",
                    format!("trigger \"{}\" for relation \"{}\" already exists", y.name, rel_part(&kid.name)),
                );
            }
            Some(i) => kid.triggers[i] = y.clone(),
            None => kid.triggers.push(y.clone()),
        }
        kid.triggers.sort_by(|a, b| a.name.cmp(&b.name));
        if !kid.hier.inherited.contains(&mark) {
            kid.hier.inherited.push(mark);
        }
        writes.push((schema_key(&kid.name), Some(schema_json(&kid))));
        if kid.hier.part.is_some() {
            writes.extend(spread_trigger(v, &kid, &y, replace)?);
        }
    }
    Ok(writes)
}

/// The writes that take trigger `name` of partitioned table `t` away from its partitions.
pub(super) fn unspread_trigger(v: &View, t: &Table, name: &str) -> Result<Writes> {
    unspread_triggers(v, t, &[name.to_string()])
}

/// `unspread_trigger`, for several triggers at once.
fn unspread_triggers(v: &View, t: &Table, names: &[String]) -> Result<Writes> {
    let mut writes = vec![];
    if t.hier.part.is_none() || names.is_empty() {
        return Ok(writes);
    }
    let marks: Vec<String> = names.iter().map(|n| format!("trigger:{n}")).collect();
    for mut kid in children(v, &t.name)? {
        if kid.hier.of.is_none() {
            continue;
        }
        let gone: Vec<String> =
            names.iter().zip(&marks).filter(|(_, m)| kid.hier.inherited.contains(m)).map(|(n, _)| n.clone()).collect();
        if gone.is_empty() {
            continue;
        }
        kid.triggers.retain(|x| !gone.contains(&x.name));
        kid.hier.inherited.retain(|x| !marks.contains(x));
        writes.push((schema_key(&kid.name), Some(schema_json(&kid))));
        if kid.hier.part.is_some() {
            writes.extend(unspread_triggers(v, &kid, &gone)?);
        }
    }
    Ok(writes)
}
