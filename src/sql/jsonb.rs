//! jsonb as Postgres has it: its text form, `->` / `->>` / `#>` / `#>>` lookups, `@>`
//! containment and `||`.

use serde_json::Value as Json;

/// jsonb's text form, as Postgres prints it: `{"a": 1, "bb": [1, 2]}`, object keys shorter first,
/// then by bytes (the order jsonb stores them in).
pub(super) fn text(j: &Json) -> String {
    let mut out = String::new();
    write(j, &mut out);
    out
}

fn write(j: &Json, out: &mut String) {
    match j {
        Json::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(x, out);
            }
            out.push(']');
        }
        Json::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&Json::String(k.clone()).to_string());
                out.push_str(": ");
                write(&o[k], out);
            }
            out.push('}');
        }
        j => out.push_str(&j.to_string()),
    }
}

/// How deep jsonb may nest, in arrays and objects: serde_json reads nothing deeper than 128
/// levels (so a stored row holding one couldn't be read back), and cloning, printing and
/// dropping a value recurse.
const MAX_DEPTH: usize = 100;

/// `j`, if it nests no deeper than MAX_DEPTH: what a value built from others (which may already
/// be that deep) is checked with.
pub(super) fn shallow(j: Json) -> super::Result<Json> {
    fn deeper(j: &Json, n: usize) -> bool {
        match j {
            Json::Array(a) => n == 0 || a.iter().any(|x| deeper(x, n - 1)),
            Json::Object(o) => n == 0 || o.values().any(|x| deeper(x, n - 1)),
            _ => false,
        }
    }
    if deeper(&j, MAX_DEPTH) {
        return super::err(
            "54000",
            format!("jsonb value too deep: arrays and objects nest more than {MAX_DEPTH} levels"),
        );
    }
    Ok(j)
}

/// `j -> key`: an object's field, or an array's element (negative counts from the end).
pub(super) fn get<'a>(j: &'a Json, key: &Key) -> Option<&'a Json> {
    match (j, key) {
        (Json::Object(o), Key::Name(k)) => o.get(k),
        (Json::Array(a), Key::Index(i)) => {
            let i = if *i < 0 { a.len() as i64 + i } else { *i };
            usize::try_from(i).ok().and_then(|i| a.get(i))
        }
        _ => None,
    }
}

pub(super) enum Key {
    Name(String),
    Index(i64),
}

/// `j #> path`: each step a field name, or an array index written as a number.
pub(super) fn path<'a>(j: &'a Json, steps: &[String]) -> Option<&'a Json> {
    steps.iter().try_fold(j, |j, s| match j {
        Json::Array(_) => get(j, &Key::Index(s.trim().parse().ok()?)),
        _ => get(j, &Key::Name(s.clone())),
    })
}

/// `->>`'s text: a string without its quotes, anything else as jsonb text; JSON null is NULL.
pub(super) fn as_text(j: &Json) -> Option<String> {
    match j {
        Json::Null => None,
        Json::String(s) => Some(s.clone()),
        j => Some(text(j)),
    }
}

/// `a @> b`: every part of b is in a. Objects hold b's keys with values that contain b's;
/// arrays hold, for each of b's elements, one that contains it; scalars are equal. A top-level
/// array also contains a scalar it has.
pub(super) fn contains(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Object(x), Json::Object(y)) => y.iter().all(|(k, v)| x.get(k).is_some_and(|w| contains(w, v))),
        (Json::Array(x), Json::Array(y)) => y.iter().all(|v| x.iter().any(|w| contains(w, v))),
        (Json::Array(x), y) if !y.is_object() => x.iter().any(|w| w == y),
        (x, y) => numbers_equal(x, y) || x == y,
    }
}

fn numbers_equal(a: &Json, b: &Json) -> bool {
    matches!((a.as_f64(), b.as_f64()), (Some(x), Some(y)) if a.is_number() && b.is_number() && x == y)
}

/// `a || b`: objects merge (b's keys win), arrays concatenate, and anything else joins an array.
pub(super) fn concat(a: &Json, b: &Json) -> Json {
    match (a, b) {
        (Json::Object(x), Json::Object(y)) => {
            let mut o = x.clone();
            o.extend(y.iter().map(|(k, v)| (k.clone(), v.clone())));
            Json::Object(o)
        }
        (x, y) => {
            let items = |j: &Json| match j {
                Json::Array(a) => a.clone(),
                j => vec![j.clone()],
            };
            Json::Array([items(x), items(y)].concat())
        }
    }
}

/// `jsonb_set(j, path, value, create)`: `value` put at `path`, if its parent exists; the last
/// step added when `create` and it isn't there. A path's steps are None where its element is null
/// (22004 where the path reaches one); a scalar can't have a path set in it.
pub(super) fn set(j: &Json, steps: &[Option<String>], value: &Json, create: bool) -> super::Result<Json> {
    if !(j.is_object() || j.is_array()) {
        return super::err("22023", "cannot set path in scalar");
    }
    if steps.is_empty() {
        return Ok(j.clone());
    }
    set_at(j, steps, 0, value, create)
}

/// Step `level` of a path, which the walk reached: 22004 if it is null, as Postgres counts it from 1.
fn step_at(steps: &[Option<String>], level: usize) -> super::Result<&str> {
    match &steps[level] {
        Some(s) => Ok(s),
        None => super::err("22004", format!("path element at position {} is null", level + 1)),
    }
}

/// Step `level` of a path read as an array's index, as Postgres's strtoint reads it: 22P02 for one that isn't.
fn path_index(step: &str, level: usize) -> super::Result<i64> {
    match step.trim_start().parse::<i32>() {
        Ok(i) => Ok(i64::from(i)),
        Err(_) => super::err("22P02", format!("path element at position {} is not an integer: \"{step}\"", level + 1)),
    }
}

fn set_at(j: &Json, steps: &[Option<String>], level: usize, value: &Json, create: bool) -> super::Result<Json> {
    let step = step_at(steps, level)?;
    let last = level + 1 == steps.len();
    let mut j = j.clone();
    match &mut j {
        Json::Object(o) => match o.get(step) {
            Some(_) if last => {
                o.insert(step.to_string(), value.clone());
            }
            Some(child) => {
                let v = set_at(child, steps, level + 1, value, create)?;
                o.insert(step.to_string(), v);
            }
            None if last && create => {
                o.insert(step.to_string(), value.clone());
            }
            None => {}
        },
        Json::Array(a) => {
            let i = path_index(step, level)?;
            let len = a.len() as i64;
            let at = if i < 0 { len + i } else { i };
            match usize::try_from(at).ok().filter(|&at| at < a.len()) {
                Some(at) if last => a[at] = value.clone(),
                Some(at) => a[at] = set_at(&a[at], steps, level + 1, value, create)?,
                None if last && create => {
                    if at < 0 {
                        a.insert(0, value.clone())
                    } else {
                        a.push(value.clone())
                    }
                }
                None => {}
            }
        }
        _ => {}
    }
    Ok(j)
}

/// A number's text as Postgres's numeric prints it: no exponent, the scale it was written with
/// (`7.50`, `1.0e2` is `100`), no sign on zero.
pub(super) fn numeric_text(t: &str) -> String {
    let t = t.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (mant, exp) = match t.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().unwrap_or(0)),
        None => (t, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits = format!("{int}{frac}");
    // the decimal point sits this far from the digits' start
    let point = int.len() as i64 + exp;
    let scale = (frac.len() as i64 - exp).max(0) as usize;
    let mut out = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    };
    // the scale: pad or (never needed to) cut the fraction
    if scale == 0 {
        if let Some(i) = out.find('.') {
            out.truncate(i);
        }
    } else {
        let have = out.find('.').map_or(0, |i| out.len() - i - 1);
        if have < scale {
            if !out.contains('.') {
                out.push('.');
            }
            out.push_str(&"0".repeat(scale - have));
        }
    }
    let int_end = out.find('.').unwrap_or(out.len());
    let trimmed = out[..int_end].trim_start_matches('0');
    let rest = &out[int_end..];
    let out = format!("{}{}", if trimmed.is_empty() { "0" } else { trimmed }, rest);
    let zero = out.chars().all(|c| c == '0' || c == '.');
    if neg && !zero { format!("-{out}") } else { out }
}

/// Whether every number in `j` is one numeric holds (jsonb reads its numbers as numeric_in does:
/// past 10^131072, or 16383 places, "value overflows numeric format").
pub(super) fn numbers_fit(j: &Json) -> bool {
    match j {
        Json::Number(n) => super::decimal::Dec::parse_input(&n.to_string()) != Err(true),
        Json::Array(a) => a.iter().all(numbers_fit),
        Json::Object(o) => o.values().all(numbers_fit),
        _ => true,
    }
}

/// `j` with every number written as Postgres's numeric prints it.
pub(super) fn normalize(j: &mut Json) {
    match j {
        Json::Number(n) => {
            if let Ok(m) = numeric_text(&n.to_string()).parse::<serde_json::Number>() {
                *n = m;
            }
        }
        Json::Array(a) => a.iter_mut().for_each(normalize),
        Json::Object(o) => o.values_mut().for_each(normalize),
        _ => {}
    }
}

/// Step `level` of a path read as an array index: `-1` is the last element.
fn index_of(step: &str, level: usize, len: usize) -> super::Result<Option<usize>> {
    let i = path_index(step, level)?;
    let at = if i < 0 { len as i64 + i } else { i };
    Ok(usize::try_from(at).ok().filter(|at| *at < len))
}

/// `j - key`: an object without the field, or an array without the string elements equal to it.
pub(super) fn delete_key(j: &Json, key: &str) -> super::Result<Json> {
    match j {
        Json::Object(o) => {
            let mut o = o.clone();
            o.remove(key);
            Ok(Json::Object(o))
        }
        Json::Array(a) => Ok(Json::Array(a.iter().filter(|x| x.as_str() != Some(key)).cloned().collect())),
        _ => super::err("22023", "cannot delete from scalar"),
    }
}

/// `j - n`: an array without its nth element (from the end when negative).
pub(super) fn delete_index(j: &Json, n: i64) -> super::Result<Json> {
    match j {
        Json::Array(a) => {
            let mut a = a.clone();
            let at = if n < 0 { a.len() as i64 + n } else { n };
            if let Some(at) = usize::try_from(at).ok().filter(|at| *at < a.len()) {
                a.remove(at);
            }
            Ok(Json::Array(a))
        }
        Json::Object(_) => super::err("22023", "cannot delete from object using integer index"),
        _ => super::err("22023", "cannot delete from scalar"),
    }
}

/// `j #- path`: without what the path names; unchanged where it leads nowhere (or `j` is empty).
pub(super) fn delete_path(j: &Json, steps: &[Option<String>]) -> super::Result<Json> {
    match j {
        Json::Object(o) if o.is_empty() => Ok(j.clone()),
        Json::Array(a) if a.is_empty() => Ok(j.clone()),
        Json::Object(_) | Json::Array(_) if steps.is_empty() => Ok(j.clone()),
        Json::Object(_) | Json::Array(_) => delete_at(j, steps, 0),
        _ => super::err("22023", "cannot delete path in scalar"),
    }
}

fn delete_at(j: &Json, steps: &[Option<String>], level: usize) -> super::Result<Json> {
    let step = step_at(steps, level)?;
    let last = level + 1 == steps.len();
    match j {
        Json::Object(o) => {
            let mut o = o.clone();
            if last {
                o.remove(step);
            } else if let Some(child) = o.get(step) {
                let v = delete_at(child, steps, level + 1)?;
                o.insert(step.to_string(), v);
            }
            Ok(Json::Object(o))
        }
        Json::Array(a) => {
            let mut a = a.clone();
            if let Some(at) = index_of(step, level, a.len())? {
                if last {
                    a.remove(at);
                } else {
                    a[at] = delete_at(&a[at], steps, level + 1)?;
                }
            }
            Ok(Json::Array(a))
        }
        j => Ok(j.clone()),
    }
}

/// `jsonb_insert(j, path, value, after)`: `value` put at the path, before the element it names
/// (after it if `after`); a new field in an object, never over an existing one.
pub(super) fn insert(j: &Json, steps: &[Option<String>], value: &Json, after: bool) -> super::Result<Json> {
    if !(j.is_object() || j.is_array()) {
        return super::err("22023", "cannot set path in scalar");
    }
    if steps.is_empty() {
        return Ok(j.clone());
    }
    insert_at(j, steps, 0, value, after)
}

fn insert_at(j: &Json, steps: &[Option<String>], level: usize, value: &Json, after: bool) -> super::Result<Json> {
    let step = step_at(steps, level)?;
    let last = level + 1 == steps.len();
    match j {
        Json::Object(o) => {
            let mut o = o.clone();
            match o.get(step) {
                Some(_) if last => {
                    return Err(super::SqlError {
                        code: "22023",
                        message: "cannot replace existing key".into(),
                        info: Some(Box::new(super::Violation {
                            hint: Some("Try using the function jsonb_set to replace key value.".into()),
                            ..Default::default()
                        })),
                    });
                }
                Some(child) => {
                    let v = insert_at(child, steps, level + 1, value, after)?;
                    o.insert(step.to_string(), v);
                }
                None if last => {
                    o.insert(step.to_string(), value.clone());
                }
                None => {}
            }
            Ok(Json::Object(o))
        }
        Json::Array(a) => {
            let mut a = a.clone();
            let i = path_index(step, level)?;
            let len = a.len() as i64;
            let at = if i < 0 { len + i } else { i };
            if last {
                // past either end it goes on that end
                let place = (at + i64::from(after)).clamp(0, len) as usize;
                a.insert(place, value.clone());
            } else if let Some(at) = usize::try_from(at).ok().filter(|at| *at < a.len()) {
                a[at] = insert_at(&a[at], steps, level + 1, value, after)?;
            }
            Ok(Json::Array(a))
        }
        j => Ok(j.clone()),
    }
}

/// `jsonb_pretty`: four spaces a level, an element or field to a line.
pub(super) fn pretty(j: &Json) -> String {
    fn go(j: &Json, depth: usize, out: &mut String) {
        let pad = |out: &mut String, n: usize| out.push_str(&"    ".repeat(n));
        match j {
            Json::Array(a) => {
                out.push_str("[\n");
                for (i, x) in a.iter().enumerate() {
                    pad(out, depth + 1);
                    go(x, depth + 1, out);
                    out.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push(']');
            }
            Json::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
                out.push_str("{\n");
                for (i, k) in keys.iter().enumerate() {
                    pad(out, depth + 1);
                    out.push_str(&Json::String((*k).clone()).to_string());
                    out.push_str(": ");
                    go(&o[*k], depth + 1, out);
                    out.push_str(if i + 1 < keys.len() { ",\n" } else { "\n" });
                }
                pad(out, depth);
                out.push('}');
            }
            j => write(j, out),
        }
    }
    let mut out = String::new();
    go(j, 0, &mut out);
    out
}

/// jsonb without its object fields that are null, at every level.
pub(super) fn strip_nulls(j: &Json) -> Json {
    match j {
        Json::Object(o) => {
            Json::Object(o.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), strip_nulls(v))).collect())
        }
        Json::Array(a) => Json::Array(a.iter().map(strip_nulls).collect()),
        j => j.clone(),
    }
}

/// How deep JSON input may nest before its reading stops as Postgres's does when the stack runs out (its parser
/// recurses a level per array and object; past this much any max_stack_depth it may have is exceeded).
const INPUT_DEPTH: usize = 1000;

/// Why text isn't JSON, as Postgres's parser (src/common/jsonapi.c, de-escaping as jsonb's input does) finds it.
#[derive(Clone, Copy, PartialEq)]
enum Bad {
    EscapingInvalid,
    EscapingRequired,
    ExpectedEnd,
    ExpectedArrayNext,
    ExpectedColon,
    ExpectedJson,
    ExpectedMore,
    ExpectedObjectFirst,
    ExpectedObjectNext,
    ExpectedString,
    InvalidToken,
    CodePointZero,
    EscapeFormat,
    HighSurrogate,
    LowSurrogate,
    /// nested past `INPUT_DEPTH`
    Deep,
}

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Invalid,
    ObjStart,
    ObjEnd,
    ArrStart,
    ArrEnd,
    Comma,
    Colon,
    Str,
    Num,
    True,
    False,
    Null,
    End,
}

/// Where in the parse a token was wrong, for which error it is (Postgres's JsonParseContext).
#[derive(Clone, Copy)]
enum Want {
    Value,
    String,
    ArrayNext,
    ObjectStart,
    ObjectLabel,
    ObjectNext,
    End,
}

/// Postgres's JSON lexer over the input's bytes: the current token (`start`, None at the end, to `term`), and the
/// line it is on. `escapes`: strings are de-escaped (jsonb's input, which refuses `\u0000` and lone surrogates);
/// json's input only checks that each escape is well formed.
struct Lex<'a> {
    s: &'a [u8],
    start: Option<usize>,
    term: usize,
    tok: Tok,
    line: usize,
    line_start: usize,
    escapes: bool,
}

/// The characters a bare word or a number runs over (Postgres's JSON_ALPHANUMERIC_CHAR).
fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

impl Lex<'_> {
    /// Just past the character at `i` (a UTF-8 sequence's bytes, as many as there are).
    fn char_end(&self, i: usize) -> usize {
        let n = match self.s[i] {
            b if b < 0x80 => 1,
            b if b >= 0xF0 => 4,
            b if b >= 0xE0 => 3,
            b if b >= 0xC0 => 2,
            _ => 1,
        };
        (i + n).min(self.s.len())
    }

    fn fail(&mut self, term: usize, why: Bad) -> Result<(), Bad> {
        self.term = term;
        Err(why)
    }

    fn lex(&mut self) -> Result<(), Bad> {
        let (s, n) = (self.s, self.s.len());
        let mut p = self.term;
        while p < n && matches!(s[p], b' ' | b'\t' | b'\n' | b'\r') {
            if s[p] == b'\n' {
                self.line += 1;
                self.line_start = p + 1;
            }
            p += 1;
        }
        self.start = Some(p);
        if p >= n {
            self.start = None;
            self.term = p;
            self.tok = Tok::End;
            return Ok(());
        }
        let one = |l: &mut Self, t: Tok| -> Result<(), Bad> {
            l.term = p + 1;
            l.tok = t;
            Ok(())
        };
        match s[p] {
            b'{' => one(self, Tok::ObjStart),
            b'}' => one(self, Tok::ObjEnd),
            b'[' => one(self, Tok::ArrStart),
            b']' => one(self, Tok::ArrEnd),
            b',' => one(self, Tok::Comma),
            b':' => one(self, Tok::Colon),
            b'"' => {
                self.string(p)?;
                self.tok = Tok::Str;
                Ok(())
            }
            b'-' => {
                self.number(p + 1)?;
                self.tok = Tok::Num;
                Ok(())
            }
            b'0'..=b'9' => {
                self.number(p)?;
                self.tok = Tok::Num;
                Ok(())
            }
            _ => {
                let mut q = p;
                while q < n && alnum(s[q]) {
                    q += 1;
                }
                if q == p {
                    return self.fail(p + 1, Bad::InvalidToken);
                }
                self.term = q;
                self.tok = match &s[p..q] {
                    b"true" => Tok::True,
                    b"false" => Tok::False,
                    b"null" => Tok::Null,
                    _ => return Err(Bad::InvalidToken),
                };
                Ok(())
            }
        }
    }

    fn string(&mut self, start: usize) -> Result<(), Bad> {
        let (s, n) = (self.s, self.s.len());
        let mut i = start;
        let mut high: Option<u32> = None;
        loop {
            i += 1;
            if i >= n {
                return self.fail(i, Bad::InvalidToken);
            }
            match s[i] {
                b'"' => break,
                b'\\' => {
                    i += 1;
                    if i >= n {
                        return self.fail(i, Bad::InvalidToken);
                    }
                    if s[i] == b'u' {
                        let mut ch = 0u32;
                        for _ in 0..4 {
                            i += 1;
                            if i >= n {
                                return self.fail(i, Bad::InvalidToken);
                            }
                            match (s[i] as char).to_digit(16) {
                                Some(d) => ch = ch * 16 + d,
                                None => return self.fail(self.char_end(i), Bad::EscapeFormat),
                            }
                        }
                        if !self.escapes {
                            continue;
                        }
                        if (0xD800..=0xDBFF).contains(&ch) {
                            if high.is_some() {
                                return self.fail(self.char_end(i), Bad::HighSurrogate);
                            }
                            high = Some(ch);
                            continue;
                        } else if (0xDC00..=0xDFFF).contains(&ch) {
                            if high.take().is_none() {
                                return self.fail(self.char_end(i), Bad::LowSurrogate);
                            }
                            ch = 0x10000; // (a pair: some character past the first plane)
                        }
                        if high.is_some() {
                            return self.fail(self.char_end(i), Bad::LowSurrogate);
                        }
                        if ch == 0 {
                            return self.fail(self.char_end(i), Bad::CodePointZero);
                        }
                    } else {
                        if high.is_some() {
                            return self.fail(self.char_end(i), Bad::LowSurrogate);
                        }
                        if !b"\"\\/bfnrt".contains(&s[i]) {
                            // (the escape alone is the token reported)
                            self.start = Some(i);
                            return self.fail(self.char_end(i), Bad::EscapingInvalid);
                        }
                    }
                }
                _ => {
                    if high.is_some() {
                        return self.fail(self.char_end(i), Bad::LowSurrogate);
                    }
                    let mut p = i;
                    while p < n && s[p] != b'\\' && s[p] != b'"' {
                        if s[p] <= 31 {
                            return self.fail(p, Bad::EscapingRequired);
                        }
                        p += 1;
                    }
                    i = p - 1;
                }
            }
        }
        if high.is_some() {
            return self.fail(i + 1, Bad::LowSurrogate);
        }
        self.term = i + 1;
        Ok(())
    }

    /// A number from `i`, after any minus sign: digits without a leading zero, then maybe a fraction and an
    /// exponent; letters or digits run on after it make it one bad token.
    fn number(&mut self, mut i: usize) -> Result<(), Bad> {
        let (s, n) = (self.s, self.s.len());
        let digit = |i: usize| i < n && s[i].is_ascii_digit();
        let mut bad = false;
        if i < n && s[i] == b'0' {
            i += 1;
        } else if i < n && (b'1'..=b'9').contains(&s[i]) {
            while digit(i) {
                i += 1;
            }
        } else {
            bad = true;
        }
        if i < n && s[i] == b'.' {
            i += 1;
            if !digit(i) {
                bad = true;
            } else {
                while digit(i) {
                    i += 1;
                }
            }
        }
        if i < n && (s[i] == b'e' || s[i] == b'E') {
            i += 1;
            if i < n && (s[i] == b'+' || s[i] == b'-') {
                i += 1;
            }
            if !digit(i) {
                bad = true;
            } else {
                while digit(i) {
                    i += 1;
                }
            }
        }
        while i < n && alnum(s[i]) {
            i += 1;
            bad = true;
        }
        self.term = i;
        if bad { Err(Bad::InvalidToken) } else { Ok(()) }
    }

    /// The error a wrong token is where `want` was expected (Postgres's report_parse_error).
    fn wrong(&self, want: Want) -> Bad {
        if self.start.is_none() || self.tok == Tok::End {
            return Bad::ExpectedMore;
        }
        match want {
            Want::End => Bad::ExpectedEnd,
            Want::Value => Bad::ExpectedJson,
            Want::String => Bad::ExpectedString,
            Want::ArrayNext => Bad::ExpectedArrayNext,
            Want::ObjectStart => Bad::ExpectedObjectFirst,
            Want::ObjectLabel => Bad::ExpectedColon,
            Want::ObjectNext => Bad::ExpectedObjectNext,
        }
    }

    /// Past a token of kind `t`, else the error for `want`.
    fn expect(&mut self, want: Want, t: Tok) -> Result<(), Bad> {
        if self.tok == t { self.lex() } else { Err(self.wrong(want)) }
    }

    /// The whole input, read as Postgres's recursive descent reads it (with a stack of the open arrays and objects
    /// instead of recursion: true for an object).
    fn parse(&mut self) -> Result<(), Bad> {
        self.lex()?;
        let mut open: Vec<bool> = vec![];
        'value: loop {
            // a value
            match self.tok {
                Tok::ObjStart | Tok::ArrStart if open.len() >= INPUT_DEPTH => return Err(Bad::Deep),
                Tok::ObjStart => {
                    open.push(true);
                    self.lex()?;
                    match self.tok {
                        Tok::Str => {
                            self.lex()?;
                            self.expect(Want::ObjectLabel, Tok::Colon)?;
                            continue 'value;
                        }
                        Tok::ObjEnd => {
                            self.lex()?;
                            open.pop();
                        }
                        _ => return Err(self.wrong(Want::ObjectStart)),
                    }
                }
                Tok::ArrStart => {
                    open.push(false);
                    self.lex()?;
                    if self.tok != Tok::ArrEnd {
                        continue 'value;
                    }
                    self.lex()?;
                    open.pop();
                }
                Tok::Str | Tok::Num | Tok::True | Tok::False | Tok::Null => self.lex()?,
                _ => return Err(self.wrong(Want::Value)),
            }
            // after a value: the next element or field, or the end of what holds it
            loop {
                match open.last().copied() {
                    None => break 'value,
                    Some(true) if self.tok == Tok::Comma => {
                        self.lex()?;
                        if self.tok != Tok::Str {
                            return Err(self.wrong(Want::String));
                        }
                        self.lex()?;
                        self.expect(Want::ObjectLabel, Tok::Colon)?;
                        continue 'value;
                    }
                    Some(true) => {
                        self.expect(Want::ObjectNext, Tok::ObjEnd)?;
                        open.pop();
                    }
                    Some(false) if self.tok == Tok::Comma => {
                        self.lex()?;
                        continue 'value;
                    }
                    Some(false) => {
                        self.expect(Want::ArrayNext, Tok::ArrEnd)?;
                        open.pop();
                    }
                }
            }
        }
        self.expect(Want::End, Tok::End)
    }

    /// The token the error is about, as Postgres quotes it.
    fn token(&self) -> String {
        let start = self.start.unwrap_or(self.term).min(self.term);
        String::from_utf8_lossy(&self.s[start..self.term]).into_owned()
    }

    /// Postgres's CONTEXT line: the error's line up to where it was found, its last 50 bytes or so.
    fn context(&self) -> String {
        let (s, end) = (self.s, self.term.min(self.s.len()));
        let line_start = self.line_start.min(end);
        let mut from = line_start;
        while end - from >= 50 {
            from = self.char_end(from);
        }
        if from - line_start <= 3 {
            from = line_start;
        }
        let prefix = if from > line_start { "..." } else { "" };
        let suffix =
            if self.tok != Tok::End && end < s.len() && s[end] != b'\n' && s[end] != b'\r' { "..." } else { "" };
        format!("JSON data, line {}: {prefix}{}{suffix}", self.line, String::from_utf8_lossy(&s[from..end]))
    }
}

/// The error Postgres gives for text that isn't JSON (jsonb's input, which also refuses `\u0000`), with its
/// DETAIL and CONTEXT; None for text its parser reads.
pub(super) fn input_error(text: &str) -> Option<super::SqlError> {
    parse_error(text, true)
}

/// `input_error` for json's input, which keeps its text: escapes are only checked for their form, so `\u0000` and
/// a lone surrogate are taken (json_in parses without de-escaping).
pub(super) fn json_input_error(text: &str) -> Option<super::SqlError> {
    parse_error(text, false)
}

fn parse_error(text: &str, escapes: bool) -> Option<super::SqlError> {
    let mut lex = Lex { s: text.as_bytes(), start: None, term: 0, tok: Tok::Invalid, line: 1, line_start: 0, escapes };
    let why = lex.parse().err()?;
    let token = lex.token();
    let detail = match why {
        Bad::EscapingInvalid => format!("Escape sequence \"\\{token}\" is invalid."),
        Bad::EscapingRequired => format!("Character with value 0x{:02x} must be escaped.", lex.s[lex.term]),
        Bad::ExpectedEnd => format!("Expected end of input, but found \"{token}\"."),
        Bad::ExpectedArrayNext => format!("Expected \",\" or \"]\", but found \"{token}\"."),
        Bad::ExpectedColon => format!("Expected \":\", but found \"{token}\"."),
        Bad::ExpectedJson => format!("Expected JSON value, but found \"{token}\"."),
        Bad::ExpectedMore => "The input string ended unexpectedly.".into(),
        Bad::ExpectedObjectFirst => format!("Expected string or \"}}\", but found \"{token}\"."),
        Bad::ExpectedObjectNext => format!("Expected \",\" or \"}}\", but found \"{token}\"."),
        Bad::ExpectedString => format!("Expected string, but found \"{token}\"."),
        Bad::InvalidToken => format!("Token \"{token}\" is invalid."),
        Bad::CodePointZero => "\\u0000 cannot be converted to text.".into(),
        Bad::EscapeFormat => "\"\\u\" must be followed by four hexadecimal digits.".into(),
        Bad::HighSurrogate => "Unicode high surrogate must not follow a high surrogate.".into(),
        Bad::LowSurrogate => "Unicode low surrogate must follow a high surrogate.".into(),
        Bad::Deep => {
            let depth = super::session_setting("max_stack_depth").unwrap_or_else(|| "2MB".into());
            return Some(super::SqlError {
                code: "54001",
                message: "stack depth limit exceeded".into(),
                info: Some(Box::new(super::Violation {
                    hint: Some(format!(
                        "Increase the configuration parameter \"max_stack_depth\" (currently {depth}), after ensuring the platform's stack depth limit is adequate."
                    )),
                    ..Default::default()
                })),
            });
        }
    };
    let (code, message) = match why {
        Bad::CodePointZero => ("22P05", "unsupported Unicode escape sequence"),
        _ => ("22P02", "invalid input syntax for type json"),
    };
    Some(super::SqlError {
        code,
        message: message.into(),
        info: Some(Box::new(super::Violation {
            detail: Some(detail),
            context: Some(lex.context()),
            ..Default::default()
        })),
    })
}

pub(super) fn typeof_(j: &Json) -> &'static str {
    match j {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn postgres_text_and_containment() {
        assert_eq!(text(&json!({"bb": [1, "x"], "a": {"c": null}})), r#"{"a": {"c": null}, "bb": [1, "x"]}"#);
        assert!(contains(&json!({"a": 1, "b": [1, 2, {"c": 3}]}), &json!({"b": [{"c": 3}, 1]})));
        assert!(!contains(&json!({"a": 1}), &json!({"a": 1, "b": 2})));
        assert!(contains(&json!([1, 2]), &json!(2)));
        let path = [Some("a".to_string()), Some("1".to_string())];
        assert_eq!(set(&json!({"a": [1, 2]}), &path, &json!(9), true).unwrap(), json!({"a": [1, 9]}));
        assert!(set(&json!({"a": [1, 2]}), &[Some("a".to_string()), None], &json!(9), true).is_err());
        assert!(set(&json!("a"), &path, &json!(9), true).is_err());
    }
}
