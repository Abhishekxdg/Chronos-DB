//! Postgres's `numeric` as a path sees it, exactly: a sign, a string of decimal digits, a power of
//! ten and a display scale. Nothing is rounded to a float between one step of a path and the
//! next, so `1 / 3 + 2 / 3`, `99999999999999999999 - 99999999999999999998` and `1e400 > 1` answer
//! as they do in Postgres.
//!
//! The limits are Postgres's own: under 10^131072, at most 16383 digits after the point. The
//! results of `+ - * / %`, the display scale (`1.50 + 0` is `1.50`, `0.5 * 4` is `2.0`, `10 / 4`
//! is `2.5000000000000000`) and the rounding of a quotient follow numeric.c.

use std::cmp::Ordering;
use std::fmt;

/// Why a number could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumError {
    /// more than 131072 digits before the point or 16383 after it (22003)
    Overflow,
    /// a division or remainder by zero (22012)
    DivZero,
    /// not a number at all (22P02)
    Syntax,
}

impl NumError {
    pub fn parts(self) -> (&'static str, &'static str) {
        match self {
            NumError::Overflow => ("22003", "value overflows numeric format"),
            NumError::DivZero => ("22012", "division by zero"),
            NumError::Syntax => ("22P02", "invalid input syntax for type numeric"),
        }
    }

    /// As a path reports it.
    pub fn err(self) -> (&'static str, String) {
        let (code, msg) = self.parts();
        (code, msg.to_string())
    }
}

/// Digits before the point, at most.
const MAX_DIGITS: i64 = 131_072;
/// Digits after the point, at most.
const MAX_DSCALE: i64 = 16383;
/// Postgres's limit on the scale of a quotient.
const MAX_DIV_SCALE: i64 = 1000;

/// An exact decimal: `mant` (most significant digit first, no leading and no trailing zero; empty
/// for zero) times 10^`pow`, and the `dscale` it is shown with.
#[derive(Clone, Debug)]
pub struct Numeric {
    neg: bool,
    mant: Vec<u8>,
    pow: i64,
    dscale: u32,
}

// ---------------------------------------------------------------------------------------------
// magnitudes: strings of digits

fn strip_leading(v: &mut Vec<u8>) {
    let zeros = v.iter().take_while(|&&d| d == 0).count();
    if zeros > 0 {
        v.drain(..zeros);
    }
}

fn cmp_mag(a: &[u8], b: &[u8]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn add_mag(a: &[u8], b: &[u8]) -> Vec<u8> {
    let n = a.len().max(b.len());
    let mut out = vec![0u8; n + 1];
    let mut carry = 0u8;
    for i in 0..n {
        let x = if i < a.len() { a[a.len() - 1 - i] } else { 0 };
        let y = if i < b.len() { b[b.len() - 1 - i] } else { 0 };
        let s = x + y + carry;
        out[n - i] = s % 10;
        carry = s / 10;
    }
    out[0] = carry;
    strip_leading(&mut out);
    out
}

/// `a` - `b`, for `a` >= `b`, in place.
fn sub_assign_mag(a: &mut Vec<u8>, b: &[u8]) {
    let mut borrow = 0i8;
    let n = a.len();
    for i in 0..n {
        let y = if i < b.len() { b[b.len() - 1 - i] as i8 } else { 0 };
        let mut d = a[n - 1 - i] as i8 - y - borrow;
        if d < 0 {
            d += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        a[n - 1 - i] = d as u8;
    }
    strip_leading(a);
}

fn mul_mag(a: &[u8], b: &[u8]) -> Vec<u8> {
    if a.is_empty() || b.is_empty() {
        return vec![];
    }
    let mut acc = vec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().rev().enumerate() {
        if x == 0 {
            continue;
        }
        for (j, &y) in b.iter().rev().enumerate() {
            acc[i + j] += u32::from(x) * u32::from(y);
        }
        // keep the accumulators from overflowing on long operands
        if i % 1_000_000 == 999_999 {
            let mut carry = 0;
            for c in acc.iter_mut() {
                let v = *c + carry;
                *c = v % 10;
                carry = v / 10;
            }
        }
    }
    let mut out = vec![0u8; acc.len()];
    let mut carry = 0u32;
    for (i, c) in acc.iter().enumerate() {
        let v = c + carry;
        out[acc.len() - 1 - i] = (v % 10) as u8;
        carry = v / 10;
    }
    strip_leading(&mut out);
    out
}

/// The quotient and the remainder of `n` by `d` (not empty), by long division.
fn divrem_mag(n: &[u8], d: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if cmp_mag(n, d) == Ordering::Less {
        return (vec![], n.to_vec());
    }
    if d.len() == 1 {
        let d = u32::from(d[0]);
        let (mut q, mut r) = (Vec::with_capacity(n.len()), 0u32);
        for &x in n {
            r = r * 10 + u32::from(x);
            q.push((r / d) as u8);
            r %= d;
        }
        strip_leading(&mut q);
        return (q, if r == 0 { vec![] } else { vec![r as u8] });
    }
    let mut q = Vec::with_capacity(n.len());
    let mut r: Vec<u8> = Vec::with_capacity(d.len() + 1);
    for &x in n {
        r.push(x);
        strip_leading(&mut r);
        let mut c = 0u8;
        while cmp_mag(&r, d) != Ordering::Less {
            sub_assign_mag(&mut r, d);
            c += 1;
        }
        q.push(c);
    }
    strip_leading(&mut q);
    (q, r)
}

fn shifted(m: &[u8], zeros: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(m.len() + zeros);
    v.extend_from_slice(m);
    v.resize(m.len() + zeros, 0);
    v
}

// ---------------------------------------------------------------------------------------------

impl Numeric {
    pub fn zero() -> Numeric {
        Numeric { neg: false, mant: vec![], pow: 0, dscale: 0 }
    }

    /// A number from its parts, normalised; `dscale` is raised to what the digits need. Not checked
    /// against the limit on the scale (a product may be rounded to it).
    fn raw(neg: bool, mut mant: Vec<u8>, mut pow: i64, dscale: i64) -> Result<Numeric, NumError> {
        strip_leading(&mut mant);
        if mant.is_empty() {
            return Ok(Numeric { neg: false, mant, pow: 0, dscale: dscale.clamp(0, i64::from(u32::MAX)) as u32 });
        }
        let trailing = mant.iter().rev().take_while(|&&d| d == 0).count();
        mant.truncate(mant.len() - trailing);
        pow += trailing as i64;
        if pow + mant.len() as i64 > MAX_DIGITS {
            return Err(NumError::Overflow);
        }
        let dscale = dscale.max(-pow).clamp(0, i64::from(u32::MAX));
        Ok(Numeric { neg, mant, pow, dscale: dscale as u32 })
    }

    /// A number from its parts, normalised, and within Postgres's limits.
    fn make(neg: bool, mant: Vec<u8>, pow: i64, dscale: i64) -> Result<Numeric, NumError> {
        let n = Numeric::raw(neg, mant, pow, dscale)?;
        if i64::from(n.dscale) > MAX_DSCALE { Err(NumError::Overflow) } else { Ok(n) }
    }

    pub fn from_i64(i: i64) -> Numeric {
        Numeric::from_u128(i.unsigned_abs().into(), i < 0)
    }

    pub fn from_u64(u: u64) -> Numeric {
        Numeric::from_u128(u.into(), false)
    }

    pub fn from_usize(u: usize) -> Numeric {
        Numeric::from_u128(u as u128, false)
    }

    fn from_u128(u: u128, neg: bool) -> Numeric {
        let mant: Vec<u8> = u.to_string().bytes().map(|b| b - b'0').collect();
        Numeric::make(neg, mant, 0, 0).unwrap_or_else(|_| Numeric::zero())
    }

    /// `[ws][sign]digits[.digits][e[sign]digits][ws]`, as numeric_in reads a decimal.
    pub fn parse(s: &str) -> Result<Numeric, NumError> {
        let s = s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'));
        let (neg, s) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let (mant_text, exp_text) = match s.find(['e', 'E']) {
            Some(k) => (&s[..k], Some(&s[k + 1..])),
            None => (s, None),
        };
        let (int, frac) = mant_text.split_once('.').unwrap_or((mant_text, ""));
        if int.len() + frac.len() == 0 || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
            return Err(NumError::Syntax);
        }
        let exp: i64 = match exp_text {
            None => 0,
            Some(t) => {
                let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(NumError::Syntax);
                }
                let v = digits.trim_start_matches('0');
                let v: i64 = if v.is_empty() { 0 } else { v.parse().unwrap_or(i64::MAX) };
                if v >= i64::from(i32::MAX / 2) {
                    return Err(NumError::Syntax);
                }
                if t.starts_with('-') { -v } else { v }
            }
        };
        if int.len() + frac.len() > 400_000 {
            return Err(NumError::Overflow);
        }
        let mant: Vec<u8> = int.bytes().chain(frac.bytes()).map(|b| b - b'0').collect();
        let dscale = (frac.len() as i64 - exp).max(0);
        Numeric::make(neg, mant, exp - frac.len() as i64, dscale)
    }

    /// The integer the digits spell in base `radix` (2, 8 or 16).
    pub fn from_radix(digits: &str, radix: u32) -> Result<Numeric, NumError> {
        const LIMB: u64 = 1_000_000_000;
        let values: Vec<u32> =
            digits.chars().map(|c| c.to_digit(radix).ok_or(NumError::Syntax)).collect::<Result<_, _>>()?;
        // the value in limbs of nine decimal digits, least first: several digits go in at a time
        let per = (32.0 / f64::from(radix).log2()) as usize;
        let mut limbs: Vec<u32> = vec![];
        for chunk in values.chunks(per.max(1)) {
            let (mut mult, mut add) = (1u64, 0u64);
            for &d in chunk {
                mult *= u64::from(radix);
                add = add * u64::from(radix) + u64::from(d);
            }
            let mut carry = add;
            for l in limbs.iter_mut() {
                let v = u64::from(*l) * mult + carry;
                *l = (v % LIMB) as u32;
                carry = v / LIMB;
            }
            while carry > 0 {
                limbs.push((carry % LIMB) as u32);
                carry /= LIMB;
            }
            if limbs.len() as i64 * 9 > MAX_DIGITS + 9 {
                return Err(NumError::Overflow);
            }
        }
        let mut dec: Vec<u8> = vec![];
        for (k, l) in limbs.iter().rev().enumerate() {
            let text = if k == 0 { l.to_string() } else { format!("{l:09}") };
            dec.extend(text.bytes().map(|b| b - b'0'));
        }
        Numeric::make(false, dec, 0, 0)
    }

    /// The shortest decimal that reads back as `f` (what a JSON writer prints); None for NaN and
    /// the infinities.
    pub fn from_f64_shortest(f: f64) -> Option<Numeric> {
        if !f.is_finite() {
            return None;
        }
        Numeric::parse(&format!("{f:e}")).ok()
    }

    /// `float8` as a numeric, as float8_numeric makes it: 15 significant digits (`%.15g`).
    pub fn from_f64_15(f: f64) -> Option<Numeric> {
        if !f.is_finite() {
            return None;
        }
        let text = format!("{f:.14e}");
        let (m, e) = text.split_once('e')?;
        let e: i64 = e.parse().ok()?;
        let neg = m.starts_with('-');
        let mant: Vec<u8> = m.bytes().filter(u8::is_ascii_digit).map(|b| b - b'0').collect();
        Numeric::make(neg, mant, e - 14, 0).ok()
    }

    /// The nearest float8 (infinite past the range, zero below it).
    pub fn to_f64(&self) -> f64 {
        self.to_string().parse().unwrap_or(f64::NAN)
    }

    pub fn is_zero(&self) -> bool {
        self.mant.is_empty()
    }

    pub fn sign(&self) -> i32 {
        if self.mant.is_empty() {
            0
        } else if self.neg {
            -1
        } else {
            1
        }
    }

    /// Whether the value has no fraction.
    pub fn is_integer(&self) -> bool {
        self.pow >= 0 || self.mant.is_empty()
    }

    pub fn dscale(&self) -> u32 {
        self.dscale
    }

    /// The value as an i64, if it is an integer that fits.
    pub fn to_i64(&self) -> Option<i64> {
        if !self.is_integer() {
            return None;
        }
        if self.mant.is_empty() {
            return Some(0);
        }
        if self.pow + self.mant.len() as i64 > 19 {
            return None;
        }
        let mut v: i128 = 0;
        for &d in &self.mant {
            v = v * 10 + i128::from(d);
        }
        v *= 10i128.pow(self.pow as u32);
        i64::try_from(if self.neg { -v } else { v }).ok()
    }

    /// Its whole part, if that fits an i32 (what a subscript is: `numeric_trunc` then int4).
    pub fn trunc_i32(&self) -> Option<i32> {
        let t = self.trunc();
        t.to_i64().and_then(|i| i32::try_from(i).ok())
    }

    pub fn neg(&self) -> Numeric {
        let mut n = self.clone();
        n.neg = !n.neg && !n.mant.is_empty();
        n
    }

    pub fn abs(&self) -> Numeric {
        let mut n = self.clone();
        n.neg = false;
        n
    }

    /// Numerically, scale aside.
    pub fn cmp(&self, o: &Numeric) -> Ordering {
        let (sa, sb) = (self.sign(), o.sign());
        if sa != sb || sa == 0 {
            return sa.cmp(&sb);
        }
        let lead = |n: &Numeric| n.pow + n.mant.len() as i64;
        let mag = lead(self).cmp(&lead(o)).then_with(|| {
            // the same number of digits before the point: the digits, then what is left of the longer
            let k = self.mant.len().min(o.mant.len());
            self.mant[..k].cmp(&o.mant[..k]).then(self.mant.len().cmp(&o.mant.len()))
        });
        if sa < 0 { mag.reverse() } else { mag }
    }

    /// The whole digits of the magnitude at 10^`pow` (`pow` no more than the number's).
    fn at(&self, pow: i64) -> Vec<u8> {
        shifted(&self.mant, (self.pow - pow) as usize)
    }

    pub fn add(&self, o: &Numeric) -> Result<Numeric, NumError> {
        let dscale = i64::from(self.dscale.max(o.dscale));
        if self.is_zero() {
            return Numeric::make(o.neg, o.mant.clone(), o.pow, dscale);
        }
        if o.is_zero() {
            return Numeric::make(self.neg, self.mant.clone(), self.pow, dscale);
        }
        let pow = self.pow.min(o.pow);
        let (a, b) = (self.at(pow), o.at(pow));
        if self.neg == o.neg {
            return Numeric::make(self.neg, add_mag(&a, &b), pow, dscale);
        }
        match cmp_mag(&a, &b) {
            Ordering::Equal => Numeric::make(false, vec![], 0, dscale),
            Ordering::Greater => {
                let mut r = a;
                sub_assign_mag(&mut r, &b);
                Numeric::make(self.neg, r, pow, dscale)
            }
            Ordering::Less => {
                let mut r = b;
                sub_assign_mag(&mut r, &a);
                Numeric::make(o.neg, r, pow, dscale)
            }
        }
    }

    pub fn sub(&self, o: &Numeric) -> Result<Numeric, NumError> {
        self.add(&o.neg())
    }

    pub fn mul(&self, o: &Numeric) -> Result<Numeric, NumError> {
        let dscale = i64::from(self.dscale) + i64::from(o.dscale);
        if self.is_zero() || o.is_zero() {
            return Numeric::make(false, vec![], 0, dscale.min(MAX_DSCALE));
        }
        if self.pow + self.mant.len() as i64 + o.pow + o.mant.len() as i64 > MAX_DIGITS + 2 {
            return Err(NumError::Overflow);
        }
        let n = Numeric::raw(self.neg != o.neg, mul_mag(&self.mant, &o.mant), self.pow + o.pow, dscale)?;
        if dscale > MAX_DSCALE { n.round(MAX_DSCALE) } else { Ok(n) }
    }

    /// Rounded half away from zero to `scale` digits after the point, shown with that many.
    fn round(&self, scale: i64) -> Result<Numeric, NumError> {
        let drop = -self.pow - scale;
        if self.mant.is_empty() || drop <= 0 {
            return Numeric::make(self.neg, self.mant.clone(), self.pow, scale);
        }
        let len = self.mant.len() as i64;
        if drop > len {
            return Numeric::make(false, vec![], 0, scale);
        }
        let keep = (len - drop) as usize;
        let up = self.mant[keep] >= 5;
        let mut m = self.mant[..keep].to_vec();
        if up {
            m = add_mag(&m, &[1]);
        }
        Numeric::make(self.neg, m, -scale, scale)
    }

    /// The position of the first digit, in Postgres's base 10000: its weight, and its value.
    fn lead(&self) -> (i64, u32) {
        if self.mant.is_empty() {
            return (0, 0);
        }
        let top = self.pow + self.mant.len() as i64 - 1;
        let w = top.div_euclid(4);
        let take = (top - 4 * w + 1) as usize;
        let mut v = 0u32;
        for i in 0..take {
            v = v * 10 + u32::from(self.mant.get(i).copied().unwrap_or(0));
        }
        (w, v)
    }

    /// The scale a division is carried to (select_div_scale): 16 significant digits at least.
    fn div_scale(&self, o: &Numeric) -> i64 {
        let (w1, f1) = self.lead();
        let (w2, f2) = o.lead();
        let mut qweight = w1 - w2;
        if f1 <= f2 {
            qweight -= 1;
        }
        (16 - qweight * 4).max(i64::from(self.dscale)).max(i64::from(o.dscale)).clamp(0, MAX_DIV_SCALE)
    }

    pub fn div(&self, o: &Numeric) -> Result<Numeric, NumError> {
        if o.is_zero() {
            return Err(NumError::DivZero);
        }
        let rscale = self.div_scale(o);
        if self.is_zero() {
            return Numeric::make(false, vec![], 0, rscale);
        }
        // the quotient in units of 10^-rscale: self / o x 10^rscale
        let k = self.pow - o.pow + rscale;
        if self.mant.len() as i64 + k - o.mant.len() as i64 > MAX_DIGITS + MAX_DSCALE + 2 {
            return Err(NumError::Overflow);
        }
        let (n, d) = if k >= 0 {
            (shifted(&self.mant, k as usize), o.mant.clone())
        } else {
            (self.mant.clone(), shifted(&o.mant, (-k) as usize))
        };
        let (mut q, r) = divrem_mag(&n, &d);
        // rounded half away from zero: the next digit is 5 or more when twice the remainder is the divisor's
        if !r.is_empty() && cmp_mag(&add_mag(&r, &r), &d) != Ordering::Less {
            q = add_mag(&q, &[1]);
        }
        Numeric::make(self.neg != o.neg, q, -rscale, rscale)
    }

    /// The remainder, with the sign of `self` (`%`).
    pub fn rem(&self, o: &Numeric) -> Result<Numeric, NumError> {
        if o.is_zero() {
            return Err(NumError::DivZero);
        }
        let dscale = i64::from(self.dscale.max(o.dscale));
        if self.is_zero() {
            return Numeric::make(false, vec![], 0, dscale);
        }
        let pow = self.pow.min(o.pow);
        let (_, r) = divrem_mag(&self.at(pow), &o.at(pow));
        Numeric::make(self.neg, r, pow, dscale)
    }

    /// Toward zero, shown with no digits after the point.
    pub fn trunc(&self) -> Numeric {
        if self.pow >= 0 || self.mant.is_empty() {
            return Numeric { dscale: 0, ..self.clone() };
        }
        let keep = self.mant.len() as i64 + self.pow;
        if keep <= 0 {
            return Numeric::zero();
        }
        Numeric::make(self.neg, self.mant[..keep as usize].to_vec(), 0, 0).unwrap_or_else(|_| Numeric::zero())
    }

    pub fn floor(&self) -> Numeric {
        let t = self.trunc();
        if self.neg && t.cmp(self) != Ordering::Equal { t.sub(&Numeric::from_i64(1)).unwrap_or(t) } else { t }
    }

    pub fn ceil(&self) -> Numeric {
        let t = self.trunc();
        if !self.neg && !self.mant.is_empty() && t.cmp(self) != Ordering::Equal {
            t.add(&Numeric::from_i64(1)).unwrap_or(t)
        } else {
            let mut t = t;
            if t.mant.is_empty() {
                t.neg = false;
            }
            t
        }
    }

    /// How many bytes jsonb stores this number in: a numeric datum, 4 of header, then 2 or 4 more,
    /// then 2 for each base-10000 digit.
    pub fn stored_len(&self) -> usize {
        if self.mant.is_empty() {
            return if self.dscale <= 63 { 6 } else { 8 };
        }
        let weight = (self.pow + self.mant.len() as i64 - 1).div_euclid(4);
        let groups = weight - self.pow.div_euclid(4) + 1;
        let short = self.dscale <= 63 && (-64..=63).contains(&weight);
        4 + if short { 2 } else { 4 } + 2 * groups as usize
    }
}

impl PartialEq for Numeric {
    fn eq(&self, o: &Numeric) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}

impl fmt::Display for Numeric {
    /// As numeric_out writes it: no exponent, `dscale` digits after the point.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = i64::from(self.dscale);
        let mut all: Vec<u8> = self.mant.iter().map(|d| b'0' + d).collect();
        all.resize(all.len() + (self.pow + scale).max(0) as usize, b'0');
        if all.len() as i64 <= scale {
            let pad = (scale + 1) as usize - all.len();
            all.splice(0..0, std::iter::repeat_n(b'0', pad));
        }
        let split = all.len() - scale as usize;
        let mut out = String::with_capacity(all.len() + 2);
        if self.neg {
            out.push('-');
        }
        out.push_str(std::str::from_utf8(&all[..split]).unwrap_or("0"));
        if scale > 0 {
            out.push('.');
            out.push_str(std::str::from_utf8(&all[split..]).unwrap_or(""));
        }
        f.write_str(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads `JP_NUM_CASES` (lines `op<TAB>a<TAB>b`) and writes what this code answers to
    /// `JP_NUM_OUT`: for differential runs against a live Postgres.
    #[test]
    #[ignore = "a differential run against a live Postgres, fed by JP_NUM_CASES"]
    fn differential() {
        let (Ok(cases), Ok(out)) = (std::env::var("JP_NUM_CASES"), std::env::var("JP_NUM_OUT")) else { return };
        let mut res = String::new();
        for line in std::fs::read_to_string(cases).unwrap().lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let p = |s: &str| Numeric::parse(s);
            let r = (|| -> Result<String, NumError> {
                if let Some(radix) = match f[0] {
                    "hex" => Some(16),
                    "oct" => Some(8),
                    "bin" => Some(2),
                    _ => None,
                } {
                    return Ok(Numeric::from_radix(f[1], radix)?.to_string());
                }
                let a = p(f[1])?;
                Ok(match f[0] {
                    "id" => a.to_string(),
                    "neg" => a.neg().to_string(),
                    "abs" => a.abs().to_string(),
                    "floor" => a.floor().to_string(),
                    "ceil" => a.ceil().to_string(),
                    "trunc" => a.trunc().to_string(),
                    "len" => a.stored_len().to_string(),
                    "f15" => match Numeric::from_f64_15(a.to_f64()) {
                        Some(n) => n.to_string(),
                        None => return Err(NumError::Overflow),
                    },
                    "cmp" => match a.cmp(&p(f[2])?) {
                        Ordering::Less => "-1",
                        Ordering::Equal => "0",
                        Ordering::Greater => "1",
                    }
                    .into(),
                    op => {
                        let b = p(f[2])?;
                        match op {
                            "add" => a.add(&b)?,
                            "sub" => a.sub(&b)?,
                            "mul" => a.mul(&b)?,
                            "div" => a.div(&b)?,
                            "rem" => a.rem(&b)?,
                            _ => panic!("{op}"),
                        }
                        .to_string()
                    }
                })
            })();
            match r {
                Ok(s) => res.push_str(&s),
                Err(e) => res.push_str(&format!("ERR {}", e.parts().0)),
            }
            res.push('\n');
        }
        std::fs::write(out, res).unwrap();
    }
}
