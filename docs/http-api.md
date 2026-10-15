# HTTP API

`chronos serve <folder> [--listen 127.0.0.1:7070] [--token T] [--safe [--admin-token A]]`

- **Requests:** every operation is `POST /v1/<op>` with a JSON body, and replies are JSON. `GET /v1/health` answers without auth. An agent's token (see `create_agent`) acts as that agent: reads anywhere, changes only what it's allowed to (403 otherwise; 429 when writing too fast, running `max_concurrent` requests, or past the database's `max_worlds`; 413 when a query passes its `max_memory_mb`); its worlds are its own, and a person merges them unless it may.
- **Branch:** every operation takes `"branch"` (default `"main"`).
- **Rows** go in and come out as JSON objects.

## Auth and exposure

- **By default** the server listens on 127.0.0.1 only.
- **Any other address needs a token,** from `--token` or `CHRONOS_TOKEN`. Clients send it as `Authorization: Bearer <token>`, and a missing or wrong token gets `401`.
- **Without a server token,** a request with no `Authorization` header is the database's own user. A token that matches no agent (a typo, or a disabled or dropped agent's) still gets `401`.
- **Safe mode:** with `--safe`, a request without an agent's token (none, or the server's) acts as the agent `guest`: it reads anything, forks, and changes only worlds `guest` forked; merging, restoring, writing to main, dropping others' worlds and settings get `403`. The admin token (`--admin-token` or `CHRONOS_ADMIN_TOKEN`, sent as the bearer token) is the database's own user.
- **Web pages can't use it:** a request with an `Origin` header (a browser's, from a web page) gets `403`; a body must be sent with `Content-Type: application/json` (`415` otherwise); and without a server token, the `Host` must be `localhost`, `127.x.x.x` or `[::1]` (`403` otherwise, against DNS rebinding).
- **Metrics:** `GET /metrics` gives Prometheus' text format (see docs/operations.md). It needs the server's token if there is one; an agent's token gets `403`.
- **HTTPS:** start the server with `--tls-cert cert.pem --tls-key key.pem` (or `CHRONOS_TLS_CERT` and `CHRONOS_TLS_KEY`) and it serves HTTPS only. Without them it serves plain HTTP, which is fine on loopback or a private network. See docs/operations.md.

## Errors

```json
{ "error": "no branch 'x'; see: branches, or create it with: fork x" }
```

| Status | Meaning |
|---|---|
| 400 | bad input (the message says what's wrong) |
| 404 | no such branch or row |
| 409 | conflict: merge conflicts, a branch that already exists or has forks, or a stale branch (see below) |
| 500 | storage error |

`error` is a string, or an object with a `message` (plus `code`, the SQLSTATE, from the `sql` op, and `conflicts` from a merge): read `error.message ?? error`. The TypeScript and Python clients do this for you and raise `ChronosError`.

Other statuses (401 and 403 for tokens and capabilities, 413 for bodies over 32 MB, 429 for quotas, and the `sql` op's `{"error": {"message", "code"}}` body with its SQLSTATE) are listed in [errors](reference/errors.md).

A merge conflict returns the rows involved:

```json
{ "error": { "message": "1 rows changed on both sides; nothing merged. Retry with resolve: ours or theirs",
             "conflicts": [ { "key": "users/1", "base": {…}, "ours": {…}, "theirs": {…} } ] } }
```

## Operations

| Op | Body | Reply |
|---|---|---|
| `get` | `table`, `id` | `{id, row}`, or 404 |
| `put` | `table`, `id`, `record` (object) | `{ok, version}` |
| `delete` | `table`, `id` | `{ok, version}`, or 404 |
| `batch` | `rows: [{table, id, record \| null}]` (all or nothing; null deletes) | `{ok, count, version}` |
| `find` | `table`, `where` ({field: value}), `text`, `vector` + `vector_field`, `limit` (20), `offset`, `ef` (graph beam, see [search](search.md)) | `{total, hits: [{id, score, row}], next_offset}` |
| `fork` | `name`, `from` (default main; a name or a world ID), `meta` (object) | `{ok, world}` |
| `branches` (or `worlds`) |  | `{branches: [world]}`; a world is `{name, id, parent, depth, created, version, flagged, meta, owner, expires, active, pinned, checkpoints}` |
| `world` | `branch` (a name or a world ID) | `world` |
| `set_meta` | `branch`, `meta` (object; null removes a key) | `world` |
| `history` | `branch`, `limit` (100) | `{events: [{at, world, event, rows}]}` (newest first) |
| `create_agent` | `name`, `can` (list), `max_worlds`, `max_changes`, `writes_per_minute`, `world_ttl` (ms), `max_query_ms`, `max_concurrent`, `max_memory_mb`, `policy` (a [merge policy](guides/merge-policies.md)'s name, or `null`) | `{agent, token}` (the token only now) |
| `alter_agent` | the same, plus `disabled` | `{agent}` |
| `drop_agent` | `name` | `{ok}` |
| `agents` |  | `{agents: [...]}` |
| `audit` | `agent`, `limit` (100) | `{events: [{at, agent, world, action, rows}]}` |
| `backup` | `to` (a new folder on the server) | `{files, bytes}` |
| `verify` | | `{pages, bytes, log_records}` |
| `checkpoint` | `branch`, `name` | `{ok, at}`; `restore` takes `checkpoint` instead of `at` |
| `undo_merge` | `branch` (the world that was merged), `skip_changed: true` | `{ok, parent, at, undone, skipped}` |
| `restore` | `branch`, `at` (a time, or a time ago like `-10 minutes`) | `{ok, restored}` |
| `simulate` | `worlds`, `prefix`, `script`, `score`, `from` (default main; or `name@when`), `keep` (a number, or `"all"`: the default), `order` (`desc` or `asc`), `seed` (0), `threads` | `{worlds: [{world, id, index, seed, score, error, kept}], base, at, timings: {fork_ms, run_ms, discard_ms, total_ms}}`, best first. See [SQL](sql.md#simulations) |
| `replay` | `world` (one a simulation kept), `as` (keep the replay under this name) | `{world, replay, identical, rows_differing, score, recorded_score, error}` |

Any `branch` that is read (`get`, `find`, `sql`, `diff`) can be a world as it was: `main@2026-09-20 10:00` or `main@-1 hour`. `diff` also takes `to`: the changes from `branch` to `to`. Each change lists its changed `columns`; with `"sql": true`, `diff` returns `{sql: [...]}` instead: statements that make the change.
| `diff` | `branch`, `to`, `sql: true`, `count: true`, `limit`, `after`, `readers: true` | `{changes: [{key, before, after, columns}]}`; see below for `count` and pages. With `readers`, also `readers: [{table, columns, kind, reader, detail}]` and `coverage` for the whole diff, even on a page (see [DIFF ... READERS](reference/worlds.md#diff--readers)) |

For big diffs, `diff` can count or page without reading every change into memory:

- `"count": true` (alone): `{total, tables: {name: n}}`. Walks the diff once, keeping no rows.
- `"limit": n` and/or `"after": cursor`: `{changes, next}`, the first `n` changes with key after `after` (leave it out, or `null`, for the first page), in key order. `next` is the cursor for the next page, `null` on the last one. It reads only as far as the page, so memory follows `n`, not the size of the diff; a later page skips what came before without re-reading it. Add `"count": true` to get `total` and `tables` too (a full walk). Pages can't be combined with `"sql": true`.

Pages concatenated equal the unpaged `changes`. SQL has the same as `DIFF WORLD w [TO v] LIMIT n` (the first `n` changes); the MCP `diff` tool takes `count` and `after`.
| `merge` | `branch`, `resolve` (`fail`/`ours`/`theirs`), `version` or `confirm: true`, `columns: true`, `picks` (`{"table/id": "ours" \| "theirs" \| row \| null}`), `only_tables` / `only_keys` (arrays: merge just those; the branch stays), `into` (another world to merge into; the branch stays), `dry_run: true` | `{merged}`; 409 with `conflicts` (each with `explain`), or when the result breaks a [merge check](guides/merge-checks.md) (23514, the rows it found in the message); a dry run returns `{rows: [{key, outcome, detail, base, ours, theirs, result}], conflicts, blocked, readers, coverage}` (`blocked` names the checks that would fail; `readers`, what in the world merged into reads the columns it would change) |
| `discard` | `branch`, `cascade: true` (and every world forked from it) | `{ok}`; with cascade `{ok, discarded}` |

## Versions

`put`, `delete` and `batch` return the branch's version. Send the last one as `merge`'s `"version"`. If a server crash lost some of those writes, the merge answers `409` instead of merging less than you wrote.

After a crash, a branch that was open can't be merged without a `version`. Check its `diff`, then merge with `"confirm": true` to take it as it is, or discard it. See [concepts](concepts.md#versions-and-crashes).

The TypeScript and Python clients track versions for you.

## Example

```bash
curl -s -H 'Content-Type: application/json' localhost:7070/v1/fork -d '{"name":"agent-7"}'
curl -s -H 'Content-Type: application/json' localhost:7070/v1/put -d '{"branch":"agent-7","table":"users","id":"1","record":{"name":"Ada L."}}'
# {"ok":true,"version":1}
curl -s -H 'Content-Type: application/json' localhost:7070/v1/merge -d '{"branch":"agent-7","version":1}'
# {"merged":1}
```
