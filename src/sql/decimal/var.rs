//! Postgres's NumericVar and the arithmetic numeric.c does on it, ported step for step so results
//! come out digit for digit as Postgres's: base-10000 digits (most significant first), the weight
//! of the first (it is times 10000^weight), a sign and a display scale (decimal digits shown after
//! the point). `numeric` values past what `Dec`'s 128-bit form holds live here, and every
//! function whose result scale or rounding Postgres defines by these steps (division,
//! multiplication to a scale, sqrt, exp, ln, log, power) runs here whatever its arguments' size.
//!
//! Each function returns its result rather than writing into an argument. Where numeric.c relies on
//! wrapping int arithmetic (div_var_fast) the wrapping is kept; elsewhere wider integers stand in
//! for its normalisation passes, which move carries without changing the value.

use super::R;
use std::cmp::Ordering;

pub(super) const NBASE: i32 = 10000;
const HALF_NBASE: i32 = 5000;
pub(super) const DEC_DIGITS: i32 = 4;
const MUL_GUARD_DIGITS: i32 = 2;
const DIV_GUARD_DIGITS: i32 = 4;
/// The largest weight a stored numeric has (NUMERIC_WEIGHT_MAX): under 10^131072.
pub(super) const WEIGHT_MAX: i32 = i16::MAX as i32;
/// The most digits after the point a stored numeric shows (NUMERIC_DSCALE_MAX).
pub(super) const DSCALE_MAX: i32 = 0x3FFF;
/// NUMERIC_MAX_DISPLAY_SCALE: the most places a computed result is given.
pub(super) const MAX_DISPLAY_SCALE: i32 = 1000;
/// NUMERIC_MAX_RESULT_SCALE.
pub(super) const MAX_RESULT_SCALE: i32 = 2000;
/// NUMERIC_MIN_SIG_DIGITS: a result is given at least this many significant digits.
pub(super) const MIN_SIG_DIGITS: i32 = 16;
const ROUND_POWERS: [i32; 4] = [0, 1000, 100, 10];

pub(super) const OVERFLOW: &str = "value overflows numeric format";
pub(super) const DIV_ZERO: &str = "division by zero";
pub(super) const SQRT_NEG: &str = "cannot take square root of a negative number";
pub(super) const LOG_ZERO: &str = "cannot take logarithm of zero";
pub(super) const LOG_NEG: &str = "cannot take logarithm of a negative number";
pub(super) const NEG_POWER: &str = "a negative number raised to a non-integer power yields a complex result";

/// A finite numeric in numeric.c's working form. Zero has no digits; `strip` takes zero digits off
/// both ends (as strip_var), but intermediate results may keep them, as Postgres's do.
#[derive(Clone, Debug, Default)]
pub(crate) struct Var {
    pub(super) weight: i32,
    pub(super) neg: bool,
    pub(super) dscale: i32,
    pub(super) digits: Vec<i32>,
}

impl Var {
    fn n(&self) -> i32 {
        self.digits.len() as i32
    }

    fn dig(&self, i: i32) -> i32 {
        self.digits[i as usize]
    }

    /// Zero, shown with `dscale` places.
    pub(super) fn zero(dscale: i32) -> Var {
        Var { weight: 0, neg: false, dscale, digits: Vec::new() }
    }

    pub(super) fn one() -> Var {
        Var { weight: 0, neg: false, dscale: 0, digits: vec![1] }
    }

    fn minus_one() -> Var {
        Var { weight: 0, neg: true, dscale: 0, digits: vec![1] }
    }

    fn two() -> Var {
        Var { weight: 0, neg: false, dscale: 0, digits: vec![2] }
    }

    fn zero_point_nine() -> Var {
        Var { weight: -1, neg: false, dscale: 1, digits: vec![9000] }
    }

    fn one_point_one() -> Var {
        Var { weight: 0, neg: false, dscale: 1, digits: vec![1, 1000] }
    }

    /// int64_to_numericvar (and int128_to_numericvar): the digits as they come, zeros at the end kept.
    pub(super) fn from_i128(v: i128) -> Var {
        let neg = v < 0;
        let mut u = v.unsigned_abs();
        if u == 0 {
            return Var::zero(0);
        }
        let mut digits = Vec::with_capacity(10);
        while u > 0 {
            digits.push((u % NBASE as u128) as i32);
            u /= NBASE as u128;
        }
        digits.reverse();
        Var { weight: digits.len() as i32 - 1, neg, dscale: 0, digits }
    }

    pub(super) fn from_i64(v: i64) -> Var {
        Var::from_i128(i128::from(v))
    }

    /// Decimal digits (each 0-9), the first of them times 10^`dweight`, shown with `dscale` places:
    /// set_var_from_str's conversion to base 10000, stripped.
    pub(super) fn from_decimal(neg: bool, dec: &[u8], dweight: i32, dscale: i32) -> Var {
        let ddigits = dec.len() as i32;
        let weight = if dweight >= 0 {
            (dweight + 1 + DEC_DIGITS - 1) / DEC_DIGITS - 1
        } else {
            -((-dweight - 1) / DEC_DIGITS + 1)
        };
        let offset = (weight + 1) * DEC_DIGITS - (dweight + 1);
        let ndigits = (ddigits + offset + DEC_DIGITS - 1) / DEC_DIGITS;
        let at = |k: i32| -> i32 {
            let j = k - offset;
            if (0..ddigits).contains(&j) { i32::from(dec[j as usize]) } else { 0 }
        };
        let mut digits = Vec::with_capacity(ndigits.max(0) as usize);
        for k in 0..ndigits {
            let b = k * DEC_DIGITS;
            digits.push(((at(b) * 10 + at(b + 1)) * 10 + at(b + 2)) * 10 + at(b + 3));
        }
        let mut v = Var { weight, neg, dscale, digits };
        v.strip();
        v
    }

    /// strip_var: no zero digits at either end; zero is positive, of weight 0.
    pub(super) fn strip(&mut self) {
        let lead = self.digits.iter().take_while(|&&d| d == 0).count();
        if lead > 0 {
            self.digits.drain(..lead);
            self.weight -= lead as i32;
        }
        while self.digits.last() == Some(&0) {
            self.digits.pop();
        }
        if self.digits.is_empty() {
            self.neg = false;
            self.weight = 0;
        }
    }

    pub(super) fn is_zero(&self) -> bool {
        self.digits.iter().all(|&d| d == 0)
    }

    /// get_str_from_var: the digits before the point (at least one), then `dscale` after it, cut
    /// (not rounded) there.
    pub(super) fn to_text(&self) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(((self.weight + 1).max(1) * DEC_DIGITS + self.dscale.max(0) + 3) as usize);
        if self.neg {
            s.push('-');
        }
        let digit = |d: i32| if (0..self.n()).contains(&d) { self.dig(d) } else { 0 };
        let mut d;
        if self.weight < 0 {
            d = self.weight + 1;
            s.push('0');
        } else {
            d = 0;
            while d <= self.weight {
                if d == 0 {
                    let _ = write!(s, "{}", digit(d));
                } else {
                    let _ = write!(s, "{:04}", digit(d));
                }
                d += 1;
            }
        }
        if self.dscale > 0 {
            s.push('.');
            let end = s.len() + self.dscale as usize;
            let mut i = 0;
            while i < self.dscale {
                let _ = write!(s, "{:04}", digit(d));
                d += 1;
                i += DEC_DIGITS;
            }
            s.truncate(end);
        }
        s
    }

    /// The decimal digits of |self| times 10^dscale (cut there), without leading zeros: "0" for zero.
    pub(super) fn mantissa(&self) -> String {
        let t = self.to_text();
        let digits: String = t.chars().filter(char::is_ascii_digit).collect();
        let trimmed = digits.trim_start_matches('0');
        if trimmed.is_empty() { "0".to_string() } else { trimmed.to_string() }
    }

    /// numericvar_to_double_no_overflow: through the text, as strtod reads it (an infinity past a
    /// double's range).
    pub(super) fn to_f64(&self) -> f64 {
        self.to_text().parse().unwrap_or(f64::NAN)
    }

    /// numericvar_to_int64: rounded to a whole number (half away from zero), if it fits.
    pub(super) fn to_i64(&self) -> Option<i64> {
        let mut r = self.clone();
        r.round(0);
        r.strip();
        if r.n() == 0 {
            return Some(0);
        }
        let mut val: i64 = -i64::from(r.dig(0));
        for i in 1..=r.weight {
            val = val.checked_mul(i64::from(NBASE))?;
            if i < r.n() {
                val = val.checked_sub(i64::from(r.dig(i)))?;
            }
        }
        if !r.neg {
            if val == i64::MIN {
                return None;
            }
            val = -val;
        }
        Some(val)
    }

    /// The value as a 128-bit mantissa at its display scale, if it fits one and the scale is at most
    /// 38 (digits past the scale cut off).
    pub(super) fn to_small(&self) -> Option<(i128, u32)> {
        if !(0..=38).contains(&self.dscale) {
            return None;
        }
        if self.is_zero() {
            return Some((0, self.dscale as u32));
        }
        if self.weight > 10 {
            return None;
        }
        let mut m: u128 = 0;
        for (i, &d) in self.digits.iter().enumerate() {
            // the power of ten this digit's last decimal digit stands for in the mantissa
            let e = DEC_DIGITS * (self.weight - i as i32) + self.dscale;
            if e <= -DEC_DIGITS {
                break;
            }
            let part = if e >= 0 {
                (d as u128).checked_mul(10u128.checked_pow(e as u32)?)?
            } else {
                d as u128 / 10u128.pow((-e) as u32)
            };
            m = m.checked_add(part)?;
        }
        let m = if self.neg {
            if m == 1u128 << 127 { i128::MIN } else { -i128::try_from(m).ok()? }
        } else {
            i128::try_from(m).ok()?
        };
        Some((m, self.dscale as u32))
    }

    /// The decimal weight of the first non-zero digit (10^dweight); 0 for zero.
    pub(super) fn dweight(&self) -> i32 {
        match self.digits.iter().position(|&d| d != 0) {
            Some(i) => {
                let d = self.digits[i];
                let lead = if d >= 1000 {
                    3
                } else if d >= 100 {
                    2
                } else if d >= 10 {
                    1
                } else {
                    0
                };
                (self.weight - i as i32) * DEC_DIGITS + lead
            }
            None => 0,
        }
    }

    /// round_var: half away from zero at `rscale` places (a negative one rounds left of the point),
    /// which becomes the display scale.
    pub(super) fn round(&mut self, rscale: i32) {
        self.dscale = rscale;
        let mut di = (self.weight + 1) * DEC_DIGITS + rscale;
        if di < 0 {
            self.digits.clear();
            self.weight = 0;
            self.neg = false;
            return;
        }
        let ndigits = (di + DEC_DIGITS - 1) / DEC_DIGITS;
        di %= DEC_DIGITS;
        let n = self.n();
        let cut = ndigits < n || (ndigits == n && di > 0);
        if cut {
            self.round_off(ndigits, di);
        }
    }

    /// round_var past its checks: keep `ndigits` digits, the last of them to `di` decimal digits
    /// (0: all four), and carry a rounding up.
    fn round_off(&mut self, mut ndigits: i32, di: i32) {
        let mut carry = 0;
        if di == 0 {
            carry = i32::from(self.dig(ndigits) >= HALF_NBASE);
            self.digits.truncate(ndigits as usize);
        } else {
            self.digits.truncate(ndigits as usize);
            ndigits -= 1;
            let k = ndigits as usize;
            let mut pow10 = ROUND_POWERS[di as usize];
            let extra = self.digits[k] % pow10;
            self.digits[k] -= extra;
            if extra >= pow10 / 2 {
                pow10 += self.digits[k];
                if pow10 >= NBASE {
                    pow10 -= NBASE;
                    carry = 1;
                }
                self.digits[k] = pow10;
            }
        }
        while carry != 0 {
            ndigits -= 1;
            if ndigits < 0 {
                // (into the digit before the first: one more digit, one more weight)
                self.digits.insert(0, carry);
                self.weight += 1;
                break;
            }
            let k = ndigits as usize;
            carry += self.digits[k];
            if carry >= NBASE {
                self.digits[k] = carry - NBASE;
                carry = 1;
            } else {
                self.digits[k] = carry;
                carry = 0;
            }
        }
    }

    /// trunc_var: cut toward zero at `rscale` places (a negative one cuts left of the point).
    pub(super) fn trunc(&mut self, rscale: i32) {
        self.dscale = rscale;
        let mut di = (self.weight + 1) * DEC_DIGITS + rscale;
        if di <= 0 {
            self.digits.clear();
            self.weight = 0;
            self.neg = false;
            return;
        }
        let mut ndigits = (di + DEC_DIGITS - 1) / DEC_DIGITS;
        if ndigits <= self.n() {
            self.digits.truncate(ndigits as usize);
            di %= DEC_DIGITS;
            if di > 0 {
                ndigits -= 1;
                let k = ndigits as usize;
                let pow10 = ROUND_POWERS[di as usize];
                let extra = self.digits[k] % pow10;
                self.digits[k] -= extra;
            }
        }
    }

    /// get_min_scale: the fewest places that show the value exactly.
    pub(super) fn min_scale(&self) -> i32 {
        let mut last = self.n() - 1;
        while last >= 0 && self.dig(last) == 0 {
            last -= 1;
        }
        if last < 0 {
            return 0;
        }
        let mut min_scale = (last - self.weight) * DEC_DIGITS;
        if min_scale > 0 {
            let mut d = self.dig(last);
            while d % 10 == 0 {
                min_scale -= 1;
                d /= 10;
            }
            min_scale
        } else {
            0
        }
    }

    /// The base-10000 digits without zeros at either end, and the first one's weight (for hashing
    /// and the binary protocol, which have Postgres's stored form).
    pub(super) fn groups(&self) -> (Vec<i32>, i32) {
        let mut v = self.clone();
        v.strip();
        (v.digits, v.weight)
    }
}

// ---- comparison ----

fn cmp_abs_common(d1: &[i32], mut w1: i32, d2: &[i32], mut w2: i32) -> i32 {
    let (n1, n2) = (d1.len(), d2.len());
    let (mut i1, mut i2) = (0usize, 0usize);
    while w1 > w2 && i1 < n1 {
        if d1[i1] != 0 {
            return 1;
        }
        i1 += 1;
        w1 -= 1;
    }
    while w2 > w1 && i2 < n2 {
        if d2[i2] != 0 {
            return -1;
        }
        i2 += 1;
        w2 -= 1;
    }
    if w1 == w2 {
        while i1 < n1 && i2 < n2 {
            let stat = d1[i1] - d2[i2];
            i1 += 1;
            i2 += 1;
            if stat != 0 {
                return if stat > 0 { 1 } else { -1 };
            }
        }
    }
    while i1 < n1 {
        if d1[i1] != 0 {
            return 1;
        }
        i1 += 1;
    }
    while i2 < n2 {
        if d2[i2] != 0 {
            return -1;
        }
        i2 += 1;
    }
    0
}

/// cmp_abs: -1, 0 or 1 as |a| is below, at or above |b|.
pub(super) fn cmp_abs(a: &Var, b: &Var) -> i32 {
    cmp_abs_common(&a.digits, a.weight, &b.digits, b.weight)
}

/// cmp_var.
pub(super) fn cmp_var(a: &Var, b: &Var) -> i32 {
    if a.n() == 0 {
        if b.n() == 0 {
            return 0;
        }
        return if b.neg { 1 } else { -1 };
    }
    if b.n() == 0 {
        return if !a.neg { 1 } else { -1 };
    }
    if !a.neg {
        if b.neg {
            return 1;
        }
        return cmp_abs(a, b);
    }
    if !b.neg {
        return -1;
    }
    cmp_abs(b, a)
}

pub(super) fn ordering(c: i32) -> Ordering {
    c.cmp(&0)
}

// ---- addition and subtraction ----

/// add_abs: |a| + |b|, positive.
fn add_abs(v1: &Var, v2: &Var) -> Var {
    let (n1, n2) = (v1.n(), v2.n());
    let res_weight = v1.weight.max(v2.weight) + 1;
    let res_dscale = v1.dscale.max(v2.dscale);
    let rscale1 = n1 - v1.weight - 1;
    let rscale2 = n2 - v2.weight - 1;
    let res_rscale = rscale1.max(rscale2);
    let mut res_ndigits = res_rscale + res_weight + 1;
    if res_ndigits <= 0 {
        res_ndigits = 1;
    }
    let mut digits = vec![0i32; res_ndigits as usize];
    let mut i1 = res_rscale + v1.weight + 1;
    let mut i2 = res_rscale + v2.weight + 1;
    let mut carry = 0;
    for i in (0..res_ndigits as usize).rev() {
        i1 -= 1;
        i2 -= 1;
        if (0..n1).contains(&i1) {
            carry += v1.dig(i1);
        }
        if (0..n2).contains(&i2) {
            carry += v2.dig(i2);
        }
        if carry >= NBASE {
            digits[i] = carry - NBASE;
            carry = 1;
        } else {
            digits[i] = carry;
            carry = 0;
        }
    }
    let mut r = Var { weight: res_weight, neg: false, dscale: res_dscale, digits };
    r.strip();
    r
}

/// sub_abs: |a| - |b|, positive; |a| must be at least |b|.
fn sub_abs(v1: &Var, v2: &Var) -> Var {
    let (n1, n2) = (v1.n(), v2.n());
    let res_weight = v1.weight;
    let res_dscale = v1.dscale.max(v2.dscale);
    let rscale1 = n1 - v1.weight - 1;
    let rscale2 = n2 - v2.weight - 1;
    let res_rscale = rscale1.max(rscale2);
    let mut res_ndigits = res_rscale + res_weight + 1;
    if res_ndigits <= 0 {
        res_ndigits = 1;
    }
    let mut digits = vec![0i32; res_ndigits as usize];
    let mut i1 = res_rscale + v1.weight + 1;
    let mut i2 = res_rscale + v2.weight + 1;
    let mut borrow = 0;
    for i in (0..res_ndigits as usize).rev() {
        i1 -= 1;
        i2 -= 1;
        if (0..n1).contains(&i1) {
            borrow += v1.dig(i1);
        }
        if (0..n2).contains(&i2) {
            borrow -= v2.dig(i2);
        }
        if borrow < 0 {
            digits[i] = borrow + NBASE;
            borrow = -1;
        } else {
            digits[i] = borrow;
            borrow = 0;
        }
    }
    let mut r = Var { weight: res_weight, neg: false, dscale: res_dscale, digits };
    r.strip();
    r
}

fn signed(mut v: Var, neg: bool) -> Var {
    v.neg = neg && v.n() > 0;
    v
}

/// add_var.
pub(super) fn add(a: &Var, b: &Var) -> Var {
    if a.neg == b.neg {
        return signed(add_abs(a, b), a.neg);
    }
    // the signs differ: the larger magnitude's sign
    match cmp_abs(a, b) {
        0 => Var::zero(a.dscale.max(b.dscale)),
        1 => signed(sub_abs(a, b), a.neg),
        _ => signed(sub_abs(b, a), b.neg),
    }
}

/// sub_var.
pub(super) fn sub(a: &Var, b: &Var) -> Var {
    if a.neg != b.neg {
        return signed(add_abs(a, b), a.neg);
    }
    match cmp_abs(a, b) {
        0 => Var::zero(a.dscale.max(b.dscale)),
        1 => signed(sub_abs(a, b), a.neg),
        _ => signed(sub_abs(b, a), !a.neg),
    }
}

// ---- multiplication ----

/// mul_var: the product, computed to `rscale` places (with numeric.c's guard digits) and rounded there.
pub(super) fn mul(var1: &Var, var2: &Var, rscale: i32) -> Var {
    let (v1, v2) = if var1.n() > var2.n() { (var2, var1) } else { (var1, var2) };
    let (n1, n2) = (v1.n(), v2.n());
    if n1 == 0 || n2 == 0 {
        return Var::zero(rscale);
    }
    let neg = v1.neg != v2.neg;
    let res_weight = v1.weight + v2.weight + 2;
    let maxdigits = res_weight + 1 + (rscale + DEC_DIGITS - 1) / DEC_DIGITS + MUL_GUARD_DIGITS;
    let res_ndigits = (n1 + n2 + 1).min(maxdigits);
    if res_ndigits < 3 {
        return Var::zero(rscale);
    }
    // (64-bit sums need none of mul_var's normalising passes: the value, and so the digits after the
    // final carry pass, are the same)
    let mut dig = vec![0i64; res_ndigits as usize];
    let mut i1 = (n1 - 1).min(res_ndigits - 3);
    while i1 >= 0 {
        let d1 = i64::from(v1.dig(i1));
        if d1 != 0 {
            let limit = n2.min(res_ndigits - i1 - 2).max(0) as usize;
            let base = (i1 + 2) as usize;
            for (k, &d2) in v2.digits[..limit].iter().enumerate() {
                dig[base + k] += d1 * i64::from(d2);
            }
        }
        i1 -= 1;
    }
    let mut digits = vec![0i32; res_ndigits as usize];
    let mut carry = 0i64;
    for i in (0..res_ndigits as usize).rev() {
        let newdig = dig[i] + carry;
        carry = newdig / i64::from(NBASE);
        digits[i] = (newdig % i64::from(NBASE)) as i32;
    }
    let mut r = Var { weight: res_weight, neg, dscale: 0, digits };
    r.round(rscale);
    r.strip();
    r
}

// ---- division ----

/// The divisor as one integer and its weight, when it has at most four digits (div_var hands those
/// to div_var_int and div_var_int64).
fn short_divisor(v: &Var) -> Option<(i64, i32)> {
    if v.n() > 4 {
        return None;
    }
    let mut d = i64::from(v.dig(0));
    let mut w = v.weight;
    for i in 1..v.n() {
        d = d * i64::from(NBASE) + i64::from(v.dig(i));
        w -= 1;
    }
    Some((if v.neg { -d } else { d }, w))
}

/// div_var_int / div_var_int64: `var / (ival * NBASE^ival_weight)` by short division, exact to
/// `rscale` places and then rounded or cut there.
pub(super) fn div_int(var: &Var, ival: i64, ival_weight: i32, rscale: i32, round: bool) -> R<Var> {
    if ival == 0 {
        return Err(DIV_ZERO);
    }
    let n = var.n();
    if n == 0 {
        return Ok(Var::zero(rscale));
    }
    let neg = if !var.neg { ival < 0 } else { ival > 0 };
    let res_weight = var.weight - ival_weight;
    let mut res_ndigits = (res_weight + 1 + (rscale + DEC_DIGITS - 1) / DEC_DIGITS).max(1);
    if round {
        res_ndigits += 1;
    }
    let divisor = u128::from(ival.unsigned_abs());
    let mut carry: u128 = 0;
    let mut digits = Vec::with_capacity(res_ndigits as usize);
    for i in 0..res_ndigits {
        carry = carry * NBASE as u128 + if i < n { var.dig(i) as u128 } else { 0 };
        digits.push((carry / divisor) as i32);
        carry %= divisor;
    }
    let mut r = Var { weight: res_weight, neg, dscale: 0, digits };
    if round {
        r.round(rscale);
    } else {
        r.trunc(rscale);
    }
    r.strip();
    Ok(r)
}

/// div_var: the quotient exact to `rscale` places (Knuth's long division), rounded half away from
/// zero there or (`round` false) cut toward zero.
pub(super) fn div(var1: &Var, var2: &Var, rscale: i32, round: bool) -> R<Var> {
    let (n1, n2) = (var1.n(), var2.n());
    if n2 == 0 || var2.dig(0) == 0 {
        return Err(DIV_ZERO);
    }
    if let Some((d, w)) = short_divisor(var2) {
        return div_int(var1, d, w, rscale, round);
    }
    if n1 == 0 {
        return Ok(Var::zero(rscale));
    }
    let neg = var1.neg != var2.neg;
    let res_weight = var1.weight - var2.weight;
    let mut res_ndigits = (res_weight + 1 + (rscale + DEC_DIGITS - 1) / DEC_DIGITS).max(1);
    if round {
        res_ndigits += 1;
    }
    let div_ndigits = (res_ndigits + n2).max(n1);
    // dividend[0] is room for a carry; divisor[0] is a zero before the divisor's digits
    let mut dividend = vec![0i64; (div_ndigits + 1) as usize];
    for (i, &d) in var1.digits.iter().enumerate() {
        dividend[i + 1] = i64::from(d);
    }
    let mut divisor = vec![0i64; (n2 + 1) as usize];
    for (i, &d) in var2.digits.iter().enumerate() {
        divisor[i + 1] = i64::from(d);
    }
    let nbase = i64::from(NBASE);
    if divisor[1] < i64::from(HALF_NBASE) {
        let d = nbase / (divisor[1] + 1);
        let mut carry = 0;
        for i in (1..=n2 as usize).rev() {
            carry += divisor[i] * d;
            divisor[i] = carry % nbase;
            carry /= nbase;
        }
        carry = 0;
        for i in (0..=n1 as usize).rev() {
            carry += dividend[i] * d;
            dividend[i] = carry % nbase;
            carry /= nbase;
        }
    }
    let (divisor1, divisor2) = (divisor[1], divisor[2]);
    let n2u = n2 as usize;
    let mut res = vec![0i32; res_ndigits as usize];
    for j in 0..res_ndigits as usize {
        let next2digits = dividend[j] * nbase + dividend[j + 1];
        if next2digits == 0 {
            res[j] = 0;
            continue;
        }
        let mut qhat = if dividend[j] == divisor1 { nbase - 1 } else { next2digits / divisor1 };
        while divisor2 * qhat > (next2digits - qhat * divisor1) * nbase + dividend[j + 2] {
            qhat -= 1;
        }
        if qhat > 0 {
            let mut borrow = 0;
            for i in (0..=n2u).rev() {
                let tmp = dividend[j + i] - borrow - divisor[i] * qhat;
                borrow = (nbase - 1 - tmp) / nbase;
                dividend[j + i] = tmp + borrow * nbase;
            }
            if borrow != 0 {
                qhat -= 1;
                let mut carry = 0;
                for i in (0..=n2u).rev() {
                    carry += dividend[j + i] + divisor[i];
                    if carry >= nbase {
                        dividend[j + i] = carry - nbase;
                        carry = 1;
                    } else {
                        dividend[j + i] = carry;
                        carry = 0;
                    }
                }
            }
        }
        res[j] = qhat as i32;
    }
    let mut r = Var { weight: res_weight, neg, dscale: 0, digits: res };
    if round {
        r.round(rscale);
    } else {
        r.trunc(rscale);
    }
    r.strip();
    Ok(r)
}

/// The quotient digit div_var_fast estimates from the dividend's next four digits.
fn fast_qdigit(div: &[i32], qi: usize, div_ndigits: usize, inverse: f64) -> i32 {
    let mut fdividend = f64::from(div[qi]);
    for i in 1..4 {
        fdividend *= f64::from(NBASE);
        if qi + i <= div_ndigits {
            fdividend += f64::from(div[qi + i]);
        }
    }
    let fquotient = fdividend * inverse;
    if fquotient >= 0.0 { fquotient as i32 } else { (fquotient as i32).wrapping_sub(1) }
}

/// div_var_fast: the "FM" library's division, which the transcendental functions use. Its digits
/// past the guard digits may be off, so it is ported as it is, int arithmetic and all.
pub(super) fn div_fast(var1: &Var, var2: &Var, rscale: i32, round: bool) -> R<Var> {
    let (n1, n2) = (var1.n(), var2.n());
    if n2 == 0 || var2.dig(0) == 0 {
        return Err(DIV_ZERO);
    }
    if let Some((d, w)) = short_divisor(var2) {
        return div_int(var1, d, w, rscale, round);
    }
    if n1 == 0 {
        return Ok(Var::zero(rscale));
    }
    let neg = var1.neg != var2.neg;
    let res_weight = var1.weight - var2.weight + 1;
    let mut div_ndigits = res_weight + 1 + (rscale + DEC_DIGITS - 1) / DEC_DIGITS;
    div_ndigits += DIV_GUARD_DIGITS;
    if div_ndigits < DIV_GUARD_DIGITS {
        div_ndigits = DIV_GUARD_DIGITS;
    }
    let dn = div_ndigits as usize;
    let mut div = vec![0i32; dn + 1];
    let load = div_ndigits.min(n1) as usize;
    div[1..=load].copy_from_slice(&var1.digits[..load]);
    let mut fdivisor = f64::from(var2.dig(0));
    for i in 1..4 {
        fdivisor *= f64::from(NBASE);
        if i < n2 {
            fdivisor += f64::from(var2.dig(i));
        }
    }
    let inverse = 1.0 / fdivisor;
    let limit = (i32::MAX - i32::MAX / NBASE - 1) / (NBASE - 1);
    let mut maxdiv: i32 = 1;
    let mut qi = 0usize;
    while qi < dn {
        let mut qdigit = fast_qdigit(&div, qi, dn, inverse);
        if qdigit != 0 {
            maxdiv = maxdiv.wrapping_add(qdigit.wrapping_abs());
            if maxdiv > limit {
                // normalise the digits prior steps subtracted from
                let mut carry: i32 = 0;
                let mut i = (qi as i32 + n2 - 2).min(div_ndigits);
                while i > qi as i32 {
                    let k = i as usize;
                    let mut newdig = div[k].wrapping_add(carry);
                    if newdig < 0 {
                        carry = -((-newdig - 1) / NBASE) - 1;
                        newdig -= carry * NBASE;
                    } else if newdig >= NBASE {
                        carry = newdig / NBASE;
                        newdig -= carry * NBASE;
                    } else {
                        carry = 0;
                    }
                    div[k] = newdig;
                    i -= 1;
                }
                div[qi] = div[qi].wrapping_add(carry);
                maxdiv = 1;
                qdigit = fast_qdigit(&div, qi, dn, inverse);
                maxdiv = maxdiv.wrapping_add(qdigit.wrapping_abs());
            }
            if qdigit != 0 {
                let istop = n2.min(div_ndigits - qi as i32 + 1).max(0) as usize;
                for i in 0..istop {
                    div[qi + i] = div[qi + i].wrapping_sub(qdigit.wrapping_mul(var2.digits[i]));
                }
            }
        }
        // (may overflow and cancel, as numeric.c says it does)
        div[qi + 1] = div[qi + 1].wrapping_add(div[qi].wrapping_mul(NBASE));
        div[qi] = qdigit;
        qi += 1;
    }
    let mut fdividend = f64::from(div[qi]);
    for _ in 1..4 {
        fdividend *= f64::from(NBASE);
    }
    let fquotient = fdividend * inverse;
    div[qi] = if fquotient >= 0.0 { fquotient as i32 } else { (fquotient as i32).wrapping_sub(1) };
    let mut digits = vec![0i32; dn + 1];
    let mut carry: i32 = 0;
    for i in (0..=dn).rev() {
        let mut newdig = div[i].wrapping_add(carry);
        if newdig < 0 {
            carry = -((-newdig - 1) / NBASE) - 1;
            newdig -= carry * NBASE;
        } else if newdig >= NBASE {
            carry = newdig / NBASE;
            newdig -= carry * NBASE;
        } else {
            carry = 0;
        }
        digits[i] = newdig;
    }
    let mut r = Var { weight: res_weight, neg, dscale: 0, digits };
    if round {
        r.round(rscale);
    } else {
        r.trunc(rscale);
    }
    r.strip();
    Ok(r)
}

/// select_div_scale: enough places for 16 significant digits, no fewer than either side shows,
/// at most 1000.
pub(super) fn div_scale_of(w1: i32, first1: i32, dscale1: i32, w2: i32, first2: i32, dscale2: i32) -> i32 {
    let mut qweight = w1 - w2;
    if first1 <= first2 {
        qweight -= 1;
    }
    (MIN_SIG_DIGITS - qweight * DEC_DIGITS).max(dscale1).max(dscale2).clamp(0, MAX_DISPLAY_SCALE)
}

/// The weight of the first non-zero digit and that digit (0 and 0 for zero), as select_div_scale
/// finds them.
pub(super) fn head(v: &Var) -> (i32, i32) {
    for (i, &d) in v.digits.iter().enumerate() {
        if d != 0 {
            return (v.weight - i as i32, d);
        }
    }
    (0, 0)
}

/// mod_var: x - trunc(x / y) * y.
pub(super) fn modulo(v1: &Var, v2: &Var) -> R<Var> {
    let tmp = div(v1, v2, 0, false)?;
    let tmp = mul(v2, &tmp, v2.dscale);
    Ok(sub(v1, &tmp))
}

/// div_mod_var: the quotient cut to a whole number, and the remainder.
fn div_mod(v1: &Var, v2: &Var) -> R<(Var, Var)> {
    let mut q = div_fast(v1, v2, 0, false)?;
    let mut r = mul(v2, &q, v2.dscale);
    r = sub(v1, &r);
    let one = Var::one();
    while r.n() != 0 && r.neg != v1.neg {
        if v1.neg == v2.neg {
            q = sub(&q, &one);
            r = add(&r, v2);
        } else {
            q = add(&q, &one);
            r = sub(&r, v2);
        }
    }
    while cmp_abs(&r, v2) >= 0 {
        if v1.neg == v2.neg {
            q = add(&q, &one);
            r = sub(&r, v2);
        } else {
            q = sub(&q, &one);
            r = add(&r, v2);
        }
    }
    Ok((q, r))
}

/// ceil_var.
pub(super) fn ceil(v: &Var) -> Var {
    let mut t = v.clone();
    t.trunc(0);
    if !v.neg && cmp_var(v, &t) != 0 {
        t = add(&t, &Var::one());
    }
    t
}

/// floor_var.
pub(super) fn floor(v: &Var) -> Var {
    let mut t = v.clone();
    t.trunc(0);
    if v.neg && cmp_var(v, &t) != 0 {
        t = sub(&t, &Var::one());
    }
    t
}

/// gcd_var: by Euclid's algorithm, positive, at the larger display scale.
pub(super) fn gcd(a: &Var, b: &Var) -> R<Var> {
    let res_dscale = a.dscale.max(b.dscale);
    let cmp = cmp_abs(a, b);
    let (v1, v2) = if cmp < 0 { (b, a) } else { (a, b) };
    if cmp == 0 || v2.n() == 0 {
        let mut r = v1.clone();
        r.neg = false;
        r.dscale = res_dscale;
        return Ok(r);
    }
    let mut tmp = v1.clone();
    let mut result = v2.clone();
    loop {
        let m = modulo(&tmp, &result)?;
        if m.n() == 0 {
            break;
        }
        tmp = result;
        result = m;
    }
    result.neg = false;
    result.dscale = res_dscale;
    Ok(result)
}

// ---- square root ----

/// sqrt_var: by the Karatsuba square root, to `rscale` places (which may be negative), rounded.
pub(super) fn sqrt(arg: &Var, rscale: i32) -> R<Var> {
    let stat = cmp_var(arg, &Var::zero(0));
    if stat == 0 {
        return Ok(Var::zero(rscale));
    }
    if stat < 0 {
        return Err(SQRT_NEG);
    }
    let an = arg.n();
    let res_weight = if arg.weight >= 0 { arg.weight / 2 } else { -((-arg.weight - 1) / 2 + 1) };
    let mut res_ndigits = if rscale + 1 >= 0 {
        res_weight + 1 + (rscale + DEC_DIGITS) / DEC_DIGITS
    } else {
        res_weight + 1 - (-rscale - 1) / DEC_DIGITS
    };
    res_ndigits = res_ndigits.max(1);
    let mut src_ndigits = (arg.weight + 1 + (res_ndigits - res_weight - 1) * 2).max(1);

    // the digits of the input used at the end of each step, outermost first
    let mut ndigits = [0i32; 32];
    let mut step = 0usize;
    loop {
        ndigits[step] = src_ndigits;
        if src_ndigits <= 4 {
            break;
        }
        let mut blen = src_ndigits / 4;
        if blen * 4 == src_ndigits && arg.dig(0) < NBASE / 4 {
            blen -= 1;
        }
        src_ndigits -= 2 * blen;
        step += 1;
    }
    let digit = |i: i32| -> i64 { if i < an { i64::from(arg.dig(i)) } else { 0 } };

    // the innermost square root and remainder, in 64 bits
    let mut arg_int64 = i64::from(arg.dig(0));
    let mut src_idx = 1;
    while src_idx < src_ndigits {
        arg_int64 *= i64::from(NBASE);
        arg_int64 += digit(src_idx);
        src_idx += 1;
    }
    let mut s_int64 = (arg_int64 as f64).sqrt() as i64;
    let mut r_int64 = arg_int64 - s_int64 * s_int64;
    while r_int64 < 0 || r_int64 > 2 * s_int64 {
        s_int64 = (s_int64 + arg_int64 / s_int64) / 2;
        r_int64 = arg_int64 - s_int64 * s_int64;
    }

    // steps of at most 8 input digits: still 64 bits
    let mut step = step as i32 - 1;
    while step >= 0 {
        src_ndigits = ndigits[step as usize];
        if src_ndigits > 8 {
            break;
        }
        let blen = (src_ndigits - src_idx) / 2;
        let (mut a0, mut a1, mut b) = (0i64, 0i64, 1i64);
        for _ in 0..blen {
            b *= i64::from(NBASE);
            a1 = a1 * i64::from(NBASE) + digit(src_idx);
            src_idx += 1;
        }
        for _ in 0..blen {
            a0 = a0 * i64::from(NBASE) + digit(src_idx);
            src_idx += 1;
        }
        let numer = r_int64 * b + a1;
        let denom = 2 * s_int64;
        let q = numer / denom;
        let u = numer - q * denom;
        s_int64 = s_int64 * b + q;
        r_int64 = u * b + a0 - q * q;
        if r_int64 < 0 {
            r_int64 += s_int64;
            s_int64 -= 1;
            r_int64 += s_int64;
        }
        step -= 1;
    }

    // steps of at most 16: 128 bits
    let (mut s_var, mut r_var) = if step >= 0 {
        let (mut s128, mut r128) = (i128::from(s_int64), i128::from(r_int64));
        while step >= 0 {
            src_ndigits = ndigits[step as usize];
            if src_ndigits > 16 {
                break;
            }
            let blen = (src_ndigits - src_idx) / 2;
            let (mut a0, mut a1, mut b) = (0i128, 0i128, 1i128);
            for _ in 0..blen {
                b *= i128::from(NBASE);
                a1 = a1 * i128::from(NBASE) + i128::from(digit(src_idx));
                src_idx += 1;
            }
            for _ in 0..blen {
                a0 = a0 * i128::from(NBASE) + i128::from(digit(src_idx));
                src_idx += 1;
            }
            let numer = r128 * b + a1;
            let denom = 2 * s128;
            let q = numer / denom;
            let u = numer - q * denom;
            s128 = s128 * b + q;
            r128 = u * b + a0 - q * q;
            if r128 < 0 {
                r128 += s128;
                s128 -= 1;
                r128 += s128;
            }
            step -= 1;
        }
        (Var::from_i128(s128), if step >= 0 { Var::from_i128(r128) } else { Var::zero(0) })
    } else {
        (Var::from_i64(s_int64), Var::zero(0))
    };

    // the rest in numerics
    let one = Var::one();
    while step >= 0 {
        src_ndigits = ndigits[step as usize];
        let blen = (src_ndigits - src_idx) / 2;
        let part = |from: i32| -> Var {
            if from < an {
                let len = blen.min(an - from).max(0) as usize;
                let mut v = Var {
                    weight: blen - 1,
                    neg: false,
                    dscale: 0,
                    digits: arg.digits[from as usize..from as usize + len].to_vec(),
                };
                v.strip();
                v
            } else {
                Var::zero(0)
            }
        };
        let a1_var = part(src_idx);
        src_idx += blen;
        let a0_var = part(src_idx);
        src_idx += blen;

        // (q, u) = DivRem(r * b + a1, 2 * s)
        let mut q_var = r_var.clone();
        q_var.weight += blen;
        q_var = add(&q_var, &a1_var);
        let u_var = add(&s_var, &s_var);
        let (q_var, u_var) = div_mod(&q_var, &u_var)?;

        // s = s * b + q
        s_var.weight += blen;
        s_var = add(&s_var, &q_var);

        // r = u * b + a0 - q^2 (in the last step, only whether it is negative)
        let mut u_var = u_var;
        u_var.weight += blen;
        let u_var = add(&u_var, &a0_var);
        let q_sq = mul(&q_var, &q_var, 0);
        if step > 0 {
            r_var = sub(&u_var, &q_sq);
            if r_var.neg {
                r_var = add(&r_var, &s_var);
                s_var = sub(&s_var, &one);
                r_var = add(&r_var, &s_var);
            }
        } else if cmp_var(&u_var, &q_sq) < 0 {
            s_var = sub(&s_var, &one);
        }
        step -= 1;
    }

    let mut result = s_var;
    result.weight = res_weight;
    result.neg = false;
    result.round(rscale);
    result.strip();
    Ok(result)
}

// ---- exp, ln, log, power ----

/// exp_var: e^x to `rscale` places.
pub(super) fn exp(arg: &Var, rscale: i32) -> R<Var> {
    let mut x = arg.clone();
    let mut val = x.to_f64();
    if val.abs() >= f64::from(MAX_RESULT_SCALE * 3) {
        if val > 0.0 {
            return Err(OVERFLOW);
        }
        return Ok(Var::zero(rscale));
    }
    // decimal weight = log10(e^x) = x * log10(e)
    let dweight = (val * 0.434294481903252) as i32;
    // x brought to about [-0.01, 0.01] by halving it ndiv2 times
    let mut ndiv2 = 0;
    if val.abs() > 0.01 {
        ndiv2 = 1;
        val /= 2.0;
        while val.abs() > 0.01 {
            ndiv2 += 1;
            val /= 2.0;
        }
        let local_rscale = x.dscale + ndiv2;
        x = div_int(&x, 1i64 << ndiv2, 0, local_rscale, true)?;
    }
    let mut sig_digits = 1 + dweight + rscale + (f64::from(ndiv2) * 0.301029995663981) as i32;
    sig_digits = sig_digits.max(0) + 8;
    let local_rscale = sig_digits - 1;

    // the Taylor series, 1 + x + x^2/2! + x^3/3! + ..., until its terms are below local_rscale
    let mut result = add(&Var::one(), &x);
    let mut elem = mul(&x, &x, local_rscale);
    let mut ni: i64 = 2;
    elem = div_int(&elem, ni, 0, local_rscale, true)?;
    while elem.n() != 0 {
        result = add(&result, &elem);
        elem = mul(&elem, &x, local_rscale);
        ni += 1;
        elem = div_int(&elem, ni, 0, local_rscale, true)?;
    }
    // squared back ndiv2 times
    while ndiv2 > 0 {
        ndiv2 -= 1;
        let local_rscale = (sig_digits - result.weight * 2 * DEC_DIGITS).max(0);
        result = mul(&result, &result, local_rscale);
    }
    result.round(rscale);
    Ok(result)
}

/// estimate_ln_dweight: about where the first digit of ln(var) is; 0 for what ln refuses.
pub(super) fn ln_dweight(var: &Var) -> i32 {
    if var.neg {
        return 0;
    }
    if cmp_var(var, &Var::zero_point_nine()) >= 0 && cmp_var(var, &Var::one_point_one()) <= 0 {
        // ln(1 + x) is about x
        let x = sub(var, &Var::one());
        if x.n() > 0 { x.weight * DEC_DIGITS + f64::from(x.dig(0)).log10() as i32 } else { 0 }
    } else if var.n() > 0 {
        let mut digits = var.dig(0);
        let mut dweight = var.weight * DEC_DIGITS;
        if var.n() > 1 {
            digits = digits * NBASE + var.dig(1);
            dweight -= DEC_DIGITS;
        }
        let ln_var = f64::from(digits).ln() + f64::from(dweight) * 2.302585092994046;
        ln_var.abs().log10() as i32
    } else {
        0
    }
}

/// ln_var: the natural logarithm to `rscale` places.
pub(super) fn ln(arg: &Var, rscale: i32) -> R<Var> {
    match cmp_var(arg, &Var::zero(0)) {
        0 => return Err(LOG_ZERO),
        c if c < 0 => return Err(LOG_NEG),
        _ => {}
    }
    let mut x = arg.clone();
    let mut fact = Var::two();
    let two = Var::two();
    // into 0.9 < x < 1.1 by square roots, each keeping enough digits
    let mut nsqrt = 0;
    while cmp_var(&x, &Var::zero_point_nine()) <= 0 {
        let local_rscale = rscale - x.weight * DEC_DIGITS / 2 + 8;
        x = sqrt(&x, local_rscale)?;
        fact = mul(&fact, &two, 0);
        nsqrt += 1;
    }
    while cmp_var(&x, &Var::one_point_one()) >= 0 {
        let local_rscale = rscale - x.weight * DEC_DIGITS / 2 + 8;
        x = sqrt(&x, local_rscale)?;
        fact = mul(&fact, &two, 0);
        nsqrt += 1;
    }
    // z + z^3/3 + z^5/5 + ..., z = (x - 1) / (x + 1)
    let local_rscale = rscale + (f64::from(nsqrt + 1) * 0.301029995663981) as i32 + 8;
    let one = Var::one();
    let mut result = sub(&x, &one);
    let elem = add(&x, &one);
    result = div_fast(&result, &elem, local_rscale, true)?;
    let mut xx = result.clone();
    let x = mul(&result, &result, local_rscale);
    let mut ni: i64 = 1;
    loop {
        ni += 2;
        xx = mul(&xx, &x, local_rscale);
        let elem = div_int(&xx, ni, 0, local_rscale, true)?;
        if elem.n() == 0 {
            break;
        }
        result = add(&result, &elem);
        if elem.weight < result.weight - local_rscale * 2 / DEC_DIGITS {
            break;
        }
    }
    Ok(mul(&result, &fact, rscale))
}

/// log_var: the logarithm of `num` to `base`, at the scale it picks (16 significant digits, no fewer
/// places than either argument shows).
pub(super) fn log(base: &Var, num: &Var) -> R<Var> {
    let ln_base_dweight = ln_dweight(base);
    let ln_num_dweight = ln_dweight(num);
    let result_dweight = ln_num_dweight - ln_base_dweight;
    let rscale = (MIN_SIG_DIGITS - result_dweight).max(base.dscale).max(num.dscale).clamp(0, MAX_DISPLAY_SCALE);
    let ln_base_rscale = (rscale + result_dweight - ln_base_dweight + 8).max(0);
    let ln_num_rscale = (rscale + result_dweight - ln_num_dweight + 8).max(0);
    let ln_base = ln(base, ln_base_rscale)?;
    let ln_num = ln(num, ln_num_rscale)?;
    div_fast(&ln_num, &ln_base, rscale, true)
}

/// power_var: base ^ exp, at the scale it picks.
pub(super) fn power(base: &Var, exp: &Var) -> R<Var> {
    // a whole exponent that fits an int: power_var_int
    if (exp.n() == 0 || exp.n() <= exp.weight + 1)
        && let Some(e) = exp.to_i64()
        && let Ok(e) = i32::try_from(e)
    {
        return power_int(base, e, exp.dscale);
    }
    if cmp_var(base, &Var::zero(0)) == 0 {
        return Ok(Var::zero(MIN_SIG_DIGITS));
    }
    let abs_base;
    let (base, res_neg) = if base.neg {
        if exp.n() > 0 && exp.n() > exp.weight + 1 {
            return Err(NEG_POWER);
        }
        let odd = exp.n() > 0 && exp.n() == exp.weight + 1 && (exp.dig(exp.n() - 1) & 1) == 1;
        abs_base = Var { neg: false, ..base.clone() };
        (&abs_base, odd)
    } else {
        (base, false)
    };
    // the result's weight from a low-precision exp * ln(base), and an early overflow test
    let ln_dw = ln_dweight(base);
    let local_rscale = (8 - ln_dw).max(0);
    let ln_base = ln(base, local_rscale)?;
    let ln_num = mul(&ln_base, exp, local_rscale);
    let mut val = ln_num.to_f64();
    if val.abs() > f64::from(MAX_RESULT_SCALE) * 3.01 {
        if val > 0.0 {
            return Err(OVERFLOW);
        }
        return Ok(Var::zero(MAX_DISPLAY_SCALE));
    }
    val *= 0.434294481903252;
    let rscale = (MIN_SIG_DIGITS - val as i32).max(base.dscale).max(exp.dscale).clamp(0, MAX_DISPLAY_SCALE);
    let sig_digits = (rscale + val as i32).max(0);
    let local_rscale = (sig_digits - ln_dw + 8).max(0);
    let ln_base = ln(base, local_rscale)?;
    let ln_num = mul(&ln_base, exp, local_rscale);
    let mut result = self::exp(&ln_num, rscale)?;
    if res_neg && result.n() > 0 {
        result.neg = true;
    }
    Ok(result)
}

/// power_var_int: base ^ exp for a whole exp, by repeated squaring at enough significant digits.
fn power_int(base: &Var, exp: i32, exp_dscale: i32) -> R<Var> {
    // base is about f * 10^p, so log10(result) is about exp * (log10(f) + p)
    let f = if base.n() != 0 {
        let mut f = f64::from(base.dig(0));
        let mut p = base.weight * DEC_DIGITS;
        let mut i = 1;
        while i < base.n() && i * DEC_DIGITS < 16 {
            f = f * f64::from(NBASE) + f64::from(base.dig(i));
            p -= DEC_DIGITS;
            i += 1;
        }
        f64::from(exp) * (f.log10() + f64::from(p))
    } else {
        0.0
    };
    if f > f64::from((WEIGHT_MAX + 1) * DEC_DIGITS) {
        return Err(OVERFLOW);
    }
    if f + 1.0 < -f64::from(MAX_DISPLAY_SCALE) {
        return Ok(Var::zero(MAX_DISPLAY_SCALE));
    }
    let rscale = (MIN_SIG_DIGITS - f as i32).max(base.dscale).max(exp_dscale).clamp(0, MAX_DISPLAY_SCALE);
    match exp {
        0 => {
            let mut r = Var::one();
            r.dscale = rscale;
            return Ok(r);
        }
        1 => {
            let mut r = base.clone();
            r.round(rscale);
            return Ok(r);
        }
        -1 => return div(&Var::one(), base, rscale, true),
        2 => return Ok(mul(base, base, rscale)),
        _ => {}
    }
    if base.n() == 0 {
        if exp < 0 {
            return Err(DIV_ZERO);
        }
        return Ok(Var::zero(rscale));
    }
    let mut sig_digits = 1 + rscale + f as i32;
    sig_digits += f64::from(exp).abs().ln() as i32 + 8;
    let mut neg = exp < 0;
    let mut mask = exp.unsigned_abs();
    let mut base_prod = base.clone();
    let mut result = if mask & 1 == 1 { base.clone() } else { Var::one() };
    loop {
        mask >>= 1;
        if mask == 0 {
            break;
        }
        let local_rscale = (sig_digits - 2 * base_prod.weight * DEC_DIGITS).clamp(0, (2 * base_prod.dscale).max(0));
        base_prod = mul(&base_prod, &base_prod, local_rscale);
        if mask & 1 == 1 {
            let local_rscale = (sig_digits - (base_prod.weight + result.weight) * DEC_DIGITS)
                .clamp(0, (base_prod.dscale + result.dscale).max(0));
            result = mul(&base_prod, &result, local_rscale);
        }
        // past a stored numeric's weight: an overflow, or for a negative exp zero
        if base_prod.weight > WEIGHT_MAX || result.weight > WEIGHT_MAX {
            if !neg {
                return Err(OVERFLOW);
            }
            result = Var::zero(result.dscale);
            neg = false;
            break;
        }
    }
    if neg {
        div_fast(&Var::one(), &result, rscale, true)
    } else {
        result.round(rscale);
        Ok(result)
    }
}

/// The comparisons `numeric_power` needs before power_var: whether `v` is exactly 1 or -1.
pub(super) fn is_one(v: &Var) -> bool {
    cmp_var(v, &Var::one()) == 0
}

pub(super) fn is_minus_one(v: &Var) -> bool {
    cmp_var(v, &Var::minus_one()) == 0
}
