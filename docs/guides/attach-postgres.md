# Working beside your Postgres (`chronos attach`)

Your data stays in the Postgres you already run. Chronos copies the tables an agent works on, the agent does its
whole task in a world, a person sees the row-level diff, and merging the world writes the change to Postgres in one
transaction. A row that changed in Postgres while the agent worked comes back as a conflict, and nothing is written.

**Status: Stage 1, for staging copies.** Read the limits below before pointing it at production.

## Attach

```bash
export CHRONOS_SOURCE_URL='postgres://agent_writer:...@staging-db:5432/crm?sslmode=verify-full'
chronos attach ./crm --tables public.leads,public.accounts
chronos serve ./crm
```

- The URL comes from `CHRONOS_SOURCE_URL`, or from a file only you can read (`chronos serve --source-file f`,
  or `CHRONOS_SOURCE_FILE`). It is never a command argument and is written nowhere in the folder.
- `--schema app` attaches every ordinary table of a schema. Above about 1,000,000 rows attach asks for `--yes`:
  it holds one read open on Postgres while it copies.
- Attach makes one table in Postgres, `chronos.merges` (a row per merge written, and one naming this folder). If
  the role can't, attach prints the statements for an administrator.

## Work

```sql
create world fix_leads;
use world fix_leads;           -- the agent's whole task, in its world
update leads set owner = 'ada' where region = 'EU';
delete from leads where email is null;
diff world fix_leads;          -- what it changed, row by row
merge world fix_leads;         -- written to Postgres first, then here
```

What happens at `merge world`:

1. Chronos's own checks, as for any merge: conflicts with main, constraints, merge policies, merge checks, stale reads.
2. One Postgres transaction: each row the world changed is locked and compared with the value main holds. Any
   difference means it changed in Postgres since: everything is rolled back, main takes Postgres's current values
   for those rows, and the merge returns the conflicts. Merge again and settle them as any conflict.
3. Otherwise the updates, inserts and deletes are applied with `RETURNING`. Then, in the same transaction, the
   [merge checks](#merge-checks-against-live-postgres) on attached tables run on Postgres as those writes leave it;
   rows found roll everything back. Otherwise it commits. Values a Postgres trigger or default set come back into
   main.

### Dry run against live Postgres

`merge world fix_leads dry run` shows, row by row, what the merge would apply, conflict on or be held for. In an
attached folder it then checks the rows it would write against Postgres as it is now:

- one read-only transaction (`REPEATABLE READ`, the same settings and source check as a write-back, and the same
  check that each table still has the shape it was attached with), compared as the write-back compares, then
  `ROLLBACK`. Nothing is written to Postgres, and main doesn't take Postgres's rows (a real merge's conflict does);
- each row gets a `postgres` column, Postgres's row now. A row changed there since main's copy has outcome
  `changed in postgres`, and the plan's `blocked` row says the merge would be refused;
- a last row, outcome `postgres`, says when Postgres was read and lists the tables' triggers;
- the rows are read with no locks, so what it shows is a snapshot. The merge itself locks and compares again.
  Triggers aren't run, so values they would set show up only after the real merge (main takes them then);
- if Postgres can't be reached (or no `CHRONOS_SOURCE_URL` is set), the plan still comes back, and the `postgres`
  row says it wasn't checked.

The HTTP `merge` op with `dry_run: true` returns the same as `postgres: {source, at, error, triggers, summary,
rows: [{key, changed, postgres}]}`, and MCP's `merge_preview` shows the column.

### Reads checked against live Postgres

A world forked `with (check_reads = true)` (or by an agent whose merge policy has `check_reads`) has its reads
checked against Postgres too, inside the write-back's transaction and before anything is written. Within Chronos a
read is checked against main, Chronos's copy, which a row changed straight in Postgres never reaches; so the
write-back also reads, for each attached table the world read:

- **rows read by key:** compared with Postgres's as the write-back compares the rows it writes;
- **filtered scans (`where org_id = 42 and status = 'open'`), with or without an index:** Postgres is asked only
  for the rows the conditions can match, now and at the fork, however big the table. Conditions go to Postgres as
  they compare here: whole numbers on integer and numeric columns, text on text and varchar compared byte by byte,
  booleans, `IS NULL`. Other conditions are left out of what's asked, so more rows are compared, never fewer;
- **ranges, whole-table reads, lookups through an expression index, keys that found no row, and filtered scans
  with no condition Postgres can be asked by:** the table is read whole from Postgres (at most 100,000 rows; past
  that the merge is held for a person),

so a row added there that the read would have found counts, an empty result included.

Postgres's rows are put in the world's fork point, with the index entries they make, and checked as a read against
main is. A row the read couldn't have found (another email) doesn't hold the merge. A read that changed holds it as
any stale read: refused with `40001` and nothing written (`read rows that changed in Postgres since it forked:
suppressed/(12)`), or queued for a person under the agent's merge policy. A dry run reports the same.

The check reads without a predicate lock (`READ COMMITTED`): a row Postgres takes between the check and `COMMIT`
lands after the merge in time, an order in which the world's decision was right.

**An empty read isn't checked at all without `check_reads`.** An agent that decides "create it if it's missing" (no
lead for this email, so make one) relies on a read that found nothing. Without `check_reads` nothing at merge
looks at that read again, so a matching row added in Postgres meanwhile isn't seen, and the agent's row is written
beside it (Postgres refuses only an exact duplicate of a unique key). Turn on `check_reads` for such agents (a
merge policy with `check_reads`, plus `require_scope` if they must also declare what they change), or add a merge
check (below) that finds the duplicates.

### Merge checks against live Postgres

A [merge check](../reference/worlds.md#merge-checks) runs first on main, Chronos's copy, as for any merge. A row added straight in
Postgres since the attach isn't there, so a check such as "no two leads with the same `lower(email)`" passes on main
and the duplicate is written, when no unique index in Postgres catches it (case-insensitive and fuzzy matches seldom
have one). So a merge check whose `ON TABLES` are all attached tables also runs in Postgres, inside the write-back,
after the merge's rows are applied and before `COMMIT`:

```sql
create merge check one_lead_per_email on tables (leads) as
  select lower(email) as email, count(*) as n from leads group by 1 having count(*) > 1;
```

- Rows found: `ROLLBACK`, nothing written, and the merge is refused with `23514` (`breaks merge check
  one_lead_per_email in Postgres, which finds {"email":"ada@x.com","n":2}`), or queued in `SHOW REVIEWS` under the
  agent's merge policy. The world and its queued effects (`NOTIFY ON MERGE`) stay in the world.
- The check's SQL runs in Postgres as written, under the write-back's settings (UTC, ISO dates, `search_path =
  public`) with its timeout as `statement_timeout`; a check that fails or times out there holds the merge too.
- A check with no `ON TABLES` may read any table, Chronos-only ones too, so it runs only on main. Give it `ON TABLES`
  naming the attached tables it reads to have it run in Postgres.
- `MERGE ... DRY RUN` runs merge checks on main only: its Postgres transaction is read only, so the merge's rows
  aren't there to check.

Main's rows change only this way: writes to main, `RESTORE` and `UNDO` on main are refused, and no statement may
change a table's shape here. Change it in Postgres: a refresh takes added columns (below); any other change needs a
new attach into a new folder.

An agent's merge into main needs a person unless the agent has a [merge policy](merge-policies.md) of its own; a
person approves by merging the queued world.

## Limits in Stage 1

- Postgres 12 or later: attach reads catalog columns 12 added (`attgenerated`, `relispartition`). Tested on 16 and
  17 (and Neon's 17); 12 to 15 and 18 aren't tested yet. On 11 or older the check and attach fail with Postgres's own
  error ("column attgenerated does not exist") rather than saying the version is too old; a clear refusal and a run
  across versions come after the 10 Nov launch.
- Each table needs a primary key, and no generated column or row-level security. Partitioned and inheriting tables
  aren't attached.
- Serial and identity columns take their new values from Postgres's own sequence, reserved 1,000 at a time, so an
  id a world gives a row is never given to another by Postgres, and is the id written at merge. The cost is gaps in
  the sequence. An id typed by hand into such a column is refused at merge. The role needs USAGE on the sequence.
- At most 50,000 changed rows per merge by default (`CHRONOS_ATTACH_MAX_ROWS` raises it, to 1,000,000), sent 5,000
  at a time in the one transaction; each stays locked until it commits. `MERGE WORLD w ONLY TABLES (...)` merges
  part of one.
- Rows no merge touched are as old as the last refresh (below). A refresh reads every attached table whole: on big
  tables, run it from a read replica's URL and not too often.
- A world that reads a table more broadly than 100,000 rows (a range or a whole-table read of a bigger table, or
  a filtered scan more than 100,000 of its rows can match) with `check_reads` on can't have that read checked
  against Postgres: its merge is held for a person, saying so.
- Triggers stay in Postgres and run as the merge is written. Rows they (or a rule, or a foreign key's `ON DELETE` /
  `ON UPDATE` action) write or delete in other attached tables come into main with the merge: the write-back counts
  what its transaction wrote per table (`pg_stat_xact_user_tables`), and the tables written besides the merge's own
  rows are refreshed alone right after it (read whole, as a refresh reads them). Deferred constraint triggers run
  before the counts are read (`SET CONSTRAINTS ALL IMMEDIATE`), so a deferred check that fails refuses the merge
  there rather than at COMMIT. Until that refresh is done the pending record stays, so after a crash recovery does
  it. With `track_counts` off, they wait for the next refresh.
- Tested on Neon, both straight to the database and through its pooler (PgBouncer, transaction mode): a merge of
  1,000 rows took about 1.2 s from a machine 60 ms away, mostly round trips ([BENCHMARKS.md §14](https://github.com/Abhishekxdg/chronosdb/blob/main/BENCHMARKS.md#14-attach-writing-merges-back-to-a-managed-postgres-neon)).
- The folder keeps one connection to Postgres between merges (refreshes and dry runs use it too), so a merge doesn't
  connect, handshake TLS and log in each time. It closes after 5 minutes unused, so Neon or Supabase can scale to
  zero; one closed by the server is replaced at the next merge's first statement, before anything is written. A
  changed `CHRONOS_SOURCE_URL` or `CHRONOS_SOURCE_FILE` is used from the next merge on.

## Keeping main up to date (refresh)

```bash
chronos refresh ./crm                      # once, on a folder no server has open
chronos serve ./crm --refresh-every 5m     # on a timer, while it serves
curl -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' localhost:7070/v1/refresh -d '{}'
```

A refresh reads every attached table in one `REPEATABLE READ, READ ONLY` transaction, under the write-back lock (no
merge into main runs meanwhile). Each of main's rows is compared with Postgres's as the write-back compares them, and
rows Postgres has that main hasn't are read whole; main takes the differences in one change. Open worlds see main
move: a row a world changed that Postgres changed too is a conflict at its merge, settled as any.

- **Columns added in Postgres** (`ALTER TABLE ... ADD COLUMN`, as a migration does) are added to main's table first,
  with Postgres's type and default; main's rows then take their values in the same refresh, and the column is made
  `NOT NULL` if Postgres has it. A default worked out per row (`gen_random_uuid()`, `random()`), which Chronos won't
  fill rows it has with, is set once they have Postgres's values. The refresh lists the columns (`added`). One
  stopped halfway is redone at the next. Until a refresh has taken them, a merge writing to the table is refused and
  says to refresh. A column added with a sequence or identity default, or a generated one, isn't taken.
- Worlds forked before a column was added keep the table as they had it. When one merges, a row it changed keeps the
  values Postgres holds in the columns it never had, settled `USING OURS` too: it can't have meant to change them.
  A row it inserts leaves those columns out, so Postgres fills them as it fills its own rows (`now()` is the
  insert's time), and main takes the row back as Postgres has it.
- A table whose columns were dropped, renamed or retyped, or whose key or row security changed, is left as it was
  and named (`skipped`); attach again into a new folder to take the new shape.
- `POST /v1/attached` says what the folder is attached to, its tables, and `as_of`: how old main's untouched rows
  may be (the last refresh, else the attach).

## Undoing a merge that reached Postgres

`UNDO MERGE OF WORLD w` (or HTTP `undo_merge`) on a merge into main puts the rows back through Postgres: they go
back in a world of main, which is merged as any world is, every row locked and compared there. A row changed in
Postgres since the merge is a conflict, and nothing is written.

## Agents held to one tenant

For an agent working for one of your customers, `create agent run_42 with (tenant = 'org_id = 42', valid_for =
'1 hour', ...)` (HTTP `create_agent` with `tenant` and `valid_for` in ms):

- in every world it reads and changes only rows of tables with an `org_id` column that hold 42, whatever its role,
  the table's own row security, or `row_security`; a row it writes must hold 42 before and after;
- every world it forks is declared that tenant's (`tenant` in its scope), so its merge is held to it too;
- it sees changes as rows (`DIFF`, a dry run, `CHECK READS`, `history`) only of the worlds it forked, and not `SHOW
  REVIEWS`, `SHOW AUDIT` or the whole `SHOW HISTORY`; the row API is refused on tables the tenant limits (SQL
  applies it);
- its token stops working once `valid_for` has passed.

## After a crash

If Chronos stops during a write-back, the outcome is settled the next time `chronos serve` starts or a merge into
main runs: Chronos asks Postgres whether that transaction committed, then makes the merge here or drops it. Until it
knows, merges into main wait; everything else works.
