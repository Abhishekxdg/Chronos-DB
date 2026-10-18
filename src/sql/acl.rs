//! Privileges: who owns each object, what each role may do with it (GRANT and REVOKE), and the
//! checks statements pass, as Postgres's ACLs.
//!
//! An object's owner and grants are a row of its world, `_sonos_acl/<kind>/<name>`
//! (`rel/<table, view or sequence>`, `col/<table>/<column>`, `schema/<name>`,
//! `func/<signature>`, `type/<name>`, `db`): `{"owner": oid, "acl": [[grantee, grantor, privileges,
//! grant options], ...]}`. So they fork, merge, roll back and travel in time with the objects they
//! are on; a rename moves them and a drop drops them (`moves`, `after`). An object with no row is
//! owned by the bootstrap superuser and has Postgres's default privileges, which is every object
//! of a database that never made a role: such a database is unchanged.
//!
//! ALTER DEFAULT PRIVILEGES's entries are rows of the world too, `defacl/<role oid>/<kind>` (a
//! role's own, for every schema) and `defacl/<role oid>/<kind>/<schema>` (`{"acl": [...], "at": n}`,
//! `at` the order they were made in): what an object a role makes starts with (`made`), as
//! Postgres's get_user_default_acl combines them.
//!
//! The checks run before a statement does, on the statement as written (`check`): the relations it
//! reads and writes, and the columns, as Postgres's executor checks its range table. A view is
//! read with its owner's privileges: its query is checked as that role. Functions are checked
//! when called (`may_execute`), sequences by nextval and its kin, and a SECURITY DEFINER
//! function runs as its owner (`roles::acting`). A superuser, and every session whose login is no
//! role, skips them all.

use super::roles::{self, BOOTSTRAP, DB_OWNER, PUBLIC, Spec, Who};
use super::*;
use std::collections::BTreeSet;

pub(crate) const ACLS: &str = "_sonos_acl";

// privilege bits, as Postgres numbers them: an ACL item's letters are in this order
pub(super) const INSERT: u32 = 1;
pub(super) const SELECT: u32 = 1 << 1;
pub(super) const UPDATE: u32 = 1 << 2;
pub(super) const DELETE: u32 = 1 << 3;
pub(super) const TRUNCATE: u32 = 1 << 4;
pub(super) const REFERENCES: u32 = 1 << 5;
pub(super) const TRIGGER: u32 = 1 << 6;
pub(super) const EXECUTE: u32 = 1 << 7;
pub(super) const USAGE: u32 = 1 << 8;
pub(super) const CREATE: u32 = 1 << 9;
pub(super) const TEMP: u32 = 1 << 10;
pub(super) const CONNECT: u32 = 1 << 11;
const LETTERS: &[u8] = b"arwdDxtXUCTc";

thread_local! {
    /// The table CREATE TABLE AS (or SELECT INTO) is filling with its query's rows (see `filling`).
    static FILLING: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f`, CREATE TABLE AS's INSERT of its query's rows into `table`, the table it made: its maker needs no INSERT
/// privilege on it (a default privilege may have taken it away), as Postgres's CREATE TABLE AS asks none; what the
/// query reads still needs SELECT.
pub(super) fn filling<T>(table: &str, f: impl FnOnce() -> T) -> T {
    let before = FILLING.replace(Some(table.to_string()));
    let out = f();
    FILLING.set(before);
    out
}

/// What an ACL is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Table,
    View,
    MatView,
    Sequence,
    Schema,
    Function,
    Procedure,
    Database,
    Column,
    Type,
    /// an extension CREATE EXTENSION recorded (pgcrypto): an owner, no privileges
    Extension,
    /// a foreign-data wrapper, and a foreign server (see `foreign`)
    Fdw,
    Server,
    /// a large object (see `largeobj`): owned by the role that made it, by its oid
    LargeObject,
}

impl Kind {
    /// Every privilege an object of this kind can have.
    pub(crate) fn all(self) -> u32 {
        match self {
            Kind::Table | Kind::View | Kind::MatView => {
                INSERT | SELECT | UPDATE | DELETE | TRUNCATE | REFERENCES | TRIGGER
            }
            Kind::Sequence => USAGE | SELECT | UPDATE,
            Kind::Schema => USAGE | CREATE,
            Kind::Function | Kind::Procedure => EXECUTE,
            Kind::Database => CREATE | TEMP | CONNECT,
            Kind::Column => SELECT | INSERT | UPDATE | REFERENCES,
            Kind::Type | Kind::Fdw | Kind::Server => USAGE,
            Kind::LargeObject => SELECT | UPDATE,
            Kind::Extension => 0,
        }
    }

    /// The word Postgres's messages name it by.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Kind::Table => "table",
            Kind::View => "view",
            Kind::MatView => "materialized view",
            Kind::Sequence => "sequence",
            Kind::Schema => "schema",
            Kind::Function => "function",
            Kind::Procedure => "procedure",
            Kind::Database => "database",
            Kind::Column => "column",
            Kind::Type => "type",
            Kind::Extension => "extension",
            Kind::Fdw => "foreign-data wrapper",
            Kind::Server => "foreign server",
            Kind::LargeObject => "large object",
        }
    }

    /// The privileges an object has when no one granted or revoked any (Postgres's acldefault).
    pub(crate) fn defaults(self, owner: i64) -> Vec<AclItem> {
        let own = |privs| AclItem { grantee: owner, grantor: owner, privs, opts: 0 };
        let public = |privs| AclItem { grantee: PUBLIC, grantor: owner, privs, opts: 0 };
        match self {
            Kind::Function | Kind::Procedure => vec![public(EXECUTE), own(EXECUTE)],
            Kind::Database => vec![public(TEMP | CONNECT), own(CREATE | TEMP | CONNECT)],
            Kind::Type => vec![public(USAGE), own(USAGE)],
            Kind::Column | Kind::Extension => vec![],
            k => vec![own(k.all())],
        }
    }
}

/// One grant: privileges (and grant options, a subset) a grantee holds from a grantor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AclItem {
    pub grantee: i64,
    pub grantor: i64,
    pub privs: u32,
    pub opts: u32,
}

/// An object's row: its owner (None: the bootstrap superuser) and ACL (None: the default).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Entry {
    pub owner: Option<i64>,
    pub acl: Option<Vec<AclItem>>,
}

impl Entry {
    fn parse(raw: &str) -> Entry {
        let j: Json = serde_json::from_str(raw).unwrap_or_default();
        let acl = j["acl"].as_array().map(|items| {
            items
                .iter()
                .filter_map(|i| {
                    let n = |k: usize| i.get(k).and_then(Json::as_i64);
                    Some(AclItem { grantee: n(0)?, grantor: n(1)?, privs: n(2)? as u32, opts: n(3)? as u32 })
                })
                .collect()
        });
        Entry { owner: j["owner"].as_i64(), acl }
    }

    /// The row's value; None when it says nothing (the defaults).
    fn raw(&self) -> Option<String> {
        if self.owner.is_none() && self.acl.is_none() {
            return None;
        }
        let mut j = serde_json::Map::new();
        if let Some(o) = self.owner {
            j.insert("owner".into(), o.into());
        }
        if let Some(acl) = &self.acl {
            let items: Vec<Json> =
                acl.iter().map(|i| serde_json::json!([i.grantee, i.grantor, i.privs, i.opts])).collect();
            j.insert("acl".into(), items.into());
        }
        Some(Json::Object(j).to_string())
    }
}

pub(crate) fn row_key(key: &str) -> String {
    format!("{ACLS}/{key}")
}

/// The row of object `key` (see the module docs) in world `v`.
pub(crate) fn entry(v: &View, key: &str) -> Result<Entry> {
    if !v.core.has_privileges() {
        return Ok(Entry::default());
    }
    Ok(v.get(&row_key(key))?.map(|r| Entry::parse(&r)).unwrap_or_default())
}

/// Every row of world `v`, by key.
pub(crate) fn rows(v: &View) -> Result<HashMap<String, Entry>> {
    let mut out = HashMap::new();
    if !v.core.has_privileges() {
        return Ok(out);
    }
    let prefix = format!("{ACLS}/");
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        out.insert(k[prefix.len()..].to_string(), Entry::parse(&raw));
    }
    Ok(out)
}

/// The write that keeps `e` as object `key`'s row.
fn write(key: &str, e: &Entry) -> (String, Option<String>) {
    (row_key(key), e.raw())
}

/// `write`, for the objects other modules keep (see `foreign`).
pub(crate) fn write_entry(key: &str, e: &Entry) -> (String, Option<String>) {
    write(key, e)
}

/// Foreign-data wrapper or server `name` (kind `Fdw` or `Server`) of world `v`, as privileges see it: owned by the
/// role that made it, with only its owner's USAGE until a GRANT.
pub(crate) fn foreign_obj(v: &View, kind: Kind, name: &str) -> Result<Obj> {
    let key = key_of(kind, name);
    let e = entry(v, &key)?;
    Ok(Obj::new(kind, key, name.to_string(), e, BOOTSTRAP))
}

/// `foreign_obj`, from its row as the catalog holds it.
pub(crate) fn foreign_obj_of(kind: Kind, name: &str, e: Option<&Entry>) -> Obj {
    Obj::new(kind, key_of(kind, name), name.to_string(), e.cloned().unwrap_or_default(), BOOTSTRAP)
}

/// Whether `who` holds any privilege on the relation whose row is `e` and whose owner is `owner`.
pub(crate) fn rel_privileged(e: Option<&Entry>, owner: i64, who: &Who) -> bool {
    let acl = e.and_then(|e| e.acl.clone()).unwrap_or_else(|| Kind::Table.defaults(owner));
    held(who, owner, &acl, Kind::Table.all()).0 != 0
}

/// `new_owner`, for the objects other modules keep.
pub(crate) fn new_owner_acl(acl: Vec<AclItem>, old: i64, new: i64) -> Vec<AclItem> {
    new_owner(acl, old, new)
}

/// An object's key by kind and stored name.
pub(crate) fn key_of(kind: Kind, name: &str) -> String {
    match kind {
        Kind::Table | Kind::View | Kind::MatView | Kind::Sequence => format!("rel/{name}"),
        Kind::Schema => format!("schema/{name}"),
        Kind::Function | Kind::Procedure => format!("func/{name}"),
        Kind::Type => format!("type/{name}"),
        Kind::Extension => format!("ext/{name}"),
        Kind::Fdw => format!("fdw/{name}"),
        Kind::Server => format!("server/{name}"),
        Kind::LargeObject => format!("lo/{name}"),
        Kind::Database => "db".into(),
        Kind::Column => format!("col/{name}"),
    }
}

/// An ACL as Postgres writes it: `{alice=arwdDxt/owner,=r/owner}` (`*` after a letter for its grant
/// option); a role that's gone is its oid.
pub(crate) fn text(reg: &roles::Registry, acl: &[AclItem]) -> String {
    let items: Vec<String> = acl.iter().map(|i| roles::array_elem(&item_text(reg, i))).collect();
    format!("{{{}}}", items.join(","))
}

/// One ACL item, `grantee=privs/grantor` (PUBLIC's grantee empty).
pub(crate) fn item_text(reg: &roles::Registry, i: &AclItem) -> String {
    let name = |o: i64| match o {
        PUBLIC => String::new(),
        o => quote_role(&reg.name_of(o)),
    };
    let mut privs = String::new();
    for (k, l) in LETTERS.iter().enumerate() {
        if i.privs & (1 << k) != 0 {
            privs.push(*l as char);
            if i.opts & (1 << k) != 0 {
                privs.push('*');
            }
        }
    }
    format!("{}={privs}/{}", name(i.grantee), name(i.grantor))
}

/// A role's name in an ACL item: quoted unless it is all letters, digits and underscores.
fn quote_role(n: &str) -> String {
    if !n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_') {
        n.to_string()
    } else {
        format!("\"{}\"", n.replace('"', "\"\""))
    }
}

/// The privileges among `mask` that `who` holds under `acl` on an object `owner` owns, and its grant
/// options among them (Postgres's aclmask): an owner holds every grant option.
pub(crate) fn held(who: &Who, owner: i64, acl: &[AclItem], mask: u32) -> (u32, u32) {
    if who.superuser {
        return (mask, mask);
    }
    let mut opts = if who.has_privs_of(owner) { mask } else { 0 };
    let mut privs = 0;
    for i in acl.iter().filter(|i| i.grantee == PUBLIC || who.privs_of.contains(&i.grantee)) {
        privs |= i.privs & mask;
        if i.grantee != PUBLIC {
            opts |= i.opts & mask;
        }
    }
    (privs, opts)
}

/// pg_read_all_data and pg_write_all_data.
const READ_ALL_DATA: i64 = 6181;
const WRITE_ALL_DATA: i64 = 6182;

/// Which of `privs` the predefined roles give `who` on an object of kind `kind`, as Postgres 16's
/// do: pg_read_all_data SELECT on every relation, pg_write_all_data INSERT, UPDATE and DELETE on
/// every relation, and either USAGE on every schema (none with a grant option).
fn predefined(who: &Who, kind: Kind, privs: u32) -> u32 {
    let read = who.privs_of.contains(&READ_ALL_DATA);
    let write = who.privs_of.contains(&WRITE_ALL_DATA);
    let given = match kind {
        Kind::Table | Kind::View | Kind::MatView | Kind::Sequence => {
            (if read { SELECT } else { 0 }) | (if write { INSERT | UPDATE | DELETE } else { 0 })
        }
        Kind::Schema if read || write => USAGE,
        _ => 0,
    };
    given & privs
}

// ---------------------------------------------------------------- objects

/// An object privileges are checked or granted on.
#[derive(Clone, Debug)]
pub(crate) struct Obj {
    pub kind: Kind,
    /// its row's key
    pub key: String,
    /// how messages name it
    pub name: String,
    pub owner: i64,
    /// its ACL, the default if it has none
    pub acl: Vec<AclItem>,
    /// whether it has one of its own
    pub own: bool,
}

impl Obj {
    fn new(kind: Kind, key: String, name: String, e: Entry, owner: i64) -> Obj {
        let owner = e.owner.unwrap_or(owner);
        let own = e.acl.is_some();
        let acl = e.acl.unwrap_or_else(|| kind.defaults(owner));
        Obj { kind, key, name, owner, acl, own }
    }

    pub(crate) fn has(&self, who: &Who, privs: u32) -> bool {
        (held(who, self.owner, &self.acl, privs).0 | predefined(who, self.kind, privs)) == privs
    }

    pub(crate) fn denied(&self) -> SqlError {
        SqlError {
            code: "42501",
            message: format!("permission denied for {} {}", self.kind.noun(), self.name),
            info: None,
        }
    }

    /// 42501 `must be owner of ...`.
    pub(crate) fn not_owner(&self) -> SqlError {
        SqlError { code: "42501", message: format!("must be owner of {} {}", self.kind.noun(), self.name), info: None }
    }
}

/// Relation `name` (as stored) of world `v`: a table, view, materialized view or sequence; None
/// if it is none of them.
pub(crate) fn rel(v: &View, name: &str) -> Result<Option<Obj>> {
    let kind = match relation_kind(v, name)? {
        Some("view") => Kind::View,
        Some("sequence") => Kind::Sequence,
        Some(_) => match load(v, name)? {
            Some(t) if t.matview.is_some() => Kind::MatView,
            Some(_) => Kind::Table,
            None => return Ok(None),
        },
        None if sysinfo::implicit_sequence(v, name)?.is_some() => Kind::Sequence,
        None => return Ok(None),
    };
    let e = entry(v, &key_of(kind, name))?;
    // a serial's sequence is its table's owner's
    let owner = match (kind, e.owner) {
        (Kind::Sequence, None) => match sysinfo::implicit_sequence(v, name)?.and_then(|d| d.owned) {
            Some((t, _)) => entry(v, &key_of(Kind::Table, &t))?.owner.unwrap_or(BOOTSTRAP),
            None => BOOTSTRAP,
        },
        _ => BOOTSTRAP,
    };
    Ok(Some(Obj::new(kind, key_of(kind, name), rel_part(name).to_string(), e, owner)))
}

/// Relation `name`, or 42P01.
fn need_rel(v: &View, name: &str) -> Result<Obj> {
    rel(v, name)?.ok_or_else(|| SqlError {
        code: NO_TABLE,
        message: format!("relation \"{}\" does not exist", shown_name(name)),
        info: None,
    })
}

/// Schema `name`: public's ACL is Postgres 15's (`pg_database_owner` owns it, everyone may use it).
pub(crate) fn schema(v: &View, name: &str) -> Result<Obj> {
    let e = entry(v, &key_of(Kind::Schema, name))?;
    let (owner, acl) = match name {
        "public" => (DB_OWNER, Some(public_acl())),
        "pg_catalog" | "information_schema" => {
            (BOOTSTRAP, Some(vec![AclItem { grantee: PUBLIC, grantor: BOOTSTRAP, privs: USAGE, opts: 0 }]))
        }
        _ => (BOOTSTRAP, None),
    };
    let mut o = Obj::new(Kind::Schema, key_of(Kind::Schema, name), name.to_string(), e.clone(), owner);
    if e.acl.is_none()
        && let Some(mut acl) = acl
    {
        if name != "public" {
            acl.insert(0, AclItem { grantee: o.owner, grantor: o.owner, privs: USAGE | CREATE, opts: 0 });
        }
        o.acl = acl;
        o.own = true;
    }
    Ok(o)
}

/// public's ACL until someone changes it.
fn public_acl() -> Vec<AclItem> {
    vec![
        AclItem { grantee: DB_OWNER, grantor: DB_OWNER, privs: USAGE | CREATE, opts: 0 },
        AclItem { grantee: PUBLIC, grantor: DB_OWNER, privs: USAGE, opts: 0 },
    ]
}

/// Function `f`.
pub(crate) fn func(v: &View, f: &functions::Function) -> Result<Obj> {
    let kind = if f.procedure { Kind::Procedure } else { Kind::Function };
    let key = key_of(kind, &f.sig());
    let e = entry(v, &key)?;
    Ok(Obj::new(kind, key, rel_part(&f.name).to_string(), e, BOOTSTRAP))
}

/// User aggregate `key` (its row's, see `userdefs::agg_key`), named `name`, as privileges see it: EXECUTE, as a
/// function's (`func/<name>(<types>)`), owned by the role that made it.
pub(crate) fn aggregate(v: &View, key: &str, name: &str) -> Result<Obj> {
    let k = key_of(Kind::Function, userdefs::agg_sig(key));
    let e = entry(v, &k)?;
    let owner = userdefs::agg_owner(v, key)?;
    Ok(Obj::new(Kind::Function, k, rel_part(name).to_string(), e, owner))
}

/// Type or domain `name` (as stored), named as Postgres's messages name it (with its schema where
/// the search_path doesn't find it); None if there is none.
fn type_obj(v: &View, name: &str) -> Result<Option<Obj>> {
    if v.get(&type_key(name))?.is_none() {
        return Ok(None);
    }
    let key = key_of(Kind::Type, name);
    let e = entry(v, &key)?;
    Ok(Some(Obj::new(Kind::Type, key, shown_type(name), e, BOOTSTRAP)))
}

/// Extension `name`, as CREATE EXTENSION recorded it, owned by the role that made it; None if it
/// didn't.
fn extension(v: &View, name: &str) -> Result<Option<Obj>> {
    if v.get(&pgcrypto::key(name))?.is_none() {
        return Ok(None);
    }
    let key = key_of(Kind::Extension, name);
    let e = entry(v, &key)?;
    Ok(Some(Obj::new(Kind::Extension, key, name.to_string(), e, BOOTSTRAP)))
}

/// Large object `oid` (whether it exists is the caller's to say), owned by the role that made it: only its
/// owner may read or write it until a GRANT (Postgres's acldefault for large objects).
pub(crate) fn large_object(v: &View, oid: i64) -> Result<Obj> {
    let key = key_of(Kind::LargeObject, &oid.to_string());
    let e = entry(v, &key)?;
    Ok(Obj::new(Kind::LargeObject, key, oid.to_string(), e, BOOTSTRAP))
}

/// The world itself, as a database.
pub(crate) fn database(v: &View, name: &str) -> Result<Obj> {
    Ok(Obj::new(Kind::Database, "db".into(), name.to_string(), entry(v, "db")?, BOOTSTRAP))
}

/// CREATE on the database the statement runs in, as CREATE PUBLICATION asks it: 42501 `permission denied for
/// database ...` without it.
pub(super) fn need_create_on_database(v: &View, who: &Who) -> Result<()> {
    let d = database(v, &database_name(v))?;
    if d.has(who, CREATE) { Ok(()) } else { Err(d.denied()) }
}

/// The database (world) a statement runs in, as messages name it: the session's world, not its
/// transaction's.
fn database_name(v: &View) -> String {
    STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| c.home.clone())))
        .unwrap_or_else(|| v.branch.clone())
}

/// Column `col` of relation `table` (its ACL: none by default).
fn column(v: &View, table: &str, col: &str) -> Result<Obj> {
    let key = key_of(Kind::Column, &format!("{table}/{col}"));
    let e = entry(v, &key)?;
    Ok(Obj {
        kind: Kind::Column,
        key,
        name: col.into(),
        owner: BOOTSTRAP,
        own: e.acl.is_some(),
        acl: e.acl.unwrap_or_default(),
    })
}

/// Whether `who` holds `privilege` on column `col` of relation `table` (owned by `owner`) by the column's own ACL.
pub(crate) fn column_has(v: &View, table: &str, col: &str, who: &Who, owner: i64, privilege: u32) -> Result<bool> {
    Ok(held(who, owner, &column(v, table, col)?.acl, privilege).0 == privilege)
}

/// The columns of relation `name` and their ACLs (one with none: empty).
fn columns(v: &View, name: &str) -> Result<Vec<(String, Vec<AclItem>)>> {
    let names: Vec<String> = match view_def(v, name)? {
        Some(d) => d.cols,
        // (a catalog table's, as Postgres has them)
        None if name.strip_prefix("pg_catalog.").is_some_and(catalog_table) => {
            super::catalog_schema::columns(&name["pg_catalog.".len()..]).into_iter().map(|(n, _)| n).collect()
        }
        None => match load(v, name)? {
            Some(t) => t.cols.iter().filter(|c| !c.hidden).map(|c| c.name.clone()).collect(),
            None => vec!["last_value".into(), "log_cnt".into(), "is_called".into()],
        },
    };
    let mut out = vec![];
    for n in names {
        let acl = column(v, name, &n)?.acl;
        out.push((n, acl));
    }
    Ok(out)
}

// ---------------------------------------------------------------- checks

/// What a statement needs of one relation: per privilege, whether at all, and on which columns
/// (none: any column will do, as for `count(*)`).
#[derive(Default, Debug)]
struct Need {
    select: Option<BTreeSet<String>>,
    insert: Option<BTreeSet<String>>,
    update: Option<BTreeSet<String>>,
    references: Option<BTreeSet<String>>,
    /// table-wide ones: DELETE, TRUNCATE, TRIGGER
    whole: u32,
}

/// A FROM item in scope: what it's called, its stored relation (if it is one) and its columns.
struct Src {
    alias: String,
    rel: Option<String>,
    cols: Vec<String>,
}

/// One query level's FROM items.
type Level = Vec<Src>;

/// Works out what a statement needs, as one role: relations in the order Postgres's range table
/// has them, the schemas their names are in, and the views to check as their owners.
struct Checker<'a, 'v> {
    v: &'a View<'v>,
    needs: Vec<(String, Need)>,
    schemas: Vec<String>,
    /// views read: checked after, as their owners
    views: Vec<String>,
    /// views written: (view, the privilege)
    through: Vec<(String, u32)>,
    /// relations' columns, looked up once
    cols: HashMap<String, Option<Vec<String>>>,
    depth: usize,
    /// for a superuser: only the views it reads matter (their owners' privileges are checked)
    views_only: bool,
    /// a catalog table only a superuser may read, if the statement reads one
    catalog_secrets: Option<String>,
    /// the user aggregates it calls (their places in the registry, see `userdefs::aggregate_at`): EXECUTE
    aggs: Vec<u8>,
    /// a whole row (`select t from t`) reads every column of its relation: for privileges it does, but what depends
    /// on it depends on the relation, not on each column (see `columns_used`)
    whole_rows: bool,
}

impl<'a, 'v> Checker<'a, 'v> {
    fn new(v: &'a View<'v>, depth: usize, views_only: bool) -> Checker<'a, 'v> {
        Checker {
            v,
            needs: vec![],
            schemas: vec![],
            views: vec![],
            through: vec![],
            cols: HashMap::new(),
            depth,
            views_only,
            catalog_secrets: None,
            aggs: vec![],
            whole_rows: true,
        }
    }

    fn need(&mut self, rel: &str) -> &mut Need {
        let at = match self.needs.iter().position(|(r, _)| r == rel) {
            Some(i) => i,
            None => {
                self.needs.push((rel.to_string(), Need::default()));
                self.needs.len() - 1
            }
        };
        &mut self.needs[at].1
    }

    /// The columns of `name` if it is a relation of the world (not a catalog's, a temporary
    /// table, or a name the statement made).
    fn relation(&mut self, name: &str) -> Result<Option<Vec<String>>> {
        self.relation_in(name, false)
    }

    /// `relation`, of what source `s` reads: what a policy reads (`Source::bound`) is the world's own relation,
    /// whatever temporary table has its name.
    fn relation_of(&mut self, s: &Source) -> Result<Option<Vec<String>>> {
        self.relation_in(&s.table, s.bound && has_temp(&s.table))
    }

    fn relation_in(&mut self, name: &str, world: bool) -> Result<Option<Vec<String>>> {
        if let Some(c) = self.cols.get(name).filter(|_| !world) {
            return Ok(c.clone());
        }
        // (the catalog's tables aren't stored ones: anyone may read them, but pg_authid and pg_shadow,
        // which hold passwords)
        let found = if name.starts_with("_sonos") {
            None
        } else if let Some(d) = view_def(self.v, name)? {
            Some(d.cols)
        } else if self.views_only {
            None
        } else if let Some(t) = if world { load_world(self.v, name)? } else { load(self.v, name)? } {
            Some(t.cols.iter().filter(|c| !c.hidden).map(|c| c.name.clone()).collect())
        } else if seq_def(self.v, name)?.is_some() || sysinfo::implicit_sequence(self.v, name)?.is_some() {
            Some(vec!["last_value".into(), "log_cnt".into(), "is_called".into()])
        } else {
            None
        };
        if !world {
            self.cols.insert(name.to_string(), found.clone());
        }
        Ok(found)
    }

    /// A write to one of the catalog's tables (`pg_catalog.pg_class`, a `pg_toast` table): no role but a superuser
    /// holds a privilege to write them, so it is refused as Postgres refuses it (`permission denied for table
    /// pg_class`).
    fn writes_catalog(&mut self, table: &str) -> Result<()> {
        let bare = table.strip_prefix("pg_catalog.").unwrap_or(table);
        if (catalog_table(bare) || table.starts_with("pg_toast.")) && self.relation(table)?.is_none() {
            self.catalog_secrets.get_or_insert_with(|| rel_part(table).to_string());
        }
        Ok(())
    }

    /// A relation the statement names: its schema needs USAGE, and a view is checked as its owner too.
    fn named(&mut self, rel: &str) -> Result<()> {
        let s = schema_part(rel).to_string();
        if !self.schemas.contains(&s) {
            self.schemas.push(s);
        }
        if view_def(self.v, rel)?.is_some() && !self.views.contains(&rel.to_string()) {
            self.views.push(rel.to_string());
        }
        Ok(())
    }

    fn stmt(&mut self, st: &Stmt) -> Result<()> {
        match st {
            Stmt::Select(q) => self.select(q, &mut vec![]),
            Stmt::Locking(q, clauses) => {
                self.select(q, &mut vec![])?;
                // FOR UPDATE and its kin need UPDATE on some column of each table they lock
                let tables: Vec<String> = q
                    .from
                    .iter()
                    .chain(q.joins.iter().map(|j| &j.src))
                    .filter(|s| s.sub.is_none() && s.srf.is_none())
                    .map(|s| s.table.clone())
                    .collect();
                for (_, of, _) in clauses {
                    for t in tables.iter().filter(|t| of.is_empty() || of.iter().any(|o| o == *t || o == rel_part(t))) {
                        if self.relation(t)?.is_some() {
                            self.need(t).update.get_or_insert_with(Default::default);
                        }
                    }
                }
                Ok(())
            }
            Stmt::Explain { stmt, .. } | Stmt::Declare { query: stmt, .. } => self.stmt(stmt),
            Stmt::With(w) => {
                for s in w.dml.iter().map(|(_, s)| s).chain([&w.main]) {
                    self.stmt(s)?;
                }
                Ok(())
            }
            Stmt::Insert { table, cols, rows, query, conflict, returning, .. } => {
                self.writes_catalog(table)?;
                // (a relation needs nothing when it is no stored one, or a superuser writes it: what
                // it reads still may)
                let target = self.relation(table)?;
                let all = target.clone().unwrap_or_default();
                // (CREATE TABLE AS filling the table it made: its maker needs no INSERT on it, as in Postgres)
                if target.is_some() && !FILLING.with_borrow(|f| f.as_deref() == Some(table.as_str())) {
                    self.named(table)?;
                    let width = match (cols, query) {
                        (Some(c), _) => c.len(),
                        (None, Some(q)) => output_names(self, q)?.len(),
                        (None, None) => rows.iter().map(Vec::len).max().unwrap_or(0),
                    };
                    let inserted: BTreeSet<String> = match cols {
                        Some(c) => c.iter().cloned().collect(),
                        None => all.iter().take(width).cloned().collect(),
                    };
                    self.need(table).insert.get_or_insert_with(Default::default).extend(inserted);
                    self.writes_view(table, INSERT)?;
                    // (a view only written, by a plain INSERT, isn't read: its table needs INSERT, not SELECT, as
                    // Postgres asks; RETURNING and ON CONFLICT read it)
                    if returning.is_none() && conflict.is_none() {
                        let read_elsewhere = query.as_ref().is_some_and(|q| {
                            let mut names = vec![];
                            super::select_tables(q, &mut names);
                            names.iter().any(|n| n == table)
                        });
                        if !read_elsewhere {
                            self.views.retain(|x| x != table);
                        }
                    }
                }
                for e in rows.iter().flatten() {
                    self.expr(e, &mut vec![])?;
                }
                if let Some(q) = query {
                    self.select(q, &mut vec![])?;
                }
                let mut level = vec![self.target(table, target.as_deref())];
                if let Some(OnConflict { update: Some((sets, filter)), .. }) = conflict {
                    if target.is_some() {
                        let set: BTreeSet<String> = sets.iter().map(|(c, _)| c.clone()).collect();
                        self.need(table).update.get_or_insert_with(Default::default).extend(set);
                    }
                    let mut levels = vec![vec![
                        self.target(table, target.as_deref()),
                        Src { alias: "excluded".into(), rel: None, cols: all.clone() },
                    ]];
                    for e in sets.iter().map(|(_, e)| e).chain(filter) {
                        self.expr(e, &mut levels)?;
                    }
                }
                if let Some(items) = returning {
                    self.items(items, &mut level)?;
                }
                Ok(())
            }
            Stmt::Update { table, sets, filter, returning, .. } => {
                self.writes_catalog(table)?;
                let target = self.relation(table)?;
                if target.is_some() {
                    self.named(table)?;
                    let set: BTreeSet<String> = sets.iter().map(|(c, _)| c.clone()).collect();
                    self.need(table).update.get_or_insert_with(Default::default).extend(set);
                    self.writes_view(table, UPDATE)?;
                }
                let mut levels = vec![vec![self.target(table, target.as_deref())]];
                for e in sets.iter().map(|(_, e)| e).chain(filter) {
                    self.expr(e, &mut levels)?;
                }
                if let Some(items) = returning {
                    self.items(items, &mut levels[0])?;
                }
                Ok(())
            }
            Stmt::Delete { table, filter, returning, .. } => {
                self.writes_catalog(table)?;
                let target = self.relation(table)?;
                if target.is_some() {
                    self.named(table)?;
                    self.need(table).whole |= DELETE;
                    self.writes_view(table, DELETE)?;
                }
                let mut levels = vec![vec![self.target(table, target.as_deref())]];
                if let Some(e) = filter {
                    self.expr(e, &mut levels)?;
                }
                if let Some(items) = returning {
                    self.items(items, &mut levels[0])?;
                }
                Ok(())
            }
            Stmt::Merge(m) => {
                let target = self.relation(&m.table)?;
                if target.is_some() {
                    self.named(&m.table)?;
                    self.need(&m.table).select.get_or_insert_with(Default::default);
                    for w in &m.whens {
                        match &w.action {
                            MergeAction::Update(_, cols) => self
                                .need(&m.table)
                                .update
                                .get_or_insert_with(Default::default)
                                .extend(cols.iter().cloned()),
                            MergeAction::Insert(cols, values) => {
                                // without a column list, the first columns, as many as VALUES has
                                let cols = match cols {
                                    Some((_, c)) => c.clone(),
                                    None => {
                                        let width = merge_values_width(values);
                                        target.iter().flatten().take(width).cloned().collect()
                                    }
                                };
                                self.need(&m.table).insert.get_or_insert_with(Default::default).extend(cols)
                            }
                            MergeAction::Delete => self.need(&m.table).whole |= DELETE,
                            MergeAction::Nothing => {}
                        }
                    }
                }
                // what it reads: USING's table or subquery, and the columns ON, the WHEN conditions and
                // the SET and VALUES expressions read of either side (`exprs` holds them all, and
                // USING's subquery)
                let source = match lex(&format!("select 1 from {}", m.source)).map(Parser::new) {
                    Ok(mut p) => match p.stmt() {
                        Ok(Stmt::Select(q)) => Some(q),
                        _ => None,
                    },
                    Err(_) => None,
                };
                let mut level = vec![self.target(&m.table, target.as_deref())];
                level[0].alias = m.alias.clone();
                if let Some(q) = &source {
                    self.select(q, &mut vec![])?;
                    if let Some(s) = &q.from {
                        let stored = s.sub.is_none() && s.srf.is_none();
                        let cols = match (&s.sub, stored) {
                            (_, true) => self.relation(&s.table)?,
                            (Some(sub), _) => Some(output_names(self, sub)?),
                            _ => None,
                        };
                        let rel = (stored && cols.is_some()).then(|| s.table.clone());
                        level.push(Src { alias: m.source_alias.clone(), rel, cols: cols.unwrap_or_default() });
                    }
                }
                let mut levels = vec![level];
                // (the second is USING's subquery, when it is one: checked above)
                let sub = source.as_ref().and_then(|q| q.from.as_ref()).is_some_and(|s| s.sub.is_some());
                for (i, e) in m.exprs.iter().enumerate() {
                    if !(sub && i == 1) {
                        self.expr(e, &mut levels)?;
                    }
                }
                Ok(())
            }
            Stmt::Copy(c) => {
                if let Some(q) = &c.query {
                    return self.select(q, &mut vec![]);
                }
                let Some(t) = &c.table else { return Ok(()) };
                let Some(all) = self.relation(t)? else { return Ok(()) };
                self.named(t)?;
                let cols: BTreeSet<String> = c.cols.clone().unwrap_or(all).into_iter().collect();
                let need = self.need(t);
                if c.from {
                    need.insert.get_or_insert_with(Default::default).extend(cols);
                } else {
                    need.select.get_or_insert_with(Default::default).extend(cols);
                }
                Ok(())
            }
            Stmt::Truncate { tables, .. } => {
                for t in tables {
                    self.writes_catalog(t)?;
                    if self.relation(t)?.is_some() {
                        self.named(t)?;
                        self.need(t).whole |= TRUNCATE;
                    }
                }
                Ok(())
            }
            Stmt::CreateAs { query, .. } => self.select(query, &mut vec![]),
            // a materialized view's rows are its query's, read as it is made (WITH NO DATA reads nothing)
            Stmt::CreateView { query, materialized: true, data: true, .. } => self.select(query, &mut vec![]),
            Stmt::Do(q) | Stmt::Call(q) => self.select(q, &mut vec![]),
            // UPDATE pg_class SET reltuples ... (or a MERGE into it): it changes nothing here (see
            // `Stmt::PlannerStats`), but is a write to the catalog all the same, refused to all but a
            // superuser as Postgres refuses it; its WHERE reads what the query it runs reads
            Stmt::PlannerStats(sql) => {
                self.writes_catalog("pg_catalog.pg_class")?;
                for st in super::parse(sql)? {
                    self.stmt(&st.0)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The FROM item a statement's target is (with its columns, if it is a stored relation).
    fn target(&self, table: &str, cols: Option<&[String]>) -> Src {
        let rel = cols.map(|_| table.to_string());
        Src { alias: rel_part(table).to_string(), rel, cols: cols.unwrap_or_default().to_vec() }
    }

    /// A write to a view is a write to its table by the view's owner.
    fn writes_view(&mut self, name: &str, privilege: u32) -> Result<()> {
        if view_def(self.v, name)?.is_some() {
            self.through.push((name.to_string(), privilege));
        }
        Ok(())
    }

    /// RETURNING's (or a SELECT list's) items, over `level`.
    fn items(&mut self, items: &[Item], level: &mut Level) -> Result<()> {
        let mut levels = vec![std::mem::take(level)];
        let out = self.items_in(items, &mut levels);
        *level = levels.pop().unwrap_or_default();
        out
    }

    fn items_in(&mut self, items: &[Item], levels: &mut Vec<Level>) -> Result<()> {
        for i in items {
            match i {
                Item::Star => {
                    let all: Vec<(String, Vec<String>)> = levels
                        .last()
                        .into_iter()
                        .flatten()
                        .filter_map(|s| s.rel.clone().map(|r| (r, s.cols.clone())))
                        .collect();
                    for (r, cols) in all {
                        self.need(&r).select.get_or_insert_with(Default::default).extend(cols);
                    }
                }
                Item::TableStar(a) => {
                    let all: Vec<(String, Vec<String>)> = levels
                        .last()
                        .into_iter()
                        .flatten()
                        .filter(|s| s.alias == *a || s.rel.as_deref() == Some(a))
                        .filter_map(|s| s.rel.clone().map(|r| (r, s.cols.clone())))
                        .collect();
                    for (r, cols) in all {
                        self.need(&r).select.get_or_insert_with(Default::default).extend(cols);
                    }
                }
                Item::Expr(e, _) => self.expr(e, levels)?,
            }
        }
        Ok(())
    }

    fn select(&mut self, q: &Select, outer: &mut Vec<Level>) -> Result<()> {
        if self.depth > MAX_DEPTH {
            return err("54001", "stack depth limit exceeded: queries nest too deep");
        }
        let sources: Vec<&Source> = q.from.iter().chain(q.joins.iter().map(|j| &j.src)).collect();
        // FOR UPDATE and its kin in a subquery or a WITH query: UPDATE on some column of each table they lock, as a
        // statement's own need (see `stmt`)
        for (_, of, _) in &q.locking {
            for s in sources.iter().filter(|s| s.sub.is_none() && s.srf.is_none()) {
                let t = &s.table;
                if (of.is_empty() || of.iter().any(|o| o == t || o == rel_part(t))) && self.relation(t)?.is_some() {
                    self.need(t).update.get_or_insert_with(Default::default);
                }
            }
        }
        // the stored relations first, as Postgres's range table has them
        let mut level: Level = vec![];
        for s in &sources {
            let stored = s.sub.is_none() && s.rec.is_none() && s.srf.is_none() && !s.working;
            let cols = match stored {
                true => self.relation_of(s)?,
                false => None,
            };
            // (and pg_user_mapping, which holds the options user mappings keep: passwords among them; and
            // pg_largeobject, whose rows are the large objects' bytes, which their own privileges guard)
            if stored
                && cols.is_none()
                && matches!(s.table.as_str(), "pg_authid" | "pg_shadow" | "pg_user_mapping" | "pg_largeobject")
            {
                self.catalog_secrets = Some(s.table.clone());
            }
            match cols {
                Some(cols) => {
                    self.named(&s.table)?;
                    self.need(&s.table).select.get_or_insert_with(Default::default);
                    level.push(Src { alias: s.alias.clone(), rel: Some(s.table.clone()), cols });
                }
                None => {
                    let cols = match (&s.sub, &s.rec) {
                        _ if !s.names.is_empty() => s.names.clone(),
                        (Some(sub), _) => output_names(self, sub)?,
                        (_, Some(r)) => output_names(self, &r.base)?,
                        _ => vec![s.alias.clone()],
                    };
                    level.push(Src { alias: s.alias.clone(), rel: None, cols });
                }
            }
        }
        self.depth += 1;
        for (i, s) in sources.iter().enumerate() {
            if let Some(sub) = &s.sub {
                match s.lateral {
                    true => {
                        outer.push(
                            level[..i]
                                .iter()
                                .map(|x| Src { alias: x.alias.clone(), rel: x.rel.clone(), cols: x.cols.clone() })
                                .collect(),
                        );
                        let out = self.select(sub, outer);
                        outer.pop();
                        out?;
                    }
                    false => self.select(sub, outer)?,
                }
            }
            if let Some(r) = &s.rec {
                self.select(&r.base, outer)?;
                self.select(&r.step, outer)?;
            }
            outer.push(
                level[..i]
                    .iter()
                    .map(|x| Src { alias: x.alias.clone(), rel: x.rel.clone(), cols: x.cols.clone() })
                    .collect(),
            );
            let args: Vec<&Expr> = s.srf.iter().flat_map(|f| &f.1).chain(&s.at).collect();
            let mut out = Ok(());
            for e in args {
                out = out.and_then(|_| self.expr(e, outer));
            }
            outer.pop();
            out?;
        }
        outer.push(level);
        let aliases: Vec<String> = q
            .items
            .iter()
            .filter_map(|i| if let Item::Expr(_, Some(a)) = i { Some(a.clone()) } else { None })
            .collect();
        let out = (|| {
            self.items_in(&q.items, outer)?;
            for j in &q.joins {
                for u in &j.using {
                    self.column(None, u, outer, false);
                }
                if let Some(e) = &j.on {
                    self.expr(e, outer)?;
                }
            }
            for e in q.filter.iter().chain(&q.having).chain(&q.distinct_on).chain(&q.limit).chain(&q.offset) {
                self.expr(e, outer)?;
            }
            // ORDER BY and GROUP BY may name an output column
            for e in q.group.iter().chain(q.order.iter().map(|(e, _)| e)) {
                match e {
                    Expr::Col(None, n) if aliases.contains(n) => {}
                    e => self.expr(e, outer)?,
                }
            }
            Ok(())
        })();
        outer.pop();
        for (_, _, p) in &q.set {
            self.select(p, outer)?;
        }
        self.depth -= 1;
        out
    }

    /// The columns expression `e` reads (and the queries in it).
    fn expr(&mut self, e: &Expr, levels: &mut Vec<Level>) -> Result<()> {
        let mut found: Vec<(Option<String>, String)> = vec![];
        let mut subs: Vec<Select> = vec![];
        let mut aggs = vec![];
        walk(e, &mut |x| match x {
            Expr::Col(q, n) => found.push((q.clone(), n.clone())),
            Expr::Sub(sq) | Expr::Exists(sq, _) | Expr::InSub(_, sq, _) => subs.push((*sq.q).clone()),
            Expr::Agg(Agg::List(ListAgg::User(i), _), ..) => aggs.push(*i),
            Expr::Window(w) => {
                if let WinFn::Agg(Agg::List(ListAgg::User(i), _)) = &w.func {
                    aggs.push(*i);
                }
            }
            _ => {}
        });
        for i in aggs {
            if !self.aggs.contains(&i) {
                self.aggs.push(i);
            }
        }
        for (q, n) in found {
            self.column(q.as_deref(), &n, levels, true);
        }
        for s in &subs {
            self.select(s, levels)?;
        }
        Ok(())
    }

    /// Column `n` (of `q`, if qualified): the nearest FROM item with it. A bare name that is a FROM
    /// item's alias (and no column's) is its whole row.
    fn column(&mut self, q: Option<&str>, n: &str, levels: &[Level], whole: bool) {
        for level in levels.iter().rev() {
            let hit: Vec<(String, Vec<String>)> = match q {
                Some(q) => match level.iter().find(|s| s.alias == q || s.rel.as_deref() == Some(q)) {
                    Some(s) => match &s.rel {
                        Some(r) => vec![(r.clone(), vec![n.to_string()])],
                        None => return,
                    },
                    None => continue,
                },
                None => {
                    let with: Vec<&Src> = level.iter().filter(|s| s.cols.iter().any(|c| c == n)).collect();
                    if with.is_empty() {
                        match level.iter().find(|s| whole && s.alias == n) {
                            Some(s) => match &s.rel {
                                Some(r) if !self.whole_rows => vec![(r.clone(), vec![])],
                                Some(r) => vec![(r.clone(), s.cols.clone())],
                                None => return,
                            },
                            None => continue,
                        }
                    } else {
                        with.iter().filter_map(|s| s.rel.clone().map(|r| (r, vec![n.to_string()]))).collect()
                    }
                }
            };
            for (r, cols) in hit {
                self.need(&r).select.get_or_insert_with(Default::default).extend(cols);
            }
            return;
        }
    }

    /// Checks what was found, as `who`; then the views read and written, as their owners (whoever
    /// reads a view, a superuser too).
    fn finish(self, who: &Who) -> Result<()> {
        let v = self.v;
        if !who.superuser {
            self.needs_met(who)?;
        }
        let mut seen = vec![];
        for view in &self.views {
            let Some(d) = view_def(v, view)? else { continue };
            if seen.contains(view) {
                continue;
            }
            seen.push(view.clone());
            // (a security_invoker view's tables are read as whoever reads it)
            let owner = if d.invoker { who.clone() } else { who.as_role(need_rel(v, view)?.owner) };
            let q = view_query(view, &d.sql)?;
            let mut c = Checker::new(v, self.depth + 1, owner.superuser);
            c.select(&q, &mut vec![])?;
            c.schemas.clear(); // (its names were looked up when it was made)
            c.finish(&owner)?;
        }
        for (view, privilege) in &self.through {
            let Some(d) = view_def(v, view)? else { continue };
            let owner = if d.invoker { who.clone() } else { who.as_role(need_rel(v, view)?.owner) };
            let q = view_query(view, &d.sql)?;
            if let Some(base) = q.from.as_ref().filter(|s| s.sub.is_none() && s.srf.is_none())
                && let Some(t) = self::rel(v, &base.table)?
                && !t.has(&owner, *privilege)
            {
                return Err(t.denied());
            }
        }
        Ok(())
    }

    /// The schemas, relations and columns found: `who` must have what each needs.
    fn needs_met(&self, who: &Who) -> Result<()> {
        let v = self.v;
        if let Some(t) = &self.catalog_secrets {
            return err("42501", format!("permission denied for table {t}"));
        }
        for s in &self.schemas {
            if !schema(v, s)?.has(who, USAGE) {
                return Err(schema(v, s)?.denied());
            }
        }
        for (rel, need) in &self.needs {
            let Some(obj) = self::rel(v, rel)? else { continue };
            let mut cols: Option<Vec<(String, Vec<AclItem>)>> = None;
            for (privilege, on) in
                [(SELECT, &need.select), (INSERT, &need.insert), (UPDATE, &need.update), (REFERENCES, &need.references)]
            {
                let Some(on) = on else { continue };
                if obj.has(who, privilege) {
                    continue;
                }
                // without it on the table, then on each column the statement uses (or any one, if none)
                if obj.kind == Kind::Sequence {
                    return Err(obj.denied());
                }
                let all = match &cols {
                    Some(c) => c,
                    None => cols.insert(columns(v, rel)?),
                };
                let ok = |acl: &Vec<AclItem>| held(who, obj.owner, acl, privilege).0 == privilege;
                let fine = match on.is_empty() {
                    true => all.iter().any(|(_, acl)| ok(acl)),
                    false => on.iter().all(|c| all.iter().find(|(n, _)| n == c).is_some_and(|(_, acl)| ok(acl))),
                };
                if !fine {
                    return Err(obj.denied());
                }
            }
            if need.whole != 0 && !obj.has(who, need.whole) {
                return Err(obj.denied());
            }
        }
        // the user aggregates it calls: EXECUTE on each (as Postgres's ExecInitAgg checks it)
        for &i in &self.aggs {
            let Some(d) = userdefs::aggregate_at(i) else { continue };
            // (the registry holds every database's: only one of this world counts)
            if v.get(&d.key)?.is_none() {
                continue;
            }
            if !aggregate(v, &d.key, &d.name)?.has(who, EXECUTE) {
                return err("42501", format!("permission denied for aggregate {}", rel_part(&d.name)));
            }
        }
        Ok(())
    }
}

/// The columns of each relation statement `st` reads or writes (a `*` stands for the columns its relation has now),
/// as Postgres records what a view's query or a SQL-standard function body depends on: by relation, in the order
/// they are first found.
pub(crate) fn columns_used(v: &View, st: &Stmt) -> Result<Vec<(String, BTreeSet<String>)>> {
    let mut c = Checker::new(v, 0, false);
    c.whole_rows = false;
    c.stmt(st)?;
    Ok(c.needs
        .into_iter()
        .map(|(r, n)| {
            let mut all = BTreeSet::new();
            for set in [n.select, n.insert, n.update, n.references].into_iter().flatten() {
                all.extend(set);
            }
            (r, all)
        })
        .collect())
}

/// The columns of each relation statement `st` reads (SELECT's need: a whole row is all its columns), by
/// relation; one it reads no column of (`count(*)`) has none.
pub(crate) fn columns_read(v: &View, st: &Stmt) -> Result<Vec<(String, BTreeSet<String>)>> {
    let mut c = Checker::new(v, 0, false);
    c.stmt(st)?;
    Ok(c.needs.into_iter().filter_map(|(r, n)| Some((r, n.select?))).collect())
}

/// The names of a query's columns, as they come out of it.
fn output_names(c: &mut Checker, q: &Select) -> Result<Vec<String>> {
    let mut out = vec![];
    let sources: Vec<&Source> = q.from.iter().chain(q.joins.iter().map(|j| &j.src)).collect();
    let cols_of = |c: &mut Checker, s: &Source| -> Result<Vec<String>> {
        if !s.names.is_empty() {
            return Ok(s.names.clone());
        }
        match (&s.sub, s.srf.is_some()) {
            (Some(sub), _) => output_names(c, sub),
            (None, true) => Ok(vec![s.alias.clone()]),
            (None, false) => Ok(c.relation_of(s)?.unwrap_or_default()),
        }
    };
    for i in &q.items {
        match i {
            Item::Star => {
                for s in &sources {
                    out.extend(cols_of(c, s)?);
                }
            }
            Item::TableStar(a) => {
                for s in sources.iter().filter(|s| s.alias == *a || s.table == *a) {
                    out.extend(cols_of(c, s)?);
                }
            }
            Item::Expr(e, alias) => {
                out.push(alias.clone().or_else(|| figure_name(e)).unwrap_or_else(|| "?column?".into()))
            }
        }
    }
    Ok(out)
}

/// Checks that `who` may run statement `st` in world `v` (the relations and columns it reads and
/// writes; see the module docs). Statements that make, drop or change objects are checked by
/// `ddl`; this is what reads and writes rows.
pub(crate) fn check_as(v: &View, who: &Who, st: &Stmt) -> Result<()> {
    if !who.superuser {
        ddl(v, who, st)?;
    }
    // REFRESH reads a materialized view's query as its owner, whoever runs it
    if let Stmt::Refresh { name, data: true, .. } = st
        && let Some(o) = rel(v, name)?
        && let Some(sql) = load(v, name)?.and_then(|t| t.matview).and_then(|m| m["sql"].as_str().map(String::from))
    {
        let owner = who.as_role(o.owner);
        let mut c = Checker::new(v, 0, owner.superuser);
        c.select(&view_query(name, &sql)?, &mut vec![])?;
        c.schemas.clear(); // (its names were looked up when it was made)
        c.finish(&owner)?;
    }
    let mut c = Checker::new(v, 0, who.superuser);
    c.stmt(st)?;
    c.finish(who)
}

/// Whether statement `st` reads its target `table`: a WHERE, SET or RETURNING that names its columns,
/// which takes SELECT on it, and its SELECT policies too (see `rls`).
pub(crate) fn reads_target(v: &View, st: &Stmt, table: &str) -> Result<bool> {
    let mut c = Checker::new(v, 0, false);
    c.stmt(st)?;
    Ok(c.needs.iter().any(|(r, n)| r == table && n.select.is_some()))
}

/// `check_as` for the running statement's role. A superuser's statements are checked only for the
/// views they read, whose owners may not be superusers; in a database that never had a role or a
/// grant, nothing is.
pub(crate) fn check(db: &Db, branch: &str, st: &Stmt) -> Result<()> {
    if !db.core.has_privileges() {
        return Ok(());
    }
    // an agent's session (the guest's too) logged in with a token, not as a superuser role:
    // whatever its login's role says, it may not read the catalog's passwords
    if agents() {
        let v = db.view_tx(branch)?;
        let mut c = Checker::new(&v, 0, true);
        c.stmt(st)?;
        if let Some(t) = &c.catalog_secrets {
            return err("42501", format!("permission denied for table {t}"));
        }
    }
    let Some(who) = roles::current() else { return Ok(()) };
    // (a superuser's only checks are the views it reads)
    if who.superuser && !db.has_views() && !branch.contains('@') && !matches!(st, Stmt::Refresh { .. }) {
        return Ok(());
    }
    // (a world that is gone has nothing to check: a statement that needs it fails as it runs, and
    // one that doesn't, UNDO MERGE of it from a session left in it, goes on)
    let Ok(v) = db.view_tx(branch) else {
        return match who.superuser {
            true => Ok(()),
            false => only_superusers(st),
        };
    };
    check_as(&v, &who, st)?;
    // a world as it was (`main@...`): the privileges its objects have now apply too (both must allow), as a read AS
    // OF gets the policies a table has now as well as those it had: a REVOKE holds for the past. A relation with no
    // relation of its name now (renamed, moved or dropped since: a name is all that says which it is) has no
    // privileges now to check, so only a superuser reads it there
    if !who.superuser
        && let Some((now, _)) = branch.split_once('@')
    {
        // (a world dropped since has no relations now, nor privileges to check)
        let Ok(n) = db.view_tx(now) else {
            return Err(SqlError {
                code: "42501",
                message: format!("permission denied for world {now}"),
                info: Some(Box::new(Violation {
                    detail: Some(format!(
                        "World \"{now}\" isn't there now, so the privileges its relations have now can't be \
                         checked: only a superuser reads its past."
                    )),
                    ..Violation::default()
                })),
            });
        };
        if let Some((gone, col)) = gone_since(&v, &n, st)? {
            let what = match &col {
                Some(c) => format!("column \"{c}\" of relation \"{gone}\" isn't there now (dropped since)"),
                None => {
                    format!("relation \"{gone}\" isn't there now (renamed, moved, or dropped and made again since)")
                }
            };
            return Err(SqlError {
                code: "42501",
                message: format!("permission denied for table {}", rel_part(&gone)),
                info: Some(Box::new(Violation {
                    detail: Some(format!(
                        "The {what}, so its privileges now can't be checked: only a superuser reads it in the past."
                    )),
                    ..Violation::default()
                })),
            });
        }
        check_as(&n, &who, st)?;
    }
    Ok(())
}

/// The first relation statement `st` reads in `past` (a world as it was) that isn't the relation of its name in `now`:
/// none has its name now, or one with another owner does (dropped and made again: a role that made the new one
/// would otherwise read the old one's past with its own privileges on the new); or, with it, a column of it the
/// statement names that the relation hasn't now (dropped since: its privileges now can't be checked, and the
/// statement's columns, looked up in the relation as it is now, would lose it). ponytail: one dropped and made again
/// by the same owner isn't told apart (a table keeps no mark of its making that its name doesn't give).
fn gone_since(past: &View, now: &View, st: &Stmt) -> Result<Option<(String, Option<String>)>> {
    let mut c = Checker::new(past, 0, false);
    c.stmt(st)?;
    let mut names: Vec<(String, Vec<String>)> = c
        .needs
        .iter()
        .map(|(n, need)| {
            let cols = [&need.select, &need.insert, &need.update, &need.references];
            (n.clone(), cols.into_iter().flatten().flatten().cloned().collect())
        })
        .collect();
    names.extend(c.views.iter().map(|v| (v.clone(), vec![])));
    let mut current = Checker::new(now, 0, false);
    for (name, cols) in names {
        let Some(then) = rel(past, &name)? else { continue };
        if rel(now, &name)?.is_none_or(|o| o.owner != then.owner) {
            return Ok(Some((name, None)));
        }
        let has = current.relation_in(&name, false)?.unwrap_or_default();
        // (a system column, ctid and the like, is every table's)
        let system = |c: &str| matches!(c, "ctid" | "xmin" | "xmax" | "cmin" | "cmax" | "tableoid" | "oid");
        if let Some(col) = cols.iter().find(|c| !has.contains(c) && !system(c)) {
            return Ok(Some((name, Some(col.clone()))));
        }
    }
    Ok(None)
}

/// 42501 for a statement only a superuser may run (see `superuser_only`).
fn only_superusers(st: &Stmt) -> Result<()> {
    match superuser_only(st) {
        Some(what) => Err(roles::denied(
            format!("permission denied: {what} needs a superuser"),
            "Roles' privileges are on tables, views, sequences, functions and schemas, not on worlds; give an agent the capabilities instead.",
        )),
        None => Ok(()),
    }
}

/// Whether the running statement is an agent's (the guest's too).
pub(crate) fn agents() -> bool {
    STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref()).is_some_and(|c| c.agent))
}

thread_local! {
    /// The statement about to run is one checked already: the one it was expanded from (its views
    /// inlined), whose checks were the right ones.
    static VETTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `check`, unless the statement is the expansion of one just checked (see `vetted`); whether it was.
pub(crate) fn check_unless_vetted(db: &Db, branch: &str, st: &Stmt) -> Result<bool> {
    if VETTED.replace(false) {
        return Ok(true);
    }
    check(db, branch, st).map(|_| false)
}

thread_local! {
    /// Relations being dropped because what they depend on goes (see `dropping_dependents`).
    static DEPENDENTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(vec![]) };
}

/// Runs `f`, which drops relations `names` because what they depend on goes (DROP SERVER ... CASCADE's foreign
/// tables), with their owners not asked, as Postgres's dependency drop doesn't ask them: only the check that the
/// caller may drop them is waived, and only for them; everything else `f` runs (event triggers, what they call) is
/// checked as the caller as ever.
pub(crate) fn dropping_dependents<T>(names: &[String], f: impl FnOnce() -> T) -> T {
    struct Back(Vec<String>);
    impl Drop for Back {
        fn drop(&mut self) {
            DEPENDENTS.set(std::mem::take(&mut self.0));
        }
    }
    let _back = Back(DEPENDENTS.replace(names.to_vec()));
    f()
}

/// Runs `f`, the expansion of a statement just checked, without checking it again.
pub(crate) fn vetted<T>(f: impl FnOnce() -> T) -> T {
    VETTED.set(true);
    let out = f();
    VETTED.set(false);
    out
}

/// The statements only a superuser may run here: worlds, agents and the database's own settings
/// (roles' privileges are on objects, not worlds; agents' capabilities are for that), and those
/// that show other roles' rows (DIFF, history, audits).
fn superuser_only(st: &Stmt) -> Option<&'static str> {
    Some(match st {
        Stmt::CreateBranch { .. } => "CREATE WORLD",
        Stmt::DropBranch { .. } => "DROP WORLD",
        Stmt::MergeBranch { .. } => "MERGE WORLD",
        Stmt::Rebase { .. } => "REBASE WORLD",
        Stmt::AlterWorld { .. } => "ALTER WORLD",
        Stmt::Restore { .. } => "RESTORE WORLD",
        Stmt::UndoMerge { .. } | Stmt::UndoAgent { .. } => "UNDO",
        Stmt::ReviewWorld { .. } => "REVIEW",
        // (Postgres's own CHECKPOINT takes pg_checkpoint's privileges: see `Stmt::Checkpoint`)
        Stmt::Checkpoint { world, .. } if world.is_empty() => return None,
        Stmt::Checkpoint { .. } => "CHECKPOINT WORLD",
        Stmt::Diff(..) | Stmt::Readers(..) => "DIFF",
        Stmt::ShowHistory { .. } => "SHOW HISTORY",
        Stmt::ShowAudit { .. } => "SHOW AUDIT",
        Stmt::ShowReviews => "SHOW REVIEWS",
        Stmt::CheckReads(_) => "CHECK READS",
        Stmt::ShowMergeOutcomes { .. } => "SHOW MERGE OUTCOMES",
        Stmt::ShowOutbox | Stmt::AckOutbox(_) | Stmt::RetryOutbox(_) | Stmt::ClaimOutbox { .. } => "the outbox",
        Stmt::CreateAgent { .. } | Stmt::AlterAgent { .. } | Stmt::DropAgent(_) | Stmt::ShowAgents => "agents",
        Stmt::ShowAgentPrivileges { .. } => "SHOW AGENT ... PRIVILEGES",
        Stmt::CreatePolicy { .. } | Stmt::AlterPolicy { .. } | Stmt::DropPolicy(..) | Stmt::ShowPolicies => {
            "merge policies"
        }
        Stmt::CreateCheck { .. } | Stmt::DropCheck(_) | Stmt::ShowChecks => "merge checks",
        Stmt::SetSystem(..) | Stmt::SetRetention(_) | Stmt::SetSync(_) => "ALTER SYSTEM",
        Stmt::Backup(_) | Stmt::Verify => "BACKUP and VERIFY",
        Stmt::Simulate(_) | Stmt::Replay { .. } => "SIMULATE and REPLAY",
        Stmt::RegisterReader { .. } | Stmt::UnregisterReader { .. } | Stmt::MarkReader { .. } => "readers",
        Stmt::UseBranch(..) => return None,
        _ => return None,
    })
}

/// CREATE needs CREATE on the schema; DROP, ALTER and the like need ownership (of the object, or
/// of its schema for a DROP); CREATE INDEX the table's; CREATE TRIGGER, TRIGGER on it.
fn ddl(v: &View, who: &Who, st: &Stmt) -> Result<()> {
    only_superusers(st)?;
    let owns = |o: &Obj| who.has_privs_of(o.owner);
    let in_schema = |name: &str| -> Result<()> {
        let s = schema(v, schema_part(name))?;
        if !s.has(who, CREATE) {
            return Err(s.denied());
        }
        Ok(())
    };
    // an object's owner, or its schema's, may drop it
    let may_drop = |o: &Obj, name: &str| -> Result<()> {
        if owns(o) || owns(&schema(v, schema_part(name))?) {
            return Ok(());
        }
        Err(o.not_owner())
    };
    let own_rel = |name: &str| -> Result<()> {
        let Some(o) = rel(v, name)? else { return Ok(()) };
        if !owns(&o) {
            return Err(o.not_owner());
        }
        Ok(())
    };
    match st {
        Stmt::Create { table, temp: false, constraints, cols, likes, hier, .. } => {
            in_schema(table)?;
            // a foreign key needs REFERENCES on the columns it references
            for c in constraints.iter().chain(cols.iter().flat_map(|c| &c.constraints)) {
                if let ConstraintDef::Foreign { table: t, refcols, .. } = c {
                    references(v, who, t, refcols)?;
                }
            }
            for l in likes {
                if let Some(o) = rel(v, &l.table)?
                    && !o.has(who, SELECT)
                {
                    return Err(o.denied());
                }
            }
            for p in hier.of.iter().map(|(p, _)| p).chain(&hier.inherits) {
                own_rel(p)?;
            }
            // USAGE on its columns' types: those written, and those LIKE, PARTITION OF and INHERITS copy
            use_types(v, who, cols.iter().map(|c| c.ty))?;
            // (and OF's composite type: a typed table's)
            use_type_names(v, who, hier.of_type.iter().cloned())?;
            for t in likes.iter().map(|l| &l.table).chain(hier.of.iter().map(|(p, _)| p)).chain(&hier.inherits) {
                use_type_names(v, who, table_types(v, t)?)?;
            }
        }
        Stmt::CreateAs { table, temp: false, query, .. } => {
            in_schema(table)?;
            use_type_names(v, who, query_types(v, query))?;
        }
        Stmt::CreateView { name, replace, query, .. } => {
            in_schema(name)?;
            if *replace {
                own_rel(name)?;
            }
            // USAGE on its columns' types
            use_type_names(v, who, query_types(v, query))?;
        }
        Stmt::CreateSequence { name, .. } | Stmt::CreateType { name, .. } => in_schema(name)?,
        Stmt::CreateDomain { name, base, .. } => {
            in_schema(name)?;
            // (USAGE on its base type)
            if let Ok(c) = lex(&format!("x {base}")).and_then(|t| Parser::new(t).column()) {
                use_types(v, who, [c.ty])?;
            }
        }
        Stmt::CreateFunction(f, replace, _) => {
            in_schema(&f.name)?;
            // USAGE on its arguments' types and what it returns
            let returns = match &f.ret {
                functions::Ret::Value(t) | functions::Ret::SetOf(t) => vec![*t],
                functions::Ret::Table(cols) => cols.iter().map(|(_, t)| *t).collect(),
                _ => vec![],
            };
            use_types(v, who, f.args.iter().map(|(_, t)| *t).chain(returns))?;
            if *replace
                && let Some(old) = functions::named(v, &f.name)?.into_iter().find(|g| g.sig() == f.sig())
                && !owns(&func(v, &old)?)
            {
                return Err(func(v, &old)?.not_owner());
            }
        }
        Stmt::CreateSchema { owner, .. } => {
            let d = database(v, &database_name(v))?;
            if !d.has(who, CREATE) {
                return Err(d.denied());
            }
            if let Some(o) = owner {
                let r = o.resolve(&who.reg)?;
                if !who.reg.can_set(who.oid, r.oid) && !who.superuser {
                    return err("42501", format!("must be able to SET ROLE \"{}\"", r.name));
                }
            }
        }
        Stmt::CreateDatabase(_) if !who.reg.by_oid(who.oid).is_some_and(|r| r.createdb) => {
            return err("42501", "permission denied to create database");
        }
        Stmt::DropDatabase { .. } => return err("42501", "must be owner of database"),
        Stmt::CreateIndex { table, .. } | Stmt::VectorIndex { table, .. } => own_rel(table)?,
        Stmt::CreateTrigger(t, table, _) => {
            if let Some(o) = rel(v, table)?
                && !o.has(who, TRIGGER)
            {
                return Err(o.denied());
            }
            // and EXECUTE on its function (calling it as a trigger isn't checked again, as in Postgres)
            if let Some(f) = functions::find_routine(v, &t.func, Some(&[][..]))?
                && !func(v, &f)?.has(who, EXECUTE)
            {
                return err("42501", format!("permission denied for function {}", shown_name(&f.name)));
            }
        }
        // event triggers: a superuser's to make, and their owners' to change (only a superuser owns one)
        Stmt::UserDef { tag: "CREATE EVENT TRIGGER", key, .. } if !who.superuser => {
            let name = key.rsplit('/').next().unwrap_or_default();
            return Err(SqlError {
                code: "42501",
                message: format!("permission denied to create event trigger \"{name}\""),
                info: Some(Box::new(Violation {
                    hint: Some("Must be superuser to create an event trigger.".into()),
                    ..Violation::default()
                })),
            });
        }
        Stmt::UserDef { tag: "ALTER EVENT TRIGGER" | "DROP EVENT TRIGGER", key, .. } if !who.superuser => {
            return err(
                "42501",
                format!("must be owner of event trigger {}", key.rsplit('/').next().unwrap_or_default()),
            );
        }
        Stmt::Drop { tables: names, .. } | Stmt::DropObjects { names, .. } => {
            for n in names {
                // (one dropped because what it depends on goes: its owner isn't asked, see `dropping_dependents`)
                if DEPENDENTS.with_borrow(|d| d.contains(n)) {
                    continue;
                }
                if let Some(o) = rel(v, n)? {
                    may_drop(&o, n)?;
                }
            }
        }
        Stmt::DropIndex { name, .. }
        | Stmt::RenameIndex { name, .. }
        | Stmt::IndexColumnAlter { name, .. }
        | Stmt::AttachIndex { parent: name, .. } => {
            let cat = catalog::Catalog::of(v.core, &v.branch)?;
            if let Some(catalog::Obj::Index(r, _)) = cat.named(name)?.and_then(|o| cat.find(o)) {
                let t = cat.rels[r].name.clone();
                if let Some(o) = rel(v, &t)?
                    && !owns(&o)
                {
                    return Err(SqlError {
                        code: "42501",
                        message: format!("must be owner of index {}", rel_part(name)),
                        info: None,
                    });
                }
            }
        }
        Stmt::DropTrigger { table, .. }
        | Stmt::RenameTrigger { table, .. }
        | Stmt::RenameRule { table, .. }
        | Stmt::Refresh { name: table, .. } => own_rel(table)?,
        Stmt::Alter { table, changes, .. } => {
            own_rel(table)?;
            for c in changes {
                match c {
                    // USAGE on a column's new type
                    Alter::AddColumn(c, _) => use_types(v, who, [c.ty])?,
                    Alter::SetType(_, ty, ..) => use_types(v, who, [*ty])?,
                    Alter::AddConstraint(ConstraintDef::Foreign { table: t, refcols, .. }) => {
                        references(v, who, t, refcols)?;
                    }
                    // CREATE on the schema it moves to
                    Alter::SetSchema(to) => {
                        let s = schema(v, to)?;
                        if namespaces(v)?.contains(to) && !s.has(who, CREATE) {
                            return Err(s.denied());
                        }
                    }
                    _ => {}
                }
            }
        }
        // ALTER TYPE (ADD VALUE, RENAME VALUE, RENAME TO): its owner
        Stmt::AlterType { name, .. } => {
            if let Some(o) = type_obj(v, name)?
                && !owns(&o)
            {
                return Err(o.not_owner());
            }
        }
        // COMMENT ON: the object's owner
        Stmt::Comment(c) => match &c.on {
            maintain::Commented::Rel(_, n) => own_rel(n)?,
            // (a column's or a constraint's: Postgres's message calls the table a relation)
            maintain::Commented::Column(n, _) | maintain::Commented::Constraint(_, n) => {
                if let Some(o) = rel(v, n)?
                    && !owns(&o)
                {
                    return Err(SqlError {
                        code: "42501",
                        message: format!("must be owner of relation {}", rel_part(n)),
                        info: None,
                    });
                }
            }
            // ON DOMAIN: the domain's owner
            maintain::Commented::DomainConstraint(_, n) => {
                if let Some(o) = type_obj(v, n)?
                    && !owns(&o)
                {
                    return Err(o.not_owner());
                }
            }
            maintain::Commented::Schema(n) => {
                if namespaces(v)?.contains(n) && !owns(&schema(v, n)?) {
                    return Err(schema(v, n)?.not_owner());
                }
            }
            // a type's or domain's owner
            maintain::Commented::Type(n) => {
                if let Some(o) = type_obj(v, n)?
                    && !owns(&o)
                {
                    return Err(o.not_owner());
                }
            }
            // a function's or procedure's owner
            maintain::Commented::Func(n, types, _) => {
                if let Some(f) = functions::find_routine(v, n, types.as_deref())?
                    && !owns(&func(v, &f)?)
                {
                    return Err(func(v, &f)?.not_owner());
                }
            }
            // a large object's owner
            maintain::Commented::LargeObject(oid) => {
                let o = large_object(v, *oid)?;
                if super::largeobj::exists_in(v, *oid)? && !owns(&o) {
                    return Err(o.not_owner());
                }
            }
            // (a wrapper's or server's owner: see `foreign::comment_target`)
            // (a role's: see `maintain::comment`)
            maintain::Commented::Other
            | maintain::Commented::Role(_)
            | maintain::Commented::Publication(_)
            | maintain::Commented::Aggregate(..)
            | maintain::Commented::Foreign(..)
            | maintain::Commented::Rule(..) => {}
        },
        // REINDEX and CLUSTER of a table: its owner
        Stmt::Maintain(m) if matches!(m.verb, maintain::Verb::Reindex | maintain::Verb::Cluster) => {
            for t in &m.targets {
                if let maintain::Target::Rel(n, _) = t {
                    own_rel(n)?;
                }
            }
        }
        // LOCK TABLE: a privilege on each table, as Postgres asks (SELECT will do for the weakest)
        // (as Postgres's LockTableAclCheck: ACCESS SHARE takes SELECT, ROW EXCLUSIVE a privilege to write, the others
        // UPDATE, DELETE or TRUNCATE; on each relation named, found along the search_path)
        Stmt::Lock(l) => {
            for (n, _) in &l.tables {
                if let Some(o) = rel(v, &find_named(&text_rel(n), super::Kind::Rel))? {
                    let any = |ps: &[u32]| ps.iter().any(|p| o.has(who, *p));
                    let ok = match l.mode {
                        "AccessShareLock" => any(&[SELECT]),
                        "RowExclusiveLock" => any(&[INSERT, UPDATE, DELETE, TRUNCATE]),
                        _ => any(&[UPDATE, DELETE, TRUNCATE]),
                    };
                    if !ok {
                        return Err(o.denied());
                    }
                }
            }
        }
        // pgcrypto is trusted: CREATE on the database makes it (one already there is 42710, or a
        // notice, first), and the role that makes it owns it
        Stmt::CreateExtension { name, .. }
            if v.get(&pgcrypto::key(name))?.is_none() && !database(v, &database_name(v))?.has(who, CREATE) =>
        {
            return Err(SqlError {
                code: "42501",
                message: format!("permission denied to create extension \"{name}\""),
                info: Some(Box::new(Violation {
                    hint: Some("Must have CREATE privilege on current database to create this extension.".into()),
                    ..Violation::default()
                })),
            });
        }
        Stmt::AlterExtension { name, schema: to, .. } => {
            if let Some(o) = extension(v, name)? {
                // (SET SCHEMA's schema is looked up, and its CREATE checked, first, as in Postgres)
                if let Some(s) = to
                    && namespaces(v)?.contains(s)
                {
                    let s = schema(v, s)?;
                    if !s.has(who, CREATE) {
                        return Err(s.denied());
                    }
                }
                if !owns(&o) {
                    return Err(o.not_owner());
                }
            }
        }
        Stmt::DropExtension { names, .. } => {
            for n in names {
                if let Some(o) = extension(v, n)?
                    && !owns(&o)
                {
                    return Err(o.not_owner());
                }
            }
        }
        Stmt::DropType { names, .. } => {
            for n in names {
                if let Some(o) = type_obj(v, n)? {
                    may_drop(&o, n)?;
                }
            }
        }
        Stmt::RenameView { name, .. }
        | Stmt::ViewOptions { name, .. }
        | Stmt::ViewDefault { name, .. }
        | Stmt::AlterSequence { name, .. } => own_rel(name)?,
        Stmt::DropFunction { names, .. } => {
            for (name, types) in names {
                if let Some(f) = functions::find_routine(v, name, types.as_deref())? {
                    let o = func(v, &f)?;
                    if !owns(&o) && !owns(&schema(v, schema_part(&f.name))?) {
                        return Err(SqlError {
                            code: "42501",
                            message: format!("must be owner of {} {}", o.kind.noun(), shown_name(&f.name)),
                            info: None,
                        });
                    }
                }
            }
        }
        Stmt::DropSchema { names, .. } => {
            for n in names {
                if n != "public" && namespaces(v)?.contains(n) && !owns(&schema(v, n)?) {
                    return Err(schema(v, n)?.not_owner());
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// 42501 unless `who` has USAGE on those of types `tys` made with CREATE TYPE or CREATE DOMAIN, as
/// making a column, a domain or a function of one needs.
fn use_types(v: &View, who: &Who, tys: impl IntoIterator<Item = Type>) -> Result<()> {
    let names = tys.into_iter().filter_map(|t| match t {
        Type::Enum(i) => Some(enum_name(i)),
        _ => None,
    });
    use_type_names(v, who, names)
}

/// `use_types`, of types by their stored names.
fn use_type_names(v: &View, who: &Who, names: impl IntoIterator<Item = String>) -> Result<()> {
    for name in names {
        if let Some(o) = type_obj(v, &name)?
            && !o.has(who, USAGE)
        {
            return Err(o.denied());
        }
    }
    Ok(())
}

/// The type made with CREATE TYPE or CREATE DOMAIN a stored column is, if it is one (a domain
/// column's: the domain).
fn col_type(c: &Col) -> Option<String> {
    c.domain.clone().or_else(|| match c.ty {
        Type::Enum(i) => Some(enum_name(i)),
        _ => None,
    })
}

/// Those of table `name`'s columns' types made here, as LIKE, PARTITION OF and INHERITS copy them.
fn table_types(v: &View, name: &str) -> Result<Vec<String>> {
    Ok(load(v, name)?.map(|t| t.cols.iter().filter(|c| !c.hidden).filter_map(col_type).collect()).unwrap_or_default())
}

/// Those of a query's output columns' types made here, which a view or a table made from it has:
/// an enum's, and a domain column's read as it is (`select p from t`, `*`).
fn query_types(v: &View, q: &Select) -> Vec<String> {
    let mut out: Vec<String> = match select_columns_with(v, q, &[], &[]) {
        Ok((cols, _)) => cols
            .iter()
            .filter_map(|(_, t)| match t {
                Type::Enum(i) => Some(enum_name(*i)),
                _ => None,
            })
            .collect(),
        Err(_) => vec![],
    };
    let tables: Vec<(&Source, Table)> = q
        .from
        .iter()
        .chain(q.joins.iter().map(|j| &j.src))
        .filter(|s| s.sub.is_none() && s.srf.is_none())
        .filter_map(|s| load(v, &s.table).ok().flatten().map(|t| (s, t)))
        .collect();
    let named = |s: &Source, n: &str| s.alias == n || s.table == n || rel_part(&s.table) == n;
    let domains = |t: &Table| t.cols.iter().filter(|c| !c.hidden).filter_map(|c| c.domain.clone()).collect::<Vec<_>>();
    for item in &q.items {
        match item {
            Item::Star => tables.iter().for_each(|(_, t)| out.extend(domains(t))),
            Item::TableStar(a) => tables.iter().filter(|(s, _)| named(s, a)).for_each(|(_, t)| out.extend(domains(t))),
            Item::Expr(Expr::Col(q, n), _) => {
                let col = tables
                    .iter()
                    .filter(|(s, _)| q.as_deref().is_none_or(|q| named(s, q)))
                    .find_map(|(_, t)| t.cols.iter().find(|c| c.name == *n));
                out.extend(col.and_then(|c| c.domain.clone()));
            }
            _ => {}
        }
    }
    out
}

/// How many values MERGE's `INSERT VALUES (...)` gives (`values` is the list inside the parentheses, as text): its
/// top-level commas, plus one.
fn merge_values_width(values: &str) -> usize {
    let Ok(toks) = lex(values) else { return 0 };
    let mut depth = 0i32;
    let commas = toks
        .iter()
        .filter(|t| {
            match t {
                Tok::Sym("(" | "[") => depth += 1,
                Tok::Sym(")" | "]") => depth -= 1,
                _ => {}
            }
            depth == 0 && matches!(t, Tok::Sym(","))
        })
        .count();
    if toks.is_empty() { 0 } else { commas + 1 }
}

/// REFERENCES on table `t`'s columns `cols` (its primary key's, none named).
fn references(v: &View, who: &Who, t: &str, cols: &[String]) -> Result<()> {
    let Some(o) = rel(v, t)? else { return Ok(()) };
    if o.has(who, REFERENCES) {
        return Ok(());
    }
    let cols: Vec<String> = match cols.is_empty() {
        false => cols.to_vec(),
        true => {
            load(v, t)?.map(|tb| tb.key_cols().iter().map(|&k| tb.cols[k].name.clone()).collect()).unwrap_or_default()
        }
    };
    for c in &cols {
        if held(who, o.owner, &column(v, t, c)?.acl, REFERENCES).0 != REFERENCES {
            return Err(o.denied());
        }
    }
    Ok(())
}

/// Whether `who` may call function `f` (EXECUTE, and USAGE on its schema), in world `v`.
pub(crate) fn may_execute(v: &View, who: &Who, f: &functions::Function) -> Result<()> {
    if who.superuser {
        return Ok(());
    }
    let s = schema_part(&f.name);
    if !schema(v, s)?.has(who, USAGE) {
        return Err(schema(v, s)?.denied());
    }
    let o = func(v, f)?;
    if !o.has(who, EXECUTE) {
        return Err(o.denied());
    }
    Ok(())
}

/// Who a SECURITY DEFINER function `f` runs as: its owner.
pub(crate) fn definer(v: &View, who: &Who, f: &functions::Function) -> Result<Who> {
    Ok(who.as_role(func(v, f)?.owner))
}

/// What sequence function `what` (nextval, currval, setval) needs of sequence `name`, checked for
/// the running statement's role: USAGE or UPDATE for nextval, USAGE or SELECT for currval and
/// lastval, UPDATE for setval.
pub(crate) fn sequence(v: &View, name: &str, what: CtxFn) -> Result<()> {
    let Some(who) = roles::checked() else { return Ok(()) };
    let s = schema_part(name);
    if !schema(v, s)?.has(&who, USAGE) {
        return Err(schema(v, s)?.denied());
    }
    let Some(o) = rel(v, name)? else { return Ok(()) };
    let ok = match what {
        CtxFn::NextVal => o.has(&who, USAGE) || o.has(&who, UPDATE),
        CtxFn::SetVal => o.has(&who, UPDATE),
        _ => o.has(&who, USAGE) || o.has(&who, SELECT),
    };
    if !ok {
        return Err(o.denied());
    }
    Ok(())
}

// ---------------------------------------------------------------- GRANT and REVOKE

/// What a GRANT or REVOKE of privileges is on.
#[derive(Clone, Debug)]
pub(crate) enum On {
    /// TABLE (also views and sequences, as in Postgres) or SEQUENCE: their names
    Rels(Vec<String>, bool),
    Schemas(Vec<String>),
    /// FUNCTION, PROCEDURE or ROUTINE (`Some(false)`, `Some(true)`, None): names, and argument types if given
    Routines(Vec<(String, Option<Vec<Type>>)>, Option<bool>),
    Databases(Vec<String>),
    /// ALL TABLES | SEQUENCES | FUNCTIONS | PROCEDURES | ROUTINES IN SCHEMA s, ...
    AllIn(&'static str, Vec<String>),
    /// TYPE or DOMAIN (true): their stored names
    Types(Vec<String>, bool),
    /// FOREIGN DATA WRAPPER (`Fdw`) or FOREIGN SERVER (`Server`): their names
    Foreign(Kind, Vec<String>),
    /// LARGE OBJECT: their oids
    LargeObjects(Vec<i64>),
    /// ALTER DEFAULT PRIVILEGES: the objects roles will make
    Defaults(Box<Defaults>),
}

/// ALTER DEFAULT PRIVILEGES's FOR ROLE and IN SCHEMA, and the kind of object its GRANT or REVOKE is
/// on.
#[derive(Clone, Debug)]
pub(crate) struct Defaults {
    /// FOR ROLE | USER (none: the running role)
    pub roles: Vec<Spec>,
    /// IN SCHEMA (none: a role's own entry, for every schema)
    pub schemas: Vec<String>,
    /// pg_default_acl.defaclobjtype: `r` tables, `S` sequences, `f` functions, `T` types, `n` schemas
    pub objtype: char,
}

/// GRANT or REVOKE privileges ON ... TO | FROM roles.
#[derive(Clone, Debug)]
pub(crate) struct Grant {
    pub grant: bool,
    /// the privileges by name, each with the columns it is on (none: the object); None: ALL
    pub privs: Option<Vec<(String, Vec<String>)>>,
    /// ALL (columns)
    pub all_cols: Vec<String>,
    pub on: On,
    pub grantees: Vec<Spec>,
    /// WITH GRANT OPTION, or REVOKE GRANT OPTION FOR
    pub option: bool,
    pub by: Option<Spec>,
    pub cascade: bool,
}

impl Parser {
    /// After GRANT or REVOKE: privileges ON objects TO | FROM roles, or roles TO | FROM roles.
    pub(super) fn grant(&mut self, grant: bool) -> Result<Stmt> {
        self.grant_or_default(grant, None)
    }

    /// After ALTER: `DEFAULT PRIVILEGES [FOR ROLE | USER r, ...] [IN SCHEMA s, ...]`, then its GRANT
    /// or REVOKE ... ON TABLES | SEQUENCES | FUNCTIONS | ROUTINES | TYPES | SCHEMAS.
    pub(super) fn default_privileges(&mut self) -> Result<Stmt> {
        self.expect_kw("default")?;
        self.expect_kw("privileges")?;
        let (mut roles, mut schemas) = (None, None);
        loop {
            let at = self.i;
            if self.kw("for") {
                if !self.kw("role") {
                    self.expect_kw("user")?;
                }
                // (PUBLIC is no role: 42704 when it is looked up, as in Postgres)
                let mut list = vec![self.role_spec(true)?];
                while self.sym(",") {
                    list.push(self.role_spec(true)?);
                }
                if roles.replace(list).is_some() {
                    self.i = at;
                    return err(SYNTAX, "conflicting or redundant options");
                }
            } else if self.kw("in") {
                self.expect_kw("schema")?;
                if schemas.replace(self.names_list()?).is_some() {
                    self.i = at;
                    return err(SYNTAX, "conflicting or redundant options");
                }
            } else {
                break;
            }
        }
        let grant = match self.kw("grant") {
            true => true,
            false => {
                self.expect_kw("revoke")?;
                false
            }
        };
        self.grant_or_default(grant, Some((roles.unwrap_or_default(), schemas.unwrap_or_default())))
    }

    /// GRANT or REVOKE, of what objects are (or, with `defaults`, ALTER DEFAULT PRIVILEGES's FOR ROLE
    /// and IN SCHEMA, will be) made: after the GRANT or REVOKE.
    fn grant_or_default(&mut self, grant: bool, mut defaults: Option<(Vec<Spec>, Vec<String>)>) -> Result<Stmt> {
        let mut option = false;
        let mut role_option = None;
        if !grant {
            if self.kw("grant") {
                self.expect_kw("option")?;
                self.expect_kw("for")?;
                option = true;
            } else if self.is_kw("admin") || self.is_kw("inherit") || self.is_kw("set") {
                let word = if self.kw("admin") {
                    "admin"
                } else if self.kw("inherit") {
                    "inherit"
                } else {
                    self.expect_kw("set")?;
                    "set"
                };
                self.expect_kw("option")?;
                self.expect_kw("for")?;
                role_option = Some(word);
            }
        }
        // roles (GRANT a, b TO ...) unless an ON comes before the TO or FROM
        let mut depth = 0;
        let on = self.t[self.i..]
            .iter()
            .find_map(|t| match t {
                Tok::Sym("(") => {
                    depth += 1;
                    None
                }
                Tok::Sym(")") => {
                    depth -= 1;
                    None
                }
                Tok::Sym(";") => Some(false),
                Tok::Word(w) if depth == 0 && w == "on" => Some(true),
                Tok::Word(w) if depth == 0 && (w == "to" || w == "from") => Some(false),
                _ => None,
            })
            .unwrap_or(false);
        if !on || (defaults.is_some() && role_option.is_some()) {
            if defaults.is_some() {
                return self.fail("expected ON");
            }
            let mut roles = vec![self.role_name()?];
            while self.sym(",") {
                roles.push(self.role_name()?);
            }
            return if grant { self.grant_role(roles) } else { self.revoke_role(role_option, roles) };
        }
        // privileges: ALL [PRIVILEGES] [(cols)], or a list of names, each with columns
        let (mut privs, mut all_cols) = (Some(vec![]), vec![]);
        if self.kw("all") {
            self.kw("privileges");
            if self.sym("(") {
                all_cols = self.names()?;
            }
            privs = None;
        } else {
            loop {
                let name = match self.next() {
                    Some(Tok::Word(w) | Tok::Ident(w)) => w,
                    t => {
                        self.put_back(t);
                        return self.fail("expected a privilege");
                    }
                };
                let cols = if self.sym("(") { self.names()? } else { vec![] };
                privs.as_mut().unwrap().push((name, cols));
                if !self.sym(",") {
                    break;
                }
            }
        }
        self.expect_kw("on")?;
        if role_option.is_some() {
            return self.fail("expected a role");
        }
        if let Some(ps) = &privs {
            for (p, _) in ps {
                if !matches!(
                    p.as_str(),
                    "select"
                        | "insert"
                        | "update"
                        | "delete"
                        | "truncate"
                        | "references"
                        | "trigger"
                        | "execute"
                        | "usage"
                        | "create"
                        | "connect"
                        | "temporary"
                        | "temp"
                ) {
                    return err(SYNTAX, format!("unrecognized privilege type \"{p}\""));
                }
            }
        }
        let on = if let Some((roles, schemas)) = defaults.take() {
            let objtype = if self.kw("tables") {
                'r'
            } else if self.kw("sequences") {
                'S'
            } else if self.kw("functions") || self.kw("routines") {
                'f'
            } else if self.kw("types") {
                'T'
            } else if self.kw("schemas") {
                'n'
            } else {
                return self.fail("expected TABLES, SEQUENCES, FUNCTIONS, ROUTINES, TYPES or SCHEMAS");
            };
            On::Defaults(Box::new(Defaults { roles, schemas, objtype }))
        } else if self.kw("all") {
            let what = if self.kw("tables") {
                "TABLES"
            } else if self.kw("sequences") {
                "SEQUENCES"
            } else if self.kw("functions") {
                "FUNCTIONS"
            } else if self.kw("procedures") {
                "PROCEDURES"
            } else {
                self.expect_kw("routines")?;
                "ROUTINES"
            };
            self.expect_kw("in")?;
            self.expect_kw("schema")?;
            On::AllIn(what, self.names_list()?)
        } else if self.kw("schema") {
            On::Schemas(self.names_list()?)
        } else if self.kw("database") {
            On::Databases(self.names_list()?)
        } else if self.is_kw("function") || self.is_kw("procedure") || self.is_kw("routine") {
            let kind = if self.kw("function") {
                Some(false)
            } else if self.kw("procedure") {
                Some(true)
            } else {
                self.expect_kw("routine")?;
                None
            };
            let Stmt::DropFunction { names, .. } = self.drop_function(kind == Some(true))? else { unreachable!() };
            On::Routines(names, kind)
        } else if self.kw("sequence") {
            On::Rels(self.rel_names()?, true)
        } else if self.is_kw("type") || self.is_kw("domain") {
            let domain = self.kw("domain");
            if !domain {
                self.expect_kw("type")?;
            }
            let mut names = vec![self.qualified(false, super::Kind::Type)?];
            while self.sym(",") {
                names.push(self.qualified(false, super::Kind::Type)?);
            }
            On::Types(names, domain)
        } else if self.is_kw("foreign")
            && matches!(self.t.get(self.i + 1), Some(Tok::Word(w)) if w == "data" || w == "server")
        {
            self.i += 1;
            let kind = if self.kw("data") {
                self.expect_kw("wrapper")?;
                Kind::Fdw
            } else {
                self.expect_kw("server")?;
                Kind::Server
            };
            On::Foreign(kind, self.names_list()?)
        } else if self.is_kw("large") && matches!(self.t.get(self.i + 1), Some(Tok::Word(w)) if w == "object") {
            self.i += 2;
            let mut oids = vec![self.large_object_oid()?];
            while self.sym(",") {
                oids.push(self.large_object_oid()?);
            }
            On::LargeObjects(oids)
        } else if self.is_kw("language") || self.is_kw("foreign") || self.is_kw("tablespace") || self.is_kw("parameter")
        {
            let w = if let Some(Tok::Word(w)) = self.peek() { w.to_uppercase() } else { String::new() };
            return err(
                UNSUPPORTED,
                format!(
                    "GRANT and REVOKE ON {w} aren't supported: tables, sequences, schemas, functions, types, domains \
                     and databases are"
                ),
            );
        } else {
            self.kw("table");
            On::Rels(self.rel_names()?, false)
        };
        if grant {
            self.expect_kw("to")?
        } else {
            self.expect_kw("from")?
        }
        let grantees = self.role_specs_public()?;
        if grant && self.kw("with") {
            self.expect_kw("grant")?;
            self.expect_kw("option")?;
            option = true;
        }
        let by = if !matches!(on, On::Defaults(_)) && self.kw("granted") {
            self.expect_kw("by")?;
            Some(self.role_spec(false)?)
        } else {
            None
        };
        let cascade = !grant && self.cascade();
        Ok(Stmt::Grant(Box::new(Grant { grant, privs, all_cols, on, grantees, option, by, cascade })))
    }

    /// A large object's oid, as GRANT ... ON LARGE OBJECT names one (a number, maybe signed).
    /// After ALTER: `LARGE OBJECT oid OWNER TO role`.
    pub(super) fn alter_large_object(&mut self) -> Result<Stmt> {
        self.expect_kw("large")?;
        self.expect_kw("object")?;
        let oid = self.large_object_oid()?;
        self.expect_kw("owner")?;
        self.expect_kw("to")?;
        Ok(Stmt::AlterOwner(Owned::LargeObject(oid), self.role_spec(false)?))
    }

    pub(super) fn large_object_oid(&mut self) -> Result<i64> {
        let neg = self.sym("-");
        match self.next() {
            Some(Tok::Num(n)) => match n.parse::<i64>() {
                Ok(x) if (0..=u32::MAX as i64).contains(&x) && !neg => Ok(x),
                _ => err("22003", format!("value \"{}{n}\" is out of range for type oid", if neg { "-" } else { "" })),
            },
            t => {
                self.put_back(t);
                self.fail("expected a large object's oid")
            }
        }
    }

    fn names_list(&mut self) -> Result<Vec<String>> {
        let mut out = vec![self.name()?];
        while self.sym(",") {
            out.push(self.name()?);
        }
        Ok(out)
    }

    pub(super) fn role_specs_public(&mut self) -> Result<Vec<Spec>> {
        let mut out = vec![];
        loop {
            self.kw("group");
            out.push(self.role_spec(true)?);
            if !self.sym(",") {
                return Ok(out);
            }
        }
    }
}

/// A privilege's bit, by name.
fn bit(name: &str) -> u32 {
    match name {
        "select" => SELECT,
        "insert" => INSERT,
        "update" => UPDATE,
        "delete" => DELETE,
        "truncate" => TRUNCATE,
        "references" => REFERENCES,
        "trigger" => TRIGGER,
        "execute" => EXECUTE,
        "usage" => USAGE,
        "create" => CREATE,
        "connect" => CONNECT,
        _ => TEMP,
    }
}

/// The objects a GRANT is on, in world `v`.
fn targets(v: &View, on: &On) -> Result<Vec<Obj>> {
    let mut out = vec![];
    match on {
        On::Rels(names, seq) => {
            for n in names {
                // (a catalog table, pg_proc, pg_authid: the bootstrap superuser's, its grants kept as a table's)
                let bare = n.strip_prefix("pg_catalog.").unwrap_or(n);
                let o = match rel(v, n)? {
                    None if !*seq && catalog_table(bare) => {
                        let key = key_of(Kind::Table, &format!("pg_catalog.{bare}"));
                        Obj::new(Kind::Table, key.clone(), bare.to_string(), entry(v, &key)?, BOOTSTRAP)
                    }
                    _ => need_rel(v, n)?,
                };
                if *seq && o.kind != Kind::Sequence {
                    return err("42809", format!("\"{}\" is not a sequence", rel_part(n)));
                }
                out.push(o);
            }
        }
        On::Schemas(names) => {
            let known = namespaces(v)?;
            for n in names {
                if !known.contains(n) && !matches!(n.as_str(), "pg_catalog" | "information_schema") {
                    return err("3F000", format!("schema \"{n}\" does not exist"));
                }
                out.push(schema(v, n)?);
            }
        }
        On::Routines(names, kind) => {
            for (name, types) in names {
                let Some(f) = functions::find_routine(v, name, types.as_deref())? else {
                    // an aggregate is a function here (ON FUNCTION or ROUTINE), by its argument types
                    if *kind != Some(true)
                        && let Some(types) = types
                        && v.get(&userdefs::agg_key(rel_part(name), types))?.is_some()
                    {
                        out.push(aggregate(v, &userdefs::agg_key(rel_part(name), types), name)?);
                        continue;
                    }
                    let noun = match kind {
                        Some(false) => "function",
                        Some(true) => "procedure",
                        None => "routine",
                    };
                    return match types {
                        Some(ts) => {
                            let shown: Vec<String> = ts.iter().map(|t| t.pg_name().into_owned()).collect();
                            err(NO_FUNCTION, format!("{noun} {}({}) does not exist", written(name), shown.join(", ")))
                        }
                        None => err(NO_FUNCTION, format!("function {} does not exist", written(name))),
                    };
                };
                if let Some(p) = kind
                    && f.procedure != *p
                {
                    return err("42809", format!("{} is not a {}", f.sig(), if *p { "procedure" } else { "function" }));
                }
                out.push(func(v, &f)?);
            }
        }
        On::Databases(names) => {
            for n in names {
                // (under CHRONOS_PG_EXACT the regression suite's database is the one the session is in)
                if *n != database_name(v) && !(pg_exact() && n == "regression") {
                    return err(
                        UNSUPPORTED,
                        format!(
                            "GRANT ON DATABASE takes the world the session is in (\"{}\"), not \"{n}\"",
                            database_name(v)
                        ),
                    );
                }
                out.push(database(v, n)?);
            }
        }
        On::AllIn(what, schemas) => {
            let known = namespaces(v)?;
            for s in schemas {
                if !known.contains(s) {
                    return err("3F000", format!("schema \"{s}\" does not exist"));
                }
                match *what {
                    "TABLES" | "SEQUENCES" => {
                        let cat = catalog::Catalog::of(v.core, &v.branch)?;
                        for r in cat.rels.iter().filter(|r| schema_part(&r.name) == s) {
                            let seq = r.kind == 'S';
                            if seq == (*what == "SEQUENCES") {
                                out.extend(rel(v, &r.name)?);
                            }
                        }
                    }
                    _ => {
                        for r in v.prefix(&format!("{}/", functions::FUNCTIONS))? {
                            let f = functions::Function::from_raw(&r?.1)?;
                            let fits = match *what {
                                "FUNCTIONS" => !f.procedure,
                                "PROCEDURES" => f.procedure,
                                _ => true,
                            };
                            if fits && schema_part(&f.name) == s {
                                out.push(func(v, &f)?);
                            }
                        }
                    }
                }
            }
        }
        On::Types(names, domain) => {
            // (a built-in type, by any of its names: `int`, `int4`, `pg_catalog.int4`, `varchar`)
            let builtin = |name: &str| {
                Type::from_name(name).is_some()
                    || catalog::reg_oid(&catalog::Catalog::empty(), super::Reg::Type, name).is_ok()
            };
            for n in names {
                let Some(mut o) = type_obj(v, n)? else {
                    // an array type (`_mood`, `_int4`): its element's privileges are its own
                    let elem = qualify(schema_part(n), rel_part(n).strip_prefix('_').unwrap_or_default());
                    if rel_part(n).starts_with('_') && (v.get(&type_key(&elem))?.is_some() || builtin(&elem)) {
                        return Err(SqlError {
                            code: "0LP01",
                            message: "cannot set privileges of array types".into(),
                            info: Some(Box::new(Violation {
                                hint: Some("Set the privileges of the element type instead.".into()),
                                ..Violation::default()
                            })),
                        });
                    }
                    if builtin(n) {
                        return err(
                            UNSUPPORTED,
                            format!(
                                "GRANT and REVOKE ON TYPE {n} aren't supported: a built-in type's privileges are \
                                 Postgres's defaults; a type made with CREATE TYPE or CREATE DOMAIN has its own"
                            ),
                        );
                    }
                    if relation_kind(v, n)?.is_some() {
                        return err(
                            UNSUPPORTED,
                            format!(
                                "GRANT and REVOKE ON TYPE of a table's row type ({n}) aren't supported: no column can \
                                 be of one here"
                            ),
                        );
                    }
                    return err("42704", format!("type \"{n}\" does not exist"));
                };
                if *domain && !is_domain(v, n)? {
                    return err("42809", format!("\"{}\" is not a domain", rel_part(n)));
                }
                // (GRANT's messages name it without its schema, as Postgres's do)
                o.name = rel_part(n).to_string();
                out.push(o);
            }
        }
        On::Foreign(kind, names) => {
            for n in names {
                let what = if *kind == Kind::Fdw { "fdw" } else { "server" };
                if !super::foreign::exists(v, what, n)? {
                    let noun = if *kind == Kind::Fdw { "foreign-data wrapper" } else { "server" };
                    return err("42704", format!("{noun} \"{n}\" does not exist"));
                }
                out.push(foreign_obj(v, *kind, n)?);
            }
        }
        On::LargeObjects(oids) => {
            for &oid in oids {
                if !super::largeobj::exists_in(v, oid)? {
                    return err("42704", format!("large object {oid} does not exist"));
                }
                out.push(large_object(v, oid)?);
            }
        }
        On::Defaults(_) => {}
    }
    Ok(out)
}

/// Whether type `name` (as stored) is a domain.
fn is_domain(v: &View, name: &str) -> Result<bool> {
    let raw = v.get(&type_key(name))?.unwrap_or_default();
    Ok(serde_json::from_str::<Json>(&raw).unwrap_or_default().get("domain").is_some())
}

/// Runs a GRANT or REVOKE of privileges in world `branch`, as the running statement's role.
pub(super) fn run_grant(db: &Db, branch: &str, g: &Grant) -> Result<Output> {
    if let On::Defaults(d) = &g.on {
        return set_defaults(db, branch, g, d);
    }
    let who = roles::current().map_or_else(|| Who::boot("chronos"), |w| (*w).clone());
    let reg = roles::registry(&db.core)?;
    let who = Who { reg: reg.clone(), ..who };
    let grantees: Vec<i64> = g.grantees.iter().map(|s| s.oid(&reg)).collect::<Result<_>>()?;
    let by = g.by.as_ref().map(|b| b.resolve(&reg).map(|r| r.oid)).transpose()?;
    let mut notes = vec![];
    db.batch_with::<(), SqlError>(branch, |v| {
        let mut writes = vec![];
        for obj in targets(v, &g.on)? {
            // the privileges asked for on the object, and on each column
            let mut want = 0u32;
            let mut per_col: Vec<(String, u32)> = vec![];
            let mut on_col = |c: &String, b: u32| match per_col.iter_mut().find(|(n, _)| n == c) {
                Some((_, x)) => *x |= b,
                None => per_col.push((c.clone(), b)),
            };
            match &g.privs {
                None if !g.all_cols.is_empty() => g.all_cols.iter().for_each(|c| on_col(c, Kind::Column.all())),
                None => want = obj.kind.all(),
                Some(ps) => {
                    for (p, cols) in ps {
                        let b = bit(p);
                        let what = if cols.is_empty() { obj.kind } else { Kind::Column };
                        let valid = match what {
                            Kind::Column if !matches!(obj.kind, Kind::Table | Kind::View | Kind::MatView) => 0,
                            k => k.all(),
                        };
                        if b & valid == 0 {
                            let noun = match g.on {
                                On::Types(_, true) => "domain",
                                _ => what.noun(),
                            };
                            return err("0LP01", format!("invalid privilege type {} for {noun}", p.to_uppercase()));
                        }
                        match cols.is_empty() {
                            true => want |= b,
                            false => cols.iter().for_each(|c| on_col(c, b)),
                        }
                    }
                }
            }
            let table = obj.key.strip_prefix("rel/").unwrap_or_default().to_string();
            let cols =
                if per_col.is_empty() && (g.grant || !matches!(obj.kind, Kind::Table | Kind::View | Kind::MatView)) {
                    vec![]
                } else {
                    columns(v, &table)?
                };
            for (c, _) in &per_col {
                if !cols.iter().any(|(n, _)| n == c) {
                    return err(NO_COLUMN, format!("column \"{c}\" of relation \"{}\" does not exist", obj.name));
                }
            }
            // what isn't given: a warning, as Postgres's
            let mut warn = |gave: u32, asked: u32, what: String| {
                if gave != asked {
                    let n = match (gave == 0, g.grant) {
                        (true, true) => "no privileges were granted",
                        (true, false) => "no privileges could be revoked",
                        (false, true) => "not all privileges were granted",
                        (false, false) => "not all privileges could be revoked",
                    };
                    notes.push(format!("{n} for {what}"));
                }
            };
            // the object's own privileges, as the role holding their grant options gives them
            let mut table_gave = (BOOTSTRAP, 0u32);
            if want != 0 {
                let (grantor, gave) = grantor_for(&who, &obj, by, want)?;
                warn(gave, want, format!("\"{}\"", obj.name));
                table_gave = (grantor, gave);
                // (one that gives or takes nothing still writes the ACL out, as Postgres does)
                if gave != 0 || !obj.own {
                    let mut acl = obj.acl.clone();
                    change_acl(&mut acl, &grantees, grantor, gave, g)?;
                    let mut e = entry(v, &obj.key)?;
                    e.acl = Some(acl);
                    writes.push(write(&obj.key, &e));
                }
            }
            // each column's, as the role holding their grant options on the table or the column gives
            // them (Postgres's select_best_grantor over both); a table's REVOKE takes its columns' too
            for (c, acl) in &cols {
                let own = per_col.iter().find(|(n, _)| n == c).map_or(0, |(_, b)| *b);
                let from_table = match g.grant {
                    true => 0,
                    false => table_gave.1 & Kind::Column.all(),
                };
                if own == 0 && (from_table == 0 || acl.is_empty()) {
                    continue;
                }
                let mut after = acl.clone();
                if own != 0 {
                    let merged = Obj {
                        kind: Kind::Column,
                        key: key_of(Kind::Column, &format!("{table}/{c}")),
                        name: format!("\"{c}\" of relation \"{}\"", obj.name),
                        owner: obj.owner,
                        acl: obj.acl.iter().chain(acl).copied().collect(),
                        own: true,
                    };
                    let (grantor, gave) = grantor_for(&who, &merged, by, own)?;
                    warn(gave, own, format!("column \"{c}\" of relation \"{}\"", obj.name));
                    change_acl(&mut after, &grantees, grantor, gave, g)?;
                }
                if from_table != 0 {
                    change_acl(&mut after, &grantees, table_gave.0, from_table, g)?;
                }
                if after != *acl || own != 0 {
                    writes.push(col_write(&table, c, after));
                }
            }
        }
        Ok((writes, ()))
    })?;
    for n in notes {
        NOTICES.with_borrow_mut(|x| {
            x.push(Notice { level: "WARNING", code: "01007", message: n, detail: None, hint: None })
        });
    }
    tag(if g.grant { "GRANT" } else { "REVOKE" })
}

/// The write of column `c`'s ACL (none left: no row).
fn col_write(table: &str, c: &str, acl: Vec<AclItem>) -> (String, Option<String>) {
    let key = key_of(Kind::Column, &format!("{table}/{c}"));
    let e = Entry { owner: None, acl: (!acl.is_empty()).then_some(acl) };
    write(&key, &e)
}

/// Who a GRANT or REVOKE on `obj` is made as, and which of `want` it may give: the owner, when
/// `who` is a superuser or has the owner's privileges; else the role through which it holds the
/// most of them with grant option (Postgres's select_best_grantor).
fn grantor_for(who: &Who, obj: &Obj, by: Option<i64>, want: u32) -> Result<(i64, u32)> {
    if let Some(b) = by {
        if !who.has_privs_of(b) {
            let name = who.reg.name_of(b);
            return Err(roles::denied(
                format!("permission denied to grant privileges as role \"{name}\""),
                format!("Only roles with privileges of role \"{name}\" may grant privileges as this role."),
            ));
        }
        let as_b = who.as_role(b);
        if as_b.superuser || as_b.has_privs_of(obj.owner) {
            return Ok((obj.owner, want));
        }
        let (_, opts) = held(&as_b, obj.owner, &obj.acl, want);
        return Ok((b, opts & want));
    }
    if who.superuser || who.has_privs_of(obj.owner) {
        return Ok((obj.owner, want));
    }
    // the role among `who`'s with the most of them as grant options
    let mut best: Option<(i64, u32)> = None;
    for r in &who.privs_of {
        let opts = obj.acl.iter().filter(|i| i.grantee == *r).fold(0, |a, i| a | i.opts) & want;
        if best.is_none_or(|(_, b)| opts.count_ones() > b.count_ones()) {
            best = Some((*r, opts));
        }
    }
    let (role, opts) = best.unwrap_or((who.oid, 0));
    // none to give: 42501, unless it holds some privilege on it (then only a warning)
    if opts == 0 && (held(who, obj.owner, &obj.acl, obj.kind.all()).0 | predefined(who, obj.kind, obj.kind.all())) == 0
    {
        return Err(obj.denied());
    }
    Ok((role, opts))
}

/// Gives (or takes away) `privs` to `grantees` from `grantor` in `acl`, as GRANT (REVOKE) does:
/// a new grantee's item goes last; a REVOKE of a grant option others' grants rest on takes them
/// too with CASCADE, and is refused without (2BP01).
fn change_acl(acl: &mut Vec<AclItem>, grantees: &[i64], grantor: i64, privs: u32, g: &Grant) -> Result<()> {
    if privs == 0 {
        return Ok(());
    }
    for &grantee in grantees {
        if g.grant {
            if grantee == PUBLIC && g.option {
                return err("0LP01", "grant options can only be granted to roles");
            }
            match acl.iter_mut().find(|i| i.grantee == grantee && i.grantor == grantor) {
                Some(i) => {
                    i.privs |= privs;
                    if g.option {
                        i.opts |= privs;
                    }
                }
                None => acl.push(AclItem { grantee, grantor, privs, opts: if g.option { privs } else { 0 } }),
            }
            continue;
        }
        let Some(at) = acl.iter().position(|i| i.grantee == grantee && i.grantor == grantor) else { continue };
        // the grant options it loses: what it granted on with them depends on them
        let losing = acl[at].opts & privs;
        if losing != 0 {
            let rest = acl
                .iter()
                .enumerate()
                .filter(|(k, i)| *k != at && i.grantee == grantee)
                .fold(0, |a, (_, i)| a | i.opts);
            let gone = losing & !rest;
            if gone != 0 && acl.iter().any(|i| i.grantor == grantee && i.privs & gone != 0) {
                if !g.cascade {
                    return Err(SqlError {
                        code: "2BP01",
                        message: "dependent privileges exist".into(),
                        info: Some(Box::new(Violation {
                            hint: Some("Use CASCADE to revoke them too.".into()),
                            ..Violation::default()
                        })),
                    });
                }
                cascade(acl, grantee, gone);
            }
        }
        let Some(at) = acl.iter().position(|i| i.grantee == grantee && i.grantor == grantor) else { continue };
        acl[at].opts &= !privs;
        if !g.option {
            acl[at].privs &= !privs;
        }
        if acl[at].privs == 0 {
            acl.remove(at);
        }
    }
    Ok(())
}

/// Takes `privs` away from what `grantor` granted, and on down.
fn cascade(acl: &mut Vec<AclItem>, grantor: i64, privs: u32) {
    let mut next = vec![];
    for i in acl.iter_mut().filter(|i| i.grantor == grantor && i.privs & privs != 0) {
        let lost = i.opts & privs;
        i.privs &= !privs;
        i.opts &= !privs;
        if lost != 0 {
            next.push((i.grantee, lost));
        }
    }
    acl.retain(|i| i.privs != 0);
    for (g, p) in next {
        let still = acl.iter().filter(|i| i.grantee == g).fold(0, |a, i| a | i.opts);
        if p & !still != 0 {
            cascade(acl, g, p & !still);
        }
    }
}

// ---------------------------------------------------------------- default privileges

/// What default privileges of this pg_default_acl kind (`r`, `S`, `f`, `T`, `n`) are on.
fn defacl_kind(objtype: char) -> Kind {
    match objtype {
        'S' => Kind::Sequence,
        'f' => Kind::Function,
        'T' => Kind::Type,
        'n' => Kind::Schema,
        _ => Kind::Table,
    }
}

/// The row key of role `role`'s default privileges for objects of `objtype` made in `schema`
/// (None: the role's own entry, for every schema).
fn defacl_key(role: i64, objtype: char, schema: Option<&str>) -> String {
    match schema {
        Some(s) => format!("defacl/{role}/{objtype}/{s}"),
        None => format!("defacl/{role}/{objtype}"),
    }
}

/// A default privileges row's role, kind and schema, from its key.
pub(crate) fn defacl_parts(key: &str) -> Option<(i64, char, Option<&str>)> {
    let mut parts = key.strip_prefix("defacl/")?.splitn(3, '/');
    let role = parts.next()?.parse().ok()?;
    let objtype = parts.next()?.chars().next()?;
    Some((role, objtype, parts.next()))
}

/// One row of pg_default_acl: the role whose new objects it is for, their kind, the schema (None:
/// every schema), the ACL, and when it was made (DROP ROLE lists them in that order, as Postgres
/// lists them by oid).
#[derive(Clone, Debug)]
struct DefAcl {
    role: i64,
    objtype: char,
    schema: Option<String>,
    acl: Vec<AclItem>,
    at: i64,
}

impl DefAcl {
    fn key(&self) -> String {
        defacl_key(self.role, self.objtype, self.schema.as_deref())
    }

    /// The write that keeps it.
    fn write(&self) -> (String, Option<String>) {
        let acl: Vec<Json> =
            self.acl.iter().map(|i| serde_json::json!([i.grantee, i.grantor, i.privs, i.opts])).collect();
        (row_key(&self.key()), Some(serde_json::json!({"acl": acl, "at": self.at}).to_string()))
    }

    /// What Postgres's messages call it: `default privileges on new relations belonging to role r in
    /// schema s`.
    fn described(&self, reg: &roles::Registry) -> String {
        let what = match self.objtype {
            'S' => "sequences",
            'f' => "functions",
            'T' => "types",
            'n' => "schemas",
            _ => "relations",
        };
        let mut out = format!("default privileges on new {what} belonging to role {}", reg.name_of(self.role));
        if let Some(s) = &self.schema {
            out.push_str(&format!(" in schema {s}"));
        }
        out
    }
}

/// Every default privileges row of world `v`, oldest first.
fn defaults(v: &View) -> Result<Vec<DefAcl>> {
    let mut out = vec![];
    if !v.core.has_privileges() {
        return Ok(out);
    }
    let prefix = format!("{ACLS}/defacl/");
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        let Some((role, objtype, schema)) = defacl_parts(&k[ACLS.len() + 1..]) else { continue };
        let at = serde_json::from_str::<Json>(&raw).ok().and_then(|j| j["at"].as_i64()).unwrap_or(0);
        let acl = Entry::parse(&raw).acl.unwrap_or_default();
        out.push(DefAcl { role, objtype, schema: schema.map(String::from), acl, at });
    }
    out.sort_by_key(|d| d.at);
    Ok(out)
}

/// An ACL in Postgres's order (aclitemsort): by grantee, grantor, then privileges and their grant
/// options.
fn sorted(mut acl: Vec<AclItem>) -> Vec<AclItem> {
    acl.sort_by_key(|i| (i.grantee, i.grantor, (u64::from(i.opts) << 32) | u64::from(i.privs)));
    acl
}

/// A privilege's name as Postgres's messages write it.
fn privilege_name(p: &str) -> String {
    match p {
        "temporary" | "temp" => "TEMP".into(),
        p => p.to_uppercase(),
    }
}

/// ALTER DEFAULT PRIVILEGES: changes the default privileges of each role (FOR ROLE, or the running
/// one) for objects of its kind, in each schema (IN SCHEMA) or for every schema, as Postgres's
/// SetDefaultACL: a role's own entry starts from the built-in defaults and replaces them, a
/// schema's starts empty and adds to them; one back where it started is no row at all.
fn set_defaults(db: &Db, branch: &str, g: &Grant, d: &Defaults) -> Result<Output> {
    let who = roles::current().map_or_else(|| Who::boot("chronos"), |w| (*w).clone());
    let reg = roles::registry(&db.core)?;
    let who = Who { reg: reg.clone(), ..who };
    // the grantees, then the privileges, then each role, as Postgres checks them
    let grantees: Vec<i64> = g.grantees.iter().map(|s| s.oid(&reg)).collect::<Result<_>>()?;
    let kind = defacl_kind(d.objtype);
    let privs = match &g.privs {
        None if !g.all_cols.is_empty() => return err("0LP01", "default privileges cannot be set for columns"),
        None => kind.all(),
        Some(ps) => {
            let mut all = 0;
            for (p, cols) in ps {
                if !cols.is_empty() {
                    return err("0LP01", "default privileges cannot be set for columns");
                }
                if bit(p) & kind.all() == 0 {
                    let noun = if d.objtype == 'r' { "relation" } else { kind.noun() };
                    return err("0LP01", format!("invalid privilege type {} for {noun}", privilege_name(p)));
                }
                all |= bit(p);
            }
            all
        }
    };
    // FOR ROLE's roles, each looked up and checked in turn, as Postgres does (none: the running role)
    let roles: Vec<Option<&Spec>> = match d.roles.is_empty() {
        true => vec![None],
        false => d.roles.iter().map(Some).collect(),
    };
    db.batch_with::<(), SqlError>(branch, |v| {
        let known = namespaces(v)?;
        let old = defaults(v)?;
        let mut next = old.iter().map(|d| d.at).max().unwrap_or(0) + 1;
        // what this statement has changed so far, by key (None: dropped)
        let mut now: Vec<(String, Option<DefAcl>)> = vec![];
        for spec in &roles {
            let role = match spec {
                Some(r) => r.resolve(&reg)?.oid,
                None => who.oid,
            };
            if spec.is_some() && !who.has_privs_of(role) {
                return err("42501", "permission denied to change default privileges");
            }
            let schemas: Vec<Option<&str>> = match d.schemas.is_empty() {
                true => vec![None],
                false => d.schemas.iter().map(|s| Some(s.as_str())).collect(),
            };
            for schema in schemas {
                if let Some(s) = schema
                    && !known.iter().any(|n| n == s)
                    && !matches!(s, "pg_catalog" | "information_schema")
                {
                    return err("3F000", format!("schema \"{s}\" does not exist"));
                }
                if d.objtype == 'n' && schema.is_some() {
                    return err("0LP01", "cannot use IN SCHEMA clause when using GRANT/REVOKE ON SCHEMAS");
                }
                let key = defacl_key(role, d.objtype, schema);
                let builtin = if schema.is_none() { kind.defaults(role) } else { vec![] };
                let was = match now.iter().find(|(k, _)| *k == key) {
                    Some((_, x)) => x.clone(),
                    None => old.iter().find(|x| x.key() == key).cloned(),
                };
                let at = was.as_ref().map_or(next, |x| x.at);
                let mut acl = was.map_or_else(|| builtin.clone(), |x| x.acl);
                change_acl(&mut acl, &grantees, role, privs, g)?;
                let acl = sorted(acl);
                let entry = (acl != sorted(builtin)).then(|| DefAcl {
                    role,
                    objtype: d.objtype,
                    schema: schema.map(String::from),
                    acl,
                    at,
                });
                if at == next && entry.is_some() {
                    next += 1;
                }
                now.retain(|(k, _)| *k != key);
                now.push((key, entry));
            }
        }
        let writes = now
            .into_iter()
            .map(|(k, e)| match e {
                Some(e) => e.write(),
                None => (row_key(&k), None),
            })
            .collect();
        Ok((writes, ()))
    })?;
    tag("ALTER DEFAULT PRIVILEGES")
}

/// The ACL a new object of default privileges kind `objtype` that `owner` owns, in schema `schema`,
/// starts with (Postgres's get_user_default_acl): the owner's own entry in place of the built-in
/// defaults, with the schema's added; None: the built-in defaults.
fn initial(v: &View, owner: i64, objtype: char, schema: Option<&str>) -> Result<Option<Vec<AclItem>>> {
    if !v.core.has_privileges() {
        return Ok(None);
    }
    let own = entry(v, &defacl_key(owner, objtype, None))?.acl;
    let added = match schema {
        Some(s) => entry(v, &defacl_key(owner, objtype, Some(s)))?.acl,
        None => None,
    };
    if own.is_none() && added.is_none() {
        return Ok(None);
    }
    let builtin = defacl_kind(objtype).defaults(owner);
    let mut acl = own.unwrap_or_else(|| builtin.clone());
    for i in added.unwrap_or_default() {
        match acl.iter_mut().find(|x| x.grantee == i.grantee && x.grantor == i.grantor) {
            Some(x) => {
                x.privs |= i.privs;
                x.opts |= i.opts;
            }
            None => acl.push(i),
        }
    }
    let acl = sorted(acl);
    Ok((acl != sorted(builtin)).then_some(acl))
}

/// DROP OWNED's part in default privileges: drops the roles' own, and takes them out of others'
/// (dropping those left as they started).
fn drop_owned_defaults(v: &View, oids: &[i64]) -> Result<Vec<(String, Option<String>)>> {
    let mut writes = vec![];
    for mut d in defaults(v)? {
        if oids.contains(&d.role) {
            writes.push((row_key(&d.key()), None));
            continue;
        }
        if !d.acl.iter().any(|i| oids.contains(&i.grantee)) {
            continue;
        }
        d.acl.retain(|i| !oids.contains(&i.grantee));
        let builtin = match d.schema {
            Some(_) => vec![],
            None => defacl_kind(d.objtype).defaults(d.role),
        };
        writes.push(match sorted(d.acl.clone()) == sorted(builtin) {
            true => (row_key(&d.key()), None),
            false => d.write(),
        });
    }
    Ok(writes)
}

/// pg_default_acl's rows: oid, role, schema oid (0: every schema), kind and ACL as Postgres writes
/// it, from the catalog's rows (`cat_acls`) by key.
pub(crate) fn default_acl_rows(
    reg: &roles::Registry,
    acls: &HashMap<String, Entry>,
    schema_oid: impl Fn(&str) -> i64,
) -> Vec<(i64, i64, char, String)> {
    let mut keys: Vec<&String> = acls.keys().filter(|k| k.starts_with("defacl/")).collect();
    keys.sort();
    keys.into_iter()
        .filter_map(|k| {
            let (role, objtype, schema) = defacl_parts(k)?;
            let acl = acls[k].acl.as_deref().unwrap_or_default();
            Some((role, schema.map_or(0, &schema_oid), objtype, text(reg, acl)))
        })
        .collect()
}

// ---------------------------------------------------------------- owners

/// Whether statement `st` makes something its role owns: a relation, type, schema, function or
/// extension, run by a role other than the bootstrap superuser (who owns whatever has no row), or
/// by any once the database has privileges (default privileges may give what it makes an ACL).
pub(crate) fn may_make(db: &Db, st: &Stmt) -> bool {
    matches!(
        st,
        Stmt::Create { temp: false, .. }
            | Stmt::CreateAs { temp: false, .. }
            | Stmt::CreateView { .. }
            | Stmt::CreateSequence { .. }
            | Stmt::CreateType { .. }
            | Stmt::CreateDomain { .. }
            | Stmt::CreateSchema { owner: None, .. }
            | Stmt::CreateFunction(..)
            | Stmt::CreateExtension { .. }
            | Stmt::Alter { .. }
    ) && roles::current().is_some_and(|w| w.oid != BOOTSTRAP || db.core.has_privileges())
}

/// What a statement is about to make (see `to_make`): its row's key, and the kind of default
/// privileges it starts with (None: an extension, which has none).
pub(crate) struct Made {
    key: String,
    objtype: Option<char>,
    /// ALTER TABLE's new serial and identity columns: the table, whose owner owns their sequences,
    /// and those columns
    serials_of: Option<(String, Vec<String>)>,
}

/// What statement `st` is about to make, when it isn't there yet: the running role will own it.
pub(crate) fn to_make(v: &View, st: &Stmt) -> Result<Option<Made>> {
    if roles::current().is_none() {
        return Ok(None);
    }
    let made = |key: String, objtype: char| Made { key, objtype: Some(objtype), serials_of: None };
    let rel = |name: &str, objtype: char| -> Result<Option<Made>> {
        Ok((relation_kind(v, name)?.is_none()).then(|| made(key_of(Kind::Table, name), objtype)))
    };
    Ok(match st {
        Stmt::Create { table, temp: false, .. } | Stmt::CreateAs { table, temp: false, .. } => rel(table, 'r')?,
        Stmt::CreateView { name, .. } => rel(name, 'r')?,
        Stmt::CreateSequence { name, .. } => rel(name, 'S')?,
        Stmt::CreateType { name, .. } | Stmt::CreateDomain { name, .. } => {
            v.get(&type_key(name))?.is_none().then(|| made(key_of(Kind::Type, name), 'T'))
        }
        Stmt::CreateSchema { name, owner: None, .. } => {
            (!namespaces(v)?.contains(name)).then(|| made(key_of(Kind::Schema, name), 'n'))
        }
        Stmt::CreateFunction(f, _, _) => {
            v.get(&f.key())?.is_none().then(|| made(key_of(Kind::Function, &f.sig()), 'f'))
        }
        Stmt::CreateExtension { name, .. } => v.get(&pgcrypto::key(name))?.is_none().then(|| Made {
            key: key_of(Kind::Extension, name),
            objtype: None,
            serials_of: None,
        }),
        // a serial or identity column added (or a column made an identity one): its new sequence
        Stmt::Alter { table, changes, .. } => {
            let cols: Vec<String> = changes
                .iter()
                .filter_map(|c| match c {
                    Alter::AddColumn(c, _) if c.auto == Some(Auto::Serial) || c.identity.is_some() => {
                        Some(c.name.clone())
                    }
                    Alter::Identity(c, Some(_), false, _) => Some(c.clone()),
                    _ => None,
                })
                .collect();
            match cols.is_empty() {
                true => None,
                false => need_table(v, table).ok().map(|t| Made {
                    key: String::new(),
                    objtype: None,
                    serials_of: Some((t.name, cols)),
                }),
            }
        }
        _ => None,
    })
}

/// Records that the running statement's role owns what it made (`to_make`'s), and the ACL its
/// default privileges give it; a table's serial columns' sequences get theirs too.
pub(crate) fn made(db: &Db, branch: &str, m: &Made) -> Result<()> {
    let Some(owner) = roles::current().map(|w| w.oid) else { return Ok(()) };
    if let Some((table, cols)) = &m.serials_of {
        return db.batch_with::<(), SqlError>(branch, |v| Ok((serials_made(v, table, cols)?, ()))).map(drop);
    }
    db.batch_with::<(), SqlError>(branch, |v| {
        let mut writes = vec![];
        let (kind, name) = m.key.split_once('/').unwrap_or_default();
        let schema = match kind {
            "schema" => None,
            _ => Some(schema_part(name.split('(').next().unwrap_or(name))),
        };
        let acl = match m.objtype {
            Some(t) => initial(v, owner, t, schema)?,
            None => None,
        };
        let e = Entry { owner: (owner != BOOTSTRAP).then_some(owner), acl };
        if e.raw().is_some() {
            writes.push(write(&m.key, &e));
        }
        if m.objtype == Some('r')
            && let Some(t) = load(v, name)?
        {
            for c in t.cols.iter().filter(|c| sysinfo::own_counter(v, c)) {
                let seq = implicit_sequence_name(name, &c.name);
                if let Some(acl) = initial(v, owner, 'S', schema)? {
                    // (its owner is its table's)
                    writes.push(write(&key_of(Kind::Sequence, &seq), &Entry { owner: None, acl: Some(acl) }));
                }
            }
        }
        Ok((writes, ()))
    })?;
    Ok(())
}

/// The ACLs of the sequences ALTER TABLE just made for table `table`'s columns `cols` (new serial and
/// identity columns): its owner's default privileges for sequences, as Postgres makes them for the
/// table's owner whoever alters it. Its other columns' sequences are left as they are.
fn serials_made(v: &View, table: &str, cols: &[String]) -> Result<Vec<(String, Option<String>)>> {
    let mut writes = vec![];
    let Some(t) = load(v, table)? else { return Ok(writes) };
    let owner = entry(v, &key_of(Kind::Table, table))?.owner.unwrap_or(BOOTSTRAP);
    for c in t.cols.iter().filter(|c| cols.contains(&c.name) && sysinfo::own_counter(v, c)) {
        let key = key_of(Kind::Sequence, &implicit_sequence_name(table, &c.name));
        if v.get(&row_key(&key))?.is_none()
            && let Some(acl) = initial(v, owner, 'S', Some(schema_part(table)))?
        {
            writes.push(write(&key, &Entry { owner: None, acl: Some(acl) }));
        }
    }
    Ok(writes)
}

/// CREATE SCHEMA ... AUTHORIZATION role: the schema's owner, and the ACL its default privileges
/// give it.
pub(crate) fn set_owner(db: &Db, branch: &str, key: &str, owner: i64) -> Result<()> {
    db.batch_with::<(), SqlError>(branch, |v| {
        let mut e = entry(v, key)?;
        e.owner = (owner != BOOTSTRAP).then_some(owner);
        e.acl = initial(v, owner, 'n', None)?;
        Ok((vec![write(key, &e)], ()))
    })?;
    Ok(())
}

/// ALTER ... OWNER TO: what it is on.
#[derive(Clone, Debug)]
pub(crate) enum Owned {
    Rel(&'static str, String),
    Schema(String),
    Routine(String, Option<Vec<Type>>, bool),
    /// a type, or with true ALTER DOMAIN's domain: its stored name
    Type(String, bool),
    /// a large object, by its oid
    LargeObject(i64),
}

/// The kind `Owned::Rel` has for ALTER TABLE IF EXISTS ... OWNER TO: a table, skipped if it isn't there.
pub(super) const TABLE_IF_EXISTS: &str = "table if exists";

/// ALTER TABLE | VIEW | SEQUENCE | SCHEMA | FUNCTION ... OWNER TO role: the owner's privileges in
/// the ACL become the new owner's, as Postgres's aclnewowner does.
pub(super) fn alter_owner(db: &Db, branch: &str, what: &Owned, to: &Spec) -> Result<Output> {
    // ALTER TABLE IF EXISTS ... OWNER TO: a table that isn't there is skipped, with a notice
    if let Owned::Rel(TABLE_IF_EXISTS, n) = what
        && rel(&db.view_tx(branch)?, n)?.is_none()
    {
        notice("NOTICE", format!("relation \"{}\" does not exist, skipping", shown_name(n)));
        return Ok(Output::tag("ALTER TABLE"));
    }
    let who = roles::current().map_or_else(|| Who::boot("chronos"), |w| (*w).clone());
    let reg = roles::registry(&db.core)?;
    let who = Who { reg: reg.clone(), ..who };
    let new = to.resolve(&reg)?;
    let tag_name = match what {
        Owned::Rel(TABLE_IF_EXISTS, _) => "ALTER TABLE".into(),
        Owned::Rel(kind, _) => format!("ALTER {}", kind.to_uppercase()),
        Owned::Schema(_) => "ALTER SCHEMA".into(),
        Owned::Routine(_, _, p) => {
            if *p {
                "ALTER PROCEDURE".into()
            } else {
                "ALTER FUNCTION".into()
            }
        }
        Owned::Type(_, domain) => if *domain { "ALTER DOMAIN" } else { "ALTER TYPE" }.into(),
        Owned::LargeObject(_) => "ALTER LARGE OBJECT".into(),
    };
    db.batch_with::<(), SqlError>(branch, |v| {
        let (obj, schema_name) = match what {
            Owned::Rel(_, n) => (need_rel(v, n)?, schema_part(n).to_string()),
            Owned::Schema(n) => {
                if !namespaces(v)?.contains(n) {
                    return err("3F000", format!("schema \"{n}\" does not exist"));
                }
                (schema(v, n)?, String::new())
            }
            Owned::Routine(n, types, _) => {
                let Some(f) = functions::find_routine(v, n, types.as_deref())? else {
                    return err(NO_FUNCTION, format!("function {} does not exist", written(n)));
                };
                (func(v, &f)?, schema_part(&f.name).to_string())
            }
            Owned::Type(n, domain) => {
                let Some(o) = type_obj(v, n)? else {
                    return err("42704", format!("type \"{n}\" does not exist"));
                };
                if *domain && !is_domain(v, n)? {
                    return err("42809", format!("{} is not a domain", o.name));
                }
                (o, schema_part(n).to_string())
            }
            Owned::LargeObject(oid) => {
                if !super::largeobj::exists_in(v, *oid)? {
                    return err("42704", format!("large object {oid} does not exist"));
                }
                (large_object(v, *oid)?, String::new())
            }
        };
        if obj.owner == new.oid {
            return Ok((vec![], ()));
        }
        if !who.superuser {
            if !who.has_privs_of(obj.owner) {
                return Err(match &obj.kind {
                    Kind::Function | Kind::Procedure => SqlError {
                        code: "42501",
                        message: format!("must be owner of {} {}", obj.kind.noun(), obj.name),
                        info: None,
                    },
                    _ => obj.not_owner(),
                });
            }
            if !who.reg.can_set(who.oid, new.oid) {
                return err("42501", format!("must be able to SET ROLE \"{}\"", new.name));
            }
            if !schema_name.is_empty() {
                let s = schema(v, &schema_name)?;
                if !s.has(&who.as_role(new.oid), CREATE) {
                    return Err(s.denied());
                }
            }
        }
        let mut writes = vec![];
        let e = entry(v, &obj.key)?;
        let acl = e.acl.map(|acl| new_owner(acl, obj.owner, new.oid));
        writes.push(write(&obj.key, &Entry { owner: (new.oid != BOOTSTRAP).then_some(new.oid), acl }));
        // a table's columns' grants name the owner as grantor too, and its serial columns' sequences
        // are its owner's
        if matches!(obj.kind, Kind::Table | Kind::View | Kind::MatView) {
            let table = &obj.key[4..];
            for (c, acl) in columns(v, table)? {
                if !acl.is_empty() {
                    writes.push(col_write(table, &c, new_owner(acl, obj.owner, new.oid)));
                }
            }
            writes.extend(serials_new_owner(v, table, obj.owner, new.oid)?);
        }
        Ok((writes, ()))
    })?;
    tag(tag_name)
}

/// ALTER FUNCTION | PROCEDURE | ROUTINE ... RENAME TO, SET SCHEMA or attributes, as Postgres's AlterFunction and
/// its generic rename and namespace changes do them: on a routine its role owns (or a superuser), one of a
/// procedure's attributes being SECURITY or a setting, LEAKPROOF a superuser's to give. A rename or a move to
/// another schema is refused where a routine of the name and argument types is there already (42723).
pub(super) fn alter_routine(
    db: &Db,
    branch: &str,
    name: &str,
    types: Option<&[Type]>,
    kind: functions::RoutineKind,
    action: &functions::RoutineAlter,
) -> Result<Output> {
    use functions::{RoutineAlter, RoutineAttr};
    let who = roles::current().map_or_else(|| Who::boot("chronos"), |w| (*w).clone());
    db.batch_with::<(), SqlError>(branch, |v| {
        let f = functions::lookup_routine(v, name, types, kind)?;
        let o = func(v, &f)?;
        if !who.has_privs_of(o.owner) {
            let noun = if f.procedure { "procedure" } else { "function" };
            return err("42501", format!("must be owner of {noun} {}", written(name)));
        }
        let mut writes = vec![];
        match action {
            RoutineAlter::Attrs(attrs) => {
                let mut j = serde_json::from_str::<Json>(&v.get(&f.key())?.unwrap_or_default()).unwrap_or_default();
                let mut config = f.config.clone();
                for (a, at) in attrs {
                    if f.procedure && !a.procedures_have() {
                        return Err(SqlError {
                            code: "42P13",
                            message: "invalid attribute in procedure definition".into(),
                            info: Some(Box::new(Violation { position: Some(*at), ..Violation::default() })),
                        });
                    }
                    let flag = |j: &mut Json, k: &str, on: bool| match on {
                        true => j[k] = Json::Bool(true),
                        false => {
                            if let Some(m) = j.as_object_mut() {
                                m.remove(k);
                            }
                        }
                    };
                    match a {
                        RoutineAttr::Volatility(x) => j["volatility"] = Json::from(x.name()),
                        RoutineAttr::Strict(on) => j["strict"] = Json::Bool(*on),
                        RoutineAttr::Definer(on) => flag(&mut j, "definer", *on),
                        RoutineAttr::Leakproof(on) => {
                            if *on && !who.superuser {
                                return err("42501", "only superuser can define a leakproof function");
                            }
                            flag(&mut j, "leakproof", *on);
                        }
                        RoutineAttr::Cost(c) => {
                            if *c <= 0.0 {
                                return err("22023", "COST must be positive");
                            }
                            j["cost"] = Json::from(*c);
                        }
                        RoutineAttr::Rows(r) => {
                            if *r <= 0.0 {
                                return err("22023", "ROWS must be positive");
                            }
                            if !f.ret.sets() {
                                return err("22023", "ROWS is not applicable when function does not return a set");
                            }
                            j["rows"] = Json::from(*r);
                        }
                        RoutineAttr::Parallel(p) => j["parallel"] = Json::String(p.to_string()),
                        RoutineAttr::Support => {}
                        RoutineAttr::Set(n, values) => {
                            if let Some(values) = values {
                                match config.iter_mut().find(|(c, _)| c == n) {
                                    Some(c) => c.1 = values.clone(),
                                    None => config.push((n.clone(), values.clone())),
                                }
                            }
                        }
                        RoutineAttr::Reset(Some(n)) => config.retain(|(c, _)| c != n),
                        RoutineAttr::Reset(None) => config.clear(),
                    }
                }
                match config.is_empty() {
                    true => {
                        if let Some(m) = j.as_object_mut() {
                            m.remove("config");
                        }
                    }
                    false => j["config"] = serde_json::json!(config),
                }
                writes.push((f.key(), Some(j.to_string())));
            }
            RoutineAlter::Rename(_) | RoutineAlter::Schema(_) => {
                let to = match action {
                    RoutineAlter::Rename(n) => qualify(schema_part(&f.name), n),
                    RoutineAlter::Schema(s) => {
                        if !namespaces(v)?.contains(s) {
                            return err("3F000", format!("schema \"{s}\" does not exist"));
                        }
                        qualify(s, rel_part(&f.name))
                    }
                    RoutineAlter::Attrs(_) => unreachable!("matched above"),
                };
                if to == f.name {
                    return Ok((vec![], ()));
                }
                // (the schema it goes to, or is renamed in, takes CREATE from whoever puts it there)
                if !who.superuser {
                    let s = schema(v, schema_part(&to))?;
                    if !s.has(&who, CREATE) {
                        return Err(s.denied());
                    }
                }
                let raw = functions::edited(&f, |j| j["name"] = Json::String(to.clone()));
                let moved = functions::Function::from_json(&serde_json::from_str(&raw).unwrap_or_default())?;
                if v.get(&moved.key())?.is_some() {
                    return err(
                        "42723",
                        format!(
                            "function {}({}) already exists in schema \"{}\"",
                            rel_part(&to),
                            f.args.iter().map(|a| a.1.name()).collect::<Vec<_>>().join(", "),
                            schema_part(&to)
                        ),
                    );
                }
                writes.push((f.key(), None));
                writes.push((moved.key(), Some(raw)));
                // (its triggers call it by name)
                for mut t in schemas(v)? {
                    if t.triggers.iter().any(|x| x.func == f.name) {
                        for x in t.triggers.iter_mut().filter(|x| x.func == f.name) {
                            x.func = to.clone();
                        }
                        writes.push((schema_key(&t.name), Some(schema_json(&t))));
                    }
                }
            }
        }
        Ok((writes, ()))
    })?;
    functions::forget_parsed();
    tag(kind.tag())
}

/// An ACL with `old` (owner) replaced by `new` as grantor and grantee, items merged.
fn new_owner(acl: Vec<AclItem>, old: i64, new: i64) -> Vec<AclItem> {
    let mut out: Vec<AclItem> = vec![];
    for mut i in acl {
        if i.grantee == old {
            i.grantee = new;
        }
        if i.grantor == old {
            i.grantor = new;
        }
        match out.iter_mut().find(|x| x.grantee == i.grantee && x.grantor == i.grantor) {
            Some(x) => {
                x.privs |= i.privs;
                x.opts |= i.opts;
            }
            None => out.push(i),
        }
    }
    out
}

/// What role `oid` owns or holds privileges on in world `v`, as DROP ROLE's detail lists them
/// (`owner of table t`, `privileges for schema s`).
fn depending(v: &View, reg: &roles::Registry, oid: i64) -> Result<Vec<(u8, String)>> {
    let mut out = vec![];
    let names = |key: &str| -> Option<(u8, String)> {
        // (the database's row is `db`, with nothing after it)
        let (kind, rest) = key.split_once('/').unwrap_or((key, ""));
        Some(match kind {
            "schema" => (0, format!("schema {rest}")),
            // (default privileges come next: see below)
            "rel" => {
                let noun = match relation_kind(v, rest).ok().flatten() {
                    Some("view") => "view",
                    Some("sequence") => "sequence",
                    _ if load(v, rest).ok().flatten().is_some_and(|t| t.matview.is_some()) => "materialized view",
                    _ if sysinfo::implicit_sequence(v, rest).ok().flatten().is_some() => "sequence",
                    _ => "table",
                };
                (1, format!("{noun} {}", shown_name(rest)))
            }
            "col" => {
                let (t, c) = rest.rsplit_once('/')?;
                (2, format!("column {c} of table {}", shown_name(t)))
            }
            "func" => {
                let (name, args) = rest.split_once('(')?;
                (3, format!("function {}({args}", shown_name(name)))
            }
            "type" => (4, format!("type {}", shown_name(rest))),
            "ext" => (4, format!("extension {rest}")),
            "db" => (5, format!("database {}", database_name(v))),
            "fdw" => (5, format!("foreign-data wrapper {rest}")),
            "server" => (5, format!("server {rest}")),
            "lo" => (5, format!("large object {rest}")),
            _ => return None,
        })
    };
    let mut keys: Vec<(String, Entry)> = rows(v)?.into_iter().collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, e) in keys {
        let Some((rank, name)) = names(&k) else { continue };
        // (a schema first; everything else after default privileges)
        let rank = if rank == 0 { 0 } else { rank + 1 };
        if e.owner == Some(oid) {
            out.push((rank, format!("owner of {name}")));
        } else if e.acl.as_ref().is_some_and(|a| a.iter().any(|i| i.grantee == oid || i.grantor == oid)) {
            out.push((rank, format!("privileges for {name}")));
        }
    }
    // default privileges, in the order they were made (Postgres's oid order)
    for d in defaults(v)? {
        if d.role == oid {
            out.push((1, format!("owner of {}", d.described(reg))));
        } else if d.acl.iter().any(|i| i.grantee == oid) {
            out.push((1, format!("privileges for {}", d.described(reg))));
        }
    }
    // (the user mappings it owns)
    out.extend(super::foreign::depending(v, reg, oid)?.into_iter().map(|n| (6, n)));
    out.extend(super::rls::depending(v, oid)?.into_iter().map(|n| (7, n)));
    // (the publications and statistics objects it owns, whose owner is in their own rows)
    out.extend(super::publication::owned_by(v, oid)?.into_iter().map(|n| (8, format!("owner of publication {n}"))));
    out.extend(
        (super::statistics::owned_by(v, oid)?.into_iter())
            .map(|n| (8, format!("owner of statistics object {}", shown_name(&n)))),
    );
    out.sort_by_key(|(r, _)| *r);
    Ok(out)
}

/// Every world's objects role `oid` owns or has privileges on (DROP ROLE refuses while there are):
/// the session's world's by name, the others' by count, as Postgres lists other databases'.
pub(crate) fn dependents(db: &Db, oid: i64) -> Result<Vec<String>> {
    let reg = roles::registry(&db.core)?;
    let mut out = vec![];
    let here = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| (c.world.clone(), c.home.clone()))));
    let mut worlds: Vec<String> =
        db.core.worlds().into_iter().map(|w| w.name).filter(|w| !w.starts_with("_tx_")).collect();
    if let Some((world, home)) = &here {
        worlds.retain(|w| w != world && w != home);
        out.extend(depending(&db.core.view_tx(world)?, &reg, oid)?.into_iter().map(|(_, n)| n));
    }
    for w in worlds {
        let Ok(v) = db.core.view(&w) else { continue };
        let n = depending(&v, &reg, oid)?.len();
        if n > 0 {
            out.push(format!("{n} object{} in database {w}", if n == 1 { "" } else { "s" }));
        }
    }
    Ok(out)
}

/// 42501 with a DETAIL line.
fn denied_with(message: &str, detail: String) -> SqlError {
    SqlError {
        code: "42501",
        message: message.into(),
        info: Some(Box::new(Violation { detail: Some(detail), ..Violation::default() })),
    }
}

/// DROP OWNED BY roles: drops what they own in this world and takes away their privileges here.
pub(super) fn drop_owned(db: &Db, branch: &str, roles: &[Spec], cascade: bool) -> Result<Output> {
    let reg = roles::registry(&db.core)?;
    let who = roles::current().map_or_else(|| Who::boot("chronos"), |w| (*w).clone());
    let named: Vec<(i64, String)> =
        roles.iter().map(|r| r.resolve(&reg).map(|r| (r.oid, r.name))).collect::<Result<_>>()?;
    for (o, n) in &named {
        if !who.superuser && !reg.has_privs_of(who.oid, *o) {
            return Err(denied_with(
                "permission denied to drop objects",
                format!("Only roles with privileges of role \"{n}\" may drop objects owned by it."),
            ));
        }
    }
    let oids: Vec<i64> = named.into_iter().map(|(o, _)| o).collect();
    let v = db.view_tx(branch)?;
    let all = rows(&v)?;
    // what they own, dropped in an order that drops what depends first
    let mut stmts: Vec<(u8, String)> = vec![];
    for (k, e) in &all {
        if !e.owner.is_some_and(|o| oids.contains(&o)) {
            continue;
        }
        let Some((kind, rest)) = k.split_once('/') else { continue };
        let tail = if cascade { " cascade" } else { "" };
        match kind {
            "rel" => {
                let what = match relation_kind(&v, rest)? {
                    Some("view") => "view",
                    Some("sequence") => "sequence",
                    Some(_) if load(&v, rest)?.is_some_and(|t| t.matview.is_some()) => "materialized view",
                    Some(_) if super::foreign::rel_noun(&v, rest) == "foreign table" => "foreign table",
                    Some(_) => "table",
                    None => continue,
                };
                stmts.push((
                    if what == "view" { 1 } else { 2 },
                    format!("drop {what} if exists {}{tail}", ident_rel(rest)),
                ));
            }
            "func" => {
                let (name, args) = rest.split_once('(').unwrap_or((rest, ")"));
                let p = functions::find_routine(&v, name, None).ok().flatten().is_some_and(|f| f.procedure);
                stmts.push((
                    0,
                    format!(
                        "drop {} if exists {}({args}{tail}",
                        if p { "procedure" } else { "function" },
                        ident_rel(name)
                    ),
                ));
            }
            "type" => stmts.push((3, format!("drop type if exists {}{tail}", ident_rel(rest)))),
            "ext" => stmts.push((3, format!("drop extension if exists {}{tail}", ident(rest)))),
            "server" => stmts.push((3, format!("drop server if exists {}{tail}", ident(rest)))),
            "fdw" => stmts.push((4, format!("drop foreign data wrapper if exists {}{tail}", ident(rest)))),
            "schema" => stmts.push((4, format!("drop schema if exists {}{tail}", ident(rest)))),
            _ => {}
        }
    }
    // (and their publications and statistics objects, which keep their owners themselves)
    for o in &oids {
        for p in super::publication::owned_by(&v, *o)? {
            stmts.push((0, format!("drop publication if exists {}", ident(&p))));
        }
        for s in super::statistics::owned_by(&v, *o)? {
            stmts.push((0, format!("drop statistics if exists {}", ident_rel(&s))));
        }
    }
    // (and their user mappings, which they own: see `foreign`)
    for (user, server) in super::foreign::mappings_of(&v, &oids)? {
        stmts.push((
            0,
            format!("drop user mapping if exists for {} server {}", ident(&reg.name_of(user)), ident(&server)),
        ));
    }
    stmts.sort();
    drop(v);
    // (what depends on more than one of them is refused as Postgres refuses it, for them all)
    let many = stmts.len() > 1;
    for (_, sql) in stmts {
        for st in parse(&sql)? {
            st.execute(db, branch, &[], &[]).map_err(|mut e| {
                if many && e.code == "2BP01" {
                    e.message = "cannot drop desired object(s) because other objects depend on them".into();
                }
                e
            })?;
        }
    }
    // and their privileges on what's left, with what they granted on from their grant options (as
    // Postgres's DROP OWNED revokes them, CASCADE or not)
    db.batch_with::<(), SqlError>(branch, |v| {
        // (default privileges: theirs dropped, others' without them)
        let mut writes = drop_owned_defaults(v, &oids)?;
        for (k, mut e) in rows(v)? {
            if k.starts_with("defacl/") {
                continue;
            }
            // (what the object's owner granted them: a revoke made as the owner, as Postgres's is)
            let owner = owner_of(v, &k, &e)?;
            let Some(acl) = &mut e.acl else { continue };
            let before = acl.clone();
            acl.retain(|i| !(oids.contains(&i.grantee) && i.grantor == owner));
            for o in &oids {
                let lost = before.iter().filter(|i| i.grantee == *o && i.grantor == owner).fold(0, |a, i| a | i.opts);
                let lost = lost & !acl.iter().filter(|i| i.grantee == *o).fold(0, |a, i| a | i.opts);
                if lost != 0 {
                    self::cascade(acl, *o, lost);
                }
            }
            if *acl != before {
                if k.starts_with("col/") && acl.is_empty() {
                    e.acl = None;
                }
                writes.push(write(&k, &e));
            }
        }
        Ok((writes, ()))
    })?;
    // and the policies they are named in
    super::rls::drop_owned(db, branch, &oids)?;
    // and the grants of roles they made
    roles::drop_grants_by(db, &oids)?;
    tag("DROP OWNED")
}

/// The owner of the object row `key` is on (a column's: its table's).
fn owner_of(v: &View, key: &str, e: &Entry) -> Result<i64> {
    let (kind, rest) = key.split_once('/').unwrap_or((key, ""));
    Ok(match kind {
        "rel" => rel(v, rest)?.map_or(BOOTSTRAP, |o| o.owner),
        "col" => rel(v, rest.rsplit_once('/').map_or(rest, |(t, _)| t))?.map_or(BOOTSTRAP, |o| o.owner),
        "schema" => schema(v, rest)?.owner,
        _ => e.owner.unwrap_or(BOOTSTRAP),
    })
}

/// REASSIGN OWNED BY roles TO role: what they own in this world is the new role's.
pub(super) fn reassign(db: &Db, branch: &str, from: &[Spec], to: &Spec) -> Result<Output> {
    let reg = roles::registry(&db.core)?;
    let who = roles::current().map_or_else(|| Who::boot("chronos"), |w| (*w).clone());
    let named: Vec<(i64, String)> =
        from.iter().map(|r| r.resolve(&reg).map(|r| (r.oid, r.name))).collect::<Result<_>>()?;
    let what = "permission denied to reassign objects";
    for (o, n) in &named {
        if !who.superuser && !reg.has_privs_of(who.oid, *o) {
            let detail = format!("Only roles with privileges of role \"{n}\" may reassign objects owned by it.");
            return Err(denied_with(what, detail));
        }
    }
    let olds: Vec<i64> = named.into_iter().map(|(o, _)| o).collect();
    let to = to.resolve(&reg)?;
    let new = to.oid;
    if !who.superuser && !reg.has_privs_of(who.oid, new) {
        let detail = format!("Only roles with privileges of role \"{}\" may reassign objects to it.", to.name);
        return Err(denied_with(what, detail));
    }
    db.batch_with::<(), SqlError>(branch, |v| {
        let all = rows(v)?;
        let mut writes = vec![];
        // (a table's: its columns' grants name its owner too, as grantor)
        let mut tables = HashMap::new();
        for (k, e) in &all {
            let Some(old) = e.owner.filter(|o| olds.contains(o)) else { continue };
            let mut e = e.clone();
            e.owner = (new != BOOTSTRAP).then_some(new);
            e.acl = e.acl.map(|a| new_owner(a, old, new));
            writes.push(write(k, &e));
            if let Some(t) = k.strip_prefix("rel/") {
                tables.insert(t.to_string(), old);
            }
        }
        for (k, e) in &all {
            let Some((t, c)) = k.strip_prefix("col/").and_then(|r| r.rsplit_once('/')) else { continue };
            if let (Some(old), Some(acl)) = (tables.get(t), &e.acl) {
                writes.push(col_write(t, c, new_owner(acl.clone(), *old, new)));
            }
        }
        for (t, old) in &tables {
            writes.extend(serials_new_owner(v, t, *old, new)?);
        }
        // (publications and statistics objects keep their owners in their own rows)
        writes.extend(super::publication::reassigned(v, &olds, new)?);
        writes.extend(super::statistics::reassigned(v, &olds, new)?);
        Ok((writes, ()))
    })?;
    tag("REASSIGN OWNED")
}

/// The writes that give table `table`'s serial columns' sequences, which are its owner's, owner
/// `new` in their grants in place of `old` (those with an owner of their own are left as they are).
fn serials_new_owner(v: &View, table: &str, old: i64, new: i64) -> Result<Vec<(String, Option<String>)>> {
    let mut writes = vec![];
    let Some(t) = load(v, table)? else { return Ok(writes) };
    for c in t.cols.iter().filter(|c| sysinfo::own_counter(v, c)) {
        let key = key_of(Kind::Sequence, &implicit_sequence_name(table, &c.name));
        let e = entry(v, &key)?;
        if let (None, Some(acl)) = (e.owner, e.acl) {
            writes.push(write(&key, &Entry { owner: None, acl: Some(new_owner(acl, old, new)) }));
        }
    }
    Ok(writes)
}

// ---------------------------------------------------------------- renames and drops

/// The rows a statement is about to move (renames), as (from, to) keys, worked out before it runs.
pub(crate) fn moves(v: &View, st: &Stmt) -> Result<Vec<(String, String)>> {
    if !v.core.has_privileges() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    match st {
        Stmt::Alter { table, changes, .. } => {
            let from = need_table(v, table).map(|t| t.name).unwrap_or_else(|_| table.clone());
            // its serial and identity columns, whose sequences are named after the table and the column
            let serials: Vec<String> = match load(v, &from)? {
                Some(t) => t.cols.iter().filter(|c| sysinfo::own_counter(v, c)).map(|c| c.name.clone()).collect(),
                None => vec![],
            };
            let seq = |t: &str, c: &str| key_of(Kind::Sequence, &implicit_sequence_name(t, c));
            for c in changes {
                match c {
                    Alter::RenameTable(_) | Alter::SetSchema(_) => {
                        let to = match c {
                            Alter::RenameTable(to) => qualify(schema_part(&from), to),
                            Alter::SetSchema(s) => qualify(s, rel_part(&from)),
                            _ => unreachable!(),
                        };
                        out.push((format!("rel/{from}"), format!("rel/{to}")));
                        for k in rows(v)?.into_keys() {
                            if let Some(rest) = k.strip_prefix(&format!("col/{from}/")) {
                                out.push((k.clone(), format!("col/{to}/{rest}")));
                            }
                        }
                        out.extend(serials.iter().map(|c| (seq(&from, c), seq(&to, c))));
                    }
                    Alter::RenameColumn(a, b) => {
                        out.push((format!("col/{from}/{a}"), format!("col/{from}/{b}")));
                        if serials.contains(a) {
                            out.push((seq(&from, a), seq(&from, b)));
                        }
                    }
                    _ => {}
                }
            }
        }
        Stmt::RenameView { name, to, .. } => {
            let from = need_table(v, name).map(|t| t.name).unwrap_or_else(|_| name.clone());
            let to = qualify(schema_part(&from), to);
            out.push((format!("rel/{from}"), format!("rel/{to}")));
            // (and its columns' grants, as a table's)
            for k in rows(v)?.into_keys() {
                if let Some(rest) = k.strip_prefix(&format!("col/{from}/")) {
                    out.push((k.clone(), format!("col/{to}/{rest}")));
                }
            }
        }
        Stmt::AlterType { name, change: TypeChange::Rename(to) } => {
            // (to the name the rename stores it under)
            out.push((key_of(Kind::Type, name), key_of(Kind::Type, to)));
        }
        Stmt::AlterRoutine { name, types, kind, action } => {
            let to = match action {
                functions::RoutineAlter::Rename(n) => Some((None, n)),
                functions::RoutineAlter::Schema(s) => Some((Some(s), &String::new())),
                functions::RoutineAlter::Attrs(_) => None,
            };
            if let (Some((schema, new)), Ok(f)) = (to, functions::lookup_routine(v, name, types.as_deref(), *kind)) {
                let moved = match schema {
                    Some(s) => qualify(s, rel_part(&f.name)),
                    None => qualify(schema_part(&f.name), new),
                };
                let args = f.args.iter().map(|a| a.1.name()).collect::<Vec<_>>().join(", ");
                out.push((key_of(Kind::Function, &f.sig()), key_of(Kind::Function, &format!("{moved}({args})"))));
            }
        }
        _ => {}
    }
    Ok(out)
}

/// After a statement that may have dropped or renamed objects: moves the rows it renamed and drops
/// those whose object is gone.
pub(crate) fn after(db: &Db, branch: &str, moves: Vec<(String, String)>) -> Result<()> {
    if !db.core.has_privileges() {
        return Ok(());
    }
    let v = db.core.view_tx(branch)?;
    let all = rows(&v)?;
    if all.is_empty() {
        return Ok(());
    }
    let mut now: HashMap<String, Entry> = all.clone();
    for (from, to) in moves {
        if let Some(e) = now.remove(&from) {
            now.insert(to, e);
        }
    }
    let funcs: HashSet<String> = {
        let mut s = HashSet::new();
        for r in v.prefix(&format!("{}/", functions::FUNCTIONS))? {
            s.insert(functions::Function::from_raw(&r?.1)?.sig());
        }
        s
    };
    let schemas = namespaces(&v)?;
    let live = |k: &str| -> Result<bool> {
        let Some((kind, rest)) = k.split_once('/') else { return Ok(true) };
        Ok(match kind {
            "rel" => relation_kind(&v, rest)?.is_some() || sysinfo::implicit_sequence(&v, rest)?.is_some(),
            "col" => match rest.rsplit_once('/') {
                Some((t, c)) => {
                    load(&v, t)?.is_some_and(|t| t.cols.iter().any(|x| x.name == c))
                        || view_def(&v, t)?.is_some_and(|d| d.cols.iter().any(|x| x == c))
                }
                None => false,
            },
            "schema" => schemas.iter().any(|s| s == rest),
            "func" => funcs.contains(rest),
            "type" => v.get(&format!("_sonos_types/{rest}"))?.is_some(),
            "ext" => v.get(&pgcrypto::key(rest))?.is_some(),
            // a schema's default privileges go with it
            "defacl" => match defacl_parts(k).and_then(|(_, _, s)| s) {
                Some(s) => schemas.iter().any(|n| n == s),
                None => true,
            },
            _ => true,
        })
    };
    let mut keep = HashMap::new();
    for (k, e) in now {
        if live(&k)? {
            keep.insert(k, e);
        }
    }
    let mut writes: Vec<(String, Option<String>)> = vec![];
    writes.extend(all.keys().filter(|k| !keep.contains_key(*k)).map(|k| (row_key(k), None)));
    writes.extend(keep.iter().filter(|(k, e)| all.get(*k) != Some(*e)).map(|(k, e)| (row_key(k), e.raw())));
    drop(v);
    if writes.is_empty() {
        return Ok(());
    }
    db.batch_with::<(), SqlError>(branch, |_| Ok((writes, ())))?;
    Ok(())
}

// ---------------------------------------------------------------- has_*_privilege

/// has_table_privilege and its kin: `kind` is what `sysinfo` calls the object (table, column,
/// any_column, sequence, schema, function, database), `who` the role asked about, then the object as
/// given, a column, and the privileges' text (any one of them will do).
pub(crate) fn has_privilege(
    core: &crate::Core,
    world: &str,
    kind: &str,
    who: &Who,
    object: &Value,
    col: Option<&Value>,
    privs: &str,
) -> Result<Option<bool>> {
    let v = core.view_tx(world)?;
    let mut asked: Vec<(u32, bool)> = vec![];
    for token in privs.split(',') {
        let t = token.trim().to_lowercase();
        let (name, opt) = match t.strip_suffix("with grant option") {
            Some(n) => (n.trim_end().to_string(), true),
            None => (t.clone(), false),
        };
        asked.push((bit(&name), opt));
    }
    let any = |obj: &Obj, cols: Option<&[(String, Vec<AclItem>)]>| {
        let ok = |acl: &[AclItem], b: u32, opt: bool| {
            let (p, o) = held(who, obj.owner, acl, b);
            (if opt { o } else { p | predefined(who, obj.kind, b) }) == b
        };
        asked.iter().any(|(b, opt)| {
            ok(&obj.acl, *b, *opt) || cols.is_some_and(|cs| cs.iter().any(|(_, acl)| ok(acl, *b, *opt)))
        })
    };
    let name = object.to_text();
    Ok(Some(match kind {
        "table" | "sequence" | "column" | "any_column" => {
            let shown = catalog::rel_shown(object)?.unwrap_or_default();
            let stored = text_rel(&shown);
            // a catalog table: everyone may read it, but pg_authid and pg_shadow
            let Some(obj) = rel(&v, &stored)? else {
                let plain = rel_part(&stored).trim_start_matches("pg_catalog.");
                if !catalog_table(plain) && !catalog_table(&stored) {
                    return Ok(Some(who.superuser));
                }
                // (`'pg_authid'::regclass` reads as pg_roles here: the name as given says which)
                let given = name.trim().trim_start_matches("pg_catalog.").to_string();
                // a column of it, by name or number: one it hasn't is 42703, a number past its last null
                if let ("column", Some(c)) = (kind, col) {
                    let table = [given.as_str(), plain].into_iter().find(|n| !catalog_schema::columns(n).is_empty());
                    if let Some(table) = table {
                        let cols = catalog_schema::columns(table);
                        match c {
                            Value::Int(n) if *n < 1 || *n as usize > cols.len() => return Ok(None),
                            Value::Int(_) => {}
                            c if !cols.iter().any(|(x, _)| *x == c.to_text()) => {
                                return err(
                                    NO_COLUMN,
                                    format!("column \"{}\" of relation \"{table}\" does not exist", c.to_text()),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                let secret =
                    [plain, given.as_str()].iter().any(|n| matches!(*n, "pg_authid" | "pg_shadow" | "pg_user_mapping"));
                let reads = asked.iter().any(|(b, opt)| *b == SELECT && !opt);
                return Ok(Some(who.superuser || (reads && !secret)));
            };
            if kind == "sequence" && obj.kind != Kind::Sequence {
                return err("42809", format!("\"{name}\" is not a sequence"));
            }
            match (kind, col) {
                ("column", Some(c)) => {
                    let cols = columns(&v, &stored)?;
                    let named = match c {
                        Value::Int(n) => {
                            // (an attnum, which counts the columns dropped before it: one dropped names none)
                            let at = match load(&v, &stored)? {
                                Some(t) => (0..t.cols.len()).find(|&i| t.attnum(i) == *n),
                                None => usize::try_from(*n - 1).ok(),
                            };
                            match at.and_then(|i| cols.get(i)) {
                                Some(c) => Some(c.0.clone()),
                                // (a column number that names none: null)
                                None => return Ok(None),
                            }
                        }
                        c => Some(c.to_text()),
                    };
                    let Some(n) = named.filter(|n| cols.iter().any(|(x, _)| x == n)) else {
                        return err(
                            NO_COLUMN,
                            format!("column \"{}\" of relation \"{}\" does not exist", c.to_text(), obj.name),
                        );
                    };
                    let mine: Vec<(String, Vec<AclItem>)> = cols.into_iter().filter(|(x, _)| *x == n).collect();
                    any(&obj, Some(&mine))
                }
                ("any_column", _) => any(&obj, Some(&columns(&v, &stored)?)),
                _ => any(&obj, None),
            }
        }
        "schema" => any(&schema(&v, &name)?, None),
        "database" => any(&database(&v, world)?, None),
        "fdw" => any(&foreign_obj(&v, Kind::Fdw, &name)?, None),
        "server" => any(&foreign_obj(&v, Kind::Server, &name)?, None),
        "type" => {
            let cat = catalog::Catalog::of(core, world)?;
            let oid = match object {
                Value::Int(o) | Value::Reg(o, _) => *o,
                o => catalog::reg_oid(&cat, super::Reg::Type, &o.to_text())?,
            };
            // a type made here, or its array type (whose privileges are its element's)
            match (0..cat.enums.len()).find(|&k| oid == cat.enum_oid(k) || oid == cat.enum_oid(k) + 1) {
                Some(k) => type_obj(&v, &cat.enums[k].0)?.is_none_or(|o| any(&o, None)),
                // a built-in type: everyone may use them
                None if cat.regtype_name(oid) != oid.to_string() => true,
                // an oid of no type
                None => return Ok(None),
            }
        }
        "function" => {
            let cat = catalog::Catalog::of(core, world)?;
            let oid = match object {
                Value::Int(o) | Value::Reg(o, _) => *o,
                _ => catalog::proc_oid(&cat, Reg::Procedure, &name)?,
            };
            match cat.func_oids.iter().position(|o| *o == oid) {
                Some(k) => any(&func(&v, &cat.funcs[k])?, None),
                // a built-in function: everyone may run them
                None if catalog_schema::builtin_proc(oid).is_some() => true,
                // an oid of no function
                None => return Ok(None),
            }
        }
        _ => true,
    }))
}

// ---------------------------------------------------------------- aclexplode and makeaclitem

/// What aclexplode calls each privilege, in an ACL item's letter order.
const NAMES: [&str; 12] = [
    "INSERT",
    "SELECT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
    "EXECUTE",
    "USAGE",
    "CREATE",
    "TEMPORARY",
    "CONNECT",
];

/// The roles, as the running statement's database has them.
fn registry_now() -> Result<Arc<roles::Registry>> {
    match STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| c.core.clone()))) {
        Some(core) => roles::registry(&core),
        None => Ok(Arc::default()),
    }
}

/// One ACL item from its text, as `item_text` writes it (`alice=r*w/bob`, `=r/bob` for PUBLIC, a
/// role's name in double quotes where it needs them).
fn parse_item(reg: &roles::Registry, s: &str) -> Result<AclItem> {
    let bad = || err("22P02", format!("invalid input syntax for type aclitem: \"{s}\""));
    // a role's name, quoted or not, up to `end`; and what follows it
    let name = |t: &str, end: char| -> Option<(String, usize)> {
        let Some(q) = t.strip_prefix('"') else {
            let n = t.find(end).unwrap_or(t.len());
            return Some((t[..n].to_string(), n));
        };
        let mut out = String::new();
        let mut chars = q.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' if chars.peek().is_some_and(|(_, d)| *d == '"') => {
                    out.push('"');
                    chars.next();
                }
                '"' => return Some((out, i + 2)),
                c => out.push(c),
            }
        }
        None
    };
    let oid = |n: &str| -> Result<i64> {
        if n.is_empty() {
            return Ok(PUBLIC);
        }
        match reg.named(n) {
            Some(r) => Ok(r.oid),
            None if n == "postgres" => Ok(BOOTSTRAP),
            None => err("42704", format!("role \"{n}\" does not exist")),
        }
    };
    let Some((grantee, at)) = name(s, '=') else { return bad() };
    let Some(rest) = s[at..].strip_prefix('=') else { return bad() };
    // no grantor: the bootstrap superuser, with a warning, as Postgres's aclitemin does
    let (letters, grantor) = match rest.split_once('/') {
        Some((letters, g)) => match name(g, '\0') {
            Some((g, _)) => (letters, Some(g)),
            None => return bad(),
        },
        None => {
            NOTICES.with_borrow_mut(|x| {
                x.push(Notice {
                    level: "WARNING",
                    code: "0L000",
                    message: format!("defaulting grantor to user ID {BOOTSTRAP}"),
                    detail: None,
                    hint: None,
                })
            });
            (rest, None)
        }
    };
    let (mut privs, mut opts, mut last) = (0u32, 0u32, None);
    for c in letters.chars() {
        match (c, last) {
            ('*', Some(b)) => opts |= b,
            _ => match LETTERS.iter().position(|l| *l as char == c) {
                Some(k) => {
                    privs |= 1 << k;
                    last = Some(1 << k);
                }
                None => return bad(),
            },
        }
    }
    let grantor = match grantor {
        Some(g) => oid(&g)?,
        None => BOOTSTRAP,
    };
    Ok(AclItem { grantee: oid(&grantee)?, grantor, privs, opts })
}

/// aclexplode(acl): each privilege of each of its items, as (grantor, grantee, privilege type,
/// whether grantable).
pub(crate) fn explode(acl: &Value) -> Result<Vec<Vec<Value>>> {
    let reg = registry_now()?;
    let mut out = vec![];
    for item in super::array_values(acl.clone())?.unwrap_or_default() {
        if item == Value::Null {
            return err("22004", "ACL arrays must not contain null values");
        }
        let i = parse_item(&reg, &item.to_text())?;
        for (k, name) in NAMES.iter().enumerate().filter(|(k, _)| i.privs & (1 << k) != 0) {
            out.push(vec![
                Value::Int(i.grantor),
                Value::Int(i.grantee),
                Value::Text((*name).into()),
                Value::Bool(i.opts & (1 << k) != 0),
            ]);
        }
    }
    Ok(out)
}

/// makeaclitem(grantee, grantor, privileges, grantable): one ACL item; the privileges are a list of
/// their names (`SELECT, INSERT`), all with grant options or none.
pub(crate) fn make_item(a: &[Value]) -> Result<Value> {
    if a.contains(&Value::Null) {
        return Ok(Value::Null);
    }
    // (an oid: a negative integer wraps around, as an int4 cast to oid does)
    let oid = |v: &Value| match coerce(v.clone(), Type::BigInt)? {
        Value::Int(o) if (0..=i64::from(u32::MAX)).contains(&o) => Ok(o),
        Value::Int(o) if o >= i64::from(i32::MIN) && o < 0 => Ok(o + (1 << 32)),
        Value::Int(_) => err("22003", "OID out of range"),
        _ => err("22P02", format!("invalid input syntax for type oid: \"{}\"", v.to_text())),
    };
    let mut privs = 0;
    for chunk in a[2].to_text().split(',') {
        let chunk = chunk.trim();
        privs |= match chunk.to_uppercase().as_str() {
            "TEMP" => TEMP,
            "RULE" => 0,
            "SET" | "ALTER SYSTEM" => {
                return err(UNSUPPORTED, "SET and ALTER SYSTEM privileges (on settings) aren't supported");
            }
            n => match NAMES.iter().position(|x| *x == n) {
                Some(k) => 1 << k,
                None => return err("22023", format!("unrecognized privilege type: \"{chunk}\"")),
            },
        };
    }
    let grantable = matches!(coerce(a[3].clone(), Type::Bool)?, Value::Bool(true));
    let item = AclItem { grantee: oid(&a[0])?, grantor: oid(&a[1])?, privs, opts: if grantable { privs } else { 0 } };
    Ok(Value::Text(item_text(&*registry_now()?, &item)))
}

// ---------------------------------------------------------------- the catalog

/// What the catalog shows of an object's privileges: its owner, and its ACL as Postgres writes
/// it (None: the default, null in the catalog).
pub(crate) fn shown(reg: &roles::Registry, e: Option<&Entry>, owner: i64) -> (i64, Value) {
    let owner = e.and_then(|e| e.owner).unwrap_or(owner);
    let acl = e.and_then(|e| e.acl.as_ref()).map_or(Value::Null, |a| Value::Text(text(reg, a)));
    (owner, acl)
}

/// A schema's owner and ACL as pg_namespace shows them: public's is Postgres 15's until changed
/// (`pg_database_owner` may make objects in it, everyone may use it), as pg_dump expects it;
/// pg_catalog's and information_schema's are null, which pg_dump takes as unchanged (everyone may
/// use them all the same).
pub(crate) fn schema_shown(reg: &roles::Registry, e: Option<&Entry>, name: &str) -> (i64, Value) {
    match (name, e.and_then(|e| e.acl.as_ref())) {
        ("public", None) => (e.and_then(|e| e.owner).unwrap_or(DB_OWNER), Value::Text(text(reg, &public_acl()))),
        _ => shown(reg, e, BOOTSTRAP),
    }
}
