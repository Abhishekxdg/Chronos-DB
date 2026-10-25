//! The columns of Postgres 16's catalog tables, so a tool that reads one (pg_dump reads sixty) finds
//! every column it names: `catalog_columns.txt` lists them in Postgres's order with a type, and for
//! the tables Chronos fills, one sample value. A table Chronos has no rows for is empty; a column it
//! doesn't work out itself takes the sample (a plain table's `relpersistence` is `p`, an ordinary
//! type's `typtype` is `b`), which is right for everything that has no reason to differ.

use super::*;
use std::sync::LazyLock;

/// A column: its name, type, and the sample value (None: null).
pub(super) type SchemaCol = (String, Type, Option<Value>);

static SCHEMA: LazyLock<HashMap<String, Vec<SchemaCol>>> = LazyLock::new(|| {
    let mut tables: HashMap<String, Vec<SchemaCol>> = HashMap::new();
    for line in include_str!("catalog_columns.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with("row\t") && !l.is_empty())
    {
        let mut f = line.splitn(4, '\t');
        let (Some(table), Some(name), Some(ty), Some(sample)) = (f.next(), f.next(), f.next(), f.next()) else {
            continue;
        };
        let (ty, value) = column_type(ty, sample);
        tables.entry(table.to_string()).or_default().push((name.to_string(), ty, value));
    }
    tables
});

/// Rows Postgres has that Chronos keeps as they are (its languages, its built-in types, the
/// tablespaces, plpgsql): a table's rows, each value in the column's type.
static ROWS: LazyLock<HashMap<String, Vec<Vec<Value>>>> = LazyLock::new(|| {
    let mut rows: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
    for line in include_str!("catalog_columns.txt").lines().filter(|l| l.starts_with("row\t")) {
        let mut f = line.split('\t').skip(1);
        let Some(table) = f.next() else { continue };
        let Some(schema) = SCHEMA.get(table) else { continue };
        let row: Vec<Value> = schema
            .iter()
            .zip(f.chain(std::iter::repeat("\\N")))
            .map(|((_, ty, _), text)| match column_value(*ty, text) {
                Some(v) => v,
                None => Value::Null,
            })
            .collect();
        rows.entry(table.to_string()).or_default().push(row);
    }
    rows
});

/// `text` (Postgres's output for a column, `\N` for null) as a value of type `ty`.
fn column_value(ty: Type, text: &str) -> Option<Value> {
    if text == "\\N" {
        return None;
    }
    Some(match ty {
        Type::Bool => Value::Bool(matches!(text, "t" | "true")),
        t if t.whole() => text.parse().map(Value::Int).ok()?,
        Type::Float | Type::Float4 => text.parse().map(Value::Float).ok()?,
        Type::TimestampTz => return None,
        Type::Numeric => coerce(Value::Text(text.to_string()), Type::Numeric).ok()?,
        Type::Reg(Reg::Proc | Reg::Procedure) => proc_value(text, ty),
        // (another reg type's sample is its oid)
        Type::Reg(r) => Value::Reg(text.parse().ok()?, r),
        _ => Value::Text(text.to_string()),
    })
}

/// Postgres's type name as a type here, and the sample as a value of it.
fn column_type(ty: &str, sample: &str) -> (Type, Option<Value>) {
    let null = sample == "\\N";
    let ty = match ty {
        "bool" => Type::Bool,
        "int2" => Type::SmallInt,
        "int4" => Type::Int,
        "oid" => Type::Oid,
        "int8" | "xid" | "cid" => Type::BigInt,
        // (a float4's values are doubles held to 32 bits, written as float4out writes them: reltuples 1e+06)
        "float4" => Type::Float4,
        "float8" => Type::Float,
        "numeric" => Type::Numeric,
        "int2vector" => Type::Array(22),
        "oidvector" => Type::Array(30),
        // an ACL: its items as text (`alice=r/owner`), as an array
        "_aclitem" => Type::Array(25),
        // the other arrays, as arrays of their elements as Chronos types them (a name is text)
        "_text" | "_name" => Type::Array(25),
        "_char" => Type::Array(18),
        "_oid" => Type::Array(26),
        "_int2" => Type::Array(21),
        "_float4" | "_float8" => Type::Array(701),
        "_bool" => Type::Array(16),
        t if Reg::named(t).is_some() => Type::Reg(Reg::named(t).unwrap_or(Reg::Proc)),
        t if t.strip_prefix('_').and_then(Reg::named).is_some() => {
            Type::Array(t.strip_prefix('_').and_then(Reg::named).map_or(25, Reg::oid))
        }
        "timestamptz" => Type::TimestampTz,
        _ => Type::Text,
    };
    let value = if null { None } else { column_value(ty, sample) };
    (ty, value)
}

/// Whether Postgres has a catalog table or view of this name (plain, without `pg_catalog.`).
pub(super) fn known(name: &str) -> bool {
    SCHEMA.contains_key(name)
}

/// `name`'s rows with every column Postgres has, in its order: the ones `cols` has from it, the
/// others from the sample. Columns of `cols` that Postgres doesn't have come last.
pub(super) fn complete(
    name: &str,
    cols: &[(&str, Type)],
    rows: Vec<Vec<Value>>,
) -> (Vec<(String, Type)>, Vec<Vec<Value>>) {
    let Some(schema) = SCHEMA.get(name) else {
        return (cols.iter().map(|(n, t)| (n.to_string(), *t)).collect(), rows);
    };
    // a table Chronos has no rows of its own for, whose rows are Postgres's as they are
    if cols.is_empty()
        && let Some(rows) = ROWS.get(name)
    {
        return (schema.iter().map(|(n, t, _)| (n.clone(), *t)).collect(), rows.clone());
    }
    let full = ROWS.get(name);
    let oid_at = cols.iter().position(|(c, _)| *c == "oid");
    // where each of Postgres's columns comes from: a column of `cols`, or its sample
    let from: Vec<Option<usize>> = schema.iter().map(|(n, ..)| cols.iter().position(|(c, _)| c == n)).collect();
    let mut out_cols: Vec<(String, Type)> =
        schema.iter().zip(&from).map(|((n, t, _), f)| (n.clone(), f.map_or(*t, |i| cols[i].1))).collect();
    let extra: Vec<usize> = (0..cols.len()).filter(|i| !from.contains(&Some(*i))).collect();
    out_cols.extend(extra.iter().map(|&i| (cols[i].0.to_string(), cols[i].1)));
    let rows = rows
        .into_iter()
        .enumerate()
        .map(|(at, r)| {
            // Postgres's own row for this object, if it has one (a built-in type)
            let theirs = oid_at.and_then(|i| full.and_then(|f| f.iter().find(|x| x[0] == r[i])));
            let mut out: Vec<Value> = schema
                .iter()
                .enumerate()
                .zip(&from)
                .map(|((k, (n, _, sample)), f)| match f {
                    Some(i) => r[*i].clone(),
                    None if theirs.is_some() => theirs.map_or(Value::Null, |t| t[k].clone()),
                    // (an object's own oid is its own: the sample's would be the same for all)
                    None if n == "oid" => Value::Int(60_000 + at as i64),
                    None => sample.clone().unwrap_or(Value::Null),
                })
                .collect();
            out.extend(extra.iter().map(|&i| r[i].clone()));
            out
        })
        .collect();
    (out_cols, rows)
}

/// Postgres's columns of `table`, in its order.
pub(super) fn columns(table: &str) -> Vec<(String, Type)> {
    SCHEMA.get(table).map(|c| c.iter().map(|(n, t, _)| (n.clone(), *t)).collect()).unwrap_or_default()
}

/// Postgres's own rows of `table` (its built-in types, its languages).
pub(super) fn builtin_rows(table: &str) -> Vec<Vec<Value>> {
    ROWS.get(table).cloned().unwrap_or_default()
}

/// A row of `table` from the values `fields` name, in Postgres's column order: every column it doesn't
/// name is null, never a sample (a row of an object of the world's own has none of Postgres's).
pub(super) fn named_row(table: &str, fields: Vec<(&str, Value)>) -> Vec<Value> {
    let Some(schema) = SCHEMA.get(table) else { return vec![] };
    let value = |n: &str, ty: Type| match fields.iter().find(|(f, _)| *f == n) {
        Some((_, Value::Text(t))) if matches!(ty, Type::Reg(Reg::Proc | Reg::Procedure)) => proc_value(t, ty),
        Some((_, v)) => v.clone(),
        None => Value::Null,
    };
    schema.iter().map(|(n, ty, _)| value(n, *ty)).collect()
}

/// A catalog row's regproc as Postgres writes it (`int4in`, `-`), or as its oid (`852`, the built-in rows of
/// pg_operator and pg_aggregate, whose functions' names are often several's): its function's oid, 0 for `-` (or a
/// name no built-in function has alone).
fn proc_value(name: &str, ty: Type) -> Value {
    let o = match (name.parse::<i64>(), &builtin_procs(name)[..]) {
        (Ok(o), _) => o,
        (_, [o]) => *o,
        _ => 0,
    };
    Value::Reg(o, ty.reg().unwrap_or(Reg::Proc))
}

/// Whether `name` is one of Postgres 16's own functions (a `pg_catalog` one: a line of `builtin_procs.tsv`), not
/// an extension's. An extension's function built in here (uuid-ossp's, pg_trgm's, ...) is called by whatever
/// schema CREATE EXTENSION put it in (`public.uuid_generate_v4()`, as pg_dump writes it); a core one only by
/// `pg_catalog`'s.
pub(super) fn core_function(name: &str) -> bool {
    static NAMES: LazyLock<HashSet<&'static str>> =
        LazyLock::new(|| PROCS.iter().filter(|p| p.0 < 16000).map(|p| p.1.trim_matches('"')).collect());
    NAMES.contains(name)
}

/// Whether a built-in type (one Chronos has a `pg_type` row for) is named `name` (its `typname`, `int4`).
pub(super) fn builtin_type_named(name: &str) -> bool {
    static NAMES: LazyLock<HashSet<String>> = LazyLock::new(|| {
        let at = SCHEMA.get("pg_type").and_then(|c| c.iter().position(|(n, ..)| n == "typname"));
        builtin_rows("pg_type").iter().filter_map(|r| at.map(|i| r[i].to_text())).collect()
    });
    NAMES.contains(name)
}

/// What Postgres says of a built-in type: its length, whether it is passed by value, its alignment,
/// its storage, and its collation (the columns `attlen`, `attbyval`, `attalign`, `attstorage` and
/// `attcollation` copy for a column of the type).
pub(super) fn type_facts(oid: i64) -> Option<(i64, bool, String, String, i64)> {
    let schema = SCHEMA.get("pg_type")?;
    let at = |n: &str| schema.iter().position(|(c, ..)| c == n);
    let (o, len, byval, align, storage, coll) =
        (at("oid")?, at("typlen")?, at("typbyval")?, at("typalign")?, at("typstorage")?, at("typcollation")?);
    let row = ROWS.get("pg_type")?.iter().find(|r| r[o] == Value::Int(oid))?;
    let int = |i: usize| if let Value::Int(n) = row[i] { n } else { 0 };
    let text = |i: usize| row[i].to_text();
    Some((int(len), row[byval] == Value::Bool(true), text(align), text(storage), int(coll)))
}

/// Postgres 16's built-in functions (`builtin_procs.tsv`), by oid: the name as regproc shows it (quoted where
/// Postgres quotes it, `"left"`), the argument types as regprocedure does, `(text,integer)`, and the whole line,
/// whose other fields (`pg_proc`'s columns) are read when a row is made (see `builtin_proc_rows`).
static PROCS: LazyLock<Vec<(i64, &'static str, &'static str, &'static str)>> = LazyLock::new(|| {
    let mut procs: Vec<(i64, &str, &str, &str)> = include_str!("builtin_procs.tsv")
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some((f.next()?.parse().ok()?, f.next()?, f.next()?, l))
        })
        .chain(EXTENSION_PROCS)
        .collect();
    procs.sort_by_key(|p| p.0);
    procs
});

/// Postgres's descriptions of its built-in functions, operators, access methods and types
/// (`builtin_descriptions.tsv`): objoid, classoid (pg_proc's 1255, ...) and description, by classoid and then objoid.
pub(super) fn builtin_descriptions() -> &'static [(i64, i64, &'static str)] {
    static ROWS: LazyLock<Vec<(i64, i64, &'static str)>> = LazyLock::new(|| {
        include_str!("builtin_descriptions.tsv")
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?))
            })
            .collect()
    });
    &ROWS
}

/// The description Postgres gives built-in object `oid` of the catalog table whose oid is `class` (pg_proc's 1255,
/// pg_operator's 2617, pg_am's 2601, pg_type's 1247).
pub(super) fn builtin_description(oid: i64, class: i64) -> Option<&'static str> {
    let all = builtin_descriptions();
    all.binary_search_by_key(&(class, oid), |(o, c, _)| (*c, *o)).ok().map(|i| all[i].2)
}

/// The functions of the extensions built in that the catalog names: pgvector's index handler (`pg_am`'s
/// `amhandler` for hnsw), and the input and output functions of hstore's and ltree's types (their `pg_type` rows'
/// `typinput` and the rest), each at an oid of Chronos's own, as hnsw's is. They have no `pg_proc` rows.
const EXTENSION_PROCS: [(i64, &str, &str, &str); 13] = [
    (16301, "hnswhandler", "(internal)", ""),
    (16320, "hstore_in", "(cstring)", ""),
    (16321, "hstore_out", "(hstore)", ""),
    (16322, "hstore_recv", "(internal)", ""),
    (16323, "hstore_send", "(hstore)", ""),
    (16324, "ltree_in", "(cstring)", ""),
    (16325, "ltree_out", "(ltree)", ""),
    (16326, "ltree_recv", "(internal)", ""),
    (16327, "ltree_send", "(ltree)", ""),
    (16328, "lquery_in", "(cstring)", ""),
    (16329, "lquery_out", "(lquery)", ""),
    (16330, "lquery_recv", "(internal)", ""),
    (16331, "lquery_send", "(lquery)", ""),
];

/// A built-in function's fields, from its line in `builtin_procs.tsv` (see its header): `pg_proc`'s columns and
/// what `pg_get_function_arguments`, `_result` and `pg_get_functiondef` give.
struct Builtin<'a> {
    oid: i64,
    name: &'a str,
    args: &'a str,
    f: Vec<&'a str>,
}

impl Builtin<'static> {
    fn of(oid: i64) -> Option<Builtin<'static>> {
        let at = PROCS.binary_search_by_key(&oid, |p| p.0).ok()?;
        let (oid, name, args, line) = PROCS[at];
        let f: Vec<&str> = line.split('\t').collect();
        (f.len() >= 28).then_some(Builtin { oid, name, args, f })
    }
}

/// pg_proc.procost of built-in function `oid` (its 15th field; see `builtin_procs.tsv`): what the planner charges
/// a call, in cpu_operator_costs (a real, widened as Postgres widens it). add_function_cost asks a prosupport
/// function first, but no built-in one answers SupportRequestCost (only the regression suite's test function does).
pub(super) fn builtin_proc_cost(oid: i64) -> Option<f64> {
    Builtin::of(oid)?.f.get(14)?.parse::<f32>().ok().map(f64::from)
}

impl Builtin<'_> {
    /// Field `i`, as COPY's text format writes it: None for `\N`, with `\t`, `\n` and `\\` read back.
    fn text(&self, i: usize) -> Option<String> {
        let raw = self.f[i];
        if raw == "\\N" {
            return None;
        }
        if !raw.contains('\\') {
            return Some(raw.to_string());
        }
        let (mut out, mut chars) = (String::with_capacity(raw.len()), raw.chars());
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(o) => out.push(o),
                None => {}
            }
        }
        Some(out)
    }

    fn int(&self, i: usize) -> i64 {
        self.f[i].parse().unwrap_or(0)
    }

    fn flag(&self, i: usize) -> bool {
        self.f[i] == "t"
    }

    /// Its name as stored, unquoted (`left`).
    fn proname(&self) -> String {
        match self.name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
            Some(n) => n.replace("\"\"", "\""),
            None => self.name.to_string(),
        }
    }

    /// `pg_get_function_identity_arguments`: the argument types with `, ` between, unless the line says more
    /// (names, OUT arguments, VARIADIC).
    fn identity_arguments(&self) -> String {
        self.text(24).unwrap_or_else(|| {
            let inner = self.args.strip_prefix('(').and_then(|a| a.strip_suffix(')')).unwrap_or(self.args);
            inner.replace(',', ", ")
        })
    }

    /// `pg_get_function_arguments`: the identity arguments, with defaults where it has some.
    fn arguments(&self) -> String {
        self.text(25).unwrap_or_else(|| self.identity_arguments())
    }

    /// `proargdefaults`: its arguments' defaults as `pg_get_function_arguments` writes them (`'{}'::jsonb`), in
    /// parentheses with `, ` between; None when it has none (`pronargdefaults` is 0).
    fn arg_defaults(&self) -> Option<String> {
        if self.int(11) == 0 {
            return None;
        }
        let args = self.arguments();
        let (mut parts, mut depth, mut quoted, mut start) = (vec![], 0i32, false, 0);
        for (i, c) in args.bytes().enumerate() {
            match c {
                b'\'' => quoted = !quoted,
                b'(' | b'[' if !quoted => depth += 1,
                b')' | b']' if !quoted => depth -= 1,
                b',' if !quoted && depth == 0 => {
                    parts.push(&args[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        parts.push(&args[start..]);
        let defaults: Vec<&str> =
            parts.iter().filter_map(|p| p.split_once(" DEFAULT ").map(|(_, d)| d.trim())).collect();
        Some(format!("({})", defaults.join(", ")))
    }

    /// Its ACL as Postgres writes it, its bootstrap superuser (`postgres` there) being Chronos's `chronos`.
    fn acl(&self) -> Option<String> {
        Some(self.text(23)?.replace("postgres=", "chronos=").replace("/postgres", "/chronos"))
    }

    /// `pg_get_functiondef`, as Postgres's ruleutils writes it; 42809 for an aggregate.
    fn def(&self) -> Result<String> {
        if self.f[3] == "a" {
            return err("42809", format!("\"{}\" is an aggregate function", self.proname()));
        }
        let mut d = format!(
            "CREATE OR REPLACE FUNCTION pg_catalog.{}({})\n RETURNS {}\n LANGUAGE {}\n",
            self.name,
            self.arguments(),
            self.text(26).unwrap_or_default(),
            match self.int(13) {
                12 => "internal",
                13 => "c",
                _ => "sql",
            }
        );
        let mut opts = String::new();
        if self.f[3] == "w" {
            opts.push_str(" WINDOW");
        }
        opts.push_str(match self.f[6] {
            "i" => " IMMUTABLE",
            "s" => " STABLE",
            _ => "",
        });
        opts.push_str(match self.f[10] {
            "s" => " PARALLEL SAFE",
            "r" => " PARALLEL RESTRICTED",
            _ => "",
        });
        for (i, word) in [(7, " STRICT"), (8, " SECURITY DEFINER"), (9, " LEAKPROOF")] {
            if self.flag(i) {
                opts.push_str(word);
            }
        }
        let (cost, rows): (f64, f64) = (self.f[14].parse().unwrap_or(0.0), self.f[15].parse().unwrap_or(0.0));
        // (the cost a function of its language has when none is given isn't written)
        if cost != if matches!(self.int(13), 12 | 13) { 1.0 } else { 100.0 } {
            opts.push_str(&format!(" COST {cost}"));
        }
        if rows > 0.0 && rows != 1000.0 {
            opts.push_str(&format!(" ROWS {rows}"));
        }
        if self.int(16) != 0 {
            opts.push_str(&format!(" SUPPORT {}", super::catalog::reg_name(self.int(16), Reg::Proc)));
        }
        if !opts.is_empty() {
            d.push_str(&opts);
            d.push('\n');
        }
        match self.text(27) {
            Some(body) => d.push_str(&body),
            None => {
                d.push_str("AS ");
                if let Some(bin) = self.text(22) {
                    d.push_str(&format!("'{}', ", bin.replace('\'', "''")));
                }
                let src = self.text(21).unwrap_or_default();
                let mut tag = "$function".to_string();
                while src.contains(&format!("{tag}$")) {
                    tag.push('x');
                }
                d.push_str(&format!("{tag}${src}{tag}$"));
            }
        }
        d.push('\n');
        Ok(d)
    }

    /// Its `pg_proc` row, in the order of `cols` (pg_proc's columns).
    fn row(&self, cols: &[(String, Type, Option<Value>)]) -> Vec<Value> {
        use Value::{Bool as b, Int as int, Null};
        let t = |i: usize| self.text(i).map_or(Null, Value::Text);
        let float = |i: usize| Value::Float(self.f[i].parse().unwrap_or(0.0));
        cols.iter()
            .map(|(c, ..)| match c.as_str() {
                "oid" => int(self.oid),
                "proname" => Value::Text(self.proname()),
                "pronamespace" => int(11),
                "proowner" => int(10),
                "prolang" => int(self.int(13)),
                "procost" => float(14),
                "prorows" => float(15),
                "provariadic" => int(self.int(12)),
                "prosupport" => Value::Reg(self.int(16), Reg::Proc),
                "prokind" => t(3),
                "prosecdef" => b(self.flag(8)),
                "proleakproof" => b(self.flag(9)),
                "proisstrict" => b(self.flag(7)),
                "proretset" => b(self.flag(5)),
                "provolatile" => t(6),
                "proparallel" => t(10),
                "pronargs" => int(self.f[17].split(' ').filter(|x| !x.is_empty()).count() as i64),
                "pronargdefaults" => int(self.int(11)),
                "prorettype" => int(self.int(4)),
                "proargtypes" => Value::Text(self.f[17].to_string()),
                "proallargtypes" => t(18),
                "proargmodes" => t(19),
                "proargnames" => t(20),
                "prosrc" => t(21),
                // (a function whose body is SQL, `RETURN lpad($1, $2, ' '::text)`: what pg_get_functiondef writes)
                "prosqlbody" => t(27),
                "proargdefaults" => self.arg_defaults().map_or(Null, Value::Text),
                "probin" => t(22),
                "proacl" => self.acl().map_or(Null, Value::Text),
                _ => Null,
            })
            .collect()
    }
}

/// `pg_proc`'s rows for the built-in functions, in oid order, each with its system columns last (its place among them,
/// and the `tableoid` 1255: see `catalog_system`): made the first time a query reads `pg_proc`, and kept (about 2 MB).
pub(super) fn builtin_proc_rows() -> &'static [Vec<Value>] {
    static ROWS: LazyLock<Vec<Vec<Value>>> = LazyLock::new(|| {
        let Some(cols) = SCHEMA.get("pg_proc") else { return vec![] };
        let mut rows: Vec<Vec<Value>> = PROCS.iter().filter_map(|p| Builtin::of(p.0)).map(|f| f.row(cols)).collect();
        catalog_system("pg_proc", &mut rows, 0);
        rows
    });
    &ROWS
}

/// The built-in rows `builtin_proc_rows` has for the functions with this oid, or this name.
pub(super) fn builtin_proc_rows_of(oid: Option<i64>, name: Option<&str>) -> Vec<Vec<Value>> {
    let rows = builtin_proc_rows();
    let at = |o: i64| rows.binary_search_by_key(&o, |r| if let Value::Int(x) = r[0] { x } else { -1 }).ok();
    let oids: Vec<i64> = match (oid, name) {
        (Some(o), _) => vec![o],
        (None, Some(n)) => builtin_procs(n),
        (None, None) => return rows.to_vec(),
    };
    oids.into_iter().filter_map(at).map(|i| rows[i].clone()).collect()
}

/// `pg_init_privs`' rows: the built-in functions whose privileges aren't the default (`lo_import`'s), as initdb left
/// them, so pg_dump, which dumps what differs from them, leaves them alone.
pub(super) fn builtin_init_privs() -> Vec<Vec<Value>> {
    PROCS
        .iter()
        .filter_map(|p| Builtin::of(p.0))
        .filter_map(|f| {
            let acl = f.acl()?;
            Some(named_row(
                "pg_init_privs",
                vec![
                    ("objoid", Value::Int(f.oid)),
                    ("classoid", Value::Int(1255)),
                    ("objsubid", Value::Int(0)),
                    ("privtype", Value::Text("i".into())),
                    ("initprivs", Value::Text(acl)),
                ],
            ))
        })
        .collect()
}

/// `pg_get_function_arguments` (or with `identity`, `pg_get_function_identity_arguments`) of a built-in function.
pub(super) fn builtin_arguments(oid: i64, identity: bool) -> Option<String> {
    let f = Builtin::of(oid)?;
    Some(if identity { f.identity_arguments() } else { f.arguments() })
}

/// `pg_get_function_result` of a built-in function.
pub(super) fn builtin_result(oid: i64) -> Option<String> {
    Builtin::of(oid)?.text(26)
}

/// `pg_get_functiondef` of a built-in function (42809 for an aggregate).
pub(super) fn builtin_def(oid: i64) -> Option<Result<String>> {
    Some(Builtin::of(oid)?.def())
}

/// The built-in functions named `name` (unquoted, as stored): their oids.
pub(super) fn builtin_procs(name: &str) -> Vec<i64> {
    static NAMED: LazyLock<HashMap<&'static str, Vec<i64>>> = LazyLock::new(|| {
        let mut named: HashMap<&str, Vec<i64>> = HashMap::new();
        for (o, n, ..) in PROCS.iter() {
            named.entry(n.trim_matches('"')).or_default().push(*o);
        }
        named
    });
    NAMED.get(name).cloned().unwrap_or_default()
}

/// Whether every built-in function named `name` that a call can be (not an aggregate or window function) is STABLE or
/// VOLATILE (`provolatile`): a call of it is not immutable, whatever its arguments' types. False for a name no
/// built-in function has.
pub(super) fn builtin_mutable(name: &str) -> bool {
    let all: Vec<Builtin> =
        builtin_procs(name).into_iter().filter_map(Builtin::of).filter(|b| !matches!(b.f[3], "a" | "w")).collect();
    !all.is_empty() && all.iter().all(|b| b.f[6] != "i")
}

/// A regprocedure's argument list, `(bigint[],integer)`, as its types.
fn split_args(args: &str) -> Vec<String> {
    let inner = args.strip_prefix('(').and_then(|a| a.strip_suffix(')')).unwrap_or(args);
    if inner.is_empty() { vec![] } else { inner.split(',').map(String::from).collect() }
}

/// The built-in functions named `name` that a call can be (not aggregates or window functions): for each, its
/// argument types as regprocedure shows them, its result type (`SETOF t` for a set), whether it is strict and
/// whether its last argument is VARIADIC.
pub(super) fn builtin_signatures(name: &str) -> Vec<(Vec<String>, String, bool, bool)> {
    builtin_procs(name)
        .into_iter()
        .filter_map(|o| Builtin::of(o))
        .filter(|b| !matches!(b.f[3], "a" | "w"))
        .map(|b| (split_args(b.args), b.text(26).unwrap_or_default(), b.flag(7), b.int(12) != 0))
        .collect()
}

/// The built-in function Postgres runs for C symbol `symbol` (what LANGUAGE internal's `AS 'int4pl'` names, its
/// `prosrc`): its name, its argument types as regprocedure shows them, its result type, and whether it returns a set.
pub(super) fn internal_function(symbol: &str) -> Option<(String, Vec<String>, String, bool)> {
    static BY_SYMBOL: LazyLock<HashMap<String, i64>> = LazyLock::new(|| {
        let mut by = HashMap::new();
        for p in PROCS.iter() {
            if let Some(b) = Builtin::of(p.0)
                && b.int(13) == 12
                && let Some(src) = b.text(21)
            {
                by.entry(src).or_insert(p.0);
            }
        }
        by
    });
    let b = Builtin::of(*BY_SYMBOL.get(symbol)?)?;
    Some((b.proname(), split_args(b.args), b.text(26).unwrap_or_default(), b.flag(5)))
}

/// The built-in function with this oid: its name as stored (unquoted), its `prokind` and its `proargtypes`.
pub(super) fn builtin_proc_facts(oid: i64) -> Option<(String, char, Vec<i64>)> {
    let b = Builtin::of(oid)?;
    let kind = b.f[3].chars().next().unwrap_or('f');
    let args = b.f[17].split_whitespace().filter_map(|a| a.parse().ok()).collect();
    Some((b.proname(), kind, args))
}

/// The built-in function with this oid: its name as regproc shows it, and its argument types as regprocedure does.
pub(super) fn builtin_proc(oid: i64) -> Option<(&'static str, &'static str)> {
    let at = PROCS.binary_search_by_key(&oid, |p| p.0).ok()?;
    Some((PROCS[at].1, PROCS[at].2))
}
