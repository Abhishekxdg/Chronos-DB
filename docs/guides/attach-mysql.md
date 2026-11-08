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
- It needs a primary key on each table, and InnoDB. A view, a MyISAM table, a table without a key, a binary,
  `BIT` or spatial column, and a generated column are refused, each named with the reason.
- Types are held as: integers as `smallint`, `integer` or `bigint` (`bigint unsigned` as `numeric(20, 0)`),
  `decimal` as `numeric`, `float` and `double` as `double precision`, `char` and `varchar` as `varchar`, the
  text types, `enum`, `set` and `time` as `text`, `date` as `date`, `datetime` and `timestamp` as `timestamp`
  (read and written in UTC), and `json` as `jsonb`.
- A table keyed by `AUTO_INCREMENT` takes no inserts from worlds yet, since its values are MySQL's to give.
  Updates and deletes work.

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

## Not yet with MySQL

Each of these is refused with a message that says so: `serve --follow`, dry runs and rehearsals, `check_reads`,
merge checks run in the source, `writeback.json`, `pg_role`, and `--lazy-tenant`.
