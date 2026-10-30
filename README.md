<div align="center">

# Chronos DB

### Let AI agents write to your Postgres. Each run works on its own branch; your rules decide what lands.

![release](https://img.shields.io/badge/release-v0.4.4%20beta-f59e0b?style=flat-square)
![postgres wire](https://img.shields.io/badge/postgres-wire%20protocol-336791?style=flat-square&logo=postgresql&logoColor=white)
![mcp](https://img.shields.io/badge/MCP-ready-7c3aed?style=flat-square)
![rust](https://img.shields.io/badge/rust-1.90%2B-b7410e?style=flat-square&logo=rust&logoColor=white)
![license](https://img.shields.io/badge/license-BUSL--1.1%20engine%20%2B%20MIT-64748b?style=flat-square)

[**Docs site**](https://chronosdb-docs.vercel.app/) ·
[**Quickstart**](docs/quickstart.md) ·
[**Concepts**](docs/concepts.md) ·
[**SQL**](docs/sql.md) ·
[**Benchmarks**](BENCHMARKS.md) ·
[**Changelog**](CHANGELOG.md) ·
[**Releases**](https://github.com/Abhishekxdg/chronosdb/releases)

</div>

Chronos DB is a database for teams that want AI agents to change real data without a person reading every change. Each agent run gets its own **world**: a private, copy-on-write copy of the whole database, made in about 2 microseconds at any size. When the run is done, Chronos shows exactly which rows it changed, and merges it only if the change keeps to rules you set once; anything else waits for a person.

It speaks the Postgres wire protocol, so `psql`, drivers and ORMs connect to it. It can hold your data itself, or sit **beside the Postgres you already run** and write each approved merge back to it in one checked transaction.

<p align="center"><img src="docs/assets/attach-demo.gif" alt="An agent cleans up a CRM in its own world beside a real Postgres: the diff, a merge policy holding back its deletes, Postgres unchanged until a person approves, then the change written to Postgres" width="820"></p>

> [!IMPORTANT]
> **Public beta, version 0.4.4.** Try it on a staging copy first, not production. It is built by one person and has no outside production use yet. Chronos is not Postgres: it runs much of Postgres 16's SQL, not all of it. Read [Where it stands](#where-it-stands) before you move data in.

## In one example

The whole loop, in SQL, on a `leads` table you already have:

```sql
-- once, as you: the rules an agent's merges must keep
create merge policy agents with (max_deletes = 0, max_rows = 500, review_tables = 'payments');
create agent bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'agents');
-- the agent's token is in the `token` column, shown only now

-- as the agent:  psql "postgres://bot:<token>@127.0.0.1:5433/main"
create world run_42 with (task = 'clean up stale leads');   -- its own copy of everything
switch world run_42;
update leads set status = 'stale' where last_contact < now() - interval '1 year';
delete from leads where email is null;

diff world run_42;                   -- table, id, change, before, after
merge world run_42 dry run;          -- what the merge would do, row by row, and why it would be held
merge world run_42;                  -- it deletes rows, so the policy holds it: queued in SHOW REVIEWS

-- as you: look, then approve or throw it away
show reviews;
merge world run_42;                  -- approve
-- or: drop world run_42;            -- reject: main never saw it
```

If a merge keeps to every rule (no deletes, at most 500 rows, nothing in `payments`), it goes through on its own. If not, it is refused with the reasons and waits for you.

## Contents

- [Install](#install)
- [Quickstart: five minutes](#quickstart-five-minutes)
- [How it works](#how-it-works)
- [What it is good at](#what-it-is-good-at)
- [Where it stands](#where-it-stands)
- [Clients and integrations](#clients-and-integrations)
- [Benchmarks](#benchmarks)
- [Documentation](#documentation)
- [Feedback and contributing](#feedback-and-contributing)
- [What's in this repository](#whats-in-this-repository)
- [License](#license)

## Install

```bash
curl -fsSL https://github.com/Abhishekxdg/chronosdb/releases/latest/download/install.sh | sh
chronos --version
```

The script picks the build for your machine, checks it against the release's SHA-256 sums, and puts `chronos` in `~/.local/bin`. It never runs anything as root.

| Platform | Build |
|---|---|
| Linux, x86-64 and arm64 | static (musl), runs on any distribution; x86-64 needs an x86-64-v2 CPU (about 2009 or later) |
| macOS 13 or later, Apple silicon and Intel | native |
| Windows | not supported |

Options, set as environment variables for the script:

| Variable | What it does |
|---|---|
| `CHRONOS_INSTALL_DIR=<folder>` | installs into another folder you can write (default `~/.local/bin`) |
| `CHRONOS_VERSION=v0.4.4` | installs a given release tag instead of the latest |
| `CHRONOS_VERIFY=1` | also checks the build's signature with [cosign](https://docs.sigstore.dev/cosign/system_config/installation/) (keyless, Sigstore) |

If `~/.local/bin` isn't on your `PATH`, the script says so; add it with `export PATH="$HOME/.local/bin:$PATH"`. The examples below use `psql`: `brew install libpq` on macOS, `sudo apt-get install postgresql-client` on Debian or Ubuntu.

**No telemetry.** Chronos connects only to what you point it at: a Postgres you import from or attach to, a storage bucket you give it, or Chronos Cloud when you run `chronos init` or `chronos cloud`.

## Quickstart: five minutes

Pick one: work beside a Postgres you already run, or let Chronos hold the data.

### A. Beside your Postgres (`chronos attach`)

**Attach** means: Chronos copies the tables an agent will work on into a local folder, agents work in worlds of that copy, and each merge into `main` is written to your Postgres first. Your application keeps using Postgres as before.

```bash
export CHRONOS_SOURCE_URL='postgres://agent_writer:...@staging-db:5432/crm?sslmode=verify-full'
chronos attach ./crm --tables public.leads,public.accounts   # or: --schema app
chronos serve ./crm                                          # Postgres protocol on 127.0.0.1:5433
```

The connection URL is read from the environment (or a file only you can read), never from the command line, and is not written into the folder. The role needs to be allowed to write those tables. Attach makes one table in your Postgres, `chronos.merges`, to record each merge it writes.

In another terminal:

```sql
-- psql postgres://127.0.0.1:5433/main
create world fix_leads;
use world fix_leads;           -- the agent's whole task happens in its world
update leads set owner = 'ada' where region = 'EU';
delete from leads where email is null;
diff world fix_leads;          -- what it changed, row by row
merge world fix_leads rehearse;  -- run the write-back in Postgres, then roll it back
merge world fix_leads;         -- written to Postgres in one transaction, then here
```

At `merge world`, Chronos runs its own checks, then opens one Postgres transaction. It locks each row the world changed and compares it with what Chronos last saw. If any row changed in Postgres meanwhile, nothing is written and the merge returns those rows as conflicts. Otherwise it applies the changes, runs your checks on the result, and commits. Values your Postgres triggers or defaults set come back into Chronos.

Requirements and limits are under [Where it stands](#where-it-stands). The full guide, including refresh, undo and a restore drill: [Working beside your Postgres](docs/guides/attach-postgres.md).

### B. On its own (`chronos serve`)

```bash
chronos serve mydb               # makes the folder mydb if it's new
```

This listens on 127.0.0.1 only: the Postgres protocol on port 5433 (not 5432, so it doesn't collide with a Postgres you run) and the HTTP API on 7070. On loopback without a token, no password is asked. In another terminal:

```sql
-- psql postgres://127.0.0.1:5433/main
create table todos (id bigserial primary key, title text not null, done boolean not null default false);
create world first;              -- main's rows change only by merging a world
use world first;
insert into todos (title) values ('try chronos'), ('read the docs');
update todos set done = true where id = 1;
diff world first;                -- what it changed
use world main;
merge world first;
select * from todos order by id;
```

**`main` is protected** in a database `chronos serve` makes: its rows change only by merging a world, whoever asks. Tables and other definitions still go straight in. An application that writes `main` directly needs this turned off; the [quickstart](docs/quickstart.md#start-a-server-and-connect-with-psql) shows how, and how to add tokens and TLS before other machines connect.

Moving an existing Postgres database in: [Migrating from Postgres](docs/migrating-from-postgres.md) (`chronos import postgres://...`, then check row counts and checksums).

### C. Give an agent access

For Claude Code, or any MCP client:

```bash
claude mcp add chronos -- chronos mcp /path/to/mydb     # an attached folder works the same
```

By default the agent can read, fork worlds, change only the worlds it forked, diff and preview merges. **A person merges.** To let an agent merge within rules, give it an agent account with a merge policy (`--agent NAME`). See the [MCP reference](docs/reference/mcp.md).

To see the whole idea end to end, the [five-minute trial](examples/trial) runs an agent on a small CRM and shows four of its mistakes caught before they reach `main`.

## How it works

### The words

| Term | Meaning |
|---|---|
| **World** | A branch of the whole database: every table, copy-on-write. Forking one copies a pointer, not the data, so it takes about 2 µs at any size. A world stores only the rows it changes. "Branch" and "world" mean the same thing. |
| **`main`** | The world that holds the real state. Every other world is forked from it, directly or from another world. |
| **Diff** | The rows a world changed since its fork, each with its before and after values. Also available as SQL statements. |
| **Merge** | Applying a world's changes to its parent. It is three-way: if both sides changed the same row, nothing is merged and the conflicts come back as rows for you to settle. |
| **Agent** | An account with its own token and a list of what it may do (`read`, `fork`, `write_own`, `merge_own`, ...), plus quotas and limits. |
| **Merge policy** | Rules an agent's merges must keep to go through without a person: `max_rows`, `max_deletes`, `review_tables`, `review_columns`, rows per hour, and more. |
| **Merge check** | A SQL query a merge's result must return no rows for, such as "every lead list still has a lead". It runs inside every merge. |
| **Review** | A merge that broke a rule waits in `SHOW REVIEWS` with its reasons. A person merges it (approve) or drops it (reject). |
| **Attach** | Running beside an existing Postgres: merges into `main` are written back to it in one checked transaction. |

### The life of an agent run

1. **Fork.** The agent creates a world. It sees all of `main` as it was at that moment and nobody else sees its writes.
2. **Work.** It reads and writes with ordinary SQL. It can write only the worlds it forked, unless you give it more.
3. **Diff.** You, or the agent, see every changed row. `merge ... dry run` shows what a merge would do and why it would be held.
4. **Merge.** Inside the merge, while it holds its locks, Chronos checks conflicts, constraints, the merge policy, merge checks, and (if asked) whether rows the agent read changed since. What is checked is exactly what merges.
5. **Review or land.** Within the rules, it lands. Outside them, it waits in `SHOW REVIEWS`. Attached, it is written to Postgres first.
6. **Undo, if needed.** `UNDO MERGE OF WORLD w` or `UNDO AGENT bot SINCE '-2 hours'` puts the rows back, within the history window (1 hour by default for a new database; `history_retention` sets it). In an attached folder the undo goes back through Postgres too.

More on each step: [Concepts](docs/concepts.md), [Worlds reference](docs/reference/worlds.md), [Merge policies guide](docs/guides/merge-policies.md), [Merge checks guide](docs/guides/merge-checks.md).

## What it is good at

| Agents need to... | Without Chronos | With Chronos |
|---|---|---|
| Write real data without a person reading every change | Read-only agents, or hand-built staging tables and approvals | A merge policy: changes within the rules land, the rest wait in `SHOW REVIEWS` |
| Show what they changed | Audit tables you build yourself | `diff world w`: every changed row, before and after |
| Keep the good work and drop the rest | Hand-written migrations | Merge all of a world, some tables (`only tables`), some rows (`only keys`), or `drop world` |
| Undo a bad run | Restore a backup and lose everyone's work | `UNDO AGENT bot SINCE '-2 hours'`, row by row, leaving others' changes |
| Try many things at once | Copy the database, or share one and fight over locks | Thousands of isolated worlds; 100,000 tested |
| Not act on stale data | Hope | A world can record what it read; its merge is held if that changed |
| Search while they work | A separate vector store that can't see uncommitted edits | Filters, full text and vectors (pgvector syntax) inside every world |
| Stay inside their lane | Database roles | Per-agent capabilities, quotas, time and memory limits, masked columns, one tenant's rows only |

Other things in the box: time travel (`select * from orders as of '-1 hour'`, `restore world`), simulations (`SIMULATE` many worlds and keep the best), a Postgres-compatible SQL surface with PL/pgSQL, JSONB and full text, and a write-ahead log with `fsync` on `main` and merges. The full list is in [SQL](docs/sql.md) and [Search](docs/search.md).

## Where it stands

Chronos DB 0.4.4 is a **public beta**. Use it with staging copies first. More detail: [What Chronos guarantees today](docs/guarantees.md) and [What it protects, and what it doesn't](docs/what-chronos-protects.md).

**Project.** One maintainer. The storage engine has not had an outside review. No outside production use yet.

**Postgres compatibility** (on its own, without attach). Chronos has its own SQL parser and executor; it is not a Postgres fork.

| Measure | Result |
|---|---|
| Postgres 16's own regression suite, output compared line by line | about **89%** of the expected lines; 66 of its 219 files match exactly (run of 2026-10-27) |
| Real databases moved in (Chinook, Northwind, Pagila, pgbench at 2M rows, and others) | **7 of 7** with identical row checksums |
| Differential tests against a real Postgres (`tests/pgdiff.rs`) | 468 tests, each giving the same rows or the same error class |
| Speed, pgbench-style, same machine | point reads at about **half** Postgres's rate; writes at a fraction of it |

Known gaps include: no C extensions or functions in languages other than SQL and PL/pgSQL (common extensions such as `vector`, `pgcrypto`, `citext`, `hstore` and `pg_trgm` are built in); `SERIALIZABLE` is refused rather than weakened; no replication or `pg_hba.conf`; text sorts by bytes unless a column has an ICU collation. Full list and tested drivers and ORMs: [Postgres compatibility](docs/postgres-compatibility.md#where-it-stands-beta).

**Attach limits** (Stage 1, for staging copies):

- Postgres 12 or later. Tested on 16, 17 and 18, and on Neon, direct and through its pooler.
- Each attached table needs a primary key, and no generated columns or row-level security. Partitioned and inheriting tables aren't attached.
- At most 50,000 changed rows per merge by default (`CHRONOS_ATTACH_MAX_ROWS` raises it to 1,000,000), all in one transaction.
- Rows no merge touched are as fresh as the last refresh (`chronos refresh`, or `chronos serve --refresh-every 5m`). A refresh reads every attached table whole.
- Columns added in Postgres are picked up by a refresh. Other schema changes (dropped, renamed or retyped columns) need a new attach.
- Details: [Limits in Stage 1](docs/guides/attach-postgres.md#limits-in-stage-1).

**Operations.** One process per database folder (share it through `chronos serve`). No hot standby or automatic failover. macOS 13+ and Linux only. See [Operations](docs/operations.md).

**Benchmarks it loses** are listed first in [BENCHMARKS.md](BENCHMARKS.md#where-chronos-db-loses), and summarized [below](#benchmarks).

## Clients and integrations

| Client | How | Docs |
|---|---|---|
| `psql`, Postgres drivers and ORMs | Connect to port 5433. Each world is also a database name: `postgres://127.0.0.1:5433/run_42` | [Postgres compatibility: clients](docs/postgres-compatibility.md#clients) |
| TypeScript / JavaScript | `npm install @chronosdb/client`, or copy [one file](clients/typescript/chronos.ts) with no dependencies. Talks to the HTTP API on port 7070 | [Clients](docs/reference/clients.md) |
| Python | Copy [one file](clients/python/chronos.py), standard library only (Python 3.9+). Not on PyPI yet | [Clients](docs/reference/clients.md) |
| Claude Code and other MCP clients | `chronos mcp <folder>`: 30 tools (fork, diff, merge preview, checkpoints, simulations, reviews...) | [MCP](docs/reference/mcp.md) |
| Chronos Studio (web UI) | `chronos studio mydb`: worlds, a data grid, SQL, search, and a Changes view to review and merge. Built into every install, opens on 127.0.0.1 only | [Studio](docs/studio.md) |
| HTTP | `POST /v1/<op>` with JSON | [HTTP API](docs/http-api.md) |
| Rust, embedded | The engine is a Rust library (`chronos::Db`). Not on crates.io; build it from this repository | [Quickstart: Rust](docs/quickstart.md#from-rust) |

A short taste of the TypeScript client:

```ts
import { Chronos } from "@chronosdb/client";

const db = new Chronos("http://127.0.0.1:7070");
const agent = await db.fork("agent-7");
await agent.put("users", "1", { name: "Ada L.", role: "admin" });
console.log(await agent.diff());
await agent.merge(); // throws ChronosError 409 on conflicts
```

Tool definitions for any LLM API with tool calling, and a parallel-agents example, are in [`examples/agents`](examples/agents).

**Chronos Gateway** is a separate product built on this engine: a self-hosted service that lets your own product's agents change the Postgres your app already uses, one customer's rows at a time, through approved merges. It has its own releases at [chronos-gateway-releases](https://github.com/Abhishekxdg/chronos-gateway-releases).

## Benchmarks

Engineering numbers from one machine, not a published benchmark. Unless noted: one GCP c2d-standard-32 (32 vCPU, 128 GB, Linux), every rival on the same machine, 2026-09-28. Methods, raw tables and every caveat: **[BENCHMARKS.md](BENCHMARKS.md)**.

| Workload | Chronos DB | Rivals | |
|---|---|---|---|
| Fork a world, 10 to 100,000 live worlds | **1.4–2.0 µs**; 503–564 B on disk per world with one changed row | — | [§9](BENCHMARKS.md#9-many-worlds-10-to-100000) |
| 1,000 agents each fork, write 1,000 rows and merge (on disk, 64 workers) | **713k–716k rows/s** | Dolt 2.3.5: 28.3k (25× fewer) · Postgres 17 template databases: 9.8k | [§1](BENCHMARKS.md#1-forks-and-concurrent-writes-phase-1-kill-gate-10-postgres) |
| Vector search, 1M real OpenAI embeddings (1,536-d), p50 / recall@10 | **1.34–1.36 ms / 98.6%** | pgvector 0.8.6: 2.40 ms / 92.8%, and 13.5 ms at 98.6% recall · Qdrant: 6.67 ms / 99.0% | [§6](BENCHMARKS.md#6-vector-search-vs-other-vector-databases-on-real-embeddings) |
| Same, filtered to one user (1% of rows) | **1.16–1.19 ms / 100%** | Milvus: 3.89 ms / 99.7% · pgvector: 30.9 ms / 94.6% | [§6](BENCHMARKS.md#6-vector-search-vs-other-vector-databases-on-real-embeddings) |
| BranchBench: six agent workflows (2026-10-24, 16 vCPU) | fastest in **4 of 6** | Doltgres 1.4.0 faster in S4, and in S6, where half its checks failed · Postgres 18 clones slower than Chronos in all six | [§16](BENCHMARKS.md#16-branchbench-six-agent-workflows-against-doltgres-and-postgres-copies) |
| SQLite's sqllogictest, 5.68M records over the Postgres protocol | **99.912%** pass; no wrong answer that Postgres gets right | Postgres 17: 99.796% | [§13](BENCHMARKS.md#13-correctness-sqlites-sqllogictest-over-the-postgres-protocol) |
| Crash safety | 10,000 of 10,000 cut-log recoveries exact; 300 `kill -9`s during 108,178 transfers, none lost | — | [§5](BENCHMARKS.md#5-crash-safety) |

**Where it loses** (the full table is at the top of [BENCHMARKS.md](BENCHMARKS.md#where-chronos-db-loses)):

- **Analytics:** DuckDB 1.5.5 is 2.2–7× faster on reports over 200,000-row tables. Chronos stores rows, not columns.
- **Analytical joins in BranchBench:** CH-benCHmark Q3 and Q18 take about 4× Postgres 18's time.
- **Merges under extreme contention:** with 1,000 agents on one thread each, on disk, merge p50 is 714–718 ms against Postgres's 128 ms. At 25 workers it is 25–26 ms.
- **Redis** answers vector searches faster (0.46–0.50 ms), at 74–75% recall against Chronos's 98.6–99.98%.
- **LanceDB** loads vectors faster (1.0 s against 4.7 s for 76k).

## Documentation

| Start here | Guides | Reference | Running it |
|---|---|---|---|
| [Quickstart](docs/quickstart.md) | [Working beside your Postgres](docs/guides/attach-postgres.md) | [SQL](docs/sql.md) | [Postgres compatibility](docs/postgres-compatibility.md) |
| [Concepts](docs/concepts.md) | [Merge policies](docs/guides/merge-policies.md) | [Worlds](docs/reference/worlds.md) | [Security and auth](docs/security.md) |
| [Migrating from Postgres](docs/migrating-from-postgres.md) | [Merge checks](docs/guides/merge-checks.md) | [Search](docs/search.md) | [What Chronos protects](docs/what-chronos-protects.md) |
| [Five-minute trial](examples/trial) | [Agent sandbox](docs/guides/agent-sandbox.md) | [HTTP API](docs/http-api.md) | [What it guarantees](docs/guarantees.md) |
| [Chronos Cloud](docs/cloud.md) (private alpha) | [Agent evaluation](docs/guides/agent-evaluation.md) | [MCP](docs/reference/mcp.md), [Clients](docs/reference/clients.md) | [Operations](docs/operations.md) |
| | [Scenario planning](docs/guides/scenario-planning.md) | [CLI and settings](docs/reference/cli.md), [Errors](docs/reference/errors.md) | [Benchmarks](BENCHMARKS.md), [Changelog](CHANGELOG.md) |

The same pages, with search, are on the [docs site](https://chronosdb-docs.vercel.app/). The guides' SQL scripts are in [`examples/guides`](examples/guides).

## Feedback and contributing

- **Questions or ideas:** [Discussions](https://github.com/Abhishekxdg/chronosdb/discussions).
- **A bug, or something unclear:** [open an issue](https://github.com/Abhishekxdg/chronosdb/issues/new/choose). The templates ask for the few lines that reproduce it.
- **Security issues:** report them privately, as [SECURITY.md](SECURITY.md) describes.
- **Contributing:** see [CONTRIBUTING.md](CONTRIBUTING.md).

What changed in each release: [CHANGELOG.md](CHANGELOG.md). This release: [v0.4.3](https://github.com/Abhishekxdg/chronosdb/releases/tag/v0.4.3), 2026-10-30.

## What's in this repository

| Path | What |
|---|---|
| [`src/`](src), [`tests/`](tests) | The database engine and its tests (Rust 1.90+). `cargo build --release` builds `target/release/chronos` |
| [`studio/`](studio) | Chronos Studio's source, the web UI built into every release (React, TypeScript, Vite). [Working on it](docs/studio.md#developing-the-studio) |
| [`clients/`](clients) | The TypeScript and Python clients, one file each |
| [`docs/`](docs), [`site/`](site) | The documentation and the docs site that publishes it |
| [`examples/`](examples) | The five-minute trial, the guides' SQL, agent tool definitions, and the Rust benchmark harnesses |
| [`bench/slt`](bench/slt) | The sqllogictest runner's scripts and results |
| [`install.sh`](install.sh) | The installer; signed binaries are on the [releases page](https://github.com/Abhishekxdg/chronosdb/releases) |

## License

- **The engine** (`src/`, `tests/`, the Cargo build, `examples/*.rs`) **and the `chronos` binaries:** [Business Source License 1.1](LICENSE-ENGINE). You may use, change and redistribute it free of charge, including in production and inside commercial products. The one limit: you may not offer Chronos DB to others as a hosted or managed database service. Running it as your own application's database is fine. Each version becomes Apache 2.0 four years after it is published. It is source-available, not open source by the OSI's definition. Plain-language summary: [TERMS.md](TERMS.md).
- **The Studio, the clients, the docs site and the guides' examples:** [MIT License](LICENSE).
