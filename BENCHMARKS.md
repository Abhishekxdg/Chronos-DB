# Benchmarks

> The numbers, the methods and the engine's source are public (most harnesses are `examples/*.rs`); a few runner scripts stay in the private development repository.

**Machine.** Unless a section says otherwise, everything here was rerun end to end on 2026-09-28 on one Linux server: GCP `c2d-standard-32` (32 vCPUs: an AMD EPYC 7B13, 16 cores with 2 threads each; 128 GB of RAM; a 500 GB pd-ssd; Ubuntu 24.04). Every rival ran on the same machine, driven by the same client as Chronos DB, one job at a time on an otherwise idle machine (a queue ran the jobs; each section gives the load average where it matters). The raw output of every run, and the script that made it, is in [`bench/results/2026-09-28-rerun/`](bench/results/2026-09-28-rerun).

**The machine changed.** Earlier editions of this page ran on an 8 GB Apple M2 laptop (sections 1–7 and 9–12, often while other builds ran on it) and on a 4-vCPU, 32 GB GCP VM (section 8). Where a result changed, the old figure is given beside the new one. Numbers from different machines don't compare directly: one of these cores is slower than one of the M2's, and there are four times as many.

**Chronos build.** Most Chronos numbers are `0f1a19af`: `main` at `368410af` (which keeps query threads between queries, and uses mimalloc on Linux) plus one fix this rerun found (section 6). Runs marked `8db2f701` used `main` from before those changes. Section 4's joins and reports also show `cf1c9f3a`, which adds #22 (small tables split into more parts): it changes nothing at 200,000 rows, but a lot at 20,000 (the protocol join + `GROUP BY` went from 7.0 to 4.4 ms). The rivals' numbers don't depend on which Chronos build ran beside them.

**Rivals.** Postgres 17.11 and 16.15, pgvector 0.8.6, pgvectorscale 0.9.1, SQLite (bundled with rusqlite), Dolt 2.3.5, DuckDB 1.5.5, Qdrant 1.19.1, Chroma 1.5.9, LanceDB 0.39, Milvus 3.0.1, Weaviate 1.39.7, Elasticsearch 9.5.4, OpenSearch 3.8.0 and Redis 8.10.2 (the last five in Docker). Each runs at its defaults unless a section says otherwise, and never tuned below them. Recall is always shown beside speed, because the defaults trade the two differently. Neon wasn't rerun (it needs an API key this run didn't have); its numbers are from 2026-09-27.

**Rerunning them.** Every harness is in [`examples/`](examples) and [`bench/`](bench), including the rival races (`bench/rivals/forks.py`). Losses are reported here along with the wins, and listed first.

## Where Chronos DB loses

| against | workload | Chronos DB | rival | section |
|---|---|---|---|---|
| **DuckDB 1.5.5** | analytics reports over 200,000-row tables | 2.8–25.4 ms | 0.4–8.3 ms: **2.2–7× faster** | [4](#4-sql-over-the-postgres-protocol) |
| **Postgres 17** | merge p50, 1,000 agents on disk, one thread per agent | 714–718 ms (1,231 ms before #23 and #29) | **128 ms** (25 workers) | [1](#1-forks-and-concurrent-writes-phase-1-kill-gate-10-postgres) |
| **its own target** | vector search inside a fork, against main, 1M rows | 1.55× main (no filter), 1.12× (text) | target: within 1.1× | [3](#3-hybrid-search-inside-branches-phase-3-targets-p99--5-ms-at-1m-fork-within-10-of-main) |
| **its own gate** | filtered vector search at 10%, 500k × 384, against tuned pgvector | 0.93 ms | 1.17–1.22 ms: only 1.3×, under the 5× gate | [2](#2-vector-search-vs-postgres--pgvector-phase-1-kill-gate-5-pgvector) |
| **Redis 8.10** | vector search median, all rows, 76k and 1M real embeddings | 0.72–0.74 ms, 1.36 ms | **0.46 ms, 0.50 ms**, at 75.0% and 74.2% recall (Chronos: 99.98%, 98.6%) | [6](#6-vector-search-vs-other-vector-databases-on-real-embeddings) |
| **Postgres 17 + pgvector 0.8.7** | filtered vector search (50% of rows), 8 clients, 1M × 768 | 902 q/s at 99.96% recall | **1,289 q/s** at 98.3% (`ef_search` 1,000) | [2](#side-by-side-at-1m-768-and-1536-dimensions-every-distance-2026-10-17-issue-34) |
| **its own scaling** | filtered vector search (10%, 50%), 1 to 8 clients, 1M × 768 | 2.3× and 1.8× the throughput; p99 14.8 and 20.4 ms | | [2](#side-by-side-at-1m-768-and-1536-dimensions-every-distance-2026-10-17-issue-34) |
| **LanceDB 0.39** | loading 76k × 1,536 vectors | 4.7 s | **1.0 s** (in its own process, from an Arrow table) | [6](#6-vector-search-vs-other-vector-databases-on-real-embeddings) |
| **Weaviate, Redis** | one-user (1%) vector search p99, 76k | 2.1–2.3 ms | **1.46 ms, 1.23 ms** | [6](#6-vector-search-vs-other-vector-databases-on-real-embeddings) |
| **Postgres 17** | one order joined to its row by key, over the protocol | 52 µs | **50 µs** | [4](#4-sql-over-the-postgres-protocol) |
| **Redis, Weaviate** | server memory after the 76k run | 1.14 GB (2.3 GB at its peak) | 1.05 GB, 1.10 GB | [6](#6-vector-search-vs-other-vector-databases-on-real-embeddings) |
| **Doltgres 1.4.0** | BranchBench S4: dev worlds rebasing off a busy spine, W=5 | 663 s | **563 s** | [16](#16-branchbench-six-agent-workflows-against-doltgres-and-postgres-copies) |
| **Postgres 18** | CH-benCHmark Q3 and Q18, W=1 | 0.32 s, 0.27 s | **0.08 s** each | [16](#16-branchbench-six-agent-workflows-against-doltgres-and-postgres-copies) |
| **Doltgres, Postgres copies** | the spine's TPC-C throughput while BranchBench's agents work (S4) | 28 txn/s | **30, 73** txn/s | [16](#16-branchbench-six-agent-workflows-against-doltgres-and-postgres-copies) |

The merge row's cause is found and its fix is being built (section 1, TODOS.md). No longer losses: server memory at 1M real embeddings, 7.7 GB after the run before and 3.1 GB since (section 6, "Memory at 1M"), under Milvus's 5.2 GB and Qdrant's 6.6 GB; unfiltered vector search at 1M, 4.69 ms before and 1.36 ms since (section 6, "Where the 1M search's time went"), now faster than every system that finds more than 75% of the true top 10; the 8.6 ms vector p99 of this rerun's first runs was a readiness bug, fixed in the same change (section 6); the protocol join + `GROUP BY` is 4.4 ms against Postgres's 6.9 ms since #22 (section 4); and the slow checkpoint and reopen at 100,000 worlds came from the loaded laptop (section 9).

## 1. Forks and concurrent writes (Phase 1 kill gate: 10× Postgres)

`cargo run --release --example bench -- 10 100 1000`, with 100k seed rows in main. Each agent forks main, writes 1,000 rows and merges back. Chronos runs in process; the others through their usual Rust or Python clients, on the same machine.

| agents | system | fork p50 | merge p50 | total time | agent rows/s |
|---|---|---|---|---|---|
| 10 | Chronos (memory) | 0.009 ms | 3.4 ms | 0.01 s | 1.23M |
| 10 | SQLite (file copy) | 6.3 ms | 190 ms | 0.79 s | 12.6k |
| 10 | Postgres 17 (template DB) | 3,553 ms | 68 ms | 4.1 s | 2.4k |
| 100 | Chronos (memory) | 0.009 ms | 33 ms | 0.07 s | 1.33M |
| 100 | SQLite | 16 ms | 1,064 ms | 10.0 s | 10.0k |
| 100 | Postgres 17 | 2,169 ms | 127 ms | 11.2 s | 8.9k |
| 1,000 | Chronos (memory) | 0.005 ms | 322 ms | 0.77 s | 1.29M |
| 1,000 | SQLite | 2,762 ms | 3,113 ms | 404 s | 2.5k |
| 1,000 | Postgres 17 | 2,133 ms | 128 ms | 102 s | 9.8k |

On disk (`chronos-disk`: write-ahead log, fsync on every merge into main, background checkpoints), agent rows/s:

| agents | one thread per agent | 25 workers | 64 workers |
|---|---|---|---|
| 10 | 396k | 405k | 409k |
| 100 | 459k | 621k | 555k |
| 1,000 | **207k** | 543k | 456k |

These on-disk runs are from before two fixes (section 8): #23 logs an open world's writes in batches, and #29 has each thread remember its last world rather than look it up under the branch map's lock. **With both, at 1,000 agents** (same machine, two rounds each, alternating; before is `main` at `368410af`; #23 alone from its own jobs):

| threads | before | #23 | #23 + #29 (`main`) | in memory (no log) |
|---|---|---|---|---|
| one per agent (1,000) | 4.96–5.08 s | 1.84–1.98 s | **1.76–1.80 s** (554k–568k rows/s) | 0.76–0.79 s |
| 64 | 2.20–2.26 s | 1.39–1.41 s | **1.40 s** (713k–716k rows/s) | 0.68 s |
| 25 | 1.91–1.97 s | 1.43–1.46 s | **1.40–1.41 s** (707k–712k rows/s) | 0.72 s |

At 100 agents: 0.22–0.24 s before, 0.10 s after. Merge p50 and fork p50 moved the other way (see the losses below): with one thread per agent, merge p50 went from 1,231–1,273 ms to 576–622 ms with #23 and 714–718 ms with both, and fork p50 from 3.9–4.9 ms to 65–73 ms (`bench/results/2026-09-28-coalesce/65-branchmap.out`).

**Verdict:** on disk, Chronos is 57–73× Postgres at 1,000 agents (554k–716k rows/s against 9.8k; 21–55× before #23 and #29, and 190–340× on the M2, where Postgres did 1.9k rows/s). The gate passes.

**How the baselines were set up:**
- **SQLite:** a branch is a file copy, and the merge is a full-table `EXCEPT`, because file copies don't track changes. Capped at 64 workers.
- **Postgres:** a branch is `CREATE DATABASE … TEMPLATE`. The template can't have connections, so agents fork a frozen copy rather than live main. The merge replays the agent's known writes, which is a generous shortcut. Capped at 25 workers because `max_connections` is 100.

### Against Dolt and Postgres, same machine (`bench/rivals/forks.py`)

`python bench/rivals/forks.py dolt 10 100 1000`, then `WORKERS=64 ONLY=chronos-disk cargo run --release --example bench -- 10 100 1000` straight after, twice (build `8db2f701`). **Dolt 2.3.5** runs as `dolt sql-server`, driven over the MySQL protocol: a fork is `CALL DOLT_BRANCH`, the writes go to the branch's revision database in batches of 100 rows (one transaction, then `DOLT_COMMIT`), and the merge is a real `CALL DOLT_MERGE` into main, retried if main moved under it (no retries were needed). 64 workers each.

| agents | Chronos on disk | Dolt | | fork p50: Chronos · Dolt | merge p50: Chronos · Dolt |
|---:|---:|---:|---:|---:|---:|
| 10 | 388k–400k rows/s | 17.9k | **22×** | 0.02 ms · 8–15 ms | 4.4–4.9 ms · 9–10 ms |
| 100 | 519k–531k | 26.5k–26.9k | **20×** | 0.06–0.08 ms · 43–54 ms | 13–19 ms · 247–295 ms |
| 1,000 | 457k | 28.3k–28.4k | **16×** | 0.008 ms · 24 ms | 11–12 ms · 855–877 ms |

These Chronos runs predate #23 and #29; with them, Chronos on disk at 64 workers does 713k–716k rows/s at 1,000 agents, **25×** Dolt (the Dolt runs weren't repeated). On the 4-vCPU VM (2026-09-24) the gap was 18×, 18× and 11×, and Chronos lost fork p50 at 1,000 agents (59 ms against 26 ms): that was 1,000 threads on 4 cores. At equal workers on 32 cores Chronos forks in 8 µs.

The same harness's Postgres mode (template databases, Python client, 25 workers) gave 2.1–2.2 s per fork and 9.6k–9.9k rows/s at 100 and 1,000 agents, as the Rust harness above did.

**Neon** (not rerun; 2026-09-27, from the 4-vCPU VM in Mumbai to Neon's nearest region, Singapore, 64 ms away): a fork is a branch with its own compute, ready when it answers `SELECT 1`; Neon has no merge, so an agent's rows are replayed into `main`; 5 agents at a time, the free plan's branch limit. 10 and 100 agents: fork p50 3.7 s and 2.3 s, 783 and 1,051 agent rows/s (`bench/results/2026-09-27-vector-load/20-neon.out`).

**Fast copies that don't merge (not measured by us):** two newer ways to copy a Postgres database quickly still stop at the copy.
- **Postgres 18 clones:** `CREATE DATABASE … TEMPLATE src STRATEGY FILE_COPY` with [`file_copy_method = clone`](https://www.postgresql.org/docs/18/runtime-config-resource.html) copies the files with `copy_file_range()`, which XFS or Btrfs can do by sharing blocks. It's still a whole database per copy, with a checkpoint before and after, and [no other session may be connected to the source while it copies](https://www.postgresql.org/docs/18/sql-createdatabase.html), so a live `main` can't be forked. There is no diff and no merge back.
- **Xata** (open source, Apache 2.0): [copy-on-write branches at the storage layer](https://xata.io/blog/open-source-postgres-branching-copy-on-write) under unmodified Postgres, instant whatever the size. Its docs answer "Can I merge branches?" with ["No"](https://xata.io/docs/core-concepts/branching): schema changes are applied to each branch by migration, and data doesn't flow back.

Neither returns an agent's changes as rows or its conflicts as data; Dolt and Chronos do.

**Losses and caveats:**
- **Merges on disk with one thread per agent.** At 1,000 agents, merge p50 is 714–718 ms against Postgres's 128 ms at 25 workers (1,231 ms before #23 and #29); the run takes 1.76–1.80 s (was 4.96–5.08 s) against 1.40 s at 25 workers. Merges into `main` queue behind each other: in memory, with no log, the p50 is 325 ms.
- **Forks under heavy load:** at 1,000 threads on disk, fork p50 is 65–73 ms and p99 229–233 ms with #23 and #29 (3.9–4.9 ms and 55–68 ms before). Forks take the branch map for writing and log while holding it, and 1,000 agents now reach their forks and merges together.
- **Merge p50 at 25 and 64 workers rose on disk** with #23 and #29: 4.1–4.8 ms to 25–26 ms at 25 workers, 11–16 ms to 64–66 ms at 64 (still 2–5× faster than Postgres's 128 ms). Agents now reach their merges sooner and queue on `main` together, and a merge logs its world's rows. The runs as a whole are 27–37% faster.

## 2. Vector search vs Postgres + pgvector (Phase 1 kill gate: 5× pgvector)

`N=500000 OVERSAMPLE=100 EF=300 cargo run --release --example search`: 500k × 384-dimension synthetic clustered vectors, top 10, 500 queries per filter mix.

Postgres got one documented tuning pass:
- `shared_buffers = 8GB` (2 GB on the 8 GB laptop), with the table and indexes prewarmed (`pg_prewarm`).
- HNSW with `ef_search = 300`.
- `hnsw.iterative_scan = relaxed_order`.

| filter | Chronos p50 | p99 | pgvector p50 | p99 | Chronos recall@10 | pgvector recall@10 |
|---|---|---|---|---|---|---|
| none | 0.32 ms | 0.74 ms | 1.09–1.13 ms | 3.0–3.7 ms | 99.7% | 96.7% |
| 10% | 0.93 ms | 2.01 ms | 1.17–1.22 ms | 3.0–3.2 ms | 100% | 96.3% |
| 1% | 0.43 ms | 0.51 ms | 7.5–7.7 ms | 14.4–14.7 ms | 100% | 80.3% |
| 0.1% | 0.14 ms | 0.17 ms | 3.9–6.8 ms | 4.4–6.9 ms | 100% | 100% |

pgvector ranges are two runs (before and after prewarming). At its default `ef_search` (40), pgvector took 0.59, 0.92, 7.1 and 6.8 ms at 82.2%, 86.2%, 78.2% and 100% recall.

**Verdict:** 1.3–47× pgvector at p50, with higher recall everywhere. **The 5× gate fails at the 10% filter** (1.3×); the other three pass. On the 8 GB laptop it was 24–64×: there pgvector had 2 GB of cache for its index and was partly disk-bound.

### At 1M × 384 (journal task `p1-search-1m`)

`N=1000000 OVERSAMPLE=100 EF=300 cargo run --release --example search`, the same tuning pass (8 GB fits the table and index; pgvector's index took 92 s to build, Chronos's graph 28 s):

| filter | Chronos p50 | p99 | pgvector p50 | p99 | Chronos recall@10 | pgvector recall@10 |
|---|---|---|---|---|---|---|
| none | 0.74 ms | 0.90 ms | 1.56–1.61 ms | 4.8–6.7 ms | 100% | 91.8% |
| 10% | 1.30 ms | 2.02 ms | 1.68–1.78 ms | 5.0–5.2 ms | 100% | 91.6% |
| 1% | 0.64 ms | 0.77 ms | 6.3–6.4 ms | 16.6–17.1 ms | 100% | 88.6% |
| 0.1% | 0.32 ms | 0.35 ms | 10.1–16.1 ms | 10.9–16.4 ms | 100% | 100% |

On `8db2f701` (before query threads were kept between queries) Chronos's unfiltered search took 2.8 ms here and lost to pgvector; the parallel scan's thread start-up was most of it.

**Losses and caveats:**
- **The 10% filter** is where pgvector comes closest: 1.3× at 500k, 1.3–1.4× at 1M.
- **Untuned Postgres** (128 MB `shared_buffers`) is slower still; the tuning pass above is the fair comparison.

### Side by side at 1M, 768 and 1,536 dimensions, every distance (2026-10-17, issue #34)

`bash bench/pgvector_vs.sh setup` then `bash bench/pgvector_vs.sh run` ([`bench/pgvector_vs.py`](bench/pgvector_vs.py)), on one otherwise idle GCP `c2d-standard-16` (16 vCPUs, AMD EPYC 7B13, 62 GB, Ubuntu 24.04). Postgres 17.11 with pgvector 0.8.7. Chronos `141013f` (`main` with #32's graph per distance and the 8-bit store), plus #191 where marked. One system at a time, the other's server stopped. Raw output: [`bench/results/2026-10-17-pgvector-vs/`](bench/results/2026-10-17-pgvector-vs).

**Setup.** Both get the same 1M rows and the same SQL from the same Python client (psycopg 3, vectors in binary through pgvector's `register_vector`, prepared statements): `ORDER BY embedding <op> $1 LIMIT 10`, optionally `WHERE tN = $2`. There are 1,000 held-out queries, run from 1 and from 8 connections at once. Recall@10 is against exact search by the same distance.

- **pgvector:** HNSW at its defaults (`m = 16`, `ef_construction = 64`), built with 15 parallel workers and 16 GB of `maintenance_work_mem`; `shared_buffers` is a quarter of memory. It's measured twice: at its default `hnsw.ef_search` (40), and at the smallest `ef_search` that reaches Chronos's recall (the sweep is in the output; 1,000 is the most pgvector allows).
  - Filtered searches use `hnsw.iterative_scan = relaxed_order`.
  - Every session has `enable_seqscan = off`: at a high `ef_search`, Postgres's planner otherwise reads the whole table instead of the index (1.76 s a query at 1M × 1,536).
- **Chronos:** at its defaults.

**Data:**
- **DBpedia:** 999,000 real OpenAI `text-embedding-ada-002` vectors (1,536 dimensions, unit length).
- **Synthetic 768:** 1M vectors of 768 dimensions, clustered in a 64-dimensional space and projected up, as real embeddings vary along far fewer directions than they have. Once at unit length, once scaled to lengths between 0.25 and 4, where cosine, L2 and inner product rank differently.

**Unfiltered, at the same recall** (p50 with 1 client; queries a second with 8):

| data | distance | Chronos | pgvector, matched `ef_search` | pgvector, default (40) |
|---|---|---|---|---|
| DBpedia 1,536 | cosine | **1.29 ms**, 3,675 q/s, 98.5% | 7.44 ms, 724 q/s, 98.7% (`ef` 400) | 1.59 ms, 2,332 q/s, 91.6% |
| synthetic 768, unit | cosine | **0.87 ms**, 4,700 q/s, 99.8% | 3.76 ms, 1,211 q/s, 98.2% (`ef` 1,000: never reached 99.8%) | 0.97 ms, 3,394 q/s, 87.9% |
| synthetic 768, unit | inner product | **0.87 ms**, 4,173 q/s, 99.8% | 3.60 ms, 1,302 q/s, 98.2% (`ef` 1,000) | 0.97 ms, 3,248 q/s, 89.6% |
| synthetic 768, mixed lengths | cosine | **0.89 ms**, 4,594 q/s, 99.8% (#191; 24.3 ms before) | 3.68 ms, 1,253 q/s, 97.9% (`ef` 1,000) | 0.98 ms, 3,389 q/s, 87.5% |
| synthetic 768, mixed lengths | L2 | **1.03 ms**, 4,092 q/s, 95.8% (#191; 22.2 ms before) | 5.87 ms, 970 q/s, 95.9% (`ef` 1,000) | 0.76 ms, 4,286 q/s, 80.8% |

**Filtered** (synthetic 768, unit, cosine; `WHERE` a tag that keeps 1%, 10% or 50% of the rows; inner product measured the same within 3%):

| filter | Chronos | pgvector, matched `ef_search` | pgvector, default (40) |
|---|---|---|---|
| 1% | **1.33 ms**, 3,248 q/s, 100% | 40.3 ms, 168 q/s, 100% (`ef` 300) | 4.83 ms, 1,004 q/s, 92.2% |
| 10% | **2.16 ms**, 1,003 q/s, 100% | 211 ms, 19 q/s, 100% (`ef` 600) | 1.54 ms, 2,445 q/s, 90.9% |
| 50% | **2.02 ms**, 902 q/s, 99.96% | 3.90 ms, **1,289 q/s**, 98.3% (`ef` 1,000) | 1.00 ms, 3,316 q/s, 87.6% |

**Building the index** (after the rows are loaded):

| data | Chronos | pgvector | pgvector's index on disk |
|---|---|---|---|
| DBpedia 1,536 | **94 s** | 349 s | 7.8 GB |
| synthetic 768, cosine | **37 s** | 127 s | 3.9 GB |
| synthetic 768, mixed, cosine | **74 s** (two graphs) | 128 s | 3.9 GB |

Loading 1M rows through 100-row `INSERT`s took about the same in both (Chronos 57–87 s, Postgres 49–95 s). Chronos's server peaked at 7.6 GB with DBpedia, while the graph was built, and held 3.2 GB after. Postgres's table and index on disk came to 15.8 GB.

**Verdict:** at the same recall, Chronos is 4.1–5.8× faster than pgvector unfiltered (p50, one client) and 3.2–5.1× in throughput with 8 clients. Under a 1% or 10% filter it's 30–100× faster: to fill k at Chronos's recall there, pgvector's iterative scan walks a much wider beam. pgvector at its default `ef_search` is about as fast as Chronos, or faster under a 10% or 50% filter, at 81–92% recall.

**Losses and caveats:**
- **Filtered searches from 8 clients scale poorly.** Under a 10% filter, 1 to 8 clients gives only 2.3× the throughput (444 to 1,003 q/s, p99 14.8 ms); under 50%, 1.8× (510 to 902 q/s, p99 20.4 ms). At 50% with 8 clients, pgvector at `ef_search` 1,000 serves more: 1,289 q/s at 98.3% recall, against Chronos's 902 at 99.96%.
- **L2 recall is lower:** 95.8% at Chronos's default beam, against 98–99.8% for cosine and inner product. `SET hnsw.ef_search` buys more.
- **Found and fixed during this run:** cosine and L2 on vectors of mixed lengths compared every row (24.3 ms and 22.2 ms) because their graph measured just under the bar for use. #191 gives them the inner-product graph's rule. The mixed-length Chronos rows above are from #191's branch, rerun on the same machine.
- **Synthetic data is not real data.** The first synthetic set, with independent noise in all 768 dimensions, held both systems to 70–78% recall at their defaults; real embeddings don't look like that, so it was discarded. The DBpedia rows are the real-data result.
- One client machine and one server: no network between them (both on localhost).

## 3. Hybrid search inside branches (Phase 3 targets: p99 < 5 ms at 1M, fork within 10% of main)

`N=1000000 cargo run --release --example find`: 1M rows, 64-dimension vectors, and a fork with 500 changed rows. Three runs:

| query | main p50 | main p99 | fork / main (p50) |
|---|---|---|---|
| filter (1%) | 0.048–0.050 ms | 0.09–0.10 ms | 0.91–0.95× |
| text | 0.37 ms | 0.40–0.41 ms | **1.12–1.13×** |
| vector + filter (20%) | 0.98–0.99 ms | 1.03–1.04 ms | 1.03–1.05× |
| vector, no filter | 1.34 ms | 1.44–1.45 ms | **1.55–1.58×** |
| filter + text + vector | 1.98–2.08 ms | 2.11–2.18 ms | 1.03–1.05× |
| repeated query | 0.008 ms | 0.010–0.012 ms | ~1.0× |

**Verdict:** the p99 target is met with room (at most 2.2 ms). **Forks within 10% of main is missed** for text (12–13% slower) and for unfiltered vector search (55–58% slower); filtered and hybrid searches in a fork are within 5%.

**Losses and caveats:**
- **Unfiltered vector search in a fork:** 2.1 ms against 1.34 ms in main. The ratio got worse as main got faster: on `8db2f701` it was 4.3 ms in main and 1.14–1.16× in the fork. Probably the fork's 500 changed rows searched on their own and main's copies of them dropped from its results; not profiled yet.
- **Only 64 dimensions at 1M here;** 384 dimensions at 1M are in section 2, and 1,536 in section 6.
- `N=1000000 DIM=64 ONLY=chronos cargo run --release --example search`: 0.83 ms at 97.9% recall with no filter, 0.12–0.59 ms at 99.7–100% with filters.

## 4. SQL over the Postgres protocol

`chronos serve` on one side and Postgres on the other, both over TCP on the same machine, with the Rust `postgres` driver (binary values) and `N=10000 cargo run --release --example sql`. The workload:
- 10,000 single-row inserts, each its own commit.
- 10,000 more rows in 1,000-row statements.
- 10,000 lookups by primary key.
- 1,000 updates by key.
- 50 queries of `age = $1 AND name LIKE $2` over 20,000 rows. Chronos answers these from its search index; Postgres has no index on `age`.

| machine | system | inserts/s | batch rows/s | lookup p50 | lookup p99 | update p50 | filtered query p50 |
|---|---|---|---|---|---|---|---|
| 32 cores | Chronos (`0f1a19af`, two rounds) | 2,395–2,543 | 290k–297k | 46–48 µs | 62–63 µs | 0.41–0.42 ms | <1 ms |
| 32 cores | Postgres 17 (`fdatasync`, Linux's default) | 2,286–2,320 | 222k–223k | 46 µs | 63–66 µs | 0.43–0.46 ms | 1–2 ms |
| 32 cores | Chronos, `synchronous_commit = off` (`8db2f701`) | 19,449 | 294k | 43 µs | 61 µs | 0.05 ms | <1 ms |
| M2 | Chronos | 341 | 125k | 40 µs | 85 µs | 3.0 ms | <1 ms |
| M2 | Postgres 16, flushing to disk like Chronos (`wal_sync_method=fsync_writethrough`) | 322 | 130k | 40 µs | 86 µs | 4.5 ms | 1 ms |
| M2 | Postgres 16, macOS default (`open_datasync`) | 9,275 | 484k | 40 µs | 54 µs | 0.12 ms | 1 ms |

The 32-core rows are the chronos-bench VM (section header), `bench/results/2026-09-28-rerun/54-final-chronos.out` and `03-sql.out`; `0f1a19af` is `main` at `368410af` with the vector fix of section 6 (before #22). Both sides flush every commit to disk.

**Verdict:** at equal safety, Chronos is level with Postgres on lookups and updates, and ahead on commits (3–11%), batches (about 30%) and filtered queries.

**Joins and grouping.** Run over the same 20,000 rows, plus a 20,000-row `sql_orders` table pointing at them. These are reads, so the durability setting doesn't matter.

| query | M2: Chronos (JSON rows) | M2: Chronos, one core | M2: Chronos | M2: Postgres 16 | 32 cores: Chronos `8db2f701` | 32 cores: Chronos `cf1c9f3a` | 32 cores: Postgres 17 |
|---|---|---|---|---|---|---|---|
| one order joined to its row by primary key | 62 µs | 62 µs | 62 µs | 56 µs | 50 µs | 52 µs | 50 µs |
| `count(*)` of orders joined to rows `WHERE age = $1` | 8.4 ms | 1.1 ms (with join ordering) | 1.1 ms | 2.6 ms | 1.2 ms | 1.0 ms | 2.3 ms |
| `GROUP BY age` with `count(*)`, `max(name)` | 10.0 ms | 5.5 ms | 2.2 ms | 8.1 ms | 2.6 ms | 2.0 ms | 3.1 ms |
| join of both tables, then `GROUP BY` with `sum` | 24.5 ms | 11.7 ms | 3.9 ms | 6.9 ms | 11.5 ms | 4.4 ms | 6.9 ms |

The M2 columns are the laptop runs this section began with. The 32-core columns are the chronos-bench VM (GCP c2d-standard-32, Linux, glibc builds, 2026-09-28), medians of two or three interleaved rounds of `N=10000 examples/sql.rs`. `8db2f701` is before the changes below, and `cf1c9f3a` has mimalloc (#17), helper threads kept between queries (#21) and small tables in more parts (#22).

**Packed rows.** SQL tables now store rows packed (see [SQL](docs/sql.md#storage)) instead of as JSON, which cut the cost of reading a column by more than half. `GROUP BY` now beats Postgres.

**Join ordering.** The filtered join now starts from the 222 rows `age = $1` picks through the index, then looks their orders up through the index on `ref`, so it never reads all 20,000 orders. That makes it 2.4× faster than Postgres.

**Batches.** Rows move through a query in flat batches (one buffer for many rows), each stored row is decoded into one reused buffer, and join and group tables use a fast hash. Whole-table joins went from 16.1 ms to 11.7 ms, and `GROUP BY` from 6.5 ms to 5.5 ms.

**Every core.** Big scans, hash-join probes and `GROUP BY` now split across the machine's cores (see [SQL](docs/sql.md#speed)). Whole-table joins went from 11.7 ms to 4.4 ms and now beat Postgres, and `GROUP BY` went from 5.5 ms to 2.4 ms.

**Bigger reports.** `cargo run --release --example report`: 200,000 rows in each table, Chronos in process against the same tables in Postgres 16 over local TCP (Postgres with its default of 2 parallel workers per query). Milliseconds per query, M2 with 8 cores (4 fast, 4 efficiency):

| query | M2: Chronos, one core | M2: Chronos, 8 cores | M2: Postgres 16 | 32 cores: Chronos `8db2f701`, one core | 32 cores: Chronos `8db2f701` | 32 cores: Chronos `cf1c9f3a` | 32 cores: Postgres 17 |
|---|---|---|---|---|---|---|---|
| `count(*)` of orders | 15.9 | 4.8 | 6.8 | 20.6 | 5.0 | 2.8 | 6.4 |
| `count(*)` of orders `WHERE amount > 500` | 30.0 | 6.6 | 8.4 | 33.7 | 5.5 | 4.0 | 8.7 |
| `GROUP BY age` with `count(*)`, `max(name)` | 55.5 | 15.1 | 45.6 | 73.0 | 12.0 | 9.8 | 19.6 |
| join of both tables, then `GROUP BY` with `sum` | 123.9 | 29.4 | 47.8 | 175.2 | 33.1 | 25.3 | 68.6 |
| `count(*)` of the join `WHERE name LIKE 'user 1%'` (111,111 users) | 107.1 | 22.5 | 29.4 | 129.6 | 25.3 | 18.6 | 45.2 |

The 32-core columns are the chronos-bench VM (GCP c2d-standard-32, Linux, 2026-09-28). The `8db2f701` and Postgres 17 columns come from the benchmark rerun (two rounds, `bench/results/2026-09-28-rerun/04-report.out` on the `bench-rerun` branch). The `cf1c9f3a` column is medians of three rounds. `cf1c9f3a` keeps helper threads between queries (#21) and splits small tables into more parts (#22), which changes nothing at 200,000 rows. The example runs Chronos in process with the system allocator, so #17's mimalloc isn't in these numbers. A core of this VM is slower than an M2 performance core: one core took 175.2 ms for the join against the M2's 123.9.

**Against DuckDB 1.5.5.** The same five reports in DuckDB, in process on a database file (`python bench/rivals/forks.py duckdb`: the same rows, loaded in the same 5,000-row statements, timed as `report.rs` times: best of 3 rounds of 5 after a warm-up), on the same VM, two runs; Chronos is the `0f1a19af` build of the rerun (`54-final-chronos.out`), Postgres 16 beside it:

| query | Chronos, 32 cores | Postgres 16 | DuckDB, 1 thread | DuckDB, 32 threads |
|---|---|---|---|---|
| `count(*)` of orders | 2.7–2.8 | 6.8–7.0 | 0.3 | **0.4** |
| `count(*)` of orders `WHERE amount > 500` | 4.0–4.1 | 8.8–8.9 | 0.7 | **0.6–0.7** |
| `GROUP BY age` with `count(*)`, `max(name)` | 9.5–9.9 | 19.5–19.6 | 6.8 | **4.4** |
| join of both tables, then `GROUP BY` with `sum` | 25.4–25.7 | 67.0–69.5 | 6.1–6.2 | **7.2–7.7** |
| `count(*)` of the join `WHERE name LIKE 'user 1%'` | 18.3–18.5 | 43.8–44.4 | 9.0–12.9 | **7.1–8.3** |

**A loss:** DuckDB is 2.2–7× faster than Chronos on every report (3.3–3.6× on the join + `GROUP BY`). It stores columns and runs vectorized code; Chronos stores rows, and decodes each to read a column. Chronos is 2.0–2.8× faster than Postgres 16 and 17 here. Columnar storage isn't planned for v0.1; row counts kept in the tree and per-page minimum and maximum values would narrow the gap on `count(*)` and range filters.

Before this round, on one core, those were 26, 37, 63, 214 and 217 ms. Walking the table without copying each key, a hash index with no list per value, and `LIKE` without allocating account for the one-core gains; the cores do the rest. The join's hash table is also built on every core (rows split by hash into partitions, each indexed on its own), and a scan drops columns only its own filter reads (here `name`, once `LIKE` has passed it).

**Joining while reading.** A two-table join no longer reads its first table whole before joining: each core reads a part of it, joins that part through the other table's hash index, and, for `GROUP BY` with exact aggregates, groups it right there. Neither the first table nor the joined rows are ever held in full. Join + `GROUP BY` went from 33.9 ms to 29.4 ms and the filtered join from 26.3 ms to 22.5 ms. Queries whose first table filters down to 1,000 rows or fewer still look their matches up by key (the 1.1 ms filtered join over the protocol is unchanged).

**Where the time goes now:** about a third of these joins is reading and indexing the other table (200,000 users), which joining while reading doesn't change.

**On 32 cores, Linux: glibc's malloc.** On the chronos-bench VM (GCP c2d-standard-32, glibc build of `8db2f701`, Postgres 17), the protocol join + `GROUP BY` took 11.5 ms against Postgres's 6.9 ms, and capping the server's threads (`CHRONOS_THREADS=4` to `32`) didn't change it. Three suspects were ruled out:
- **Not the protocol or prepared plans:** simple, extended and prepared protocol all took 11.4–11.9 ms (pgbench), and planning, sending and everything outside the join took under 0.1 ms.
- **Not rows waiting for a checkpoint:** 10.8 ms after an offline checkpoint, and 10.7 ms in process (`examples/report.rs`, `N=20000`, checkpointed).
- **Not the join strategy:** it was the planned hash join, grouping each part as it's joined.

`perf` showed page faults taking 18% of the query's CPU and `madvise` another 4%. glibc hands each query's big batches back to the kernel when they're freed, and the next query faults them in again. The same binary with `GLIBC_TUNABLES` keeping that memory (`mmap_threshold=32M`, no trim) took 8.3 ms. Linux builds now use mimalloc on glibc too (the musl builds already did). Medians of two interleaved rounds of `examples/sql.rs`, `N=10000`:

| | glibc malloc | mimalloc |
|---|---|---|
| join + `GROUP BY` | 11.5 ms | 8.1 ms |
| `GROUP BY age` | 2.6 ms | 2.2 ms |
| filtered join | 1.2 ms | 1.0 ms |
| batch rows/s | 254k | 294k |
| lookup p50 / p99, join by key | 45 / 64 µs, 50 µs | 44 / 60 µs, 50 µs |

On one core the join went from 16.5 to 12.9 ms.

**Smaller parts: tried first without kept threads.** At 20,000 rows the join split into only 4 parts (one per 4,096 rows), so at most 4 cores shared it. Splitting small tables into more parts didn't pay yet: with 1,024-row parts and no cap, the join took 9.7 ms on 32 threads against 6.2 ms on 8, because every step of a query started its helper threads afresh. Threads kept between queries remove that cost (below).

**Threads kept between queries.** A query's helper threads now wait for the next query's work instead of being started for each step (at most `CHRONOS_THREADS - 1` of them wait). Same VM, both builds with mimalloc, medians of two interleaved rounds:

| query | threads started per step | threads kept |
|---|---|---|
| protocol join + `GROUP BY` (20,000 rows, `examples/sql.rs`) | 8.5 ms | 7.4 ms |
| protocol `GROUP BY age` (20,000 rows) | 2.3 ms | 1.8 ms |
| in process, 200,000 rows: `count(*)` of orders | 5.0 ms | 2.8 ms |
| in process, 200,000 rows: `count(*) WHERE amount > 500` | 5.4 ms | 4.1 ms |
| in process, 200,000 rows: `GROUP BY age` | 11.9 ms | 9.8 ms |
| in process, 200,000 rows: join + `GROUP BY` | 33.6 ms | 25.9 ms |
| in process, 200,000 rows: join `WHERE name LIKE 'user 1%'` | 25.6 ms | 18.8 ms |

On one thread (`CHRONOS_THREADS=1`, no helpers) nothing changes: 12.7 against 12.9 ms.

**Small tables in more parts.** With threads kept, a table of 8,192 rows or more now splits into up to 16 parts of at least 1,024 rows (a bigger one still into one part per 4,096 rows). For such a table the parts are cut at runs of about 256 rows, several to a part. A stored run is one subtree, and subtrees vary in size: parts cut at single 1,024-row runs came out uneven, and a checkpointed 20,000-row `GROUP BY` got slower (2.8 to 3.6 ms) instead of faster. Same VM, medians of three interleaved rounds:

| query | 4 parts | up to 16 parts |
|---|---|---|
| protocol join + `GROUP BY` (20,000 rows, `examples/sql.rs`) | 7.2 ms | 4.4 ms |
| protocol `GROUP BY age` (20,000 rows) | 1.9 ms | 2.0 ms |
| in process, 20,000 rows: join + `GROUP BY` | 8.6 ms | 3.5 ms |
| in process, 20,000 rows: `GROUP BY age` | 2.8 ms | 1.6 ms |
| in process, 20,000 rows: join `WHERE name LIKE 'user 1%'` | 6.6 ms | 2.5 ms |
| in process, 50,000 rows: join + `GROUP BY` | 11.2 ms | 7.4 ms |
| in process, 200,000 rows: join + `GROUP BY` (the other four within 0.2 ms too) | 25.5 ms | 25.3 ms |

The protocol join + `GROUP BY` that started this, 11.5 ms against Postgres's 6.9 ms, now takes 4.4 ms.

**Why the macOS default is so much faster at commits:** it doesn't flush the drive's cache, so a power cut can lose commits it acknowledged. By default Chronos flushes the drive's cache (`F_FULLFSYNC`); `alter system set synchronous_commit = normal` makes it do what Postgres does there (a plain `fsync`), and `off` stops waiting for the disk at all. The Linux comparison is in section 8: it found a real gap (Chronos's log file grew on every commit), now closed.

**Other caveats:**
- **The protocol itself** costs about 31 µs per round trip over TCP for Chronos, and about 35 µs for Postgres over TCP (25 µs over a Unix socket, which Chronos doesn't offer yet).
- **Engine-only timings** are 2.8 µs for a lookup by key and about 1.4 µs per inserted row, plus about 0.4 µs per row to parse a long `VALUES` list.

### Correlated subqueries, joins on expressions and long `IN` lists (2026-10-02)

A CRM demo (six agents on one Chronos) showed minutes-long statements over a 150,600-row `leads` table. Alone, the plain `GROUP BY` ones were fast; the slow ones were correlated `EXISTS` / `NOT EXISTS`, a join on `lower(a.email) = lower(b.email)` and `x IN (select ...)` over a long list. The agents weren't waiting on each other: each has a session of its own in the server ([Several sessions on one database](#several-sessions-on-one-database-2026-10-14), below). [PR #72](https://github.com/Abhishekxdg/chronosdb-engine/pull/72) fixed them (see [SQL](docs/sql.md#speed)).

`python bench/sql-subqueries/bench.py` runs each statement alone on 150,600 rows (25,000 accounts of 6 leads in 10 regions; region R3 has 15,000 leads of 2,500 companies; a `PRIMARY KEY` on `id` and one index on `region`) and prints the wall time and a digest of the sorted rows. Run on an idle GCP e2-standard-4 (4 vCPUs, 16 GB, Ubuntu 24.04, cross-built Linux binaries), one run each, statements stopped at 450 s. Before is `main` at `b8624047`, after is `c7f90dda`. Raw output is in [`bench/results/2026-10-02-sql-subqueries/`](bench/results/2026-10-02-sql-subqueries).

| statement | before | after |
|---|---|---|
| `group by company having count(*) > 1`, one region | 0.07 s | 0.08 s |
| CTE of `max(created_at)` per company, joined back | 0.11 s | 0.19 s |
| `group by lower(email)`, one region | 0.09 s | 0.11 s |
| correlated `EXISTS` (a newer lead of the same company), 15,000 outer rows | 169 s | 0.17 s |
| `DELETE` with that `EXISTS` | 167 s | 0.25 s |
| correlated `NOT EXISTS` (the newest lead per company) | 173 s | 0.21 s |
| `id IN (select ... join on lower(l1.email) = lower(l2.email))` | over 450 s | 0.26 s |
| `company IN (15,000 values)` over the whole table | 27.4 s | 0.22 s |
| `company NOT IN (15,000 values)` | 28.0 s | 0.36 s |
| `EXISTS` on an unindexed column, every row as outer | 7.6 s | 0.96 s |
| `NOT EXISTS` on `email`, every row as outer | over 450 s | 1.16 s |
| `EXISTS` on the indexed `region` (15,000 rows per probe) | 330 s | 0.77 s |
| `EXISTS` with 243 distinct outer values against one 50,000-row bucket | 5.8 s | 2.9 s |

Every statement the old build finished returns the same rows on both builds (same digests). The three plain `GROUP BY` / CTE statements were never slow and are unchanged. The last row gains least: the old per-row path already kept its answer for each distinct outer value, so it ran 243 times, and the new one does the same work 243 times over a hash bucket. A correlated subquery whose equality is on an indexed column keeps running per outer row until that has taken half a second, then hashes (the `EXISTS` on `region` row).

### Several sessions on one database (2026-10-14)

An earlier edition of this page said the demo's agents queued because `chronos mcp` serves one request at a time. That was never measured, and it's wrong: each agent's `chronos mcp` joins the `chronos serve` that has the folder open and gets a session and a thread of its own there (`src/local.rs`).

`CHRONOS=target/release/chronos python3 bench/mcp-sessions/bench.py` starts one server and a `chronos mcp` process per session, as the demo's agents ran, and times fast calls in some sessions while session A runs something slow. **Not the 32-core machine:** an 8-core, 8 GB M2 laptop, a v0.1.3 release build of 2026-10-02, one run. Raw output is in [`bench/results/2026-10-14-mcp-sessions/`](bench/results/2026-10-14-mcp-sessions). Median / worst, in ms:

| while session A | another's read of main | another's write in its own world | another's write to main |
|---|---|---|---|
| nothing | 0.1 / 0.6 | 0.1 / 0.9 | 3.0 / 3.8 |
| sleeps 4 s | 0.6 / 2.3 | 0.6 / 2.3 | 4.0 / 5.6 |
| reads main for 3.4 s (a nested loop) | 0.2 / 1.1 | 0.2 / 1.6 | 3.0 / 4.9 |
| writes its own world for 1.7 s (a slow `UPDATE`) | 0.2 / 1.2 | 0.2 / 1.2 | 3.0 / 4.8 |
| the same, with a checkpoint meanwhile (it took 0.03 s) | 0.1 / 2.0 | 0.1 / 1.7 | 3.0 / 9.3 |
| writes main for 1.7 s (the same `UPDATE`) | 0.2 / 0.8 | 0.2 / 0.9 | 3.9 / **1,671** |

Queries big enough to be split across cores share the cores, and no more:

| while | `GROUP BY` over one region (15,000 rows) | `GROUP BY` over the table (150,000 rows) |
|---|---|---|
| nothing | 35 / 49 | 7.7 / 35 |
| 5 sessions loop a 138 ms `GROUP BY` over 1.5M rows | 131 / 259 | 85 / 265 |
| 1 session runs a 21 s join | 129 / 481 | 109 / 372 |
| 3 sessions run a join each (30 to 58 s) | 60 / 633 | 20 / 674 |

**Verdict:** sessions don't wait for each other's statements. Beside statements that take every core, a 35 ms query took 60 to 131 ms and never more than 0.7 s.

**What does wait:**
- **Writes to `main` behind a statement writing `main`:** the other session's 3 ms write took 1.67 s, all of that statement's time. Reads of `main` and writes in other worlds didn't wait. Merges into `main` weren't measured here; section 1's losses are about them.
- **Calls in one session:** two calls sent together are answered in order (`pg_sleep(2)` and `select 1` both at 2.00 s). An agent that sends tool calls in parallel gets them one after another.
- **The five looping sessions themselves:** 12 runs of the 138 ms `GROUP BY` took 4.8 to 10.0 s per session, against 1.7 s alone, on 8 cores shared by seven busy sessions.

**Not explained:** the demo's 5 to 8 minute calls of a plain `GROUP BY ... HAVING` over one region. Alone it takes 0.07 s (the table above this section), and here beside long statements under 0.7 s; it wasn't reproduced.

## 5. Crash safety

These are correctness checks, not speed, but they belong on the record. Each ran on the PR build and on `8db2f701`.

| Test | What it does | Result |
|---|---|---|
| Crash suite (`CRASH_RUNS=10000`) | Cuts the log at random bytes, with torn-write garbage and cleanup, then recovers | 10,000/10,000 recover exactly the operations that reached disk, on both builds (132 s each) |
| Deterministic simulation (`--features shuttle`, `SIM_ITERS=3000`) | Agents, direct writes, checkpoints and cleanup, all interleaved under controlled schedules | Passes on both (3,000 schedules each of its schedulers, 199–202 s). Earlier found a planted ordering bug within about 3,000 schedules. |
| Jepsen-style (`JEPSEN_KILLS=20`, three runs) | `kill -9` the server during concurrent fork/merge bank transfers | 60 kills and 31,397 acknowledged transfers on the PR build, 60 and 32,723 on `8db2f701`: none lost, every balance matched the ledger |

**What the Jepsen-style test found.** It found two real bugs before this release, and both are fixed:
1. **Money created from nothing.** A merge treated "both sides wrote the same value" as no conflict. Two transfers that each debited an account from 100 to 90 became one, so money appeared. Merges now use first-merge-wins for rows both sides wrote.
2. **Silent loss after a crash.** A branch's last write died with the process, the server restarted in milliseconds, and the client's merge of the now-empty branch returned success. Writes now return versions, merges check them, and branches open during a crash are flagged until confirmed.

After the fixes, 15 runs of 20 kills each passed on the laptop with no failures (300 `kill -9`s, 108,178 confirmed transfers), and the 6 runs here (120 kills, 64,120 transfers) did too.

## 6. Vector search vs other vector databases, on real embeddings

`bench/vector_dbs.py` (see its header). **Data:** real OpenAI embeddings (1,536 dimensions) of DBpedia entities, from Hugging Face's `KShivendu/dbpedia-entities-openai-1M`: its first two files (76,924 vectors) and all 26 (1,000,000). The last 500 are the queries and the rest are stored. **Filter:** each stored row gets a user (row % 100), so "one user" keeps 1% of the rows, as a memory layer such as Mem0 filters every search. **Method:** one client, one query at a time, query inputs prepared before timing, 20 warm-up queries, top 10, each system at its defaults. Recall@10 is measured against exact brute force over the same rows. **Each system ran alone,** with its server started fresh for its run and stopped after (`bench/results/2026-09-28-rerun/vec.sh`).

Chronos DB is queried through SQL over the Postgres protocol, exactly as Mem0's pgvector store queries it (`ORDER BY embedding <=> $1::vector LIMIT 10`, with `payload->>'user_id' = $2` for one user), after `CREATE INDEX ... USING hnsw`. pgvector and pgvectorscale get the same SQL; pgvector's index build gets `maintenance_work_mem` of 1 GB at 76k and 16 GB at 1M, so its graph fits in memory as its docs advise. Load time leaves out each client's own conversion of rows; "index" is what's left to wait for after the load (Qdrant, Weaviate, Chroma and Redis build as rows arrive, so theirs is in the load).

### 76,424 vectors

| system | load | index | all rows: p50 | p99 | recall@10 | one user: p50 | p99 | recall@10 | memory after |
|---|---|---|---|---|---|---|---|---|---|
| **Chronos DB** (SQL) | 4.7 s | 6.8 s | 0.72–0.74 ms | 2.1–2.7 ms | **99.98%** | **0.61–0.65 ms** | 2.1–2.3 ms | 100% | 1.14 GB |
| Redis 8.10 | 8.2 s | — | **0.46 ms** | **1.98 ms** | 75.0% | 1.09 ms | **1.23 ms** | 100% | 1.05 GB |
| pgvector 0.8.6 | 3.8–3.9 s | 46 s | 1.01–1.03 ms | 2.33–2.35 ms | 82.0% | 13.0 ms | 13.3–13.4 ms | 100% | — |
| Milvus 3.0.1 | 7.8 s | 6.8 s | 1.64 ms | 2.06 ms | 89.2% | 1.73 ms | 1.95 ms | 99.7% | 1.42 GB |
| Weaviate 1.39.7 | 21.8 s | — | 1.82 ms | 2.31 ms | 97.0% | 1.35 ms | 1.46 ms | 100% | 1.10 GB |
| Chroma 1.5.9 (embedded) | 26.8 s | — | 1.94 ms | 2.19 ms | 92.6% | 36.5 ms | 39.4 ms | 100% | — |
| pgvectorscale 0.9.1 | 3.8 s | 75.5 s | 1.95 ms | 2.32 ms | 97.5% | 15.1 ms | 26.1 ms | 74.9% | — |
| Elasticsearch 9.5.4 | 102.2 s | 0.7 s | 3.24 ms | 7.18 ms | 90.8% | 2.83 ms | 3.75 ms | 99.8% | 32.7 GB |
| LanceDB 0.39 (embedded) | **1.0 s** | 9.4 s | 3.30 ms | 3.79 ms | 85.0% | 4.66 ms | 5.15 ms | 97.0% | — |
| Qdrant 1.19.1 | 86.5 s | 1.0 s | 3.73 ms | 5.47 ms | 98.8% | 2.63 ms | 4.21 ms | 100% | 2.4 GB |
| OpenSearch 3.8.0 | 105.2 s | 44.3 s | 5.12 ms | 7.57 ms | 96.6% | 2.49 ms | 3.48 ms | 100% | 2.5 GB |

Chronos ranges are two runs of the PR build; the others ran once each, and pgvector twice. Memory is the server's resident memory once the run was over (Chronos peaked at 2.3 GB); "—" is in the benchmark's own process or not measured.

**Verdict:** the best recall of the eleven, and the fastest median of every system that finds more than 75% of the true top 10. Redis is faster at its defaults, at 75% recall (its default search beam, `EF_RUNTIME`, is 10).

### 999,500 vectors

| system | load | index | all rows: p50 | p99 | recall@10 | one user: p50 | p99 | recall@10 | memory after |
|---|---|---|---|---|---|---|---|---|---|
| **Chronos DB** (SQL) | 63.7 s | 58.7 s | 1.34 ms | **1.73 ms** | 98.6% | **1.19 ms** | 1.56 ms | **100%** | **3.1 GB** (7.7 GB peak) |
| Redis 8.10 | 208 s | — | **0.50 ms** | **2.23 ms** | 74.2% | 11.1 ms | 11.9 ms | 100% | 13.2 GB |
| Chroma 1.5.9 (embedded) | 586 s | — | 2.25 ms | 2.74 ms | 96.8% | 265 ms | 290 ms | 99.7% | — |
| pgvector 0.8.6 | 88.6 s | 427 s | 2.40 ms | 4.03 ms | 92.8% | 30.9 ms | 59.1 ms | 94.6% | — |
| Weaviate 1.39.7 | 362 s | — | 2.48 ms | 3.70 ms | 98.2% | 8.47 ms | 9.88 ms | 100% | 11.9 GB |
| LanceDB 0.39 (embedded) | **12.0 s** | 148 s | 3.43 ms | 3.95 ms | 85.0% | 28.0 ms | 29.4 ms | 96.7% | — |
| Elasticsearch 9.5.4 | 1,612 s | 129 s | 3.56 ms | 8.03 ms | 94.9% | 3.47 ms | 16.9 ms | 61.3% | 33.0 GB |
| Milvus 3.0.1 | 125 s | 35.8 s | 3.92 ms | 6.17 ms | 97.3% | 3.89 ms | 5.72 ms | 99.7% | 5.2 GB |
| Qdrant 1.19.1 | 1,372 s | 6.2 s | 6.67 ms | 10.57 ms | **99.0%** | 5.83 ms | 7.36 ms | 99.98% | 6.6 GB |
| pgvectorscale 0.9.1 | 90.3 s | 2,342 s | 8.33 ms | 13.44 ms | 96.6% | 51.4 ms | 103.6 ms | 69.8% | — |
| OpenSearch 3.8.0 | 1,431 s | 363 s | 10.06 ms | 12.28 ms | 98.5% | 4.83 ms | 20.8 ms | 80.6% | 8.9 GB |

**Verdict at 1M: the fastest search of every system that finds more than 75% of the true top 10, and the fastest filtered to one user.** Only Redis answers an all-rows search faster, at 74.2% recall; Qdrant alone finds more (99.0% against 98.6%), at 6.67 ms. Filtered to one user, Chronos is the fastest of all (1.16 ms; Elasticsearch 3.47 ms at 61.3% recall, Milvus 3.89 ms at 99.7%). Chronos's row is the 2026-09-28 build of "Where the 1M search's time went" below; before it, 4.69 ms at 99.3% recall, with seven systems faster.

**pgvector at matched recall.** Defaults trade recall for speed differently, so pgvector's search beam was raised on the same 1M table (`bench/rivals/pgvector_ef.py`, HNSW index rebuilt as above; the sweep was stopped after 400):

| pgvector `hnsw.ef_search` | all rows: p50 | p99 | recall@10 | one user: p50 | recall@10 |
|---|---|---|---|---|---|
| 40 (default) | 2.43 ms | 3.97 ms | 92.1% | 32.0 ms | 94.6% |
| 80 | 3.96 ms | 6.54 ms | 95.9% | 91.8 ms | 100% |
| 120 | 5.33 ms | 9.02 ms | 97.0% | 91.9 ms | 100% |
| 200 | 7.75 ms | 13.28 ms | 97.9% | 91.9 ms | 100% |
| 300 | 10.60 ms | 18.91 ms | 98.3% | 91.8 ms | 100% |
| 400 | 13.54 ms | 24.08 ms | 98.6% | 92.0 ms | 100% |
| *Chronos DB, defaults* | *1.36 ms* | *1.71 ms* | *98.6%* | *1.16 ms* | *100%* |

At Chronos's recall (98.6%, `ef_search = 400`) pgvector is 10× slower: 13.5 ms against 1.36 ms. A sweep of the other systems' settings hasn't been run.

### Where the 1M search's time went (2026-09-28)

The 1M run above was 4.69 ms at first. Profiled on the same machine (raw output and scripts: [`bench/results/2026-09-28-vec1m-profile`](bench/results/2026-09-28-vec1m-profile)), the search index itself took 2.33 ms of it (`examples/walk.rs`: the graph walked on 1-bit codes with a beam of 1,024, then 1,024 candidates rescored), and the server spent most of the rest fetching the 10 rows found: each lives in a leaf page of about 32 rows, and at 6 KB a row (1,536 numbers) a leaf is about 200 KB, read, decompressed and checked with blake3 whole to return one row (23% of the server's time was blake3 alone).

| change | engine alone: p50 / recall@10 | over SQL: p50 / p99 / recall@10 | server peak |
|---|---|---|---|
| before | 2.40 ms / 99.4% | 4.69 ms / 6.74 ms / 99.3% | 14.4 GB |
| the graph's default beam at most 400 (1,024 on this data: none reached the self-test's 99.5%) | 1.03 ms / 98.6% | 3.39 ms / 5.30 ms / 98.6% | |
| the index build holds each vector once (copied into one buffer as rows are read) | | | 10.3 GB |
| the walk fetches a node's new neighbours before measuring any | **0.84 ms / 98.6%** | | |
| leaves of rows of 1 KB or more end at about 16 KB, not every ~32 rows; rows unpacked into the index a part at a time | | **1.36 ms / 1.71 ms / 98.6%** | **9.0 GB** |

**The speedup by cause, at matched recall.** The beam cap trades recall for speed, so each change was also run alone over SQL, back to back on the same machine (`40-`, `41-`, `42-vec1m-*.out`):

| build | beam | all rows: p50 / p99 | recall@10 | one user: p50 | peak |
|---|---|---|---|---|---|
| main (v0.1.2) | its default (1,024 here) | 4.75 / 6.75 ms | 99.3% | 3.59 ms | 14.4 GB |
| main | 400 (`SET hnsw.ef_search`) | 3.57 / 5.56 ms | 98.5% | 3.61 ms | 14.5 GB |
| this change | 1,000 (`SET hnsw.ef_search`) | **2.30 / 3.07 ms** | **99.3%** | 1.14 ms | 9.0 GB |
| this change | its default (400) | **1.36 / 1.71 ms** | 98.6% | 1.16 ms | 9.0 GB |

At the old recall (99.3%) the leaves, the prefetch and the memory changes make it 2.1× faster (4.75 to 2.30 ms), which is already faster than Weaviate (2.48 ms at 98.2%); only Chroma (2.25 ms at 96.8%) and Redis (0.50 ms at 74.2%) are faster, with less recall. At the new recall they make it 2.6× faster than main with the same beam (3.57 to 1.36 ms), and 9.9× faster than pgvector raised to the same recall (13.5 ms at 98.6%; pgvector's sweep never reached 99.3%). The one-user search is a scan, which the beam doesn't touch: its 3.1× is the leaves alone.

The engine-alone column is two rounds of `examples/walk.rs`, main against the branch back to back (`20-walk-prefetch.out`). With the fetches gone, the server's time is the search itself: the graph walk about half, rescoring the candidates 16%, the SQL layer's own distances for the 10 rows 6%, fetching them about 2% (`30-vec1m-sql.out`). One user's search, a scan of 1% of the rows, went from 3.68 ms to 1.16 ms from the smaller leaves alone.

**Smaller leaves for wide rows** change where leaves end, so pages are laid out differently from here on. Rows under 1 KB end their leaves exactly where they did, so narrow tables keep their pages. A database written before reads, diffs, merges and verifies the same (`tests/layout.rs`, on a database written by v0.1.2), and edits on its trees reuse their untouched subtrees, since every place a leaf ended before is still an end. The first edit to an old wide leaf rewrites it as several smaller ones: about the bytes the rule before wrote for it, and edits after that cost what they do on a tree built now (`prolly::tests`). No format version changes: reading doesn't depend on where leaves end.

### Memory at 1M (2026-09-29)

The index held every vector as 4-byte floats, 6.1 GB of the 7.7 GB the server kept after the 1M run. A vector column now keeps floats only while it holds at most 64M numbers, and 8 bits a number above that, each vector with its own scale. Ranked by 8-bit numbers alone, the 1-bit scan found 94.5% of the true top 10 however many candidates it rescored (16, 32 or 64 per result; 99.98% with floats), because DBpedia's near-duplicate entities differ by less than the rounding (`20-walk.out`). So an 8-bit index proposes twice the rows asked for and ranks them from the rows' own vectors, read on the free cores. Same machine, back to back, over SQL ([`bench/results/2026-09-29-vec-int8`](bench/results/2026-09-29-vec-int8)):

| build | all rows: p50 / p99 | recall@10 | one user: p50 / recall@10 | index build | after the run | peak |
|---|---|---|---|---|---|---|
| floats (main, `31-`) | 1.32 / 1.67 ms | 98.7% | 1.14 ms / 100% | 133.5 s | 7.66 GB | 9.07 GB |
| 8-bit alone (`30-`) | 1.33 / 1.65 ms | 93.8% | 1.18 ms / 95.9% | 58.8 s | 3.06 GB | 7.67 GB |
| 8-bit, 4x candidates ranked exactly, one at a time (`40-`) | 2.33 / 2.85 ms | 98.7% | 2.32 ms / 100% | 58.5 s | 3.11 GB | 7.61 GB |
| 8-bit, 4x, on the free cores (`44-`) | 1.44 / 1.95 ms | 98.7% | 1.45 ms / 100% | 58.6 s | 3.09 GB | 7.80 GB |
| **8-bit, 2x, on the free cores (`45-`, the default)** | **1.34 / 1.73 ms** | **98.6%** | **1.19 ms / 100%** | **58.7 s** | **3.09 GB** | **7.67 GB** |

The index builds 2.3 times faster too: its graph is built on 8-bit numbers (44 s against 118 s in `examples/walk.rs`), and its self-test gives 97.5–97.7% as it did with floats. At the recall of the old default beam (`SET hnsw.ef_search = 1000`) it takes 2.24 ms for 99.3% of the true top 10, against 2.30 ms for 99.3% with floats (`46-`).

### The p99 this rerun found, and fixed

The first 76k runs gave Chronos a p99 of 7.8–8.6 ms against a 1 ms median, even with Chronos running alone. Over three passes of the same 500 queries, the slow ones were all in the first pass and weren't the same queries as the slowest later ones (correlation 0.06; `19-p99diag.out`): p99 8.0 ms, then 1.09 and 1.08 ms. The cause: after an index builds its HNSW graph, it tests it with 128 exact searches spread over every core, and the server said its graphs were built (`search_graphs_building` 0) as soon as the graph existed, before that test ended. So the benchmark, which waits for that signal, timed Chronos's first searches against the test; every other system was timed after its own indexing ended. A graph now counts as built once it has tested itself (`src/search.rs`). With the fix the p99 is 2.1–2.7 ms and the index takes 6.8 s instead of 5.2 s: the wait now includes the test.

### How it got here (8 GB M2 laptop, 76k vectors)

The first edition's table: one run, the five systems one after another on a loaded laptop with 9–10 GB of swap in use. It doesn't compare with the tables above.

| system | load | index | all rows: p50 | p99 | recall@10 | one user: p50 | p99 | recall@10 |
|---|---|---|---|---|---|---|---|---|
| **Chronos DB** (SQL) | 42.0 s | 35.7 s | **1.00 ms** | 4.89 ms | **99.98%** | **0.75 ms** | 5.62 ms | 100% |
| LanceDB (embedded, IVF_HNSW_SQ) | 1.2 s | 23.9 s | 1.71 ms | 3.20 ms | 85.4% | 2.44 ms | 3.89 ms | 97.1% |
| Chroma (embedded) | 85.9 s | (in load) | 3.44 ms | 9.35 ms | 93.3% | 75.5 ms | 136 ms | 100% |
| Postgres 17 + pgvector 0.8.4 | 75.0 s | 252.0 s | 4.06 ms | 6.89 ms | 82.5% | 10.1 ms | 14.8 ms | 100% |
| Qdrant 1.19 (Docker) | 144.7 s | 14.5 s | 5.14 ms | 17.9 ms | 93.6% | 4.22 ms | 11.3 ms | 100% |

**How it got here** (all on this data, same laptop):

| Chronos DB change | all rows: p50 | recall@10 |
|---|---|---|
| pgvector SQL, comparing every row | ~350 ms (at 20k × 768) | 100% |
| `ORDER BY` through the search index's graph | 15.3 ms | 95.6% |
| reading only the rows returned, by key | 4.7 ms | 94.0% |
| vectors stored as 4-byte floats, not JSON | 1.3 ms | 91.0% |
| exact 1-bit scan unless the graph proves itself (it scored 97%) | **1.0 ms** | **99.98%** |

**Why not the graph on this data:** the graph walked on 1-bit codes answers in about 0.35 ms at a beam of 400, but finds only 92.8% of the true top 10: DBpedia holds many near-duplicate entities, and a graph navigates poorly among them. The 1-bit scan with full-precision rescoring finds 99.98% in about 0.6 ms (`examples/walk.rs`). Chronos DB measures its graph when it's built and uses it only if it scores at least 98% against the scan; on synthetic clustered vectors (section 2) it scores 99.5% and is used.

**Losses and caveats:**
- **Unfiltered search at 1M** trades 0.7 points of recall for its speed: the graph's default beam is at most 400 now (it was 1,024 on this data). `SET hnsw.ef_search = 1000` gives back about 99.4% of the true top 10 (a beam of 1,024 found 99.4%, `examples/walk.rs`).
- **Memory:** at 1M the Chronos server holds 3.1 GB after the run, against 6 GB of raw vectors, and peaks at 7.7 GB while it builds the index ("Memory at 1M" below); at 76k, 2.3 GB at its peak (that column keeps floats: it's under the 64M-number limit).
- **The 76k runs weren't rerun** after the 1M changes; they may be faster now (smaller leaves for wide rows), not slower.
- **Loading:** LanceDB takes an Arrow table in its own process (1.0 s at 76k, 12 s at 1M); Chronos gets rows over the Postgres protocol, in binary (4.7 s, 64 s).
- **In-process vs over the network:** LanceDB and Chroma answer in the benchmark's own process; the others over TCP (Chronos, Postgres) or HTTP and gRPC.
- **Elasticsearch** gives itself up to half the machine's memory by default (32–33 GB here), and **Qdrant**'s loads include JSON over HTTP, per its default client: 23 minutes at 1M.
- **What only Chronos DB was asked to do:** every row is also in a world that can fork in about a microsecond (section 1); no other system here can branch.

## 7. Undoing an agent

`ROWS=500000 AGENT=100000 OTHERS=50000 cargo run --release --example undo_agent`: 500k rows in main. An agent changes 100k of them straight in main (no world, no approval), in ten 10k-row statements. Then a person changes 50k other rows and 100 of the agent's.

| step | time | on the M2 |
|---|---|---|
| load 500k rows | 3.1 s | 12.0 s |
| the agent's 100k changes | 1.0 s | 12.6 s |
| `UNDO AGENT bot SINCE ... SKIP CHANGED` | 1.47 s | 6.2 s |

**Result:** 99,900 rows put back and 100 left as the person changed them. None of the agent's values remain, and all 50,100 of the person's changes are kept.

**Where the time goes:** the undo rebuilds history from the checkpoint before the moment named, then replays the log up to now. Its cost grows with how much was written since that checkpoint, not with the size of the database. Undoing something from weeks ago with a busy log in between will take longer; indexing changes by agent would fix that.

## 8. The losses, rerun on Linux

### On-disk writes at 1,000 agents (2026-09-28)

**Machine:** GCP c2d-standard-32 (32 vCPUs, 128 GB, pd-ssd), glibc release builds, the machine's one-job queue. `examples/bench.rs`, `ONLY=chronos-disk`, 1,000 agents each forking `main`, putting 1,000 rows one at a time and merging.

**The loss:** 5.0 s with one thread per agent, against 0.79 s in memory; 1.95 s at 25 workers against 0.72 s.

**Cause:** every put to a world appended its own record to the log: a million appends through one lock. `perf` (context switches by call chain): 70% waited for the log's lock, 26% for the branch map's. Letting a waiting thread spin before sleeping gained 8% at 1,000 threads and lost 7–12% at 25 and 64, so it was backed out.

**Fix:** a world other than `main` keeps its writes (up to 64 KB) and logs them as one record, before anything that depends on them: a fork from it, a merge of or into it, a discard, another agent's write to it, a checkpoint, a read of history or the past, a backup, shutdown. The record keeps each write's time and rows, so replay rebuilds versions write by write and `AS OF` stays exact to the millisecond. A crash can now lose a world's writes even after a later `main` commit was synced; worlds become durable when they reach `main`, as before, and open worlds are flagged.

| setup | before | after |
|---|---|---|
| one thread per agent: wall | 4.96–5.06 s | **1.84–1.98 s** |
| one thread per agent: merge p50 | 1,231–1,263 ms | 576–622 ms |
| 64 workers: wall | 2.20–2.25 s | **1.39–1.41 s** |
| 25 workers: wall | 1.94–1.97 s | **1.43–1.46 s** |
| 100 agents: wall | 0.22 s | 0.10–0.11 s |

**Then #29:** each thread remembers the last world it looked up (a weak reference, checked against a retired flag that merges and discards set before the world leaves the map), so a put no longer takes the branch map's lock. Waits there fell from 67% to 29% of context switches; fsync is now 20% and `merge_with` 10%. At 1,000 threads the run went to 1.76–1.80 s (554k–568k rows/s); at 25 and 64 workers, 1.40–1.41 s. Merge p50 at 1,000 threads rose to 714–718 ms, and fork p50 to 65–73 ms (`bench/results/2026-09-28-coalesce/65-branchmap.out`).

**What's left:** 1.8 s on disk against 0.76 s in memory at 1,000 threads. Forks and merges still take the branch map for writing and append to the log while they hold it; the 29% of waits left in `Core::branch` are probably each agent's first lookups while 1,000 forks take the map at the start. Logging a merge's batch before it takes its locks made the 1,000-thread run slower (2.35 s), so it isn't in. Next would be not logging while holding the map for writing, or sharding the map.

### The losses of sections 1, 4 and 6

*History (2026-09-24, a 4-vCPU VM).* This section reran the laptop's losses on a small Linux VM and recorded the fixes they led to. Every number in it is from that VM; where each loss stands now is in sections 1, 4 and 6, and in "Where Chronos DB loses" at the top.

**Machine:** GCP e2-highmem-4 (4 vCPUs, 32 GB, x86-64, Ubuntu 24.04, balanced persistent disk), glibc builds, no swap in use. **Before** is `main` at `0b4f867`; **after** is the `losses` branch at `b5ae045`. Each harness ran against both builds back to back, under the machine's shared benchmark lock, starting with a load average under 1. **Others:** Postgres 17.11 and pgvector 0.8.6 at their defaults (on Linux, `fdatasync` on commit, which flushes like Chronos), and LanceDB 0.39 embedded.

**Where each loss stands:**

| loss (from sections 1, 4, 6) | cause found | now |
|---|---|---|
| merge median at 1,000 agents: 736 ms vs 348 ms | the setup: 1,000 Chronos threads merging at once against 25 Postgres workers | at 25 workers each: Chronos 46 ms, Postgres 1,157 ms |
| vector p99: 4.89 ms vs LanceDB 3.20 ms | swap on an 8 GB laptop; the x86 build had no `POPCNT` (a software bit count) | Chronos 6.38 ms, LanceDB 17.07 ms |
| load + index: 77.7 s vs LanceDB 25.1 s | Python formatting floats inside the timed load; the index turning every vector into text and back | load 20.0 s + index 44.8 s = 64.8 s; LanceDB 2.0 s + 86.2 s = 88.2 s |
| commits: 341/s vs Postgres's 9,275/s (macOS) | macOS: Postgres doesn't flush the drive's cache. Linux: Chronos's log file grew on every commit | Linux: Chronos 644–806/s, Postgres 591–656/s |
| single-key join: 62 µs vs 56 µs | per-query planning and schema work, more than the join itself | Chronos 71 µs, Postgres 92 µs (medians of 5 interleaved rounds) |

**Merges.** `examples/bench.rs`, 1,000 agents each writing 1,000 rows over 100k seed rows. `WORKERS=25` caps Chronos at the Postgres run's 25 workers.

| setup | before p50 | after p50 | after p99 |
|---|---|---|---|
| Chronos in memory, one thread per agent | 1,788 ms | 1,486 ms | 2,702 ms |
| Chronos in memory, 25 workers | 49.8 ms | 45.8 ms | 126 ms |
| Chronos on disk, 25 workers | 68.3 ms | 68.1 ms | 172 ms |
| Postgres 17, 25 workers | | 1,157 ms | 2,077 ms |

A merge now reads both values of each changed row from the overlays' own diff instead of looking every key up twice. That halved a 1,000-row merge on the M2 (3.9 to 2.0 ms), but moved it much less here. What merges still hold the lock for is setting each row in main's map and checking it hasn't changed: about 2 µs a row, in order. Postgres is slower on this machine than on the M2 (its merge replays 1,000 upserts in one transaction, then commits on a network disk).

**Commits.** `examples/sql.rs`, one client, 10,000 single-row inserts, each its own commit.

| | inserts/s | update p50 |
|---|---|---|
| Chronos before | 258 | 3.4 ms |
| Chronos after | 614 | 1.8 ms |
| Postgres 17 (`fdatasync`) | 638 | 1.3 ms |
| Chronos after, `synchronous_commit = off` | 11,812 | 0.08 ms |

**Why Chronos was slow at commits on Linux:** each commit did exactly one `fdatasync` (checked with `strace`), but on a log file that had just grown. The file system then also has to commit the new file size to its journal. On this disk, 4 KB writes each followed by `fdatasync` ran at 351/s when appending and 1,311/s when overwriting space already written. Postgres fills its WAL segments with zeros ahead of time for this reason. The live log segment now does the same, 4 MiB ahead (see `disk.rs`).

**Caveat:** another job shared the CPU during this section. Chronos's reads here were about twice as slow as in the runs before and after, while its commits are bound by the disk rather than the CPU. In a first run with the machine busy (load average 9), before the change, Chronos did 384–392/s and Postgres 965/s.

**Rerun of this section:** before, after and Postgres back to back, twice. The machine was busier in the first round, so compare within a round.

| round | Chronos before | Chronos after | Postgres 17 |
|---|---|---|---|
| 1: inserts/s | 229 | 644 | 591 |
| 1: update p50 | 4.4 ms | 1.7 ms | 1.6 ms |
| 2: inserts/s | 342 | 806 | 656 |
| 2: update p50 | 2.8 ms | 1.6 ms | 1.4 ms |

**Joins.** One order joined to its row by key, prepared once, with the Rust `postgres` driver. The three ran interleaved: 5 rounds of 8 seconds each, over 20,000 rows in each table. The "after" build is `461e896`.

| | p50 per round | median | best |
|---|---|---|---|
| Chronos before | 163, 91, 126, 109, 97 µs | 109 µs | 91 µs |
| **Chronos after** | 73, 70, 71, 66, 71 µs | **71 µs** | **66 µs** |
| Postgres 17 | 92, 69, 69, 92, 125 µs | 92 µs | 69 µs |

**What changed.** A profile of this query showed the server spending more time planning than joining: loading and cloning each table's schema, building the plan, and copying the whole query to fold vector literals it didn't have. Now:
- **A prepared plain `SELECT` plans once.** "Plain" means tables with schemas, no subqueries, and no `AS OF`, set-returning functions or `WITH` in `FROM`. It keeps its tables and plan between executions. Each execution checks that the tables' stored schemas, the world and the parameter types are the ones it planned with, so a column added, a table made again or another world's schema plans afresh (`tests/sql.rs`, `prepared_plan_follows_schema_changes`).
- **Queries without a distance operator skip the vector-folding copy.**
- **Statements and bound parameters are shared**, not copied on every execute, and trace text is built only when tracing is on.

What remains in the profile is the two key lookups the join needs.

**Not done:** clients that describe every execution (psycopg does; the Rust driver doesn't) still have the result's columns worked out each time, which costs about as much as the query itself.

**Vectors.** `bench/vector_dbs.py`, the section 6 data (76,424 × 1,536). Each client's conversion of rows is now left out of load time, and SQL rows go 100 per `INSERT`.

| system | load | index | all rows: p50 | p99 | recall@10 | one user: p50 | p99 | recall@10 |
|---|---|---|---|---|---|---|---|---|
| Chronos DB before | 20.9 s | 92.0 s | 3.46 ms | 8.37 ms | 96.3% | 2.24 ms | 8.82 ms | 100% |
| **Chronos DB after** | 20.0 s | **44.8 s** | **3.38 ms** | **6.38 ms** | **96.8%** | **2.41 ms** | **7.57 ms** | 100% |
| LanceDB 0.39 (embedded) | **2.0 s** | 86.2 s | 12.04 ms | 17.07 ms | 86.4% | 16.01 ms | 20.96 ms | 97.0% |
| Postgres 17 + pgvector 0.8.6 | 35.4 s | 159.1 s | 3.80 ms | 7.22 ms | 83.9% | 29.17 ms | 37.35 ms | 100% |

**Index build:** the search index used to get every row as JSON, with each vector written out as decimal text and parsed back (about 117 million floats each way here). It now takes vectors as stored. The build halved, and at 20,000 rows it went from 10.1 s to 2.8 s.

**Still losing:**
- **Loading:** LanceDB takes an Arrow table in its own process, while Chronos DB gets rows over the Postgres protocol. These runs sent vectors as text; the harness now sends them in binary (pgvector's `register_vector`), which hasn't been rerun here. `COPY` would be next.

**Other caveats:**
- **Recall in these runs was 96–97%**, against 99.98% on the M2. On this data the graph passed its self-test here, and then served searches it was worse at. Since `62620e2` the self-test uses probes close to, but not on, stored vectors, and the bar is 99%. The graph no longer passes it here, and the latest build finds 100% of the top 10, the same as with the graph turned off (`CHRONOS_VECTOR=flat`), in three runs.
- **Same machine, back to back:** the old and new builds share this machine and harness. This machine is slower per core than the M2, so compare across rows, not with the M2's tables above.

## 9. Many worlds: 10 to 100,000

`SWEEP=1 ROWS=100000 cargo run --release --example worlds`: for each size, a fresh database on disk with 100,000 rows in main, then that many worlds forked from main, each writing one row of its own. Load average about 3. Memory is the process's resident memory added by the worlds; CPU is user + system time for the forks and writes.

| worlds | fork p50 | fork p99 | first write p50 | write p99 | point query, main / world | aggregate, main / world | disk after checkpoint | disk per world | memory added | per world | CPU | checkpoint | reopen |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10 | 2.0 µs | 17 µs | 8.8 µs | 99 µs | 16 / 15 µs | 5.1 / 4.7 ms | 7.5 MB | 503 B | 0.0 MB | — | 0.00 s | 16 ms | 3 ms |
| 100 | 1.8 µs | 11 µs | 8.0 µs | 27 µs | 15 / 16 µs | 4.8 / 4.6 ms | 7.6 MB | 521 B | 0.0 MB | — | 0.00 s | 18 ms | 3 ms |
| 1,000 | 1.7 µs | 7 µs | 7.9 µs | 13 µs | 21 / 14 µs | 4.8 / 4.7 ms | 8.1 MB | 551 B | 0.3 MB | 0.3 KB | 0.01 s | 20 ms | 4 ms |
| 10,000 | 1.5 µs | 3.5 µs | 8.1 µs | 13 µs | 21 / 21 µs | 4.8 / 4.7 ms | 13.1 MB | 557 B | 1.4 MB | 0.1 KB | 0.11 s | 72 ms | 14 ms |
| 100,000 | 1.4 µs | 3.5 µs | 8.2 µs | 16 µs | 23 / 21 µs | 5.0 / 4.8 ms | 64.0 MB | 564 B | 103 MB | 1.0 KB | 1.08 s | 868 ms | 166 ms |

The point query is `select name from t where id = 5`; the aggregate is `select n, count(*), sum(id) from t group by n` over all 100,000 rows. Both are averages of 20 runs after one warm-up.

**Verdict:** a fork costs about 1.5–2 µs at every size, and a world with one changed row costs about 560 bytes on disk and 1 KB of memory. A query in one of 100,000 worlds is as fast as in main. Checkpoint and reopen grow about linearly: 0.87 s and 0.17 s for 100,000 worlds.

**Changed from the laptop:** the M2 run (load average 70–90, 6 GB of swap in use) took 8.3 s to checkpoint and 3.5 s to reopen 100,000 worlds, 34× and 49× its 10,000-world times, and was listed as a loss. On a quiet machine it's 12× for 10× the worlds: the loaded machine, not the code. Memory per world was 3.8 KB there (the macOS footprint, which counts differently from resident memory here).

**Caveats:**
- **Small sizes are noise.** At 10 and 100 worlds the memory added is below what can be resolved.
- **One row per world.** Worlds that change more pay for what they change (section 10).

### Discarding 99,000 of 100,000 worlds

`DISCARD=one|many|expire ROWS=10000 cargo run --release --example worlds` (build `8db2f701`): 100,000 worlds forked from main (10,000 rows), each writing one row, then all but 1,000 discarded: one by one (`discard`), in one call (`discard_many`), or by the expiry sweep after `expire` on each.

| | now | 4-vCPU VM, before the fix | 4-vCPU VM, after |
|---|---|---|---|
| one by one | **0.35 s** (3.5 µs each, 0.5 s CPU) | 54.8 s (554 µs each) | 0.93 s (9.4 µs each) |
| in one call | **0.30 s** (3.0 µs each) | — | 0.51 s |
| expiry sweep | **0.51 s** for 99,000 (5.2 µs each) | 37.9 s for 56,181 | 1.25 s |
| `world(id)` lookup | under 1 µs | 8.7–21.5 ms | under 1 µs |
| memory after, then after a checkpoint and `malloc_trim` | 249–364 MB, then 54–91 MB | 1,305 MB | 985 MB, then 47–59 MB |

**Why:** each discard used to scan every live world for forks under the branch map's write lock, and finding a world by ID scanned them all too. Each world now keeps its live fork count and depth, the map indexes worlds by ID, and `discard_many` drops a batch under one hold of the map and the log (deepest first, one Discard record each). `DROP WORLD ... CASCADE`, `SIMULATE`'s losers and the expiry and idle sweeps go through it. Retired worlds leave the search caches, and a checkpoint hands freed memory back to the system on glibc.

**The 10,000-world simulation's discards** (section 10), which on the 4-vCPU VM didn't get faster with this change (709–1,130 ms per round), now take 218–310 ms per round of 9,900–9,999 losers.

## 10. The ultimate test: one real state, 100,000 worlds, a learning loop, one merge

`examples/simulate.rs` is a supply-chain optimization. **Main holds the real state:** warehouses, products (price, cost, lost-sale penalty, ordering cost, lead time of 1–5 days), stock, a seasonal, trending, noisy demand forecast from a seeded generator, and the reorder policy in use today (reorder at lead-time demand, order a week's worth).

**Round 1:** fork WORLDS worlds from main. Each gets its own policy, derived from its index and the seed: safety stock in days of demand for fast and for slow movers, and order size in days of demand. Each world runs the simulation in SQL inside itself. One `WITH RECURSIVE` query steps every SKU through DAYS days (morning order if stock plus pipeline is at or below the reorder point, arrivals, demand sold or lost, pipeline moving up); its results are written as orders and as the end-of-round stock. So every world really diverges. A SQL score (margin on units sold, minus lost sales, holding and ordering costs, in cents) evaluates it, the best 1% are kept and the rest are discarded.

**Rounds 2 and 3, the learning loop:** 100 children are forked from each winner (worlds of worlds). Child 0 keeps the winner's policy and the others move each knob at random by up to 12.5% of its range in round 2 and 8.3% in round 3. Each simulates the next DAYS days from its parent's own stock and pipeline, and the best 1% are kept again. Ancestors left with no living descendant are dropped, leaves first. **Finally** the best world's policy table is merged into main for real.

Worlds run on every core (32 threads here, 8 on the M2), on disk, with history retention set to 0. Phases are timed one after another, each across all the round's worlds. The load column is the machine's load average after the round: this job's own threads.

**10,000 worlds:** `cargo run --release --example simulate`. 4 warehouses × 10 products = 40 SKUs, 3 rounds of 30 days, keep 100. About 85 s in all (5 minutes on the M2).

| round | worlds | alive after | fork | compute | eval | discard | CPU (all phases) | memory after compute | disk | best score | median | load |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 10,000 | 100 | 65 ms | 18.3 s | 3.9 s | 0.24 s | 577 s | 408 MB | 78 MB | 37,140,360 | 36,607,070 | 10 |
| 2 | 10,000 | 120 | 108 ms | 21.2 s | 5.5 s | 0.31 s | 566 s | 598 MB | 151 MB | 77,234,392 | 77,137,748 | 13 |
| 3 | 10,000 | 121 | 289 ms | 25.7 s | 9.2 s | 0.22 s | 661 s | 716 MB | 125 MB | 119,896,685 | 119,401,998 | 15 |

On the M2 the same rounds took 74–100 s each to compute and 321–422 s of CPU (its quieter run).

**100,000 worlds, the ultimate test:** `ULTIMATE=1 cargo run --release --example simulate`. It runs smaller worlds (sized to fit the 8 GB laptop it was written on): 3 warehouses × 4 products = 12 SKUs, 3 rounds of 10 days, keep 1,000 (100 children each). Each world writes its policy (12 rows), its orders and 12 stock rows per round. In all, 300,000 worlds were forked, simulated and scored, in **4 minutes 40 seconds** (about 21 minutes on the M2), with the process at 1.29 GB at its peak.

| round | worlds | alive after | fork | compute | eval | discard | CPU (all phases) | memory after compute | disk | best score | median | load |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 100,000 | 1,000 | 729 ms | 50.0 s | 14.3 s | 0.31 s | 866 s | 753 MB | 369 MB | 4,298,031 | 4,240,222 | 14 |
| 2 | 100,000 | 1,175 | 851 ms | 70.6 s | 23.1 s | 0.51 s | 947 s | 899 MB | 342 MB | 8,065,892 | 8,054,717 | 9 |
| 3 | 100,000 | 1,055 | 886 ms | 84.6 s | 31.6 s | 0.69 s | 1,034 s | 1,047 MB | 348 MB | 11,552,511 | 11,535,203 | 8 |

On the M2: 223–389 s to compute per round, 27–116 s to discard (since fixed, section 9), 790–874 s of CPU.

**What the search found**, over the whole horizon from main's real state (each fixed policy rerun in a fresh world from main):

| | 10,000 worlds (90 days) | 100,000 worlds (30 days) |
|---|---|---|
| main's policy today | 80,756,358 | 8,741,200 |
| round 1's best policy, kept for the whole horizon | 118,543,085 (+46.8%) | 11,515,264 (+31.7%) |
| the final policy, from day 0 | 119,493,894 (+48.0%) | 11,305,117 (+29.3%) |
| **the winning lineage** (its policy changed each round) | **119,896,685 (+48.5%, +1.1% over round 1)** | **11,552,511 (+32.2%, +0.3% over round 1)** |

**The merge:** the winner's 40 (or 12) policy rows went into main in 6–7 ms (9–12 ms on the M2), and a check compares main's policy with the winner's row for row. Main's stock, orders and forecast are untouched. **Determinism:** the winner is chosen by score, with ties broken by name, and the policies come from the seed, so thread timing can't change it. Every run of the 10,000-world test with `SEED=42`, on the M2 with 8 threads and here with 32, picked the same winner (`r3_66_26`, lineage `r1_4492 > r2_28_27 > r3_66_26`), with the same score in every round and the same policy in main afterwards.

**Losses and caveats:**
- **The learning loop adds little here.** After round 1's random search over 10,000 or 100,000 policies, the loop gained 1.1% and 0.3%. At 100,000 worlds, the final round's policy run alone from day 0 did 1.8% worse than round 1's best: it was selected for days 20–30, from its parents' stock. The policies are tuned to one forecast; a fairer test would score each on several demand samples.
- **No selective merge yet.** The winner also holds its simulated stock and orders, which must not reach main. So its policy rows are copied into a fresh world forked from main, and that world is merged. Merging only some tables of a world is being built on another branch.
- **Two recursive queries per world per round.** `UPDATE ... FROM` a `WITH RECURSIVE` query took about 4.7 s for 40 rows (the same query as an `INSERT ... SELECT` takes about 10 ms), so the stock is appended as a snapshot per round instead of updated, and the orders and the stock each run the recursion. About 40 ms of CPU per 40-SKU world per round goes to SQL expression evaluation.
- **More CPU for the same work:** 566–1,034 s of CPU per round here, against 321–874 s on the M2. Half of these 32 threads are hyperthreads, and a core is slower than an M2 performance core; the wall time falls, the total work rises.
- **Disk until `gc`:** with every world dropped and main checkpointed, the 10,000-world folder still held 82.6 MB, and 567 MB after the 100,000-world run. `gc` then freed 85,800 pages (76.8 MB) in 374 ms, leaving 2.3 MB (measured in the second 10,000-world run; the 100,000-world run predates the `gc` step).

## 11. Monte Carlo Tree Search over worlds

`GAMES=20 ITERS=200 cargo run --release --example mcts`: Connect Four, where every node of the search tree is a world holding the board in a table (`cells`, one row per piece). An in-memory database.
- **Expansion** forks the parent's world and plays the move with SQL (`insert into cells select ... count(*) ... where col = $c`); a SQL query then checks for four in a row.
- **Rollouts** fork a throwaway world from the leaf, fill the board with random moves in one `INSERT`, let SQL find who completed four in a row first (a four-way self-join ordered by move number), then discard the world. A Rust check of every rollout agrees with SQL.
- **Selection** is UCT (c = 1.4), and visit and win counts are backed up in the search's own memory.
- **After each real move** (MCTS's or the opponent's), every subtree that can no longer be reached is dropped, leaves first. The game's own path stays until the game ends, since each position is a fork of the one before.

| games | iterations per move | MCTS won | lost | drawn | worlds created | discarded | peak alive | per iteration | total | CPU | peak memory | load |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 20 | 200 | 20 | 0 | 0 | 49,402 | 49,402 | 230 | 0.84 ms | 22.6 s | 22.4 s | 20 MB | 3 |

MCTS played first in half the games and second in the rest, and won all 20 in 7 to 26 moves. An iteration (select, fork and move, fork, rollout, discard, back up) takes 0.84 ms (1.09 ms on the M2, at a load of 20; the same 49,402 worlds, since the games are seeded), and a game creates 1,400 to 4,900 worlds.

**Losses and caveats:**
- **A random opponent is weak.** It shows the search works, not how strong it is; an MCTS-versus-MCTS or solver comparison hasn't been run.
- **The tree's statistics live in Rust,** not in the worlds; only positions do.

## 12. A database bigger than its cache

`CHRONOS_CACHE_MB=32 MB=500 KEEP=1 cargo run --release --example big`: 1.31 million rows of 256 hex characters each (which barely compress), with a unique column and a B-tree index, plus 328,000 rows with text keys. That's 842 MB of pages, 25 times the cache. The database is built once (42 s), then queried in a new process, twice. The footprint is the process's resident memory after each step. Cold is the first run of a query in a process, warm the second; each cell gives both runs.

| step | cold ms | warm ms | resident memory |
|---|---|---|---|
| open | 142, 142 | | 40 MB |
| 1,000 lookups by key, p50 (p99) | 0.062, 0.064 (0.17, 0.18) | 0.052, 0.053 (0.11, 0.11) | 73–74 MB |
| 1,000 lookups by B-tree index, p50 (p99) | 0.074, 0.074 (0.15, 0.16) | 0.058, 0.059 (0.12, 0.12) | 74 MB |
| primary-key range, 1,000 keys | 2.5, 2.4 | 1.3, 1.3 | 76 MB |
| text-key range, 1/256 of the table | 1.7, 1.6 | 0.9, 0.9 | 76 MB |
| `count(*), sum, max` over everything | 716, 710 | 700, 697 | 325 MB |
| `GROUP BY`, 50 groups | 700, 690 | 710, 704 | 333–341 MB |
| join + `GROUP BY` | 735, 736 | 724, 727 | 483–491 MB |
| `tag = ...`, no index | 743, 741 | 744, 735 | 491–494 MB |
| the same in a fork with 100 changed rows | 743, 738 | 742, 747 | 494–495 MB |
| `ON CONFLICT (email) DO NOTHING` | 0.6, 0.6 | 0.1, 0.1 | 494–495 MB |
| `ON CONFLICT (email) DO UPDATE` | 5.2, 5.0 | 0.4, 0.5 | 494–495 MB |

`cache_bytes` finished at 33.5 MB against a cap of 33.55 MB. Build `0f1a19af`; `8db2f701` gave the same within noise (`25-big.out`).

**Loss to look into: resident memory.** The page cache holds to its cap, but the process reached 495 MB during the scans and joins, where the M2 run's highest footprint was 188 MB. The two measures differ (macOS's footprint against Linux's resident memory, which keeps what the allocator hasn't returned), so this may not be growth; it hasn't been separated.

### Earlier, on the M2 (8 GB)

The same run on the M2 (8 GB): 1.31 million rows of 256 hex characters each (which barely compress), with a unique column and a B-tree index, plus 328,000 rows with text keys. That's 811 MB of pages, 25 times the cache. The database is built once, then queried in a new process. The footprint is `top`'s MEM after each step, which is the true figure (RSS under-reports once pages swap). Each cell is two runs, "before" being `453e46f`. Cold is the first run of a query, warm the second.

| step | before: cold / warm ms | before: footprint | after: cold / warm ms | after: footprint |
|---|---|---|---|---|
| open | 458, 138 | 59, 31 MB | 122, 84 | 65, 90 MB |
| 1,000 lookups by key, p50 | 0.52, 0.42 / 0.028, 0.011 | 63, 64 MB | 0.40, 0.53 / 0.019, 0.014 | 63, 63 MB |
| 1,000 lookups by B-tree index, p50 | 0.52, 0.43 / 0.043, 0.027 | 83, 87 MB | 0.76, 0.86 / 0.20, 0.10 | 75, 70 MB |
| primary-key range, 1,000 keys | 6.8, 9.7 / 2.5, 1.0 | 84, 88 MB | 10.9, 8.2 / 2.8, 2.5 | 76, 66 MB |
| **text-key range, 1/256 of the table** | 537, 223 / 288, 128 | 84, 111 MB | **28, 31 / 3.1, 2.1** | 73, 67 MB |
| `count(*), sum, max` over everything | 2,221, 993 / 1,959, 992 | 88, 156 MB | 3,121, 2,079 / 1,021, 2,389 | 97, 91 MB |
| `GROUP BY`, 50 groups | 1,877, 555 | 89, 237 MB | 851, 1,477 | 128, 91 MB |
| join + `GROUP BY` | 1,891, 474 | 107, 297 MB | 817, 1,650 | 188, 78 MB |
| `tag = ...`, no index | 2,530, 655 | 89, 211 MB | 978, 1,346 | 195, 92 MB |
| the same in a fork with 100 changed rows | 2,884, 463 | 105, 183 MB | 1,404, 1,274 | 108, 83 MB |
| **`ON CONFLICT (email) DO NOTHING`** | **killed past 2 GB, both runs** | 2,134+ MB | **2.1, 3.2 / 0.0, 0.1** | 98, 83 MB |
| `ON CONFLICT (email) DO UPDATE` | not reached | | 13.6, 8.9 / 1.7, 2.2 | 85, 79 MB |

After the fixes, the highest footprint seen in a run (sampled every 2–3 seconds as well as after each step) was 188 MB and 118 MB, and `cache_bytes` finished at 33.5 MB against a cap of 33.55 MB.

**What changed:**
- **Unique checks:** `ON CONFLICT` on a `UNIQUE` column found the clashing row through the table's search index, which it built whole with no budget. On a 1.67 GB build of this database it reached 5 GB of footprint in 6.5 minutes without finishing. It now reads the constraint's own entry, which the insert's check already used.
- **Text-key ranges** read just the keys in the range, where they used to scan the table.
- **The page cache** counts pages by the memory they take decoded. Counting their size on disk undercounted small rows about 2.5 times, so a "32 MB" cache could hold up to 2.5 times that. With the true count, the same setting holds fewer pages, and warm B-tree lookups, whose index and row pages no longer all fit, went from 0.03 to 0.1–0.2 ms. With `CHRONOS_CACHE_MB=64` (a footprint of 98 MB at that step, about what "32" had before) they took 0.017 ms.
- **The search index** kept every one-letter deletion of every word for typo correction. That's 64 KB for each 256-character hash, so a 19 MB table's index grew past 2.5 GB before it was killed (`MB=20`, a table small enough for `column = value` to build one). Words over 32 letters are no longer corrected. The same query now peaks at 244 MB, about ten times the table (see the [operations guide](docs/operations.md#databases-bigger-than-memory)).

**Caveats:**
- **The timings are noisy:** the load average was 23 to 58 on 8 cores, since other builds were running. Scans varied twofold between identical runs, so only the text-key range and `ON CONFLICT` rows show real changes.
- **Cold means a new process, not a cold disk.** macOS kept the files in its page cache, so cold reads cost decompression and hashing, not the disk.
- **What still grows with the database:** the page directory, about 90 bytes per page (30 MB for these 330,000 pages, more while opening). Wide integer-key ranges and B-tree reads over up to a fifth of the table hold the rows they find.

### Building indexes and loading, under a 2 GB cap

*Not rerun: from the 4-vCPU, 32 GB VM.*

On the 4-core, 32 GB Linux VM, each step a new `chronos` process in a `systemd-run` scope with `MemoryMax=2G` and no swap, `CHRONOS_CACHE_MB=64`, `history_retention` 0. The table: `big (id bigint primary key, grp bigint, body text)`, 7 million rows of 540 bytes (3.8 GB of rows; 664 MB of pages, as the bodies are hex of neighbouring hashes and compress well), loaded in 100,000-row `INSERT ... SELECT` statements. "Anon" is the process's own memory at its highest, sampled every half second; the cgroup total adds the kernel's file cache, which it gives back under pressure. Before is main at `ddf74a0`, after is branch `fx-index-memory`.

| step | before: result, time, anon | after: result, time, anon (cgroup) |
|---|---|---|
| load, 70 statements of 100,000 rows (no cap) | | done, 421 s, 454 MB (1,157 MB) |
| `create index big_grp on big (grp)` | killed at the cap after 91 s | done, 65 s, 548 MB (1,266 MB) |
| `create unique index big_gid on big (grp, id)` | killed at the cap after 98 s | done, 206 s, 767 MB (2,048 MB) |
| `INSERT ... SELECT`, 250,000 rows | done, 66 s, 1,608 MB | done, 63 s, 1,022 MB (1,603 MB) |
| `INSERT ... SELECT`, 1,000,000 rows | killed at the cap after 52 s | killed at the cap after 96 s |
| the same, no cap | done, 208 s, 6,159 MB | done, 276 s, 3,508 MB |

- **Index builds** sort their entries within `CHRONOS_WORK_MEM` (256 MB), spilling to disk, and write them in parts with checkpoints between, so they no longer grow with the table. Before, one write held every entry in several maps, the log record and the in-memory changes at once (7.8 GB on a 9.3 GB table in the audit on the 8 GB Mac).
- **A big single statement is still unbounded.** It commits as one write, so it holds its rows about six times over at 1 million rows: the `SELECT`'s rows, the packed rows, a copy the constraint checks make, the index entries, the log record and the rows kept until the next checkpoint. The changes here remove some of those copies (6.2 GB to 3.5 GB). Loads in statements of 100,000 rows stayed at 454 MB. (Bounded since: see the next section.)

### One statement bigger than memory, under a 30 GB cap

*Not rerun: from the 4-vCPU, 32 GB VM.*

The same VM (4 cores, 31 GB), each statement in a new `chronos <db> < file.sql` process in a `systemd-run` scope with `MemoryMax=30G` (the whole VM less 1 GB for the system) and no swap, one run at a time. The table: `t (id int primary key, k int, body text)` with an index on `k`, filled by one `INSERT INTO t SELECT g, (g*7919) % 1000003, md5(g::text) || ... || md5((g+15)::text) FROM generate_series(1, n) g` (540-byte rows). "Anon" is the process's own memory at its highest, sampled every half second; the cgroup figure adds the kernel's file cache, which it gives back under pressure. Main is `70ebcde`; the branch is `fx-bulk-spill` (source as of `d9798eb`).

**Flat, with a small budget** (`CHRONOS_WORK_MEM=32MB`, `CHRONOS_CACHE_MB=64`):

| rows | main: time, anon (cgroup) | branch: time, anon (cgroup) |
|---|---|---|
| 200,000 | 14.4 s, 439 MB (566 MB) | 10.9 s, **61 MB** (98 MB) |
| 1,000,000 | 81.8 s, 2,251 MB (2,858 MB) | 54.8 s, **67 MB** (271 MB) |
| 2,000,000 | 186.1 s, 4,531 MB (5,741 MB) | 113.2 s, **78 MB** (491 MB) |

**The record runs, default settings** (256 MB work memory, 256 MB cache):

| statement | time | anon (cgroup) |
|---|---|---|
| branch, `INSERT ... SELECT` 1,000,000 rows | 58.4 s | 405 MB (523 MB) |
| branch, 5,000,000 rows | 369.6 s | 406 MB (1,335 MB) |
| branch, 10,000,000 rows (1.2 GB on disk) | 589.4 s | 411 MB (2,458 MB) |
| main, 3,000,000 rows | 325.7 s | 6,811 MB (8,633 MB) |

Main grows by about 2.25 KB of memory per row (2.25 GB per million rows at 1, 2 and 3 million), so 10 million rows would need about 23 GB: it would fit under this cap, but main also logs every row (700 MB of disk per million rows), and 10 million didn't fit on the VM's 8 GB of free disk, so main stopped at 3 million. The branch's database at 10 million rows took 1.2 GB.

**Other big statements** (branch, default settings; the first three on a 5-million-row table, the `COPY`s each into a new database):

| statement | result | time | anon (cgroup) |
|---|---|---|---|
| `UPDATE t SET k = k + 1, body = upper(body)`, every row | UPDATE 5000000 | 308.6 s | 548 MB (1,921 MB) |
| 5 million new rows, the 4-millionth taking an id the table has | 23505, nothing written | 236.3 s | 613 MB (1,005 MB) |
| 5 million new rows, the 1st and the 4-millionth taking one id (found as the runs merge) | 23505, nothing written | 305.5 s | 617 MB (1,105 MB) |
| `\copy` of 500,000 / 2,000,000 / 4,000,000 rows into a new table, through the server | COPY n | 24.6 / 94.9 / 192.8 s | 433 / 440 / 419 MB |
| the same with the small budget, 500,000 / 2,000,000 rows | COPY n | 21.2 / 112.7 s | 83 / 123 MB |

After each failed statement the table had its 5 million rows, `VERIFY DATABASE` passed and the spill folder was empty. The inserts into the 5-million-row table peak higher than into an empty one because their key checks read the table through the page cache: what grows is the database's share (the cache, up to its cap, and the page directory, about 90 bytes a page), not the statement's. With the small budget, 200,000 rows into an empty table peaked at 62 MB, and into one holding 2 million rows at 154 MB; a 500,000-row `COPY` at 67 and 97 MB. (A 2-million-row `COPY` into the 5-million-row database, whose server held a 646,000-page directory and a full cache, peaked at 1,043 MB.)

**Crashes** (kill -9, then reopen twice, `VERIFY DATABASE`, count the rows): a table of 1,000 rows and a 4,999,000-row insert killed at 90 s (140 MB of runs in the spill folder) and 200 s (309 MB): 1,000 rows each time, spill folder emptied. A 2,999,000-row insert killed during its commit: as the new tree's pages began (+20 MB of pages) and late in them (+240 MB of 346): 1,000 rows; as soon as the log's new segment appeared, and as soon as the manifest changed: 3,000,000 rows (the checkpoint had landed). Never a count in between, and verify passed every time.

**Small statements aren't slower** (default settings, fresh database each, three rounds; the first round was slower for both and is in brackets):

| rows | main | branch |
|---|---|---|
| 100,000 (held in memory on both) | 7.3, 6.8 s (15.1) | 6.8, 6.2 s (11.6) |
| 1,000,000 | 79.1, 86.3 s (91.0) | 56.8, 56.3 s (92.6) |

- **What held the memory before** (the branch's own first cut too): a statement is one atomic write, so it held its rows several times over (the SELECT's rows, packed rows, index entries, constraint checks, the log record, then the rows until the next checkpoint). The first version of this branch spilled writes past `CHRONOS_WORK_MEM` but still grew linearly on these runs: `generate_series` in `FROM` is parsed as a `LATERAL` source, and the code that reads an `INSERT ... SELECT` a part at a time refused `LATERAL` sources, so every series was computed whole and handed over as one part.
- **Where the branch's memory goes:** a part of the statement, whose writes are held until their estimated size passes half of `CHRONOS_WORK_MEM` (a quarter after the first part) and take about three times that estimate with their index entries and marks (so about 400 MB at the default 256 MB, 60 MB at 32 MB); a block of each run being merged (at most 16); and a batch of 8 MB of changes as the merged runs go into the tree.

### Checked through the Postgres server (the item 23 audit, main `ae51cf7`)

The merged build as a server (`chronos serve`, 30 GB cap), driven by psql like an application, a fresh database per size; the server's own memory (anon) sampled every 0.2 s, one phase at a time. Table `t (id bigint primary key, k bigint, body text)` with an index on `k`, 540-byte rows, one `INSERT ... SELECT` of n rows.

| phase | 1,000,000 rows | 4,000,000 rows |
|---|---|---|
| the `INSERT` of n rows | 58 s, **397 MB** | 247 s, **397 MB** |
| idle after the insert | 96 MB | 156 MB |
| `select count(*) from t` | 336 MB | 378 MB |
| `select count(distinct k) from t` | 667 MB | **1,364 MB** |
| idle after the queries | 483 MB | 698 MB |

All rows were there, `VERIFY DATABASE` passed and the spill folder was empty. The insert is flat and gives its memory back. The idle figure after the queries is the page cache (up to its cap) and the page directory, which grow with the database. `count(distinct k)` was not bounded: see the next section.

### `DISTINCT` aggregates

The same VM, 30 GB cap, each query in a new `chronos <db> < query.sql` process, the query's own peak `RssAnon`. Tables `t (id bigint primary key, k, j, g)`: `k` all distinct, `j` and `g` 100,000 values. Main is `5722d58`; the branch is `fx-distinct-spill` with its wave fix.

| query | 1M rows: main / branch | 4M: main / branch | 10M: main / branch |
|---|---|---|---|
| `count(distinct k)`, all distinct, defaults | 603 / 452 MB | 2,054 / 555 MB | 3,998 / 672 MB |
| `count(distinct j)`, 100,000 values, defaults | 554 / 370 MB | 1,738 / 345 MB | 3,503 / 572 MB |
| `group by g`, 100,000 groups, `count(distinct k)`, defaults | 616 / 441 MB | 1,816 / 737 MB | 3,650 / 817 MB |
| `count(distinct j)`, 32 MB cache, 64 MB budget | – / 76 MB | 1,488 / 94 MB | 3,271 / 295 MB |
| `count(*)`, 32 MB cache (the scan alone) | – | 70 / 70 MB | 210 / 195 MB |

Main reads the whole table into memory before grouping a DISTINCT aggregate; the branch holds the groups, their distinct values up to the budget, then spills. What still grows at 10 million rows is the scan's own share, the same with `count(*)` on main (70 MB at 4 million rows, 210 MB at 10 million), and at the defaults the page cache filling to 256 MB. Grouped DISTINCT spilling to disk is slower than holding it: 23.5 s against 8.0 s at 4 million rows, 52 s against 19 s at 10 million. While it fits, it's as fast: `count(distinct j)` over 1 million rows took 2.1 s on both builds, five runs each.

## 13. Correctness: SQLite's sqllogictest over the Postgres protocol

*Not rerun on the 32-core machine: the 4-vCPU VM, 2026-09-27.*

SQLite's [sqllogictest](https://sqlite.org/sqllogictest) corpus (622 files; the [GitHub mirror](https://github.com/gregrahn/sqllogictest)) checks query answers: every query comes with its result, compared value by value or by an MD5 of all values. It's run here as the `postgresql` engine, so the records the corpus marks as not for Postgres are skipped. The same runner (its scripts and these results are in [`bench/slt`](bench/slt); its Rust source is in the engine repository) sends every record over the Postgres protocol to a fresh `chronos serve` per file, and to a reference **Postgres 17** (a database per worker), and prints values as SQLite's own runner does. A record that fails on both is the corpus's SQLite dialect, not the engine. Linux VM (4 vCPUs, 32 GB), 2026-09-27; Chronos with the fixes listed below, each file that a fix touched run again after it.

| | files | records run | passed | failed |
|---|---:|---:|---:|---:|
| **Chronos DB** | 622 | 5,675,180 | **5,670,175 (99.912%)** | 5,005 |
| Postgres 17 | 622 | 5,675,180 | 5,663,585 (99.796%) | 11,595 |

1,745,458 records were skipped on both (marked `skipif postgresql` or for another engine).

**Wrong answers: none of Chronos's own.** Chronos returned a different answer from the corpus's 8 times, and Postgres 17 returned the identical answer each time (SQLite's `REPLACE`). There is no record where Postgres answers right and Chronos answers wrong.

**Where only Chronos fails (63 records):** `22012 division by zero` in machine-generated expressions that also compare with a NULL constant, such as `WHERE NULL <> - 39 * - col4 / + 0`. Postgres folds the comparison with NULL to NULL when it plans and never divides; Chronos divides. (47 other such records fail only on Postgres: the two evaluate in different orders.)

**Where only Postgres fails (6,653 records), Chronos is laxer than Postgres, not better:** it accepts unary `+` on text (6,420; Postgres: `operator does not exist: + text`), does integer arithmetic in 64 bits where Postgres's `integer` overflows at 32 (29; Postgres: `22003`), and keeps some `NULLIF` / `COALESCE` / `CASE` results integer where Postgres makes them `numeric` (157, where Chronos matches SQLite's answer and Postgres doesn't). These pass against the corpus but are differences from Postgres.

**Both fail (4,942):** SQLite's own rules, where both answer as Postgres does: dropping a view other views read without `CASCADE` (2BP01, most of them, in `index/view`), SQLite's trigger syntax, `REPLACE` and `REINDEX`, `'hello' IN (SELECT int_col ...)` (22P02), and 31 division-by-zero records both raise.

**Found and fixed by this run** (in `fx-pgcompat`): a column set twice in one `SET` was accepted (Postgres: 42601); a string literal that isn't its column's type passed when no row was compared (Postgres: 22P02 when planning); aggregates rejected `ALL` (`sum(ALL x)`); a parenthesis holding an `OR` counted 16 levels toward the nesting limit, so `(a OR (a OR ...))` was refused at 25 deep; and `FROM (a JOIN b ON ...)` was a syntax error (3,017 records). Still refused: a parenthesized join with an alias of its own or after an outer join or `USING` (0A000).

```bash
git clone --depth 1 https://github.com/gregrahn/sqllogictest
cargo build --release && cargo build --release --manifest-path bench/slt/Cargo.toml
cp target/release/chronos bench/slt/target/release/slt bench/slt/
bench/slt/par.sh chronos out-chronos 3 $(find sqllogictest/test -name '*.test')
bench/slt/par.sh postgres out-postgres 2 $(find sqllogictest/test -name '*.test')   # Postgres at host=/tmp
```

## 14. Attach: writing merges back to a managed Postgres (Neon)

*2026-10-17. Chronos on the chronos-test VM (GCP, Mumbai); Postgres 16.15 on Neon (AWS Singapore, 0.25 compute units, the smallest), about 60 ms apart. One run, so read these as orders of magnitude.*

`chronos attach` of two tables (200 accounts, 20,000 leads with a `serial` id and a trigger that sets `updated_at`), then merges of worlds into main, each written to Neon first (see [the guide](docs/guides/attach-postgres.md)). The same run twice: straight to the database, and through Neon's pooler (PgBouncer, transaction mode).

| | direct | through the pooler |
|---|---:|---:|
| attach: copy 20,200 rows in one consistent read | 7.7 s | 7.9 s |
| merge 1,000 updated rows | 1.19 s | 1.20 s |
| merge 1,000 inserted rows (ids reserved from Neon's sequence) | 1.19 s | 1.19 s |
| merge 500 deleted rows | 1.13 s | 1.07 s |
| a row changed in Neon meanwhile | conflict, nothing written | conflict, nothing written |

After the run, Neon and Chronos's main held the same rows (count, sum of `score`, highest id), and main held every `updated_at` Neon's trigger set. A merge's time is mostly round trips to Neon (a new TLS connection and login, then about four statements per table), so it grows with the distance to the database, not much with the rows: it is not a measure of Chronos itself. The transaction-mode pooler works because every setting a write-back makes is `SET LOCAL` and every statement is unnamed.

## 15. What checking a world's reads costs

*2026-10-17. A temporary GCP c2d-standard-8 (8 vCPUs; standard persistent disk, so merges' fsyncs are slower than on the other sections' SSDs), release build. Script: [`examples/readsets.rs`](examples/readsets.rs).*

A world forked `WITH (check_reads = true)` records what it reads ([src/stale.rs](src/stale.rs)); its merge is held if any of that changed in main since the fork. The same work with recording off and on, on a table of 100,000 rows: the agent reads through a SQL session, main then changes 1,000 rows the world didn't read, the world writes one row and merges. Medians aren't taken: one run, repeated once with the same result to within 10%.

| reads | check_reads | per read | merge | held |
|---|---|---:|---:|---|
| 9,000 point lookups by key | off | 40.8 µs | 0.7 ms | no |
| 9,000 point lookups by key | on | 38.3 µs | 3.5 ms | no |
| 100,000 point lookups by key | off | 40.5 µs | 0.8 ms | no |
| 100,000 point lookups by key | on | 42.0 µs | 0.2 ms | **yes** |
| 1,000 filtered scans (`where region = ...`, 25 distinct conditions) | off | 641 µs | 1.8 ms | no |
| 1,000 filtered scans | on | 622 µs | **1,825 ms** | no |

- **Reading costs nothing measurable:** per-statement time with recording on is within noise of off, for lookups and scans alike.
- **Past 10,000 keys of one table, the read counts as the whole table** (by design: the read set stays small), so *any* change to that table holds the merge, as in the third row. An agent that reads that many rows of a table main is busy with will be held often; a filter (`where ...`) keeps the check precise.
- **The merge-time check for filters grows with filters × changed rows**, about 1.7 µs per pair here: 1,000 filters × 1,000 changed rows took 1.8 s, and 100 × 1,000 or 1,000 × 100 about 0.18 s. Two causes, both fixable ([issue #29](https://github.com/Abhishekxdg/chronosdb/issues/29)): identical filters aren't kept once (25 conditions were kept as 1,000 filters), and each changed row is decoded again for each filter. Both fixed in 0.3.0: the 1,000-scan merge takes 2.7 ms there (same script, from the 0.3.0 release notes).

## 16. BranchBench: six agent workflows against Doltgres and Postgres copies

*2026-10-24. GCP `c2d-standard-16` (16 vCPUs, AMD EPYC 7B13; 62 GB; 400 GB pd-ssd; Ubuntu 24.04; us-west1-a): a c2d-standard-32 didn't fit the project's CPU quota. One job at a time on an otherwise idle VM, page cache dropped between jobs, each run on a fresh database. Chronos served over the Postgres protocol with `CHRONOS_CACHE_MB=8192`: S4 on `be3f4cfc` (main with #237–#244) plus #257, the other five on `07c25646` (main with #237–#243; #244 only acts on a query past its memory limit, #257 only where both sides change one table's columns, and neither happens there); Postgres 18.6 with `shared_buffers = 8GB` on an XFS volume with reflinks (`file_copy_method = clone`); Doltgres 1.4.0. Harness: BranchBench (Columbia DAP Lab, [arXiv 2604.17180](https://arxiv.org/abs/2604.17180)) at `1ca34a3` plus the `chronosdb` backend ([Abhishekxdg/db-fork `chronosdb-backend`](https://github.com/Abhishekxdg/db-fork/tree/chronosdb-backend)). Raw stats and logs: [`bench/results/2026-10-24-branchbench/`](bench/results/2026-10-24-branchbench).*

BranchBench drives each database through one git-like API (branch, commit, diff, log, merge, rebase, revert, reset, delete) in six agent workflows over TPC-C / CH-benCHmark data, and checks after each step that what the workflow expects of the data holds (its "invariants"). Each backend is an adapter: Chronos DB's maps every verb to one or two of its own statements (`CREATE WORLD ... FROM`, `DIFF WORLD a TO b`, `MERGE WORLD`, `REBASE WORLD`, `RESTORE WORLD ... TO`, `DROP WORLD`; a commit is a moment, read back as `'world@time'`). Native: branch, diff, merge, rebase, reset, delete and commit refs. Composed: commit (a log row plus the moment) and revert (the engine's inverse `DIFF ... AS SQL`, run by the adapter). Simulated: log (a bookkeeping table). Unsupported: one statement over several branches, so cross-branch reads run once per branch. `file_copy` is plain Postgres where a branch is a reflinked copy of the database and merge is SQL the adapter writes; `dolt` is Doltgres. The full configurations at W=5 (five warehouses, 2.6 million rows seeded), with the paper's two-hour cap per run; one run each.

| scenario | Chronos DB | file_copy (Postgres 18) | Doltgres 1.4.0 |
|---|---:|---:|---:|
| S1 RL environment: 16 tasks, 256 rollout leaves forked from commits | **1,130 s**, 560/560 | 2,529 s, 560/560 | stopped at 2 h (12 of 16 tasks), 420/420, 12 operations stalled |
| S2 context management: candidates, compaction, revert and rebase | **2.2 s**, 3/3 | 53 s, 3/3 | 13 s, 3/3 |
| S3 multi-agent: 512 merges into a moving spine | **3.9 s**, 8/8 | 149 s, 8/8 | 19 s, 8/8 |
| S4 dev agent: rebases off a busy spine, a schema clash | 663 s, 122/122 | 4,987 s, 123/123 | **563 s**, 121/121 |
| S5 ops agent: bad deploy, investigation branches, point-in-time reset | **515 s**, 10/10 | 1,817 s, 10/10 | 634 s, 10/10 |
| S6 data agent: 32 ingest batches rebased and merged, resets | **2,275 s**, 128/128 | 2,817 s, 128/128 | 1,493 s, **64/128** |

(seconds of the workflow, then invariants passed; setup and seeding not counted. A run's invariant count follows the path it took, so it varies a little between backends.)

- **Chronos was fastest in four of six (S1, S2, S3, S5), and the one whose answers held in all six.** Doltgres was faster in S4, and finished S6 sooner, but half its S6 invariants failed: its `dolt_reset('--hard', ...)` returned success and left the rows loaded after the commit (`S6.2 batch 5 reset to step 1 restores its rows: expected 200, got 400`). That may be this Doltgres version or our setup; the BranchBench authors' own runs are the reference for it.
- **Recovery after the bad deploy (S5)** took 18.8 s on Chronos (`RESTORE WORLD ... TO` the commit's moment), 195 s on file_copy (drop and re-clone) and 208 s on Doltgres.
- **S4 is Doltgres's.** 563 s to 663 s. Chronos's S4 also needed #257 first (below).
- **The background TPC-C load ran slower on Chronos** while the agents worked: 28 transactions/s in S4 (Doltgres 30, file_copy 73) and 21/s in S5 (Doltgres 30, file_copy 24).
- **Conflict counts don't compare.** Chronos reports a row both sides changed as a conflict even when they changed different columns or wrote the same value (its default whole-row merge; `BY COLUMNS` would combine the different-column ones, and the adapter left it off in this run). Doltgres's count is `dolt_merge()`'s `conflicts` result, about one per merge, not a count of rows. S3: 35,503 rows in conflict on Chronos, 508 on Doltgres (over 512 merges), 1,261 on file_copy; every merge succeeded and every invariant held on all three. See 16.1 for the run with `BY COLUMNS`.
- **Storage** isn't compared: file_copy's number is the sum of its databases' sizes (a reflinked clone counted whole), and Doltgres's directory wasn't measured in this setup. Chronos's folder went from 1.6 GB after seeding to 2.2–3.9 GB at the end of S1, S4, S5 and S6 (with a day of history kept, for the commits).

**Found and fixed on the way.** The first runs, on 0.4.1, did much worse. BranchBench's consistency checks join TPC-C tables on three key columns and S4's spine runs CH Q3 and Q18. On 0.4.1 those joins ran as nested loops and S1 made no progress; with #241, Q3 took the server to 55 GB and the OOM killer two minutes into S4; with #242, S4's Q18 still took 17 minutes. Each became its own change, with a test:

| | what BranchBench hit | fix |
|---|---|---|
| #241 | hash joins keyed on the first equality only: `order_line JOIN orders` on (w, d, o) > 120 s at W=1 (Postgres 74 ms) | key on every equality: 56 ms |
| #242 | CH Q3 joined customer to new_order first (each new order × each customer of its district): > 12 GB at W=1 | the equalities the ON clauses imply decide the join order: 0.35 s, 850 MB |
| #243 | the on-disk (grace) join, past `CHRONOS_WORK_MEM`, still keyed on one column: Q3 and Q18 17 min at W=5 | key on every equality there too: 0.6 s |
| #244 | no memory limit outside an agent's `max_memory_mb`; and a join's probe never checked limits, so a 120 s `statement_timeout` landed at 161–205 s | `CHRONOS_QUERY_MEM` for every statement (a quarter of RAM), all statements to half; the probe checks as it goes: a 51 GB runaway join now fails in 1 s with the server up |
| #257 | a merge or rebase where both sides changed one table's columns was refused even for different columns: 4 of S4's invariants (114/118) | merge the definitions column by column: 122/122 |
| #237 | `USE WORLD 'name@when'` was documented but refused | works (BranchBench's commit refs use it) |
| #239 | `DIFF` missed a column's new size (`varchar(5)` to `varchar(10)`) | it shows, and `DIFF ... AS SQL` carries it |

**Where Chronos still loses here.**
- **Analytical joins are about 4× Postgres.** CH Q3 0.32 s and Q18 0.27 s at W=1, against 0.08 s each on Postgres 18 (same VM; Chronos held to 2 threads, Postgres at its defaults, so this flatters Postgres somewhat).
- **Memory.** Chronos peaked at 23 GB RSS in S4 (eight CH queries at a time over W=5, before #244). Rows a query holds are decoded values, several times their size on disk; the limit now fails a runaway query alone, but doesn't make the others smaller.
- **Diff and revert of many rows.** `diff` returns every changed row: 47 ms at 1,000 rows, 495 ms at 10,000. `revert` runs the engine's inverse statements one at a time: 211 ms at 1,000 rows, 2.05 s at 10,000.

**Microbenchmarks** (Chronos only, CH-benCHmark at W=1, 16 branches; BranchBench's `run_single_thread_bench.sh` and `run_micro_verbs.sh`, medians):

| operation | Chronos DB |
|---|---|
| point read / insert / update, one thread | 0.25 / 1.9 / 1.9 ms per operation (incl. the harness) |
| range read / range update (200 rows) | 3.7 ms / 38 ms |
| connect (switch to a branch) | 0.1 ms |
| branch, from main / from a commit 1–128 commits back | 0 ms / 4–8 ms |
| commit (1–10,000 rows written since the last) | 1–3 ms |
| delete a branch (any history, any size) | 0–2 ms |
| merge 1 / 1,000 / 10,000 rows | 1 / 10 / 90 ms |
| merge with conflicts (10–100% of 1,000 rows) | 101–131 ms |
| rebase 1 / 1,000 / 10,000 rows | 1 / 9 / 88 ms |
| reset, undoing 1 / 10,000 rows | 7 / 33 ms |

Throughput (`run_throughput_bench.sh --sweep-proportional --mode sync`, 4 threads per branch, 1 to 128 branches, no failures): point reads 6,532 ops/s at 4 threads and 4,946 at 512; range reads 365–395 ops/s throughout. **That's BranchBench's client, not Chronos:** in sync mode it is one Python process, flat from 4 threads to 512 while the VM's load stayed at 2–6 of 16. A throughput comparison needs its async mode (`--mode async`, which the backend supports), not run here.

### 16.1 Rerun with Chronos's own merge, diff and revert statements

*2026-10-25, same VM and harness, Chronos only. Engine: main with #257 and #260 (`MERGE ... BY COLUMNS` counting its conflicts in a NOTICE, `DIFF ... SUMMARY`, `UNDO DIFF`). Adapter `f3ce02f`: merges run `BY COLUMNS USING OURS|THEIRS` in one statement, `diff` reads `DIFF ... SUMMARY`, and `revert` is `UNDO DIFF ... EXCEPT TABLES (_bb_commits)`, which fails on a row changed since the commit (as git's and Dolt's revert do) where the old one overwrote it. Raw stats: [`bench/results/2026-10-24-branchbench/rerun-0.4.2/`](bench/results/2026-10-24-branchbench/rerun-0.4.2).*

| scenario | Chronos before | Chronos now | file_copy | Doltgres |
|---|---:|---:|---:|---:|
| S1 RL environment | 1,130 s, 560/560 | **994 s**, 560/560 | 2,529 s | 2 h cap |
| S2 context management | 2.2 s, 3/3 | **2.0 s**, 3/3 | 53 s | 13 s |
| S3 multi-agent | 3.9 s, 8/8 | **4.7 s**, 8/8 | 149 s | 19 s |
| S4 dev agent | 663 s, 122/122 | 580 s, 124/124 | 4,987 s | **563 s** |
| S5 ops agent | 515 s, 10/10 | **505 s**, 10/10; recovery 10.4 s (was 18.8) | 1,817 s; 195 s | 634 s; 208 s |
| S6 data agent | 2,275 s, 128/128 | 2,221 s, **128/128** | 2,817 s, 128/128 | 1,493 s, 64/128 |

- **S4 is still Doltgres's, by 3%** (580 s to 563 s, from 18%).
- **Conflicts, by columns.** S4's rebase conflicts went from 2,174 rows to 254 (Doltgres 117, file_copy 30,886). S3's stayed at 35,533: there the spine and the agents change the same columns of the same task rows (`status`, `version`), which conflict cell by cell too.
- **The first `BY COLUMNS` run found a bug**, fixed in #260 before these numbers: a row both sides changed, in a table both sides changed the columns of (S4's `c_tier` added as two types), failed the merge with that row's error (22P02) instead of the clash itself, so 4 of S4's invariants failed.
- **Microbenchmarks** (W=1, medians, 97 of 97 points ok): `diff` of 1,000 / 10,000 rows 47 / 495 ms → **1.8 / 14 ms**; `revert` of 1,000 / 10,000 rows 211 / 2,051 ms → **23 / 120 ms**; a merge of 1,000 rows with 10–100% of them in conflict 101–131 ms → **14–49 ms** (one statement instead of three). Merge and rebase without conflicts unchanged (1 / 10 / 94 ms at 1 / 1,000 / 10,000 rows).

**Why the spine's TPC-C stays at about 30 transactions/s** (S4: 29.6/s, Doltgres 30, file_copy 73). Two CH queries the spine runs among its transactions, Q3 and Q18, took 4.2 and 4.0 s each (file_copy's Postgres: 0.8 and 1.6 s): 73% of the spine's time. It isn't the agents beside them. At W=1 six Q3s at once take 0.5 s each against 0.19 s alone, and a fair split of the helper threads between running queries (tried, #261) changed nothing. It's the data size: on a W=5 copy of the CH tables, Q3 takes 3.0 s and Q18 5.3 s alone, because their hash joins outgrow `CHRONOS_WORK_MEM` (256 MB) and go to disk. With `CHRONOS_WORK_MEM=4GB` they take 0.75 s and 1.3 s, but Q18 then holds 2.7 GB. The likely cause, not yet confirmed, is that joined rows are held at their tables' full width rather than only the columns the query reads; holding only those would be the fix, not done yet.

## Rerun everything

```bash
cargo run --release --example bench -- 10 100 1000                 # needs Postgres at host=/tmp
WORKERS=25 cargo run --release --example bench -- 1000             # Chronos at Postgres's 25 workers (TIMINGS=1: its own timings)
python bench/rivals/forks.py dolt 10 100 1000                       # Dolt sql-server; also: postgres, neon (NEON_API_KEY), duckdb
# BranchBench (section 16): in a clone of Abhishekxdg/db-fork on branch chronosdb-backend, with chronos serve on :5433
./scripts/run_macrobench.sh --outdir run_stats --max-runtime-sec 7200 dev_agent chronosdb 5   # each of rl_env context_mgmt multi_agent dev_agent ops_agent data_agent; also file_copy, dolt
N=500000 OVERSAMPLE=100 EF=300 cargo run --release --example search  # needs pgvector; N=1000000 for 1M x 384
N=1000000 cargo run --release --example find
CRASH_RUNS=10000 cargo test --release --test durability crash_suite
SIM_ITERS=3000 cargo test --release --features shuttle --test sim   # own CARGO_TARGET_DIR: shuttle rebuilds the chronos binary
JEPSEN_KILLS=20 cargo test --release --test jepsen -- --nocapture
chronos serve /tmp/sqlbench & N=10000 cargo run --release --example sql   # needs Postgres on 127.0.0.1:5432
N=200000 cargo run --release --example report                       # PG=off runs Chronos alone
ROWS=500000 AGENT=100000 OTHERS=50000 cargo run --release --example undo_agent
CHRONOS_CACHE_MB=32 MB=500 cargo run --release --example big            # builds ~800 MB in a temp folder
python bench/sql-subqueries/bench.py [--timeout 300]                # the CRM demo's slow SQL on 150,600 rows; needs psql
python bench/mcp-sessions/bench.py                                  # MCP sessions beside one running something slow; needs psql
python bench/vector_dbs.py [system ...]                             # one system per run; see its header
DATA=... python bench/rivals/pgvector_ef.py                         # pgvector's ef_search sweep on a loaded table
SWEEP=1 ROWS=100000 cargo run --release --example worlds            # 10 .. 100,000 worlds
cargo run --release --example simulate                              # 10,000 worlds x 3 rounds
ULTIMATE=1 cargo run --release --example simulate                   # the ultimate test: 100,000 worlds x 3 rounds
GAMES=20 ITERS=200 cargo run --release --example mcts               # MCTS over worlds vs a random player
```

The 2026-09-28 rerun ran these as queued jobs, one at a time: each job's script and output are in [`bench/results/2026-09-28-rerun/`](bench/results/2026-09-28-rerun) (`vec.sh` starts each vector database fresh, `02-install-vdbs.sh` and `03-install-pgvectorscale.sh` pin their versions).
