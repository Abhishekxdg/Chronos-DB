//! Range types (int4range, int8range, numrange, daterange, tsrange, tstzrange): Postgres's literal syntax,
//! canonical form for the discrete ones, and the operators and functions over them. A range value is its
//! canonical literal as text; every operation parses its inputs and writes a literal back.

use super::{SqlError, Type, Value, coerce, compare, err};
use std::cmp::Ordering;

type Result<T> = std::result::Result<T, SqlError>;

/// (range oid, name, subtype oid, multirange oid)
pub(super) const RANGES: &[(u32, &str, u32, u32)] = &[
    (3904, "int4range", 23, 4451),
    (3906, "numrange", 1700, 4532),
    (3908, "tsrange", 1114, 4533),
    (3910, "tstzrange", 1184, 4534),
    (3912, "daterange", 1082, 4535),
    (3926, "int8range", 20, 4536),
];

pub(super) fn by_name(name: &str) -> Option<u32> {
    if let Some(r) = RANGES.iter().find(|r| r.1 == name) {
        return Some(r.0);
    }
    USER.lock().unwrap().iter().find(|x| x.1 == name).map(|x| x.0)
}

pub(super) fn name_of(oid: u32) -> Option<std::borrow::Cow<'static, str>> {
    if let Some(r) = RANGES.iter().find(|r| r.0 == oid) {
        return Some(r.1.into());
    }
    USER.lock().unwrap().iter().find(|x| x.0 == oid).map(|x| x.1.clone().into())
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Kind {
    pub sub: Type,
    /// discrete subtypes (int4, int8, date) keep ranges in `[)` form
    pub canon: bool,
}

static USER: std::sync::Mutex<Vec<(u32, String, Type)>> = std::sync::Mutex::new(Vec::new());

/// A range made with CREATE TYPE ... AS RANGE: its id comes from its name, so every session agrees on it.
pub(super) fn register(name: &str, sub: Type) -> u32 {
    let h = name.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    let oid = 100_000 + (h % 1_900_000_000) as u32;
    let mut u = USER.lock().unwrap();
    match u.iter_mut().find(|x| x.0 == oid) {
        Some(x) => *x = (oid, name.to_string(), sub),
        None => u.push((oid, name.to_string(), sub)),
    }
    oid
}

pub(super) fn kind(oid: u32) -> Option<Kind> {
    if let Some(r) = RANGES.iter().find(|r| r.0 == oid) {
        let sub = Type::from_oid(r.2)?;
        return Some(Kind { sub, canon: matches!(sub, Type::Int | Type::BigInt | Type::Date) });
    }
    USER.lock().unwrap().iter().find(|x| x.0 == oid).map(|x| Kind { sub: x.2, canon: false })
}

pub(super) fn subtype(oid: u32) -> Option<Type> {
    kind(oid).map(|k| k.sub)
}

const USER_MULTI: u32 = 2_200_000_000;

/// `int4range` is `int4multirange`, `textrange` `textmultirange`; a name without "range" in it gets `_multirange`.
fn multi_name(range_name: &str) -> String {
    match range_name.rfind("range") {
        Some(p) => format!("{}multirange{}", &range_name[..p], &range_name[p + 5..]),
        None => format!("{range_name}_multirange"),
    }
}

/// The id of a range's multirange.
pub(super) fn multi_of(range: u32) -> Option<u32> {
    if let Some(r) = RANGES.iter().find(|r| r.0 == range) {
        return Some(r.3);
    }
    USER.lock().unwrap().iter().any(|x| x.0 == range).then_some(range + USER_MULTI)
}

/// The range a multirange id is of.
pub(super) fn of_multi(multi: u32) -> Option<u32> {
    if let Some(r) = RANGES.iter().find(|r| r.3 == multi) {
        return Some(r.0);
    }
    let range = multi.checked_sub(USER_MULTI)?;
    USER.lock().unwrap().iter().any(|x| x.0 == range).then_some(range)
}

pub(super) fn multi_by_name(name: &str) -> Option<u32> {
    if let Some(r) = RANGES.iter().find(|r| multi_name(r.1) == name) {
        return Some(r.3);
    }
    USER.lock().unwrap().iter().find(|x| multi_name(&x.1) == name).map(|x| x.0 + USER_MULTI)
}

/// The range a multirange constructor's name (`int4multirange`) is of.
pub(super) fn multi_ctor(name: &str) -> Option<u32> {
    multi_by_name(name).and_then(of_multi)
}

pub(super) fn multi_name_of(multi: u32) -> Option<String> {
    of_multi(multi).and_then(name_of).map(|n| multi_name(&n))
}

#[derive(Clone, Debug)]
pub(super) struct Range {
    pub empty: bool,
    pub lo: Option<Value>,
    pub hi: Option<Value>,
    pub lo_inc: bool,
    pub hi_inc: bool,
}

fn malformed(text: &str, detail: &str) -> SqlError {
    SqlError {
        code: "22P02",
        message: format!("malformed range literal: \"{text}\""),
        info: Some(Box::new(super::Violation { detail: Some(detail.into()), ..Default::default() })),
    }
}

impl Range {
    pub fn empty() -> Range {
        Range { empty: true, lo: None, hi: None, lo_inc: false, hi_inc: false }
    }

    pub fn text(&self) -> String {
        if self.empty {
            return "empty".into();
        }
        let bound = |v: &Option<Value>| match v {
            None => String::new(),
            Some(v) => {
                let t = v.to_text();
                let plain = !t.is_empty()
                    && !t.chars().any(|c| matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || c.is_whitespace());
                match plain {
                    true => t,
                    false => format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\"\"")),
                }
            }
        };
        format!(
            "{}{},{}{}",
            if self.lo_inc { '[' } else { '(' },
            bound(&self.lo),
            bound(&self.hi),
            if self.hi_inc { ']' } else { ')' }
        )
    }
}

fn parse_bound(c: &[char], i: &mut usize, whole: &str) -> Result<Option<String>> {
    if matches!(c.get(*i), Some(',' | ')' | ']')) {
        return Ok(None);
    }
    let (mut s, mut quote) = (String::new(), false);
    loop {
        let Some(&ch) = c.get(*i) else { return Err(malformed(whole, "Unexpected end of input.")) };
        if ch == '\\' {
            *i += 1;
            match c.get(*i) {
                Some(&x) => s.push(x),
                None => return Err(malformed(whole, "Unexpected end of input.")),
            }
        } else if ch == '"' {
            if !quote {
                quote = true;
            } else if c.get(*i + 1) == Some(&'"') {
                s.push('"');
                *i += 1;
            } else {
                quote = false;
            }
        } else if !quote && matches!(ch, ',' | ')' | ']') {
            break;
        } else {
            s.push(ch);
        }
        *i += 1;
    }
    Ok(Some(s))
}

pub(super) fn parse(text: &str, k: Kind) -> Result<Range> {
    let c: Vec<char> = text.chars().collect();
    let mut i = 0;
    while c.get(i).is_some_and(|x| x.is_whitespace()) {
        i += 1;
    }
    let rest: String = c[i..].iter().take(5).collect();
    if rest.eq_ignore_ascii_case("empty") {
        i += 5;
        while c.get(i).is_some_and(|x| x.is_whitespace()) {
            i += 1;
        }
        if i < c.len() {
            return Err(malformed(text, "Junk after \"empty\" key word."));
        }
        return Ok(Range::empty());
    }
    let lo_inc = match c.get(i) {
        Some('[') => true,
        Some('(') => false,
        _ => return Err(malformed(text, "Missing left parenthesis or bracket.")),
    };
    i += 1;
    let lo = parse_bound(&c, &mut i, text)?;
    if c.get(i) != Some(&',') {
        return Err(malformed(text, "Missing comma after lower bound."));
    }
    i += 1;
    let hi = parse_bound(&c, &mut i, text)?;
    let hi_inc = match c.get(i) {
        Some(')') => false,
        Some(']') => true,
        Some(',') => return Err(malformed(text, "Too many commas.")),
        _ => return Err(malformed(text, "Unexpected end of input.")),
    };
    i += 1;
    while c.get(i).is_some_and(|x| x.is_whitespace()) {
        i += 1;
    }
    if i < c.len() {
        return Err(malformed(text, "Junk after right parenthesis or bracket."));
    }
    let conv = |s: Option<String>| s.map(|s| coerce(Value::Text(s), k.sub)).transpose();
    make(conv(lo)?, lo_inc, conv(hi)?, hi_inc, k)
}

fn step(v: &Value, sub: Type) -> Option<Value> {
    match (v, sub) {
        (Value::Int(n), Type::Int) => n.checked_add(1).filter(|n| *n <= i32::MAX as i64).map(Value::Int),
        (Value::Int(n), Type::BigInt) => n.checked_add(1).map(Value::Int),
        (Value::Date(d), Type::Date) => d.checked_add(1).map(Value::Date),
        _ => None,
    }
}

fn check(lo: &Option<Value>, hi: &Option<Value>, lo_inc: bool, hi_inc: bool) -> Result<bool> {
    if let (Some(l), Some(h)) = (lo, hi) {
        match compare(l, h)? {
            Ordering::Greater => {
                return err("22000", "range lower bound must be less than or equal to range upper bound");
            }
            Ordering::Equal if !(lo_inc && hi_inc) => return Ok(true),
            _ => {}
        }
    }
    Ok(false)
}

pub(super) fn make(
    mut lo: Option<Value>,
    mut lo_inc: bool,
    mut hi: Option<Value>,
    mut hi_inc: bool,
    k: Kind,
) -> Result<Range> {
    // (an unbounded side is never inclusive)
    lo_inc &= lo.is_some();
    hi_inc &= hi.is_some();
    if check(&lo, &hi, lo_inc, hi_inc)? {
        return Ok(Range::empty());
    }
    if k.canon {
        // (one past the last value of the type is out of range)
        let past = || match k.sub {
            Type::Int => err("22003", "integer out of range"),
            Type::BigInt => err("22003", "bigint out of range"),
            _ => err("22008", "date out of range"),
        };
        if let (Some(v), false) = (&lo, lo_inc) {
            match step(v, k.sub) {
                Some(n) => (lo, lo_inc) = (Some(n), true),
                // (an infinite date is left as it is)
                None if matches!(v, Value::Date(i32::MAX | i32::MIN)) => {}
                None => return past(),
            }
        }
        if let (Some(v), true) = (&hi, hi_inc) {
            match step(v, k.sub) {
                Some(n) => (hi, hi_inc) = (Some(n), false),
                None if matches!(v, Value::Date(i32::MAX | i32::MIN)) => {}
                None => return past(),
            }
        }
        if check(&lo, &hi, lo_inc, hi_inc)? {
            return Ok(Range::empty());
        }
    }
    Ok(Range { empty: false, lo, hi, lo_inc, hi_inc })
}

fn cmp_lo(a: &Range, b: &Range) -> Result<Ordering> {
    Ok(match (&a.lo, &b.lo) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Less,
        (_, None) => Ordering::Greater,
        (Some(x), Some(y)) => match compare(x, y)? {
            Ordering::Equal => b.lo_inc.cmp(&a.lo_inc),
            o => o,
        },
    })
}

fn cmp_hi(a: &Range, b: &Range) -> Result<Ordering> {
    Ok(match (&a.hi, &b.hi) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (Some(x), Some(y)) => match compare(x, y)? {
            Ordering::Equal => a.hi_inc.cmp(&b.hi_inc),
            o => o,
        },
    })
}

pub(super) fn contains_elem(r: &Range, v: &Value) -> Result<bool> {
    if r.empty {
        return Ok(false);
    }
    if let Some(l) = &r.lo {
        match compare(l, v)? {
            Ordering::Greater => return Ok(false),
            Ordering::Equal if !r.lo_inc => return Ok(false),
            _ => {}
        }
    }
    if let Some(h) = &r.hi {
        match compare(v, h)? {
            Ordering::Greater => return Ok(false),
            Ordering::Equal if !r.hi_inc => return Ok(false),
            _ => {}
        }
    }
    Ok(true)
}

pub(super) fn contains(a: &Range, b: &Range) -> Result<bool> {
    if b.empty {
        return Ok(true);
    }
    if a.empty {
        return Ok(false);
    }
    Ok(cmp_lo(a, b)? != Ordering::Greater && cmp_hi(a, b)? != Ordering::Less)
}

fn before(a: &Range, b: &Range) -> Result<bool> {
    match (&a.hi, &b.lo) {
        (Some(h), Some(l)) => Ok(match compare(h, l)? {
            Ordering::Less => true,
            Ordering::Equal => !(a.hi_inc && b.lo_inc),
            Ordering::Greater => false,
        }),
        _ => Ok(false),
    }
}

pub(super) fn overlaps(a: &Range, b: &Range) -> Result<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    Ok(!before(a, b)? && !before(b, a)?)
}

pub(super) fn left_of(a: &Range, b: &Range) -> Result<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    before(a, b)
}

pub(super) fn adjacent(a: &Range, b: &Range) -> Result<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    let touch = |x: &Range, y: &Range| -> Result<bool> {
        match (&x.hi, &y.lo) {
            (Some(h), Some(l)) => Ok(compare(h, l)? == Ordering::Equal && x.hi_inc != y.lo_inc),
            _ => Ok(false),
        }
    };
    Ok(touch(a, b)? || touch(b, a)?)
}

pub(super) fn union(a: &Range, b: &Range, k: Kind, merge: bool) -> Result<Range> {
    if a.empty {
        return Ok(b.clone());
    }
    if b.empty {
        return Ok(a.clone());
    }
    if !merge && !overlaps(a, b)? && !adjacent(a, b)? {
        return err("22000", "result of range union would not be contiguous");
    }
    let (lo, lo_inc) = if cmp_lo(a, b)? != Ordering::Greater { (&a.lo, a.lo_inc) } else { (&b.lo, b.lo_inc) };
    let (hi, hi_inc) = if cmp_hi(a, b)? != Ordering::Less { (&a.hi, a.hi_inc) } else { (&b.hi, b.hi_inc) };
    make(lo.clone(), lo_inc, hi.clone(), hi_inc, k)
}

pub(super) fn intersect(a: &Range, b: &Range, k: Kind) -> Result<Range> {
    if a.empty || b.empty || !overlaps(a, b)? {
        return Ok(Range::empty());
    }
    let (lo, lo_inc) = if cmp_lo(a, b)? != Ordering::Less { (&a.lo, a.lo_inc) } else { (&b.lo, b.lo_inc) };
    let (hi, hi_inc) = if cmp_hi(a, b)? != Ordering::Greater { (&a.hi, a.hi_inc) } else { (&b.hi, b.hi_inc) };
    make(lo.clone(), lo_inc, hi.clone(), hi_inc, k)
}

pub(super) fn difference(a: &Range, b: &Range, k: Kind) -> Result<Range> {
    if a.empty || b.empty || !overlaps(a, b)? {
        return Ok(a.clone());
    }
    let (cl, ch) = (cmp_lo(a, b)?, cmp_hi(a, b)?);
    if cl == Ordering::Less && ch == Ordering::Greater {
        return err("22000", "result of range difference would not be contiguous");
    }
    if cl != Ordering::Less && ch != Ordering::Greater {
        return Ok(Range::empty());
    }
    if cl == Ordering::Less {
        return make(a.lo.clone(), a.lo_inc, b.lo.clone(), !b.lo_inc, k);
    }
    make(b.hi.clone(), !b.hi_inc, a.hi.clone(), a.hi_inc, k)
}

pub(super) fn not_right_of(a: &Range, b: &Range) -> Result<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    Ok(cmp_hi(a, b)? != Ordering::Greater)
}

pub(super) fn not_left_of(a: &Range, b: &Range) -> Result<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    Ok(cmp_lo(a, b)? != Ordering::Less)
}

use super::funcs::Scalar;

fn arg_sub(a: &[Value]) -> Result<Kind> {
    match a.last() {
        Some(Value::Int(o)) => kind(of_multi(*o as u32).unwrap_or(*o as u32)).ok_or_else(|| SqlError {
            code: "XX000",
            message: "not a range type".into(),
            info: None,
        }),
        _ => err("XX000", "a range function was called without its type"),
    }
}

fn flags_of(v: Option<&Value>) -> Result<(bool, bool)> {
    let Some(v) = v else { return Ok((true, false)) };
    if *v == Value::Null {
        return err("22000", "range constructor flags argument must not be null");
    }
    match v.to_text().as_str() {
        "[)" => Ok((true, false)),
        "[]" => Ok((true, true)),
        "(]" => Ok((false, true)),
        "()" => Ok((false, false)),
        _ => Err(SqlError {
            code: "42601",
            message: "invalid range bound flags".into(),
            info: Some(Box::new(super::Violation {
                detail: Some("Valid values are \"[]\", \"[)\", \"(]\", and \"()\".".into()),
                ..Default::default()
            })),
        }),
    }
}

/// The range functions and operators; the last argument is always the range type's oid.
pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    use Value::{Bool, Null, Text};
    if let Scalar::RangeNew(oid) = f {
        let k = kind(oid).unwrap_or(Kind { sub: Type::Text, canon: false });
        let (lo_inc, hi_inc) = flags_of(a.get(2))?;
        let bound = |v: &Value| match v {
            Null => Ok(None),
            v => coerce(v.clone(), k.sub).map(Some),
        };
        return Ok(Text(make(bound(&a[0])?, lo_inc, bound(&a[1])?, hi_inc, k)?.text()));
    }
    if let Scalar::MultiNew(oid) = f {
        if a.contains(&Null) {
            return Ok(Null);
        }
        let k = kind(oid).unwrap_or(Kind { sub: Type::Text, canon: false });
        let mut ranges = vec![];
        for v in a {
            ranges.extend(as_list(v, k)?);
        }
        return Ok(Text(multi_text(&normalize(ranges, k)?)));
    }
    if a.contains(&Null) {
        return Ok(Null);
    }
    let k = arg_sub(a)?;
    let multi = matches!(a.last(), Some(Value::Int(o)) if of_multi(*o as u32).is_some());
    let listy = |v: &Value| v.to_text().trim_start().starts_with('{');
    if let Scalar::RangeBound(_) | Scalar::RangeFlag(_) | Scalar::RangeMerge = f
        && listy(&a[0])
    {
        let l = parse_multi(&a[0].to_text(), k)?;
        return Ok(match f {
            Scalar::RangeBound(upper) => {
                match if upper { l.last().and_then(|r| r.hi.clone()) } else { l.first().and_then(|r| r.lo.clone()) } {
                    Some(v) => v,
                    None => Null,
                }
            }
            Scalar::RangeFlag(n) => Bool(match n {
                0 => l.is_empty(),
                1 => l.first().is_some_and(|r| r.lo_inc),
                2 => l.last().is_some_and(|r| r.hi_inc),
                3 => l.first().is_some_and(|r| r.lo.is_none()),
                _ => l.last().is_some_and(|r| r.hi.is_none()),
            }),
            _ => match (l.first(), l.last()) {
                (Some(x), Some(y)) => Text(make(x.lo.clone(), x.lo_inc, y.hi.clone(), y.hi_inc, k)?.text()),
                _ => Text("empty".into()),
            },
        });
    }
    if let Scalar::RangeOp | Scalar::RangeSetOp = f
        && (multi || listy(&a[0]) || listy(&a[1]))
    {
        let op = a[2].to_text();
        let x = as_list(&a[0], k)?;
        if op == "@>" && !listy(&a[1]) && parse(&a[1].to_text(), k).is_err() {
            let e = coerce(a[1].clone(), k.sub)?;
            let mut found = false;
            for r in &x {
                found |= contains_elem(r, &e)?;
            }
            return Ok(Bool(found));
        }
        // multiranges ordered as multirange_cmp orders them: range by range, then the one with fewer first
        if matches!(op.as_str(), "<" | "<=" | ">" | ">=") && listy(&a[0]) && listy(&a[1]) {
            let (kx, ky) = (sort_key(&a[0].to_text(), oid_of(a))?, sort_key(&a[1].to_text(), oid_of(a))?);
            let o = kx.cmp(&ky);
            return Ok(Bool(match op.as_str() {
                "<" => o.is_lt(),
                "<=" => o.is_le(),
                ">" => o.is_gt(),
                _ => o.is_ge(),
            }));
        }
        return list_op(&op, &x, &as_list(&a[1], k)?, k);
    }
    let r = |v: &Value| parse(&v.to_text(), k);
    Ok(match f {
        Scalar::RangeBound(upper) => {
            let x = r(&a[0])?;
            match (x.empty, if upper { x.hi } else { x.lo }) {
                (false, Some(v)) => v,
                _ => Null,
            }
        }
        Scalar::RangeFlag(k) => {
            let x = r(&a[0])?;
            Bool(match k {
                0 => x.empty,
                1 => !x.empty && x.lo_inc,
                2 => !x.empty && x.hi_inc,
                3 => !x.empty && x.lo.is_none(),
                _ => !x.empty && x.hi.is_none(),
            })
        }
        Scalar::RangeMerge => Text(union(&r(&a[0])?, &r(&a[1])?, k, true)?.text()),
        Scalar::RangeOp | Scalar::RangeSetOp => {
            let op = a[2].to_text();
            let x = r(&a[0])?;
            if op == "@>" {
                if let Ok(y) = r(&a[1]) {
                    return Ok(Bool(contains(&x, &y)?));
                }
                return Ok(Bool(contains_elem(&x, &coerce(a[1].clone(), k.sub)?)?));
            }
            let y = r(&a[1])?;
            if matches!(op.as_str(), "<" | "<=" | ">" | ">=") {
                let (kx, ky) = (sort_key(&a[0].to_text(), oid_of(a))?, sort_key(&a[1].to_text(), oid_of(a))?);
                let o = kx.cmp(&ky);
                return Ok(Bool(match op.as_str() {
                    "<" => o.is_lt(),
                    "<=" => o.is_le(),
                    ">" => o.is_gt(),
                    _ => o.is_ge(),
                }));
            }
            match op.as_str() {
                "&&" => Bool(overlaps(&x, &y)?),
                "<<" => Bool(left_of(&x, &y)?),
                ">>" => Bool(left_of(&y, &x)?),
                "&<" => Bool(not_right_of(&x, &y)?),
                "&>" => Bool(not_left_of(&x, &y)?),
                "-|-" => Bool(adjacent(&x, &y)?),
                "+" => Text(union(&x, &y, k, false)?.text()),
                "*" => Text(intersect(&x, &y, k)?.text()),
                "-" => Text(difference(&x, &y, k)?.text()),
                _ => return err("XX000", format!("unknown range operator {op}")),
            }
        }
        _ => return err("XX000", "not a range function"),
    })
}

/// Makes the database's own range and composite types known before a script is read (a restart forgets the
/// registry). A type made over another that sorts after it by name is made in a later pass (a composite of
/// composites, a range over a composite).
pub(super) fn hydrate() {
    super::with_path_view(|v| {
        let Ok(rows) = v.prefix("_sonos_types/") else { return };
        let mut todo: Vec<(String, serde_json::Value)> = rows
            .flatten()
            .map(|r| {
                (
                    r.0["_sonos_types/".len()..].to_string(),
                    serde_json::from_str::<serde_json::Value>(&r.1).unwrap_or_default(),
                )
            })
            .filter(|(_, j)| j["composite"].is_array() || j["range"]["subtype"].is_string())
            .collect();
        // (each pass takes what resolves; a pass that takes nothing ends it: what's left has a field of a
        // type that isn't there)
        while !todo.is_empty() {
            let before = todo.len();
            todo.retain(|(name, j)| {
                if let Some(fields) = j["composite"].as_array() {
                    if super::userdefs::composite(name).is_some_and(|t| t != super::Type::record(vec![])) {
                        return false;
                    }
                    let fs: Vec<(String, String)> = fields
                        .iter()
                        .filter_map(|f| Some((f[0].as_str()?.to_string(), f[1].as_str()?.to_string())))
                        .collect();
                    super::userdefs::register_composite(name, &fs);
                    return super::userdefs::composite(name).is_none();
                }
                let Some(sub) = j["range"]["subtype"].as_str() else { return false };
                if by_name(name).is_some() {
                    return false;
                }
                if let Ok(c) = super::Parser::new(super::lex(&format!("x {sub}")).unwrap_or_default()).column()
                    && let Ok(t) = super::resolve_type(v, c.ty)
                {
                    register(name, t);
                    return false;
                }
                true
            });
            if todo.len() == before {
                break;
            }
        }
    });
}

// ---------------------------------------------------------------- multiranges

fn bad_multi(text: &str, detail: &str) -> SqlError {
    SqlError {
        code: "22P02",
        message: format!("malformed multirange literal: \"{text}\""),
        info: Some(Box::new(super::Violation { detail: Some(detail.into()), ..Default::default() })),
    }
}

/// Sorted, with the overlapping and adjacent ranges merged and the empty ones gone.
pub(super) fn normalize(mut v: Vec<Range>, k: Kind) -> Result<Vec<Range>> {
    v.retain(|r| !r.empty);
    let mut failed = None;
    v.sort_by(|a, b| {
        cmp_lo(a, b).unwrap_or_else(|e| {
            failed = Some(e);
            Ordering::Equal
        })
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let mut out: Vec<Range> = vec![];
    for r in v {
        match out.last_mut() {
            Some(cur) if overlaps(cur, &r)? || adjacent(cur, &r)? => *cur = union(cur, &r, k, true)?,
            _ => out.push(r),
        }
    }
    Ok(out)
}

pub(super) fn multi_text(v: &[Range]) -> String {
    format!("{{{}}}", v.iter().map(Range::text).collect::<Vec<_>>().join(","))
}

pub(super) fn parse_multi(text: &str, k: Kind) -> Result<Vec<Range>> {
    let c: Vec<char> = text.chars().collect();
    let mut i = 0;
    let ws = |i: &mut usize| {
        while c.get(*i).is_some_and(|x| x.is_whitespace()) {
            *i += 1;
        }
    };
    ws(&mut i);
    if c.get(i) != Some(&'{') {
        return Err(bad_multi(text, "Missing left brace."));
    }
    i += 1;
    ws(&mut i);
    let mut ranges = vec![];
    if c.get(i) == Some(&'}') {
        i += 1;
    } else {
        loop {
            ws(&mut i);
            let start = i;
            match c.get(i) {
                None => return Err(bad_multi(text, "Unexpected end of input.")),
                Some('[' | '(') => {
                    i += 1;
                    let mut quote = false;
                    loop {
                        match c.get(i) {
                            None => return Err(bad_multi(text, "Unexpected end of input.")),
                            Some('\\') => i += 1,
                            Some('"') => {
                                if !quote {
                                    quote = true;
                                } else if c.get(i + 1) == Some(&'"') {
                                    i += 1;
                                } else {
                                    quote = false;
                                }
                            }
                            Some(']' | ')') if !quote => break,
                            _ => {}
                        }
                        i += 1;
                    }
                    i += 1;
                }
                Some('e' | 'E') => i += 5,
                _ => return Err(bad_multi(text, "Expected range start.")),
            }
            let piece: String = c[start..i.min(c.len())].iter().collect();
            ranges.push(parse(&piece, k)?);
            ws(&mut i);
            match c.get(i) {
                Some(',') => i += 1,
                Some('}') => {
                    i += 1;
                    break;
                }
                None => return Err(bad_multi(text, "Unexpected end of input.")),
                _ => return Err(bad_multi(text, "Expected comma or end of multirange.")),
            }
        }
    }
    ws(&mut i);
    if i < c.len() {
        return Err(bad_multi(text, "Junk after closing right brace."));
    }
    normalize(ranges, k)
}

/// A range or a multirange value as a list of ranges.
fn as_list(v: &Value, k: Kind) -> Result<Vec<Range>> {
    let t = v.to_text();
    if t.trim_start().starts_with('{') {
        return parse_multi(&t, k);
    }
    let r = parse(&t, k)?;
    Ok(if r.empty { vec![] } else { vec![r] })
}

fn minus_pieces(a: &Range, b: &Range, k: Kind) -> Result<Vec<Range>> {
    if a.empty || b.empty || !overlaps(a, b)? {
        return Ok(vec![a.clone()]);
    }
    let mut out = vec![];
    if cmp_lo(a, b)? == Ordering::Less {
        out.push(make(a.lo.clone(), a.lo_inc, b.lo.clone(), !b.lo_inc, k)?);
    }
    if cmp_hi(a, b)? == Ordering::Greater {
        out.push(make(b.hi.clone(), !b.hi_inc, a.hi.clone(), a.hi_inc, k)?);
    }
    Ok(out)
}

fn list_op(op: &str, a: &[Range], b: &[Range], k: Kind) -> Result<Value> {
    use Value::Bool;
    let touch = |x: &Range, y: &Range| adjacent(x, y);
    Ok(match op {
        "@>" => Bool(b.iter().try_fold(true, |acc, y| -> Result<bool> {
            Ok(acc && a.iter().try_fold(false, |f, x| Ok::<_, SqlError>(f || contains(x, y)?))?)
        })?),
        "&&" => {
            let mut hit = false;
            for x in a {
                for y in b {
                    hit |= overlaps(x, y)?;
                }
            }
            Bool(hit)
        }
        "<<" => Bool(match (a.last(), b.first()) {
            (Some(x), Some(y)) => left_of(x, y)?,
            _ => false,
        }),
        ">>" => Bool(match (b.last(), a.first()) {
            (Some(x), Some(y)) => left_of(x, y)?,
            _ => false,
        }),
        "&<" => Bool(match (a.last(), b.last()) {
            (Some(x), Some(y)) => not_right_of(x, y)?,
            _ => false,
        }),
        "&>" => Bool(match (a.first(), b.first()) {
            (Some(x), Some(y)) => not_left_of(x, y)?,
            _ => false,
        }),
        "-|-" => Bool(match (a.last(), a.first(), b.first(), b.last()) {
            (Some(al), Some(af), Some(bf), Some(bl)) => touch(al, bf)? || touch(bl, af)?,
            _ => false,
        }),
        "+" => Value::Text(multi_text(&normalize(a.iter().chain(b).cloned().collect(), k)?)),
        "*" => {
            let mut out = vec![];
            for x in a {
                for y in b {
                    out.push(intersect(x, y, k)?);
                }
            }
            Value::Text(multi_text(&normalize(out, k)?))
        }
        "-" => {
            let mut out = vec![];
            for x in a {
                let mut pieces = vec![x.clone()];
                for y in b {
                    let mut next = vec![];
                    for p in &pieces {
                        next.extend(minus_pieces(p, y, k)?);
                    }
                    pieces = next;
                }
                out.extend(pieces);
            }
            Value::Text(multi_text(&normalize(out, k)?))
        }
        _ => return err("XX000", format!("unknown range operator {op}")),
    })
}

fn oid_of(a: &[Value]) -> u32 {
    match a.last() {
        Some(Value::Int(o)) => *o as u32,
        _ => 0,
    }
}

/// A key whose text order is the order of the values of a range or multirange type: empty first, then by lower
/// bound (an unbounded one first; inclusive before exclusive), then upper (an unbounded one last).
pub(super) fn sort_key(text: &str, oid: u32) -> Result<String> {
    let k = kind(of_multi(oid).unwrap_or(oid)).unwrap_or(Kind { sub: Type::Text, canon: false });
    let enc = |v: &Value| -> String {
        match v {
            Value::Int(n) => format!("{:032x}", i128::from(*n) + (1i128 << 100)),
            Value::Date(d) => format!("{:032x}", i128::from(*d) + (1i128 << 100)),
            Value::Timestamp(t) | Value::TimestampTz(t) => format!("{:032x}", i128::from(*t) + (1i128 << 100)),
            Value::Float(f) => {
                let b = f.to_bits();
                format!("{:016x}", if b >> 63 == 1 { !b } else { b | (1 << 63) })
            }
            Value::Numeric(d) => {
                let b = d.to_f64().to_bits();
                format!("{:016x}", if b >> 63 == 1 { !b } else { b | (1 << 63) })
            }
            v => format!("{}\u{0}", v.to_text()),
        }
    };
    let one = |r: &Range| -> String {
        if r.empty {
            return "0".into();
        }
        let lo = match &r.lo {
            None => "0".to_string(),
            Some(v) => format!("1{}{}", enc(v), if r.lo_inc { "0" } else { "1" }),
        };
        let hi = match &r.hi {
            None => "2".to_string(),
            Some(v) => format!("1{}{}", enc(v), if r.hi_inc { "1" } else { "0" }),
        };
        format!("1{lo}{hi}")
    };
    if text.trim_start().starts_with('{') {
        return Ok(parse_multi(text, k)?.iter().map(|r| one(r) + "\u{1}").collect());
    }
    Ok(one(&parse(text, k)?))
}
