---
title: What Chronos protects, and what it doesn't
---

Chronos is a database. It controls what agents do to the data inside it. It can't see, stop or undo anything an
agent does anywhere else. If agents work on production data, read this page before you rely on it.

## What it protects: data inside Chronos

- **Agents write in worlds, not in `main`.** An agent forks a world (a copy-on-write branch), writes there, and its
  work reaches `main` only by a merge. Agents without `write_main` can't write `main` at all
  ([who can do what](security.md#who-can-do-what)).
- **Merges go through rules.** A [merge policy](guides/merge-policies.md) lets an agent's merge through on its own
  only within limits you set (`max_rows`, `max_deletes = 0`, `tables`, `review_tables`, ...); everything else waits
  in `SHOW REVIEWS` for a person. [Merge checks](guides/merge-checks.md) are SQL rules every merge's result must
  keep. A world can also [declare its scope](reference/worlds.md#declared-scope-may_change-tenant) when it's
  forked (which tables and columns, which tenant, how many rows), and its merge is held to it.
- **Decisions on stale data are caught.** A world that [checks its reads](reference/worlds.md#stale-reads-check_reads)
  is held when something it read changed before its merge.
- **Any agent's changes can be undone.** `UNDO AGENT bot SINCE '-2 hours'`, `UNDO MERGE OF WORLD w` and
  `RESTORE WORLD main TO '-10 minutes'` work within the history window ([undoing](security.md#undoing)).
- **Agents held to one tenant see only its rows.** An agent made `WITH (tenant = 'org_id = 42')` reads and changes
  only rows of that tenant, in tables with the column and in tables tied to one by foreign keys; tables with neither
  are shared, read-only for it ([agents](sql.md#agents)).
- **Masks hide columns.** [Masked columns](sql.md#masked-columns) show agents a stand-in instead of the value.
  What they don't hide (a person's own copies, a unique column's insert) is [listed with them](sql.md#masked-columns).
- **Beside your Postgres:** in a folder [attached to Postgres](guides/attach-postgres.md), the same rules apply
  before a merge is written to Postgres, and nothing reaches Postgres any other way.

**One condition:** these hold only as long as whoever acts is held to them. The database's own user and admin
tokens can write `main` directly unless [`protect_main`](security.md#protecting-main) is on. An agent that finds such
a token in a `.env` file or a config folder gets past every rule above. Give agents their own tokens
(`CREATE AGENT`), keep admin tokens out of anything an agent can read, and turn `protect_main` on for any database
agents work on ([a hardened deployment](security.md#a-hardened-deployment)).

## What it doesn't: everything outside the database

- **Cloud and platform APIs.** An agent that calls your cloud provider, your hosting platform or a SaaS API directly
  (deleting a volume, dropping a managed database, writing to Firestore with a key it found on disk) never goes
  through Chronos. Chronos doesn't see those calls and can't undo them.
- **Files on disk**, including the database folder itself: an agent with shell access to the machine can delete it.
  Run agents in [containers](security.md#agents-in-containers) that don't mount the folder.
- **Other services' data**, even when an agent copied it from Chronos. `NOTIFY ON MERGE` queues effects until a merge,
  but what a worker then does with them is the worker's.

Scope credentials for those systems the way Chronos scopes agents: one token per agent, with only the rights its
task needs, never an account-wide key, and nothing an agent can find in an unrelated file.

## Backups: keep them out of the agent's reach

A backup an agent can delete isn't a backup. The PocketOS incident (April 2026) lost production and its backups in
one call, because the backups lived in the same volume, under the same credentials, as the database.

- **Backups** (`BACKUP DATABASE TO`, `chronos mydb backup`) are full copies of the folder
  ([operations](operations.md#backups)). Copy them to storage the database's machine, and every agent, can't write
  or delete: another account, another bucket with object lock or versioning, another machine.
- **Cloud storage** (`--remote s3://...`): the bucket holds the database as of its last checkpoint. Turn on bucket
  versioning, and keep the credentials that can delete versions away from the machine that runs Chronos and its
  agents ([cloud storage](operations.md#cloud-storage---features-s3)).
- **History is not a backup.** `RESTORE` and `UNDO` work within the history window, inside the same folder. They
  undo an agent's writes, not the loss of the folder.

## Not the database's job: rolling back the agent itself

Reverting an agent means reverting its prompt, its tool schema and its model together; reverting the prompt alone
can break against a newer tool schema. That bundle is your application's version, and Chronos doesn't version it.
What Chronos can do is say which run made each change: put the run's id, prompt version, model and tool schema in
the world's metadata when it's forked (`CREATE WORLD w WITH (run = 'planner/3', intent = '...')`), so a diff or an
audit entry leads back to the version that made it, and `UNDO AGENT` can put that run's changes back.
