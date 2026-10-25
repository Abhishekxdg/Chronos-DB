<div align="center">

# Chronos DB

### Let AI agents write to your Postgres. Each run works on its own branch; your rules decide what lands.

The branch is the easy part; the merge is what has to be trusted.<br>
Chronos sits **beside the Postgres you already run** (`chronos attach`) or holds the data itself. Every agent run works in its own copy-on-write **world** (a branch of the whole database). Chronos **diffs** it row by row and checks the merge against **rules you approve once**: no deletes without a person, at most 500 rows, every lead list keeps a lead.<br>
What keeps to the rules is written back to your Postgres in one checked transaction; the rest waits for review, with the reasons. It speaks **Postgres** and searches filters, text and vectors inside every world.

<br>

![status](https://img.shields.io/badge/status-v0.4.2%20beta-f59e0b?style=flat-square)
![rust](https://img.shields.io/badge/rust-1.90%2B-b7410e?style=flat-square&logo=rust&logoColor=white)
![postgres wire](https://img.shields.io/badge/postgres-wire%20protocol-336791?style=flat-square&logo=postgresql&logoColor=white)
![mcp](https://img.shields.io/badge/MCP-ready-7c3aed?style=flat-square)
![tests](https://img.shields.io/badge/tests-560%2B%20passing-16a34a?style=flat-square)
![license](https://img.shields.io/badge/license-BUSL--1.1%20engine%20%2B%20MIT-64748b?style=flat-square)

[**Docs site**](https://abhishekxdg.github.io/chronosdb/) ·
[**Quickstart**](docs/quickstart.md) ·
[**Concepts**](docs/concepts.md) ·
[**SQL**](docs/sql.md) ·
[**Search**](docs/search.md) ·
[**HTTP API**](docs/http-api.md) ·
[**Operations**](docs/operations.md) ·
[**Benchmarks**](BENCHMARKS.md) ·
[**Changelog**](CHANGELOG.md)

</div>

<p align="center"><img src="docs/assets/attach-demo.gif" alt="An agent cleans up a CRM in its own world beside a real Postgres: the diff, a merge policy holding back its deletes, Postgres unchanged until a person approves, then the change written to Postgres" width="820"></p>

> [!IMPORTANT]
> **Public beta (0.2): try it on a staging copy, not production.** Nobody outside the author has run a real agent on it yet, and it is built by one person.
> - **Attach** (working beside your Postgres) is new: at most 10,000 changed rows per merge, no refresh of rows a merge doesn't touch, tables need a primary key. Tested on Neon, direct and through its pooler. See [Working beside your Postgres](docs/guides/attach-postgres.md).
> - **On its own,** Chronos speaks the Postgres 16 protocol and much of its SQL, but it is not Postgres: 86.24% of the lines Postgres 16's regression suite expects (60 of its 219 files exactly), 7 real databases moved in with identical row checksums, point reads at about half Postgres's rate and writes at a fraction of it. Read [where it stands](docs/postgres-compatibility.md#where-it-stands-beta) before moving data in.
> - macOS 13+ and Linux only. The benchmarks it loses are listed in [BENCHMARKS.md](BENCHMARKS.md#where-chronos-db-loses).

---

```sql
-- once, as you: the rules an agent's merges must keep
create merge policy agents with (max_deletes = 0, max_rows = 500, review_tables = 'payments');
create agent bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'agents');

-- as the agent (psql postgres://bot:<token>@127.0.0.1:5433/main)
create world run_42 with (task = 'clean up stale leads');   -- private copy of everything, any size
switch world run_42;
update leads set status = 'stale' where last_contact < now() - interval '1 year';
delete from leads where email is null;

diff world run_42;                   -- table, id, change, before, after
merge world run_42 dry run;          -- row by row, plus why it would be held
merge world run_42;                  -- the delete breaks a rule: held, queued in SHOW REVIEWS

-- as you: look, then approve or throw it away
show reviews;
merge world run_42;                  -- or: drop world run_42 (main never saw it)
```

**Already on Postgres?** Keep it there. Attach the tables an agent works on, and every merge above is written to your Postgres first, in one transaction, after checking each row is still as Chronos last saw it (a row changed there meanwhile is a conflict, and nothing is written):

```bash
export CHRONOS_SOURCE_URL='postgres://agent_writer:...@staging-db:5432/crm?sslmode=verify-full'
chronos attach ./crm --tables public.leads,public.accounts
chronos serve ./crm
```


## Contents

- [Why Chronos DB](#why-chronos-db)
- [At a glance](#at-a-glance)
- [How it works](#how-it-works)
- [Features](#features)
- [Quickstart](#quickstart)
- [Benchmarks](#benchmarks)
- [Correctness and testing](#correctness-and-testing)
- [Status and roadmap](#status-and-roadmap)
- [Feedback and contributing](#feedback-and-contributing)
- [What's in this repository](#whats-in-this-repository)
- [License](#license)

---

## Why Chronos DB

Postgres and SQLite were built for **a few long-lived databases with one shared truth**. Agents work differently:

| Agents need to... | Traditional databases | Chronos DB |
|---|---|---|
| Write to real data without a person reading every change | Read-only agents, or hand-built staging tables and approval steps | Merge policies and merge checks: within the rules it merges, outside them it waits in `SHOW REVIEWS` |
| Review what an agent changed | Audit tables you build yourself | `diff` any two worlds or moments, as rows or as SQL |
| Keep the good work, drop the rest | Hand-written migrations | Three-way `merge` (whole, by table, by key, by column), conflicts as rows, or `drop` |
| Undo a bad agent | Restore a backup and lose everyone's work | `UNDO AGENT bot SINCE '-2 hours'`, row by row |
| Try something without breaking production | Copy the database (seconds to minutes) or mock it | `fork` in ~2 µs, any size, on disk |
| Run many attempts in parallel | One connection pool, one truth, lock contention | Thousands of isolated worlds, 100,000 tested |
| Search while working | A separate vector DB that can't see uncommitted edits | Filters + full text + vectors inside every world |
| Stay safe with untrusted agents | Roles and hope | Per-agent capabilities, quotas, time and memory limits; humans approve merges |

---

## At a glance

Headline numbers against the best rival measured on the same machine. Every row links to its method; **losses are listed too**.

| Workload | Chronos DB | Best rival measured | |
|---|---|---|---|
| Fork a 100k-row database for an agent | **~1.5–2 µs** | Dolt 8–54 ms · Postgres 2.1–3.6 s (template DB) | [§1](BENCHMARKS.md#1-forks-and-concurrent-writes-phase-1-kill-gate-10-postgres) |
| 1,000 agents each fork, write 1,000 rows, merge (on disk, 64 workers each) | **713k–716k rows/s** | Dolt 28.3k rows/s (**25×**) · Postgres 17: 9.8k rows/s | [§1](BENCHMARKS.md#1-forks-and-concurrent-writes-phase-1-kill-gate-10-postgres) |
| Merge p50, 1,000 agents at 25 workers (on disk) | **25–26 ms** | Postgres 17: 128 ms | [§1](BENCHMARKS.md#1-forks-and-concurrent-writes-phase-1-kill-gate-10-postgres) |
| Vector search, 76k real OpenAI embeddings (1,536-d), p50 / recall@10 | **0.72 ms / 99.98%** | pgvector 1.01 ms / 82.0% · Weaviate 1.82 ms / 97.0% · Qdrant 3.73 ms / 98.8% | [§6](BENCHMARKS.md#6-vector-search-vs-other-vector-databases-on-real-embeddings) |
| Vector search, 1M real embeddings, p50 / recall@10 | **1.36 ms** / 98.6% | Chroma 2.25 ms / 96.8% · pgvector 2.40 ms / 92.8% · Weaviate 2.48 ms / 98.2% (pgvector at 98.6% recall: 13.5 ms) | [§6](BENCHMARKS.md#6-vector-search-vs-other-vector-databases-on-real-embeddings) |
| Vector search for one user (1%), 1M real embeddings, p50 / recall@10 | **1.16 ms / 100%** | Milvus 3.89 ms / 99.7% · Weaviate 8.47 ms / 100% · pgvector 30.9 ms / 94.6% | [§6](BENCHMARKS.md#6-vector-search-vs-other-vector-databases-on-real-embeddings) |
| Filtered vector search, 1M × 384, p50 | **0.32–1.30 ms** | tuned pgvector 1.7–16 ms (**1.3–50×**) | [§2](BENCHMARKS.md#2-vector-search-vs-postgres--pgvector-phase-1-kill-gate-5-pgvector) |
| Hybrid search (filter + text + vector), 1M rows, p99 | **~2.2 ms**, within 5% inside a fork | — | [§3](BENCHMARKS.md#3-hybrid-search-inside-branches-phase-3-targets-p99--5-ms-at-1m-fork-within-10-of-main) |
| Durable single-row commits (fsync on both) | **2,395–2,543 /s** | Postgres 17: 2,286–2,320 /s | [§4](BENCHMARKS.md#4-sql-over-the-postgres-protocol) |
| Join + `GROUP BY` over the Postgres protocol, 20k rows per table | **4.4 ms** | Postgres 17: 6.9 ms | [§4](BENCHMARKS.md#4-sql-over-the-postgres-protocol) |
| Join + `GROUP BY` in process, 200k rows per table | **25.3 ms** | DuckDB 1.5: 7.5 ms (**a loss, 3.4×**) · Postgres 17: 68.6 ms | [§4](BENCHMARKS.md#4-sql-over-the-postgres-protocol) |
| 100,000 live worlds | **1.4 µs** fork · **564 B** disk per world | — | [§9](BENCHMARKS.md#9-many-worlds-10-to-100000) |

> [!NOTE]
> Every row ran on one GCP c2d-standard-32 (32 vCPU, 128 GB, Linux) on 2026-09-28, with every rival on the same machine. Earlier editions ran on an 8 GB M2 laptop and a 4-vCPU VM. They are engineering numbers, not a published benchmark. Methods and results for each are in [BENCHMARKS.md](BENCHMARKS.md).

---

## How it works

```mermaid
gitGraph
    commit id: "main: real state"
    branch agent-1
    checkout main
    branch agent-2
    checkout agent-1
    commit id: "reprice"
    checkout agent-2
    commit id: "restock"
    commit id: "reprice (conflict)"
    checkout main
    merge agent-1 id: "merge: 1 change"
    checkout agent-2
    commit id: "resolve by columns"
    checkout main
    merge agent-2 id: "merge: settled"
```

**Worlds are copy-on-write.** A world is a pointer to an immutable, content-addressed tree of pages. Forking copies the pointer, not the data, so it costs the same ~2 µs at 1,000 rows or 1 million. A world then pays only for what it changes (about 450 bytes on disk for one changed row).

**Merges are three-way and exact.** A merge diffs the world against its fork point, skipping every subtree both sides share, and applies each changed row. If both sides changed the same row, the merge stops and returns the conflicts as data, even when the values happen to match. You then settle them: `USING OURS/THEIRS`, row by row, or `BY COLUMNS` when the two sides touched different columns. Partial merges (`ONLY TABLES`, `ONLY KEYS`) and merges into another world (`INTO`) are logged exactly, so replay after a crash gives the same result.

```mermaid
flowchart LR
    subgraph Clients
        PSQL[psql / Postgres drivers / ORMs]
        HTTP[HTTP JSON API<br/>TypeScript + Python clients]
        MCP[MCP server<br/>Claude Code, agent frameworks]
        RUST[Rust: chronos::Db]
    end
    subgraph Engine
        SQL[SQL planner + executor<br/>parallel scans, joins, spill to disk]
        SEARCH[Search<br/>B-tree · BM25 · tsvector · HNSW + 1-bit scan · geo]
        WORLDS[Worlds<br/>fork · diff · merge · time travel · undo]
        AGENTS[Agents<br/>tokens · capabilities · quotas · limits]
    end
    subgraph Storage
        TREE[Copy-on-write page tree<br/>content-addressed, compressed]
        WAL[Write-ahead log<br/>fsync on main + merges]
        CKPT[Checkpoints + GC]
        S3[(Local folder or<br/>S3 / R2 / MinIO)]
    end
    PSQL --> SQL
    HTTP --> WORLDS
    MCP --> AGENTS
    RUST --> WORLDS
    AGENTS --> WORLDS
    SQL --> SEARCH
    SQL --> WORLDS
    SEARCH --> TREE
    WORLDS --> TREE
    WORLDS --> WAL
    TREE --> CKPT
    CKPT --> S3
    WAL --> S3
```

**Search indexes are shared.** Indexes are built for anchor states and shared by every world forked from them. A world searches the anchor's index minus the keys it changed, plus its changed rows scored directly. That is why a query inside one of 100,000 worlds runs as fast as in `main`.

---

## Features

<table>
<tr>
<td width="50%" valign="top">

### 🌍 Worlds (branches of everything)
- `CREATE / FORK / SWITCH / DROP WORLD`, with JSON metadata, owners and IDs
- Worlds of worlds, to any depth (10,000 deep tested)
- `DIFF` between any two worlds or moments, as rows or as runnable SQL
- `MERGE`: whole, `DRY RUN`, `ONLY TABLES`, `ONLY KEYS`, `INTO` another world, `BY COLUMNS`, per-row `RESOLVE`
- **Time travel:** `SELECT ... AS OF '-1 hour'`, `RESTORE WORLD`, `UNDO MERGE` (30-day retention by default)
- **Undo an agent:** `UNDO AGENT bot SINCE '-2 hours' SKIP CHANGED`
- **Simulations:** `SIMULATE 1000 WORLDS ... RUN $$...$$ SCORE $$...$$ KEEP 10 SEED 42`, deterministic and replayable
- `SHOW STORAGE` per world, `SET TTL`, idle-world sweep, pinned worlds

</td>
<td width="50%" valign="top">

### 🐘 Postgres, for real
- Postgres wire protocol: `psql`, drivers, ORMs, prepared statements, `COPY`, `LISTEN/NOTIFY`
- Joins (inner/left/right/full), subqueries, CTEs, `WITH RECURSIVE`, window functions
- Unique, foreign-key (multi-column, cascade) and `CHECK` constraints; `ON CONFLICT` upserts
- Views, materialized views, sequences, enums, temp tables, schemas, `information_schema` / `pg_catalog`
- **PL/pgSQL** functions and triggers, with `EXCEPTION` blocks and `EXECUTE`
- JSONB, arrays, regex, dates/intervals, numeric `to_char`, `SIMILAR TO`
- Transactions, savepoints, `statement_timeout`, `EXPLAIN`
- Checked against real Postgres 17 by a differential test suite (`tests/pgdiff.rs`)

</td>
</tr>
<tr>
<td valign="top">

### 🔎 Search, inside every world
- **Vectors:** pgvector-compatible `vector(N)`, `<=>` `<->` `<#>`, `USING hnsw`. Runs [Mem0](https://github.com/mem0ai/mem0)'s pgvector store unchanged
- HNSW graph built in the background, tuned per graph, used only when it proves ≥ 99% recall against exact search; otherwise an exact 1-bit scan with full-precision rescoring
- **Full text:** Postgres `tsvector` / `tsquery`, `ts_rank`, `websearch_to_tsquery`, English Snowball stemmer (identical on 20,000 words), GIN indexes
- **BM25 + typo tolerance + fusion:** `search('table', 'words')`, and `find ... where ... match ...`
- **Geo:** PostGIS points, `ST_DWithin`, exact WGS84 geodesic `ST_Distance`, `USING gist`, nearest-first `<->`
- **Graphs:** recursive traversals driven by indexes (4 hops over 200k edges: 30 ms to 1 ms)

</td>
<td valign="top">

### 🤖 Built for agents
- **MCP server:** `claude mcp add chronos -- chronos mcp mydb`, with `fork`, `diff`, `merge_preview`, `checkpoint`, `rollback` and more
- **Safe by default:** agents change only worlds they forked; a person approves merges (`--allow-merge` to opt out)
- **Agent accounts:** tokens (blake3-hashed), capabilities (`read`, `fork`, `write_own`, `merge_own`, `admin`...), quotas (worlds, changes, writes per minute)
- **Per-agent limits:** `max_query_ms`, `max_concurrent`, `max_memory_mb`
- **Server-wide:** `statement_timeout`, `max_worlds`, `world_idle_ttl`; `serve --safe --admin-token`
- One folder shared by many processes: the first opener serves the rest
- Examples for Claude Code, LLM tool definitions and parallel agents in [`examples/agents`](examples/agents)
- A five-minute trial on a small CRM, where four mistakes an agent makes are caught before they reach `main`: [`examples/trial`](examples/trial)

</td>
</tr>
<tr>
<td valign="top">

### 💾 Storage and durability
- Write-ahead log with `fsync` (macOS `F_FULLFSYNC`) on `main` and merges before they return
- Branch writes are fast, and loss is never silent: versions are checked on merge
- LZ4-compressed, content-addressed pages; background checkpoints and garbage collection
- `synchronous_commit = normal | off` when you trade safety for speed
- Page cache with a hard memory cap; databases many times bigger than RAM
- Encryption at rest, backups, and **S3 / R2 / MinIO** storage with a single-writer lease and scale-to-zero (`--features s3`)

</td>
<td valign="top">

### ⚡ Speed
- Scans, hash joins and `GROUP BY` split across every core
- Joins stream: each core joins and groups its part without holding either side whole
- Sorts on every core; `ORDER BY ... LIMIT k` keeps only k rows
- **Spills to disk** past `CHRONOS_WORK_MEM`: `GROUP BY`, `DISTINCT` aggregates, `ORDER BY`, grace hash joins, and the writes of a big `INSERT`, `UPDATE`, `DELETE` or `COPY`, which stays one atomic statement
- Packed binary rows, prepared plans cached per schema version
- AVX2 dot products and `POPCNT` 1-bit vector scans
- `SHOW METRICS`: count and p50 / p95 / p99 latency per operation, plus counters

</td>
</tr>
</table>

---

## Quickstart

### Install

A prebuilt binary for macOS or Linux (x86_64 and arm64), checked against the release's SHA-256 sums, into `~/.local/bin`:

```bash
curl -fsSL https://github.com/Abhishekxdg/chronosdb/releases/latest/download/install.sh | sh
chronos --version
```

`CHRONOS_VERIFY=1` also checks the release's signature with [cosign](https://docs.sigstore.dev/cosign/system_config/installation/).

No telemetry: Chronos never connects anywhere on its own, only to what you give it (a `--remote` to store in, a Postgres you import from or attach to).

### Postgres: psql, drivers, ORMs

```bash
chronos serve mydb                              # HTTP on :7070, Postgres on :5433, loopback only
psql postgres://127.0.0.1:5433/main
export DATABASE_URL="postgres://app@127.0.0.1:5433/main"    # for your application
```

Every world is also a database name: `psql postgres://127.0.0.1:5433/agent_7`. Moving a Postgres database in, and checking it came over: [Migrating from Postgres](docs/migrating-from-postgres.md).

### The shell

```text
$ chronos mydb
chronos:main> put users 1 {"name": "Ada", "role": "admin"}
chronos:main> fork agent-7                       # instant private copy
chronos:agent-7> put users 1 {"name": "Ada L.", "role": "admin"}
chronos:agent-7> diff
1 change on agent-7
~ users/1  name: "Ada" -> "Ada L."
chronos:agent-7> merge
merged 1 change from agent-7 into main; now on main
chronos:main> find users where role = admin match ada
```

### TypeScript and Python (one file each, no dependencies)

```ts
import { Chronos } from "./clients/typescript/chronos";

const db = new Chronos("http://127.0.0.1:7070");
const agent = await db.fork("agent-7");
await agent.put("users", "1", { name: "Ada L.", role: "admin" });
console.log(await agent.diff());
await agent.merge(); // throws ChronosError 409 on conflicts, or if a crash lost writes
```

```python
from chronos import Chronos

agent = Chronos("http://127.0.0.1:7070").fork("agent-7")
agent.put("users", "1", {"name": "Ada L.", "role": "admin"})
agent.merge()
```

### Chronos Studio: a web UI, built in

```bash
chronos studio mydb        # opens the Studio in your browser, on 127.0.0.1 only
```

Worlds as a tree, each table's rows in a fast grid (filters, sorting, any table as of any moment), a row inspector, a SQL console, and the Changes view where you review a world's changes, preview the merge with conflicts explained, and merge into main (or discard, or undo). Edits on main offer to go into a new world first. Agents, the audit log and status are one menu away; ⌘K does the rest. It's in every install, runs beside a running `chronos serve` on the same folder, and a per-run key in the link keeps other pages and programs out. See [docs/studio.md](docs/studio.md), which also covers working on the UI (`npm run dev` in `studio/`).

### Claude Code and other MCP agents

```bash
claude mcp add chronos -- chronos mcp /path/to/mydb
```

The agent can describe, search, query, fork, write, diff and preview merges. **A person merges** unless you pass `--allow-merge` or give an agent account that right.

### Bring your data

```bash
psql postgres://127.0.0.1:5433/main -c "\copy orders from 'orders.csv' csv header"
chronos import mydb orders orders.csv --schema schema.csv    # a Postgres table, schema and all
chronos mydb migrate ./migrations                            # apply numbered .sql files once each
```

---

## Benchmarks

The full record, with methods, raw tables and every caveat, is in **[BENCHMARKS.md](BENCHMARKS.md)**. Highlights:

### Forks and concurrent agents

1,000 agents each fork `main` (100k seed rows), write 1,000 rows and merge back. Same 32-core machine for every system:

| system | fork p50 | merge p50 | total | agent rows/s |
|---|---:|---:|---:|---:|
| **Chronos DB** (in memory) | **0.005 ms** | 322 ms¹ | **0.77 s** | **1.29M** |
| **Chronos DB** (on disk, 25 workers) | 0.007 ms | 25–26 ms | 1.4 s | 707k–712k |
| **Chronos DB** (on disk, 64 workers) | 0.008 ms | 64–66 ms | 1.4 s | 713k–716k |
| Dolt 2.3.5 (real `DOLT_MERGE`, 64 workers) | 24 ms | 866 ms | 35 s | 28.3k |
| Postgres 17 (`CREATE DATABASE ... TEMPLATE`, 25 workers) | 2,133 ms | 128 ms | 102 s | 9.8k |
| SQLite (file copy + `EXCEPT` merge) | 2,762 ms | 3,113 ms | 404 s | 2.5k |

Against **Dolt 2.3.5**, the only other database with real merges, at the same 64 workers: **22×** at 10 agents, **20×** at 100 and **16×** at 1,000 before #23 and #29, and **25×** at 1,000 with them.

Postgres 18 clones (`CREATE DATABASE … STRATEGY FILE_COPY` with `file_copy_method = clone`, on XFS or Btrfs, with nothing else connected to the source) and [Xata](https://xata.io/docs/core-concepts/branching)'s open-source copy-on-write branches make copies fast too, but neither diffs or merges an agent's changes back or returns conflicts as rows. We haven't measured either; see [BENCHMARKS.md §1](BENCHMARKS.md#1-forks-and-concurrent-writes-phase-1-kill-gate-10-postgres).

¹ Merges into `main` queue behind each other. On disk with one thread per agent, 1,000 at once, the merge p50 is **714–718 ms, still a loss** to Postgres's 128 ms (it was 1,231 ms before an open world's writes reached the log in batches, #23, and each thread remembered its last world, #29), and fork p50 there rose to 65–73 ms. What's left is the branch map's lock, which forks and merges hold while they log ([§1](BENCHMARKS.md#1-forks-and-concurrent-writes-phase-1-kill-gate-10-postgres), [§8](BENCHMARKS.md#8-the-losses-rerun-on-linux)).

### Vector search on real embeddings

OpenAI embeddings (1,536-d, DBpedia) + 500 queries, top 10, eleven systems each at its defaults, each alone on the same machine. Chronos DB is queried over SQL exactly as Mem0's pgvector store queries it.

| system | 76k: p50 | recall@10 | 1M: p50 | recall@10 | 1M, one user (1%): p50 | recall@10 |
|---|---:|---:|---:|---:|---:|---:|
| **Chronos DB** (SQL, pgvector syntax) | **0.72 ms** | **99.98%** | 1.36 ms | 98.6% | **1.16 ms** | **100%** |
| Redis 8.10 | 0.46 ms | 75.0% | 0.50 ms | 74.2% | 11.1 ms | 100% |
| pgvector 0.8.6 | 1.01 ms | 82.0% | 2.40 ms | 92.8% | 30.9 ms | 94.6% |
| Milvus 3.0 | 1.64 ms | 89.2% | 3.92 ms | 97.3% | 3.89 ms | 99.7% |
| Weaviate 1.39 | 1.82 ms | 97.0% | 2.48 ms | 98.2% | 8.47 ms | 100% |
| Chroma 1.5 (embedded) | 1.94 ms | 92.6% | 2.25 ms | 96.8% | 265 ms | 99.7% |
| pgvectorscale 0.9 | 1.95 ms | 97.5% | 8.33 ms | 96.6% | 51.4 ms | 69.8% |
| Elasticsearch 9.5 | 3.24 ms | 90.8% | 3.56 ms | 94.9% | 3.47 ms | 61.3% |
| LanceDB 0.39 (embedded) | 3.30 ms | 85.0% | 3.43 ms | 85.0% | 28.0 ms | 96.7% |
| Qdrant 1.19 | 3.73 ms | 98.8% | 6.67 ms | 99.0% | 5.83 ms | 99.98% |
| OpenSearch 3.8 | 5.12 ms | 96.6% | 10.06 ms | 98.5% | 4.83 ms | 80.6% |

At 1M, Chronos is the fastest of every system that finds more than 75% of the true top 10 (only Redis is faster, at 74%), and the fastest of all for one user; only Qdrant finds more (99.0% against 98.6%, at 6.67 ms). At 76k it has the best recall. Raised to Chronos's 98.6% recall, pgvector takes 13.5 ms against Chronos's 1.36 ms; at the 99.3% recall of Chronos's earlier default beam (`SET hnsw.ef_search = 1000`), Chronos takes 2.30 ms, still ahead of every rival that finds more than 96.8% ([§6](BENCHMARKS.md#6-vector-search-vs-other-vector-databases-on-real-embeddings)).

Synthetic 1M × 384 against **tuned** pgvector (`shared_buffers = 8GB`, prewarmed, `ef_search = 300`, iterative scan):

| filter keeps | Chronos p50 | pgvector p50 | Chronos recall | pgvector recall |
|---|---:|---:|---:|---:|
| all rows | 0.74 ms | 1.56–1.61 ms | 100% | 91.8% |
| 10% | 1.30 ms | 1.68–1.78 ms | 100% | 91.6% |
| 1% | 0.64 ms | 6.3–6.4 ms | 100% | 88.6% |
| 0.1% | 0.32 ms | 10.1–16.1 ms | 100% | 100% |

### Hybrid search inside a fork (1M rows)

| query | main p50 | main p99 | fork ÷ main |
|---|---:|---:|---:|
| filter (1%) | 0.05 ms | 0.09–0.10 ms | 0.91–0.95× |
| text | 0.37 ms | 0.40–0.41 ms | 1.12–1.13× |
| vector + filter (20%) | 0.98–0.99 ms | 1.03–1.04 ms | 1.03–1.05× |
| filter + text + vector | 1.98–2.08 ms | 2.11–2.18 ms | 1.03–1.05× |

Unfiltered vector search in a fork is 1.55× main (2.1 against 1.34 ms), a miss against the 10% target ([§3](BENCHMARKS.md#3-hybrid-search-inside-branches-phase-3-targets-p99--5-ms-at-1m-fork-within-10-of-main)).

### SQL over the Postgres protocol

200,000 rows per table, ms per query. Chronos and DuckDB 1.5 run in process; Postgres 17 runs over local TCP with its default parallel workers:

| query | **Chronos** | Postgres 17 | DuckDB 1.5 |
|---|---:|---:|---:|
| `count(*)` | **2.8** | 6.4 | 0.4 |
| `count(*) WHERE amount > 500` | **4.0** | 8.7 | 0.7 |
| `GROUP BY` with `count`, `max` | **9.8** | 19.6 | 4.4 |
| join, then `GROUP BY` with `sum` | **25.3** | 68.6 | 7.5 |
| join `WHERE name LIKE 'user 1%'` | **18.6** | 45.2 | 7.7 |

Over the Postgres protocol at 20,000 rows per table, join + `GROUP BY` takes **4.4 ms** against Postgres 17's 6.9 ms. DuckDB, a column store built for this kind of query, is 2–7× faster than Chronos on these reports.

OLTP at equal durability (`fdatasync` on both): **2,395–2,543 commits/s** against Postgres 17's 2,286–2,320; lookups by key 46–48 µs against 46 µs; key join 52 µs against **50 µs**.

### 100,000 worlds

| worlds | fork p50 | point query main / world | disk per world | memory per world |
|---:|---:|---:|---:|---:|
| 1,000 | 1.7 µs | 21 / 14 µs | 551 B | 0.3 KB |
| 10,000 | 1.5 µs | 21 / 21 µs | 557 B | 0.1 KB |
| 100,000 | 1.4 µs | 23 / 21 µs | 564 B | 1.0 KB |

Checkpoint and reopen with 100,000 worlds: 0.87 s and 0.17 s.

### The ultimate test: 300,000 simulated futures, one merge

A real supply chain lives in `main`. **100,000 worlds** each simulate their own reorder policy in SQL (`WITH RECURSIVE` over days of demand), are scored in SQL, and the best 1% fork 100 children each, for three rounds. The winning policy is merged back into `main`: **+32.2% profit** over today's policy, in **4 minutes 40 seconds** on 32 cores (21 minutes on an 8 GB laptop), and the same winner on every run with the same seed, on either machine ([§10](BENCHMARKS.md#10-the-ultimate-test-one-real-state-100000-worlds-a-learning-loop-one-merge)).

Also measured: **Monte Carlo Tree Search** where every tree node is a world (Connect Four, 49,402 worlds, 0.84 ms per iteration, 20–0 against a random player, [§11](BENCHMARKS.md#11-monte-carlo-tree-search-over-worlds)), and a database **25× bigger than its cache**, its page cache held to its 32 MB cap ([§12](BENCHMARKS.md#12-a-database-bigger-than-its-cache)).

### Where Chronos DB loses (today)

| workload | result | why / plan |
|---|---|---|
| Analytics vs **DuckDB 1.5.5** (200k-row reports, 32 threads) | DuckDB **2.2–7× faster** | DuckDB is a columnar OLAP engine; Chronos stores rows. Columnar storage is not planned for v0.1; row counts in the tree and per-page min/max would narrow it |
| Merges on disk, 1,000 agents on one thread each | merge p50 **714–718 ms**, Postgres 128 ms (was 1,231 ms); fork p50 65–73 ms (was 4–5 ms); the run takes 1.8 s against 0.76 s in memory (was 5.0 s) | An open world's writes now reach the log in batches; forks and merges still hold the branch map's lock while they log, which parks the writers. At 25 workers: 1.4 s, merge p50 25–26 ms. See [BENCHMARKS §8](BENCHMARKS.md#8-the-losses-rerun-on-linux) |
| Vector search inside a fork, 1M rows | **1.55×** main, unfiltered; 1.12× text (target: 1.1×) | Probably the fork's changed rows searched separately; not profiled yet |
| Filtered vector search at 10%, 500k × 384 | **1.3×** tuned pgvector (the Phase 1 gate is 5×) | The other filter mixes pass (3.4–47×) |
| Redis vector search (76k and 1M) | Redis **0.46–0.50 ms** against 0.72 and 1.36 ms | At its defaults Redis finds 74–75% of the true top 10; Chronos 98.6–99.98% |
| Bulk vector load vs LanceDB | Chronos 4.7 s (76k), 64 s (1M) vs LanceDB **1.0 s, 12 s** | LanceDB takes an in-process Arrow table; Chronos gets rows over the Postgres protocol |
| Server memory at 1M vectors | Chronos 7.7 GB after, 14.4 GB peak; Milvus 5.2 GB, Qdrant 6.6 GB | 6 GB of raw vectors; not investigated yet |

---

## Correctness and testing

| Check | What it does | Result |
|---|---|---|
| **Test suite** | 71 integration suites + unit and doc tests, run in CI on Linux | **563 passed, 0 failed** (7 ignored) |
| **Crash suite** | Cuts the log at random bytes with torn writes, then recovers (`CRASH_RUNS=10000`) | 10,000 / 10,000 recover exactly what reached disk |
| **Deterministic simulation** | [shuttle](https://github.com/awslabs/shuttle) explores concurrent schedules of agents, writes, checkpoints and GC | Passes 3,000 schedules; failing schedules replay exactly |
| **Jepsen-style** | `kill -9` the server during concurrent fork/merge bank transfers | 300 kills, 108,178 transfers, none lost, every balance matches |
| **Postgres differential** | Same queries on Chronos DB and real Postgres 17, results compared | Functions, types, errors and formatting match |
| **sqllogictest** | SQLite's query corpus (622 files, 5.68M records) over the Postgres protocol, the same runner on Chronos DB and Postgres 17 | Chronos **99.912%**, Postgres 17 99.796%; **no wrong answer Postgres gets right**; 63 fail only on Chronos (division by zero beside a NULL constant). [§13](BENCHMARKS.md#13-correctness-sqlites-sqllogictest-over-the-postgres-protocol) |
| **Capability audit** | 30 end-to-end capability checks, executed | 27 pass, 3 partial (being closed) |

The Jepsen-style test found two real bugs before this release (money created by a merge that treated equal values as no conflict, and a silent loss after a fast restart), and both are fixed. See [BENCHMARKS.md §5](BENCHMARKS.md#5-crash-safety).

---

## Status and roadmap

Chronos DB is a **public beta** (0.4.2), and not yet used in production. The previous release was [v0.4.1](https://github.com/Abhishekxdg/chronosdb/releases/tag/v0.4.1), on 2026-10-23.

- [x] Worlds: fork, diff, three-way merge, partial merges, time travel, undo
- [x] Postgres wire protocol and a broad SQL surface, checked against Postgres 17
- [x] Schemas, `search_path`, `::regclass`, `pg_index` / `pg_constraint` and the `information_schema` constraint views
- [x] PL/pgSQL `EXCEPTION` blocks, `EXECUTE`, `ALTER TYPE` for enums
- [x] Hybrid search: B-tree, BM25, full text, pgvector + HNSW (graded against exact search), PostGIS points
- [x] Agents: MCP, accounts, capabilities, quotas, per-agent limits, safe mode, `SIMULATE` / `REPLAY`
- [x] Durability: WAL, crash suite, shuttle, Jepsen-style kills; S3 storage
- [x] Parallel execution and spill to disk
- [x] Bounded memory for big work: `CREATE INDEX` / `ADD UNIQUE`, one big `INSERT` / `UPDATE` / `DELETE` / `COPY` (atomic), `DISTINCT` aggregates
- [x] Chronos Studio, built in: worlds and the worldline, data grid with in-place editing, import and export, schema diagram, SQL, hybrid and vector search with a map of the vector space, changes and merges, history and checkpoints, simulations, agents, settings
- [x] Fixes from four full code reviews: parser and regex recursion limits, caps on user-controlled sizes, lock poisoning after a panic, Origin and Host checks on the loopback HTTP API, hardened spill files
- [x] First tagged release, [v0.1.0](https://github.com/Abhishekxdg/chronosdb/releases/tag/v0.1.0): signed binaries for macOS and Linux
- [x] [v0.1.1](https://github.com/Abhishekxdg/chronosdb/releases/tag/v0.1.1): merge policies, the sqllogictest correctness run and its fixes, and the fourth review's fixes
- [x] [v0.1.2](https://github.com/Abhishekxdg/chronosdb/releases/tag/v0.1.2): `chronos import postgres://`, merge checks, the stale-read check, what a change affects, merging by columns per table, and faster joins and agent writes on many cores
- [x] [v0.1.3](https://github.com/Abhishekxdg/chronosdb/releases/tag/v0.1.3): vector search at 1M 2.1× faster at the same recall, a run's declared scope, `review_columns`, effects only on merge (`NOTIFY ON MERGE`), limits per agent over an hour, and `max_age`
- [x] A plain scan's memory stays flat as tables grow: a `count(*)` or `sum` needs +6 to +8 MB at 1M to 10M rows (it was +63 MB at 10M, and RSS 250 MB is now 107 MB). What still grows is the page directory an open database keeps, about 3 MB per million narrow rows
- [x] Bulk vector ingest: `COPY FROM STDIN (FORMAT binary)` with pgvector's binary vectors, COPY streaming into its INSERT, vectors stored 7 bits to a character. Loading 76k embeddings is now bound by the disk, not the protocol (it decodes them all in 0.7 s); LanceDB's 2 s load doesn't wait for the disk (no fsync)
- [x] Write less per vector load: integer keys are stored in number order (tables made from now on), so a load in id order goes straight into the tree. A binary COPY of 76k embeddings writes 526 MB (it was 1.82 GB) in 8–15 s
- [x] Bulk loads write once: COPY, `INSERT ... SELECT` or a big `INSERT` skip the log. Many small transactions write each row twice (the log, then the tree), as Postgres does: 1.27 GB for the same 76k embeddings (see [Loading and unloading](docs/sql.md#loading-and-unloading-copy))
- [x] Neon branching benchmark: a Neon branch is ready in 2.3–3.7 s (p50) against Chronos's 0.1–0.2 ms fork in the same harness, and 10 to 100 agents write 0.8k–1.1k rows/s against 270k
- [ ] Outside review of the storage code, design partners

See [CHANGELOG.md](CHANGELOG.md) for everything that has shipped, and [SECURITY.md](SECURITY.md) to report a vulnerability.

---

## Feedback and contributing

- **Questions or ideas?** Ask in [Discussions](https://github.com/Abhishekxdg/chronosdb/discussions).
- **Found a bug, or something unclear?** [Open an issue](https://github.com/Abhishekxdg/chronosdb/issues/new/choose): the templates ask for the few lines that reproduce it.
- **Security issues:** privately, as [SECURITY.md](SECURITY.md) says.
- **Contributing:** see [CONTRIBUTING.md](CONTRIBUTING.md).

## What's in this repository

Everything Chronos DB is, in one place:

- **[`src/`](src), [`tests/`](tests):** the database engine's source and its tests (Rust), under the Business Source License (see License below). Build it with `cargo build --release`; the binary is `target/release/chronos`.
- **Releases:** signed `chronos` binaries for macOS and Linux, built from that source, and `install.sh`.
- **[`studio/`](studio):** Chronos Studio's source, the web UI built into every release (React, TypeScript, Vite). See [docs/studio.md](docs/studio.md#developing-the-studio) to work on it.
- **[`clients/`](clients):** the TypeScript and Python clients, one file each, no dependencies.
- **[`docs/`](docs)** and **[`site/`](site):** the documentation, published at [abhishekxdg.github.io/chronosdb](https://abhishekxdg.github.io/chronosdb/).
- **[`examples/`](examples):** SQL walkthroughs and agent tool definitions.

## License

- **The engine** (`src/`, `tests/`, the Cargo build, `examples/*.rs`) **and the `chronos` binaries built from it:** the [Business Source License 1.1](LICENSE-ENGINE). Free to use, change and redistribute, including in production and inside commercial products, except to offer Chronos DB itself to others as a hosted or managed database service. Running it as your own application's database is fine. Each version becomes Apache 2.0 four years after it is published. It is source-available, not open source by the OSI's definition. See [TERMS.md](TERMS.md).
- **The Studio, the clients, the docs site and the guides' examples:** the [MIT License](LICENSE).
