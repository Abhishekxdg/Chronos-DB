//! Bit strings (`bit(n)`, `bit varying(n)`): values are their text of 0s and 1s, `101`. Postgres's input, casts, the
//! bitwise operators and the functions over them.

use super::funcs::Scalar;
use super::{SqlError, Type, Value, err};

type Result<T> = std::result::Result<T, SqlError>;

fn bad_digit(c: char) -> SqlError {
    SqlError { code: "22P02", message: format!("\"{c}\" is not a valid binary digit"), info: None }
}

/// The bits of `B'101'` / `X'1F'` text as input reads them: `101`, or `X1F` (hex, four bits a digit).
pub(super) fn parse(s: &str) -> Result<String> {
    if let Some(h) = s.strip_prefix(['x', 'X']) {
        let mut out = String::new();
        for c in h.chars() {
            let d = c.to_digit(16).ok_or_else(|| SqlError {
                code: "22P02",
                message: format!("\"{c}\" is not a valid hexadecimal digit"),
                info: None,
            })?;
            out.push_str(&format!("{d:04b}"));
        }
        return Ok(out);
    }
    let b = s.strip_prefix(['b', 'B']).unwrap_or(s);
    match b.chars().find(|c| !matches!(c, '0' | '1')) {
        Some(c) => Err(bad_digit(c)),
        None => Ok(b.to_string()),
    }
}

/// A value assigned to a bit(n) or bit varying(n) column: bit(n) takes only n bits, varying at most n.
pub(super) fn assign(ty: Type, x: &str, n: u32, fixed: bool) -> Result<String> {
    let len = x.chars().count();
    match (ty, fixed) {
        (Type::Bit, _) | (_, true) if len != n as usize => Err(SqlError {
            code: "22026",
            message: format!("bit string length {len} does not match type bit({n})"),
            info: None,
        }),
        (Type::VarBit, _) if len > n as usize => Err(SqlError {
            code: "22001",
            message: format!("bit string too long for type bit varying({n})"),
            info: None,
        }),
        _ => Ok(x.to_string()),
    }
}

/// `x::bit(n)`, `x::bit varying(n)`: cut to n bits, a bit(n) padded with zeros on the right. An integer gives its
/// lowest n bits.
pub(super) fn fit(v: &Value, n: usize, fixed: bool) -> Result<String> {
    if let Value::Int(i) = v {
        let all = format!("{:032b}", *i as i32);
        let all = if *i > i64::from(i32::MAX) || *i < i64::from(i32::MIN) { format!("{:064b}", *i) } else { all };
        return Ok(if all.len() >= n { all[all.len() - n..].to_string() } else { format!("{all:0>n$}") });
    }
    let x = parse(&v.to_text())?;
    let mut out: String = x.chars().take(n).collect();
    if fixed {
        while out.chars().count() < n {
            out.push('0');
        }
    }
    Ok(out)
}

pub(super) fn to_int(x: &str, bits: u32) -> Result<i64> {
    let len = x.len();
    let take = len.min(bits as usize);
    let tail = &x[len - take..];
    let v = if tail.is_empty() { 0 } else { u64::from_str_radix(tail, 2).unwrap_or(0) };
    // (a bit string longer than the integer loses its high bits; a shorter one is not sign-extended)
    Ok(match bits {
        32 => v as u32 as i32 as i64,
        64 => v as i64,
        _ => v as i64,
    })
}

fn same_size(a: &str, b: &str) -> Result<()> {
    if a.len() != b.len() {
        return err("22026", "cannot AND bit strings of different sizes");
    }
    Ok(())
}

pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    use Value::{Bool, Int, Null, Text};
    if a.contains(&Null) {
        return Ok(Null);
    }
    match f {
        Scalar::BitOp(k) => {
            let x = a[0].to_text();
            let out: String = match k {
                3 => x.chars().map(|c| if c == '0' { '1' } else { '0' }).collect(),
                4 | 5 => {
                    let n = match &a[1] {
                        Int(n) => *n,
                        v => v.to_text().parse().unwrap_or(0),
                    };
                    let len = x.chars().count() as i64;
                    let n = if k == 5 { -n } else { n };
                    // (left by n; a negative n goes right)
                    if n.abs() >= len {
                        "0".repeat(len as usize)
                    } else if n >= 0 {
                        format!("{}{}", &x[n as usize..], "0".repeat(n as usize))
                    } else {
                        format!("{}{}", "0".repeat((-n) as usize), &x[..(len + n) as usize])
                    }
                }
                _ => {
                    let y = a[1].to_text();
                    if x.len() != y.len() {
                        let what = ["AND", "OR", "XOR"][k as usize % 3];
                        return err("22026", format!("cannot {what} bit strings of different sizes"));
                    }
                    same_size(&x, &y)?;
                    x.chars()
                        .zip(y.chars())
                        .map(|(p, q)| match k {
                            0 => {
                                if p == '1' && q == '1' {
                                    '1'
                                } else {
                                    '0'
                                }
                            }
                            1 => {
                                if p == '1' || q == '1' {
                                    '1'
                                } else {
                                    '0'
                                }
                            }
                            _ => {
                                if p != q {
                                    '1'
                                } else {
                                    '0'
                                }
                            }
                        })
                        .collect()
                }
            };
            Ok(Text(out))
        }
        Scalar::BitFit => {
            let n = match &a[1] {
                Int(n) => *n as usize,
                _ => 0,
            };
            let fixed = matches!(a[2], Bool(true));
            Ok(Text(fit(&a[0], n, fixed)?))
        }
        Scalar::BitToInt => {
            let bits = match &a[1] {
                Int(n) => *n as u32,
                _ => 32,
            };
            let x = a[0].to_text();
            let v = to_int(&x, bits)?;
            if bits == 16 && !(i64::from(i16::MIN)..=i64::from(i16::MAX)).contains(&v) {
                return Ok(Int(v as i16 as i64));
            }
            Ok(Int(v))
        }
        // get_bit and set_bit of a bytea: bit n is bit n % 8 of byte n / 8, counted from the low end
        Scalar::BitFn(k @ (0 | 1)) if matches!(a[0], Value::Bytea(_)) => {
            let Value::Bytea(mut b) = a[0].clone() else { unreachable!("matched a bytea") };
            let Int(n) = a[1] else { return err("22023", "bit index must be an integer") };
            let bits = b.len() as i64 * 8;
            if n < 0 || n >= bits {
                return err("2202E", format!("index {n} out of valid range, 0..{}", bits - 1));
            }
            let (byte, bit) = ((n / 8) as usize, (n % 8) as u32);
            if k == 0 {
                return Ok(Int(i64::from((b[byte] >> bit) & 1)));
            }
            let Int(v) = a[2] else { return err("22023", "new bit must be 0 or 1") };
            if !(0..=1).contains(&v) {
                return err("22023", "new bit must be 0 or 1");
            }
            b[byte] = (b[byte] & !(1u8 << bit)) | ((v as u8) << bit);
            Ok(Value::Bytea(b))
        }
        // bit_count of a bytea: the 1 bits of its bytes
        Scalar::BitFn(2) if matches!(a[0], Value::Bytea(_)) => {
            let Value::Bytea(b) = &a[0] else { unreachable!("matched a bytea") };
            Ok(Int(b.iter().map(|x| i64::from(x.count_ones())).sum()))
        }
        Scalar::BitFn(k) => {
            let x = a[0].to_text();
            match k {
                // get_bit(b, n): the nth bit, from 0
                0 => {
                    let Int(n) = a[1] else { return err("22023", "bit index must be an integer") };
                    if n < 0 || n as usize >= x.len() {
                        return err("2202E", format!("bit index {n} out of valid range (0..{})", x.len() as i64 - 1));
                    }
                    Ok(Int(i64::from(x.as_bytes()[n as usize] == b'1')))
                }
                // set_bit(b, n, v)
                1 => {
                    let (Int(n), Int(v)) = (&a[1], &a[2]) else { return err("22023", "set_bit takes integers") };
                    if *n < 0 || *n as usize >= x.len() {
                        return err("2202E", format!("bit index {n} out of valid range (0..{})", x.len() as i64 - 1));
                    }
                    if !(0..=1).contains(v) {
                        return err("22023", "new bit must be 0 or 1");
                    }
                    let mut bytes = x.into_bytes();
                    bytes[*n as usize] = if *v == 1 { b'1' } else { b'0' };
                    Ok(Text(String::from_utf8(bytes).unwrap_or_default()))
                }
                // bit_count
                2 => Ok(Int(x.bytes().filter(|b| *b == b'1').count() as i64)),
                // bit_length
                3 => Ok(Int(x.len() as i64)),
                // position(sub in b): Postgres's bitposition, 0 when b is empty or sub is longer
                5 => {
                    let sub = a[1].to_text();
                    if x.is_empty() || sub.len() > x.len() {
                        return Ok(Int(0));
                    }
                    Ok(Int(x.find(&sub).map_or(0, |b| b as i64 + 1)))
                }
                // octet_length
                _ => Ok(Int(x.len().div_ceil(8) as i64)),
            }
        }
        _ => err("XX000", "not a bit function"),
    }
}
