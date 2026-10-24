---
title: Simulate a thousand futures and merge one
---

You'll put a small pricing model in `main`, let `SIMULATE` try a thousand futures of it (each a world with its own prices and its own random demand), score each future in SQL, look inside the best ones, check that one comes out the same when replayed, and merge its prices into `main`. One statement does the forking, running, scoring and cleanup.

The whole script is [examples/guides/scenario-planning.sql](https://github.com/Abhishekxdg/chronosdb/blob/main/examples/guides/scenario-planning.sql).

## Setup

```bash
CHRONOS_PROTECT_MAIN=off chronos serve plandb   # the setup below writes main directly
psql -v ON_ERROR_STOP=1 -f examples/guides/scenario-planning.sql postgres://127.0.0.1:5433/main
```

Use a database on disk, as `chronos serve` does: `REPLAY WORLD` rebuilds a world from history, and an in-memory database keeps none.

The model: five products, each with a cost, today's price, weekly demand at that price, and an elasticity (how fast demand falls as the price rises). `weeks` holds 20 weeks to sample demand over; `forecast` and `run_info` start empty and get filled inside each future.

```sql
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

select sum((list_price - cost) * demand) as weekly_profit_today from products;   -- 9,940
```

The simulation forks from a world of its own, `pricing`, so one `DROP WORLD pricing CASCADE` clears everything it keeps:

```sql
create world pricing with (task = 'next quarter prices');
```

## 1. Run a thousand futures

```sql
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
```

The grammar is `SIMULATE n WORLDS [FROM world] AS prefix RUN script SCORE query`, then any of `ASC | DESC`, `KEEP k | KEEP ALL`, `SEED s`, `THREADS t`, in any order.

- **What each world does:** it's forked from `pricing` (from the session's world without `FROM`; `FROM 'main@-1 hour'` starts from the past). The script tries a price within 20% of today's for each product, then samples 20 weeks of demand with ±30% noise. The score query gives its average weekly profit: one SELECT whose first value is a number.
- **`$1` and `$2`:** in the script and the score query, `$1` is the world's index (0 to 999) and `$2` its seed. `run_info` records both.
- **Random, repeatably:** in each world, `random()` and `gen_random_uuid()` follow `SEED` (0 if not given) and the world's index, and `now()` is the base moment. The same statement from the same moment gives the same worlds and scores on any machine and any number of threads. `serial` numbers are the exception: every world shares the database's counters.
- **Keep:** the highest score is best (`ASC` for the lowest). `KEEP 10` keeps the best 10, named `trial_<index>`, and discards the rest as it goes: worlds are forked 1,024 at a time, and the losers go after each batch. `KEEP ALL`, the default, keeps every world.
- **What it can't do:** a script can't fork, merge, restore, switch or drop worlds, or change settings (0A000). A world whose script or score fails is reported with its error, and the rest carry on.

It returns one row per world, 1,000 in all, best first (failed worlds last): `world`, `id`, `index`, `seed`, `score`, `error`, `kept`. The first 10 have `kept = t`.

SQL can't select from `SIMULATE`'s result, so the script saves it with psql's `\o` and reads the names of the best and the tenth-best worlds from the file (`\set best` with a shell command in backquotes). By hand, you'd read them off the screen.

## 2. Look inside the best worlds

Each kept world is a world like any other:

```sql
switch world trial_<best index>;
select sku, name, list_price, round(price::numeric, 2) as price,
       round(((price / list_price - 1) * 100)::numeric, 1) as pct
from products order by sku;
select * from run_info;   -- its index ($1) and seed ($2)
switch world main;
```

What to expect: for a constant elasticity e > 1, profit peaks at a price of cost × e / (e − 1). That's above today's price, and above the +20% the script allows, for the beans, the filter papers, the jug and the descaler; for the grinder it's 75, about 16% below today's 89. But 1,000 random tries over five prices, scored on noisy demand, find the big levers and not the small ones. With `SEED 42` the best world is `trial_137`, scoring 10,898 a week against 9,940 today:

```text
 sku |        name        | list_price | price |  pct
-----+--------------------+------------+-------+-------
   1 | Espresso beans 1kg |         18 | 20.77 |  15.4
   2 | Filter papers      |          3 |  3.11 |   3.8
   3 | Burr grinder       |         89 | 75.68 | -15.0
   4 | Milk jug           |         14 | 13.76 |  -1.7
   5 | Descaler           |          7 |  6.74 |  -3.7
```

The beans went up and the grinder came down almost to its best price; the other three barely matter to profit, so where they land is mostly noise. Compare the tenth-best world, and the last row of the result (the worst of all 1,000, already discarded).

Kept worlds also record what made them, in their metadata (`meta` in `SHOW WORLDS`, key `sim`): the seed, index, script, score query, score, and the base world and exact moment.

## 3. Check it replays

```sql
replay world trial_<best index>;
-- world, replay, identical, rows_differing, score, recorded_score, error
```

`REPLAY WORLD` forks the base as it was at the simulation's moment (from history), runs the script and score again with the same index and seed, and says whether the rows and the score are identical: `identical = t`, `rows_differing = 0`. The replay is thrown away, unless `AS name` keeps it. It needs history back to that moment: not an in-memory database, nor past the retention window (30 days by default).

## 4. Merge the chosen future

The best world holds its prices and also its simulated `forecast` and `run_info` rows, which don't belong in `main`. Merge only the products table, straight into `main`:

```sql
merge world trial_<best index> into main only tables (products) dry run;   -- 5 rows, all apply
merge world trial_<best index> into main only tables (products);
drop world pricing cascade;   -- pricing and every world kept under it
select sku, name, list_price, round(price::numeric, 2) as new_price from products order by sku;
select count(*) as forecast_rows_in_main from forecast;   -- 0
```

`INTO main` merges three-way against the world's fork point, and `ONLY TABLES` takes just those tables and leaves the world open. `main`'s `list_price` is still today's; `price` is the plan.

## What just happened

- One statement forked 1,000 worlds, ran a script in each on every core, scored each with a query, and kept 10.
- The randomness was repeatable: seeded per world, checked by `REPLAY WORLD`.
- The chosen future's prices reached `main` with a partial merge; its simulated data didn't.

A caveat: the best of 1,000 noisy scores is partly luck. Averaging 20 weeks per world, as here, damps it; a stricter test scores each price set on several independent samples.

## The big version

[BENCHMARKS.md §10](https://github.com/Abhishekxdg/chronosdb/blob/main/BENCHMARKS.md#10-the-ultimate-test-one-real-state-100000-worlds-a-learning-loop-one-merge) runs a supply-chain model the same way, driven from Rust ([examples/simulate.rs](https://github.com/Abhishekxdg/chronosdb/blob/main/examples/simulate.rs)): three rounds of 100,000 worlds, 300,000 futures in all, each stepping every product through the days with a `WITH RECURSIVE` query, the best 1% of each round forked again, and the winner's policy merged into `main`.

## Next steps

- [SQL: simulations](../sql.md#simulations): every option, and what agents' quotas do to a simulation.
- [Concepts: simulations](../concepts.md#simulations) and [time travel](../concepts.md#time-travel).
- [Agent evaluation](agent-evaluation.md): grade worlds against an expected state instead of a score.
- [Agent sandbox](agent-sandbox.md): merging, conflicts and safe mode.
