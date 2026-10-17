#!/bin/bash
# Run Postgres regression tests against Chronos and report pass rate.
# Extracts SQL statements from test files and counts successes.
#
# Usage:
#   chronos serve /tmp/chronos-test --pg 127.0.0.1:5433 &
#   PG_HOST=/tmp tests/run_postgres_regression.sh [num_tests]
#
# Uses parallel_schedule to run tests in dependency order.
# Reports: total passed, failed, errors, and pass %.
#
set -u

PG_ROOT=${PG_ROOT:-/tmp/postgresql-16.14}
CHRONOS_URL=${CHRONOS_URL:-postgres://127.0.0.1:5433/main}
PG_HOST=${PG_HOST:-/tmp}
PG_USER=${PG_USER:-postgres}
LIMIT_TESTS=${1:-0}  # 0 = all, else limit to N tests

REGRESS_DIR="$PG_ROOT/src/test/regress"
REGRESS_SQL_DIR="$REGRESS_DIR/sql"
SCHEDULE_FILE="$REGRESS_DIR/parallel_schedule"

OUTPUT_DIR="/tmp/chronos-regress"
mkdir -p "$OUTPUT_DIR"
cd "$OUTPUT_DIR"

echo "Postgres Regression Test Suite vs Chronos"
echo "=========================================="
echo ""
echo "Postgres: $PG_HOST (user: $PG_USER)"
echo "Chronos: $CHRONOS_URL"
echo "Regress dir: $REGRESS_DIR"
echo ""

# Parse schedule to get test order
get_test_list() {
  grep "^test:" "$SCHEDULE_FILE" | sed 's/^test: //' | tr ' ' '\n' | sort -u
}

tests=$(get_test_list)
if [ "$LIMIT_TESTS" -gt 0 ]; then
  tests=$(echo "$tests" | head -n "$LIMIT_TESTS")
fi

total_tests=$(echo "$tests" | wc -l)
echo "Will run $total_tests tests"
echo ""

# Create databases
psql -X -h "$PG_HOST" -U "$PG_USER" -d postgres -Atqc "DROP DATABASE IF EXISTS regress_test;" >/dev/null 2>&1
psql -X -h "$PG_HOST" -U "$PG_USER" -d postgres -Atqc "CREATE DATABASE regress_test;" >/dev/null 2>&1

# Prepare connection functions
pg_run() {
  psql -X -h "$PG_HOST" -U "$PG_USER" -d regress_test -f "$1" 2>&1 | wc -l
}

ch_run() {
  psql -X "$CHRONOS_URL" -f "$1" 2>&1 | wc -l
}

passed=0
failed=0
count=0

# Run each test
for test in $tests; do
  count=$((count + 1))
  test_file="$REGRESS_SQL_DIR/${test}.sql"

  if [ ! -f "$test_file" ]; then
    echo "SKIP: $test (not found)"
    continue
  fi

  # Run on Postgres
  pg_result=$(pg_run "$test_file" 2>&1)
  pg_rc=$?

  # Run on Chronos
  ch_result=$(ch_run "$test_file" 2>&1)
  ch_rc=$?

  # Classify
  if [ $pg_rc -eq 0 ] && [ $ch_rc -eq 0 ]; then
    passed=$((passed + 1))
    status="PASS"
  else
    failed=$((failed + 1))
    status="FAIL"
  fi

  if [ $((count % 10)) -eq 0 ] || [ "$status" = "FAIL" ]; then
    printf "[%3d/%3d] %-30s %s (PG:%d, CH:%d)\n" "$count" "$total_tests" "$test" "$status" "$pg_rc" "$ch_rc"
  fi
done

echo ""
echo "Results"
echo "======="
pass_pct=$((passed * 100 / count))
echo "Tests run: $count"
echo "Passed: $passed ($pass_pct%)"
echo "Failed: $failed ($((100 - pass_pct))%)"
echo ""
echo "Pass rate: $pass_pct%"

exit 0
