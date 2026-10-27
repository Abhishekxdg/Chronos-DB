//! Index access methods: what each one can do, as Postgres 16's handlers report it, Postgres's own operator classes,
//! and the index property functions (pg_indexam_has_property, pg_index_has_property, pg_index_column_has_property).
//!
//! Chronos keeps every index as a B-tree, or as one of its search indexes (see `btree`), whatever its access method.
//! The method CREATE INDEX names is kept with the index (`btree::Index::am`), with each column's ordering and
//! operator class, so what the catalog shows of it and what these functions report are the method's own, and
//! CREATE INDEX refuses what the method can't do (a unique hash index, DESC in a gin index) as Postgres does.

use super::*;
use std::sync::LazyLock;

/// An index access method's capabilities: its IndexAmRoutine's flags, as its handler sets them (nbtree.c
/// bthandler, hash.c hashhandler, gist.c gisthandler, spgutils.c spghandler, ginutil.c ginhandler, brin.c
/// brinhandler; pgvector's hnswhandler).
#[derive(Clone, Copy, Debug)]
pub(super) struct Am {
    pub name: &'static str,
    pub can_order: bool,
    pub can_order_by_op: bool,
    pub can_backward: bool,
    pub can_unique: bool,
    pub can_multicol: bool,
    pub search_array: bool,
    pub search_nulls: bool,
    pub clusterable: bool,
    pub can_include: bool,
    /// amgettuple: it has plain index scans (and so takes exclusion constraints)
    pub gettuple: bool,
    /// amgetbitmap: it has bitmap scans
    pub getbitmap: bool,
    /// amcanreturn: it can give a column's value back (index-only scans)
    pub can_return: bool,
}

const fn am(
    name: &'static str,
    [can_order, can_order_by_op, can_backward, can_unique, can_multicol]: [bool; 5],
    [search_array, search_nulls, clusterable, can_include]: [bool; 4],
    [gettuple, getbitmap, can_return]: [bool; 3],
) -> Am {
    Am {
        name,
        can_order,
        can_order_by_op,
        can_backward,
        can_unique,
        can_multicol,
        search_array,
        search_nulls,
        clusterable,
        can_include,
        gettuple,
        getbitmap,
        can_return,
    }
}

/// The index access methods (pg_am's of kind `i`): [amcanorder, amcanorderbyop, amcanbackward, amcanunique,
/// amcanmulticol], [amsearcharray, amsearchnulls, amclusterable, amcaninclude], [amgettuple, amgetbitmap,
/// amcanreturn], each set or not as its handler sets it.
const AMS: [Am; 8] = [
    am("btree", [true, false, true, true, true], [true, true, true, true], [true, true, true]),
    am("hash", [false, false, true, false, false], [false, false, false, false], [true, true, false]),
    am("gist", [false, true, false, false, true], [false, true, true, true], [true, true, true]),
    am("spgist", [false, true, false, false, false], [false, true, false, true], [true, true, true]),
    am("gin", [false, false, false, false, true], [false, false, false, false], [false, true, false]),
    am("brin", [false, false, false, false, true], [false, true, false, false], [false, true, false]),
    am("hnsw", [false, true, false, false, false], [false, false, false, false], [true, false, false]),
    am("bloom", [false, false, false, false, true], [false, false, false, false], [false, true, false]),
];

/// Index access method `name`'s capabilities: None for one that isn't an index access method (heap).
pub(super) fn method(name: &str) -> Option<&'static Am> {
    AMS.iter().find(|a| a.name == name)
}

/// 0A000 for what an index of access method `am` can't be: unique, with INCLUDE columns, of several key columns
/// (DefineIndex's checks, in its order: indexcmds.c:857-871).
pub(super) fn check_create(am: &str, unique: bool, include: bool, keys: usize) -> Result<()> {
    let Some(a) = method(am) else { return Ok(()) };
    let refuse = |what: &str| err(UNSUPPORTED, format!("access method \"{am}\" does not support {what}"));
    if unique && !a.can_unique {
        return refuse("unique indexes");
    }
    if include && !a.can_include {
        return refuse("included columns");
    }
    if keys > 1 && !a.can_multicol {
        return refuse("multicolumn indexes");
    }
    Ok(())
}

/// A key column's indoption for ASC/DESC (`desc`: Some(true) for DESC, Some(false) for ASC) and NULLS FIRST/LAST
/// (`nulls_first`), as ComputeIndexAttrs works it out (indexcmds.c:2131-2167): DESC puts nulls first unless it says
/// otherwise, and an access method that doesn't order refuses either (0A000).
pub(super) fn indoption(am: &str, desc: Option<bool>, nulls_first: Option<bool>) -> Result<i16> {
    if !method(am).is_none_or(|a| a.can_order) {
        if desc.is_some() {
            return err(UNSUPPORTED, format!("access method \"{am}\" does not support ASC/DESC options"));
        }
        if nulls_first.is_some() {
            return err(UNSUPPORTED, format!("access method \"{am}\" does not support NULLS FIRST/LAST options"));
        }
        return Ok(0);
    }
    let desc = desc == Some(true);
    let first = nulls_first.unwrap_or(desc);
    Ok(if desc { btree::INDOPTION_DESC } else { 0 } | if first { btree::INDOPTION_NULLS_FIRST } else { 0 })
}

/// One of Postgres's own operator classes, as its pg_opclass row has it.
#[derive(Debug)]
pub(super) struct OpClass {
    pub oid: i64,
    /// its access method's name
    pub am: &'static str,
    pub name: String,
    /// its operator family (opcfamily): what CompareIndexInfo compares of two indexes' columns
    pub family: i64,
    /// the type it indexes (opcintype): a polymorphic one (anyarray, anyenum, anyrange) for a family of types
    pub intype: i64,
    /// whether it is the class an index of its type and method gets when none is named (opcdefault)
    pub default: bool,
    /// the type its index's column holds where that isn't the indexed type (opckeytype), 0 where it is
    pub keytype: i64,
}

/// Postgres's own operator classes (pg_opclass's rows in `catalog_columns.txt`).
fn classes() -> &'static [OpClass] {
    static ALL: LazyLock<Vec<OpClass>> = LazyLock::new(|| {
        let cols = catalog_schema::columns("pg_opclass");
        let at = |n: &str| cols.iter().position(|(c, _)| c == n);
        let int = |r: &[Value], n: &str| match at(n).and_then(|i| r.get(i)) {
            Some(Value::Int(x) | Value::Reg(x, _)) => *x,
            _ => 0,
        };
        catalog_schema::builtin_rows("pg_opclass")
            .iter()
            .filter_map(|r| {
                let am = catalog::AMS.iter().find(|(o, ..)| *o == int(r, "opcmethod"))?.1;
                Some(OpClass {
                    oid: int(r, "oid"),
                    am,
                    name: r.get(at("opcname")?)?.to_text(),
                    family: int(r, "opcfamily"),
                    intype: int(r, "opcintype"),
                    default: r.get(at("opcdefault")?) == Some(&Value::Bool(true)),
                    keytype: int(r, "opckeytype"),
                })
            })
            .collect()
    });
    &ALL
}

/// Postgres's operator class `name` of access method `am`, if it has one.
pub(super) fn class_named(am: &str, name: &str) -> Option<&'static OpClass> {
    classes().iter().find(|c| c.am == am && c.name == name)
}

/// The extensions' operator classes Chronos has (pg_trgm's, hstore's, ltree's, bloom's): access method, name, the type
/// it indexes, and whether it is that type's default for the method. They have no row in pg_opclass here.
const EXTENSION_CLASSES: [(&str, &str, u32, bool); 12] = [
    ("gin", "gin_trgm_ops", 25, false),
    ("gist", "gist_trgm_ops", 25, false),
    ("gin", "gin_hstore_ops", hstore::OID, true),
    ("gist", "gist_hstore_ops", hstore::OID, true),
    ("btree", "btree_hstore_ops", hstore::OID, true),
    ("hash", "hash_hstore_ops", hstore::OID, true),
    ("gist", "gist_ltree_ops", ltree::OID, true),
    ("gist", "gist__ltree_ops", ltree::ARRAY_OID, true),
    ("btree", "ltree_ops", ltree::OID, true),
    ("hash", "hash_ltree_ops", ltree::OID, true),
    ("bloom", "int4_ops", 23, true),
    ("bloom", "text_ops", 25, true),
];

/// Whether operator class `name` is one access method `am` has: Postgres's own, or an extension's Chronos has.
pub(super) fn has_class(am: &str, name: &str) -> bool {
    class_named(am, name).is_some() || EXTENSION_CLASSES.iter().any(|(m, n, ..)| *m == am && *n == name)
}

/// The types a value of type `ty` (whose type id is `oid`: a varchar's and a char(n)'s their own) is indexed as,
/// most fitting first: its own, then the one it is binary-coercible to (varchar to text, cidr to inet), then the
/// polymorphic one of its kind, as GetDefaultOpClass (indexcmds.c:2286) tries them.
pub(super) fn indexed_as(ty: Type, oid: i64) -> Vec<i64> {
    let mut out = vec![oid];
    match (ty, oid) {
        (_, 1043) => out.push(25),
        (_, 650) => out.push(869),
        (Type::Array(_), _) => out.push(2277),
        (Type::Enum(_), _) => out = vec![3500],
        (Type::Range(_), _) => out.push(3831),
        (Type::Multirange(_), _) => out.push(4537),
        (Type::Record(_), _) => out.push(2249),
        _ => {}
    }
    out
}

/// Whether operator class `name` of access method `am` takes a column of type `ty` (type id `oid`), as
/// ResolveOpClass asks of the index made again when the column's type changes (indexcmds.c:2260, IsBinaryCoercible):
/// None for a class CREATE OPERATOR CLASS made, whose type isn't kept here.
pub(super) fn accepts(am: &str, name: &str, ty: Type, oid: i64) -> Option<bool> {
    let intype = class_named(am, name).map(|c| c.intype).or_else(|| {
        EXTENSION_CLASSES.iter().find(|(m, n, ..)| *m == am && *n == name).map(|(_, _, t, _)| i64::from(*t))
    })?;
    Some(indexed_as(ty, oid).contains(&intype))
}

/// Each key part of index `x` of table `t`: its type, and its type's id as pg_attribute has it (a varchar's and a
/// char(n)'s their own).
pub(super) fn part_types(t: &Table, x: &btree::Index) -> Vec<(Type, i64)> {
    let mut exprs = x.exprs.iter();
    x.cols
        .iter()
        .map(|&c| match c {
            btree::EXPR => {
                let ty = exprs
                    .next()
                    .and_then(|e| index_expr(t, e).ok())
                    .and_then(|e| ty_in(&e, &t.cols, &[], &[]).ok())
                    .unwrap_or(Type::Text);
                (ty, i64::from(ty.oid()))
            }
            c => t.cols.iter().find(|x| x.id == c).map_or((Type::Text, 25), |c| (c.ty, col_oid(c))),
        })
        .collect()
}

/// What CompareIndexInfo (index.c:2590) compares of each key part of index `x` of table `t`: its operator class's
/// family, as Postgres's own pg_opclass has it, for the class it names or else its type's default for its access
/// method; an extension's class (no row here) or one CREATE OPERATOR CLASS made by its name; None where none fits.
pub(super) fn families(t: &Table, x: &btree::Index) -> Vec<Option<String>> {
    let am = x.method();
    part_types(t, x)
        .into_iter()
        .enumerate()
        .map(|(k, (ty, oid))| {
            let (_, name, _) = class_of(am, x.said_class(k), ty, oid)?;
            Some(class_named(am, &name).map_or(name, |c| c.family.to_string()))
        })
        .collect()
}

/// An index column's operator class: the one named (`named`), else the default one of its access method for its
/// type, as Postgres's own pg_opclass has it: (its oid, 0 for an extension's, which has no row here; its name; its
/// key type, 0 where the column holds the indexed type itself). None: none fits (a type of no built-in class).
pub(super) fn class_of(am: &str, named: Option<&str>, ty: Type, oid: i64) -> Option<(i64, String, i64)> {
    let keyed = |c: &OpClass| {
        // (ANYELEMENT over ANYARRAY: the array's element, as ConstructTupleDescriptor (index.c:456) reads it)
        let key = match (c.keytype, c.intype, ty) {
            (2283, 2277, Type::Array(e)) => i64::from(e),
            (k, ..) => k,
        };
        (c.oid, c.name.clone(), key)
    };
    if let Some(n) = named {
        return Some(class_named(am, n).map_or_else(|| (0, n.to_string(), 0), keyed));
    }
    let types = indexed_as(ty, oid);
    for t in &types {
        if let Some(c) = classes().iter().find(|c| c.am == am && c.default && c.intype == *t) {
            return Some(keyed(c));
        }
    }
    EXTENSION_CLASSES
        .iter()
        .find(|(m, _, t, default)| *m == am && *default && types.contains(&i64::from(*t)))
        .map(|(_, n, ..)| (0, n.to_string(), 0))
}

/// Whether `named` is the class an index column of access method `am` and type `ty` gets when it names none: then
/// pg_get_indexdef leaves it out (ruleutils.c get_opclass_name).
pub(super) fn is_default(am: &str, named: &str, ty: Type, oid: i64) -> bool {
    class_of(am, None, ty, oid).is_some_and(|(_, n, _)| n == named)
}

/// What the gist and spgist operator classes report of a column: whether it can order by distance (a distance
/// support function, gist's GIST_DISTANCE_PROC; spgist's ordering operator), and whether it can give its values
/// back (gist: a fetch function, or no compress function; spgist: its config's canReturnData), as gistproperty
/// (gistutil.c:933) and spgproperty (spgutils.c:1288) and spgcanreturn (spgscan.c:1085) work them out from
/// Postgres's catalog.
const CLASS_PROPERTIES: [(&str, &str, bool, bool); 20] = [
    ("gist", "point_ops", true, true),
    ("gist", "box_ops", true, true),
    ("gist", "poly_ops", true, false),
    ("gist", "circle_ops", true, false),
    ("gist", "inet_ops", false, true),
    ("gist", "range_ops", false, true),
    ("gist", "multirange_ops", false, false),
    ("gist", "tsvector_ops", false, false),
    ("gist", "tsquery_ops", false, false),
    ("gist", "gist_trgm_ops", true, false),
    ("gist", "gist_hstore_ops", false, false),
    ("gist", "gist_ltree_ops", false, false),
    ("gist", "gist__ltree_ops", false, false),
    ("spgist", "quad_point_ops", true, true),
    ("spgist", "kd_point_ops", true, true),
    ("spgist", "box_ops", true, true),
    ("spgist", "poly_ops", true, false),
    ("spgist", "range_ops", false, true),
    ("spgist", "text_ops", false, true),
    ("spgist", "inet_ops", false, true),
];

/// What an index is to the property functions: its number of columns and of key columns, and
/// each key column's indoption and operator class.
pub(super) struct Facts<'a> {
    pub natts: usize,
    pub nkey: usize,
    pub opts: &'a [i64],
    pub classes: &'a [String],
}

/// The properties the functions know (amutils.c's am_propnames); another name is NULL, as an access method that
/// defines none of its own answers it.
const PROPERTIES: [&str; 18] = [
    "asc",
    "desc",
    "nulls_first",
    "nulls_last",
    "orderable",
    "distance_orderable",
    "returnable",
    "search_array",
    "search_nulls",
    "clusterable",
    "index_scan",
    "bitmap_scan",
    "backward_scan",
    "can_order",
    "can_unique",
    "can_multi_col",
    "can_exclude",
    "can_include",
];

/// Property `name` (in any case) of access method `am` (`index` None, `col` 0), of an index (`col` 0), or of its
/// column `col` (from 1): indexam_property (amutils.c:148), with the method's own answer first where it has one
/// (btproperty nbtutils.c:2154, gistproperty, spgproperty). None is NULL: a property it doesn't know, a column past
/// the index's, or one the method doesn't say.
pub(super) fn property(am: &Am, index: Option<&Facts>, col: i64, name: &str) -> Option<bool> {
    let lower = name.to_lowercase();
    let prop = PROPERTIES.iter().find(|p| **p == lower).copied().unwrap_or("");
    let natts = index.map_or(0, |x| x.natts) as i64;
    if col < 0 || col > natts {
        return None;
    }
    let k = col as usize; // (a column, from 1, where `col` > 0)
    // the method's own answers: btree can always give a column back; gist and spgist ask the column's class
    if col > 0 {
        let class = || {
            let x = index?;
            (k <= x.nkey).then(|| x.classes.get(k - 1)).flatten().filter(|c| !c.is_empty())
        };
        let of_class = |distance: bool| -> Option<bool> {
            let c = class()?;
            let (_, _, d, r) = CLASS_PROPERTIES.iter().find(|(m, n, ..)| *m == am.name && *n == c.as_str())?;
            Some(if distance { *d } else { *r })
        };
        match (am.name, prop) {
            ("btree", "returnable") => return Some(true),
            ("gist", "distance_orderable") => return of_class(true),
            ("gist", "returnable") => return of_class(false),
            ("spgist", "distance_orderable") => return of_class(true),
            _ => {}
        }
        let x = index?;
        // (a column INCLUDE gave is no key: most of what a key column is, it isn't)
        let key = !(am.can_include && k > x.nkey);
        let opt = x.opts.get(k - 1).copied().unwrap_or(0);
        let bit = |mask: i16, want: i16| -> Option<bool> {
            key.then_some(am.can_order && opt & i64::from(mask) == i64::from(want))
        };
        return match prop {
            "asc" => bit(btree::INDOPTION_DESC, 0),
            "desc" => bit(btree::INDOPTION_DESC, btree::INDOPTION_DESC),
            "nulls_first" => bit(btree::INDOPTION_NULLS_FIRST, btree::INDOPTION_NULLS_FIRST),
            "nulls_last" => bit(btree::INDOPTION_NULLS_FIRST, 0),
            "orderable" => Some(key && am.can_order),
            "distance_orderable" => (!key || !am.can_order_by_op).then_some(false),
            // (spgist gives back what its class's config says it can, and its INCLUDE columns: spgcanreturn)
            "returnable" => Some(
                am.can_return
                    && (k > 1
                        || class().is_some_and(|c| {
                            CLASS_PROPERTIES.iter().any(|(m, n, _, r)| *m == am.name && *n == c.as_str() && *r)
                        })),
            ),
            "search_array" => key.then_some(am.search_array),
            "search_nulls" => key.then_some(am.search_nulls),
            _ => None,
        };
    }
    if index.is_some() {
        return match prop {
            "clusterable" => Some(am.clusterable),
            "index_scan" => Some(am.gettuple),
            "bitmap_scan" => Some(am.getbitmap),
            "backward_scan" => Some(am.can_backward),
            _ => None,
        };
    }
    match prop {
        "can_order" => Some(am.can_order),
        "can_unique" => Some(am.can_unique),
        "can_multi_col" => Some(am.can_multicol),
        "can_exclude" => Some(am.gettuple),
        "can_include" => Some(am.can_include),
        _ => None,
    }
}
