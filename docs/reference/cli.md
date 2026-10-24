---
title: CLI and settings
---

The `chronos` binary: its commands and flags, the environment variables it reads, the interactive shell, and the settings SQL can change. Running a server in production: [operations](../operations.md). The HTTP API: [HTTP API](../http-api.md).

```sh
chronos mydb                                  # interactive shell (the folder is created if missing)
chronos mydb get users 1                      # one command, then exit
CHRONOS_TOKEN=$(openssl rand -hex 24) chronos serve mydb   # HTTP on 127.0.0.1:7070, Postgres on 127.0.0.1:5433
chronos mcp mydb                              # MCP server for AI agents, over stdio
```

## Commands

| Command | Does |
|---|---|
| `chronos <folder>` | the interactive shell, when stdin is a terminal (see [shell commands](#shell-commands)) |
| `chronos <folder> <command...>` | runs one shell command or SQL statement and exits |
| `chronos <folder> < script.txt` | runs stdin, one command per line (SQL may span lines and ends with `;`) |
| `chronos serve <folder>` | the HTTP JSON API and the Postgres protocol, until SIGTERM or Ctrl-C |
| `chronos mcp <folder>` | an MCP server over stdio |
| `chronos import <folder> postgres://user:password@host:port/database [--dry-run]` | a whole Postgres database, moved in one command (see [SQL](../sql.md#moving-from-postgres)) |
| `chronos import <folder> <table> <data.csv> --schema <schema.csv>` | a copy of one Postgres table (see [SQL](../sql.md)) |
| `chronos restore <backup> <new folder>` | starts a database from a backup folder, and verifies it |
| `chronos init [--no-open]` | in a project folder: signs in to [Chronos Cloud](../cloud.md), finds or makes the project's database, and writes `.chronos/project.json`, `.mcp.json`, `.env` and the `CLAUDE.md` block |
| `chronos cloud login\|logout\|list\|usage\|production on\|off` | Chronos Cloud's account and the folder's project (see [Chronos Cloud](../cloud.md)) |
| `chronos keygen` | prints a new encryption key (see [encryption](../operations.md#encryption-at-rest)) |
| `chronos -V`, `chronos --version` | prints `chronos <version>` |
| `chronos`, `chronos -h`, `chronos --help` | usage and the shell's help, exit code 2 |

- **Exit codes:** 0 on success; 1 when a command, statement or line of a script failed (a script runs on past a failed line), or the database can't be opened; 2 for bad usage or an unknown option.
- **Unknown options:** any leftover `--x` fails with `chronos: unknown option --x; see chronos --help` (exit 2), rather than opening a database called `--x`.
- **One folder, several processes:** a folder is open in one process at a time. When `chronos <folder>`, `chronos mcp` or `chronos import` finds another chronos process (a shell, `mcp`, or `serve`) holding it, it runs as a session inside that process instead. `serve` and `--remote` never do: they must own the folder.
- **Stopping:** `serve`, `mcp` and the interactive shell close the database cleanly on SIGTERM or Ctrl-C (Ctrl-C at the shell prompt only clears the line being typed).

## Flags

Flags may go anywhere on the line. A flag that takes a value and has none prints the usage (exit 2).

| Flag | Default | Used by | Effect |
|---|---|---|---|
| `-b`, `--branch <name>` | `main` | shell, `mcp`, `import` | the world to start on (or import into); it must exist |
| `--remote <url>` | none | all that open a folder | back the folder with object storage: `s3://bucket/prefix` (needs a build with `--features s3`) or `file:///dir`; see [cloud storage](../operations.md#cloud-storage---features-s3) |
| `--takeover` | off | with `--remote` | replace a dead owner's lease on the prefix |
| `--listen <addr>` | `127.0.0.1:7070` | `serve` | the HTTP API's address |
| `--pg <addr>\|off` | `127.0.0.1:5433` | `serve` | the Postgres protocol's address; `off` turns it off |
| `--token <T>` | `CHRONOS_TOKEN` | `serve` | the server's bearer token (HTTP) and password (Postgres), at least 24 characters (`openssl rand -hex 24`; `change-me` is refused): a client that sends it is the agent `guest` (since 0.4; the database's own user with `--unsafe`). Needed for any address that isn't loopback: without one, `serve` refuses to start. Prefer the variable: a flag shows in `ps` |
| `--admin-token <A>` | `CHRONOS_ADMIN_TOKEN` | `serve` | the token that is the database's own user over the network; at least 24 characters. Keep it away from agents |
| `--safe` | off | `serve` | every client that isn't an agent or the admin token is the agent `guest`, loopback without a token too; see [security](../security.md) |
| `--unsafe` | off | `serve` | the server token is the database's own user, as before 0.4; not with `--safe`. `serve` warns at start on an address other machines can reach unless `protect_main` is on |
| `--tls-cert <file>` | `CHRONOS_TLS_CERT` | `serve` | PEM certificate; with `--tls-key`, HTTPS only and TLS on the Postgres port. One without the other fails: `TLS needs both --tls-cert and --tls-key`. Needed for any address that isn't loopback, unless `--insecure-no-tls` |
| `--tls-key <file>` | `CHRONOS_TLS_KEY` | `serve` | PEM private key |
| `--insecure-no-tls` | off | `serve` | listen on an address that isn't loopback without TLS (with a warning): only where something else encrypts the connection |
| `--allow-plaintext-password` | off | `import ... postgres://` | send the password to a source that asks for it in the clear or as MD5 over a connection that isn't TLS with the server's certificate checked for its name (`sslmode=verify-full`; refused otherwise) |
| `--allow-merge` | off | `mcp` | lets the MCP client merge; without it agents change only worlds they fork, and a person merges |
| `--agent <name>` | none | `mcp` | the MCP client acts as that agent (it must exist), with its permissions and limits |
| `--schema <schema.csv>` | none | `import` | the table's columns; required |
| `--dry-run` | off | `import ... postgres://` | read Postgres and try every definition on an empty in-memory database: the report, and nothing written |

An empty `--token`, `--admin-token`, `--tls-cert` or `--tls-key` (or its variable) counts as not given. The encryption key is never a flag, so it can't show in the process list: see `CHRONOS_KEY` below.

## Environment variables

| Variable | Default | Effect |
|---|---|---|
| `CHRONOS_TOKEN` | none | `serve`'s `--token` (the guest's) |
| `CHRONOS_ADMIN_TOKEN` | none | `serve`'s `--admin-token` (the database's own user) |
| `CHRONOS_TLS_CERT`, `CHRONOS_TLS_KEY` | none | `serve`'s `--tls-cert` and `--tls-key` |
| `CHRONOS_KEY` | none | the database key, 64 hex digits (from `chronos keygen`). A new folder is encrypted with it; an encrypted one needs it to open. Also needed to `restore` an encrypted backup |
| `CHRONOS_KEY_FILE` | none | a file holding the key; used when `CHRONOS_KEY` is unset or empty |
| `CHRONOS_WORK_MEM` | `256MB` | memory a query may hold in one grouping or sort before it spills to disk: `512kB`, `64MB`, `2GB` (units `b`, `k`/`kb`, `m`/`mb`, `g`/`gb`, any case) or plain bytes. An agent's `max_memory_mb` lowers it to a quarter of that for its queries. See [memory and spilling to disk](../operations.md#memory-and-spilling-to-disk) |
| `CHRONOS_SPILL_DIR` | the system's temporary folder | where query spill files go (`sonos-spill-<pid>-<random>`, `0600`, unlinked as soon as they're opened, sealed with the database's key if it has one) |
| `CHRONOS_CACHE_MB` | `256` | the page cache: megabytes of decoded pages kept in memory, per process |
| `CHRONOS_THREADS` | the number of cores | threads queries may use together (the caller plus `CHRONOS_THREADS - 1` helpers, shared by all running queries) |
| `CHRONOS_SCAN_WORDS` | `8388608` | how much a vector search compares exactly before it walks an HNSW graph instead: higher for recall, lower for speed. See [search](../search.md) |
| `CHRONOS_VECTOR` | graphs on | `flat` turns HNSW graphs off (every vector search is exact), to compare |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `AWS_ENDPOINT`, `AWS_ALLOW_HTTP` | none | credentials and endpoint for `--remote s3://...`, read the usual AWS way (any `AWS_*` variable the S3 client knows). `AWS_ENDPOINT` for R2, MinIO, B2; `AWS_ALLOW_HTTP=true` for a local MinIO |
| `HOSTNAME` | `this machine` | names the owner in a `--remote` lease, shown to a second folder that is refused |
| `CHRONOS_CRASH_AT` | `0` | debug builds only (tests): aborts the process at the n-th crash point |

The values of `CHRONOS_WORK_MEM`, `CHRONOS_CACHE_MB`, `CHRONOS_THREADS`, `CHRONOS_SCAN_WORDS` and `CHRONOS_VECTOR` are read once, when first needed; a value that doesn't parse is ignored (the default applies).

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
