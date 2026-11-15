# Attach beside MySQL

`chronos attach` also works beside a MySQL 8.0+ or MariaDB 10.5+ database. The promise is the same as with
Postgres: main is a copy of the tables you name, agents work in worlds, and a merge into main is written to MySQL
first, in one InnoDB transaction. Each row it writes must still be as main has it. If MySQL changed one meanwhile,
the merge comes back as a conflict and nothing is written.

```bash
export CHRONOS_SOURCE_URL='mysql://chronos:secret@db.internal:3306/shop?ssl-mode=VERIFY_IDENTITY'
chronos attach shop --tables customers,orders
chronos serve shop
```

The URL takes `ssl-mode` (`DISABLED`, `PREFERRED` (the default), `REQUIRED`, `VERIFY_CA`, `VERIFY_IDENTITY`),
`ssl-ca=<file.pem>` and `connect-timeout`. `mariadb://` works too. Logins: `caching_sha2_password` (MySQL 8's
default) and `mysql_native_password`. A login that needs the password itself (the first one after a server
restart with `caching_sha2_password`, or `sha256_password`) is only sent over TLS.

## What attach does

- It makes `chronos_merges` in the database (a row per merge written, plus the folder's marker row), or prints the
  statements an administrator runs.
- It reads every table in one consistent snapshot (`REPEATABLE READ`, `WITH CONSISTENT SNAPSHOT, READ ONLY`).
- It needs a primary key on each table, and InnoDB. A view, a MyISAM table, a table without a key, and a binary,
  `BIT` or spatial column are refused, each named with the reason.
- Types are held as: integers as `smallint`, `integer` or `bigint` (`bigint unsigned` as `numeric(20, 0)`),
  `decimal` as `numeric`, `float` and `double` as `double precision`, `char` and `varchar` as `varchar`, the
  text types, `enum`, `set` and `time` as `text`, `date` as `date`, `datetime` and `timestamp` as `timestamp`
  (read and written in UTC), and `json` as `jsonb`.
- A column's default comes over when it is a constant or `CURRENT_TIMESTAMP`, so a row inserted in a world holds
  the value MySQL would give. A default that is another expression (`DEFAULT (uuid())`) is MySQL's to work out: the
  column is NULL in the world, and the merge writes `DEFAULT` there. A NULL you meant as NULL is written as the
  default too.
- A generated column (`AS (...) STORED` or `VIRTUAL`) is held as a plain column that only MySQL gives values. A
  world reads it; a merge never writes it, and main takes MySQL's value back with the row. A merge that gives it a
  value of its own is refused.

## New rows in AUTO_INCREMENT tables

MySQL gives an `AUTO_INCREMENT` column its value when the row is inserted, and no values can be reserved ahead. So
a row inserted in a world gets a provisional id: a negative number, from -1,000,000,001 down, that no row of MySQL's
has. Leave the column out of the `INSERT`, as you would in MySQL.

```sql
insert into orders (customer) values ('ada');          -- id is -1000000001 in this world
insert into order_lines (order_id, sku) values (-1000000001, 'x');
```

At the merge the row is inserted without the id, MySQL gives its own (`LAST_INSERT_ID()`), and that id is written
wherever the provisional one stood: the row itself, and every row of the merge that refers to it through a foreign
key MySQL declares to the `AUTO_INCREMENT` column. Parents are inserted before their children. After the merge, main
holds the rows under MySQL's ids, and no provisional id is left anywhere.

- The id an agent saw in its world isn't the row's id after the merge. Read the row again from main by something
  else it holds (the order's reference, the customer and time) to learn it.
- A provisional id in a column with no declared foreign key to the new row's table is refused, and nothing is
  written: no number written there would be right. Declare the foreign key in MySQL and attach again, or merge the
  parent first and write its real id in a second world.
- An id given by hand (`insert into orders (id, ...) values (900, ...)`) is refused.
- New rows that refer to each other in a circle are refused: merge them in two steps.
- A table whose `AUTO_INCREMENT` column is narrower than `INT`, or is `BIGINT UNSIGNED`, takes no inserts from
  worlds: no provisional id fits it. Updates and deletes work. Attach says which tables these are, and the HTTP
  API's `attached` gives `inserts: false` for them.
- A folder attached before this takes no inserts into `AUTO_INCREMENT` tables: attach again into a new folder.
- Each new row is its own `INSERT`, so a merge of many new rows sends more SQL than one of updates.

## A merge

A merge is three round trips, whatever its size, up to 4 MB of SQL each:

1. `START TRANSACTION`, the folder's marker row, and every row it writes read `FOR UPDATE` by key. Each is
   compared with main's copy by type, so `1.5` and `1.50`, or two spellings of the same JSON, are the same value.
   Text compares exactly, whatever the column's collation.
2. The updates, inserts and deletes, then the rows read back as MySQL stored them (MySQL has no `RETURNING`), and
   the merge's row. Each statement must touch as many rows as the merge sent.
3. `COMMIT`. If the connection is lost during it, the merge's row in `chronos_merges` tells whether it committed,
   at the next merge or at `chronos serve`.

The session runs at `READ COMMITTED` with `STRICT_ALL_TABLES`, so a value MySQL would cut or change is an error
rather than a silent difference.

## Refresh

`chronos refresh shop` (or `serve --refresh-every`) reads each table whole in one snapshot and gives main MySQL's
inserts, updates and deletes. A table whose columns changed in MySQL is left as it was, and the refresh says to
attach again. Columns don't come over by a refresh yet.

## Reads checked against MySQL

A world that checks its reads (`WITH (check_reads = true)`, or an agent whose merge policy asks for it) has what it
read checked against MySQL inside the write-back, before anything is written, and by `CHECK READS` between steps, as
with Postgres: a row it read that changed in MySQL since its fork, or a row added there that its read would have
found, holds the merge (40001), and nothing is written. A world that read for one tenant is held by that tenant's
rows only.

One difference from Postgres: a table read by a filtered scan (`where status = 'new'`) is read whole from MySQL to
check it, for the world's tenant when it has one, where Postgres is asked only for the rows the filter can match. Past
100,000 rows the read is too broad to check, and a person decides.

## Dry runs

`MERGE WORLD x DRY RUN` (and `dry_run: true`, `preview()`) compares each row the merge would write with MySQL's, in
one snapshot with no locks, and checks the world's reads, as with Postgres. It names the triggers on the tables it
would write; they aren't run. Nothing is written.

## Undo by merge id

Each write-back keeps the rows it wrote in `chronos_merge_rows`, in its transaction, and a merge can be undone by
its id for 30 days, as [beside Postgres](attach-postgres.md#undo-by-merge-id). A row the merge added goes under the
id MySQL gave it; a row it deleted comes back with its old id. The table is made at the first merge if the user may
create tables; otherwise attach's printed statements make it. Keeping the records adds one round trip to a merge.

One difference: MySQL doesn't say what its cascades and triggers wrote besides the undo's own rows, so an undo isn't
held for them and a dry run's `cascades` is null. Check `ON DELETE CASCADE` foreign keys yourself before undoing an
insert.

## Not yet with MySQL

Each of these is refused with a message that says so: `serve --follow`, rehearsals (`MERGE WORLD x REHEARSE`), merge
checks run in the source, `writeback.json`, `pg_role`, and `--lazy-tenant`.

A program can ask rather than try: the HTTP API's [`attached`](../http-api.md) says `kind` (`mysql` or `mariadb`)
and `capabilities`, each of these as true or false.
