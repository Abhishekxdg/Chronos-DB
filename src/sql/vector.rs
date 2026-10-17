//! pgvector's `vector`: 4-byte floats, as pgvector keeps them. Written '[1,2,3]' as pgvector
//! writes it, or as an array ('{1,2,3}', ARRAY[1,2,3]: how drivers send lists) or a JSON array.
//! Packed rows hold the floats' bytes in base64 (see `sql::pack`), so a 1,536-dimension vector
//! is 8 KB rather than ~17 KB of decimal text, and reading it parses no numbers. Outside SQL it's
//! a JSON array, so the search index sees the column as a vector field (see `index`).

use super::decimal::R;
use serde_json::Value as Json;
use std::sync::Arc;

pub const MAX_DIM: u32 = 16_000; // pgvector's limit

pub type Vector = Arc<[f32]>;

/// The numbers of '[1,2,3]' or '{1,2,3}'.
pub fn parse(s: &str) -> Option<Vec<f32>> {
    let s = s.trim();
    let inner = s
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .or_else(|| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))?;
    if inner.trim().is_empty() {
        return Some(vec![]);
    }
    inner.split(',').map(|x| x.trim().parse::<f32>().ok().filter(|f| f.is_finite())).collect()
}

/// The numbers of a JSON array, if it's all finite numbers.
pub fn from_json(j: &Json) -> Option<Vec<f32>> {
    j.as_array()?.iter().map(|x| x.as_f64().map(|f| f as f32).filter(|f| f.is_finite())).collect()
}

/// A vector value: 1 to 16,000 dimensions, as pgvector requires.
pub fn check(v: Vec<f32>) -> R<Vector> {
    if v.is_empty() {
        return Err("vector must have at least 1 dimension");
    }
    if v.len() > MAX_DIM as usize {
        return Err("vector cannot have more than 16000 dimensions");
    }
    Ok(v.into())
}

/// As pgvector writes it: [1,0.5,-2], each number as short as it can be and still read back.
pub fn text(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 10 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

/// The floats' little-endian bytes, 7 bits to a character: 7 bytes make 8 characters, all
/// ASCII (0 to 127), so a vector is text 8/7 its size. A tail's bits are written out whole,
/// zeros after them.
pub fn pack_into(v: &[f32], s: &mut String) {
    let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    let mut out = Vec::with_capacity(packed_len(v.len()));
    for c in bytes.chunks(7) {
        let mut w = [0u8; 8];
        w[1..1 + c.len()].copy_from_slice(c);
        let n = u64::from_be_bytes(w); // the 56 bits, the first byte's highest
        out.extend((0..(c.len() * 8).div_ceil(7)).map(|i| (n >> (49 - 7 * i) & 127) as u8));
    }
    s.push_str(std::str::from_utf8(&out).expect("7-bit text is ASCII"));
}

#[cfg(test)]
pub fn pack(v: &[f32]) -> String {
    let mut s = String::new();
    pack_into(v, &mut s);
    s
}

/// How long `pack_into` makes a vector of `dims` floats.
pub fn packed_len(dims: usize) -> usize {
    let bytes = dims * 4;
    bytes / 7 * 8 + (bytes % 7 * 8).div_ceil(7)
}

/// `pack_into`'s text back to the floats; None unless it's exactly such text.
pub fn unpack(s: &str) -> Option<Vector> {
    let src = s.as_bytes();
    let mut bytes = Vec::with_capacity(src.len() / 8 * 7 + 7);
    for c in src.chunks(8) {
        let mut n = 0u64;
        for (i, &b) in c.iter().enumerate() {
            if b > 127 {
                return None;
            }
            n |= (b as u64) << (49 - 7 * i);
        }
        bytes.extend_from_slice(&n.to_be_bytes()[1..1 + c.len() * 7 / 8]);
    }
    if bytes.len() % 4 != 0 || packed_len(bytes.len() / 4) != src.len() {
        return None;
    }
    Some(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

/// The first packing, which rows written before 0.1.0 hold: the floats' little-endian bytes in
/// base64, no padding (a tail's 1 or 2 bytes are 2 or 3 characters).
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Each base64 character's value plus one (0: not base64).
const B64_VAL: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 64 {
        t[B64[i] as usize] = i as u8 + 1;
        i += 1;
    }
    t
};

pub fn unpack_base64(s: &str) -> Option<Vector> {
    let src = s.as_bytes();
    let mut bytes = Vec::with_capacity(src.len() / 4 * 3 + 3);
    for c in src.chunks(4) {
        if c.len() == 1 {
            return None; // a lone character holds no whole byte
        }
        let mut n = 0u32;
        for (i, &b) in c.iter().enumerate() {
            n |= ((B64_VAL[b as usize].checked_sub(1)?) as u32) << (18 - 6 * i);
        }
        bytes.extend_from_slice(&n.to_be_bytes()[1..c.len()]);
    }
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

pub enum Dist {
    Cosine, // <=>: 1 - cosine similarity
    L2,     // <->: Euclidean distance
    NegDot, // <#>: the negative inner product, so that smaller is closer
}

/// Summed in doubles, as pgvector does for cosine distance.
pub fn distance(d: Dist, a: &[f32], b: &[f32]) -> std::result::Result<f64, String> {
    if a.len() != b.len() {
        return Err(format!("different vector dimensions {} and {}", a.len(), b.len()));
    }
    let (mut dot, mut na, mut nb, mut l2) = (0f64, 0f64, 0f64, 0f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (x as f64, y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
        l2 += (x - y) * (x - y);
    }
    Ok(match d {
        Dist::NegDot => -dot,
        Dist::L2 => l2.sqrt(),
        Dist::Cosine => {
            let norms = (na * nb).sqrt();
            if norms == 0.0 { f64::NAN } else { 1.0 - (dot / norms).clamp(-1.0, 1.0) }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_measures() {
        assert_eq!(parse("[1, 2.5,-3]"), Some(vec![1.0, 2.5, -3.0]));
        assert_eq!(parse("{1,2}"), Some(vec![1.0, 2.0]));
        assert_eq!(parse("[1,x]"), None);
        assert_eq!(parse("[1,NaN]"), None);
        assert!(check(vec![]).is_err());
        assert_eq!(text(&[1.0, 0.1, -2.5, 0.0]), "[1,0.1,-2.5,0]");
        let (a, b) = ([1.0, 0.0], [0.0, 2.0]);
        assert_eq!(distance(Dist::Cosine, &a, &b), Ok(1.0));
        assert_eq!(distance(Dist::Cosine, &a, &[3.0, 0.0]), Ok(0.0));
        assert_eq!(distance(Dist::L2, &a, &b), Ok(5f64.sqrt()));
        assert_eq!(distance(Dist::NegDot, &[1.0, 2.0], &[3.0, 4.0]), Ok(-11.0));
        assert!(distance(Dist::Cosine, &[0.0, 0.0], &a).unwrap().is_nan());
        assert!(distance(Dist::L2, &a, &[1.0]).is_err());
    }

    #[test]
    fn packs_exactly() {
        for n in [1, 2, 3, 5, 7, 1536] {
            let mut v: Vec<f32> = (0..n).map(|i| (i as f32 - 7.3) / 3.0).collect();
            v[0] = if n > 3 { f32::NAN } else { -0.0 };
            let s = pack(&v);
            assert!(s.bytes().all(|b| b < 128));
            assert_eq!(s.len(), packed_len(n));
            let back = unpack(&s).unwrap();
            assert!(back.iter().zip(&v).all(|(a, b)| a.to_bits() == b.to_bits()), "{n}");
            // what the first packing wrote still reads
            let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            let mut first = String::new();
            for c in bytes.chunks(3) {
                let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
                for i in 0..=c.len() {
                    first.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
                }
            }
            let back = unpack_base64(&first).unwrap();
            assert!(back.iter().zip(&v).all(|(a, b)| a.to_bits() == b.to_bits()), "{n}");
        }
        assert_eq!(packed_len(1536), 7022); // base64 took 8192
        assert_eq!(pack(&[1.0]).len(), 5); // 4 bytes: 5 characters
        assert!(unpack("A").is_none() && unpack("\u{80}AAAA").is_none() && unpack(&pack(&[1.0])[..4]).is_none());
        assert!(unpack_base64("A").is_none() && unpack_base64("A!AA").is_none());
    }
}
