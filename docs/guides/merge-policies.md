---
title: Let agents merge on their own, within rules
---

Reviewing every agent's diff works for ten changes a day. It breaks at ten thousand, especially when the data keeps changing under both the agents and the reviewer. A **merge policy** changes what a person approves: you approve the *rules* once, and review only the merges that break them.

- A merge that keeps to its agent's rules goes through on its own.
- A merge that breaks any rule is refused, with the reasons. Its world stays, queued in `SHOW REVIEWS`, until a person merges it or drops it.

You'll set up a shop, write a policy, give it to an agent, watch a small fix merge on its own while a bulk change and a refund wait for you, then approve one and reject the other. It all runs in `psql` in a few minutes. The whole script is [examples/guides/merge-policies.sql](https://github.com/Abhishekxdg/chronosdb/blob/main/examples/guides/merge-policies.sql).

## How it works

```
agent: MERGE WORLD w
   │
   ├─ its rights: may it merge w at all? (merge_own / merge, write_main for main) ── no ──> refused
   │
   └─ inside the merge, holding its locks, on the exact rows it's about to apply:
        does the agent keep to a merge policy?
          no  ───────────────────────────────────────────────> merged
          yes: every rule kept? ── yes ──────────────────────> merged
                                └─ no ──> refused (42501, the rules broken),
                                          the world stays, queued in SHOW REVIEWS
                                             │
                                  a person: DIFF, then MERGE WORLD w  (approve)
                                                   or DROP WORLD w    (reject)
```

Three things make this safe while data keeps changing:

- **What's checked is what merges.** The rules run inside the merge, while it holds its locks, on the rows it's about to apply. Nothing written a moment before or after can slip past the check, and there's no gap between "checked" and "merged" for a race to use.
- **Stale work doesn't win.** If `main` changed a row after the agent forked, a merge that would overwrite it (`USING OURS`, or rows picked by hand) needs a person, unless the policy allows `overwrite`. Without `USING OURS`, those rows are conflicts and the merge fails anyway.
- **The queue says when it's out of date.** `SHOW REVIEWS` records the world's version when the agent asked. `changed_since` is true if the agent kept writing afterwards, or part of the world was merged since, so you know the diff you're about to approve isn't the one that was refused.

A policy only ever takes rights away: an agent needs `merge_own` or `merge` to merge at all, and a policy decides which of those merges need a person. An agent keeping to a policy changes `main` **only by merging**: its `write_main` right lets its merges reach `main`, but a direct `INSERT`, `UPDATE`, `DELETE`, `put` or restore on `main` is refused, since it would skip the rules. So are `UNDO MERGE` and `UNDO AGENT`, which write their rows straight into a world. People, and agents without a policy, act as their rights alone decide.

## Setup

Start a server on a new folder and connect to it:

```bash
chronos serve shopdb                          # HTTP on 7070, Postgres protocol on 5433
psql postgres://127.0.0.1:5433/main
```

Or run the whole guide at once:

```bash
psql -v ON_ERROR_STOP=1 -f examples/guides/merge-policies.sql postgres://127.0.0.1:5433/main
```

Make the shop:

```sql
create table orders (id int primary key, customer text not null, status text not null, total numeric(10, 2));
create table payments (id int primary key, order_id int not null, amount numeric(10, 2) not null);
insert into orders values
  (1, 'ada', 'new', 20.00), (2, 'bo', 'new', 35.50), (3, 'cy', 'paid', 12.00),
  (4, 'di', 'paid', 99.00), (5, 'ed', 'new', 8.25), (6, 'flo', 'shipped', 41.00);
insert into payments values (1, 3, 12.00), (2, 4, 99.00);
```

## Start here: no delete merges on its own

Whatever else an agent's policy says, start it with `max_deletes = 0`: every merge that deletes a row waits for a person. A wrong update leaves a row you can still see and fix; a wrong delete leaves nothing to look at, and a bulk one is the change people most regret. Most agents rarely need to delete, so the queue stays short, and each delete that comes through shows you whether the agent should be trusted with more.

```sql
create merge policy agents with (max_deletes = 0);
alter agent bot set (policy = 'agents');
```

Loosen it only once the queue shows the deletes are routine, and then with a bound: `max_deletes = 50` for one merge, and `deletes_per_hour = 200` so the agent can't reach more by splitting the job across worlds.

## 1. Write the rules

Small changes to orders may merge on their own. Bulk changes, deletes and anything touching money wait for you:

```sql
create merge policy careful with (max_rows = 3, max_deletes = 0, review_tables = 'payments');
show merge policies;
```

```
  name   | max_rows | max_deletes | tables | review_tables | review_columns | schema | overwrite | critical | check_reads | require_scope | rows_per_hour | deletes_per_hour | max_age |          created
---------+----------+-------------+--------+---------------+----------------+--------+-----------+----------+-------------+---------------+---------------+------------------+---------+---------------------------
 careful |        3 |           0 |        | payments      |                | f      | f         | f        | f           | f             |               |                  |         | 2026-09-27 16:19:35.06+00
```

Every rule is optional. A rule left out doesn't limit anything, except `schema` and `overwrite`, which are `false` (the safe side) until you allow them:

| Rule | A merge needs a person when | Default |
|---|---|---|
| `max_rows` | it changes more rows than this | no limit |
| `max_deletes` | it deletes more rows than this; `0` means any delete | no limit |
| `rows_per_hour` | together with the agent's other merges into `main` in the last hour, it changes more rows than this | no limit |
| `deletes_per_hour` | together with the agent's other merges into `main` in the last hour, it deletes more rows than this | no limit |
| `max_age` | its world was forked longer ago than this (`'1h'`): the longer a world stays open, the more its merge rests on what it read long ago | no limit |
| `tables` | it changes a table not in this list (`'orders,items'`) | any table |
| `review_tables` | it changes any table in this list | none |
| `review_columns` | it changes any column in this list (`'users.email,accounts.owner'`), even in one row: identity, owner and billing fields, where a quiet wrong value does more harm than a loud delete | none |
| `schema` | `false` and it changes a table itself (`CREATE`/`ALTER`/`DROP TABLE`), a view, function, sequence, schema or type (or a registered reader, or a critical mark) | `false` |
| `overwrite` | `false` and it overwrites rows its parent changed since the fork | `false` |
| `critical` | `false` and it changes a column a reader marked critical reads (`MARK READER billing CRITICAL`) | `false` |
| `check_reads` | `true` and it read rows that changed in its parent since the fork (its agent decided on state that's gone) | `false` |
| `require_scope` | never itself: `true` makes its agents declare what each world may change when forking it (`may_change`, `tenant`); a merge outside that [declared scope](../reference/worlds.md#declared-scope-may_change-tenant) waits for a person | `false` |

`max_age` complements the [stale-read check](../reference/worlds.md#stale-reads-check_reads): `check_reads` holds a merge whose reads changed since the fork, `max_age` one whose reads may be fine but are old (`was forked 2 h 5 min ago (at most 1 h)`).

`max_rows` and `max_deletes` judge one merge, so an agent that splits a job across several worlds stays under them. `rows_per_hour` and `deletes_per_hour` add up each agent's merges into `main` over the last hour (a sliding window), this one included: `deletes_per_hour = 50` holds an agent to 50 deletes an hour however it splits them. Only an agent's own merges that land in `main` count: not merges between its worlds (they count when they reach `main`), not merges queued for review, and not a person's merge approving one. The counts are kept beside the log (the `tallies` file) and survive a restart.

Rows are counted as the merge would apply them: one per row it inserts, updates or deletes in the parent, after `ONLY TABLES` / `ONLY KEYS`. A row settled `USING THEIRS` (the parent's value kept) changes nothing, so it doesn't count. Table names are read as SQL reads them: `orders` is folded to lowercase, and `"Orders"`, quoted, is another table (`tables = 'orders,"Orders"'`). The database's own bookkeeping (indexes, constraint rows, `serial` counters) doesn't count. Rows combined `BY COLUMNS` never count as overwrites: they keep the parent's changed columns.

## 2. Give them to an agent

```sql
create agent bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'careful');
```

`merge_own` lets `bot` merge the worlds it forks, and `write_main` lets those merges reach `main`. The policy decides which of those merges need you. The `token` column is the agent's password, shown only now. The agent connects with it as its user:

```bash
psql "postgres://bot:<token>@127.0.0.1:5433/main"
```

(The script does this with `\gset` and `\connect`.)

## 3. As the agent: a small fix merges on its own

```sql
create world bot_fix with (task = 'mark paid orders shipped');
switch world bot_fix;
update orders set status = 'shipped' where status = 'paid';   -- 2 rows
switch world main;
merge world bot_fix;
```

```
MERGE 2
```

Two rows, no deletes, no payments: it keeps to every rule, so no person was involved.

## 4. As the agent: a bulk change and a refund wait

```sql
create world bot_bulk with (task = 'archive every order');
switch world bot_bulk;
update orders set status = 'archived';                        -- 6 rows
switch world main;
merge world bot_bulk dry run;
```

The dry run lists every row as usual, and a last `blocked` row says why the merge won't go through, before the agent tries:

```
 table  | id | outcome | detail
--------+----+---------+-------------------------------------------------------------------------
 orders | 1  | apply   |
 ...
        |    | blocked | merging bot_bulk needs a person's review (merge policy careful: it changes 6 rows (at most 3)); it's queued (SHOW REVIEWS)
```

```sql
merge world bot_bulk;
```

```
ERROR:  merging bot_bulk needs a person's review (merge policy careful: it changes 6 rows (at most 3)); it's queued (SHOW REVIEWS)
```

The error is SQLSTATE `42501` (over HTTP, `403`). An agent should stop there and tell its person the world's name. Nothing was merged, and `bot_bulk` is still there, untouched.

One row can also need a person, if it's in a table you always review:

```sql
create world bot_refund with (task = 'refund order 4');
switch world bot_refund;
insert into payments values (3, 4, -99.00);
switch world main;
merge world bot_refund;
```

```
ERROR:  merging bot_refund needs a person's review (merge policy careful: it changes payments (always reviewed)); it's queued (SHOW REVIEWS)
```

When a merge breaks several rules, the message lists them all: `it changes 40 rows (at most 3); it deletes 2 rows (at most 0)`.

## 5. As yourself: the queue

```sql
show reviews;
```

```
   world    | owner | policy  |              reasons               |           asked            | version | changed_since | scope
------------+-------+---------+------------------------------------+----------------------------+---------+---------------+-------
 bot_bulk   | bot   | careful | changes 6 rows (at most 3)         | 2026-09-27 16:19:35.137+00 |       1 | f             |
 bot_refund | bot   | careful | changes payments (always reviewed) | 2026-09-27 16:19:35.141+00 |       1 | f             |
```

Oldest first. Look at a merge as you would any world, then approve it by merging, or reject it by dropping:

```sql
diff world bot_refund;
merge world bot_refund;      -- approve: you have no policy, so your rights decide
drop world bot_bulk;         -- reject
show reviews;                -- (0 rows)
```

- **Approve what you read:** if `changed_since` is `t`, the agent wrote to the world after it asked, so read the diff again. Over HTTP, `merge` takes the `version` you read and merges only if the world is still at it (`409` otherwise).
- **Approve part of it:** `MERGE WORLD bot_bulk ONLY TABLES (orders)` or `ONLY KEYS ('orders/1', ...)` merges some rows and leaves the world open with the rest.
- **The agent can't skip the queue:** its queue entry is in the world's metadata under a key only the database sets. The agent can't remove it, and can't merge the world until a person does, or until it fits the rules.
- **Who did what:** `SHOW AUDIT FOR AGENT bot` lists the agent's forks, writes and merges. A refused merge shows as `metadata`: that's the world being queued.

## 6. Tune the rules

Rules change without touching the agents that keep to them:

```sql
alter merge policy careful set (max_rows = 100);       -- the named rules change; the rest stay
alter merge policy careful set (max_deletes = null);   -- null: no limit
alter agent bot set (policy = null);                   -- bot's rights alone decide now
drop merge policy careful;                             -- refused while an agent keeps to it
drop merge policy careful cascade;                     -- its agents keep to none from now on, in the same step
```

Only an `admin` creates, changes or drops policies, or gives one to an agent. Any agent may `SHOW MERGE POLICIES` and `SHOW REVIEWS`, so it can see its own rules.

## Recipes

Start strict, then loosen the rules as the queue shows you what the agent actually does. Every recipe but the data-cleaning job keeps `max_deletes = 0`.

| Agent | Policy | Why |
|---|---|---|
| Any new agent | `max_deletes = 0` | nothing it deletes merges without you; the default to start from |
| Support bot fixing typos in tickets | `tables = 'tickets', max_rows = 20, max_deletes = 0` | it can't reach anything else, or delete |
| Pricing agent | `tables = 'prices', max_rows = 200, max_deletes = 0, review_tables = 'prices_history'` | routine repricing flows; history rewrites wait |
| Data-cleaning job | `max_rows = 5000, max_deletes = 50, deletes_per_hour = 200` | deleting is its job, so bounded, not banned; a runaway `DELETE` waits |
| Agent near money | `max_deletes = 0, review_tables = 'payments,refunds,payouts'` | every change to money has a person on it |
| Migration assistant | `schema = true, max_deletes = 0, review_tables = 'payments'` | may change tables; deletes and money wait |
| Many agents, one table | leave `overwrite = false` | the agent that forked earlier can't clobber a newer write |

`max_rows = 0` means no row may change, so every merge that changes rows goes to a person.

## From Claude Code and other MCP clients

The same rules apply over MCP. With `chronos mcp shopdb --agent bot`, the agent's `merge` tool goes through or is refused exactly as in SQL. Five tools manage policies:

| Tool | Does |
|---|---|
| `merge_policies` | the policies, and which agents keep to each |
| `check_merge_policy` | tries rules on a branch's real changes: would it merge on its own, and if not, why |
| `set_merge_policy` | creates or changes a policy and gives it to agents (needs `admin`) |
| `drop_merge_policy` | removes one (`release_agents` takes it from its agents in the same step) |
| `reviews` | the queue |

Try rules on real work before any agent keeps to them. `rules` changes a named policy's rules for this one check, so you can ask "what if?" without changing anything:

```json
{"name": "check_merge_policy", "arguments": {"branch": "bot_bulk", "name": "careful", "rules": {"max_rows": 10}}}
```

```
bot_bulk's merge would go through on its own under merge policy careful with those changes (...)
```

Then set them:

```json
{"name": "set_merge_policy", "arguments": {"name": "careful", "rules": {"max_rows": 10, "review_tables": ["payments"]}, "agents": ["bot"]}}
```

In MCP's default [safe mode](../reference/mcp.md#safe-mode), an agent can *draft* rules but not apply them. `set_merge_policy` changes nothing and answers with the SQL for you to run:

```
safe mode: a person sets the rules agents' merges keep to. Show them the rules and why, and ask them to run this in `chronos <database folder>` or psql:
alter merge policy 'careful' set (max_rows = 10, review_tables = 'payments');
alter agent 'bot' set (policy = 'careful');
```

So a coding agent can study how your agents work, propose rules, test them with `check_merge_policy`, and hand you the SQL. You stay the one who approves the rules.

## Over HTTP

Policies are SQL, so over HTTP you use the `sql` operation.

```bash
curl -s localhost:7070/v1/sql -H 'Content-Type: application/json' \
  -d '{"sql": "create merge policy careful with (max_rows = 3, max_deletes = 0, review_tables = '\''payments'\'')"}'
curl -s localhost:7070/v1/alter_agent -H 'Content-Type: application/json' -d '{"name": "bot", "policy": "careful"}'
```
 Agents take a `policy` field in `create_agent` and `alter_agent` (a name, or `null` for none). An agent's refused `merge` gets `403` with the reasons, and a `dry_run` merge returns them in `blocked`.

## Limits, today

- **Counts, not content:** rules look at which tables change and how many rows, not at values. "Refunds under $50 may merge" isn't a rule yet. Put such tables in `review_tables`, or give the agent a separate world and a stricter policy.
- **One policy per agent.** Give agents that need different rules different policies.
- **A partial approval leaves the entry:** after `MERGE ... ONLY TABLES` of a queued world, it stays in `SHOW REVIEWS` with the rest of its changes, with `changed_since` true (its reasons were for the whole world). The world's version doesn't move, so a client's merge at the version it had still works. Drop the world, or merge the rest, to clear it.
- **Merges into other worlds:** a policy checks every merge its agent runs, into `main` or any world, but only `main` is closed to its direct writes. Other worlds it may write (its own, or any with `write`) it writes directly.
- **One queue entry per world:** a world refused again updates its entry (new reasons, version and time).

For rules about the data a merge leaves, not the change itself ("every lead list keeps a lead", "an order's total is its items' sum"), use [merge checks](merge-checks.md): an agent under a policy whose merge breaks one is queued here too.

## Next steps

- [SQL reference: merge policies](../reference/worlds.md#merge-policies), and [MCP: the policy tools](../reference/mcp.md#merge_policies).
- [Concepts: agents](../concepts.md#agents): rights, quotas, the audit trail, undoing an agent's changes.
- [Agent sandbox](agent-sandbox.md): worlds per agent, diffs, conflicts.
