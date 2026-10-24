// Many agents at once, the Chronos way: each forks its own branch (instantly), works without
// blocking anyone, then merges. Agents that touched the same rows as an earlier merge get a
// conflict back as data, and retry on a fresh fork.
//
//   CHRONOS_PROTECT_MAIN=off chronos serve demo-db &   (it seeds main directly)
//   CHRONOS_URL=http://127.0.0.1:7070 node parallel.ts

import { Chronos, ChronosError } from "../../clients/typescript/chronos.ts";

const db = new Chronos(process.env.CHRONOS_URL, { token: process.env.CHRONOS_TOKEN });
const AGENTS = 20;
const TASKS = 8;

// a shared task board
await db.batch(
  Array.from({ length: TASKS }, (_, i) => ({ table: "tasks", id: String(i), record: { title: `task ${i}`, votes: 0 } })),
);

// each agent reads two tasks on its own branch and upvotes them; overlapping agents conflict
async function agent(n: number): Promise<{ attempts: number }> {
  for (let attempt = 1; ; attempt++) {
    const me = await db.fork(`agent-${n}-${attempt}`);
    for (const id of [String(n % TASKS), String((n * 3 + 1) % TASKS)]) {
      const t = (await me.get<{ title: string; votes: number }>("tasks", id))!;
      await me.put("tasks", id, { ...t, votes: t.votes + 1 });
    }
    try {
      await me.merge();
      return { attempts: attempt };
    } catch (e) {
      if (!(e instanceof ChronosError) || e.status !== 409) throw e;
      await me.discard(); // someone merged the same tasks first: redo on a fresh fork
    }
  }
}

const t0 = performance.now();
const results = await Promise.all(Array.from({ length: AGENTS }, (_, n) => agent(n)));
const ms = performance.now() - t0;

const { hits } = await db.find<{ votes: number }>("tasks", { limit: TASKS });
const votes = hits.reduce((s, h) => s + h.row.votes, 0);
const retries = results.reduce((s, r) => s + r.attempts - 1, 0);
console.log(`${AGENTS} agents, ${retries} retries after conflicts, ${ms.toFixed(0)} ms`);
console.log(`votes recorded: ${votes} (expected ${AGENTS * 2}: no update lost)`);
if (votes !== AGENTS * 2) process.exit(1);
