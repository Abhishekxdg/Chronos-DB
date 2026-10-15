"""End-to-end check against a running `chronos serve` (CHRONOS_URL, CHRONOS_TOKEN). Run: python3 test_chronos.py"""
import os

# a proxy that isn't there: a server on this machine is reached directly, so the token never goes via a proxy
os.environ["http_proxy"] = "http://127.0.0.1:9"
os.environ.pop("no_proxy", None)

from chronos import Chronos, ChronosError

db = Chronos(os.environ["CHRONOS_URL"], token=os.environ.get("CHRONOS_TOKEN"))

db.batch([
    {"table": "notes", "id": "a", "record": {"text": "Bengaluru office opening", "city": "blr"}},
    {"table": "notes", "id": "b", "record": {"text": "Mumbai office", "city": "bom"}},
])
assert db.get("notes", "a") == {"city": "blr", "text": "Bengaluru office opening"}
assert db.get("notes", "zzz") is None

found = db.find("notes", text="Bangaluru")  # one typo
assert found["total"] == 1 and found["hits"][0]["id"] == "a", found  # only "a" has the word

agent = db.fork("py-agent")
agent.put("notes", "c", {"text": "Delhi office", "city": "del"})
assert [c["key"] for c in agent.diff()] == ["notes/c"]
assert db.get("notes", "c") is None
assert agent.merge() == 1
assert db.get("notes", "c")["city"] == "del"

scratch = db.fork("scratch", meta={"owner": "py", "task": 7})
w = scratch.world()
assert w["parent"] == "main" and w["depth"] == 1 and w["meta"] == {"owner": "py", "task": 7}, w
assert scratch.set_meta({"task": None, "note": "tmp"})["meta"] == {"owner": "py", "note": "tmp"}
assert [b["id"] for b in db.branches() if b["name"] == "scratch"] == [w["id"]]
scratch.delete("notes", "a")
scratch.discard()
assert db.get("notes", "a") is not None

# SQL over the same rows: a table without a schema has id plus its rows' fields
assert db.sql("select id, city from notes where city = $1", ["del"])["rows"] == [{"id": "c", "city": "del"}]
sql_agent = db.fork("py-sql")
assert sql_agent.sql("update notes set city = 'DEL' where id = 'c'")["command"] == "UPDATE 1"
assert sql_agent.merge() == 1
assert db.get("notes", "c")["city"] == "DEL"

try:
    db.branch("ghost").get("notes", "a")
    raise AssertionError("expected 404")
except ChronosError as e:
    assert e.status == 404, e
# time travel: read the past, see history, restore
import time
db.put("notes", "tt", {"v": 1})
time.sleep(0.02)
then = db.sql("select now()::text as t")["rows"][0]["t"]
time.sleep(0.02)
db.put("notes", "tt", {"v": 2})
assert db.at(then).get("notes", "tt")["v"] == 1
assert db.get("notes", "tt")["v"] == 2
assert db.history(limit=5)[0]["event"] == "write"
assert db.restore(then) >= 1 and db.get("notes", "tt")["v"] == 1
# merge engine: preview, then settle by columns and picks
db.put("notes", "m", {"a": 1, "b": 1})
m = db.fork("merge-me")
m.put("notes", "m", {"a": 2, "b": 1})
db.put("notes", "m", {"a": 1, "b": 2})
p = m.preview()
assert p["conflicts"] == 1 and "different columns" in p["rows"][0]["detail"], p
assert m.preview(columns=True)["rows"][0]["outcome"] == "by columns"
assert m.merge(columns=True) == 1 and db.get("notes", "m") == {"a": 2, "b": 2}
# merge part of a branch, or into another branch: the branch stays
part = db.fork("part-me")
part.put("notes", "p1", {"v": 1})
part.put("notes", "p2", {"v": 2})
other = db.fork("other")
assert part.preview(only_keys=["notes/p1"])["rows"][0]["key"] == "notes/p1"
assert part.merge(into="other", only_keys=["notes/p2"]) == 1 and other.get("notes", "p2") == {"v": 2}
assert part.merge(only_keys=["notes/p1"]) == 1 and db.get("notes", "p1") == {"v": 1} and db.get("notes", "p2") is None
assert [c["key"] for c in part.diff()] == ["notes/p2"]
assert part.merge() == 1 and db.get("notes", "p2") == {"v": 2}
other.discard()

# simulate: many worlds, the best kept; replay one from history
db.sql("create table stock (id integer primary key, qty integer)")
db.sql("insert into stock values (1, 10), (2, 20)")
script = "update stock set qty = qty + floor(random() * 10) + $1"
sim = db.simulate(8, "try", script, "select sum(qty) from stock", keep=2, seed=7)
assert len(sim["worlds"]) == 8 and [w["kept"] for w in sim["worlds"]] == [True] * 2 + [False] * 6, sim
again = db.simulate(8, "try2", script, "select sum(qty) from stock", keep=2, seed=7)
assert [w["score"] for w in again["worlds"]] == [w["score"] for w in sim["worlds"]]
assert db.replay(sim["worlds"][0]["world"])["identical"]
print("python client: ok")
