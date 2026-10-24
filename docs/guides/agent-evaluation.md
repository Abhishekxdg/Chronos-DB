---
title: Grade an agent by diffing its world against the expected state
---

You'll write an answer key once, as a world holding the state a correct run leaves behind. Each agent run gets a world of its own, and `DIFF` between the two worlds is the grade: every row it returns is a mistake, with the exact columns that are wrong. No output parsing, no string matching of SQL: two different ways of doing the task right both score 0.

The whole script is [examples/guides/agent-evaluation.sql](https://github.com/Abhishekxdg/chronosdb/blob/main/examples/guides/agent-evaluation.sql).

## Setup

```bash
CHRONOS_PROTECT_MAIN=off chronos serve evaldb   # the setup below writes main directly
psql -v ON_ERROR_STOP=1 -f examples/guides/agent-evaluation.sql postgres://127.0.0.1:5433/main
```

Start the server without `--token`: step 6 logs in as an agent and back, and on loopback without a token psql needs no password for the database's own user.

The task's starting state goes in `main`:

```sql
create table tickets (
  id       int primary key,
  title    text not null,
  status   text not null,
  priority text not null,
  assignee text
);
insert into tickets values
  (1, 'Checkout button does nothing', 'open', 'high', null),
  (2, 'Checkout button broken on Safari', 'open', 'high', null),
  (3, 'Typo on pricing page', 'open', 'low', null),
  (4, 'Export to CSV times out', 'open', 'high', 'linus'),
  (5, 'Dark mode colors', 'open', 'low', null);
```

The task given to the agent: *close ticket 2 as a duplicate of 1, assign every open high-priority ticket to ada, and add ticket 6, "Outage postmortem", high priority, assigned to ada.*

## 1. Write the answer key as a world

```sql
create world expected with (task = 'triage-42', role = 'answer key');
switch world expected;
update tickets set status = 'closed' where id = 2;
update tickets set assignee = 'ada' where status = 'open' and priority = 'high';
insert into tickets values (6, 'Outage postmortem', 'open', 'high', 'ada');
switch world main;
```

Keep `expected` for as long as the task is in your eval set; every run is graded against it.

## 2. Give each run its own world

```sql
create world run_1 with (task = 'triage-42', model = 'model-a');
create world run_2 with (task = 'triage-42', model = 'model-b');
checkpoint world run_2 as 'start';   -- a named moment to go back to (step 5)
```

Fork `expected` and the runs from the same moment of `main`. If `main` changes in between, that change shows up in every grade.

## 3. Run the agents

Point each agent at its own world: `postgres://127.0.0.1:5433/run_1`. Here, SQL stands in for two agents. The first assigns before it closes, so the duplicate ends up assigned too. The second closes the wrong ticket, skips the one already assigned to linus, and forgets ticket 6.

```sql
switch world run_1;
update tickets set assignee = 'ada' where status = 'open' and priority = 'high';
update tickets set status = 'closed' where id = 2;
insert into tickets values (6, 'Outage postmortem', 'open', 'high', 'ada');

switch world run_2;
update tickets set status = 'closed' where id = 1;
update tickets set assignee = 'ada' where status = 'open' and priority = 'high' and assignee is null;
switch world main;
```

## 4. Grade: diff the run against the answer key

`DIFF WORLD a TO b` works between any two worlds (or moments, `a@-1 hour`) and returns what it takes to turn `a` into `b`. So with `a` the answer key and `b` the run, each row is one place the run differs:

- `update`: the row is in both, and `columns` lists the fields the run got wrong. `before` is the expected row, `after` the run's.
- `insert`: the run has a row the answer key doesn't.
- `delete`: the answer key has a row the run is missing.

```sql
diff world expected to run_1;
diff world expected to run_2;
```

Abbreviated (`table`, `id`, `change`, `columns`; `before` and `after` left out):

```
-- run_1: 1 row
 tickets | 2 | update | ["assignee"]
-- run_2: 4 rows
 tickets | 1 | update | ["assignee", "status"]
 tickets | 2 | update | ["assignee", "status"]
 tickets | 4 | update | ["assignee"]
 tickets | 6 | delete | []
```

**The score is the row count:** 0 is a pass, and fewer is better. psql prints it under the rows (`(1 row)`, `(4 rows)`); from a driver it's the length of the result. `DIFF` is a statement of its own, so SQL can't wrap it in a `count(*)`.

Diff the other way round with `AS SQL` and you get the fix: the statements that turn the run into the answer key.

```sql
diff world run_2 to expected as sql;   -- one column, sql: update ... where id = 1; ... insert into tickets ...;
```

Two things to keep out of the grade:

- **Numbers the database hands out:** `serial` and sequence counters belong to the database, not to a world, so two worlds inserting "the next id" get different ids. Give new rows explicit keys in the task, as ticket 6 does here, or compare on a natural key.
- **The clock:** a `default now()` column differs between any two runs. Leave such columns out of the tables you grade, or give them fixed values.

`DIFF` also compares rows as stored: after a schema change (say `ADD COLUMN ... DEFAULT`), a row can show as an `update` whose `columns` list is empty. Skip those when you count.

## 5. Reset a run and go again

A checkpoint names a moment of a world, and restoring to it writes the rows back as they were:

```sql
restore world run_2 to checkpoint 'start';
diff world run_2;   -- no changes since the fork: ready for another attempt
```

`RESTORE WORLD run_2 TO '-5 minutes'` works too (any moment in the history window: 1 hour by default for a new database, `alter system set history_retention` keeps more). Or drop the world and fork a new one: forking costs the same whatever the database's size.

## 6. A real agent, and undoing it

An agent made with `CREATE AGENT` logs in as itself (user = its name, password = its token). Its world is its own (`owner` in `SHOW WORLDS`), everything it writes is in the audit trail, and by default it can't write `main` or merge. The script creates one, captures its token with psql's `\gset`, and reconnects as it:

```sql
create agent evalbot \gset agent_
\set agent_uri 'postgres://evalbot:' :agent_token '@' :HOST ':' :PORT '/main'
\connect :agent_uri
create world run_3 with (task = 'triage-42', model = 'model-c');
switch world run_3;
update tickets set status = 'closed' where id in (2, 3);   -- 3 is a mistake
update tickets set assignee = 'ada' where status = 'open' and priority = 'high';
\connect :admin_uri
```

Back as the database's own user, grade it, see what it did, then put back everything it changed in that world:

```sql
diff world expected to run_3;          -- 2 rows: ticket 3's status, and ticket 6 missing
show audit for agent evalbot;          -- at, agent, world, action, rows
switch world run_3;
undo agent evalbot since '-10 minutes';
switch world main;
diff world run_3;                      -- nothing left of the agent's changes
```

`UNDO AGENT` works row by row in the current world, for as far back as history goes. Rows someone else changed after the agent are named and nothing changes, or with `SKIP CHANGED` they're left as they are.

## Many evals in parallel

One world per run is the whole design:

- **Fork as many as you need.** Worlds are O(1) to fork and store only what they change. 100,000 worlds with a row each take 24 MB.
- **Runs don't wait for each other.** Each world has its own writer, and readers read a snapshot. Give each agent its own connection string (`.../run_17`) and start them all at once.
- **Clean up by themselves:** `ALTER WORLD run_17 SET TTL '1 day'`, an agent's `world_ttl`, or `ALTER SYSTEM SET world_idle_ttl = '7 days'`. `ALTER SYSTEM SET max_worlds = n` caps the total.
- **Deterministic policies instead of LLM agents?** `SIMULATE` forks, runs and scores a thousand worlds in one statement. See [scenario planning](scenario-planning.md).

## What just happened

- The answer key and every run were worlds forked from one `main`, so the only differences between them are what each run did.
- `DIFF WORLD expected TO run` gave the grade as rows: which rows, which columns, and whether a row was extra or missing.
- `DIFF ... AS SQL` gave the fix, a checkpoint reset a run, and `UNDO AGENT` took back an agent's work in its world.

## Next steps

- [SQL: worlds](../sql.md#worlds) and [time travel](../sql.md#time-travel) for every statement used here.
- [Concepts: agents](../concepts.md#agents): rights, quotas, tokens and the audit trail.
- [Agent sandbox](agent-sandbox.md): merge the best run instead of just grading it.
