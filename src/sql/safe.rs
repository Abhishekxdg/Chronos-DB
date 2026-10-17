//! What a statement can change, so a caller can refuse some before running them (the MCP
//! server's safe mode: agents change only their own worlds, and a person merges).

use super::{CtxFn, Expr, Func, SRFS, Statement, Stmt, expand_stmt, functions};
use crate::Db;

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
/// setval), other sessions (pg_notify), or a large object (lo_create, lo_put, lo_unlink, ...).
pub(super) fn side_effects(s: &Stmt) -> bool {
    finds(s, |e| match e {
        Expr::Func(Func::Ctx(CtxFn::NextVal | CtxFn::SetVal | CtxFn::Notify), _) => true,
        Expr::Func(Func::Ctx(CtxFn::LargeObject(n)), _) => super::largeobj::writes(n),
        _ => false,
    })
}

/// What statement `s` itself can change, apart from the functions it calls (see `Statement::access`).
pub(super) fn reach(s: &Stmt) -> Reach {
    access(s)
}

fn access(s: &Stmt) -> Reach {
    match s {
        Stmt::Select(_) if side_effects(s) => Reach::Write,
        Stmt::Locking(..) => Reach::Write, // it locks rows it may then change
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
        | Stmt::ShowPolicies
        | Stmt::ShowChecks
        | Stmt::ShowReviews
        | Stmt::CheckReads(_)
        | Stmt::ShowMergeOutcomes { .. }
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
        | Stmt::SetDefiner(..)
        | Stmt::RoutineConfig(..)
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
            "reindex table t",
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
            "create extension if not exists pgcrypto", // so is pgcrypto's record, for pg_extension
            "drop extension if exists pgcrypto",
            "update t set a = 1",
            "delete from t",
            "copy (delete from t returning id) to stdout",
            "copy (insert into t values (1) returning id) to stdout",
            "create table t (id int primary key)",
            "begin",
            "explain analyze delete from t",
            "select my_function(1)",
            "select * from my_rows(2)",
            "create function f() returns int language sql as 'select 1'",
            "create trigger t after insert on t for each row execute function f()",
            "drop function f",
            "drop trigger t on t",
            "select setval('orders_id_seq', 1)",
            "select nextval('s')",
            "select 1 where exists (select pg_notify('c', 'x'))",
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
}
