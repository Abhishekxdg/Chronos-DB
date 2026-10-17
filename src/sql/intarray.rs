//! intarray: functions and operators over arrays of integers without nulls, as the extension has them. `sort`,
//! `uniq`, `icount`, `idx`, `subarray`, `intset`; `+` and `-` of an element or an array, `|` (union) and `&`
//! (intersection). (`&&`, `@>` and `<@` are the arrays' own.)

use super::funcs::Scalar;
use super::{SqlError, Type, Value, err};

type Result<T> = std::result::Result<T, SqlError>;

// What `call` does, by `Scalar::IntArr`'s number.
pub(super) const ICOUNT: u8 = 0;
pub(super) const SORT: u8 = 1; // sort(a [, 'asc' | 'desc'])
pub(super) const SORT_ASC: u8 = 2;
pub(super) const SORT_DESC: u8 = 3;
pub(super) const UNIQ: u8 = 4;
pub(super) const IDX: u8 = 5;
pub(super) const SUBARRAY: u8 = 6;
pub(super) const INTSET: u8 = 7;
pub(super) const ADD: u8 = 8; // a + n, a + b
pub(super) const SUB: u8 = 9; // a - n, a - b
pub(super) const UNION: u8 = 10; // a | n, a | b
pub(super) const INTER: u8 = 11; // a & b

pub(super) fn result_type(k: u8) -> Type {
    match k {
        ICOUNT | IDX => Type::Int,
        _ => Type::Array(23),
    }
}

/// An int[]'s elements: none may be null, as the extension says.
fn ints(v: &Value) -> Result<Vec<i64>> {
    let vals = super::array_values(v.clone())?.unwrap_or_default();
    vals.iter()
        .map(|x| match x {
            Value::Null => err("22004", "array must not contain nulls"),
            Value::Int(n) => Ok(*n),
            x => x
                .to_text()
                .trim()
                .parse::<i64>()
                .or_else(|_| err("22P02", format!("invalid input syntax for type integer: \"{}\"", x.to_text()))),
        })
        .collect()
}

/// Whether an operand is an array (`{1,2}`, or with its bounds written out, `[0:1]={1,2}`), not an element.
fn is_array(v: &Value) -> bool {
    match v {
        Value::Json(serde_json::Value::Array(_)) => true,
        Value::Text(t) => t.trim_start().starts_with(['{', '[']),
        _ => false,
    }
}

/// An element or an array: what the right of `+`, `-` and `|` is.
fn one_or_many(v: &Value) -> Result<Vec<i64>> {
    match v {
        Value::Int(n) => Ok(vec![*n]),
        v if is_array(v) => ints(v),
        v => Ok(vec![
            v.to_text()
                .trim()
                .parse::<i64>()
                .or_else(|_| err("22P02", format!("invalid input syntax for type integer: \"{}\"", v.to_text())))?,
        ]),
    }
}

fn array(xs: &[i64]) -> Value {
    let parts: Vec<String> = xs.iter().map(i64::to_string).collect();
    Value::Text(format!("{{{}}}", parts.join(",")))
}

/// `xs` in the shape of array `a`, as the extension changes an array in place (sort, uniq, `-` of an element): its
/// dimensions and lower bounds kept when as many elements are left; when fewer, the first dimension holds them all
/// and the others are 1 (resize_intArrayType), and an empty array when none are.
fn reshaped(a: &Value, xs: &[i64]) -> Result<Value> {
    let Some(mut arr) = super::mdarray::read(a)? else { return Ok(Value::Null) };
    if xs.is_empty() {
        return Ok(array(&[]));
    }
    if xs.len() != arr.elems.len() {
        let mut n = xs.len();
        for d in &mut arr.dims {
            d.len = n;
            n = 1;
        }
    }
    arr.elems = xs.iter().map(|x| Value::Int(*x)).collect();
    Ok(arr.value())
}

fn sorted_unique(mut xs: Vec<i64>) -> Vec<i64> {
    xs.sort_unstable();
    xs.dedup();
    xs
}

fn int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(n) => Ok(*n),
        v => v
            .to_text()
            .trim()
            .parse()
            .or_else(|_| err("22P02", format!("invalid input syntax for type integer: \"{}\"", v.to_text()))),
    }
}

pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    let Scalar::IntArr(k) = f else { return err("XX000", "not an intarray function") };
    if a.contains(&Value::Null) {
        return Ok(Value::Null);
    }
    Ok(match k {
        // (the elements, nulls among them, as the extension counts them)
        ICOUNT => Value::Int(super::array_values(a[0].clone())?.unwrap_or_default().len() as i64),
        // (an array of fewer than two is as it is, whatever the direction says, as the extension has it)
        SORT | SORT_ASC | SORT_DESC if ints(&a[0])?.len() < 2 => reshaped(&a[0], &ints(&a[0])?)?,
        SORT | SORT_ASC | SORT_DESC => {
            let desc = match (k, a.get(1)) {
                (SORT_DESC, _) => true,
                (SORT, Some(d)) => match d.to_text().to_lowercase().as_str() {
                    "asc" => false,
                    "desc" => true,
                    _ => return err("22023", "second parameter must be \"ASC\" or \"DESC\""),
                },
                _ => false,
            };
            let mut xs = ints(&a[0])?;
            xs.sort_unstable();
            if desc {
                xs.reverse();
            }
            // (sorted in place: the array keeps its dimensions and bounds)
            reshaped(&a[0], &xs)?
        }
        // (equal neighbours made one, as uniq(1) does: uniq(sort(a)) for each value once)
        UNIQ => {
            let mut xs = ints(&a[0])?;
            xs.dedup();
            reshaped(&a[0], &xs)?
        }
        IDX => {
            let (xs, n) = (ints(&a[0])?, int(&a[1])?);
            Value::Int(xs.iter().position(|x| *x == n).map_or(0, |i| i as i64 + 1))
        }
        // subarray(a, start [, len]): from start (counted from 1, or from the end when negative), len elements (to the
        // end when 0 or not given; all but the last -len when negative), as the extension's subarray works them out
        SUBARRAY => {
            let xs = ints(&a[0])?;
            let c = xs.len() as i64;
            let mut start = int(&a[1])?;
            start = if start > 0 { start - 1 } else { start };
            let len = match a.get(2) {
                Some(l) => int(l)?,
                None => 0,
            };
            if c == 0 {
                return Ok(array(&[]));
            }
            if start < 0 {
                start += c;
            }
            let end = match len {
                l if l < 0 => c + l,
                0 => c,
                l => start.saturating_add(l),
            }
            .min(c);
            let start = start.max(0);
            if start >= end || end <= 0 {
                return Ok(array(&[]));
            }
            array(&xs[start as usize..end as usize])
        }
        INTSET => array(&[int(&a[0])?]),
        ADD => {
            let mut xs = ints(&a[0])?;
            xs.extend(one_or_many(&a[1])?);
            array(&xs)
        }
        // an element goes wherever it is; an array's elements go, and what is left is sorted, each once
        SUB => match &a[1] {
            v if is_array(v) => {
                let gone = ints(v)?;
                array(&sorted_unique(ints(&a[0])?.into_iter().filter(|x| !gone.contains(x)).collect()))
            }
            // (an element taken out in place: intarray_del_elem)
            v => {
                let gone = one_or_many(v)?;
                reshaped(&a[0], &ints(&a[0])?.into_iter().filter(|x| !gone.contains(x)).collect::<Vec<_>>())?
            }
        },
        UNION => {
            let mut xs = ints(&a[0])?;
            xs.extend(one_or_many(&a[1])?);
            array(&sorted_unique(xs))
        }
        INTER => {
            let b = ints(&a[1])?;
            array(&sorted_unique(ints(&a[0])?.into_iter().filter(|x| b.contains(x)).collect()))
        }
        _ => return err("XX000", "not an intarray function"),
    })
}
