//! Object addresses, as Postgres's objectaddress.c makes them: pg_get_object_address (the catalog, oid and column
//! an object's type and names give, as get_object_address finds them), pg_identify_object and
//! pg_identify_object_as_address (an address's type, schema, name and identity, and the names and arguments that give
//! it back, as getObjectIdentityParts writes them) and pg_describe_object (getObjectDescription's text). They read the
//! oids Chronos's catalog serves, so they agree with pg_class, pg_proc, pg_type and the others: an oid of a catalog
//! Chronos has no rows of (pg_cast, pg_amop, pg_event_trigger, ...) names nothing, and an object found by its names
//! that has no oid here (an aggregate made with CREATE AGGREGATE, a domain's constraint) is 0A000.

use super::catalog::{self, Catalog, Obj};
use super::*;

/// pg_get_object_address, as `CtxFn::ObjAddr` numbers the four functions.
pub(super) const GET_ADDRESS: u8 = 0;
/// pg_identify_object
pub(super) const IDENTIFY: u8 = 1;
/// pg_identify_object_as_address
pub(super) const AS_ADDRESS: u8 = 2;
/// pg_describe_object
pub(super) const DESCRIBE: u8 = 3;

/// What function `which` gives: the records of pg_get_object_address's, pg_identify_object's and
/// pg_identify_object_as_address's OUT parameters, and pg_describe_object's text.
pub(super) fn result(which: u8) -> Type {
    let record = |fs: &[(&str, Type)]| Type::record(fs.iter().map(|(n, t)| (n.to_string(), *t)).collect());
    match which {
        GET_ADDRESS => record(&[("classid", Type::Oid), ("objid", Type::Oid), ("objsubid", Type::Int)]),
        IDENTIFY => {
            record(&[("type", Type::Text), ("schema", Type::Text), ("name", Type::Text), ("identity", Type::Text)])
        }
        AS_ADDRESS => {
            record(&[("type", Type::Text), ("object_names", Type::Array(25)), ("object_args", Type::Array(25))])
        }
        _ => Type::Text,
    }
}

// ---------------------------------------------------------------- the catalogs an address names

const PG_CLASS: i64 = 1259;
const PG_PROC: i64 = 1255;
const PG_TYPE: i64 = 1247;
const PG_CAST: i64 = 2605;
const PG_COLLATION: i64 = 3456;
const PG_CONSTRAINT: i64 = 2606;
const PG_CONVERSION: i64 = 2607;
const PG_ATTRDEF: i64 = 2604;
const PG_LANGUAGE: i64 = 2612;
const PG_LARGEOBJECT: i64 = 2613;
const PG_OPERATOR: i64 = 2617;
const PG_OPCLASS: i64 = 2616;
const PG_OPFAMILY: i64 = 2753;
const PG_AM: i64 = 2601;
const PG_AMOP: i64 = 2602;
const PG_AMPROC: i64 = 2603;
const PG_REWRITE: i64 = 2618;
const PG_TRIGGER: i64 = 2620;
const PG_NAMESPACE: i64 = 2615;
const PG_STATISTIC_EXT: i64 = 3381;
const PG_TS_PARSER: i64 = 3601;
const PG_TS_DICT: i64 = 3600;
const PG_TS_TEMPLATE: i64 = 3764;
const PG_TS_CONFIG: i64 = 3602;
const PG_AUTHID: i64 = 1260;
const PG_AUTH_MEMBERS: i64 = 1261;
const PG_DATABASE: i64 = 1262;
const PG_TABLESPACE: i64 = 1213;
const PG_FDW: i64 = 2328;
const PG_SERVER: i64 = 1417;
const PG_USER_MAPPING: i64 = 1418;
const PG_DEFAULT_ACL: i64 = 826;
const PG_EXTENSION: i64 = 3079;
const PG_EVENT_TRIGGER: i64 = 3466;
const PG_PARAMETER_ACL: i64 = 6243;
const PG_POLICY: i64 = 3256;
const PG_PUBLICATION: i64 = 6104;
const PG_PUBLICATION_NAMESPACE: i64 = 6237;
const PG_PUBLICATION_REL: i64 = 6106;
const PG_SUBSCRIPTION: i64 = 6100;
const PG_TRANSFORM: i64 = 3576;

/// The catalogs getObjectClass knows, each with getObjectTypeDescription's word for its objects (for pg_class,
/// pg_proc and pg_constraint, the word for one that isn't there: the others' depends on the object).
const CLASSES: [(i64, &str); 41] = [
    (PG_CLASS, "relation"),
    (PG_PROC, "routine"),
    (PG_TYPE, "type"),
    (PG_CAST, "cast"),
    (PG_COLLATION, "collation"),
    (PG_CONSTRAINT, "constraint"),
    (PG_CONVERSION, "conversion"),
    (PG_ATTRDEF, "default value"),
    (PG_LANGUAGE, "language"),
    (PG_LARGEOBJECT, "large object"),
    (PG_OPERATOR, "operator"),
    (PG_OPCLASS, "operator class"),
    (PG_OPFAMILY, "operator family"),
    (PG_AM, "access method"),
    (PG_AMOP, "operator of access method"),
    (PG_AMPROC, "function of access method"),
    (PG_REWRITE, "rule"),
    (PG_TRIGGER, "trigger"),
    (PG_NAMESPACE, "schema"),
    (PG_STATISTIC_EXT, "statistics object"),
    (PG_TS_PARSER, "text search parser"),
    (PG_TS_DICT, "text search dictionary"),
    (PG_TS_TEMPLATE, "text search template"),
    (PG_TS_CONFIG, "text search configuration"),
    (PG_AUTHID, "role"),
    (PG_AUTH_MEMBERS, "role membership"),
    (PG_DATABASE, "database"),
    (PG_TABLESPACE, "tablespace"),
    (PG_FDW, "foreign-data wrapper"),
    (PG_SERVER, "server"),
    (PG_USER_MAPPING, "user mapping"),
    (PG_DEFAULT_ACL, "default acl"),
    (PG_EXTENSION, "extension"),
    (PG_EVENT_TRIGGER, "event trigger"),
    (PG_PARAMETER_ACL, "parameter ACL"),
    (PG_POLICY, "policy"),
    (PG_PUBLICATION, "publication"),
    (PG_PUBLICATION_NAMESPACE, "publication namespace"),
    (PG_PUBLICATION_REL, "publication relation"),
    (PG_SUBSCRIPTION, "subscription"),
    (PG_TRANSFORM, "transform"),
];

/// The kinds of object pg_get_object_address takes (Postgres's ObjectType, those its type names map to).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ot {
    Table,
    Index,
    Sequence,
    View,
    MatView,
    ForeignTable,
    Column,
    Aggregate,
    Function,
    Procedure,
    Type,
    Cast,
    Collation,
    TabConstraint,
    DomConstraint,
    Conversion,
    Default,
    Language,
    LargeObject,
    Operator,
    OpClass,
    OpFamily,
    AccessMethod,
    AmOp,
    AmProc,
    Rule,
    Trigger,
    Schema,
    TsParser,
    TsDict,
    TsTemplate,
    TsConfig,
    Role,
    Database,
    Tablespace,
    Fdw,
    Server,
    UserMapping,
    DefAcl,
    Extension,
    EventTrigger,
    ParamAcl,
    Policy,
    Publication,
    PubNamespace,
    PubRel,
    Subscription,
    Transform,
    Statistics,
}

/// Postgres's ObjectTypeMap: the type names getObjectTypeDescription writes, and the kind each reads back as (none:
/// "unsupported object type").
const TYPES: [(&str, Option<Ot>); 59] = [
    ("table", Some(Ot::Table)),
    ("index", Some(Ot::Index)),
    ("sequence", Some(Ot::Sequence)),
    ("toast table", None),
    ("view", Some(Ot::View)),
    ("materialized view", Some(Ot::MatView)),
    ("composite type", None),
    ("foreign table", Some(Ot::ForeignTable)),
    ("table column", Some(Ot::Column)),
    ("index column", None),
    ("sequence column", None),
    ("toast table column", None),
    ("view column", None),
    ("materialized view column", None),
    ("composite type column", None),
    ("foreign table column", Some(Ot::Column)),
    ("aggregate", Some(Ot::Aggregate)),
    ("function", Some(Ot::Function)),
    ("procedure", Some(Ot::Procedure)),
    ("type", Some(Ot::Type)),
    ("cast", Some(Ot::Cast)),
    ("collation", Some(Ot::Collation)),
    ("table constraint", Some(Ot::TabConstraint)),
    ("domain constraint", Some(Ot::DomConstraint)),
    ("conversion", Some(Ot::Conversion)),
    ("default value", Some(Ot::Default)),
    ("language", Some(Ot::Language)),
    ("large object", Some(Ot::LargeObject)),
    ("operator", Some(Ot::Operator)),
    ("operator class", Some(Ot::OpClass)),
    ("operator family", Some(Ot::OpFamily)),
    ("access method", Some(Ot::AccessMethod)),
    ("operator of access method", Some(Ot::AmOp)),
    ("function of access method", Some(Ot::AmProc)),
    ("rule", Some(Ot::Rule)),
    ("trigger", Some(Ot::Trigger)),
    ("schema", Some(Ot::Schema)),
    ("text search parser", Some(Ot::TsParser)),
    ("text search dictionary", Some(Ot::TsDict)),
    ("text search template", Some(Ot::TsTemplate)),
    ("text search configuration", Some(Ot::TsConfig)),
    ("role", Some(Ot::Role)),
    ("role membership", None),
    ("database", Some(Ot::Database)),
    ("tablespace", Some(Ot::Tablespace)),
    ("foreign-data wrapper", Some(Ot::Fdw)),
    ("server", Some(Ot::Server)),
    ("user mapping", Some(Ot::UserMapping)),
    ("default acl", Some(Ot::DefAcl)),
    ("extension", Some(Ot::Extension)),
    ("event trigger", Some(Ot::EventTrigger)),
    ("parameter ACL", Some(Ot::ParamAcl)),
    ("policy", Some(Ot::Policy)),
    ("publication", Some(Ot::Publication)),
    ("publication namespace", Some(Ot::PubNamespace)),
    ("publication relation", Some(Ot::PubRel)),
    ("subscription", Some(Ot::Subscription)),
    ("transform", Some(Ot::Transform)),
    ("statistics object", Some(Ot::Statistics)),
];

/// The kinds named by one bare name (get_object_address_unqualified's).
const UNQUALIFIED: [Ot; 13] = [
    Ot::AccessMethod,
    Ot::Database,
    Ot::EventTrigger,
    Ot::Extension,
    Ot::Fdw,
    Ot::Server,
    Ot::Language,
    Ot::ParamAcl,
    Ot::Publication,
    Ot::Role,
    Ot::Schema,
    Ot::Subscription,
    Ot::Tablespace,
];

/// Postgres 16's text search parser and templates, all in pg_catalog.
const TS_PARSERS: [(i64, &str); 1] = [(3722, "default")];
const TS_TEMPLATES: [(i64, &str); 4] = [(3727, "simple"), (3730, "synonym"), (3733, "ispell"), (3742, "thesaurus")];

/// Postgres's own operator families, operator classes, family members and conversions (see `opfamilies.tsv`).
struct OpData {
    /// oid, access method, name
    families: Vec<(i64, String, String)>,
    /// oid (None where initdb numbers it), access method, name, family
    classes: Vec<(Option<i64>, String, String, String)>,
    /// `o` (pg_amop) or `p` (pg_amproc), `method/family`, left and right types' names, strategy or number
    members: Vec<(char, String, String, String, i64)>,
    /// oid, name
    conversions: Vec<(i64, String)>,
}

fn op_data() -> &'static OpData {
    static DATA: LazyLock<OpData> = LazyLock::new(|| {
        let mut d = OpData { families: vec![], classes: vec![], members: vec![], conversions: vec![] };
        for line in include_str!("opfamilies.tsv").lines().filter(|l| !l.starts_with('#')) {
            let f: Vec<&str> = line.split('\t').collect();
            let num = |i: usize| f.get(i).and_then(|x| x.parse::<i64>().ok());
            let s = |i: usize| f.get(i).map_or_else(String::new, |x| x.to_string());
            match f.first().copied() {
                Some("f") => d.families.push((num(1).unwrap_or(0), s(2), s(3))),
                Some("c") => d.classes.push((num(1), s(2), s(3), s(4))),
                Some(k @ ("o" | "p")) => {
                    d.members.push((if k == "o" { 'o' } else { 'p' }, s(1), s(2), s(3), num(4).unwrap_or(0)))
                }
                Some("v") => d.conversions.push((num(1).unwrap_or(0), s(2))),
                _ => {}
            }
        }
        d
    });
    &DATA
}

// ---------------------------------------------------------------- the four functions

/// Function `which` on its arguments (all three given: the functions are strict).
pub(super) fn call(cat: &Catalog, which: u8, a: &[Value]) -> Result<Value> {
    use Value::*;
    if a.len() < 3 || a.contains(&Null) {
        return Ok(Null);
    }
    if which == GET_ADDRESS {
        let (class, oid, sub) = address(cat, &a[0].to_text(), &a[1], &a[2])?;
        return Ok(Text(composite_text(&[Int(class), Int(oid), Int(sub)])));
    }
    let (class, oid, sub) = (oid_arg(&a[0])?, oid_arg(&a[1])?, int_arg(&a[2])?);
    // (a pinned object's row in pg_depend has neither)
    if which == DESCRIBE && class == 0 && oid == 0 {
        return Ok(Null);
    }
    let (ty, found) = identify(cat, class, oid, sub)?;
    let text = |s: &str| Text(s.to_string());
    let opt = |s: &Option<String>| s.as_deref().map_or(Null, |x| Text(x.to_string()));
    let list = |l: &[String]| Text(array_text(l.iter().map(|s| Text(s.clone())).collect::<Vec<_>>().iter()));
    Ok(match (which, &found) {
        (IDENTIFY, Some(f)) => Text(composite_text(&[text(&ty), opt(&f.schema), opt(&f.name), text(&f.identity)])),
        (IDENTIFY, None) => Text(composite_text(&[text(&ty), Null, Null, Null])),
        (AS_ADDRESS, Some(f)) => Text(composite_text(&[text(&ty), list(&f.names), list(&f.args)])),
        (AS_ADDRESS, None) => Text(composite_text(&[text(&ty), Null, Null])),
        (_, Some(f)) => Text(f.descr.clone()),
        (_, None) => Null,
    })
}

/// An oid argument: an integer or a reg value (a regclass is an oid), as oidin reads one.
fn oid_arg(v: &Value) -> Result<i64> {
    match v {
        Value::Int(n) | Value::Reg(n, _) => catalog::int_oid(*n),
        v => match coerce(Value::Text(v.to_text()), Type::BigInt)? {
            Value::Int(n) => catalog::int_oid(n),
            _ => Ok(0),
        },
    }
}

fn int_arg(v: &Value) -> Result<i64> {
    match coerce(v.clone(), Type::BigInt)? {
        Value::Int(n) => Ok(n),
        _ => Ok(0),
    }
}

/// 22023, as pg_get_object_address's checks of its arguments raise it.
fn bad<T>(message: impl Into<String>) -> Result<T> {
    err("22023", message)
}

fn missing<T>(code: &'static str, message: impl Into<String>) -> Result<T> {
    err(code, message)
}

/// An object found by its names that Chronos's catalog has no row (no oid) for.
fn no_oid<T>(what: &str) -> Result<T> {
    err(UNSUPPORTED, format!("{what} has no oid here: Chronos's catalog has no row for it"))
}

/// The running session's database, which a three-part name's first part must be.
fn current_db() -> String {
    STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| c.home.clone()))).unwrap_or_default()
}

/// A name list as Postgres's NameListToString writes it in messages: the names with dots between, unquoted.
fn joined(names: &[String]) -> String {
    names.join(".")
}

/// A schema's oid (0: none): Postgres's own, or one made here.
fn schema_oid(cat: &Catalog, s: &str) -> i64 {
    cat.schema_oid(s)
}

/// A schema's name by its oid.
fn schema_named(cat: &Catalog, oid: i64) -> Option<String> {
    match oid {
        11 => Some("pg_catalog".into()),
        2200 => Some("public".into()),
        13000 => Some("information_schema".into()),
        o => cat.schemas.iter().find(|s| cat.schema_oid(s) == o).cloned(),
    }
}

/// 3F000 for a schema a name gives that isn't there (LookupExplicitNamespace's).
fn need_schema(cat: &Catalog, s: &str) -> Result<i64> {
    match schema_oid(cat, s) {
        0 => missing("3F000", format!("schema \"{s}\" does not exist")),
        o => Ok(o),
    }
}

/// A name list read as DeconstructQualifiedName reads one: its schema, if it names one, and its name.
pub(super) fn qualified(names: &[String]) -> Result<(Option<&str>, &str)> {
    match names {
        [n] => Ok((None, n.as_str())),
        [s, n] => Ok((Some(s.as_str()), n.as_str())),
        [c, s, n] if *c == current_db() => Ok((Some(s.as_str()), n.as_str())),
        [_, _, _] => err(UNSUPPORTED, format!("cross-database references are not implemented: {}", joined(names))),
        _ => err(SYNTAX, format!("improper qualified name (too many dotted names): {}", joined(names))),
    }
}

/// A type's name read as regtype (and Postgres's LookupTypeNameOid) reads it: its oid, 42704 for one no type has.
fn type_oid(cat: &Catalog, name: &str) -> Result<i64> {
    catalog::reg_oid(cat, Reg::Type, name.trim())
}

/// A text array argument's elements (None for a NULL one).
fn elems(v: &Value) -> Result<Vec<Option<String>>> {
    let vals = array_values(v.clone())?.unwrap_or_default();
    Ok(vals.into_iter().map(|x| if x == Value::Null { None } else { Some(x.to_text()) }).collect())
}

// ---------------------------------------------------------------- relations

/// A relation an address names: a table, view, sequence or foreign table made here (its place in `cat.rels`), an
/// index (its place in `cat.idx`), or one of the catalog's own tables (by oid).
#[derive(Clone, Copy)]
enum Rel {
    Made(usize),
    Index(usize),
    Catalog(i64),
}

/// What a relation's identity and description are made of.
struct RelFacts {
    oid: i64,
    schema: String,
    name: String,
    kind: char,
    cols: Vec<String>,
}

/// The catalog table with this oid, by name (not one of its views, pg_roles, pg_tables, ..., which `facts` would
/// describe as a table: see `catalog_view`).
fn catalog_table(oid: i64) -> Option<&'static str> {
    catalog::CATALOG_OIDS.iter().find(|(n, o)| *o == oid && !super::catalog_view(n)).map(|(n, _)| *n)
}

/// The relation with this oid.
fn rel_of(cat: &Catalog, oid: i64) -> Option<Rel> {
    match cat.find(oid) {
        Some(Obj::Rel(i)) => Some(Rel::Made(i)),
        Some(Obj::Index(..)) => (0..cat.idx.len()).find(|&k| cat.idx_oid(k) == oid).map(Rel::Index),
        Some(_) => None,
        None => catalog_table(oid).map(|_| Rel::Catalog(oid)),
    }
}

fn facts(cat: &Catalog, r: Rel) -> RelFacts {
    match r {
        Rel::Made(i) => {
            let t = &cat.rels[i];
            RelFacts {
                oid: cat.rel_oid(i),
                schema: schema_part(&t.name).to_string(),
                name: rel_part(&t.name).to_string(),
                kind: t.kind,
                cols: t.columns.iter().map(|c| c.0.clone()).collect(),
            }
        }
        Rel::Index(k) => {
            let (i, x) = &cat.idx[k];
            let t = &cat.rels[*i];
            RelFacts {
                oid: cat.idx_oid(k),
                schema: schema_part(&t.name).to_string(),
                name: x.name.clone(),
                kind: if t.kind == 'p' { 'I' } else { 'i' },
                cols: vec![],
            }
        }
        Rel::Catalog(oid) => {
            let name = catalog_table(oid).unwrap_or_default();
            RelFacts {
                oid,
                schema: "pg_catalog".into(),
                name: name.to_string(),
                kind: 'r',
                cols: catalog_schema::columns(name).into_iter().map(|c| c.0).collect(),
            }
        }
    }
}

/// getRelationTypeDescription's word for a relation of this relkind.
fn kind_word(kind: char) -> &'static str {
    match kind {
        'r' | 'p' => "table",
        'i' | 'I' => "index",
        'S' => "sequence",
        't' => "toast table",
        'v' => "view",
        'm' => "materialized view",
        'c' => "composite type",
        'f' => "foreign table",
        _ => "relation",
    }
}

/// A relation's identity: its schema and name, each quoted where it needs it.
fn rel_identity(f: &RelFacts) -> String {
    format!("{}.{}", quoted(&f.schema), quoted(&f.name))
}

/// getRelationDescription: its kind and name, with its schema where the search_path doesn't find it so.
fn rel_descr(cat: &Catalog, f: &RelFacts) -> String {
    let shown = match cat.rel_named(None, &f.name) == Some(f.oid) {
        true => quoted(&f.name),
        false => rel_identity(f),
    };
    format!("{} {shown}", kind_word(f.kind))
}

fn quoted(name: &str) -> String {
    collation::quoted(name)
}

/// A relation a name list names (makeRangeVarFromNameList, then RangeVarGetRelid), with Postgres's errors for one
/// that isn't there.
fn relation(cat: &Catalog, names: &[String]) -> Result<Rel> {
    let (catalog, schema, name) = match names {
        [n] => (None, None, n.as_str()),
        [s, n] => (None, Some(s.as_str()), n.as_str()),
        [c, s, n] => (Some(c.as_str()), Some(s.as_str()), n.as_str()),
        _ => return err(SYNTAX, format!("improper relation name (too many dotted names): {}", joined(names))),
    };
    if let Some(c) = catalog
        && c != current_db()
    {
        return err(UNSUPPORTED, format!("cross-database references are not implemented: \"{}\"", joined(names)));
    }
    if let Some(s) = schema {
        need_schema(cat, s)?;
    }
    let shown = match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    };
    match cat.rel_named(schema, name).and_then(|o| rel_of(cat, o)) {
        Some(r) => Ok(r),
        None => missing(NO_TABLE, format!("relation \"{shown}\" does not exist")),
    }
}

/// A relation of the kind `ot` asks for (get_relation_by_qualified_name), 42809 for one of another kind.
fn relation_of_kind(cat: &Catalog, ot: Ot, names: &[String]) -> Result<i64> {
    let f = facts(cat, relation(cat, names)?);
    let (fits, what) = match ot {
        Ot::Index => (matches!(f.kind, 'i' | 'I'), "an index"),
        Ot::Sequence => (f.kind == 'S', "a sequence"),
        Ot::Table => (matches!(f.kind, 'r' | 'p'), "a table"),
        Ot::View => (f.kind == 'v', "a view"),
        Ot::MatView => (f.kind == 'm', "a materialized view"),
        _ => (f.kind == 'f', "a foreign table"),
    };
    if !fits {
        return err("42809", format!("\"{}\" is not {what}", f.name));
    }
    Ok(f.oid)
}

/// The table (a relation that isn't an index) a name list names, as table_openrv opens it.
fn table(cat: &Catalog, names: &[String]) -> Result<Rel> {
    let r = relation(cat, names)?;
    if let Rel::Index(k) = r {
        return err("42809", format!("\"{}\" is an index", cat.idx[k].1.name));
    }
    Ok(r)
}

// ---------------------------------------------------------------- pg_get_object_address

/// pg_get_object_address(type, names, args): the address the names (and arguments) give an object of the type.
fn address(cat: &Catalog, ttype: &str, names: &Value, args: &Value) -> Result<(i64, i64, i64)> {
    let ot = match TYPES.iter().find(|(n, _)| *n == ttype) {
        None => return bad(format!("unrecognized object type \"{ttype}\"")),
        Some((_, None)) => return bad(format!("unsupported object type \"{ttype}\"")),
        Some((_, Some(o))) => *o,
    };
    let (names, args) = (elems(names)?, elems(args)?);
    let no_nulls = |l: &[Option<String>]| -> Result<Vec<String>> {
        l.iter().map(|x| x.clone().map_or_else(|| bad("name or argument lists may not contain nulls"), Ok)).collect()
    };
    let exactly = |what: &str, n: usize| bad::<()>(format!("{what} list length must be exactly {n}"));
    // a type's name is one element (a type name as SQL writes it); a large object's its oid
    let names = match ot {
        Ot::Type | Ot::Cast | Ot::Transform | Ot::DomConstraint | Ot::LargeObject => {
            if names.len() != 1 {
                exactly("name", 1)?;
            }
            if ot == Ot::LargeObject && names[0].is_none() {
                return bad("large object OID may not be null");
            }
            no_nulls(&names)?
        }
        _ => {
            let n = no_nulls(&names)?;
            if n.is_empty() {
                return bad("name list length must be at least 1");
            }
            n
        }
    };
    let args = no_nulls(&args)?;
    match ot {
        Ot::PubNamespace | Ot::UserMapping => {
            if names.len() != 1 {
                exactly("name", 1)?;
            }
            if args.len() != 1 {
                exactly("argument", 1)?;
            }
        }
        Ot::DomConstraint | Ot::Cast | Ot::PubRel | Ot::DefAcl | Ot::Transform if args.len() != 1 => {
            exactly("argument", 1)?
        }
        Ot::OpFamily | Ot::OpClass if names.len() < 2 => return bad("name list length must be at least 2"),
        Ot::AmOp | Ot::AmProc => {
            if names.len() < 3 {
                return bad("name list length must be at least 3");
            }
            if args.len() != 2 {
                exactly("argument", 2)?;
            }
        }
        Ot::Operator if args.len() != 2 => exactly("argument", 2)?,
        _ => {}
    }
    if UNQUALIFIED.contains(&ot) && names.len() != 1 {
        exactly("name", 1)?;
    }
    resolve(cat, ot, &names, &args)
}

/// get_object_address: the address of the object of kind `ot` these names and arguments (checked) give.
fn resolve(cat: &Catalog, ot: Ot, names: &[String], args: &[String]) -> Result<(i64, i64, i64)> {
    let only = names[0].as_str();
    Ok(match ot {
        Ot::Table | Ot::Index | Ot::Sequence | Ot::View | Ot::MatView | Ot::ForeignTable => {
            (PG_CLASS, relation_of_kind(cat, ot, names)?, 0)
        }
        Ot::Column | Ot::Default => {
            if names.len() < 2 {
                return err(SYNTAX, "column name must be qualified");
            }
            let (relname, att) = (&names[..names.len() - 1], &names[names.len() - 1]);
            let r = relation(cat, relname)?;
            let f = facts(cat, r);
            let at = f.cols.iter().position(|c| c == att);
            if ot == Ot::Column {
                let Some(at) = at else {
                    return missing(
                        "42703",
                        format!("column \"{att}\" of relation \"{}\" does not exist", joined(relname)),
                    );
                };
                return Ok((PG_CLASS, f.oid, at as i64 + 1));
            }
            let def = match (r, at) {
                (Rel::Made(i), Some(j)) => (0..cat.defaults.len()).find(|&n| cat.defaults[n] == (i, j)),
                _ => None,
            };
            match def {
                Some(n) => (PG_ATTRDEF, cat.default_oid(n), 0),
                None => {
                    return missing(
                        "42703",
                        format!(
                            "default value for column \"{att}\" of relation \"{}\" does not exist",
                            joined(relname)
                        ),
                    );
                }
            }
        }
        Ot::Rule | Ot::Trigger | Ot::TabConstraint | Ot::Policy => relobject(cat, ot, names)?,
        Ot::DomConstraint => {
            let domain = type_oid(cat, only)?;
            let (wcore, wname) = catalog::world_owner()?;
            let v = wcore.view_tx(&wname)?;
            let stored = cat.enum_at(domain, false).map(|k| cat.enums[k].0.clone());
            let j: Option<Json> = match &stored {
                Some(n) => v.get(&type_key(n))?.and_then(|raw| serde_json::from_str(&raw).ok()),
                None => None,
            };
            let Some(d) = j.as_ref().map(|j| &j["domain"]).filter(|d| d.is_object()) else {
                return err("42809", format!("\"{only}\" is not a domain"));
            };
            let given = d["constraint_names"].as_array().into_iter().flatten().filter_map(|n| n.as_str());
            let added = d["named"].as_array().into_iter().flatten().filter_map(|n| n[0].as_str());
            let con = &args[0];
            if !given.chain(added).any(|n| n == con.as_str()) {
                return missing(
                    "42704",
                    format!("constraint \"{con}\" for domain {} does not exist", cat.format_type(domain, -1)),
                );
            }
            return no_oid(&format!("constraint \"{con}\" of domain {}", cat.format_type(domain, -1)));
        }
        Ot::Aggregate | Ot::Function | Ot::Procedure => (PG_PROC, routine(cat, ot, names, args)?, 0),
        Ot::Operator => (PG_OPERATOR, operator(cat, names, args)?, 0),
        Ot::Type => (PG_TYPE, type_oid(cat, only)?, 0),
        Ot::Cast => {
            let (from, to) = (type_oid(cat, only)?, type_oid(cat, &args[0])?);
            return no_oid(&format!("the cast from {} to {}", cat.format_type(from, -1), cat.format_type(to, -1)));
        }
        Ot::Transform => {
            let ty = type_oid(cat, only)?;
            language(&args[0])?;
            // (Chronos has no transforms: CREATE TRANSFORM isn't taken)
            return missing(
                "42704",
                format!("transform for type {} language \"{}\" does not exist", cat.format_type(ty, -1), args[0]),
            );
        }
        Ot::Collation => {
            qualified(names)?;
            let text = names.iter().map(|n| quoted(n)).collect::<Vec<_>>().join(".");
            (PG_COLLATION, catalog::reg_oid(cat, Reg::Collation, &text)?, 0)
        }
        Ot::Conversion => {
            let (schema, name) = qualified(names)?;
            if let Some(s) = schema {
                need_schema(cat, s)?;
            }
            let found = op_data().conversions.iter().find(|(_, n)| n == name).filter(|_| in_catalog(schema));
            match found {
                Some((o, _)) => (PG_CONVERSION, *o, 0),
                None => return missing("42704", format!("conversion \"{}\" does not exist", joined(names))),
            }
        }
        Ot::Language => (PG_LANGUAGE, language(only)?, 0),
        Ot::LargeObject => {
            let oid = oid_text(only)?;
            if !catalog::with_world_view(|v| super::largeobj::exists(v, oid))? {
                return missing("42704", format!("large object {oid} does not exist"));
            }
            (PG_LARGEOBJECT, oid, 0)
        }
        Ot::OpClass | Ot::OpFamily => {
            let am = index_am(only)?;
            if ot == Ot::OpFamily {
                (PG_OPFAMILY, opfamily(cat, am, &names[1..])?.0, 0)
            } else {
                (PG_OPCLASS, opclass(cat, am, &names[1..])?, 0)
            }
        }
        Ot::AmOp | Ot::AmProc => {
            let last = &names[names.len() - 1];
            let num: i64 =
                last.trim().chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0);
            let am = index_am(only)?;
            let (_, fam) = opfamily(cat, am, &names[1..names.len() - 1])?;
            let (l, r) = (type_oid(cat, &args[0])?, type_oid(cat, &args[1])?);
            let kind = if ot == Ot::AmOp { 'o' } else { 'p' };
            let key = format!("{am}/{fam}");
            let there = op_data().members.iter().any(|(k, f, lt, rt, n)| {
                *k == kind
                    && *f == key
                    && *n == num
                    && type_oid(cat, lt).is_ok_and(|o| o == l)
                    && type_oid(cat, rt).is_ok_and(|o| o == r)
            });
            let what = if ot == Ot::AmOp { "operator" } else { "function" };
            let member = format!(
                "{what} {num} ({}, {}) of operator family {} for access method {am}",
                args[0],
                args[1],
                quoted(&fam)
            );
            if !there {
                return missing("42704", format!("{member} does not exist"));
            }
            return no_oid(&member);
        }
        // (Postgres's own, or one CREATE ACCESS METHOD made)
        Ot::AccessMethod => match cat.am_oid(only) {
            0 => return missing("42704", format!("access method \"{only}\" does not exist")),
            o => (PG_AM, o, 0),
        },
        Ot::Schema => (PG_NAMESPACE, need_schema(cat, only)?, 0),
        Ot::TsParser | Ot::TsDict | Ot::TsTemplate | Ot::TsConfig => text_search(cat, ot, names)?,
        Ot::Role => match cat.reg.named(only) {
            Some(r) => (PG_AUTHID, r.oid, 0),
            None => return missing("42704", format!("role \"{only}\" does not exist")),
        },
        Ot::Database => match databases(cat).into_iter().find(|(_, n)| n == only) {
            Some((o, _)) => (PG_DATABASE, o, 0),
            None => return missing("3D000", format!("database \"{only}\" does not exist")),
        },
        // (Postgres's two and the world's: see `tablespace`)
        Ot::Tablespace => match cat.spaces.iter().find(|s| s.name == only) {
            Some(s) => (PG_TABLESPACE, s.oid, 0),
            None => return missing("42704", format!("tablespace \"{only}\" does not exist")),
        },
        Ot::Fdw => match cat.foreign.fdw_oid(only) {
            Some(o) => (PG_FDW, o, 0),
            None => return missing("42704", format!("foreign-data wrapper \"{only}\" does not exist")),
        },
        Ot::Server => match cat.foreign.server_oid(only) {
            Some(o) => (PG_SERVER, o, 0),
            None => return missing("42704", format!("server \"{only}\" does not exist")),
        },
        Ot::UserMapping => {
            let (user, server) = (only, args[0].as_str());
            let none = || format!("user mapping for user \"{user}\" on server \"{server}\" does not exist");
            let uid = match user {
                "public" => super::roles::PUBLIC,
                u => match cat.reg.named(u) {
                    Some(r) => r.oid,
                    None => return missing("42704", none()),
                },
            };
            if cat.foreign.server(server).is_none() {
                return missing("42704", format!("server \"{server}\" does not exist"));
            }
            match cat.foreign.mappings.iter().position(|m| m.user == uid && m.server == server) {
                Some(i) => (PG_USER_MAPPING, cat.foreign.mapping_oid(i), 0),
                None => return missing("42704", none()),
            }
        }
        Ot::DefAcl => (PG_DEFAULT_ACL, default_acl(cat, names, &args[0])?, 0),
        Ot::Extension => match extensions(cat).into_iter().find(|(_, n)| n == only) {
            Some((o, _)) => (PG_EXTENSION, o, 0),
            None => return missing("42704", format!("extension \"{only}\" does not exist")),
        },
        Ot::EventTrigger => {
            if catalog::with_world_view(|v| Ok(v.get(&format!("_sonos_defs/evt/{only}"))?.is_some()))? {
                return no_oid(&format!("event trigger \"{only}\""));
            }
            return missing("42704", format!("event trigger \"{only}\" does not exist"));
        }
        Ot::ParamAcl => return no_oid(&format!("parameter ACL \"{only}\"")),
        Ot::Publication => match publication(cat, only) {
            Some((o, _)) => (PG_PUBLICATION, o, 0),
            None => return missing("42704", format!("publication \"{only}\" does not exist")),
        },
        Ot::PubNamespace => {
            let (schema, pubname) = (only, args[0].as_str());
            need_schema(cat, schema)?;
            let Some((_, k)) = publication(cat, pubname) else {
                return missing("42704", format!("publication \"{pubname}\" does not exist"));
            };
            match super::publication::namespace_oids(cat).into_iter().find(|(_, p, s)| *p == k && s == schema) {
                Some((o, ..)) => (PG_PUBLICATION_NAMESPACE, o, 0),
                None => {
                    return missing(
                        "42704",
                        format!("publication schema \"{schema}\" in publication \"{pubname}\" does not exist"),
                    );
                }
            }
        }
        Ot::PubRel => {
            let f = facts(cat, relation(cat, names)?);
            let pubname = args[0].as_str();
            let Some((_, k)) = publication(cat, pubname) else {
                return missing("42704", format!("publication \"{pubname}\" does not exist"));
            };
            let rel = match relation(cat, names)? {
                Rel::Made(i) => Some(i),
                _ => None,
            };
            match super::publication::rel_oids(cat).into_iter().find(|(_, p, i)| *p == k && Some(*i) == rel) {
                Some((o, ..)) => (PG_PUBLICATION_REL, o, 0),
                None => {
                    return missing(
                        "42704",
                        format!("publication relation \"{}\" in publication \"{pubname}\" does not exist", f.name),
                    );
                }
            }
        }
        Ot::Subscription => {
            if catalog::with_world_view(|v| Ok(v.get(&format!("_sonos_defs/sub/{only}/"))?.is_some()))? {
                return no_oid(&format!("subscription \"{only}\""));
            }
            return missing("42704", format!("subscription \"{only}\" does not exist"));
        }
        Ot::Statistics => {
            let (schema, name) = qualified(names)?;
            if let Some(s) = schema {
                need_schema(cat, s)?;
            }
            let schemas = match schema {
                Some(s) => vec![s.to_string()],
                None => path_schemas().unwrap_or_else(|| vec!["public".into()]),
            };
            let all = super::statistics::object_oids(cat);
            let found = schemas.iter().find_map(|s| all.iter().find(|(n, ..)| *n == qualify(s, name)));
            match found {
                Some((_, _, o)) => (PG_STATISTIC_EXT, *o, 0),
                None => return missing("42704", format!("statistics object \"{}\" does not exist", joined(names))),
            }
        }
    })
}

/// Whether a name's schema (none, or pg_catalog) is where Postgres's own objects of a kind only pg_catalog has are.
fn in_catalog(schema: Option<&str>) -> bool {
    schema.is_none_or(|s| s == "pg_catalog")
}

/// A large object's oid as oidin reads its text: 22P02 for text that isn't one.
fn oid_text(text: &str) -> Result<i64> {
    let t = text.trim();
    let bad_oid =
        || SqlError { code: BAD_INPUT, message: format!("invalid input syntax for type oid: \"{text}\""), info: None };
    let n: i64 = t.parse().map_err(|_| bad_oid())?;
    match n {
        0..=0xFFFF_FFFF => Ok(n),
        -0x8000_0000..0 => Ok(n + (1 << 32)),
        _ => err(RANGE, format!("value \"{text}\" is out of range for type oid")),
    }
}

/// A language's oid (Postgres's four: internal, c, sql and plpgsql).
fn language(name: &str) -> Result<i64> {
    match languages().into_iter().find(|(_, n)| n == name) {
        Some((o, _)) => Ok(o),
        None => missing("42704", format!("language \"{name}\" does not exist")),
    }
}

fn languages() -> Vec<(i64, String)> {
    catalog_schema::builtin_rows("pg_language")
        .into_iter()
        .filter_map(|r| Some((r.first()?.to_text().parse::<i64>().ok()?, r.get(1)?.to_text())))
        .collect()
}

/// The databases pg_database has: every world, and Postgres's three where no world has their names.
fn databases(cat: &Catalog) -> Vec<(i64, String)> {
    let mut out: Vec<(i64, String)> = [("template1", 1), ("template0", 4), ("postgres", 5)]
        .into_iter()
        .filter(|(n, _)| !cat.worlds.iter().any(|w| w == n))
        .map(|(n, o)| (o, n.to_string()))
        .collect();
    out.extend(cat.worlds.iter().enumerate().map(|(i, w)| (16384 + i as i64, w.clone())));
    out
}

/// The extensions pg_extension has: plpgsql, and what CREATE EXTENSION recorded.
fn extensions(cat: &Catalog) -> Vec<(i64, String)> {
    let mut out = vec![(13608, "plpgsql".to_string())];
    out.extend(cat.extensions.iter().map(|(n, ..)| (super::pgcrypto::EXTENSION_OID, n.clone())));
    out
}

/// A publication's oid and its place in `cat.pubs`.
fn publication(cat: &Catalog, name: &str) -> Option<(i64, usize)> {
    super::publication::publication_oids(cat).into_iter().find(|(_, k)| cat.pubs[*k].name == name)
}

/// An index access method by name (get_index_am_oid): its oid and name.
fn index_am(name: &str) -> Result<&'static str> {
    match catalog::AMS.iter().find(|a| a.1 == name) {
        None => missing("42704", format!("access method \"{name}\" does not exist")),
        Some(a) if a.3 != "i" => err("42809", format!("access method \"{name}\" is not of type INDEX")),
        Some(a) => Ok(a.1),
    }
}

/// An operator family of access method `am` by its (maybe qualified) name: its oid and name; 42704 for none.
fn opfamily(cat: &Catalog, am: &str, names: &[String]) -> Result<(i64, String)> {
    let (schema, name) = qualified(names)?;
    if let Some(s) = schema {
        need_schema(cat, s)?;
    }
    let found = op_data().families.iter().find(|(_, m, n)| m == am && n == name).filter(|_| in_catalog(schema));
    match found {
        Some((o, _, n)) => Ok((*o, n.clone())),
        None => {
            missing("42704", format!("operator family \"{}\" does not exist for access method \"{am}\"", joined(names)))
        }
    }
}

/// An operator class of access method `am` by its (maybe qualified) name: its oid; 42704 for none, 0A000 for one
/// initdb numbers (Chronos has no oid for it).
fn opclass(cat: &Catalog, am: &str, names: &[String]) -> Result<i64> {
    let (schema, name) = qualified(names)?;
    if let Some(s) = schema {
        need_schema(cat, s)?;
    }
    let found = op_data().classes.iter().find(|(_, m, n, _)| m == am && n == name).filter(|_| in_catalog(schema));
    match found {
        Some((Some(o), ..)) => Ok(*o),
        Some((None, ..)) => no_oid(&format!("operator class \"{name}\" for access method \"{am}\"")),
        None => {
            missing("42704", format!("operator class \"{}\" does not exist for access method \"{am}\"", joined(names)))
        }
    }
}

/// A function, procedure or aggregate by name and argument types (LookupFuncWithArgs), with its errors.
fn routine(cat: &Catalog, ot: Ot, names: &[String], args: &[String]) -> Result<i64> {
    let types = args.iter().map(|t| type_oid(cat, t)).collect::<Result<Vec<i64>>>()?;
    let (schema, name) = qualified(names)?;
    if let Some(s) = schema {
        need_schema(cat, s)?;
    }
    let schemas = match schema {
        Some(s) => vec![s.to_string()],
        None => func_path(),
    };
    let mut found: Option<(i64, char)> = None;
    for s in &schemas {
        if s == "pg_catalog" {
            found = catalog_schema::builtin_procs(name).into_iter().find_map(|o| {
                let (_, kind, at) = catalog_schema::builtin_proc_facts(o)?;
                (at == types).then_some((o, kind))
            });
        } else {
            let stored = qualify(s, name);
            found = cat
                .funcs
                .iter()
                .enumerate()
                .find(|(_, f)| {
                    f.name == stored && f.args.iter().map(|(_, t)| cat.type_oid(*t)).eq(types.iter().copied())
                })
                .map(|(k, f)| (cat.func_oid(k), if f.procedure { 'p' } else { 'f' }));
        }
        if found.is_some() {
            break;
        }
    }
    let sig =
        format!("{}({})", joined(names), types.iter().map(|o| cat.format_type(*o, -1)).collect::<Vec<_>>().join(", "));
    match (ot, found) {
        (Ot::Function, Some((_, 'p'))) => err("42809", format!("{sig} is not a function")),
        (Ot::Procedure, Some((_, k))) if k != 'p' => err("42809", format!("{sig} is not a procedure")),
        (Ot::Aggregate, Some((_, k))) if k != 'a' => err("42809", format!("function {sig} is not an aggregate")),
        (_, Some((o, _))) => Ok(o),
        (_, None) => {
            // an aggregate made with CREATE AGGREGATE has no pg_proc row here
            let (wcore, wname) = catalog::world_owner()?;
            let v = wcore.view_tx(&wname)?;
            for s in &schemas {
                if v.prefix(&format!("_sonos_defs/agg/{}(", qualify(s, name)))?.next().is_some() {
                    return no_oid(&format!("aggregate {}", joined(names)));
                }
            }
            match ot {
                Ot::Procedure => missing("42883", format!("procedure {sig} does not exist")),
                Ot::Aggregate if types.is_empty() => {
                    missing("42883", format!("aggregate {}(*) does not exist", joined(names)))
                }
                Ot::Aggregate => missing("42883", format!("aggregate {sig} does not exist")),
                _ => missing("42883", format!("function {sig} does not exist")),
            }
        }
    }
}

/// An operator by name and operand types (LookupOperWithArgs): one of Postgres's, which are pg_catalog's.
fn operator(cat: &Catalog, names: &[String], args: &[String]) -> Result<i64> {
    let (l, r) = (type_oid(cat, &args[0])?, type_oid(cat, &args[1])?);
    let (schema, name) = qualified(names)?;
    if let Some(s) = schema {
        need_schema(cat, s)?;
    }
    let found = catalog::opers().iter().find(|o| o.1 == name && o.2 == l && o.3 == r).filter(|_| in_catalog(schema));
    match found {
        Some(o) => Ok(o.0),
        None => missing(
            "42883",
            format!("operator does not exist: {} {} {}", cat.format_type(l, -1), joined(names), cat.format_type(r, -1)),
        ),
    }
}

/// A rule, trigger, table constraint or policy: the relation the names before the last give, and its object the last
/// one names.
fn relobject(cat: &Catalog, ot: Ot, names: &[String]) -> Result<(i64, i64, i64)> {
    if names.len() < 2 {
        return err(SYNTAX, "must specify relation and object name");
    }
    let (relname, dep) = (&names[..names.len() - 1], names[names.len() - 1].as_str());
    let r = table(cat, relname)?;
    let f = facts(cat, r);
    let made = match r {
        Rel::Made(i) => Some(i),
        _ => None,
    };
    let rel = f.name.as_str();
    Ok(match ot {
        Ot::Rule => {
            let returns = made.filter(|_| dep == "_RETURN").and_then(|i| cat.rewrites.iter().position(|x| *x == i));
            if let Some(n) = returns {
                (PG_REWRITE, cat.rewrite_oid(n), 0)
            } else if made.is_some_and(|i| cat.rules.iter().any(|(t, n, _)| *t == cat.rels[i].name && n == dep)) {
                return no_oid(&format!("rule \"{dep}\" for relation \"{rel}\""));
            } else {
                return missing("42704", format!("rule \"{dep}\" for relation \"{rel}\" does not exist"));
            }
        }
        Ot::Trigger => {
            let n = made.and_then(|i| {
                let k = cat.rels[i].triggers.iter().position(|t| t.0 == dep)?;
                cat.triggers.iter().position(|x| *x == (i, k))
            });
            match n {
                Some(n) => (PG_TRIGGER, cat.trigger_oid(n), 0),
                None => return missing("42704", format!("trigger \"{dep}\" for table \"{rel}\" does not exist")),
            }
        }
        Ot::TabConstraint => match made.and_then(|i| cat.cons.iter().position(|(r, c)| *r == i && c.name == dep)) {
            Some(k) => (PG_CONSTRAINT, cat.con_oid(k), 0),
            None => return missing("42704", format!("constraint \"{dep}\" for table \"{rel}\" does not exist")),
        },
        _ => match made.and_then(|i| policies(cat).into_iter().find(|(_, x, p)| *x == i && p == dep)) {
            Some((n, ..)) => (PG_POLICY, cat.policy_oid(n), 0),
            None => return missing("42704", format!("policy \"{dep}\" for table \"{rel}\" does not exist")),
        },
    })
}

/// Every policy, as pg_policy numbers them: its place, its relation and its name.
fn policies(cat: &Catalog) -> Vec<(usize, usize, String)> {
    let mut out = vec![];
    for (i, r) in cat.rels.iter().enumerate() {
        for p in &r.policies {
            out.push((out.len(), i, p.name.clone()));
        }
    }
    out
}

/// A text search parser, dictionary, template or configuration: Postgres's own, which pg_catalog has (one made with
/// CREATE TEXT SEARCH has no oid here).
fn text_search(cat: &Catalog, ot: Ot, names: &[String]) -> Result<(i64, i64, i64)> {
    let (schema, name) = qualified(names)?;
    if let Some(s) = schema {
        need_schema(cat, s)?;
    }
    let (class, all, kind): (i64, &[(i64, &str)], &str) = match ot {
        Ot::TsParser => (PG_TS_PARSER, &TS_PARSERS[..], "parser"),
        Ot::TsDict => (PG_TS_DICT, &catalog::TS_DICTS[..], "dictionary"),
        Ot::TsTemplate => (PG_TS_TEMPLATE, &TS_TEMPLATES[..], "template"),
        _ => (PG_TS_CONFIG, &catalog::TS_CONFIGS[..], "configuration"),
    };
    if let Some((o, _)) = all.iter().find(|(_, n)| *n == name).filter(|_| in_catalog(schema)) {
        return Ok((class, *o, 0));
    }
    if catalog::with_world_view(|v| Ok(v.get(&format!("_sonos_defs/ts{kind}/{name}"))?.is_some()))? {
        return no_oid(&format!("text search {kind} \"{}\"", joined(names)));
    }
    missing("42704", format!("text search {kind} \"{}\" does not exist", joined(names)))
}

/// The pg_default_acl row of a role's (and maybe a schema's) default privileges on one kind of object.
fn default_acl(cat: &Catalog, names: &[String], kind: &str) -> Result<i64> {
    let user = names[0].as_str();
    let schema = names.get(1).map(String::as_str);
    let objtype = kind.chars().next().unwrap_or(' ');
    let what = match objtype {
        'r' => "tables",
        'S' => "sequences",
        'f' => "functions",
        'T' => "types",
        'n' => "schemas",
        c => {
            return Err(SqlError {
                code: "22023",
                message: format!("unrecognized default ACL object type \"{c}\""),
                info: Some(Box::new(Violation {
                    hint: Some("Valid object types are \"r\", \"S\", \"f\", \"T\", \"n\".".into()),
                    ..Violation::default()
                })),
            });
        }
    };
    let role = cat.reg.named(user).map(|r| r.oid);
    let nsp = match schema {
        Some(s) => Some(schema_oid(cat, s)).filter(|o| *o != 0),
        None => Some(0),
    };
    let rows = super::acl::default_acl_rows(&cat.reg, &cat.acls, |s| cat.schema_oid(s));
    let found = match (role, nsp) {
        (Some(r), Some(n)) => rows.iter().position(|(ro, s, t, _)| *ro == r && *s == n && *t == objtype),
        _ => None,
    };
    match (found, schema) {
        (Some(n), _) => Ok(super::catalog_objects::DEFAULT_ACL_OIDS + n as i64),
        (None, Some(s)) => {
            missing("42704", format!("default ACL for user \"{user}\" in schema \"{s}\" on {what} does not exist"))
        }
        (None, None) => missing("42704", format!("default ACL for user \"{user}\" on {what} does not exist")),
    }
}

// ---------------------------------------------------------------- identities and descriptions

/// An object an address names, as the identity functions show it.
struct Found {
    /// pg_identify_object's schema and name: each quoted, where the object's catalog has them (a name only where it
    /// identifies the object within its schema)
    schema: Option<String>,
    name: Option<String>,
    /// getObjectIdentityParts: its identity, and the names and arguments pg_get_object_address takes back
    identity: String,
    names: Vec<String>,
    args: Vec<String>,
    /// getObjectDescription
    descr: String,
}

impl Found {
    fn new(identity: String, names: Vec<String>, descr: String) -> Found {
        Found { schema: None, name: None, identity, names, args: vec![], descr }
    }

    fn at(mut self, schema: &str) -> Found {
        self.schema = Some(quoted(schema));
        self
    }

    fn named(mut self, name: &str) -> Found {
        self.name = Some(quoted(name));
        self
    }

    fn with_args(mut self, args: Vec<String>) -> Found {
        self.args = args;
        self
    }
}

/// The type an address's object is of (getObjectTypeDescription), and the object, if it is there.
fn identify(cat: &Catalog, class: i64, oid: i64, sub: i64) -> Result<(String, Option<Found>)> {
    let Some((_, fallback)) = CLASSES.iter().find(|(c, _)| *c == class) else {
        return err("XX000", format!("unrecognized object class: {class}"));
    };
    let ty = fallback.to_string();
    let none = |ty: String| Ok((ty, None));
    let some = |ty: &str, f: Found| Ok((ty.to_string(), Some(f)));
    match class {
        PG_CLASS => {
            let Some(r) = rel_of(cat, oid) else { return none(ty) };
            let f = facts(cat, r);
            let word = kind_word(f.kind);
            let ty = if sub != 0 { format!("{word} column") } else { word.to_string() };
            if sub == 0 {
                let found = Found::new(rel_identity(&f), vec![f.schema.clone(), f.name.clone()], rel_descr(cat, &f));
                return some(&ty, found.at(&f.schema).named(&f.name));
            }
            let Some(att) = usize::try_from(sub - 1).ok().and_then(|k| f.cols.get(k)) else { return none(ty) };
            let found = Found::new(
                format!("{}.{}", rel_identity(&f), quoted(att)),
                vec![f.schema.clone(), f.name.clone(), att.clone()],
                format!("column {att} of {}", rel_descr(cat, &f)),
            );
            some(&ty, found.at(&f.schema).named(&f.name))
        }
        PG_PROC => {
            let routine = match (catalog_schema::builtin_proc_facts(oid), cat.find(oid)) {
                (Some((name, kind, args)), _) => Some(("pg_catalog".to_string(), name, kind, args)),
                (None, Some(Obj::Func(k))) => {
                    let f = &cat.funcs[k];
                    let args = f.args.iter().map(|(_, t)| cat.type_oid(*t)).collect();
                    let kind = if f.procedure { 'p' } else { 'f' };
                    Some((schema_part(&f.name).to_string(), rel_part(&f.name).to_string(), kind, args))
                }
                _ => None,
            };
            let Some((schema, name, kind, args)) = routine else { return none(ty) };
            let ty = match kind {
                'a' => "aggregate",
                'p' => "procedure",
                _ => "function",
            };
            let types: Vec<String> = args.iter().map(|o| type_identity(cat, *o)).collect();
            let found = Found::new(
                format!("{}.{}({})", quoted(&schema), quoted(&name), types.join(",")),
                vec![schema.clone(), name],
                format!("function {}", catalog::proc_shown(cat, Reg::Procedure, oid)),
            );
            some(ty, found.at(&schema).with_args(types))
        }
        PG_TYPE => {
            let Some((schema, typname)) = type_parts(cat, oid) else { return none(ty) };
            let identity = type_identity(cat, oid);
            let found = Found::new(identity.clone(), vec![identity], format!("type {}", cat.format_type(oid, -1)));
            some(&ty, found.at(&schema).named(&typname))
        }
        PG_COLLATION => {
            let made = |k: usize| cat.colls[k].0.clone();
            let stored = match collation::builtins().find(|b| b.0 == oid) {
                Some((_, name, ..)) => Some(qualify("pg_catalog", &name)),
                None => cat.coll_at(oid).map(made),
            };
            let Some(stored) = stored else { return none(ty) };
            // (a built-in one's is pg_catalog's, though stored bare)
            let builtin = collation::builtins().any(|b| b.0 == oid);
            let (schema, name) = match builtin {
                true => ("pg_catalog".to_string(), stored.clone()),
                false => (schema_part(&stored).to_string(), rel_part(&stored).to_string()),
            };
            let shown = cat.collation_shown(oid).unwrap_or_else(|| quoted(&name));
            let found = Found::new(
                format!("{}.{}", quoted(&schema), quoted(&name)),
                vec![schema.clone(), name.clone()],
                format!("collation {shown}"),
            );
            some(&ty, found.at(&schema).named(&name))
        }
        PG_CONSTRAINT => {
            let Some(k) = (0..cat.cons.len()).find(|&k| cat.con_oid(k) == oid) else { return none(ty) };
            let (i, c) = &cat.cons[k];
            let f = facts(cat, Rel::Made(*i));
            let found = Found::new(
                format!("{} on {}", quoted(&c.name), rel_identity(&f)),
                vec![f.schema.clone(), f.name.clone(), c.name.clone()],
                format!("constraint {} on {}", c.name, rel_descr(cat, &f)),
            );
            some("table constraint", found.at(&f.schema))
        }
        PG_CONVERSION => {
            let Some((_, name)) = op_data().conversions.iter().find(|(o, _)| *o == oid) else { return none(ty) };
            let found = Found::new(
                format!("pg_catalog.{}", quoted(name)),
                vec!["pg_catalog".into(), name.clone()],
                format!("conversion {}", quoted(name)),
            );
            some(&ty, found.at("pg_catalog").named(name))
        }
        PG_ATTRDEF => {
            let Some(n) = (0..cat.defaults.len()).find(|&n| cat.default_oid(n) == oid) else { return none(ty) };
            let (i, j) = cat.defaults[n];
            let f = facts(cat, Rel::Made(i));
            let att = &cat.rels[i].columns[j].0;
            let found = Found::new(
                format!("for {}.{}", rel_identity(&f), quoted(att)),
                vec![f.schema.clone(), f.name.clone(), att.clone()],
                format!("default value for column {att} of {}", rel_descr(cat, &f)),
            );
            some(&ty, found)
        }
        PG_LANGUAGE => {
            let Some((_, name)) = languages().into_iter().find(|(o, _)| *o == oid) else { return none(ty) };
            let found = Found::new(quoted(&name), vec![name.clone()], format!("language {name}"));
            some(&ty, found.named(&name))
        }
        PG_LARGEOBJECT => {
            if !catalog::with_world_view(|v| super::largeobj::exists(v, oid))? {
                return none(ty);
            }
            some(&ty, Found::new(oid.to_string(), vec![oid.to_string()], format!("large object {oid}")))
        }
        PG_OPERATOR => {
            let Some(o) = catalog::opers().iter().find(|o| o.0 == oid) else { return none(ty) };
            let side = |t: i64| if t == 0 { "NONE".to_string() } else { type_identity(cat, t) };
            let args: Vec<String> = [o.2, o.3].into_iter().filter(|t| *t != 0).map(|t| type_identity(cat, t)).collect();
            let found = Found::new(
                format!("pg_catalog.{}({},{})", o.1, side(o.2), side(o.3)),
                vec!["pg_catalog".into(), o.1.to_string()],
                format!("operator {}({},{})", o.1, o.4, o.5),
            );
            some(&ty, found.at("pg_catalog").with_args(args))
        }
        PG_OPCLASS => {
            let found = op_data().classes.iter().find(|(o, ..)| *o == Some(oid));
            let Some((_, am, name, _)) = found else { return none(ty) };
            let found = Found::new(
                format!("pg_catalog.{} USING {}", quoted(name), quoted(am)),
                vec![am.clone(), "pg_catalog".into(), name.clone()],
                format!("operator class {} for access method {am}", quoted(name)),
            );
            some(&ty, found.at("pg_catalog").named(name))
        }
        PG_OPFAMILY => {
            let Some((_, am, name)) = op_data().families.iter().find(|(o, ..)| *o == oid) else { return none(ty) };
            let found = Found::new(
                format!("pg_catalog.{} USING {am}", quoted(name)),
                vec![am.clone(), "pg_catalog".into(), name.clone()],
                format!("operator family {} for access method {am}", quoted(name)),
            );
            some(&ty, found.at("pg_catalog").named(name))
        }
        PG_AM => {
            let all = cat.access_methods();
            let Some((_, name, ..)) = all.iter().find(|a| a.0 == oid) else { return none(ty) };
            let found = Found::new(quoted(name), vec![name.clone()], format!("access method {name}"));
            some(&ty, found.named(name))
        }
        PG_REWRITE => {
            let Some(n) = (0..cat.rewrites.len()).find(|&n| cat.rewrite_oid(n) == oid) else { return none(ty) };
            let f = facts(cat, Rel::Made(cat.rewrites[n]));
            let found = Found::new(
                format!("{} on {}", quoted("_RETURN"), rel_identity(&f)),
                vec![f.schema.clone(), f.name.clone(), "_RETURN".into()],
                format!("rule _RETURN on {}", rel_descr(cat, &f)),
            );
            some(&ty, found)
        }
        PG_TRIGGER => {
            let Some(n) = (0..cat.triggers.len()).find(|&n| cat.trigger_oid(n) == oid) else { return none(ty) };
            let (i, k) = cat.triggers[n];
            let f = facts(cat, Rel::Made(i));
            let name = &cat.rels[i].triggers[k].0;
            let found = Found::new(
                format!("{} on {}", quoted(name), rel_identity(&f)),
                vec![f.schema.clone(), f.name.clone(), name.clone()],
                format!("trigger {name} on {}", rel_descr(cat, &f)),
            );
            some(&ty, found)
        }
        PG_NAMESPACE => {
            let Some(name) = schema_named(cat, oid) else { return none(ty) };
            some(&ty, Found::new(quoted(&name), vec![name.clone()], format!("schema {name}")).named(&name))
        }
        PG_STATISTIC_EXT => {
            let all = super::statistics::object_oids(cat);
            let Some((stored, ..)) = all.iter().find(|(_, _, o)| *o == oid) else { return none(ty) };
            let (schema, name) = (schema_part(stored), rel_part(stored));
            let identity = format!("{}.{}", quoted(schema), quoted(name));
            let shown = match super::statistics::visible(cat, oid) {
                Some(true) => quoted(name),
                _ => identity.clone(),
            };
            let found =
                Found::new(identity, vec![schema.to_string(), name.to_string()], format!("statistics object {shown}"));
            some(&ty, found.at(schema).named(name))
        }
        PG_TS_PARSER | PG_TS_DICT | PG_TS_TEMPLATE | PG_TS_CONFIG => {
            let all: &[(i64, &str)] = match class {
                PG_TS_PARSER => &TS_PARSERS[..],
                PG_TS_DICT => &catalog::TS_DICTS[..],
                PG_TS_TEMPLATE => &TS_TEMPLATES[..],
                _ => &catalog::TS_CONFIGS[..],
            };
            let Some((_, name)) = all.iter().find(|(o, _)| *o == oid) else { return none(ty) };
            let found = Found::new(
                format!("pg_catalog.{}", quoted(name)),
                vec!["pg_catalog".into(), name.to_string()],
                format!("{ty} {}", quoted(name)),
            );
            some(&ty, found.at("pg_catalog").named(name))
        }
        PG_AUTHID => {
            let Some(r) = cat.reg.by_oid(oid) else { return none(ty) };
            some(&ty, Found::new(quoted(&r.name), vec![r.name.clone()], format!("role {}", r.name)).named(&r.name))
        }
        PG_AUTH_MEMBERS => {
            let rows = super::roles::member_rows(&cat.reg);
            let row = rows.iter().find(|r| r.first() == Some(&Value::Int(oid)));
            let name = |v: Option<&Value>| match v {
                Some(Value::Int(o)) => cat.reg.by_oid(*o).map_or_else(|| o.to_string(), |r| r.name),
                _ => String::new(),
            };
            let Some(row) = row else { return none(ty) };
            let text = format!("membership of role {} in role {}", name(row.get(2)), name(row.get(1)));
            some(&ty, Found::new(text.clone(), vec![], text))
        }
        PG_DATABASE => {
            let Some((_, name)) = databases(cat).into_iter().find(|(o, _)| *o == oid) else { return none(ty) };
            some(&ty, Found::new(quoted(&name), vec![name.clone()], format!("database {name}")).named(&name))
        }
        PG_TABLESPACE => {
            let Some(s) = cat.spaces.iter().find(|s| s.oid == oid) else { return none(ty) };
            let name = &s.name;
            some(&ty, Found::new(quoted(name), vec![name.to_string()], format!("tablespace {name}")).named(name))
        }
        PG_FDW => {
            let fdw = cat.foreign.fdws.iter().find(|f| cat.foreign.fdw_oid(&f.name) == Some(oid));
            let Some(f) = fdw else { return none(ty) };
            let found = Found::new(quoted(&f.name), vec![f.name.clone()], format!("foreign-data wrapper {}", f.name));
            some(&ty, found.named(&f.name))
        }
        PG_SERVER => {
            let server = cat.foreign.servers.iter().find(|s| cat.foreign.server_oid(&s.name) == Some(oid));
            let Some(s) = server else { return none(ty) };
            some(&ty, Found::new(quoted(&s.name), vec![s.name.clone()], format!("server {}", s.name)).named(&s.name))
        }
        PG_USER_MAPPING => {
            let at = (0..cat.foreign.mappings.len()).find(|&i| cat.foreign.mapping_oid(i) == oid);
            let Some(i) = at else { return none(ty) };
            let m = &cat.foreign.mappings[i];
            let user = match m.user {
                super::roles::PUBLIC => "public".to_string(),
                u => cat.reg.by_oid(u).map_or_else(|| u.to_string(), |r| r.name),
            };
            let found = Found::new(
                format!("{} on server {}", quoted(&user), m.server),
                vec![user.clone()],
                format!("user mapping for {user} on server {}", m.server),
            );
            some(&ty, found.with_args(vec![m.server.clone()]))
        }
        PG_DEFAULT_ACL => {
            let rows = super::acl::default_acl_rows(&cat.reg, &cat.acls, |s| cat.schema_oid(s));
            let n = oid - super::catalog_objects::DEFAULT_ACL_OIDS;
            let Some((role, nsp, objtype, _)) = usize::try_from(n).ok().and_then(|n| rows.get(n)) else {
                return none(ty);
            };
            let user = cat.reg.by_oid(*role).map_or_else(|| role.to_string(), |r| r.name);
            let schema = if *nsp == 0 { None } else { schema_named(cat, *nsp) };
            let (on, new) = match objtype {
                'r' => ("tables", "relations"),
                'S' => ("sequences", "sequences"),
                'f' => ("functions", "functions"),
                'T' => ("types", "types"),
                _ => ("schemas", "schemas"),
            };
            let in_schema = |q: bool| match &schema {
                Some(s) if q => format!(" in schema {}", quoted(s)),
                Some(s) => format!(" in schema {s}"),
                None => String::new(),
            };
            let mut names = vec![user.clone()];
            names.extend(schema.clone());
            let found = Found::new(
                format!("for role {}{} on {on}", quoted(&user), in_schema(true)),
                names,
                format!("default privileges on new {new} belonging to role {user}{}", in_schema(false)),
            );
            some(&ty, found.with_args(vec![objtype.to_string()]))
        }
        PG_EXTENSION => {
            let Some((_, name)) = extensions(cat).into_iter().find(|(o, _)| *o == oid) else { return none(ty) };
            some(&ty, Found::new(quoted(&name), vec![name.clone()], format!("extension {name}")).named(&name))
        }
        PG_POLICY => {
            let all = policies(cat);
            let Some((_, i, name)) = all.iter().find(|(n, ..)| cat.policy_oid(*n) == oid) else { return none(ty) };
            let f = facts(cat, Rel::Made(*i));
            let found = Found::new(
                format!("{} on {}", quoted(name), rel_identity(&f)),
                vec![f.schema.clone(), f.name.clone(), name.clone()],
                format!("policy {name} on {}", rel_descr(cat, &f)),
            );
            some(&ty, found)
        }
        PG_PUBLICATION => {
            let Some((_, k)) = super::publication::publication_oids(cat).into_iter().find(|(o, _)| *o == oid) else {
                return none(ty);
            };
            let name = &cat.pubs[k].name;
            some(&ty, Found::new(quoted(name), vec![name.clone()], format!("publication {name}")).named(name))
        }
        PG_PUBLICATION_NAMESPACE => {
            let all = super::publication::namespace_oids(cat);
            let Some((_, k, schema)) = all.iter().find(|(o, ..)| *o == oid) else { return none(ty) };
            let pubname = &cat.pubs[*k].name;
            let found = Found::new(
                format!("{schema} in publication {pubname}"),
                vec![schema.clone()],
                format!("publication of schema {schema} in publication {pubname}"),
            );
            some(&ty, found.with_args(vec![pubname.clone()]))
        }
        PG_PUBLICATION_REL => {
            let all = super::publication::rel_oids(cat);
            let Some((_, k, i)) = all.iter().find(|(o, ..)| *o == oid) else { return none(ty) };
            let pubname = &cat.pubs[*k].name;
            let f = facts(cat, Rel::Made(*i));
            let found = Found::new(
                format!("{} in publication {pubname}", rel_identity(&f)),
                vec![f.schema.clone(), f.name.clone()],
                format!("publication of {} in publication {pubname}", rel_descr(cat, &f)),
            );
            some(&ty, found.with_args(vec![pubname.clone()]))
        }
        // pg_cast, pg_amop, pg_amproc, pg_event_trigger, pg_parameter_acl, pg_subscription and pg_transform: Chronos's
        // catalog has no rows of them, so no oid names one
        _ => none(ty),
    }
}

/// A type's schema and `typname`: a built-in one's (pg_catalog's), or one made here (an enum, a domain, a composite
/// type, a relation's row type, or the array type of one).
fn type_parts(cat: &Catalog, oid: i64) -> Option<(String, String)> {
    if oid == 0 {
        return None;
    }
    if let Some(k) = cat.enum_at(oid, false) {
        let n = &cat.enums[k].0;
        return Some((schema_part(n).to_string(), rel_part(n).to_string()));
    }
    if let Some(i) = cat.row_type_of(oid, false) {
        let n = &cat.rels[i].name;
        return Some((schema_part(n).to_string(), rel_part(n).to_string()));
    }
    let made_array = cat.enum_at(oid, true).map(|k| (cat.enum_oid(k), &cat.enums[k].0));
    let row_array = cat.row_type_of(oid, true).map(|i| (cat.row_type_oid(i), &cat.rels[i].name));
    if let Some((elem, n)) = made_array.or(row_array) {
        let typname = cat.array_type_names().get(&elem).cloned().unwrap_or_else(|| format!("_{}", rel_part(n)));
        return Some((schema_part(n).to_string(), typname));
    }
    if let Some(n) = catalog::builtin_typname(oid) {
        return Some(("pg_catalog".into(), n.to_string()));
    }
    if let Some((elem, _)) = ARRAYS.iter().find(|(e, a)| i64::from(*a) == oid && e != a) {
        return catalog::builtin_typname(i64::from(*elem)).map(|n| ("pg_catalog".into(), format!("_{n}")));
    }
    // (another type Chronos knows by oid, a range type: as format_type names it)
    match cat.format_type(oid, -1) {
        s if s.contains("???") => None,
        s => Some(("pg_catalog".into(), s)),
    }
}

/// format_type_be_qualified: a type's name with its schema, but the types SQL has names of its own for (`integer`)
/// and arrays of a type as that type's with `[]`.
fn type_identity(cat: &Catalog, oid: i64) -> String {
    const SPECIAL: [(i64, &str); 16] = [
        (1560, "bit"),
        (16, "boolean"),
        (1042, "character"),
        (700, "real"),
        (701, "double precision"),
        (21, "smallint"),
        (23, "integer"),
        (20, "bigint"),
        (1700, "numeric"),
        (1186, "interval"),
        (1083, "time without time zone"),
        (1266, "time with time zone"),
        (1114, "timestamp without time zone"),
        (1184, "timestamp with time zone"),
        (1562, "bit varying"),
        (1043, "character varying"),
    ];
    if let Some((_, n)) = SPECIAL.iter().find(|(o, _)| *o == oid) {
        return n.to_string();
    }
    // an array type is its element type's, with []
    let elem = ARRAYS
        .iter()
        .find(|(e, a)| i64::from(*a) == oid && e != a)
        .map(|(e, _)| i64::from(*e))
        .or_else(|| cat.enum_at(oid, true).map(|k| cat.enum_oid(k)))
        .or_else(|| cat.row_type_of(oid, true).map(|i| cat.row_type_oid(i)));
    if let Some(e) = elem {
        return format!("{}[]", type_identity(cat, e));
    }
    match type_parts(cat, oid) {
        Some((schema, name)) => format!("{}.{}", quoted(&schema), quoted(&name)),
        None => "???".into(),
    }
}
