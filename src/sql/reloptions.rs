//! Storage parameters (reloptions): `CREATE TABLE ... WITH (fillfactor = 70)`, `ALTER TABLE ... SET (...)` and
//! `RESET (...)`, `CREATE INDEX ... WITH (...)` and `ALTER INDEX ... SET (...)`. Chronos has no pages to fill nor
//! vacuum to tune, so none of them changes how a table is kept; they are checked as Postgres checks them
//! (`transformRelOptions`, then each kind's `parseRelOptions`), kept as Postgres keeps them, `name=value` as written,
//! and shown where Postgres shows them (pg_class.reloptions).

use super::*;

/// What a set of parameters is for: a table (`heap`), its TOAST table (`toast.` ones), an index of an access
/// method, or a tablespace (see `tablespace`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Kind {
    Heap,
    Toast,
    Btree,
    Hash,
    Gist,
    Gin,
    Brin,
    Spgist,
    /// contrib/bloom's: a signature `length` and the bits each key column sets (`col1` ... `col32`)
    Bloom,
    Tablespace,
}

impl Kind {
    /// The kind of an index of access method `method`; None for one whose parameters aren't checked here (pgvector's).
    pub(super) fn of_method(method: &str) -> Option<Kind> {
        Some(match method {
            "btree" => Kind::Btree,
            "hash" => Kind::Hash,
            "gist" => Kind::Gist,
            "gin" => Kind::Gin,
            "brin" => Kind::Brin,
            "spgist" => Kind::Spgist,
            "bloom" => Kind::Bloom,
            _ => return None,
        })
    }
}

/// A parameter's type, as Postgres's `relopt_type`: its bounds, and an enum's values with the DETAIL a value that
/// is none of them gets.
#[derive(Clone, Copy)]
enum Ty {
    Bool,
    Int(i64, i64),
    Real(f64, f64),
    Enum(&'static [&'static str], &'static str),
}

const ON_OFF_AUTO: &str = "Valid values are \"on\", \"off\", and \"auto\".";
const HT: &[Kind] = &[Kind::Heap, Kind::Toast];
const H: &[Kind] = &[Kind::Heap];
const MAX: i64 = i32::MAX as i64;
const TS: &[Kind] = &[Kind::Tablespace];

/// Postgres 16's parameters (reloptions.c), with the kinds each is for.
const PARAMS: &[(&str, Ty, &[Kind])] = &[
    ("autovacuum_enabled", Ty::Bool, HT),
    ("user_catalog_table", Ty::Bool, H),
    ("fastupdate", Ty::Bool, &[Kind::Gin]),
    ("vacuum_truncate", Ty::Bool, HT),
    ("deduplicate_items", Ty::Bool, &[Kind::Btree]),
    ("autosummarize", Ty::Bool, &[Kind::Brin]),
    ("fillfactor", Ty::Int(10, 100), &[Kind::Heap, Kind::Btree, Kind::Hash, Kind::Gist, Kind::Spgist]),
    ("autovacuum_vacuum_threshold", Ty::Int(0, MAX), HT),
    ("autovacuum_vacuum_insert_threshold", Ty::Int(-1, MAX), HT),
    ("autovacuum_analyze_threshold", Ty::Int(0, MAX), H),
    ("autovacuum_vacuum_cost_limit", Ty::Int(1, 10000), HT),
    ("autovacuum_freeze_min_age", Ty::Int(0, 1_000_000_000), HT),
    ("autovacuum_multixact_freeze_min_age", Ty::Int(0, 1_000_000_000), HT),
    ("autovacuum_freeze_max_age", Ty::Int(100_000, 2_000_000_000), HT),
    ("autovacuum_multixact_freeze_max_age", Ty::Int(10_000, 2_000_000_000), HT),
    ("autovacuum_freeze_table_age", Ty::Int(0, 2_000_000_000), HT),
    ("autovacuum_multixact_freeze_table_age", Ty::Int(0, 2_000_000_000), HT),
    ("log_autovacuum_min_duration", Ty::Int(-1, MAX), HT),
    ("toast_tuple_target", Ty::Int(128, 8160), H),
    ("pages_per_range", Ty::Int(1, 131_072), &[Kind::Brin]),
    ("gin_pending_list_limit", Ty::Int(64, MAX / 1024), &[Kind::Gin]),
    ("parallel_workers", Ty::Int(0, 1024), H),
    ("autovacuum_vacuum_cost_delay", Ty::Real(0.0, 100.0), HT),
    ("autovacuum_vacuum_scale_factor", Ty::Real(0.0, 100.0), HT),
    ("autovacuum_vacuum_insert_scale_factor", Ty::Real(0.0, 100.0), HT),
    ("autovacuum_analyze_scale_factor", Ty::Real(0.0, 100.0), H),
    ("vacuum_index_cleanup", Ty::Enum(&["auto", "on", "off", "true", "false", "yes", "no", "1", "0"], ON_OFF_AUTO), HT),
    ("buffering", Ty::Enum(&["auto", "on", "off"], ON_OFF_AUTO), &[Kind::Gist]),
    // (a tablespace's: what the planner would cost reading from it, and how much it may read ahead)
    ("random_page_cost", Ty::Real(0.0, f64::MAX), TS),
    ("seq_page_cost", Ty::Real(0.0, f64::MAX), TS),
    ("effective_io_concurrency", Ty::Int(0, 1000), TS),
    ("maintenance_io_concurrency", Ty::Int(0, 1000), TS),
];

/// One parameter as a statement writes it: its namespace (`toast` of `toast.fillfactor`), its name, and its value
/// as written (None: none given, which is `true`).
pub(super) type Given = (Option<String>, String, Option<String>);

/// The lock ALTER TABLE, VIEW or INDEX takes to SET or RESET `given`, as Postgres's AlterTableGetRelOptionsLockLevel
/// works it out (whatever their namespace): ACCESS EXCLUSIVE for a parameter that changes what a query sees (a
/// view's, user_catalog_table) or how an index is read (fastupdate, buffering, autosummarize), else SHARE UPDATE
/// EXCLUSIVE, which lets reads and writes go on.
pub(super) fn lock_mode(given: &[Given]) -> &'static str {
    const STRONG: &[&str] = &[
        "autosummarize",
        "user_catalog_table",
        "fastupdate",
        "security_barrier",
        "security_invoker",
        "buffering",
        "check_option",
    ];
    match given.iter().any(|(_, n, _)| STRONG.iter().any(|s| s.eq_ignore_ascii_case(n))) {
        true => "AccessExclusiveLock",
        false => "ShareUpdateExclusiveLock",
    }
}

fn bad(message: String) -> SqlError {
    SqlError { code: "22023", message, info: None }
}

/// `( name [= value], ... )`, after WITH, SET or RESET: each parameter as written. A value is a word, a string or a
/// number (a negative one too); a name may have a namespace, `toast.autovacuum_enabled`.
pub(super) fn parse(p: &mut Parser) -> Result<Vec<Given>> {
    p.expect_sym("(")?;
    let mut out = vec![];
    loop {
        let mut name = p.name()?;
        let mut space = None;
        if p.sym(".") {
            space = Some(std::mem::replace(&mut name, p.name()?));
        }
        let value = if p.sym("=") {
            let minus = p.sym("-");
            if !minus {
                p.sym("+");
            }
            Some(match p.next() {
                Some(Tok::Num(n)) if minus => format!("-{n}"),
                Some(Tok::Word(w) | Tok::Str(w) | Tok::Ident(w) | Tok::Num(w)) if !minus => w,
                _ => return p.fail("expected a value"),
            })
        } else {
            None
        };
        out.push((space, name, value));
        if !p.sym(",") {
            break;
        }
    }
    p.expect_sym(")")?;
    Ok(out)
}

/// Postgres's `transformRelOptions`: `old` (kept `name=value`s of namespace `space`) with `given` applied, SET or
/// RESET (`reset`): those it doesn't name stay first, in their order, then what it sets, in its. A namespace not
/// among `valid` is refused, and so is a value in a RESET.
pub(super) fn transform(
    old: &[String],
    given: &[Given],
    space: Option<&str>,
    valid: &[&str],
    reset: bool,
) -> Result<Vec<String>> {
    let named = |o: &str| {
        let n = o.split_once('=').map_or(o, |(n, _)| n);
        given.iter().any(|(s, g, _)| s.as_deref() == space && g == n)
    };
    let mut out: Vec<String> = old.iter().filter(|o| !named(o)).cloned().collect();
    for (s, name, value) in given {
        if reset {
            if value.is_some() {
                return Err(SqlError {
                    code: SYNTAX,
                    message: "RESET must not include values for parameters".into(),
                    info: None,
                });
            }
            continue;
        }
        if let Some(s) = s
            && !valid.contains(&s.as_str())
        {
            return Err(bad(format!("unrecognized parameter namespace \"{s}\"")));
        }
        if s.as_deref() != space {
            continue;
        }
        out.push(format!("{name}={}", value.as_deref().unwrap_or("true")));
    }
    Ok(out)
}

/// Parameter `name` of `kind`: its name as Postgres has it, and its type. bloom's are its own (bloom.c's
/// `_PG_init`): `length` of 1 to 4096 bits, and `col1` to `col32` (INDEX_MAX_KEYS) of 1 to 4095.
fn param(kind: Kind, name: &str) -> Option<(String, Ty)> {
    if kind == Kind::Bloom {
        let name = name.to_ascii_lowercase();
        let col = name.strip_prefix("col").and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=32).contains(n));
        return match (name.as_str(), col) {
            ("length", _) => Some((name, Ty::Int(1, 4096))),
            (_, Some(n)) if name == format!("col{n}") => Some((name, Ty::Int(1, 4095))),
            _ => None,
        };
    }
    PARAMS
        .iter()
        .find(|(n, _, kinds)| n.eq_ignore_ascii_case(name) && kinds.contains(&kind))
        .map(|(n, ty, _)| (n.to_string(), *ty))
}

/// Postgres's `parseRelOptions` with validation: each of `options` (`name=value`) has to be a parameter of `kind`,
/// given once, with a value of its type within its bounds.
pub(super) fn check(kind: Kind, options: &[String]) -> Result<()> {
    let mut seen: Vec<String> = vec![];
    for o in options {
        let (name, value) = o.split_once('=').unwrap_or((o, "true"));
        let Some((name, ty)) = param(kind, name) else {
            return Err(bad(format!("unrecognized parameter \"{name}\"")));
        };
        if seen.contains(&name) {
            return Err(bad(format!("parameter \"{name}\" specified more than once")));
        }
        seen.push(name.clone());
        let bounds = |lo: String, hi: String| SqlError {
            code: "22023",
            message: format!("value {value} out of bounds for option \"{name}\""),
            info: Some(Box::new(Violation {
                detail: Some(format!("Valid values are between \"{lo}\" and \"{hi}\".")),
                ..Violation::default()
            })),
        };
        match ty {
            Ty::Bool => {
                if settings::parse_bool(value.trim()).is_none() {
                    return Err(bad(format!("invalid value for boolean option \"{name}\": {value}")));
                }
            }
            Ty::Int(lo, hi) => {
                let Some(n) = parse_int(value) else {
                    return Err(bad(format!("invalid value for integer option \"{name}\": {value}")));
                };
                if !(lo..=hi).contains(&n) {
                    return Err(bounds(lo.to_string(), hi.to_string()));
                }
            }
            Ty::Real(lo, hi) => {
                let Some(x) = value.trim().parse::<f64>().ok().filter(|x| !x.is_nan()) else {
                    return Err(bad(format!("invalid value for floating point option \"{name}\": {value}")));
                };
                if !(lo..=hi).contains(&x) {
                    return Err(bounds(format!("{lo:.6}"), format!("{hi:.6}")));
                }
            }
            Ty::Enum(values, detail) => {
                if !values.iter().any(|v| v.eq_ignore_ascii_case(value)) {
                    return Err(SqlError {
                        code: "22023",
                        message: format!("invalid value for enum option \"{name}\": {value}"),
                        info: Some(Box::new(Violation { detail: Some(detail.into()), ..Violation::default() })),
                    });
                }
            }
        }
    }
    Ok(())
}

/// An integer as Postgres's `parse_int` reads one: decimal, `0x` hex or `0` octal, or a number with a fraction or an
/// exponent rounded to the nearest; None for anything else or past 32 bits.
fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let n = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()?
    } else if body.len() > 1 && body.starts_with('0') && body.bytes().all(|b| b.is_ascii_digit()) {
        i64::from_str_radix(&body[1..], 8).ok()?
    } else if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit()) {
        body.parse().ok()?
    } else {
        if !body.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
            return None;
        }
        let x: f64 = body.parse().ok().filter(|x: &f64| x.is_finite())?;
        x.round_ties_even() as i64
    };
    let n = if neg { -n } else { n };
    i32::try_from(n).ok().map(i64::from)
}

/// A table's parameters from what CREATE TABLE gives: its own and its TOAST table's, each checked.
pub(super) fn for_table(given: &[Given]) -> Result<(Vec<String>, Vec<String>)> {
    let heap = transform(&[], given, None, &["toast"], false)?;
    let toast = transform(&[], given, Some("toast"), &["toast"], false)?;
    check(Kind::Heap, &heap)?;
    check(Kind::Toast, &toast)?;
    Ok((heap, toast))
}

/// An index's parameters from what CREATE INDEX gives, for access method `method`.
pub(super) fn for_index(method: &str, given: &[Given]) -> Result<Vec<String>> {
    let options = transform(&[], given, None, &[], false)?;
    if let Some(kind) = Kind::of_method(method) {
        check(kind, &options)?;
    }
    Ok(options)
}

/// pg_class.reloptions: the parameters as a text[], or null for none.
pub(super) fn shown(options: &[String]) -> Value {
    if options.is_empty() {
        return Value::Null;
    }
    Value::Text(array_text(options.iter().map(|o| Value::Text(o.clone())).collect::<Vec<_>>().iter()))
}
