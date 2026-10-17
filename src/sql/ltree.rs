//! ltree: a path of labels, `top.science.astronomy`, kept as that text; and lquery, a pattern over paths
//! (`top.*{1,2}.astro*@`), kept as its text too.

use super::funcs::Scalar;
use super::{SqlError, Type, Value, err};

type Result<T> = std::result::Result<T, SqlError>;

/// The types' ids and their arrays': Postgres numbers an extension's types as they are made; ours are fixed, in
/// the range it leaves to user types.
pub(crate) const OID: u32 = 16003;
pub(crate) const LQUERY_OID: u32 = 16004;
pub(crate) const ARRAY_OID: u32 = 16013;
pub(crate) const LQUERY_ARRAY_OID: u32 = 16014;

/// The most labels a path has, and the most characters a label has.
const MAX_LEVELS: usize = 65535;
const MAX_LABEL: usize = 1000;

// What `call` does, by `Scalar::Ltree`'s number.
pub(super) const ANCESTOR: u8 = 0; // a @> b
pub(super) const DESCENDANT: u8 = 1; // a <@ b
pub(super) const LT: u8 = 2;
pub(super) const LE: u8 = 3;
pub(super) const GT: u8 = 4;
pub(super) const GE: u8 = 5;
pub(super) const CONCAT: u8 = 6; // a || b
pub(super) const MATCH: u8 = 7; // ltree ~ lquery
pub(super) const MATCH_ANY: u8 = 8; // ltree ? lquery[]
pub(super) const NLEVEL: u8 = 9;
pub(super) const SUBLTREE: u8 = 10;
pub(super) const SUBPATH: u8 = 11;
pub(super) const INDEX: u8 = 12;
pub(super) const TEXT2LTREE: u8 = 13;
pub(super) const LTREE2TEXT: u8 = 14;
pub(super) const LCA: u8 = 15;

pub(super) fn result_type(k: u8) -> Type {
    match k {
        ANCESTOR | DESCENDANT | LT | LE | GT | GE | MATCH | MATCH_ANY => Type::Bool,
        NLEVEL | INDEX => Type::Int,
        LTREE2TEXT => Type::Text,
        _ => Type::Ltree,
    }
}

fn label_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

fn syntax(what: &str, at: usize) -> SqlError {
    SqlError { code: "42601", message: format!("{what} syntax error at character {at}"), info: None }
}

fn ended(what: &str) -> SqlError {
    SqlError {
        code: "42601",
        message: format!("{what} syntax error"),
        info: Some(Box::new(super::Violation {
            detail: Some("Unexpected end of input.".into()),
            ..Default::default()
        })),
    }
}

fn too_long(n: usize, at: usize) -> SqlError {
    SqlError {
        code: "42622",
        message: "label string is too long".into(),
        info: Some(Box::new(super::Violation {
            detail: Some(format!("Label length is {n}, must be at most {MAX_LABEL}, at character {at}.")),
            ..Default::default()
        })),
    }
}

fn too_many(what: &str, n: usize) -> SqlError {
    SqlError {
        code: "54000",
        message: format!("number of {what} levels ({n}) exceeds the maximum allowed ({MAX_LEVELS})"),
        info: None,
    }
}

/// A path's labels, checked as ltree_in checks them: letters, digits, `_` and `-`, none empty. No labels for ''.
pub(super) fn labels(s: &str) -> Result<Vec<&str>> {
    if s.is_empty() {
        return Ok(vec![]);
    }
    let (mut out, mut at) = (vec![], 1usize);
    for l in s.split('.') {
        let n = l.chars().count();
        if let Some(i) = l.chars().position(|c| !label_char(c)) {
            return Err(syntax("ltree", at + i));
        }
        if n == 0 {
            // (an empty label is at its dot, or at the end)
            return Err(if at > s.chars().count() { ended("ltree") } else { syntax("ltree", at) });
        }
        if n > MAX_LABEL {
            return Err(too_long(n, at + n));
        }
        out.push(l);
        at += n + 1;
    }
    if out.len() > MAX_LEVELS {
        return Err(too_many("ltree", out.len()));
    }
    Ok(out)
}

/// A path's text as it is kept: itself, once its labels are checked.
pub(super) fn canonical(s: &str) -> Result<String> {
    labels(s)?;
    Ok(s.to_string())
}

/// A path as text that sorts as paths do: label by label, so `a.b` comes before `a-b` though `.` is after `-`.
pub(super) fn sort_key(s: &str) -> String {
    s.replace('.', "\u{1}")
}

/// One name a level of an lquery takes: `foo`, or `foo*` (what starts with it), `foo@` (in any case), `foo%` (its
/// words, split at `_`, each among the label's).
struct Var {
    name: String,
    prefix: bool,
    ci: bool,
    words: bool,
}

/// One level of an lquery: its names (none: `*`, any label), `!` for none of them, and how many labels it takes.
struct Level {
    vars: Vec<Var>,
    not: bool,
    low: usize,
    high: usize,
}

/// `{n}`, `{n,}`, `{,m}`, `{n,m}` and `{,}`, without the braces.
fn counts(q: &str, at: usize) -> Result<(usize, usize)> {
    let num = |s: &str, none: usize| match s {
        "" => Ok(none),
        s if s.bytes().all(|c| c.is_ascii_digit()) => match s.parse::<usize>() {
            Ok(n) if n <= MAX_LEVELS => Ok(n),
            _ => Err(SqlError {
                code: "42601",
                message: "lquery syntax error".into(),
                info: Some(Box::new(super::Violation {
                    detail: Some(format!(
                        "Low limit ({s}) exceeds the maximum allowed ({MAX_LEVELS}), at character {at}."
                    )),
                    ..Default::default()
                })),
            }),
        },
        _ => Err(syntax("lquery", at)),
    };
    let (low, high) = match q.split_once(',') {
        Some((l, h)) => (num(l, 0)?, num(h, MAX_LEVELS)?),
        None if q.is_empty() => return Err(syntax("lquery", at)),
        None => (num(q, 0)?, num(q, 0)?),
    };
    if low > high {
        return Err(SqlError {
            code: "42601",
            message: "lquery syntax error".into(),
            info: Some(Box::new(super::Violation {
                detail: Some(format!("Low limit ({low}) is greater than high limit ({high}), at character {at}.")),
                ..Default::default()
            })),
        });
    }
    Ok((low, high))
}

/// An lquery's levels, as lquery_in reads them.
fn lquery(s: &str) -> Result<Vec<Level>> {
    if s.is_empty() {
        return Err(ended("lquery"));
    }
    let (mut out, mut at) = (vec![], 1usize);
    for l in s.split('.') {
        let n = l.chars().count();
        if n == 0 {
            return Err(if at > s.chars().count() { ended("lquery") } else { syntax("lquery", at) });
        }
        // the count, at the level's end
        let (body, count) = match l.strip_suffix('}').and_then(|x| x.split_once('{')) {
            Some((body, q)) => (body, Some(counts(q, at + n - 2)?)),
            None => (l, None),
        };
        if let Some(rest) = body.strip_prefix('*') {
            if !rest.is_empty() {
                return Err(syntax("lquery", at + 1));
            }
            let (low, high) = count.unwrap_or((0, MAX_LEVELS));
            out.push(Level { vars: vec![], not: false, low, high });
        } else {
            let (not, names) = match body.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, body),
            };
            let mut pos = at + usize::from(not);
            let mut vars = vec![];
            for v in names.split('|') {
                let end = v.char_indices().find(|(_, c)| !label_char(*c)).map_or(v.len(), |(i, _)| i);
                let (name, flags) = v.split_at(end);
                let len = name.chars().count();
                if len == 0 {
                    return Err(if pos > s.chars().count() { ended("lquery") } else { syntax("lquery", pos) });
                }
                if len > MAX_LABEL {
                    return Err(too_long(len, pos + len));
                }
                let mut var = Var { name: name.into(), prefix: false, ci: false, words: false };
                for (i, c) in flags.chars().enumerate() {
                    match c {
                        '*' => var.prefix = true,
                        '@' => var.ci = true,
                        '%' => var.words = true,
                        _ => return Err(syntax("lquery", pos + len + i)),
                    }
                }
                vars.push(var);
                pos += v.chars().count() + 1;
            }
            let (low, high) = count.unwrap_or((1, 1));
            out.push(Level { vars, not, low, high });
        }
        at += n + 1;
    }
    if out.len() > MAX_LEVELS {
        return Err(too_many("lquery", out.len()));
    }
    Ok(out)
}

/// An lquery's text as it is kept: itself, once it is read.
pub(super) fn lquery_canonical(s: &str) -> Result<String> {
    lquery(s)?;
    Ok(s.to_string())
}

fn same(q: &str, t: &str, ci: bool, prefix: bool) -> bool {
    match (ci, prefix) {
        (false, false) => q == t,
        (false, true) => t.starts_with(q),
        (true, false) => q.to_lowercase() == t.to_lowercase(),
        (true, true) => t.to_lowercase().starts_with(&q.to_lowercase()),
    }
}

fn level_takes(q: &Level, label: &str) -> bool {
    let hit = q.vars.iter().any(|v| match v.words {
        true => v
            .name
            .split('_')
            .filter(|w| !w.is_empty())
            .all(|w| label.split('_').filter(|x| !x.is_empty()).any(|x| same(w, x, v.ci, v.prefix))),
        false => same(&v.name, label, v.ci, v.prefix),
    });
    q.vars.is_empty() || hit != q.not
}

/// Whether the levels take the whole path: lquery_op.c's checkCond, each level trying each count it allows.
fn takes(q: &[Level], t: &[&str]) -> bool {
    let (mut q, mut t) = (q, t);
    while let Some((cur, rest)) = q.split_first() {
        let (low, high) = (cur.low, cur.high.min(t.len()));
        if high < low {
            return false;
        }
        q = rest;
        for n in 0..high {
            if n >= low && takes(rest, t) {
                return true;
            }
            if !level_takes(cur, t[0]) {
                return false;
            }
            t = &t[1..];
        }
    }
    t.is_empty()
}

/// The elements of an array of paths or patterns, if the value is one (a path's text never starts with a brace).
fn list(v: &Value) -> Result<Option<Vec<String>>> {
    let array = match v {
        Value::Json(serde_json::Value::Array(_)) => true,
        Value::Text(t) => t.trim_start().starts_with('{'),
        _ => false,
    };
    if !array {
        return Ok(None);
    }
    let vals = super::array_values(v.clone())?.unwrap_or_default();
    Ok(Some(vals.into_iter().filter(|x| *x != Value::Null).map(|x| x.to_text()).collect()))
}

fn each(v: &Value) -> Result<Vec<String>> {
    Ok(list(v)?.unwrap_or_else(|| vec![v.to_text()]))
}

fn int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(n) => Ok(*n),
        v => v
            .to_text()
            .trim()
            .parse()
            .or_else(|_| err("22P02", format!("invalid input syntax for type integer: \"{}\"", v.to_text()))),
    }
}

/// The labels from `start` up to `end`, as inner_subltree takes them.
fn part(l: &[&str], start: i64, end: i64) -> Result<Value> {
    let n = l.len() as i64;
    if start < 0 || end < 0 || start >= n || start > end {
        return err("22023", "invalid positions");
    }
    Ok(Value::Text(l[start as usize..end.min(n) as usize].join(".")))
}

pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    use Value::{Bool, Int, Null, Text};
    let Scalar::Ltree(k) = f else { return err("XX000", "not an ltree function") };
    if a.contains(&Null) {
        return Ok(Null);
    }
    let t = |i: usize| a[i].to_text();
    Ok(match k {
        // (either side may be an array of paths: true if any of them is)
        ANCESTOR | DESCENDANT => {
            let (xs, ys) = if k == ANCESTOR { (each(&a[0])?, each(&a[1])?) } else { (each(&a[1])?, each(&a[0])?) };
            let mut any = false;
            for x in &xs {
                let x = labels(x)?;
                for y in &ys {
                    let y = labels(y)?;
                    any |= y.len() >= x.len() && y[..x.len()] == x[..];
                }
            }
            Bool(any)
        }
        LT | LE | GT | GE => {
            let (x, y) = (t(0), t(1));
            let o = labels(&x)?.cmp(&labels(&y)?);
            Bool(match k {
                LT => o.is_lt(),
                LE => o.is_le(),
                GT => o.is_gt(),
                _ => o.is_ge(),
            })
        }
        CONCAT => {
            let (x, y) = (t(0), t(1));
            let n = labels(&x)?.len() + labels(&y)?.len();
            if n > MAX_LEVELS {
                return Err(too_many("ltree", n));
            }
            Text(if x.is_empty() || y.is_empty() { x + &y } else { format!("{x}.{y}") })
        }
        // a path (or any of an array of them) against a pattern, or any of an array of patterns
        MATCH | MATCH_ANY => {
            let queries = each(&a[1])?.iter().map(|q| lquery(q)).collect::<Result<Vec<_>>>()?;
            let mut any = false;
            for x in each(&a[0])? {
                let x = labels(&x)?;
                any |= queries.iter().any(|q| takes(q, &x));
            }
            Bool(any)
        }
        NLEVEL => Int(labels(&t(0))?.len() as i64),
        SUBLTREE => part(&labels(&t(0))?, int(&a[1])?, int(&a[2])?)?,
        SUBPATH => {
            let x = t(0);
            let l = labels(&x)?;
            let n = l.len() as i64;
            let (mut start, len) = (int(&a[1])?, a.get(2).map(int).transpose()?);
            // (from the end, and again if that is still before the start: Postgres 16's ltree_subpath adjusts twice,
            // so subpath('a.b.c', -4) is c there)
            for _ in 0..2 {
                if start < 0 {
                    start += n;
                }
            }
            let end = match len {
                None => 0xffff,
                Some(len) if len < 0 => n + len,
                Some(0) => start,
                Some(len) => start + len,
            };
            part(&l, start, end)?
        }
        INDEX => {
            let (x, y) = (t(0), t(1));
            let (x, y) = (labels(&x)?, labels(&y)?);
            let mut start = a.get(2).map(int).transpose()?.unwrap_or(0);
            if start < 0 {
                start = (x.len() as i64 + start).max(0);
            }
            let start = start as usize;
            let found = (!x.is_empty() && !y.is_empty() && x.len() >= start + y.len())
                .then(|| (start..=x.len() - y.len()).find(|i| x[*i..*i + y.len()] == y[..]))
                .flatten();
            Int(found.map_or(-1, |i| i as i64))
        }
        TEXT2LTREE => Text(canonical(&t(0))?),
        LTREE2TEXT => Text(t(0)),
        // the longest path above every one of them: of lca(a, b, ...) or of an array's
        LCA => {
            let paths = match a {
                [one] => each(one)?,
                all => all.iter().map(Value::to_text).collect(),
            };
            let Some(first) = paths.first() else { return Ok(Null) };
            let first = labels(first)?;
            if first.is_empty() {
                return Ok(Null);
            }
            let mut n = first.len() - 1;
            for p in &paths[1..] {
                let p = labels(p)?;
                if p.is_empty() {
                    return Ok(Null);
                }
                let most = n.min(p.len() - 1);
                n = (0..most).take_while(|i| first[*i] == p[*i]).count();
            }
            Text(first[..n].join("."))
        }
        _ => return err("XX000", "not an ltree function"),
    })
}
