// Prisma against Chronos, after `prisma db push` made the tables: nested writes, a relation read back, a unique
// violation's code, a transaction.
import assert from "node:assert/strict";
import { PrismaClient } from "@prisma/client";

const prisma = new PrismaClient();
await prisma.drvPost.deleteMany();
await prisma.drvUser.deleteMany();
const ada = await prisma.drvUser.create({
  data: { email: "ada@x.io", name: "Ada", posts: { create: [{ title: "one" }, { title: "two", done: true }] } },
  include: { posts: true },
});
assert.equal(ada.posts.length, 2);
await prisma.drvUser.create({ data: { email: "bob@x.io" } });
const users = await prisma.drvUser.findMany({ orderBy: { email: "asc" }, include: { _count: { select: { posts: true } } } });
assert.deepEqual(users.map((u) => [u.email, u.name, u._count.posts]), [["ada@x.io", "Ada", 2], ["bob@x.io", null, 0]]);
assert.equal(await prisma.drvPost.count({ where: { done: true } }), 1);
await prisma.drvPost.updateMany({ where: { authorId: ada.id }, data: { done: true } });
assert.equal(await prisma.drvPost.count({ where: { done: true } }), 2);
await assert.rejects(prisma.drvUser.create({ data: { email: "ada@x.io" } }), (e) => e.code === "P2002");
await assert.rejects(
  prisma.$transaction([
    prisma.drvUser.create({ data: { email: "cy@x.io" } }),
    prisma.drvUser.create({ data: { email: "bob@x.io" } }),
  ]),
);
assert.equal(await prisma.drvUser.count(), 2, "the transaction rolled back whole");
await prisma.$disconnect();
console.log("ok");
