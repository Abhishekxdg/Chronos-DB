//! CREATE ACCESS METHOD and DROP ACCESS METHOD: access methods made over a built-in one's handler (`gist2` over
//! gisthandler, `heap2` over heap_tableam_handler), as Postgres's create_am.sql makes them (amcmds.c).
//!
//! Each is kept as a row of the world, `_sonos_defs/am/<name>` (`{"type": "i" | "t", "handler", "oid"}`), so it forks
//! and merges with the data; its oid is the next of a counter every world shares, so two made in different worlds
//! never share one when the worlds merge. pg_am lists it after Postgres's own (see `catalog::Catalog::ams`).
//!
//! What one does is its handler's: an index of `gist2` is a gist index, kept as one (`btree::Index::builtin`), under
//! its own name (pg_class.relam, pg_get_indexdef), with the operator classes CREATE OPERATOR CLASS ... USING gist2
//! made for it; a table of `heap2` is stored as every table is, there being one store, and its access method is
//! recorded (`hier::Hier::am`: pg_class.relam, pg_depend, psql's "Access method:").

use super::*;

/// Where the rows of the access methods made here are.
const KEY: &str = "_sonos_defs/am/";

/// Where the oids of the access methods made here begin: past the kinds `catalog::Named` numbers (the last ends at
/// 13 * 2^26), below the functions' (from 2^30), so none is any other object's.
const AM_OIDS: i64 = 14 << 26;

/// The handlers CREATE ACCESS METHOD takes, as Postgres's pg_proc has them: the built-in methods', each with its
/// method and its kind (`i` index, `t` table).
const HANDLERS: [(&str, &str, char); 7] = [
    ("bthandler", "btree", 'i'),
    ("hashhandler", "hash", 'i'),
    ("gisthandler", "gist", 'i'),
    ("ginhandler", "gin", 'i'),
    ("spghandler", "spgist", 'i'),
    ("brinhandler", "brin", 'i'),
    ("heap_tableam_handler", "heap", 't'),
];

/// An access method CREATE ACCESS METHOD made.
#[derive(Clone, Debug)]
pub(crate) struct MadeAm {
    pub name: String,
    /// pg_am.amtype: `i` an index method, `t` a table method
    pub kind: char,
    /// its handler function's name (pg_am.amhandler)
    pub handler: String,
    pub oid: i64,
}

impl MadeAm {
    /// The built-in access method its handler is, which it does as.
    pub(crate) fn builtin(&self) -> &'static str {
        HANDLERS.iter().find(|(h, ..)| *h == self.handler).map_or("heap", |(_, m, _)| *m)
    }

    fn from_row(name: &str, raw: &str) -> MadeAm {
        let j: Json = serde_json::from_str(raw).unwrap_or_default();
        MadeAm {
            name: name.to_string(),
            kind: j["type"].as_str().and_then(|t| t.chars().next()).unwrap_or('i'),
            handler: j["handler"].as_str().unwrap_or_default().to_string(),
            oid: j["oid"].as_i64().unwrap_or(0),
        }
    }

    fn row(&self) -> (String, Option<String>) {
        let j = serde_json::json!({ "type": self.kind.to_string(), "handler": self.handler, "oid": self.oid });
        (format!("{KEY}{}", self.name), Some(j.to_string()))
    }
}

/// The access methods made here, by name.
pub(super) fn all(v: &View) -> Result<Vec<MadeAm>> {
    let mut out = vec![];
    for r in v.prefix(KEY)? {
        let (k, raw) = r?;
        out.push(MadeAm::from_row(&k[KEY.len()..], &raw));
    }
    Ok(out)
}

/// The access method made here called `name`, if there is one.
pub(super) fn named(v: &View, name: &str) -> Result<Option<MadeAm>> {
    Ok(v.get(&format!("{KEY}{name}"))?.map(|raw| MadeAm::from_row(name, &raw)))
}

/// The access method made here called `name`, as the statement being parsed finds it in its session's world (None
/// outside a session).
pub(super) fn at_parse(name: &str) -> Option<MadeAm> {
    with_path_view(|v| named(v, name).ok().flatten()).flatten()
}

/// Postgres's own access method called `name`: (oid, name, handler, kind).
fn builtin(name: &str) -> Option<&'static (i64, &'static str, &'static str, &'static str)> {
    catalog::AMS.iter().find(|a| a.1 == name)
}

/// What `name` is as an access method: its kind (`i`, `t`), Postgres's own or made here; None for none.
fn kind_of(v: &View, name: &str) -> Result<Option<char>> {
    if let Some(a) = builtin(name) {
        return Ok(a.3.chars().next());
    }
    Ok(named(v, name)?.map(|a| a.kind))
}

/// 55000 for an access method of another kind than the one wanted (get_am_type_oid, amcmds.c:139).
fn not_of_type(name: &str, kind: char) -> SqlError {
    let k = if kind == 'i' { "INDEX" } else { "TABLE" };
    SqlError { code: "55000", message: format!("access method \"{name}\" is not of type {k}"), info: None }
}

fn missing(name: &str) -> SqlError {
    SqlError { code: "42704", message: format!("access method \"{name}\" does not exist"), info: None }
}

/// Table access method `name`, as get_table_am_oid looks it up (amcmds.c:166): heap or one made here of kind
/// TABLE; 42704 for none, 55000 for an index method. Looked for in `v`, then in the session's world (a temporary
/// table is made in the session's own database, which has none).
pub(super) fn table_am(v: &View, name: &str) -> Result<()> {
    let kind = match kind_of(v, name)? {
        Some(k) => Some(k),
        None => with_path_view(|w| kind_of(w, name)).transpose()?.flatten(),
    };
    match kind {
        Some('t') => Ok(()),
        Some(_) => Err(not_of_type(name, 't')),
        None => Err(missing(name)),
    }
}

/// The table access method a new relation of this kind gets, as DefineRelation chooses it (tablecmds.c:962): the
/// one USING names (none for a partitioned table, which has no storage: 0A000), else default_table_access_method's;
/// none for a partitioned table. Checked as get_table_am_oid checks it; None is heap (or none, for a partitioned
/// table), which is what a table kept before this was has.
pub(super) fn for_new(v: &View, given: Option<&str>, partitioned: bool) -> Result<Option<String>> {
    let name = match given {
        Some(_) if partitioned => {
            return err(UNSUPPORTED, "specifying a table access method is not supported on a partitioned table");
        }
        Some(n) => n.to_string(),
        None if partitioned => return Ok(None),
        None => session_setting("default_table_access_method").unwrap_or_else(|| "heap".into()),
    };
    table_am(v, &name)?;
    Ok(Some(name).filter(|n| n != "heap"))
}

/// The index access method a CREATE INDEX ... USING `name` being parsed means, where it isn't a word Postgres's own
/// are known by: the built-in one an access method made here is over (its handler's). XX000 for a table method, whose
/// handler gives no index routine (GetIndexAmRoutine, amapi.c:42); None for none of that name.
pub(super) fn index_at_parse(name: &str) -> Result<Option<&'static str>> {
    let made = at_parse(name);
    if builtin(name).is_some_and(|a| a.3 == "t") || made.as_ref().is_some_and(|a| a.kind == 't') {
        let handler = made.map_or("heap_tableam_handler".to_string(), |a| a.handler);
        let oid = catalog_schema::builtin_procs(&handler).first().copied().unwrap_or(0);
        return Err(SqlError {
            code: "XX000",
            message: format!("index access method handler function {oid} did not return an IndexAmRoutine struct"),
            info: None,
        });
    }
    Ok(made.map(|a| a.builtin()))
}

/// Index access method `name` as an operator class or family being parsed names it (USING name: get_index_am_oid,
/// amcmds.c:156): Postgres's own index methods, or one made here of kind INDEX; 42704 for none, 55000 for a table
/// method (heap among them).
pub(super) fn opclass_am_at_parse(name: &str) -> Result<()> {
    let kind = match builtin(name) {
        Some(a) => a.3.chars().next(),
        None => at_parse(name).map(|a| a.kind),
    };
    match kind {
        Some('i') => Ok(()),
        Some(_) => Err(not_of_type(name, 'i')),
        None => Err(missing(name)),
    }
}

/// default_table_access_method set to `value`: a name, so cut to 63 bytes first, with a notice, as
/// parse_and_validate_value cuts a GUC_IS_NAME setting's (guc.c:3198, truncate_identifier: at a character's end),
/// then checked as check_default_table_access_method checks it (tableamapi.c:112), in the session's world (which
/// never sees one too long). The value as it is taken.
pub(super) fn check_default(value: &str) -> Result<String> {
    let mut cut = value.len().min(63);
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut < value.len() {
        let message = format!("identifier \"{value}\" will be truncated to \"{}\"", &value[..cut]);
        notice_raised("NOTICE", "42622", message, None, None);
    }
    let value = &value[..cut];
    let invalid = |detail: String| SqlError {
        code: "22023",
        message: format!("invalid value for parameter \"default_table_access_method\": \"{value}\""),
        info: Some(Box::new(Violation { detail: Some(detail), ..Violation::default() })),
    };
    if value.is_empty() {
        return Err(invalid("default_table_access_method cannot be empty.".into()));
    }
    // (outside a session there is no catalog to look in: taken on faith, as Postgres takes it outside a transaction)
    let kind = match builtin(value) {
        Some(a) => a.3.chars().next(),
        None => match with_path_view(|v| named(v, value)) {
            Some(found) => found?.map(|a| a.kind),
            None => return Ok(value.to_string()),
        },
    };
    match kind {
        Some('t') => Ok(value.to_string()),
        // (an index method is refused by get_table_am_oid itself, whatever the check hook was asked)
        Some(_) => Err(not_of_type(value, 't')),
        None => Err(invalid(format!("Table access method \"{value}\" does not exist."))),
    }
}

/// The operator class CREATE OPERATOR CLASS ... DEFAULT FOR TYPE made for access method `am` (one made here) and a
/// column of type `ty` (type id `oid`), the one an index column that names none gets (GetDefaultOpClass,
/// indexcmds.c:2286), if there is one.
pub(super) fn made_default_class(v: &View, am: &str, ty: Type, oid: i64) -> Result<Option<String>> {
    let at = "_sonos_defs/opclass/";
    let types = indexam::indexed_as(ty, oid);
    for r in v.prefix(at)? {
        let (k, raw) = r?;
        let Some((name, m)) = k[at.len()..].rsplit_once('/') else { continue };
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        if m == am && j["default"] == true && j["type"].as_i64().is_some_and(|t| types.contains(&t)) {
            return Ok(Some(name.to_string()));
        }
    }
    Ok(None)
}

/// CREATE ACCESS METHOD and DROP ACCESS METHOD.
#[derive(Clone, Debug)]
pub(crate) enum AmStmt {
    /// CREATE ACCESS METHOD name TYPE INDEX | TABLE (`kind` `i` or `t`) HANDLER handler (its name as written, maybe
    /// with its schema)
    Create { name: String, kind: char, handler: Vec<String> },
    /// DROP ACCESS METHOD [IF EXISTS] name [, ...] [CASCADE | RESTRICT]
    Drop { names: Vec<String>, if_exists: bool, cascade: bool },
}

impl AmStmt {
    pub(super) fn tag(&self) -> &'static str {
        match self {
            AmStmt::Create { .. } => "CREATE ACCESS METHOD",
            AmStmt::Drop { .. } => "DROP ACCESS METHOD",
        }
    }
}

impl Parser {
    /// A table access method's name, after USING. A quoted name of no characters is the lexer's error in Postgres
    /// (scan.l), where it is written.
    pub(super) fn am_name(&mut self) -> Result<String> {
        if matches!(self.peek(), Some(Tok::Ident(s)) if s.is_empty()) {
            let e = self.syntax_at(self.i, "\"\"\"\"".into(), "");
            return Err(SqlError { message: "zero-length delimited identifier at or near \"\"\"\"".into(), ..e });
        }
        self.name()
    }

    /// After CREATE or DROP: `ACCESS METHOD ...` (see `AmStmt`). There is no ALTER ACCESS METHOD.
    pub(super) fn access_method_stmt(&mut self, verb: &str) -> Result<Stmt> {
        if !matches!(verb, "create" | "drop") {
            return self.fail("");
        }
        self.expect_kw("access")?;
        self.expect_kw("method")?;
        let st = match verb {
            "create" => {
                let name = self.name()?;
                self.expect_kw("type")?;
                let kind = if self.kw("index") {
                    'i'
                } else if self.kw("table") {
                    't'
                } else {
                    return self.fail("");
                };
                self.expect_kw("handler")?;
                let mut handler = vec![self.name()?];
                while self.sym(".") {
                    handler.push(self.name()?);
                }
                AmStmt::Create { name, kind, handler }
            }
            "drop" => {
                let if_exists = self.if_exists()?;
                let mut names = vec![self.name()?];
                while self.sym(",") {
                    names.push(self.name()?);
                }
                AmStmt::Drop { names, if_exists, cascade: self.cascade() }
            }
            _ => return self.fail(""),
        };
        Ok(Stmt::Foreign(Box::new(foreign::ForeignStmt::AccessMethod(st))))
    }
}

pub(super) fn run(db: &Db, branch: &str, s: &AmStmt) -> Result<Output> {
    match s {
        AmStmt::Create { name, kind, handler } => create(db, branch, name, *kind, handler),
        AmStmt::Drop { names, if_exists, cascade } => drop(db, branch, names, *if_exists, *cascade),
    }
}

/// CREATE ACCESS METHOD, as CreateAccessMethod makes one (amcmds.c:42): a superuser's, of a name no access method
/// has, over a handler of its kind (lookup_am_handler_func, amcmds.c:236).
fn create(db: &Db, branch: &str, name: &str, kind: char, handler: &[String]) -> Result<Output> {
    if !roles::current().is_none_or(|w| w.superuser) {
        return Err(SqlError {
            code: "42501",
            message: format!("permission denied to create access method \"{name}\""),
            info: Some(Box::new(Violation {
                hint: Some("Must be superuser to create an access method.".into()),
                ..Violation::default()
            })),
        });
    }
    let taken = |v: &View| -> Result<()> {
        match kind_of(v, name)? {
            Some(_) => err("42710", format!("access method \"{name}\" already exists")),
            None => Ok(()),
        }
    };
    taken(&db.view_tx(branch)?)?;
    // the handler: a function of one argument of type internal (LookupFuncName), of the kind's result type; its name
    // read as DeconstructQualifiedName reads one (namespace.c:2863), a database's only the one being used
    let (schema, bare) = objaddr::qualified(handler)?;
    let written = handler.join(".");
    let takes_internal = match schema.is_none_or(|s| s == "pg_catalog") {
        true => catalog_schema::builtin_signatures(bare).into_iter().find(|(args, ..)| args == &["internal"]),
        false => None,
    };
    let Some((_, result, ..)) = takes_internal else {
        return err(NO_FUNCTION, format!("function {written}(internal) does not exist"));
    };
    let want = if kind == 'i' { "index_am_handler" } else { "table_am_handler" };
    if result != want {
        return err("42809", format!("function {bare} must return type {want}"));
    }
    // (pgvector's hnswhandler is an index handler too, whose method is a search index here, not a btree)
    if !HANDLERS.iter().any(|(h, ..)| *h == bare) {
        return err(UNSUPPORTED, format!("an access method over handler {bare} isn't supported here"));
    }
    // (its oid drawn before the write, as a tablespace's is: the counter takes the gate the write holds, which a
    // checkpoint waiting between the two would deadlock; one lost to a method made meanwhile under the same name is
    // only a gap)
    let n = db.core.next_values("_sonos_am_oids", 1)?[0];
    let am = MadeAm { name: name.to_string(), kind, handler: bare.to_string(), oid: AM_OIDS + n };
    db.batch_with::<(), SqlError>(branch, |v| {
        taken(v)?;
        Ok((vec![am.row()], ()))
    })?;
    Ok(Output::tag("CREATE ACCESS METHOD"))
}

/// What depends on an access method that goes, as Postgres's pg_depend has it: the tables and materialized views
/// of a table method (and what reads them), the indexes of an index method's operator classes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Node {
    Am(String),
    Rel(String),
}

/// An index of a method DROP ACCESS METHOD drops: its table's place, its own among the table's, its name, what it
/// depends on, and the foreign keys (as (table, name)) that reference it.
type Found = (MadeKey, usize, String, String, Vec<(String, String)>);

/// DROP ACCESS METHOD, as RemoveObjects drops one (dependency.c): not one of Postgres's own; what depends on it
/// stops the drop (2BP01, each with what it depends on), or with CASCADE goes with it. Its operator classes and
/// families go with it whatever (they depend on it automatically), and its comment.
fn drop(db: &Db, branch: &str, names: &[String], if_exists: bool, cascade: bool) -> Result<Output> {
    let v = db.view_tx(branch)?;
    let mut gone: Vec<MadeAm> = vec![];
    let mut pinned = None;
    // each looked up, then its owner checked, as RemoveObjects does in turn (dropcmds.c:73); Postgres's own refused
    // only once all are, when their deletion is (findDependentObjects: pinned)
    for n in names {
        let made = named(&v, n)?;
        if made.is_none() && builtin(n).is_none() {
            if !if_exists {
                return Err(missing(n));
            }
            notice("NOTICE", format!("access method \"{n}\" does not exist, skipping"));
            continue;
        }
        // (an access method is taken as a superuser's: check_object_ownership, objectaddress.c:2574)
        if !roles::current().is_none_or(|w| w.superuser) {
            return err("42501", "must be superuser");
        }
        match made {
            Some(a) if gone.iter().any(|g| g.name == a.name) => {}
            Some(a) => gone.push(a),
            None => {
                pinned.get_or_insert(n);
            }
        }
    }
    if let Some(n) = pinned {
        return err("2BP01", format!("cannot drop access method {n} because it is required by the database system"));
    }
    if gone.is_empty() {
        return Ok(Output::tag("DROP ACCESS METHOD"));
    }
    let tables = schemas(&v)?;
    // the session's temporary relations, kept in a database of their own (see `TEMP`): another session's are out of
    // reach here, as Postgres's are to a DROP
    let temps = session_temp_view(schemas)?.unwrap_or_default();
    let (mut temp_indexes, mut temp_rels): (Vec<String>, Vec<String>) = (vec![], vec![]);
    // what depends on them, each with what it depends on, in the order Postgres reports them
    let mut listed: Vec<(String, String)> = vec![];
    let (mut indexes, mut rels, mut matviews): (Vec<String>, Vec<String>, Vec<String>) = (vec![], vec![], vec![]);
    // the foreign keys, as (their table, their name), that reference a unique index of a method: they go first
    let mut fks: Vec<(String, String)> = vec![];
    for a in &gone {
        if a.kind == 'i' {
            // what an index depends on: its first column's class, the one it names, else the default made for its
            // type
            let on = |t: &Table, x: &btree::Index| -> Result<String> {
                let (ty, oid) = indexam::part_types(t, x).first().copied().unwrap_or((Type::Text, 25));
                let class = match x.said_class(0) {
                    Some(c) => Some(c.to_string()),
                    None => made_default_class(&v, &a.name, ty, oid)?,
                };
                Ok(match class {
                    Some(c) => format!("operator class {c} for access method {}", a.name),
                    None => format!("access method {}", a.name),
                })
            };
            // (a partition's copy of its table's goes with that one)
            let of = |t: &Table| -> Vec<(usize, btree::Index)> {
                t.indexes
                    .iter()
                    .enumerate()
                    .filter(|(_, x)| x.method() == a.name && !t.hier.inherited.contains(&x.name))
                    .map(|(at, x)| (at, x.clone()))
                    .collect()
            };
            // in the order they were made, as pg_depend's rows on the class are read: an index has no stamp of its
            // own, so its table's, then its place among the table's indexes (which keep the order they were made in)
            let mut found: Vec<Found> = vec![];
            for t in &tables {
                for (at, x) in of(t) {
                    // (a unique index a foreign key references: the key depends on it, as on its constraint)
                    let refs = match t.uniques.iter().find(|u| u.name == x.name) {
                        Some(u) => referencing_fks(&v, t, Some(u), 0)?,
                        None => vec![],
                    };
                    let name = qualify(schema_part(&t.name), &x.name);
                    found.push((rel_order(&v, &t.name, false), at, name, on(t, &x)?, refs));
                }
            }
            found.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
            for (_, _, x, on, refs) in found {
                listed.push((format!("index {}", rel_part(&x)), on));
                for (child, fk) in refs {
                    listed.push((
                        format!("constraint {fk} on table {}", rel_part(&child)),
                        format!("index {}", rel_part(&x)),
                    ));
                    fks.push((child, fk));
                }
                indexes.push(x);
            }
            // then the session's temporary ones, made after any of the world's it can see
            for t in &temps {
                for (_, x) in of(t) {
                    listed.push((format!("index {}", x.name), on(t, &x)?));
                    temp_indexes.push(x.name.clone());
                }
            }
            continue;
        }
        let users = |am: &str| -> Vec<&Table> { tables.iter().filter(|t| t.hier.am.as_deref() == Some(am)).collect() };
        let order = cascade_order(&[Node::Am(a.name.clone())], &mut |n: &Node| -> Result<Vec<(Node, MadeKey)>> {
            match n {
                Node::Am(am) => Ok(users(am)
                    .into_iter()
                    .map(|t| (Node::Rel(t.name.clone()), rel_order(&v, &t.name, t.matview.is_some())))
                    .collect()),
                Node::Rel(r) => Ok(dependents(&v, r)?
                    .into_iter()
                    .map(|d| (Node::Rel(d.clone()), rel_order(&v, &d, true)))
                    .collect()),
            }
        })?;
        let noun = |name: &str| -> Result<String> {
            let k = relation_kind(&v, name)?.unwrap_or("view");
            Ok(format!("{} {}", if k == "table" { foreign::rel_noun(&v, name) } else { k }, rel_part(name)))
        };
        for (d, from) in order {
            let (Node::Rel(d), from) = (d, from) else { continue };
            let on = match &from {
                Node::Am(am) => format!("access method {am}"),
                Node::Rel(r) => noun(r)?,
            };
            if matches!(from, Node::Am(_)) {
                match relation_kind(&v, &d)? {
                    Some("materialized view") => matviews.push(d.clone()),
                    _ => rels.push(d.clone()),
                }
            }
            listed.push((noun(&d)?, on));
        }
        // the session's temporary tables of the method (a temporary table is a plain table: no views or
        // materialized views in that database depend on it but its own, which its DROP ... CASCADE takes)
        for t in temps.iter().filter(|t| t.hier.am.as_deref() == Some(a.name.as_str())) {
            listed.push((format!("table {}", t.name), format!("access method {}", a.name)));
            temp_rels.push(t.name.clone());
        }
    }
    if !cascade && !listed.is_empty() {
        let message = match &gone[..] {
            [one] => format!("cannot drop access method {} because other objects depend on it", one.name),
            _ => "cannot drop desired object(s) because other objects depend on them".to_string(),
        };
        let lines: Vec<String> =
            listed.iter().take(MAX_REPORTED_DEPS).map(|(d, on)| format!("{d} depends on {on}")).collect();
        return Err(SqlError {
            code: "2BP01",
            message,
            info: Some(Box::new(Violation {
                detail: Some(lines.join("\n") + &not_reported(listed.len())),
                hint: Some("Use DROP ... CASCADE to drop the dependent objects too.".into()),
                ..Violation::default()
            })),
        });
    }
    let own = |w: &View| -> Result<Vec<(String, Option<String>)>> {
        let mut writes = vec![];
        for a in &gone {
            writes.push((format!("{KEY}{}", a.name), None));
            writes.push((comments::row_key(&format!("am/{}", a.name)), None));
            // (its operator classes and families: `_sonos_defs/<kind>/<name>/<method>`)
            for kind in ["opclass", "opfamily"] {
                for r in w.prefix(&format!("_sonos_defs/{kind}/"))? {
                    let (k, _) = r?;
                    if k.rsplit_once('/').is_some_and(|(_, m)| m == a.name) {
                        writes.push((k, None));
                    }
                }
            }
        }
        Ok(writes)
    };
    if listed.is_empty() {
        db.batch_with::<(), SqlError>(branch, |w| Ok((own(w)?, ())))?;
        return Ok(Output::tag("DROP ACCESS METHOD"));
    }
    // CASCADE: what depends on them dropped first, by statements of their own, in a fork merged back as one
    let tmp = format!("{TX_PREFIX}{}", new_id());
    db.fork_info(branch, &tmp, crate::Info::new())?;
    let mark = NOTICES.with_borrow(Vec::len);
    let plain = |st: Stmt| Statement(st, PlanCache::default(), false);
    let done = (|| -> Result<usize> {
        for (child, fk) in &fks {
            let st = Stmt::Alter {
                table: child.clone(),
                changes: vec![Alter::DropConstraint(fk.clone(), true, false)],
                if_exists: true,
                only: false,
            };
            plain(st).execute(db, &tmp, &[], &[])?;
        }
        for x in &indexes {
            let st = Stmt::DropIndex { name: x.clone(), if_exists: true, more: vec![], concurrently: false };
            plain(st).execute(db, &tmp, &[], &[])?;
        }
        if !matviews.is_empty() {
            let st = Stmt::DropObjects {
                kind: "materialized view",
                names: matviews.clone(),
                if_exists: true,
                cascade: true,
            };
            plain(st).execute(db, &tmp, &[], &[])?;
        }
        if !rels.is_empty() {
            plain(Stmt::Drop { tables: rels.clone(), if_exists: true, cascade: true }).execute(db, &tmp, &[], &[])?;
        }
        // the session's temporary ones, on the statement's fork of their database: kept with it, or not at all (see
        // `temp_fork`)
        if (!temp_indexes.is_empty() || !temp_rels.is_empty())
            && let Some(d) = session_temp()
        {
            let at = temp_fork(&d)?;
            let actor = crate::disk::actor();
            let _private = crate::acting_as(""); // its own database: see `Session::temp_db`
            catalog::at_home(&db.core, branch, &actor, || -> Result<()> {
                for x in &temp_indexes {
                    let st = Stmt::DropIndex { name: x.clone(), if_exists: true, more: vec![], concurrently: false };
                    plain(st).execute(&d, &at, &[], &[])?;
                }
                if !temp_rels.is_empty() {
                    let st = Stmt::Drop { tables: temp_rels.clone(), if_exists: true, cascade: true };
                    plain(st).execute(&d, &at, &[], &[])?;
                }
                Ok(())
            })?;
        }
        db.batch_with::<(), SqlError>(&tmp, |w| Ok((own(w)?, ())))?;
        Ok(db.merge(&tmp, crate::Resolve::Fail)?)
    })();
    NOTICES.with_borrow_mut(|n| n.truncate(mark));
    if done.is_err() {
        let _ = db.discard(&tmp);
    }
    done?;
    cascade_notice(&listed.into_iter().map(|(d, _)| d).collect::<Vec<_>>());
    Ok(Output::tag("DROP ACCESS METHOD"))
}
