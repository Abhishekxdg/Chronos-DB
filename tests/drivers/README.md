# Drivers and ORMs against Chronos

Each folder or file here is one client talking to `chronos serve` over the Postgres protocol, the way an
application would: make a table, write rows with typed parameters, read them back, use a transaction, and read
a constraint error's SQLSTATE. `.github/workflows/drivers.yml` runs them all (nightly, on request, and on pull
requests that change this folder).

Each takes the server from `CHRONOS_URL` (default `postgres://tester@127.0.0.1:5433/main`) and prints `ok` last.

| Client | File |
|---|---|
| psycopg 3 (Python) | `psycopg_smoke.py` |
| SQLAlchemy 2 (Python, over psycopg) | `sqlalchemy_smoke.py` |
| Django ORM (Python) | `django_smoke.py` |
| node-postgres | `node_pg_smoke.mjs` |
| pgx v5 (Go) | `pgx/main.go` |
| JDBC (pgjdbc) | `jdbc/Smoke.java` |
| Prisma | `prisma/` |
