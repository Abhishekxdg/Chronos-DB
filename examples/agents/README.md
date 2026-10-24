# Agents on Chronos

The pattern is the same whatever the framework: every agent works on **its own fork**, which is instant and private. When it's done, it **merges**. If another agent changed the same rows first, the merge returns the conflicts as data, and the agent decides: keep its version, keep main's, or redo the work on a fresh fork.

## Claude Code (MCP)

```bash
claude mcp add chronos -- chronos mcp /path/to/mydb
```

The server's instructions teach the workflow: `describe`, then `fork`, then edit, then `diff`, then a person merges (or the agent calls `discard`). `merge` is offered only with `--allow-merge`, or to an agent account with the right (`--agent NAME`); the instructions then tell the agent to merge its own branch, and to stop and report if a merge policy holds it for a person. Try asking it to "Fork a branch, clean up the duplicate rows in `contacts`, show me the diff, then merge."

## Any LLM API with tool calling

- **[`tools.json`](tools.json):** 8 tool definitions (`chronos_fork`, `chronos_find`, `chronos_get`, `chronos_put`, `chronos_delete`, `chronos_diff`, `chronos_merge`, `chronos_discard`) with JSON Schemas. They're in the `name` / `description` / `input_schema` shape. Rename `input_schema` to `parameters` for APIs that use that field name.
- **[`tools.py`](tools.py):** `run_tool(db, name, args)` runs a tool call against `chronos serve` and returns JSON text for the model.
  - Errors, including merge conflicts, come back as text the model can act on.
  - It keeps one client per branch, so merges send the branch version and catch writes lost in a crash.

```python
import json, tools
from chronos import Chronos

db = Chronos("http://127.0.0.1:7070")
definitions = json.load(open("tools.json"))
# in your agent loop, for each tool call the model makes:
result = tools.run_tool(db, call.name, call.input)   # send `result` back as the tool result
```

## Many agents at once

[`parallel.ts`](parallel.ts) runs 20 agents that each fork, upvote two shared tasks and merge. Agents that collide get a conflict and redo their work on a fresh fork, so no vote is lost:

```
CHRONOS_PROTECT_MAIN=off chronos serve demo-db &   # the example seeds main directly
CHRONOS_URL=http://127.0.0.1:7070 node parallel.ts
# 20 agents, 42 retries after conflicts, 90 ms
# votes recorded: 40 (expected 40: no update lost)
```

Both examples run in CI (`tests/clients.rs`).
