-- Plan prices with a thousand simulated futures (docs/guides/scenario-planning.md).
-- Against a fresh database, on disk (REPLAY needs history):
--   CHRONOS_PROTECT_MAIN=off chronos serve plandb   (this setup writes main directly)
--   psql -v ON_ERROR_STOP=1 -f examples/guides/scenario-planning.sql postgres://127.0.0.1:5433/main
-- Writes /tmp/chronos-scenario-sim.txt (the simulation's result) and reads kept worlds' names from it.
\set ON_ERROR_STOP 1

-- The model, in main
create table products (
  sku        int primary key,
  name       text not null,
  cost       double precision not null,
  list_price double precision not null,  -- today's price
  price      double precision not null,  -- the price a world tries
  demand     double precision not null,  -- units a week at today's price
  elasticity double precision not null   -- 2: a 1% higher price sells about 2% fewer
);
insert into products values
  (1, 'Espresso beans 1kg', 11, 18, 18, 400, 1.8),
  (2, 'Filter papers',       1,  3,  3, 900, 1.2),
  (3, 'Burr grinder',       45, 89, 89,  60, 2.5),
  (4, 'Milk jug',            6, 14, 14, 150, 1.5),
  (5, 'Descaler',            2,  7,  7, 300, 1.1);
create table weeks (week int primary key);
insert into weeks select g from generate_series(1, 20) g;
create table forecast (sku int, week int, sold double precision not null, primary key (sku, week));
create table run_info (id int primary key, world_index bigint, seed bigint);

-- today's prices, without noise
select sum((list_price - cost) * demand) as weekly_profit_today from products;

-- 1. A world to plan in; the simulation's worlds are forked from it
create world pricing with (task = 'next quarter prices');

-- 2. A thousand futures
\pset format unaligned
\pset tuples_only on
\o /tmp/chronos-scenario-sim.txt
simulate 1000 worlds from pricing as trial
  run $$
    update products set price = list_price * (0.8 + random() * 0.4);
    insert into forecast
      select p.sku, w.week,
             p.demand * power(p.price / p.list_price, -p.elasticity) * (0.7 + random() * 0.6)
      from products p, weeks w;
    insert into run_info values (1, $1, $2)
  $$
  score $$
    select sum((p.price - p.cost) * f.sold) / 20
    from products p join forecast f on f.sku = p.sku
  $$
  keep 10 seed 42;
\o
\pset format aligned
\pset tuples_only off

-- 3. The best 10 (kept), and the worst of all 1,000 (discarded)
\! head -n 10 /tmp/chronos-scenario-sim.txt
\! tail -n 1 /tmp/chronos-scenario-sim.txt
\set best `head -n 1 /tmp/chronos-scenario-sim.txt | cut -d '|' -f 1`
\set tenth `sed -n 10p /tmp/chronos-scenario-sim.txt | cut -d '|' -f 1`
\echo best: :best  tenth: :tenth

-- 4. Inside the best and the tenth-best world
switch world :'best';
select sku, name, list_price, round(price::numeric, 2) as price,
       round(((price / list_price - 1) * 100)::numeric, 1) as pct
from products order by sku;
select * from run_info;
switch world :'tenth';
select sku, name, list_price, round(price::numeric, 2) as price from products order by sku;
switch world main;

-- 5. The same inputs give the same world
replay world :'best';

-- 6. Take the best world's prices into main, and nothing else
merge world :'best' into main only tables (products) dry run;
merge world :'best' into main only tables (products);
drop world pricing cascade;
select sku, name, list_price, round(price::numeric, 2) as new_price from products order by sku;
select count(*) as forecast_rows_in_main from forecast;
show worlds;
