# Security

## Reporting a problem

Please don't open a public issue for security problems. Report them privately with GitHub's [private vulnerability reporting](https://github.com/Abhishekxdg/chronosdb/security/advisories/new) on the chronosdb repository. We aim to reply within 3 working days.

Data-loss and corruption bugs count as security problems here: please report them privately too.

## What Chronos protects, and what it doesn't

- **`chronos serve`** listens on 127.0.0.1 by default. Any other address requires a bearer token, and a missing or wrong token gets `401`.
  - TLS is built in (see [operations](docs/operations.md#tls)); without it the server speaks plain HTTP and Postgres.
  - The server token gives full access. Agent accounts get their own tokens, capabilities and quotas, and `--safe` keeps them off `main` (see [security and auth](docs/security.md)).
  - Web pages can't reach it through your browser: a request with an `Origin` header gets `403`, a body must be `Content-Type: application/json` (so a browser must ask first, and is never allowed), and without a token the `Host` must be `localhost`, `127.x.x.x` or `[::1]` (against DNS rebinding).
  - Each listener (HTTP, Postgres) serves at most 1000 connections at once. An HTTP request must arrive within 2 minutes, and is checked (token, headers) before its body is read. A Postgres client must log in within a minute, with messages under 10 kB until it has.
- **Other `chronos` processes on the same folder** connect over a Unix socket in `$TMPDIR/chronos-<uid>/` (mode 0700). Both ends check that the other runs as the same user.
- **`chronos mcp`** runs as your user over stdio, with full access to the database folder it's given unless started as an agent.
- **Data at rest** is checksummed (CRC32 and blake3 page names) against corruption, and can be encrypted (see [operations](docs/operations.md#encryption-at-rest)), spill files included.
- **Rollback of a folder's files** isn't detected by default, with or without a key: encryption authenticates each log record (bound to its segment and offset), page and manifest, but someone who can write the folder can put back an older, consistent set of files (a manifest and the log it names), and it opens as that older state. A manifest older than the log beside it is refused at open. With a remote, a folder whose state isn't the remote's latest manifest is refused too. For a purely local folder, turn on the **anchor** (`--anchor FILE`, `CHRONOS_ANCHOR=FILE`, or `Open::anchor`): a file kept outside the folder that records the database's identity and how far its log has got, moved on at every checkpoint and clean close. An open that finds the folder behind its anchor refuses (`database folder is older than its anchor: rolled back?`, code XX0A1); so does one that finds the anchor missing (deleting it would otherwise switch the check off), except for a brand-new folder, which creates it. A crash before the anchor moves leaves the folder ahead of it, which opens and moves it on. After a deliberate restore from a backup, open once with `--anchor-reset` (`CHRONOS_ANCHOR_RESET=1`, `Open::anchor_reset`) to accept the folder and re-anchor it. Limits: with a key the anchor is sealed with it, so it can't be forged without the key, but an older copy of it can still be put back; without a key it's a plain file, protected only by its location's permissions, so keep it where whoever can write the folder can't (another volume, owned by another user). Commits since the last checkpoint or clean close aren't in it, so cutting the log back within that stretch goes unnoticed.
- **Release binaries** come with a SHA-256 checksum that `install.sh` checks, and a keyless cosign signature (Sigstore) from the release builds (the identity of the service account they run as): no key to leak, and each signature is recorded in Sigstore's public transparency log. `CHRONOS_VERIFY=1` makes `install.sh` check it too (it needs `cosign`).
- **Cloud credentials** come from the standard `AWS_*` environment variables. They're never written to the database folder.
- **The one-process-per-folder lock** and the remote lease prevent accidental concurrent writers. They're not a security boundary.

Known limitations that matter before exposing a server to untrusted clients are listed in [docs/security.md](docs/security.md#known-limitations-dont-expose-to-untrusted-networks-yet).
