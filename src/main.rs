//! `chronos`: the command-line shell, like `sqlite3`.
//!
//!   chronos mydb                   interactive shell on the folder `mydb` (created if missing)
//!   chronos mydb get users 1       run one command and exit (exit code 1 on error)
//!   chronos mydb < script.txt      run commands from stdin, one per line
//!   chronos -b agent-7 mydb ...    start on a branch other than main
//!   chronos mcp mydb [--allow-merge]  serve the database to AI agents over MCP (stdio); agents
//!                                change only branches they fork, and a person merges, unless --allow-merge
//!   chronos import mydb users data.csv --schema schema.csv
//!                                a copy of one Postgres table (see src/import.rs, docs/sql.md)
//!   chronos mydb backup /backups/mydb-1   copy the database into a new folder (while it runs: BACKUP
//!                                DATABASE TO '...' over SQL, or the `backup` HTTP op, on the server)
//!   chronos restore /backups/mydb-1 mydb2   start a database from a backup, and verify it
//!   chronos keygen                 a new encryption key; CHRONOS_KEY=<key> (or CHRONOS_KEY_FILE=<file>)
//!                                encrypts a new database, and is then needed to open it
//!   chronos serve mydb [--listen 127.0.0.1:7070] [--pg 127.0.0.1:5433|off] [--token T]
//!                [--tls-cert cert.pem --tls-key key.pem]   HTTPS, and TLS on the Postgres port (needed
//!                                to listen on any address but loopback, unless --insecure-no-tls)
//!                [--safe [--admin-token A]]   clients that aren't agents get MCP's safe mode
//!                                HTTP JSON API, and the Postgres protocol for psql and drivers
//!   chronos studio mydb [--port N] [--no-open]   a web UI for the database on 127.0.0.1 (see
//!                                src/studio.rs, docs/studio.md), beside a running `serve` too
//!   chronos studio mydb --login [--listen ADDR:PORT] [--host NAME[:PORT]] [--tls-cert ... --tls-key ...]
//!                                the same, signed in with a database role (its privileges), reachable elsewhere
//!   chronos --remote s3://bucket/db mydb   back the folder with object storage (`--features s3`)
//!   chronos --anchor /etc/chronos/mydb.anchor mydb   refuse the folder if its files were rolled back
//!                                (or CHRONOS_ANCHOR=<file>; --anchor-reset or CHRONOS_ANCHOR_RESET=1
//!                                accepts it as it is, after a restore from a backup)
//!
//! Records are JSON objects keyed `table/id`, stored with sorted keys so equal records compare equal.

mod api;
mod backoff;
mod http;
mod import;
mod local;
mod mcp;
mod pg;
mod pgimport;
use chronos::pgclient;
mod studio;
mod studio_import;
mod tls;

use chronos::sql::show_key;
use chronos::{Db, Error, MAIN, Resolve};
use rustyline::error::ReadlineError;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal};
use std::sync::Arc;

pub(crate) const PREVIEW: usize = 20;

const HELP: &str = "\
sql       any SQL statement, e.g. select name from users where age > 30 order by name;
          create table, drop table, insert, select (with joins, group by), update, delete;
          begin; ... commit; (or rollback;) makes several statements one transaction;
          create branch, merge branch, drop branch, show branches: branches in SQL (see docs/sql.md);
          end with ; to spread a statement over several lines
records   put <table> <id> <json>      write a row, e.g. put users 1 {\"name\": \"Ada\"}
          get <table> <id>             read a row
          del <table> <id>             delete a row
          scan <table> [n]             list rows (first 20 unless n)
          find <table> [where <f> = <v> [and ...]] [match <words>] [near <field> [numbers]] [limit n] [offset n]
                                       filters, text search, vector search; best matches first
          tables                       tables and row counts
          describe [table]             fields, example and allowed values, a sample row
worlds    fork <name> [from <world>]   a new world: an instant copy of the current one; switch to it
          use <world>                  switch world
          worlds                       list worlds (also: branches)
          diff [world]                 what changed since the fork
          merge [world] [ours|theirs] [confirm | at <version>]  merge into the parent; on conflict,
                                       pick a side; after a crash, confirm once you've checked the
                                       diff; at <version> (diff shows it) merges only what you read
          discard [world] [cascade]    throw a world away (with cascade, its forks too)
          undo [world]                 undo the latest merge into this world (or of world): puts back
                                       the rows it changed; refuses if any changed again since
other     migrate <folder>             apply the folder's new .sql files in name order, each in one
                                       transaction; applied ones are recorded and skipped
          checkpoint                   compact the log (speeds up the next open)
          backup <folder>              copy the database, while it runs, into a new folder
          verify                       read every page and log record, checking each
          gc                           delete pages nothing uses any more (runs on its own too)
          help, quit";

/// Who a server's clients are (see `api` and `pg`): the server's token, the admin token, and
/// whether safe mode is on (then a client without an agent's or the admin token is the guest).
pub(crate) struct Auth {
    token: Option<String>,
    admin: Option<String>,
    safe: bool,
}

pub(crate) struct Shell {
    db: Arc<Db>, // shared with other processes' sessions when this process serves the folder
    branch: String,
    sql: chronos::sql::Session, // SQL's branch and open transaction, kept in step with `branch`
}

// however a shell ends (a client gone mid-line, a panic), a transaction it left open is rolled back
impl Drop for Shell {
    fn drop(&mut self) {
        self.sql.end(&self.db);
    }
}

/// A SQL session that is closed (an open transaction rolled back) when dropped, a panic included.
pub(crate) struct Closing<'a>(pub chronos::sql::Session, pub &'a Db);

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        self.0.close(self.1);
    }
}

// musl's own malloc is slow with several threads: on Linux it took most of a vector search's
// time. glibc's hands a big query's batches back to the kernel (munmap, madvise) and faults them
// in again on the next run. Linux builds use mimalloc instead; macOS keeps the system allocator.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The last part of a folder's path (`./data/leads/` is `leads`).
fn folder_name(dir: &str) -> String {
    let p = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.into());
    p.file_name().map_or_else(|| dir.to_string(), |n| n.to_string_lossy().into_owned())
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut flag = |names: &[&str]| -> Option<String> {
        let i = args.iter().position(|a| names.contains(&a.as_str()))?;
        args.remove(i);
        if i >= args.len() {
            usage();
        }
        Some(args.remove(i))
    };
    let branch = flag(&["-b", "--branch"]).unwrap_or_else(|| MAIN.to_string());
    let remote = flag(&["--remote"]);
    let agent = flag(&["--agent"]);
    let listen_given = flag(&["--listen"]);
    let listen = listen_given.clone().unwrap_or_else(|| "127.0.0.1:7070".into());
    let pg_listen = flag(&["--pg"]).unwrap_or_else(|| "127.0.0.1:5433".into());
    let schema = flag(&["--schema"]);
    let attach_tables = flag(&["--tables"]);
    let source_file = flag(&["--source-file"]);
    let port = flag(&["--port"]);
    let mut hosts = vec![]; // chronos studio --login's names, --host repeated
    while let Some(h) = flag(&["--host"]) {
        hosts.push(h);
    }
    let (tls_cert, tls_key) = (flag(&["--tls-cert"]), flag(&["--tls-key"]));
    let tls_given = tls_cert.is_some() || tls_key.is_some();
    let tls_cert = tls_cert.or_else(|| std::env::var("CHRONOS_TLS_CERT").ok()).filter(|t| !t.is_empty());
    let tls_key = tls_key.or_else(|| std::env::var("CHRONOS_TLS_KEY").ok()).filter(|t| !t.is_empty());
    let token = flag(&["--token"]).or_else(|| std::env::var("CHRONOS_TOKEN").ok()).filter(|t| !t.is_empty());
    let admin =
        flag(&["--admin-token"]).or_else(|| std::env::var("CHRONOS_ADMIN_TOKEN").ok()).filter(|t| !t.is_empty());
    let anchor = flag(&["--anchor"]).or_else(|| std::env::var("CHRONOS_ANCHOR").ok()).filter(|a| !a.is_empty());
    let safe = args.iter().position(|a| a == "--safe").map(|i| args.remove(i)).is_some();
    let insecure_no_tls = args.iter().position(|a| a == "--insecure-no-tls").map(|i| args.remove(i)).is_some();
    let plain_password = args.iter().position(|a| a == "--allow-plaintext-password").map(|i| args.remove(i)).is_some();
    let takeover = args.iter().position(|a| a == "--takeover").map(|i| args.remove(i)).is_some();
    let anchor_reset = args.iter().position(|a| a == "--anchor-reset").map(|i| args.remove(i)).is_some()
        || std::env::var("CHRONOS_ANCHOR_RESET").is_ok_and(|v| v == "1");
    let allow_merge = args.iter().position(|a| a == "--allow-merge").map(|i| args.remove(i)).is_some();
    let no_open = args.iter().position(|a| a == "--no-open").map(|i| args.remove(i)).is_some();
    let login = args.iter().position(|a| a == "--login").map(|i| args.remove(i)).is_some();
    let dry_run = args.iter().position(|a| a == "--dry-run").map(|i| args.remove(i)).is_some();
    let yes = args.iter().position(|a| a == "--yes").map(|i| args.remove(i)).is_some();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        usage();
    }
    if args[0] == "-V" || args[0] == "--version" {
        println!("chronos {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // an option nobody took is a typo, not a folder name: opening it would make a database called "--x"
    if let Some(a) = args.iter().find(|a| a.starts_with("--")) {
        eprintln!("chronos: unknown option {a}; see chronos --help");
        std::process::exit(2);
    }
    if args[0] == "keygen" {
        println!("{}", chronos::crypt::new_key());
        eprintln!(
            "chronos: keep this key safe: without it the database can't be read. Use it with CHRONOS_KEY=<key> (or CHRONOS_KEY_FILE)"
        );
        return;
    }
    let key = match db_key() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("chronos: {e}");
            std::process::exit(1);
        }
    };
    // the Postgres an attached folder is a copy of: from a file only its owner reads (see chronos::attach::source_url)
    if let Some(f) = &source_file {
        // (before any thread starts: nothing reads the environment at the same time)
        unsafe { std::env::set_var("CHRONOS_SOURCE_FILE", f) };
    }
    // chronos attach <new folder> --tables ... | --schema ...: tables of a Postgres beside which Chronos works
    if args[0] == "attach" {
        if args.len() != 2 {
            eprintln!("{}", pgimport::attach_help());
            std::process::exit(2);
        }
        let opts = chronos::Open {
            remote: remote.clone(),
            takeover,
            key,
            anchor: anchor.clone().map(Into::into),
            anchor_reset,
        };
        let split = |v: &Option<String>| -> Vec<String> {
            v.iter().flat_map(|s| s.split(',')).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
        };
        if let Err(e) = pgimport::attach(&args[1], &opts, &split(&attach_tables), &split(&schema), yes) {
            eprintln!("chronos attach: {e}");
            std::process::exit(1);
        }
        return;
    }
    if attach_tables.is_some() || yes {
        eprintln!("chronos: --tables and --yes are for chronos attach");
        std::process::exit(2);
    }
    // chronos import <folder> postgres://...: a live Postgres database, moved in one command
    let from_pg = |a: &String| a.starts_with("postgres://") || a.starts_with("postgresql://");
    if args[0] == "import" && args.get(2).is_some_and(from_pg) {
        if args.len() != 3 {
            eprintln!("{}", pgimport::help());
            std::process::exit(2);
        }
        let opts = chronos::Open {
            remote: remote.clone(),
            takeover,
            key,
            anchor: anchor.clone().map(Into::into),
            anchor_reset,
        };
        let db = match dry_run {
            true => None,
            false => match Db::open_with(&args[1], &opts) {
                Ok(db) => Some(db),
                Err(e) => {
                    eprintln!("chronos import: {}: {e} (stop what has it open, or import into a new folder)", args[1]);
                    std::process::exit(1);
                }
            },
        };
        if let Err(e) = pgimport::run(db.as_ref(), &args[2], &branch, dry_run, plain_password) {
            eprintln!("chronos import: {e}");
            std::process::exit(1);
        }
        return;
    }
    if dry_run || plain_password {
        let flag = if dry_run { "--dry-run" } else { "--allow-plaintext-password" };
        eprintln!("chronos: {flag} is for chronos import <folder> postgres://...");
        std::process::exit(2);
    }
    let studio_login = args[0] == "studio" && login;
    if insecure_no_tls && args[0] != "serve" && !studio_login {
        eprintln!("chronos: --insecure-no-tls is for chronos serve and chronos studio --login");
        std::process::exit(2);
    }
    if (login || !hosts.is_empty()) && args[0] != "studio" {
        eprintln!("chronos: --login and --host are for chronos studio");
        std::process::exit(2);
    }
    // the key in the link is for 127.0.0.1 only: anywhere else, people sign in with roles
    if args[0] == "studio" && !login && (listen_given.is_some() || !hosts.is_empty() || tls_given) {
        eprintln!(
            "chronos: chronos studio serves 127.0.0.1 only; --listen, --host, --tls-cert and --tls-key need --login \
             (people sign in with database roles)"
        );
        std::process::exit(2);
    }
    if studio_login && listen_given.is_some() && port.is_some() {
        eprintln!("chronos: give the port in --listen ADDR:PORT, not --port too");
        std::process::exit(2);
    }
    if args[0] == "restore" {
        if args.len() != 3 {
            usage();
        }
        match Db::from_backup(&args[1], &args[2], key.as_ref()) {
            Ok(_) => println!("restored {} into {}, and verified it", args[1], args[2]),
            Err(e) => {
                eprintln!("chronos: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let serve = args[0] == "serve";
    let mcp = args[0] == "mcp";
    let import = args[0] == "import";
    let studio = args[0] == "studio";
    let port: u16 = match port.as_deref().map(str::parse) {
        None => 0,
        Some(Ok(p)) if studio => p,
        _ => usage(),
    };
    // chronos studio --login: where it listens (127.0.0.1, at --port, unless --listen says), checked before the
    // folder is opened; anywhere but loopback needs TLS unless --insecure-no-tls
    let signin = if studio_login {
        let listen = listen_given.clone().unwrap_or_else(|| format!("127.0.0.1:{port}"));
        let tls = match (&tls_cert, &tls_key) {
            (Some(c), Some(k)) => tls::config(c, k).map(Some),
            (None, None) => Ok(None),
            _ => Err(std::io::Error::other("TLS needs both --tls-cert and --tls-key")),
        };
        let checked = tls.and_then(|tls| match loopback(&listen) {
            true => Ok(tls),
            false => check_tls(&listen, tls.is_some(), insecure_no_tls).map(|()| tls),
        });
        match checked {
            Ok(tls) => Some(studio::Login { listen, hosts: hosts.clone(), tls }),
            Err(e) => {
                eprintln!("chronos: {e}");
                std::process::exit(2)
            }
        }
    } else {
        None
    };
    // the shell with nothing to run: typed into, and the one shell that lives long enough to serve
    let interactive = !serve && !mcp && !import && !studio && args.len() == 1 && std::io::stdin().is_terminal();
    // before any thread starts (opening starts one), so only our waiter gets them: a process that
    // others may be using closes the database cleanly instead of dying mid-write
    let stop = (serve || mcp || interactive || studio).then(stop_signals);
    if import && (args.len() != 4 || schema.is_none()) {
        eprintln!("{}", import::help());
        std::process::exit(2);
    }
    if mcp || serve || import || studio {
        let cmd = args.remove(0);
        if args.is_empty() {
            eprintln!(
                "chronos {cmd}: give the database's folder, e.g. chronos {cmd} mydb (a new name makes a new database)"
            );
            std::process::exit(2);
        }
        if studio && args.len() > 1 {
            usage();
        }
    }
    let dir = args.remove(0);
    if remote.is_none() && std::path::Path::new(&dir).is_file() {
        eprintln!("chronos: {dir} is a file; give a database folder (a new name makes a new database)");
        std::process::exit(2);
    }
    // if another process has the folder open, run there as a session of our own (see local.rs)
    let hello = if serve || remote.is_some() {
        None // a server must own the folder; a remote folder has one owner by lease
    } else if mcp {
        Some(serde_json::json!({"mode": "mcp", "branch": branch.clone(), "agent": agent, "allow_merge": allow_merge}))
    } else if studio {
        Some(serde_json::json!({"mode": "api"}))
    } else if import {
        let file = |i: usize| local::absolute(&args[i]);
        let schema = local::absolute(schema.as_deref().unwrap_or_default());
        Some(
            serde_json::json!({"mode": "import", "branch": branch.clone(), "table": args[0], "data": file(1), "schema": schema}),
        )
    } else {
        Some(serde_json::json!({"mode": "shell", "branch": branch.clone()}))
    };
    let studio_opts = studio::Options { port, open: !no_open, login: signin };
    let opts = chronos::Open { remote: remote.clone(), takeover, key, anchor: anchor.map(Into::into), anchor_reset };
    let opened = local::open_or_connect(&dir, || Db::open_with(&dir, &opts).map_err(|e| e.to_string()), hello);
    let db = match opened {
        Ok(local::Opened::Here(db)) => db,
        Ok(local::Opened::Remote(from, to)) if studio => {
            drop((from, to)); // each call connects on its own: a session left open would keep the owner from quitting
            if let Some(set) = stop {
                std::thread::spawn(move || {
                    wait_for(set);
                    std::process::exit(0) // nothing to close: the owner holds the database
                });
            }
            if let Err(e) = studio::run(studio::Backend::Remote(dir.clone()), &folder_name(&dir), studio_opts) {
                eprintln!("chronos: {e}");
                std::process::exit(1);
            }
            std::process::exit(0)
        }
        Ok(local::Opened::Remote(from, to)) => {
            if let Some(set) = stop {
                std::thread::spawn(move || {
                    wait_for(set);
                    std::process::exit(130) // the owner ends our session when the socket closes
                });
            }
            remote_client(mcp, import, interactive, &args, &branch, from, to)
        }
        Err(e) => {
            eprintln!("chronos: {e}");
            std::process::exit(1)
        }
    };
    chronos::sql::cleanup(&db); // transactions of a process that died
    let db = Arc::new(db);
    let sql = chronos::sql::Session::new(&branch);
    let mut sh = Shell { db, branch, sql };
    if sh.db.branches().iter().all(|(b, _)| *b != sh.branch) {
        eprintln!("chronos: no branch '{}'; see: chronos {dir} branches", sh.branch);
        std::process::exit(1);
    }

    // long-lived processes serve the folder to later ones; a closing waiter on SIGTERM/Ctrl-C
    let served = (mcp || interactive || serve || studio)
        .then(|| {
            local::listen(sh.db.clone(), &dir)
                .inspect_err(|e| eprintln!("chronos: other chronos processes can't share this folder: {e}"))
                .ok()
        })
        .flatten();
    if let (Some(set), false) = (stop, serve) {
        let db = sh.db.clone();
        std::thread::spawn(move || {
            wait_for(set);
            eprintln!("chronos: stopping: closing the database");
            std::process::exit(if db.close().is_ok() { 0 } else { 1 });
        });
    }
    if mcp {
        let guard = match &agent {
            Some(name) => {
                if let Err(e) = sh.db.as_agent(name) {
                    eprintln!("chronos: {e}");
                    drop(sh);
                    std::process::exit(1);
                }
                mcp::Guard::agent(name)
            }
            None => mcp::Guard::new(allow_merge),
        };
        let db = sh.db.clone();
        mcp::serve(sh, guard);
        finish(db, served);
    }
    if studio {
        let db = sh.db.clone();
        if let Err(e) = studio::run(studio::Backend::Here(db.clone()), &folder_name(&dir), studio_opts) {
            eprintln!("chronos: {e}");
        }
        drop(sh);
        finish(db, served);
    }
    if import {
        let r = import::run(&sh.db, &sh.branch, &args[0], &args[1], schema.as_deref().unwrap_or_default());
        drop(sh); // close the database before exit, which skips destructors
        match r {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("chronos: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if serve {
        let pg_on = pg_listen != "off";
        let tls_on = tls_cert.is_some() || tls_key.is_some();
        let checked = check_token("server token (--token or CHRONOS_TOKEN)", token.as_deref())
            .and_then(|_| check_token("admin token (--admin-token or CHRONOS_ADMIN_TOKEN)", admin.as_deref()))
            .and_then(|_| check_listen(&listen, token.as_deref(), tls_on, insecure_no_tls))
            .and_then(
                |_| if pg_on { check_listen(&pg_listen, token.as_deref(), tls_on, insecure_no_tls) } else { Ok(()) },
            );
        let tls = match (&tls_cert, &tls_key) {
            (Some(c), Some(k)) => tls::config(c, k).map(Some),
            (None, None) => Ok(None),
            _ => Err(std::io::Error::other("TLS needs both --tls-cert and --tls-key")),
        };
        let db = &sh.db;
        // attached to a Postgres: that it answers as the one it was copied from, and a write-back a crash left settled
        if db.attached().is_some() {
            match chronos::attach::check(db) {
                Ok(line) => eprintln!("chronos: {line}"),
                Err(e) => eprintln!("chronos: warning: {e}; worlds work, merges into main are refused until it does"),
            }
        }
        // reachable from other machines, where a server token in someone's .env changes main with no world and no
        // review: said at start, unless --safe (the token alone is then the guest) or protect_main covers it
        let exposed = !loopback(&listen) || (pg_on && !loopback(&pg_listen));
        if checked.is_ok() && exposed && !safe && db.setting(chronos::Setting::ProtectMain) == 0 {
            eprintln!(
                "chronos: warning: the server token can change main directly, with no world and no review. If agents or \
                 their tools can reach this server, give each agent its own token (CREATE AGENT), keep the server token \
                 out of their environment, and serve with --safe --admin-token, or ALTER SYSTEM SET protect_main = on \
                 (see the hardened deployment checklist in docs/security.md)"
            );
        }
        let auth = &Auth { token, admin, safe };
        let r = checked.and(tls).and_then(|tls| {
            std::thread::scope(|s| {
                s.spawn(move || {
                    wait_for(stop.expect("blocked for serve"));
                    eprintln!("chronos: stopping: closing the database");
                    let code = match db.close() {
                        Ok(()) => 0,
                        Err(e) => {
                            eprintln!("chronos: closing failed: {e}; the next open recovers from the log");
                            1
                        }
                    };
                    std::process::exit(code);
                });
                let pg_tls = tls.clone();
                // each server runs until the process ends; one that can't start ends the process
                if pg_on {
                    s.spawn(|| {
                        if let Err(e) = pg::serve(db, &pg_listen, auth, pg_tls) {
                            eprintln!("chronos: {e}");
                            std::process::exit(1);
                        }
                    });
                }
                if let Err(e) = api::serve(db, &listen, auth, tls.map(|t| t.cfg)) {
                    eprintln!("chronos: {e}");
                    std::process::exit(1);
                }
                Ok(())
            })
        });
        if let Err(e) = r {
            eprintln!("chronos: {e}");
            std::process::exit(1);
        }
        return;
    }
    if !args.is_empty() {
        let ok = sh.run(&args.join(" "));
        drop(sh); // exit skips destructors: close the database first, or unflushed writes (a fork) are lost
        std::process::exit(if ok { 0 } else { 1 });
    }
    if !std::io::stdin().is_terminal() {
        let (mut ok, mut pending) = (true, String::new());
        for line in std::io::stdin().lock().lines() {
            let line = line.unwrap_or_default();
            if pending.is_empty() && is_quit(&line) {
                break;
            }
            if let Some(cmd) = feed(&mut pending, &line) {
                ok &= sh.run(&cmd);
            }
        }
        if !pending.is_empty() {
            ok &= sh.run(&pending);
        }
        drop(sh); // see above
        std::process::exit(if ok { 0 } else { 1 });
    }

    println!("chronos: {dir} (type help)");
    let mut rl = rustyline::DefaultEditor::new().expect("terminal");
    let mut pending = String::new();
    loop {
        let tx = if sh.sql.status() == b'I' { "" } else { " (transaction)" };
        let prompt = format!("chronos:{}{tx}{}> ", sh.branch, if pending.is_empty() { "" } else { "-" });
        match rl.readline(&prompt) {
            Ok(line) if pending.is_empty() && is_quit(&line) => break,
            Ok(line) => {
                if let Some(cmd) = feed(&mut pending, &line) {
                    let _ = rl.add_history_entry(cmd.trim_end());
                    sh.run(&cmd);
                }
            }
            Err(ReadlineError::Interrupted) => pending.clear(),
            Err(_) => break, // Ctrl-D
        }
    }
    let db = sh.db.clone();
    drop(sh);
    finish(db, served);
}

/// Ends a process that served the folder: waits for other processes' sessions, then closes the
/// database (the serving threads still hold it, so dropping it here wouldn't) and exits.
fn finish(db: Arc<Db>, served: Option<local::Server>) -> ! {
    if let Some(s) = served {
        s.drain();
    }
    let code = match db.close() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("chronos: closing failed: {e}; the next open recovers from the log");
            1
        }
    };
    std::process::exit(code)
}

/// This process as a client of the one that has the folder open: `chronos mcp` relays JSON-RPC,
/// the shell sends commands and prints the answers, `import` asks for the import. Exits.
fn remote_client(
    mcp: bool,
    import: bool,
    interactive: bool,
    args: &[String],
    start: &str,
    from: std::io::BufReader<std::os::unix::net::UnixStream>,
    to: std::os::unix::net::UnixStream,
) -> ! {
    if mcp {
        local::pipe(from, to);
        std::process::exit(0);
    }
    let mut r = local::Remote::new(from, to);
    let mut run = |cmd: &str| -> (bool, String, bool) {
        match r.run(cmd) {
            Ok((ok, out, branch, tx)) => {
                if ok && !out.is_empty() {
                    println!("{out}");
                } else if !ok {
                    eprintln!("{}{out}", if import { "chronos: " } else { "error: " });
                }
                (ok, branch, tx)
            }
            Err(e) => {
                eprintln!("chronos: {e}");
                std::process::exit(1)
            }
        }
    };
    if import {
        // the owner answers the hello, then the import: read that second answer
        let (ok, ..) = run("");
        std::process::exit(if ok { 0 } else { 1 });
    }
    if !args.is_empty() {
        let (ok, ..) = run(&args.join(" "));
        std::process::exit(if ok { 0 } else { 1 });
    }
    let (mut ok, mut pending) = (true, String::new());
    if !interactive {
        for line in std::io::stdin().lock().lines() {
            let line = line.unwrap_or_default();
            if pending.is_empty() && is_quit(&line) {
                break;
            }
            if let Some(cmd) = feed(&mut pending, &line) {
                ok &= run(&cmd).0;
            }
        }
        if !pending.is_empty() {
            ok &= run(&pending).0;
        }
        std::process::exit(if ok { 0 } else { 1 });
    }
    println!("chronos: connected through the chronos process that has this folder open (type help)");
    let mut rl = rustyline::DefaultEditor::new().expect("terminal");
    let (mut branch, mut tx) = (start.to_string(), false);
    loop {
        let prompt = format!(
            "chronos:{branch}{}{}> ",
            if tx { " (transaction)" } else { "" },
            if pending.is_empty() { "" } else { "-" }
        );
        match rl.readline(&prompt) {
            Ok(line) if pending.is_empty() && is_quit(&line) => break,
            Ok(line) => {
                if let Some(cmd) = feed(&mut pending, &line) {
                    let _ = rl.add_history_entry(cmd.trim_end());
                    (_, branch, tx) = run(&cmd);
                }
            }
            Err(ReadlineError::Interrupted) => pending.clear(),
            Err(_) => break,
        }
    }
    std::process::exit(0)
}

/// Blocks SIGTERM and SIGINT (Ctrl-C) in this thread and every thread it starts from here on,
/// so they arrive only where `wait_for` asks for them: a server then closes the database cleanly
/// instead of dying with acknowledged writes still in memory.
fn stop_signals() -> libc::sigset_t {
    // SAFETY: plain signal-set calls on a set we own, before any other thread exists
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

fn wait_for(set: libc::sigset_t) {
    let mut sig = 0;
    // SAFETY: waits on a set of signals this process blocked (see `stop_signals`)
    unsafe { libc::sigwait(&set, &mut sig) };
}

/// The database key, from CHRONOS_KEY (64 hex digits) or the file CHRONOS_KEY_FILE names: never a
/// command-line flag, which other users could read in the process list.
fn db_key() -> std::io::Result<Option<[u8; 32]>> {
    let hex = match (std::env::var("CHRONOS_KEY").ok(), std::env::var("CHRONOS_KEY_FILE").ok()) {
        (Some(k), _) if !k.is_empty() => k,
        (_, Some(f)) if !f.is_empty() => {
            std::fs::read_to_string(&f).map_err(|e| std::io::Error::other(format!("CHRONOS_KEY_FILE {f}: {e}")))?
        }
        _ => return Ok(None),
    };
    chronos::crypt::parse_key(&hex).map(Some)
}

fn usage() -> ! {
    eprintln!(
        "usage: chronos [-b branch] [--remote s3://bucket/prefix [--takeover]] [--anchor FILE [--anchor-reset]] <folder> [command]\n       chronos mcp <folder> [--allow-merge | --agent NAME]   (MCP server for AI agents, over stdio; without --allow-merge, agents can't merge; --agent acts as that agent)\n       chronos import <folder> <table> <data.csv> --schema <schema.csv>   (a copy of one Postgres table)\n       chronos attach <new folder> --tables a,b | --schema s [--yes]   (work beside a Postgres: merges into main are written there; CHRONOS_SOURCE_URL)\n       chronos restore <backup> <new folder>   (start a database from a backup, and verify it)\n       chronos keygen   (a new encryption key: CHRONOS_KEY=<key> chronos <new folder> makes an encrypted database)\n       chronos studio <folder> [--port N] [--no-open]   (a web UI for the database, on 127.0.0.1; works beside a running serve)\n       chronos studio <folder> --login [--port N | --listen ADDR:PORT] [--host NAME[:PORT]]... [--tls-cert cert.pem --tls-key key.pem | --insecure-no-tls] [--no-open]\n                                  (the web UI, signed in with a database role and its password, acting with its privileges;\n                                   make one with: create role admin login superuser password '...'; any address but loopback needs TLS)\n       chronos serve <folder> [--listen 127.0.0.1:7070] [--pg 127.0.0.1:5433|off] [--token T] [--tls-cert cert.pem --tls-key key.pem | --insecure-no-tls] [--safe [--admin-token A]]
                                  (HTTP JSON API, and Postgres protocol: psql postgres://127.0.0.1:5433/main; with --safe,
                                   clients that aren't agents only fork and change their own worlds, unless they give the admin token;
                                   a token is 24 characters or more (openssl rand -hex 24; pass it as CHRONOS_TOKEN, not on the
                                   command line, where other users can see it); any address but loopback needs a token and TLS)\n\n{HELP}"
    );
    std::process::exit(2)
}

/// Adds a line to a command being typed; returns the command once it's complete. SQL can span
/// lines and ends with `;`; shell commands are one line.
fn feed(pending: &mut String, line: &str) -> Option<String> {
    if !pending.is_empty() {
        pending.push('\n');
    }
    pending.push_str(line);
    if is_sql(pending) && !pending.trim_end().ends_with(';') {
        return None;
    }
    Some(std::mem::take(pending))
}

fn is_sql(line: &str) -> bool {
    let first =
        line.split(|c: char| c.is_whitespace() || c == ';').find(|w| !w.is_empty()).unwrap_or("").to_lowercase();
    matches!(
        first.as_str(),
        "select"
            | "insert"
            | "update"
            | "delete"
            | "create"
            | "drop"
            | "alter"
            | "begin"
            | "commit"
            | "rollback"
            | "show"
            | "restore"
            | "with"
            | "explain"
            | "values"
            | "savepoint"
            | "release"
            | "set"
            | "simulate"
            // (DO blocks and the other statements a migration or a session writes; `discard` is the shell's own)
            | "do"
            | "call"
            | "grant"
            | "revoke"
            | "comment"
            | "truncate"
            | "lock"
            | "prepare"
            | "execute"
            | "deallocate"
            | "declare"
            | "fetch"
            | "close"
            | "listen"
            | "unlisten"
            | "notify"
            | "reset"
            | "refresh"
            | "reindex"
            | "vacuum"
            | "analyze"
            | "start"
            | "end"
            | "table"
    ) || {
        let second = line.to_lowercase().split_whitespace().nth(1).map(String::from);
        (first == "merge" && second.as_deref() == Some("branch"))
            || (first == "undo" && second.as_deref() == Some("merge"))
            || (matches!(first.as_str(), "backup" | "verify") && second.as_deref() == Some("database"))
            || (matches!(first.as_str(), "fork" | "use" | "switch" | "diff" | "merge" | "replay")
                && second.as_deref() == Some("world"))
    }
}

/// The shortest server or admin token a server starts with.
const MIN_TOKEN: usize = 24;

/// A server's token (or its admin token), if it has one, must be one nobody guesses: at least
/// `MIN_TOKEN` characters, and not the placeholder the examples once used.
fn check_token(what: &str, token: Option<&str>) -> std::io::Result<()> {
    match token {
        Some("change-me") => Err(std::io::Error::other(format!(
            "refusing to start: the {what} is the documented placeholder `change-me`; make one with `openssl rand -hex 24`"
        ))),
        Some(t) if t.chars().count() < MIN_TOKEN => Err(std::io::Error::other(format!(
            "refusing to start: the {what} is shorter than {MIN_TOKEN} characters; make one with `openssl rand -hex 24`"
        ))),
        _ => Ok(()),
    }
}

/// Any address but loopback needs a token, and TLS (`--tls-cert` and `--tls-key`) unless
/// `--insecure-no-tls` says something else protects the connection (a VPN, a TLS proxy in front):
/// every address the name resolves to must be loopback.
fn check_listen(listen: &str, token: Option<&str>, tls: bool, insecure_no_tls: bool) -> std::io::Result<()> {
    if loopback(listen) {
        return Ok(());
    }
    if token.is_none() {
        return Err(std::io::Error::other(format!(
            "refusing to listen on {listen} without a token: pass --token or set CHRONOS_TOKEN"
        )));
    }
    check_tls(listen, tls, insecure_no_tls)
}

/// Whether every address `listen` resolves to is loopback.
fn loopback(listen: &str) -> bool {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = listen.to_socket_addrs().map(Iterator::collect).unwrap_or_default();
    !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback())
}

/// Listening on `listen`, not loopback, needs TLS unless `--insecure-no-tls` (then a warning).
fn check_tls(listen: &str, tls: bool, insecure_no_tls: bool) -> std::io::Result<()> {
    if !tls && !insecure_no_tls {
        return Err(std::io::Error::other(format!(
            "refusing to listen on {listen} without TLS: passwords, tokens and rows would cross the network in the \
             clear. Pass --tls-cert and --tls-key, or --insecure-no-tls if something else protects the connection \
             (a VPN, a TLS proxy in front, a port published only on 127.0.0.1)"
        )));
    }
    if !tls {
        eprintln!(
            "chronos: WARNING: listening on {listen} WITHOUT TLS (--insecure-no-tls): anyone on the network path can read \
             the token, passwords and rows. Use --tls-cert and --tls-key unless something else encrypts the connection."
        );
    }
    Ok(())
}

/// Whether a secret someone sent is `want`, in a time that doesn't tell them how much matched.
pub(crate) fn same_secret(given: &str, want: &str) -> bool {
    let (a, b) = (given.as_bytes(), want.as_bytes());
    a.len() == b.len() && std::hint::black_box(a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y))) == 0
}

fn is_quit(line: &str) -> bool {
    matches!(line.trim(), "quit" | "exit" | ".quit" | "\\q")
}

impl Shell {
    /// Runs one command line and prints the result; false if it failed.
    fn run(&mut self, line: &str) -> bool {
        match self.exec(line) {
            Ok(out) => {
                if !out.is_empty() {
                    println!("{out}");
                }
                true
            }
            Err(e) => {
                eprintln!("error: {e}");
                false
            }
        }
    }

    fn exec(&mut self, line: &str) -> Result<String, String> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("--") {
            return Ok(String::new());
        }
        if is_sql(line) {
            self.sql.branch = self.branch.clone();
            let outs = self.sql.run(&self.db, line, &[]);
            self.branch = self.sql.branch.clone();
            return outs.map(|o| show(&o)).map_err(|e| e.described());
        }
        let (cmd, rest) = word(line);
        if matches!(cmd, "get" | "put" | "del" | "scan" | "ls" | "find" | "describe")
            && chronos::reserved_table(word(rest).0)
        {
            return Err("tables starting with _sonos are the database's own; see SHOW AGENTS, SHOW WORLDS".into());
        }
        if self.sql.status() != b'I' && matches!(cmd, "fork" | "use" | "merge" | "discard") {
            return Err("a transaction is open: commit; or rollback; first".into());
        }
        // inside a transaction, reads and writes go to it
        let b = if self.sql.status() == b'I' { self.branch.clone() } else { self.sql.target().to_string() };
        match cmd {
            "help" | "?" => Ok(HELP.into()),
            "get" => {
                let (t, id) = table_id(rest, "get <table> <id>")?;
                match self.db.get(&b, &key(&self.db, &b, t, id)).map_err(msg)? {
                    Some(v) => Ok(pretty(&v)),
                    None => Err(format!("no row {t}/{id} on {b}; see: scan {t}")),
                }
            }
            "put" => {
                let (t, rest) = word(rest);
                let (id, json) = word(rest);
                if t.is_empty() || id.is_empty() || json.is_empty() {
                    return Err("usage: put <table> <id> <json>, e.g. put users 1 {\"name\": \"Ada\"}".into());
                }
                check_table(t)?;
                let v: Value = serde_json::from_str(json)
                    .map_err(|e| format!("invalid JSON ({e}); e.g. put {t} {id} {{\"name\": \"Ada\"}}"))?;
                self.db.put(&b, &key(&self.db, &b, t, id), &v.to_string()).map_err(msg)?;
                Ok(format!("put {t}/{id}"))
            }
            "del" => {
                let (t, id) = table_id(rest, "del <table> <id>")?;
                let k = key(&self.db, &b, t, id);
                if self.db.get(&b, &k).map_err(msg)?.is_none() {
                    return Err(format!("no row {k} on {b}"));
                }
                self.db.del(&b, &k).map_err(msg)?;
                Ok(format!("deleted {k}"))
            }
            "scan" | "ls" => {
                let (t, n) = word(rest);
                if t.is_empty() {
                    return Err("usage: scan <table> [n]; see: tables".into());
                }
                let n = if n.is_empty() { PREVIEW } else { n.parse().map_err(|_| format!("'{n}' is not a number"))? };
                let rows = self.db.prefix(&b, &format!("{t}/")).map_err(msg)?;
                Ok(listing(&rows, n, &plural(rows.len(), "row"), &format!("scan {t} {}", rows.len())))
            }
            "find" => self.find(rest),
            "tables" => {
                let t = self.tables()?;
                if t.is_empty() {
                    return Ok(format!("no tables on {b}; add one with: put users 1 {{\"name\": \"Ada\"}}"));
                }
                Ok(t.iter().map(|(t, n)| format!("{t}  {}", plural(*n, "row"))).collect::<Vec<_>>().join("\n"))
            }
            "describe" => self.describe(rest, &|_| false),
            "fork" => {
                let (name, rest) = word(rest);
                let from = match word(rest) {
                    ("from", f) if !f.is_empty() => f.to_string(),
                    ("", _) => b,
                    _ => return Err("usage: fork <name> [from <branch>]".into()),
                };
                if name.is_empty() {
                    return Err("usage: fork <name> [from <branch>]".into());
                }
                self.db.fork(&from, name).map_err(msg)?;
                self.branch = name.into();
                Ok(format!("forked {from} -> {name}; now on {name}"))
            }
            "use" => {
                if !self.db.branches().iter().any(|(n, _)| n == rest) {
                    return Err(format!("no branch '{rest}'; see: branches"));
                }
                self.branch = rest.into();
                Ok(String::new())
            }
            "branches" | "worlds" => Ok(self
                .db
                .branches()
                .iter()
                .filter(|(n, _)| !n.starts_with("_tx_")) // sessions' transactions: private to them
                .map(|(n, p)| {
                    let mark = if *n == b { "*" } else { " " };
                    let from = p
                        .as_ref()
                        .map(|p| format!("  (from {p}, {})", plural(self.changes(n), "change")))
                        .unwrap_or_default();
                    format!("{mark} {n}{from}")
                })
                .collect::<Vec<_>>()
                .join("\n")),
            "diff" => {
                let target = if rest.is_empty() { b } else { rest.to_string() };
                let version = self.db.version(&target).map_err(msg)?; // first: merging at it merges no more than shown
                let mut d = self.db.diff(&target).map_err(msg)?;
                d.sort_by(|a, b| key_order(&a.key).cmp(&key_order(&b.key)));
                if d.is_empty() {
                    return Ok(format!("no changes on {target}"));
                }
                let lines: Vec<String> = d
                    .iter()
                    .map(|c| match (&c.before, &c.after) {
                        (None, Some(a)) => format!("+ {}  {a}", show_key(&c.key)),
                        (Some(_), None) => format!("- {}", show_key(&c.key)),
                        (Some(x), Some(y)) => format!("~ {}  {}", show_key(&c.key), field_diff(x, y)),
                        (None, None) => unreachable!(),
                    })
                    .collect();
                Ok(format!(
                    "{} on {target}\n{}\n(at version {version}: merge {target} at {version} merges exactly this)",
                    plural(d.len(), "change"),
                    lines.join("\n")
                ))
            }
            "undo" => {
                // undo [world]: the latest merge into this branch, or of `world`, row for row
                let (w, extra) = word(rest);
                if !extra.is_empty() {
                    return Err("usage: undo [world]".into());
                }
                let latest = w.is_empty();
                let w = if latest { self.last_merged()? } else { w.to_string() };
                let u = self.db.undo_merge(&w, false).map_err(|e| {
                    let hint = if latest {
                        format!(" (undo takes the latest merge, {w}; if you already undid it, name an earlier one: undo <world>)")
                    } else {
                        String::new()
                    };
                    msg(e) + &hint
                })?;
                Ok(format!("undid the merge of {w} into {}: put back {}", u.parent, plural(u.rows, "row")))
            }
            "merge" => {
                let mut target = b;
                let mut how = Resolve::Fail;
                let (mut confirm, mut at) = (false, None);
                let mut words = rest.split_whitespace();
                while let Some(a) = words.next() {
                    match a {
                        "ours" => how = Resolve::Ours,
                        "theirs" => how = Resolve::Theirs,
                        "confirm" => confirm = true,
                        // at <version>: only if the world is still as it was reviewed (see `diff`)
                        "at" => {
                            let v = words.next().unwrap_or_default();
                            at = Some(v.parse::<u64>().map_err(|_| format!("'at' takes a version number, not '{v}'"))?);
                        }
                        _ => target = a.into(),
                    }
                }
                let parent = self.parent(&target)?;
                let expected = match (at, confirm) {
                    (Some(v), _) => Some(v),
                    (None, true) => Some(self.db.version(&target).map_err(msg)?),
                    (None, false) => None,
                };
                match self.db.merge_at(&target, how, expected) {
                    Ok(n) => {
                        if self.branch == target {
                            self.branch = parent.clone();
                        }
                        Ok(format!(
                            "merged {} from {target} into {parent}; now on {}",
                            plural(n, "change"),
                            self.branch
                        ))
                    }
                    Err(Error::Conflicts(cs)) => {
                        let rows: Vec<String> = cs
                            .iter()
                            .map(|c| {
                                let s = |v: &Option<chronos::Val>| v.as_deref().unwrap_or("(deleted)").to_string();
                                format!(
                                    "  {}\n    base:   {}\n    ours:   {}\n    theirs: {}",
                                    show_key(&c.key),
                                    s(&c.base),
                                    s(&c.ours),
                                    s(&c.theirs)
                                )
                            })
                            .collect();
                        Err(format!(
                            "{}: both {target} and {parent} changed these rows; nothing was merged\n{}\n\
                             resolve with: merge {target} ours    (keep {target}'s values)\n\
                             \x20        or: merge {target} theirs  (keep {parent}'s values)",
                            plural(cs.len(), "conflict"),
                            rows.join("\n")
                        ))
                    }
                    Err(e) => Err(msg(e)),
                }
            }
            "discard" => {
                let (rest, cascade) = match rest.strip_suffix("cascade") {
                    Some(r) if r.is_empty() || r.ends_with(' ') => (r.trim(), true),
                    _ => (rest, false),
                };
                let target = if rest.is_empty() { b } else { rest.to_string() };
                self.discard(target, cascade)
            }
            "gc" => {
                let (pages, freed) = self.db.gc().map_err(|e| e.to_string())?;
                Ok(format!("dropped {} unused pages, freed {:.1} MB", pages, freed as f64 / 1e6))
            }
            "checkpoint" => {
                self.db.checkpoint().map_err(|e| e.to_string())?;
                Ok("checkpoint written".into())
            }
            "backup" if !rest.is_empty() => {
                let b = self.db.backup(rest).map_err(|e| e.to_string())?;
                Ok(format!("backed up to {rest}: {} files, {:.1} MB", b.files, b.bytes as f64 / 1e6))
            }
            "migrate" if !rest.is_empty() => {
                let ms = self.db.migrate(&self.branch, rest).map_err(|e| e.message)?;
                let new: Vec<&str> = ms.iter().filter(|m| m.applied).map(|m| m.file.as_str()).collect();
                Ok(match new.len() {
                    0 => format!("up to date: {} applied before", ms.len()),
                    _ => format!("applied {}", new.join(", ")),
                })
            }
            "verify" => {
                let v = self.db.verify().map_err(|e| e.to_string())?;
                Ok(format!(
                    "ok: {} pages ({:.1} MB) and {} log records read and checked",
                    v.pages,
                    v.bytes as f64 / 1e6,
                    v.log_records
                ))
            }
            _ => Err(format!("unknown command '{cmd}'; type help")),
        }
    }

    /// SQL run on its own (MCP: no session carries over between calls) as `agent`, if one: each statement is checked
    /// as the Postgres port checks it.
    pub(crate) fn sql_as(
        &self,
        q: &str,
        params: &[chronos::sql::Value],
        agent: Option<&str>,
    ) -> Result<String, String> {
        let mut s = Closing(chronos::sql::Session::new(&self.branch), &self.db);
        s.0.agent = agent.map(String::from);
        s.0.run(&self.db, q, params).map(|o| show(&o)).map_err(|e| e.described())
    }

    /// Discards `target` (with its forks, if `cascade`); taken structurally by MCP, whose branch
    /// names could read as the shell's `cascade` keyword.
    pub(crate) fn discard(&mut self, target: String, cascade: bool) -> Result<String, String> {
        let parent = self.parent(&target)?;
        let also = if cascade {
            let n = self.db.discard_tree(&target).map_err(msg)?;
            format!(" and its {}", plural(n - 1, "fork"))
        } else {
            self.db.discard(&target).map_err(msg)?;
            String::new()
        };
        if self.db.world(&self.branch).is_err() {
            self.branch = parent;
        }
        Ok(format!("discarded {target}{also}; now on {}", self.branch))
    }

    fn parent(&self, b: &str) -> Result<String, String> {
        match self.db.branches().into_iter().find(|(n, _)| n == b) {
            Some((_, Some(p))) => Ok(p),
            Some((_, None)) => Err(msg(Error::IsMain)),
            None => Err(msg(Error::NoBranch(b.into()))),
        }
    }

    /// The world merged into this branch most recently, from its history.
    fn last_merged(&self) -> Result<String, String> {
        let events = self.db.history(&self.branch, 1000).map_err(msg)?;
        events
            .iter()
            .find_map(|e| e.what.strip_prefix("merged ").filter(|w| !w.starts_with("into ")))
            .map(String::from)
            .ok_or_else(|| format!("nothing was merged into {} recently; name the world: undo <world>", self.branch))
    }

    fn changes(&self, b: &str) -> usize {
        self.db.diff(b).map(|d| d.len()).unwrap_or(0)
    }

    /// Table names with row counts. Skip-scans from one table to the next, then counts each.
    // ponytail: counting is O(rows); keep per-table counts if describe on big tables gets slow.
    fn tables(&self) -> Result<Vec<(String, usize)>, String> {
        let mut out = vec![];
        let mut from = String::new();
        while let Some((k, _)) = self.db.scan(&self.branch, &from, 1).map_err(msg)?.pop() {
            let t = k.split('/').next().unwrap_or(&k).to_string();
            let n = self.db.prefix_len(&self.branch, &format!("{t}/")).map_err(msg)?;
            from = format!("{t}0"); // '0' sorts right after '/', so this skips the whole table
            if !t.starts_with("_sonos") {
                out.push((t, n)); // SQL's own bookkeeping (schemas, sequences, constraint indexes) stays out of sight
            }
        }
        Ok(out)
    }

    /// find <table> [where f = v [and ...]] [match <words>] [near <field> <json array>] [limit n]
    fn find(&self, rest: &str) -> Result<String, String> {
        const USAGE: &str = "usage: find <table> [where <field> = <value> [and ...]] [match <words>] \
                             [near <field> [numbers]] [limit n]";
        let (t, rest) = word(rest);
        if t.is_empty() {
            return Err(USAGE.into());
        }
        let mut clauses: Vec<(&str, Vec<&str>)> = vec![];
        for w in rest.split_whitespace() {
            match w {
                "where" | "match" | "near" | "limit" | "offset" => clauses.push((w, vec![])),
                _ => clauses.last_mut().ok_or(USAGE)?.1.push(w),
            }
        }
        let mut q = chronos::Query { table: t.into(), k: PREVIEW, ..Default::default() };
        for (kw, words) in clauses {
            let text = words.join(" ");
            match kw {
                "where" => {
                    for c in text.split(" and ") {
                        let (f, v) = c.split_once('=').ok_or(USAGE)?;
                        let v = v.trim();
                        q.filters.push((
                            f.trim().into(),
                            serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.into())),
                        ));
                    }
                }
                "match" if !text.is_empty() => q.text = Some(text),
                "near" => {
                    let (field, v) = word(&text);
                    let v: Vec<f32> = serde_json::from_str(v).map_err(|_| {
                        format!(
                            "near needs a field and a JSON array of numbers, e.g. near embedding [0.1, 0.2]; {USAGE}"
                        )
                    })?;
                    q.vector = Some((field.into(), v));
                }
                "limit" => q.k = text.parse().map_err(|_| format!("'{text}' is not a number"))?,
                "offset" => q.offset = text.parse().map_err(|_| format!("'{text}' is not a number"))?,
                _ => return Err(USAGE.into()),
            }
        }
        self.run_find(&q)
    }

    /// Runs a search on the current branch: the match count, then the hits (with scores when ranked).
    fn run_find(&self, q: &chronos::Query) -> Result<String, String> {
        check_table(&q.table)?;
        let (total, hits) = self.db.find(&self.branch, q).map_err(msg)?;
        let ranked = q.text.is_some() || q.vector.is_some();
        let mut out = vec![plural(total, "match")];
        out.extend(hits.iter().map(|h| match ranked {
            true => format!("  {}  {:.3}  {}", show_key(&h.key), h.score, h.row),
            false => format!("  {}  {}", show_key(&h.key), h.row),
        }));
        if !ranked && total > hits.len() {
            out.push(format!("  ... {} more (narrow the filter or raise the limit)", total - hits.len()));
        }
        Ok(out.join("\n"))
    }

    /// Each table's fields, read from its first 1000 rows, and a sample row; a table `limited` says yes to (one a
    /// row-level security policy limits an agent on) shows its row count and no rows.
    fn describe(&self, table: &str, limited: &dyn Fn(&str) -> bool) -> Result<String, String> {
        let tables = self.tables()?;
        if tables.is_empty() {
            return Ok(format!("no tables on {}", self.branch));
        }
        let mut out = vec![];
        for (t, n) in tables.iter().filter(|(t, _)| table.is_empty() || t == table) {
            out.push(format!("{t} ({})", plural(*n, "row")));
            if limited(t) {
                out.push("  (row-level security limits what you see of it: read it with sql)".into());
                continue;
            }
            // (the first rows only: a big table isn't read whole)
            let p = format!("{t}/");
            let mut rows = self.db.scan(&self.branch, &p, 1000).map_err(msg)?;
            rows.retain(|(k, _)| k.starts_with(&p));
            let mut fields: BTreeMap<String, (String, Vec<String>, usize)> = BTreeMap::new();
            for (_, v) in rows.iter().take(1000) {
                if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(v) {
                    for (f, v) in o {
                        let e = fields.entry(f).or_insert_with(|| (kind(&v).into(), vec![], 0));
                        e.2 += 1;
                        let s = v.to_string();
                        if e.1.len() <= 5 && !e.1.contains(&s) {
                            e.1.push(s);
                        }
                    }
                }
            }
            let width = fields.keys().map(|f| f.len()).max().unwrap_or(0);
            for (f, (k, vals, seen)) in &fields {
                // few distinct values that repeat look like an enum: show them all
                let hint = if vals.len() <= 5 && *seen > vals.len() {
                    format!("one of: {}", vals.join(", "))
                } else {
                    format!("e.g. {}", vals[0])
                };
                out.push(format!("  {f:width$}  {k:6}  {hint}"));
            }
            if let Some((k, v)) = rows.first() {
                out.push(format!("  sample: {k} {v}"));
            }
        }
        if out.is_empty() {
            return Err(format!("no table '{table}'; see: tables"));
        }
        Ok(out.join("\n"))
    }
}

/// SQL results as psql shows them: a table for rows, else the command tag.
fn show(outs: &[chronos::sql::Output]) -> String {
    let parts: Vec<String> = outs
        .iter()
        .map(|o| match &o.columns {
            Some(cols) => grid(cols, &o.rows),
            None => o.tag.clone(),
        })
        .collect();
    parts.join("\n")
}

/// Rows as a psql-style table.
fn grid(cols: &[(String, chronos::sql::Type)], rows: &[Vec<chronos::sql::Value>]) -> String {
    let cells: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect()).collect();
    let width =
        |i: usize| cells.iter().map(|r| r[i].chars().count()).chain([cols[i].0.chars().count()]).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols.len()).map(width).collect();
    let line = |vals: Vec<&str>| {
        let padded: Vec<String> = vals.iter().zip(&widths).map(|(v, w)| format!(" {v:<w$} ")).collect();
        padded.join("|").trim_end().to_string()
    };
    let mut out = vec![line(cols.iter().map(|c| c.0.as_str()).collect())];
    out.push(widths.iter().map(|w| "-".repeat(w + 2)).collect::<Vec<_>>().join("+"));
    out.extend(cells.iter().map(|r| line(r.iter().map(String::as_str).collect())));
    out.push(format!("({})", plural(rows.len(), "row")));
    out.join("\n")
}

/// How to list row keys for people: by table, then ids that are numbers by value
/// (users/2 before users/10), then the rest as text.
fn key_order(k: &str) -> (&str, Option<u128>, &str) {
    let (table, id) = k.split_once('/').unwrap_or((k, ""));
    (table, id.parse().ok(), id)
}

fn plural(n: usize, one: &str) -> String {
    match (n, one.ends_with("ch")) {
        (1, _) => format!("1 {one}"),
        (_, true) => format!("{n} {one}es"),
        _ => format!("{n} {one}s"),
    }
}

fn word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

fn table_id<'a>(rest: &'a str, usage: &str) -> Result<(&'a str, &'a str), String> {
    let (t, id) = word(rest);
    if t.is_empty() || id.is_empty() || id.contains(char::is_whitespace) {
        return Err(format!("usage: {usage}"));
    }
    check_table(t)?;
    Ok((t, id))
}

fn check_table(t: &str) -> Result<(), String> {
    if t.contains('/') {
        return Err(format!("table names can't contain '/': {t}"));
    }
    Ok(())
}

/// A row's key: `t/id`, with an id typed as `(1,abc)` for a key of several columns.
fn key(db: &Db, b: &str, t: &str, id: &str) -> String {
    chronos::sql::row_key_of(db, b, &format!("{t}/{id}"))
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn pretty(v: &str) -> String {
    serde_json::from_str::<Value>(v).and_then(|v| serde_json::to_string_pretty(&v)).unwrap_or_else(|_| v.into())
}

/// Count first, then a preview, then how to see more.
fn listing(rows: &[(String, chronos::Val)], n: usize, head: &str, more: &str) -> String {
    let mut out = vec![head.to_string()];
    out.extend(rows.iter().take(n).map(|(k, v)| format!("  {}  {v}", show_key(k))));
    if rows.len() > n {
        out.push(format!("  ... {} more ({more})", rows.len() - n));
    }
    out.join("\n")
}

/// `name: "Ada" -> "Ada L."` for changed fields of two JSON objects, else the whole values.
fn field_diff(before: &str, after: &str) -> String {
    let obj = |s: &str| -> Option<Map<String, Value>> {
        match serde_json::from_str(s) {
            Ok(Value::Object(o)) => Some(o),
            _ => None,
        }
    };
    let (Some(a), Some(b)) = (obj(before), obj(after)) else {
        return format!("{before} -> {after}");
    };
    let mut parts = vec![];
    for (f, v) in &a {
        match b.get(f) {
            None => parts.push(format!("-{f}")),
            Some(w) if w != v => parts.push(format!("{f}: {v} -> {w}")),
            _ => {}
        }
    }
    parts.extend(b.iter().filter(|(f, _)| !a.contains_key(*f)).map(|(f, w)| format!("+{f}: {w}")));
    parts.join(", ")
}

fn msg(e: Error) -> String {
    match e {
        Error::NoBranch(b) => format!("no branch '{b}'; see: branches, or create it with: fork {b}"),
        Error::Exists(b) => format!("branch '{b}' already exists; pick another name"),
        Error::HasChildren(b) => {
            format!(
                "branch '{b}' has forks of its own; merge or discard them first (see: branches), or: discard {b} cascade"
            )
        }
        Error::IsMain => "main is the trunk; it has no parent to merge into or be discarded from".into(),
        Error::Stale { branch, expected: None, .. } => format!(
            "branch '{branch}' was open when the database crashed, so its last writes may be lost; \
             check: diff {branch}, then: merge {branch} confirm (or: discard {branch})"
        ),
        e @ Error::Stale { .. } => e.to_string(),
        e @ (Error::Conflicts(_) | Error::Constraint { .. }) => e.to_string(),
        Error::Io(e) => format!("storage error: {e}; the database folder may be damaged"),
    }
}
