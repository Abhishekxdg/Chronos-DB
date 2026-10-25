# Quickstart

> **Beta.** Chronos 0.2 is a public beta for developers. It is not yet used in production anywhere, and it is not Postgres, though it speaks Postgres's protocol and much of its SQL: see [where it stands](postgres-compatibility.md#where-it-stands-beta) before you move real data.

## Install

```bash
# a prebuilt binary for macOS or Linux (x86_64 or arm64), into ~/.local/bin
curl -fsSL https://github.com/Abhishekxdg/chronosdb/releases/latest/download/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"   # if ~/.local/bin isn't on your PATH yet (a fresh Ubuntu's isn't)
chronos --version
```

The examples below use `psql`; on a machine without it: `sudo apt-get install postgresql-client` (Debian, Ubuntu) or `brew install libpq` (macOS).

The script picks the build for your OS and CPU, checks its SHA-256 against the release's `SHA256SUMS`, and installs it for your user; it never runs anything as root. `CHRONOS_INSTALL_DIR=<folder>` installs into another folder you can write (for `/usr/local/bin`, download the script and run it with `sudo` yourself), `CHRONOS_VERSION=v0.4.2` picks a release, and `CHRONOS_VERIFY=1` also checks the build's signature with [cosign](https://docs.sigstore.dev/cosign/system_config/installation/): proof Chronos DB's release workflow built it. The Linux builds are static (musl), so they run on any distribution.

## Already run Postgres? Start beside it

Your data stays in your Postgres. Chronos copies the tables agents work on into a folder, agents fork worlds of it
at once, and a merge into main is written back to your Postgres in one checked transaction (a row changed there
meanwhile comes back as a conflict, and nothing is written):

```bash
export CHRONOS_SOURCE_URL='postgres://agent_writer:...@staging-db:5432/crm?sslmode=verify-full'
chronos attach ./crm --tables public.leads,public.accounts   # or --schema app
claude mcp add chronos -- chronos mcp ./crm                  # agents get worlds now (or: chronos serve ./crm)
```

The URL is read from the environment (or a file only you can read), never from the command line, and isn't written
to the folder. What attach doesn't do yet, before you rely on it:

- **Each table needs a primary key**, and no generated column or row-level security; partitioned and inheriting
  tables aren't attached.
- **At most 10,000 changed rows per merge** (merge a world a table at a time past that).
- **No refresh:** rows no merge touches stay as of the attach; attach again into a new folder to pick up the rest.
- **Your schema doesn't change through Chronos:** change tables in Postgres, then attach again.
- **For staging copies first.** It's Stage 1: [the guide](guides/attach-postgres.md) has the details, the limits and
  what happens at a merge.

Moving the data into Chronos instead, or starting from nothing, is the rest of this page.

## Start a server and connect with psql

```bash
chronos serve mydb               # makes the folder mydb if it's new
```

```
chronos: a new database: main is protected (protect_main): its rows change only by merging a world, even with the server's token. ...
chronos: serving on http://127.0.0.1:7070 (POST /v1/<op>; GET /v1/health)
chronos: postgres protocol on 127.0.0.1:5433 (psql postgres://127.0.0.1:5433/main)
```

`chronos serve` listens on 127.0.0.1 only: the Postgres protocol on port 5433 (not 5432, so it doesn't collide with a Postgres you already run) and the HTTP API on 7070. On loopback without a token no password is asked. In another terminal:

```bash
psql postgres://127.0.0.1:5433/main
```

```sql
create table todos (id bigserial primary key, title text not null, done boolean not null default false);
create world first;              -- main's rows change by merging a world: do the work in one
use world first;
insert into todos (title) values ('try chronos'), ('read the docs');
update todos set done = true where id = 1;
diff world first;                -- what it changed
use world main;
merge world first;
select * from todos order by id;
```

```
 id |     title     | done
----+---------------+------
  1 | try chronos   | t
  2 | read the docs | f
(2 rows)
```

The database name is the **world**: `main` is the database itself, and every world you fork is another name (see [concepts](concepts.md)). Stop the server with Ctrl-C or SIGTERM; it closes the database cleanly.

**Main is protected** in a database `chronos serve` makes: its rows change only by merging a world, for every token, the server's included, so a token an agent finds in a `.env` file can't change it with one `DELETE`. Tables, views and other definitions still go straight in, and `RESTORE` and `UNDO` still work. An application that writes `main` itself (the next section) needs it off: run `chronos mydb` on the database's machine (it joins the running server) and `alter system set protect_main = off;`, or make the folder with `CHRONOS_PROTECT_MAIN=off chronos serve mydb`. No client of the server, and no agent, can turn it off.

## Connect an application

Point the application's `DATABASE_URL` (or its driver's connection settings) at the server:

```bash
export DATABASE_URL="postgres://app@127.0.0.1:5433/main"
```

Postgres drivers and ORMs connect as they do to Postgres; whether everything they send works depends on the SQL they send. Which have been tested, and how far: [Clients](postgres-compatibility.md#clients). For a server other machines reach, a token of at least 24 characters and TLS are required (`chronos serve` refuses to start on another address without them). The server token is the password, with any user name, and lets a client in as the guest: it reads, forks and changes its own worlds, and can't write `main` or merge. The person or application that owns the data takes a second token, the admin token:

```bash
export CHRONOS_TOKEN=$(openssl rand -hex 24)         # serve reads both from the environment (not flags, which ps shows)
export CHRONOS_ADMIN_TOKEN=$(openssl rand -hex 24)   # the database's own user: keep it away from agents
chronos serve mydb --pg 0.0.0.0:5433 --tls-cert cert.pem --tls-key key.pem
export DATABASE_URL="postgres://app:${CHRONOS_ADMIN_TOKEN}@db-host:5433/main?sslmode=require"   # your application
psql "postgres://agent:${CHRONOS_TOKEN}@db-host:5433/main?sslmode=require"                      # a client with the server token: the guest
```

Without any token, anyone who reaches the loopback port can log in under any name, as a superuser. Before you expose a server, read [security](security.md): TLS (`--tls-cert`, `--tls-key`), agents with their own tokens, roles with passwords. Upgrading from 0.3, where the server token was the database's own user: [the migration note](security.md#upgrading-from-03-the-server-token).

Moving an existing Postgres database in: [Migrating from Postgres](migrating-from-postgres.md).

## Five minutes in the shell

```
$ chronos mydb
chronos:main> put users 1 {"name": "Ada", "role": "admin"}
chronos:main> put users 2 {"name": "Linus", "role": "user"}
chronos:main> describe users
users (2 rows)
  name  string  e.g. "Ada"
  role  string  e.g. "admin"
chronos:main> fork agent-7                       # instant private copy
chronos:agent-7> put users 1 {"name": "Ada L.", "role": "admin"}
chronos:agent-7> diff
1 change on agent-7
~ users/1  name: "Ada" -> "Ada L."
chronos:agent-7> merge
merged 1 change from agent-7 into main; now on main
chronos:main> find users where role = admin match ada
```

`help` lists every command. `chronos mydb <command>` runs one command and exits, and `chronos mydb < script.txt` runs a file.

## From TypeScript or Python (the HTTP API)

With `chronos serve mydb` running (HTTP on 127.0.0.1:7070), copy the client you need. Each is one file with no dependencies.

```ts
// clients/typescript/chronos.ts
import { Chronos } from "./chronos";

const db = new Chronos("http://127.0.0.1:7070");
const agent = await db.fork("agent-7");
await agent.put("users", "1", { name: "Ada L.", role: "admin" });
console.log(await agent.diff());
await agent.merge(); // throws ChronosError 409 on conflicts, or if a crash lost writes
```

```python
# clients/python/chronos.py
from chronos import Chronos

agent = Chronos("http://127.0.0.1:7070").fork("agent-7")
agent.put("users", "1", {"name": "Ada L.", "role": "admin"})
agent.merge()
```

## From Claude Code (MCP)

```bash
claude mcp add chronos -- chronos mcp /path/to/mydb   # a folder beside your Postgres works the same (see above)
```

The agent gets `describe`, `find`, `sql`, `get`, `put`, `delete`, `fork`, `set_meta`, `branches`, `diff` (rows, or with `as_sql` the SQL statements that make the change), `history`, `merge_preview` (what a merge would do, row by row and why), `checkpoint` and `rollback` (name a moment of its world and go back to it), `simulate` and `replay`, and `discard`, plus instructions for the safe workflow: describe, fork, edit, diff, then a person merges. Every tool and its parameters: [MCP reference](reference/mcp.md).

By default (safe mode) the agent changes only worlds it forked, and approving is left to a person: `merge` (with `resolve`, `by_columns`, per-row decisions in `rows`, and `only_tables`, `only_keys` or `into` to merge part of a world or into another world), `restore` and `undo_merge` appear only with `--allow-merge`, or for an agent (`--agent NAME`) with those rights.

## From Rust

```rust
use chronos::{Db, Query, Resolve};

let db = Db::open("mydb")?;
db.fork("main", "agent-7")?;
let v = db.put("agent-7", "users/1", r#"{"name":"Ada L."}"#)?;
db.merge_at("agent-7", Resolve::Fail, Some(v))?;
```

## One process per folder

A database folder is opened by one process at a time: the shell, `chronos serve`, `chronos mcp` or your program. Close one before opening another, or run `chronos serve` and point everything at it.

Next: the [five-minute trial](https://github.com/Abhishekxdg/chronosdb/tree/main/examples/trial), an agent on a small CRM whose mistakes are caught before they land, then [concepts](concepts.md).
