//! xid (32 bits), xid8 (64 bits) and pg_snapshot: Postgres's input rules, snapshot text and what is visible in one.
//! An xid is an integer, an xid8 a numeric (so that it orders and prints as an unsigned number), a snapshot its text.

use super::funcs::Scalar;
use super::{SqlError, Value, decimal, err};

type Result<T> = std::result::Result<T, SqlError>;

fn bad(ty: &str, s: &str) -> SqlError {
    SqlError { code: "22P02", message: format!("invalid input syntax for type {ty}: \"{s}\""), info: None }
}

fn range(ty: &str, s: &str) -> SqlError {
    SqlError { code: "22003", message: format!("value \"{s}\" is out of range for type {ty}"), info: None }
}

/// `strtoul` with base 0 over the whole text: `42`, `010` (octal), `0x1f`, `-1` (wraps around). The magnitude.
fn number(s: &str) -> Option<(bool, u128)> {
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (digits, radix) = match body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        Some(h) => (h, 16),
        None if body.len() > 1 && body.starts_with('0') => (&body[1..], 8),
        None => (body, 10),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    // (past 128 bits it is far out of range: saturate)
    Some((neg, u128::from_str_radix(digits, radix).unwrap_or(u128::MAX)))
}

pub(super) fn parse_xid(s: &str) -> Result<i64> {
    let (neg, m) = number(s).ok_or_else(|| bad("xid", s))?;
    let v: u128 = if neg { (1u128 << 64).wrapping_sub(m) & u128::from(u64::MAX) } else { m };
    if m > u128::from(u64::MAX) || (v > u128::from(u32::MAX) && v < u128::from(u64::MAX - u64::from(u32::MAX))) {
        return Err(range("xid", s));
    }
    Ok(i64::from(v as u32))
}

pub(super) fn parse_xid8(s: &str) -> Result<Value> {
    let (neg, m) = number(s).ok_or_else(|| bad("xid8", s))?;
    if m > u128::from(u64::MAX) {
        return Err(range("xid8", s));
    }
    let v = if neg { u64::MAX - (m as u64) + 1 } else { m as u64 };
    Ok(Value::Numeric(decimal::Dec::parse(&v.to_string()).ok_or_else(|| range("xid8", s))?))
}

pub(super) fn as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Int(n) => u64::try_from(*n).ok(),
        v => v.to_text().parse().ok(),
    }
}

pub(super) struct Snap {
    pub xmin: u64,
    pub xmax: u64,
    pub xip: Vec<u64>,
}

impl Snap {
    pub fn text(&self) -> String {
        let xip: Vec<String> = self.xip.iter().map(u64::to_string).collect();
        format!("{}:{}:{}", self.xmin, self.xmax, xip.join(","))
    }

    pub fn visible(&self, x: u64) -> bool {
        x < self.xmin || (x < self.xmax && !self.xip.contains(&x))
    }
}

pub(super) fn parse_snap(s: &str, ty: &str) -> Result<Snap> {
    let fail = || bad(ty, s);
    let mut it = s.splitn(3, ':');
    let (Some(a), Some(b), Some(c)) = (it.next(), it.next(), it.next()) else { return Err(fail()) };
    let num = |t: &str| -> Result<u64> {
        if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit()) {
            return Err(fail());
        }
        t.parse::<u64>().map_err(|_| fail())
    };
    let (xmin, xmax) = (num(a)?, num(b)?);
    // (xmin and xmax must be valid transaction ids: their low 32 bits, the epoch's xid, not 0)
    if xmin & 0xffff_ffff == 0 || xmax & 0xffff_ffff == 0 || xmin > xmax {
        return Err(fail());
    }
    let mut xip: Vec<u64> = vec![];
    if !c.is_empty() {
        for t in c.split(',') {
            let x = num(t)?;
            if x < xmin || x >= xmax || xip.last().is_some_and(|l| x < *l) {
                return Err(fail());
            }
            if xip.last() != Some(&x) {
                xip.push(x);
            }
        }
    }
    Ok(Snap { xmin, xmax, xip })
}

pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    use Value::{Bool, Int, Null};
    if a.contains(&Null) {
        return Ok(Null);
    }
    let Scalar::XidFn(k) = f else { return err("XX000", "not an xid function") };
    let snap = |v: &Value| parse_snap(&v.to_text(), "pg_snapshot");
    Ok(match k {
        0 => {
            let (x, y) = (as_u64(&a[0]).unwrap_or(0), as_u64(&a[1]).unwrap_or(0));
            Int(match x.cmp(&y) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })
        }
        1 | 2 => {
            let s = snap(&a[0])?;
            let v = if k == 1 { s.xmin } else { s.xmax };
            Value::Numeric(decimal::Dec::parse(&v.to_string()).ok_or_else(|| range("xid8", ""))?)
        }
        3 => Bool(snap(&a[1])?.visible(as_u64(&a[0]).unwrap_or(0))),
        4 | 5 => {
            let s = snap(&a[0])?;
            Int(if k == 4 { s.xmin } else { s.xmax } as i64)
        }
        _ => Bool(snap(&a[1])?.visible(as_u64(&a[0]).unwrap_or(0))),
    })
}

/// pg_snapshot_xip(snap): the in-progress ones, a row each.
pub(super) fn xip_rows(v: &Value, ty: &str, numeric: bool) -> Result<Vec<Vec<Value>>> {
    if *v == Value::Null {
        return Ok(vec![]);
    }
    let s = parse_snap(&v.to_text(), ty)?;
    Ok(s.xip
        .into_iter()
        .map(|x| {
            vec![if numeric {
                decimal::Dec::parse(&x.to_string()).map_or(Value::Null, Value::Numeric)
            } else {
                Value::Int(x as i64)
            }]
        })
        .collect())
}
