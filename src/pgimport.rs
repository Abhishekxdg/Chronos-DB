//! `chronos import <folder> postgres://...`: a live Postgres database, moved in one command.
//!
//!   read the catalog ─> schemas, types (enums, domains, composite and range types, in the order
//!                       they depend on one another), sequences, functions, tables (columns, defaults,
//!                       identity, keys, UNIQUE, CHECK, exclusion constraints, partitions and
//!                       inheritance), then the rows of each table (COPY ... TO STDOUT, text), then
//!                       foreign keys, indexes, views, sequences set to where they were, triggers,
//!                       rules and comments
//!   anything Chronos can't take (an extension, row-level security, a type it doesn't have, a
//!   statement it refuses) goes in a report instead of stopping the rest, and so does anything it
//!   takes in part (an index method it has no use for, a sort order)
//!
//! Rows move in COPY's text format: Chronos reads each value as its column's type, as any
//! INSERT does, so a type whose binary layout it doesn't read still comes over, and floats come
//! over exactly (Postgres 12 on prints them so they read back the same).
//!
//! `--dry-run` reads the catalog and runs every definition on an empty in-memory database: the
//! same report, and how many rows each table has (Postgres's estimate), with nothing written.

use crate::pgclient::{Client, Rows, Url, parse};
use chronos::Db;
use chronos::attach::{self, Attached};
use chronos::sql::{Session, SqlError};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Postgres's own schemas, and those of extensions' objects, aren't the user's.
const USER: &str = "n.nspname !~ '^pg_' and n.nspname <> 'information_schema'";
/// An object an extension made (PostGIS's spatial_ref_sys, pgvector's functions) isn't the user's.
const OWN: &str = "not exists (select 1 from pg_depend e where e.objid = {oid} and e.deptype = 'e')";

/// A name as SQL writes it: quoted, so any name comes over as it is.
fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Schema and name as Chronos stores and SQL writes them: `"app"."orders"`, `"orders"` in public.
fn qualified(schema: &str, name: &str) -> String {
    match schema {
        "public" => q(name),
        s => format!("{}.{}", q(s), q(name)),
    }
}

/// A function's definition as `pg_get_functiondef` writes it, with CREATE for its CREATE OR REPLACE.
fn not_replacing(def: &str) -> String {
    match def.strip_prefix("CREATE OR REPLACE ") {
        Some(rest) => format!("CREATE {rest}"),
        None => def.to_string(),
    }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A sequence's name as `nextval` and `setval` take it: `orders_id_seq`, `app.orders_id_seq`.
fn seq_name(schema: &str, name: &str) -> String {
    match schema {
        "public" => name.to_string(),
        s => format!("{s}.{name}"),
    }
}

/// SQL (run on Postgres) for the name of the relation in alias `c` with its namespace in `n`, as `qualified` writes it.
fn pg_qualified(n: &str, c: &str) -> String {
    format!(
        "case when {n}.nspname = 'public' then quote_ident({c}.relname) else quote_ident({n}.nspname) || '.' || quote_ident({c}.relname) end"
    )
}

struct Table {
    oid: String,
    name: String, // qualified, as SQL writes it
    rows: i64,    // Postgres's estimate
    /// columns COPY sends (not generated ones)
    copied: Vec<String>,
    create: String,
    /// what makes it after it: its partitions (and their own constraints), as (what, SQL)
    parts: Vec<(String, String)>,
    /// the table without its partitioning or inheritance, for when Chronos refuses either
    flat: Option<String>,
    /// other tables inherit from it, so its own rows are `FROM ONLY` it
    only: bool,
    /// row-level security is on and the role imported as doesn't bypass it (not a superuser, BYPASSRLS, or the owner
    /// of a table that doesn't FORCE it): Postgres would hand over only the rows its policies show that role, so none
    /// are copied (see `Report::hidden`)
    hidden: bool,
}

/// What an import did, or would do.
#[derive(Default)]
pub struct Report {
    pub done: Vec<String>,
    /// objects brought over that aren't counted with the keys and indexes: types, functions,
    /// triggers, rules, partitions
    pub carried: Vec<String>,
    pub skipped: Vec<String>,
    /// objects that came over in part: the index method it has no use for, a sort order it doesn't keep
    pub partial: Vec<String>,
    /// unique indexes that didn't come over: the table now accepts rows Postgres refused
    pub unenforced: Vec<String>,
    /// tables whose rows weren't copied: row-level security would have shown the role only some of them (see
    /// `Table::hidden`)
    pub hidden: Vec<String>,
    /// tables Chronos refused to make (a type it doesn't have, say): they and their rows aren't here
    pub missing: Vec<String>,
    /// how many tables came over, of how many were read, and their rows
    pub imported: usize,
    pub total: usize,
    pub rows: u64,
}

/// How far an import is, for whoever watches it (Studio polls it), and its stop switch.
#[derive(Default)]
pub struct Progress {
    /// the tables found to bring over
    pub tables: AtomicUsize,
    /// the tables made (with their rows), or given up on
    pub done: AtomicUsize,
    /// rows copied so far, the table being copied's too
    pub rows: AtomicU64,
    /// what it is doing now
    pub now: Mutex<String>,
    /// each table made: its name and rows (Postgres's estimate in a dry run)
    pub per_table: Mutex<Vec<(String, u64)>>,
    /// set to stop: it stops between tables, and a COPY under way stops (that table stays empty)
    pub stop: AtomicBool,
}

impl Progress {
    fn now(&self, what: impl Into<String>) {
        *self.now.lock().unwrap_or_else(|p| p.into_inner()) = what.into();
    }

    fn stopped(&self) -> Result<(), String> {
        match self.stop.load(Ordering::Relaxed) {
            true => Err(CANCELED.into()),
            false => Ok(()),
        }
    }
}

/// The error an import that was stopped (see `Progress::stop`) ends with.
pub const CANCELED: &str = "the import was canceled";

/// Which of Postgres's schemas and tables an import brings over: `include`'s schemas (all their tables) and
/// `schema.table`s; everything when it is empty. SQL (run on Postgres) restricting the namespace `n`, and the
/// relation `c` in it.
fn included(include: &[String]) -> (String, String, String) {
    if include.is_empty() {
        return (String::new(), String::new(), String::new());
    }
    let (tables, schemas): (Vec<&String>, Vec<&String>) = include.iter().partition(|i| i.contains('.'));
    let list = |v: &[&str]| v.iter().map(|x| lit(x)).collect::<Vec<_>>().join(", ");
    let whole: Vec<&str> = schemas.iter().map(|s| s.as_str()).collect();
    let mut spaces: Vec<&str> = whole.clone();
    spaces.extend(tables.iter().map(|t| t.split_once('.').map_or(t.as_str(), |(s, _)| s)));
    let named: Vec<&str> = tables.iter().map(|t| t.as_str()).collect();
    let ns = format!(" and n.nspname in ({})", list(&spaces));
    let schemas = if whole.is_empty() { "null".into() } else { list(&whole) };
    let rel = format!(
        " and (n.nspname in ({schemas}) or n.nspname || '.' || c.relname in ({}))",
        if named.is_empty() { "null".into() } else { list(&named) }
    );
    (ns, rel, schemas)
}

struct Import<'a> {
    pg: Rc<RefCell<Client>>,
    db: &'a Db,
    s: Session,
    report: Report,
    dry: bool,
    /// SQL restricting the namespace `n` to the user's schemas (and to those included: see `included`)
    user: String,
    /// and the relation `c` to the tables included
    rel: String,
    /// the schemas included whole, as a list for SQL's `in` (`null` when none)
    whole: String,
    progress: Arc<Progress>,
    /// functions Chronos refused before the tables were there (one that reads a table), tried again after
    later: Vec<(String, String, String)>,
    /// the oids of the partitions of the tables read (their indexes and triggers are looked for too)
    part_oids: Vec<String>,
    /// the sequences that could not be made (one by that name already here): not set to Postgres's values
    seqs_refused: BTreeSet<(String, String)>,
    /// for `chronos attach`: triggers and rules stay in Postgres (they run there when a merge is written)
    attach: bool,
    /// the schemas Postgres keeps extensions Chronos has built in (Supabase: `extensions`), and the names of those
    /// extensions' types, functions and operator classes: written there as `extensions.vector(64)`, made here as
    /// `vector(64)` (see `unqualify`)
    ext_schemas: Vec<String>,
    ext_names: std::collections::HashSet<String>,
}

impl Report {
    /// The world an import goes into is named `world`, not after the Postgres database: said, as
    /// `current_database()` shows it.
    fn database_name(&mut self, source: &str, world: &str) {
        if source != world && !source.is_empty() {
            self.partial.push(format!(
                "database name: Postgres's {source} is the world {world} here (current_database() says {world})"
            ));
        }
    }
}

/// A definition to run and what to call it in the report.
type Item = (String, String);

/// Why text a database of collation `collate` (provider `provider`: `c` libc, `i` ICU, `b` builtin; ICU's and
/// builtin's `locale`) sorts may sort otherwise here, where text sorts by bytes unless given a collation: None
/// when it sorts by bytes (or code points, the same order in UTF-8) there too. `known`: whether Chronos has a
/// collation of that name, the only kind the warning offers.
fn collation_warning(
    collate: &str,
    provider: &str,
    locale: Option<&str>,
    known: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let bytes = |l: &str| ["c", "posix", "c.utf8", "c.utf-8", "ucs_basic"].contains(&l.to_ascii_lowercase().as_str());
    let (shown, icu) = match provider {
        "b" => return None, // builtin: C, C.UTF-8 or PG_C_UTF8, all by code point
        "i" => {
            let l = locale.filter(|l| !l.is_empty()).unwrap_or("und");
            (format!("the ICU locale {l}"), format!("{l}-x-icu"))
        }
        _ if bytes(collate) => return None,
        _ => {
            let lang = collate.split(['.', '@']).next().unwrap_or(collate).replace('_', "-");
            (format!("{collate} (libc)"), format!("{lang}-x-icu"))
        }
    };
    let said = format!(
        "database collation: Postgres sorts this database's text by {shown}; here text sorts by bytes, as COLLATE \"C\" \
         does, so ORDER BY, min/max and comparisons of text can give other results (\"Zara\" before \"arjun\")"
    );
    // the language's own collation when the locale's isn't one (a region, script or keywords it hasn't)
    let lang = icu.split('-').next().map(|l| format!("{l}-x-icu"));
    let fix = match [Some(icu.clone()), lang].into_iter().flatten().find(|c| known(c)) {
        Some(c) => format!(
            ". Give the columns whose order matters a collation (ALTER TABLE ... ALTER COLUMN name TYPE text COLLATE \"{c}\"), \
             or write ORDER BY name COLLATE \"{c}\""
        ),
        None => format!(
            ". Chronos has no collation for that locale ({icu} isn't one): \"unicode\" orders text by Unicode's \
             language-neutral rules (COLLATE \"unicode\")"
        ),
    };
    Some(said + &fix)
}

/// How an index came over short of Postgres's: what pg_get_indexdef says it has that Chronos keeps
/// no part of (it only finds rows by it, in one order).
fn index_losses(def: &str) -> Vec<String> {
    let mut out = vec![];
    if let Some(m) = def.split(" USING ").nth(1).and_then(|r| r.split([' ', '(']).next())
        && ["hash", "brin"].contains(&m)
    {
        out.push(format!("a {m} index came over as a btree (it finds the same rows)"));
    }
    let key = def.split_once(" (").map_or("", |(_, r)| r);
    let words: Vec<&str> = key.split([' ', ',', '(', ')']).collect();
    if words.iter().any(|w| *w == "DESC" || *w == "NULLS") {
        out.push("DESC and NULLS FIRST/LAST aren't kept: it is one ascending order".into());
    }
    if def.contains(") INCLUDE (") {
        out.push("INCLUDE columns aren't kept".into());
    }
    // (pgvector's are kept: they pick the distance an hnsw index's graph is for; pg_trgm's too: the index is one
    // of trigrams)
    let kept = |w: &str| {
        let w = w.rsplit('.').next().unwrap_or(w);
        ["vector_cosine_ops", "vector_l2_ops", "vector_ip_ops", "gin_trgm_ops", "gist_trgm_ops"].contains(&w)
    };
    if let Some(c) = words.iter().find(|w| w.ends_with("_ops") && !w.starts_with('"') && !kept(w)) {
        out.push(format!("operator class {c} isn't kept (the index finds the same rows without it)"));
    }
    if def.contains(" WITH (") {
        out.push("storage parameters aren't kept".into());
    }
    out
}

impl Import<'_> {
    /// The source database's collation, when its text sorts otherwise than by bytes: said first, since it changes
    /// what queries return without any error (see `collation_warning`).
    fn source_collation(&mut self) {
        // (to_jsonb, as datlocprovider is Postgres 15's and the ICU locale's column is daticulocale in 15 and 16,
        // datlocale from 17)
        let rows = self.pg_or_none(
            "select d.datcollate, coalesce(to_jsonb(d)->>'datlocprovider', 'c'), \
             coalesce(to_jsonb(d)->>'datlocale', to_jsonb(d)->>'daticulocale') \
             from pg_database d where d.datname = current_database()",
        );
        let Some(r) = rows.first() else { return };
        let v = |i: usize| r.get(i).cloned().flatten();
        // (a collation Chronos has is one a query may name)
        let (s, db) = (std::cell::RefCell::new(&mut self.s), self.db);
        let known = |c: &str| {
            let q = format!("select 'a' collate {}", chronos::sql::quote_ident(c));
            s.borrow_mut().run(db, &q, &[]).is_ok()
        };
        if let Some(w) =
            collation_warning(&v(0).unwrap_or_default(), &v(1).unwrap_or_default(), v(2).as_deref(), &known)
        {
            self.report.partial.insert(0, w);
        }
    }

    fn pg(&self, sql: &str) -> Result<Rows, String> {
        let mut pg = self.pg.borrow_mut();
        if !self.attach {
            return pg.query(sql).map_err(|e| format!("reading Postgres: {e}"));
        }
        // attach reads in one transaction, which a failed query would end: each under a savepoint of its own (a
        // query an older server refuses is tried another way)
        pg.query("savepoint chronos_read").map_err(|e| format!("reading Postgres: {e}"))?;
        let out = pg.query(sql).map_err(|e| format!("reading Postgres: {e}"));
        let end = if out.is_ok() { "release savepoint chronos_read" } else { "rollback to savepoint chronos_read" };
        pg.query(end).map_err(|e| format!("reading Postgres: {e}"))?;
        out
    }

    /// `pg`, for what an older server has no catalog for: nothing then, without a word.
    fn pg_or_none(&self, sql: &str) -> Rows {
        self.pg(sql).unwrap_or_default()
    }

    /// Runs `sql` in Chronos: why not, if it refuses.
    fn attempt(&mut self, sql: &str) -> Result<(), String> {
        let sql = unqualify(sql, &self.ext_schemas, &self.ext_names);
        self.s.run(self.db, &sql, &[]).map(|_| ()).map_err(|e| e.message)
    }

    /// SQL keeping the function `p` (in the namespace `n`) to those of an included schema, or used by an included
    /// table's defaults, constraints, triggers or indexes, or by an included view's query; nothing when everything is
    /// included.
    // ponytail: a function another one calls (in its body, which pg_depend doesn't record) isn't followed
    fn fn_rel(&self, p: &str) -> String {
        if self.rel.is_empty() {
            return String::new();
        }
        let tables = format!(
            "select c.oid from pg_class c join pg_namespace n on n.oid = c.relnamespace where true{}",
            self.rel
        );
        format!(
            " and (n.nspname in ({}) or exists (select 1 from pg_depend dd where dd.refclassid = 'pg_proc'::regclass \
             and dd.refobjid = {p}.oid and (dd.classid = 'pg_attrdef'::regclass \
             and dd.objid in (select oid from pg_attrdef where adrelid in ({tables})) \
             or dd.classid = 'pg_constraint'::regclass and dd.objid in (select oid from pg_constraint where conrelid in ({tables})) \
             or dd.classid = 'pg_trigger'::regclass and dd.objid in (select oid from pg_trigger where tgrelid in ({tables})) \
             or dd.classid = 'pg_class'::regclass and dd.objid in (select indexrelid from pg_index where indrelid in ({tables})) \
             or dd.classid = 'pg_rewrite'::regclass and dd.objid in (select oid from pg_rewrite where ev_class in ({tables})))))",
            self.whole
        )
    }

    /// SQL keeping the sequence `s` (in the namespace `n`) to the included relations': in an included schema or
    /// named, owned by an included table (a serial's, an identity's) or read by one's column defaults; nothing when
    /// everything is included.
    fn seq_rel(&self, s: &str) -> String {
        if self.rel.is_empty() {
            return String::new();
        }
        let tables = format!(
            "select c.oid from pg_class c join pg_namespace n on n.oid = c.relnamespace where true{}",
            self.rel
        );
        format!(
            " and (true{} or exists (select 1 from pg_depend dd where dd.classid = 'pg_class'::regclass \
             and dd.objid = {s}.oid and dd.refclassid = 'pg_class'::regclass and dd.deptype in ('a', 'i') \
             and dd.refobjid in ({tables})) or exists (select 1 from pg_depend dd join pg_attrdef ad on ad.oid = dd.objid \
             where dd.classid = 'pg_attrdef'::regclass and dd.refclassid = 'pg_class'::regclass and dd.refobjid = {s}.oid \
             and ad.adrelid in ({tables})))",
            self.rel.replace("c.relname", &format!("{s}.relname"))
        )
    }

    /// Runs `sql` in Chronos; a refusal is reported (as `what`), not fatal.
    fn run(&mut self, what: &str, sql: &str) -> bool {
        match self.attempt(sql) {
            Ok(()) => true,
            Err(e) => {
                self.report.skipped.push(format!("{what}: {e}"));
                false
            }
        }
    }

    /// Runs each of `items` until none more runs: types made over other types, functions that call
    /// functions, whatever order Postgres lists them in. What never ran comes back with its last error.
    fn settle(&mut self, mut items: Vec<Item>) -> Vec<(String, String, String)> {
        let mut failed: Vec<(String, String, String)> = vec![];
        loop {
            failed.clear();
            let before = items.len();
            for (what, sql) in std::mem::take(&mut items) {
                match self.attempt(&sql) {
                    Ok(()) => self.report.carried.push(what),
                    Err(e) => failed.push((what, sql, e)),
                }
            }
            if failed.is_empty() || failed.len() == before {
                return std::mem::take(&mut failed);
            }
            items = failed.iter().map(|(w, s, _)| (w.clone(), s.clone())).collect();
        }
    }

    /// The types the user made: enums, domains (with their constraints, default and NOT NULL), composite types
    /// and ranges, each after the types it is made of.
    fn types(&mut self) -> Result<(), String> {
        let user = self.user.clone();
        let oid = |c: &str| OWN.replace("{oid}", c);
        let mut items: Vec<Item> = vec![];
        // enum types, with their labels in order
        let mut enums: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        let enum_sql = format!(
            "select n.nspname, t.typname, e.enumlabel from pg_type t join pg_enum e on e.enumtypid = t.oid \
             join pg_namespace n on n.oid = t.typnamespace where {user} and {} order by 1, 2, e.enumsortorder",
            oid("t.oid")
        );
        for r in self.pg(&enum_sql)? {
            let [Some(n), Some(t), Some(l)] = [&r[0], &r[1], &r[2]] else { continue };
            enums.entry((n.clone(), t.clone())).or_default().push(lit(l));
        }
        for ((n, t), labels) in enums {
            items.push((
                format!("type {n}.{t}"),
                format!("create type {} as enum ({})", qualified(&n, &t), labels.join(", ")),
            ));
        }
        // domains: the base type, DEFAULT, NOT NULL and the CHECKs by name
        let mut checks: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in self.pg("select c.contypid, c.conname, pg_get_constraintdef(c.oid) from pg_constraint c \
                          where c.contypid <> 0 and c.contype = 'c' order by c.contypid, c.oid")?
        {
            let [Some(t), Some(n), Some(d)] = [&r[0], &r[1], &r[2]] else { continue };
            checks.entry(t.clone()).or_default().push(format!("constraint {} {d}", q(n)));
        }
        let domains = self.pg(&format!(
            "select n.nspname, t.typname, format_type(t.typbasetype, t.typtypmod), t.typnotnull, t.typdefault, t.oid \
             from pg_type t join pg_namespace n on n.oid = t.typnamespace \
             where t.typtype = 'd' and {user} and {} order by t.oid",
            oid("t.oid")
        ))?;
        for r in domains {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let mut sql = format!("create domain {} as {}", qualified(&v(0), &v(1)), v(2));
            if let Some(d) = &r[4] {
                sql.push_str(&format!(" default {d}"));
            }
            if v(3) == "t" {
                sql.push_str(" not null");
            }
            for c in checks.get(&v(5)).into_iter().flatten() {
                sql.push_str(&format!(" {c}"));
            }
            items.push((format!("domain {}.{}", v(0), v(1)), sql));
        }
        // composite types (a table's row type is not one: relkind c is what CREATE TYPE ... AS makes)
        let mut fields: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        let comp_sql = format!(
            "select n.nspname, t.typname, a.attname, format_type(a.atttypid, a.atttypmod) from pg_type t \
             join pg_class c on c.oid = t.typrelid and c.relkind = 'c' join pg_namespace n on n.oid = t.typnamespace \
             left join pg_attribute a on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped \
             where {user} and {} order by n.nspname, t.typname, a.attnum",
            oid("t.oid")
        );
        for r in self.pg(&comp_sql)? {
            let [Some(n), Some(t)] = [&r[0], &r[1]] else { continue };
            let f = fields.entry((n.clone(), t.clone())).or_default();
            if let (Some(a), Some(ty)) = (&r[2], &r[3]) {
                f.push(format!("{} {ty}", q(a)));
            }
        }
        for ((n, t), fs) in fields {
            items.push((format!("type {n}.{t}"), format!("create type {} as ({})", qualified(&n, &t), fs.join(", "))));
        }
        // ranges (Chronos keeps the subtype; subtype_diff, collation and canonical only tune Postgres's index)
        let ranges = self.pg(&format!(
            "select n.nspname, t.typname, format_type(r.rngsubtype, -1) from pg_range r \
             join pg_type t on t.oid = r.rngtypid join pg_namespace n on n.oid = t.typnamespace \
             where {user} and {} order by 1, 2",
            oid("t.oid")
        ))?;
        for r in ranges {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            items.push((
                format!("type {}.{}", v(0), v(1)),
                format!("create type {} as range (subtype = {})", qualified(&v(0), &v(1)), v(2)),
            ));
        }
        // base types are made with functions in C
        let bases = self.pg(&format!(
            "select n.nspname, t.typname from pg_type t join pg_namespace n on n.oid = t.typnamespace \
             where t.typtype = 'b' and t.typcategory <> 'A' and {user} and {} order by 1, 2",
            oid("t.oid")
        ))?;
        for r in bases {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            self.report.skipped.push(format!(
                "type {}.{}: a base type (made with C functions) isn't in Chronos",
                v(0),
                v(1)
            ));
        }
        for (what, _, e) in self.settle(items) {
            self.report.skipped.push(format!("{what}: {e}"));
        }
        Ok(())
    }

    /// The functions and procedures: made before the tables (a default or a check may call one), and again
    /// after them and the views (one that reads a table can't be made before it).
    fn functions(&mut self) -> Result<(), String> {
        let user = self.user.clone();
        let of_rel = self.fn_rel("p");
        let rows = self.pg(&format!(
            "select n.nspname, p.proname, pg_get_function_identity_arguments(p.oid), l.lanname, p.prokind, \
             case when p.prokind in ('f', 'p') then pg_get_functiondef(p.oid) end \
             from pg_proc p join pg_namespace n on n.oid = p.pronamespace join pg_language l on l.oid = p.prolang \
             where {user}{of_rel} and {} order by n.nspname, p.proname, p.oid",
            OWN.replace("{oid}", "p.oid")
        ))?;
        let mut items: Vec<Item> = vec![];
        for r in rows {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let label = format!("{}.{}({})", v(0), v(1), v(2));
            match (v(4).as_str(), v(3).as_str()) {
                // (aggregates are made below, from pg_aggregate)
                ("a", _) => {}
                ("w", _) => self.report.skipped.push(format!("window function {label}: not imported")),
                // (CREATE, not Postgres's CREATE OR REPLACE: one of that name already here is said, never replaced)
                (_, "sql" | "plpgsql") => match &r[5] {
                    Some(def) => items.push((format!("function {label}"), not_replacing(def))),
                    None => self.report.skipped.push(format!("function {label}: its definition can't be read")),
                },
                (_, lang) => self.report.skipped.push(format!(
                    "function {label}: language {lang} isn't in Chronos (SQL and PL/pgSQL functions are)"
                )),
            }
        }
        // aggregates: a plain one is its state function, state type, final function and initial value
        let aggs = self.pg_or_none(&format!(
            "select n.nspname, p.proname, pg_get_function_identity_arguments(p.oid), a.aggtransfn::regproc::text, \
             format_type(a.aggtranstype, -1), case when a.aggfinalfn <> 0 then a.aggfinalfn::regproc::text end, \
             a.agginitval, a.aggkind from pg_aggregate a join pg_proc p on p.oid = a.aggfnoid \
             join pg_namespace n on n.oid = p.pronamespace where {user}{of_rel} and {} order by n.nspname, p.proname, p.oid",
            OWN.replace("{oid}", "p.oid")
        ));
        for r in aggs {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let label = format!("{}.{}({})", v(0), v(1), v(2));
            if v(7) != "n" {
                self.report
                    .skipped
                    .push(format!("aggregate {label}: an ordered-set or hypothetical-set aggregate isn't imported"));
                continue;
            }
            let mut opts = format!("sfunc = {}, stype = {}", v(3), v(4));
            if let Some(f) = &r[5] {
                opts.push_str(&format!(", finalfunc = {f}"));
            }
            if let Some(i) = &r[6] {
                opts.push_str(&format!(", initcond = {}", lit(i)));
            }
            let name = qualified(&v(0), &v(1));
            items.push((format!("aggregate {label}"), format!("create aggregate {name}({}) ({opts})", v(2))));
        }
        self.later = self.settle(items);
        Ok(())
    }

    fn catalog(&mut self) -> Result<Vec<Table>, String> {
        let user = self.user.clone();
        let oid = |c: &str| OWN.replace("{oid}", c);
        let mut built_in: Vec<String> = vec![];
        for r in self.pg("select extname from pg_extension where extname <> 'plpgsql' order by 1")? {
            let e = r[0].clone().unwrap_or_default();
            // one Chronos has built in (pgcrypto, uuid-ossp, citext, ...): made here as Postgres had it
            if ["vector", "postgis"].contains(&e.as_str())
                || self.attempt(&format!("create extension if not exists {}", q(&e))).is_ok()
            {
                self.report.carried.push(format!("extension {e} (built into Chronos)"));
                built_in.push(e);
            } else {
                self.report.skipped.push(format!(
                    "extension {e}: not in Chronos (the ones it has are built in: pgvector, PostGIS points, pgcrypto, \
                     uuid-ossp, citext, hstore, ltree, pg_trgm and others)"
                ));
            }
        }
        // where Postgres keeps them, and what of theirs a definition may name with that schema (Supabase puts every
        // extension in `extensions`): here they have no schema, so the name goes without it
        if !built_in.is_empty() {
            let names = built_in.iter().map(|e| lit(e)).collect::<Vec<_>>().join(", ");
            let ext = format!("(select oid from pg_extension where extname in ({names}))");
            self.ext_schemas = self
                .pg_or_none(&format!(
                    "select distinct n.nspname from pg_extension e join pg_namespace n on n.oid = e.extnamespace \
                     where e.oid in {ext}"
                ))
                .into_iter()
                .filter_map(|r| r[0].clone())
                .filter(|n| n != "public" && n != "pg_catalog")
                .collect();
            let owned = |catalog: &str, name: &str| {
                format!(
                    "select x.{name} from pg_depend d join {catalog} x on x.oid = d.objid \
                     where d.classid = '{catalog}'::regclass and d.deptype = 'e' and d.refobjid in {ext}"
                )
            };
            let sql = [
                owned("pg_type", "typname"),
                owned("pg_proc", "proname"),
                owned("pg_opclass", "opcname"),
                owned("pg_opfamily", "opfname"),
            ]
            .join(" union ");
            self.ext_names = self.pg_or_none(&sql).into_iter().filter_map(|r| r[0].clone()).collect();
        }
        // whether the role imported as reads past row-level security: else a table with it on would come over with
        // only the rows its policies show that role (see `Table::hidden`)
        let bypass = self
            .pg_or_none("select rolsuper or rolbypassrls from pg_roles where rolname = current_user")
            .first()
            .and_then(|r| r[0].clone())
            .is_some_and(|b| b == "t");
        for r in
            self.pg(&format!("select n.nspname from pg_namespace n where {user} and n.nspname <> 'public' order by 1"))?
        {
            let n = r[0].clone().unwrap_or_default();
            self.run(&format!("schema {n}"), &format!("create schema if not exists {}", q(&n)));
        }
        self.types()?;
        // identity columns' sequences are made with their columns, with the options Postgres has for them
        let mut identity_seqs: HashMap<(String, String), String> = HashMap::new();
        let mut identity_names: BTreeSet<(String, String)> = BTreeSet::new();
        let id_sql = "select d.refobjid, d.refobjsubid, s.seqstart, s.seqincrement, s.seqmax, s.seqmin, s.seqcache, \
                      s.seqcycle, n.nspname, c.relname from pg_depend d join pg_sequence s on s.seqrelid = d.objid \
                      join pg_class c on c.oid = d.objid join pg_namespace n on n.oid = c.relnamespace \
                      where d.classid = 'pg_class'::regclass and d.refclassid = 'pg_class'::regclass and d.deptype = 'i'";
        for r in self.pg_or_none(id_sql) {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let cycle = if v(7) == "t" { "cycle" } else { "no cycle" };
            let opts = format!(
                "(start with {} increment by {} minvalue {} maxvalue {} {cycle} cache {})",
                v(2),
                v(3),
                v(5),
                v(4),
                v(6)
            );
            identity_seqs.insert((v(0), v(1)), opts);
            identity_names.insert((v(8), v(9)));
        }
        // sequences first: columns' defaults take their next values
        let seqs = self.pg(&format!(
            "select schemaname, sequencename, data_type::text, increment_by, min_value, max_value, start_value, cycle, \
             cache_size from pg_sequences n0 join pg_namespace n on n.nspname = n0.schemaname \
             join pg_class s on s.relnamespace = n.oid and s.relname = n0.sequencename where {user}{} order by 1, 2",
            self.seq_rel("s")
        ))?;
        for r in &seqs {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            if identity_names.contains(&(v(0), v(1))) {
                continue;
            }
            let cycle = if v(7) == "t" { "cycle" } else { "no cycle" };
            let cache = if v(8).is_empty() || v(8) == "1" { String::new() } else { format!(" cache {}", v(8)) };
            let sql = format!(
                "create sequence {} as {} increment by {} minvalue {} maxvalue {} start with {} {cycle}{cache}",
                qualified(&v(0), &v(1)),
                v(2),
                v(3),
                v(4),
                v(5),
                v(6)
            );
            if !self.run(&format!("sequence {}.{}", v(0), v(1)), &sql) {
                self.seqs_refused.insert((v(0), v(1))); // (one already here, say: left as it is, never set)
            }
        }
        self.functions()?;
        let tables = self.pg(&format!(
            "select c.oid, n.nspname, c.relname, c.relrowsecurity, greatest(c.reltuples, 0)::bigint, c.relkind, \
             case when c.relkind = 'p' then pg_get_partkeydef(c.oid) end, \
             exists (select 1 from pg_inherits h where h.inhparent = c.oid), \
             c.relforcerowsecurity, pg_get_userbyid(c.relowner) = current_user from pg_class c \
             join pg_namespace n on n.oid = c.relnamespace where c.relkind in ('r', 'p') and not c.relispartition \
             and {user}{} and {} order by 2, 3",
            self.rel,
            oid("c.oid")
        ))?;
        // foreign tables, views of other kinds: said, not left out in silence
        for r in self.pg(&format!(
            "select n.nspname, c.relname from pg_class c join pg_namespace n on n.oid = c.relnamespace \
             where c.relkind = 'f' and {user} and {} order by 1, 2",
            oid("c.oid")
        ))? {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            self.report.skipped.push(format!("foreign table {}.{}: foreign tables aren't imported", v(0), v(1)));
        }
        let oids: Vec<String> = tables.iter().filter_map(|r| r[0].clone()).collect();
        if oids.is_empty() {
            return Ok(vec![]);
        }
        let within = format!("({})", oids.join(", "));
        // a column's own collation isn't carried (text sorts by bytes here): said, not left out in silence
        for r in self.pg_or_none(&format!(
            "select n.nspname, c.relname, a.attname, co.collname from pg_attribute a \
             join pg_class c on c.oid = a.attrelid join pg_namespace n on n.oid = c.relnamespace \
             join pg_collation co on co.oid = a.attcollation \
             where c.oid in {within} and a.attnum > 0 and not a.attisdropped \
             and a.attcollation <> (select t.typcollation from pg_type t where t.oid = a.atttypid) order by 1, 2, a.attnum"
        )) {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            self.report.partial.push(format!(
                "column {}.{}.{}: collation {} isn't carried (text sorts by bytes here, as COLLATE \"C\" does)",
                v(0),
                v(1),
                v(2),
                v(3)
            ));
        }
        // per table: each column's name, type, NOT NULL and default (or generation, or identity), and whether
        // it is the table's own (a column a parent gave it is made by INHERITS)
        type Col = (String, String, bool, Option<String>, bool);
        let mut cols: BTreeMap<String, Vec<Col>> = BTreeMap::new();
        let mut copied: BTreeMap<String, Vec<String>> = BTreeMap::new();
        // defaults Chronos refuses (see `chronos::sql::default_refused`): column and default, by table
        let mut dropped: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let col_sql = format!(
            "select a.attrelid, a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
             pg_get_expr(d.adbin, d.adrelid), a.attidentity, a.attgenerated, a.attnum, a.attislocal, a.attinhcount \
             from pg_attribute a left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
             where a.attnum > 0 and not a.attisdropped and a.attrelid in {within} order by a.attrelid, a.attnum"
        );
        for r in self.pg(&col_sql)? {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let (rel, name, ty) = (v(0), v(1), v(2));
            let default = match (v(5).as_str(), v(6).as_str()) {
                // an identity column: its own sequence's options, and whether a value may be given
                (kind @ ("a" | "d"), _) => {
                    let opts = identity_seqs.get(&(rel.clone(), v(7))).cloned().unwrap_or_default();
                    let how = if kind == "a" { "always" } else { "by default" };
                    Some(format!("generated {how} as identity {opts}"))
                }
                (_, "s") => r[4].clone().map(|e| format!("generated always as ({e}) stored")),
                _ => match r[4].clone() {
                    Some(e) if chronos::sql::default_refused(&e) => {
                        dropped.entry(rel.clone()).or_default().push((name.clone(), e));
                        None
                    }
                    e => e.map(|e| format!("default {e}")),
                },
            };
            if v(6).is_empty() {
                copied.entry(rel.clone()).or_default().push(q(&name));
            }
            let own = v(8) == "t" || v(9) == "0";
            cols.entry(rel).or_default().push((name, ty, v(3) == "t", default, own));
        }
        // a table's parents (ordinary inheritance), in the order INHERITS lists them
        let mut parents: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in self.pg_or_none(&format!(
            "select h.inhrelid, n.nspname, c.relname from pg_inherits h join pg_class c on c.oid = h.inhparent \
             join pg_namespace n on n.oid = c.relnamespace where h.inhrelid in {within} order by h.inhrelid, h.inhseqno"
        )) {
            let [Some(child), Some(schema), Some(parent)] = [&r[0], &r[1], &r[2]] else { continue };
            parents.entry(child.clone()).or_default().push(qualified(schema, parent));
        }
        // partitions, each with its parent, bound and (if it is partitioned too) its own key: parents first
        struct Part {
            oid: String,
            parent: String,
            name: String,
            bound: String,
            key: Option<String>,
        }
        let mut parts: Vec<Part> = vec![];
        let part_sql = format!(
            "with recursive p as (select i.inhparent, i.inhrelid, 1 as depth from pg_inherits i \
             where i.inhparent in {within} union all select i.inhparent, i.inhrelid, p.depth + 1 \
             from pg_inherits i join p on i.inhparent = p.inhrelid) \
             select c.oid, pn.nspname, pc.relname, n.nspname, c.relname, pg_get_expr(c.relpartbound, c.oid), \
             case when c.relkind = 'p' then pg_get_partkeydef(c.oid) end, p.depth \
             from p join pg_class c on c.oid = p.inhrelid join pg_namespace n on n.oid = c.relnamespace \
             join pg_class pc on pc.oid = p.inhparent join pg_namespace pn on pn.oid = pc.relnamespace \
             where c.relispartition order by p.depth, n.nspname, c.relname"
        );
        for r in self.pg_or_none(&part_sql) {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            parts.push(Part {
                oid: v(0),
                parent: qualified(&v(1), &v(2)),
                name: qualified(&v(3), &v(4)),
                bound: v(5),
                key: r[6].clone(),
            });
        }
        let part_oids: Vec<String> = parts.iter().map(|p| p.oid.clone()).collect();
        let mut cons: BTreeMap<String, Vec<String>> = BTreeMap::new();
        // (a partition's own constraints, by its oid, that its parent did not give it)
        let every = if part_oids.is_empty() {
            within.clone()
        } else {
            format!("({}, {})", oids.join(", "), part_oids.join(", "))
        };
        let con_sql = format!(
            "select conrelid, conname, pg_get_constraintdef(oid) from pg_constraint \
             where contype in ('p', 'u', 'c', 'x') and conislocal and conrelid in {every} order by conrelid, contype, conname"
        );
        for r in self.pg(&con_sql)? {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            cons.entry(v(0)).or_default().push(format!("constraint {} {}", q(&v(1)), v(2)));
        }
        let mut out: Vec<Table> = vec![];
        for r in &tables {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let (rel, name) = (v(0), qualified(&v(1), &v(2)));
            if v(3) == "t" {
                self.report.skipped.push(format!(
                    "row-level security on {name}: not carried over (Chronos has it: make the policies again with \
                     CREATE POLICY, and ALTER TABLE ... ENABLE ROW LEVEL SECURITY)"
                ));
            }
            // its owner reads past its own policies unless the table FORCEs them
            let hidden = v(3) == "t" && !bypass && !(v(9) == "t" && v(8) != "t");
            for (c, e) in dropped.remove(&rel).unwrap_or_default() {
                self.report.skipped.push(format!(
                    "default {e} of {name}.{}: Postgres works it out afresh for each row and Chronos keeps a default \
                     as one value, so the column came over without it (give it a value in each INSERT)",
                    q(&c)
                ));
            }
            let inheriting = parents.contains_key(&rel);
            let column = |(c, ty, not_null, default, _): &Col| {
                let nn = if *not_null { " not null" } else { "" };
                let d = default.as_ref().map_or(String::new(), |d| format!(" {d}"));
                format!("{} {ty}{nn}{d}", q(c))
            };
            let all_cols: Vec<String> = cols.get(&rel).into_iter().flatten().map(column).collect();
            let own_cols: Vec<String> =
                cols.get(&rel).into_iter().flatten().filter(|c| !inheriting || c.4).map(column).collect();
            let constraints = cons.remove(&rel).unwrap_or_default();
            let key = r[6].clone();
            let tail = key.as_ref().map_or(String::new(), |k| format!(" partition by {k}"));
            let inherits = parents.get(&rel).map_or(String::new(), |ps| format!(" inherits ({})", ps.join(", ")));
            let mut with_keys = own_cols;
            with_keys.extend(constraints.clone());
            let create = format!("create table {name} ({}){inherits}{tail}", with_keys.join(", "));
            // (the same without partitioning or inheritance: every column, and the constraints it has of its own)
            let mut flat_cols = all_cols;
            flat_cols.extend(constraints);
            let flat = (key.is_some() || inheriting).then(|| format!("create table {name} ({})", flat_cols.join(", ")));
            let mut parts_of: Vec<(String, String)> = vec![];
            if key.is_some() {
                // its partitions, parents before their own partitions, and constraints of theirs
                let (mut family, mut queue) = (vec![], vec![name.clone()]);
                while let Some(parent) = queue.pop() {
                    for part in parts.iter().filter(|p| p.parent == parent) {
                        queue.push(part.name.clone());
                        family.push(part);
                    }
                }
                for part in family {
                    let mut sql = format!("create table {} partition of {} {}", part.name, part.parent, part.bound);
                    if let Some(k) = &part.key {
                        sql.push_str(&format!(" partition by {k}"));
                    }
                    parts_of.push((format!("partition {}", part.name), sql));
                    for c in cons.remove(&part.oid).unwrap_or_default() {
                        parts_of.push((
                            format!("constraint on partition {}", part.name),
                            format!("alter table {} add {c}", part.name),
                        ));
                    }
                }
            }
            out.push(Table {
                create,
                name,
                rows: v(4).parse().unwrap_or(0),
                copied: copied.remove(&rel).unwrap_or_default(),
                oid: rel,
                parts: parts_of,
                flat,
                only: v(7) == "t" && key.is_none(),
                hidden,
            });
        }
        // a table after the tables it inherits from (`parents` has their names, as `Table::name` is written)
        fn visit(
            i: usize,
            out: &[Table],
            parents: &BTreeMap<String, Vec<String>>,
            seen: &mut BTreeSet<usize>,
            order: &mut Vec<usize>,
        ) {
            if !seen.insert(i) {
                return;
            }
            for p in parents.get(&out[i].oid).into_iter().flatten() {
                if let Some(j) = out.iter().position(|t| &t.name == p) {
                    visit(j, out, parents, seen, order);
                }
            }
            order.push(i);
        }
        let (mut order, mut seen): (Vec<usize>, BTreeSet<usize>) = (vec![], BTreeSet::new());
        for i in 0..out.len() {
            visit(i, &out, &parents, &mut seen, &mut order);
        }
        let mut slots: Vec<Option<Table>> = out.into_iter().map(Some).collect();
        self.part_oids = part_oids;
        Ok(order.into_iter().filter_map(|i| slots[i].take()).collect())
    }

    /// One index, made from its Postgres definition: the method it has here if that one isn't, a btree if not even
    /// that (it only finds rows by it), what it lost said in the report.
    fn carry_index(&mut self, name: String, def: String) {
        // (a partitioned table's index reads `ON ONLY`, its partitions' own indexes attached after: here the
        // partitions get theirs with it, which is what that comes to, and what a unique one needs)
        let def = def.replacen(" ON ONLY ", " ON ", 1);
        let unique = def.starts_with("CREATE UNIQUE INDEX");
        let mut made = self.attempt(&def);
        // a method Chronos has no index for on that column: the index it has for the column, or a btree
        // (it only finds rows by it), said
        if let Err(first) = &made {
            let first = first.clone();
            for method in ["gin", "btree"] {
                let Some((head, tail)) = def.split_once(" USING ") else { break };
                let Some((old, rest)) = tail.split_once(' ') else { break };
                if ["btree", "gin"].contains(&old) {
                    break;
                }
                if self.attempt(&format!("{head} USING {method} {rest}")).is_ok() {
                    self.report
                        .partial
                        .push(format!("index {name}: a {old} index ({first}) came over as a {method} index"));
                    made = Ok(());
                    break;
                }
            }
        }
        match made {
            Ok(()) => {
                self.report.done.push(format!("index {name}"));
                for l in index_losses(&def) {
                    self.report.partial.push(format!("index {name}: {l}"));
                }
            }
            Err(e) => {
                self.report.skipped.push(format!("index {name}: {e}"));
                if unique {
                    self.report.unenforced.push(name);
                }
            }
        }
    }

    /// Everything that comes after the rows: foreign keys (checked against them), indexes,
    /// views, and sequences set to where Postgres had them.
    fn after(&mut self, within: &str) -> Result<(), String> {
        let user = self.user.clone();
        let idx = self.pg(&format!(
            "select i.indexrelid::regclass::text, pg_get_indexdef(i.indexrelid) from pg_index i \
             where i.indrelid in {within} \
             and not exists (select 1 from pg_constraint c where c.conindid = i.indexrelid and c.contype in ('p', 'u', 'x')) \
             and not exists (select 1 from pg_inherits h where h.inhrelid = i.indexrelid) order by 1"
        ))?;
        for r in idx {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            self.carry_index(v(0), v(1));
        }
        // (after the indexes: a foreign key may refer to a unique index that is no constraint)
        let fks = self.pg(&format!(
            "select k.conrelid::regclass::text, k.conname, pg_get_constraintdef(k.oid), n.nspname, c.relname from pg_constraint k \
             join pg_class c on c.oid = k.conrelid join pg_namespace n on n.oid = c.relnamespace \
             where k.contype = 'f' and k.conrelid in {within} and k.conparentid = 0 order by 1, 2"
        ))?;
        for r in fks {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let sql = format!("alter table {} add constraint {} {}", qualified(&v(3), &v(4)), q(&v(1)), v(2));
            if self.run(&format!("foreign key {} on {}", v(1), v(0)), &sql) {
                self.report.done.push(format!("foreign key {} on {}", v(1), v(0)));
            }
        }
        // functions that read tables: now there are tables (those still refused, one that reads a view, say, try
        // once more after the views)
        let items: Vec<Item> = std::mem::take(&mut self.later).into_iter().map(|(w, s, _)| (w, s)).collect();
        self.later = self.settle(items);
        let views = self.pg(&format!(
            "select n.nspname, c.relname, c.relkind, pg_get_viewdef(c.oid), c.relispopulated from pg_class c \
             join pg_namespace n on n.oid = c.relnamespace where c.relkind in ('v', 'm') and {user}{} and {} order by c.oid",
            self.rel,
            OWN.replace("{oid}", "c.oid")
        ))?;
        for r in views {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let (kind, word) = if v(2) == "m" { ("materialized view", "materialized view") } else { ("view", "view") };
            let def = v(3).trim().trim_end_matches(';').to_string();
            let name = qualified(&v(0), &v(1));
            // (a materialized view Postgres holds unpopulated, `WITH NO DATA`, stays so: reading it is 55000 until
            // REFRESH, as there, and pg_dump's script makes it the same way)
            let empty = if v(2) == "m" && v(4) == "f" { " with no data" } else { "" };
            if self.run(&format!("{kind} {name}"), &format!("create {word} {name} as {def}{empty}")) {
                self.report.done.push(format!("{kind} {name}"));
            }
        }
        // a materialized view's indexes (a unique one is what REFRESH ... CONCURRENTLY wants), made once it is
        let mviews = self.pg(&format!(
            "select i.indexrelid::regclass::text, pg_get_indexdef(i.indexrelid) from pg_index i \
             join pg_class c on c.oid = i.indrelid join pg_namespace n on n.oid = c.relnamespace \
             where c.relkind = 'm' and {user}{} and {} order by 1",
            self.rel,
            OWN.replace("{oid}", "c.oid")
        ))?;
        for r in mviews {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            self.carry_index(v(0), v(1));
        }
        // functions that read a view: now there are views
        for (what, e) in self.settle_later() {
            self.report.skipped.push(format!("{what}: {e}"));
        }
        // a serial's sequence belongs to its column: dropping the table drops it
        for r in self.pg_or_none(&format!(
            "select ns.nspname, s.relname, nt.nspname, t.relname, a.attname from pg_depend d \
             join pg_class s on s.oid = d.objid and s.relkind = 'S' join pg_namespace ns on ns.oid = s.relnamespace \
             join pg_class t on t.oid = d.refobjid join pg_namespace nt on nt.oid = t.relnamespace \
             join pg_attribute a on a.attrelid = t.oid and a.attnum = d.refobjsubid \
             where d.classid = 'pg_class'::regclass and d.refclassid = 'pg_class'::regclass and d.deptype = 'a' \
             and d.refobjsubid > 0 and t.oid in {within} order by 1, 2"
        )) {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let sql =
                format!("alter sequence {} owned by {}.{}", qualified(&v(0), &v(1)), qualified(&v(2), &v(3)), q(&v(4)));
            self.run(&format!("sequence {}.{} owned by {}.{}", v(0), v(1), v(2), v(3)), &sql);
        }
        let seqs = self.pg(&format!(
            "select schemaname, sequencename, last_value from pg_sequences n0 join pg_namespace n on n.nspname = n0.schemaname \
             join pg_class s on s.relnamespace = n.oid and s.relname = n0.sequencename \
             where {user}{} and last_value is not null",
            self.seq_rel("s")
        ))?;
        for r in seqs {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            if self.seqs_refused.contains(&(v(0), v(1))) {
                continue;
            }
            let name = seq_name(&v(0), &v(1));
            self.run(&format!("sequence {name}"), &format!("select setval({}, {})", lit(&name), v(2)));
        }
        // triggers, once the rows are in (they would have fired on each) and their functions are there
        let newer = format!(
            "select {}, t.tgname, pg_get_triggerdef(t.oid) from pg_trigger t join pg_class c on c.oid = t.tgrelid \
             join pg_namespace n on n.oid = c.relnamespace where not t.tgisinternal and t.tgparentid = 0 and {user}{} \
             and {} order by 1, 2",
            pg_qualified("n", "c"),
            self.rel,
            OWN.replace("{oid}", "c.oid")
        );
        let older = newer.replace(" and t.tgparentid = 0", "");
        let triggers = match self.pg(&newer) {
            Ok(r) => r,
            Err(_) => self.pg(&older)?,
        };
        for r in triggers {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let what = format!("trigger {} on {}", v(1), v(0));
            if self.attach {
                self.report.carried.push(format!("{what}: stays in Postgres, and runs there as a merge is written"));
                continue;
            }
            if self.run(&what, &v(2)) {
                self.report.carried.push(what);
            }
        }
        // (pg_rules and pg_policies name the schema and table so: the tables included, as `rel` says)
        let of_tables = self.rel.replace("n.nspname", "schemaname").replace("c.relname", "tablename");
        let rules = self.pg(&format!(
            "select schemaname, tablename, rulename, definition from pg_rules \
             where schemaname !~ '^pg_' and schemaname <> 'information_schema'{of_tables} order by 1, 2, 3"
        ))?;
        for r in rules {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            let what = format!("rule {} on {}.{}", v(2), v(0), v(1));
            if self.attach {
                self.report.carried.push(format!("{what}: stays in Postgres, and runs there as a merge is written"));
                continue;
            }
            if self.run(&what, &v(3)) {
                self.report.carried.push(what);
            }
        }
        let policies = self.pg(&format!(
            "select schemaname || '.' || tablename, policyname from pg_policies where true{of_tables} order by 1, 2"
        ))?;
        for r in policies {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            self.report.skipped.push(format!(
                "policy {} on {}: not carried over (make it again with CREATE POLICY: Chronos has row-level security)",
                v(1),
                v(0)
            ));
        }
        // what Chronos has in a form Postgres's definitions don't carry over to: said, not guessed at
        for (kind, sql) in [
            ("event trigger", "select evtname from pg_event_trigger order by 1".to_string()),
            (
                "operator",
                format!(
                    "select n.nspname || '.' || o.oprname from pg_operator o join pg_namespace n on n.oid = o.oprnamespace \
                     where {user} and {} order by 1",
                    OWN.replace("{oid}", "o.oid")
                ),
            ),
            ("publication", "select pubname from pg_publication order by 1".to_string()),
        ] {
            for r in self.pg_or_none(&sql) {
                let n = r[0].clone().unwrap_or_default();
                self.report.skipped.push(format!("{kind} {n}: not imported"));
            }
        }
        self.comments()?;
        Ok(())
    }

    /// `settle` for the functions left over (see `later`): what's still refused is reported by the caller.
    fn settle_later(&mut self) -> Vec<(String, String)> {
        let items: Vec<Item> = std::mem::take(&mut self.later).into_iter().map(|(w, s, _)| (w, s)).collect();
        self.settle(items).into_iter().map(|(w, _, e)| (w, e)).collect()
    }

    /// COMMENT ON for what Chronos keeps comments of: schemas, tables, views, sequences, indexes, columns,
    /// constraints, enum types and functions. Postgres writes each statement.
    fn comments(&mut self) -> Result<(), String> {
        let user = self.user.clone();
        let rel = |alias: &str| format!("{alias}.relnamespace = n.oid");
        let own = |c: &str| OWN.replace("{oid}", c);
        let rn = pg_qualified("n", "c");
        // (only the included relations': an index by its table, a sequence as `seq_rel` keeps them)
        let sel = &self.rel;
        let seq = self.seq_rel("c");
        let of_fn = self.fn_rel("p");
        let of_rel = format!(
            "(case when c.relkind in ('i', 'I') then exists (select 1 from pg_index x join pg_class c on c.oid = x.indrelid \
             join pg_namespace n on n.oid = c.relnamespace where x.indexrelid = d.objoid{sel}) \
             when c.relkind = 'S' then true{seq} else true{sel} end)"
        );
        let sql = format!(
            "select format('comment on %s %s is %L', case c.relkind when 'v' then 'view' when 'm' then 'materialized view' \
               when 'i' then 'index' when 'I' then 'index' when 'S' then 'sequence' else 'table' end, {rn}, d.description) \
             from pg_description d join pg_class c on c.oid = d.objoid and d.classoid = 'pg_class'::regclass \
             join pg_namespace n on {} where d.objsubid = 0 and c.relkind in ('r', 'p', 'v', 'm', 'i', 'I', 'S') \
             and {user} and {of_rel} and {} \
             union all \
             select format('comment on column %s.%s is %L', {rn}, quote_ident(a.attname), d.description) \
             from pg_description d join pg_class c on c.oid = d.objoid and d.classoid = 'pg_class'::regclass \
             join pg_namespace n on {} join pg_attribute a on a.attrelid = c.oid and a.attnum = d.objsubid \
             where d.objsubid > 0 and c.relkind in ('r', 'p', 'v', 'm') and not a.attisdropped and {user}{sel} and {} \
             union all \
             select format('comment on constraint %s on %s is %L', quote_ident(k.conname), {rn}, d.description) \
             from pg_description d join pg_constraint k on k.oid = d.objoid and d.classoid = 'pg_constraint'::regclass \
             join pg_class c on c.oid = k.conrelid join pg_namespace n on {} where {user}{sel} and {} \
             union all \
             select format('comment on schema %s is %L', quote_ident(n.nspname), d.description) \
             from pg_description d join pg_namespace n on n.oid = d.objoid and d.classoid = 'pg_namespace'::regclass \
             where {user} and n.nspname <> 'public' \
             union all \
             select format('comment on %s %s is %L', case when t.typtype = 'd' then 'domain' else 'type' end, \
               case when n.nspname = 'public' then quote_ident(t.typname) else quote_ident(n.nspname) || '.' || quote_ident(t.typname) end, \
               d.description) \
             from pg_description d join pg_type t on t.oid = d.objoid and d.classoid = 'pg_type'::regclass \
             join pg_namespace n on n.oid = t.typnamespace where d.objsubid = 0 and {user} and {} \
             union all \
             select format('comment on %s %s(%s) is %L', case when p.prokind = 'p' then 'procedure' else 'function' end, \
               case when n.nspname = 'public' then quote_ident(p.proname) else quote_ident(n.nspname) || '.' || quote_ident(p.proname) end, \
               pg_get_function_identity_arguments(p.oid), d.description) \
             from pg_description d join pg_proc p on p.oid = d.objoid and d.classoid = 'pg_proc'::regclass \
             join pg_namespace n on n.oid = p.pronamespace where p.prokind in ('f', 'p') and {user}{of_fn} and {}",
            rel("c"),
            own("c.oid"),
            rel("c"),
            own("c.oid"),
            rel("c"),
            own("k.oid"),
            own("t.oid"),
            own("p.oid"),
        );
        let (mut n, mut why) = (0, vec![]);
        for r in self.pg(&sql)? {
            let Some(stmt) = r[0].clone() else { continue };
            match self.attempt(&stmt) {
                Ok(()) => n += 1,
                Err(e) => why.push(format!("comment ({}): {e}", stmt.chars().take(60).collect::<String>())),
            }
        }
        if n > 0 {
            self.report.carried.push(format!("{n} comments"));
        }
        self.report.skipped.extend(why);
        Ok(())
    }

    /// The rows of `t`, streamed from Postgres's COPY into Chronos's, as one INSERT.
    fn rows(&mut self, t: &Table) -> Result<u64, String> {
        let cols = t.copied.join(", ");
        // (a table others inherit from: its own rows, the others' being copied with them)
        let only = if t.only { "only " } else { "" };
        // a table with no column to copy (none at all, or only stored generated ones): COPY has no empty column
        // list, so its rows come as many DEFAULT VALUES rows as Postgres has, which is all such a row holds
        if t.copied.is_empty() {
            let n: u64 = self
                .pg(&format!("select count(*) from {only}{}", t.name))?
                .first()
                .and_then(|r| r.first().cloned().flatten())
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            // (all of them or none, as a COPY's rows: one transaction, rolled back if stopped or refused)
            self.s.run(self.db, "begin", &[]).map_err(|e| e.message)?;
            let mut done = Ok(n);
            for _ in 0..n {
                if self.progress.stop.load(Ordering::Relaxed) {
                    done = Err(CANCELED.to_string());
                    break;
                }
                if let Err(e) = self.s.run(self.db, &format!("insert into {} default values", t.name), &[]) {
                    done = Err(e.message);
                    break;
                }
                self.progress.rows.fetch_add(1, Ordering::Relaxed);
            }
            let end = if done.is_ok() { "commit" } else { "rollback" };
            if let Err(e) = self.s.run(self.db, end, &[]) {
                let _ = self.s.run(self.db, "rollback", &[]);
                return Err(e.message);
            }
            return done;
        }
        self.pg
            .borrow_mut()
            .copy_out(&format!("copy (select {cols} from {only}{}) to stdout", t.name))
            .map_err(|e| format!("reading Postgres: {e}"))?;
        let copy_in = |s: &mut Session, db: &Db| -> Result<chronos::sql::CopyIn, SqlError> {
            let st = s.parse(db, &format!("copy {} ({cols}) from stdin", t.name))?.pop();
            s.copy_in(db, &st.ok_or_else(|| SqlError { code: "42601", message: "no COPY".into(), info: None })?)
        };
        let pg = self.pg.clone();
        let drain = |pg: &Rc<RefCell<Client>>| while let Ok(Some(_)) = pg.borrow_mut().copy_chunk() {};
        let copy = match copy_in(&mut self.s, self.db) {
            Ok(c) => c,
            Err(e) => {
                drain(&pg);
                return Err(e.message);
            }
        };
        let from = pg.clone();
        let progress = self.progress.clone();
        let next = Box::new(move || {
            if progress.stop.load(Ordering::Relaxed) {
                return Err(SqlError { code: "57014", message: CANCELED.into(), info: None });
            }
            let chunk = from.borrow_mut().copy_chunk().map_err(|e| SqlError {
                code: "08006",
                message: format!("reading Postgres: {e}"),
                info: None,
            })?;
            // (COPY's text format ends each row with a newline, and writes one inside a value as \n)
            if let Some(c) = &chunk {
                progress.rows.fetch_add(c.iter().filter(|b| **b == b'\n').count() as u64, Ordering::Relaxed);
            }
            Ok(chunk)
        });
        let out = copy.run(&mut self.s, self.db, next);
        // what's left of a COPY that failed partway (not of one stopped: the import ends, and the connection with it)
        if !self.progress.stop.load(Ordering::Relaxed) {
            drain(&pg);
        }
        let out = out.map_err(|e| e.message)?;
        Ok(out.tag.trim_start_matches("COPY ").parse().unwrap_or(0))
    }

    /// Makes `t`: with its partitions, or inheritance; as a plain table, said, when Chronos refuses those.
    fn make(&mut self, t: &Table) -> bool {
        let mut why = self.attempt(&t.create).err();
        if why.is_none() {
            for (what, sql) in &t.parts {
                if let Err(e) = self.attempt(sql) {
                    why = Some(format!("{what}: {e}"));
                    break;
                }
            }
            if why.is_some() {
                // (the partitions made so far go with it)
                let _ = self.attempt(&format!("drop table {} cascade", t.name));
            }
        }
        let Some(why) = why else {
            for (what, _) in t.parts.iter().filter(|(w, _)| w.starts_with("partition ")) {
                self.report.carried.push(format!("{what} of {}", t.name));
            }
            return true;
        };
        match t.flat.as_ref().map(|flat| self.attempt(flat)) {
            Some(Ok(())) => {
                self.report.skipped.push(format!(
                    "partitioning or inheritance of {}: {why} (it came over as one plain table with all its rows)",
                    t.name
                ));
                true
            }
            _ => {
                self.report.skipped.push(format!("table {}: {why}", t.name));
                false
            }
        }
    }
}

/// `sql` with `schema.name` (either part quoted or not) written as `name` wherever `schema` is one of `schemas` and
/// `name` one of `names`: the extensions Chronos has built in have no schema here, while Postgres may keep them in one
/// (Supabase's `extensions`), and its definitions then name their types, functions and operator classes with it.
fn unqualify<'s>(
    sql: &'s str,
    schemas: &[String],
    names: &std::collections::HashSet<String>,
) -> std::borrow::Cow<'s, str> {
    if schemas.is_empty() || names.is_empty() {
        return std::borrow::Cow::Borrowed(sql);
    }
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    // the identifier at the start of `s` (quoted or not), and how many bytes it takes
    let word = |s: &str| -> Option<(String, usize)> {
        if let Some(rest) = s.strip_prefix('"') {
            let end = rest.find('"')?;
            return Some((rest[..end].to_string(), end + 2));
        }
        let end = s.find(|c: char| !ident(c)).unwrap_or(s.len());
        (end > 0).then(|| (s[..end].to_lowercase(), end))
    };
    let (mut out, mut i, mut changed) = (String::with_capacity(sql.len()), 0, false);
    while i < sql.len() {
        let prev = sql[..i].chars().next_back();
        let at_start = prev.is_none_or(|p| !ident(p) && p != '.' && p != '"');
        if at_start
            && let Some((schema, n)) = word(&sql[i..])
            && schemas.contains(&schema)
            && sql[i + n..].starts_with('.')
            && let Some((name, _)) = word(&sql[i + n + 1..])
            && names.contains(&name)
        {
            i += n + 1; // the name stays; its schema goes
            changed = true;
            continue;
        }
        let c = sql[i..].chars().next().expect("within the string");
        out.push(c);
        i += c.len_utf8();
    }
    match changed {
        true => std::borrow::Cow::Owned(out),
        false => std::borrow::Cow::Borrowed(sql),
    }
}

pub fn help() -> &'static str {
    "usage: chronos import <folder> postgres://user:password@host:port/database [--dry-run] [-b <world>]\n\
     \x20                                   [--allow-plaintext-password]\n\n\
     Moves a live Postgres database into Chronos: schemas, types (enums, domains, composite types, ranges),\n\
     sequences, functions, tables (partitioned and inheriting ones too) and their rows, keys, foreign keys, indexes,\n\
     views, triggers, rules and comments. Anything it can't bring over, or only in part, is listed at the end instead.\n\
     --dry-run reads Postgres and tries every definition without writing anything.\n\
     TLS follows the URL's sslmode (prefer by default; require; verify-full with sslrootcert or the system's CAs).\n\
     The password can also come from PGPASSWORD. A server that asks for it in the clear or as MD5 is refused unless\n\
     the connection is TLS with the certificate checked for the server's name (sslmode=verify-full) or\n\
     --allow-plaintext-password is given (SCRAM-SHA-256 never sends it).\n\
     connect_timeout=<seconds> in the URL (default 30) bounds connecting; a server silent for 10 minutes is dropped."
}

/// Imports the Postgres database at `url` into world `branch` of `db` (with `dry`, into an empty
/// in-memory database, rows left out). Prints the report; Err when it couldn't start. With
/// `plain_password`, a password may go in the clear (or as MD5) over a connection that isn't TLS.
pub fn run(db: Option<&Db>, url: &str, branch: &str, dry: bool, plain_password: bool) -> Result<(), String> {
    let mut url = parse(url)?;
    url.plain_password = plain_password;
    let report = import(db, url, branch, dry, &[], Arc::default(), true)?;
    let summary = match dry {
        true => format!("would import {} of {} tables", report.imported, report.total),
        false => {
            format!("imported {} of {} tables ({} rows) into {branch}", report.imported, report.total, report.rows)
        }
    };
    println!("\n{summary}, and {} foreign keys, indexes and views", report.done.len());
    if !report.carried.is_empty() {
        println!("\nalso brought over ({}):", report.carried.len());
        for s in &report.carried {
            println!("  - {s}");
        }
    }
    if !report.partial.is_empty() {
        println!("\nbrought over in part ({}):", report.partial.len());
        for s in &report.partial {
            println!("  - {s}");
        }
    }
    if !report.skipped.is_empty() {
        println!("\nnot imported ({}):", report.skipped.len());
        for s in &report.skipped {
            println!("  - {s}");
        }
    }
    println!(
        "\nroles, grants and passwords aren't imported: give each program an agent (CREATE AGENT) with the rights it needs"
    );
    match unenforced(&report) {
        Some(e) => Err(e), // a rule on the data was lost, not just a convenience: not a success
        None => Ok(()),
    }
}

/// Why an import isn't a success: a table that didn't come over, a unique index that didn't (the tables now accept
/// rows Postgres refused), or tables whose rows row-level security kept from the role imported as (empty here).
pub fn unenforced(report: &Report) -> Option<String> {
    let mut why = vec![];
    if !report.missing.is_empty() {
        why.push(format!(
            "{} table(s) did not come over, nor their rows: {} (see \"not imported\" above for why)",
            report.missing.len(),
            report.missing.join(", ")
        ));
    }
    if !report.hidden.is_empty() {
        why.push(format!(
            "{} table(s) have row-level security the role imported as doesn't bypass, so their rows weren't copied: {}. \
             Import again as a role that reads past it: the tables' owner, a superuser, or one with BYPASSRLS (on \
             Supabase, the postgres role)",
            report.hidden.len(),
            report.hidden.join(", ")
        ));
    }
    if !report.unenforced.is_empty() {
        why.push(format!(
            "{} unique index(es) did not come over, so those tables now accept rows Postgres refused: {} (see \"not imported\" above)",
            report.unenforced.len(),
            report.unenforced.join(", ")
        ));
    }
    (!why.is_empty()).then(|| why.join("; "))
}

/// `run`'s import, told as it goes to `progress` (and printed, with `echo`), with only the schemas and
/// `schema.table`s in `include` (all when empty). The password in `url` is wiped once connected: nothing here keeps it.
pub fn import(
    db: Option<&Db>,
    mut url: Url,
    branch: &str,
    dry: bool,
    include: &[String],
    progress: Arc<Progress>,
    echo: bool,
) -> Result<Report, String> {
    progress.now("connecting to Postgres");
    let client = Client::connect(&url);
    wipe(&mut url.password);
    let client = client.map_err(|e| format!("Postgres at {}:{}: {e}", url.host, url.port))?;
    import_with(db, client, &url.database, branch, dry, include, progress, echo, false).map(|(r, _)| r)
}

/// `import` on a connection made already (`chronos attach` runs it inside its own transaction), which it gives back.
/// With `attach`, triggers and rules aren't made here: they stay in Postgres.
#[allow(clippy::too_many_arguments)]
fn import_with(
    db: Option<&Db>,
    client: Client,
    source: &str,
    branch: &str,
    dry: bool,
    include: &[String],
    progress: Arc<Progress>,
    echo: bool,
    attach: bool,
) -> Result<(Report, Client), String> {
    let scratch = Db::new();
    let (db, branch) = match (dry, db) {
        (true, _) => (&scratch, chronos::MAIN),
        (false, Some(db)) => (db, branch),
        (false, None) => return Err("no database to import into".into()),
    };
    let s = Session::new(branch);
    let (ns, rel, whole) = included(include);
    let mut im = Import {
        pg: Rc::new(RefCell::new(client)),
        db,
        s,
        report: Report::default(),
        dry,
        user: format!("{USER}{ns}"),
        rel,
        whole,
        progress: progress.clone(),
        later: vec![],
        part_oids: vec![],
        seqs_refused: BTreeSet::new(),
        attach,
        ext_schemas: vec![],
        ext_names: Default::default(),
    };
    im.report.database_name(source, branch);
    // as pg_dump does: a COPY that row-level security would filter is an error, never fewer rows in silence (a
    // pooler in transaction mode may drop the setting between statements: `Table::hidden` checks beforehand too)
    if !attach {
        let _ = im.pg("set row_security = off");
    }
    im.source_collation();
    progress.now("reading the catalog: schemas, types, sequences, functions and tables");
    let tables = im.catalog()?;
    progress.tables.store(tables.len(), Ordering::Relaxed);
    let mut oids: Vec<&str> = tables.iter().map(|t| t.oid.as_str()).collect();
    let (mut n, mut rows) = (0, 0u64);
    for t in &tables {
        progress.stopped()?;
        progress.now(format!("table {}", t.name));
        let made = im.make(t);
        if !made {
            im.report.missing.push(t.name.clone());
        }
        if made && t.hidden {
            // its rows aren't all the role's to read: none copied, rather than only those its policies show
            n += 1;
            im.report.hidden.push(t.name.clone());
            im.report.skipped.push(format!(
                "rows of {}: row-level security is on, and the role imported as doesn't bypass it, so Postgres would \
                 hand over only the rows its policies show that role (often none): none were copied (the table is \
                 there, empty)",
                t.name
            ));
            if echo {
                println!("{}: not copied (row-level security hides its rows from this role)", t.name);
            }
        } else if made {
            n += 1;
            let k = match im.dry {
                true => Some(t.rows.max(0) as u64),
                false => match im.rows(t) {
                    Ok(k) => Some(k),
                    Err(e) => {
                        progress.stopped()?;
                        im.report.skipped.push(format!("rows of {}: {e} (the table is there, empty)", t.name));
                        None
                    }
                },
            };
            if let Some(k) = k {
                if echo {
                    match im.dry {
                        true => println!("{}: about {} rows", t.name, t.rows),
                        false => println!("{}: {k} rows", t.name),
                    }
                }
                rows += if im.dry { 0 } else { k };
                progress.per_table.lock().unwrap_or_else(|p| p.into_inner()).push((t.name.clone(), k));
            }
        }
        // (the rows counted as they came: now exactly the tables' own)
        progress.rows.store(rows, Ordering::Relaxed);
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    progress.stopped()?;
    progress.now("foreign keys, indexes, views, sequences, triggers, rules and comments");
    let part_oids = im.part_oids.clone();
    oids.extend(part_oids.iter().map(String::as_str));
    if !oids.is_empty() {
        im.after(&format!("({})", oids.join(", ")))?;
    }
    progress.now("done");
    im.report.imported = n;
    im.report.total = tables.len();
    im.report.rows = rows;
    let Import { pg, report, .. } = im;
    let client = Rc::try_unwrap(pg).map_err(|_| "the connection is still in use".to_string())?.into_inner();
    Ok((report, client))
}

/// `chronos attach`'s usage.
pub fn attach_help() -> &'static str {
    "usage: chronos attach <new folder> (--tables <schema.table>,... | --schema <schema>) [--yes]\n\n\
     Works beside a Postgres database you already run: copies the tables named (or every table of a schema) into a\n\
     new folder, in one consistent read. Agents fork worlds and work there as anywhere; merging a world into main\n\
     writes its rows to Postgres first, in one transaction, after checking each row is still as main has it (a row\n\
     changed in Postgres meanwhile comes back as a conflict, and nothing is written). Main's rows change only so.\n\n\
     The connection string is read from CHRONOS_SOURCE_URL, or from the file CHRONOS_SOURCE_FILE names (mode 600),\n\
     never from the command line, and is written nowhere. chronos serve on the folder needs it too.\n\
     Attach makes one table in Postgres, chronos.merges (a row per merge written), or prints what an administrator\n\
     runs to make it. A table needs a primary key, and no generated column or row-level security. Inserts into\n\
     tables keyed by a sequence or identity column are refused for now (updates and deletes go). Triggers stay in\n\
     Postgres and run as merges are written. Above 1,000,000 rows (Postgres's estimate) it asks for --yes.\n\
     Tables or columns changed in Postgres later: attach again into a new folder."
}

/// What attach reads of a table before copying it: its name, kind, what Chronos's attach refuses in it, and its
/// columns and primary key as the folder's record keeps them.
struct Source {
    oid: String,
    schema: String,
    name: String,
    rows: i64,
    refused: Vec<String>,
    cols: Vec<attach::Col>,
    key: Vec<usize>,
    /// columns fed by a sequence: position in `cols`, and Postgres's sequence if the role may take values from it
    seqs: Vec<(usize, Option<String>)>,
}

/// `chronos attach <dir>`: see `attach_help`. Prints what it did; Err when it couldn't, with nothing left behind
/// (the folder is removed again).
pub fn attach(dir: &str, opts: &chronos::Open, tables: &[String], schemas: &[String], yes: bool) -> Result<(), String> {
    let path = std::path::Path::new(dir);
    if path.exists() {
        return Err(format!("{dir} exists: attach makes a new folder (to attach again, give another)"));
    }
    if tables.is_empty() && schemas.is_empty() {
        return Err("name the tables to attach: --tables a,b,app.c or --schema <schema>".into());
    }
    let url = attach::source_url()?;
    let mut pg = Client::connect_as(&url, "chronos attach")
        .map_err(|e| format!("Postgres at {}:{}: {e}", url.host, url.port))?;
    // Chronos's own table there: a row per merge written (see `chronos::attach`), and one naming this folder
    let role = pg.query("select current_user").map_err(|e| e.to_string())?;
    let role = role.first().and_then(|r| r[0].clone()).unwrap_or_default();
    let setup = || format!("\nan administrator runs, once:\n\n{}\n", attach::setup_statements(&role));
    let exists = pg.query("select to_regclass('chronos.merges') is not null").map_err(|e| e.to_string())?;
    if exists.first().and_then(|r| r[0].as_deref()) != Some("t") {
        let made = pg.query(
            "create schema if not exists chronos; \
             create table if not exists chronos.merges (id text primary key, at timestamptz not null default now())",
        );
        if let Err(e) = made {
            return Err(format!("{role} can't make the table chronos.merges in Postgres ({e}){}", setup()));
        }
    }
    let shape = pg
        .query(
            "select string_agg(a.attname || ' ' || format_type(a.atttypid, a.atttypmod), ', ' order by a.attnum), \
             has_table_privilege('chronos.merges', 'select') and has_table_privilege('chronos.merges', 'insert') \
             from pg_attribute a where a.attrelid = 'chronos.merges'::regclass and a.attnum > 0 and not a.attisdropped",
        )
        .map_err(|e| e.to_string())?;
    let (cols, may) =
        shape.first().map(|r| (r[0].clone().unwrap_or_default(), r[1].as_deref() == Some("t"))).unwrap_or_default();
    if cols != "id text, at timestamp with time zone" {
        return Err(format!(
            "chronos.merges in Postgres isn't Chronos's table (its columns are {cols}): rename it, then attach"
        ));
    }
    if !may {
        return Err(format!("{role} may not read and insert into chronos.merges{}", setup()));
    }
    // one consistent read of everything from here on
    pg.query(&format!("begin isolation level repeatable read read only; {}", attach::SETTINGS))
        .map_err(|e| format!("starting the read: {e}"))?;
    let named: Vec<String> =
        tables.iter().map(|t| if t.contains('.') { t.clone() } else { format!("public.{t}") }).collect();
    let found = read_sources(&mut pg, &named, schemas)?;
    let missing: Vec<&String> =
        named.iter().filter(|n| !found.iter().any(|s| &format!("{}.{}", s.schema, s.name) == *n)).collect();
    if !missing.is_empty() {
        return Err(format!(
            "no table {} in Postgres",
            missing.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    if found.is_empty() {
        return Err(format!("schema {} has no tables", schemas.join(", ")));
    }
    let refused: Vec<String> =
        found.iter().flat_map(|s| s.refused.iter().map(move |r| format!("  - {}.{}: {r}", s.schema, s.name))).collect();
    if !refused.is_empty() {
        return Err(format!("can't attach, nothing was copied:\n{}", refused.join("\n")));
    }
    let total: i64 = found.iter().map(|s| s.rows).sum();
    for s in &found {
        println!("{}.{}: about {} rows", s.schema, s.name, s.rows);
    }
    if total > 1_000_000 && !yes {
        return Err(format!(
            "about {total} rows in all (Postgres's estimate): copying holds one read open on Postgres for that long. \
             Run again with --yes to go on"
        ));
    }
    let order = parents_first(&mut pg, &found)?;
    let db = Db::open_with(dir, opts).map_err(|e| format!("{dir}: {e}"))?;
    let include: Vec<String> = found.iter().map(|s| format!("{}.{}", s.schema, s.name)).collect();
    let done = (|| -> Result<Attached, String> {
        let (report, mut pg) =
            import_with(Some(&db), pg, &url.database, chronos::MAIN, false, &include, Arc::default(), true, true)?;
        pg.query("commit").map_err(|e| format!("ending the read: {e}"))?;
        let mut lost: Vec<String> =
            report.unenforced.iter().map(|u| format!("unique index {u} didn't come over")).collect();
        lost.extend(
            report
                .skipped
                .iter()
                .filter(|s| s.starts_with("table ") || s.starts_with("rows of ") || s.starts_with("partitioning"))
                .cloned(),
        );
        if !lost.is_empty() {
            return Err(format!(
                "can't attach, as Chronos can't hold these as Postgres does:\n  - {}",
                lost.join("\n  - ")
            ));
        }
        for (kind, list) in [
            ("also brought over", &report.carried),
            ("brought over in part", &report.partial),
            ("not brought over", &report.skipped),
        ] {
            if !list.is_empty() {
                println!("\n{kind} ({}):", list.len());
                for x in list {
                    println!("  - {x}");
                }
            }
        }
        let tables: Vec<attach::Table> = order
            .iter()
            .map(|&i| {
                let s = &found[i];
                attach::Table {
                    name: if s.schema == "public" { s.name.clone() } else { format!("{}.{}", s.schema, s.name) },
                    sql: qualified(&s.schema, &s.name),
                    pg: format!("{}.{}", q(&s.schema), q(&s.name)),
                    cols: s.cols.clone(),
                    key: s.key.clone(),
                    seqs: s
                        .seqs
                        .iter()
                        .map(|(col, pg)| attach::Seq {
                            col: *col,
                            pg: pg.clone(),
                            // the counter the column's default takes values from here, now that the table is
                            counter: chronos::sql::column_counter(
                                &db,
                                &if s.schema == "public" { s.name.clone() } else { format!("{}.{}", s.schema, s.name) },
                                &s.cols[*col].name,
                            ),
                        })
                        .collect(),
                }
            })
            .collect();
        let att = Attached::new(path, &url, tables);
        // the marker saying which Postgres this folder is a copy of, checked before every write-back
        pg.query_params(
            "insert into chronos.merges (id, at) values ($1, now())",
            &[Some(&format!("attach:{}", att.id))],
        )
        .map_err(|e| format!("recording the attach in chronos.merges: {e}"))?;
        att.save().map_err(|e| e.to_string())?;
        Ok(att)
    })();
    drop(db);
    let att = match done {
        Ok(a) => a,
        Err(e) => {
            let _ = std::fs::remove_dir_all(path);
            return Err(e);
        }
    };
    println!("\nattached {} tables of {} to {dir}", att.tables.len(), att.source());
    for t in &att.tables {
        for sq in &t.seqs {
            let col = attach::q(&t.cols[sq.col].name);
            match (&sq.pg, &sq.counter) {
                (Some(seq), Some(_)) => println!(
                    "  {}.{col}: new values come from Postgres's sequence {seq}, reserved {} at a time (the gaps that \
                     leaves in it are normal)",
                    t.sql, 1000
                ),
                _ => println!(
                    "  {}.{col}: its sequence can't be used from here (the role needs USAGE on it): rows of {} can't be \
                     inserted here",
                    t.sql, t.sql
                ),
            }
        }
    }
    println!(
        "\nnext: chronos serve {dir} (with CHRONOS_SOURCE_URL set), fork a world, change rows there, and MERGE WORLD:\n\
         the change goes to Postgres first. main's own rows can't be written directly, and tables can't change shape."
    );
    Ok(())
}

/// The tables `named` (`schema.table`) and those of `schemas`, with what attach reads of each.
fn read_sources(pg: &mut Client, named: &[String], schemas: &[String]) -> Result<Vec<Source>, String> {
    let list = |v: &[String]| crate::pgclient::text_array(v.iter().map(|s| Some(s.as_str())));
    let rel = pg
        .query_params(
            "select c.oid, n.nspname, c.relname, c.relkind, c.relrowsecurity, c.relispartition, \
             exists (select 1 from pg_inherits h where h.inhrelid = c.oid or h.inhparent = c.oid), \
             greatest(c.reltuples, 0)::bigint from pg_class c join pg_namespace n on n.oid = c.relnamespace \
             where (n.nspname || '.' || c.relname = any($1::text[]) and c.relkind in ('r', 'p', 'v', 'm', 'f')) \
             or (n.nspname = any($2::text[]) and c.relkind in ('r', 'p') and not c.relispartition) order by 2, 3",
            &[Some(&list(named)), Some(&list(schemas))],
        )
        .map_err(|e| format!("reading Postgres: {e}"))?;
    let mut out = vec![];
    for r in rel {
        let v = |i: usize| r[i].clone().unwrap_or_default();
        let mut refused = vec![];
        match v(3).as_str() {
            "r" => {}
            "p" => refused.push("a partitioned table (not attached yet)".to_string()),
            "v" | "m" => refused.push("a view: attach the tables it reads".to_string()),
            _ => refused.push("not a table".to_string()),
        }
        if v(4) == "t" {
            refused.push("row-level security is on (the copy could hold only some of its rows)".into());
        }
        if v(5) == "t" || v(6) == "t" {
            refused.push("a partition, or inherits or is inherited from (not attached yet)".into());
        }
        out.push(Source {
            oid: v(0),
            schema: v(1),
            name: v(2),
            rows: v(7).parse().unwrap_or(0),
            refused,
            cols: vec![],
            key: vec![],
            seqs: vec![],
        });
    }
    if out.is_empty() {
        return Ok(out);
    }
    let oids = format!("({})", out.iter().map(|s| s.oid.as_str()).collect::<Vec<_>>().join(", "));
    let cols = pg
        .query(&format!(
            "select a.attrelid, a.attname, format_type(a.atttypid, a.atttypmod), a.attidentity, a.attgenerated, \
             coalesce(pg_get_expr(d.adbin, d.adrelid), '') like 'nextval(%', \
             array_position((select i.indkey::int2[] from pg_index i where i.indrelid = a.attrelid and i.indisprimary), a.attnum), \
             s.seq, s.seq is not null and has_sequence_privilege(s.seq, 'usage') \
             from pg_attribute a left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
             cross join lateral (select pg_get_serial_sequence(a.attrelid::regclass::text, a.attname) seq) s \
             where a.attrelid in {oids} and a.attnum > 0 and not a.attisdropped order by a.attrelid, a.attnum"
        ))
        .map_err(|e| format!("reading Postgres: {e}"))?;
    let mut keys: HashMap<String, Vec<(i64, usize)>> = HashMap::new();
    for r in cols {
        let v = |i: usize| r[i].clone().unwrap_or_default();
        let Some(s) = out.iter_mut().find(|s| s.oid == v(0)) else { continue };
        if !v(4).is_empty() {
            s.refused.push(format!("column {} is generated (Postgres works it out; it can't be written back)", v(1)));
        }
        if !v(3).is_empty() || v(5) == "t" {
            s.seqs.push((s.cols.len(), r[7].clone().filter(|_| v(8) == "t")));
        }
        if let Ok(p) = v(6).parse::<i64>() {
            keys.entry(v(0)).or_default().push((p, s.cols.len()));
        }
        s.cols.push(attach::Col { name: v(1), ty: v(2), always: v(3) == "a" });
    }
    for s in &mut out {
        let mut k = keys.remove(&s.oid).unwrap_or_default();
        k.sort();
        s.key = k.into_iter().map(|(_, i)| i).collect();
        if s.key.is_empty() && s.refused.is_empty() {
            s.refused.push("no primary key (a row written back must be found by one)".into());
        }
    }
    Ok(out)
}

/// The order to write tables in, by position in `found`: a table after those its foreign keys refer to (a cycle is
/// broken where it closes).
fn parents_first(pg: &mut Client, found: &[Source]) -> Result<Vec<usize>, String> {
    let oids = format!("({})", found.iter().map(|s| s.oid.as_str()).collect::<Vec<_>>().join(", "));
    let refs = pg
        .query(&format!(
            "select conrelid::text, confrelid::text from pg_constraint where contype = 'f' and conrelid in {oids} \
             and confrelid in {oids} and conrelid <> confrelid"
        ))
        .map_err(|e| format!("reading Postgres: {e}"))?;
    let at = |oid: &str| found.iter().position(|s| s.oid == oid);
    let mut parents: Vec<Vec<usize>> = vec![vec![]; found.len()];
    for r in refs {
        if let (Some(c), Some(p)) = (r[0].as_deref().and_then(at), r[1].as_deref().and_then(at)) {
            parents[c].push(p);
        }
    }
    let mut order = vec![];
    let mut state = vec![0u8; found.len()]; // 0 not seen, 1 on the way, 2 placed
    fn visit(i: usize, parents: &[Vec<usize>], state: &mut [u8], order: &mut Vec<usize>) {
        if state[i] != 0 {
            return;
        }
        state[i] = 1;
        for &p in &parents[i] {
            visit(p, parents, state, order);
        }
        state[i] = 2;
        order.push(i);
    }
    for i in 0..found.len() {
        visit(i, &parents, &mut state, &mut order);
    }
    Ok(order)
}

/// Overwrites a password's bytes before it is let go of, so the memory freed doesn't hold it.
fn wipe(password: &mut Option<String>) {
    if let Some(p) = password.take() {
        let mut b = p.into_bytes();
        for x in b.iter_mut() {
            // (volatile: a write to memory about to be freed is otherwise left out)
            unsafe { std::ptr::write_volatile(x, 0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{collation_warning, index_losses, unqualify};

    #[test]
    fn a_vector_operator_class_is_kept() {
        let losses = index_losses("CREATE INDEX d_e ON lab.d USING hnsw (e extensions.vector_cosine_ops)");
        assert!(losses.is_empty(), "{losses:?}");
    }

    #[test]
    fn extension_names_lose_the_schema_postgres_kept_them_in() {
        let schemas = vec!["extensions".to_string()];
        let names: std::collections::HashSet<String> =
            ["vector", "vector_cosine_ops", "citext", "uuid_generate_v4"].iter().map(|s| s.to_string()).collect();
        let cases = [
            (
                "create table d (e extensions.vector(64), m \"extensions\".citext)",
                "create table d (e vector(64), m citext)",
            ),
            (
                "CREATE INDEX i ON lab.d USING hnsw (e extensions.vector_cosine_ops)",
                "CREATE INDEX i ON lab.d USING hnsw (e vector_cosine_ops)",
            ),
            ("id uuid default extensions.uuid_generate_v4()", "id uuid default uuid_generate_v4()"),
            ("e extensions.vector(3)[]", "e vector(3)[]"),
            // not an extension's: kept as written
            ("select * from extensions.my_table", "select * from extensions.my_table"),
            ("select my_extensions.vector from t", "select my_extensions.vector from t"),
            ("select a.extensions.vector", "select a.extensions.vector"),
        ];
        for (sql, want) in cases {
            assert_eq!(unqualify(sql, &schemas, &names), want, "{sql}");
        }
        assert!(matches!(unqualify("select 1", &schemas, &names), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn a_trigram_operator_class_is_kept() {
        assert!(index_losses("CREATE INDEX i ON public.d USING gin (name gin_trgm_ops)").is_empty());
        assert!(index_losses("CREATE INDEX i ON public.d USING gist (name gist_trgm_ops)").is_empty());
        let l = index_losses("CREATE INDEX i ON public.d USING btree (name text_pattern_ops)");
        assert!(l.iter().any(|l| l.contains("text_pattern_ops")), "{l:?}");
    }

    #[test]
    fn a_source_collation_other_than_bytes_is_said() {
        for (collate, provider, locale) in [
            ("C", "c", None),
            ("POSIX", "c", None),
            ("C.UTF-8", "c", None),
            ("C.utf8", "c", None),
            ("C", "b", Some("C.UTF-8")),
        ] {
            assert_eq!(collation_warning(collate, provider, locale, &|_| true), None, "{collate} {provider}");
        }
        let has = |names: &'static [&'static str]| move |c: &str| names.contains(&c);
        let w = collation_warning("en_US.UTF-8", "c", None, &has(&["en-US-x-icu"])).unwrap();
        assert!(w.contains("en_US.UTF-8 (libc)") && w.contains("COLLATE \"en-US-x-icu\""), "{w}");
        // the language's when the locale's isn't one
        let w = collation_warning("C", "i", Some("de-DE-u-co-phonebk"), &has(&["de-x-icu"])).unwrap();
        assert!(w.contains("the ICU locale de-DE-u-co-phonebk") && w.contains("COLLATE \"de-x-icu\""), "{w}");
        // neither: none is offered that would be refused
        let w = collation_warning("mr_IN.UTF-8", "c", None, &has(&[])).unwrap();
        assert!(w.contains("no collation for that locale") && !w.contains("mr-IN-x-icu\")"), "{w}");
    }
}
