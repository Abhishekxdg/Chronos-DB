#!/bin/bash
# The Postgres regression run (tests/regress_expected.sh) against the floor it has reached, tests/regress_floor.tsv:
# no file may match fewer of its expected lines than the floor says (less SLACK points, for a psql or a Postgres
# of another minor version), nor may fewer files match exactly. A run that does better says which lines of the
# floor to raise; `--write` raises them.
#
#   OUT=/tmp/chronos-regress tests/regress_ratchet.sh [--write]
set -u
OUT=${OUT:-/tmp/chronos-regress}
FLOOR=${FLOOR:-$(dirname "$0")/regress_floor.tsv}
SLACK=${SLACK:-2}
[ -s "$OUT/summary.tsv" ] || { echo "no $OUT/summary.tsv: run tests/regress_expected.sh first"; exit 2; }
if [ "${1:-}" = "--write" ]; then
  sort "$OUT/summary.tsv" >"$FLOOR"
  echo "floor written: $(grep -c PASS "$FLOOR") exact of $(wc -l <"$FLOOR" | tr -d ' ')"
  exit 0
fi
[ -s "$FLOOR" ] || { echo "no floor at $FLOOR: write one with --write"; exit 2; }
awk -F'\t' -v slack="$SLACK" '
  NR == FNR { floor[$1] = $3; was[$1] = $2; next }
  {
    seen[$1] = 1
    if (!($1 in floor)) { print "new:    " $1 " " $2 " " $3 "%"; next }
    if ($3 + slack < floor[$1]) { print "worse:  " $1 " " floor[$1] "% -> " $3 "%"; bad++ }
    else if ($3 > floor[$1]) { print "better: " $1 " " floor[$1] "% -> " $3 "%"; better++ }
    if (was[$1] == "PASS") { exact_was++; if ($2 != "PASS") { print "no longer exact: " $1; bad++ } }
    if ($2 == "PASS") exact++
  }
  END {
    for (t in floor) if (!(t in seen)) { print "missing: " t; bad++ }
    printf "exact: %d (floor %d)   worse: %d   better: %d\n", exact, exact_was, bad, better
    if (better && !bad) print "raise the floor: OUT=... tests/regress_ratchet.sh --write, and commit tests/regress_floor.tsv"
    exit bad ? 1 : 0
  }' "$FLOOR" "$OUT/summary.tsv"
