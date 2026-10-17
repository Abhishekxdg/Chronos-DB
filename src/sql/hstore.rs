//! hstore: text keys, each with a text value or NULL, kept as the text Postgres writes: `"a"=>"1", "b"=>NULL`, the
//! keys in hstore's order (the shorter first, then by their bytes) and each one once.

use super::funcs::Scalar;
use super::{Json, SqlError, Type, Value, err};

type Result<T> = std::result::Result<T, SqlError>;
type Pair = (String, Option<String>);
type Pairs = Vec<Pair>;

/// The type's id and its array's: Postgres numbers an extension's types as they are made; ours are fixed, in the
/// range it leaves to user types.
pub(crate) const OID: u32 = 16002;
pub(crate) const ARRAY_OID: u32 = 16012;

// What `call` does, by `Scalar::Hstore`'s number.
pub(super) const FETCH: u8 = 0; // h -> key
pub(super) const FETCH_MANY: u8 = 1; // h -> keys[]
pub(super) const CONCAT: u8 = 2; // h || h
pub(super) const EXIST: u8 = 3; // h ? key
pub(super) const EXIST_ANY: u8 = 4; // h ?| keys[]
pub(super) const EXIST_ALL: u8 = 5; // h ?& keys[]
pub(super) const CONTAINS: u8 = 6; // h @> h
pub(super) const DELETE_KEY: u8 = 7; // h - key
pub(super) const DELETE_KEYS: u8 = 8; // h - keys[]
pub(super) const DELETE_PAIRS: u8 = 9; // h - h
pub(super) const MAKE: u8 = 10; // hstore(key, value); hstore(...) of anything, until its arguments' types are known
pub(super) const MAKE_ARRAY: u8 = 11; // hstore(text[])
pub(super) const MAKE_ARRAYS: u8 = 12; // hstore(keys[], values[])
pub(super) const AKEYS: u8 = 13;
pub(super) const AVALS: u8 = 14;
pub(super) const TO_ARRAY: u8 = 15;
pub(super) const TO_MATRIX: u8 = 16;
pub(super) const TO_JSON: u8 = 17;
pub(super) const TO_JSON_LOOSE: u8 = 18;
pub(super) const SLICE: u8 = 19;
pub(super) const DEFINED: u8 = 20;
pub(super) const DELETE: u8 = 21; // delete(h, x): one of the three above, by x's type
pub(super) const CONTAINED: u8 = 22; // h <@ h

pub(super) fn result_type(k: u8) -> Type {
    match k {
        FETCH => Type::Text,
        FETCH_MANY | AKEYS | AVALS | TO_ARRAY | TO_MATRIX => Type::Array(25),
        EXIST | EXIST_ANY | EXIST_ALL | CONTAINS | CONTAINED | DEFINED => Type::Bool,
        TO_JSON | TO_JSON_LOOSE => Type::Json,
        _ => Type::Hstore,
    }
}

fn space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn eof() -> SqlError {
    SqlError { code: "42601", message: "syntax error in hstore: unexpected end of string".into(), info: None }
}

fn syntax(s: &[u8], at: usize) -> SqlError {
    // (the whole character the error is at, where it is of several bytes)
    let end = (at + 1..=s.len()).find(|i| std::str::from_utf8(&s[at..*i]).is_ok()).unwrap_or(at + 1);
    SqlError {
        code: "42601",
        message: format!(
            "syntax error in hstore, near \"{}\" at position {at}",
            String::from_utf8_lossy(&s[at..end.min(s.len())])
        ),
        info: None,
    }
}

/// One key (or, with `value`, one value) from `at`, and whether it was in quotes; None at the end of the text.
/// Leaves `at` on what ended it (`=`, `,`, the end), or past the space or the quote that did. As hstore_io.c's
/// get_val reads: a backslash takes the next character as it is, in quotes or out.
fn word(s: &[u8], at: &mut usize, value: bool) -> Result<Option<(Vec<u8>, bool)>> {
    enum St {
        Wait,
        In,
        Quoted,
        Esc,
        QuotedEsc,
    }
    let (mut st, mut out, mut quoted) = (St::Wait, vec![], false);
    loop {
        let c = s.get(*at).copied();
        match st {
            St::Wait => match c {
                Some(b'"') => {
                    quoted = true;
                    st = St::Quoted;
                }
                None => return Ok(None),
                Some(b'=') if !value => return Err(syntax(s, *at)),
                Some(b'\\') => st = St::Esc,
                Some(c) if !space(c) => {
                    out.push(c);
                    st = St::In;
                }
                _ => {}
            },
            St::In => match c {
                Some(b'\\') => st = St::Esc,
                Some(b'=') if !value => return Ok(Some((out, quoted))),
                Some(b',') if value => return Ok(Some((out, quoted))),
                None => return Ok(Some((out, quoted))),
                Some(c) if space(c) => {
                    *at += 1;
                    return Ok(Some((out, quoted)));
                }
                Some(c) => out.push(c),
            },
            St::Quoted => match c {
                Some(b'\\') => st = St::QuotedEsc,
                Some(b'"') => {
                    *at += 1;
                    return Ok(Some((out, quoted)));
                }
                None => return Err(eof()),
                Some(c) => out.push(c),
            },
            St::Esc | St::QuotedEsc => match c {
                None => return Err(eof()),
                Some(c) => {
                    out.push(c);
                    st = if matches!(st, St::Esc) { St::In } else { St::Quoted };
                }
            },
        }
        *at += 1;
    }
}

fn utf8(b: Vec<u8>, what: &str) -> Result<String> {
    if b.len() > 0x3FFF_FFFF {
        return err("22001", format!("string too long for hstore {what}"));
    }
    String::from_utf8(b).or_else(|_| err("22021", "invalid byte sequence for encoding \"UTF8\""))
}

/// The pairs of an hstore's text, as hstore_in reads it: `key => value` separated by commas, either in double
/// quotes or bare, a bare NULL value (in any case) for none.
pub(super) fn parse(s: &str) -> Result<Pairs> {
    let b = s.as_bytes();
    let (mut at, mut out): (usize, Pairs) = (0, vec![]);
    while let Some((k, _)) = word(b, &mut at, false)? {
        loop {
            match b.get(at) {
                Some(b'=') => break,
                None => return Err(eof()),
                Some(c) if space(*c) => at += 1,
                _ => return Err(syntax(b, at)),
            }
        }
        at += 1;
        match b.get(at) {
            Some(b'>') => at += 1,
            None => return Err(eof()),
            _ => return Err(syntax(b, at)),
        }
        let Some((v, quoted)) = word(b, &mut at, true)? else { return Err(eof()) };
        let null = !quoted && v.eq_ignore_ascii_case(b"null");
        out.push((utf8(k, "key")?, if null { None } else { Some(utf8(v, "value")?) }));
        loop {
            match b.get(at) {
                Some(b',') => break,
                None => return Ok(unique(out)),
                Some(c) if space(*c) => at += 1,
                _ => return Err(syntax(b, at)),
            }
        }
        at += 1;
    }
    Ok(unique(out))
}

/// In hstore's order, each key once: the first of those written.
fn unique(mut p: Pairs) -> Pairs {
    p.sort_by(|a, b| (a.0.len(), a.0.as_bytes()).cmp(&(b.0.len(), b.0.as_bytes())));
    p.dedup_by(|later, first| later.0 == first.0);
    p
}

pub(super) fn text(p: &[Pair]) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let pairs: Vec<String> =
        p.iter().map(|(k, v)| format!("{}=>{}", q(k), v.as_deref().map_or("NULL".into(), q))).collect();
    pairs.join(", ")
}

/// An hstore's text as Postgres writes it back.
pub(super) fn canonical(s: &str) -> Result<String> {
    Ok(text(&parse(s)?))
}

/// An array's elements, a nested array's too, each its text or None for NULL.
fn items(v: &Value) -> Result<Vec<Option<String>>> {
    if let Value::Text(t) = v
        && t.trim_start().starts_with("{{")
    {
        let mut flat = vec![];
        super::map_array_text(t, &mut |x| {
            flat.push(x.clone());
            Ok(x)
        })?;
        return Ok(flat);
    }
    let vals = super::array_values(v.clone())?.unwrap_or_default();
    Ok(vals
        .into_iter()
        .map(|x| match x {
            Value::Null => None,
            x => Some(x.to_text()),
        })
        .collect())
}

/// How many elements the first row of a two-dimensional array's text has.
fn width(t: &str) -> usize {
    let (mut n, mut depth, mut quoted, mut chars) = (1, 0, false, t.chars());
    while let Some(c) = chars.next() {
        match c {
            '\\' if quoted => {
                chars.next();
            }
            '"' => quoted = !quoted,
            '{' if !quoted => depth += 1,
            '}' if !quoted => return n,
            ',' if !quoted && depth == 2 => n += 1,
            _ => {}
        }
    }
    n
}

/// An array's dimensions as their lower bounds: `[0:1]={a,b}` is [0], `{{a},{b}}` [1, 1] (each 1 where none is
/// written). What two arrays given together must agree on, besides their elements' count.
fn bounds(v: &Value) -> Vec<i64> {
    let t = v.to_text();
    let t = t.trim();
    let (written, body) = match t.starts_with('[') {
        true => t.split_once('=').map_or(("", t), |(b, r)| (b, r.trim_start())),
        false => ("", t),
    };
    let dims = body.chars().take_while(|c| *c == '{').count();
    let mut lows: Vec<i64> =
        (written.split('[').skip(1)).filter_map(|d| d.split(':').next()?.trim().parse().ok()).collect();
    lows.resize(dims.max(lows.len()), 1);
    lows
}

fn array(vals: impl Iterator<Item = Option<String>>) -> Value {
    let vals: Vec<Value> = vals.map(|v| v.map_or(Value::Null, Value::Text)).collect();
    Value::Text(super::array_text(vals.iter()))
}

fn find<'a>(p: &'a [Pair], k: &str) -> Option<&'a Option<String>> {
    p.iter().find(|(x, _)| x == k).map(|(_, v)| v)
}

fn key(k: Option<String>) -> Result<String> {
    k.map_or_else(|| err("22004", "null value not allowed for hstore key"), Ok)
}

/// What JSON holds for a value read loosely: a number where it is written as JSON writes one, `t` and `f` as
/// booleans, else its text.
fn loose(v: &str) -> Json {
    match v {
        "t" => return Json::Bool(true),
        "f" => return Json::Bool(false),
        _ => {}
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    let body = v.strip_prefix('-').unwrap_or(v);
    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (whole, frac) = match mantissa.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (mantissa, None),
    };
    let number = digits(whole)
        && (whole == "0" || !whole.starts_with('0'))
        && frac.is_none_or(digits)
        && exp.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    match number.then(|| v.parse::<serde_json::Number>().ok()).flatten() {
        Some(n) => Json::Number(n),
        None => Json::String(v.into()),
    }
}

pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    use Value::{Bool, Null, Text};
    let Scalar::Hstore(k) = f else { return err("XX000", "not an hstore function") };
    let opt = |v: &Value| if *v == Null { None } else { Some(v.to_text()) };
    // hstore(key, NULL) is the key with no value, and hstore(keys, NULL) each of them with none
    match k {
        MAKE if a.len() == 2 => {
            return Ok(match opt(&a[0]) {
                Some(key) => Text(text(&[(key, opt(&a[1]))])),
                None => Null,
            });
        }
        MAKE_ARRAYS => {
            if a[0] == Null {
                return Ok(Null);
            }
            let keys = items(&a[0])?;
            let vals = if a[1] == Null { vec![None; keys.len()] } else { items(&a[1])? };
            if keys.len() != vals.len() || (a[1] != Null && bounds(&a[0]) != bounds(&a[1])) {
                return err("2202E", "arrays must have same bounds");
            }
            let pairs = keys.into_iter().zip(vals).map(|(k, v)| Ok((key(k)?, v))).collect::<Result<Pairs>>()?;
            return Ok(Text(text(&unique(pairs))));
        }
        _ => {}
    }
    if a.contains(&Null) {
        return Ok(Null);
    }
    let h = |i: usize| parse(&a[i].to_text());
    let hs = |p: Pairs| Text(text(&p));
    Ok(match k {
        // (one argument: its text read as an hstore, or a text[] the binder couldn't tell was one)
        MAKE => match a[0].to_text() {
            t if t.trim_start().starts_with('{') => return call(Scalar::Hstore(MAKE_ARRAY), a),
            t => Text(canonical(&t)?),
        },
        MAKE_ARRAY => {
            let two = matches!(&a[0], Text(t) if t.trim_start().starts_with("{{"));
            let flat = items(&a[0])?;
            if matches!(&a[0], Text(t) if two && width(t) != 2) || flat.len() % 2 != 0 {
                return err(
                    "2202E",
                    if two { "array must have two columns" } else { "array must have even number of elements" },
                );
            }
            let mut pairs: Pairs = vec![];
            for kv in flat.chunks(2) {
                pairs.push((key(kv[0].clone())?, kv[1].clone()));
            }
            hs(unique(pairs))
        }
        FETCH => find(&h(0)?, &a[1].to_text()).cloned().flatten().map_or(Null, Text),
        // (the values in the keys' array's shape: its dimensions and its bounds, `[0:1]={1,2}`)
        FETCH_MANY => {
            let p = h(0)?;
            match &a[1] {
                Text(t) => {
                    Text(super::map_array_text(t, &mut |k| Ok(k.and_then(|k| find(&p, &k).cloned().flatten())))?)
                }
                keys => array(items(keys)?.into_iter().map(|k| k.and_then(|k| find(&p, &k).cloned().flatten()))),
            }
        }
        CONCAT => {
            let (mut p, q) = (h(0)?, h(1)?);
            p.retain(|(k, _)| find(&q, k).is_none());
            p.extend(q);
            hs(unique(p))
        }
        EXIST => Bool(find(&h(0)?, &a[1].to_text()).is_some()),
        DEFINED => Bool(matches!(find(&h(0)?, &a[1].to_text()), Some(Some(_)))),
        EXIST_ANY | EXIST_ALL => {
            let (p, keys) = (h(0)?, items(&a[1])?);
            let has = |k: &Option<String>| k.as_deref().is_some_and(|k| find(&p, k).is_some());
            // (a NULL in the array is no key: ?& skips it, as Postgres does)
            Bool(if k == EXIST_ANY { keys.iter().any(has) } else { keys.iter().filter(|k| k.is_some()).all(has) })
        }
        CONTAINS | CONTAINED => {
            let (p, q) = if k == CONTAINS { (h(0)?, h(1)?) } else { (h(1)?, h(0)?) };
            Bool(q.iter().all(|(k, v)| find(&p, k) == Some(v)))
        }
        DELETE_KEY => {
            let (mut p, key) = (h(0)?, a[1].to_text());
            p.retain(|(k, _)| *k != key);
            hs(p)
        }
        DELETE_KEYS => {
            let (mut p, keys) = (h(0)?, items(&a[1])?);
            p.retain(|(k, _)| !keys.iter().any(|x| x.as_deref() == Some(k)));
            hs(p)
        }
        DELETE_PAIRS => {
            let (mut p, q) = (h(0)?, h(1)?);
            p.retain(|(k, v)| find(&q, k) != Some(v));
            hs(p)
        }
        // (delete(h, x) the binder couldn't type: x read as an array of keys or as one)
        DELETE => match a[1].to_text() {
            t if t.trim_start().starts_with('{') => return call(Scalar::Hstore(DELETE_KEYS), a),
            _ => return call(Scalar::Hstore(DELETE_KEY), a),
        },
        SLICE => {
            let (p, keys) = (h(0)?, items(&a[1])?);
            hs(p.into_iter().filter(|(k, _)| keys.iter().any(|x| x.as_deref() == Some(k))).collect())
        }
        AKEYS => array(h(0)?.into_iter().map(|(k, _)| Some(k))),
        AVALS => array(h(0)?.into_iter().map(|(_, v)| v)),
        TO_ARRAY => array(h(0)?.into_iter().flat_map(|(k, v)| [Some(k), v])),
        TO_MATRIX => {
            let rows: Vec<String> = h(0)?
                .iter()
                .map(|(k, v)| {
                    format!("{{{},{}}}", super::array_elem(k), v.as_deref().map_or("NULL".into(), super::array_elem))
                })
                .collect();
            Text(if rows.is_empty() { "{}".into() } else { format!("{{{}}}", rows.join(",")) })
        }
        TO_JSON | TO_JSON_LOOSE => {
            let mut o = serde_json::Map::new();
            for (key, v) in h(0)? {
                let v = match v {
                    None => Json::Null,
                    Some(v) if k == TO_JSON_LOOSE => loose(&v),
                    Some(v) => Json::String(v),
                };
                o.insert(key, v);
            }
            Value::Json(Json::Object(o))
        }
        _ => return err("XX000", "not an hstore function"),
    })
}

/// From the binary form (see `super::hstore_from_wire`): the text it is kept as.
pub(super) fn from_wire(b: &[u8]) -> Option<String> {
    let int = |at: &mut usize| -> Option<i32> {
        let n = i32::from_be_bytes(b.get(*at..*at + 4)?.try_into().ok()?);
        *at += 4;
        Some(n)
    };
    let field = |at: &mut usize, len: usize| -> Option<String> {
        let s = String::from_utf8(b.get(*at..at.checked_add(len)?)?.to_vec()).ok()?;
        *at += len;
        Some(s)
    };
    let mut at = 0;
    let n = int(&mut at)?;
    let mut pairs: Pairs = vec![];
    for _ in 0..n {
        let len = usize::try_from(int(&mut at)?).ok()?;
        let key = field(&mut at, len)?;
        let val = match int(&mut at)? {
            len if len < 0 => None,
            len => Some(field(&mut at, len as usize)?),
        };
        pairs.push((key, val));
    }
    (at == b.len()).then(|| text(&unique(pairs)))
}

/// The binary form of an hstore's text.
pub(super) fn to_wire(s: &str) -> Option<Vec<u8>> {
    let pairs = parse(s).ok()?;
    let mut out = (pairs.len() as i32).to_be_bytes().to_vec();
    for (k, v) in pairs {
        out.extend((k.len() as i32).to_be_bytes());
        out.extend(k.as_bytes());
        match v {
            Some(v) => {
                out.extend((v.len() as i32).to_be_bytes());
                out.extend(v.as_bytes());
            }
            None => out.extend((-1i32).to_be_bytes()),
        }
    }
    Some(out)
}

/// The rows of skeys, svals and each.
pub(super) fn rows(name: &str, h: &Value) -> Result<Vec<Vec<Value>>> {
    if *h == Value::Null {
        return Ok(vec![]);
    }
    let val = |v: Option<String>| v.map_or(Value::Null, Value::Text);
    Ok(parse(&h.to_text())?
        .into_iter()
        .map(|(k, v)| match name {
            "skeys" => vec![Value::Text(k)],
            "svals" => vec![val(v)],
            _ => vec![Value::Text(k), val(v)],
        })
        .collect())
}
