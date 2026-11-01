//! Row-level security: ALTER TABLE ... ENABLE | DISABLE | FORCE | NO FORCE ROW LEVEL SECURITY,
//! CREATE, ALTER and DROP POLICY, and the policies applied as statements read and write rows, as
//! Postgres 16's.
//!
//! A table's state is a row of its world, `_sonos_rls/<table>` (`{"enable": true, "force": true}`;
//! no row: neither), and so is each policy, `_sonos_policy/<table>/<name>` (the table's name with `/`
//! and `%` escaped, see `key_part`, so one table's policies never run into another's; its command, whether it
//! is permissive, the roles it is for by oid, and its USING and WITH CHECK as SQL). So they fork,
//! merge, roll back and travel in time with the table; two worlds' new policies on one table merge
//! without a conflict; a rename moves them and a drop drops them (`moves`, `after`). A database
//! that never had any pays nothing for them (`Core::has_row_security`).
//!
//! Who they apply to (`applies`): not a superuser, a role with BYPASSRLS, or the table's owner (or a
//! member of it) unless the table says FORCE. For everyone else a table with row security shows and
//! takes only the rows a policy allows (none at all without one); with `row_security = off` a
//! statement they would limit is refused instead, as pg_dump wants.
//!
//! How, as Postgres's rewriter does it with security quals and WITH CHECK options: a read of the
//! table gets its policies' USING where views are expanded (`expand_select` calls `read`), as a
//! subquery, or first in the WHERE of a query level that reads nothing else (which keeps index
//! reads); UPDATE and DELETE get theirs first in their WHERE (`write`); the rows INSERT and UPDATE
//! write are checked before NOT NULL and CHECK constraints are (`Checks`). The policies are those of
//! whoever reads: the statement's role, a view's owner inside the view's query, a SECURITY DEFINER
//! function's owner in its statements; what they read is that role's to read, policies included.
//! Foreign keys, unique keys and TRUNCATE see every row, as in Postgres.
//!
//! What a policy reads is bound when it is made: its condition names what its maker's search_path
//! finds outside public with its schema (`Parser::bind`), and is read with its other names public's
//! and built-in functions the built-ins (`expr_of`), whoever reads; a temporary table can't stand in
//! for a table it reads. A column's rename renames it in the policies that read it, and a drop of one
//! they read is refused or, with CASCADE, drops them (`renamed_column`, `dropped_column`).

use super::roles::{BOOTSTRAP, PUBLIC, Spec, Who};
use super::*;
use std::cell::RefCell;

pub(crate) const RLS: &str = "_sonos_rls";
pub(crate) const POLICIES: &str = "_sonos_policy";

/// A policy, as its row keeps it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Policy {
    pub name: String,
    /// its table, as stored
    pub table: String,
    /// pg_policy.polcmd: `*` every command, `r` SELECT, `a` INSERT, `w` UPDATE, `d` DELETE
    pub cmd: char,
    pub permissive: bool,
    /// the roles it is for, by oid (PUBLIC's is 0)
    pub roles: Vec<i64>,
    /// USING and WITH CHECK, as SQL
    pub using: Option<String>,
    pub check: Option<String>,
    /// its place among its table's policies in the order they were made (0: made before this was kept), which
    /// pg_policy's rows come in
    pub made: u64,
}

impl Policy {
    fn parse(table: &str, name: &str, raw: &str) -> Policy {
        let j: Json = serde_json::from_str(raw).unwrap_or_default();
        let text = |k: &str| j[k].as_str().map(String::from);
        Policy {
            name: name.into(),
            table: table.into(),
            cmd: j["cmd"].as_str().and_then(|c| c.chars().next()).unwrap_or('*'),
            permissive: j["permissive"] != false,
            roles: j["roles"].as_array().into_iter().flatten().filter_map(Json::as_i64).collect(),
            using: text("using"),
            check: text("check"),
            made: j["made"].as_u64().unwrap_or(0),
        }
    }

    fn raw(&self) -> String {
        let mut j = serde_json::json!({
            "cmd": self.cmd.to_string(),
            "permissive": self.permissive,
            "roles": self.roles,
        });
        if self.made > 0 {
            j["made"] = self.made.into();
        }
        if let Some(u) = &self.using {
            j["using"] = u.clone().into();
        }
        if let Some(c) = &self.check {
            j["check"] = c.clone().into();
        }
        j.to_string()
    }

    pub(crate) fn key(&self) -> String {
        policy_key(&self.table, &self.name)
    }

    /// The command, as pg_policies and psql name it.
    pub(crate) fn cmd_name(&self) -> &'static str {
        match self.cmd {
            'r' => "SELECT",
            'a' => "INSERT",
            'w' => "UPDATE",
            'd' => "DELETE",
            _ => "ALL",
        }
    }

    /// The roles it is for, as pg_policies shows them: `{public}`, or the names of the roles there are
    /// (PUBLIC among others is none of them), in order.
    pub(crate) fn role_names(&self, reg: &roles::Registry) -> Vec<String> {
        if self.roles == [PUBLIC] {
            return vec!["public".into()];
        }
        let mut names: Vec<String> =
            self.roles.iter().filter(|o| **o != PUBLIC).filter_map(|o| reg.by_oid(*o).map(|r| r.name)).collect();
        names.sort();
        names.dedup();
        names
    }
}

/// A table's name as a part of a key: `/` and `%` written `%2F` and `%25`, so the part has no `/` and
/// the table's policies (`_sonos_policy/<table>/<name>`) never run into another table's whatever the
/// names (`a/b`'s would be `a`'s). A name without either is as it is, so keys made before this read
/// the same.
fn key_part(table: &str) -> String {
    table.replace('%', "%25").replace('/', "%2F")
}

/// The table's name `key_part` wrote.
fn table_of(part: &str) -> String {
    part.replace("%2F", "/").replace("%25", "%")
}

fn flags_key(table: &str) -> String {
    format!("{RLS}/{}", key_part(table))
}

fn policy_key(table: &str, name: &str) -> String {
    format!("{POLICIES}/{}/{name}", key_part(table))
}

/// The key `acl::MADE` keeps when policy `name` on `table` (as stored) was made under: `policy/<table>/<name>`, the
/// table's part as `policy_key`'s.
pub(crate) fn made_key(table: &str, name: &str) -> String {
    format!("policy/{}/{name}", key_part(table))
}

/// What `acl::MADE` keeps for a policy, by statement `st`: CREATE POLICY's, the policy it is about to make (its
/// `made_key`), when it isn't there yet.
pub(crate) fn to_stamp(v: &View, st: &PolicyStmt) -> Result<Option<String>> {
    let PolicyStmt::Create { name, table, .. } = st else { return Ok(None) };
    let Ok(t) = need_table(v, table) else { return Ok(None) };
    Ok(v.get(&policy_key(&t.name, name))?.is_none().then(|| made_key(&t.name, name)))
}

/// ALTER POLICY ... RENAME TO's (statement `st`): the policy's `made_key` before and after, worked out before it runs.
pub(crate) fn stamp_moves(v: &View, st: &PolicyStmt) -> Vec<(String, String)> {
    let PolicyStmt::Alter { name, table, rename: Some(to), .. } = st else { return vec![] };
    let Ok(t) = need_table(v, table) else { return vec![] };
    vec![(made_key(&t.name, name), made_key(&t.name, to))]
}

/// Whether `table` has row security enabled, and forced (pg_class.relrowsecurity and
/// relforcerowsecurity).
pub(crate) fn flags(v: &View, table: &str) -> Result<(bool, bool)> {
    if !v.core.has_row_security() {
        return Ok((false, false));
    }
    Ok(match v.get(&flags_key(table))? {
        Some(raw) => parse_flags(&raw),
        None => (false, false),
    })
}

fn parse_flags(raw: &str) -> (bool, bool) {
    let j: Json = serde_json::from_str(raw).unwrap_or_default();
    (j["enable"] == true, j["force"] == true)
}

/// The write that keeps `table`'s flags (none when both are off).
fn flags_write(table: &str, enable: bool, force: bool) -> (String, Option<String>) {
    let raw = (enable || force).then(|| serde_json::json!({ "enable": enable, "force": force }).to_string());
    (flags_key(table), raw)
}

/// Every table's flags in world `v`.
pub(crate) fn all_flags(v: &View) -> Result<HashMap<String, (bool, bool)>> {
    let mut out = HashMap::new();
    if !v.core.has_row_security() {
        return Ok(out);
    }
    let prefix = format!("{RLS}/");
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        out.insert(table_of(&k[prefix.len()..]), parse_flags(&raw));
    }
    Ok(out)
}

/// `table`'s policies, by name.
pub(crate) fn policies(v: &View, table: &str) -> Result<Vec<Policy>> {
    if !v.core.has_row_security() {
        return Ok(vec![]);
    }
    let prefix = format!("{POLICIES}/{}/", key_part(table));
    let mut out = vec![];
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        out.push(Policy::parse(table, &k[prefix.len()..], &raw));
    }
    Ok(out)
}

/// Every policy of world `v`, by table and name.
pub(crate) fn all(v: &View) -> Result<Vec<Policy>> {
    let mut out = vec![];
    if !v.core.has_row_security() {
        return Ok(out);
    }
    let prefix = format!("{POLICIES}/");
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        // (the table's part has no '/': see `key_part`; a policy's name may)
        let Some((t, n)) = k[prefix.len()..].split_once('/') else { continue };
        out.push(Policy::parse(&table_of(t), n, &raw));
    }
    Ok(out)
}

// ---------------------------------------------------------------- who

/// Whether world `v` has a table with row security on (reads and writes then look for theirs).
pub(super) fn any(v: &View) -> Result<bool> {
    if tenant().is_some() {
        return Ok(true);
    }
    Ok(v.core.has_row_security() && v.prefix(&format!("{RLS}/"))?.next().is_some())
}

// ---------------------------------------------------------------- an agent held to a tenant

/// The tenant the running statement's agent is held to (see `crate::agents::Agent::tenant`): the condition, as SQL
/// (`"org_id" = 42`), every row it reads or writes of a table with that column meets, whatever its role and whatever
/// the table's own row security says; a table without it is held to it through its foreign keys, or read-only (see
/// `reach`). None: an agent with no tenant, or no agent.
pub(crate) fn tenant() -> Option<Arc<str>> {
    STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref()).and_then(|c| c.tenant.clone()))
}

/// Whether row security may limit the running statement: some table has had it, or its agent is held to a tenant.
pub(super) fn possible(core: &crate::Core) -> bool {
    core.has_row_security() || tenant().is_some()
}

/// The column a tenant's condition (`"org_id" = 42`, see `tenant`) names: `org_id`.
pub(crate) fn tenant_column(cond: &str) -> String {
    let col = cond.split_once(" = ").map_or("", |(c, _)| c);
    col.strip_prefix('"').and_then(|c| c.strip_suffix('"')).unwrap_or(col).replace("\"\"", "\"")
}

/// The most foreign keys an agent held to a tenant is followed through, from a table without the tenant's column to
/// one with it (grandchild, child, parent: a line item's note, a line item, an order).
pub(crate) const MOST_HOPS: usize = 3;

/// How a table stands to a tenant whose column is `col` (see `reach`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reach {
    /// it has the column: its rows are the tenant's that hold its value
    Column,
    /// it hasn't, but a foreign key leads from it to a table that has, in `hops` foreign keys at most: its rows are the
    /// tenant's whose columns `cols` are the columns `refs` of a row of `parent` that is the tenant's (its own one
    /// hop nearer, or its value)
    Through { cols: Vec<String>, parent: String, refs: Vec<String>, hops: usize },
    /// neither: its rows are every tenant's (a lookup table), read whole, and written by none
    Shared,
}

impl Reach {
    /// The condition, as SQL, a row of the table meets when it is the tenant's of condition `cond` (see `tenant`).
    /// None for a shared table, whose rows all are, to read, and none, to write.
    fn condition(&self, cond: &str) -> Option<String> {
        match self {
            Reach::Column => Some(cond.to_string()),
            Reach::Through { cols, parent, refs, .. } => {
                let list = |cs: &[String]| cs.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ");
                let mine = match cols.len() {
                    1 => list(cols),
                    _ => format!("({})", list(cols)),
                };
                // (the parent's rows read here are held to the tenant in turn, its own one hop nearer: see `reach`)
                Some(format!("{mine} in (select {} from {})", list(refs), ident_rel(parent)))
            }
            Reach::Shared => None,
        }
    }

    /// In words, for messages and SHOW AGENT ... PRIVILEGES: how its rows are the tenant's of column `col`.
    pub(crate) fn words(&self, col: &str) -> String {
        match self {
            Reach::Column => format!("its rows with the tenant's {col}"),
            Reach::Through { cols, parent, hops, .. } => format!(
                "its rows whose {} is a row of {} that is the tenant's (through {hops} foreign key{})",
                cols.join(", "),
                rel_part(parent),
                if *hops == 1 { "" } else { "s" }
            ),
            Reach::Shared => {
                format!(
                    "shared by every tenant: no column {col} and no foreign key to a table that has it, so read-only"
                )
            }
        }
    }
}

/// How table `table` of world `v` stands to the tenant whose column is `col`: it has the column, or a foreign key
/// leads from it to a table that has it (in `MOST_HOPS` at most), or neither and it is shared; None when there is no
/// such table. Of the foreign keys that lead there, the one in the fewest hops, the table's first of those: so the
/// parent it is held through is held in fewer hops itself, and following them never comes back round (two tables
/// whose keys point at each other aren't each other's way to the tenant). The foreign key's columns are matched to
/// the columns it references (its unique constraint's, or the parent's primary key).
pub(crate) fn reach(v: &View, table: &str, col: &str) -> Result<Option<Reach>> {
    let mut seen: HashMap<String, Option<std::rc::Rc<Table>>> = HashMap::new();
    let Some(t) = loaded(v, table, &mut seen)? else { return Ok(None) };
    if t.cols.iter().any(|c| c.name == col) {
        return Ok(Some(Reach::Column));
    }
    for hops in 1..=MOST_HOPS {
        for fk in &t.fks {
            let Some(p) = loaded(v, &fk.table, &mut seen)? else { continue };
            let Some((cols, refs)) = fk_link(&t, fk, &p) else { continue };
            if reaches(v, &p, col, hops - 1, &mut seen)? {
                return Ok(Some(Reach::Through { cols, parent: fk.table.clone(), refs, hops }));
            }
        }
    }
    Ok(Some(Reach::Shared))
}

/// World table `name`, loaded once for one `reach` (`seen`).
fn loaded(
    v: &View,
    name: &str,
    seen: &mut HashMap<String, Option<std::rc::Rc<Table>>>,
) -> Result<Option<std::rc::Rc<Table>>> {
    if let Some(t) = seen.get(name) {
        return Ok(t.clone());
    }
    let t = load_world(v, name)?.map(std::rc::Rc::new);
    seen.insert(name.to_string(), t.clone());
    Ok(t)
}

/// Whether table `t` has column `col`, or a foreign key leads from it to a table that has, in `hops` at most.
fn reaches(
    v: &View,
    t: &Table,
    col: &str,
    hops: usize,
    seen: &mut HashMap<String, Option<std::rc::Rc<Table>>>,
) -> Result<bool> {
    if t.cols.iter().any(|c| c.name == col) {
        return Ok(true);
    }
    if hops == 0 {
        return Ok(false);
    }
    for fk in &t.fks {
        let Some(p) = loaded(v, &fk.table, seen)? else { continue };
        if fk_link(t, fk, &p).is_some() && reaches(v, &p, col, hops - 1, seen)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Foreign key `fk` of table `t` by name: its columns, and the columns of `parent` they reference (those of its
/// unique constraint, or the parent's primary key). None when a column isn't there (a parent without a key).
fn fk_link(t: &Table, fk: &Fk, parent: &Table) -> Option<(Vec<String>, Vec<String>)> {
    let name = |tb: &Table, id: &u32| tb.cols.iter().find(|c| c.id == *id).map(|c| c.name.clone());
    let cols: Vec<String> = fk.cols.iter().map(|id| name(t, id)).collect::<Option<_>>()?;
    let refs: Vec<String> = if fk.refs.is_empty() {
        parent.key_cols().iter().map(|&k| parent.cols.get(k).map(|c| c.name.clone())).collect::<Option<_>>()?
    } else {
        fk.refs.iter().map(|id| name(parent, id)).collect::<Option<_>>()?
    };
    (!cols.is_empty() && cols.len() == refs.len()).then_some((cols, refs))
}

/// The tenant row `id` of `table`, stored as `raw` in `v`, is held to through its foreign keys (see `reach`): the
/// value of the tenant's column `col` in the row they lead to, as JSON. None when they lead to no row (a key that is
/// null, a parent not there, as the tenant's condition reads it: no tenant's), or the table reaches no table with the
/// column. What a merge checks a world's change to such a row by (see `scope::outside`): the row has no column of
/// the tenant's to compare.
pub(crate) fn reached_tenant(v: &View, table: &str, id: &str, raw: &str, col: &str) -> Result<Option<Json>> {
    let (mut table, mut id, mut raw) = (table.to_string(), id.to_string(), raw.to_string());
    for _ in 0..=MOST_HOPS {
        let Some(t) = load_world(v, &table)? else { return Ok(None) };
        let vals = decode_raw(&t, &id, &raw, None)?;
        if let Some(i) = t.cols.iter().position(|c| c.name == col) {
            return Ok(Some(vals[i].to_json_as(t.cols[i].ty)).filter(|j| !j.is_null()));
        }
        let Some(Reach::Through { cols, parent, refs, .. }) = reach(v, &table, col)? else { return Ok(None) };
        let Some(p) = load_world(v, &parent)? else { return Ok(None) };
        let link = Some((cols, refs));
        let Some(fk) = t.fks.iter().find(|f| f.table == parent && fk_link(&t, f, &p) == link) else { return Ok(None) };
        let Some(x) = fk_vals(&t, fk, &vals) else { return Ok(None) };
        let get = |k: &str| -> Result<Option<String>> { Ok(v.get(k)?.map(|x| x.to_string())) };
        let Some(pid) = fk_parent_any(v, Some(&p), fk, &x, &get)? else { return Ok(None) };
        // (a partitioned parent's row is in one of its partitions)
        let mut found = None;
        for leaf in hier::row_tables(v, &p)? {
            if let Some(r) = v.get(&row_key(&leaf.name, &pid))? {
                found = Some((leaf.name, r.to_string()));
                break;
            }
        }
        let Some((leaf, r)) = found else { return Ok(None) };
        (table, id, raw) = (leaf, pid, r);
    }
    Ok(None)
}

/// Which rows of one view are one tenant's, as `reached_tenant` tells, for many rows at a time: each table's way to
/// the tenant worked out once, and each parent row's tenant read once. For checking what a world read for its tenant
/// against only that tenant's rows (see `stale`): a row no read of the tenant's could have found doesn't count.
pub(crate) struct Tenants {
    col: String,
    value: Json,
    /// by table: the table, and how it stands to the tenant (None: no such table)
    tables: HashMap<String, Option<(std::rc::Rc<Table>, Reach)>>,
    /// by a parent's row key: whether that row is the tenant's
    parents: HashMap<String, bool>,
}

impl Tenants {
    /// For tenant `tenant`, `{"column": c, "value": v}` (see `scope::tenant`).
    pub(crate) fn new(tenant: &Json) -> Option<Tenants> {
        let col = tenant["column"].as_str()?.to_string();
        Some(Tenants { col, value: tenant["value"].clone(), tables: HashMap::new(), parents: HashMap::new() })
    }

    /// How table `table` of `v` stands to the tenant (see `reach`); None: no such table.
    pub(crate) fn reach(&mut self, v: &View, table: &str) -> Result<Option<Reach>> {
        Ok(self.table(v, table)?.map(|(_, r)| r))
    }

    fn table(&mut self, v: &View, table: &str) -> Result<Option<(std::rc::Rc<Table>, Reach)>> {
        if let Some(t) = self.tables.get(table) {
            return Ok(t.clone());
        }
        let found = match load_world(v, table)? {
            Some(t) => reach(v, table, &self.col)?.map(|r| (std::rc::Rc::new(t), r)),
            None => None,
        };
        self.tables.insert(table.to_string(), found.clone());
        Ok(found)
    }

    /// Whether row `key` of `v` (`raw` as stored; None: no such row) is the tenant's: Some(false) for no row, a
    /// foreign key that is null or leads to no row, or another tenant's. None when its table is shared by every
    /// tenant, or isn't a table: every tenant reads it.
    pub(crate) fn mine(&mut self, v: &View, key: &str, raw: Option<&str>) -> Result<Option<bool>> {
        self.mine_within(v, key, raw, MOST_HOPS + 1)
    }

    fn mine_within(&mut self, v: &View, key: &str, raw: Option<&str>, hops: usize) -> Result<Option<bool>> {
        let Some((table, id)) = key.split_once('/') else { return Ok(None) };
        let Some((t, way)) = self.table(v, table)? else { return Ok(None) };
        if way == Reach::Shared {
            return Ok(None);
        }
        let Some(raw) = raw else { return Ok(Some(false)) };
        let vals = decode_raw(&t, id, raw, None)?;
        let Reach::Through { cols, parent, refs, .. } = way else {
            let Some(i) = t.cols.iter().position(|c| c.name == self.col) else { return Ok(Some(false)) };
            let got = vals[i].to_json_as(t.cols[i].ty);
            return Ok(Some(!got.is_null() && crate::scope::same(&got, &self.value)));
        };
        if hops == 0 {
            return Ok(Some(false));
        }
        let Some((p, _)) = self.table(v, &parent)? else { return Ok(Some(false)) };
        let link = Some((cols, refs));
        let Some(fk) = t.fks.iter().find(|f| f.table == parent && fk_link(&t, f, &p) == link) else {
            return Ok(Some(false));
        };
        let Some(x) = fk_vals(&t, fk, &vals) else { return Ok(Some(false)) };
        let get = |k: &str| -> Result<Option<String>> { Ok(v.get(k)?.map(|x| x.to_string())) };
        let Some(pid) = fk_parent_any(v, Some(&p), fk, &x, &get)? else { return Ok(Some(false)) };
        // (a partitioned parent's row is in one of its partitions)
        for leaf in hier::row_tables(v, &p)? {
            let pk = row_key(&leaf.name, &pid);
            if let Some(&known) = self.parents.get(&pk) {
                return Ok(Some(known));
            }
            if let Some(r) = v.get(&pk)? {
                let mine = self.mine_within(v, &pk, Some(r.as_ref()), hops - 1)?.unwrap_or(false);
                self.parents.insert(pk, mine);
                return Ok(Some(mine));
            }
        }
        Ok(Some(false))
    }
}

/// How `table` stands to the running statement's agent's tenant (see `reach`); None: no tenant, or no such table.
fn tenant_reach(v: &View, table: &str) -> Result<Option<Reach>> {
    let Some(cond) = tenant() else { return Ok(None) };
    reach(v, table, &tenant_column(&cond))
}

/// The policy holding the running statement's agent to its tenant on `table` (see `tenant`), when the table has the
/// tenant's column or a foreign key to a table that has it (see `reach`): for every command, every role, its
/// condition both USING and WITH CHECK. A shared table has none: it reads whole, and `read_only` refuses its writes.
fn tenant_policy(v: &View, table: &str) -> Result<Option<Policy>> {
    let Some(cond) = tenant() else { return Ok(None) };
    let Some(using) = reach(v, table, &tenant_column(&cond))?.and_then(|r| r.condition(&cond)) else {
        return Ok(None);
    };
    Ok(Some(Policy {
        name: "chronos_tenant".into(),
        table: table.into(),
        cmd: '*',
        permissive: true,
        roles: vec![PUBLIC],
        using: Some(using.clone()),
        check: Some(using),
        made: 0,
    }))
}

/// 42501 for a write to `table` by an agent held to a tenant when the table is shared by every tenant (see `reach`):
/// a row it inserted, changed or deleted there would be every tenant's to see, so it may only read it.
fn read_only(v: &View, table: &str) -> Result<()> {
    let Some(cond) = tenant() else { return Ok(()) };
    let col = tenant_column(&cond);
    if reach(v, table, &col)? != Some(Reach::Shared) {
        return Ok(());
    }
    err(
        "42501",
        format!(
            "an agent held to tenant {cond} may only read table \"{}\": it has no column {col} and no foreign key to a \
             table that has it (in {MOST_HOPS} at most), so its rows are every tenant's",
            rel_part(table)
        ),
    )
}

/// The policies a read or write of `table` by `who` keeps to: the table's own, and, for an agent held to a tenant,
/// its tenant's on top (one more that must hold where the table's own limit `who`; the only one where they don't).
fn effective(v: &View, table: &str, who: &Who) -> Result<Vec<Policy>> {
    let Some(mut t) = tenant_policy(v, table)? else { return policies(v, table) };
    if !applies_own(v, table, Some(who), true)? {
        return Ok(vec![t]);
    }
    t.permissive = false;
    let mut ps = policies(v, table)?;
    ps.push(t);
    Ok(ps)
}

/// Whether statement `st` reads or writes a table with row security on, or a view (whose query may
/// read one): only such a statement is looked at for policies, so one of other tables pays a lookup
/// per table it names.
pub(super) fn touched(v: &View, st: &Stmt) -> Result<bool> {
    if !possible(v.core) {
        return Ok(false);
    }
    let mut names = vec![];
    match st {
        Stmt::Explain { stmt, .. } => return touched(v, stmt),
        Stmt::Insert { table, .. } => names.push(table.clone()),
        _ => {}
    }
    // (a write to a table shared by every tenant, by an agent held to one: refused, see `read_only`)
    if let Stmt::Insert { table, .. } | Stmt::Update { table, .. } | Stmt::Delete { table, .. } = st
        && tenant_reach(v, table)? == Some(Reach::Shared)
    {
        return Ok(true);
    }
    reads_of(st, &mut names);
    // (a table read AS OF a moment: its policies then, whatever its name has now)
    if reads_past(st) {
        return Ok(true);
    }
    let views = v.core.has_views();
    for n in names {
        if flags(v, &n)?.0 || (views && view_def(v, &n)?.is_some()) || tenant_policy(v, &n)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// For the HTTP and MCP row API, which reads and writes rows as they are, as agent `agent`: the
/// message refusing it the rows of `tables` in world `branch` when row security limits the agent's
/// role (a role of its name) on one of them.
pub(crate) fn agent_refused(db: &Db, agent: &str, branch: &str, tables: &[&str]) -> Result<Option<String>> {
    if !db.core.has_row_security() || tables.is_empty() {
        return Ok(None);
    }
    let reg = roles::registry(&db.core)?;
    let Some(role) = reg.named(agent) else { return Ok(None) };
    let who = Who::for_role(&reg, role.oid);
    let v = db.view_tx(branch)?;
    for t in tables {
        let limited = match applies(&v, t, Some(&who)) {
            Ok(l) => l,
            Err(e) if e.code == "42501" => true,
            Err(e) => return Err(e),
        };
        if limited {
            return Ok(Some(format!(
                "row-level security on table \"{}\" limits role \"{agent}\", and the row API works on rows as they \
                 are: read and change them with SQL (the sql tool, or POST /v1/sql), which applies its policies",
                rel_part(t)
            )));
        }
    }
    Ok(None)
}

/// Relation `rel`'s owner, as whose the reads in it are made (a view's).
pub(super) fn owner(v: &View, rel: &str) -> Result<Option<Arc<Who>>> {
    let reg = roles::registry(v.core)?;
    let o = acl::rel(v, rel)?.map_or(BOOTSTRAP, |o| o.owner);
    Ok(Some(Arc::new(Who::for_role(&reg, o))))
}

thread_local! {
    /// The statement being rewritten is one already rewritten (see `skipping`)
    static SKIP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` with row security left out of the statements it rewrites when `skip`: they are the
/// rewrite of one that has it in already (see `Statement::execute_here`).
pub(super) fn skipping<T>(skip: bool, f: impl FnOnce() -> T) -> T {
    let was = SKIP.replace(skip);
    let out = f();
    SKIP.set(was);
    out
}

/// Whether the statement being rewritten has its row security in already.
pub(super) fn skipped() -> bool {
    SKIP.get()
}

/// The running statement's `row_security` setting: off, or on (the default).
fn row_security_on() -> bool {
    !roles::setting_now("row_security").is_some_and(|s| setting_off(&s))
}

/// Whether a `row_security` setting is off.
pub(super) fn setting_off(s: &str) -> bool {
    matches!(s.to_lowercase().as_str(), "off" | "false" | "no" | "0" | "f" | "n" | "of")
}

/// Whether row security limits what `who` reads and writes of `table` (Postgres's check_enable_rls):
/// not when the table hasn't it on, or `who` is a superuser, has BYPASSRLS, or owns the table (and the
/// table doesn't FORCE it). When it would and `row_security` is off, 42501.
pub(crate) fn applies(v: &View, table: &str, who: Option<&Who>) -> Result<bool> {
    applies_as(v, table, who, row_security_on())
}

/// Whether row security limits `who` on `table`, whatever `row_security` says (Postgres's check_enable_rls
/// asked not to fail: what row_security_active says, and whether a constraint's error may show a key).
pub(crate) fn limits(v: &View, table: &str, who: Option<&Who>) -> Result<bool> {
    applies_as(v, table, who, true)
}

/// `applies`, with `row_security` on or off (`on`). An agent held to a tenant is limited on every table with the
/// tenant's column or a foreign key to one (see `reach`), whatever its role and whatever `row_security` says (see
/// `tenant`).
pub(crate) fn applies_as(v: &View, table: &str, who: Option<&Who>, on: bool) -> Result<bool> {
    if tenant_policy(v, table)?.is_some() {
        return Ok(true);
    }
    applies_own(v, table, who, on)
}

/// Whether the table's own row security limits `who` (`applies_as` without the tenant).
fn applies_own(v: &View, table: &str, who: Option<&Who>, on: bool) -> Result<bool> {
    let (enable, force) = flags(v, table)?;
    if !enable {
        return Ok(false);
    }
    let Some(who) = who.filter(|w| !w.superuser) else { return Ok(false) };
    if who.reg.by_oid(who.oid).is_some_and(|r| r.bypassrls) {
        return Ok(false);
    }
    let owner = acl::rel(v, table)?.map_or(BOOTSTRAP, |o| o.owner);
    let owns = who.has_privs_of(owner);
    if owns && !force {
        return Ok(false);
    }
    if !on {
        return Err(SqlError {
            code: "42501",
            message: format!("query would be affected by row-level security policy for table \"{}\"", rel_part(table)),
            info: owns.then(|| {
                Box::new(Violation {
                    hint: Some(
                        "To disable the policy for the table's owner, use ALTER TABLE NO FORCE ROW LEVEL SECURITY."
                            .into(),
                    ),
                    ..Violation::default()
                })
            }),
        });
    }
    Ok(true)
}

/// row_security_active(table): whether row security limits the running role on it, whatever `row_security`
/// says (see `limits`); a table by an oid that names none is false, by a name that names none 42P01.
pub(super) fn active(ctx: &Ctx, rel: &Value) -> Result<Value> {
    let Some(shown) = catalog::rel_shown(rel)? else {
        if matches!(rel, Value::Int(_) | Value::Reg(..)) {
            return Ok(Value::Bool(false));
        }
        return err(NO_TABLE, format!("relation \"{}\" does not exist", rel.to_text()));
    };
    let v = ctx.core.view_tx(&ctx.world)?;
    let who = roles::current();
    Ok(Value::Bool(limits(&v, &text_rel(&shown), who.as_deref())?))
}

/// Whether a constraint's error may show the values of a key of `table` (its DETAIL, `Key (a)=(1) ...`), as
/// Postgres's ri_ReportViolation and BuildIndexValueDescription decide: not where row security limits the
/// running role on the table, or the role can't read it (column privileges aside: then not either); nor where
/// the table has masked columns that the running statement reads masked (see `mask`).
pub(super) fn may_show_key(table: &str) -> bool {
    if let Some(ctx) = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone()))
        && ctx.masked
    {
        let Ok(v) = ctx.core.view_tx(&ctx.world) else { return false };
        if !super::mask::of(&v, table).is_ok_and(|m| m.is_empty()) {
            return false;
        }
    }
    let Some(who) = roles::current().filter(|w| !w.superuser) else { return true };
    let Some(ctx) = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone())) else { return true };
    if !ctx.core.has_row_security() && !ctx.core.has_privileges() {
        return true;
    }
    let Ok(v) = ctx.core.view_tx(&ctx.world) else { return false };
    if limits(&v, table, Some(&*who)).unwrap_or(true) {
        return false;
    }
    if !ctx.core.has_privileges() {
        return true;
    }
    match acl::rel(&v, table) {
        Ok(Some(o)) => o.has(&who, acl::SELECT),
        Ok(None) => true, // (a temporary table: its session's)
        Err(_) => false,
    }
}

/// The policies of `ps` for command `cmd` (`r`, `a`, `w`, `d`) that `who` is among the roles of:
/// (permissive ones, restrictive ones), by name.
fn chosen<'a>(ps: &'a [Policy], cmd: char, who: &Who) -> (Vec<&'a Policy>, Vec<&'a Policy>) {
    ps.iter()
        .filter(|p| (p.cmd == '*' || p.cmd == cmd) && p.roles.iter().any(|r| *r == PUBLIC || who.has_privs_of(*r)))
        .partition(|p| p.permissive)
}

/// Policies' conditions parsed, by their text (see `expr_of`), each with the composite types' shapes it was parsed
/// with (a cast to one): parsed again where one is no longer its type's in the world looked up in (see
/// `stale_record`), as a world's shape of a type is its own.
#[allow(clippy::type_complexity)]
static PARSED: std::sync::LazyLock<std::sync::Mutex<HashMap<String, (Expr, Vec<Type>)>>> =
    std::sync::LazyLock::new(Default::default);

/// Most conditions `PARSED` keeps: past them it starts again (rather than track which were used last).
const MOST_PARSED: usize = 4096;

/// Empties `PARSED`, whose conditions may hold enum types' places (see `Enums::sweep`).
pub(super) fn forget_parsed() {
    PARSED.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// A policy's condition (as SQL), parsed: its names as its maker's search_path found them (see
/// `Parser::bind`), an unqualified one public's and a built-in function's the built-in, whatever the
/// search_path of whoever reads. Parsed once for each text: the same text is the same condition, whoever reads (but
/// for a cast to a composite type, whose shape is its world's: see `PARSED`).
fn expr_of(text: &str) -> Result<Expr> {
    let hit = PARSED.lock().unwrap_or_else(|e| e.into_inner()).get(text).cloned();
    if let Some((e, shapes)) = hit
        && !shapes.iter().any(|t| stale_record(*t))
    {
        return Ok(e);
    }
    let (e, shapes) = userdefs::noting(|| -> Result<Expr> {
        let mut e = default_names(|| {
            let mut p = Parser::new(lex(text)?);
            p.pinned = true;
            let e = p.expr()?;
            if p.peek().is_some() {
                return p.fail("expected the end of the policy's condition");
            }
            Ok(e)
        })?;
        // (what it reads is the world's, whatever temporary tables have the names)
        map_subqueries(&mut e, &mut |q| {
            bind_sources(q);
            Ok(true)
        })?;
        Ok(e)
    });
    let e = e?;
    let mut parsed = PARSED.lock().unwrap_or_else(|e| e.into_inner());
    if parsed.len() >= MOST_PARSED {
        parsed.clear();
    }
    parsed.insert(text.to_string(), (e.clone(), shapes));
    Ok(e)
}

/// `es` joined by `op` as a balanced tree, as deep as the logarithm of how many there are: a table's
/// hundreds of policies nest no deeper than a few levels.
fn balanced(mut es: Vec<Expr>, op: Op) -> Option<Expr> {
    while es.len() > 1 {
        let mut next = Vec::with_capacity(es.len().div_ceil(2));
        let mut it = es.into_iter();
        while let Some(a) = it.next() {
            next.push(match it.next() {
                Some(b) => bin(a, op, b),
                None => a,
            });
        }
        es = next;
    }
    es.pop()
}

fn or_all(es: Vec<Expr>) -> Option<Expr> {
    balanced(es, Op::Or)
}

pub(super) fn and_all(es: Vec<Expr>) -> Option<Expr> {
    balanced(es, Op::And)
}

/// 54001 when the conditions of `table`'s policies, put together, nest deeper than a statement may
/// (see MAX_DEPTH): evaluating them would run out of stack.
pub(super) fn not_too_deep(table: &str, e: &Expr) -> Result<()> {
    if depth(e) > MAX_DEPTH {
        return err(
            "54001",
            format!("the row-level security policies for table \"{}\" nest too deep to apply", rel_part(table)),
        );
    }
    Ok(())
}

/// Whether `e` has a subquery (Postgres's sublinks).
pub(super) fn has_sublinks(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| found |= matches!(x, Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..)));
    found
}

/// The conditions a row must meet to be seen (Postgres's security quals): each restrictive policy's
/// USING, then the permissive ones' ORed together; with no permissive one, none is (false).
fn quals(perm: &[&Policy], restr: &[&Policy]) -> Result<Vec<Expr>> {
    let p: Vec<Expr> = perm.iter().filter_map(|p| p.using.as_deref()).map(expr_of).collect::<Result<_>>()?;
    let Some(any) = or_all(p) else { return Ok(vec![Expr::Lit(Value::Bool(false))]) };
    if let Some(t) = perm.first() {
        not_too_deep(&t.table, &any)?;
    }
    let mut out: Vec<Expr> = restr.iter().filter_map(|p| p.using.as_deref()).map(expr_of).collect::<Result<_>>()?;
    out.push(any);
    Ok(out)
}

/// The conditions `who` reads `table` by (SELECT's policies, and before them UPDATE's for FOR UPDATE
/// and FOR SHARE), or None when row security doesn't limit it.
pub(super) fn read(v: &View, table: &str, who: Option<&Who>, locking: bool) -> Result<Option<Vec<Expr>>> {
    if !applies(v, table, who)? {
        return Ok(None);
    }
    let boot;
    let who = match who {
        Some(w) => w,
        None => {
            boot = Who::boot("chronos");
            &boot
        }
    };
    let ps = effective(v, table, who)?;
    let mut out = vec![];
    if locking {
        let (p, r) = chosen(&ps, 'w', who);
        out.extend(quals(&p, &r)?);
    }
    let (p, r) = chosen(&ps, 'r', who);
    out.extend(quals(&p, &r)?);
    Ok(Some(out))
}

// ---------------------------------------------------------------- the rows written

/// How a failed check is reported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Kind {
    /// `new row violates row-level security policy for table "t"`
    New,
    /// ON CONFLICT DO UPDATE's row it would change: `new row violates ... (USING expression) ...`
    Conflict,
    /// MERGE's row it would change: `target row violates ... (USING expression) ...`
    Target,
}

/// One check of a row (Postgres's WithCheckOption): the permissive policies' conditions ORed (no
/// name), or one restrictive policy's.
#[derive(Clone, Debug)]
pub(super) struct Wco {
    kind: Kind,
    policy: Option<String>,
    pub(super) expr: Expr,
}

impl Wco {
    fn violated(&self, table: &str) -> SqlError {
        let named = self.policy.as_ref().map_or(String::new(), |p| format!(" \"{p}\""));
        let t = rel_part(table);
        SqlError {
            code: "42501",
            message: match self.kind {
                Kind::New => format!("new row violates row-level security policy{named} for table \"{t}\""),
                Kind::Conflict => {
                    format!("new row violates row-level security policy{named} (USING expression) for table \"{t}\"")
                }
                Kind::Target => {
                    format!("target row violates row-level security policy{named} (USING expression) for table \"{t}\"")
                }
            },
            info: None,
        }
    }
}

/// The checks of the rows written under policies `perm` and `restr`: each one's WITH CHECK, or its
/// USING without one (always its USING with `using`).
fn wcos(kind: Kind, perm: &[&Policy], restr: &[&Policy], using: bool) -> Result<Vec<Wco>> {
    let cond = |p: &Policy| if !using && p.check.is_some() { p.check.clone() } else { p.using.clone() };
    let p: Vec<Expr> = perm.iter().filter_map(|p| cond(p)).map(|t| expr_of(&t)).collect::<Result<_>>()?;
    let Some(any) = or_all(p) else {
        return Ok(vec![Wco { kind, policy: None, expr: Expr::Lit(Value::Bool(false)) }]);
    };
    if let Some(t) = perm.first() {
        not_too_deep(&t.table, &any)?;
    }
    let mut out = vec![Wco { kind, policy: None, expr: any }];
    for r in restr {
        if let Some(t) = cond(r) {
            out.push(Wco { kind, policy: Some(r.name.clone()), expr: expr_of(&t)? });
        }
    }
    Ok(out)
}

/// What the rows a statement writes to a table are checked against (see `write`).
#[derive(Clone, Debug, Default)]
pub(super) struct Checks {
    /// the table the policies are on, as stored
    pub(super) on: String,
    /// each row INSERT or UPDATE writes
    pub(super) new: Vec<Wco>,
    /// each row before MERGE's UPDATE or DELETE changes it, or before ON CONFLICT DO UPDATE does
    pub(super) old: Vec<Wco>,
    /// ON CONFLICT DO UPDATE: the row the one there becomes
    pub(super) upsert: Vec<Wco>,
}

impl Checks {
    /// The checks bound to the columns of table `t` (the table itself, or the partition a row is in).
    pub(super) fn bind(&self, v: &View, t: &Table) -> Result<Bound> {
        let me = rel_part(&self.on).to_string();
        let cols: Vec<Col> = t.cols.iter().map(|c| Col { table: me.clone(), ..c.clone() }).collect();
        let bind = |ws: &[Wco]| -> Result<Vec<Wco>> {
            ws.iter().map(|w| Ok(Wco { expr: Binder::plain(&cols, Some(v)).bind(&w.expr)?, ..w.clone() })).collect()
        };
        Ok(Bound {
            table: self.on.clone(),
            new: bind(&self.new)?,
            old: bind(&self.old)?,
            upsert: bind(&self.upsert)?,
            cols,
        })
    }
}

/// `Checks`, bound to one table's columns.
pub(super) struct Bound {
    table: String,
    pub(super) cols: Vec<Col>,
    pub(super) new: Vec<Wco>,
    pub(super) old: Vec<Wco>,
    pub(super) upsert: Vec<Wco>,
}

impl Bound {
    /// Checks `row` against `wcos` (one of this one's lists): the first that doesn't hold is the error.
    pub(super) fn check(&self, wcos: &[Wco], row: &[Value], params: &[Value], sub: &dyn Runner) -> Result<()> {
        for w in wcos {
            let env = Env { cols: &self.cols, row, params, aggs: &[], sub: Some(sub) };
            if truth(ev(&w.expr, &env)?.as_ref())? != Some(true) {
                return Err(w.violated(&self.table));
            }
        }
        Ok(())
    }
}

/// A write's checks (see `Checks`), bound once for each table its rows are in.
pub(super) struct Writing {
    checks: Option<Checks>,
    bound: RefCell<HashMap<String, std::rc::Rc<Bound>>>,
}

impl Writing {
    /// A write's checks: its statement's (see `write`), none when row security doesn't limit it.
    pub(super) fn new(checks: Option<&Checks>) -> Writing {
        Writing { checks: checks.cloned(), bound: Default::default() }
    }

    fn bound(&self, v: &View, t: &Table) -> Result<Option<std::rc::Rc<Bound>>> {
        let Some(c) = &self.checks else { return Ok(None) };
        if let Some(b) = self.bound.borrow().get(&t.name) {
            return Ok(Some(b.clone()));
        }
        let b = std::rc::Rc::new(c.bind(v, t)?);
        self.bound.borrow_mut().insert(t.name.clone(), b.clone());
        Ok(Some(b))
    }

    /// Checks `row`, as table `t` has it, against the checks `which` picks.
    pub(super) fn check(
        &self,
        v: &View,
        t: &Table,
        which: fn(&Bound) -> &[Wco],
        row: &[Value],
        params: &[Value],
        sub: &dyn Runner,
    ) -> Result<()> {
        match self.bound(v, t)? {
            Some(b) => b.check(which(&b), row, params, sub),
            None => Ok(()),
        }
    }
}

/// The rows written (INSERT's and UPDATE's).
pub(super) fn new_rows(b: &Bound) -> &[Wco] {
    &b.new
}

/// The rows before they change (MERGE's UPDATE and DELETE, ON CONFLICT DO UPDATE's).
pub(super) fn old_rows(b: &Bound) -> &[Wco] {
    &b.old
}

/// The rows ON CONFLICT DO UPDATE makes.
pub(super) fn upserted_rows(b: &Bound) -> &[Wco] {
    &b.upsert
}

thread_local! {
    /// The statement being rewritten is one of MERGE's (see `Exec::merge_into`)
    static MERGING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the statement being rewritten is one of MERGE's (see `merging`).
pub(super) fn merging_now() -> bool {
    MERGING.get()
}

/// Rewrites `f`'s statements as MERGE's: UPDATE and DELETE check their policies on the rows they
/// change instead of passing them by (their rows were chosen by the MERGE's join).
pub(super) fn merging<T>(f: impl FnOnce() -> T) -> T {
    let was = MERGING.replace(true);
    let out = f();
    MERGING.set(was);
    out
}

/// Row security for the table statement `st` writes (a write to a view is one to its table by then),
/// as `who`: UPDATE's and DELETE's conditions first in their WHERE, and the checks of the rows INSERT
/// and UPDATE write in the statement, for its run (`Writing`). `expand` expands the views and policies
/// the conditions read. Whether `st` changed.
pub(super) fn write(
    v: &View,
    st: &mut Stmt,
    who: Option<&Who>,
    expand: &mut dyn FnMut(&mut Expr) -> Result<()>,
) -> Result<bool> {
    let (table, cmd) = match &*st {
        Stmt::Insert { table, .. } => (table.clone(), 'a'),
        Stmt::Update { table, .. } => (table.clone(), 'w'),
        Stmt::Delete { table, .. } => (table.clone(), 'd'),
        _ => return Ok(false),
    };
    read_only(v, &table)?;
    if !applies(v, &table, who)? {
        return Ok(false);
    }
    let boot;
    let who = match who {
        Some(w) => w,
        None => {
            boot = Who::boot("chronos");
            &boot
        }
    };
    let merging = MERGING.get();
    let ps = effective(v, &table, who)?;
    // (SELECT's policies count where SELECT is needed: a WHERE, SET or RETURNING that reads the table)
    let reads = acl::reads_target(v, st, &table)?;
    let (sp, sr) = chosen(&ps, 'r', who);
    let mut c = Checks { on: table.clone(), ..Checks::default() };
    let mut conds = vec![];
    match cmd {
        'a' => {
            let (p, r) = chosen(&ps, 'a', who);
            c.new = wcos(Kind::New, &p, &r, false)?;
            let upsert = matches!(&*st, Stmt::Insert { conflict: Some(OnConflict { update: Some(_), .. }), .. });
            if reads || upsert {
                c.new.extend(wcos(Kind::New, &sp, &sr, true)?);
            }
            if upsert {
                let (up, ur) = chosen(&ps, 'w', who);
                c.old = wcos(Kind::Conflict, &up, &ur, true)?;
                c.old.extend(wcos(Kind::Conflict, &sp, &sr, true)?);
                c.upsert = wcos(Kind::New, &up, &ur, false)?;
                c.upsert.extend(wcos(Kind::New, &sp, &sr, true)?);
            }
        }
        'w' => {
            let (up, ur) = chosen(&ps, 'w', who);
            if merging {
                c.old = wcos(Kind::Target, &up, &ur, true)?;
            } else {
                if reads {
                    conds.extend(quals(&sp, &sr)?);
                }
                conds.extend(quals(&up, &ur)?);
            }
            c.new = wcos(Kind::New, &up, &ur, false)?;
            if reads || merging {
                c.new.extend(wcos(Kind::New, &sp, &sr, true)?);
            }
        }
        _ => {
            let (dp, dr) = chosen(&ps, 'd', who);
            if merging {
                c.old = wcos(Kind::Target, &dp, &dr, true)?;
            } else {
                if reads {
                    conds.extend(quals(&sp, &sr)?);
                }
                conds.extend(quals(&dp, &dr)?);
            }
        }
    }
    for w in c.new.iter_mut().chain(c.old.iter_mut()).chain(c.upsert.iter_mut()) {
        expand(&mut w.expr)?;
    }
    let (Stmt::Insert { checks, .. } | Stmt::Update { checks, .. } | Stmt::Delete { checks, .. }) = &mut *st else {
        unreachable!()
    };
    *checks = Some(Box::new(c));
    let Some(mut cond) = and_all(conds) else {
        policed_target(st, None);
        return Ok(true);
    };
    not_too_deep(&table, &cond)?;
    expand(&mut cond)?;
    // UPDATE ... FROM's and DELETE ... USING's join finds the rows they change: the target in it is the
    // rows these conditions let them change (and no SELECT policy reads it unless the statement does)
    policed_target(st, Some(&cond));
    // (over the table's own name, as the statement's other conditions are)
    let me = rel_part(&table);
    if me != table {
        cond = unalias(&cond, me, &table);
    }
    let (Stmt::Update { filter, .. } | Stmt::Delete { filter, .. }) = st else { unreachable!() };
    *filter = Some(match filter.take() {
        Some(f) => bin(cond, Op::And, f),
        None => cond,
    });
    Ok(true)
}

/// The target in UPDATE ... FROM's (DELETE ... USING's) join (see `joined_select`), its row security
/// settled: a subquery of the rows `cond` (over the table's own name) lets the statement change, or
/// the table as it is (MERGE's statements check theirs on each row).
fn policed_target(st: &mut Stmt, cond: Option<&Expr>) {
    let (Stmt::Update { from: Some(q), .. } | Stmt::Delete { from: Some(q), .. }) = st else { return };
    let Some(target) = &mut q.from else { return };
    target.policed = true;
    let Some(cond) = cond else { return };
    let mut inner = target.clone();
    inner.alias = rel_part(&target.table).to_string();
    let mut body = select_of(Expr::Lit(Value::Null));
    body.items = vec![Item::Star];
    body.from = Some(inner);
    body.filter = Some(cond.clone());
    target.sub = Some(Box::new(body));
}

// ---------------------------------------------------------------- statements

/// CREATE, ALTER and DROP POLICY.
#[derive(Clone, Debug)]
pub(crate) enum PolicyStmt {
    Create {
        name: String,
        table: String,
        permissive: bool,
        cmd: char,
        roles: Option<Vec<Spec>>,
        using: Option<String>,
        check: Option<String>,
    },
    Alter {
        name: String,
        table: String,
        rename: Option<String>,
        roles: Option<Vec<Spec>>,
        using: Option<String>,
        check: Option<String>,
    },
    Drop {
        name: String,
        table: String,
        if_exists: bool,
    },
}

impl Parser {
    /// CREATE POLICY name ON table [AS PERMISSIVE | RESTRICTIVE] [FOR ALL | SELECT | INSERT | UPDATE
    /// | DELETE] [TO role, ...] [USING (cond)] [WITH CHECK (cond)], after CREATE POLICY.
    pub(super) fn create_policy(&mut self) -> Result<Stmt> {
        let name = self.name()?;
        self.expect_kw("on")?;
        let table = self.table_name()?;
        let mut permissive = true;
        if self.kw("as") {
            permissive = !self.kw("restrictive");
            if permissive && !self.kw("permissive") {
                // (Postgres's: any name parses there, and is then refused)
                let Some(Tok::Word(w) | Tok::Ident(w)) = self.peek().cloned() else {
                    return self.fail("expected PERMISSIVE or RESTRICTIVE");
                };
                let mut e = self.syntax_at(self.i, String::new(), "");
                e.message = format!("unrecognized row security option \"{w}\"");
                if let Some(info) = e.info.as_mut() {
                    info.hint = Some("Only PERMISSIVE or RESTRICTIVE policies are supported currently.".into());
                }
                return Err(e);
            }
        }
        let cmd = if self.kw("for") {
            match self.next() {
                Some(Tok::Word(w)) if w == "all" => '*',
                Some(Tok::Word(w)) if w == "select" => 'r',
                Some(Tok::Word(w)) if w == "insert" => 'a',
                Some(Tok::Word(w)) if w == "update" => 'w',
                Some(Tok::Word(w)) if w == "delete" => 'd',
                _ => return self.fail("expected ALL, SELECT, INSERT, UPDATE or DELETE"),
            }
        } else {
            '*'
        };
        let roles = if self.kw("to") { Some(self.role_specs_public()?) } else { None };
        let using = if self.kw("using") { Some(self.policy_cond()?) } else { None };
        let check = if self.kw("with") {
            self.expect_kw("check")?;
            Some(self.policy_cond()?)
        } else {
            None
        };
        Ok(Stmt::Policy(Box::new(PolicyStmt::Create { name, table, permissive, cmd, roles, using, check })))
    }

    /// ALTER POLICY name ON table RENAME TO new | [TO role, ...] [USING (cond)] [WITH CHECK (cond)],
    /// after ALTER POLICY.
    pub(super) fn alter_policy(&mut self) -> Result<Stmt> {
        let name = self.name()?;
        self.expect_kw("on")?;
        let table = self.table_name()?;
        if self.kw("rename") {
            self.expect_kw("to")?;
            let to = self.name()?;
            return Ok(Stmt::Policy(Box::new(PolicyStmt::Alter {
                name,
                table,
                rename: Some(to),
                roles: None,
                using: None,
                check: None,
            })));
        }
        let roles = if self.kw("to") { Some(self.role_specs_public()?) } else { None };
        let using = if self.kw("using") { Some(self.policy_cond()?) } else { None };
        let check = if self.kw("with") {
            self.expect_kw("check")?;
            Some(self.policy_cond()?)
        } else {
            None
        };
        if roles.is_none() && using.is_none() && check.is_none() {
            return self.fail("expected RENAME TO, TO, USING or WITH CHECK");
        }
        Ok(Stmt::Policy(Box::new(PolicyStmt::Alter { name, table, rename: None, roles, using, check })))
    }

    /// DROP POLICY [IF EXISTS] name ON table [CASCADE | RESTRICT], after DROP POLICY.
    pub(super) fn drop_policy(&mut self) -> Result<Stmt> {
        let if_exists = self.if_exists()?;
        let name = self.name()?;
        self.expect_kw("on")?;
        let table = self.table_name()?;
        let _ = self.kw("cascade") || self.kw("restrict"); // nothing depends on a policy
        Ok(Stmt::Policy(Box::new(PolicyStmt::Drop { name, table, if_exists })))
    }

    /// ROW LEVEL SECURITY, after ALTER TABLE's ENABLE, DISABLE, FORCE or NO FORCE.
    pub(super) fn row_level_security(&mut self) -> Result<()> {
        self.expect_kw("row")?;
        self.expect_kw("level")?;
        self.expect_kw("security")
    }

    /// `(cond)`: the condition, as SQL, with the names it reads that the search_path finds outside
    /// public written with their schemas (see `bind`).
    fn policy_cond(&mut self) -> Result<String> {
        self.expect_sym("(")?;
        // copied first: parsing takes the text out of the tokens it reads
        let (start, toks) = (self.i, self.ahead(true));
        let outer = self.binding.replace(vec![]);
        let read = self.expr();
        let bound = std::mem::replace(&mut self.binding, outer).unwrap_or_default();
        read?;
        let mut out: Vec<Tok> = vec![];
        for (i, t) in self.since(&toks, start)?.iter().enumerate() {
            match bound.iter().find(|(at, ..)| *at == start + i) {
                Some((_, schema, name)) => {
                    out.extend([Tok::Ident(schema.clone()), Tok::Sym("."), Tok::Ident(name.clone())]);
                }
                None => out.push(t.clone()),
            }
        }
        let text = tokens_shown(&out);
        self.expect_sym(")")?;
        Ok(text)
    }
}

/// The roles of a TO list, by oid (none: PUBLIC); PUBLIC among others is PUBLIC alone, as in
/// Postgres.
fn role_oids(reg: &roles::Registry, specs: &Option<Vec<Spec>>) -> Result<Vec<i64>> {
    let Some(specs) = specs else { return Ok(vec![PUBLIC]) };
    let mut out = vec![];
    for (i, s) in specs.iter().enumerate() {
        if *s == Spec::Public {
            if specs.len() > 1 || i > 0 {
                notice("WARNING", "ignoring specified roles other than PUBLIC".into());
            }
            return Ok(vec![PUBLIC]);
        }
        out.push(s.oid(reg)?);
    }
    Ok(out)
}

/// The table a policy statement is on: it has to be one (42P01, 42809), and the running role has to
/// own it (42501, `must be owner of <noun> t`).
fn target(v: &View, table: &str, noun: &str) -> Result<Table> {
    match relation_kind(v, table)? {
        Some("table") => {}
        Some(_) => return err("42809", format!("\"{}\" is not a table", rel_part(table))),
        None => return err(NO_TABLE, format!("relation \"{}\" does not exist", shown_name(table))),
    }
    let t = need_table(v, table)?;
    if let Some(who) = roles::current().filter(|w| !w.superuser) {
        let owner = acl::rel(v, &t.name)?.map_or(BOOTSTRAP, |o| o.owner);
        if !who.has_privs_of(owner) {
            return err("42501", format!("must be owner of {noun} {}", rel_part(table)));
        }
    }
    Ok(t)
}

/// Checks a policy's condition against its table, as Postgres does when it is made: its columns are
/// the table's, it is a boolean, and it has no aggregate, window function, set-returning function or
/// parameter.
fn checked(v: &View, t: &Table, text: &Option<String>) -> Result<()> {
    let Some(text) = text else { return Ok(()) };
    let e = expr_of(text)?;
    let mut srf = false;
    walk(&e, &mut |x| srf |= matches!(x, Expr::Call(c) if is_srf(&c.name)));
    if has_agg(&e) {
        return err("42803", "aggregate functions are not allowed in policy expressions");
    }
    if has_window(&e) {
        return err("42P20", "window functions are not allowed in policy expressions");
    }
    if srf {
        return err(UNSUPPORTED, "set-returning functions are not allowed in policy expressions");
    }
    if max_param(&e) > 0 {
        return err(NO_PARAM, format!("there is no parameter ${}", max_param(&e)));
    }
    let mut q = select_of(e.clone());
    q.from = Some(Source {
        table: t.name.clone(),
        alias: rel_part(&t.name).into(),
        sub: None,
        names: vec![],
        rec: None,
        working: false,
        at: None,
        lateral: false,
        srf: None,
        cte: 0,
        only: true,
        hier: false,
        policed: true,
        bound: false,
    });
    // (the views it reads, as their queries)
    expand_select(v, &mut q, None, true, 0, &Expand::default())?;
    let ty = select_columns(v, &q, &[])?.first().map(|c| c.1);
    match ty {
        Some(Type::Bool) | None => Ok(()),
        _ if matches!(e, Expr::Lit(Value::Null)) => Ok(()),
        Some(ty) => err(WRONG_TYPE, format!("argument of POLICY must be type boolean, not type {}", ty.pg_name())),
    }
}

/// Runs a policy statement in `branch`.
pub(super) fn run(db: &Db, branch: &str, st: &PolicyStmt) -> Result<Output> {
    match st {
        PolicyStmt::Create { name, table, permissive, cmd, roles, using, check } => {
            if matches!(cmd, 'r' | 'd') && check.is_some() {
                return err(SYNTAX, "WITH CHECK cannot be applied to SELECT or DELETE");
            }
            if *cmd == 'a' && using.is_some() {
                return err(SYNTAX, "only WITH CHECK expression allowed for INSERT");
            }
            let reg = roles::registry(&db.core)?;
            let oids = role_oids(&reg, roles)?;
            db.batch_with(branch, |v| {
                let t = target(v, table, "table")?;
                checked(v, &t, using)?;
                checked(v, &t, check)?;
                let p = Policy {
                    name: name.clone(),
                    table: t.name.clone(),
                    cmd: *cmd,
                    permissive: *permissive,
                    roles: oids.clone(),
                    using: using.clone(),
                    check: check.clone(),
                    // (after the table's others: two worlds' new ones merged may share a place, and then go by name)
                    made: policies(v, &t.name)?.iter().map(|p| p.made).max().unwrap_or(0) + 1,
                };
                if v.get(&p.key())?.is_some() {
                    return err(
                        "42710",
                        format!("policy \"{name}\" for table \"{}\" already exists", rel_part(&t.name)),
                    );
                }
                Ok((vec![(p.key(), Some(p.raw()))], ()))
            })?;
            Ok(Output::tag("CREATE POLICY"))
        }
        PolicyStmt::Alter { name, table, rename, roles, using, check } => {
            let reg = roles::registry(&db.core)?;
            let oids = roles.as_ref().map(|_| role_oids(&reg, roles)).transpose()?;
            db.batch_with(branch, |v| {
                let t = target(v, table, "table")?;
                let missing = || {
                    err::<()>("42704", format!("policy \"{name}\" for table \"{}\" does not exist", rel_part(&t.name)))
                };
                if let Some(to) = rename {
                    if v.get(&policy_key(&t.name, to))?.is_some() {
                        return err(
                            "42710",
                            format!("policy \"{to}\" for table \"{}\" already exists", rel_part(&t.name)),
                        );
                    }
                    let Some(raw) = v.get(&policy_key(&t.name, name))? else { return missing().map(|_| (vec![], ())) };
                    let mut p = Policy::parse(&t.name, name, &raw);
                    let old = p.key();
                    p.name = to.clone();
                    return Ok((vec![(old, None), (p.key(), Some(p.raw()))], ()));
                }
                checked(v, &t, using)?;
                checked(v, &t, check)?;
                let Some(raw) = v.get(&policy_key(&t.name, name))? else { return missing().map(|_| (vec![], ())) };
                let mut p = Policy::parse(&t.name, name, &raw);
                if matches!(p.cmd, 'r' | 'd') && check.is_some() {
                    return err(SYNTAX, "only USING expression allowed for SELECT, DELETE");
                }
                if p.cmd == 'a' && using.is_some() {
                    return err(SYNTAX, "only WITH CHECK expression allowed for INSERT");
                }
                if let Some(o) = &oids {
                    p.roles.clone_from(o);
                }
                if using.is_some() {
                    p.using.clone_from(using);
                }
                if check.is_some() {
                    p.check.clone_from(check);
                }
                Ok((vec![(p.key(), Some(p.raw()))], ()))
            })?;
            Ok(Output::tag("ALTER POLICY"))
        }
        PolicyStmt::Drop { name, table, if_exists } => {
            let mut skipped = None;
            db.batch_with(branch, |v| {
                if relation_kind(v, table)?.is_none() && *if_exists {
                    skipped = Some(format!("relation \"{}\" does not exist, skipping", shown_name(table)));
                    return Ok((vec![], ()));
                }
                let t = target(v, table, "relation")?;
                let key = policy_key(&t.name, name);
                if v.get(&key)?.is_none() {
                    if *if_exists {
                        skipped = Some(format!(
                            "policy \"{name}\" for relation \"{}\" does not exist, skipping",
                            shown_name(&t.name)
                        ));
                        return Ok((vec![], ()));
                    }
                    return err(
                        "42704",
                        format!("policy \"{name}\" for table \"{}\" does not exist", rel_part(&t.name)),
                    );
                }
                Ok((vec![(key, None)], ()))
            })?;
            if let Some(m) = skipped {
                notice("NOTICE", m);
            }
            Ok(Output::tag("DROP POLICY"))
        }
    }
}

/// ALTER TABLE ... ENABLE | DISABLE (`enable`) and FORCE | NO FORCE (`force`) ROW LEVEL SECURITY's
/// write, for table `table` (which a change before it may have renamed).
pub(super) fn alter_flags(
    v: &View,
    table: &str,
    enable: Option<bool>,
    force: Option<bool>,
) -> Result<(String, Option<String>)> {
    match relation_kind(v, table)? {
        Some("table") => {}
        Some(_) => return err("42809", format!("\"{}\" is not a table", rel_part(table))),
        None => return err(NO_TABLE, format!("relation \"{}\" does not exist", shown_name(table))),
    }
    let (e, f) = match v.get(&flags_key(table))? {
        Some(raw) => parse_flags(&raw),
        None => (false, false),
    };
    Ok(flags_write(table, enable.unwrap_or(e), force.unwrap_or(f)))
}

// ---------------------------------------------------------------- renames, drops and roles

/// The tables a statement is about to rename, as (from, to), worked out before it runs.
pub(crate) fn moves(v: &View, st: &Stmt) -> Result<Vec<(String, String)>> {
    if !v.core.has_row_security() {
        return Ok(vec![]);
    }
    // (a view a policy reads: renamed in it too)
    if let Stmt::RenameView { name, to, .. } = st {
        let stored = qualify(schema_part(name), to);
        return Ok((relation_kind(v, name)?.is_some() && *name != stored)
            .then(|| (name.clone(), stored))
            .into_iter()
            .collect());
    }
    let Stmt::Alter { table, changes, .. } = st else { return Ok(vec![]) };
    let Ok(t) = need_table(v, table) else { return Ok(vec![]) };
    Ok(changes
        .iter()
        .filter_map(|c| match c {
            Alter::RenameTable(to) => Some((t.name.clone(), qualify(schema_part(&t.name), to))),
            Alter::SetSchema(s) => Some((t.name.clone(), qualify(s, rel_part(&t.name)))),
            _ => None,
        })
        .filter(|(a, b)| a != b)
        .collect())
}

/// After a statement that may have dropped or renamed tables: their flags and policies move with
/// them, or go with them.
pub(crate) fn after(db: &Db, branch: &str, moves: Vec<(String, String)>) -> Result<()> {
    if !db.core.has_row_security() {
        return Ok(());
    }
    let v = db.core.view_tx(branch)?;
    let (flags, pols) = (all_flags(&v)?, all(&v)?);
    if flags.is_empty() && pols.is_empty() {
        return Ok(());
    }
    let moved = |t: &str| moves.iter().find(|(from, _)| from == t).map(|(_, to)| to.clone());
    let mut writes: Vec<(String, Option<String>)> = vec![];
    let mut live = HashMap::new();
    let mut is_table = |t: &str| -> Result<bool> {
        if let Some(l) = live.get(t) {
            return Ok(*l);
        }
        let l = relation_kind(&v, t)? == Some("table");
        live.insert(t.to_string(), l);
        Ok(l)
    };
    for (t, (e, f)) in &flags {
        match moved(t) {
            Some(to) => {
                writes.push((flags_key(t), None));
                writes.push(flags_write(&to, *e, *f));
            }
            None if !is_table(t)? => writes.push((flags_key(t), None)),
            None => {}
        }
    }
    for p in &pols {
        let mut q = Policy { table: moved(&p.table).unwrap_or_else(|| p.table.clone()), ..p.clone() };
        // what it reads, renamed or moved: it reads it still (as do its own table's columns, by its new name)
        for (from, to) in &moves {
            for c in [&mut q.using, &mut q.check] {
                if let Some(text) = c.as_deref()
                    && let Some(new) = moved_in(&v, &p.table, text, from, to)?
                {
                    *c = Some(new);
                }
            }
        }
        match moved(&p.table) {
            Some(_) => {
                writes.push((p.key(), None));
                writes.push((q.key(), Some(q.raw())));
            }
            None if !is_table(&p.table)? => writes.push((p.key(), None)),
            None if q != *p => writes.push((q.key(), Some(q.raw()))),
            None => {}
        }
    }
    drop(v);
    if writes.is_empty() {
        return Ok(());
    }
    db.batch_with::<(), SqlError>(branch, |_| Ok((writes, ())))?;
    Ok(())
}

// ---------------------------------------------------------------- what a policy reads

/// What a column reference of a policy's condition reads (see `Resolver`).
#[derive(Clone, Debug, PartialEq)]
enum Of {
    /// a column of this relation (a table or view), as stored
    Rel(String),
    /// an output of a subquery, function or WITH query that names its columns, or no column at all
    Other,
    /// an output of a subquery that reads these relations and whose columns aren't named (`*`), or of a NATURAL
    /// join of them: it may be one of theirs
    Via(Vec<String>),
    /// can't tell: an unqualified name more than one source (or one whose columns aren't known) may have
    Unsure,
}

/// A source of one query level of a condition, as `Resolver` sees it.
#[derive(Clone)]
struct Src {
    alias: String,
    /// the relation it reads, as stored (None: a subquery, a function or a WITH query)
    rel: Option<String>,
    /// its columns' names (None: not known, as a function's, or a `*` subquery's)
    cols: Option<Vec<String>>,
    /// the relations a subquery reads, at any depth of its FROM
    reads: Vec<String>,
}

/// A column reference: its qualifier and name as written, what it reads, and whether it is a select list's item
/// of its own (the output named after it).
struct Ref {
    qual: Option<String>,
    name: String,
    of: Of,
    item: bool,
}

/// Works out what a policy's condition reads, as Postgres binds it when the policy is made: each column
/// reference resolved through the query levels around it (the policy's table outermost) to the relation whose
/// column it is, and every relation read in a FROM. `mark` is a name standing in for another (see `marked`).
struct Resolver<'a, 'v> {
    v: &'a View<'v>,
    levels: Vec<Vec<Src>>,
    refs: Vec<Ref>,
    rels: Vec<String>,
    mark: Option<(&'static str, String)>,
}

impl<'a, 'v> Resolver<'a, 'v> {
    /// Resolves condition `e` of a policy on `table`, which it calls `alias`; with `mark`, references to `MARK`
    /// are looked up as that name.
    fn run(v: &'a View<'v>, table: &str, alias: &str, e: &Expr, mark: Option<&str>) -> Self {
        let own = Src { alias: alias.into(), rel: Some(table.into()), cols: columns_of(v, table), reads: vec![] };
        let mark = mark.map(|m| (MARK, m.to_string()));
        let mut r = Resolver { v, levels: vec![vec![own]], refs: vec![], rels: vec![], mark };
        r.expr(e, false);
        r
    }

    /// The name a reference's `n` stands for.
    fn looked_up(&self, n: &str) -> String {
        match &self.mark {
            Some((m, was)) if *m == n => was.clone(),
            _ => n.to_string(),
        }
    }

    fn resolve(&self, q: Option<&str>, n: &str) -> Of {
        let n = self.looked_up(n);
        let n = n.as_str();
        for level in self.levels.iter().rev() {
            match q {
                Some(q) => {
                    if let Some(s) = level.iter().find(|s| s.alias == q) {
                        return of_src(s);
                    }
                }
                None => {
                    let has: Vec<&Src> =
                        level.iter().filter(|s| s.cols.as_ref().is_some_and(|c| c.iter().any(|x| x == n))).collect();
                    let unknown = level.iter().any(|s| s.cols.is_none() && (s.rel.is_some() || !s.reads.is_empty()));
                    match (&has[..], unknown) {
                        ([], false) => continue,
                        ([one], false) => return of_src(one),
                        _ => return Of::Unsure,
                    }
                }
            }
        }
        Of::Other
    }

    fn expr(&mut self, e: &Expr, item: bool) {
        let (mut cols, mut subs) = (vec![], vec![]);
        walk(e, &mut |x| match x {
            Expr::Col(q, n) => cols.push((q.clone(), n.clone())),
            Expr::Sub(sq) | Expr::Exists(sq, _) | Expr::InSub(_, sq, _) => subs.push((*sq.q).clone()),
            _ => {}
        });
        let bare = item && matches!(e, Expr::Col(..));
        for (q, n) in cols {
            let of = self.resolve(q.as_deref(), &n);
            self.refs.push(Ref { qual: q, name: n, of, item: bare });
        }
        for q in subs {
            self.select(&q);
        }
    }

    fn select(&mut self, q: &Select) {
        let srcs: Vec<&Source> = q.from.iter().chain(q.joins.iter().map(|j| &j.src)).collect();
        let mut level: Vec<Src> = vec![];
        for s in &srcs {
            let plain = s.sub.is_none() && s.srf.is_none() && s.rec.is_none() && !s.working;
            if plain {
                self.rels.push(s.table.clone());
            }
            level.push(match (plain, s.names.is_empty()) {
                (true, true) => Src {
                    alias: s.alias.clone(),
                    rel: Some(s.table.clone()),
                    cols: columns_of(self.v, &s.table),
                    reads: vec![],
                },
                // (its columns renamed: what they read is the relation's, by place)
                (true, false) => Src { alias: s.alias.clone(), rel: None, cols: None, reads: vec![s.table.clone()] },
                (false, _) => {
                    let body = s.sub.as_deref().or(s.rec.as_ref().map(|r| &r.base));
                    let cols = match (&s.names[..], body) {
                        ([], Some(b)) => output_cols(b),
                        ([], None) => None,
                        (names, _) => Some(names.to_vec()),
                    };
                    let mut reads = vec![];
                    if let Some(b) = body {
                        from_reads(b, &mut reads);
                    }
                    Src { alias: s.alias.clone(), rel: None, cols, reads }
                }
            });
        }
        // FROM's subqueries and WITH queries: scopes of their own (a LATERAL one sees the sources before it)
        for (i, s) in srcs.iter().enumerate() {
            if let Some(sub) = &s.sub {
                if s.lateral {
                    self.levels.push(level[..i].to_vec());
                    self.select(sub);
                    self.levels.pop();
                } else {
                    self.select(sub);
                }
            }
            if let Some(r) = &s.rec {
                self.select(&r.base);
                self.select(&r.step);
            }
        }
        // a NATURAL join's columns are the ones both sides have: as through a `*`
        for (k, j) in q.joins.iter().enumerate() {
            if j.natural {
                let mut both: Vec<String> =
                    level[..=k + 1].iter().flat_map(|s| s.rel.iter().chain(&s.reads)).cloned().collect();
                both.dedup();
                self.refs.push(Ref { qual: None, name: String::new(), of: Of::Via(both), item: false });
            }
        }
        self.levels.push(level);
        for s in &srcs {
            for a in s.srf.iter().flat_map(|f| f.1.iter()) {
                self.expr(a, false);
            }
        }
        for (k, j) in q.joins.iter().enumerate() {
            if let Some(on) = &j.on {
                self.expr(on, false);
            }
            // USING (a): `a` of the sources before it and of the one it joins
            for u in &j.using {
                let n = self.looked_up(u);
                let level = self.levels.last().expect("pushed");
                let before: Vec<&Src> =
                    level[..=k].iter().filter(|s| s.cols.as_ref().is_none_or(|c| c.contains(&n))).collect();
                let left = match &before[..] {
                    [one] => of_src(one),
                    _ => Of::Unsure,
                };
                let right = of_src(&level[k + 1]);
                for of in [left, right] {
                    self.refs.push(Ref { qual: None, name: u.clone(), of, item: false });
                }
            }
        }
        for i in &q.items {
            if let Item::Expr(e, alias) = i {
                self.expr(e, alias.is_none());
            }
        }
        let rest = q.filter.iter().chain(&q.group).chain(&q.having).chain(&q.distinct_on);
        for e in rest.chain(q.order.iter().map(|(e, _)| e)).chain(&q.limit).chain(&q.offset) {
            self.expr(e, false);
        }
        self.levels.pop();
        for (_, _, p) in &q.set {
            self.select(p);
        }
    }
}

/// What a reference to source `s` reads.
fn of_src(s: &Src) -> Of {
    match (&s.rel, &s.cols) {
        (Some(r), _) => Of::Rel(r.clone()),
        // (its columns not known: maybe one of what it reads, if anything)
        (None, None) => Of::Via(s.reads.clone()),
        (None, Some(_)) => Of::Other,
    }
}

/// The columns of relation `rel` (a table or view), by name; None if there's no such relation.
fn columns_of(v: &View, rel: &str) -> Option<Vec<String>> {
    if let Ok(Some(d)) = view_def(v, rel) {
        return Some(d.cols);
    }
    let t = load_world(v, rel).ok().flatten()?;
    Some(t.cols.iter().filter(|c| !c.hidden).map(|c| c.name.clone()).collect())
}

/// The names of the columns query `q` gives, where its select list names them all (no `*`).
fn output_cols(q: &Select) -> Option<Vec<String>> {
    q.items
        .iter()
        .map(|i| match i {
            Item::Expr(_, Some(a)) => Some(a.clone()),
            Item::Expr(Expr::Col(_, n), None) => Some(n.clone()),
            Item::Expr(..) => Some("?column?".into()),
            Item::Star | Item::TableStar(_) => None,
        })
        .collect()
}

/// The relations `q` reads in its FROM, at any depth of FROM's subqueries.
fn from_reads(q: &Select, out: &mut Vec<String>) {
    for s in q.from.iter().chain(q.joins.iter().map(|j| &j.src)) {
        match &s.sub {
            Some(b) => from_reads(b, out),
            None if s.srf.is_none() && s.rec.is_none() && !s.working => out.push(s.table.clone()),
            None => {}
        }
        if let Some(r) = &s.rec {
            from_reads(&r.base, out);
        }
    }
    for (_, _, p) in &q.set {
        from_reads(p, out);
    }
}

/// A stand-in name no column has (see `marked`).
const MARK: &str = "\u{1}mark";

/// Condition `toks` with token `i` replaced by `MARK`, parsed as a policy's condition is: None where that isn't
/// SQL (the token wasn't a name).
fn marked(toks: &[Tok], i: usize) -> Option<Expr> {
    let mut t = toks.to_vec();
    t[i] = Tok::Ident(MARK.into());
    default_names(|| {
        let mut p = Parser::new(t);
        p.pinned = true;
        let e = p.expr().ok()?;
        p.peek().is_none().then_some(e)
    })
}

/// A name's tokens, quoted only where it has to be.
fn name_toks(n: &str) -> Vec<Tok> {
    lex(&ident(n)).unwrap_or_else(|_| vec![Tok::Ident(n.into())])
}

/// A relation's stored name as a condition writes it: with its schema unless it's public's.
fn rel_toks(stored: &str) -> Vec<Tok> {
    let mut out = vec![];
    if stored.contains('.') {
        out.extend(name_toks(schema_part(stored)));
        out.push(Tok::Sym("."));
    }
    out.extend(name_toks(rel_part(stored)));
    out
}

/// `toks` with each `(at, upto, with)` of `repl` done: tokens `at..upto` replaced by `with`.
fn replaced(toks: &[Tok], mut repl: Vec<(usize, usize, Vec<Tok>)>) -> String {
    repl.sort_by_key(|r| r.0);
    let mut out = vec![];
    let mut i = 0;
    for (at, upto, with) in repl {
        out.extend(toks[i..at].iter().cloned());
        out.extend(with);
        i = upto;
    }
    out.extend(toks[i..].iter().cloned());
    tokens_shown(&out)
}

/// The relations (tables and views) policy `p`'s conditions read, in order: what it depends on.
pub(crate) fn reads_relations(v: &View, p: &Policy) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for text in [&p.using, &p.check].into_iter().flatten() {
        if let Ok(e) = expr_of(text) {
            for r in Resolver::run(v, &p.table, rel_part(&p.table), &e, None).rels {
                if !out.contains(&r) {
                    out.push(r);
                }
            }
        }
    }
    out
}

/// Whether policy `p`'s conditions read column `col` of relation `rel` (or may: through a `*` or a NATURAL join
/// of it, or a name more than one source could mean).
fn reads_column(v: &View, p: &Policy, rel: &str, col: &str) -> bool {
    [&p.using, &p.check].into_iter().flatten().any(|text| {
        let Ok(e) = expr_of(text) else { return true }; // (unreadable: taken to)
        Resolver::run(v, &p.table, rel_part(&p.table), &e, None).refs.iter().any(|r| match &r.of {
            Of::Rel(t) => t == rel && r.name == col,
            Of::Via(reads) => reads.iter().any(|t| t == rel) && (r.name == col || r.name.is_empty()),
            Of::Unsure => r.name == col,
            Of::Other => false,
        })
    })
}

/// Condition `text` of policy `p` with column `a` of relation `rel` renamed `b` wherever it reads it (an output
/// a select list named after it keeps its name: `b AS a`); None if it doesn't read it. Where it reads it in a way
/// the rename can't follow (a `*` or NATURAL join over it, a USING joining it to another relation's column, an
/// unqualified name more than one source could mean), 0A000.
fn renamed_in(v: &View, p: &Policy, text: &str, rel: &str, a: &str, b: &str) -> Result<Option<String>> {
    let toks = lex(text)?;
    let refused = || {
        err(
            UNSUPPORTED,
            format!(
                "cannot rename column \"{a}\" of \"{}\": policy \"{}\" on table \"{}\" reads it where the rename can't \
                 follow (through a * or a NATURAL or USING join); drop the policy, rename the column, then create \
                 the policy again",
                rel_part(rel),
                p.name,
                rel_part(&p.table)
            ),
        )
    };
    // (a NATURAL join over the relation: its columns of the name are joined by it)
    if let Ok(e) = expr_of(text) {
        let r = Resolver::run(v, &p.table, rel_part(&p.table), &e, None);
        if r.refs.iter().any(|x| x.name.is_empty() && matches!(&x.of, Of::Via(reads) if reads.iter().any(|t| t == rel)))
        {
            return refused();
        }
    }
    let mut repl = vec![];
    for (i, t) in toks.iter().enumerate() {
        if !matches!(t, Tok::Word(w) | Tok::Ident(w) if w == a) {
            continue;
        }
        let Some(e) = marked(&toks, i) else { continue };
        let r = Resolver::run(v, &p.table, rel_part(&p.table), &e, Some(a));
        let mine: Vec<&Ref> = r.refs.iter().filter(|x| x.name == MARK).collect();
        let reads = |x: &&Ref| matches!(&x.of, Of::Rel(t) if t == rel);
        let maybe = |x: &&Ref| match &x.of {
            Of::Via(reads) => reads.iter().any(|t| t == rel),
            Of::Unsure => true,
            _ => false,
        };
        if !mine.is_empty() && mine.iter().all(reads) {
            let mut with = name_toks(b);
            if mine.iter().any(|x| x.item) {
                with.push(Tok::Word("as".into()));
                with.extend(name_toks(a));
            }
            repl.push((i, i + 1, with));
        } else if mine.iter().any(|x| reads(x) || maybe(x)) {
            return refused();
        }
    }
    Ok((!repl.is_empty()).then(|| replaced(&toks, repl)))
}

/// ALTER TABLE `table` RENAME COLUMN `a` TO `b` (or a view's): the policies that read the column, with it
/// renamed in their conditions (as their rows' writes), wherever they read it: their own table's, in a subquery
/// or another table's policy, as Postgres follows the rename (see `renamed_in`).
pub(crate) fn renamed_column(v: &View, table: &str, a: &str, b: &str) -> Result<Vec<(String, Option<String>)>> {
    let mut writes = vec![];
    for mut p in all(v)? {
        let mut changed = false;
        for text in [p.using.clone(), p.check.clone()] {
            let Some(text) = text else { continue };
            if let Some(new) = renamed_in(v, &p, &text, table, a, b)? {
                for c in [&mut p.using, &mut p.check] {
                    if c.as_deref() == Some(text.as_str()) {
                        *c = Some(new.clone());
                    }
                }
                changed = true;
            }
        }
        if changed {
            writes.push((p.key(), Some(p.raw())));
        }
    }
    Ok(writes)
}

/// Condition `text` of a policy on table `was` (as it was named) with relation `from` renamed or moved to
/// `to` wherever it names it: in a FROM (keeping the name it was called by) and, when it is the policy's own
/// table, as the qualifier of its columns. None if it doesn't name it.
fn moved_in(v: &View, was: &str, text: &str, from: &str, to: &str) -> Result<Option<String>> {
    let toks = lex(text)?;
    let name = rel_part(from);
    let mut repl = vec![];
    for (i, t) in toks.iter().enumerate() {
        if !matches!(t, Tok::Word(w) | Tok::Ident(w) if w == name) {
            continue;
        }
        let Some(e) = marked(&toks, i) else { continue };
        // written with its schema: `s . name`
        let qualified = i >= 2 && toks[i - 1] == Tok::Sym(".") && matches!(toks[i - 2], Tok::Word(_) | Tok::Ident(_));
        let stand_in = if from.contains('.') { qualify(schema_part(from), MARK) } else { MARK.to_string() };
        // in a FROM: the relation itself
        let mut found = None;
        each_subquery(&e, &mut |q| {
            visit_sources(q, &mut |s| {
                if s.sub.is_none() && s.srf.is_none() && s.table == stand_in {
                    found = Some(s.alias == MARK);
                }
            })
        });
        if let Some(unaliased) = found {
            let mut with = rel_toks(to);
            if unaliased {
                with.extend(name_toks(name)); // (still called what it was)
            }
            repl.push((if qualified { i - 2 } else { i }, i + 1, with));
            continue;
        }
        // the policy's own table, qualifying its columns
        if from == was && !qualified {
            let r = Resolver::run(v, was, MARK, &e, None);
            if r.refs.iter().any(|x| x.qual.as_deref() == Some(MARK) && x.of == Of::Rel(was.into())) {
                repl.push((i, i + 1, name_toks(rel_part(to))));
            }
        }
    }
    Ok((!repl.is_empty()).then(|| replaced(&toks, repl)))
}

/// Every source of `q`, and of the queries inside it.
fn visit_sources(q: &Select, f: &mut dyn FnMut(&Source)) {
    for s in q.from.iter().chain(q.joins.iter().map(|j| &j.src)) {
        f(s);
        if let Some(b) = &s.sub {
            visit_sources(b, f);
        }
        if let Some(r) = &s.rec {
            visit_sources(&r.base, f);
            visit_sources(&r.step, f);
        }
        for a in s.srf.iter().flat_map(|x| x.1.iter()) {
            each_subquery(a, &mut |b| visit_sources(b, f));
        }
    }
    for (_, _, p) in &q.set {
        visit_sources(p, f);
    }
    for e in select_exprs(q) {
        each_subquery(e, &mut |b| visit_sources(b, f));
    }
}

/// `f` on each subquery of `e` (not those inside them: see `visit_sources`).
fn each_subquery(e: &Expr, f: &mut dyn FnMut(&Select)) {
    walk(e, &mut |x| {
        if let Expr::Sub(sq) | Expr::Exists(sq, _) | Expr::InSub(_, sq, _) = x {
            f(&sq.q);
        }
    });
}

/// ALTER TABLE `table` DROP COLUMN `col`: the policies that read the column go with CASCADE (their
/// rows' removal), with Postgres's notice; without, 2BP01 as Postgres says.
pub(crate) fn dropped_column(v: &View, table: &str, col: &str, cascade: bool) -> Result<Vec<(String, Option<String>)>> {
    if !v.core.has_row_security() {
        return Ok(vec![]);
    }
    let reading: Vec<Policy> = all(v)?.into_iter().filter(|p| reads_column(v, p, table, col)).collect();
    if reading.is_empty() {
        return Ok(vec![]);
    }
    let each: Vec<String> =
        reading.iter().map(|p| format!("policy {} on table {}", p.name, rel_part(&p.table))).collect();
    if !cascade {
        let detail = each
            .iter()
            .map(|p| format!("{p} depends on column {col} of table {}", rel_part(table)))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(SqlError {
            code: "2BP01",
            message: format!(
                "cannot drop column {col} of table {} because other objects depend on it",
                rel_part(table)
            ),
            info: Some(Box::new(Violation {
                detail: Some(detail),
                hint: Some("Use DROP ... CASCADE to drop the dependent objects too.".into()),
                ..Violation::default()
            })),
        });
    }
    match &each[..] {
        [one] => notice("NOTICE", format!("drop cascades to {one}")),
        many => notice_detail(
            "NOTICE",
            format!("drop cascades to {} other objects", many.len()),
            many.iter().map(|p| format!("drop cascades to {p}")).collect::<Vec<_>>().join("\n"),
        ),
    }
    Ok(reading.iter().map(|p| (p.key(), None)).collect())
}

/// DROP TABLE or VIEW of relations `gone` (with what goes with them): the policies of other tables that read
/// one, each with the relation it reads, as Postgres lists what depends on them.
pub(crate) fn reading(v: &View, gone: &[(String, &'static str)]) -> Result<Vec<(Policy, String, &'static str)>> {
    if !v.core.has_row_security() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for p in all(v)? {
        if gone.iter().any(|(g, _)| *g == p.table) {
            continue; // (it goes with its table)
        }
        for r in reads_relations(v, &p) {
            if let Some((_, k)) = gone.iter().find(|(g, _)| *g == r) {
                out.push((p.clone(), r, *k));
            }
        }
    }
    Ok(out)
}

/// The policies of world `v` role `oid` is named in, as DROP ROLE's detail lists them.
pub(crate) fn depending(v: &View, oid: i64) -> Result<Vec<String>> {
    Ok(all(v)?
        .into_iter()
        .filter(|p| p.roles.contains(&oid))
        .map(|p| format!("target of policy {} on table {}", p.name, shown_name(&p.table)))
        .collect())
}

/// DROP OWNED BY's part: roles `oids` are taken out of the policies of `branch` that name them, and a
/// policy that named only them goes.
pub(crate) fn drop_owned(db: &Db, branch: &str, oids: &[i64]) -> Result<()> {
    if !db.core.has_row_security() {
        return Ok(());
    }
    db.batch_with::<(), SqlError>(branch, |v| {
        let mut writes = vec![];
        for mut p in all(v)? {
            if !p.roles.iter().any(|r| oids.contains(r)) {
                continue;
            }
            p.roles.retain(|r| !oids.contains(r));
            writes.push((p.key(), (!p.roles.is_empty()).then(|| p.raw())));
        }
        Ok((writes, ()))
    })?;
    Ok(())
}

// ---------------------------------------------------------------- the catalog and DIFF

/// A policy's condition as Postgres writes it back (`(owner = CURRENT_USER)`), as far as Chronos's
/// deparser knows it; else as written, in parentheses.
pub(crate) fn shown(v: &View, p: &Policy, check: bool) -> Option<String> {
    let text = if check { p.check.as_ref()? } else { p.using.as_ref()? };
    let cols = load(v, &p.table).ok().flatten().map(|t| t.cols).unwrap_or_default();
    Some(catalog::expr_def(text, &cols, true))
}

/// How DIFF shows a change to row `key` (one of `_sonos_rls/` or `_sonos_policy/`): the table, what
/// changed, and the policy's name.
pub(crate) fn describe(
    key: &str,
    before: Option<&Json>,
    after: Option<&Json>,
) -> Option<(String, &'static str, Vec<String>)> {
    if let Some(t) = key.strip_prefix(RLS).and_then(|k| k.strip_prefix('/')) {
        let f = |j: Option<&Json>| j.map_or((false, false), |j| (j["enable"] == true, j["force"] == true));
        let ((be, bf), (ae, af)) = (f(before), f(after));
        let change = match () {
            _ if ae && !be => "enable row level security",
            _ if be && !ae => "disable row level security",
            _ if af && !bf => "force row level security",
            _ if bf && !af => "no force row level security",
            _ => "alter row level security",
        };
        return Some((table_of(t), change, vec![]));
    }
    let (t, name) = key.strip_prefix(POLICIES)?.strip_prefix('/')?.split_once('/')?;
    let change = match (before, after) {
        (None, _) => "create policy",
        (_, None) => "drop policy",
        _ => "alter policy",
    };
    Some((table_of(t), change, vec![name.to_string()]))
}

/// A DIFF's change to row security (see `describe`) as the statements that make it.
pub(crate) fn diff_sql(after: &View, d: &Described) -> Option<String> {
    let table = ident_rel(&d.table);
    let flag = |enable: bool| d.after.as_ref().map(|j| j[if enable { "enable" } else { "force" }] == true);
    match d.change {
        "enable row level security"
        | "disable row level security"
        | "force row level security"
        | "no force row level security"
        | "alter row level security" => {
            let enable = flag(true).unwrap_or(false);
            let force = flag(false).unwrap_or(false);
            let mut out =
                vec![format!("alter table {table} {} row level security;", if enable { "enable" } else { "disable" })];
            let forced = d.before.as_ref().is_some_and(|j| j["force"] == true);
            if force != forced || d.before.is_none() && force {
                out.push(format!(
                    "alter table {table} {} row level security;",
                    if force { "force" } else { "no force" }
                ));
            }
            Some(out.join("\n"))
        }
        "drop policy" => Some(format!("drop policy {} on {table};", ident(d.columns.first()?))),
        "create policy" | "alter policy" => {
            let name = d.columns.first()?;
            let p = Policy::parse(&d.table, name, &d.after.as_ref()?.to_string());
            let reg = roles::registry(after.core).ok()?;
            let mut sql = format!("create policy {} on {table}", ident(name));
            if !p.permissive {
                sql += " as restrictive";
            }
            if p.cmd != '*' {
                sql += &format!(" for {}", p.cmd_name().to_lowercase());
            }
            if p.roles != [PUBLIC] {
                let names: Vec<String> = p.roles.iter().map(|o| roles_sql(&reg, *o)).collect();
                sql += &format!(" to {}", names.join(", "));
            }
            if let Some(u) = shown(after, &p, false) {
                sql += &format!(" using ({u})");
            }
            if let Some(c) = shown(after, &p, true) {
                sql += &format!(" with check ({c})");
            }
            sql.push(';');
            Some(match d.change {
                "alter policy" => format!("drop policy {} on {table};\n{sql}", ident(name)),
                _ => sql,
            })
        }
        _ => None,
    }
}

fn roles_sql(reg: &roles::Registry, oid: i64) -> String {
    match oid {
        PUBLIC => "public".into(),
        o => ident(&reg.name_of(o)),
    }
}
