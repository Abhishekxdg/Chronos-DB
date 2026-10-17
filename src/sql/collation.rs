//! Collations: the order text sorts and compares in. Text sorts by its bytes, which is collation "C" (and "POSIX",
//! "ucs_basic", and "default", since the database's own is C). An ICU collation sorts as a language does:
//! `"de-x-icu"`, `"sv-x-icu"`, or one made with `CREATE COLLATION name (provider = icu, locale = 'de-u-co-phonebk')`.
//! ICU here is ICU4X (`icu_collator`, its data compiled in) where Postgres links ICU4C; on the locales of
//! `collations.tsv` the two order the same (tests/pgdiff.rs compares them with Postgres 16 and ICU4C 78).
//!
//! A collation is deterministic unless it was made with `deterministic = false`: two strings are equal only if
//! their bytes are, and where ICU ties two strings their bytes decide, as in Postgres. So equality, hashing,
//! GROUP BY, DISTINCT and unique keys are bytes in a deterministic collation; only order changes. A collated order
//! is a key (`key`): the ICU sort key, a 0 byte (no sort key holds one), then the text's bytes. Keys compare as
//! bytes in the collation's order, so ORDER BY, min and max, and an index of a collation work on keys with the
//! machinery bytes already have.
//!
//! In a nondeterministic collation two strings ICU ties are equal (`und-u-ks-level2`: 'abc' = 'ABC'). Their
//! equality is a key too (`eq_key`): the ICU sort key alone. Comparisons, IN lists, unique constraints and
//! indexes of such a collation work on those keys; what isn't done by key yet (GROUP BY, DISTINCT, set
//! operations, a primary or a foreign key, LIKE) is refused rather than done by bytes.
//!
//! `citext` is text in the collation "citext" (`citext`): nondeterministic, with no ICU locale. Two of its strings
//! are equal when their lower case is, as the extension compares them, and it sorts by that lower case.
//!
//! A collation made with CREATE COLLATION is a row of its world, `_sonos_collations/<name>` (`app.name` outside
//! public), so it forks, merges and shows in DIFF as tables do. A column keeps a copy of its collation's name and
//! locale in its table's schema, as an enum column keeps its labels: the table reads without the collation's row.

use super::{
    Col, Expr, Func, Json, Result, SqlError, Tok, Type, UNSUPPORTED, View, Violation, err, notice, path_schemas,
    qualify, rel_part, schema_part, ty_in, with_path_view,
};
use icu_collator::options::{AlternateHandling, CaseLevel, CollatorOptions, MaxVariable, Strength as Level};
use icu_collator::{CollatorBorrowed, CollatorPreferences};
use icu_locale_core::extensions::unicode::{Key, Value as KwValue};
use icu_locale_core::subtags::Variant;
use icu_locale_core::{LanguageIdentifier, Locale};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, RwLock};

/// Where CREATE COLLATION keeps its collations: `_sonos_collations/<name>`, the definition as JSON (`provider`
/// "icu" or "libc", `locale`, and for libc `encoding`).
pub(super) const COLLATIONS: &str = "_sonos_collations";

pub(super) fn row_key(name: &str) -> String {
    format!("{COLLATIONS}/{name}")
}

/// What collversion and pg_collation_actual_version() say of an ICU collation: ICU4X's version, which isn't
/// ICU4C's (Postgres says `153.136` and the like).
pub(super) const VERSION: &str = "icu4x 2.3.1";

/// The ICU locales Postgres 16 imports at initdb, each collation `<locale>-x-icu` (see `collations.tsv`).
static PREDEFINED: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| include_str!("collations.tsv").lines().filter(|l| !l.starts_with('#') && !l.is_empty()).collect());

/// Where the predefined `-x-icu` collations' oids begin (Postgres gives them oids at initdb, past its libc ones;
/// these are clear of the oids Chronos uses for other objects).
const ICU_OIDS: i64 = 14000;

/// The locales whose rules ICU4C has and ICU4X's data doesn't (or has otherwise): ICU4X would order them differently
/// from Postgres, so their collations are refused. Found by comparing each of Postgres's ICU collations with ICU4X's
/// data (ICU4C gives a tailored collation's version a third part, `153.136.48`), and their orders of 300 strings of
/// many scripts.
const UNCOVERED: [(&str, &str); 15] = [
    ("az-Cyrl", "Azerbaijani in Cyrillic"),
    ("bo", "Tibetan"),
    ("haw", "Hawaiian"),
    ("hi-Latn", "Hindi in Latin script"),
    ("kl", "Kalaallisut"),
    // (Konkani and Marathi: ICU4X has their rules, but sorts the conjunct क्ष after क where ICU4C sorts it after ख)
    ("kok", "Konkani"),
    ("lkt", "Lakota"),
    ("ln", "Lingala"),
    ("mr", "Marathi"),
    ("pa-Arab", "Punjabi in Arabic script"),
    ("se", "Northern Sami"),
    ("smn", "Inari Sami"),
    ("uz-Arab", "Uzbek in Arabic script"),
    ("uz-Cyrl", "Uzbek in Cyrillic"),
    ("yi", "Yiddish"),
];

/// The language (and script) of a locale whose rules ICU4X lacks, if `locale` (canonical) is one.
fn uncovered(locale: &str) -> Option<(&'static str, &'static str)> {
    let lower = locale.to_ascii_lowercase();
    UNCOVERED.iter().copied().find(|(p, _)| {
        let p = p.to_ascii_lowercase();
        lower == p || lower.starts_with(&format!("{p}-"))
    })
}

/// The collations every database has: oid, name, provider, the ICU locale, collencoding. `C.UTF-8` is libc's (a
/// Postgres on Linux or macOS has it from the system), bytes in UTF-8 as `C` is.
const FIXED: [(i64, &str, char, Option<&str>, i64); 6] = [
    (100, "default", 'd', None, -1),
    (950, "C", 'c', None, -1),
    (951, "POSIX", 'c', None, -1),
    (962, "ucs_basic", 'c', None, 6),
    (963, "unicode", 'i', Some("und"), -1),
    (13990, "C.UTF-8", 'c', None, 6),
];

/// What a libc collation of bytes says it is (`collcollate`, `collctype`).
pub(super) fn libc_locale(name: &str) -> &str {
    match name {
        "POSIX" | "C.UTF-8" => name,
        _ => "C",
    }
}

/// A built-in collation by its name: its oid and its ICU locale (None: bytes).
pub(super) fn builtin(name: &str) -> Option<(i64, Option<&'static str>)> {
    static BY_NAME: LazyLock<HashMap<String, (i64, Option<&'static str>)>> = LazyLock::new(|| {
        let mut m: HashMap<String, (i64, Option<&'static str>)> =
            FIXED.iter().map(|(o, n, _, l, _)| (n.to_string(), (*o, *l))).collect();
        for (i, l) in PREDEFINED.iter().enumerate() {
            m.insert(format!("{l}-x-icu"), (ICU_OIDS + i as i64, Some(*l)));
        }
        m
    });
    BY_NAME.get(name).copied()
}

/// Every built-in collation, as pg_collation lists them: oid, name, provider, ICU locale, collencoding.
pub(super) fn builtins() -> impl Iterator<Item = (i64, String, char, Option<&'static str>, i64)> {
    let fixed = FIXED.iter().map(|(o, n, p, l, e)| (*o, n.to_string(), *p, *l, *e));
    fixed.chain(
        PREDEFINED.iter().enumerate().map(|(i, l)| (ICU_OIDS + i as i64, format!("{l}-x-icu"), 'i', Some(*l), -1)),
    )
}

// ---------------------------------------------------------------- collations as expressions carry them

/// A collation as columns and expressions carry it: its place among those in use, each (name, locale) once. Only
/// collations that exist are kept (a name not looked up yet stays a name: see `Named`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Coll(u32);

impl Coll {
    /// The database's collation, "default": bytes.
    pub(crate) const DEFAULT: Coll = Coll(0);
    /// Two different implicit collations that met where one is needed: an error when it is used (42P22).
    pub(crate) const INDETERMINATE: Coll = Coll(u32::MAX);
}

/// What a `Coll` stands for: a built-in collation, or one made with CREATE COLLATION (its stored name).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Def {
    name: String,
    builtin: bool,
    /// the ICU locale (None: bytes)
    locale: Option<String>,
    /// nondeterministic: strings its collator ties are equal (see `eq_key`)
    nd: bool,
}

#[derive(Default)]
struct Interned {
    defs: Vec<Arc<Def>>,
    at: HashMap<Arc<Def>, u32>,
}

/// The collations in use. They are few (the built-ins a database names, and those made with CREATE COLLATION that
/// its tables and statements name), so nothing is forgotten.
static IN_USE: LazyLock<RwLock<Interned>> = LazyLock::new(|| {
    let mut t = Interned::default();
    intern_in(&mut t, Def { name: "default".into(), builtin: true, locale: None, nd: false });
    RwLock::new(t)
});

fn intern_in(t: &mut Interned, d: Def) -> Coll {
    if let Some(&i) = t.at.get(&d) {
        return Coll(i);
    }
    let i = t.defs.len() as u32;
    let d = Arc::new(d);
    t.at.insert(d.clone(), i);
    t.defs.push(d);
    Coll(i)
}

/// How many collations are kept (see `IN_USE`); for tests.
pub(crate) fn in_use() -> usize {
    IN_USE.read().unwrap().defs.len()
}

fn intern(d: Def) -> Coll {
    if let Some(&i) = IN_USE.read().unwrap().at.get(&d) {
        return Coll(i);
    }
    intern_in(&mut IN_USE.write().unwrap(), d)
}

/// What this thread has read of `IN_USE`, by place: a collation's definition never changes once it has a place, so
/// a comparison or a key made per row reads it without a lock. Also its collator (None: bytes), once made, and its
/// locale's language, for case mapping.
#[derive(Default, Clone)]
struct Seen {
    def: Option<Arc<Def>>,
    collator: Option<Option<Arc<CollatorBorrowed<'static>>>>,
    lang: Option<LanguageIdentifier>,
}

thread_local! {
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` on what this thread keeps of `c` (see `Seen`), reading `IN_USE` the first time.
fn seen<T>(c: Coll, f: impl FnOnce(&mut Seen) -> T) -> T {
    SEEN.with_borrow_mut(|all| {
        let i = c.0 as usize;
        if all.len() <= i {
            all.resize(i + 1, Seen::default());
        }
        if all[i].def.is_none() {
            all[i].def = IN_USE.read().unwrap().defs.get(i).cloned();
        }
        f(&mut all[i])
    })
}

fn def(c: Coll) -> Option<Arc<Def>> {
    if c == Coll::INDETERMINATE {
        return None;
    }
    seen(c, |s| s.def.clone())
}

/// The names main read as collations of bytes, any case (`collate c`, `"C.utf8"`), which views, CHECKs and functions
/// it kept may hold: what each is now.
const LENIENT: [(&str, &str); 6] = [
    ("c", "C"),
    ("posix", "POSIX"),
    ("default", "default"),
    ("ucs_basic", "ucs_basic"),
    ("c.utf-8", "C.UTF-8"),
    ("c.utf8", "C.UTF-8"),
];

/// A built-in collation by the name a COLLATE clause gives it (in pg_catalog, or unqualified): its `Coll`.
pub(super) fn builtin_named(schema: Option<&str>, name: &str) -> Option<Coll> {
    if name == CITEXT && matches!(schema, None | Some("pg_catalog" | "public")) {
        return Some(citext());
    }
    if !matches!(schema, None | Some("pg_catalog")) {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    // a libc locale's name (`en_US`, `de_DE.utf8`): the ICU collation of that locale, which Chronos sorts with (it
    // has no libc locales; ICU and glibc order a few strings differently, punctuation among them)
    if builtin(name).is_none()
        && !LENIENT.iter().any(|(l, _)| *l == lower)
        && let Some(icu) = libc_as_icu(name)
    {
        return Some(intern(Def { name: name.into(), builtin: true, locale: Some(icu), nd: false }));
    }
    let name = match builtin(name) {
        Some(_) => name,
        None => LENIENT.iter().find(|(l, _)| *l == lower)?.1,
    };
    let (_, locale) = builtin(name)?;
    Some(intern(Def { name: name.into(), builtin: true, locale: locale.map(String::from), nd: false }))
}

/// The name of the collation a `citext` column has.
const CITEXT: &str = "citext";

/// The type id the catalog gives a citext column (`pg_attribute.atttypid`): `format_type` says `citext` of it. On
/// the wire it is text.
pub(crate) const CITEXT_OID: i64 = 16001;

/// The collation of `citext`: its text is equal, and sorts, by its lower case (see the module docs).
pub(crate) fn citext() -> Coll {
    intern(Def { name: CITEXT.into(), builtin: true, locale: None, nd: true })
}

/// What citext compares text by: its lower case, as `lower()` gives it.
fn fold(s: &str) -> String {
    s.to_lowercase()
}

/// The `Coll` of a collation made with CREATE COLLATION, as a column keeps it: its stored name and locale (None
/// for libc's C).
pub(super) fn made(name: &str, locale: Option<&str>) -> Coll {
    made_as(name, locale, false)
}

/// `made`, for one that may be nondeterministic (`deterministic = false`).
pub(super) fn made_as(name: &str, locale: Option<&str>, nd: bool) -> Coll {
    intern(Def { name: name.into(), builtin: false, locale: locale.map(String::from), nd })
}

impl Coll {
    /// The collation's name as pg_collation has it (with its schema for one made here: `app.german`), and whether
    /// it is built in.
    pub(crate) fn name(self) -> (String, bool) {
        match def(self) {
            Some(d) => (d.name.clone(), d.builtin),
            None => ("default".into(), true),
        }
    }

    /// Its name as pg_collation's collname has it, without its schema, as Postgres's messages name a collation.
    pub(crate) fn collname(self) -> String {
        match self.name() {
            (name, true) => name,
            (name, false) => rel_part(&name).to_string(),
        }
    }

    /// Its ICU locale; None for bytes.
    pub(crate) fn locale(self) -> Option<String> {
        def(self).and_then(|d| d.locale.clone())
    }

    /// Whether it sorts by bytes (C, POSIX, the default).
    pub(crate) fn bytes(self) -> bool {
        self == Coll::DEFAULT || def(self).is_some_and(|d| d.locale.is_none() && !d.nd)
    }

    /// Whether it is citext's: equal and sorted by lower case, with no ICU locale.
    pub(crate) fn folds(self) -> bool {
        self != Coll::DEFAULT && self != Coll::INDETERMINATE && def(self).is_some_and(|d| d.nd && d.locale.is_none())
    }

    /// Whether it sorts by a key (`key`), not by its text's bytes: an ICU collation, or citext's.
    pub(crate) fn keyed(self) -> bool {
        self.icu() || self.folds()
    }

    /// Whether strings its collator ties are equal in it (`deterministic = false`).
    pub(crate) fn nondeterministic(self) -> bool {
        self != Coll::DEFAULT && self != Coll::INDETERMINATE && def(self).is_some_and(|d| d.nd)
    }

    /// Whether it sorts by ICU's rules, not by bytes.
    pub(crate) fn icu(self) -> bool {
        self != Coll::DEFAULT && self != Coll::INDETERMINATE && def(self).is_some_and(|d| d.locale.is_some())
    }

    /// The name as a COLLATE clause writes it: quoted where it needs it, with its schema when it isn't public's or
    /// built in.
    pub(crate) fn sql(self) -> String {
        let (name, builtin) = self.name();
        match (builtin, name.split_once('.')) {
            (false, Some((s, n))) => format!("{}.{}", super::ident(s), super::ident(n)),
            _ => super::ident(&name),
        }
    }

    /// The name as Postgres writes it in an index's definition or for regcollation: quoted where it needs it, with
    /// its schema when the search_path doesn't find it by its name.
    pub(crate) fn shown(self) -> String {
        let (name, builtin) = self.name();
        if builtin {
            return quoted(&name);
        }
        let (s, n) = (schema_part(&name), rel_part(&name));
        let found = path_schemas().unwrap_or_else(|| vec!["public".into()]).iter().any(|p| p == s);
        if found { quoted(n) } else { format!("{}.{}", quoted(s), quoted(n)) }
    }

    /// As a column's schema keeps it.
    pub(super) fn to_json(self) -> Option<Json> {
        if self == Coll::DEFAULT || self == Coll::INDETERMINATE {
            return None;
        }
        let d = def(self)?;
        let mut j = serde_json::json!({ "name": d.name, "locale": d.locale });
        if d.builtin {
            j["builtin"] = Json::Bool(true);
        }
        if d.locale.is_some() {
            j["version"] = Json::String(VERSION.into()); // (what made the keys an index of it holds)
        }
        if d.nd {
            j["deterministic"] = Json::Bool(false);
        }
        Some(j)
    }

    /// From a column's schema (`to_json`); the default where there is none.
    pub(super) fn from_json(j: &Json) -> Coll {
        let Some(name) = j["name"].as_str() else { return Coll::DEFAULT };
        let locale = j["locale"].as_str().map(String::from);
        intern(Def { name: name.into(), builtin: j["builtin"] == true, locale, nd: j["deterministic"] == false })
    }
}

/// A name as Postgres's quote_identifier writes it: bare when it is lowercase letters, digits and underscores
/// and no keyword but an unreserved one, else quoted (`"de-x-icu"`, `"C"`, `"default"`).
pub(super) fn quoted(name: &str) -> String {
    static KEYWORDS: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
        include_str!("keywords.tsv")
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                Some((f.next()?, f.next()?))
            })
            .collect()
    });
    let plain = name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && KEYWORDS.get(name).is_none_or(|k| *k == "U");
    if plain { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

/// A collation as a statement names it (`COLLATE "de-x-icu"`, `app.german`): a built-in one, found when the
/// statement is parsed; one made with CREATE COLLATION keeps its name until the statement runs (`resolve`), as
/// Postgres looks names up then. A name that names nothing is never kept beyond its statement.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Named {
    Coll(Coll),
    Name(Option<String>, String),
}

impl Named {
    pub(super) fn new(schema: Option<String>, name: String) -> Named {
        match builtin_named(schema.as_deref(), &name) {
            Some(c) => Named::Coll(c),
            None => Named::Name(schema, name),
        }
    }

    /// The collation it names: looked up in `v`, or the running session's world, along its search_path; 42704 if
    /// there's none.
    pub(super) fn resolve(&self, v: Option<&View>) -> Result<Coll> {
        match self {
            Named::Coll(c) => Ok(*c),
            Named::Name(schema, name) => lookup(v, schema.as_deref(), name),
        }
    }

    /// Whether it is a collation of bytes (one made with CREATE COLLATION isn't known to be until it's looked up).
    pub(super) fn bytes(&self) -> bool {
        matches!(self, Named::Coll(c) if c.bytes())
    }
}

/// Collation `schema.name` (or `name` along the search_path) of `v` or of the running session's world: 42704 if
/// there's none, 0A000 for one of Postgres's whose rules ICU4X lacks or a libc locale.
pub(super) fn lookup(v: Option<&View>, schema: Option<&str>, name: &str) -> Result<Coll> {
    if let Some(c) = builtin_named(schema, name) {
        return Ok(c);
    }
    let found = match v {
        Some(v) => find(v, schema, name)?,
        None => with_path_view(|v| find(v, schema, name)).transpose()?.flatten(),
    };
    let Some((stored, j)) = found else { return Err(missing(schema, name)) };
    // (as Postgres warns when a collation's version isn't the library's: its keys in indexes may be another order)
    if j["provider"] == "icu"
        && let Some(made) = j["version"].as_str().filter(|x| *x != VERSION)
    {
        notice(
            "WARNING",
            format!(
                "collation \"{name}\" has version mismatch: it was made with version {made}, but Chronos's ICU is \
                 version {VERSION}; rebuild the indexes of it (REINDEX) and run ALTER COLLATION {name} REFRESH VERSION"
            ),
        );
    }
    Ok(made_as(&stored, j["locale"].as_str(), j["deterministic"] == false))
}

/// The ICU locale a libc locale's name stands for (`en_US.utf8` is `en-US`), if it is one ICU4X has the rules of.
fn libc_as_icu(name: &str) -> Option<String> {
    if !libc_name(name) {
        return None;
    }
    let icu = name.split(['.', '@']).next().unwrap_or(name).replace('_', "-");
    uncovered(&icu).is_none().then_some(icu)
}

/// Whether `name` is a libc locale's (`en_US`, `de_DE.UTF-8`): a language, `_`, a region, maybe an encoding.
fn libc_name(name: &str) -> bool {
    let base = name.split(['.', '@']).next().unwrap_or(name);
    let Some((lang, region)) = base.split_once('_') else { return false };
    (2..=3).contains(&lang.len())
        && lang.chars().all(|c| c.is_ascii_lowercase())
        && (region.len() == 2 && region.chars().all(|c| c.is_ascii_uppercase())
            || region.len() == 3 && region.chars().all(|c| c.is_ascii_digit()))
}

/// 42704 for a collation there's none of; 0A000 for one Postgres has that Chronos hasn't (one of its ICU
/// collations whose rules ICU4X lacks, or a libc locale's).
pub(super) fn missing(schema: Option<&str>, name: &str) -> SqlError {
    if matches!(schema, None | Some("pg_catalog")) {
        if let Some(loc) = name.strip_suffix("-x-icu")
            && let Some((_, lang)) = uncovered(loc)
        {
            return refused_locale(name, lang);
        }
        if libc_name(name) {
            let icu = name.split(['.', '@']).next().unwrap_or(name).replace('_', "-");
            return SqlError {
                code: UNSUPPORTED,
                message: format!(
                    "collation \"{name}\" isn't supported: Postgres makes it from the operating system's libc locale, \
                     and Chronos has no libc collations but C and POSIX; use the ICU one, \"{icu}-x-icu\""
                ),
                info: None,
            };
        }
    }
    let shown = match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    };
    SqlError {
        code: "42704",
        message: format!("collation \"{shown}\" for encoding \"UTF8\" does not exist"),
        info: None,
    }
}

fn refused_locale(what: &str, lang: &str) -> SqlError {
    SqlError {
        code: UNSUPPORTED,
        message: format!(
            "collation \"{what}\" isn't supported: ICU4X, which Chronos sorts with, has no rules for {lang} that \
             order as ICU4C's do, so its order would differ from Postgres's; use \"und-x-icu\" (the root order) or a \
             supported locale"
        ),
        info: None,
    }
}

/// A collation made with CREATE COLLATION, found as `schema.name` or along the search_path: its stored name and
/// definition.
pub(super) fn find(v: &View, schema: Option<&str>, name: &str) -> Result<Option<(String, Json)>> {
    let schemas = match schema {
        Some(s) => vec![s.to_string()],
        None => path_schemas().unwrap_or_else(|| vec!["public".into()]),
    };
    for s in schemas {
        let stored = qualify(&s, name);
        if let Some(raw) = v.get(&row_key(&stored))? {
            return Ok(Some((stored, serde_json::from_str(&raw).unwrap_or_default())));
        }
    }
    Ok(None)
}

/// Every collation made with CREATE COLLATION in `v`: stored name, definition.
pub(super) fn all(v: &View) -> Result<Vec<(String, Json)>> {
    let prefix = format!("{COLLATIONS}/");
    let mut out = vec![];
    for r in v.prefix(&prefix)? {
        let (k, raw) = r?;
        out.push((k[prefix.len()..].to_string(), serde_json::from_str(&raw).unwrap_or_default()));
    }
    Ok(out)
}

// ---------------------------------------------------------------- ICU

/// The collators made so far, by locale.
static COLLATORS: LazyLock<Mutex<HashMap<String, Arc<CollatorBorrowed<'static>>>>> = LazyLock::new(Default::default);

/// The collator for a canonical locale (see `canonical`): ICU4X's preferences from it (`co`, `kf`, `kn`), and the
/// options ICU4X doesn't read from a locale (`ks`, `ka`, `kv`, `kc`) set from its keywords, as ICU4C reads them.
fn collator_of(locale: &str) -> Result<Arc<CollatorBorrowed<'static>>> {
    if let Some(c) = COLLATORS.lock().unwrap().get(locale) {
        return Ok(c.clone());
    }
    let bad = || SqlError {
        code: UNSUPPORTED,
        message: format!("ICU locale \"{locale}\" isn't supported: ICU4X can't make a collator of it"),
        info: None,
    };
    let mut l: Locale = Locale::try_from_str(locale).map_err(|_| bad())?;
    let mut o = CollatorOptions::default();
    let kw = |k: &str| Key::try_from_str(k).ok();
    let mut take = |k: &str| -> Option<String> {
        let key = kw(k)?;
        let v = l.extensions.unicode.keywords.remove(key)?;
        Some(v.to_string())
    };
    if let Some(v) = take("ks") {
        o.strength = Some(match v.as_str() {
            "level1" => Level::Primary,
            "level2" => Level::Secondary,
            "level3" => Level::Tertiary,
            "level4" => Level::Quaternary,
            "identic" => Level::Identical,
            _ => return Err(bad()),
        });
    }
    if let Some(v) = take("ka") {
        o.alternate_handling = Some(match v.as_str() {
            "noignore" => AlternateHandling::NonIgnorable,
            "shifted" => AlternateHandling::Shifted,
            _ => return Err(bad()),
        });
    }
    if let Some(v) = take("kv") {
        o.max_variable = Some(match v.as_str() {
            "space" => MaxVariable::Space,
            "punct" => MaxVariable::Punctuation,
            "symbol" => MaxVariable::Symbol,
            "currency" => MaxVariable::Currency,
            _ => return Err(bad()),
        });
    }
    if let Some(v) = take("kc") {
        o.case_level = Some(if v.is_empty() || v == "true" { CaseLevel::On } else { CaseLevel::Off });
    }
    take("kk"); // normalization: ICU4X always normalizes, which only matters for unnormalized text
    // ICU4C's POSIX variant is a keyword, `en-US-u-va-posix`; ICU4X's data has it as a variant
    if take("va").as_deref() == Some("posix")
        && let Ok(p) = Variant::try_from_str("posix")
    {
        l.id.variants.push(p);
    }
    let prefs = CollatorPreferences::from(&l);
    let c = Arc::new(CollatorBorrowed::try_new(prefs, o).map_err(|_| bad())?);
    COLLATORS.lock().unwrap().insert(locale.to_string(), c.clone());
    Ok(c)
}

/// `c`'s collator; None for bytes. Made when first used, then kept by each thread that uses it (see `Seen`).
fn collator(c: Coll) -> Result<Option<Arc<CollatorBorrowed<'static>>>> {
    if c == Coll::DEFAULT {
        return Ok(None);
    }
    if c == Coll::INDETERMINATE {
        return Err(indeterminate());
    }
    if let Some(x) = seen(c, |s| s.collator.clone()) {
        return Ok(x);
    }
    let x = match def(c).and_then(|d| d.locale.clone()) {
        Some(locale) => Some(collator_of(&locale)?),
        None => None,
    };
    seen(c, |s| s.collator = Some(x.clone()));
    Ok(x)
}

/// An error with Postgres's HINT.
fn hinted(code: &'static str, message: String, hint: &str) -> SqlError {
    SqlError { code, message, info: Some(Box::new(Violation { hint: Some(hint.to_string()), ..Violation::default() })) }
}

/// 42P22: text compared or sorted where two implicit collations met.
pub(super) fn indeterminate() -> SqlError {
    hinted(
        "42P22",
        "could not determine which collation to use for string comparison".into(),
        "Use the COLLATE clause to set the collation explicitly.",
    )
}

/// 42P22: a new column (CREATE TABLE AS, a materialized view's) of text where two implicit collations met.
pub(super) fn underived(column: &str) -> SqlError {
    hinted(
        "42P22",
        format!("no collation was derived for column \"{column}\" with collatable type text"),
        "Use the COLLATE clause to set the collation explicitly.",
    )
}

/// The key text sorts by in collation `c` (see the module docs): ICU's sort key, a 0 byte, then the text's bytes.
/// In a collation of bytes, the bytes.
pub(crate) fn key(c: Coll, s: &str) -> Result<Vec<u8>> {
    if c.folds() {
        // (its lower case, a 0 byte, then the text: equal strings sort together, their bytes deciding among them)
        let mut k = fold(s).into_bytes();
        k.push(0);
        k.extend_from_slice(s.as_bytes());
        return Ok(k);
    }
    let Some(x) = collator(c)? else { return Ok(s.as_bytes().to_vec()) };
    let mut k = Vec::with_capacity(s.len() * 3 + 1);
    let Ok(()) = x.write_sort_key_to(s, &mut k);
    debug_assert!(!k.contains(&0), "ICU sort keys hold no zero byte");
    k.push(0);
    k.extend_from_slice(s.as_bytes());
    Ok(k)
}

/// The key text is equal by in nondeterministic collation `c`: ICU's sort key alone, so the strings its collator
/// ties have one key. It orders as the collation does, with ties equal. In another collation, the bytes.
pub(crate) fn eq_key(c: Coll, s: &str) -> Result<Vec<u8>> {
    if !c.nondeterministic() {
        return Ok(s.as_bytes().to_vec());
    }
    if c.folds() {
        return Ok(fold(s).into_bytes());
    }
    let Some(x) = collator(c)? else { return Ok(s.as_bytes().to_vec()) };
    let mut k = Vec::with_capacity(s.len() * 3);
    let Ok(()) = x.write_sort_key_to(s, &mut k);
    Ok(k)
}

/// The text a key (`key`) was made of.
pub(crate) fn text_of(key: &[u8]) -> &[u8] {
    key.iter().position(|&b| b == 0).map_or(key, |i| &key[i + 1..])
}

/// upper (`upper`) or lower of text in ICU collation `c`, as Postgres maps case under one: ICU's full mapping in the
/// locale's language (`ß` is `SS`; Turkish dotted i; Greek final sigma and accents); 42P22 where two implicit
/// collations met.
pub(crate) fn recase(c: Coll, s: &str, upper: bool) -> Result<String> {
    if c == Coll::INDETERMINATE {
        let what = if upper { "upper()" } else { "lower()" };
        return err("42P22", format!("could not determine which collation to use for {what} function"));
    }
    let lang = seen(c, |seen| {
        if seen.lang.is_none() {
            let locale = seen.def.as_ref().and_then(|d| d.locale.clone());
            seen.lang = Some(
                locale
                    .and_then(|l| Locale::try_from_str(&l).ok())
                    .map(|l| LanguageIdentifier::from(l.id.language))
                    .unwrap_or(LanguageIdentifier::UNKNOWN),
            );
        }
        seen.lang.clone().unwrap_or(LanguageIdentifier::UNKNOWN)
    });
    let m = icu_casemap::CaseMapper::new();
    Ok(match upper {
        true => m.uppercase_to_string(s, &lang).into_owned(),
        false => m.lowercase_to_string(s, &lang).into_owned(),
    })
}

/// Two texts in collation `c`'s order: ICU's, then their bytes where it ties them.
pub(crate) fn compare(c: Coll, a: &str, b: &str) -> Result<Ordering> {
    if c.folds() {
        return Ok(fold(a).cmp(&fold(b)));
    }
    Ok(match collator(c)? {
        Some(x) if c.nondeterministic() => x.compare(a, b), // (what it ties is equal)
        Some(x) => x.compare(a, b).then_with(|| a.as_bytes().cmp(b.as_bytes())),
        None => a.as_bytes().cmp(b.as_bytes()),
    })
}

// ---------------------------------------------------------------- locales

/// A locale as Postgres 16 keeps an ICU collation's (`colliculocale`): a BCP 47 language tag, `de-DE`,
/// `und-u-kn`, from what CREATE COLLATION was given (`de_DE`, `de-DE@collation=phonebook`, `''`), as ICU4C's
/// uloc_toLanguageTag writes it.
pub(super) fn canonical(given: &str) -> Result<String> {
    let mut s = given.trim().to_string();
    let mut keywords: Vec<(String, String)> = vec![];
    if let Some((base, kws)) = s.clone().split_once('@') {
        s = base.to_string();
        for kv in kws.split(';').filter(|x| !x.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
            let (key, value) = match (k.as_str(), v.as_str()) {
                ("collation", v) => (
                    "co",
                    match v {
                        "phonebook" => "phonebk",
                        "traditional" => "trad",
                        "dictionary" => "dict",
                        "gb2312han" => "gb2312",
                        v => v,
                    }
                    .to_string(),
                ),
                // (a value ICU doesn't know stays as it is, for `check_locale` to refuse as ICU does)
                ("colnumeric", v) => ("kn", yes_no(v)),
                ("colcasefirst", v) => ("kf", if v == "no" { "false".into() } else { v.to_string() }),
                ("colcaselevel", v) => ("kc", yes_no(v)),
                ("colalternate", v) => ("ka", if v == "non-ignorable" { "noignore".into() } else { v.to_string() }),
                ("colstrength", v) => (
                    "ks",
                    match v {
                        "primary" => "level1",
                        "secondary" => "level2",
                        "tertiary" => "level3",
                        "quaternary" => "level4",
                        "identical" => "identic",
                        v => v,
                    }
                    .into(),
                ),
                ("colreorder", v) => ("kr", v.replace(['_', ','], "-")),
                ("colbackwards", v) => ("kb", yes_no(v)),
                ("colnormalization", v) => ("kk", yes_no(v)),
                // (keywords that don't change an order: a currency, a calendar; and what isn't a keyword at all)
                _ => continue,
            };
            keywords.push((key.into(), value));
        }
    }
    // a POSIX locale name's encoding (`en_US.UTF-8`) goes
    if let Some((base, _)) = s.clone().split_once('.') {
        s = base.to_string();
    }
    let s = s.replace('_', "-");
    if s.is_empty() || s.eq_ignore_ascii_case("root") {
        return Ok(with_keywords("und".into(), &keywords));
    }
    let Ok(mut l) = Locale::try_from_str(&s) else {
        return Ok(with_keywords("und".into(), &keywords));
    };
    for (k, v) in &keywords {
        if let (Ok(k), Ok(v)) = (Key::try_from_str(k), KwValue::try_from_str(v)) {
            l.extensions.unicode.keywords.set(k, v);
        }
    }
    Ok(l.to_string())
}

/// An ICU `@` keyword's yes or no as a `-u-` keyword's (anything else as it is).
fn yes_no(v: &str) -> String {
    match v {
        "yes" | "true" | "on" => "true".into(),
        "no" | "false" | "off" => "false".into(),
        v => v.into(),
    }
}

fn with_keywords(base: String, keywords: &[(String, String)]) -> String {
    if keywords.is_empty() {
        return base;
    }
    let mut kws: Vec<String> = keywords.iter().map(|(k, v)| format!("{k}-{v}")).collect();
    kws.sort();
    canonical(&format!("{base}-u-{}", kws.join("-"))).unwrap_or(base)
}

/// Locales Postgres doesn't make collations of, checked to order as ICU4C orders them (against Postgres 16 with
/// ICU4C 78): a region without the script (`zh-TW` for `zh-Hant-TW`). Other such forms (`sr-ME`, `uz-AF`, `pa-PK`,
/// `zh-Latn`, `sh`) resolve to other rules in ICU4X than in ICU4C, and are refused.
const ALIASES: [&str; 39] = [
    "az-AZ", "bs-BA", "ff-BF", "ff-CM", "ff-GH", "ff-GM", "ff-GN", "ff-GW", "ff-LR", "ff-MR", "ff-NE", "ff-NG",
    "ff-SL", "ff-SN", "kk-CN", "ks-IN", "ku-SY", "kxv-IN", "mni-IN", "pa-IN", "sat-IN", "sd-IN", "sd-PK", "shi-MA",
    "sr-BA", "sr-RS", "sr-XK", "su-ID", "uz-UZ", "vai-LR", "yue-CN", "yue-HK", "yue-MO", "zh-CN", "zh-HK", "zh-MO",
    "zh-MY", "zh-SG", "zh-TW",
];

/// The collation types (`-u-co-`) ICU4X orders as ICU4C does, on every locale it makes a collator of (`standard`
/// but in Chinese and Cantonese). ICU4X has no `search` or `searchjl`.
const TYPES: [&str; 17] = [
    "big5han", "compat", "dict", "direct", "ducet", "emoji", "eor", "gb2312", "phonebk", "phonetic", "pinyin",
    "reformed", "standard", "stroke", "trad", "unihan", "zhuyin",
];

/// XX000, as Postgres reports a locale ICU4C can't open a collator of (a keyword's value it doesn't know).
fn cant_open(locale: &str) -> SqlError {
    SqlError {
        code: "XX000",
        message: format!("could not open collator for locale \"{locale}\": U_ILLEGAL_ARGUMENT_ERROR"),
        info: None,
    }
}

/// Checks ICU4X sorts as `locale` (canonical) does in ICU4C: 0A000 for a locale or a collation type not shown to
/// (a language whose rules ICU4X lacks, a locale Postgres makes no collation of and that isn't an alias checked,
/// `search`), and for keywords ICU4X can't apply (`kr`, `kb`); XX000 for a keyword's value ICU doesn't know, as
/// Postgres has it.
pub(super) fn check_locale(locale: &str) -> Result<()> {
    let l = Locale::try_from_str(locale).map_err(|_| cant_open(locale))?;
    let base = l.id.to_string();
    if let Some((_, lang)) = uncovered(&base) {
        return Err(refused_locale(locale, lang));
    }
    if base != "und" && !PREDEFINED.contains(&base.as_str()) && !ALIASES.contains(&base.as_str()) {
        return err(
            UNSUPPORTED,
            format!(
                "ICU locale \"{locale}\" isn't supported: Chronos has the locales Postgres 16 makes ICU collations of \
                 (select colliculocale from pg_collation), whose orders ICU4X is checked to match; \"{base}\" isn't \
                 one of them"
            ),
        );
    }
    let lang = l.id.language.to_string();
    for (k, v) in l.extensions.unicode.keywords.iter() {
        let v = v.to_string();
        let ok = match k.as_str() {
            "co" if v.contains('-') => false,
            "co" if !TYPES.contains(&v.as_str()) || (v == "standard" && matches!(lang.as_str(), "zh" | "yue")) => {
                return err(
                    UNSUPPORTED,
                    format!(
                        "ICU collation type \"{v}\" (in \"{locale}\") isn't supported: ICU4X, which Chronos sorts with, \
                         doesn't order it as ICU4C does"
                    ),
                );
            }
            "co" => true,
            "kn" | "kc" | "kk" => matches!(v.as_str(), "" | "true" | "false"),
            "kf" => matches!(v.as_str(), "upper" | "lower" | "false"),
            "ks" => matches!(v.as_str(), "level1" | "level2" | "level3" | "level4" | "identic"),
            "ka" => matches!(v.as_str(), "noignore" | "shifted"),
            "kv" => matches!(v.as_str(), "space" | "punct" | "symbol" | "currency"),
            "kr" | "kb" => {
                let what = if k.as_str() == "kr" { "reordering scripts" } else { "backwards secondary order" };
                return err(
                    UNSUPPORTED,
                    format!("ICU locale \"{locale}\" isn't supported: ICU4X can't apply its keyword {k} ({what})"),
                );
            }
            "va" if v == "posix" && base == "en-US" => true,
            "va" => {
                return err(UNSUPPORTED, format!("ICU locale \"{locale}\" isn't supported: no variant \"{v}\" here"));
            }
            _ => true, // (a calendar, a currency: no order changes with them)
        };
        if !ok {
            return Err(cant_open(locale));
        }
    }
    collator_of(locale).map(|_| ())
}

/// Whether ICU knows the language of `locale` (canonical): one of the predefined collations' languages, as Postgres
/// warns of one it doesn't.
pub(super) fn known_language(locale: &str) -> bool {
    let lang = locale.split('-').next().unwrap_or_default();
    lang == "und" || PREDEFINED.iter().any(|l| l.split('-').next() == Some(lang))
}

// ---------------------------------------------------------------- CREATE COLLATION

/// ALTER COLLATION's changes (OWNER TO is a notice).
#[derive(Clone, Debug)]
pub(super) enum Change {
    Rename(String),
    Schema(String),
    Refresh,
}

/// CREATE COLLATION's writes: the collation's row, from its options or a copy of `from`'s.
pub(super) fn create(
    v: &View,
    (schema, name): &(Option<String>, String),
    opts: &[(String, String)],
    from: Option<&(Option<String>, String)>,
    if_not_exists: bool,
) -> Result<Vec<(String, Option<String>)>> {
    let stored = match schema {
        Some(s) => qualify(s, name),
        None => super::create_in(name)?,
    };
    super::need_schema(v, &stored)?;
    if v.get(&row_key(&stored))?.is_some() {
        if if_not_exists {
            notice("NOTICE", format!("collation \"{name}\" already exists, skipping"));
            return Ok(vec![]);
        }
        return err("42710", format!("collation \"{name}\" already exists"));
    }
    let def = match from {
        Some((s, n)) => copied(v, s.as_deref(), n)?,
        None => defined(opts)?,
    };
    Ok(vec![(row_key(&stored), Some(def.to_string()))])
}

/// CREATE COLLATION ... FROM other: other's definition.
fn copied(v: &View, schema: Option<&str>, name: &str) -> Result<Json> {
    if matches!(schema, None | Some("pg_catalog"))
        && let Some((oid, locale)) = builtin(name)
    {
        let (.., provider, _, encoding) =
            FIXED.iter().find(|f| f.0 == oid).copied().unwrap_or((oid, "", 'i', None, -1));
        return Ok(match (provider, locale) {
            ('d', _) => return err("42P17", "collation \"default\" cannot be copied"),
            (_, Some(l)) => serde_json::json!({ "provider": "icu", "locale": l, "version": VERSION }),
            (_, None) => {
                let c = libc_locale(name);
                serde_json::json!({ "provider": "libc", "collate": c, "ctype": c, "encoding": encoding })
            }
        });
    }
    match find(v, schema, name)? {
        Some((_, j)) => Ok(j),
        None => Err(missing(schema, name)),
    }
}

/// A boolean option's value, as Postgres reads one.
fn boolean(key: &str, v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" | "t" => Ok(true),
        "false" | "off" | "no" | "0" | "f" => Ok(false),
        _ => err("22023", format!("{key} requires a Boolean value")),
    }
}

/// CREATE COLLATION's options as a definition: an ICU collation of a locale ICU4X sorts as ICU4C does (one that is
/// nondeterministic too), or libc's C or POSIX. ICU rules and other libc locales are 0A000.
fn defined(opts: &[(String, String)]) -> Result<Json> {
    let mut get: HashMap<&str, &str> = HashMap::new();
    for (k, v) in opts {
        match k.as_str() {
            "provider" | "locale" | "lc_collate" | "lc_ctype" | "deterministic" | "rules" | "version" => {
                if get.insert(k.as_str(), v.as_str()).is_some() {
                    return err("42601", "conflicting or redundant options");
                }
            }
            _ => return err("42601", format!("collation attribute \"{k}\" not recognized")),
        }
    }
    let provider = get.get("provider").map_or("libc".to_string(), |p| p.to_ascii_lowercase());
    if !matches!(provider.as_str(), "icu" | "libc") {
        return err("42P17", format!("unrecognized collation provider: {provider}"));
    }
    let nd = match get.get("deterministic") {
        Some(d) => !boolean("deterministic", d)?,
        None => false,
    };
    if nd && provider != "icu" {
        return err(UNSUPPORTED, "nondeterministic collations not supported with this provider");
    }
    if get.contains_key("rules") {
        return err(
            UNSUPPORTED,
            "ICU rules aren't supported: ICU4X, which Chronos sorts with, has no rule-based tailoring",
        );
    }
    if provider == "libc" {
        let collate = get.get("lc_collate").or(get.get("locale")).copied();
        let ctype = get.get("lc_ctype").or(get.get("locale")).copied();
        let (Some(collate), Some(ctype)) = (collate, ctype) else {
            let which = if collate.is_none() { "lc_collate" } else { "lc_ctype" };
            return err("42P17", format!("parameter \"{which}\" must be specified"));
        };
        // (a libc locale is made the ICU collation of that locale: see `libc_as_icu`)
        if !matches!(collate, "C" | "POSIX" | "C.UTF-8" | "C.utf8")
            && let Some(icu) = libc_as_icu(collate)
        {
            return Ok(serde_json::json!({ "provider": "icu", "locale": icu, "version": VERSION }));
        }
        for l in [collate, ctype] {
            if !matches!(l, "C" | "POSIX" | "C.UTF-8" | "C.utf8") {
                return err(
                    UNSUPPORTED,
                    format!(
                        "libc locale \"{l}\" isn't supported: Chronos has no libc collations but C and POSIX (text by its \
                         bytes); use an ICU collation, (provider = icu, locale = '...')"
                    ),
                );
            }
        }
        return Ok(serde_json::json!({ "provider": "libc", "collate": collate, "ctype": ctype, "encoding": 6 }));
    }
    let Some(given) = get.get("locale").copied() else {
        return err("42P17", "parameter \"locale\" must be specified");
    };
    let locale = canonical(given)?;
    if locale != given {
        notice("NOTICE", format!("using standard form \"{locale}\" for ICU locale \"{given}\""));
    }
    if !known_language(&locale) {
        let lang = locale.split('-').next().unwrap_or_default();
        notice("WARNING", format!("ICU locale \"{locale}\" has unknown language \"{lang}\""));
    }
    check_locale(&locale)?;
    let version = get.get("version").map_or(VERSION, |v| v);
    let mut j = serde_json::json!({ "provider": "icu", "locale": locale, "version": version });
    if nd {
        j["deterministic"] = Json::Bool(false);
    }
    Ok(j)
}

/// The collations' rows a statement writes keep columns' copies right: `from` renamed `to` in every table that has
/// a column or an index part of it (the schemas keep its name).
pub(super) fn renamed_in(tables: &mut [super::Table], from: &str, to: &str) -> Vec<String> {
    let mut changed = vec![];
    let is = |c: Coll| {
        let (n, b) = c.name();
        !b && n == from
    };
    for t in tables {
        let mut any = false;
        for c in &mut t.cols {
            if is(c.coll) {
                c.coll = made_as(to, c.coll.locale().as_deref(), c.coll.nondeterministic());
                any = true;
            }
        }
        for x in &mut t.indexes {
            for c in &mut x.colls {
                if is(*c) {
                    *c = made_as(to, c.locale().as_deref(), c.nondeterministic());
                    any = true;
                }
            }
        }
        if any {
            changed.push(t.name.clone());
        }
    }
    changed
}

/// Whether a collation is `stored` (one made with CREATE COLLATION).
pub(super) fn is_made(c: Coll, stored: &str) -> bool {
    let (n, b) = c.name();
    !b && n == stored
}

/// The collations SQL names in COLLATE clauses (`collate german`, `collate app."Phone"`): schema, name. Read by
/// tokens, so a string that says `collate x` names nothing.
pub(super) fn names_in(sql: &str) -> Vec<(Option<String>, String)> {
    let Ok(toks) = super::lex(sql) else { return vec![] };
    let name = |t: Option<&Tok>| match t {
        Some(Tok::Word(w) | Tok::Ident(w)) => Some(w.clone()),
        _ => None,
    };
    let mut out = vec![];
    for (i, t) in toks.iter().enumerate() {
        if !matches!(t, Tok::Word(w) if w == "collate") {
            continue;
        }
        let Some(first) = name(toks.get(i + 1)) else { continue };
        match (toks.get(i + 2), name(toks.get(i + 3))) {
            (Some(Tok::Sym(".")), Some(second)) => out.push((Some(first), second)),
            _ => out.push((None, first)),
        }
    }
    out
}

/// SQL to keep (a CHECK, a generated column's expression, an index's expression or WHERE, a view's query) with
/// each collation it names without a schema named with the schema the search_path finds it in now: as Postgres
/// keeps the collation's oid, it names the same collation under any later search_path (`collate c` made under
/// `search_path = a, b` is `collate a.c`), and DROP COLLATION finds it. Built-in collations, found before the
/// search_path is, stay as written.
pub(super) fn bind(v: &View, sql: &str) -> String {
    let Ok((toks, starts)) = super::lex_at(sql) else { return sql.into() };
    let mut at = vec![]; // where a schema goes, and which
    for (i, t) in toks.iter().enumerate() {
        let (Tok::Word(w), Some(Tok::Word(n) | Tok::Ident(n))) = (t, toks.get(i + 1)) else { continue };
        if w != "collate" || toks.get(i + 2) == Some(&Tok::Sym(".")) || builtin_named(None, n).is_some() {
            continue;
        }
        if let Ok(Some((stored, _))) = find(v, None, n) {
            at.push((starts[i + 1], format!("{}.", super::ident(schema_part(&stored)))));
        }
    }
    let mut out: Vec<char> = sql.chars().collect();
    for (i, schema) in at.into_iter().rev() {
        out.splice(i..i, schema.chars());
    }
    out.into_iter().collect()
}

/// Whether SQL (a view's query, a CHECK, an index's expression or WHERE) names the collation stored as `stored`.
/// Names are bound to their schemas when the SQL is kept (see `bind`); a bare one that isn't a built-in's (kept
/// before names were bound) may mean the collation of that name in any schema, so it is taken for each.
pub(super) fn uses(sql: &str, stored: &str) -> bool {
    names_in(sql).into_iter().any(|(s, n)| match s {
        Some(s) => qualify(&s, &n) == stored,
        None => n == rel_part(stored) && builtin_named(None, &n).is_none(),
    })
}

// ---------------------------------------------------------------- derivation

/// An expression's collation and how sure it is, as Postgres derives them: none (not text), implicit (a column's,
/// or the default), two different implicit ones (an error where one is needed), or explicit (COLLATE).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Strength {
    None,
    Implicit(Coll),
    Conflict(Coll, Coll),
    Explicit(Coll),
}

impl Strength {
    /// The collation it settles on: the default where there's none, INDETERMINATE for a conflict.
    pub(super) fn coll(self) -> Coll {
        match self {
            Strength::None => Coll::DEFAULT,
            Strength::Implicit(c) | Strength::Explicit(c) => c,
            Strength::Conflict(..) => Coll::INDETERMINATE,
        }
    }

    /// What a column holding a value of this collation has (a subquery's, a view's): a conflict leaves it none.
    pub(super) fn column(self) -> Coll {
        match self {
            Strength::Conflict(..) => Coll::INDETERMINATE,
            s => s.coll(),
        }
    }
}

fn shown(c: Coll) -> String {
    c.name().0
}

/// Two expressions' collations meeting (`a < b`, `a || b`, a CASE's branches): explicit beats implicit, a
/// collation other than the default beats the default, two different explicit ones are 42P21, and two different
/// implicit ones a conflict, an error only where a collation is needed.
pub(super) fn merge(a: Strength, b: Strength) -> Result<Strength> {
    use Strength::*;
    Ok(match (a, b) {
        (None, x) | (x, None) => x,
        (Explicit(x), Explicit(y)) if x != y => {
            return err(
                "42P21",
                format!("collation mismatch between explicit collations \"{}\" and \"{}\"", shown(x), shown(y)),
            );
        }
        (Explicit(_), _) => a,
        (_, Explicit(_)) => b,
        (Conflict(..), _) => a,
        (_, Conflict(..)) => b,
        // (citext is text of the default collation to Postgres, which is only citext's type: where it meets a collation
        // of another name, that one wins, as it does against the default)
        (Implicit(x), Implicit(y)) if x == y || y == Coll::DEFAULT => a,
        (Implicit(x), Implicit(_)) if x == Coll::DEFAULT => b,
        (Implicit(_), Implicit(y)) if y.folds() => a,
        (Implicit(x), Implicit(_)) if x.folds() => b,
        (Implicit(x), Implicit(y)) => Conflict(x, y),
    })
}

/// What a function or operator of text gives from arguments in citext: text, which is of the default collation
/// (citext is cast to text unasked; another collation, ICU's, is kept).
fn unfolded(s: Strength) -> Strength {
    match s {
        Strength::Implicit(c) if c.folds() => Strength::Implicit(Coll::DEFAULT),
        s => s,
    }
}

/// Whether `f` is a function of text (not one that unifies its arguments' types, as GREATEST and NULLIF do) that
/// gives text: with citext arguments, text of the default collation.
fn text_function(f: super::funcs::Scalar) -> bool {
    use super::funcs::Scalar::*;
    matches!(
        f,
        Concat
            | ConcatWs
            | Substr
            | Replace
            | Btrim
            | Ltrim
            | Rtrim
            | Left
            | Right
            | Lpad
            | Rpad
            | Repeat
            | Reverse
            | Translate
            | SplitPart
            | Initcap
            | Overlay
            | RegexpReplace
            | Format
    )
}

/// 42P21: two implicit collations where one is needed (ORDER BY, UNION).
pub(super) fn mismatch(s: Strength) -> Result<()> {
    if let Strength::Conflict(x, y) = s {
        return Err(hinted(
            "42P21",
            format!("collation mismatch between implicit collations \"{}\" and \"{}\"", shown(x), shown(y)),
            "You can choose the collation by applying the COLLATE clause to one or both expressions.",
        ));
    }
    Ok(())
}

/// Whether values of `ty` have a collation (text, and the types read as text: varchar, char, name).
pub(super) fn collatable(ty: Type) -> bool {
    ty == Type::Text
}

/// The collation of a bound expression over `cols` (aggregates' results in `aggs`), and 42P21 for two explicit
/// collations meeting anywhere in it. A subquery's comes from `sub`, given its place (None: the default).
pub(super) fn of(e: &Expr, cols: &[Col], aggs: &[Expr], sub: &dyn Fn(&Expr) -> Option<Strength>) -> Result<Strength> {
    let text = |e: &Expr| ty_in(e, cols, &[], aggs).is_ok_and(collatable);
    let all = |es: &mut dyn Iterator<Item = &Expr>| -> Result<Strength> {
        let mut s = Strength::None;
        for x in es {
            s = merge(s, of(x, cols, aggs, sub)?)?;
        }
        Ok(s)
    };
    // a node of text takes its arguments' collation, or the default if none of them has one; a node of another
    // type has none (its arguments' are checked all the same)
    let of_args = |e: &Expr, es: &mut dyn Iterator<Item = &Expr>| -> Result<Strength> {
        let s = all(es)?;
        Ok(match (text(e), s) {
            (false, _) => Strength::None,
            (true, Strength::None) => Strength::Implicit(Coll::DEFAULT),
            (true, s) => s,
        })
    };
    Ok(match e {
        Expr::Func(Func::Collate(c), args) => {
            all(&mut args.iter())?; // (its own argument's explicit collations still clash with each other)
            Strength::Explicit(*c)
        }
        // an outer query's column in a subquery: its collation, implicitly (none where two met)
        Expr::Func(Func::Implicit(c), args) => {
            all(&mut args.iter())?;
            match text(e) && *c != Coll::INDETERMINATE {
                true => Strength::Implicit(*c),
                false => Strength::None,
            }
        }
        // a key and the text it gives back (min, max, GREATEST) have their text's collation
        Expr::Func(
            Func::CollKey(_) | Func::CollEq(_) | Func::NullifIn(_) | Func::CollText | Func::CollateNamed,
            args,
        ) => match args.first() {
            Some(x) => of(x, cols, aggs, sub)?,
            None => Strength::None,
        },
        Expr::Idx(i) => match cols.get(*i) {
            Some(c) if collatable(c.ty) && c.coll == Coll::INDETERMINATE => Strength::None,
            Some(c) if collatable(c.ty) => Strength::Implicit(c.coll),
            _ => Strength::None,
        },
        Expr::Lit(_) | Expr::Param(_) | Expr::Default | Expr::WinRef(..) => match text(e) {
            true => Strength::Implicit(Coll::DEFAULT),
            false => Strength::None,
        },
        Expr::AggRef(k) => match aggs.get(*k) {
            Some(a) => of(a, cols, aggs, sub)?,
            None => Strength::None,
        },
        Expr::Agg(_, arg, _) => match arg.as_deref() {
            Some(x @ Expr::Func(Func::CollKey(_), _)) => of(x, cols, aggs, sub)?,
            // an aggregate of several values (string_agg, array_agg ... ORDER BY): its first is what it gives
            Some(Expr::Func(Func::Row, xs)) => {
                let s = of_args(e, &mut xs.iter().take(1))?;
                // (string_agg(text, text) of citext is text)
                match e {
                    Expr::Agg(super::Agg::List(super::ListAgg::Text, _), ..) => unfolded(s),
                    _ => s,
                }
            }
            Some(x) => of_args(e, &mut std::iter::once(x))?,
            None => Strength::None,
        },
        Expr::Sub(_) => match text(e) {
            true => sub(e).unwrap_or(Strength::Implicit(Coll::DEFAULT)),
            false => Strength::None,
        },
        Expr::Cast(x, ty) => match (collatable(*ty), text(x)) {
            // (citext as text: text(citext) gives the default collation, not citext's)
            (true, true) => match of(x, cols, aggs, sub)? {
                Strength::Implicit(c) if c.folds() => Strength::Implicit(Coll::DEFAULT),
                s => s,
            },
            (true, false) => {
                of(x, cols, aggs, sub)?;
                Strength::Implicit(Coll::DEFAULT)
            }
            _ => {
                of(x, cols, aggs, sub)?;
                Strength::None
            }
        },
        Expr::Case(op, whens, other) => {
            // the result's from its THENs and ELSE; the WHENs are checked
            let mut conds = op.iter().map(|b| &**b).chain(whens.iter().map(|(w, _)| w));
            all(&mut conds)?;
            of_args(e, &mut whens.iter().map(|(_, t)| t).chain(other.iter().map(|b| &**b)))?
        }
        // (a function of text gives text, whatever its arguments are: citext is cast to text unasked, and its result
        // has the default collation; but COALESCE, GREATEST, LEAST and NULLIF unify their arguments' types)
        Expr::Func(f, args) => {
            let s = of_args(e, &mut args.iter())?;
            match f {
                Func::Lower | Func::Upper => unfolded(s),
                Func::Scalar(sf) if text_function(*sf) => unfolded(s),
                _ => s,
            }
        }
        Expr::Call(c) => of_args(e, &mut c.args.iter())?,
        Expr::Neg(x) | Expr::Not(x) | Expr::IsNull(x, _) => of_args(e, &mut std::iter::once(&**x))?,
        // (`||` is textcat(text, text): citext meeting text is text)
        Expr::Bin(a, super::Op::Concat, b) => unfolded(of_args(e, &mut [&**a, &**b].into_iter())?),
        Expr::Bin(a, _, b) | Expr::Like(a, b, ..) => of_args(e, &mut [&**a, &**b].into_iter())?,
        Expr::In(x, list, _) => of_args(e, &mut std::iter::once(&**x).chain(list.iter()))?,
        Expr::InSub(x, ..) => of_args(e, &mut std::iter::once(&**x))?,
        Expr::Col(..) | Expr::Exists(..) | Expr::Window(_) => match text(e) {
            true => Strength::Implicit(Coll::DEFAULT),
            false => Strength::None,
        },
    })
}

/// `e` as the key it sorts by in collation strength `s` (an ORDER BY key, min and max's argument): wrapped in
/// `CollKey` for an ICU collation; 42P21 for a conflict, 42P22 for text with no collation.
pub(super) fn sort_key(e: Expr, s: Strength, is_text: bool) -> Result<Expr> {
    mismatch(s)?;
    if is_text && s == Strength::None {
        return Err(indeterminate());
    }
    let c = s.coll();
    Ok(if is_text && c.keyed() { Expr::Func(Func::CollKey(c), vec![e]) } else { e })
}

/// Whether `e` is an array or a row (`array[x]`, `(x, id)`, `array_agg(x)`), whose order this doesn't carry its
/// text's collation into.
pub(super) fn composite(e: &Expr, cols: &[Col], aggs: &[Expr]) -> bool {
    match e {
        Expr::Func(Func::RowValue | Func::Row | Func::Record(_), _) => true,
        Expr::Agg(super::Agg::List(super::ListAgg::Array, _), ..) => true,
        Expr::AggRef(k) => {
            aggs.get(*k).is_some_and(|a| matches!(a, Expr::Agg(super::Agg::List(super::ListAgg::Array, _), ..)))
        }
        e => matches!(ty_in(e, cols, &[], aggs), Ok(Type::Array(_) | Type::Record(_))),
    }
}

/// 0A000 where an array or a row is ordered (ORDER BY, `<`, min and max, GREATEST) and holds text in an ICU
/// collation (or where two met): they would sort that text by its bytes.
pub(super) fn no_icu_inside(
    e: &Expr,
    cols: &[Col],
    aggs: &[Expr],
    sub: &dyn Fn(&Expr) -> Option<Strength>,
) -> Result<()> {
    if !composite(e, cols, aggs) {
        return Ok(());
    }
    // a text part decides by its collation (`x COLLATE "C"` inside is bytes); another looks at its parts, and an
    // aggregate's argument
    fn inside(e: &Expr, cols: &[Col], aggs: &[Expr], sub: &dyn Fn(&Expr) -> Option<Strength>, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        let deeper = |x: &Expr| inside(x, cols, aggs, sub, depth + 1);
        if let Expr::AggRef(k) = e {
            return aggs.get(*k).is_some_and(deeper);
        }
        if !matches!(e, Expr::Agg(..)) && ty_in(e, cols, &[], aggs).is_ok_and(collatable) {
            return of(e, cols, aggs, sub).map_or(true, |s| matches!(s, Strength::Conflict(..)) || s.coll().keyed());
        }
        match e {
            Expr::Neg(a) | Expr::Not(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) | Expr::Agg(_, Some(a), _) => deeper(a),
            Expr::Bin(a, _, b) | Expr::Like(a, b, ..) => deeper(a) || deeper(b),
            Expr::In(a, l, _) => deeper(a) || l.iter().any(deeper),
            Expr::Func(_, args) => args.iter().any(deeper),
            Expr::Call(c) => c.args.iter().any(deeper),
            Expr::Case(o, ws, el) => {
                o.iter().chain(el.iter()).any(|x| deeper(x)) || ws.iter().any(|(w, t)| deeper(w) || deeper(t))
            }
            Expr::InSub(x, ..) => deeper(x),
            _ => false,
        }
    }
    match inside(e, cols, aggs, sub, 0) {
        true => err(
            UNSUPPORTED,
            "ordering arrays or rows of text in an ICU collation (ORDER BY, comparisons, min and max of them) isn't \
             supported yet: they would sort the text by its bytes; order by the text itself",
        ),
        false => Ok(()),
    }
}

/// 0A000 for `what` (GROUP BY, DISTINCT, LIKE, ...) on text in a nondeterministic collation: it would be done by
/// the text's bytes, where that collation's equal strings differ.
pub(super) fn nd_refused(what: &str) -> SqlError {
    SqlError {
        code: UNSUPPORTED,
        message: format!(
            "{what} of text in a nondeterministic collation isn't supported yet: it would tell apart strings the \
             collation holds equal. Comparisons (=, <, IN), ORDER BY, min, max and unique constraints work"
        ),
        info: None,
    }
}

/// Whether `e` is text in a nondeterministic collation, or an array or a row holding such text.
pub(super) fn nd_in(e: &Expr, cols: &[Col], aggs: &[Expr], sub: &dyn Fn(&Expr) -> Option<Strength>) -> bool {
    fn inside(e: &Expr, cols: &[Col], aggs: &[Expr], sub: &dyn Fn(&Expr) -> Option<Strength>, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        let deeper = |x: &Expr| inside(x, cols, aggs, sub, depth + 1);
        if let Expr::AggRef(k) = e {
            return aggs.get(*k).is_some_and(deeper);
        }
        if !matches!(e, Expr::Agg(..)) && ty_in(e, cols, &[], aggs).is_ok_and(collatable) {
            return of(e, cols, aggs, sub).is_ok_and(|s| s.coll().nondeterministic());
        }
        if !composite(e, cols, aggs) {
            return false;
        }
        match e {
            Expr::Neg(a) | Expr::Not(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) | Expr::Agg(_, Some(a), _) => deeper(a),
            Expr::Bin(a, _, b) | Expr::Like(a, b, ..) => deeper(a) || deeper(b),
            Expr::Func(_, args) => args.iter().any(deeper),
            Expr::Call(c) => c.args.iter().any(deeper),
            Expr::Case(o, ws, el) => {
                o.iter().chain(el.iter()).any(|x| deeper(x)) || ws.iter().any(|(w, t)| deeper(w) || deeper(t))
            }
            _ => false,
        }
    }
    inside(e, cols, aggs, sub, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales_as_postgres_writes_them() {
        for (given, want) in [
            ("de_DE", "de-DE"),
            ("de-DE@collation=phonebook", "de-DE-u-co-phonebk"),
            ("und-u-kn-true", "und-u-kn"),
            ("DE-de", "de-DE"),
            ("", "und"),
            ("root", "und"),
            ("en_US.UTF-8", "en-US"),
            ("sv", "sv"),
            ("en-u-kf-upper", "en-u-kf-upper"),
            ("@@@", "und"),
        ] {
            assert_eq!(canonical(given).unwrap(), want, "{given}");
        }
    }

    #[test]
    fn keys_order_as_the_collation_then_bytes() {
        let de = intern(Def { name: "de-x-icu".into(), builtin: true, locale: Some("de".into()), nd: false });
        let words = ["", "a", "A", "ä", "Ä", "b", "z", "Z"];
        for w in words.windows(2) {
            let (a, b) = (key(de, w[0]).unwrap(), key(de, w[1]).unwrap());
            assert!(a < b, "{:?} < {:?}", w[0], w[1]);
            assert_eq!(compare(de, w[0], w[1]).unwrap(), Ordering::Less);
            assert_eq!(text_of(&a), w[0].as_bytes());
        }
        assert_eq!(key(Coll::DEFAULT, "ä").unwrap(), "ä".as_bytes());
    }
}
