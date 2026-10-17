#!/usr/bin/env python3
"""Ranks what breaks tests/regress_expected.sh's run, by how much each cause costs.

    python3 tests/regress_triage.py [--out /tmp/chronos-regress] [--top 40] [--test name]

Reads the run's results and diffs. An error Chronos gives where Postgres gave none is a cause. A
statement that then fails because the first one did (the table it made isn't there, the transaction
it was in is aborted) is charged to that cause, so one refused CREATE that fails fifty statements
after it ranks above fifty unrelated errors. Syntax errors keep the token they stopped at; other
messages have their names and numbers taken out, so the same gap in many files is one line here.
"""
import argparse, collections, os, re

ap = argparse.ArgumentParser()
ap.add_argument("--out", default=os.environ.get("OUT", "/tmp/chronos-regress"))
ap.add_argument("--top", type=int, default=40)
ap.add_argument("--test", help="only this test, and list each of its causes with its statement")
args = ap.parse_args()

HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
CREATE = re.compile(
    r"^\s*create\s+(?:or\s+replace\s+)?(?:(?:global|local|temp|temporary|unlogged|unique|materialized|recursive|"
    r"constraint|foreign|default)\s+)*(table|view|type|function|procedure|index|sequence|domain|aggregate|"
    r"operator|schema|role|user|group|trigger|rule|collation|cast|policy|statistics|extension|database|"
    r"tablespace|publication|subscription|server|language|conversion|access\s+method|text\s+search\s+\w+|"
    r"event\s+trigger|operator\s+class|operator\s+family)\s+(?:concurrently\s+)?(?:if\s+not\s+exists\s+)?"
    r"((?:\"[^\"]+\"|[\w$]+)(?:\.(?:\"[^\"]+\"|[\w$]+))?)", re.I)
INTO = re.compile(r"\binto\s+(?:temp(?:orary)?\s+|unlogged\s+)?(?:table\s+)?([\w$\"]+)\s+from\b", re.I)
MISSING = re.compile(r'(?:relation|table|type|view|sequence|schema|role|index|function|operator|collation|'
                     r'domain|trigger|rule|policy|cursor|prepared statement|server|language|access method|'
                     r'extension|publication|aggregate|procedure|text search \w+|statistics object|column) '
                     r'"?([^"(\s]+)"?.* does not exist')
OUTPUT = re.compile(r"^(ERROR|NOTICE|WARNING|DETAIL|HINT|LINE \d+|CONTEXT|INFO|LOG|QUERY|\s*\^|-[-+]+|\(\d+ rows?\))")


def bare(name):
    return name.split(".")[-1].strip('"').lower()


def key(msg):
    """The message as a cause: a syntax error keeps its token, anything else loses names and numbers."""
    if "syntax error" in msg:
        return msg
    msg = re.sub(r'"[^"]*"', '"…"', msg)
    msg = re.sub(r"'[^']*'", "'…'", msg)
    return re.sub(r"\b\d+\b", "N", msg)


def statement(lines, at):
    """The statement psql echoed before line `at` (0-based) of the results: back to the output before it."""
    got = []
    i = at - 1
    while i >= 0 and len(got) < 40:
        ln = lines[i]
        if not ln.strip() or OUTPUT.match(ln):
            break
        if got and ln.rstrip().endswith(";"):
            break
        got.append(ln)
        i -= 1
    return " ".join(x.strip() for x in reversed(got))


causes = collections.defaultdict(lambda: {"direct": 0, "cascade": 0, "tests": collections.Counter(), "eg": None})
per_test = []
names = sorted(f[:-5] for f in os.listdir(os.path.join(args.out, "diffs")) if f.endswith(".diff"))
for t in names:
    if args.test and t != args.test:
        continue
    diff = open(os.path.join(args.out, "diffs", t + ".diff"), errors="replace").read().splitlines()
    try:
        res = open(os.path.join(args.out, "results", t + ".out"), errors="replace").read().splitlines()
    except FileNotFoundError:
        continue
    failed, last, i = {}, None, 0
    while i < len(diff):
        m = HUNK.match(diff[i])
        if not m:
            i += 1
            continue
        at = int(m.group(3))  # the first '+' line's number in the results (1-based)
        i += 1
        minus, plus = [], []
        while i < len(diff) and not diff[i].startswith("@@"):
            (minus if diff[i].startswith("-") else plus).append(diff[i][1:])
            i += 1
        theirs = {ln.split(":", 1)[1].strip() for ln in minus if ln.startswith("ERROR:")}
        for off, ln in enumerate(plus):
            if not ln.startswith("ERROR:"):
                continue
            msg = ln.split(":", 1)[1].strip()
            if msg in theirs:
                continue  # Postgres fails here too, with the same words
            stmt = statement(res, at - 1 + off)
            made = CREATE.match(stmt)
            made = bare(made.group(2)) if made else (bare(INTO.search(stmt).group(1)) if INTO.search(stmt) else None)
            miss = MISSING.search(msg)
            if "current transaction is aborted" in msg and last:
                root, how = last, "cascade"
            elif miss and bare(miss.group(1)) in failed:
                root, how = failed[bare(miss.group(1))], "cascade"
            else:
                root, how = key(msg), "direct"
                last = root
                if causes[root]["eg"] is None:
                    causes[root]["eg"] = (t, stmt[:150])
            if made:
                failed.setdefault(made, root)
            causes[root][how] += 1
            causes[root]["tests"][t] += 1
            if args.test:
                per_test.append((how, root, stmt[:200]))

if args.test:
    for how, root, stmt in per_test:
        print(f"{how:8} {root}\n         {stmt}")
total = sum(c["direct"] + c["cascade"] for c in causes.values())
print(f"{total} statements fail on Chronos alone, from {len(causes)} causes\n")
print(f"{'total':>6} {'own':>5} {'after':>5} {'files':>5}  cause")
for root, c in sorted(causes.items(), key=lambda kv: -(kv[1]["direct"] + kv[1]["cascade"]))[: args.top]:
    t, stmt = c["eg"] or ("", "")
    most = ", ".join(f"{n} {k}" for n, k in ((v, k) for k, v in c["tests"].most_common(3)))
    print(f"{c['direct'] + c['cascade']:6} {c['direct']:5} {c['cascade']:5} {len(c['tests']):5}  {root[:110]}")
    print(f"{'':25}{most}\n{'':25}e.g. {t}: {stmt}")
