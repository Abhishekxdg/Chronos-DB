# Chronos Cloud

> **Private alpha.** Chronos Cloud, at `chronos.2090.tech` (the dashboard) with its backend on a name Google gives, and built by [2090.tech](https://2090.tech), is invite only while it is built; the commands below are in the `chronos` binary now. The hosted platform itself is developed in a separate, private repository.

Chronos Cloud hosts a project's database, so an agent in Claude Code, Codex or Cursor (or in CI, or on a teammate's laptop) can use it without anyone running `chronos serve`. One command in the project folder connects it:

```bash
cd myapp
chronos init
```

```
chronos: to sign in to Chronos Cloud, open https://chronos.2090.tech/device and enter the code WDJB-MJHT
chronos: signed in as ada (key saved in /home/ada/.config/chronos/cloud.toml)
chronos: project myapp (prj_...), database db_... (running, asia-south1)
chronos: DATABASE_URL=postgres://app:****@db.chronos.2090.tech:5432/main?sslmode=require&options=endpoint%3Ddb_... (in .env)
chronos: MCP https://db.chronos.2090.tech/?project=prj_...
chronos: wrote .chronos/project.json, .mcp.json, .env, .gitignore, CLAUDE.md
Open Claude Code here and ask it to build.
```

Then open Claude Code in the folder and ask for what you want ("add user accounts and a billing page"). The agent forks a world, makes the change there, runs the app's tests against it, shows the diff, and merges.

## What `chronos init` does

1. **Signs in**, the first time: it prints a code to approve at chronos.2090.tech/device (signed in with an email code or GitHub) and opens the browser (`open` on macOS, `xdg-open` on Linux; elsewhere, or with `--no-open`, open the printed address yourself). The API key is kept in `~/.config/chronos/cloud.toml`, readable only by you.
2. **Finds or makes the project's database.** A folder with `.chronos/project.json` is linked already: that project is used. Otherwise a project named after the folder is made, matched to the git remote (`origin`, without any credential in its URL), so a second run, or a clone of the same repository, gets the same database. New accounts are on the free plan: 2 databases.
3. **Writes the project's files**, merging into what is there and never replacing it:

| File | What is written |
|---|---|
| `.chronos/project.json` | `{"project_id", "name"}`: the link, no secrets. Commit it, so teammates and CI reach the same database |
| `.mcp.json` | the `chronos` server in `mcpServers` (`{"type": "http", "url": "https://db.chronos.2090.tech/?project=prj_..."}`); other servers are kept |
| `.env` | `DATABASE_URL=...` for the app, pointing at `main`: the line is replaced if there is one, else added; other lines are kept. A new `.env` is readable only by you |
| `.gitignore` | `.env`, unless it is ignored already |
| `CLAUDE.md`, and `AGENTS.md` if there is one | a short block between `<!-- chronos:begin -->` and `<!-- chronos:end -->`: the database is Chronos Cloud; change the schema or data in a world, run the app's tests against the world, show the diff, then merge. The rest of the file is kept; a second run replaces only the block |

Running it again changes nothing that is already right (`files already up to date`). The password in `DATABASE_URL` is never printed.

The app's tests run against a world by changing the database name: `DATABASE_URL` with `/main` replaced by the world's name (see [concepts](concepts.md)).

## Other commands

```bash
chronos cloud login              # sign in again (or as someone else)
chronos cloud logout             # forget the API key on this machine
chronos cloud list               # your org's projects and their databases
chronos cloud usage              # this folder's project: disk, worlds, egress, awake time, against the plan
chronos cloud production on      # agents' worlds wait for a person to merge them into main (off: agents merge)
```

`usage` and `production` act on the folder's project (`.chronos/project.json`).

## Settings

| Setting | |
|---|---|
| `CHRONOS_CLOUD_API` | the API's address instead of `https://chronos.2090.tech` (the dashboard's address, which forwards `/v1` and `/auth` to the backend) (a local control plane in tests) |
| `~/.config/chronos/cloud.toml` | `api_key` and the `api` address it came from (`$XDG_CONFIG_HOME/chronos/cloud.toml` where that is set) |
| `SSL_CERT_FILE` | CA certificates for the API's TLS, where the system's bundle isn't in `/etc/ssl` |

## Errors

| Message | What to do |
|---|---|
| `not signed in to Chronos Cloud, or the API key was revoked: run chronos cloud login` | sign in again |
| `your plan's limit is reached (...)` | the plan's databases are all in use: delete a project you no longer need, or upgrade |
| `can't reach the Chronos Cloud API at ...` | check the network (and `CHRONOS_CLOUD_API` if set) |
| `.chronos/project.json names project prj_..., which isn't one of your org's` | sign in as a member of the project's org, or remove `.chronos/project.json` to make a new project |
