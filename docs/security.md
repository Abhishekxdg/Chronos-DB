---
title: Security and auth
---

Chronos DB has three ways in (Postgres, HTTP, MCP), one identity model (the database's own user, agents, the guest) and a few server-wide limits. This page says what each protects and what it doesn't yet.

The short version: **agents can't write `main`, and a person approves merges.** An agent works in worlds it forks; someone with the right to merge reads the diff and merges it. Everything below is how that's enforced, and where it isn't yet.

## Who can do what

| Who | How they're identified | Reads | Writes | Merges into `main` | Agents, settings, backups |
|---|---|---|---|---|---|
| **The database's own user** (a person) | the shell; a loopback client when the server has no token; the server token (`--token`); the admin token (`--admin-token`); `chronos mcp --allow-merge` | everything | everything | yes | yes |
| **An agent**, default rights | its own token; `chronos mcp --agent NAME` | every world | only worlds it forked | no | no |
| **An agent** with more rights | its own token | every world | what its `can` list allows | with `merge_own` (its worlds) or `merge` (any) | with `admin` |
| **The guest** (`chronos serve --safe`) | any HTTP or Postgres client without an agent's token or the admin token | every world | only worlds `guest` forked (shared by every guest) | no | no |
| **`chronos mcp`**, default safe mode | the process on stdio | every world | only worlds forked in this MCP session | no | no |
| **Someone on the network** | nothing | nothing on a non-loopback address: a token is required (HTTP `401`, Postgres `28P01`) | nothing | no | no |
| **Anything on the same machine** | nothing | with no server token, loopback clients are the database's own user (the guest with `--safe`), browser pages included (see [known limitations](#known-limitations-dont-expose-to-untrusted-networks-yet)) | same | same | same |
| **Processes of the same OS user** | the folder and its Unix socket | everything: the files, and the socket `chronos` uses to share an open folder | everything | yes | yes |

- **Reads aren't restricted.** Anyone who may read reads every world, `main` and the past included. There are no per-table, per-row or per-world read rights.
- **Agents are rows in the database,** not OS or Postgres users. A tenant's agents only exist in that tenant's folder (see [operations](operations.md#tenants)).
- **Safe mode and agents are guardrails for tool calls,** not a sandbox. An agent that can also run shell commands as the OS user that owns the folder can run `chronos <folder> merge <world>` itself. Run agents as a different OS user, or somewhere they can't reach the folder (see [agents in containers](#agents-in-containers)).

## Authentication

### Postgres (`chronos serve`, port 5433)

The user name chooses how the client proves who it is. An agent proves with SCRAM-SHA-256 that it knows its token, against a verifier of it that `CREATE AGENT` keeps beside the token's hash, so the token never crosses the wire. An agent made by an older Chronos, which kept only the hash, gets its verifier the first time it uses its token over the HTTP API; until then, on a server with `--token`, it is asked for SCRAM as every name is (so the exchange doesn't show it is an agent) and can't pass it. On loopback without `--token` it may instead send its token in the clear (`AuthenticationCleartextPassword`) once, which keeps a verifier too; from then on it uses SCRAM. A role made with `CREATE ROLE ... PASSWORD` proves it knows its password with SCRAM-SHA-256 (MD5 if it is kept as an md5 hash), even on loopback without a token. That guards only that role's own name, though: **without `--token`, loopback is trust for every other name**, and such a login is a superuser that may `SET SESSION AUTHORIZATION` to any role. Role passwords protect something only on a server with `--token`. Any other name proves with SCRAM-SHA-256 that it knows the server's (or admin) token, which never crosses the wire; over TLS the proof is bound to the server's certificate (SCRAM-SHA-256-PLUS), so `channel_binding=require` in libpq stops a server in the middle. See [Postgres compatibility](postgres-compatibility.md#connecting).

| Server started with | Client connects as | Gets |
|---|---|---|
| no `--token` (loopback only) | any user but a role with a password, no password asked | the database's own user, or the role |
| no `--token` | `user=<agent>`, `password=<its token>` | that agent |
| `--token T` | any user but a role or an agent, `password=T` | the database's own user |
| `--token T` | `user=<agent>`, `password=<its token>` | that agent |
| any | `user=<role>`, `password=<its password>` | that role, with its privileges |
| `--token T`, or an agent user | a wrong or missing password | refused: `28P01 password authentication failed for user "..."` (an agent made by an older Chronos, on loopback without `--token`, before its first login: `28P01 wrong password: use the server's token, or the agent's`) |
| any | from an address with more than 10 wrong passwords lately | refused at once until it has waited: `28P01 too many failed logins from this address: try again in N s` |
| `--token T` | a role without a password | refused: `28P01`, as Postgres refuses it |
| `--safe` | anyone who isn't an agent or the admin token (the server token included) | the guest |
| `--safe --admin-token A` | any user, `password=A` | the database's own user |

```bash
psql "postgres://anyone:$CHRONOS_TOKEN@db.internal:5433/main"      # the server token
psql "postgres://bot:chronos_4f…@db.internal:5433/agent_7"         # agent bot, on its world agent_7
```

- **With `--admin-token` and no `--token`,** every connection is asked for a password (it may be the admin token), and one that is neither the admin token nor an agent's just makes the client the guest.
- **Agents and the guest never read the passwords roles keep** (`pg_authid.rolpassword` and `pg_shadow.passwd` are 42501 to them, as to a role that isn't a superuser), so they can't answer a role's MD5 challenge with its hash.
- **A disabled or dropped agent** can't log in (`28P01`, after the same SCRAM exchange as a wrong token). An open session of an agent that's disabled is refused from its next statement.
- **An agent's name takes only its token**, not the server's or admin token (a role's name takes only its password). An agent with a role of its name logs in as the agent; the role's password doesn't log that name in.
- **Which names exist doesn't show before logging in.** With `--token`, every name gets SCRAM with the same iteration count and a salt of its own, the same each time: a role's or agent's verifier's salt, or for any other name one made from the name and a secret the server draws at start, as Postgres's mock authentication does. A login that can't succeed runs the whole exchange and gets `28P01`, and every SCRAM login takes its last step in the same time, so how long a login takes doesn't tell either. See [Postgres compatibility](postgres-compatibility.md#connecting) for what still shows.
- **The database name** is the world to start on (`main`, a world's name or ID, or `name@when` to read the past). An unknown one is `3D000`, and so is a session's transaction world (`_tx_…`): it is that session's own, on this port, over the HTTP API (`404 no world`) and for `USE WORLD` alike.
- **Failed logins slow down, per client address,** on this port and the HTTP API together: after 10 wrong passwords or tokens, the address waits 1 s before its next try, then 2 s, 4 s, up to 60 s; a try during the wait is refused at once (here `28P01 too many failed logins from this address`, over HTTP `429`), the right password too. The count is forgotten 15 minutes after the address's last failure (a login that succeeds doesn't reset it, or one good credential would let a client guess others at full speed), and lives in memory only. `CHRONOS_LOGIN_FREE_TRIES` changes the 10. Clients behind one proxy share its address.
- **TLS:** with `--tls-cert` and `--tls-key`, a connection that doesn't start TLS is refused (`28000 this server requires TLS`). Without them, a client asking for TLS is told no and carries on in plain text, which is why a server listening on anything but loopback needs TLS (or `--insecure-no-tls`). See [transport](#transport-tls).
- **Cancel requests** name a connection by its backend key, whose secret comes from the system's random number generator, so a cancel can't be forged by guessing it.
- **psql's catalog queries** (`\d`, `\dt`, `\l`), which this port answers itself, get the same checks as any statement first (an agent's rights, a role's privileges).

### HTTP (`chronos serve`, port 7070)

Clients send `Authorization: Bearer <token>` (the word `Bearer`, one space, the token).

| Request | Gets |
|---|---|
| `GET /v1/health` | always answered, no auth |
| no header, server has no `--token` (loopback only) | the database's own user; the guest with `--safe` |
| no header, server has `--token` | `401 missing token` |
| the server token | the database's own user; the guest with `--safe` |
| the admin token (`--admin-token` or `CHRONOS_ADMIN_TOKEN`) | the database's own user |
| an enabled agent's token | that agent |
| any other token (a typo, a disabled or dropped agent's), even with no server token | `401 wrong token` |
| any token, from an address that sent more than 10 wrong ones lately (see [Postgres](#postgres-chronos-serve-port-5433)) | `429 too many wrong tokens from this address` until it has waited |
| an agent (or the guest) doing what it may not | `403` |
| `GET /metrics` with an agent's token | `403`; it needs the server token if the server has one |

In safe mode, the guest can read `/metrics`, as its `read` right already gives it `SHOW METRICS`. Everything else is `POST /v1/<op>`; see [HTTP API](http-api.md#auth-and-exposure). Every response says `X-Content-Type-Options: nosniff` and `Cache-Control: no-store`, and over TLS `Strict-Transport-Security: max-age=31536000`. A request naming a session's transaction world (`_tx_…`) as its `branch`, `from`, `into`, `world` or the like is `404 no world`, an agent's `sql`, `simulate` and `replay` included.

### Tokens and flags

| Flag | Environment | What it is |
|---|---|---|
| `--listen 127.0.0.1:7070` | | HTTP address (the default is loopback) |
| `--pg 127.0.0.1:5433` or `--pg off` | | Postgres address (the default is loopback) |
| `--token T` | `CHRONOS_TOKEN` | the server token, at least 24 characters. Any address that isn't loopback refuses to start without one: `refusing to listen on … without a token` |
| `--safe` | | clients that aren't agents are the guest |
| `--admin-token A` | `CHRONOS_ADMIN_TOKEN` | with `--safe`: the database's own user; at least 24 characters too |
| `--tls-cert`, `--tls-key` | `CHRONOS_TLS_CERT`, `CHRONOS_TLS_KEY` | TLS on both ports; both or neither. Any address that isn't loopback refuses to start without them: `refusing to listen on … without TLS` |
| `--insecure-no-tls` | | listen on an address that isn't loopback without TLS anyway, with a warning at start: only where something else encrypts the connection (a VPN, a TLS proxy in front, a container port published only on `127.0.0.1`) |
| | `CHRONOS_LOGIN_FREE_TRIES` | wrong passwords or tokens an address gets before it must wait (10) |

- **Loopback** means every address the name resolves to is a loopback one (`127.x.x.x`, `::1`, `localhost`). Give numeric addresses to be sure.
- **Pass tokens in the environment, not on the command line:** a token passed as `--token` shows in the process list (`ps`) to every user of the machine. `CHRONOS_TOKEN=$(openssl rand -hex 24) chronos serve ...`, or from a secrets file your service manager reads.
- **The server and admin tokens** are whatever strings you choose, kept only in the process; the server refuses to start with one shorter than 24 characters, or with `change-me` (the placeholder older examples used). Make them with `openssl rand -hex 24`. Changing one takes a restart.

### MCP (`chronos mcp <folder>`)

`chronos mcp` runs over stdio as the OS user that starts it, with the folder open (or shared from the process that has it open, over the socket below). There's no token: whoever starts the process decides who it is. `chronos serve` also serves the MCP tools over HTTP (`POST /mcp`), and there a token decides: it acts only as the agent whose token is sent (the guest under `--safe`), never as the database's own user (see [MCP over HTTP](reference/mcp.md#over-http-chronos-serve)).

| Started as | Acts as | May |
|---|---|---|
| `chronos mcp mydb` | safe mode (the default) | read every world, fork, and change only worlds forked in this session. `merge`, `restore` and `undo_merge` aren't listed. SQL that forks, merges, restores, drops or switches worlds, or changes settings, agents or backups, is refused |
| `chronos mcp mydb --agent bot` | agent `bot` | what `bot` may do: tools it can't use aren't listed, its forks stay its own across sessions, its quotas apply, and what it does is in the audit trail |
| `chronos mcp mydb --allow-merge` | the database's own user | everything |
| `POST /mcp` on `chronos serve`, with an agent's token | that agent | as `--agent` with its name; the server's and admin tokens get `403` |

- **`--agent` takes a name, not a token:** anyone who can run `chronos mcp` on the folder can act as any agent. It names who does what; the OS decides who may start it. A disabled or unknown agent is refused at start.
- **Safe mode without `--agent`** leaves no audit trail: its writes aren't attributed to anyone, like the database's own user's. Use `--agent` to get the audit trail and quotas.

### Studio (`chronos studio <folder>`)

By default the Studio listens on 127.0.0.1 only and acts as the database's own user, for whoever holds the link it prints (a random 256-bit key after `#`); requests must name its exact Host and Origin. With `--login`, there is no key: people sign in with a database role and its password, and the Studio acts as that role.

| Signed in as | May |
|---|---|
| a superuser role | everything the database's own user may in the Studio (but `backup`, a path, is refused to everyone) |
| any other role with `LOGIN` and a password | `sql` as that role (its GRANTs, row-level security, `SET ROLE` limits, exactly as over the Postgres port); `status`, the worlds (without their change counts), the tables (rows counted as the role). Anything else: `403 <op> needs a superuser role` |

- **Every wrong sign-in looks alike:** a name that is no role, a role without a password or past its `VALID UNTIL`, an agent's name, the guest, or a wrong password all get `401 wrong role name or password`, after the same hashing (a mock salt for names that aren't roles). With the right password, `NOLOGIN` and a missing `CONNECT` are refused as Postgres words them. Failed sign-ins count toward the same per-address wait as the other ports (`429` during it).
- **Sessions** are random 256-bit ids in memory (keyed by their SHA-256), in an `HttpOnly; SameSite=Strict` cookie, `Secure` over HTTPS; they end on sign-out, after 30 minutes unused or 12 hours, and at the next call after the role is dropped (even if made again under its name), made `NOLOGIN` or passes its `VALID UNTIL`. Whether it is a superuser is read again on every call. A restart signs everyone out.
- **Off loopback** (`--listen ADDR:PORT`) it needs `--tls-cert` and `--tls-key` unless `--insecure-no-tls`. The Host must be one of its names (`--host`, and the address it listens on) and the Origin, if any, that same name and scheme.
- Beside a running `chronos serve`, sign-ins and calls go over the folder's socket below to the server, which checks the role itself on each call.

### The folder's socket

The first long-lived `chronos` on a folder (`serve`, `mcp`, the interactive shell) also listens on a Unix socket, `chronos-<hash>.sock` in `$TMPDIR/chronos-<uid>/` (a folder only its user can open, mode `0700`, checked on use), and both ends check the other runs as the same OS user. Later `chronos` commands on the same folder run through it: a shell command there is the database's own user, and a `chronos mcp` there gets the same modes as above. Tokens and `--safe` don't apply to it: it's for the OS user who owns the folder.

## Agents

An agent has a name, an ID, a token, a list of what it may do (`can`) and quotas. Agents are rows on `main` (`_sonos_agents`), so they survive restarts, but no SQL statement or row command can read or write that table, the database's own user's included (`42501`).

### Creating and managing them

In SQL (psql, the `sql` HTTP op, or the `chronos mydb` shell), as the database's own user or an agent with `admin`:

An agent that merges into `main` (`write_main` and `merge_own`) should keep to a merge policy, and the one to start from is `max_deletes = 0`: nothing it deletes reaches `main` without a person (see [merge policies](guides/merge-policies.md#start-here-no-delete-merges-on-its-own)).

```sql
create agent bot with (can = 'read,fork,write_own', max_worlds = 10, world_ttl = '1 day');
-- name | id | token      the token is shown once: keep it
alter agent bot set (writes_per_minute = 600, max_query_ms = 30000, max_concurrent = 4, max_memory_mb = 512);
alter agent bot set (disabled = true);    -- its token stops working; false turns it back on
show agents;                              -- name, id, can, quotas, world_ttl, created, disabled, limits
drop agent bot;                           -- its token stops working; its worlds stay, still its own
```

Over HTTP (needs the server token, the admin token, or an agent with `admin`):

```bash
curl -s -H "Authorization: Bearer $CHRONOS_TOKEN" localhost:7070/v1/create_agent \
  -d '{"name":"bot","can":["read","fork","write_own"],"max_worlds":10,"world_ttl":86400000}'
# {"agent":{...},"token":"chronos_4f…"}
curl -s -H "Authorization: Bearer $CHRONOS_TOKEN" localhost:7070/v1/alter_agent -d '{"name":"bot","disabled":true}'
curl -s -H "Authorization: Bearer $CHRONOS_TOKEN" localhost:7070/v1/agents -d '{}'
curl -s -H "Authorization: Bearer $CHRONOS_TOKEN" localhost:7070/v1/drop_agent -d '{"name":"bot"}'
```

- **Names:** letters, digits, `_` and `-`. `guest` and `system` are the database's own (`22023`). A dropped agent's name can't be reused while it still owns worlds (`22023`): merge or drop them first, so a new agent never inherits them.
- **Times:** in SQL, `world_ttl` and `max_query_ms` take an interval (`'1 day'`) or milliseconds; over HTTP, milliseconds.
- **Tokens:** `chronos_` and 64 hex digits (32 bytes from `/dev/urandom`). Only the token's blake3 hash is stored, with a SCRAM-SHA-256 verifier of it for Postgres logins (as a role's password is kept), and an agent's ID (`a` and 16 hex digits) is derived from it. There's no rotation: disable the agent and make a new one.
- **Anyone who may read** can run `SHOW AGENTS` and `SHOW AUDIT` in SQL (they're reads: names, rights, quotas and actions, never tokens). The HTTP `agents` and `audit` ops need `admin`.

### What it may do

`can` is a list of these. A new agent gets `read, fork, write_own`: it works in its own worlds, and a person reviews and merges.

| Capability | Allows |
|---|---|
| `read` | reading any world, `main` and the past included: queries, `get`, `find`, `diff`, history, merge dry runs, `SHOW AGENTS`, `SHOW AUDIT`, `SHOW METRICS` |
| `fork` | forking `main`; forking another world also needs the right to change it (a fork pins its parent) |
| `write_own` | changing worlds it forked: rows, tables, discarding them, their metadata and named checkpoints |
| `write` | changing any world but `main`, other agents' included, and discarding them |
| `write_main` | changing `main` directly |
| `merge_own` | merging worlds it forked into their parent, if it may change that parent (`main` needs `write_main` too) |
| `merge` | merging any world |
| `restore` | `RESTORE WORLD` (with the right to change that world); simulating from a past moment. With `merge` too: `UNDO MERGE` and `UNDO AGENT` |
| `outbox` | working `main`'s outbox: `CLAIM OUTBOX`, `ACK OUTBOX` and `RETRY OUTBOX` (give it to the workers that send effects; an agent that may merge doesn't get it from `write_main`, or it could take back what its merges queued) |
| `admin` | everything: managing agents, `ALTER SYSTEM`, `BACKUP`, `VERIFY`, pinning worlds or changing their TTL, forking the past, and the HTTP ops for these |

- **A world an agent forks is its own** (`owner` in `SHOW WORLDS` and HTTP `world`). The owner and expiry are metadata only the database sets: no one can change them with `set_meta` or `ALTER WORLD`.
- **Checked on every surface:** HTTP ops, each SQL statement over Postgres or the `sql` op (transactions included), and each MCP tool call run with `--agent`.
- **`admin` is total,** including making more admins.

### Quotas and limits

All are off (`0`) by default, for agents and for the guest.

| Setting | Limits | Over it | HTTP |
|---|---|---|---|
| `max_worlds` | its live worlds at once | refused, `42501` | 403 |
| `max_changes` | rows each of its worlds may change since its fork, across restarts. Every row counts: statements, transactions (at `COMMIT`), restores, worlds merged into it | refused, `42501` | 403 |
| `writes_per_minute` | statements, batches and API writes, as a token bucket that holds a minute's worth | refused, `53400` | 429 |
| `world_ttl` | how long its worlds live | discarded by the background worker (every 30 seconds), logged as `system` | |
| `max_query_ms` | how long one of its SQL statements runs. It may lower its `statement_timeout`, never raise it past this | stopped, `57014` | 400, with `"code": "57014"` |
| `max_concurrent` | its statements and HTTP requests running at once | refused, `53300` | 429 |
| `max_memory_mb` | rows one of its statements holds (joined rows, a join's other side, its result); its sorts, groups and joins spill at a quarter of it | stopped, `53200` | 413 |
| `max_idle_ms` | how long a Postgres connection of its sits idle inside a transaction. It may lower its `idle_in_transaction_session_timeout`, never raise it past this | connection dropped (FATAL `25P03`), its transaction rolled back and its locks let go | |

- **`max_changes` counts a transaction at `COMMIT`,** so one huge transaction uses memory before it's refused.
- **`max_memory_mb` counts rows,** not every structure: a `GROUP BY`'s groups and a window's rows aren't counted (see [operations](operations.md#memory-and-spilling-to-disk)).
- **The rate limiter and `max_concurrent`** are counted in memory: a restart resets them.
- **Agents hold connections while their model thinks.** A request over HTTP or MCP holds nothing between calls; an agent on `psql` or a Postgres driver holds its connection, and a transaction it has open, across every model call, which can take seconds. Point agents at [HTTP](http-api.md) or [MCP](reference/mcp.md) where you can. Where they use the Postgres protocol, set `max_idle_ms` (`ALTER AGENT bot SET (max_idle_ms = '30s')`) so one that stops mid-transaction lets its rows go; `idle_session_timeout` (`SET`, or `options=-c idle_session_timeout=5min` in its connection string) also drops a connection idle outside a transaction, freeing its slot.
- **The guest has no quotas,** and all guests share one set of worlds. Bound what it can do with the server-wide limits below.

## Protecting `main`

- **Agents can't write `main`** unless they have `write_main`, and can't merge unless they have `merge_own` or `merge`. The guest and MCP safe mode never can.
- **The approval step is a person merging.** An agent forks, writes and tells you its world. You look, then merge or throw it away:

```sql
diff world agent_7;              -- every row it changed
merge world agent_7 dry run;     -- what a merge would do, conflicts included (agents may run this too)
merge world agent_7;             -- as the database's own user
drop world agent_7;              -- or not
```

- **Approving means merging,** as the database's own user or an agent allowed to; merges a policy refuses wait in `SHOW REVIEWS`. Approve the version you read (`MERGE WORLD w AT VERSION n`, HTTP `merge` with `version`, Studio does it for you): an agent that keeps writing to its world while you review doesn't get its later writes merged with it. `merge_own` with `write_main` lets an agent publish its own work to `main` with no one looking. The database's own user and admins can write `main` directly unless `protect_main` is on; `chronos serve` warns as it starts when other machines can reach it with such a token (no `--safe`, `protect_main` off).
- **MCP:** without flags, agents can't merge; `--allow-merge` gives the MCP session everything, not just merging. Prefer `--agent NAME` with `merge_own` (plus `write_main` to publish to `main`) when an agent should publish.
- **Crashes can't make a merge publish less than was written:** merges check the world's version, and worlds open during a crash are flagged until someone confirms them (see [concepts](concepts.md#versions-and-crashes)).

### Undoing

Within the history window (1 hour by default for a new database, 30 days for one made by an older release):

```sql
undo agent bot since '-2 hours';              -- put back every row bot changed in the current world since then
undo agent bot since '-2 hours' skip changed; -- ...leaving rows someone changed after it
undo merge of world agent_7;                  -- put back a merge someone ran
restore world main to '-10 minutes';          -- the whole world as it was
checkpoint world agent_7 as 'before';         -- name a moment; later: restore world agent_7 to checkpoint 'before'
```

- **`UNDO AGENT`** puts back the agent's own writes, its transactions and its worlds it merged in itself; everyone else's changes stay. A row someone else changed afterwards is named and nothing changes, unless `SKIP CHANGED`. A merge a person ran of an agent's world is the person's: undo that with `UNDO MERGE`.
- **Agents' own rows** (`_sonos_agents`) are never put back from the past by `RESTORE`, `UNDO` or a fork of the past, so rights removed stay removed.
- **All of these are ordinary writes:** durable, in history, and themselves undoable. They need `restore` (and `merge`, for the undos) when an agent runs them.

## Server-wide limits

Set as the database's own user; kept in the folder across restarts. All off by default.

| Setting | Does | Over it |
|---|---|---|
| `alter system set statement_timeout = '30s'` | every SQL session starts with it | `57014`. **A session may `SET statement_timeout = 0`**, as in Postgres: only an agent's `max_query_ms` can't be raised |
| `alter system set max_worlds = 10000` | live worlds besides `main` (transactions don't count) | fork refused, `53400` (HTTP 429) |
| `alter system set world_idle_ttl = '7 days'` | worlds unused that long are discarded, unless pinned or forked from; logged as `system` | |
| `alter system set protect_main = on` | `main`'s rows change only by merging a world, for everyone, the database's own user and admins included (see [SQL](sql.md#limits-for-the-whole-database)) | `42501`, naming a row |
| `alter system set history_retention = '7 days'` | how far back time travel, undo and the audit trail reach (1 hour by default for a new database, 30 days for one made by an older release) | |

Set by the operator where the process starts, and not by anyone inside the database, its own user and admins included (see [operations](operations.md#limits-the-operator-sets)):

| Environment | Does | Over it |
|---|---|---|
| `CHRONOS_MAX_DISK_BYTES=536870912` | the most the folder may hold: pages, log, history, spills (queries' too with `CHRONOS_SPILL_DIR` inside the folder). Reads, `DELETE`, `DROP`, `TRUNCATE`, merges, discarding worlds and cleanup still work | write, fork, `CREATE DATABASE`, a world's `CHECKPOINT 'label'` or spill refused: `53100` (HTTP 507) |
| `CHRONOS_LIMITS=/etc/chronos/limits.toml` | ceilings `max_worlds`, `history_retention_ms`, `world_idle_ttl_ms`, `statement_timeout_ms`; a setting left at 0 (no limit) is the ceiling | `ALTER SYSTEM SET` past one: `42501` (HTTP 403), naming the limit. A session's `SET statement_timeout` above it, or to 0, is clamped to it |

Fixed and environment limits:

- **Memory:** `CHRONOS_WORK_MEM` (default `256MB`) per grouping, sort or join before it spills; `CHRONOS_CACHE_MB` (default 256) for the page cache. There's no cap on the whole process, and a query's result is held whole before it's sent.
- **HTTP:** request headers up to 64 KB (`431`), bodies up to 32 MB (`413`), sent with `Content-Length` (chunked bodies get `411`) and `Content-Type: application/json` (`415`); a request is checked (token, `Origin`, `Host`) from its headers before its body is read and must arrive within 2 minutes (`CHRONOS_REQUEST_TIMEOUT`, seconds); a connection idle for 60 seconds is closed. At most 1000 connections at once (`503`; `CHRONOS_MAX_CONNECTIONS`, lowered to what the process's open-file limit holds: about one connection per 8 descriptors after 256 for the server itself; the soft limit is raised to the hard one at start).
- **Postgres:** messages up to 64 MB (a bigger one is read and dropped, and answered `54000` with the connection kept; past 1 GB, as Postgres, `08P01` and closed), and under 10 kB until login, which must finish within 60 seconds (`CHRONOS_LOGIN_TIMEOUT`, seconds). At most 1000 connections at once (`53300` `sorry, too many clients already`; `CHRONOS_MAX_CONNECTIONS`, per listener, lowered to what the process's open-file limit holds, which `SHOW max_connections` says); no idle timeout after login unless the session sets `idle_in_transaction_session_timeout` (FATAL `25P03`) or `idle_session_timeout` (FATAL `57P05`), as in Postgres, except inside `COPY ... FROM STDIN`, which holds its world's writes while it reads: a client that sends no data for 60 seconds is dropped and its COPY undone (`CHRONOS_COPY_IDLE`, seconds).
- **Browsers:** HTTP refuses any request with an `Origin` header (`403`), and a server without a token answers only loopback `Host` names, so web pages can't reach it (no CSRF or DNS rebinding).
- **Simulations:** at most 1,048,576 worlds, and within `max_worlds`.
- **Functions and triggers** nest at most 40 deep (`54001`); `WITH RECURSIVE` stops at a round or row cap (`54001`); a regular expression that backtracks too long stops (`54001`).

## Transport (TLS)

```bash
CHRONOS_TOKEN=… chronos serve mydb --listen 10.0.0.5:7070 --pg 10.0.0.5:5433 --tls-cert cert.pem --tls-key key.pem
psql "postgres://app:$CHRONOS_TOKEN@10.0.0.5:5433/main?sslmode=verify-full&sslrootcert=ca.pem"
```

- **Both ports:** HTTP becomes HTTPS only, and Postgres refuses connections that don't use TLS. A Postgres client that sends anything between asking for TLS and the handshake is dropped.
- **Files:** PEM, the certificate chain first; the key as PKCS#8, PKCS#1 or SEC1. TLS 1.2 and 1.3 (rustls). A certificate change takes a restart.
- **No client certificates:** clients are identified by token only.
- **TLS protects tokens, it doesn't replace them.** Without TLS, the Postgres password and the HTTP bearer token cross the network in the clear: so `chronos serve` refuses to listen on anything but loopback without `--tls-cert` and `--tls-key`, unless `--insecure-no-tls` says something else encrypts the connection (a VPN, a TLS proxy in front, a container whose port is published only on the host's `127.0.0.1`), and then warns at start.
- **`chronos import postgres://...`** never sends a password in the clear or as MD5 to a server it hasn't checked: a source that asks for one (`password` or `md5` in its `pg_hba.conf`) is refused unless the connection is TLS with the server's certificate checked for its name (`sslmode=verify-full`; the default `prefer`, and `require`, encrypt without checking whose certificate it is, so anyone on the way could be the one asking, and `verify-ca` checks only that a trusted CA issued it, for any name) or `--allow-plaintext-password` is given. SCRAM-SHA-256, Postgres's default, never sends the password, so it goes over either. Connecting gives up after `connect_timeout` seconds (30 unless the URL says), and a source silent for 10 minutes is dropped.

### Server files

`COPY ... FROM '/path'` and `TO '/path'`, file_fdw's `filename`, `lo_import` and `lo_export` read and write the server's own files. They are off unless the server runs with `CHRONOS_ALLOW_SERVER_COPY=1`, and then go as Postgres guards them: COPY FROM a file takes the privileges of `pg_read_server_files`, COPY TO one those of `pg_write_server_files` (`42501 permission denied to COPY from a file`, with Postgres's DETAIL and HINT, before any file is opened or query run), `lo_import` and `lo_export` a superuser's, and making a file_fdw foreign table `pg_read_server_files` (here reading one does too, which Postgres doesn't ask). A login that is no role is the bootstrap superuser and may; an agent's session, the guest's included, never may, whatever its role. COPY TO a file counts as a write for agents and read-only snapshots.

See [operations](operations.md#tls).

## Storage

### Encryption at rest

```bash
chronos keygen                                        # prints a new key: 64 hex digits
CHRONOS_KEY_FILE=/run/secrets/chronos chronos serve mydb  # a new folder is encrypted from its first byte
```

- **What's encrypted** (ChaCha20-Poly1305): log records (each bound to its place in the log), the manifest and its history copies, every page, and every spill file (queries' and big writes'). Page names become keyed hashes. Tampered bytes are refused, never read as data.
- **What isn't:** the file names, sizes and timing of writes. Pages are encrypted deterministically so equal pages still dedupe: someone with the files can tell two pages are the same, not what's in them.
- **The key:** from `CHRONOS_KEY` or the file `CHRONOS_KEY_FILE` names; there's no flag, which would show in the process list. It's never stored: the folder keeps only a check value, so a wrong or missing key is refused on open. Lose it and the data is gone. `CHRONOS_KEY_FILE` with a `0600` file is better than `CHRONOS_KEY`, since a process's environment can be read by its OS user.
- **Chosen at creation:** an existing folder can't be encrypted in place, and there's no key rotation yet.

See [operations](operations.md#encryption-at-rest).

### Spill files

| Spilled by | Where | Named | Encrypted with the database |
|---|---|---|---|
| `GROUP BY`, `ORDER BY`, `DISTINCT` aggregates, hash joins, sorting an index's entries (`CREATE INDEX`, `ADD CONSTRAINT ... UNIQUE`) | `CHRONOS_SPILL_DIR` (default: the OS temporary folder) | `sonos-spill-<pid>-<random>`, deleted from the folder as soon as it's opened | yes |
| a big `INSERT`, `UPDATE`, `DELETE` or `COPY` | `<database>/spill` | `run-<pid>-<n>` | yes |

Spill files are created new (never reusing a file already there) and readable by their user only (`0600`). A query's spill file is unlinked at once, so nothing is left behind even after a crash; big writes' runs are removed when the statement ends, and the `spill` folder is emptied when the database opens.

### Files, backups and object storage

- **Files** are created with the process's default permissions (its `umask`); nothing sets them tighter. Keep the folder `0700`.
- **Backups** (`BACKUP DATABASE TO`, the `backup` HTTP op, `chronos mydb backup`) copy the files as they are: an encrypted database's backup is encrypted, and restoring it needs the key. `BACKUP` and `VERIFY` need `admin`, and `BACKUP` writes wherever the server process can.
- **Object storage** (`--features s3`): credentials come from the `AWS_*` environment variables and are never written to the folder. Uploads are the same bytes, so an encrypted database is encrypted in the bucket.
- **The one-process-per-folder lock and the bucket's `lease`** stop accidental second writers, and `--takeover` fences off a dead one. They aren't a security boundary: anyone who can write the bucket can take the lease.

## Audit trail

There's no dedicated security log. What's recorded:

- **`SHOW AUDIT [FOR AGENT name] [LIMIT n]`** (HTTP `audit`, admin only): what agents, the guest and `system` did, newest first: `at`, `agent`, `world`, `action` (`write`, `fork from …`, `merge into …`, `merge part into …`, `discard`, `metadata`) and `rows`. It's read from the log, so it survives restarts and lasts as long as history does.
- **`SHOW HISTORY`** (HTTP `history`): each world's forks, writes, commits, merges and discards, with who unrecorded.
- **Worlds:** `owner`, `created`, `expires`, `active` (last activity) and `parent` in `SHOW WORLDS` and the HTTP `world` op.
- **Agents:** `created` and `disabled` in `SHOW AGENTS`.
- **Metrics** (`SHOW METRICS`, `GET /metrics`): counts of operations, `errors` and `conflicts`, per database, not per client.

Not recorded: who, when it's the database's own user (the shell, the server or admin token, MCP without `--agent`): its changes are in `SHOW HISTORY` but not in the audit trail. Nor reads, logins, failed logins or client addresses; and creating, changing or dropping an agent is only a write to `main`, not an event of its own. Lowering `history_retention` drops the audit trail with the history, and a database in memory keeps none.

## A hardened deployment

1. Keep both ports on loopback unless something else must reach them; then give a numeric address, `CHRONOS_TOKEN` (`openssl rand -hex 24`, in the environment, not on the command line) and TLS. In a container, publish the ports on the host's loopback (`-p 127.0.0.1:5432:5432`) unless TLS is on.
2. Run with `--safe --admin-token`, so clients without an agent's token are the guest; keep the admin token for people.
   Without `--safe`, the server token is the database's own user: whoever holds it changes `main` directly, with no
   world and no review. `chronos serve` says so as it starts on an address other machines can reach.
3. **Keep the server and admin tokens out of agents' reach:** not in an agent's environment, its container, a `.env`
   file its tools can read, or a repository it works in. An agent connects with its own token only.
4. `ALTER SYSTEM SET protect_main = on`, so `main`'s rows change only by merging a world, for everyone, the admin
   included: a leaked admin token can then still merge, but not quietly rewrite `main` (see [protecting
   `main`](#protecting-main)). Turn it on after `chronos import`, which writes `main`'s rows; from then on, row
   edits go through a world.
5. Give every agent its own agent and token, with only the rights it needs (the default is right for most), and set `max_worlds`, `max_changes`, `writes_per_minute`, `world_ttl`, `max_query_ms`, `max_concurrent` and `max_memory_mb`.
6. Set `max_worlds` and `world_idle_ttl` for the whole database.
7. For MCP, give each agent its token and `chronos serve`'s `/mcp`, or run `chronos mcp <folder> --agent NAME` where only you start it; never `--allow-merge` for an agent you don't fully trust.
8. Run the server as its own OS user, with the folder `0700`, and agents as other OS users, or in containers that hold only their token (see [agents in containers](#agents-in-containers)).
9. Encrypt with `CHRONOS_KEY_FILE`, point `CHRONOS_SPILL_DIR` at a private directory on an encrypted disk, and keep the key somewhere other than the backups.
10. Put the server behind a proxy that limits connections and request rates if clients you don't control can reach it.

## Agents in containers

An agent's rights hold for what it asks the database, not for what it runs. An agent that can run shell commands as the OS user that owns the folder can run `chronos <folder> merge <world>` itself, or read the folder's files. A container keeps its shell away from the folder; its token then decides what it may do in the database. The two need each other: a container does nothing for your data if the credential inside it can write `main`, and a token does nothing if the agent can reach the folder.

- **Give the container the agent's token and nothing else.** Not the folder, not the folder's socket directory (`$TMPDIR/chronos-<uid>/`), not the server token or the admin token. Pass the token in the environment, and make one agent per container, with only the rights it needs (the default `read,fork,write_own` can't write `main` or merge) and its quotas (`max_worlds`, `world_ttl`, `writes_per_minute`, `max_query_ms`).
- **Connect with the token, not the folder.** Over MCP, point the client at `chronos serve`'s `/mcp` with `Authorization: Bearer $BOT_TOKEN` ([MCP over HTTP](reference/mcp.md#over-http-chronos-serve)); over Postgres, `postgres://bot:$BOT_TOKEN@<host>:5433/agent_7`; over HTTP, the same bearer token. `chronos mcp <folder>` opens the folder (or its socket) and takes an agent's name, not its token, so it doesn't belong in the container.
- **Let the container reach the server, and nothing else private.** A container on Docker's default bridge can still reach the host and its network. If you reject private and link-local addresses from containers (in the `DOCKER-USER` chain and on the agents' bridge, IPv6 too: `fc00::/7`, `fe80::/10`; link-local covers the cloud metadata address `169.254.169.254`), allow the Chronos server's address and port before those rules, and DNS if the container's resolver is on the host. Rules like these are wiped when the VM or host restarts, so put them where every start applies them again. Chronos doesn't install them.
- **Serve on an address the container can reach,** with the token and TLS (`serve` refuses an address that isn't loopback without them, unless `--insecure-no-tls` says a private network carries it): the bridge's address on the host, or a network shared with the agent's container. Run it with `--safe --admin-token`, so a client without an agent's token is the guest.
- **Reads aren't scoped yet.** The agent reads every world, `main`, its history and other agents' worlds included. Keep out of the database what the agent mustn't see, and let the network rules decide where it can send what it reads.

## Known limitations (don't expose to untrusted networks yet)

Found in a full code review on 2026-09-26 and still open on `main` (the review's other findings, such as browser access to the HTTP API, spill files, connection caps and uncapped string widths, are fixed). Until they are, treat any client that can send SQL, the guest included, as able to stop the server.

- **Deeply nested SQL can crash the process.** The SQL parser has no nesting limit, so a statement like `select ((((…))))` nested deeply enough overflows the stack, which aborts the whole server, not just the connection. (Regular expressions and text-search queries are now limited: 54001 / 2201B.)
- **`$N` parameter numbers in a statement aren't capped:** `$999999999` makes room for that many. (`repeat()`, `lpad()`, `rpad()` and `format()` widths past 1 GB are refused with 54000.)
- **A panic can leave internal locks poisoned.** Most counters and flags recover, but a panic while the database holds one of its internal read-write locks (the list of worlds, a world's state, checkpoints) makes later writes fail until a restart.
- **An agent made by an older Chronos needs one HTTP request first.** Its token was kept only as a hash, so no SCRAM exchange can check it: on a server with `--token` it is asked for SCRAM like every name and refused until it has used its token once over the HTTP API, which keeps a verifier of it. On loopback without `--token` it may send its token in the clear once instead, and is asked for a cleartext password until then, which tells it apart from others there. There is no token rotation: disable such an agent and make a new one to be rid of it. There is no `pg_hba.conf`.
- **Names that still show.** A role whose password is kept as an md5 hash is asked for MD5 (as under Postgres's `md5` method). The secret mock salts are made from is drawn at each start, so a name with no verifier gets a new salt after a restart where a role keeps its own; Postgres keeps its secret across restarts.
- **No request-rate limits** on either port beyond agents' own quotas and the wait after failed logins (connections are capped at 1000 per port).

What to do meanwhile:

- **Bind to loopback** (the default), or to a private network only your own services reach.
- **Put TLS in front,** built in or a proxy, whenever the traffic leaves the machine.
- **Use `--safe` and agent tokens,** so untrusted callers get the guest's or an agent's rights, never the database's own user's.
- **Set the limits:** agents' quotas, `max_worlds`, `world_idle_ttl`, `max_query_ms` for anything that runs queries you didn't write.

## Reporting a vulnerability

Don't open a public issue. Email the maintainer or use the repository's private advisory feature, as [SECURITY.md](https://github.com/Abhishekxdg/chronosdb/blob/main/SECURITY.md) describes; the aim is a reply within 3 working days. Data-loss and corruption bugs count as security problems here: report them privately too.
