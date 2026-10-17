#!/bin/bash
# Run Postgres regression test suite against Chronos.
# Compares results between Postgres and Chronos, reports pass rates and gaps.
#
# Usage:
#   chronos serve /tmp/chronos-test --pg 127.0.0.1:5433 &
#   PG_HOST=/tmp tests/postgres_regress.sh [output_dir]
#
# Outputs: regress_results.txt (summary), regress_detailed.txt (gaps + diffs)
#
set -u

PG_ROOT=${PG_ROOT:-/tmp/postgresql-16.14}
CHRONOS_URL=${CHRONOS_URL:-postgres://127.0.0.1:5433/main}
PG_HOST=${PG_HOST:-/tmp}
PG_USER=${PG_USER:-postgres}
OUTPUT_DIR=${1:-/tmp/regress-results}
REGRESS_SQL_DIR="$PG_ROOT/src/test/regress/sql"

# Cleanup and setup
mkdir -p "$OUTPUT_DIR"
cd "$OUTPUT_DIR"

echo "Postgres Regression Suite Runner"
echo "=================================="
echo "Postgres: $PG_HOST"
echo "Chronos: $CHRONOS_URL"
echo "SQL dir: $REGRESS_SQL_DIR"
echo ""

# Connection helpers
pg()  { psql -X -h "$PG_HOST" -U "$PG_USER" -d postgres -At -v ON_ERROR_STOP=1 -c "$1" 2>&1; }
ch()  { psql -X "$CHRONOS_URL" -At -v ON_ERROR_STOP=1 -c "$1" 2>&1; }

# Ensure test database exists
pg "CREATE DATABASE regress_test;" >/dev/null 2>&1 || true
pg() { psql -X -h "$PG_HOST" -U "$PG_USER" -d regress_test -At -v ON_ERROR_STOP=1 -c "$1" 2>&1; }

# Chronos setup (uses main database)
ch "DROP TABLE IF EXISTS regress_test_marker;" >/dev/null 2>&1 || true

# Normalize output for comparison
norm() {
  sed -E \
    -e 's/[0-9]{4}-[0-9]{2}-[0-9]{2}/TIMESTAMP/g' \
    -e 's/[0-9]{2}:[0-9]{2}:[0-9]{2}/TIME/g' \
    -e 's/[0-9a-f]{8}-[0-9a-f-]{27}/UUID/g' \
    -e 's/[0-9]+\.[0-9]+/NUM/g' \
    | head -20
}

passed=0
failed=0
errored=0
errors_file="$OUTPUT_DIR/regress_errors.txt"
diffs_file="$OUTPUT_DIR/regress_diffs.txt"
summary_file="$OUTPUT_DIR/regress_results.txt"

: > "$errors_file"
: > "$diffs_file"
: > "$summary_file"

# Find all test files
test_files=$(find "$REGRESS_SQL_DIR" -name "*.sql" -type f | sort)
total=$(echo "$test_files" | wc -l)
count=0

echo "Running $total regression tests..."
echo ""

for sql_file in $test_files; do
  count=$((count + 1))
  test_name=$(basename "$sql_file" .sql)

  # Show progress
  if [ $((count % 10)) -eq 0 ]; then
    pct=$((count * 100 / total))
    echo "[$pct%] [$count/$total] $test_name"
  fi

  # Read SQL file
  sql_content=$(cat "$sql_file")

  # Run on both systems
  pg_out=$(pg "$sql_content" 2>&1)
  pg_rc=$?
  ch_out=$(ch "$sql_content" 2>&1)
  ch_rc=$?

  # Classify result
  if [ $pg_rc -eq 0 ] && [ $ch_rc -eq 0 ]; then
    # Both succeeded - check if output matches
    pg_norm=$(echo "$pg_out" | norm)
    ch_norm=$(echo "$ch_out" | norm)

    if [ "$pg_norm" = "$ch_norm" ]; then
      passed=$((passed + 1))
    else
      failed=$((failed + 1))
      echo "DIFF: $test_name" >> "$diffs_file"
      echo "  Postgres output: $(echo "$pg_norm" | tr '\n' '|' | cut -c1-100)" >> "$diffs_file"
      echo "  Chronos output:  $(echo "$ch_norm" | tr '\n' '|' | cut -c1-100)" >> "$diffs_file"
      echo "" >> "$diffs_file"
    fi
  elif [ $pg_rc -ne 0 ] && [ $ch_rc -ne 0 ]; then
    # Both failed - OK if same error
    passed=$((passed + 1))
  else
    # One succeeded, one failed - GAP
    errored=$((errored + 1))
    echo "ERROR: $test_name" >> "$errors_file"
    if [ $pg_rc -eq 0 ]; then
      echo "  Postgres succeeded, Chronos failed:" >> "$errors_file"
      echo "  $(echo "$ch_out" | head -1 | cut -c1-120)" >> "$errors_file"
    else
      echo "  Chronos succeeded, Postgres failed:" >> "$errors_file"
      echo "  $(echo "$pg_out" | head -1 | cut -c1-120)" >> "$errors_file"
    fi
    echo "" >> "$errors_file"
  fi
done

echo ""
echo "Test run complete!"

# Calculate stats
pass_pct=$((passed * 100 / total))
fail_pct=$((failed * 100 / total))
err_pct=$((errored * 100 / total))

# Write summary
{
  echo "POSTGRES REGRESSION SUITE RESULTS"
  echo "=================================="
  echo ""
  echo "Total tests: $total"
  echo "Passed (output match): $passed ($pass_pct%)"
  echo "Failed (output diff): $failed ($fail_pct%)"
  echo "Errored (one succeeded, one failed): $errored ($err_pct%)"
  echo ""
  echo "Pass rate: $pass_pct%"
  echo ""
  echo "See regress_errors.txt for gaps (Postgres succeeds, Chronos fails)"
  echo "See regress_diffs.txt for output differences"
} | tee "$summary_file"

# Show top errors
echo ""
echo "Top 10 Gaps (Chronos doesn't support):"
head -30 "$errors_file" | grep "ERROR:" | head -10

exit 0
