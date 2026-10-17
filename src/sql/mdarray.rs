//! Arrays of more than one dimension, and with lower bounds other than 1, as Postgres has them. An array is kept as
//! its text (`{{1,2},{3,4}}`, `[0:1]={7,8}`: see `elem_text`); `Arr` is that text read into its dimensions and its
//! elements, in row-major order, and written back, for what works by dimension: subscripts and slices (and their
//! assignment), array_dims and its kin, array_cat, array_fill, ARRAY[...] of arrays and array_agg of arrays.

use super::{SqlError, Type, Value, Violation, err};

type Result<T> = std::result::Result<T, SqlError>;

/// The most dimensions an array has (Postgres's MAXDIM).
pub(super) const MAXDIM: usize = 6;

/// One dimension of an array: where its subscripts start, and how many it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Dim {
    pub lo: i64,
    pub len: usize,
}

impl Dim {
    /// Its last subscript.
    pub fn hi(self) -> i64 {
        self.lo.saturating_add(self.len as i64).saturating_sub(1)
    }
}

/// An array: its dimensions, outermost first (none for an empty array), and its elements in row-major order (as
/// read, each its text or null; as built, any value), as many as the dimensions' lengths multiplied.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Arr {
    pub dims: Vec<Dim>,
    pub elems: Vec<Value>,
}

/// 54000 for an array of `n` dimensions, past MAXDIM.
pub(super) fn too_many_dims(n: usize) -> SqlError {
    SqlError {
        code: "54000",
        message: format!("number of array dimensions ({n}) exceeds the maximum allowed ({MAXDIM})"),
        info: None,
    }
}

/// An error with Postgres's DETAIL.
pub(super) fn detailed(code: &'static str, message: &str, detail: &str) -> SqlError {
    SqlError {
        code,
        message: message.into(),
        info: Some(Box::new(Violation { detail: Some(detail.into()), ..Violation::default() })),
    }
}

fn malformed(text: &str, detail: &str) -> SqlError {
    detailed("22P02", &format!("malformed array literal: \"{text}\""), detail)
}

/// Whitespace as Postgres's array input reads it (ASCII's only, whatever the locale).
fn space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}')
}

/// A bound as C's atoi reads it: a sign, then digits up to the first that isn't one (an int4's range).
fn atoi(s: &[char]) -> i64 {
    let (neg, digits) = match s.first() {
        Some('-') => (true, &s[1..]),
        Some('+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut n: i64 = 0;
    for c in digits.iter().take_while(|c| c.is_ascii_digit()) {
        n = n.saturating_mul(10).saturating_add(i64::from(*c as u8 - b'0'));
    }
    // (as Postgres 16 reads one: strtol's long, saturated, then kept in an int, so past int4 it wraps)
    i64::from((if neg { -n } else { n }) as i32)
}

/// An array value read: a JSON array (of one dimension), an int2vector's or oidvector's text (`1 2`, from 0), or
/// an array's text (see `Arr::parse`; text that doesn't parse is read as one dimension the way Chronos always read
/// it, which kept values tolerate). None for null; 22P02 for anything else.
pub(super) fn read(v: &Value) -> Result<Option<Arr>> {
    match v {
        Value::Null => Ok(None),
        Value::Json(serde_json::Value::Array(a)) => Ok(Some(Arr::one(a.iter().map(Value::from_json).collect()))),
        Value::Text(t) if super::is_vector_text(t.trim()) => {
            let items: Vec<Value> = t.split(' ').filter(|n| !n.is_empty()).map(|n| Value::Text(n.into())).collect();
            let mut a = Arr::one(items);
            if let Some(d) = a.dims.first_mut() {
                d.lo = 0;
            }
            Ok(Some(a))
        }
        Value::Text(t) => match Arr::parse(t) {
            Ok(a) => Ok(Some(a)),
            // (bounds written out, `[0:1]=`, are meant: they aren't read leniently)
            Err(e) if t.trim_start().starts_with('[') => Err(e),
            Err(e) => match super::lenient_items(t) {
                Ok(items) => Ok(Some(Arr::one(items))),
                Err(_) => Err(e),
            },
        },
        _ => err("22P02", "an array is a JSON array or a literal like '{1,2,3}'"),
    }
}

/// Text cast to an array of `elem`, as Postgres's array_in reads it and array_out writes it back: 22P02 for what
/// isn't an array's text; each element read as its type where Chronos keeps that type's values apart from their
/// text (numbers, booleans, times), so `'{1, 02}'::int[]` is `{1,2}` and `'{1,zed}'::int[]` is an error. An
/// int2vector's or oidvector's text stays as it is.
pub(super) fn normalize(text: &str, elem: Option<Type>) -> Result<String> {
    if super::is_vector_text(text.trim()) {
        return Ok(text.to_string());
    }
    let mut a = Arr::parse(text)?;
    // (a real is kept as the double its float4 is, which would print with all its digits)
    let typed = elem.filter(|t| {
        matches!(
            t,
            Type::SmallInt
                | Type::Int
                | Type::BigInt
                | Type::Float
                | Type::Numeric
                | Type::Bool
                | Type::Date
                | Type::Timestamp
                | Type::TimestampTz
                | Type::Interval
                | Type::Time
                | Type::Uuid
        )
    });
    if let Some(t) = typed {
        for v in &mut a.elems {
            if let Value::Text(_) = v {
                *v = super::coerce(std::mem::take(v), t)?;
            }
        }
    }
    Ok(a.text())
}

impl Arr {
    /// An empty array.
    pub fn empty() -> Arr {
        Arr { dims: vec![], elems: vec![] }
    }

    /// An array of one dimension from 1 (empty for no elements).
    pub fn one(elems: Vec<Value>) -> Arr {
        let dims = if elems.is_empty() { vec![] } else { vec![Dim { lo: 1, len: elems.len() }] };
        Arr { dims, elems }
    }

    /// Its text as a value.
    pub fn value(&self) -> Value {
        Value::Text(self.text())
    }

    /// Its text, as Postgres's array_out writes it: `{}` for an empty array, the bounds first (`[0:1][1:2]=`)
    /// where any dimension doesn't start at 1.
    pub fn text(&self) -> String {
        if self.dims.is_empty() || self.elems.is_empty() {
            return "{}".into();
        }
        let mut out = String::new();
        if self.dims.iter().any(|d| d.lo != 1) {
            for d in &self.dims {
                out += &format!("[{}:{}]", d.lo, d.hi());
            }
            out.push('=');
        }
        let mut k = 0;
        self.level(0, &mut k, &mut out);
        out
    }

    fn level(&self, d: usize, k: &mut usize, out: &mut String) {
        out.push('{');
        for j in 0..self.dims[d].len {
            if j > 0 {
                out.push(',');
            }
            if d + 1 < self.dims.len() {
                self.level(d + 1, k, out);
            } else {
                out.push_str(&self.elems.get(*k).map_or_else(|| "NULL".into(), super::elem_text));
                *k += 1;
            }
        }
        out.push('}');
    }

    /// array_dims: `[1:2][0:3]`.
    pub fn dims_text(&self) -> String {
        self.dims.iter().map(|d| format!("[{}:{}]", d.lo, d.hi())).collect()
    }

    /// An array's text read as Postgres's array_in reads it: optional bounds (`[1:2][0:1]=`, or `[2]` for
    /// `[1:2]`) that must match the braces, then the braces, whose sub-arrays must all have the same dimensions;
    /// elements unquoted (outer whitespace dropped, `NULL` in any case a null) or quoted, `\` escaping a character.
    /// 22P02 with Postgres's DETAIL for anything else.
    pub fn parse(text: &str) -> Result<Arr> {
        let c: Vec<char> = text.chars().collect();
        let mut i = 0;
        let mut given: Vec<Dim> = vec![];
        loop {
            while c.get(i).is_some_and(|&ch| space(ch)) {
                i += 1;
            }
            if c.get(i) != Some(&'[') {
                break;
            }
            i += 1;
            if given.len() >= MAXDIM {
                return Err(too_many_dims(given.len() + 1));
            }
            let number = |i: &mut usize| -> Option<i64> {
                let start = *i;
                while c.get(*i).is_some_and(|ch| ch.is_ascii_digit() || *ch == '-' || *ch == '+') {
                    *i += 1;
                }
                (*i > start).then(|| atoi(&c[start..*i]))
            };
            let Some(first) = number(&mut i) else {
                return Err(malformed(text, "\"[\" must introduce explicitly-specified array dimensions."));
            };
            let (lo, hi) = if c.get(i) == Some(&':') {
                i += 1;
                match number(&mut i) {
                    Some(hi) => (first, hi),
                    None => return Err(malformed(text, "Missing array dimension value.")),
                }
            } else {
                (1, first)
            };
            if c.get(i) != Some(&']') {
                return Err(malformed(text, "Missing \"]\" after array dimensions."));
            }
            i += 1;
            if hi < lo {
                return err("2202E", "upper bound cannot be less than lower bound");
            }
            given.push(Dim { lo, len: (hi - lo + 1) as usize });
        }
        if given.is_empty() {
            if c.get(i) != Some(&'{') {
                return Err(malformed(text, "Array value must start with \"{\" or dimension information."));
            }
        } else {
            if c.get(i) != Some(&'=') {
                return Err(malformed(text, "Missing \"=\" after array dimensions."));
            }
            i += 1;
            while c.get(i).is_some_and(|&ch| space(ch)) {
                i += 1;
            }
            if c.get(i) != Some(&'{') {
                return Err(malformed(text, "Array contents must start with \"{\"."));
            }
        }
        // (what the braces' errors show is the text from its `{`, as Postgres shows it)
        let shown: String = c[i..].iter().collect();
        let (lens, elems) = braces(&shown, &c[i..])?;
        let dims = if given.is_empty() {
            lens.into_iter().map(|len| Dim { lo: 1, len }).collect()
        } else {
            if lens.len() != given.len() || lens.iter().zip(&given).any(|(n, d)| *n != d.len) {
                return Err(malformed(text, "Specified array dimensions do not match array contents."));
            }
            // (as ArrayCheckBounds: a dimension's lower bound plus its length must fit an int)
            if let Some(d) = given.iter().find(|d| i32::try_from(d.lo + d.len as i64).is_err()) {
                return err("54000", format!("array lower bound is too large: {}", d.lo));
            }
            given
        };
        Ok(Arr { dims, elems })
    }

    /// Where the element at `at` (a subscript for each dimension, in range) is among the elements.
    fn offset(&self, at: &[i64]) -> usize {
        self.dims.iter().zip(at).fold(0, |off, (d, &i)| off * d.len + (i - d.lo) as usize)
    }

    /// a[i][j]...: the element at `at`, one subscript for each dimension; null for one out of range, or for more or
    /// fewer subscripts than dimensions (as Postgres's array_get_element).
    pub fn get(&self, at: &[i64]) -> Value {
        if at.is_empty() || at.len() != self.dims.len() {
            return Value::Null;
        }
        if self.dims.iter().zip(at).any(|(d, &i)| i < d.lo || i > d.hi()) {
            return Value::Null;
        }
        self.elems.get(self.offset(at)).cloned().unwrap_or(Value::Null)
    }

    /// The offsets of a region's elements in order: each dimension's first offset from its start, and how many.
    fn offsets(&self, span: &[(usize, usize)]) -> Vec<usize> {
        fn walk(dims: &[Dim], span: &[(usize, usize)], d: usize, base: usize, out: &mut Vec<usize>) {
            let (from, n) = span[d];
            for j in from..from + n {
                let at = base * dims[d].len + j;
                if d + 1 == dims.len() {
                    out.push(at);
                } else {
                    walk(dims, span, d + 1, at, out);
                }
            }
        }
        let mut out = vec![];
        if !span.is_empty() && span.len() == self.dims.len() {
            walk(&self.dims, span, 0, 0, &mut out);
        }
        out
    }

    /// a[lo:hi][lo:hi]...: the part from each range's `lo` to its `hi` (i64::MIN and i64::MAX for a bound left
    /// out), as far as the array goes, with the dimensions past the ranges taken whole, as an array of its own from
    /// 1; empty where a range is, or there are more ranges than dimensions (as Postgres's array_get_slice).
    pub fn slice(&self, ranges: &[(i64, i64)]) -> Arr {
        if self.dims.is_empty() || ranges.len() > self.dims.len() {
            return Arr::empty();
        }
        let mut span = vec![];
        for (k, d) in self.dims.iter().enumerate() {
            let (lo, hi) = ranges.get(k).copied().unwrap_or((i64::MIN, i64::MAX));
            let (lo, hi) = (lo.max(d.lo), hi.min(d.hi()));
            if lo > hi {
                return Arr::empty();
            }
            span.push(((lo - d.lo) as usize, (hi - lo + 1) as usize));
        }
        let elems =
            self.offsets(&span).into_iter().map(|i| self.elems.get(i).cloned().unwrap_or(Value::Null)).collect();
        Arr { dims: span.iter().map(|&(_, len)| Dim { lo: 1, len }).collect(), elems }
    }

    /// One dimension grown to take `lo..=hi`, with nulls where nothing was.
    fn grow(&mut self, lo: i64, hi: i64) -> Result<()> {
        let d = self.dims[0];
        if lo < d.lo {
            let add = usize::try_from(i128::from(d.lo) - i128::from(lo)).unwrap_or(usize::MAX);
            super::funcs::fits(add.checked_add(self.elems.len()).and_then(|n| n.checked_mul(super::funcs::ITEM)))?;
            let mut grown = vec![Value::Null; add];
            grown.append(&mut self.elems);
            self.elems = grown;
            self.dims[0] = Dim { lo, len: d.len + add };
        }
        let d = self.dims[0];
        if hi > d.hi() {
            let add = usize::try_from(i128::from(hi) - i128::from(d.hi())).unwrap_or(usize::MAX);
            super::funcs::fits(add.checked_add(self.elems.len()).and_then(|n| n.checked_mul(super::funcs::ITEM)))?;
            self.elems.resize(self.elems.len() + add, Value::Null);
            self.dims[0].len += add;
        }
        Ok(())
    }

    /// a[i][j]... = v (as Postgres's array_set_element): an empty array becomes one of as many dimensions as
    /// subscripts, each of length 1 from its subscript; one dimension grows to take a subscript past either end
    /// (with nulls between); more dimensions don't (2202E), and there must be a subscript for each.
    pub fn set(mut self, at: &[i64], v: Value) -> Result<Arr> {
        if at.is_empty() || at.len() > MAXDIM {
            return err("2202E", "wrong number of array subscripts");
        }
        if self.dims.is_empty() {
            return Ok(Arr { dims: at.iter().map(|&lo| Dim { lo, len: 1 }).collect(), elems: vec![v] });
        }
        if at.len() != self.dims.len() {
            return err("2202E", "wrong number of array subscripts");
        }
        if self.dims.len() == 1 {
            self.grow(at[0], at[0])?;
        } else if self.dims.iter().zip(at).any(|(d, &i)| i < d.lo || i > d.hi()) {
            return err("2202E", "array subscript out of range");
        }
        let k = self.offset(at);
        self.elems[k] = v;
        Ok(self)
    }

    /// a[lo:hi][lo:hi]... = src (as Postgres's array_set_slice): the slice's elements made `src`'s, in order, each
    /// range from `lo` to `hi` (i64::MIN and i64::MAX for a bound left out: the array's own). An empty array becomes
    /// the slice (whose bounds must then be given); one dimension grows to take it; more don't, and the dimensions
    /// past the ranges are taken whole. 2202E where `src` has too few elements.
    pub fn set_slice(mut self, ranges: &[(i64, i64)], src: &[Value]) -> Result<Arr> {
        if ranges.is_empty() || ranges.len() > MAXDIM {
            return err("2202E", "wrong number of array subscripts");
        }
        let too_small = || err("2202E", "source array too small");
        let backwards = || err("2202E", "upper bound cannot be less than lower bound");
        if self.dims.is_empty() {
            let mut dims = vec![];
            for &(lo, hi) in ranges {
                if lo == i64::MIN || hi == i64::MAX {
                    return Err(detailed(
                        "2202E",
                        "array slice subscript must provide both boundaries",
                        "When assigning to a slice of an empty array value, slice boundaries must be fully specified.",
                    ));
                }
                if hi < lo {
                    return backwards();
                }
                dims.push(Dim { lo, len: usize::try_from(i128::from(hi) - i128::from(lo) + 1).unwrap_or(usize::MAX) });
            }
            let n = dims.iter().try_fold(1usize, |n, d| n.checked_mul(d.len));
            let n = super::funcs::fits(n.and_then(|n| n.checked_mul(super::funcs::ITEM)))? / super::funcs::ITEM;
            if src.len() < n {
                return too_small();
            }
            return Ok(Arr { dims, elems: src[..n].to_vec() });
        }
        if ranges.len() > self.dims.len() {
            return err("2202E", "wrong number of array subscripts");
        }
        if self.dims.len() == 1 {
            let d = self.dims[0];
            let (lo, hi) = ranges[0];
            let lo = if lo == i64::MIN { d.lo } else { lo };
            let hi = if hi == i64::MAX { d.hi() } else { hi };
            if lo > hi {
                return backwards();
            }
            let want = usize::try_from(i128::from(hi) - i128::from(lo) + 1).unwrap_or(usize::MAX);
            if src.len() < want {
                return too_small();
            }
            self.grow(lo, hi)?;
            let from = (lo - self.dims[0].lo) as usize;
            for (k, x) in src.iter().take(want).enumerate() {
                self.elems[from + k] = x.clone();
            }
            return Ok(self);
        }
        let mut span = vec![];
        for (k, d) in self.dims.iter().enumerate() {
            let (lo, hi) = match ranges.get(k) {
                Some(&(lo, hi)) => (if lo == i64::MIN { d.lo } else { lo }, if hi == i64::MAX { d.hi() } else { hi }),
                None => (d.lo, d.hi()),
            };
            if lo > hi {
                return backwards();
            }
            if k < ranges.len() && (lo < d.lo || hi > d.hi()) {
                return err("2202E", "array subscript out of range");
            }
            span.push(((lo - d.lo) as usize, (hi - lo + 1) as usize));
        }
        let at = self.offsets(&span);
        if src.len() < at.len() {
            return too_small();
        }
        for (k, i) in at.into_iter().enumerate() {
            self.elems[i] = src[k].clone();
        }
        Ok(self)
    }
}

/// array_cat (and `||` of two arrays), as Postgres joins them: an empty one gives the other; two of the same
/// dimensions are joined along the first (the rest must match, bounds too), keeping the first's bounds; one of a
/// dimension less is put on as one more row of the other. 2202E for any other pair.
pub(super) fn cat(x: Arr, y: Arr) -> Result<Arr> {
    let (n1, n2) = (x.dims.len(), y.dims.len());
    if n1 == 0 {
        return Ok(y);
    }
    if n2 == 0 {
        return Ok(x);
    }
    let incompatible = |detail: &str| Err(detailed("2202E", "cannot concatenate incompatible arrays", detail));
    let differing = "Arrays with differing dimensions are not compatible for concatenation.";
    let dims = if n1 == n2 {
        if x.dims[1..] != y.dims[1..] {
            return incompatible("Arrays with differing element dimensions are not compatible for concatenation.");
        }
        let mut dims = x.dims.clone();
        dims[0].len += y.dims[0].len;
        dims
    } else if n1 + 1 == n2 {
        if x.dims[..] != y.dims[1..] {
            return incompatible(differing);
        }
        let mut dims = y.dims.clone();
        dims[0].len += 1;
        dims
    } else if n2 + 1 == n1 {
        if y.dims[..] != x.dims[1..] {
            return incompatible(differing);
        }
        let mut dims = x.dims.clone();
        dims[0].len += 1;
        dims
    } else {
        let detail = format!("Arrays of {n1} and {n2} dimensions are not compatible for concatenation.");
        return incompatible(detail.as_str());
    };
    let mut elems = x.elems;
    elems.extend(y.elems);
    Ok(Arr { dims, elems })
}

/// ARRAY[x, y, ...] of arrays: one of a dimension more, each of them a row (as Postgres's ARRAY[] of sub-arrays):
/// all must have the same dimensions and bounds; null and empty ones are left out if all are, else 2202E.
pub(super) fn nest(parts: &[Value]) -> Result<Arr> {
    let (mut rows, mut empty): (Vec<Arr>, bool) = (vec![], false);
    let mismatch = || err("2202E", "multidimensional arrays must have array expressions with matching dimensions");
    for v in parts {
        match read(v)? {
            Some(x) if !x.dims.is_empty() => {
                match rows.first() {
                    None if x.dims.len() + 1 > MAXDIM => return Err(too_many_dims(x.dims.len() + 1)),
                    Some(f) if f.dims != x.dims => return mismatch(),
                    _ => {}
                }
                rows.push(x);
            }
            _ => empty = true,
        }
    }
    let Some(first) = rows.first() else { return Ok(Arr::empty()) };
    if empty {
        return mismatch();
    }
    let mut dims = vec![Dim { lo: 1, len: rows.len() }];
    dims.extend(first.dims.iter().copied());
    Ok(Arr { dims, elems: rows.into_iter().flat_map(|r| r.elems).collect() })
}

/// array_agg of arrays: one of a dimension more, each of them a row (as Postgres's accumArrayResultArr): none may be
/// null (22004) or empty, and all must have the same dimensions and bounds (2202E).
pub(super) fn accumulate(parts: &[Value]) -> Result<Arr> {
    let mut rows: Vec<Arr> = vec![];
    for v in parts {
        let Some(x) = read(v)? else { return err("22004", "cannot accumulate null arrays") };
        match rows.first() {
            None if x.dims.is_empty() => return err("2202E", "cannot accumulate empty arrays"),
            None if x.dims.len() + 1 > MAXDIM => return Err(too_many_dims(x.dims.len() + 1)),
            Some(f) if f.dims != x.dims => return err("2202E", "cannot accumulate arrays of different dimensionality"),
            _ => {}
        }
        rows.push(x);
    }
    let Some(first) = rows.first() else { return Ok(Arr::empty()) };
    let mut dims = vec![Dim { lo: 1, len: rows.len() }];
    dims.extend(first.dims.iter().copied());
    Ok(Arr { dims, elems: rows.into_iter().flat_map(|r| r.elems).collect() })
}

/// array_fill(v, dims [, lower bounds]): an array of those dimensions, every element `v` (as Postgres's
/// array_fill): the dimensions and bounds are arrays of one dimension with no nulls, as many bounds as dimensions.
pub(super) fn fill(v: &Value, dims: &Value, lbs: Option<&Value>) -> Result<Arr> {
    let null = || err("22004", "dimension array or low bound array cannot be null");
    let Some(dims) = read(dims)? else { return null() };
    let lbs = match lbs {
        None => None,
        Some(l) => match read(l)? {
            Some(l) => Some(l),
            None => return null(),
        },
    };
    let numbers = |x: &Arr| -> Result<Vec<i64>> {
        if x.dims.len() > 1 {
            return Err(detailed(
                "2202E",
                "wrong number of array subscripts",
                "Dimension array must be one dimensional.",
            ));
        }
        x.elems
            .iter()
            .map(|v| match v {
                Value::Null => err("22004", "dimension values cannot be null"),
                v => match super::coerce(v.clone(), Type::Int)? {
                    Value::Int(n) => Ok(n),
                    _ => err("22P02", "a dimension is an integer"),
                },
            })
            .collect()
    };
    let lens = numbers(&dims)?;
    if lens.len() > MAXDIM {
        return Err(too_many_dims(lens.len()));
    }
    let lows = match lbs {
        None => vec![1; lens.len()],
        Some(l) => {
            let l = numbers(&l)?;
            if l.len() != lens.len() {
                return Err(detailed(
                    "2202E",
                    "wrong number of array subscripts",
                    "Low bound array has different size than dimensions array.",
                ));
            }
            l
        }
    };
    if lens.iter().any(|n| *n < 0) {
        return err("54000", "array size exceeds the maximum allowed");
    }
    let total = lens.iter().try_fold(1usize, |n, &d| n.checked_mul(d as usize));
    let total = super::funcs::fits(total.and_then(|n| n.checked_mul(super::funcs::ITEM)))? / super::funcs::ITEM;
    if lens.is_empty() || total == 0 {
        return Ok(Arr::empty());
    }
    let dims = lens.iter().zip(&lows).map(|(&len, &lo)| Dim { lo, len: len as usize }).collect();
    Ok(Arr { dims, elems: vec![v.clone(); total] })
}

/// The braces of an array's text from its `{` (`c`; `shown` in errors, as Postgres shows it): each dimension's
/// length (none for `{}`) and the elements in order, as Postgres's ArrayCount and ReadArrayStr read them, with
/// their errors' DETAILs.
fn braces(shown: &str, c: &[char]) -> Result<(Vec<usize>, Vec<Value>)> {
    // where the reading is, as ArrayCount's states say
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        NoLevel,
        LevelStarted,
        ElemStarted,
        QuotedStarted,
        QuotedDone,
        LevelDone,
        ElemDelimited,
        LevelDelimited,
    }
    use St::*;
    const MATCHING: &str = "Multidimensional arrays must have sub-arrays with matching dimensions.";
    let bad = |detail: &str| malformed(shown, detail);
    let (mut st, mut quoted, mut in_quotes) = (NoLevel, false, false);
    // the items so far of each array open; each depth's length, from the first array closed there; the depth the
    // elements are at
    let (mut counts, mut lens, mut leaf): (Vec<usize>, Vec<Option<usize>>, Option<usize>) = (vec![], vec![], None);
    // the element so far, and its length up to its last character that counts (trailing whitespace doesn't)
    let (mut out, mut cur, mut keep) = (Vec::<Value>::new(), String::new(), 0usize);
    let mut i = 0;
    loop {
        let Some(&ch) = c.get(i) else { return Err(bad("Unexpected end of input.")) };
        i += 1;
        if in_quotes {
            match ch {
                '\\' => match c.get(i) {
                    Some(&n) => {
                        cur.push(n);
                        i += 1;
                    }
                    None => return Err(bad("Unexpected end of input.")),
                },
                '"' => {
                    in_quotes = false;
                    st = QuotedDone;
                    keep = cur.len();
                }
                ch => cur.push(ch),
            }
            continue;
        }
        let element = matches!(st, ElemStarted | QuotedDone) && matches!(ch, ',' | '}');
        match ch {
            '\\' => {
                if !matches!(st, LevelStarted | ElemStarted | ElemDelimited) {
                    return Err(bad("Unexpected \"\\\" character."));
                }
                let Some(&n) = c.get(i) else { return Err(bad("Unexpected end of input.")) };
                i += 1;
                cur.push(n);
                keep = cur.len();
                (st, quoted) = (ElemStarted, true);
            }
            '"' => {
                if !matches!(st, LevelStarted | ElemDelimited) {
                    return Err(bad("Unexpected array element."));
                }
                (st, quoted, in_quotes) = (QuotedStarted, true, true);
            }
            '{' => {
                if !matches!(st, NoLevel | LevelStarted | LevelDelimited) {
                    return Err(bad("Unexpected \"{\" character."));
                }
                if counts.len() >= MAXDIM {
                    return Err(too_many_dims(counts.len() + 1));
                }
                counts.push(0);
                if lens.len() < counts.len() {
                    lens.push(None);
                }
                st = LevelStarted;
            }
            '}' => {
                if !(matches!(st, ElemStarted | QuotedDone | LevelDone) || (counts.len() == 1 && st == LevelStarted)) {
                    return Err(bad("Unexpected \"}\" character."));
                }
            }
            ',' => {
                if !matches!(st, ElemStarted | QuotedDone | LevelDone) {
                    return Err(bad("Unexpected \",\" character."));
                }
                st = if st == LevelDone { LevelDelimited } else { ElemDelimited };
            }
            ch if space(ch) => {
                if matches!(st, ElemStarted | QuotedDone) {
                    cur.push(ch);
                }
            }
            ch => {
                if !matches!(st, LevelStarted | ElemStarted | ElemDelimited) {
                    return Err(bad("Unexpected array element."));
                }
                cur.push(ch);
                keep = cur.len();
                st = ElemStarted;
            }
        }
        if element {
            // an element ends at its `,` or `}`: at the depth all the others are at
            let depth = counts.len();
            if leaf.is_some_and(|l| l != depth) {
                return Err(bad(MATCHING));
            }
            leaf = Some(depth);
            cur.truncate(keep);
            let t = std::mem::take(&mut cur);
            out.push(if !quoted && t.eq_ignore_ascii_case("null") { Value::Null } else { Value::Text(t) });
            (quoted, keep) = (false, 0);
            if let Some(k) = counts.last_mut() {
                *k += 1;
            }
            // (each element a value besides its text: a gigabyte of commas would be far more)
            super::funcs::fits(
                out.len().checked_mul(std::mem::size_of::<Value>()).and_then(|n| n.checked_add(c.len())),
            )?;
        }
        if ch == '}' {
            let n = counts.pop().unwrap_or(0);
            let d = counts.len();
            match lens.get(d).copied().flatten() {
                Some(l) if l != n => return Err(bad(MATCHING)),
                _ => {
                    if let Some(slot) = lens.get_mut(d) {
                        *slot = Some(n);
                    }
                }
            }
            st = LevelDone;
            match counts.last_mut() {
                Some(k) => *k += 1,
                None => break,
            }
        }
    }
    if c[i..].iter().any(|&ch| !space(ch)) {
        return Err(bad("Junk after closing right brace."));
    }
    if out.is_empty() {
        return Ok((vec![], out));
    }
    Ok((lens.into_iter().map(|l| l.unwrap_or(0)).collect(), out))
}
