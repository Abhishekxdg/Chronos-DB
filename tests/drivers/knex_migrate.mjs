// Knex migrations (over node-postgres) against Chronos: up to latest, all the way down and up again. Knex runs each
// migration in a transaction and keeps its place in knex_migrations, guarded by a row in knex_migrations_lock.
import knexFactory from "knex";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import assert from "node:assert/strict";

const dir = mkdtempSync(join(tmpdir(), "knex-"));
writeFileSync(join(dir, "001_accounts.cjs"), `
exports.up = (k) => k.schema.createTable("drv_knex_account", (t) => {
  t.increments("id");
  t.string("email", 255).notNullable().unique();
  t.timestamp("created_at", { useTz: true }).notNullable().defaultTo(k.fn.now());
});
exports.down = (k) => k.schema.dropTable("drv_knex_account");
`);
writeFileSync(join(dir, "002_transfers.cjs"), `
exports.up = async (k) => {
  await k.schema.alterTable("drv_knex_account", (t) => t.decimal("balance", 12, 2).notNullable().defaultTo(0));
  await k.schema.createTable("drv_knex_transfer", (t) => {
    t.increments("id");
    t.integer("account_id").notNullable().references("id").inTable("drv_knex_account").onDelete("CASCADE");
    t.decimal("amount", 12, 2).notNullable();
    t.index(["account_id"]);
  });
  await k("drv_knex_account").insert([{ email: "ada@example.com" }, { email: "bo@example.com" }]);
};
exports.down = async (k) => {
  await k.schema.dropTable("drv_knex_transfer");
  await k.schema.alterTable("drv_knex_account", (t) => t.dropColumn("balance"));
};
`);
writeFileSync(join(dir, "003_rename.cjs"), `
exports.up = (k) => k.schema.alterTable("drv_knex_account", (t) => t.renameColumn("email", "address"));
exports.down = (k) => k.schema.alterTable("drv_knex_account", (t) => t.renameColumn("address", "email"));
`);

const knex = knexFactory({
  client: "pg",
  connection: process.env.CHRONOS_URL ?? "postgres://tester@127.0.0.1:5433/main",
  migrations: { directory: dir, loadExtensions: [".cjs"] },
});
try {
  // a rerun starts from nothing
  await knex.raw("drop table if exists drv_knex_transfer, drv_knex_account, knex_migrations, knex_migrations_lock");
  const [, up] = await knex.migrate.latest();
  assert.equal(up.length, 3, String(up));
  assert.deepEqual(await knex.migrate.currentVersion(), "003");
  const rows = await knex("drv_knex_account").select("id", "address", "balance").orderBy("id");
  assert.deepEqual(rows.map((r) => [r.address, r.balance]), [["ada@example.com", "0.00"], ["bo@example.com", "0.00"]]);
  await knex("drv_knex_transfer").insert({ account_id: rows[0].id, amount: 5 });
  // down one, down to nothing, and up again
  await knex.migrate.down();
  assert.equal(await knex.migrate.currentVersion(), "002");
  assert.ok(await knex.schema.hasColumn("drv_knex_account", "email"));
  await knex.migrate.rollback(undefined, true);
  assert.equal(await knex.migrate.currentVersion(), "none");
  assert.ok(!(await knex.schema.hasTable("drv_knex_account")));
  await knex.migrate.latest();
  assert.equal(await knex.migrate.currentVersion(), "003");
  const [{ n }] = await knex("knex_migrations").count({ n: "*" });
  assert.equal(Number(n), 3);
  console.log("knex: ok");
} finally {
  await knex.destroy();
}
