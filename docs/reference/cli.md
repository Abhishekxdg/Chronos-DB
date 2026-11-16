---
title: CLI and settings
---

The `chronos` binary: its commands and flags, the environment variables it reads, the interactive shell, and the settings SQL can change. Running a server in production: [operations](../operations.md). The HTTP API: [HTTP API](../http-api.md).

```sh
chronos mydb                                  # interactive shell (the folder is created if missing)
chronos mydb get users 1                      # one command, then exit
CHRONOS_TOKEN=$(openssl rand -hex 24) chronos serve mydb   # HTTP on 127.0.0.1:7070, Postgres on 127.0.0.1:5433
chronos mcp mydb                              # MCP server for AI agents, over stdio
chronos studio mydb                           # a web UI for the database, on 127.0.0.1
CHRONOS_SOURCE_URL=postgres://... chronos attach crm --tables public.leads   # work beside a database you already run
```

## Commands

| Command | Does |
|---|---|
| `chronos <folder>` | the interactive shell, when stdin is a terminal (see [shell commands](#shell-commands)) |
| `chronos <folder> <command...>` | runs one shell command or SQL statement and exits |
| `chronos <folder> < script.txt` | runs stdin, one command per line (SQL may span lines and ends with `;`) |
| `chronos serve <folder>` | the HTTP JSON API and the Postgres protocol, until SIGTERM or Ctrl-C. On an attached folder, `--refresh-every` and `--follow` keep main up to date (see [attach, refresh and follow](#attach-refresh-and-follow)) |
| `chronos mcp <folder> [--allow-merge \| --agent NAME] [--unmask]` | an MCP server over stdio (see [MCP](mcp.md)) |
| `chronos studio <folder> [--port N] [--no-open]` | a web UI for the database on 127.0.0.1, beside a running `serve` too (see [Studio](../studio.md)) |
| `chronos studio <folder> --login [--port N \| --listen ADDR:PORT] [--host NAME[:PORT]]... [--tls-cert cert.pem --tls-key key.pem \| --insecure-no-tls] [--no-open]` | the web UI, signed in with a database role and its password, acting with its privileges; any address but loopback needs TLS |
| `chronos attach <new folder> (--tables a,b \| --schema s) [--lazy-tenant column] [--fields a,b] [--yes]` | works beside a database you already run (Postgres, MySQL, MariaDB or MongoDB, by the scheme of `CHRONOS_SOURCE_URL`): copies the tables named into a new folder; merges into main are written to that database first |
| `chronos refresh <attached folder> [--unfollow \| --tenant <value>]` | main takes the source's rows as they are now; `--unfollow` drops what `serve --follow` keeps; `--tenant` loads one tenant's rows |
| `chronos import <folder> postgres://user:password@host:port/database [--dry-run]` | a whole Postgres database, moved in one command (see [SQL](../sql.md#moving-from-postgres)) |
| `chronos import <folder> <table> <data.csv> --schema <schema.csv>` | a copy of one Postgres table (see [SQL](../sql.md)) |
| `chronos restore <backup> <new folder>` | starts a database from a backup folder (one `backup` made), and verifies it. An encrypted backup needs its key (`CHRONOS_KEY`) |
| `chronos init [--no-open]` | in a project folder: signs in to [Chronos Cloud](../cloud.md), finds or makes the project's database, and writes `.chronos/project.json`, `.mcp.json`, `.env` and the `CLAUDE.md` block |
| `chronos cloud login\|logout\|list\|usage\|production on\|off` | Chronos Cloud's account and the folder's project (see [Chronos Cloud](../cloud.md)) |
| `chronos keygen` | prints a new encryption key on stdout, 64 hex digits (see [encryption](../operations.md#encryption-at-rest)). `CHRONOS_KEY=<key> chronos <new folder>` makes an encrypted database |
| `chronos -V`, `chronos --version` | prints `chronos <version>` |
| `chronos`, `chronos -h`, `chronos --help` | usage and the shell's help, exit code 2 |

- **Exit codes:** 0 on success; 1 when a command, statement or line of a script failed (a script runs on past a failed line), or the database can't be opened; 2 for bad usage or an unknown option.
- **Unknown options:** any leftover `--x` fails with `chronos: unknown option --x; see chronos --help` (exit 2), rather than opening a database called `--x`.
- **Flags for another command** are refused (exit 2) with a line that names the command they are for: `--tables` and `--yes` (`attach`), `--unfollow` (`refresh`), `--refresh-every` and `--follow` (`serve`), `--login` and `--host` (`studio`), `--dry-run` and `--allow-plaintext-password` (`import ... postgres://`).
- **One folder, several processes:** a folder is open in one process at a time. When `chronos <folder>`, `chronos mcp`, `chronos studio` or `chronos import` finds another chronos process (a shell, `mcp`, or `serve`) holding it, it runs as a session inside that process instead. `serve` and `--remote` never do: they must own the folder.
- **Stopping:** `serve`, `mcp` and the interactive shell close the database cleanly on SIGTERM or Ctrl-C (Ctrl-C at the shell prompt only clears the line being typed).

## Flags

Flags may go anywhere on the line. A flag that takes a value and has none prints the usage (exit 2).

| Flag | Default | Used by | Effect |
|---|---|---|---|
| `-b`, `--branch <name>` | `main` | shell, `mcp`, `import` | the world to start on (or import into); it must exist |
| `--remote <url>` | none | all that open a folder | back the folder with object storage: `s3://bucket/prefix` or `gs://bucket/prefix` (both need a build with `--features s3`) or `file:///dir`; see [cloud storage](../operations.md#cloud-storage---features-s3) |
| `--takeover` | off | with `--remote` | replace a dead owner's lease on the prefix |
| `--anchor <file>` | `CHRONOS_ANCHOR` | all that open a folder | refuse the folder if its files were rolled back to an older copy; see [rollback detection](../operations.md#rollback-detection-anchor) |
| `--anchor-reset` | off (`CHRONOS_ANCHOR_RESET=1`) | with `--anchor` | accept the folder as it is and anchor it anew, after a restore from a backup |
| `--listen <addr>` | `127.0.0.1:7070` | `serve`, `studio --login` | the HTTP API's address. For `studio --login`: where the Studio listens (127.0.0.1 at `--port` unless given; not with `--port`) |
| `--pg <addr>\|off` | `127.0.0.1:5433` | `serve` | the Postgres protocol's address; `off` turns it off |
| `--token <T>` | `CHRONOS_TOKEN` | `serve` | the server's bearer token (HTTP) and password (Postgres), at least 24 characters (`openssl rand -hex 24`; `change-me` is refused): a client that sends it is the agent `guest` (since 0.4; the database's own user with `--unsafe`). Needed for any address that isn't loopback: without one, `serve` refuses to start. Prefer the variable: a flag shows in `ps` |
| `--admin-token <A>` | `CHRONOS_ADMIN_TOKEN` | `serve` | the token that is the database's own user over the network; at least 24 characters. Keep it away from agents |
| `--safe` | off | `serve` | every client that isn't an agent or the admin token is the agent `guest`, loopback without a token too; see [security](../security.md) |
| `--unsafe` | off | `serve` | the server token is the database's own user, as before 0.4; not with `--safe`. `serve` warns at start on an address other machines can reach unless `protect_main` is on |
| `--tls-cert <file>` | `CHRONOS_TLS_CERT` | `serve`, `studio --login` | PEM certificate; with `--tls-key`, HTTPS only and TLS on the Postgres port. One without the other fails: `TLS needs both --tls-cert and --tls-key`. Needed for any address that isn't loopback, unless `--insecure-no-tls` |
| `--tls-key <file>` | `CHRONOS_TLS_KEY` | `serve`, `studio --login` | PEM private key |
| `--studio-api` | off | `serve` | also serve the Studio's calls at `POST /studio/api/<op>`, to the admin token only; `serve` refuses to start with it and no admin token. See [Studio](../studio.md#behind-another-service-serve---studio-api) |
| `--refresh-every <time>` | off | `serve` on an attached folder | main takes the source's rows on a timer: `60s`, `5m`, `1h` (a plain number is seconds; at least 1 second). A refresh that fails is said on stderr and tried again at the next turn |
| `--follow` | off | `serve` on an attached folder | main takes the source's changes about a second after they commit. Beside Postgres through a logical replication slot (`wal_level = logical`); beside MongoDB through change streams (since 0.5.1). Not beside MySQL or MariaDB yet: use `--refresh-every` there |
| `--port <N>` | a free port | `studio` | the Studio's port on 127.0.0.1 |
| `--no-open` | off | `studio`, `init`, `cloud login` | don't open the browser |
| `--login` | off | `studio` | people sign in with a database role and its password; needed for `--listen`, `--host`, `--tls-cert` and `--tls-key` on `studio` |
| `--host <name[:port]>` | none | `studio --login` | a name the Studio answers to besides the address it listens on; may be repeated |
| `--insecure-no-tls` | off | `serve`, `studio --login` | listen on an address that isn't loopback without TLS (with a warning): only where something else encrypts the connection |
| `--allow-plaintext-password` | off | `import ... postgres://` | send the password to a source that asks for it in the clear or as MD5 over a connection that isn't TLS with the server's certificate checked for its name (`sslmode=verify-full`; refused otherwise) |
| `--allow-merge` | off | `mcp` | lets the MCP client merge; without it agents change only worlds they fork, and a person merges |
| `--agent <name>` | none | `mcp` | the MCP client acts as that agent (it must exist), with its permissions and limits |
| `--unmask` | off | `mcp` | masked columns read as their values; without it an MCP session reads them masked, `--allow-merge` included (an `--agent` session reads as that agent does) |
| `--tables <a,b>` | none | `attach` | the tables to copy, `schema.table` for Postgres; MySQL's tables or MongoDB's collections by name |
| `--schema <schema>` | none | `attach` | every table of a Postgres schema (Postgres only) |
| `--lazy-tenant <column>` | none | `attach` | tables with that column start empty; a tenant's rows are loaded at the first fork for it, or by `refresh --tenant` (Postgres only) |
| `--fields <a,b>` | none | `attach` | MongoDB only: top-level fields kept as columns of their own |
| `--yes` | off | `attach` | copy more than about 1,000,000 rows in all (the source's estimate); without it attach stops there and says so |
| `--source-file <file>` | `CHRONOS_SOURCE_FILE` | `attach`, `refresh`, and `serve` on an attached folder | a file holding the source's connection string, readable by its owner only |
| `--unfollow` | off | `refresh` | drop what `serve --follow` keeps: Postgres's replication slot (which holds its WAL until read), or the place in MongoDB's change streams |
| `--tenant <value>` | none | `refresh` | in a folder attached with `--lazy-tenant`: main takes that tenant's rows now |
| `--schema <schema.csv>` | none | `import` | the table's columns; required |
| `--dry-run` | off | `import ... postgres://` | read Postgres and try every definition on an empty in-memory database: the report, and nothing written |

An empty `--token`, `--admin-token`, `--anchor`, `--tls-cert` or `--tls-key` (or its variable) counts as not given. The encryption key is never a flag, so it can't show in the process list: see `CHRONOS_KEY` below.

## Attach, refresh and follow

```sh
export CHRONOS_SOURCE_URL='postgres://agent_writer:...@db:5432/crm?sslmode=verify-full'
chronos attach crm --tables public.leads,public.accounts    # or --schema app
chronos serve crm --follow --refresh-every 1h                # main follows the source; a whole refresh each hour
chronos refresh crm                                          # one refresh now (the folder must not be open in serve)
chronos refresh crm --unfollow                               # drop the replication slot when the folder is no longer followed
```

- **The source** is named by `CHRONOS_SOURCE_URL` (or the file `CHRONOS_SOURCE_FILE` or `--source-file` names), never on the command line, and is written nowhere. Its scheme chooses the database: `postgres://` (Postgres 12 or later), `mysql://` or `mariadb://` (MySQL 8.0 or later, MariaDB 10.5 or later), `mongodb://` or `mongodb+srv://` (MongoDB 5.0 or later, a replica set; needs a build with MongoDB, which the released binaries are). `serve`, `refresh` and merges into main need it too.
- **`attach` makes a new folder:** an existing one is refused. Beside MySQL, MariaDB and MongoDB it takes `--tables` only: `--schema` and `--lazy-tenant` are refused there.
- **`refresh` opens the folder itself,** so it fails while `chronos serve` has it open: use `POST /v1/refresh` with the admin token, or `serve --refresh-every`. It prints, per table, the rows inserted, updated and deleted.
- **What each source can do** besides the write-back and the refresh (`follow`, dry runs, checked reads and the rest) is what the HTTP API's [`attached`](../http-api.md#operations) answers as `capabilities`.

Guides: [beside Postgres](../guides/attach-postgres.md), [beside MySQL](../guides/attach-mysql.md), [beside MongoDB](../guides/attach-mongodb.md).

## Environment variables

| Variable | Default | Effect |
|---|---|---|
| `CHRONOS_TOKEN` | none | `serve`'s `--token` (the guest's) |
| `CHRONOS_ADMIN_TOKEN` | none | `serve`'s `--admin-token` (the database's own user) |
| `CHRONOS_TLS_CERT`, `CHRONOS_TLS_KEY` | none | `serve`'s `--tls-cert` and `--tls-key` |
| `CHRONOS_KEY` | none | the database key, 64 hex digits (from `chronos keygen`). A new folder is encrypted with it; an encrypted one needs it to open. Also needed to `restore` an encrypted backup |
| `CHRONOS_KEY_FILE` | none | a file holding the key; used when `CHRONOS_KEY` is unset or empty |
| `CHRONOS_ANCHOR`, `CHRONOS_ANCHOR_RESET` | none | `--anchor`'s file; `CHRONOS_ANCHOR_RESET=1` is `--anchor-reset` |
| `CHRONOS_PROTECT_MAIN` | on | `off`: a new folder `chronos serve` makes starts without `protect_main` (see [security](../security.md#protecting-main)) |
| `CHRONOS_SOURCE_URL` | none | an attached folder's source: a `postgres://`, `mysql://`, `mariadb://`, `mongodb://` or `mongodb+srv://` connection string. Read by `attach`, `refresh`, `serve` and every merge into main; written nowhere |
| `CHRONOS_SOURCE_FILE` | none | a file holding that connection string, readable by its owner only; used when `CHRONOS_SOURCE_URL` is unset or empty. `--source-file` sets it |
| `PGPASSWORD`, `MYSQL_PWD` | none | the source's password, when the connection string has none (Postgres, MySQL or MariaDB) |
| `CHRONOS_ATTACH_MAX_ROWS` | `50000` | the most rows one merge writes to an attached database, up to 1,000,000 |
| `CHRONOS_UNDO_WINDOW_DAYS` | `30` | how many days a merge written to an attached database can be undone by its id |
| `CHRONOS_WRITEBACK_RULES` | `writeback.json` in the folder | the file holding an attached folder's write-back rules |
| `CHRONOS_MAX_CONNECTIONS` | `1000` | client connections served at once, HTTP and Postgres together; lowered to what the process's open-file limit holds. `SHOW max_connections` says it |
| `CHRONOS_REQUEST_TIMEOUT` | `120` | seconds an HTTP request's headers and body may take to arrive |
| `CHRONOS_LOGIN_TIMEOUT` | `60` | seconds a Postgres client may take to log in |
| `CHRONOS_LOGIN_FREE_TRIES` | `10` | wrong passwords or tokens an address gets before it must wait (see [security](../security.md)) |
| `CHRONOS_COPY_IDLE` | `60` | seconds a `COPY ... FROM STDIN` waits for its client's next data before the connection is dropped |
| `CHRONOS_MAX_DISK_BYTES` | none | the most the folder may hold, in bytes; past it writes fail with 53100 (HTTP 507). See [operations](../operations.md#limits-the-operator-sets) |
| `CHRONOS_MAX_MEMORY_BYTES` | none | resident memory past which the process refuses new statements with 53200, in bytes; a statement already running is not stopped |
| `CHRONOS_LIMITS` | none | a file of ceilings on the database's own settings (`max_worlds`, `history_retention_ms`, `world_idle_ttl_ms`, `statement_timeout_ms`), one `key = integer` a line; `ALTER SYSTEM` can't go past them |
| `CHRONOS_ALLOW_SERVER_COPY` | off | `1`: `COPY` to or from a server file, file_fdw, `lo_import` and `lo_export` work, for the roles Postgres lets (never with `serve --studio-api`). See [security](../security.md#server-files) |
| `CHRONOS_WAL_SHIPPING`, `CHRONOS_WAL_SHIPPING_DELAY_MS` | none | override the `wal_shipping` and `wal_shipping_delay` settings [below](#database-settings) |
| `CHRONOS_WORK_MEM` | `256MB` | memory a query may hold in one grouping or sort before it spills to disk: `512kB`, `64MB`, `2GB` (units `b`, `k`/`kb`, `m`/`mb`, `g`/`gb`, any case) or plain bytes. An agent's `max_memory_mb` lowers it to a quarter of that for its queries. See [memory and spilling to disk](../operations.md#memory-and-spilling-to-disk) |
| `CHRONOS_QUERY_MEM` | a quarter of the machine's memory | working memory one statement may hold before it fails with 53200 (an agent's lower `max_memory_mb` wins); all statements together may hold twice it. `0` for no limit; units as for `CHRONOS_WORK_MEM`. See [memory and spilling to disk](../operations.md#memory-and-spilling-to-disk) |
| `CHRONOS_SPILL_DIR` | the system's temporary folder | where query spill files go (`sonos-spill-<pid>-<random>`, `0600`, unlinked as soon as they're opened, sealed with the database's key if it has one) |
| `CHRONOS_CACHE_MB` | `256` | the page cache: megabytes of decoded pages kept in memory, per process |
| `CHRONOS_THREADS` | the number of cores | threads queries may use together (the caller plus `CHRONOS_THREADS - 1` helpers, shared by all running queries) |
| `CHRONOS_SCAN_WORDS` | `8388608` | how much a vector search compares exactly before it walks an HNSW graph instead: higher for recall, lower for speed. See [search](../search.md) |
| `CHRONOS_VECTOR` | graphs on | `flat` turns HNSW graphs off (every vector search is exact), to compare |
| `CHRONOS_VECTOR_FLOATS` | `67108864` | numbers a vector column keeps in memory as floats (256 MB); above it they are kept as 8-bit numbers. `0` keeps 8-bit numbers for every column |
| `CHRONOS_RERANK` | `2` | candidates per row wanted that an index of 8-bit numbers proposes, ranked exactly from the rows; at least 1 |
| `CHRONOS_TSEARCH_DATA` | none | folders (separated by `:`) where text search dictionary files are looked for, before the working directory |
| `CHRONOS_PG_EXACT` | off | `1`: errors and notices say only what Postgres says (no hints on syntax errors, none of Chronos's own notices) |
| `CHRONOS_TRACE` | off | anything but empty, `0`, `off`, `false` or `no` turns tracing on |
| `TZDIR` | none | a time zone database folder, looked in before the system's (`/usr/share/zoneinfo` and the like) |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `AWS_ENDPOINT`, `AWS_ALLOW_HTTP` | none | credentials and endpoint for `--remote s3://...`, read the usual AWS way (any `AWS_*` variable the S3 client knows). `AWS_ENDPOINT` for R2, MinIO, B2; `AWS_ALLOW_HTTP=true` for a local MinIO |
| `CHRONOS_GCS_TOKEN_FILE` | none | for `--remote gs://...`: a file holding a bearer token that another process keeps fresh |
| `CHRONOS_CLOUD_API` | Chronos Cloud's | the API `chronos init` and `chronos cloud` talk to |
| `XDG_CONFIG_HOME`, `HOME` | `~/.config` | where `chronos init` and `chronos cloud` keep `chronos/cloud.toml` |
| `SSL_CERT_FILE` | the system's | a PEM bundle of CA certificates for `chronos init` and `chronos cloud`'s TLS |
| `HOSTNAME` | `this machine` | names the owner in a `--remote` lease, shown to a second folder that is refused |
| `CHRONOS_CRASH_AT`, `CHRONOS_ATTACH_CRASH`, `CHRONOS_FAIL_PAGE_WRITES` | none | debug builds only (tests): abort the process at a crash point, or fail page writes as a full disk would |

The values of `CHRONOS_WORK_MEM`, `CHRONOS_CACHE_MB`, `CHRONOS_THREADS`, `CHRONOS_SCAN_WORDS`, `CHRONOS_VECTOR`, `CHRONOS_VECTOR_FLOATS`, `CHRONOS_RERANK`, `CHRONOS_MAX_CONNECTIONS` and the timeouts are read once, when first needed; a value that doesn't parse is ignored (the default applies). `CHRONOS_MAX_DISK_BYTES`, `CHRONOS_MAX_MEMORY_BYTES` and `CHRONOS_LIMITS` are read when the folder opens, and one that doesn't parse stops it from opening.

## Shell commands

The shell runs SQL and its own commands. A line whose first word is `select`, `insert`, `update`, `delete`, `create`, `drop`, `alter`, `begin`, `commit`, `rollback`, `show`, `restore`, `with`, `explain`, `values`, `savepoint`, `release`, `set` or `simulate`, or that starts `merge branch`, `undo merge`, `backup database`, `verify database`, or `fork`/`use`/`switch`/`diff`/`merge`/`replay` `world`, is SQL: it may span lines and ends with `;`. Anything else is one of the commands below, one line each. Empty lines and lines starting `#` or `--` are skipped. `quit`, `exit`, `.quit` or `\q` ends the shell (so does Ctrl-D).

The prompt is `chronos:<world>> `, with ` (transaction)` while a transaction is open and `-` while a SQL statement continues.

| Command | Does |
|---|---|
| `help`, `?` | the command list |
| `get <table> <id>` | prints a row as JSON |
| `put <table> <id> <json>` | writes a row, e.g. `put users 1 {"name": "Ada"}` |
| `del <table> <id>` | deletes a row (an error if there is none) |
| `scan <table> [n]`, `ls <table> [n]` | lists rows, the first 20 unless `n` |
| `find <table> [where <f> = <v> [and ...]] [match <words>] [near <field> [numbers]] [limit n] [offset n]` | filters (a value is JSON if it parses, else text), text search, vector search (`near embedding [0.1, 0.2]`); best matches first, 20 unless `limit` |
| `tables` | tables and row counts |
| `describe [table]` | fields, example and allowed values, a sample row |
| `fork <name> [from <world>]` | a new world from the current one (or `<world>`), and switches to it |
| `use <world>` | switches world (a name) |
| `worlds` (or `branches`) | lists worlds: `*` marks the current one, and each fork shows its parent and change count |
| `diff [world]` | what changed since the fork: `+` added, `-` deleted, `~` changed (by field) |
| `merge [world] [ours\|theirs] [confirm]` | merges into the parent (the current world unless named); on conflict nothing is merged and the rows are listed, then `ours` keeps the world's values and `theirs` the parent's. `confirm` merges a world that was open in a crash, once you've checked its diff. Merging the world you're on moves you to its parent |
| `discard [world] [cascade]` | drops a world (the current one unless named); `cascade` drops its forks too |
| `undo [world]` | undoes the latest merge into the current world, or the merge of `world`: puts back the rows it changed, and refuses if any changed again since |
| `migrate <folder>` | applies the folder's new `.sql` files in name order, each in one transaction; applied ones are recorded and skipped |
| `checkpoint` | compacts the log (speeds up the next open). Not SQL's `CHECKPOINT WORLD` (see [worlds](worlds.md#checkpoint-world)) |
| `backup <folder>` | copies the database, while it runs, into a new folder |
| `verify` | reads every page and log record, checking each |
| `gc` | deletes pages nothing uses any more (it runs on its own too) |

- **Inside a transaction** (`begin;`), `get`, `put`, `del`, `scan` and `find` go to it; `fork`, `use`, `merge` and `discard` fail with `a transaction is open: commit; or rollback; first`.
- **Tables starting `_sonos`** are the database's own: `get`, `put`, `del`, `scan`, `ls`, `find` and `describe` refuse them (see `SHOW AGENTS`, `SHOW WORLDS`).
- **From the command line**, the same commands run one at a time: `chronos mydb backup /backups/mydb-1`, `chronos -b agent-7 mydb diff`.

## Database settings

Settings the database keeps in its folder, across restarts. Setting one needs an agent with `admin` (or no agent), and fails with 25001 inside a transaction.

| Setting | Default | Set with | Effect |
|---|---|---|---|
| `statement_timeout` | `0` (none) | `ALTER SYSTEM SET statement_timeout = 30000` or `'30s'` | the timeout every SQL session starts with; units as for `SET statement_timeout` below |
| `max_worlds` | `0` (no limit) | `ALTER SYSTEM SET max_worlds = 1000` | live worlds at most, besides main and transactions' own; a fork past it fails with 53400 |
| `world_idle_ttl` | `0` (never) | `ALTER SYSTEM SET world_idle_ttl = '7 days'` (or `0`) | the background worker discards worlds unused this long |
| `history_retention` | `1 hour` (a database made before 0.2.0: `30 days`) | `ALTER SYSTEM SET history_retention = '7 days'` (or `'0'`) | how far back time travel reaches; see [worlds](worlds.md#history-retention) |
| `synchronous_commit` | `full` | `ALTER SYSTEM SET synchronous_commit = full \| normal \| off` (`on` means `full`) | how far a commit pushes the log before it returns: `full` through the drive's cache to stable storage (`F_FULLFSYNC` on macOS, `fdatasync` elsewhere); `normal` a plain `fsync` (on macOS the drive may still cache it; elsewhere the same as `full`); `off` handed to the OS only, which survives a process crash but not a machine crash |
| `wal_shipping` | `async` (with a remote) | `ALTER SYSTEM SET wal_shipping = off \| async \| sync` (`on` means `sync`); `CHRONOS_WAL_SHIPPING` overrides | with [cloud storage](../operations.md#cloud-storage---features-s3), how the log reaches the bucket: `async` within `wal_shipping_delay` of being written, `sync` before a commit to `main` or a merge returns, `off` only at checkpoints. Between `async` and `sync` it changes at once; to or from `off`, at the next open |
| `wal_shipping_delay` | `1000` (ms) | `ALTER SYSTEM SET wal_shipping_delay = 250`; `CHRONOS_WAL_SHIPPING_DELAY_MS` overrides | in `async` mode, how long written log may wait before it is shipped: about what losing the machine loses |

`=` or `TO` both work. `SHOW <setting>` reads each one (`statement_timeout` shows the session's, below).

## Session settings

These last for the session (the Postgres connection, or the shell).

| Statement | Effect |
|---|---|
| `SET statement_timeout = 5000` / `'5s'` / `DEFAULT`, `RESET statement_timeout`, `SHOW statement_timeout` | cancels a statement that runs longer, with 57014. A number is ms; a string may end `us`, `ms`, `s`, `min`, `h` or `d`; `0` is none; range 0 to 2147483647 ms, else 22023. `DEFAULT`/`RESET` go back to the database's `statement_timeout`. An agent's `max_query_ms` caps it |
| `SET search_path TO a, "B", public` / `'a, public'` / `DEFAULT`, `SET SCHEMA 'a'`, `RESET search_path`, `SHOW search_path` | where unqualified names are looked up and created; default `"$user", public` |
| `SET hnsw.ef_search = 100` / `DEFAULT` | the HNSW graph's beam for this session's vector searches, 1 to 1000 (else 22023); `DEFAULT` lets the index choose. See [search](../search.md) |
| `SET SESSION ...`, `SET LOCAL ...` | the same as `SET ...` |
| any other `SET` | checked as Postgres checks it (22023 for a bad value, 42704 for a name Postgres 16 doesn't have) and kept for the session, so `SHOW` reads it back; many change nothing here (`application_name`, `extra_float_digits` and the like). A session `SET synchronous_commit` doesn't change how commits reach the disk: use `ALTER SYSTEM` |

- **Isolation levels:** `READ COMMITTED` (the default) and `REPEATABLE READ`, on `BEGIN`, `START TRANSACTION`, `SET TRANSACTION` or `default_transaction_isolation`; `SERIALIZABLE` is refused (0A000). See [Transactions](../sql.md#transactions).
- **Postgres's settings:** `SHOW`, `SET` and `pg_settings` know all of Postgres 16's settings, checked as Postgres checks them ([Postgres compatibility](../postgres-compatibility.md#known-gaps)). A Postgres client gets these parameters at connect: `server_version` 16.15, `server_encoding` and `client_encoding` UTF8, `DateStyle` ISO, MDY, `TimeZone` (UTC unless the client asks for another), `integer_datetimes` on, `standard_conforming_strings` on.
