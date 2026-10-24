//! Statements Postgres has for looking after storage and sessions, that a database made of
//! copy-on-write pages has little or nothing to do for: VACUUM, ANALYZE and REINDEX (no dead rows
//! to reclaim, indexes always current, no statistics kept), CLUSTER (marks its index, and orders
//! the rows of a table without a primary key: see `cluster`), DISCARD (resets a session: what
//! pools send between clients), COMMENT ON (checked, not kept) and LOCK TABLE.
//!
//! Each is parsed as Postgres parses it and refuses what Postgres refuses (names that don't
//! exist, the wrong kind of object, a transaction block where there mustn't be one), so a
//! migration or pool that works here works on Postgres, and a mistake is found in both.

use super::catalog::{Catalog, Obj};
use super::*;

/// What a maintenance statement names.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Target {
    /// a table as written (maybe `schema.name`), and the columns listed after it
    Rel(String, Vec<String>),
    Index(String),
    Schema(String),
    /// the whole database, or every table: nothing to look up
    Everything,
    /// REINDEX DATABASE: the database's own indexes (not the catalogs', which REINDEX SYSTEM names as `Everything`)
    Database,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Verb {
    Vacuum,
    Analyze,
    Reindex,
    Cluster,
}

impl Verb {
    fn tag(self) -> &'static str {
        match self {
            Verb::Vacuum => "VACUUM",
            Verb::Analyze => "ANALYZE",
            Verb::Reindex => "REINDEX",
            Verb::Cluster => "CLUSTER",
        }
    }
}

/// VACUUM, ANALYZE, REINDEX or CLUSTER.
#[derive(Clone, Debug)]
pub(super) struct Maintain {
    pub verb: Verb,
    pub targets: Vec<Target>,
    /// CLUSTER t USING index
    pub using: Option<String>,
    /// REINDEX CONCURRENTLY, which can't run in a transaction block
    pub concurrently: bool,
    /// REINDEX (TABLESPACE name): where the indexes rebuilt are moved (see `tablespace`)
    pub tablespace: Option<String>,
    /// VACUUM (with ANALYZE or not): what a skipped table's warning says it couldn't do
    pub vacuum: bool,
    /// VACUUM (PARALLEL n), n > 0: which a temporary table can't be vacuumed with
    pub parallel: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Discard {
    All,
    Plans,
    Sequences,
    Temp,
}

/// What COMMENT ON names, as far as it can be checked.
#[derive(Clone, Debug)]
pub(super) enum Commented {
    /// a relation of this kind (`table`, `view`, `materialized view`, `sequence`, `index`)
    Rel(&'static str, String),
    /// table, column
    Column(String, String),
    Schema(String),
    /// constraint, table
    Constraint(String, String),
    /// constraint, domain (ON DOMAIN): its comment isn't kept, but both have to exist
    DomainConstraint(String, String),
    /// an aggregate: its row's key, and its signature for a message
    Aggregate(String, String),
    /// a foreign-data wrapper (`fdw`) or server (`server`), by name
    Foreign(&'static str, String),
    /// a rule, table (ON table): its comment isn't kept, but both have to exist
    Rule(String, String),
    /// a trigger, table (ON table): the same
    Trigger(String, String),
    /// a type or domain, as stored (`app.mood`): pg_description shows the comment of an enum type
    Type(String),
    /// a function or procedure, by name and argument types (None: the one of that name)
    Func(String, Option<Vec<Type>>, bool),
    /// a role, by name
    Role(String),
    /// a publication, by name
    Publication(String),
    /// a large object, by its oid
    LargeObject(i64),
    /// a database, extension, function, role...: nothing here to look up
    Other,
}

#[derive(Clone, Debug)]
pub(super) struct Comment {
    pub on: Commented,
    /// the text; None for NULL (which takes the comment away, as an empty string does)
    pub text: Option<String>,
}

/// LOCK TABLE
#[derive(Clone, Debug)]
pub(super) struct Lock {
    /// each table as written, and whether ONLY (its children aren't locked)
    pub tables: Vec<(String, bool)>,
    /// held against writers as well as other lockers: a mode that conflicts with its own kind
    pub strong: bool,
    pub nowait: bool,
    /// the mode, as pg_locks names it (`AccessExclusiveLock`)
    pub mode: &'static str,
}

/// BUFFER_USAGE_LIMIT's value as Postgres's parse_int reads it in kB (`512 kB`, `1MB`, or a number of kB): 0, or
/// from 128 kB to 16 GB.
fn buffer_usage_limit(v: &str) -> Result<()> {
    let refuse = |hint: Option<&str>| SqlError {
        code: "22023",
        message: "BUFFER_USAGE_LIMIT option must be 0 or between 128 kB and 16777216 kB".into(),
        info: hint.map(|h| Box::new(Violation { hint: Some(h.into()), ..Violation::default() })),
    };
    let t = v.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len());
    let (n, unit) = (&t[..digits], t[digits..].trim());
    let Ok(n) = n.parse::<f64>() else { return Err(refuse(None)) };
    let kb = match unit {
        "" | "kB" => n,
        "MB" => n * 1024.0,
        "GB" => n * 1024.0 * 1024.0,
        "TB" => n * 1024.0 * 1024.0 * 1024.0,
        "B" => n / 1024.0,
        _ => return Err(refuse(None)),
    };
    let kb = kb.round_ties_even();
    if kb > f64::from(i32::MAX) {
        return Err(refuse(Some("Value exceeds integer range.")));
    }
    if kb != 0.0 && !(128.0..=16_777_216.0).contains(&kb) {
        return Err(refuse(None));
    }
    Ok(())
}

impl Parser {
    /// True at the end of the statement.
    pub(super) fn at_end(&self) -> bool {
        matches!(self.peek(), None | Some(Tok::Sym(";")))
    }

    /// `name` or `schema.name`, as written.
    pub(super) fn rel_text(&mut self) -> Result<String> {
        let first = self.name()?;
        Ok(if self.sym(".") { format!("{first}.{}", self.name()?) } else { first })
    }

    /// `( option [value] [, ...] )` after its `(`, each option one of `known`: (name, value's text, and where its
    /// name is, for an error that points at it). A value is a word, a string or a number, maybe negative.
    fn options(&mut self, verb: &str, known: &[&str]) -> Result<Vec<(String, Option<String>, usize)>> {
        let mut out = vec![];
        loop {
            let at = self.i;
            let name = match self.next() {
                Some(Tok::Word(w)) => w,
                t => {
                    self.put_back(t);
                    return self.fail("expected an option name");
                }
            };
            let value = match self.peek() {
                Some(Tok::Word(_) | Tok::Num(_) | Tok::Str(_)) => match self.next() {
                    Some(Tok::Word(v) | Tok::Num(v) | Tok::Str(v)) => Some(v),
                    _ => None,
                },
                // (a negative number; `-` before anything else is a syntax error at what follows it)
                Some(Tok::Sym("-")) => {
                    self.i += 1;
                    match self.next() {
                        Some(Tok::Num(n)) => Some(format!("-{n}")),
                        t => {
                            self.put_back(t);
                            return self.fail("expected a number");
                        }
                    }
                }
                _ => None,
            };
            if !known.contains(&name.as_str()) {
                return self.err_at(at, SYNTAX, &format!("unrecognized {verb} option \"{name}\""));
            }
            out.push((name, value, at));
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(out)
    }

    /// After VACUUM or ANALYZE. Its options are checked as Postgres's ExecVacuum checks them, though none changes
    /// what it does here.
    pub(super) fn vacuum(&mut self, verb: Verb) -> Result<Stmt> {
        let mut analyze = verb == Verb::Analyze;
        // (FULL, PARALLEL's workers, BUFFER_USAGE_LIMIT given, DISABLE_PAGE_SKIPPING, PROCESS_TOAST, ONLY_DATABASE_STATS
        // and whether another option is given with it)
        let (mut full, mut workers, mut limit, mut skipping, mut toast, mut only, mut others) =
            (false, 0, false, false, true, false, false);
        let on =
            |v: &Option<String>| !matches!(v.as_deref().map(str::to_lowercase).as_deref(), Some("false" | "off" | "0"));
        if self.sym("(") {
            let known: &[&str] = if verb == Verb::Vacuum {
                &[
                    "analyze",
                    "analyse",
                    "full",
                    "freeze",
                    "verbose",
                    "disable_page_skipping",
                    "skip_locked",
                    "index_cleanup",
                    "process_main",
                    "process_toast",
                    "truncate",
                    "parallel",
                    "skip_database_stats",
                    "only_database_stats",
                    "buffer_usage_limit",
                ]
            } else {
                &["verbose", "skip_locked", "buffer_usage_limit"]
            };
            for (name, value, at) in self.options(verb.tag(), known)? {
                others |=
                    !matches!(name.as_str(), "verbose" | "process_main" | "process_toast" | "only_database_stats");
                match name.as_str() {
                    "analyze" | "analyse" => analyze = on(&value),
                    "full" => full = on(&value),
                    "disable_page_skipping" => skipping = on(&value),
                    "process_toast" => toast = on(&value),
                    "only_database_stats" => only = on(&value),
                    "parallel" => {
                        let Some(v) = value else {
                            return self.err_at(at, SYNTAX, "parallel option requires a value between 0 and 1024");
                        };
                        match v.parse::<i64>() {
                            Ok(n) if (0..=1024).contains(&n) => workers = n,
                            _ => {
                                return self.err_at(
                                    at,
                                    SYNTAX,
                                    "parallel workers for vacuum must be between 0 and 1024",
                                );
                            }
                        }
                    }
                    "buffer_usage_limit" => {
                        buffer_usage_limit(value.as_deref().unwrap_or_default())?;
                        limit = true;
                    }
                    _ => {}
                }
            }
        } else if verb == Verb::Vacuum {
            // the older spelling: FULL, FREEZE, VERBOSE and ANALYZE, in any order
            loop {
                if self.kw("full") {
                    full = true;
                    continue;
                }
                if self.kw("freeze") || self.kw("verbose") {
                    continue;
                }
                if self.kw("analyze") || self.kw("analyse") {
                    analyze = true;
                    continue;
                }
                break;
            }
        } else {
            self.kw("verbose");
        }
        let mut targets = vec![];
        let mut listed = false;
        while !self.at_end() {
            let name = self.rel_text()?;
            let cols = if self.sym("(") { self.names()? } else { vec![] };
            listed |= !cols.is_empty();
            targets.push(Target::Rel(name, cols));
            if !self.sym(",") {
                break;
            }
        }
        let bad = |message: &str| err(UNSUPPORTED, message.to_string());
        if full && workers > 0 {
            return bad("VACUUM FULL cannot be performed in parallel");
        }
        if full && limit {
            return bad("BUFFER_USAGE_LIMIT cannot be specified for VACUUM FULL");
        }
        if verb == Verb::Vacuum && !analyze && listed {
            return bad("ANALYZE option must be specified when a column list is provided");
        }
        if full && skipping {
            return bad("VACUUM option DISABLE_PAGE_SKIPPING cannot be used with FULL");
        }
        if full && !toast {
            return bad("PROCESS_TOAST required with VACUUM FULL");
        }
        if only && !targets.is_empty() {
            return bad("ONLY_DATABASE_STATS cannot be specified with a list of tables");
        }
        if only && (others || analyze || full) {
            return bad("ONLY_DATABASE_STATS cannot be specified with other VACUUM options");
        }
        if targets.is_empty() {
            targets.push(Target::Everything);
        }
        Ok(Stmt::Maintain(Box::new(Maintain {
            verb,
            targets,
            using: None,
            concurrently: false,
            tablespace: None,
            vacuum: verb == Verb::Vacuum,
            parallel: workers > 0,
        })))
    }

    /// After REINDEX: `[(options)] {INDEX | TABLE | SCHEMA | DATABASE | SYSTEM} [CONCURRENTLY] name`.
    pub(super) fn reindex(&mut self) -> Result<Stmt> {
        let (mut concurrently, mut tablespace) = (false, None);
        if self.sym("(") {
            for (name, value, _) in self.options("REINDEX", &["concurrently", "tablespace", "verbose"])? {
                if name == "concurrently" {
                    concurrently = !matches!(value.as_deref(), Some("false" | "off" | "0"));
                }
                if name == "tablespace" {
                    tablespace = value;
                }
            }
        }
        let kind = match self.next() {
            Some(Tok::Word(w)) if matches!(w.as_str(), "index" | "table" | "schema" | "database" | "system") => w,
            t => {
                self.put_back(t);
                return self.fail("expected INDEX, TABLE, SCHEMA, DATABASE or SYSTEM");
            }
        };
        concurrently |= self.kw("concurrently");
        let target = match kind.as_str() {
            "index" => Target::Index(self.rel_text()?),
            "table" => Target::Rel(self.rel_text()?, vec![]),
            "schema" => Target::Schema(self.name()?),
            _ => {
                if !self.at_end() {
                    self.name()?; // (the database's name: only the one there is)
                }
                if kind == "database" { Target::Database } else { Target::Everything }
            }
        };
        Ok(Stmt::Maintain(Box::new(Maintain {
            verb: Verb::Reindex,
            targets: vec![target],
            using: None,
            concurrently,
            tablespace,
            vacuum: false,
            parallel: false,
        })))
    }

    /// After CLUSTER: `[VERBOSE] [table [USING index]]`, or the old `index ON table`.
    pub(super) fn cluster(&mut self) -> Result<Stmt> {
        if self.sym("(") {
            self.options("CLUSTER", &["verbose"])?;
        } else {
            self.kw("verbose");
        }
        let (target, using) = if self.at_end() {
            (Target::Everything, None)
        } else {
            let first = self.rel_text()?;
            if self.kw("on") {
                (Target::Rel(self.rel_text()?, vec![]), Some(first))
            } else {
                (Target::Rel(first, vec![]), if self.kw("using") { Some(self.name()?) } else { None })
            }
        };
        Ok(Stmt::Maintain(Box::new(Maintain {
            verb: Verb::Cluster,
            targets: vec![target],
            using,
            concurrently: false,
            tablespace: None,
            vacuum: false,
            parallel: false,
        })))
    }

    /// After DISCARD.
    pub(super) fn discard(&mut self) -> Result<Stmt> {
        Ok(Stmt::Discard(if self.kw("all") {
            Discard::All
        } else if self.kw("plans") {
            Discard::Plans
        } else if self.kw("sequences") {
            Discard::Sequences
        } else if self.kw("temp") || self.kw("temporary") {
            Discard::Temp
        } else {
            return self.fail("expected ALL, PLANS, SEQUENCES or TEMP");
        }))
    }

    /// After COMMENT: `ON object IS 'text' | NULL`.
    pub(super) fn comment(&mut self) -> Result<Stmt> {
        self.expect_kw("on")?;
        let kind = match self.next() {
            Some(Tok::Word(w)) => w,
            t => {
                self.put_back(t);
                return self.fail("expected the kind of object to comment on");
            }
        };
        let on = match kind.as_str() {
            "table" | "view" | "sequence" | "index" => {
                let k = match kind.as_str() {
                    "table" => "table",
                    "view" => "view",
                    "sequence" => "sequence",
                    _ => "index",
                };
                Commented::Rel(k, self.rel_text()?)
            }
            "materialized" => {
                self.expect_kw("view")?;
                Commented::Rel("materialized view", self.rel_text()?)
            }
            "column" => {
                let mut parts = vec![self.name()?];
                while self.sym(".") {
                    parts.push(self.name()?);
                }
                let Some(column) = parts.pop().filter(|_| !parts.is_empty()) else {
                    return self.fail("expected table.column");
                };
                Commented::Column(parts.join("."), column)
            }
            "schema" => Commented::Schema(self.name()?),
            "server" => Commented::Foreign("server", self.name()?),
            "foreign" if self.kw("table") => Commented::Rel("foreign table", self.rel_text()?),
            "foreign" => {
                self.expect_kw("data")?;
                self.expect_kw("wrapper")?;
                Commented::Foreign("fdw", self.name()?)
            }
            "aggregate" => {
                let (key, shown) = self.aggregate_target()?;
                Commented::Aggregate(key, shown)
            }
            "constraint" => {
                let name = self.name()?;
                self.expect_kw("on")?;
                if self.kw("domain") {
                    Commented::DomainConstraint(name, self.qualified(false, Kind::Type)?)
                } else {
                    Commented::Constraint(name, self.rel_text()?)
                }
            }
            "rule" => {
                let name = self.name()?;
                self.expect_kw("on")?;
                Commented::Rule(name, self.qualified(false, Kind::Rel)?)
            }
            "trigger" => {
                let name = self.name()?;
                self.expect_kw("on")?;
                Commented::Trigger(name, self.qualified(false, Kind::Rel)?)
            }
            "type" | "domain" => Commented::Type(self.qualified(false, Kind::Type)?),
            "function" | "procedure" => {
                let procedure = kind == "procedure";
                match self.drop_function(procedure)? {
                    Stmt::DropFunction { mut names, .. } if names.len() == 1 => {
                        let (name, types) = names.pop().unwrap_or_default();
                        Commented::Func(name, types, procedure)
                    }
                    _ => return self.fail("expected one function"),
                }
            }
            "role" => Commented::Role(self.name()?),
            "publication" => Commented::Publication(self.name()?),
            "large" => {
                self.expect_kw("object")?;
                Commented::LargeObject(self.large_object_oid()?)
            }
            "database" | "extension" | "routine" | "user" | "group" | "policy" | "subscription" | "collation"
            | "conversion" | "language" | "operator" | "cast" | "transform" | "tablespace" | "statistics" | "text"
            | "access" | "event" => {
                // skip the name (a function's has a signature): up to IS outside parentheses
                let mut depth = 0;
                while let Some(t) = self.peek() {
                    match t {
                        Tok::Sym("(") => depth += 1,
                        Tok::Sym(")") => depth -= 1,
                        Tok::Word(w) if w == "is" && depth == 0 => break,
                        _ => {}
                    }
                    self.i += 1;
                }
                Commented::Other
            }
            _ => {
                self.i -= 1;
                return self.fail("expected the kind of object to comment on");
            }
        };
        self.expect_kw("is")?;
        // 'text', E'text', or NULL
        let text = match self.next() {
            Some(Tok::Str(t)) => Some(t),
            Some(Tok::Word(w)) if w == "null" => None,
            Some(Tok::Word(w)) if w == "e" && matches!(self.peek(), Some(Tok::Str(_))) => {
                let Some(Tok::Str(t)) = self.next() else { unreachable!("peeked a string") };
                Some(t)
            }
            t => {
                self.put_back(t);
                return self.fail("expected a string or NULL");
            }
        };
        Ok(Stmt::Comment(Box::new(Comment { on, text })))
    }

    /// After LOCK: `[TABLE] [ONLY] name [*] [, ...] [IN mode MODE] [NOWAIT]`.
    pub(super) fn lock(&mut self) -> Result<Stmt> {
        self.kw("table");
        let mut tables = vec![];
        loop {
            let only = self.kw("only");
            tables.push((self.rel_text()?, only));
            self.sym("*");
            if !self.sym(",") {
                break;
            }
        }
        // the default, ACCESS EXCLUSIVE, conflicts with everything
        let mut mode = "AccessExclusiveLock";
        if self.kw("in") {
            mode = if self.kw("access") {
                if self.kw("share") {
                    "AccessShareLock"
                } else {
                    self.expect_kw("exclusive")?;
                    "AccessExclusiveLock"
                }
            } else if self.kw("row") {
                if self.kw("share") {
                    "RowShareLock"
                } else if self.kw("exclusive") {
                    "RowExclusiveLock"
                } else {
                    return self.fail("expected SHARE or EXCLUSIVE");
                }
            } else if self.kw("share") {
                if self.kw("update") {
                    self.expect_kw("exclusive")?;
                    "ShareUpdateExclusiveLock"
                } else if self.kw("row") {
                    self.expect_kw("exclusive")?;
                    "ShareRowExclusiveLock"
                } else {
                    "ShareLock"
                }
            } else {
                self.expect_kw("exclusive")?;
                "ExclusiveLock"
            };
            self.expect_kw("mode")?;
        }
        // (SHARE UPDATE EXCLUSIVE and stronger conflict with their own kind, or with writers)
        let strong = !matches!(mode, "AccessShareLock" | "RowShareLock" | "RowExclusiveLock");
        let nowait = self.kw("nowait");
        Ok(Stmt::Lock(Box::new(Lock { tables, strong, nowait, mode })))
    }
}

/// No relation of that name (as written).
fn no_rel(name: &str) -> SqlError {
    SqlError { code: NO_TABLE, message: format!("relation \"{name}\" does not exist"), info: None }
}

fn wrong_kind(name: &str, what: &str) -> SqlError {
    SqlError { code: "42809", message: format!("\"{name}\" is not {what}"), info: None }
}

/// Finds the relation `text` names, or `None` for a session's temporary table (which has no
/// entry in the catalog).
fn find_rel<'a>(cat: &'a Catalog, text: &str) -> Result<Option<(usize, &'a TableInfo)>> {
    match cat.named(text)?.and_then(|o| cat.find(o)) {
        Some(Obj::Rel(i)) => Ok(Some((i, &cat.rels[i]))),
        Some(Obj::Index(..)) => Err(wrong_kind(text, "a table")),
        _ if has_temp(&text_rel(text)) => Ok(None),
        _ => Err(no_rel(text)),
    }
}

fn no_column(column: &str, rel: &str) -> SqlError {
    SqlError {
        code: "42703",
        message: format!("column \"{column}\" of relation \"{}\" does not exist", rel_part(rel)),
        info: None,
    }
}

/// VACUUM and ANALYZE: nothing to do, once the names check out. REINDEX: what pg_class shows of a rebuild (see
/// `rebuild`). CLUSTER: see `cluster`.
pub(super) fn maintain(db: &Db, branch: &str, m: &Maintain) -> Result<Output> {
    if m.verb == Verb::Cluster {
        return cluster(db, branch, m);
    }
    // REINDEX (TABLESPACE t): t is there and the role may create there, before anything is looked up (as Postgres's
    // ExecReindex checks it); a catalog's indexes are neither moved nor rebuilt concurrently
    let mut space = None;
    if m.verb == Verb::Reindex {
        if let Some(n) = &m.tablespace {
            space = Some(super::tablespace::target(&db.view_tx(branch)?, n)?);
        }
        for target in &m.targets {
            let (Target::Index(name) | Target::Rel(name, _)) = target else { continue };
            let Some(first) = super::tablespace::system_index(name) else { continue };
            // (the bootstrap superuser's, as every catalog is)
            if let Some(w) = roles::checked()
                && !w.has_privs_of(roles::BOOTSTRAP)
            {
                let noun = if matches!(target, Target::Index(_)) { "index" } else { "table" };
                return err("42501", format!("must be owner of {noun} {}", rel_part(&text_rel(name))));
            }
            if m.concurrently {
                return err(UNSUPPORTED, "cannot reindex system catalogs concurrently");
            }
            if space.is_some() {
                return err(UNSUPPORTED, format!("cannot move system relation \"{first}\""));
            }
            return Ok(Output::tag(m.verb.tag()));
        }
    }
    let cat = Catalog::of(&db.core, branch)?;
    // (REINDEX INDEX and SCHEMA: the index's owner, its table's, and the schema's, as Postgres's
    // RangeVarCallbackForReindexIndex and ReindexMultipleTables ask, after the tablespace; REINDEX TABLE's is asked
    // with the statement's privileges, see `acl`)
    let who = if m.verb == Verb::Reindex { roles::checked() } else { None };
    for target in &m.targets {
        match target {
            Target::Everything | Target::Database => {}
            Target::Schema(s) => {
                if !cat.schemas.contains(s) && !matches!(s.as_str(), "public" | "pg_catalog" | "information_schema") {
                    return err("3F000", format!("schema \"{s}\" does not exist"));
                }
                if let Some(w) = &who {
                    let o = super::acl::schema(&db.view_tx(branch)?, s)?;
                    if !w.has_privs_of(o.owner) {
                        return Err(o.not_owner());
                    }
                }
            }
            Target::Index(name) => match cat.named(name)?.and_then(|o| cat.find(o)) {
                Some(Obj::Index(r, _)) => {
                    if let Some(w) = &who
                        && !w.has_privs_of(cat.rel_acl(&cat.rels[r]).0)
                    {
                        return err("42501", format!("must be owner of index {}", rel_part(&text_rel(name))));
                    }
                }
                Some(_) => return Err(wrong_kind(name, "an index")),
                None => return Err(no_rel(name)),
            },
            Target::Rel(name, cols) => {
                let vacuuming = matches!(m.verb, Verb::Vacuum | Verb::Analyze);
                // (as Postgres's vacuum_is_permitted_for_relation: a table's owner, or a role with its privileges, or
                // a superuser; another is warned and the table skipped)
                let skipped = |owner: i64, table: &str| {
                    let Some(who) = super::roles::checked() else { return false };
                    if who.has_privs_of(owner) {
                        return false;
                    }
                    let what = if m.vacuum { "vacuum" } else { "analyze" };
                    notice("WARNING", format!("permission denied to {what} \"{table}\", skipping it"));
                    true
                };
                // a catalog table (pg_class, pg_authid): the bootstrap superuser's
                if vacuuming
                    && let Some(o) = cat.named(name)?
                    && cat.find(o).is_none()
                {
                    skipped(super::roles::BOOTSTRAP, rel_part(&text_rel(name)));
                    continue;
                }
                // (a temporary table: in the temporary tables' database)
                if vacuuming && m.parallel && has_temp(&text_rel(name)) {
                    let table = rel_part(&text_rel(name)).to_string();
                    notice(
                        "WARNING",
                        format!(
                            "disabling parallel option of vacuum on \"{table}\" --- cannot vacuum temporary tables in parallel"
                        ),
                    );
                }
                let Some((i, r)) = find_rel(&cat, name).or_else(|e| {
                    // VACUUM and ANALYZE skip what isn't a table, with a warning; the others refuse it
                    if e.code == "42809" && matches!(m.verb, Verb::Vacuum | Verb::Analyze) {
                        notice(
                            "WARNING",
                            format!(
                                "skipping \"{name}\" --- cannot {} non-tables or special system tables",
                                m.verb.tag().to_lowercase()
                            ),
                        );
                        return Ok(None);
                    }
                    // (REINDEX TABLE names what it takes: a table or a materialized view)
                    if e.code == "42809" && m.verb == Verb::Reindex {
                        return Err(wrong_kind(name, "a table or materialized view"));
                    }
                    Err(e)
                })?
                else {
                    continue;
                };
                if !matches!(r.kind, 'r' | 'm' | 'p') {
                    if matches!(m.verb, Verb::Vacuum | Verb::Analyze) {
                        notice(
                            "WARNING",
                            format!(
                                "skipping \"{name}\" --- cannot {} non-tables or special system tables",
                                m.verb.tag().to_lowercase()
                            ),
                        );
                        continue;
                    }
                    return Err(wrong_kind(name, "a table or materialized view"));
                }
                if let Some(c) = cols.iter().find(|c| !r.columns.iter().any(|x| x.0 == **c)) {
                    return Err(no_column(c, &r.name));
                }
                // the table, and a partitioned table's partitions, each its owner's to vacuum
                if vacuuming {
                    let mut tree = vec![i];
                    let mut k = 0;
                    while k < tree.len() {
                        let below = cat.rels[tree[k]].hier.created.clone();
                        tree.extend(below.iter().filter_map(|n| cat.rels.iter().position(|x| x.name == *n)));
                        k += 1;
                    }
                    if cat.rels[i].kind != 'p' {
                        tree.truncate(1);
                    }
                    for j in tree {
                        let r = &cat.rels[j];
                        skipped(cat.rel_acl(r).0, rel_part(&r.name));
                    }
                }
                if let Some(idx) = &m.using
                    && !cat.idx.iter().any(|(rel, x)| *rel == i && x.name == *idx)
                {
                    return err("42704", format!("index \"{idx}\" for table \"{}\" does not exist", rel_part(&r.name)));
                }
            }
        }
    }
    if m.verb == Verb::Reindex {
        rebuild(db, branch, m, space)?;
    }
    Ok(Output::tag(m.verb.tag()))
}

/// What REINDEX shows of the indexes it rebuilds (each named, or each of a table's; a partitioned table's or index's
/// are its leaf partitions'): a new filenode each, and with TABLESPACE the tablespace they are moved to (see
/// `tablespace::move_rel`). Their entries are always current here; this is what pg_class says of a rebuild, in the
/// statement's transaction. A rebuild is written to the tables' filenode rows alone (see `tablespace::Gens`), so it
/// merges with any change to their schemas; only a move changes a schema row.
fn rebuild(db: &Db, branch: &str, m: &Maintain, space: Option<i64>) -> Result<()> {
    db.batch_with::<(), SqlError>(branch, |v| {
        let cat = Catalog::of(v.core, &v.branch)?;
        // (table, index), each once
        let mut todo: Vec<(String, String)> = vec![];
        let mut add = |t: &str, x: &str| {
            if !todo.iter().any(|(a, b)| a == t && b == x) {
                todo.push((t.to_string(), x.to_string()));
            }
        };
        // (the tables below a partitioned one that hold rows)
        let leaves = |i: usize| {
            let (mut out, mut stack) = (vec![], vec![i]);
            while let Some(k) = stack.pop() {
                let r = &cat.rels[k];
                if r.kind != 'p' {
                    out.push(k);
                    continue;
                }
                stack.extend(r.hier.created.iter().filter_map(|n| cat.rels.iter().position(|x| x.name == *n)));
            }
            out
        };
        for target in &m.targets {
            match target {
                Target::Index(name) => {
                    let Some(Obj::Index(r, x)) = cat.named(name)?.and_then(|o| cat.find(o)) else { continue };
                    let owner = &cat.rels[r];
                    if owner.kind == 'p' {
                        for (kid, n) in hier::leaf_indexes(v, &need_table(v, &owner.name)?, &x.name)? {
                            add(&kid.name, &n);
                        }
                    } else {
                        add(&owner.name, &x.name);
                    }
                }
                Target::Rel(name, _) => {
                    let Ok(Some((i, _))) = find_rel(&cat, name) else { continue };
                    for k in leaves(i) {
                        for (_, x) in cat.idx.iter().filter(|(r, _)| *r == k) {
                            add(&cat.rels[k].name, &x.name);
                        }
                    }
                }
                Target::Schema(s) => {
                    for (r, x) in cat.idx.iter().filter(|(r, _)| schema_part(&cat.rels[*r].name) == s) {
                        if cat.rels[*r].kind != 'p' {
                            add(&cat.rels[*r].name, &x.name);
                        }
                    }
                }
                // REINDEX DATABASE: every index of the database's tables and materialized views the role owns (a
                // partitioned table's are its partitions'), as Postgres 16's, which leaves the catalogs' to REINDEX
                // SYSTEM; a session's temporary tables are a database of their own, not reached here
                Target::Database => {
                    for (r, x) in &cat.idx {
                        let rel = &cat.rels[*r];
                        if matches!(rel.kind, 'r' | 'm') && owns(v, &rel.name)? {
                            add(&rel.name, &x.name);
                        }
                    }
                }
                // (REINDEX SYSTEM rebuilds the catalogs', which aren't shown here)
                Target::Everything => {}
            }
        }
        // (a relation every database shares is the only kind pg_global holds)
        if space == Some(super::tablespace::GLOBAL) && m.concurrently && !todo.is_empty() {
            return err(UNSUPPORTED, "cannot move non-shared relation to tablespace \"pg_global\"");
        }
        // (each table, its generations, and whether an index of it moved)
        let mut tables: Vec<(Table, super::tablespace::Gens, bool)> = vec![];
        for (table, index) in todo {
            let at = match tables.iter().position(|(t, ..)| t.name == table) {
                Some(k) => k,
                None => {
                    tables.push((need_table(v, &table)?, super::tablespace::Gens::load(v, &table)?, false));
                    tables.len() - 1
                }
            };
            let (t, gens, moved) = &mut tables[at];
            if let Some(to) = space {
                *moved |= super::tablespace::move_rel(t, Some(&index), to)?;
            }
            gens.bump_index(&index);
        }
        let mut writes = vec![];
        for (t, gens, moved) in &tables {
            if *moved {
                writes.push((schema_key(&t.name), Some(schema_json(t))));
            }
            writes.push(gens.write(&t.name));
        }
        Ok((writes, ()))
    })?;
    Ok(())
}

/// Whether table `t` has an index named `ix`: its primary key's, a unique constraint's, or one CREATE INDEX made.
fn has_index(t: &Table, ix: &str) -> bool {
    (t.has_key() && t.pkey_name() == ix)
        || t.uniques.iter().any(|u| u.name == ix)
        || t.indexes.iter().any(|x| x.name == ix)
        || t.ann.iter().any(|(n, _, _)| n == ix)
}

/// Whether table `t` may be clustered on its index `ix`, as Postgres's `check_index_is_clusterable` asks (and,
/// first, whether the index is there): CLUSTER ... USING and ALTER TABLE ... CLUSTER ON.
pub(super) fn clusterable(v: &View, t: &Table, ix: &str) -> Result<()> {
    let table = rel_part(&t.name);
    if !has_index(t, ix) {
        // (the index is looked for in the table's schema: one of another table's there is the wrong one)
        let schema = schema_part(&t.name);
        if schemas(v)?.iter().any(|o| schema_part(&o.name) == schema && has_index(o, ix)) {
            return err("42809", format!("\"{ix}\" is not an index for table \"{table}\""));
        }
        return err("42704", format!("index \"{ix}\" for table \"{table}\" does not exist"));
    }
    let x = t.indexes.iter().find(|x| x.name == ix);
    if t.ann.iter().any(|(n, _, _)| n == ix) || x.is_some_and(|x| x.gin) {
        return err(
            UNSUPPORTED,
            format!("cannot cluster on index \"{ix}\" because access method does not support clustering"),
        );
    }
    if x.is_some_and(|x| x.filter.is_some()) {
        return err(UNSUPPORTED, format!("cannot cluster on partial index \"{ix}\""));
    }
    if !hier::index_valid(v, t, ix) {
        return err(UNSUPPORTED, format!("cannot cluster on invalid index \"{ix}\""));
    }
    Ok(())
}

/// Whether the role running the statement owns table `name` (as stored): what CLUSTER without a table, and of a
/// partitioned table, asks of each table it would cluster (and silently skips the others).
fn owns(v: &View, name: &str) -> Result<bool> {
    let Some(who) = roles::checked() else { return Ok(true) };
    Ok(acl::rel(v, name)?.is_none_or(|o| who.has_privs_of(o.owner)))
}

/// Whether a CLUSTER has to run outside a transaction block, as Postgres's does when it clusters several tables:
/// without a table (every one clustered before), or of a partitioned table.
/// What a REINDEX that can't run inside a transaction block is called in the error, as Postgres has it: one of a
/// schema or the database (`REINDEX SCHEMA`, `REINDEX DATABASE`), or of a partitioned table or its index (each
/// partition's is rebuilt in a transaction of its own: `REINDEX TABLE`, `REINDEX INDEX`). None: it may.
pub(super) fn reindex_alone(db: &Db, branch: &str, m: &Maintain) -> Result<Option<&'static str>> {
    if m.verb != Verb::Reindex {
        return Ok(None);
    }
    let cat = Catalog::of(&db.core, branch)?;
    Ok(match m.targets.first() {
        Some(Target::Schema(_)) => Some("REINDEX SCHEMA"),
        Some(Target::Database) => Some("REINDEX DATABASE"),
        Some(Target::Rel(name, _)) => {
            matches!(find_rel(&cat, name), Ok(Some((_, r))) if r.kind == 'p').then_some("REINDEX TABLE")
        }
        Some(Target::Index(name)) => match cat.named(name)?.and_then(|o| cat.find(o)) {
            Some(Obj::Index(r, _)) if cat.rels[r].kind == 'p' => Some("REINDEX INDEX"),
            _ => None,
        },
        _ => None,
    })
}

pub(super) fn cluster_alone(db: &Db, branch: &str, m: &Maintain) -> Result<bool> {
    if m.verb != Verb::Cluster {
        return Ok(false);
    }
    match m.targets.first() {
        None | Some(Target::Everything) => Ok(true),
        Some(Target::Rel(name, _)) => {
            let cat = Catalog::of(&db.core, branch)?;
            Ok(matches!(find_rel(&cat, name), Ok(Some((_, r))) if r.kind == 'p'))
        }
        Some(_) => Ok(false),
    }
}

/// CLUSTER: marks the index (pg_index.indisclustered) and writes the table's rows again in its order. A table with a
/// primary key keeps its rows in the key's order, whatever index it is clustered on: only the mark changes. The rows
/// of a table without one (and of a materialized view) are in the order they were written, which CLUSTER changes, as
/// Postgres's does: they get new keys, in the index's order. Without a table, every table the role owns that was
/// clustered before is clustered again on its index; a partitioned table's partitions are clustered on their indexes
/// of the one named (those the role owns).
fn cluster(db: &Db, branch: &str, m: &Maintain) -> Result<Output> {
    let cat = Catalog::of(&db.core, branch)?;
    let target = m.targets.first().cloned().unwrap_or(Target::Everything);
    // a big table without a key: its rows in parts (see `rewrite`)
    if let Target::Rel(name, _) = &target
        && let Some((_, r)) = find_rel(&cat, name)?
        && matches!(r.kind, 'r' | 'm')
        && let Some(out) = super::rewrite::cluster_big(db, branch, &r.name, m.using.as_deref())?
    {
        return Ok(out);
    }
    db.batch_with::<(), SqlError>(branch, |v| {
        let mut writes = vec![];
        match &target {
            Target::Rel(name, _) => {
                let Some((_, r)) = find_rel(&cat, name)? else { return Ok((writes, ())) }; // a temporary table
                if !matches!(r.kind, 'r' | 'm' | 'p') {
                    return Err(wrong_kind(name, "a table or materialized view"));
                }
                let t = need_table(v, &r.name)?;
                let ix = cluster_index(&t, m.using.as_deref())?;
                clusterable(v, &t, &ix)?;
                if t.hier.part.is_some() {
                    for (leaf, lix) in hier::leaf_indexes(v, &t, &ix)? {
                        if owns(v, &leaf.name)? {
                            writes.extend(cluster_writes(v, &leaf, &lix)?);
                        }
                    }
                } else {
                    writes.extend(cluster_writes(v, &t, &ix)?);
                }
            }
            _ => {
                for t in schemas(v)? {
                    let Some(ix) = t.hier.clustered.clone().filter(|x| has_index(&t, x)) else { continue };
                    if t.hier.part.is_none() && owns(v, &t.name)? && clusterable(v, &t, &ix).is_ok() {
                        writes.extend(cluster_writes(v, &t, &ix)?);
                    }
                }
            }
        }
        Ok((writes, ()))
    })?;
    Ok(Output::tag("CLUSTER"))
}

/// The index CLUSTER orders table `t` by: USING's, or the one it was clustered on before.
pub(super) fn cluster_index(t: &Table, using: Option<&str>) -> Result<String> {
    match using {
        Some(ix) => Ok(ix.to_string()),
        None => match t.hier.clustered.clone().filter(|x| has_index(t, x)) {
            Some(ix) => Ok(ix),
            None => err("42704", format!("there is no previously clustered index for table \"{}\"", rel_part(&t.name))),
        },
    }
}

/// Where a row of `t` goes in the order of its index `ix`: the index's entry for it (which ends in the row's key, so
/// rows equal in the index keep their order), or a unique constraint's values and the key.
pub(super) fn cluster_key(t: &Table, ix: &str, vals: &[Value], id: &str) -> Option<String> {
    if let Some(x) = t.indexes.iter().find(|x| x.name == ix) {
        return btree::entry(t, x, vals, id);
    }
    let u = t.uniques.iter().find(|u| u.name == ix)?;
    let mut k = String::new();
    for &c in &u.cols {
        btree::enc(&mut k, &vals[position(t, c)?])?;
    }
    k.push('\u{1}');
    k.push_str(id);
    Some(k)
}

/// CLUSTER of one table on its index `ix` (checked): marks the index, and writes the rows of a table without a
/// primary key (or of a materialized view) again under new keys in the index's order, with their index rows. Rows
/// already in that order are left as they are.
pub(super) fn cluster_writes(v: &View, t: &Table, ix: &str) -> Result<Vec<(String, Option<String>)>> {
    let mut t = t.clone();
    let mut writes = vec![];
    if t.hier.clustered.as_deref() != Some(ix) {
        t.hier.clustered = Some(ix.to_string());
        writes.push((schema_key(&t.name), Some(schema_json(&t))));
    }
    // (a materialized view WITH NO DATA has no rows to order, and Postgres skips it)
    let unpopulated = t.matview.as_ref().is_some_and(|m| m["populated"] == false);
    if t.hier.part.is_some() || unpopulated || (t.has_key() && t.matview.is_none()) {
        return Ok(writes);
    }
    let prefix = format!("{}/", t.name);
    let mut rows: Vec<((bool, String), String, String)> = vec![];
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        let id = &k[prefix.len()..];
        let vals = decode_raw(&t, id, &raw, None)?;
        // (a row the index has no entry for, one holding JSON, goes last)
        let place = cluster_key(&t, ix, &vals, id).map_or((true, String::new()), |p| (false, p));
        rows.push((place, k.clone(), raw.to_string()));
    }
    if rows.windows(2).all(|w| w[0].0 <= w[1].0) {
        return Ok(writes);
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    // a materialized view's rows are keyed by their place; a table's get new made-up keys, in the order made
    let ids: Vec<String> = if t.matview.is_some() {
        (1..=rows.len()).map(|i| format!("{i:010}")).collect()
    } else {
        let mut ids: Vec<String> = rows.iter().map(|_| new_row_id()).collect();
        ids.sort();
        ids
    };
    let fresh: Vec<String> = ids.iter().map(|id| row_key(&t.name, id)).collect();
    let kept: HashSet<&str> = fresh.iter().map(String::as_str).collect();
    writes.extend(rows.iter().filter(|(_, k, _)| !kept.contains(k.as_str())).map(|(_, k, _)| (k.clone(), None)));
    writes.extend(fresh.iter().zip(&rows).map(|(k, (_, _, raw))| (k.clone(), Some(raw.clone()))));
    // the index rows, made again for the rows under their new keys
    writes.extend(index_rows(v, &t.name)?.into_iter().map(|k| (k, None)));
    enforce(v, &mut writes, &fresh, false)?;
    Ok(writes)
}

/// COMMENT ON: the object has to exist, and be what it's said to be; its comment is kept as a
/// row of the world (see `comments`), taken away by NULL or an empty string. A kind of object
/// whose comments aren't kept (a function, a role, a database, ...) says so.
pub(super) fn comment(db: &Db, branch: &str, c: &Comment) -> Result<Output> {
    let key = match &c.on {
        Commented::Other => None,
        // a role's comment is its owner's to give: a superuser's a superuser's, another's a role's with CREATEROLE and
        // the ADMIN option on it (as Postgres's check_object_ownership has it)
        // a publication's, which has to be there, and is its owner's to give
        Commented::Publication(name) => {
            let cat = Catalog::of(&db.core, branch)?;
            let Some(p) = cat.pubs.iter().find(|p| p.name == *name) else {
                return err("42704", format!("publication \"{name}\" does not exist"));
            };
            if let Some(who) = super::roles::checked()
                && !who.has_privs_of(p.owner)
            {
                return err("42501", format!("must be owner of publication {name}"));
            }
            Some(format!("publication/{name}"))
        }
        Commented::Role(name) => {
            let reg = super::roles::registry(&db.core)?;
            let Some(r) = reg.named(name) else { return err("42704", format!("role \"{name}\" does not exist")) };
            if let Some(who) = super::roles::checked() {
                let denied = |detail: String| {
                    Err(SqlError {
                        code: "42501",
                        message: "permission denied".into(),
                        info: Some(Box::new(Violation { detail: Some(detail), ..Violation::default() })),
                    })
                };
                if r.superuser {
                    return denied("The current user must have the SUPERUSER attribute.".into());
                }
                if !reg.by_oid(who.oid).is_some_and(|me| me.createrole) {
                    return denied("The current user must have the CREATEROLE attribute.".into());
                }
                if !reg.is_admin_of(who.oid, r.oid) {
                    return denied(format!("The current user must have the ADMIN option on role \"{}\".", r.name));
                }
            }
            Some(format!("role/{}", r.oid))
        }
        Commented::Foreign(kind, name) => {
            super::foreign::comment_target(&db.view_tx(branch)?, kind, name)?;
            Some(format!("{kind}/{name}"))
        }
        // an aggregate has to be there; its comment isn't kept, as a function's isn't (and nothing says so)
        Commented::Aggregate(key, shown) => {
            if db.core.view_tx(branch)?.get(key)?.is_none() {
                return err(NO_FUNCTION, format!("aggregate {shown} does not exist"));
            }
            return Ok(Output::tag("COMMENT"));
        }
        Commented::Schema(s) => {
            let cat = Catalog::of(&db.core, branch)?;
            if !cat.schemas.contains(s) && !matches!(s.as_str(), "public" | "pg_catalog" | "information_schema") {
                return err("3F000", format!("schema \"{s}\" does not exist"));
            }
            Some(format!("schema/{s}"))
        }
        Commented::Rel(kind, name) => {
            let cat = Catalog::of(&db.core, branch)?;
            match (cat.named(name)?.and_then(|o| cat.find(o)), *kind) {
                (Some(Obj::Index(r, x)), "index") => {
                    Some(format!("index/{}", qualify(schema_part(&cat.rels[r].name), &x.name)))
                }
                (Some(Obj::Rel(i)), "table") if matches!(cat.rels[i].kind, 'r' | 'p') => {
                    Some(format!("table/{}", cat.rels[i].name))
                }
                // (a foreign table's comment is kept as a table's)
                (Some(Obj::Rel(i)), "foreign table") if cat.rels[i].kind == 'f' => {
                    Some(format!("table/{}", cat.rels[i].name))
                }
                (Some(Obj::Rel(i)), "view") if cat.rels[i].kind == 'v' => Some(format!("view/{}", cat.rels[i].name)),
                (Some(Obj::Rel(i)), "materialized view") if cat.rels[i].kind == 'm' => {
                    Some(format!("materialized view/{}", cat.rels[i].name))
                }
                (Some(Obj::Rel(i)), "sequence") if cat.rels[i].kind == 'S' => {
                    Some(format!("sequence/{}", cat.rels[i].name))
                }
                (Some(_), kind) => {
                    return Err(wrong_kind(name, &format!("{} {kind}", if kind == "index" { "an" } else { "a" })));
                }
                (None, _) if has_temp(&text_rel(name)) => None,
                (None, _) => return Err(no_rel(name)),
            }
        }
        Commented::Column(table, column) => {
            let cat = Catalog::of(&db.core, branch)?;
            // (a composite type's attribute is a column of the type's relation)
            let stored = text_named(table, Kind::Type);
            let composite = (cat.enums.iter().position(|(n, _)| *n == stored))
                .filter(|&k| cat.composites.get(k).is_some_and(Option::is_some));
            if composite.is_some() && cat.named(table)?.is_none() {
                if cat.composite_attnum(&stored, column).is_none() {
                    return Err(no_column(column, &stored));
                }
                comments::set(
                    db,
                    branch,
                    &format!("column/{stored}/{column}"),
                    c.text.as_deref().filter(|t| !t.is_empty()),
                )?;
                return Ok(Output::tag("COMMENT"));
            }
            match find_rel(&cat, table)? {
                Some((_, r)) if !r.columns.iter().any(|c| c.0 == *column) => return Err(no_column(column, &r.name)),
                Some((_, r)) => Some(format!("column/{}/{column}", r.name)),
                None => None,
            }
        }
        Commented::Constraint(name, table) => {
            let cat = Catalog::of(&db.core, branch)?;
            match find_rel(&cat, table)? {
                Some((_, r)) if !r.constraints.iter().any(|c| c.name == *name) => {
                    return err(
                        "42704",
                        format!("constraint \"{name}\" for table \"{}\" does not exist", rel_part(&r.name)),
                    );
                }
                Some((_, r)) => Some(format!("constraint/{}/{name}", r.name)),
                None => None,
            }
        }
        // (its text isn't kept: rules have no comments here)
        Commented::Rule(name, table) => {
            let v = db.view_tx(branch)?;
            let view = view_def(&v, table)?.is_some();
            if !view && load(&v, table)?.is_none() {
                return err(NO_TABLE, format!("relation \"{}\" does not exist", rel_part(table)));
            }
            let on_select = view && name == "_RETURN";
            if !on_select && (name.contains(':') || v.get(&super::rules::key(table, name))?.is_none()) {
                return err("42704", format!("rule \"{name}\" for relation \"{}\" does not exist", rel_part(table)));
            }
            return Ok(Output::tag("COMMENT"));
        }
        // (its text isn't kept either: triggers have no comments here)
        Commented::Trigger(name, table) => {
            let v = db.view_tx(branch)?;
            let found = match load(&v, table)? {
                Some(t) => t.triggers.iter().any(|x| x.name == *name),
                // (a view's INSTEAD OF and statement triggers are kept as its rules: see `rules::view_triggers`)
                None if view_def(&v, table)?.is_some() => {
                    super::rules::view_triggers(&v, table)?.iter().any(|x| x.name == *name)
                }
                None => return err(NO_TABLE, format!("relation \"{}\" does not exist", rel_part(table))),
            };
            if !found {
                return err("42704", format!("trigger \"{name}\" for table \"{}\" does not exist", rel_part(table)));
            }
            return Ok(Output::tag("COMMENT"));
        }
        Commented::DomainConstraint(name, domain) => {
            let v = db.view_tx(branch)?;
            let j: Json = match v.get(&type_key(domain))? {
                Some(raw) => serde_json::from_str(&raw).unwrap_or_default(),
                None => return err("42704", format!("type \"{}\" does not exist", shown_type(domain))),
            };
            if j.get("domain").is_none() {
                return err("42809", format!("\"{}\" is not a domain", rel_part(domain)));
            }
            // its constraints: those CREATE DOMAIN named, and those ALTER DOMAIN ... ADD did
            let given = j["domain"]["constraint_names"].as_array().into_iter().flatten().filter_map(|n| n.as_str());
            let added = j["domain"]["named"].as_array().into_iter().flatten().filter_map(|n| n[0].as_str());
            // (one CREATE DOMAIN made without naming it is named `<domain>_check`, ...: see `domain::own_constraints`)
            let (_, own) = super::domain::own_constraints(&j["domain"], domain);
            if !given.chain(added).any(|n| n == name.as_str()) && !own.iter().any(|(n, _)| n == name) {
                return err("42704", format!("constraint \"{name}\" for domain {} does not exist", shown_type(domain)));
            }
            Some(format!("domain constraint/{domain}/{name}"))
        }
        Commented::LargeObject(oid) => {
            if !super::largeobj::exists_in(&db.view_tx(branch)?, *oid)? {
                return err("42704", format!("large object {oid} does not exist"));
            }
            Some(format!("large object/{oid}"))
        }
        Commented::Type(name) => {
            let v = db.view_tx(branch)?;
            if v.get(&type_key(name))?.is_none() {
                return err("42704", format!("type \"{}\" does not exist", shown_type(name)));
            }
            Some(format!("type/{name}"))
        }
        Commented::Func(name, types, procedure) => {
            let v = db.view_tx(branch)?;
            match functions::find_routine(&v, name, types.as_deref())? {
                Some(f) if f.procedure == *procedure => Some(format!("function/{}", f.sig())),
                Some(f) => {
                    let what = if *procedure { "a procedure" } else { "a function" };
                    return err("42809", format!("{} is not {what}", f.sig()));
                }
                None => {
                    let shown = match types {
                        Some(ts) => format!(
                            "function {}({}) does not exist",
                            written(name),
                            ts.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")
                        ),
                        None => format!("function {} does not exist", written(name)),
                    };
                    return err(NO_FUNCTION, shown);
                }
            }
        }
    };
    match key {
        Some(key) => {
            let text = c.text.as_deref().filter(|t| !t.is_empty());
            comments::set(db, branch, &key, text)?;
        }
        // (said only outside the regression suite's exact mode, where Postgres says nothing)
        None if pg_exact() => {}
        None => notice("NOTICE", "the comment isn't kept: Chronos keeps comments on tables, views, sequences, indexes, columns, constraints and schemas".into()),
    }
    Ok(Output::tag("COMMENT"))
}

/// LOCK TABLE, in a transaction. The database has no table locks: writers never wait for one. So
/// a mode that would keep writers out (or another locker) is taken as a lock only other
/// LOCK TABLE statements wait for (an advisory lock on the table's name, held to the end of the
/// transaction), which is what a migration tool that locks a table to run alone needs; and it
/// says so. The modes that only conflict with those (ACCESS SHARE, ROW SHARE, ROW EXCLUSIVE)
/// stop nothing that isn't already stopped.
pub(super) fn lock(db: &Db, branch: &str, l: &Lock) -> Result<Output> {
    let cat = Catalog::of(&db.core, branch)?;
    let mut names = vec![];
    // what pg_locks lists the transaction holding: each relation named, a view's relations (and theirs), and a
    // table's children unless ONLY
    let mut held: Vec<String> = vec![];
    for (t, only) in &l.tables {
        match find_rel(&cat, t)? {
            Some((_, r)) if r.kind == 'S' => {
                return err("42809", format!("cannot lock relation \"{}\"", rel_part(&r.name)));
            }
            Some((i, r)) => {
                names.push((t, r.name.clone()));
                let mut at = vec![(i, *only)];
                while let Some((k, only)) = at.pop() {
                    let r = &cat.rels[k];
                    if held.contains(&r.name) {
                        continue;
                    }
                    held.push(r.name.clone());
                    let below: Vec<&String> = match r.kind {
                        'v' => r.reads.iter().collect(),
                        _ if only => vec![],
                        _ => r.hier.children.iter().map(|(c, _)| c).collect(),
                    };
                    at.extend(
                        below
                            .into_iter()
                            .filter_map(|n| cat.rels.iter().position(|x| x.name == *n))
                            .map(|k| (k, false)),
                    );
                }
            }
            None => names.push((t, t.clone())),
        }
    }
    super::hold_relation_locks(&held, l.mode);
    if l.strong {
        for (written, stored) in &names {
            // two integers, apart from the numbers an application picks for pg_advisory_lock
            let key = [Value::Int(0x4C4F_434B), Value::Int(i64::from(advisory::hashtext(stored)))];
            let got = ctx_call(CtxFn::AdvisoryLock(true, l.nowait, false), &key)?;
            if got == Value::Bool(false) {
                return err("55P03", format!("could not obtain lock on relation \"{written}\""));
            }
        }
        // (Chronos's own warning: not under CHRONOS_PG_EXACT, which says only what Postgres says)
        if !pg_exact() {
            notice(
                "WARNING",
                "LOCK TABLE holds off other LOCK TABLE statements only: writers don't take table locks here".into(),
            );
        }
    }
    Ok(Output::tag("LOCK TABLE"))
}

impl Session {
    /// DISCARD: what a pool sends before it hands a connection to the next client. ALL puts the
    /// session back as it was on connecting: settings, prepared statements, LISTENs, temporary
    /// tables, advisory locks, sequence memory, and its world.
    pub(super) fn discard(&mut self, db: &Db, what: Discard, in_tx: bool) -> Result<Output> {
        let clear_sequences = |s: &mut Session| {
            let mut st = s.state.lock().unwrap();
            st.currval.clear();
            st.last = None;
            st.last_seq = None;
        };
        match what {
            Discard::Plans => return Ok(Output::tag("DISCARD PLANS")),
            Discard::Sequences => {
                clear_sequences(self);
                return Ok(Output::tag("DISCARD SEQUENCES"));
            }
            Discard::Temp if in_tx => {
                return err(UNSUPPORTED, "DISCARD TEMP inside a transaction isn't supported yet");
            }
            Discard::Temp => {
                self.temp = None;
                return Ok(Output::tag("DISCARD TEMP"));
            }
            Discard::All if in_tx => return err("25001", "DISCARD ALL cannot run inside a transaction block"),
            Discard::All => {}
        }
        {
            let mut st = self.state.lock().unwrap();
            st.settings.clear();
            st.local.clear();
        }
        clear_sequences(self);
        (self.search_path, self.timeout, self.ef) = (None, None, 0);
        (self.zone, self.zone_after) = (None, None);
        // SET SESSION AUTHORIZATION DEFAULT and RESET ROLE, as Postgres's DISCARD ALL does
        (self.ident.auth, self.ident.role, self.ident_after) = (None, None, None);
        self.use_zone();
        self.prepared = Default::default();
        self.cursors.clear();
        {
            let mut ls = db.core.listeners.lock().unwrap();
            let before = ls.len();
            ls.retain(|(_, _, i)| !std::ptr::eq(i.as_ptr(), Arc::as_ptr(&self.inbox)));
            self.state.lock().unwrap().channels -= before - ls.len();
        }
        self.temp = None;
        if std::mem::take(&mut self.state.lock().unwrap().advisory) {
            advisory::release_all(self.id, false, db.core.fork_clock(), || db.core.oldest_tx());
        }
        self.branch = self.login.clone();
        Ok(Output::tag("DISCARD ALL"))
    }
}
