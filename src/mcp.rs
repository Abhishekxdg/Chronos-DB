//! `chronos mcp <folder>`: the database as MCP tools for AI agents (JSON-RPC 2.0, one message per
//! line on stdin/stdout; or over HTTP from `chronos serve`, as the agent whose token is sent: see `http`). Tools reuse the shell's commands, so agents get the same count-first
//! previews and errors that say how to fix the call. Every tool takes an explicit `branch`
//! (default main) instead of shell-style "current branch" state, since agents call tools independently.
//!
//! Safe mode (the default; `--allow-merge` turns it off) is the approval step: the agent reads
//! anything and forks, but changes only worlds it forked in this session, and never merges,
//! restores or touches other worlds. A person reads the diff and merges in the `chronos` shell.
//!
//!   tools/call ──> safe? ── no ──────────────────────────────────────────> run
//!                    └ yes: merge/restore ─────────────────────────────────> refuse
//!                           sql with a world statement (MERGE, RESTORE, ...) ──> refuse
//!                           a write (put/delete/set_meta/discard/sql write)
//!                             on no branch, main, or a world not forked here ──> refuse
//!                           otherwise ──> run; a fork adds its name to `owned`
//!
//! Merge policies (see `chronos::agents`) are the rules under which an agent's merges go through
//! without a person. Any session reads them, tries rules on a branch (`check_merge_policy`) and
//! lists merges waiting (`reviews`); setting them needs `admin`. In safe mode an agent may draft
//! rules: `set_merge_policy` answers with the SQL for the person to run. Merge checks (see
//! `chronos::MergeCheck`), queries a merge's result must find nothing in, go the same way:
//! `merge_checks` lists them, and `set_merge_check` and `drop_merge_check` need `admin`.

use crate::{MAIN, PREVIEW, Shell};
use chronos::sql::Reach;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, Write};

const PROTOCOL: &str = "2025-06-18";

/// For a session that may merge (`--allow-merge`, or an agent with `merge` or `merge_own`).
const INSTRUCTIONS: &str = "Chronos is a database built for agents. Rows are JSON objects in tables, \
addressed by table and id. Workflow: call describe to learn the tables and fields; fork your own \
branch from main before changing anything (instant, no copying); make changes on your branch; \
check them with diff; then merge to publish or discard to throw them away. Your work isn't live \
until it's merged, so when the diff is right, call merge yourself instead of handing the branch to \
a person. Reads and writes take a branch argument (default main). If merge reports conflicts, decide \
which side wins and call merge again with resolve set to ours or theirs. A merge policy may hold \
your merge for a person: if merge says it needs a person's review, it is queued, not lost: stop, \
and tell the person the branch and the reasons. Merge policies are the rules under which agents' \
merges go through without a person (merge_policies lists them, check_merge_policy tries rules on a \
branch's real changes, set_merge_policy writes them, reviews lists merges waiting for a person). \
Merge checks are SQL queries every merge's result must find nothing in (merge_checks lists them); a \
merge that breaks one is refused with the rows it found.";

/// For a session that may not merge: a person does.
const SAFE_INSTRUCTIONS: &str = "Chronos is a database built for agents. Rows are JSON objects in tables, \
addressed by table and id. Workflow: call describe to learn the tables and fields; fork your own \
branch before changing anything (instant, no copying); make changes on your branch, passing its \
name as branch; check them with diff. You can read any branch, but you can only change branches you \
forked, and you can't merge: a person reviews your diff and merges it from a terminal with \
`chronos <database folder> merge <branch>` (there is no web UI). When you're done, tell them the branch \
name, what changed, and that command. If merge says it needs a person's review, a merge policy sent \
it to one: stop, and tell the person the branch and the reasons. You may draft merge policies (the \
rules under which agents' merges go through without a person): start from max_deletes = 0, so no \
delete merges without a person, try them with check_merge_policy, then set_merge_policy gives the \
SQL for the person to run.";

/// Tools a safe-mode agent doesn't get: approving changes is the person's job.
const HIDDEN: &[&str] = &["merge", "restore", "undo_merge"];

/// Safe mode's state: whether it's on, and the worlds this session forked. With `--agent`, the
/// session acts as that agent instead: its rights and quotas decide, its forks are its own across
/// sessions, and what it writes is in the audit trail (see `chronos::agents`).
pub struct Guard {
    safe: bool,
    owned: HashSet<String>,
    agent: Option<String>,
}

impl Guard {
    pub fn new(allow_merge: bool) -> Guard {
        Guard { safe: !allow_merge, owned: HashSet::new(), agent: None }
    }

    pub fn agent(name: &str) -> Guard {
        Guard { safe: false, owned: HashSet::new(), agent: Some(name.into()) }
    }

    /// Whether this session may merge its own branches: the `merge` tool is offered.
    fn may_merge(&self, db: &chronos::Db) -> bool {
        match &self.agent {
            Some(n) => db.agent(n).is_ok_and(|a| a.may(chronos::Can::Merge) || a.may(chronos::Can::MergeOwn)),
            None => !self.safe,
        }
    }

    /// What the server tells the model: a session that may merge is told to, one that may not is
    /// told a person does.
    fn instructions(&self, db: &chronos::Db) -> &'static str {
        if self.may_merge(db) { INSTRUCTIONS } else { SAFE_INSTRUCTIONS }
    }

    /// Tools this session doesn't get.
    fn hidden(&self, db: &chronos::Db) -> Vec<&'static str> {
        match &self.agent {
            Some(n) => {
                let a = db.agent(n).ok();
                let may = |c: chronos::Can| a.as_ref().is_some_and(|a| a.may(c));
                let mut h = vec![];
                if !self.may_merge(db) {
                    h.push("merge");
                }
                if !may(chronos::Can::Restore) {
                    h.extend(["restore", "rollback"]);
                }
                let bound = a.as_ref().is_some_and(|a| a.policy.is_some()); // undoing goes past its rules
                if bound || !(may(chronos::Can::Admin) || may(chronos::Can::Restore) && may(chronos::Can::Merge)) {
                    h.push("undo_merge");
                }
                if !may(chronos::Can::Admin) {
                    h.extend(["set_merge_policy", "drop_merge_policy", "set_merge_check", "drop_merge_check"]);
                }
                h
            }
            None if self.safe => HIDDEN.to_vec(),
            None => vec![],
        }
    }

    /// An agent's call: whether it may (see `chronos::Acting`).
    fn check_agent(&self, db: &chronos::Db, agent: &str, name: &str, a: &Value) -> Result<(), String> {
        let me = db.as_agent(agent).map_err(|e| e.to_string())?;
        let branch = a.get("branch").and_then(Value::as_str).filter(|b| !b.is_empty()).unwrap_or(MAIN);
        let r = match name {
            "put" | "delete" => me.may_write(branch, 1),
            "rebase" => me.may_write(branch, 0),
            "merge" => match a.get("into").and_then(Value::as_str) {
                Some(into) => me.may_merge_into(branch, into),
                None => me.may_merge(branch),
            },
            "restore" => me.may_restore(branch),
            "discard" if a.get("cascade").and_then(Value::as_bool) == Some(true) => me.may_manage_tree(branch),
            "set_meta" | "discard" => me.may_manage(branch),
            "fork" => Ok(()), // checked as it forks
            // run as SQL in a session acting as the agent: checked statement by statement
            "sql" | "undo_merge" | "checkpoint" | "rollback" => Ok(()),
            "simulate" | "replay" => Ok(()), // checked as they run; the worlds are its own
            "merge_preview" => me.may_read(branch),
            _ => me.may_read(branch),
        };
        r.map_err(|e| e.to_string())?;
        // row-level security: the row tools work on rows as they are, which a policy limiting the
        // agent's role forbids (SQL applies it)
        let op = match name {
            "merge_preview" => "merge",
            n => n,
        };
        crate::api::row_security(db, agent, op, a)
            .map_err(|f| f.1.get("message").and_then(Value::as_str).map_or_else(|| f.1.to_string(), String::from))
    }

    /// Refuses a call safe mode doesn't allow, saying what to do instead.
    fn check(&self, db: &chronos::Db, name: &str, a: &Value) -> Result<(), String> {
        if !self.safe {
            return Ok(());
        }
        if HIDDEN.contains(&name) {
            return Err(format!(
                "safe mode: agents can't {name}. A person reviews your diff and runs \
                 `chronos <database folder> merge <branch>` in a terminal; tell them your branch name"
            ));
        }
        let writes = match name {
            "put" | "delete" | "set_meta" | "discard" | "checkpoint" | "rollback" | "rebase" => true,
            "sql" => match chronos::sql::parse(a["sql"].as_str().unwrap_or("")) {
                // a statement that doesn't parse never runs: the engine reports it
                Err(_) => false,
                Ok(stmts) => {
                    if stmts.iter().any(|s| s.access() == Reach::World) {
                        return Err("safe mode: agents can't fork, merge, restore, drop or switch worlds in SQL, \
                                    or change system settings. Use the fork tool for a new branch; a person merges it \
                                    with `chronos <database folder> merge <branch>` in a terminal"
                            .into());
                    }
                    // where it runs: a view there may call functions that change something
                    let at = a.get("branch").and_then(Value::as_str).filter(|b| !b.is_empty()).unwrap_or(MAIN);
                    if stmts.iter().any(|s| s.moves_counters(db, at)) {
                        return Err("safe mode: agents can't restart or set sequences (setval, ALTER SEQUENCE ... \
                                    RESTART, TRUNCATE ... RESTART IDENTITY): every branch shares their counters"
                            .into());
                    }
                    stmts.iter().any(|s| s.access_in(db, at) == Reach::Write)
                }
            },
            _ => false,
        };
        if !writes {
            return Ok(());
        }
        match a.get("branch").and_then(Value::as_str).filter(|b| !b.is_empty()) {
            None => Err("safe mode: changes need branch=\"<a branch you forked>\". Call fork first".into()),
            Some(b) if !self.owned.contains(b) => Err(format!(
                "safe mode: you can only change branches you forked in this session, and '{b}' isn't one. \
                 Call fork, then pass its name as branch"
            )),
            Some(b) if name == "discard" && a.get("cascade").and_then(Value::as_bool) == Some(true) => {
                match db.descendants(b).into_iter().find(|d| !self.owned.contains(d)) {
                    Some(d) => Err(format!(
                        "safe mode: '{d}' was forked from '{b}' outside this session, so cascade can't discard it"
                    )),
                    None => Ok(()),
                }
            }
            Some(_) => Ok(()),
        }
    }
}

/// MCP's Streamable HTTP transport, as `chronos serve` answers `POST /mcp`: one JSON-RPC message per request, its
/// reply as the response's JSON body (no event stream). The caller is `agent`, the agent whose token came with the
/// request (the guest under `--safe`), as with `chronos mcp --agent`; so a client needs only that token, never the
/// folder. Nothing carries over between requests, as nothing does between stdio's calls (each `sql` call is a
/// session of its own). Returns the status and the reply: none (202) for a notification or a client's response.
pub fn http(db: &std::sync::Arc<chronos::Db>, agent: &str, body: &[u8]) -> (u16, Option<Value>) {
    let req = match std::str::from_utf8(body)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_str::<Value>(b).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(e) => return (400, Some(error(Value::Null, -32700, &format!("parse error: {e}")))),
    };
    if req.is_array() {
        return (400, Some(error(Value::Null, -32600, "send one JSON-RPC message per request, not a batch")));
    }
    if req.get("method").is_none() {
        return (202, None); // a response to the server: it asks nothing, so nothing awaits it
    }
    let mut sh = Shell { db: db.clone(), branch: MAIN.into(), sql: chronos::sql::Session::new(MAIN) };
    match handle(&mut sh, &mut Guard::agent(agent), &req) {
        Some(reply) => (200, Some(reply)),
        None => (202, None),
    }
}

pub fn serve(sh: Shell, g: Guard) {
    serve_io(sh, g, std::io::stdin().lock(), std::io::stdout().lock());
}

/// The MCP session on any line stream: stdio, or another process's connection (see local.rs).
pub fn serve_io(mut sh: Shell, mut g: Guard, input: impl BufRead, mut out: impl Write) {
    for line in input.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
            Ok(req) => handle(&mut sh, &mut g, &req),
        };
        if let Some(r) = reply {
            // stdout carries only protocol messages; a closed pipe means the client is gone
            if writeln!(out, "{r}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
}

fn handle(sh: &mut Shell, g: &mut Guard, req: &Value) -> Option<Value> {
    let id = req.get("id")?.clone(); // no id: a notification, which gets no reply
    let params = &req["params"];
    let result = match req["method"].as_str().unwrap_or("") {
        "initialize" => json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "chronos", "version": env!("CARGO_PKG_VERSION") },
            "instructions": g.instructions(&sh.db),
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools(&g.hidden(&sh.db)) }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let (text, is_error) = match call(sh, g, name, &params["arguments"]) {
                Ok(t) => (t, false),
                Err(e) => (e, true),
            };
            json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
        }
        m => return Some(error(id, -32601, &format!("method not found: {m}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn call(sh: &mut Shell, g: &mut Guard, name: &str, a: &Value) -> Result<String, String> {
    // a session's transaction world (`_tx_...`) is private to it: no tool reaches one by name
    for k in ["branch", "from", "to", "into", "world", "as"] {
        if let Some(w) = a.get(k).and_then(Value::as_str).filter(|w| w.starts_with("_tx_")) {
            return Err(format!("no world {w}: names starting with _tx_ are other sessions' transactions"));
        }
    }
    // (by ID too, and `name@time`: the HTTP API's guard)
    crate::api::private_worlds(&sh.db, a).map_err(|f| f.1.as_str().map_or_else(|| f.1.to_string(), String::from))?;
    let db = sh.db.clone();
    let (_as, _slot) = match g.agent.clone() {
        Some(agent) => {
            g.check_agent(&db, &agent, name, a)?;
            // its max_concurrent, as over the HTTP API: SQL takes a slot per statement, everything else one per call
            let slot = match name {
                "sql" => None,
                _ => db.slot(&db.as_agent(&agent).map_err(|e| e.to_string())?.agent).map_err(|e| e.to_string())?,
            };
            (Some(chronos::acting_as(&agent)), slot) // what this call writes is the agent's
        }
        None => {
            g.check(&db, name, a)?;
            (None, None)
        }
    };
    let me = g.agent.clone();
    let me = me.as_deref(); // the SQL a tool runs is checked and limited as the agent's, as its own `sql` is
    // what the check can't see (a prepared statement, a function's body, a trigger) is refused
    // where a counter would move
    let _fixed = g.safe.then(chronos::fixed_counters);
    let opt = |k: &str| a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    // names become shell words, so they must be single tokens
    let word = |k: &str| -> Result<&str, String> {
        opt(k)
            .filter(|s| !s.contains(char::is_whitespace))
            .ok_or_else(|| format!("'{k}' is required: a non-empty string without spaces"))
    };
    sh.branch = opt("branch").unwrap_or(MAIN).into();
    match name {
        "describe" => {
            // a table a row-level security policy limits the agent on shows no rows here: SQL applies the policy
            let limited = |t: &str| {
                me.is_some_and(|ag| {
                    chronos::sql::row_security_possible(&db)
                        && !matches!(chronos::sql::row_security_refuses(&db, ag, &sh.branch, &[t]), Ok(None))
                })
            };
            sh.describe(opt("table").unwrap_or(""), &limited)
        }
        "get" => sh.exec(&format!("get {} {}", word("table")?, word("id")?)),
        "put" => {
            let rec = a.get("record").ok_or("'record' is required: the row as a JSON object")?;
            sh.exec(&format!("put {} {} {rec}", word("table")?, word("id")?))
        }
        "delete" => sh.exec(&format!("del {} {}", word("table")?, word("id")?)),
        "find" => {
            let filters = match a.get("where") {
                None | Some(Value::Null) => vec![],
                Some(Value::Object(o)) => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                Some(_) => return Err("'where' must be an object of field: value, e.g. {\"role\": \"admin\"}".into()),
            };
            let vector = match (opt("vector_field"), a.get("vector")) {
                (Some(f), Some(v)) => Some((
                    f.to_string(),
                    serde_json::from_value::<Vec<f32>>(v.clone())
                        .map_err(|_| "'vector' must be an array of numbers")?,
                )),
                (None, None) => None,
                _ => return Err("give both 'vector' and 'vector_field' (the field holding the stored vectors)".into()),
            };
            let q = chronos::Query {
                table: word("table")?.into(),
                filters,
                text: opt("text").map(String::from),
                vector,
                k: bounded(a, "limit", PREVIEW, MAX_PAGE)?,
                offset: bounded(a, "offset", 0, MAX_OFFSET)?,
                ef: bounded(a, "ef", 0, MAX_EF)?,
            };
            sh.run_find(&q)
        }
        "sql" => {
            let params: Vec<chronos::sql::Value> = match a.get("params") {
                None | Some(Value::Null) => vec![],
                Some(Value::Array(ps)) => ps.iter().map(chronos::sql::Value::from_json).collect(),
                Some(_) => return Err("'params' must be an array of values for $1, $2, ...".into()),
            };
            sh.sql_as(opt("sql").ok_or("'sql' is required")?, &params, g.agent.as_deref())
        }
        "branches" => sh.exec("branches"),
        "fork" => {
            let from = opt("from").unwrap_or(MAIN);
            let name = word("name")?;
            let meta = a.get("meta").cloned().unwrap_or(Value::Null);
            let w = match &g.agent {
                Some(agent) => sh.db.as_agent(agent).and_then(|me| me.fork(from, name, meta)),
                None => sh.db.fork_with(from, name, meta),
            }
            .map_err(|e| e.to_string())?;
            sh.branch = name.into();
            g.owned.insert(name.into());
            Ok(format!("created branch {name} (world id {}) from {from}; pass branch=\"{name}\" to work on it", w.id))
        }
        "set_meta" => {
            let meta = a.get("meta").cloned().ok_or("'meta' must be a JSON object")?;
            let w = sh.db.set_meta(word("branch")?, meta).map_err(|e| e.to_string())?;
            Ok(format!("{}: {}", w.name, w.meta))
        }
        "diff" => {
            // a big change would flood the agent's context (and resend it with every call after),
            // so it gets the count and a page of rows; the person's shell diff shows them all
            let limit = a.get("limit").and_then(Value::as_u64).map_or(DIFF_PAGE, |n| n as usize);
            let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            if a.get("count").and_then(Value::as_bool) == Some(true) {
                // how big the change is, per table, without reading it into memory
                let b = word("branch")?;
                let tables = sh.db.diff_count(b, opt("to")).map_err(|e| e.to_string())?;
                let kinds = sh.db.diff_kinds(b, opt("to")).map_err(|e| e.to_string())?;
                // per table, deletes first: `leads: 4,812 deleted, 69 updated, 0 inserted`
                let summary = chronos::sql::summary(kinds);
                let per: Vec<String> = tables.iter().map(|(t, n)| format!("{t}: {n}")).collect();
                let summary = if summary.is_empty() { per.join(", ") } else { summary };
                // and what its merge would send (NOTIFY ON MERGE)
                let effects = match opt("to") {
                    None => chronos::sql::effects_line(&sh.db.world_effects(b).map_err(|e| e.to_string())?),
                    Some(_) => String::new(),
                };
                let effects = if effects.is_empty() { String::new() } else { format!("; effects on merge: {effects}") };
                return Ok(format!("{} changes on {b} ({summary}){effects}", tables.values().sum::<usize>()));
            }
            if let Some(after) = a.get("after").and_then(Value::as_str) {
                // a page after a cursor: reads only this page, in storage key order
                let b = word("branch")?;
                let (d, next) = sh
                    .db
                    .diff_page(b, opt("to"), Some(after).filter(|s| !s.is_empty()), limit)
                    .map_err(|e| e.to_string())?;
                let mut lines: Vec<String> = d
                    .iter()
                    .map(|c| {
                        let k = chronos::sql::show_key(&c.key);
                        match (&c.before, &c.after) {
                            (None, Some(a)) => format!("+ {k}  {a}"),
                            (Some(x), Some(y)) => format!("~ {k}  {}", crate::field_diff(x, y)),
                            _ => format!("- {k}"),
                        }
                    })
                    .collect();
                lines.push(match next {
                    Some(n) => format!("(call diff again with after={} for the next ones)", json!(n)),
                    None => "(no more changes)".into(),
                });
                return Ok(lines.join("\n"));
            }
            if a.get("readers").and_then(Value::as_bool) == Some(true) {
                // what reads the changed columns: short, so not paged
                let to = opt("to").map_or(String::new(), |t| format!(" to {}", lit(t)));
                return sh.sql_as(&format!("diff world {}{to} readers", lit(word("branch")?)), &[], me);
            }
            if a.get("as_sql").and_then(Value::as_bool) == Some(true) {
                let to = opt("to").map_or(String::new(), |t| format!(" to {}", lit(t)));
                let out = sh.sql_as(&format!("diff world {}{to} as sql", lit(word("branch")?)), &[], me)?;
                return Ok(page(out, 2, 1, offset, limit));
            }
            match opt("to") {
                Some(to) => {
                    let (from, to) = (word("branch")?.replace('\'', "''"), to.replace('\'', "''"));
                    // a table: 2 header lines, a row per change, then "(n rows)"
                    Ok(page(sh.sql_as(&format!("diff world '{from}' to '{to}'"), &[], me)?, 2, 1, offset, limit))
                }
                // "N changes on b", then a line per change
                None => Ok(page(sh.exec(&format!("diff {}", word("branch")?))?, 1, 0, offset, limit)),
            }
        }
        "history" => {
            let limit = bounded(a, "limit", 30, MAX_PAGE)?;
            let b = word("branch")?.replace('\'', "''");
            sh.sql_as(&format!("show history for world '{b}' limit {limit}"), &[], me)
        }
        "restore" => {
            let b = word("branch")?.replace('\'', "''");
            let at = chronos::sql::Value::Text(
                opt("at").ok_or("'at' is required: a time, or a time ago like -10 minutes")?.into(),
            );
            sh.sql_as(&format!("restore world '{b}' to $1"), &[at], me)
        }
        "merge_preview" => {
            let limit = a.get("limit").and_then(Value::as_u64).map_or(DIFF_PAGE, |n| n as usize);
            let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
            // the version on both sides of the dry run, and the world it merges into's stamp: the
            // same, and the plan is of them
            let b = word("branch")?;
            let into = match opt("into") {
                Some(y) => y.to_string(),
                None => sh.db.world(b).map_err(|e| e.to_string())?.parent.unwrap_or_else(|| MAIN.into()),
            };
            let db = sh.db.clone();
            let ends = || -> Result<(u64, u64), String> {
                Ok((db.version(b).map_err(|e| e.to_string())?, db.stamp(&into).map_err(|e| e.to_string())?))
            };
            let (before, target) = ends()?;
            let out = sh.sql_as(&format!("merge world {} dry run", merge_clauses(b, a)?), &[], me)?;
            let (after, target_after) = ends()?;
            // and what in the database reads the columns the branch changes
            let readers = sh.sql_as(&format!("diff world {} readers", lit(word("branch")?)), &[], me)?;
            // a later page carries the first page's version: a branch changed since is refused, else
            // a row added before the offset would be skipped yet in the version merged at
            if let Some(v) = a.get("version").and_then(Value::as_u64)
                && (before != v || after != v)
            {
                return Err(format!(
                    "the branch changed since version {v}, which this preview started at: preview it again from \
                     offset 0, without version"
                ));
            }
            let version = match before == after && target == target_after {
                true => format!(
                    "The branch is at version {before}: pass version={before} with the next pages (offset) and with \
                     merge, to merge exactly what the pages show (refused if the branch changes first). With rows \
                     settled (resolve, by_columns, rows), pass target={target} to merge too, so they settle against \
                     {into} as this saw it."
                ),
                false => "The branch was written to while this ran, so it may show less than is there: preview \
                          again before merging."
                    .into(),
            };
            Ok(format!("{}\n\nWhat reads the changed columns:\n{readers}\n\n{version}", page(out, 2, 1, offset, limit)))
        }
        "rebase" => sh
            .sql_as(&format!("rebase world {}", merge_clauses(word("branch")?, a)?), &[], g.agent.as_deref())
            .map(no_cursor),
        "check_reads" => {
            let out = sh.sql_as(&format!("check reads of world {}", lit(word("branch")?)), &[], g.agent.as_deref())?;
            Ok(match out.contains("(0 rows)") {
                true => {
                    format!("Nothing {} read changed since it forked: what it decided on still holds.", word("branch")?)
                }
                false => format!(
                    "{out}\n\nThese reads changed since the branch forked: read them again and decide again before \
                     writing on them (rebase first to see the parent as it is now). Its merge would be held for them."
                ),
            })
        }
        "merge"
            if ["by_columns", "rows", "into", "only_tables", "only_keys", "version", "target", "confirm_deletes"]
                .iter()
                .any(|k| a.get(*k).is_some()) =>
        {
            sh.sql_as(&format!("merge world {}", merge_clauses(word("branch")?, a)?), &[], g.agent.as_deref())
                .map(no_cursor)
        }
        "undo_merge" => {
            let skip = if a.get("skip_changed").and_then(Value::as_bool) == Some(true) { " skip changed" } else { "" };
            sh.sql_as(&format!("undo merge of world {}{skip}", lit(word("branch")?)), &[], g.agent.as_deref())
        }
        "checkpoint" => {
            let q = format!("checkpoint world {} as {}", lit(word("branch")?), lit(word("name")?));
            sh.sql_as(&q, &[], g.agent.as_deref())
        }
        "rollback" => {
            let q = format!("restore world {} to checkpoint {}", lit(word("branch")?), lit(word("checkpoint")?));
            sh.sql_as(&q, &[], g.agent.as_deref())
        }
        "merge" => {
            let resolve = match opt("resolve").unwrap_or("fail") {
                "fail" => "",
                r @ ("ours" | "theirs") => r,
                r => return Err(format!("resolve must be fail, ours or theirs, not '{r}'")),
            };
            let confirm = if a.get("confirm").and_then(Value::as_bool) == Some(true) { "confirm" } else { "" };
            // a conflict's hint names the shell's commands; agents have finer tools. The branch
            // goes in as the shell's own (sh.branch), not a word, which a branch named ours,
            // theirs or confirm would turn into the keyword
            word("branch")?;
            sh.exec(&format!("merge {resolve} {confirm}")).map(no_cursor).map_err(|e| {
                if !e.contains("nothing was merged") {
                    return e;
                }
                format!(
                    "{e}\nor, as tool calls: merge with resolve=\"ours\"|\"theirs\"; by_columns=true (only a column both \
                     changed conflicts); rows={{\"table/id\": \"ours\"|\"theirs\"|\"delete\"|{{row}}}} per row. \
                     merge_preview shows what a choice would do first"
                )
            })
        }
        "discard" => {
            let b = word("branch")?;
            let cascade = a.get("cascade").and_then(Value::as_bool) == Some(true);
            let forks = if cascade { sh.db.descendants(b) } else { vec![] };
            // not a shell line: `discard cascade` would read the branch `cascade` as the keyword
            let out = sh.discard(b.into(), cascade).map(no_cursor)?;
            g.owned.remove(b);
            forks.iter().for_each(|f| _ = g.owned.remove(f));
            Ok(out)
        }
        // the worlds a simulation keeps are this session's own, as its forks are
        "simulate" => {
            let mut spec = crate::api::sim_spec(a)?;
            spec.agent = g.agent.clone();
            let r = sh.db.simulate(&spec).map_err(|e| e.to_string())?;
            g.owned.extend(r.worlds.iter().filter(|w| w.kept).map(|w| w.name.clone()));
            Ok(sim_text(&r))
        }
        "replay" => {
            let r = sh.db.replay(word("world")?, opt("as"), g.agent.as_deref()).map_err(|e| e.to_string())?;
            g.owned.extend(r.replay.clone());
            let score = |s: Option<f64>| s.map_or("none".into(), |s| s.to_string());
            let kept = r.replay.as_ref().map_or(String::new(), |n| format!("; kept as {n}"));
            Ok(format!(
                "{}: {} ({} rows differ; score {} now, {} recorded{}){kept}",
                r.world,
                if r.identical { "identical" } else { "not identical" },
                r.rows_differing,
                score(r.score),
                score(r.recorded),
                r.error.map_or(String::new(), |e| format!("; failed: {e}")),
            ))
        }
        "merge_policies" => {
            let mut out = sh.sql_as("show merge policies", &[], g.agent.as_deref())?;
            let keeping: Vec<String> = sh
                .db
                .agents()
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter_map(|ag| Some(format!("agent {} keeps to {}", ag.name, ag.policy?)))
                .collect();
            out.push('\n');
            out.push_str(&match keeping.is_empty() {
                true => "no agent keeps to a merge policy".into(),
                false => keeping.join("\n"),
            });
            Ok(out)
        }
        "set_merge_policy" | "drop_merge_policy" => {
            let sql = policy_sql(&sh.db, name, word("name")?, a)?;
            if g.safe && g.agent.is_none() {
                return Err(format!(
                    "safe mode: a person sets the rules agents' merges keep to. Show them the rules and why, \
                     and ask them to run this in `chronos <database folder>` or psql:\n{sql}"
                ));
            }
            sh.sql_as(&sql, &[], g.agent.as_deref())
        }
        "check_merge_policy" => {
            let b = word("branch")?;
            let rules = a.get("rules").cloned().unwrap_or(Value::Null);
            let broken = sh.db.check_policy(b, opt("name"), &rules).map_err(|e| e.to_string())?;
            let under = match (opt("name"), rules.is_null()) {
                (Some(n), true) => format!("merge policy {n}"),
                (Some(n), false) => format!("merge policy {n} with those changes"),
                (None, _) => "those rules".into(),
            };
            Ok(match broken.is_empty() {
                true => format!("{b}'s merge would go through on its own under {under}"),
                false => format!("{b}'s merge would wait for a person under {under}: it {}", broken.join("; it ")),
            } + " (rows both sides changed are merge_preview's: a merge that doesn't settle them fails anyway)")
        }
        "reviews" => sh.sql_as("show reviews", &[], g.agent.as_deref()),
        "merge_checks" => sh.sql_as("show merge checks", &[], g.agent.as_deref()),
        "set_merge_check" | "drop_merge_check" => {
            let sql = check_sql(name, word("name")?, a)?;
            if g.safe && g.agent.is_none() {
                return Err(format!(
                    "safe mode: a person sets the checks every merge keeps to. Show them the check and why, \
                     and ask them to run this in `chronos <database folder>` or psql:\n{sql}"
                ));
            }
            sh.sql_as(&sql, &[], g.agent.as_deref())
        }
        _ => Err(format!("unknown tool '{name}'; see tools/list")),
    }
}

/// A set_merge_check or drop_merge_check call as SQL: what runs, or in safe mode what the person
/// runs.
fn check_sql(tool: &str, name: &str, a: &Value) -> Result<String, String> {
    if tool == "drop_merge_check" {
        return Ok(format!("drop merge check {};", lit(name)));
    }
    let query = a.get("query").and_then(Value::as_str).unwrap_or("").trim().trim_end_matches(';');
    if chronos::sql::parse(query).map_err(|e| e.to_string())?.len() != 1 {
        return Err("'query' must be one SELECT, e.g. \"select id from lists where not exists (...)\"".into());
    }
    let mut sql = format!("create or replace merge check {}", lit(name));
    if let Some(ts) = a.get("tables").and_then(Value::as_array) {
        // each name as written (`app.orders`: schema app, table orders), each part quoted: a quoted
        // name is exactly its text, even `select` or `123orders`
        let part = |p: &str| format!("\"{}\"", p.replace('"', "\"\""));
        let t: Vec<String> = ts
            .iter()
            .map(|t| match t.as_str() {
                Some(t) => Ok(match t.split_once('.') {
                    Some((schema, table)) => format!("{}.{}", part(schema), part(table)),
                    None => part(t),
                }),
                None => Err("'tables' must be a list of table names".to_string()),
            })
            .collect::<Result<_, _>>()?;
        if !t.is_empty() {
            sql += &format!(" on tables ({})", t.join(", "));
        }
    }
    if let Some(ms) = a.get("timeout_ms") {
        let ms = ms.as_u64().ok_or("'timeout_ms' must be a number of milliseconds")?;
        sql += &format!(" with (timeout = {ms})");
    }
    Ok(format!("{sql} as {query};"))
}

/// A set_merge_policy or drop_merge_policy call as SQL: what runs, or in safe mode what the
/// person runs.
fn policy_sql(db: &chronos::Db, tool: &str, name: &str, a: &Value) -> Result<String, String> {
    let n = lit(name);
    let names = |k: &str| -> Result<Vec<String>, String> {
        match a.get(k) {
            None | Some(Value::Null) => Ok(vec![]),
            Some(Value::Array(xs)) => xs
                .iter()
                .map(|x| x.as_str().map(String::from).ok_or_else(|| format!("'{k}' must be a list of names")))
                .collect(),
            Some(_) => Err(format!("'{k}' must be a list of names")),
        }
    };
    let mut q = vec![];
    if tool == "drop_merge_policy" {
        // releasing its agents and dropping it are one step (CASCADE): never one without the other
        let cascade = if a.get("release_agents").and_then(Value::as_bool) == Some(true) { " cascade" } else { "" };
        return Ok(format!("drop merge policy {n}{cascade};"));
    }
    let rules = match a.get("rules") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => {
            return Err("'rules' must be an object, e.g. {\"max_rows\": 500, \"review_tables\": [\"payments\"]}".into());
        }
    };
    let mut pairs = vec![];
    for (k, v) in &rules {
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "'{k}' isn't a rule (max_rows, max_deletes, confirm_deletes_over, rows_per_hour, deletes_per_hour, max_age, max_effects, effects_per_hour, review_channels, tables, review_tables, review_columns, schema, overwrite, critical, check_reads, require_scope)"
            ));
        }
        let v = match v {
            Value::String(s) => lit(s),
            Value::Array(xs) => lit(&xs
                .iter()
                .map(|x| x.as_str().ok_or_else(|| format!("{k} must be a list of table names")))
                .collect::<Result<Vec<_>, _>>()?
                .join(",")),
            Value::Object(_) => return Err(format!("{k} must be a number, true/false, null or a list of table names")),
            v => v.to_string(),
        };
        pairs.push(format!("{k} = {v}"));
    }
    let pairs = pairs.join(", ");
    match (db.policy(name).is_ok(), pairs.is_empty()) {
        (false, true) => q.push(format!("create merge policy {n}")),
        (false, false) => q.push(format!("create merge policy {n} with ({pairs})")),
        (true, false) => q.push(format!("alter merge policy {n} set ({pairs})")),
        (true, true) => {}
    }
    for ag in names("agents")? {
        q.push(format!("alter agent {} set (policy = {n})", lit(&ag)));
    }
    if q.is_empty() {
        return Err(format!("merge policy {name} exists: give rules to change, or agents to keep to it"));
    }
    Ok(q.join(";\n") + ";")
}

/// A simulation's worlds, best first: the first page of them, and how many failed.
fn sim_text(r: &chronos::Simulated) -> String {
    let kept: Vec<&str> = r.worlds.iter().filter(|w| w.kept).map(|w| w.name.as_str()).collect();
    let failed = r.worlds.iter().filter(|w| w.error.is_some()).count();
    let mut out = vec![format!(
        "{} worlds from {} in {:.0} ms, best first; {} failed; kept: {}",
        r.worlds.len(),
        r.base,
        r.total_ms,
        failed,
        if kept.is_empty() { "none".to_string() } else { kept.join(", ") }
    )];
    for w in r.worlds.iter().take(DIFF_PAGE) {
        let how = match (&w.score, &w.error) {
            (Some(s), _) => format!("score {s}"),
            (None, e) => format!("failed: {}", e.as_deref().unwrap_or("")),
        };
        out.push(format!("{} (index {}): {how}{}", w.name, w.index, if w.kept { ", kept" } else { "" }));
    }
    if r.worlds.len() > DIFF_PAGE {
        out.push(format!("({} more)", r.worlds.len() - DIFF_PAGE));
    }
    out.join("\n")
}

/// A SQL string literal.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `'branch' [into 'w'] [only tables (...)] [only keys (...)] [by columns] [using ours|theirs]
/// [resolve ('t/id' = ours, ...)]` from a merge call.
fn merge_clauses(branch: &str, a: &Value) -> Result<String, String> {
    let mut q = lit(branch);
    if let Some(into) = a.get("into").and_then(Value::as_str) {
        q.push_str(&format!(" into {}", lit(into)));
    }
    for (k, clause) in [("only_tables", "tables"), ("only_keys", "keys")] {
        let names = match a.get(k) {
            None | Some(Value::Null) => continue,
            Some(Value::Array(v)) if !v.is_empty() => v.iter().map(|x| x.as_str()).collect::<Option<Vec<_>>>(),
            Some(_) => None,
        };
        let names = names.ok_or(format!("'{k}' must be a non-empty array of strings"))?;
        // tables as quoted identifiers, keys as literals
        let quote = |n: &str| if k == "only_tables" { format!("\"{}\"", n.replace('"', "\"\"")) } else { lit(n) };
        q.push_str(&format!(" only {clause} ({})", names.iter().map(|n| quote(n)).collect::<Vec<_>>().join(", ")));
    }
    if a.get("by_columns").and_then(Value::as_bool) == Some(true) {
        q.push_str(" by columns");
    }
    match a.get("resolve").and_then(Value::as_str).unwrap_or("fail") {
        "fail" => {}
        r @ ("ours" | "theirs") => q.push_str(&format!(" using {r}")),
        r => return Err(format!("resolve must be fail, ours or theirs, not '{r}'")),
    }
    match a.get("rows") {
        None | Some(Value::Null) => {}
        Some(Value::Object(rows)) if !rows.is_empty() => {
            let picks: Vec<String> = rows
                .iter()
                .map(|(k, v)| {
                    let pick = match v {
                        Value::String(p) if ["ours", "theirs", "delete"].contains(&p.as_str()) => p.clone(),
                        Value::Object(_) => lit(&v.to_string()),
                        _ => {
                            return Err(format!(
                                "rows.{k} must be \"ours\", \"theirs\", \"delete\" or the row to keep, as an object"
                            ));
                        }
                    };
                    Ok(format!("{} = {pick}", lit(k)))
                })
                .collect::<Result<_, String>>()?;
            q.push_str(&format!(" resolve ({})", picks.join(", ")));
        }
        Some(_) => {
            return Err("'rows' must be an object of \"table/id\": \"ours\" | \"theirs\" | \"delete\" | {row}".into());
        }
    }
    match a.get("version") {
        None | Some(Value::Null) => {}
        Some(v) => q.push_str(&format!(" at version {}", v.as_u64().ok_or("'version' must be a whole number")?)),
    }
    match a.get("target") {
        None | Some(Value::Null) => {}
        Some(v) => q.push_str(&format!(" target {}", v.as_u64().ok_or("'target' must be a whole number")?)),
    }
    match a.get("confirm_deletes") {
        None | Some(Value::Null) => {}
        Some(v) => q.push_str(&format!(
            " confirm deletes {}",
            v.as_u64().ok_or("'confirm_deletes' must be the number of rows the merge deletes")?
        )),
    }
    Ok(q)
}

/// Changes an agent sees per diff call unless it asks for more.
const DIFF_PAGE: usize = 20;

/// The most rows a `find` page or `history` gives a model at once (every row is in its context from then on), the
/// furthest `offset` `find` pages to, and the widest vector search beam (`ef`): past them, the `sql` tool reads
/// with LIMIT, OFFSET and a `where`. (A `find` costs what its table holds whatever it's asked, so the HTTP API's
/// `find`, which programs page through, has no such bounds.)
const MAX_PAGE: u64 = 1000;
const MAX_OFFSET: u64 = 100_000;
const MAX_EF: u64 = 4096;

/// `a[key]` as a count: `default` when absent, refused past `max`.
fn bounded(a: &Value, key: &str, default: usize, max: u64) -> Result<usize, String> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => match v.as_u64() {
            Some(n) if n <= max => Ok(n as usize),
            Some(_) => Err(format!("'{key}' may be at most {max}: read more with the sql tool (LIMIT, OFFSET)")),
            None => Err(format!("'{key}' must be a whole number from 0 to {max}")),
        },
    }
}

/// A page of a listing: its first `head` lines (a count, or a table's header), `limit` of the
/// lines after them from `offset`, its last `tail` lines, and how to ask for the rest.
fn page(text: String, head: usize, tail: usize, offset: usize, limit: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < head + tail {
        return text;
    }
    let rows = &lines[head..lines.len() - tail];
    if offset == 0 && rows.len() <= limit {
        return text;
    }
    if offset >= rows.len() {
        return format!("{}\n(no changes past offset {offset}; there are {})", lines[..head].join("\n"), rows.len());
    }
    let from = offset;
    let to = (from + limit).min(rows.len());
    let mut out: Vec<&str> = lines[..head].to_vec();
    out.extend(&rows[from..to]);
    out.extend(&lines[lines.len() - tail..]);
    let more =
        if to < rows.len() { format!("; call diff again with offset={to} for the next ones") } else { String::new() };
    format!("{}\n(showing changes {}-{to} of {}{more})", out.join("\n"), from + 1, rows.len())
}

/// Drops the shell's "; now on <branch>" suffix: agents have no current branch.
fn no_cursor(s: String) -> String {
    s.split("; now on ").next().unwrap_or_default().to_string()
}

fn tools(hidden: &[&str]) -> Value {
    let branch = json!({ "type": "string", "description": "Branch to use (default main)" });
    let table = json!({ "type": "string", "description": "Table name, e.g. users" });
    let id = json!({ "type": "string", "description": "Row id within the table" });
    let rules = json!({
        "type": "object",
        "description": "A merge needs a person when it breaks any rule. null clears a limit.",
        "properties": {
            "max_rows": { "type": ["integer", "null"], "description": "Most rows one merge may change (null: no limit)" },
            "max_deletes": { "type": ["integer", "null"], "description": "Most rows one merge may delete; 0: any delete needs a person (the recommended start for any agent)" },
            "tables": { "type": "array", "items": { "type": "string" }, "description": "The only tables it may change (empty: any)" },
            "review_tables": { "type": "array", "items": { "type": "string" }, "description": "Tables every change to needs a person" },
            "review_columns": { "type": "array", "items": { "type": "string" }, "description": "Columns (table.column) every change to needs a person, even in one row, e.g. [\"users.email\", \"accounts.owner\"]" },
            "schema": { "type": "boolean", "description": "May it change tables themselves, views, functions, types? (default false)" },
            "overwrite": { "type": "boolean", "description": "May it overwrite rows changed in the parent since the fork? (default false)" },
            "critical": { "type": "boolean", "description": "May it change columns a reader marked critical reads (MARK READER ... CRITICAL)? (default false)" },
            "check_reads": { "type": "boolean", "description": "Keep what its worlds read, and hold a merge whose reads changed since the fork (default false)" },
            "rows_per_hour": { "type": ["integer", "null"], "description": "Most rows one agent's merges into main may change in an hour, together (null: no limit)" },
            "deletes_per_hour": { "type": ["integer", "null"], "description": "Most rows one agent's merges into main may delete in an hour, together (null: no limit)" },
            "confirm_deletes_over": { "type": ["integer", "null"], "description": "A merge deleting more rows than this waits for a person, who approves it only by giving the count (MERGE WORLD x CONFIRM DELETES n)" },
            "max_age": { "type": ["string", "integer", "null"], "description": "The oldest a branch may be and still merge on its own, e.g. \"1h\" (older ones wait for a person)" },
            "require_scope": { "type": "boolean", "description": "Its agents must say what a world may change (meta may_change or tenant) when forking it (default false)" },
            "max_effects": { "type": ["integer", "null"], "description": "Most effects (NOTIFY ON MERGE: emails, webhooks) one merge may queue (null: no limit)" },
            "effects_per_hour": { "type": ["integer", "null"], "description": "Most effects one agent's merges into main may queue in an hour, together (null: no limit)" },
            "review_channels": { "type": "array", "items": { "type": "string" }, "description": "Channels every effect queued on needs a person, e.g. [\"payments\"]" },
        },
    });
    let tool = |name: &str, desc: &str, props: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": desc,
            "inputSchema": { "type": "object", "properties": props, "required": required },
        })
    };
    let all = json!([
        tool(
            "describe",
            "Tables with row counts, their fields with types and example or allowed values, and a sample row. Call first.",
            json!({ "table": { "type": "string", "description": "One table (default all)" }, "branch": branch }),
            &[],
        ),
        tool(
            "find",
            "Search a table: exact filters, plus optional text search (BM25) and/or vector similarity, best first. Returns the match count, then up to `limit` rows with scores. With neither text nor vector, lists matching rows in key order.",
            json!({
                "table": table,
                "where": { "type": "object", "description": "Field: value pairs that must all match, e.g. {\"role\": \"admin\"}" },
                "text": { "type": "string", "description": "Words to search for in the rows' string fields" },
                "vector": { "type": "array", "items": { "type": "number" }, "description": "Query embedding; ranks by dot product with vector_field" },
                "vector_field": { "type": "string", "description": "The field holding stored embeddings, e.g. embedding" },
                "limit": { "type": "integer", "minimum": 0, "maximum": 1000, "description": "Max rows to return (default 20, at most 1000)" },
                "offset": { "type": "integer", "minimum": 0, "maximum": 100000, "description": "Skip this many results (for the next page; at most 100000)" },
                "ef": { "type": "integer", "minimum": 0, "maximum": 4096, "description": "Vector search beam on large tables: wider finds more of the true nearest, slower (default 128 or 2 x limit; at most 4096)" },
                "branch": branch,
            }),
            &["table"],
        ),
        tool(
            "sql",
            "Run SQL on a branch: SELECT (joins, GROUP BY, count/sum/avg/min/max, WHERE, ORDER BY, LIMIT), INSERT, UPDATE, DELETE, CREATE TABLE, DROP TABLE, RETURNING; branches (CREATE BRANCH, DIFF, MERGE BRANCH, SHOW BRANCHES); BEGIN ... COMMIT within one call. Tables are the same rows get/put/find see; a table without a schema has an `id` column plus its rows' fields. Use $1, $2 with params for values.",
            json!({
                "sql": { "type": "string", "description": "e.g. select name, age from users where age > $1 order by age limit 10" },
                "params": { "type": "array", "description": "Values for $1, $2, ..." },
                "branch": branch,
            }),
            &["sql"],
        ),
        tool("get", "Read one row.", json!({ "table": table, "id": id, "branch": branch }), &["table", "id"]),
        tool(
            "put",
            "Create or replace one row, on a branch you forked (writing to main needs the right to).",
            json!({
                "table": table,
                "id": id,
                "record": { "type": "object", "description": "The whole row as a JSON object" },
                "branch": branch,
            }),
            &["table", "id", "record"],
        ),
        tool("delete", "Delete one row.", json!({ "table": table, "id": id, "branch": branch }), &["table", "id"]),
        tool(
            "fork",
            "Create a branch: an instant private copy to work on without affecting anyone else.",
            json!({
                "name": { "type": "string", "description": "New branch name, e.g. your task id" },
                "from": { "type": "string", "description": "Branch to copy (default main)" },
                "meta": { "type": "object", "description": "Optional notes on the branch, e.g. {\"task\": \"...\"}. Declare what it may change, held to at its merge: {\"may_change\": \"leads.status, lists\", \"tenant\": \"org_id = 42\", \"intent\": \"close stale leads\", \"run\": \"your run or subagent id\"}" },
            }),
            &["name"],
        ),
        tool(
            "set_meta",
            "Set notes on a branch (a JSON object merged into its metadata; null removes a key).",
            json!({ "branch": branch, "meta": { "type": "object" } }),
            &["branch", "meta"],
        ),
        tool("branches", "List branches with their parent and number of changes.", json!({}), &[]),
        tool(
            "diff",
            "What changed on a branch since it was forked, row by row and field by field: the count first, \
             then up to `limit` changes (default 20; page with offset, or with `after` for big changes; `count` \
             for just the size). With `to`: from `branch` to `to` \
             instead; either can be a branch as it was, `name@when` (e.g. main@-1 hour). With as_sql: the \
             change as SQL statements (INSERT, UPDATE, DELETE, CREATE/ALTER TABLE) that make it. With readers: \
             what in the database reads the columns it changes, so you see what the change could break.",
            json!({
                "branch": branch,
                "to": { "type": "string", "description": "Optional: compare against this branch, or name@when" },
                "as_sql": { "type": "boolean", "description": "Show the change as SQL statements instead of rows" },
                "limit": { "type": "integer", "description": "Changes to show (default 20)" },
                "offset": { "type": "integer", "description": "Skip this many changes (for the next page)" },
                "count": { "type": "boolean", "description": "Only count the changes, per table (cheap for big changes)" },
                "readers": { "type": "boolean", "description": "Instead of rows: what in the database reads each changed column (views, merge checks, triggers, functions, keys, indexes), and what that couldn't see" },
                "after": { "type": "string", "description": "Page by cursor instead of offset: \"\" for the first page, then the key the last page gave" },
            }),
            &["branch"],
        ),
        tool(
            "history",
            "What happened to a branch recently, newest first: forks, writes, commits, merges.",
            json!({ "branch": branch, "limit": { "type": "integer", "minimum": 0, "maximum": 1000, "description": "Events to show (default 30, at most 1000)" } }),
            &["branch"],
        ),
        tool(
            "restore",
            "Put a branch back as it was at a moment (e.g. \"-10 minutes\" or \"2026-09-20 10:00\"), as an ordinary \
             write. Any read also takes a branch as it was: name@when.",
            json!({ "branch": branch, "at": { "type": "string", "description": "A time, or a time ago like -10 minutes" } }),
            &["branch", "at"],
        ),
        tool(
            "merge",
            "Apply a branch's changes to its parent and delete the branch. Rows both sides changed come back as conflicts and nothing is merged; check with merge_preview, then call again with resolve, by_columns or per-row decisions in rows. With only_tables or only_keys just those rows merge, and with into they go to another branch; either way the branch stays.",
            json!({
                "branch": branch,
                "into": { "type": "string", "description": "Merge into this branch instead of the parent; the branch stays" },
                "only_tables": { "type": "array", "items": { "type": "string" }, "description": "Merge only these tables; the branch stays with the rest" },
                "only_keys": { "type": "array", "items": { "type": "string" }, "description": "Merge only these rows (\"table/id\"); the branch stays with the rest" },
                "by_columns": { "type": "boolean", "description": "Rows both sides changed merge column by column; only a column both changed conflicts" },
                "rows": { "type": "object", "description": "Per-row decisions: {\"table/id\": \"ours\" | \"theirs\" | \"delete\" | {the row to keep}}" },
                "resolve": { "type": "string", "enum": ["fail", "ours", "theirs"], "description": "On conflict: fail (default), ours (keep this branch's values), theirs (keep the parent's)" },
                "confirm": { "type": "boolean", "description": "After a crash, merge the branch as it is now; check its diff first, since some of its writes may be lost" },
                "version": { "type": "integer", "description": "Merge only if the branch is still at this version (the one merge_preview or the review showed): what was reviewed is what merges" },
                "target": { "type": "integer", "description": "With rows settled (resolve, by_columns, rows): the target merge_preview named, so they settle against the parent as the preview saw it (refused if it changed)" },
                "confirm_deletes": { "type": "integer", "description": "For a person approving a merge whose merge policy has confirm_deletes_over: the number of rows it deletes, as merge_preview counts them (a wrong count is refused with the real one; an agent's own merge goes to review instead)" },
            }),
            &["branch"],
        ),
        tool(
            "rebase",
            "Move a branch's fork point up to its parent as it is now, keeping the branch's own changes on top: \
             pick up what changed under a long-running branch, and settle conflicts now rather than all at its \
             merge. The parent doesn't change. Refused, with nothing changed, on a conflict nothing settles, or \
             when a row the branch read (with check_reads) changed since: redo that work in a new branch.",
            json!({
                "branch": branch,
                "by_columns": { "type": "boolean", "description": "Rows both sides changed combine column by column; only a column both changed conflicts" },
                "rows": { "type": "object", "description": "Per-row decisions: {\"table/id\": \"ours\" | \"theirs\" | \"delete\" | {the row to keep}}" },
                "resolve": { "type": "string", "enum": ["fail", "ours", "theirs"], "description": "On conflict: fail (default), ours (keep this branch's values), theirs (take the parent's)" },
                "version": { "type": "integer", "description": "Rebase only if the branch is still at this version" },
            }),
            &["branch"],
        ),
        tool(
            "check_reads",
            "Whether anything a branch read changed since it forked (a branch forked with check_reads), without \
             merging or changing anything: the rows, key ranges and filtered scans it read (an empty result \
             included) compared with its parent as it is now, and in a folder attached to Postgres with Postgres \
             as it is now too. Run it between steps of a long task: if something changed, read it again and \
             decide again (rebase first) before writing; its merge would be held for it anyway. Nothing changes: \
             the branch's view stays as it was.",
            json!({ "branch": branch }),
            &["branch"],
        ),
        tool(
            "merge_preview",
            "What merging a branch would do, without doing it: per row, whether it applies cleanly or \
             conflicts and why, with the parent's, the branch's and the fork point's versions, then what in \
             the database reads the columns the branch changes. In a folder attached to Postgres, a merge into \
             main is also checked against Postgres as it is now (read only, rolled back, nothing written): each \
             row it would write shows Postgres's current value, and rows changed there since the copy are marked. \
             Takes the same by_columns, resolve, rows, into, only_tables and only_keys as merge, so you can try a \
             resolution first.",
            json!({
                "branch": branch,
                "into": { "type": "string", "description": "Merge into this branch instead of the parent" },
                "only_tables": { "type": "array", "items": { "type": "string" }, "description": "Merge only these tables" },
                "only_keys": { "type": "array", "items": { "type": "string" }, "description": "Merge only these rows (\"table/id\")" },
                "by_columns": { "type": "boolean", "description": "Rows both sides changed merge column by column; only a column both changed conflicts" },
                "resolve": { "type": "string", "enum": ["fail", "ours", "theirs"], "description": "What conflicts become" },
                "rows": { "type": "object", "description": "Per-row decisions: {\"table/id\": \"ours\" | \"theirs\" | \"delete\" | {the row to keep}}" },
                "limit": { "type": "integer", "description": "Rows to show (default 20)" },
                "offset": { "type": "integer", "description": "Skip this many rows (for the next page)" },
                "version": { "type": "integer", "description": "With a later page: the version the first page named. Refused if the branch changed since" },
            }),
            &["branch"],
        ),
        tool(
            "undo_merge",
            "Undo a branch's latest merge: put back every row it changed in the parent. Refuses if some \
             were changed again since, naming them; skip_changed leaves those as they are.",
            json!({
                "branch": { "type": "string", "description": "The branch that was merged (it's gone; its name still works)" },
                "skip_changed": { "type": "boolean", "description": "Leave rows changed since the merge alone" },
            }),
            &["branch"],
        ),
        tool(
            "checkpoint",
            "Name this moment of a branch, to roll back to it later with rollback.",
            json!({ "branch": branch, "name": { "type": "string", "description": "e.g. before-migration" } }),
            &["branch", "name"],
        ),
        tool(
            "rollback",
            "Put a branch back as it was at one of its checkpoints (an ordinary write: its history keeps the rest).",
            json!({ "branch": branch, "checkpoint": { "type": "string", "description": "A name given to checkpoint" } }),
            &["branch", "checkpoint"],
        ),
        tool(
            "discard",
            "Throw a branch and its changes away. With cascade, the branches forked from it too.",
            json!({
                "branch": branch,
                "cascade": { "type": "boolean", "description": "Also discard every branch forked from it (and from those)" },
            }),
            &["branch"],
        ),
        tool(
            "simulate",
            "Try many variations at once: fork `worlds` branches from one moment of `from`, run the same SQL \
             script in each in parallel ($1 is the world's index 0, 1, ..., $2 its seed), score each with a \
             query, keep the best `keep` and discard the rest. random() is seeded per world, so the same call \
             gives the same worlds. Kept branches are yours; each records its inputs for replay.",
            json!({
                "worlds": { "type": "integer", "description": "How many worlds, e.g. 100" },
                "prefix": { "type": "string", "description": "World i is named <prefix>_<i>" },
                "script": { "type": "string", "description": "SQL run in each world, e.g. update prices set p = p * (0.9 + random() * 0.2)" },
                "score": { "type": "string", "description": "A SELECT giving one number per world, e.g. select sum(p * sold) from prices" },
                "from": { "type": "string", "description": "Branch to fork (default main), or a moment of one: name@when" },
                "keep": { "type": ["integer", "string"], "description": "Worlds to keep, best first (default all), or \"all\"" },
                "order": { "type": "string", "enum": ["desc", "asc"], "description": "desc: highest score best (default); asc: lowest" },
                "seed": { "type": "integer", "description": "Seeds random() (default 0)" },
                "threads": { "type": "integer", "description": "Threads to use (default all free cores)" },
            }),
            &["worlds", "prefix", "script", "score"],
        ),
        tool(
            "replay",
            "Make a simulated branch again from its recorded inputs (its base as it was then, the same script, \
             index and seed) and say whether it comes out identical: rows and score. With `as`, keep the \
             replay as a new branch.",
            json!({
                "world": { "type": "string", "description": "A branch a simulation kept" },
                "as": { "type": "string", "description": "Optional: keep the replay under this name" },
            }),
            &["world"],
        ),
        tool(
            "merge_policies",
            "Merge policies: the rules under which an agent's merge goes through without a person (a merge \
             breaking any waits for one), and which agents keep to each.",
            json!({}),
            &[],
        ),
        tool(
            "check_merge_policy",
            "Try rules on a branch's real changes before an agent keeps to them: would its merge go through on \
             its own, or wait for a person, and why. Give a policy's name, rules, or both (the rules change \
             the named policy's, for this check only).",
            json!({ "branch": branch, "name": { "type": "string", "description": "A merge policy" }, "rules": rules.clone() }),
            &["branch"],
        ),
        tool(
            "set_merge_policy",
            "Create a merge policy, or change the rules given of an existing one (the rest stay), and have agents \
             keep to it. Needs admin; in safe mode it answers with the SQL for a person to run.",
            json!({
                "name": { "type": "string", "description": "Policy name: letters, digits, _ and -" },
                "rules": rules,
                "agents": { "type": "array", "items": { "type": "string" }, "description": "Agents to keep to it from now on" },
            }),
            &["name"],
        ),
        tool(
            "drop_merge_policy",
            "Remove a merge policy (refused while an agent keeps to it, unless release_agents).",
            json!({
                "name": { "type": "string" },
                "release_agents": { "type": "boolean", "description": "First take it from the agents keeping to it: their rights alone decide then" },
            }),
            &["name"],
        ),
        tool(
            "merge_checks",
            "Merge checks: SQL queries every merge's result must find nothing in (a merge whose result they \
             find rows in is refused, or for an agent keeping to a merge policy waits for a person), the \
             tables whose changes each runs for, and its timeout.",
            json!({}),
            &[],
        ),
        tool(
            "set_merge_check",
            "Create a merge check, or replace the one of that name: a SELECT that must find no rows in what a \
             merge would make (e.g. lead lists left with no leads). Needs admin; in safe mode it answers with \
             the SQL for a person to run.",
            json!({
                "name": { "type": "string", "description": "Check name: letters, digits, _ and -" },
                "query": { "type": "string", "description": "One SELECT; the rows it finds are what's wrong" },
                "tables": { "type": "array", "items": { "type": "string" }, "description": "Optional: run it only for merges changing these tables (default: every merge)" },
                "timeout_ms": { "type": "integer", "description": "Optional: how long it may run (default 1000); a merge it doesn't finish in is refused" },
            }),
            &["name", "query"],
        ),
        tool(
            "drop_merge_check",
            "Remove a merge check. Needs admin; in safe mode it answers with the SQL for a person to run.",
            json!({ "name": { "type": "string" } }),
            &["name"],
        ),
        tool(
            "reviews",
            "Merges waiting for a person: the branch, its agent, the policy and the rules its merge broke, and \
             whether it changed since the agent asked. A person approves by merging the branch, or discards it.",
            json!({}),
            &[],
        ),
    ]);
    let Value::Array(all) = all else { unreachable!() };
    all.into_iter().filter(|t| !hidden.contains(&t["name"].as_str().unwrap_or(""))).collect()
}

#[cfg(test)]
mod tests {
    use super::page;

    #[test]
    fn diff_pages() {
        let rows: Vec<String> = (1..=30).map(|i| format!("~ t/{i}")).collect();
        let text = format!("30 changes on w\n{}", rows.join("\n"));
        let p = page(text.clone(), 1, 0, 0, 20);
        assert!(
            p.starts_with("30 changes on w\n~ t/1\n")
                && p.contains("~ t/20\n(showing changes 1-20 of 30; call diff again with offset=20"),
            "{p}"
        );
        assert!(!p.contains("~ t/21"));
        let p = page(text.clone(), 1, 0, 20, 20);
        assert!(p.contains("~ t/21\n") && p.ends_with("~ t/30\n(showing changes 21-30 of 30)"), "{p}");
        assert_eq!(
            page("3 changes on w\na\nb\nc".into(), 1, 0, 0, 20),
            "3 changes on w\na\nb\nc",
            "small diffs unchanged"
        );
        assert_eq!(page("no changes on w".into(), 1, 0, 0, 20), "no changes on w");
        assert!(page(text, 1, 0, 99, 20).contains("no changes past offset 99; there are 30"));
        // a table keeps its header and "(n rows)" footer
        let t = format!(" key | op\n-----+----\n{}\n(30 rows)", rows.join("\n"));
        let p = page(t, 2, 1, 0, 5);
        assert!(
            p.starts_with(" key | op\n-----+----\n~ t/1\n")
                && p.contains("~ t/5\n(30 rows)\n(showing changes 1-5 of 30;"),
            "{p}"
        );
    }
}
