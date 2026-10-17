//! `numeric`: exact decimals of Postgres's whole range, up to 131072 digits before the point and
//! 16383 after it. A value a 128-bit mantissa holds at at most 38 places is kept as one, and sums,
//! differences, products and comparisons of such values are done in it; anything larger (or with
//! more places), and every result whose scale and rounding numeric.c defines step by step
//! (division, sqrt, exp, ln, log, power), goes through numeric.c's own algorithms (see `var`), so
//! results print digit for digit as Postgres's.

// numeric.c writes these constants (log10(e), log10(2), ln(10)) out to 15 digits, and results depend on those
// digits: they are kept as Postgres has them, not replaced by f64's more exact ones
#![allow(clippy::approx_constant)]

mod var;

use std::borrow::{Borrow, Cow};
use std::cmp::Ordering;
use std::sync::Arc;
use var::Var;

/// A numeric: a finite value, NaN, Infinity or -Infinity. Each value has one form (a value that
/// fits the small one is never kept as a `Var`), so `==` is the same value at the same scale.
#[derive(Clone, Debug)]
pub struct Dec(Repr);

#[derive(Clone, Debug)]
enum Repr {
    /// m / 10^scale, the scale at most 38; or, at scale SPECIAL, NaN (m 0), Infinity (1) or -Infinity (-1)
    Small(i128, u32),
    /// any other finite value, stripped (see `Var::strip`)
    Big(Arc<Var>),
}

/// The most places a small numeric has.
const MAX_SCALE: u32 = 38;

/// The scale of NaN and the infinities: their mantissa says which (0 NaN, 1 Infinity, -1 -Infinity).
const SPECIAL: u32 = u32::MAX;

pub(super) type R<T> = Result<T, &'static str>;

const OVERFLOW: &str = var::OVERFLOW;
const ZERO_NEG_POWER: &str = "zero raised to a negative power is undefined";
const FACTORIAL_NEG: &str = "factorial of a negative number is undefined";

/// The SQLSTATE of an error a numeric function gives (by its message).
pub fn sqlstate(msg: &str) -> &'static str {
    match msg {
        var::DIV_ZERO => "22012",
        var::SQRT_NEG | var::NEG_POWER | ZERO_NEG_POWER => "2201F",
        var::LOG_ZERO | var::LOG_NEG => "2201E",
        _ => "22003",
    }
}

fn pow10(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

/// Both mantissas at the larger scale, if they still fit.
fn align(x: i128, xs: u32, y: i128, ys: u32) -> Option<(i128, i128, u32)> {
    let s = xs.max(ys);
    Some((x.checked_mul(pow10(s - xs)?)?, y.checked_mul(pow10(s - ys)?)?, s))
}

/// How many decimal digits |m| has (0 for 0).
fn digit_count(m: i128) -> u32 {
    match m.unsigned_abs() {
        0 => 0,
        u => u.ilog10() + 1,
    }
}

impl PartialEq for Dec {
    fn eq(&self, o: &Dec) -> bool {
        match (&self.0, &o.0) {
            (Repr::Small(m, s), Repr::Small(n, t)) => m == n && s == t,
            (Repr::Big(a), Repr::Big(b)) => {
                a.neg == b.neg && a.weight == b.weight && a.dscale == b.dscale && a.digits == b.digits
            }
            _ => false,
        }
    }
}

#[allow(clippy::should_implement_trait)] // they return Result: overflow is an error, not a panic
impl Dec {
    pub fn int(n: i64) -> Dec {
        Dec(Repr::Small(i128::from(n), 0))
    }

    pub const NAN: Dec = Dec(Repr::Small(0, SPECIAL));
    pub const INF: Dec = Dec(Repr::Small(1, SPECIAL));
    pub const NEG_INF: Dec = Dec(Repr::Small(-1, SPECIAL));

    /// m / 10^scale (at scale u32::MAX, the special value m names).
    pub fn new(m: i128, scale: u32) -> Dec {
        if scale <= MAX_SCALE || scale == SPECIAL {
            return Dec(Repr::Small(m, scale));
        }
        let mut v = small_var(m, scale);
        v.strip();
        Dec(Repr::Big(Arc::new(v)))
    }

    /// The finite value at most 38 places hold as m / 10^scale.
    fn small(&self) -> Option<(i128, u32)> {
        match self.0 {
            Repr::Small(m, s) if s != SPECIAL => Some((m, s)),
            _ => None,
        }
    }

    /// m and scale of a finite value a 128-bit mantissa holds at at most 38 places (m / 10^scale).
    pub fn as_small(&self) -> Option<(i128, u32)> {
        self.small()
    }

    /// A finite value in numeric.c's form.
    fn var(&self) -> Cow<'_, Var> {
        match &self.0 {
            Repr::Big(v) => Cow::Borrowed(&**v),
            Repr::Small(m, s) => Cow::Owned(small_var(*m, *s)),
        }
    }

    /// make_result: a numeric from numeric.c's form, if a stored numeric holds it (weight at most
    /// 32767, at most 16383 places).
    fn from_var(mut v: Var) -> R<Dec> {
        v.strip();
        if v.dscale > var::DSCALE_MAX || v.weight > var::WEIGHT_MAX || v.weight < i32::from(i16::MIN) {
            return Err(OVERFLOW);
        }
        let v = Var { dscale: v.dscale.max(0), ..v };
        Ok(match v.to_small() {
            Some((m, s)) => Dec(Repr::Small(m, s)),
            None => Dec(Repr::Big(Arc::new(v))),
        })
    }

    /// NaN, Infinity or -Infinity.
    pub fn is_special(&self) -> bool {
        matches!(self.0, Repr::Small(_, SPECIAL))
    }

    pub fn is_nan(&self) -> bool {
        matches!(self.0, Repr::Small(0, SPECIAL))
    }

    /// A finite zero (at any scale).
    pub fn is_zero(&self) -> bool {
        match &self.0 {
            Repr::Small(m, s) => *m == 0 && *s != SPECIAL,
            Repr::Big(v) => v.is_zero(),
        }
    }

    /// -1, 0 or 1; an infinity's sign, and 0 for NaN.
    pub fn signum(&self) -> i32 {
        match &self.0 {
            Repr::Small(m, _) => m.signum() as i32,
            Repr::Big(v) if v.is_zero() => 0,
            Repr::Big(v) => {
                if v.neg {
                    -1
                } else {
                    1
                }
            }
        }
    }

    /// The display scale: digits shown after the point (u32::MAX for NaN and the infinities).
    pub fn scale(&self) -> u32 {
        match &self.0 {
            Repr::Small(_, s) => *s,
            Repr::Big(v) => v.dscale as u32,
        }
    }

    /// Where it sorts among NaN and the infinities: -1 below every number, 1 above them, 2 NaN above all; 0 a number.
    fn class(&self) -> i8 {
        match self.0 {
            Repr::Small(0, SPECIAL) => 2,
            Repr::Small(m, SPECIAL) if m > 0 => 1,
            Repr::Small(_, SPECIAL) => -1,
            _ => 0,
        }
    }

    /// Whether it has no digits after the point (numeric_is_integral: an infinity is, NaN isn't).
    fn is_integral(&self) -> bool {
        match &self.0 {
            Repr::Small(0, SPECIAL) => false,
            Repr::Small(_, SPECIAL) => true,
            _ => {
                let v = self.var();
                v.digits.is_empty() || v.digits.len() as i32 <= v.weight + 1
            }
        }
    }

    /// The base-10000 weight of the first non-zero digit, and that digit (0 and 0 for zero): what
    /// select_div_scale chooses a quotient's scale by.
    fn head(&self) -> (i32, i32) {
        match &self.0 {
            Repr::Small(m, s) => {
                if *m == 0 {
                    return (0, 0);
                }
                let dw = digit_count(*m) as i32 - 1 - *s as i32;
                let w = dw.div_euclid(var::DEC_DIGITS);
                // the first group of four is |m| / 10^(4w + s) (at most three places short of a whole one)
                let e = var::DEC_DIGITS * w + *s as i32;
                let u = m.unsigned_abs();
                let first = if e >= 0 { u / 10u128.pow(e as u32) } else { u * 10u128.pow((-e) as u32) };
                (w, first as i32)
            }
            Repr::Big(v) => var::head(v),
        }
    }

    /// The decimal weight of the first non-zero digit (10^dweight); 0 for zero.
    pub fn dweight(&self) -> i64 {
        match &self.0 {
            Repr::Small(0, _) => 0,
            Repr::Small(m, s) => i64::from(digit_count(*m)) - 1 - i64::from(*s),
            Repr::Big(v) => i64::from(v.dweight()),
        }
    }

    /// The same value at `scale` digits after the point, rounding half away from zero.
    pub fn rescale(&self, scale: u32) -> R<Dec> {
        if self.is_special() {
            return Ok(self.clone());
        }
        if let Some((m, s)) = self.small()
            && scale <= MAX_SCALE
            && let Some(m) = rescale_small(m, s, scale)
        {
            return Ok(Dec(Repr::Small(m, scale)));
        }
        let mut v = self.var().into_owned();
        v.round(i32::try_from(scale).map_err(|_| OVERFLOW)?);
        Dec::from_var(v)
    }

    pub fn add(a: impl Borrow<Dec>, b: impl Borrow<Dec>) -> R<Dec> {
        let (a, b) = (a.borrow(), b.borrow());
        if a.is_special() || b.is_special() {
            return Ok(match (a.class(), b.class()) {
                (2, _) | (_, 2) => Dec::NAN,
                (x, y) if x != 0 && y != 0 => {
                    if x == y {
                        a.clone()
                    } else {
                        Dec::NAN
                    }
                }
                (0, _) => b.clone(),
                _ => a.clone(),
            });
        }
        if let (Some((x, xs)), Some((y, ys))) = (a.small(), b.small())
            && let Some((x, y, s)) = align(x, xs, y, ys)
            && let Some(m) = x.checked_add(y)
        {
            return Ok(Dec(Repr::Small(m, s)));
        }
        Dec::from_var(var::add(&a.var(), &b.var()))
    }

    pub fn sub(a: impl Borrow<Dec>, b: impl Borrow<Dec>) -> R<Dec> {
        let (a, b) = (a.borrow(), b.borrow());
        if a.is_special() || b.is_special() {
            return Dec::add(a, b.neg()?);
        }
        if let (Some((x, xs)), Some((y, ys))) = (a.small(), b.small())
            && let Some((x, y, s)) = align(x, xs, y, ys)
            && let Some(m) = x.checked_sub(y)
        {
            return Ok(Dec(Repr::Small(m, s)));
        }
        Dec::from_var(var::sub(&a.var(), &b.var()))
    }

    /// The exact product, at the sum of the scales (rounded only past 16383 places).
    pub fn mul(a: impl Borrow<Dec>, b: impl Borrow<Dec>) -> R<Dec> {
        let (a, b) = (a.borrow(), b.borrow());
        if a.is_special() || b.is_special() {
            let (x, y) = (a.signum(), b.signum());
            return Ok(if a.is_nan() || b.is_nan() || x == 0 || y == 0 {
                Dec::NAN
            } else if x * y > 0 {
                Dec::INF
            } else {
                Dec::NEG_INF
            });
        }
        if let (Some((x, xs)), Some((y, ys))) = (a.small(), b.small())
            && xs + ys <= MAX_SCALE
            && let Some(m) = x.checked_mul(y)
        {
            return Ok(Dec(Repr::Small(m, xs + ys)));
        }
        let (x, y) = (a.var(), b.var());
        let mut r = var::mul(&x, &y, x.dscale + y.dscale);
        if r.dscale > var::DSCALE_MAX {
            r.round(var::DSCALE_MAX);
        }
        Dec::from_var(r)
    }

    /// NaN and the infinities as numeric_div and numeric_div_trunc have them: None for two finite
    /// numbers. `zero`: what a finite number over an infinity gives.
    fn special_quotient(a: &Dec, b: &Dec) -> Option<R<Dec>> {
        if !a.is_special() && !b.is_special() {
            return None;
        }
        Some(if a.is_nan() || b.is_nan() {
            Ok(Dec::NAN)
        } else if a.is_special() {
            if b.is_special() {
                Ok(Dec::NAN)
            } else if b.is_zero() {
                Err(var::DIV_ZERO)
            } else if a.signum() * b.signum() > 0 {
                Ok(Dec::INF)
            } else {
                Ok(Dec::NEG_INF)
            }
        } else {
            // (POSIX would underflow; numeric has no underflow, so a plain zero)
            Ok(Dec::int(0))
        })
    }

    /// `a / b` at Postgres's scale for a quotient (select_div_scale: enough for 16 significant
    /// digits, and no less than either side's scale), rounded half away from zero.
    pub fn div(a: impl Borrow<Dec>, b: impl Borrow<Dec>) -> R<Dec> {
        let (a, b) = (a.borrow(), b.borrow());
        if let Some(r) = Dec::special_quotient(a, b) {
            return r;
        }
        if b.is_zero() {
            return Err(var::DIV_ZERO);
        }
        let (w1, f1) = a.head();
        let (w2, f2) = b.head();
        let rscale = var::div_scale_of(w1, f1, a.scale() as i32, w2, f2, b.scale() as i32);
        Dec::div_at(a, b, rscale)
    }

    /// `a / b` rounded (half away from zero) to `scale` digits.
    pub fn div_at(a: impl Borrow<Dec>, b: impl Borrow<Dec>, scale: i32) -> R<Dec> {
        let (a, b) = (a.borrow(), b.borrow());
        if let Some(r) = Dec::special_quotient(a, b) {
            return r;
        }
        if b.is_zero() {
            return Err(var::DIV_ZERO);
        }
        if let (Some((x, xs)), Some((y, ys))) = (a.small(), b.small())
            && (0..=MAX_SCALE as i32).contains(&scale)
            && let Some(m) = div_small(x, xs, y, ys, scale as u32)
        {
            return Ok(Dec(Repr::Small(m, scale as u32)));
        }
        Dec::from_var(var::div(&a.var(), &b.var(), scale, true)?)
    }

    /// numeric_div_trunc (`div(a, b)`): the quotient cut toward zero to a whole number.
    pub fn div_trunc(a: impl Borrow<Dec>, b: impl Borrow<Dec>) -> R<Dec> {
        let (a, b) = (a.borrow(), b.borrow());
        if let Some(r) = Dec::special_quotient(a, b) {
            return r;
        }
        if b.is_zero() {
            return Err(var::DIV_ZERO);
        }
        if let (Some((x, xs)), Some((y, ys))) = (a.small(), b.small())
            && let Some((x, y, _)) = align(x, xs, y, ys)
            && let Some(q) = x.checked_div(y)
        {
            return Ok(Dec(Repr::Small(q, 0)));
        }
        Dec::from_var(var::div(&a.var(), &b.var(), 0, false)?)
    }

    /// The square root at `scale` digits, rounded to nearest (Postgres's `sqrt_var` at that scale).
    pub fn sqrt_at(&self, scale: u32) -> R<Dec> {
        if self.is_special() {
            return if self.signum() < 0 { Err(var::SQRT_NEG) } else { Ok(self.clone()) };
        }
        Dec::from_var(var::sqrt(&self.var(), i32::try_from(scale).map_err(|_| OVERFLOW)?)?)
    }

    /// numeric_sqrt: at least 16 significant digits, and no fewer places than the argument has.
    pub fn sqrt(&self) -> R<Dec> {
        if self.is_special() {
            return self.sqrt_at(0);
        }
        let v = self.var();
        let sweight = v.weight * var::DEC_DIGITS / 2 + 1;
        let rscale = (var::MIN_SIG_DIGITS - sweight).max(v.dscale).clamp(0, var::MAX_DISPLAY_SCALE);
        Dec::from_var(var::sqrt(&v, rscale)?)
    }

    /// numeric_mod: x - trunc(x / y) * y, at the larger scale.
    pub fn rem(a: impl Borrow<Dec>, b: impl Borrow<Dec>) -> R<Dec> {
        let (a, b) = (a.borrow(), b.borrow());
        if a.is_special() || b.is_special() {
            if !b.is_special() && b.is_zero() && !a.is_nan() {
                return Err(var::DIV_ZERO);
            }
            return Ok(if a.is_special() || b.is_nan() { Dec::NAN } else { a.clone() });
        }
        if b.is_zero() {
            return Err(var::DIV_ZERO);
        }
        if let (Some((x, xs)), Some((y, ys))) = (a.small(), b.small())
            && let Some((x, y, s)) = align(x, xs, y, ys)
        {
            return Ok(Dec(Repr::Small(x.checked_rem(y).unwrap_or(0), s))); // i128::MIN % -1 is 0, not a panic
        }
        Dec::from_var(var::modulo(&a.var(), &b.var())?)
    }

    pub fn neg(&self) -> R<Dec> {
        match &self.0 {
            Repr::Small(m, SPECIAL) => Ok(Dec(Repr::Small(-m, SPECIAL))),
            Repr::Small(m, s) if *m != i128::MIN => Ok(Dec(Repr::Small(-m, *s))),
            _ => {
                let mut v = self.var().into_owned();
                v.neg = !v.neg && !v.is_zero();
                Dec::from_var(v)
            }
        }
    }

    /// |x| (an infinity's is Infinity, NaN's NaN).
    pub fn abs(&self) -> Dec {
        match &self.0 {
            Repr::Small(m, SPECIAL) => Dec(Repr::Small(m.abs(), SPECIAL)),
            Repr::Small(m, s) if *m != i128::MIN => Dec(Repr::Small(m.abs(), *s)),
            _ => {
                let mut v = self.var().into_owned();
                v.neg = false;
                Dec::from_var(v).unwrap_or_else(|_| self.clone())
            }
        }
    }

    pub fn cmp(a: impl Borrow<Dec>, b: impl Borrow<Dec>) -> Ordering {
        let (a, b) = (a.borrow(), b.borrow());
        if a.is_special() || b.is_special() {
            return a.class().cmp(&b.class());
        }
        if let (Some((x, xs)), Some((y, ys))) = (a.small(), b.small())
            && let Some((x, y, _)) = align(x, xs, y, ys)
        {
            return x.cmp(&y);
        }
        var::ordering(var::cmp_var(&a.var(), &b.var()))
    }

    /// The nearest double. A mantissa and a power of ten that a double holds exactly need one
    /// division; longer ones go through the text, so the rounding is done once, as Postgres's does.
    pub fn to_f64(&self) -> f64 {
        match self.0 {
            Repr::Small(0, SPECIAL) => f64::NAN,
            Repr::Small(m, SPECIAL) if m > 0 => f64::INFINITY,
            Repr::Small(_, SPECIAL) => f64::NEG_INFINITY,
            Repr::Small(m, s) if m.unsigned_abs() < (1 << 53) && s <= 22 => m as f64 / 10f64.powi(s as i32),
            _ => self.to_string().parse().unwrap_or(f64::NAN),
        }
    }

    /// The nearest integer, half away from zero, if it fits.
    pub fn to_i64(&self) -> Option<i64> {
        match &self.0 {
            Repr::Small(_, SPECIAL) => None,
            Repr::Small(m, s) => i64::try_from(rescale_small(*m, *s, 0)?).ok(),
            Repr::Big(v) => v.to_i64(),
        }
    }

    /// The fewest places that show it exactly (numeric's min_scale); 0 for NaN and the infinities.
    pub fn min_scale(&self) -> u32 {
        match &self.0 {
            Repr::Small(_, SPECIAL) | Repr::Small(0, _) => 0,
            Repr::Small(m, s) => {
                let (mut m, mut s) = (*m, *s);
                while s > 0 && m % 10 == 0 {
                    m /= 10;
                    s -= 1;
                }
                s
            }
            Repr::Big(v) => v.min_scale() as u32,
        }
    }

    /// Without trailing zeros after the point (numeric's trim_scale): equal values, one form.
    pub fn normal(&self) -> Dec {
        match &self.0 {
            Repr::Small(_, SPECIAL) => self.clone(),
            Repr::Small(m, s) => {
                let (mut m, mut s) = (*m, *s);
                while s > 0 && m % 10 == 0 {
                    m /= 10;
                    s -= 1;
                }
                Dec(Repr::Small(m, s))
            }
            Repr::Big(v) => {
                let mut v = Var::clone(v);
                v.dscale = v.min_scale();
                Dec::from_var(v).unwrap_or_else(|_| self.clone())
            }
        }
    }

    pub fn parse(s: &str) -> Option<Dec> {
        Dec::parse_input(s).ok()
    }

    /// A numeric's text as Postgres's numeric_in reads it: spaces around it; NaN; a sign, then
    /// Infinity or inf, or digits with an optional point and exponent, or (Postgres 16) 0x, 0o or 0b
    /// and digits of that base; single underscores between digits (`1_000_000`). The error says
    /// whether the value is too big to hold ("value overflows numeric format") rather than not a
    /// number at all.
    pub fn parse_input(s: &str) -> Result<Dec, bool> {
        const SYNTAX: bool = false;
        const TOO_BIG: bool = true;
        let lower = s.trim_matches(|c: char| c.is_ascii_whitespace()).to_ascii_lowercase();
        if lower == "nan" {
            return Ok(Dec::NAN);
        }
        let (neg, body) = match lower.strip_prefix('-') {
            Some(b) => (true, b),
            None => (false, lower.strip_prefix('+').unwrap_or(&lower)),
        };
        if body == "inf" || body == "infinity" {
            return Ok(if neg { Dec::NEG_INF } else { Dec::INF });
        }
        let b = body.as_bytes();
        let digit_at = |i: usize, radix: u32| b.get(i).is_some_and(|c| (*c as char).is_digit(radix));
        // 0x1F, 0o17, 0b101: a whole number in that base
        if b.len() > 2 && b[0] == b'0' && matches!(b[1], b'x' | b'o' | b'b') {
            let radix = match b[1] {
                b'x' => 16,
                b'o' => 8,
                _ => 2,
            };
            let mut ds: Vec<u32> = vec![];
            let mut i = 2;
            while i < b.len() {
                if b[i] == b'_' {
                    i += 1;
                    if !digit_at(i, radix) {
                        return Err(SYNTAX);
                    }
                    continue;
                }
                ds.push((b[i] as char).to_digit(radix).ok_or(SYNTAX)?);
                i += 1;
            }
            if ds.is_empty() {
                return Err(SYNTAX);
            }
            return radix_value(neg, &ds, radix).ok_or(TOO_BIG);
        }
        if !(digit_at(0, 10) || b.first() == Some(&b'.')) {
            return Err(SYNTAX);
        }
        let (mut digits, mut frac, mut point, mut i) = (String::new(), 0i64, false, 0);
        while i < b.len() {
            match b[i] {
                c if c.is_ascii_digit() => {
                    digits.push(c as char);
                    frac += i64::from(point);
                    i += 1;
                }
                b'.' if !point => {
                    point = true;
                    i += 1;
                    if b.get(i) == Some(&b'_') {
                        return Err(SYNTAX);
                    }
                }
                b'.' => return Err(SYNTAX),
                b'_' => {
                    i += 1;
                    if !digit_at(i, 10) {
                        return Err(SYNTAX);
                    }
                }
                _ => break,
            }
        }
        if digits.is_empty() {
            return Err(SYNTAX);
        }
        let mut exp: i64 = 0;
        if b.get(i) == Some(&b'e') {
            i += 1;
            let eneg = match b.get(i) {
                Some(b'+') => {
                    i += 1;
                    false
                }
                Some(b'-') => {
                    i += 1;
                    true
                }
                _ => false,
            };
            if !digit_at(i, 10) {
                return Err(SYNTAX);
            }
            while i < b.len() {
                match b[i] {
                    c if c.is_ascii_digit() => {
                        exp = exp * 10 + i64::from(c - b'0');
                        if exp > i64::from(i32::MAX / 2) {
                            return Err(TOO_BIG);
                        }
                        i += 1;
                    }
                    b'_' => {
                        i += 1;
                        if !digit_at(i, 10) {
                            return Err(SYNTAX);
                        }
                    }
                    _ => break,
                }
            }
            if eneg {
                exp = -exp;
            }
        }
        if i != b.len() {
            return Err(SYNTAX);
        }
        // the value is digits / 10^scale, shown with max(scale, 0) places
        let scale = frac - exp;
        let digits = digits.trim_start_matches('0');
        let dscale = scale.max(0);
        if dscale > i64::from(var::DSCALE_MAX) {
            return Err(TOO_BIG);
        }
        // what 38 digits at at most 38 places hold
        if digits.len() as i64 + (-scale).max(0) <= i64::from(MAX_SCALE) && dscale <= i64::from(MAX_SCALE) {
            let mut m: i128 = if digits.is_empty() { 0 } else { digits.parse().map_err(|_| TOO_BIG)? };
            if scale < 0 {
                m *= pow10((-scale) as u32).ok_or(TOO_BIG)?;
            }
            return Ok(Dec(Repr::Small(if neg { -m } else { m }, dscale as u32)));
        }
        let dweight = digits.len() as i64 - 1 - scale;
        if !digits.is_empty() && dweight >= i64::from(var::DEC_DIGITS) * i64::from(var::WEIGHT_MAX + 1) {
            return Err(TOO_BIG);
        }
        let dec: Vec<u8> = digits.bytes().map(|c| c - b'0').collect();
        let dweight = if digits.is_empty() { 0 } else { dweight as i32 };
        Dec::from_var(Var::from_decimal(neg, &dec, dweight, dscale as i32)).map_err(|_| TOO_BIG)
    }

    /// A float as Postgres converts it: through its shortest text (0.1 is 0.1).
    pub fn from_f64(f: f64) -> Option<Dec> {
        if f.is_nan() {
            return Some(Dec::NAN);
        }
        if f.is_infinite() {
            return Some(if f > 0.0 { Dec::INF } else { Dec::NEG_INF });
        }
        Dec::parse(&format!("{f}"))
    }

    /// numeric_round: half away from zero to `scale` digits after the point; a negative scale
    /// rounds to the left of it (-3: to thousands), leaving no digits after it.
    pub fn round_to(&self, scale: i32) -> R<Dec> {
        if self.is_special() {
            return Ok(self.clone());
        }
        let scale = scale.clamp(-(var::WEIGHT_MAX + 1) * var::DEC_DIGITS - 1, var::DSCALE_MAX);
        if scale >= 0 {
            return self.rescale(scale as u32);
        }
        let mut v = self.var().into_owned();
        v.round(scale);
        v.dscale = 0;
        Dec::from_var(v)
    }

    /// numeric_trunc: cut toward zero at `scale` digits after the point (a negative scale cuts to
    /// the left of it).
    pub fn trunc_to(&self, scale: i32) -> R<Dec> {
        if self.is_special() {
            return Ok(self.clone());
        }
        let scale = scale.clamp(-(var::WEIGHT_MAX + 1) * var::DEC_DIGITS, var::DSCALE_MAX);
        if let Some((m, s)) = self.small()
            && (0..=MAX_SCALE as i32).contains(&scale)
        {
            let to = scale as u32;
            let m = if to >= s { m.checked_mul(pow10(to - s).unwrap_or(0)) } else { pow10(s - to).map(|p| m / p) };
            if let Some(m) = m {
                return Ok(Dec(Repr::Small(m, to)));
            }
        }
        let mut v = self.var().into_owned();
        v.trunc(scale);
        v.dscale = v.dscale.max(0);
        Dec::from_var(v)
    }

    /// numeric_ceil.
    pub fn ceil(&self) -> R<Dec> {
        self.whole(true)
    }

    /// numeric_floor.
    pub fn floor(&self) -> R<Dec> {
        self.whole(false)
    }

    fn whole(&self, up: bool) -> R<Dec> {
        if self.is_special() {
            return Ok(self.clone());
        }
        if let Some((m, s)) = self.small() {
            let p = pow10(s).unwrap_or(1);
            let (q, r) = (m / p, m % p);
            let q = match (up, r.signum()) {
                (true, 1) => q + 1,
                (false, -1) => q - 1,
                _ => q,
            };
            return Ok(Dec(Repr::Small(q, 0)));
        }
        let v = self.var();
        Dec::from_var(if up { var::ceil(&v) } else { var::floor(&v) })
    }

    /// Fits `numeric(precision, scale)` as Postgres's apply_typmod: rounded to the scale (see
    /// `round_to`), then at most `precision - scale` digits before the point (which may be none,
    /// or fewer than none: numeric(3, 6) holds less than 10^-3). NaN fits any; the error is the
    /// "numeric field overflow" DETAIL.
    pub fn fit(&self, precision: u32, scale: i32) -> Result<Dec, String> {
        if self.is_special() {
            return if self.is_nan() {
                Ok(self.clone())
            } else {
                Err(format!("A field with precision {precision}, scale {scale} cannot hold an infinite value."))
            };
        }
        let max = i64::from(precision) - i64::from(scale);
        let too_big = || {
            let bound = if max == 0 { "1".to_string() } else { format!("10^{max}") };
            format!(
                "A field with precision {precision}, scale {scale} must round to an absolute value less than {bound}."
            )
        };
        let d = self.round_to(scale).map_err(|_| too_big())?;
        if !d.is_zero() && d.dweight() + 1 > max {
            return Err(too_big());
        }
        Ok(d)
    }

    // ---- what is kept: rows, keys, hashes and the binary protocol ----

    /// The value as a row keeps it: its mantissa's digits (with a sign) and its scale, which
    /// `from_parts` reads back. A small numeric's are those of its 128-bit form, as they always were.
    pub fn parts(&self) -> (String, u32) {
        match &self.0 {
            Repr::Small(m, s) => (m.to_string(), *s),
            Repr::Big(v) => (format!("{}{}", if v.neg { "-" } else { "" }, v.mantissa()), v.dscale as u32),
        }
    }

    /// What `parts` gave: None if it isn't that.
    pub fn from_parts(m: &str, scale: u32) -> Option<Dec> {
        if let Ok(m) = m.parse::<i128>() {
            return Some(Dec::new(m, scale));
        }
        let (neg, digits) = match m.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, m),
        };
        if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) || scale == SPECIAL {
            return None;
        }
        let digits = digits.trim_start_matches('0');
        let dec: Vec<u8> = digits.bytes().map(|c| c - b'0').collect();
        let scale = i32::try_from(scale).ok()?;
        let dweight = i32::try_from(dec.len()).ok()? - 1 - scale;
        Dec::from_var(Var::from_decimal(neg, &dec, dweight, scale)).ok()
    }

    /// Appends the value in a form whose text order is numeric order (an index entry's, a key's):
    /// the whole part (rounded down) as 32 hex digits of its 128 bits, its sign bit flipped, then
    /// the fraction's digits without the zeros that end it, then \0 (below every digit, so 0.5 <
    /// 0.51). A whole part past 128 bits: above every one that fits, 32 f's, ':', its digit count
    /// in 6 digits and its digits; below every one, '/', 999999 less its digit count, and each
    /// digit of its magnitude as 9 less it. NaN and the infinities (which Postgres orders, an index holds an
    /// entry for): `-` below every number (whose whole part starts with `/` or a hex digit), `g` above them, `h`
    /// for NaN above that, then \0.
    pub fn order_key(&self, out: &mut String) -> Option<()> {
        use std::fmt::Write;
        if self.is_special() {
            out.push(match self.class() {
                -1 => '-',
                1 => 'g',
                _ => 'h',
            });
            out.push('\0');
            return Some(());
        }
        let d = self.normal();
        if let Some((m, s)) = d.small() {
            let unit = pow10(s)?;
            write!(out, "{:032x}", (m.div_euclid(unit) as u128) ^ (1 << 127)).ok()?;
            if s > 0 {
                write!(out, "{:0>width$}", m.rem_euclid(unit), width = s as usize).ok()?;
            }
            out.push('\0');
            return Some(());
        }
        let v = d.var();
        let whole = var::floor(&v);
        let frac = var::sub(&v, &whole);
        let frac = frac.to_text();
        let frac = frac.split_once('.').map_or("", |(_, f)| f);
        match whole.to_small() {
            Some((w, _)) => write!(out, "{:032x}", (w as u128) ^ (1 << 127)).ok()?,
            None => {
                let digits = whole.mantissa();
                if whole.neg {
                    let inverted: String = digits.bytes().map(|c| (b'9' - c + b'0') as char).collect();
                    write!(out, "/{:06}{inverted}", 999_999 - digits.len()).ok()?;
                } else {
                    write!(out, "{}:{:06}{digits}", "f".repeat(32), digits.len()).ok()?;
                }
            }
        }
        out.push_str(frac);
        out.push('\0');
        Some(())
    }

    /// The value `order_key` wrote at the start of `s`, and how many bytes it took.
    pub fn from_order_key(s: &str) -> Option<(Dec, usize)> {
        if let Some(special) = s.get(..2).and_then(|p| match p {
            "-\0" => Some(Dec::NEG_INF),
            "g\0" => Some(Dec::INF),
            "h\0" => Some(Dec::NAN),
            _ => None,
        }) {
            return Some((special, 2));
        }
        let (whole, at) = if let Some(rest) = s.strip_prefix('/') {
            let n = 999_999 - rest.get(..6)?.parse::<usize>().ok()?;
            let inverted = rest.get(6..6 + n)?;
            let digits: String = inverted.bytes().map(|c| (b'9' - c + b'0') as char).collect();
            (Dec::from_parts(&format!("-{digits}"), 0)?, 1 + 6 + n)
        } else if s.get(32..33) == Some(":") && s.get(..32)?.bytes().all(|c| c == b'f') {
            let n = s.get(33..39)?.parse::<usize>().ok()?;
            (Dec::from_parts(s.get(39..39 + n)?, 0)?, 39 + n)
        } else {
            let whole = (u128::from_str_radix(s.get(..32)?, 16).ok()? ^ (1 << 127)) as i128;
            (Dec::new(whole, 0), 32)
        };
        let end = s.get(at..)?.find('\0')?;
        let frac = &s[at..at + end];
        let consumed = at + end + 1;
        if frac.is_empty() {
            return Some((whole, consumed));
        }
        if !frac.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let frac = Dec::from_parts(frac, u32::try_from(frac.len()).ok()?)?;
        Some((Dec::add(whole, frac).ok()?, consumed))
    }

    /// The base-10000 digits Postgres stores (no zeros at either end) and the first one's weight;
    /// None for NaN and the infinities.
    pub fn groups(&self) -> Option<(Vec<i16>, i32)> {
        if self.is_special() {
            return None;
        }
        let (digits, weight) = self.var().groups();
        Some((digits.into_iter().map(|d| d as i16).collect(), weight))
    }

    /// A numeric from Postgres's stored form: base-10000 digits, the first one's weight, the sign
    /// and the display scale.
    pub fn from_groups(digits: &[i16], weight: i32, neg: bool, dscale: u32) -> Option<Dec> {
        if digits.iter().any(|&d| !(0..10000).contains(&d)) {
            return None;
        }
        let v = Var {
            weight,
            neg,
            dscale: i32::try_from(dscale).ok()?,
            digits: digits.iter().map(|&d| i32::from(d)).collect(),
        };
        Dec::from_var(v).ok()
    }
}

// ---- ln, log, exp, power and the rest of numeric's functions ----

impl Dec {
    /// numeric_ln: at least 16 significant digits, and no fewer places than the argument.
    pub fn ln(&self) -> R<Dec> {
        if self.is_special() {
            return if self.signum() < 0 { Err(var::LOG_NEG) } else { Ok(self.clone()) };
        }
        let v = self.var();
        let rscale = (var::MIN_SIG_DIGITS - var::ln_dweight(&v)).max(v.dscale).clamp(0, var::MAX_DISPLAY_SCALE);
        Dec::from_var(var::ln(&v, rscale)?)
    }

    /// numeric_log: log of `x` to `base`, its scale from both logarithms' estimated weights.
    pub fn log(base: impl Borrow<Dec>, x: impl Borrow<Dec>) -> R<Dec> {
        let (base, x) = (base.borrow(), x.borrow());
        if base.is_special() || x.is_special() {
            if base.is_nan() || x.is_nan() {
                return Ok(Dec::NAN);
            }
            if base.signum() < 0 || x.signum() < 0 {
                return Err(var::LOG_NEG);
            }
            if base.signum() == 0 || x.signum() == 0 {
                return Err(var::LOG_ZERO);
            }
            // log(Inf, Inf) is Inf / Inf; log(Inf, x) zero; log(b, Inf) Inf
            return Ok(match (base.is_special(), x.is_special()) {
                (true, true) => Dec::NAN,
                (true, false) => Dec::int(0),
                _ => Dec::INF,
            });
        }
        Dec::from_var(var::log(&base.var(), &x.var())?)
    }

    /// numeric_exp: at least 16 significant digits, and no fewer places than the argument.
    pub fn exp(&self) -> R<Dec> {
        if self.is_special() {
            return Ok(match self.class() {
                -1 => Dec::int(0),
                _ => self.clone(),
            });
        }
        let v = self.var();
        let val =
            (v.to_f64() * 0.434294481903252).clamp(-f64::from(var::MAX_RESULT_SCALE), f64::from(var::MAX_RESULT_SCALE));
        let rscale = (var::MIN_SIG_DIGITS - val as i32).max(v.dscale).clamp(0, var::MAX_DISPLAY_SCALE);
        Dec::from_var(var::exp(&v, rscale)?)
    }

    /// numeric_power: x ^ y, NaN and the infinities as POSIX's pow has them, and the scale
    /// power_var picks.
    pub fn power(x: impl Borrow<Dec>, y: impl Borrow<Dec>) -> R<Dec> {
        let (x, y) = (x.borrow(), y.borrow());
        let one = || Dec::int(1);
        let is_one = |d: &Dec| !d.is_special() && var::is_one(&d.var());
        if x.is_special() || y.is_special() {
            // NaN ^ 0 and 1 ^ NaN are 1; anything else with a NaN is NaN
            if x.is_nan() {
                return Ok(if !y.is_special() && y.is_zero() { one() } else { Dec::NAN });
            }
            if y.is_nan() {
                return Ok(if is_one(x) { one() } else { Dec::NAN });
            }
            let (s1, s2) = (x.signum(), y.signum());
            if s1 == 0 && s2 < 0 {
                return Err(ZERO_NEG_POWER);
            }
            if s1 < 0 && !y.is_integral() {
                return Err(var::NEG_POWER);
            }
            if is_one(x) || s2 == 0 {
                return Ok(one());
            }
            if s1 == 0 && s2 > 0 {
                return Ok(Dec::int(0));
            }
            if y.is_special() {
                // |x| against 1, and which infinity y is
                let abs_x_gt_one = if x.is_special() {
                    true
                } else {
                    let v = x.var();
                    if var::is_minus_one(&v) {
                        return Ok(one());
                    }
                    Dec::cmp(x.abs(), one()) == Ordering::Greater
                };
                return Ok(if abs_x_gt_one == (s2 > 0) { Dec::INF } else { Dec::int(0) });
            }
            if s1 > 0 {
                return Ok(if s2 > 0 { Dec::INF } else { Dec::int(0) });
            }
            // -Infinity: to an odd whole power -Infinity, to a negative one zero
            if s2 < 0 {
                return Ok(Dec::int(0));
            }
            let v = y.var();
            let n = v.digits.len() as i32;
            let odd = n > 0 && n == v.weight + 1 && v.digits[n as usize - 1] & 1 == 1;
            return Ok(if odd { Dec::NEG_INF } else { Dec::INF });
        }
        if x.signum() == 0 && y.signum() < 0 {
            return Err(ZERO_NEG_POWER);
        }
        Dec::from_var(var::power(&x.var(), &y.var())?)
    }

    /// numeric_fac: n! (an error past 32177!, which overflows numeric).
    pub fn factorial(n: i64) -> R<Dec> {
        if n < 0 {
            return Err(FACTORIAL_NEG);
        }
        if n <= 1 {
            return Ok(Dec::int(1));
        }
        if n > 32177 {
            return Err(OVERFLOW);
        }
        // n * (n - 1) * ... * 2, in 128 bits while it fits
        let mut m = i128::from(n);
        let mut k = n - 1;
        while k > 1 {
            match m.checked_mul(i128::from(k)) {
                Some(p) => m = p,
                None => break,
            }
            k -= 1;
        }
        let mut v = Var::from_i128(m);
        while k > 1 {
            v = var::mul(&v, &Var::from_i64(k), 0);
            k -= 1;
        }
        Dec::from_var(v)
    }

    /// numeric_gcd: of the values at the larger scale, at that scale; NaN with NaN or an infinity.
    pub fn gcd(a: impl Borrow<Dec>, b: impl Borrow<Dec>) -> R<Dec> {
        let (a, b) = (a.borrow(), b.borrow());
        if a.is_special() || b.is_special() {
            return Ok(Dec::NAN);
        }
        if let (Some((x, xs)), Some((y, ys))) = (a.small(), b.small())
            && let Some((p, q, s)) = align(x, xs, y, ys)
            && let Ok(g) = i128::try_from(gcd_u128(p.unsigned_abs(), q.unsigned_abs()))
        {
            return Ok(Dec(Repr::Small(g, s)));
        }
        Dec::from_var(var::gcd(&a.var(), &b.var())?)
    }

    /// numeric_lcm: |x / gcd(x, y) * y|, zero if either is, at the larger scale.
    pub fn lcm(a: impl Borrow<Dec>, b: impl Borrow<Dec>) -> R<Dec> {
        let (a, b) = (a.borrow(), b.borrow());
        if a.is_special() || b.is_special() {
            return Ok(Dec::NAN);
        }
        if let (Some((x, xs)), Some((y, ys))) = (a.small(), b.small())
            && let Some((p, q, s)) = align(x, xs, y, ys)
        {
            let g = gcd_u128(p.unsigned_abs(), q.unsigned_abs());
            let m = (p.unsigned_abs().checked_div(g)).and_then(|d| d.checked_mul(q.unsigned_abs())).or(if g == 0 {
                Some(0)
            } else {
                None
            });
            if let Some(m) = m.and_then(|m| i128::try_from(m).ok()) {
                return Ok(Dec(Repr::Small(m, s)));
            }
        }
        let (x, y) = (a.var(), b.var());
        let dscale = x.dscale.max(y.dscale);
        let mut r = if x.is_zero() || y.is_zero() {
            Var::zero(0)
        } else {
            let g = var::gcd(&x, &y)?;
            let q = var::div(&x, &g, 0, false)?;
            let mut r = var::mul(&y, &q, y.dscale);
            r.neg = false;
            r
        };
        r.dscale = dscale;
        Dec::from_var(r)
    }

    /// numeric_inc: x + 1.
    pub fn inc(&self) -> R<Dec> {
        Dec::add(self, Dec::int(1))
    }
}

fn gcd_u128(mut p: u128, mut q: u128) -> u128 {
    while q != 0 {
        (p, q) = (q, p % q);
    }
    p
}

/// m / 10^s as a `Var`.
fn small_var(m: i128, s: u32) -> Var {
    let mut buf = [0u8; 40];
    let mut u = m.unsigned_abs();
    let mut len = 0;
    while u > 0 {
        buf[len] = (u % 10) as u8;
        u /= 10;
        len += 1;
    }
    buf[..len].reverse();
    let s = i32::try_from(s).unwrap_or(i32::MAX);
    Var::from_decimal(m < 0, &buf[..len], len as i32 - 1 - s, s)
}

/// m at scale `s` as a mantissa at scale `to` (rounding half away from zero when that drops
/// digits), if it fits.
fn rescale_small(m: i128, s: u32, to: u32) -> Option<i128> {
    match to.cmp(&s) {
        Ordering::Equal => Some(m),
        Ordering::Greater => m.checked_mul(pow10(to - s)?),
        Ordering::Less => {
            let Some(d) = pow10(s - to) else { return Some(0) };
            let (q, r) = (m / d, m % d);
            let up = r.unsigned_abs() * 2 >= d.unsigned_abs();
            Some(if up { q + m.signum() } else { q })
        }
    }
}

/// x / y (each a mantissa at its scale) rounded half away from zero to `scale` digits, if it fits
/// 128 bits.
fn div_small(x: i128, xs: u32, y: i128, ys: u32, scale: u32) -> Option<i128> {
    // x/y = (x / 10^xs) / (y / 10^ys); at `scale` (+1 to round): x * 10^(scale + 1 + ys - xs) / y
    let shift = i64::from(scale) + 1 + i64::from(ys) - i64::from(xs);
    let (num, den) = if shift >= 0 {
        (x.checked_mul(pow10(shift as u32)?)?, y)
    } else {
        (x, y.checked_mul(pow10((-shift) as u32)?)?)
    };
    let q = num.checked_div(den)?; // i128::MIN / -1
    let r = q % 10;
    Some(q / 10 + if r.abs() >= 5 { q.signum() } else { 0 })
}

/// Digits of base `radix` (2, 8 or 16), most significant first, as a whole numeric
/// (set_var_from_non_decimal_integer_str); None past numeric's range.
fn radix_value(neg: bool, ds: &[u32], radix: u32) -> Option<Dec> {
    let mut m: Option<i128> = Some(0);
    for &d in ds {
        m = m.and_then(|m| m.checked_mul(i128::from(radix))?.checked_add(i128::from(d)));
    }
    if let Some(m) = m {
        return Some(Dec::new(if neg { -m } else { m }, 0));
    }
    // in chunks that fit 64 bits: acc * radix^k + chunk
    let mut acc = Var::zero(0);
    let (mut chunk, mut unit) = (0i64, 1i64);
    for &d in ds {
        if unit > i64::MAX / 16 {
            acc = var::add(&var::mul(&acc, &Var::from_i64(unit), 0), &Var::from_i64(chunk));
            (chunk, unit) = (0, 1);
            if acc.weight > var::WEIGHT_MAX {
                return None;
            }
        }
        chunk = chunk * i64::from(radix) + i64::from(d);
        unit *= i64::from(radix);
    }
    acc = var::add(&var::mul(&acc, &Var::from_i64(unit), 0), &Var::from_i64(chunk));
    acc.neg = neg && !acc.is_zero();
    Dec::from_var(acc).ok()
}

impl std::fmt::Display for Dec {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.0 {
            Repr::Small(0, SPECIAL) => f.write_str("NaN"),
            Repr::Small(m, SPECIAL) if *m > 0 => f.write_str("Infinity"),
            Repr::Small(_, SPECIAL) => f.write_str("-Infinity"),
            Repr::Small(m, s) => {
                let digits = m.unsigned_abs().to_string();
                let sign = if *m < 0 { "-" } else { "" };
                if *s == 0 {
                    return write!(f, "{sign}{digits}");
                }
                let s = *s as usize;
                let digits = format!("{digits:0>width$}", width = s + 1);
                let (int, frac) = digits.split_at(digits.len() - s);
                write!(f, "{sign}{int}.{frac}")
            }
            Repr::Big(v) => f.write_str(&v.to_text()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Dec {
        Dec::parse(s).unwrap()
    }

    /// Square roots at a scale, rounded to nearest: numbers Postgres's `sqrt` gives, and widths a
    /// 128-bit square would overflow.
    #[test]
    fn square_roots_at_a_scale() {
        assert_eq!(d("2").sqrt_at(16).unwrap().to_string(), "1.4142135623730950");
        assert_eq!(d("2").sqrt_at(18).unwrap().to_string(), "1.414213562373095049");
        assert_eq!(d("0").sqrt_at(4).unwrap().to_string(), "0.0000");
        assert_eq!(d("16").sqrt_at(0).unwrap().to_string(), "4");
        assert_eq!(d("0.25").sqrt_at(3).unwrap().to_string(), "0.500");
        assert_eq!(d("10").sqrt_at(0).unwrap().to_string(), "3");
        assert_eq!(d("12").sqrt_at(0).unwrap().to_string(), "3");
        assert_eq!(d("13").sqrt_at(0).unwrap().to_string(), "4"); // 3.6: up
        // 10^30 and 10^36 at 16 digits: past what m * 10^(2 * scale) holds in 128 bits
        assert_eq!(
            d("1000000000000000000000000000000").sqrt_at(16).unwrap().to_string(),
            "1000000000000000.0000000000000000"
        );
        assert_eq!(d("123456789012345678.123456789").sqrt_at(16).unwrap().to_string(), "351364182.8820144242043251");
        assert!(d("-1").sqrt_at(4).is_err());
    }

    #[test]
    fn exact_arithmetic_as_postgres() {
        assert_eq!(Dec::add(d("0.1"), d("0.2")).unwrap().to_string(), "0.3");
        assert_eq!(Dec::mul(d("1.50"), d("2")).unwrap().to_string(), "3.00");
        assert_eq!(Dec::div(d("1"), d("3")).unwrap().to_string(), "0.33333333333333333333");
        assert_eq!(Dec::div(d("10"), d("4")).unwrap().to_string(), "2.5000000000000000");
        assert_eq!(Dec::div(d("100"), d("7")).unwrap().to_string(), "14.2857142857142857");
        assert_eq!(Dec::div(d("2.00"), d("3")).unwrap().to_string(), "0.66666666666666666667");
        assert_eq!(Dec::div(d("1"), d("0.001")).unwrap().to_string(), "1000.0000000000000000");
        assert_eq!(Dec::div(d("0.001"), d("7")).unwrap().to_string(), "0.00014285714285714286");
        assert_eq!(d("-0.005").rescale(2).unwrap().to_string(), "-0.01");
        assert_eq!(d("1.5e3").to_string(), "1500");
        assert_eq!(d("12.3400").normal().to_string(), "12.34");
        assert_eq!(d(".5").to_string(), "0.5");
        assert_eq!(d("123.456").fit(5, 2).unwrap().to_string(), "123.46");
        assert!(d("1234.5").fit(5, 2).is_err());
        assert_eq!(Dec::from_f64(0.1).unwrap().to_string(), "0.1");
        assert_eq!(Dec::cmp(d("1.50"), d("1.5")), Ordering::Equal);
        assert!(Dec::parse("1.2.3").is_none() && Dec::parse("").is_none() && Dec::parse("x").is_none());
    }

    /// Past 38 digits: exact, in numeric.c's form, and printed as Postgres prints it.
    #[test]
    fn big_values_as_postgres() {
        let big = "123456789012345678901234567890123456789012345678901234567890";
        assert_eq!(d(big).to_string(), big);
        assert_eq!(d(&format!("-{big}.5")).to_string(), format!("-{big}.5"));
        assert_eq!(
            Dec::add(d(big), d("1")).unwrap().to_string(),
            "123456789012345678901234567890123456789012345678901234567891"
        );
        assert_eq!(
            Dec::mul(d("99999999999999999999"), d("99999999999999999999")).unwrap().to_string(),
            "9999999999999999999800000000000000000001"
        );
        // 2^128 sits just past a 128-bit mantissa: it and its neighbours compare and subtract exactly
        let two128 = d("340282366920938463463374607431768211456");
        assert_eq!(Dec::sub(two128.clone(), d("340282366920938463463374607431768211455")).unwrap(), Dec::int(1));
        assert_eq!(Dec::cmp(&two128, d("340282366920938463463374607431768211455")), Ordering::Greater);
        // one form per value: a result that fits 128 bits is the small one
        assert_eq!(Dec::sub(&two128, &two128).unwrap(), Dec::int(0));
        assert_eq!(
            d("0.00000000000000000000000000000000000000001").to_string(),
            "0.00000000000000000000000000000000000000001"
        );
        assert_eq!(d("1e-16383").scale(), 16383);
        assert_eq!(Dec::parse_input("1e-16384"), Err(true));
        assert_eq!(Dec::parse_input("1e131072"), Err(true));
        assert_eq!(d("9.9e131071").dweight(), 131071);
        assert_eq!(Dec::factorial(30).unwrap().to_string(), "265252859812191058636308480000000");
        assert_eq!(
            Dec::power(d("2"), d("200")).unwrap().to_string(),
            "1606938044258990275541962092341162602522202993782792835301376"
        );
        assert_eq!(d("0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF").to_string(), "43556142965880123323311949751266331066367");
        // numeric.c's ln and exp, digit for digit (numeric.out)
        assert_eq!(d("1").exp().unwrap().to_string(), "2.7182818284590452");
        assert_eq!(d("2").ln().unwrap().to_string(), "0.6931471805599453");
        assert_eq!(d("1.0000000000000000000000000000000000000000").exp().unwrap().to_string().len(), 42);
    }

    /// Rows and keys: what a row kept before still reads, and keys order as the values do.
    #[test]
    fn kept_forms() {
        for s in
            ["0", "-1.5", "12345.6789", "1e-50", "-123456789012345678901234567890123456789012345678901234567890.25"]
        {
            let v = d(s);
            let (m, sc) = v.parts();
            assert_eq!(Dec::from_parts(&m, sc).unwrap(), v, "{s}");
        }
        assert_eq!(Dec::from_parts("150", 2).unwrap().to_string(), "1.50");
        let vals = [
            "-1e60",
            "-1e59",
            "-170141183460469231731687303715884105729",
            "-2.5",
            "0",
            "0.5",
            "0.51",
            "1",
            "170141183460469231731687303715884105727.5",
            "170141183460469231731687303715884105728",
            "1e59",
            "1e60",
        ];
        let keys: Vec<String> = vals
            .iter()
            .map(|s| {
                let mut k = String::new();
                d(s).order_key(&mut k).unwrap();
                k
            })
            .collect();
        for w in keys.windows(2) {
            assert!(w[0] < w[1], "{w:?}");
        }
        for (s, k) in vals.iter().zip(&keys) {
            let (back, n) = Dec::from_order_key(k).unwrap();
            assert_eq!((Dec::cmp(&back, d(s)), n), (Ordering::Equal, k.len()), "{s}");
        }
    }
}
