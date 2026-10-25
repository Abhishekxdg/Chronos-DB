---
title: Clients
---

A TypeScript and a Python client for [`chronos serve`](../http-api.md), one file each, no dependencies. They speak the HTTP API (`POST /v1/<op>`), not the Postgres port: for Postgres drivers (psql, `pg`, psycopg, JDBC, ...) see [Postgres compatibility](../postgres-compatibility.md).

```ts
const db = new Chronos("http://127.0.0.1:7070", { token: process.env.CHRONOS_TOKEN });
const agent = await db.fork("agent-7");            // instant copy of main
await agent.put("users", "1", { name: "Ada L." });
console.log(await agent.diff());
await agent.merge();                                // or agent.discard()
```

## Getting them

TypeScript/JavaScript, from npm ([`@chronosdb/client`](https://www.npmjs.com/package/@chronosdb/client), ESM, with its types):

```bash
npm install @chronosdb/client
```

Python isn't on PyPI yet: copy the file into your project.

| Language | Get it | Needs |
|---|---|---|
| TypeScript / JavaScript | `npm install @chronosdb/client`, or copy [clients/typescript/chronos.ts](https://github.com/Abhishekxdg/chronosdb/blob/main/clients/typescript/chronos.ts) | global `fetch`: Node 18+, browsers, edge runtimes |
| Python | [clients/python/chronos.py](https://github.com/Abhishekxdg/chronosdb/blob/main/clients/python/chronos.py) | Python 3.9+, standard library only |

```ts
import { Chronos, ChronosError } from "@chronosdb/client";
```
```python
from chronos import Chronos, ChronosError
```

## Constructor

```ts
new Chronos(url = "http://127.0.0.1:7070", { token?: string, branch?: string } = {})
```
```python
Chronos(url="http://127.0.0.1:7070", token=None, branch="main")
```

| Option | Default | |
|---|---|---|
| `url` | `http://127.0.0.1:7070` | the server; a trailing `/` is dropped |
| `token` | none | sent as `Authorization: Bearer <token>`: the admin token (the database's own user), an agent's token, or the server's token (the guest; see [auth](../http-api.md#auth-and-exposure)) |
| `branch` | `main` | the branch every call works on; also `name@when` (see [`at`](#reading-the-past)) |

- **No timeouts or retries:** neither client sets one. Calls wait as long as `fetch` / `urllib.request.urlopen` do; wrap them (an `AbortSignal` isn't accepted) if you need a limit.
- **A client is one branch.** `fork`, `branch` and `at` return a new client; the old one is unchanged.
- **Its branch:** `db.branchName` (TypeScript, read only), `db.branch_name` (Python).

## Methods

TypeScript methods return promises. Python names are snake_case: `setMeta` is `set_meta`, `undoMerge` is `undo_merge`, `vectorField` is `vector_field`, `onlyTables` is `only_tables`, `onlyKeys` is `only_keys`, `skipChanged` is `skip_changed`. Each maps to one [HTTP op](../http-api.md#operations) on the client's branch.

| Method | Op | Returns |
|---|---|---|
| [`get(table, id)`](#rows) | `get` | the row, or `null` / `None` |
| [`put(table, id, record)`](#rows) | `put` | nothing |
| [`delete(table, id)`](#rows) | `delete` | nothing |
| [`batch(rows)`](#rows) | `batch` | rows written |
| [`find(table, options)`](#find) | `find` | `{total, hits, nextOffset}` |
| [`sql(sql, params)`](#sql) | `sql` | the last statement's `{command, rows}` |
| [`fork(name, meta?)`](#worlds) | `fork` | a client on the new branch |
| [`branch(name)`](#worlds) | none | a client on `name` |
| [`branches()`](#worlds) | `branches` | the branches |
| [`world()`](#worlds) | `world` | this branch |
| [`setMeta(meta)`](#worlds) | `set_meta` | this branch |
| [`diff()`](#worlds) | `diff` | the changes |
| [`discard()`](#worlds) | `discard` | nothing |
| [`merge(resolve, options)`](#merge) | `merge` | changes merged |
| [`preview(resolve, options)`](#merge) | `merge` with `dry_run` | the plan |
| [`undoMerge(world, options)`](#merge) | `undo_merge` | what was put back |
| [`at(when)`](#reading-the-past) | none | a client reading the past |
| [`history(limit = 100)`](#reading-the-past) | `history` | events, newest first |
| [`restore(at)`](#reading-the-past) | `restore` | rows written |
| [`simulate(worlds, prefix, script, score, options)`](#simulate) | `simulate` | the worlds, best first |
| [`replay(world, as?)`](#simulate) | `replay` | whether it came out identical |

### Rows

| Method | Arguments | Returns |
|---|---|---|
| `get(table, id)` | strings | the row object; `null` / `None` when there is no such row. Other 404s (no such branch) throw |
| `put(table, id, record)` | `record`: an object, the whole row | nothing |
| `delete(table, id)` | | nothing; a missing row throws 404 |
| `batch(rows)` | `[{table, id, record}]`; `record: null` / `None` deletes | the count. All rows or none |

`put`, `delete`, `batch` and a writing `sql` remember the branch's version, which `merge` sends (see [merge](#merge)).

### find

```ts
find(table, { where?, text?, vector?, vectorField?, limit?, offset? })
```
```python
find(table, where=None, text=None, vector=None, vector_field=None, limit=None, offset=None)
```

| Option | Default | |
|---|---|---|
| `where` | none | `{field: value}`, exact match on top-level fields, all must match |
| `text` | none | BM25 over the rows' string fields |
| `vector` + `vectorField` | none | rank by dot product with the vectors in that field; give both |
| `limit` | 20 (server) | |
| `offset` | 0 | |

Returns `{ total, hits: [{ id, score, row }], nextOffset }` in TypeScript, `{"total", "hits", "next_offset"}` in Python. `total` counts rows passing the filters; pass `nextOffset` as `offset` for the next page (`null` / `None` on the last). The clients don't send `ef`. See [search](../search.md).

### sql

```ts
sql(sql, params = [])      // { command: string, rows: R[] }
```
```python
sql(sql, params=None)      # {"command": ..., "columns": [...], "rows": [...]}
```

Runs one or more statements on the client's branch; `params` fill `$1`, `$2`, .... Returns only the last statement's result: `command` is its tag (`INSERT 0 1`, `SELECT 2`), `rows` are objects keyed by column name (the reply also carries `columns`, the names in order). Nothing ran: `{command: "", rows: []}`. Each call is its own session: `BEGIN ... COMMIT` must be within one call. Errors: status 400, 403 (42501), 404 (3D000, 42P01), 409 (23xxx), 413, 429, 500. See [SQL](../sql.md).

### Worlds

| Method | Arguments | Returns |
|---|---|---|
| `fork(name, meta?)` | `meta`: notes kept with the branch | a client on `name`, forked from this client's branch |
| `branch(name)` | | a client on `name` (no request) |
| `branches()` | | `[World]`, `_tx_` worlds left out |
| `world()` | | this branch's `World` |
| `setMeta(meta)` | an object merged into the metadata; `null` removes a key | the branch's `World` |
| `diff()` | | `[{ key, before, after, columns }]`: `before` null for an insert, `after` null for a delete |
| `discard()` | | nothing |

A `World` is `{ name, id, parent, depth, created, version, flagged, meta, owner, expires, active, pinned, checkpoints }` (`created` in ms since 1970; `checkpoints` `[{name, at}]`). The TypeScript `World` type declares the first eight.

The clients don't send `diff`'s `to` or `sql`, or `discard`'s `cascade`: use `sql("diff world ...")` or the [HTTP API](../http-api.md) for those.

### Merge

```ts
merge(resolve = "fail", { columns?, picks?, into?, onlyTables?, onlyKeys?, confirm? })
preview(resolve = "fail", { columns?, picks?, into?, onlyTables?, onlyKeys? })
undoMerge(world, { skipChanged? })
```
```python
merge(resolve="fail", confirm=False, columns=False, picks=None, into=None, only_tables=None, only_keys=None)
preview(resolve="fail", columns=False, picks=None, into=None, only_tables=None, only_keys=None)
undo_merge(world, skip_changed=False)
```

| Option | Default | |
|---|---|---|
| `resolve` | `fail` | on conflict: `fail`, `ours` (this branch's values) or `theirs` (the parent's) |
| `columns` | `false` | rows both sides changed merge column by column; only a column both changed conflicts |
| `picks` | none | `{"table/id": "ours" \| "theirs" \| {row} \| null}`; `null` / `None` deletes |
| `into` | the parent | merge into another branch; this branch stays |
| `onlyTables` | all | merge only these tables; this branch stays |
| `onlyKeys` | all | merge only these rows (`"table/id"`); this branch stays |
| `confirm` | `false` | after a crash, merge the flagged branch as it is now (check `diff()` first) |

- **`merge`** applies this branch to its parent and deletes it (unless `into`, `onlyTables` or `onlyKeys`). Returns the number of changes merged.
- **Version check:** `merge` sends the version from this client's last write (0 right after `fork`), so writes lost in a server crash fail with 409 instead of merging silently. A client made with `branch(name)` that hasn't written sends none. `confirm: true` sends none and takes the branch as it is.
- **Conflicts:** `ChronosError` with status 409 and `conflicts`: `[{ key, base, ours, theirs, explain }]`. Nothing was merged.
- **`preview`** changes nothing and returns `{ rows: [{ key, outcome, detail, base, ours, theirs, result }], conflicts, blocked }`: `conflicts` counts them, `blocked` is why constraints would refuse the merge, or `null`. Outcomes: [MERGE DRY RUN](worlds.md#merge-dry-run).
- **`undoMerge(world)`** puts back, in the parent, every row `world`'s latest merge changed. Returns `{ ok, parent, at, undone, skipped }`. Rows changed since fail it (409) unless `skipChanged`.

### Reading the past

| Method | Arguments | Returns |
|---|---|---|
| `at(when)` | a time (`"2026-09-20 10:00"`) or a time ago (`"-5 minutes"`) | a client on `<branch>@<when>`: reads the past, writes fail |
| `history(limit = 100)` | | `[{ at, world, event, rows }]`, newest first |
| `restore(at)` | a time, as for `at` | rows written: the branch put back as it was, as an ordinary write |

See [worlds](worlds.md#names-and-moments) for what `when` can be.

### Simulate

```ts
simulate(worlds, prefix, script, score, { keep?, order?, seed?, threads? })
replay(world, as?)
```
```python
simulate(worlds, prefix, script, score, keep=None, order="desc", seed=0, threads=None)
replay(world, as_name=None)
```

| Argument | Default | |
|---|---|---|
| `worlds` | | how many, more than 0 |
| `prefix` | | world `i` is named `<prefix>_<i>` |
| `script` | | SQL run in each world; `$1` is its index, `$2` its seed |
| `score` | | a `SELECT` giving one number per world |
| `keep` | all | worlds to keep, best first, or `"all"` |
| `order` | `desc` | `desc`: highest score best; `asc`: lowest |
| `seed` | 0 | seeds `random()` in every world |
| `threads` | every free core | |

Forks from this client's branch at one moment. `simulate` returns `{ worlds: [{ world, id, index, seed, score, error, kept }], base, at, timings: { fork_ms, run_ms, discard_ms, total_ms } }`, best first. `replay` returns `{ world, replay, identical, rows_differing, score, recorded_score, error }`; `as` / `as_name` keeps the replay as a new branch (`replay` is its name, else `null`). See [SIMULATE](worlds.md#simulate).

## Errors

Every non-2xx reply throws `ChronosError`:

| Field | TypeScript | Python | |
|---|---|---|---|
| message | `e.message` | `str(e)` | the server's message, which says what to fix |
| status | `e.status` | `e.status` | 400, 401, 403, 404, 409, 413, 429, 500 (see [HTTP API](../http-api.md#errors)) |
| conflicts | `e.conflicts` | `e.conflicts` | a merge's conflicting rows; `[]` otherwise |

- **No SQLSTATE:** the server's `sql` errors carry `code` (e.g. `42P01`), but `ChronosError` keeps only `status`, `message` and `conflicts`. Use a Postgres driver when you need the code.
- **Not `ChronosError`:** a network error (`TypeError` from `fetch`, `urllib.error.URLError`) or a reply that isn't JSON (`SyntaxError`, `json.JSONDecodeError`) throws the runtime's own error.

## Examples

TypeScript: fork, change, check, merge, retrying on a conflict.

```ts
import { Chronos, ChronosError } from "./chronos";

const db = new Chronos("http://127.0.0.1:7070", { token: process.env.CHRONOS_TOKEN });
const agent = await db.fork("agent-7", { task: "fix names" });
await agent.put("users", "1", { name: "Ada L." });
const { rows } = await agent.sql("select count(*) as n from users");
console.log(rows[0].n, await agent.diff());
try {
  await agent.merge();
} catch (e) {
  if (!(e instanceof ChronosError) || e.status !== 409) throw e;
  console.log(e.conflicts.map((c) => c.key));
  await agent.merge("ours");
}
```

Python: search, then write on a fork.

```python
import os
from chronos import Chronos, ChronosError

db = Chronos("http://127.0.0.1:7070", token=os.environ.get("CHRONOS_TOKEN"))
found = db.find("notes", where={"city": "blr"}, text="office", limit=5)
agent = db.fork("agent-8")
for hit in found["hits"]:
    agent.put("notes", hit["id"], {**hit["row"], "checked": True})
print(agent.preview()["conflicts"])
agent.merge()
```

## LangGraph and pytest (Python)

Two more files beside `chronos.py`, each needing only its framework.

[`chronos_langgraph.py`](https://github.com/Abhishekxdg/chronosdb/blob/main/clients/python/chronos_langgraph.py) (`pip install langgraph`): `ChronosSaver`, a checkpointer, and `ChronosStore`, a long-term memory store, on a branch. Give them a world: main is protected by default, and a world keeps the agent's state beside the rows it changed.

```python
from chronos import Chronos
from chronos_langgraph import ChronosSaver, ChronosStore

db = Chronos("http://127.0.0.1:7070", token=os.environ.get("CHRONOS_TOKEN"))
world = db.fork(f"run-{job_id}")
graph = builder.compile(checkpointer=ChronosSaver(world), store=ChronosStore(world))
graph.invoke(inputs, {"configurable": {"thread_id": job_id}})
print(world.diff())   # the rows it changed, and its checkpoints
world.merge()         # both land; world.discard() drops both
```

- Rows go in `langgraph_checkpoints`, `langgraph_writes` and `langgraph_store`, made on the first write. A checkpoint is one row (its channel values with it, as LangGraph's serde writes them); a task's pending writes are one `batch`, all or none.
- Forking the world forks the thread: each fork goes on from the same checkpoint. `world.at("-5 minutes")` reads the rows and the thread as they were.
- `ChronosSaver` passes LangGraph's checkpointer conformance suite (`langgraph-checkpoint-conformance`), except `prune` and `delete_for_runs`, which it doesn't offer. The async methods (`aget_tuple`, `aput`, ...) run the same calls on a worker thread.
- `ChronosStore.search(query=...)` is [find](#find)'s full-text search over the item's strings (or the fields a put's `index` names; `index=False` leaves it out), typos allowed. There is no vector search: a store given an embedding `index` is refused. Filters take values and `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`.

[`chronos_pytest.py`](https://github.com/Abhishekxdg/chronosdb/blob/main/clients/python/chronos_pytest.py) (`pip install pytest`): the fixture `chronos_world`, a fork of main for each test, discarded after it (a test that merges it is fine). Turn it on with `pytest -p chronos_pytest` or `pytest_plugins = ["chronos_pytest"]`; it reads `CHRONOS_URL` and `CHRONOS_TOKEN`, and skips the test without `CHRONOS_URL`.

```python
def test_agent_fixes_names(chronos_world):
    my_agent(chronos_world).run("fix the misspelt names")
    assert {c["key"] for c in chronos_world.diff()} == {"users/1", "users/7"}
```

More: [examples/agents](https://github.com/Abhishekxdg/chronosdb/blob/main/examples/agents) (tool definitions for LLM APIs, 20 agents merging at once), run in CI by `tests/clients.rs`.
