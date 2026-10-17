//! Extended statistics: CREATE, ALTER and DROP STATISTICS. Chronos's planner keeps no statistics, so a statistics
//! object is its definition: checked as Postgres checks it, kept as a row of the world (`_sonos_defs/stat/<name>/`,
//! JSON, the name schema-qualified outside public), and shown where Postgres shows it (pg_statistic_ext,
//! pg_get_statisticsobjdef, psql's "Statistics objects:"). ANALYZE builds nothing for it: pg_statistic_ext_data and
//! pg_stats_ext have no row of it, as for an object never analyzed.
//!
//! It goes with its table (see `forget`); one whose table or columns are gone isn't shown, and a new one may take
//! its name.

use super::catalog::{Catalog, Obj};
use super::*;

pub(super) const KEY: &str = "_sonos_defs/stat/";

fn key(name: &str) -> String {
    format!("{KEY}{name}/")
}

/// At most this many columns and expressions (Postgres's STATS_MAX_DIMENSIONS).
const MAX_DIMENSIONS: usize = 8;

/// A statistics object as its row keeps it.
#[derive(Clone, Debug)]
pub(crate) struct StatObj {
    /// as stored: `schema.name` outside public
    pub name: String,
    /// its table, as stored
    pub table: String,
    pub owner: i64,
    /// SET STATISTICS's target (-1: the default)
    pub target: i64,
    /// stxkind: `d` ndistinct, `f` dependencies, `m` mcv, `e` expressions
    pub kinds: Vec<char>,
    /// its columns, in column order
    pub cols: Vec<String>,
    /// its expressions, as Postgres writes them back
    pub exprs: Vec<String>,
}

impl StatObj {
    fn from_json(name: &str, j: &Json) -> StatObj {
        let strs = |x: &Json| -> Vec<String> {
            x.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
        };
        StatObj {
            name: name.to_string(),
            table: j["table"].as_str().unwrap_or_default().to_string(),
            owner: j["owner"].as_i64().unwrap_or(roles::BOOTSTRAP),
            target: j["target"].as_i64().unwrap_or(-1),
            kinds: j["kinds"].as_str().unwrap_or_default().chars().collect(),
            cols: strs(&j["cols"]),
            exprs: strs(&j["exprs"]),
        }
    }

    fn write(&self) -> (String, Option<String>) {
        let j = serde_json::json!({
            "table": self.table,
            "owner": self.owner,
            "target": self.target,
            "kinds": self.kinds.iter().collect::<String>(),
            "cols": self.cols,
            "exprs": self.exprs,
        });
        (key(&self.name), Some(j.to_string()))
    }
}

/// Every statistics object of the world, in name order.
pub(crate) fn load(v: &View) -> Result<Vec<StatObj>> {
    let mut out = vec![];
    for r in v.prefix(KEY)? {
        let (k, raw) = r?;
        let Some(name) = k[KEY.len()..].strip_suffix('/') else { continue };
        out.push(StatObj::from_json(name, &serde_json::from_str(&raw).unwrap_or_default()));
    }
    Ok(out)
}

impl StatObj {
    /// Every column it names: its columns, and those its expressions read (on which Postgres records its
    /// dependencies, so that it goes with any of them).
    fn names(&self) -> Vec<String> {
        let mut out = self.cols.clone();
        for e in &self.exprs {
            if let Ok(x) = check_expr(e) {
                walk(&x, &mut |n| {
                    if let Expr::Col(_, c) = n
                        && !out.contains(c)
                    {
                        out.push(c.clone());
                    }
                });
            }
        }
        out
    }
}

/// Whether object `s` still stands: its table is there, with every column it names (its expressions' too).
fn stands(v: &View, s: &StatObj) -> Result<bool> {
    Ok(match load_world(v, &s.table)? {
        Some(t) => s.names().iter().all(|c| t.cols.iter().any(|x| x.name == *c && !x.hidden)),
        None => false,
    })
}

/// The writes that make the statistics objects of table `from` follow it to its new name `to` (RENAME TO,
/// SET SCHEMA), as Postgres's, which name their table by oid, do.
pub(super) fn renamed_table(v: &View, from: &str, to: &str) -> Result<Vec<(String, Option<String>)>> {
    Ok(load(v)?
        .into_iter()
        .filter(|s| s.table == from)
        .map(|mut s| {
            to.clone_into(&mut s.table);
            s.write()
        })
        .collect())
}

/// The writes that make the statistics objects of table `table` follow its column `a`, renamed `b`: in their
/// columns and in their expressions' text (written back as before, over the table's columns `cols`).
pub(super) fn renamed_column(
    v: &View,
    table: &str,
    a: &str,
    b: &str,
    cols: &[Col],
) -> Result<Vec<(String, Option<String>)>> {
    let mut writes = vec![];
    for mut s in load(v)?.into_iter().filter(|s| s.table == table && s.names().iter().any(|c| c == a)) {
        for c in s.cols.iter_mut().filter(|c| *c == a) {
            b.clone_into(c);
        }
        for e in &mut s.exprs {
            *e = catalog::pretty_def(&renamed_in(e, a, b)?, cols);
        }
        writes.push(s.write());
    }
    Ok(writes)
}

/// Text `text` with column `a` named `b` (by its tokens: a string saying `a` stays).
pub(super) fn renamed_in(text: &str, a: &str, b: &str) -> Result<String> {
    let toks: Vec<Tok> = lex(text)?
        .into_iter()
        .map(|x| match x {
            Tok::Word(w) | Tok::Ident(w) if w == a => Tok::Ident(b.to_string()),
            x => x,
        })
        .collect();
    Ok(tokens_sql(&toks))
}

/// The writes that drop the statistics objects of table `table` that name column `c` (as a column, or in an
/// expression): ALTER TABLE ... DROP COLUMN takes them with it.
pub(super) fn dropped_column(v: &View, table: &str, c: &str) -> Result<Vec<(String, Option<String>)>> {
    Ok(load(v)?
        .into_iter()
        .filter(|s| s.table == table && s.names().iter().any(|n| n == c))
        .map(|s| (key(&s.name), None))
        .collect())
}

/// The statistics objects role `oid` owns that stand, as DROP ROLE names them (`statistics object s`).
pub(super) fn owned_by(v: &View, oid: i64) -> Result<Vec<String>> {
    let mut out = vec![];
    for s in load(v)? {
        if s.owner == oid && stands(v, &s)? {
            out.push(s.name);
        }
    }
    Ok(out)
}

/// The writes that give roles `olds`' statistics objects to role `new` (REASSIGN OWNED).
pub(super) fn reassigned(v: &View, olds: &[i64], new: i64) -> Result<Vec<(String, Option<String>)>> {
    Ok(load(v)?
        .into_iter()
        .filter(|s| olds.contains(&s.owner))
        .map(|mut s| {
            s.owner = new;
            s.write()
        })
        .collect())
}

fn find(v: &View, name: &str) -> Result<Option<StatObj>> {
    let Some(raw) = v.get(&key(name))? else { return Ok(None) };
    let s = StatObj::from_json(name, &serde_json::from_str(&raw).unwrap_or_default());
    Ok(if stands(v, &s)? { Some(s) } else { None })
}

/// A name as written: its schema (if written) and name.
type Named = (Option<String>, String);

/// The stored name a written name means: in the schema written, else the first schema of the search_path that has
/// an object of the name (`create`: the first that exists, where a new one goes).
fn stored(v: &View, n: &Named, create: bool) -> Result<String> {
    if let Some(s) = &n.0 {
        return Ok(qualify(s, &n.1));
    }
    let path = path_schemas().unwrap_or_else(|| vec!["public".into()]);
    let schemas = namespaces(v)?;
    let exists = |s: &str| s == "public" || schemas.iter().any(|x| x == s);
    for s in &path {
        if !exists(s) {
            continue;
        }
        let q = qualify(s, &n.1);
        if create || find(v, &q)?.is_some() {
            return Ok(q);
        }
    }
    Ok(qualify(if create { "public" } else { path.first().map_or("public", String::as_str) }, &n.1))
}

/// 42704 for an object that isn't there, as written.
fn missing(n: &Named) -> SqlError {
    let shown = n.0.as_ref().map_or_else(|| n.1.clone(), |s| format!("{s}.{}", n.1));
    SqlError { code: "42704", message: format!("statistics object \"{shown}\" does not exist"), info: None }
}

// ---------------------------------------------------------------- statements

/// What CREATE STATISTICS is on: a column, or an expression as written.
#[derive(Clone, Debug)]
pub(crate) enum Item {
    Col(String),
    Expr(String),
}

#[derive(Clone, Debug)]
pub(crate) enum StatStmt {
    Create { name: Option<Named>, if_not_exists: bool, kinds: Vec<String>, items: Vec<Item>, table: Named },
    Alter { name: Named, if_exists: bool, how: StatAlter },
    Drop { names: Vec<Named>, if_exists: bool },
}

#[derive(Clone, Debug)]
pub(crate) enum StatAlter {
    Target(i64),
    Rename(String),
    Schema(String),
    Owner(roles::Spec),
}

/// The words after a table in FROM that make it more than a relation's name.
const NOT_ALIASES: [&str; 12] =
    ["tablesample", "natural", "join", "cross", "inner", "left", "right", "full", "on", "using", "where", "lateral"];

impl Parser {
    /// `[schema.]name`.
    fn stat_name(&mut self) -> Result<Named> {
        let first = self.name()?;
        Ok(if self.sym(".") { (Some(first), self.name()?) } else { (None, first) })
    }

    /// After CREATE, ALTER or DROP STATISTICS.
    pub(super) fn statistics(&mut self, verb: &str) -> Result<Stmt> {
        let guard = |p: &mut Parser, not: bool| -> Result<bool> {
            if !p.kw("if") {
                return Ok(false);
            }
            if not {
                p.expect_kw("not")?;
            }
            p.expect_kw("exists")?;
            Ok(true)
        };
        let st = match verb {
            "create" => {
                let if_not_exists = guard(self, true)?;
                let name = if self.is_kw("on") || matches!(self.peek(), Some(Tok::Sym("("))) {
                    None
                } else {
                    Some(self.stat_name()?)
                };
                let kinds = if self.sym("(") { self.names()? } else { vec![] };
                self.expect_kw("on")?;
                let mut items = vec![];
                loop {
                    items.push(self.stat_item()?);
                    if !self.sym(",") {
                        break;
                    }
                }
                self.expect_kw("from")?;
                let table = self.stat_from()?;
                StatStmt::Create { name, if_not_exists, kinds, items, table }
            }
            "alter" => {
                let if_exists = guard(self, false)?;
                let name = self.stat_name()?;
                let how = if self.kw("rename") {
                    self.expect_kw("to")?;
                    StatAlter::Rename(self.name()?)
                } else if self.kw("owner") {
                    self.expect_kw("to")?;
                    StatAlter::Owner(self.role_spec(false)?)
                } else {
                    self.expect_kw("set")?;
                    if self.kw("schema") {
                        StatAlter::Schema(self.name()?)
                    } else {
                        self.expect_kw("statistics")?;
                        let neg = self.sym("-");
                        let n = match self.next() {
                            Some(Tok::Num(n)) => n.parse::<i64>().ok(),
                            _ => None,
                        };
                        let Some(n) = n else { return self.fail("expected an integer") };
                        StatAlter::Target(if neg { -n } else { n })
                    }
                };
                StatStmt::Alter { name, if_exists, how }
            }
            _ => {
                let if_exists = guard(self, false)?;
                let mut names = vec![self.stat_name()?];
                while self.sym(",") {
                    names.push(self.stat_name()?);
                }
                let _ = self.kw("cascade") || self.kw("restrict");
                StatStmt::Drop { names, if_exists }
            }
        };
        Ok(Stmt::Foreign(Box::new(foreign::ForeignStmt::Statistics(st))))
    }

    /// One thing CREATE STATISTICS is on: a column, `(expression)`, or a function's call.
    fn stat_item(&mut self) -> Result<Item> {
        if self.sym("(") {
            let (e, text) = self.text_of(|p| p.expr())?;
            self.expect_sym(")")?;
            // (a column in parentheses is the column)
            return Ok(match e {
                Expr::Col(None, n) => Item::Col(n),
                _ => Item::Expr(text),
            });
        }
        if matches!(self.t.get(self.i + 1), Some(Tok::Sym("("))) {
            let (_, text) = self.text_of(|p| p.expr())?;
            return Ok(Item::Expr(text));
        }
        Ok(Item::Col(self.name()?))
    }

    /// FROM's one relation, by name.
    fn stat_from(&mut self) -> Result<Named> {
        let only_names = || err(UNSUPPORTED, "CREATE STATISTICS only supports relation names in the FROM clause");
        if matches!(self.peek(), Some(Tok::Sym("("))) || matches!(self.t.get(self.i + 1), Some(Tok::Sym("("))) {
            return only_names();
        }
        let table = self.stat_name()?;
        if matches!(self.peek(), Some(Tok::Sym("("))) {
            return only_names();
        }
        if self.kw("as")
            || matches!(self.peek(), Some(Tok::Word(w)) if !NOT_ALIASES.contains(&w.as_str()))
            || matches!(self.peek(), Some(Tok::Ident(_)))
        {
            self.name()?;
        }
        if self.at_end() {
            return Ok(table);
        }
        if self.is_sym(",") {
            return err(UNSUPPORTED, "only a single relation is allowed in CREATE STATISTICS");
        }
        only_names()
    }

    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(x)) if *x == s)
    }
}

// ---------------------------------------------------------------- running them

/// The types that have no default B-tree operator class, so no statistics: Postgres's names for them.
const UNORDERED: [&str; 9] = ["json", "xml", "point", "line", "lseg", "box", "path", "polygon", "circle"];

/// 42501 unless the role running the statement may act as object `s`'s owner. `written`: the name as the statement
/// wrote it, which ALTER STATISTICS ... SET STATISTICS and DROP STATISTICS say (the others say the object's own).
fn owned(s: &StatObj, written: Option<&Named>) -> Result<()> {
    match roles::checked() {
        Some(w) if !w.has_privs_of(s.owner) => {
            let shown = match written {
                Some((Some(schema), n)) => format!("{schema}.{n}"),
                Some((None, n)) => n.clone(),
                None => rel_part(&s.name).to_string(),
            };
            err("42501", format!("must be owner of statistics object {shown}"))
        }
        _ => Ok(()),
    }
}

/// The name Postgres makes up for an unnamed object (`ChooseExtendedStatisticName`): the table's, its columns' (an
/// expression is `expr`), and `stat`, made unique in the schema with a number.
fn made_up_name(v: &View, schema: &str, table: &str, items: &[Item]) -> Result<String> {
    let mut words = String::new();
    for i in items {
        let w = match i {
            Item::Col(c) => c.as_str(),
            Item::Expr(_) => "expr",
        };
        if !words.is_empty() {
            words.push('_');
        }
        words.push_str(w);
        if words.len() >= 63 {
            break;
        }
    }
    let base = format!("{table}_{words}");
    let mut n = 0;
    loop {
        let name = if n == 0 { format!("{base}_stat") } else { format!("{base}_stat{n}") };
        if find(v, &qualify(schema, &name))?.is_none() {
            return Ok(name);
        }
        n += 1;
    }
}

/// Runs a statistics statement on `branch`.
pub(super) fn run(db: &Db, branch: &str, st: &StatStmt) -> Result<Output> {
    let (me, superuser) = match roles::current() {
        Some(w) => (w.oid, w.superuser),
        None => (roles::BOOTSTRAP, true),
    };
    let tag = match st {
        StatStmt::Create { .. } => "CREATE STATISTICS",
        StatStmt::Alter { .. } => "ALTER STATISTICS",
        StatStmt::Drop { .. } => "DROP STATISTICS",
    };
    db.batch_with::<(), SqlError>(branch, |v| {
        let cat = Catalog::of(v.core, &v.branch)?;
        let writes = match st {
            StatStmt::Create { name, if_not_exists, kinds, items, table } => {
                let written = table.0.as_ref().map_or_else(|| table.1.clone(), |s| format!("{s}.{}", table.1));
                let Some(oid) = cat.rel_named(table.0.as_deref(), &table.1) else {
                    return err(NO_TABLE, format!("relation \"{written}\" does not exist"));
                };
                let bad_kind = |detail: &str| {
                    Err(SqlError {
                        code: "42809",
                        message: format!("cannot define statistics for relation \"{}\"", table.1),
                        info: Some(Box::new(Violation { detail: Some(detail.to_string()), ..Violation::default() })),
                    })
                };
                let i = match cat.find(oid) {
                    Some(Obj::Rel(i)) => i,
                    Some(Obj::Index(..)) => return bad_kind("This operation is not supported for indexes."),
                    _ if catalog::table_oid(&table.1) == Some(oid) => {
                        return err("42501", format!("permission denied: \"{}\" is a system catalog", table.1));
                    }
                    _ => return err(NO_TABLE, format!("relation \"{written}\" does not exist")),
                };
                let r = &cat.rels[i];
                match r.kind {
                    'r' | 'm' | 'f' | 'p' => {}
                    'v' => return bad_kind("This operation is not supported for views."),
                    'S' => return bad_kind("This operation is not supported for sequences."),
                    _ => return bad_kind("This operation is not supported for composite types."),
                }
                if !superuser
                    && let Some(w) = roles::checked()
                    && let Some(o) = acl::rel(v, &r.name)?
                    && !w.has_privs_of(o.owner)
                {
                    return Err(o.not_owner());
                }
                let t = need_table(v, &r.name)?;
                let name = match name {
                    Some(n) => stored(v, n, true)?,
                    None => {
                        let s = schema_part(&r.name).to_string();
                        qualify(&s, &made_up_name(v, &s, rel_part(&r.name), items)?)
                    }
                };
                // its schema: one that exists, where the role may CREATE (as CreateStatistics checks it)
                let schema = schema_part(&name).to_string();
                if schema != "public" && !namespaces(v)?.contains(&schema) {
                    return err("3F000", format!("schema \"{schema}\" does not exist"));
                }
                if !superuser
                    && let Some(w) = roles::checked()
                {
                    let o = acl::schema(v, &schema)?;
                    if !o.has(&w, acl::CREATE) {
                        return Err(o.denied());
                    }
                }
                if find(v, &name)?.is_some() {
                    if *if_not_exists {
                        notice(
                            "NOTICE",
                            format!("statistics object \"{}\" already exists, skipping", rel_part(&name)),
                        );
                        return Ok((vec![], ()));
                    }
                    return err("42710", format!("statistics object \"{}\" already exists", rel_part(&name)));
                }
                if items.len() > MAX_DIMENSIONS {
                    return err("54011", format!("cannot have more than {MAX_DIMENSIONS} columns in statistics"));
                }
                let system = || err(UNSUPPORTED, "statistics creation on system columns is not supported");
                let mut nums: Vec<usize> = vec![];
                let mut exprs: Vec<(String, String)> = vec![];
                for item in items {
                    match item {
                        Item::Col(c) => {
                            let Some(k) = t.cols.iter().position(|x| x.name == *c && !x.hidden) else {
                                if system_name(c) {
                                    return system();
                                }
                                return err(NO_COLUMN, format!("column \"{c}\" does not exist"));
                            };
                            let ty = t.cols[k].ty.pg_name();
                            if UNORDERED.contains(&ty.as_ref()) {
                                return err(
                                    UNSUPPORTED,
                                    format!(
                                        "column \"{c}\" cannot be used in statistics because its type {ty} has no default btree operator class"
                                    ),
                                );
                            }
                            nums.push(k);
                        }
                        Item::Expr(text) => {
                            let raw = check_expr(text)?;
                            let mut sys = false;
                            walk(&raw, &mut |x| {
                                if let Expr::Col(_, n) = x
                                    && system_name(n)
                                    && !t.cols.iter().any(|c| c.name == *n)
                                {
                                    sys = true;
                                }
                            });
                            if sys {
                                return system();
                            }
                            let e = index_expr(&t, text)?;
                            if items.len() > 1 {
                                let ty = ty_in(&e, &t.cols, &[], &[])?.pg_name();
                                if UNORDERED.contains(&ty.as_ref()) {
                                    return err(
                                        UNSUPPORTED,
                                        format!(
                                            "expression cannot be used in multivariate statistics because its type {ty} has no default btree operator class"
                                        ),
                                    );
                                }
                            }
                            exprs.push((catalog::pretty_def(text, &t.cols), text.clone()));
                        }
                    }
                }
                if items.len() == 1 && exprs.len() == 1 && !kinds.is_empty() {
                    return err(
                        UNSUPPORTED,
                        "when building statistics on a single expression, statistics kinds may not be specified",
                    );
                }
                let mut wanted = [false; 3];
                for k in kinds {
                    match k.as_str() {
                        "ndistinct" => wanted[0] = true,
                        "dependencies" => wanted[1] = true,
                        "mcv" => wanted[2] = true,
                        other => return err(SYNTAX, format!("unrecognized statistics kind \"{other}\"")),
                    }
                }
                if kinds.is_empty() && items.len() >= 2 {
                    wanted = [true; 3];
                }
                if items.len() < 2 && exprs.len() != 1 {
                    return err("42P17", "extended statistics require at least 2 columns");
                }
                nums.sort_unstable();
                if nums.windows(2).any(|w| w[0] == w[1]) {
                    return err("42701", "duplicate column name in statistics definition");
                }
                if exprs.iter().enumerate().any(|(a, x)| exprs.iter().skip(a + 1).any(|y| y.0 == x.0)) {
                    return err("42701", "duplicate expression in statistics definition");
                }
                let mut letters: Vec<char> =
                    ['d', 'f', 'm'].into_iter().zip(wanted).filter(|(_, on)| *on).map(|(c, _)| c).collect();
                if !exprs.is_empty() {
                    letters.push('e');
                }
                let s = StatObj {
                    name,
                    table: r.name.clone(),
                    owner: me,
                    target: -1,
                    kinds: letters,
                    cols: nums.iter().map(|&k| t.cols[k].name.clone()).collect(),
                    exprs: exprs.into_iter().map(|(shown, _)| shown).collect(),
                };
                vec![s.write()]
            }
            StatStmt::Alter { name, if_exists, how } => {
                // (SET STATISTICS's target is checked first, before the object is looked up, as in Postgres)
                let target = match how {
                    StatAlter::Target(n) if *n < -1 => {
                        return err("22023", format!("statistics target {n} is too low"));
                    }
                    StatAlter::Target(n) if *n > 10000 => {
                        notice("WARNING", "lowering statistics target to 10000".into());
                        10000
                    }
                    StatAlter::Target(n) => *n,
                    _ => -1,
                };
                let at = stored(v, name, false)?;
                let Some(mut s) = find(v, &at)? else {
                    if *if_exists {
                        let shown = name.0.as_ref().map_or_else(|| name.1.clone(), |x| format!("{x}.{}", name.1));
                        notice("NOTICE", format!("statistics object \"{shown}\" does not exist, skipping"));
                        return Ok((vec![], ()));
                    }
                    return Err(missing(name));
                };
                // (SET STATISTICS says the name as written, the others the object's own)
                owned(&s, matches!(how, StatAlter::Target(_)).then_some(name))?;
                match how {
                    StatAlter::Target(_) => {
                        s.target = target;
                        vec![s.write()]
                    }
                    // (as AlterObjectOwner_internal: one who isn't a superuser must be able to SET ROLE to the new
                    // owner, which must be able to CREATE in the object's schema)
                    StatAlter::Owner(spec) => {
                        let reg = roles::registry(v.core)?;
                        let to = spec.resolve(&reg)?;
                        if to.oid == s.owner {
                            return Ok((vec![], ()));
                        }
                        if !superuser {
                            if !reg.can_set(me, to.oid) {
                                return err("42501", format!("must be able to SET ROLE \"{}\"", to.name));
                            }
                            let o = acl::schema(v, schema_part(&s.name))?;
                            if !o.has(&roles::Who::for_role(&reg, to.oid), acl::CREATE) {
                                return Err(o.denied());
                            }
                        }
                        s.owner = to.oid;
                        vec![s.write()]
                    }
                    StatAlter::Rename(to) | StatAlter::Schema(to) => {
                        let schema = match how {
                            StatAlter::Schema(_) => {
                                if to != "public" && !namespaces(v)?.contains(to) {
                                    return err("3F000", format!("schema \"{to}\" does not exist"));
                                }
                                // (into the schema it is in: nothing to do, as in Postgres)
                                if to == schema_part(&s.name) {
                                    return Ok((vec![], ()));
                                }
                                to.clone()
                            }
                            _ => schema_part(&s.name).to_string(),
                        };
                        // (with CREATE on the schema it is, or goes, in: AlterObjectRename_internal and
                        // AlterObjectNamespace_internal)
                        if !superuser
                            && let Some(w) = roles::checked()
                        {
                            let o = acl::schema(v, &schema)?;
                            if !o.has(&w, acl::CREATE) {
                                return Err(o.denied());
                            }
                        }
                        let bare = if matches!(how, StatAlter::Rename(_)) { to.clone() } else { rel_part(&s.name).into() };
                        let new = qualify(&schema, &bare);
                        if new != s.name && find(v, &new)?.is_some() {
                            return err(
                                "42710",
                                format!("statistics object \"{bare}\" already exists in schema \"{schema}\""),
                            );
                        }
                        let gone = (key(&s.name), None);
                        s.name = new;
                        vec![gone, s.write()]
                    }
                }
            }
            StatStmt::Drop { names, if_exists } => {
                let mut writes = vec![];
                for n in names {
                    let at = stored(v, n, false)?;
                    match find(v, &at)? {
                        Some(s) => {
                            owned(&s, Some(n))?;
                            writes.push((key(&s.name), None));
                        }
                        None if *if_exists => {
                            let shown = n.0.as_ref().map_or_else(|| n.1.clone(), |x| format!("{x}.{}", n.1));
                            notice("NOTICE", format!("statistics object \"{shown}\" does not exist, skipping"));
                        }
                        None => return Err(missing(n)),
                    }
                }
                writes
            }
        };
        let _ = &cat;
        Ok((writes, ()))
    })?;
    Ok(Output::tag(tag))
}

/// The writes that drop the statistics objects of dropped tables (stored names).
pub(super) fn forget(v: &View, tables: &[String]) -> Result<Vec<(String, Option<String>)>> {
    Ok(load(v)?.into_iter().filter(|s| tables.contains(&s.table)).map(|s| (key(&s.name), None)).collect())
}

// ---------------------------------------------------------------- the catalog

const STAT_KIND: i64 = 11;

/// The objects that stand (their table is there with their columns), each with its relation and oid.
fn standing(cat: &Catalog) -> Vec<(&StatObj, usize, i64)> {
    let oids = foreign::name_oids(STAT_KIND, &cat.stats.iter().map(|s| s.name.clone()).collect::<Vec<_>>());
    cat.stats
        .iter()
        .zip(oids)
        .filter_map(|(s, o)| {
            let i = cat.rels.iter().position(|r| r.name == s.table)?;
            s.names().iter().all(|c| cat.rels[i].columns.iter().any(|x| x.0 == *c)).then_some((s, i, o))
        })
        .collect()
}

/// The statistics objects that stand, as `pg_statistic_ext` has them: each one's stored name, relation and oid.
pub(super) fn object_oids(cat: &Catalog) -> Vec<(String, usize, i64)> {
    standing(cat).into_iter().map(|(s, i, o)| (s.name.clone(), i, o)).collect()
}

/// An object's columns' attnums, in column order.
fn attnums(cat: &Catalog, s: &StatObj, i: usize) -> Vec<i64> {
    let mut nums: Vec<i64> = s
        .cols
        .iter()
        .filter_map(|c| cat.rels[i].columns.iter().position(|x| x.0 == *c).map(|k| cat.rels[i].attnum(k)))
        .collect();
    nums.sort_unstable();
    nums
}

/// Whether an expression as written back reads as a function's call (`date_trunc('day', d)`), which
/// pg_get_statisticsobjdef shows without parentheses.
fn function_like(e: &str) -> bool {
    let Some(open) = e.find('(') else { return false };
    let head = &e[..open];
    if head.is_empty() || !head.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '"') {
        return false;
    }
    // the parenthesis it opens closes at the end
    let mut depth = 0;
    let mut quoted = false;
    for (k, c) in e.char_indices().skip(open) {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return k == e.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// What pg_get_statisticsobjdef_columns gives: the columns, then the expressions.
fn on_list(cat: &Catalog, s: &StatObj, i: usize) -> String {
    let mut parts: Vec<String> = attnums(cat, s, i)
        .iter()
        .filter_map(|&n| cat.rels[i].at_attnum(n).and_then(|j| cat.rels[i].columns.get(j)).map(|c| ident(&c.0)))
        .collect();
    parts.extend(s.exprs.iter().map(|e| if function_like(e) { e.clone() } else { format!("({e})") }));
    parts.join(", ")
}

/// pg_get_statisticsobjdef (or with `columns`, pg_get_statisticsobjdef_columns) of the object with this oid.
pub(super) fn def(cat: &Catalog, oid: i64, columns: bool) -> Option<String> {
    let (s, i, _) = standing(cat).into_iter().find(|(_, _, o)| *o == oid)?;
    let on = on_list(cat, s, i);
    if columns {
        return Some(on);
    }
    let n = s.cols.len() + s.exprs.len();
    let has = |c: char| s.kinds.contains(&c);
    let mut kinds = String::new();
    if (!has('d') || !has('f') || !has('m')) && n > 1 {
        let on: Vec<&str> = [('d', "ndistinct"), ('f', "dependencies"), ('m', "mcv")]
            .into_iter()
            .filter(|(c, _)| has(*c))
            .map(|(_, w)| w)
            .collect();
        kinds = format!(" ({})", on.join(", "));
    }
    let name = format!("{}.{}", ident(schema_part(&s.name)), ident(rel_part(&s.name)));
    let table = catalog::reg_shown(cat, Reg::Class, cat.rel_oid(i));
    Some(format!("CREATE STATISTICS {name}{kinds} ON {on} FROM {table}"))
}

/// pg_statistics_obj_is_visible: its schema is on the search_path (None: no such object).
pub(super) fn visible(cat: &Catalog, oid: i64) -> Option<bool> {
    let (s, ..) = standing(cat).into_iter().find(|(_, _, o)| *o == oid)?;
    let schema = schema_part(&s.name);
    Some(path_schemas().unwrap_or_else(|| vec!["public".into()]).iter().any(|p| p == schema))
}

/// Catalog table `name`'s rows of the world's statistics objects: pg_statistic_ext (pg_statistic_ext_data,
/// pg_stats_ext and pg_stats_ext_exprs have none: nothing is ever built).
pub(super) fn catalog_rows(name: &str, cat: &Catalog) -> Vec<Vec<Value>> {
    use Value::{Int as int, Null};
    let t = |s: &str| Value::Text(s.into());
    if name != "pg_statistic_ext" {
        return vec![];
    }
    standing(cat)
        .into_iter()
        .map(|(s, i, o)| {
            let keys = attnums(cat, s, i).iter().map(i64::to_string).collect::<Vec<_>>().join(" ");
            let kinds = s.kinds.iter().map(char::to_string).collect::<Vec<_>>().join(",");
            catalog_schema::named_row(
                name,
                vec![
                    ("oid", int(o)),
                    ("stxrelid", int(cat.rel_oid(i))),
                    ("stxname", t(rel_part(&s.name))),
                    ("stxnamespace", int(cat.schema_oid(schema_part(&s.name)))),
                    ("stxowner", int(s.owner)),
                    ("stxstattarget", int(s.target)),
                    ("stxkeys", t(&keys)),
                    ("stxkind", t(&format!("{{{kinds}}}"))),
                    ("stxexprs", if s.exprs.is_empty() { Null } else { t(&s.exprs.join(", ")) }),
                ],
            )
        })
        .collect()
}

/// psql's "Statistics objects:" of table `name` (stored): oid, table (as regclass shows it), schema, name, columns,
/// whether ndistinct, dependencies and mcv are built, and the target, as text, by schema and name.
pub(super) fn shown_for_table(cat: &Catalog, name: &str) -> Vec<Vec<Value>> {
    let t = |s: &str| Value::Text(s.into());
    let b = |x: bool| t(if x { "t" } else { "f" });
    let mut found: Vec<(String, String, Vec<Value>)> = standing(cat)
        .into_iter()
        .filter(|(s, ..)| s.table == name)
        .map(|(s, i, o)| {
            let (nsp, stx) = (schema_part(&s.name).to_string(), rel_part(&s.name).to_string());
            let row = vec![
                t(&o.to_string()),
                t(&catalog::reg_shown(cat, Reg::Class, cat.rel_oid(i))),
                t(&nsp),
                t(&stx),
                t(&on_list(cat, s, i)),
                b(s.kinds.contains(&'d')),
                b(s.kinds.contains(&'f')),
                b(s.kinds.contains(&'m')),
                t(&s.target.to_string()),
            ];
            (nsp, stx, row)
        })
        .collect();
    found.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    found.into_iter().map(|(_, _, r)| r).collect()
}
