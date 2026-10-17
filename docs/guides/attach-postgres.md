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

Main's rows change only this way: writes to main, `RESTORE` and `UNDO` on main are refused, and no statement may
change a table's shape (do that in Postgres, then attach again into a new folder).

An agent's merge into main needs a person unless the agent has a [merge policy](merge-policies.md) of its own; a
person approves by merging the queued world.

## Limits in Stage 1

- Each table needs a primary key, and no generated column or row-level security. Partitioned and inheriting tables
  aren't attached.
- Inserts into a table with a serial or identity column are refused at merge (updates and deletes work). Tables
  keyed by a uuid the world supplies take inserts.
- At most 10,000 changed rows per merge (`MERGE WORLD w ONLY TABLES (...)` merges part of one).
- Main is as old as the attach for rows no merge touched: a merge only compares the rows it writes. To pick up
  Postgres's other changes, attach again into a new folder (no refresh yet).
- Triggers stay in Postgres and run as the merge is written. Rows they change in other tables show up as conflicts
  later, not in main right away.
- Tested on Neon, both straight to the database and through its pooler (PgBouncer, transaction mode): a merge of
  1,000 rows took about 1.2 s from a machine 60 ms away, mostly round trips ([BENCHMARKS.md §14](https://github.com/Abhishekxdg/chronosdb/blob/main/BENCHMARKS.md#14-attach-writing-merges-back-to-a-managed-postgres-neon)).

## After a crash

If Chronos stops during a write-back, the outcome is settled the next time `chronos serve` starts or a merge into
main runs: Chronos asks Postgres whether that transaction committed, then makes the merge here or drops it. Until it
knows, merges into main wait; everything else works.
