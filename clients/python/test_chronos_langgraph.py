"""LangGraph on a world, against a running `chronos serve` (CHRONOS_URL, CHRONOS_TOKEN). Run: python3
test_chronos_langgraph.py. Skipped without langgraph; with langgraph-checkpoint-conformance installed, LangGraph's
own checkpointer suite runs too."""
import asyncio
import os
import sys
import uuid
from typing import Annotated, TypedDict

try:
    from langgraph.graph import END, START, StateGraph
except ImportError:
    print("skipped: langgraph not installed")
    sys.exit(0)
import operator

from chronos import Chronos
from chronos_langgraph import ChronosSaver, ChronosStore

db = Chronos(os.environ["CHRONOS_URL"], token=os.environ.get("CHRONOS_TOKEN"))


class State(TypedDict):
    steps: Annotated[list, operator.add]


def plan(state: State) -> dict:
    return {"steps": ["plan"]}


def act(state: State) -> dict:
    return {"steps": ["act"]}


builder = StateGraph(State)
builder.add_node("plan", plan)
builder.add_node("act", act)
builder.add_edge(START, "plan")
builder.add_edge("plan", "act")
builder.add_edge("act", END)

# a run stops before "act" for a person, then resumes from its checkpoint, in a world
world = db.fork(f"lg-run-{uuid.uuid4().hex[:8]}")
graph = builder.compile(checkpointer=ChronosSaver(world), interrupt_before=["act"])
cfg = {"configurable": {"thread_id": f"t-{uuid.uuid4().hex[:8]}"}}  # main may have threads of runs before
graph.invoke({"steps": []}, cfg)
assert graph.get_state(cfg).next == ("act",)
note = cfg["configurable"]["thread_id"]
world.put("lg_notes", note, {"text": "written by the run"})

# a new saver on the same world, as a new process would make, picks the thread up
graph = builder.compile(checkpointer=ChronosSaver(world), interrupt_before=["act"])
assert graph.invoke(None, cfg)["steps"] == ["plan", "act"]
history = list(graph.get_state_history(cfg))
assert len(history) >= 3 and history[0].values["steps"] == ["plan", "act"], history

# main has neither the run's rows nor its state until the world merges; then both
assert db.get("lg_notes", note) is None
assert builder.compile(checkpointer=ChronosSaver(db)).get_state(cfg).values == {}
keys = {c["key"].split("/")[0] for c in world.diff()}
assert keys == {"lg_notes", "langgraph_checkpoints", "langgraph_writes"}, keys
world.merge()
assert db.get("lg_notes", note) is not None
assert builder.compile(checkpointer=ChronosSaver(db)).get_state(cfg).values["steps"] == ["plan", "act"]

# forking a world forks the thread: each copy goes on from the same checkpoint on its own
a, b = db.fork(f"lg-a-{uuid.uuid4().hex[:8]}"), db.fork(f"lg-b-{uuid.uuid4().hex[:8]}")
ga = builder.compile(checkpointer=ChronosSaver(a))
ga.update_state(cfg, {"steps": ["only in a"]})
assert ga.get_state(cfg).values["steps"][-1] == "only in a"
assert builder.compile(checkpointer=ChronosSaver(b)).get_state(cfg).values["steps"] == ["plan", "act"]
a.discard()
b.discard()

# the store: items by namespace, filters, text search, namespaces, deletes; atomic in its batch
w = db.fork(f"lg-store-{uuid.uuid4().hex[:8]}")
store = ChronosStore(w)
store.put(("users", "1", "memories"), "m1", {"text": "likes dark roast coffee", "kind": "pref", "n": 3})
store.put(("users", "1", "memories"), "m2", {"text": "prefers tea in the evening", "kind": "pref", "n": 5})
store.put(("users", "1", "facts"), "f1", {"text": "lives in Bengaluru", "kind": "fact"})
store.put(("users", "2", "memories"), "m1", {"text": "drinks coffee black", "kind": "pref"}, index=False)
item = store.get(("users", "1", "memories"), "m1")
assert item.value["n"] == 3 and item.namespace == ("users", "1", "memories")
first = item.created_at
store.put(("users", "1", "memories"), "m1", {"text": "likes dark roast coffee", "kind": "pref", "n": 4})
again = store.get(("users", "1", "memories"), "m1")
assert again.created_at == first and again.updated_at >= first and again.value["n"] == 4
assert {i.key for i in store.search(("users", "1"))} == {"m1", "m2", "f1"}
assert [i.key for i in store.search(("users", "1"), filter={"kind": "fact"})] == ["f1"]
assert [i.key for i in store.search(("users",), filter={"n": {"$gte": 5}})] == ["m2"]
hits = store.search(("users",), query="cofee")  # a typo; user 2's item isn't searchable (index=False)
assert [(i.namespace[1], i.key) for i in hits] == [("1", "m1")], hits
assert hits[0].score is not None
assert store.list_namespaces(prefix=("users",), max_depth=2) == [("users", "1"), ("users", "2")]
assert store.list_namespaces(suffix=("facts",)) == [("users", "1", "facts")]
store.delete(("users", "1", "facts"), "f1")
assert store.get(("users", "1", "facts"), "f1") is None
assert asyncio.run(store.aget(("users", "1", "memories"), "m2")).value["n"] == 5
w.discard()

try:
    from langgraph.checkpoint.conformance import checkpointer_test, validate
except ImportError:
    print("ok (conformance suite not installed)")
    sys.exit(0)


@checkpointer_test(name="ChronosSaver")
async def saver():
    w = db.fork(f"lg-conf-{uuid.uuid4().hex[:8]}")
    yield ChronosSaver(w)
    w.discard()


report = asyncio.run(validate(saver))
failed = {k: r.failures for k, r in report.results.items() if r.passed is False}
assert not failed, failed
print("ok")
