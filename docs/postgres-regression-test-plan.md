---
title: Postgres Regression Test Suite Against Chronos
---

## Goal

Run Postgres 16's full regression test suite (~222 SQL test files) against Chronos, measure pass rate, and use results to prioritize remaining gap work.

This replaces guessing about what matters. Real data on what breaks tells us which gaps will have the most impact.

## Approach

1. **Setup phase:** Run Postgres's `test_setup` script on both Postgres and Chronos to create required tables and fixtures.

2. **Test phase:** For each test file in `src/test/regress/sql/*.sql`:
   - Execute the SQL against both systems
   - Record success/failure (exit code + error)
   - Count output lines (rough proxy for query volume)

3. **Analysis phase:**
   - Tally: total tests, passed, failed, error rate
   - Group failures by category (type system, DDL, DML, functions, etc.)
   - Correlate to gap list in `postgres-full-gap-list.md`
   - Identify highest-impact gaps to close next

## Test Categories (from parallel_schedule)

### Essential (test_setup required)

`test_setup` — creates base schema and data (`onek`, etc.)

### Type Tests

`boolean`, `char`, `name`, `varchar`, `text`, `int2`, `int4`, `int8`, `oid`, `float4`, `float8`, `bit`, `numeric`, `txid`, `uuid`, `enum`, `money`, `rangetypes`, `pg_lsn`, `regproc`, `strings`, `md5`, `numerology`, `point`, `lseg`, `line`, `box`, `path`, `polygon`, `circle`, `date`, `time`, `timetz`, `timestamp`, `timestamptz`, `interval`, `inet`, `macaddr`, `macaddr8`, `multirangetypes`

### SQL & Operations

`select`, `join`, `expressions`, `misc`, `copy`, `copyselect`, `copydml`, `insert`, `insert_conflict`, `update`, `delete`, `create_table`, `create_view`, `create_schema`, `create_index`, `drop`, `truncate`, `alter_table`

### Advanced

`create_type`, `create_function_c`, `create_operator`, `create_procedure`, `create_view`, `create_aggregate`, `create_cast`, `horology`, `tstypes`, `geometry`, `regex`, `opr_sanity`, `type_sanity`, `index_including`, `index_including_gist`, `privileges`, `security_label`, `json`, `jsonb`, `range`, `window`, `with`, `partition_prune`

## Success Criteria

- **Target:** 90%+ pass rate (same acceptance bar as Postgres compatibility doc)
- **Data collection:** Record every failure with test name, SQL snippet, error message
- **Actionable:** Failures map to specific gap list items

## Known Limitations

- This runs *individual test files* as monolithic SQL scripts, not the structured regression framework.
- Some tests depend on prior state (e.g., `create_index` assumes `create_table` ran first).
- Output comparison is rough (line count, not exact format match).
- No expected output validation (just success/failure).

To run the full framework with exact output matching would require pg_regress itself, which is C-based and not portable.

## Timeline

- **Setup & test extraction:** today
- **Initial run (all ~220 tests):** 1–2 hours
- **Analysis & prioritization:** 1 hour
- **Next sprint:** focus on top-impact gaps

## Commands

```bash
# Start Chronos server
chronos serve /tmp/chronos-test --pg 127.0.0.1:5433 &

# Run regression tests
PG_HOST=/tmp tests/run_postgres_regression.sh

# View results
cat /tmp/chronos-regress/results.txt
```
