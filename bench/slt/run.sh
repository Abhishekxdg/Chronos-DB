#!/bin/bash
# run.sh <chronos|postgres> <out dir> <test files...>: runs examples/slt on each file, on a fresh
# `chronos serve` per file (a crash or hang costs one file) or on the reference Postgres at host=/tmp,
# in a database of its own (slt<N>, made if missing). N (default 0) numbers parallel runs: each has
# its own ports and database. Per-file counts go to <out>/counts (file records ok failed skipped),
# failures to <out>/failures.
set -u
E=$1 O=$2; shift 2
N=${N:-0}
D=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$O"; touch "$O/counts" "$O/failures"
[ "$E" = postgres ] && { psql -X -q -h /tmp -d postgres -tAc "select 1 from pg_database where datname = 'slt$N'" | grep -q 1 || createdb -h /tmp "slt$N"; }
for f in "$@"; do
  if [ "$E" = chronos ]; then
    db=$(mktemp -d); CHRONOS_PROTECT_MAIN=off "$D/chronos" serve "$db/db" --listen 127.0.0.1:$((17391 + N)) --pg 127.0.0.1:$((15391 + N)) > "$O/serve.log" 2>&1 & pid=$!
    for _ in $(seq 100); do (exec 3<>/dev/tcp/127.0.0.1/$((15391 + N))) 2>/dev/null && break; sleep 0.1; done
    conn="host=127.0.0.1 port=$((15391 + N)) dbname=main user=slt"
  else
    conn="host=/tmp dbname=slt$N user=$USER"
  fi
  timeout 1800 "$D/slt" "$conn" "$f" >> "$O/counts" 2>> "$O/failures" || echo "$f exit $?" >> "$O/failures"
  if [ "$E" = chronos ]; then kill $pid 2>/dev/null; wait $pid 2>/dev/null; rm -rf "$db"; fi
done
awk '{r+=$2; o+=$3; f+=$4; s+=$5} END {printf "%d files, %d records run, %d passed (%.2f%%), %d failed, %d skipped for postgresql\n", NR, r, o, 100*o/r, f, s}' "$O/counts"
