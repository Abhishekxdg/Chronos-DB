# @chronosdb/client

The TypeScript/JavaScript client for [Chronos DB](https://github.com/Abhishekxdg/chronosdb)'s HTTP API (`chronos serve`): give every agent its own **world**, a copy-on-write branch of the whole database, then diff and merge what it did. No dependencies; needs a global `fetch` (Node 18+, browsers, edge runtimes).

```bash
npm install @chronosdb/client
```

```ts
import { Chronos } from "@chronosdb/client";

const db = new Chronos("http://127.0.0.1:7070", { token: process.env.CHRONOS_TOKEN });
const agent = await db.fork("agent-7");            // instant copy of main
await agent.put("users", "1", { name: "Ada L." });
console.log(await agent.diff());
await agent.merge();                                // or agent.discard()
```

Every method, its options and its errors: [the client reference](https://abhishekxdg.github.io/chronosdb/reference/clients). The client speaks the HTTP API, not the Postgres port: for Postgres drivers (`pg`, Prisma, Drizzle, ...) see [Postgres compatibility](https://abhishekxdg.github.io/chronosdb/postgres-compatibility).

MIT licensed.
