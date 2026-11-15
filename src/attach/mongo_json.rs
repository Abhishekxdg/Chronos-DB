//! BSON values as MongoDB's relaxed Extended JSON, and back, for `mongo`. Written here rather than taken from the
//! `bson` crate: its own conversion needs `serde_json` built with ordered maps, which would change how every JSON
//! object in the engine keeps its keys.

use mongodb::bson::{self, Bson, Document, oid::ObjectId};
use serde_json::{Value as Json, json};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64(data: &[u8]) -> String {
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    let mut out = vec![];
    let (mut acc, mut bits) = (0u32, 0);
    for ch in text.bytes().filter(|b| *b != b'=' && !b.is_ascii_whitespace()) {
        acc = acc << 6 | B64.iter().position(|b| *b == ch)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Days since 1970-01-01 as (year, month, day).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = ((doy - (153 * mp + 2) / 5 + 1) as u32, if mp < 10 { mp + 3 } else { mp - 9 } as u32);
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Milliseconds since 1970 as `2026-10-01T10:00:00Z` (with `.123` when it has milliseconds), for years 1970 to 9999.
fn iso(ms: i64) -> Option<String> {
    let (day, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil(day);
    if !(1970..=9999).contains(&y) {
        return None;
    }
    let (h, mi, s, f) = (rest / 3_600_000, rest / 60_000 % 60, rest / 1000 % 60, rest % 1000);
    let frac = if f == 0 { String::new() } else { format!(".{f:03}") };
    Some(format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}{frac}Z"))
}

/// An RFC 3339 time (`2026-10-01T10:00:00.5+05:30`) as milliseconds since 1970.
fn uniso(text: &str) -> Option<i64> {
    let t = text.trim();
    let (date, time) = t.split_once(['T', 't', ' '])?;
    let mut p = date.split('-');
    let (y, m, d): (i64, u32, u32) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?);
    let (clock, offset) = match time.find(['Z', 'z', '+', '-']) {
        Some(i) => (&time[..i], &time[i..]),
        None => (time, "Z"),
    };
    let off_min = match offset {
        "Z" | "z" => 0,
        o => {
            let (h, mi) = o[1..].split_once(':').unwrap_or((&o[1..], "0"));
            let n = h.parse::<i64>().ok()? * 60 + mi.parse::<i64>().ok()?;
            if o.starts_with('-') { -n } else { n }
        }
    };
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let mut c = hms.split(':');
    let (h, mi, s): (i64, i64, i64) =
        (c.next()?.parse().ok()?, c.next()?.parse().ok()?, c.next().unwrap_or("0").parse().ok()?);
    let f: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(days(y, m, d) * 86_400_000 + ((h * 60 + mi - off_min) * 60 + s) * 1000 + f)
}

/// A BSON value as relaxed Extended JSON: numbers plain, the other types as `{"$type": ...}`.
pub(super) fn ext(b: &Bson) -> Json {
    match b {
        Bson::Double(f) if f.is_finite() => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Bson::Double(f) => {
            json!({"$numberDouble": if f.is_nan() { "NaN" } else if *f > 0.0 { "Infinity" } else { "-Infinity" }})
        }
        Bson::String(s) => Json::String(s.clone()),
        Bson::Boolean(v) => Json::Bool(*v),
        Bson::Null => Json::Null,
        Bson::Int32(n) => json!(n),
        Bson::Int64(n) => json!(n),
        Bson::Array(a) => Json::Array(a.iter().map(ext).collect()),
        Bson::Document(d) => ext_doc(d),
        Bson::ObjectId(o) => json!({"$oid": o.to_hex()}),
        Bson::DateTime(t) => match iso(t.timestamp_millis()) {
            Some(s) => json!({"$date": s}),
            None => json!({"$date": {"$numberLong": t.timestamp_millis().to_string()}}),
        },
        Bson::Decimal128(d) => json!({"$numberDecimal": d.to_string()}),
        Bson::Binary(b) => {
            json!({"$binary": {"base64": b64(&b.bytes), "subType": format!("{:02x}", u8::from(b.subtype))}})
        }
        Bson::Timestamp(t) => json!({"$timestamp": {"t": t.time, "i": t.increment}}),
        Bson::RegularExpression(r) => {
            json!({"$regularExpression": {"pattern": r.pattern.to_string(), "options": r.options.to_string()}})
        }
        Bson::MinKey => json!({"$minKey": 1}),
        Bson::MaxKey => json!({"$maxKey": 1}),
        Bson::Undefined => json!({"$undefined": true}),
        Bson::Symbol(s) => json!({"$symbol": s}),
        Bson::JavaScriptCode(c) => json!({"$code": c}),
        Bson::JavaScriptCodeWithScope(c) => json!({"$code": c.code, "$scope": ext_doc(&c.scope)}),
        Bson::DbPointer(_) => json!({"$dbPointer": "a DBPointer (an old type): not shown"}),
    }
}

pub(super) fn ext_doc(d: &Document) -> Json {
    Json::Object(d.iter().map(|(k, v)| (k.clone(), ext(v))).collect())
}

/// The BSON value an Extended JSON object (`{"$oid": ...}`) stands for. Err: not one this writes.
pub(super) fn unext(o: &serde_json::Map<String, Json>) -> Result<Bson, String> {
    let (key, v) = o.iter().next().ok_or("an empty object")?;
    let text = || v.as_str().ok_or_else(|| format!("{key} takes a string"));
    let bad = |what: &str| format!("{} isn't {what}", Json::Object(o.clone()));
    Ok(match key.as_str() {
        "$oid" => Bson::ObjectId(ObjectId::parse_str(text()?).map_err(|_| bad("an ObjectId (24 hex digits)"))?),
        "$date" => {
            let ms = match v {
                Json::String(s) => uniso(s).ok_or_else(|| bad("a date (2026-10-01T10:00:00Z)"))?,
                Json::Number(n) => n.as_i64().ok_or_else(|| bad("a date"))?,
                Json::Object(inner) => inner
                    .get("$numberLong")
                    .and_then(Json::as_str)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| bad("a date"))?,
                _ => return Err(bad("a date")),
            };
            Bson::DateTime(bson::DateTime::from_millis(ms))
        }
        "$numberLong" => Bson::Int64(text()?.parse().map_err(|_| bad("a long"))?),
        "$numberInt" => Bson::Int32(text()?.parse().map_err(|_| bad("an int"))?),
        "$numberDouble" => Bson::Double(match text()? {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            s => s.parse().map_err(|_| bad("a double"))?,
        }),
        "$numberDecimal" => Bson::Decimal128(text()?.parse().map_err(|_| bad("a decimal"))?),
        "$binary" => {
            let bytes = v["base64"].as_str().and_then(unb64).ok_or_else(|| bad("binary data (base64)"))?;
            let sub = u8::from_str_radix(v["subType"].as_str().unwrap_or("00"), 16).map_err(|_| bad("binary data"))?;
            Bson::Binary(bson::Binary { subtype: sub.into(), bytes })
        }
        "$timestamp" => {
            let part = |k: &str| v[k].as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| bad("a timestamp"));
            Bson::Timestamp(bson::Timestamp { time: part("t")?, increment: part("i")? })
        }
        "$minKey" => Bson::MinKey,
        "$maxKey" => Bson::MaxKey,
        "$undefined" => Bson::Undefined,
        "$symbol" => Bson::Symbol(text()?.to_string()),
        "$code" if o.len() == 1 => Bson::JavaScriptCode(text()?.to_string()),
        other => {
            return Err(format!(
                "{other} isn't a type a merge writes to MongoDB (it writes $oid, $date, $numberLong, $numberInt, \
                 $numberDouble, $numberDecimal, $binary, $timestamp)"
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_binary_round_trip() {
        assert_eq!(iso(0).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(iso(1_790_000_000_123).as_deref(), Some("2026-09-21T14:13:20.123Z"));
        for ms in [0, 1_790_000_000_123, 951_782_400_000, 4_102_444_799_999, 68_256_000_000] {
            assert_eq!(uniso(&iso(ms).unwrap()), Some(ms));
        }
        assert_eq!(uniso("2026-10-01T10:00:00+05:30"), uniso("2026-10-01T04:30:00Z"));
        assert_eq!(uniso("2026-10-01 10:00:00.5Z"), uniso("2026-10-01T10:00:00.500Z"));
        assert!(uniso("yesterday").is_none());
        for data in [&b""[..], b"a", b"ab", b"abc", b"\x00\xff\x10\x80"] {
            assert_eq!(unb64(&b64(data)).as_deref(), Some(data));
        }
        assert_eq!(b64(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn values_round_trip_through_extended_json() {
        let oid = ObjectId::parse_str("65f1a2b3c4d5e6f708192a3b").unwrap();
        for b in [
            Bson::ObjectId(oid),
            Bson::DateTime(bson::DateTime::from_millis(1_790_000_000_123)),
            Bson::Decimal128("1.50".parse().unwrap()),
            Bson::Timestamp(bson::Timestamp { time: 7, increment: 2 }),
            Bson::Binary(bson::Binary { subtype: bson::spec::BinarySubtype::Generic, bytes: vec![1, 2, 3] }),
        ] {
            let Json::Object(o) = ext(&b) else { panic!("{b:?}") };
            assert_eq!(unext(&o).unwrap(), b, "{o:?}");
        }
        assert_eq!(ext(&Bson::Double(1.5)), json!(1.5));
        assert_eq!(ext(&Bson::Int64(5_000_000_000)), json!(5_000_000_000_i64));
        assert!(unext(json!({"$dbPointer": "x"}).as_object().unwrap()).is_err());
    }
}
