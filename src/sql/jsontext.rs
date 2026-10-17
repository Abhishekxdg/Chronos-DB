//! json as Postgres has it beside jsonb: a value is its text as it was written (spacing, key order and duplicate
//! keys kept, `\u0000` taken), checked on input and never normalised. Its functions write the layouts Postgres's
//! json.c writes (`json_build_object`'s `{"a" : 1}`, `row_to_json`'s `{"a":1}`, `json_agg`'s `[1, 2]`), and its
//! lookups (`->`, `json_each`, `json_array_elements`, ...) give the parts of the text they find, as written.
//!
//! The text is read where it is: a lookup skips the values it passes over without building them, so a large array
//! costs nothing to look into.

use super::{Result, SqlError, Type, Value, err};

/// json functions, by their place here (`Scalar::JsonFn`): the json_ ones jsonb's share no code with.
pub(super) const BUILD_OBJECT: u8 = 0;
pub(super) const BUILD_ARRAY: u8 = 1;
pub(super) const TO_JSON: u8 = 2;
pub(super) const ROW_TO_JSON: u8 = 3;
pub(super) const ARRAY_TO_JSON: u8 = 4;
/// json_object(text[]) and json_object(keys text[], values text[])
pub(super) const OBJECT: u8 = 5;
pub(super) const TYPEOF: u8 = 6;
pub(super) const ARRAY_LENGTH: u8 = 7;
pub(super) const EXTRACT_PATH: u8 = 8;
pub(super) const EXTRACT_PATH_TEXT: u8 = 9;
pub(super) const STRIP_NULLS: u8 = 10;
/// A value whose JSON its type decides, not its value (an array, a record, both held as text): `[value, code]`,
/// the code put there by the binder (see `type_code`). No name calls it.
pub(super) const TYPED: u8 = 11;
/// A table's whole row as an object, as row_to_json writes one: `[name, value, name, value, ...]` (see
/// `Binder::whole_rows`). No name calls it.
pub(super) const ROW: u8 = 12;

/// The functions' names, by their place.
pub(super) const NAMES: [&str; 13] = [
    "json_build_object",
    "json_build_array",
    "to_json",
    "row_to_json",
    "array_to_json",
    "json_object",
    "json_typeof",
    "json_array_length",
    "json_extract_path",
    "json_extract_path_text",
    "json_strip_nulls",
    "to_json",
    "row_to_json",
];

/// The function a json_ name calls, and how many arguments it takes.
pub(super) fn lookup(name: &str) -> Option<(u8, std::ops::RangeInclusive<usize>)> {
    const MANY: usize = usize::MAX;
    Some(match name {
        "json_build_object" => (BUILD_OBJECT, 0..=MANY),
        "json_build_array" => (BUILD_ARRAY, 0..=MANY),
        "to_json" => (TO_JSON, 1..=1),
        "row_to_json" => (ROW_TO_JSON, 1..=2),
        "array_to_json" => (ARRAY_TO_JSON, 1..=2),
        "json_object" => (OBJECT, 1..=2),
        "json_typeof" => (TYPEOF, 1..=1),
        "json_array_length" => (ARRAY_LENGTH, 1..=1),
        "json_extract_path" => (EXTRACT_PATH, 2..=MANY),
        "json_extract_path_text" => (EXTRACT_PATH_TEXT, 2..=MANY),
        "json_strip_nulls" => (STRIP_NULLS, 1..=1),
        _ => return None,
    })
}

/// Whether a function writes its arguments as JSON (their types say how: see `Binder::json_typed`).
pub(super) fn builds(k: u8) -> bool {
    matches!(k, BUILD_OBJECT | BUILD_ARRAY | TO_JSON | ROW_TO_JSON | ARRAY_TO_JSON | ROW)
}

pub(super) fn result_type(k: u8) -> Type {
    match k {
        TYPEOF | EXTRACT_PATH_TEXT => Type::Text,
        ARRAY_LENGTH => Type::Int,
        _ => Type::JsonText,
    }
}

/// json's input: the text, if it is JSON (Postgres's error, saying where, if not). Escapes are checked for their
/// form only: `\u0000` and a lone surrogate are taken, as json_in takes them.
pub(super) fn check(s: &str) -> Result<()> {
    match super::jsonb::json_input_error(s) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// What the binder puts beside a value whose JSON its type decides (see `TYPED`): an array's element type id, a
/// record type's place as -1 - place. None for a type whose values say it.
pub(super) fn type_code(t: Type) -> Option<i64> {
    match t {
        Type::Array(e) => Some(i64::from(e)),
        Type::Record(i) if t.fields().is_some() => Some(-1 - i64::from(i)),
        _ => None,
    }
}

fn code_type(c: i64) -> Type {
    if c >= 0 { Type::Array(c as u32) } else { Type::Record((-1 - c) as u16) }
}

pub(super) fn call(k: u8, a: &[Value]) -> Result<Value> {
    use Value::*;
    let first = a.first().unwrap_or(&Null);
    Ok(match k {
        BUILD_OBJECT => JsonText(build_object(a)?),
        BUILD_ARRAY => JsonText(build_array(a)),
        ROW => JsonText(row(a)),
        _ if *first == Null => Null,
        TO_JSON => JsonText(datum(first)),
        ROW_TO_JSON | ARRAY_TO_JSON => {
            let t = match (k, first) {
                // (an array the binder had no type for: of text)
                (ARRAY_TO_JSON, Text(s)) => array_json(s, Type::Text, ",")?,
                (_, v) => datum(v),
            };
            // pretty: a line to each top-level field or element
            if a.get(1) == Some(&Bool(true)) { JsonText(relayout(&t, ",\n ")?) } else { JsonText(t) }
        }
        TYPED => match a.get(1) {
            Some(Int(c)) => JsonText(by_type(first, code_type(*c))?),
            _ => JsonText(datum(first)),
        },
        OBJECT => object(a)?,
        _ => {
            let t = text_of(first)?.unwrap_or_default();
            let root = ws(t.as_bytes(), 0);
            match k {
                TYPEOF => Text(typeof_(&t).into()),
                ARRAY_LENGTH => match t.as_bytes().get(root) {
                    Some(b'[') => {
                        let mut n = 0i64;
                        each_element(t.as_bytes(), root, |_, _| {
                            n += 1;
                            Ok(true)
                        })?;
                        Int(n)
                    }
                    Some(b'{') => return err("22023", "cannot get array length of a non-array"),
                    _ => return err("22023", "cannot get array length of a scalar"),
                },
                STRIP_NULLS => {
                    escapes_ok(&t)?;
                    let mut out = String::new();
                    strip_nulls(&t, root, &mut out)?;
                    JsonText(out)
                }
                // json_extract_path(_text): a null step finds nothing
                _ if a[1..].contains(&Null) => Null,
                _ => {
                    escapes_ok(&t)?;
                    let steps: Vec<String> = a[1..].iter().map(Value::to_text).collect();
                    walk(&t, &steps, k == EXTRACT_PATH_TEXT)?
                }
            }
        }
    })
}

/// The json text of a value given where json is taken: json as it is, text read as json (checked), jsonb as its
/// text; None for NULL.
fn text_of(v: &Value) -> Result<Option<String>> {
    Ok(Some(match v {
        Value::Null => return Ok(None),
        Value::JsonText(s) => s.clone(),
        Value::Text(s) => {
            check(s)?;
            s.clone()
        }
        Value::Json(j) => super::jsonb::text(j),
        v => datum(v),
    }))
}

// ---------------------------------------------------------------- writing JSON

/// A string as a JSON string, as Postgres's escape_json writes it: `"`, `\` and the control characters escaped,
/// everything else as it is.
/// Whether JSON text `s` (valid JSON) has no object with a key in it twice, as `IS JSON WITH UNIQUE KEYS` checks
/// (keys compared with their escapes read).
pub(super) fn unique_keys(s: &str) -> bool {
    let b = s.as_bytes();
    // each object or array open around the place: an object's keys so far
    let mut open: Vec<Option<std::collections::HashSet<String>>> = vec![];
    let (mut i, mut key_next) = (0, false);
    while i < b.len() {
        match b[i] {
            b'{' => {
                open.push(Some(Default::default()));
                key_next = true;
            }
            b'[' => {
                open.push(None);
                key_next = false;
            }
            b'}' | b']' => {
                open.pop();
                key_next = false;
            }
            b',' => key_next = matches!(open.last(), Some(Some(_))),
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                let end = (i + 1).min(b.len());
                if key_next {
                    let key: String = serde_json::from_str(&s[start..end]).unwrap_or_default();
                    if let Some(Some(keys)) = open.last_mut()
                        && !keys.insert(key)
                    {
                        return false;
                    }
                    key_next = false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    true
}

pub(super) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Whether text is a JSON number (Postgres's IsValidJsonNumber): `-`, digits without a leading zero, a fraction,
/// an exponent.
fn is_number(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > start
    };
    match b.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            digits(&mut i);
        }
        _ => return false,
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == b.len()
}

/// A value as JSON, as Postgres's datum_to_json writes it by the value's type: json as it is, jsonb as its text,
/// numbers as numbers (NaN and the infinities as strings), booleans, dates and timestamps as ISO 8601 strings
/// (whatever DateStyle says), anything else as its text in a string.
pub(super) fn datum(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::JsonText(s) => s.clone(),
        Value::Json(j) => super::jsonb::text(j),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(_) | Value::Numeric(_) => {
            let t = v.to_text();
            if is_number(&t) { t } else { escape(&t) }
        }
        Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_) => match v.to_json() {
            serde_json::Value::String(s) => escape(&s),
            j => j.to_string(),
        },
        v => escape(&v.to_text()),
    }
}

/// An object's key, as datum_to_json writes one: always a string; NULL, json, jsonb, an array or a record refused.
pub(super) fn key(v: &Value) -> Result<String> {
    match v {
        Value::Null => err("22004", "null value not allowed for object key"),
        Value::JsonText(_) | Value::Json(_) => err("22023", "key value must be scalar, not array, composite, or json"),
        Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_) => Ok(datum(v)),
        v => Ok(escape(&v.to_text())),
    }
}

/// A value of type `t` as JSON (see `TYPED`): an array as a JSON array of its elements (of any dimensions), a
/// record as an object of its fields, each as its own type has it; anything else by its value.
fn by_type(v: &Value, t: Type) -> Result<String> {
    Ok(match (v, t) {
        (Value::Null, _) => "null".into(),
        (Value::Text(s), Type::Array(_)) => array_json(s, t.elem().unwrap_or(Type::Text), ",")?,
        (Value::Text(s), Type::Record(_)) => record_json(s, t)?,
        (Value::Text(s), Type::JsonText) => s.clone(),
        (Value::Text(_), Type::Text) => datum(v),
        // (a real element or field is written with its float4 digits, as float4out has them: `9.8`)
        (Value::Text(_), Type::Float4) => match super::coerce(v.clone(), Type::Float4) {
            Ok(Value::Float(f)) => {
                let t = super::fmt_float4(f);
                if is_number(&t) { t } else { escape(&t) }
            }
            _ => datum(v),
        },
        // (an element or a field is its text: read as its type, as the type's own output would show it)
        (Value::Text(_), t) => datum(&super::coerce(v.clone(), t).unwrap_or_else(|_| v.clone())),
        (v, _) => datum(v),
    })
}

/// An array's text as a JSON array of its elements, each of type `elem`, its sub-arrays nested; `sep` between the
/// outermost elements (array_to_json's pretty form puts a line between them).
fn array_json(text: &str, elem: Type, sep: &str) -> Result<String> {
    let (dims, vals) = match super::array_shape(text) {
        Ok(x) => x,
        // (an int2vector's or oidvector's `1 2`, which has no braces)
        Err(e) => match super::array_values(Value::Text(text.into())) {
            Ok(Some(v)) => (if v.is_empty() { vec![] } else { vec![v.len()] }, v),
            _ => return Err(e),
        },
    };
    if dims.is_empty() {
        return Ok("[]".into());
    }
    fn nest(dims: &[usize], vals: &[Value], at: &mut usize, elem: Type, sep: &str, out: &mut String) -> Result<()> {
        out.push('[');
        for i in 0..dims[0] {
            if i > 0 {
                out.push_str(sep);
            }
            if dims.len() == 1 {
                out.push_str(&by_type(vals.get(*at).unwrap_or(&Value::Null), elem)?);
                *at += 1;
            } else {
                // (the inner dimensions on one line)
                nest(&dims[1..], vals, at, elem, ",", out)?;
            }
        }
        out.push(']');
        Ok(())
    }
    let mut out = String::new();
    nest(&dims, &vals, &mut 0, elem, sep, &mut out)?;
    Ok(out)
}

/// A record's composite text as an object of its fields, as composite_to_json writes it: `{"f1":1,"f2":"foo"}`.
fn record_json(text: &str, t: Type) -> Result<String> {
    let (Some(fields), Some(vals)) = (t.fields(), super::composite_fields(text)) else { return Ok(escape(text)) };
    if fields.len() != vals.len() {
        return Ok(escape(text));
    }
    let mut out = String::from("{");
    for (i, ((name, ft), v)) in fields.iter().zip(vals).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&escape(name));
        out.push(':');
        out.push_str(&by_type(&v.map_or(Value::Null, Value::Text), *ft)?);
    }
    out.push('}');
    Ok(out)
}

/// A table's whole row (`ROW`): names and values in turn.
fn row(a: &[Value]) -> String {
    let fields: Vec<String> = a
        .chunks(2)
        .map(|kv| format!("{}:{}", escape(&kv[0].to_text()), datum(kv.get(1).unwrap_or(&Value::Null))))
        .collect();
    format!("{{{}}}", fields.join(","))
}

/// json_build_object(k, v, ...): `{"k" : v, "k2" : v2}`.
fn build_object(a: &[Value]) -> Result<String> {
    if a.len() % 2 == 1 {
        return Err(SqlError {
            code: "22023",
            message: "argument list must have even number of elements".into(),
            info: Some(Box::new(super::Violation {
                hint: Some("The arguments of json_build_object() must consist of alternating keys and values.".into()),
                ..Default::default()
            })),
        });
    }
    let mut fields = Vec::with_capacity(a.len() / 2);
    for kv in a.chunks(2) {
        fields.push(format!("{} : {}", key(&kv[0])?, datum(&kv[1])));
    }
    Ok(format!("{{{}}}", fields.join(", ")))
}

/// json_build_array(v, ...): `[v, v2]`.
fn build_array(a: &[Value]) -> String {
    format!("[{}]", a.iter().map(datum).collect::<Vec<_>>().join(", "))
}

/// json_object(text[]) of key, value pairs (a flat array or one of two columns), or json_object(keys, values):
/// `{"a" : "1", "b" : null}`.
fn object(a: &[Value]) -> Result<Value> {
    if a.contains(&Value::Null) {
        return Ok(Value::Null);
    }
    let shaped = |v: &Value| match v {
        Value::Text(t) => super::array_shape(t),
        v => super::array_values(v.clone()).map(|x| {
            let x = x.unwrap_or_default();
            (if x.is_empty() { vec![] } else { vec![x.len()] }, x)
        }),
    };
    let (keys, vals): (Vec<Value>, Vec<Value>) = match a.get(1) {
        Some(v) => {
            let ((kd, keys), (vd, vals)) = (shaped(&a[0])?, shaped(v)?);
            if kd.len() > 1 || kd.len() != vd.len() {
                return err("2202E", "wrong number of array subscripts");
            }
            if keys.len() != vals.len() {
                return err("2202E", "mismatched array dimensions");
            }
            (keys, vals)
        }
        None => {
            let (dims, all) = shaped(&a[0])?;
            match dims.len() {
                0 => {}
                1 if all.len() % 2 == 1 => return err("2202E", "array must have even number of elements"),
                1 => {}
                2 if dims[1] != 2 => return err("2202E", "array must have two columns"),
                2 => {}
                _ => return err("2202E", "wrong number of array subscripts"),
            }
            (all.iter().step_by(2).cloned().collect(), all.iter().skip(1).step_by(2).cloned().collect())
        }
    };
    let mut fields = Vec::with_capacity(keys.len());
    for (k, v) in keys.iter().zip(&vals) {
        if *k == Value::Null {
            return err("22004", "null value not allowed for object key");
        }
        let v = if *v == Value::Null { "null".into() } else { escape(&v.to_text()) };
        fields.push(format!("{} : {v}", escape(&k.to_text())));
    }
    Ok(Value::JsonText(format!("{{{}}}", fields.join(", "))))
}

/// json_agg's result: its values in a JSON array, `, ` between them, and a line before each array or record after
/// the first (`structured`: the values are of an array or a row type).
pub(super) fn agg<'a>(vals: impl Iterator<Item = &'a Value>, structured: bool) -> String {
    let mut out = String::from("[");
    for (i, v) in vals.enumerate() {
        if i > 0 {
            out.push_str(", ");
            if structured && *v != Value::Null {
                out.push_str("\n ");
            }
        }
        out.push_str(&datum(v));
    }
    out.push(']');
    out
}

/// json_object_agg's result: `{ "k" : v, "k2" : v2 }`.
pub(super) fn object_agg<'a>(pairs: impl Iterator<Item = (&'a Value, &'a Value)>) -> Result<String> {
    let mut fields = vec![];
    for (k, v) in pairs {
        fields.push(format!("{} : {}", key(k)?, datum(v)));
    }
    Ok(format!("{{ {} }}", fields.join(", ")))
}

/// `t` with a line between its top-level fields or elements (row_to_json's and array_to_json's pretty form); it is
/// one of ours, which has no space in it at that level.
fn relayout(t: &str, sep: &str) -> Result<String> {
    let s = t.as_bytes();
    let root = ws(s, 0);
    let mut parts: Vec<&str> = vec![];
    let (open, close) = match s.get(root) {
        Some(b'{') => {
            each_field(s, root, |k, v| {
                parts.push(&t[k.0..v.1]);
                Ok(true)
            })?;
            ('{', '}')
        }
        Some(b'[') => {
            each_element(s, root, |a, b| {
                parts.push(&t[a..b]);
                Ok(true)
            })?;
            ('[', ']')
        }
        _ => return Ok(t.to_string()),
    };
    Ok(format!("{open}{}{close}", parts.join(sep)))
}

// ---------------------------------------------------------------- reading JSON text

fn ws(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Just past the string whose opening quote is at `i`.
fn string_end(s: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < s.len() {
        match s[i] {
            b'"' => return i + 1,
            b'\\' => i += 2,
            _ => i += 1,
        }
    }
    s.len()
}

/// Just past the value that starts at `i` (after any space): the text is JSON, so an array or an object ends where
/// its brackets balance, outside strings.
fn skip(s: &[u8], i: usize) -> usize {
    let mut i = ws(s, i);
    match s.get(i) {
        Some(b'"') => return string_end(s, i),
        Some(b'[' | b'{') => {}
        _ => {
            while i < s.len() && !matches!(s[i], b',' | b']' | b'}' | b':' | b' ' | b'\t' | b'\n' | b'\r') {
                i += 1;
            }
            return i;
        }
    }
    let mut depth = 0usize;
    while i < s.len() {
        match s[i] {
            b'"' => {
                i = string_end(s, i);
                continue;
            }
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    i
}

/// Each element of the array whose `[` is at `i`, as where it starts and ends, until `f` says to stop.
fn each_element(s: &[u8], i: usize, mut f: impl FnMut(usize, usize) -> Result<bool>) -> Result<()> {
    let mut i = ws(s, i + 1);
    if s.get(i) == Some(&b']') {
        return Ok(());
    }
    loop {
        let start = ws(s, i);
        let end = skip(s, start);
        if !f(start, end)? {
            return Ok(());
        }
        i = ws(s, end);
        match s.get(i) {
            Some(b',') => i += 1,
            _ => return Ok(()),
        }
    }
}

/// Each field of the object whose `{` is at `i`, in the order written (a key that comes twice, twice): its key's
/// place (quotes and all) and its value's, until `f` says to stop.
fn each_field(s: &[u8], i: usize, mut f: impl FnMut((usize, usize), (usize, usize)) -> Result<bool>) -> Result<()> {
    let mut i = ws(s, i + 1);
    if s.get(i) == Some(&b'}') {
        return Ok(());
    }
    loop {
        let ks = ws(s, i);
        let ke = string_end(s, ks);
        let colon = ws(s, ke);
        let vs = ws(s, colon + 1);
        let ve = skip(s, vs);
        if !f((ks, ke), (vs, ve))? {
            return Ok(());
        }
        i = ws(s, ve);
        match s.get(i) {
            Some(b',') => i += 1,
            _ => return Ok(()),
        }
    }
}

/// A JSON string's text (quotes and all) de-escaped; `\u0000` and a lone surrogate are Postgres's errors, as text
/// can't hold them.
fn unescape(raw: &str) -> Result<String> {
    match serde_json::from_str::<String>(raw) {
        Ok(s) if !s.contains('\0') => Ok(s),
        _ => Err(super::jsonb::input_error(raw).unwrap_or_else(|| SqlError {
            code: "22P02",
            message: "invalid input syntax for type json".into(),
            info: None,
        })),
    }
}

/// The text's `\u` escapes, which Postgres's lookups (`->`, json_each, json_strip_nulls, ...) de-escape as they read
/// the whole text: a `\u0000` or a lone surrogate anywhere is their error, saying where.
fn escapes_ok(t: &str) -> Result<()> {
    if t.contains("\\u")
        && let Some(e) = super::jsonb::input_error(t)
    {
        return Err(e);
    }
    Ok(())
}

/// Whether the key at `k` is `key`.
fn key_is(t: &str, k: (usize, usize), key: &str) -> bool {
    let raw = &t[k.0 + 1..k.1.saturating_sub(1).max(k.0 + 1)];
    if !raw.contains('\\') {
        return raw == key;
    }
    unescape(&t[k.0..k.1]).is_ok_and(|x| x == key)
}

type Span = Option<(usize, usize)>;

/// The object at `at`'s field `key`: the last of that name, as Postgres's json lookups find it.
fn field(t: &str, at: usize, key: &str) -> Result<Span> {
    let s = t.as_bytes();
    if s.get(at) != Some(&b'{') {
        return Ok(None);
    }
    let mut found = None;
    each_field(s, at, |k, v| {
        if key_is(t, k, key) {
            found = Some(v);
        }
        Ok(true)
    })?;
    Ok(found)
}

/// The array at `at`'s element `n`, counting from 0 (from the end when negative).
fn element(t: &str, at: usize, n: i64) -> Result<Span> {
    let s = t.as_bytes();
    if s.get(at) != Some(&b'[') {
        return Ok(None);
    }
    let n = if n < 0 {
        let mut count = 0i64;
        each_element(s, at, |_, _| {
            count += 1;
            Ok(true)
        })?;
        count + n
    } else {
        n
    };
    if n < 0 {
        return Ok(None);
    }
    let (mut i, mut found) = (0i64, None);
    each_element(s, at, |a, b| {
        if i == n {
            found = Some((a, b));
            return Ok(false);
        }
        i += 1;
        Ok(true)
    })?;
    Ok(found)
}

/// One step of a path: an object's field, or an array's element by its number (a step that isn't one finds
/// nothing in an array).
fn step(t: &str, at: usize, step: &str) -> Result<Span> {
    match t.as_bytes().get(at) {
        Some(b'{') => field(t, at, step),
        Some(b'[') => match step.trim_start().parse::<i32>() {
            Ok(n) => element(t, at, n.into()),
            Err(_) => Ok(None),
        },
        _ => Ok(None),
    }
}

/// `->>`'s text of a value: a string's de-escaped, NULL for null, anything else as written.
fn scalar_text(raw: &str) -> Result<Option<String>> {
    Ok(match raw.as_bytes().first() {
        Some(b'"') => Some(unescape(raw)?),
        Some(b'n') => None,
        _ => Some(raw.to_string()),
    })
}

/// What a lookup found: the json there, as written, or (`as_text`) its text.
fn found(t: &str, span: Span, as_text: bool) -> Result<Value> {
    let Some((a, b)) = span else { return Ok(Value::Null) };
    let raw = &t[a..b];
    Ok(if as_text { scalar_text(raw)?.map_or(Value::Null, Value::Text) } else { Value::JsonText(raw.to_string()) })
}

/// The value at a path of steps from the top (all of it for none).
fn walk(t: &str, steps: &[String], as_text: bool) -> Result<Value> {
    let s = t.as_bytes();
    let root = ws(s, 0);
    let mut span = Some((root, skip(s, root)));
    for st in steps {
        let Some((at, _)) = span else { break };
        span = step(t, at, st)?;
    }
    found(t, span, as_text)
}

/// json's `->`, `->>` (`path` false: `key` an object's field, or an array's element if it is an integer) and `#>`,
/// `#>>` (a path, a text[]; a null step finds nothing).
pub(super) fn get(t: &str, key: &Value, path: bool, as_text: bool) -> Result<Value> {
    escapes_ok(t)?;
    if !path {
        let root = ws(t.as_bytes(), 0);
        let span = match key {
            Value::Int(n) => element(t, root, *n)?,
            k => field(t, root, &k.to_text())?,
        };
        return found(t, span, as_text);
    }
    let Some(steps) = super::array_values(key.clone())? else { return Ok(Value::Null) };
    if steps.contains(&Value::Null) {
        return Ok(Value::Null);
    }
    let steps: Vec<String> = steps.iter().map(Value::to_text).collect();
    walk(t, &steps, as_text)
}

fn typeof_(t: &str) -> &'static str {
    match t.as_bytes().get(ws(t.as_bytes(), 0)) {
        Some(b'{') => "object",
        Some(b'[') => "array",
        Some(b'"') => "string",
        Some(b't' | b'f') => "boolean",
        Some(b'n') => "null",
        _ => "number",
    }
}

/// json_strip_nulls: the value at `at` written again without its objects' null fields, at every level, as Postgres
/// writes it: no space, keys and strings escaped anew, other scalars as they were.
fn strip_nulls(t: &str, at: usize, out: &mut String) -> Result<()> {
    let s = t.as_bytes();
    let end = skip(s, at);
    match s.get(at) {
        Some(b'{') => {
            let mut fields = vec![];
            each_field(s, at, |k, v| {
                fields.push((k, v));
                Ok(true)
            })?;
            out.push('{');
            let mut first = true;
            for (k, v) in fields {
                if s.get(v.0) == Some(&b'n') {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&escape(&unescape(&t[k.0..k.1])?));
                out.push(':');
                strip_nulls(t, v.0, out)?;
            }
            out.push('}');
        }
        Some(b'[') => {
            let mut items = vec![];
            each_element(s, at, |a, _| {
                items.push(a);
                Ok(true)
            })?;
            out.push('[');
            for (i, a) in items.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                strip_nulls(t, a, out)?;
            }
            out.push(']');
        }
        Some(b'"') => out.push_str(&escape(&unescape(&t[at..end])?)),
        _ => out.push_str(&t[at..end]),
    }
    Ok(())
}

/// The rows of json_array_elements(_text), json_each(_text) and json_object_keys: the parts of the text, as written
/// (or, for the _text ones, a string's text).
pub(super) fn rows(name: &str, v: &Value) -> Result<Vec<Vec<Value>>> {
    let Some(t) = text_of(v)? else { return Ok(vec![]) };
    escapes_ok(&t)?;
    let s = t.as_bytes();
    let root = ws(s, 0);
    let as_text = name.ends_with("_text");
    let value = |a: usize, b: usize| -> Result<Value> {
        Ok(match as_text {
            true => scalar_text(&t[a..b])?.map_or(Value::Null, Value::Text),
            false => Value::JsonText(t[a..b].to_string()),
        })
    };
    let mut out = vec![];
    match name {
        "json_array_elements" | "json_array_elements_text" => match s.get(root) {
            Some(b'[') => each_element(s, root, |a, b| {
                out.push(vec![value(a, b)?]);
                Ok(true)
            })?,
            Some(b'{') => return err("22023", "cannot extract elements from an object"),
            _ => return err("22023", "cannot extract elements from a scalar"),
        },
        "json_each" | "json_each_text" => match s.get(root) {
            Some(b'{') => each_field(s, root, |k, (a, b)| {
                out.push(vec![Value::Text(unescape(&t[k.0..k.1])?), value(a, b)?]);
                Ok(true)
            })?,
            Some(b'[') => return err("22023", "cannot deconstruct an array as an object"),
            _ => return err("22023", "cannot deconstruct a scalar"),
        },
        _ => match s.get(root) {
            Some(b'{') => each_field(s, root, |k, _| {
                out.push(vec![Value::Text(unescape(&t[k.0..k.1])?)]);
                Ok(true)
            })?,
            Some(b'[') => return err("22023", "cannot call json_object_keys on an array"),
            _ => return err("22023", "cannot call json_object_keys on a scalar"),
        },
    }
    Ok(out)
}

/// The fields of json objects' text, as written: each key's text and its value's json text, for the object at the
/// top (`set` false) or each element of the array at the top (an element that isn't an object has none). What
/// json_to_record and json_to_recordset read a json value's fields from; None where the top is neither.
#[allow(clippy::type_complexity)]
pub(super) fn records(t: &str, set: bool) -> Result<Option<Vec<Vec<(String, String)>>>> {
    escapes_ok(t)?;
    let s = t.as_bytes();
    let root = ws(s, 0);
    let fields = |at: usize| -> Result<Vec<(String, String)>> {
        let mut out = vec![];
        if s.get(at) == Some(&b'{') {
            each_field(s, at, |k, (a, b)| {
                out.push((unescape(&t[k.0..k.1])?, t[a..b].to_string()));
                Ok(true)
            })?;
        }
        Ok(out)
    };
    match (s.get(root), set) {
        (Some(b'{'), false) => Ok(Some(vec![fields(root)?])),
        (Some(b'['), true) => {
            let mut starts = vec![];
            each_element(s, root, |a, _| {
                starts.push(a);
                Ok(true)
            })?;
            Ok(Some(starts.into_iter().map(fields).collect::<Result<_>>()?))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_give_the_text_as_written() {
        let t = r#"{"a": {"b":{"c": "foo"}}, "x": [1, [2,3], {"y" :4}], "a": 2}"#;
        assert_eq!(get(t, &Value::Text("a".into()), false, false).unwrap(), Value::JsonText("2".into()));
        let x = get(t, &Value::Text("x".into()), false, false).unwrap();
        assert_eq!(x, Value::JsonText(r#"[1, [2,3], {"y" :4}]"#.into()));
        let Value::JsonText(x) = x else { unreachable!() };
        assert_eq!(get(&x, &Value::Int(-1), false, false).unwrap(), Value::JsonText(r#"{"y" :4}"#.into()));
        assert_eq!(get(&x, &Value::Int(1), false, true).unwrap(), Value::Text("[2,3]".into()));
        let path = ["x".to_string(), "2".to_string(), "y".to_string()];
        assert_eq!(walk(t, &path, false).unwrap(), Value::JsonText("4".into()));
        assert_eq!(typeof_(" [1]"), "array");
        let mut out = String::new();
        strip_nulls(r#"{"a": 1, "b": null, "c": [2, null], "d": {"e": null}}"#, 0, &mut out).unwrap();
        assert_eq!(out, r#"{"a":1,"c":[2,null],"d":{}}"#);
    }

    #[test]
    fn layouts_are_postgres_own() {
        let a = [Value::Text("a".into()), Value::Int(1), Value::Text("b".into()), Value::JsonText("[1,2]".into())];
        assert_eq!(build_object(&a).unwrap(), r#"{"a" : 1, "b" : [1,2]}"#);
        assert_eq!(build_array(&a), r#"["a", 1, "b", [1,2]]"#);
        assert_eq!(array_json("{{1,5},{99,100}}", Type::Int, ",").unwrap(), "[[1,5],[99,100]]");
        assert_eq!(relayout(r#"{"a":1,"b":[1,2]}"#, ",\n ").unwrap(), "{\"a\":1,\n \"b\":[1,2]}");
        assert_eq!(datum(&Value::Float(f64::NAN)), r#""NaN""#);
        assert_eq!(escape("a\"\u{1}"), r#""a\"\u0001""#);
        assert!(is_number("-1.5e+10") && !is_number("01") && !is_number("Infinity"));
    }
}
