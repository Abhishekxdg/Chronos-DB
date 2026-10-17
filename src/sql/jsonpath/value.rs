//! jsonb values as a path walks them: like `serde_json::Value`, but a number is an exact
//! `Numeric` (`0.10`, `1e400` and 30-digit integers keep what they say), an object keeps its
//! members in the order jsonb stores them, and a `.datetime()` result is a value of its own until
//! the path ends.

use super::PathError;
use super::datetime::Datetime;
use super::num::{NumError, Numeric};
use serde_json::Value as Json;
use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Numeric),
    String(String),
    Array(Vec<Value>),
    Object(Object),
    /// what `.datetime()` makes: it lives inside a path, and is a string in a result
    Datetime(Datetime),
}

/// An object's members, each key once, in the order jsonb keeps them: shorter keys first, then
/// by bytes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object(Vec<(String, Value)>);

fn key_order(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

impl Object {
    pub fn new() -> Object {
        Object(vec![])
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.binary_search_by(|(k, _)| key_order(k, key)).ok().map(|i| &self.0[i].1)
    }

    /// Sets `key`; a key that is there already takes the new value (as jsonb has it).
    pub fn insert(&mut self, key: String, v: Value) {
        match self.0.binary_search_by(|(k, _)| key_order(k, &key)) {
            Ok(i) => self.0[i].1 = v,
            Err(i) => self.0.insert(i, (key, v)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter().map(|(k, v)| (k, v))
    }

    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.0.iter().map(|(_, v)| v)
    }
}

impl FromIterator<(String, Value)> for Object {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(it: I) -> Object {
        let mut o = Object::new();
        for (k, v) in it {
            o.insert(k, v);
        }
        o
    }
}

/// How deep JSON text may nest.
const MAX_DEPTH: usize = 1000;

impl Value {
    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    pub fn is_container(&self) -> bool {
        matches!(self, Value::Array(_) | Value::Object(_))
    }

    /// About how much memory the value holds, in units of 64 bytes (what a walk may hold is
    /// counted in them).
    pub fn weight(&self) -> u64 {
        match self {
            Value::Null | Value::Bool(_) | Value::Datetime(_) => 1,
            Value::Number(_) => 2,
            Value::String(s) => 1 + s.len() as u64 / 64,
            Value::Array(a) => 1 + a.iter().map(Value::weight).sum::<u64>(),
            Value::Object(o) => 1 + o.iter().map(|(k, v)| 1 + k.len() as u64 / 64 + v.weight()).sum::<u64>(),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn from_i64(i: i64) -> Value {
        Value::Number(Numeric::from_i64(i))
    }

    pub fn from_usize(i: usize) -> Value {
        Value::Number(Numeric::from_usize(i))
    }

    /// Reads JSON text as `jsonb_in` does: a number keeps every digit, a key repeated keeps its
    /// last value. The error is Postgres's SQLSTATE and message.
    pub fn parse(text: &str) -> Result<Value, PathError> {
        let mut p = Reader { s: text.as_bytes(), i: 0 };
        let v = p.value(0)?;
        p.blanks();
        if p.i < p.s.len() {
            return Err(Reader::bad());
        }
        Ok(v)
    }

    /// From serde_json's value: a number is what serde_json printed for it (the shortest
    /// decimal that reads back as the same float, for a float).
    pub fn from_json(j: &Json) -> Value {
        match j {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => Numeric::parse(&n.to_string()).map_or(Value::Null, Value::Number),
            Json::String(s) => Value::String(s.clone()),
            Json::Array(a) => Value::Array(a.iter().map(Value::from_json).collect()),
            Json::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), Value::from_json(v))).collect()),
        }
    }

    /// As serde_json's value. A number is an integer if it is one that fits, else the nearest
    /// float; Err if it is past what a float holds.
    pub fn to_json(&self) -> Result<Json, PathError> {
        Ok(match self {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Number(n) => {
                // a whole number stays one, a number with digits after the point is a float
                if let Some(i) = n.to_i64().filter(|_| n.dscale() == 0) {
                    Json::from(i)
                } else if let Some(u) =
                    Some(n).filter(|n| n.dscale() == 0).and_then(|n| n.to_string().parse::<u64>().ok())
                {
                    Json::from(u)
                } else {
                    let f = n.to_f64();
                    match serde_json::Number::from_f64(f) {
                        Some(n) => Json::Number(n),
                        None => return Err(NumError::Overflow.err()),
                    }
                }
            }
            Value::String(s) => Json::String(s.clone()),
            Value::Array(a) => Json::Array(a.iter().map(Value::to_json).collect::<Result<_, _>>()?),
            Value::Object(o) => {
                Json::Object(o.iter().map(|(k, v)| Ok((k.clone(), v.to_json()?))).collect::<Result<_, PathError>>()?)
            }
            Value::Datetime(d) => Json::String(d.to_string()),
        })
    }

    /// With each `.datetime()` result made the string it is in a result.
    pub fn finish(self) -> Value {
        match self {
            Value::Datetime(d) => Value::String(d.to_string()),
            v => v,
        }
    }
}

impl fmt::Display for Value {
    /// jsonb's text form, as Postgres prints it: `{"a": 1, "bb": [1, 2]}`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        self.write(&mut out);
        f.write_str(&out)
    }
}

impl Value {
    fn write(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => escape(s, out),
            Value::Datetime(d) => escape(&d.to_string(), out),
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    x.write(out);
                }
                out.push(']');
            }
            Value::Object(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    escape(k, out);
                    out.push_str(": ");
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// `s` as a JSON string, as Postgres's escape_json writes it.
pub fn escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Reader<'a> {
    s: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn bad() -> PathError {
        ("22P02", "invalid input syntax for type json".into())
    }

    fn blanks(&mut self) {
        while self.s.get(self.i).is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, PathError> {
        if depth > MAX_DEPTH {
            return Err(("54001", "stack depth limit exceeded".into()));
        }
        self.blanks();
        match self.s.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut o = Object::new();
                self.blanks();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Value::Object(o));
                }
                loop {
                    self.blanks();
                    if self.s.get(self.i) != Some(&b'"') {
                        return Err(Reader::bad());
                    }
                    let k = self.string()?;
                    self.blanks();
                    if self.s.get(self.i) != Some(&b':') {
                        return Err(Reader::bad());
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    o.insert(k, v);
                    self.blanks();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::Object(o));
                        }
                        _ => return Err(Reader::bad()),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = vec![];
                self.blanks();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Value::Array(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    self.blanks();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::Array(a));
                        }
                        _ => return Err(Reader::bad()),
                    }
                }
            }
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b't') if self.eat("true") => Ok(Value::Bool(true)),
            Some(b'f') if self.eat("false") => Ok(Value::Bool(false)),
            Some(b'n') if self.eat("null") => Ok(Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(Reader::bad()),
        }
    }

    fn number(&mut self) -> Result<Value, PathError> {
        let from = self.i;
        if self.s.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let digits = |r: &mut Reader| {
            let at = r.i;
            while r.s.get(r.i).is_some_and(u8::is_ascii_digit) {
                r.i += 1;
            }
            r.i - at
        };
        match self.s.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(Reader::bad()),
        }
        if self.s.get(self.i) == Some(&b'.') {
            self.i += 1;
            if digits(self) == 0 {
                return Err(Reader::bad());
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return Err(Reader::bad());
            }
        }
        // a number runs up to something that can't be part of it
        if self.s.get(self.i).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'.') {
            return Err(Reader::bad());
        }
        let text = std::str::from_utf8(&self.s[from..self.i]).map_err(|_| Reader::bad())?;
        match Numeric::parse(text) {
            Ok(n) => Ok(Value::Number(n)),
            Err(NumError::Syntax) => Err(Reader::bad()),
            Err(e) => Err(e.err()),
        }
    }

    fn hex4(&mut self) -> Result<u32, PathError> {
        let mut v = 0;
        for _ in 0..4 {
            let d = self.s.get(self.i).and_then(|b| (*b as char).to_digit(16)).ok_or_else(Reader::bad)?;
            v = v << 4 | d;
            self.i += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, PathError> {
        self.i += 1; // the opening quote
        let mut out = String::new();
        loop {
            let from = self.i;
            while self.s.get(self.i).is_some_and(|b| *b != b'"' && *b != b'\\' && *b >= 0x20) {
                self.i += 1;
            }
            out.push_str(std::str::from_utf8(&self.s[from..self.i]).map_err(|_| Reader::bad())?);
            match self.s.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let c = *self.s.get(self.i).ok_or_else(Reader::bad)?;
                    self.i += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\x08'),
                        b'f' => out.push('\x0c'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                if !self.eat("\\u") {
                                    return Err(Reader::bad());
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(Reader::bad());
                                }
                                cp = 0x10000 + ((cp & 0x3FF) << 10) + (lo & 0x3FF);
                            } else if (0xDC00..0xE000).contains(&cp) {
                                return Err(Reader::bad());
                            }
                            if cp == 0 {
                                return Err(("22P05", "unsupported Unicode escape sequence".into()));
                            }
                            out.push(char::from_u32(cp).ok_or_else(Reader::bad)?);
                        }
                        _ => return Err(Reader::bad()),
                    }
                }
                _ => return Err(Reader::bad()),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// jsonb's layout, for .keyvalue()'s id

/// Walks `v` in the order jsonb stores it, calling `visit` with each array and object and the
/// byte where it starts counting from the start of `v`, until `visit` says true (then `done`).
/// Containers start on a 4-byte boundary; a container holds a 4-byte header, a 4-byte entry per
/// element (two per object member), then the keys and then the values, a number aligned and
/// taking 4 + 2 + 2 per base-10000 digit bytes. That start is what Postgres calls the id of an
/// object in `.keyvalue()`.
pub fn lay(v: &Value, at: usize, done: &mut bool, visit: &mut dyn FnMut(&Value, usize) -> bool) -> usize {
    if *done {
        return at;
    }
    match v {
        Value::Array(a) => {
            let at = at.next_multiple_of(4);
            if visit(v, at) {
                *done = true;
                return at;
            }
            a.iter().fold(at + 4 + 4 * a.len(), |p, x| lay(x, p, done, visit))
        }
        Value::Object(o) => {
            let at = at.next_multiple_of(4);
            if visit(v, at) {
                *done = true;
                return at;
            }
            let p = at + 4 + 8 * o.len() + o.iter().map(|(k, _)| k.len()).sum::<usize>();
            o.values().fold(p, |p, x| lay(x, p, done, visit))
        }
        Value::String(s) => at + s.len(),
        Value::Number(n) => at.next_multiple_of(4) + n.stored_len(),
        _ => at,
    }
}
