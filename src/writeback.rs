//! Write-back rules: how an attached folder's merges are written to Postgres, table by table, kept as code in the
//! folder (`writeback.json`, or the file `CHRONOS_WRITEBACK_RULES` names) so they can be reviewed in a pull request
//! like any other change. Read again at every write-back, so a change takes effect at the next merge; a file that
//! doesn't parse refuses merges into main (saying why) rather than write with rules nobody meant.
//!
//! ```json
//! {
//!   "mode": "enforce",                      // or "observe": every write-back runs, then rolls back, and says so
//!   "isolation": "read committed",          // or "serializable": write skew between merges is refused by Postgres
//!   "tables": {
//!     "inventory": { "delta": ["stock"], "check": "stock >= 0" },
//!     "leads":     { "ignore": ["updated_at"], "keep_live": ["last_seen"] },
//!     "payments":  { "procedure": "app.apply_payment_change", "outbox": "app.chronos_outbox" }
//!   }
//! }
//! ```
//!
//! Per table:
//! - `delta`: numeric columns written as increments (`stock = stock + (world's value - main's value)`) and never
//!   compared, so a counter the app changes all the time doesn't make every merge conflict. Inside Chronos too a
//!   merge combines them so (`theirs + ours - base`).
//! - `ignore`: columns not compared: the world's value is written whatever Postgres holds now (last writer wins).
//! - `keep_live`: columns not compared and not written by an update: Postgres's value stays.
//! - `check`: a condition every row the merge leaves (updated or inserted) must meet in Postgres, after the writes
//!   and before COMMIT; rows that don't hold the merge, as a merge check does.
//! - `writable`: a condition every row the merge updates or deletes must meet in Postgres as it is now, before any
//!   write (`status <> 'closed'`): a row that doesn't refuses the merge, so an agent can't touch what's settled.
//! - `scope`: the rows the merge may reach, as a condition of SQL (`org_id = current_setting('app.org_id')::int`,
//!   with the agent's `pg_settings` giving the value): checked on every row it updates or deletes as Postgres holds
//!   it before the writes (as `writable`), and on every row it leaves after them (as `check`), so a merge can't
//!   reach past it or move a row out of it. Any condition: several columns, a join through a subquery.
//! - `resolver`: a function of the customer's, `f(main jsonb, world jsonb, postgres jsonb) returns jsonb`, that
//!   settles a row the world and Postgres both changed (an update on both sides) instead of refusing the merge: the
//!   row it returns is written (null: Postgres's row stays), and main takes it.
//! - `procedure`: a function of the customer's, `f(changes jsonb) returns bigint`, called in place of the inserts,
//!   updates and deletes (the rows are still locked and compared first): it applies them through the app's own
//!   code and returns how many it applied. Main then takes the table from Postgres.
//! - `outbox`: a table each changed row is recorded in, in the same transaction, for the app's own worker to act on
//!   (send the email, call the API) once the merge has committed: see `OUTBOX_COLUMNS`.

use serde_json::Value as Json;
use std::collections::HashMap;
use std::path::Path;

/// The file in an attached folder that holds its write-back rules.
pub const FILE: &str = "writeback.json";

/// The columns an outbox table must have (names; any types that take these values, as text, `jsonb`, `timestamptz`).
pub const OUTBOX_COLUMNS: &str = "merge_id text, world text, table_name text, op text, key jsonb, before jsonb, \
                                  after jsonb, at timestamptz";

/// A folder's write-back rules (see the module's doc). The default: every column compared and written, Postgres
/// committing, at READ COMMITTED.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rules {
    pub observe: bool,
    pub serializable: bool,
    tables: HashMap<String, TableRules>,
}

/// One table's rules (see the module's doc).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableRules {
    pub delta: Vec<String>,
    pub ignore: Vec<String>,
    pub keep_live: Vec<String>,
    pub check: Option<String>,
    pub writable: Option<String>,
    pub scope: Option<String>,
    pub resolver: Option<String>,
    pub procedure: Option<String>,
    pub outbox: Option<String>,
}

static NONE: std::sync::LazyLock<TableRules> = std::sync::LazyLock::new(TableRules::default);

impl Rules {
    /// Whether any table has rules of its own.
    pub fn has_tables(&self) -> bool {
        !self.tables.is_empty()
    }

    /// Table `name`'s rules (as Chronos's rows name it: `inventory`, `app.orders`); the default when it has none.
    pub fn table(&self, name: &str) -> &TableRules {
        self.tables.get(name).unwrap_or(&NONE)
    }

    /// Whether any table has rules.
    pub fn any(&self) -> bool {
        !self.tables.is_empty()
    }

    /// The rules in `dir` (an attached folder), or in the file `CHRONOS_WRITEBACK_RULES` names: the default when
    /// there's no file. Err: a file that can't be read or doesn't hold rules, in words.
    pub fn load(dir: &Path) -> Result<Rules, String> {
        let path = match std::env::var("CHRONOS_WRITEBACK_RULES") {
            Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
            _ => dir.join(FILE),
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Rules::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Rules::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Rules from their JSON text (see the module's doc).
    pub fn parse(text: &str) -> Result<Rules, String> {
        let j: Json = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let o = j.as_object().ok_or("the rules must be a JSON object")?;
        let mut r = Rules::default();
        for (k, v) in o {
            match k.as_str() {
                "mode" => {
                    r.observe = match v.as_str() {
                        Some("enforce") => false,
                        Some("observe") => true,
                        _ => return Err("mode must be \"enforce\" or \"observe\"".into()),
                    }
                }
                "isolation" => {
                    r.serializable = match v.as_str().map(str::to_ascii_lowercase).as_deref() {
                        Some("read committed") => false,
                        Some("serializable") => true,
                        _ => return Err("isolation must be \"read committed\" or \"serializable\"".into()),
                    }
                }
                "tables" => {
                    let ts = v.as_object().ok_or("tables must be an object of table names")?;
                    for (name, t) in ts {
                        r.tables.insert(name.clone(), table_rules(name, t)?);
                    }
                }
                // a note for the people who review the file
                "comment" | "$comment" => {}
                k => return Err(format!("no rule {k} (there are mode, isolation and tables)")),
            }
        }
        Ok(r)
    }
}

fn table_rules(name: &str, t: &Json) -> Result<TableRules, String> {
    let o = t.as_object().ok_or_else(|| format!("tables.{name} must be an object"))?;
    let mut out = TableRules::default();
    let cols = |k: &str, v: &Json| -> Result<Vec<String>, String> {
        let a = v.as_array().ok_or_else(|| format!("tables.{name}.{k} must be a list of column names"))?;
        a.iter()
            .map(|c| c.as_str().filter(|c| !c.is_empty()).map(String::from))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("tables.{name}.{k} must be a list of column names"))
    };
    let text = |k: &str, v: &Json| -> Result<String, String> {
        v.as_str()
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
            .ok_or_else(|| format!("tables.{name}.{k} must be text"))
    };
    for (k, v) in o {
        match k.as_str() {
            "delta" => out.delta = cols(k, v)?,
            "ignore" => out.ignore = cols(k, v)?,
            "keep_live" => out.keep_live = cols(k, v)?,
            "check" => out.check = Some(text(k, v)?),
            "writable" => out.writable = Some(text(k, v)?),
            "scope" => out.scope = Some(text(k, v)?),
            "resolver" => out.resolver = Some(qualified(name, k, &text(k, v)?)?),
            "procedure" => out.procedure = Some(qualified(name, k, &text(k, v)?)?),
            "outbox" => out.outbox = Some(qualified(name, k, &text(k, v)?)?),
            "comment" | "$comment" => {}
            k => {
                return Err(format!(
                    "tables.{name} has no rule {k} (there are delta, ignore, keep_live, check, writable, scope, resolver, \
                     procedure and outbox)"
                ));
            }
        }
    }
    // a column one rule says to write as an increment and another not to write, or not to compare and to keep
    for c in &out.delta {
        if out.ignore.contains(c) || out.keep_live.contains(c) {
            return Err(format!("tables.{name}: column {c} is in delta and in ignore or keep_live; pick one"));
        }
    }
    if let Some(c) = out.ignore.iter().find(|c| out.keep_live.contains(c)) {
        return Err(format!("tables.{name}: column {c} is in ignore and keep_live; pick one"));
    }
    Ok(out)
}

/// A function's or table's name as the rules give it (`apply_change`, `app.apply_change`), checked to be one: one or
/// two parts of letters, digits and `_`, so it is put in SQL as it is.
fn qualified(table: &str, k: &str, s: &str) -> Result<String, String> {
    let ok = !s.is_empty()
        && s.split('.').count() <= 2
        && s.split('.').all(|p| {
            !p.is_empty()
                && !p.as_bytes()[0].is_ascii_digit()
                && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
    match ok {
        true => Ok(s.split('.').map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(".")),
        false => {
            Err(format!("tables.{table}.{k}: {s:?} isn't a name (name, or schema.name, of letters, digits and _)"))
        }
    }
}

// ---------------------------------------------------------------- numbers, for delta columns

/// A decimal number's digits as an integer and its scale (digits after the point): `-12.50` is (-1250, 2).
fn decimal(v: &Json) -> Option<(i128, u32)> {
    let s = match v {
        Json::Number(n) => n.to_string(),
        Json::String(s) => s.clone(),
        _ => return None,
    };
    let s = s.trim();
    if s.contains(['e', 'E']) {
        return None; // a float's exponent: not a column delta applies to
    }
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits: i128 = format!("{int}{frac}").trim_start_matches('0').parse().unwrap_or(0);
    let scale = u32::try_from(frac.len()).ok().filter(|s| *s <= 30)?;
    Some((if neg { -digits } else { digits }, scale))
}

fn scaled(v: (i128, u32), to: u32) -> Option<i128> {
    v.0.checked_mul(10i128.checked_pow(to - v.1)?)
}

/// `theirs + ours - base` for a delta column (see `TableRules::delta`), in the form `theirs` has it (a JSON number,
/// or text): what both sides' increments leave. None: not numbers delta can add.
pub fn add_deltas(base: &Json, ours: &Json, theirs: &Json) -> Option<Json> {
    let (b, o, t) = (decimal(base)?, decimal(ours)?, decimal(theirs)?);
    let scale = b.1.max(o.1).max(t.1);
    let sum = scaled(t, scale)?.checked_add(scaled(o, scale)?)?.checked_sub(scaled(b, scale)?)?;
    let text = {
        let neg = sum < 0;
        let digits = format!("{:0>width$}", sum.unsigned_abs(), width = scale as usize + 1);
        let (i, f) = digits.split_at(digits.len() - scale as usize);
        let body = if scale == 0 { i.to_string() } else { format!("{i}.{f}") };
        if neg { format!("-{body}") } else { body }
    };
    match theirs {
        Json::String(_) => Some(Json::String(text)),
        _ => serde_json::from_str(&text).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rules_parse_and_refuse() {
        let r = Rules::parse(
            r#"{"mode": "observe", "isolation": "SERIALIZABLE", "tables": {
                 "inventory": {"delta": ["stock"], "check": "stock >= 0", "writable": "sku <> 'gone'"},
                 "leads": {"resolver": "app.settle_lead"},
                 "payments": {"procedure": "app.apply_payment", "outbox": "chronos_outbox", "comment": "finance"}}}"#,
        )
        .unwrap();
        assert!(r.observe && r.serializable);
        assert_eq!(r.table("inventory").delta, vec!["stock"]);
        assert_eq!(r.table("inventory").writable.as_deref(), Some("sku <> 'gone'"));
        assert_eq!(r.table("leads").resolver.as_deref(), Some("\"app\".\"settle_lead\""));
        assert_eq!(r.table("payments").procedure.as_deref(), Some("\"app\".\"apply_payment\""));
        assert_eq!(r.table("payments").outbox.as_deref(), Some("\"chronos_outbox\""));
        assert_eq!(r.table("nothing"), &TableRules::default());
        assert_eq!(Rules::parse("{}").unwrap(), Rules::default());
        for bad in [
            "[]",
            r#"{"mode": "maybe"}"#,
            r#"{"nope": 1}"#,
            r#"{"tables": {"t": {"delta": "stock"}}}"#,
            r#"{"tables": {"t": {"delta": ["a"], "ignore": ["a"]}}}"#,
            r#"{"tables": {"t": {"procedure": "f(); drop table x"}}}"#,
            r#"{"tables": {"t": {"procedure": "a.b.c"}}}"#,
            r#"{"tables": {"t": {"colour": "red"}}}"#,
        ] {
            assert!(Rules::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn deltas_add_up() {
        assert_eq!(add_deltas(&json!(10), &json!(7), &json!(8)), Some(json!(5)));
        assert_eq!(add_deltas(&json!("10.50"), &json!("9.25"), &json!("12")), Some(json!("10.75")));
        assert_eq!(add_deltas(&json!(0), &json!(-3), &json!(1)), Some(json!(-2)));
        assert_eq!(add_deltas(&json!("0.10"), &json!("0.05"), &json!("0.10")), Some(json!("0.05")));
        assert_eq!(add_deltas(&json!(1), &json!("x"), &json!(1)), None);
        assert_eq!(add_deltas(&json!(1), &json!(1e300), &json!(1)), None);
    }
}
