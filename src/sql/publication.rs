//! Publications: CREATE, ALTER and DROP PUBLICATION. Chronos has no logical replication to send a publication's
//! changes with, so a publication is its definition: kept as a row of the world (`_sonos_defs/pub/<name>/`, JSON),
//! checked as Postgres checks it, and shown where Postgres shows it (pg_publication, pg_publication_rel,
//! pg_publication_namespace, pg_publication_tables, psql's `\dRp+` and the "Publications:" of `\d`).
//!
//! A table a publication names is kept by its stored name, a schema by its name: a dropped table leaves its
//! publications (see `forget`), and what no longer exists isn't shown.

use super::catalog::{Catalog, Obj};
use super::*;

/// Where publications are kept (as before, when only their names were).
pub(super) const KEY: &str = "_sonos_defs/pub/";

fn key(name: &str) -> String {
    format!("{KEY}{name}/")
}

/// A table a publication publishes: its stored name, its column list as written, its WHERE as written and as
/// Postgres writes it back (`pg_get_expr`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Member {
    pub table: String,
    pub cols: Option<Vec<String>>,
    pub filter: Option<String>,
    pub shown: Option<String>,
}

/// A publication as its row keeps it.
#[derive(Clone, Debug)]
pub(crate) struct Publication {
    pub name: String,
    pub owner: i64,
    /// FOR ALL TABLES
    pub all: bool,
    /// publish = insert, update, delete, truncate
    pub publish: [bool; 4],
    /// publish_via_partition_root
    pub via_root: bool,
    pub tables: Vec<Member>,
    /// FOR TABLES IN SCHEMA, by name
    pub schemas: Vec<String>,
}

impl Publication {
    fn from_json(name: &str, j: &Json) -> Publication {
        let strs = |x: &Json| -> Vec<String> {
            x.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
        };
        let publish = match j["publish"].as_array() {
            Some(a) => std::array::from_fn(|k| a.get(k).and_then(Json::as_bool).unwrap_or(true)),
            None => [true; 4],
        };
        let tables = j["tables"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                Some(Member {
                    table: m["table"].as_str()?.to_string(),
                    cols: m["cols"].as_array().map(|_| strs(&m["cols"])),
                    filter: m["where"].as_str().map(String::from),
                    shown: m["shown"].as_str().map(String::from),
                })
            })
            .collect();
        Publication {
            name: name.to_string(),
            owner: j["owner"].as_i64().unwrap_or(roles::BOOTSTRAP),
            all: j["all"] == true,
            publish,
            via_root: j["via_root"] == true,
            tables,
            schemas: strs(&j["schemas"]),
        }
    }

    fn write(&self) -> (String, Option<String>) {
        let tables: Vec<Json> = self
            .tables
            .iter()
            .map(|m| serde_json::json!({ "table": m.table, "cols": m.cols, "where": m.filter, "shown": m.shown }))
            .collect();
        let j = serde_json::json!({
            "owner": self.owner,
            "all": self.all,
            "publish": self.publish,
            "via_root": self.via_root,
            "tables": tables,
            "schemas": self.schemas,
        });
        (key(&self.name), Some(j.to_string()))
    }
}

/// Every publication of the world, in name order.
pub(crate) fn load(v: &View) -> Result<Vec<Publication>> {
    let mut out = vec![];
    for r in v.prefix(KEY)? {
        let (k, raw) = r?;
        let Some(name) = k[KEY.len()..].strip_suffix('/') else { continue };
        out.push(Publication::from_json(name, &serde_json::from_str(&raw).unwrap_or_default()));
    }
    Ok(out)
}

/// What an UPDATE (`update`) or DELETE of table `t` needs of the publications it is in, as Postgres's
/// `CheckCmdReplicaIdentity` checks before a row is read: a WHERE of a publication that publishes the change may read
/// only replica identity columns, and its column list must cover them (a table of REPLICA IDENTITY FULL may have none);
/// then the table needs a replica identity (its primary key, an index USING INDEX named, or FULL) if one publishes it.
/// A partition is in its ancestors' publications too; with publish_via_partition_root, the topmost ancestor the
/// publication names lends its WHERE and column list.
pub(super) fn check_replica_identity(v: &View, t: &Table, update: bool) -> Result<()> {
    let pubs = load(v)?;
    if pubs.is_empty() || !t.schema {
        return Ok(());
    }
    // the partition's ancestors, nearest first
    let mut ancestors: Vec<Table> = vec![];
    let mut up = t.hier.of.as_ref().map(|(p, _)| p.clone());
    while let Some(p) = up {
        if ancestors.iter().any(|a| a.name == p) {
            break;
        }
        let Some(a) = super::load(v, &p)? else { break };
        up = a.hier.of.as_ref().map(|(p, _)| p.clone());
        ancestors.push(a);
    }
    let rels: Vec<&Table> = std::iter::once(t).chain(&ancestors).collect();
    let replident = t.hier.replident.as_ref().map_or('d', |r| r.0);
    let name_of = |id: u32| t.cols.iter().find(|c| c.id == id).map(|c| c.name.clone());
    // the replica identity index's columns (INDEX_ATTR_BITMAP_IDENTITY_KEY), if the table has one
    let identity: Option<Vec<String>> = match (replident, t.hier.replident.as_ref().and_then(|r| r.1.as_deref())) {
        ('d', _) if t.has_key() => Some(t.key_cols().iter().map(|&i| t.cols[i].name.clone()).collect()),
        ('d', _) => t.uniques.iter().find(|u| u.primary).map(|u| u.cols.iter().filter_map(|&c| name_of(c)).collect()),
        ('i', Some(x)) if x == t.pkey_name() && t.has_key() => {
            Some(t.key_cols().iter().map(|&i| t.cols[i].name.clone()).collect())
        }
        ('i', Some(x)) => t
            .uniques
            .iter()
            .find(|u| u.name == x)
            .map(|u| u.cols.clone())
            .or_else(|| t.indexes.iter().find(|i| i.name == x).map(|i| i.cols.clone()))
            .map(|ids| ids.iter().filter_map(|&c| name_of(c)).collect()),
        _ => None,
    };
    let key: &[String] = identity.as_deref().unwrap_or(&[]);
    let (mut publishes, mut filter_ok, mut cols_ok) = (false, true, true);
    for p in &pubs {
        let named = |r: &Table| p.tables.iter().any(|m| m.table == r.name);
        let in_schema = |r: &Table| p.schemas.iter().any(|s| s == schema_part(&r.name));
        if !(p.all || rels.iter().any(|r| named(r) || in_schema(r))) {
            continue;
        }
        let does = if update { p.publish[1] } else { p.publish[2] };
        publishes |= does;
        if !(p.publish[1] || p.publish[2]) {
            continue;
        }
        // the member whose WHERE and column list apply: the table's own, or with publish_via_partition_root the
        // topmost ancestor the publication publishes (GetTopMostAncestorInPublication)
        let mut from: &Table = t;
        if p.via_root
            && let Some(a) = ancestors.iter().rev().find(|a| named(a) || in_schema(a))
        {
            from = a;
        }
        let Some(m) = p.tables.iter().find(|m| m.table == from.name) else { continue };
        if let Some(text) = m.filter.as_deref()
            && replident != 'f'
            && let Ok(e) = check_expr(text)
        {
            let mut read = false;
            walk(&e, &mut |x| {
                if let Expr::Col(_, c) = x
                    && !key.contains(c)
                {
                    read = true;
                }
            });
            if read && does {
                filter_ok = false;
            }
        }
        if let Some(cols) = &m.cols
            && (replident == 'f' || key.iter().any(|k| !cols.contains(k)))
            && does
        {
            cols_ok = false;
        }
    }
    let table = rel_part(&t.name);
    let (verb, what) = if update { ("update", "updates") } else { ("delete from", "deletes") };
    if !filter_ok {
        return Err(with_detail(
            "42P10",
            format!("cannot {verb} table \"{table}\""),
            "Column used in the publication WHERE expression is not part of the replica identity.",
        ));
    }
    if !cols_ok {
        return Err(with_detail(
            "42P10",
            format!("cannot {verb} table \"{table}\""),
            "Column list used by the publication does not cover the replica identity.",
        ));
    }
    if publishes && identity.is_none() && replident != 'f' {
        let hint = if update { "updating" } else { "deleting from" };
        return Err(SqlError {
            code: "55000",
            message: format!(
                "cannot {verb} table \"{table}\" because it does not have a replica identity and publishes {what}"
            ),
            info: Some(Box::new(Violation {
                hint: Some(format!("To enable {hint} the table, set REPLICA IDENTITY using ALTER TABLE.")),
                ..Violation::default()
            })),
        });
    }
    Ok(())
}

fn find(v: &View, name: &str) -> Result<Option<Publication>> {
    Ok(v.get(&key(name))?.map(|raw| Publication::from_json(name, &serde_json::from_str(&raw).unwrap_or_default())))
}

fn missing(name: &str) -> SqlError {
    SqlError { code: "42704", message: format!("publication \"{name}\" does not exist"), info: None }
}

fn with_detail(code: &'static str, message: String, detail: &str) -> SqlError {
    SqlError {
        code,
        message,
        info: Some(Box::new(Violation { detail: Some(detail.to_string()), ..Violation::default() })),
    }
}

// ---------------------------------------------------------------- statements

/// What a publication's FOR, ADD, SET or DROP names.
#[derive(Clone, Debug)]
pub(crate) enum PubObj {
    /// TABLE [ONLY] [schema.]name [*] [(columns)] [WHERE (condition)]: the WHERE as written
    Table { schema: Option<String>, name: String, only: bool, cols: Option<Vec<String>>, filter: Option<String> },
    /// TABLES IN SCHEMA name, or CURRENT_SCHEMA (None)
    Schema(Option<String>),
}

/// WITH (publish = '...', publish_via_partition_root = ...): what was given.
#[derive(Clone, Debug, Default)]
pub(crate) struct Opts {
    publish: Option<[bool; 4]>,
    via_root: Option<bool>,
}

/// The options, or the error they make: raised where Postgres raises it, after the publication is looked up.
type OptsOr = std::result::Result<Opts, SqlError>;

#[derive(Clone, Debug)]
pub(crate) enum PubStmt {
    Create { name: String, all: bool, objs: Vec<PubObj>, opts: OptsOr },
    Alter { name: String, how: How },
    Drop { names: Vec<String>, if_exists: bool },
}

/// What ALTER PUBLICATION does.
#[derive(Clone, Debug)]
pub(crate) enum How {
    Add(Vec<PubObj>),
    Set(Vec<PubObj>),
    Drop(Vec<PubObj>),
    Options(OptsOr),
    Owner(roles::Spec),
    Rename(String),
}

impl Parser {
    /// A syntax error (42601) with a message of its own, at token `at`.
    fn error_at(&self, at: usize, message: &str, detail: Option<&str>) -> SqlError {
        SqlError {
            code: SYNTAX,
            message: message.to_string(),
            info: Some(Box::new(Violation {
                position: self.starts.get(at).map(|s| *s as u32 + 1),
                detail: detail.map(String::from),
                ..Violation::default()
            })),
        }
    }

    /// After CREATE, ALTER or DROP PUBLICATION.
    pub(super) fn publication(&mut self, verb: &str) -> Result<Stmt> {
        let st = match verb {
            "create" => {
                let name = self.name()?;
                let (mut all, mut objs) = (false, vec![]);
                if self.kw("for") {
                    if self.kw("all") {
                        self.expect_kw("tables")?;
                        all = true;
                    } else {
                        objs = self.pub_objects()?;
                    }
                }
                let opts = if self.kw("with") { self.pub_definition()? } else { Ok(Opts::default()) };
                PubStmt::Create { name, all, objs, opts }
            }
            "alter" => {
                let name = self.name()?;
                let how = if self.kw("add") {
                    How::Add(self.pub_objects()?)
                } else if self.kw("drop") {
                    How::Drop(self.pub_objects()?)
                } else if self.kw("set") {
                    if matches!(self.peek(), Some(Tok::Sym("("))) {
                        How::Options(self.pub_definition()?)
                    } else {
                        How::Set(self.pub_objects()?)
                    }
                } else if self.kw("owner") {
                    self.expect_kw("to")?;
                    How::Owner(self.role_spec(false)?)
                } else if self.kw("rename") {
                    self.expect_kw("to")?;
                    How::Rename(self.name()?)
                } else {
                    return self.fail("expected ADD, DROP, SET, OWNER TO or RENAME TO");
                };
                PubStmt::Alter { name, how }
            }
            _ => {
                let if_exists = if self.kw("if") {
                    self.expect_kw("exists")?;
                    true
                } else {
                    false
                };
                let mut names = vec![self.name()?];
                while self.sym(",") {
                    names.push(self.name()?);
                }
                let _ = self.kw("cascade") || self.kw("restrict");
                PubStmt::Drop { names, if_exists }
            }
        };
        Ok(Stmt::Foreign(Box::new(foreign::ForeignStmt::Publication(st))))
    }

    /// `TABLE ... | TABLES IN SCHEMA ... [, ...]`, where an item without either is of the kind before it.
    fn pub_objects(&mut self) -> Result<Vec<PubObj>> {
        let mut out = vec![];
        // the kind the last item was: tables (true) or schemas (false)
        let mut kind: Option<bool> = None;
        loop {
            let at = self.i;
            if self.kw("table") {
                kind = Some(true);
                out.push(self.pub_table()?);
            } else if self.is_kw("tables") {
                self.i += 1;
                self.expect_kw("in")?;
                self.expect_kw("schema")?;
                kind = Some(false);
                if self.kw("current_schema") {
                    out.push(PubObj::Schema(None));
                } else {
                    out.push(PubObj::Schema(Some(self.name()?)));
                }
            } else {
                match kind {
                    None => {
                        return Err(self.error_at(
                            at,
                            "invalid publication object list",
                            Some(
                                "One of TABLE or TABLES IN SCHEMA must be specified before a standalone table or schema name.",
                            ),
                        ));
                    }
                    Some(true) => {
                        if self.is_kw("current_schema") {
                            return Err(self.error_at(at, "invalid table name", None));
                        }
                        out.push(self.pub_table()?);
                    }
                    Some(false) => {
                        if self.kw("current_schema") {
                            out.push(PubObj::Schema(None));
                        } else {
                            let name = self.name()?;
                            let dotted = self.sym(".");
                            if dotted {
                                self.name()?;
                            }
                            let cols = matches!(self.peek(), Some(Tok::Sym("(")));
                            if cols {
                                self.i += 1;
                                self.names()?;
                            }
                            if self.is_kw("where") {
                                return Err(self.error_at(at, "WHERE clause not allowed for schema", None));
                            }
                            if cols {
                                return Err(self.error_at(at, "column specification not allowed for schema", None));
                            }
                            if dotted {
                                return Err(self.error_at(at, "invalid schema name", None));
                            }
                            out.push(PubObj::Schema(Some(name)));
                        }
                    }
                }
            }
            if !self.sym(",") {
                break;
            }
        }
        Ok(out)
    }

    /// `[ONLY] [schema.]name [*] [(columns)] [WHERE (condition)]`.
    fn pub_table(&mut self) -> Result<PubObj> {
        let only = self.kw("only");
        let first = self.name()?;
        let (schema, name) = if self.sym(".") { (Some(first), self.name()?) } else { (None, first) };
        self.sym("*");
        let cols = if self.sym("(") { Some(self.names()?) } else { None };
        let filter = if self.kw("where") {
            self.expect_sym("(")?;
            let (_, text) = self.text_of(|p| p.expr())?;
            self.expect_sym(")")?;
            Some(text)
        } else {
            None
        };
        Ok(PubObj::Table { schema, name, only, cols, filter })
    }

    /// `( option [= value] [, ...] )`: the options, or the first error Postgres raises for them.
    fn pub_definition(&mut self) -> Result<OptsOr> {
        self.expect_sym("(")?;
        let mut opts = Opts::default();
        let mut bad: Option<SqlError> = None;
        loop {
            let at = self.i;
            let name = match self.next() {
                Some(Tok::Word(w)) => w,
                Some(Tok::Ident(w)) => w,
                t => {
                    self.put_back(t);
                    return self.fail("expected an option name");
                }
            };
            let value = if self.sym("=") {
                match self.next() {
                    Some(Tok::Str(s) | Tok::Num(s) | Tok::Word(s) | Tok::Ident(s)) => Some(s),
                    t => {
                        self.put_back(t);
                        return self.fail("expected a value");
                    }
                }
            } else {
                None
            };
            if bad.is_none()
                && let Err(e) = self.pub_option(&mut opts, &name, value.as_deref(), at)
            {
                bad = Some(e);
            }
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(match bad {
            Some(e) => Err(e),
            None => Ok(opts),
        })
    }

    /// One option of a publication's WITH, as Postgres's `parse_publication_options` takes it.
    fn pub_option(&self, opts: &mut Opts, name: &str, value: Option<&str>, at: usize) -> Result<()> {
        let redundant = || self.error_at(at, "conflicting or redundant options", None);
        match name {
            "publish" => {
                if opts.publish.is_some() {
                    return Err(redundant());
                }
                let Some(text) = value else { return err(SYNTAX, "publish requires a parameter") };
                let mut publish = [false; 4];
                if !text.trim().is_empty() {
                    for part in text.split(',') {
                        let part = part.trim();
                        let word = match part.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
                            Some(quoted) => quoted.to_string(),
                            None => part.to_lowercase(),
                        };
                        match word.as_str() {
                            "insert" => publish[0] = true,
                            "update" => publish[1] = true,
                            "delete" => publish[2] = true,
                            "truncate" => publish[3] = true,
                            "" => return err(SYNTAX, "invalid list syntax in parameter \"publish\""),
                            other => {
                                return err(
                                    SYNTAX,
                                    format!("unrecognized value for publication option \"publish\": \"{other}\""),
                                );
                            }
                        }
                    }
                }
                opts.publish = Some(publish);
            }
            "publish_via_partition_root" => {
                if opts.via_root.is_some() {
                    return Err(redundant());
                }
                opts.via_root = Some(match value.map(str::to_lowercase).as_deref() {
                    None => true,
                    Some("true" | "on" | "1" | "yes" | "t" | "y") => true,
                    Some("false" | "off" | "0" | "no" | "f" | "n") => false,
                    Some(_) => return err(SYNTAX, "publish_via_partition_root requires a Boolean value"),
                });
            }
            _ => return err(SYNTAX, format!("unrecognized publication parameter: \"{name}\"")),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- running them

/// A table a statement names, found: the relation (in the catalog), how it was named, and what was given for it.
struct Wanted {
    rel: usize,
    cols: Option<Vec<String>>,
    filter: Option<String>,
}

/// Who runs the statement: their role, and whether they are a superuser.
fn me() -> (i64, bool) {
    match roles::current() {
        Some(w) => (w.oid, w.superuser),
        None => (roles::BOOTSTRAP, true),
    }
}

/// The schemas the objects name, each once, in order: 3F000 for one that isn't there.
fn schemas_of(cat: &Catalog, objs: &[PubObj]) -> Result<Vec<String>> {
    let exists = |s: &str| {
        s == "public" || cat.schemas.iter().any(|x| x == s) || matches!(s, "pg_catalog" | "information_schema")
    };
    let mut out: Vec<String> = vec![];
    for o in objs {
        let PubObj::Schema(s) = o else { continue };
        let s = match s {
            Some(s) => s.clone(),
            None => {
                // the first schema of the search_path that exists
                let path = path_schemas().unwrap_or_else(|| vec!["public".into()]);
                match path.into_iter().find(|s| exists(s)) {
                    Some(s) => s,
                    None => return err("3F000", "no schema has been selected for CURRENT_SCHEMA"),
                }
            }
        };
        if !exists(&s) {
            return err("3F000", format!("schema \"{s}\" does not exist"));
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    Ok(out)
}

/// What a relation's kind is, in Postgres's "This operation is not supported for ..." words.
fn kind_detail(kind: char) -> &'static str {
    match kind {
        'v' => "This operation is not supported for views.",
        'm' => "This operation is not supported for materialized views.",
        'S' => "This operation is not supported for sequences.",
        'f' => "This operation is not supported for foreign tables.",
        'c' => "This operation is not supported for composite types.",
        _ => "This operation is not supported for indexes.",
    }
}

/// The tables the objects name, found, with those that inherit from them where the table isn't ONLY (as Postgres's
/// `OpenTableList` has them): 42P01 for one that isn't there, and a table named twice where one has a WHERE or a
/// column list refused.
fn tables_of(cat: &Catalog, objs: &[PubObj]) -> Result<Vec<Wanted>> {
    let mut out: Vec<Wanted> = vec![];
    for o in objs {
        let PubObj::Table { schema, name, only, cols, filter } = o else { continue };
        let written = schema.as_ref().map_or_else(|| name.clone(), |s| format!("{s}.{name}"));
        let Some(oid) = cat.rel_named(schema.as_deref(), name) else {
            let stored = qualify(schema.as_deref().unwrap_or("public"), name);
            if schema.is_none() && has_temp(name) || has_temp(&stored) {
                return Err(with_detail(
                    "22023",
                    format!("cannot add relation \"{name}\" to publication"),
                    "This operation is not supported for temporary tables.",
                ));
            }
            return err(NO_TABLE, format!("relation \"{written}\" does not exist"));
        };
        let rel = match cat.find(oid) {
            Some(Obj::Rel(i)) => i,
            Some(Obj::Index(..)) => {
                return Err(with_detail(
                    "22023",
                    format!("cannot add relation \"{name}\" to publication"),
                    kind_detail('i'),
                ));
            }
            _ if catalog::table_oid(name) == Some(oid) => {
                return Err(with_detail(
                    "22023",
                    format!("cannot add relation \"{name}\" to publication"),
                    "This operation is not supported for system tables.",
                ));
            }
            _ => return err(NO_TABLE, format!("relation \"{written}\" does not exist")),
        };
        let r = &cat.rels[rel];
        let mut each = vec![rel];
        // (a partitioned table's partitions aren't named with it: publishing it publishes them)
        if !only && r.kind == 'r' {
            let mut k = 0;
            while k < each.len() {
                let parent = cat.rels[each[k]].name.clone();
                for (i, c) in cat.rels.iter().enumerate() {
                    if c.hier.parents.contains(&parent) && !each.contains(&i) {
                        each.push(i);
                    }
                }
                k += 1;
            }
        }
        for (n, i) in each.into_iter().enumerate() {
            if let Some(w) = out.iter().find(|w| w.rel == i) {
                let shown = rel_part(&cat.rels[if n == 0 { i } else { rel }].name).to_string();
                if filter.is_some() || w.filter.is_some() {
                    return err("42710", format!("conflicting or redundant WHERE clauses for table \"{shown}\""));
                }
                if cols.is_some() || w.cols.is_some() {
                    return err("42710", format!("conflicting or redundant column lists for table \"{shown}\""));
                }
                continue;
            }
            out.push(Wanted { rel: i, cols: cols.clone(), filter: filter.clone() });
        }
    }
    Ok(out)
}

/// Checks each WHERE as Postgres's `TransformPubWhereClauses` does (and works out how it is written back), and each
/// column list as `CheckPubRelationColumnList` does.
fn check_tables(
    v: &View,
    cat: &Catalog,
    pubname: &str,
    via_root: bool,
    with_schemas: bool,
    tables: &[Wanted],
) -> Result<Vec<Option<String>>> {
    let mut shown = vec![];
    for w in tables {
        let r = &cat.rels[w.rel];
        let Some(text) = &w.filter else {
            shown.push(None);
            continue;
        };
        if !via_root && r.kind == 'p' {
            return Err(with_detail(
                "22023",
                format!("cannot use publication WHERE clause for relation \"{}\"", rel_part(&r.name)),
                "WHERE clause cannot be used for a partitioned table when publish_via_partition_root is false.",
            ));
        }
        let t = need_table(v, &r.name)?;
        let raw = check_expr(text)?;
        let (mut agg, mut sub, mut system, mut user) = (false, false, false, false);
        walk(&raw, &mut |x| match x {
            Expr::Agg(..) | Expr::AggRef(_) => agg = true,
            Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) => sub = true,
            Expr::Col(_, n) if system_name(n) && !t.cols.iter().any(|c| c.name == *n) => system = true,
            Expr::Call(c) if !c.builtin => user = true,
            // (current_date and current_user are SQLValueFunctions there, not function calls: of no kind it takes)
            Expr::Func(
                Func::CurrentDate | Func::Ctx(CtxFn::CurrentUser | CtxFn::SessionUser | CtxFn::CurrentRole),
                _,
            ) => sub = true,
            // a built-in function that isn't IMMUTABLE (random(), now(), gen_random_uuid(), to_char() ...), as
            // Postgres's contain_mutable_or_user_functions_checker finds it
            Expr::Call(c) => {
                let name = c.name.trim_start_matches(PLAIN);
                user |= catalog_schema::builtin_mutable(name.strip_prefix("pg_catalog.").unwrap_or(name));
            }
            Expr::Func(f, _) => user |= catalog_schema::builtin_mutable(&func_name(f)),
            _ => {}
        });
        // (and what is STABLE in a time zone's way: a timestamptz's date, to_char of one ...)
        walk(&raw, &mut |x| user |= hier::mutable_here(x, &t.cols));
        if agg {
            return err("42803", "aggregate functions are not allowed in WHERE");
        }
        let invalid =
            |detail: &str| Err(with_detail(UNSUPPORTED, "invalid publication WHERE expression".into(), detail));
        if system {
            return invalid("System columns are not allowed.");
        }
        if sub {
            return invalid(
                "Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed.",
            );
        }
        let e = index_expr(&t, text)?;
        match ty_in(&e, &t.cols, &[], &[])? {
            Type::Bool => {}
            _ if matches!(e, Expr::Lit(Value::Null)) => {}
            ty => {
                return err(
                    WRONG_TYPE,
                    format!("argument of PUBLICATION WHERE must be type boolean, not type {}", ty.pg_name()),
                );
            }
        }
        // (a function of its own, or a built-in one that isn't immutable: the clock, random(), ...)
        if user || hier::not_immutable(&e, &t.cols) {
            return invalid("User-defined or built-in mutable functions are not allowed.");
        }
        let mut enum_col = false;
        walk(&raw, &mut |x| {
            if let Expr::Col(_, n) = x
                && t.cols.iter().any(|c| c.name == *n && matches!(c.ty, Type::Enum(_)))
            {
                enum_col = true;
            }
        });
        if enum_col {
            return invalid("User-defined types are not allowed.");
        }
        shown.push(Some(catalog::expr_def(text, &t.cols, true)));
    }
    for w in tables.iter().filter(|w| w.cols.is_some()) {
        let r = &cat.rels[w.rel];
        let named = format!("{}.{}", schema_part(&r.name), rel_part(&r.name));
        if with_schemas {
            return Err(with_detail(
                "22023",
                format!("cannot use column list for relation \"{named}\" in publication \"{pubname}\""),
                "Column lists cannot be specified in publications containing FOR TABLES IN SCHEMA elements.",
            ));
        }
        if !via_root && r.kind == 'p' {
            return Err(with_detail(
                "22023",
                format!("cannot use column list for relation \"{named}\" in publication \"{pubname}\""),
                "Column lists cannot be specified for partitioned tables when publish_via_partition_root is false.",
            ));
        }
    }
    Ok(shown)
}

/// Adds the tables to publication `p`, as Postgres's `publication_add_relation` does each: one already there is
/// skipped (`quiet`) or refused; it has to be a table, and one the role owns; its column list's columns must be
/// columns of it, and not generated ones.
fn add_tables(
    v: &View,
    cat: &Catalog,
    p: &mut Publication,
    tables: Vec<Wanted>,
    shown: Vec<Option<String>>,
    quiet: bool,
) -> Result<()> {
    let (_, superuser) = me();
    for (w, shown) in tables.into_iter().zip(shown) {
        let r = &cat.rels[w.rel];
        let rel = rel_part(&r.name);
        if p.tables.iter().any(|m| m.table == r.name) {
            if quiet {
                continue;
            }
            return err("42710", format!("relation \"{rel}\" is already member of publication \"{}\"", p.name));
        }
        if !matches!(r.kind, 'r' | 'p') {
            return Err(with_detail(
                "22023",
                format!("cannot add relation \"{rel}\" to publication"),
                kind_detail(r.kind),
            ));
        }
        if !superuser
            && let Some(who) = roles::checked()
            && let Some(o) = acl::rel(v, &r.name)?
            && !who.has_privs_of(o.owner)
        {
            return Err(o.not_owner());
        }
        if let Some(cols) = &w.cols {
            let mut seen: Vec<&str> = vec![];
            for c in cols {
                match r.columns.iter().position(|x| x.0 == *c) {
                    None if system_name(c) => {
                        return err("42P10", format!("cannot use system column \"{c}\" in publication column list"));
                    }
                    None => return err(NO_COLUMN, format!("column \"{c}\" of relation \"{rel}\" does not exist")),
                    Some(k) if r.generated.get(k).is_some_and(Option::is_some) => {
                        return err("42P10", format!("cannot use generated column \"{c}\" in publication column list"));
                    }
                    Some(_) if seen.contains(&c.as_str()) => {
                        return err("42710", format!("duplicate column \"{c}\" in publication column list"));
                    }
                    Some(_) => seen.push(c),
                }
            }
        }
        p.tables.push(Member { table: r.name.clone(), cols: w.cols, filter: w.filter, shown });
    }
    Ok(())
}

/// Adds schemas to publication `p` (`quiet`: one already there is skipped, else refused).
fn add_schemas(p: &mut Publication, schemas: Vec<String>, quiet: bool) -> Result<()> {
    for s in schemas {
        if matches!(s.as_str(), "pg_catalog" | "information_schema" | "pg_toast") {
            return Err(with_detail(
                "22023",
                format!("cannot add schema \"{s}\" to publication"),
                "This operation is not supported for system schemas.",
            ));
        }
        if p.schemas.contains(&s) {
            if quiet {
                continue;
            }
            return err("42710", format!("schema \"{s}\" is already member of publication \"{}\"", p.name));
        }
        p.schemas.push(s);
    }
    Ok(())
}

/// 42501 unless the role running the statement may act as publication `p`'s owner.
fn owned(p: &Publication) -> Result<()> {
    match roles::checked() {
        Some(w) if !w.has_privs_of(p.owner) => err("42501", format!("must be owner of publication {}", p.name)),
        _ => Ok(()),
    }
}

/// Runs a publication statement on `branch`.
pub(super) fn run(db: &Db, branch: &str, st: &PubStmt) -> Result<Output> {
    let (me, superuser) = me();
    let tag = match st {
        PubStmt::Create { .. } => "CREATE PUBLICATION",
        PubStmt::Alter { .. } => "ALTER PUBLICATION",
        PubStmt::Drop { .. } => "DROP PUBLICATION",
    };
    db.batch_with::<(), SqlError>(branch, |v| {
        let cat = Catalog::of(v.core, &v.branch)?;
        let writes = match st {
            PubStmt::Create { name, all, objs, opts } => {
                if let Some(w) = roles::checked() {
                    acl::need_create_on_database(v, &w)?;
                }
                if *all && !superuser {
                    return err("42501", "must be superuser to create FOR ALL TABLES publication");
                }
                if find(v, name)?.is_some() {
                    return err("42710", format!("publication \"{name}\" already exists"));
                }
                let opts = opts.clone()?;
                let mut p = Publication {
                    name: name.clone(),
                    owner: me,
                    all: *all,
                    publish: opts.publish.unwrap_or([true; 4]),
                    via_root: opts.via_root.unwrap_or(false),
                    tables: vec![],
                    schemas: vec![],
                };
                let schemas = schemas_of(&cat, objs)?;
                if !schemas.is_empty() && !superuser {
                    return err("42501", "must be superuser to create FOR TABLES IN SCHEMA publication");
                }
                let tables = tables_of(&cat, objs)?;
                let shown = check_tables(v, &cat, name, p.via_root, !schemas.is_empty(), &tables)?;
                add_tables(v, &cat, &mut p, tables, shown, true)?;
                add_schemas(&mut p, schemas, true)?;
                notice_raised(
                    "WARNING",
                    "55000",
                    "wal_level is insufficient to publish logical changes".into(),
                    None,
                    Some("Set wal_level to \"logical\" before creating subscriptions.".into()),
                );
                vec![p.write()]
            }
            PubStmt::Alter { name, how } => {
                let Some(mut p) = find(v, name)? else { return Err(missing(name)) };
                match how {
                    How::Rename(to) => {
                        owned(&p)?;
                        if find(v, to)?.is_some() {
                            return err("42710", format!("publication \"{to}\" already exists"));
                        }
                        let gone = (key(&p.name), None);
                        p.name.clone_from(to);
                        vec![gone, p.write()]
                    }
                    How::Owner(spec) => {
                        let reg = roles::registry(v.core)?;
                        let to = spec.resolve(&reg)?;
                        if to.oid == p.owner {
                            return Ok((vec![], ()));
                        }
                        if !superuser {
                            owned(&p)?;
                            if !reg.can_set(me, to.oid) {
                                return err("42501", format!("must be able to SET ROLE \"{}\"", to.name));
                            }
                            // (the new owner has CREATE on the database, as AlterPublicationOwner_internal checks)
                            acl::need_create_on_database(v, &roles::Who::for_role(&reg, to.oid))?;
                            let refused = |what: &str| SqlError {
                                code: "42501",
                                message: format!("permission denied to change owner of publication \"{}\"", p.name),
                                info: Some(Box::new(Violation {
                                    hint: Some(format!("The owner of a {what} publication must be a superuser.")),
                                    ..Violation::default()
                                })),
                            };
                            if p.all && !to.superuser {
                                return Err(refused("FOR ALL TABLES"));
                            }
                            if !p.schemas.is_empty() && !to.superuser {
                                return Err(refused("FOR TABLES IN SCHEMA"));
                            }
                        }
                        p.owner = to.oid;
                        vec![p.write()]
                    }
                    How::Options(opts) => {
                        owned(&p)?;
                        let opts = opts.clone()?;
                        if !p.all && opts.via_root == Some(false) {
                            for m in &p.tables {
                                let Some(r) = cat.rels.iter().find(|r| r.name == m.table && r.kind == 'p') else {
                                    continue;
                                };
                                let what = if m.filter.is_some() {
                                    "a WHERE clause"
                                } else if m.cols.is_some() {
                                    "a column list"
                                } else {
                                    continue;
                                };
                                return Err(with_detail(
                                    "22023",
                                    format!(
                                        "cannot set parameter \"publish_via_partition_root\" to false for publication \"{}\"",
                                        p.name
                                    ),
                                    &format!(
                                        "The publication contains {what} for partitioned table \"{}\", which is not allowed when \"publish_via_partition_root\" is false.",
                                        rel_part(&r.name)
                                    ),
                                ));
                            }
                        }
                        if let Some(x) = opts.publish {
                            p.publish = x;
                        }
                        if let Some(x) = opts.via_root {
                            p.via_root = x;
                        }
                        vec![p.write()]
                    }
                    How::Add(objs) | How::Set(objs) | How::Drop(objs) => {
                        owned(&p)?;
                        let schemas = schemas_of(&cat, objs)?;
                        let named_tables = objs.iter().any(|o| matches!(o, PubObj::Table { .. }));
                        if matches!(how, How::Add(_) | How::Set(_)) && !schemas.is_empty() && !superuser {
                            return err("42501", "must be superuser to add or set schemas");
                        }
                        let all_tables = |detail: &str| {
                            Err(with_detail(
                                "55000",
                                format!("publication \"{}\" is defined as FOR ALL TABLES", p.name),
                                detail,
                            ))
                        };
                        if !schemas.is_empty() && p.all {
                            return all_tables("Schemas cannot be added to or dropped from FOR ALL TABLES publications.");
                        }
                        if named_tables && p.all {
                            return all_tables("Tables cannot be added to or dropped from FOR ALL TABLES publications.");
                        }
                        let tables = tables_of(&cat, objs)?;
                        match how {
                            How::Add(_) => {
                                let with_schemas = !schemas.is_empty() || !p.schemas.is_empty();
                                if !tables.is_empty() {
                                    let shown = check_tables(v, &cat, &p.name, p.via_root, with_schemas, &tables)?;
                                    add_tables(v, &cat, &mut p, tables, shown, false)?;
                                }
                                if !schemas.is_empty() && p.tables.iter().any(|m| m.cols.is_some()) {
                                    return Err(with_detail(
                                        "22023",
                                        format!("cannot add schema to publication \"{}\"", p.name),
                                        "Schemas cannot be added if any tables that specify a column list are already part of the publication.",
                                    ));
                                }
                                add_schemas(&mut p, schemas, false)?;
                            }
                            How::Set(_) => {
                                let shown = check_tables(v, &cat, &p.name, p.via_root, !schemas.is_empty(), &tables)?;
                                p.tables.clear();
                                add_tables(v, &cat, &mut p, tables, shown, true)?;
                                p.schemas.clear();
                                add_schemas(&mut p, schemas, true)?;
                            }
                            _ => {
                                for w in &tables {
                                    let r = &cat.rels[w.rel];
                                    if w.cols.is_some() {
                                        return err(
                                            SYNTAX,
                                            "column list must not be specified in ALTER PUBLICATION ... DROP",
                                        );
                                    }
                                    let Some(at) = p.tables.iter().position(|m| m.table == r.name) else {
                                        return err(
                                            "42704",
                                            format!("relation \"{}\" is not part of the publication", rel_part(&r.name)),
                                        );
                                    };
                                    if w.filter.is_some() {
                                        return err(
                                            SYNTAX,
                                            "cannot use a WHERE clause when removing a table from a publication",
                                        );
                                    }
                                    p.tables.remove(at);
                                }
                                for s in &schemas {
                                    let Some(at) = p.schemas.iter().position(|x| x == s) else {
                                        return err(
                                            "42704",
                                            format!("tables from schema \"{s}\" are not part of the publication"),
                                        );
                                    };
                                    p.schemas.remove(at);
                                }
                            }
                        }
                        vec![p.write()]
                    }
                }
            }
            PubStmt::Drop { names, if_exists } => {
                let mut writes = vec![];
                for name in names {
                    match find(v, name)? {
                        Some(p) => {
                            owned(&p)?;
                            writes.push((key(name), None));
                        }
                        None if *if_exists => {
                            notice("NOTICE", format!("publication \"{name}\" does not exist, skipping"));
                        }
                        None => return Err(missing(name)),
                    }
                }
                writes
            }
        };
        Ok((writes, ()))
    })?;
    Ok(Output::tag(tag))
}

/// The writes that take dropped tables (stored names) out of the publications that name them.
/// The writes that make publications follow table `from` to its new name `to` (RENAME TO, SET SCHEMA), as
/// Postgres's, which name their tables by oid, do.
pub(super) fn renamed_table(v: &View, from: &str, to: &str) -> Result<Vec<(String, Option<String>)>> {
    let mut writes = vec![];
    for mut p in load(v)?.into_iter().filter(|p| p.tables.iter().any(|m| m.table == from)) {
        for m in p.tables.iter_mut().filter(|m| m.table == from) {
            to.clone_into(&mut m.table);
        }
        writes.push(p.write());
    }
    Ok(writes)
}

/// The writes that make publications of table `table` follow its column `a`, renamed `b`: in their column lists
/// and their WHERE (as written, and as written back over the table's columns `cols`).
pub(super) fn renamed_column(
    v: &View,
    table: &str,
    a: &str,
    b: &str,
    cols: &[Col],
) -> Result<Vec<(String, Option<String>)>> {
    let mut writes = vec![];
    for mut p in load(v)? {
        let mut changed = false;
        for m in p.tables.iter_mut().filter(|m| m.table == table) {
            for c in m.cols.iter_mut().flatten().filter(|c| *c == a) {
                b.clone_into(c);
                changed = true;
            }
            if let Some(f) = &m.filter {
                let new = super::statistics::renamed_in(f, a, b)?;
                if new != *f {
                    m.shown = Some(catalog::expr_def(&new, cols, true));
                    m.filter = Some(new);
                    changed = true;
                }
            }
        }
        if changed {
            writes.push(p.write());
        }
    }
    Ok(writes)
}

/// The publications role `oid` owns, as DROP ROLE names them.
pub(super) fn owned_by(v: &View, oid: i64) -> Result<Vec<String>> {
    Ok(load(v)?.into_iter().filter(|p| p.owner == oid).map(|p| p.name).collect())
}

/// The writes that give roles `olds`' publications to role `new` (REASSIGN OWNED).
pub(super) fn reassigned(v: &View, olds: &[i64], new: i64) -> Result<Vec<(String, Option<String>)>> {
    Ok(load(v)?
        .into_iter()
        .filter(|p| olds.contains(&p.owner))
        .map(|mut p| {
            p.owner = new;
            p.write()
        })
        .collect())
}

pub(super) fn forget(v: &View, tables: &[String]) -> Result<Vec<(String, Option<String>)>> {
    let mut writes = vec![];
    for mut p in load(v)? {
        let before = p.tables.len();
        p.tables.retain(|m| !tables.contains(&m.table));
        if p.tables.len() != before {
            writes.push(p.write());
        }
    }
    Ok(writes)
}

// ---------------------------------------------------------------- the catalog

/// Where publications' oids begin, among the objects `foreign::name_oids` numbers.
const PUB_KIND: i64 = 8;

/// The oids of the world's publications (`cat.pubs`), in order.
fn pub_oids(cat: &Catalog) -> Vec<i64> {
    foreign::name_oids(PUB_KIND, &cat.pubs.iter().map(|p| p.name.clone()).collect::<Vec<_>>())
}

/// The world's publications with their oids, in order (what `pg_publication` has).
pub(super) fn publication_oids(cat: &Catalog) -> Vec<(i64, usize)> {
    pub_oids(cat).into_iter().enumerate().map(|(k, o)| (o, k)).collect()
}

/// `pg_publication_rel`'s rows: each one's oid, publication (its place in `cat.pubs`) and relation.
pub(super) fn rel_oids(cat: &Catalog) -> Vec<(i64, usize, usize)> {
    let all = members(cat);
    let keys: Vec<String> = all.iter().map(|(k, m, _)| format!("{}/{}", cat.pubs[*k].name, m.table)).collect();
    all.iter().zip(foreign::name_oids(PUB_KIND + 1, &keys)).map(|((k, _, i), o)| (o, *k, *i)).collect()
}

/// `pg_publication_namespace`'s rows: each one's oid, publication and schema.
pub(super) fn namespace_oids(cat: &Catalog) -> Vec<(i64, usize, String)> {
    let all = namespaces(cat);
    let keys: Vec<String> = all.iter().map(|(k, s)| format!("{}/{s}", cat.pubs[*k].name)).collect();
    all.iter().zip(foreign::name_oids(PUB_KIND + 2, &keys)).map(|((k, s), o)| (o, *k, s.to_string())).collect()
}

/// The relation a member names, if it is there.
fn member_rel(cat: &Catalog, m: &Member) -> Option<usize> {
    cat.rels.iter().position(|r| r.name == m.table && matches!(r.kind, 'r' | 'p'))
}

/// A member's column list as attnums, in column order.
fn member_attnums(cat: &Catalog, i: usize, m: &Member) -> Option<Vec<i64>> {
    let cols = m.cols.as_ref()?;
    let r = &cat.rels[i];
    let mut nums: Vec<i64> =
        cols.iter().filter_map(|c| r.columns.iter().position(|x| x.0 == *c).map(|k| r.attnum(k))).collect();
    nums.sort_unstable();
    Some(nums)
}

/// The column names of attnums of relation `i`.
fn attnames(cat: &Catalog, i: usize, nums: &[i64]) -> Vec<String> {
    let r = &cat.rels[i];
    nums.iter().filter_map(|&n| r.at_attnum(n).and_then(|j| r.columns.get(j)).map(|c| c.0.clone())).collect()
}

/// Every (publication, member, relation) whose table is there, in publication order then as added.
fn members(cat: &Catalog) -> Vec<(usize, &Member, usize)> {
    let mut out = vec![];
    for (k, p) in cat.pubs.iter().enumerate() {
        for m in &p.tables {
            if let Some(i) = member_rel(cat, m) {
                out.push((k, m, i));
            }
        }
    }
    out
}

/// Every (publication, schema) whose schema is there.
fn namespaces(cat: &Catalog) -> Vec<(usize, &str)> {
    let mut out = vec![];
    for (k, p) in cat.pubs.iter().enumerate() {
        for s in &p.schemas {
            if s == "public" || cat.schemas.contains(s) {
                out.push((k, s.as_str()));
            }
        }
    }
    out
}

/// Whether relation `i` can be published: a table (not a view, sequence or foreign table).
fn publishable(cat: &Catalog, i: usize) -> bool {
    matches!(cat.rels[i].kind, 'r' | 'p')
}

/// The leaf partitions below relation `i` (itself, if it is none).
fn leaves(cat: &Catalog, i: usize) -> Vec<usize> {
    if cat.rels[i].kind != 'p' {
        return vec![i];
    }
    let name = &cat.rels[i].name;
    let mut out = vec![];
    for (k, r) in cat.rels.iter().enumerate() {
        if r.hier.of.as_ref().is_some_and(|(p, ..)| p == name) {
            out.extend(leaves(cat, k));
        }
    }
    out
}

/// Whether relation `i` is a partition of a table among `rels` (at any level).
fn has_ancestor_in(cat: &Catalog, i: usize, rels: &[usize]) -> bool {
    let mut at = i;
    let mut steps = 0;
    while let Some((p, ..)) = &cat.rels[at].hier.of {
        let Some(k) = cat.rels.iter().position(|r| r.name == *p) else { return false };
        if rels.contains(&k) {
            return true;
        }
        at = k;
        steps += 1;
        if steps > cat.rels.len() {
            return false;
        }
    }
    false
}

/// The tables publication `k` publishes, as `pg_get_publication_tables` gives them: each with the member that named
/// it (its column list and WHERE), if one did.
fn published(cat: &Catalog, k: usize) -> Vec<(usize, Option<&Member>)> {
    let p = &cat.pubs[k];
    let mut rels: Vec<(usize, Option<&Member>)> = vec![];
    fn add<'a>(i: usize, m: Option<&'a Member>, rels: &mut Vec<(usize, Option<&'a Member>)>) {
        if !rels.iter().any(|(x, _)| *x == i) {
            rels.push((i, m));
        }
    }
    if p.all {
        for i in 0..cat.rels.len() {
            let r = &cat.rels[i];
            let take = if p.via_root { publishable(cat, i) && r.hier.of.is_none() } else { r.kind == 'r' };
            if take {
                add(i, None, &mut rels);
            }
        }
        return rels;
    }
    for m in &p.tables {
        let Some(i) = member_rel(cat, m) else { continue };
        if p.via_root {
            add(i, Some(m), &mut rels);
        } else {
            for l in leaves(cat, i) {
                add(l, if l == i { Some(m) } else { None }, &mut rels);
            }
        }
    }
    for s in &p.schemas {
        for i in 0..cat.rels.len() {
            let r = &cat.rels[i];
            if schema_part(&r.name) != s || !publishable(cat, i) {
                continue;
            }
            if p.via_root || r.kind == 'r' {
                add(i, None, &mut rels);
            }
        }
    }
    if p.via_root {
        let all: Vec<usize> = rels.iter().map(|(i, _)| *i).collect();
        rels.retain(|(i, _)| !has_ancestor_in(cat, *i, &all));
    }
    rels
}

/// Catalog table `name`'s rows of the world's publications: pg_publication, pg_publication_rel,
/// pg_publication_namespace and pg_publication_tables.
pub(super) fn catalog_rows(name: &str, cat: &Catalog) -> Vec<Vec<Value>> {
    use Value::{Bool as b, Int as int, Null};
    let t = |s: &str| Value::Text(s.into());
    let row = |fields: Vec<(&str, Value)>| catalog_schema::named_row(name, fields);
    let oids = pub_oids(cat);
    match name {
        "pg_publication" => cat
            .pubs
            .iter()
            .zip(&oids)
            .map(|(p, o)| {
                row(vec![
                    ("oid", int(*o)),
                    ("pubname", t(&p.name)),
                    ("pubowner", int(p.owner)),
                    ("puballtables", b(p.all)),
                    ("pubinsert", b(p.publish[0])),
                    ("pubupdate", b(p.publish[1])),
                    ("pubdelete", b(p.publish[2])),
                    ("pubtruncate", b(p.publish[3])),
                    ("pubviaroot", b(p.via_root)),
                ])
            })
            .collect(),
        "pg_publication_rel" => {
            let all = members(cat);
            let keys: Vec<String> = all.iter().map(|(k, m, _)| format!("{}/{}", cat.pubs[*k].name, m.table)).collect();
            let rel_oids = foreign::name_oids(PUB_KIND + 1, &keys);
            all.iter()
                .zip(rel_oids)
                .map(|((k, m, i), o)| {
                    let attrs = member_attnums(cat, *i, m)
                        .map_or(Null, |n| t(&n.iter().map(i64::to_string).collect::<Vec<_>>().join(" ")));
                    row(vec![
                        ("oid", int(o)),
                        ("prpubid", int(oids[*k])),
                        ("prrelid", int(cat.rel_oid(*i))),
                        ("prqual", m.shown.as_deref().map_or(Null, t)),
                        ("prattrs", attrs),
                    ])
                })
                .collect()
        }
        "pg_publication_namespace" => {
            let all = namespaces(cat);
            let keys: Vec<String> = all.iter().map(|(k, s)| format!("{}/{s}", cat.pubs[*k].name)).collect();
            let ns_oids = foreign::name_oids(PUB_KIND + 2, &keys);
            all.iter()
                .zip(ns_oids)
                .map(|((k, s), o)| {
                    row(vec![("oid", int(o)), ("pnpubid", int(oids[*k])), ("pnnspid", int(cat.schema_oid(s)))])
                })
                .collect()
        }
        "pg_publication_tables" => {
            let mut out = vec![];
            for k in 0..cat.pubs.len() {
                for (i, m) in published(cat, k) {
                    let r = &cat.rels[i];
                    let names = match m.and_then(|m| member_attnums(cat, i, m)) {
                        Some(nums) => attnames(cat, i, &nums),
                        None => r
                            .columns
                            .iter()
                            .enumerate()
                            .filter(|(c, _)| r.generated.get(*c).is_none_or(Option::is_none))
                            .map(|(_, c)| c.0.clone())
                            .collect(),
                    };
                    let names: Vec<String> = names.iter().map(|n| roles::array_elem(n)).collect();
                    out.push(row(vec![
                        ("pubname", t(&cat.pubs[k].name)),
                        ("schemaname", t(schema_part(&r.name))),
                        ("tablename", t(rel_part(&r.name))),
                        ("attnames", t(&format!("{{{}}}", names.join(",")))),
                        ("rowfilter", m.and_then(|m| m.shown.as_deref()).map_or(Null, t)),
                    ]));
                }
            }
            out
        }
        _ => vec![],
    }
}

/// psql's `\dRp+` tables of the publication with this oid: schema, table, WHERE (as `pg_get_expr` writes it) and
/// column list (`a, b`), by schema and table.
pub(crate) fn shown_tables(cat: &Catalog, oid: i64) -> Vec<(String, String, Option<String>, Option<String>)> {
    let oids = pub_oids(cat);
    let Some(k) = oids.iter().position(|o| *o == oid) else { return vec![] };
    let mut out: Vec<_> = cat.pubs[k]
        .tables
        .iter()
        .filter_map(|m| {
            let i = member_rel(cat, m)?;
            let r = &cat.rels[i];
            let cols = member_attnums(cat, i, m).map(|n| attnames(cat, i, &n).join(", "));
            Some((schema_part(&r.name).to_string(), rel_part(&r.name).to_string(), m.shown.clone(), cols))
        })
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

/// psql's `\dRp+` schemas of the publication with this oid, by name.
pub(crate) fn shown_schemas(cat: &Catalog, oid: i64) -> Vec<String> {
    let oids = pub_oids(cat);
    let Some(k) = oids.iter().position(|o| *o == oid) else { return vec![] };
    let mut out: Vec<String> =
        namespaces(cat).into_iter().filter(|(x, _)| *x == k).map(|(_, s)| s.to_string()).collect();
    out.sort();
    out
}

/// The publications `\d` lists for table `name` (stored): each publication's name, the table's WHERE in it and its
/// column list (`a, b`), by name: those naming it, publishing its schema, or all tables.
pub(crate) fn shown_for_table(cat: &Catalog, name: &str) -> Vec<(String, Option<String>, Option<String>)> {
    let Some(i) = cat.rels.iter().position(|r| r.name == name) else { return vec![] };
    if !publishable(cat, i) {
        return vec![];
    }
    let mut out: Vec<(String, Option<String>, Option<String>)> = vec![];
    for p in &cat.pubs {
        let mut add = |x: (String, Option<String>, Option<String>)| {
            if !out.contains(&x) {
                out.push(x);
            }
        };
        if p.all || p.schemas.iter().any(|s| s == schema_part(name)) {
            add((p.name.clone(), None, None));
        }
        if let Some(m) = p.tables.iter().find(|m| m.table == name) {
            let cols = member_attnums(cat, i, m).map(|n| attnames(cat, i, &n).join(", "));
            add((p.name.clone(), m.shown.clone(), cols));
        }
    }
    out.sort();
    out
}
