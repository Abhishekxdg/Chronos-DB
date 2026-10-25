//! Postgres's configuration parameters: the 363 of them `pg_settings` lists in version 16, with
//! their types, units, ranges and contexts (`settings.tsv`, read from a real Postgres 16), so SET
//! checks a value as Postgres does and SHOW, `current_setting()` and `pg_settings` answer for all of
//! them, not the few Chronos acts on.
//!
//! A setting Chronos has no use for (`work_mem`, `enable_seqscan`, `jit`) is kept as set and read
//! back, as Postgres would, and changes nothing. One that would change an answer if Postgres honored
//! it and Chronos does not (`DateStyle`, `standard_conforming_strings`, `bytea_output`, ...) takes only
//! the values Chronos implements, and anything else is 0A000, never a silent no-op.

use super::{Result, SqlError, err};
use std::collections::HashMap;
use std::sync::LazyLock;

/// One setting, as `pg_settings` describes it.
pub(super) struct Def {
    pub name: &'static str,
    /// Its value at start, in its base unit (the number `pg_settings.setting` shows)
    pub value: &'static str,
    pub unit: &'static str,
    pub category: &'static str,
    pub short: &'static str,
    pub extra: &'static str,
    /// `user`, `superuser`, `sighup`, `postmaster`, `backend`, `superuser-backend` or `internal`
    pub context: &'static str,
    /// `bool`, `integer`, `real`, `string` or `enum`
    pub vartype: &'static str,
    pub min: &'static str,
    pub max: &'static str,
    pub enums: Vec<String>,
    pub boot: &'static str,
}

static DEFS: LazyLock<Vec<Def>> = LazyLock::new(|| {
    include_str!("settings.tsv")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            Def {
                name: f[0],
                value: f[1],
                unit: f[2],
                category: f[3],
                short: f[4],
                extra: f[5],
                context: f[6],
                vartype: f[7],
                min: f[8],
                max: f[9],
                enums: enum_list(f[10]),
                boot: f[11],
            }
        })
        .collect()
});

static BY_NAME: LazyLock<HashMap<String, usize>> =
    LazyLock::new(|| DEFS.iter().enumerate().map(|(i, d)| (d.name.to_lowercase(), i)).collect());

/// Every setting, by name.
pub(super) fn all() -> &'static [Def] {
    &DEFS
}

/// The setting called `name` (any case).
pub(super) fn find(name: &str) -> Option<&'static Def> {
    BY_NAME.get(&name.to_lowercase()).map(|&i| &DEFS[i])
}

/// `{a,b,"c d"}` as its items.
fn enum_list(text: &str) -> Vec<String> {
    let inner = text.trim().trim_start_matches('{').trim_end_matches('}');
    let (mut out, mut cur, mut quoted) = (vec![], String::new(), false);
    for c in inner.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    if !inner.is_empty() {
        out.push(cur);
    }
    out
}

/// A unit's family, and the size of one of it in the family's smallest unit (bytes, microseconds).
type Units = &'static [(&'static str, f64)];
const MEMORY: Units =
    &[("TB", 1_099_511_627_776.0), ("GB", 1_073_741_824.0), ("MB", 1_048_576.0), ("kB", 1024.0), ("B", 1.0)];
const TIME: Units = &[("d", 86_400e6), ("h", 3_600e6), ("min", 60e6), ("s", 1e6), ("ms", 1e3), ("us", 1.0)];

/// The family a base unit belongs to, and its size.
fn base_unit(unit: &str) -> Option<(Units, f64)> {
    Some(match unit {
        "B" => (MEMORY, 1.0),
        "kB" => (MEMORY, 1024.0),
        "8kB" => (MEMORY, 8192.0),
        "MB" => (MEMORY, 1_048_576.0),
        "us" => (TIME, 1.0),
        "ms" => (TIME, 1e3),
        "s" => (TIME, 1e6),
        "min" => (TIME, 60e6),
        _ => return None,
    })
}

fn invalid(name: &str, value: &str) -> SqlError {
    SqlError { code: "22023", message: format!("invalid value for parameter \"{name}\": \"{value}\""), info: None }
}

/// A number at the start of `s`, and what follows it.
fn number(s: &str) -> Option<(f64, &str)> {
    let s = s.trim_start();
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let mut digits = i - start;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let frac = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        digits += i - frac;
    }
    if digits == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        j += usize::from(matches!(b.get(j), Some(b'+' | b'-')));
        let exp = j;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j > exp {
            i = j;
        }
    }
    s[..i].parse::<f64>().ok().map(|v| (v, &s[i..]))
}

/// Postgres's boolean spellings (a prefix of `true`, `false`, `yes`, `no`, `on` and `off` will do).
pub(super) fn parse_bool(v: &str) -> Option<bool> {
    let v = v.trim().to_lowercase();
    match v.as_str() {
        "1" => return Some(true),
        "0" => return Some(false),
        "o" | "" => return None,
        _ => {}
    }
    let is = |word: &str| word.starts_with(&v);
    match (is("true") || is("yes") || v == "on", is("false") || is("no") || is("off")) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// A real number as `%g` writes it (15 digits, none it doesn't need).
fn real_text(v: f64) -> String {
    let rounded = format!("{v:.14e}").parse::<f64>().unwrap_or(v);
    format!("{rounded}")
}

/// An integer or real in its base unit as SHOW writes it: in the largest unit it is a whole number of.
fn with_unit(v: f64, unit: &str) -> String {
    if v <= 0.0 {
        return real_text(v);
    }
    let Some((table, base)) = base_unit(unit) else { return real_text(v) };
    let total = v * base;
    for (name, size) in table {
        if total % size == 0.0 {
            return format!("{}{name}", real_text(total / size));
        }
    }
    format!("{}{unit}", real_text(v))
}

impl Def {
    /// Whether this setting's value is a whole number.
    fn is_int(&self) -> bool {
        self.vartype == "integer"
    }

    /// Its value at start as SHOW writes it.
    pub(super) fn default_text(&self) -> String {
        self.shown(self.value)
    }

    /// `value`, held in the base unit (as `pg_settings.setting` writes it), as SHOW writes it.
    pub(super) fn shown(&self, value: &str) -> String {
        match (self.vartype, value.parse::<f64>()) {
            ("integer" | "real", Ok(v)) if !self.unit.is_empty() => with_unit(v, self.unit),
            ("real", Ok(v)) => real_text(v),
            _ => value.to_string(),
        }
    }

    /// A value SHOW wrote (`64MB`), in the base unit as `pg_settings.setting` writes it (`65536`).
    pub(super) fn base(&self, shown: &str) -> String {
        match self.vartype {
            "integer" | "real" => self.number_in_base(shown).map_or_else(|_| shown.to_string(), real_text),
            _ => shown.to_string(),
        }
    }

    /// Its allowed values as a Postgres array, for `pg_settings.enumvals`.
    pub(super) fn enum_array(&self) -> Option<String> {
        (self.vartype == "enum").then(|| {
            let items: Vec<String> = self
                .enums
                .iter()
                .map(|e| if e.is_empty() || e.contains([' ', ',']) { format!("\"{e}\"") } else { e.clone() })
                .collect();
            format!("{{{}}}", items.join(","))
        })
    }

    /// Refuses a SET where Postgres does: the settings a session may not change.
    pub(super) fn check_context(&self) -> Result<()> {
        let n = self.name;
        let (message, code) = match self.context {
            "internal" => (format!("parameter \"{n}\" cannot be changed"), "55P02"),
            "postmaster" => (format!("parameter \"{n}\" cannot be changed without restarting the server"), "55P02"),
            "sighup" => (format!("parameter \"{n}\" cannot be changed now"), "55P02"),
            "backend" | "superuser-backend" => {
                (format!("parameter \"{n}\" cannot be set after connection start"), "55P02")
            }
            _ => return Ok(()),
        };
        Err(SqlError { code, message, info: None })
    }

    /// A number written with an optional unit, in this setting's base unit.
    fn number_in_base(&self, raw: &str) -> Result<f64> {
        let Some((v, rest)) = number(raw) else { return Err(invalid(self.name, raw)) };
        let rest = rest.trim();
        if rest.is_empty() {
            return Ok(v);
        }
        let Some((table, base)) = base_unit(self.unit) else { return Err(invalid(self.name, raw)) };
        match table.iter().find(|(n, _)| *n == rest) {
            Some((_, size)) => Ok(v * size / base),
            None => {
                let names: Vec<String> = table.iter().rev().map(|(n, _)| format!("\"{n}\"")).collect();
                let hint = match names.split_last() {
                    Some((last, rest)) => {
                        format!("Valid units for this parameter are {}, and {last}.", rest.join(", "))
                    }
                    None => String::new(),
                };
                Err(SqlError {
                    code: "22023",
                    message: format!("invalid value for parameter \"{}\": \"{raw}\"", self.name),
                    info: Some(Box::new(super::Violation { hint: Some(hint), ..Default::default() })),
                })
            }
        }
    }

    /// The value a SET of `raw` gives this setting, as SHOW then writes it, or the error Postgres gives.
    pub(super) fn set(&self, raw: &str) -> Result<String> {
        self.check_context()?;
        let name = self.name;
        let value = match self.vartype {
            "bool" => match parse_bool(raw) {
                Some(b) => (if b { "on" } else { "off" }).to_string(),
                None => {
                    return err("22023", format!("parameter \"{name}\" requires a Boolean value"));
                }
            },
            "integer" | "real" => {
                let mut v = self.number_in_base(raw)?;
                if self.is_int() {
                    v = v.round_ties_even();
                }
                let (min, max) =
                    (self.min.parse::<f64>().unwrap_or(f64::MIN), self.max.parse::<f64>().unwrap_or(f64::MAX));
                if !v.is_finite() || v < min || v > max {
                    let shown =
                        if self.unit.is_empty() { real_text(v) } else { format!("{} {}", real_text(v), self.unit) };
                    let (lo, hi) = (real_text(min), real_text(max));
                    return err(
                        "22023",
                        format!("{shown} is outside the valid range for parameter \"{name}\" ({lo} .. {hi})"),
                    );
                }
                self.shown(&real_text(v))
            }
            "enum" => {
                let wanted = raw.trim().to_lowercase();
                let listed = self.enums.iter().find(|e| e.to_lowercase() == wanted).cloned();
                let bool_like = self.enums.iter().any(|e| e == "on") && self.enums.iter().any(|e| e == "off");
                let alias = if wanted == "debug" && self.enums.iter().any(|e| e == "debug2") {
                    Some("debug2".to_string())
                } else {
                    None
                };
                match (listed, alias, bool_like.then(|| parse_bool(&wanted)).flatten()) {
                    (Some(e), ..) => e,
                    (None, Some(a), _) => a,
                    (None, None, Some(b)) => (if b { "on" } else { "off" }).to_string(),
                    _ => {
                        let names: Vec<String> = self.enums.iter().map(|e| format!("\"{e}\"")).collect();
                        return Err(SqlError {
                            code: "22023",
                            message: format!("invalid value for parameter \"{name}\": \"{raw}\""),
                            info: Some(Box::new(super::Violation {
                                hint: Some(format!("Available values: {}.", names.join(", ").replace('"', ""))),
                                ..Default::default()
                            })),
                        });
                    }
                }
            }
            _ => raw.to_string(),
        };
        self.supported(value)
    }

    /// `value`, unless the setting is one whose other values would change what Chronos answers.
    fn supported(&self, value: String) -> Result<String> {
        let lower = value.trim().to_lowercase();
        let only = |allowed: &[&str], shown: &str| -> Result<String> {
            if allowed.contains(&lower.as_str()) {
                Ok(shown.to_string())
            } else {
                err(
                    "0A000",
                    format!(
                        "{} = {value} isn't supported: Chronos only does {}",
                        self.name,
                        allowed.first().unwrap_or(&"")
                    ),
                )
            }
        };
        match self.name {
            "DateStyle" => {
                // a style and an order, in either order, as Postgres normalizes them
                let words: Vec<&str> = lower.split([',', ' ']).filter(|w| !w.is_empty()).collect();
                // (what it leaves out stays as the session has it)
                let now = super::time::date_style();
                match super::time::parse_date_style_from(now, &words).filter(|_| !words.is_empty()) {
                    Some((style, order)) => Ok(super::time::date_style_name(style, order)),
                    None => err("22023", format!("invalid value for parameter \"DateStyle\": \"{value}\"")),
                }
            }
            "IntervalStyle" => only(&["postgres", "postgres_verbose", "sql_standard", "iso_8601"], &lower),
            "standard_conforming_strings" | "array_nulls" => only(&["on"], "on"),
            "quote_all_identifiers" | "transform_null_equals" => only(&["off"], "off"),
            // (READ ONLY transactions: see `Session::default_read_only`)
            "default_transaction_read_only" | "transaction_read_only" => match lower.as_str() {
                "on" | "true" | "yes" | "1" => Ok("on".into()),
                "off" | "false" | "no" | "0" => Ok("off".into()),
                _ => err("22023", format!("parameter \"{}\" requires a Boolean value", self.name)),
            },
            "bytea_output" => only(&["hex", "escape"], &lower),
            "session_replication_role" => only(&["origin"], "origin"),
            "extra_float_digits" => match lower.parse::<i64>() {
                Ok(n) if (-15..=3).contains(&n) => Ok(lower),
                Ok(_) => err(
                    "22023",
                    format!("{value} is outside the valid range for parameter \"extra_float_digits\" (-15 .. 3)"),
                ),
                _ => err("22023", format!("invalid value for parameter \"extra_float_digits\": \"{value}\"")),
            },
            "client_encoding" => match lower.replace(['-', '_'], "").as_str() {
                "utf8" | "unicode" => Ok("UTF8".into()),
                _ => err("0A000", format!("client_encoding = {value} isn't supported: Chronos speaks UTF8")),
            },
            "lc_monetary" | "lc_numeric" | "lc_time" => match lower.as_str() {
                "c" | "posix" | "c.utf-8" | "c.utf8" | "" => Ok(value),
                _ => err(
                    "0A000",
                    format!("{} = {value} isn't supported: Chronos formats numbers and dates as C does", self.name),
                ),
            },
            "default_text_search_config" => match lower.as_str() {
                "pg_catalog.english" | "english" | "pg_catalog.simple" | "simple" => Ok(value),
                _ => err(
                    "0A000",
                    format!("default_text_search_config = {value} isn't supported: english and simple are"),
                ),
            },
            // (heap, or a table access method CREATE ACCESS METHOD made: see `accessmethod`)
            "default_table_access_method" => super::accessmethod::check_default(&value),
            _ => Ok(value),
        }
    }
}
