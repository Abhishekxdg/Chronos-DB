#!/bin/bash
# pg_regress-style run of Postgres's own regression files against Chronos.
# Each test runs on a fresh copy of a test_setup template and is diffed against expected/<test>.out.
#
# Usage: tests/regress_expected.sh [test ...]      (no args: every test in parallel_schedule)
#   PG_ROOT=/tmp/postgresql-16.14  BIN=target/release/chronos  OUT=/tmp/chronos-regress
set -u
PG_ROOT=${PG_ROOT:-/tmp/postgresql-16.14}
BIN=${BIN:-$(pwd)/target/release/chronos}
OUT=${OUT:-/tmp/chronos-regress}
PORT=${PORT:-5439}
HPORT=${HPORT:-7079}
R="$PG_ROOT/src/test/regress"
# (the files run in a database named regression, as pg_regress's: psql's patterns, information_schema's *_catalog
# columns and the privilege messages name it; here it is a world of that name, made in the template)
MAIN_URL="postgres://127.0.0.1:$PORT/main"
URL="postgres://127.0.0.1:$PORT/regression"
mkdir -p "$OUT/results" "$OUT/diffs"
rm -f "$OUT/summary.tsv"

# as pg_regress sets them for the expected files
export PGTZ=America/Los_Angeles PGDATESTYLE="Postgres, MDY" PGOPTIONS="${PGOPTIONS:-} -c intervalstyle=postgres_verbose"
# and the paths the files read with \getenv, as pg_regress exports them (the C library's for the functions of it here)
export PG_ABS_SRCDIR="$R" PG_ABS_BUILDDIR="$OUT" PG_LIBDIR="$R" PG_DLSUFFIX=.so
PSQL_VARS=(-v "abs_srcdir=$R" -v "abs_builddir=$OUT" -v "libdir=$R" -v "regresslib=$R/regress" -v HIDE_TABLEAM=on -v HIDE_TOAST_COMPRESSION=on)

prep() { cat "$1"; }

# (CHRONOS_PROTECT_MAIN=off: the files write main directly, which a folder `chronos serve` makes refuses by default)
start() { CHRONOS_PROTECT_MAIN=off CHRONOS_PG_EXACT=1 CHRONOS_ALLOW_SERVER_COPY=1 CHRONOS_TSEARCH_DATA="$PG_ROOT/src/backend/tsearch/dicts:$PG_ROOT/src/backend/snowball/stopwords" "$BIN" serve "$1" --pg 127.0.0.1:$PORT --listen 127.0.0.1:$HPORT >"$OUT/server.log" 2>&1 & SRV=$!; for _ in $(seq 40); do psql -X "$MAIN_URL" -c 'select 1' >/dev/null 2>&1 && return 0; sleep 0.25; done; return 1; }
stop() { kill "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; }
runsql() { # file, output
  prep "$1" >"$OUT/cur.sql"
  psql -X -a -q "${PSQL_VARS[@]}" "$URL" <"$OUT/cur.sql" >"$2" 2>&1 &
  local p=$!
  (sleep 60; kill "$p" 2>/dev/null) & local w=$!
  wait "$p" 2>/dev/null; kill "$w" 2>/dev/null; wait "$w" 2>/dev/null
}

# tests whose tables come from earlier tests of the schedule: those run first, in the same database, unscored
listed_deps() {
  case "$1" in
    geometry) echo "point lseg line box path polygon circle" ;;
    horology) echo "date time timetz timestamp timestamptz interval" ;;
    select_views) echo "create_view" ;;
    # (parallel_schedule: "aggregates depends on create_aggregate", its newavg, newsum, newcnt ...)
    aggregates) echo "create_aggregate" ;;
    # a_star and its children (b_star ... f_star) are create_misc's
    select_parallel|join) echo "create_misc" ;;
    # (alter_table renames create_index's indexes (onek_unique1) and attaches create_table's partitions (hpart_4);
    # create_am reads create_index's fast_emp4000)
    alter_table) echo "create_table create_index" ;;
    create_am) echo "create_index" ;;
    # (parallel_schedule: "psql depends on create_am", "amutils depends on geometry, create_index_spgist, hash_index,
    # brin", "with depends on create_misc"; deps are flat, so each one's own prerequisites are listed too)
    psql) echo "create_index create_am" ;;
    amutils) echo "point lseg line box path polygon circle geometry create_index create_index_spgist hash_index brin" ;;
    with) echo "create_misc" ;;
  esac
}

# the line of parallel_schedule a test is on
at() { grep -n "^test:.* $1\( \|\$\)" "$R/parallel_schedule" | head -1 | cut -d: -f1; }
CREATE_INDEX_AT=$(at create_index)
# a test's prerequisites, in schedule order: those listed above, and create_index for every test of a later group
# (parallel_schedule: "many of the tests in later groups depend on create_index": they read and EXPLAIN its indexes)
deps() {
  local own here
  own=$(listed_deps "$1")
  here=$(at "$1")
  if [ -n "$here" ] && [ "$here" -gt "$CREATE_INDEX_AT" ] && ! grep -qw create_index <<<"$own"; then
    own="$own create_index"
  fi
  for d in $own; do echo "$(at "$d") $d"; done | sort -s -n -k1,1 | awk '{print $2}' | tr '\n' ' '
}

pkill -f "chronos serve $OUT" 2>/dev/null
rm -rf "$OUT/template"
start "$OUT/template" || { echo "server failed to start"; exit 1; }
psql -X -q "$MAIN_URL" -c 'create database regression' || { echo "couldn't make the regression database"; exit 1; }
runsql "$R/sql/test_setup.sql" "$OUT/results/test_setup.out"
stop

if [ $# -gt 0 ]; then tests="$*"; else tests=$(grep "^test:" "$R/parallel_schedule" | sed 's/^test: //' | tr ' ' '\n' | grep -v '^$' | grep -vx test_setup); fi

exact=0; n=0; sumpct=0; all=0; allmissed=0
for t in $tests; do
  [ -f "$R/sql/$t.sql" ] || continue
  [ -f "$R/expected/$t.out" ] || continue
  n=$((n+1))
  rm -rf "$OUT/db"; cp -R "$OUT/template" "$OUT/db"
  if start "$OUT/db"; then
    for d in $(deps "$t"); do [ -f "$R/sql/$d.sql" ] && runsql "$R/sql/$d.sql" "$OUT/results/dep_$d.out"; done
    runsql "$R/sql/$t.sql" "$OUT/results/$t.out"
  else echo "start failed" >"$OUT/results/$t.out"; fi
  stop
  # (as pg_regress: the expected file, or one of its variants (`t_1.out`, `t_2.out`: another platform's or build's
  # output, a test skipped where its locales are missing), whichever differs least)
  best=""
  for e in "$R/expected/$t.out" "$R/expected/${t}"_[0-9].out; do
    [ -f "$e" ] || continue
    diff -U0 "$e" "$OUT/results/$t.out" >"$OUT/cur.diff"
    # (the diff's first two lines name the files; every other line starting with - or + is an expected line missed
    # or a line added, a blank one, a `----` separator or an echoed `--` comment too)
    tot=$(wc -l <"$e"); mi=$(tail -n +3 "$OUT/cur.diff" | grep -c '^-'); pl=$(tail -n +3 "$OUT/cur.diff" | grep -c '^+')
    b=$((mi>pl?mi:pl))
    [ "$tot" -gt 0 ] || tot=1
    p=$(( (tot - b) * 100 / tot )); [ $p -lt 0 ] && p=0
    if [ -z "$best" ] || [ ! -s "$OUT/cur.diff" ] || [ "$p" -gt "$pct" ]; then
      best=$e; pct=$p; lines=$tot; missed=$mi; cp "$OUT/cur.diff" "$OUT/diffs/$t.diff"
      [ -s "$OUT/cur.diff" ] || break
    fi
  done
  [ ! -s "$OUT/diffs/$t.diff" ] && { exact=$((exact+1)); st=PASS; } || st=FAIL
  sumpct=$((sumpct + pct)); all=$((all + lines)); allmissed=$((allmissed + missed))
  printf "%s\t%s\t%d\t%d\t%d\n" "$t" "$st" "$pct" "$lines" "$missed" >>"$OUT/summary.tsv"
  printf "%-28s %s %3d%%\n" "$t" "$st" "$pct"
done
# (the suite's figure: of the lines of the expected files the tests were compared with, the share the output has)
echo "exact passes: $exact / $n   mean line match: $((sumpct / (n>0?n:1)))%   expected lines matched:" \
  "$(awk -v a="$all" -v m="$allmissed" 'BEGIN { printf "%.2f%% (%d of %d)", a ? 100 * (a - m) / a : 0, a - m, a }')"
