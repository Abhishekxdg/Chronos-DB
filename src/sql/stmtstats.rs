//! pg_stat_statements: what each statement took, by its text with its constants taken out (`select * from t
//! where id = $1`), as the extension keeps it: how many times it ran, how long it took in all and at its least,
//! most and on average, and the rows it gave or changed. Kept in memory by the process, from when it started or
//! `pg_stat_statements_reset()`, apart for each database server (a `Core`), and in it by user and world (database),
//! as the extension keys them (userid, dbid, queryid); what Postgres keeps of buffers, WAL, planning and JIT is 0
//! here.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use super::{Tok, Value, lex, tokens_shown};

/// The most statements kept, as `pg_stat_statements.max` (5000): past it, the ones run least go.
const MAX: usize = 5000;

#[derive(Clone, Default)]
pub(super) struct Stat {
    pub calls: u64,
    pub total_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    /// for the standard deviation (Welford's running mean and sum of squares)
    pub mean_ms: f64,
    m2: f64,
    pub rows: u64,
}

/// What an entry is kept by, as the extension's `pgssHashKey`: the database server it ran in (its
/// `Core`'s uid, never reused in the process: each keeps its own, as each Postgres cluster does), who
/// ran the statement (its role's oid, as Postgres keys it, so a role renamed keeps its entries; and, for
/// the bootstrap superuser's oid only, the name it ran as: logins and agents that are no role all run as
/// it, and are told apart by name), the database (world) it ran in, and the statement's identity.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    core: u64,
    userid: i64,
    user: Arc<str>,
    world: Arc<str>,
    query: Shape,
}

/// A statement's identity: its text with constants taken out, and the kinds of those constants
/// (`SELECT 1` and `SELECT 'x'` are two statements to Postgres, whose query id comes from the
/// analyzed tree, though both are shown as `SELECT $1`).
#[derive(Clone, PartialEq, Eq, Hash)]
struct Shape {
    text: Arc<str>,
    kinds: Arc<str>,
}

impl Shape {
    /// Its query id: FNV-1a of its text and its constants' kinds, as a bigint.
    fn id(&self) -> i64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.text.bytes().chain([0]).chain(self.kinds.bytes()) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h as i64
    }
}

struct Stats {
    by_query: HashMap<Key, Stat>,
    /// a statement's text as it came, to its shape (shared with `by_query`'s keys, so counting a
    /// statement that ran before copies no text)
    seen: HashMap<Arc<str>, Option<Shape>>,
}

static STATS: LazyLock<Mutex<Stats>> =
    LazyLock::new(|| Mutex::new(Stats { by_query: HashMap::new(), seen: HashMap::new() }));

/// The query identifier pg_stat_statements keeps statement `sql` by (see `Shape::id`); None for text that doesn't lex.
pub(super) fn query_id(sql: &str) -> Option<i64> {
    normalized(sql).map(|s| s.id())
}

/// `sql` with each constant (a number, a quoted string) made a parameter, `$1`, `$2`, ..., after the ones it has,
/// and the kinds of the constants taken out (`i` int4, `b` int8, `n` numeric, `s` a string). None for text that
/// doesn't lex.
fn normalized(sql: &str) -> Option<Shape> {
    let mut toks = lex(sql).ok()?;
    let mut n = toks.iter().filter_map(|t| if let Tok::Param(k) = t { Some(*k) } else { None }).max().unwrap_or(0);
    let mut kinds = String::new();
    for t in &mut toks {
        let kind = match t {
            Tok::Str(_) => 's',
            Tok::Num(v) if v.parse::<i32>().is_ok() => 'i',
            Tok::Num(v) if v.parse::<i64>().is_ok() => 'b',
            Tok::Num(_) => 'n',
            _ => continue,
        };
        kinds.push(kind);
        n += 1;
        *t = Tok::Param(n);
    }
    while toks.last() == Some(&Tok::Sym(";")) {
        toks.pop();
    }
    (!toks.is_empty()).then(|| Shape { text: tokens_shown(&toks).into(), kinds: kinds.into() })
}

/// Who ran a statement and where: the database server (its `Core`'s address), the role's oid, the
/// name it ran as, and the world (database).
pub(super) struct Ran<'a> {
    pub core: u64,
    pub userid: i64,
    pub user: &'a str,
    pub world: &'a str,
}

/// Counts a statement that ran: who ran it where, its text (a simple query's, or a prepared statement's), how long
/// it took, and the rows it gave or changed. The shape is worked out once per text (kept in `seen`), and a
/// statement that ran before is found by that and counted with one more lookup.
pub(super) fn record(by: &Ran, sql: &Arc<str>, ms: f64, rows: u64) {
    let mut s = STATS.lock().unwrap_or_else(|p| p.into_inner());
    let shape = match s.seen.get(sql) {
        Some(k) => k.clone(),
        None => {
            let k = normalized(sql);
            if s.seen.len() >= MAX * 4 {
                s.seen.clear();
            }
            s.seen.insert(sql.clone(), k.clone());
            k
        }
    };
    let Some(query) = shape else { return };
    // (a role is its oid; only the bootstrap superuser's oid, which every login that is no role shares, keeps the name)
    let user: Arc<str> = if by.userid == BOOTSTRAP { by.user.into() } else { Arc::from("") };
    let key = Key { core: by.core, userid: by.userid, user, world: by.world.into(), query };
    // (one lookup for a statement that ran before)
    if let Some(st) = s.by_query.get_mut(&key) {
        st.add(ms, rows);
        return;
    }
    // (MAX for each database server: one never pushes out another's; counted only once the whole map is that big)
    if s.by_query.len() >= MAX && s.by_query.keys().filter(|q| q.core == by.core).count() >= MAX {
        // (as the extension deallocates: the ones run least, a tenth of them)
        let mut calls: Vec<(u64, Key)> =
            (s.by_query.iter()).filter(|(q, _)| q.core == by.core).map(|(q, st)| (st.calls, q.clone())).collect();
        calls.sort_by_key(|c| c.0);
        for (_, q) in calls.into_iter().take(MAX / 10) {
            s.by_query.remove(&q);
        }
    }
    s.by_query.entry(key).or_default().add(ms, rows);
}

impl Stat {
    /// One more run of the statement: how long it took and the rows it gave or changed.
    fn add(&mut self, ms: f64, rows: u64) {
        self.calls += 1;
        self.total_ms += ms;
        self.min_ms = if self.calls == 1 { ms } else { self.min_ms.min(ms) };
        self.max_ms = self.max_ms.max(ms);
        let delta = ms - self.mean_ms;
        self.mean_ms += delta / self.calls as f64;
        self.m2 += delta * (ms - self.mean_ms);
        self.rows += rows;
    }
}

/// One entry as the view shows it.
pub(super) struct Entry {
    pub userid: i64,
    pub world: String,
    /// None for a caller that may not see it (the view's `<insufficient privilege>`, and no query id)
    pub query: Option<(String, i64)>,
    pub stat: Stat,
    /// of the population, as the extension's
    pub stddev: f64,
}

/// Whether the running statement may see every role's statements, as the extension's
/// `is_allowed_role`: a superuser or a role with pg_read_all_stats's privileges (outside a session's
/// statement: the library, a superuser). An agent's session never is.
fn sees_all() -> bool {
    !super::acl::agents() && super::roles::current().is_none_or(|w| w.has_privs_of(READ_ALL_STATS))
}

/// `pg_read_all_stats`.
const READ_ALL_STATS: i64 = 3375;

/// The bootstrap superuser's oid, which logins and agents that are no role run as.
const BOOTSTRAP: i64 = 10;

/// The database server the running statement is in (its `Core`'s uid); None outside a session's
/// statement (the library's own reads: every server's).
fn core_now() -> Option<u64> {
    super::STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref()).map(|c| c.core.uid))
}

/// Forgets the statements of a database server that closed (see `crate::Core`'s Drop), so nothing of
/// it stays in the process.
pub(super) fn forget(core: u64) {
    STATS.lock().unwrap_or_else(|p| p.into_inner()).by_query.retain(|k, _| k.core != core);
}

/// Forgets the statements run in worlds of database server `core` that are gone.
pub(super) fn forget_worlds(core: u64, worlds: &[&str]) {
    let gone: std::collections::HashSet<&str> = worlds.iter().copied().collect();
    (STATS.lock().unwrap_or_else(|p| p.into_inner()).by_query)
        .retain(|k, _| k.core != core || !gone.contains(&*k.world));
}

/// Every statement kept in the running statement's database server, with its statistics, ordered by
/// text. Another role's statement's text and id are hidden from a caller that may not see all (see
/// `sees_all`), as the extension hides them.
pub(super) fn all() -> Vec<Entry> {
    let all = sees_all();
    let me = super::roles::current();
    let core = core_now();
    let s = STATS.lock().unwrap_or_else(|p| p.into_inner());
    let mut out: Vec<(Arc<str>, Entry)> = (s.by_query.iter())
        .filter(|(k, _)| core.is_none_or(|c| c == k.core))
        .map(|(k, st)| {
            let own = me.as_ref().is_some_and(|w| w.oid == k.userid && (k.userid != BOOTSTRAP || *w.name == *k.user));
            let entry = Entry {
                userid: k.userid,
                world: k.world.to_string(),
                query: (all || own).then(|| (k.query.text.to_string(), k.query.id())),
                stat: st.clone(),
                stddev: if st.calls > 0 { (st.m2 / st.calls as f64).sqrt() } else { 0.0 },
            };
            (k.query.text.clone(), entry)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.into_iter().map(|(_, e)| e).collect()
}

/// pg_stat_statements_reset(userid, dbid, queryid): forgets the statements that match each argument
/// not 0 (all of them when all are 0); `dbs` gives the worlds' oids. Postgres revokes it from
/// PUBLIC: a superuser's here (never an agent's). Strict: a null argument does nothing. Only the
/// running statement's database server's statements go.
pub(super) fn reset(a: &[Value], dbs: impl FnOnce() -> Vec<(String, i64)>) -> super::Result<Value> {
    if super::acl::agents() || super::roles::checked().is_some() {
        return super::err("42501", "permission denied for function pg_stat_statements_reset");
    }
    let mut want = [0i64; 3];
    for (w, v) in want.iter_mut().zip(a) {
        *w = match v {
            Value::Null => return Ok(Value::Null),
            // (a regrole's oid, `'alice'::regrole`)
            Value::Int(i) | Value::Reg(i, _) => *i,
            v => match v.to_text().trim().parse() {
                Ok(i) => i,
                Err(_) => {
                    let e = format!("invalid input syntax for type bigint: \"{}\"", v.to_text());
                    return super::err("22P02", e);
                }
            },
        };
    }
    let [userid, dbid, queryid] = want;
    let dbs = if dbid != 0 { dbs() } else { vec![] };
    let core = core_now();
    let mut s = STATS.lock().unwrap_or_else(|p| p.into_inner());
    s.by_query.retain(|k, _| {
        let hit = core.is_none_or(|c| c == k.core)
            && (userid == 0 || k.userid == userid)
            && (dbid == 0 || dbs.iter().any(|(w, o)| *o == dbid && **w == *k.world))
            && (queryid == 0 || k.query.id() == queryid);
        !hit
    });
    Ok(Value::Null)
}

/// The worlds' database oids, as pg_database numbers them (see `catalog`): by when they were made,
/// from 16384. The running statement's database server's, or none outside one.
pub(super) fn world_oids() -> Vec<(String, i64)> {
    let Some(core) = super::STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| c.core.clone())))
    else {
        return vec![];
    };
    let mut worlds = core.worlds();
    worlds.retain(|w| !w.name.starts_with("_tx_"));
    worlds.sort_by(|a, b| (a.created, &a.name).cmp(&(b.created, &b.name)));
    worlds.into_iter().enumerate().map(|(i, w)| (w.name, 16384 + i as i64)).collect()
}

/// The rows a statement's output says it gave or changed: a SELECT's rows, the count its tag ends with.
pub(super) fn rows_of(tag: &str, rows: usize) -> u64 {
    match tag.rsplit(' ').next().and_then(|n| n.parse::<u64>().ok()) {
        Some(n) if !tag.starts_with("SELECT") => n,
        _ => rows as u64,
    }
}
