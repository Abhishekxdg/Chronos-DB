---
title: Give every agent its own world
---

You'll build a small shop in `main`, give three agents a world each, see what each one changed, merge the best, settle a conflict, and throw the rest away. Nothing an agent does reaches `main` until you merge it.

Everything below runs in `psql` in a few minutes. The whole script is [examples/guides/agent-sandbox.sql](https://github.com/Abhishekxdg/chronosdb/blob/main/examples/guides/agent-sandbox.sql).

## Setup

Start a server on a new folder and connect to it. The database name is the world you start in.

```bash
CHRONOS_PROTECT_MAIN=off chronos serve shopdb  # HTTP on 7070, Postgres on 5433; the setup writes main directly
psql postgres://127.0.0.1:5433/main
```

Or run the whole guide at once:

```bash
psql -v ON_ERROR_STOP=1 -f examples/guides/agent-sandbox.sql postgres://127.0.0.1:5433/main
```

Make the shop:

```sql
create table products (
  sku   int primary key,
  name  text not null,
  price numeric(10, 2) not null,
  stock int not null
);
insert into products values
  (1, 'Espresso beans 1kg', 18.00, 40),
  (2, 'Filter papers (100)', 3.50, 900),
  (3, 'Burr grinder', 89.00, 150),
  (4, 'Milk jug', 14.00, 12),
  (5, 'Descaler', 7.00, 300);
```

## 1. Fork a world per agent

```sql
create world agent_a with (bot = 'pricer', task = 'discount slow movers 10%');
create world agent_b with (bot = 'copywriter', task = 'tidy names and prices');
create world agent_c with (bot = 'janitor', task = 'remove dead stock');
show worlds;
```

A fork is O(1): each world shares all of `main`'s data and stores only what it changes. `WITH (...)` is metadata of your own (up to 64 KB), shown in the `meta` column of `SHOW WORLDS`. The `owner` column is filled in only when an [agent](../concepts.md#agents) logged in as itself makes the world.

## 2. Let each agent work

`SWITCH WORLD` moves this connection, and only this one, to another world. In a real setup each agent connects straight to its own world instead: `psql postgres://127.0.0.1:5433/agent_a`.

```sql
switch world agent_a;
update products set price = price * 0.9 where stock > 100;

switch world agent_b;
update products set name = 'Burr grinder, 40 settings' where sku = 3;
update products set name = 'Milk jug 600 ml' where sku = 4;
update products set price = 6.99 where sku = 5;

switch world agent_c;
delete from products where stock < 20;

switch world main;
select * from products order by sku;   -- main is as it was
```

Writes to different worlds never wait for each other, and none of them changed `main`.

## 3. See what each agent did

```sql
diff world agent_a;
diff world agent_b;
diff world agent_c;
```

`DIFF` returns one row per changed row: `table`, `id`, `change` (`insert`, `update`, `delete`), `before` and `after` (jsonb), and `columns` (what changed). Abbreviated, without `before` and `after`:

```
-- diff world agent_a
 products | 2 | update | ["price"]
 products | 3 | update | ["price"]
 products | 5 | update | ["price"]
-- diff world agent_c
 products | 4 | delete | []
```

The janitor deleted a product that's merely low on stock. That's the one to throw away.

To read a change as the SQL that makes it:

```sql
diff world agent_a as sql;   -- one statement per row: run them on main to make the same change
```

## 4. Merge the best one

Look first. A dry run shows every row the merge would touch and what would happen to it, and changes nothing:

```sql
merge world agent_a dry run;   -- table, id, outcome, detail, base, ours, theirs, result, postgres, columns
merge world agent_a;           -- MERGE 3
```

`main` hadn't changed since the fork, so all three rows apply. The world is gone after the merge.

## 5. Settle a conflict

The copywriter's world was forked before the pricer's merge. It renamed the grinder (whose price `main` now changed) and repriced the descaler (whose price `main` also changed). A merge compares three versions of each row: **base** (when the world was forked), **ours** (the world's) and **theirs** (the parent's, now). A row both sides changed is a conflict, and a merge with conflicts merges nothing.

```sql
merge world agent_b dry run;
```

Abbreviated (`table`, `id`, `outcome`, `detail`):

```
 products | 3 | conflict | here: name; the parent: price (different columns: merge by columns combines them)
 products | 4 | apply    |
 products | 5 | conflict | both changed price
```

A plain `merge world agent_b` would fail with SQLSTATE 40001 and the same rows in its message ("2 rows were changed on both sides, so nothing was merged: ..."). The script doesn't run it, so it stays green. To settle it:

- **`BY COLUMNS`** combines rows where the two sides changed different columns: the grinder gets the new name and keeps `main`'s new price.
- **`RESOLVE ('products/5' = theirs)`** keeps `main`'s price for the descaler. A row can also be `ours`, `delete`, or a row you give as JSON.
- **`USING OURS`** or **`USING THEIRS`** settles whatever is left in one go.

Try the settlement as a dry run, then merge:

```sql
merge world agent_b by columns resolve ('products/5' = theirs) dry run;
-- 3: by columns, 4: apply, 5: picked theirs
merge world agent_b by columns resolve ('products/5' = theirs);   -- MERGE 2
```

## 6. Drop the rest

```sql
drop world agent_c;
show worlds;                            -- just main
select * from products order by sku;
```

`main` now holds the pricer's prices, the copywriter's names, and every product:

```
 sku | name                      | price | stock
   1 | Espresso beans 1kg        | 18.00 |    40
   2 | Filter papers (100)       |  3.15 |   900
   3 | Burr grinder, 40 settings | 80.10 |   150
   4 | Milk jug 600 ml           | 14.00 |    12
   5 | Descaler                  |  6.30 |   300
```

## The guest: clients with the server token can't write main

On a shared server you don't want to trust every client. Give it a server token, which lets clients in as the guest, and an admin token for the person who approves:

```bash
CHRONOS_TOKEN="$SERVER_TOKEN" CHRONOS_ADMIN_TOKEN="$ADMIN_TOKEN" chronos serve shopdb
```

- **Clients with the server token act as the agent `guest`** (since 0.4; with `--safe`, every client that isn't an agent or the admin token, loopback without a token too). The guest reads anything, forks, and changes only worlds it forked. It can't write `main`, merge, restore, drop others' worlds, make agents or change settings (SQLSTATE 42501).
- **The person approving** connects with the admin token as the password and runs the merges: `psql "postgres://admin:$ADMIN_TOKEN@127.0.0.1:5433/main"`.
- **Agents with their own names and rights** (`CREATE AGENT`, then user = its name, password = its token) keep their own rights. See [concepts](../concepts.md#agents). One allowed to merge into `main` should keep to a [merge policy](merge-policies.md#start-here-no-delete-merges-on-its-own), starting with `max_deletes = 0`.

Safe mode needs its own server start, so the script doesn't cover it.

## The same from Claude Code (MCP)

```bash
claude mcp add chronos -- chronos mcp /path/to/shopdb
```

If `chronos serve` already has the folder open, `chronos mcp` joins it rather than failing. By default MCP runs in safe mode: the agent gets `describe`, `find`, `sql`, `get`, `put`, `delete`, `fork`, `set_meta`, `branches`, `diff`, `history`, `merge_preview`, `checkpoint`, `rollback`, `discard`, `simulate`, `replay` and the merge-policy tools (it drafts policies; you apply them), and changes only the worlds it forked in that session. `merge`, `restore` and `undo_merge` aren't listed, and world statements in its `sql` calls (fork, merge, switch, drop) are refused: it uses the `fork` tool instead.

The loop is the same as above: `describe`, `fork` (with `meta`), `put` or `sql` with `branch` set to its world, `diff`, `merge_preview`. Then it tells you the world's name, and you merge it from a terminal (`chronos /path/to/shopdb merge agent_a`) or from psql. `--allow-merge` gives it the merge tools, and `--agent NAME` makes it act as an agent you created, with that agent's rights.

## What just happened

- Three agents changed the same table at once, in three worlds, and `main` never saw a half-finished change.
- `DIFF` showed each agent's work row by row; `DRY RUN` showed what a merge would do before it did anything.
- The conflict was reported as data (which rows, and why), and settled row by row and by columns.
- Throwing an agent's work away was one `DROP WORLD`.

## Next steps

- [Concepts: merging](../concepts.md#merging) and [settling conflicts](../concepts.md#settling-conflicts).
- [SQL: worlds](../sql.md#worlds): partial merges (`ONLY TABLES`, `ONLY KEYS`), `MERGE ... INTO` another world, TTLs, pinning.
- [Agent evaluation](agent-evaluation.md): grade an agent's world against the state it should have left.
- [Scenario planning](scenario-planning.md): let `SIMULATE` fork and score a thousand worlds for you.
- [Merge policies](merge-policies.md): let agents merge on their own within rules, and review only what breaks them.
- [Security: agents in containers](../security.md#agents-in-containers): run each agent with only its token, away from the folder and your network.
