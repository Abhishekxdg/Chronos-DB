//! The statistical aggregates: stddev and variance (and their `_pop` and `_samp` forms), corr,
//! covar_pop, covar_samp and regr_*. Postgres's own arithmetic, so the last digits agree: floats
//! accumulate as Postgres's `float8_accum` and `float8_regr_accum` do (Youngs-Cramer), and
//! integers and numerics through its numeric arithmetic (so the result is a numeric, at the scale
//! Postgres picks for a quotient).

use super::decimal::Dec;
use super::{Result, Type, Value, err};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Stat {
    VarPop,
    VarSamp,
    /// variance(x): var_samp by its own name (the column is called variance)
    Variance,
    StddevPop,
    StddevSamp,
    /// stddev(x): stddev_samp by its own name
    Stddev,
    CovarPop,
    CovarSamp,
    Corr,
    RegrCount,
    RegrAvgX,
    RegrAvgY,
    RegrSxx,
    RegrSyy,
    RegrSxy,
    RegrSlope,
    RegrIntercept,
    RegrR2,
}

impl Stat {
    /// The aggregate called `name`.
    pub(super) fn parse(name: &str) -> Option<Stat> {
        use Stat::*;
        Some(match name {
            "variance" => Variance,
            "var_samp" => VarSamp,
            "var_pop" => VarPop,
            "stddev" => Stddev,
            "stddev_samp" => StddevSamp,
            "stddev_pop" => StddevPop,
            "covar_pop" => CovarPop,
            "covar_samp" => CovarSamp,
            "corr" => Corr,
            "regr_count" => RegrCount,
            "regr_avgx" => RegrAvgX,
            "regr_avgy" => RegrAvgY,
            "regr_sxx" => RegrSxx,
            "regr_syy" => RegrSyy,
            "regr_sxy" => RegrSxy,
            "regr_slope" => RegrSlope,
            "regr_intercept" => RegrIntercept,
            "regr_r2" => RegrR2,
            _ => return None,
        })
    }

    /// How many arguments it takes: one value, or (dependent, independent).
    pub(super) fn arity(self) -> usize {
        if self.one_argument() { 1 } else { 2 }
    }

    fn one_argument(self) -> bool {
        matches!(
            self,
            Stat::VarPop | Stat::VarSamp | Stat::Variance | Stat::StddevPop | Stat::StddevSamp | Stat::Stddev
        )
    }

    /// The name Postgres gives its column.
    pub(super) fn name(self) -> &'static str {
        use Stat::*;
        match self {
            VarPop => "var_pop",
            VarSamp => "var_samp",
            Variance => "variance",
            StddevPop => "stddev_pop",
            StddevSamp => "stddev_samp",
            Stddev => "stddev",
            CovarPop => "covar_pop",
            CovarSamp => "covar_samp",
            Corr => "corr",
            RegrCount => "regr_count",
            RegrAvgX => "regr_avgx",
            RegrAvgY => "regr_avgy",
            RegrSxx => "regr_sxx",
            RegrSyy => "regr_syy",
            RegrSxy => "regr_sxy",
            RegrSlope => "regr_slope",
            RegrIntercept => "regr_intercept",
            RegrR2 => "regr_r2",
        }
    }
}

/// What the aggregate gives, from its arguments' types: numeric for integers and numerics in the
/// one-argument forms, a count for regr_count, a double precision otherwise. Anything that is not
/// a number is 42883, as in Postgres.
pub(super) fn result_type(st: Stat, args: &[Type]) -> Result<Type> {
    if args.iter().any(|t| !(t.integral() || matches!(t, Type::Numeric | Type::Float | Type::Float4))) {
        let given: Vec<String> = args.iter().map(|t| t.name().into_owned()).collect();
        return err("42883", format!("function {}({}) does not exist", st.name(), given.join(", ")));
    }
    Ok(match st {
        Stat::RegrCount => Type::BigInt,
        _ if st.one_argument() && args.first().is_some_and(|t| t.integral() || *t == Type::Numeric) => Type::Numeric,
        _ => Type::Float,
    })
}

fn float(v: &Value) -> Result<f64> {
    match super::coerce(v.clone(), Type::Float)? {
        Value::Float(f) => Ok(f),
        _ => err("22P02", "a number is needed"),
    }
}

/// The aggregate over `rows`, each the aggregate's arguments (never null: those rows are skipped).
pub(super) fn finish(st: Stat, rows: &[&Vec<Value>]) -> Result<Value> {
    if st.one_argument() {
        // integers and numerics: exact, as numeric
        if rows.iter().all(|r| matches!(r[0], Value::Int(_) | Value::Numeric(_))) {
            let mut vals = vec![];
            for r in rows {
                vals.push(match &r[0] {
                    Value::Int(n) => Dec::int(*n),
                    Value::Numeric(d) => d.clone(),
                    _ => unreachable!(),
                });
            }
            return numeric_variance(
                &vals,
                matches!(st, Stat::VarSamp | Stat::Variance | Stat::StddevSamp | Stat::Stddev),
                matches!(st, Stat::StddevPop | Stat::StddevSamp | Stat::Stddev),
            );
        }
        let xs = rows.iter().map(|r| float(&r[0])).collect::<Result<Vec<_>>>()?;
        return Ok(float_variance(&xs, st));
    }
    let pairs = rows.iter().map(|r| Ok((float(&r[0])?, float(&r[1])?))).collect::<Result<Vec<_>>>()?;
    Ok(regression(st, &pairs))
}

fn null_or(v: Option<f64>) -> Value {
    v.map_or(Value::Null, Value::Float)
}

/// float8_accum's running state over `xs`: N, Sx and Sxx (the sum of squares about the mean).
fn variance_state(xs: &[f64]) -> (f64, f64, f64) {
    let (mut n, mut sx, mut sxx) = (0.0f64, 0.0f64, 0.0f64);
    for &x in xs {
        let new_n = n + 1.0;
        let new_sx = sx + x;
        sxx = if n > 0.0 {
            let tmp = x * new_n - new_sx;
            sxx + tmp * tmp / (new_n * n)
        } else {
            0.0
        };
        // (an infinity in gives NaN out, as Postgres does)
        if !x.is_finite() {
            sxx = f64::NAN;
        }
        n = new_n;
        sx = new_sx;
    }
    (n, sx, sxx)
}

fn float_variance(xs: &[f64], st: Stat) -> Value {
    let (n, _, sxx) = variance_state(xs);
    let sample = matches!(st, Stat::VarSamp | Stat::Variance | Stat::StddevSamp | Stat::Stddev);
    let root = matches!(st, Stat::StddevPop | Stat::StddevSamp | Stat::Stddev);
    if n == 0.0 || (sample && n <= 1.0) {
        return Value::Null;
    }
    let var = sxx / if sample { n - 1.0 } else { n };
    Value::Float(if root { var.sqrt() } else { var })
}

/// float8_regr_accum's state over (y, x) pairs: N, Sx, Sxx, Sy, Syy, Sxy.
fn regression_state(pairs: &[(f64, f64)]) -> [f64; 6] {
    let [mut n, mut sx, mut sxx, mut sy, mut syy, mut sxy] = [0.0f64; 6];
    for &(y, x) in pairs {
        let new_n = n + 1.0;
        let (new_sx, new_sy) = (sx + x, sy + y);
        if n > 0.0 {
            let (tx, ty) = (x * new_n - new_sx, y * new_n - new_sy);
            let scale = 1.0 / (n * new_n);
            sxx += tx * tx * scale;
            sxy += tx * ty * scale;
            syy += ty * ty * scale;
        }
        if !x.is_finite() || !y.is_finite() {
            (sxx, sxy, syy) = (f64::NAN, f64::NAN, f64::NAN);
        }
        (n, sx, sy) = (new_n, new_sx, new_sy);
    }
    [n, sx, sxx, sy, syy, sxy]
}

fn regression(st: Stat, pairs: &[(f64, f64)]) -> Value {
    let [n, sx, sxx, sy, syy, sxy] = regression_state(pairs);
    if n == 0.0 {
        return if st == Stat::RegrCount { Value::Int(0) } else { Value::Null };
    }
    use Stat::*;
    match st {
        RegrCount => Value::Int(n as i64),
        RegrAvgX => Value::Float(sx / n),
        RegrAvgY => Value::Float(sy / n),
        RegrSxx => Value::Float(sxx),
        RegrSyy => Value::Float(syy),
        RegrSxy => Value::Float(sxy),
        CovarPop => Value::Float(sxy / n),
        CovarSamp => null_or((n >= 2.0).then(|| sxy / (n - 1.0))),
        Corr => {
            if sxx == 0.0 || syy == 0.0 {
                return Value::Null;
            }
            // (a rounding error may carry it just past one)
            Value::Float((sxy / (sxx * syy).sqrt()).clamp(-1.0, 1.0))
        }
        RegrSlope => null_or((sxx != 0.0).then(|| sxy / sxx)),
        RegrIntercept => null_or((sxx != 0.0).then(|| (sy - sx * sxy / sxx) / n)),
        RegrR2 => {
            if sxx == 0.0 {
                Value::Null
            } else if syy == 0.0 {
                Value::Float(1.0)
            } else {
                Value::Float((sxy * sxy) / (sxx * syy))
            }
        }
        VarPop | VarSamp | Variance | StddevPop | StddevSamp | Stddev => Value::Null,
    }
}

/// numeric_stddev_internal: (N * sum(x^2) - sum(x)^2) / (N * N) or / (N * (N - 1)), at the scale
/// Postgres picks for a quotient, and its square root at that scale for the standard deviation. NaN
/// or an infinity among the values gives NaN (they count toward N, as Postgres's do).
fn numeric_variance(vals: &[Dec], sample: bool, root: bool) -> Result<Value> {
    let n = vals.len() as i64;
    if n < if sample { 2 } else { 1 } {
        return Ok(Value::Null);
    }
    if vals.iter().any(Dec::is_special) {
        return Ok(Value::Numeric(Dec::NAN));
    }
    let overflow = |e: &str| super::SqlError { code: "22003", message: e.into(), info: None };
    // the sums at the largest scale given (and its square), as Postgres's accumulators keep them
    let scale = vals.iter().map(Dec::scale).max().unwrap_or(0);
    let (mut sum_x, mut sum_x2) = (Dec::new(0, scale), Dec::new(0, 2 * scale));
    for d in vals {
        sum_x = Dec::add(&sum_x, d).map_err(overflow)?;
        sum_x2 = Dec::add(&sum_x2, Dec::mul(d, d).map_err(overflow)?).map_err(overflow)?;
    }
    let n_sum_x2 = Dec::mul(Dec::int(n), &sum_x2).map_err(overflow)?;
    let sum_x_sq = Dec::mul(&sum_x, &sum_x).map_err(overflow)?;
    let numerator = Dec::sub(n_sum_x2, sum_x_sq).map_err(overflow)?;
    if numerator.signum() <= 0 {
        // no spread (or roundoff below none): Postgres's plain zero, without the quotient's scale
        return Ok(Value::Numeric(Dec::int(0)));
    }
    let denominator = Dec::int(n * if sample { n - 1 } else { n });
    let var = Dec::div(numerator, denominator).map_err(overflow)?;
    Ok(Value::Numeric(if root { var.sqrt_at(var.scale()).map_err(overflow)? } else { var }))
}

/// float8_accum (0), float8_regr_accum (1), float8_combine (2) and float8_regr_combine (3), the floats' transition
/// and combine functions called by name: a state array of N, Sx, Sxx (and Sy, Syy, Sxy for regr) taking a value (a
/// pair: y, then x) or another state, as Postgres's float.c works them out. The arguments are not null.
pub(super) fn transition(k: u8, a: &[Value]) -> Result<Value> {
    let names = ["float8_accum", "float8_regr_accum", "float8_combine", "float8_regr_combine"];
    let name = names[usize::from(k) % 4];
    let width = if k.is_multiple_of(2) { 3 } else { 6 };
    let state = |v: &Value| -> Result<Vec<f64>> {
        let xs = super::array_values(v.clone())?.unwrap_or_default();
        if xs.len() != width || xs.contains(&Value::Null) {
            return err("XX000", format!("{name}: expected {width}-element float8 array"));
        }
        xs.iter().map(float).collect()
    };
    fn overflow<T>() -> Result<T> {
        err("22003", "value out of range: overflow")
    }
    // float8_pl: an infinity from finite operands is an overflow
    let plus = |x: f64, y: f64| -> Result<f64> {
        let s = x + y;
        if s.is_infinite() && x.is_finite() && y.is_finite() {
            return overflow();
        }
        Ok(s)
    };
    let s = state(&a[0])?;
    let out: Vec<f64> = match k {
        0 => {
            let x = float(&a[1])?;
            let (n0, sx0) = (s[0], s[1]);
            let (n, sx) = (n0 + 1.0, sx0 + x);
            let mut sxx = s[2];
            if n0 > 0.0 {
                let tmp = x * n - sx;
                sxx += tmp * tmp / (n * n0);
                if sx.is_infinite() || sxx.is_infinite() {
                    if sx0.is_finite() && x.is_finite() {
                        return overflow();
                    }
                    sxx = f64::NAN;
                }
            } else if !x.is_finite() {
                sxx = f64::NAN;
            }
            vec![n, sx, sxx]
        }
        1 => {
            let (y, x) = (float(&a[1])?, float(&a[2])?);
            let n0 = s[0];
            let n = n0 + 1.0;
            let (sx, sy) = (s[1] + x, s[3] + y);
            let (mut sxx, mut syy, mut sxy) = (s[2], s[4], s[5]);
            if n0 > 0.0 {
                let (tx, ty) = (x * n - sx, y * n - sy);
                let scale = 1.0 / (n * n0);
                sxx += tx * tx * scale;
                syy += ty * ty * scale;
                sxy += tx * ty * scale;
                if [sx, sxx, sy, syy, sxy].iter().any(|v| v.is_infinite()) {
                    let (fx, fy) = (s[1].is_finite() && x.is_finite(), s[3].is_finite() && y.is_finite());
                    if ((sx.is_infinite() || sxx.is_infinite()) && fx)
                        || ((sy.is_infinite() || syy.is_infinite()) && fy)
                        || (sxy.is_infinite() && fx && fy)
                    {
                        return overflow();
                    }
                    for v in [&mut sxx, &mut syy, &mut sxy] {
                        if v.is_infinite() {
                            *v = f64::NAN;
                        }
                    }
                }
            } else {
                if !x.is_finite() {
                    (sxx, sxy) = (f64::NAN, f64::NAN);
                }
                if !y.is_finite() {
                    (syy, sxy) = (f64::NAN, f64::NAN);
                }
            }
            vec![n, sx, sxx, sy, syy, sxy]
        }
        _ => {
            let t = state(&a[1])?;
            let (n1, n2) = (s[0], t[0]);
            if n1 == 0.0 {
                t
            } else if n2 == 0.0 {
                s
            } else {
                let n = n1 + n2;
                // one pair of sums (Sx and Sxx at `at`) put together, and the difference of the means it used
                let part = |at: usize| -> Result<(f64, f64, f64)> {
                    let sum = plus(s[at], t[at])?;
                    let d = s[at] / n1 - t[at] / n2;
                    let sq = s[at + 1] + t[at + 1] + n1 * n2 * d * d / n;
                    if sq.is_infinite() && s[at + 1].is_finite() && t[at + 1].is_finite() {
                        return overflow();
                    }
                    Ok((sum, sq, d))
                };
                let (sx, sxx, dx) = part(1)?;
                if k == 2 {
                    vec![n, sx, sxx]
                } else {
                    let (sy, syy, dy) = part(3)?;
                    let sxy = s[5] + t[5] + n1 * n2 * dx * dy / n;
                    if sxy.is_infinite() && s[5].is_finite() && t[5].is_finite() {
                        return overflow();
                    }
                    vec![n, sx, sxx, sy, syy, sxy]
                }
            }
        }
    };
    Ok(Value::Text(super::array_literal(&out.into_iter().map(Value::Float).collect::<Vec<_>>())))
}
