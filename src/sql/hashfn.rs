//! Postgres's hash functions (`hashint4`, `hashtext`, ... and their `extended` forms): Bob Jenkins' lookup3 as
//! `hash_bytes` and `hash_uint32` run it, so the numbers match a real server's.

use super::funcs::Scalar;
use super::{SqlError, Value, err};

type Result<T> = std::result::Result<T, SqlError>;

fn rot(x: u32, k: u32) -> u32 {
    x.rotate_left(k)
}

fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c);
    *a ^= rot(*c, 4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= rot(*a, 6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= rot(*b, 8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c);
    *a ^= rot(*c, 16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= rot(*a, 19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= rot(*b, 4);
    *b = b.wrapping_add(*a);
}

fn fin(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b;
    *c = c.wrapping_sub(rot(*b, 14));
    *a ^= *c;
    *a = a.wrapping_sub(rot(*c, 11));
    *b ^= *a;
    *b = b.wrapping_sub(rot(*a, 25));
    *c ^= *b;
    *c = c.wrapping_sub(rot(*b, 16));
    *a ^= *c;
    *a = a.wrapping_sub(rot(*c, 4));
    *b ^= *a;
    *b = b.wrapping_sub(rot(*a, 14));
    *c ^= *b;
    *c = c.wrapping_sub(rot(*b, 24));
}

/// `hash_bytes_extended`: the 64-bit hash with a seed (its low 32 bits are `hash_bytes` for seed 0).
pub(super) fn hash_bytes(k: &[u8], seed: u64) -> (u32, u64) {
    let len = k.len();
    let init = 0x9e37_79b9u32.wrapping_add(len as u32).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        mix(&mut a, &mut b, &mut c);
    }
    let le = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    let mut rest = k;
    while rest.len() >= 12 {
        a = a.wrapping_add(le(&rest[0..4]));
        b = b.wrapping_add(le(&rest[4..8]));
        c = c.wrapping_add(le(&rest[8..12]));
        mix(&mut a, &mut b, &mut c);
        rest = &rest[12..];
    }
    let n = rest.len();
    let byte = |i: usize| u32::from(rest[i]);
    if n >= 11 {
        c = c.wrapping_add(byte(10) << 24);
    }
    if n >= 10 {
        c = c.wrapping_add(byte(9) << 16);
    }
    if n >= 9 {
        c = c.wrapping_add(byte(8) << 8);
    }
    if n >= 8 {
        b = b.wrapping_add(byte(7) << 24);
    }
    if n >= 7 {
        b = b.wrapping_add(byte(6) << 16);
    }
    if n >= 6 {
        b = b.wrapping_add(byte(5) << 8);
    }
    if n >= 5 {
        b = b.wrapping_add(byte(4));
    }
    if n >= 4 {
        a = a.wrapping_add(byte(3) << 24);
    }
    if n >= 3 {
        a = a.wrapping_add(byte(2) << 16);
    }
    if n >= 2 {
        a = a.wrapping_add(byte(1) << 8);
    }
    if n >= 1 {
        a = a.wrapping_add(byte(0));
    }
    fin(&mut a, &mut b, &mut c);
    (c, (u64::from(b) << 32) | u64::from(c))
}

/// `hash_uint32_extended`.
pub(super) fn hash_u32(k: u32, seed: u64) -> (u32, u64) {
    let init = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        mix(&mut a, &mut b, &mut c);
    }
    a = a.wrapping_add(k);
    fin(&mut a, &mut b, &mut c);
    (c, (u64::from(b) << 32) | u64::from(c))
}

/// The names, in order of `Scalar::HashFn`'s kinds (each also as `<name>extended`).
pub(super) const NAMES: [&str; 12] = [
    "hashint2",
    "hashint4",
    "hashint8",
    "hashoid",
    "hashchar",
    "hashfloat4",
    "hashfloat8",
    "hashtext",
    "hashbpchar",
    "hashname",
    "hashenum",
    // (its extended form is interval_hash_extended)
    "interval_hash",
];

pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    use Value::{Int, Null};
    let Scalar::HashFn(k, ext) = f else { return err("XX000", "not a hash function") };
    if a.contains(&Null) {
        return Ok(Null);
    }
    let seed = if ext {
        match &a[1] {
            Int(s) => *s as u64,
            v => v.to_text().parse::<i64>().unwrap_or(0) as u64,
        }
    } else {
        0
    };
    let out = |h: (u32, u64)| Ok(if ext { Int(h.1 as i64) } else { Int(i64::from(h.0 as i32)) });
    let int = |v: &Value| -> i64 {
        match v {
            Int(n) => *n,
            v => v.to_text().parse().unwrap_or(0),
        }
    };
    match NAMES[k as usize % NAMES.len()] {
        "hashint2" | "hashint4" | "hashoid" | "hashenum" => out(hash_u32(int(&a[0]) as i32 as u32, seed)),
        "hashchar" => out(hash_u32(int(&a[0]) as i8 as i32 as u32, seed)),
        "hashint8" => {
            let v = int(&a[0]);
            let (mut lo, hi) = (v as u32, (v >> 32) as u32);
            lo ^= if v >= 0 { hi } else { !hi };
            out(hash_u32(lo, seed))
        }
        // an interval hashes as hashint8 of the low 64 bits of its span (a month 30 days), as interval_hash does:
        // '30 days' and '1 month' hash alike, as they are equal
        "interval_hash" => {
            let i = match &a[0] {
                Value::Interval(i) => *i,
                v => super::time::parse_interval(&v.to_text()).unwrap_or_default(),
            };
            let v = i.total() as i64;
            let (mut lo, hi) = (v as u32, (v >> 32) as u32);
            lo ^= if v >= 0 { hi } else { !hi };
            out(hash_u32(lo, seed))
        }
        "hashfloat4" | "hashfloat8" => {
            let f = match &a[0] {
                Value::Float(f) => *f,
                v => v.to_text().parse().unwrap_or(0.0),
            };
            if f == 0.0 {
                return out((0, seed));
            }
            if NAMES[k as usize % NAMES.len()] == "hashfloat4" {
                let x = if f.is_nan() { f32::NAN } else { f as f32 };
                out(hash_bytes(&x.to_bits().to_le_bytes(), seed))
            } else {
                let x = if f.is_nan() { f64::NAN } else { f };
                out(hash_bytes(&x.to_bits().to_le_bytes(), seed))
            }
        }
        "hashbpchar" => out(hash_bytes(a[0].to_text().trim_end_matches(' ').as_bytes(), seed)),
        _ => out(hash_bytes(a[0].to_text().as_bytes(), seed)),
    }
}
