//! A merge policy's row rules (`row_rules`): conditions each row a merge changes in a table must keep, read as a CHECK
//! reads its row, over the row as the world merged into has it now (`old.col`) and as the merge leaves it
//! (`new.col`). `payments: new.amount >= -50` holds refunds over 50 for a person; `prices: new.price <= old.price *
//! 1.2` holds a price raised by more than a fifth. A condition that comes out NULL keeps the rule, as a CHECK's does
//! (an inserted row's `old.*` and a deleted row's `new.*` are NULL): `new.id is not null` holds every delete.
//!
//! What's said is which rows broke a rule, by key, never their values: a reviewer reading the reasons reads no text
//! from the rows (see `MergePolicy::untrusted_columns`). A rule that can't be checked (a column the table doesn't
//! have, a value it can't read) holds the merge, saying why: a rule that can't run never lets a merge through.

use super::*;

/// How many rows' keys a reason names before saying how many more.
const SHOWN: usize = 5;

/// Whether `text` parses as a row rule's condition (as a CHECK's text does); the parser's message if not.
pub(crate) fn row_rule_parses(text: &str) -> std::result::Result<(), String> {
    check_expr(text).map(|_| ()).map_err(|e| e.message)
}

/// The reasons the rows a merge changes break `rules` (each `(table, condition)`), in words: one per rule broken.
/// `rows`: each changed row as `(table, id, before, after)`, its stored text in the world merged into now and as the
/// merge leaves it (None: not there), read with `v`'s definition of its table.
pub(crate) fn row_rule_reasons(
    v: &View,
    rules: &[(String, String)],
    rows: &[(String, String, Option<String>, Option<String>)],
) -> Vec<String> {
    let mut out = vec![];
    for (table, cond) in rules {
        let mine: Vec<&(String, String, Option<String>, Option<String>)> =
            rows.iter().filter(|r| &r.0 == table).collect();
        if mine.is_empty() {
            continue;
        }
        match broken_rows(v, table, cond, &mine) {
            Ok(ids) if ids.is_empty() => {}
            Ok(ids) => {
                let more = if ids.len() > SHOWN { format!(", and {} more", ids.len() - SHOWN) } else { String::new() };
                let shown: Vec<String> =
                    ids.iter().take(SHOWN).map(|id| crate::agents::shown_key(&format!("{table}/{id}"))).collect();
                let n = if ids.len() == 1 { "1 row".to_string() } else { format!("{} rows", ids.len()) };
                out.push(format!("changes {n} breaking row rule {table}: {cond} ({}{more})", shown.join(", ")));
            }
            Err(e) => out.push(format!(
                "changes {table}, whose row rule `{cond}` can't be checked ({}): a person looks",
                e.message
            )),
        }
    }
    out
}

/// The ids of `rows` (all of table `table`) that make condition `cond` false.
fn broken_rows(
    v: &View,
    table: &str,
    cond: &str,
    rows: &[&(String, String, Option<String>, Option<String>)],
) -> Result<Vec<String>> {
    let Some(t) = load_world(v, table)? else {
        return err(NO_TABLE, format!("relation \"{table}\" does not exist"));
    };
    // the row twice: as it was (`old`), then as the merge leaves it (`new`)
    let mut cols = Vec::with_capacity(t.cols.len() * 2);
    for side in ["old", "new"] {
        for c in &t.cols {
            let mut c = c.clone();
            c.table = side.into();
            cols.push(c);
        }
    }
    let e = Binder::plain(&cols, None).bind(&check_expr(cond)?)?;
    let mut out = vec![];
    for (_, id, before, after) in rows {
        let side = |raw: &Option<String>| -> Result<Vec<Value>> {
            match raw {
                Some(r) => decode_raw(&t, id, r, None),
                None => Ok(vec![Value::Null; t.cols.len()]),
            }
        };
        let mut row = side(before)?;
        row.extend(side(after)?);
        let env = Env { cols: &cols, row: &row, params: &[], aggs: &[], sub: None };
        if truth(&eval(&e, &env)?)? == Some(false) {
            out.push(id.clone());
        }
    }
    Ok(out)
}

/// The columns of `table` (as `v` defines it) that can hold text someone typed: all but numbers, times, flags,
/// addresses, enum labels and the like. For `untrusted_columns = '*'`; empty for a table that isn't there.
pub(crate) fn text_columns(v: &View, table: &str) -> Vec<String> {
    let Ok(Some(t)) = load_world(v, table) else { return vec![] };
    t.cols
        .iter()
        .filter(|c| {
            !matches!(
                c.ty,
                Type::SmallInt
                    | Type::Int
                    | Type::BigInt
                    | Type::Oid
                    | Type::Float
                    | Type::Bool
                    | Type::Date
                    | Type::Timestamp
                    | Type::TimestampTz
                    | Type::Interval
                    | Type::Time
                    | Type::TimeTz
                    | Type::Numeric
                    | Type::Vector
                    | Type::Point
                    | Type::Lseg
                    | Type::Box
                    | Type::Circle
                    | Type::Line
                    | Type::Money
                    | Type::PgLsn
                    | Type::Tid
                    | Type::Xid
                    | Type::Bit
                    | Type::VarBit
                    | Type::Uuid
                    | Type::Inet
                    | Type::Cidr
                    | Type::MacAddr
                    | Type::Enum(_)
            )
        })
        .map(|c| c.name.clone())
        .collect()
}
