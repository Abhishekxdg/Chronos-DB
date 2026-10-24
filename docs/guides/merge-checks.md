---
title: Rules every merge's result must keep
---

Two agents clean up a CRM. One removes a duplicate lead from the VIP list; the other removes a stale one from the same list. Each change is fine on its own. Together they leave the list empty, and no diff shows that: each diff is one harmless row. A **merge check** catches it, because it looks at what the merge would make, not at the change.

A merge check is a named SQL query that must find nothing in the merged result. If it finds rows, the merge is refused and the rows are the reason. Unique and foreign-key constraints are already checked again at every merge; merge checks do the same for your own rules.

You'll set up a CRM, write the rule "every lead list keeps at least one lead", watch two fine changes get caught together, then see how agents under a merge policy are queued for a person instead. It all runs in `psql`.

## How it works

```
MERGE WORLD w
   │
   └─ inside the merge, holding its locks, before anything is written:
        rows planned ─> the world merged into, as the merge would leave it
                          │
                          └─ each check for the tables it changes: SELECT ..., read only, within its timeout
                                finds nothing ───────────────────────────────> merged
                                finds rows, or doesn't finish in time:
                                  an agent keeping to a merge policy ─> refused (42501), queued in SHOW REVIEWS
                                  anyone else, a person included ─────> refused (23514) with the rows
```

- **What's checked is what merges.** The checks run inside the merge, under its locks, on the exact result. Nothing written a moment before or after slips between the check and the merge.
- **Partial merges check what they'd make.** `ONLY TABLES` and `ONLY KEYS` check the parent with only those rows merged; `INTO` checks the other world.
- **No one skips a check.** A person's merge is refused like an agent's. To make an exception, change the check (only an `admin` can).

## Setup

```bash
CHRONOS_PROTECT_MAIN=off chronos serve crmdb   # the setup below writes main directly
psql postgres://127.0.0.1:5433/main
```

```sql
create table lists (id int primary key, name text);
create table leads (id int primary key, list int, who text);
insert into lists values (1, 'vip'), (2, 'cold');
insert into leads values (1, 1, 'ada'), (2, 1, 'bo'), (3, 2, 'cy');
```

## 1. Write the rule

The query finds what's wrong: lists with no leads.

```sql
create merge check lists_keep_leads on tables (leads, lists) as
  select id from lists where id not in (select list from leads);
show merge checks;
```

```
       name       |   tables    | timeout_ms |                             query                              |          created
------------------+-------------+------------+----------------------------------------------------------------+---------------------------
 lists_keep_leads | leads,lists |       1000 | select id from lists where id not in ( select list from leads ) | 2026-09-28 10:00:00.12+00
```

`ON TABLES` says when it runs: only for merges that change `leads` or `lists` (their rows, or the tables themselves). Leave it out and it runs for every merge. The check is run once on `main` as you create it, so a misspelled table is refused now rather than at every merge.

## 2. Two fine changes, caught together

```sql
create world a; switch world a; delete from leads where id = 1; switch world main;
create world b; switch world b; delete from leads where id = 2; switch world main;
merge world a;          -- MERGE 1: list 1 still has lead 2
merge world b dry run;
```

The dry run lists the rows as usual, and a last `blocked` row names the check and what it found:

```
 table | id | outcome | detail
-------+----+---------+------------------------------------------------------------------------------
 leads | 2  | apply   |
       |    | blocked | merging b breaks merge check lists_keep_leads, which finds (id = 1); nothing was merged
```

```sql
merge world b;
```

```
ERROR:  merging b breaks merge check lists_keep_leads, which finds (id = 1); nothing was merged
```

The error is SQLSTATE `23514` (over HTTP, `409`). Nothing was merged, and `b` is still there. A reason shows the first 10 rows a check finds, then `, and more`.

## 3. Agents under a merge policy: queued, not refused

For an agent keeping to a [merge policy](merge-policies.md), a broken check is one more broken rule: the merge waits for a person.

```sql
create merge policy careful;
create agent bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'careful');
```

As `bot`:

```sql
create world wipe; switch world wipe; delete from leads where list = 2; switch world main;
merge world wipe;
```

```
ERROR:  merging wipe needs a person's review (merge policy careful: it breaks merge check lists_keep_leads, which finds (id = 2)); it's queued (SHOW REVIEWS)
```

As yourself, `show reviews` lists `wipe` with that reason. The check applies to you too, so `merge world wipe` is refused until the world keeps the rule. Fix it and merge, or drop it:

```sql
switch world wipe; insert into leads values (9, 2, 'new'); switch world main;
merge world wipe;             -- list 2 has a lead again: merged, and off the queue
```

An agent without a policy is refused like a person (23514), and nothing is queued.

## 4. What a check may be, and what it costs

- **One `SELECT` that changes nothing.** No `nextval`, `setval` or `pg_notify`, and no function made with `CREATE FUNCTION`, in it or in a view it reads (25006). It runs read only, as the database's own.
- **A timeout.** Default 1 second, set with `with (timeout = '2s')`. The merge waits for its checks holding its locks, so a check that doesn't finish in time refuses the merge (`it didn't finish within 1000 ms`). Keep checks to what indexes answer quickly, and scope them with `ON TABLES`.
- **Change or drop:** `create or replace merge check ...` swaps one in a single step; `drop merge check lists_keep_leads` removes it. Only `admin` does either; any agent can `show merge checks`.

## Recipes

| Rule | Check |
|---|---|
| Every list keeps a lead | `select id from lists where id not in (select list from leads)` |
| An order's total is its items' sum | `select o.id from orders o left join (select order_id, sum(price * qty) s from items group by order_id) i on i.order_id = o.id where o.total <> coalesce(i.s, 0)` |
| Nothing oversold | `select p.id from products p join (select product, sum(qty) n from reservations group by product) r on r.product = p.id where r.n > p.stock` |
| At most 3 admins | `select role from users where role = 'admin' group by role having count(*) > 3` |
| Every open ticket has an owner | `select id from tickets where status = 'open' and owner is null` |

Per-row rules (`total >= 0`) are `CHECK` constraints; references are foreign keys. Merge checks are for rules across rows and tables, which only the whole result shows.

## Over HTTP and MCP

Checks are SQL, so over HTTP use the `sql` operation. A `merge` whose result breaks a check gets `409` with the reason; a `dry_run` merge returns it in `blocked`. Over MCP, `merge_checks` lists them, and `set_merge_check` and `drop_merge_check` (for `admin`) change them; in [safe mode](../reference/mcp.md#safe-mode) those two answer with the SQL for a person to run:

```json
{"name": "set_merge_check", "arguments": {"name": "lists_keep_leads", "query": "select id from lists where id not in (select list from leads)", "tables": ["leads", "lists"]}}
```

## Limits, today

- **The result, not the change.** A check sees the world as the merge would leave it, not what it was before, so "a deal's stage never goes backwards" needs the old stage kept in a row the check can read.
- **Merges only.** Direct writes and a transaction's `COMMIT` don't run checks. An agent keeping to a merge policy changes `main` only by merging, so every change it makes to `main` is checked.
- **Every world.** Checks live on `main` and run for merges into any world.
- **The default search_path.** A check's query reads names along `public` only, so a table in another schema is named in full (`app.orders`), in the query and in `ON TABLES`.
- **The rows found are in the error**, for whoever merged, even if the check reads tables that agent otherwise wouldn't look at. Write checks to return keys, not secrets.

## Next steps

- [SQL reference: merge checks](../reference/worlds.md#merge-checks), and [MCP: the check tools](../reference/mcp.md#merge_checks).
- [Merge policies](merge-policies.md): rules about the change itself (how many rows, which tables), under which agents merge on their own.
