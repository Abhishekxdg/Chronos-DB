---
title: Worlds in SQL
---

Every statement that forks, compares, merges, rewinds or simulates worlds. Why they work the way they do: [concepts](../concepts.md#worlds). Other SQL: [SQL](../sql.md).

```sql
create world agent_7 with (task = 42);
switch world agent_7;
update orders set status = 'paid' where id = 7;
diff;                       -- what agent_7 changed
merge world agent_7;        -- into main; the session moves back to main
```

## Names and moments

- **World names:** a bare name (`agent_7`), a double-quoted name (`"agent-7"`), or a string (`'agent-7'`). A name with a dash must be quoted: `task-1` unquoted fails with 42601 `world names with a dash need quotes`.
- **Reserved:** a name can't be empty or contain `@` (42602), start with `_tx_` (42939: transactions use those), or be one of MERGE's own words, in any case: `using`, `ours`, `theirs`, `confirm`, `by`, `resolve`, `dry`, `into`, `only` (42939). `MERGE WORLD ours` reads `ours` as MERGE's word, so a world named so couldn't be merged unquoted. A world given such a name before this rule still loads; merge it quoted: `MERGE WORLD "ours"`. A MERGE that names no world while on `main` fails with 0A000 and says this.
- **A world ID** works wherever a world is read or switched to: `switch world '<id>'`, `fork world '<id>' as b`.
- **A world as it was:** `'name@when'`, read only. Works in `DIFF ... TO` (either side), `FORK WORLD` / `CREATE WORLD ... FROM`, `SIMULATE ... FROM`, and as a Postgres database name (the whole session reads the past; writes fail with 25006).
- **`when`** is one of:

| Form | Example | Meaning |
|---|---|---|
| a time | `2026-09-20 10:00`, `2026-09-20 10:00:00+02` | UTC unless it gives an offset |
| `now` | `now` | this moment |
| a time ago | `-15 minutes`, `-1 day`, `-2 hours` | a new moment each time it runs |

In `AS OF`, `RESTORE ... TO` and `UNDO AGENT ... SINCE`, `when` is an expression: a string as above, a `timestamp`, `timestamptz` or `date` value, or a parameter (`$1`). Anything else fails with 22P02 `AS OF needs a time`.

History reaches back `history_retention` (30 days by default). Earlier than that, later than now, or on an in-memory database: 22023.

## Statements

| Statement | Returns | Needs (agents) |
|---|---|---|
| [`CREATE WORLD` / `CREATE BRANCH`](#create-world) | tag `CREATE WORLD` | `fork` |
| [`FORK WORLD`](#fork-world) | tag `CREATE WORLD` | `fork` |
| [`SWITCH WORLD` / `USE WORLD` / `USE BRANCH`](#switch-world) | tag `SWITCH WORLD` / `USE BRANCH` | `read` |
| [`SHOW WORLDS`](#show-worlds) | rows | `read` |
| [`SHOW BRANCHES`](#show-branches) | rows | `read` |
| [`ALTER WORLD`](#alter-world) | the world's row, tag `ALTER WORLD` | own world; TTL and pin: `admin` |
| [`DROP WORLD` / `DELETE WORLD` / `DROP BRANCH`](#drop-world) | tag `DROP WORLD` / `DROP BRANCH` | own world(s) |
| [`DIFF`](#diff) | rows | `read` |
| [`MERGE WORLD` / `MERGE BRANCH`](#merge) | tag `MERGE n` | `merge`, or `merge_own` |
| [`MERGE ... DRY RUN`](#merge-dry-run) | rows | `read` |
| [`UNDO MERGE`](#undo-merge) | tag `UNDO MERGE n` | `restore` and `merge`, or `admin` |
| [`AS OF`](#as-of) | a table as it was | `read` |
| [`RESTORE WORLD`](#restore-world) | tag `RESTORE n` | `restore` |
| [`CHECKPOINT WORLD`](#checkpoint-world) | tag `CHECKPOINT` | own world |
| [`UNDO AGENT`](#undo-agent) | tag `UNDO AGENT n` | `restore` and `merge`, or `admin` |
| [`SHOW HISTORY`](#show-history) | rows | `read` |
| [`SIMULATE`](#simulate) | rows, tag `SIMULATE n` | `fork` (checked as it runs) |
| [`REPLAY WORLD`](#replay-world) | one row, tag `REPLAY` | checked as it runs |
| [`SHOW STORAGE`](#show-storage) | rows | `read` |
| [`SHOW DISK`](#show-disk) | rows | `read` |
| [`SHOW METRICS`](#show-metrics) | rows | `read` |
| [`ALTER SYSTEM SET history_retention`](#history-retention) | tag `ALTER SYSTEM` | `admin` |

- **Not inside a transaction:** `CREATE`/`FORK`/`DROP`/`SWITCH`/`USE`/`MERGE`/`ALTER WORLD`, `RESTORE`, `UNDO MERGE`, `UNDO AGENT`, `SIMULATE`, `REPLAY` and `ALTER SYSTEM` fail with 25001 between `BEGIN` and `COMMIT`.
- **Agents:** rights are listed in [concepts](../concepts.md#agents). A refused statement fails with 42501. "Own world" means one the agent forked.
- **MCP safe mode** refuses every statement above except the `SHOW`s, `DIFF`, `MERGE ... DRY RUN` and `AS OF` reads (see [MCP](mcp.md#safe-mode)).

## CREATE WORLD

```
CREATE WORLD name [FROM source] [WITH (key = value, ...) | META '<json object>']
CREATE BRANCH name [FROM source] [WITH (...) | META '...']
```

- **Source:** the session's world unless `FROM` names one: a name, an ID, or `'name@when'` (the new world holds that past; agents need `admin` for this).
- **Metadata:** `WITH` values are strings, numbers, `true`, `false` or `null`. `META` takes a JSON object. Keys starting with `_sonos` are the database's (22023). At most 64 KB as JSON (22023).
- **The session stays** on its world. Switch with `SWITCH WORLD`.

```sql
create world agent_7 from main with (owner = 'claude', task = 42);
create world check_it from 'main@-1 hour';
```

Errors: 42710 exists; 3D000 no such source; 53400 past `max_worlds` (see [CLI settings](cli.md#database-settings)); 42501 an agent past its own `max_worlds`; 22P02 `META` isn't JSON.

## FORK WORLD

```
FORK WORLD source [AS] name [WITH (key = value, ...) | META '<json object>']
```

`CREATE WORLD name FROM source` with the source first. Same errors.

```sql
fork world agent_7 as agent_7b meta '{"tags": ["retry"]}';
fork world 'main@2026-09-20 10:00' as before_the_deploy;
```

## SWITCH WORLD

```
SWITCH WORLD name
SWITCH BRANCH name
USE WORLD name
USE BRANCH name
USE name
```

The session's statements run on that world from now on. `name` is a world's name or ID (3D000 otherwise).

```sql
switch world agent_7;
use world '<its id>';   -- by ID, from SHOW WORLDS
```

Over the Postgres port the database name does the same: `psql postgres://127.0.0.1:5433/agent_7`. To read a past moment for a whole session, connect to the database `main@-1 hour` (read only; writes fail with 25006).

## SHOW WORLDS

```
SHOW WORLDS
```

One row per live world, main included (transactions' hidden worlds are left out).

| Column | Type | |
|---|---|---|
| `name` | text | |
| `id` | text | stable ID; main's is `main` |
| `parent` | text | null for main |
| `depth` | integer | forks from main |
| `created` | timestamptz | null for main |
| `version` | bigint | the world's write version |
| `meta` | jsonb | its metadata, without the database's own keys |
| `owner` | text | the agent that forked it, or null |
| `expires` | timestamptz | when it's discarded (TTL), or null |
| `active` | timestamptz | last used; null for main |
| `pinned` | boolean | never discarded for being idle |

## SHOW BRANCHES

```
SHOW BRANCHES
```

| Column | Type | |
|---|---|---|
| `name` | text | |
| `parent` | text | null for main |
| `changes` | bigint | rows changed since its fork; null for main |

## ALTER WORLD

```
ALTER WORLD name SET (key = value, ...)
ALTER WORLD name SET TTL '<interval>'
ALTER WORLD name RESET TTL
ALTER WORLD name SET PINNED
ALTER WORLD name RESET PINNED
```

| Form | Effect |
|---|---|
| `SET (k = v, ...)` | sets each key in its metadata; `null` removes one |
| `SET TTL '2 hours'` | discarded that long from now (unless it has forks then). No `=`. |
| `RESET TTL` | never expires |
| `SET PINNED` | never discarded for being idle (`world_idle_ttl`) |
| `RESET PINNED` | idle rules apply again |

Returns the world's row, with the `SHOW WORLDS` columns. TTL and pin forms on main fail with 22023. Agents need `admin` for TTL and pins, else the world must be their own.

```sql
alter world agent_7 set (task = null, reviewer = 'ada');
alter world agent_7 set ttl '1 day';
```

## DROP WORLD

```
DROP WORLD [IF EXISTS] name [CASCADE]
DELETE WORLD [IF EXISTS] name [CASCADE]
DROP BRANCH [IF EXISTS] name [CASCADE]
```

- **Without `CASCADE`:** refused while it has live forks (2BP01).
- **With `CASCADE`:** it and every world forked from it, deepest first.
- **The session's world:** if it's dropped, the session carries on from its parent.

Errors: 3D000 no such world (unless `IF EXISTS`); 2BP01 has forks; 0A000 main.

```sql
drop world agent_7 cascade;
```

## DIFF

```
DIFF
DIFF [WORLD | BRANCH] a
DIFF [WORLD | BRANCH] a TO b
... [AS SQL]
... READERS
```

| Form | Compares |
|---|---|
| `DIFF` | the session's world since its fork |
| `DIFF WORLD a` | `a` since its fork |
| `DIFF WORLD a TO b` | from `a` to `b`, any two worlds or moments (`'main@-1 hour'`): what turns `a` into `b` |

Rows:

| Column | Type | |
|---|---|---|
| `table` | text | the table (or view, sequence, function, schema) |
| `id` | text | the row's key; null for a change to the table itself |
| `change` | text | `insert`, `update`, `delete`; `create table`, `alter table`, `drop table`; `create`/`drop materialized view`; `create`/`drop`/`alter view`; `create`/`drop`/`alter sequence`; `create`/`drop`/`replace function`; `create`/`drop`/`alter schema` |
| `before` | jsonb | the row before, or null |
| `after` | jsonb | the row after, or null |
| `columns` | jsonb | the columns that changed; for a table, what changed in it (`"+email text"`, `"-age"`) |

With `AS SQL`: one column, `sql` (text), one statement per row (`INSERT`, `UPDATE`, `DELETE`, `CREATE`/`ALTER TABLE`, ...). Run them on the other side to make the same change.

```sql
diff world agent_7;
diff world 'main@-1 hour' to main as sql;
```

### DIFF ... READERS

What a change affects: what in the database reads each column the diff changes, so a reviewer sees what depends on a field, not only the rows that moved. Readers come from the world's own definitions (with `TO`, from `b`'s) and the merge checks.

| Column | Type | |
|---|---|---|
| `table` | text | the changed table |
| `columns` | text | the changed columns this reader reads (`status, score`) |
| `kind` | text | `view`, `materialized view`, `merge check`, `trigger`, `function`, `primary key`, `unique`, `foreign key`, `check`, `index`, `vector index`, `registered reader`, `client`; `coverage` for the last rows |
| `reader` | text | its name |
| `detail` | text | its definition, or how it reads (`reads every column of lists`, `leads references lists`, `UPDATE runs bump()`) |

- **How each is traced:** views, materialized views and merge checks column by column through their queries, with the views they read inlined, so a view on a view reaches the table. `*`, or an unqualified name several of a query's tables have, counts for every column it could mean. Keys, unique constraints, foreign keys, CHECK constraints and indexes by their columns, expressions and `WHERE`; a foreign key pointing at the table when its key changes or a row is deleted. Triggers when their event fires (`UPDATE OF` columns included). Functions where their text names the table and a changed column (or any column, when rows are added or deleted).
- **Rows added or deleted** change every column of their table; a column added (`ALTER TABLE ... ADD`) reaches only what reads every column.
- **Clients seen reading (`client`):** SQL over the Postgres protocol records which columns each client's statements read (a `SELECT`'s, an `UPDATE`'s or `DELETE`'s `WHERE` and `SET`), named by role, with the program's `application_name` when it gives one (`billing (nightly)`); over HTTP, by agent (or `http`). `detail` says when it last read one of these columns. A session records a column at most once a minute, so "last read" is that exact. Give each integration its own role, or at least its own `application_name`: clients sharing both show as one. Recorded reads are the database's, not a world's (a read of any world counts), kept in the folder's `reads` file (encrypted with the database), at most 100,000 (client, table, column) entries, the longest unseen dropped first.
- **Registered readers:** things that read outside SQL (an export, a sync tool reading a dump) are named by hand, in the world like a view (so forks carry them, and under a merge policy a merge that adds or drops one is a schema change):

  ```
  REGISTER READER name ON table [(column, ...)]    -- no columns: every column; adds to the tables it reads
  UNREGISTER READER name [ON table]                -- 42704 if it isn't registered
  SHOW READERS                                     -- reader, kind (registered | client | marked), table, columns,
                                                   -- last_read, critical, marked_by, marked_at
  ```

- **Critical readers, and blast radius:** `MARK READER billing CRITICAL` marks a reader, by name, of any kind: a client's role, a registered reader, a view, a function; `UNMARK READER billing` takes it away (42704 if it isn't marked). A mark lives in the world like a view: forks carry it, `DIFF` shows marking and unmarking (`mark critical`, `unmark critical`), it's kept with who marked it and when (`SHOW READERS`), and under a merge policy a merge that adds or removes one is a schema change a person reviews. `DIFF ... READERS` has a `critical` column and lists critical readers first, then the tables with the most readers, then the readers of the most changed columns. A merge policy's `critical` rule (default `false`) holds an agent's merge that changes a column a critical reader reads: `it changes leads.status, which critical client billing reads`.

- **Coverage, always:** the last three rows (`kind` = `coverage`) say what was traced, since when clients' reads have been recorded and through which doors (the JSON API's `get` and `find` aren't recorded), and that queries a function builds as strings for `EXECUTE` aren't followed. "No readers" means none found, not that nothing reads the field.

```sql
diff world cleanup readers;
```

```
 table |   columns    |       kind        |      reader        |               detail
-------+--------------+-------------------+--------------------+-------------------------------------
 leads | status       | check             | leads_status_check | CHECK (status IN ('new', 'won'))
 leads | status       | view              | open_leads         |
       |              | coverage          |                    | traced inside the database: views, ...
```

## MERGE

```
MERGE WORLD [name] [clause ...]
MERGE BRANCH [name] [clause ...]
```

`name` defaults to the session's world. Clauses, in any order:

| Clause | Effect |
|---|---|
| `INTO world` | merge into another live world instead of the parent, three-way against `name`'s fork point; `name` stays open |
| `ONLY TABLES (t, ...)` | merge just those tables; `name` stays open with the rest |
| `ONLY KEYS ('t/1', ...)` | merge just those rows; `name` stays open with the rest |
| `[USING] OURS` | a row both sides changed: keep this world's |
| `[USING] THEIRS` | a row both sides changed: keep the parent's |
| `BY COLUMNS` | a row both sides changed merges column by column; only a column both changed conflicts. A table set `ALTER TABLE t SET (merge_by_columns = true)` merges this way in every merge into a world that has the setting |
| `RESOLVE ('t/1' = OURS \| THEIRS \| DELETE \| '<row as JSON>', ...)` | settle rows one by one |
| `CONFIRM` | after a crash: merge the world as it is now (check `DIFF` first) |
| `DRY RUN` | change nothing; see [below](#merge-dry-run) |

- **Default:** a row both sides changed fails the merge with 40001, naming up to 10 rows; nothing is merged.
- **Order of settling:** `RESOLVE` for its rows, then `BY COLUMNS`, then `USING`.
- **Afterwards:** a whole merge into the parent drops the world. If it was the session's, the session moves to the parent.
- **Keys** of several columns are written `'t/(1,abc)'`.

Returns tag `MERGE n`, n the rows applied.

| SQLSTATE | When |
|---|---|
| 40001 | rows changed on both sides; or both sides changed one table's columns or constraints |
| 23505 | both sides added rows with the same unique values |
| 23503 / 23514 | the merged result breaks a foreign key or `CHECK` |
| 22023 | `INTO` itself; `ONLY KEYS` given something that isn't `'table/id'`; a row tied (unique value or reference) to one left out; a row of a table the world changed itself (use `ONLY TABLES`) |
| 55000 | the world was open when the database crashed: `CONFIRM` after checking `DIFF`, or drop it |
| 0A000 | main has no parent |
| 3D000 | no such world |

```sql
merge world agent_7 by columns resolve ('orders/7' = theirs, 'orders/9' = '{"id": 9, "status": "paid"}');
merge world agent_7 only tables (orders);
merge world agent_7b into agent_7;
```

See [concepts](../concepts.md#merging) for three-way merging and [partial merges](../concepts.md#merging-part-of-a-world-or-into-another-world).

### MERGE DRY RUN

```
MERGE WORLD [name] [clause ...] DRY RUN
```

What the merge would do with the same clauses, row by row. Changes nothing.

| Column | Type | |
|---|---|---|
| `table` | text | |
| `id` | text | null for a table, view or sequence itself |
| `outcome` | text | `apply`, `conflict`, `by columns`, `picked ours`, `picked theirs`, `picked row`, `kept ours`, `kept theirs`, or `blocked` |
| `detail` | text | why: for a conflict, which columns each side changed |
| `base` | jsonb | the row at the fork point |
| `ours` | jsonb | this world's row |
| `theirs` | jsonb | the parent's (or `INTO` world's) row |
| `result` | jsonb | the row after the merge; null for a conflict |

A last row with outcome `blocked` (and null `table`) means constraints would refuse the merge; `detail` says why.

### Stale reads (check_reads)

A merge conflicts on rows both sides *wrote*. A world that checks its reads is also held when rows it only *read* changed in the world it merges into since it forked: an agent that read a balance, decided on it and wrote a payout would otherwise merge a decision made on a balance that's gone.

```sql
create world payout with (check_reads = true);   -- only when forking: what it read before isn't known later
-- ... reads and writes in payout ...
merge world payout;
-- ERROR:  merging payout read rows that changed since it forked: accounts/1; nothing was merged: redo the work in a new world forked from now
alter world payout set (check_reads = false);    -- a person's way past it
```

- **What counts as read:** every row read through the world (a key looked up, and a scan's range, up to where it stopped when it stopped early), the entries of indexes and constraints it looked in (so a row the parent added that a lookup would have found counts too), and whole tables for full-text and vector searches. Past 10,000 keys or 1,000 ranges of one table, reads of it count as the whole table. Reads in a transaction count for its world. Rows the world wrote itself aren't checked here: they conflict or settle as always.
- **Refused or queued:** SQLSTATE `40001` (HTTP 409); for an agent keeping to a merge policy, a reason like any broken rule, and the world waits in `SHOW REVIEWS`. `MERGE ... DRY RUN` shows it in its `blocked` row.
- **For every world of an agent:** a merge policy with `check_reads = true` turns it on for every world its agents fork, and they can't turn it off (42501); a person can.
- **Restarts:** what a world read is kept in memory. After the database restarts, a world that checks its reads can't show its earlier reads were current, so its merge is held (`relies on reads made before the database restarted`): redo the work in a new world, or turn the check off.
- **Anyone reading it counts:** a person inspecting the world with `SELECT` adds to what it read.

### Declared scope (may_change, tenant)

An agent allowed to write a table can still overwrite the wrong rows of it. A world can say, when it's forked and before its first write, what it's meant to change. Its merge is then held to that:

```sql
create world close_stale with (
  may_change = 'leads.status, lists',  -- whole tables, or single columns
  tenant     = 'org_id = 42',          -- every row changed has org_id = 42, before and after
  intent     = 'close leads idle 90 days',
  run        = 'planner/3'             -- your own id for the run (a subagent's, a job's)
);
-- ... writes in close_stale ...
merge world close_stale;
-- ERROR:  merging close_stale changes leads.owner: leads/7; it changes leads rows outside tenant org_id = 42: leads/9,
--         outside the scope it was forked with (may change leads.status, lists; tenant org_id = 42); nothing was merged: ...
```

- **`may_change`:** a table named whole may be inserted into, deleted from, updated and altered. A column (`leads.status`) may only be updated. Names are read as SQL reads them (`"Leads"."Status"`), and each must be a table or column of the world it's forked from (42P01 otherwise). Leave it out to allow every table.
- **`tenant`:** `'column = value'`, where the value is a number, `true`/`false`, or text in single quotes (`'region = ''eu'''`); or `{"org_id": 42}` in `META`. A row counts when the column holds the value before the change (updates, deletes) and after it (inserts, updates), so moving a row out of the tenant is outside too. A table without the column is outside; one named in `may_change` without it is refused at the fork.
- **Everything else is outside:** views, functions, sequences, schemas, and a table's own changes when only its columns are listed or a tenant is set. `intent` and `run` are only shown.
- **Checked on what the world changed:** its own rows, as the merge applies them (after `ONLY TABLES` / `ONLY KEYS`). Columns the parent changed and a `BY COLUMNS` merge combines in don't count against it.
- **Refused or queued:** SQLSTATE `42501`; for an agent keeping to a merge policy, a broken rule (`..., outside its declared scope`), and the world waits in `SHOW REVIEWS`. `MERGE ... DRY RUN` shows it in its `blocked` row.
- **Changing it:** `ALTER WORLD w SET (may_change = 'leads')` widens it, `= null` drops a part. Only the database's own users and admins may; the agent that forked the world can't (42501).
- **Shown:** `SHOW WORLDS` and `SHOW REVIEWS` have a `scope` column (JSON: `may_change` as `[{"table": ..., "column": ...}]`, `tenant` as `{"column": ..., "value": ...}`, `intent`, `run`); the world's owner is the agent that forked it. Over HTTP and MCP a world has a `scope` field.
- **Required:** a merge policy with `require_scope = true` refuses its agents' forks that declare neither `may_change` nor `tenant`.

## UNDO MERGE

```
UNDO MERGE [OF] [WORLD | BRANCH] name [SKIP CHANGED]
```

Puts back, in the world it merged into, every row `name`'s latest merge changed, as an ordinary write. `name` is the world that was merged (it's gone; its name still works).

- **Rows changed again since:** 40001, naming up to 10; nothing is undone. `SKIP CHANGED` leaves those and undoes the rest.
- **History:** the merge must be within the retention window (22023 `no merge of ... within the history window`).

Returns tag `UNDO MERGE n`.

```sql
undo merge of world agent_7 skip changed;
```

## AS OF

```
FROM table AS OF when [alias]
FROM table [alias] AS OF when
FROM table FOR SYSTEM_TIME AS OF when
```

Reads a table as it was, with its schema as it was then; tables dropped since can be read. Works on any table reference in a query, so now and then join in one statement.

```sql
select now.id, now.status, old.status
from orders now join orders as of '-1 hour' old on old.id = now.id
where old.status <> now.status;
```

A fixed time is rebuilt from history once, then cached. See [concepts](../concepts.md#time-travel).

## RESTORE WORLD

```
RESTORE WORLD name TO when
RESTORE WORLD name TO CHECKPOINT 'label'
```

Writes the world's rows back as they were, as an ordinary write: history keeps what it replaced, and a restore of a fork merges like any change. Returns tag `RESTORE n`, n the rows written.

Errors: 22023 outside history, or no such checkpoint; 22P02 not a time.

```sql
restore world main to '-10 minutes';
restore world bot_task to checkpoint 'before';
```

## CHECKPOINT WORLD

```
CHECKPOINT WORLD name AS 'label'
```

Names this moment of the world, for `RESTORE ... TO CHECKPOINT`. The same label again moves it. Checkpoints show in the HTTP `world` op's `checkpoints`. Returns tag `CHECKPOINT`.

This is the SQL statement. The shell's own `checkpoint` command compacts the log instead (see [CLI](cli.md#shell-commands)).

## UNDO AGENT

```
UNDO AGENT name SINCE when [SKIP CHANGED]
```

In the session's world, puts back every row agent `name` changed since `when`.

- **Rows someone else changed after the agent:** 40001, naming up to 10; nothing is undone. `SKIP CHANGED` leaves those.
- **History** must reach back to `when` (22023).

Returns tag `UNDO AGENT n`.

```sql
undo agent bot since '-2 hours' skip changed;
```

## SHOW HISTORY

```
SHOW HISTORY [FOR [WORLD] name] [LIMIT n]
```

What happened to the world (the session's by default), newest first. `LIMIT` defaults to 100. A dropped world's name still works.

| Column | Type | |
|---|---|---|
| `at` | timestamptz | |
| `world` | text | |
| `event` | text | `write`, `commit`, `forked from w`, `forked w`, `merged into w`, `merged w`, `merged part into w`, `merged part of w`, `discarded`, `metadata changed` |
| `rows` | bigint | rows written; for a merge or commit, rows the merged world wrote |

## SIMULATE

```
SIMULATE n WORLDS [FROM base] AS prefix
  RUN 'script'
  SCORE 'select ...'
  [ASC | DESC] [KEEP k | KEEP ALL] [SEED s] [THREADS t]
```

Forks `n` worlds (`prefix_0` ... `prefix_{n-1}`) from one moment of `base`, runs the script in each in parallel, scores each, keeps the best and discards the rest. The clauses after `SCORE` go in any order.

| Part | Default | |
|---|---|---|
| `FROM base` | the session's world | a world, or `'name@when'` (agents need `restore` for a past moment) |
| `RUN` | required | any statements; `$1` is the world's index, `$2` its seed. Can't fork, merge, restore, switch or drop worlds, or change settings (0A000) |
| `SCORE` | required | one `SELECT` whose first value is a number |
| `DESC` / `ASC` | `DESC` | highest score first / lowest first |
| `KEEP` | `ALL` | worlds kept, best first |
| `SEED` | 0 | seeds `random()` and `gen_random_uuid()` with the world's index |
| `THREADS` | every free core | |

Strings can be dollar-quoted: `$$...$$` or `$tag$...$tag$`.

| Column | Type | |
|---|---|---|
| `world` | text | |
| `id` | text | |
| `index` | bigint | `$1` |
| `seed` | bigint | `$2` |
| `score` | double precision | null if it failed |
| `error` | text | the failure, with its SQLSTATE |
| `kept` | boolean | |

Rows are best first; failed worlds last. Kept worlds hold their inputs in metadata `sim` (`seed`, `index`, `script`, `score_query`, `score`, `error`, `base`, `base_id`, `at`).

| SQLSTATE | When |
|---|---|
| 22023 | 0 worlds; script and score over 32 KB together; the score isn't one `SELECT` |
| 54000 | more than 1,048,576 worlds |
| 42710 | a world named `prefix_i` exists |
| 53400 | not enough room under `max_worlds` (`KEEP k` needs k + 1 at once; `KEEP ALL` needs n) |
| 42501 | not enough room under the agent's `max_worlds` |
| 0A000 | the script leaves its world |

```sql
simulate 1000 worlds from main as trial
  run $$update prices set p = p * (0.9 + random() * 0.2) where sku % 100 = $1 % 100$$
  score $$select sum(p * sold) from prices$$
  keep 10 seed 42;
```

Determinism, batching and sizes: [SQL](../sql.md#simulations).

## REPLAY WORLD

```
REPLAY WORLD world [AS name]
```

Forks the kept world's base as it was then, runs its script and score again with the same index and seed, and compares. The replay is dropped unless `AS name` keeps it. Needs history back to the simulation's moment.

| Column | Type | |
|---|---|---|
| `world` | text | |
| `replay` | text | the kept replay, or null |
| `identical` | boolean | same rows and same score (or error) |
| `rows_differing` | bigint | |
| `score` | double precision | now |
| `recorded_score` | double precision | then |
| `error` | text | |

22023 if the world has no `sim` metadata, or it's incomplete.

## SHOW STORAGE

```
SHOW STORAGE [FOR [WORLD] name]
```

What each world (or one) costs on its own.

| Column | Type | |
|---|---|---|
| `world` | text | |
| `rows_changed` | bigint | since its fork; null for main |
| `pages` | bigint | pages only it holds |
| `bytes` | bigint | their bytes |

## SHOW DISK

```
SHOW DISK
```

Four rows of `what` (text) and `bytes` (bigint): `live` (pages in use), `history` (pages kept only for time travel), `reclaimable` (pages nothing needs), `log`. See [operations](../operations.md#cleanup).

## SHOW METRICS

```
SHOW METRICS
```

Columns `metric` (text), `count` (bigint), `total_ms`, `p50_ms`, `p95_ms`, `p99_ms`, `max_ms` (double precision). Timed operations first, then counters with null latencies. Names: [operations](../operations.md#metrics).

## History retention

```
ALTER SYSTEM SET history_retention = '<interval>'
ALTER SYSTEM SET history_retention TO '0'
SHOW history_retention
```

How far back time travel, `RESTORE`, `UNDO MERGE`, `UNDO AGENT`, `SHOW HISTORY` and `REPLAY` reach. Default `30 days`; `'0'` keeps none. Shorter settings free space at the next checkpoint. `SHOW history_retention` returns one `interval`. On an in-memory database: 22023.

## Agents

The statements that manage agents, whose worlds these rights govern:

```
CREATE AGENT name [WITH (key = value, ...)]      -- name, id, token (shown only now)
ALTER AGENT name SET (key = value, ...)          -- the agent's row, tag ALTER AGENT
DROP AGENT name                                  -- tag DROP AGENT; its worlds stay
SHOW AGENTS                                      -- name, id, can, max_worlds, writes_per_minute, max_changes,
                                                 -- world_ttl, created, disabled, max_query_ms, max_concurrent, max_memory_mb, policy
SHOW AUDIT [FOR [AGENT] name] [LIMIT n]          -- at, agent, world, action, rows (LIMIT 100 by default)
```

| Key | Value | Default |
|---|---|---|
| `can` | a list, `'read,fork,write_own'`: `read`, `fork`, `write_own`, `write`, `write_main`, `merge_own`, `merge`, `restore`, `admin` | `read,fork,write_own` |
| `max_worlds` | live worlds it may own at once | 0 (no limit) |
| `writes_per_minute` | | 0 (no limit) |
| `max_changes` | rows each of its worlds may change | 0 (no limit) |
| `world_ttl` | ms, or an interval string (`'1 day'`) | 0 (until merged or dropped) |
| `max_query_ms` | ms, or an interval string | 0 (no limit) |
| `max_concurrent` | statements at once | 0 (no limit) |
| `max_memory_mb` | working memory per statement | 0 (no limit) |
| `disabled` | `true` / `false` | `false` |
| `policy` | a merge policy's name (see below); `null` for none | none |

Names are letters, digits, `_` and `-`; `guest` and `system` are the database's (22023). An unknown key fails with 22P02. Only `admin` may manage agents.

### Merge policies

A person can't approve every change once agents make thousands of them. So people approve rules
instead (walkthrough: the [merge policies guide](../guides/merge-policies.md)): an agent that may merge (`merge_own` or `merge`) and keeps to a merge policy merges on its
own when the merge keeps to the rules, and is refused (42501) when it breaks any. The refused world
stays, queued for a person in `SHOW REVIEWS`; the person merges it, or drops it. The rules are
checked inside the merge, on the rows it's about to apply, so what's checked is what merges even
while the world and its parent keep changing.

```
CREATE MERGE POLICY name [WITH (key = value, ...)]  -- the policy's row, tag CREATE MERGE POLICY
ALTER MERGE POLICY name SET (key = value, ...)      -- the keys named change; the rest stay
DROP MERGE POLICY name [CASCADE]                    -- refused (22023) while an agent keeps to it, unless
                                                    -- CASCADE: then its agents keep to none (a NOTICE each)
SHOW MERGE POLICIES                                 -- name, max_rows, max_deletes, tables, review_tables, review_columns,
                                                    -- schema, overwrite, critical, check_reads, require_scope,
                                                    -- rows_per_hour, deletes_per_hour, max_age, created
SHOW REVIEWS                                        -- world, owner, policy, reasons, asked, version,
                                                    -- changed_since (written to, or partly merged, since the agent asked),
                                                    -- scope (what the world was forked to change)
ALTER AGENT name SET (policy = 'name')              -- or policy = null
```

| Rule | A merge needs a person when | Default |
|---|---|---|
| `max_rows` | it changes more rows than this | `null` (no limit) |
| `max_deletes` | it deletes more rows than this (`0`: any delete, the recommended start for any agent) | `null` (no limit) |
| `rows_per_hour` | it's a merge into `main`, and with the agent's other merges into `main` in the last hour it changes more rows than this | `null` (no limit) |
| `deletes_per_hour` | likewise for deleted rows (`50`: at most 50 deletes an hour, however the job is split) | `null` (no limit) |
| `max_age` | the world was forked longer ago than this (`'1h'`, `'30 minutes'`, or ms): its merge rests on what it read then | `null` (no limit) |
| `tables` | it changes a table not in this list (`'orders,items'`) | any table |
| `review_tables` | it changes any table in this list | none |
| `review_columns` | it changes any column in this list (`'users.email,accounts.owner'`), even in one row: an update changing it, an insert giving it a value, a delete of a row with one | none |
| `schema` | `false` and it changes a table itself, a view, function, sequence, schema or type | `false` |
| `overwrite` | `false` and it overwrites rows its parent changed since the fork (`MERGE ... OURS`, picked rows; rows combined `BY COLUMNS` don't count) | `false` |
| `critical` | `false` and it changes a column a reader marked critical reads (see [DIFF ... READERS](#diff--readers)) | `false` |
| `check_reads` | `true`: its agents' worlds check their reads, and a merge that read rows changed since the fork waits (see [stale reads](#stale-reads-check_reads)) | `false` |
| `require_scope` | `true`: its agents must declare `may_change` or `tenant` when forking (the fork is refused otherwise); a merge outside a world's declared scope waits whatever this says (see [declared scope](#declared-scope-may_change-tenant)) | `false` |

An agent keeping to a policy changes `main` only by merging: a direct write there, `RESTORE`,
`UNDO MERGE` or `UNDO AGENT` is refused (42501), since it would skip the rules. Rules count the
rows a merge applies (a row settled `USING THEIRS` changes nothing), and read table names as SQL
does: unquoted names fold to lowercase, `'orders,"Orders"'` names two tables. Merges into other worlds are checked too.
An agent's `MERGE ... DRY RUN` shows a `blocked` row when its merge would need a person. To try
rules on a world before an agent keeps to them, use MCP's `check_merge_policy`. Only
`admin` may manage policies; any agent may `SHOW MERGE POLICIES` and `SHOW REVIEWS`. A person, or an
agent without a policy, merges as its rights alone decide.

```sql
CREATE MERGE POLICY small WITH (max_rows = 500, max_deletes = 0, review_tables = 'payments');
ALTER AGENT bot SET (can = 'read,fork,write_own,write_main,merge_own', policy = 'small');
-- bot: small changes to orders merge on their own; a bulk update or any change to payments waits
SHOW REVIEWS;
MERGE WORLD bot_world;   -- a person approves
```

### Merge checks

A merge check is a query that must find nothing in what a merge would make: rules about the data,
where a merge policy's rules are about the change (walkthrough: the [merge checks guide](../guides/merge-checks.md)).
Every merge runs the checks for the tables it changes on the world it merges into, as the merge
would leave it: inside the merge, under its locks, before anything is written. If one finds rows,
the merge is refused (23514) with the rows as the reason, and nothing is merged. For an agent
keeping to a merge policy, a check broken is a rule broken: the merge is refused (42501) and queued
in `SHOW REVIEWS`. Two changes that are each fine alone but break a check together are caught,
because it's the merged result that's checked.

```
CREATE [OR REPLACE] MERGE CHECK name [ON TABLES (t, ...)] [WITH (timeout = '2s')] AS SELECT ...
DROP MERGE CHECK name                               -- 3D000 if there's none
SHOW MERGE CHECKS                                   -- name, tables, timeout_ms, query, created
```

- **What it runs on:** the world merged into as the merge would leave it: with `ONLY TABLES` or `ONLY KEYS`, only those rows merged; with `INTO`, the other world. A transaction's `COMMIT` isn't a merge, and neither are direct writes: checks don't run for them.
- **Which run:** with `ON TABLES`, only for merges that change one of those tables' rows or the table itself; without, for every merge. Table names are read as SQL reads them: `app.orders`, or `orders` found along the session's `search_path`; one that isn't a table on `main` is refused (42P01), since the check would never run.
- **Schemas:** the query runs with the default `search_path` (`public`), whatever the session that made it had, so name tables in other schemas in full (`select id from app.orders`).
- **What a check may be:** one `SELECT` (or `WITH ... SELECT`) that changes nothing: no `nextval`, `setval` or `pg_notify`, and no function made with `CREATE FUNCTION`, in it or in a view it reads (25006). It runs read only, as the database's own (not as the merging agent), and is run once on `main` when created, so one that can't run is refused then.
- **What it costs:** it runs within its `timeout` (default 1 second): a merge it doesn't finish in is refused, since the merge waits for it holding its locks. It stops at the 11th row it finds (a `LIMIT` of its own applies first); a reason shows the first 10 (`, and more` past them).
- **Dry runs:** `MERGE ... DRY RUN` runs the checks too, and its `blocked` row names the checks that would fail and the rows they find (over HTTP, `blocked`). A dry run with conflicts runs none: there's no merged result yet.
- **No one skips a check.** A person's merge, or an admin's, is refused like anyone's. A person approves a queued world by changing it until it passes, then merging it; an exception to the rule is made by changing the check. Only `admin` creates or drops checks; any agent may `SHOW MERGE CHECKS`. A check's row is a definition, so a merge bringing one in is a schema change under a merge policy.
- **Replaying the log** (at recovery, or rebuilding the past for time travel) doesn't run checks: those merges were checked when they happened.

```sql
CREATE MERGE CHECK lists_keep_leads ON TABLES (leads, lists) AS
  SELECT id FROM lists WHERE id NOT IN (SELECT list FROM leads);
MERGE WORLD cleanup;
-- ERROR:  merging cleanup breaks merge check lists_keep_leads, which finds (id = 3); nothing was merged
```
