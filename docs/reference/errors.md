---
title: Errors
---

The SQLSTATE codes Chronos returns, and how an error reaches each kind of client. Codes follow Postgres, so drivers and ORMs that branch on them keep working; the Chronos-specific causes (worlds, merges, agents, limits) reuse the nearest Postgres code. Messages say what went wrong and usually what to do next.

## How errors reach clients

| Client | What it gets |
|---|---|
| Postgres protocol | an `ErrorResponse` with `S` and `V` (`ERROR`), `C` (the SQLSTATE), `M` (the message) and, where the error has them, `D` (detail), `H` (hint), `P` (position: a syntax error's place in the query, counted in characters from 1) and `s`, `t`, `c`, `n` (the schema, table, column and constraint a violation names). `W` (where) is never sent. Notices (`RAISE NOTICE`, `... already exists, skipping`) come as `NoticeResponse` with the same fields |
| HTTP API | a status code and a JSON body with `error` (see [below](#http)) |
| MCP | a failed tool call is a normal JSON-RPC result with `isError: true` and the message as its text content; no SQLSTATE (see [below](#mcp)) |
| Shell | `error: <message>` on stderr, no SQLSTATE; a one-shot command or script exits 1 (see [CLI](cli.md)) |

- **In a transaction,** any error aborts it: later statements fail with 25P02 until `ROLLBACK` (or `COMMIT`, which rolls back).
- **A failed merge merges nothing,** whatever the error.

## Merge conflicts

A merge whose rows both sides changed is refused, not half done:

- **`MERGE WORLD`** fails with 40001, naming up to 10 of the rows; nothing is merged. Settle them with `USING OURS`, `USING THEIRS`, `BY COLUMNS` or `RESOLVE (...)`.
- **`MERGE WORLD ... DRY RUN`** returns the conflicts as data: one row per key, with `outcome = 'conflict'` and each side's row. It's how to inspect them before choosing.
- **HTTP `merge`** answers 409 with the conflicting rows in the body (`dry_run: true` returns them as a normal reply).
- **MCP `merge`** returns an error result listing each row's base, ours and theirs, and the tool arguments that settle them; `merge_preview` shows them without trying.

Details: [worlds](worlds.md#merge).

## SQLSTATE codes

### Class 08: connection

| Code | Meaning | Typical causes |
|---|---|---|
| 08P01 | protocol violation | a malformed message; a message out of place during `COPY FROM STDIN`; a statement given the wrong number of parameters |

### Class 0A: feature not supported

| Code | Meaning | Typical causes |
|---|---|---|
| 0A000 | feature not supported | SQL Chronos doesn't do (see [Postgres compatibility](../postgres-compatibility.md)); merging or dropping `main` (it has no parent); geometry beyond 2D points |

### Class 0Z, 20, 21, 2F, P0: PL/pgSQL and subqueries

| Code | Meaning | Typical causes |
|---|---|---|
| 0Z002 | stacked diagnostics accessed without active handler | `GET STACKED DIAGNOSTICS` or a bare `RAISE` outside an exception handler |
| 20000 | case not found | a PL/pgSQL `CASE` with no matching `WHEN` and no `ELSE` |
| 21000 | cardinality violation | a subquery used as a value returned more than one row; `ON CONFLICT DO UPDATE` would change one row twice in a statement |
| 2F005 | function executed no return statement | a function or trigger reached its end without `RETURN` |
| P0001 | raise exception | `RAISE EXCEPTION` without its own `ERRCODE` |
| P0002, P0003 | no data found, too many rows | `SELECT ... INTO STRICT` |
| P0004 | assert failure | `ASSERT` |

`RAISE ... USING ERRCODE` takes Postgres's condition names or any five-character code, which the client then gets.

### Class 22: data exception

| Code | Meaning | Typical causes |
|---|---|---|
| 22023 | invalid parameter value | a setting out of range (`statement_timeout`, `hnsw.ef_search`); time travel past the retention window, into the future, or on an in-memory database; world metadata over 64 KB or with `_sonos` keys; a TTL or pin on `main`; bad arguments to JSON and other functions |
| 22P02 | invalid text representation | text that isn't a value of the column's type; `AS OF` given something that isn't a time; `META` that isn't JSON; a bad interval |
| 22003 | numeric value out of range | integer or numeric overflow |
| 22012 | division by zero | |
| 22000 | data exception | vectors of different dimensions |
| 22004 | null value not allowed | a null where a function needs a value |
| 22008 | datetime field overflow | a date or timestamp out of range |
| 2200H | sequence generator limit exceeded | `nextval` past a sequence's maximum or minimum |
| 2201B | invalid regular expression | a pattern that doesn't parse |
| 2201E, 2201F | invalid argument for logarithm, power | `ln(0)`, `sqrt(-1)` |
| 2201W | invalid row count in LIMIT | a negative `LIMIT` or `OFFSET` |
| 22011, 22013, 22014, 22016 | substring, window frame, ntile, nth_value arguments | a negative length or size, or a non-positive argument |
| 22021 | invalid byte sequence | bytes that aren't UTF-8 |
| 22025 | invalid escape sequence | a `LIKE` or `SIMILAR TO` escape of more than one character |
| 2202E | array subscript error | |
| 22P04 | bad COPY file format | an unterminated quoted CSV field, extra columns |

### Class 23: integrity constraint violation

| Code | Meaning | Typical causes |
|---|---|---|
| 23502 | not null violation | |
| 23503 | foreign key violation | also a merge that would leave a row pointing at one the other side deleted |
| 23505 | unique violation | also a merge where both sides added rows with the same unique values |
| 23514 | check violation | a `CHECK`; a row whose key columns don't match its id; a merge whose result a [merge check](../guides/merge-checks.md) finds rows in (`merging w breaks merge check c, which finds (id = 3); nothing was merged`), or can't finish within its timeout |

A merge that would break a constraint, though each side was valid alone, fails with that constraint's code and merges nothing.

### Class 25: invalid transaction state

| Code | Meaning | Typical causes |
|---|---|---|
| 25001 | active SQL transaction | a world statement (`CREATE`/`FORK`/`DROP`/`SWITCH`/`MERGE`/`ALTER WORLD`, `RESTORE`, `UNDO`, `SIMULATE`, `REPLAY`) or `ALTER SYSTEM` between `BEGIN` and `COMMIT` |
| 25006 | read-only SQL transaction | a write to a world as it was (`'name@when'`, and a connection to one as its database) |
| 25P01 | no active SQL transaction | `SAVEPOINT`, `RELEASE` or `ROLLBACK TO` outside a transaction |
| 25P02 | in failed SQL transaction | a statement after an error in the same transaction |

### Classes 26, 34, 3B, 3D, 3F: missing objects

| Code | Meaning | Typical causes |
|---|---|---|
| 26000 | invalid SQL statement name | `EXECUTE` or `DEALLOCATE` of a prepared statement that doesn't exist |
| 34000 | invalid cursor name | an extended-protocol portal that doesn't exist |
| 3B001 | invalid savepoint | `ROLLBACK TO` or `RELEASE` of a savepoint that doesn't exist |
| 3D000 | invalid catalog name | **the world doesn't exist:** a statement naming one, or a Postgres connection whose database name isn't a world (the connection then closes) |
| 3F000 | invalid schema name | a schema that doesn't exist; a `search_path` with nowhere to create |

### Class 28: authorization

| Code | Meaning | Typical causes |
|---|---|---|
| 28000 | invalid authorization specification | the server has TLS and the client didn't ask for it: connect with `sslmode=require`; a role that may not log in (`NOLOGIN`); a SCRAM channel binding that doesn't match the server's certificate |
| 28P01 | invalid password | a password that is neither the server's token nor the agent's (the user name) token, or not the role's own; a role without a password where the server has a token, or past its `VALID UNTIL`; see [security](../security.md) |

### Class 2B: dependent objects

| Code | Meaning | Typical causes |
|---|---|---|
| 2BP01 | dependent objects still exist | dropping a world with forks of its own (merge or drop them, or `DROP WORLD ... CASCADE`); dropping a schema, function or other object something depends on without `CASCADE` |

### Class 40: transaction rollback

| Code | Meaning | Typical causes |
|---|---|---|
| 40001 | serialization failure | **merge conflicts** (see [above](#merge-conflicts)); both sides changed one table's columns or constraints; `UNDO MERGE` or `UNDO AGENT` when rows changed again since (`SKIP CHANGED` undoes the rest). Counted by the `conflicts` metric |

### Class 42: syntax error or access rule violation

| Code | Meaning | Typical causes |
|---|---|---|
| 42501 | insufficient privilege | **an agent may not:** it lacks the capability (`agent x may not ...: it needs 'merge'`), the world isn't its own, it's past its own `max_worlds` or its world's `max_changes`, it's disabled, or no agent has the token; safe mode's guest doing more than forking and changing its own worlds; a merge its agent's [merge policy](../guides/merge-policies.md) sends to a person (`merging w needs a person's review (merge policy p: it ...)`), merge checks it breaks included, or a direct write to main by an agent keeping to one; tables starting `_sonos`; `COPY` to or from a server file or program |
| 42601 | syntax error | SQL that doesn't parse; an unquoted world name with a dash (`task-1`); an unknown `SHOW` |
| 42602 | invalid name | a world name that is empty or contains `@` |
| 42939 | reserved name | a world name starting `_tx_` (transactions use those); a schema name starting `pg_` |
| 42710 | duplicate object | a world, agent or other object that already exists |
| 42P07 | duplicate table | |
| 42701 | duplicate column | |
| 42P05, 42P06, 42723 | duplicate prepared statement, schema, function | |
| 42P01 | undefined table | |
| 42703 | undefined column | |
| 42704 | undefined object | an index, constraint, type, trigger, window or exception condition that doesn't exist |
| 42883 | undefined function | a function or operator with no match for the argument types |
| 42P02 | undefined parameter | `$n` past the parameters given |
| 42702, 42725, 42712 | ambiguous column, function, alias | a column name two tables have; a table named twice without an alias |
| 42804 | datatype mismatch | a value of the wrong type |
| 42803 | grouping error | an aggregate where it isn't allowed, a column outside `GROUP BY` |
| 42809 | wrong object type | a window function without `OVER`; a statement on the wrong kind of relation |
| 42830 | invalid foreign key | a foreign key to a table with no primary key, or to other columns |
| 42P10 | invalid column reference | `COPY` options naming columns it doesn't copy; an `ON CONFLICT` target that isn't the primary key or a `UNIQUE` constraint |
| 42P13, 42P16, 42P17, 42P20 | invalid function, table, object, window definition | |

### Class 53: insufficient resources

| Code | Meaning | Typical causes |
|---|---|---|
| 53100 | disk full | a spill file couldn't be written (see `CHRONOS_SPILL_DIR` in [CLI](cli.md#environment-variables)) |
| 53200 | out of memory | a query past its agent's `max_memory_mb`: narrow it or add a `LIMIT` |
| 53300 | too many connections | an agent running `max_concurrent` statements already; a Postgres client past the server's 1000 connections (refused before login) |
| 53400 | configuration limit exceeded | **quota exceeded:** an agent past `writes_per_minute`; a fork past the database's `max_worlds` (see [database settings](cli.md#database-settings)) |

### Class 54: program limit exceeded

| Code | Meaning | Typical causes |
|---|---|---|
| 54000 | program limit exceeded | `generate_series()` or another set function past 10,000,000 rows; a `SIMULATE` with too many worlds |
| 54001 | statement too complex | **a regular expression that backtracks too long** (a step budget stops it rather than hanging the query); functions and triggers nested too deep; a `WITH RECURSIVE` past its round or row limit (a missing stop condition) |

### Class 55: object not in prerequisite state

| Code | Meaning | Typical causes |
|---|---|---|
| 55000 | object not in prerequisite state | a world open when the database crashed, which may have lost its last writes: check `DIFF`, then `MERGE WORLD ... CONFIRM` or drop it; a world not at the version a merge expected; `currval` or `lastval` before `nextval`; a materialized view not yet refreshed; a PL/pgSQL record not yet assigned |

### Class 57: operator intervention

| Code | Meaning | Typical causes |
|---|---|---|
| 57014 | query canceled | **`statement_timeout`** (or an agent's `max_query_ms`) passed: `canceling statement due to statement timeout`; a cancel request from the client; a client's `CopyFail` during `COPY FROM STDIN` |

### Classes 58 and XX: system and internal errors

| Code | Meaning | Typical causes |
|---|---|---|
| 58030 | I/O error | reading or writing storage failed: the folder or object storage is damaged or unavailable. Check with `VERIFY DATABASE` |
| XX000 | internal error | a migration file that failed (`migrate`); a geometry that doesn't parse |
| XX001 | data corrupted | a stored table schema, view or function definition that can't be read |

## HTTP

Errors are JSON with an `error` field:

```json
{ "error": "no branch 'x'; see: branches, or create it with: fork x" }
```

The `sql` operation gives the SQLSTATE too:

```json
{ "error": { "message": "table \"userz\" does not exist", "code": "42P01" } }
```

A merge conflict gives the rows (each with `key`, `base`, `ours`, `theirs` and `explain`):

```json
{ "error": { "message": "1 rows changed on both sides; nothing merged. Retry with resolve: ours or theirs, columns: true, or picks (see dry_run: true)",
             "conflicts": [ { "key": "users/1", "base": {…}, "ours": {…}, "theirs": {…}, "explain": "…" } ] } }
```

| Status | Meaning |
|---|---|
| 400 | bad input: a body that isn't JSON or lacks a field, an invalid value; merging `main`; for `sql`, any SQLSTATE not listed below (including 40001 and 57014) |
| 401 | missing or wrong token (a token no agent has, or a disabled or dropped agent's) |
| 403 | an agent or the guest (a client with the server token) may not (42501); `_sonos` tables; `/metrics` with an agent's token, `merge_outcomes` with any but the admin token; any request with an `Origin` header (browsers); without a server token, a `Host` that isn't loopback |
| 404 | no such world or row; for `sql`, 3D000 and 42P01; an unknown endpoint or operation |
| 405 | not `POST` (only `/v1/health` and `/metrics` take `GET`) |
| 409 | merge conflicts; a world that already exists or has forks; a stale world (55000); a constraint violation (23xxx) |
| 411 | no `Content-Length` |
| 413 | a body over 32 MB; a query past its agent's `max_memory_mb` (53200) |
| 415 | a body without `Content-Type: application/json` |
| 429 | an agent writing too fast or running `max_concurrent` requests, or a fork past `max_worlds` (53400, 53300) |
| 431 | request headers too large |
| 500 | storage error (58030; for `sql`, any 58xxx or XXxxx) |
| 503 | the server already has 1000 connections |

## MCP

- **A tool that fails** returns `{"content": [{"type": "text", "text": "<message>"}], "isError": true}`: the agent reads the message and can act on it. Safe mode's refusals (`safe mode: agents can't merge...`) come the same way.
- **JSON-RPC errors** are only for the protocol: -32700 for a line that isn't JSON, -32601 for an unknown method. An unknown tool is a tool error (`unknown tool '<name>'; see tools/list`).
