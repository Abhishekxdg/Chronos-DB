---
title: What Chronos guarantees today
---

Chronos is a **public beta**, built by one person, with no outside production use yet. This page
states plainly what it does and does not guarantee, so you can judge it for your own use. It links
to the pages that back each claim. Where a number is a benchmark, it is an engineering measurement
on one machine, not a published result; the losses are listed beside the wins.

## Status

- **Beta.** Try it on a staging copy, not production. The on-disk format is versioned and read
  across releases, but a release has changed authentication semantics before, so read the
  [CHANGELOG](../CHANGELOG.md) when you upgrade.
- **One maintainer.** The storage engine has not had an outside review. Treat it accordingly.
- **Platforms:** macOS 13+ and Linux (x86-64-v2 and aarch64). No Windows.

## Durability — what survives a crash

| Write | On a `kill -9` or power loss |
|---|---|
| `main` writes and merges into `main` | **Durable** once acknowledged (`synchronous_commit = full`, the default; `normal` on macOS stops at the drive cache, `off` syncs nothing) |
| Writes to other worlds (branches) | **Can be lost**, never silently — the world is flagged on reopen and its merge needs the version its last write returned ([concepts](concepts.md#versions-and-crashes)) |

Chronos's crash, disk-full and kill tests re-open the folder with every acknowledged write and pass
`VERIFY DATABASE` ([operations](operations.md#crashes)). What is *not* yet tested: real power-loss
and fault-injected-storage tooling (LazyFS/dm-log-writes) — the kill tests keep the OS page cache,
so they don't prove the fsyncs themselves are correctly placed. A `main` write becomes visible to
other sessions when it enters the shared snapshot, which is before its fsync returns; a power loss
in that window can erase a write another session already read.

## Recovery — backups, point-in-time, failover

- **Backups** are **full copies** of the folder, consistent as of the moment they start; there are
  no incremental backups. Keep them where the database's machine and its agents can't reach
  ([what it protects](what-chronos-protects.md#backups-keep-them-out-of-the-agents-reach)).
- **Point-in-time** travel and `RESTORE` reach back only within the **history-retention window**
  (1 hour for a new database; set `history_retention` higher). There is no restore of a backup to
  an arbitrary timestamp.
- **High availability:** none built in — one process writes one database, nothing fails over on its
  own. The recovery-point objective on losing the machine depends on storage ([operations](operations.md#high-availability)):

  | Setup | Data loss on losing the machine (RPO) |
  |---|---|
  | One machine, backups only | as old as the last backup |
  | Cloud storage, `wal_shipping = off` | everything since the last checkpoint |
  | Cloud storage, `wal_shipping = async` (default) | at most ~`wal_shipping_delay` (1 s) |
  | Cloud storage, `wal_shipping = sync` | none acknowledged |

  No hot standby, no automatic failover, no read replicas.

## PostgreSQL compatibility — close, not identical

Chronos speaks the Postgres 16 wire protocol and much of its SQL, and mainstream drivers are
tested, but **it is not Postgres**. See the [compatibility page](postgres-compatibility.md) for the
measured score (and how it is counted — the headline line-match number is inflated by echoed
statements, so the per-statement figure is the one to use) and the full matrix. The differences
most likely to surprise you ([Differences that bite](postgres-compatibility.md#differences-that-bite)):

- a multi-statement simple `Query` is **not** one implicit transaction;
- some unique/foreign-key conflicts surface at **COMMIT**, not at the statement;
- `LOCK TABLE` serializes other `LOCK TABLE`s but does **not** block ordinary writers (it warns);
- a `GIN` index on `jsonb` is **accepted but finds nothing** — every query scans (index an
  expression instead);
- an equality filter on a column with no B-tree index builds an in-memory index ~85× the table;
- `SERIALIZABLE` and two-phase commit are refused; there is no replication, `pg_hba.conf`, or
  tablespaces;
- the world is connection state (a hazard behind a pooler without a reset query).

## Security — the data inside, not the world outside

Chronos controls what agents do to the data **inside** it: agents write in worlds, merges go
through the rules you approve, and any agent's changes can be undone. It cannot see or undo what an
agent does **outside** the database (cloud APIs, files, other services), and its guarantees hold
only while whoever acts is held to them — keep admin tokens away from agents and turn on
`protect_main`. The full scope, and the known gaps, are in
[what it protects, and what it doesn't](what-chronos-protects.md) and
[security](security.md). Encryption at rest is available; there is no key rotation yet, and the
audit trail records privileged and agent actions but not logins.

## Performance — measured, with the losses

The benchmarks are engineering numbers on one machine, with every rival run on the same machine and
the losses listed ([BENCHMARKS](../BENCHMARKS.md)). Fork is O(1) and microsecond-scale; durable
single-row commit is at parity with Postgres for a single client; vector search beats pgvector at
matched recall over SQL. Chronos is slower than DuckDB on in-process analytics, merges are slower
than Postgres under a thousand concurrent agents, and several "versus" rows compare an in-process
library call to a networked server — each such row says so. Measure your own workload.

## In one line

A beta that genuinely branches, diffs and merges a database and gates agent writes with rules —
honest about being pre-production, Postgres-compatible-not-Postgres, and built by one person.
