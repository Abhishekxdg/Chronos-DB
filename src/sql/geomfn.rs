//! The calls into `geom` from SQL: operators with the types the binder found, and functions by number.

use super::funcs::Scalar;
use super::{Type, Value, geom};

/// The geometric functions by number (`length`, one past the end, is apart: it is also text's). The prefix
/// operators (`@@ x` center, `@-@ x` length, `# x` points, `?- x` horizontal, `?| x` vertical) are functions here
/// too. `__interpt_pp` is regress.so's `interpt_pp(path, path)`, which a LANGUAGE C function of that library
/// runs (see `functions::regress_c`).
pub(super) const NAMES: [&str; 32] = [
    "area",
    "center",
    "radius",
    "diameter",
    "width",
    "height",
    "npoints",
    "isopen",
    "isclosed",
    "popen",
    "pclose",
    "slope",
    "isvertical",
    "ishorizontal",
    "bound_box",
    "diagonal",
    "point",
    "box",
    "lseg",
    "circle",
    "polygon",
    "path",
    "line",
    "poly_center",
    "isparallel",
    "isperp",
    "@@",
    "@-@",
    "#",
    "?-",
    "?|",
    "__interpt_pp",
];

/// The number of the function or prefix operator `name`.
pub(super) fn number(name: &str) -> u8 {
    NAMES.iter().position(|n| *n == name).unwrap_or(NAMES.len()) as u8
}

/// Whether function `k` is a prefix operator (its column is `?column?`).
pub(super) fn is_operator(k: u8) -> bool {
    NAMES.get(k as usize).is_some_and(|n| !n.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_'))
}

pub(super) fn result_type(k: u8) -> Type {
    if k as usize == NAMES.len() {
        return Type::Float;
    }
    match NAMES[k as usize % NAMES.len()] {
        "area" | "radius" | "diameter" | "width" | "height" | "slope" | "@-@" => Type::Float,
        "npoints" | "#" => Type::Int,
        "isopen" | "isclosed" | "isvertical" | "ishorizontal" | "isparallel" | "isperp" | "?-" | "?|" => Type::Bool,
        "center" | "point" | "poly_center" | "@@" | "__interpt_pp" => Type::Point,
        "popen" | "pclose" | "path" => Type::Path,
        "bound_box" | "box" => Type::Box,
        "diagonal" | "lseg" => Type::Lseg,
        "circle" => Type::Circle,
        "polygon" => Type::Polygon,
        _ => Type::Line,
    }
}

fn ty(v: Option<&Value>) -> Type {
    match v {
        Some(Value::Int(o)) => Type::from_oid(*o as u32).unwrap_or(Type::Text),
        _ => Type::Text,
    }
}

pub(super) fn call(f: Scalar, a: &[Value]) -> super::Result<Value> {
    if a.contains(&Value::Null) {
        return Ok(Value::Null);
    }
    match f {
        Scalar::GeoOp | Scalar::GeoOpF | Scalar::GeoOpV => {
            let op = a.get(2).map(Value::to_text).unwrap_or_default();
            let (x, y) = (a.first().map(Value::to_text), a.get(1).map(Value::to_text));
            geom::operate(&op, ty(a.get(3)), ty(a.get(4)), &x.unwrap_or_default(), &y.unwrap_or_default())
        }
        Scalar::GeoFn(k) => {
            // (the binder appends each argument's type)
            let n = a.len() / 2;
            let tys: Vec<Type> = a[n..].iter().map(|v| ty(Some(v))).collect();
            let name = if k as usize == NAMES.len() { "length" } else { NAMES[k as usize % NAMES.len()] };
            geom::function(name, &tys, &a[..n])
        }
        _ => unreachable!(),
    }
}
