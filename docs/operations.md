# Operations

## The database folder

```
mydb/
  LOCK            held while a process has the database open (one process at a time)
  wal-<n>         write-ahead log segments: every change, checksummed, in order
  manifest        the last checkpoint: each branch's tree roots, version and crash flag
  pages/          pack-<n> files of immutable, content-addressed, LZ4-compressed pages (+ .idx)
  remote, owner   only when attached to object storage (owner also with an anchor)
  reads           which clients' SQL read which columns, and when (see DIFF ... READERS); not published to a remote
```

- **Opening:** it loads the manifest, replays the log after it, and reads pages only when needed.
- **Torn tails:** a torn write at the end of the log (from a crash) is dropped. Corruption anywhere else refuses to open rather than guessing.

## Checkpoints

A checkpoint folds every branch's recent changes into its page tree, writes a new manifest and drops the old log.
- **When:** in the background once the log passes 64 MB, or on `checkpoint` (shell), `Db::checkpoint()`, or closing a cloud-backed database.
- **Cost:** only changed pages are written.
- **Memory:** it holds only changes since the last checkpoint.

## Cleanup

Pages nothing uses any more (overwritten data, merged or discarded branches) are deleted by cleanup.
- **When:** automatically after a checkpoint once page files double in size; in the background, with a checkpoint first, once 1,000 worlds, or worlds that changed 100,000 rows, were discarded since the last cleanup; or with `gc` (shell) or `Db::gc()`.
- **What's kept:** anything a branch, an in-flight reader or the manifest on disk still needs, and **history**: pages the checkpoints kept for time travel still use stay for the retention window (1 hour for a new database; 30 days for a folder made by an older release, which has no `retention` file). The window costs disk: old pages and log stay for all of it, so a busy database grows with it. Dropping worlds frees their space only once history no longer holds them; lower `history_retention` to free it sooner.
- **What it would free:** `SHOW DISK` gives bytes of pages in use (`live`), kept only for history (`history`), needed by nothing (`reclaimable`), and the log with its history (`log`). `Db::disk_usage()` in Rust. It reads every tree's inner pages, as cleanup does.
- **Order:** live pages are copied out of mostly-dead files and made durable before the old files are deleted.

## Crashes

- **Always safe:** `main` writes and merges into `main` are fsynced before they return (with the default `synchronous_commit = full`; see [concepts](concepts.md#durability-in-one-table)).
- **Can be lost:** writes to other branches can be lost in a crash, but never silently. Each world logs its writes in batches (at its next fork, merge or discard, another agent's write to it, a checkpoint, a read of history, or shutdown), so a crash can lose them even after a later `main` commit was synced.
  - After a crash, open branches are flagged.
  - Merging one needs the version its last write returned, or an explicit confirm after checking `diff`.
  - See [concepts](concepts.md#versions-and-crashes).
- **Checkpoints and cleanup** can be interrupted at any step: tests/crash.rs kills them at every step between their file writes, renames and deletions (232 points), and each time the folder reopens with the same rows and passes `VERIFY DATABASE`.
- **Schema changes are all or nothing:** a crash comes back as before or after each CREATE, ALTER or DROP, never halfway.
- **A full disk:** a page write that fails part way is cut off its page file, so the pages written after it in the same process land where the index says; the checkpoint that hit it fails, and the next one, once there is space, writes the rest. A log write that fails stops the process (as Postgres does), and the next open cuts the torn tail. Either way, after freeing space the folder opens with every acknowledged write and passes `VERIFY DATABASE` (tests/disk_full.rs, injected failures and a filled tmpfs).

## Cloud storage (`--features s3`)

```bash
export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=…
export AWS_ENDPOINT=https://<account>.r2.cloudflarestorage.com     # R2, MinIO, B2; omit for AWS
chronos --remote s3://bucket/prefix mydb        # or: chronos serve … with the folder already attached
```

- **Uploads:** checkpoints upload finished page files, their indexes, then the manifest, so pages always land before the manifest that names them. Closing the database checkpoints.
- **Another machine:** an empty folder attached to the same remote opens the database as of its last checkpoint and downloads pages as they're read. The first read of a page downloads its whole page file (up to 64 MB), which then stays on local disk until `Db::evict_local()`, so the local disk needs room for the files a workload touches.
- **Scale to zero:** `Db::evict_local()` deletes local copies of uploaded pages, leaving the bucket as the database.
- **The log ships too:** as it is written, the log goes to the bucket in chunks (`wal-<segment>-<offset>` objects), so another machine opening the bucket has every commit that was shipped, not just the last checkpoint. `wal_shipping` (see [settings](reference/cli.md)) says when: `async` (the default) within `wal_shipping_delay` (1 s) of being written; `sync` before a commit to `main` or a merge returns, which adds a round trip to the bucket to each; `off` only at checkpoints, as before. Checkpoints delete the shipped log the bucket no longer needs, and publish the folder's own settings (`folder-settings`), which a new folder takes. In `sync` mode, while the bucket can't be reached for 10 s, new writes and merges are refused with SQLSTATE 58030 (reads work) until it can. `/metrics` shows `wal_shipping_lag_bytes`, `wal_shipping_lag_ms`, `wal_shipping_uploads`, `wal_shipping_upload_failures` and `wal_shipping_fenced`. A `gs://` remote reads its token from `CHRONOS_GCS_TOKEN_FILE` when that is set. Put costs follow the number of chunks: about $0.0013 per GB written at bulk rates on GCS, and at a slow trickle one put per delay while writes continue.
- **One writer per remote,** enforced by a `lease` object:
  - A second folder gets an error naming the holder.
  - If that process is gone, open with `--takeover` (or `Db::open_remote_takeover`). The old writer is fenced off: it re-checks the lease before every upload of pages, and the takeover puts a fence at the end of the shipped log, which its next log upload runs into. A fenced writer in `sync` mode stops (it can't say a commit is safe); in `async` mode it stops shipping and its checkpoints fail.
  - A folder that fell behind (another folder wrote since) is refused; with `--takeover` its own database is set aside in `stale-<ms>`, or, if only its log differs, the log in `stale-log-<ms>`.
- **Local testing:** a local MinIO container works well for this (see `tests/remote.rs`).

## High availability

Chronos is one process that writes one database. There is no replica that follows it and nothing fails over on its own; what keeps a database available is how fast another process can open it.

- **One machine:** the database is its folder. `main` writes and merges are on disk before they return, so a crash or a `kill -9` loses none that were acknowledged; the process opens the folder again in the time it takes to read the log since the last checkpoint. Run it under a supervisor that restarts it.
- **Losing the machine:** with [cloud storage](#cloud-storage---features-s3) the bucket holds the database as of its last checkpoint plus the log shipped since. Another machine opens it from an empty folder with `--takeover`, which fences the old writer off. With `wal_shipping = sync` that loses no acknowledged write; with `async` (the default), at most about `wal_shipping_delay` of writes (1 s); with `off`, everything since the last checkpoint.
- **Without cloud storage:** a [backup](#backups) is the copy on another machine, and it is as old as it is.
- **One writer, always:** two processes never write the same database, on a folder (the second gets an error) or a bucket (the lease).
- **Not here:** a hot standby that follows the log, automatic failover, several writers.

## TLS

`chronos serve mydb --tls-cert cert.pem --tls-key key.pem` (or `CHRONOS_TLS_CERT` and `CHRONOS_TLS_KEY`) turns on TLS for both ports:
- **HTTP** becomes HTTPS only. Clients use `https://` URLs.
- **Postgres** requires TLS on every connection. psql's default (`sslmode=prefer`) uses it; use `sslmode=verify-full` with `sslrootcert` to check the certificate too. A client that won't use TLS is refused.
- **Files:** PEM, the certificate chain first, then the key (PKCS#8, PKCS#1 or SEC1). TLS 1.2 and 1.3.
- **Still needs a token** on any address other than loopback: TLS protects the token, it doesn't replace it.
- **Certificate changes** take a restart.

## Encryption at rest

A database made with a key is encrypted in every file that holds data. That covers log records, the manifest and its history copies, and every page. Page names become keyed hashes, so they don't reveal content either. Backups and cloud uploads copy those bytes, so they're encrypted too.

```bash
chronos keygen                                  # prints a new key: 64 hex digits
CHRONOS_KEY=<key> chronos mydb                    # a new folder is encrypted from its first byte
CHRONOS_KEY_FILE=/run/secrets/chronos chronos serve mydb
```

- **The key is never stored.** The folder keeps only a check value (`encryption`), so a wrong key or a missing one is refused on open. Lose the key and the data is gone.
- **Environment only:** there's no command-line flag for the key, which would show in the process list. In Rust, use `Db::open_encrypted(dir, &key)` or `Db::open_with`.
- **Authenticated** (ChaCha20-Poly1305): changed bytes are refused as tampering, never read as data. Log records are bound to their place in the log.
- **Chosen at creation:** an existing unencrypted folder can't be opened with a key. There's no key rotation yet.
- **Restoring an encrypted backup** needs the key: `CHRONOS_KEY=<key> chronos restore /backups/db-1 newdb`.

## Rollback detection (anchor)

Someone who can write the folder can put back an older, consistent copy of its files, and it would open as that older state, even encrypted. An anchor catches that for a local folder (a remote-backed folder already refuses a state that isn't the remote's latest):

```bash
chronos --anchor /var/lib/chronos-anchors/mydb serve mydb   # or CHRONOS_ANCHOR=/var/lib/chronos-anchors/mydb
```

`Open { anchor: Some(path), .. }` with `Db::open_with` in Rust.
- **What it holds:** the database's identity (the folder's `owner` file) and how far its log has got, rewritten durably after every checkpoint's manifest and at every clean close.
- **At open:** a folder behind its anchor refuses: `database folder is older than its anchor: rolled back?` (code XX0A1). So does a folder that isn't the one the anchor belongs to, and an existing folder whose anchor is missing, since deleting the anchor would otherwise skip the check. A brand-new folder creates its anchor.
- **Crashes:** a crash after a checkpoint's manifest but before the anchor moved leaves the folder ahead of it: that opens, and moves the anchor on. Only behind is refused.
- **Restores and starting on an existing database:** open once with `--anchor-reset` (`CHRONOS_ANCHOR_RESET=1`, `Open::anchor_reset`): the folder is accepted as it is and the anchor rewritten for it. E.g. `chronos restore /backups/db-1 mydb` then `chronos --anchor A --anchor-reset mydb checkpoint`. A backup has an identity of its own, so a restored folder always needs this once.
- **Where to keep it:** outside the folder, somewhere whoever can write the folder can't (another volume, owned by another user). With a key, the anchor is sealed with it, so it can't be forged without the key (an older copy of it can still be put back); without one it's a plain file, protected only by its location's permissions.
- **Not covered:** commits since the last checkpoint or clean close aren't in the anchor, so cutting the log back within that stretch goes unnoticed.

## Limits and cleanup

Settings for a server shared by agents, all off by default:

- **`ALTER SYSTEM SET statement_timeout = '30s'`:** every SQL session starts with it (a session may change its own).
- **Per agent** (`CREATE AGENT` / `ALTER AGENT`, or HTTP `create_agent` / `alter_agent`): `max_query_ms`, `max_concurrent`, `max_memory_mb`. See [concepts](concepts.md#agents).
- **`ALTER SYSTEM SET max_worlds = n`:** forks past n live worlds are refused.
- **`ALTER SYSTEM SET world_idle_ttl = '7 days'`:** the background worker discards worlds unused that long (see [concepts](concepts.md#worlds)), logged and audited as `system`, then gives the space back (see Cleanup).
- **`ALTER SYSTEM SET protect_main = on`:** `main`'s rows change only by merging a world, for every user (see [SQL](sql.md#limits-for-the-whole-database)).
- **`chronos serve --token T --admin-token A`:** `T` (or `CHRONOS_TOKEN`) guards the port and lets a client in as the guest, MCP's safe mode (see [concepts](concepts.md#agents)); `A` (or `CHRONOS_ADMIN_TOKEN`) is the database's own user. `--unsafe` makes `T` the database's own user, as before 0.4; `--safe` makes every client but an agent or `A` the guest, loopback without a token too.

## Limits the operator sets

For a host that runs databases for others, whose admins hold the admin token and so can change every setting above: two limits set where the process starts, which nothing inside the database can raise. In Rust, `Open::max_disk_bytes` and `Open::ceilings` (None reads the environment).

- **`CHRONOS_MAX_DISK_BYTES=536870912`:** the most the folder may hold, in bytes (unset or 0: no cap). It counts the page files, the log (the live segment by its records, not the 4 MB of zeros kept ahead of them), history, page indexes, big statements' spills in `<database>/spill`, and the rest of the folder; and queries' spill files when `CHRONOS_SPILL_DIR` is inside the folder (set it to `<database>/spill` so they count; files there are deleted as queries end, and the folder emptied at open). Pages and the live segment are followed by running counts, the rest measured at most a second apart (and after every checkpoint and cleanup), so the check costs a few atomic reads per write. A write (`INSERT`, `UPDATE`, `COPY`, a big statement's part, a query's spill), a fork, `CREATE DATABASE` and a world's `CHECKPOINT 'label'` that would take the folder past the cap is refused: `53100` `database is over its 512 MB limit: drop worlds, delete rows or upgrade`, over HTTP `507`. Reads, `DELETE`, `DROP`, `TRUNCATE`, `BEGIN`, `COMMIT`, merges (their rows were let in when written in the world), discarding worlds, `ALTER SYSTEM`, checkpoints and cleanup keep working, so a full database can free itself: within a tenth of its cap every checkpoint cleans up too, and the background worker checkpoints once a megabyte of log (a sixteenth of the cap, if less) has piled up since the last, so deleted rows give their space back within about 30 seconds (`CHECKPOINT` does it at once). History is kept for its window whatever the cap: lower `history_retention` to free it. A change near the cap can still pass it a little (a checkpoint's new pages, a merge, a delete's log record); the filesystem's own quota is the backstop. `SHOW DISK` gives `total` (the folder as the cap counts it) and `limit`; `/metrics` `chronos_disk_bytes` and `chronos_disk_limit_bytes`.
- **`CHRONOS_LIMITS=/etc/chronos/limits.toml`:** ceilings over the database's own settings, one `key = integer` a line (`#` comments; `_` between digits). A key it doesn't know, or a value that isn't a whole number, stops the database from opening.

  ```toml
  max_worlds = 100
  history_retention_ms = 86_400_000   # a day
  world_idle_ttl_ms = 604_800_000     # 7 days
  statement_timeout_ms = 30_000
  ```

  `ALTER SYSTEM SET` past a ceiling is refused: `42501` `max_worlds may be at most 100 on this database, not 500: a limit of its plan, which its operator sets (CHRONOS_LIMITS) and ALTER SYSTEM can't raise`. A setting left at 0 (no limit) is the ceiling, and so is one set before the ceiling was: `SHOW max_worlds` says what's in force. `history_retention` past its ceiling is lowered to it when the database opens, and the history past it goes at the next checkpoint. A session's `SET statement_timeout` above the ceiling, or to 0, is clamped to it (`SHOW statement_timeout` says so), as an agent's `max_query_ms` clamps it.

## Memory and spilling to disk

A query's grouping or sorting holds up to `CHRONOS_WORK_MEM` (default `256MB`; also `64MB`, `2GB`, or plain bytes). Past that it spills to temporary files in `CHRONOS_SPILL_DIR` (default the system's temporary folder), deleted when the query ends. `spilled_bytes` in `SHOW METRICS` counts what was written.

- **`GROUP BY` on one table:** each part is grouped as it's read, so memory holds the groups, not the rows. With more groups than fit, the table is read again and every row goes to one of several files by its group; each file is then grouped on its own. The answer is the same as without spilling, groups in the order they first appear.
- **`ORDER BY` on one table:** each part's sort keys and output rows go to a sorter. With a `LIMIT`, only the best rows are ever held, so a top-10 of a billion rows needs memory for ten. Without one, rows are held while they fit, then written out as sorted runs and merged at the end. Ties keep the table's order either way.
- **Hash joins:** when the table a join builds its hash table from outgrows the budget, both sides go to files by the hash of the joined value, and each pair of files is joined on its own (a grace hash join). Rows come out in the order they would in memory, and a `GROUP BY` over a two-table join is grouped file by file. This works at every step of a join of several tables.
- **`DISTINCT` aggregates** (`count(DISTINCT x)`, `sum(DISTINCT x)`, `string_agg(DISTINCT ...)`, ...) on one table are grouped part by part like any `GROUP BY`, each part's distinct values merged into the group's; the values count against the budget. Past it, rows go to files by their group and, when every `DISTINCT` aggregate takes the same argument (and no list aggregate would be split), by that argument too, so one group's values are spread over files with none in two; each file is grouped alone and the groups' results added up. On the VM with `CHRONOS_CACHE_MB=32` and `CHRONOS_WORK_MEM=64MB`, `count(DISTINCT j)` of 100,000 values over 4 million rows peaked at 94 MB (main: 1,488 MB) and over 10 million at 295 MB (main: 3,271 MB), of which about 195 MB is the scan itself (a plain `count(*)` took 195–210 MB on both).
- **Big writes:** an `INSERT`, `UPDATE`, `DELETE` or `COPY` whose writes outgrow half the budget goes on in parts, its writes spilling to `<database>/spill` until it commits (see [databases bigger than memory](#databases-bigger-than-memory)).
- **Not yet:** the result itself is held in memory (it's sent to the client whole), and so are a join's output before its `ORDER BY`, and `GROUP BY` over three or more tables, `DISTINCT` aggregates over a join, and window functions. Different `DISTINCT` arguments in one query (or one with a list aggregate) spill by group only, so one huge group still holds its values.
- **Bounding every query:** `CHRONOS_QUERY_MEM` (default a quarter of the machine's memory; `0` for no limit) stops any statement (SQLSTATE 53200) once the rows it holds (joined rows, a join's other side, its result) pass it, so one runaway join fails alone instead of taking the server's memory from every session. All statements together may hold twice that (by default half the machine's): past it, those holding at least a sixteenth of it fail and the small ones go on. A join's probe checks as it goes, on every core. It counts row slots and values, not every structure (a `GROUP BY`'s groups and a window's rows aren't counted), and a vector growing briefly holds its old and new copy, so the process can pass the limit by a few times for a moment.
- **Bounding an agent:** `max_memory_mb` stops its statement (SQLSTATE 53200) the same way at its own limit, when that's lower, and its spilling starts at a quarter of it.

## Databases bigger than memory

Pages stay on disk (or in the bucket) and are read as queries need them, so a database can be many times the machine's memory. What a process holds:

- **Page cache:** decoded pages, up to `CHRONOS_CACHE_MB` (default 256) of memory. Pages are counted by the memory they take decoded, which for small rows is about 2.5 times their size on disk, and the cache is back under its cap after every page it takes in. A page read since it last came up for eviction gets a second chance (CLOCK). `cache_bytes`, `cache_hits` and `cache_misses` in `SHOW METRICS` show how it's doing.
- **Page directory:** where each page is, about 90 bytes per page and twice that while opening. This is the part that grows with the database: 330,000 pages (811 MB) take about 30 MB, so 100 GB of pages would take about 4 GB.
- **Queries:** scans and key ranges read as they go. Lookups by key hold the rows they find, and so do integer-key ranges on tables made with 0.1.0 or before (up to 100,000 keys). B-tree index reads hold the row ids they find, up to a fifth of the table or `CHRONOS_WORK_MEM` of ids (about 4 million at 256 MB), and read each row as the query reaches it; wider ones scan. Grouping, sorting and joins stay within `CHRONOS_WORK_MEM` (above). A unique check, `ON CONFLICT` included, is one lookup of the constraint's own entry, however big the table.
- **The search index behind `column = value`:** a query whose equality condition is on a column no B-tree index covers builds it, only for tables under `CHRONOS_WORK_MEM / 8` of rows (32 MB by default; see [SQL](sql.md#speed)), and the last four versions of each table's index are kept, a new one built once more than 5% of the table's rows (and more than 256) have changed since the newest. It holds every row, a bitmap per distinct value and a text index of every text column, so it takes many times the table's size: on the VM a Rails-style `comments` table of 150,000 rows (18.5 MB of row text: short text, an md5, timestamps) took 1.57 GB, and four versions of it 6.6 GB. No cap covers all tables together. Where that matters, give the column a B-tree index (`CREATE INDEX`, which the query then reads instead) or lower `CHRONOS_WORK_MEM` (at `64MB`, tables over 8 MB of rows are scanned instead).
- **Freed memory:** Linux builds use mimalloc, which hands memory a query freed back to the system lazily, as the threads that freed it run again: RSS can stay at a big query's peak while the server is idle (on the VM 5.0 GB, down to 2.4 GB at the next queries). `MIMALLOC_PURGE_DELAY=0` hands it back at once, for a little more time spent in the allocator.
- **Building an index** (`CREATE INDEX`, `ADD CONSTRAINT ... UNIQUE`): `CHRONOS_WORK_MEM` for sorting the entries (spilling past it), plus a checkpoint's worth of written entries, whatever the table's size (see [SQL](sql.md#indexes)). On 7 million rows the process peaked at 548 MB (767 MB for a unique index on two columns) with the default 256 MB. Adding a `PRIMARY KEY` to a table with rows moves every row to its key: on a big table in parts (see whole-table rewrites below).
- **Whole-table rewrites** (`ADD PRIMARY KEY`, `ALTER COLUMN ... TYPE` that converts the values or retypes the key, `ADD COLUMN` of a generated column, and `CLUSTER` of a table without a key): on a table past an eighth of `CHRONOS_WORK_MEM`, in a database on disk, the rows are read in key order and their new versions go on in parts like a big statement's (below): each part's rows and remade index rows go to a run in `<database>/spill`, every row's new key is marked so two rows taking one key are found when the runs merge (23505 `could not create unique index "t_pkey"`, `Key (id)=(…) is duplicated.`, and nothing is written), and one checkpoint switches the world to the new tree: a crash leaves the table as it was or as it became, never a mix. Memory holds a part, a block of each run and a batch of changes, whatever the table's size: on the VM with the default 256 MB, `ADD PRIMARY KEY` on 1, 5 and 20 million 100-byte rows peaked at 417, 619 and 953 MB of RSS (the process held 41, 154 and 392 MB before it: what grows is the database's page directory and cache), where held in memory 1 and 5 million took 1.5 and 6.1 GB; `TYPE ... USING`, a generated column and a key's new type on 5 million rows at 556–632 MB, `CLUSTER` at 922 MB (it sorts the rows too). Held in memory still, as before: a table with partitions, parents or children, an `EXCLUDE` constraint or a foreign key to itself, several changes in one `ALTER TABLE`, and statements run by functions.
- **Changes since the last checkpoint:** a checkpoint starts once the log passes 64 MB, and writers wait for one past 256 MB. Rows held this way take about twice their size (a 54 MB statement of 100,000 rows held 63 MB).
- **A big statement:** a statement holds its writes in memory while they fit in half of `CHRONOS_WORK_MEM`, and commits as one write, as it always did. Past that, an `INSERT` (`VALUES`, `SELECT` or `COPY`), `UPDATE` or `DELETE` goes on in parts of a quarter of `CHRONOS_WORK_MEM` (by a rough count of its writes; they take about three times that in memory with their index entries):
  - **Each part** is checked against the table as the statement found it (keys, `UNIQUE`, `CHECK`, `NOT NULL`, foreign keys), and its rows and index entries go to a sorted, lz4-compressed run file in `<database>/spill` (sealed with the database's key when it's encrypted), with marks for what depends on other parts: a key a row takes, a unique value it takes or lets go of. Runs are merged 16 at a time as they pile up. An `INSERT ... SELECT` reads its `SELECT` a part at a time when it's one table or `generate_series` with no grouping, ordering, `DISTINCT`, `LIMIT`, joins or subqueries; other `SELECT`s are computed whole first, and their writes still spill.
  - **The commit** merges the runs in key order. Two rows of the statement taking one key or unique value, or a row taking one the table already has for a row the statement doesn't change, is an error (23505), exactly as for a small statement, and nothing is written. The merged changes go into a new tree built beside the world's, 8 MB at a time, its pages written but used by nothing; then one checkpoint makes that tree the world's state, with the world's writers held off and readers switching at once. Its manifest is the commit record: a crash before it leaves the table as it was, a crash after it keeps the whole statement, and nothing waits in memory for the next checkpoint. A statement too big to hold is logged as a note (`Op::Bulk`: the world, the rows, the new tree), so history, `SHOW HISTORY`, time travel and merges see one write.
  - **Memory** holds a part, a block of each run being merged, and a batch of changes, whatever the statement's size: under a 30 GB cap on the VM, `INSERT ... SELECT`s of 200,000, 1 million and 2 million 540-byte rows into a table with two indexes peaked at 61, 67 and 78 MB with `CHRONOS_WORK_MEM=32MB` (main before this: 439 MB, 2.3 GB and 4.5 GB), and at 405, 406 and 411 MB for 1, 5 and 10 million rows with the default 256 MB; `COPY`s of 2 and 4 million rows through the server at 440 and 419 MB (see [BENCHMARKS.md](https://github.com/Abhishekxdg/chronosdb/blob/main/BENCHMARKS.md#12-a-database-bigger-than-its-cache)). What grows is the database's share, not the statement's: the page directory (above), and the page cache that a statement's checks read through. 200,000 rows into a table that already held 2 million peaked at 154 MB, against 62 MB into an empty one.
  - **Disk:** the runs take about the statement's writes, compressed, until it ends; they are deleted when it ends however it ends, and the spill folder is emptied when the database opens, so a crash leaves none. `spilled_bytes` in `SHOW METRICS` counts them. Pages of a tree that never committed are freed by cleanup.
  - **Held in memory still** (they write as before, all at once): `INSERT ... ON CONFLICT`, anything with `RETURNING`, tables with triggers (any, enabled) or a foreign key to themselves, a `DELETE` or a key-changing `UPDATE` of a table other tables' foreign keys cascade or set null from, statements run by functions or triggers, and ones whose functions wrote rows before the statement got big (0A000 if they write after), schemaless (JSON) tables, temporary tables, and databases in memory. Inside `BEGIN ... COMMIT` (or on a world) the statement spills and commits into the transaction's hidden world as above; for its `COMMIT` see the next point.
- **A big transaction** (`BEGIN ... COMMIT` around millions of rows: `pg_restore -1`, `psql --single-transaction`, `pgbench -i`, a migration's data step; and `CREATE TABLE AS`, which makes a transaction of its own): its big statements spill into the transaction's hidden world as above, and checkpoints fold the rows of its small ones into pages as they do any world's, so memory holds what one statement needs. A `COMMIT` of a transaction whose rows went into pages of their own (or that changed 10,000 rows or more) doesn't merge them one by one: when nothing was committed to the world since the transaction began, the world takes the transaction's state whole, at once; when other sessions committed meanwhile, the transaction is first brought up to what they committed (as a `READ COMMITTED` statement is; a row both changed fails the `COMMIT` as before), then its rows are read in key order, a sixteenth of `CHRONOS_WORK_MEM` of keys at a time (at most 4 MB), and checked on the result as a merge checks them (unique values, foreign keys, `CHECK` and `EXCLUDE` constraints, index entries). `COMMIT` is one log record and one fsync as before, and a crash before it leaves nothing of the transaction. On the VM under a 30 GB cap, one transaction loading 1, 5 and 20 million 100-byte rows peaked at 556, 594 and 684 MB by `COPY` (before: 557 MB, 2.0 GB and 7.8 GB) and at 383, 386 and 542 MB by `INSERT ... SELECT` (before: 485 MB, 2.1 GB and 7.9 GB; the same statements outside a transaction: 379, 462 and 929 MB); `CREATE TABLE AS` of 1, 5 and 10 million rows at 378, 378 and 377 MB (before: 580 MB, 2.5 GB and 4.8 GB); an `UPDATE` of every row of 1 and 5 million at 385 and 611 MB (before: 690 MB and 2.5 GB). Merged one by one in memory still, as before: the commit of a transaction that changed a row another session's commit changed too (it fails), or that altered a sequence another session's commit altered too; and a table with an `EXCLUDE` constraint is read whole for each part of its rows checked.
- **Results,** which are sent whole (above).
- **`chronos <db> verify`** (and `VERIFY DATABASE`) isn't bounded yet: it peaked at about 1.7 GB on a database of 646,000 pages (1.2 GB on disk, 6.2 GB decoded).

`examples/big.rs` builds an 811 MB database and queries it with a 32 MB cache: the process stayed under 200 MB throughout (see [BENCHMARKS.md](https://github.com/Abhishekxdg/chronosdb/blob/main/BENCHMARKS.md#12-a-database-bigger-than-its-cache)).

## Metrics

What the database has done since it opened: each kind of operation's count and latency (p50, p95, p99, max), and counters.

- **In SQL:** `SHOW METRICS;` (also in the shell).
- **For Prometheus:** `GET /metrics` on `chronos serve`, with the server's token (the guest may read them) or the admin token (agents' tokens get 403). Latencies are a summary, `chronos_op_seconds{op="...",quantile="..."}`.
- **Operations timed:** `select`, `insert`, `update`, `delete`, `schema` (CREATE, ALTER, DROP), `other_sql`, `commit` (a transaction going into its world), `log_sync` (waiting for the log to reach disk), `fork`, `diff`, `merge`, `search`, `checkpoint`, `gc`, `simulate` (SIMULATE and REPLAY WORLD).
- **Counters:** `rows_written`, `errors`, `conflicts`, `agent_yield_ms` (time low-priority agents' statements gave way to the database's own users; see [agent limits](security.md)), `spilled_bytes`, `cache_hits`, `cache_misses` (since opening); `worlds`, `pages_bytes`, `log_bytes`, `history_bytes`, `cache_bytes`, `uptime_seconds` (now); `disk_bytes` and `disk_limit_bytes` (the folder as its cap counts it, and the cap; see [limits the operator sets](#limits-the-operator-sets)); `last_statement_unix_ms`, when a client's last statement or operation started or ended (ms since 1970, 0 for none since opening; background checkpoints and cleanups don't count).
- **Idle:** `GET /metrics` also gives `chronos_connections_open`, the HTTP and Postgres connections open now, not counting the one asking. With `chronos_last_statement_unix_ms`, a host tells an idle database from these two without reading its logs.
- **Cost:** a few atomic adds per operation, no locks. Quantiles are within 6% of the true value.
- **Search graphs:** `chronos_search_graphs_building` is 1 while an HNSW graph builds in the background (searches scan until it's done).

## Backups

- **While it runs:** `BACKUP DATABASE TO '/backups/db-1'` in SQL, the `backup` HTTP op, or `chronos mydb backup /backups/db-1` from the shell when nothing else has the folder open.
  - The backup is a database folder, consistent as of the moment it started: every write before then, none after. Worlds come with their unmerged work.
  - History comes too: a restored database can time travel as far back as the original could.
  - It goes into a new or empty folder and is renamed into place at the end, so a failed backup never looks like one.
  - Writers keep going while it copies. Checkpoints wait for it, and writers wait once the log passes 256 MB, so very large copies can stall writes.
  - Each backup is a full copy.
- **Restoring:** `chronos restore /backups/db-1 newdb` copies a backup into a new folder, opens it and verifies it (`Db::from_backup`). The backup is only read. Any closed database folder works as a backup too. With an [anchor](#rollback-detection-anchor), open the restored folder once with `--anchor-reset`.
- **Verifying:** `VERIFY DATABASE`, `verify` in the shell, or the `verify` HTTP op. It reads every page any world, reader or kept history needs, checks each against its hash, and checks every log record against its checksum. An error names what is missing or corrupt.
- **Who may:** backups and verification are admin-only for agents, and MCP's safe mode doesn't allow them.
- **Cloud-backed:** the bucket is the backup. It holds the database as of the last checkpoint, so versioned buckets give you point-in-time copies. `BACKUP` refuses a cloud-backed database.

## Migrations

`chronos mydb migrate migrations/` (or `Db::migrate` in Rust) applies a folder's `.sql` files in name order, such as `0001_users.sql` then `0002_orders.sql`:

- **Whole or not at all:** each file runs in one transaction. A file that fails rolls back completely, and the ones before it stay applied. Files must not contain `BEGIN`, `COMMIT` or `ROLLBACK`.
- **Once each:** the same transaction records the file and its checksum, so the next run skips it. A file edited after it was applied is refused: add a new one instead.
- **Two runs at once** can't both apply a file. The second one's commit conflicts and rolls back.
- **Try it on a world first:** `chronos -b staging mydb migrate migrations/` applies the files to world `staging` only. Check it, then merge.

## Tenants

Give each tenant its own folder and its own `chronos serve` process:

```bash
CHRONOS_TOKEN=$ACME_TOKEN CHRONOS_KEY_FILE=/secrets/acme  chronos serve /data/acme  --listen 10.0.0.5:7071 --pg 10.0.0.5:5441 --tls-cert acme.pem --tls-key acme.key
CHRONOS_TOKEN=$GLOBEX_TOKEN CHRONOS_KEY_FILE=/secrets/globex chronos serve /data/globex --listen 10.0.0.5:7072 --pg 10.0.0.5:5442 --tls-cert globex.pem --tls-key globex.key
```

- **Nothing is shared:** files, memory, caches, crashes, the server token, agents and their tokens, and, with a key each, encryption.
- **An agent's token only works in its own tenant:** agents are rows in that tenant's database.
- **One process can't starve another's cache.** Each opens its folder alone; a second process on the same folder is refused.
- Routing many tenants through one process (one port, a tenant per path or database name) comes with the hosted service. Until then, a reverse proxy maps names to ports.

## Limits worth knowing

- **One process per folder.** Share it through `chronos serve`.
- **x86-64 CPUs from about 2009 on:** the x86-64 binaries use POPCNT and SSE4.2 (x86-64-v2), which every x86 server CPU since then has. Build from source with `RUSTFLAGS="-C target-cpu=x86-64"` for anything older.
- **Size limits:** a single row value is limited by memory, and large blobs belong in object storage with a key in the row.
- **Checkpoint pauses:** the background checkpoint briefly pauses writers while it takes a consistent snapshot.
- **Vector search:** `CHRONOS_SCAN_WORDS` (default 8388608) is how much a vector search scans exactly before it walks an HNSW graph instead: higher for recall, lower for speed on big tables (see [search](search.md)).
- **Page cache:** 256 MB of decoded pages per process by default; `CHRONOS_CACHE_MB=n` sets it. Raise it when the rows queries read (vectors, big JSON documents) outgrow it: a page read from disk is decompressed and hash-checked on every read that misses (`cache_misses`). See [databases bigger than memory](#databases-bigger-than-memory).
