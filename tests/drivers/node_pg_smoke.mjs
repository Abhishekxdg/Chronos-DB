// node-postgres against Chronos: typed parameters, a named prepared statement, a transaction, a pool, an error's code.
import assert from "node:assert/strict";
import pg from "pg";

const url = process.env.CHRONOS_URL ?? "postgres://tester@127.0.0.1:5433/main";
const client = new pg.Client({ connectionString: url });
await client.connect();
await client.query("drop table if exists drv_node");
await client.query(
  "create table drv_node (id int primary key, name text not null unique, score numeric(10,2), ok boolean, at timestamptz, doc jsonb, tags text[])",
);
const at = new Date("2026-01-02T03:04:05Z");
await client.query("insert into drv_node values ($1, $2, $3, $4, $5, $6, $7)", [1, "Ada", "9.50", true, at, { a: [1, 2] }, ["x", "y"]]);
await client.query("insert into drv_node (id, name) values ($1, $2)", [2, "Bob"]);
const { rows } = await client.query("select * from drv_node order by id");
assert.equal(rows.length, 2);
assert.deepEqual(
  { ...rows[0], at: rows[0].at.toISOString() },
  { id: 1, name: "Ada", score: "9.50", ok: true, at: at.toISOString(), doc: { a: [1, 2] }, tags: ["x", "y"] },
);
assert.deepEqual(rows[1], { id: 2, name: "Bob", score: null, ok: null, at: null, doc: null, tags: null });

// a named statement is prepared once and run again
for (const [id, name] of [[1, "Ada"], [2, "Bob"]]) {
  const r = await client.query({ name: "by-id", text: "select name from drv_node where id = $1", values: [id] });
  assert.equal(r.rows[0].name, name);
}

await client.query("begin");
await client.query("insert into drv_node (id, name) values (100, 'gone')");
await client.query("rollback");
await client.query("begin");
await client.query("insert into drv_node (id, name) values (101, 'kept')");
await client.query("commit");
assert.deepEqual((await client.query("select id from drv_node where id >= 100")).rows, [{ id: 101 }]);

await assert.rejects(client.query("insert into drv_node (id, name) values (3, 'Ada')"), (e) => e.code === "23505");
// (the connection is still good after an error)
assert.equal((await client.query("select count(*)::int as n from drv_node")).rows[0].n, 3);
await client.end();

const pool = new pg.Pool({ connectionString: url, max: 4 });
const counts = await Promise.all([1, 2, 3, 4, 5, 6].map((i) => pool.query("select $1::int * 2 as n", [i])));
assert.deepEqual(counts.map((r) => r.rows[0].n), [2, 4, 6, 8, 10, 12]);
await pool.query("drop table drv_node");
await pool.end();
console.log("ok");
