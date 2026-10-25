//! B-tree indexes (`CREATE INDEX`): one hidden row per indexed row, keyed so that key order is
//! the column values' SQL order: `_sonos_idx/<table>/<index>/<value><value>...\u{1}<row id>`,
//! holding the row id. They live in the branch like any row, so forks share them and crash
//! recovery rebuilds them. A range condition is one ordered read of these keys.
//!
//! Merges never merge entries: a row can win while its entries would lose, so the merge rebuilds
//! the entries of the rows it changed (`sql::reindex`). Readers still fetch and recheck each row.

use super::collation::Coll;
use super::{Table, Type, Value, index_holds, index_value, position};

pub(super) const IDX: &str = "_sonos_idx";

/// Above every character a key holds: `x` then END sorts after every key that starts with `x`.
const END: char = '\u{10ffff}';

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Index {
    pub name: String,
    pub cols: Vec<u32>,         // column ids, in index order; EXPR for the next of `exprs`
    pub exprs: Vec<String>,     // expression parts' SQL, in order (`lower(email)`)
    pub filter: Option<String>, // a partial index's WHERE: only rows it holds for have entries
    /// GIN on a tsvector: one entry per lexeme of its one part, `...<lexeme>\u{1}<row id>`
    pub gin: bool,
    /// pg_trgm's operator class (`gin_trgm_ops`, `gist_trgm_ops`) on a text: with `gin`, one entry per trigram of
    /// its one part (as `show_trgm` writes it) instead of per lexeme, for `%` and `LIKE '%...%'` (see
    /// `sql::trgm_access`)
    pub trgm: Option<String>,
    /// each part's collation, in order (none: every part's is the default); a part of text in an ICU collation
    /// holds its key in that collation (see `collation::key`), so entries are in the collation's order
    pub colls: Vec<Coll>,
    // What CREATE INDEX said that changes no entry: kept for the catalog (pg_class.relam, pg_index, pg_get_indexdef)
    // and the index property functions. Each is absent from an index kept before they were, which reads as it did.
    /// the access method as written (btree, hash, gist, spgist, gin, brin); None: btree, or for an index kept
    /// before this was, what `catalog::describe` works out from `gin` and `trgm`
    pub am: Option<String>,
    /// each part's pg_index.indoption: `DESC` (1) and `NULLS FIRST` (2); none: every part's is 0, ascending with
    /// nulls last. Entries are kept ascending whatever this says: a scan reads them backward for a DESC
    pub opts: Vec<i16>,
    /// INCLUDE's columns (ids), after the key parts in pg_index.indkey; no entry holds their values
    pub include: Vec<u32>,
    /// each part's operator class as written (`text_pattern_ops`), without a schema; none: no part names one
    pub classes: Vec<Option<String>>,
    /// for an index of an access method CREATE ACCESS METHOD made (`am`), the built-in one its handler is, which it
    /// is kept as and does as (gist for gist2 over gisthandler: see `accessmethod`)
    pub builtin: Option<String>,
}

/// pg_index.indoption's bits (Postgres's pg_index.h).
pub(super) const INDOPTION_DESC: i16 = 1;
pub(super) const INDOPTION_NULLS_FIRST: i16 = 2;

impl Index {
    /// Part `k`'s collation.
    pub(super) fn coll(&self, k: usize) -> Coll {
        self.colls.get(k).copied().unwrap_or(Coll::DEFAULT)
    }

    /// Its access method: as written, or for an index kept before that was, what its entries say.
    pub(super) fn method(&self) -> &str {
        match (&self.am, &self.trgm) {
            (Some(am), _) => am,
            (None, Some(class)) if class.starts_with("gist") => "gist",
            _ if self.gin => "gin",
            _ => "btree",
        }
    }

    /// The built-in access method it does as: its own, or for one CREATE ACCESS METHOD made, its handler's.
    pub(super) fn kind(&self) -> &str {
        self.builtin.as_deref().unwrap_or_else(|| self.method())
    }

    /// Part `k`'s indoption.
    pub(super) fn opt(&self, k: usize) -> i16 {
        self.opts.get(k).copied().unwrap_or(0)
    }

    /// Part `k`'s operator class as written, if it names one.
    pub(super) fn class(&self, k: usize) -> Option<&str> {
        self.classes.get(k).and_then(Option::as_deref)
    }

    /// Part `k`'s operator class as written, or for a trigram index kept before classes were, its one part's.
    pub(super) fn said_class(&self, k: usize) -> Option<&str> {
        self.class(k).or(self.trgm.as_deref().filter(|_| k == 0 && self.classes.is_empty()))
    }

    /// An index like `self` with what CREATE INDEX said beyond its entries (its access method, ordering, INCLUDE
    /// columns and operator classes) as `from` has them: a partition's copy of its parent's index, whose INCLUDE
    /// columns are its own ids (`include`).
    pub(super) fn said_as(mut self, from: &Index, include: Vec<u32>) -> Index {
        self.am = from.am.clone();
        self.builtin = from.builtin.clone();
        self.opts = from.opts.clone();
        self.include = include;
        self.classes = from.classes.clone();
        self
    }
}

/// In `Index::cols`: an expression part.
pub(super) const EXPR: u32 = u32::MAX;

pub(super) fn prefix(table: &str, index: &str) -> String {
    format!("{IDX}/{table}/{index}/")
}

/// Appends one value in a form whose character order is SQL's order for its type. Non-null
/// values start with \u{2} and a null is \u{3}, so nulls sort last, as in Postgres. Every form is
/// prefix-free (fixed width, or text ended by \0), so several columns concatenate safely.
/// None: the value can't be indexed (JSON).
pub(super) fn enc(out: &mut String, v: &Value) -> Option<()> {
    use std::fmt::Write;
    let tag = match v {
        Value::Null => {
            out.push('\u{3}');
            return Some(());
        }
        Value::Json(_) | Value::JsonText(_) | Value::Vector(_) => return None,
        _ => '\u{2}',
    };
    out.push(tag);
    let int = |out: &mut String, n: i64| write!(out, "{:016x}", (n as u64) ^ (1 << 63)).ok();
    match v {
        Value::Int(n) | Value::Reg(n, _) | Value::Timestamp(n) | Value::TimestampTz(n) | Value::Time(n) => {
            int(out, *n)?
        }
        Value::Date(d) => int(out, *d as i64)?,
        Value::Interval(i) => write!(out, "{:032x}", (i.total() as u128) ^ (1 << 127)).ok()?,
        // the whole part (rounded down) in order, then the fraction's digits, which order as text; \0 ends
        // them (below every digit, so 0.5 < 0.51); see `Dec::order_key`
        Value::Numeric(d) => d.order_key(out)?,
        Value::Float(f) => {
            // IEEE order: flip every bit of negatives, only the sign bit of the rest
            let b = super::fcanon(*f).to_bits();
            write!(out, "{:016x}", if b >> 63 == 1 { !b } else { b | 1 << 63 }).ok()?
        }
        Value::Bool(b) => out.push(if *b { '1' } else { '0' }),
        // the hex digits order as the bytes do; \0 ends them
        Value::Bytea(b) => {
            super::bytes::hex_into(b, out);
            out.push('\0');
        }
        // the stored form is memcmp-ordered like the type (see `net::encode`)
        Value::Net(n) => {
            super::bytes::hex_into(&super::net::encode(n), out);
            out.push('\0');
        }
        Value::Text(s) => {
            // \0 ends the text, so the two characters below \u{2} are escaped, keeping order
            for c in s.chars() {
                match c {
                    '\0' => out.push_str("\u{1}\u{1}"),
                    '\u{1}' => out.push_str("\u{1}\u{2}"),
                    c => out.push(c),
                }
            }
            out.push('\0');
        }
        Value::Null | Value::Json(_) | Value::JsonText(_) | Value::Vector(_) => unreachable!(),
    }
    Some(())
}

/// `enc` for a part of collation `c`: text in an ICU collation as its key in it (which `enc` writes as it writes
/// bytea: hex digits ending in \0, so `skip` steps over it as over text). None: as `enc`.
pub(super) fn enc_in(out: &mut String, v: &Value, c: Coll) -> Option<()> {
    match v {
        // (in a nondeterministic collation: the key the text is equal by, so equal strings have one entry's values)
        Value::Text(s) if c.nondeterministic() => enc(out, &Value::Bytea(super::collation::eq_key(c, s).ok()?)),
        Value::Text(s) if c.icu() => enc(out, &Value::Bytea(super::collation::key(c, s).ok()?)),
        v => enc(out, v),
    }
}

/// `s` past its first encoded value of type `ty`: where the next one starts.
pub(super) fn skip(s: &str, ty: Type) -> Option<&str> {
    if let Some(rest) = s.strip_prefix('\u{3}') {
        return Some(rest); // a null
    }
    let r = s.strip_prefix('\u{2}')?;
    match ty {
        Type::SmallInt
        | Type::Int
        | Type::BigInt
        | Type::Oid
        | Type::Float
        | Type::Float4
        | Type::Date
        | Type::Timestamp
        | Type::TimestampTz
        | Type::Time
        | Type::Reg(_) => r.get(16..),
        Type::Interval => r.get(32..),
        Type::Bool => r.get(1..),
        _ => r.find('\0').map(|i| &r[i + 1..]), // text and numeric end with \0, which nothing inside holds
    }
}

/// The number `s` starts with, if it's a non-null integer or float as `enc` writes them.
pub(super) fn number(s: &str, ty: Type) -> Option<Value> {
    let h = u64::from_str_radix(s.strip_prefix('\u{2}')?.get(..16)?, 16).ok()?;
    match ty {
        Type::Float | Type::Float4 => {
            Some(Value::Float(f64::from_bits(if h >> 63 == 1 { h & !(1 << 63) } else { !h })))
        }
        t if t.whole() => Some(Value::Int((h ^ (1 << 63)) as i64)),
        _ => None,
    }
}

/// The index entry for a row with values `vals` (the table's columns): None if a column is gone
/// or holds JSON.
/// No entry, too, for a row a partial index's WHERE doesn't hold for.
pub(super) fn entry(t: &Table, ix: &Index, vals: &[Value], id: &str) -> Option<String> {
    if let Some(f) = &ix.filter
        && !index_holds(t, f, vals)
    {
        return None;
    }
    if ix.gin {
        return None; // see `entries`
    }
    let mut k = prefix(&t.name, &ix.name);
    let mut exprs = ix.exprs.iter();
    for (n, c) in ix.cols.iter().enumerate() {
        match *c {
            EXPR => enc_in(&mut k, &index_value(t, exprs.next()?, vals)?, ix.coll(n))?,
            c => enc_in(&mut k, &vals[position(t, c)?], ix.coll(n))?,
        }
    }
    k.push('\u{1}');
    k.push_str(id);
    Some(k)
}

/// A row's entries in an index: one (or none), or with GIN one per lexeme.
pub(super) fn entries(t: &Table, ix: &Index, vals: &[Value], id: &str) -> Vec<String> {
    if !ix.gin {
        return entry(t, ix, vals, id).into_iter().collect();
    }
    if let Some(f) = &ix.filter
        && !index_holds(t, f, vals)
    {
        return vec![];
    }
    // a trigram index: its text's trigrams
    if ix.trgm.is_some() {
        let v = match (ix.cols.first(), ix.exprs.first()) {
            (Some(&EXPR), Some(e)) => index_value(t, e, vals),
            (Some(&c), _) => position(t, c).map(|i| vals[i].clone()),
            _ => None,
        };
        let Some(Value::Text(s)) = v else { return vec![] };
        return super::textext::index_trigrams(&s)
            .iter()
            .filter_map(|g| lexeme_key(t, ix, g).map(|k| format!("{k}\u{1}{id}")))
            .collect();
    }
    let v = match (ix.cols.first(), ix.exprs.first()) {
        (Some(&EXPR), Some(e)) => index_value(t, e, vals),
        // (one on a jsonb or an array column holds no entries: an array is text too, and no tsvector)
        (Some(&c), _) => position(t, c).filter(|&i| t.cols[i].ty == Type::TsVector).map(|i| vals[i].clone()),
        _ => None,
    };
    let Some(Value::Text(tsv)) = v else { return vec![] };
    let Ok(tsv) = super::tsearch::TsVector::parse(&tsv) else { return vec![] };
    tsv.0.keys().filter_map(|lex| lexeme_key(t, ix, lex).map(|k| format!("{k}\u{1}{id}"))).collect()
}

/// Where a GIN index's entries for `lex` start: its rows follow, `\u{1}<row id>` each.
pub(super) fn lexeme_key(t: &Table, ix: &Index, lex: &str) -> Option<String> {
    let mut k = prefix(&t.name, &ix.name);
    enc(&mut k, &Value::Text(lex.into()))?;
    Some(k)
}

/// Where a GIN index's entries for every lexeme starting with `lex` are (`to_tsquery`'s `:*`).
pub(super) fn lexeme_prefix(t: &Table, ix: &Index, lex: &str) -> Option<String> {
    let mut k = lexeme_key(t, ix, lex)?;
    k.pop(); // the \0 that ends the text
    Some(k)
}

/// The keys a scan of an index reads, as `View::each` takes them: entries under `prefix` after
/// `after` (exclusive) up to `upto` (inclusive).
#[derive(Debug, PartialEq)]
pub(super) struct Span {
    pub prefix: String,
    pub after: Option<String>,
    pub upto: Option<String>,
}

/// Rows whose first index columns equal `eq` (non-null, in the columns' types) and whose next
/// column lies within `lo` and `hi` (value, inclusive?). None if a value can't be indexed.
pub(super) fn span(
    t: &Table,
    ix: &Index,
    eq: &[Value],
    lo: Option<(&Value, bool)>,
    hi: Option<(&Value, bool)>,
) -> Option<Span> {
    let mut p = prefix(&t.name, &ix.name);
    for (n, v) in eq.iter().enumerate() {
        enc_in(&mut p, v, ix.coll(n))?;
    }
    if lo.is_none() && hi.is_none() {
        return Some(Span { prefix: p, after: None, upto: None });
    }
    let ranged = ix.coll(eq.len());
    let at = |v: &Value| {
        let mut s = p.clone();
        enc_in(&mut s, v, ranged).map(|_| s)
    };
    let after = match lo {
        None => None,
        Some((v, true)) => Some(at(v)?), // the next key after it is the first entry for v
        Some((v, false)) => Some(format!("{}{END}", at(v)?)),
    };
    let upto = Some(match hi {
        None => format!("{p}\u{2}{END}"), // every value, but not the nulls after them
        Some((v, true)) => format!("{}{END}", at(v)?),
        Some((v, false)) => at(v)?,
    });
    Some(Span { prefix: p, after, upto })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(vals: &[Value]) -> String {
        let mut s = String::new();
        for v in vals {
            enc(&mut s, v).unwrap();
        }
        s
    }

    #[test]
    fn encoding_keeps_sql_order() {
        let ordered: Vec<Vec<Value>> = vec![
            vec![Value::Int(i64::MIN)],
            vec![Value::Int(-5)],
            vec![Value::Int(0)],
            vec![Value::Int(7)],
            vec![Value::Int(i64::MAX)],
            vec![Value::Null],
        ];
        for w in ordered.windows(2) {
            assert!(key(&w[0]) < key(&w[1]), "{:?} < {:?}", w[0], w[1]);
        }
        let floats = [f64::NEG_INFINITY, -2.5, 0.0, 1e-9, 3.0, f64::INFINITY, f64::NAN];
        for w in floats.windows(2) {
            assert!(key(&[Value::Float(w[0])]) < key(&[Value::Float(w[1])]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(key(&[Value::Float(-0.0)]), key(&[Value::Float(0.0)])); // equal, as in Postgres
        for f in [-2.5, 0.0, 1e300, -0.0, 7.25] {
            let k = key(&[Value::Float(f), Value::Int(-3)]);
            assert_eq!(number(&k, Type::Float), Some(Value::Float(f)));
            assert_eq!(number(skip(&k, Type::Float).unwrap(), Type::Int), Some(Value::Int(-3)));
        }
        let k = key(&[Value::Text("a\u{1}b".into()), Value::Null, Value::Float(1.5)]);
        let rest = skip(skip(&k, Type::Text).unwrap(), Type::Float).unwrap();
        assert_eq!(number(rest, Type::Float), Some(Value::Float(1.5)));
        let texts = ["", "\0", "\u{1}", "\u{2}", "a", "a\0", "ab", "b", "z\u{10ffff}", "é"];
        for w in texts.windows(2) {
            assert!(key(&[Value::Text(w[0].into())]) < key(&[Value::Text(w[1].into())]), "{:?} < {:?}", w[0], w[1]);
        }
        // several columns: the first decides, then the next
        let t = |s: &str, n: i64| key(&[Value::Text(s.into()), Value::Int(n)]);
        assert!(t("a", 9) < t("ab", 0));
        assert!(t("a", 1) < t("a", 2));
        assert!(key(&[Value::Text("a".into()), Value::Null]) > t("a", i64::MAX));
    }
}
