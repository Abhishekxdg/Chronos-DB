//! An attached folder: its tables are copies of tables in a Postgres database the user already runs
//! (`chronos attach`, see `docs/designs/postgres-adapter.md`), and Postgres stays their source of truth.
//! Agents work in worlds as anywhere; a merge into main is first written to Postgres, in one transaction, and
//! only then made in Chronos.
//!
//!   merge world x (into main)
//!     ├─ the Chronos side as any merge: conflicts, constraints, policy, checks, stale reads (locks held)
//!     ├─ Postgres, one transaction:  settings ─> source identity ─> schema unchanged? ─> per table, parents first:
//!     │    lock the rows ─> compare each with main's value ─(any differs)─> roll back; main takes Postgres's
//!     │                                                                     values for them; conflict error
//!     │    ─> update, insert (parents first), delete (children first), each RETURNING
//!     │    ─> merge checks on attached tables, on Postgres as the writes leave it (rows found: roll back, held)
//!     │    ─> marker row ─> COMMIT
//!     ├─ the merge published in Chronos
//!     └─ rows Postgres changed as it wrote them (a trigger, a default): main takes them too
//!
//!   merge world x dry run (into main): the plan as any dry run, then, after its locks (see `dry_run`)
//!     └─ Postgres, one read-only transaction: settings ─> source identity ─> schema unchanged? ─> compare each row
//!        it would write with main's value (no locks: a snapshot) ─> the tables' triggers ─> ROLLBACK
//!        (nothing written there, and main doesn't take Postgres's rows)
//!
//! Comparing: Postgres casts each value Chronos holds to the column's type and both sides to text, under the same
//! session settings, so a row nobody changed always compares equal. Main only ever holds what Postgres last sent
//! (writes to main are refused in an attached folder), so a row that differs changed in Postgres since.
//!
//! Recovery: before COMMIT, the merge's id and the rows it touches go to `attached.pending`. A crash, or a connection
//! lost during COMMIT, leaves it there; `resolve` then inserts the same id into `chronos.merges` from a new
//! connection, which waits for the first transaction to end: a duplicate key means it committed (Chronos makes the
//! merge, or takes the rows from Postgres), success means it didn't (rolled back again). Until that is known, merges
//! into main are refused; reads, forks and work in worlds go on.

use crate::pgclient::{self, Client, Url, pg_error, text_array};
use crate::{Core, Error, MAIN, Result, State, Val};
use serde_json::{Value as Json, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::time::{Duration, Instant};

/// The folder's file saying it is attached, and to what (no secret in it).
pub(crate) const FILE: &str = "attached";
/// A write-back whose COMMIT may or may not have happened (see `resolve`).
const PENDING: &str = "attached.pending";
/// The most rows one merge writes to Postgres by default (`CHRONOS_ATTACH_MAX_ROWS` sets it, up to `MAX_ROWS_CAP`):
/// every one stays locked there until the merge commits.
pub const MAX_ROWS: usize = 50_000;
pub const MAX_ROWS_CAP: usize = 1_000_000;
/// Rows of one table a write-back statement sends at once: a bigger merge is sent in parts, in the same transaction.
const CHUNK: usize = 5_000;

/// The most rows one merge writes to Postgres (see `MAX_ROWS`).
pub fn max_rows() -> usize {
    std::env::var("CHRONOS_ATTACH_MAX_ROWS")
        .ok()
        .and_then(|v| v.trim().replace('_', "").parse::<usize>().ok())
        .filter(|n| *n > 0)
        .map_or(MAX_ROWS, |n| n.min(MAX_ROWS_CAP))
}
/// How long a write-back waits for a row lock, a statement, or between its own statements.
const LOCK_TIMEOUT: &str = "5s";
const STATEMENT_TIMEOUT: &str = "60s";
const IDLE_TIMEOUT: &str = "60s";
/// Session settings that decide how Postgres writes values as text, fixed in every transaction attach and
/// write-back run (SET LOCAL: a pooler in transaction mode may hand each transaction another connection), so the
/// same value is always the same text, whatever the role's or database's own settings are.
pub const SETTINGS: &str = "set local timezone = 'UTC'; set local datestyle = 'ISO, YMD'; \
     set local intervalstyle = 'postgres'; set local extra_float_digits = 1; set local bytea_output = 'hex'; \
     set local search_path = public";
/// What pg_stat_activity shows for Chronos's connections.
const APP: &str = "chronos attach";
/// How long the connection kept to the source (see `Lease`) may go unused before it is closed, so a Postgres that
/// scales to zero (Neon, Supabase) can; the next use connects again.
const KEEP_IDLE: Duration = Duration::from_secs(300);
/// How often a kept connection is looked at for having gone unused that long.
const KEEP_CHECK: Duration = Duration::from_secs(30);

/// A table of the folder, attached.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    /// as Chronos's rows name it (`orders`, `app.orders`)
    pub name: String,
    /// as Chronos's SQL names it (`"orders"`, `"app"."orders"`)
    pub sql: String,
    /// as Postgres's SQL names it (`"public"."orders"`)
    pub pg: String,
    pub cols: Vec<Col>,
    /// the primary key's columns, by position in `cols`, in the key's order
    pub key: Vec<usize>,
    /// columns whose values come from a sequence (serial, identity, DEFAULT nextval): see `Seq`
    pub seqs: Vec<Seq>,
}

/// A column whose default takes values from a Postgres sequence. In the folder its default takes them from that
/// very sequence, reserved a block at a time (see `values`), so a value given here is never given by Postgres too;
/// a merge writes back only values so reserved (see `check_ids`).
#[derive(Clone, Debug, PartialEq)]
pub struct Seq {
    /// by position in the table's `cols`
    pub col: usize,
    /// Postgres's sequence, as regclass writes it; None: one attach can't take values from (no USAGE on it, or a
    /// default it can't read), and the table takes no inserts
    pub pg: Option<String>,
    /// the counter the column's default takes values from here (see `sql::column_counter`)
    pub counter: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Col {
    pub name: String,
    /// Postgres's `format_type` of it, under `SETTINGS`' search_path: what values are cast to
    pub ty: String,
    /// GENERATED ALWAYS AS IDENTITY: never in an UPDATE's SET
    pub always: bool,
}

/// An attached folder's record (`FILE`).
#[derive(Debug)]
pub struct Attached {
    /// random; also the row `attach:<id>` in Postgres's `chronos.merges`, which says it is the database attached
    pub id: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    /// when the copy was taken (ms since 1970)
    pub at: u64,
    /// parents before children (foreign keys among them); a refresh widens a table Postgres added columns to (see
    /// `widen`), under the write-back lock, so a write-back sees one shape from start to end
    tables: RwLock<Arc<Vec<Table>>>,
    dir: PathBuf,
    /// held by a merge into main from its write-back until main holds what Postgres sent back: one at a time
    lock: Mutex<()>,
    /// values reserved from Postgres's sequences (see `values`)
    ids: Mutex<Ids>,
    /// when main last took Postgres's rows (`refresh`; ms since 1970), 0: never since the attach
    refreshed: std::sync::atomic::AtomicU64,
    /// columns a refresh added to main whose default or NOT NULL waits for their values: made so once main has
    /// Postgres's values (see `settle`), at the end of that refresh or, if it failed, the next
    unsettled: Mutex<Vec<Unsettled>>,
    /// the connection to the source kept between write-backs, refreshes and dry runs (see `Lease`)
    conn: Arc<Conn>,
}

/// The connection to the source kept from its last use (see `Lease`): taken out for a use and put back after, never
/// used in place, so its mutex is held only for that and orders with no other lock. Two uses at once (a dry run
/// beside a merge) each have one; the second put back closes the first.
#[derive(Default)]
struct Conn {
    kept: Mutex<Option<Kept>>,
    /// a thread closes the kept connection once unused for `KEEP_IDLE` (see `close_idle`)
    watched: AtomicBool,
}

/// A connection kept, the URL it was made to, and since when it is unused.
struct Kept {
    pg: Client,
    url: Url,
    since: Instant,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Conn")
    }
}

impl Conn {
    fn take(&self) -> Option<Kept> {
        self.kept.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    /// Keeps `pg` for the next use, closing any other kept meanwhile, and has a thread watch it for going unused.
    fn put(self: &Arc<Conn>, pg: Client, url: Url) {
        let old = {
            let mut kept = self.kept.lock().unwrap_or_else(|p| p.into_inner());
            let old = kept.replace(Kept { pg, url, since: Instant::now() });
            // (under the lock: the watcher stops only when it finds none kept, under it too)
            if !self.watched.swap(true, Ordering::SeqCst) {
                let conn = Arc::downgrade(self);
                let started =
                    std::thread::Builder::new().name("chronos-attach-idle".into()).spawn(move || close_idle(conn));
                if started.is_err() {
                    self.watched.store(false, Ordering::SeqCst);
                }
            }
            old
        };
        drop(old); // (outside the lock: closing it says goodbye to the server)
    }
}

/// Closes the connection `conn` keeps once unused for `KEEP_IDLE`; ends when none is kept, or the folder is closed.
fn close_idle(conn: Weak<Conn>) {
    loop {
        std::thread::sleep(KEEP_CHECK);
        let Some(conn) = conn.upgrade() else { return };
        let idle = {
            let mut kept = conn.kept.lock().unwrap_or_else(|p| p.into_inner());
            let Some(since) = kept.as_ref().map(|k| k.since) else {
                conn.watched.store(false, Ordering::SeqCst);
                return;
            };
            if since.elapsed() >= KEEP_IDLE { kept.take() } else { None }
        };
        drop(idle);
    }
}

/// A connection to the source for one use (a write-back, a refresh, a dry run, a check of reads, ids reserved): the
/// one kept from the last use (see `Conn`) when it is to the URL `source_url` gives now, unused for less than
/// `KEEP_IDLE`, and whole; else a new one. A merge then costs its statements alone, not a connect, a TLS handshake
/// and a login too (a third of a small merge's round trips). Nothing of one use is left for the next: each use
/// starts a transaction (`begin`) and settings are SET LOCAL, or it is one statement on its own; let go, the
/// connection is kept only with no transaction open (an error path rolls back first) and nothing broken, else closed.
pub(crate) struct Lease {
    /// Some until dropped
    pg: Option<Client>,
    url: Url,
    /// kept from an earlier use: the server may have closed it since (see `first`)
    reused: bool,
    conn: Arc<Conn>,
}

impl Attached {
    /// A connection to the source for one use (see `Lease`).
    fn lease(&self) -> std::result::Result<Lease, String> {
        let url = source_url()?;
        let (pg, reused) = match self.conn.take() {
            Some(k) if k.url == url && k.since.elapsed() < KEEP_IDLE && k.pg.status == b'I' && !k.pg.broken => {
                (k.pg, true)
            }
            other => {
                drop(other);
                (open(&url)?, false)
            }
        };
        Ok(Lease { pg: Some(pg), url, reused, conn: self.conn.clone() })
    }
}

impl Lease {
    /// Runs `f`, the use's first statement. On a kept connection the server closed while it was unused (a restart, a
    /// pooler's or `idle_session_timeout`, `pg_terminate_backend`), the connection fails then, and `f` runs once
    /// more on a new one: only the first statement, which nothing was sent before on this connection and which
    /// writes nothing (or, reserving ids, only leaves a gap). After it, a lost connection is the use's own error,
    /// never retried: a write may have been sent.
    fn first<T>(&mut self, mut f: impl FnMut(&mut Client) -> io::Result<T>) -> io::Result<T> {
        match f(self) {
            Err(e) if self.reused && pg_error(&e).is_none() => {
                self.reused = false;
                let url = &self.url;
                let fresh = Client::connect_as(url, APP)
                    .map_err(|e| io::Error::new(e.kind(), format!("Postgres at {}:{}: {e}", url.host, url.port)))?;
                self.pg = Some(fresh); // (the dead one closed)
                f(self)
            }
            out => out,
        }
    }
}

impl std::ops::Deref for Lease {
    type Target = Client;
    fn deref(&self) -> &Client {
        self.pg.as_ref().expect("a lease holds its connection until dropped")
    }
}

impl std::ops::DerefMut for Lease {
    fn deref_mut(&mut self) -> &mut Client {
        self.pg.as_mut().expect("a lease holds its connection until dropped")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(pg) = self.pg.take()
            && pg.status == b'I'
            && !pg.broken
        {
            self.conn.put(pg, self.url.clone());
        }
    }
}

/// A column a refresh added whose default or NOT NULL waits for its values (see `Attached::unsettled`): its table,
/// its name, the default to set, and NOT NULL.
type Unsettled = (usize, String, Option<String>, bool);

/// The file of the values reserved from Postgres's sequences: what a merge may write back to their columns.
const IDS: &str = "attached.ids";
/// Values reserved from a Postgres sequence at once (`nextval` in one statement), at least.
const BLOCK: usize = 1000;

/// Values reserved from Postgres's sequences: those not handed out yet, by counter (forgotten at a restart: a gap,
/// as Postgres's own cache leaves), and every one ever reserved, by sequence, as runs (first value, step, count), kept
/// in `IDS`.
#[derive(Debug, Default)]
struct Ids {
    pool: HashMap<String, std::collections::VecDeque<i64>>,
    runs: BTreeMap<String, Vec<(i64, i64, i64)>>,
}

impl Ids {
    fn load(dir: &Path) -> io::Result<Ids> {
        let text = match std::fs::read_to_string(dir.join(IDS)) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Ids::default()),
            Err(e) => return Err(e),
        };
        let runs = serde_json::from_str(&text).map_err(|e| other(format!("{}: {e}", dir.join(IDS).display())))?;
        Ok(Ids { pool: HashMap::new(), runs })
    }

    /// Whether `v` was reserved from sequence `seq`.
    fn reserved(&self, seq: &str, v: i64) -> bool {
        self.runs.get(seq).is_some_and(|runs| {
            runs.iter().any(|&(first, step, n)| {
                let d = v.wrapping_sub(first);
                step != 0 && d % step == 0 && (0..n).contains(&(d / step))
            })
        })
    }

    /// `vals` (in the order Postgres gave them) as runs of one step each.
    fn add(&mut self, seq: &str, vals: &[i64]) {
        let runs = self.runs.entry(seq.to_string()).or_default();
        let mut i = 0;
        while i < vals.len() {
            let step = if i + 1 < vals.len() { vals[i + 1].wrapping_sub(vals[i]) } else { 1 };
            let mut n = 1;
            while i + n < vals.len() && vals[i + n].wrapping_sub(vals[i + n - 1]) == step {
                n += 1;
            }
            runs.push((vals[i], if n == 1 { 1 } else { step }, n as i64));
            i += n;
        }
    }
}

thread_local! {
    /// This thread holds `Attached::lock` (a merge made while resolving a pending one takes it no second time).
    static HOLDING: Cell<bool> = const { Cell::new(false) };
    /// This thread makes a merge Postgres already has: no write-back (see `resolve`).
    static SKIP: Cell<bool> = const { Cell::new(false) };
    /// What the merge just made in `merge_locked` left for after its locks (see `After`).
    static AFTER: RefCell<Option<After>> = const { RefCell::new(None) };
}

/// What a write-back leaves to do once the merge's own locks are let go: main takes Postgres's rows (its values
/// after triggers and defaults, or, on a conflict, its current ones), and a committed merge's pending record goes.
pub(crate) struct After {
    rows: Vec<Stored>,
    /// the merge committed in Postgres: its pending record is removed once main has the rows
    committed: bool,
    /// tables whose rows Postgres changed besides those `rows` hold (a trigger wrote or deleted some: see `others`):
    /// main takes them by a refresh of those tables alone before the pending record goes
    refresh: Vec<usize>,
}

/// A row as Postgres has it: its key and its columns' text (None: gone from Postgres).
#[derive(Clone, Debug)]
struct Stored {
    table: usize,
    key: Vec<Option<String>>,
    row: Option<Vec<Option<String>>>,
}

fn other(m: impl Into<String>) -> io::Error {
    io::Error::other(m.into())
}

fn random_id() -> String {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut b).expect("random numbers");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A name as Postgres's SQL writes it.
pub fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A literal as SQL writes it (standard strings: only the quote is doubled).
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl Table {
    fn to_json(&self) -> Json {
        json!({
            "name": self.name, "sql": self.sql, "pg": self.pg, "key": self.key,
            "seqs": self.seqs.iter().map(|q| json!({"col": q.col, "pg": q.pg, "counter": q.counter})).collect::<Vec<_>>(),
            "cols": self.cols.iter().map(|c| json!({"name": c.name, "ty": c.ty, "always": c.always})).collect::<Vec<_>>(),
        })
    }

    fn from_json(j: &Json) -> Option<Table> {
        let s = |k: &str| j[k].as_str().map(String::from);
        Some(Table {
            name: s("name")?,
            sql: s("sql")?,
            pg: s("pg")?,
            key: j["key"].as_array()?.iter().map(|k| k.as_u64().map(|k| k as usize)).collect::<Option<_>>()?,
            seqs: match j["seqs"].as_array() {
                None => vec![],
                Some(a) => a
                    .iter()
                    .map(|q| {
                        Some(Seq {
                            col: q["col"].as_u64()? as usize,
                            pg: q["pg"].as_str().map(String::from),
                            counter: q["counter"].as_str().map(String::from),
                        })
                    })
                    .collect::<Option<_>>()?,
            },
            cols: j["cols"]
                .as_array()?
                .iter()
                .map(|c| {
                    Some(Col {
                        name: c["name"].as_str()?.into(),
                        ty: c["ty"].as_str()?.into(),
                        always: c["always"].as_bool().unwrap_or(false),
                    })
                })
                .collect::<Option<_>>()?,
        })
    }
}

impl Attached {
    /// A new record for the database at `url`, with its tables: written by `save` once the copy is in.
    pub fn new(dir: &Path, url: &Url, tables: Vec<Table>) -> Attached {
        Attached {
            id: random_id(),
            host: url.host.clone(),
            port: url.port,
            database: url.database.clone(),
            at: crate::now_ms(),
            tables: RwLock::new(Arc::new(tables)),
            dir: dir.to_path_buf(),
            lock: Mutex::new(()),
            ids: Mutex::new(Ids::default()),
            refreshed: Default::default(),
            unsettled: Mutex::default(),
            conn: Arc::default(),
        }
    }

    /// The folder's record, if it is attached.
    pub fn load(dir: &Path) -> io::Result<Option<Attached>> {
        let text = match std::fs::read_to_string(dir.join(FILE)) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let bad = || other(format!("{}: not an attached folder's record", dir.join(FILE).display()));
        let j: Json = serde_json::from_str(&text).map_err(|_| bad())?;
        Ok(Some(Attached {
            id: j["id"].as_str().ok_or_else(bad)?.into(),
            host: j["host"].as_str().unwrap_or_default().into(),
            port: j["port"].as_u64().unwrap_or(5432) as u16,
            database: j["database"].as_str().unwrap_or_default().into(),
            at: j["at"].as_u64().unwrap_or(0),
            tables: RwLock::new(Arc::new(
                j["tables"]
                    .as_array()
                    .ok_or_else(bad)?
                    .iter()
                    .map(Table::from_json)
                    .collect::<Option<_>>()
                    .ok_or_else(bad)?,
            )),
            ids: Mutex::new(Ids::load(dir)?),
            refreshed: j["refreshed"].as_u64().unwrap_or(0).into(),
            unsettled: Mutex::default(),
            dir: dir.to_path_buf(),
            lock: Mutex::new(()),
            conn: Arc::default(),
        }))
    }

    pub fn save(&self) -> io::Result<()> {
        let mut j = json!({
            "id": self.id, "host": self.host, "port": self.port, "database": self.database, "at": self.at,
            "tables": self.tables().iter().map(Table::to_json).collect::<Vec<_>>(),
        });
        let refreshed = self.refreshed_at();
        if refreshed > 0 {
            j["refreshed"] = refreshed.into();
        }
        crate::disk::write_file(&self.dir.join(FILE), serde_json::to_string_pretty(&j)?.as_bytes())
    }

    /// When main last took Postgres's rows (see `refresh`), 0: not since the attach.
    pub fn refreshed_at(&self) -> u64 {
        self.refreshed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How old main's rows that no merge touched may be: since the last refresh, or the attach (ms since 1970).
    pub fn as_of(&self) -> u64 {
        self.refreshed_at().max(self.at)
    }

    /// Records a refresh that read Postgres at `at`.
    fn refreshed(&self, at: u64) -> io::Result<()> {
        self.refreshed.store(at, std::sync::atomic::Ordering::Relaxed);
        self.save()
    }

    /// The attached tables, as recorded now: parents before children. A table's place never changes; its columns
    /// may grow (see `widen`), so a caller reading several of them takes this once.
    pub fn tables(&self) -> Arc<Vec<Table>> {
        self.tables.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The attached table rows of Chronos name `name` belong to.
    pub fn table(&self, name: &str) -> Option<usize> {
        self.tables().iter().position(|t| t.name == name)
    }

    /// The source, as said at attach, for messages (no secret).
    pub fn source(&self) -> String {
        format!("{} on {}:{}", self.database, self.host, self.port)
    }

    /// The marker row's id saying a Postgres database is this folder's source.
    fn marker(&self) -> String {
        format!("attach:{}", self.id)
    }

    fn pending_path(&self) -> PathBuf {
        self.dir.join(PENDING)
    }

    /// Whether a write-back's outcome is still unknown (see `resolve`).
    pub fn pending(&self) -> bool {
        self.pending_path().exists()
    }

    /// Takes the write-back lock, unless this thread holds it already.
    fn hold(&self) -> Option<Hold<'_>> {
        if HOLDING.get() {
            return None;
        }
        let g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        HOLDING.set(true);
        Some(Hold(g))
    }
}

/// `Attached::lock`, held by this thread until dropped.
pub(crate) struct Hold<'a>(#[allow(dead_code)] MutexGuard<'a, ()>);

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        HOLDING.set(false);
    }
}

/// The statements an administrator runs when the role attach connects as can't make Chronos's table itself.
pub fn setup_statements(role: &str) -> String {
    format!(
        "create schema if not exists chronos;\n\
         create table if not exists chronos.merges (id text primary key, at timestamptz not null default now());\n\
         grant usage on schema chronos to {r};\n\
         grant select, insert on chronos.merges to {r};",
        r = q(role)
    )
}

/// The connection string of the source: `CHRONOS_SOURCE_URL`, or the file `CHRONOS_SOURCE_FILE` names, which only
/// its owner may read. Never a command's argument, and never written anywhere.
pub fn source_url() -> std::result::Result<Url, String> {
    let text = match (std::env::var("CHRONOS_SOURCE_URL"), std::env::var("CHRONOS_SOURCE_FILE")) {
        (Ok(u), _) if !u.is_empty() => u,
        (_, Ok(f)) if !f.is_empty() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&f).map_err(|e| format!("{f}: {e}"))?.permissions().mode();
                if mode & 0o077 != 0 {
                    return Err(format!(
                        "{f} can be read by others (mode {:o}): chmod 600 it, as it holds a password",
                        mode & 0o777
                    ));
                }
            }
            std::fs::read_to_string(&f).map_err(|e| format!("{f}: {e}"))?.trim().to_string()
        }
        _ => {
            return Err("the Postgres it is attached to isn't given: set CHRONOS_SOURCE_URL, or CHRONOS_SOURCE_FILE \
                        to a file holding the URL (chronos serve --source-file <file>)"
                .into());
        }
    };
    pgclient::parse(&text)
}

/// A new connection to the source, as `source_url` gives it (most uses take the kept one: see `Lease`).
fn connect() -> std::result::Result<Client, String> {
    open(&source_url()?)
}

/// A new connection to `url`.
fn open(url: &Url) -> std::result::Result<Client, String> {
    Client::connect_as(url, APP).map_err(|e| format!("Postgres at {}:{}: {e}", url.host, url.port))
}

/// A Postgres failure as the merge's error. `retry`: nothing was written, and the same merge may simply be run again
/// (Postgres unreachable, a lock or statement timeout, a deadlock, a transaction ended for being idle).
fn failed(what: &str, e: &io::Error) -> Error {
    let (code, retry) = match pg_error(e) {
        Some(p) => (p.code.as_str(), matches!(p.code.as_str(), "55P03" | "57014" | "40P01" | "40001" | "25P03")),
        None => ("08006", true),
    };
    let detail = pg_error(e).and_then(|p| p.detail.clone()).map_or(String::new(), |d| format!(" ({d})"));
    let again = if retry { "; nothing was written: safe to merge again" } else { "; nothing was written" };
    Error::Constraint {
        code: if retry { "40001" } else { "23000" },
        message: format!("Postgres, {what}: {e}{detail}{again}"),
    }
    .with_code(code)
}

impl Error {
    /// A Postgres error's own SQLSTATE where it is one Chronos knows, else the one given.
    fn with_code(self, code: &str) -> Error {
        let known: &[&'static str] = &["23505", "23503", "23502", "23514", "23P01", "22001", "22003", "22P02", "42501"];
        match self {
            Error::Constraint { message, code: c } => {
                Error::Constraint { code: known.iter().find(|k| **k == code).copied().unwrap_or(c), message }
            }
            e => e,
        }
    }
}

/// For crash tests (tests/attach.rs), in debug builds: with CHRONOS_ATTACH_CRASH=<at>, the process aborts at that step
/// of a write-back (`before_commit`: the pending record written, COMMIT not sent; `after_commit`: Postgres committed,
/// nothing published here; `before_trigger_refresh`: the merge made here, the tables its triggers wrote not yet
/// refreshed), as a kill -9 there would.
fn crash_point(at: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("CHRONOS_ATTACH_CRASH").is_ok_and(|v| v == at) {
        std::process::abort();
    }
    #[cfg(not(debug_assertions))]
    let _ = at;
}

fn refused(m: impl Into<String>) -> Error {
    Error::Constraint { code: "55000", message: m.into() }
}

/// Rolls back what `pg` has open, if anything (on a broken connection, nothing can be sent: the server rolls it back
/// as the connection closes).
fn rollback(pg: &mut Client) {
    if pg.status != b'I' && !pg.broken {
        let _ = pg.query("rollback");
    }
}

// ---------------------------------------------------------------- ids from Postgres's sequences

/// The next `n` values of counter `counter` (a sequence's id, or a serial's `table.column`: see
/// `sql::column_counter`), when it is an attached table's: taken from Postgres's own sequence, reserved
/// `BLOCK` at a time and recorded in the folder before any is handed out, so a row a world inserts gets an id
/// Postgres will never give another row, and keeps it at merge (the id a person reviewed is the one written).
/// None for any other counter. Err: Postgres couldn't be reached, or won't give values.
pub(crate) fn values(core: &Core, counter: &str, n: usize) -> Option<std::result::Result<Vec<i64>, String>> {
    let att = core.attached.as_deref()?;
    let tables = att.tables();
    let (t, sq) = tables
        .iter()
        .find_map(|t| t.seqs.iter().find(|sq| sq.counter.as_deref() == Some(counter)).map(|sq| (t, sq)))?;
    let Some(seq) = &sq.pg else {
        return Some(Err(format!(
            "{}.{} takes its values from a sequence in Postgres this folder can't take values from (the role needs \
             USAGE on it): give rows of {} no new {} here",
            t.sql,
            q(&t.cols[sq.col].name),
            t.sql,
            q(&t.cols[sq.col].name)
        )));
    };
    Some(att.take(seq, counter, n))
}

impl Attached {
    /// `n` values of Postgres's sequence `seq` for counter `counter`: from those reserved, reserving more first.
    fn take(&self, seq: &str, counter: &str, n: usize) -> std::result::Result<Vec<i64>, String> {
        let mut ids = self.ids.lock().unwrap_or_else(|p| p.into_inner());
        let have = ids.pool.get(counter).map_or(0, |p| p.len());
        if have < n {
            let got = reserve(self, seq, (n - have).max(BLOCK))?;
            ids.add(seq, &got);
            let runs = serde_json::to_string(&ids.runs).map_err(|e| e.to_string())?;
            // on disk before any is handed out: a merge after a restart still knows them
            crate::disk::write_file(&self.dir.join(IDS), runs.as_bytes()).map_err(|e| format!("{IDS}: {e}"))?;
            ids.pool.entry(counter.to_string()).or_default().extend(got);
        }
        let pool = ids.pool.get_mut(counter).expect("filled above");
        Ok(pool.drain(..n).collect())
    }

    /// Whether `v` is a value this folder reserved from Postgres's sequence `seq`.
    fn reserved(&self, seq: &str, v: i64) -> bool {
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).reserved(seq, v)
    }
}

/// `n` values of Postgres's sequence `seq`, taken in one statement (gaps in the sequence are the cost).
fn reserve(att: &Attached, seq: &str, n: usize) -> std::result::Result<Vec<i64>, String> {
    let mut pg = att.lease()?;
    let n = n.to_string();
    let rows = pg
        .first(|pg| {
            pg.query_params(
                "select nextval($1::regclass) from generate_series(1, $2::int)",
                &[Some(seq), Some(n.as_str())],
            )
        })
        .map_err(|e| format!("reserving ids from Postgres's sequence {seq}: {e}"))?;
    rows.iter()
        .map(|r| r[0].as_deref().and_then(|v| v.parse().ok()).ok_or_else(|| format!("{seq} gave no value")))
        .collect()
}

/// Refuses a row whose sequence-fed column gets a value (inserted, or changed) that this folder didn't reserve from
/// Postgres's sequence: one given by hand, which Postgres could give another row later.
fn check_ids(
    att: &Attached,
    world: &str,
    t: &Table,
    before: Option<&[Option<String>]>,
    after: Option<&[Option<String>]>,
) -> Result<()> {
    let Some(after) = after else { return Ok(()) };
    for sq in &t.seqs {
        let new = after[sq.col].as_deref();
        let Some(v) = new.filter(|_| before.is_none_or(|b| b[sq.col].as_deref() != new)) else { continue };
        let ok = sq.pg.as_deref().is_some_and(|seq| v.parse::<i64>().is_ok_and(|v| att.reserved(seq, v)));
        if !ok {
            let col = &t.cols[sq.col].name;
            return Err(refused(format!(
                "{world} gives {}.{} the value {v}, which isn't one this folder reserved from Postgres's sequence{}: \
                 Postgres could give it to another row too. Leave the column to its default (it takes values \
                 reserved from Postgres); nothing was merged",
                t.sql,
                q(col),
                sq.pg.as_deref().map_or(String::new(), |s| format!(" {s}"))
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- the merge

/// A row's columns as text (None: NULL).
type Texts = Vec<Option<String>>;

/// One row a merge changes: its key in Chronos, and the text of each column as main holds it now and as the
/// merge leaves it (None: no row).
struct Row {
    key: String,
    table: usize,
    before: Option<Vec<Option<String>>>,
    after: Option<Vec<Option<String>>>,
    /// `after` keeps main's values of columns the world's own table hasn't (see `rows_of`): main takes the row back
    /// from Postgres once written, as the world's row there holds something else
    kept: bool,
    /// a row the world inserts: the columns its table hadn't (added in Postgres after its fork, see `widen`), left
    /// out of the INSERT so Postgres fills them as it fills its own rows' (its default, now)
    unseen: Vec<usize>,
}

impl Row {
    fn key_texts(&self, t: &Table) -> Vec<Option<String>> {
        let r = self.before.as_ref().or(self.after.as_ref()).expect("a row on one side at least");
        t.key.iter().map(|&i| r[i].clone()).collect()
    }
}

/// A stored row's columns in the attached table's order, as text.
fn texts(core: &Core, state: &State, t: &Table, key: &str, raw: &Val) -> Result<Vec<Option<String>>> {
    let schema = core.val(state, &format!("{}/{}", crate::sql::SCHEMAS, t.name))?;
    let by_name = schema
        .as_ref()
        .and_then(|s| crate::sql::row_texts(s, raw))
        .ok_or_else(|| refused(format!("row {} isn't a row of an attached table", crate::sql::show_key(key))))?;
    let mut by_name: HashMap<String, Option<String>> = by_name.into_iter().collect();
    t.cols
        .iter()
        .map(|c| {
            by_name.remove(&c.name).ok_or_else(|| {
                refused(format!("table {} has no column {} here: it changed shape since the attach", t.name, c.name))
            })
        })
        .collect()
}

/// `t` as its table is at `state`: without the columns a refresh added after it (see `widen`), so a world forked before
/// reads and compares its rows as it saw them. (Added columns come last: the others keep their places, and the key's.)
fn as_at(core: &Core, state: &State, t: &Table) -> Result<Table> {
    let schema = core.val(state, &format!("{}/{}", crate::sql::SCHEMAS, t.name))?;
    let had = schema.as_ref().and_then(crate::sql::column_names).unwrap_or_default();
    let mut t = t.clone();
    if !had.is_empty() {
        t.cols.retain(|c| had.contains(&c.name));
    }
    Ok(t)
}

/// Whether merges into `name` in this database write back to Postgres now.
pub(crate) fn writes_back(core: &Core, name: &str, tx: bool) -> bool {
    name == MAIN && !tx && core.attached.is_some() && core.me.get().is_some() && !crate::RECOVERING.get() && !SKIP.get()
}

/// The rows of attached tables a merge into main changes (`apply`, from the parent's state `theirs`; the world's
/// rows as `ours` holds them), as text, refused as the write-back would refuse them: a table's shape changed, a
/// table that isn't attached, a sequence-fed value not reserved from Postgres, more than `MAX_ROWS`.
fn rows_of(
    core: &Core,
    att: &Attached,
    world: &str,
    theirs: &State,
    ours: &State,
    apply: &[crate::Change],
) -> Result<Vec<Row>> {
    let tables = att.tables();
    let mut rows: Vec<Row> = vec![];
    // per table, the columns the world's own table hasn't: added in Postgres after its fork (see `widen`)
    let mut unseen: HashMap<usize, Vec<usize>> = HashMap::new();
    for c in apply {
        let table = c.key.split_once('/').map_or(c.key.as_str(), |(t, _)| t);
        if c.key.starts_with(&format!("{}/", crate::sql::SCHEMAS)) {
            return Err(refused(format!(
                "{world} changes the shape of table {}: an attached folder's tables keep Postgres's shape (change it \
                 in Postgres, then attach again); nothing was merged",
                c.key.split_once('/').map_or("", |(_, t)| t)
            )));
        }
        if crate::reserved_table(table) {
            continue; // the database's own rows: agents, policies, the outbox, constraint and index rows
        }
        let Some(ti) = att.table(table) else {
            return Err(refused(format!(
                "{world} changes table {table}, which isn't attached to Postgres; nothing was merged"
            )));
        };
        let t = &tables[ti];
        let before = core.val(theirs, &c.key)?.map(|v| texts(core, theirs, t, &c.key, &v)).transpose()?;
        let mut after = match &c.after {
            Some(v) => Some(texts(core, theirs, t, &c.key, v).or_else(|_| texts(core, ours, t, &c.key, v))?),
            None => None,
        };
        if let std::collections::hash_map::Entry::Vacant(e) = unseen.entry(ti) {
            let had = core.val(ours, &format!("{}/{}", crate::sql::SCHEMAS, t.name))?;
            let had = had.as_ref().and_then(crate::sql::column_names).unwrap_or_default();
            let missing = (0..t.cols.len()).filter(|&i| !had.is_empty() && !had.contains(&t.cols[i].name)).collect();
            e.insert(missing);
        }
        // a world that never had a column can't have meant to change it: its row there decodes as the column's
        // default (or NULL), and writing that back would wipe what Postgres holds. A row it updates keeps main's
        let mut kept = false;
        if let (Some(b), Some(a)) = (&before, &mut after) {
            for &i in &unseen[&ti] {
                if a[i] != b[i] {
                    a[i] = b[i].clone();
                    kept = true;
                }
            }
        }
        if before == after {
            continue;
        }
        check_ids(att, world, t, before.as_deref(), after.as_deref())?;
        let unseen = if before.is_none() { unseen[&ti].clone() } else { vec![] };
        rows.push(Row { key: c.key.clone(), table: ti, before, after, kept, unseen });
    }
    let most = max_rows();
    if rows.len() > most {
        return Err(refused(format!(
            "{world} changes {} rows of attached tables, more than {most} written to Postgres in one merge: \
             merge it a table at a time (MERGE WORLD {world} ONLY TABLES (...)); nothing was merged",
            rows.len()
        )));
    }
    Ok(rows)
}

/// What a world that checks its reads read (see `stale`), for the write-back to check against Postgres: its read
/// set, its fork point, and the keys it wrote itself (compared as rows the merge writes).
pub(crate) struct Reads<'a> {
    pub(crate) set: &'a crate::Recorder,
    pub(crate) base: &'a State,
    pub(crate) wrote: &'a HashSet<String>,
}

/// Why a write-back rolled back rather than commit (see `write_back`), in words.
pub(crate) enum Held {
    /// what the world read changed in Postgres since its fork (see `live_reads`)
    Stale(Vec<String>),
    /// merge checks that found rows in Postgres as the merge's writes left it (see `live_checks`)
    Checks(Vec<String>),
}

/// The merge checks of a merge into main that run in Postgres too (see `live_checks`): those whose ON TABLES are all
/// attached tables. A check with no ON TABLES may read any table, so it runs only in Chronos.
pub(crate) fn checks_for(att: &Attached, checks: Vec<crate::checks::MergeCheck>) -> Vec<crate::checks::MergeCheck> {
    checks.into_iter().filter(|c| !c.tables.is_empty() && c.tables.iter().all(|t| att.table(t).is_some())).collect()
}

/// The write-back of a merge into main (see the module's doc), run by `merge_locked` under the merge's locks once
/// everything Chronos checks has passed: `apply` is what the merge changes in main (`theirs`), the world's rows
/// as `ours` holds them; `reads`, for a world that checks its reads; `checks`, the merge's checks to run in Postgres
/// (see `checks_for`). Ok(None): Postgres committed (or there was nothing to write), and the merge goes on.
/// Ok(Some): rolled back, nothing written, and the merge is held for what it says. Err: nothing was written to
/// Postgres, and nothing merges; on a conflict main takes Postgres's rows after the locks (see `after_merge`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_back(
    core: &Core,
    att: &Attached,
    world: &str,
    world_id: &str,
    theirs: &State,
    ours: &State,
    apply: &[crate::Change],
    reads: Option<&Reads>,
    checks: &[crate::checks::MergeCheck],
) -> Result<Option<Held>> {
    let rows = rows_of(core, att, world, theirs, ours, apply)?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut pg = att.lease().map_err(|e| refused(format!("{e}; nothing was merged")))?;
    let out = transaction(core, &mut pg, att, world, world_id, &rows, reads, checks);
    if out.is_err() {
        rollback(&mut pg);
    }
    out
}

/// `CHECK READS` of a world of main (see `Core::reads_changed`): what it read, checked against Postgres as it is now,
/// as the write-back checks it (`live_reads`), outside any merge: a read-only transaction (the dry run's start:
/// settings, source identity), rolled back after. Err: Postgres couldn't be asked (say so, rather than "nothing
/// changed").
pub(crate) fn check_reads(
    core: &Core,
    att: &Attached,
    set: &crate::Recorder,
    base: &State,
    wrote: &HashSet<String>,
) -> std::result::Result<Vec<String>, String> {
    let mut pg = att.lease()?;
    let r = Reads { set, base, wrote };
    let out = begin(&mut pg, att, true).and_then(|()| live_reads(core, &mut pg, att, &r));
    rollback(&mut pg);
    out.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- a dry run against Postgres

/// What a dry run of a merge into main checks against Postgres (see `dry_run`), worked out by `merge_locked` under
/// the merge's locks, so the round trips to Postgres happen after them: the rows the merge would write, or why the
/// write-back would refuse them.
pub(crate) struct Dry {
    rows: Result<Vec<Row>>,
    /// a world that checks its reads: its read set, fork point and own keys (see `Reads`)
    reads: Option<(crate::Recorder, State, HashSet<String>)>,
}

impl Dry {
    pub(crate) fn new(
        core: &Core,
        att: &Attached,
        world: &str,
        theirs: &State,
        ours: &State,
        apply: &[crate::Change],
        reads: Option<(crate::Recorder, State, HashSet<String>)>,
    ) -> Dry {
        Dry { rows: rows_of(core, att, world, theirs, ours, apply), reads }
    }
}

/// `MERGE WORLD x DRY RUN` in an attached folder: the rows the merge would write, compared with Postgres as the
/// write-back compares them, in one read-only transaction that is rolled back. Nothing is written to Postgres, and
/// main doesn't take Postgres's rows (as a real merge's conflict does). Read with no locks (a plain SELECT, at
/// REPEATABLE READ: one snapshot across the tables), so it says what a merge would meet then, not later; triggers
/// aren't run (their names are listed), so values they'd set show only after a real merge. Never fails: what went
/// wrong is the check's `error`.
pub(crate) fn dry_run(core: &Core, att: &Attached, dry: Dry) -> crate::LiveCheck {
    let mut live = crate::LiveCheck { source: att.source(), ..Default::default() };
    let rows = match dry.rows {
        Ok(rows) => rows,
        Err(e) => {
            live.error = Some(e.to_string());
            return live;
        }
    };
    if rows.is_empty() {
        return live; // writes nothing to Postgres: nothing there to check
    }
    let mut pg = match att.lease() {
        Ok(pg) => pg,
        Err(e) => {
            live.error = Some(e);
            return live;
        }
    };
    let reads = dry.reads.as_ref().map(|(set, base, wrote)| Reads { set, base, wrote });
    let out = dry_transaction(core, &mut pg, att, &rows, reads.as_ref(), &mut live);
    rollback(&mut pg);
    if let Err(e) = out {
        live.rows.clear();
        live.stale_reads.clear();
        live.error = Some(e.to_string());
    }
    live
}

fn dry_transaction(
    core: &Core,
    pg: &mut Lease,
    att: &Attached,
    rows: &[Row],
    reads: Option<&Reads>,
    live: &mut crate::LiveCheck,
) -> Result<()> {
    begin(pg, att, true)?;
    live.at = crate::now_ms();
    let tables = att.tables();
    let by_table = by_table(rows);
    for &ti in by_table.keys() {
        same_shape(pg, &tables[ti])?;
    }
    if let Some(r) = reads {
        live.stale_reads = live_reads(core, pg, att, r)?;
    }
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        for c in compare(pg, t, rs)? {
            live.rows.push(crate::LiveRow {
                key: rs[c.at].key.clone(),
                changed: c.changed,
                postgres: c.now.as_ref().map(|r| row_json(t, r)),
            });
        }
    }
    // user triggers on the tables written: the dry run doesn't run the writes, so what they'd set isn't shown
    let names = text_array(by_table.keys().map(|&ti| Some(tables[ti].pg.as_str())));
    let sql = "select tgrelid::regclass::text, tgname from pg_trigger where tgrelid = any($1::text[]::regclass[]) \
               and not tgisinternal and tgenabled <> 'D' order by 1, 2";
    for r in run(pg, "reading the tables' triggers", sql, &[names])? {
        live.triggers.push(format!("{}: {}", r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default()));
    }
    Ok(())
}

/// Why a merge the dry run checked against Postgres would be refused there now (rows changed since main's copy),
/// or None.
pub(crate) fn dry_blocked(live: &crate::LiveCheck) -> Option<String> {
    let changed = live.rows.iter().filter(|r| r.changed).count();
    if changed == 0 && !live.stale_reads.is_empty() {
        return Some(format!("merging it would be held: it {} ({})", live.stale_reads.join("; it "), live.source));
    }
    (changed > 0).then(|| {
        format!(
            "{changed} rows it would write changed in Postgres since main's copy ({}): merging now is refused, nothing \
             is written to Postgres, and main takes Postgres's values for them; then merge again and settle them as \
             any conflict",
            live.source
        )
    })
}

/// Conditions `conds` (each a Postgres condition) joined by OR, as one condition: in parentheses whole, so what is
/// ANDed to it after applies to all of them.
fn any_of(conds: &[String]) -> String {
    format!("(({}))", conds.join(") or ("))
}

/// A row of `t` as JSON: each column's text as Postgres writes it (null for NULL).
fn row_json(t: &Table, r: &[Option<String>]) -> String {
    let o: serde_json::Map<String, Json> =
        t.cols.iter().zip(r).map(|(c, v)| (c.name.clone(), v.clone().map_or(Json::Null, Json::String))).collect();
    Json::Object(o).to_string()
}

/// `rows` by table, in the tables' order (parents first).
fn by_table(rows: &[Row]) -> BTreeMap<usize, Vec<&Row>> {
    let mut out: BTreeMap<usize, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        out.entry(r.table).or_default().push(r);
    }
    out
}

/// The parameters of `rows` for one statement: per key column, then (with `present`) whether main has the row,
/// then per column of `cols` (positions in the table), a text array each.
fn arrays(rows: &[&Row], t: &Table, present: bool, cols: &[usize], after: bool) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let keys: Vec<Vec<Option<String>>> = rows.iter().map(|r| r.key_texts(t)).collect();
    for k in 0..t.key.len() {
        out.push(text_array(keys.iter().map(|r| r[k].as_deref())));
    }
    if present {
        out.push(format!(
            "{{{}}}",
            rows.iter().map(|r| if r.before.is_some() { "t" } else { "f" }).collect::<Vec<_>>().join(",")
        ));
    }
    for &c in cols {
        out.push(text_array(
            rows.iter().map(|r| if after { r.after.as_ref() } else { r.before.as_ref() }.and_then(|v| v[c].as_deref())),
        ));
    }
    out
}

/// `unnest($1::text[], ...) with ordinality as k(k0, ..., [p,] c0, ..., i)`, numbering parameters from `from`.
fn unnest(t: &Table, present: bool, cols: &[usize], from: usize) -> String {
    let mut types = vec![];
    let mut names = vec![];
    for k in 0..t.key.len() {
        types.push("text[]");
        names.push(format!("k{k}"));
    }
    if present {
        types.push("bool[]");
        names.push("p".into());
    }
    for c in cols {
        types.push("text[]");
        names.push(format!("c{c}"));
    }
    let params: Vec<String> = types.iter().enumerate().map(|(i, ty)| format!("${}::{ty}", from + i)).collect();
    names.push("i".into());
    format!("unnest({}) with ordinality as k({})", params.join(", "), names.join(", "))
}

/// The table's key in the rows of alias `a`, and the key values of `k` cast to its types.
fn key_match(t: &Table, a: &str) -> String {
    let ours: Vec<String> = t.key.iter().map(|&i| format!("{a}.{}", q(&t.cols[i].name))).collect();
    let given: Vec<String> = t.key.iter().enumerate().map(|(n, &i)| format!("k.k{n}::{}", t.cols[i].ty)).collect();
    format!("({}) = ({})", ours.join(", "), given.join(", "))
}

/// Runs `sql` with text parameters on `pg`.
fn run(pg: &mut Client, what: &str, sql: &str, params: &[String]) -> Result<pgclient::Rows> {
    let p: Vec<Option<&str>> = params.iter().map(|s| Some(s.as_str())).collect();
    pg.query_params(sql, &p).map_err(|e| failed(what, &e))
}

/// One write-back transaction (see the module's doc). Err leaves the transaction for the caller to roll back.
#[allow(clippy::too_many_arguments)]
fn transaction(
    core: &Core,
    pg: &mut Lease,
    att: &Attached,
    world: &str,
    world_id: &str,
    rows: &[Row],
    reads: Option<&Reads>,
    checks: &[crate::checks::MergeCheck],
) -> Result<Option<Held>> {
    begin(pg, att, false)?;
    let tables = att.tables();
    let by_table = by_table(rows);
    for &ti in by_table.keys() {
        same_shape(pg, &tables[ti])?;
    }
    // what the world read, as Postgres has it now: before any row is locked or written
    if let Some(r) = reads {
        let stale = live_reads(core, pg, att, r)?;
        if !stale.is_empty() {
            rollback(pg);
            return Ok(Some(Held::Stale(stale)));
        }
    }
    // every row locked and compared before anything is written: any conflict, and nothing is
    // (table, the row, Postgres's row now, its key as Postgres writes it)
    let mut conflicts: Vec<(usize, &Row, Option<Texts>, Texts)> = vec![];
    // per table, the key text Postgres gives each row (by its place in the table's rows)
    let mut canon: HashMap<usize, Vec<Vec<Option<String>>>> = HashMap::new();
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let tkeys: Vec<String> = t.key.iter().map(|&i| format!("t.{}", q(&t.cols[i].name))).collect();
        let kcast: Vec<String> = t.key.iter().enumerate().map(|(n, &i)| format!("k.k{n}::{}", t.cols[i].ty)).collect();
        let lock = format!(
            "select 1 from {} t where ({}) in (select {} from {}) for update of t",
            t.pg,
            tkeys.join(", "),
            kcast.join(", "),
            unnest(t, false, &[], 1)
        );
        let mut keys_here = vec![vec![]; rs.len()];
        for (part, chunk) in rs.chunks(CHUNK).enumerate() {
            let from = part * CHUNK;
            run(pg, &format!("locking rows of {}", t.pg), &lock, &arrays(chunk, t, false, &[], false))?;
            for c in compare(pg, t, chunk)? {
                keys_here[from + c.at] = c.key.clone();
                if c.changed {
                    conflicts.push((ti, rs[from + c.at], c.now, c.key));
                }
            }
        }
        canon.insert(ti, keys_here);
    }
    if !conflicts.is_empty() {
        rollback(pg);
        let mut lines = vec![];
        let mut stored = vec![];
        for (ti, row, now, key) in &conflicts {
            let t = &tables[*ti];
            let show = |r: &Option<Vec<Option<String>>>| r.as_ref().map_or("no row".to_string(), |r| row_json(t, r));
            if lines.len() < 10 {
                lines.push(format!(
                    "  {}: the world's {}, main's {}, Postgres's {}",
                    crate::sql::show_key(&row.key),
                    show(&row.after),
                    show(&row.before),
                    show(now)
                ));
            }
            stored.push(Stored { table: *ti, key: key.clone(), row: now.clone() });
        }
        if conflicts.len() > lines.len() {
            lines.push(format!("  ... and {} more", conflicts.len() - lines.len()));
        }
        AFTER.set(Some(After { rows: stored, committed: false, refresh: vec![] }));
        return Err(conflict(conflicts.len(), world, &att.source(), &lines));
    }
    // writes: updates and inserts parents first, then deletes children first
    let mut sent_back: Vec<Stored> = vec![];
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let all: Vec<usize> = (0..t.cols.len()).collect();
        let back: Vec<String> = t.cols.iter().map(|c| format!("t.{}::text", q(&c.name))).collect();
        let sent: Vec<String> = all.iter().map(|&i| format!("k.c{i}::{}::text", t.cols[i].ty)).collect();
        let updates: Vec<&Row> = rs.iter().copied().filter(|r| r.before.is_some() && r.after.is_some()).collect();
        let set: Vec<usize> = all.iter().copied().filter(|i| !t.key.contains(i) && !t.cols[*i].always).collect();
        if !updates.is_empty() && !set.is_empty() {
            let sets: Vec<String> =
                set.iter().map(|&i| format!("{} = k.c{i}::{}", q(&t.cols[i].name), t.cols[i].ty)).collect();
            let sql = format!(
                "update {} t set {} from {} where {} returning ({}) is distinct from ({}), {}",
                t.pg,
                sets.join(", "),
                unnest(t, false, &all, 1),
                key_match(t, "t"),
                back.join(", "),
                sent.join(", "),
                back.join(", ")
            );
            for chunk in updates.chunks(CHUNK) {
                let got = run(pg, &format!("updating {}", t.pg), &sql, &arrays(chunk, t, false, &all, true))?;
                count(t, "updated", chunk.len(), got.len())?;
                // (first: a row Postgres then stored otherwise, as `keep_changed` finds, is taken as stored)
                for r in chunk.iter().filter(|r| r.kept) {
                    sent_back.push(Stored { table: ti, key: r.key_texts(t), row: r.after.clone() });
                }
                keep_changed(t, ti, got, &mut sent_back);
            }
        }
        let inserts: Vec<&Row> = rs.iter().copied().filter(|r| r.before.is_none()).collect();
        if !inserts.is_empty() {
            // columns the world's table hadn't (one world's rows: the same for each) are Postgres's to fill, and
            // compared as unknown, so main takes what Postgres put there
            let unseen = &inserts[0].unseen;
            let given_cols: Vec<usize> = all.iter().copied().filter(|i| !unseen.contains(i)).collect();
            let cols: Vec<String> = given_cols.iter().map(|&i| q(&t.cols[i].name)).collect();
            let given: Vec<String> = given_cols.iter().map(|&i| format!("k.c{i}::{}", t.cols[i].ty)).collect();
            let sent: Vec<String> = all
                .iter()
                .map(|&i| match unseen.contains(&i) {
                    true => "null::text".to_string(),
                    false => format!("k.c{i}::{}::text", t.cols[i].ty),
                })
                .collect();
            let back_ins: Vec<String> = t.cols.iter().map(|c| format!("ins.{}::text", q(&c.name))).collect();
            let key_eq: Vec<String> = t
                .key
                .iter()
                .map(|&i| format!("ins.{}::text = k.c{i}::{}::text", q(&t.cols[i].name), t.cols[i].ty))
                .collect();
            let sql = format!(
                "with k as (select * from {}), ins as (insert into {} ({}){} select {} from k order by k.i returning *) \
                 select ({}) is distinct from ({}), {} from ins join k on {}",
                unnest(t, false, &all, 1),
                t.pg,
                cols.join(", "),
                if t.cols.iter().any(|c| c.always) { " overriding system value" } else { "" },
                given.join(", "),
                back_ins.join(", "),
                sent.join(", "),
                back_ins.join(", "),
                key_eq.join(" and ")
            );
            for chunk in inserts.chunks(CHUNK) {
                let got = run(pg, &format!("inserting into {}", t.pg), &sql, &arrays(chunk, t, false, &all, true))?;
                count(t, "inserted", chunk.len(), got.len())?;
                keep_changed(t, ti, got, &mut sent_back);
            }
        }
    }
    for (&ti, rs) in by_table.iter().rev() {
        let t = &tables[ti];
        let deletes: Vec<&Row> = rs.iter().copied().filter(|r| r.after.is_none()).collect();
        if deletes.is_empty() {
            continue;
        }
        let sql = format!(
            "delete from {} t using {} where {} returning 1",
            t.pg,
            unnest(t, false, &[], 1),
            key_match(t, "t")
        );
        for chunk in deletes.chunks(CHUNK) {
            let got = run(pg, &format!("deleting from {}", t.pg), &sql, &arrays(chunk, t, false, &[], false))?;
            count(t, "deleted", chunk.len(), got.len())?;
        }
    }
    // deferred constraint triggers (and foreign key checks) run now, not at COMMIT: what they write is counted below,
    // and the merge checks see it; one that fails rolls the write-back back here, as it would at COMMIT
    run(pg, "running deferred triggers", "set constraints all immediate", &[])?;
    // what else the writes made Postgres write (triggers, rules, foreign keys' actions): counted now, read after COMMIT
    let others = others(pg, &tables, &by_table)?;
    // the merge checks, on Postgres as these writes leave it: rows added there straight since the attach count too
    let found = live_checks(pg, checks);
    if !found.is_empty() {
        rollback(pg);
        return Ok(Some(Held::Checks(found)));
    }
    // the merge's own row, and what recovery needs should the commit's outcome be lost
    let merge_id = format!("merge:{}", random_id());
    run(
        pg,
        "recording the merge",
        "insert into chronos.merges (id, at) values ($1, now())",
        std::slice::from_ref(&merge_id),
    )?;
    let mut touched: BTreeMap<String, Vec<Vec<Option<String>>>> = BTreeMap::new();
    for (ti, keys) in &canon {
        touched.entry(tables[*ti].name.clone()).or_default().extend(keys.iter().cloned());
    }
    // (and the tables written besides: after a crash, recovery refreshes them, as nothing else would say they changed)
    let besides: Vec<&str> = others.iter().map(|&ti| tables[ti].name.as_str()).collect();
    let pending = json!({"merge": merge_id, "world": world, "world_id": world_id, "rows": touched, "others": besides});
    crate::disk::write_file(&att.pending_path(), pending.to_string().as_bytes()).map_err(Error::Io)?;
    crash_point("before_commit");
    let outcome = pg.query("commit");
    crash_point("after_commit");
    match outcome {
        Ok(_) if pg.status == b'I' => {}
        Ok(_) => {
            // (COMMIT of a failed transaction answers ROLLBACK)
            let _ = std::fs::remove_file(att.pending_path());
            return Err(refused("Postgres rolled the write-back back at COMMIT; nothing was merged"));
        }
        Err(e) if pg_error(&e).is_some() => {
            // the server answered: it didn't commit (a deferred constraint, say)
            let _ = std::fs::remove_file(att.pending_path());
            return Err(failed("at COMMIT", &e));
        }
        Err(e) => {
            // the connection went during COMMIT: it may have committed. Asked of Postgres now, from a new one
            match committed(&merge_id) {
                Ok(true) => {}
                Ok(false) => {
                    let _ = std::fs::remove_file(att.pending_path());
                    return Err(failed("the connection was lost at COMMIT, which didn't happen", &e));
                }
                Err(why) => {
                    return Err(refused(format!(
                        "the connection to Postgres was lost during COMMIT ({e}), and whether it committed can't be \
                         told yet ({why}): merges into main wait until it can (the next merge, or chronos serve, asks \
                         again); {world} is as it was"
                    )));
                }
            }
        }
    }
    // tables this transaction wrote besides the merge's own rows: main takes them by a refresh of those tables alone,
    // on a connection of its own, once the merge is made (see `after_merge`); until it has, the pending record stays
    // and names them, so recovery does it after a crash (see `resolve`)
    let refresh = others;
    AFTER.set(Some(After { rows: sent_back, committed: true, refresh }));
    Ok(None)
}

/// The attached tables the write-back's transaction wrote more rows of than the merge sent: a trigger, a rule or a
/// foreign key's ON DELETE / ON UPDATE action wrote them, and main has no other way to hear of it. By place, from
/// Postgres's counts of rows inserted, updated and deleted in this transaction
/// (`pg_stat_xact_user_tables`, subtransactions included; with `track_counts` off they read 0, and nothing is found,
/// as before). Which rows isn't asked: their `xmin` may be a subtransaction's, and finding them reads the table
/// whole anyway, so main takes those tables by a refresh.
fn others(pg: &mut Client, tables: &[Table], by_table: &BTreeMap<usize, Vec<&Row>>) -> Result<Vec<usize>> {
    let names = text_array(tables.iter().map(|t| Some(t.pg.as_str())));
    let sql = "select k.i, s.n_tup_ins, s.n_tup_upd, s.n_tup_del from unnest($1::text[]) with ordinality as k(t, i) \
               join pg_stat_xact_user_tables s on s.relid = k.t::regclass";
    let mut out = vec![];
    for r in run(pg, "counting the rows the transaction wrote", sql, &[names])? {
        let n = |i: usize| r[i].as_deref().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        let ti = n(0).saturating_sub(1);
        let sent = by_table.get(&ti).map_or(&[][..], |v| v.as_slice());
        let ins = sent.iter().filter(|r| r.before.is_none()).count();
        let upd = sent.iter().filter(|r| r.before.is_some() && r.after.is_some()).count();
        let del = sent.iter().filter(|r| r.after.is_none()).count();
        if n(1) > ins || n(2) > upd || n(3) > del {
            out.push(ti);
        }
    }
    Ok(out)
}

/// The merge checks `checks` (see `checks_for`) run in the write-back's transaction, after its writes, each within
/// its own timeout: what they break, in the words `Core::failed_checks` uses (none: all found nothing). A check that
/// fails leaves the transaction aborted, so the checks after it aren't run; the merge is held either way.
fn live_checks(pg: &mut Client, checks: &[crate::checks::MergeCheck]) -> Vec<String> {
    let mut out = vec![];
    for c in checks {
        let query = c.query.trim().trim_end_matches(';');
        let sql = format!(
            "set local statement_timeout = '{}ms'; select row_to_json(c)::text from ({query}\n) c limit {}; \
             set local statement_timeout = '{STATEMENT_TIMEOUT}'",
            c.timeout,
            crate::checks::SHOWN + 1
        );
        match pg.query(&sql) {
            Ok(rows) if rows.is_empty() => {}
            Ok(rows) => {
                let mut shown: Vec<String> =
                    rows.iter().take(crate::checks::SHOWN).filter_map(|r| r[0].clone()).collect();
                if rows.len() > crate::checks::SHOWN {
                    shown.push("and more".into());
                }
                out.push(format!("breaks merge check {} in Postgres, which finds {}", c.name, shown.join(", ")));
            }
            Err(e) if pg_error(&e).is_some_and(|p| p.code == "57014") => {
                out.push(format!(
                    "breaks merge check {} in Postgres: it didn't finish within {} ms",
                    c.name, c.timeout
                ));
                break;
            }
            Err(e) => {
                out.push(format!("breaks merge check {} in Postgres: it failed ({e})", c.name));
                break;
            }
        }
    }
    out
}

/// Starts a transaction on the source (see `SETTINGS`, the timeouts) and checks it is the database this folder was
/// attached to, in the same round trip. `dry`: a dry run's (read only, one snapshot), which writes nothing. The use's
/// first statement: on a kept connection found gone, run again on a new one (see `Lease::first`).
fn begin(pg: &mut Lease, att: &Attached, dry: bool) -> Result<()> {
    let (how, what, nothing) = match dry {
        true => ("isolation level repeatable read, read only", "starting the dry run", "nothing was checked"),
        false => ("isolation level read committed", "starting the write-back", "nothing was merged"),
    };
    let start = format!(
        "begin {how}; set local lock_timeout = '{LOCK_TIMEOUT}'; \
         set local statement_timeout = '{STATEMENT_TIMEOUT}'; \
         set local idle_in_transaction_session_timeout = '{IDLE_TIMEOUT}'; {SETTINGS}; \
         select count(*) from chronos.merges where id = {}",
        lit(&att.marker())
    );
    let found = pg.first(|pg| pg.query(&start)).map_err(|e| failed(what, &e))?;
    if found.last().and_then(|r| r[0].as_deref()) != Some("1") {
        return Err(refused(format!(
            "the Postgres CHRONOS_SOURCE_URL names isn't the one this folder was attached to ({}): its chronos.merges \
             has no row {}; {nothing}",
            att.source(),
            att.marker()
        )));
    }
    Ok(())
}

/// One row of a table compared with Postgres (see `compare`).
struct Compared {
    /// its place in the rows given
    at: usize,
    /// Postgres's row isn't the one main holds (or is there when main has none, or gone when main has one)
    changed: bool,
    /// Postgres's row now (None: no row there)
    now: Option<Texts>,
    /// its key as Postgres writes it
    key: Texts,
}

/// Each of `rs` (rows of `t`) compared with Postgres's row of the same key: main's value of it cast to the column's
/// type and both sides to text (see the module's doc), so a row nobody changed always compares equal.
fn compare(pg: &mut Client, t: &Table, rs: &[&Row]) -> Result<Vec<Compared>> {
    let all: Vec<usize> = (0..t.cols.len()).collect();
    let params = arrays(rs, t, true, &all, false);
    let pg_text: Vec<String> = t.cols.iter().map(|c| format!("t.{}::text", q(&c.name))).collect();
    let held: Vec<String> = all.iter().map(|&i| format!("k.c{i}::{}::text", t.cols[i].ty)).collect();
    let canon_keys: Vec<String> =
        t.key.iter().enumerate().map(|(n, &i)| format!("k.k{n}::{}::text", t.cols[i].ty)).collect();
    let k0 = format!("t.{}", q(&t.cols[t.key[0]].name));
    let sql = format!(
        "select k.i, ({k0} is null) = k.p or (k.p and ({}) is distinct from ({})), {k0} is null, {}, {} \
         from {} left join {} t on {} order by k.i",
        pg_text.join(", "),
        held.join(", "),
        canon_keys.join(", "),
        pg_text.join(", "),
        unnest(t, true, &all, 1),
        t.pg,
        key_match(t, "t"),
    );
    let found = run(pg, &format!("comparing rows of {}", t.pg), &sql, &params)?;
    let nk = t.key.len();
    let mut out = vec![];
    for r in found {
        let i: usize = r[0].as_deref().and_then(|i| i.parse().ok()).unwrap_or(0);
        if i == 0 || i > rs.len() {
            continue;
        }
        out.push(Compared {
            at: i - 1,
            changed: r[1].as_deref() == Some("t"),
            now: (r[2].as_deref() != Some("t")).then(|| r[3 + nk..].to_vec()),
            key: r[3..3 + nk].to_vec(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- reads checked against Postgres

/// Rows of one attached table a world's read of it may need read whole from Postgres (see `live_reads`): past it,
/// the read is too broad to check there, and a person decides.
const LIVE_ROWS: usize = 100_000;

/// What a world that checks its reads read of attached tables, checked against Postgres inside the write-back's
/// transaction before anything is written (issue #28): within Chronos a read is checked against main, Chronos's copy,
/// which a row changed straight in Postgres never reaches. Postgres's rows are put in place of the fork point's in a
/// copy of it (with the index and constraint entries they make), and the same check as against main
/// (`Core::stale_reasons`) runs on that. A table read only by keys has those rows compared as the write-back compares
/// them; one read by a range, a filtered scan, whole, through an index or constraint, or by a key that found no row,
/// is read whole from Postgres (at most `LIVE_ROWS` rows), so a row added there that the read would have found (an
/// empty result included) counts. Read at READ COMMITTED with no predicate lock: a row Postgres takes between this
/// and COMMIT comes after the merge in time, a serial order the world's decision holds in. The reasons, said as
/// changed in Postgres; none: what it read is as Postgres has it.
fn live_reads(core: &Core, pg: &mut Client, att: &Attached, r: &Reads) -> Result<Vec<String>> {
    let (read_tables, filters) = {
        let set = r.set.lock().unwrap_or_else(|p| p.into_inner());
        let read_tables = set.by_table();
        let filters: HashMap<String, Vec<crate::sql::RowFilter>> =
            read_tables.keys().filter_map(|n| Some((n.clone(), set.only_filtered(n)?))).collect();
        (read_tables, filters)
    };
    let tables = att.tables();
    let base = core.past_view(r.base);
    let mut live = r.base.clone();
    let (mut keys, mut out): (Vec<String>, Vec<String>) = (vec![], vec![]);
    let too_broad = |t: &Table| {
        format!(
            "read {} more broadly than can be checked against Postgres (more than {LIVE_ROWS} rows), and Postgres may \
             have changed it since the copy",
            crate::sql::shown_name(&t.name)
        )
    };
    for (name, (read, mut whole)) in read_tables {
        let Some(ti) = att.table(&name) else { continue };
        // as the world saw it: columns Postgres added after its fork (see `widen`) weren't there to read
        let t = &as_at(core, r.base, &tables[ti])?;
        // the fork point's rows the read covers: those read one by one, or every one of the table
        let mut rows: Vec<Row> = vec![];
        if !whole {
            for k in read.iter().filter(|k| !r.wrote.contains(k.as_str())) {
                match core.val(r.base, k)? {
                    Some(v) => rows.push(Row {
                        key: k.clone(),
                        table: ti,
                        before: Some(texts(core, r.base, t, k, &v)?),
                        after: None,
                        kept: false,
                        unseen: vec![],
                    }),
                    // a key that found no row: its key as Postgres writes it isn't known here, so the table is read whole
                    None => whole = true,
                }
            }
        }
        // read only by filtered scans (and keys): Postgres is asked for the rows the filters can match, as the
        // fork point's rows are, not for the table; written as Postgres compares them, else the table is read whole
        let types: Vec<(String, String)> = t.cols.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
        let mut narrow: Option<(Vec<&crate::sql::RowFilter>, String)> = filters.get(&name).and_then(|fs| {
            let conds: Vec<String> = fs.iter().map(|f| f.to_postgres("t", &types)).collect::<Option<_>>()?;
            Some((fs.iter().collect(), any_of(&conds)))
        });
        // a key looked up that found no row at the fork: Postgres may have that row now, with values no filter
        // matches, and its key as Postgres writes it isn't known here, so the table is read whole
        if narrow.is_some() {
            for k in read.iter().filter(|k| !r.wrote.contains(k.as_str())) {
                if core.val(r.base, k)?.is_none() {
                    narrow = None;
                    break;
                }
            }
        }
        let by_key: HashSet<&String> = read.iter().collect();
        if whole {
            rows.clear();
            let matches = narrow.as_ref().map(|(fs, _)| crate::sql::Filters::new(&base, fs));
            for kv in base.prefix(&format!("{name}/"))? {
                let (k, v) = kv?;
                if matches.as_ref().is_some_and(|m| !m.any(&k, &v)) && !by_key.contains(&k) {
                    continue;
                }
                if rows.len() == LIVE_ROWS {
                    break;
                }
                rows.push(Row {
                    before: Some(texts(core, r.base, t, &k, &v)?),
                    key: k,
                    table: ti,
                    after: None,
                    kept: false,
                    unseen: vec![],
                });
            }
            if rows.len() == LIVE_ROWS {
                out.push(too_broad(t));
                continue;
            }
        }
        let named = |row: &[Option<String>]| -> Vec<(String, Option<String>)> {
            t.cols.iter().map(|c| c.name.clone()).zip(row.iter().cloned()).collect()
        };
        let rs: Vec<&Row> = rows.iter().collect();
        if !rs.is_empty() {
            for c in compare(pg, t, &rs)?.into_iter().filter(|c| c.changed) {
                let key = &rs[c.at].key;
                if r.wrote.contains(key.as_str()) {
                    continue;
                }
                let now = match &c.now {
                    Some(row) => Some(crate::sql::row_from_texts(&base, &t.name, &named(row))?.1),
                    None => None,
                };
                live.set(key, now);
                keys.push(key.clone());
            }
        }
        if whole {
            // and the rows Postgres has that the fork point didn't
            let back: Vec<String> = t.cols.iter().map(|c| format!("t.{}::text", q(&c.name))).collect();
            let only = narrow.as_ref().map_or(String::new(), |(_, w)| format!("{w} and "));
            let sql = format!(
                "select {} from {} t where {only}not exists (select 1 from {} where {}) limit {}",
                back.join(", "),
                t.pg,
                unnest(t, false, &[], 1),
                key_match(t, "t"),
                LIVE_ROWS + 1
            );
            let added = run(
                pg,
                &format!("reading rows of {} the copy hasn't", t.pg),
                &sql,
                &arrays(&rs, t, false, &[], false),
            )?;
            if rows.len() + added.len() > LIVE_ROWS {
                out.push(too_broad(t));
                continue;
            }
            for row in added {
                let (k, v) = crate::sql::row_from_texts(&base, &t.name, &named(&row))?;
                if !r.wrote.contains(k.as_str()) {
                    live.set(&k, Some(v));
                    keys.push(k);
                }
            }
        }
    }
    if keys.is_empty() {
        return Ok(out);
    }
    // the index and constraint entries Postgres's rows make, as a merge makes them: what a lookup through them finds
    let after = core.past_view(&live);
    match crate::sql::reindex(&after, &base, &keys) {
        Ok(entries) => {
            for (k, v) in entries {
                live.set(&k, v);
            }
        }
        Err(e) => out.push(format!("read rows Postgres changed in a way that can't be checked here ({})", e.message)),
    }
    let stale = core.stale_reasons(r.set, r.base, &live, r.wrote)?;
    // (reads lost in a restart are main's check's to report: it held the merge already)
    out.extend(
        stale
            .into_iter()
            .filter(|s| s.contains("changed since it forked"))
            .map(|s| s.replace("changed since it forked", "changed in Postgres since it forked")),
    );
    Ok(out)
}

/// Refuses a statement that wrote another number of rows than it was given: a trigger skipped some (BEFORE ...
/// RETURN NULL), or a rule rewrote it.
fn count(t: &Table, what: &str, sent: usize, got: usize) -> Result<()> {
    if sent == got {
        return Ok(());
    }
    Err(refused(format!(
        "Postgres {what} {got} rows of {}, not the {sent} the merge has (a trigger or rule there skipped or added \
         some); rolled back, nothing was merged",
        t.pg
    )))
}

/// Rows Postgres stored otherwise than the merge sent them (a trigger set a column, say): main is to take them.
fn keep_changed(t: &Table, ti: usize, got: pgclient::Rows, out: &mut Vec<Stored>) {
    for r in got {
        if r[0].as_deref() == Some("t") {
            let row: Vec<Option<String>> = r[1..].to_vec();
            let key = t.key.iter().map(|&i| row[i].clone()).collect();
            out.push(Stored { table: ti, key, row: Some(row) });
        }
    }
}

/// A table's shape in Postgres, against the folder's record of it (see `shape`).
enum Shape {
    Same,
    /// only columns added after the recorded ones, each of which a refresh can add here too (see `widen`)
    Added(Vec<Added>),
    /// anything else, in words: a column dropped, renamed or retyped, the key, row-level security, a generated or
    /// sequence-fed column added
    Changed(Vec<String>),
}

/// A column Postgres added to an attached table: as the record keeps it, and its definition here.
struct Added {
    col: Col,
    /// its default in Postgres, if Chronos can run it
    default: Option<String>,
    /// NOT NULL in Postgres: made so here once main's rows have Postgres's values (see `settle`)
    not_null: bool,
}

/// `t`'s shape in Postgres now: its columns (names, types, generated or not), primary key, and row-level security
/// (on, the role sees only some rows, and the rest would look gone), against what the folder recorded.
fn shape(pg: &mut Client, t: &Table) -> Result<Shape> {
    let sql = "select a.attname, format_type(a.atttypid, a.atttypmod), a.attgenerated <> '', c.relrowsecurity, \
               array_position(i.indkey::int2[], a.attnum), a.attnotnull, pg_get_expr(d.adbin, d.adrelid), \
               a.attidentity <> '' or coalesce(pg_get_expr(d.adbin, d.adrelid), '') like 'nextval(%' \
               from pg_class c join pg_attribute a on a.attrelid = c.oid \
               left join pg_index i on i.indrelid = c.oid and i.indisprimary \
               left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
               where c.oid = $1::regclass and a.attnum > 0 and not a.attisdropped order by a.attnum";
    let rows = run(pg, &format!("reading {}'s columns", t.pg), sql, std::slice::from_ref(&t.pg))?;
    let v = |r: &Texts, i: usize| r[i].clone().unwrap_or_default();
    let mut diffs = vec![];
    if rows.first().is_some_and(|r| r[3].as_deref() == Some("t")) {
        diffs.push("row-level security is on".to_string());
    }
    let now: Vec<(String, String)> = rows.iter().map(|r| (v(r, 0), v(r, 1))).collect();
    let was: Vec<(String, String)> = t.cols.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
    // Postgres numbers a column added later after every one there: the recorded ones first, unchanged, then new
    let widened = now.len() > was.len() && now[..was.len()] == was[..];
    if now != was && !widened {
        let show = |v: &[(String, String)]| v.iter().map(|(n, ty)| format!("{n} {ty}")).collect::<Vec<_>>().join(", ");
        diffs.push(format!("its columns are ({}), not ({})", show(&now), show(&was)));
    }
    if let Some(r) = rows.iter().find(|r| r[2].as_deref() == Some("t")) {
        diffs.push(format!("column {} is generated", v(r, 0)));
    }
    let mut key: Vec<(i64, String)> =
        rows.iter().filter_map(|r| Some((r[4].as_deref()?.parse().ok()?, v(r, 0)))).collect();
    key.sort();
    let key: Vec<String> = key.into_iter().map(|(_, n)| n).collect();
    let was_key: Vec<String> = t.key.iter().map(|&i| t.cols[i].name.clone()).collect();
    if key != was_key {
        diffs.push(format!("its primary key is ({}), not ({})", key.join(", "), was_key.join(", ")));
    }
    if !diffs.is_empty() {
        return Ok(Shape::Changed(diffs));
    }
    if !widened {
        return Ok(Shape::Same);
    }
    let mut added = vec![];
    for r in &rows[was.len()..] {
        let (name, ty) = (v(r, 0), v(r, 1));
        if r[7].as_deref() == Some("t") {
            // values from a sequence: attach reserves them from Postgres, and that is set up only at attach
            diffs.push(format!("column {name} was added taking its values from a sequence"));
            continue;
        }
        // a default Chronos can't run is left off, as attach leaves it off. NOT NULL comes after the rows' values
        // (see `settle`): a column added NULL, filled, then made NOT NULL in Postgres has no default to fill it here
        let default = r[6].clone().filter(|e| !crate::sql::default_refused(e));
        let not_null = r[5].as_deref() == Some("t");
        added.push(Added { col: Col { name, ty, always: false }, default, not_null });
    }
    Ok(match diffs.is_empty() {
        true => Shape::Added(added),
        false => Shape::Changed(diffs),
    })
}

/// Refuses a write-back to a table whose shape in Postgres isn't as recorded (see `shape`). Columns Postgres added
/// come over at the next refresh; any other change needs a new attach.
fn same_shape(pg: &mut Client, t: &Table) -> Result<()> {
    match shape(pg, t)? {
        Shape::Same => Ok(()),
        Shape::Added(cols) => Err(refused(format!(
            "{} has columns in Postgres main hasn't taken yet ({}): a refresh takes them (chronos refresh, or serve \
             --refresh-every), then merge again; nothing was merged",
            t.pg,
            cols.iter().map(|a| a.col.name.as_str()).collect::<Vec<_>>().join(", ")
        ))),
        Shape::Changed(diffs) => Err(refused(format!(
            "{} changed in Postgres since the attach: {}. Attach it again into a new folder (worlds here, and their \
             unmerged work, stay in this one); nothing was merged",
            t.pg,
            diffs.join("; ")
        ))),
    }
}

// ---------------------------------------------------------------- after the merge, and recovery

/// The write-back lock for a merge into main of an attached folder, and any write-back left pending settled first
/// (see `resolve`): taken by `merge_kept` before the merge's own locks, held until `after_merge` is done.
pub(crate) fn before_merge(core: &Core, into_main: bool) -> Result<Option<Hold<'_>>> {
    let Some(att) = core.attached.as_deref() else { return Ok(None) };
    if !into_main || SKIP.get() || crate::RECOVERING.get() || core.me.get().is_none() {
        return Ok(None);
    }
    let held = att.hold();
    if att.pending() {
        resolve(core).map_err(|e| {
            refused(format!(
                "a merge into main written to Postgres before isn't settled yet: {e}; merges into main wait until it is"
            ))
        })?;
    }
    Ok(held)
}

/// What `merge_locked` left (see `After`), done now that its locks are let go: main takes Postgres's rows, and a
/// committed merge's pending record goes. Any error here leaves main behind Postgres on those rows, which a later
/// merge finds and reports as conflicts: nothing is lost, so it is said, not returned.
pub(crate) fn after_merge(core: &Core) {
    let Some(after) = AFTER.take() else { return };
    let Some(att) = core.attached.as_deref() else { return };
    let failed = store(core, att, &after.rows).err();
    if let Some(e) = failed {
        eprintln!(
            "chronos: main couldn't take Postgres's values for {} rows ({e}); a later merge touching them reports \
             them as conflicts",
            after.rows.len()
        );
    }
    // the tables its triggers wrote besides, then the pending record: kept if that fails, so the next merge into main
    // (or a refresh, or chronos serve) settles it first, refreshing them again (see `resolve`)
    if !after.refresh.is_empty() {
        crash_point("before_trigger_refresh");
    }
    if !after.refresh.is_empty()
        && let Err(e) = refresh_now(core, att, Some(&after.refresh))
    {
        eprintln!(
            "chronos: main couldn't take the rows the merge's triggers changed in Postgres ({e}); the next merge into \
             main takes them first"
        );
        return;
    }
    if after.committed {
        let _ = std::fs::remove_file(att.pending_path());
    }
}

/// What a merge's conflict with Postgres says (see `is_conflict`).
const CHANGED_IN_POSTGRES: &str = "changed in Postgres since main last had them";

/// A merge's conflict with Postgres: `n` rows `world` changes changed there since main last had them, a line each.
fn conflict(n: usize, world: &str, source: &str, lines: &[String]) -> Error {
    Error::Constraint {
        code: "40001",
        message: format!(
            "{n} rows {world} changes {CHANGED_IN_POSTGRES} (attached {source}); nothing was written to Postgres and \
             nothing merged. main now holds Postgres's values for them: merge again, and settle them as any conflict \
             (OURS, THEIRS, or row by row).\n{}",
            lines.join("\n")
        ),
    }
}

/// Whether `e` is a merge's conflict with Postgres (see `conflict`).
pub(crate) fn is_conflict(e: &Error) -> bool {
    matches!(e, Error::Constraint { code: "40001", message } if message.contains(CHANGED_IN_POSTGRES))
}

/// After a merge that failed: on a conflict with Postgres, main takes Postgres's rows for the conflicting ones (the
/// merge again then settles them as any conflict). A merge Postgres committed that then failed here (the disk, say)
/// is settled now as after a crash (see `resolve`).
pub(crate) fn after_failed(core: &Core) {
    let Some(after) = AFTER.take() else { return };
    let Some(att) = core.attached.as_deref() else { return };
    let note = match after.committed {
        false => store(core, att, &after.rows).err().map(|e| format!("main couldn't take Postgres's rows: {e}")),
        true => match resolve(core) {
            Ok(said) => said,
            Err(e) => Some(format!("a merge committed in Postgres failed here, and isn't settled yet: {e}")),
        },
    };
    if let Some(n) = note {
        eprintln!("chronos: {n}");
    }
}

/// Puts `rows` (as Postgres has them) into main, in one transaction: upserts parents first, deletes children first.
fn store(core: &Core, att: &Attached, rows: &[Stored]) -> std::result::Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    let db = core.db().ok_or("no database")?;
    let tables = att.tables();
    let mut sql = vec!["begin".to_string()];
    let mut by_table: BTreeMap<usize, Vec<&Stored>> = BTreeMap::new();
    for r in rows {
        by_table.entry(r.table).or_default().push(r);
    }
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let cols: Vec<String> = t.cols.iter().map(|c| q(&c.name)).collect();
        let keys: Vec<String> = t.key.iter().map(|&i| q(&t.cols[i].name)).collect();
        let overriding = if t.cols.iter().any(|c| c.always) { " overriding system value" } else { "" };
        let sets: Vec<String> = (0..t.cols.len())
            .filter(|i| !t.key.contains(i) && !t.cols[*i].always)
            .map(|i| format!("{c} = excluded.{c}", c = q(&t.cols[i].name)))
            .collect();
        let on = match sets.is_empty() {
            true => "do nothing".to_string(),
            false => format!("do update set {}", sets.join(", ")),
        };
        for r in rs.iter().filter(|r| r.row.is_some()) {
            let vals: Vec<String> =
                r.row.as_ref().unwrap().iter().map(|v| v.as_deref().map_or("null".into(), lit)).collect();
            sql.push(format!(
                "insert into {} ({}){overriding} values ({}) on conflict ({}) {on}",
                t.sql,
                cols.join(", "),
                vals.join(", "),
                keys.join(", ")
            ));
        }
    }
    for (&ti, rs) in by_table.iter().rev() {
        let t = &tables[ti];
        for r in rs.iter().filter(|r| r.row.is_none()) {
            let conds: Vec<String> = t
                .key
                .iter()
                .zip(&r.key)
                .map(|(&i, v)| format!("{} = {}", q(&t.cols[i].name), v.as_deref().map_or("null".into(), lit)))
                .collect();
            sql.push(format!("delete from {} where {}", t.sql, conds.join(" and ")));
        }
    }
    sql.push("commit".into());
    let _r = crate::Recovering::enter();
    let skip = SKIP.replace(true);
    let out = crate::sql::run(&db, MAIN, &sql.join(";\n"), &[]);
    SKIP.set(skip);
    out.map(|_| ()).map_err(|e| e.message)
}

/// Whether the write-back that inserted `merge_id` committed: inserting it again from a new connection waits for
/// the first transaction to end if it still runs, then fails with a duplicate key if it committed, or succeeds if
/// not (and is rolled back).
fn committed(merge_id: &str) -> std::result::Result<bool, String> {
    let mut pg = connect()?;
    let start = "begin; set local lock_timeout = '60s'";
    pg.query(start).map_err(|e| e.to_string())?;
    let out = pg.query_params("insert into chronos.merges (id, at) values ($1, now())", &[Some(merge_id)]);
    let done = match out {
        Ok(_) => Ok(false),
        Err(e) if pg_error(&e).is_some_and(|p| p.code == "23505") => Ok(true),
        Err(e) => Err(e.to_string()),
    };
    let _ = pg.query("rollback");
    done
}

/// Settles a write-back whose COMMIT's outcome was lost (a crash, a connection gone): committed, the merge is made
/// in Chronos if it wasn't (the world is still there) and main takes the rows it touched as Postgres has them; not
/// committed, the record goes and the world stays as it was. Err: it can't be told yet (Postgres unreachable, or
/// the first transaction still holds its row).
pub fn resolve(core: &Core) -> std::result::Result<Option<String>, String> {
    let Some(att) = core.attached.as_deref() else { return Ok(None) };
    let _held = att.hold();
    let Ok(text) = std::fs::read_to_string(att.pending_path()) else { return Ok(None) };
    let p: Json = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", att.pending_path().display()))?;
    let (merge_id, world, world_id) =
        (p["merge"].as_str().unwrap_or_default(), p["world"].as_str().unwrap_or_default(), p["world_id"].as_str());
    if !committed(merge_id)? {
        std::fs::remove_file(att.pending_path()).map_err(|e| e.to_string())?;
        return Ok(Some(format!(
            "the merge of {world} written to Postgres before a crash didn't commit there: nothing of it is in Postgres, \
             and {world} is as it was"
        )));
    }
    let db = core.db().ok_or("no database")?;
    let mut said = format!("the merge of {world} committed in Postgres before a crash");
    // the world still there (the crash came before Chronos made the merge): made now, with no second write-back
    let there = db.world(world).ok().is_some_and(|w| Some(w.id.as_str()) == world_id);
    if there {
        let skip = SKIP.replace(true);
        let merged = db.merge_with(world, &crate::MergeOptions::default());
        SKIP.set(skip);
        match merged {
            Ok(_) => said.push_str(&format!("; {world} is merged into main now")),
            Err(e) => said.push_str(&format!(
                "; merging {world} in Chronos failed after ({e}): main takes its rows from Postgres, and {world} \
                 stays open (discard it)"
            )),
        }
    }
    // main takes every row it touched as Postgres has it (what triggers set too)
    let mut rows = vec![];
    let mut pg = connect()?;
    let touched = p["rows"].as_object().cloned().unwrap_or_default();
    let tables = att.tables();
    pg.query(&format!("begin read only; {SETTINGS}")).map_err(|e| e.to_string())?;
    for (name, keys) in touched {
        let Some(ti) = att.table(&name) else { continue };
        let t = &tables[ti];
        let keys: Vec<Vec<Option<String>>> = serde_json::from_value(keys).unwrap_or_default();
        let found = read_rows(&mut pg, t, &keys).map_err(|e| e.to_string())?;
        for k in keys {
            let row = found.get(&k).cloned();
            rows.push(Stored { table: ti, key: k, row });
        }
    }
    let _ = pg.query("rollback");
    drop(pg);
    store(core, att, &rows)?;
    // and the tables its triggers wrote besides (see `others`), read again whole: which rows isn't known any more
    let besides: Vec<usize> = p["others"]
        .as_array()
        .map(|a| a.iter().filter_map(|n| n.as_str().and_then(|n| att.table(n))).collect())
        .unwrap_or_default();
    if !besides.is_empty() {
        refresh_now(core, att, Some(&besides))?;
        let names: Vec<&str> = besides.iter().map(|&ti| tables[ti].name.as_str()).collect();
        said.push_str(&format!("; main took what its triggers wrote in {} from Postgres", names.join(", ")));
    }
    std::fs::remove_file(att.pending_path()).map_err(|e| e.to_string())?;
    Ok(Some(said))
}

/// Rows of `t` with keys `keys` (as Postgres writes them), by key.
fn read_rows(pg: &mut Client, t: &Table, keys: &[Vec<Option<String>>]) -> io::Result<HashMap<Texts, Texts>> {
    let mut params = vec![];
    for k in 0..t.key.len() {
        params.push(text_array(keys.iter().map(|r| r.get(k).and_then(|v| v.as_deref()))));
    }
    let back: Vec<String> = t.cols.iter().map(|c| format!("t.{}::text", q(&c.name))).collect();
    let sql = format!(
        "select {} from {} t join {} on {}",
        back.join(", "),
        t.pg,
        unnest(t, false, &[], 1),
        key_match(t, "t")
    );
    let p: Vec<Option<&str>> = params.iter().map(|s| Some(s.as_str())).collect();
    let rows = pg.query_params(&sql, &p)?;
    Ok(rows.into_iter().map(|r| (t.key.iter().map(|&i| r[i].clone()).collect(), r)).collect())
}

// ---------------------------------------------------------------- refresh

/// Rows of main compared with Postgres in one statement of a refresh.
const REFRESH_BATCH: usize = 2_000;

/// What a refresh did (see `refresh`).
#[derive(Debug, Default)]
pub struct Refreshed {
    /// per table brought up to date: its name, and the rows main took that Postgres inserted, updated and deleted
    pub tables: Vec<(String, usize, usize, usize)>,
    /// columns Postgres added that main took (`table.column type`), before its rows (see `widen`)
    pub added: Vec<String>,
    /// tables left as they were, and why (their shape changed in Postgres)
    pub skipped: Vec<String>,
    /// when Postgres was read (ms since 1970)
    pub at: u64,
}

impl Refreshed {
    pub fn changed(&self) -> usize {
        self.tables.iter().map(|(_, i, u, d)| i + u + d).sum()
    }

    pub fn to_json(&self) -> Json {
        json!({
            "at": self.at,
            "changed": self.changed(),
            "tables": self.tables.iter().map(|(t, i, u, d)| json!({"table": t, "inserted": i, "updated": u, "deleted": d}))
                .collect::<Vec<_>>(),
            "added": self.added,
            "skipped": self.skipped,
        })
    }
}

/// `chronos refresh`, `serve --refresh-every`, HTTP `refresh`: main takes Postgres's rows as they are now, for every
/// attached table, so rows no merge touched don't stay as old as the attach. One read of Postgres (`REPEATABLE READ`,
/// read only: a replica will do), under the write-back lock, so no merge into main runs between the read and main
/// taking what it read. Each of main's rows is compared with Postgres's as the write-back compares them (main's value
/// cast to the column's type there, so a row nobody changed always compares equal), and rows Postgres has that main
/// hasn't are read whole; main takes the differences in one change. Open worlds then see main moved, and settle the
/// rows they changed too as any conflict at their merge. Columns Postgres added to a table are added to main's first
/// (see `widen`), so its rows then take their values; a table whose shape changed otherwise is left as it was, and
/// said.
pub fn refresh(core: &Core) -> std::result::Result<Refreshed, String> {
    refresh_with(core, None)
}

/// `refresh`, of the tables `only` names (by place) if given: then the folder doesn't count as refreshed (`as_of`),
/// as the other tables weren't read.
fn refresh_with(core: &Core, only: Option<&[usize]>) -> std::result::Result<Refreshed, String> {
    let Some(att) = core.attached.as_deref() else {
        return Err("this folder isn't attached to Postgres (chronos attach makes one that is)".into());
    };
    let _held = att.hold();
    if att.pending() {
        resolve(core).map_err(|e| format!("a merge written to Postgres before isn't settled yet: {e}"))?;
    }
    refresh_now(core, att, only)
}

/// `refresh_with`, with the write-back lock held and no write-back left pending (or settling one: see `resolve`).
fn refresh_now(core: &Core, att: &Attached, only: Option<&[usize]>) -> std::result::Result<Refreshed, String> {
    let mut pg = att.lease()?;
    let out = refresh_read(core, att, &mut pg, only);
    rollback(&mut pg);
    drop(pg);
    let mut done = out.map_err(|e| e.to_string())?;
    let rows: Vec<Stored> = std::mem::take(&mut done.1);
    store(core, att, &rows)?;
    let unsettled = settle(core, att);
    done.0.skipped.extend(unsettled);
    if only.is_none() {
        att.refreshed(done.0.at).map_err(|e| e.to_string())?;
    }
    Ok(done.0)
}

/// The read of a refresh: what main is to take, per table (see `refresh`).
fn refresh_read(
    core: &Core,
    att: &Attached,
    pg: &mut Lease,
    only: Option<&[usize]>,
) -> Result<(Refreshed, Vec<Stored>)> {
    let start = format!(
        "begin isolation level repeatable read, read only; set local statement_timeout = '{STATEMENT_TIMEOUT}'; \
         {SETTINGS}; select count(*) from chronos.merges where id = {}",
        lit(&att.marker())
    );
    let found = pg.first(|pg| pg.query(&start)).map_err(|e| failed("starting the refresh", &e))?;
    if found.last().and_then(|r| r[0].as_deref()) != Some("1") {
        return Err(refused(format!(
            "the Postgres CHRONOS_SOURCE_URL names isn't the one this folder was attached to ({}): nothing was refreshed",
            att.source()
        )));
    }
    let mut out = Refreshed { at: crate::now_ms(), ..Default::default() };
    let mut rows = vec![];
    for ti in 0..att.tables().len() {
        if only.is_some_and(|o| !o.contains(&ti)) {
            continue;
        }
        let before = att.tables();
        let t = &before[ti];
        match shape(pg, t)? {
            Shape::Same => {}
            Shape::Added(cols) => {
                let named: Vec<String> =
                    cols.iter().map(|a| format!("{}.{} {}", t.name, a.col.name, a.col.ty)).collect();
                if let Err(e) = widen(core, att, ti, cols) {
                    out.skipped.push(format!(
                        "{}: columns added in Postgres ({}) couldn't be added here: {e}",
                        t.name,
                        named.join(", ")
                    ));
                    continue;
                }
                out.added.extend(named);
            }
            Shape::Changed(diffs) => {
                out.skipped.push(format!(
                    "{}: changed in Postgres since the attach: {}. Attach it again into a new folder",
                    t.name,
                    diffs.join("; ")
                ));
                continue;
            }
        }
        // (as recorded now: with any columns just added, which main's table has too)
        let tables = att.tables();
        let t = &tables[ti];
        let main = core.view(MAIN)?;
        let (mut ins, mut upd, mut del) = (0, 0, 0);
        // main's rows, compared with Postgres's a batch at a time: changed or gone there
        let mut held: Vec<Row> = vec![];
        let mut keys: Vec<Texts> = vec![];
        let prefix = format!("{}/", t.name);
        let mut flush = |held: &mut Vec<Row>, pg: &mut Client| -> Result<()> {
            if held.is_empty() {
                return Ok(());
            }
            let batch: Vec<&Row> = held.iter().collect();
            for c in compare(pg, t, &batch)? {
                if c.changed {
                    match &c.now {
                        Some(_) => upd += 1,
                        None => del += 1,
                    }
                    rows.push(Stored { table: ti, key: c.key, row: c.now });
                }
            }
            held.clear();
            Ok(())
        };
        for r in core.rows(&main.s, &prefix)? {
            let (k, raw) = r?;
            if !k.starts_with(&prefix) {
                break;
            }
            let before = texts(core, &main.s, t, &k, &raw)?;
            let row = Row { key: k, table: ti, before: Some(before), after: None, kept: false, unseen: vec![] };
            keys.push(row.key_texts(t));
            held.push(row);
            if held.len() >= REFRESH_BATCH {
                flush(&mut held, pg)?;
            }
        }
        flush(&mut held, pg)?;
        // rows Postgres has that main hasn't: inserted there since
        let mut params = vec![];
        for k in 0..t.key.len() {
            params.push(text_array(keys.iter().map(|r| r[k].as_deref())));
        }
        let pg_text: Vec<String> = t.cols.iter().map(|c| format!("t.{}::text", q(&c.name))).collect();
        let k_cols: Vec<String> = (0..t.key.len()).map(|n| format!("k{n}")).collect();
        let k_params: Vec<String> = (1..=t.key.len()).map(|n| format!("${n}::text[]")).collect();
        let sql = format!(
            "select {} from {} t where not exists (select 1 from unnest({}) as k({}) where {})",
            pg_text.join(", "),
            t.pg,
            k_params.join(", "),
            k_cols.join(", "),
            key_match(t, "t"),
        );
        for r in run(pg, &format!("reading rows of {} main hasn't", t.pg), &sql, &params)? {
            let key = t.key.iter().map(|&i| r[i].clone()).collect();
            rows.push(Stored { table: ti, key, row: Some(r) });
            ins += 1;
        }
        out.tables.push((t.name.clone(), ins, upd, del));
    }
    Ok((out, rows))
}

/// Adds `added`, columns Postgres added to attached table `ti`, to main's table and then to the folder's record in
/// memory, under the write-back lock (see `refresh`): one ALTER TABLE, made as the database's own change (as `store`
/// writes rows). Main's rows get each column's default here, then Postgres's values from the same refresh; NOT NULL
/// waits for those (`settle`). A default Chronos won't fill existing rows with (one worked out per row, such as
/// `gen_random_uuid()`, on a table with rows: 0A000) waits too: the column comes bare, and Postgres's values fill it.
/// The record is saved only once the refresh is done (`refreshed`): stopped before, the folder opens with the old
/// record, finds the columns again, adds here only those main hasn't, and sets defaults and NOT NULL again. Worlds
/// forked before keep the table as they had it until forked again.
fn widen(core: &Core, att: &Attached, ti: usize, added: Vec<Added>) -> std::result::Result<(), String> {
    let db = core.db().ok_or("no database")?;
    let t = &att.tables()[ti];
    let main = core.view(MAIN).map_err(|e| e.to_string())?;
    let schema = core.val(&main.s, &format!("{}/{}", crate::sql::SCHEMAS, t.name)).map_err(|e| e.to_string())?;
    let has = schema.as_ref().and_then(crate::sql::column_names).unwrap_or_default();
    let alter = |sql: String| {
        let _r = crate::Recovering::enter();
        let skip = SKIP.replace(true);
        let out = crate::sql::run(&db, MAIN, &sql, &[]);
        SKIP.set(skip);
        out.map(|_| ())
    };
    let mut later = vec![];
    for a in &added {
        let bare = format!("alter table {} add column {} {}", t.sql, q(&a.col.name), a.col.ty);
        let default = match &a.default {
            // (already here: a refresh stopped before its record was saved; its default may not be)
            _ if has.contains(&a.col.name) => a.default.clone(),
            Some(e) => match alter(format!("{bare} default {e}")) {
                Ok(()) => None,
                Err(err) if err.code == "0A000" => {
                    alter(bare).map_err(|e| e.message)?;
                    Some(e.clone())
                }
                Err(err) => return Err(err.message),
            },
            None => {
                alter(bare).map_err(|e| e.message)?;
                None
            }
        };
        if default.is_some() || a.not_null {
            later.push((ti, a.col.name.clone(), default, a.not_null));
        }
    }
    att.unsettled.lock().unwrap_or_else(|p| p.into_inner()).extend(later);
    let mut tables = att.tables.write().unwrap_or_else(|p| p.into_inner());
    let mut next = (**tables).clone();
    next[ti].cols.extend(added.into_iter().map(|a| a.col));
    *tables = Arc::new(next);
    Ok(())
}

/// Gives the columns a refresh added the default and NOT NULL that waited for their values (see `widen`), now that
/// main's rows have Postgres's. One that can't be (a row of main still NULL there, say) stays to try at the next
/// refresh, and is said.
fn settle(core: &Core, att: &Attached) -> Vec<String> {
    let mut unsettled = att.unsettled.lock().unwrap_or_else(|p| p.into_inner());
    let Some(db) = core.db() else { return vec![] };
    let tables = att.tables();
    let mut said = vec![];
    unsettled.retain(|(ti, col, default, not_null)| {
        let t = &tables[*ti];
        let mut sets = vec![];
        if let Some(e) = default {
            sets.push(format!("alter column {} set default {e}", q(col)));
        }
        if *not_null {
            sets.push(format!("alter column {} set not null", q(col)));
        }
        let sql = format!("alter table {} {}", t.sql, sets.join(", "));
        let _r = crate::Recovering::enter();
        let skip = SKIP.replace(true);
        let out = crate::sql::run(&db, MAIN, &sql, &[]);
        SKIP.set(skip);
        match out {
            Ok(_) => false,
            Err(e) => {
                said.push(format!(
                    "{}: column {col}'s default or NOT NULL, as Postgres has them, couldn't be set here: {}",
                    t.name, e.message
                ));
                true
            }
        }
    });
    said
}

/// For `chronos serve` on an attached folder: whether the Postgres it was attached to answers as that database, and
/// a pending write-back settled. A line to print either way; merges into main are refused while it fails.
pub fn check(core: &Core) -> std::result::Result<String, String> {
    let Some(att) = core.attached.as_deref() else { return Ok(String::new()) };
    let mut pg = att.lease()?;
    let marker = att.marker();
    let found = pg
        .first(|pg| pg.query_params("select count(*) from chronos.merges where id = $1", &[Some(&marker)]))
        .map_err(|e| format!("reading chronos.merges: {e}"))?;
    if found.first().and_then(|r| r[0].as_deref()) != Some("1") {
        return Err(format!(
            "the Postgres CHRONOS_SOURCE_URL names isn't the one this folder was attached to ({}): merges into main \
             are refused",
            att.source()
        ));
    }
    drop(pg);
    let (what, at) = match att.refreshed_at() {
        0 => ("copied", att.at),
        r => ("refreshed", r),
    };
    let mut line = format!(
        "attached to {} ({} tables, {what} {} s ago): merges into main are written there first",
        att.source(),
        att.tables().len(),
        crate::now_ms().saturating_sub(at) / 1000
    );
    if let Some(s) = resolve(core)? {
        line.push_str(&format!("\n{s}"));
    }
    Ok(line)
}

/// Refuses `b`'s change straight into main of an attached folder (a RESTORE, an UNDO): Postgres holds main's rows,
/// and they change only by a merge written back.
pub(crate) fn refuse_main(core: &Core, what: &str) -> Result<()> {
    match core.attached.as_deref() {
        Some(att) if !SKIP.get() => Err(Error::Constraint {
            code: "42501",
            message: format!(
                "{what} on main isn't done in a folder attached to Postgres ({}): main's rows change only by merging a \
                 world, which writes to Postgres first",
                att.source()
            ),
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_reads_back_as_written() {
        let dir = std::env::temp_dir().join(format!("chronos-attach-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let url = pgclient::parse("postgres://u:secret@db.example.com:6543/shop").unwrap();
        let t = Table {
            name: "app.orders".into(),
            sql: "\"app\".\"orders\"".into(),
            pg: "\"app\".\"orders\"".into(),
            cols: vec![
                Col { name: "id".into(), ty: "uuid".into(), always: false },
                Col { name: "total".into(), ty: "numeric(10,2)".into(), always: false },
            ],
            key: vec![0],
            seqs: vec![],
        };
        let a = Attached::new(&dir, &url, vec![t.clone()]);
        a.save().unwrap();
        let b = Attached::load(&dir).unwrap().unwrap();
        assert_eq!(
            (b.id.as_str(), b.host.as_str(), b.port, b.database.as_str()),
            (a.id.as_str(), "db.example.com", 6543, "shop")
        );
        assert_eq!(*b.tables(), vec![t]);
        // no secret goes into the folder
        assert!(!std::fs::read_to_string(dir.join(FILE)).unwrap().contains("secret"));
        assert!(Attached::load(&dir.join("nowhere")).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn filters_joined_by_or_stay_one_condition() {
        let w = any_of(&["t.a = 1".into(), "t.b = 2".into()]);
        assert_eq!(format!("{w} and not exists (x)"), "((t.a = 1) or (t.b = 2)) and not exists (x)");
    }

    #[test]
    fn statements_take_arrays_never_values() {
        let t = Table {
            name: "t".into(),
            sql: "\"t\"".into(),
            pg: "\"public\".\"t\"".into(),
            cols: vec![
                Col { name: "a".into(), ty: "integer".into(), always: false },
                Col { name: "b".into(), ty: "text".into(), always: false },
            ],
            key: vec![0],
            seqs: vec![],
        };
        assert_eq!(
            unnest(&t, true, &[0, 1], 1),
            "unnest($1::text[], $2::bool[], $3::text[], $4::text[]) with ordinality as k(k0, p, c0, c1, i)"
        );
        assert_eq!(key_match(&t, "t"), "(t.\"a\") = (k.k0::integer)");
        let r = Row {
            key: "t/1".into(),
            table: 0,
            before: None,
            after: Some(vec![Some("1".into()), Some("x\"y".into())]),
            kept: false,
            unseen: vec![],
        };
        assert_eq!(arrays(&[&r], &t, true, &[0, 1], true), ["{\"1\"}", "{f}", "{\"1\"}", "{\"x\\\"y\"}"]);
        assert_eq!(text_array([None, Some("a\\b")]), "{NULL,\"a\\\\b\"}");
    }

    /// A conflict with Postgres, a stale read and Postgres being busy share 40001: merge outcomes count them apart.
    #[test]
    fn a_conflict_is_told_from_other_refusals() {
        let pg = conflict(2, "w", "shop on db.example.com", &["  t/1: ...".into()]);
        assert!(is_conflict(&pg) && !crate::stale::is_stale(&pg));
        let stale = crate::stale::stale("w", "merging", &["read t/1, which changed since".into()]);
        assert!(crate::stale::is_stale(&stale) && !is_conflict(&stale));
        let busy = Error::Constraint {
            code: "40001",
            message: "Postgres, writing t: lock timeout; nothing was written: safe to merge again".into(),
        };
        assert!(!is_conflict(&busy) && !crate::stale::is_stale(&busy));
    }
}
