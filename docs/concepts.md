# Concepts

## Rows and tables

A database is a set of rows. Each row has a key of the form `table/id` and a value, usually a JSON object.

- **Canonical storage:** the shell, the HTTP API and MCP store JSON with sorted keys. `{"b":1,"a":2}` and `{"a":2,"b":1}` are the same row.
- **Tables** are just key prefixes. There's no schema to declare: `describe` works out fields, types and repeated values from the rows.

## Branches

`main` is the trunk. Any branch can be forked, and forking is O(1) whatever the size: the new branch shares all data with its source and stores only what it changes.

- **One writer per branch:** writes to different branches never wait for each other.
- **Readers never wait:** they read a snapshot.
- **Finishing a branch:** it's either **merged** into its parent or **discarded**. A branch with its own forks must finish those first, or be dropped with them (`DROP WORLD w CASCADE`). Merging only part of it, or into another world, leaves it open (see [merging](#merging-part-of-a-world-or-into-another-world)).

### Worlds

A branch is also called a **world**; the two words mean the same thing. Each world has:

- **A name**, which can be reused once the world is merged or discarded.
- **An ID** (`w` and 16 hex digits; `main` for main), given when it's forked and never reused. Anywhere a world is named in SQL or the HTTP API, its ID works too.
- **When it was created**, its **parent**, and its **depth** (forks between it and main).
- **Metadata:** a JSON object of your own, such as an owner, a task or tags, up to 64 KB. It's set when forking and can be changed later; it survives restarts.

Worlds nest as deep as you like: a world forked from a world forked from main reads just as fast as main.

- **Last activity** (`active` in `SHOW WORLDS`): its last write, fork, merge into it, or read. It survives restarts; a crash can forget reads since the last checkpoint.
- **Idle worlds:** with `ALTER SYSTEM SET world_idle_ttl = '7 days'`, a world unused that long is discarded (checked every 30 seconds), unless it's main, has forks of its own, or is pinned (`ALTER WORLD w SET PINNED`). An idle parent goes after its idle forks. Off by default.
- **At most:** `ALTER SYSTEM SET max_worlds = n` refuses forks past n live worlds (main and transactions don't count).
- **Isolated:** a write in one world never changes another world's rows or version, main's included. The one thing worlds share is the database's number counters (`serial`, identity and `CREATE SEQUENCE`): they belong to the database, not to any world, so numbers taken in forks never collide when they merge. Taking a number writes no world; main may see gaps where a fork took numbers, as Postgres shows gaps after a rolled-back insert. The counters are logged and kept in each checkpoint, so a restart or crash never hands a number out twice.

A world pays only for what it changes. On disk, a world with a few changed rows (up to 32 rows and 4 KB) keeps them as a short list in the checkpoint rather than pages of its own; bigger changes get pages, shared with everything they didn't change. 100,000 worlds with a row each take 24 MB, checkpoint in about a second and reopen in about a second.

## Agents

An **agent** is who does something: it has a name, an ID and a secret token, and it can do only what it's allowed to. Agents are made with `CREATE AGENT` (or `create_agent` over HTTP); the token is shown once.

- **What it may do** (`can`): `read` (any world), `fork`, `write_own` (change the worlds it forked), `write` (any world but main), `write_main`, `merge_own` (publish its own worlds into parents it may change, so `main` also needs `write_main`), `merge` (any world), `restore` (restore, undo a merge), `outbox` (work main's outbox), `unmask` (read [masked columns](sql.md#masked-columns) as they are), `admin` (everything, and managing agents). New agents get `read, fork, write_own`: they work on their own worlds, and a person reviews the diff and merges.
- **Its worlds:** a world an agent forks is its own (`owner` in `SHOW WORLDS`); no one can change that with metadata. With a `world_ttl`, its worlds are discarded when their time is up (checked every 30 seconds). Those discards, and idle worlds', are in the audit trail as `system`.
- **Quotas,** and over one it's refused (HTTP 403, or 429 for writing too fast):
  - `max_worlds`: its live worlds at once.
  - `max_query_ms`: how long any of its SQL statements may run. Its `statement_timeout` can go lower, never higher (SQLSTATE 57014).
  - `max_concurrent`: its statements and HTTP requests running at once; one more is refused (SQLSTATE 53300, HTTP 429).
  - `max_memory_mb`: the rows one of its statements may hold. Past it the statement stops (SQLSTATE 53200, HTTP 413) rather than growing the server; its sorts, groups and joins spill to disk at a quarter of it. Other agents' queries carry on.
  - `max_changes`: rows each of its worlds may change since its fork, across restarts. Every row counts, however it's written: SQL statements, transactions (at COMMIT), restores, and worlds merged into it. A transaction's rows are held until COMMIT before they're checked, so one huge transaction uses memory before it's refused.
  - `writes_per_minute`: each statement, batch or API write counts as one; `max_changes` is the limit on rows.
- **Merge policies:** rules under which an agent's merges go through without a person: `max_rows`, `max_deletes`, `tables`, `review_tables`, `schema`, `overwrite`. A merge breaking any is refused (42501) and its world waits in `SHOW REVIEWS` until a person merges or drops it. Start every agent with `max_deletes = 0`, so no delete merges without a person. The rules are checked inside the merge, on the rows it applies, so what's checked is what merges however fast the data changes. An agent keeping to a policy changes `main` only by merging. See the [guide](guides/merge-policies.md).
- **Merge checks:** SQL queries a merge's result must find nothing in ("every lead list keeps a lead"), run inside every merge on what it would make, so two changes that are each fine but break a rule together are caught. Rows found refuse the merge (23514), or queue it for an agent keeping to a policy. See the [guide](guides/merge-checks.md).
- **Who did what:** everything an agent writes is logged as its, and `SHOW AUDIT [FOR AGENT name]` (or `audit`) lists it: forks, writes, merges, discards, metadata. It lasts as long as history does (see the retention setting).
- **Undo, within the history window:** `UNDO AGENT bot SINCE '-2 hours'` puts back, row by row, everything the agent changed in the current world since then: its own writes, its transactions, and its worlds it merged in, with or without anyone approving. Everyone else's changes stay. A row someone changed after the agent is named and nothing changes, or with `SKIP CHANGED` it's left as it is and the rest goes back. Indexes follow, and unique and foreign keys are checked as in a merge. A merge a person ran of an agent's world is the person's: undo it with `UNDO MERGE`. It reaches back as far as history does: 1 hour by default for a new database (raise `history_retention` to undo further back).
- **Checkpoints:** `CHECKPOINT WORLD w AS 'before'` names a moment of a world, and `RESTORE WORLD w TO CHECKPOINT 'before'` goes back to it.
- **Tokens:** only a hash of each token is kept; disabling or dropping an agent stops its token. An agent acts as itself:
  - **over HTTP:** `Authorization: Bearer <its token>` (the server's own token is the guest, the admin token the database's own user; see the next item);
  - **over the Postgres protocol:** user = the agent's name, password = its token (`psql "postgres://bot:<token>@127.0.0.1:5433/main"`). Its SQL is checked statement by statement: `CREATE WORLD` makes a world it owns, it writes only where it may (transactions included), and merging or managing needs the right to;
  - **over MCP:** `chronos mcp <folder> --agent bot`. Tools it may not use (merge, restore) aren't listed, its forks stay its own across sessions, and what it does is in the audit trail. Without `--agent`, MCP's safe mode applies (see [MCP](reference/mcp.md#safe-mode)).
- **The guest on the server:** a client of `chronos serve` that proves the server token (`--token`), or no token on a server with only an admin token, acts as the agent `guest`: it reads anything, forks, and changes only worlds `guest` forked. It can't merge, restore, write to main, drop others' worlds, manage agents or change settings. All such clients share that one identity. A client with the `--admin-token` (HTTP bearer, or Postgres password) is the database's own user, and so is every client of a server with no token at all (loopback only). `--safe` makes every client that isn't an agent or the admin token the guest, loopback without a token too; `--unsafe` makes the server token the database's own user, as before 0.4. Agents keep their own rights.

## Time travel

Every world can be read as it was at any moment in the **retention window**: 1 hour by default for a new database (30 days for one made by an older release), set with `alter system set history_retention = '7 days'` (or `set_retention` in Rust; `0` keeps none). A moment is written `name@when` wherever a world is read: SQL (`AS OF`, `use world`), the database name in psql, `branch` in the HTTP API and MCP, the clients' `at()`. See [SQL](sql.md#time-travel).

- **How:** checkpoints are kept, with the log after them, for the window. A moment is rebuilt from the last checkpoint before it plus the log up to it (the log records the time as it goes), once, then cached. Rebuilding costs a replay of up to one checkpoint's worth of log (up to 64 MB): milliseconds to about a second.
- **To the millisecond, open worlds too:** a world's writes reach the log in batches (see [versions and crashes](#versions-and-crashes)), but each write keeps the time it was made, and reading the past logs every waiting batch first. `AS OF` a moment between two writes of a world not yet merged shows the first and not the second, as for any world.
- **What history costs:** the log for the window, and pages only older checkpoints still use.
- **Restore** writes a world's past rows back as an ordinary write, so it's durable, merges like any change, and history keeps what it replaced. **Forking the past** gives a world holding it: its diff is everything since, and merging it restores.
- **Number counters don't go back:** `serial` and sequence counters are the database's, not a world's, so neither reading the past nor restoring it rewinds them: numbers are never handed out twice. A world read `AS OF` a moment can't take numbers (it's read-only).
- **History** lists what happened to a world within the window: forks, writes, commits, merges, discards.
- **In memory:** databases not on disk keep no history.

## Simulations

A simulation tries many variations at once. It forks n worlds from one moment of a world, runs the same SQL script in each on every core, scores each with a query, keeps the best and discards the rest: `SIMULATE` in SQL, `simulate` over HTTP, MCP and the clients, `Db::simulate` in Rust.

- **Repeatable:** each world's random numbers come from the simulation's seed and the world's index, and its clock stands at the base moment. The same simulation gives the same worlds on any machine and any number of threads. `serial` numbers are the exception: they come from the database's counters, which every world shares so merges never collide, so they differ between runs.
- **Replayable:** a kept world records what made it (metadata `sim`). Replaying it rebuilds its base from history, runs the script again and checks the rows and score are the same.
- **Agents:** the worlds are the agent's, within its quotas.

See [SQL](sql.md#simulations).

## Merging

A merge is a three-way, row-by-row comparison between three versions of each row:
- **base:** the row when the branch was forked.
- **ours:** the row on the branch now.
- **theirs:** the row on the parent now.

For every row the branch changed:
- **Parent unchanged since the fork:** the branch's version is applied.
- **Parent changed it too:** it's a **conflict**, even if both sides wrote the same value. The branch computed its write from a value that's gone, so the first merge wins. (Equal values can be a coincidence: two transfers that each debit an account from 100 to 90 must not become one.)

When there are conflicts, nothing is merged. They come back as data (`key`, `base`, `ours`, `theirs`), and you retry with:
- `ours`: keep the branch's values.
- `theirs`: keep the parent's values.
- or redo the work on a fresh fork.

### Settling conflicts

A merge that finds conflicts changes nothing and says, for each row, what each side did ("both changed status", "deleted here, changed by the parent"...). Then:

- **Look first:** a dry run (`MERGE WORLD x DRY RUN`, `dry_run: true`, `preview()`) shows every row the merge would touch and what would happen to it, changing nothing.
- **Row by row:** settle each conflicting row as ours, theirs, a row you give, or deleted (`RESOLVE (...)`, `picks`).
- **By columns:** where the two sides changed different columns of a row, combine them (`BY COLUMNS`, `columns: true`). Off by default, per table: `ALTER TABLE leads SET (merge_by_columns = true)` makes every merge into that table combine such rows (turn it on where the columns don't depend on each other; `RESET (merge_by_columns)` turns it off). Two changes to the same column conflict, unless a side is named: `BY COLUMNS USING OURS` (or `THEIRS`, or `USING` on a table set to merge by columns) takes that side only for the columns both changed, and each column only one side changed keeps that side's value. A conflict whose sides changed different columns says so, and names the setting.
- **All at once:** `USING OURS` / `THEIRS` for whatever's left, as whole rows: `USING OURS` puts back the fork's value in a column only the parent changed. A dry run says, for each row both sides changed, the columns each changed (`columns`) and what each choice would discard.
- **Tables themselves:** if both sides changed one table's columns or constraints differently, no side can be picked (each side's rows follow its own columns). Merge one side, then redo the other's change. The same change on both sides merges.

Rows settled by picks or by columns are logged as the rows they became, so a restart replays the merge exactly.

### Merging part of a world, or into another world

- **Part:** `MERGE WORLD x ONLY TABLES (orders)` or `ONLY KEYS ('orders/7', 'orders/9')` merges just those rows. x stays open with the rest.
  - **What goes with them:** a table goes whole: its schema, rows, and unique, reference and index rows. A row takes its own unique and reference rows. If one of those is shared with a row left out (two rows swapped an email), or x changed the row's table itself, the merge is refused: add the other row, or use `ONLY TABLES`.
  - **Merged rows stay merged:** x's fork point moves on to the rows merged, so a later merge of the rest doesn't take them again, or call them conflicts because the parent now holds x's values. If either side changes one of them afterwards, it conflicts as usual. `DIFF` shows what's left to merge.
- **Into another world:** `MERGE WORLD x INTO y` applies x's changes to y, three-way with x's fork point as base: a row y changed since then is a conflict, settled the same ways. x stays open, and its changes are still to merge into its parent. It takes what x hasn't merged into its parent yet. `INTO` x's own parent is the ordinary merge.
- **Everything else is the same:** dry runs, settling conflicts, versions and `CONFIRM`, history, the audit trail and `UNDO MERGE` (which puts back the world merged into; rows x merged into its parent stay merged for x).
- **Agents** need the right to merge x, and to change y.

**Undoing a merge:** `UNDO MERGE OF WORLD x` finds x's latest merge in history (within the retention window), rebuilds its parent just before and after it, and writes back every row the merge changed, constraint rows included, as one ordinary write. If rows it changed have changed again since, it names them and does nothing; `SKIP CHANGED` undoes the rest.

## Versions and crashes

Every write to a branch returns the branch's **version**, the number of writes since the fork. Pass the last version you saw to the merge (`merge_at`, `"version"` over HTTP; the TypeScript and Python clients do this for you). If the branch isn't at that version, the merge is refused.

Why this matters: writes to `main` and merges are forced to disk before they return. Writes to other branches are not, for speed: each world keeps its writes and logs them together (up to 64 KB at a time) when something needs them there: a fork from it, a merge of it or into it, a discard, a write by another agent, a checkpoint, a read of history or the past, or a clean shutdown. So a crash can lose a branch's writes since then, even when later writes to `main` were synced; each world comes back as it was after some of its own writes, in order. That's the trade, and it's safe only because it's never silent:

- **Merge with a version:** if a crash rolled the branch back, the merge fails with "N of your writes were lost; redo them or discard".
- **Merge without a version:** after a crash, every branch that was open is **flagged**, and a plain merge is refused. Check its `diff`, then merge with its current version (`merge <b> confirm` in the shell, `"confirm": true` over HTTP or MCP), or discard it.
- **Clean shutdowns** flag nothing.

The flag survives restarts and checkpoints until the branch is merged or discarded.

## Durability, in one table

| Operation | Safe when it returns? |
|---|---|
| write to `main` | yes (fsynced; concurrent callers share one fsync) |
| merge into `main` | yes |
| merge into another world | no, until that world reaches `main` |
| write to another branch | no, until merged; a crash can lose it (a later `main` commit doesn't save it), and the merge will say so |
| fork, discard | no, until the next fsync; a lost fork is simply gone |
| `batch` | all rows or none |

"Fsynced" follows `alter system set synchronous_commit` (kept across restarts):
- `full` (the default): through the drive's cache to stable storage (`F_FULLFSYNC` on macOS). Survives power loss.
- `normal`: a plain `fsync`. On macOS the drive may still hold the write in its cache, which is what Postgres does there by default. On Linux it is the same as `full`.
- `off`: handed to the OS only. Survives the process crashing, not the machine.

## Search

Search runs over one table of one branch: filters, text, vectors, or any mix of them. See [search.md](search.md). Each branch sees its own changes immediately. Indexes are shared between branches, so a fork searches almost as fast as `main`.

## Storage

A branch is a tree of immutable pages on disk plus a small in-memory layer of changes since the last checkpoint. Checkpoints fold those layers into new pages, writing only what changed. Pages are named by the hash of their content, so identical pages are stored once across branches.

See [operations.md](operations.md) for the folder layout, cloud storage and cleanup.
