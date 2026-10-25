//! EXPLAIN's plan tree and how it is printed. A plan is a tree of PostgreSQL's plan nodes (`plan` builds one where
//! it can make the plan Postgres's planner would) over a range table, its target lists Vars of the relations in it;
//! or Chronos's own steps (`Node::Step`). Either is written as commands/explain.c writes a plan: its text layout, and
//! FORMAT XML, JSON and YAML with explain.c's property names, in its order, and the groups the options ask for
//! (Planning, Settings, Triggers, the buffer counts).
//!
//! Portions Copyright (c) 1996-2023, PostgreSQL Global Development Group
//! Portions Copyright (c) 1994, The Regents of the University of California
//! (ported from PostgreSQL 16's src/backend/commands/explain.c, under the PostgreSQL License: see the NOTICE file)

use super::viewdef::{PlanRel, plan_expr};
use super::*;

/// EXPLAIN's output format (explain.c's ExplainFormat).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Format {
    Text,
    Xml,
    Json,
    Yaml,
}

/// EXPLAIN's options, as ExplainQuery reads them (explain.c:176-251): `timing` and `summary` are already their
/// defaults where not given (ANALYZE's).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ExplainOpts {
    pub(super) analyze: bool,
    pub(super) verbose: bool,
    pub(super) costs: bool,
    pub(super) buffers: bool,
    pub(super) wal: bool,
    pub(super) settings: bool,
    pub(super) generic: bool,
    pub(super) timing: bool,
    pub(super) summary: bool,
    pub(super) format: Format,
}

impl Default for ExplainOpts {
    /// NewExplainState's (explain.c:316): COSTS on, everything else off, TEXT.
    fn default() -> ExplainOpts {
        ExplainOpts {
            analyze: false,
            verbose: false,
            costs: true,
            buffers: false,
            wal: false,
            settings: false,
            generic: false,
            timing: false,
            summary: false,
            format: Format::Text,
        }
    }
}

impl ExplainOpts {
    /// The type of EXPLAIN's one column, as ExplainResultDesc gives it (explain.c:337): xml for XML, json for JSON,
    /// text otherwise.
    pub(super) fn column_type(&self) -> Type {
        match self.format {
            Format::Xml => Type::Xml,
            Format::Json => Type::JsonText,
            _ => Type::Text,
        }
    }
}

// ---------------------------------------------------------------- the plan

/// A relation of the plan's range table (PlannedStmt.rtable), and what EXPLAIN calls it.
#[derive(Clone, Debug)]
pub(super) struct Rte {
    /// the table's own name and its schema's (`pg_temp` for a temporary one)
    pub(super) relname: String,
    pub(super) schema: String,
    /// the name EXPLAIN shows it by: select_rtable_names_for_explain's (its alias, numbered where several share one)
    pub(super) refname: String,
    /// the name the query calls it, which its columns are qualified with as written
    pub(super) written: String,
    /// its columns: name, type, and the type id and modifier `col_ty` gives
    pub(super) cols: Vec<(String, Type, (i64, i64))>,
    /// its columns' collations
    pub(super) colls: Vec<collation::Coll>,
}

/// A column of a relation of the range table: Var's varno (from 1, in `PlannedStmt::rtable`) and varattno (from 1,
/// in the relation's `cols`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Var {
    pub(super) varno: usize,
    pub(super) attno: usize,
}

/// An expression of a plan node over the columns of one relation of the range table (`varno`), as the query wrote
/// it: what ruleutils.c deparses with that relation in its context.
#[derive(Clone, Debug)]
pub(super) struct PExpr {
    pub(super) expr: Expr,
    pub(super) varno: usize,
}

/// An entry of a node's target list, as VERBOSE's Output shows it.
#[derive(Clone, Debug)]
pub(super) enum Target {
    Var(Var),
    Expr(PExpr),
}

/// A sort key: what is sorted by (an expression the node below computes is shown in parentheses), and how: its
/// direction, where its nulls go, and the collation it sorts in (None: of a type that has none).
#[derive(Clone, Debug)]
pub(super) struct SortKey {
    pub(super) expr: PExpr,
    pub(super) desc: bool,
    pub(super) nulls_first: bool,
    pub(super) coll: Option<collation::Coll>,
}

/// What the planner's model says a node costs (Plan's startup_cost, total_cost, plan_rows and plan_width).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Cost {
    pub(super) startup: f64,
    pub(super) total: f64,
    pub(super) rows: f64,
    pub(super) width: i32,
}

/// What Chronos's executor did running a node, for ANALYZE (its Instrumentation): times in ms, the rows it gave,
/// how many times it ran (0: never), and the rows its Filter removed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Actual {
    pub(super) startup_ms: f64,
    pub(super) total_ms: f64,
    pub(super) rows: f64,
    pub(super) loops: f64,
    pub(super) filtered: f64,
}

/// Aggregate's strategy (AggStrategy).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum AggStrategy {
    Plain,
}

/// ModifyTable's operation (CmdType).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum CmdType {
    Update,
    Delete,
}

/// A plan node's kind, with what is its own.
#[derive(Clone, Debug)]
pub(super) enum Node {
    /// Result; `one_time`: its One-Time Filter (a relation that can't hold a row is read as `false`)
    Result {
        one_time: Option<PExpr>,
    },
    SeqScan {
        varno: usize,
    },
    Append,
    Sort {
        keys: Vec<SortKey>,
    },
    Limit,
    Agg {
        strategy: AggStrategy,
    },
    /// UPDATE or DELETE of the range table's relation `nominal`, writing the relations `targets`
    ModifyTable {
        op: CmdType,
        nominal: usize,
        targets: Vec<usize>,
    },
    /// a step of Chronos's own (chronos.explain = chronos): see `Step`
    Step(Step),
}

/// A step of Chronos's own as EXPLAIN shows it: `name` (`Seq Scan`, `Inner Join`, `Limit`) on `target` (a table),
/// `suffix` what the text layout adds after it (`  (actual rows=3)`), and a condition line (`Index Cond: ...`).
#[derive(Clone, Debug, Default)]
pub(super) struct Step {
    pub(super) name: String,
    pub(super) target: Option<String>,
    pub(super) suffix: String,
    pub(super) cond: Option<(String, String)>,
    /// the rows it gave under ANALYZE (`Actual Rows`), or found without it (`Rows Found`), and its loops
    pub(super) actual_rows: Option<usize>,
    pub(super) found_rows: Option<usize>,
    pub(super) loops: Option<usize>,
}

/// A plan node: its kind, what it costs, its target list, its Filter, its children and, under ANALYZE, what
/// running it did.
#[derive(Clone, Debug)]
pub(super) struct Plan {
    pub(super) node: Node,
    pub(super) cost: Option<Cost>,
    /// None: not known, so VERBOSE can't be shown
    pub(super) tlist: Option<Vec<Target>>,
    pub(super) qual: Vec<PExpr>,
    pub(super) kids: Vec<Plan>,
    pub(super) actual: Option<Actual>,
}

impl Plan {
    pub(super) fn new(node: Node) -> Plan {
        Plan { node, cost: None, tlist: None, qual: vec![], kids: vec![], actual: None }
    }

    pub(super) fn above(node: Node, kid: Plan) -> Plan {
        Plan { kids: vec![kid], ..Plan::new(node) }
    }

    /// Whether every node of the tree has what the options need: costs for COSTS, a target list for VERBOSE,
    /// actuals for ANALYZE (all or nothing: a plan missing one is not shown).
    pub(super) fn complete(&self, o: &ExplainOpts) -> bool {
        (!o.costs || self.cost.is_some())
            && (!o.verbose || self.tlist.is_some())
            && (!o.analyze || self.actual.is_some())
            && self.kids.iter().all(|k| k.complete(o))
    }
}

/// A statement's plan (PlannedStmt): its range table and its plan tree (Chronos's steps may be several, one after
/// another).
#[derive(Clone, Debug, Default)]
pub(super) struct PlannedStmt {
    pub(super) rtable: Vec<Rte>,
    pub(super) roots: Vec<Plan>,
}

/// What ExplainOnePlan prints around the plan tree: the planning and execution times (in ms), the settings that
/// change planning (name, value), the query's identifier.
#[derive(Clone, Debug, Default)]
pub(super) struct Summary {
    pub(super) planning_ms: Option<f64>,
    pub(super) execution_ms: Option<f64>,
    pub(super) settings: Vec<(String, String)>,
    pub(super) query_id: Option<i64>,
    /// track_io_timing: the I/O times are shown with the buffer counts in a non-text format
    pub(super) io_timing: bool,
}

// ---------------------------------------------------------------- the output's state

/// explain.c's ExplainState, as far as its printing goes: the format, the indent, and the grouping stack (JSON: 0
/// where nothing is out yet at that level; YAML: 0 where the level is unlabeled and its first line is to come).
struct Es<'a> {
    o: &'a ExplainOpts,
    str: String,
    indent: usize,
    grouping: Vec<u8>,
    p: &'a PlannedStmt,
    /// a deparse failed: the plan can't be shown
    failed: bool,
}

/// escape_json: a JSON string literal.
fn escape_json(out: &mut String, s: &str) {
    out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()));
}

/// escape_xml (utils/adt/xml.c): `&`, `<`, `>` and carriage returns as entities.
fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#x0d;"),
            c => out.push(c),
        }
    }
    out
}

const X_OPENING: u8 = 0;
const X_CLOSING: u8 = 1;
const X_CLOSE_IMMEDIATE: u8 = 2;
const X_NOWHITESPACE: u8 = 4;

impl Es<'_> {
    fn spaces(&mut self, n: usize) {
        self.str.extend(std::iter::repeat_n(' ', n));
    }

    /// ExplainXMLTag (explain.c:4964): characters a tag can't have made dashes (`I/O Read Time` is `I-O-Read-Time`).
    fn xml_tag(&mut self, tag: &str, flags: u8) {
        if flags & X_NOWHITESPACE == 0 {
            self.spaces(2 * self.indent);
        }
        self.str.push('<');
        if flags & X_CLOSING != 0 {
            self.str.push('/');
        }
        for c in tag.chars() {
            self.str.push(if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' });
        }
        if flags & X_CLOSE_IMMEDIATE != 0 {
            self.str.push_str(" /");
        }
        self.str.push('>');
        if flags & X_NOWHITESPACE == 0 {
            self.str.push('\n');
        }
    }

    /// ExplainIndentText (explain.c:4991).
    fn indent_text(&mut self) {
        if self.str.is_empty() || self.str.ends_with('\n') {
            self.spaces(self.indent * 2);
        }
    }

    /// ExplainJSONLineEnding (explain.c:5006): a comma after what came before at this level.
    fn json_line_ending(&mut self) {
        match self.grouping.last_mut() {
            Some(g) if *g != 0 => self.str.push(','),
            Some(g) => *g = 1,
            None => {}
        }
        self.str.push('\n');
    }

    /// ExplainYAMLLineStarting (explain.c:5026).
    fn yaml_line_starting(&mut self) {
        match self.grouping.last_mut() {
            Some(g) if *g == 0 => *g = 1,
            _ => {
                self.str.push('\n');
                self.spaces(self.indent * 2);
            }
        }
    }

    /// ExplainProperty (explain.c:4501): `numeric`, a value JSON doesn't quote.
    fn property(&mut self, label: &str, unit: Option<&str>, value: &str, numeric: bool) {
        match self.o.format {
            Format::Text => {
                self.indent_text();
                match unit {
                    Some(u) => self.str.push_str(&format!("{label}: {value} {u}\n")),
                    None => self.str.push_str(&format!("{label}: {value}\n")),
                }
            }
            Format::Xml => {
                self.spaces(self.indent * 2);
                self.xml_tag(label, X_OPENING | X_NOWHITESPACE);
                self.str.push_str(&escape_xml(value));
                self.xml_tag(label, X_CLOSING | X_NOWHITESPACE);
                self.str.push('\n');
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                escape_json(&mut self.str, label);
                self.str.push_str(": ");
                if numeric {
                    self.str.push_str(value);
                } else {
                    escape_json(&mut self.str, value);
                }
            }
            Format::Yaml => {
                self.yaml_line_starting();
                self.str.push_str(&format!("{label}: "));
                if numeric {
                    self.str.push_str(value);
                } else {
                    escape_json(&mut self.str, value);
                }
            }
        }
    }

    fn text(&mut self, label: &str, value: &str) {
        self.property(label, None, value, false);
    }

    fn integer(&mut self, label: &str, unit: Option<&str>, value: i64) {
        self.property(label, unit, &value.to_string(), true);
    }

    /// ExplainPropertyFloat: `%.*f`.
    fn float(&mut self, label: &str, unit: Option<&str>, value: f64, digits: usize) {
        self.property(label, unit, &format!("{value:.digits$}"), true);
    }

    fn boolean(&mut self, label: &str, value: bool) {
        self.property(label, None, if value { "true" } else { "false" }, true);
    }

    /// ExplainPropertyList (explain.c:4378): a list of unlabeled items.
    fn list(&mut self, label: &str, items: &[String]) {
        match self.o.format {
            Format::Text => {
                self.indent_text();
                self.str.push_str(&format!("{label}: {}\n", items.join(", ")));
            }
            Format::Xml => {
                self.xml_tag(label, X_OPENING);
                for it in items {
                    self.spaces(self.indent * 2 + 2);
                    self.str.push_str(&format!("<Item>{}</Item>\n", escape_xml(it)));
                }
                self.xml_tag(label, X_CLOSING);
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                escape_json(&mut self.str, label);
                self.str.push_str(": [");
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        self.str.push_str(", ");
                    }
                    escape_json(&mut self.str, it);
                }
                self.str.push(']');
            }
            Format::Yaml => {
                self.yaml_line_starting();
                self.str.push_str(&format!("{label}: "));
                for it in items {
                    self.str.push('\n');
                    self.spaces(self.indent * 2 + 2);
                    self.str.push_str("- ");
                    escape_json(&mut self.str, it);
                }
            }
        }
    }

    /// ExplainOpenGroup (explain.c:4619): `labeled`, its members are labeled properties (an object), else unlabeled
    /// ones (a list).
    fn open_group(&mut self, objtype: &str, label: Option<&str>, labeled: bool) {
        match self.o.format {
            Format::Text => {}
            Format::Xml => {
                self.xml_tag(objtype, X_OPENING);
                self.indent += 1;
            }
            Format::Json => {
                self.json_line_ending();
                self.spaces(2 * self.indent);
                if let Some(l) = label {
                    escape_json(&mut self.str, l);
                    self.str.push_str(": ");
                }
                self.str.push(if labeled { '{' } else { '[' });
                self.grouping.push(0);
                self.indent += 1;
            }
            Format::Yaml => {
                self.yaml_line_starting();
                match label {
                    Some(l) => {
                        self.str.push_str(&format!("{l}: "));
                        self.grouping.push(1);
                    }
                    None => {
                        self.str.push_str("- ");
                        self.grouping.push(0);
                    }
                }
                self.indent += 1;
            }
        }
    }

    /// ExplainCloseGroup (explain.c:4682).
    fn close_group(&mut self, objtype: &str, labeled: bool) {
        match self.o.format {
            Format::Text => {}
            Format::Xml => {
                self.indent -= 1;
                self.xml_tag(objtype, X_CLOSING);
            }
            Format::Json => {
                self.indent -= 1;
                self.str.push('\n');
                self.spaces(2 * self.indent);
                self.str.push(if labeled { '}' } else { ']' });
                self.grouping.pop();
            }
            Format::Yaml => {
                self.indent -= 1;
                self.grouping.pop();
            }
        }
    }

    /// ExplainBeginOutput (explain.c:4875).
    fn begin_output(&mut self) {
        match self.o.format {
            Format::Text => {}
            Format::Xml => {
                self.str.push_str("<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n");
                self.indent += 1;
            }
            Format::Json => {
                self.str.push('[');
                self.grouping.push(0);
                self.indent += 1;
            }
            Format::Yaml => self.grouping.push(0),
        }
    }

    /// ExplainEndOutput (explain.c:4906).
    fn end_output(&mut self) {
        match self.o.format {
            Format::Text => {}
            Format::Xml => {
                self.indent -= 1;
                self.str.push_str("</explain>");
            }
            Format::Json => {
                self.indent -= 1;
                self.str.push_str("\n]");
                self.grouping.pop();
            }
            Format::Yaml => {
                self.grouping.pop();
            }
        }
    }

    // ------------------------------------------------------------ deparsing

    /// An expression as deparse_expression writes it over the plan's range table (`prefix`: columns qualified by
    /// their relation's name; varno 0: an expression of no relation); a failure marks the output as not to be shown.
    fn deparse(&mut self, e: &PExpr, prefix: bool, want: Type) -> String {
        let rels: Vec<PlanRel> = match e.varno {
            0 => vec![],
            n => match self.p.rtable.get(n - 1) {
                Some(r) => vec![PlanRel {
                    shown: r.refname.clone(),
                    written: r.written.clone(),
                    cols: r.cols.clone(),
                    colls: r.colls.clone(),
                }],
                None => {
                    self.failed = true;
                    return String::new();
                }
            },
        };
        match plan_expr(&e.expr, &rels, prefix, want) {
            Some(s) => s,
            None => {
                self.failed = true;
                String::new()
            }
        }
    }

    /// A target-list entry as show_plan_tlist writes it.
    fn target(&mut self, t: &Target, prefix: bool) -> String {
        match t {
            Target::Var(v) => {
                let Some(r) = self.p.rtable.get(v.varno.wrapping_sub(1)) else {
                    self.failed = true;
                    return String::new();
                };
                let Some(c) = r.cols.get(v.attno.wrapping_sub(1)) else {
                    self.failed = true;
                    return String::new();
                };
                match prefix {
                    true => format!("{}.{}", ident(&r.refname), ident(&c.0)),
                    false => ident(&c.0),
                }
            }
            Target::Expr(e) => self.deparse(e, prefix, Type::Text),
        }
    }

    /// show_qual (explain.c:2329): the conditions AND-ed (make_ands_explicit), under `label`.
    fn qual(&mut self, qual: &[PExpr], label: &str, prefix: bool) {
        let Some(first) = qual.first() else { return };
        let mut e = first.expr.clone();
        for q in &qual[1..] {
            e = bin(e, Op::And, q.expr.clone());
        }
        let text = self.deparse(&PExpr { expr: e, varno: first.varno }, prefix, Type::Bool);
        self.text(label, &text);
    }

    // ------------------------------------------------------------ the nodes

    /// ExplainTargetRel (explain.c:3801): ` on relname refname` in text, its properties otherwise.
    fn target_rel(&mut self, varno: usize) {
        let Some(r) = self.p.rtable.get(varno.wrapping_sub(1)).cloned() else {
            self.failed = true;
            return;
        };
        let namespace = self.o.verbose.then_some(r.schema.as_str());
        match self.o.format {
            Format::Text => {
                self.str.push_str(" on");
                match namespace {
                    Some(n) => self.str.push_str(&format!(" {}.{}", ident(n), ident(&r.relname))),
                    None => self.str.push_str(&format!(" {}", ident(&r.relname))),
                }
                if r.refname != r.relname {
                    self.str.push_str(&format!(" {}", ident(&r.refname)));
                }
            }
            _ => {
                self.text("Relation Name", &r.relname);
                if let Some(n) = namespace {
                    self.text("Schema", n);
                }
                self.text("Alias", &r.refname);
            }
        }
    }

    /// show_buffer_usage (explain.c:3543) for counts that are all 0: Chronos reads no shared buffers, so text shows
    /// nothing and the other formats show the zeros (and the I/O times with track_io_timing).
    fn buffers(&mut self, io_timing: bool) {
        if self.o.format == Format::Text {
            return;
        }
        for label in [
            "Shared Hit Blocks",
            "Shared Read Blocks",
            "Shared Dirtied Blocks",
            "Shared Written Blocks",
            "Local Hit Blocks",
            "Local Read Blocks",
            "Local Dirtied Blocks",
            "Local Written Blocks",
            "Temp Read Blocks",
            "Temp Written Blocks",
        ] {
            self.integer(label, None, 0);
        }
        if io_timing {
            for label in ["I/O Read Time", "I/O Write Time", "Temp I/O Read Time", "Temp I/O Write Time"] {
                self.float(label, Some("ms"), 0.0, 3);
            }
        }
    }

    /// show_wal_usage (explain.c:3705) for no WAL written (Chronos writes none of Postgres's).
    fn wal(&mut self) {
        if self.o.format == Format::Text {
            return;
        }
        self.integer("WAL Records", None, 0);
        self.integer("WAL FPI", None, 0);
        self.integer("WAL Bytes", None, 0);
    }

    /// ExplainNode (explain.c:1178).
    fn node(&mut self, plan: &Plan, relationship: Option<&str>, io_timing: bool) {
        let save_indent = self.indent;
        let (pname, sname, strategy, partial, operation): (String, &str, _, _, _) = match &plan.node {
            Node::Result { .. } => ("Result".into(), "Result", None, None, None),
            Node::SeqScan { .. } => ("Seq Scan".into(), "Seq Scan", None, None, None),
            Node::Append => ("Append".into(), "Append", None, None, None),
            Node::Sort { .. } => ("Sort".into(), "Sort", None, None, None),
            Node::Limit => ("Limit".into(), "Limit", None, None, None),
            Node::Agg { strategy: AggStrategy::Plain } => {
                ("Aggregate".into(), "Aggregate", Some("Plain"), Some("Simple"), None)
            }
            Node::ModifyTable { op, .. } => {
                let o = match op {
                    CmdType::Update => "Update",
                    CmdType::Delete => "Delete",
                };
                (o.into(), "ModifyTable", None, None, Some(o))
            }
            Node::Step(s) => (s.name.clone(), "", None, None, None),
        };
        let sname = if let Node::Step(s) = &plan.node { s.name.clone() } else { sname.to_string() };
        self.open_group("Plan", if relationship.is_some() { None } else { Some("Plan") }, true);
        if self.o.format == Format::Text {
            if self.indent > 0 {
                self.indent_text();
                self.str.push_str("->  ");
                self.indent += 2;
            }
            self.str.push_str(&pname);
            self.indent += 1;
        } else {
            self.text("Node Type", &sname);
            if let Some(s) = strategy {
                self.text("Strategy", s);
            }
            if let Some(p) = partial {
                self.text("Partial Mode", p);
            }
            if let Some(op) = operation {
                self.text("Operation", op);
            }
            if let Some(r) = relationship {
                self.text("Parent Relationship", r);
            }
            if !matches!(plan.node, Node::Step(_)) {
                self.boolean("Parallel Aware", false);
                self.boolean("Async Capable", false);
            }
        }
        match &plan.node {
            Node::SeqScan { varno } => self.target_rel(*varno),
            Node::ModifyTable { nominal, .. } => self.target_rel(*nominal),
            Node::Step(s) => match self.o.format {
                Format::Text => {
                    if let Some(t) = &s.target {
                        self.str.push_str(&format!(" on {t}"));
                    }
                    self.str.push_str(&s.suffix);
                }
                _ => {
                    if let Some(t) = &s.target {
                        self.text("Relation Name", t);
                    }
                }
            },
            _ => {}
        }
        if self.o.costs
            && let Some(c) = plan.cost
        {
            match self.o.format {
                Format::Text => self.str.push_str(&format!(
                    "  (cost={:.2}..{:.2} rows={:.0} width={})",
                    c.startup, c.total, c.rows, c.width
                )),
                _ => {
                    self.float("Startup Cost", None, c.startup, 2);
                    self.float("Total Cost", None, c.total, 2);
                    self.float("Plan Rows", None, c.rows, 0);
                    self.integer("Plan Width", None, i64::from(c.width));
                }
            }
        }
        if self.o.analyze
            && let Some(a) = plan.actual
        {
            if a.loops > 0.0 {
                let (startup, total, rows) = (a.startup_ms / a.loops, a.total_ms / a.loops, a.rows / a.loops);
                match self.o.format {
                    Format::Text if self.o.timing => self.str.push_str(&format!(
                        " (actual time={startup:.3}..{total:.3} rows={rows:.0} loops={:.0})",
                        a.loops
                    )),
                    Format::Text => self.str.push_str(&format!(" (actual rows={rows:.0} loops={:.0})", a.loops)),
                    _ => {
                        if self.o.timing {
                            self.float("Actual Startup Time", Some("ms"), startup, 3);
                            self.float("Actual Total Time", Some("ms"), total, 3);
                        }
                        self.float("Actual Rows", None, rows, 0);
                        self.float("Actual Loops", None, a.loops, 0);
                    }
                }
            } else {
                match self.o.format {
                    Format::Text => self.str.push_str(" (never executed)"),
                    _ => {
                        if self.o.timing {
                            self.float("Actual Startup Time", Some("ms"), 0.0, 3);
                            self.float("Actual Total Time", Some("ms"), 0.0, 3);
                        }
                        self.float("Actual Rows", None, 0.0, 0);
                        self.float("Actual Loops", None, 0.0, 0);
                    }
                }
            }
        }
        if self.o.format == Format::Text {
            self.str.push('\n');
        }
        // (Chronos's steps: what each found, in the other formats)
        if let Node::Step(s) = &plan.node
            && self.o.format != Format::Text
        {
            if let Some(n) = s.actual_rows {
                self.integer("Actual Rows", None, n as i64);
            }
            if let Some(n) = s.found_rows {
                self.integer("Rows Found", None, n as i64);
            }
            if let Some(n) = s.loops {
                self.integer("Loops", None, n as i64);
            }
        }
        // show_plan_tlist (explain.c:2248): not an Append's
        if self.o.verbose
            && !matches!(plan.node, Node::Append | Node::Step(_))
            && let Some(tl) = &plan.tlist
            && !tl.is_empty()
        {
            let prefix = self.p.rtable.len() > 1;
            let items: Vec<String> = tl.iter().map(|t| self.target(t, prefix)).collect();
            self.list("Output", &items);
        }
        let upper_prefix = self.p.rtable.len() > 1 || self.o.verbose;
        match &plan.node {
            Node::SeqScan { .. } => {
                self.qual(&plan.qual, "Filter", self.o.verbose);
                if !plan.qual.is_empty() {
                    self.instrumentation_count("Rows Removed by Filter", plan.actual.map(|a| (a.filtered, a.loops)));
                }
            }
            Node::Result { one_time } => {
                if let Some(c) = one_time {
                    self.qual(std::slice::from_ref(c), "One-Time Filter", upper_prefix);
                }
                self.qual(&plan.qual, "Filter", upper_prefix);
            }
            Node::Sort { keys } => {
                // show_sort_group_keys (explain.c:2578): an expression the node below computes is shown as the
                // Var of its output, in parentheses; then show_sortorder_options (explain.c:2640): a collation other
                // than its type's default (the default collation, for text), the direction, where nulls go
                let mut shown = vec![];
                for k in keys {
                    let text = self.deparse(&k.expr, upper_prefix, Type::Text);
                    let mut text = if matches!(k.expr.expr, Expr::Col(..)) { text } else { format!("({text})") };
                    // (citext's is its type's own)
                    if let Some(c) = k.coll
                        && c != collation::Coll::DEFAULT
                        && c != collation::citext()
                    {
                        text.push_str(&format!(" COLLATE {}", ident(&c.name().0)));
                    }
                    if k.desc {
                        text.push_str(" DESC");
                    }
                    if k.nulls_first && !k.desc {
                        text.push_str(" NULLS FIRST");
                    } else if !k.nulls_first && k.desc {
                        text.push_str(" NULLS LAST");
                    }
                    shown.push(text);
                }
                self.list("Sort Key", &shown);
            }
            Node::Agg { .. } => self.qual(&plan.qual, "Filter", upper_prefix),
            Node::ModifyTable { op, nominal, targets } => {
                // show_modifytable_info (explain.c:3924): the tables written, where they aren't the one named
                let label = targets.len() > 1 || (targets.len() == 1 && targets[0] != *nominal);
                if label {
                    self.open_group("Target Tables", Some("Target Tables"), false);
                    for t in targets {
                        self.open_group("Target Table", None, true);
                        if self.o.format == Format::Text {
                            self.indent_text();
                            self.str.push_str(match op {
                                CmdType::Update => "Update",
                                CmdType::Delete => "Delete",
                            });
                        }
                        self.target_rel(*t);
                        if self.o.format == Format::Text {
                            self.str.push('\n');
                        }
                        self.close_group("Target Table", true);
                    }
                    self.close_group("Target Tables", false);
                }
            }
            Node::Step(s) => {
                if let Some((label, cond)) = &s.cond {
                    self.text(label, cond);
                }
            }
            Node::Append | Node::Limit => {}
        }
        if self.o.buffers && !matches!(plan.node, Node::Step(_)) {
            self.buffers(io_timing);
        }
        if self.o.wal && !matches!(plan.node, Node::Step(_)) {
            self.wal();
        }
        // ExplainMissingMembers (explain.c:4154): nothing is pruned when the plan runs
        if matches!(plan.node, Node::Append) && self.o.format != Format::Text {
            self.integer("Subplans Removed", None, 0);
        }
        let has_children = !plan.kids.is_empty() || matches!(plan.node, Node::Append);
        if has_children {
            self.open_group("Plans", Some("Plans"), false);
            let rel = match plan.node {
                Node::Append => "Member",
                _ => "Outer",
            };
            for k in &plan.kids {
                self.node(k, Some(rel), io_timing);
            }
            self.close_group("Plans", false);
        }
        if self.o.format == Format::Text {
            self.indent = save_indent;
        }
        self.close_group("Plan", true);
    }

    /// show_instrumentation_count (explain.c): under ANALYZE, zero not shown in text.
    fn instrumentation_count(&mut self, label: &str, counts: Option<(f64, f64)>) {
        if !self.o.analyze {
            return;
        }
        let Some((filtered, loops)) = counts else { return };
        if filtered > 0.0 || self.o.format != Format::Text {
            let v = if loops > 0.0 { filtered / loops } else { 0.0 };
            self.float(label, None, v, 0);
        }
    }
}

/// EXPLAIN's output for `p`, as ExplainQuery and ExplainOnePlan (explain.c:164, 527) make it: one string, its lines
/// the text format's rows. None where a part of the plan can't be written (an expression this doesn't deparse).
pub(super) fn print(p: &PlannedStmt, o: &ExplainOpts, s: &Summary) -> Option<String> {
    let mut es = Es { o, str: String::new(), indent: 0, grouping: vec![], p, failed: false };
    es.begin_output();
    es.open_group("Query", None, true);
    // (Chronos's steps may be several, one after another: in a format with one plan, under one node)
    let roots: Vec<Plan> = if p.roots.len() > 1 && o.format != Format::Text {
        vec![Plan { kids: p.roots.clone(), ..Plan::new(Node::Step(Step { name: "Steps".into(), ..Step::default() })) }]
    } else {
        p.roots.clone()
    };
    for r in &roots {
        es.node(r, None, s.io_timing);
    }
    // ExplainPrintSettings (explain.c:682)
    if o.settings {
        if o.format != Format::Text {
            es.open_group("Settings", Some("Settings"), true);
            for (n, v) in &s.settings {
                es.text(n, v);
            }
            es.close_group("Settings", true);
        } else if !s.settings.is_empty() {
            let shown: Vec<String> = s.settings.iter().map(|(n, v)| format!("{n} = '{v}'")).collect();
            es.text("Settings", &shown.join(", "));
        }
    }
    if o.verbose
        && let Some(id) = s.query_id
    {
        es.integer("Query Identifier", None, id);
    }
    if o.buffers {
        es.open_group("Planning", Some("Planning"), true);
        es.buffers(s.io_timing);
        es.close_group("Planning", true);
    }
    if o.summary
        && let Some(ms) = s.planning_ms
    {
        es.float("Planning Time", Some("ms"), ms, 3);
    }
    if o.analyze {
        es.open_group("Triggers", Some("Triggers"), false);
        es.close_group("Triggers", false);
    }
    if o.summary
        && o.analyze
        && let Some(ms) = s.execution_ms
    {
        es.float("Execution Time", Some("ms"), ms, 3);
    }
    es.close_group("Query", true);
    es.end_output();
    if es.failed {
        return None;
    }
    let mut out = es.str;
    if o.format == Format::Text && out.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

/// The settings EXPLAIN (SETTINGS) lists, get_explain_guc_options's (utils/misc/guc.c:5213): those marked
/// GUC_EXPLAIN (guc_tables.c) whose value isn't their default, listed in this order. (Postgres lists them in
/// guc_nondef_list's order, the one they first left their defaults in (set_guc_source, guc.c:2082): two set in
/// another order than this come out in another order than Postgres's.)
pub(super) const EXPLAIN_SETTINGS: &[&str] = &[
    "enable_seqscan",
    "enable_indexscan",
    "enable_indexonlyscan",
    "enable_bitmapscan",
    "enable_tidscan",
    "enable_sort",
    "enable_incremental_sort",
    "enable_hashagg",
    "enable_material",
    "enable_memoize",
    "enable_nestloop",
    "enable_mergejoin",
    "enable_hashjoin",
    "enable_gathermerge",
    "enable_partitionwise_join",
    "enable_partitionwise_aggregate",
    "enable_parallel_append",
    "enable_parallel_hash",
    "enable_partition_pruning",
    "enable_presorted_aggregate",
    "enable_async_append",
    "geqo",
    "parallel_leader_participation",
    "jit",
    "from_collapse_limit",
    "join_collapse_limit",
    "geqo_threshold",
    "geqo_effort",
    "geqo_pool_size",
    "geqo_generations",
    "temp_buffers",
    "work_mem",
    "effective_io_concurrency",
    "maintenance_io_concurrency",
    "max_parallel_workers_per_gather",
    "max_parallel_workers",
    "effective_cache_size",
    "min_parallel_table_scan_size",
    "min_parallel_index_scan_size",
    "seq_page_cost",
    "random_page_cost",
    "cpu_tuple_cost",
    "cpu_index_tuple_cost",
    "cpu_operator_cost",
    "parallel_tuple_cost",
    "parallel_setup_cost",
    "jit_above_cost",
    "jit_optimize_above_cost",
    "jit_inline_above_cost",
    "cursor_tuple_fraction",
    "recursive_worktable_factor",
    "geqo_selection_bias",
    "geqo_seed",
    "hash_mem_multiplier",
    "search_path",
    "constraint_exclusion",
    "debug_parallel_query",
    "plan_cache_mode",
];
