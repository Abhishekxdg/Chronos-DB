//! Studio's Import from Postgres: the import `chronos import <folder> postgres://...` runs (see `pgimport`), into
//! the open database, as a job on a thread of its own that the page polls and may cancel.
//!
//!   import_postgres {host, port, database, user, password, sslmode, include, dry_run,
//!                    allow_plaintext_password, world, new_world, same_world}   -> {job, world, forked}
//!   import_status {job}   -> how far it is (tables, rows), then its report and each table's rows
//!   import_cancel {job}   -> stops it between tables, or in the middle of a table's COPY
//!
//! The password comes only in the request's body: it is handed to the connection and wiped once connected, and is
//! never kept with the job, logged or sent back (a failure's message is the connection's, which never holds it).
//! A database that already has tables is imported into a new world, forked from the one named, unless
//! `same_world` says otherwise: the result is reviewed in the Changes view and merged, or discarded.
//! One import runs at a time.

use crate::api::Fail;
use crate::pgclient::Url;
use crate::pgimport::{self, Progress};
use chronos::Db;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};

/// An import that runs, or ran: what it imports into, how far it is, and how it ended.
struct Job {
    world: String,
    forked: bool,
    dry: bool,
    database: String,
    started: u64,
    progress: Arc<Progress>,
    end: Mutex<Option<End>>,
}

/// How a job ended: its report, or why it stopped.
struct End {
    at: u64,
    result: Result<Value, String>,
}

/// The jobs of this process, by id: the one running and the last few that ended.
static JOBS: LazyLock<Mutex<HashMap<String, Arc<Job>>>> = LazyLock::new(Default::default);

/// Jobs that ended, kept for the page to read: older ones go.
const KEPT: usize = 8;

fn bad(msg: impl Into<String>) -> Fail {
    Fail(400, Value::String(msg.into()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A job's id: 128 random bits, as hex.
fn job_id() -> Result<String, Fail> {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut b).map_err(|_| Fail(500, json!("no random numbers")))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Whether world `w` has a table: one with a SQL schema, or rows of one made by writing JSON rows.
fn has_tables(db: &Db, w: &str) -> Result<bool, Fail> {
    if !db.prefix(w, "_sonos_tables/")?.is_empty() {
        return Ok(true);
    }
    let mut from = String::new();
    while let Some((k, _)) = db.scan(w, &from, 1)?.pop() {
        let t = k.split('/').next().unwrap_or(&k).to_string();
        if !t.starts_with("_sonos") {
            return Ok(true);
        }
        from = format!("{t}0"); // '0' sorts right after '/': skips the rest of the table
    }
    Ok(false)
}

/// The Studio ops of this module, or None for another op.
pub fn call(db: &Arc<Db>, op: &str, a: &Value) -> Option<Result<Value, Fail>> {
    Some(match op {
        "import_postgres" => start(db, a),
        "import_status" => job(a).map(|j| status(&j)),
        "import_cancel" => job(a).map(|j| {
            j.progress.stop.store(true, Ordering::Relaxed);
            json!({ "ok": true })
        }),
        _ => return None,
    })
}

fn job(a: &Value) -> Result<Arc<Job>, Fail> {
    let id =
        a.get("job").and_then(Value::as_str).ok_or_else(|| bad("'job' is required (the id import_postgres gave)"))?;
    let jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
    jobs.get(id).cloned().ok_or_else(|| Fail(404, json!(format!("no import {id}"))))
}

/// Where to connect, from the form's fields. The password is the request's: it goes into the URL the connection
/// reads, and nowhere else.
fn url(a: &Value) -> Result<Url, Fail> {
    let s = |k: &str| a.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    let host = s("host").unwrap_or("localhost");
    if host.starts_with('/') {
        return Err(bad("Unix sockets aren't supported: connect over TCP (host and port)"));
    }
    let port = match a.get("port") {
        None | Some(Value::Null) => 5432,
        Some(Value::Number(n)) => {
            n.as_u64().and_then(|p| u16::try_from(p).ok()).ok_or_else(|| bad("'port' must be 1 to 65535"))?
        }
        Some(Value::String(p)) if p.trim().is_empty() => 5432,
        Some(Value::String(p)) => p.trim().parse().map_err(|_| bad(format!("'port' must be a number, not {p}")))?,
        Some(_) => return Err(bad("'port' must be a number")),
    };
    let user = s("user").ok_or_else(|| bad("'user' is required: the Postgres role to read the database as"))?;
    let database = s("database").unwrap_or(user);
    let sslmode = s("sslmode").unwrap_or("prefer");
    if !["disable", "allow", "prefer", "require", "verify-ca", "verify-full"].contains(&sslmode) {
        return Err(bad(format!("sslmode {sslmode} isn't one of disable, prefer, require, verify-ca, verify-full")));
    }
    // (not trimmed: a password is as typed)
    let password = a.get("password").and_then(Value::as_str).filter(|p| !p.is_empty()).map(String::from);
    Ok(Url {
        host: host.to_string(),
        port,
        user: user.to_string(),
        password,
        database: database.to_string(),
        sslmode: sslmode.to_string(),
        sslrootcert: None,
        channel_binding: "prefer".into(),
        connect_timeout: 30,
        plain_password: a.get("allow_plaintext_password").and_then(Value::as_bool) == Some(true),
    })
}

/// The schemas and `schema.table`s to bring over: a list, or text split at commas and spaces.
fn include(a: &Value) -> Result<Vec<String>, Fail> {
    let items: Vec<String> = match a.get("include") {
        None | Some(Value::Null) => vec![],
        Some(Value::String(t)) => t.split([',', ' ', '\n']).map(str::to_string).collect(),
        Some(Value::Array(v)) => v
            .iter()
            .map(|x| x.as_str().map(String::from).ok_or_else(|| bad("'include' must be names")))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(bad("'include' must be a list of schemas and schema.tables")),
    };
    let items: Vec<String> = items.into_iter().map(|i| i.trim().to_string()).filter(|i| !i.is_empty()).collect();
    if let Some(i) = items.iter().find(|i| i.matches('.').count() > 1 || i.starts_with('.') || i.ends_with('.')) {
        return Err(bad(format!("{i} isn't a schema or schema.table")));
    }
    Ok(items)
}

fn start(db: &Arc<Db>, a: &Value) -> Result<Value, Fail> {
    let dry = a.get("dry_run").and_then(Value::as_bool) == Some(true);
    let url = url(a)?;
    let include = include(a)?;
    let from = a.get("world").and_then(Value::as_str).filter(|w| !w.is_empty()).unwrap_or(chronos::MAIN);
    if db.private_world(from) {
        return Err(Fail(404, json!(format!("no world {from}"))));
    }
    let from = db.world(from)?.name;
    let mut jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
    if jobs.values().any(|j| j.end.lock().unwrap_or_else(|p| p.into_inner()).is_none()) {
        return Err(Fail(409, json!("an import is already running: wait for it, or cancel it")));
    }
    // into a new world when asked, or when the world has tables (unless told to import beside them)
    let named = a.get("new_world").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty());
    let same = a.get("same_world").and_then(Value::as_bool) == Some(true);
    let (world, forked) = match named {
        Some(n) if !dry => (n.to_string(), true),
        None if !dry && !same && has_tables(db, &from)? => {
            let base: String = url.database.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            // (to the second, and numbered past a world already there: two imports never take one name)
            let first = format!("import_{base}_{}", stamp());
            let mut name = first.clone();
            for n in 2.. {
                if db.world(&name).is_err() {
                    break;
                }
                name = format!("{first}_{n}");
            }
            (name, true)
        }
        _ => (from.clone(), false),
    };
    if forked {
        db.fork_with(&from, &world, json!({ "import": format!("Postgres {}", url.database) }))?;
    }
    // ended jobs past the last few go
    if jobs.len() >= KEPT {
        let mut ended: Vec<(u64, String)> = jobs
            .iter()
            .filter_map(|(id, j)| j.end.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|e| (e.at, id.clone())))
            .collect();
        ended.sort();
        for (_, id) in ended.iter().take(jobs.len() + 1 - KEPT) {
            jobs.remove(id);
        }
    }
    let id = job_id()?;
    let job = Arc::new(Job {
        world: world.clone(),
        forked,
        dry,
        database: url.database.clone(),
        started: now_ms(),
        progress: Arc::default(),
        end: Mutex::new(None),
    });
    jobs.insert(id.clone(), job.clone());
    drop(jobs);
    let (db, target) = (db.clone(), world.clone());
    let run = move || {
        let report = pgimport::import(Some(&db), url, &target, dry, &include, job.progress.clone(), false);
        let result = report.map(|r| {
            let error = pgimport::unenforced(&r);
            json!({
                "imported": r.imported, "total": r.total, "rows": r.rows,
                "done": r.done, "carried": r.carried, "partial": r.partial, "skipped": r.skipped,
                "unenforced": r.unenforced, "error": error,
            })
        });
        *job.end.lock().unwrap_or_else(|p| p.into_inner()) = Some(End { at: now_ms(), result });
    };
    if let Err(e) = std::thread::Builder::new().name("studio-import".into()).spawn(run) {
        JOBS.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
        return Err(Fail(500, json!(format!("couldn't start the import: {e}"))));
    }
    Ok(json!({ "job": id, "world": world, "forked": forked, "dry_run": dry }))
}

/// `YYYYMMDD_HHMMSS` in UTC, for a new world's name.
fn stamp() -> String {
    let secs = now_ms() / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // days since 1970-01-01 to a month and day (Howard Hinnant's civil_from_days)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}_{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A job as the page reads it: running (with how far), or done, failed or canceled (with its report).
fn status(j: &Job) -> Value {
    let p = &j.progress;
    let tables: Vec<Value> = p
        .per_table
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(name, rows)| json!({ "name": name, "rows": rows }))
        .collect();
    let mut v = json!({
        "world": j.world, "forked": j.forked, "dry_run": j.dry, "database": j.database, "started": j.started,
        "tables_total": p.tables.load(Ordering::Relaxed), "tables_done": p.done.load(Ordering::Relaxed),
        "rows": p.rows.load(Ordering::Relaxed), "now": p.now.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        "tables": tables,
    });
    match &*j.end.lock().unwrap_or_else(|e| e.into_inner()) {
        None => v["state"] = json!(if p.stop.load(Ordering::Relaxed) { "canceling" } else { "running" }),
        Some(End { at, result: Ok(r) }) => {
            v["state"] = json!("done");
            v["finished"] = json!(at);
            v["report"] = r.clone();
        }
        Some(End { at, result: Err(e) }) => {
            v["state"] = json!(if e == pgimport::CANCELED { "canceled" } else { "failed" });
            v["finished"] = json!(at);
            v["error"] = json!(e);
        }
    }
    v
}
