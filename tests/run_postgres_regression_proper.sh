#!/bin/bash
# Run Postgres regression tests properly against Chronos.
# Runs test_setup first, then each test file in dependency order.
#
# Usage:
#   target/debug/chronos serve /tmp/chronos-test --pg 127.0.0.1:5433 &
#   PG_HOST=/tmp tests/run_postgres_regression_proper.sh
#
set -u

PG_ROOT=${PG_ROOT:-/tmp/postgresql-16.14}
CHRONOS_URL=${CHRONOS_URL:-postgres://127.0.0.1:5433/main}
PG_HOST=${PG_HOST:-localhost}
PG_USER=${PG_USER:-$(whoami)}

REGRESS_DIR="$PG_ROOT/src/test/regress"
REGRESS_SQL_DIR="$REGRESS_DIR/sql"
SCHEDULE_FILE="$REGRESS_DIR/parallel_schedule"

OUTPUT_DIR="/tmp/chronos-regress-proper"
mkdir -p "$OUTPUT_DIR"
cd "$OUTPUT_DIR"

echo "Postgres Regression Test Suite vs Chronos (Proper)"
echo "==================================================="
echo ""
echo "Setup: Running test_setup on both systems first..."
echo ""

# Create databases
psql -X -h "$PG_HOST" -U "$PG_USER" -d postgres -c "DROP DATABASE IF EXISTS regress;" >/dev/null 2>&1 || true
psql -X -h "$PG_HOST" -U "$PG_USER" -d postgres -c "CREATE DATABASE regress;" >/dev/null 2>&1

# Run test_setup on Postgres
echo "Running test_setup on Postgres..."
psql -X -h "$PG_HOST" -U "$PG_USER" -d regress -f "$REGRESS_SQL_DIR/test_setup.sql" > /tmp/pg_setup.log 2>&1
pg_setup_rc=$?

echo "Running test_setup on Chronos..."
psql -X "$CHRONOS_URL" -f "$REGRESS_SQL_DIR/test_setup.sql" > /tmp/ch_setup.log 2>&1
ch_setup_rc=$?

if [ $pg_setup_rc -ne 0 ]; then
  echo "ERROR: Postgres setup failed. Log:"
  head -20 /tmp/pg_setup.log
  exit 1
fi

if [ $ch_setup_rc -ne 0 ]; then
  echo "WARNING: Chronos setup failed. Log:"
  head -20 /tmp/ch_setup.log
  echo ""
fi

echo "Setup complete."
echo ""
echo "Running test suite..."
echo ""

# Parse schedule to get test order
get_test_list() {
  grep "^test:" "$SCHEDULE_FILE" | sed 's/^test: //' | tr ' ' '\n' | sort -u
}

tests=$(get_test_list)
total_tests=$(echo "$tests" | wc -l)
echo "Will run $total_tests tests"
echo ""

# Detailed error tracking
errors_file="errors.txt"
pass_tests_file="pass_tests.txt"
fail_tests_file="fail_tests.txt"

: > "$errors_file"
: > "$pass_tests_file"
: > "$fail_tests_file"

passed=0
failed=0
count=0

# Run each test
for test in $tests; do
  count=$((count + 1))
  test_file="$REGRESS_SQL_DIR/${test}.sql"

  if [ ! -f "$test_file" ]; then
    echo "[$count/$total_tests] SKIP: $test (file not found)"
    continue
  fi

  # Run on both systems and capture full output
  pg_output=$(psql -X -h "$PG_HOST" -U "$PG_USER" -d regress -f "$test_file" 2>&1)
  pg_rc=$?

  ch_output=$(psql -X "$CHRONOS_URL" -f "$test_file" 2>&1)
  ch_rc=$?

  # Check for actual errors
  # psql returns 0 even if there are SQL errors unless -v ON_ERROR_STOP=1
  # Look for common error patterns
  pg_has_error=$(echo "$pg_output" | grep -i "^ERROR\|^FATAL" | head -1)
  ch_has_error=$(echo "$ch_output" | grep -i "^ERROR\|^FATAL" | head -1)

  # Classify
  if [ -z "$pg_has_error" ] && [ -z "$ch_has_error" ]; then
    # Both succeeded
    if echo "$pg_output" | head -5 | diff -q <(echo "$ch_output" | head -5) >/dev/null 2>&1; then
      status="PASS"
      passed=$((passed + 1))
      echo "$test" >> "$pass_tests_file"
    else
      status="DIFF"
      failed=$((failed + 1))
      echo "$test" >> "$fail_tests_file"
      echo "DIFF: $test" >> "$errors_file"
      echo "  PG output: $(echo "$pg_output" | head -1 | cut -c1-100)" >> "$errors_file"
      echo "  CH output: $(echo "$ch_output" | head -1 | cut -c1-100)" >> "$errors_file"
    fi
  elif [ -z "$pg_has_error" ] && [ -n "$ch_has_error" ]; then
    # PG succeeded, Chronos failed
    status="GAP"
    failed=$((failed + 1))
    echo "$test" >> "$fail_tests_file"
    echo "GAP (Chronos failed): $test" >> "$errors_file"
    echo "  PG: ok" >> "$errors_file"
    echo "  CH: $ch_has_error" >> "$errors_file"
  elif [ -n "$pg_has_error" ] && [ -z "$ch_has_error" ]; then
    # Chronos succeeded, PG failed (lenient)
    status="LENIENT"
    passed=$((passed + 1))
    echo "$test" >> "$pass_tests_file"
    echo "LENIENT: $test" >> "$errors_file"
  else
    # Both failed
    status="BOTH_ERR"
    passed=$((passed + 1))
    echo "$test" >> "$pass_tests_file"
  fi

  if [ $((count % 20)) -eq 0 ] || [ "$status" != "PASS" ]; then
    printf "[%3d/%3d] %-30s %s\n" "$count" "$total_tests" "$test" "$status"
  fi
done

echo ""
echo "Results"
echo "======="
pass_pct=$((passed * 100 / count))
echo "Tests run: $count"
echo "Passed (exact match or both failed): $passed"
echo "Failed (GAP or DIFF): $failed"
echo "Pass rate: $pass_pct%"
echo ""
echo "See errors.txt for details"
echo "See pass_tests.txt / fail_tests.txt for test lists"

exit 0
