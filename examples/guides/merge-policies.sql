-- Let agents merge on their own, within rules (docs/guides/merge-policies.md).
-- Against a fresh database:
--   CHRONOS_PROTECT_MAIN=off chronos serve shopdb   (this setup writes main directly)
--   psql -v ON_ERROR_STOP=1 -f examples/guides/merge-policies.sql postgres://127.0.0.1:5433/main
\set ON_ERROR_STOP 1
-- how to come back as yourself after acting as the agent
\set me 'host=' :HOST ' port=' :PORT ' dbname=main user=' :USER

-- The shop, in main
create table orders (id int primary key, customer text not null, status text not null, total numeric(10, 2));
create table payments (id int primary key, order_id int not null, amount numeric(10, 2) not null);
insert into orders values
  (1, 'ada', 'new', 20.00), (2, 'bo', 'new', 35.50), (3, 'cy', 'paid', 12.00),
  (4, 'di', 'paid', 99.00), (5, 'ed', 'new', 8.25), (6, 'flo', 'shipped', 41.00);
insert into payments values (1, 3, 12.00), (2, 4, 99.00);

-- 1. The rules: small changes merge on their own; bulk changes, deletes and money wait for you
create merge policy careful with (max_rows = 3, max_deletes = 0, review_tables = 'payments');
show merge policies;

-- 2. An agent that may merge its own worlds into main, within those rules
create agent bot with (can = 'read,fork,write_own,write_main,merge_own', policy = 'careful') \gset bot_
\set bot 'host=' :HOST ' port=' :PORT ' dbname=main user=bot password=' :bot_token

-- 3. As the agent: a small fix merges on its own
\connect :bot
create world bot_fix with (task = 'mark paid orders shipped');
switch world bot_fix;
update orders set status = 'shipped' where status = 'paid';
switch world main;
merge world bot_fix;

-- 4. As the agent: a bulk change and a refund wait for a person
create world bot_bulk with (task = 'archive every order');
switch world bot_bulk;
update orders set status = 'archived';
switch world main;
merge world bot_bulk dry run;   -- the last row says why it's blocked
\set ON_ERROR_STOP 0
merge world bot_bulk;           -- ERROR: merging bot_bulk needs a person's review ...
\set ON_ERROR_STOP 1

create world bot_refund with (task = 'refund order 4');
switch world bot_refund;
insert into payments values (3, 4, -99.00);
switch world main;
\set ON_ERROR_STOP 0
merge world bot_refund;         -- ERROR: ... it changes payments (always reviewed) ...
\set ON_ERROR_STOP 1

-- 5. Back as yourself: the queue
\connect :me
show reviews;
diff world bot_refund;
merge world bot_refund;         -- approved
drop world bot_bulk;            -- rejected
show reviews;

-- 6. Loosen a rule, and see who did what
alter merge policy careful set (max_rows = 100);
show merge policies;
show audit for agent bot limit 10;
select * from orders order by id;
