//! pg_get_viewdef, pg_views.definition and psql's `\d+` "View definition:": a view's query written back as Postgres's
//! ruleutils.c writes the analysed query, not as it was typed. Every column is qualified with the name of the
//! relation it comes from where the query reads more than one (or sits inside another), `*` is the columns it stands
//! for, constants carry their types (`'foo'::text`), casts are `::type`, operations are parenthesized (or, pretty,
//! only where precedence needs it), and each select-list and FROM item goes on a line of its own, indented as
//! Postgres indents. A query of a form this doesn't write back gives None, and the caller shows the text as it was
//! written. A trigger's WHEN (pg_get_triggerdef) is written the same way.

use super::*;
use std::sync::LazyLock;

const STD: i32 = 8; // PRETTYINDENT_STD
const JOIN_IND: i32 = 4; // PRETTYINDENT_JOIN
const VAR_IND: i32 = 4; // PRETTYINDENT_VAR
const LIMIT_IND: i32 = 40; // PRETTYINDENT_LIMIT

/// View `i` of `rels` as pg_get_viewdef writes it (`pretty`: with only the parentheses precedence needs; `wrap`, the
/// line length the select list wraps at, 0 for an item a line): Postgres's text where its query is one this writes
/// back, else its query as it was written, in Postgres's ` ...;`.
pub(super) fn shown(rels: &[TableInfo], i: usize, pretty: bool, wrap: i32) -> Option<String> {
    let r = rels.get(i)?;
    let def = r.definition.as_deref()?;
    Some(view_text(rels, i, pretty, wrap).unwrap_or_else(|| format!(" {};", def.trim().trim_end_matches(';'))))
}

/// View `i`'s query as Postgres writes it back; None for a form this doesn't know.
fn view_text(rels: &[TableInfo], i: usize, pretty: bool, wrap: i32) -> Option<String> {
    let r = rels.get(i)?;
    let q = view_query(&r.name, r.definition.as_deref()?).ok()?;
    let names: Vec<String> = r.columns.iter().map(|c| c.0.clone()).collect();
    let mut d = Dp::new(rels, pretty, wrap);
    d.query(&q, Some(&names[..]), true, 0)?;
    d.buf.push(';');
    Some(d.buf)
}

/// A trigger's WHEN condition (as stored) as pg_get_triggerdef writes it, over OLD and NEW, each a row of the table's
/// columns `cols` (name, type, and its type id and modifier as `TableInfo::typmods` keeps them); None for a form this
/// doesn't write back.
pub(super) fn trigger_when(text: &str, cols: &[(String, Type, (i64, i64))], pretty: bool) -> Option<String> {
    let e = check_expr(text).ok()?;
    let mut d = Dp::new(&[], pretty, 0);
    let mut lv = Level::empty(None, true);
    for side in ["old", "new"] {
        lv.rtes.push(Rte {
            written: side.into(),
            refname: Some(side.into()),
            cols: cols.iter().map(|c| c.0.clone()).collect(),
            types: cols.iter().map(|c| Ty::with(c.1, c.2)).collect(),
            via: vec![Via::Own; cols.len()],
            rel: None,
        });
    }
    lv.prefix = true;
    d.levels.push(lv);
    let t = d.tn(&e)?;
    let n = d.settle(t, BOOL);
    d.emit(&n, false)?;
    Some(d.buf)
}

/// A relation a plan's expression reads (see `plan_expr`): the name EXPLAIN shows it by, the name the query calls
/// it, and its columns as `col_ty` gives them.
pub(super) struct PlanRel {
    pub(super) shown: String,
    pub(super) written: String,
    pub(super) cols: Vec<(String, Type, (i64, i64))>,
}

/// An expression of a plan as EXPLAIN writes it (ruleutils.c's deparse_expression over the plan's range table): a
/// column bare, or named by its relation where `prefix`; constants as the planner has folded them (`1::bigint` is
/// `'1'::bigint`, an IN list's constants one array constant). An untyped literal standing alone takes type `want`.
/// None for a form this doesn't write.
pub(super) fn plan_expr(e: &Expr, rels: &[PlanRel], prefix: bool, want: Type) -> Option<String> {
    let mut d = Dp::new(&[], false, 0);
    d.fold = true;
    let mut lv = Level::empty(None, true);
    for (i, r) in rels.iter().enumerate() {
        lv.rtes.push(Rte {
            written: r.written.clone(),
            refname: Some(r.shown.clone()),
            cols: r.cols.iter().map(|c| c.0.clone()).collect(),
            types: r.cols.iter().map(|c| Ty::with(c.1, c.2)).collect(),
            via: vec![Via::Own; r.cols.len()],
            rel: None,
        });
        lv.from.push(Node::Rel { rte: i, name: r.shown.clone(), only: false, aliased: false, cols_given: false });
    }
    lv.prefix = prefix;
    d.levels.push(lv);
    let t = d.tn(e)?;
    let n = d.settle(t, Ty::of(want));
    d.emit(&n, false)?;
    Some(d.buf)
}

/// A table column's type as `trigger_when` takes it.
pub(super) fn col_ty(c: &Col) -> (String, Type, (i64, i64)) {
    let m = match (c.chars, c.numeric) {
        (Some(chars), _) if matches!(c.ty, Type::Text | Type::Array(25)) => {
            super::chars_oid(Some(chars), c.ty != Type::Text).map_or((0, -1), |(o, m)| (i64::from(o), i64::from(m)))
        }
        (_, Some((p, s))) => (1700, (i64::from(p) << 16 | (i64::from(s) & 0x7ff)) + 4),
        _ if c.small => (21, -1),
        _ => (0, -1),
    };
    (c.name.clone(), c.ty, m)
}

/// The views of Postgres's own catalog (pg_catalog's), by name, each with its definition as pg_get_viewdef writes it:
/// what pg_views shows for them (rules.out's listing, `name| SELECT ...;`).
pub(super) fn system_views() -> &'static [(String, String)] {
    static VIEWS: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
        let mut out: Vec<(String, String)> = vec![];
        for line in include_str!("system_views.txt").lines() {
            let start = line.split_once("| ").filter(|(n, _)| {
                !n.is_empty() && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            });
            match start {
                Some((n, rest)) => out.push((n.to_string(), format!(" {rest}"))),
                None => {
                    if let Some(last) = out.last_mut() {
                        last.1.push('\n');
                        last.1.push_str(line);
                    }
                }
            }
        }
        out
    });
    &VIEWS
}

/// pg_settings's rules, as pg_rules shows them: (name, definition).
pub(super) const SETTINGS_RULES: [(&str, &str); 2] = [
    ("pg_settings_n", "CREATE RULE pg_settings_n AS\n    ON UPDATE TO pg_catalog.pg_settings DO INSTEAD NOTHING;"),
    (
        "pg_settings_u",
        "CREATE RULE pg_settings_u AS\n    ON UPDATE TO pg_catalog.pg_settings\n   WHERE (new.name = old.name) DO  SELECT set_config(old.name, new.setting, false) AS set_config;",
    ),
];

// ---------------------------------------------------------------- types

/// A type as Postgres has it: Chronos's type, and the type id and modifier `TableInfo::typmods` keeps for one that
/// isn't its type's own (`character varying(10)` is (1043, 14), `numeric(10,2)` (1700, ...), smallint (21, -1)).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Ty {
    t: Type,
    m: (i64, i64),
}

const TEXT: Ty = Ty { t: Type::Text, m: (0, -1) };
const BOOL: Ty = Ty { t: Type::Bool, m: (0, -1) };
const INT4: Ty = Ty { t: Type::Int, m: (0, -1) };
const INT8: Ty = Ty { t: Type::BigInt, m: (0, -1) };
const NUMERIC: Ty = Ty { t: Type::Numeric, m: (0, -1) };
const FLOAT8: Ty = Ty { t: Type::Float, m: (0, -1) };
const FLOAT4: Ty = Ty { t: Type::Float4, m: (0, -1) };

impl Ty {
    fn of(t: Type) -> Ty {
        Ty { t, m: (0, -1) }
    }

    fn with(t: Type, m: (i64, i64)) -> Ty {
        // (a smallint column is an integer kept to 16 bits)
        let t = if m.0 == 21 { Type::SmallInt } else { t };
        Ty { t, m }
    }

    fn varchar(self) -> bool {
        self.t == Type::Text && self.m.0 == 1043
    }

    fn bpchar(self) -> bool {
        self.t == Type::Text && self.m.0 == 1042
    }

    /// The type without its modifier (`character varying`, `numeric`, `bpchar`).
    fn base(self) -> Ty {
        Ty { m: (self.m.0, -1), ..self }
    }

    /// Whether `self` and `o` are one type, maybe of other modifiers.
    fn same_base(self, o: Ty) -> bool {
        let kind = |t: Ty| if t.t == Type::Text { t.m.0 } else { 0 };
        self.t == o.t && kind(self) == kind(o)
    }

    /// format_type_with_typemod.
    fn name(self) -> String {
        match self.m {
            (1043, n) if self.t == Type::Text && n >= 4 => format!("character varying({})", n - 4),
            (1043, _) if self.t == Type::Text => "character varying".into(),
            (1042, n) if self.t == Type::Text && n >= 4 => format!("character({})", n - 4),
            (1042, _) if self.t == Type::Text => "bpchar".into(),
            // (an array of them: its elements' name, with their length)
            (o @ (1015 | 1014), n) if self.t == Type::Array(25) => {
                format!("{}[]", Ty { t: Type::Text, m: (if o == 1015 { 1043 } else { 1042 }, n) }.name())
            }
            (1700, n) if self.t == Type::Numeric && n >= 4 => {
                format!("numeric({},{})", (n - 4) >> 16, (((n - 4) & 0x7ff) ^ 1024) - 1024)
            }
            _ => match self.t {
                Type::Enum(_) | Type::Record(_) => shown_name(&self.t.name()),
                Type::Array(e) => match Type::from_oid(e) {
                    Some(et) if !matches!(et, Type::Array(_)) => format!("{}[]", Ty::of(et).name()),
                    _ => self.t.name().into_owned(),
                },
                t => t.pg_name().into_owned(),
            },
        }
    }

    /// Its place among the numbers, for the casts an operator's operands take: an integer 1, numeric 2, real 3, double
    /// precision 4.
    fn rank(self) -> Option<u8> {
        Some(match self.t {
            Type::SmallInt | Type::Int | Type::BigInt => 1,
            Type::Numeric => 2,
            Type::Float4 => 3,
            Type::Float => 4,
            _ => return None,
        })
    }
}

/// The wider of two integer types.
fn wider_int(a: Ty, b: Ty) -> Ty {
    let w = |t: Ty| match t.t {
        Type::BigInt => 3,
        Type::Int => 2,
        _ => 1,
    };
    if w(b) > w(a) { b } else { a }
}

/// The common type of values of these types (None: an untyped literal), as select_common_type finds it: numbers widen,
/// text wins over the other character types; None where all are untyped.
fn common(tys: impl Iterator<Item = Option<Ty>>) -> Option<Ty> {
    let mut c: Option<Ty> = None;
    for t in tys.flatten() {
        c = Some(match c {
            None => t,
            Some(c) => match (c.rank(), t.rank()) {
                (Some(1), Some(1)) => wider_int(c, t),
                (Some(a), Some(b)) if b > a => t,
                (Some(_), Some(_)) => c,
                _ if c.t == Type::Text && t.t == Type::Text && !c.same_base(t) => TEXT,
                _ if c.same_base(t) && c.m != t.m => c.base(),
                _ => c,
            },
        });
    }
    c
}

/// How a cast is made, which says whether its operand is parenthesized (see `Dp::simple`): a function (`int4` to
/// `numeric`), a relabeling of a binary-compatible type (`varchar` to `text`), or the types' text forms.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CastKind {
    Func,
    Relabel,
    Io,
}

fn cast_kind(from: Option<Ty>, to: Ty) -> CastKind {
    match from {
        Some(f) if f.t == Type::Text && to.t == Type::Text && !f.bpchar() => CastKind::Relabel,
        Some(f) if f.t == Type::Text && to.t == Type::Text => CastKind::Func,
        Some(f) if matches!(to.t, Type::Array(_)) && matches!(f.t, Type::Array(_)) => CastKind::Relabel,
        Some(f) if f.t == Type::Text || to.t == Type::Text => CastKind::Io,
        _ => CastKind::Func,
    }
}

/// A constant of type `ty` as get_const_expr writes it: an integer and a number with a point bare, a boolean as
/// `true`, anything else quoted and labeled with its type.
fn konst<'q>(v: &Value, ty: Ty) -> N<'q> {
    let name = ty.name();
    let quoted = |s: &str| format!("'{}'", s.replace('\'', "''"));
    match v {
        Value::Null => N::Const { text: "NULL".into(), label: Some(name.clone()), ty: name },
        Value::Bool(b) => N::Const { text: b.to_string(), label: None, ty: name },
        Value::Int(n) if ty.t == Type::Int => match *n >= 0 {
            true => N::Const { text: n.to_string(), label: None, ty: name },
            false => N::Const { text: quoted(&n.to_string()), label: Some(name.clone()), ty: name },
        },
        v if ty.t == Type::Numeric => {
            let s = v.to_text();
            let bare = s.starts_with(|c: char| c.is_ascii_digit()) && s.contains(['.', 'e', 'E']);
            match (bare, ty.m.1 >= 0) {
                (true, false) => N::Const { text: s, label: None, ty: name },
                (true, true) => N::Const { text: s, label: Some(name.clone()), ty: name },
                (false, _) => N::Const { text: quoted(&s), label: Some(name.clone()), ty: name },
            }
        }
        v => N::Const { text: quoted(&v.to_text()), label: Some(name.clone()), ty: name },
    }
}

// ---------------------------------------------------------------- expressions, as Postgres's nodes

#[derive(Clone, Copy, Debug, PartialEq)]
enum BoolOp {
    And,
    Or,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SubKind {
    Exists,
    Any,
    All,
    Expr,
}

/// An expression as the parse tree Postgres would have made of it, with its implicit casts and the types of its
/// constants: what get_rule_expr writes.
#[derive(Clone, Debug)]
enum N<'q> {
    /// a column, written out (`t.a`)
    Var(String),
    /// a whole row, `t.*`, and its row type's name (shown after it at the top of a list: `t.*::t`)
    Row(String, String),
    /// a constant: its text, the type it is labeled with (`'foo'` and `text`), and its type's name
    Const {
        text: String,
        label: Option<String>,
        ty: String,
    },
    /// an operator: its symbol and one (prefix) or two operands
    Op(String, Vec<N<'q>>),
    Bool(BoolOp, Vec<N<'q>>),
    /// a function called by name
    Func(String, Vec<N<'q>>),
    /// an argument given by name: `name => value`
    Named(String, Box<N<'q>>),
    Agg(Box<AggN<'q>>),
    Win(Box<WinN<'q>>),
    /// a cast: the operand, the type's name, how it is made, and whether it is implicit
    Cast(Box<N<'q>>, String, CastKind, bool),
    Case(Option<Box<N<'q>>>, Vec<(N<'q>, N<'q>)>, Box<N<'q>>),
    /// COALESCE(, GREATEST(, LEAST(, NULLIF( and their arguments
    Keyed(&'static str, Vec<N<'q>>),
    NullTest(Box<N<'q>>, bool),
    /// x IS [NOT] TRUE | FALSE
    BoolTest(Box<N<'q>>, &'static str),
    Distinct(Box<N<'q>>, Box<N<'q>>),
    /// x op ANY | ALL (array): ALL when true
    ArrayOp(Box<N<'q>>, String, bool, Box<N<'q>>),
    Array(Vec<N<'q>>),
    RowExpr(Vec<N<'q>>),
    /// a subquery: how it is read, what it is compared with (and by which operator), and its query
    Sub(SubKind, Option<(Box<N<'q>>, String)>, &'q Select),
    /// a keyword that is a value: CURRENT_DATE, CURRENT_USER
    Word(&'static str),
    Collate(Box<N<'q>>, String),
    Subscript(Box<N<'q>>, Vec<N<'q>>),
    Field(Box<N<'q>>, String),
    /// x AT TIME ZONE zone
    AtZone(Box<N<'q>>, Box<N<'q>>),
    /// EXTRACT(field FROM x)
    Extract(String, Box<N<'q>>),
}

#[derive(Clone, Debug)]
struct AggN<'q> {
    name: String,
    distinct: bool,
    star: bool,
    args: Vec<N<'q>>,
    /// an ordered-set aggregate: `args` are its direct arguments, `order` its WITHIN GROUP
    within: bool,
    order: Vec<(N<'q>, Dir)>,
    filter: Option<N<'q>>,
}

#[derive(Clone, Debug)]
struct WinN<'q> {
    name: String,
    star: bool,
    args: Vec<N<'q>>,
    partition: Vec<N<'q>>,
    order: Vec<(N<'q>, Dir)>,
    frame: Option<String>,
}

/// An expression built (see `Dp::tn`), its type, and for an untyped literal (a quoted string, NULL) its value, which
/// takes the type its place gives it.
#[derive(Clone, Debug)]
struct Tn<'q> {
    n: N<'q>,
    ty: Option<Ty>,
    raw: Option<Value>,
}

impl<'q> Tn<'q> {
    fn typed(n: N<'q>, ty: Ty) -> Tn<'q> {
        Tn { n, ty: Some(ty), raw: None }
    }
}

// ---------------------------------------------------------------- the range table

/// A range-table entry: a relation, subquery, function or join a query reads, and the names it shows.
#[derive(Clone, Debug)]
struct Rte {
    /// what the query calls it (its alias, or the relation's name); empty for a join without an alias
    written: String,
    /// the name it is shown by, unique among those of its query and the queries around it (set_rtable_names);
    /// None for a join without an alias
    refname: Option<String>,
    cols: Vec<String>,
    types: Vec<Ty>,
    /// a join's columns: what each one is
    via: Vec<Via>,
    /// a relation's stored name (its row type's)
    rel: Option<String>,
}

/// What a join's column is: a column of one of its sides, or (a FULL JOIN's merged column) its bare name.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Via {
    Own,
    Col(usize, usize),
    Bare,
}

/// A FROM item.
#[derive(Clone, Debug)]
enum Node<'q> {
    Rel {
        rte: usize,
        name: String,
        only: bool,
        aliased: bool,
        cols_given: bool,
    },
    Sub {
        rte: usize,
        q: &'q Select,
        lateral: bool,
        cols_given: bool,
    },
    Func {
        rte: usize,
        name: String,
        args: &'q [Expr],
    },
    Join {
        rte: usize,
        kind: JoinKind,
        left: Box<Node<'q>>,
        right: Box<Node<'q>>,
        on: Option<&'q Expr>,
        using: Vec<String>,
        named: bool,
        cols_given: bool,
    },
}

impl Node<'_> {
    fn rte(&self) -> usize {
        match self {
            Node::Rel { rte, .. } | Node::Sub { rte, .. } | Node::Func { rte, .. } | Node::Join { rte, .. } => *rte,
        }
    }
}

/// A select-list item: a column (levels out, entry, column), or an expression.
#[derive(Clone, Debug)]
enum Out<'q> {
    Var(usize, usize, usize),
    Expr(&'q Expr),
}

/// One query's namespace, and what its context (deparse_context) says while it is written.
#[derive(Clone, Debug)]
struct Level<'q> {
    rtes: Vec<Rte>,
    from: Vec<Node<'q>>,
    /// columns are qualified (varprefix): it reads other than one relation, or is inside another query
    prefix: bool,
    /// its select list: each item's name and what it is
    out: Vec<(String, Out<'q>)>,
    /// the view's column names (resultDesc), which name the output
    desc: Option<Vec<String>>,
    /// colNamesVisible
    visible: bool,
}

impl Level<'_> {
    fn empty(desc: Option<&[String]>, visible: bool) -> Self {
        Level { rtes: vec![], from: vec![], prefix: true, out: vec![], desc: desc.map(<[String]>::to_vec), visible }
    }
}

/// A key of ORDER BY, GROUP BY, DISTINCT ON or a window: a column, or an expression.
#[derive(Clone, Debug)]
enum Key<'q> {
    Var(usize, usize, usize),
    Node(N<'q>),
}

/// UNION / INTERSECT / EXCEPT, as a tree.
enum SetTree<'q> {
    Leaf(&'q Select),
    Op(SetOp, bool, Box<SetTree<'q>>, Box<SetTree<'q>>),
}

fn set_tree(q: &Select) -> SetTree<'_> {
    let mut t = SetTree::Leaf(q);
    for (op, all, part) in &q.set {
        let right = if part.set.is_empty() { SetTree::Leaf(part) } else { set_tree(part) };
        t = SetTree::Op(*op, *all, Box::new(t), Box::new(right));
    }
    t
}

/// Whether `q` is a VALUES list as the parser makes one: a row of `column1, column2, ...` and the other rows UNION
/// ALL-ed to it, none reading a table.
fn values_shaped(q: &Select) -> bool {
    let row = |s: &Select| {
        s.from.is_none()
            && s.joins.is_empty()
            && s.filter.is_none()
            && s.group.is_empty()
            && s.having.is_none()
            && !s.distinct
            && !s.items.is_empty()
            && s.items
                .iter()
                .enumerate()
                .all(|(i, it)| matches!(it, Item::Expr(_, Some(a)) if *a == format!("column{}", i + 1)))
    };
    row(q)
        && q.order.is_empty()
        && q.limit.is_none()
        && q.offset.is_none()
        && q.set.iter().all(|(op, all, p)| {
            *op == SetOp::Union && *all && p.set.is_empty() && row(p) && p.items.len() == q.items.len()
        })
}

/// Whether a UNION ALL's parts are the grouping sets of one query, as the parser expands GROUP BY ROLLUP, CUBE and
/// GROUPING SETS (a query of each set, over the same FROM): not written back.
fn grouping_sets(q: &Select) -> bool {
    let src = |s: &Select| s.from.as_ref().map(|f| (f.table.clone(), f.alias.clone()));
    let grouped = |s: &Select| !s.group.is_empty() || matches!(s.having, Some(Expr::Lit(Value::Bool(true))));
    !q.set.is_empty()
        && grouped(q)
        && q.set.iter().all(|(op, all, p)| *op == SetOp::Union && *all && grouped(p) && src(p) == src(q))
}

/// Whether `q` is what the parser makes of `ARRAY(SELECT ...)`: the query's one column aggregated into an array,
/// which is not written back.
fn array_query(q: &Select) -> bool {
    let agg = |e: &Expr| matches!(e, Expr::Agg(Agg::List(ListAgg::Array, _), ..));
    matches!(&q.items[..], [Item::Expr(Expr::Func(Func::Coalesce, a), _)]
        if a.len() == 2 && agg(&a[0]) && matches!(&a[1], Expr::Lit(Value::Text(t)) if t == "{}"))
}

/// The query of a parenthesized join used as one FROM item (`(a JOIN b USING (x)) j`), as the parser keeps it.
fn group_of(src: &Source) -> Option<&Select> {
    let s = src.sub.as_deref()?;
    let plain = s.set.is_empty()
        && matches!(s.items[..], [Item::Star])
        && !s.joins.is_empty()
        && s.filter.is_none()
        && s.group.is_empty()
        && s.having.is_none()
        && s.order.is_empty()
        && s.limit.is_none()
        && s.offset.is_none()
        && !s.distinct;
    (plain || src.alias == GROUP).then_some(s)
}

/// Whether `e` reads a column.
fn has_col(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| found |= matches!(x, Expr::Col(..)));
    found
}

/// A relation's stored name as a FROM item names it: bare where the search_path finds it so, else with its schema.
fn rel_text(name: &str) -> String {
    let s = shown_name(name);
    if s.contains('.') { s } else { quoted(&s) }
}

/// A function's stored name as a call writes it.
fn func_text(name: &str) -> String {
    let w = written(name);
    match w.split_once('.') {
        Some(("public" | "pg_catalog", f)) => quoted(f),
        Some(_) => {
            let s = shown_name(w);
            if s.contains('.') { s } else { quoted(&s) }
        }
        None => quoted(w),
    }
}

fn quoted(name: &str) -> String {
    collation::quoted(name)
}

fn trim_spaces(s: &mut String) {
    while s.ends_with(' ') {
        s.pop();
    }
}

/// An operator's symbol, as Postgres names it.
fn op_sym(op: Op) -> Option<&'static str> {
    Some(match op {
        Op::Arrow => "->",
        Op::ArrowText => "->>",
        Op::Path => "#>",
        Op::PathText => "#>>",
        Op::Has => "?",
        Op::HasAny => "?|",
        Op::HasAll => "?&",
        Op::CosDist => "<=>",
        Op::L2Dist => "<->",
        Op::NegDot => "<#>",
        Op::TsMatch => "@@",
        Op::ArrContains => "@>",
        Op::ArrContainedBy => "<@",
        Op::Like { ci: false, neg: false } => "~~",
        Op::Like { ci: true, neg: false } => "~~*",
        Op::Like { ci: false, neg: true } => "!~~",
        Op::Like { ci: true, neg: true } => "!~~*",
        op => return op_symbol(op),
    })
}

/// A window function's name.
fn win_name(f: WinFn) -> Option<&'static str> {
    Some(match f {
        WinFn::RowNumber => "row_number",
        WinFn::Rank => "rank",
        WinFn::DenseRank => "dense_rank",
        WinFn::PercentRank => "percent_rank",
        WinFn::CumeDist => "cume_dist",
        WinFn::Ntile => "ntile",
        WinFn::Lag => "lag",
        WinFn::Lead => "lead",
        WinFn::FirstValue => "first_value",
        WinFn::LastValue => "last_value",
        WinFn::NthValue => "nth_value",
        WinFn::Agg(a) => agg_name(&a),
        WinFn::Ctid => return None,
    })
}

// ---------------------------------------------------------------- the writer

/// What ruleutils.c's deparse_context holds while a query is written: the text so far, the indentation, and the
/// namespaces of the queries being written, innermost last.
struct Dp<'q> {
    rels: &'q [TableInfo],
    /// PRETTY_PAREN: only the parentheses precedence needs
    paren: bool,
    /// the wrap column (-1: none)
    wrap: i32,
    indent: i32,
    buf: String,
    levels: Vec<Level<'q>>,
    /// inside GROUP BY (inGroupBy)
    in_group: bool,
    /// writing a plan's expression (see `plan_expr`): constants as the planner folds them
    fold: bool,
}

impl<'q> Dp<'q> {
    fn new(rels: &'q [TableInfo], pretty: bool, wrap: i32) -> Self {
        Dp { rels, paren: pretty, wrap, indent: 0, buf: String::new(), levels: vec![], in_group: false, fold: false }
    }

    /// Where constants are folded (see `fold`): a cast of a constant as the constant of the type it is cast to.
    fn folded_cast(&self, n: &N<'q>, from: Option<Ty>, to: Ty) -> Option<N<'q>> {
        let N::Const { text, label, .. } = n else { return None };
        if !self.fold {
            return None;
        }
        let v = match (text.as_str(), label) {
            ("NULL", Some(_)) => Value::Null,
            (t, Some(_)) => {
                let raw = t.strip_prefix('\'')?.strip_suffix('\'')?.replace("''", "'");
                coerce(Value::Text(raw), from?.t).ok()?
            }
            (t, None) => coerce(Value::Text(t.to_string()), from?.t).ok()?,
        };
        let v = if v == Value::Null { v } else { coerce(v, to.t).ok()? };
        Some(konst(&v, to))
    }

    /// Where constants are folded: an ARRAY[...] of constants as one constant of the array type `label`
    /// (`'{a,b}'::text[]`), as array_out writes it.
    fn const_array(&self, items: &[N<'q>], label: &str) -> Option<N<'q>> {
        if !self.fold || items.is_empty() {
            return None;
        }
        let mut elems = vec![];
        for it in items {
            let N::Const { text, label: l, .. } = it else { return None };
            let raw = match (text.as_str(), l) {
                ("NULL", Some(_)) => {
                    elems.push("NULL".to_string());
                    continue;
                }
                (t, Some(_)) => t.strip_prefix('\'')?.strip_suffix('\'')?.replace("''", "'"),
                ("true", None) => "t".into(),
                ("false", None) => "f".into(),
                (t, None) => t.to_string(),
            };
            let plain = !raw.is_empty()
                && !raw.eq_ignore_ascii_case("null")
                && !raw.chars().any(|c| matches!(c, '"' | '\\' | '{' | '}' | ',') || c.is_whitespace());
            elems.push(if plain { raw } else { format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\"")) });
        }
        let text = format!("'{{{}}}'", elems.join(",").replace('\'', "''"));
        Some(N::Const { text, label: Some(label.to_string()), ty: label.to_string() })
    }

    /// appendContextKeyword: a line break and the indentation before `s` (PRETTY_INDENT is always on here).
    fn kw(&mut self, s: &str, before: i32, after: i32, plus: i32) {
        self.indent += before;
        trim_spaces(&mut self.buf);
        self.buf.push('\n');
        let amount = if self.indent < LIMIT_IND {
            self.indent.max(0) + plus
        } else {
            (LIMIT_IND + (self.indent - LIMIT_IND) / (STD / 2)) % LIMIT_IND + plus
        };
        for _ in 0..amount.max(0) {
            self.buf.push(' ');
        }
        self.buf.push_str(s);
        self.indent += after;
        if self.indent < 0 {
            self.indent = 0;
        }
    }

    /// The length of the line the text ends with.
    fn line_len(&self) -> usize {
        self.buf.rsplit('\n').next().map_or(0, str::len)
    }

    /// get_query_def: `q`, named by `desc` (a view's columns), its columns' names shown where `visible`, indented
    /// from `start`.
    fn query(&mut self, q: &'q Select, desc: Option<&[String]>, visible: bool, start: i32) -> Option<()> {
        let saved = self.indent;
        self.indent = start;
        let r = if values_shaped(q) {
            self.values_query(q, desc)
        } else if grouping_sets(q) {
            None
        } else if !q.set.is_empty() {
            self.set_query(q, desc, visible)
        } else {
            self.simple_query(q, desc, visible, true)
        };
        self.indent = saved;
        r
    }

    /// A query without UNION: its namespace in place while its parts are written (and, with `tail`, its ORDER BY
    /// and LIMIT).
    fn simple_query(&mut self, q: &'q Select, desc: Option<&[String]>, visible: bool, tail: bool) -> Option<()> {
        let mut lv = self.level(q)?;
        lv.desc = desc.map(<[String]>::to_vec);
        lv.visible = visible;
        self.levels.push(lv);
        let r = self.basic(q, tail);
        self.levels.pop();
        r
    }

    /// get_basic_select_query, then get_select_query_def's ORDER BY, OFFSET and LIMIT.
    fn basic(&mut self, q: &'q Select, tail: bool) -> Option<()> {
        let out = self.outputs(q)?;
        if let Some(lv) = self.levels.last_mut() {
            lv.out = out;
        }
        self.indent += STD;
        self.buf.push(' ');
        self.buf.push_str("SELECT");
        if q.distinct {
            if q.distinct_on.is_empty() {
                self.buf.push_str(" DISTINCT");
            } else {
                self.buf.push_str(" DISTINCT ON (");
                for (i, e) in q.distinct_on.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    let k = self.key_of(e, false)?;
                    self.emit_key(&k, false)?;
                }
                self.buf.push(')');
            }
        }
        self.target_list()?;
        self.from_clause()?;
        if let Some(f) = &q.filter {
            self.kw(" WHERE ", -STD, STD, 1);
            self.expr_as(f, BOOL, false)?;
        }
        if !q.group.is_empty() {
            self.kw(" GROUP BY ", -STD, STD, 1);
            self.in_group = true;
            let mut ok = Some(());
            for (i, e) in q.group.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                ok = self.key_of(e, true).and_then(|k| self.emit_key(&k, false));
                if ok.is_none() {
                    break;
                }
            }
            self.in_group = false;
            ok?;
        }
        if let Some(h) = &q.having {
            self.kw(" HAVING ", -STD, STD, 0);
            self.expr_as(h, BOOL, false)?;
        }
        if tail {
            self.tail(q, None)?;
        }
        Some(())
    }

    /// ORDER BY (by column numbers, `cols`, for a UNION's), OFFSET and LIMIT.
    fn tail(&mut self, q: &'q Select, cols: Option<&[String]>) -> Option<()> {
        if !q.order.is_empty() {
            self.kw(" ORDER BY ", -STD, STD, 1);
            for (i, (e, d)) in q.order.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                match cols {
                    Some(names) => {
                        let at = match e {
                            Expr::Lit(Value::Int(k)) if *k >= 1 && (*k as usize) <= names.len() => *k as usize,
                            Expr::Col(None, n) => names.iter().position(|c| c == n)? + 1,
                            _ => return None,
                        };
                        self.buf.push_str(&at.to_string());
                    }
                    None => {
                        let k = self.key_of(e, false)?;
                        self.emit_key(&k, true)?;
                    }
                }
                self.dir(*d);
            }
        }
        if let Some(o) = &q.offset {
            self.kw(" OFFSET ", -STD, STD, 0);
            self.expr_as(o, INT8, false)?;
        }
        match &q.limit {
            Some(Expr::Func(Func::WithTies, a)) => {
                self.kw(" FETCH FIRST ", -STD, STD, 0);
                self.buf.push('(');
                self.expr_as(a.first()?, INT8, false)?;
                self.buf.push_str(") ROWS WITH TIES");
            }
            Some(Expr::Lit(Value::Null)) => {
                self.kw(" LIMIT ", -STD, STD, 0);
                self.buf.push_str("ALL");
            }
            Some(l) => {
                self.kw(" LIMIT ", -STD, STD, 0);
                self.expr_as(l, INT8, false)?;
            }
            None => {}
        }
        Some(())
    }

    /// A sort key's direction, as get_rule_orderby writes it.
    fn dir(&mut self, d: Dir) {
        match d.desc {
            false if d.nulls_first => self.buf.push_str(" NULLS FIRST"),
            false => {}
            true => {
                self.buf.push_str(" DESC");
                if !d.nulls_first {
                    self.buf.push_str(" NULLS LAST");
                }
            }
        }
    }

    /// UNION / INTERSECT / EXCEPT: the parts (get_setop_query), then the whole's ORDER BY and LIMIT.
    fn set_query(&mut self, q: &'q Select, desc: Option<&[String]>, visible: bool) -> Option<()> {
        // (the parts are queries of a namespace of their own: their columns are qualified)
        self.levels.push(Level::empty(desc, visible));
        let tree = set_tree(q);
        let mut r = self.setop(&tree, desc, visible);
        if r.is_some() && (!q.order.is_empty() || q.limit.is_some() || q.offset.is_some()) {
            let names: Option<Vec<String>> = self.out_cols(q).map(|c| c.into_iter().map(|(n, _)| n).collect());
            r = names.and_then(|n| self.tail(q, Some(&n[..])));
        }
        self.levels.pop();
        r
    }

    fn setop(&mut self, t: &SetTree<'q>, desc: Option<&[String]>, visible: bool) -> Option<()> {
        match t {
            SetTree::Leaf(q) => {
                let q: &'q Select = q;
                let saved = self.indent;
                let r = if values_shaped(q) && q.set.is_empty() {
                    self.values_query(q, desc)
                } else {
                    self.simple_query(q, desc, visible, false)
                };
                self.indent = saved;
                r
            }
            SetTree::Op(op, all, l, r) => {
                let need = match &**l {
                    SetTree::Op(lop, lall, ..) => !(lop == op && lall == all),
                    SetTree::Leaf(_) => false,
                };
                let sub = if need { STD } else { 0 };
                if need {
                    self.buf.push('(');
                    self.kw("", sub, 0, 0);
                }
                self.setop(l, desc, visible)?;
                if need {
                    self.kw(") ", -sub, 0, 0);
                } else {
                    self.kw("", -sub, 0, 0);
                }
                self.buf.push_str(match op {
                    SetOp::Union => "UNION ",
                    SetOp::Intersect => "INTERSECT ",
                    SetOp::Except => "EXCEPT ",
                });
                if *all {
                    self.buf.push_str("ALL ");
                }
                let need = matches!(**r, SetTree::Op(..));
                let sub = if need { STD } else { 0 };
                if need {
                    self.buf.push('(');
                }
                self.kw("", sub, 0, 0);
                self.setop(r, desc, false)?;
                self.indent -= sub;
                if need {
                    self.kw(")", 0, 0, 0);
                }
                Some(())
            }
        }
    }

    /// A VALUES list: bare where the view's columns keep their names (column1, ...), else as Postgres writes a view
    /// that renamed them, `SELECT column1 AS x, ... FROM (VALUES ...) "*VALUES*"`.
    fn values_query(&mut self, q: &'q Select, desc: Option<&[String]>) -> Option<()> {
        let rows: Vec<&'q [Item]> =
            std::iter::once(&q.items[..]).chain(q.set.iter().map(|(_, _, p)| &p.items[..])).collect();
        let n = rows.first()?.len();
        let names: Vec<String> = (1..=n).map(|i| format!("column{i}")).collect();
        let simple = desc.is_none_or(|d| d.len() == n && d.iter().zip(&names).all(|(a, b)| a == b));
        self.levels.push(Level::empty(None, true));
        self.indent += STD;
        self.buf.push(' ');
        let r = if simple {
            self.values_def(&rows)
        } else {
            let desc = desc.unwrap_or_default();
            self.buf.push_str("SELECT");
            let mut multi = false;
            for (k, name) in names.iter().enumerate() {
                self.buf.push_str(if k == 0 { " " } else { ", " });
                let colname = desc.get(k).unwrap_or(name);
                let mut item = quoted(name);
                if colname != name {
                    item.push_str(" AS ");
                    item.push_str(&quoted(colname));
                }
                self.push_item(item, k + 1, &mut multi);
            }
            self.kw(" FROM ", -STD, STD, 2);
            self.buf.push('(');
            let r = self.values_def(&rows);
            self.buf.push_str(") \"*VALUES*\"");
            r
        };
        self.levels.pop();
        r
    }

    /// get_values_def: each column's values cast to their common type (an untyped literal takes it, and is text
    /// where all are).
    fn values_def(&mut self, rows: &[&'q [Item]]) -> Option<()> {
        let n = rows.first()?.len();
        let mut cells: Vec<Vec<Tn<'q>>> = vec![];
        for &row in rows {
            if row.len() != n {
                return None;
            }
            let mut v = vec![];
            for it in row {
                let Item::Expr(e, _) = it else { return None };
                v.push(self.tn(e)?);
            }
            cells.push(v);
        }
        let tys: Vec<Ty> = (0..n).map(|c| common(cells.iter().map(|r| r[c].ty)).map_or(TEXT, Ty::base)).collect();
        self.buf.push_str("VALUES ");
        for (ri, row) in cells.into_iter().enumerate() {
            if ri > 0 {
                self.buf.push_str(", ");
            }
            self.buf.push('(');
            for (ci, t) in row.into_iter().enumerate() {
                if ci > 0 {
                    self.buf.push(',');
                }
                let node = self.cast(t, tys[ci], true);
                self.emit_top(&node, false)?;
            }
            self.buf.push(')');
        }
        Some(())
    }

    // ------------------------------------------------------------ the namespace

    /// A query's namespace: its FROM items and their range-table entries (set_deparse_for_query).
    fn level(&mut self, q: &'q Select) -> Option<Level<'q>> {
        let mut lv = Level::empty(None, true);
        if let Some(first) = &q.from {
            let mut cur = self.node(&mut lv, first)?;
            for j in &q.joins {
                if j.kind == JoinKind::Cross {
                    // (a comma: another FROM item)
                    lv.from.push(cur);
                    cur = self.node(&mut lv, &j.src)?;
                } else {
                    let right = self.node(&mut lv, &j.src)?;
                    cur = self.join(&mut lv, cur, right, j.kind, j.on.as_ref(), &j.using, j.natural)?;
                }
            }
            lv.from.push(cur);
        }
        lv.prefix = !self.levels.is_empty() || lv.rtes.len() != 1;
        Some(lv)
    }

    /// A name for an entry unique among those of the queries being written and of this one so far: `base`, else
    /// `base_1`, `base_2`, ...
    fn unique(&self, lv: &Level<'q>, base: &str) -> String {
        let taken = |n: &str| {
            lv.rtes
                .iter()
                .chain(self.levels.iter().flat_map(|l| l.rtes.iter()))
                .any(|r| r.refname.as_deref() == Some(n))
        };
        if !taken(base) {
            return base.into();
        }
        (1..).map(|i| format!("{base}_{i}")).find(|n| !taken(n)).unwrap_or_else(|| base.into())
    }

    fn add_rte(
        &self,
        lv: &mut Level<'q>,
        written: &str,
        cols: Vec<String>,
        types: Vec<Ty>,
        rel: Option<String>,
    ) -> usize {
        let refname = Some(self.unique(lv, written));
        let n = cols.len();
        lv.rtes.push(Rte { written: written.into(), refname, cols, types, via: vec![Via::Own; n], rel });
        lv.rtes.len() - 1
    }

    /// A FROM source: a relation, a subquery, a function, or a parenthesized join.
    fn node(&mut self, lv: &mut Level<'q>, src: &'q Source) -> Option<Node<'q>> {
        if src.cte != 0
            || src.rec.is_some()
            || src.working
            || src.at.is_some()
            || src.hier
            || src.alias.starts_with('?')
        {
            return None;
        }
        if let Some(g) = group_of(src) {
            // a parenthesized join: its joins one item (a comma inside is CROSS JOIN)
            let mut cur = self.node(lv, g.from.as_ref()?)?;
            for j in &g.joins {
                let right = self.node(lv, &j.src)?;
                cur = self.join(lv, cur, right, j.kind, j.on.as_ref(), &j.using, j.natural)?;
            }
            if src.alias != GROUP {
                let Node::Join { rte, named, cols_given, .. } = &mut cur else { return None };
                let refname = self.unique(lv, &src.alias);
                let r = &mut lv.rtes[*rte];
                r.written = src.alias.clone();
                r.refname = Some(refname);
                for (c, n) in r.cols.iter_mut().zip(&src.names) {
                    *c = n.clone();
                }
                *named = true;
                *cols_given = !src.names.is_empty();
            }
            return Some(cur);
        }
        if let Some(sub) = src.sub.as_deref() {
            if src.alias.starts_with('\u{1}') {
                return None;
            }
            // (TABLESAMPLE and WITH ORDINALITY, as the parser keeps them, are not written back)
            let sampled = matches!(&sub.filter, Some(Expr::Func(Func::Scalar(funcs::Scalar::SampleKeep), _)));
            let numbered = sub.from.as_ref().is_some_and(|f| f.srf.is_some())
                && sub.items.iter().any(|it| matches!(it, Item::Expr(_, Some(a)) if a == "ordinality"));
            if sampled || numbered {
                return None;
            }
            // a function that gives one value, in FROM: the function, its one column named as its alias
            if src.lateral
                && sub.from.is_none()
                && let [Item::Expr(call @ Expr::Call(c), Some(alias))] = &sub.items[..]
            {
                let ty = self.tn(call).and_then(|t| t.ty).unwrap_or(TEXT);
                let names = if src.names.is_empty() { vec![alias.clone()] } else { src.names.clone() };
                let types = vec![ty; names.len()];
                let rte = self.add_rte(lv, &src.alias, names, types, None);
                return Some(Node::Func { rte, name: c.name.clone(), args: &c.args });
            }
            let cols = self.out_cols(sub)?;
            let mut names: Vec<String> = cols.iter().map(|c| c.0.clone()).collect();
            for (c, n) in names.iter_mut().zip(&src.names) {
                *c = n.clone();
            }
            let types = cols.iter().map(|c| c.1).collect();
            // a subquery without an alias is shown as Postgres names it: unnamed_subquery, then _1, _2, ...
            let base = if src.alias == "subquery" { "unnamed_subquery" } else { src.alias.as_str() };
            let rte = self.add_rte(lv, base, names, types, None);
            return Some(Node::Sub { rte, q: sub, lateral: src.lateral, cols_given: !src.names.is_empty() });
        }
        if let Some(f) = src.srf.as_deref() {
            let (name, args) = (&f.0, &f.1[..]);
            if name.contains(COLDEFS) {
                return None;
            }
            let bare = rel_part(written(name));
            let mut names: Vec<String> = match bare {
                "generate_series"
                | "unnest"
                | "generate_subscripts"
                | "regexp_split_to_table"
                | "json_object_keys"
                | "jsonb_object_keys" => vec![src.alias.clone()],
                "jsonb_array_elements"
                | "json_array_elements"
                | "jsonb_array_elements_text"
                | "json_array_elements_text" => vec!["value".into()],
                "jsonb_each" | "json_each" | "jsonb_each_text" | "json_each_text" => vec!["key".into(), "value".into()],
                _ if !src.names.is_empty() => src.names.clone(),
                _ => return None,
            };
            if names.len() < src.names.len() && names.len() == 1 {
                names = src.names.clone();
            }
            for (c, n) in names.iter_mut().zip(&src.names) {
                *c = n.clone();
            }
            let first = match (bare, args.first()) {
                ("generate_series", Some(a)) => self.tn(a).and_then(|t| t.ty).unwrap_or(INT4),
                _ => TEXT,
            };
            let types = vec![first; names.len()];
            let rte = self.add_rte(lv, &src.alias, names, types, None);
            return Some(Node::Func { rte, name: name.clone(), args });
        }
        let r = self.rels.iter().find(|r| r.name == src.table)?;
        if !matches!(r.kind, 'r' | 'p' | 'v' | 'm' | 'f') {
            return None;
        }
        let mut names: Vec<String> = r.columns.iter().map(|c| c.0.clone()).collect();
        for (c, n) in names.iter_mut().zip(&src.names) {
            *c = n.clone();
        }
        let types = r
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| Ty::with(c.1, r.typmods.get(i).copied().unwrap_or((0, -1))))
            .collect();
        let rte = self.add_rte(lv, &src.alias, names, types, Some(r.name.clone()));
        Some(Node::Rel {
            rte,
            name: r.name.clone(),
            only: src.only,
            aliased: src.alias != rel_part(&src.table),
            cols_given: !src.names.is_empty(),
        })
    }

    /// A join of `left` and `right`, and its entry: USING's columns first (the left side's, the right's for a RIGHT
    /// JOIN, a FULL JOIN's merged), then each side's others.
    #[allow(clippy::too_many_arguments)]
    fn join(
        &mut self,
        lv: &mut Level<'q>,
        left: Node<'q>,
        right: Node<'q>,
        kind: JoinKind,
        on: Option<&'q Expr>,
        using: &[String],
        natural: bool,
    ) -> Option<Node<'q>> {
        let (lr, rr) = (left.rte(), right.rte());
        let (lcols, rcols) = (lv.rtes[lr].cols.clone(), lv.rtes[rr].cols.clone());
        let using: Vec<String> = match natural {
            true => lcols.iter().filter(|c| rcols.contains(c)).cloned().collect(),
            false => using.to_vec(),
        };
        let (mut cols, mut types, mut via) = (vec![], vec![], vec![]);
        for u in &using {
            let li = lcols.iter().position(|c| c == u)?;
            let ri = rcols.iter().position(|c| c == u)?;
            cols.push(u.clone());
            types.push(lv.rtes[lr].types[li]);
            via.push(match kind {
                JoinKind::Full => Via::Bare,
                JoinKind::Right => Via::Col(rr, ri),
                _ => Via::Col(lr, li),
            });
        }
        for (side, names) in [(lr, &lcols), (rr, &rcols)] {
            for (i, c) in names.iter().enumerate() {
                if !using.contains(c) {
                    cols.push(c.clone());
                    types.push(lv.rtes[side].types[i]);
                    via.push(Via::Col(side, i));
                }
            }
        }
        lv.rtes.push(Rte { written: String::new(), refname: None, cols, types, via, rel: None });
        let rte = lv.rtes.len() - 1;
        let kind = if kind == JoinKind::Inner && on.is_none() && using.is_empty() { JoinKind::Cross } else { kind };
        Some(Node::Join {
            rte,
            kind,
            left: Box::new(left),
            right: Box::new(right),
            on,
            using,
            named: false,
            cols_given: false,
        })
    }

    /// The output columns of `q` (names and types), as a FROM subquery shows them.
    fn out_cols(&mut self, q: &'q Select) -> Option<Vec<(String, Ty)>> {
        let lv = self.level(q)?;
        self.levels.push(lv);
        let r = self.out_cols_here(q);
        self.levels.pop();
        r
    }

    fn out_cols_here(&mut self, q: &'q Select) -> Option<Vec<(String, Ty)>> {
        let mut out = vec![];
        for it in &q.items {
            match it {
                Item::Star | Item::TableStar(_) => {
                    let lv = self.levels.last()?;
                    let rtes: Vec<usize> = match it {
                        Item::TableStar(t) => {
                            vec![lv.rtes.iter().position(|r| r.refname.is_some() && r.written == *t)?]
                        }
                        _ => lv.from.iter().map(Node::rte).collect(),
                    };
                    for r in rtes {
                        out.extend(lv.rtes[r].cols.iter().cloned().zip(lv.rtes[r].types.iter().copied()));
                    }
                }
                Item::Expr(e, alias) => {
                    let name = alias.clone().or_else(|| figure_name(e)).unwrap_or_else(|| "?column?".into());
                    let ty = self.tn(e).and_then(|t| t.ty).unwrap_or(TEXT);
                    out.push((name, ty));
                }
            }
        }
        Some(out)
    }

    /// Where column `name` (of `qual`) is: levels out, entry, column.
    fn find_col(&self, qual: Option<&str>, name: &str) -> Option<(usize, usize, usize)> {
        for (up, lv) in self.levels.iter().rev().enumerate() {
            match qual {
                // (`s.t.col`: the relation of that stored name)
                Some(t) if t.contains('.') => {
                    if let Some(ri) = lv.rtes.iter().position(|r| r.refname.is_some() && r.rel.as_deref() == Some(t)) {
                        let ci = lv.rtes[ri].cols.iter().position(|c| c == name)?;
                        return Some((up, ri, ci));
                    }
                }
                // (an entry of the name without the column: `s.t.col` names an outer query's t, which the parser
                // keeps as `t.col`)
                Some(t) => {
                    let at = lv.rtes.iter().enumerate().find_map(|(ri, r)| {
                        let ci = r.cols.iter().position(|c| c == name)?;
                        (r.refname.is_some() && r.written == t).then_some((ri, ci))
                    });
                    if let Some((ri, ci)) = at {
                        return Some((up, ri, ci));
                    }
                }
                None => {
                    for node in &lv.from {
                        let ri = node.rte();
                        if let Some(ci) = lv.rtes[ri].cols.iter().position(|c| c == name) {
                            return Some((up, ri, ci));
                        }
                    }
                }
            }
        }
        None
    }

    /// The entry and column a column reference comes to: a join's column without the join's own name is the
    /// column of its side it is.
    fn canon(&self, up: usize, rte: usize, col: usize) -> (usize, usize) {
        let lv = &self.levels[self.levels.len() - 1 - up];
        match (&lv.rtes[rte].refname, lv.rtes[rte].via.get(col)) {
            (None, Some(Via::Col(r, c))) => self.canon(up, *r, *c),
            _ => (rte, col),
        }
    }

    /// get_variable: a column as written (`t.a`, or `a` where its query reads one relation), and its name.
    fn var_text(&self, up: usize, rte: usize, col: usize, force: bool) -> Option<(String, String)> {
        let lv = self.levels.get(self.levels.len().checked_sub(1 + up)?)?;
        let r = lv.rtes.get(rte)?;
        if r.refname.is_none() {
            return match r.via.get(col)? {
                Via::Col(r2, c2) => self.var_text(up, *r2, *c2, force),
                _ => {
                    let n = r.cols.get(col)?.clone();
                    Some((quoted(&n), n))
                }
            };
        }
        let name = r.cols.get(col)?.clone();
        let prefix = force || self.levels.last().is_some_and(|l| l.prefix);
        let text = match (&r.refname, prefix) {
            (Some(refname), true) => format!("{}.{}", quoted(refname), quoted(&name)),
            _ => quoted(&name),
        };
        Some((text, name))
    }

    fn var_ty(&self, up: usize, rte: usize, col: usize) -> Option<Ty> {
        let lv = self.levels.get(self.levels.len().checked_sub(1 + up)?)?;
        lv.rtes.get(rte)?.types.get(col).copied()
    }

    // ------------------------------------------------------------ the select list and FROM

    /// The select list's items: `*` and `t.*` as the columns they stand for.
    fn outputs(&mut self, q: &'q Select) -> Option<Vec<(String, Out<'q>)>> {
        let mut out = vec![];
        for it in &q.items {
            match it {
                Item::Star | Item::TableStar(_) => {
                    let lv = self.levels.last()?;
                    let rtes: Vec<usize> = match it {
                        Item::TableStar(t) => {
                            vec![lv.rtes.iter().position(|r| r.refname.is_some() && r.written == *t)?]
                        }
                        _ if lv.from.is_empty() => return None,
                        _ => lv.from.iter().map(Node::rte).collect(),
                    };
                    for r in rtes {
                        for (c, name) in lv.rtes[r].cols.iter().enumerate() {
                            out.push((name.clone(), Out::Var(0, r, c)));
                        }
                    }
                }
                Item::Expr(e, alias) => {
                    let name = alias.clone().or_else(|| figure_name(e)).unwrap_or_else(|| "?column?".into());
                    match e {
                        Expr::Col(qual, n) if n != "*" => {
                            let (up, r, c) = self.find_col(qual.as_deref(), n)?;
                            out.push((name, Out::Var(up, r, c)));
                        }
                        e => out.push((name, Out::Expr(e))),
                    }
                }
            }
        }
        // (a view's columns are those it was made with: a `*` over a table that gained columns since stands for
        // the ones it had)
        if let Some(d) = self.levels.last().and_then(|l| l.desc.as_ref())
            && out.len() > d.len()
            && self.levels.len() == 1
        {
            out.truncate(d.len());
        }
        Some(out)
    }

    /// An item of a list that wraps (get_target_list's and get_from_clause's): on a new line where it would pass the
    /// wrap column, or the one before it took more than one line.
    fn push_item(&mut self, item: String, colno: usize, multi: &mut bool) {
        if self.wrap >= 0 {
            let lead = item.starts_with('\n');
            if lead {
                trim_spaces(&mut self.buf);
            } else if colno > 1 && (self.line_len() + item.len() > self.wrap as usize || *multi) {
                self.kw("", -STD, STD, VAR_IND);
            }
            *multi = item[usize::from(lead)..].contains('\n');
        }
        self.buf.push_str(&item);
    }

    /// get_target_list.
    fn target_list(&mut self) -> Option<()> {
        let lv = self.levels.last()?;
        let (out, desc, visible) = (lv.out.clone(), lv.desc.clone(), lv.visible);
        let mut multi = false;
        for (k, (resname, o)) in out.iter().enumerate() {
            self.buf.push_str(if k == 0 { " " } else { ", " });
            let saved = std::mem::take(&mut self.buf);
            let attname = match o {
                Out::Var(up, r, c) => {
                    let (text, att) = self.var_text(*up, *r, *c, false)?;
                    self.buf.push_str(&text);
                    Some(att)
                }
                Out::Expr(e) => {
                    let t = self.tn(e)?;
                    let n = self.settle(t, TEXT);
                    self.emit(&n, true)?;
                    if visible { None } else { Some("?column?".to_string()) }
                }
            };
            let colname = desc.as_ref().and_then(|d| d.get(k)).unwrap_or(resname);
            if attname.as_deref() != Some(colname.as_str()) {
                self.buf.push_str(" AS ");
                self.buf.push_str(&quoted(colname));
            }
            let item = std::mem::replace(&mut self.buf, saved);
            self.push_item(item, k + 1, &mut multi);
        }
        Some(())
    }

    /// get_from_clause.
    #[allow(clippy::wrong_self_convention)]
    fn from_clause(&mut self) -> Option<()> {
        let from = self.levels.last()?.from.clone();
        for (k, node) in from.iter().enumerate() {
            if k == 0 {
                self.kw(" FROM ", -STD, STD, 2);
                self.from_item(node)?;
                continue;
            }
            self.buf.push_str(", ");
            let saved = std::mem::take(&mut self.buf);
            self.from_item(node)?;
            let item = std::mem::replace(&mut self.buf, saved);
            if self.wrap >= 0 {
                if item.starts_with('\n') {
                    trim_spaces(&mut self.buf);
                } else if self.line_len() + item.len() > self.wrap as usize {
                    self.kw("", -STD, STD, VAR_IND);
                }
            }
            self.buf.push_str(&item);
        }
        Some(())
    }

    /// An entry's column alias list, `(a, b, c)`.
    fn col_list(&mut self, rte: usize) -> Option<()> {
        let cols = self.levels.last()?.rtes.get(rte)?.cols.clone();
        self.buf.push('(');
        self.buf.push_str(&cols.iter().map(|c| quoted(c)).collect::<Vec<_>>().join(", "));
        self.buf.push(')');
        Some(())
    }

    fn refname(&self, rte: usize) -> Option<String> {
        self.levels.last()?.rtes.get(rte)?.refname.clone()
    }

    /// get_from_clause_item.
    #[allow(clippy::wrong_self_convention)]
    fn from_item(&mut self, node: &Node<'q>) -> Option<()> {
        match node {
            Node::Rel { rte, name, only, aliased, cols_given } => {
                if *only {
                    self.buf.push_str("ONLY ");
                }
                self.buf.push_str(&rel_text(name));
                let refname = self.refname(*rte)?;
                if *aliased || *cols_given || refname != rel_part(name) {
                    self.buf.push(' ');
                    self.buf.push_str(&quoted(&refname));
                }
                if *cols_given {
                    self.col_list(*rte)?;
                }
            }
            Node::Sub { rte, q, lateral, cols_given } => {
                if *lateral {
                    self.buf.push_str("LATERAL ");
                }
                self.buf.push('(');
                let ind = self.indent;
                self.query(q, None, true, ind)?;
                self.buf.push(')');
                let refname = self.refname(*rte)?;
                self.buf.push(' ');
                self.buf.push_str(&quoted(&refname));
                if *cols_given {
                    self.col_list(*rte)?;
                }
            }
            Node::Func { rte, name, args } => {
                let args: &'q [Expr] = args;
                if args.iter().any(has_col) {
                    self.buf.push_str("LATERAL ");
                }
                self.buf.push_str(&func_text(name));
                self.buf.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    self.expr_as(a, TEXT, true)?;
                }
                self.buf.push(')');
                let refname = self.refname(*rte)?;
                self.buf.push(' ');
                self.buf.push_str(&quoted(&refname));
                self.col_list(*rte)?;
            }
            Node::Join { rte, kind, left, right, on, using, named, cols_given } => {
                let paren = self.paren;
                let right_simple = matches!(**right, Node::Rel { .. } | Node::Sub { .. } | Node::Func { .. })
                    || matches!(**right, Node::Join { named: true, .. });
                let paren_right = paren && !right_simple;
                if !paren || *named {
                    self.buf.push('(');
                }
                self.from_item(left)?;
                let word = match kind {
                    JoinKind::Inner => " JOIN ",
                    JoinKind::Cross => " CROSS JOIN ",
                    JoinKind::Left => " LEFT JOIN ",
                    JoinKind::Right => " RIGHT JOIN ",
                    JoinKind::Full => " FULL JOIN ",
                };
                self.kw(word, -STD, STD, JOIN_IND);
                if paren_right {
                    self.buf.push('(');
                }
                self.from_item(right)?;
                if paren_right {
                    self.buf.push(')');
                }
                if !using.is_empty() {
                    self.buf.push_str(" USING (");
                    self.buf.push_str(&using.iter().map(|c| quoted(c)).collect::<Vec<_>>().join(", "));
                    self.buf.push(')');
                } else if let Some(e) = on {
                    self.buf.push_str(" ON ");
                    if !paren {
                        self.buf.push('(');
                    }
                    self.expr_as(e, BOOL, false)?;
                    if !paren {
                        self.buf.push(')');
                    }
                } else if !matches!(kind, JoinKind::Inner | JoinKind::Cross) {
                    self.buf.push_str(" ON TRUE");
                }
                if !paren || *named {
                    self.buf.push(')');
                }
                if *named {
                    let refname = self.refname(*rte)?;
                    self.buf.push(' ');
                    self.buf.push_str(&quoted(&refname));
                    if *cols_given {
                        self.col_list(*rte)?;
                    }
                }
            }
        }
        Some(())
    }

    // ------------------------------------------------------------ sort and group keys

    /// An ORDER BY (or GROUP BY, with `group`; or DISTINCT ON) key: a select-list item by its number or (ORDER BY's
    /// first, GROUP BY's after the FROM columns) its name, else the expression.
    fn key_of(&mut self, e: &'q Expr, group: bool) -> Option<Key<'q>> {
        let out = self.levels.last()?.out.clone();
        let from = |d: &mut Self, o: &Out<'q>| match o {
            Out::Var(u, r, c) => Some(Key::Var(*u, *r, *c)),
            Out::Expr(e) => d.key_expr(e),
        };
        match e {
            Expr::Lit(Value::Int(k)) if *k >= 1 && (*k as usize) <= out.len() => {
                return from(self, &out[*k as usize - 1].1);
            }
            Expr::Col(None, n) => {
                let named = out.iter().find(|(r, _)| r == n).map(|(_, o)| o.clone());
                if let Some(o) = named
                    && (!group || self.find_col(None, n).is_none())
                {
                    return from(self, &o);
                }
            }
            _ => {}
        }
        self.key_expr(e)
    }

    fn key_expr(&mut self, e: &'q Expr) -> Option<Key<'q>> {
        if let Expr::Col(qual, n) = e
            && n != "*"
        {
            let (u, r, c) = self.find_col(qual.as_deref(), n)?;
            return Some(Key::Var(u, r, c));
        }
        let t = self.tn(e)?;
        Some(Key::Node(self.settle(t, TEXT)))
    }

    /// get_rule_sortgroupclause: a column (qualified in an ORDER BY where an output column of its name is something
    /// else), a constant with its type, else the expression, parenthesized where it could be read otherwise.
    fn emit_key(&mut self, k: &Key<'q>, in_order: bool) -> Option<()> {
        match k {
            Key::Var(u, r, c) => {
                let force = in_order && !self.in_group && self.order_conflict(*u, *r, *c);
                let (text, _) = self.var_text(*u, *r, *c, force)?;
                self.buf.push_str(&text);
            }
            Key::Node(N::Const { text, ty, .. }) => {
                self.buf.push_str(text);
                self.buf.push_str("::");
                self.buf.push_str(ty);
            }
            Key::Node(n) => self.emit_sortkey(n)?,
        }
        Some(())
    }

    /// An expression as a sort key: parenthesized when pretty, and always when it looks like a function call.
    fn emit_sortkey(&mut self, n: &N<'q>) -> Option<()> {
        let call = matches!(n, N::Func(..) | N::Agg(_) | N::Win(_) | N::AtZone(..) | N::Extract(..))
            || matches!(n, N::Cast(_, _, CastKind::Func, _));
        let p = match n {
            N::Var(_) | N::Row(..) => false,
            N::Const { .. } => {
                if let N::Const { text, ty, .. } = n {
                    self.buf.push_str(text);
                    self.buf.push_str("::");
                    self.buf.push_str(ty);
                }
                return Some(());
            }
            _ => self.paren || call,
        };
        if p {
            self.buf.push('(');
        }
        self.emit(n, true)?;
        if p {
            self.buf.push(')');
        }
        Some(())
    }

    /// Whether a column in ORDER BY must be qualified: an output column has its name and is something else.
    fn order_conflict(&self, u: usize, r: usize, c: usize) -> bool {
        let Some(lv) = self.levels.last() else { return false };
        if lv.prefix {
            return false;
        }
        let Some((_, att)) = self.var_text(u, r, c, false) else { return false };
        let me = (u, self.canon(u, r, c));
        lv.out.iter().enumerate().any(|(k, (resname, o))| {
            let colname = lv.desc.as_ref().and_then(|d| d.get(k)).unwrap_or(resname);
            *colname == att
                && match o {
                    Out::Var(u2, r2, c2) => (*u2, self.canon(*u2, *r2, *c2)) != me,
                    Out::Expr(_) => true,
                }
        })
    }

    // ------------------------------------------------------------ expressions: building

    /// get_rule_expr of `e`, its untyped literals taking type `want`.
    fn expr_as(&mut self, e: &'q Expr, want: Ty, si: bool) -> Option<()> {
        let t = self.tn(e)?;
        let n = self.settle(t, want);
        self.emit(&n, si)
    }

    /// An untyped literal as a constant of type `to`; anything else as it is.
    fn settle(&self, t: Tn<'q>, to: Ty) -> N<'q> {
        match t.raw {
            Some(Value::Null) => konst(&Value::Null, to),
            Some(v) => {
                let shown = match to.t {
                    Type::Text => v.clone(),
                    _ => coerce(v.clone(), to.t).unwrap_or(v),
                };
                konst(&shown, to)
            }
            None => t.n,
        }
    }

    /// `t` cast to `to` (implicitly: shown only where implicit casts are), unless it is of that type already.
    fn cast(&self, t: Tn<'q>, to: Ty, implicit: bool) -> N<'q> {
        if t.raw.is_some() {
            return self.settle(t, to);
        }
        match t.ty {
            Some(f) if f.same_base(to) && (implicit || to.m.1 < 0 || f.m.1 == to.m.1) => t.n,
            from => match self.folded_cast(&t.n, from, to) {
                Some(c) => c,
                None => N::Cast(Box::new(t.n), to.name(), cast_kind(from, to), implicit),
            },
        }
    }

    /// The operands of a comparison or arithmetic, with the casts Postgres's choice of operator puts on them: an
    /// untyped literal takes the other side's type; an integer meeting a numeric is a numeric, a number meeting a
    /// double precision one; character types other than text meeting text are text. And their common type.
    fn pair(&self, a: Tn<'q>, b: Tn<'q>) -> (N<'q>, N<'q>, Ty) {
        let target = |x: Ty| if x.varchar() { TEXT } else { x.base() };
        match (a.ty, b.ty) {
            (None, None) => (self.settle(a, TEXT), self.settle(b, TEXT), TEXT),
            (Some(x), None) => {
                let to = target(x);
                (self.cast(a, to, true), self.settle(b, to), to)
            }
            (None, Some(y)) => {
                let to = target(y);
                let bn = self.cast(b, to, true);
                (self.settle(a, to), bn, to)
            }
            (Some(x), Some(y)) => match (x.rank(), y.rank()) {
                (Some(1), Some(1)) => (a.n, b.n, wider_int(x, y)),
                (Some(rx), Some(ry)) => {
                    let hi = rx.max(ry);
                    let to = |r: u8| match hi {
                        2 => (r == 1).then_some(NUMERIC),
                        _ => (r <= 2).then_some(FLOAT8),
                    };
                    let common = match hi {
                        2 => NUMERIC,
                        3 if rx == 3 && ry == 3 => FLOAT4,
                        _ => FLOAT8,
                    };
                    let an = match to(rx) {
                        Some(t) => self.cast(a, t, true),
                        None => a.n,
                    };
                    let bn = match to(ry) {
                        Some(t) => self.cast(b, t, true),
                        None => b.n,
                    };
                    (an, bn, common)
                }
                _ if x.t == Type::Text && y.t == Type::Text => {
                    let plain = |t: Ty| !t.varchar() && !t.bpchar();
                    if (plain(x) && plain(y)) || (x.bpchar() && y.bpchar()) {
                        (a.n, b.n, x)
                    } else {
                        (self.cast(a, TEXT, true), self.cast(b, TEXT, true), TEXT)
                    }
                }
                _ => (a.n, b.n, x),
            },
        }
    }

    /// An operand of a text operator (LIKE, `~`, `||`): an untyped literal is text, and so (cast) is a varchar.
    fn text_side(&self, t: Tn<'q>) -> N<'q> {
        match t.ty {
            None => self.settle(t, TEXT),
            Some(x) if x.varchar() => self.cast(t, TEXT, true),
            _ => t.n,
        }
    }

    /// The type of `e` as Chronos works it out, over the columns of the query being written.
    fn ty_fallback(&self, e: &Expr) -> Ty {
        let Some(lv) = self.levels.last() else { return TEXT };
        let mut cols = vec![];
        for r in lv.rtes.iter().filter(|r| r.refname.is_some()) {
            for (c, t) in r.cols.iter().zip(&r.types) {
                cols.push(Col::new(&r.written, c, t.t));
            }
        }
        ty_in(e, &cols, &[], &[]).map_or(TEXT, Ty::of)
    }

    /// Each of `args`, its untyped literals cast to the common type, which it gives too.
    fn unify(&mut self, args: &'q [Expr]) -> Option<(Vec<N<'q>>, Ty)> {
        let ts: Vec<Tn<'q>> = args.iter().map(|a| self.tn(a)).collect::<Option<_>>()?;
        let to = common(ts.iter().map(|t| t.ty)).map_or(TEXT, Ty::base);
        Some((ts.into_iter().map(|t| self.cast(t, to, true)).collect(), to))
    }

    fn args(&mut self, args: &'q [Expr]) -> Option<Vec<N<'q>>> {
        let mut out = vec![];
        for a in args {
            let t = self.tn(a)?;
            out.push(self.settle(t, TEXT));
        }
        Some(out)
    }

    /// The type of a subquery's (first) column.
    fn sub_ty(&mut self, q: &'q Select) -> Ty {
        self.out_cols(q).and_then(|c| c.first().map(|c| c.1)).unwrap_or(TEXT)
    }

    /// `e` as the node Postgres's parser would have made of it, and its type.
    fn tn(&mut self, e: &'q Expr) -> Option<Tn<'q>> {
        Some(match e {
            Expr::Lit(v) => self.lit(v)?,
            Expr::Col(qual, n) if n == "*" => {
                let qual = qual.as_deref()?;
                let lv = self
                    .levels
                    .iter()
                    .rev()
                    .find(|l| l.rtes.iter().any(|r| r.refname.is_some() && r.written == qual))?;
                let r = lv.rtes.iter().find(|r| r.refname.is_some() && r.written == qual)?;
                let refname = r.refname.clone()?;
                let rel = r.rel.as_deref().map_or_else(|| quoted(&refname), shown_name);
                Tn::typed(N::Row(format!("{}.*", quoted(&refname)), rel), TEXT)
            }
            Expr::Col(qual, n) => {
                let (u, r, c) = self.find_col(qual.as_deref(), n)?;
                let (text, _) = self.var_text(u, r, c, false)?;
                Tn::typed(N::Var(text), self.var_ty(u, r, c)?)
            }
            Expr::Neg(x) => match &**x {
                // (the grammar makes `-1` a negative constant)
                Expr::Lit(Value::Int(n)) if self.fold => {
                    let ty = if i32::try_from(-*n).is_ok() { INT4 } else { INT8 };
                    Tn::typed(konst(&Value::Int(-*n), ty), ty)
                }
                Expr::Lit(Value::Numeric(d)) => {
                    let v = Value::Numeric(d.clone());
                    let neg = coerce(Value::Text(format!("-{}", v.to_text())), Type::Numeric).ok()?;
                    Tn::typed(konst(&neg, NUMERIC), NUMERIC)
                }
                _ => {
                    let t = self.tn(x)?;
                    let ty = t.ty.unwrap_or(TEXT);
                    let n = self.settle(t, ty);
                    Tn::typed(N::Op("-".into(), vec![n]), ty)
                }
            },
            Expr::Bin(_, Op::And | Op::Or, _) => {
                let Expr::Bin(_, op, _) = e else { return None };
                let mut terms = vec![];
                let mut cur = e;
                // (a AND b AND c is one AND of three, as the grammar makes it; the right side stays its own)
                while let Expr::Bin(l, o, r) = cur
                    && o == op
                {
                    terms.push(&**r);
                    cur = &**l;
                }
                terms.push(cur);
                terms.reverse();
                let mut args = vec![];
                for t in terms {
                    let tn = self.tn(t)?;
                    args.push(self.settle(tn, BOOL));
                }
                let b = if *op == Op::And { BoolOp::And } else { BoolOp::Or };
                Tn::typed(N::Bool(b, args), BOOL)
            }
            Expr::Bin(a, op, b) => self.binop(e, a, *op, b)?,
            Expr::Not(x) => {
                let t = self.tn(x)?;
                Tn::typed(N::Bool(BoolOp::Not, vec![self.settle(t, BOOL)]), BOOL)
            }
            Expr::IsNull(x, neg) => {
                let t = self.tn(x)?;
                Tn::typed(N::NullTest(Box::new(self.settle(t, TEXT)), *neg), BOOL)
            }
            Expr::Like(a, p, ci, neg) => {
                let sym = op_sym(Op::Like { ci: *ci, neg: *neg })?;
                let (ta, tp) = (self.tn(a)?, self.tn(p)?);
                let an = match ta.ty {
                    Some(x) if x.bpchar() || x.t == Type::Name => ta.n,
                    _ => self.text_side(ta),
                };
                Tn::typed(N::Op(sym.into(), vec![an, self.text_side(tp)]), BOOL)
            }
            Expr::In(x, list, neg) => self.in_list(x, list, *neg)?,
            Expr::Cast(x, t) => {
                let to = Ty::of(*t);
                let tx = self.tn(x)?;
                if tx.raw.is_some() {
                    Tn::typed(self.settle(tx, to), to)
                } else {
                    Tn::typed(self.cast(tx, to, false), to)
                }
            }
            Expr::Case(op, whens, otherwise) => {
                let mut results: Vec<Tn<'q>> = vec![];
                for (_, r) in whens {
                    results.push(self.tn(r)?);
                }
                let els = match otherwise {
                    Some(o) => Some(self.tn(o)?),
                    None => None,
                };
                let to = common(results.iter().chain(els.iter()).map(|t| t.ty)).map_or(TEXT, Ty::base);
                let arg = match op {
                    Some(o) => Some(self.tn(o)?),
                    None => None,
                };
                let mut arms = vec![];
                for ((w, _), r) in whens.iter().zip(results) {
                    let tw = self.tn(w)?;
                    let cond = match &arg {
                        Some(a) => self.pair(a.clone(), tw).1,
                        None => self.settle(tw, BOOL),
                    };
                    arms.push((cond, self.cast(r, to, true)));
                }
                let els = match els {
                    Some(t) => self.cast(t, to, true),
                    None => konst(&Value::Null, to),
                };
                let arg = arg.map(|a| Box::new(self.settle(a, TEXT)));
                Tn::typed(N::Case(arg, arms, Box::new(els)), to)
            }
            Expr::Func(f, args) => self.func(e, f, args)?,
            Expr::Call(c) => {
                if let [Expr::Agg(Agg::List(ListAgg::User(_), _), ..)] = &c.args[..] {
                    return self.tn(&c.args[0]);
                }
                let mut out = vec![];
                for (i, a) in c.args.iter().enumerate() {
                    let t = self.tn(a)?;
                    let n = self.settle(t, TEXT);
                    out.push(match c.names.get(i).filter(|n| !n.is_empty()) {
                        Some(name) => N::Named(name.clone(), Box::new(n)),
                        None => n,
                    });
                }
                Tn::typed(N::Func(func_text(&c.name), out), self.ty_fallback(e))
            }
            Expr::Agg(f, arg, distinct) => self.agg(e, f, arg.as_deref(), *distinct)?,
            Expr::Window(w) => self.window(e, w)?,
            Expr::Sub(sq) => {
                if array_query(&sq.q) {
                    return None;
                }
                let ty = self.sub_ty(&sq.q);
                Tn::typed(N::Sub(SubKind::Expr, None, &sq.q), ty)
            }
            Expr::Exists(sq, neg) => {
                let n = N::Sub(SubKind::Exists, None, &sq.q);
                let n = if *neg { N::Bool(BoolOp::Not, vec![n]) } else { n };
                Tn::typed(n, BOOL)
            }
            Expr::InSub(x, sq, neg) => {
                if is_row(x) {
                    return None;
                }
                let tx = self.tn(x)?;
                let ty = self.sub_ty(&sq.q);
                let (kind, sym) = match sq.cmp {
                    None => (SubKind::Any, "="),
                    Some((op, all)) => (if all { SubKind::All } else { SubKind::Any }, op_sym(op)?),
                };
                let (left, _, _) = self.pair(tx, Tn::typed(N::Word(""), ty));
                let n = N::Sub(kind, Some((Box::new(left), sym.into())), &sq.q);
                let n = if *neg { N::Bool(BoolOp::Not, vec![n]) } else { n };
                Tn::typed(n, BOOL)
            }
            _ => return None,
        })
    }

    /// A literal: an integer, a number with a point, a boolean typed; a quoted string or NULL untyped.
    fn lit(&self, v: &Value) -> Option<Tn<'q>> {
        Some(match v {
            Value::Null => Tn { n: N::Word("NULL"), ty: None, raw: Some(Value::Null) },
            Value::Text(s) => Tn { n: N::Word("NULL"), ty: None, raw: Some(Value::Text(s.clone())) },
            Value::Int(n) if i32::try_from(*n).is_ok() => Tn::typed(konst(v, INT4), INT4),
            Value::Int(_) => Tn::typed(konst(v, INT8), INT8),
            Value::Numeric(_) | Value::Float(_) => Tn::typed(konst(v, NUMERIC), NUMERIC),
            Value::Bool(_) => Tn::typed(konst(v, BOOL), BOOL),
            v => {
                let ty = Ty::of(v.ty()?);
                Tn::typed(konst(v, ty), ty)
            }
        })
    }

    fn binop(&mut self, e: &'q Expr, a: &'q Expr, op: Op, b: &'q Expr) -> Option<Tn<'q>> {
        let sym = op_sym(op)?;
        let (ta, tb) = (self.tn(a)?, self.tn(b)?);
        Some(match op {
            Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                let (an, bn, _) = self.pair(ta, tb);
                Tn::typed(N::Op(sym.into(), vec![an, bn]), BOOL)
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => {
                let (x, y) = (ta.ty, tb.ty);
                let numbers = x.is_none_or(|t| t.rank().is_some()) && y.is_none_or(|t| t.rank().is_some());
                if numbers {
                    let (an, bn, ty) = self.pair(ta, tb);
                    Tn::typed(N::Op(sym.into(), vec![an, bn]), ty)
                } else {
                    // (times and intervals: an untyped literal takes the other side's type)
                    let ty = self.ty_fallback(e);
                    let an = match x {
                        None => self.settle(ta, y.map_or(TEXT, Ty::base)),
                        Some(_) => ta.n,
                    };
                    let bn = match y {
                        None => {
                            let to = match (x.map(|t| t.t), op) {
                                (Some(Type::Date | Type::Timestamp | Type::TimestampTz | Type::Time), _) => {
                                    Ty::of(Type::Interval)
                                }
                                _ => x.map_or(TEXT, Ty::base),
                            };
                            self.settle(tb, to)
                        }
                        Some(_) => tb.n,
                    };
                    Tn::typed(N::Op(sym.into(), vec![an, bn]), ty)
                }
            }
            Op::Pow => {
                let numeric = ta.ty == Some(NUMERIC) || tb.ty == Some(NUMERIC);
                let to = if numeric { NUMERIC } else { FLOAT8 };
                let (an, bn) = (self.cast(ta, to, true), self.cast(tb, to, true));
                Tn::typed(N::Op(sym.into(), vec![an, bn]), to)
            }
            Op::Concat => {
                let array = |t: Option<Ty>| t.is_some_and(|t| matches!(t.t, Type::Array(_)));
                let (x, y) = (ta.ty, tb.ty);
                if array(x) || array(y) {
                    let ty = if array(x) { x? } else { y? };
                    let (an, bn) = (self.settle(ta, ty), self.settle(tb, ty));
                    Tn::typed(N::Op(sym.into(), vec![an, bn]), ty)
                } else if x.is_some_and(|t| t.t == Type::Json) && y.is_none_or(|t| t.t == Type::Json) {
                    let j = Ty::of(Type::Json);
                    let (an, bn) = (ta.n, self.settle(tb, j));
                    Tn::typed(N::Op(sym.into(), vec![an, bn]), j)
                } else {
                    let side = |d: &Self, t: Tn<'q>| match t.ty {
                        Some(x) if x.bpchar() => d.cast(t, TEXT, true),
                        _ => d.text_side(t),
                    };
                    let (an, bn) = (side(self, ta), side(self, tb));
                    Tn::typed(N::Op(sym.into(), vec![an, bn]), TEXT)
                }
            }
            Op::Regex { .. } => {
                let an = match ta.ty {
                    Some(x) if x.bpchar() || x.t == Type::Name => ta.n,
                    _ => self.text_side(ta),
                };
                Tn::typed(N::Op(sym.into(), vec![an, self.text_side(tb)]), BOOL)
            }
            Op::Arrow | Op::ArrowText | Op::Has => {
                let ty = if op == Op::ArrowText {
                    TEXT
                } else if op == Op::Has {
                    BOOL
                } else {
                    ta.ty.unwrap_or(TEXT)
                };
                let an = self.settle(ta, Ty::of(Type::Json));
                Tn::typed(N::Op(sym.into(), vec![an, self.settle(tb, TEXT)]), ty)
            }
            Op::Path | Op::PathText | Op::HasAny | Op::HasAll => {
                let ty = match op {
                    Op::PathText => TEXT,
                    Op::Path => ta.ty.unwrap_or(TEXT),
                    _ => BOOL,
                };
                let an = self.settle(ta, Ty::of(Type::Json));
                Tn::typed(N::Op(sym.into(), vec![an, self.settle(tb, Ty::of(Type::Array(25)))]), ty)
            }
            _ => {
                // (the other operators: an untyped literal takes the other side's type)
                let ty = self.ty_fallback(e);
                let (x, y) = (ta.ty, tb.ty);
                let an = self.settle(ta, y.map_or(TEXT, Ty::base));
                let bn = self.settle(tb, x.map_or(TEXT, Ty::base));
                Tn::typed(N::Op(sym.into(), vec![an, bn]), ty)
            }
        })
    }

    /// x [NOT] IN (list), as transformAExprIn makes it: the constants one `x = ANY (ARRAY[...])` of their common
    /// type (where there are several), each other item a comparison of its own, OR-ed (NOT IN: `<> ALL`, AND-ed).
    fn in_list(&mut self, x: &'q Expr, list: &'q [Expr], neg: bool) -> Option<Tn<'q>> {
        let (sym, combine) = if neg { ("<>", BoolOp::And) } else { ("=", BoolOp::Or) };
        let tx = self.tn(x)?;
        let (vars, consts): (Vec<&'q Expr>, Vec<&'q Expr>) = list.iter().partition(|e| has_col(e));
        let mut terms = vec![];
        let rest: Vec<&'q Expr> = if consts.len() > 1 {
            let mut elems = vec![];
            for &c in &consts {
                elems.push(self.tn(c)?);
            }
            let to = common(std::iter::once(tx.ty).chain(elems.iter().map(|t| t.ty))).map_or(TEXT, Ty::base);
            let items: Vec<N<'q>> = elems.into_iter().map(|t| self.cast(t, to, true)).collect();
            let (left, arr) = if to.varchar() {
                let arr = match self.const_array(&items, "text[]") {
                    Some(c) => c,
                    None => N::Cast(Box::new(N::Array(items)), "text[]".into(), CastKind::Relabel, true),
                };
                (self.cast(tx.clone(), TEXT, true), arr)
            } else {
                let arr = self.const_array(&items, &format!("{}[]", to.name())).unwrap_or(N::Array(items));
                (self.cast(tx.clone(), to, true), arr)
            };
            terms.push(N::ArrayOp(Box::new(left), sym.into(), neg, Box::new(arr)));
            vars
        } else {
            list.iter().collect()
        };
        for e in rest {
            let te = self.tn(e)?;
            let (an, bn, _) = self.pair(tx.clone(), te);
            terms.push(N::Op(sym.into(), vec![an, bn]));
        }
        let mut it = terms.into_iter();
        let first = it.next()?;
        let n = it.fold(first, |acc, t| N::Bool(combine, vec![acc, t]));
        Some(Tn::typed(n, BOOL))
    }

    fn func(&mut self, e: &'q Expr, f: &Func, args: &'q [Expr]) -> Option<Tn<'q>> {
        use funcs::Scalar as S;
        Some(match f {
            Func::Coalesce | Func::Scalar(S::Greatest | S::Least) => {
                let (items, to) = self.unify(args)?;
                let kw = match f {
                    Func::Coalesce => "COALESCE(",
                    Func::Scalar(S::Greatest) => "GREATEST(",
                    _ => "LEAST(",
                };
                Tn::typed(N::Keyed(kw, items), to)
            }
            Func::Scalar(S::Nullif) => {
                let [a, b] = args else { return None };
                let (ta, tb) = (self.tn(a)?, self.tn(b)?);
                let ty = ta.ty.unwrap_or(TEXT);
                let (an, bn, _) = self.pair(ta, tb);
                Tn::typed(N::Keyed("NULLIF(", vec![an, bn]), ty)
            }
            Func::Scalar(S::ArrayOf) => {
                if args.is_empty() {
                    return None;
                }
                let (items, to) = self.unify(args)?;
                Tn::typed(N::Array(items), Ty::of(Type::Array(to.t.oid())))
            }
            Func::Scalar(S::ArrayAt) => {
                let (first, rest) = args.split_first()?;
                let t = self.tn(first)?;
                let ty = self.ty_fallback(e);
                let mut idx = vec![];
                for a in rest {
                    let ti = self.tn(a)?;
                    idx.push(self.settle(ti, INT4));
                }
                Tn::typed(N::Subscript(Box::new(self.settle(t, TEXT)), idx), ty)
            }
            // `x::varchar(n)`, `x::char(n)` and `x::varchar` (marked by a third argument), as the parser keeps them
            Func::Scalar(S::Left)
                if matches!(args, [Expr::Cast(_, Type::Text), Expr::Lit(Value::Int(_)), Expr::Lit(Value::Bool(_))]) =>
            {
                let (Expr::Cast(x, _), Expr::Lit(Value::Int(n)), Expr::Lit(Value::Bool(fixed))) =
                    (&args[0], &args[1], &args[2])
                else {
                    return None;
                };
                let m = super::chars_oid(u32::try_from(*n).ok().map(|n| (n, *fixed)), false)?;
                let to = Ty { t: Type::Text, m: (i64::from(m.0), i64::from(m.1)) };
                let tx = self.tn(x)?;
                let n = if tx.raw.is_some() { self.settle(tx, to) } else { self.cast(tx, to, false) };
                Tn::typed(n, to)
            }
            Func::Scalar(S::Spelled(k)) => {
                let name = funcs::SPELLED.get(*k as usize)?;
                let [Expr::Func(_, inner)] = args else { return None };
                let out = self.args(inner)?;
                Tn::typed(N::Func((*name).to_string(), out), self.ty_fallback(e))
            }
            Func::Now => Tn::typed(N::Func("now".into(), vec![]), Ty::of(Type::TimestampTz)),
            Func::CurrentDate => Tn::typed(N::Word("CURRENT_DATE"), Ty::of(Type::Date)),
            Func::Ctx(CtxFn::CurrentUser) if args.is_empty() => Tn::typed(N::Word("CURRENT_USER"), Ty::of(Type::Name)),
            Func::Ctx(CtxFn::SessionUser) if args.is_empty() => Tn::typed(N::Word("SESSION_USER"), Ty::of(Type::Name)),
            Func::Ctx(CtxFn::CurrentRole) if args.is_empty() => Tn::typed(N::Word("CURRENT_ROLE"), Ty::of(Type::Name)),
            Func::Ctx(CtxFn::RegOid(kind)) => {
                let [x] = args else { return None };
                let to = Ty::of(Type::Reg(*kind));
                let tx = self.tn(x)?;
                let n = if tx.raw.is_some() { self.settle_raw(tx, to) } else { self.cast(tx, to, false) };
                Tn::typed(n, to)
            }
            Func::Ctx(c @ (CtxFn::NextVal | CtxFn::CurrVal | CtxFn::SetVal)) => {
                let (first, rest) = args.split_first()?;
                let tf = self.tn(first)?;
                let class = Ty::of(Type::Reg(Reg::Class));
                let mut out = vec![if tf.raw.is_some() { self.settle_raw(tf, class) } else { tf.n }];
                out.extend(self.args(rest)?);
                Tn::typed(N::Func(c.name().into(), out), INT8)
            }
            Func::AtTimeZone => {
                let [x, z] = args else { return None };
                let (tx, tz) = (self.tn(x)?, self.tn(z)?);
                let ty = self.ty_fallback(e);
                let xn = self.settle(tx, Ty::of(Type::TimestampTz));
                Tn::typed(N::AtZone(Box::new(xn), Box::new(self.settle(tz, TEXT))), ty)
            }
            Func::Extract => {
                let [Expr::Lit(Value::Text(field)), x] = args else { return None };
                let tx = self.tn(x)?;
                let xn = self.settle(tx, Ty::of(Type::Timestamp));
                Tn::typed(N::Extract(field.to_lowercase(), Box::new(xn)), NUMERIC)
            }
            Func::Distinct(not) => {
                let [a, b] = args else { return None };
                if let Expr::Lit(Value::Bool(v)) = b {
                    let ta = self.tn(a)?;
                    let test = match (*not, *v) {
                        (true, true) => " IS TRUE",
                        (false, true) => " IS NOT TRUE",
                        (true, false) => " IS FALSE",
                        (false, false) => " IS NOT FALSE",
                    };
                    return Some(Tn::typed(N::BoolTest(Box::new(self.settle(ta, BOOL)), test), BOOL));
                }
                let (ta, tb) = (self.tn(a)?, self.tn(b)?);
                let (an, bn, _) = self.pair(ta, tb);
                let d = N::Distinct(Box::new(an), Box::new(bn));
                Tn::typed(if *not { N::Bool(BoolOp::Not, vec![d]) } else { d }, BOOL)
            }
            Func::Quant(op, all) => {
                let (first, rest) = args.split_first()?;
                let tx = self.tn(first)?;
                let mut elems = vec![];
                for a in rest {
                    elems.push(self.tn(a)?);
                }
                let to = common(std::iter::once(tx.ty).chain(elems.iter().map(|t| t.ty))).map_or(TEXT, Ty::base);
                let items: Vec<N<'q>> = elems.into_iter().map(|t| self.cast(t, to, true)).collect();
                let left = self.cast(tx, to, true);
                let arr = self.const_array(&items, &format!("{}[]", to.name())).unwrap_or(N::Array(items));
                Tn::typed(N::ArrayOp(Box::new(left), op_sym(*op)?.into(), *all, Box::new(arr)), BOOL)
            }
            Func::QuantArray(op, all) => {
                let [x, arr] = args else { return None };
                let (tx, ta) = (self.tn(x)?, self.tn(arr)?);
                let elem = tx.ty.unwrap_or(TEXT);
                let xn = self.settle(tx, TEXT);
                let an = self.settle(ta, Ty::of(Type::Array(elem.t.oid())));
                Tn::typed(N::ArrayOp(Box::new(xn), op_sym(*op)?.into(), *all, Box::new(an)), BOOL)
            }
            Func::FieldOf if args.len() == 2 => {
                let [x, Expr::Lit(Value::Text(field))] = args else { return None };
                let tx = self.tn(x)?;
                let ty = self.ty_fallback(e);
                Tn::typed(N::Field(Box::new(self.settle(tx, TEXT)), field.clone()), ty)
            }
            Func::Collate(c) => {
                let [x] = args else { return None };
                let tx = self.tn(x)?;
                let ty = tx.ty.unwrap_or(TEXT);
                let (name, _) = c.name();
                Tn::typed(N::Collate(Box::new(self.settle(tx, TEXT)), quoted(&name)), ty)
            }
            Func::RowValue => {
                let out = self.args(args)?;
                Tn::typed(N::RowExpr(out), TEXT)
            }
            Func::Lower
            | Func::Upper
            | Func::Length
            | Func::CharLength
            | Func::Abs
            | Func::Round
            | Func::DateTrunc
            | Func::DatePart
            | Func::Scalar(_)
            | Func::Ctx(_) => {
                let name = match f {
                    Func::Lower => "lower".to_string(),
                    Func::Upper => "upper".into(),
                    Func::Length => "length".into(),
                    Func::CharLength => "char_length".into(),
                    Func::Abs => "abs".into(),
                    Func::Round => "round".into(),
                    Func::DateTrunc => "date_trunc".into(),
                    Func::DatePart => "date_part".into(),
                    f => func_name(f),
                };
                if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                    return None;
                }
                let out = self.args(args)?;
                Tn::typed(N::Func(quoted(&name), out), self.ty_fallback(e))
            }
            _ => return None,
        })
    }

    /// An untyped literal of a reg type (`'t'::regclass`): its text as written, labeled.
    fn settle_raw(&self, t: Tn<'q>, to: Ty) -> N<'q> {
        match t.raw {
            Some(Value::Text(s)) => {
                let name = to.name();
                N::Const { text: format!("'{}'", s.replace('\'', "''")), label: Some(name.clone()), ty: name }
            }
            _ => self.settle(t, to),
        }
    }

    fn agg(&mut self, e: &'q Expr, f: &Agg, arg: Option<&'q Expr>, distinct: bool) -> Option<Tn<'q>> {
        let ty = self.ty_fallback(e);
        let name = agg_name(f).to_string();
        let mut a = AggN { name, distinct, star: false, args: vec![], within: false, order: vec![], filter: None };
        match (f, arg) {
            (Agg::Count, None) => a.star = true,
            (Agg::List(kind, keys), Some(Expr::Func(Func::Row, all))) => {
                let (n, filtered) = (usize::from(keys.n), usize::from(keys.filtered));
                let own = all.len().checked_sub(n + filtered)?;
                a.args = self.args(&all[..own])?;
                for (i, k) in all[own..own + n].iter().enumerate() {
                    let t = self.tn(k)?;
                    let d = Dir { desc: keys.desc >> i & 1 == 1, nulls_first: keys.nulls_first >> i & 1 == 1 };
                    a.order.push((self.settle(t, TEXT), d));
                }
                if keys.filtered {
                    let t = self.tn(all.last()?)?;
                    a.filter = Some(self.settle(t, BOOL));
                }
                a.within = matches!(
                    kind,
                    ListAgg::PercentileCont | ListAgg::PercentileDisc | ListAgg::Mode | ListAgg::Hypo(_)
                );
                if a.within && a.order.is_empty() {
                    return None;
                }
            }
            (Agg::List(..), _) => return None,
            (_, Some(x)) => a.args = self.args(std::slice::from_ref(x))?,
            (_, None) => return None,
        }
        Some(Tn::typed(N::Agg(Box::new(a)), ty))
    }

    fn window(&mut self, e: &'q Expr, w: &'q Win) -> Option<Tn<'q>> {
        if w.base.is_some() {
            return None;
        }
        let ty = self.ty_fallback(e);
        let name = win_name(w.func)?.to_string();
        let (star, args) = match (w.func, &w.args[..]) {
            (WinFn::Agg(Agg::Count), []) => (true, vec![]),
            (WinFn::Agg(Agg::List(_, keys)), [Expr::Func(Func::Row, all)]) => {
                if keys.n > 0 || keys.filtered {
                    return None;
                }
                (false, self.args(all)?)
            }
            (_, a) => (false, self.args(a)?),
        };
        let mut partition = vec![];
        for p in &w.partition {
            let t = self.tn(p)?;
            partition.push(self.settle(t, TEXT));
        }
        let mut order = vec![];
        for (o, d) in &w.order {
            let t = self.tn(o)?;
            order.push((self.settle(t, TEXT), *d));
        }
        let frame = match &w.frame {
            None => None,
            Some((unit, start, end)) => {
                let bound = |b: &Bound, first: bool| -> Option<String> {
                    // (an offset is a constant: a row count bare, an interval with its type)
                    let off = |v: &Value| match v {
                        Value::Int(n) => n.to_string(),
                        v => match konst(v, Ty::of(v.ty().unwrap_or(Type::Text))) {
                            N::Const { text, label: Some(l), .. } => format!("{text}::{l}"),
                            N::Const { text, .. } => text,
                            _ => String::new(),
                        },
                    };
                    Some(match b {
                        Bound::UnboundedPreceding if first => "UNBOUNDED PRECEDING".into(),
                        Bound::UnboundedFollowing if !first => "UNBOUNDED FOLLOWING".into(),
                        Bound::Current => "CURRENT ROW".into(),
                        Bound::Preceding(v) => format!("{} PRECEDING", off(v)),
                        Bound::Following(v) => format!("{} FOLLOWING", off(v)),
                        _ => return None,
                    })
                };
                let unit = match unit {
                    FrameUnit::Rows => "ROWS",
                    FrameUnit::Range => "RANGE",
                    FrameUnit::Groups => "GROUPS",
                };
                let mut s = format!("{unit} BETWEEN {} AND {}", bound(start, true)?, bound(end, false)?);
                match w.exclude {
                    Exclude::NoOthers => {}
                    Exclude::CurrentRow => s.push_str(" EXCLUDE CURRENT ROW"),
                    Exclude::Group => s.push_str(" EXCLUDE GROUP"),
                    Exclude::Ties => s.push_str(" EXCLUDE TIES"),
                }
                Some(s)
            }
        };
        Some(Tn::typed(N::Win(Box::new(WinN { name, star, args, partition, order, frame })), ty))
    }

    // ------------------------------------------------------------ expressions: writing

    /// isSimpleNode: whether `n`, under `parent` (on its left, `left`), needs no parentheses of its own.
    fn simple(&self, n: &N<'q>, parent: &N<'q>, left: bool) -> bool {
        // (what a sublink, a null test or an operator of no known precedence needs under each parent)
        let test = |p: &N<'q>| match p {
            N::Func(..) | N::Named(..) => true,
            N::Cast(..) | N::AtZone(..) | N::Extract(..) => false,
            N::Bool(..)
            | N::Subscript(..)
            | N::Array(..)
            | N::RowExpr(..)
            | N::Keyed(..)
            | N::Agg(_)
            | N::Win(_)
            | N::Case(..) => true,
            _ => false,
        };
        match n {
            N::Var(_) | N::Row(..) | N::Const { .. } | N::Word(_) => true,
            N::Func(..)
            | N::Agg(_)
            | N::Win(_)
            | N::Keyed(..)
            | N::Array(..)
            | N::RowExpr(_)
            | N::Case(..)
            | N::Subscript(..)
            | N::AtZone(..)
            | N::Extract(..) => true,
            N::Named(_, a) => self.simple(a, parent, left),
            N::Field(..) => !matches!(parent, N::Field(..)),
            N::Cast(arg, _, kind, _) => match kind {
                CastKind::Func => true,
                _ => self.simple(arg, n, left),
            },
            N::Collate(..) | N::ArrayOp(..) => false,
            N::Op(sym, args) => {
                if self.paren
                    && let N::Op(psym, pargs) = parent
                {
                    let one = |s: &str, a: &[N<'q>]| a.len() == 2 && s.len() == 1;
                    if !one(sym.as_str(), &args[..]) || !one(psym.as_str(), &pargs[..]) {
                        return false;
                    }
                    let (lo, hi) = ("+-".contains(sym.as_str()), "*/%".contains(sym.as_str()));
                    let (plo, phi) = ("+-".contains(psym.as_str()), "*/%".contains(psym.as_str()));
                    if !(lo || hi) || !(plo || phi) {
                        return false;
                    }
                    if hi && plo {
                        return true;
                    }
                    if lo && phi {
                        return false;
                    }
                    return left;
                }
                test(parent)
            }
            N::NullTest(..) | N::BoolTest(..) | N::Distinct(..) | N::Sub(..) => test(parent),
            N::Bool(op, _) => match parent {
                N::Bool(pop, _) => {
                    self.paren
                        && match op {
                            BoolOp::Not | BoolOp::And => matches!(pop, BoolOp::And | BoolOp::Or),
                            BoolOp::Or => *pop == BoolOp::Or,
                        }
                }
                N::Func(..) | N::Named(..) => true,
                N::Cast(..) | N::AtZone(..) | N::Extract(..) => false,
                N::Subscript(..)
                | N::Array(..)
                | N::RowExpr(..)
                | N::Keyed(..)
                | N::Agg(_)
                | N::Win(_)
                | N::Case(..) => true,
                _ => false,
            },
        }
    }

    /// get_rule_expr_paren.
    fn emit_paren(&mut self, n: &N<'q>, si: bool, parent: &N<'q>, left: bool) -> Option<()> {
        let need = self.paren && !self.simple(n, parent, left);
        if need {
            self.buf.push('(');
        }
        self.emit(n, si)?;
        if need {
            self.buf.push(')');
        }
        Some(())
    }

    /// get_rule_expr_toplevel: a whole row at the top of a list is `t.*::type`.
    fn emit_top(&mut self, n: &N<'q>, si: bool) -> Option<()> {
        if let N::Row(text, ty) = n {
            self.buf.push_str(text);
            self.buf.push_str("::");
            self.buf.push_str(ty);
            return Some(());
        }
        self.emit(n, si)
    }

    fn list(&mut self, items: &[N<'q>], si: bool) -> Option<()> {
        for (i, a) in items.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.emit(a, si)?;
        }
        Some(())
    }

    fn order_list(&mut self, keys: &[(N<'q>, Dir)]) -> Option<()> {
        for (i, (k, d)) in keys.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.emit_sortkey(k)?;
            self.dir(*d);
        }
        Some(())
    }

    /// get_rule_expr: `n` written out; `si` shows its implicit cast, if it is one.
    fn emit(&mut self, n: &N<'q>, si: bool) -> Option<()> {
        let open = !self.paren;
        match n {
            N::Var(s) | N::Row(s, _) => self.buf.push_str(s),
            N::Word(w) => self.buf.push_str(w),
            N::Const { text, label, .. } => {
                self.buf.push_str(text);
                if let Some(l) = label {
                    self.buf.push_str("::");
                    self.buf.push_str(l);
                }
            }
            N::Op(sym, args) => {
                if open {
                    self.buf.push('(');
                }
                match &args[..] {
                    [a, b] => {
                        self.emit_paren(a, true, n, true)?;
                        self.buf.push(' ');
                        self.buf.push_str(sym);
                        self.buf.push(' ');
                        self.emit_paren(b, true, n, false)?;
                    }
                    [a] => {
                        self.buf.push_str(sym);
                        self.buf.push(' ');
                        self.emit_paren(a, true, n, false)?;
                    }
                    _ => return None,
                }
                if open {
                    self.buf.push(')');
                }
            }
            N::Bool(op, args) => {
                if open {
                    self.buf.push('(');
                }
                match op {
                    BoolOp::Not => {
                        self.buf.push_str("NOT ");
                        self.emit_paren(args.first()?, false, n, false)?;
                    }
                    _ => {
                        let word = if *op == BoolOp::And { " AND " } else { " OR " };
                        for (i, a) in args.iter().enumerate() {
                            if i > 0 {
                                self.buf.push_str(word);
                            }
                            self.emit_paren(a, false, n, i == 0)?;
                        }
                    }
                }
                if open {
                    self.buf.push(')');
                }
            }
            N::Func(name, args) => {
                self.buf.push_str(name);
                self.buf.push('(');
                self.list(args, true)?;
                self.buf.push(')');
            }
            N::Named(name, a) => {
                self.buf.push_str(&quoted(name));
                self.buf.push_str(" => ");
                self.emit(a, si)?;
            }
            N::Agg(a) => {
                self.buf.push_str(&a.name);
                self.buf.push('(');
                if a.distinct {
                    self.buf.push_str("DISTINCT ");
                }
                if a.within {
                    self.list(&a.args, true)?;
                    self.buf.push_str(") WITHIN GROUP (ORDER BY ");
                    self.order_list(&a.order)?;
                } else {
                    if a.star {
                        self.buf.push('*');
                    } else {
                        self.list(&a.args, true)?;
                    }
                    if !a.order.is_empty() {
                        self.buf.push_str(" ORDER BY ");
                        self.order_list(&a.order)?;
                    }
                }
                if let Some(f) = &a.filter {
                    self.buf.push_str(") FILTER (WHERE ");
                    self.emit(f, false)?;
                }
                self.buf.push(')');
            }
            N::Win(w) => {
                self.buf.push_str(&w.name);
                self.buf.push('(');
                if w.star {
                    self.buf.push('*');
                } else {
                    self.list(&w.args, true)?;
                }
                self.buf.push_str(") OVER (");
                let mut space = false;
                if !w.partition.is_empty() {
                    self.buf.push_str("PARTITION BY ");
                    for (i, p) in w.partition.iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.emit_sortkey(p)?;
                    }
                    space = true;
                }
                if !w.order.is_empty() {
                    if space {
                        self.buf.push(' ');
                    }
                    self.buf.push_str("ORDER BY ");
                    self.order_list(&w.order)?;
                    space = true;
                }
                if let Some(f) = &w.frame {
                    if space {
                        self.buf.push(' ');
                    }
                    self.buf.push_str(f);
                }
                self.buf.push(')');
            }
            N::Cast(arg, ty, _, implicit) => {
                if *implicit && !si {
                    self.emit_paren(arg, false, n, false)?;
                } else {
                    // get_coercion_expr: a constant of the type already is shown without its label
                    match &**arg {
                        N::Const { text, label: Some(l), .. } if l == ty => self.buf.push_str(text),
                        a => {
                            if open {
                                self.buf.push('(');
                            }
                            self.emit_paren(a, false, n, false)?;
                            if open {
                                self.buf.push(')');
                            }
                        }
                    }
                    self.buf.push_str("::");
                    self.buf.push_str(ty);
                }
            }
            N::Case(arg, arms, els) => {
                self.kw("CASE", 0, VAR_IND, 0);
                if let Some(a) = arg {
                    self.buf.push(' ');
                    self.emit(a, true)?;
                }
                for (w, r) in arms {
                    self.kw("WHEN ", 0, 0, 0);
                    self.emit(w, false)?;
                    self.buf.push_str(" THEN ");
                    self.emit(r, true)?;
                }
                self.kw("ELSE ", 0, 0, 0);
                self.emit(els, true)?;
                self.kw("END", -VAR_IND, 0, 0);
            }
            N::Keyed(kw, args) => {
                self.buf.push_str(kw);
                self.list(args, true)?;
                self.buf.push(')');
            }
            N::NullTest(a, not) => {
                if open {
                    self.buf.push('(');
                }
                self.emit_paren(a, true, n, false)?;
                self.buf.push_str(if *not { " IS NOT NULL" } else { " IS NULL" });
                if open {
                    self.buf.push(')');
                }
            }
            N::BoolTest(a, test) => {
                if open {
                    self.buf.push('(');
                }
                self.emit_paren(a, false, n, false)?;
                self.buf.push_str(test);
                if open {
                    self.buf.push(')');
                }
            }
            N::Distinct(a, b) => {
                if open {
                    self.buf.push('(');
                }
                self.emit_paren(a, true, n, true)?;
                self.buf.push_str(" IS DISTINCT FROM ");
                self.emit_paren(b, true, n, false)?;
                if open {
                    self.buf.push(')');
                }
            }
            N::ArrayOp(a, sym, all, arr) => {
                if open {
                    self.buf.push('(');
                }
                self.emit_paren(a, true, n, true)?;
                self.buf.push(' ');
                self.buf.push_str(sym);
                self.buf.push_str(if *all { " ALL (" } else { " ANY (" });
                self.emit_paren(arr, true, n, false)?;
                self.buf.push(')');
                if open {
                    self.buf.push(')');
                }
            }
            N::Array(items) => {
                self.buf.push_str("ARRAY[");
                self.list(items, true)?;
                self.buf.push(']');
            }
            N::RowExpr(items) => {
                self.buf.push_str("ROW(");
                for (i, a) in items.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    self.emit_top(a, true)?;
                }
                self.buf.push(')');
            }
            N::Sub(kind, test, q) => {
                self.buf.push('(');
                if let Some((left, sym)) = test {
                    self.emit(left, true)?;
                    match kind {
                        SubKind::Any if sym == "=" => self.buf.push_str(" IN "),
                        SubKind::Any => {
                            self.buf.push(' ');
                            self.buf.push_str(sym);
                            self.buf.push_str(" ANY ");
                        }
                        _ => {
                            self.buf.push(' ');
                            self.buf.push_str(sym);
                            self.buf.push_str(" ALL ");
                        }
                    }
                }
                if *kind == SubKind::Exists {
                    self.buf.push_str("EXISTS ");
                }
                let paren = *kind != SubKind::Expr;
                if paren {
                    self.buf.push('(');
                }
                let ind = self.indent;
                self.query(q, None, false, ind)?;
                self.buf.push_str(if paren { "))" } else { ")" });
            }
            N::Collate(a, c) => {
                if open {
                    self.buf.push('(');
                }
                self.emit_paren(a, si, n, false)?;
                self.buf.push_str(" COLLATE ");
                self.buf.push_str(c);
                if open {
                    self.buf.push(')');
                }
            }
            N::Subscript(a, idx) => {
                let bare = matches!(**a, N::Var(_) | N::Field(..));
                if !bare {
                    self.buf.push('(');
                }
                self.emit(a, si)?;
                if !bare {
                    self.buf.push(')');
                }
                for i in idx {
                    self.buf.push('[');
                    self.emit(i, true)?;
                    self.buf.push(']');
                }
            }
            N::Field(a, f) => {
                let bare = matches!(**a, N::Subscript(..) | N::Field(..));
                if !bare {
                    self.buf.push('(');
                }
                self.emit(a, true)?;
                if !bare {
                    self.buf.push(')');
                }
                self.buf.push('.');
                self.buf.push_str(&quoted(f));
            }
            N::AtZone(x, z) => {
                self.buf.push('(');
                self.emit_paren(x, false, n, true)?;
                self.buf.push_str(" AT TIME ZONE ");
                self.emit_paren(z, false, n, false)?;
                self.buf.push(')');
            }
            N::Extract(field, x) => {
                self.buf.push_str("EXTRACT(");
                self.buf.push_str(field);
                self.buf.push_str(" FROM ");
                self.emit(x, true)?;
                self.buf.push(')');
            }
        }
        Some(())
    }
}
