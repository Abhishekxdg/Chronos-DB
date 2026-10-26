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
pub(super) const NAMES: [&str; 22] = [
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
    // (its extended form is interval_hash_extended, as for the others with an underscore: see `extended_name`)
    "interval_hash",
    // date.c's time_hash and timetz_hash, timestamp.c's timestamp_hash, uuid.c's uuid_hash, pg_lsn.c's pg_lsn_hash,
    // network.c's hashinet, mac.c's hashmacaddr, mac8.c's hashmacaddr8, hashfunc.c's hashoidvector, numeric.c's
    // hash_numeric
    "time_hash",
    "timetz_hash",
    "timestamp_hash",
    "uuid_hash",
    "pg_lsn_hash",
    "hashinet",
    "hashmacaddr",
    "hashmacaddr8",
    "hashoidvector",
    "hash_numeric",
];

/// The name of a hash function's extended form: `_extended` after one whose name has an underscore
/// (`interval_hash_extended`, `hash_numeric_extended`), `extended` after the others (`hashint4extended`)
pub(super) fn extended_name(name: &str) -> String {
    if name.contains('_') { format!("{name}_extended") } else { format!("{name}extended") }
}

/// hashint8 and hashint8extended: the low 32 bits, with the high ones folded in so a value that fits in an int4
/// hashes as hashint4 hashes it
fn hash_int8(v: i64, seed: u64) -> (u32, u64) {
    let (mut lo, hi) = (v as u32, (v >> 32) as u32);
    lo ^= if v >= 0 { hi } else { !hi };
    hash_u32(lo, seed)
}

/// Microseconds from Postgres's epoch (2000-01-01) to Unix's: a timestamp's int8 as Postgres stores it
const PG_EPOCH_US: i64 = 946_684_800_000_000;

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
        "hashint8" => out(hash_int8(int(&a[0]), seed)),
        // an interval hashes as hashint8 of the low 64 bits of its span (a month 30 days), as interval_hash does:
        // '30 days' and '1 month' hash alike, as they are equal
        "interval_hash" => {
            let i = match &a[0] {
                Value::Interval(i) => *i,
                v => super::time::parse_interval(&v.to_text()).unwrap_or_default(),
            };
            out(hash_int8(i.total() as i64, seed))
        }
        // time_hash, timestamp_hash and pg_lsn_hash are hashint8 of the value's int8 (a time's microseconds, a
        // timestamp's since 2000-01-01, an lsn's 64 bits)
        "time_hash" => out(hash_int8(time_of(&a[0])?, seed)),
        "timestamp_hash" => out(hash_int8(timestamp_of(&a[0])?, seed)),
        "pg_lsn_hash" => out(hash_int8(lsn_of(&a[0])? as i64, seed)),
        // timetz_hash: hashint8 of its time XOR hash_uint32 of its zone (seconds west of UTC), each separately to
        // stay clear of the struct's padding
        "timetz_hash" => {
            // (its text as timetz reads it, a zone name too, then its stored form)
            let text = super::coerce(a[0].clone(), super::Type::TimeTz)?.to_text();
            let (us, east) = super::timetz::read(&text)?;
            let (t, z) = (hash_int8(us, seed), hash_u32((-east) as u32, seed));
            out((t.0 ^ z.0, t.1 ^ z.1))
        }
        // uuid_hash: hash_any of its 16 bytes
        "uuid_hash" => {
            let Some(u) = super::parse_uuid(&a[0].to_text()) else {
                return err("22P02", format!("invalid input syntax for type uuid: \"{}\"", a[0].to_text()));
            };
            let hex: String = u.chars().filter(|c| *c != '-').collect();
            let bytes: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0)).collect();
            out(hash_bytes(&bytes, seed))
        }
        // hashinet, hashmacaddr and hashmacaddr8: hash_any of the value's bytes as Postgres stores them (an inet's
        // family, bits and the address's bytes; a macaddr's 6 or 8)
        "hashinet" | "hashmacaddr" | "hashmacaddr8" => {
            use super::net::{Kind, Net};
            let kind = match NAMES[k as usize % NAMES.len()] {
                "hashinet" => Kind::Inet,
                "hashmacaddr" => Kind::Mac,
                _ => Kind::Mac8,
            };
            // (a value of another network type converted to the function's, as its call converts it: a macaddr
            // given hashmacaddr8 is the eight bytes with ff:fe in the middle)
            let n = match &a[0] {
                Value::Net(n) => super::net::convert(n, kind, super::net::Syntax::Call)?,
                v => super::net::parse(kind, &v.to_text())?,
            };
            let bytes = match n {
                Net::Inet(x) | Net::Cidr(x) => {
                    // PGSQL_AF_INET is AF_INET (2), PGSQL_AF_INET6 one more
                    let mut b = vec![if x.v6 { 3 } else { 2 }, x.bits];
                    match x.v6 {
                        true => b.extend(x.ip.to_be_bytes()),
                        false => b.extend((x.ip as u32).to_be_bytes()),
                    }
                    b
                }
                Net::Mac(m) => m.to_be_bytes()[2..].to_vec(),
                Net::Mac8(m) => m.to_be_bytes().to_vec(),
            };
            out(hash_bytes(&bytes, seed))
        }
        // hashoidvector: hash_any of its oids' bytes
        "hashoidvector" => {
            let mut bytes = vec![];
            for o in a[0].to_text().split_whitespace() {
                let o: u32 = o.parse().map_err(|_| SqlError {
                    code: "22P02",
                    message: format!("invalid input syntax for type oid: \"{o}\""),
                    info: None,
                })?;
                bytes.extend(o.to_le_bytes());
            }
            out(hash_bytes(&bytes, seed))
        }
        // hash_numeric: hash_any of its base-10000 digits (no zeros at either end) XOR the first's weight; NaN and
        // the infinities hash to 0 (extended: the seed), zero to -1 (the seed less one)
        "hash_numeric" => {
            let d = match super::coerce(a[0].clone(), super::Type::Numeric)? {
                Value::Numeric(d) => d,
                v => return err("22P02", format!("invalid input syntax for type numeric: \"{}\"", v.to_text())),
            };
            let Some((digits, weight)) = d.groups() else { return out((0, seed)) };
            if digits.is_empty() {
                return out((u32::MAX, seed.wrapping_sub(1)));
            }
            let bytes: Vec<u8> = digits.iter().flat_map(|g| g.to_le_bytes()).collect();
            let h = hash_bytes(&bytes, seed);
            out((h.0 ^ weight as u32, h.1 ^ (i64::from(weight) as u64)))
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

/// A time's microseconds since midnight
fn time_of(v: &Value) -> Result<i64> {
    match super::coerce(v.clone(), super::Type::Time)? {
        Value::Time(us) => Ok(us),
        v => err("22007", format!("invalid input syntax for type time: \"{}\"", v.to_text())),
    }
}

/// A timestamp's microseconds since 2000-01-01, Postgres's int8 of it
fn timestamp_of(v: &Value) -> Result<i64> {
    match super::coerce(v.clone(), super::Type::Timestamp)? {
        // (infinity and -infinity are i64::MAX and i64::MIN here as in Postgres: no epoch to shift them by)
        Value::Timestamp(t) if t == super::time::TS_NEG_INF || t == super::time::TS_INF => Ok(t),
        Value::Timestamp(t) => Ok(t - PG_EPOCH_US),
        v => err("22007", format!("invalid input syntax for type timestamp: \"{}\"", v.to_text())),
    }
}

/// An lsn's 64 bits: `hi/lo` in hexadecimal (or a number)
fn lsn_of(v: &Value) -> Result<u64> {
    if let Value::Int(n) = v {
        return Ok(*n as u64);
    }
    let t = v.to_text();
    let parsed = match t.split_once('/') {
        Some((hi, lo)) => u32::from_str_radix(hi, 16).ok().zip(u32::from_str_radix(lo, 16).ok()),
        None => None,
    };
    match parsed {
        Some((hi, lo)) => Ok(u64::from(hi) << 32 | u64::from(lo)),
        None => t.parse::<u64>().or_else(|_| err("22P02", format!("invalid input syntax for type pg_lsn: \"{t}\""))),
    }
}
