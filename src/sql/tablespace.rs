//! Tablespaces: CREATE, ALTER and DROP TABLESPACE, and where each relation is kept. Chronos keeps every relation in
//! its one store, so a tablespace has no directory and no files: it is its name and its record, a row of the world
//! (`_sonos_defs/tablespace/<name>/`, JSON: its oid, owner, options and location), checked as Postgres checks it and
//! shown where Postgres shows it (pg_tablespace, pg_class.reltablespace, psql's `\db` and its `Tablespace:` footers).
//! Its privileges are an ACL row as any object's (`_sonos_acl/tablespace/<oid>`, see `acl`), kept by oid so that a
//! rename leaves them. pg_default and pg_global are Postgres's own two, built in.
//!
//! A relation's tablespace is kept by oid with its table's schema (`hier::Hier::tablespace`, and its indexes' by
//! index name), 0 for the database's default as pg_class has it. Moving one (SET TABLESPACE, REINDEX (TABLESPACE
//! ...)) changes that; moving it and rebuilding it (REINDEX) change the relation's filenode generation, which
//! pg_class.relfilenode is worked out from (see `filenode`), as Postgres writes such a relation's files anew; its rows
//! stay where they are. The generations are a row of their own beside the schema (`_sonos_filenodes/<table>`, see
//! `Gens`), so that a REINDEX, which is maintenance, leaves the table's schema row as it was and merges with any
//! change to it (see `crate::Core::merge`).

use super::catalog::Catalog;
use super::*;

pub(super) const KEY: &str = "_sonos_defs/tablespace/";

/// Postgres's two: the database's default, and the one of the relations every database shares.
pub(crate) const DEFAULT: i64 = 1663;
pub(crate) const GLOBAL: i64 = 1664;

/// Where the oids CREATE TABLESPACE hands out begin: the range after event triggers' among those `foreign::name_oids`
/// works out (its kind 14), numbered by a counter every world shares, so a rename keeps one and two worlds never give
/// the same one.
const OIDS: i64 = (1 << 29) + 14 * (1 << 24);

fn key(name: &str) -> String {
    format!("{KEY}{name}/")
}

/// The key of tablespace `oid`'s ACL row (see `acl`).
pub(crate) fn acl_key(oid: i64) -> String {
    format!("tablespace/{oid}")
}

/// A tablespace as its row keeps it.
#[derive(Clone, Debug)]
pub(crate) struct Space {
    pub name: String,
    pub oid: i64,
    pub owner: i64,
    /// WITH (...) and ALTER ... SET (...): `name=value` as written (pg_tablespace.spcoptions)
    pub options: Vec<String>,
    /// LOCATION as written (without a slash at its end); '' for one made in place (allow_in_place_tablespaces)
    pub location: String,
}

impl Space {
    fn from_json(name: &str, j: &Json) -> Space {
        Space {
            name: name.to_string(),
            oid: j["oid"].as_i64().unwrap_or_default(),
            owner: j["owner"].as_i64().unwrap_or(roles::BOOTSTRAP),
            options: j["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|o| o.as_str().map(String::from))
                .collect(),
            location: j["location"].as_str().unwrap_or_default().to_string(),
        }
    }

    fn write(&self) -> (String, Option<String>) {
        let j = serde_json::json!({
            "oid": self.oid,
            "owner": self.owner,
            "options": self.options,
            "location": self.location,
        });
        (key(&self.name), Some(j.to_string()))
    }

    fn builtin(name: &str, oid: i64) -> Space {
        Space { name: name.into(), oid, owner: roles::BOOTSTRAP, options: vec![], location: String::new() }
    }
}

/// Runs `f` on the view tablespaces are read in: `v`, or the session's world while a statement runs on its temporary
/// tables' database, which has none of its own (a tablespace is every database's, in Postgres).
fn in_home<T>(v: &View, f: impl FnOnce(&View) -> Result<T>) -> Result<T> {
    match super::catalog::home() {
        Some((core, world, _)) if !std::ptr::eq(&*core, v.core) => f(&core.view_tx(&world)?),
        _ => f(v),
    }
}

/// Every tablespace of the world: Postgres's two (or the rows that changed them: ALTER TABLESPACE pg_default ...),
/// then the others in name order.
pub(crate) fn load(v: &View) -> Result<Vec<Space>> {
    in_home(v, load_here)
}

fn load_here(v: &View) -> Result<Vec<Space>> {
    let mut stored = vec![];
    for r in v.prefix(KEY)? {
        let (k, raw) = r?;
        let Some(name) = k[KEY.len()..].strip_suffix('/') else { continue };
        stored.push(Space::from_json(name, &serde_json::from_str(&raw).unwrap_or_default()));
    }
    let mut out = vec![];
    for b in [Space::builtin("pg_default", DEFAULT), Space::builtin("pg_global", GLOBAL)] {
        match stored.iter().position(|s| s.oid == b.oid) {
            Some(k) => out.push(stored.remove(k)),
            None => out.push(b),
        }
    }
    out.extend(stored);
    Ok(out)
}

/// Tablespace `name`, if there is one.
pub(crate) fn find(v: &View, name: &str) -> Result<Option<Space>> {
    Ok(load(v)?.into_iter().find(|s| s.name == name))
}

fn missing(name: &str) -> SqlError {
    SqlError { code: "42704", message: format!("tablespace \"{name}\" does not exist"), info: None }
}

/// Tablespace `name`, or 42704.
pub(crate) fn named(v: &View, name: &str) -> Result<Space> {
    find(v, name)?.ok_or_else(|| missing(name))
}

/// The name of tablespace `oid`, if there is one.
pub(crate) fn name_of(v: &View, oid: i64) -> Result<Option<String>> {
    Ok(load(v)?.into_iter().find(|s| s.oid == oid).map(|s| s.name))
}

fn hinted(code: &'static str, message: String, hint: Option<&str>, detail: Option<String>) -> SqlError {
    SqlError {
        code,
        message,
        info: Some(Box::new(Violation { hint: hint.map(String::from), detail, ..Violation::default() })),
    }
}

/// 42939 for a name of Postgres's own kind.
fn reserved(name: &str) -> Result<()> {
    if !name.starts_with("pg_") {
        return Ok(());
    }
    Err(hinted(
        "42939",
        format!("unacceptable tablespace name \"{name}\""),
        None,
        Some("The prefix \"pg_\" is reserved for system tablespaces.".into()),
    ))
}

/// The tablespace as privileges see it: its owner's, with its owner's CREATE only until a GRANT.
pub(crate) fn obj(v: &View, s: &Space) -> Result<acl::Obj> {
    in_home(v, |v| acl::tablespace_obj(v, &acl_key(s.oid), &s.name, s.owner))
}

/// Whether the running role may act as the tablespace's owner: a superuser, or a role with its owner's privileges.
fn owns(s: &Space) -> bool {
    roles::checked().is_none_or(|w| w.has_privs_of(s.owner))
}

fn not_owner(s: &Space) -> SqlError {
    SqlError { code: "42501", message: format!("must be owner of tablespace {}", s.name), info: None }
}

fn denied(s: &Space) -> SqlError {
    SqlError { code: "42501", message: format!("permission denied for tablespace {}", s.name), info: None }
}

/// CREATE on tablespace `oid`, which a role needs to put a relation there (42501 without it); the database's default
/// asks none.
pub(super) fn need_create(v: &View, oid: i64) -> Result<()> {
    let Some(who) = roles::checked() else { return Ok(()) };
    if oid == 0 || oid == DEFAULT {
        return Ok(());
    }
    let Some(s) = load(v)?.into_iter().find(|s| s.oid == oid) else { return Ok(()) };
    let o = obj(v, &s)?;
    if o.has(&who, acl::CREATE) { Ok(()) } else { Err(o.denied()) }
}

fn on(setting: &str) -> bool {
    super::session_setting(setting).and_then(|v| settings::parse_bool(&v)).unwrap_or(false)
}

/// 0A000 for a partitioned relation in the database's default named: it has no files to put there, and an empty
/// setting already means its partitions' default (Postgres's GetDefaultTablespace and DefineRelation refuse it).
fn partitioned_default() -> SqlError {
    SqlError {
        code: "0A000",
        message: "cannot specify default tablespace for partitioned relations".into(),
        info: None,
    }
}

/// Where default_tablespace puts a new relation, as Postgres's GetDefaultTablespace reads it: '' (or a tablespace
/// that isn't there) is the database's default, 0.
fn default_space(v: &View, partitioned: bool) -> Result<i64> {
    let name = super::session_setting("default_tablespace").unwrap_or_default();
    if name.is_empty() {
        return Ok(0);
    }
    match find(v, &name)? {
        Some(s) if s.oid == DEFAULT && partitioned => Err(partitioned_default()),
        Some(s) if s.oid == DEFAULT => Ok(0),
        Some(s) => Ok(s.oid),
        None => Ok(0),
    }
}

/// Where temp_tablespaces puts a new temporary relation, as Postgres's GetNextTempTableSpace reads it: '' is the
/// database's default, 0; else one of its tablespaces that is there and the role may create in (PrepareTempTablespaces
/// leaves the others out; Postgres starts at a random one, this takes the first), the database's default if none is.
fn temp_space(v: &View) -> Result<i64> {
    let list = super::session_setting("temp_tablespaces").unwrap_or_default();
    for name in list.split(',').map(|n| n.trim().trim_matches('"')).filter(|n| !n.is_empty()) {
        let Some(s) = find(v, name)? else { continue };
        if s.oid == DEFAULT {
            return Ok(0);
        }
        if need_create(v, s.oid).is_ok() {
            return Ok(s.oid);
        }
    }
    Ok(0)
}

/// Whether `v` is the session's temporary tables' database, not its world's: a temporary relation is being made.
fn temporary(v: &View) -> bool {
    super::catalog::home().is_some_and(|(core, ..)| !std::ptr::eq(&*core, v.core))
}

/// Where a new relation is kept (pg_class.reltablespace, 0 for the database's default), as Postgres's DefineRelation
/// and DefineIndex choose: the tablespace named, else `up` (a partition's parent's own), else default_tablespace's
/// (temp_tablespaces' for a temporary one, as Postgres's GetDefaultTablespace has it).
/// The database's default named for a partitioned relation is refused, and so is pg_global, which only the relations
/// every database shares are in. `check`: the role needs CREATE on it (not for an index a partition is given as a
/// copy of its parent's, which Postgres makes without asking).
pub(super) fn place(v: &View, given: Option<&str>, up: i64, partitioned: bool, check: bool) -> Result<i64> {
    let mut oid = match given {
        Some(n) => {
            let o = named(v, n)?.oid;
            if partitioned && o == DEFAULT {
                return Err(partitioned_default());
            }
            o
        }
        None => up,
    };
    if oid == 0 {
        oid = if temporary(v) { temp_space(v)? } else { default_space(v, partitioned)? };
    }
    if check {
        need_create(v, oid)?;
    }
    if oid == GLOBAL {
        return Err(only_shared());
    }
    Ok(if oid == DEFAULT { 0 } else { oid })
}

fn only_shared() -> SqlError {
    SqlError {
        code: "22023",
        message: "only shared relations can be placed in pg_global tablespace".into(),
        info: None,
    }
}

/// The tablespace a SET TABLESPACE, ALTER ... ALL IN TABLESPACE or REINDEX (TABLESPACE ...) names, as Postgres checks
/// it before anything moves: there, and the role has CREATE on it (the database's default asks none). Its oid.
pub(super) fn target(v: &View, name: &str) -> Result<i64> {
    let s = named(v, name)?;
    need_create(v, s.oid)?;
    Ok(s.oid)
}

/// What SET default_tablespace takes (Postgres's check_default_tablespace): '' or a tablespace that is there.
pub(super) fn check_default(v: &View, value: &str) -> Result<()> {
    if value.is_empty() || find(v, value)?.is_some() {
        return Ok(());
    }
    Err(hinted(
        "22023",
        format!("invalid value for parameter \"default_tablespace\": \"{value}\""),
        None,
        Some(format!("Tablespace \"{value}\" does not exist.")),
    ))
}

/// pg_class.relfilenode of a relation of oid `oid` whose storage was made anew `generation` times (REINDEX, SET
/// TABLESPACE): its oid at first, then the oid with a generation in the bits above every relation's (oids worked out
/// from names are below 2^29), so no two relations share one and a moved relation's is another.
pub(crate) fn filenode(oid: i64, generation: i64) -> i64 {
    if generation == 0 { oid } else { oid | ((((generation - 1) % 7) + 1) << 29) }
}

/// Where a table's filenode generations are kept, by its stored name (see `Gens`).
pub(crate) const FILENODES: &str = "_sonos_filenodes/";

fn gens_key(table: &str) -> String {
    format!("{FILENODES}{table}")
}

/// How many times a table's storage, and each of its indexes', was made anew (SET TABLESPACE, REINDEX): a row of its
/// own (`_sonos_filenodes/<table>`, JSON `{"table": n, "indexes": {name: n}}`), apart from the table's schema, as
/// maintenance that changes no definition. None made anew: no row.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Gens {
    pub table: i64,
    pub indexes: Vec<(String, i64)>,
}

impl Gens {
    fn from_raw(raw: &str) -> Gens {
        let j: Json = serde_json::from_str(raw).unwrap_or_default();
        let indexes = j["indexes"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(x, n)| Some((x.clone(), n.as_i64()?)))
            .collect();
        Gens { table: j["table"].as_i64().unwrap_or(0), indexes }
    }

    /// Table `table`'s (none: all 0).
    pub(super) fn load(v: &View, table: &str) -> Result<Gens> {
        Ok(v.get(&gens_key(table))?.map_or_else(Gens::default, |raw| Gens::from_raw(&raw)))
    }

    /// Every table's, by stored name: what pg_class shows (see `tables`).
    pub(super) fn all(v: &View) -> Result<HashMap<String, Gens>> {
        let mut out = HashMap::new();
        for r in v.prefix(FILENODES)? {
            let (k, raw) = r?;
            out.insert(k[FILENODES.len()..].to_string(), Gens::from_raw(&raw));
        }
        Ok(out)
    }

    /// The write that keeps them as table `table`'s (a delete when there are none).
    pub(super) fn write(&self, table: &str) -> (String, Option<String>) {
        if self.table == 0 && self.indexes.is_empty() {
            return (gens_key(table), None);
        }
        let indexes: serde_json::Map<String, Json> =
            self.indexes.iter().map(|(x, n)| (x.clone(), Json::from(*n))).collect();
        (gens_key(table), Some(serde_json::json!({ "table": self.table, "indexes": indexes }).to_string()))
    }

    /// The writes that keep them, if they differ from `was` (what the row held).
    pub(super) fn changed(&self, was: &Gens, table: &str) -> Vec<(String, Option<String>)> {
        if self == was { vec![] } else { vec![self.write(table)] }
    }

    pub(super) fn index(&self, x: &str) -> i64 {
        self.indexes.iter().find(|(n, _)| n == x).map_or(0, |(_, g)| *g)
    }

    pub(super) fn bump_index(&mut self, x: &str) {
        let g = self.index(x) + 1;
        self.indexes.retain(|(n, _)| n != x);
        self.indexes.push((x.to_string(), g));
    }

    /// Index `x` is gone (or is made anew under its name): its storage's count starts afresh.
    pub(super) fn forget(&mut self, x: &str) {
        self.indexes.retain(|(n, _)| n != x);
    }

    /// Index `from` is called `to` now, with the storage it had.
    pub(super) fn renamed(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        self.indexes.retain(|(n, _)| n != to);
        for (n, _) in self.indexes.iter_mut().filter(|(n, _)| n == from) {
            *n = to.to_string();
        }
    }
}

/// The write that drops table `table`'s generations with it, if it has any.
pub(super) fn gens_dropped(v: &View, table: &str) -> Result<Option<(String, Option<String>)>> {
    Ok(v.get(&gens_key(table))?.map(|_| (gens_key(table), None)))
}

/// Moves table `t` (`index` None) or its index `index` to tablespace `to` (an oid, 0 or DEFAULT for the database's
/// default), as ALTER ... SET TABLESPACE does: nothing when it is there already, and pg_global refused. A partitioned
/// relation only records where its partitions are made from then on; one with storage is given a new filenode by the
/// caller (see `Gens`). Whether it moved.
pub(super) fn move_rel(t: &mut Table, index: Option<&str>, to: i64) -> Result<bool> {
    let to = if to == DEFAULT { 0 } else { to };
    let now = match index {
        Some(x) => t.hier.index_space(x),
        None => t.hier.tablespace,
    };
    if to == now {
        return Ok(false);
    }
    if to == GLOBAL {
        return Err(only_shared());
    }
    match index {
        Some(x) => t.hier.set_index_space(x, to),
        None => t.hier.tablespace = to,
    }
    Ok(true)
}

/// `move_rel` with the new filenode a relation with storage gets (in `gens`, its table's).
pub(super) fn move_with_storage(t: &mut Table, gens: &mut Gens, index: Option<&str>, to: i64) -> Result<bool> {
    let moved = move_rel(t, index, to)?;
    if moved && t.hier.part.is_none() {
        match index {
            Some(x) => gens.bump_index(x),
            None => gens.table += 1,
        }
    }
    Ok(moved)
}

/// What ALTER TABLE | INDEX | MATERIALIZED VIEW name SET TABLESPACE space moves, a table (`.1` None) or the table index
/// `.1` is one of, and the tablespace's oid, checked as Postgres's ATPrepSetTableSpace checks them before anything
/// changes: the relation is there and isn't a foreign table, and the tablespace is one the role may use (see `target`).
pub(super) fn set_prep(v: &View, name: &str, space: &str) -> Result<(Table, Option<String>, i64)> {
    let (t, index) = match super::load(v, name)?.filter(|t| t.schema) {
        Some(t) => (t, None),
        None => {
            // an index: its table's to keep
            let full = text_rel(name);
            let (schema, ix) = (schema_part(&full).to_string(), rel_part(&full).to_string());
            let owner = schemas(v)?.into_iter().find(|t| {
                schema_part(&t.name) == schema
                    && (t.indexes.iter().any(|x| x.name == ix)
                        || t.uniques.iter().any(|u| u.name == ix)
                        || t.ann.iter().any(|(n, _, _)| *n == ix)
                        || (t.has_key() && t.pkey_name() == ix))
            });
            match owner {
                Some(t) => (t, Some(ix)),
                None => return err(NO_TABLE, format!("relation \"{name}\" does not exist")),
            }
        }
    };
    if index.is_none() && t.hier.foreign.is_some() {
        return Err(SqlError {
            code: "42809",
            message: format!("ALTER action SET TABLESPACE cannot be performed on relation \"{}\"", rel_part(&t.name)),
            info: Some(Box::new(Violation {
                detail: Some("This operation is not supported for foreign tables.".into()),
                ..Violation::default()
            })),
        });
    }
    let to = target(v, space)?;
    Ok((t, index, to))
}

/// The writes of ALTER TABLE | INDEX | MATERIALIZED VIEW name SET TABLESPACE space (see `Exec::set_tablespace`),
/// checked as Postgres's ATPrepSetTableSpace and ATExecSetTableSpace check it: none when the relation is there already.
pub(super) fn set_writes(v: &View, name: &str, space: &str) -> Result<Vec<(String, Option<String>)>> {
    let (mut t, index, to) = set_prep(v, name, space)?;
    let mut gens = Gens::load(v, &t.name)?;
    let was = gens.clone();
    if !move_with_storage(&mut t, &mut gens, index.as_deref(), to)? {
        return Ok(vec![]);
    }
    let mut writes = vec![(schema_key(&t.name), Some(schema_json(&t)))];
    writes.extend(gens.changed(&was, &t.name));
    Ok(writes)
}

/// The move of an ALTER TABLE's SET TABLESPACE `space`, if it has one, after the statement's other changes `writes`,
/// which aren't written yet: made on the relation as they leave it and added to them (in place of their write of a key
/// it writes too), so that it is written with them.
pub(super) fn set_after(
    v: &View,
    writes: &mut Vec<(String, Option<String>)>,
    name: &str,
    space: Option<&str>,
) -> Result<()> {
    let Some(space) = space else { return Ok(()) };
    let mut s = v.s.clone();
    for (k, x) in writes.iter() {
        s.set(k, x.as_deref().map(Val::from));
    }
    for (k, x) in set_writes(&View { s, ..v.clone() }, name, space)? {
        match writes.iter_mut().rev().find(|(w, _)| *w == k) {
            Some(w) => w.1 = x,
            None => writes.push((k, x)),
        }
    }
    Ok(())
}

/// The tablespace's location as pg_tablespace_location gives it: a path in the data folder for one made in place,
/// '' for Postgres's two (and the database's default, 0) which are the data folder's own; None for an oid of none.
pub(super) fn location(cat: &Catalog, oid: i64) -> Option<String> {
    if matches!(oid, 0 | DEFAULT | GLOBAL) {
        return Some(String::new());
    }
    let s = cat.spaces.iter().find(|s| s.oid == oid)?;
    Some(if s.location.is_empty() { format!("pg_tblspc/{oid}") } else { s.location.clone() })
}

/// pg_tablespace's rows.
pub(super) fn catalog_rows(cat: &Catalog) -> Vec<Vec<Value>> {
    let row = |fields: Vec<(&str, Value)>| catalog_schema::named_row("pg_tablespace", fields);
    cat.spaces
        .iter()
        .map(|s| {
            let acl = cat
                .acls
                .get(&acl_key(s.oid))
                .and_then(|e| e.acl.as_ref())
                .map_or(Value::Null, |a| Value::Text(acl::text(&cat.reg, a)));
            row(vec![
                ("oid", Value::Int(s.oid)),
                ("spcname", Value::Text(s.name.clone())),
                ("spcowner", Value::Int(s.owner)),
                ("spcacl", acl),
                ("spcoptions", reloptions::shown(&s.options)),
            ])
        })
        .collect()
}

/// The tablespaces role `oid` owns, by name: what DROP ROLE lists.
pub(super) fn owned_by(v: &View, oid: i64) -> Result<Vec<String>> {
    Ok(load(v)?.into_iter().filter(|s| s.owner == oid && !matches!(s.oid, DEFAULT | GLOBAL)).map(|s| s.name).collect())
}

/// The first index (by oid, as Postgres's RelationGetIndexList orders them) of the catalog relations REINDEX may
/// name: what Postgres's reindex_index refuses to move first. Each catalog table, TOAST table and index by the names
/// a statement may give it.
const SYSTEM_INDEXES: &[(&str, &str)] = &[
    ("pg_am", "pg_am_name_index"),
    ("pg_authid", "pg_authid_rolname_index"),
    ("pg_class", "pg_class_oid_index"),
    ("pg_toast.pg_toast_1260", "pg_toast_1260_index"),
    ("pg_toast.pg_toast_1260_index", "pg_toast_1260_index"),
];

/// The catalog relation REINDEX names as `text` (`pg_am`, `pg_catalog.pg_am`, `pg_toast.pg_toast_1260`): the index it
/// would rebuild first. None for any other relation.
pub(super) fn system_index(text: &str) -> Option<&'static str> {
    let t = text.trim().to_lowercase();
    let t = t.strip_prefix("pg_catalog.").unwrap_or(&t);
    SYSTEM_INDEXES.iter().find(|(n, _)| *n == t).map(|(_, x)| *x)
}

// ---------------------------------------------------------------- statements

/// What ALTER TABLESPACE does.
#[derive(Clone, Debug)]
pub(crate) enum How {
    /// SET (...) or RESET (...) (true)
    Options(Vec<reloptions::Given>, bool),
    Rename(String),
    Owner(roles::Spec),
}

#[derive(Clone, Debug)]
pub(crate) enum TsStmt {
    Create {
        name: String,
        owner: Option<roles::Spec>,
        location: String,
        options: Vec<reloptions::Given>,
    },
    Alter {
        name: String,
        how: How,
    },
    Drop {
        name: String,
        if_exists: bool,
    },
    /// ALTER TABLE | INDEX | MATERIALIZED VIEW ALL IN TABLESPACE from [OWNED BY roles] SET TABLESPACE to [NOWAIT]: the
    /// kind (`r` tables, `i` indexes, `m` materialized views)
    MoveAll {
        kind: char,
        from: String,
        owned_by: Vec<roles::Spec>,
        to: String,
    },
}

impl TsStmt {
    /// Its command tag.
    pub(super) fn tag(&self) -> &'static str {
        match self {
            TsStmt::Create { .. } => "CREATE TABLESPACE",
            TsStmt::Alter { .. } => "ALTER TABLESPACE",
            TsStmt::Drop { .. } => "DROP TABLESPACE",
            TsStmt::MoveAll { kind: 'i', .. } => "ALTER INDEX",
            TsStmt::MoveAll { kind: 'm', .. } => "ALTER MATERIALIZED VIEW",
            TsStmt::MoveAll { .. } => "ALTER TABLE",
        }
    }

    /// The command, if it is one that can't run inside a transaction block (it would make or remove the
    /// tablespace's directory, in Postgres).
    pub(super) fn alone(&self) -> Option<&'static str> {
        matches!(self, TsStmt::Create { .. } | TsStmt::Drop { .. }).then(|| self.tag())
    }
}

impl Parser {
    /// After CREATE, ALTER or DROP TABLESPACE.
    pub(super) fn tablespace(&mut self, verb: &str) -> Result<Stmt> {
        let st = match verb {
            "create" => {
                let name = self.name()?;
                let owner = if self.kw("owner") { Some(self.role_spec(false)?) } else { None };
                self.expect_kw("location")?;
                let location = match self.next() {
                    Some(Tok::Str(l)) => l,
                    t => {
                        self.put_back(t);
                        return self.fail("expected the tablespace's location as a string");
                    }
                };
                let options = if self.kw("with") { reloptions::parse(self)? } else { vec![] };
                TsStmt::Create { name, owner, location, options }
            }
            "alter" => {
                let name = self.name()?;
                let how = if self.kw("rename") {
                    self.expect_kw("to")?;
                    How::Rename(self.name()?)
                } else if self.kw("owner") {
                    self.expect_kw("to")?;
                    How::Owner(self.role_spec(false)?)
                } else if self.is_kw("set") || self.is_kw("reset") {
                    let reset = self.kw("reset");
                    if !reset {
                        self.expect_kw("set")?;
                    }
                    How::Options(reloptions::parse(self)?, reset)
                } else {
                    return self.fail("expected SET, RESET, RENAME TO or OWNER TO");
                };
                TsStmt::Alter { name, how }
            }
            _ => {
                let if_exists = self.if_exists()?;
                TsStmt::Drop { name: self.name()?, if_exists }
            }
        };
        Ok(Stmt::Foreign(Box::new(foreign::ForeignStmt::Tablespace(st))))
    }

    /// Whether ALTER TABLE, INDEX or MATERIALIZED VIEW goes on with `ALL IN TABLESPACE`.
    pub(super) fn at_move_all(&self) -> bool {
        let at = |n: usize, w: &str| matches!(self.t.get(self.i + n), Some(Tok::Word(x)) if x == w);
        at(0, "all") && at(1, "in") && at(2, "tablespace")
    }

    /// After ALTER TABLE, INDEX or MATERIALIZED VIEW (`kind` `r`, `i` or `m`), at ALL: `ALL IN TABLESPACE name [OWNED
    /// BY role, ...] SET TABLESPACE new [NOWAIT]`.
    pub(super) fn move_all(&mut self, kind: char) -> Result<Stmt> {
        self.expect_kw("all")?;
        self.expect_kw("in")?;
        self.expect_kw("tablespace")?;
        let from = self.name()?;
        let mut owned_by = vec![];
        if self.kw("owned") {
            self.expect_kw("by")?;
            owned_by.push(self.role_spec(false)?);
            while self.sym(",") {
                owned_by.push(self.role_spec(false)?);
            }
        }
        self.expect_kw("set")?;
        self.expect_kw("tablespace")?;
        let to = self.name()?;
        // (NOWAIT: nothing here waits for a lock to move a relation)
        self.kw("nowait");
        Ok(Stmt::Foreign(Box::new(foreign::ForeignStmt::Tablespace(TsStmt::MoveAll { kind, from, owned_by, to }))))
    }
}

/// A location as Postgres's canonicalize_path keeps it: without a slash at its end.
fn canonical(location: &str) -> String {
    let t = location.trim_end_matches('/');
    if t.is_empty() && !location.is_empty() { "/".into() } else { t.to_string() }
}

/// Runs a tablespace statement on `branch`.
pub(super) fn run(db: &Db, branch: &str, st: &TsStmt) -> Result<Output> {
    let me = roles::current().map_or(roles::BOOTSTRAP, |w| w.oid);
    match st {
        TsStmt::Create { name, owner, location, options } => {
            if roles::checked().is_some() {
                return Err(hinted(
                    "42501",
                    format!("permission denied to create tablespace \"{name}\""),
                    Some("Must be superuser to create a tablespace."),
                    None,
                ));
            }
            let reg = roles::registry(&db.core)?;
            let owner = match owner {
                Some(s) => s.resolve(&reg)?.oid,
                None => me,
            };
            if location.contains('\'') {
                return err("42602", "tablespace location cannot contain single quotes");
            }
            let location = canonical(location);
            // (allow_in_place_tablespaces: '' is a directory of the data folder's own, for tests; there is none here
            // either way)
            let in_place = location.is_empty() && on("allow_in_place_tablespaces");
            if !in_place && !location.starts_with('/') {
                return err("42P17", "tablespace location must be an absolute path");
            }
            reserved(name)?;
            let v = db.view_tx(branch)?;
            if find(&v, name)?.is_some() {
                return err("42710", format!("tablespace \"{name}\" already exists"));
            }
            let given = reloptions::transform(&[], options, None, &[], false)?;
            reloptions::check(reloptions::Kind::Tablespace, &given)?;
            // the directory has to be there, as Postgres's create_tablespace_directories asks, though nothing is put
            // in it; a session that may not see the server's files (an agent's) isn't told what is there
            if !in_place && (acl::agents() || !std::path::Path::new(&location).is_dir()) {
                return err("58P01", format!("directory \"{location}\" does not exist"));
            }
            let taken: Vec<i64> = load(&v)?.iter().map(|s| s.oid).collect();
            let oid = loop {
                let n = db.core.next_values("_sonos_tablespace_oids", 1)?[0];
                let o = OIDS + n;
                if o >= OIDS + (1 << 24) {
                    return err("54000", "no tablespace oids are left");
                }
                if !taken.contains(&o) {
                    break o;
                }
            };
            let s = Space { name: name.clone(), oid, owner, options: given, location };
            db.batch_with::<(), SqlError>(branch, |v| {
                if find(v, name)?.is_some() {
                    return err("42710", format!("tablespace \"{name}\" already exists"));
                }
                Ok((vec![s.write()], ()))
            })?;
        }
        TsStmt::Alter { name, how } => {
            db.batch_with::<(), SqlError>(branch, |v| {
                let Some(mut s) = find(v, name)? else { return Err(missing(name)) };
                let writes = match how {
                    How::Rename(to) => {
                        if !owns(&s) {
                            return Err(denied(&s));
                        }
                        reserved(to)?;
                        if find(v, to)?.is_some() {
                            return err("42710", format!("tablespace \"{to}\" already exists"));
                        }
                        let gone = (key(&s.name), None);
                        s.name.clone_from(to);
                        vec![gone, s.write()]
                    }
                    How::Owner(spec) => {
                        let reg = roles::registry(v.core)?;
                        let to = spec.resolve(&reg)?;
                        if to.oid == s.owner {
                            return Ok((vec![], ()));
                        }
                        if let Some(w) = roles::checked() {
                            if !w.has_privs_of(s.owner) {
                                return Err(not_owner(&s));
                            }
                            if !reg.can_set(w.oid, to.oid) {
                                return err("42501", format!("must be able to SET ROLE \"{}\"", to.name));
                            }
                        }
                        // (the grants the old owner gave are the new owner's, as Postgres's aclnewowner has them)
                        let mut writes = vec![];
                        let k = acl_key(s.oid);
                        let mut e = acl::entry(v, &k)?;
                        if let Some(a) = e.acl.take() {
                            e.acl = Some(acl::new_owner_acl(a, s.owner, to.oid));
                            writes.push(acl::write_entry(&k, &e));
                        }
                        s.owner = to.oid;
                        writes.push(s.write());
                        writes
                    }
                    How::Options(given, reset) => {
                        if !owns(&s) {
                            return Err(not_owner(&s));
                        }
                        let options = reloptions::transform(&s.options, given, None, &[], *reset)?;
                        reloptions::check(reloptions::Kind::Tablespace, &options)?;
                        s.options = options;
                        vec![s.write()]
                    }
                };
                Ok((writes, ()))
            })?;
        }
        TsStmt::Drop { name, if_exists } => {
            let mut skipped = false;
            db.batch_with::<(), SqlError>(branch, |v| {
                let Some(s) = find(v, name)? else {
                    if *if_exists {
                        skipped = true;
                        return Ok((vec![], ()));
                    }
                    return Err(missing(name));
                };
                if !owns(&s) {
                    return Err(not_owner(&s));
                }
                if matches!(s.oid, DEFAULT | GLOBAL) {
                    return Err(denied(&s));
                }
                // (the world's relations, and the session's temporary ones, kept in a database of their own: another
                // session's are out of reach here)
                let mut cats = vec![Catalog::of(v.core, &v.branch)?];
                if let Some((tdb, tbranch)) = super::TEMP.with_borrow(Clone::clone) {
                    cats.push(Catalog::of(&tdb.core, &tbranch)?);
                }
                // the partitioned relations that name it: having no files there, they are its dependents (Postgres's
                // pg_shdepend rows of SHARED_DEPENDENCY_TABLESPACE)
                let mut deps = vec![];
                for cat in &cats {
                    for r in cat.rels.iter().filter(|r| r.kind == 'p' && r.hier.tablespace == s.oid) {
                        deps.push(format!("tablespace for table {}", shown_name(&r.name)));
                    }
                    for (i, x) in cat.idx.iter().filter(|(i, x)| cat.rels[*i].kind == 'p' && x.tablespace == s.oid) {
                        deps.push(format!("tablespace for index {}", shown_index(&cat.rels[*i].name, &x.name)));
                    }
                }
                if !deps.is_empty() {
                    return Err(hinted(
                        "2BP01",
                        format!("tablespace \"{}\" cannot be dropped because some objects depend on it", s.name),
                        None,
                        Some(deps.join("\n")),
                    ));
                }
                // and the others are in it: its directory isn't empty
                let used = cats.iter().any(|cat| {
                    cat.rels.iter().any(|r| r.kind != 'p' && r.hier.tablespace == s.oid)
                        || cat.idx.iter().any(|(_, x)| x.tablespace == s.oid)
                });
                if used {
                    return err("55000", format!("tablespace \"{}\" is not empty", s.name));
                }
                Ok((vec![(key(&s.name), None), (acl::row_key(&acl_key(s.oid)), None)], ()))
            })?;
            if skipped {
                notice("NOTICE", format!("tablespace \"{name}\" does not exist, skipping"));
            }
        }
        TsStmt::MoveAll { kind, from, owned_by, to } => move_all(db, branch, *kind, from, owned_by, to)?,
    }
    Ok(Output::tag(st.tag()))
}

/// ALTER TABLE | INDEX | MATERIALIZED VIEW ALL IN TABLESPACE from SET TABLESPACE to, as Postgres's AlterTableMoveAll:
/// every relation of the kind in `from` (those the roles OWNED BY names own) moved to `to`, the catalogs' left; the
/// role must own each.
fn move_all(db: &Db, branch: &str, kind: char, from: &str, owned_by: &[roles::Spec], to: &str) -> Result<()> {
    let reg = roles::registry(&db.core)?;
    let owners: Vec<i64> = owned_by.iter().map(|s| s.oid(&reg)).collect::<Result<_>>()?;
    let mut none = None;
    db.batch_with::<(), SqlError>(branch, |v| {
        none = None;
        let orig = named(v, from)?.oid;
        let new = named(v, to)?.oid;
        if orig == GLOBAL || new == GLOBAL {
            return err("22023", "cannot move relations in to or out of pg_global tablespace");
        }
        need_create(v, new)?;
        let norm = |o: i64| if o == DEFAULT { 0 } else { o };
        let (orig, new) = (norm(orig), norm(new));
        if orig == new {
            return Ok((vec![], ()));
        }
        let cat = Catalog::of(v.core, &v.branch)?;
        let system = |name: &str| matches!(schema_part(name), "pg_catalog" | "information_schema" | "pg_toast");
        // (table, index or none, its owner, its kind as messages name it, its name)
        let mut found: Vec<(String, Option<String>, i64, &str, String)> = vec![];
        if kind == 'i' {
            for (i, x) in cat.idx.iter().filter(|(_, x)| x.tablespace == orig) {
                let r = &cat.rels[*i];
                if !system(&r.name) {
                    found.push((r.name.clone(), Some(x.name.clone()), cat.rel_acl(r).0, "index", x.name.clone()));
                }
            }
        } else {
            let kinds: &[char] = if kind == 'm' { &['m'] } else { &['r', 'p'] };
            for r in cat.rels.iter().filter(|r| kinds.contains(&r.kind) && r.hier.tablespace == orig) {
                if !system(&r.name) {
                    let noun = if r.kind == 'm' { "materialized view" } else { "table" };
                    found.push((r.name.clone(), None, cat.rel_acl(r).0, noun, rel_part(&r.name).to_string()));
                }
            }
        }
        found.retain(|(.., owner, _, _)| owners.is_empty() || owners.contains(owner));
        if let Some(w) = roles::checked()
            && let Some((.., noun, name)) = found.iter().find(|(_, _, o, ..)| !w.has_privs_of(*o))
        {
            return err("42501", format!("must be owner of {noun} {name}"));
        }
        if found.is_empty() {
            none = Some(if orig == 0 { "(database default)".to_string() } else { from.to_string() });
            return Ok((vec![], ()));
        }
        // (each table with its generations, as they were and as they are)
        let mut tables: Vec<(Table, Gens, Gens)> = vec![];
        for (table, index, ..) in found {
            let at = match tables.iter().position(|(t, ..)| t.name == table) {
                Some(k) => k,
                None => {
                    let g = Gens::load(v, &table)?;
                    tables.push((need_table(v, &table)?, g.clone(), g));
                    tables.len() - 1
                }
            };
            // (a partitioned table and its indexes have no files to write anew)
            let (t, _, g) = &mut tables[at];
            move_with_storage(t, g, index.as_deref(), new)?;
        }
        let mut writes = vec![];
        for (t, was, g) in &tables {
            writes.push((schema_key(&t.name), Some(schema_json(t))));
            writes.extend(g.changed(was, &t.name));
        }
        Ok((writes, ()))
    })?;
    if let Some(name) = none {
        notice("NOTICE", format!("no matching relations in tablespace \"{name}\" found"));
    }
    Ok(())
}
