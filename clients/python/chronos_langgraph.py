"""LangGraph persistence on a Chronos world: a checkpointer and a store. Needs `pip install langgraph`.

    from chronos import Chronos
    from chronos_langgraph import ChronosSaver, ChronosStore

    db = Chronos("http://127.0.0.1:7070", token=os.environ.get("CHRONOS_TOKEN"))
    world = db.fork(f"run-{job.id}")                 # the agent's data and its state, in one copy of main
    graph = builder.compile(checkpointer=ChronosSaver(world), store=ChronosStore(world))
    graph.invoke(inputs, {"configurable": {"thread_id": job.id}})
    print(world.diff())                              # rows it changed, and the checkpoints that led there
    world.merge()                                    # both land together; world.discard() drops both

A thread's checkpoints live in the world it ran in, beside the rows it wrote: a reviewer sees them in the diff,
`world.at("-5 minutes")` reads both as they were, and forking the world forks the thread. Main is protected by
default, so give these a world, not main. Each needs only the `chronos` client and LangGraph; rows are kept in the
tables `langgraph_checkpoints`, `langgraph_writes` and `langgraph_store`, made on first write.
"""

from __future__ import annotations

import asyncio
import base64
import random
from collections.abc import AsyncIterator, Iterable, Iterator, Sequence
from datetime import datetime, timezone
from typing import Any, Optional
from urllib.parse import quote

from langgraph.checkpoint.base import (
    WRITES_IDX_MAP,
    BaseCheckpointSaver,
    ChannelVersions,
    Checkpoint,
    CheckpointMetadata,
    CheckpointTuple,
    get_checkpoint_id,
    get_checkpoint_metadata,
    writes_sort_key,
)
from langgraph.store.base import (
    BaseStore,
    GetOp,
    Item,
    ListNamespacesOp,
    Op,
    PutOp,
    Result,
    SearchItem,
    SearchOp,
)

from chronos import Chronos, ChronosError

CHECKPOINTS, WRITES, STORE = "langgraph_checkpoints", "langgraph_writes", "langgraph_store"


def _id(*parts: Any) -> str:
    """A row id from parts, each escaped so ids of different parts never collide ("t:1" vs "t", "1")."""
    return ":".join(quote(str(p), safe="") for p in parts)


def _rows(db: Chronos, sql: str, params: list) -> list[dict]:
    """A query's rows; none when the table isn't made yet (nothing was written to it)."""
    try:
        return db.sql(sql, params)["rows"]
    except ChronosError as e:
        if e.status == 404 and "does not exist" in str(e):
            return []
        raise


def _b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


def _config(thread_id: str, ns: str, checkpoint_id: str) -> dict:
    return {"configurable": {"thread_id": thread_id, "checkpoint_ns": ns, "checkpoint_id": checkpoint_id}}


class ChronosSaver(BaseCheckpointSaver[str]):
    """A LangGraph checkpointer keeping threads in a Chronos branch (a world: `db.fork(...)`).

    Each checkpoint is one row of `langgraph_checkpoints` (the checkpoint with its channel values, serialized by
    LangGraph's serde), and each pending write one row of `langgraph_writes`. `put_writes` writes a task's rows
    in one batch: all or none.
    """

    def __init__(self, db: Chronos, *, serde: Any = None) -> None:
        super().__init__(serde=serde)
        self.db = db

    # ---- reads

    def _tuple(self, row: dict, writes: list[dict]) -> CheckpointTuple:
        thread_id, ns, cid = row["thread_id"], row["checkpoint_ns"], row["checkpoint_id"]
        loads = self.serde.loads_typed
        writes = sorted(writes, key=lambda w: writes_sort_key(w["task_path"], w["task_id"], w["idx"]))
        parent = row.get("parent_checkpoint_id")
        return CheckpointTuple(
            config=_config(thread_id, ns, cid),
            checkpoint=loads((row["checkpoint_type"], base64.b64decode(row["checkpoint"]))),
            metadata=loads((row["metadata_type"], base64.b64decode(row["metadata"]))),
            parent_config=_config(thread_id, ns, parent) if parent else None,
            pending_writes=[
                (w["task_id"], w["channel"], loads((w["type"], base64.b64decode(w["value"])))) for w in writes
            ],
        )

    def _writes(self, thread_id: str, ns: Optional[str]) -> dict[tuple[str, str], list[dict]]:
        """The thread's pending writes (one namespace's, or all), by (namespace, checkpoint id)."""
        sql = f"select * from {WRITES} where thread_id = $1"
        params: list = [thread_id]
        if ns is not None:
            sql += " and checkpoint_ns = $2"
            params.append(ns)
        out: dict[tuple[str, str], list[dict]] = {}
        for w in _rows(self.db, sql, params):
            out.setdefault((w["checkpoint_ns"], w["checkpoint_id"]), []).append(w)
        return out

    def get_tuple(self, config: dict) -> Optional[CheckpointTuple]:
        """The checkpoint `config` names, or the thread's latest in its namespace."""
        c = config["configurable"]
        thread_id, ns = c["thread_id"], c.get("checkpoint_ns", "")
        if checkpoint_id := get_checkpoint_id(config):
            rows = _rows(self.db, f"select * from {CHECKPOINTS} where id = $1", [_id(thread_id, ns, checkpoint_id)])
        else:
            rows = _rows(
                self.db,
                f"select * from {CHECKPOINTS} where thread_id = $1 and checkpoint_ns = $2 "
                "order by checkpoint_id desc limit 1",
                [thread_id, ns],
            )
        if not rows:
            return None
        row = rows[0]
        writes = _rows(
            self.db,
            f"select * from {WRITES} where thread_id = $1 and checkpoint_ns = $2 and checkpoint_id = $3",
            [thread_id, ns, row["checkpoint_id"]],
        )
        return self._tuple(row, writes)

    def list(
        self,
        config: Optional[dict],
        *,
        filter: Optional[dict[str, Any]] = None,
        before: Optional[dict] = None,
        limit: Optional[int] = None,
    ) -> Iterator[CheckpointTuple]:
        """Checkpoints newest first: of one thread (and namespace, if `config` has one), or of every thread with
        `config` None. `filter` keeps those whose metadata has each key equal to its value."""
        sql, params = f"select * from {CHECKPOINTS} where true", []
        thread_id = ns = None
        if config:
            c = config["configurable"]
            thread_id, ns = c["thread_id"], c.get("checkpoint_ns")
            params.append(thread_id)
            sql += f" and thread_id = ${len(params)}"
            if ns is not None:
                params.append(ns)
                sql += f" and checkpoint_ns = ${len(params)}"
            if checkpoint_id := get_checkpoint_id(config):
                params.append(checkpoint_id)
                sql += f" and checkpoint_id = ${len(params)}"
        if before and (before_id := get_checkpoint_id(before)):
            params.append(before_id)
            sql += f" and checkpoint_id < ${len(params)}"
        sql += " order by thread_id, checkpoint_ns, checkpoint_id desc"
        if limit is not None and not filter:
            sql += f" limit {int(limit)}"
        rows = _rows(self.db, sql, params)
        writes: dict[str, dict] = {}  # per thread, read once
        for row in rows:
            t = self._tuple(row, [])
            if filter and not all(t.metadata.get(k) == v for k, v in filter.items()):
                continue
            if limit is not None:
                if limit <= 0:
                    return
                limit -= 1
            if row["thread_id"] not in writes:
                writes[row["thread_id"]] = self._writes(row["thread_id"], ns)
            pending = writes[row["thread_id"]].get((row["checkpoint_ns"], row["checkpoint_id"]), [])
            yield self._tuple(row, pending) if pending else t

    # ---- writes

    def put(
        self, config: dict, checkpoint: Checkpoint, metadata: CheckpointMetadata, new_versions: ChannelVersions
    ) -> dict:
        c = config["configurable"]
        thread_id, ns = c["thread_id"], c.get("checkpoint_ns", "")
        meta = get_checkpoint_metadata(config, metadata)
        ctype, cdata = self.serde.dumps_typed(checkpoint)
        mtype, mdata = self.serde.dumps_typed(meta)
        self.db.put(CHECKPOINTS, _id(thread_id, ns, checkpoint["id"]), {
            "thread_id": thread_id,
            "checkpoint_ns": ns,
            "checkpoint_id": checkpoint["id"],
            "parent_checkpoint_id": c.get("checkpoint_id"),
            "step": meta.get("step"),
            "source": meta.get("source"),
            "checkpoint_type": ctype,
            "checkpoint": _b64(cdata),
            "metadata_type": mtype,
            "metadata": _b64(mdata),
        })
        return _config(thread_id, ns, checkpoint["id"])

    def put_writes(self, config: dict, writes: Sequence[tuple[str, Any]], task_id: str, task_path: str = "") -> None:
        """A task's writes to a checkpoint. A write already kept is left as it is, but for the special channels
        (errors, interrupts), whose latest replaces the one before."""
        c = config["configurable"]
        thread_id, ns, checkpoint_id = c["thread_id"], c.get("checkpoint_ns", ""), c["checkpoint_id"]
        kept = {
            w["idx"]
            for w in _rows(
                self.db,
                f"select idx from {WRITES} where thread_id = $1 and checkpoint_ns = $2 and checkpoint_id = $3 "
                "and task_id = $4",
                [thread_id, ns, checkpoint_id, task_id],
            )
        }
        rows = []
        for i, (channel, value) in enumerate(writes):
            idx = WRITES_IDX_MAP.get(channel, i)
            if idx >= 0 and idx in kept:
                continue
            vtype, vdata = self.serde.dumps_typed(value)
            rows.append({"table": WRITES, "id": _id(thread_id, ns, checkpoint_id, task_id, idx), "record": {
                "thread_id": thread_id,
                "checkpoint_ns": ns,
                "checkpoint_id": checkpoint_id,
                "task_id": task_id,
                "task_path": task_path,
                "idx": idx,
                "channel": channel,
                "type": vtype,
                "value": _b64(vdata),
            }})
        if rows:
            self.db.batch(rows)

    def delete_thread(self, thread_id: str) -> None:
        rows = [
            {"table": table, "id": r["id"], "record": None}
            for table in (CHECKPOINTS, WRITES)
            for r in _rows(self.db, f"select id from {table} where thread_id = $1", [thread_id])
        ]
        if rows:
            self.db.batch(rows)

    def copy_thread(self, source_thread_id: str, target_thread_id: str) -> None:
        """Every checkpoint and write of one thread, under another thread id, in one batch."""
        rows = []
        for table, key in ((CHECKPOINTS, ("checkpoint_ns", "checkpoint_id")),
                           (WRITES, ("checkpoint_ns", "checkpoint_id", "task_id", "idx"))):
            for r in _rows(self.db, f"select * from {table} where thread_id = $1", [source_thread_id]):
                r.pop("id")
                r["thread_id"] = target_thread_id
                rows.append({"table": table, "id": _id(target_thread_id, *(r[k] for k in key)), "record": r})
        if rows:
            self.db.batch(rows)

    def get_next_version(self, current: Optional[str], channel: None = None) -> str:
        """As LangGraph's own savers: a zero-padded counter, so versions sort as strings too."""
        n = 0 if current is None else current if isinstance(current, int) else int(current.split(".")[0])
        return f"{n + 1:032}.{random.random():016}"

    # ---- async, each call on a worker thread

    async def aget_tuple(self, config: dict) -> Optional[CheckpointTuple]:
        return await asyncio.to_thread(self.get_tuple, config)

    async def alist(
        self,
        config: Optional[dict],
        *,
        filter: Optional[dict[str, Any]] = None,
        before: Optional[dict] = None,
        limit: Optional[int] = None,
    ) -> AsyncIterator[CheckpointTuple]:
        items = await asyncio.to_thread(lambda: list(self.list(config, filter=filter, before=before, limit=limit)))
        for item in items:
            yield item

    async def aput(
        self, config: dict, checkpoint: Checkpoint, metadata: CheckpointMetadata, new_versions: ChannelVersions
    ) -> dict:
        return await asyncio.to_thread(self.put, config, checkpoint, metadata, new_versions)

    async def aput_writes(
        self, config: dict, writes: Sequence[tuple[str, Any]], task_id: str, task_path: str = ""
    ) -> None:
        await asyncio.to_thread(self.put_writes, config, writes, task_id, task_path)

    async def adelete_thread(self, thread_id: str) -> None:
        await asyncio.to_thread(self.delete_thread, thread_id)

    async def acopy_thread(self, source_thread_id: str, target_thread_id: str) -> None:
        await asyncio.to_thread(self.copy_thread, source_thread_id, target_thread_id)


def _path(namespace: Sequence[str]) -> str:
    """A namespace as a string whose prefixes are its parents' ("a/b/" starts with "a/")."""
    return "".join(quote(p, safe="") + "/" for p in namespace)


def _strings(value: Any) -> Iterator[str]:
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for v in value.values():
            yield from _strings(v)
    elif isinstance(value, (list, tuple)):
        for v in value:
            yield from _strings(v)


def _text(value: dict, fields: Optional[Sequence[str]]) -> str:
    """The words `query` searches: the given top-level fields' strings, or every string in the value."""
    if fields is not None:
        value = {k: value[k] for k in fields if k in value}
    return " ".join(_strings(value))


def _matches(value: Any, want: Any) -> bool:
    """A filter's value against an item's, as LangGraph's stores compare: nested dicts by their keys, and
    `{"$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte": v}` as operators."""
    if isinstance(want, dict):
        if any(k.startswith("$") for k in want):
            ops = {
                "$eq": lambda a, b: a == b, "$ne": lambda a, b: a != b,
                "$gt": lambda a, b: a > b, "$gte": lambda a, b: a >= b,
                "$lt": lambda a, b: a < b, "$lte": lambda a, b: a <= b,
            }
            try:
                return all(ops[k](value, v) for k, v in want.items())
            except (KeyError, TypeError):
                return False
        return isinstance(value, dict) and all(_matches(value.get(k), v) for k, v in want.items())
    return value == want


class ChronosStore(BaseStore):
    """LangGraph's long-term memory store on a Chronos branch: one row of `langgraph_store` per item.

    `search(query=...)` uses Chronos's own full-text search (typos allowed) over the item's strings, or over the
    fields a put's `index` names; `index=False` leaves an item out of it. Vector search isn't offered: a store
    made with an `index` embedding config is refused.
    """

    def __init__(self, db: Chronos, *, index: Optional[dict] = None) -> None:
        if index:
            raise ValueError("ChronosStore searches text; it has no embedding index")
        self.db = db

    @staticmethod
    def _item(row: dict, score: Optional[float] = None, search: bool = False) -> Item:
        args = dict(
            value=row["value"],
            key=row["key"],
            namespace=tuple(row["namespace"]),
            created_at=datetime.fromisoformat(row["created_at"]),
            updated_at=datetime.fromisoformat(row["updated_at"]),
        )
        return SearchItem(**args, score=score) if search else Item(**args)

    def _search(self, op: SearchOp) -> list[SearchItem]:
        prefix = _path(op.namespace_prefix)
        if op.query:
            # find ranks by the query; the prefix and filter are applied to its hits
            want = op.offset + op.limit
            hits, offset = [], 0
            while True:
                r = self.db.find(STORE, text=op.query, limit=max(100, want * 4), offset=offset)
                for h in r["hits"]:
                    row = {**h["row"], "id": h["id"]}
                    if row["path"].startswith(prefix) and all(
                        _matches(row["value"].get(k), v) for k, v in (op.filter or {}).items()
                    ):
                        hits.append((row, h["score"]))
                offset = r.get("next_offset")
                if len(hits) >= want or not offset:
                    break
            return [self._item(row, score, True) for row, score in hits[op.offset:want]]
        rows = _rows(self.db, f"select * from {STORE} where starts_with(path, $1) order by path, key", [prefix])
        if op.filter:
            rows = [r for r in rows if all(_matches(r["value"].get(k), v) for k, v in op.filter.items())]
        return [self._item(r, None, True) for r in rows[op.offset:op.offset + op.limit]]

    def _namespaces(self, op: ListNamespacesOp) -> list[tuple[str, ...]]:
        seen = {tuple(r["namespace"]) for r in _rows(self.db, f"select namespace from {STORE}", [])}

        def match(cond: Any, ns: tuple[str, ...]) -> bool:
            path = cond.path
            if len(ns) < len(path):
                return False
            pairs = zip(ns, path) if cond.match_type == "prefix" else zip(reversed(ns), reversed(path))
            return all(p == "*" or n == p for n, p in pairs)

        names = [ns for ns in seen if all(match(c, ns) for c in op.match_conditions or ())]
        if op.max_depth is not None:
            names = list({ns[: op.max_depth] for ns in names})
        return sorted(names)[op.offset:op.offset + op.limit]

    def batch(self, ops: Iterable[Op]) -> list[Result]:
        """The ops in order; the puts and deletes are written together at the end, in one batch (the last for an
        item wins), as LangGraph's stores do."""
        results: list[Result] = []
        puts: dict[tuple[tuple[str, ...], str], PutOp] = {}
        for op in ops:
            if isinstance(op, GetOp):
                rows = _rows(self.db, f"select * from {STORE} where id = $1", [_id(_path(op.namespace), op.key)])
                results.append(self._item(rows[0]) if rows else None)
            elif isinstance(op, SearchOp):
                results.append(self._search(op))
            elif isinstance(op, ListNamespacesOp):
                results.append(self._namespaces(op))
            elif isinstance(op, PutOp):
                puts[(op.namespace, op.key)] = op
                results.append(None)
            else:
                raise ValueError(f"unknown operation {type(op).__name__}")
        if puts:
            self._write(list(puts.values()))
        return results

    def _write(self, puts: list[PutOp]) -> None:
        now = datetime.now(timezone.utc).isoformat()
        ids = [_id(_path(p.namespace), p.key) for p in puts]
        created = {}
        for i in range(0, len(ids), 500):
            chunk = ids[i:i + 500]
            marks = ", ".join(f"${n + 1}" for n in range(len(chunk)))
            for r in _rows(self.db, f"select id, created_at from {STORE} where id in ({marks})", chunk):
                created[r["id"]] = r["created_at"]
        rows = []
        for row_id, p in zip(ids, puts):
            if p.value is None:
                rows.append({"table": STORE, "id": row_id, "record": None})
                continue
            fields = None if p.index is None else ([] if p.index is False else [f.split(".")[0] for f in p.index])
            rows.append({"table": STORE, "id": row_id, "record": {
                "namespace": list(p.namespace),
                "path": _path(p.namespace),
                "key": p.key,
                "value": dict(p.value),
                "text": _text(p.value, fields),
                "created_at": created.get(row_id, now),
                "updated_at": now,
            }})
        self.db.batch(rows)

    async def abatch(self, ops: Iterable[Op]) -> list[Result]:
        ops = list(ops)
        return await asyncio.to_thread(self.batch, ops)
