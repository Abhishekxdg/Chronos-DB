//! Declarative partitioning's model: what a table is partitioned by, the bound each partition
//! has, which partition a row belongs to, and how Postgres writes these down (`pg_get_partkeydef`,
//! `pg_get_expr(relpartbound, ..)`). The statements that use them are in `hier`.
//!
//! A partitioned table holds no rows; each row lives in a leaf partition, an ordinary table keyed
//! as any other. Everything here works on values already read from a row, so it is the same for
//! INSERT, for COPY and for an UPDATE that moves a row.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Strategy {
    Range,
    List,
    Hash,
}

impl Strategy {
    pub(super) fn name(self) -> &'static str {
        match self {
            Strategy::Range => "range",
            Strategy::List => "list",
            Strategy::Hash => "hash",
        }
    }

    /// pg_partitioned_table.partstrat
    pub(super) fn letter(self) -> char {
        match self {
            Strategy::Range => 'r',
            Strategy::List => 'l',
            Strategy::Hash => 'h',
        }
    }

    pub(super) fn from_name(s: &str) -> Option<Strategy> {
        [Strategy::Range, Strategy::List, Strategy::Hash].into_iter().find(|x| x.name() == s)
    }
}

/// One part of a partition key: a column (by id, so a rename follows) or an expression over the
/// table's columns, as SQL.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum KeyPart {
    Col(u32),
    Expr(String),
}

/// PARTITION BY strategy (parts).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PartKey {
    pub(super) strategy: Strategy,
    pub(super) parts: Vec<KeyPart>,
}

/// A key part ready to read from the rows of one table.
pub(super) enum KeyEval {
    Col(usize),
    Expr(Expr),
}

impl PartKey {
    pub(super) fn to_json(&self) -> Json {
        let parts: Vec<Json> = self
            .parts
            .iter()
            .map(|p| match p {
                KeyPart::Col(id) => serde_json::json!({ "col": id }),
                KeyPart::Expr(e) => serde_json::json!({ "expr": e }),
            })
            .collect();
        serde_json::json!({ "strategy": self.strategy.name(), "parts": parts })
    }

    pub(super) fn from_json(j: &Json) -> Option<PartKey> {
        let strategy = Strategy::from_name(j["strategy"].as_str()?)?;
        let parts = j["parts"]
            .as_array()?
            .iter()
            .map(|p| match (p["col"].as_u64(), p["expr"].as_str()) {
                (Some(id), _) => Some(KeyPart::Col(id as u32)),
                (_, Some(e)) => Some(KeyPart::Expr(e.into())),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        Some(PartKey { strategy, parts })
    }

    /// The key's parts bound to `t`'s columns.
    pub(super) fn compile(&self, t: &Table) -> Result<Vec<KeyEval>> {
        self.parts
            .iter()
            .map(|p| match p {
                KeyPart::Col(id) => position(t, *id).map(KeyEval::Col).ok_or_else(|| SqlError {
                    code: "XX001",
                    message: "a partition key column is gone".into(),
                    info: None,
                }),
                KeyPart::Expr(text) => index_expr(t, text).map(KeyEval::Expr),
            })
            .collect()
    }

    /// The type each part has.
    pub(super) fn types(&self, t: &Table) -> Result<Vec<Type>> {
        self.compile(t)?
            .iter()
            .map(|k| match k {
                KeyEval::Col(i) => Ok(t.cols[*i].ty),
                KeyEval::Expr(e) => ty_in(e, &t.cols, &[], &[]),
            })
            .collect()
    }

    /// How each part is shown in a bound: its type, and what its column says about its size.
    pub(super) fn shows(&self, t: &Table) -> Result<Vec<Shown>> {
        let types = self.types(t)?;
        Ok(self
            .compile(t)?
            .iter()
            .zip(types)
            .map(|(k, ty)| match k {
                KeyEval::Col(i) => Shown { ty, small: t.cols[*i].small, scale: t.cols[*i].numeric.map(|(_, s)| s) },
                KeyEval::Expr(_) => Shown { ty, small: false, scale: None },
            })
            .collect())
    }

    /// The names of the columns the key reads: the columns themselves and those its expressions name.
    pub(super) fn columns(&self, t: &Table) -> Vec<u32> {
        let mut out = vec![];
        for p in &self.parts {
            match p {
                KeyPart::Col(id) => out.push(*id),
                KeyPart::Expr(text) => {
                    if let Ok(e) = check_expr(text) {
                        walk(&e, &mut |x| {
                            if let Expr::Col(_, n) = x
                                && let Some(c) = t.cols.iter().find(|c| c.name == *n)
                            {
                                out.push(c.id);
                            }
                        });
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// `RANGE (a, (b + 1), lower(c))`, as pg_get_partkeydef writes it.
    pub(super) fn def(&self, t: &Table) -> String {
        let parts: Vec<String> = self
            .parts
            .iter()
            .map(|p| match p {
                KeyPart::Col(id) => t.cols.iter().find(|c| c.id == *id).map_or_else(String::new, |c| ident(&c.name)),
                KeyPart::Expr(text) => match check_expr(text).ok().and_then(|e| catalog::deparse_expr(&e, &t.cols)) {
                    Some((d, call)) if call => d,
                    Some((d, _)) => format!("({d})"),
                    None => format!("({text})"),
                },
            })
            .collect();
        format!("{} ({})", self.strategy.name().to_uppercase(), parts.join(", "))
    }
}

/// The values of a row's key (`vals` are `t`'s columns).
pub(super) fn row_key_values(t: &Table, compiled: &[KeyEval], vals: &[Value]) -> Result<Vec<Value>> {
    compiled
        .iter()
        .map(|k| match k {
            KeyEval::Col(i) => Ok(vals[*i].clone()),
            KeyEval::Expr(e) => eval(e, &Env { cols: &t.cols, row: vals, params: &[], aggs: &[], sub: None }),
        })
        .collect()
}

/// A key part's type and size, which decide how its values are shown in a bound.
#[derive(Clone, Copy, Debug)]
pub(super) struct Shown {
    pub(super) ty: Type,
    pub(super) small: bool,
    pub(super) scale: Option<i32>,
}

/// One end of a range bound, for one key column.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Edge {
    Min,
    Max,
    Val(Value),
}

/// A partition's bound: the rows it takes.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum PartBound {
    /// FROM (..) TO (..): from inclusive, to exclusive
    Range(Vec<Edge>, Vec<Edge>),
    /// IN (..), null among them or not
    List(Vec<Value>),
    /// WITH (MODULUS m, REMAINDER r)
    Hash(u32, u32),
    /// DEFAULT: every row no other partition takes
    Default,
}

impl PartBound {
    pub(super) fn to_json(&self) -> Json {
        let edge = |e: &Edge| match e {
            Edge::Min => serde_json::json!({ "min": true }),
            Edge::Max => serde_json::json!({ "max": true }),
            Edge::Val(v) => serde_json::json!({ "v": v.kept_json() }),
        };
        match self {
            PartBound::Range(f, t) => {
                serde_json::json!({ "range": [f.iter().map(edge).collect::<Vec<_>>(), t.iter().map(edge).collect::<Vec<_>>()] })
            }
            PartBound::List(vs) => serde_json::json!({ "list": vs.iter().map(Value::kept_json).collect::<Vec<_>>() }),
            PartBound::Hash(m, r) => serde_json::json!({ "hash": [m, r] }),
            PartBound::Default => serde_json::json!({ "default": true }),
        }
    }

    pub(super) fn from_json(j: &Json) -> Option<PartBound> {
        let edge = |e: &Json| {
            if e["min"] == true {
                Some(Edge::Min)
            } else if e["max"] == true {
                Some(Edge::Max)
            } else {
                e.get("v").map(|v| Edge::Val(Value::from_json(v)))
            }
        };
        if let Some(r) = j["range"].as_array() {
            let side = |x: &Json| x.as_array()?.iter().map(edge).collect::<Option<Vec<_>>>();
            return Some(PartBound::Range(side(r.first()?)?, side(r.get(1)?)?));
        }
        if let Some(l) = j["list"].as_array() {
            return Some(PartBound::List(l.iter().map(Value::from_json).collect()));
        }
        if let Some(h) = j["hash"].as_array() {
            return Some(PartBound::Hash(h.first()?.as_u64()? as u32, h.get(1)?.as_u64()? as u32));
        }
        (j["default"] == true).then_some(PartBound::Default)
    }

    /// The bound with its values taken as the key columns' types (see `coerce`).
    pub(super) fn typed(&self, types: &[Type]) -> Result<PartBound> {
        let edges = |es: &[Edge]| -> Result<Vec<Edge>> {
            es.iter()
                .zip(types)
                .map(|(e, ty)| match e {
                    Edge::Val(v) => Ok(Edge::Val(coerce(v.clone(), *ty)?)),
                    e => Ok(e.clone()),
                })
                .collect()
        };
        Ok(match self {
            PartBound::Range(f, t) => PartBound::Range(edges(f)?, edges(t)?),
            PartBound::List(vs) => match types.first() {
                Some(ty) => PartBound::List(
                    vs.iter()
                        .map(|v| if *v == Value::Null { Ok(Value::Null) } else { coerce(v.clone(), *ty) })
                        .collect::<Result<_>>()?,
                ),
                None => self.clone(),
            },
            b => b.clone(),
        })
    }

    /// `FOR VALUES FROM (1) TO (10)`, `DEFAULT`, ...: pg_get_expr(relpartbound, oid).
    pub(super) fn text(&self, shown: &[Shown]) -> String {
        let one = |v: &Value, s: &Shown| const_text(v, s);
        let edges = |es: &[Edge]| -> String {
            es.iter()
                .zip(shown)
                .map(|(e, s)| match e {
                    Edge::Min => "MINVALUE".to_string(),
                    Edge::Max => "MAXVALUE".to_string(),
                    Edge::Val(v) => one(v, s),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            PartBound::Range(f, t) => format!("FOR VALUES FROM ({}) TO ({})", edges(f), edges(t)),
            PartBound::List(vs) => {
                let s = shown.first().copied().unwrap_or(Shown { ty: Type::Text, small: false, scale: None });
                format!("FOR VALUES IN ({})", vs.iter().map(|v| one(v, &s)).collect::<Vec<_>>().join(", "))
            }
            PartBound::Hash(m, r) => format!("FOR VALUES WITH (modulus {m}, remainder {r})"),
            PartBound::Default => "DEFAULT".into(),
        }
    }

    /// Whether the key belongs to the partition: for a DEFAULT one, nothing is known here (it is
    /// what no other takes).
    pub(super) fn holds(&self, key: &[Value], types: &[Type]) -> Result<bool> {
        Ok(match self {
            PartBound::Range(f, t) => {
                !key.contains(&Value::Null) && cmp_row(key, f)? != Ordering::Less && cmp_row(key, t)? == Ordering::Less
            }
            PartBound::List(vs) => {
                let Some(k) = key.first() else { return Ok(false) };
                let mut found = false;
                for v in vs {
                    found |= match (k, v) {
                        (Value::Null, Value::Null) => true,
                        (Value::Null, _) | (_, Value::Null) => false,
                        (k, v) => compare(k, v)? == Ordering::Equal,
                    };
                }
                found
            }
            PartBound::Hash(m, r) => hash_key(key, types)? % u64::from(*m) == u64::from(*r),
            PartBound::Default => false,
        })
    }
}

/// A value as a bound shows it (Postgres's `get_const_expr`): an integer or numeric as it is unless
/// it is negative, a boolean as a word, null as NULL, anything else quoted.
fn const_text(v: &Value, s: &Shown) -> String {
    let quoted = |t: &str| format!("'{}'", t.replace('\'', "''"));
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) if s.ty == Type::Int && !s.small && *n >= 0 => n.to_string(),
        Value::Numeric(d) => {
            let t = match s.scale {
                Some(sc) => round_dec(d, sc).map_or_else(|_| d.to_string(), |r| r.to_string()),
                None => d.to_string(),
            };
            if t.starts_with('-') { quoted(&t) } else { t }
        }
        Value::Int(n) if s.ty == Type::Numeric && *n >= 0 => match s.scale {
            Some(sc) if sc > 0 => format!("{n}.{}", "0".repeat(sc as usize)),
            _ => n.to_string(),
        },
        v => quoted(&v.to_text()),
    }
}

/// A row's key against a bound's edges, as a row of values compares with a bound: column by
/// column, MINVALUE below every value and MAXVALUE above.
pub(super) fn cmp_row(key: &[Value], edges: &[Edge]) -> Result<Ordering> {
    for (k, e) in key.iter().zip(edges) {
        let o = match e {
            Edge::Min => Ordering::Greater,
            Edge::Max => Ordering::Less,
            Edge::Val(b) => compare(k, b)?,
        };
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// Two bounds' edges against each other.
pub(super) fn cmp_edges(a: &[Edge], b: &[Edge]) -> Result<Ordering> {
    for (x, y) in a.iter().zip(b) {
        let o = match (x, y) {
            (Edge::Min, Edge::Min) | (Edge::Max, Edge::Max) => Ordering::Equal,
            (Edge::Min, _) | (_, Edge::Max) => Ordering::Less,
            (Edge::Max, _) | (_, Edge::Min) => Ordering::Greater,
            (Edge::Val(p), Edge::Val(q)) => compare(p, q)?,
        };
        if o != Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// Edges as Postgres shows them in a message: `(1, 2)`.
pub(super) fn edges_text(es: &[Edge], shown: &[Shown]) -> String {
    let parts: Vec<String> = es
        .iter()
        .zip(shown)
        .map(|(e, s)| match e {
            Edge::Min => "MINVALUE".to_string(),
            Edge::Max => "MAXVALUE".to_string(),
            Edge::Val(v) => match (v, s.ty) {
                (Value::Text(t), _) => t.clone(),
                (v, _) => v.to_text(),
            },
        })
        .collect();
    format!("({})", parts.join(", "))
}

/// Whether two bounds of one table take a row in common (they are both of one strategy, and typed).
pub(super) fn overlaps(a: &PartBound, b: &PartBound) -> Result<bool> {
    Ok(match (a, b) {
        (PartBound::Range(f1, t1), PartBound::Range(f2, t2)) => {
            cmp_edges(f1, t2)? == Ordering::Less && cmp_edges(f2, t1)? == Ordering::Less
        }
        (PartBound::List(x), PartBound::List(y)) => {
            let mut any = false;
            for p in x {
                for q in y {
                    any |= match (p, q) {
                        (Value::Null, Value::Null) => true,
                        (Value::Null, _) | (_, Value::Null) => false,
                        (p, q) => compare(p, q)? == Ordering::Equal,
                    };
                }
            }
            any
        }
        (PartBound::Hash(m1, r1), PartBound::Hash(m2, r2)) => {
            let g = gcd(*m1, *m2);
            g != 0 && r1 % g == r2 % g
        }
        _ => false,
    })
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

// ---------------------------------------------------------------- hash partitioning

/// HASH_PARTITION_SEED: what Postgres seeds each key column's hash with.
const HASH_SEED: u64 = 0x7A5B_2236_7996_DCFD;

fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c) ^ c.rotate_left(4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a) ^ a.rotate_left(6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b) ^ b.rotate_left(8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c) ^ c.rotate_left(16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a) ^ a.rotate_left(19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b) ^ b.rotate_left(4);
    *b = b.wrapping_add(*a);
}

fn finish(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(14));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(11));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(25));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(16));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(4));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(14));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(24));
}

/// Postgres's `hash_bytes_extended` (Bob Jenkins's lookup3), which `hashtextextended` and the
/// other hash functions of a partition key's types call.
fn hash_bytes(k: &[u8], seed: u64) -> u64 {
    let len = k.len() as u32;
    let init = 0x9e37_79b9u32.wrapping_add(len).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        mix(&mut a, &mut b, &mut c);
    }
    let word = |s: &[u8]| {
        let mut w = [0u8; 4];
        w[..s.len().min(4)].copy_from_slice(&s[..s.len().min(4)]);
        u32::from_le_bytes(w)
    };
    let mut rest = k;
    while rest.len() >= 12 {
        a = a.wrapping_add(word(&rest[0..4]));
        b = b.wrapping_add(word(&rest[4..8]));
        c = c.wrapping_add(word(&rest[8..12]));
        mix(&mut a, &mut b, &mut c);
        rest = &rest[12..];
    }
    // the last 11 bytes: the lowest byte of c is left for the length
    a = a.wrapping_add(word(rest.get(0..4.min(rest.len())).unwrap_or(&[])));
    if rest.len() > 4 {
        b = b.wrapping_add(word(&rest[4..8.min(rest.len())]));
    }
    if rest.len() > 8 {
        c = c.wrapping_add(word(&rest[8..]) << 8);
    }
    finish(&mut a, &mut b, &mut c);
    (u64::from(b) << 32) | u64::from(c)
}

/// `hash_uint32_extended`: what the integer types' hash functions come to.
fn hash_u32(k: u32, seed: u64) -> u64 {
    let init = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        mix(&mut a, &mut b, &mut c);
    }
    a = a.wrapping_add(k);
    finish(&mut a, &mut b, &mut c);
    (u64::from(b) << 32) | u64::from(c)
}

/// `hashint8extended`: made to agree with the 32-bit one where the value fits in it.
fn hash_i64(v: i64, seed: u64) -> u64 {
    let (lo, hi) = (v as u32, (v >> 32) as u32);
    hash_u32(lo ^ if v >= 0 { hi } else { !hi }, seed)
}

/// Microseconds from Postgres's epoch (2000-01-01) to Unix's.
const PG_EPOCH_US: i64 = 946_684_800_000_000;

/// `hash_numeric_extended`: the value's base-10000 digits (leading and trailing zeros left out) hashed, with
/// the weight of the first mixed in.
fn hash_numeric(d: &decimal::Dec, seed: u64) -> u64 {
    // (NaN and the infinities hash to the seed itself, as Postgres's do)
    let Some((groups, weight)) = d.groups() else { return seed };
    if groups.is_empty() {
        return seed.wrapping_sub(1);
    }
    let bytes: Vec<u8> = groups.iter().flat_map(|g| g.to_le_bytes()).collect();
    hash_bytes(&bytes, seed) ^ (i64::from(weight) as u64)
}

/// One value's hash, as its type's hash function gives it (the type's own: a varchar's is text's).
fn hash_value(v: &Value, ty: Type, seed: u64) -> Result<u64> {
    Ok(match v {
        Value::Text(s) if ty == Type::Uuid => {
            let hex: String = s.chars().filter(|c| *c != '-').collect();
            let bytes: Vec<u8> =
                (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0)).collect();
            hash_bytes(&bytes, seed)
        }
        Value::Numeric(d) => hash_numeric(d, seed),
        Value::Int(n) if ty == Type::Numeric => hash_numeric(&decimal::Dec::int(*n), seed),
        Value::Interval(i) => {
            // (its 128-bit comparison value, cut to 64 bits)
            let span =
                i128::from(i.micros) + i128::from(i.months) * 30 * 86_400_000_000 + i128::from(i.days) * 86_400_000_000;
            hash_i64(span as i64, seed)
        }
        Value::Net(n) => match n {
            net::Net::Inet(a) | net::Net::Cidr(a) => {
                let mut b = vec![if a.v6 { 3 } else { 2 }, a.bits];
                if a.v6 {
                    b.extend(a.ip.to_be_bytes());
                } else {
                    b.extend((a.ip as u32).to_be_bytes());
                }
                hash_bytes(&b, seed)
            }
            net::Net::Mac(m) => hash_bytes(&m.to_be_bytes()[2..], seed),
            net::Net::Mac8(m) => hash_bytes(&m.to_be_bytes(), seed),
        },
        Value::Int(n) | Value::Reg(n, _) => match i32::try_from(*n) {
            // (int4 and int2 alike: both widen to the same 32 bits; int8 is made to agree)
            Ok(i) => hash_u32(i as u32, seed),
            Err(_) => hash_i64(*n, seed),
        },
        Value::Bool(b) => hash_u32(u32::from(*b), seed),
        Value::Text(s) => hash_bytes(s.as_bytes(), seed),
        Value::Bytea(b) => hash_bytes(b, seed),
        Value::Date(d) => hash_u32((i64::from(*d) - 10_957) as i32 as u32, seed),
        Value::Timestamp(t) | Value::TimestampTz(t) => hash_i64(*t - PG_EPOCH_US, seed),
        Value::Time(t) => hash_i64(*t, seed),
        Value::Float(f) => {
            if *f == 0.0 {
                seed
            } else {
                let bits = if f.is_nan() { 0x7ff8_0000_0000_0000u64 } else { f.to_bits() };
                hash_bytes(&bits.to_le_bytes(), seed)
            }
        }
        v => {
            return err(
                UNSUPPORTED,
                format!(
                    "hash partitioning on a {} column isn't supported yet",
                    v.ty().map_or("null".into(), Type::name)
                ),
            );
        }
    })
}

/// A row's hash for HASH partitioning: its key values' hashes, nulls left out, combined as
/// Postgres combines them, so a row lands in the partition Postgres's would take it to.
pub(super) fn hash_key(key: &[Value], types: &[Type]) -> Result<u64> {
    let mut row = 0u64;
    for (i, v) in key.iter().enumerate().filter(|(_, v)| **v != Value::Null) {
        let h = hash_value(v, types.get(i).copied().unwrap_or(Type::Text), HASH_SEED)?;
        row ^= h.wrapping_add(0x49a0_f4dd_15e5_a8e3).wrapping_add(row << 54).wrapping_add(row >> 7);
    }
    Ok(row)
}

// ---------------------------------------------------------------- where a row goes

/// A partition of a partitioned table, with its (typed) bound.
pub(super) struct Kid {
    pub(super) bound: PartBound,
}

/// Finds the partition a key belongs to, among one partitioned table's.
pub(super) struct Router {
    strategy: Strategy,
    pub(super) kids: Vec<Kid>,
    /// range: the kids by lower bound
    sorted: Vec<usize>,
    lists: HashMap<HKey, usize>,
    /// the types of the key's parts
    types: Vec<Type>,
    null_kid: Option<usize>,
    pub(super) default: Option<usize>,
}

impl Router {
    pub(super) fn new(strategy: Strategy, kids: Vec<Kid>, types: Vec<Type>) -> Result<Router> {
        let mut r = Router {
            strategy,
            sorted: vec![],
            lists: HashMap::new(),
            types,
            null_kid: None,
            default: kids.iter().position(|k| k.bound == PartBound::Default),
            kids,
        };
        for (i, k) in r.kids.iter().enumerate() {
            match &k.bound {
                PartBound::Range(..) => r.sorted.push(i),
                PartBound::List(vs) => {
                    for v in vs {
                        match v {
                            Value::Null => r.null_kid = Some(i),
                            v => {
                                r.lists.insert(hkey(v), i);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let kids = &r.kids;
        let mut failed = None;
        r.sorted.sort_by(|&a, &b| match (&kids[a].bound, &kids[b].bound) {
            (PartBound::Range(f1, _), PartBound::Range(f2, _)) => cmp_edges(f1, f2).unwrap_or_else(|e| {
                failed = Some(e);
                Ordering::Equal
            }),
            _ => Ordering::Equal,
        });
        match failed {
            Some(e) => Err(e),
            None => Ok(r),
        }
    }

    /// The index (in `kids`) of the partition the key goes to: the one whose bound takes it, else
    /// the default one; None when there is none.
    pub(super) fn find(&self, key: &[Value]) -> Result<Option<usize>> {
        let found = match self.strategy {
            Strategy::Range => {
                if key.contains(&Value::Null) {
                    None
                } else {
                    // the last partition starting at or before the key, and the key must be below its end
                    let (mut lo, mut hi) = (0, self.sorted.len());
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let PartBound::Range(f, _) = &self.kids[self.sorted[mid]].bound else { unreachable!() };
                        if cmp_row(key, f)? == Ordering::Less {
                            hi = mid;
                        } else {
                            lo = mid + 1;
                        }
                    }
                    match lo.checked_sub(1).map(|i| self.sorted[i]) {
                        Some(k) if self.kids[k].bound.holds(key, &self.types)? => Some(k),
                        _ => None,
                    }
                }
            }
            Strategy::List => match key.first() {
                Some(Value::Null) => self.null_kid,
                Some(v) => self.lists.get(&hkey(v)).copied(),
                None => None,
            },
            Strategy::Hash => {
                let h = hash_key(key, &self.types)?;
                self.kids
                    .iter()
                    .position(|k| matches!(k.bound, PartBound::Hash(m, r) if h % u64::from(m) == u64::from(r)))
            }
        };
        Ok(found.or(self.default))
    }
}
