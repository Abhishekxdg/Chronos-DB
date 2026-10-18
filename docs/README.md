---
title: Chronos DB documentation
slug: /
---

> **Beta.** Chronos 0.2 is a public beta for developers; see [where it stands](postgres-compatibility.md#where-it-stands-beta).

Chronos DB gives every AI agent its own **world**: a copy-on-write branch of the whole database, made in about 2 microseconds. Agents change their world freely; you look at what changed and keep what works.

**Fork, experiment, diff, merge.**

```sql
-- psql postgres://127.0.0.1:5433/main
create world agent_7 with (owner = 'claude');
switch world agent_7;
update prices set amount = amount * 0.9 where sku like 'SUMMER-%';
diff world agent_7;              -- what the agent changed, row by row
merge world agent_7 dry run;     -- what a merge would do, and any conflicts
merge world agent_7;             -- or: drop world agent_7
```

It speaks the Postgres protocol, so `psql`, drivers and ORMs connect as they do to Postgres, and it searches filters, text and vectors inside every world.

## Start here

- [Quickstart](quickstart.md): install, start beside the Postgres you already run (`chronos attach`), or start a server, connect with psql and from an application, then the shell, TypeScript, Python, Claude Code, Rust.
- [Migrating from Postgres](migrating-from-postgres.md): `chronos import` or a `pg_dump` replay, checking row counts and checksums, pointing an ORM at it, the differences to plan for.
- [Concepts](concepts.md): worlds, merging and conflicts, time travel, durability.
- [Chronos Cloud](cloud.md): `chronos init` in a project folder: a hosted database, wired up for your agent (private alpha).

## Guides

- [Working beside your Postgres](guides/attach-postgres.md): `chronos attach`: agents work on a copy, merges are written to your Postgres in one checked transaction.
- [Agent sandbox](guides/agent-sandbox.md): give several agents their own worlds, compare them, merge the best.
- [Agent evaluation](guides/agent-evaluation.md): grade an agent by diffing its world against the expected state.
- [Scenario planning](guides/scenario-planning.md): simulate a thousand futures, score them in SQL, merge one.
- [Merge policies](guides/merge-policies.md): let agents merge on their own within rules you set; review only what breaks them.
- [Merge checks](guides/merge-checks.md): SQL rules every merge's result must keep, so two changes that are fine alone but break a rule together are caught.

## Reference

- [SQL](sql.md): what SQL works now and what doesn't yet.
- [Worlds](reference/worlds.md): every world statement: fork, diff, merge, time travel, undo, simulate.
- [Search](search.md): filters, full text, vectors, fusion.
- [HTTP API](http-api.md), [MCP](reference/mcp.md), [Clients](reference/clients.md), [CLI and settings](reference/cli.md), [Errors](reference/errors.md).

## Running it

- [Postgres compatibility](postgres-compatibility.md): where the beta stands, as measured; tested drivers and ORMs, protocol features, known gaps.
- [Security and auth](security.md): tokens, agents, safe mode, limits, and what not to expose yet.
- [What Chronos protects](what-chronos-protects.md): what it guards for agents on production data, what an agent calling other systems directly gets past, and where backups must live.
- [Operations](operations.md): the folder, checkpoints, crashes, cloud storage, backups, memory.
- [Benchmarks](https://github.com/Abhishekxdg/chronosdb/blob/main/BENCHMARKS.md): every result with its method, losses included.
