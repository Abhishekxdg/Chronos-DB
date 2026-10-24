#!/usr/bin/env python3
"""Per-statement pass rate of a run of tests/regress_expected.sh: how many of the regression files' statements give
exactly Postgres's output, beside the run's line figure (which counts the SQL psql -a echoes too) and its count of
files that match exactly.

    python3 tests/regress_statements.py <run dir> <regress dir> [--files]

<run dir> holds the run's results/<test>.out (and diffs/<test>.diff, whose header names the expected file chosen);
<regress dir> is Postgres's src/test/regress (expected/, sql/, parallel_schedule).

A statement is one SQL statement or psql meta-command of a test's .sql file. Each is found in the expected output
and in the result by the input lines psql -a echoes (COPY data and \\set ECHO none regions aren't echoed, so they
are skipped, not matched), split as psql splits its input (quotes, dollar quotes, comments, parentheses, BEGIN ATOMIC
bodies, \\g and its kind). A statement's block is its echoed lines and the output after them, up to the next
statement; statements sharing a line (`BEGIN; LOCK TABLE t; COMMIT;`) share its block, which psql's output can't split,
and each counts with the block's verdict. It passes when the two blocks are identical. It is a partial pass (Doltgres's published rate counts these
too) when both fail with an error and only the messages differ (the error's text, LINE, caret, DETAIL, HINT and
CONTEXT, and any NOTICE or WARNING), or when only a table's column names differ (same columns, same rows).
"""
import bisect
import os
import re
import sys

ERRISH = re.compile(
    r"^(ERROR|DETAIL|HINT|CONTEXT|LINE \d+|WARNING|NOTICE|QUERY|PL/pgSQL|SQL (function|statement))\b|^\s*\^\s*$|^\s+$"
)
SEP = re.compile(r"^-+(\+-+)*$")


def read(p):
    with open(p, encoding="utf-8", errors="replace") as f:
        return f.read().split("\n")


def tests(regress):
    out = []
    for line in read(os.path.join(regress, "parallel_schedule")):
        if line.startswith("test:"):
            out += [t for t in line[5:].split() if t != "test_setup"]
    return out


def echoed_input(sql):
    """The .sql's non-blank lines psql -a echoes: not COPY ... FROM stdin data, nor what \\set ECHO none hides."""
    out, in_copy, echo, pending_copy = [], False, True, False
    for line in sql:
        s = line.strip()
        if in_copy:
            if s == "\\.":
                in_copy = False
            continue
        m = re.match(r"\\set\s+ECHO\s+(\S+)", s)
        if m:
            was = echo
            echo = m.group(1).lower() not in ("none", "errors", "queries")
            if was and s:
                out.append(line)
            continue
        if echo and s:
            out.append(line)
        low = re.sub(r"--.*$", "", line).lower()
        if re.search(r"\bfrom\s+stdin\b", low) or pending_copy and re.search(r"\bstdin\b", low):
            if low.rstrip().endswith(";") or s.startswith("\\copy"):
                in_copy, pending_copy = True, False
            else:
                pending_copy = True
        elif pending_copy and low.rstrip().endswith(";"):
            pending_copy = False
        if re.match(r"^\s*copy\b", low) and "stdin" not in low and not low.rstrip().endswith(";"):
            pending_copy = True
    return out


def place(want, lines):
    """Where each echoed input line is in an output file, in order: (line index, input line number). An input line
    not found is skipped, keeping the position (psql didn't echo it, or the run stopped early)."""
    index = {}
    for i, l in enumerate(lines):
        index.setdefault(l, []).append(i)
    pos, out = 0, []
    for k, w in enumerate(want):
        at = index.get(w, [])
        j = bisect.bisect_left(at, pos)
        if j < len(at):
            out.append((at[j], k))
            pos = at[j] + 1
    return out


def starts(echoed, lines):
    """Where statements start, as psql splits its input: (line index, input line number, how many start on that
    line)."""
    out, empty = [], True
    sq = dq = esc = False
    dol, block, paren, depth, words = None, 0, 0, 0, []
    for i, k in echoed:
        line, j, n = lines[i], 0, len(lines[i])
        while j < n:
            c = line[j]
            if sq:
                if esc and c == "\\":
                    j += 2
                    continue
                if c == "'":
                    if j + 1 < n and line[j + 1] == "'":
                        j += 2
                        continue
                    sq = False
                j += 1
                continue
            if dq:
                dq = c != '"'
                j += 1
                continue
            if dol is not None:
                if line.startswith(dol, j):
                    j, dol = j + len(dol), None
                else:
                    j += 1
                continue
            if block:
                if line.startswith("*/", j):
                    block, j = block - 1, j + 2
                elif line.startswith("/*", j):
                    block, j = block + 1, j + 2
                else:
                    j += 1
                continue
            if line.startswith("--", j):
                break
            if c.isspace():
                j += 1
                continue
            if empty and c != ";":
                if out and out[-1][0] == i:
                    out[-1] = (i, k, out[-1][2] + 1)
                else:
                    out.append((i, k, 1))
                empty, words, depth, paren = False, [], 0, 0
            if c == "\\":
                empty = True  # a meta-command, \g and its kind included, ends what came before it
                break
            if line.startswith("/*", j):
                block, j = 1, j + 2
                continue
            if c == "'":
                esc = j > 0 and line[j - 1] in "eE" and (j < 2 or not (line[j - 2].isalnum() or line[j - 2] == "_"))
                sq, j = True, j + 1
                continue
            if c == '"':
                dq, j = True, j + 1
                continue
            m = re.match(r"\$([A-Za-z_][A-Za-z_0-9]*)?\$", line[j:])
            if m and not (j > 0 and (line[j - 1].isalnum() or line[j - 1] == "_")):
                dol, j = m.group(0), j + len(m.group(0))
                continue
            if c == "(":
                paren, j = paren + 1, j + 1
                continue
            if c == ")":
                paren, j = max(0, paren - 1), j + 1
                continue
            if c == ";":
                if paren == 0 and depth == 0:
                    empty, words = True, []
                j += 1
                continue
            m = re.match(r"[A-Za-z_][A-Za-z_0-9$]*", line[j:])
            if m:
                w = m.group(0).lower()
                words.append(w)
                # (a function's BEGIN ATOMIC body: its own ';'s don't end the statement)
                body = words[0] == "create" and any(x in ("function", "procedure") for x in words[:6])
                if body and w in ("begin", "case"):
                    depth += 1
                elif body and w == "end" and depth > 0:
                    depth -= 1
                j += len(m.group(0))
                continue
            j += 1
    return out


def blocks(lines, sql):
    """Each block, by its first input line's number: its lines, and how many statements start on its first line."""
    st = starts(place(echoed_input(sql), lines), lines)
    out = {}
    for n, (s, k, count) in enumerate(st):
        e = (st[n + 1][0] - 1) if n + 1 < len(st) else len(lines) - 1
        while e > s and lines[e] == "":
            e -= 1
        out[k] = (lines[s : e + 1], count)
    return out


def judge(exp, res):
    """strict, partial or fail."""
    if res is None:
        return "fail"
    if exp == res:
        return "strict"
    if any(x.startswith("ERROR") for x in exp) and any(x.startswith("ERROR") for x in res):
        if [x for x in exp if not ERRISH.match(x)] == [x for x in res if not ERRISH.match(x)]:
            return "partial"
    if len(exp) == len(res):
        header = set()
        for i in range(1, len(exp)):
            if SEP.match(exp[i]) and "-" in exp[i]:
                if SEP.match(res[i]) and res[i].count("+") == exp[i].count("+"):
                    header.update({i - 1, i})
        if all(i in header for i in range(len(exp)) if exp[i] != res[i]):
            return "partial"
    return "fail"


def expected_file(run, regress, t, res):
    """The expected file the run compared with: the diff's, or for a file with no diff the one (or the variant) the
    result equals."""
    diff = os.path.join(run, "diffs", t + ".diff")
    if os.path.exists(diff) and os.path.getsize(diff) > 0:
        first = read(diff)[0]
        if first.startswith("--- "):
            return os.path.join(regress, "expected", os.path.basename(first[4:].split("\t")[0]))
    cands = sorted(
        os.path.join(regress, "expected", f)
        for f in os.listdir(os.path.join(regress, "expected"))
        if f == t + ".out" or re.fullmatch(re.escape(t) + r"_\d\.out", f)
    )
    return next((c for c in cands if read(c) == res), cands[0] if cands else None)


def main():
    run, regress = sys.argv[1], sys.argv[2]
    totals = {"strict": 0, "partial": 0, "fail": 0}
    rows = []
    for t in tests(regress):
        sql_p, res_p = os.path.join(regress, "sql", t + ".sql"), os.path.join(run, "results", t + ".out")
        if not (os.path.exists(sql_p) and os.path.exists(res_p)):
            continue
        sql, res = read(sql_p), read(res_p)
        exp_p = expected_file(run, regress, t, res)
        if not exp_p or not os.path.exists(exp_p):
            continue
        eb, rb = blocks(read(exp_p), sql), blocks(res, sql)
        got = {"strict": 0, "partial": 0, "fail": 0}
        for k, (block, count) in eb.items():
            got[judge(block, rb.get(k, (None, 0))[0])] += count
        for c in got:
            totals[c] += got[c]
        rows.append((t, sum(got.values()), got["strict"], got["partial"]))
    if "--files" in sys.argv:
        for t, n, s, p in sorted(rows, key=lambda r: r[2] / max(r[1], 1)):
            print(f"{t}\t{n}\t{s}\t{p}\t{100 * s / max(n, 1):.1f}%")
    n = sum(totals.values())
    s, p = totals["strict"], totals["partial"]
    print(
        f"files {len(rows)}  statements {n}  strict {s} ({100 * s / n:.2f}%)  "
        f"with partial {s + p} ({100 * (s + p) / n:.2f}%)"
    )


if __name__ == "__main__":
    main()
