---
title: Migrating from Postgres
---

> **Beta.** Chronos 0.2 is a public beta. It speaks the Postgres 16 protocol and much of its SQL, but it is not Postgres: check your own database with the steps below before you rely on it, and keep your Postgres until you have. What is measured so far is on [Postgres compatibility](postgres-compatibility.md#where-it-stands-beta).

There are two ways in. Both leave the Postgres database untouched (they only read it).

| | `chronos import postgres://...` | `pg_dump`, replayed with `psql` |
|---|---|---|
| What you run | one command | `pg_dump` and `psql` (Postgres's own tools) |
| Needs | the `chronos` binary and network access to Postgres | `pg_dump`/`psql` and a running `chronos serve` |
| Something Chronos refuses | goes in a report at the end; the rest comes over | an `ERROR` line from psql; the rest runs on |
| Best for | most databases: you get a list of what came over in part or not at all | databases where you want to edit the SQL first, or where only a dump file is at hand |

Seven databases went through these routes in the trials before this beta (Chinook, Northwind, pgbench at 2M rows, Pagila, a synthetic 50-construct schema, a 47-type micro schema, a Rails-style app and a multi-tenant schema with row-level security and NOTIFY). Every table came over with identical row counts and row checksums; Pagila needed one manual fix (`DISTINCT` inside a user-defined aggregate, since fixed). Your schema may use something they didn't, so verify (below).

## Route 1: `chronos import`

```bash
# see what would happen; nothing is written
chronos import ./mydb "postgres://user:password@db.example.com:5432/shop" --dry-run

# do it: the folder ./mydb becomes a Chronos database (it must not be open in another process)
chronos import ./mydb "postgres://user:password@db.example.com:5432/shop"
```

It reads the catalog, makes the schema, copies every table's rows (COPY, text format, a table at a time), then adds foreign keys, indexes, views, triggers, rules and comments, and sets sequences to where Postgres had them. At the end it prints a report:

```
"customers": 2000 rows
"order_lines": 40000 rows
"orders": 20000 rows
"products": 500 rows

imported 4 of 4 tables (62500 rows) into main, and 6 foreign keys, indexes and views

also brought over (5):
  - extension uuid-ossp (built into Chronos)
  - type public.status
  - function public.touch()
  - trigger orders_touch on orders
  - 1 comments

brought over in part (2):
  - database name: Postgres's shop is the world main here (current_database() says main)
  - index orders_status: a hash index came over as a btree (it finds the same rows)

not imported (3):
  - extension isn: not in Chronos (the ones it has are built in: pgvector, PostGIS points, pgcrypto, uuid-ossp, citext, hstore, ltree, pg_trgm and others)
  - row-level security on "customers": not carried over (Chronos has it: make the policies again with CREATE POLICY, and ALTER TABLE ... ENABLE ROW LEVEL SECURITY)
  - policy own on public.customers: not carried over (make it again with CREATE POLICY: Chronos has row-level security)

roles, grants and passwords aren't imported: give each program an agent (CREATE AGENT) with the rights it needs
```

Read the whole report. An extension Chronos has built in is made as Postgres had it (`uuid-ossp`'s `uuid_generate_v4()`, `pgcrypto`'s functions); one it doesn't have is listed. Chronos has row-level security, but the importer doesn't carry the policies yet: make them again with `CREATE POLICY` and `ALTER TABLE ... ENABLE ROW LEVEL SECURITY`. The command exits with status 1 if a unique index or constraint did not come over (the table would then accept rows Postgres refused), if a table didn't come over at all (a type Chronos doesn't have, say), or if row-level security kept rows from the role you import as (below).

**Import as a role that reads past row-level security.** For a role without it, Postgres hands over only the rows a table's policies show that role, often none, and says nothing. Supabase turns row-level security on for every table its dashboard makes, so a read-only role sees empty tables. The importer never brings over part of a table that way: it asks Postgres to refuse instead (`row_security = off`, as `pg_dump` does) and checks each table first. A table whose rows the role can't all read is made, left empty, named in the report (`"customers": not copied`), and the import exits with status 1, saying so. Import as the tables' owner (it reads past their policies unless the table `FORCE`s them), a superuser, or a role with `BYPASSRLS`. On Supabase that's the `postgres` role. Through Supabase's pooler, use session mode (port 5432) with the user `postgres.<project ref>`, as `chronos import ./mydb "postgres://postgres.<ref>:<password>@aws-0-<region>.pooler.supabase.com:5432/postgres?sslmode=require"`; the direct host `db.<ref>.supabase.co` is reachable over IPv6 only. Supabase's own schemas (`auth`, `storage`) come over with the rest: `auth.users` and the tables that reference it keep working. Extensions Supabase keeps in its `extensions` schema (pgvector, PostGIS, citext, pg_trgm, hstore, ltree, btree_gist, pgcrypto, uuid-ossp) are built into Chronos: their types, functions and operator classes come over without that schema (`extensions.vector(64)` is `vector(64)` here).

**What comes over:** schemas; enum, composite, range and domain types (with defaults, `NOT NULL` and checks); sequences (with `CACHE`, set to where they were, so `serial` and identity columns go on from there); tables with their columns, defaults, `GENERATED ... AS IDENTITY`, primary keys, `UNIQUE`, `CHECK`, `NOT NULL`, exclusion constraints, partitions and inheritance; every row; foreign keys; indexes; views and materialized views; SQL and PL/pgSQL functions and procedures; plain aggregates; triggers; rules; comments. The full list is in [SQL: Moving from Postgres](sql.md#moving-from-postgres).

**What comes over in part, and is listed:** an index method Chronos has no use for (`hash` and `brin` become btrees; they find the same rows), `DESC`/`NULLS FIRST` and `INCLUDE` on an index, operator classes (but pgvector's and pg_trgm's, which are kept) and storage parameters. A table whose partitioning or inheritance Chronos refuses comes over as one plain table with all its rows.

**What doesn't come over, and is listed:** extensions other than the ones built in (`vector`, `postgis` points, `citext`, `hstore`, `ltree`, `pg_trgm`, `pgcrypto` and the others on [Postgres compatibility](postgres-compatibility.md#known-gaps)); functions in languages other than SQL and PL/pgSQL (C, PL/Python, PL/v8, ...); ordered-set aggregates; operators; event triggers; publications; foreign tables; row-level security and its policies (make them again with `CREATE POLICY`: Chronos has row-level security, the importer doesn't carry it yet); columns' collations (text sorts by bytes, as `COLLATE "C"` does, unless you give a column an ICU collation); roles, grants and passwords.

**Connecting:** `sslmode` (`prefer` by default, `disable`, `require`, `verify-ca`, `verify-full` with `sslrootcert=<file.pem>` or the system's CAs) and logins with a password, MD5 or SCRAM-SHA-256 (with channel binding over TLS) work as in libpq. `-b <world>` imports into another world than `main`.

**While it runs:** the import reads Postgres as it is at that moment. Rows written to Postgres during the import may or may not be in it: stop writes to Postgres first (or import from a replica or a restored backup), then point the application at Chronos.

## Route 2: `pg_dump` and `psql`

```bash
chronos serve ./mydb &                             # Postgres protocol on 127.0.0.1:5433
pg_dump --no-owner --no-privileges -d shop -f shop.sql
psql "postgres://me@127.0.0.1:5433/main" -X -v ON_ERROR_STOP=0 -f shop.sql 2> errors.txt
grep -c ERROR errors.txt                           # how many statements were refused
```

- `--no-owner --no-privileges` leaves out `ALTER ... OWNER TO` and `GRANT`, which name roles that don't exist in a new Chronos database. Make the roles you need first (`CREATE ROLE`) if you want to keep them.
- Plain-format dumps only (`-Fp`, the default): `pg_restore` of a custom or directory dump works by writing it out as SQL first, `pg_restore -f shop.sql shop.dump`.
- psql sends each statement on its own, so a refused one stops nothing else. Read `errors.txt`: each line names the statement (line number in `shop.sql`) and the reason. Typical ones are `CREATE EXTENSION` of an extension Chronos doesn't have and functions in other languages.
- `COPY ... FROM stdin` blocks in the dump load the rows (text format).

## Check that everything came over

Run the same queries on Postgres and on Chronos and compare the output. Row counts first:

```sql
-- every user table and its exact row count (run it on both, compare)
select format('select %L as t, count(*) from %I.%I', table_schema || '.' || table_name, table_schema, table_name)
from information_schema.tables
where table_type = 'BASE TABLE' and table_schema not in ('pg_catalog', 'information_schema')
order by 1 \gexec
```

Then a checksum of each table's contents, which is what the trials compared. It casts every column to text, so it compares values as Postgres writes them:

```sql
-- one table: the same md5 on both means the same rows
select count(*), md5(string_agg(r, ',' order by r))
from (select concat_ws(chr(1), coalesce(id::text, '<NULL>'), coalesce(name::text, '<NULL>'), coalesce(price::text, '<NULL>')) as r
      from products) s;
```

```sql
-- the same for every table, generated from the catalog (psql's \gexec runs each line)
select format(
  'select %L as t, count(*), md5(string_agg(r, %L order by r)) from (select concat_ws(chr(1), %s) as r from %I.%I) s',
  c.table_schema || '.' || c.table_name, ',',
  string_agg(format('coalesce(%I::text, %L)', c.column_name, '<NULL>'), ', ' order by c.ordinal_position),
  c.table_schema, c.table_name)
from information_schema.columns c
join information_schema.tables t using (table_schema, table_name)
where t.table_type = 'BASE TABLE' and c.table_schema not in ('pg_catalog', 'information_schema')
group by c.table_schema, c.table_name
order by 1 \gexec
```

```bash
psql -X -At -d shop -f checksums.sql > pg.txt
psql -X -At "postgres://me@127.0.0.1:5433/main" -f checksums.sql > chronos.txt
diff pg.txt chronos.txt && echo "same rows"
```

On a table of many millions of rows, `string_agg` holds the whole table's text in memory on both sides; check such a table in key ranges (`where id between ... and ...`) instead.

What a matching checksum doesn't prove: that queries give the same results (run your application's own tests against Chronos), or that every constraint, trigger and default came over (compare `\d table` on both, and read the import report).

## Point your application at it

`chronos serve` listens on 127.0.0.1:5433 by default. The database name is the world: `main` is the one you imported into.

```bash
chronos serve ./mydb                                         # loopback only, no password asked
DATABASE_URL="postgres://app@127.0.0.1:5433/main"

# from other machines: a token (24 characters or more) and TLS are required; the admin token is your application's
# password (any user name): the server token lets a client in only as the guest
export CHRONOS_TOKEN=$(openssl rand -hex 24) CHRONOS_ADMIN_TOKEN=$(openssl rand -hex 24)
chronos serve ./mydb --pg 0.0.0.0:5433 --tls-cert cert.pem --tls-key key.pem
DATABASE_URL="postgres://app:${CHRONOS_ADMIN_TOKEN}@db-host:5433/main?sslmode=require"
```

Without any token, anyone who can reach the loopback port can log in as any user, superuser included. Over a network, use `--token` and TLS (`--tls-cert`, `--tls-key`), and the admin token for the application that owns the data; see [security](security.md) and [operations](operations.md#tls). Before 0.4 the server token was the database's own user: [upgrading](security.md#upgrading-from-03-the-server-token).

**ORMs and drivers** connect as they do to Postgres:

- **Set `DATABASE_URL`** (or the driver's host, port 5433, database `main`) as above. Which drivers and ORMs have been run against Chronos, and how far, is under [Clients](postgres-compatibility.md#clients); the others should connect, and what decides whether they work is the SQL they send. Run your migration tool (Rails, Django, Alembic, Prisma Migrate, ...) against a fresh, empty Chronos database once before you move data, to see whether any statement it sends is refused.
- **Isolation:** `READ COMMITTED` is the default, as in Postgres, and `REPEATABLE READ` is supported. `SERIALIZABLE` is refused (0A000): an ORM or setting that asks for it must ask for `REPEATABLE READ` instead, and code that relied on serializable checks to stop write skew needs explicit locks (`SELECT ... FOR UPDATE`).
- **Connection poolers:** a session-level `SWITCH WORLD` stays on that connection. If you use worlds, put the world in the connection string, one pool per world.

## Known differences to plan for

The full list is on [Postgres compatibility](postgres-compatibility.md#known-gaps). The ones that most often matter in a migration:

- **Not supported:** C-language functions and extensions that ship C code (only the extensions built into Chronos work), custom index access methods, `SERIALIZABLE`, logical replication and publications in use, foreign tables in use.
- **`ALTER TABLE ... RENAME TO` and `ALTER TABLE ... SET SCHEMA` rewrite every row of the table**: about 6.5 s per million rows, where Postgres changes only the catalog. Run them in a maintenance window on big tables. (Renaming a column, adding or dropping one, and most other `ALTER TABLE` changes write only the catalog, as in Postgres.)
- **`ALTER TABLE ... ADD COLUMN` with a default that differs per row** (`random()`, `gen_random_uuid()`, `serial`, a sequence's value, a function of your own) is refused (0A000) on a table that has rows, where Postgres fills each row: add the column, `UPDATE` it, then `ALTER COLUMN ... SET DEFAULT`. A constant, `now()` or `current_date` default works as in Postgres. Migration tools that add such columns to existing tables need that step by hand.
- **History is kept for 1 hour by default.** Chronos keeps old versions of rows for time travel and undo. A new database keeps 1 hour (`alter system set history_retention = '30 days'` keeps more, at the cost of disk); a database made by an earlier release keeps the 30 days it had.
- **Text sorts by bytes** (`COLLATE "C"`) unless a column has an ICU collation, where your Postgres probably used `en_US.UTF-8`. `ORDER BY name` can give a different order; give the column `collate "en-US-x-icu"` if it matters. `chronos import` says so first in its report when the source database's collation isn't `C`, `POSIX` or `C.UTF-8`, naming the collation to use.
- **`EXPLAIN` output** shows how Chronos runs the statement, not Postgres's plan; don't parse it. `SET chronos.explain = postgres` makes `EXPLAIN (COSTS OFF)` of simple statements show PostgreSQL 16's plan, with a NOTICE (see [EXPLAIN](sql.md#explain)).
- **Function signatures** in the catalog show `text` where a parameter was declared `varchar` (`\df`, `pg_get_function_arguments`).
- **A multi-statement simple query** (`insert ...; insert ...;` in one string) commits statement by statement, where Postgres runs it as one implicit transaction. Wrap such strings in `BEGIN ... COMMIT`.
- **Speed:** in the trials, point reads ran at about half Postgres's rate and writes at a fraction of it on the same machine. Measure your own workload before moving one that is sensitive to latency.

## Going back

Postgres's `pg_dump` reads a Chronos database through its catalog and writes a script that restores into Postgres ([Dumping with pg_dump](sql.md#dumping-with-pg_dump)):

```bash
pg_dump -h 127.0.0.1 -p 5433 --no-owner main > from-chronos.sql
psql -d shop_again -f from-chronos.sql
```
