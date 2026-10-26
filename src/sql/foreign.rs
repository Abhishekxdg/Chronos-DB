//! Foreign-data wrappers, foreign servers, user mappings and foreign tables (Postgres's SQL/MED), as
//! Postgres's catalog keeps them: their names, owners and privileges, a wrapper's handler and
//! validator, and the generic options each takes. Nothing here reaches another server: a foreign
//! table is a table whose rows can't be read, as Postgres's are with a wrapper that has no handler.
//!
//! Rows of the world: `_sonos_defs/fdw/<name>` (`{"handler", "validator", "options", "at"}`),
//! `_sonos_defs/server/<name>` (`{"fdw", "type", "version", "options", "at"}`) and
//! `_sonos_defs/mapping/<role oid, 0 for PUBLIC>/<server>` (`{"options", "at"}`); `at` is the order
//! they were made in, which a cascade lists them by. Owners and grants are `acl`'s rows (`fdw/<name>`,
//! `server/<name>`), comments `comments`' (`fdw/<name>`, `server/<name>`). A foreign table's server and
//! options are its schema row's (`ftable`, see `hier::Hier::foreign`): `{"server", "options", "cols":
//! [[column id, options], ...]}`.

use super::acl::{self, Kind as AclKind};
use super::catalog::Catalog;
use super::roles::{self, BOOTSTRAP, PUBLIC, Spec, Who};
use super::*;

const DEFS: &str = "_sonos_defs/";

/// Generic options, in the order they were given: name, value.
pub(crate) type Opts = Vec<(String, String)>;

/// What an option in OPTIONS (...) does to the list (no word is ADD).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum OptOp {
    Add,
    Set,
    Drop,
}

/// OPTIONS (...) as written: each option's action, name and value (empty for DROP).
pub(crate) type OptChanges = Vec<(OptOp, String, String)>;

#[derive(Clone, Debug, Default)]
pub(crate) struct Fdw {
    pub name: String,
    /// the handler function's stored name
    pub handler: Option<String>,
    /// the validator: `postgresql_fdw_validator`, the one built in
    pub validator: Option<String>,
    pub options: Opts,
    pub at: i64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Server {
    pub name: String,
    pub fdw: String,
    /// TYPE
    pub kind: Option<String>,
    pub version: Option<String>,
    pub options: Opts,
    pub at: i64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Mapping {
    /// the role's oid; 0 for PUBLIC
    pub user: i64,
    pub server: String,
    pub options: Opts,
    pub at: i64,
}

/// A world's wrappers, servers and user mappings, each with its oid (see `oids`).
#[derive(Clone, Debug, Default)]
pub(crate) struct Objects {
    pub fdws: Vec<Fdw>,
    pub servers: Vec<Server>,
    pub mappings: Vec<Mapping>,
    fdw_oids: Vec<i64>,
    server_oids: Vec<i64>,
    mapping_oids: Vec<i64>,
}

/// A foreign table's server and options as the catalog shows them, and each column's (by position).
#[derive(Clone, Debug, Default)]
pub(crate) struct TableFdw {
    pub server: String,
    pub options: Opts,
    pub cols: Vec<Opts>,
}

fn fdw_key(name: &str) -> String {
    format!("{DEFS}fdw/{name}")
}

fn server_key(name: &str) -> String {
    format!("{DEFS}server/{name}")
}

fn mapping_key(user: i64, server: &str) -> String {
    format!("{DEFS}mapping/{user}/{server}")
}

/// A write of a server's or a wrapper's row (`key`) marked afresh, with a mark no row had before. Each object made
/// on one (a foreign table, a user mapping, a wrapper's server) changes its row, so a DROP ... CASCADE whose transaction
/// looked for dependents before that object was made deletes a row that has changed since: its merge fails, rather
/// than leave the new object on what is gone. (Two transactions that each make one on the same server or wrapper clash
/// so too, and the second to commit fails, where Postgres would make it wait.)
fn depended_on(v: &View, key: &str) -> Result<(String, Option<String>)> {
    let raw = v.get(key)?.unwrap_or_default();
    let mut j: Json = serde_json::from_str(&raw).unwrap_or_default();
    j["made_on"] = Json::from(new_id());
    Ok((key.to_string(), row_json(j)))
}

fn opts_json(o: &Opts) -> Json {
    Json::Array(o.iter().map(|(k, v)| serde_json::json!([k, v])).collect())
}

fn opts_of(j: &Json) -> Opts {
    j.as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string())))
        .collect()
}

fn text_of(j: &Json, k: &str) -> Option<String> {
    j.get(k).and_then(|x| x.as_str()).map(String::from)
}

/// The world's wrappers, servers and user mappings, each kind in key order.
pub(crate) fn load(v: &View) -> Result<Objects> {
    let mut o = Objects::default();
    let rows = |kind: &str| -> Result<Vec<(String, Json)>> {
        let prefix = format!("{DEFS}{kind}/");
        let mut out = vec![];
        for r in v.prefix(&prefix)? {
            let (k, raw) = r?;
            out.push((k[prefix.len()..].to_string(), serde_json::from_str::<Json>(&raw).unwrap_or_default()));
        }
        Ok(out)
    };
    for (name, j) in rows("fdw")? {
        o.fdws.push(Fdw {
            name,
            handler: text_of(&j, "handler"),
            validator: text_of(&j, "validator"),
            options: opts_of(&j["options"]),
            at: j["at"].as_i64().unwrap_or(0),
        });
    }
    for (name, j) in rows("server")? {
        o.servers.push(Server {
            name,
            fdw: text_of(&j, "fdw").unwrap_or_default(),
            kind: text_of(&j, "type"),
            version: text_of(&j, "version"),
            options: opts_of(&j["options"]),
            at: j["at"].as_i64().unwrap_or(0),
        });
    }
    for (rest, j) in rows("mapping")? {
        let Some((user, server)) = rest.split_once('/') else { continue };
        // (a mapping made before roles were kept by oid: `public`, or the session's role)
        let user = user.parse::<i64>().unwrap_or(if user == "public" { PUBLIC } else { BOOTSTRAP });
        o.mappings.push(Mapping {
            user,
            server: server.to_string(),
            options: opts_of(&j["options"]),
            at: j["at"].as_i64().unwrap_or(0),
        });
    }
    let fdws: Vec<String> = o.fdws.iter().map(|f| f.name.clone()).collect();
    let servers: Vec<String> = o.servers.iter().map(|s| s.name.clone()).collect();
    let mappings: Vec<String> = o.mappings.iter().map(|m| format!("{}/{}", m.user, m.server)).collect();
    o.fdw_oids = oids(0, &fdws);
    o.server_oids = oids(1, &servers);
    o.mapping_oids = oids(2, &mappings);
    Ok(o)
}

/// Where the oids of wrappers, servers and mappings begin: past the objects' worked out from their names
/// (see `catalog::Named`, below 2^29) and below the functions' (2^30 up).
const OIDS: i64 = 1 << 29;

/// The oids of a kind's objects by their names, as `catalog::Named` works them out: FNV-1a of the name in a range
/// of 2^24 of the kind's own, the next free one where two meet (in name order). An object keeps its oid whatever
/// else is made or dropped.
fn oids(kind: i64, keys: &[String]) -> Vec<i64> {
    let slots: i64 = 1 << 24;
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
    let (mut taken, mut out) = (HashSet::new(), vec![0; keys.len()]);
    for k in order {
        let h = keys[k].bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
        let mut slot = i64::from(h) % slots;
        while !taken.insert(slot) {
            slot = (slot + 1) % slots;
        }
        out[k] = OIDS + kind * slots + slot;
    }
    out
}

/// `oids`, for the other objects numbered this way (publications are kinds 8 to 10, statistics objects 11,
/// subscriptions 12, event triggers 13, aggregates 14, text search objects 15 to 18, transforms 19: see
/// `objaddr::SUB_KIND`): kinds 0 to 2 are this module's.
pub(super) fn name_oids(kind: i64, keys: &[String]) -> Vec<i64> {
    oids(kind, keys)
}

impl Objects {
    pub(crate) fn fdw(&self, name: &str) -> Option<&Fdw> {
        self.fdws.iter().find(|f| f.name == name)
    }

    pub(crate) fn server(&self, name: &str) -> Option<&Server> {
        self.servers.iter().find(|s| s.name == name)
    }

    pub(crate) fn fdw_oid(&self, name: &str) -> Option<i64> {
        self.fdws.iter().position(|f| f.name == name).map(|i| self.fdw_oids[i])
    }

    pub(crate) fn server_oid(&self, name: &str) -> Option<i64> {
        self.servers.iter().position(|s| s.name == name).map(|i| self.server_oids[i])
    }

    /// User mapping `i`'s oid (its place in `mappings`).
    pub(crate) fn mapping_oid(&self, i: usize) -> i64 {
        self.mapping_oids[i]
    }

    /// The order the next object made comes in.
    fn next_at(&self) -> i64 {
        let ats = self.fdws.iter().map(|f| f.at).chain(self.servers.iter().map(|s| s.at));
        ats.chain(self.mappings.iter().map(|m| m.at)).max().unwrap_or(0) + 1
    }

    /// The validator of server `name`'s wrapper.
    fn validator_of(&self, server: &str) -> Option<String> {
        self.server(server).and_then(|s| self.fdw(&s.fdw)).and_then(|f| f.validator.clone())
    }
}

/// The name of the wrapper (`fdw`) or server (`server`) with this oid.
pub(crate) fn name_of_oid(v: &View, kind: &str, oid: i64) -> Result<Option<String>> {
    let o = load(v)?;
    Ok(match kind {
        "fdw" => o.fdw_oids.iter().position(|x| *x == oid).map(|i| o.fdws[i].name.clone()),
        _ => o.server_oids.iter().position(|x| *x == oid).map(|i| o.servers[i].name.clone()),
    })
}

/// Whether the wrapper (`fdw`) or server (`server`) named so is there.
pub(crate) fn exists(v: &View, kind: &str, name: &str) -> Result<bool> {
    Ok(match kind {
        "fdw" => v.get(&fdw_key(name))?.is_some(),
        _ => v.get(&server_key(name))?.is_some(),
    })
}

/// 42704 for a wrapper (`fdw`) or server (`server`) that isn't there.
fn missing(kind: AclKind, name: &str) -> SqlError {
    let what = if kind == AclKind::Fdw { "foreign-data wrapper" } else { "server" };
    SqlError { code: "42704", message: format!("{what} \"{name}\" does not exist"), info: None }
}

fn row_json(j: Json) -> Option<String> {
    Some(j.to_string())
}

fn fdw_row(f: &Fdw) -> (String, Option<String>) {
    let mut j = serde_json::json!({ "options": opts_json(&f.options), "at": f.at });
    if let Some(h) = &f.handler {
        j["handler"] = Json::from(h.as_str());
    }
    if let Some(x) = &f.validator {
        j["validator"] = Json::from(x.as_str());
    }
    (fdw_key(&f.name), row_json(j))
}

fn server_row(s: &Server) -> (String, Option<String>) {
    let mut j = serde_json::json!({ "fdw": s.fdw, "options": opts_json(&s.options), "at": s.at });
    if let Some(t) = &s.kind {
        j["type"] = Json::from(t.as_str());
    }
    if let Some(x) = &s.version {
        j["version"] = Json::from(x.as_str());
    }
    (server_key(&s.name), row_json(j))
}

fn mapping_row(m: &Mapping) -> (String, Option<String>) {
    (mapping_key(m.user, &m.server), row_json(serde_json::json!({ "options": opts_json(&m.options), "at": m.at })))
}

// ---------------------------------------------------------------- options

/// The options `old` becomes with `changes`, as Postgres's transformGenericOptions makes them: ADD (or no action)
/// appends one not there yet, SET replaces one in its place, DROP takes one away.
fn transform(old: &Opts, changes: &OptChanges) -> Result<Opts> {
    let mut out = old.clone();
    for (op, k, val) in changes {
        let at = out.iter().position(|(n, _)| n == k);
        match (op, at) {
            (OptOp::Drop, Some(i)) => {
                out.remove(i);
            }
            (OptOp::Set, Some(i)) => out[i].1 = val.clone(),
            (OptOp::Drop | OptOp::Set, None) => return err("42704", format!("option \"{k}\" not found")),
            (OptOp::Add, Some(_)) => return err("42710", format!("option \"{k}\" provided more than once")),
            (OptOp::Add, None) => out.push((k.clone(), val.clone())),
        }
    }
    Ok(out)
}

/// What a validator is given options for: the catalog of the object they are on.
#[derive(Clone, Copy, Debug, PartialEq)]
enum OptsOf {
    Wrapper,
    Server,
    Mapping,
    Table,
    Column,
}

/// The connection options postgresql_fdw_validator takes, and what each may be given on (Postgres's
/// libpq_conninfo_options).
const LIBPQ: [(&str, OptsOf); 15] = [
    ("authtype", OptsOf::Server),
    ("service", OptsOf::Server),
    ("user", OptsOf::Mapping),
    ("password", OptsOf::Mapping),
    ("connect_timeout", OptsOf::Server),
    ("dbname", OptsOf::Server),
    ("host", OptsOf::Server),
    ("hostaddr", OptsOf::Server),
    ("port", OptsOf::Server),
    ("tty", OptsOf::Server),
    ("options", OptsOf::Server),
    ("requiressl", OptsOf::Server),
    ("sslmode", OptsOf::Server),
    ("gsslib", OptsOf::Server),
    ("gssdelegation", OptsOf::Server),
];

/// The edit distance of two names, in characters (each insertion, deletion and substitution costs 1).
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The name among `candidates` closest to `source`, as Postgres's getClosestMatch picks it: at most 4 edits
/// away and at most half the source's length, the first of the closest.
pub(super) fn closest<'a>(source: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<(usize, &str)> = None;
    if source.is_empty() || source.len() > 255 {
        return None;
    }
    for c in candidates {
        let d = distance(source, c);
        if d <= 4 && d <= source.len() / 2 && best.is_none_or(|(b, _)| d < b) {
            best = Some((d, *c));
        }
    }
    best.map(|(_, c)| c)
}

/// The wrapper's validator on options `opts` of an object of kind `of`: postgresql_fdw_validator takes libpq's
/// connection options on a server and a user mapping, and none elsewhere (a wrapper with no validator takes any).
fn validate(validator: Option<&str>, of: OptsOf, opts: &Opts) -> Result<()> {
    if validator.is_none() {
        return Ok(());
    }
    for (k, _) in opts {
        if LIBPQ.iter().any(|(n, c)| *n == k.as_str() && *c == of) {
            continue;
        }
        let valid: Vec<&str> = LIBPQ.iter().filter(|(_, c)| *c == of).map(|(n, _)| *n).collect();
        let hint = match valid.is_empty() {
            true => Some("There are no valid options in this context.".to_string()),
            false => closest(k, &valid).map(|m| format!("Perhaps you meant the option \"{m}\".")),
        };
        return Err(SqlError {
            code: SYNTAX,
            message: format!("invalid option \"{k}\""),
            info: hint.map(|h| Box::new(Violation { hint: Some(h), ..Violation::default() })),
        });
    }
    Ok(())
}

/// An options column's value: Postgres's text array of `name=value`, null for none.
pub(crate) fn options_value(o: &Opts) -> Value {
    if o.is_empty() {
        return Value::Null;
    }
    let items: Vec<String> = o.iter().map(|(k, v)| roles::array_elem(&format!("{k}={v}"))).collect();
    Value::Text(format!("{{{}}}", items.join(",")))
}

// ---------------------------------------------------------------- statements

/// What CREATE, ALTER and DROP of a wrapper or a server name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum What {
    Fdw,
    Server,
}

impl What {
    fn kind(self) -> AclKind {
        match self {
            What::Fdw => AclKind::Fdw,
            What::Server => AclKind::Server,
        }
    }

    fn tag(self, verb: &str) -> String {
        match self {
            What::Fdw => format!("{verb} FOREIGN DATA WRAPPER"),
            What::Server => format!("{verb} SERVER"),
        }
    }
}

/// HANDLER and VALIDATOR as given: Some(None) for NO HANDLER, NO VALIDATOR.
type Funcs = (Option<Option<String>>, Option<Option<String>>);

/// CREATE, ALTER and DROP of foreign-data wrappers, servers and user mappings, and IMPORT FOREIGN SCHEMA.
#[derive(Clone, Debug)]
pub(crate) enum ForeignStmt {
    /// `conflict`: a HANDLER or VALIDATOR given twice (raised as Postgres raises it, after the checks before it)
    CreateFdw {
        name: String,
        funcs: Funcs,
        conflict: Option<SqlError>,
        options: OptChanges,
    },
    AlterFdw {
        name: String,
        funcs: Funcs,
        conflict: Option<SqlError>,
        options: OptChanges,
    },
    CreateServer {
        name: String,
        if_not_exists: bool,
        kind: Option<String>,
        version: Option<String>,
        fdw: String,
        options: OptChanges,
    },
    /// VERSION (Some(None): VERSION NULL), OPTIONS
    AlterServer {
        name: String,
        version: Option<Option<String>>,
        options: OptChanges,
    },
    /// `verb`: create, alter or drop; `guard`: IF NOT EXISTS, IF EXISTS
    Mapping {
        verb: &'static str,
        user: Spec,
        server: String,
        guard: bool,
        options: OptChanges,
    },
    Drop {
        what: What,
        names: Vec<String>,
        if_exists: bool,
        cascade: bool,
    },
    Rename {
        what: What,
        name: String,
        to: String,
    },
    Owner {
        what: What,
        name: String,
        to: Spec,
    },
    /// IMPORT FOREIGN SCHEMA ... FROM SERVER server INTO schema
    Import {
        server: String,
        into: String,
    },
    /// CREATE, ALTER and DROP PUBLICATION (see `publication`)
    Publication(super::publication::PubStmt),
    /// CREATE, ALTER and DROP STATISTICS (see `statistics`)
    Statistics(super::statistics::StatStmt),
    /// CREATE, ALTER and DROP TABLESPACE, and ALTER ... ALL IN TABLESPACE (see `tablespace`)
    Tablespace(super::tablespace::TsStmt),
    /// CREATE and DROP ACCESS METHOD (see `accessmethod`)
    AccessMethod(super::accessmethod::AmStmt),
}

impl ForeignStmt {
    /// Its command tag, as event triggers are told it (see `event_tag`).
    pub(super) fn tag(&self) -> String {
        use super::publication::PubStmt;
        use super::statistics::StatStmt;
        match self {
            ForeignStmt::CreateFdw { .. } => What::Fdw.tag("CREATE"),
            ForeignStmt::AlterFdw { .. } => What::Fdw.tag("ALTER"),
            ForeignStmt::CreateServer { .. } => What::Server.tag("CREATE"),
            ForeignStmt::AlterServer { .. } => What::Server.tag("ALTER"),
            ForeignStmt::Mapping { verb, .. } => format!("{} USER MAPPING", verb.to_uppercase()),
            ForeignStmt::Drop { what, .. } => what.tag("DROP"),
            ForeignStmt::Rename { what, .. } | ForeignStmt::Owner { what, .. } => what.tag("ALTER"),
            ForeignStmt::Import { .. } => "IMPORT FOREIGN SCHEMA".into(),
            ForeignStmt::Publication(p) => match p {
                PubStmt::Create { .. } => "CREATE PUBLICATION",
                PubStmt::Alter { .. } => "ALTER PUBLICATION",
                PubStmt::Drop { .. } => "DROP PUBLICATION",
            }
            .into(),
            ForeignStmt::Statistics(s) => match s {
                StatStmt::Create { .. } => "CREATE STATISTICS",
                StatStmt::Alter { .. } => "ALTER STATISTICS",
                StatStmt::Drop { .. } => "DROP STATISTICS",
            }
            .into(),
            ForeignStmt::Tablespace(t) => t.tag().into(),
            ForeignStmt::AccessMethod(a) => a.tag().into(),
        }
    }
}

/// 42601 `conflicting or redundant options` at token `at`.
fn conflicting(p: &Parser, at: usize) -> SqlError {
    let position = p.starts.get(at).map(|s| *s as u32 + 1);
    SqlError {
        code: SYNTAX,
        message: "conflicting or redundant options".into(),
        info: Some(Box::new(Violation { position, ..Violation::default() })),
    }
}

/// 42809 for a constraint a foreign table can't have, at token `at` (where its CONSTRAINT or its keyword is).
fn not_on_foreign(p: &Parser, at: usize, what: &str) -> SqlError {
    let position = p.starts.get(at).map(|s| *s as u32 + 1);
    SqlError {
        code: "42809",
        message: format!("{what} constraints are not supported on foreign tables"),
        info: Some(Box::new(Violation { position, ..Violation::default() })),
    }
}

impl Parser {
    /// After CREATE, ALTER or DROP, at FOREIGN, SERVER or USER: a foreign-data wrapper, server, user mapping or
    /// foreign table statement.
    pub(super) fn sql_med(&mut self, verb: &str) -> Result<Stmt> {
        if self.is_kw("foreign") && matches!(self.t.get(self.i + 1), Some(Tok::Word(w)) if w == "table") {
            return self.foreign_table(verb);
        }
        let what = if self.kw("foreign") {
            self.expect_kw("data")?;
            self.expect_kw("wrapper")?;
            What::Fdw
        } else if self.kw("server") {
            What::Server
        } else {
            self.expect_kw("user")?;
            self.expect_kw("mapping")?;
            return self.user_mapping_stmt(verb);
        };
        let stmt = match (verb, what) {
            ("drop", _) => {
                let if_exists = self.if_exists()?;
                let mut names = vec![self.name()?];
                while self.sym(",") {
                    names.push(self.name()?);
                }
                ForeignStmt::Drop { what, names, if_exists, cascade: self.cascade() }
            }
            ("create", What::Fdw) => {
                let name = self.name()?;
                let (funcs, conflict) = self.fdw_funcs()?;
                let options = if self.kw("options") { self.generic_options(false)? } else { vec![] };
                ForeignStmt::CreateFdw { name, funcs, conflict, options }
            }
            ("create", What::Server) => {
                let if_not_exists = self.if_not_exists()?;
                let name = self.name()?;
                let kind = if self.kw("type") { Some(self.string()?) } else { None };
                let version = match self.kw("version") {
                    true if self.kw("null") => None,
                    true => Some(self.string()?),
                    false => None,
                };
                self.expect_kw("foreign")?;
                self.expect_kw("data")?;
                self.expect_kw("wrapper")?;
                let fdw = self.name()?;
                let options = if self.kw("options") { self.generic_options(false)? } else { vec![] };
                ForeignStmt::CreateServer { name, if_not_exists, kind, version, fdw, options }
            }
            (_, what) => {
                let name = self.name()?;
                if self.kw("owner") {
                    self.expect_kw("to")?;
                    ForeignStmt::Owner { what, name, to: self.role_spec(false)? }
                } else if self.kw("rename") {
                    self.expect_kw("to")?;
                    ForeignStmt::Rename { what, name, to: self.name()? }
                } else if what == What::Fdw {
                    let (funcs, conflict) = self.fdw_funcs()?;
                    let options = if self.kw("options") { self.generic_options(true)? } else { vec![] };
                    if funcs.0.is_none() && funcs.1.is_none() && options.is_empty() {
                        return self.fail("");
                    }
                    ForeignStmt::AlterFdw { name, funcs, conflict, options }
                } else {
                    let version = match self.kw("version") {
                        true if self.kw("null") => Some(None),
                        true => Some(Some(self.string()?)),
                        false => None,
                    };
                    let options = if self.kw("options") { self.generic_options(true)? } else { vec![] };
                    if version.is_none() && options.is_empty() {
                        return self.fail("");
                    }
                    ForeignStmt::AlterServer { name, version, options }
                }
            }
        };
        Ok(Stmt::Foreign(Box::new(stmt)))
    }

    /// A string constant (TYPE 'oracle', VERSION '1.0').
    fn string(&mut self) -> Result<String> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(s),
            t => {
                self.put_back(t);
                self.fail("expected a string")
            }
        }
    }

    /// `HANDLER f | NO HANDLER` and `VALIDATOR f | NO VALIDATOR`, each at most once (a second one is kept as the
    /// error it is, at its place).
    fn fdw_funcs(&mut self) -> Result<(Funcs, Option<SqlError>)> {
        let (mut funcs, mut conflict): (Funcs, Option<SqlError>) = ((None, None), None);
        loop {
            let at = self.i;
            let next_is = |p: &Self, w: &str| matches!(p.t.get(p.i + 1), Some(Tok::Word(x)) if x == w);
            let (handler, f) = if self.is_kw("handler") || self.is_kw("validator") {
                let handler = self.is_kw("handler");
                self.i += 1;
                (handler, Some(self.qualified(false, Kind::Func)?))
            } else if self.is_kw("no") && (next_is(self, "handler") || next_is(self, "validator")) {
                let handler = next_is(self, "handler");
                self.i += 2;
                (handler, None)
            } else {
                return Ok((funcs, conflict));
            };
            let slot = if handler { &mut funcs.0 } else { &mut funcs.1 };
            match slot {
                Some(_) => {
                    if conflict.is_none() {
                        conflict = Some(conflicting(self, at));
                    }
                }
                None => *slot = Some(f),
            }
        }
    }

    /// After OPTIONS: `( [ADD | SET | DROP] name ['value'], ... )`; with `alter`, the actions may be given.
    pub(super) fn generic_options(&mut self, alter: bool) -> Result<OptChanges> {
        self.expect_sym("(")?;
        let mut out = vec![];
        loop {
            let mut op = OptOp::Add;
            // (a word before a name is an action; `add 'x'` is an option named add)
            if alter && matches!(self.t.get(self.i + 1), Some(Tok::Word(_) | Tok::Ident(_))) {
                if self.kw("add") {
                    op = OptOp::Add;
                } else if self.kw("set") {
                    op = OptOp::Set;
                } else if self.kw("drop") {
                    op = OptOp::Drop;
                }
            }
            let name = match self.next() {
                Some(Tok::Word(w) | Tok::Ident(w)) => w,
                t => {
                    self.put_back(t);
                    return self.fail("expected an option's name");
                }
            };
            let value = if op == OptOp::Drop { String::new() } else { self.string()? };
            out.push((op, name, value));
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(out)
    }

    /// After CREATE, ALTER or DROP USER MAPPING: `[IF [NOT] EXISTS] FOR role SERVER name [OPTIONS (...)]`.
    fn user_mapping_stmt(&mut self, verb: &str) -> Result<Stmt> {
        let guard = match verb {
            "create" => self.if_not_exists()?,
            "drop" => self.if_exists()?,
            _ => false,
        };
        self.expect_kw("for")?;
        let user = if self.kw("user") { Spec::CurrentUser } else { self.role_spec(true)? };
        self.expect_kw("server")?;
        let server = self.name()?;
        let options = match verb {
            "drop" => vec![],
            "alter" => {
                self.expect_kw("options")?;
                self.generic_options(true)?
            }
            _ if self.kw("options") => self.generic_options(false)?,
            _ => vec![],
        };
        let verb = match verb {
            "create" => "create",
            "alter" => "alter",
            _ => "drop",
        };
        Ok(Stmt::Foreign(Box::new(ForeignStmt::Mapping { verb, user, server, guard, options })))
    }

    /// After IMPORT: `FOREIGN SCHEMA remote [LIMIT TO | EXCEPT (tables)] FROM SERVER s INTO schema [OPTIONS (...)]`.
    pub(super) fn import_foreign_schema(&mut self) -> Result<Stmt> {
        self.expect_kw("foreign")?;
        self.expect_kw("schema")?;
        self.name()?;
        if self.kw("limit") {
            self.expect_kw("to")?;
            self.expect_sym("(")?;
            self.names()?;
        } else if self.kw("except") {
            self.expect_sym("(")?;
            self.names()?;
        }
        self.expect_kw("from")?;
        self.expect_kw("server")?;
        let server = self.name()?;
        self.expect_kw("into")?;
        let into = self.name()?;
        if self.kw("options") {
            self.generic_options(false)?;
        }
        Ok(Stmt::Foreign(Box::new(ForeignStmt::Import { server, into })))
    }

    /// After CREATE, ALTER or DROP, at FOREIGN TABLE: a table whose server and options are kept with it (see
    /// `Hier::foreign`); a file_fdw table (one naming a file) is a view over the file's rows.
    fn foreign_table(&mut self, verb: &str) -> Result<Stmt> {
        let start = self.i;
        let end = (start..self.t.len()).find(|k| self.t[*k] == Tok::Sym(";")).unwrap_or(self.t.len());
        // the constraints a foreign table can't have, where they are written (in the column list, or what ALTER adds)
        if verb != "drop" {
            // (CREATE's are in its column list; ALTER's are its actions)
            let level = if verb == "create" { 1 } else { 0 };
            let (mut depth, mut named) = (0i32, None::<usize>);
            for j in start..end {
                let word = |k: usize| match self.t.get(k) {
                    Some(Tok::Word(w)) => w.as_str(),
                    _ => "",
                };
                match &self.t[j] {
                    Tok::Sym("(") => depth += 1,
                    Tok::Sym(")") => depth -= 1,
                    Tok::Sym(",") => named = None,
                    Tok::Word(w) if depth == level => {
                        let at = named.unwrap_or(j);
                        match w.as_str() {
                            "constraint" => named = Some(j),
                            "primary" if word(j + 1) == "key" => return Err(not_on_foreign(self, at, "primary key")),
                            "unique" => return Err(not_on_foreign(self, at, "unique")),
                            "references" => return Err(not_on_foreign(self, at, "foreign key")),
                            "foreign" if word(j + 1) == "key" => return Err(not_on_foreign(self, at, "foreign key")),
                            "exclude" => return Err(not_on_foreign(self, at, "exclusion")),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        self.i += 1; // (FOREIGN: TABLE is next)
        match verb {
            "drop" => {
                self.expect_kw("table")?;
                let if_exists = self.if_exists()?;
                let names = self.rel_names()?;
                let cascade = self.cascade();
                Ok(Stmt::DropObjects { kind: "foreign table", names, if_exists, cascade })
            }
            "alter" => {
                self.col_options.clear();
                let mut st = self.alter()?;
                let added = std::mem::take(&mut self.col_options);
                if let Stmt::Alter { changes, .. } = &mut st {
                    for (col, opts) in added {
                        changes.push(Alter::FdwOptions(Some(col), opts));
                    }
                }
                Ok(st)
            }
            _ => {
                // SERVER name [OPTIONS (...)], after the columns and INHERITS or the partition bound
                let (mut depth, mut cut) = (0i32, None);
                for j in self.i..end {
                    match &self.t[j] {
                        Tok::Sym("(") => depth += 1,
                        Tok::Sym(")") => depth -= 1,
                        Tok::Word(w) if w == "server" && depth == 0 => {
                            cut = Some(j);
                            break;
                        }
                        _ => {}
                    }
                }
                let Some(cut) = cut else {
                    // (where SERVER should have been: the statement's end)
                    let near = match self.t.get(end) {
                        Some(Tok::Sym(s)) => format!("\"{s}\""),
                        _ => "end of input".to_string(),
                    };
                    return Err(self.syntax_at(end, near, ""));
                };
                let tail: Vec<Tok> = self.t[cut..end].to_vec();
                // a file_fdw table (it names a file): a view over the file's rows
                if let Some(view) = super::userdefs::file_table_view(&self.t[self.i..cut], &tail)
                    && let Ok(toks) = lex(&view)
                {
                    self.t.drain(cut..end);
                    self.t.splice(start - 1..cut, toks);
                    self.i = start - 1;
                    return self.stmt_in();
                }
                let head = self.i;
                self.i = cut + 1;
                let server = self.name()?;
                let options = if self.kw("options") { self.generic_options(false)? } else { vec![] };
                if self.i < end {
                    return self.fail("");
                }
                self.t.drain(cut..end);
                self.i = head;
                self.col_options.clear();
                let mut st = self.create()?;
                let cols = std::mem::take(&mut self.col_options);
                let Stmt::Create { hier, .. } = &mut st else {
                    return err(UNSUPPORTED, "CREATE FOREIGN TABLE makes a table with columns, not one AS a query");
                };
                let pairs = |o: &OptChanges| Json::Array(o.iter().map(|(_, k, v)| serde_json::json!([k, v])).collect());
                let cols: Vec<Json> = cols.iter().map(|(c, o)| serde_json::json!([c, pairs(o)])).collect();
                hier.foreign = Some(serde_json::json!({ "server": server, "options": pairs(&options), "cols": cols }));
                Ok(st)
            }
        }
    }
}

// ---------------------------------------------------------------- running them

/// The role the statement runs as.
fn who(db: &Db) -> Result<Who> {
    let w = roles::current().map_or_else(|| Who::boot("chronos"), |w| (*w).clone());
    Ok(Who { reg: roles::registry(&db.core)?, ..w })
}

/// 42501 for a role that isn't a superuser, as CREATE (`verb` create) and ALTER FOREIGN DATA WRAPPER need one.
fn superuser_only(who: &Who, verb: &str, name: &str) -> Result<()> {
    if who.superuser {
        return Ok(());
    }
    let doing = if verb == "create" { "create" } else { "alter" };
    Err(denied_hint(
        format!("permission denied to {doing} foreign-data wrapper \"{name}\""),
        &format!("Must be superuser to {doing} a foreign-data wrapper."),
    ))
}

/// 42501 with Postgres's HINT.
fn denied_hint(message: String, hint: &str) -> SqlError {
    SqlError {
        code: "42501",
        message,
        info: Some(Box::new(Violation { hint: Some(hint.to_string()), ..Violation::default() })),
    }
}

/// The handler `name` (no arguments): 42883 if there is none, 42809 if it doesn't return fdw_handler (none made
/// here can: Postgres's handlers are C functions).
fn handler(v: &View, name: &str) -> Result<String> {
    match functions::find_routine(v, name, Some(&[][..]))? {
        None => err(NO_FUNCTION, format!("function {}() does not exist", written(name))),
        Some(_) => err("42809", format!("function {} must return type fdw_handler", written(name))),
    }
}

/// The validator `name` (text[], oid): postgresql_fdw_validator, the built-in one.
fn validator(v: &View, name: &str) -> Result<String> {
    let plain = rel_part(name);
    if plain == "postgresql_fdw_validator" && (!name.contains('.') || schema_part(name) == "pg_catalog") {
        return Ok(plain.to_string());
    }
    match functions::find_routine(v, name, Some(&[Type::Array(25), Type::Oid][..]))? {
        None => err(NO_FUNCTION, format!("function {}(text[], oid) does not exist", written(name))),
        Some(_) => err(
            UNSUPPORTED,
            format!(
                "validator {} isn't supported: a wrapper's options are checked by postgresql_fdw_validator or not at all",
                written(name)
            ),
        ),
    }
}

/// The owner and privileges of wrapper or server `name`.
fn obj(v: &View, kind: AclKind, name: &str) -> Result<acl::Obj> {
    acl::foreign_obj(v, kind, name)
}

/// A mapping's role as messages name it.
fn user_name(reg: &roles::Registry, user: i64) -> String {
    if user == PUBLIC { "public".into() } else { reg.name_of(user) }
}

/// Runs a foreign-data wrapper, server or user mapping statement.
/// Whether `s` may drop foreign tables after what it drops (DROP SERVER or FOREIGN DATA WRAPPER ...
/// CASCADE), by a statement of their own: the session runs it in a transaction, so that it is all
/// or nothing.
pub(super) fn drops_tables(s: &ForeignStmt) -> bool {
    matches!(s, ForeignStmt::Drop { cascade: true, .. })
}

/// CREATE or DROP TABLESPACE (its tag), which can't run inside a transaction block (see `tablespace`).
pub(super) fn tablespace_alone(s: &ForeignStmt) -> Option<&'static str> {
    match s {
        ForeignStmt::Tablespace(t) => t.alone(),
        _ => None,
    }
}

pub(super) fn run(db: &Db, branch: &str, s: &ForeignStmt) -> Result<Output> {
    match s {
        ForeignStmt::Publication(p) => return super::publication::run(db, branch, p),
        ForeignStmt::Statistics(x) => return super::statistics::run(db, branch, x),
        ForeignStmt::Tablespace(t) => return super::tablespace::run(db, branch, t),
        ForeignStmt::AccessMethod(a) => return super::accessmethod::run(db, branch, a),
        _ => {}
    }
    let who = who(db)?;
    match s {
        ForeignStmt::CreateFdw { name, funcs, conflict, options } => {
            superuser_only(&who, "create", name)?;
            db.batch_with::<(), SqlError>(branch, |v| {
                let o = load(v)?;
                if o.fdw(name).is_some() {
                    return err("42710", format!("foreign-data wrapper \"{name}\" already exists"));
                }
                if let Some(e) = conflict {
                    return Err(e.clone());
                }
                let handler = match &funcs.0 {
                    Some(Some(h)) => Some(handler(v, h)?),
                    _ => None,
                };
                let validator = match &funcs.1 {
                    Some(Some(x)) => Some(validator(v, x)?),
                    _ => None,
                };
                let options = transform(&Opts::new(), options)?;
                validate(validator.as_deref(), OptsOf::Wrapper, &options)?;
                let f = Fdw { name: name.clone(), handler, validator, options, at: o.next_at() };
                let mut writes = vec![fdw_row(&f)];
                writes.extend(owned_by(AclKind::Fdw, name, who.oid));
                Ok((writes, ()))
            })?;
            Ok(Output::tag("CREATE FOREIGN DATA WRAPPER"))
        }
        ForeignStmt::AlterFdw { name, funcs, conflict, options } => {
            superuser_only(&who, "alter", name)?;
            let mut warnings = vec![];
            db.batch_with::<(), SqlError>(branch, |v| {
                warnings.clear();
                let o = load(v)?;
                let Some(mut f) = o.fdw(name).cloned() else { return Err(missing(AclKind::Fdw, name)) };
                if let Some(e) = conflict {
                    return Err(e.clone());
                }
                if let Some(h) = &funcs.0 {
                    f.handler = match h {
                        Some(h) => Some(handler(v, h)?),
                        None => None,
                    };
                    warnings.push("changing the foreign-data wrapper handler can change behavior of existing foreign tables");
                }
                if let Some(x) = &funcs.1 {
                    f.validator = match x {
                        Some(x) => Some(validator(v, x)?),
                        None => None,
                    };
                    if f.validator.is_some() {
                        warnings.push(
                            "changing the foreign-data wrapper validator can cause the options for dependent objects to become invalid",
                        );
                    }
                }
                if !options.is_empty() {
                    f.options = transform(&f.options, options)?;
                    validate(f.validator.as_deref(), OptsOf::Wrapper, &f.options)?;
                }
                Ok((vec![fdw_row(&f)], ()))
            })?;
            for w in warnings {
                notice("WARNING", w.into());
            }
            Ok(Output::tag("ALTER FOREIGN DATA WRAPPER"))
        }
        ForeignStmt::CreateServer { name, if_not_exists, kind, version, fdw, options } => {
            let mut skipped = false;
            db.batch_with::<(), SqlError>(branch, |v| {
                skipped = false;
                let o = load(v)?;
                if o.server(name).is_some() {
                    if *if_not_exists {
                        skipped = true;
                        return Ok((vec![], ()));
                    }
                    return err("42710", format!("server \"{name}\" already exists"));
                }
                let Some(w) = o.fdw(fdw) else { return Err(missing(AclKind::Fdw, fdw)) };
                let wo = obj(v, AclKind::Fdw, fdw)?;
                if !wo.has(&who, acl::USAGE) {
                    return Err(wo.denied());
                }
                let options = transform(&Opts::new(), options)?;
                validate(w.validator.as_deref(), OptsOf::Server, &options)?;
                let s = Server {
                    name: name.clone(),
                    fdw: fdw.clone(),
                    kind: kind.clone(),
                    version: version.clone(),
                    options,
                    at: o.next_at(),
                };
                let mut writes = vec![server_row(&s), depended_on(v, &fdw_key(fdw))?];
                writes.extend(owned_by(AclKind::Server, name, who.oid));
                Ok((writes, ()))
            })?;
            if skipped {
                notice("NOTICE", format!("server \"{name}\" already exists, skipping"));
            }
            Ok(Output::tag("CREATE SERVER"))
        }
        ForeignStmt::AlterServer { name, version, options } => {
            db.batch_with::<(), SqlError>(branch, |v| {
                let o = load(v)?;
                let Some(mut s) = o.server(name).cloned() else { return Err(missing(AclKind::Server, name)) };
                let so = obj(v, AclKind::Server, name)?;
                if !who.has_privs_of(so.owner) {
                    return Err(so.not_owner());
                }
                if let Some(x) = version {
                    s.version = x.clone();
                }
                if !options.is_empty() {
                    s.options = transform(&s.options, options)?;
                    validate(o.validator_of(name).as_deref(), OptsOf::Server, &s.options)?;
                }
                Ok((vec![server_row(&s)], ()))
            })?;
            Ok(Output::tag("ALTER SERVER"))
        }
        ForeignStmt::Mapping { verb, user, server, guard, options } => {
            mapping(db, branch, &who, verb, user, server, *guard, options)
        }
        ForeignStmt::Drop { what, names, if_exists, cascade } => {
            drop_objects(db, branch, &who, *what, names, *if_exists, *cascade)
        }
        ForeignStmt::Rename { what, name, to } => rename(db, branch, &who, *what, name, to),
        ForeignStmt::Owner { what, name, to } => owner(db, branch, &who, *what, name, to),
        ForeignStmt::Publication(p) => super::publication::run(db, branch, p),
        ForeignStmt::Statistics(x) => super::statistics::run(db, branch, x),
        ForeignStmt::Tablespace(t) => super::tablespace::run(db, branch, t),
        ForeignStmt::AccessMethod(a) => super::accessmethod::run(db, branch, a),
        ForeignStmt::Import { server, into } => {
            let v = db.view_tx(branch)?;
            let o = load(&v)?;
            let Some(s) = o.server(server) else { return Err(missing(AclKind::Server, server)) };
            let so = obj(&v, AclKind::Server, server)?;
            if !so.has(&who, acl::USAGE) {
                return Err(so.denied());
            }
            if into != "public" && !namespaces(&v)?.contains(into) {
                return err("3F000", format!("schema \"{into}\" does not exist"));
            }
            match o.fdw(&s.fdw).and_then(|f| f.handler.as_ref()) {
                None => err("55000", format!("foreign-data wrapper \"{}\" has no handler", s.fdw)),
                Some(_) => err(
                    UNSUPPORTED,
                    "IMPORT FOREIGN SCHEMA isn't supported: Chronos runs no foreign-data wrapper's code",
                ),
            }
        }
    }
}

/// The write that makes role `oid` the owner of a wrapper or server just made (none for the bootstrap
/// superuser, who owns whatever has no row).
fn owned_by(kind: AclKind, name: &str, oid: i64) -> Vec<(String, Option<String>)> {
    match oid {
        BOOTSTRAP => vec![],
        o => vec![acl::write_entry(&acl::key_of(kind, name), &acl::Entry { owner: Some(o), acl: None })],
    }
}

/// Postgres's user_mapping_ddl_aclcheck: a server's owner may change any of its mappings, a role its own if it
/// may use the server.
fn mapping_allowed(v: &View, who: &Who, user: i64, server: &str) -> Result<()> {
    let so = obj(v, AclKind::Server, server)?;
    if who.has_privs_of(so.owner) {
        return Ok(());
    }
    if user == who.oid {
        if !so.has(who, acl::USAGE) {
            return Err(so.denied());
        }
        return Ok(());
    }
    Err(so.not_owner())
}

#[allow(clippy::too_many_arguments)]
fn mapping(
    db: &Db,
    branch: &str,
    who: &Who,
    verb: &str,
    user: &Spec,
    server: &str,
    guard: bool,
    options: &OptChanges,
) -> Result<Output> {
    let tag = format!("{} USER MAPPING", verb.to_uppercase());
    // the role: PUBLIC is 0; one that isn't there is 42704 (with DROP ... IF EXISTS, a notice)
    let uid = match user.oid(&who.reg) {
        Ok(o) => o,
        Err(e) if verb == "drop" && guard => {
            notice("NOTICE", format!("{}, skipping", e.message.trim_end_matches(", skipping")));
            return Ok(Output::tag(tag));
        }
        Err(e) => return Err(e),
    };
    let shown = user_name(&who.reg, uid);
    let mut said: Option<String> = None;
    db.batch_with::<(), SqlError>(branch, |v| {
        said = None;
        let o = load(v)?;
        let Some(s) = o.server(server).cloned() else {
            if verb == "drop" && guard {
                said = Some(format!("server \"{server}\" does not exist, skipping"));
                return Ok((vec![], ()));
            }
            return Err(missing(AclKind::Server, server));
        };
        let had = o.mappings.iter().find(|m| m.user == uid && m.server == server).cloned();
        let validator = o.validator_of(server);
        match verb {
            "create" => {
                mapping_allowed(v, who, uid, server)?;
                if had.is_some() {
                    let m = format!("user mapping for \"{shown}\" already exists for server \"{server}\"");
                    if guard {
                        said = Some(format!("{m}, skipping"));
                        return Ok((vec![], ()));
                    }
                    return err("42710", m);
                }
                let options = transform(&Opts::new(), options)?;
                validate(validator.as_deref(), OptsOf::Mapping, &options)?;
                let m = Mapping { user: uid, server: s.name.clone(), options, at: o.next_at() };
                Ok((vec![mapping_row(&m), depended_on(v, &server_key(&s.name))?], ()))
            }
            _ => {
                let Some(mut m) = had else {
                    let gone = format!("user mapping for \"{shown}\" does not exist for server \"{server}\"");
                    if verb == "drop" && guard {
                        said = Some(format!("{gone}, skipping"));
                        return Ok((vec![], ()));
                    }
                    return err("42704", gone);
                };
                mapping_allowed(v, who, uid, server)?;
                if verb == "drop" {
                    return Ok((vec![(mapping_key(m.user, &m.server), None)], ()));
                }
                m.options = transform(&m.options, options)?;
                validate(validator.as_deref(), OptsOf::Mapping, &m.options)?;
                Ok((vec![mapping_row(&m)], ()))
            }
        }
    })?;
    if let Some(m) = said {
        notice("NOTICE", m);
    }
    Ok(Output::tag(tag))
}

/// What depends on the wrappers and servers being dropped, as Postgres lists it: each server of a wrapper, then
/// that server's user mappings and foreign tables, in the order they were made; as (description, what it depends
/// on, the rows that go with it).
type Dependent = (String, String, Vec<(String, Option<String>)>);

/// The foreign tables of server `server`, by stored name.
fn tables_of(v: &View, server: &str) -> Result<Vec<String>> {
    Ok(schemas(v)?
        .into_iter()
        .filter(|t| t.hier.foreign.as_ref().is_some_and(|f| f["server"].as_str() == Some(server)))
        .map(|t| t.name)
        .collect())
}

/// The rows that go with server `name` (its mappings, its owner and grants, its comment), and what depends on it.
fn server_gone(
    v: &View,
    o: &Objects,
    reg: &roles::Registry,
    name: &str,
    deps: &mut Vec<Dependent>,
    tables: &mut Vec<String>,
) -> Result<Vec<(String, Option<String>)>> {
    let mut mine: Vec<&Mapping> = o.mappings.iter().filter(|m| m.server == name).collect();
    mine.sort_by_key(|m| m.at);
    for m in mine {
        deps.push((
            format!("user mapping for {} on server {name}", user_name(reg, m.user)),
            format!("server {name}"),
            vec![(mapping_key(m.user, &m.server), None)],
        ));
    }
    for t in tables_of(v, name)? {
        deps.push((format!("foreign table {}", rel_part(&t)), format!("server {name}"), vec![]));
        tables.push(t);
    }
    Ok(vec![
        (server_key(name), None),
        (acl::row_key(&acl::key_of(AclKind::Server, name)), None),
        (comments::row_key(&format!("server/{name}")), None),
    ])
}

#[allow(clippy::too_many_arguments)]
fn drop_objects(
    db: &Db,
    branch: &str,
    who: &Who,
    what: What,
    names: &[String],
    if_exists: bool,
    cascade: bool,
) -> Result<Output> {
    let kind = what.kind();
    let mut skipped: Vec<String> = vec![];
    let mut tables: Vec<String> = vec![];
    let mut cascaded: Vec<String> = vec![];
    // the objects and the foreign tables on them go together or not at all: in a transaction of their own
    let tmp = format!("{TX_PREFIX}{}", new_id());
    db.fork_info(branch, &tmp, crate::Info::new())?;
    let done = db.batch_with::<(), SqlError>(&tmp, |v| {
        (skipped, tables, cascaded) = (vec![], vec![], vec![]);
        let o = load(v)?;
        let mut gone: Vec<&str> = vec![];
        for n in names {
            let there = match what {
                What::Fdw => o.fdw(n).is_some(),
                What::Server => o.server(n).is_some(),
            };
            if !there {
                if if_exists {
                    skipped.push(n.clone());
                    continue;
                }
                return Err(missing(kind, n));
            }
            let x = obj(v, kind, n)?;
            if !who.has_privs_of(x.owner) {
                return Err(x.not_owner());
            }
            if !gone.contains(&n.as_str()) {
                gone.push(n);
            }
        }
        let mut writes = vec![];
        let mut deps: Vec<Dependent> = vec![];
        for n in &gone {
            match what {
                What::Fdw => {
                    writes.push((fdw_key(n), None));
                    writes.push((acl::row_key(&acl::key_of(AclKind::Fdw, n)), None));
                    writes.push((comments::row_key(&format!("fdw/{n}")), None));
                    let mut servers: Vec<&Server> = o.servers.iter().filter(|s| s.fdw == *n).collect();
                    servers.sort_by_key(|s| s.at);
                    for s in servers {
                        let mut sd = vec![];
                        let rows = server_gone(v, &o, &who.reg, &s.name, &mut sd, &mut tables)?;
                        deps.push((format!("server {}", s.name), format!("foreign-data wrapper {n}"), rows));
                        deps.extend(sd);
                    }
                }
                What::Server => {
                    let mut sd = vec![];
                    writes.extend(server_gone(v, &o, &who.reg, n, &mut sd, &mut tables)?);
                    deps.extend(sd);
                }
            }
        }
        if !deps.is_empty() && !cascade {
            let first = gone.first().copied().unwrap_or_default();
            let noun = if what == What::Fdw { "foreign-data wrapper" } else { "server" };
            let detail: Vec<String> = deps.iter().map(|(d, on, _)| format!("{d} depends on {on}")).collect();
            return Err(SqlError {
                code: "2BP01",
                message: format!("cannot drop {noun} {first} because other objects depend on it"),
                info: Some(Box::new(Violation {
                    detail: Some(detail.join("\n")),
                    hint: Some("Use DROP ... CASCADE to drop the dependent objects too.".into()),
                    ..Violation::default()
                })),
            });
        }
        for (d, _, rows) in deps {
            cascaded.push(d);
            writes.extend(rows);
        }
        Ok((writes, ()))
    });
    if let Err(e) = done {
        let _ = db.discard(&tmp);
        return Err(e);
    }
    // the foreign tables on the servers dropped, as DROP FOREIGN TABLE drops them: whoever owns them (a dependent
    // object goes with what it depends on, as Postgres drops it, without its owner's say; the rest of the statement,
    // its event triggers among it, runs as the caller)
    let mark = NOTICES.with_borrow(Vec::len);
    let done = (|| -> Result<()> {
        if !tables.is_empty() {
            let list: Vec<String> = tables.iter().map(|t| ident_rel(t)).collect();
            for st in parse(&format!("drop foreign table if exists {} cascade", list.join(", ")))? {
                acl::dropping_dependents(&tables, || st.execute(db, &tmp, &[], &[]))?;
            }
        }
        db.merge(&tmp, crate::Resolve::Fail)?;
        Ok(())
    })();
    // what the drops said: told only once all of it has taken effect, after this statement's own notices
    let theirs: Vec<Notice> = NOTICES.with_borrow_mut(|n| n.split_off(mark.min(n.len())));
    if let Err(e) = done {
        let _ = db.discard(&tmp);
        return Err(e);
    }
    for n in &skipped {
        let noun = if what == What::Fdw { "foreign-data wrapper" } else { "server" };
        notice("NOTICE", format!("{noun} \"{n}\" does not exist, skipping"));
    }
    match &cascaded[..] {
        [] => {}
        [one] => notice("NOTICE", format!("drop cascades to {one}")),
        many => notice_detail(
            "NOTICE",
            format!("drop cascades to {} other objects", many.len()),
            many.iter().map(|d| format!("drop cascades to {d}")).collect::<Vec<_>>().join("\n"),
        ),
    }
    NOTICES.with_borrow_mut(|n| n.extend(theirs));
    Ok(Output::tag(what.tag("DROP")))
}

fn rename(db: &Db, branch: &str, who: &Who, what: What, name: &str, to: &str) -> Result<Output> {
    let kind = what.kind();
    db.batch_with::<(), SqlError>(branch, |v| {
        let o = load(v)?;
        let there = match what {
            What::Fdw => o.fdw(name).is_some(),
            What::Server => o.server(name).is_some(),
        };
        if !there {
            return Err(missing(kind, name));
        }
        let x = obj(v, kind, name)?;
        if !who.has_privs_of(x.owner) {
            return Err(x.not_owner());
        }
        let taken = match what {
            What::Fdw => o.fdw(to).is_some(),
            What::Server => o.server(to).is_some(),
        };
        if taken {
            let noun = if what == What::Fdw { "foreign-data wrapper" } else { "server" };
            return err("42710", format!("{noun} \"{to}\" already exists"));
        }
        let mut writes = vec![];
        // its owner and grants, and its comment, under the new name
        let (ak, bk) = (acl::key_of(kind, name), acl::key_of(kind, to));
        if let Some(raw) = v.get(&acl::row_key(&ak))? {
            writes.push((acl::row_key(&ak), None));
            writes.push((acl::row_key(&bk), Some(raw.to_string())));
        }
        let word = if what == What::Fdw { "fdw" } else { "server" };
        let (ck, cn) = (comments::row_key(&format!("{word}/{name}")), comments::row_key(&format!("{word}/{to}")));
        if let Some(raw) = v.get(&ck)? {
            writes.push((ck, None));
            writes.push((cn, Some(raw.to_string())));
        }
        match what {
            What::Fdw => {
                let mut f = o.fdw(name).cloned().unwrap_or_default();
                writes.push((fdw_key(name), None));
                f.name = to.to_string();
                writes.push(fdw_row(&f));
                // its servers name it
                for s in o.servers.iter().filter(|s| s.fdw == name) {
                    let mut s = s.clone();
                    s.fdw = to.to_string();
                    writes.push(server_row(&s));
                }
            }
            What::Server => {
                let mut s = o.server(name).cloned().unwrap_or_default();
                writes.push((server_key(name), None));
                s.name = to.to_string();
                writes.push(server_row(&s));
                for m in o.mappings.iter().filter(|m| m.server == name) {
                    writes.push((mapping_key(m.user, &m.server), None));
                    let mut m = m.clone();
                    m.server = to.to_string();
                    writes.push(mapping_row(&m));
                }
                // and its foreign tables
                for mut t in schemas(v)? {
                    if let Some(f) = t.hier.foreign.as_mut()
                        && f["server"].as_str() == Some(name)
                    {
                        f["server"] = Json::from(to);
                        writes.push((schema_key(&t.name), Some(schema_json(&t))));
                    }
                }
            }
        }
        Ok((writes, ()))
    })?;
    Ok(Output::tag(what.tag("ALTER")))
}

fn owner(db: &Db, branch: &str, who: &Who, what: What, name: &str, to: &Spec) -> Result<Output> {
    let kind = what.kind();
    let new = to.resolve(&who.reg)?;
    db.batch_with::<(), SqlError>(branch, |v| {
        let o = load(v)?;
        let x = obj(v, kind, name)?;
        match what {
            What::Fdw => {
                if o.fdw(name).is_none() {
                    return Err(missing(kind, name));
                }
                if !who.superuser {
                    return Err(denied_hint(
                        format!("permission denied to change owner of foreign-data wrapper \"{name}\""),
                        "Must be superuser to change owner of a foreign-data wrapper.",
                    ));
                }
                if !new.superuser {
                    return Err(denied_hint(
                        format!("permission denied to change owner of foreign-data wrapper \"{name}\""),
                        "The owner of a foreign-data wrapper must be a superuser.",
                    ));
                }
            }
            What::Server => {
                let Some(s) = o.server(name) else { return Err(missing(kind, name)) };
                if x.owner == new.oid {
                    return Ok((vec![], ()));
                }
                if !who.superuser {
                    if !who.has_privs_of(x.owner) {
                        return Err(x.not_owner());
                    }
                    if !who.reg.can_set(who.oid, new.oid) {
                        return err("42501", format!("must be able to SET ROLE \"{}\"", new.name));
                    }
                    let w = obj(v, AclKind::Fdw, &s.fdw)?;
                    if !w.has(&who.as_role(new.oid), acl::USAGE) {
                        return Err(w.denied());
                    }
                }
            }
        }
        if x.owner == new.oid {
            return Ok((vec![], ()));
        }
        let key = acl::key_of(kind, name);
        let e = acl::entry(v, &key)?;
        let acl = e.acl.map(|a| acl::new_owner_acl(a, x.owner, new.oid));
        Ok((vec![acl::write_entry(&key, &acl::Entry { owner: (new.oid != BOOTSTRAP).then_some(new.oid), acl })], ()))
    })?;
    Ok(Output::tag(what.tag("ALTER")))
}

// ---------------------------------------------------------------- foreign tables

/// The column options CREATE FOREIGN TABLE gave, by column name, from `hier.foreign` (see `Parser::foreign_table`).
fn named_cols(f: &Json) -> Vec<(String, Opts)> {
    f["cols"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| Some((c.get(0)?.as_str()?.to_string(), opts_of(c.get(1)?))))
        .collect()
}

/// CREATE FOREIGN TABLE: table `t` (made, linked to its parents) gets its server and options. The server has to
/// be there and the role able to use it; a partition's parent can't have unique indexes, and its other indexes
/// aren't the foreign table's. What it gives is the write of the server's row that says a table was made on it
/// (see `depended_on`).
pub(super) fn made(v: &View, t: &mut Table, f: &Json) -> Result<(String, Option<String>)> {
    let server = f["server"].as_str().unwrap_or_default().to_string();
    if let Some((parent, _)) = &t.hier.of
        && let Some(p) = load_table(v, parent)?
        && (p.has_key() || !p.uniques.is_empty())
    {
        return Err(SqlError {
            code: "42809",
            message: format!("cannot create foreign partition of partitioned table \"{}\"", rel_part(parent)),
            info: Some(Box::new(Violation {
                detail: Some(format!("Table \"{}\" contains indexes that are unique.", rel_part(parent))),
                ..Violation::default()
            })),
        });
    }
    let o = load(v)?;
    if o.server(&server).is_none() {
        return Err(missing(AclKind::Server, &server));
    }
    if let Some(who) = roles::checked() {
        let so = obj(v, AclKind::Server, &server)?;
        if !so.has(&who, acl::USAGE) {
            return Err(so.denied());
        }
    }
    let validator = o.validator_of(&server);
    let changes = |opts: Opts| -> OptChanges { opts.into_iter().map(|(k, x)| (OptOp::Add, k, x)).collect() };
    let options = transform(&Opts::new(), &changes(opts_of(&f["options"])))?;
    validate(validator.as_deref(), OptsOf::Table, &options)?;
    let mut cols = vec![];
    for (name, opts) in named_cols(f) {
        let opts = transform(&Opts::new(), &changes(opts))?;
        validate(validator.as_deref(), OptsOf::Column, &opts)?;
        if let Some(c) = t.cols.iter().find(|c| c.name == name)
            && !opts.is_empty()
        {
            cols.push(serde_json::json!([c.id, opts_json(&opts)]));
        }
    }
    // (a partition's copies of its parent's indexes: a foreign table has none)
    let dropped: Vec<String> = t.indexes.iter().map(|x| x.name.clone()).collect();
    t.indexes.clear();
    t.hier.inherited.retain(|n| !dropped.contains(n));
    t.hier.foreign = Some(serde_json::json!({ "server": server, "options": opts_json(&options), "cols": cols }));
    depended_on(v, &server_key(&server))
}

fn load_table(v: &View, name: &str) -> Result<Option<Table>> {
    super::load(v, name)
}

/// ALTER FOREIGN TABLE ... OPTIONS (...) (`col` None) and ALTER COLUMN c OPTIONS (...): table `t`'s options or
/// its column's, changed and checked by its wrapper's validator.
pub(super) fn alter_options(v: &View, t: &mut Table, col: Option<&str>, changes: &OptChanges) -> Result<()> {
    let name = rel_part(&t.name).to_string();
    let Some(mut f) = t.hier.foreign.clone() else {
        return err("42809", format!("\"{name}\" is not a foreign table"));
    };
    let server = f["server"].as_str().unwrap_or_default().to_string();
    let validator = load(v)?.validator_of(&server);
    match col {
        None => {
            let opts = transform(&opts_of(&f["options"]), changes)?;
            validate(validator.as_deref(), OptsOf::Table, &opts)?;
            f["options"] = opts_json(&opts);
        }
        Some(c) => {
            let Some(id) = t.cols.iter().find(|x| x.name == c).map(|x| x.id) else {
                if SYSTEM_NAMES.contains(&c) {
                    return err(UNSUPPORTED, format!("cannot alter system column \"{c}\""));
                }
                return err(NO_COLUMN, format!("column \"{c}\" of relation \"{name}\" does not exist"));
            };
            let mut cols: Vec<Json> = f["cols"].as_array().cloned().unwrap_or_default();
            let at = cols.iter().position(|x| x.get(0).and_then(|i| i.as_u64()) == Some(u64::from(id)));
            let old = at.map(|i| opts_of(&cols[i][1])).unwrap_or_default();
            let opts = transform(&old, changes)?;
            validate(validator.as_deref(), OptsOf::Column, &opts)?;
            if let Some(i) = at {
                cols.remove(i);
            }
            if !opts.is_empty() {
                cols.push(serde_json::json!([id, opts_json(&opts)]));
            }
            f["cols"] = Json::Array(cols);
        }
    }
    t.hier.foreign = Some(f);
    Ok(())
}

/// The system columns' names.
const SYSTEM_NAMES: [&str; 6] = ["tableoid", "cmax", "xmax", "cmin", "xmin", "ctid"];

/// What the catalog shows of foreign table `t`: its server and options, and each column's.
pub(super) fn table_info(t: &Table) -> Option<TableFdw> {
    let f = t.hier.foreign.as_ref()?;
    let by_id: Vec<(u64, Opts)> = f["cols"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| Some((c.get(0)?.as_u64()?, opts_of(c.get(1)?))))
        .collect();
    // (by position, as the catalog's columns are)
    let cols = t
        .cols
        .iter()
        .map(|c| by_id.iter().find(|(i, _)| *i == u64::from(c.id)).map(|(_, o)| o.clone()).unwrap_or_default())
        .collect();
    Some(TableFdw {
        server: f["server"].as_str().unwrap_or_default().to_string(),
        options: opts_of(&f["options"]),
        cols,
    })
}

/// 55000 for reading or writing foreign table `t`'s rows: its wrapper has no handler (and one that has is never
/// run here).
pub(super) fn readable(v: &View, t: &Table) -> Result<()> {
    let Some(f) = &t.hier.foreign else { return Ok(()) };
    let server = f["server"].as_str().unwrap_or_default();
    let o = load(v)?;
    let fdw = o.server(server).map(|s| s.fdw.clone()).unwrap_or_default();
    match o.fdw(&fdw).and_then(|w| w.handler.as_ref()) {
        None => err("55000", format!("foreign-data wrapper \"{fdw}\" has no handler")),
        Some(_) => err(
            UNSUPPORTED,
            format!(
                "foreign table \"{}\" can't be read: Chronos runs no foreign-data wrapper's code",
                rel_part(&t.name)
            ),
        ),
    }
}

/// How messages name relation `name`: `foreign table` for one, else `table`.
pub(super) fn rel_noun(v: &View, name: &str) -> &'static str {
    match super::load(v, name) {
        Ok(Some(t)) if t.hier.foreign.is_some() => "foreign table",
        _ => "table",
    }
}

/// 42809 when a unique index is made on partitioned table `t` and a partition below it is a foreign table.
pub(super) fn unique_on_partitions(v: &View, t: &Table) -> Result<()> {
    if t.hier.part.is_none() {
        return Ok(());
    }
    if hier::descendants(v, t)?.iter().any(|k| k.hier.foreign.is_some()) {
        return Err(unique_refused(&t.name));
    }
    Ok(())
}

/// 42809 `cannot create unique index on partitioned table`: partitioned table `table` has a foreign partition.
pub(super) fn unique_refused(table: &str) -> SqlError {
    let n = rel_part(table);
    SqlError {
        code: "42809",
        message: format!("cannot create unique index on partitioned table \"{n}\""),
        info: Some(Box::new(Violation {
            detail: Some(format!("Table \"{n}\" contains partitions that are foreign tables.")),
            ..Violation::default()
        })),
    }
}

/// 42809 for an ALTER TABLE action Postgres doesn't take on a foreign table.
pub(super) fn not_for_foreign(t: &Table, action: &str) -> Result<()> {
    if t.hier.foreign.is_none() {
        return Ok(());
    }
    Err(SqlError {
        code: "42809",
        message: format!("ALTER action {action} cannot be performed on relation \"{}\"", rel_part(&t.name)),
        info: Some(Box::new(Violation {
            detail: Some("This operation is not supported for foreign tables.".into()),
            ..Violation::default()
        })),
    })
}

/// 42809 for CREATE INDEX on foreign table `t`.
pub(super) fn no_index(t: &Table) -> Result<()> {
    if t.hier.foreign.is_none() {
        return Ok(());
    }
    Err(SqlError {
        code: "42809",
        message: format!("cannot create index on relation \"{}\"", rel_part(&t.name)),
        info: Some(Box::new(Violation {
            detail: Some("This operation is not supported for foreign tables.".into()),
            ..Violation::default()
        })),
    })
}

/// ATTACH PARTITION of foreign table `child`: 42809 if partitioned table `parent` has unique indexes.
pub(super) fn attachable(parent: &Table, child: &Table) -> Result<()> {
    if child.hier.foreign.is_none() || (!parent.has_key() && parent.uniques.is_empty()) {
        return Ok(());
    }
    Err(SqlError {
        code: "42809",
        message: format!(
            "cannot attach foreign table \"{}\" as partition of partitioned table \"{}\"",
            rel_part(&child.name),
            rel_part(&parent.name)
        ),
        info: Some(Box::new(Violation {
            detail: Some(format!("Partitioned table \"{}\" contains unique indexes.", rel_part(&parent.name))),
            ..Violation::default()
        })),
    })
}

// ---------------------------------------------------------------- comments, roles

/// COMMENT ON FOREIGN DATA WRAPPER (`fdw`) or SERVER (`server`) `name`: it has to be there, and be the role's.
pub(super) fn comment_target(v: &View, kind: &str, name: &str) -> Result<()> {
    let k = if kind == "fdw" { AclKind::Fdw } else { AclKind::Server };
    if !exists(v, kind, name)? {
        return Err(missing(k, name));
    }
    if let Some(who) = roles::checked() {
        let x = obj(v, k, name)?;
        if !who.has_privs_of(x.owner) {
            return Err(x.not_owner());
        }
    }
    Ok(())
}

/// The user mappings of roles `oids`: (role, server), in the order they were made.
pub(super) fn mappings_of(v: &View, oids: &[i64]) -> Result<Vec<(i64, String)>> {
    let o = load(v)?;
    let mut mine: Vec<&Mapping> = o.mappings.iter().filter(|m| oids.contains(&m.user)).collect();
    mine.sort_by_key(|m| m.at);
    Ok(mine.iter().map(|m| (m.user, m.server.clone())).collect())
}

/// The user mappings of role `oid`, as DROP ROLE's detail lists them.
pub(super) fn depending(v: &View, reg: &roles::Registry, oid: i64) -> Result<Vec<String>> {
    let o = load(v)?;
    let mut mine: Vec<&Mapping> = o.mappings.iter().filter(|m| m.user == oid).collect();
    mine.sort_by_key(|m| m.at);
    Ok(mine
        .iter()
        .map(|m| format!("owner of user mapping for {} on server {}", user_name(reg, m.user), m.server))
        .collect())
}

// ---------------------------------------------------------------- the catalog

/// The catalog's rows of `name` (pg_foreign_data_wrapper, pg_foreign_server, pg_user_mapping, pg_user_mappings,
/// pg_foreign_table), each column not named here null. What pg_user_mappings shows of a mapping's options is what
/// its view lets the running role see.
pub(super) fn catalog_rows(name: &str, cat: &Catalog) -> Vec<Vec<Value>> {
    use Value::{Int as int, Null};
    let t = |s: &str| Value::Text(s.into());
    let row = |fields: Vec<(&str, Value)>| catalog_schema::named_row(name, fields);
    let o = &cat.foreign;
    let shown = |kind: &str, n: &str| acl::shown(&cat.reg, cat.acls.get(&format!("{kind}/{n}")), BOOTSTRAP);
    let opt_text = |x: &Option<String>| x.as_deref().map_or(Null, t);
    match name {
        "pg_foreign_data_wrapper" => {
            o.fdws
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let (owner, acl) = shown("fdw", &f.name);
                    let handler = f.handler.as_ref().and_then(|h| {
                        cat.funcs.iter().position(|x| rel_part(&x.name) == rel_part(h) && x.args.is_empty())
                    });
                    row(vec![
                        ("oid", int(o.fdw_oids[i])),
                        ("fdwname", t(&f.name)),
                        ("fdwowner", int(owner)),
                        ("fdwhandler", int(handler.map_or(0, |k| cat.func_oid(k)))),
                        ("fdwvalidator", int(if f.validator.is_some() { 2316 } else { 0 })),
                        ("fdwacl", acl),
                        ("fdwoptions", options_value(&f.options)),
                    ])
                })
                .collect()
        }
        "pg_foreign_server" => o
            .servers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (owner, acl) = shown("server", &s.name);
                row(vec![
                    ("oid", int(o.server_oids[i])),
                    ("srvname", t(&s.name)),
                    ("srvowner", int(owner)),
                    ("srvfdw", int(o.fdw_oid(&s.fdw).unwrap_or(0))),
                    ("srvtype", opt_text(&s.kind)),
                    ("srvversion", opt_text(&s.version)),
                    ("srvacl", acl),
                    ("srvoptions", options_value(&s.options)),
                ])
            })
            .collect(),
        "pg_user_mapping" => o
            .mappings
            .iter()
            .enumerate()
            .map(|(i, m)| {
                row(vec![
                    ("oid", int(o.mapping_oids[i])),
                    ("umuser", int(m.user)),
                    ("umserver", int(o.server_oid(&m.server).unwrap_or(0))),
                    ("umoptions", options_value(&m.options)),
                ])
            })
            .collect(),
        "pg_user_mappings" => {
            let who = roles::current();
            o.mappings
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let owner = shown("server", &m.server).0;
                    let visible = match who.as_deref() {
                        None => true,
                        Some(w) if w.superuser => true,
                        Some(w) if m.user == PUBLIC => w.has_privs_of(owner),
                        Some(w) => m.user == w.oid && (w.has_privs_of(owner) || server_usage(cat, w, &m.server)),
                    };
                    row(vec![
                        ("umid", int(o.mapping_oids[i])),
                        ("srvid", int(o.server_oid(&m.server).unwrap_or(0))),
                        ("srvname", t(&m.server)),
                        ("umuser", int(m.user)),
                        ("usename", t(&user_name(&cat.reg, m.user))),
                        ("umoptions", if visible { options_value(&m.options) } else { Null }),
                    ])
                })
                .collect()
        }
        "pg_foreign_table" => cat
            .rels
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let f = r.foreign.as_ref()?;
                Some(row(vec![
                    ("ftrelid", int(cat.rel_oid(i))),
                    ("ftserver", int(o.server_oid(&f.server).unwrap_or(0))),
                    ("ftoptions", options_value(&f.options)),
                ]))
            })
            .collect(),
        _ => vec![],
    }
}

/// Whether role `w` holds USAGE on server `name` (its owner's or granted).
fn server_usage(cat: &Catalog, w: &Who, name: &str) -> bool {
    acl::foreign_obj_of(AclKind::Server, name, cat.acls.get(&format!("server/{name}"))).has(w, acl::USAGE)
}

fn fdw_usage(cat: &Catalog, w: &Who, name: &str) -> bool {
    acl::foreign_obj_of(AclKind::Fdw, name, cat.acls.get(&format!("fdw/{name}"))).has(w, acl::USAGE)
}

/// The information_schema views of wrappers, servers, user mappings and foreign tables, as Postgres's define them
/// (what the running role may see of them): their columns and rows.
#[allow(clippy::type_complexity)]
pub(super) fn info_schema(
    name: &str,
    database: &str,
    cat: &Catalog,
) -> Option<(Vec<(&'static str, Type)>, Vec<Vec<Value>>)> {
    let short = name.strip_prefix("information_schema.")?;
    if !INFO_VIEWS.contains(&name) {
        return None;
    }
    use Type::Text as T;
    let t = |s: &str| Value::Text(s.into());
    let null = Value::Null;
    let o = &cat.foreign;
    let who = roles::current().map_or_else(|| Who::boot("chronos"), |w| (*w).clone());
    let owner_of = |kind: &str, n: &str| acl::shown(&cat.reg, cat.acls.get(&format!("{kind}/{n}")), BOOTSTRAP).0;
    // (Postgres's _pg_foreign_data_wrappers and _pg_foreign_servers: those the role owns or may use)
    let fdws: Vec<&Fdw> =
        o.fdws.iter().filter(|f| who.has_privs_of(owner_of("fdw", &f.name)) || fdw_usage(cat, &who, &f.name)).collect();
    let servers: Vec<&Server> = o
        .servers
        .iter()
        .filter(|s| who.has_privs_of(owner_of("server", &s.name)) || server_usage(cat, &who, &s.name))
        .collect();
    let mappings: Vec<&Mapping> = o.mappings.iter().filter(|m| servers.iter().any(|s| s.name == m.server)).collect();
    let auth = |user: i64| if user == PUBLIC { "PUBLIC".to_string() } else { cat.reg.name_of(user) };
    let mut rows: Vec<Vec<Value>> = vec![];
    let cols: Vec<(&'static str, Type)> = match short {
        "foreign_data_wrappers" => {
            for f in &fdws {
                rows.push(vec![
                    t(database),
                    t(&f.name),
                    t(&cat.reg.name_of(owner_of("fdw", &f.name))),
                    null.clone(),
                    t("c"),
                ]);
            }
            vec![
                ("foreign_data_wrapper_catalog", T),
                ("foreign_data_wrapper_name", T),
                ("authorization_identifier", T),
                ("library_name", T),
                ("foreign_data_wrapper_language", T),
            ]
        }
        "foreign_data_wrapper_options" => {
            for f in &fdws {
                for (k, x) in &f.options {
                    rows.push(vec![t(database), t(&f.name), t(k), t(x)]);
                }
            }
            vec![
                ("foreign_data_wrapper_catalog", T),
                ("foreign_data_wrapper_name", T),
                ("option_name", T),
                ("option_value", T),
            ]
        }
        "foreign_servers" => {
            for s in &servers {
                rows.push(vec![
                    t(database),
                    t(&s.name),
                    t(database),
                    t(&s.fdw),
                    s.kind.as_deref().map_or(null.clone(), t),
                    s.version.as_deref().map_or(null.clone(), t),
                    t(&cat.reg.name_of(owner_of("server", &s.name))),
                ]);
            }
            vec![
                ("foreign_server_catalog", T),
                ("foreign_server_name", T),
                ("foreign_data_wrapper_catalog", T),
                ("foreign_data_wrapper_name", T),
                ("foreign_server_type", T),
                ("foreign_server_version", T),
                ("authorization_identifier", T),
            ]
        }
        "foreign_server_options" => {
            for s in &servers {
                for (k, x) in &s.options {
                    rows.push(vec![t(database), t(&s.name), t(k), t(x)]);
                }
            }
            vec![("foreign_server_catalog", T), ("foreign_server_name", T), ("option_name", T), ("option_value", T)]
        }
        "user_mappings" => {
            for m in &mappings {
                rows.push(vec![t(&auth(m.user)), t(database), t(&m.server)]);
            }
            vec![("authorization_identifier", T), ("foreign_server_catalog", T), ("foreign_server_name", T)]
        }
        "user_mapping_options" => {
            for m in &mappings {
                let owner = owner_of("server", &m.server);
                // (the values only the mapping's role, the server's owner for PUBLIC's, and a superuser see)
                let visible = who.superuser
                    || (m.user != PUBLIC && m.user == who.oid)
                    || (m.user == PUBLIC && who.has_privs_of(owner));
                for (k, x) in &m.options {
                    rows.push(vec![
                        t(&auth(m.user)),
                        t(database),
                        t(&m.server),
                        t(k),
                        if visible { t(x) } else { null.clone() },
                    ]);
                }
            }
            vec![
                ("authorization_identifier", T),
                ("foreign_server_catalog", T),
                ("foreign_server_name", T),
                ("option_name", T),
                ("option_value", T),
            ]
        }
        "foreign_tables" | "foreign_table_options" => {
            let options = short == "foreign_table_options";
            for r in &cat.rels {
                let Some(f) = r.foreign.as_ref() else { continue };
                let (owner, _) = cat.rel_acl(r);
                // (those the role owns or has a privilege on)
                let mine = who.has_privs_of(owner)
                    || acl::rel_privileged(cat.acls.get(&format!("rel/{}", r.name)), owner, &who);
                if !mine {
                    continue;
                }
                let (schema, table) = (schema_part(&r.name), rel_part(&r.name));
                if options {
                    for (k, x) in &f.options {
                        rows.push(vec![t(database), t(schema), t(table), t(k), t(x)]);
                    }
                } else {
                    rows.push(vec![t(database), t(schema), t(table), t(database), t(&f.server)]);
                }
            }
            match options {
                true => vec![
                    ("foreign_table_catalog", T),
                    ("foreign_table_schema", T),
                    ("foreign_table_name", T),
                    ("option_name", T),
                    ("option_value", T),
                ],
                false => vec![
                    ("foreign_table_catalog", T),
                    ("foreign_table_schema", T),
                    ("foreign_table_name", T),
                    ("foreign_server_catalog", T),
                    ("foreign_server_name", T),
                ],
            }
        }
        _ => return None,
    };
    Some((cols, rows))
}

/// The information_schema views here, as the catalog names them.
pub(super) const INFO_VIEWS: [&str; 8] = [
    "information_schema.foreign_data_wrappers",
    "information_schema.foreign_data_wrapper_options",
    "information_schema.foreign_servers",
    "information_schema.foreign_server_options",
    "information_schema.user_mappings",
    "information_schema.user_mapping_options",
    "information_schema.foreign_tables",
    "information_schema.foreign_table_options",
];
