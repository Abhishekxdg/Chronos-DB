"""A pytest fixture for testing agents on a world: `chronos_world`, a fork of main discarded after the test.

    # pytest -p chronos_pytest  (or `pytest_plugins = ["chronos_pytest"]` in conftest.py)
    def test_agent_fixes_names(chronos_world):
        my_agent(chronos_world).run("fix the misspelt names")
        assert {c["key"] for c in chronos_world.diff()} == {"users/1", "users/7"}

Set CHRONOS_URL (and CHRONOS_TOKEN if the server has one); without it the fixture skips the test. Every test
gets its own world, so tests run in parallel (pytest-xdist) without seeing each other's writes, and main is never
changed unless a test merges.
"""

import os
import re
import uuid

import pytest

from chronos import Chronos, ChronosError


@pytest.fixture
def chronos_world(request):
    url = os.environ.get("CHRONOS_URL")
    if not url:
        pytest.skip("set CHRONOS_URL to a chronos serve")
    db = Chronos(url, token=os.environ.get("CHRONOS_TOKEN"))
    name = re.sub(r"[^a-z0-9]+", "-", request.node.name.lower()).strip("-")[:40]
    world = db.fork(f"test-{name}-{uuid.uuid4().hex[:6]}", meta={"test": request.node.nodeid})
    yield world
    try:
        world.discard()
    except ChronosError:  # the test merged it
        pass
