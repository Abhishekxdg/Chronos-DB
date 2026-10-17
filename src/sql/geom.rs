//! Geometric types (point, lseg, line, box, path, polygon, circle) as Postgres's geo_ops.c has them: their input
//! and output, and the operators and functions over them, with its arithmetic (EPSILON comparisons, float8
//! overflow and underflow checks, closest points, crossings). A value is its text; every operation reads its
//! operands and writes text (or a number) back.

use std::cell::Cell;

use super::{SqlError, Type, Value, err};

type Result<T> = std::result::Result<T, SqlError>;

/// Postgres's EPSILON: geometric comparisons take values this close as equal.
const EPSILON: f64 = 1.0e-6;

fn fp_zero(a: f64) -> bool {
    a.abs() <= EPSILON
}
fn fp_eq(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= EPSILON
}
fn fp_ne(a: f64, b: f64) -> bool {
    a != b && (a - b).abs() > EPSILON
}
fn fp_lt(a: f64, b: f64) -> bool {
    a + EPSILON < b
}
fn fp_le(a: f64, b: f64) -> bool {
    a <= b + EPSILON
}
fn fp_gt(a: f64, b: f64) -> bool {
    a > b + EPSILON
}
fn fp_ge(a: f64, b: f64) -> bool {
    a + EPSILON >= b
}

/// float8's comparisons, where every NaN is one value, above all numbers.
fn f8_eq(a: f64, b: f64) -> bool {
    if a.is_nan() { b.is_nan() } else { !b.is_nan() && a == b }
}
fn f8_lt(a: f64, b: f64) -> bool {
    !a.is_nan() && (b.is_nan() || a < b)
}
fn f8_gt(a: f64, b: f64) -> bool {
    !b.is_nan() && (a.is_nan() || a > b)
}
fn f8_min(a: f64, b: f64) -> f64 {
    if f8_lt(a, b) { a } else { b }
}
fn f8_max(a: f64, b: f64) -> f64 {
    if f8_gt(a, b) { a } else { b }
}

// ---------------------------------------------------------------- float8 arithmetic

thread_local! {
    /// The first error the arithmetic of the operation running met: Postgres stops there, so what is worked out
    /// after it is thrown away (see `guarded`).
    static FAILED: Cell<Option<(&'static str, &'static str)>> = const { Cell::new(None) };
}

fn fail(code: &'static str, message: &'static str) {
    FAILED.with(|f| {
        if f.get().is_none() {
            f.set(Some((code, message)));
        }
    });
}

fn overflow() {
    fail("22003", "value out of range: overflow");
}

fn underflow() {
    fail("22003", "value out of range: underflow");
}

/// `f`, failing with the first error its arithmetic met (float8 overflow, underflow, division by zero).
fn guarded<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let outer = FAILED.with(|c| c.take());
    let r = f();
    match FAILED.with(|c| c.replace(outer)) {
        Some((code, message)) => err(code, message),
        None => r,
    }
}

/// float8_pl: a + b, an overflow when finite numbers make an infinity.
fn pl(a: f64, b: f64) -> f64 {
    let r = a + b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        overflow();
    }
    r
}

fn mi(a: f64, b: f64) -> f64 {
    let r = a - b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        overflow();
    }
    r
}

fn mul(a: f64, b: f64) -> f64 {
    let r = a * b;
    if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
        overflow();
    }
    if r == 0.0 && a != 0.0 && b != 0.0 {
        underflow();
    }
    r
}

fn div(a: f64, b: f64) -> f64 {
    if b == 0.0 && !a.is_nan() {
        fail("22012", "division by zero");
    }
    let r = a / b;
    if r.is_infinite() && !a.is_infinite() {
        overflow();
    }
    if r == 0.0 && a != 0.0 && !b.is_infinite() {
        underflow();
    }
    r
}

/// pg_hypot: sqrt(x² + y²) without overflowing on the way.
fn hypot(x: f64, y: f64) -> f64 {
    if x.is_infinite() || y.is_infinite() {
        return f64::INFINITY;
    }
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    let (mut x, mut y) = (x.abs(), y.abs());
    if x < y {
        std::mem::swap(&mut x, &mut y);
    }
    if y == 0.0 {
        return x;
    }
    let yx = y / x;
    let r = x * (1.0 + yx * yx).sqrt();
    if r.is_infinite() {
        overflow();
    }
    if r == 0.0 {
        underflow();
    }
    r
}

// ---------------------------------------------------------------- the types

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Pt {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Seg {
    pub a: Pt,
    pub b: Pt,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Bx {
    pub hi: Pt,
    pub lo: Pt,
}

#[derive(Clone, Debug)]
pub(super) struct Pth {
    pub closed: bool,
    pub pts: Vec<Pt>,
}

/// A polygon: its points and the box bounding them.
#[derive(Clone, Debug)]
struct Poly {
    p: Vec<Pt>,
    bb: Bx,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Circ {
    pub c: Pt,
    pub r: f64,
}

/// A line Ax + By + C = 0.
#[derive(Clone, Copy, Debug)]
pub(super) struct Ln {
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

// ---------------------------------------------------------------- input

fn space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The text a value of type `ty` is read from, as geo_ops.c's decoders walk it.
struct Src<'a> {
    s: &'a [u8],
    ty: &'static str,
    orig: &'a str,
}

/// strtod's reading of a number at the start of `b`: the number, how long it is, and whether it is out of range
/// (an overflow to infinity, or digits that underflow to zero).
fn strtod(b: &[u8]) -> Option<(f64, usize, bool)> {
    let mut j = 0;
    let neg = matches!(b.first(), Some(b'-'));
    if matches!(b.first(), Some(b'-' | b'+')) {
        j = 1;
    }
    let word = |w: &str| b.len() >= j + w.len() && b[j..j + w.len()].eq_ignore_ascii_case(w.as_bytes());
    let inf = if neg { f64::NEG_INFINITY } else { f64::INFINITY };
    if word("infinity") {
        return Some((inf, j + 8, false));
    }
    if word("inf") {
        return Some((inf, j + 3, false));
    }
    if word("nan") {
        return Some((f64::NAN, j + 3, false));
    }
    let digit = |k: usize| b.get(k).is_some_and(u8::is_ascii_digit);
    let (mut digits, mut nonzero) = (false, false);
    while digit(j) {
        nonzero |= b[j] != b'0';
        digits = true;
        j += 1;
    }
    if b.get(j) == Some(&b'.') {
        j += 1;
        while digit(j) {
            nonzero |= b[j] != b'0';
            digits = true;
            j += 1;
        }
    }
    if !digits {
        return None;
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        let from = k;
        while digit(k) {
            k += 1;
        }
        if k > from {
            j = k;
        }
    }
    let v: f64 = std::str::from_utf8(&b[..j]).ok()?.parse().ok()?;
    Some((v, j, v.is_infinite() || (v == 0.0 && nonzero)))
}

impl<'a> Src<'a> {
    fn new(s: &'a str, ty: &'static str) -> Src<'a> {
        Src { s: s.as_bytes(), ty, orig: s }
    }

    fn at(&self, i: usize) -> u8 {
        self.s.get(i).copied().unwrap_or(0)
    }

    fn ws(&self, mut i: usize) -> usize {
        while space(self.at(i)) {
            i += 1;
        }
        i
    }

    fn bad(&self) -> SqlError {
        SqlError {
            code: "22P02",
            message: format!("invalid input syntax for type {}: \"{}\"", self.ty, self.orig),
            info: None,
        }
    }

    fn end(&self, i: usize) -> Result<()> {
        if i < self.s.len() { Err(self.bad()) } else { Ok(()) }
    }

    /// float8in_internal: a number, with the space around it; where the text goes on after it.
    fn single(&self, i: usize) -> Result<(f64, usize)> {
        let i = self.ws(i);
        if i >= self.s.len() {
            return Err(self.bad());
        }
        let rest = &self.s[i..];
        let Some((v, n, range)) = strtod(rest) else { return Err(self.bad()) };
        if range {
            let t = String::from_utf8_lossy(&rest[..n]);
            return err("22003", format!("\"{t}\" is out of range for type double precision"));
        }
        Ok((v, self.ws(i + n)))
    }

    /// pair_decode: `x,y` or `(x,y)`.
    fn pair(&self, i: usize) -> Result<(Pt, usize)> {
        let mut i = self.ws(i);
        let delim = self.at(i) == b'(';
        if delim {
            i += 1;
        }
        let (x, j) = self.single(i)?;
        if self.at(j) != b',' {
            return Err(self.bad());
        }
        let (y, mut j) = self.single(j + 1)?;
        if delim {
            if self.at(j) != b')' {
                return Err(self.bad());
            }
            j = self.ws(j + 1);
        }
        Ok((Pt { x, y }, j))
    }

    /// path_decode: `npts` points, in `[...]` (when `opentype` allows it), `(...)` or neither; whether `[` opened
    /// them, and where the text goes on.
    fn path(&self, i: usize, opentype: bool, npts: usize) -> Result<(Vec<Pt>, bool, usize)> {
        let mut i = self.ws(i);
        let mut depth = 0;
        let isopen = self.at(i) == b'[';
        if isopen {
            if !opentype {
                return Err(self.bad());
            }
            depth += 1;
            i += 1;
        } else if self.at(i) == b'(' {
            let cp = self.ws(i + 1);
            if self.at(cp) == b'(' || self.s[i..].iter().rposition(|&c| c == b'(') == Some(0) {
                depth += 1;
                i = cp;
            }
        }
        let mut pts = Vec::with_capacity(npts.min(1 << 16));
        for _ in 0..npts {
            let (p, j) = self.pair(i)?;
            i = j;
            if self.at(i) == b',' {
                i += 1;
            }
            pts.push(p);
        }
        while depth > 0 {
            if self.at(i) == b')' || (self.at(i) == b']' && isopen && depth == 1) {
                depth -= 1;
                i = self.ws(i + 1);
            } else {
                return Err(self.bad());
            }
        }
        Ok((pts, isopen, i))
    }

    /// pair_count: how many points the commas say there are (none when their count is even).
    fn pairs(&self) -> usize {
        let n = self.s.iter().filter(|&&c| c == b',').count();
        if n % 2 == 1 { n.div_ceil(2) } else { 0 }
    }
}

pub(super) fn parse_point(s: &str) -> Result<Pt> {
    let src = Src::new(s, "point");
    let (p, i) = src.pair(0)?;
    src.end(i)?;
    Ok(p)
}

pub(super) fn parse_lseg(s: &str) -> Result<Seg> {
    let src = Src::new(s, "lseg");
    let (p, _, i) = src.path(0, true, 2)?;
    src.end(i)?;
    Ok(Seg { a: p[0], b: p[1] })
}

/// A box, its corners put in order (high is upper right).
pub(super) fn parse_box(s: &str) -> Result<Bx> {
    let src = Src::new(s, "box");
    let (p, _, i) = src.path(0, false, 2)?;
    src.end(i)?;
    let (mut hi, mut lo) = (p[0], p[1]);
    if f8_lt(hi.x, lo.x) {
        std::mem::swap(&mut hi.x, &mut lo.x);
    }
    if f8_lt(hi.y, lo.y) {
        std::mem::swap(&mut hi.y, &mut lo.y);
    }
    Ok(Bx { hi, lo })
}

pub(super) fn parse_path(s: &str) -> Result<Pth> {
    let src = Src::new(s, "path");
    let npts = src.pairs();
    if npts == 0 {
        return Err(src.bad());
    }
    let mut i = src.ws(0);
    let mut depth = 0;
    if src.at(i) == b'(' && src.s[i..].iter().rposition(|&c| c == b'(') == Some(0) {
        i += 1;
        depth += 1;
    }
    let (pts, isopen, mut i) = src.path(i, true, npts)?;
    if depth >= 1 {
        if src.at(i) != b')' {
            return Err(src.bad());
        }
        i = src.ws(i + 1);
    }
    src.end(i)?;
    Ok(Pth { closed: !isopen, pts })
}

pub(super) fn parse_polygon(s: &str) -> Result<Vec<Pt>> {
    let src = Src::new(s, "polygon");
    let npts = src.pairs();
    if npts == 0 {
        return Err(src.bad());
    }
    let (pts, _, i) = src.path(0, false, npts)?;
    src.end(i)?;
    Ok(pts)
}

fn read_poly(s: &str) -> Result<Poly> {
    Ok(poly_of(parse_polygon(s)?))
}

/// make_bound_box: the polygon with the box around its points.
fn poly_of(p: Vec<Pt>) -> Poly {
    let (mut x1, mut x2, mut y1, mut y2) = (p[0].x, p[0].x, p[0].y, p[0].y);
    for q in &p[1..] {
        if f8_lt(q.x, x1) {
            x1 = q.x;
        }
        if f8_gt(q.x, x2) {
            x2 = q.x;
        }
        if f8_lt(q.y, y1) {
            y1 = q.y;
        }
        if f8_gt(q.y, y2) {
            y2 = q.y;
        }
    }
    Poly { p, bb: Bx { hi: Pt { x: x2, y: y2 }, lo: Pt { x: x1, y: y1 } } }
}

/// `<(x,y),r>`, `((x,y),r)`, `(x,y),r` or `x,y,r`.
pub(super) fn parse_circle(s: &str) -> Result<Circ> {
    let src = Src::new(s, "circle");
    let mut i = src.ws(0);
    let mut depth = 0;
    if src.at(i) == b'<' {
        depth += 1;
        i += 1;
    } else if src.at(i) == b'(' {
        let cp = src.ws(i + 1);
        if src.at(cp) == b'(' {
            depth += 1;
            i = cp;
        }
    }
    let (c, j) = src.pair(i)?;
    i = j;
    if src.at(i) == b',' {
        i += 1;
    }
    let (r, j) = src.single(i)?;
    i = j;
    if r < 0.0 {
        return Err(src.bad());
    }
    while depth > 0 {
        if src.at(i) == b')' || (src.at(i) == b'>' && depth == 1) {
            depth -= 1;
            i = src.ws(i + 1);
        } else {
            return Err(src.bad());
        }
    }
    src.end(i)?;
    Ok(Circ { c, r })
}

/// `{A,B,C}`, or two points the line goes through.
pub(super) fn parse_line(s: &str) -> Result<Ln> {
    let src = Src::new(s, "line");
    let i = src.ws(0);
    if src.at(i) == b'{' {
        let (a, j) = src.single(i + 1)?;
        if src.at(j) != b',' {
            return Err(src.bad());
        }
        let (b, j) = src.single(j + 1)?;
        if src.at(j) != b',' {
            return Err(src.bad());
        }
        let (c, j) = src.single(j + 1)?;
        if src.at(j) != b'}' {
            return Err(src.bad());
        }
        src.end(src.ws(j + 1))?;
        if fp_zero(a) && fp_zero(b) {
            return err("22P02", "invalid line specification: A and B cannot both be zero");
        }
        return Ok(Ln { a, b, c });
    }
    let (p, _, j) = src.path(i, true, 2)?;
    src.end(j)?;
    if pt_eq(p[0], p[1]) {
        return err("22P02", "invalid line specification: must be two distinct points");
    }
    guarded(|| Ok(line_construct(p[0], pt_sl(p[0], p[1]))))
}

// ---------------------------------------------------------------- output

fn num(f: f64) -> String {
    super::fmt_float(f)
}

fn pair(p: Pt) -> String {
    format!("({},{})", num(p.x), num(p.y))
}

fn pairs(p: &[Pt]) -> String {
    p.iter().map(|q| pair(*q)).collect::<Vec<_>>().join(",")
}

pub(super) fn out_point(p: Pt) -> String {
    pair(p)
}
pub(super) fn out_lseg(s: &Seg) -> String {
    format!("[{},{}]", pair(s.a), pair(s.b))
}
pub(super) fn out_box(b: &Bx) -> String {
    format!("{},{}", pair(b.hi), pair(b.lo))
}
pub(super) fn out_path(p: &Pth) -> String {
    if p.closed { format!("({})", pairs(&p.pts)) } else { format!("[{}]", pairs(&p.pts)) }
}
pub(super) fn out_polygon(p: &[Pt]) -> String {
    format!("({})", pairs(p))
}
pub(super) fn out_circle(c: &Circ) -> String {
    format!("<{},{}>", pair(c.c), num(c.r))
}
pub(super) fn out_line(l: &Ln) -> String {
    format!("{{{},{},{}}}", num(l.a), num(l.b), num(l.c))
}

/// Text as its canonical text, for the geometric type `ty`.
pub(super) fn canonical(ty: Type, s: &str) -> Result<String> {
    Ok(match ty {
        Type::Point => out_point(parse_point(s)?),
        Type::Lseg => out_lseg(&parse_lseg(s)?),
        Type::Box => out_box(&parse_box(s)?),
        Type::Path => out_path(&parse_path(s)?),
        Type::Polygon => out_polygon(&parse_polygon(s)?),
        Type::Circle => out_circle(&parse_circle(s)?),
        Type::Line => out_line(&parse_line(s)?),
        _ => s.to_string(),
    })
}

// ---------------------------------------------------------------- points

fn pt_add(a: Pt, b: Pt) -> Pt {
    Pt { x: pl(a.x, b.x), y: pl(a.y, b.y) }
}

fn pt_sub(a: Pt, b: Pt) -> Pt {
    Pt { x: mi(a.x, b.x), y: mi(a.y, b.y) }
}

/// A point times a point: complex multiplication (a scaling and a rotation).
fn pt_mul(a: Pt, b: Pt) -> Pt {
    Pt { x: mi(mul(a.x, b.x), mul(a.y, b.y)), y: pl(mul(a.x, b.y), mul(a.y, b.x)) }
}

/// A point over a point: complex division.
fn pt_div(a: Pt, b: Pt) -> Pt {
    let d = pl(mul(b.x, b.x), mul(b.y, b.y));
    Pt { x: div(pl(mul(a.x, b.x), mul(a.y, b.y)), d), y: div(mi(mul(a.y, b.x), mul(a.x, b.y)), d) }
}

/// point_eq_point: the same within EPSILON (NaNs only equal NaNs).
fn pt_eq(a: Pt, b: Pt) -> bool {
    if a.x.is_nan() || a.y.is_nan() || b.x.is_nan() || b.y.is_nan() {
        return f8_eq(a.x, b.x) && f8_eq(a.y, b.y);
    }
    fp_eq(a.x, b.x) && fp_eq(a.y, b.y)
}

fn pt_dt(a: Pt, b: Pt) -> f64 {
    hypot(mi(a.x, b.x), mi(a.y, b.y))
}

/// The slope from one point to another: infinite when vertical, 0 when horizontal.
fn pt_sl(a: Pt, b: Pt) -> f64 {
    if fp_eq(a.x, b.x) {
        return f64::INFINITY;
    }
    if fp_eq(a.y, b.y) {
        return 0.0;
    }
    div(mi(a.y, b.y), mi(a.x, b.x))
}

/// The slope perpendicular to the one from `a` to `b`.
fn pt_invsl(a: Pt, b: Pt) -> f64 {
    if fp_eq(a.x, b.x) {
        return 0.0;
    }
    if fp_eq(a.y, b.y) {
        return f64::INFINITY;
    }
    div(mi(a.x, b.x), mi(b.y, a.y))
}

// ---------------------------------------------------------------- lines

/// The line through `p` with slope `m`.
fn line_construct(p: Pt, m: f64) -> Ln {
    if m.is_infinite() {
        Ln { a: -1.0, b: 0.0, c: p.x }
    } else if m == 0.0 {
        Ln { a: 0.0, b: -1.0, c: p.y }
    } else {
        let c = mi(p.y, mul(m, p.x));
        Ln { a: m, b: -1.0, c: if c == 0.0 { 0.0 } else { c } }
    }
}

fn line_sl(l: &Ln) -> f64 {
    if fp_zero(l.a) {
        return 0.0;
    }
    if fp_zero(l.b) {
        return f64::INFINITY;
    }
    div(l.a, -l.b)
}

fn line_invsl(l: &Ln) -> f64 {
    if fp_zero(l.a) {
        return f64::INFINITY;
    }
    if fp_zero(l.b) {
        return 0.0;
    }
    div(l.b, l.a)
}

/// Where two lines meet; none when they are parallel.
fn line_interpt_line(l1: &Ln, l2: &Ln) -> Option<Pt> {
    let (x, y) = if !fp_zero(l1.b) {
        if fp_eq(l2.a, mul(l1.a, div(l2.b, l1.b))) {
            return None;
        }
        let x = div(mi(mul(l1.b, l2.c), mul(l2.b, l1.c)), mi(mul(l1.a, l2.b), mul(l2.a, l1.b)));
        (x, div(-pl(mul(l1.a, x), l1.c), l1.b))
    } else if !fp_zero(l2.b) {
        if fp_eq(l1.a, mul(l2.a, div(l1.b, l2.b))) {
            return None;
        }
        let x = div(mi(mul(l2.b, l1.c), mul(l1.b, l2.c)), mi(mul(l2.a, l1.b), mul(l1.a, l2.b)));
        (x, div(-pl(mul(l2.a, x), l2.c), l2.b))
    } else {
        return None;
    };
    // (no minus zero)
    let unsigned = |v: f64| if v == 0.0 { 0.0 } else { v };
    Some(Pt { x: unsigned(x), y: unsigned(y) })
}

fn line_contain_point(l: &Ln, p: Pt) -> bool {
    fp_zero(pl(pl(mul(l.a, p.x), mul(l.b, p.y)), l.c))
}

/// The distance from a point to a line, and the line's point closest to it (NaN when it can't be told).
fn line_closept_point(l: &Ln, p: Pt) -> (f64, Pt) {
    let tmp = line_construct(p, line_invsl(l));
    match line_interpt_line(&tmp, l) {
        Some(c) => (pt_dt(c, p), c),
        None => (f64::NAN, p),
    }
}

fn line_perp(l1: &Ln, l2: &Ln) -> bool {
    if fp_zero(l1.a) {
        return fp_zero(l2.b);
    }
    if fp_zero(l2.a) {
        return fp_zero(l1.b);
    }
    if fp_zero(l1.b) {
        return fp_zero(l2.a);
    }
    if fp_zero(l2.b) {
        return fp_zero(l1.a);
    }
    fp_eq(div(mul(l1.a, l2.a), mul(l1.b, l2.b)), -1.0)
}

/// The same line: one's coefficients a multiple of the other's.
fn line_eq(l1: &Ln, l2: &Ln) -> bool {
    if [l1.a, l1.b, l1.c, l2.a, l2.b, l2.c].iter().any(|v| v.is_nan()) {
        return f8_eq(l1.a, l2.a) && f8_eq(l1.b, l2.b) && f8_eq(l1.c, l2.c);
    }
    let ratio = if !fp_zero(l2.a) {
        div(l1.a, l2.a)
    } else if !fp_zero(l2.b) {
        div(l1.b, l2.b)
    } else if !fp_zero(l2.c) {
        div(l1.c, l2.c)
    } else {
        1.0
    };
    fp_eq(l1.a, mul(ratio, l2.a)) && fp_eq(l1.b, mul(ratio, l2.b)) && fp_eq(l1.c, mul(ratio, l2.c))
}

fn line_distance(l1: &Ln, l2: &Ln) -> f64 {
    if line_interpt_line(l1, l2).is_some() {
        return 0.0;
    }
    let ok = |v: f64| !fp_zero(v) && !v.is_nan();
    let ratio = if ok(l1.a) && ok(l2.a) {
        div(l1.a, l2.a)
    } else if ok(l1.b) && ok(l2.b) {
        div(l1.b, l2.b)
    } else {
        1.0
    };
    div(mi(l1.c, mul(ratio, l2.c)).abs(), hypot(l1.a, l1.b))
}

// ---------------------------------------------------------------- line segments

fn seg(a: Pt, b: Pt) -> Seg {
    Seg { a, b }
}

fn lseg_sl(s: &Seg) -> f64 {
    pt_sl(s.a, s.b)
}

fn lseg_invsl(s: &Seg) -> f64 {
    pt_invsl(s.a, s.b)
}

fn lseg_len(s: &Seg) -> f64 {
    pt_dt(s.a, s.b)
}

fn lseg_center(s: &Seg) -> Pt {
    Pt { x: div(pl(s.a.x, s.b.x), 2.0), y: div(pl(s.a.y, s.b.y), 2.0) }
}

/// A point on a segment: its distances to the ends add up to the segment's length.
fn lseg_contain_point(s: &Seg, p: Pt) -> bool {
    fp_eq(pt_dt(p, s.a) + pt_dt(p, s.b), pt_dt(s.a, s.b))
}

/// Where a segment meets a line (an end, when the meeting point is one).
fn lseg_interpt_line(s: &Seg, l: &Ln) -> Option<Pt> {
    let tmp = line_construct(s.a, lseg_sl(s));
    let ip = line_interpt_line(&tmp, l)?;
    if !lseg_contain_point(s, ip) {
        return None;
    }
    Some(if pt_eq(s.a, ip) {
        s.a
    } else if pt_eq(s.b, ip) {
        s.b
    } else {
        ip
    })
}

/// Where two segments meet.
fn lseg_interpt_lseg(l1: &Seg, l2: &Seg) -> Option<Pt> {
    let tmp = line_construct(l2.a, lseg_sl(l2));
    let ip = lseg_interpt_line(l1, &tmp)?;
    if !lseg_contain_point(l2, ip) {
        return None;
    }
    Some(ip)
}

/// The distance from a segment to a line, and the segment's point closest to it.
fn lseg_closept_line(s: &Seg, l: &Ln) -> (f64, Pt) {
    if let Some(p) = lseg_interpt_line(s, l) {
        return (0.0, p);
    }
    let d1 = line_closept_point(l, s.a).0;
    let d2 = line_closept_point(l, s.b).0;
    if d1 < d2 { (d1, s.a) } else { (d2, s.b) }
}

/// The distance from a point to a segment, and the segment's point closest to it.
fn lseg_closept_point(s: &Seg, p: Pt) -> (f64, Pt) {
    let tmp = line_construct(p, pt_invsl(s.a, s.b));
    let c = lseg_closept_line(s, &tmp).1;
    (pt_dt(c, p), c)
}

/// The distance between two segments, and the point of `on` closest to `to`.
fn lseg_closept_lseg(on: &Seg, to: &Seg) -> (f64, Pt) {
    if let Some(p) = lseg_interpt_lseg(on, to) {
        return (0.0, p);
    }
    let (mut dist, mut res) = lseg_closept_point(on, to.a);
    let (d, p) = lseg_closept_point(on, to.b);
    if f8_lt(d, dist) {
        (dist, res) = (d, p);
    }
    let d = lseg_closept_point(to, on.a).0;
    if f8_lt(d, dist) {
        (dist, res) = (d, on.a);
    }
    let d = lseg_closept_point(to, on.b).0;
    if f8_lt(d, dist) {
        (dist, res) = (d, on.b);
    }
    (dist, res)
}

// ---------------------------------------------------------------- boxes

/// The box with these two corners.
fn box_construct(p1: Pt, p2: Pt) -> Bx {
    let (hx, lx) = if f8_gt(p1.x, p2.x) { (p1.x, p2.x) } else { (p2.x, p1.x) };
    let (hy, ly) = if f8_gt(p1.y, p2.y) { (p1.y, p2.y) } else { (p2.y, p1.y) };
    Bx { hi: Pt { x: hx, y: hy }, lo: Pt { x: lx, y: ly } }
}

fn box_ov(b1: &Bx, b2: &Bx) -> bool {
    fp_le(b1.lo.x, b2.hi.x) && fp_le(b2.lo.x, b1.hi.x) && fp_le(b1.lo.y, b2.hi.y) && fp_le(b2.lo.y, b1.hi.y)
}

fn box_wd(b: &Bx) -> f64 {
    mi(b.hi.x, b.lo.x)
}

fn box_ht(b: &Bx) -> f64 {
    mi(b.hi.y, b.lo.y)
}

fn box_ar(b: &Bx) -> f64 {
    mul(box_wd(b), box_ht(b))
}

fn box_cn(b: &Bx) -> Pt {
    Pt { x: div(pl(b.hi.x, b.lo.x), 2.0), y: div(pl(b.hi.y, b.lo.y), 2.0) }
}

/// A point in a box, edges included (exactly, without EPSILON).
fn box_contain_point(b: &Bx, p: Pt) -> bool {
    b.hi.x >= p.x && b.lo.x <= p.x && b.hi.y >= p.y && b.lo.y <= p.y
}

fn box_contain_box(a: &Bx, b: &Bx) -> bool {
    fp_ge(a.hi.x, b.hi.x) && fp_le(a.lo.x, b.lo.x) && fp_ge(a.hi.y, b.hi.y) && fp_le(a.lo.y, b.lo.y)
}

fn box_contain_lseg(b: &Bx, s: &Seg) -> bool {
    box_contain_point(b, s.a) && box_contain_point(b, s.b)
}

/// The box's four sides, in the order geo_ops.c tries them.
fn box_sides(b: &Bx) -> [Seg; 4] {
    let ul = Pt { x: b.lo.x, y: b.hi.y };
    let lr = Pt { x: b.hi.x, y: b.lo.y };
    [seg(b.lo, ul), seg(b.hi, ul), seg(b.lo, lr), seg(b.hi, lr)]
}

/// The distance from a point to a box, and the box's point closest to it.
fn box_closept_point(b: &Bx, p: Pt) -> (f64, Pt) {
    if box_contain_point(b, p) {
        return (0.0, p);
    }
    let sides = box_sides(b);
    let (mut dist, mut res) = lseg_closept_point(&sides[0], p);
    for s in &sides[1..] {
        let (d, c) = lseg_closept_point(s, p);
        if f8_lt(d, dist) {
            (dist, res) = (d, c);
        }
    }
    (dist, res)
}

/// Whether a segment crosses a box; with `want`, the segment's point closest to the box's center too.
fn box_interpt_lseg(b: &Bx, s: &Seg, want: bool) -> Option<Pt> {
    let lbox = Bx {
        lo: Pt { x: f8_min(s.a.x, s.b.x), y: f8_min(s.a.y, s.b.y) },
        hi: Pt { x: f8_max(s.a.x, s.b.x), y: f8_max(s.a.y, s.b.y) },
    };
    if !box_ov(&lbox, b) {
        return None;
    }
    let res = if want { lseg_closept_point(s, box_cn(b)).1 } else { Pt { x: 0.0, y: 0.0 } };
    if box_contain_point(b, s.a) || box_contain_point(b, s.b) {
        return Some(res);
    }
    for side in &box_sides(b) {
        if lseg_interpt_lseg(side, s).is_some() {
            return Some(res);
        }
    }
    None
}

/// The distance from a segment to a box, and the box's point closest to it.
fn box_closept_lseg(b: &Bx, s: &Seg) -> (f64, Pt) {
    if let Some(p) = box_interpt_lseg(b, s, true) {
        return (0.0, p);
    }
    let sides = box_sides(b);
    let (mut dist, mut res) = lseg_closept_lseg(&sides[0], s);
    for side in &sides[1..] {
        let (d, c) = lseg_closept_lseg(side, s);
        if f8_lt(d, dist) {
            (dist, res) = (d, c);
        }
    }
    (dist, res)
}

/// Whether a line crosses a box: any of its sides.
fn inter_lb(l: &Ln, b: &Bx) -> bool {
    let mut p1 = b.lo;
    let mut p2 = Pt { x: b.lo.x, y: b.hi.y };
    if lseg_interpt_line(&seg(p1, p2), l).is_some() {
        return true;
    }
    p1 = b.hi;
    if lseg_interpt_line(&seg(p1, p2), l).is_some() {
        return true;
    }
    p2 = Pt { x: b.hi.x, y: b.lo.y };
    if lseg_interpt_line(&seg(p1, p2), l).is_some() {
        return true;
    }
    p1 = b.lo;
    lseg_interpt_line(&seg(p1, p2), l).is_some()
}

fn circle_box(c: &Circ) -> Bx {
    let d = div(c.r, 2.0f64.sqrt());
    Bx { hi: Pt { x: pl(c.c.x, d), y: pl(c.c.y, d) }, lo: Pt { x: mi(c.c.x, d), y: mi(c.c.y, d) } }
}

fn box_circle(b: &Bx) -> Circ {
    let c = Pt { x: div(pl(b.hi.x, b.lo.x), 2.0), y: div(pl(b.hi.y, b.lo.y), 2.0) };
    Circ { c, r: pt_dt(c, b.hi) }
}

fn box_poly(b: &Bx) -> Vec<Pt> {
    vec![b.lo, Pt { x: b.lo.x, y: b.hi.y }, b.hi, Pt { x: b.hi.x, y: b.lo.y }]
}

// ---------------------------------------------------------------- paths

/// The segments of a path, each with the point before it (a closed path's first goes from its last point).
fn path_segs(p: &Pth) -> Vec<Seg> {
    let n = p.pts.len();
    (0..n)
        .filter_map(|i| match i {
            0 if !p.closed => None,
            0 => Some(seg(p.pts[n - 1], p.pts[0])),
            _ => Some(seg(p.pts[i - 1], p.pts[i])),
        })
        .collect()
}

fn path_length(p: &Pth) -> f64 {
    path_segs(p).iter().fold(0.0, |l, s| pl(l, pt_dt(s.a, s.b)))
}

fn path_area(p: &Pth) -> Value {
    if !p.closed {
        return Value::Null;
    }
    let n = p.pts.len();
    let mut area = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        area = pl(area, mul(p.pts[i].x, p.pts[j].y));
        area = mi(area, mul(p.pts[i].y, p.pts[j].x));
    }
    Value::Float(div(area.abs(), 2.0))
}

fn path_bbox(p: &[Pt]) -> Bx {
    let mut b = Bx { hi: p[0], lo: p[0] };
    for q in &p[1..] {
        b.hi.x = f8_max(q.x, b.hi.x);
        b.hi.y = f8_max(q.y, b.hi.y);
        b.lo.x = f8_min(q.x, b.lo.x);
        b.lo.y = f8_min(q.y, b.lo.y);
    }
    b
}

/// Whether two paths cross.
fn path_inter(p1: &Pth, p2: &Pth) -> bool {
    if !box_ov(&path_bbox(&p1.pts), &path_bbox(&p2.pts)) {
        return false;
    }
    let s2 = path_segs(p2);
    path_segs(p1).iter().any(|a| s2.iter().any(|b| lseg_interpt_lseg(a, b).is_some()))
}

/// The least distance between two paths' segments (none when one has no segment).
fn path_distance(p1: &Pth, p2: &Pth) -> Value {
    let s2 = path_segs(p2);
    let mut min: Option<f64> = None;
    for a in &path_segs(p1) {
        for b in &s2 {
            let d = lseg_closept_lseg(a, b).0;
            if min.is_none_or(|m| f8_lt(d, m)) {
                min = Some(d);
            }
        }
    }
    min.map_or(Value::Null, Value::Float)
}

/// The least distance from a point to a path's segments (0 for a path of one point).
fn dist_ppath(p: Pt, path: &Pth) -> f64 {
    let mut min: Option<f64> = None;
    for s in &path_segs(path) {
        let d = lseg_closept_point(s, p).0;
        if min.is_none_or(|m| f8_lt(d, m)) {
            min = Some(d);
        }
    }
    min.unwrap_or(0.0)
}

/// A point on an open path's segments, or in a closed path.
fn on_ppath(p: Pt, path: &Pth) -> bool {
    if !path.closed {
        let pts = &path.pts;
        let mut a = pt_dt(p, pts[0]);
        for i in 0..pts.len() - 1 {
            let b = pt_dt(p, pts[i + 1]);
            if fp_eq(pl(a, b), pt_dt(pts[i], pts[i + 1])) {
                return true;
            }
            a = b;
        }
        return false;
    }
    point_inside(p, &path.pts) != 0
}

fn path_map(p: &Pth, f: impl Fn(Pt) -> Pt) -> Value {
    Value::Text(out_path(&Pth { closed: p.closed, pts: p.pts.iter().map(|q| f(*q)).collect() }))
}

// ---------------------------------------------------------------- polygons

const POINT_ON_POLYGON: i32 = i32::MAX;

/// 0 when the point is outside the polygon, 1 inside, 2 on its edge (by the crossings of a ray along +x).
fn point_inside(p: Pt, plist: &[Pt]) -> i32 {
    let x0 = mi(plist[0].x, p.x);
    let y0 = mi(plist[0].y, p.y);
    let (mut prev_x, mut prev_y) = (x0, y0);
    let mut total = 0;
    for q in &plist[1..] {
        let x = mi(q.x, p.x);
        let y = mi(q.y, p.y);
        let cross = lseg_crossing(x, y, prev_x, prev_y);
        if cross == POINT_ON_POLYGON {
            return 2;
        }
        total += cross;
        (prev_x, prev_y) = (x, y);
    }
    let cross = lseg_crossing(x0, y0, prev_x, prev_y);
    if cross == POINT_ON_POLYGON {
        return 2;
    }
    total += cross;
    i32::from(total != 0)
}

/// How the segment from (prev_x, prev_y) to (x, y) crosses the positive x axis: ±2 across it, ±1 to or from it,
/// POINT_ON_POLYGON through the origin.
fn lseg_crossing(x: f64, y: f64, prev_x: f64, prev_y: f64) -> i32 {
    if fp_zero(y) {
        if fp_zero(x) {
            POINT_ON_POLYGON
        } else if fp_gt(x, 0.0) {
            if fp_zero(prev_y) {
                return if fp_gt(prev_x, 0.0) { 0 } else { POINT_ON_POLYGON };
            }
            if fp_lt(prev_y, 0.0) { 1 } else { -1 }
        } else {
            if fp_zero(prev_y) {
                return if fp_lt(prev_x, 0.0) { 0 } else { POINT_ON_POLYGON };
            }
            0
        }
    } else {
        let y_sign = if fp_gt(y, 0.0) { 1 } else { -1 };
        if fp_zero(prev_y) {
            if fp_lt(prev_x, 0.0) { 0 } else { y_sign }
        } else if (y_sign < 0 && fp_lt(prev_y, 0.0)) || (y_sign > 0 && fp_gt(prev_y, 0.0)) {
            0
        } else {
            if fp_ge(x, 0.0) && fp_gt(prev_x, 0.0) {
                return 2 * y_sign;
            }
            if fp_lt(x, 0.0) && fp_le(prev_x, 0.0) {
                return 0;
            }
            let z = mi(mul(mi(x, prev_x), y), mul(mi(y, prev_y), x));
            if fp_zero(z) {
                return POINT_ON_POLYGON;
            }
            if (y_sign < 0 && fp_lt(z, 0.0)) || (y_sign > 0 && fp_gt(z, 0.0)) {
                return 0;
            }
            2 * y_sign
        }
    }
}

/// The same points, starting anywhere and going either way round.
fn plist_same(p1: &[Pt], p2: &[Pt]) -> bool {
    let n = p1.len() as i64;
    for i in 0..n {
        if !pt_eq(p2[i as usize], p1[0]) {
            continue;
        }
        let (mut ii, mut j) = (1, i + 1);
        while ii < n {
            if j >= n {
                j = 0;
            }
            if !pt_eq(p2[j as usize], p1[ii as usize]) {
                break;
            }
            ii += 1;
            j += 1;
        }
        if ii == n {
            return true;
        }
        let (mut ii, mut j) = (1, i - 1);
        while ii < n {
            if j < 0 {
                j = n - 1;
            }
            if !pt_eq(p2[j as usize], p1[ii as usize]) {
                break;
            }
            ii += 1;
            j -= 1;
        }
        if ii == n {
            return true;
        }
    }
    false
}

/// The polygon's edges, each from the point before (the first from the last point).
fn poly_edges(p: &Poly) -> impl Iterator<Item = Seg> + '_ {
    let n = p.p.len();
    (0..n).map(move |i| seg(p.p[if i == 0 { n - 1 } else { i - 1 }], p.p[i]))
}

fn poly_overlap(a: &Poly, b: &Poly) -> bool {
    if !box_ov(&a.bb, &b.bb) {
        return false;
    }
    let crossed = poly_edges(a).any(|sa| poly_edges(b).any(|sb| lseg_interpt_lseg(&sa, &sb).is_some()));
    crossed || point_inside(a.p[0], &b.p) != 0 || point_inside(b.p[0], &a.p) != 0
}

/// Whether the segment from `a` to `b`, which touches `s` at `a`, stays in the polygon from edge `start` on.
fn touched_lseg_inside_poly(a: Pt, b: Pt, s: &Seg, poly: &Poly, start: usize) -> bool {
    let t = seg(a, b);
    if pt_eq(a, s.a) {
        if lseg_contain_point(&t, s.b) {
            return lseg_inside_poly(b, s.b, poly, start);
        }
    } else if pt_eq(a, s.b) {
        if lseg_contain_point(&t, s.a) {
            return lseg_inside_poly(b, s.a, poly, start);
        }
    } else if lseg_contain_point(&t, s.a) {
        return lseg_inside_poly(b, s.a, poly, start);
    } else if lseg_contain_point(&t, s.b) {
        return lseg_inside_poly(b, s.b, poly, start);
    }
    true
}

/// Whether the segment from `a` to `b` is inside the polygon, checked against its edges from `start` on.
fn lseg_inside_poly(a: Pt, b: Pt, poly: &Poly, start: usize) -> bool {
    let n = poly.p.len();
    let t = seg(a, b);
    let mut s0 = poly.p[if start == 0 { n - 1 } else { start - 1 }];
    let (mut res, mut intersection) = (true, false);
    let mut i = start;
    while i < n && res {
        let s = seg(s0, poly.p[i]);
        if lseg_contain_point(&s, t.a) {
            if lseg_contain_point(&s, t.b) {
                return true; // t is a part of s
            }
            res = touched_lseg_inside_poly(t.a, t.b, &s, poly, i + 1);
        } else if lseg_contain_point(&s, t.b) {
            res = touched_lseg_inside_poly(t.b, t.a, &s, poly, i + 1);
        } else if let Some(ip) = lseg_interpt_lseg(&t, &s) {
            // the edge cuts t in two: both halves must be inside
            intersection = true;
            res = lseg_inside_poly(t.a, ip, poly, i + 1);
            if res {
                res = lseg_inside_poly(t.b, ip, poly, i + 1);
            }
        }
        s0 = s.b;
        i += 1;
    }
    if res && !intersection {
        let p = Pt { x: div(pl(t.a.x, t.b.x), 2.0), y: div(pl(t.a.y, t.b.y), 2.0) };
        res = point_inside(p, &poly.p) != 0;
    }
    res
}

fn poly_contain_poly(a: &Poly, b: &Poly) -> bool {
    if !box_contain_box(&a.bb, &b.bb) {
        return false;
    }
    poly_edges(b).all(|s| lseg_inside_poly(s.a, s.b, a, 0))
}

fn poly_distance(a: &Poly, b: &Poly) -> f64 {
    if poly_overlap(a, b) {
        return 0.0;
    }
    let mut min: Option<f64> = None;
    for sa in poly_edges(a) {
        for sb in poly_edges(b) {
            let d = lseg_closept_lseg(&sa, &sb).0;
            if min.is_none_or(|m| f8_lt(d, m)) {
                min = Some(d);
            }
        }
    }
    min.unwrap_or(0.0)
}

fn dist_ppoly(p: Pt, poly: &Poly) -> f64 {
    if point_inside(p, &poly.p) != 0 {
        return 0.0;
    }
    let n = poly.p.len();
    let mut result = lseg_closept_point(&seg(poly.p[0], poly.p[n - 1]), p).0;
    for i in 0..n - 1 {
        let d = lseg_closept_point(&seg(poly.p[i], poly.p[i + 1]), p).0;
        if f8_lt(d, result) {
            result = d;
        }
    }
    result
}

/// The circle a polygon makes: centered on its points' mean, the radius their mean distance from it.
fn poly_to_circle(p: &[Pt]) -> Circ {
    let n = p.len() as f64;
    let mut c = Pt { x: 0.0, y: 0.0 };
    for q in p {
        c = pt_add(c, *q);
    }
    c = Pt { x: div(c.x, n), y: div(c.y, n) };
    let mut r = 0.0;
    for q in p {
        r = pl(r, pt_dt(*q, c));
    }
    Circ { c, r: div(r, n) }
}

// ---------------------------------------------------------------- circles

fn circle_ar(c: &Circ) -> f64 {
    mul(mul(c.r, c.r), std::f64::consts::PI)
}

/// A polygon of `npts` points on the circle.
fn circle_poly(npts: i64, c: &Circ) -> Result<Vec<Pt>> {
    if fp_zero(c.r) {
        return err("0A000", "cannot convert circle with radius zero to polygon");
    }
    if npts < 2 {
        return err("22023", "must request at least 2 points");
    }
    if npts > i64::from(i32::MAX) / 16 {
        return err("54000", "too many points requested");
    }
    let step = div(2.0 * std::f64::consts::PI, npts as f64);
    Ok((0..npts)
        .map(|i| {
            let angle = mul(step, i as f64);
            Pt { x: mi(c.c.x, mul(c.r, angle.cos())), y: pl(c.c.y, mul(c.r, angle.sin())) }
        })
        .collect())
}

fn circ_dist(d: f64, r: f64) -> f64 {
    let v = mi(d, r);
    if v < 0.0 { 0.0 } else { v }
}

// ---------------------------------------------------------------- the operators

pub(super) fn geo_type(t: Type) -> bool {
    matches!(t, Type::Point | Type::Lseg | Type::Line | Type::Box | Type::Path | Type::Polygon | Type::Circle)
}

/// What `a op b` gives for operands of these types, as pg_operator has it; none when there is no such operator.
pub(super) fn op_result(op: &str, ta: Type, tb: Type) -> Option<Type> {
    use Type::*;
    let (b, f) = (Some(Bool), Some(Float));
    let arith = |t: Type| matches!(op, "+" | "-" | "*" | "/").then_some(t);
    match (ta, tb) {
        (Point, Point) => match op {
            "<<" | ">>" | ">^" | "|>>" | "<^" | "<<|" | "~=" | "<>" | "?-" | "?|" => b,
            "<->" => f,
            _ => arith(Point),
        },
        (Point, Box | Line | Lseg) => match op {
            "<@" => b,
            "<->" => f,
            "##" => Some(Point),
            _ => None,
        },
        (Point, Path | Polygon | Circle) => match op {
            "<@" => b,
            "<->" => f,
            _ => None,
        },
        (Box | Path | Polygon | Circle, Point) => match op {
            "@>" => b,
            "<->" => f,
            _ if ta == Polygon => None,
            _ => arith(ta),
        },
        (Line | Lseg, Point) | (Box, Lseg) | (Circle, Polygon) | (Polygon, Circle) => (op == "<->").then_some(Float),
        (Box, Box) => match op {
            "<<" | "&<" | "&>" | ">>" | "<@" | "@>" | "~=" | "&&" | ">=" | ">" | "=" | "<" | "<=" | ">^" | "<^"
            | "?#" | "<<|" | "&<|" | "|&>" | "|>>" => b,
            "#" => Some(Box),
            "<->" => f,
            _ => None,
        },
        (Lseg, Lseg) => match op {
            "?#" | "?||" | "?-|" | "=" | "<>" | "<" | "<=" | ">" | ">=" => b,
            "#" | "##" => Some(Point),
            "<->" => f,
            _ => None,
        },
        (Lseg, Line) => match op {
            "?#" | "<@" => b,
            "<->" => f,
            _ => None,
        },
        (Line, Lseg) => match op {
            "<->" => f,
            "##" => Some(Point),
            _ => None,
        },
        (Lseg, Box) => match op {
            "?#" | "<@" => b,
            "<->" => f,
            "##" => Some(Point),
            _ => None,
        },
        (Line, Box) => (op == "?#").then_some(Bool),
        (Line, Line) => match op {
            "?#" | "?||" | "?-|" | "=" => b,
            "#" => Some(Point),
            "<->" => f,
            _ => None,
        },
        (Path, Path) => match op {
            "=" | "<" | ">" | "<=" | ">=" | "?#" => b,
            "+" => Some(Path),
            "<->" => f,
            _ => None,
        },
        (Polygon, Polygon) => match op {
            "<<" | "&<" | "&>" | ">>" | "<@" | "@>" | "~=" | "&&" | "<<|" | "&<|" | "|&>" | "|>>" => b,
            "<->" => f,
            _ => None,
        },
        (Circle, Circle) => match op {
            "=" | "<>" | "<" | ">" | "<=" | ">=" | "<<" | "&<" | "&>" | ">>" | "<@" | "@>" | "~=" | "&&" | "|>>"
            | "<<|" | "&<|" | "|&>" => b,
            "<->" => f,
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn no_op(op: &str, a: Type, b: Type) -> SqlError {
    SqlError {
        code: "42883",
        message: format!("operator does not exist: {} {op} {}", a.name(), b.name()),
        info: Some(Box::new(super::Violation {
            hint: Some(
                "No operator matches the given name and argument types. You might need to add explicit type casts."
                    .into(),
            ),
            ..Default::default()
        })),
    }
}

/// A geometric value, read.
#[derive(Clone, Debug)]
enum G {
    P(Pt),
    S(Seg),
    L(Ln),
    B(Bx),
    Pa(Pth),
    Po(Poly),
    C(Circ),
}

fn read(t: Type, s: &str) -> Result<G> {
    Ok(match t {
        Type::Point => G::P(parse_point(s)?),
        Type::Lseg => G::S(parse_lseg(s)?),
        Type::Line => G::L(parse_line(s)?),
        Type::Box => G::B(parse_box(s)?),
        Type::Path => G::Pa(parse_path(s)?),
        Type::Polygon => G::Po(read_poly(s)?),
        Type::Circle => G::C(parse_circle(s)?),
        _ => return err("42804", format!("{} is not a geometric type", t.name())),
    })
}

fn tp(p: Pt) -> Value {
    Value::Text(out_point(p))
}

fn tbox(b: Bx) -> Value {
    Value::Text(out_box(&b))
}

fn tcirc(c: Circ) -> Value {
    Value::Text(out_circle(&c))
}

/// A closest point, or none when its distance is NaN.
fn closest((d, p): (f64, Pt)) -> Value {
    if d.is_nan() { Value::Null } else { tp(p) }
}

/// `a op b` for geometric operands of types `ta` and `tb`.
pub(super) fn operate(op: &str, ta: Type, tb: Type, a: &str, b: &str) -> Result<Value> {
    if op_result(op, ta, tb).is_none() {
        return Err(no_op(op, ta, tb));
    }
    let (x, y) = (read(ta, a)?, read(tb, b)?);
    guarded(|| Ok(apply(op, &x, &y)))
}

fn apply(op: &str, x: &G, y: &G) -> Value {
    use self::G::*;
    use super::Value::{Bool as B_, Float as F_};
    match (x, y) {
        (P(p), P(q)) => {
            let (p, q) = (*p, *q);
            match op {
                "<<" => B_(fp_lt(p.x, q.x)),
                ">>" => B_(fp_gt(p.x, q.x)),
                ">^" | "|>>" => B_(fp_gt(p.y, q.y)),
                "<^" | "<<|" => B_(fp_lt(p.y, q.y)),
                "~=" => B_(pt_eq(p, q)),
                "<>" => B_(!pt_eq(p, q)),
                "?-" => B_(fp_eq(p.y, q.y)),
                "?|" => B_(fp_eq(p.x, q.x)),
                "<->" => F_(pt_dt(p, q)),
                "+" => tp(pt_add(p, q)),
                "-" => tp(pt_sub(p, q)),
                "*" => tp(pt_mul(p, q)),
                "/" => tp(pt_div(p, q)),
                _ => Value::Null,
            }
        }
        (P(p), B(b)) => match op {
            "<@" => B_(box_contain_point(b, *p)),
            "<->" => F_(box_closept_point(b, *p).0),
            "##" => closest(box_closept_point(b, *p)),
            _ => Value::Null,
        },
        (B(b), P(p)) => {
            let p = *p;
            match op {
                "@>" => B_(box_contain_point(b, p)),
                "<->" => F_(box_closept_point(b, p).0),
                "+" => tbox(Bx { hi: pt_add(b.hi, p), lo: pt_add(b.lo, p) }),
                "-" => tbox(Bx { hi: pt_sub(b.hi, p), lo: pt_sub(b.lo, p) }),
                "*" => {
                    let (h, l) = (pt_mul(b.hi, p), pt_mul(b.lo, p));
                    tbox(box_construct(h, l))
                }
                "/" => {
                    let (h, l) = (pt_div(b.hi, p), pt_div(b.lo, p));
                    tbox(box_construct(h, l))
                }
                _ => Value::Null,
            }
        }
        (P(p), L(l)) => match op {
            "<@" => B_(line_contain_point(l, *p)),
            "<->" => F_(line_closept_point(l, *p).0),
            "##" => closest(line_closept_point(l, *p)),
            _ => Value::Null,
        },
        (L(l), P(p)) => F_(line_closept_point(l, *p).0),
        (P(p), S(s)) => match op {
            "<@" => B_(lseg_contain_point(s, *p)),
            "<->" => F_(lseg_closept_point(s, *p).0),
            "##" => closest(lseg_closept_point(s, *p)),
            _ => Value::Null,
        },
        (S(s), P(p)) => F_(lseg_closept_point(s, *p).0),
        (P(p), Pa(path)) => match op {
            "<@" => B_(on_ppath(*p, path)),
            _ => F_(dist_ppath(*p, path)),
        },
        (Pa(path), P(p)) => {
            let p = *p;
            match op {
                "@>" => B_(on_ppath(p, path)),
                "<->" => F_(dist_ppath(p, path)),
                "+" => path_map(path, |q| pt_add(q, p)),
                "-" => path_map(path, |q| pt_sub(q, p)),
                "*" => path_map(path, |q| pt_mul(q, p)),
                "/" => path_map(path, |q| pt_div(q, p)),
                _ => Value::Null,
            }
        }
        (P(p), Po(poly)) | (Po(poly), P(p)) => match op {
            "<@" | "@>" => B_(point_inside(*p, &poly.p) != 0),
            _ => F_(dist_ppoly(*p, poly)),
        },
        (P(p), C(c)) => match op {
            "<@" => B_(pt_dt(c.c, *p) <= c.r),
            _ => F_(circ_dist(pt_dt(*p, c.c), c.r)),
        },
        (C(c), P(p)) => {
            let p = *p;
            match op {
                "@>" => B_(pt_dt(c.c, p) <= c.r),
                "<->" => F_(circ_dist(pt_dt(p, c.c), c.r)),
                "+" => tcirc(Circ { c: pt_add(c.c, p), r: c.r }),
                "-" => tcirc(Circ { c: pt_sub(c.c, p), r: c.r }),
                "*" => {
                    let center = pt_mul(c.c, p);
                    tcirc(Circ { c: center, r: mul(c.r, hypot(p.x, p.y)) })
                }
                "/" => {
                    let center = pt_div(c.c, p);
                    tcirc(Circ { c: center, r: div(c.r, hypot(p.x, p.y)) })
                }
                _ => Value::Null,
            }
        }
        (B(a), B(b)) => match op {
            "<<" => B_(fp_lt(a.hi.x, b.lo.x)),
            "&<" => B_(fp_le(a.hi.x, b.hi.x)),
            "&>" => B_(fp_ge(a.lo.x, b.lo.x)),
            ">>" => B_(fp_gt(a.lo.x, b.hi.x)),
            "<@" => B_(box_contain_box(b, a)),
            "@>" => B_(box_contain_box(a, b)),
            "~=" => B_(pt_eq(a.hi, b.hi) && pt_eq(a.lo, b.lo)),
            "&&" | "?#" => B_(box_ov(a, b)),
            ">=" => B_(fp_ge(box_ar(a), box_ar(b))),
            ">" => B_(fp_gt(box_ar(a), box_ar(b))),
            "=" => B_(fp_eq(box_ar(a), box_ar(b))),
            "<" => B_(fp_lt(box_ar(a), box_ar(b))),
            "<=" => B_(fp_le(box_ar(a), box_ar(b))),
            ">^" => B_(fp_ge(a.lo.y, b.hi.y)),
            "<^" => B_(fp_le(a.hi.y, b.lo.y)),
            "<<|" => B_(fp_lt(a.hi.y, b.lo.y)),
            "&<|" => B_(fp_le(a.hi.y, b.hi.y)),
            "|&>" => B_(fp_ge(a.lo.y, b.lo.y)),
            "|>>" => B_(fp_gt(a.lo.y, b.hi.y)),
            "#" => {
                if !box_ov(a, b) {
                    return Value::Null;
                }
                tbox(Bx {
                    hi: Pt { x: f8_min(a.hi.x, b.hi.x), y: f8_min(a.hi.y, b.hi.y) },
                    lo: Pt { x: f8_max(a.lo.x, b.lo.x), y: f8_max(a.lo.y, b.lo.y) },
                })
            }
            "<->" => F_(pt_dt(box_cn(a), box_cn(b))),
            _ => Value::Null,
        },
        (S(a), S(b)) => match op {
            "?#" => B_(lseg_interpt_lseg(a, b).is_some()),
            "?||" => B_(fp_eq(lseg_sl(a), lseg_sl(b))),
            "?-|" => B_(fp_eq(lseg_sl(a), lseg_invsl(b))),
            "=" => B_(pt_eq(a.a, b.a) && pt_eq(a.b, b.b)),
            "<>" => B_(!pt_eq(a.a, b.a) || !pt_eq(a.b, b.b)),
            "<" => B_(fp_lt(lseg_len(a), lseg_len(b))),
            "<=" => B_(fp_le(lseg_len(a), lseg_len(b))),
            ">" => B_(fp_gt(lseg_len(a), lseg_len(b))),
            ">=" => B_(fp_ge(lseg_len(a), lseg_len(b))),
            "#" => lseg_interpt_lseg(a, b).map_or(Value::Null, tp),
            "##" => {
                if lseg_sl(a) == lseg_sl(b) {
                    return Value::Null;
                }
                closest(lseg_closept_lseg(b, a))
            }
            "<->" => F_(lseg_closept_lseg(a, b).0),
            _ => Value::Null,
        },
        (S(s), L(l)) => match op {
            "?#" => B_(lseg_interpt_line(s, l).is_some()),
            "<@" => B_(line_contain_point(l, s.a) && line_contain_point(l, s.b)),
            _ => F_(lseg_closept_line(s, l).0),
        },
        (L(l), S(s)) => match op {
            "##" => {
                if lseg_sl(s) == line_sl(l) {
                    return Value::Null;
                }
                closest(lseg_closept_line(s, l))
            }
            _ => F_(lseg_closept_line(s, l).0),
        },
        (S(s), B(b)) => match op {
            "?#" => B_(box_interpt_lseg(b, s, false).is_some()),
            "<@" => B_(box_contain_lseg(b, s)),
            "<->" => F_(box_closept_lseg(b, s).0),
            "##" => closest(box_closept_lseg(b, s)),
            _ => Value::Null,
        },
        (B(b), S(s)) => F_(box_closept_lseg(b, s).0),
        (L(l), B(b)) => B_(inter_lb(l, b)),
        (L(a), L(b)) => match op {
            "?#" => B_(line_interpt_line(a, b).is_some()),
            "?||" => B_(line_interpt_line(a, b).is_none()),
            "?-|" => B_(line_perp(a, b)),
            "=" => B_(line_eq(a, b)),
            "#" => line_interpt_line(a, b).map_or(Value::Null, tp),
            "<->" => F_(line_distance(a, b)),
            _ => Value::Null,
        },
        (Pa(a), Pa(b)) => {
            let (n, m) = (a.pts.len(), b.pts.len());
            match op {
                "=" => B_(n == m),
                "<" => B_(n < m),
                ">" => B_(n > m),
                "<=" => B_(n <= m),
                ">=" => B_(n >= m),
                "?#" => B_(path_inter(a, b)),
                "+" => {
                    if a.closed || b.closed {
                        return Value::Null;
                    }
                    let pts = a.pts.iter().chain(&b.pts).copied().collect();
                    Value::Text(out_path(&Pth { closed: a.closed, pts }))
                }
                "<->" => path_distance(a, b),
                _ => Value::Null,
            }
        }
        (Po(a), Po(b)) => match op {
            "<<" => B_(a.bb.hi.x < b.bb.lo.x),
            "&<" => B_(a.bb.hi.x <= b.bb.hi.x),
            "&>" => B_(a.bb.lo.x >= b.bb.lo.x),
            ">>" => B_(a.bb.lo.x > b.bb.hi.x),
            "<<|" => B_(a.bb.hi.y < b.bb.lo.y),
            "&<|" => B_(a.bb.hi.y <= b.bb.hi.y),
            "|&>" => B_(a.bb.lo.y >= b.bb.lo.y),
            "|>>" => B_(a.bb.lo.y > b.bb.hi.y),
            "<@" => B_(poly_contain_poly(b, a)),
            "@>" => B_(poly_contain_poly(a, b)),
            "~=" => B_(a.p.len() == b.p.len() && plist_same(&a.p, &b.p)),
            "&&" => B_(poly_overlap(a, b)),
            "<->" => F_(poly_distance(a, b)),
            _ => Value::Null,
        },
        (C(a), C(b)) => match op {
            "=" => B_(fp_eq(circle_ar(a), circle_ar(b))),
            "<>" => B_(fp_ne(circle_ar(a), circle_ar(b))),
            "<" => B_(fp_lt(circle_ar(a), circle_ar(b))),
            ">" => B_(fp_gt(circle_ar(a), circle_ar(b))),
            "<=" => B_(fp_le(circle_ar(a), circle_ar(b))),
            ">=" => B_(fp_ge(circle_ar(a), circle_ar(b))),
            "<<" => B_(fp_lt(pl(a.c.x, a.r), mi(b.c.x, b.r))),
            "&<" => B_(fp_le(pl(a.c.x, a.r), pl(b.c.x, b.r))),
            "&>" => B_(fp_ge(mi(a.c.x, a.r), mi(b.c.x, b.r))),
            ">>" => B_(fp_gt(mi(a.c.x, a.r), pl(b.c.x, b.r))),
            "<@" => B_(fp_le(pt_dt(a.c, b.c), mi(b.r, a.r))),
            "@>" => B_(fp_le(pt_dt(a.c, b.c), mi(a.r, b.r))),
            "~=" => B_(((a.r.is_nan() && b.r.is_nan()) || fp_eq(a.r, b.r)) && pt_eq(a.c, b.c)),
            "&&" => B_(fp_le(pt_dt(a.c, b.c), pl(a.r, b.r))),
            "|>>" => B_(fp_gt(mi(a.c.y, a.r), pl(b.c.y, b.r))),
            "<<|" => B_(fp_lt(pl(a.c.y, a.r), mi(b.c.y, b.r))),
            "&<|" => B_(fp_le(pl(a.c.y, a.r), pl(b.c.y, b.r))),
            "|&>" => B_(fp_ge(mi(a.c.y, a.r), mi(b.c.y, b.r))),
            "<->" => F_(circ_dist(pt_dt(a.c, b.c), pl(a.r, b.r))),
            _ => Value::Null,
        },
        (C(c), Po(poly)) | (Po(poly), C(c)) => F_(circ_dist(dist_ppoly(c.c, poly), c.r)),
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------- functions

/// An argument of a geometric function: a geometric value, a number, or other text.
enum Arg {
    G(G),
    N(f64),
    T,
}

fn arg(t: Type, v: &Value) -> Result<Arg> {
    if geo_type(t) {
        return Ok(Arg::G(read(t, &v.to_text())?));
    }
    Ok(match v {
        Value::Int(n) => Arg::N(*n as f64),
        Value::Float(f) => Arg::N(*f),
        Value::Numeric(d) => Arg::N(d.to_f64()),
        v => v.to_text().trim().parse().map_or(Arg::T, Arg::N),
    })
}

/// A geometric function by its name (`center(x)`, `area(x)`, ..., and the prefix operators `@@`, `@-@`, `#`,
/// `?-`, `?|`), with its arguments' types.
pub(super) fn function(name: &str, tys: &[Type], a: &[Value]) -> Result<Value> {
    let mut args = vec![];
    for (i, v) in a.iter().enumerate() {
        args.push(arg(tys.get(i).copied().unwrap_or(Type::Text), v)?);
    }
    guarded(|| call(name, &args)).map_err(|e| {
        if e.code != "42883" || !e.message.is_empty() {
            return e;
        }
        let shown: Vec<String> = tys.iter().take(a.len()).map(|t| t.name().into_owned()).collect();
        let what = match name {
            "@@" | "@-@" | "#" | "?-" | "?|" => {
                return SqlError {
                    code: "42883",
                    message: format!("operator does not exist: {name} {}", shown.join(", ")),
                    info: Some(Box::new(super::Violation {
                        hint: Some("No operator matches the given name and argument type. You might need to add an explicit type cast.".into()),
                        ..Default::default()
                    })),
                };
            }
            n => n,
        };
        SqlError {
            code: "42883",
            message: format!("function {what}({}) does not exist", shown.join(", ")),
            info: Some(Box::new(super::Violation {
                hint: Some(
                    "No function matches the given name and argument types. You might need to add explicit type casts."
                        .into(),
                ),
                ..Default::default()
            })),
        }
    })
}

fn call(name: &str, args: &[Arg]) -> Result<Value> {
    use self::Arg::{G as A, N};
    use self::G::*;
    use super::Value::{Bool as B_, Float as F_};
    let txt = |s: String| -> Result<Value> { Ok(Value::Text(s)) };
    Ok(match (name, args) {
        ("area", [A(B(b))]) => F_(box_ar(b)),
        ("area", [A(Pa(p))]) => path_area(p),
        ("area", [A(C(c))]) => F_(circle_ar(c)),
        ("center" | "point" | "@@", [A(B(b))]) => tp(box_cn(b)),
        ("center" | "point" | "@@", [A(C(c))]) => tp(c.c),
        ("point" | "@@", [A(S(s))]) => tp(lseg_center(s)),
        ("point" | "poly_center" | "@@", [A(Po(p))]) => tp(poly_to_circle(&p.p).c),
        ("point", [N(x), N(y)]) => tp(Pt { x: *x, y: *y }),
        ("radius", [A(C(c))]) => F_(c.r),
        ("diameter", [A(C(c))]) => F_(mul(c.r, 2.0)),
        ("width", [A(B(b))]) => F_(box_wd(b)),
        ("height", [A(B(b))]) => F_(box_ht(b)),
        ("length" | "@-@", [A(S(s))]) => F_(lseg_len(s)),
        ("length" | "@-@", [A(Pa(p))]) => F_(path_length(p)),
        ("npoints" | "#", [A(Pa(p))]) => Value::Int(p.pts.len() as i64),
        ("npoints" | "#", [A(Po(p))]) => Value::Int(p.p.len() as i64),
        ("isopen", [A(Pa(p))]) => B_(!p.closed),
        ("isclosed", [A(Pa(p))]) => B_(p.closed),
        ("popen", [A(Pa(p))]) => return txt(out_path(&Pth { closed: false, pts: p.pts.clone() })),
        ("pclose", [A(Pa(p))]) => return txt(out_path(&Pth { closed: true, pts: p.pts.clone() })),
        ("slope", [A(P(p)), A(P(q))]) => F_(pt_sl(*p, *q)),
        ("isvertical", [A(P(p)), A(P(q))]) => B_(fp_eq(p.x, q.x)),
        ("ishorizontal", [A(P(p)), A(P(q))]) => B_(fp_eq(p.y, q.y)),
        ("isvertical" | "?|", [A(S(s))]) => B_(fp_eq(s.a.x, s.b.x)),
        ("ishorizontal" | "?-", [A(S(s))]) => B_(fp_eq(s.a.y, s.b.y)),
        ("isvertical" | "?|", [A(L(l))]) => B_(fp_zero(l.b)),
        ("ishorizontal" | "?-", [A(L(l))]) => B_(fp_zero(l.a)),
        ("isparallel", [A(S(x)), A(S(y))]) => B_(fp_eq(lseg_sl(x), lseg_sl(y))),
        ("isperp", [A(S(x)), A(S(y))]) => B_(fp_eq(lseg_sl(x), lseg_invsl(y))),
        ("isparallel", [A(L(x)), A(L(y))]) => B_(line_interpt_line(x, y).is_none()),
        ("isperp", [A(L(x)), A(L(y))]) => B_(line_perp(x, y)),
        ("bound_box", [A(B(x)), A(B(y))]) => tbox(Bx {
            hi: Pt { x: f8_max(x.hi.x, y.hi.x), y: f8_max(x.hi.y, y.hi.y) },
            lo: Pt { x: f8_min(x.lo.x, y.lo.x), y: f8_min(x.lo.y, y.lo.y) },
        }),
        ("diagonal" | "lseg", [A(B(b))]) => return txt(out_lseg(&seg(b.hi, b.lo))),
        ("box", [A(P(p)), A(P(q))]) => tbox(box_construct(*p, *q)),
        ("box", [A(P(p))]) => tbox(Bx { hi: *p, lo: *p }),
        ("box", [A(Po(p))]) => tbox(p.bb),
        ("box", [A(C(c))]) => tbox(circle_box(c)),
        ("lseg", [A(P(p)), A(P(q))]) => return txt(out_lseg(&seg(*p, *q))),
        ("circle", [A(P(p)), N(r)]) => tcirc(Circ { c: *p, r: *r }),
        ("circle", [A(Po(p))]) => tcirc(poly_to_circle(&p.p)),
        ("circle", [A(B(b))]) => tcirc(box_circle(b)),
        ("polygon", [A(B(b))]) => return txt(out_polygon(&box_poly(b))),
        ("polygon", [A(Pa(p))]) => {
            if !p.closed {
                return err("22023", "open path cannot be converted to polygon");
            }
            return txt(out_polygon(&p.pts));
        }
        ("polygon", [A(C(c))]) => return txt(out_polygon(&circle_poly(12, c)?)),
        ("polygon", [N(n), A(C(c))]) => return txt(out_polygon(&circle_poly(*n as i64, c)?)),
        ("path", [A(Po(p))]) => return txt(out_path(&Pth { closed: true, pts: p.p.clone() })),
        ("line", [A(P(p)), A(P(q))]) => {
            if pt_eq(*p, *q) {
                return err("22023", "invalid line specification: must be two distinct points");
            }
            return txt(out_line(&line_construct(*p, pt_sl(*p, *q))));
        }
        // regress.so's interpt_pp: where the first segment of the first path that meets one of the second's (in
        // order) meets it; NULL if none does
        ("__interpt_pp", [A(Pa(p)), A(Pa(q))]) => {
            for a in p.pts.windows(2) {
                for b in q.pts.windows(2) {
                    if let Some(x) = lseg_interpt_lseg(&seg(a[0], a[1]), &seg(b[0], b[1])) {
                        return Ok(tp(x));
                    }
                }
            }
            Value::Null
        }
        _ => return Err(SqlError { code: "42883", message: String::new(), info: None }),
    })
}

/// The function that converts a value of type `from` to type `to` (pg_cast's), by its name.
pub(super) fn cast_function(from: Type, to: Type) -> Option<&'static str> {
    use Type::*;
    Some(match (from, to) {
        (Point, Box) => "box",
        (Lseg, Point) | (Box, Point) | (Polygon, Point) | (Circle, Point) => "point",
        (Path, Polygon) | (Box, Polygon) | (Circle, Polygon) => "polygon",
        (Box, Lseg) => "lseg",
        (Box, Circle) | (Polygon, Circle) => "circle",
        (Polygon, Path) => "path",
        (Polygon, Box) | (Circle, Box) => "box",
        _ => return None,
    })
}

/// `x[i]` of a point (its x, then y), a line (A, B, C), an lseg (its two ends) or a box (its upper right corner,
/// then its lower left), which Postgres reads as fixed-length arrays counted from 0: null past the end. `ty` is the
/// type's id (the binder's; see `Binder::go`).
pub(super) fn subscript(x: &Value, i: &Value, ty: &Value) -> Result<Value> {
    let Value::Int(oid) = ty else { return Ok(Value::Null) };
    if *x == Value::Null || *i == Value::Null {
        return Ok(Value::Null);
    }
    let i = match i {
        Value::Int(n) => *n,
        v => match v.to_text().trim().parse::<i64>() {
            Ok(n) => n,
            Err(_) => return err("22P02", format!("invalid input syntax for type integer: \"{}\"", v.to_text())),
        },
    };
    let s = x.to_text();
    let at = usize::try_from(i).ok();
    let pick = |n: usize| at.filter(|&k| k < n);
    Ok(match Type::from_oid(*oid as u32) {
        Some(Type::Point) => {
            let p = parse_point(&s)?;
            pick(2).map_or(Value::Null, |k| Value::Float([p.x, p.y][k]))
        }
        Some(Type::Line) => {
            let l = parse_line(&s)?;
            pick(3).map_or(Value::Null, |k| Value::Float([l.a, l.b, l.c][k]))
        }
        Some(Type::Lseg) => {
            let g = parse_lseg(&s)?;
            pick(2).map_or(Value::Null, |k| Value::Text(out_point([g.a, g.b][k])))
        }
        Some(Type::Box) => {
            let b = parse_box(&s)?;
            pick(2).map_or(Value::Null, |k| Value::Text(out_point([b.hi, b.lo][k])))
        }
        _ => Value::Null,
    })
}
