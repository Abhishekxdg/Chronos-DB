#!/bin/bash
# Measures Postgres compatibility against a real Postgres: runs every statement in
# tests/fixtures/pg-gap-probe.sql (one per line, in order) on both servers and reports
#   GAP      Postgres accepts it, Chronos errors        <- the number to drive down
#   LENIENT  Chronos accepts what Postgres refuses
#   DIFF     both accept, results differ (statements that depend on earlier GAPs cascade)
#
#   chronos serve /some/empty/dir --pg 127.0.0.1:5433 &     # a fresh database
#   tests/pg_gap_probe.sh                                    # Postgres at host=/tmp
#
# Env: CHRONOS_URL (postgres://127.0.0.1:5433/main), PG_HOST (/tmp), PG_USER (postgres).
# It drops and recreates the database `probe` on the Postgres side.
set -u
FILE=${1:-$(dirname "$0")/fixtures/pg-gap-probe.sql}
CHRONOS_URL=${CHRONOS_URL:-postgres://127.0.0.1:5433/main}
PG_HOST=${PG_HOST:-/tmp}
PG_USER=${PG_USER:-postgres}
pg()  { psql -X -h "$PG_HOST" -U "$PG_USER" -d probe -At -v ON_ERROR_STOP=1 -c "$1" 2>&1; }
ch()  { psql -X "$CHRONOS_URL" -At -v ON_ERROR_STOP=1 -c "$1" 2>&1; }
psql -X -h "$PG_HOST" -U "$PG_USER" -d postgres -Atqc "drop database if exists probe" >/dev/null 2>&1
psql -X -h "$PG_HOST" -U "$PG_USER" -d postgres -Atqc "create database probe" >/dev/null 2>&1
norm() { sed -E 's/[0-9a-f]{8}-[0-9a-f-]{27}/UUID/g' | head -3; }
n=0; gap=0; len=0; dif=0
while IFS= read -r stmt; do
  n=$((n+1)); [ -z "$stmt" ] && continue
  pgout=$(pg "$stmt"); pgrc=$?
  chout=$(ch "$stmt"); chrc=$?
  if [ $pgrc -eq 0 ] && [ $chrc -ne 0 ]; then
    gap=$((gap+1)); echo "GAP     [$n] $stmt"; echo "        chronos: $(echo "$chout" | head -1 | cut -c1-140)"
  elif [ $pgrc -ne 0 ] && [ $chrc -eq 0 ]; then
    len=$((len+1)); echo "LENIENT [$n] $stmt"; echo "        postgres: $(echo "$pgout" | head -1 | cut -c1-140)"
  elif [ $pgrc -eq 0 ]; then
    a=$(echo "$pgout" | norm); b=$(echo "$chout" | norm)
    if [ "$a" != "$b" ]; then
      dif=$((dif+1)); echo "DIFF    [$n] $stmt"
      echo "        postgres: $(echo "$a" | tr '\n' '|' | cut -c1-120)"; echo "        chronos:  $(echo "$b" | tr '\n' '|' | cut -c1-120)"
    fi
  fi
done < "$FILE"
echo "statements: $n   GAP: $gap   LENIENT: $len   DIFF: $dif"
