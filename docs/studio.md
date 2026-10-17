# Chronos Studio

A web UI for a Chronos database, built into the `chronos` binary: every install (install.sh, a release tarball) has it, with nothing else to install.

```bash
chronos studio mydb                # opens http://127.0.0.1:<free port>/#t=<session key> in your browser
chronos studio mydb --port 7071    # a fixed port
chronos studio mydb --no-open      # print the link, don't open a browser
chronos studio mydb --login        # sign in with a database role instead (see Signing in with roles)
```

It works beside a running `chronos serve`, `chronos mcp` or shell on the same folder: like a second shell, it connects to the process holding the folder over its local socket, and every call runs there. Ctrl-C stops the Studio (and closes the database, if the Studio opened it).

## What it does

Studio is built around worlds: see what's in one, and decide what to merge.

- **The window:** menus, a search that opens the command palette (⌘K), an icon rail of views, a sidebar (⌘B collapses it) with the folder, worlds, tables that open to their columns, a timeline and an outline, pill tabs, and a status bar.
- **Worlds and the worldline:** a tree of worlds as lineage in the sidebar, each with its own color, owner and change count. The worldline above the views draws every world as a lane off its parent at the moment it was forked (amber for main, the live data), and each merge into main rejoining it. Click a lane to switch world; drag across the strip to read the tables as they were. Fork, see details and metadata, save a checkpoint, or discard from the world menu.
- **Data:** a grid that pages through any table (only the visible rows are fetched) with filters, sorting and a read-only view **as of** any moment the history kept reaches (the As of picker and the worldline say how far back that is: `history_retention`, 1 hour for a new database; earlier moments are greyed out). Views and materialized views are listed with the tables and open read only. Double-click a cell (or F2) to edit it in place. Import rows from CSV, JSON or NDJSON (checked, previewed, all or nothing), and export what you see as CSV. Vectors show as a strip of bars and their size.
- **Inspector:** a row's fields, JSON pretty-printed and vectors drawn, with Find similar rows. Edit, delete, add rows. On main, a change asks first and offers to make it in a new world instead: fork, edit, review, merge.
- **Diagram:** a card per table with its columns and types, foreign keys drawn between the columns they join; drag, pan, zoom and fit.
- **SQL:** runs in the active world (several statements at once) with colouring, timing, rows affected, errors with their SQLSTATE, `EXPLAIN`, history (kept in this browser), and CSV downloads.
- **Search:** words (typos forgiven), exact filters and nearness to a row's vector in one query, fused into one ranking, beside a map of the table's vectors (their two main axes, coloured by a field with few values). Click a point or a hit to search near it.
- **Changes:** what the world changed since its fork, per table (rows deleted, updated, inserted, the tables that delete most first, deletes in red) and row by row: the deleted rows first, in red with every value they held, then the rest (only the changed columns, before → after; a whole row added as one line), paged. A merge whose policy asks for it (`confirm_deletes_over`) is approved by typing the number of rows it deletes; and what in the database reads the changed columns (views, merge checks, triggers, functions, keys, indexes), with what that can't see. Preview the merge, merge into the parent, discard, or undo the last merge.
- **History:** a world's events and checkpoints; browse its tables as they were, or restore the world to a moment or a checkpoint (confirmed; history keeps what it replaces).
- **Simulations:** fork N worlds from one, run the same SQL in each (`$1` its index, `$2` its seed), score them with a query and keep the best; open, review, replay or discard what's kept.
- **Import from Postgres (File → Import from Postgres…):** the import `chronos import postgres://...` runs (see [migrating-from-postgres.md](migrating-from-postgres.md)), into the open database, from a form: host, port, database, user, password (masked), sslmode (prefer by default), the schemas and `schema.table`s to bring over (all of them if left empty), a dry run, and the plaintext-password allowance. It runs on the server, off the request, as a job the page polls: tables done and rows as they come, a Cancel (between tables, or in the middle of a table's COPY, which leaves that table empty), then the importer's report (what came over, what in part, what not) and each table's rows. A database that already has tables gets the import in a new world, forked from the open one (named `import_<database>_<YYYYMMDD_HHMMSS>` in UTC, numbered on if that name is taken, or a name you give), to review in Changes and merge; you can import beside the tables instead. One import runs at a time.
- **Sessions (View → Sessions):** the Postgres port's connections, as `pg_stat_activity` shows them, refreshed every 2 seconds while the page is open: pid, user (agents marked), application, client, world, state, query, how long it has run and its transaction has been open, and, for a session waiting for a row, the session holding it (`pg_blocking_pids`) and the table. Transactions open over a minute are marked long, idle-in-transaction ones red. Cancel query and Terminate run `pg_cancel_backend` and `pg_terminate_backend` (the server's refusal shows if a check fails; Studio acts as the owner, an agent may do neither).
- **Agents, audit log and status:** agents' capabilities and quotas (a new agent's token is shown once; one made before Chronos 0.2.0 is marked until its first HTTP login gives it a SCRAM verifier, which its Postgres logins need on a server with a token), the audit log, and the database's version, size, encryption, storage and metrics.
- **Settings (⌘,):** theme, density, the sidebar and worldline, times in your zone or UTC, the session's key, history retention (how far back time travel, UNDO AGENT and UNDO MERGE, restores and the audit trail reach, and the disk it costs: live data, history, log and reclaimable, as `SHOW DISK` gives them; a shorter window is confirmed first), and an integrity check of the whole database.

Keys: ⌘K (Ctrl+K) opens the command palette (switch world, open a table, fork, review, merge, discard, sessions, import from Postgres, theme). ⌘1–⌘6 switch tabs (Data, Diagram, SQL, Changes, History, Search), ⌘B collapses the sidebar, ⌘, opens Settings, ⌘Enter runs SQL, J/K or the arrows move through rows, Enter opens a row, E edits it, F2 or a double-click edits a cell, / filters, Esc closes, ? lists them all. Light and dark themes follow the system, or pick one in Settings.

## Security

By default (without `--login`), the Studio is yours alone, on this machine. Any program on it, and any web page open in your browser, can reach 127.0.0.1. So:

- **A session key.** Each run makes a random 256-bit key. The link carries it after `#`, which browsers never send to a server, so it isn't in logs or a `Referer`. The page moves it into the tab's session storage and sends it as `X-Studio-Token` on every API call; the server compares it in constant time. Without it (or with a wrong one), every `/studio/api/` call is refused (403). The page's own files need no key.
- **Only its own address.** Every request must name `Host: 127.0.0.1:<port>` exactly (a web page's own name pointed at 127.0.0.1, DNS rebinding, is refused), and an `Origin`, when a browser sends one, must be `http://127.0.0.1:<port>` exactly (other pages are refused). API bodies must be `application/json`.
- **Strict headers.** Every response has `Cache-Control: no-store`, a Content-Security-Policy that allows only the Studio's own scripts, styles, fonts and API (no inline script, no other site), `X-Frame-Options: DENY` with `frame-ancestors 'none'`, `Referrer-Policy: no-referrer` and `X-Content-Type-Options: nosniff`.
- **Data is text.** Row values are shown as text (React escapes them), never as HTML.
- **Passwords for an import.** The Postgres password goes only in the body of the request that starts the import (over 127.0.0.1, with the key), is wiped from memory once the connection is made, and is never kept with the job, written to a file or the log, or sent back. A source that asks for it in the clear or as MD5 is refused unless the connection is TLS with the server's certificate checked for its name (`sslmode` `verify-full`) or the form allows it, as `chronos import` without `--allow-plaintext-password`.
- **No paths.** Studio acts as the database's owner (it's your folder), but its server reads and writes no files by path: backups (a path) are refused; run `chronos mydb backup <folder>` instead. Import reads a file you pick in the browser, and exports are saved by the browser. The browser is opened on a small file that only you can read, which forwards to the link, so the key never appears in the process list.
- **Listens on 127.0.0.1 only.** `chronos serve`'s own API is unchanged: it still refuses web pages.

Keep the link to yourself: anyone with it, on this machine, can use the database as you.

## Signing in with roles

`--login` makes the Studio a sign-in page, as phpMyAdmin is for MySQL: people sign in with a database role and its password, and the Studio acts as that role, with its privileges. There is no key in the link.

```bash
chronos mydb "create role admin login superuser password 'a long passphrase';"   # or CREATE ROLE over psql
chronos studio mydb --login                       # http://127.0.0.1:<free port>/: sign in as admin
chronos studio mydb --login --listen 0.0.0.0:8443 --host db.example.com \
    --tls-cert cert.pem --tls-key key.pem         # https://db.example.com:8443/, for the team
```

- **Who may sign in:** a role made with `CREATE ROLE ... LOGIN PASSWORD '...'` (a SCRAM-SHA-256 verifier or an md5 hash, as Postgres keeps them), checked as the Postgres port checks a password, then `LOGIN`, `CONNECT` on `main` and `CONNECTION LIMIT`. A wrong name, a wrong password, a role without a password or past its `VALID UNTIL`, an agent's name and the guest all get the same `wrong role name or password`, in about the same time, so the page doesn't tell which names are roles. With the right password, `NOLOGIN` is refused as Postgres words it (`role "x" is not permitted to log in`). After 10 failed sign-ins from an address it waits before its next try (1 s, doubling to 60 s; `CHRONOS_LOGIN_FREE_TRIES`), as the Postgres port does.
- **What a role may do:** SQL runs as the role (`current_user`), so GRANTs, row-level security and what `SET ROLE` may become apply exactly as over psql; a table it may not read is `42501`. It sees the status, the worlds (without their change counts) and the tables (rows counted as it: none shown where it may not read). Everything else the Studio offers (row edits in the grid, fork, merge, discard, restore, checkpoints, history, diffs, agents, the audit log, sessions, imports, simulations, the integrity check) needs a superuser role: the page hides those for other roles, and the server refuses them (`403 <op> needs a superuser role`). Writes through SQL still work as the role's privileges allow. A superuser role may do all the key's Studio does.
- **Sessions:** a random 256-bit id kept in the Studio's memory, in a cookie (`chronos_studio`, `HttpOnly; SameSite=Strict; Path=/`, and `Secure` over HTTPS). It ends when you sign out, after 30 minutes unused or 12 hours in all, when the Studio stops, and at the next call after its role is dropped (even if a role of that name is made again), made `NOLOGIN` or past its `VALID UNTIL`; whether the role is a superuser is read again on each call. The signed-in role shows in the title bar, with Sign out.
- **Where it listens:** 127.0.0.1 with `--port` by default, or `--listen ADDR:PORT`. Anywhere but loopback needs `--tls-cert` and `--tls-key` (PEM files, as `chronos serve` takes them) unless `--insecure-no-tls` says something else encrypts the connection (it warns). The Host must be one of the Studio's names: each `--host NAME[:PORT]` (the port listened on unless given; repeat it for several names) and the address it listens on; an `Origin` must be that same name over the same scheme. API bodies must be JSON. `--listen`, `--host` and TLS without `--login` are refused: the key's Studio is for 127.0.0.1 only.
- **Beside a running server:** with `chronos serve` holding the folder, sign-ins and calls go to it over the folder's socket, and it checks the role on each call.

## Big data

The grid never loads a whole table: it counts the rows, then fetches the pages in view as you scroll (a jump deep into 250,000 rows reads one page with `OFFSET`). A SQL table is shown in key order by default. The diff is paged 50 rows at a time, and a merge preview shows at most 200 rows, conflicts first, with how many rows had each outcome. A SQL query in the console returns its whole result: add a `LIMIT` to big ones.

## Developing the Studio

The UI is a small React + TypeScript app, open source under the MIT license in [`studio/`](https://github.com/Abhishekxdg/chronosdb/tree/main/studio) of the chronosdb repo. Each release of `chronos` has that release's Studio built into it.

To work on the UI with live reload, run a Studio from any installed `chronos` for the API, then Vite's dev server, which proxies `/studio/api` to it (with the Host and Origin it expects):

```bash
chronos studio mydb --no-open --port 7071                  # prints http://127.0.0.1:7071/#t=<key>
cd studio && STUDIO=http://127.0.0.1:7071 npm run dev    # then open http://localhost:5173/#t=<key>
```

The API is `POST /studio/api/<op>` with a JSON body: the ops of the HTTP API (see [http-api.md](http-api.md)), plus `tables` (a world's tables, row counts, and whether each has a SQL schema; its views and materialized views too, with `kind`), `status` (with `history`: `retention`, history_retention in ms, and `from`, the earliest moment time travel can read: 0 while the database is younger than that), `sessions` (the Postgres port's connections with what each waits for), and `import_postgres` / `import_status` / `import_cancel` (an import from Postgres as a job: `import_postgres` takes `host`, `port`, `database`, `user`, `password`, `sslmode`, `include`, `dry_run`, `allow_plaintext_password`, `world`, `new_world` and `same_world`, and returns its `job`, `world` and whether it `forked`), `whoami` (`{"mode": "key" | "login", "user": null | {"name", "superuser"}}`, answered without a key or session too), and with `--login`, `login` (`{"user", "password"}`: sets the session cookie) and `logout`. These are the Studio's own: `chronos serve`'s HTTP API doesn't have them. `worlds` also gives each world's number of changed rows.
