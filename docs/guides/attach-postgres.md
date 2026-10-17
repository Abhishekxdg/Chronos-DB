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
3. Otherwise the updates, inserts and deletes are applied with `RETURNING`, and committed. Values a Postgres trigger
   or default set come back into main.

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
- **ranges, filtered scans (`where email = 'x'`), whole-table reads, lookups through an index, and keys that found
  no row:** the table is read whole from Postgres (at most 100,000 rows; past that the merge is held for a
  person), so a row added there that the read would have found counts, an empty result included.

Postgres's rows are put in the world's fork point, with the index entries they make, and checked as a read against
main is. A row the read couldn't have found (another email) doesn't hold the merge. A read that changed holds it as
any stale read: refused with `40001` and nothing written (`read rows that changed in Postgres since it forked:
suppressed/(12)`), or queued for a person under the agent's merge policy. A dry run reports the same.

The check reads without a predicate lock (`READ COMMITTED`): a row Postgres takes between the check and `COMMIT`
lands after the merge in time, an order in which the world's decision was right.

Main's rows change only this way: writes to main, `RESTORE` and `UNDO` on main are refused, and no statement may
change a table's shape (do that in Postgres, then attach again into a new folder).

An agent's merge into main needs a person unless the agent has a [merge policy](merge-policies.md) of its own; a
person approves by merging the queued world.

## Limits in Stage 1

- Each table needs a primary key, and no generated column or row-level security. Partitioned and inheriting tables
  aren't attached.
- Serial and identity columns take their new values from Postgres's own sequence, reserved 1,000 at a time, so an
  id a world gives a row is never given to another by Postgres, and is the id written at merge. The cost is gaps in
  the sequence. An id typed by hand into such a column is refused at merge. The role needs USAGE on the sequence.
- At most 10,000 changed rows per merge (`MERGE WORLD w ONLY TABLES (...)` merges part of one).
- Main is as old as the attach for rows no merge touched: a merge only compares the rows it writes. To pick up
  Postgres's other changes, attach again into a new folder (no refresh yet).
- A world that reads a table more broadly than 100,000 rows (a range, a filtered scan or a whole-table read of a
  bigger table) with `check_reads` on can't have that read checked against Postgres: its merge is held for a
  person, saying so.
- Triggers stay in Postgres and run as the merge is written. Rows they change in other tables show up as conflicts
  later, not in main right away.
- Tested on Neon, both straight to the database and through its pooler (PgBouncer, transaction mode): a merge of
  1,000 rows took about 1.2 s from a machine 60 ms away, mostly round trips ([BENCHMARKS.md §14](https://github.com/Abhishekxdg/chronosdb/blob/main/BENCHMARKS.md#14-attach-writing-merges-back-to-a-managed-postgres-neon)).

## After a crash

If Chronos stops during a write-back, the outcome is settled the next time `chronos serve` starts or a merge into
main runs: Chronos asks Postgres whether that transaction committed, then makes the merge here or drops it. Until it
knows, merges into main wait; everything else works.
