# SQL

Chronos runs SQL itself (its own parser and executor) and speaks the Postgres protocol, so `psql`, GUI tools and Postgres drivers connect as they would to Postgres.

```bash
chronos serve mydb                            # HTTP on 7070, Postgres protocol on 5433
psql postgres://127.0.0.1:5433/main         # the database name is the branch
```

SQL also works in the shell (`chronos mydb`, end statements with `;` to spread them over lines), over HTTP (`POST /v1/sql` with `{"sql": "...", "params": [...]}`), through the clients (`db.sql(...)`), and as the MCP tool `sql`.

```sql
create table users (id bigint primary key, name text not null, age integer, admin boolean default false);
insert into users (id, name, age) values (1, 'Ada', 36), (2, 'Alan', 41) returning id;
select name, age from users where age > 30 and name like 'A%' order by age desc limit 10;
update users set admin = true where id = 1;
delete from users where age is null;
```

## Tables are the same data

A table made with `CREATE TABLE` stores each row as a JSON object keyed by its primary key, so `get users 1`, `find`, `diff` and merges see it too. A table made by writing JSON rows (`put notes a {...}`) has no schema: SQL sees its key as the text column `id`, plus every field any row has.

## What works now

- **Statements:** `CREATE TABLE [IF NOT EXISTS]`, `DROP TABLE [IF EXISTS] a, b [CASCADE]`, `CREATE [UNIQUE] INDEX`, `DROP INDEX`, `INSERT` (several rows, or `INSERT ... SELECT`), `SELECT` (joins and grouping below), `UPDATE [AS alias]`, `DELETE [AS alias]`, `TRUNCATE`, `RETURNING`, `EXPLAIN [ANALYZE]`, `COPY` (see [Loading and unloading](#loading-and-unloading-copy)), `PREPARE` / `EXECUTE` / `DEALLOCATE` (see [Prepared statements](#prepared-statements)), `CREATE TYPE ... AS ENUM` / `DROP TYPE` (see [Enum types](#enum-types)), `CREATE TEMP TABLE` (see [Temporary tables](#temporary-tables)), `CREATE SCHEMA` / `DROP SCHEMA` and `SET search_path` (see [Schemas](#schemas)), and [views](#views), [materialized views](#materialized-views), [sequences](#sequences), [LISTEN / NOTIFY](#listen-and-notify), and `CREATE FUNCTION` / `CREATE TRIGGER` (see [Functions and triggers](#functions-and-triggers)).
- **Clauses:** `WHERE`, `ORDER BY` (by expression, name or position, `ASC`/`DESC`, `NULLS FIRST`/`LAST`; nulls sort last going up by default, as in Postgres), `LIMIT`, `OFFSET`.
- **Expressions:** `= <> < <= > >=`, `AND OR NOT`, `IS [NOT] NULL`, `[NOT] IN`, `[NOT] LIKE / ILIKE`, `BETWEEN [SYMMETRIC]`, `+ - * / %`, `||`, `CAST` and `::`. `IS [NOT] NULL` of a row (`(a, b) IS NULL`) is true when every field is null (`IS NOT NULL`: none is), so a row can be neither.
- **Types:** `integer`, `bigint`, `double precision` (also `real`), `numeric` (also `decimal`), `text` (also `varchar`), `uuid` (see [uuid](#uuid)), `boolean`, `bytea` (see [Binary strings](#binary-strings-bytea)), `jsonb`, `date`, `time`, `timestamp`, `timestamptz`, `interval`, pgvector's `vector` (see [Vectors](#vectors-pgvector)), and enums (see [Enum types](#enum-types)). Any other type name is an error (42704) naming these.
- **`numeric` is exact:** up to 38 digits, with `numeric(p, s)` rounding to its scale on every write (22003 past its precision). As in Postgres, a literal like `1.5` is numeric, so `0.1 + 0.2 = 0.3`; numeric with a float gives a float. Division keeps at least 16 significant digits (`1/3.0` is `0.33333333333333333333`), and `sum`/`avg` of numeric stay exact. Over the Postgres port it travels exactly, in text or binary; in JSON (the HTTP API, `get`) it's a number, so past about 15 digits the JSON view rounds.
- **Constraints:** `PRIMARY KEY` (one column or several, see [Keys of several columns](#keys-of-several-columns): integers, numeric, double precision, text, bytea, uuid, boolean, date, time, timestamps, the address types and enums; not an interval, an array or JSON), `NOT NULL`, `DEFAULT`, `CHECK`, `UNIQUE` (one or more columns; `DEFERRABLE` is taken, `INITIALLY DEFERRED` isn't), `EXCLUDE` (`exclude using gist (room with =, during with &&)`, with `WHERE`; a statement that writes such a table reads it whole, as no index answers `&&`), and foreign keys (`REFERENCES`, `FOREIGN KEY`) with `ON DELETE CASCADE`, `SET NULL` or `RESTRICT` / `NO ACTION`.
- **`LIKE`:** `CREATE TABLE t (LIKE other [INCLUDING | EXCLUDING option ...], ...)` copies a table's (or view's) columns, types and `NOT NULL`, wherever it stands among the columns; `INCLUDING DEFAULTS`, `CONSTRAINTS` (checks, with their names), `INDEXES` (primary key, unique constraints and other indexes, named for the new table), `IDENTITY` (a counter of its own), `GENERATED` (a generated column keeps its expression; else it is a plain column) and `ALL` copy more; `COMMENTS`, `STORAGE`, `COMPRESSION` and `STATISTICS` are read and change nothing. Foreign keys aren't copied, as in Postgres. Not for `CREATE TEMP TABLE` (0A000).
- **`CREATE TABLE AS` and `SELECT INTO`:** `CREATE [TEMP] TABLE [IF NOT EXISTS] t [(a, b)] AS query [WITH [NO] DATA]` (the query a `SELECT`, `WITH` or `VALUES`) and `SELECT ... INTO [TEMP] [TABLE] t FROM ...` make a table with the query's columns (named by the list if there is one, the rest by the query) and its rows. The table is a plain one after that: no primary key, defaults or constraints (Postgres makes none either), and a column has the query's type as Chronos types it (a `varchar(20)` is `text`, `numeric(10,2)` a `numeric`). The tag is `SELECT n`, or `CREATE TABLE AS` with `WITH NO DATA` or when `IF NOT EXISTS` found the table. It is all or nothing: a query that fails leaves no table. More names than columns is 42601, a repeated column name 42701, a table that is there 42P07. `SELECT INTO` in PL/pgSQL is the assignment to variables, as before.
- **Generated values:** `serial`, `bigserial` and `GENERATED ... AS IDENTITY` columns, and `DEFAULT gen_random_uuid()`, `DEFAULT now()`, `DEFAULT current_date` or `DEFAULT nextval('sequence')` (also set later, as pg_dump does, with `ALTER TABLE ... ALTER COLUMN ... SET DEFAULT`). A default Postgres works out afresh for each row, such as `random()`, `now() + interval '7 days'`, `nextval('s') + 1`, `upper(current_user)` or a function of your own, is kept as its SQL and run for each row that takes it. Any other default is kept as one value, worked out when it's set (`DEFAULT 60 * 60`). `ALTER TABLE ... ADD COLUMN` with a default that gives each row a value of its own (`random()`, a sequence, a function) is refused (0A000) on a table with rows; one from the clock or the date gives the rows there what it gives then, as in Postgres. `DEFAULT` can also go in a `VALUES` list, and in `UPDATE ... SET col = DEFAULT` and `ON CONFLICT DO UPDATE SET col = DEFAULT` (the counter's next number, the clock, a new uuid, the default the column was made with, or null).
- **Changing tables:** `ALTER TABLE` can add, drop and rename columns, rename the table, change a column's type, and set or drop its default or `NOT NULL`, several changes in one statement. `ALTER TABLE t RENAME CONSTRAINT a TO b` renames a CHECK, primary key, unique or foreign key constraint (the violation message, `pg_constraint`, its comment and an inheriting child's copy of a CHECK follow; a key's or unique constraint's index is renamed with it). A missing name is 42704; a taken one is 42710, or 42P07 when an index or other relation has the name a key or unique constraint is given, as in Postgres. A renamed table keeps its primary key's name, and a new key is `<table>_pkey1`, `<table>_pkey2`, ... when a relation or constraint of the schema has `<table>_pkey`, as in Postgres.
- **Parameters:** `$1, $2, ...`, with types inferred from use as Postgres does.
- **Errors:** Postgres SQLSTATE codes (23505 duplicate key, 42P01 no such table, ...). A constraint violation has Postgres's message and fields (`constraint`, `table`, `column`, `schema`, `detail`), so `err.constraint === 'bookings_unit_uq'` works; Chronos adds the row it is about in the `hint` (`Row bookings/7 already has this key.`), which the HTTP API and MCP show too.

Outside a transaction each statement commits on its own, durably on `main` (see [concepts](concepts.md)).

## Joins, grouping and functions

```sql
select c.name, count(o.id) as orders, sum(o.total)
from customers c left join orders o on o.customer = c.id and o.status = 'paid'
where c.city = 'London'
group by c.name
having count(o.id) > 0
order by orders desc;
```

- **Joins:** `[INNER] JOIN`, `LEFT`, `RIGHT` and `FULL [OUTER] JOIN`, each `ON ...`, `USING (cols)` or `NATURAL` (joined on every column both sides have, shown once, as `USING` does; none in common is a cross join), `CROSS JOIN`, and `FROM a, b`. Tables can have aliases (`orders o`), columns can be qualified (`o.total`), and `t.*` selects one table's columns.
  - **`USING`:** as in Postgres, `*` shows each USING column once and first, and a bare name means the joined value (for `FULL`, the first non-null of the two sides). `a.x` and `b.x` still name each side.
  - **`RIGHT` and `FULL`:** every row of the right table appears, matched or padded with nulls (and for `FULL`, every left row too). Chronos reads the right table whole and hashes it on an equality in `ON`; without one, it compares every pair.
  - **Join order:** with only inner joins, Chronos starts from the table its conditions narrow most (a key lookup, or the fewest index matches), then joins linked tables, key links first. Outer joins keep the written order.
  - **Few rows so far (up to 1,000):** a join on the next table's primary key reads just the matching rows. A join on another integer, text or boolean column looks each value up in that table's index.
  - **Otherwise,** equality joins use a hash join, and joins on anything else compare every pair.
  - **Conditions on one table** are applied, and use its index, as it's read.
- **Set-returning functions in the select list:** `SELECT unnest(tags), generate_subscripts(a, 1) ...` and, inside an expression, any of `generate_series`, `unnest`, `jsonb_array_elements(_text)`, `jsonb_object_keys`, `regexp_split_to_table` and `generate_subscripts` (`generate_series(1, 3) * 2`, `upper(unnest(names))`, `jsonb_array_elements(doc -> 'items') ->> 'sku'`): a row per element, several side by side (the shorter padded with nulls). Not in `WHERE`, `GROUP BY`, `HAVING` or `ON` (0A000), and not yet in a grouped query, where Postgres runs them after the grouping. A lone jsonb value is an array of one, so `'1'::jsonb -> 0` is `1`.
- **`WITH ORDINALITY`:** a set-returning function in `FROM` (`unnest`, `generate_series`, `jsonb_array_elements`, `jsonb_each`, `regexp_split_to_table`, ...) may say `f(...) WITH ORDINALITY [AS] alias(a, n)`: its rows numbered from 1 in a bigint column after its own (`ordinality`, unless the alias's column list names it). After a table it is read for each of the table's rows.
- **`||` by type:** text with anything concatenates (`'a' || 1`, `1 || '2'`), arrays append, and beside a jsonb an unknown literal reads as jsonb (`payload || '{"k":1}'` merges, `'[3]'` appends). Two typed numbers or booleans are 42883 as in Postgres. `jsonb_array_elements(x) AS i` names its column `i`.
- **Row values:** `(a, b) IN (SELECT x, y ...)`, `(a, b) = ANY | ALL (SELECT ...)` and `(a, b) = (SELECT x, y ...)` (and the other comparisons) compare row by row with SQL's rules for nulls; `UPDATE t SET (a, b) = (1, 'x')` and `SET (a, b) = (SELECT ...)` assign several columns (a subquery is asked once per column). A row is also a value, held as the text Postgres writes a composite as: `select t from t`, `select row(1, 'a b')` (`(1,"a b")`), `count(t)`, `t IS NULL`, `t = t`, `(t).col`, `row(t.*)`. Not yet: `pg_typeof` of one (text, not record), a column of composite type, `(select t)` of an outer table, `(row_col).field` of a subquery's row.
- **Choosing rows:** `SELECT DISTINCT ON (a, b) ... ORDER BY a, b, c` keeps the first row (by `ORDER BY`) of each set with the same a and b; the `ON` expressions that `ORDER BY` names have to lead it (42P10 otherwise), as in Postgres. Not yet with `GROUP BY`, aggregates or window functions. `FETCH FIRST | NEXT [n] ROW | ROWS ONLY` is `LIMIT n`, with `OFFSET n ROWS` before or after; `FETCH FIRST n ROWS WITH TIES` (which needs an `ORDER BY`, 42601 otherwise) also gives the rows that tie with the last on its keys; it reads the whole ordered result, so it does not get a `LIMIT`'s top-n shortcut. `TABLE t` is `SELECT * FROM t` (also in a subquery, a UNION, `CREATE TABLE AS` and `INSERT`). A `WITH` query may say `MATERIALIZED` or `NOT MATERIALIZED`; it is read and ignored, as its rows are the same either way.
- **Grouping sets:** `GROUP BY ROLLUP (a, b)`, `CUBE (a, b)`, `GROUPING SETS ((a), (b), ())` (nested, and multiplied by other `GROUP BY` items: `GROUP BY a, ROLLUP (b)`), `GROUP BY DISTINCT` to drop repeated sets, and `GROUPING(a, b)` for which columns a row's set left out (the first argument the highest bit). It runs as the same query once per set with `UNION ALL`, a column outside a set as NULL: `ORDER BY` and `LIMIT` apply to all of it, and can name output columns, positions, or an expression the select list has (`order by sum(qty)`). Not yet with `DISTINCT` (0A000) or a window function.
- **Ordered-set aggregates:** `percentile_cont(f) WITHIN GROUP (ORDER BY x)` (interpolated, as double precision; `f` may be an array of fractions), `percentile_disc(f)` and `mode()`, with `FILTER`. Nulls aren't in the distribution; `f` outside 0 to 1 is 22003. Not yet: intervals in `percentile_cont`, and the hypothetical-set aggregates (`rank(x) WITHIN GROUP ...`).
- **Row values:** `(a, b) < (c, d)`, and `=`, `<>`, `<=`, `>`, `>=`, `ROW(a, b)` and `(a, b) IN ((1, 2), (3, 4))`, compared as Postgres compares them (by the first pair that differs, nulls as in Postgres): keyset pagination, `where (created_at, id) < ($1, $2) order by created_at desc, id desc limit 20`. They work as `a < c OR (a = c AND b < d)`, so an index on the first column serves them and the rest is checked. A row is only ever compared, not selected or stored.
- **Operators:** `^` (a power, as double precision: `2 ^ 3 ^ 2` is 64, and `0 ^ -1` is 2201F), the bitwise `&`, `|`, `#` (exclusive or), `~` (not; its operand is all the arithmetic after it, so `~5 + 1` is `~(5 + 1)`), `<<` and `>>`, and `x IS [NOT] TRUE | FALSE | UNKNOWN`, which bind as in Postgres (`IS` looser than the comparisons: `x = y IS TRUE`). Integers are 64-bit here, so a shift past 32 bits doesn't wrap as `int4` does.
- **`pg_trgm`, `fuzzystrmatch`, `unaccent`:** `similarity(a, b)`, `word_similarity`, `strict_word_similarity`, `show_trgm(text)` and the operators `%` (similar), `<%`, `%>`, `<<%`, `%>>` (word similarity, either way round) and the distances `<->`, `<<->`, `<->>`, `<<<->`, `<->>>` between two texts (pg_trgm, at the default thresholds 0.3, 0.6 and 0.5); `levenshtein(a, b[, ins, del, sub])`, `levenshtein_less_equal`, `soundex`, `difference`, `metaphone(s, n)`, `dmetaphone`, `dmetaphone_alt` and `daitch_mokotoff` (fuzzystrmatch); `unaccent([dictionary,] text)` with the default rules. They are built in, so `CREATE EXTENSION pg_trgm` is accepted and changes nothing. The results are pg_trgm's own algorithms (checked against Postgres 16 on 138,000 generated cases); `similarity` and the distances are `real` in Postgres and print with a float4's digits here, though they are held as double precision. `SET pg_trgm.similarity_threshold` isn't read by `%` yet.
- **`pgcrypto`:** `digest(data, 'sha256')` and `hmac(data, key, 'sha256')` (md5, sha1, sha224, sha256, sha384, sha512, ripemd160; of text or bytea), password hashes with `crypt(password, gen_salt('bf'))` (and `'md5'`, `'xdes'`, `'des'`; `gen_salt('bf', 10)` for more rounds; check one with `crypt(password, hash) = hash`), `gen_random_bytes(n)` (1 to 1024, from the system's generator), `encrypt(data, key, 'aes')` and `decrypt` (`aes`, `bf`, `cast5`, `des`, `3des`; `-cbc` or `-ecb`; `/pad:pkcs` or `/pad:none`), `encrypt_iv` and `decrypt_iv`, OpenPGP messages with `pgp_sym_encrypt(text, password [, 'cipher-algo=aes256, compress-algo=1'])` and `pgp_sym_decrypt` (and `_bytea` forms for bytes), `armor` and `dearmor`, and `pgp_key_id`. Each gives what Postgres's pgcrypto 1.3 gives, so hashes and messages move between the two both ways. They are built in; `CREATE EXTENSION pgcrypto` lists it in `pg_extension`. The public-key functions (`pgp_pub_encrypt`, `pgp_pub_decrypt`) and `pgp_armor_headers` aren't here yet; the rest of what differs is in [Postgres compatibility](postgres-compatibility.md).
- **`jsonb_to_record(j)` / `jsonb_to_recordset(j)`** (and `json_to_record`, `json_to_recordset`): a JSON object, or array of objects, as rows by the column definition list that follows it in `FROM`: `jsonb_to_recordset(o.items) AS i(sku text, qty int)`. A key not in the list is ignored, a missing one is NULL, and each value is cast to the column's type (with `numeric(p,s)` and `varchar(n)` limits). Without a list it is 42601; `jsonb_populate_record` (composite types) is 0A000.
- **`to_number(text, template)`** with every numeric template pattern (`9 0 . , D G S MI PL SG PR L V RN TH FM`; `EEEE` is refused as in Postgres), numbers beyond 38 digits aside.
- **Domains:** `CREATE DOMAIN name [AS] type [DEFAULT e] [NOT NULL] [CHECK (... VALUE ...)]` and `DROP DOMAIN [CASCADE]`: a column of the domain is its base type with the domain's NOT NULL, DEFAULT and CHECKs (the column's own DEFAULT wins). A cast to a domain (`x::posint`) is to its base type without the checks, `pg_typeof` says the base type, and there is no `ALTER DOMAIN`.
- **`ORDER BY x USING < | >`** (ascending, descending: the btree operators), and **`unnest(a, b, ...)`** in FROM: a column for each array, side by side, the shorter padded with nulls.
- **ALTER options accepted as no-ops** (with a notice): `REPLICA IDENTITY`, `CLUSTER ON`, `SET WITHOUT CLUSTER`, `ALTER COLUMN ... SET STATISTICS | STORAGE | COMPRESSION | (options)` and `RESET (...)`, and a function's attributes (`ALTER FUNCTION f(int) IMMUTABLE | SECURITY DEFINER | COST n | SET ...`).
- **Constraint options:** `UNIQUE NULLS NOT DISTINCT (a)` (also on `CREATE UNIQUE INDEX` and in `pg_get_indexdef`), `FOREIGN KEY ... MATCH FULL` (all columns null or none; `MATCH PARTIAL` is 0A000), `CREATE TEMP TABLE ... ON COMMIT DELETE ROWS | DROP | PRESERVE ROWS`. Not here: `DEFERRABLE` unique constraints (a unique key is claimed as the row is written).
- **`TABLESAMPLE`:** `FROM t [alias] TABLESAMPLE BERNOULLI|SYSTEM (percent) [REPEATABLE (seed)]`: each row is kept with that chance (both methods a row at a time; Postgres's SYSTEM takes whole pages), the same seed keeps the same rows. `generate_series(0, 1, 0.25)` takes a numeric step.
- **Array assignment:** `UPDATE t SET a[2] = v`, `SET a[1] = x, a[2] = y` and `SET a[lo:hi] = '{..}'` (a place past the end grows the array with nulls; one before the start moves its lower bound). One subscript only.
- **Small syntax:** array slices `a[2:3]`, `a[:2]`, `a[2:]` (as Postgres clamps them: past the ends is fewer elements, none is `{}`; a subscript after a slice is 0A000, where Postgres would read a second dimension); the prefix operators `@ x` (absolute value), `|/ x` (square root) and `||/ x` (cube root, also `cbrt(x)`), taking all the arithmetic after them as `~` does; a type's name as a function, `int4('1')`, `int8`, `int2`, `float4`, `float8`, `text(5)`, `bool('t')` and `date('2024-01-02')`, which cast; `make_interval(years, months, weeks, days, hours, mins, secs)`; `x COLLATE "de-x-icu"` (see [Collations](#collations)); and named arguments, `make_interval(days => 1, hours => 2)` or `:=`, and for functions made with CREATE FUNCTION, `f(b => 2, a => 1)` (by position first, then names; each parameter once; a parameter left out takes its default). Other built-ins don't take names (42883).
- **A series in the select list:** `select generate_series(1, 3)` and `generate_series(a, b)::date` (one per query; several set-returning functions at once are only `unnest` and `generate_subscripts`).
- **Aggregates** (each can take `FILTER (WHERE ...)`): `count(*)`, `count(x)`, `count(distinct x)`, `sum`, `avg`, `min`, `max`, `bool_and`/`every`, `bool_or`, and `string_agg(x, sep)`, `jsonb_agg`, `jsonb_object_agg(k, v)`, `array_agg` (as Postgres's array text, `{1,2}`), each with its own `ORDER BY` (`string_agg(name, ', ' order by name)`) and `DISTINCT`, with `GROUP BY` (expressions, output names or positions), `HAVING` and `SELECT DISTINCT`. A column that's neither grouped nor aggregated is an error (42803), as in Postgres, unless its table's whole primary key is grouped (`select id, name, count(*) from users group by id`: the other columns follow from the key; a derived table or view has no key). On `INSERT ... ON CONFLICT DO UPDATE`, `RETURNING (xmax = 0) AS inserted` is true for a row the statement made and false for one the conflict changed; `xmin`, `xmax`, `cmin` and `cmax` read 1, 0, 0, 0 elsewhere.
- **Statistical aggregates:** `stddev`, `stddev_pop`, `stddev_samp`, `variance`, `var_pop`, `var_samp` (integers and numerics give a numeric at the scale Postgres picks, floats a double precision), and `corr`, `covar_pop`, `covar_samp`, `regr_count`, `regr_avgx`, `regr_avgy`, `regr_sxx`, `regr_syy`, `regr_sxy`, `regr_slope`, `regr_intercept` and `regr_r2` (each `(dependent, independent)`, double precision). They work as Postgres's do, so the digits agree, including the last ones: floats accumulate as `float8_accum` does, integers and numerics through numeric arithmetic (a sum of squares past 38 digits is 22003). A row with a null argument is left out; no rows, and one row for the sample forms, is null (`regr_count` of no rows is 0). `FILTER`, `DISTINCT` (one argument) and an `ORDER BY` inside work; anything that is not a number is 42883. Not for a `real` column: it is held as a double here, so its values are not rounded to four bytes first, and a `stddev` over one can differ from Postgres's in the last digits.
- **Aggregates as window functions:** `string_agg`, `array_agg`, `jsonb_agg`, `jsonb_object_agg` and the statistical aggregates take `OVER (...)` with any frame, as `sum` and `avg` do (`string_agg(name, ',') over (partition by team order by id)`, `stddev(x) over (order by ts rows between 6 preceding and current row)`). `DISTINCT` and an aggregate `ORDER BY` inside one are 0A000, as is an ordered-set aggregate (`percentile_cont ... over`) in Postgres.
- **Functions**, with Postgres's names, NULL rules and edge cases:
  - **Text:** `lower`, `upper` (one character for one, as Postgres's locale functions do: `upper('ß')` is `ß`), `length`/`char_length`/`character_length`, `octet_length`, `concat`, `concat_ws`, `substring` (also `substring(s from a for b)`) and `substr`, `position(a in s)` and `strpos`, `replace`, `trim` (also `trim(leading 'x' from s)`), `btrim`, `ltrim`, `rtrim`, `left`, `right`, `lpad`, `rpad`, `repeat`, `reverse`, `translate`, `split_part`, `initcap`, `ascii`, `chr`, `unistr` (`\0041`, `\+01F600`, `\u0041`, `\U00000041`), the literal `U&'d\0061t'` (with `UESCAPE '!'`), `starts_with`; `bpchar` is read as `char`, without the padding.
  - **Math:** `abs`, `round(x[, places])`, `ceil`, `floor`, `trunc(x[, places])`, `sign`, `mod`, `power`, `sqrt`, `exp`, `ln`, `log(x)` / `log(b, x)`, `pi`, `random`, `setseed` (see [Simulations](#simulations)), `greatest`, `least`, the trigonometric `sin`, `cos`, `tan`, `cot`, `asin`, `acos`, `atan`, `atan2`, `sinh`, `cosh`, `tanh`, `degrees` and `radians` (doubles), `div(a, b)` (numeric, cut toward zero), `gcd`, `lcm`, `factorial` (up to what 38 digits hold), `width_bucket(x, low, high, count)` and `to_hex`. `ceil`, `floor`, `trunc` and `sign` of an integer are double precision, as in Postgres; `round` of a double rounds halves to the even number (`round(2.5::float8)` is `2`, `round(2.5)` numeric is `3`). Integer literals may be written `0x1F`, `0o17` and `0b101` (underscores between digits allowed), and `1::boolean` / `true::int` cast between the two (int4 only). `bit_and`, `bit_or` and `bit_xor` of integers and `any_value(x)` are aggregates. Not yet: `sind`/`cosd`/`tand`, `asinh`/`acosh`/`atanh`, and numeric (exact) `sqrt`, `exp`, `ln` and `power` (they give doubles).
  - **NULLs:** `coalesce`, `nullif`, `num_nulls(...)` and `num_nonnulls(...)`.
  - **Sizes:** `pg_size_pretty(bigint | numeric)` and `pg_size_bytes('1.5 MB')` as Postgres writes and reads them, and `pg_relation_size(rel [, fork])`, `pg_table_size`, `pg_indexes_size`, `pg_total_relation_size` and `pg_database_size(name)`. The byte counts are estimates in Postgres's units (whole 8 kB pages of the rows and index entries Chronos stores, so a table of 1,000 rows of 100 bytes comes out near what Postgres says), not what Chronos's files take, and reading them reads the table. An index takes the name of the index (`pg_relation_size('t_pkey')`), a view has no storage (0), a sequence is one page. `pg_database_size` is the bytes of the database's files, shared by every world. psql's `\dt+`, `\di+`, `\ds+`, `\dv+` and `\l+` show them.
  - **The session and the server:** `version()` (`PostgreSQL 16.15 (Chronos 0.1.3) on ...`: clients read the leading `PostgreSQL 16.15`), `current_user`, `session_user` and `current_role` (the name a Postgres client logged in with, or an agent's name, until `SET ROLE` or `SET SESSION AUTHORIZATION` changes them; a `SECURITY DEFINER` function's `current_user` is its owner: see [Roles and privileges](#roles-and-privileges)), `current_catalog`, `current_schema`, `pg_backend_pid()` (the connection's id, also what `pg_cancel_backend(pid)` takes to stop that connection's statement, and what a NOTIFY carries), `pg_is_in_recovery()` (false), `pg_postmaster_start_time()`, `txid_current()` (one number for a transaction), `inet_server_addr()` and the other `inet_*` functions (null, as over a Unix socket), `pg_sleep(seconds)` (stops when the statement is canceled or times out), `pg_encoding_to_char(n)`, and `has_table_privilege` and its kin ([Postgres compatibility](postgres-compatibility.md#known-gaps)).
  - **Dates:** see below, plus `make_date(y, m, d)`, `to_timestamp(epoch seconds)`, `age(a[, b])` (years, months and days by the calendar, as Postgres counts them) and `to_char(ts, 'YYYY-MM-DD HH24:MI')` (Postgres's date and time patterns: `YYYY`, `MM`, `Mon`, `Month`, `DD`, `Day`, `Dy`, `HH12`, `AM`, `MI`, `SS`, `MS`, `US`, `Q`, `DDD`, `FM`, `"literal"`).
  - **Numbers as text:** `to_char(n, 'FM9,999.00')` with Postgres's number patterns: `9`, `0`, `.`/`D`, `,`/`G`, `S`, `MI`, `PL`, `SG`, `PR`, `FM` and `"literal"`.
  - **Regular expressions:** `s ~ 'pattern'` (`~*` ignoring case, `!~` and `!~*` negated), `regexp_replace(s, pattern, replacement[, 'gi'])` (with `\1` and `\&`), `regexp_match`, `regexp_like`, `regexp_count`, `regexp_substr`, `regexp_instr` (all with Postgres's start, N, flags, end-option and sub-expression arguments; `regexp_replace(s, p, r, start, N, flags)` too), `string_to_table`, `regexp_split_to_array`, and in FROM `regexp_split_to_table` and `regexp_matches`. `s LIKE 'pattern' ESCAPE 'c'` (and `NOT LIKE`, `ILIKE`; `ESCAPE ''` turns escaping off; the operators `~~`, `!~~`, `~~*`, `!~~*`, and `^@` for a prefix; `LIKE`, `ILIKE`, `~` and `~*` with `ANY` or `ALL` over an array or subquery), `substring(s from 'regex')` (its first parenthesized part, or the whole match), `substring(s from 'pattern' for 'esc')` and `substring(s similar 'pattern' escape 'esc')` (the part between the escape-double-quotes), `s SIMILAR TO 'pattern' [ESCAPE 'c']` (and `NOT SIMILAR TO`), with `%`, `_`, `|`, `*`, `+`, `?`, `{n,m}`, `()` and `[...]`. Postgres's advanced syntax (`src/sql/regex.rs`, checked against Postgres 16 on thousands of random patterns): classes (`[a-z]`, `[[:alpha:]]`, `[[.a.]]`, `[[=a=]]`, `\d \w \s` and `\D \W \S`, also inside brackets), anchors and word edges (`\A \Z \y \Y \m \M`, `[[:<:]]`, `[[:>:]]`), groups, `(?:...)`, alternation, greedy and lazy quantifiers (`{n,m}` up to 255), lookaround (`(?=...)`, `(?!...)`, `(?<=...)`, `(?<!...)`; their parentheses capture nothing), back references (`\1`, a lone digit is always one, `\10` only with ten groups open, else octal; refused inside lookaround and before the group closes), the escapes `\t \n \e \cX \xhhh \uhhhh \Uhhhhhhhh \ooo`, and the options: embedded at the start (`(?i)`, `(?n)`, `(?p)`, `(?w)`, `(?s)`, `(?c)`, `(?x)`, `(?q)`, `***=`, `***:`) or as flag letters (`b c e g i m n p q s t w x`). Of the matches at the leftmost place it takes the longest (the shortest when the first quantifier is lazy), as Postgres's engine does. A quantifier after an anchor or lookaround, or a back reference to a group that does not exist, is 2201B as in Postgres. Known differences: a lazy quantifier inside an alternation under a counted repeat, `(x)+` where `x` can match nothing, and `regexp_matches` beside other set-returning functions in the select list. A pattern that would backtrack for too long stops with 54001.
  - **Arrays:** `text[]`, `integer[]` and other array columns (stored as Postgres's array text), `ARRAY[...]`, `ARRAY(SELECT x ...)` (the rows of one column, `{}` for none; a plain query may read the query around it), `a @> b`, `a <@ b` and `a && b` (contains, is contained by, overlaps: by element, a null equal to none; `@>` and `<@` are jsonb's when neither side is an array), `a[i]` (from 1; outside the array: null), `a[lo:hi]` slices, `||` (array with array, or with an element on either side), `array_append`, `array_prepend`, `array_cat`, `array_remove`, `array_replace`, `array_position`, `array_positions`, `array_dims`, `array_ndims`, `array_fill` and `trim_array` (one dimension: a multi-dimensional `array_fill` is 0A000), `string_to_array`, `array_to_string(a, sep[, null_string])`, `unnest`, `= ANY(...)`. (`array_to_string` of a boolean array writes `t` and `f`, not `true` and `false`: an array is text here, so its elements' types aren't known.) `jsonb` takes subscripts too: `doc['key']`, `doc['list'][0]`.
  - **Ranked search:** `search('table', 'words' [, k])` in FROM: the built-in text index's best k rows (BM25, typo-tolerant), as `id`, `score`, `row` (jsonb). See [search](search.md).
  - **Full-text search, as Postgres does it:** `to_tsvector([config,] text)`, `to_tsquery`, `plainto_tsquery`, `phraseto_tsquery`, `websearch_to_tsquery`, `@@`, `ts_rank([weights,] vector, query [, normalization])`, `setweight`, `strip`, `tsvector || tsvector`, and `tsvector`/`tsquery` columns and casts. The `english` (the default) and `simple` configurations match Postgres's: its parser, stop words and Snowball stemmer, checked word for word on 20,000 dictionary words. `text @@ text` and `text @@ tsquery` work as in Postgres. `CREATE INDEX ... USING gin (to_tsvector('english', body))` (or on a `tsvector` column) keeps an entry per lexeme: `@@` with the same expression reads the rows its query's lexemes allow (`&` intersects, `|` unites, `:*` reads a range) and checks just those, 12 times faster than a scan at 100,000 rows. As in Postgres, the index's expression must match the query's (config included). Without one, `@@` computes each row's vector as it scans. Not yet: `ts_headline`, `ts_rank_cd`, and the parser's file-path and version tokens.
  - **Points, as PostGIS does them:** `geometry` and `geography` columns and casts (hex EWKB, WKT or `SRID=4326;POINT(lon lat)`), `ST_MakePoint`, `ST_Point`, `ST_SetSRID`, `ST_SRID`, `ST_GeomFromText`, `ST_GeogFromText`, `ST_GeomFromGeoJSON`, `ST_AsText`, `ST_AsEWKT`, `ST_AsGeoJSON`, `ST_X`, `ST_Y`, `ST_Distance` (meters on the WGS84 ellipsoid for geography, the exact geodesic, or on PostGIS's sphere with `false`), `ST_DWithin` and `ST_DistanceSphere`, checked against PostGIS 3.6. Values print as PostGIS prints them, and clients receive them as text. `CREATE INDEX ON places USING gist (location)` makes `ST_DWithin(location, point, distance)` read only the band of latitudes (or y) the distance can reach, then check those rows: a 1 km search among 200,000 points worldwide reads about 20 rows. `a <-> b` is PostGIS's distance operator (planar for geometry, on the sphere for geography), and `ORDER BY location <-> point LIMIT k` (or `ORDER BY ST_Distance(location, point)`) reads nearest first from the index, widening its search until the LIMIT fills: the nearest 10 of 200,000 points take about 1 ms. `CREATE EXTENSION postgis` is accepted. Not yet: lines and polygons, and `ST_Transform`.
  - **Other:** `md5(text)`, `format(fmt, ...)` (`%s`, `%I` for identifiers, `%L` for literals, `%%`, positions `%2$s`, widths `%-10s` and `%*s`), `quote_ident`, `quote_literal` and `quote_nullable`, as Postgres writes them.
  - **jsonb:** `->` and `->>` (a field or array element, as jsonb or text), `#>` and `#>>` (a path, `'{a,b,0}'`), `@>` and `<@` (containment), `?`, `?|`, `?&` (keys), `||` (merge), `-` (a key, an index, or a `text[]` of keys), `#-` (delete at a path), `jsonb_insert`, `jsonb_pretty`, `jsonb_set_lax`, and `jsonb_build_object`, `jsonb_build_array`, `to_jsonb`, `row_to_json` and `array_to_json` (a table's name, alias or `t.*` in `to_jsonb(t)`, `json_agg(t)` or `jsonb_build_object('k', t)` is its row as an object; arrays are JSON arrays), `jsonb_typeof`, `jsonb_array_length`, `jsonb_extract_path(_text)`, `jsonb_set`, `jsonb_strip_nulls` (`json_` names too). jsonb prints as Postgres prints it: `{"a": 1, "bb": [1, 2]}`, shorter keys first.
  - Functions made with `CREATE FUNCTION` too: see [Functions and triggers](#functions-and-triggers).
  - An unknown function is error 42883, as in Postgres, and the message lists the functions there are.

## Unique constraints and foreign keys

```sql
create table users (id serial primary key, email text unique, org int references orgs on delete cascade);
alter table posts add constraint posts_author_fkey foreign key (author) references users on delete set null;
```

- **When they're checked:** at every statement, and again at every merge, including a transaction's `COMMIT`. Two branches can each be valid and still clash together: the same email on both, or one side deleting a row the other side's new row references. The merge then fails with 23505 or 23503, merges nothing, and says which rows clash. That holds even when you force a side with `USING OURS` / `THEIRS`.
- **How they're stored:** each unique value and each reference is a small index row, so a check is one lookup (about 1 µs), not a scan. Those rows don't show in `DIFF`.
- **Changing them:** `ALTER TABLE ... ADD CONSTRAINT` checks existing rows first. `DROP CONSTRAINT` removes one, and `DROP COLUMN` and `DROP CONSTRAINT` take `CASCADE` (the foreign keys that reference what goes are dropped too, with a NOTICE; without it they are refused with 2BP01). Constraints follow renamed tables and columns, and a table others reference can't be dropped.
- **Several columns:** `FOREIGN KEY (a, b) REFERENCES t` references a primary key of several columns (naming its columns in any order), with `MATCH SIMPLE`'s rule: a row with a null among them references nothing.
- **Deferrable foreign keys:** `DEFERRABLE`, `NOT DEFERRABLE`, `INITIALLY DEFERRED` (which implies `DEFERRABLE`) and `INITIALLY IMMEDIATE` on `REFERENCES` and `FOREIGN KEY`, as Django writes on every foreign key. In a transaction, a deferred key isn't checked when its row is written (or its parent deleted) but when the transaction ends, so children can come before parents; a row that fails then fails the `COMMIT` (23503) and the transaction is rolled back. `SET CONSTRAINTS ALL | name, ... DEFERRED | IMMEDIATE` changes that for the rest of the transaction (`IMMEDIATE` checks what was put off at once), and `ALTER TABLE ... ALTER CONSTRAINT name [NOT] DEFERRABLE [INITIALLY DEFERRED | IMMEDIATE]` changes it for good. Outside a transaction a statement is its own commit, so nothing is put off. `RESTRICT` is checked at once, and `CASCADE`, `SET NULL` and `SET DEFAULT` act at once, as in Postgres. Unique and primary keys can't be deferred (0A000).
- **Actions:** `ON DELETE` and `ON UPDATE` each take `NO ACTION` (the default), `RESTRICT`, `CASCADE`, `SET NULL` and `SET DEFAULT`. Changing the primary key of a row others reference (`UPDATE p SET id = 5`) is refused (23503) unless `ON UPDATE` says otherwise: `CASCADE` gives the referencing rows the new key (a reference that is part of their own key moves them, and their children in turn), `SET NULL` and `SET DEFAULT` set the reference to null or to its column's default (which must then be a row of the parent, or the statement fails with 23503). An update that leaves the key as it was never touches the referencing rows. `SET NULL` can't apply to a primary key column (42601), and a table with row triggers can't be the target of an acting `ON UPDATE` or `ON DELETE` (0A000).
- **To a UNIQUE column:** `REFERENCES t (col)` and `FOREIGN KEY (a, b) REFERENCES t (x, y)` may name a `UNIQUE` constraint's columns (or a plain unique index's, in any order) as well as the primary key's; a partial or expression unique index can't be referenced (42830, as in Postgres). The referencing row's values are looked up by the unique entry, a change of the referenced value acts as `ON UPDATE` says, and a merge that leaves a row pointing at a value the other world deleted fails with 23503. The unique constraint or index, and its columns, can't be dropped while a foreign key uses it (2BP01), and its type can't change except between `integer` and `bigint` (0A000).
- **Not yet:** `MATCH PARTIAL` (0A000). `MATCH FULL` works.
- **JSON writes too:** agents' `put`, `delete` and `batch` (shell, HTTP, MCP, Rust) on a SQL table are checked like an `INSERT`. See [JSON writes into SQL tables](#json-writes-into-sql-tables).

## JSON writes into SQL tables

Agents writing JSON rows (`put`, `delete`, `batch`, over the shell, HTTP, MCP or Rust) into a table made with `CREATE TABLE` get the same rules as SQL, in the same transaction-like batch:

- **Columns:** the row must be a JSON object of the table's columns (an unknown column is 42703) with values that fit their types (22P02). A missing column gets its default, a `serial` number or a UUID. `NOT NULL` holds (23502).
- **Keys:** the key names the row. `put users/5` sets `id` to 5, and an `id` that disagrees with the key is refused (23514).
- **Constraints:** `UNIQUE` and foreign keys hold (23505, 23503), and a delete follows `ON DELETE`.
- **Storage:** the row is stored packed, and read back as JSON.
- **HTTP status:** a row that doesn't fit is a 400; a clash is a 409.
- **Reserved names:** keys and tables starting with `_sonos` are the database's own bookkeeping (schemas, constraint indexes, agents). Nobody can read or write them with SQL or JSON writes, or create or rename a table to such a name.

Tables made by writing JSON (no `CREATE TABLE`) stay free-form. Until a SQL table exists, JSON writes skip these checks entirely.

- **Triggers:** JSON writes to a table with triggers turned on are refused (0A000): triggers fire for SQL's `INSERT`, `UPDATE` and `DELETE` only, and a write that skipped them would leave their work undone. `ALTER TABLE ... DISABLE TRIGGER` lets JSON writes in again.

## Upserts and CASE

```sql
insert into stock (sku, qty) values ('a', 3)
on conflict (sku) do update set qty = stock.qty + excluded.qty
returning *;

select name, case when total >= 100 then 'big' when total >= 10 then 'mid' else 'small' end
from orders;
```

- **`ON CONFLICT`:** `DO NOTHING`, or `DO UPDATE SET ... [WHERE ...]`, on the primary key or any `UNIQUE` constraint: named by its columns in any order, `ON CONFLICT (room, num)`, or by its name, `ON CONFLICT ON CONSTRAINT seat_room_num_key`. A partial unique index is named by its columns and a `WHERE` that implies the index's own, `ON CONFLICT (n) WHERE status = 'live'` (each of the index's ANDed conditions written in the `WHERE`; more may be added); one it doesn't imply is 42P10, as in Postgres. `DO NOTHING` without a target skips any clash.
  - `excluded.col` is the row that was to be inserted, and bare column names mean the existing row.
  - `INSERT INTO t AS alias` works.
  - Columns `SET` doesn't name keep their values.
- **`CASE`:** both the `CASE WHEN ...` and `CASE value WHEN ...` forms, anywhere an expression goes, including inside and around aggregates.

## Numbering rows

A `serial` column (and `GENERATED ... AS IDENTITY`) takes the next number from a counter that belongs to the database, not to any branch, so rows inserted on two branches at once never collide when they merge. Taking a number writes nothing to any branch: inserts in a fork leave main's rows and version as they were. As with Postgres sequences:
- **Not taken back:** a rollback or a failed insert still uses its number.
- **Gaps:** a restart can skip numbers, because numbers are reserved 64 at a time. A branch sees gaps where other branches took numbers too: after a fork inserts 3 rows, main's next row may be 5, not 2.
- **Never twice:** the counters are logged and kept in every checkpoint, so a restart or a crash never hands a number out again. Time travel doesn't rewind them: restoring a world to the past leaves the counters where they are.

`CREATE SEQUENCE` makes a named one, and `DEFAULT nextval('name')` numbers a column from it (see [Sequences](#sequences)).

A serial or identity column's counter has the name Postgres gives its sequence, `table_column_seq` (`app.t_id_seq` in a schema), which `pg_get_serial_sequence('t', 'id')` returns, and `nextval`, `currval`, `setval` and `lastval` take: `select setval(pg_get_serial_sequence('t', 'id'), coalesce(max(id), 1), max(id) is not null) from t` after loading rows with their own ids, as Django and Rails do, moves the counter, and the next insert takes the number after it. The counter is a sequence, listed as one: `\ds`, `pg_class` (`relkind` `S`), `pg_sequence`, `pg_sequences` and `information_schema.sequences` show `t_id_seq`, `select last_value, is_called from t_id_seq` reads where it is (what `pg_dump` reads to write its `setval`), and a `serial` column's default is `nextval('t_id_seq'::regclass)`. `ALTER SEQUENCE` doesn't reach it. `INSERT INTO t DEFAULT VALUES` makes a row of defaults.

**Identity columns** keep which kind they are. `GENERATED ALWAYS AS IDENTITY` refuses a value written to the column (`INSERT` and `UPDATE` fail with 428C9) unless the `INSERT` says `OVERRIDING SYSTEM VALUE`; `OVERRIDING USER VALUE` takes the counter's number whatever was given; `BY DEFAULT` takes either. `COPY` fills either kind, as Postgres's does. `ALTER TABLE t ALTER COLUMN c ADD GENERATED ALWAYS | BY DEFAULT AS IDENTITY` (the column must be `NOT NULL`, an integer, with no default), `SET GENERATED ...` and `DROP IDENTITY [IF EXISTS]` work as in Postgres, and `pg_attribute.attidentity` and `information_schema.columns.is_identity` / `identity_generation` say what each column is. The options in `(...)` (`START WITH`, `INCREMENT BY`, `MINVALUE`, `MAXVALUE`, `CYCLE`, `SEQUENCE NAME`) work as a sequence's do: an identity column made with any that aren't the defaults gets a sequence of its own, `t_c_seq`, owned by the column (`pg_sequence`, `select last_value from t_c_seq`, and `ALTER SEQUENCE` reach it; `DROP IDENTITY`, dropping the column or the table drops it), and with the defaults it uses the column's own counter, which counts from 1 by 1. `ALTER COLUMN ... SET INCREMENT / RESTART` (changing the sequence through the column) isn't supported: use `ALTER SEQUENCE`.

**Generated columns:** `col type GENERATED ALWAYS AS (expr) STORED`, in `CREATE TABLE` and `ALTER TABLE ... ADD COLUMN` (which fills every existing row), works out `expr` from the row's other columns each time the row is inserted or updated, and keeps the result. A value written to it is refused (428C9; `DEFAULT` is fine), and the expression may read only the row's own columns, not another generated column (42P17), a subquery (0A000), or anything that isn't immutable, such as `random()` or `now()` (42P17); `VIRTUAL` (Postgres 18's) and `GENERATED BY DEFAULT AS (...)` are syntax errors, as in Postgres 16. A generated column can be indexed, `NOT NULL`, and read in a `WHERE`; a column it reads can't be dropped (2BP01) or have its type changed (0A000), but can be renamed. `ALTER COLUMN c DROP EXPRESSION [IF EXISTS]` keeps the values and makes it a plain column; `SET DEFAULT` on one is 42601. `COPY` with no column list leaves generated columns out (both ways), and naming one is 42P10. `attgenerated`, `information_schema.columns.is_generated` / `generation_expression`, `pg_attrdef` (so `pg_dump` writes `GENERATED ALWAYS AS (...) STORED`) and `INCLUDING GENERATED` in `LIKE` know them.

`gen_random_uuid()` gives version-4 UUIDs, a good fit for rows made on branches.

## Dates and times

```sql
create table events (id serial primary key, at timestamptz default now(), day date, lasts interval);
select * from events where at > now() - interval '7 days' order by at desc;
select date_trunc('month', at) as m, count(*) from events group by m order by m;
select day + 7, extract(dow from day), at - '2024-01-01'::timestamptz from events;
```

- **Time zone:** `timestamptz` is stored as a moment (UTC). A session shows it in its time zone, UTC until it sets another:
  - `SET TIME ZONE 'Asia/Kolkata'` (also `SET timezone = ...`, `SET TIME ZONE INTERVAL '+05:30' HOUR TO MINUTE`, `SET TIME ZONE 5.5`, `LOCAL` / `DEFAULT` / `RESET timezone` for UTC; `SHOW timezone`). `SET LOCAL TIME ZONE` lasts until the transaction ends. A Postgres client can set it at connect, as the `TimeZone` parameter or in `options=-c TimeZone=...`.
  - In that zone: `timestamptz` prints with its offset (`2026-09-29 10:30:00+05:30`), text without a zone is read as its time, `current_date`, `localtimestamp` and `current_time` are its date and time, casts between `timestamptz` and `date`, `timestamp` or `time` convert through it, a `date` compared with a `timestamptz` is its midnight, and `date_trunc`, `extract` (not `epoch`) and `to_char` work in its clock.
  - Named zones come from the system's tz database (`/usr/share/zoneinfo`, or `TZDIR`), with their daylight saving time and their history, as in Postgres built with system tzdata: `SET TIME ZONE 'America/New_York'`, `'Europe/London'`, `'Asia/Kolkata'`, in any case (shown as the database spells it). A timestamptz is shown with the offset the zone had at that moment (`2024-07-01 08:00:00-04`), a wall-clock time that a change skips is read in the zone's time before the change and one it repeats in its time after, and `+ interval '1 day'` counts days on the zone's calendar (across a change it is 23 or 25 hours). Without a tz database (a minimal container: install `tzdata`) only UTC, fixed offsets, `Etc/GMT±n` and a short list of zones with one offset all year (`Asia/Kolkata`, `Asia/Tokyo`, ...) are known, and any other name is 22023. A string offset is POSIX, as in Postgres: `'UTC+5'` and `'+05'` are five hours *west* of UTC; a number or `INTERVAL` is east. `pg_timezone_names` and `pg_timezone_abbrevs` list them.
  - `AS OF '2026-09-20 10:00'` and world names like `main@2026-09-20 10:00` read the time as UTC unless it says otherwise.
  - JSON (`to_json`, `row_to_json`, the HTTP and MCP APIs, diffs) shows a `timestamptz` in UTC with its offset (`2026-09-29T05:00:00+00:00`), whatever the session's zone: the same moment, where Postgres's `to_json` would show the session's time.
- **Values:** `date '2024-03-10'`, `timestamp '...'` and `interval '1 day 02:00'` literals, or plain text where a date is expected, and `now()`, `current_date` and `current_timestamp`. Years before the common era are written and shown with `BC` (`0044-03-15 BC`; there is no year 0, and a year of one or two digits is of this century or the last, `24` being 2024 and `70` 1970). `infinity` and `-infinity` are dates and timestamps that sort past every other (`valid_to date default 'infinity'`); `infinity ± an interval or a number of days` is itself, `date - date` with one is 22008, `isfinite(x)` tells them, `date_trunc` leaves them and `to_char` gives null. `extract` of one is 0A000 (Postgres gives `Infinity`). `epoch`, `now`, `today`, `tomorrow` and `yesterday` are read as the moments they name. `generate_series(date, date, interval)` is over `timestamptz`, as in Postgres, and refuses an infinite bound (22023).
- **The clock, as in Postgres:** `now()`, `current_timestamp`, `transaction_timestamp()`, `localtimestamp`, `current_time` and `current_date` are fixed when the transaction begins (outside `BEGIN`, when the statement does), so `created_at = updated_at = now()` in two statements of one transaction agree, and so does a column's `DEFAULT now()`. `statement_timestamp()` is fixed when the statement begins, and `clock_timestamp()` reads the clock at each call (a `DEFAULT clock_timestamp()` too). Postgres's `current_time` carries a time zone; ours is a plain `time`.
- **Arithmetic:** timestamp ± interval (months by the calendar: Jan 31 + 1 month is the end of February), timestamp − timestamp, date ± days, and date − date. An interval times or divided by a number (`interval '1 hour' * 2.5`, `/ 4`) keeps whole months and days and cascades the fractions down (a fraction of a month into days, of a day into time), as Postgres's `interval_mul` and `interval_div` do; `sum`, `avg`, `min` and `max` of intervals work (`avg` is the sum over the count); `justify_hours`, `justify_days` and `justify_interval` turn 24 hours into a day and 30 days into a month; and `(start, end) OVERLAPS (start, end)` (an end may be an interval: the period's length) follows Postgres's null rules. A bare number in an interval is seconds (`interval '5'`, `'5 days 3'`).
- **`AT TIME ZONE`:** `timestamp AT TIME ZONE zone` reads a wall clock in the zone and gives the timestamptz (a date is its midnight), `timestamptz AT TIME ZONE zone` gives the wall clock there as a timestamp, and `timezone(zone, value)` is the same with the operands the other way round. The zone is a name from the tz database (`America/New_York`, `UTC`, `Etc/GMT+5`), an abbreviation (`PST`, `CEST`: its offset all year, as Postgres's default abbreviation set has it), a POSIX offset string (`'+05:30'`, `'UTC+3'`: west of UTC for `+`, as Postgres reads it), or an interval (`interval '05:30'`, east of UTC; not with months or days). It binds tighter than `*` and `+` and looser than `::`, as in Postgres, so `a + b AT TIME ZONE 'UTC'` is `a + (b AT TIME ZONE 'UTC')`. `date_trunc(unit, timestamptz, zone)` and `generate_series(from, to, step, zone)` take a zone too, and a timestamptz can be written with one: `'2024-03-15 12:00:00 America/New_York'`, `'... EST'`. `time AT TIME ZONE` (a timetz) is not supported.
- **Building and reading:** `make_date`, `make_time`, `make_timestamp` and `make_timestamptz(y, m, d, h, mi, s [, zone])` (a negative year is BC; `24:00:00` is a valid time), and `to_date(text, template)` / `to_timestamp(text, template)`, which read a date and time out of text by Postgres's patterns: `YYYY YYY YY Y`, `MM`, `DD`, `DDD`, `HH HH12 HH24`, `MI`, `SS`, `MS`, `US`, `FF1`-`FF6`, `SSSS`, `AM`/`PM` (also `A.M.`), `Month`/`Mon` (in any case), `Day`/`Dy`, `BC`/`AD`, `Q`, `W`, `WW`, `IYYY`/`IW`/`ID` (ISO weeks), `CC`, `J`, `TZH`/`TZM`, quoted text, `FM` and a `TH` suffix. A space or punctuation mark in the template takes the text's whitespace and one mark; digits are read as wide as the pattern unless another number follows (then exactly that wide); the text may end before the template does, and what is left over is ignored; what is not given is year 0 (1 BC), January, the 1st. `TZ`, `OF`, `RM` and `SP` are only for `to_char` (0A000). Not the same as Postgres: a few odd combinations of ISO-week patterns.
- **Functions:** `date_trunc(unit, …)`, `extract(field from …)` and `date_part(field, …)`. `extract` gives a numeric, as Postgres 14 and later does (`2024`, not `2024.0`), at the scale Postgres writes each field with: whole numbers for `year`, `month`, `day`, `hour`, `minute`, `quarter`, `dow`, `isodow`, `doy`, `week`, `isoyear`, `century`, `decade`, `millennium`, `microseconds` and `timezone*`; `second` and `epoch` with six decimals (`45.678901`, `1710513045.678901`; a date's epoch is a whole number), `milliseconds` with three, `julian` a whole number for a date and 20 decimals for a timestamp. `date_part` gives the same value as a double precision. A field that a type has no such part of (`hour` of a date, `day` of a time, `dow` of an interval) is 0A000, one Postgres does not have 22023. An interval's `epoch` counts a year as 365.25 days and a month as 30, as Postgres does; `interval '100:00:00'` reads as 100 hours.
- **Times of day:** `time` (`time without time zone`), e.g. `'09:30'`, `time '17:00:00.5'`, `localtime`.
  - time ± interval moves the clock and wraps past midnight, time − time is an interval, date + time is a timestamp, `ts::time` takes a timestamp's time of day, and `extract(hour | minute | second from t)` works.
  - `current_time` is the time now in the session's zone, with no zone of its own. There's no `time with time zone`: use `timestamptz`.
- **Outside SQL:** JSON shows ISO 8601.

## Changing tables

Rows aren't rewritten when a table changes. Each packed row records the column layout it was written with, so:
- **Old rows keep working:** after columns are added, dropped, renamed or retyped, old rows read through their own layout.
- **Across branches:** a branch that changed a table merges cleanly with rows another branch wrote under the old layout.
- **Type changes:** changing a type checks every value converts first, and refuses if one doesn't.
- **Renaming a table** moves its rows.
- **Limit:** rows stored as JSON (through the JSON commands) are matched by name, so a renamed column doesn't find their old field.

- **Merging by columns:** `alter table leads set (merge_by_columns = true)` makes merges into this table combine a row both sides changed when they changed different columns, as `MERGE ... BY COLUMNS` does; `alter table leads reset (merge_by_columns)` goes back to whole rows (the default). Set it on tables whose columns don't depend on each other: two agents changing `status` and `balance` of one account would otherwise combine into a row neither checked. It's a change to the table: `DIFF` shows it (`merge: by columns`), and under a merge policy it's a schema change.
- **What migration tools write:** `ALTER TABLE ONLY t ...` (as `pg_dump` and Rails's `structure.sql` do), `ADD CONSTRAINT ... NOT VALID` and `VALIDATE CONSTRAINT name`, `ALTER COLUMN c TYPE t USING c` and `USING c::t` (Django's and Alembic's; an expression that computes something else is 0A000), `SET SCHEMA s` (moves the table), and `ALTER INDEX a RENAME TO b` (a unique constraint's index and the constraint with it). **`NOT VALID` checks the rows there are when the constraint is added**, where Postgres adds it unchecked: a table that already breaks the rule fails here (23514, 23503, 23505). `OWNER TO role` gives the table to the role ([Roles and privileges](#roles-and-privileges)). `SET (fillfactor = 70, autovacuum_enabled = false, ...)`, `SET TABLESPACE` and `SET LOGGED` succeed and change nothing (a notice says so): there are no pages or tablespaces here.
- **A primary key later:** `alter table t add primary key (col)`, or `(a, b)` (or a new column declared `primary key` on an empty table). Every row moves to its key in one step: the key must be unique and never null (23505, 23502), and its columns become `NOT NULL`.

## Keys of several columns

```sql
create table order_lines (ord int, line int, sku text, qty int, primary key (ord, line));
select * from order_lines where ord = 7 and line = 2;   -- one row, read by its key
select * from order_lines where ord = 7;                 -- the rows whose keys start with 7
create table shipments (id int primary key, ord int, line int,
                        foreign key (ord, line) references order_lines on delete cascade);
insert into order_lines values (7, 2, 'a', 5) on conflict (line, ord) do update set qty = excluded.qty;
```

- **Types:** each column an integer, numeric, double precision, text, bytea, uuid, boolean, date, time, timestamp, timestamptz or an address type. The columns are `NOT NULL`. A one-column key of a type other than integer, bigint, text, bytea, uuid or an address can't have its type changed (`ALTER COLUMN ... TYPE` is 0A000).
- **Stored in order:** a row's key holds its key's values in a form that sorts as they do (numbers as numbers, text by its bytes, one column after another) and can't be mistaken for another row's. So a table's rows come out in key order, equality on all the key's columns reads one row, and equality on its first columns reads just the rows that start with them (`EXPLAIN` shows `Index Scan using order_lines_pkey`). Other conditions scan or use indexes as usual; joins on the key's columns hash-join.
- **Shown and typed as rows:** `DIFF`, `MERGE ... DRY RUN`, merge conflicts, error messages, the shell and the HTTP API show such a key as Postgres writes a row, `order_lines/(7,2)`, and take it that way: `get order_lines (7,2)` in the shell, `{"table": "order_lines", "id": "(7,2)"}` over HTTP, and `RESOLVE ('order_lines/(7,2)' = ours)`. JSON writes (`put order_lines (7,2) {...}`) fill the key's columns from the key.
- **`ON CONFLICT`** names the key's columns in any order, or `ON CONFLICT ON CONSTRAINT order_lines_pkey`.
- **Keys of one column** are stored as before (`users/42`), so databases made before read the same.
- **Not allowed:** dropping a key column (2BP01), or changing its type (0A000) other than between integer and bigint.

## Loading and unloading: COPY

```sql
copy orders from stdin (format csv, header);          -- then the rows, as Postgres clients send them
copy orders (id, total) from stdin;                   -- text format: tab-separated, \N for NULL
copy orders to stdout (format csv, header);
copy (select id, total from orders where total > 100) to stdout with csv;
copy (delete from orders where total = 0 returning id) to stdout;   -- the rows a statement returns
```

```bash
psql postgres://127.0.0.1:5433/main -c "\copy orders from 'orders.csv' csv header"   # a file on your side
psql postgres://127.0.0.1:5433/main -c "\copy orders to 'orders.csv' csv header"
```

- **Over the Postgres protocol:** `COPY ... FROM STDIN` and `COPY ... TO STDOUT` use Postgres's copy messages, so psql's `\copy`, `pg_dump`-style scripts with the data inline, and drivers' copy support (psycopg's `cursor.copy`, the `postgres` crate's `copy_in` / `copy_out`, ...) work. Through the simple or the extended protocol.
- **Formats:** `text` (the default), `csv`, and (FROM only) `binary`, Postgres's binary format as bulk loaders send it, with every type binary parameters take, pgvector's vectors included (psycopg: `cur.copy("copy items from stdin (format binary)")`). Text and CSV take Postgres's options: `HEADER`, `DELIMITER`, `NULL`, `QUOTE`, `ESCAPE`, `FORCE_QUOTE (cols)` or `*`, `FORCE_NOT_NULL`, `FORCE_NULL` and `ENCODING 'UTF8'`, in the `WITH (...)` form or the older one psql writes (`csv header delimiter ';'`). Values are written as Postgres writes them (`t`/`f`, `\N`, quoting only where needed), so a table copied out and in again comes back byte for byte.
- **All or nothing:** a bad value, a missing or extra column, a duplicate key, or a client that cancels (CopyFail) or disconnects leaves nothing behind. Inside `BEGIN`, a failed COPY fails the transaction, as in Postgres. Errors name the line and column: `invalid input syntax for type integer: "x" (COPY t, line 45000, column v)`.
- **Fast, and one statement however big:** rows are parsed to their columns' types as they arrive and go in as one insert of values (no SQL to parse), a part at a time (16,384 rows or 32 MB) while the client is still sending. A COPY too big to hold goes on in parts, as any big `INSERT` does, so memory doesn't grow with it (see [operations](operations.md#databases-bigger-than-memory)). In process, 100,000 three-column rows load at about 280,000-390,000 rows a second, against about 170,000-200,000 for `INSERT`s of 1,000 rows each. Over the network with psql, psql itself sets the pace.
- **Checked on insert:** `NOT NULL`, `CHECK`, `UNIQUE`, foreign keys and indexes, and defaults for columns the COPY leaves out, exactly as `INSERT` does.
- **Triggers:** a COPY FROM fires `INSERT` triggers as an `INSERT` does: row triggers for each row, and statement triggers once for the whole COPY. A table with triggers takes the COPY in memory.
- **Refused:** files and programs on the server (`COPY t FROM '/path'`, `PROGRAM`): 42501. The client reads and writes its own files (`\copy`). Also `COPY ... TO` in binary and `COPY ... WHERE` (0A000). Over HTTP and in the shell, COPY needs a Postgres connection.
- **Loading a lot:** use COPY, or one `INSERT ... SELECT` or big multi-row `INSERT`. A statement bigger than half of `CHRONOS_WORK_MEM` skips the log: its rows are written once, into the table's tree, and parts in key order (ids as they come) go straight there. Many small transactions (`INSERT`s of 100 rows, 1,000 per `COMMIT`) write each row twice, to the log at commit and into the tree at the next checkpoint, as Postgres writes its WAL and then its tables. On 76,424 OpenAI embeddings: a binary COPY wrote 526 MB, the small-transaction load 1.27 GB.


## uuid

```sql
create table accounts (id uuid primary key default gen_random_uuid(), owner text);
insert into accounts (id, owner) values ('A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11', 'ada');
select id from accounts where id = 'a0eebc999c0b4ef8bb6d6bb9bd380a11';   -- found: the same uuid
```

- **As in Postgres:** `uuid` is its own type (id 2950): drivers get a UUID, not text (sqlx's `Uuid`, pgx's `pgtype.UUID`, npgsql's `Guid`, JDBC's `getObject(UUID)`), in binary as 16 bytes and in text as lowercase `8-4-4-4-12`. Input takes the spellings Postgres takes (upper or lower case, without hyphens, in braces, hyphens after any group of four digits) and anything else is 22P02. A literal compared with a uuid is read as one, so `id = 'A0EE...'` finds `a0ee...`; uuids order and compare as their 16 bytes do; `id::text` and `'...'::uuid` cast both ways; `gen_random_uuid()` (and `uuid_generate_v4()`) give a `uuid`. It can be a primary key, a `UNIQUE` column, an indexed column, and the target of a foreign key.
- **Before this:** `uuid` was `text` underneath, so `'A0EE...'` and `'a0ee...'` were two different values and `'not-a-uuid'` was accepted. Tables made then keep `text` columns (their schema says so); make the column `uuid` with `ALTER TABLE t ALTER COLUMN id TYPE uuid` to have the new behaviour.

## Binary strings (bytea)

`bytea` holds bytes. It is written as `'\xdeadbeef'` (hex, whitespace between bytes allowed) or in the older escape form (`'abc\000def'`, a backslash as `\\`), read back as `\x` and hex (Postgres's `bytea_output = hex`), and travels in binary as the bytes themselves, so a driver's `Vec<u8>`, `Buffer`, `bytes` or `LargeBinary` round-trips byte for byte. Values are compared bytewise, so they sort, group, join and index (`CREATE INDEX`, `UNIQUE`, `PRIMARY KEY` of one column or several) as Postgres's do. Casting `text` to `bytea` gives the string's UTF-8 bytes (`'héllo'::text::bytea`), the other way the `\x` text; a number is not a `bytea` (42846).

- **Functions:** `length` and `octet_length` (bytes), `bit_length`, `substring(b FROM n FOR m)` / `substr`, `position(x IN b)` / `strpos`, `overlay(b PLACING x FROM n [FOR m])`, `||` (also `string_agg`), `get_byte`, `set_byte`, `encode(b, 'hex' | 'base64' | 'escape')`, `decode(s, ...)`, `md5(b)`, `sha224(b)`, `sha256(b)`, `sha384(b)`, `sha512(b)` (and pgcrypto's `digest` and `hmac`, below), and `convert_to(s, 'UTF8' | 'LATIN1' | 'SQL_ASCII')` / `convert_from`. `overlay(s PLACING ...)` works on text too.
- **Errors** are Postgres's: `'\xzz'` and an odd number of digits are 22023, a bad escape is 22P02, `get_byte` past the end is 2202E, a negative `substring` length is 22011.
- **Stored as** the bytes' hex text, so a value takes twice its size on disk and in memory; a multi-megabyte blob is better kept as a file with its path in the row.
- **Not here:** `bytea_output = 'escape'` (results are always hex), `bytea` in a `LIKE`, arrays of `bytea` as more than text, `min` / `max` (Postgres has none either), and `COPY ... (FORMAT binary)` out.

## Enum types

```sql
create type mood as enum ('sad', 'ok', 'happy');
create table people (id int primary key, name text, feeling mood default 'ok');
select name from people where feeling > 'sad' order by feeling desc;
alter type mood add value 'meh' before 'ok';
alter type mood rename value 'sad' to 'down';
select enum_range(null::mood);   -- {down,meh,ok,happy}
drop type mood;                  -- once nothing uses it (or CASCADE)
```

- **As in Postgres:** a value must be one of the labels (22P02 otherwise), and enums compare, sort (`ORDER BY`), `min` and `max` in the order the labels were given, not as text. `'happy'::mood` casts and checks; `feeling::text` is the label.
- **Types are rows of the world:** a type made on a branch arrives with its merge, and forks share it. A table's schema keeps its enum's labels too.
- **Everywhere a column goes:** defaults, `ALTER TABLE ... ADD COLUMN` and `TYPE`, indexes (for `=`), `UNIQUE`, COPY and JSON writes (checked the same). Clients receive labels as text.
- **Changing a type:** `ALTER TYPE t ADD VALUE [IF NOT EXISTS] 'v' [BEFORE | AFTER 'w']`, `ALTER TYPE t RENAME VALUE 'a' TO 'b'` and `ALTER TYPE t RENAME TO u`, as in Postgres: existing rows keep their labels, in the new order, and a renamed label is renamed in every row, index and column default that holds it (Chronos keeps labels as text, so the rows holding it are rewritten, in the same write). A change on a branch arrives with its merge, and `AS OF` sees the type as it was. Renaming a type that a view's column has is refused (0A000): drop the view, rename, make it again.
- **`DROP TYPE ... CASCADE`** drops the columns of the type (as `ALTER TABLE ... DROP COLUMN`) and the functions taking or returning it, with Postgres's notices; without `CASCADE`, any of those stops it (2BP01).
- **Functions:** `enum_first(x)`, `enum_last(x)` and `enum_range(x [, y])` (a text array, `{sad,ok}`), taking the type from their argument (`null::mood`, a column). Enums can be a function's arguments and result, `RETURNS SETOF` / `TABLE` columns, and PL/pgSQL variables, checked (22P02) and compared by their labels' order.
- **Errors:** a type that exists (42710), a label given twice (23505) or added twice (42710), a label that isn't one (22023 in `ALTER TYPE`, 22P02 elsewhere), `DROP TYPE` of a type something uses (2BP01), an unknown type (42704).
- **Not yet:** enums in primary keys, and `CHECK` conditions or partial indexes that name a renamed label (they keep the old one). `DIFF` doesn't list types (the tables using one show its labels).

## Schemas

```sql
create schema app;
create table app.orders (id int primary key, customer int references public.customers);
set search_path to app, public;      -- now `orders` is app.orders, `customers` public.customers
select * from orders join customers c on c.id = orders.customer;
drop schema app cascade;             -- and everything in it
```

- **Names:** `schema.name` works for tables, views, materialized views, sequences, enum types, functions, triggers' tables and indexes, anywhere they're named (DDL, `INSERT`/`UPDATE`/`DELETE`, `FROM`, joins, `REFERENCES`, `COPY`, `nextval('app.s')`, `'app.t'::regclass`). A table in another schema is called by its own name in the query (`select t.id from app.t`), as in Postgres. Default constraint and index names don't carry the schema (`t_pkey`); an index is in its table's schema (`drop index app.t_v_idx`).
- **`search_path`:** `SET search_path TO a, public` (or `= 'a, public'`, `SET SCHEMA 'a'`), `RESET search_path` and `SHOW search_path`, per session. A name without a schema is the first of its kind along the path (a temporary table first); `CREATE` without a schema makes it in the path's first schema that exists (3F000 if none). `current_schema()` and `current_schemas(bool)` follow it. The default is `"$user", public`; `$user` schemas aren't looked for. In a script of several statements, each sees the path and objects the ones before it left, as in Postgres.
- **`CREATE SCHEMA [IF NOT EXISTS] name [AUTHORIZATION role]`** (42P06 if it exists; names starting `pg_` are 42939), and **`DROP SCHEMA [IF EXISTS] a, b [CASCADE | RESTRICT]`**: without `CASCADE`, a schema holding anything is 2BP01; with it, its views, tables (and foreign keys and views elsewhere that depend on them), sequences, functions and types go too, in one step. `public` can't be dropped.
- **World data:** a schema is a row of the world, like a table's schema, and objects in it are stored under their qualified name (`app.orders/7`); objects in `public` keep their plain names, so databases made before schemas read the same. Forks, `DIFF` (and `DIFF ... AS SQL`, as `create schema app;`), merges (the same schema made on both sides merges as one), `AS OF` and restarts all carry them.
- **psql:** `\dn` lists schemas, `\dt app.*` a schema's tables, `\dt` the ones the search_path shows, and `\d app.t` a table in one.
- **Differences from Postgres:** a view's names are looked up along the search_path of the session reading it (Postgres fixes them when the view is made); a prepared statement keeps the tables its names meant when it was prepared; `CREATE SCHEMA ... CREATE TABLE ...` (objects inside the statement) is 0A000.

## Temporary tables

```sql
create temp table picks (id int primary key, score float8);
insert into picks select id, random() from products where stock > 0;
select p.name from products p join picks using (id) order by score limit 10;
```

- **The session's own:** `CREATE TEMP` (or `TEMPORARY`) `TABLE` makes a table only this connection sees, in memory: it's never written to the log, never in a `DIFF`, never merged, and it's gone when the connection ends (or `DROP TABLE`). A temporary table hides a world's table of the same name, as in Postgres.
- **With everything else:** it can be read alongside the world's tables (joins, subqueries, `INSERT INTO world_table SELECT ... FROM temp_table`), and filled from them (`INSERT INTO temp_table SELECT ... FROM world_table`), with the usual constraints, defaults and indexes. `BEGIN ... ROLLBACK` takes back its changes too.
- **Not yet:** savepoints (`ROLLBACK TO` leaves a temporary table's changes), subqueries on the world's tables inside an `UPDATE` or `DELETE` of a temporary table, foreign keys between temporary and other tables, and psql's `\d` for them.

## Prepared statements

```sql
prepare by_customer (int) as select * from orders where customer = $1 order by id;
execute by_customer(42);
deallocate by_customer;          -- or: deallocate all
```

- **As in Postgres:** parameter types given in parentheses or inferred from use, `EXECUTE name(values)` with the values converted to those types, and Postgres's errors (42P05 a name already used, 26000 no such statement, 42601 the wrong number of values). `SELECT`, `INSERT`, `UPDATE` and `DELETE` can be prepared.
- **One namespace with the protocol's:** a statement a driver prepares (Parse) and one `PREPARE` makes share names on a connection, so `EXECUTE` runs either and `DEALLOCATE` frees either, as in Postgres.
- **Per session:** they last until the connection ends (or, over HTTP, until the request ends).

## Comments

```sql
comment on table orders is 'One row per checkout';
comment on column orders.total is 'In cents, tax included';
select obj_description('orders'::regclass, 'pg_class');    -- One row per checkout
select col_description('orders'::regclass, 3);             -- In cents, tax included
```

- **As in Postgres:** `COMMENT ON TABLE | VIEW | MATERIALIZED VIEW | SEQUENCE | INDEX | COLUMN | CONSTRAINT ... ON table | SCHEMA name IS 'text'`, with `NULL` (or an empty string) taking it away, `'it''s'`, `E'...'` and `$$...$$` text, and Postgres's errors (42P01 no such relation, 42703 no such column, 42809 not that kind of object, 42704 no such constraint, 3F000 no such schema). `obj_description(oid [, catalog])`, `col_description(oid, n)` and `pg_description` read them (`pg_dump` and psql's `\d+` and `\dt+` do too), and `pg_dump` writes `COMMENT ON` statements.
- **A comment goes with its object:** dropping the table, column, index or constraint (or the schema, with its tables) drops it, and renaming a table, a column or an index moves it, as in Postgres. A table dropped and made again has none.
- **In the world:** a comment is a row of the world (`_sonos_comments/...`), so a fork has its own, `DIFF` shows a change to one (`DIFF ... AS SQL` as `COMMENT ON`, after what it's on), a merge brings it (both sides commenting one object differently is a conflict, as for any row), and it survives restarts. Writing one is a write: an agent that can only read can't.
- **Not kept:** comments on functions, procedures, types, triggers, extensions, databases, roles and the rest say so in a notice and are dropped, and so does one on a temporary table.

## Cursors

```sql
begin;
declare orders_cur scroll cursor for select * from orders order by id;
fetch 100 from orders_cur;         -- the next 100 rows
fetch backward 10 from orders_cur; -- and back over 10
move absolute 0 in orders_cur;     -- to before the first row
close orders_cur;
commit;
```

- **As in Postgres:** `DECLARE name [INSENSITIVE] [[NO] SCROLL] CURSOR [WITH | WITHOUT HOLD] FOR query`; `FETCH` and `MOVE` with `NEXT`, `PRIOR`, `FIRST`, `LAST`, `ABSOLUTE n`, `RELATIVE n`, a count (`FETCH 5`, `FETCH -2`), `ALL`, `FORWARD [n | ALL]` and `BACKWARD [n | ALL]`, `FROM` or `IN` (or neither: `FETCH cur`); `CLOSE name` and `CLOSE ALL`. Tags count the rows: `FETCH 3`, `MOVE 2`. A cursor without `WITH HOLD` needs a transaction block (25P01) and ends with it (a `ROLLBACK TO` closes the ones declared after its savepoint); `WITH HOLD` ones last until `CLOSE` or `DISCARD ALL`. Errors as Postgres's: 42P03 a name already used, 34000 no such cursor, 55000 a backward fetch on a `NO SCROLL` cursor, 25P01 outside a transaction. This is what psycopg's named cursors, JDBC's `setFetchSize` (through the protocol's row limit, below) and psql's `\set FETCH_COUNT` use.
- **A snapshot:** the query runs when the cursor is declared and its rows are kept for the cursor, so rows other sessions (or this one) change or add afterwards don't show in it, as in Postgres, where a cursor reads the snapshot of its `DECLARE`. It holds all those rows in memory until it's closed, so a cursor over a table of millions of rows costs that much: a `LIMIT` in the query, or keyset pagination, keeps it small.
- **Not here:** `BINARY` cursors (0A000), a query other than a `SELECT` or `VALUES` (42601), and `WHERE CURRENT OF cursor` in an `UPDATE` or `DELETE`. A cursor a PL/pgSQL function opened (a `refcursor` it returned) is fetched, moved and closed like one declared here, until its transaction ends; a `ROLLBACK` closes a `WITH HOLD` cursor declared in its transaction, as in Postgres. Postgres refuses a backward fetch on a cursor whose plan can't scan backward (an aggregate, say); here a cursor without `NO SCROLL` scans backward whatever it reads.
- **Over the protocol:** an Execute with a row limit sends that many rows, then `PortalSuspended`, and the next Execute on the portal goes on from there (as JDBC's `setFetchSize` and `postgres`'s `query_portal` do); a portal that has run to its end is 55000 if run again, and one left part-read is dropped when a Sync ends a statement outside a transaction. The rows of the statement are found when it's first executed, all at once, and sent a limit at a time.
- **Agents:** `DECLARE` is checked as its query is (a read for a `SELECT`); `FETCH`, `MOVE` and `CLOSE` are reads, since what they show was checked when the cursor was declared.

## psql's commands

`\dt`, `\dv`, `\dm`, `\ds`, `\d`, `\d name` (tables with their triggers, views, materialized views and sequences), `\d+ view` (with its definition) and `\l` (which lists branches as databases) work in psql. Chronos recognises the catalog queries psql 16 sends and answers them from its own tables. Other queries on the catalog go to the SQL engine, which knows [these catalog tables](#the-catalog-information_schema-and-pg_catalog).

## Views

```sql
create view paid (id, who, total) as select id, customer, total from orders where status = 'paid';
create or replace view paid as select id, customer, total, placed from orders where status = 'paid';
select who, sum(total) from paid group by who;
update paid set total = total * 0.9 where who = 'ada';   -- writes orders, only its paid rows
drop view paid;                                          -- or: drop table orders cascade
```

- **Anywhere a table is read:** `FROM`, joins, subqueries, `INSERT ... SELECT`, other views. A view's query runs as part of the query that reads it.
- **In the world:** a view's definition is a row of the world, like a table's schema. A fork has its own views, `DIFF` shows them made, changed and dropped (`DIFF ... AS SQL` as `CREATE OR REPLACE VIEW`), a merge brings them, and they survive restarts.
- **AS OF:** `select * from v as of '-1 hour'` reads the view's definition as it was then, over its tables as they were then.
- **`CREATE OR REPLACE`:** as in Postgres, the new query keeps the old columns (names and types) first and may add more (42P16 otherwise).
- **Dependencies:** dropping a table or view another view reads fails with 2BP01 unless it's `DROP ... CASCADE`, which drops the views too (and, for a table, the foreign keys referencing it).
- **Writing through a view:** `INSERT`, `UPDATE` and `DELETE` work on views that select columns of one table (or of such a view), without `DISTINCT`, `GROUP BY`, aggregates, window functions, `LIMIT` or `UNION`, as Postgres's automatically updatable views: they write the table, `UPDATE` and `DELETE` only the rows the view shows, and `RETURNING` gives the view's columns. Other views are refused (55000), and so is writing a view column that's an expression (0A000). `WITH [LOCAL | CASCADED] CHECK OPTION` works as in Postgres: an `INSERT` or `UPDATE` through the view that writes a row the view wouldn't show is 44000, LOCAL checking the view's own condition (and those of views below it that have an option), CASCADED every view below; only on views that can be written through (0A000 otherwise). `information_schema.views.check_option` says which.
- **Tables, views, materialized views and sequences share names** (42P07).
- **Renaming:** `ALTER VIEW [IF EXISTS] v RENAME TO w`, `ALTER MATERIALIZED VIEW ... RENAME TO` and `ALTER TABLE v RENAME TO w` on a view, with Postgres's errors (42P01, 42809 for the wrong kind, 42P07 a name taken); the view's comment goes with it. `RENAME COLUMN` on a view is 0A000.
- **Differences from Postgres:** a view is kept as its SQL and read by name each time, so `select *` in a view picks up columns added to its table later, renaming a table or view another view reads is refused (drop that view, rename, make it again), and a view reading a dropped or renamed column fails when it's read.

## Materialized views

```sql
create materialized view daily as select date_trunc('day', at) as day, sum(total) from orders group by 1;
refresh materialized view daily;
create materialized view later as select ... with no data;  -- reading it fails until a REFRESH
drop materialized view daily;
```

- **A table holding its query's rows,** made by `CREATE MATERIALIZED VIEW [IF NOT EXISTS] name [(columns)] AS query [WITH [NO] DATA]`. It reads like a table, takes indexes, and is written only by `REFRESH MATERIALIZED VIEW name [WITH [NO] DATA]` (42809 for `INSERT`, `UPDATE`, `DELETE`, `TRUNCATE`, `ALTER TABLE`).
- **Per world:** a REFRESH in a fork changes only the fork until it merges. Rows are kept by their place in the answer, so a REFRESH that finds the same rows changes nothing, and `DIFF` shows only the rows that did change (`DIFF ... AS SQL` as a `REFRESH`).
- `CONCURRENTLY` is accepted: a REFRESH never blocks readers anyway.

## Sequences

```sql
create sequence order_no start 1000 increment by 10;
select nextval('order_no'), currval('order_no'), lastval();
select setval('order_no', 5000);            -- or setval('order_no', 5000, false): 5000 comes next
alter sequence order_no restart with 1;     -- also INCREMENT, MINVALUE, MAXVALUE, CYCLE, ...
create table orders (id bigint primary key default nextval('order_no'), total numeric);
drop sequence order_no;
```

- **Options:** `AS smallint | integer | bigint`, `INCREMENT [BY]` (negative counts down), `MINVALUE` / `NO MINVALUE`, `MAXVALUE` / `NO MAXVALUE`, `START [WITH]`, `[NO] CYCLE`, and `ALTER SEQUENCE ... RESTART [WITH n]`, checked as Postgres checks them. `CACHE` is accepted and changes nothing. `OWNED BY table.column` (also `CREATE SEQUENCE ... OWNED BY`) is kept: `pg_depend` shows it, `pg_dump` writes it, and dropping the table drops the sequence; `OWNED BY NONE` lets go. Past the end without `CYCLE`, `nextval` fails with 2200H.
- **Read as a table:** `select last_value, log_cnt, is_called from order_no` gives one row, as in Postgres: the last value handed out and `true`, or (none yet) the value it counts from and `false`. `pg_sequences.last_value` is null until one was handed out.
- **Across worlds:** the definition is a row of the world, like a table's schema (a fork, `DIFF`, a merge and AS OF see it), but its counter belongs to the database, shared by every world holding that sequence, as `serial` columns' are, and isn't rolled back: rows numbered in two forks never collide when they merge. `nextval` in a world writes no world's rows (main's version doesn't move). A sequence created separately in two worlds under one name has two counters, and merging both definitions is a conflict. `setval` and `RESTART` move the shared counter for every world (from any world, without writing main), and `TRUNCATE ... RESTART IDENTITY` restarts its tables' `serial` counters for every world too. Restoring or reading the past doesn't move a counter back.
- **In a session:** `currval` and `lastval` are this session's last values (55000 before any), as in Postgres; a `DEFAULT nextval(...)` sets them too.
- **Differences from Postgres:** `setval` works in `SELECT` (not inside `INSERT`, `UPDATE` or `DELETE`); a write taking many values at once may skip some (it reserves more when it runs out). `ALTER SEQUENCE ... OWNER TO` is accepted with a notice: nothing has an owner here.

## LISTEN and NOTIFY

```sql
listen jobs;                               -- this connection hears the channel
notify jobs, 'order 42 paid';              -- or: select pg_notify('jobs', 'order 42 paid')
unlisten jobs;                             -- or: unlisten *
```

- **When:** at `COMMIT` (or at once outside a transaction); a `ROLLBACK`, a failed statement or a `ROLLBACK TO SAVEPOINT` drops the notifications it made. The same channel and payload twice in one transaction arrive once. Payloads are shorter than 8000 bytes (22023).
- **Who hears it:** connections that LISTEN on the channel in the same world, the sender included; each gets Postgres's NotificationResponse with the sender's backend id. An idle connection gets it at once, without sending a query (psql shows it at its next command); a connection inside a transaction gets it when the transaction ends.
- **Worlds:** a world's notifications reach its own listeners only: a NOTIFY in a fork doesn't reach main, and a merge doesn't send it (notifications are messages at commit time, not rows). For a notification that should happen when the work reaches main, use `NOTIFY ON MERGE` (below). A LISTEN is for the world the connection is in when it runs, and stays with that world if the connection switches.
- **Differences from Postgres:** a LISTEN inside a transaction takes effect at once, not at COMMIT.

### Effects on merge: NOTIFY ON MERGE and the outbox

An agent working in a world shouldn't send the refund email before anyone has approved the refund. `NOTIFY ON MERGE` queues a notification in the world instead. Nobody hears it while the world is worked on, it's dropped with the world if the world is discarded, and it happens when a merge brings it into `main`:

```sql
-- in world fix (an agent's)
update orders set status = 'refunded' where id = 42;
notify on merge billing, '{"refund": 42}';

-- a person merges fix: listeners on main hear  billing  '{"refund": 42}'  now,
-- and it waits in main's outbox until a worker says it's done
show outbox;
--            id                  | channel |    payload     | world |          queued           | by  | state  | attempts | worker | lease_until | merged_from |      approved
-- -------------------------------+---------+----------------+-------+---------------------------+-----+--------+----------+--------+-------------+-------------+--------------------
--  1790590000000-9f2c41d07e6a13b5 | billing | {"refund": 42} | fix   | 2026-09-28 10:00:00.00+00 | bot | queued |        0 |        |             | fix         | merged by a person

-- a worker: take it, do it, say so
claim outbox for billing lease '30 seconds';   -- the same columns, state claimed, attempts 1
ack outbox '1790590000000-9f2c41d07e6a13b5';   -- ACK 1: done
```

Each entry in `main` is queued, claimed or ambiguous, so a worker that dies between calling a receiver and saying so never makes an effect vanish, or go out twice without anyone seeing:

```
queued ──CLAIM OUTBOX──> claimed (a worker, a lease, attempt n) ──ACK OUTBOX──> gone: done
                         claimed ──RETRY OUTBOX 'id' ATTEMPT n──> queued (the worker knows it wasn't sent)
                         lease runs out, claimed IDEMPOTENT ──> queued (the receiver drops a repeat)
                         lease runs out, otherwise ──> ambiguous: it may have reached the receiver
                         ambiguous ──ACK or RETRY──> once someone checked the receiver
```

- **An entry, not a message:** it's a row of the world, so a `ROLLBACK` drops it, a transaction's `COMMIT` keeps it, and a merge into another world carries it along. Only reaching `main` sends it: a merge, a transaction's `COMMIT` on `main`, or `NOTIFY ON MERGE` run on `main` itself (heard as the statement or its transaction ends). A merge sends it with backend id 0.
- **Kept until acknowledged:** after the NOTIFY, the entry waits in `main`'s outbox, so work a worker missed (it wasn't listening, or crashed) is still there. A worker listens (the NOTIFY is its wake-up), claims, does the work, then runs `ACK OUTBOX 'id', ...`, which removes them (`ACK n`; ids not there are skipped, so acknowledging twice is fine). The database never sends anything out itself: no webhooks, no mail, no secrets.
- **CLAIM OUTBOX [FOR channel, ...] [LIMIT n] [LEASE 'interval'] [IDEMPOTENT]:** takes up to `n` (default 1) queued effects, oldest first, on those channels (default any), for the lease (default 5 minutes), and returns them as `SHOW OUTBOX` shows them. Two claims never get the same effect. The id is the same from queueing to acknowledgement: pass it to the receiver as an idempotency key where it takes one, and claim with `IDEMPOTENT` then.
- **A lease that runs out** puts an `IDEMPOTENT` claim back in the queue. Any other becomes `ambiguous`: the worker may have called the receiver and died before `ACK`. No `CLAIM` returns it, and it stays in `SHOW OUTBOX` until someone checks the receiver and runs `ACK OUTBOX` (it went out) or `RETRY OUTBOX 'id' ATTEMPT n, ...` (it didn't: queued again, `RETRY n`). `n` is the `attempts` its claim showed: only that attempt is retried, so a retry that arrives late, or twice, after the effect was claimed again never takes the new claim from its worker (`RETRY 0`). A worker that knows a call failed retries it at once the same way.
- **Who:** `NOTIFY ON MERGE` is a write to the session's world (an agent needs to be allowed to write it; one keeping to a merge policy can't queue on `main` directly). Merge policies count effects apart from rows (`max_effects`, `effects_per_hour`, `review_channels`: see [merge policies](reference/worlds.md#merge-policies)). `SHOW OUTBOX` lists the entries in the session's world (in a world, its own and `main`'s as they were at the fork). `CLAIM`, `ACK` and `RETRY OUTBOX` need the database's own user, or an agent with `outbox` or `admin`, not `write_main`: an agent that may merge can't take back the effects its merges queued. A merge policy doesn't stop them.
- **Partial merges:** `ONLY TABLES` and `ONLY KEYS` leave a world's effects in it until it merges whole.
- **In the review:** what a world's merge would send is shown beside its rows: `DIFF WORLD` gives a NOTICE (`fix queues 2 effects, sent when it merges into main: billing (1), email (1)`), `SHOW REVIEWS` an `effects` column, and the Studio's Changes view lists each effect, so approving the merge is approving what it sends. `MERGE ... DRY RUN` shows them as rows of `_sonos_outbox`.
- **Where an effect came from:** `SHOW OUTBOX` (and `CLAIM OUTBOX`) in `main` add `merged_from`, the world whose merge brought it in, and `approved`: `on merge policy p's rules` (an agent merging within its policy), `approved by a person after review (merge policy p)`, `merged by a person`, or `merged by agent x, which keeps to no merge policy`.
- **What a merge sent:** `SHOW MERGE world EFFECTS` (a name or a world id) lists what that world's merges brought into `main`: `id`, `channel`, `world`, `world_id`, `landed`, `merged_by` (NULL: a person), `approved`, and `state`: `queued`, `claimed`, `ambiguous`, or `done` once acknowledged, with `attempts` and `worker`. Whoever approved a merge looks there to see whether what it sent went out.
- **Ambiguous is said:** when an entry turns ambiguous, a NOTIFY on `chronos_outbox_ambiguous` says so, once per attempt, with a JSON payload: `id`, `channel`, `attempt`, `worker`, `world` (where it came from) and `approved`. `LISTEN chronos_outbox_ambiguous` in whatever pages a person. It's noticed by the background worker (every 30 seconds) and whenever `SHOW OUTBOX`, `CLAIM OUTBOX` or `SHOW MERGE ... EFFECTS` look at the outbox; after a restart one may be said again.
- **Discarded, never sent:** dropping a world that holds effects (`DROP WORLD`, rejecting a held merge, the idle sweeper) sends none of them, and records each: `SHOW OUTBOX DISCARDED [SINCE 'time' | SINCE '-1 day']` lists `id`, `channel`, `payload` (NULL unless kept, below), `payload_sha256`, `world`, `world_id`, `queued_by`, `queued`, `discarded`, `discarded_by` and `why` (`rejected: dropped by a person while its merge waited for review (merge policy p)`, `dropped by agent x`, `swept: its time ran out, or it went unused`). So "the agent went quiet" and "the agent's email was vetoed" can be told apart. `SHOW MERGE OUTCOMES` counts them per agent (`effects_discarded`). A rolled-back transaction's effects aren't recorded: nothing proposed them for review.
- **Payloads:** a payload can hold personal data, so a discarded effect keeps only its SHA-256 unless `ALTER SYSTEM SET outbox_keep_payloads = on` (`show outbox_keep_payloads`); effects in `main` keep theirs until acknowledged, as before.
- **Kept how long:** where effects came from, and those discarded, are kept beside the log (the folder's `outbox.ledger`, sealed with the folder) for the history window (`history_retention`), at most 100,000 of each; an in-memory database keeps them in memory.
- **Limits:** channels and payloads as for `NOTIFY` (a channel under 64 bytes, a payload under 8000). An effect is delivered once per landing: `UNDO MERGE` doesn't take back a notification already heard.

## TRUNCATE

`TRUNCATE [TABLE] a, b [RESTART IDENTITY | CONTINUE IDENTITY] [CASCADE | RESTRICT]` deletes every row of the tables in one write. A table other tables' foreign keys reference must be truncated with them, or with `CASCADE` (which truncates them too); else 0A000. `RESTART IDENTITY` restarts the tables' `serial` counters, which every world shares.

## Inheritance and partitioning

```sql
create table city (id int primary key, name text, pop int);
create table capital (country text) inherits (city);
select * from city;          -- the rows of both
select * from only city;     -- city's own

create table ev (id int, at date, v text) partition by range (at);
create table ev_24 partition of ev for values from ('2024-01-01') to ('2025-01-01');
create table ev_rest partition of ev default;
```

- **Inheritance:** `INHERITS (a, b)` merges columns of the same name and type; checks and `NOT NULL` are inherited; `ALTER TABLE ... INHERIT parent` / `NO INHERIT parent` link and unlink an existing table; `ADD COLUMN`, `DROP COLUMN` and constraints on the parent reach the children. A read, `UPDATE` or `DELETE` of the parent covers the children unless it says `ONLY`. Unique and primary keys are not inherited.
- **Partitioning:** `PARTITION BY RANGE | LIST | HASH (column or expression, ...)`. A partitioned table holds no rows; they live in its partitions, which are tables of their own (and can be partitioned in turn). Bounds: `FROM (..) TO (..)` with `MINVALUE`/`MAXVALUE`, `IN (..)`, `WITH (MODULUS m, REMAINDER r)`, `DEFAULT`.
- **Rows:** `INSERT`, `INSERT ... SELECT`, `COPY FROM` and `ON CONFLICT` route to the partition; no match is 23514. An `UPDATE` that changes the key moves the row; `RETURNING tableoid::regclass` names where it is now. `COPY parent TO` is 0A000 as in Postgres (use `COPY (select ...)`).
- **Attach and detach:** `ALTER TABLE p ATTACH PARTITION t FOR VALUES ...` checks the existing rows (23514) and that columns, checks and keys fit; `DETACH PARTITION t [CONCURRENTLY | FINALIZE]` leaves a plain table. Dropping a partition leaves the others; dropping the parent needs `CASCADE` while partitions exist.
- **Keys and indexes:** a primary key or unique constraint on the parent must include every partition-key column (0A000 otherwise). An index on the parent makes one on each partition, including later ones. Foreign keys may come from and point to a partitioned table.
- **Catalog:** see [The catalog](#the-catalog-information_schema-and-pg_catalog): `pg_partitioned_table`, `pg_inherits`, `relispartition`, `relpartbound`, `pg_get_partkeydef`, `pg_partition_tree`.
- **Not done:** see [Known gaps](postgres-compatibility.md#known-gaps).

## The catalog (information_schema and pg_catalog)

Tools and ORMs introspect a database by querying its catalog. These tables answer from the world's schemas, tables, views, sequences, constraints and indexes, and join, filter and sort like any table:

- `information_schema.schemata`, `.tables`, `.columns`, `.views`, `.sequences`, `.routines` (the world's functions and procedures), `.table_constraints`, `.key_column_usage`, `.referential_constraints`, `.constraint_column_usage` and `.check_constraints`.
- `pg_catalog.pg_namespace`, `pg_class` (indexes too), `pg_attribute`, `pg_type`, `pg_attrdef`, `pg_index`, `pg_constraint`, `pg_indexes`, `pg_am`, `pg_database` (a row per world), `pg_inherits` and `pg_partitioned_table` (the inheritance and partition links), `pg_opclass` (empty), `pg_stat_activity` (the open connections) and `pg_stat_database` (each world's connection count), `pg_stat_user_tables` and `pg_stat_all_tables` (each table, counters 0), the other statistics views (see [Postgres compatibility](postgres-compatibility.md)), `pg_prepared_statements`, `pg_cursors` and `pg_locks` (the session's prepared statements and cursors, and advisory locks), `pg_description` (the comments made with `COMMENT ON`), `pg_sequence`, `pg_tables`, `pg_views`, `pg_matviews` and `pg_sequences` (with or without `pg_catalog.`; columns named as in Postgres, the ones introspection reads).
- **Constraints and indexes** come from the real ones: primary keys (of one or several columns), `UNIQUE` constraints, foreign keys (several columns too, with `confkey`, `confrelid` and `confdeltype`), `CHECK`s, and indexes (`CREATE [UNIQUE] INDEX`, on expressions, partial, gin, hnsw). `pg_constraint.conindid` is the index behind a key (a foreign key's: its parent's primary key), `pg_index.indkey`, `indclass` and `indoption` are an `int2vector` (`indclass`, `indcollation` an `oidvector`), written as Postgres writes them (`1 2`; an array function reads that as the array it is, counting from 0 as Postgres does, and `indkey::int2[]` is the array, `[0:1]={1,2}`), and are those types to a driver (type ids 22 and 30; in binary an int2 array whose lower bound is 0), and `pg_get_constraintdef(oid[, pretty])` and `pg_get_indexdef(oid[, column, pretty])` write them as Postgres does (`CHECK ((qty > 0))`, `FOREIGN KEY (org) REFERENCES orgs(id) ON DELETE CASCADE`, `CREATE UNIQUE INDEX t_pkey ON public.t USING btree (id)`).
- **`regclass` and the other reg types:** `'orders'::regclass` (or `'app.orders'`, or an oid) is the relation's oid, shown by name (schema-qualified when the search_path doesn't show it); `::regclass::oid` is its `pg_class.oid`, `oid = 'orders'::regclass` compares oids, and `to_regclass('x')` is null for a relation that doesn't exist (the cast is 42P01). `regtype`, `regnamespace`, `regrole`, `regproc`, `regprocedure`, `regoper`, `regoperator`, `regconfig`, `regdictionary` and `regcollation` are oids of their kind of object the same way, and `pg_typeof` gives a regtype (see [Postgres compatibility](postgres-compatibility.md)).
- Functions: `current_schema()`, `current_schemas(bool)`, `current_database()`, `pg_table_is_visible(oid)` (along the search_path), `format_type(type oid, typmod)`, `pg_get_expr(expr, relid)`, `pg_get_userbyid`, `obj_description` and `col_description` (the object's comment), `pg_catalog.`-qualified or not.
- **Array helpers the introspection queries use:** `unnest(a)` and `generate_subscripts(a, 1)` in the select list (a row per element, side by side, as Postgres runs them; not with `GROUP BY`), `array_length`, `cardinality`, `array_lower`, `array_upper`, and bitwise `&` and `|` on integers.
- **The world's own objects** are rows too, with every column Postgres has for them (`src/sql/catalog_objects.rs`): `pg_proc` (its functions and procedures: `prokind`, `provolatile`, `proisstrict`, `proretset`, `proargtypes`, `proargnames`, `prosrc`; and Postgres 16's 3,286 built-in functions, in `pg_catalog`), `pg_class.reltype` (each table's, view's and materialized view's row type, a row of `pg_type` with its array type), `pg_attribute`'s system columns and its rows for sequences and indexes, `pg_type` and `pg_enum` (enum types and their array types, with the oids `atttypid` and `regtype` give), `pg_trigger` (`tgtype` bits, `tgfoid`; not the internal triggers foreign keys are made of in Postgres, though a table with foreign keys has `relhastriggers`), `pg_rewrite` (a view's `_RETURN` rule), `pg_collation` (the built-in collations and those made with CREATE COLLATION; `pg_attribute.attcollation`, `pg_index.indcollation` and `information_schema.columns.collation_name` name them, `regcollation` and `pg_collation_actual_version()` read them), `pg_attrdef`, and `pg_depend` (a serial's or identity's sequence and its column, a default and its sequence, a view and the tables it reads, a trigger and its table and function, a column and its enum type). `pg_attribute` and `format_type(oid, typmod)` know `character varying(20)`, `character(3)`, `numeric(10,2)` and enums. Functions: `pg_get_functiondef`, `pg_get_function_arguments`, `pg_get_function_identity_arguments`, `pg_get_function_result` (of the built-in functions too), `pg_get_triggerdef`, `pg_get_viewdef` (the query as `CREATE VIEW` was given it, not Postgres's rewritten form), `pg_get_function_sqlbody` (null), `pg_options_to_table`.
- **Every column Postgres has:** each `pg_catalog` table and view of Postgres 16 exists with its columns in its order (`src/sql/catalog_columns.txt`, made from a real Postgres 16): the columns Chronos works out are real, the others hold what a plain object has, and a table Chronos has nothing for is empty (`pg_operator`, `pg_ts_*`, `pg_foreign_*`, `pg_event_trigger`, `pg_publication`, ...: built-in operators and text-search objects aren't listed). A `tableoid` column comes last, as pg_dump reads it. `pg_roles`, `pg_authid`, `pg_user`, `pg_shadow` and `pg_auth_members` list the roles ([Roles and privileges](#roles-and-privileges)); `pg_settings` and `SHOW ALL` all 363 of Postgres 16's settings. `regnamespace`, `regrole`, `regtype`, `regproc` and the other reg types read a string as its oid and show an oid as its name (`23::regtype` is `integer`, `'public'::regnamespace::int` is 2200).
- Oids: a relation's, an index's, a constraint's, a schema's (`public` is 2200), an enum type's or domain's, a collation's and a row type's is worked out from its kind and name (FNV-1a of the name, in a range of 2^26 per kind from 2^26 up, the next free one where two names meet), so it is the same in every world and after other objects are made or dropped, and changes with a rename; functions made here have oids of their own from 2^30 up; built-in types, functions and catalog tables have Postgres's oids.
- An enum column's `data_type` is `USER-DEFINED` and its `udt_schema` and `udt_name` the enum's, as in Postgres. A primary key of several columns makes each of its columns `NOT NULL` here. Temporary tables aren't listed.
- **Checked against Postgres 17** (tests/pgdiff.rs, tests/schemas.rs): listing schemas, tables, views and sequences; columns with their types, nullability and defaults; constraints and indexes through each table and view above; and the reflection queries SQLAlchemy 2.0 sends (tables, primary keys, unique constraints, foreign keys, checks, indexes with expressions and `WHERE`, columns) and Prisma's schema describer (namespaces, tables, constraints, columns, foreign keys, indexes, sequences, views), written into the tests. The ORMs themselves weren't run against it.
- **Not yet:** `NOT NULL` constraints as rows of `table_constraints` (Postgres 17 lists them), operator classes in `pg_opclass`, the built-in operators in `pg_operator`, the catalog's own tables in `pg_class` and `pg_attribute`, and `pg_relation_size`.
## Storage

Tables made with `CREATE TABLE` store rows packed: each column's value in schema order, length-prefixed, with no names to match and no numbers to parse, about half the size of JSON. Everything outside SQL still sees JSON objects: `get`, `scan`, `find`, `diff`, merge conflicts, the HTTP API, the shell and the search index. Rows written as JSON through those (`put`) are read by SQL too. Tables made by writing JSON rows stay JSON.

## Subqueries, UNION and WITH

```sql
select name from customers c
where exists (select 1 from orders o where o.customer = c.id and o.total > 50);

select c.name, (select count(*) from orders o where o.customer = c.id) from customers c;

with paid as (select * from orders where status = 'paid')
select customer, sum(total) from paid group by customer
union all
select null, sum(total) from paid;
```

- **Subqueries:** as a value (`(select max(x) from t)`), in `IN (select ...)`, and in `EXISTS`, anywhere an expression goes, including `UPDATE` and `DELETE`.
  - **Correlated subqueries** (ones that read the outer row) run once per distinct set of outer values they read. A correlated `EXISTS`, `NOT EXISTS` or `IN` whose conditions compare its own columns with the outer row by `=` runs once, not once per outer row (see [Speed](#speed)).
  - **Parameters inside them** get their types from the subquery's own tables.
- **Set operations:** `UNION`, `INTERSECT` and `EXCEPT`, each with or without `ALL`, parts in parentheses allowed. `ORDER BY` / `LIMIT` apply to the whole and name output columns. Result types widen as in Postgres (`1` and `2.5` give double precision).
- **Subqueries in FROM:** `(select ...) as t` or `as t(a, b)`, joined, filtered and grouped like tables.
- **`WITH`:** `WITH name [(columns)] AS (select ...)` names queries for the main query, later `WITH` queries, and subqueries. A statement runs each once, however many times it's used, and every use reads the same rows (as Postgres does for a query used more than once: `nextval()` or `random()` in it gives one value to all its uses). A subquery run for each outer row, correlated or `LATERAL`, runs its own `WITH` queries each time.
- **`WITH RECURSIVE`:** `name AS (base UNION [ALL] step)`, for trees and graphs. The step reads the rows the last round added, and rounds stop when a round adds nothing new. `UNION` drops rows already seen, which ends cycles. A query that never stops is refused after 10,000 rounds or 1,000,000 rows. Each round joins from the rows the last round added, looking the next ones up by index (with an index on the edge column, a small traversal of a 200,000-edge graph takes about 1 ms).
- **`ANY`, `SOME` and `ALL`:** `x op ANY (select ...)`, `ALL (...)` with any comparison, over a subquery, `ARRAY[1, 2]`, a Postgres array literal `'{1,2}'`, or a JSON array. `= ANY` and `<> ALL` are `IN` and `NOT IN` and share their index lookups. NULLs make the answer unknown, as in Postgres, and over nothing `ANY` is false and `ALL` true.
  - **From drivers:** `where id = any($1)` takes a list however the driver sends it: a typed array, text or binary (`int8[]`, `text[]`: psycopg 3, asyncpg, pgx, Rust's `postgres`), array text (`'{1,2,3}'`, node-postgres), or JSON (over HTTP). Chronos tells drivers that `$1` is an array of the column's type.
- **`WITH` before `INSERT`, `UPDATE` and `DELETE`:** the names reach the statement's query and its subqueries.
- **Data-changing `WITH` queries:** `WITH x AS (INSERT | UPDATE | DELETE ... RETURNING ...) SELECT | INSERT | UPDATE | DELETE ...` (what PostgREST and Hasura write for every mutation, and the archive-a-row pattern `WITH moved AS (DELETE FROM a RETURNING *) INSERT INTO b SELECT * FROM moved`). The statements run once each, first to last, before the one that reads their `RETURNING` rows, which a statement without `RETURNING` (or a name nothing reads) simply doesn't give; all of it takes effect or none of it does (in a transaction, with it). They must be at the top: in a subquery it is 0A000, as in Postgres. Postgres shows every statement of the `WITH` the tables as they were before any of them changed anything; here a statement sees what those before it wrote, so one that *reads* a table an earlier one changes (its `SELECT`s, or the rows an `UPDATE` or `DELETE` looks through, by name or through a view) is refused (0A000) rather than answer differently: read the rows through the `WITH` query's `RETURNING` instead. `UPDATE ... FROM` or `DELETE ... USING` with `RETURNING` in a `WITH` query, `EXPLAIN`, `PREPARE` inside a function's body, and a temporary table as a target are 0A000. An agent needs the rights to write for one, as for any write.
- **`UPDATE ... FROM` and `DELETE ... USING`:** other tables (or views) to match against, as in Postgres. With several matching rows, `UPDATE` takes the first one's values.
- **`MERGE INTO`:** `MERGE INTO target [AS t] USING source [AS s] ON condition WHEN [NOT] MATCHED [AND condition] THEN UPDATE SET ... | DELETE | INSERT [(columns)] VALUES (...) | DO NOTHING, ...` as in Postgres 16, told apart from Chronos's `MERGE BRANCH` / `MERGE WORLD` by the `INTO`. The source is a table, a view or a `(subquery)`; each source row is matched to the target rows the `ON` condition finds and runs the first clause that fits (`WHEN MATCHED` for a row found, `WHEN NOT MATCHED` for one that isn't, each with its own `AND` condition). It's one write: every clause takes effect or none does (in a transaction, with it), and it answers `MERGE n`, the rows it changed. A target row that more than one source row would change is 21000 (`MERGE command cannot affect row a second time`), and nothing changes. Parameters (`$1`) work in the source, the conditions and the values, through a driver or `PREPARE`. A target without a primary key has its rows matched by all their columns. A temporary table as the target reads only temporary tables and VALUES in its source. A view as the target (0A000 in Postgres 16 too), `EXPLAIN MERGE`, `INSERT ... OVERRIDING` and `INSERT DEFAULT VALUES` inside a clause are refused (0A000). Postgres 17's `WHEN NOT MATCHED BY SOURCE` and `RETURNING` are a syntax error here, as in 16. An agent needs the rights to write for one, as for any write.
- **Set-returning functions in FROM:** `generate_series(from, to[, step])` (numbers, or timestamps and dates with an interval step), `unnest(array)`, `generate_subscripts(array, 1[, reverse])`, `jsonb_array_elements(_text)`, `jsonb_each(_text)` and `jsonb_object_keys`. They can read the tables before them, as `LATERAL` (`from orders o, jsonb_array_elements(o.items) item`), and a one-column function's name is its column (`select g from generate_series(1, 5) g`).
- **Arrays** are kept as Postgres array text, in values and in array columns: `ARRAY[1, 2]`, `'{a,b}'::text[]`, `array_agg`, for `ANY` and `unnest` (see Arrays above).
- **`LATERAL`:** `from customers c, lateral (select ... where o.customer = c.id order by at desc limit 3) recent`, or `[LEFT] JOIN LATERAL (...) x ON ...`. The subquery reads the tables before it and runs once for each distinct set of values it reads from them.

## Window functions

```sql
select name, dept, pay,
       rank() over (partition by dept order by pay desc),
       sum(pay) over (partition by dept order by hired),                       -- running total
       avg(pay) over (order by hired rows between 6 preceding and current row) -- moving average
from staff;
```

- **Functions:**
  - `row_number`, `rank`, `dense_rank`, `percent_rank`, `cume_dist`, `ntile`.
  - `lag` and `lead` (with offset and default), `first_value`, `last_value`, `nth_value`.
  - `count`, `sum`, `avg`, `min`, `max`, with `OVER (PARTITION BY ... ORDER BY ...)`.
- **Frames:**
  - **Default, as in Postgres:** with `ORDER BY`, from the partition's start to the current row's last tie; without it, the whole partition.
  - **Explicit:** `ROWS BETWEEN n PRECEDING / UNBOUNDED ... AND CURRENT ROW / n FOLLOWING / UNBOUNDED`, and `RANGE` with `UNBOUNDED`, `CURRENT ROW` or an offset in the one `ORDER BY` column's units (`RANGE BETWEEN 5 PRECEDING AND CURRENT ROW`, `interval '7 days' PRECEDING` on dates and timestamps).
- **With `GROUP BY`:** windows run over the groups, e.g. `rank() over (order by sum(x))` or `sum(sum(x)) over ()`.
- **Where they can go:** only in the select list and `ORDER BY` (42P20 elsewhere).
- **Named windows:** `WINDOW w AS (partition by ... order by ...)`, then `OVER w`, or `OVER (w ORDER BY ...)` to add an order, as in Postgres.
- **`FILTER (WHERE ...)`** works on window aggregates too.
- **`GROUPS` frames and `EXCLUDE`:** `GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW` counts groups of peer rows (it needs `ORDER BY`, 42P20 otherwise), and a frame may end `EXCLUDE CURRENT ROW | GROUP | TIES | NO OTHERS`, for aggregates and for `first_value`, `last_value` and `nth_value` (which take the first, last or nth row left). With an exclusion an aggregate is worked out again for each row, not slid along the frame.
- **Not yet:** `DISTINCT` inside a window aggregate.

## Branches in SQL

```sql
create branch agent_7;                 -- instant copy of the current branch (or: from main)
use branch agent_7;                    -- this connection now works on it
update people set age = 37 where id = 1;
diff;                                  -- table, id, change, before, after (jsonb)
merge branch agent_7;                  -- into its parent; you move back to the parent
show branches;                         -- name, parent, changes
drop branch agent_7;                   -- throw it away
```

- **Connecting to a branch:** it's a database, so `psql postgres://127.0.0.1:5433/agent_7` works.
- **Branch names with dashes** need quotes: `create branch "agent-7"`.
- **Merge conflicts:** if both sides changed a row, `MERGE BRANCH` fails with SQLSTATE 40001, lists the rows and merges nothing. Check `DIFF`, then choose:
  - `MERGE BRANCH agent_7 USING OURS` keeps the branch's rows.
  - `USING THEIRS` keeps the parent's rows.
- **After a crash:** a branch that was open needs `MERGE BRANCH agent_7 CONFIRM`, once you've checked its `DIFF`.

### Worlds

The same statements with `WORLD`, plus metadata and IDs (see [concepts](concepts.md#worlds)):

```sql
create world agent_7 with (owner = 'claude', task = 42);   -- from the current world (or: from main)
fork world agent_7 as agent_7b meta '{"tags": ["retry"]}'; -- from a named world
switch world agent_7b;                                     -- or: use world '<its id>'
alter world agent_7 set (task = null, reviewer = 'ada');   -- null removes a key
alter world agent_7 set ttl '2 hours';  -- discarded 2 hours from now (unless it has forks then)
alter world agent_7 reset ttl;          -- never expires (agents need admin for either)
alter world agent_7 set pinned;         -- never discarded for being idle (see world_idle_ttl); reset pinned undoes it
show worlds;                  -- name, id, parent, depth, created, version, meta (jsonb), owner, expires, active, pinned
show storage;                 -- per world: rows changed since its fork, and the pages and bytes only it holds
show storage for world agent_7;
show disk;                    -- bytes of pages in use, kept only for history, reclaimable; and the log's
diff world agent_7b;          -- table, id, change, before, after, columns (what changed)
diff world agent_7b as sql;   -- the same as statements: run them on main to make the change
diff world agent_7b readers;  -- what reads each changed column (views, checks, keys, clients seen reading...), and what that can't see
register reader nightly_export on orders (status, total);   -- a reader outside SQL, named by hand
show readers;                 -- registered readers, and the clients seen reading which columns
diff world a to b;            -- between any two worlds, or moments (a@-1 hour)
merge world agent_7b dry run;  -- what the merge would do, row by row, and why rows conflict (attached: checked against live Postgres too)
merge world agent_7b by columns resolve ('orders/7' = theirs, 'orders/9' = '{"id": 9, "status": "paid"}');
merge world agent_7b only tables (orders);           -- just these; agent_7b stays open with the rest
merge world agent_7b only keys ('orders/7', 'orders/9');
merge world agent_7b into agent_7;  -- into another world, three-way against agent_7b's fork point; it stays open
merge world agent_7b;
drop world agent_7;           -- or: delete world agent_7; refused while it has forks
drop world agent_7 cascade;   -- it and every world forked from it, deepest first
```

- **Partial merges:** rows merged with `ONLY` aren't merged again, but a later change to one by either side conflicts. A row whose unique value or reference is tied to a row left out, or whose table the world changed, is refused (SQLSTATE 22023). See [concepts](concepts.md#merging-part-of-a-world-or-into-another-world).
- **`INTO`:** any live world but the one merged. Into its own parent, it's the ordinary merge.
- **Merge checks:** queries a merge's result must find nothing in, run inside every merge (SQLSTATE 23514 when one finds rows, with the rows). See [merge checks](reference/worlds.md#merge-checks) and the [guide](guides/merge-checks.md).

```sql
create merge check lists_keep_leads on tables (leads, lists) with (timeout = '2s') as
  select id from lists where id not in (select list from leads);
show merge checks;            -- name, tables, timeout_ms, query, created
drop merge check lists_keep_leads;
```

### Databases

A database is a world, so the statements tools send to make one work:

```sql
create database app;                          -- an empty world with no parent: nothing of main's
create database app_test template app;        -- a fork of the world `app`: instant at any size
create database "test_x" encoding 'UTF8' lc_collate 'C' owner = chronos;
drop database app_test;                       -- or: drop database if exists app_test (force)
```

- Connect to one by name (`dbname=app`), as to any world. `postgres` and `template1`, where Rails, Django and Prisma connect to make the others, are `main` unless a world is named so; `template0` and `template1` as a `TEMPLATE` mean an empty database.
- A database made with `TEMPLATE` is a fork of its template, so the template can't be dropped while it exists (2BP01; `DROP WORLD ... CASCADE` drops both). A database made without one has no parent: it can't be merged (0A000), and idle expiry leaves it alone.
- Checked as Postgres checks them: 42P04 for a name in use (`postgres`, `template0`, `template1` and `main` too), 3D000 for a missing template or database, 42601 for an option it doesn't know or one given twice, 22023 for a bad strategy, 42704 for an unknown encoding or tablespace, 55006 for dropping the database the session is in, 42809 for `template0`, and 25001 inside a transaction block. An agent needs admin rights (42501).
- `OWNER`, `ENCODING 'UTF8'` (any spelling), `STRATEGY`, `TABLESPACE pg_default`, `LOCALE_PROVIDER` and `OID` are accepted and change nothing. Options that would change what a query returns or who may connect are accepted with a warning that they aren't applied: a `LOCALE`/`LC_COLLATE` other than `C` (text sorts by bytes), `CONNECTION LIMIT n`, `ALLOW_CONNECTIONS false`, `IS_TEMPLATE true`. Another encoding is 0A000. `DROP DATABASE ... (FORCE)` is accepted but doesn't cascade to worlds forked from it. `ALTER DATABASE` isn't parsed.
- `pg_database` lists them; `current_database()` is the world's name.

## Time travel

Any table can be read as it was at a moment in the retention window (1 hour for a new database unless you change it):

```sql
select * from orders as of '2026-09-20 10:00';            -- UTC unless the time says otherwise
select * from orders for system_time as of $1;            -- the SQL:2011 spelling, with a parameter
select * from orders as of '-15 minutes';                 -- a time ago

-- now against then, in one query
select now.id, now.status, old.status
from orders now join orders as of '-1 hour' old on old.id = now.id
where old.status <> now.status;

insert into orders select * from orders as of '-1 hour' where id = 42;  -- bring a row back
restore world main to '-1 hour';        -- put a whole world back (an ordinary write: history keeps what it replaced)
fork world 'main@-1 hour' as check_it;  -- a world holding main as it was: diff it, or merge it to restore
diff world 'main@-1 hour' to main;      -- what changed since
undo merge of world agent_7;           -- put back every row its latest merge changed
undo merge of world agent_7 skip changed;  -- ...except rows changed again since (else they're named)
undo agent bot since '-2 hours';        -- put back every row agent bot changed in this world since then
undo agent bot since '-2 hours' skip changed;  -- ...except rows someone changed after it (else they're named)
show history limit 20;                  -- at, world, event (write, commit, forked ..., merged ...), rows
alter system set history_retention = '7 days';  -- or '0' to keep none (a new database keeps 1 hour)
show history_retention;
alter system set synchronous_commit = normal;  -- full (default), normal or off
show synchronous_commit;
```

- **A table AS OF** comes with its schema as it was then, and tables dropped since can still be read.
- **A whole session in the past:** `use world 'main@2026-09-20 10:00'`, or connect to the database `main@2026-09-20 10:00` with psql. It's read only (SQLSTATE 25006 on writes).
- **Times:** a fixed time is rebuilt from history once, then cached; a time ago (`-15 minutes`) is a new moment each time it's run. See [concepts](concepts.md#time-travel).

## Simulations

```sql
simulate 1000 worlds from main as trial
  run $$update prices set p = p * (0.9 + random() * 0.2) where sku % 100 = $1 % 100$$
  score $$select sum(p * sold) from prices$$
  keep 10 seed 42;                         -- world, id, index, seed, score, error, kept: best first
replay world trial_17;                     -- world, replay, identical, rows_differing, score, recorded_score, error
replay world trial_17 as trial_17_again;   -- ...and keep the replay
select setseed(0.42); select random();     -- a repeatable random() for this session
```

- **What it does:** forks n worlds (`trial_0`, `trial_1`, ...) from one moment of the base, runs the script in each on every core, then the score query in each, keeps the best `KEEP k` and discards the rest.
  - **Base:** `FROM world`, or the session's world; `FROM 'main@-1 hour'` starts from the past.
  - **Best:** the highest score first; `ASC` for the lowest. `KEEP ALL`, the default, keeps every world.
  - **Parameters:** in the script and the score query, `$1` is the world's index and `$2` its seed.
  - **Strings:** the script and score can be dollar-quoted, `$$...$$` or `$tag$...$tag$`, as in Postgres.
- **Failures:** a world whose script or score fails is reported with its error and SQLSTATE, and the rest carry on. It's discarded, unless `KEEP ALL`. The score query must be one SELECT whose first value is a number.
- **The same every time:** in each world, `random()`, `gen_random_uuid()` and the keys of tables without one follow `SEED` (0 unless given) and the world's index, and `now()` (with `current_timestamp`, `current_date`, ...) is the base moment. So the same statement from the same moment gives the same worlds and scores on any machine, with any number of threads (`THREADS n`; every free core by default).
  - A seeded statement runs on one core; the worlds run side by side.
  - Not covered: `serial` numbers, which come from the database's counters that every world shares (so merged worlds never collide), and so differ between runs. Use `$1` or `gen_random_uuid()` for keys.
- **What a script can't do:** fork, merge, restore, switch or drop worlds, or change settings (0A000). Transactions are fine.
- **Replay:** each kept world holds its inputs in its metadata, `sim`: the seed, index, script, score query, score, and the base world and exact moment.
  - `REPLAY WORLD w` forks the base as it was then (time travel), runs the script and score again, and says whether the rows and the score are identical.
  - The replay is thrown away, unless `AS name` keeps it.
  - It needs history back to that moment: not an in-memory database, nor past the retention window.
- **Agents:** the worlds are the agent's own and count toward its `max_worlds`. They run in batches that fit, keeping the best so far; a `KEEP` that can't fit is refused before anything runs (42501). Each world's statements are checked as the agent's, and its row quota applies in each world.
- **Size:** worlds are forked 1,024 at a time, and the losers go after each batch, so about `KEEP` + 1,024 worlds are alive at once. 10,000 worlds that each insert a row take 0.4 s; 10,000 that each update 1,000 rows take about 30 s (8-core laptop busy with other work, release build).
- **`setseed(x)`** (x from -1 to 1) seeds the session's `random()` and `gen_random_uuid()`, as in Postgres, until the session ends or it's called again.

## Metrics

`SHOW METRICS` lists each kind of operation since the database opened, with its count and latency in milliseconds (`total_ms`, `p50_ms`, `p95_ms`, `p99_ms`, `max_ms`), then counters (`rows_written`, `errors`, `conflicts`, `worlds`, bytes on disk) with only a count. See docs/operations.md.


## Backups

```sql
backup database to '/backups/db-1';   -- files, bytes: a copy of the running database in a new folder
verify database;                      -- pages, bytes, log_records: every page and log record read and checked
```

See docs/operations.md for restoring (`chronos restore`).
## Agents

```sql
create agent bot with (can = 'read,fork,write_own', max_worlds = 10, world_ttl = '1 day');  -- returns its token, once
alter agent bot set (writes_per_minute = 600, disabled = false);
alter agent bot set (max_query_ms = 30000, max_concurrent = 4, max_memory_mb = 512);
show agents;                              -- name, id, can, quotas, world_ttl, created, disabled, limits
show audit for agent bot limit 20;        -- at, agent, world, action, rows
show merge outcomes;                      -- how each agent's worlds ended, and how often its merges were held or refused
show merge outcomes by policy;            -- the same, a row per merge policy
checkpoint world bot_task as 'before';    -- name a moment of a world...
restore world bot_task to checkpoint 'before';  -- ...and go back to it
drop agent bot;                           -- its token stops working; its worlds stay
```

See [concepts](concepts.md#agents) for what agents may do. The names `guest` and `system` are the database's own.

`SHOW MERGE OUTCOMES` reads the history window (`show history_retention`; `since` is where it starts) and counts each
world once, by how it ended:

| column | a world that ... |
|---|---|
| `on_rules` | its owner merged, keeping to a merge policy, with no hold |
| `no_policy` | its owner merged, keeping to no merge policy (the database's own users, or an agent without one) |
| `approved` | someone other than its owner merged: a person, after a hold or without one |
| `rejected` | was dropped after a hold |
| `discarded` | was dropped with no hold |
| `open` | is still open, nothing merged |
| `partly_merged` | is still open, some of it merged (`ONLY TABLES`, `ONLY KEYS`, or `INTO` another world) |
| `before_window` | was forked before the window: counted, not classed |

and counts attempts: `conflicts` (rows both sides changed), `stale_reads` (rows it relied on changed since it read
them), `check_refusals` (a merge check or a CHECK constraint the result breaks) and `holds` (a merge policy sent it to a
person), and `effects_discarded`: effects (`NOTIFY ON MERGE`) dropped with their worlds, never sent (see `SHOW OUTBOX
DISCARDED`). A merge retried in a loop adds attempts, never worlds. `rules_rate` is `on_rules / (on_rules + approved +
rejected)`: the share of decided worlds that merged on their rules alone (NULL when none were). `agent` is NULL for the
database's own users; `policy` is each agent's policy now. A transaction's COMMIT that conflicts isn't counted (it's no
agent's merge).

Its limits: it replays the whole retained log, so it costs what `SHOW AUDIT` costs: 0.47 s over 1,022,531 log records
(340,000 worlds) on the 16-core test VM, 2026-10-16. A refused merge logs its world's metadata once (about 65 bytes
for a world with only the counts), and refusals don't count toward an agent's `writes_per_minute`, so an agent
retrying a refused merge in a loop grows the log by that much each time. A merge undone later with `UNDO
MERGE` still counts as merged (the undo is logged as an ordinary write). After a crash, refusals since the last sync to
disk may be missing; merges never are. An in-memory database keeps no log: no rows, and a notice says so. Only the
database's own users may run it: an agent's session gets 42501.

## Roles and privileges

Postgres's roles and grants, beside agents:

```sql
create role app login;                          -- a role a program logs in as
create role readers;                            -- a group
grant readers to app;                           -- app has readers' privileges (INHERIT), may SET ROLE to it
grant usage on schema s to readers;
grant select on s.orders to readers;
grant insert (note), update (note) on s.orders to app;
revoke execute on function s.f() from public;
alter default privileges for role app in schema s
  grant select on tables to readers;            -- what app makes in s from now on, readers may read
alter table s.orders owner to app;
set role app;                                   -- current_user is app until RESET ROLE
select has_table_privilege('s.orders', 'insert'), pg_has_role('readers', 'member');
```

- **What's checked:** reading and writing rows (by column too), sequences, function calls, naming things in a schema and making things in it, and owning what's dropped or altered, as Postgres checks them; a view reads its tables with its owner's privileges, and a `SECURITY DEFINER` function runs as its owner. See [Postgres compatibility](postgres-compatibility.md#roles-and-privileges) for the whole list and what isn't checked yet.
- **Who is a superuser:** a role made `SUPERUSER`, and every login whose name is no role: every login was a superuser before roles, and still is, so a database that never makes a role works as before. Objects such a login makes are owned by `chronos`, the bootstrap superuser.
- **Roles are the database's, grants the world's:** every world sees the same roles; a world's tables carry their owners and grants through forks and merges, and default privileges (`ALTER DEFAULT PRIVILEGES`) fork and merge with the world too. Role statements take effect at once, even inside a transaction.
- **The past:** in a world as it was (`main@<moment>`), a role that isn't a superuser needs the privileges a relation had then and those it has now (a `REVOKE` holds for the past too); a relation with no relation of its name now (renamed, moved or dropped since) has no privileges now to check, so only a superuser reads it there.
- **Agents** keep their capabilities; an agent with a role of its name has that role's privileges too, and needs the `admin` capability to make or grant roles. Roles that aren't superusers can't touch worlds (fork, merge, restore, `DIFF`): give a program an agent for that.
- **Passwords** are kept as SCRAM-SHA-256 verifiers, never in the clear (`PASSWORD 'x'`), with `scram_iterations` iterations (4096 unless set) of the password after SASLprep, as Postgres keeps them, and a role with one logs in with it: SCRAM-SHA-256, or MD5 for one kept as an md5 hash (`SET password_encryption = 'md5'`). An agent's token has a verifier too, made by `CREATE AGENT` with the session's `scram_iterations`.

## Row-level security

Postgres's policies, on the same roles: a table with row security shows each role only the rows a policy lets it see, and takes only the rows a policy lets it write.

```sql
alter table notes enable row level security;
create policy tenant on notes using (tenant = current_user);           -- every command, every role
create policy org on orders using (org = current_setting('app.org_id')::int);
create policy read_shared on notes for select to readers using (shared);
create policy no_drafts on notes as restrictive for update using (not draft) with check (not draft);
set app.org_id = '42';                                                  -- per session (or SET LOCAL per transaction)
```

- **Who they apply to:** every role but a superuser, a role with `BYPASSRLS`, and the table's owner (and members of the owner role), unless `ALTER TABLE ... FORCE ROW LEVEL SECURITY`. With no policy that applies, a role sees no rows and writes none.
- **How they combine:** a command's permissive policies (the default) are ORed, its restrictive ones ANDed with them, and one permissive policy at least must allow a row. `USING` decides the rows `SELECT`, `UPDATE` and `DELETE` see (the others are passed by, not refused); `WITH CHECK` (or `USING`, for a policy without one) the rows `INSERT` and `UPDATE` write, which are refused with 42501 (`new row violates row-level security policy for table "t"`). Where a statement reads the table (a `WHERE`, `RETURNING`, `ON CONFLICT DO UPDATE`), its rows must also pass the `SELECT` policies, as in Postgres 16.
- **Everywhere rows are read:** joins, subqueries wherever they are, `WITH` (`RECURSIVE` too), views (as the view's owner, whose policies they are, while `current_user` stays the one asking), functions in `FROM` and their arguments, PL/pgSQL's expressions and `EXECUTE`, `COPY ... TO`, `INSERT ... ON CONFLICT`, `UPDATE ... FROM`, `DELETE ... USING` and `MERGE`; `REFRESH MATERIALIZED VIEW` reads as the view's owner. A policy's own subqueries are read with the privileges and policies of the role reading, so a policy that reads another table's policy that reads it back is 42P17. `COPY ... FROM` into a table whose policies apply is 0A000 (use `INSERT`). Foreign keys, unique keys and `TRUNCATE` see every row.
- **`SET row_security = off`** makes a statement the policies would limit fail (42501) instead, which is what pg_dump sets; a role that bypasses them is unaffected.
- **Partitions and inheritance:** a parent's policies apply when the parent is read, a partition's or child's own when it is read directly.
- **Worlds:** policies and a table's row security are rows of the world, as its schema is: a fork has its parent's, `MERGE WORLD` brings a world's (two worlds adding policies to one table merge without a conflict), `DIFF` shows them (`AS SQL` too), and `RESTORE` puts them back as they were. A read `AS OF` a moment gets the policies the table had then, and those its name has now.
- **What a policy reads is bound when it's made:** a table or function its maker's `search_path` finds outside public is written with its schema in the policy, and its other names are public's (a built-in function's name, the built-in), whatever the `search_path` of whoever reads; the tables it reads are the world's, never a temporary table of the name. Renaming a column it reads (in a subquery, or from another table's policy too), or a table or view it reads, renames it in the policy (through a subquery's `*` or a `NATURAL` or `USING` join it can't: drop the policy first); dropping a column, table or view a policy reads needs `CASCADE`, which drops the policy.
- **Cost:** a policy is parsed once, and a prepared statement is rewritten and planned once, again only when a policy, a table's row security, a view or a role it depends on changes, or another role runs it: a point read of a table with a tenant policy costs about what a plain one does. A table with policies joined to others keeps its index reads where its policies can go first (the join's conditions on it compare columns, parameters and constants).
- **The row API:** the HTTP and MCP `get`, `find`, `put`, `delete`, `batch`, `diff` and `merge` work on rows as they are, so an agent whose role (a role of its name) a policy limits on a table is refused them there (403, 42501): it reads and writes those rows with SQL.
- **The catalog:** `pg_class.relrowsecurity` and `relforcerowsecurity`, `pg_tables.rowsecurity`, `pg_policy` and `pg_policies`; psql's `\d` shows a table's policies and `\dp` their column, and `pg_dump` writes them, which restore.

## Limits for the whole database

```sql
alter system set statement_timeout = '30s';   -- every session starts with it (0: none, the default)
alter system set max_worlds = 10000;          -- live worlds besides main (0: no limit, the default)
alter system set world_idle_ttl = '7 days';   -- unused worlds are discarded (0: never, the default)
alter system set protect_main = on;           -- main's rows change only by merging a world (off by default)
alter system set outbox_keep_payloads = on;   -- discarded effects keep their payloads, not only a hash (off by default)
show max_worlds;
show world_idle_ttl;
show protect_main;
```

They're kept in the folder across restarts, as `synchronous_commit` is. Past `max_worlds`, a fork is refused with SQLSTATE 53400. See [operations](operations.md#limits-and-cleanup).

**`protect_main`:** with it on, nobody changes `main`'s rows directly, the database's own user and admins included: an `INSERT`, `UPDATE`, `DELETE`, `COPY`, `TRUNCATE` or HTTP `put` there, or a transaction's `COMMIT` that changes rows of `main`, is refused with 42501, naming a row. Rows reach `main` by merging a world, so a mistaken `DELETE` is a diff someone sees (and a merge policy can hold) before it lands. Still direct: definitions that change no rows (`CREATE TABLE`, `CREATE INDEX`, `ALTER TABLE ... ADD COLUMN` without a default to fill), the database's own rows (agents, merge policies and checks, the outbox), and the ways back (`RESTORE WORLD`, `UNDO MERGE`, `UNDO AGENT`). Turning it off is `ALTER SYSTEM SET protect_main = off`, which needs the database's own user or `admin`.

## Transactions

`BEGIN`, then statements, then `COMMIT` or `ROLLBACK`. Drivers' transaction APIs work as they do with Postgres.

- **How it works:** a transaction is a hidden branch.
- **Snapshot:** at `READ COMMITTED` (the default, as in Postgres) each statement reads what was committed before it began, plus the transaction's own writes; at `REPEATABLE READ` the transaction reads the database as it was at `BEGIN`, plus its own writes.
- **Invisible until COMMIT:** nobody else sees its writes before `COMMIT`.
- **COMMIT is durable,** like a merge.
- **Writes wait for each other, as in Postgres:** an `UPDATE`, `DELETE` or `SELECT ... FOR UPDATE` takes the rows it changes or locks until its transaction ends (outside one, until the statement ends), and a statement that meets a row another transaction holds waits for that transaction to commit or roll back (up to `lock_timeout`: 55P03; or `statement_timeout`, or a cancel: 57014). Two transactions that wait for each other in a cycle get one 40P01 (`deadlock detected`). Then the statement reads the row as the other transaction left it: at `READ COMMITTED` it runs again on what is committed then (what Postgres's EvalPlanQual does: a row that no longer matches the `WHERE` is skipped, one that still does is updated from its newest version, a deleted one is skipped), and at `REPEATABLE READ` it fails with 40001 (`could not serialize access due to concurrent update`) at the statement, as Postgres's does. A counter row that many clients update (pgbench's `pgbench_branches`) makes them take turns and none fails. Rows that were only read aren't held, as in Postgres.
- **Conflicts at COMMIT:** what takes no row intents (a merged world, a change through the API, an `ALTER TABLE` of a table both changed) can still make `COMMIT` or the next statement fail with 40001; two transactions that insert one primary key or unique value make the second to commit fail with 23505, at its next statement or at `COMMIT`, where Postgres makes it wait at the `INSERT`. A row a transaction inserted that another transaction deleted its parent of fails at `COMMIT` with 23503, where Postgres makes the deleter wait. Retry on 40001 and 40P01, as you would for Postgres.
- **Row locks:** `SELECT ... FOR UPDATE` (also `NO KEY UPDATE`, `SHARE`, `KEY SHARE`, `OF table`, `NOWAIT`) holds the rows it returns until the transaction ends, in Postgres's modes (`FOR KEY SHARE` conflicts only with `FOR UPDATE`; `FOR SHARE` with `FOR UPDATE` and `FOR NO KEY UPDATE`; shared locks don't clash with each other), and a statement that wants one in a mode that conflicts waits. Of two transactions that lock the same unit, check it's free and hold it, the second waits for the first and then sees the hold. A `ROLLBACK TO` lets go of what was taken since its savepoint, and a `RELEASE`d savepoint's rows belong to the level it was released into. An error lets go of the transaction's row locks and transaction-level advisory locks at once, as Postgres's does (with savepoints, of those taken since the innermost one), though the transaction is refused (25P02) until its `ROLLBACK`. `SKIP LOCKED` leaves out the rows another open transaction holds (or has changed or deleted), before `LIMIT` and `OFFSET` count, and `NOWAIT` fails with 55P03 on one: a worker that takes `SELECT ... FOR UPDATE SKIP LOCKED LIMIT 1` in its own transaction never gets a row another worker is on. They work in a subquery or a `WITH` query as well (`WITH next AS (SELECT id FROM jobs WHERE state = 'new' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) UPDATE jobs SET state = 'run' FROM next WHERE jobs.id = next.id`). Not allowed with `DISTINCT`, `GROUP BY`, aggregates, window functions or `UNION` (0A000). A transaction that holds more than 10,000 rows holds their tables instead, so that big statements stay in bounded memory: others wait for any row of those tables.
- **Isolation levels:** `BEGIN ISOLATION LEVEL ...`, `SET TRANSACTION ISOLATION LEVEL ...` (before the first query: 25001 after), `SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL ...` and `default_transaction_isolation` choose `READ COMMITTED` (the default; `READ UNCOMMITTED` is it too, and says so in `SHOW`) or `REPEATABLE READ`; `SHOW transaction_isolation`, `current_setting('transaction_isolation')` and `pg_settings` say which. `SERIALIZABLE` fails with 0A000: snapshot isolation lets write skew through, so a transaction that asked for more isn't handed less without a word. Lock the rows a decision rests on instead. A function a statement calls reads the statement's snapshot (Postgres gives a volatile function's own statements newer ones).
- **Savepoints:** `SAVEPOINT name`, `RELEASE [SAVEPOINT] name` and `ROLLBACK TO [SAVEPOINT] name`, nested as deep as you like. Each is a fork of the level before it: `RELEASE` merges it back, and `ROLLBACK TO` throws its changes away and keeps the savepoint open. `COMMIT` releases any still open.
- **After an error:** only `ROLLBACK`, `ROLLBACK TO SAVEPOINT` (which makes the transaction usable again) or `COMMIT` (which rolls back) is accepted, as in Postgres.
- **Not allowed inside a transaction:** branch statements.
- **Ending without COMMIT:** a transaction still open when its connection or HTTP request ends is rolled back.

## Advisory locks

Locks on numbers the application picks, as in Postgres: `pg_advisory_lock(key)`, `pg_advisory_xact_lock(key)`, `pg_try_advisory_lock(key)`, `pg_try_advisory_xact_lock(key)`, `pg_advisory_unlock(key)` and `pg_advisory_unlock_all()`, with one `bigint` key or two `integer` keys (a separate set of locks). `hashtext(text)` gives Postgres's own hash, so `pg_advisory_xact_lock(hashtext('setup:' || org_id))` locks the same key it would there.

- **Who holds them:** a session lock until `pg_advisory_unlock` or the session ends; a transaction lock (`_xact_`) until `COMMIT` or `ROLLBACK`, or, outside a transaction, until the statement ends. A session can take its own lock again, and unlocks as many times as it took it.
- **Waiting:** a lock another session holds waits, for as long as the statement may run (`statement_timeout`, a cancel: 57014). Sessions that would wait on each other in a cycle fail at once with 40P01. The `try` kinds return false instead of waiting. Only a statement that writes nothing waits; one that writes (an `INSERT ... SELECT` that takes a lock) fails with 55P03 when another session holds it, so take locks in a `SELECT` of their own.
- **What a transaction sees after the wait:** a `READ COMMITTED` transaction reads what is committed at each statement, so it sees what the holder committed. A `REPEATABLE READ` one reads the database as it was at `BEGIN`, so after waiting for a lock that another transaction released at its `COMMIT`, it reads the database anew from the next statement on; one that already wrote (or is inside a savepoint) can't, and fails with 40001: take advisory locks before writing.
- **Not supported:** shared locks (`pg_advisory_lock_shared` and the others ending in `_shared`): 0A000.

## Maintenance, DISCARD and LOCK

Statements Postgres has for looking after storage and sessions. They're parsed as Postgres parses them (options in parentheses or in the older spelling, table lists with columns) and refuse what Postgres refuses: a name that doesn't exist (42P01, 42703, 3F000), the wrong kind of object (42809), `ANALYZE` missing from a `VACUUM` with a column list (0A000), and a transaction block where there mustn't be one (25001).

- **`VACUUM`, `ANALYZE`, `REINDEX`, `CLUSTER`:** succeed and do nothing. Pages are copy-on-write, so there are no dead rows to reclaim; indexes are always current; no statistics are kept. `VACUUM` and `REINDEX ... CONCURRENTLY` are refused inside a transaction block, as in Postgres. A view or sequence given to `VACUUM` or `ANALYZE` is skipped with a warning. `CLUSTER` doesn't remember its index.
- **`DISCARD ALL`:** what a connection pool (pgbouncer's `server_reset_query`) sends between clients. It puts the session back as it was on connecting: `SET` values and custom settings (`RESET ALL`), the time zone, `search_path` and `statement_timeout`, prepared statements, `LISTEN`s, temporary tables, session advisory locks, `currval`/`lastval`, and the **world**: the connection goes back to the world it logged in to (`SWITCH WORLD` is session state, so a pooled connection isn't handed on inside someone else's world). The worlds themselves are left as they are. Not inside a transaction (25001). `DISCARD PLANS` does nothing, `DISCARD SEQUENCES` forgets `currval` and `lastval`, `DISCARD TEMP` drops temporary tables (not inside a transaction: 0A000).
- **`COMMENT ON`:** kept for tables, views, materialized views, sequences, indexes, columns, table constraints and schemas ([Comments](#comments)); for any other object it checks nothing and says the comment isn't kept.
- **`LOCK TABLE`:** only in a transaction block (25P01). Writers here never take table locks, so nothing can be held against them. The modes that would keep writers out, or each other (`ACCESS EXCLUSIVE`, the default, and `EXCLUSIVE`, `SHARE ROW EXCLUSIVE`, `SHARE UPDATE EXCLUSIVE` and `SHARE`), take an advisory lock on the table's name until `COMMIT` or `ROLLBACK`: **another `LOCK TABLE` on it waits** (or fails at once with 55P03 under `NOWAIT`, or with 57014 at the `statement_timeout`; a cycle of them is 40P01), but plain reads and writes go on, and a warning says so. That's what a tool that locks a table to run one migration at a time needs. The weaker modes (`ACCESS SHARE`, `ROW SHARE`, `ROW EXCLUSIVE`) stop nothing that isn't stopped already, so they only check the table. To keep writers out, take `SELECT ... FOR UPDATE` on the rows, or an [advisory lock](#advisory-locks) that every writer takes.

## Stopping a statement

- **`SET statement_timeout = 5000`** (ms, or a string like `'5s'`, `'500ms'`, `'1min'`) stops any statement of this session that runs longer, with SQLSTATE 57014, as Postgres does. `0` turns it off; `RESET statement_timeout` (or `DEFAULT`) goes back to the database's (`ALTER SYSTEM SET statement_timeout`, none unless set); `SHOW statement_timeout` shows it.
- **An agent's `max_query_ms`** caps its sessions' timeout: it may set a lower one, never a higher one or none.
- **An agent's `max_memory_mb`:** a statement holding more rows than that is stopped with SQLSTATE 53200 (out of memory), and its sorts, groups and joins spill to disk at a quarter of it.
- **An agent's `max_concurrent`:** a statement past that many of its statements running at once is refused with SQLSTATE 53300.
- **Cancel requests** (psql's Ctrl-C, a driver's cancel) stop the statement running on that connection, also with 57014.
- **A client that disconnects** stops its running statement too: nobody is waiting for the answer.
- **How soon:** statements check every few thousand rows, so they stop within milliseconds of the limit.

## CHECK constraints

`CHECK (condition)` on a column or the table, with Postgres's default names (`items_price_check`, `items_check`, `items_check1`) or your own (`CONSTRAINT name CHECK (...)`).

- **When they're checked:** every insert and update, SQL or JSON, and every merge. A merge that brings in a `CHECK` added on a branch tests every row of the table, so rows the other side added meanwhile must pass too.
- **Null passes:** a condition that comes out null (`price >= 0` with a null price) doesn't stop the row, as in SQL.
- **Changing them:** `ALTER TABLE ... ADD CONSTRAINT ... CHECK` tests existing rows first; `DROP CONSTRAINT` removes one. Renaming a column renames it inside its checks; dropping a column drops the checks that use it.
- **Not allowed:** subqueries in a check (0A000), as in Postgres.

## Functions and triggers

```sql
do $$ begin insert into audit values (now(), 'migrated'); end $$;   -- an anonymous PL/pgSQL block
create procedure add_row(i int, s text) language plpgsql as $$ begin insert into t values (i, s); end $$;
call add_row(1, 'a');
call add_row(s => 'b', i => 2);
```

**Parameter defaults:** `CREATE FUNCTION f(a int, b text DEFAULT 'x', c int = 3)` (also for procedures): a call may leave out the last parameters (`f(1)`, `f(1, 'y')`), or name the ones it gives (`f(1, c => 7)`); only the last parameters may have defaults (42P13), a default must be a value of its parameter's type (42804), and two functions a call fits equally well are 42725, as in Postgres. The default is worked out for each call. `pg_get_function_arguments` shows it, `pg_get_function_identity_arguments` doesn't, and `pg_proc.pronargdefaults` counts them.

- **`DO` and procedures:** `DO [LANGUAGE plpgsql] $$ ... $$` (either order) runs a block once, tagged `DO`. `CREATE [OR REPLACE] PROCEDURE`, `CALL` and `DROP PROCEDURE` work as their function counterparts, tagged `CREATE PROCEDURE`, `CALL` and `DROP PROCEDURE`; a call may use names. A block or procedure writes atomically with the statement, in the session's transaction, and an error in it undoes what it wrote. A procedure is stored as a function returning void: `select proc()` and `call some_function()` work, where Postgres says 42809. Not yet: `INOUT`/`OUT` parameters, and `COMMIT`/`ROLLBACK` inside a procedure (PL/pgSQL has no transaction control here).

```sql
create function add1(int) returns int language sql as 'select $1 + 1';
create function mul(a int, b int) returns int language sql immutable return a * b;
create function recent(n int) returns setof orders language sql as $$
  select * from orders order by at desc limit n
$$;
select add1(41), mul(6, 7);
select * from recent(5);

create function log_change() returns trigger language plpgsql as $$
begin
  insert into audit (op, tbl, row_id) values (tg_op, tg_table_name, coalesce(new.id, old.id));
  return null;                     -- an AFTER trigger's result is ignored
end $$;
create trigger orders_audit after insert or update or delete on orders
  for each row execute function log_change();
```

- **Functions:** `CREATE [OR REPLACE] FUNCTION name([name] type, ...) RETURNS type | SETOF type | SETOF table | TABLE (name type, ...) | trigger | void` (types include enums), `LANGUAGE sql` or `plpgsql`, `IMMUTABLE` / `STABLE` / `VOLATILE`, `STRICT` (also `RETURNS NULL ON NULL INPUT`, `CALLED ON NULL INPUT`), and the body `AS $$ ... $$` (or `'...'`), `RETURN expression` or `BEGIN ATOMIC ...; END`. `SECURITY`, `PARALLEL`, `LEAKPROOF`, `COST` and `ROWS` are accepted and change nothing.
  - **Calling:** a function returning a value goes in any expression; one returning rows goes in `FROM` (`select * from recent(5) r`), where it can read the tables before it, as `LATERAL`. `public.name(...)` works too.
  - **Parameters:** `$1`, `$2`, ... or their names. In a `LANGUAGE sql` body a column of the same name (of a table, view, sequence or catalog table the statement reads) wins, as in Postgres.
  - **Overloading:** several functions may share a name with different argument types. A call picks the one whose types fit best (exact types, then numbers widening, then `text` for a quoted literal or NULL), as Postgres does in the common cases; one it can't choose is 42725. `DROP FUNCTION [IF EXISTS] name [(types)], ... [CASCADE]` (`CASCADE` drops the triggers that call it; without it they stop the drop, 2BP01).
  - **SQL bodies:** `SELECT`, `INSERT`, `UPDATE` and `DELETE` statements; the last one's first row (first column) is the result, or all its rows for a set. A body that's one `SELECT expression` is folded into the calling query, as Postgres inlines it. A body's statements (a PL/pgSQL function's too) read views, sequences and the catalog's tables as a statement of its own does, as they are each time they run: `pg_prepared_statements` and `pg_cursors` there list the calling session's.
  - **Writes:** a `VOLATILE` function (the default) may write; a `STABLE` or `IMMUTABLE` one that tries gets 0A000. What a function writes commits with the statement that called it, or not at all.
  - **Depth:** functions and triggers calling one another more than 40 deep stop with 54001 (endless recursion, or near the end of a server thread's stack).
- **Triggers:** `CREATE [OR REPLACE] TRIGGER name BEFORE | AFTER INSERT | UPDATE [OF col, ...] | DELETE [OR ...] ON table [FOR EACH ROW | STATEMENT] [WHEN (condition)] EXECUTE FUNCTION f(['arg', ...])` (`PROCEDURE` too), `DROP TRIGGER [IF EXISTS] name ON table`, and `ALTER TABLE t ENABLE | DISABLE TRIGGER name | ALL | USER`. As in Postgres, the default is `FOR EACH STATEMENT`, triggers of one kind fire in name order, and `WHEN` reads `NEW.col` and `OLD.col`.
  - **BEFORE ROW** triggers can change `NEW` (the row then written, and what `RETURNING` shows), or skip the row by returning NULL; `NOT NULL`, `CHECK`, unique and foreign keys are checked after them.
  - **AFTER ROW** triggers fire once the statement's rows are written and checked, one row after another, and see the final rows. Statement triggers fire before and after them.
  - **Errors:** `RAISE EXCEPTION` (or any error) in a trigger stops the statement: nothing it or its triggers wrote is kept, and in a transaction the transaction fails, as in Postgres.
  - **Where they fire:** SQL's `INSERT`, `UPDATE` and `DELETE`, on the world the statement runs on. They don't fire when a merge brings rows in (those rows are what the triggers already did on the branch; firing again would do it twice), or when a restart replays the log (it holds the rows triggers wrote). JSON writes to a table with triggers are refused (see [JSON writes into SQL tables](#json-writes-into-sql-tables)).
  - **Not yet:** `INSTEAD OF`, `TRUNCATE`, `REFERENCING` (transition tables) and constraint triggers; `INSERT ... ON CONFLICT DO UPDATE` on a table with row triggers, and `ON DELETE CASCADE` / `SET NULL` into one (each refused with 0A000 rather than skipping its triggers).
- **In worlds:** a function is a row (`_sonos_functions`), and a table's triggers are part of its schema, so both fork, merge, travel in time (`use world 'main@...'` calls the function as it was) and show in `DIFF` (`create function`, `+trigger name`) and `DIFF ... AS SQL`. Running `DIFF ... AS SQL`'s statements fires the triggers where they run.
- **Speed:** a call of a PL/pgSQL function costs about 1 µs; a row trigger that writes a row adds about 20 µs to each row written (an audit trigger on a 20,000-row `INSERT`: 0.4 s).

### PL/pgSQL

What Chronos runs, parsed when the function is made; anything else is refused then with 0A000 naming it, never guessed at when the function runs.

- **Blocks:** `[DECLARE ...] BEGIN ... END`, nested blocks. Declarations `name [CONSTANT] type [NOT NULL] [:= | = | DEFAULT expression]` and `name RECORD`; an inner block's variable hides an outer one of the same name.
- **Statements:** `name := expression` (also `=`, and `rec.field :=`, `NEW.col :=`), `IF ... THEN ... ELSIF | ELSEIF ... ELSE ... END IF`, `CASE [x] WHEN ... THEN ... ELSE ... END CASE` (no match and no `ELSE`: 20000), `LOOP`, `WHILE cond LOOP`, `FOR i IN [REVERSE] a..b [BY n] LOOP`, `FOR rec | a, b IN SELECT ... LOOP`, `EXIT [WHEN cond]`, `CONTINUE [WHEN cond]`, `RETURN [expression]`, `RETURN NEXT [expression]`, `RETURN QUERY SELECT ...`, `PERFORM ...`, `NULL`, `ASSERT cond [, message]`, `GET [CURRENT] DIAGNOSTICS v = ROW_COUNT [, ...]`.
- **Catching errors:** `BEGIN ... EXCEPTION WHEN condition [OR condition ...] THEN ... [WHEN ...] END`, as in Postgres. A condition is a name (`division_by_zero`, `unique_violation`, `check_violation`, `no_data_found`, ... Postgres's names for the errors Chronos raises, and their classes such as `data_exception` and `integrity_constraint_violation`, which catch every code of the class), `SQLSTATE 'xxxxx'` (a code, or a class's `xx000`), or `OTHERS`. An unknown name is 42704 when the function is made.
  - **The block's writes go back:** when a handler catches an error, what the block's statements wrote (its functions and triggers included) is taken back, as Postgres's implicit savepoint does; what the handler then writes stays, and commits with the statement. Variables keep the values the block gave them. An error no handler catches goes on out, as before. The block's declarations are outside its handlers, as in Postgres.
  - **In the handler:** `SQLSTATE` and `SQLERRM` hold the error's code and message; `GET STACKED DIAGNOSTICS v = RETURNED_SQLSTATE | MESSAGE_TEXT | PG_EXCEPTION_DETAIL | PG_EXCEPTION_HINT | PG_EXCEPTION_CONTEXT [, ...]` reads them (detail and hint from `RAISE ... USING`; `PG_EXCEPTION_CONTEXT` names the function, without Postgres's line and statement; `COLUMN_NAME`, `CONSTRAINT_NAME`, `TABLE_NAME` and the like are empty); `RAISE;` raises the error again. Outside a handler, both are 0Z002.
  - **Timeouts and cancels stay fatal:** `OTHERS` doesn't catch a statement timeout or cancel (57014) or a failed `ASSERT` (P0004), as in Postgres. Naming `query_canceled` catches one, but the statement is stopped again at its next check, within milliseconds, so a loop in the handler can't outlive the timeout.
  - **Cost:** a block with handlers costs nothing extra until an error is caught (the savepoint is where the statement's writes stand, and a copy of the branch that costs O(1)).
- **Dynamic SQL:** `EXECUTE string [INTO [STRICT] targets] [USING expression, ...]` runs a `SELECT`, `INSERT`, `UPDATE` or `DELETE` made at run time, with `$1`, `$2`, ... the `USING` values (build names with `format('%I', ...)` or `quote_ident`); `FOR target IN EXECUTE string [USING ...] LOOP` and `RETURN QUERY EXECUTE string [USING ...]` too. As in Postgres, `EXECUTE` sets `ROW_COUNT` and leaves `FOUND` alone, a null string is 22004, and without `STRICT`, `INTO` takes the first row. Other statements (DDL) in `EXECUTE` are refused with 0A000.
- **SQL:** `SELECT`, `INSERT`, `UPDATE` and `DELETE` (and `WITH`), with `INTO [STRICT] targets` (variables, a record, or `rec.field`s) after a `SELECT`'s list or a `RETURNING`. Without `STRICT`, no row sets the targets to NULL; with it, no row is P0002 and several P0003 (several rows from `INSERT/UPDATE/DELETE ... RETURNING ... INTO` too). A `SELECT` without `INTO` is 42601, as in Postgres: use `PERFORM`.
- **Variables in SQL:** statements and expressions read the variables; a name that is both a variable and a column of the statement's tables, views, sequences or catalog tables is 42702, as Postgres's default `plpgsql.variable_conflict = error` makes it.
- **`FOUND`** after `SELECT INTO`, `PERFORM`, `INSERT`/`UPDATE`/`DELETE`, `FOR` loops and `RETURN QUERY`.
- **`RAISE [DEBUG | LOG | INFO | NOTICE | WARNING | EXCEPTION] 'format %', args [USING MESSAGE | ERRCODE | DETAIL | HINT = ...]`**, or `RAISE SQLSTATE 'xxxxx'` or `RAISE condition_name`. `%` takes the next argument (`<NULL>` for null), `%%` is `%`. `EXCEPTION` stops with its SQLSTATE (P0001 by default; `ERRCODE` takes a code or a condition name such as `unique_violation`). `NOTICE`, `WARNING` and `INFO` go to the client as notices (psql shows them); `DEBUG` and `LOG` go nowhere. `DETAIL` and `HINT` go with the error or notice to the client, and reach a handler's `GET STACKED DIAGNOSTICS`.
- **Where an error came from:** an error leaving a function has Postgres's CONTEXT lines, innermost first: `PL/pgSQL function f(integer) line 4 at RAISE` (the line in the body, and the statement as Postgres names it: `at assignment`, `at SQL statement`, `at PERFORM`, ...; `during statement block local variable initialization` for a default), `SQL statement "..."` for a statement that failed as it ran, and the functions it came through. `RAISE;` raises an error again with the CONTEXT it had; `GET STACKED DIAGNOSTICS ... PG_EXCEPTION_CONTEXT` reads it.
- **Cursors:** `name [[NO] SCROLL] CURSOR [(arg type, ...)] FOR query` and `refcursor` variables; `OPEN` (a bound cursor with its arguments, by position or `name := value`; `OPEN c [[NO] SCROLL] FOR query` or `FOR EXECUTE ... [USING ...]`), `FETCH [direction] [FROM | IN] c INTO ...`, `MOVE`, `CLOSE`, and `FOR r IN c [(args)] LOOP`. A cursor is a portal of the session, named by its variable (`<unnamed portal n>` for a refcursor with none): a function can return it and another call, or the session's `FETCH` and `CLOSE`, read it until the transaction ends, and `pg_cursors` lists it.
- **`ALIAS FOR`** a parameter (`$1`) or a variable (`NEW`), and an expression with a `FROM` (`IF count(*) = 0 FROM t WHERE ... THEN`), which is the query `SELECT expression FROM ...`.
- **Triggers:** `NEW` and `OLD` (NULL where there is none, as `OLD` in an `INSERT`), `TG_OP`, `TG_NAME`, `TG_WHEN`, `TG_LEVEL`, `TG_TABLE_NAME`, `TG_RELNAME`, `TG_TABLE_SCHEMA` (`public`), `TG_NARGS` and `TG_ARGV[i]` (from 0). `RETURN NEW`, `RETURN OLD`, another record, or `RETURN NULL`.
- **Ends:** a function reaching `END` without `RETURN` is 2F005, except one returning `void` or a set.
- **Statement timeouts:** `SET statement_timeout` and cancel requests stop a looping function (loops check, as scans do).
- **A record used whole:** `to_jsonb(NEW)`, `row_to_json(OLD)`, `OLD IS NULL`, `NEW IS DISTINCT FROM OLD` and a loop's record (`to_jsonb(r)`) read the record as its fields in a JSON object, null while it holds no row (OLD in an INSERT). So an audit trigger that keeps `to_jsonb(OLD)` and `to_jsonb(NEW)` works. Two records compare as those objects do: a field that is null in both is the same in both. Elsewhere a record is its row as Postgres writes it, `(3,30,"a b")`: `RAISE NOTICE '%', NEW`, `NEW::text`, `format('%s', r)`. `NEW.*` is the record's fields: `VALUES (NEW.*)`, `SELECT r.*`, `ROW(OLD.*) = ROW(NEW.*)` (field by field), `NEW.*::text`.
- **Labels:** `<<name>>` before a block or a loop, `END name` and `END LOOP name`, `EXIT name` (out of that block or loop) and `CONTINUE name`, and `name.variable` for a variable of that block, even one a block further in hides (the function's own parameters by the function's name: `f.param`).
- **`FOREACH x [SLICE n] IN ARRAY a`**, `GET DIAGNOSTICS ... PG_CONTEXT` (the call stack, as Postgres writes it) and `PG_ROUTINE_OID`, `#variable_conflict error | use_variable | use_column` and `plpgsql.variable_conflict`, `#print_strict_params` and `plpgsql.print_strict_params`, `plpgsql.check_asserts`, and `plpgsql.extra_warnings` / `extra_errors` (`shadowed_variables`, `too_many_rows`, `strict_multi_assignment`; checks on a record's fields aren't made).
- **Not yet:** `OUT` / `INOUT` / `VARIADIC` parameters of procedures, `RETURNS record`, `ROLLBACK` in a procedure, DDL inside a function (`EXECUTE` included), and taking back a temporary table's changes when a handler catches an error (as with `ROLLBACK TO`).

## Indexes

`CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS] [name] ON table [USING btree] (column [ASC|DESC], ...)` and `DROP INDEX [IF EXISTS] name`.

- **What they speed up:** `=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN (...)`, `IN (SELECT ...)`, `IS NULL` and `LIKE 'prefix%'` on an index's first columns (equal values on the first columns, then a list or a range on the next), with the constant on either side and constant expressions such as `now() - interval '7 days'`.
- **Two ranges:** with a range on an index's first column and another on its second (`a BETWEEN 1 AND 5 AND b > 0` on an index on `(a, b)`), the second is checked on each index entry, so rows outside it are never read.
- **Reading in order:** `ORDER BY` on an index's columns with a `LIMIT` reads the index in order, forwards or backwards, and stops when it has enough rows: the latest 20 of 100,000 rows takes about 0.04 ms instead of 230 ms. Equal values on the first columns can come first (`WHERE owner = $1 ORDER BY at DESC LIMIT 20` uses an index on `(owner, at)`), and more `ORDER BY` keys after the indexed ones are fine.
- **Wide ranges:** a range that holds more than about a fifth of the table reads the table instead, which is faster. So does one whose row ids outgrow `CHRONOS_WORK_MEM` (about 4 million rows at the default 256 MB): an index read holds the ids it finds, then reads each row as the query reaches it.
- **Building one on a big table** reads the table once and sorts the entries within `CHRONOS_WORK_MEM`, spilling to disk past it (as `ORDER BY` does), so memory doesn't grow with the table. A `UNIQUE` index (or `ALTER TABLE ... ADD CONSTRAINT ... UNIQUE`) finds any duplicate before it writes anything. The entries are then written in parts with a checkpoint every `CHRONOS_WORK_MEM` of them, and the index exists only once the last part is in: after a crash mid-build there is no index, and the entries written so far are unused rows until an index of that name is made again (which deletes them first) or the table is dropped. Writes and merges to the world wait while it builds; reads and forks don't. Made on a world and then merged, the merge builds the entries in memory; inside a big `BEGIN ... COMMIT` the commit takes them as they are (see [big transactions](operations.md)), and inside a small one it builds them in memory too.
- **Kept up by every write:** SQL and JSON writes, `ON DELETE` cascades, `ALTER TABLE` and merges. An index made on a branch arrives with the merge, built for every row.
- **Branches and crashes:** entries are rows like any other, so forks share them for free and crash recovery rebuilds them. `DIFF` doesn't show them.
- **Unique indexes** enforce uniqueness, like a `UNIQUE` constraint of the same name. On expressions (`create unique index on users (lower(email))`) they compare the expressions' values; partial ones (`... where active`) only compare rows their `WHERE` holds for.
- **Names** share one namespace across tables, as in Postgres. With no name, one is made up (`users_email_idx`).
- **On expressions:** `create index on users (lower(email))` or `((doc ->> 'status'))` serves conditions on that same expression, like `where lower(email) = $1`. The expression can't use subqueries, aggregates, or functions whose value changes (`now()`, `random()`).
- **Partial:** `create index on orders (placed) where not archived` holds entries only for rows its `WHERE` is true for, so it stays small. A query uses it when its own conditions include that `WHERE`.
- **Renames and drops:** an index's expression or `WHERE` follows a renamed column, and goes when a column it reads is dropped, as in Postgres.
- **Options ORMs write:** operator classes that only say how to compare (`text_pattern_ops`, `varchar_pattern_ops`, `int4_ops` and the other default ones: Django adds a `_like` index with `varchar_pattern_ops` to every indexed CharField), `COLLATE` (the order the part keeps: see [Collations](#collations)), `INCLUDE (cols)` (the columns must exist), `WITH (fillfactor = 70)`, `TABLESPACE`, and `USING hash` or `brin`, which are btrees here (they find what a btree finds). Another operator class is 0A000.
- **Not yet:** `ON CONFLICT (lower(email))` naming a unique index by its expressions (use `ON CONFLICT DO NOTHING` without a target), GIN on anything but a tsvector, or GiST on anything but a point column (pgvector's `hnsw` and `ivfflat` are accepted, see [Vectors](#vectors-pgvector); text and vector search are built in instead). A `jsonb` column can't be indexed as a whole; index an expression on it.

## Vectors (pgvector)

pgvector's SQL works as it is, so tools written for Postgres with pgvector (Mem0's `pgvector` store, LangChain, LlamaIndex) run unchanged. Mem0's store is checked against it with psycopg 2 and 3.

```sql
create extension if not exists vector;           -- accepted: vectors are built in
create table items (id uuid primary key, embedding vector(3), payload jsonb);
create index on items using hnsw (embedding vector_cosine_ops);   -- the distance its graph is for
insert into items values ('a', '[1,0,0]', '{"user_id": "u1"}');
select id, embedding <=> '[1,0.1,0]' as distance from items
where payload->>'user_id' = 'u1' order by distance limit 5;
```

- **The type:** `vector` or `vector(n)`. Values are written `'[1,2,3]'`, as an array (`'{1,2,3}'`, `ARRAY[1,2,3]`: how drivers send lists) or as a JSON array. `vector(n)` refuses other lengths (22000); a vector has 1 to 16,000 numbers.
- **Distances:** `<=>` (cosine distance), `<->` (Euclidean), `<#>` (the negative inner product). Vectors of different lengths are an error (22000). A zero vector's cosine distance is NaN, as in pgvector.
- **Stored as 4-byte floats,** as pgvector stores them: a 1,536-dimension vector takes 8 KB in a row and reads back without parsing numbers. Written out, vectors look as pgvector writes them (`[1,0.5,-2]`); outside SQL they're JSON arrays, so `find` sees them too (see [search](search.md)).
- **`SET hnsw.ef_search = n`** (1 to 1000, or `default`) sets the graph's beam for the session's searches, as in pgvector; it matters only for tables big enough to get a graph.
- **Without an index, exact:** `ORDER BY distance` compares every row the `WHERE` keeps, like pgvector without an index.
- **With `USING hnsw` (or `ivfflat`), fast:** `ORDER BY <column> <op> <constant vector> LIMIT k` on one table gets its candidates from the search index (an exact scan, or its HNSW graph when the graph has shown it finds at least 99% of what the scan does, or the table is too big to scan; see [search](search.md)), then reads just the rows it returns and runs the query on them, so distances and conditions stay exact. Equality on a column or on `jsonb ->> 'key'` (Mem0's `payload->>'user_id' = $1`) narrows the candidates in the index; other conditions are checked on them, and more candidates are fetched until the `LIMIT` fills. On 76,000 real OpenAI embeddings that's 99.98% recall@10 in about 1 ms through the Postgres protocol (see [benchmarks](https://github.com/Abhishekxdg/chronosdb/blob/main/BENCHMARKS.md)). Candidates are ranked by dot product, which is cosine for the unit-length vectors most embedding models give. The index's options (`m`, `ef_construction`) are ignored: the search index has its own.
- **Operator classes:** `vector_cosine_ops`, `vector_l2_ops` (the default) and `vector_ip_ops` are kept with the index and shown by `pg_get_indexdef` and `pg_indexes` as Postgres shows them (the default left out); any other is 42704. The search index ranks every vector column by inner product, which is also the cosine and L2 order for vectors of unit length (most embedding models give those). For vectors that aren't, a cosine or L2 index gives that distance a graph of its own (over unit-length copies for cosine, by Euclidean distance itself for L2), so `ORDER BY <=>` or `<->` walks it rather than comparing every row; it takes as much memory again as the column's search index, and each answer is scored by the true distance. Until that graph is built and has tested itself (as above), those queries compare every row, exactly.
- **Indexes:** an `hnsw` index is a note on the table, since the search index does the work (`DROP INDEX` removes it); GIN (or GiST) indexes on `to_tsvector(...)` or a `tsvector` column, and GiST indexes on points, are real indexes (see the full-text and points entries above). Other index methods are refused (0A000).
- **Over the Postgres port:** text as pgvector writes it (`[1,2,3]`), and pgvector's binary form. The type's id is fixed (90000); drivers that don't know it read vectors as text.
- **Also for these tools:** `information_schema.tables` (the branch's tables, in schema `public`; see [the catalog](#the-catalog-information_schema-and-pg_catalog)), `public.table` names in `FROM`, and `DEALLOCATE` (see [Prepared statements](#prepared-statements)).

## EXPLAIN

`EXPLAIN [ANALYZE] statement`, or Postgres's `EXPLAIN (ANALYZE, FORMAT TEXT, ...)`, for `SELECT`, `INSERT`, `UPDATE` and `DELETE`. It returns one `QUERY PLAN` column in Postgres's layout:

```
Limit  (actual rows=20)
  ->  Sort
        ->  Index Scan Backward using events_owner_at on events  (actual rows=20)
Execution Time: 0.061 ms
```

- **Each table's read** shows how Chronos reached its rows: `Index Scan using <index>` with its `Index Cond`, the primary key, the search index, or `Seq Scan`. A table looked up once per row of a join shows `loops=`.
- **`ANALYZE` runs the statement**, as in Postgres (so `EXPLAIN ANALYZE DELETE` deletes), and adds actual row counts and the time taken. Without it, a write only finds its rows.
- **Not shown:** costs, and how joins pair rows beyond the order tables are read in.

## Text order

Text compares and sorts by its bytes (UTF-8) unless it is given a collation, as Postgres does with `COLLATE "C"`: `'Z'` comes before `'a'`, and `'é'` after `'z'`. That is the database's collation (`default`, the same as `C`, `POSIX` and `ucs_basic`), and indexes keep the same order, so `LIKE 'abc%'` is a range. A Postgres database made with another collation (such as `en_US.UTF-8`) orders text differently: give the columns an ICU collation to sort as a language does.

### Collations

ICU collations sort text as a language does. Chronos has the ones Postgres 16 makes when a database is created (`unicode`, `und-x-icu`, `de-x-icu`, `sv-x-icu`, `en-US-x-icu`, ... 869 of them: `select collname from pg_collation`), and `CREATE COLLATION` makes more:

```sql
create collation german_phonebook (provider = icu, locale = 'de-u-co-phonebk');
create collation numeric (provider = icu, locale = 'und-u-kn-true');   -- 'x9' before 'x10'
create table people (id int primary key, name text collate german_phonebook, city varchar(40) collate "sv-x-icu");
create index people_city on people (city);                             -- kept in sv's order
select name from people order by name;                                 -- in the column's collation
select city from people order by city collate "de-x-icu" limit 10;     -- or the one asked for
```

- **Where a collation counts:** comparisons (`<`, `<=`, `>`, `>=`, `BETWEEN`), `ORDER BY` (of a query, a set operation, a window, an aggregate: `string_agg(x, ',' order by x)`), `min` and `max`, `greatest` and `least`, `x < ANY (a, b)`, and `upper` and `lower`, which map case as ICU does in the collation's language (`upper('straße' collate "de-x-icu")` is `STRASSE`; Turkish has its dotted i). Equality, `GROUP BY`, `DISTINCT`, unique keys and hashing are bytes in a deterministic collation: where ICU ties two strings, their bytes decide, as in Postgres. A nondeterministic one is [below](#nondeterministic-collations). `ILIKE` folds both sides to lower case as `lower` does in the collation's language (Turkish `I` is `ı`); `LIKE` and regular expressions work as they do without one.
- **Which collation:** as Postgres works it out. A column's (`COLLATE` in `CREATE TABLE`, `ADD COLUMN` or `ALTER COLUMN ... TYPE ... COLLATE`), a literal's the default, and a function of text takes its arguments'. An explicit `COLLATE` wins over a column's, and a collation other than the default wins over the default. Two different explicit ones are 42P21; two different columns' where an order is needed are 42P21 (`ORDER BY`, `UNION`) or, where a comparison runs, 42P22: say which with `COLLATE`. A subquery's, a view's, a materialized view's, a CTE's and `CREATE TABLE AS`'s columns keep their collations, and a subquery reads an outer query's column in its collation (LATERAL too). A child table's column has its parents' collation: one written with another (or without `COLLATE`, which is the default) is 42P21 in `INHERITS`, `ALTER TABLE ... INHERIT`, `ATTACH PARTITION` and a parent's `ADD COLUMN`, as in Postgres, and a `COLLATE` in a `PARTITION OF` column list is taken and the parent's kept. `LIKE` matches bytes in every collation, but two explicit collations on its sides are 42P21, as Postgres derives them.
- **Indexes:** an index part keeps its column's collation, or the one `CREATE INDEX ... (col COLLATE "de-x-icu")` names. A query uses it for `ORDER BY ... LIMIT` and for ranges only in that collation, and for equality in any.
- **CREATE COLLATION:** `(provider = icu, locale = '...')` takes a BCP 47 locale (`de-DE`, `sv`, `und`), a POSIX one (`de_DE`, kept as `de-DE`), or ICU's `@` keywords (`de@collation=phonebook`), with the keywords `co` (`phonebk`, `trad`, `pinyin`, `stroke`, ...), `kn` (numbers by value), `kf` (upper or lower case first), `ks` (strength: at `level2` case ties, then bytes decide), `ka`, `kv` and `kc`. The locale must be one Postgres makes a collation of (`select colliculocale from pg_collation`) or a region alias checked to order the same (`zh-TW`, `sr-RS`): ICU4X resolves others (`sr-ME`, `uz-AF`, `zh-Latn`) to other rules than ICU4C, so they are refused (0A000), as are the types `search` and `searchjl` and Chinese `standard`. A keyword's value ICU doesn't know (`kf-xyz`) is XX000, as in Postgres. `deterministic = false` makes a [nondeterministic](#nondeterministic-collations) one (ICU only); `provider = libc` takes `C`, `POSIX` and `C.UTF-8`, and makes a libc locale (`de_DE.UTF-8`) the ICU collation of that locale (`de-DE`), and a DEFAULT naming a collation made here is 0A000. `CREATE COLLATION name FROM other` copies one, `ALTER COLLATION ... RENAME TO` and `SET SCHEMA` rename it (the columns and indexes of it follow), and `DROP COLLATION ... CASCADE` drops the columns, generated columns, CHECKs, indexes (of it, or whose expression or WHERE names it) and views that use it, found by the SQL's tokens (without CASCADE, 2BP01; a materialized view of it is dropped by hand first). RENAME is refused (0A000) while a view, CHECK, generated column or index expression names it: they keep their SQL. That SQL names a collation written without its schema with the schema the search_path found it in when it was made (`collate c` made under `search_path = a, b` is kept as `collate a.c`), as Postgres keeps the collation itself: it compares in that collation, and DROP COLLATION finds it, under any later search_path. A primary key's column can't be dropped here, so `DROP COLLATION ... CASCADE` reaching one is refused (0A000); give the column another collation first.
- **In worlds:** a collation is a row of its world: one made in a fork isn't in main until the fork merges, `DIFF` shows it (`create collation`), and a merge brings it with the tables that use it. A merge that would drop a collation a table still uses is refused (2BP01).
- **Not carried through yet, so refused (0A000) rather than answered by bytes:** text in an ICU collation ordered against a subquery's rows (`x < ALL (SELECT ...)`, `(x, 1) < (SELECT ...)`; `x < (SELECT ...)` and `= ANY` work) or against `ANY (array)`, and arrays and rows of such text ordered (`ORDER BY array[x]`, `ORDER BY (x, id)`, `max(array[x])`).
- **Older names:** `collate c`, `posix`, `"C.utf8"` (any case), which earlier versions read as text by bytes, still are, so views, CHECKs and functions kept with them read as before.
- **ICU4X, not ICU4C:** Chronos sorts with ICU4X (the pure-Rust ICU), where Postgres links ICU4C. For the locales Chronos has, the two order text the same (tests/pgdiff.rs compares them with Postgres 16 and ICU4C 78, and every locale was checked on 300 strings of many scripts), with one exception: ideographs outside the Basic Multilingual Plane (CJK Extension B and later, `𠀀`) sort after the other Han ideographs in ICU4X and before them in ICU4C. 39 of Postgres's ICU collations are left out: ICU4X has no rules for 37 of their languages, and orders Marathi's and Konkani's `क्ष` otherwise ([Known gaps](postgres-compatibility.md#known-gaps)). The ICU crates are pinned to exact versions, as an index keeps their sort keys; a collation made with another version warns when used, as Postgres does.


### Nondeterministic collations

```sql
create collation ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);   -- case doesn't count
create table users (id int primary key, email text collate ci unique);
insert into users values (1, 'Ada@Example.com');
select id from users where email = 'ada@example.com';      -- 1
insert into users values (2, 'ADA@EXAMPLE.COM');           -- 23505: the same email
```

- **Equal is what ICU ties:** at `ks-level2` strings that differ only by case are equal; at `ks-level1`, by case or accents.
- **By that:** `=`, `<>`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN (a, b)`, `x = ANY (a, b)`, `CASE x WHEN`, `NULLIF`, `IS DISTINCT FROM`, a join's `ON a = b`, `ORDER BY`, `min`, `max`, `greatest` and `least`, unique constraints, unique indexes and `ON CONFLICT` on a column of one, and `GROUP BY`, `DISTINCT` and `DISTINCT ON` (a group shows one of its strings, either of `'Ada'` and `'ADA'`, as in Postgres).
- **Refused (0A000), not done by bytes:** `UNION`, `INTERSECT` and `EXCEPT` (not `UNION ALL`), `count(DISTINCT x)`, `PARTITION BY`, `IN`, `ANY` and `ALL` of a subquery's rows or of an array, comparing arrays or rows that hold such text, a primary key, a foreign key, and a partition key. `LIKE` and `ILIKE` are refused as Postgres 16 refuses them.
- **Read through SQL:** a comparison goes by the collation where SQL makes it. The JSON row API and the search index (`find`) compare a column's text by its bytes.
- **Speed:** `email = 'x'` on such a column reads the rows; the unique constraint's own check is one lookup.

### citext

```sql
create extension if not exists citext;                 -- the type is built in; this succeeds
create table users (id int primary key, email citext unique not null);
insert into users values (1, 'Ada@Example.com');
select id from users where email = 'ada@example.com';  -- 1
select id from users where email like 'ADA%';          -- 1: citext's LIKE doesn't look at case
```

- **What it is:** text in the built-in collation `citext`, a nondeterministic one with no ICU locale: two strings are equal when their lower case is, and they sort by it. Everything [above](#nondeterministic-collations) holds for it, what is refused too (set operations, a primary or a foreign key on a citext column); `GROUP BY` and `DISTINCT` go by its lower case.
- **Where the type goes:** a column (`email citext`), a cast (`x::citext`), a domain (`create domain email as citext check (...)`), `ALTER TABLE ... ADD COLUMN`. `format_type`, `information_schema.columns` (`USER-DEFINED`, `citext`) and a diff's SQL say `citext`; on the wire the column is text.
- **Against text:** a citext compared with a quoted string is compared as citext. Compared with text of a type (a text column, `x::text`) it is compared as text, by bytes, as Postgres picks text's operator there.
- **Not yet:** `pg_typeof` says `text`; `~` and the other regular-expression operators, and `replace`, `strpos`, `split_part` and `translate`, look at case (the extension's don't); a `citext[]` is a `text[]`; a function's `citext` parameter is text inside it.

### hstore

```sql
create extension if not exists hstore;                 -- the type is built in; this succeeds
create table items (id int primary key, attrs hstore not null default '');
insert into items values (1, 'color=>red, size=>L'), (2, hstore('color', 'blue'));
select attrs -> 'color' from items where attrs ? 'size';        -- red
select id from items where attrs @> 'color=>blue';              -- 2
update items set attrs = attrs || 'stock=>3' where id = 1;
select key, value from items, each(attrs) where id = 1;
```

- **What it is:** text keys, each with a text value or NULL, kept and shown as Postgres writes it: `"color"=>"red", "size"=>"L"`, each key once, the shorter keys first. Two hstores with the same pairs are equal however they were written.
- **Operators:** `->` (a key's value, or an array of keys' values), `||`, `?`, `?|`, `?&`, `@>`, `<@`, `-` (a key, an array of keys, or another hstore's matching pairs), `=`, `<>`, and `h['key']`. An untyped string after `-` is read as an hstore, as in Postgres: write `attrs - 'key'::text` to take a key away.
- **Functions:** `hstore(key, value)`, `hstore(text[])`, `hstore(keys[], values[])`, `akeys`, `avals`, `skeys`, `svals`, `each`, `exist`, `defined`, `delete`, `slice`, `fetchval`, `hstore_to_array`, `hstore_to_matrix`, `hstore_to_json`, `hstore_to_jsonb` and their `_loose` forms, and a cast to `json` or `jsonb`.
- **Indexes:** `USING gin (attrs)` and `USING gist (attrs)` are made, listed and dumped, and hold no entries: `@>` and `?` read the rows. An index on one key, `((attrs -> 'color'))`, is a btree that finds it.
- **By pg_dump's names:** `public.hstore`, `public.akeys(...)`, `attrs OPERATOR(public.->) 'k'` and `public.gin_hstore_ops` read as the plain ones. `pg_type` has `hstore` and `_hstore`, for a driver that looks the type up by name.
- **Not yet:** `hstore(record)`, `populate_record`, `%%`, `%#` and `#=`; `(each(h)).key` in a select list (write `from each(h)`); `<` and `>` between hstores; binary parameters (send them as text).

### ltree

```sql
create extension if not exists ltree;                  -- the type is built in; this succeeds
create table nodes (id int primary key, path ltree not null);
create index on nodes using gist (path);
insert into nodes values (1, 'top'), (2, 'top.science'), (3, 'top.science.astronomy');
select path from nodes where path <@ 'top.science';             -- top.science and what is under it
select path from nodes where path ~ '*.science.*{1}';           -- top.science.astronomy
select subpath(path, 0, 2), nlevel(path) from nodes where id = 3;
```

- **What it is:** a path of labels separated by dots, each of letters, digits, `_` and `-`, kept as its text. `lquery` is a pattern over paths: `*`, `*{n,m}`, `a|b`, `!a`, and a label's `*` (starts with), `@` (any case) and `%` (its words, split at `_`).
- **Operators:** `@>` and `<@` (above and below, or equal), `~` (an lquery takes the path), `?` (any of an `lquery[]` does), `||`, and `<`, `<=`, `>`, `>=`, `=`, `<>`. With an `ltree[]` on one side, `@>`, `<@`, `~` and `?` are true when any of its paths is.
- **Order:** label by label, as Postgres orders them, so `top.a` comes before `top-x`: in `ORDER BY`, the comparisons, and an aggregate's or a window's `ORDER BY`. `min` and `max` go by the text (Postgres has neither for ltree).
- **Functions:** `nlevel`, `subltree`, `subpath`, `index`, `lca`, `text2ltree`, `ltree2text`.
- **Indexes:** `USING gist (path)` (with `gist_ltree_ops(siglen = n)` too) and `USING gin` are kept as a btree, which finds a path by `=`; `<@`, `@>` and `~` read the rows. `ORDER BY path LIMIT n` sorts rather than reading an index.
- **By pg_dump's names:** `public.ltree`, `public.nlevel(...)`, `path OPERATOR(public.<@) 'top'` and `public.gist_ltree_ops` read as the plain ones. `pg_type` has `ltree`, `lquery` and their arrays.
- **Not yet:** `ltxtquery` and its `@`; `?@>`, `?<@`, `?~` (the first match of an array); binary parameters.

## Moving from Postgres

`chronos import` with a Postgres URL moves a whole database, while it runs:

```bash
chronos import mydb postgres://ada:secret@db.example.com:5432/shop --dry-run   # what would come over, nothing written
chronos import mydb postgres://ada:secret@db.example.com:5432/shop
```

```
"customers": 5000 rows
"orders": 18230 rows

imported 2 of 2 tables (23230 rows) into main, and 3 foreign keys, indexes and views

also brought over (3):
  - function public.touch_updated_at()
  - trigger orders_touch on orders
  - 4 comments

brought over in part (1):
  - index orders_status: a hash index came over as a btree (it finds the same rows)
```

- **What comes over:** schemas, types (enums, domains with their default, `NOT NULL` and checks, composite types and ranges, in the order they depend on one another, in any schema), sequences (with their `CACHE`, set to where Postgres had them, so `serial` and identity columns go on from there), SQL and PL/pgSQL functions and procedures, tables with their columns, types, `NOT NULL`, defaults, `GENERATED ... AS IDENTITY` (`ALWAYS` or `BY DEFAULT`, with its sequence's options), primary keys, `UNIQUE`, `CHECK` and exclusion constraints, partitions (partitioned tables with their partitions and bounds) and inheritance (`INHERITS`), then every row, then foreign keys (checked against the rows), indexes, views and materialized views, and last triggers, rules and comments (on schemas, tables, views, sequences, indexes, columns, constraints, enum types and functions).
- **What comes over in part, listed:** an index's method that Chronos has no use for (`hash` and `brin` are btrees: they find the same rows), `DESC` and `NULLS FIRST`/`LAST`, `INCLUDE` columns, operator classes and storage parameters, which an index here doesn't keep. A table whose partitioning or inheritance Chronos refuses comes over as one plain table with all its rows, and says so.
- **What doesn't, reported instead of stopping the rest:** extensions Chronos doesn't have built in (those it has, `vector`, `postgis`, `pgcrypto`, `uuid-ossp`, `citext` and the others, are made as Postgres had them), functions in other languages, aggregates, operators, event triggers, publications, foreign tables, row-level security and its policies, base types, roles and grants (make the roles and `GRANT` again, or give each program an agent), and any table, type or statement Chronos refuses, with its reason. A table whose rows fail to load stays, empty, and is named in the report.
- **Row-level security:** a table whose rows its policies would keep from the role you import as isn't brought over in part: it's made empty, named (`"customers": not copied`), and the import exits with status 1. Import as the tables' owner, a superuser, or a role with `BYPASSRLS` (on Supabase, `postgres`). See [Migrating from Postgres](migrating-from-postgres.md#route-1-chronos-import).
- **Rows** move in COPY's text format, a table at a time, each as one `INSERT`: Chronos reads each value as its column's type, so types whose binary form it doesn't read (`uuid`, `bytea`, arrays) come over too, and floats come over exactly (Postgres 12 and later print them so they read back the same).
- **Connecting:** `sslmode` works as in libpq: `prefer` (the default: TLS if the server offers it, without checking whose certificate), `disable`, `require`, and `verify-full` (checks the certificate against `sslrootcert=<file.pem>` or the system's CAs; `verify-ca` does the same). Logins: trust, a password, MD5 and SCRAM-SHA-256, bound to the server's certificate over TLS (SCRAM-SHA-256-PLUS) where the server offers that, as libpq's `channel_binding=prefer` binds; a server that offers binding with a certificate whose hash can't be made (EdDSA) is refused, as libpq refuses it. `channel_binding=require` in the URL logs in only bound, and `disable` never binds. The password can come from `PGPASSWORD`. TCP only, no Unix sockets.
- **Where it goes:** into `main`, or `-b <world>`. The folder must not be open in another process. Names are kept: `public.orders` is `orders`, other schemas' tables keep their schema (`app.orders`).
- **Text order:** Chronos sorts text by bytes, as `COLLATE "C"` does, unless a column has a collation; columns' collations aren't imported yet, and a database's own (`en_US.UTF-8`) isn't one (see [Text order](#text-order)).

## Dumping with pg_dump

Postgres's own `pg_dump` reads a Chronos database through the catalog and writes a script that makes it again: schemas, enum types, functions and procedures, tables with their defaults, checks and identity columns, sequences (owned by their columns, with a `setval` to where they are), foreign keys, indexes, views, materialized views, triggers, and every row (`COPY`).

```bash
pg_dump -h 127.0.0.1 -p 5433 -U postgres --no-owner main > main.sql
psql -h other-host -d newdb -f main.sql          # into Postgres, or into another Chronos
```

- **Owners and grants:** what no role made is owned by `chronos`, so without `--no-owner` the script has `ALTER ... OWNER TO chronos` lines, which Postgres refuses where there is no such role (and Chronos takes `OWNER TO postgres` as `chronos`). An object a role made is that role's (`ALTER TABLE s.t OWNER TO app`), and its grants are `GRANT` and `REVOKE` lines. The roles themselves aren't in it (as in Postgres, they're `pg_dumpall --roles-only`'s): make them first where it restores. `ALTER TYPE | DATABASE ... OWNER TO` are accepted with a notice (types and databases keep no owner here).
- **Restoring into Chronos:** a dump of a Chronos database restores into Chronos, and a second dump of it is the first, byte for byte (`tests/pg_dump.rs`). A dump of a Postgres database restores if it stays within what Chronos has ([Not yet](#not-yet), [Known gaps](postgres-compatibility.md#known-gaps)).
- **Into Postgres:** the script restores there without an error, and the rows, sequences and functions come back as they were. Views come back as the query was written to Chronos (Postgres writes its own form of it), and types Chronos keeps as another (`json` as `jsonb`, `real` as `double precision`) come back as that one.
- **What isn't in a dump:** comments on functions, types and the like (`COMMENT ON` keeps those on tables, columns, views, sequences, indexes, constraints and schemas, and `pg_dump` writes them), roles, and what Chronos has beyond Postgres: worlds, agents, time travel. A world is a database: dump each one (`pg_dump -d app`).
- Every query `pg_dump` sends is answered from the catalog, so `pg_dump --schema-only`, `--data-only`, `-t table`, `-n schema`, `--inserts` and `-Fc` work the same way.

## Importing a Postgres table

`chronos import` copies one table from Postgres into a new table, to try Chronos on your own data.
Make two files with psql, putting your table's name in place of `TABLE` (running `chronos import`
with no arguments prints these commands too):

```
psql -c "\copy TABLE to 'data.csv' csv header"
psql --csv -t -c "select c.column_name, c.data_type, c.is_nullable, exists (select 1 from information_schema.table_constraints tc join information_schema.key_column_usage k using (constraint_schema, constraint_name) where tc.table_name = c.table_name and tc.table_schema = c.table_schema and tc.constraint_type = 'PRIMARY KEY' and k.column_name = c.column_name) from information_schema.columns c where c.table_name = 'TABLE' order by c.ordinal_position" > schema.csv
chronos import mydb TABLE data.csv --schema schema.csv
```

- Types: integers, `bigint`, `real`/`double precision`/`numeric` (as floats), `text`/`varchar`/`char`, `uuid`
  (as text), `boolean`, `date`, `timestamp`, `timestamptz`, `interval`, `json`/`jsonb`. Any other type
  (arrays, enums, `bytea`, ...) comes in as text, with a warning naming the column.
- An empty field is NULL and a quoted empty field (`""`) is an empty string, as `\copy` writes them.
- The table needs a primary key, of one column or several. Tables with none are refused.
- It's all or nothing: a bad row or value stops the import, names the line and column, and leaves no
  table behind. Importing into a table name that already exists is refused.
- `-b <branch>` imports into a branch other than main.

Sample data to try it on: `cargo run --example sample_data -- sample/` writes 5,000 customers and
their orders (with dates relative to today; exactly 1,240 customers have been inactive for over 90
days) plus their schema files, ready for `chronos import`.

## Speed

- **Lookups by primary key** read one row, and `key IN (...)` reads each. A range on the key (`BETWEEN`, `<`, `>`, one-sided too) reads just the rows in it, however wide: keys are stored in the order SQL compares them, integers as numbers and text as text. Tables made with 0.1.0 or before keep integer keys as their digits: there a range on one is read key by key up to 100,000 keys wide, and wider ones scan the table.
- **B-tree indexes** answer the conditions above (see [Indexes](#indexes)).
- **`column = value` conditions** on integer, bigint, text or boolean columns with no B-tree index (joined by `AND`) use the search index the rest of Chronos already keeps for the table. The rest of the `WHERE` is checked on the rows the index finds. It's built on first use and updated incrementally, per branch. It holds the table in memory many times over (85 times for a 150,000-row table of short text, an md5 and timestamps: 1.57 GB; see [operations](operations.md#databases-bigger-than-memory)), so a query builds it only for tables whose rows fit in an eighth of `CHRONOS_WORK_MEM` (32 MB by default); bigger tables are scanned, unless a search already built their index.
- **Unique checks** (`UNIQUE`, `ON CONFLICT`) look up the constraint's own entry: one read, whatever the table's size.
- **Other conditions scan the table:** conditions on unindexed columns, `OR`, and float columns without an index.
- **Rows a query doesn't read** are skipped as each row is decoded.
- **Big queries use every core.** Above about 8,000 rows, scans (reading, decoding and filtering), building and probing hash joins, and `GROUP BY` split the rows into parts of about 4,000 that the cores take in turn, so fast and slow cores both stay busy. It scales with the machine on its own: Chronos uses as many threads as the operating system says it may (container CPU limits included). Queries running at the same time share those threads rather than each starting its own, so a busy server doesn't run more threads than cores. `CHRONOS_THREADS=n` sets the number instead. The parts depend on the data, not on the number of cores, so the answer is the same on any machine, sums of floats included. Queries with subqueries in their conditions, and `count(DISTINCT ...)`-style aggregates, run on one core.
- **Memory:** one-table `GROUP BY`, `DISTINCT` aggregates and `ORDER BY` spill to disk past `CHRONOS_WORK_MEM` (256 MB by default), and `ORDER BY ... LIMIT k` holds only k rows however big the table: see [operations](operations.md#memory-and-spilling-to-disk). So do the writes of a big `INSERT`, `UPDATE`, `DELETE` or `COPY`, which stays one atomic statement; `ON CONFLICT`, `RETURNING`, tables with triggers and a few other cases still hold them (the list is in [operations](operations.md#databases-bigger-than-memory)).
- **Correlated `EXISTS`, `NOT EXISTS` and `IN` run once.** `exists (select 1 from leads l2 where l2.company = l1.company and l2.created_at > l1.created_at)` used to read `leads` again for every row of `l1`. Now the subquery runs once without the conditions that read the outer row, its rows are kept by the value of `l2.company`, and each outer row finds its candidates by that value and checks only the other conditions (`l2.created_at > l1.created_at`) on them. A null on either side of the `=` matches nothing, as before. It applies when the subquery has no `LIMIT`, grouping, aggregate, nested subquery or volatile function, reads the outer row only in its `WHERE`, has an `=` between one of its own expressions and one of the outer row's (of types that hash as they compare: the same type, or integers with numeric; a float against an integer compares through a double, so it stays per outer row), and its rows fit in a quarter of `CHRONOS_WORK_MEM`. Otherwise it runs per outer row, as before. So does one whose `=` compares an indexed column or the primary key, where each outer row's lookup is cheap: until those lookups have taken half a second in all, when it's hashed after all (an index on a column with few values picks too many rows to be cheap). On 150,600 rows with 15,000 of them outer, the first form took two minutes and takes about 0.1 s.
- **Joins on an expression hash too.** `a join b on lower(a.email) = lower(b.email)` hashes the expression of the second table's columns the way a join on a column does, instead of comparing every pair (both sides the same type, or integers and numeric).
- **`x IN (select ...)` and `NOT IN` hash the list** once it has 16 rows or more, when its values are all integers and numeric, all floats, all text, all dates or all booleans (a null in the list still makes `NOT IN` unknown); a table's rows are looked up in the set, not compared with every value in the list.
- **Big joins join while reading.** In a join of two tables, each core reads part of the first and joins it straight away; with `GROUP BY`, it groups the part too when the aggregates don't depend on how rows are split (`count`, `min`/`max` of a column, `sum`/`avg` of an integer column). If the first table's own conditions leave 1,000 rows or fewer, the join looks each match up by key instead, as before. Integer `sum` and `avg` add in 128 bits, so only the total has to fit in a `bigint`, as in Postgres.
- **Errors in big queries:** when a query would fail in more than one way, which error it reports can vary between runs, as with Postgres's parallel queries. The results of queries that succeed never vary.

See [BENCHMARKS.md](https://github.com/Abhishekxdg/chronosdb/blob/main/BENCHMARKS.md#4-sql-over-the-postgres-protocol) for numbers against Postgres.

Every write to a SQL table, SQL or JSON, stores the column's type, so index lookups and scans agree.

### Records

A function that gives several columns, called in a select list, gives one value: a record, written as Postgres writes a row, `(10,1)`. `(expr).field` reads one of its fields (`(r).x`, `(q.r).x`, `(information_schema._pg_expandarray(i.indkey)).n`), typed as the field is; `(1).x` is 42809 (not a composite type) and a field it hasn't is 42703. Today one function gives records, `information_schema._pg_expandarray(array)`, each element as `x` and its place as `n` (JDBC drivers read an index's columns with it); `ROW(...)` values and whole rows are still text and JSON, and `(record).*` and declaring a composite type (`CREATE TYPE ... AS (...)`) are not supported. `pg_get_keywords()` lists Postgres 16's SQL keywords (word, category code, whether it can be a bare label, and descriptions), and `regproc` and `regprocedure` read any of Postgres's built-in functions by name or oid and back (`'now'::regproc::oid` is 1299, `42::regproc` is `int4in`, `'int4in(cstring)'::regprocedure`), as the catalog's regproc columns (`typinput`) hold them.

## Not yet

- **Functions:** `to_char`'s `EEEE`, `RN`, `TH` and `V` number patterns, among others.
- **COPY:** the binary format for `COPY ... TO` (`FROM` works), `COPY ... FROM ... WHERE`, and files or programs on the server (refused on purpose; use psql's `\copy`).
- **Types and schemas:** composite and range types (domains work).
- **Temporary tables:** savepoints (see [Temporary tables](#temporary-tables)).
- **User-defined functions and triggers:** the parts of `CREATE FUNCTION`, PL/pgSQL and `CREATE TRIGGER` listed as not yet under [Functions and triggers](#functions-and-triggers), and other languages than `sql` and `plpgsql`.
- **Views:** `WITH CHECK OPTION` on views that aren't automatically updatable, `ON CONFLICT` through a view, `ALTER VIEW ... RENAME COLUMN`, and renaming a table or view another view reads.
- **Catalog:** `pg_enum`, `pg_proc` and other catalog tables not listed under [the catalog](#the-catalog-information_schema-and-pg_catalog).

Each gives a clear error rather than a wrong answer.

## Network address types

`inet` (a host or network address, v4 or v6, with an optional mask), `cidr` (a network: host bits must be zero), `macaddr` and `macaddr8`. They order as Postgres orders them (family, then address, then mask), index, aggregate with `min`/`max`, and read back as text. Operators: `<<`, `<<=`, `>>`, `>>=`, `&&` (contained or contains), `+` and `-` with an integer, `inet - inet`, and `&`, `|`, `~`. Functions: `host`, `text`, `abbrev`, `family`, `masklen`, `netmask`, `hostmask`, `broadcast`, `network`, `set_masklen`, `inet_same_family`, `inet_merge`, `trunc` (macaddr), `macaddr8_set7bit`.

## SQL/JSON path

`jsonb_path_exists(target, path [, vars [, silent]])`, `jsonb_path_match`, `jsonb_path_query_array`, `jsonb_path_query_first` and the set-returning `jsonb_path_query` (each with a `_tz` form that may compare a date with a timestamp with time zone in the session's zone), and the operators `jsonb @? path` and `jsonb @@ path` (silent, no variables). The path language is Postgres 16's (not 17's new item methods `.bigint()`, `.string()`, ...). A path is text here: `'$.a'::jsonpath` is accepted, and a malformed one fails when it is used.
