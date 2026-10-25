//! Binary strings (`bytea`): a value is its bytes. Its text form is Postgres's `bytea_output =
//! hex`, a backslash, `x` and the bytes in hex (`\xdeadbeef`), which reads back; so does the older
//! escape form (`abc\000def`, a backslash written `\\`). It orders bytewise, as Postgres's does.

use super::*;
use funcs::Scalar;

thread_local! {
    /// bytea_output = escape (the session's setting; see `Session::use_zone`)
    pub(super) static ESCAPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The text form: `\x` and each byte in two lowercase hex digits (or, with bytea_output = escape, printable
/// characters as they are, a backslash doubled, and the rest as `\ooo` in octal).
pub(super) fn text(b: &[u8]) -> String {
    if ESCAPE.get() {
        let mut out = String::with_capacity(b.len());
        for &x in b {
            match x {
                b'\\' => out.push_str("\\\\"),
                0x20..=0x7e => out.push(x as char),
                _ => out.push_str(&format!("\\{x:03o}")),
            }
        }
        return out;
    }
    let mut out = String::with_capacity(2 + b.len() * 2);
    out.push_str("\\x");
    hex_into(b, &mut out);
    out
}

/// The bytes' hex digits appended to `out`, two lowercase digits each.
pub(super) fn hex_into(b: &[u8], out: &mut String) {
    use std::fmt::Write;
    out.reserve(b.len() * 2);
    for x in b {
        let _ = write!(out, "{x:02x}");
    }
}

/// The bytes of hex digits (as `hex_into` writes them); None for anything else.
pub(super) fn from_hex(s: &str) -> Option<Vec<u8>> {
    let d = s.as_bytes();
    if !d.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    d.chunks_exact(2).map(|p| Some(digit(p[0])? << 4 | digit(p[1])?)).collect()
}

/// 22P02: what Postgres says of an escape-form bytea it can't read.
fn bad_input() -> SqlError {
    SqlError { code: BAD_INPUT, message: "invalid input syntax for type bytea".into(), info: None }
}

fn invalid(message: String) -> SqlError {
    SqlError { code: "22023", message, info: None }
}

/// A bytea as written: `\x` and hex digits (whitespace between bytes is skipped), or the escape
/// form, where a backslash starts `\\` or three octal digits.
pub(super) fn parse(s: &str) -> Result<Vec<u8>> {
    if let Some(hex) = s.strip_prefix("\\x") {
        return hex_decode(hex);
    }
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
        } else if b.get(i + 1) == Some(&b'\\') {
            out.push(b'\\');
            i += 2;
        } else if let Some(d) =
            b.get(i + 1..i + 4).filter(|d| matches!(d[0], b'0'..=b'3') && d.iter().all(|c| (b'0'..=b'7').contains(c)))
        {
            out.push((d[0] - b'0') << 6 | (d[1] - b'0') << 3 | (d[2] - b'0'));
            i += 4;
        } else {
            return Err(bad_input());
        }
    }
    Ok(out)
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut high: Option<u8> = None;
    for c in s.chars() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let Some(d) = c.to_digit(16) else {
            return Err(invalid(format!("invalid hexadecimal digit: \"{c}\"")));
        };
        match high.take() {
            None => high = Some(d as u8),
            Some(h) => out.push(h << 4 | d as u8),
        }
    }
    if high.is_some() {
        return Err(invalid("invalid hexadecimal data: odd number of digits".into()));
    }
    Ok(out)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `encode(bytes, format)`: `hex`, `base64` (a newline every 76 characters, as Postgres breaks
/// it) or `escape`.
pub(super) fn encode(b: &[u8], format: &str) -> Result<String> {
    Ok(match format.to_lowercase().as_str() {
        "hex" => text(b)[2..].to_string(),
        "base64" => {
            let mut out = String::with_capacity(b.len().div_ceil(3) * 4 + b.len() / 57 + 1);
            let mut line = 0;
            for chunk in b.chunks(3) {
                let n = chunk.iter().enumerate().fold(0u32, |n, (i, x)| n | (*x as u32) << (16 - 8 * i));
                for k in 0..4 {
                    out.push(if k <= chunk.len() { BASE64[(n >> (18 - 6 * k) & 63) as usize] as char } else { '=' });
                    line += 1;
                }
                if line >= 76 {
                    out.push('\n');
                    line = 0;
                }
            }
            if out.ends_with('\n') {
                out.pop(); // (a line ends between groups, not after the last)
            }
            out
        }
        "escape" => {
            let mut out = String::with_capacity(b.len());
            for &x in b {
                match x {
                    0 | 0x80..=0xff => out.push_str(&format!("\\{x:03o}")),
                    b'\\' => out.push_str("\\\\"),
                    x => out.push(x as char),
                }
            }
            out
        }
        other => return Err(invalid(format!("unrecognized encoding: \"{other}\""))),
    })
}

/// `decode(text, format)`, the opposite of `encode`.
pub(super) fn decode(s: &str, format: &str) -> Result<Vec<u8>> {
    match format.to_lowercase().as_str() {
        "hex" => hex_decode(s),
        "escape" => parse_escape(s),
        "base64" => {
            let (mut out, mut acc, mut bits, mut end) = (vec![], 0u32, 0, 0);
            for c in s.bytes() {
                if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                    continue;
                }
                if c == b'=' {
                    end += 1;
                    continue;
                }
                let Some(v) = BASE64.iter().position(|x| *x == c) else {
                    return Err(invalid(format!(
                        "invalid symbol \"{}\" found while decoding base64 sequence",
                        c as char
                    )));
                };
                if end > 0 {
                    return Err(invalid("unexpected \"=\" while decoding base64 sequence".into()));
                }
                acc = acc << 6 | v as u32;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    out.push((acc >> bits) as u8);
                    acc &= (1 << bits) - 1;
                }
            }
            // the last group has to be complete, padded with `=` as Postgres insists on
            let digits = s.bytes().filter(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r')).count();
            if !digits.is_multiple_of(4) || end > 2 || bits != 0 && acc != 0 {
                return Err(SqlError {
                    code: "22023",
                    message: "invalid base64 end sequence".into(),
                    info: Some(Box::new(Violation {
                        detail: Some("Input data is missing padding, is truncated, or is otherwise corrupted.".into()),
                        ..Violation::default()
                    })),
                });
            }
            Ok(out)
        }
        other => Err(invalid(format!("unrecognized encoding: \"{other}\""))),
    }
}

/// The escape form alone (what `decode(s, 'escape')` reads), whatever the text starts with.
fn parse_escape(s: &str) -> Result<Vec<u8>> {
    if s.starts_with("\\x") {
        return Err(bad_input());
    }
    parse(s)
}

/// The bytes a function argument stands for: a bytea's, or a text read as a bytea (a literal that
/// Postgres would have taken as one).
pub(super) fn of(v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Bytea(b) => Ok(b.clone()),
        v => parse(&v.to_text()),
    }
}

fn int_of(v: &Value) -> Result<i64> {
    match coerce(v.clone(), Type::BigInt)? {
        Value::Int(n) => Ok(n),
        _ => err(WRONG_TYPE, "an integer was expected"),
    }
}

/// The part of `b` from position `from` (1-based) for `len` bytes, as `substring` counts:
/// positions before the first still use up length.
fn slice(b: &[u8], from: i64, len: Option<i64>) -> Result<Vec<u8>> {
    if len.is_some_and(|l| l < 0) {
        return err("22011", "negative substring length not allowed");
    }
    let end = len.map_or(i64::MAX, |l| from.saturating_add(l));
    let (start, end) = (from.max(1), end.min(b.len() as i64 + 1));
    Ok(if end <= start { vec![] } else { b[start as usize - 1..end as usize - 1].to_vec() })
}

/// Where `needle` first is in `hay`, from 1; 0 if it isn't there.
fn position(hay: &[u8], needle: &[u8]) -> i64 {
    if needle.is_empty() {
        return 1;
    }
    hay.windows(needle.len()).position(|w| w == needle).map_or(0, |i| i as i64 + 1)
}

/// SHA-224: SHA-256 from another starting state, cut to 28 bytes (`ring` has no SHA-224).
pub(super) fn sha224(data: &[u8]) -> Vec<u8> {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] =
        [0xc1059ed8, 0x367cd507, 0x3070dd17, 0xf70e5939, 0xffc00b31, 0x68581511, 0x64f98fa7, 0xbefa4fa4];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend(((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (x, y) in h.iter_mut().zip(v) {
            *x = x.wrapping_add(y);
        }
    }
    h[..7].iter().flat_map(|x| x.to_be_bytes()).collect()
}

/// The functions that take a bytea (or make one), called with `a`; None for one that doesn't
/// (or doesn't with these arguments): the function goes on as it does for text.
pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Option<Value>> {
    use Scalar::*;
    use Value::{Bytea, Int, Text};
    let any = a.iter().any(|v| matches!(v, Bytea(_)));
    Ok(Some(match f {
        Encode => Text(encode(&of(&a[0])?, &a[1].to_text())?),
        Decode => Bytea(decode(&a[0].to_text(), &a[1].to_text())?),
        Sha224 => Bytea(sha224(&of(&a[0])?)),
        Sha256 | Sha384 | Sha512 => {
            let algorithm = match f {
                Sha256 => &ring::digest::SHA256,
                Sha384 => &ring::digest::SHA384,
                _ => &ring::digest::SHA512,
            };
            Bytea(ring::digest::digest(algorithm, &of(&a[0])?).as_ref().to_vec())
        }
        GetByte => {
            let b = of(&a[0])?;
            let n = int_of(&a[1])?;
            match usize::try_from(n).ok().and_then(|n| b.get(n)) {
                Some(x) => Int(i64::from(*x)),
                None => {
                    return err("2202E", format!("index {n} out of valid range, 0..{}", b.len() as i64 - 1));
                }
            }
        }
        SetByte => {
            let mut b = of(&a[0])?;
            let n = int_of(&a[1])?;
            let Some(x) = usize::try_from(n).ok().and_then(|n| b.get_mut(n)) else {
                return err("2202E", format!("index {n} out of valid range, 0..{}", b.len() as i64 - 1));
            };
            *x = int_of(&a[2])? as u8;
            Bytea(b)
        }
        BitLength => Int(if any { of(&a[0])?.len() as i64 * 8 } else { a[0].to_text().len() as i64 * 8 }),
        // convert_to(text, to) and convert_from(bytes, from): from and to the database's encoding, UTF8; convert(bytes,
        // from, to) between any two. Postgres's pg_convert (see `encoding`).
        ConvertTo => Bytea(encoding::pg_convert(a[0].to_text().as_bytes(), "UTF8", &a[1].to_text())?),
        ConvertFrom => {
            let b = encoding::pg_convert(&of(&a[0])?, &a[1].to_text(), "UTF8")?;
            // (valid UTF8: the conversion checked it)
            Text(String::from_utf8(b).map_err(|e| {
                encoding::report_invalid_encoding(encoding::PG_UTF8, &e.as_bytes()[e.utf8_error().valid_up_to()..])
            })?)
        }
        Convert => Bytea(encoding::pg_convert(&of(&a[0])?, &a[1].to_text(), &a[2].to_text())?),
        TestEncValidlen | TestEncResult => {
            let no_error = matches!(a[3], Value::Bool(true));
            let (valid, out) = encoding::test_enc_conversion(&of(&a[0])?, &a[1].to_text(), &a[2].to_text(), no_error)?;
            if f == TestEncValidlen { Int(valid.into()) } else { Bytea(out) }
        }
        TestEncSetup => Text(encoding::test_enc_setup().join("\n")),
        // from here on, only for a bytea
        _ if !any => return Ok(None),
        OctetLength => Int(of(&a[0])?.len() as i64),
        Md5 => Text(funcs::md5(&of(&a[0])?)),
        Substr => {
            let len = a.get(2).map(int_of).transpose()?;
            Bytea(slice(&of(&a[0])?, int_of(&a[1])?, len)?)
        }
        Strpos | Position => Int(position(&of(&a[0])?, &of(&a[1])?)),
        Overlay => {
            let (b, by) = (of(&a[0])?, of(&a[1])?);
            let from = int_of(&a[2])?;
            let len = a.get(3).map(int_of).transpose()?.unwrap_or(by.len() as i64);
            let mut out = slice(&b, 1, Some(from - 1))?;
            out.extend(by);
            out.extend(slice(&b, from.saturating_add(len), None)?);
            Bytea(out)
        }
        _ => return Ok(None),
    }))
}
