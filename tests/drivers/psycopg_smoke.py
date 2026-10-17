"""psycopg 3 against Chronos: typed parameters both ways, a prepared statement, a transaction, an error's SQLSTATE."""
import datetime
import decimal
import os
import uuid

import psycopg
from psycopg.types.json import Jsonb

URL = os.environ.get("CHRONOS_URL", "postgres://tester@127.0.0.1:5433/main")

with psycopg.connect(URL, autocommit=True) as conn:
    conn.execute("drop table if exists drv_psycopg")
    conn.execute(
        "create table drv_psycopg (id bigint primary key, name text not null unique, score numeric(10,2), "
        "ok boolean, at timestamptz, day date, doc jsonb, tags text[], ref uuid)"
    )
    at = datetime.datetime(2026, 1, 2, 3, 4, 5, tzinfo=datetime.timezone.utc)
    ref = uuid.UUID("00000000-0000-4000-8000-000000000001")
    row = (1, "Ada", decimal.Decimal("9.50"), True, at, datetime.date(2026, 1, 2), Jsonb({"a": [1, 2]}), ["x", "y"], ref)
    conn.execute("insert into drv_psycopg values (%s, %s, %s, %s, %s, %s, %s, %s, %s)", row)
    conn.execute("insert into drv_psycopg (id, name) values (%s, %s)", (2, "Bob"))
    got = conn.execute("select * from drv_psycopg where id = %s", (1,)).fetchone()
    assert got[:6] == row[:6], got
    assert got[6] == {"a": [1, 2]} and got[7] == ["x", "y"] and got[8] == ref, got
    assert conn.execute("select score, ok, doc from drv_psycopg where id = 2").fetchone() == (None, None, None)

    # a statement prepared once, run with several values (binary parameters and results)
    with conn.cursor(binary=True) as cur:
        for i, want in ((1, "Ada"), (2, "Bob")):
            cur.execute("select name from drv_psycopg where id = %s", (i,), prepare=True)
            assert cur.fetchone() == (want,)

    # many rows at once
    with conn.cursor() as cur:
        cur.executemany("insert into drv_psycopg (id, name) values (%s, %s)", [(i, f"n{i}") for i in range(10, 20)])
    assert conn.execute("select count(*) from drv_psycopg").fetchone() == (12,)

    # a transaction that is rolled back leaves nothing; one that commits does
    conn.autocommit = False
    conn.execute("insert into drv_psycopg (id, name) values (100, 'gone')")
    conn.rollback()
    conn.execute("insert into drv_psycopg (id, name) values (101, 'kept')")
    conn.commit()
    conn.autocommit = True
    assert conn.execute("select id from drv_psycopg where id >= 100").fetchall() == [(101,)]

    # a unique violation says 23505
    try:
        conn.execute("insert into drv_psycopg (id, name) values (3, 'Ada')")
        raise SystemExit("a duplicate name was stored")
    except psycopg.errors.UniqueViolation as e:
        assert e.sqlstate == "23505", e.sqlstate

    # COPY in and out
    with conn.cursor() as cur:
        with cur.copy("copy drv_psycopg (id, name) from stdin") as copy:
            copy.write_row((200, "copied"))
            copy.write_row((201, "too"))
        with cur.copy("copy (select id, name from drv_psycopg where id >= 200 order by id) to stdout") as copy:
            assert b"".join(bytes(b) for b in copy) == b"200\tcopied\n201\ttoo\n"

    conn.execute("drop table drv_psycopg")
print("ok")
