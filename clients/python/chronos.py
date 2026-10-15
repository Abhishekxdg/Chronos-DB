"""Chronos DB client for Python 3.9+. Talks to `chronos serve <folder>`; one file, standard library only.

    db = Chronos("http://127.0.0.1:7070", token=os.environ.get("CHRONOS_TOKEN"))
    agent = db.fork("agent-7")                  # instant copy of main
    agent.put("users", "1", {"name": "Ada L."})
    print(agent.diff())
    agent.merge()                               # or agent.discard()
"""

from __future__ import annotations

import ipaddress
import json
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Optional

Row = dict


class ChronosError(Exception):
    """An error from the server. `status` is 400/404/409/...; merge conflicts come back as data."""

    def __init__(self, status: int, message: str, conflicts: Optional[list] = None):
        super().__init__(message)
        self.status = status
        self.conflicts = conflicts or []


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """The server never redirects; following one would send the token to wherever it points."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None  # the 3xx comes back as an HTTPError


_opener = urllib.request.build_opener(_NoRedirect)
# a server on this machine is never reached through http_proxy: it would see the token
_direct = urllib.request.build_opener(_NoRedirect, urllib.request.ProxyHandler({}))


def _loopback(url: str) -> bool:
    host = urllib.parse.urlsplit(url).hostname or ""
    try:
        return host == "localhost" or ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


class Chronos:
    def __init__(self, url: str = "http://127.0.0.1:7070", token: Optional[str] = None, branch: str = "main"):
        self.url = url.rstrip("/")
        self.token = token
        self.branch_name = branch
        # the branch version our last write returned; merge sends it so writes lost in a crash are caught
        self.version: Optional[int] = None

    def branch(self, name: str) -> "Chronos":
        """The same client, working on another branch."""
        return Chronos(self.url, self.token, name)

    def _call(self, op: str, **body: Any) -> dict:
        body.setdefault("branch", self.branch_name)
        req = urllib.request.Request(
            f"{self.url}/v1/{op}",
            data=json.dumps({k: v for k, v in body.items() if v is not None}).encode(),
            headers={"Content-Type": "application/json"}
            | ({"Authorization": f"Bearer {self.token}"} if self.token else {}),
            method="POST",
        )
        try:
            with (_direct if _loopback(self.url) else _opener).open(req) as res:
                return json.load(res)
        except urllib.error.HTTPError as e:
            text = e.read().decode(errors="replace")
            try:
                err = json.loads(text)["error"]
            except (ValueError, TypeError, KeyError):  # not ours: a proxy's page, say
                raise ChronosError(e.code, text.strip() or str(e.reason)) from None
            if isinstance(err, dict):
                raise ChronosError(e.code, err.get("message", ""), err.get("conflicts")) from None
            raise ChronosError(e.code, str(err)) from None

    def get(self, table: str, id: str) -> Optional[Row]:
        """The row, or None if there is none."""
        try:
            return self._call("get", table=table, id=id)["row"]
        except ChronosError as e:
            if e.status == 404 and str(e).startswith("no row"):
                return None
            raise

    def put(self, table: str, id: str, record: Row) -> None:
        self.version = self._call("put", table=table, id=id, record=record)["version"]

    def delete(self, table: str, id: str) -> None:
        self.version = self._call("delete", table=table, id=id)["version"]

    def batch(self, rows: list) -> int:
        """Writes all rows or none: [{"table", "id", "record": {...} or None}]; None deletes."""
        r = self._call("batch", rows=rows)
        self.version = r["version"]
        return r["count"]

    def find(
        self,
        table: str,
        where: Optional[dict] = None,
        text: Optional[str] = None,
        vector: Optional[list] = None,
        vector_field: Optional[str] = None,
        limit: Optional[int] = None,
        offset: Optional[int] = None,
    ) -> dict:
        """{"total": rows passing the filters, "hits": [{"id", "score", "row"}], "next_offset": ...}"""
        return self._call(
            "find", table=table, where=where, text=text, vector=vector, vector_field=vector_field,
            limit=limit, offset=offset,
        )

    def fork(self, name: str, meta: Optional[dict] = None) -> "Chronos":
        """Creates a branch from this one (instant) and returns a client working on it. `meta`:
        notes kept with the branch (owner, task, tags, ...)."""
        self._call("fork", name=name, meta=meta, **{"from": self.branch_name})
        b = self.branch(name)
        b.version = 0
        return b

    def sql(self, sql: str, params: Optional[list] = None) -> dict:
        """Runs SQL on this branch; params fill $1, $2, ... Returns the last statement's
        {"command": "INSERT 0 1", "rows": [{column: value}]}."""
        r = self._call("sql", sql=sql, params=params)
        if r.get("version") is not None:
            self.version = r["version"]
        return r["results"][-1] if r["results"] else {"command": "", "rows": []}

    def branches(self) -> list:
        """[{"name", "id", "parent", "depth", "created", "version", "flagged", "meta"}]"""
        return self._call("branches")["branches"]

    def at(self, when: str) -> "Chronos":
        """This branch as it was at `when` (a time like "2026-09-20 10:00", or "-5 minutes"): a
        client that reads the past."""
        return self.branch(f"{self.branch_name}@{when}")

    def history(self, limit: int = 100) -> list:
        """What happened to this branch, newest first: [{"at", "world", "event", "rows"}]."""
        return self._call("history", limit=limit)["events"]

    def restore(self, at: str) -> int:
        """Puts this branch back as it was at `at`; returns rows written."""
        return self._call("restore", at=at)["restored"]

    def undo_merge(self, world: str, skip_changed: bool = False) -> dict:
        """Undoes the latest merge of `world` into its parent (within the history window):
        {"parent", "at", "undone", "skipped"}."""
        return self._call("undo_merge", branch=world, skip_changed=skip_changed or None)

    def world(self) -> dict:
        """This branch's ID, lineage and metadata."""
        return self._call("world")

    def set_meta(self, meta: dict) -> dict:
        """Merges `meta` into this branch's metadata (None removes a key); returns the branch."""
        return self._call("set_meta", meta=meta)

    def diff(self) -> list:
        """What changed on this branch since it was forked: [{"key", "before", "after"}]."""
        return self._call("diff")["changes"]

    def diff_count(self) -> dict:
        """How many rows changed on this branch, without reading them: {"total", "tables": {name: n}}."""
        return self._call("diff", count=True)

    def diff_page(self, limit: int, after: Optional[str] = None) -> dict:
        """Up to `limit` changes after cursor `after`: {"changes", "next"}. Pass "next" back as `after`
        for the next page; it is None on the last one. Reads only this page, so it suits big diffs."""
        return self._call("diff", limit=limit, after=after)

    def merge(self, resolve: str = "fail", confirm: bool = False, columns: bool = False,
              picks: Optional[dict] = None, into: Optional[str] = None,
              only_tables: Optional[list] = None, only_keys: Optional[list] = None) -> int:
        """Applies this branch to its parent and deletes it. Raises ChronosError (409) with .conflicts
        (each with "explain") if both changed the same rows: retry with resolve="ours" or "theirs",
        columns=True (combine rows where the sides changed different columns), or picks
        ({"table/id": "ours" | "theirs" | row dict | None}). Also raises if the branch lost writes
        in a server crash (redo them; confirm=True merges it as it is after a crash).
        only_tables / only_keys ("table/id") merge just those rows, and into merges into another
        branch instead of the parent; either way this branch stays."""
        version = None if confirm else self.version
        return self._call("merge", resolve=resolve, version=version, confirm=confirm or None,
                          columns=columns or None, picks=picks, into=into,
                          only_tables=only_tables, only_keys=only_keys)["merged"]

    def preview(self, resolve: str = "fail", columns: bool = False, picks: Optional[dict] = None,
                into: Optional[str] = None, only_tables: Optional[list] = None,
                only_keys: Optional[list] = None) -> dict:
        """What merge() would do, row by row, changing nothing: {"rows": [{"key", "outcome",
        "detail", "base", "ours", "theirs", "result"}], "conflicts": n, "blocked": reason or None}."""
        return self._call("merge", resolve=resolve, columns=columns or None, picks=picks, into=into,
                          only_tables=only_tables, only_keys=only_keys, dry_run=True)

    def discard(self) -> None:
        self._call("discard")

    def simulate(self, worlds: int, prefix: str, script: str, score: str, keep: Any = None,
                 order: str = "desc", seed: int = 0, threads: Optional[int] = None) -> dict:
        """Forks `worlds` worlds from this branch (at one moment), runs `script` in each in parallel
        ($1: the world's index, $2: its seed), scores each with `score` (a SELECT giving a number)
        and keeps the best `keep` (default all; order="asc" when lower is better). The same call
        gives the same worlds: random() is seeded per world. Returns {"worlds": [{"world", "id",
        "index", "seed", "score", "error", "kept"}] best first, "base", "at", "timings"}."""
        return self._call("simulate", **{"from": self.branch_name}, worlds=worlds, prefix=prefix,
                          script=script, score=score, keep=keep, order=order, seed=seed, threads=threads)

    def replay(self, world: str, as_name: Optional[str] = None) -> dict:
        """Makes a simulated world again from its recorded inputs: {"identical", "rows_differing",
        "score", "recorded_score", ...}. With as_name, keeps the replay as a new branch."""
        return self._call("replay", world=world, **{"as": as_name})
