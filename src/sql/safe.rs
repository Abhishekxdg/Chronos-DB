//! What a statement can change, so a caller can refuse some before running them (the MCP
//! server's safe mode: agents change only their own worlds, and a person merges).

use super::{Agg, CtxFn, Expr, Func, ListAgg, SRFS, Statement, Stmt, WinFn, expand_stmt, functions, userdefs};
use crate::{Db, View};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Changes nothing.
    Read,
    /// Changes rows or schema of the branch it runs on, and nothing else.
    Write,
    /// Changes other worlds or the system: fork, merge, restore, drop, switch, metadata, retention.
    World,
}

impl Statement {
    pub fn access(&self) -> Reach {
        match access(&self.0) {
            // a function it calls may write
            Reach::Read if self.2 => Reach::Write,
            r => r,
        }
    }

    /// `access` in world `branch`, with the views it reads expanded: a view can call setval,
    /// nextval, pg_notify or a function made with CREATE FUNCTION, which the statement's own
    /// text doesn't show.
    pub fn access_in(&self, db: &Db, branch: &str) -> Reach {
        match self.access() {
            Reach::Read if self.expanded(db, branch).is_some_and(|st| calls(&st) || side_effects(&st)) => Reach::Write,
            r => r,
        }
    }

    /// Whether it can move a sequence's counter, which every world shares (so safe mode refuses
    /// it): setval, in the statement or a view it reads in world `branch`, ALTER SEQUENCE ...
    /// RESTART, and TRUNCATE ... RESTART IDENTITY.
    pub fn moves_counters(&self, db: &Db, branch: &str) -> bool {
        match &self.0 {
            Stmt::AlterSequence { opts, .. } => opts.restart.is_some(),
            Stmt::Truncate { restart, .. } => *restart,
            s => {
                let st = self.expanded(db, branch).unwrap_or_else(|| s.clone());
                finds(&st, |e| matches!(e, Expr::Func(Func::Ctx(CtxFn::SetVal), _)))
            }
        }
    }

    /// The statement with the views it reads in world `branch` expanded, if it reads any. None
    /// too when the world or a view can't be read: running it fails the same way, before it
    /// calls anything.
    pub(super) fn expanded(&self, db: &Db, branch: &str) -> Option<Stmt> {
        let maybe = db.has_views() || branch.contains('@'); // a world in the past may hold views
        // (no views to find, no row security to put in: only a read of a row's place (ctid) could change it, which
        // `expand_stmt` finds from the statement alone: not reading the world then is what it would say)
        if !maybe && !self.touches(self.reads_ctid(), false) {
            return None;
        }
        match db.view_tx(branch).map(|v| expand_stmt(&v, &self.0, None, maybe, false)) {
            Ok(Ok(Some((st, true)))) => Some(st),
            _ => None,
        }
    }
}

/// Whether a statement calls a function made with CREATE FUNCTION, in an expression or FROM.
pub(super) fn calls(s: &Stmt) -> bool {
    let found = std::cell::Cell::new(false);
    let _ = functions::visit(
        &mut s.clone(),
        &mut |e| {
            found.set(found.get() || matches!(e, Expr::Call(_)));
            Ok(())
        },
        &mut |q| {
            let mut srcs = q.from.iter().chain(q.joins.iter().map(|j| &j.src));
            let user = srcs.any(|s| s.srf.as_ref().is_some_and(|f| !SRFS.contains(&f.0.as_str())));
            found.set(found.get() || user);
        },
    );
    found.get()
}

/// Whether an expression of statement `s` is one `f` looks for.
fn finds(s: &Stmt, f: impl Fn(&Expr) -> bool) -> bool {
    let mut found = false;
    let _ = functions::visit(
        &mut s.clone(),
        &mut |e| {
            found |= f(e);
            Ok(())
        },
        &mut |_| {},
    );
    found
}

/// Whether a query calls a built-in function that changes something: a sequence (nextval,
/// setval), other sessions (pg_notify), or a large object (lo_create, lo_put, lo_unlink, ...); or one that runs a
/// query given as text (ts_stat, ts_rewrite, crosstab, query_to_xml), which may call what does.
pub(super) fn side_effects(s: &Stmt) -> bool {
    let found = Cell::new(false);
    let _ = functions::visit(
        &mut s.clone(),
        &mut |e| {
            let changes = match e {
                Expr::Func(Func::Ctx(CtxFn::NextVal | CtxFn::SetVal | CtxFn::Notify | CtxFn::TsRewrite), _) => true,
                Expr::Func(Func::Ctx(CtxFn::LargeObject(n)), _) => {
                    super::largeobj::writes(n) || n.starts_with("query_to_xml")
                }
                _ => false,
            };
            found.set(found.get() || changes);
            Ok(())
        },
        &mut |q| {
            let mut srfs = q.from.iter().chain(q.joins.iter().map(|j| &j.src)).filter_map(|s| s.srf.as_deref());
            found.set(found.get() || srfs.any(|(name, _)| srf_names(name).any(runs_text)));
        },
    );
    found.get()
}

/// Whether built-in set-returning function `name` runs a query given as text (ts_stat's; crosstab's).
fn runs_text(name: &str) -> bool {
    matches!(name, "ts_stat" | "crosstab")
}

/// The functions a FROM function's stored name names: `ROWS FROM`'s each (see `ROWS_FROM`), without the column
/// definition list that rides on a name (see `COLDEFS`).
fn srf_names(name: &str) -> impl Iterator<Item = &str> {
    let parts = match super::rows_from_parts(name) {
        Some(p) => p.into_iter().map(|(n, _)| n).collect(),
        None => vec![name],
    };
    parts.into_iter().map(|n| n.split_once(super::COLDEFS).map_or(n, |(bare, _)| bare))
}

/// Whether running statement `s` could change anything at all, which EXPLAIN without ANALYZE must not (Postgres's
/// runs nothing: see `Exec::explain_run`), as far as the statement shows. Any statement but a SELECT could; a SELECT
/// could by a built-in that changes something (see `CtxFn::changes`: a sequence, a notification, a large object, an
/// advisory lock, a setting, ...), setseed, pg_stat_statements_reset, ts_stat, crosstab or ts_rewrite's two-argument
/// form (which run a query given as text), FOR UPDATE (or SHARE) in a subquery or a WITH query, which locks the rows
/// it reads, or a function made with CREATE FUNCTION, an operator's or an aggregate's (CREATE OPERATOR's, CREATE
/// AGGREGATE's) that could. With `v`, the world it would run in, a function counts only if its statements could: one
/// in SQL (LANGUAGE internal's is SQL: see `userdefs::internal_sql`); without, every one does. What isn't seen here
/// (a PL/pgSQL function's body, a domain's CHECK, a function of CREATE CAST) `sql::dry_run` stops as it runs.
pub(super) fn effects(s: &Stmt, v: Option<&View>) -> bool {
    effects_in(s, v, &Seen::default())
}

/// What `effects` has looked at already, or is looking at (a function that calls itself, an operator whose function
/// compares with the operator again): a call's name, with whether it may mean the built-in, and a function's
/// signature. Looked at again it adds nothing, as what it could change is counted where it was first.
type Seen = std::cell::RefCell<std::collections::HashSet<String>>;

/// `effects`, with what was looked at already (see `Seen`).
fn effects_in(s: &Stmt, v: Option<&View>, seen: &Seen) -> bool {
    if !matches!(s, Stmt::Select(_)) {
        return true;
    }
    let user = |name: &str, builtin: bool| v.is_none_or(|v| call_changes(v, name, builtin, seen));
    let agg = |a: &Agg| match a {
        Agg::List(ListAgg::User(i), _) => userdefs::aggregate_at(*i)
            .is_none_or(|d| std::iter::once(&d.sfunc).chain(&d.finalfunc).any(|f| user(&super::func_named(f), false))),
        _ => false,
    };
    let found = Cell::new(false);
    let _ = functions::visit(
        &mut s.clone(),
        &mut |e| {
            let changes = match e {
                Expr::Func(Func::Ctx(CtxFn::TsRewrite), a) => a.len() == 2,
                Expr::Func(f, _) => func_changes(f),
                Expr::Call(c) => match &c.def {
                    Some(f) => v.is_none_or(|v| function_changes(v, f, seen)),
                    None => user(&c.name, c.builtin),
                },
                Expr::Agg(a, ..) => agg(a),
                Expr::Window(w) => matches!(&w.func, WinFn::Agg(a) if agg(a)),
                _ => false,
            };
            found.set(found.get() || changes);
            Ok(())
        },
        &mut |q| {
            let mut srfs = q.from.iter().chain(q.joins.iter().map(|j| &j.src)).filter_map(|s| s.srf.as_deref());
            let changes = !q.locking.is_empty()
                || srfs.any(|(name, _)| {
                    srf_names(name).any(|n| if SRFS.contains(&n) { runs_text(n) } else { user(n, false) })
                });
            found.set(found.get() || changes);
        },
    );
    found.get()
}

/// Whether a call of built-in `f` changes something besides giving its value (see `CtxFn::changes`; setseed and
/// pg_stat_statements_reset too).
fn func_changes(f: &Func) -> bool {
    use super::funcs::Scalar::{SetSeed, StatReset};
    match f {
        Func::Ctx(c) => c.changes(),
        Func::Scalar(SetSeed | StatReset) => true,
        _ => false,
    }
}

/// Whether a call of function `name` could change something in world `v` (see `effects`): the functions of the name
/// made with CREATE FUNCTION, and else (or with `builtin`, where the call's arguments may choose it over them: see
/// `Call::builtin`) the built-in it names, by its name or by its C symbol (an operator's procedure, `int4lt`); for a
/// call that several operators of one symbol share, any of their functions. A name of no function there is changes
/// nothing: the call fails (and an operator's that isn't there, a rolled-back CREATE OPERATOR's, which the registry
/// keeps, is never made).
fn call_changes(v: &View, name: &str, builtin: bool, seen: &Seen) -> bool {
    let first = seen.borrow_mut().insert(format!("{builtin} {name}"));
    if !first {
        return false;
    }
    if let Some(funcs) = userdefs::op_funcs(name) {
        return funcs.iter().any(|f| call_changes(v, &super::func_named(f), false, seen));
    }
    let Ok(made) = functions::named(v, name) else { return true };
    let bare = super::rel_part(super::written(name));
    let built_in = || match super::builtin_of(bare) {
        Some((f, _)) => func_changes(&f),
        // (what it does, as SQL)
        None => userdefs::internal_sql(bare).is_some_and(|sql| {
            super::check_expr(&sql)
                .map_or(true, |e| effects_in(&Stmt::Select(Box::new(super::select_of(e))), Some(v), seen))
        }),
    };
    made.iter().any(|f| function_changes(v, f, seen)) || ((made.is_empty() || builtin) && built_in())
}

/// Whether running function `f`, made with CREATE FUNCTION, could change something by its statements (see
/// `effects`): a SQL function's. A PL/pgSQL function's body isn't looked at: what it would change, `sql::dry_run`
/// stops as it runs.
fn function_changes(v: &View, f: &functions::Function, seen: &Seen) -> bool {
    let first = seen.borrow_mut().insert(f.described());
    first && f.statements().iter().any(|st| effects_in(st, Some(v), seen))
}

/// What statement `s` itself can change, apart from the functions it calls (see `Statement::access`).
pub(super) fn reach(s: &Stmt) -> Reach {
    access(s)
}

fn access(s: &Stmt) -> Reach {
    match s {
        Stmt::Select(_) if side_effects(s) => Reach::Write,
        // EXPLAIN without ANALYZE runs nothing that changes anything (see `effects`), but EXPLAIN EXECUTE works out its
        // arguments, as Postgres's does
        Stmt::Explain { analyze: false, stmt, .. } if matches!(**stmt, Stmt::Execute { .. }) && side_effects(s) => {
            Reach::Write
        }
        Stmt::Locking(..) => Reach::Write, // it locks rows it may then change
        // REINDEX writes what it rebuilds and moves (see `maintain::rebuild`): the world it runs on, a row of its own
        // per table (`tablespace::FILENODES`) that pg_class.relfilenode and pg_relation_filenode show, which a merge
        // carries. (A READ ONLY transaction still runs it, as Postgres's does: see `sql::read_only_refusal`.)
        Stmt::Maintain(m) if m.verb == super::maintain::Verb::Reindex => Reach::Write,
        Stmt::Select(_)
        | Stmt::Set
        | Stmt::SetReadOnly { .. }
        | Stmt::ShowBranches
        | Stmt::ShowWorlds
        | Stmt::ShowHistory { .. }
        | Stmt::ShowRetention
        | Stmt::ShowSync
        | Stmt::ShowSystem(_)
        | Stmt::ShowDisk
        | Stmt::ShowStorage(_)
        | Stmt::SetEf(_) // session settings: change nothing else (an agent's max_query_ms caps the timeout)
        | Stmt::SetTimeout(_)
        | Stmt::SetZone(..)
        | Stmt::SetVar { .. }
        | Stmt::ShowVar(_)
        | Stmt::SetPath(_)
        | Stmt::SetRole(..) // who the session is: what it may do is checked as each statement runs
        | Stmt::SetAuth(..)
        | Stmt::SetSnapshot(_) // (it writes only its own transaction's fork, which COMMIT drops)
        | Stmt::SetConstraints { .. }
        | Stmt::ShowPath
        | Stmt::ShowTimeout
        | Stmt::ShowZone
        | Stmt::ShowMetrics
        | Stmt::ShowAgents
        | Stmt::ShowAgentPrivileges { .. }
        | Stmt::ShowPolicies
        | Stmt::ShowChecks
        | Stmt::ShowReviews
        | Stmt::CheckReads(_)
        | Stmt::ShowMergeOutcomes { .. }
        | Stmt::ShowSecurityAudit { .. }
        | Stmt::ShowOutbox
        | Stmt::ShowAudit { .. }
        | Stmt::Diff(..)
        | Stmt::Readers(..)
        | Stmt::ShowReaders
        | Stmt::MergeBranch { dry: true, .. }
        | Stmt::Explain { analyze: false, .. }
        | Stmt::Deallocate(_)
        | Stmt::Listen(_)
        | Stmt::Unlisten(_)
        // maintenance that changes nothing, session resets, and a lock only other lockers wait for
        | Stmt::Maintain(_)
        | Stmt::Discard(_)
        | Stmt::Lock(_)
        | Stmt::Noop(_) // CREATE and DROP EXTENSION of what's built in
        | Stmt::NoopNotice(..)
        | Stmt::NoopNotices(..) => Reach::Read,
        Stmt::Explain { stmt, .. } => access(stmt), // ANALYZE runs it for real
        Stmt::Prepare { stmt, .. } => access(stmt),
        // a cursor's rows are its query's: what FETCH reads was checked when it was declared
        Stmt::Declare { query, .. } => access(query),
        Stmt::Fetch { .. } | Stmt::CloseCursor(_) => Reach::Read,
        // TO STDOUT reads, unless its query is an INSERT, UPDATE or DELETE ... RETURNING; TO a file writes (the
        // server's file system, as Postgres's read-only rules count it)
        Stmt::Copy(c) if !c.from && c.file.is_none() => c.dml.as_deref().map_or(Reach::Read, access),
        Stmt::Insert { .. }
        | Stmt::Update { .. }
        | Stmt::Delete { .. }
        | Stmt::With(_) // data-changing queries first, then what reads their rows
        | Stmt::Merge(_)
        | Stmt::RenameView { .. }
        | Stmt::ViewOptions { .. }
        | Stmt::ViewDefault { .. }
        | Stmt::Comment(_) // a row of the world
        | Stmt::Create { .. }
        | Stmt::CreateAs { .. } // (reads what its query reads too)
        | Stmt::Drop { .. }
        | Stmt::CreateIndex { .. }
        | Stmt::DropIndex { .. }
        | Stmt::Alter { .. }
        | Stmt::Do(_) // (what a block or a procedure writes is that of a function called from a query)
        | Stmt::Call(_)
        | Stmt::CreateFunction(..)
        | Stmt::DropFunction { .. }
        | Stmt::CreateTrigger(..)
        | Stmt::DropTrigger { .. }
        | Stmt::RenameTrigger { .. }
        | Stmt::RenameRule { .. }
        | Stmt::CreateView { .. }
        | Stmt::DropObjects { .. }
        | Stmt::Refresh { .. }
        | Stmt::CreateSequence { .. }
        | Stmt::AlterSequence { .. }
        | Stmt::Truncate { .. }
        | Stmt::Begin
        | Stmt::BeginRead(_)
        | Stmt::Commit
        | Stmt::Rollback
        | Stmt::Savepoint(_)
        | Stmt::Release(_)
        | Stmt::RollbackTo(_)
        | Stmt::PrepareTransaction
        | Stmt::Copy(_)
        | Stmt::CreateType { .. }
        | Stmt::DropType { .. }
        | Stmt::CreateSchema { .. }
        | Stmt::CreateExtension { .. } // a row of the world, like a schema
        | Stmt::AlterExtension { .. }
        | Stmt::DropExtension { .. }
        | Stmt::RegisterReader { .. } // a definition in the world, like a view
        | Stmt::UnregisterReader { .. }
        | Stmt::MarkReader { .. }
        | Stmt::DropSchema { .. }
        | Stmt::AlterType { .. }
        | Stmt::CreateCollation { .. }
        | Stmt::DropCollation { .. }
        | Stmt::AlterCollation { .. }
        | Stmt::Notify(..) // other sessions hear it
        | Stmt::NotifyOnMerge(..) // an entry in the world
        | Stmt::Grant(_) // rows of the world (roles themselves, `Stmt::Role`, are the database's)
        | Stmt::Foreign(_) // wrappers, servers and mappings: rows of the world
        | Stmt::AlterOwner(..)
        | Stmt::AlterRoutine { .. }
        | Stmt::DropOwned(..)
        | Stmt::Reassign(..)
        | Stmt::Policy(_) // rows of the world, as a table's schema is
        | Stmt::Execute { .. } => Reach::Write, // EXECUTE: whatever was prepared, which its PREPARE was checked for
        _ => Reach::World, // and any statement added later, until it's classified here
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use super::Reach::*;

    #[test]
    fn classifies_statements() {
        let one = |sql: &str| parse(sql).unwrap()[0].access();
        for sql in [
            "select 1",
            "show branches",
            "show history for world 'a' limit 3",
            "diff world 'a' to 'main@-1 hour'",
            "diff world a readers",
            "explain select * from t",
            "explain select nextval('s'), pg_notify('c', 'x'), lo_create(1)", // (not run: see `effects`)
            "explain delete from t",
            "explain execute p(1)",
            "merge world a dry run",
            "show agents",
            "show merge checks",
            "show readers",
            "show audit for agent bot limit 5",
            "show metrics",
            "show disk",
            "show world_idle_ttl",
            "set statement_timeout = '1h'",
            "vacuum",
            "analyze t",
            "cluster t using t_pkey",
            "discard all",
            "lock table t in share mode",
            "create extension if not exists \"uuid-ossp\"",
            "create extension if not exists vector",
            "drop extension if exists pg_trgm",
            "copy t to stdout",
            "copy (select * from t) to stdout",
        ] {
            assert_eq!(one(sql), Read, "{sql}");
        }
        for sql in [
            "insert into t values (1)",
            "comment on table t is 'x'",               // a row of the world
            "reindex table t",                         // so is its new relfilenode (Postgres's changes pg_class too)
            "create extension if not exists pgcrypto", // so is pgcrypto's record, for pg_extension
            "drop extension if exists pgcrypto",
            "update t set a = 1",
            "delete from t",
            "copy (delete from t returning id) to stdout",
            "copy (insert into t values (1) returning id) to stdout",
            "create table t (id int primary key)",
            "begin",
            "explain analyze delete from t",
            "explain execute p(nextval('s'))", // (its arguments are worked out)
            "explain execute p(my_function(1))",
            "select my_function(1)",
            "select * from my_rows(2)",
            "create function f() returns int language sql as 'select 1'",
            "create trigger t after insert on t for each row execute function f()",
            "drop function f",
            "drop trigger t on t",
            "select setval('orders_id_seq', 1)",
            "select nextval('s')",
            "select 1 where exists (select pg_notify('c', 'x'))",
            "select distinct on (lo_unlink(1)) * from t",
            "select * from ts_stat('delete from t returning to_tsvector(a)')", // (what a query given as text calls)
            "select * from crosstab('select 1, 2, my_function(3)') as ct(a int, b int)",
            "select query_to_xml('select 1', true, false, '')",
            "notify c",
            "do $$ begin null; end $$",
            "call p()",
            "register reader sync on t (a)",
            "unregister reader sync",
            "mark reader billing critical",
            "unmark reader billing",
        ] {
            assert_eq!(one(sql), Write, "{sql}");
        }
        for sql in [
            "merge world a",
            "restore world main to '-1 minute'",
            "drop world a",
            "use world main",
            "fork world main as b",
            "alter system set history_retention = '1 day'",
            "alter system set synchronous_commit = off",
            "alter system set statement_timeout = '5s'",
            "alter system set max_worlds = 10",
            "alter system set world_idle_ttl = '7 days'",
            "alter world a set pinned",
            "drop world a cascade",
            "create database d",
            "drop database d",
            "create agent bot with (can = 'read')",
            "alter agent bot set (max_worlds = 3)",
            "drop agent bot",
            "create merge check c as select 1",
            "drop merge check c",
            "checkpoint world a as 'before'",
            "restore world a to checkpoint 'before'",
            "undo merge of world a",
            "backup database to '/tmp/b'",
            "verify database",
            "simulate 3 worlds as t run 'select 1' score 'select 1'",
            "replay world t_0",
        ] {
            assert_eq!(one(sql), World, "{sql}");
        }
    }

    #[test]
    fn finds_what_running_a_query_could_change() {
        // (with no world to look in, any function made with CREATE FUNCTION counts; random() is run, its seeded
        // numbers kept as they were: see `sql::dry_run`)
        let changes = |sql: &str| super::effects(&parse(sql).unwrap()[0].0, None);
        for sql in [
            "select 1",
            "select * from t where a > 1 order by b",
            "select currval('s'), now(), random()",
            "select distinct on (a) * from t",
            "select * from t, generate_series(1, 2) g, jsonb_to_recordset('[]') as j (id text)",
            "select lo_get(1)", // (a large object's function that only reads)
            "select ts_rewrite('a'::tsquery, 'a'::tsquery, 'b'::tsquery)", // (no query of its own)
        ] {
            assert!(!changes(sql), "{sql}");
        }
        for sql in [
            "select nextval('s')",
            "select * from t where setval('s', 1) > 0",
            "select (select pg_notify('c', 'x'))",
            "select lo_unlink(1)",
            "select lo_open(1, 262144)", // (a descriptor of the session's)
            "select pg_stat_statements_reset()",
            "select ts_rewrite('a'::tsquery, 'select ''a''::tsquery, ''b''::tsquery')", // (its query, given as text)
            "select pg_advisory_lock(1)",
            "select set_config('work_mem', '1MB', false)",
            "select setseed(0.5)",
            "select pg_terminate_backend(1)",
            "select my_function(1)",
            "select * from my_rows(2)",
            "select * from t for update",
            "select * from (select * from t for share) x",
            "select distinct on (nextval('s')) * from t",
            "select * from t where a in (select distinct on (lo_unlink(1)) a from t)",
            "select * from ts_stat('select to_tsvector(a) from t')", // (its query, given as text)
            "select * from crosstab('select 1, 2, 3') as ct(a int, b int)",
            "select * from t, rows from (generate_series(1, 2), my_rows(2)) g",
            "with d as (delete from t returning *) select * from d",
            "update t set a = 1",
            "insert into t values (1)",
        ] {
            assert!(changes(sql), "{sql}");
        }
    }
}
