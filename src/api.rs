//! `chronos serve <folder>`: the database over HTTP, for any language (TypeScript and Python clients
//! in `clients/`). Every operation is `POST /v1/<op>` with a JSON body and a JSON reply; rows go in
//! and come out as JSON objects. `GET /v1/health` answers without auth; `GET /metrics` gives
//! Prometheus the database's metrics (see `chronos::metrics`), with the server's token if it has one.
//!
//! Errors are `{"error": "...how to fix it..."}` with a status: 400 bad input, 404 no such branch
//! or row, 409 conflicts (merge conflicts come back as data), 500 storage.
//!
//! Listens on 127.0.0.1:7070 by default. Any other address needs a token (`--token` or
//! CHRONOS_TOKEN), sent as `Authorization: Bearer <token>`. With `--safe`, a client without an
//! agent's token acts as the guest agent (see `chronos::GUEST`) unless it sends the admin token.
//! HTTPS with `--tls-cert` and `--tls-key` (see `tls`). Web pages are refused (see `admit`).
//! ponytail: one shared database, no per-client limits.

use crate::http;
use chronos::{Db, Error, Query, Resolve, sql};
use rustls::ServerConfig;
use serde_json::{Value, json};
use std::net::TcpListener;
use std::sync::Arc;

const MAX_BODY: usize = 32 << 20;
/// The largest `POST /mcp` body: a tool call's arguments, never a bulk load (that's `/v1/batch` or SQL's COPY).
const MAX_MCP_BODY: usize = 8 << 20;

/// An error reply: status, and the `error` payload (a message, or an object with details).
pub struct Fail(pub u16, pub Value);

fn bad(msg: impl Into<String>) -> Fail {
    Fail(400, Value::String(msg.into()))
}

/// What reads the columns a change changes (see `chronos::sql::impact`), as JSON.
fn impact_json(i: chronos::sql::Impact) -> Value {
    let readers: Vec<Value> = i
        .readers
        .into_iter()
        .map(
            |r| json!({ "table": r.table, "columns": r.columns, "kind": r.kind, "reader": r.name, "detail": r.detail, "critical": r.critical }),
        )
        .collect();
    json!({ "readers": readers, "coverage": i.coverage })
}

fn fail(status: u16, msg: impl Into<String>) -> Fail {
    Fail(status, Value::String(msg.into()))
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        let status = match &e {
            Error::NoBranch(_) => 404,
            Error::Constraint { code: "42501", .. } => 403, // an agent may not
            Error::Constraint { code: "53400" | "53300", .. } => 429, // too fast, too many worlds or statements at once
            Error::Constraint { code: "53200", .. } => 413, // a query past its agent's memory limit
            Error::Constraint { code, .. } if !code.starts_with("23") => 400, // a row that doesn't fit its table
            Error::Exists(_)
            | Error::HasChildren(_)
            | Error::Conflicts(_)
            | Error::Stale { .. }
            | Error::Constraint { .. } => 409,
            Error::IsMain => 400,
            Error::Io(_) => 500,
        };
        fail(status, crate::msg(e))
    }
}

pub fn serve(db: &Arc<Db>, listen: &str, auth: &crate::Auth, tls: Option<Arc<ServerConfig>>) -> std::io::Result<()> {
    let l = TcpListener::bind(listen).map_err(|e| std::io::Error::other(format!("listen on {listen}: {e}")))?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    // the address bound, so `--listen 127.0.0.1:0` says which port the kernel picked
    let addr = l.local_addr()?;
    eprintln!("chronos: serving on {scheme}://{addr} (POST /v1/<op>; POST /mcp; GET /v1/health)");
    let reply = |r: Result<Value, Fail>| {
        let (status, body, content_type) = match r {
            Ok(Value::String(text)) => (200, text, "text/plain; version=0.0.4"), // /metrics
            Ok(v) => (200, v.to_string(), "application/json"),
            Err(Fail(status, error)) => (status, json!({ "error": error }).to_string(), "application/json"),
        };
        http::Response { status, content_type, body: body.into_bytes(), headers: String::new() }
    };
    // on every response: no content sniffing, nothing cached (replies carry rows), and over TLS, HTTPS only
    let headers = format!(
        "X-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\n{}",
        if tls.is_some() { "Strict-Transport-Security: max-age=31536000\r\n" } else { "" }
    );
    http::serve(&l, tls, MAX_BODY, &headers, &|req| admit(db, auth, req).map_err(|e| reply(Err(e))), &|req, who| {
        if req.path == "/mcp" {
            return mcp(db, auth, req, who);
        }
        reply(handle(db, auth, req, who))
    });
    Ok(())
}

/// Who a request acts as, from its headers alone (its body isn't read yet): an agent's name, or
/// None for the database's own user; and whether it sent the admin token.
type Who = (Option<String>, bool);

fn admit(db: &Db, auth: &crate::Auth, req: &http::Request) -> Result<Who, Fail> {
    let token = auth.token.as_deref();
    // web pages: a browser sends Origin on a cross-site request, so a page can't use the API
    // through its reader's browser (CSRF); a JSON body can't be sent without a CORS preflight,
    // which is never approved; and without a server token, a Host name that isn't loopback is a
    // page's own name pointed at 127.0.0.1 (DNS rebinding)
    if req.header("Origin").is_some() {
        return Err(fail(
            403,
            "requests from web pages (with an Origin header) are refused: call the API from a program",
        ));
    }
    if token.is_none() && req.header("Host").is_some_and(|h| !loopback_host(h)) {
        return Err(fail(403, "without a server token, only localhost, 127.0.0.1 or [::1] may be the Host"));
    }
    let json_body = req
        .header("Content-Type")
        .is_some_and(|t| t.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json"));
    if req.header("Content-Length").is_some_and(|n| n != "0") && !json_body {
        return Err(fail(415, "send the body with Content-Type: application/json"));
    }
    if req.path == "/v1/health" {
        return Ok((None, false));
    }
    // (from the header, before the body is read)
    if req.path == "/mcp"
        && req.header("Content-Length").and_then(|n| n.trim().parse::<usize>().ok()).is_some_and(|n| n > MAX_MCP_BODY)
    {
        return Err(fail(413, format!("an MCP message may be at most {} MB", MAX_MCP_BODY >> 20)));
    }
    // the server's token (or none, listening only locally) acts as the database's own user; an
    // agent's token acts as that agent, with what it may do checked (see `chronos::agents`)
    let bearer = req.header("Authorization").and_then(|v| v.strip_prefix("Bearer "));
    // an address that sent too many wrong tokens waits before its next try (see `backoff`)
    if bearer.is_some()
        && let Some(left) = crate::backoff::wait(req.peer)
    {
        let secs = left.as_secs_f64().ceil() as u64;
        return Err(fail(429, format!("too many wrong tokens from this address: try again in {secs} s")));
    }
    let admin = bearer.is_some_and(|b| auth.admin.as_deref().is_some_and(|a| crate::same_secret(b, a)));
    let agent = match (bearer, token) {
        _ if admin => None,
        (Some(b), Some(t)) if crate::same_secret(b, t) => None,
        // a token that matches nobody (a typo, or a disabled or dropped agent's) is refused even
        // without a server token: taking it as the owner would give a stopped agent more rights
        (Some(b), _) => match db.agent_by_token(b) {
            Ok(a) => {
                // an agent made before agents had SCRAM verifiers: its token, given here, makes it one, so it can
                // log in over the Postgres port (which asks it for SCRAM, as any name: see `pg`)
                if a.verifier().is_none() {
                    let _ = db.upgrade_agent(&a.name, b);
                }
                Some(a.name)
            }
            Err(_) => {
                crate::backoff::failed(req.peer);
                return Err(fail(401, "wrong token: send Authorization: Bearer <server or agent token>"));
            }
        },
        (None, Some(_)) => return Err(fail(401, "missing token: send Authorization: Bearer <token>")),
        (None, None) => None,
    };
    Ok((agent, admin))
}

/// A Host header naming this machine: localhost, 127.x.x.x or [::1], with or without a port.
fn loopback_host(h: &str) -> bool {
    let host = match h.strip_prefix('[') {
        Some(v6) => {
            return v6
                .split(']')
                .next()
                .and_then(|a| a.parse::<std::net::Ipv6Addr>().ok())
                .is_some_and(|a| a.is_loopback());
        }
        None => h.split_once(':').map_or(h, |(host, _)| host),
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::Ipv4Addr>().is_ok_and(|a| a.is_loopback())
}

fn handle(db: &Db, auth: &crate::Auth, req: &http::Request, (agent, admin): Who) -> Result<Value, Fail> {
    let path = req.path.as_str();
    if path == "/v1/health" {
        return Ok(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }));
    }
    // safe mode: only the admin token (or an agent's) gets past the guest's rights
    let agent = if auth.safe && agent.is_none() && !admin { Some(chronos::GUEST.to_string()) } else { agent };
    if path == "/metrics" {
        // the guest may read them: its `read` right already gives it SHOW METRICS
        if agent.as_deref().is_some_and(|a| a != chronos::GUEST) {
            return Err(fail(403, "metrics are for the server's token, not an agent's"));
        }
        return Ok(Value::String(db.metrics().prometheus(&db.counters())));
    }
    if req.method != "POST" {
        return Err(fail(405, format!("use POST {path} with a JSON body")));
    }
    let op = path.strip_prefix("/v1/").ok_or_else(|| fail(404, format!("no such endpoint {path}; use /v1/<op>")))?;
    let body = std::str::from_utf8(&req.body).map_err(|_| bad("the body must be UTF-8 JSON"))?;
    let args: Value = if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(body).map_err(|e| bad(format!("body is not JSON: {e}")))?
    };
    match agent {
        Some(name) => call_as(db, &db.as_agent(&name)?, op, &args),
        None => call(db, op, &args),
    }
}

/// `POST /mcp`: the MCP tools over HTTP (see `crate::mcp::http`), only ever as an agent: the one whose token came
/// with the request, or the guest under `--safe`. The server's token is refused: an MCP client acting as the
/// database's own user is `chronos mcp <folder>` on its machine, where the OS says who started it.
fn mcp(db: &Arc<Db>, auth: &crate::Auth, req: &http::Request, (agent, admin): Who) -> http::Response {
    let json = |status, v: Value| http::Response {
        status,
        content_type: "application/json",
        body: v.to_string().into_bytes(),
        headers: String::new(),
    };
    let agent = if auth.safe && agent.is_none() && !admin { Some(chronos::GUEST.to_string()) } else { agent };
    if req.method != "POST" {
        return json(
            405,
            json!({ "error": "use POST /mcp with one JSON-RPC message as the body: replies come in the response, with no event stream" }),
        );
    }
    let Some(agent) = agent else {
        return json(
            403,
            json!({ "error": "MCP over HTTP acts as an agent: send an agent's token (CREATE AGENT makes one, and shows it once), \
                              or run chronos mcp <folder> on the database's machine" }),
        );
    };
    match crate::mcp::http(db, &agent, &req.body) {
        (status, Some(v)) => json(status, v),
        (status, None) => {
            http::Response { status, content_type: "application/json", body: vec![], headers: String::new() }
        }
    }
}

/// An agent's request: checked, then run as the database's own would be (logged as the agent's).
pub fn call_as(db: &Db, me: &chronos::Acting, op: &str, a: &Value) -> Result<Value, Fail> {
    // (`sql`, `simulate` and `replay` don't go through `call`: the same guard, first)
    private_worlds(db, a)?;
    let s = |k: &str| a.get(k).and_then(Value::as_str);
    let branch = s("branch").unwrap_or(chronos::MAIN);
    let _slot = if op == "sql" { None } else { db.slot(&me.agent)? }; // SQL takes one per statement
    match op {
        "get" | "find" | "world" | "branches" | "worlds" | "history" | "diff" => me.may_read(branch)?,
        "put" | "delete" => me.may_write(branch, 1)?,
        "batch" => me.may_write(branch, a.get("rows").and_then(Value::as_array).map_or(0, Vec::len))?,
        "fork" => {
            let from = s("from").unwrap_or(chronos::MAIN);
            let name = s("name").filter(|n| !n.is_empty()).ok_or_else(|| bad("'name' is required (a string)"))?;
            let w = me.fork(from, name, a.get("meta").cloned().unwrap_or(Value::Null))?;
            return Ok(json!({ "ok": true, "world": world(&w) }));
        }
        "merge" if a.get("dry_run").and_then(Value::as_bool) == Some(true) => me.may_read(branch)?,
        "merge" => match s("into") {
            Some(into) => me.may_merge_into(branch, into)?,
            None => me.may_merge(branch)?,
        },
        "discard" if a.get("cascade").and_then(Value::as_bool) == Some(true) => me.may_manage_tree(branch)?,
        "discard" | "set_meta" | "checkpoint" => me.may_manage(branch)?,
        "restore" => me.may_restore(branch)?,
        "undo_merge" => me.may_undo()?,
        "sql" => return run_sql(db, a, Some(&me.agent.name)),
        "simulate" | "replay" => return simulate(db, op, a, Some(&me.agent.name)), // checked as it runs
        // every agent's results, as /metrics are: the server's token only
        "merge_outcomes" => return Err(fail(403, "merge outcomes are for the server's token, not an agent's")),
        _ => me.may_admin()?,
    }
    row_security(db, &me.agent.name, op, a)?;
    call(db, op, a)
}

/// Row-level security for an agent's call of the row API, which reads and writes rows as they are:
/// 403 (42501) when a policy limits the agent's role (a role of its name) on a table whose rows `op`
/// reads or writes (get, find, put, delete, batch; diff and merge: the tables the world changed).
/// SQL applies the policies instead.
pub fn row_security(db: &Db, agent: &str, op: &str, a: &Value) -> Result<(), Fail> {
    if !sql::row_security_possible(db) {
        return Ok(());
    }
    let s = |k: &str| a.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    let branch = s("branch").unwrap_or(chronos::MAIN);
    let (tables, worlds): (Vec<String>, Vec<&str>) = match op {
        "get" | "put" | "delete" | "find" => (s("table").map(String::from).into_iter().collect(), vec![branch]),
        "batch" => {
            let rows = a.get("rows").and_then(Value::as_array).into_iter().flatten();
            (rows.filter_map(|r| r.get("table").and_then(Value::as_str)).map(String::from).collect(), vec![branch])
        }
        "diff" => (
            db.diff_count(branch, s("to"))?.into_keys().collect(),
            [Some(branch), s("to")].into_iter().flatten().collect(),
        ),
        "merge" => (
            db.diff_count(branch, None)?.into_keys().collect(),
            [Some(branch), s("into")].into_iter().flatten().collect(),
        ),
        _ => return Ok(()),
    };
    let names: Vec<&str> = tables.iter().map(String::as_str).collect();
    for w in worlds {
        match sql::row_security_refuses(db, agent, w, &names) {
            Ok(None) => {}
            Ok(Some(m)) => return Err(Fail(403, json!({ "message": m, "code": "42501" }))),
            Err(e) => return Err(Fail(400, json!({ "message": e.message, "code": e.code }))),
        }
    }
    Ok(())
}

/// The `sql` op: one result per statement. With `agent`, it runs as that agent.
fn run_sql(db: &Db, a: &Value, agent: Option<&str>) -> Result<Value, Fail> {
    let branch = a.get("branch").and_then(Value::as_str).unwrap_or(chronos::MAIN);
    // an agent's statements are checked one by one, as over the Postgres port
    let mut session = crate::Closing(sql::Session::new(branch), db); // closed however this ends
    session.0.agent = agent.map(String::from);
    session.0.client = Some(agent.unwrap_or("http").to_string()); // its reads are recorded (see `chronos::Read`)
    sql_in(db, a, session)
}

/// The `sql` op as role `role` (a Studio signed in with it): its statements run with the role's
/// privileges, as over the Postgres port (GRANTs, row-level security, what SET ROLE may become),
/// and it needs CONNECT on the world, as a connection to it does.
pub fn run_sql_as_role(db: &Db, a: &Value, role: &str) -> Result<Value, Fail> {
    private_worlds(db, a)?;
    let branch = a.get("branch").and_then(Value::as_str).unwrap_or(chronos::MAIN);
    let mut session = crate::Closing(sql::Session::new(branch), db);
    session.0.user = Some(role.to_string()); // current_user, session_user
    session.0.client = Some(role.to_string());
    session.0.log_in(db, role).and_then(|()| session.0.may_connect(db)).map_err(sql_fail)?;
    sql_in(db, a, session)
}

/// A SQL error as the `sql` op replies with it.
fn sql_fail(e: sql::SqlError) -> Fail {
    let status = match e.code {
        "42501" => 403,
        "53400" | "53300" => 429,
        "53200" => 413,
        c if c.starts_with("23") => 409,
        "3D000" | "42P01" => 404,
        c if c.starts_with("58") || c.starts_with("XX") => 500,
        _ => 400,
    };
    let mut body = json!({ "message": e.message, "code": e.code });
    // what a constraint violation names, as Postgres's ErrorResponse fields (`err.constraint` in node-postgres)
    if let Some(i) = &e.info {
        for (k, v) in [
            ("detail", &i.detail),
            ("hint", &i.hint),
            ("schema", &i.schema),
            ("table", &i.table),
            ("column", &i.column),
            ("constraint", &i.constraint),
        ] {
            if let Some(v) = v {
                body[k] = json!(v);
            }
        }
    }
    Fail(status, body)
}

/// Runs the `sql` op's statements in `session`, closed once they have run.
fn sql_in(db: &Db, a: &Value, mut session: crate::Closing<'_>) -> Result<Value, Fail> {
    let s = |k: &str| a.get(k).and_then(Value::as_str);
    let need = |k: &str| s(k).filter(|v| !v.is_empty()).ok_or_else(|| bad(format!("'{k}' is required (a string)")));
    let branch = s("branch").unwrap_or(chronos::MAIN);
    // {"sql": "select ... where id = $1", "params": [5]} -> one result per statement
    let params = match a.get("params") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(ps)) => ps.iter().map(sql::Value::from_json).collect(),
        Some(_) => return Err(bad("'params' must be an array of values for $1, $2, ...")),
    };
    let outs = session.0.run(db, need("sql")?, &params);
    drop(session);
    let outs = outs.map_err(sql_fail)?;
    // writes return the branch version, like put (clients pass it to merge)
    let wrote = outs.iter().any(|o| !o.tag.starts_with("SELECT") && o.tag != "SET");
    let version = if wrote { Some(db.version(branch)?) } else { None };
    let results: Vec<Value> = outs
        .into_iter()
        .map(|o| {
            let cols: Vec<&(String, sql::Type)> = o.columns.iter().flatten().collect();
            let rows: Vec<Value> = o
                .rows
                .iter()
                .map(|r| Value::Object(cols.iter().zip(r).map(|((n, t), v)| (n.clone(), cell(v, *t))).collect()))
                .collect();
            let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
            let types: Vec<String> = cols.iter().map(|(_, t)| t.pg_name().into_owned()).collect();
            json!({ "command": o.tag, "columns": names, "types": types, "rows": rows })
        })
        .collect();
    Ok(json!({ "results": results, "version": version }))
}

/// A SQL result's value as the `sql` op sends it: a numeric as its text, which keeps its scale (`15.00`) and every
/// digit (JSON readers take numbers as doubles); a real with float4's own digits (see `sql::Value::to_json_as`).
fn cell(v: &sql::Value, ty: sql::Type) -> Value {
    match v {
        sql::Value::Numeric(_) => Value::String(v.to_text()),
        v => v.to_json_as(ty),
    }
}

/// A simulation from a request: `from` (default main), `worlds`, `prefix`, `script`, `score`,
/// and optionally `keep` (a number, or "all"), `order` ("desc" or "asc"), `seed` and `threads`.
pub fn sim_spec(a: &Value) -> Result<chronos::Simulate, String> {
    let s = |k: &str| a.get(k).and_then(Value::as_str);
    let need = |k: &str, what: &str| s(k).filter(|v| !v.is_empty()).ok_or_else(|| format!("'{k}' is required: {what}"));
    let worlds = a.get("worlds").and_then(Value::as_u64).filter(|n| *n > 0);
    let worlds = worlds.ok_or("'worlds' is required: how many worlds to run")?;
    let mut spec = chronos::Simulate::new(
        s("from").unwrap_or(chronos::MAIN),
        worlds as usize,
        need("prefix", "world i is named <prefix>_<i>")?,
        need("script", "the SQL to run in each world ($1: its index, $2: its seed)")?,
        need("score", "a SELECT giving each world's score")?,
    );
    spec.ascending = match s("order") {
        None | Some("desc") => false,
        Some("asc") => true,
        Some(o) => return Err(format!("'order' must be desc (highest score best) or asc, not '{o}'")),
    };
    spec.keep = match a.get("keep") {
        None | Some(Value::Null) => None,
        Some(Value::String(k)) if k == "all" => None,
        Some(k) => Some(k.as_u64().ok_or("'keep' must be a number of worlds, or \"all\"")? as usize),
    };
    spec.seed = match a.get("seed") {
        None | Some(Value::Null) => 0,
        Some(v) => v.as_i64().ok_or("'seed' must be an integer")?,
    };
    spec.threads = a.get("threads").and_then(Value::as_u64).unwrap_or(0) as usize;
    Ok(spec)
}

/// The `simulate` and `replay` ops. With `agent`, they run as that agent: its worlds, its quotas.
fn simulate(db: &Db, op: &str, a: &Value, agent: Option<&str>) -> Result<Value, Fail> {
    if op == "replay" {
        let world = a.get("world").and_then(Value::as_str).ok_or_else(|| bad("'world' is required (a string)"))?;
        let r = db.replay(world, a.get("as").and_then(Value::as_str), agent)?;
        return Ok(json!({
            "world": r.world, "replay": r.replay, "identical": r.identical, "rows_differing": r.rows_differing,
            "score": r.score, "recorded_score": r.recorded, "error": r.error,
        }));
    }
    let mut spec = sim_spec(a).map_err(bad)?;
    spec.agent = agent.map(String::from);
    let r = db.simulate(&spec)?;
    let worlds: Vec<Value> = r
        .worlds
        .iter()
        .map(|w| {
            json!({
                "world": w.name, "id": w.id, "index": w.index, "seed": w.seed, "score": w.score, "error": w.error,
                "kept": w.kept,
            })
        })
        .collect();
    Ok(json!({
        "worlds": worlds, "base": r.base, "at": r.at,
        "timings": { "fork_ms": r.fork_ms, "run_ms": r.run_ms, "discard_ms": r.discard_ms, "total_ms": r.total_ms },
    }))
}

/// A world as JSON: its name, ID, lineage, version and metadata.
fn world(w: &chronos::World) -> Value {
    json!({
        "name": w.name, "id": w.id, "parent": w.parent, "depth": w.depth,
        "created": w.created, "version": w.version, "flagged": w.flagged, "meta": w.meta,
        "owner": w.owner, "expires": w.expires, "active": w.active, "pinned": w.pinned, "scope": w.scope,
        "checkpoints": w.checkpoints.iter().map(|(n, t)| json!({ "name": n, "at": t })).collect::<Vec<_>>(),
    })
}

/// An agent as JSON (never its token).
fn agent(a: &chronos::Agent) -> Value {
    json!({
        "name": a.name, "id": a.id, "can": a.can.iter().map(|c| c.name()).collect::<Vec<_>>(),
        "max_worlds": a.max_worlds, "writes_per_minute": a.writes_per_minute, "max_changes": a.max_changes,
        "world_ttl": a.world_ttl, "max_query_ms": a.max_query_ms, "max_concurrent": a.max_concurrent,
        "max_memory_mb": a.max_memory_mb, "created": a.created, "disabled": a.disabled, "policy": a.policy,
        // made before agents had SCRAM verifiers: one login over HTTP with its token makes it one (see `admit`)
        "needs_http_login": a.verifier().is_none(),
    })
}

/// An agent's settings from a request: `can` (a list), quotas, limits and `world_ttl` (ms) as numbers.
fn agent_spec(a: &Value) -> Result<chronos::AgentSpec, Fail> {
    let num = |k: &str| a.get(k).and_then(Value::as_u64);
    let can = match a.get("can") {
        None | Some(Value::Null) => None,
        Some(Value::Array(cs)) => Some(
            cs.iter()
                .map(|c| c.as_str().and_then(chronos::Can::parse).ok_or_else(|| bad(format!("{c} isn't a capability"))))
                .collect::<Result<Vec<_>, Fail>>()?,
        ),
        Some(_) => return Err(bad("'can' must be a list like [\"read\", \"fork\"]")),
    };
    let policy = match a.get("policy") {
        None => None,
        Some(Value::Null) => Some(String::new()), // none
        Some(Value::String(p)) => Some(p.clone()),
        Some(_) => return Err(bad("'policy' must be a merge policy's name, or null for none")),
    };
    Ok(chronos::AgentSpec {
        can,
        max_worlds: num("max_worlds"),
        writes_per_minute: num("writes_per_minute"),
        max_changes: num("max_changes"),
        world_ttl: num("world_ttl"),
        max_query_ms: num("max_query_ms"),
        max_concurrent: num("max_concurrent"),
        max_memory_mb: num("max_memory_mb"),
        disabled: a.get("disabled").and_then(Value::as_bool),
        policy,
    })
}

/// Rows are stored as canonical JSON text; replies carry them as JSON again.
fn row(v: &str) -> Value {
    serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.into()))
}

fn key(table: &str, id: &str) -> Result<String, Fail> {
    if table.is_empty() || table.contains('/') || id.is_empty() {
        return Err(bad("table and id must be non-empty, and table can't contain '/'"));
    }
    if chronos::reserved_table(table) {
        return Err(fail(403, "tables starting with _sonos are the database's own; see the agents and worlds ops"));
    }
    Ok(format!("{table}/{id}"))
}

/// A session's transaction world (`_tx_*`) is its own, whoever asks: 404 for a request that names one
/// (as a world, as one at a moment `w@...`, or as the world a replay makes), as for a world that isn't.
pub(crate) fn private_worlds(db: &Db, a: &Value) -> Result<(), Fail> {
    let s = |k: &str| a.get(k).and_then(Value::as_str);
    let private = |w: &&str| db.private_world(w) || db.private_world(w.split_once('@').map_or(*w, |(n, _)| n));
    match ["branch", "from", "into", "name", "world", "as", "to"].iter().filter_map(|k| s(k)).find(private) {
        Some(w) => Err(fail(404, format!("no world {w}"))),
        None => Ok(()),
    }
}

pub fn call(db: &Db, op: &str, a: &Value) -> Result<Value, Fail> {
    let s = |k: &str| a.get(k).and_then(Value::as_str);
    let need = |k: &str| s(k).filter(|v| !v.is_empty()).ok_or_else(|| bad(format!("'{k}' is required (a string)")));
    // a session's transaction world is its own, whoever asks (agents are refused by their checks too)
    private_worlds(db, a)?;
    let branch = s("branch").unwrap_or(chronos::MAIN);
    let id_of = |k: &str| k.split_once('/').map_or(k, |(_, id)| id).to_string();
    match op {
        "get" => {
            let (t, id) = (need("table")?, need("id")?);
            match db.get(branch, &chronos::sql::row_key_of(db, branch, &key(t, id)?))? {
                Some(v) => Ok(json!({ "id": id, "row": row(&v) })),
                None => Err(fail(404, format!("no row {t}/{id} on {branch}"))),
            }
        }
        "put" => {
            let rec = a.get("record").filter(|r| r.is_object()).ok_or_else(|| bad("'record' must be a JSON object"))?;
            let k = chronos::sql::row_key_of(db, branch, &key(need("table")?, need("id")?)?);
            let version = db.put(branch, &k, &rec.to_string())?;
            Ok(json!({ "ok": true, "version": version }))
        }
        "delete" => {
            let k = chronos::sql::row_key_of(db, branch, &key(need("table")?, need("id")?)?);
            if db.get(branch, &k)?.is_none() {
                return Err(fail(404, format!("no row {k} on {branch}")));
            }
            let version = db.del(branch, &k)?;
            Ok(json!({ "ok": true, "version": version }))
        }
        "batch" => {
            // [{"table", "id", "record": {...} | null}], applied atomically
            let rows = a.get("rows").and_then(Value::as_array).ok_or_else(|| bad("'rows' must be an array"))?;
            let mut items = vec![];
            for r in rows {
                let t = r.get("table").and_then(Value::as_str).unwrap_or("");
                let id = r.get("id").and_then(Value::as_str).unwrap_or("");
                let rec = match r.get("record") {
                    None | Some(Value::Null) => None,
                    Some(o) if o.is_object() => Some(o.to_string()),
                    Some(_) => return Err(bad("each record must be a JSON object, or null to delete")),
                };
                items.push((chronos::sql::row_key_of(db, branch, &key(t, id)?), rec));
            }
            let refs: Vec<(&str, Option<&str>)> = items.iter().map(|(k, v)| (k.as_str(), v.as_deref())).collect();
            let version = db.batch(branch, &refs)?;
            Ok(json!({ "ok": true, "count": items.len(), "version": version }))
        }
        "find" => {
            let filters = match a.get("where") {
                None | Some(Value::Null) => vec![],
                Some(Value::Object(o)) => o.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                Some(_) => return Err(bad("'where' must be an object of field: value")),
            };
            let vector = match (s("vector_field"), a.get("vector")) {
                (Some(f), Some(v)) => Some((
                    f.to_string(),
                    serde_json::from_value::<Vec<f32>>(v.clone())
                        .map_err(|_| bad("'vector' must be an array of numbers"))?,
                )),
                (None, None) => None,
                _ => return Err(bad("give both 'vector' and 'vector_field'")),
            };
            let q = Query {
                table: need("table")?.into(),
                filters,
                text: s("text").map(String::from),
                vector,
                k: a.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize,
                offset: a.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize,
                ef: a.get("ef").and_then(Value::as_u64).unwrap_or(0) as usize,
            };
            let (total, hits) = db.find(branch, &q)?;
            let next = (q.offset.saturating_add(hits.len()) < total && hits.len() == q.k).then(|| q.offset + q.k);
            let hits: Vec<Value> = hits
                .into_iter()
                .map(|h| json!({ "id": chronos::sql::show_id(&id_of(&h.key)), "score": h.score, "row": row(&h.row) }))
                .collect();
            Ok(json!({ "total": total, "hits": hits, "next_offset": next }))
        }
        "fork" => {
            let from = db.world(s("from").unwrap_or(chronos::MAIN))?.name; // a name or a world's ID
            let w = db.fork_with(&from, need("name")?, a.get("meta").cloned().unwrap_or(Value::Null))?;
            Ok(json!({ "ok": true, "world": world(&w) }))
        }
        "branches" | "worlds" => {
            let b: Vec<Value> = db.worlds().iter().filter(|w| !w.name.starts_with("_tx_")).map(world).collect();
            Ok(json!({ "branches": b }))
        }
        "world" => Ok(world(&db.world(need("branch")?)?)),
        "set_meta" => {
            let meta = a.get("meta").cloned().ok_or_else(|| bad("'meta' must be a JSON object"))?;
            Ok(world(&db.set_meta(&db.world(need("branch")?)?.name, meta)?))
        }
        "history" => {
            let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
            let events: Vec<Value> = db
                .history(&db.world(need("branch")?)?.name, limit)?
                .into_iter()
                .map(|e| json!({ "at": e.at, "world": e.world, "event": e.what, "rows": e.rows }))
                .collect();
            Ok(json!({ "events": events }))
        }
        "create_agent" | "alter_agent" => {
            let name = need("name")?;
            let spec = agent_spec(a)?;
            if op == "create_agent" {
                let (ag, token) = db.create_agent(name, &spec)?;
                return Ok(json!({ "agent": agent(&ag), "token": token }));
            }
            Ok(json!({ "agent": agent(&db.alter_agent(name, &spec)?) }))
        }
        "drop_agent" => {
            db.drop_agent(need("name")?)?;
            Ok(json!({ "ok": true }))
        }
        "agents" => Ok(json!({ "agents": db.agents()?.iter().map(agent).collect::<Vec<_>>() })),
        "audit" => {
            let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
            let events: Vec<Value> = db
                .audit(s("agent"), limit)?
                .into_iter()
                .map(|e| json!({ "at": e.at, "agent": e.agent, "world": e.world, "action": e.action, "rows": e.rows }))
                .collect();
            Ok(json!({ "events": events }))
        }
        "merge_outcomes" => {
            let by_policy = s("by") == Some("policy");
            let out = db.merge_outcomes(by_policy)?;
            let rows: Vec<Value> = out
                .rows
                .iter()
                .map(|o| {
                    let mut r = json!({
                        "worlds": o.worlds, "on_rules": o.on_rules, "no_policy": o.no_policy, "approved": o.approved,
                        "rejected": o.rejected, "discarded": o.discarded, "open": o.open,
                        "partly_merged": o.partly_merged, "before_window": o.before_window, "conflicts": o.conflicts,
                        "stale_reads": o.stale_reads, "check_refusals": o.check_refusals, "holds": o.holds,
                        "rules_rate": o.rules_rate(),
                    });
                    let name = (!o.name.is_empty()).then(|| o.name.clone());
                    if by_policy {
                        r["policy"] = json!(name);
                    } else {
                        r["agent"] = json!(name);
                        r["policy"] = json!((!o.policy.is_empty()).then(|| o.policy.clone()));
                    }
                    r
                })
                .collect();
            // with no log (in memory) there is no window: since is null
            Ok(json!({ "since": (out.since > 0).then_some(out.since), "outcomes": rows }))
        }
        "backup" => {
            let b = db.backup(need("to")?).map_err(Error::from)?;
            Ok(json!({ "files": b.files, "bytes": b.bytes }))
        }
        "verify" => {
            let v = db.verify().map_err(Error::from)?;
            Ok(json!({ "pages": v.pages, "bytes": v.bytes, "log_records": v.log_records }))
        }
        "checkpoint" => {
            let at = db.checkpoint_world(need("branch")?, need("name")?)?;
            Ok(json!({ "ok": true, "at": at }))
        }
        "undo_merge" => {
            let skip = a.get("skip_changed").and_then(Value::as_bool) == Some(true);
            let u = db.undo_merge(need("branch")?, skip)?;
            Ok(json!({ "ok": true, "parent": u.parent, "at": u.at, "undone": u.rows, "skipped": u.skipped }))
        }
        "restore" if s("checkpoint").is_some() => {
            let b = db.world(need("branch")?)?.name;
            let t = db.checkpoint_time(&b, need("checkpoint")?)?;
            Ok(json!({ "ok": true, "restored": db.restore(&b, t)? }))
        }
        "restore" => {
            let at = need("at")?;
            let t = chronos::sql::moment(at)
                .ok_or_else(|| bad(format!("'at' must be a time or a time ago, not '{at}'")))?;
            Ok(json!({ "ok": true, "restored": db.restore(&db.world(need("branch")?)?.name, t)? }))
        }
        "diff" => {
            let b = need("branch")?;
            let count = a.get("count").and_then(Value::as_bool) == Some(true);
            let limit = match a.get("limit") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().ok_or_else(|| bad("'limit' must be a whole number"))? as usize),
            };
            let after = match a.get("after") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_str().ok_or_else(|| bad("'after' must be the 'next' of a previous page"))?),
            };
            let total = |db: &Db| -> Result<Value, Error> {
                let tables = db.diff_count(b, s("to"))?;
                Ok(json!({ "total": tables.values().sum::<usize>(), "tables": tables }))
            };
            // "version": the branch's, read first, so merging at it merges no more than this shows
            let version = match s("to") {
                None => Some(db.version(b)?),
                Some(_) => None,
            };
            // a page after the first sends the version the first returned: a branch written to since
            // is refused (409), else a row added before the cursor would be skipped by the pages yet
            // in a later page's version, and merging at that would take it in unseen
            let pinned = a.get("version").and_then(Value::as_u64);
            let unchanged = |db: &Db| -> Result<(), Fail> {
                match (pinned, version) {
                    (Some(p), Some(_)) if db.version(b)? != p => Err(Fail(
                        409,
                        json!({
                            "message": format!(
                                "{b} changed since version {p}, which this review started at: read its diff again from the first page"
                            )
                        }),
                    )),
                    _ => Ok(()),
                }
            };
            unchanged(db)?;
            // "readers": true adds what in the database reads the columns the whole diff changes
            let readers = match a.get("readers").and_then(Value::as_bool) == Some(true) {
                true => {
                    let changes = match s("to") {
                        Some(to) => db.diff_between(b, to)?,
                        None => db.diff(b)?,
                    };
                    let i = chronos::sql::impact(db, s("to").unwrap_or(b), &changes).map_err(|e| bad(e.message))?;
                    Some(impact_json(i))
                }
                false => None,
            };
            let with = |mut out: Value| {
                if let Some(r) = &readers {
                    out["readers"] = r["readers"].clone();
                    out["coverage"] = r["coverage"].clone();
                }
                if let Some(v) = version {
                    out["version"] = json!(v);
                }
                out
            };
            if count && limit.is_none() && after.is_none() {
                return Ok(with(total(db)?));
            }
            let show = |changes: &[chronos::Change]| -> Vec<Value> {
                let described = chronos::sql::describe(changes);
                let columns: std::collections::HashMap<&str, &Vec<String>> =
                    described.iter().map(|d| (d.key.as_str(), &d.columns)).collect();
                changes
                    .iter()
                    .map(|c| {
                        json!({
                            "key": chronos::sql::show_key(&c.key), "before": c.before.as_deref().map(row), "after": c.after.as_deref().map(row),
                            "columns": columns.get(c.key.as_str()),
                        })
                    })
                    .collect()
            };
            if limit.is_some() || after.is_some() {
                // a page: walks the diff only as far as the page (plus a full count if asked)
                if a.get("sql").and_then(Value::as_bool) == Some(true) {
                    return Err(bad("'sql' can't be paged; leave out 'limit' and 'after'"));
                }
                let (changes, next) = db.diff_page(b, s("to"), after, limit.unwrap_or(usize::MAX))?;
                unchanged(db)?; // and not while it was read
                let mut out = json!({ "changes": show(&changes), "next": next });
                if count {
                    let t = total(db)?;
                    out["total"] = t["total"].clone();
                    out["tables"] = t["tables"].clone();
                }
                return Ok(with(out));
            }
            let changes = match s("to") {
                Some(to) => db.diff_between(b, to)?,
                None => db.diff(b)?,
            };
            let described = chronos::sql::describe(&changes);
            if a.get("sql").and_then(Value::as_bool) == Some(true) {
                let after = db.view(s("to").unwrap_or(b))?;
                let sql = chronos::sql::diff_sql(&after, &described).map_err(|e| bad(e.message))?;
                return Ok(with(json!({ "sql": sql })));
            }
            Ok(with(json!({ "changes": show(&changes) })))
        }
        "merge" => {
            let how = match s("resolve").unwrap_or("fail") {
                "fail" => Resolve::Fail,
                "ours" => Resolve::Ours,
                "theirs" => Resolve::Theirs,
                r => return Err(bad(format!("resolve must be fail, ours or theirs, not '{r}'"))),
            };
            let b = need("branch")?;
            // "version": what your last write returned (catches writes lost in a crash);
            // "confirm": true takes the branch as it is now, after you've checked its diff
            let expected = match (a.get("version").and_then(Value::as_u64), a.get("confirm").and_then(Value::as_bool)) {
                (Some(v), _) => Some(v),
                (None, Some(true)) => Some(db.version(b)?),
                _ => None,
            };
            // "picks": {"table/id": "ours" | "theirs" | {row} | null}: rows settled one by one
            let mut picks = std::collections::HashMap::new();
            if let Some(ps) = a.get("picks") {
                let ps = ps.as_object().ok_or_else(|| bad("'picks' must be an object of \"table/id\": choice"))?;
                for (k, p) in ps {
                    let pick = match p {
                        Value::String(s) if s == "ours" => chronos::Pick::Ours,
                        Value::String(s) if s == "theirs" => chronos::Pick::Theirs,
                        Value::Null => chronos::Pick::Row(None),
                        Value::Object(_) => chronos::Pick::Row(Some(p.to_string())),
                        _ => {
                            return Err(bad(format!(
                                "pick for {k} must be \"ours\", \"theirs\", a row object, or null to delete"
                            )));
                        }
                    };
                    picks.insert(chronos::sql::row_key_of(db, b, k), pick);
                }
            }
            let columns = a.get("columns").and_then(Value::as_bool) == Some(true);
            // "only_tables": ["t", ...], "only_keys": ["t/1", ...]: merge just those, the branch stays;
            // "into": another world to merge into instead of the parent (the branch stays too)
            let names = |k: &str| -> Result<Vec<String>, Fail> {
                let v = match a.get(k) {
                    None | Some(Value::Null) => return Ok(vec![]),
                    Some(Value::Array(v)) => v.iter().map(|x| x.as_str().map(String::from)).collect(),
                    Some(_) => None,
                };
                v.ok_or_else(|| bad(format!("'{k}' must be an array of strings")))
            };
            let into = s("into").map(|y| db.world(y).map(|w| w.name)).transpose()?;
            let o = chronos::MergeOptions {
                how,
                expected,
                picks,
                columns,
                only_tables: names("only_tables")?,
                only_keys: names("only_keys")?.iter().map(|k| chronos::sql::row_key_of(db, b, k)).collect(),
                into,
                // "target": a dry run's, so rows settled are settled against what it saw (409 otherwise)
                target: a.get("target").and_then(Value::as_u64),
            };
            if a.get("dry_run").and_then(Value::as_bool) == Some(true) {
                let plan = db.merge_preview(b, &o)?;
                let rows: Vec<Value> = plan
                    .rows
                    .iter()
                    .map(|p| {
                        json!({
                            "key": chronos::sql::show_key(&p.key), "outcome": p.outcome, "detail": p.detail,
                            "base": p.base.as_deref().map(row), "ours": p.ours.as_deref().map(row),
                            "theirs": p.theirs.as_deref().map(row), "result": p.result.as_deref().map(row),
                        })
                    })
                    .collect();
                // what reads the columns it would change, in the world it merges into
                let keys: std::collections::HashSet<&str> = plan.rows.iter().map(|p| p.key.as_str()).collect();
                let changes: Vec<chronos::Change> =
                    db.diff(b)?.into_iter().filter(|c| keys.contains(c.key.as_str())).collect();
                let target = match &o.into {
                    Some(y) => y.clone(),
                    None => db.world(b)?.parent.unwrap_or_else(|| chronos::MAIN.into()),
                };
                let i = chronos::sql::impact(db, &target, &changes).map_err(|e| bad(e.message))?;
                // "version": pass it back to merge only what this showed (409 if the branch changed since)
                let mut out = json!({ "rows": rows, "conflicts": plan.conflicts, "blocked": plan.blocked, "version": plan.version, "target": plan.target });
                // into main of a folder attached to Postgres: the rows it would write, as Postgres has them now
                if let Some(l) = &plan.live {
                    out["postgres"] = json!({
                        "source": l.source, "at": l.at, "error": l.error, "triggers": l.triggers,
                        "stale_reads": l.stale_reads,
                        "summary": chronos::sql::live_summary(l),
                        "rows": l.rows.iter().map(|r| json!({
                            "key": chronos::sql::show_key(&r.key), "changed": r.changed,
                            "postgres": r.postgres.as_deref().map(row),
                        })).collect::<Vec<_>>(),
                    });
                }
                let i = impact_json(i);
                out["readers"] = i["readers"].clone();
                out["coverage"] = i["coverage"].clone();
                return Ok(out);
            }
            match db.merge_kept(b, &o) {
                Ok((n, kept)) => {
                    // the branch stays (merged in part, or into another world): its version as the merge
                    // left it, which a partial merge moved, for the next merge's "version"
                    let mut out = json!({ "merged": n });
                    if let Some(v) = kept {
                        out["version"] = json!(v);
                    }
                    Ok(out)
                }
                Err(Error::Conflicts(cs)) => {
                    let conflicts: Vec<Value> = cs
                        .iter()
                        .map(|c| {
                            json!({
                                "key": chronos::sql::show_key(&c.key),
                                "base": c.base.as_deref().map(row),
                                "ours": c.ours.as_deref().map(row),
                                "theirs": c.theirs.as_deref().map(row),
                                "explain": c.explain(),
                            })
                        })
                        .collect();
                    Err(Fail(
                        409,
                        json!({
                            "message": format!(
                                "{} rows changed on both sides; nothing merged. Retry with resolve: ours or theirs, \
                                 columns: true, or picks (see dry_run: true)",
                                cs.len()
                            ),
                            "conflicts": conflicts,
                        }),
                    ))
                }
                Err(e) => Err(e.into()),
            }
        }
        "discard" if a.get("cascade").and_then(Value::as_bool) == Some(true) => {
            let n = db.discard_tree(&db.world(need("branch")?)?.name)?;
            Ok(json!({ "ok": true, "discarded": n }))
        }
        "discard" => {
            db.discard(need("branch")?)?;
            Ok(json!({ "ok": true }))
        }
        "sql" => run_sql(db, a, None),
        "simulate" | "replay" => simulate(db, op, a, None),
        _ => Err(fail(
            404,
            format!(
                "no operation '{op}': use sql, get, put, delete, batch, find, fork, branches, diff, merge, discard, \
                 simulate or replay"
            ),
        )),
    }
}
