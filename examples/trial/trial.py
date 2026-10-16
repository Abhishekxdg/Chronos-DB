"""The five-minute trial (see README.md): an agent works on a small CRM in its own worlds, and
three things it does wrong are caught before they reach `main`. Run it against `chronos serve`:

    CHRONOS_URL=http://127.0.0.1:7070 python3 trial.py

It prints what happens at each step and checks it, so it fails loudly if Chronos ever stops
catching one of them.
"""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "clients", "python"))
from chronos import Chronos, ChronosError  # noqa: E402

URL = os.environ.get("CHRONOS_URL", "http://127.0.0.1:7070")
person = Chronos(URL, token=os.environ.get("CHRONOS_TOKEN"))  # the database's own user: you


def step(title):
    print(f"\n== {title}")


def refused(call, *expect):
    """Runs `call`, which must be refused with a message holding every one of `expect`."""
    try:
        call()
    except ChronosError as e:
        msg = str(e)
        missing = [x for x in expect if x not in msg]
        assert not missing, f"refused, but without {missing}: {msg}"
        print(f"   refused: {msg}")
        return msg
    raise AssertionError(f"expected a refusal mentioning {expect}")


def rows(world, sql):
    return world.sql(sql)["rows"]


step("Set up: a CRM, a merge check, a merge policy, and an agent that keeps to it")
with open(os.path.join(os.path.dirname(__file__), "crm.sql")) as f:
    person.sql(f.read())
token = person.sql(
    "create agent crm_bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'careful')"
)["rows"][0]["token"]
bot = Chronos(URL, token=token)
print("   6 deals, 5 leads; merge check no_negative_amounts; policy careful; agent crm_bot")

step("0. main is protected: even you can't DELETE there directly")
refused(lambda: person.sql("delete from leads"), "main is protected")

step("1. The agent reads Ana's open deals and plans follow-ups")
w = bot.fork("followups")
plan = rows(w, "select id, account from deals where owner = 'ana' and stage = 'proposal' order by id")
for d in plan:
    w.sql("insert into tasks (deal, owner, note) values ($1, 'ana', $2)", [d["id"], f"Follow up with {d['account']}"])
print(f"   read {len(plan)} deals, wrote {len(plan)} tasks")
print("   meanwhile, someone moves one of BEN's deals: the agent never read it")
person.sql("create world fix_ben; switch world fix_ben; update deals set stage = 'won' where id = 4")
person.sql("merge world fix_ben")
w.merge()
print("   merged: a change to rows the agent didn't read doesn't hold its work")

step("2. The same again, but an ANA deal it read closes while it works")
w = bot.fork("followups2")
plan = rows(w, "select id, account from deals where owner = 'ana' and stage = 'proposal' order by id")
for d in plan:
    w.sql("insert into tasks (deal, owner, note) values ($1, 'ana', $2)", [d["id"], f"Chase {d['account']} again"])
person.sql("create world close3; switch world close3; update deals set stage = 'won' where id = 3")
person.sql("merge world close3")
refused(w.merge, "read rows that changed since it forked", "deals/3", "SHOW REVIEWS")
print("   held for a person: it planned a follow-up for a deal that's already won")

step("3. The agent cuts every proposal of Ben's by 1500, and one goes below zero")
w = bot.fork("discounts")
w.sql("update deals set amount = amount - 1500 where owner = 'ben' and stage = 'proposal'")
refused(w.merge, "no_negative_amounts", "Hooli")
print("   held: the merge check ran on the merged result and found the bad row")

step("4. The agent archives stale leads (a delete), and queues an email about it")
w = bot.fork("cleanup")
w.sql("delete from leads where last_contact < date '2025-01-01'")
w.sql("notify on merge email, 'archived 3 stale leads'")
refused(w.merge, "deletes 3 rows (at most 0)", "SHOW REVIEWS")
assert rows(person, "show outbox") == [], "nothing sent before the merge"
review = [r for r in rows(person, "show reviews") if r["world"] == "cleanup"][0]
print(f"   queued for you: {review['reasons']} (version {review['version']})")

step("5. You review it, but the agent writes again before you approve")
w.sql("delete from leads where id = 4")
refused(lambda: person.sql(f"merge world cleanup at version {review['version']}"), "it changed since")
print("   your approval named the version you read, so the agent's later delete didn't ride along")
now = person.branch("cleanup").diff_count()
print(f"   you read it again: {now['total']} changes, version {now['version']}; you approve those")
person.sql(f"merge world cleanup at version {now['version']}")
outbox = rows(person, "show outbox")
assert [o["payload"] for o in outbox] == ["archived 3 stale leads"], outbox
print(f"   merged; the email went out only now: outbox has {outbox[0]['payload']!r} ({outbox[0]['state']})")

step("Where things stand")
for r in rows(person, "show reviews"):
    print(f"   waiting for a person: {r['world']}: {r['reasons']}")
print("\ntrial: ok")
