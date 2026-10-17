//! The money type: Postgres's `cash_in` and `cash_out` (C locale, `$`), its arithmetic and comparisons, `cash_words`.
//! A money value is its text, `$1,234.56`; every operation reads its inputs from the text and writes text back.

use super::funcs::Scalar;
use super::{SqlError, Value, decimal, err};

type Result<T> = std::result::Result<T, SqlError>;

fn out_of_range(text: &str) -> SqlError {
    SqlError { code: "22003", message: format!("value \"{text}\" is out of range for type money"), info: None }
}

fn bad(text: &str) -> SqlError {
    SqlError { code: "22P02", message: format!("invalid input syntax for type money: \"{text}\""), info: None }
}

fn overflow() -> SqlError {
    SqlError { code: "22003", message: "money out of range".into(), info: None }
}

/// `$1,234.56`, `-$5`, `($1.00)`, `123.456` (rounded to cents) as a number of cents.
pub(super) fn parse(text: &str) -> Result<i64> {
    let s = text.trim();
    let (mut neg, mut rest) = (false, s);
    if let Some(r) = rest.strip_prefix('(') {
        neg = true;
        rest = r;
    }
    loop {
        if let Some(r) = rest.strip_prefix('-') {
            neg = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('+') {
            rest = r;
        } else if let Some(r) = rest.strip_prefix('$') {
            rest = r;
        } else {
            break;
        }
    }
    let (mut whole, mut cents, mut seen, mut fraction): (i128, i128, bool, Option<u32>) = (0, 0, false, None);
    let mut round_up = false;
    let mut chars = rest.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '0'..='9' => {
                seen = true;
                let d = i128::from(c.to_digit(10).unwrap_or(0));
                match fraction {
                    None => {
                        whole =
                            whole.checked_mul(10).and_then(|w| w.checked_add(d)).ok_or_else(|| out_of_range(text))?;
                    }
                    Some(n) if n < 2 => {
                        cents = cents * 10 + d;
                        fraction = Some(n + 1);
                    }
                    Some(2) => {
                        round_up = d >= 5;
                        fraction = Some(3);
                    }
                    _ => {}
                }
            }
            ',' if fraction.is_none() => {}
            '.' if fraction.is_none() => fraction = Some(0),
            _ => break,
        }
        chars.next();
    }
    let tail: String = chars.collect();
    let tail = tail.trim();
    match tail {
        "" => {}
        ")" if s.starts_with('(') => {}
        "-" => neg = true,
        _ => return Err(bad(text)),
    }
    if !seen {
        return Err(bad(text));
    }
    if fraction == Some(1) {
        cents *= 10;
    }
    let total = whole
        .checked_mul(100)
        .and_then(|w| w.checked_add(cents + i128::from(round_up)))
        .ok_or_else(|| out_of_range(text))?;
    let total = if neg { -total } else { total };
    i64::try_from(total).map_err(|_| out_of_range(text))
}

pub(super) fn format(cents: i64) -> String {
    let neg = cents < 0;
    let n = i128::from(cents).abs();
    let (dollars, c) = (n / 100, n % 100);
    let digits = dollars.to_string();
    let mut grouped = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{}${grouped}.{c:02}", if neg { "-" } else { "" })
}

/// A money operand as cents: its text, or a number given as money (an integer is dollars, as `1234::money` has it).
pub(super) fn cents_of(v: &Value) -> Result<i64> {
    match v {
        Value::Int(n) => n.checked_mul(100).ok_or_else(overflow),
        // (a numeric rounds to cents, half away from zero, from its own digits)
        Value::Numeric(d) => parse(&d.to_string()),
        Value::Float(f) => {
            let f = *f * 100.0;
            if f.is_nan() || f.abs() >= 9.223_372_036_854_776e18 {
                return Err(overflow());
            }
            Ok(f.round_ties_even() as i64)
        }
        v => parse(&v.to_text()),
    }
}

/// Money text from a number (an integer's dollars, a numeric rounded to cents), as `::money` reads it.
pub(super) fn from_number(v: &Value) -> Result<String> {
    cents_of(v).map(format)
}

pub(super) fn to_numeric(v: &Value) -> Result<Value> {
    let cents = parse(&v.to_text())?;
    let text = format!(
        "{}{}.{:02}",
        if cents < 0 { "-" } else { "" },
        (i128::from(cents).abs()) / 100,
        (i128::from(cents).abs()) % 100
    );
    decimal::Dec::parse(&text).map(Value::Numeric).ok_or_else(overflow)
}

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

fn below_thousand(n: u32, out: &mut String) {
    let (h, rest) = (n / 100, n % 100);
    if h > 0 {
        out.push_str(ONES[h as usize]);
        out.push_str(" hundred");
        if rest > 0 {
            out.push(' ');
        }
    }
    if rest >= 20 {
        out.push_str(TENS[(rest / 10) as usize]);
        if rest % 10 > 0 {
            out.push(' ');
            out.push_str(ONES[(rest % 10) as usize]);
        }
    } else if rest > 0 {
        out.push_str(ONES[rest as usize]);
    }
}

pub(super) fn words(cents: i64) -> String {
    let mut n = i128::from(cents).abs();
    let (c, mut dollars) = ((n % 100) as u32, n / 100);
    n = dollars;
    let mut out = String::new();
    if cents < 0 {
        out.push_str("minus ");
    }
    if n == 0 {
        out.push_str("zero");
    } else {
        const UNITS: [&str; 7] = ["", "thousand", "million", "billion", "trillion", "quadrillion", "quintillion"];
        let mut groups = vec![];
        while dollars > 0 {
            groups.push((dollars % 1000) as u32);
            dollars /= 1000;
        }
        let mut first = true;
        for (i, g) in groups.iter().enumerate().rev() {
            if *g == 0 {
                continue;
            }
            if !first {
                out.push(' ');
            }
            first = false;
            below_thousand(*g, &mut out);
            if i > 0 {
                out.push(' ');
                out.push_str(UNITS[i]);
            }
        }
    }
    out.push_str(if n == 1 { " dollar and " } else { " dollars and " });
    below_thousand(c, &mut out);
    if c == 0 {
        out.push_str("zero");
    }
    out.push_str(if c == 1 { " cent" } else { " cents" });
    let mut chars = out.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => out,
    }
}

/// The money functions and operators: `kind` 0 gives money, 1 a double (money over money), 2 a boolean.
pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    use Value::{Bool, Float, Null, Text};
    match f {
        Scalar::MoneyFn(k) => {
            if a.contains(&Null) {
                return Ok(Null);
            }
            if k == 3 {
                return to_numeric(&a[0]);
            }
            let x = cents_of(&a[0])?;
            Ok(match k {
                0 => Text(format(x.max(cents_of(&a[1])?))),
                1 => Text(format(x.min(cents_of(&a[1])?))),
                _ => Text(words(x)),
            })
        }
        Scalar::MoneyOp(kind) => {
            if a[..2].contains(&Null) {
                return Ok(Null);
            }
            let op = a[2].to_text();
            let (am, bm) = (matches!(a[3], Bool(true)), matches!(a[4], Bool(true)));
            if kind == 2 {
                let (x, y) = (cents_of(&a[0])?, cents_of(&a[1])?);
                return Ok(Bool(match op.as_str() {
                    "=" => x == y,
                    "<>" | "!=" => x != y,
                    "<" => x < y,
                    ">" => x > y,
                    "<=" => x <= y,
                    _ => x >= y,
                }));
            }
            let money = |v: &Value| cents_of(v);
            let r = match (op.as_str(), am, bm) {
                ("+", true, true) => money(&a[0])?.checked_add(money(&a[1])?).ok_or_else(overflow)?,
                ("-", true, true) => money(&a[0])?.checked_sub(money(&a[1])?).ok_or_else(overflow)?,
                ("/", true, true) => {
                    let (x, y) = (money(&a[0])? as f64, money(&a[1])? as f64);
                    if y == 0.0 {
                        return err("22012", "division by zero");
                    }
                    return Ok(Float(x / y));
                }
                ("*", mon_a, mon_b) => {
                    let (m, n) = if mon_a && !mon_b {
                        (&a[0], &a[1])
                    } else if mon_b && !mon_a {
                        (&a[1], &a[0])
                    } else {
                        return err("42883", "money times money is not defined");
                    };
                    let c = money(m)?;
                    match n {
                        Value::Int(k) => c.checked_mul(*k).ok_or_else(overflow)?,
                        n => scaled(c as f64 * float_of(n)?)?,
                    }
                }
                ("/", true, false) => {
                    let c = money(&a[0])?;
                    match &a[1] {
                        Value::Int(0) => return err("22012", "division by zero"),
                        Value::Int(k) => c.checked_div(*k).ok_or_else(overflow)?,
                        n => {
                            let d = float_of(n)?;
                            if d == 0.0 {
                                return err("22012", "division by zero");
                            }
                            scaled(c as f64 / d)?
                        }
                    }
                }
                _ => return err("42883", format!("operator does not exist: money {op} money")),
            };
            Ok(Text(format(r)))
        }
        _ => err("XX000", "not a money function"),
    }
}

fn float_of(v: &Value) -> Result<f64> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(f) => Ok(*f),
        Value::Numeric(d) => Ok(d.to_f64()),
        v => v.to_text().trim().parse().map_err(|_| bad(&v.to_text())),
    }
}

fn scaled(f: f64) -> Result<i64> {
    let r = f.round_ties_even();
    if r.is_nan() || !(-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&r) {
        return Err(overflow());
    }
    Ok(r as i64)
}
