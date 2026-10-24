# Try it in five minutes: an agent on a small CRM

An agent with write access to a CRM can do real damage, and "make it read-only" isn't an answer
when its job is to write. Here an agent works on a small CRM in its own worlds (private copies of
the database), and you watch four ways its work goes wrong get caught before they reach `main`:

| | What the agent does | What stops it |
|---|---|---|
| 1 | Plans follow-ups from Ana's deals while one of them closes | The **stale-read check**: it read a row that changed, so the merge waits for you. A change to rows it never read (Ben's deals) doesn't hold it |
| 2 | Cuts Ben's prices, and one goes below zero | A **merge check**: a query run on the merged result finds the bad row |
| 3 | Deletes stale leads and queues an email about it | A **merge policy**: no deletes without a person. The email is only sent once you approve |
| 4 | Keeps writing while you review | Your approval names the **version** you read, so its later writes don't ride along |

And `main` is protected: nobody, you included, can `DELETE` there directly. Rows reach it only by
merging a world.

## Run it

```bash
curl -fsSL https://github.com/Abhishekxdg/chronosdb/releases/latest/download/install.sh | sh
CHRONOS_PROTECT_MAIN=off chronos serve crm-trial &   # the trial seeds main directly; HTTP on 127.0.0.1:7070, Postgres on 127.0.0.1:5433
git clone https://github.com/Abhishekxdg/chronosdb && cd chronosdb/examples/trial
python3 trial.py                        # Python 3.9+, nothing to install
```

It sets up the CRM from [`crm.sql`](crm.sql), creates the agent `crm_bot`, and walks through the
four cases, printing what happens:

```
== 0. main is protected: even you can't DELETE there directly
   refused: main is protected (protect_main): its rows change only by merging a world, and this
   changes leads/1; fork a world, make the change there, and merge it (or ALTER SYSTEM SET protect_main = off)

== 1. The agent reads Ana's open deals and plans follow-ups
   read 3 deals, wrote 3 tasks
   meanwhile, someone moves one of BEN's deals: the agent never read it
   merged: a change to rows the agent didn't read doesn't hold its work

== 2. The same again, but an ANA deal it read closes while it works
   refused: merging followups2 needs a person's review (merge policy careful: it read rows that
   changed since it forked: deals/3); it's queued (SHOW REVIEWS)

== 3. The agent cuts every proposal of Ben's by 1500, and one goes below zero
   refused: merging discounts needs a person's review (merge policy careful: it breaks merge check
   no_negative_amounts, which finds (id = 5, account = 'Hooli', amount = -500)); it's queued (SHOW REVIEWS)

== 4. The agent archives stale leads (a delete), and queues an email about it
   refused: merging cleanup needs a person's review (merge policy careful: it deletes 3 rows (at most 0)); ...

== 5. You review it, but the agent writes again before you approve
   refused: branch 'cleanup' is at version 3, not 2: it changed since (written to, or partly merged), ...
   your approval named the version you read, so the agent's later delete didn't ride along
   you read it again: 4 changes, version 3; you approve those
   merged; the email went out only now: outbox has 'archived 3 stale leads' (queued)
```

It checks each outcome too, so it ends with `trial: ok` or tells you which one didn't happen.
The script is the agent here; a real one makes the same calls through MCP, the HTTP API or SQL.

## Look around yourself

The database is still there. Connect with psql (the database name is the world):

```bash
psql postgres://127.0.0.1:5433/main
```

```sql
show reviews;                              -- what waits for you, and why
diff world followups2;                     -- what the agent changed, row by row
merge world discounts dry run;             -- what a merge would do, and what blocks it
show audit for agent crm_bot;              -- everything the agent did
show outbox;                               -- effects waiting for a worker
drop world discounts;                      -- reject: nothing it did reaches main
```

Then try your own agent: give it a token (`create agent ... with (policy = 'careful')`), point it at
`postgres://crm_bot:<token>@127.0.0.1:5433/main` or the HTTP API, and let it work in worlds it forks.
[Merge policies](../../docs/guides/merge-policies.md) and [merge checks](../../docs/guides/merge-checks.md)
say how to write your own rules.

## Tell us what broke

This is the part we need most. If your agents write to real data (CRM, orders, tickets), open an
issue at [github.com/Abhishekxdg/chronosdb](https://github.com/Abhishekxdg/chronosdb/issues) with
what you ran and what happened, or what you expected to be caught and wasn't.

To start over: stop the server and delete the `crm-trial` folder.
