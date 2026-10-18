//! The world's own objects in the catalog: its functions and procedures (`pg_proc`), enum types
//! (`pg_type`, `pg_enum`), triggers (`pg_trigger`), views' rules (`pg_rewrite`), column defaults
//! (`pg_attrdef`), what depends on what (`pg_depend`), and the tables' columns as Postgres types them
//! (`pg_attribute`). What `pg_dump` reads to write them out, and what `\df`, `\sf` and ORMs'
//! introspection ask.
//!
//! Every row is made from the fields named here and no others: a column not named is null, never a
//! sample (see `catalog_schema::named_row`). Oids come after the schemas' (see `Catalog::base`), but functions' are
//! their own (see `func_oids`).

use super::catalog::{Catalog, Obj};
use super::collation::{self, Coll};
use super::functions::{Function, Lang, Ret, Volatility};
use super::*;

/// The oid of Postgres's catalog table `pg_depend` and others name in `classid`.
const PG_CLASS: i64 = 1259;
const PG_CONSTRAINT: i64 = 2606;
const PG_TYPE: i64 = 1247;
const PG_PROC: i64 = 1255;
const PG_ATTRDEF: i64 = 2604;
const PG_REWRITE: i64 = 2618;
const PG_TRIGGER: i64 = 2620;
const PG_POLICY: i64 = 3256;

/// A catalog table with every column of Postgres's: their names and types, and the rows.
pub(super) type Whole = (Vec<(String, Type)>, Vec<Vec<Value>>);

/// One row of `pg_depend`: the object (its class, oid and column), what it depends on, and how.
type Dep = (i64, i64, i64, i64, i64, i64, char);

/// The columns of `information_schema.routines` here, and `routines` makes their rows.
pub(super) const ROUTINE_COLUMNS: [(&str, Type); 15] = [
    ("specific_catalog", Type::Text),
    ("specific_schema", Type::Text),
    ("specific_name", Type::Text),
    ("routine_catalog", Type::Text),
    ("routine_schema", Type::Text),
    ("routine_name", Type::Text),
    ("routine_type", Type::Text),
    ("data_type", Type::Text),
    ("type_udt_name", Type::Text),
    ("routine_body", Type::Text),
    ("routine_definition", Type::Text),
    ("external_language", Type::Text),
    ("is_deterministic", Type::Text),
    ("security_type", Type::Text),
    ("sql_data_access", Type::Text),
];

/// `information_schema.routines`: each function and procedure the world made, as Postgres's view
/// shows them (its `specific_name` is the name and the oid, which tells overloads apart).
pub(super) fn routines(world: &str, cat: &Catalog) -> Vec<Vec<Value>> {
    let t = |s: &str| Value::Text(s.into());
    cat.funcs
        .iter()
        .enumerate()
        .map(|(k, f)| {
            let (schema, name) = (schema_part(&f.name), rel_part(&f.name));
            let data_type = |ty: Type| match ty {
                Type::Array(_) => "ARRAY".to_string(),
                Type::Vector
                | Type::Geometry
                | Type::Geography
                | Type::Enum(_)
                | Type::Hstore
                | Type::Ltree
                | Type::Lquery => "USER-DEFINED".into(),
                ty => ty.pg_name().into_owned(),
            };
            let returns = match &f.ret {
                _ if f.procedure => None,
                Ret::Value(ty) | Ret::SetOf(ty) => Some(data_type(*ty)),
                Ret::Table(_) | Ret::RowsOf(_) => Some("record".to_string()),
                Ret::Trigger => Some("trigger".to_string()),
                Ret::Void => Some("void".to_string()),
            };
            let udt = match &f.ret {
                _ if f.procedure => None,
                Ret::Value(ty) | Ret::SetOf(ty) => Some(udt_name(*ty)),
                Ret::Table(_) | Ret::RowsOf(_) => Some("record".to_string()),
                Ret::Trigger => Some("trigger".to_string()),
                Ret::Void => Some("void".to_string()),
            };
            let sql = f.lang == Lang::Sql;
            vec![
                t(world),
                t(schema),
                t(&format!("{name}_{}", cat.func_oid(k))),
                t(world),
                t(schema),
                t(name),
                t(if f.procedure { "PROCEDURE" } else { "FUNCTION" }),
                returns.map_or(Value::Null, Value::Text),
                udt.map_or(Value::Null, Value::Text),
                t(if sql { "SQL" } else { "EXTERNAL" }),
                t(&f.body),
                t(if sql { "SQL" } else { "PLPGSQL" }),
                t(if f.volatility == Volatility::Immutable { "YES" } else { "NO" }),
                t(if f.definer { "DEFINER" } else { "INVOKER" }),
                t("MODIFIES"),
            ]
        })
        .collect()
}

/// Where the oids of functions made here begin, past every other object's.
const FUNC_OIDS: i64 = 1 << 30;

/// The oids regtype gives PostGIS's geometry (and geography, the next), which have no type ids of their own here.
const GEOMETRY_OID: i64 = 90_001;

/// How far past a row type's oid its array type's is (see `catalog::Named`).
const ROW_ARRAYS: i64 = 1;

/// Where pg_default_acl's oids begin (only pg_default_acl's rows are numbered from here): past the databases'
/// (16384 up) and below the objects' worked out from their names (2^26 up, see `catalog::Named`).
pub(super) const DEFAULT_ACL_OIDS: i64 = 16384 + 2_000_001;

/// A signature's oid slot: FNV-1a of it, in [0, 2^30).
fn func_slot(sig: &str) -> i64 {
    let h = sig.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
    i64::from(h) % (1 << 30)
}

/// Function `f`'s oid: the one CREATE gave it, else its signature's slot's (what `func_oids` gives it unless
/// another function holds that slot).
pub(super) fn function_oid(f: &Function) -> i64 {
    f.oid.unwrap_or_else(|| FUNC_OIDS + func_slot(&f.sig()))
}

/// The free oid nearest `sig`'s slot, past those `taken`.
fn free_oid(sig: &str, taken: &mut HashSet<i64>) -> i64 {
    let mut o = func_slot(sig);
    while !taken.insert(FUNC_OIDS + o) {
        o = (o + 1) % (1 << 30);
    }
    FUNC_OIDS + o
}

/// The functions' oids, in their order. Each keeps the oid CREATE gave it and stored with it (see
/// `new_func_oid`), so a value that holds one (a regproc column) names the same function whatever is made or
/// dropped later, and in every world. A function made before oids were kept gets one from its signature
/// (`name(integer, text)`): FNV-1a in [2^30, 2^31), the next free one when another holds it.
pub(super) fn func_oids(funcs: &[Arc<Function>]) -> Vec<i64> {
    let mut taken = HashSet::new();
    let mut oids = vec![0; funcs.len()];
    let mut kept: Vec<usize> = (0..funcs.len()).filter(|&k| funcs[k].oid.is_some()).collect();
    kept.sort_by_key(|&k| funcs[k].sig());
    let mut rest = vec![];
    for k in kept {
        let o = funcs[k].oid.unwrap_or_default();
        if taken.insert(o) { oids[k] = o } else { rest.push(k) }
    }
    rest.extend((0..funcs.len()).filter(|&k| funcs[k].oid.is_none()));
    for k in rest {
        oids[k] = free_oid(&funcs[k].sig(), &mut taken);
    }
    oids
}

/// Where the oids CREATE FUNCTION hands out begin: past those worked out from signatures.
const NEW_FUNC_OIDS: i64 = 1 << 31;

/// The oid CREATE FUNCTION stores with function `sig` in world view `v`: the one it has, when it is replaced,
/// else the next of a counter every world shares (`core`), so functions made in different worlds never share
/// one when the worlds merge.
pub(super) fn new_func_oid(core: &crate::Core, v: &crate::View, sig: &str) -> super::Result<i64> {
    let mut funcs = vec![];
    for r in v.prefix(&format!("{}/", super::functions::FUNCTIONS))? {
        funcs.push(Function::from_raw(&r?.1)?);
    }
    let oids = func_oids(&funcs);
    if let Some(k) = funcs.iter().position(|f| f.sig() == sig) {
        return Ok(oids[k]);
    }
    loop {
        let n = core.next_values("_sonos_function_oids", 1)?[0];
        let o = NEW_FUNC_OIDS + n;
        if o > i64::from(u32::MAX) {
            return Err(super::SqlError { code: "54000", message: "no function oids are left".into(), info: None });
        }
        if !oids.contains(&o) {
            return Ok(o);
        }
    }
}

impl Catalog {
    /// Where the oids of the objects no reg type names begin (defaults, rules, triggers, enum labels, policies): each
    /// its place after the ones before it, below the oids worked out from names (see `catalog::Named`).
    fn base(&self) -> i64 {
        16384
    }

    /// Function `k`'s oid: worked out from its signature (see `func_oids`), so a value that keeps it (a regproc
    /// column) names the same function after other objects are made or dropped, and in every world.
    pub(crate) fn func_oid(&self, k: usize) -> i64 {
        self.func_oids[k]
    }

    pub(super) fn default_oid(&self, n: usize) -> i64 {
        self.base() + n as i64
    }

    pub(super) fn rewrite_oid(&self, n: usize) -> i64 {
        self.default_oid(self.defaults.len() + n)
    }

    pub(super) fn trigger_oid(&self, n: usize) -> i64 {
        self.rewrite_oid(self.rewrites.len() + n)
    }

    fn label_oid(&self, n: usize) -> i64 {
        self.trigger_oid(self.triggers.len() + n)
    }

    /// The `n`th policy's oid (in table and name order): after the enums' labels.
    pub(super) fn policy_oid(&self, n: usize) -> i64 {
        self.label_oid(self.enums.iter().map(|e| e.1.len()).sum::<usize>() + n)
    }

    /// A collation's oid: a built-in one's as Postgres has it, one made here worked out from its name (0: none).
    pub(crate) fn collation_oid(&self, c: Coll) -> i64 {
        if c == Coll::INDETERMINATE {
            return 0;
        }
        let (name, builtin) = c.name();
        match builtin {
            true => collation::builtin(&name).map_or(0, |b| b.0),
            false => self.colls.iter().position(|(n, _)| *n == name).map_or(0, |k| self.coll_oid(k)),
        }
    }

    /// The collation with this oid as regcollation shows it: quoted where it needs it, with its schema when the
    /// search_path doesn't find it.
    pub(crate) fn collation_shown(&self, oid: i64) -> Option<String> {
        if let Some((_, name, ..)) = collation::builtins().find(|b| b.0 == oid) {
            return Some(collation::quoted(&name));
        }
        let k = self.coll_at(oid)?;
        Some(collation::made(&self.colls[k].0, None).shown())
    }

    /// What an oid that isn't a relation's, an index's, a constraint's or a schema's names.
    pub(super) fn object(&self, oid: i64) -> Option<Obj<'_>> {
        if oid >= FUNC_OIDS {
            return self.func_oids.iter().position(|o| *o == oid).map(Obj::Func);
        }
        if self.is_enum_oid(oid) {
            return Some(Obj::Enum);
        }
        // (a default's and a rule's oid name nothing a function takes)
        let k = usize::try_from(oid.checked_sub(self.base())?).ok()?;
        let k = k.checked_sub(self.defaults.len() + self.rewrites.len())?;
        let (rel, at) = *self.triggers.get(k)?;
        Some(Obj::Trigger(rel, at))
    }

    /// The names of the array types of the enum types and of the relations' row types, by the oid of their element
    /// type: `_` and its name, unless a type of the schema has that name (a table named `_t`, or another array type),
    /// then `_name_1`, `_name_2`, ..., as Postgres's makeArrayTypeName names one (it to 63 bytes). Postgres names them
    /// as they are made; here the enums go first, then the relations by name.
    pub(crate) fn array_type_names(&self) -> HashMap<i64, String> {
        let rows = self.rels.iter().enumerate().filter(|(_, r)| "rvmpf".contains(r.kind));
        let elems: Vec<(i64, &String)> = (self.enums.iter().enumerate().map(|(k, (n, _))| (self.enum_oid(k), n)))
            .chain(rows.map(|(i, r)| (self.row_type_oid(i), &r.name)))
            .collect();
        let mut taken: HashSet<(String, String)> =
            elems.iter().map(|(_, n)| (schema_part(n).to_string(), rel_part(n).to_string())).collect();
        let mut out = HashMap::new();
        for (oid, name) in elems {
            let (schema, base) = (schema_part(name).to_string(), rel_part(name));
            let mut n = 0;
            let made = loop {
                let suffix = if n == 0 { String::new() } else { format!("_{n}") };
                // (the name is cut, at a character's edge, to leave room for `_` and the suffix in 63 bytes)
                let mut cut = base.len().min(63 - 1 - suffix.len());
                while !base.is_char_boundary(cut) {
                    cut -= 1;
                }
                let candidate = format!("_{}{suffix}", &base[..cut]);
                if taken.insert((schema.clone(), candidate.clone())) {
                    break candidate;
                }
                n += 1;
            };
            out.insert(oid, made);
        }
        out
    }

    /// The Postgres type id of a column or argument of type `ty`: an enum's is its own.
    pub(crate) fn type_oid(&self, ty: Type) -> i64 {
        match ty {
            Type::Enum(_) => self.enums.iter().position(|(n, _)| *n == ty.name()).map_or(25, |k| self.enum_oid(k)),
            ty => i64::from(ty.oid()),
        }
    }

    /// Postgres's name for a type as `format_type` writes it, with the type modifier: `character
    /// varying(20)`, `numeric(10,2)`, `mood[]`.
    pub(crate) fn format_type(&self, oid: i64, typmod: i64) -> String {
        match sized_type(oid, typmod) {
            Some(name) => name,
            None => {
                if let Some(k) = self.enum_at(oid, false) {
                    return shown_type(&self.enums[k].0);
                }
                if let Some(k) = self.enum_at(oid, true) {
                    return format!("{}[]", shown_type(&self.enums[k].0));
                }
                // a relation's row type, and its array type
                let row_type = |i: usize| {
                    // (named as its relation is: bare where the search_path finds the relation so, and no built-in
                    // type, which pg_catalog has first, has its name)
                    let name = &self.rels[i].name;
                    let (s, t) = (schema_part(name), rel_part(name));
                    let builtin = catalog_schema::builtin_type_named(t);
                    if find_named(t, Kind::Rel) == *name && !builtin {
                        ident(t)
                    } else {
                        format!("{}.{}", ident(s), ident(t))
                    }
                };
                if let Some(i) = self.row_type_of(oid, false) {
                    return row_type(i);
                }
                if let Some(i) = self.row_type_of(oid, true) {
                    return format!("{}[]", row_type(i));
                }
                if let Some((elem, _)) = ARRAYS.iter().find(|(e, a)| i64::from(*a) == oid && e != a) {
                    return format!("{}[]", self.format_type(i64::from(*elem), -1));
                }
                match oid {
                    22 => "int2vector".into(),
                    30 => "oidvector".into(),
                    2278 => "void".into(),
                    2279 => "trigger".into(),
                    2249 => "record".into(),
                    _ => super::catalog::type_name(oid).map_or_else(|| "???".into(), String::from),
                }
            }
        }
    }

    /// The oid of type `ty` as a regtype value has it (pg_typeof's, a prepared statement's parameter's): its type id,
    /// an enum's its own, and PostGIS's types, which have none here, Chronos's own (see `regtype_name`).
    pub(crate) fn regtype_oid(&self, ty: Type) -> i64 {
        match ty {
            Type::Geometry => GEOMETRY_OID,
            Type::Geography => GEOMETRY_OID + 1,
            ty => self.type_oid(ty),
        }
    }

    /// A type's name as regtype shows it: `format_type`'s, for a type only Chronos has (vector) its own, and for an
    /// oid that names no type the number.
    pub(crate) fn regtype_name(&self, oid: i64) -> String {
        match oid {
            GEOMETRY_OID => return "geometry".into(),
            o if o == GEOMETRY_OID + 1 => return "geography".into(),
            _ => {}
        }
        match self.format_type(oid, -1) {
            s if s.contains("???") => {
                u32::try_from(oid).ok().and_then(Type::from_oid).map_or_else(|| oid.to_string(), |t| t.pg_name().into())
            }
            s => s,
        }
    }

    /// `pg_get_function_arguments`: the arguments as `CREATE FUNCTION` lists them; a procedure's
    /// carry their mode (`IN n integer`). A `RETURNS TABLE`'s columns aren't arguments.
    pub(crate) fn function_arguments(&self, f: &Function, identity: bool) -> String {
        let args: Vec<String> = f
            .args
            .iter()
            .enumerate()
            .map(|(i, (n, t))| {
                let ty = self.format_type(self.type_oid(*t), -1);
                let mode = if f.procedure { "IN " } else { "" };
                let base = match n {
                    Some(n) => format!("{mode}{} {ty}", ident(n)),
                    None => format!("{mode}{ty}"),
                };
                // (the identity form leaves the defaults out)
                match f.default_sql(i).filter(|_| !identity) {
                    Some(d) => format!("{base} DEFAULT {d}"),
                    None => base,
                }
            })
            .collect();
        args.join(", ")
    }

    /// `pg_get_function_result`: what it returns (null for a procedure).
    pub(crate) fn function_result(&self, f: &Function) -> Option<String> {
        let ty = |t: &Type| self.format_type(self.type_oid(*t), -1);
        if f.procedure {
            return None;
        }
        Some(match &f.ret {
            Ret::Value(t) => ty(t),
            Ret::SetOf(t) => format!("SETOF {}", ty(t)),
            Ret::Table(c) => {
                format!(
                    "TABLE({})",
                    c.iter().map(|(n, t)| format!("{} {}", ident(n), ty(t))).collect::<Vec<_>>().join(", ")
                )
            }
            Ret::RowsOf(t) if f.single => shown_name(t),
            Ret::RowsOf(t) => format!("SETOF {}", shown_name(t)),
            Ret::Trigger => "trigger".into(),
            Ret::Void => "void".into(),
        })
    }

    fn function_name(f: &Function) -> String {
        format!("{}.{}", ident(schema_part(&f.name)), ident(rel_part(&f.name)))
    }

    /// `pg_get_functiondef`: the statement that makes it again.
    pub(crate) fn function_def(&self, f: &Function) -> String {
        let kind = if f.procedure { "PROCEDURE" } else { "FUNCTION" };
        let mut d =
            format!("CREATE OR REPLACE {kind} {}({})\n", Self::function_name(f), self.function_arguments(f, false));
        if let Some(r) = self.function_result(f) {
            d.push_str(&format!(" RETURNS {r}\n"));
        }
        d.push_str(&format!(" LANGUAGE {}\n", if f.lang == Lang::Sql { "sql" } else { "plpgsql" }));
        let mut flags = vec![];
        match f.volatility {
            Volatility::Immutable => flags.push("IMMUTABLE"),
            Volatility::Stable => flags.push("STABLE"),
            Volatility::Volatile => {}
        }
        let mut flags: Vec<String> = flags.into_iter().map(String::from).collect();
        match f.tuning.parallel {
            Some('s') => flags.push("PARALLEL SAFE".into()),
            Some('r') => flags.push("PARALLEL RESTRICTED".into()),
            _ => {}
        }
        if f.strict {
            flags.push("STRICT".into());
        }
        if f.definer {
            flags.push("SECURITY DEFINER".into());
        }
        if f.tuning.leakproof {
            flags.push("LEAKPROOF".into());
        }
        // (COST and ROWS where they aren't the defaults, as %g writes them)
        if let Some(c) = f.cost.filter(|c| *c != 100.0) {
            flags.push(format!("COST {}", Value::Float(c).to_text()));
        }
        if let Some(r) = f.tuning.rows.filter(|r| *r > 0.0 && *r != 1000.0) {
            flags.push(format!("ROWS {}", Value::Float(r).to_text()));
        }
        if !flags.is_empty() {
            d.push_str(&format!(" {}\n", flags.join(" ")));
        }
        // its SET clauses, a line each, the values quoted as Postgres quotes them
        for (name, values) in &f.config {
            d.push_str(&format!(" SET {name} TO {}\n", super::setting_sql(name, values)));
        }
        // a SQL-standard body written back, as Postgres keeps it parsed; else the body as it was written
        if f.std_body
            && let Some(b) = super::viewdef::function_body(&self.rels, f)
        {
            d.push_str(&b);
            d.push('\n');
            return d;
        }
        let tag = if f.procedure { "$procedure$" } else { "$function$" };
        d.push_str(&format!("AS {tag}{}{tag}\n", f.body));
        d
    }

    /// The function a trigger runs, by its stored name (a trigger function takes no arguments).
    fn trigger_function(&self, name: &str) -> Option<usize> {
        self.funcs.iter().position(|f| f.name == name && f.args.is_empty())
    }

    /// `pg_depend`'s rows, as pg_dump reads them to put what it writes in an order that restores.
    fn depends(&self) -> Vec<Dep> {
        let mut out: Vec<Dep> = vec![];
        let rel = |i: usize| self.rel_oid(i);
        for (i, r) in self.rels.iter().enumerate() {
            for (j, (_, ty, ..)) in r.columns.iter().enumerate() {
                if matches!(r.kind, 'r' | 'p' | 'f') && matches!(ty, Type::Enum(_)) {
                    let e = self.type_oid(*ty);
                    if e != 25 {
                        out.push((PG_CLASS, rel(i), r.attnum(j), PG_TYPE, e, 0, 'n'));
                    }
                }
            }
            // a sequence belongs to its column: serial's, and an identity's (`i`, dropped only with it)
            if let Some((table, column, kind)) = &r.owner
                && let Some(t) = self.rels.iter().position(|x| x.name == *table)
            {
                let at = self.rels[t].columns.iter().position(|c| c.0 == *column);
                // an identity column's sequence depends on it internally, whichever way it was made
                let identity = at.is_some_and(|a| self.rels[t].identity.get(a).is_some_and(|c| *c != ' '));
                let kind = if identity { 'i' } else { *kind };
                out.push((PG_CLASS, rel(i), 0, PG_CLASS, rel(t), at.map_or(0, |c| self.rels[t].attnum(c)), kind));
            }
        }
        // a typed table depends on its composite type
        for (i, r) in self.rels.iter().enumerate() {
            let ty = self.of_type_oid(r);
            if ty != 0 {
                out.push((PG_CLASS, rel(i), 0, PG_TYPE, ty, 0, 'n'));
            }
        }
        // a partition goes with its table (and is dumped after it); what it has of its parent's goes with the
        // parent's, and with the partition
        for (i, r) in self.rels.iter().enumerate() {
            if let Some((p, ..)) = &r.hier.of
                && let Some(pi) = self.rels.iter().position(|x| x.name == *p)
            {
                out.push((PG_CLASS, rel(i), 0, PG_CLASS, rel(pi), 0, 'a'));
            }
        }
        for (k, (i, x)) in self.idx.iter().enumerate() {
            let Some((up, ..)) = &self.rels[*i].hier.of else { continue };
            let Some(pk) =
                self.idx.iter().position(|(r, y)| self.rels[*r].name == *up && Some(&y.name) == x.given.as_ref())
            else {
                continue;
            };
            out.push((PG_CLASS, self.idx_oid(k), 0, PG_CLASS, self.idx_oid(pk), 0, 'P'));
            out.push((PG_CLASS, self.idx_oid(k), 0, PG_CLASS, rel(*i), 0, 'S'));
        }
        for (k, (i, c)) in self.cons.iter().enumerate() {
            // (a foreign key's for a partition of the table it references: internal to the one it is under)
            if c.given_here {
                if let Some(pk) =
                    self.cons.iter().position(|(r, o)| *r == *i && Some(&o.name) == c.given.as_ref() && o.kind == 'f')
                {
                    out.push((PG_CONSTRAINT, self.con_oid(k), 0, PG_CONSTRAINT, self.con_oid(pk), 0, 'i'));
                }
                continue;
            }
            let Some((up, ..)) = &self.rels[*i].hier.of else { continue };
            let Some(pk) =
                self.cons.iter().position(|(r, o)| self.rels[*r].name == *up && Some(&o.name) == c.given.as_ref())
            else {
                continue;
            };
            out.push((PG_CONSTRAINT, self.con_oid(k), 0, PG_CONSTRAINT, self.con_oid(pk), 0, 'P'));
            out.push((PG_CONSTRAINT, self.con_oid(k), 0, PG_CLASS, rel(*i), 0, 'S'));
        }
        for (n, (i, j)) in self.defaults.iter().enumerate() {
            let oid = self.default_oid(n);
            out.push((PG_ATTRDEF, oid, 0, PG_CLASS, self.rel_oid(*i), self.rels[*i].attnum(*j), 'a'));
            // an enum column's default is a value of the enum
            let ty = self.type_oid(self.rels[*i].columns[*j].1);
            if matches!(self.rels[*i].columns[*j].1, Type::Enum(_)) && ty != 25 {
                out.push((PG_ATTRDEF, oid, 0, PG_TYPE, ty, 0, 'n'));
            }
            if let Some(text) = &self.rels[*i].columns[*j].3
                && let Some(seq) = text.strip_prefix("nextval('").and_then(|t| t.split_once('\'')).map(|(s, _)| s)
                && let Ok(Some(s)) = self.named(seq)
            {
                out.push((PG_ATTRDEF, oid, 0, PG_CLASS, s, 0, 'n'));
            }
        }
        for (n, i) in self.rewrites.iter().enumerate() {
            let oid = self.rewrite_oid(n);
            out.push((PG_REWRITE, oid, 0, PG_CLASS, self.rel_oid(*i), 0, 'i'));
            for read in &self.rels[*i].reads {
                if let Some(t) = self.rels.iter().position(|x| x.name == *read) {
                    out.push((PG_REWRITE, oid, 0, PG_CLASS, self.rel_oid(t), 0, 'n'));
                }
            }
        }
        for (n, (i, k)) in self.triggers.iter().enumerate() {
            let oid = self.trigger_oid(n);
            out.push((PG_TRIGGER, oid, 0, PG_CLASS, self.rel_oid(*i), 0, 'a'));
            let (func, _, _, attnums) = &self.rels[*i].trigger_meta[*k];
            if let Some(f) = self.trigger_function(func) {
                out.push((PG_TRIGGER, oid, 0, PG_PROC, self.func_oid(f), 0, 'n'));
            }
            // an UPDATE OF trigger depends on its columns
            for a in attnums {
                out.push((PG_TRIGGER, oid, 0, PG_CLASS, self.rel_oid(*i), *a, 'n'));
            }
        }
        for (k, f) in self.funcs.iter().enumerate() {
            for (_, t) in &f.args {
                let e = self.type_oid(*t);
                if matches!(t, Type::Enum(_)) && e != 25 {
                    out.push((PG_PROC, self.func_oid(k), 0, PG_TYPE, e, 0, 'n'));
                }
            }
        }
        // a policy goes with its table (numbered as pg_policy numbers them)
        let mut n = 0;
        for (i, r) in self.rels.iter().enumerate() {
            for p in &r.policies {
                out.push((PG_POLICY, self.policy_oid(n), 0, PG_CLASS, rel(i), 0, 'a'));
                // and on each column of its table its USING and WITH CHECK read (`n`), as recordDependencyOnExpr
                // records them
                let text = [p.using.as_deref(), p.check.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" ");
                let words: Vec<String> = super::lex(&text)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|t| match t {
                        super::Tok::Word(w) | super::Tok::Ident(w) => Some(w),
                        _ => None,
                    })
                    .collect();
                for (k, c) in r.columns.iter().enumerate() {
                    if words.contains(&c.0) {
                        out.push((PG_POLICY, self.policy_oid(n), 0, PG_CLASS, rel(i), r.attnum(k), 'n'));
                    }
                }
                n += 1;
            }
        }
        out
    }
}

/// Catalog table `name`, if it holds this world's own objects: its columns as Postgres has them and its
/// rows, each column not named here null.
/// Where event triggers' oids begin, among the objects `foreign::name_oids` numbers.
const EVT_KIND: i64 = 13;

pub(super) fn objects(name: &str, cat: &Catalog) -> Option<Whole> {
    use Value::{Bool as b, Int as int, Null};
    let t = |s: &str| Value::Text(s.into());
    let row = |fields: Vec<(&str, Value)>| catalog_schema::named_row(name, fields);
    let rows: Vec<Vec<Value>> = match name {
        "pg_class" => class_rows(cat),
        "pg_attribute" => attribute_rows(cat),
        // the built-in functions whose privileges initdb set (pg_proc's built-in rows are added by `catalog_query`)
        "pg_init_privs" => catalog_schema::builtin_init_privs(),
        "pg_attrdef" => cat
            .defaults
            .iter()
            .enumerate()
            .map(|(n, (i, j))| {
                let text = cat.rels[*i].columns[*j].3.clone().unwrap_or_default();
                row(vec![
                    ("oid", int(cat.default_oid(n))),
                    ("adrelid", int(cat.rel_oid(*i))),
                    ("adnum", int(cat.rels[*i].attnum(*j))),
                    ("adbin", t(&text)),
                ])
            })
            .collect(),
        // plpgsql, in every database, and what CREATE EXTENSION recorded
        "pg_extension" => {
            let mut rows = catalog_schema::builtin_rows("pg_extension");
            rows.extend(cat.extensions.iter().map(|(name, schema, version)| {
                row(vec![
                    ("oid", int(super::pgcrypto::EXTENSION_OID)),
                    ("extname", t(name)),
                    ("extowner", int(cat.acls.get(&format!("ext/{name}")).and_then(|e| e.owner).unwrap_or(10))),
                    ("extnamespace", int(cat.schema_oid(schema))),
                    ("extrelocatable", b(true)),
                    ("extversion", t(version)),
                ])
            }));
            rows
        }
        // the event triggers (see `userdefs::event_triggers`)
        "pg_event_trigger" => {
            let names: Vec<String> = cat.evts.iter().map(|(n, _)| n.clone()).collect();
            let oids = super::foreign::name_oids(EVT_KIND, &names);
            (cat.evts.iter().zip(oids))
                .map(|((name, j), oid)| {
                    let func = super::func_named(j["func"].as_str().unwrap_or_default());
                    let foid = (cat.funcs.iter().position(|f| f.name == func && f.args.is_empty()))
                        .map_or(0, |k| cat.func_oid(k));
                    let tags: Vec<Value> = (j["tags"].as_array().into_iter().flatten())
                        .filter_map(|t| t.as_str().map(|t| Value::Text(t.into())))
                        .collect();
                    row(vec![
                        ("oid", int(oid)),
                        ("evtname", t(name)),
                        ("evtevent", t(j["event"].as_str().unwrap_or_default())),
                        ("evtowner", int(j["owner"].as_i64().unwrap_or(super::roles::BOOTSTRAP))),
                        ("evtfoid", int(foid)),
                        ("evtenabled", t(if j["enabled"] == false { "D" } else { "O" })),
                        ("evttags", if tags.is_empty() { Null } else { Value::Text(super::array_text(tags.iter())) }),
                    ])
                })
                .collect()
        }
        // foreign-data wrappers, servers, user mappings and foreign tables (see `foreign`)
        "pg_foreign_data_wrapper"
        | "pg_foreign_server"
        | "pg_user_mapping"
        | "pg_user_mappings"
        | "pg_foreign_table" => super::foreign::catalog_rows(name, cat),
        // publications (see `publication`)
        "pg_publication" | "pg_publication_rel" | "pg_publication_namespace" | "pg_publication_tables" => {
            super::publication::catalog_rows(name, cat)
        }
        // statistics objects (see `statistics`)
        "pg_statistic_ext" | "pg_statistic_ext_data" | "pg_stats_ext" | "pg_stats_ext_exprs" => {
            super::statistics::catalog_rows(name, cat)
        }
        // (and pg_settings's two rules, which Postgres's own catalog has)
        "pg_rules" => cat
            .rules
            .iter()
            // (a view's triggers are kept as its rules, but are pg_trigger's)
            .filter(|(_, _, j)| !j["trigger"].is_object())
            .map(|(table, name, j)| {
                row(vec![
                    ("schemaname", t(schema_part(table))),
                    ("tablename", t(rel_part(table))),
                    ("rulename", t(name)),
                    ("definition", t(&super::rules::definition(table, name, j))),
                ])
            })
            .chain(super::viewdef::SETTINGS_RULES.iter().map(|&(name, def)| {
                row(vec![
                    ("schemaname", t("pg_catalog")),
                    ("tablename", t("pg_settings")),
                    ("rulename", t(name)),
                    ("definition", t(def)),
                ])
            }))
            .collect(),
        "pg_type" => {
            let mut rows = catalog_schema::builtin_rows("pg_type");
            let arrays = cat.array_type_names();
            for (k, (name, _)) in cat.enums.iter().enumerate() {
                let (schema, oid) = (int(cat.schema_oid(schema_part(name))), cat.enum_oid(k));
                // its owner and ACL (an array type's: its owner's, and none of its own)
                let (owner, acl) = super::acl::shown(&cat.reg, cat.acls.get(&format!("type/{name}")), 10);
                let common = |mut f: Vec<(&'static str, Value)>| {
                    f.extend([
                        ("typnamespace", schema.clone()),
                        ("typowner", int(owner)),
                        ("typispreferred", b(false)),
                        ("typisdefined", b(true)),
                        ("typdelim", t(",")),
                        ("typrelid", int(0)),
                        ("typmodin", t("-")),
                        ("typmodout", t("-")),
                        ("typanalyze", t("-")),
                        ("typalign", t("i")),
                        ("typnotnull", b(false)),
                        ("typbasetype", int(0)),
                        ("typtypmod", int(-1)),
                        ("typndims", int(0)),
                        ("typcollation", int(0)),
                    ]);
                    f
                };
                // (a domain is a type of its own kind, over its base type: its NOT NULL is the type's)
                let domain = cat.domains.get(k).and_then(Option::as_ref);
                let mut e = common(vec![
                    ("oid", int(oid)),
                    ("typlen", int(4)),
                    ("typbyval", b(true)),
                    ("typtype", t(if domain.is_some() { "d" } else { "e" })),
                    ("typcategory", t(if domain.is_some() { "U" } else { "E" })),
                    ("typsubscript", t("-")),
                    ("typinput", t("enum_in")),
                    ("typoutput", t("enum_out")),
                    ("typreceive", t("enum_recv")),
                    ("typsend", t("enum_send")),
                    ("typelem", int(0)),
                    ("typarray", int(oid + 1)),
                    ("typstorage", t("p")),
                ]);
                let mut a = common(vec![
                    ("oid", int(oid + 1)),
                    ("typlen", int(-1)),
                    ("typbyval", b(false)),
                    ("typtype", t("b")),
                    ("typcategory", t("A")),
                    ("typsubscript", t("array_subscript_handler")),
                    ("typinput", t("array_in")),
                    ("typoutput", t("array_out")),
                    ("typreceive", t("array_recv")),
                    ("typsend", t("array_send")),
                    ("typelem", int(oid)),
                    ("typarray", int(0)),
                    ("typstorage", t("x")),
                    // (an array's statistics are array_typanalyze's, as every array type's are: this comes before
                    // `common`'s `-`, and named_row takes the first)
                    ("typanalyze", t("array_typanalyze")),
                ]);
                if let Some((not_null, _)) = domain {
                    // (the first of a field `common` gives is the one taken)
                    e.insert(0, ("typnotnull", b(*not_null)));
                }
                e.push(("typname", t(rel_part(name))));
                e.push(("typacl", acl));
                a.push(("typname", t(&arrays[&oid])));
                rows.push(row(e));
                rows.push(row(a));
            }
            // the hstore and ltree extensions' types and their arrays: what a driver looks up by name to read and
            // write them. In pg_catalog, as they are built in (in public, pg_dump would write a CREATE TYPE for each)
            let ext = [
                (super::hstore::OID, super::hstore::ARRAY_OID, "hstore", "hstore_subscript_handler"),
                (super::ltree::OID, super::ltree::ARRAY_OID, "ltree", "-"),
                (super::ltree::LQUERY_OID, super::ltree::LQUERY_ARRAY_OID, "lquery", "-"),
            ];
            for (oid, array, name, subscript) in ext {
                let common = |f: &mut Vec<(&'static str, Value)>| {
                    f.extend([
                        ("typnamespace", int(cat.schema_oid("pg_catalog"))),
                        ("typowner", int(10)),
                        ("typlen", int(-1)),
                        ("typbyval", b(false)),
                        ("typtype", t("b")),
                        ("typispreferred", b(false)),
                        ("typisdefined", b(true)),
                        ("typdelim", t(",")),
                        ("typrelid", int(0)),
                        ("typmodin", t("-")),
                        ("typmodout", t("-")),
                        ("typalign", t("i")),
                        ("typstorage", t("x")),
                        ("typnotnull", b(false)),
                        ("typbasetype", int(0)),
                        ("typtypmod", int(-1)),
                        ("typndims", int(0)),
                        ("typcollation", int(0)),
                    ])
                };
                let mut e = vec![
                    ("oid", int(i64::from(oid))),
                    ("typname", t(name)),
                    ("typcategory", t("U")),
                    ("typsubscript", t(subscript)),
                    ("typelem", int(0)),
                    ("typarray", int(i64::from(array))),
                    ("typinput", t(&format!("{name}_in"))),
                    ("typoutput", t(&format!("{name}_out"))),
                    ("typreceive", t(&format!("{name}_recv"))),
                    ("typsend", t(&format!("{name}_send"))),
                    ("typanalyze", t("-")),
                ];
                let mut a = vec![
                    ("oid", int(i64::from(array))),
                    ("typname", t(&format!("_{name}"))),
                    ("typcategory", t("A")),
                    ("typsubscript", t("array_subscript_handler")),
                    ("typelem", int(i64::from(oid))),
                    ("typarray", int(0)),
                    ("typinput", t("array_in")),
                    ("typoutput", t("array_out")),
                    ("typreceive", t("array_recv")),
                    ("typsend", t("array_send")),
                    ("typanalyze", t("array_typanalyze")),
                ];
                common(&mut e);
                common(&mut a);
                rows.push(row(e));
                rows.push(row(a));
            }
            // each table's, view's and materialized view's row type (pg_class.reltype), and its array type
            for (i, r) in cat.rels.iter().enumerate().filter(|(_, r)| "rvmpf".contains(r.kind)) {
                let (oid, schema, owner) =
                    (cat.row_type_oid(i), int(cat.schema_oid(schema_part(&r.name))), int(cat.rel_acl(r).0));
                let common = |f: &mut Vec<(&'static str, Value)>| {
                    f.extend([
                        ("typnamespace", schema.clone()),
                        ("typowner", owner.clone()),
                        ("typlen", int(-1)),
                        ("typbyval", b(false)),
                        ("typispreferred", b(false)),
                        ("typisdefined", b(true)),
                        ("typdelim", t(",")),
                        ("typmodin", t("-")),
                        ("typmodout", t("-")),
                        ("typalign", t("d")),
                        ("typstorage", t("x")),
                        ("typnotnull", b(false)),
                        ("typbasetype", int(0)),
                        ("typtypmod", int(-1)),
                        ("typndims", int(0)),
                        ("typcollation", int(0)),
                    ])
                };
                let mut c = vec![
                    ("oid", int(oid)),
                    ("typname", t(rel_part(&r.name))),
                    ("typtype", t("c")),
                    ("typcategory", t("C")),
                    ("typrelid", int(cat.rel_oid(i))),
                    ("typsubscript", t("-")),
                    ("typelem", int(0)),
                    ("typarray", int(oid + ROW_ARRAYS)),
                    ("typinput", t("record_in")),
                    ("typoutput", t("record_out")),
                    ("typreceive", t("record_recv")),
                    ("typsend", t("record_send")),
                    ("typanalyze", t("-")),
                ];
                let mut a = vec![
                    ("oid", int(oid + ROW_ARRAYS)),
                    ("typname", t(&arrays[&oid])),
                    ("typtype", t("b")),
                    ("typcategory", t("A")),
                    ("typrelid", int(0)),
                    ("typsubscript", t("array_subscript_handler")),
                    ("typelem", int(oid)),
                    ("typarray", int(0)),
                    ("typinput", t("array_in")),
                    ("typoutput", t("array_out")),
                    ("typreceive", t("array_recv")),
                    ("typsend", t("array_send")),
                    ("typanalyze", t("array_typanalyze")),
                ];
                common(&mut c);
                common(&mut a);
                rows.push(row(c));
                rows.push(row(a));
            }
            rows
        }
        "pg_collation" => {
            // the built-in ones, then those made here
            let mut rows: Vec<Vec<Value>> = collation::builtins()
                .map(|(oid, name, provider, locale, encoding)| {
                    let libc = (provider == 'c').then(|| collation::libc_locale(&name));
                    row(vec![
                        ("oid", int(oid)),
                        ("collname", t(&name)),
                        ("collnamespace", int(11)),
                        ("collowner", int(10)),
                        ("collprovider", t(&provider.to_string())),
                        ("collisdeterministic", b(true)),
                        ("collencoding", int(encoding)),
                        ("collcollate", libc.as_deref().map_or(Null, t)),
                        ("collctype", libc.as_deref().map_or(Null, t)),
                        ("colliculocale", locale.map_or(Null, t)),
                        ("collicurules", Null),
                        ("collversion", locale.map_or(Null, |_| t(collation::VERSION))),
                    ])
                })
                .collect();
            for (k, (name, def)) in cat.colls.iter().enumerate() {
                let icu = def["provider"] == "icu";
                let text = |key: &str| def[key].as_str().map_or(Null, t);
                rows.push(row(vec![
                    ("oid", int(cat.coll_oid(k))),
                    ("collname", t(rel_part(name))),
                    ("collnamespace", int(cat.schema_oid(schema_part(name)))),
                    ("collowner", int(10)),
                    ("collprovider", t(if icu { "i" } else { "c" })),
                    ("collisdeterministic", b(def["deterministic"] != false)),
                    ("collencoding", int(def["encoding"].as_i64().unwrap_or(-1))),
                    ("collcollate", if icu { Null } else { text("collate") }),
                    ("collctype", if icu { Null } else { text("ctype") }),
                    ("colliculocale", if icu { text("locale") } else { Null }),
                    ("collicurules", Null),
                    ("collversion", if icu { text("version") } else { Null }),
                ]));
            }
            rows
        }
        "pg_enum" => {
            let mut n = 0;
            let mut rows = vec![];
            for (k, (_, labels)) in cat.enums.iter().enumerate() {
                for (i, l) in labels.iter().enumerate() {
                    rows.push(row(vec![
                        ("oid", int(cat.label_oid(n))),
                        ("enumtypid", int(cat.enum_oid(k))),
                        ("enumsortorder", Value::Float(cat.enum_orders[k].get(i).copied().unwrap_or(i as f64 + 1.0))),
                        ("enumlabel", t(l)),
                    ]));
                    n += 1;
                }
            }
            rows
        }
        "pg_proc" => cat
            .funcs
            .iter()
            .enumerate()
            .map(|(k, f)| {
                let ret = match &f.ret {
                    _ if f.procedure => 2278,
                    Ret::Value(ty) | Ret::SetOf(ty) => cat.type_oid(*ty),
                    Ret::Table(_) | Ret::RowsOf(_) => 2249,
                    Ret::Trigger => 2279,
                    Ret::Void => 2278,
                };
                let oids = |v: &[i64]| v.iter().map(i64::to_string).collect::<Vec<_>>().join(" ");
                let args: Vec<i64> = f.args.iter().map(|a| cat.type_oid(a.1)).collect();
                let braces = |v: Vec<String>| t(&format!("{{{}}}", v.join(",")));
                let acl = super::acl::shown(&cat.reg, cat.acls.get(&format!("func/{}", f.sig())), 10);
                let named = f.args.iter().any(|a| a.0.is_some()) || matches!(f.ret, Ret::Table(_));
                let mut names: Vec<String> = f.args.iter().map(|a| a.0.clone().unwrap_or_default()).collect();
                let (mut all, mut modes) = (vec![], vec![]);
                if let Ret::Table(cols) = &f.ret {
                    all = args
                        .iter()
                        .map(i64::to_string)
                        .chain(cols.iter().map(|c| cat.type_oid(c.1).to_string()))
                        .collect();
                    modes =
                        f.args.iter().map(|_| "i".to_string()).chain(cols.iter().map(|_| "t".to_string())).collect();
                    names.extend(cols.iter().map(|c| c.0.clone()));
                }
                row(vec![
                    ("oid", int(cat.func_oid(k))),
                    ("proname", t(rel_part(&f.name))),
                    ("pronamespace", int(cat.schema_oid(schema_part(&f.name)))),
                    ("proowner", int(acl.0)),
                    ("prolang", int(if f.lang == Lang::Sql { 14 } else { 13612 })),
                    ("procost", Value::Float(f.cost.unwrap_or(100.0))),
                    ("prorows", Value::Float(f.tuning.rows.unwrap_or(if f.ret.sets() { 1000.0 } else { 0.0 }))),
                    ("provariadic", int(0)),
                    ("prosupport", t("-")),
                    ("prokind", t(if f.procedure { "p" } else { "f" })),
                    ("prosecdef", b(f.definer)),
                    ("proleakproof", b(f.tuning.leakproof)),
                    ("proisstrict", b(f.strict)),
                    ("proretset", b(f.ret.sets())),
                    (
                        "provolatile",
                        t(match f.volatility {
                            Volatility::Immutable => "i",
                            Volatility::Stable => "s",
                            Volatility::Volatile => "v",
                        }),
                    ),
                    ("proparallel", t(&f.tuning.parallel.unwrap_or('u').to_string())),
                    ("pronargs", int(args.len() as i64)),
                    ("pronargdefaults", int(f.defaults.iter().filter(|d| d.is_some()).count() as i64)),
                    ("prorettype", int(ret)),
                    ("proargtypes", t(&oids(&args))),
                    ("proallargtypes", if all.is_empty() { Null } else { braces(all) }),
                    ("proargmodes", if modes.is_empty() { Null } else { braces(modes) }),
                    ("proargnames", if named { braces(names) } else { Null }),
                    ("prosrc", t(&f.body)),
                    // (its SET clauses, `name=value`, a list's values joined as Postgres's GUC flattening joins them)
                    (
                        "proconfig",
                        match f.config.is_empty() {
                            true => Null,
                            false => braces(
                                f.config
                                    .iter()
                                    .map(|(n, vs)| super::roles::array_elem(&format!("{n}={}", vs.join(", "))))
                                    .collect(),
                            ),
                        },
                    ),
                    ("proacl", acl.1),
                ])
            })
            .collect(),
        // what each statement took, its constants taken out (see `stmtstats`), in the world it is read in
        // (by who ran them where: another role's text and id are hidden from a caller without
        // pg_read_all_stats, as the extension's view hides them)
        "pg_stat_statements" => super::stmtstats::all()
            .into_iter()
            .map(|e| {
                let (f, st, stddev) = (Value::Float, e.stat, e.stddev);
                let dbid = cat.worlds.iter().position(|w| *w == e.world).map_or(Null, |k| int(16384 + k as i64));
                row(vec![
                    ("userid", int(e.userid)),
                    ("dbid", dbid),
                    // (only top-level statements are counted: pg_stat_statements.track's default, `top`)
                    ("toplevel", b(true)),
                    ("queryid", e.query.as_ref().map_or(Null, |(_, id)| int(*id))),
                    ("query", e.query.as_ref().map_or_else(|| t("<insufficient privilege>"), |(q, _)| t(q))),
                    ("calls", int(st.calls as i64)),
                    ("total_exec_time", f(st.total_ms)),
                    ("min_exec_time", f(st.min_ms)),
                    ("max_exec_time", f(st.max_ms)),
                    ("mean_exec_time", f(st.mean_ms)),
                    ("stddev_exec_time", f(stddev)),
                    ("rows", int(st.rows as i64)),
                ])
            })
            .collect(),
        // the open connections, as the Postgres port keeps them (see `register_backend`)
        "pg_stat_activity" => {
            // (a row is waited for in the querying session's database: see `conn_waits`)
            let core = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| c.core.clone())));
            let mut waits = conn_waits(core.as_deref());
            backends()
                .into_iter()
                .map(|(pid, i)| {
                    let wait = waits.remove(&pid);
                    let world = cat.worlds.iter().position(|w| *w == i.world).map_or(0, |k| 16384 + k as i64);
                    let ts = |us: Option<i64>| us.map_or(Value::Null, Value::TimestampTz);
                    row(vec![
                        ("datid", int(world)),
                        ("datname", t(&i.world)),
                        ("pid", int(pid.into())),
                        ("usesysid", int(10)),
                        ("usename", t(&i.user)),
                        ("application_name", t(&i.app)),
                        ("client_addr", i.addr.as_ref().map_or(Null, |(a, _)| t(a))),
                        ("client_port", int(i.addr.as_ref().map_or(-1, |(_, p)| i64::from(*p)))),
                        ("backend_start", Value::TimestampTz(i.started)),
                        ("xact_start", ts(i.xact_start)),
                        ("query_start", ts(i.query_start)),
                        ("state_change", Value::TimestampTz(i.state_change)),
                        ("wait_event_type", wait.as_ref().map_or(Null, |_| t("Lock"))),
                        ("wait_event", wait.as_ref().map_or(Null, |w| t(w.event))),
                        ("state", t(i.state)),
                        ("query", t(&i.query)),
                        ("backend_type", t("client backend")),
                    ])
                })
                .collect()
        }
        // the cumulative statistics views: tables', indexes', functions', databases' and the cluster's (see `pgstat`)
        n if super::pgstat::view(n) => super::pgstat::view_rows(n, cat),
        // the session's prepared statements and DECLAREd cursors (see `Session::listing`)
        "pg_prepared_statements" | "pg_cursors" => {
            let ctx = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone()));
            let cols = catalog_schema::columns(name);
            let none = super::Listing::default();
            let l = ctx.as_ref().map_or(&none, |c| &c.listing);
            if name == "pg_cursors" {
                let mut cursors: Vec<_> = l.cursors.iter().collect();
                cursors.sort_by(|x, y| x.0.cmp(&y.0));
                cursors
                    .into_iter()
                    .map(|(n, text, hold, scroll, at)| {
                        (cols.iter())
                            .map(|(c, _)| match c.as_str() {
                                "name" => t(n),
                                "statement" => t(text),
                                "is_holdable" => b(*hold),
                                "is_binary" => b(false),
                                "is_scrollable" => b(*scroll),
                                "creation_time" => Value::TimestampTz(*at),
                                _ => Null,
                            })
                            .collect()
                    })
                    .collect()
            } else {
                // (the protocol's unnamed statement isn't listed)
                let mut prepared: Vec<_> = l.prepared.iter().filter(|(n, _)| !n.is_empty()).collect();
                prepared.sort_by(|x, y| x.0.cmp(y.0));
                prepared
                    .into_iter()
                    .map(|(n, p)| {
                        // regtype[]: each type's oid (an enum's its own), shown as the name regtype shows
                        let names = |types: &[Type]| {
                            let oids: Vec<Value> = types.iter().map(|&ty| Value::Int(cat.regtype_oid(ty))).collect();
                            Value::Text(array_text(oids.iter()))
                        };
                        (cols.iter())
                            .map(|(c, _)| match c.as_str() {
                                "name" => t(n),
                                "statement" => t(&p.text),
                                "prepare_time" => Value::TimestampTz(p.at),
                                "parameter_types" => names(&p.types),
                                "result_types" => p.result.as_deref().map_or(Null, names),
                                "from_sql" => b(p.from_sql),
                                "generic_plans" | "custom_plans" => int(0),
                                _ => Null,
                            })
                            .collect()
                    })
                    .collect()
            }
        }
        // the advisory locks held and waited for in this database, every session's (Chronos takes
        // no other locks a client can see), with the pid of the connection holding or waiting:
        // pg_stat_activity's, or the querying session's own. A session that isn't a connection
        // (an embedding program's) has none to show.
        "pg_locks" => {
            let ctx = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone()));
            let here = ctx.as_ref().map(|c| (Arc::as_ptr(&c.core) as usize, c.session, c.pid, c.home.clone()));
            let pids = backend_pids();
            let datoid = here.as_ref().and_then(|h| cat.worlds.iter().position(|w| *w == h.3));
            let cols = catalog_schema::columns(name);
            // the running statement's virtual transaction, which its session holds as Postgres's does (ExclusiveLock
            // on its virtualxid, `backend/local transaction`)
            let own = here.as_ref().map(|h| {
                let vxid = format!("{}/{}", h.1, h.2);
                (cols.iter())
                    .map(|(c, _)| match c.as_str() {
                        "locktype" => t("virtualxid"),
                        "virtualxid" | "virtualtransaction" => t(&vxid),
                        "pid" => int(h.2.into()),
                        "mode" => t("ExclusiveLock"),
                        "granted" | "fastpath" => b(true),
                        _ => Null,
                    })
                    .collect::<Vec<Value>>()
            });
            // the relations LOCK TABLE locked in the running session's transaction
            let rel_locks: Vec<(String, &'static str)> = ctx
                .as_ref()
                .map(|c| {
                    c.state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .rel_locks
                        .iter()
                        .map(|(n, m, _)| (n.clone(), *m))
                        .collect()
                })
                .unwrap_or_default();
            let relations: Vec<Vec<Value>> = rel_locks
                .iter()
                .filter_map(|(n, mode)| {
                    let oid = cat.oid_of(n)?;
                    let h = here.as_ref()?;
                    Some(
                        (cols.iter())
                            .map(|(c, _)| match c.as_str() {
                                "locktype" => t("relation"),
                                "database" => datoid.map_or(Null, |i| int(16384 + i as i64)),
                                "relation" => int(oid),
                                "virtualtransaction" => t(&format!("{}/{}", h.1, h.2)),
                                "pid" => int(h.2.into()),
                                "mode" => t(mode),
                                "granted" => b(true),
                                "fastpath" => b(false),
                                _ => Null,
                            })
                            .collect(),
                    )
                })
                .collect();
            own.into_iter()
                .chain(relations)
                .chain(
                    advisory::listed()
                        .into_iter()
                        .filter(|((db, ..), ..)| here.as_ref().is_some_and(|h| h.0 == *db))
                        .map(|((_, k, two), share, session, granted)| {
                            let mine = here.as_ref().filter(|h| h.1 == session).map(|h| h.2);
                            let pid = pids.get(&session).copied().or(mine);
                            let (classid, objid) = ((k >> 32) as u32, k as u32);
                            (cols.iter())
                                .map(|(c, _)| match c.as_str() {
                                    "locktype" => t("advisory"),
                                    "database" => datoid.map_or(Null, |i| int(16384 + i as i64)),
                                    "classid" => int(classid.into()),
                                    "objid" => int(objid.into()),
                                    "objsubid" => int(if two { 2 } else { 1 }),
                                    "pid" => pid.map_or(Null, |p| int(p.into())),
                                    "mode" => t(if share { "ShareLock" } else { "ExclusiveLock" }),
                                    "granted" => b(granted),
                                    "fastpath" => b(false),
                                    _ => Null,
                                })
                                .collect()
                        }),
                )
                .collect()
        }
        // the build's settings, by the names Postgres's pg_config gives them: where the server's program is, and its
        // version; the paths and flags of a C build Chronos hasn't are "not recorded", as Postgres says of one it
        // wasn't told
        "pg_config" => {
            let bin = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.display().to_string()))
                .unwrap_or_else(|| "not recorded".into());
            let version = format!("PostgreSQL {}", super::SERVER_VERSION);
            PG_CONFIG_NAMES
                .iter()
                .map(|n| {
                    let setting = match *n {
                        "BINDIR" => bin.clone(),
                        "VERSION" => version.clone(),
                        _ => "not recorded".into(),
                    };
                    vec![t(n), t(&setting)]
                })
                .collect()
        }
        // a row for each database (world): one node has no recovery, so no query was cancelled by it
        "pg_stat_database_conflicts" => cat
            .worlds
            .iter()
            .enumerate()
            .map(|(k, w)| {
                catalog_schema::columns(name)
                    .into_iter()
                    .map(|(c, ty)| match c.as_str() {
                        "datid" => int(16384 + k as i64),
                        "datname" => t(w),
                        _ if ty == Type::BigInt => int(0),
                        _ => Null,
                    })
                    .collect()
            })
            .collect(),
        // the large objects a role made, with their owners and grants (see `largeobj`), by oid
        "pg_largeobject_metadata" => {
            let mut los: Vec<(i64, &super::acl::Entry)> =
                cat.acls.iter().filter_map(|(k, e)| Some((k.strip_prefix("lo/")?.parse::<i64>().ok()?, e))).collect();
            los.sort_by_key(|(oid, _)| *oid);
            los.into_iter()
                .map(|(oid, e)| {
                    row(vec![
                        ("oid", int(oid)),
                        ("lomowner", int(e.owner.unwrap_or(10))),
                        ("lomacl", e.acl.as_ref().map_or(Null, |a| t(&super::acl::text(&cat.reg, a)))),
                    ])
                })
                .collect()
        }
        // ALTER DEFAULT PRIVILEGES's (see `acl`)
        "pg_default_acl" => super::acl::default_acl_rows(&cat.reg, &cat.acls, |s| cat.schema_oid(s))
            .into_iter()
            .enumerate()
            .map(|(n, (role, schema, objtype, acl))| {
                row(vec![
                    ("oid", int(DEFAULT_ACL_OIDS + n as i64)),
                    ("defaclrole", int(role)),
                    ("defaclnamespace", int(schema)),
                    ("defaclobjtype", t(&objtype.to_string())),
                    ("defaclacl", t(&acl)),
                ])
            })
            .collect(),
        "pg_policy" => {
            let oids = |v: &[i64]| t(&format!("{{{}}}", v.iter().map(i64::to_string).collect::<Vec<_>>().join(",")));
            let mut rows = vec![];
            for (i, r) in cat.rels.iter().enumerate() {
                for p in &r.policies {
                    rows.push(row(vec![
                        ("oid", int(cat.policy_oid(rows.len()))),
                        ("polname", t(&p.name)),
                        ("polrelid", int(cat.rel_oid(i))),
                        ("polcmd", t(&p.cmd.to_string())),
                        ("polpermissive", b(p.permissive)),
                        ("polroles", oids(&p.roles)),
                        ("polqual", p.using.as_deref().map_or(Null, t)),
                        ("polwithcheck", p.check.as_deref().map_or(Null, t)),
                    ]));
                }
            }
            rows
        }
        "pg_policies" => {
            let mut rows = vec![];
            for r in &cat.rels {
                for p in &r.policies {
                    let names: Vec<String> = p.role_names.iter().map(|n| super::roles::array_elem(n)).collect();
                    rows.push(row(vec![
                        ("schemaname", t(schema_part(&r.name))),
                        ("tablename", t(rel_part(&r.name))),
                        ("policyname", t(&p.name)),
                        ("permissive", t(if p.permissive { "PERMISSIVE" } else { "RESTRICTIVE" })),
                        ("roles", t(&format!("{{{}}}", names.join(",")))),
                        ("cmd", t(p.cmd_name())),
                        ("qual", p.using.as_deref().map_or(Null, t)),
                        ("with_check", p.check.as_deref().map_or(Null, t)),
                    ]));
                }
            }
            rows
        }
        "pg_trigger" => {
            let mut rows: Vec<Vec<Value>> = cat
                .triggers
                .iter()
                .enumerate()
                .map(|(n, (i, k))| {
                    let r = &cat.rels[*i];
                    let (func, tgtype, nargs, attnums) = &r.trigger_meta[*k];
                    let (old, new, cloned) = r.trigger_links.get(*k).cloned().unwrap_or_default();
                    let name = &r.triggers[*k].0;
                    // a partition's copy: its parent's trigger of the name
                    let parent = match (&r.hier.of, cloned) {
                        (Some((p, ..)), true) => cat
                            .triggers
                            .iter()
                            .position(|(pi, pk)| cat.rels[*pi].name == *p && cat.rels[*pi].triggers[*pk].0 == *name)
                            .map_or(0, |m| cat.trigger_oid(m)),
                        _ => 0,
                    };
                    let opt = |s: &Option<String>| s.as_deref().map_or(Value::Null, t);
                    // a constraint trigger's constraint (contype `t`), and the table its FROM names
                    let con = r.trigger_cons.get(*k).cloned().flatten();
                    let con_oid = con
                        .as_ref()
                        .and_then(|_| cat.cons.iter().position(|(ci, c)| *ci == *i && c.kind == 't' && c.name == *name))
                        .map_or(0, |p| cat.con_oid(p));
                    let from_oid = con
                        .as_ref()
                        .and_then(|c| c.2.as_ref())
                        .and_then(|f| cat.rels.iter().position(|x| x.name == *f))
                        .map_or(0, |p| cat.rel_oid(p));
                    let (deferrable, deferred) = con.as_ref().map_or((false, false), |c| (c.0, c.1));
                    row(vec![
                        ("oid", int(cat.trigger_oid(n))),
                        ("tgrelid", int(cat.rel_oid(*i))),
                        ("tgparentid", int(parent)),
                        ("tgname", t(name)),
                        // (pg_catalog's own trigger functions, built in here, by their oids in Postgres's catalog)
                        (
                            "tgfoid",
                            int(cat.trigger_function(func).map_or_else(
                                || match func.as_str() {
                                    "tsvector_update_trigger" | "tsvector_update_trigger_column" => {
                                        super::catalog_schema::builtin_procs(func).first().copied().unwrap_or(0)
                                    }
                                    _ => 0,
                                },
                                |f| cat.func_oid(f),
                            )),
                        ),
                        ("tgtype", int(*tgtype)),
                        ("tgenabled", t(&r.triggers[*k].2.to_string())),
                        ("tgoldtable", opt(&old)),
                        ("tgnewtable", opt(&new)),
                        ("tgisinternal", b(false)),
                        ("tgconstrrelid", int(from_oid)),
                        ("tgconstrindid", int(0)),
                        ("tgconstraint", int(con_oid)),
                        ("tgdeferrable", b(deferrable)),
                        ("tginitdeferred", b(deferred)),
                        ("tgnargs", int(*nargs as i64)),
                        ("tgattr", t(&attnums.iter().map(i64::to_string).collect::<Vec<_>>().join(" "))),
                    ])
                })
                .collect();
            rows.extend(foreign_key_triggers(cat));
            rows
        }
        "pg_rewrite" => cat
            .rewrites
            .iter()
            .enumerate()
            .map(|(n, i)| {
                row(vec![
                    ("oid", int(cat.rewrite_oid(n))),
                    ("rulename", t("_RETURN")),
                    ("ev_class", int(cat.rel_oid(*i))),
                    ("ev_type", t("1")),
                    ("ev_enabled", t("O")),
                    ("is_instead", b(true)),
                ])
            })
            .collect(),
        "pg_depend" => cat
            .depends()
            .into_iter()
            .map(|(classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype)| {
                row(vec![
                    ("classid", int(classid)),
                    ("objid", int(objid)),
                    ("objsubid", int(objsubid)),
                    ("refclassid", int(refclassid)),
                    ("refobjid", int(refobjid)),
                    ("refobjsubid", int(refobjsubid)),
                    ("deptype", t(&deptype.to_string())),
                ])
            })
            .collect(),
        _ => return None,
    };
    Some((catalog_schema::columns(name), rows))
}

/// The names pg_config lists, in Postgres 16's order.
const PG_CONFIG_NAMES: [&str; 23] = [
    "BINDIR",
    "DOCDIR",
    "HTMLDIR",
    "INCLUDEDIR",
    "PKGINCLUDEDIR",
    "INCLUDEDIR-SERVER",
    "LIBDIR",
    "PKGLIBDIR",
    "LOCALEDIR",
    "MANDIR",
    "SHAREDIR",
    "SYSCONFDIR",
    "PGXS",
    "CONFIGURE",
    "CC",
    "CPPFLAGS",
    "CFLAGS",
    "CFLAGS_SL",
    "LDFLAGS",
    "LDFLAGS_EX",
    "LDFLAGS_SL",
    "LIBS",
    "VERSION",
];

/// pg_class: tables, views, materialized views, sequences and indexes.
fn class_rows(cat: &Catalog) -> Vec<Vec<Value>> {
    use Value::{Bool as b, Int as int};
    let t = |s: &str| Value::Text(s.into());
    let row = |fields: Vec<(&str, Value)>| catalog_schema::named_row("pg_class", fields);
    let rels = cat.rels.iter().enumerate().filter(|(_, r)| "rvmSpf".contains(r.kind)).map(|(i, r)| {
        let table = matches!(r.kind, 'r' | 'm');
        let natts = if r.kind == 'S' { 3 } else { r.columns.len() as i64 };
        let (owner, acl) = cat.rel_acl(r);
        // a sequence is one page holding one row, as Postgres counts it; a table or index that was never
        // analyzed has none counted (-1), as in Postgres (Chronos keeps no statistics)
        let (pages, tuples) = if r.kind == 'S' { (1, 1.0) } else { (0, -1.0) };
        // a table (or materialized view) with a column whose values may be TOASTed (of a type of no fixed length,
        // stored other than PLAIN) has a TOAST table in Postgres, as needs_toast_table decides; Chronos has none to
        // show, but numbers it apart from every relation (the table's oid with the top bit set)
        let toasted = matches!(r.kind, 'r' | 'm')
            && r.columns.iter().enumerate().any(|(j, (_, ty, ..))| {
                let (own, _) = r.typmods.get(j).copied().unwrap_or((0, -1));
                let oid = if own != 0 { own } else { cat.type_oid(*ty) };
                let (len, storage) = catalog_schema::type_facts(oid).map_or_else(
                    || (if matches!(ty, Type::Enum(_)) { 4 } else { -1 }, "x".to_string()),
                    |f| (f.0, f.3),
                );
                let storage = r.attopts.get(j).map_or(storage, |o| o.0.to_string());
                len == -1 && storage != "p"
            });
        row(vec![
            ("oid", int(cat.rel_oid(i))),
            ("relname", t(rel_part(&r.name))),
            ("relnamespace", int(cat.schema_oid(schema_part(&r.name)))),
            ("reltype", int(if r.kind == 'S' { 0 } else { cat.row_type_oid(i) })),
            ("reloftype", int(cat.of_type_oid(r))),
            ("relowner", int(owner)),
            ("relam", int(if table { 2 } else { 0 })),
            // (a view, a partitioned table and a foreign table have no storage of their own)
            ("relfilenode", int(if matches!(r.kind, 'v' | 'p' | 'f') { 0 } else { cat.rel_oid(i) })),
            ("reltablespace", int(0)),
            ("relpages", int(pages)),
            ("reltuples", Value::Float(tuples)),
            ("relallvisible", int(0)),
            ("reltoastrelid", int(if toasted { cat.rel_oid(i) | (1 << 31) } else { 0 })),
            ("relhasindex", b(!r.indexes.is_empty())),
            ("relisshared", b(false)),
            // (a temporary table's is `t`: see `Catalog::of_session`; an unlogged one's `u`)
            (
                "relpersistence",
                t(if schema_part(&r.name) == super::catalog::TEMP_SCHEMA {
                    "t"
                } else if r.unlogged {
                    "u"
                } else {
                    "p"
                }),
            ),
            ("relkind", t(&r.kind.to_string())),
            ("relnatts", int(natts)),
            ("relchecks", int(r.constraints.iter().filter(|c| c.kind == 'c').count() as i64)),
            ("relhasrules", b(matches!(r.kind, 'v' | 'm'))),
            // (a foreign key is kept by triggers in Postgres, on both tables: pg_dump looks for them by this)
            ("relhastriggers", b(!r.triggers.is_empty() || !r.fks.is_empty() || !r.referenced_by.is_empty())),
            ("relhassubclass", b(!r.hier.children.is_empty())),
            ("relrowsecurity", b(r.row_security.0)),
            ("relforcerowsecurity", b(r.row_security.1)),
            ("relispopulated", b(r.populated)),
            // (only a table's rows can be replicated by key)
            // (what REPLICA IDENTITY set it to: `d` unless it said otherwise)
            (
                "relreplident",
                t(&match r.hier.replident {
                    _ if !matches!(r.kind, 'r' | 'm' | 'p') => 'n',
                    '\0' => 'd',
                    k => k,
                }
                .to_string()),
            ),
            ("relispartition", b(r.hier.of.is_some())),
            ("relpartbound", r.hier.of.as_ref().map_or(Value::Null, |(_, text, _)| t(text))),
            ("reloptions", super::reloptions::shown(&r.hier.reloptions)),
            ("relrewrite", int(0)),
            ("relfrozenxid", int(0)),
            ("relminmxid", int(0)),
            ("relacl", acl),
            (
                "reloptions",
                match r.reloptions.is_empty() {
                    true => Value::Null,
                    false => t(&format!(
                        "{{{}}}",
                        r.reloptions
                            .iter()
                            .map(|(o, v)| super::roles::array_elem(&format!("{o}={v}")))
                            .collect::<Vec<_>>()
                            .join(",")
                    )),
                },
            ),
        ])
    });
    let indexes = cat.idx.iter().enumerate().map(|(k, (i, x))| {
        let r = &cat.rels[*i];
        row(vec![
            ("oid", int(cat.idx_oid(k))),
            ("relname", t(&x.name)),
            ("relnamespace", int(cat.schema_oid(schema_part(&r.name)))),
            ("reltype", int(0)),
            ("reloftype", int(0)),
            // (an index is its table's owner's)
            ("relowner", int(cat.rel_acl(r).0)),
            ("relam", int(super::catalog::am_oid(x.method))),
            ("relfilenode", int(cat.idx_oid(k))),
            ("reltablespace", int(0)),
            ("relpages", int(0)),
            ("reltuples", Value::Float(-1.0)),
            ("relallvisible", int(0)),
            ("reltoastrelid", int(0)),
            ("relhasindex", b(false)),
            ("relisshared", b(false)),
            (
                "relpersistence",
                t(if schema_part(&r.name) == super::catalog::TEMP_SCHEMA {
                    "t"
                } else if r.unlogged {
                    "u"
                } else {
                    "p"
                }),
            ),
            ("relkind", t(if r.kind == 'p' { "I" } else { "i" })),
            (
                "reloptions",
                r.hier
                    .index_reloptions
                    .iter()
                    .find(|(n, _)| *n == x.name)
                    .map_or(Value::Null, |(_, o)| super::reloptions::shown(o)),
            ),
            ("relnatts", int(x.cols.len() as i64)),
            ("relchecks", int(0)),
            ("relhasrules", b(false)),
            ("relhastriggers", b(false)),
            // (a partitioned table's index has subclasses once a partition's index is attached to it)
            (
                "relhassubclass",
                b(r.kind == 'p' && cat.idx.iter().any(|(_, y)| y.given.as_deref() == Some(x.name.as_str()))),
            ),
            ("relrowsecurity", b(false)),
            ("relforcerowsecurity", b(false)),
            ("relispopulated", b(true)),
            ("relreplident", t("n")),
            ("relispartition", b(x.given.is_some())),
            ("relrewrite", int(0)),
            ("relfrozenxid", int(0)),
            ("relminmxid", int(0)),
        ])
    });
    rels.chain(indexes).collect()
}

/// The system columns every table, materialized view and sequence has, as Postgres numbers them: name, number, type,
/// length, and alignment (each passed by value but `ctid`).
const SYSTEM_COLUMNS: [(&str, i64, i64, i64, &str); 6] = [
    ("tableoid", -6, 26, 4, "i"),
    ("cmax", -5, 29, 4, "i"),
    ("xmax", -4, 28, 4, "i"),
    ("cmin", -3, 29, 4, "i"),
    ("xmin", -2, 28, 4, "i"),
    ("ctid", -1, 27, 6, "s"),
];

/// A sequence's columns, as reading it as a table gives them: name, type, length, alignment.
const SEQUENCE_COLUMNS: [(&str, i64, i64, &str); 3] =
    [("last_value", 20, 8, "d"), ("log_cnt", 20, 8, "d"), ("is_called", 16, 1, "c")];

/// An index expression's column name, as Postgres's FigureIndexColname gives it: `lower` for `lower(a)`, `a` for
/// `(a::varchar)`, `coalesce`, `case`, and `expr` for an operator's.
pub(super) fn expr_name(text: &str) -> String {
    check_expr(text).ok().and_then(|e| figure_name(&e)).filter(|n| n != "?column?").unwrap_or_else(|| "expr".into())
}

/// An index's column names as Postgres's ChooseIndexColumnNames makes them of `words` (its columns' names and
/// its expressions'): one the index has already gets a number, `a`, `a1`, `a2`.
pub(super) fn index_column_names(words: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    for w in words {
        let mut name = w.clone();
        let mut m = 0;
        while out.contains(&name) {
            m += 1;
            name = format!("{w}{m}");
        }
        out.push(name);
    }
    out
}

/// An index expression's type, as its table's columns type it (text where it can't be worked out).
fn expr_type(text: &str, r: &TableInfo) -> Type {
    let cols: Vec<Col> = r.columns.iter().map(|(n, t, ..)| Col::new(&r.name, n, *t)).collect();
    check_expr(text)
        .and_then(|e| Binder::plain(&cols, None).bind(&e))
        .and_then(|e| ty_of(&e, &cols, &[]))
        .unwrap_or(Type::Text)
}

/// The triggers Postgres keeps a foreign key by (tgisinternal): on its table, RI_FKey_check_ins and
/// RI_FKey_check_upd; on the table it references, the action of its ON DELETE and ON UPDATE (RI_FKey_cascade_del,
/// RI_FKey_noaction_upd, ...), which only NO ACTION's can be deferred. Their oids are of a range of their own, after
/// their constraint's.
fn foreign_key_triggers(cat: &Catalog) -> Vec<Vec<Value>> {
    use Value::{Bool as b, Int as int};
    let t = |s: &str| Value::Text(s.into());
    let mut rows = vec![];
    for (p, (i, c)) in cat.cons.iter().enumerate() {
        let Some((target, _, (upd, del), _)) = c.parent.as_ref().filter(|_| c.kind == 'f' && c.given.is_none()) else {
            continue;
        };
        let Some(ti) = cat.rels.iter().position(|r| r.name == *target) else { continue };
        let con = cat.con_oid(p);
        let action = |a: char| match a {
            'c' => "cascade",
            'r' => "restrict",
            'n' => "setnull",
            'd' => "setdefault",
            _ => "noaction",
        };
        let (deferrable, deferred) = c.defer;
        let made = [
            (*i, ti, "c", "RI_FKey_check_ins".to_string(), 5, true),
            (*i, ti, "c", "RI_FKey_check_upd".to_string(), 17, true),
            (ti, *i, "a", format!("RI_FKey_{}_del", action(*del)), 9, *del == 'a'),
            (ti, *i, "a", format!("RI_FKey_{}_upd", action(*upd)), 17, *upd == 'a'),
        ];
        for (j, (on, other, side, func, tgtype, may_defer)) in made.into_iter().enumerate() {
            let oid = 1_000_000_000 + con * 4 + j as i64;
            rows.push(catalog_schema::named_row(
                "pg_trigger",
                vec![
                    ("oid", int(oid)),
                    ("tgrelid", int(cat.rel_oid(on))),
                    ("tgparentid", int(0)),
                    ("tgname", t(&format!("RI_ConstraintTrigger_{side}_{oid}"))),
                    ("tgfoid", int(catalog_schema::builtin_procs(&func).first().copied().unwrap_or(0))),
                    ("tgtype", int(tgtype)),
                    ("tgenabled", t("O")),
                    ("tgisinternal", b(true)),
                    ("tgconstrrelid", int(cat.rel_oid(other))),
                    ("tgconstrindid", int(0)),
                    ("tgconstraint", int(con)),
                    ("tgdeferrable", b(deferrable && may_defer)),
                    ("tginitdeferred", b(deferred && may_defer)),
                    ("tgnargs", int(0)),
                    ("tgattr", t("")),
                ],
            ));
        }
    }
    rows
}

/// pg_attribute: the columns of tables, views and materialized views, typed as Postgres types them; the system
/// columns of tables, materialized views and sequences, a sequence's columns, and an index's.
fn attribute_rows(cat: &Catalog) -> Vec<Vec<Value>> {
    use Value::{Bool as b, Int as int};
    let t = |s: &str| Value::Text(s.into());
    let mut rows = vec![];
    // a column of a type with no facts of its own here (an index's, the system's), as Postgres lists one
    let plain = |rel: i64, name: &str, num: i64, ty: i64, len: i64, byval: bool, align: &str, not_null: bool| {
        catalog_schema::named_row(
            "pg_attribute",
            vec![
                ("attrelid", int(rel)),
                ("attname", t(name)),
                ("atttypid", int(ty)),
                ("attlen", int(len)),
                ("attnum", int(num)),
                ("attcacheoff", int(-1)),
                ("atttypmod", int(-1)),
                ("attndims", int(0)),
                ("attbyval", b(byval)),
                ("attalign", t(align)),
                ("attstorage", t("p")),
                ("attcompression", t("")),
                ("attnotnull", b(not_null)),
                ("atthasdef", b(false)),
                ("atthasmissing", b(false)),
                ("attidentity", t("")),
                ("attgenerated", t("")),
                ("attisdropped", b(false)),
                ("attislocal", b(true)),
                ("attinhcount", int(0)),
                // (the system columns have no statistics target)
                ("attstattarget", int(if num < 0 { 0 } else { -1 })),
                ("attcollation", int(0)),
            ],
        )
    };
    for (i, r) in cat.rels.iter().enumerate().filter(|(_, r)| "rmpSf".contains(r.kind)) {
        for (name, num, ty, len, align) in SYSTEM_COLUMNS {
            rows.push(plain(cat.rel_oid(i), name, num, ty, len, name != "ctid", align, true));
        }
        if r.kind == 'S' {
            for (n, (name, ty, len, align)) in SEQUENCE_COLUMNS.into_iter().enumerate() {
                rows.push(plain(cat.rel_oid(i), name, n as i64 + 1, ty, len, true, align, true));
            }
        }
    }
    // an index's columns: its table's it names, and its expressions (`expr`, `expr1`, ...), typed as they are
    for (k, (i, x)) in cat.idx.iter().enumerate() {
        let r = &cat.rels[*i];
        let mut names: Vec<String> = vec![];
        for (n, (&col, part)) in x.cols.iter().zip(&x.parts).enumerate() {
            let at = if col > 0 { r.at_attnum(col) } else { None };
            let (base, ty, own, typmod) = match at {
                Some(c) => {
                    let (own, typmod) = r.typmods.get(c).copied().unwrap_or((0, -1));
                    (r.columns[c].0.clone(), r.columns[c].1, own, typmod)
                }
                // (named as Postgres names it: a function's name, a cast column's, else `expr`)
                None => (expr_name(part), expr_type(part, r), 0, -1),
            };
            // (a name the index has already gets a number, as Postgres numbers them)
            let mut name = base.clone();
            let mut m = 0;
            while names.contains(&name) {
                m += 1;
                name = format!("{base}{m}");
            }
            names.push(name.clone());
            let oid = if own != 0 { own } else { cat.type_oid(ty) };
            let (len, byval, align, storage, collation) = catalog_schema::type_facts(oid).unwrap_or_else(|| match ty {
                Type::Enum(_) => (4, true, "i".into(), "p".into(), 0),
                _ => (-1, false, "i".into(), "x".into(), 0),
            });
            let mut row = plain(cat.idx_oid(k), &name, n as i64 + 1, oid, len, byval, &align, false);
            let schema = catalog_schema::columns("pg_attribute");
            let set = |row: &mut Vec<Value>, c: &str, v: Value| {
                if let Some(p) = schema.iter().position(|(n, _)| n == c) {
                    row[p] = v;
                }
            };
            set(&mut row, "atttypmod", int(typmod));
            // (a plain column's storage and compression are its table column's, as SET STORAGE keeps them)
            let opts = at.and_then(|c| r.attopts.get(c));
            set(&mut row, "attstorage", t(&opts.map_or(storage, |o| o.0.to_string())));
            if let Some(m) = opts.and_then(|o| o.2) {
                set(&mut row, "attcompression", t(&m.to_string()));
            }
            // (an index's column has the index's collation, as pg_index.indcollation has it)
            let collation = x.colls.get(n).copied().flatten().map_or(collation, |c| cat.collation_oid(c));
            set(&mut row, "attcollation", int(collation));
            set(&mut row, "attndims", int(i64::from(matches!(ty, Type::Array(_)))));
            rows.push(row);
        }
    }
    for (i, r) in cat.rels.iter().enumerate().filter(|(_, r)| "rvmpf".contains(r.kind)) {
        // a dropped column's row stays, renamed and of no type, as Postgres keeps it
        for &n in &r.dropped {
            let name = format!("........pg.dropped.{n}........");
            let mut row = plain(cat.rel_oid(i), &name, n, 0, -1, false, "i", false);
            let schema = catalog_schema::columns("pg_attribute");
            if let Some(p) = schema.iter().position(|(c, _)| c == "attisdropped") {
                row[p] = b(true);
            }
            rows.push(row);
        }
        for (j, (name, ty, not_null, default)) in r.columns.iter().enumerate() {
            let (own, typmod) = r.typmods.get(j).copied().unwrap_or((0, -1));
            let oid = if own != 0 { own } else { cat.type_oid(*ty) };
            // an enum, or an array of one, has the facts of an enum's own kind
            let (len, byval, align, storage, collation) = catalog_schema::type_facts(oid).unwrap_or_else(|| match ty {
                Type::Enum(_) => (4, true, "i".into(), "p".into(), 0),
                _ => (-1, false, "i".into(), "x".into(), 0),
            });
            let array = matches!(ty, Type::Array(_));
            // a text column's own collation, where it has one other than the default
            let collation = match r.colls.get(j).copied().flatten() {
                Some(c) if c != Coll::DEFAULT => cat.collation_oid(c),
                _ => collation,
            };
            rows.push(catalog_schema::named_row(
                "pg_attribute",
                vec![
                    ("attrelid", int(cat.rel_oid(i))),
                    ("attname", t(name)),
                    ("atttypid", int(oid)),
                    ("attlen", int(len)),
                    ("attnum", int(r.attnum(j))),
                    ("attcacheoff", int(-1)),
                    ("atttypmod", int(typmod)),
                    ("attndims", int(i64::from(array))),
                    ("attbyval", b(byval)),
                    ("attalign", t(&align)),
                    // (as SET STORAGE and SET COMPRESSION left them: see `ColOpts`)
                    ("attstorage", t(&r.attopts.get(j).map_or(storage, |o| o.0.to_string()))),
                    ("attcompression", t(&r.attopts.get(j).and_then(|o| o.2).map(String::from).unwrap_or_default())),
                    ("attnotnull", b(*not_null)),
                    ("atthasdef", b(default.is_some())),
                    ("atthasmissing", b(false)),
                    (
                        "attidentity",
                        t(&r.identity.get(j).filter(|c| **c != ' ').map(char::to_string).unwrap_or_default()),
                    ),
                    ("attgenerated", t(if r.generated.get(j).is_some_and(Option::is_some) { "s" } else { "" })),
                    ("attisdropped", b(false)),
                    ("attislocal", b(r.hier.cols.get(j).is_none_or(|c| c.0))),
                    ("attinhcount", int(r.hier.cols.get(j).map_or(0, |c| c.1))),
                    ("attstattarget", int(r.attopts.get(j).map_or(-1, |o| o.1))),
                    ("attcollation", int(collation)),
                    // (a foreign table's column's options, for its wrapper)
                    (
                        "attfdwoptions",
                        r.foreign
                            .as_ref()
                            .and_then(|f| f.cols.get(j))
                            .map_or(Value::Null, super::foreign::options_value),
                    ),
                    (
                        "attacl",
                        cat.acls
                            .get(&format!("col/{}/{name}", r.name))
                            .and_then(|e| e.acl.as_ref())
                            .map_or(Value::Null, |a| t(&super::acl::text(&cat.reg, a))),
                    ),
                ],
            ));
        }
    }
    rows
}

/// A type that keeps a length or precision, as `format_type` writes it: `character varying(20)`,
/// `character(3)`, `numeric(10,2)` (and `smallint`, which a column holds as an integer of 16 bits).
/// None for any other type, or a varchar, char or numeric without its modifier.
pub fn sized_type(oid: i64, typmod: i64) -> Option<String> {
    Some(match oid {
        21 => "smallint".into(),
        1043 if typmod < 4 => "character varying".into(),
        1043 => format!("character varying({})", typmod - 4),
        1560 if typmod >= 0 => format!("bit({typmod})"),
        1562 if typmod >= 0 => format!("bit varying({typmod})"),
        1042 if typmod < 4 => "character".into(),
        1042 => format!("character({})", typmod - 4),
        // (an array of varchar(n) or char(n): its elements' type, and its typmod, which is theirs)
        1015 => format!("{}[]", sized_type(1043, typmod)?),
        1014 => format!("{}[]", sized_type(1042, typmod)?),
        // (numerictypmodout: always both, the scale 11 bits and signed)
        1700 if typmod >= 4 => {
            let m = typmod - 4;
            format!("numeric({},{})", m >> 16, ((m & 0x7ff) ^ 1024) - 1024)
        }
        _ => return None,
    })
}
