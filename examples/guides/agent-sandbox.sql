-- Give every agent its own world (docs/guides/agent-sandbox.md).
-- Against a fresh database:
--   CHRONOS_PROTECT_MAIN=off chronos serve shopdb   (this setup writes main directly)
--   psql -v ON_ERROR_STOP=1 -f examples/guides/agent-sandbox.sql postgres://127.0.0.1:5433/main
\set ON_ERROR_STOP 1

-- The shop, in main
create table products (
  sku   int primary key,
  name  text not null,
  price numeric(10, 2) not null,
  stock int not null
);
insert into products values
  (1, 'Espresso beans 1kg', 18.00, 40),
  (2, 'Filter papers (100)', 3.50, 900),
  (3, 'Burr grinder', 89.00, 150),
  (4, 'Milk jug', 14.00, 12),
  (5, 'Descaler', 7.00, 300);

-- 1. A world per agent
create world agent_a with (bot = 'pricer', task = 'discount slow movers 10%');
create world agent_b with (bot = 'copywriter', task = 'tidy names and prices');
create world agent_c with (bot = 'janitor', task = 'remove dead stock');
show worlds;

-- 2. Each agent works in its own world
switch world agent_a;
update products set price = price * 0.9 where stock > 100;

switch world agent_b;
update products set name = 'Burr grinder, 40 settings' where sku = 3;
update products set name = 'Milk jug 600 ml' where sku = 4;
update products set price = 6.99 where sku = 5;

switch world agent_c;
delete from products where stock < 20;

switch world main;
select * from products order by sku;   -- main is as it was

-- 3. What each agent did
diff world agent_a;
diff world agent_b;
diff world agent_c;
diff world agent_a as sql;

-- 4. Merge the best one
merge world agent_a dry run;
merge world agent_a;

-- 5. A conflict: agent_b and the merged agent_a both changed rows 3 and 5
merge world agent_b dry run;
merge world agent_b by columns resolve ('products/5' = theirs) dry run;
merge world agent_b by columns resolve ('products/5' = theirs);

-- 6. Drop the rest
drop world agent_c;
show worlds;
select * from products order by sku;
