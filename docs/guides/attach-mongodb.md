# Attach beside MongoDB

`chronos attach` also works beside MongoDB 5.0 or later. The promise is the same as with Postgres and MySQL: main
is a copy of the collections you name, agents work in worlds, and a merge into main is written to MongoDB first, in
one multi-document transaction. Each document it writes must still be as main has it. If MongoDB changed one
meanwhile, the merge comes back as a conflict and nothing is written.

The released binaries have MongoDB in them, from 0.5.0. A build of your own needs `cargo build --features mongo`;
one without it says so when asked.

```bash
export CHRONOS_SOURCE_URL='mongodb://chronos:secret@db.internal:27017/shop?replicaSet=rs0&tls=true'
chronos attach shop --tables customers,orders --fields org_id
chronos serve shop
```

The URL is MongoDB's own connection string (`mongodb://` or `mongodb+srv://`) and must name the database. Its
collections are the ones attached.

## It needs a replica set

MongoDB has transactions only on a replica set (Atlas is one). A standalone `mongod` is refused at attach, with
the reason: a merge couldn't be written all or nothing. One member is enough (`mongod --replSet rs0`, then
`rs.initiate()`). A sharded cluster (`mongos`) is refused for now.

## A collection is a table, a document a row

| column | holds |
|---|---|
| `_id` | the document's `_id`: an ObjectId as its 24 hex digits, any other id as JSON (`"abc"`, `7`) |
| one per `--fields` name | that top-level field, as a column of its own (a tenant's id, say) |
| `doc` | the rest of the document, as `jsonb` |

```sql
select doc->>'name' from customers where org_id = 42;
update customers set doc = jsonb_set(doc, '{owner}', '"ada"') where _id = '65f1a2b3c4d5e6f708192a3b';
insert into customers (org_id, doc) values (42, '{"name": "NewCo"}');   -- _id is a new ObjectId
delete from customers where _id = '"lead-2"';                            -- a string id, with its quotes
```

- Values are MongoDB's relaxed Extended JSON: an ObjectId inside `doc` is `{"$oid": "..."}`, a date
  `{"$date": "2026-10-01T10:00:00Z"}`, a Decimal128 `{"$numberDecimal": "1.50"}`. Numbers are plain.
- A field named by `--fields` must hold one type throughout the collection: string, int, long or ObjectId.
  Documents without it have NULL. It isn't in `doc`; set the column.
- A new row's `_id` defaults to a new ObjectId. Give your own by setting `_id`.

## A merge

1. A transaction starts. The folder's marker in `chronos_merges` is checked.
2. Every document the merge writes is read and compared with main's copy. Any difference is a conflict: nothing is
   written, main takes MongoDB's document, and the merge is settled as any conflict. Numbers compare by value and
   objects whatever their fields' order.
3. Inserts and updates, then deletes. **An update sends only the fields that changed**, as `$set` and `$unset` by
   path, never the whole document: the fields it leaves keep their order and their types.
4. The documents are read back, the merge is recorded in `chronos_merges`, and the transaction commits. If the
   connection is lost during the commit, `chronos_merges` tells whether it happened, at the next merge.

A document another writer changes between step 2 and step 3 fails the write: MongoDB aborts the transaction, and
the merge says it is safe to run again.

### Types

`jsonb` has one kind of number; MongoDB has three. The rule:

- A number written to a field that already holds one keeps that field's type. A double stays a double when you
  write `9`; a long stays a long.
- In a new field, a whole number is an int (a long past 32 bits), and a number with a fraction part is a double.
- To write another type, write its Extended JSON: `{"$numberLong": "5"}`, `{"$numberDecimal": "1.50"}`,
  `{"$date": "..."}`, `{"$oid": "..."}`.

### Arrays

An array is one value. A change to an array writes the whole array, so two changes to the same array are a
conflict even when they touch different elements.

## Refresh

`chronos refresh shop` (or `serve --refresh-every`) reads each collection whole in one snapshot and gives main
MongoDB's inserts, updates and deletes.

## Undo by merge id

Each write-back keeps the documents it wrote, before and after, in `chronos_merge_rows`, in its transaction. A
merge can be undone by its id for 30 days, as [beside Postgres](attach-postgres.md#undo-by-merge-id). MongoDB has no
cascades to hold an undo for.

## Dry runs and checked reads

`MERGE WORLD w DRY RUN` (the HTTP API's `merge_preview`) reads MongoDB in one snapshot: each document the merge
would write is compared with MongoDB's as the write-back compares it, and nothing is written or locked.

A world forked `WITH (check_reads = true)` has what it read checked against MongoDB too: inside the write-back's
transaction before anything is written, and by `CHECK READS` between steps. A document it read that MongoDB changed
since, or one added there that its read would have found, holds the merge.

- A read by `_id` is checked by that document alone.
- A read with a condition on a field kept as a column (`--fields`) is checked by the documents the condition can
  match.
- A read with a condition on a field inside `doc` counts as a read of the whole collection: any document added or
  changed there holds the merge. Name the fields an agent filters by in `--fields`.
- A collection read whole is read whole from MongoDB, 100,000 documents at most; past that the merge is held for a
  person.
- A world that read for one tenant is held by that tenant's documents only, when the tenant's field is one of
  `--fields`.

## What attach refuses

Each named with the reason, and nothing copied: a standalone server, a sharded cluster, MongoDB older than 5.0,
views, time-series and capped collections, a document over 4 MB, a `--fields` field of mixed types.

Field names with a dot or a leading `$` can't be written by a merge.

## Not yet with MongoDB

Each of these is refused with a message that says so: `serve --follow` (change streams), rehearsals, merge
checks run in the source, `writeback.json`, `pg_role`, and `--lazy-tenant`. The HTTP API's
[`attached`](../http-api.md) says `kind: "mongodb"`, `dry_run` and `check_reads` as true, and each other capability
as false.

## The user it connects as

`readWrite` on the database is enough. It makes two collections there: `chronos_merges` and `chronos_merge_rows`.
