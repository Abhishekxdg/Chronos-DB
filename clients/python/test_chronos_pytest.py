"""The `chronos_world` fixture, against a running `chronos serve` (CHRONOS_URL). Run: python3 test_chronos_pytest.py.
Skipped without pytest."""
import os
import sys
import tempfile

try:
    import pytest
except ImportError:
    print("skipped: pytest not installed")
    sys.exit(0)

from chronos import Chronos

TESTS = '''
import os
import pytest
from chronos import Chronos

def test_writes_stay_in_the_world(chronos_world):
    chronos_world.put("fixture", "1", {"v": 1})
    assert [c["key"] for c in chronos_world.diff()] == ["fixture/1"]
    assert Chronos(os.environ["CHRONOS_URL"], token=os.environ.get("CHRONOS_TOKEN")).get("fixture", "1") is None

@pytest.mark.parametrize("n", [1, 2])
def test_a_merged_world_tears_down(chronos_world, n):
    chronos_world.put("fixture", f"m{n}", {"v": n})
    chronos_world.merge()
'''

db = Chronos(os.environ["CHRONOS_URL"], token=os.environ.get("CHRONOS_TOKEN"))
before = {b["name"] for b in db.branches()}
with tempfile.TemporaryDirectory() as d:
    path = os.path.join(d, "test_agent.py")
    with open(path, "w") as f:
        f.write(TESTS)
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    assert pytest.main(["-q", "-p", "chronos_pytest", "-p", "no:cacheprovider", path]) == 0
assert {b["name"] for b in db.branches()} == before  # every test's world is gone
assert db.get("fixture", "m1") and db.get("fixture", "m2") and db.get("fixture", "1") is None
print("ok")
