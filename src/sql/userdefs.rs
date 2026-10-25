//! CREATE OPERATOR and CREATE CAST: a symbol or a type pair that stands for a function.
//! Kept as rows under `_sonos_defs/` (so they fork and merge with the data) and read into a registry before a
//! script is parsed (see `hydrate`).

use super::*;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub(super) struct OpDef {
    pub(super) sym: String,
    pub(super) left: Option<Type>,
    pub(super) right: Option<Type>,
    pub(super) func: String,
}

#[derive(Clone, Debug)]
pub(super) struct CastDef {
    pub(super) from: Type,
    pub(super) to: Type,
    /// None: WITHOUT FUNCTION or WITH INOUT: the value is read as the target type
    pub(super) func: Option<String>,
    /// WITHOUT FUNCTION: the value's bits taken as the target type's (where their representations differ: an int4
    /// base type's as a real's), as Postgres's binary-coercible casts take a datum
    pub(super) binary: bool,
}

#[derive(Clone, Debug)]
pub(super) struct AggDef {
    pub(super) name: String,
    pub(super) args: Vec<Type>,
    pub(super) sfunc: String,
    pub(super) stype: Type,
    pub(super) finalfunc: Option<String>,
    pub(super) initcond: Option<String>,
    /// what it gives: the final function's type, or the state's
    #[allow(dead_code)] // (kept as stored; a call's type comes from the function that ends it)
    pub(super) result: Type,
    /// its last argument is VARIADIC: a call gives it any number of arguments from there on
    pub(super) variadic: bool,
    /// its row's key (see `agg_key`)
    pub(super) key: String,
}

/// The base types made with CREATE TYPE name (INPUT = ..., OUTPUT = ...) of a built-in type's I/O functions: each one's
/// name and the type its values are (see `Type::Base`).
static BASES: Mutex<Vec<(String, Type)>> = Mutex::new(vec![]);

/// Base type `name`, its values `repr`'s: made known (or its representation changed, made anew).
pub(super) fn register_base(name: &str, repr: Type) -> Type {
    let mut all = BASES.lock().unwrap();
    let i = match all.iter().position(|(n, _)| n == name) {
        Some(i) => {
            all[i].1 = repr;
            i
        }
        None => {
            all.push((name.to_string(), repr));
            all.len() - 1
        }
    };
    Type::Base(i as u16)
}

/// Base type `i`'s name.
pub(super) fn base_name(i: u16) -> String {
    BASES.lock().unwrap().get(usize::from(i)).map_or_else(|| "unknown".into(), |(n, _)| n.clone())
}

/// The type base type `i`'s values are.
pub(super) fn base_repr(i: u16) -> Type {
    BASES.lock().unwrap().get(usize::from(i)).map_or(Type::Text, |(_, t)| *t)
}

/// The base type called `name`, if there is one.
pub(super) fn base_named(name: &str) -> Option<Type> {
    BASES.lock().unwrap().iter().position(|(n, _)| n == name).map(|i| Type::Base(i as u16))
}

/// Base type `name`, dropped: its name no longer finds it (its number keeps its place, as the others are known by
/// theirs).
pub(super) fn forget_base(name: &str) {
    BASES.lock().unwrap().iter_mut().filter(|(n, _)| n == name).for_each(|(n, _)| n.clear());
}

/// The casts made from or to `t` (each as its from and to types), in the order they were made.
pub(super) fn casts_of(t: Type) -> Vec<(Type, Type)> {
    DEFS.lock().unwrap().casts.iter().filter(|c| c.from == t || c.to == t).map(|c| (c.from, c.to)).collect()
}

/// A composite type's row as `Composites` keeps it: its name, its fields (name, type as written), and whether it is one
/// of no attributes that is no shell type (made `AS ()`, or ALTER TYPE dropped the last; see `sql::shell_row`).
type Row = (String, Vec<(String, String)>, bool);

/// A world's composite types' rows as they are stored, each one's key and text, in key order: two states with the same
/// texts have the same types, which tells them apart without the rows read as JSON (see `read_in`).
type Texts = Vec<(String, Val)>;

/// Composite types (made with CREATE TYPE name AS (...)) as the process knows them from a world's rows: the rows, by
/// name, and each one's record shape (its own: see `Type::composite_record`), by which a schema or a function read with
/// an older one is told (see `sql::stale_record`). A row with a field of a type that isn't there has no shape. Shared
/// by the worlds whose rows are the same (a fork's and a transaction's are mostly their parent's), until one's change.
#[derive(Clone, Default)]
struct Types {
    rows: Vec<Row>,
    shapes: Vec<(String, Type)>,
    /// the texts of the rows they were read from (see `Texts`); None once changed otherwise (see `Composites::set`)
    texts: Option<Texts>,
}

/// A world's composite types (see `Types`), and how they were read in.
#[derive(Clone, Default)]
struct Composites {
    types: Arc<Types>,
    /// how many times they were changed: rows read before a change made since don't undo it (see `read_in`)
    changes: u64,
    /// the state of the world they were read from, or last found to match the rows of: a view of that very state has
    /// them without its rows read again (see `read_in`, `as_of`). None once a statement changed them (see `define`).
    at: Option<crate::Stamp>,
}

impl Composites {
    fn of(types: Types, at: Option<crate::Stamp>) -> Composites {
        Composites { types: Arc::new(types), changes: 0, at }
    }

    fn shape(&self, name: &str) -> Option<Type> {
        self.types.shapes.iter().find(|(n, _)| n == name).map(|(_, t)| *t)
    }

    fn row(&self, name: &str) -> Option<&Row> {
        self.types.rows.iter().find(|r| r.0 == name)
    }

    /// Type `name`'s row made `row` (None: gone), and its shape `shape` (these types alone: see `Types`).
    fn set(&mut self, name: &str, row: Option<Row>, shape: Option<Type>) {
        let types = Arc::make_mut(&mut self.types);
        types.rows.retain(|r| r.0 != name);
        types.rows.extend(row);
        types.rows.sort();
        types.shapes.retain(|(n, _)| n != name);
        types.shapes.extend(shape.map(|t| (name.to_string(), t)));
        types.texts = None;
    }

    /// `other`'s types put in place of these of their names, `only_new`: only those of names these have none of.
    fn take(&mut self, other: &Composites, only_new: bool) {
        for r in &other.types.rows {
            if !(only_new && self.row(&r.0).is_some()) {
                self.set(&r.0, Some(r.clone()), other.shape(&r.0));
            }
        }
    }
}

/// Each world's composite types, by its database (`Core::uid`) and its name (see `key_of`): a world reads its own, so
/// an ALTER TYPE in a fork changes the type there only. A world's are read in from its rows when first looked up
/// (`fill`), again before a script or a statement is parsed where its state moved on since (`hydrate_composites`),
/// and where a statement finds its row changed (`sync_composite`); they are forgotten with the world
/// (`forget_worlds`).
static WORLDS: LazyLock<Mutex<HashMap<u64, HashMap<String, Composites>>>> = LazyLock::new(Default::default);

/// What any world made known last of each name: what lookups outside any world find (a row shown as JSON by the
/// key-value API), as the process knew composite types before each world had its own; and the types it took last,
/// which are not taken again (most worlds share their parent's: see `known_anywhere`).
static ANY: LazyLock<Mutex<(Composites, Option<Arc<Types>>)>> = LazyLock::new(Default::default);

thread_local! {
    /// The world `in_world` reads a view of (its database's uid and key, see `key_of`): lookups find its composite
    /// types rather than the running session's.
    static READING: std::cell::RefCell<Option<(u64, String)>> = const { std::cell::RefCell::new(None) };
    /// The composite types of the view as it was that `in_world` reads (see `Held`): lookups find these first.
    static HELD: std::cell::RefCell<Option<Held>> = const { std::cell::RefCell::new(None) };
    /// The composite types `settle` is working out: lookups find these meanwhile (they are no world's yet).
    static BUILDING: std::cell::RefCell<Option<Composites>> = const { std::cell::RefCell::new(None) };
    /// The shapes `composite` gave while `noting`.
    static NOTED: std::cell::RefCell<Option<Vec<Type>>> = const { std::cell::RefCell::new(None) };
}

/// The composite types of a view `in_world` reads that is `as_it_was`: a merge's result, the world it goes into as the
/// merge found it, a world as it was. They are those of its own state's rows (see `as_of`), held for that `in_world`
/// alone, so that no session meanwhile, of its world or of another, changes them under it; found the first time they
/// are looked up, as most of what reads such a view (a merge of rows of no composite type) needs none.
struct Held {
    uid: u64,
    /// its database, to read the view's rows by (none while the log is replayed: its types are then found at once)
    core: std::sync::Weak<crate::Core>,
    born: u64,
    s: crate::State,
    branch: String,
    types: Option<Composites>,
}

/// Puts a thread-local back as it was once dropped, on a panic too (a pooled helper thread runs on).
struct Restore<T: 'static>(&'static std::thread::LocalKey<std::cell::RefCell<Option<T>>>, Option<T>);

impl<T: 'static> Drop for Restore<T> {
    fn drop(&mut self) {
        self.0.set(self.1.take());
    }
}

/// Whether `v` is of a world as it was (`name@when`, or as a merge would leave it) or of a state of no world (a merge's
/// result, or the world it goes into as the merge found it: see `Core::merge`): its composite types are those of its
/// own rows (see `as_of`), which may be others than its world has now.
fn as_it_was(v: &View) -> bool {
    v.born == crate::PAST || v.branch.is_empty()
}

/// What `v`'s world's composite types are kept by: its database's uid and its name; `name@` for a view `as_it_was`
/// (`@` for one of no world), whose types are those of the state it read last (see `as_of`).
fn key_of(v: &View) -> (u64, String) {
    let name = if as_it_was(v) { format!("{}@", v.branch) } else { v.branch.clone() };
    (v.core.uid, name)
}

/// `f` of world `key`'s composite types, where they have been read in.
fn kept<T>(key: &(u64, String), f: impl FnOnce(&Composites) -> T) -> Option<T> {
    WORLDS.lock().unwrap().get(&key.0).and_then(|w| w.get(&key.1)).map(f)
}

/// Whether `v` is of the running session's temporary tables' database, which has no types of its own: the types of
/// its tables' columns are the session's world's (see `catalog::at_home`).
fn temporary(v: &View) -> bool {
    TEMP.with_borrow(|t| t.as_ref().is_some_and(|(db, _)| std::ptr::eq(v.core, &*db.core)))
}

/// `key_of` a session's world, by its name.
fn session_key(core: &crate::Core, world: &str) -> (u64, String) {
    let name = world.split_once('@').map_or_else(|| world.to_string(), |(n, _)| format!("{n}@"));
    (core.uid, name)
}

/// The world lookups find composite types in now: `in_world`'s, else the running session's; None outside both.
fn reading() -> Option<(u64, String)> {
    READING
        .with_borrow(Clone::clone)
        .or_else(|| SESSION.with_borrow(|s| s.as_ref().map(|(core, w)| session_key(core, w))))
}

/// `f` of the composite types lookups find now: `settle`'s while it works; those `in_world` holds for a view as it
/// was; else the world's, read in from its rows the first time they're looked up; else (outside any world) what any
/// world made known.
fn looked_up<T>(f: impl Fn(&Composites) -> T) -> T {
    if let Some(t) = BUILDING.with_borrow(|b| b.as_ref().map(&f)) {
        return t;
    }
    if let Some(c) = held() {
        return f(&c);
    }
    let Some(key) = reading() else { return f(&ANY.lock().unwrap().0) };
    if !key.1.ends_with('@')
        && let Some(t) = kept(&key, &f)
    {
        return t;
    }
    // (the session's world, read in as its database has it, or a world it reads as it was, as that state has it:
    // `in_world`'s was read in as it began)
    let session = SESSION.with_borrow(Clone::clone).filter(|(core, w)| session_key(core, w) == key);
    match session.as_ref().and_then(|(core, w)| core.view_tx(w).ok()) {
        Some(v) if as_it_was(&v) => f(&as_of(&v)),
        Some(v) => f(&fill(&v)),
        None => f(&ANY.lock().unwrap().0),
    }
}

/// The composite types `in_world` holds now for a view as it was (see `Held`), found the first time they're looked up.
fn held() -> Option<Composites> {
    let todo = HELD.with_borrow(|h| {
        let h = h.as_ref()?;
        Some(h.types.clone().ok_or_else(|| (h.core.clone(), h.born, h.s.clone(), h.branch.clone())))
    })?;
    let (core, born, s, branch) = match todo {
        Ok(types) => return Some(types),
        Err(view) => view,
    };
    // (its database is open while a view of it is read)
    let types = core.upgrade().map(|core| as_of(&View { core: &core, born, s, branch, reads: None }));
    let types = types.unwrap_or_default();
    HELD.with_borrow_mut(|h| {
        if let Some(h) = h {
            h.types = Some(types.clone());
        }
    });
    Some(types)
}

/// The composite types' rows of `v` as they are stored (see `Texts`).
fn texts_of(v: &View) -> Texts {
    let Ok(rows) = v.prefix("_sonos_types/") else { return vec![] };
    rows.flatten().filter(|(_, raw)| raw.contains("\"composite\"")).collect()
}

/// The composite types' rows `texts` (see `texts_of`), by name.
fn rows_of(texts: &Texts) -> Vec<Row> {
    let mut out: Vec<Row> = (texts.iter())
        .filter_map(|(k, raw)| {
            let j: Json = serde_json::from_str(raw).ok()?;
            let fields = fields_of(&j)?;
            let empty = fields.is_empty() && !shell_row(&j);
            Some((k["_sonos_types/".len()..].to_string(), fields, empty))
        })
        .collect();
    out.sort();
    out
}

/// A type row's composite fields (name, type as written); None for another kind of type's row.
fn fields_of(j: &Json) -> Option<Vec<(String, String)>> {
    let fields = j["composite"].as_array()?;
    Some(fields.iter().filter_map(|f| Some((f[0].as_str()?.to_string(), f[1].as_str()?.to_string()))).collect())
}

/// The composite types of `rows`, each a record of its fields read as types, worked out in passes: one with an
/// attribute of another is made once that one is, with its shape (so one with an attribute of a type ALTER TYPE
/// changed takes the change). A row with a field of a type that isn't there has none.
fn settle(rows: &[Row]) -> Types {
    let building = Composites::of(Types { rows: rows.to_vec(), ..Default::default() }, None);
    let was = Restore(&BUILDING, BUILDING.replace(Some(building)));
    for _ in 0..=rows.len() {
        let mut changed = false;
        for (name, fields, _) in rows {
            let Some(t) = shape_of(name, fields) else { continue };
            BUILDING.with_borrow_mut(|b| {
                if let Some(b) = b
                    && b.shape(name) != Some(t)
                {
                    b.set(name, b.row(name).cloned(), Some(t));
                    changed = true;
                }
            });
        }
        if !changed {
            break;
        }
    }
    let made = BUILDING.take().unwrap_or_default();
    drop(was);
    Arc::unwrap_or_clone(made.types)
}

/// The composite types of `v`'s rows (`texts`, see `texts_of`): those of another world of its database whose rows are
/// the same (a fork's, or a transaction's, are mostly its parent's, and a merge's result the world's it goes into),
/// rather than worked out again.
fn made_of(v: &View, texts: Texts) -> Composites {
    let found = |same: &dyn Fn(&Types) -> bool| {
        let all = WORLDS.lock().unwrap();
        all.get(&v.core.uid).and_then(|w| w.values().find(|c| same(&c.types)).map(|c| c.types.clone()))
    };
    let at = Some(v.s.stamp());
    let same = found(&|t| t.texts.as_ref() == Some(&texts));
    if let Some(types) = same {
        return Composites { types, changes: 0, at };
    }
    let rows = rows_of(&texts);
    let same = found(&|t| t.rows == rows);
    let types = match same {
        Some(t) => Types { texts: Some(texts), ..(*t).clone() },
        None => Types { texts: Some(texts), ..settle(&rows) },
    };
    Composites::of(types, at)
}

/// Makes composite types `c` known to lookups outside any world (see `ANY`), unless they're the ones it took last.
fn known_anywhere(c: &Composites) {
    let mut any = ANY.lock().unwrap();
    if any.1.as_ref().is_some_and(|t| Arc::ptr_eq(t, &c.types)) {
        return;
    }
    any.0.take(c, false);
    any.1 = Some(c.types.clone());
}

/// `v`'s world's composite types read in from its rows, and kept for its lookups (unless another thread kept them
/// first: those are given).
fn fill(v: &View) -> Composites {
    let key = key_of(v);
    let made = made_of(v, texts_of(v));
    known_anywhere(&made);
    let mut all = WORLDS.lock().unwrap();
    all.entry(key.0).or_default().entry(key.1).or_insert(made).clone()
}

/// The composite types of `v`, a view `as_it_was`: those kept for its key where they were read from its very state,
/// else read from its rows and kept for the next view of that state (a merge reads its result through several, a
/// merge by columns one for each row).
fn as_of(v: &View) -> Composites {
    let key = key_of(v);
    let found = kept(&key, Clone::clone);
    if let Some(c) = found.filter(|c| c.at.as_ref().is_some_and(|s| v.s.is(s))) {
        return c;
    }
    let made = made_of(v, texts_of(v));
    WORLDS.lock().unwrap().entry(key.0).or_default().insert(key.1, made.clone());
    made
}

/// `v`'s world's composite types made what its rows have, where they differ (read in anew, those whose rows are gone
/// forgotten): unless they were changed since they had made `before` changes, as one may be that `v` is too old to
/// have the row of (made meanwhile, by another session); then only the types of names they have none of are taken.
/// Whether they matched `v`'s rows (were changed meanwhile, or now are). Nothing is read where they were read from
/// `v`'s very state, or found to match it, before; nor read as JSON where they were from the same texts.
fn read_in(v: &View, before: Option<u64>) -> bool {
    let key = key_of(v);
    if kept(&key, |c| c.at.as_ref().is_some_and(|s| v.s.is(s))) == Some(true) {
        return true;
    }
    let texts = texts_of(v);
    match matched(&key, v, &texts, |t| t.texts.as_ref() == Some(&texts)) {
        Some(true) => return true,
        // (a world's first: nothing was read with them yet, so nothing parsed need be parsed again)
        None => {
            let made = made_of(v, texts);
            known_anywhere(&made);
            WORLDS.lock().unwrap().entry(key.0).or_default().entry(key.1).or_insert(made);
            return true;
        }
        Some(false) => {}
    }
    // (or the same rows: a statement changed them since they were read)
    let rows = rows_of(&texts);
    if matched(&key, v, &texts, |t| t.rows == rows) == Some(true) {
        return true;
    }
    let made = Composites::of(Types { texts: Some(texts), ..settle(&rows) }, Some(v.s.stamp()));
    known_anywhere(&made);
    {
        let mut all = WORLDS.lock().unwrap();
        let world = all.entry(key.0).or_default();
        match world.get_mut(&key.1) {
            Some(c) if Some(c.changes) == before => *c = Composites { changes: c.changes + 1, ..made },
            Some(c) => {
                c.take(&made, true);
                c.changes += 1;
                c.at = None; // (what no one state has)
                return false;
            }
            None => {
                world.insert(key.1, made);
            }
        }
    }
    composites_changed();
    true
}

/// Whether world `key`'s composite types are those of `v`'s rows `texts`, as `same` tells (None where none are kept):
/// those that are are marked as read from `v`'s state.
fn matched(key: &(u64, String), v: &View, texts: &Texts, same: impl Fn(&Types) -> bool) -> Option<bool> {
    let mut all = WORLDS.lock().unwrap();
    let c = all.get_mut(&key.0).and_then(|w| w.get_mut(&key.1))?;
    if !same(&c.types) {
        return Some(false);
    }
    c.at = Some(v.s.stamp());
    if c.types.texts.as_ref() != Some(texts) {
        Arc::make_mut(&mut c.types).texts = Some(texts.clone());
    }
    Some(true)
}

/// Reads the running session's world's composite types in again where its rows changed (another session's ALTER
/// TYPE or DROP TYPE, a merge, a rebase), before a script or a statement of it is parsed: unless its state is the one
/// they were read from, or last found to match, which reads no row. (A world the session reads as it was has its
/// state's own: see `as_of`.)
pub(super) fn hydrate_composites() {
    let Some(key) = reading().filter(|k| !k.1.ends_with('@')) else { return };
    // (tried again where the types changed while the rows were read: a statement of another session made one)
    for _ in 0..3 {
        let before = kept(&key, |c| c.changes);
        if with_path_view(|v| read_in(v, before)) != Some(false) {
            return;
        }
    }
}

/// World `world` of `core`'s composite types read in again from its rows as they are now, where the process has read
/// them in: its rows were changed other than by its own sessions' DDL (a merge into it, a rebase, a write through the
/// API, RESTORE, UNDO MERGE), which lookups that go by what was read in would otherwise miss until a session of it
/// next parses a statement. Tried again where they changed while the rows were read, as in `hydrate_composites`.
pub(super) fn reread(core: &crate::Core, world: &str) {
    let key = (core.uid, world.to_string());
    for _ in 0..3 {
        let Some(before) = kept(&key, |c| c.changes) else { return };
        let Ok(v) = core.view_tx(world) else { return };
        if read_in(&v, Some(before)) {
            return;
        }
    }
}

/// World `child` of `core` merged whole into `parent` (a transaction's commit, a savepoint's release, MERGE WORLD):
/// its composite types are forgotten with it, and the parent's read in again (see `reread`) where the merge may have
/// changed them. A transaction's DDL of a type reads its types in (see `sync_composite`), so one whose are not known
/// changed none; a world's own may have, before the database was opened.
pub(super) fn merged(core: &crate::Core, child: &str, parent: Option<&str>) {
    let (mine, same) = {
        let mut all = WORLDS.lock().unwrap();
        let Some(w) = all.get_mut(&core.uid) else { return };
        w.remove(&format!("{child}@"));
        let mine = w.remove(child);
        // (whether the child's are the parent's, where the parent's are known)
        let same = parent
            .and_then(|p| w.get(p))
            .map(|p| mine.as_ref().is_some_and(|c| Arc::ptr_eq(&c.types, &p.types) || c.types.rows == p.types.rows));
        (mine.is_some(), same)
    };
    let Some(parent) = parent else { return };
    match (mine, same) {
        (true, Some(false)) => reread(core, parent), // (an ALTER TYPE there, say)
        // (the parent's not read in yet, a transaction's under a savepoint released: read in now, as the merge left
        // its rows, so that its own merge tells whether they changed)
        (true, None) => {
            if let Ok(v) = core.view_tx(parent) {
                fill(&v);
            }
        }
        (false, Some(_)) if !child.starts_with(TX_PREFIX) => reread(core, parent),
        _ => {}
    }
}

/// `f`, with composite types looked up in `v`'s world: read in from its rows first if they aren't yet. A view
/// `as_it_was` (a merge's, one of a world as it was) has those of its own state's rows, held for `f` (see `Held`), and
/// so do the views of that very state `f` reads through. A table's schema read from `v`.
pub(super) fn in_world<T>(v: &View, f: impl FnOnce() -> T) -> T {
    if temporary(v) {
        return f();
    }
    if as_it_was(v) {
        if HELD.with_borrow(|h| h.as_ref().is_some_and(|h| h.uid == v.core.uid && h.s.same(&v.s))) {
            return f();
        }
        let core = v.core.me.get().cloned();
        let types = core.is_none().then(|| as_of(v)); // (no `Db` of it yet: the log being replayed)
        let held = Held {
            uid: v.core.uid,
            core: core.unwrap_or_default(),
            born: v.born,
            s: v.s.clone(),
            branch: v.branch.clone(),
            types,
        };
        let _was = Restore(&HELD, HELD.replace(Some(held)));
        return f();
    }
    let key = key_of(v);
    if kept(&key, |_| ()).is_none() {
        fill(v);
    }
    // (a world's view read inside one as it was: the world's types, not those held)
    let _held = Restore(&HELD, HELD.take());
    let _was = Restore(&READING, READING.replace(Some(key)));
    f()
}

/// Composite type `name`'s record shape, as the world lookups read has it (see `looked_up`).
pub(super) fn composite(name: &str) -> Option<Type> {
    let t = current(name)?;
    note(t);
    Some(t)
}

/// `composite`, without noting it (see `noting`).
pub(super) fn current(name: &str) -> Option<Type> {
    looked_up(|c| c.shape(name))
}

/// Notes shape `t` for `noting` (not one `settle` is working out).
fn note(t: Type) {
    if BUILDING.with_borrow(Option::is_some) {
        return;
    }
    NOTED.with_borrow_mut(|n| {
        if let Some(n) = n
            && !n.contains(&t)
        {
            n.push(t);
        }
    });
}

/// `f`, and the composite types' shapes it found by name (see `composite`): what a function is read with, so it is
/// read again once one is no longer its type's (see `functions::from_raw`).
pub(super) fn noting<T>(f: impl FnOnce() -> T) -> (T, Vec<Type>) {
    let was = Restore(&NOTED, NOTED.replace(Some(vec![])));
    let out = f();
    let noted = NOTED.take().unwrap_or_default();
    drop(was);
    (out, noted)
}

/// Whether `t`, a composite type's shape, is no longer the shape `v`'s world has the type in (see
/// `sql::stale_record`).
pub(super) fn stale_in(v: &View, t: Type) -> bool {
    let Some(n) = t.composite_of() else { return false };
    if temporary(v) {
        return stale_record(t);
    }
    if as_it_was(v) {
        return in_world(v, || current(&n)) != Some(t);
    }
    let now = kept(&key_of(v), |c| c.shape(&n));
    now.unwrap_or_else(|| fill(v).shape(&n)) != Some(t)
}

/// The composite type `t` is a shape of, by name: the type it was made for, where the world lookups read has one of
/// that name (whatever its shape now: `t` may be its shape before an ALTER TYPE, or another world's).
pub(super) fn composite_name(t: Type) -> Option<String> {
    let n = t.composite_of()?;
    looked_up(|c| c.shape(&n).is_some()).then(|| n.to_string())
}

/// Composite type `name`'s record type of these fields (read as types), if they all resolve. (Made, not known: see
/// `sync_composite`.)
pub(super) fn shape_of(name: &str, fields: &[(String, String)]) -> Option<Type> {
    let mut fs = vec![];
    for (n, t) in fields {
        let read = || Parser::new(lex(&format!("x {t}")).ok()?).column().ok().map(|c| c.ty);
        fs.push((n.clone(), Type::from_name(t).or_else(|| composite(t)).or_else(read)?));
    }
    Some(Type::composite_record(name, fs))
}

/// Composite type `name` known in `v`'s world as its type row `row` there has it now (forgotten where there is none,
/// or one of another kind of type): what a statement that reads the row does, which may have changed since the world's
/// types were read in (by an ALTER TYPE, here or in a session of the world; a merge). The types with an attribute of
/// it are made again with its new shape. Its shape now, if its fields resolve. (A view `as_it_was` has its rows'
/// types, `row` among them, which nothing reading it changes.)
pub(super) fn sync_composite(v: &View, name: &str, row: Option<&Json>) -> Option<Type> {
    let now = row.and_then(|j| {
        let fields = fields_of(j)?;
        let empty = fields.is_empty() && !shell_row(j);
        Some((name.to_string(), fields, empty))
    });
    if temporary(v) {
        return current(name);
    }
    if as_it_was(v) {
        return in_world(v, || composite(name));
    }
    let key = key_of(v);
    let found = kept(&key, |c| (c.row(name).cloned(), c.shape(name)));
    let (was, shape) = found.unwrap_or_else(|| {
        let c = fill(v);
        (c.row(name).cloned(), c.shape(name))
    });
    let t = if was == now { shape } else { define(&key, name, now) };
    if let Some(t) = t {
        note(t);
    }
    t
}

/// Composite type `name` known in `v`'s world no longer: it was dropped, or another type is made by its name.
pub(super) fn forget_composite(v: &View, name: &str) {
    sync_composite(v, name, None);
}

/// Composite type `name`'s row in world `key` made `now` (None: gone), and the world's shapes worked out again from
/// its rows: those that changed (its own, those with an attribute of it) are put in place, as the others may have
/// been changed meanwhile. Its shape now.
fn define(key: &(u64, String), name: &str, now: Option<Row>) -> Option<Type> {
    let base = kept(key, Clone::clone).unwrap_or_default();
    let mut rows = base.types.rows.clone();
    rows.retain(|r| r.0 != name);
    rows.extend(now.clone());
    rows.sort();
    let made = Composites::of(settle(&rows), None);
    let t = made.shape(name);
    let changed: Vec<(&str, Option<Row>, Type)> = (made.types.shapes.iter())
        .filter(|(n, s)| n != name && base.shape(n) != Some(*s))
        .map(|(n, s)| (n.as_str(), made.row(n).cloned(), *s))
        .collect();
    let apply = |c: &mut Composites| {
        c.set(name, now.clone(), t);
        for (n, row, s) in &changed {
            c.set(n, row.clone(), Some(*s));
        }
    };
    {
        let mut all = WORLDS.lock().unwrap();
        let c = all.entry(key.0).or_default().entry(key.1.clone()).or_default();
        apply(c);
        c.changes += 1;
        c.at = None; // (no longer just what a state of the world has: read again from the next)
    }
    apply(&mut ANY.lock().unwrap().0);
    composites_changed();
    t
}

/// Forgets the composite types of database `uid`'s worlds that were dropped or merged (and theirs as they were): a
/// world made again by one of their names reads its own in.
pub(super) fn forget_worlds(uid: u64, worlds: &[&str]) {
    if let Some(w) = WORLDS.lock().unwrap().get_mut(&uid) {
        for n in worlds {
            w.remove(*n);
            w.remove(&format!("{n}@"));
        }
    }
}

/// Forgets the composite types of database `uid`'s worlds: it closed.
pub(super) fn forget_database(uid: u64) {
    WORLDS.lock().unwrap().remove(&uid);
}

/// Whether `name` is a shell type (`CREATE TYPE name`): a composite of no fields yet (not one of none, see `Row`).
pub(super) fn is_shell(name: &str) -> bool {
    looked_up(|c| c.shape(name).is_some() && c.row(name).is_some_and(|(_, f, empty)| f.is_empty() && !empty))
}

#[derive(Default)]
struct Defs {
    ops: Vec<OpDef>,
    casts: Vec<CastDef>,
    aggs: Vec<AggDef>,
}

static DEFS: Mutex<Defs> = Mutex::new(Defs { ops: vec![], casts: vec![], aggs: vec![] });

/// A user aggregate called `name`: its place in the registry and its number of arguments.
pub(super) fn aggregate(name: &str) -> Option<(u8, usize)> {
    let d = DEFS.lock().unwrap();
    d.aggs.iter().position(|a| a.name == name).map(|i| (i as u8, d.aggs[i].args.len()))
}

/// The user aggregate called `name` that a call with `n` arguments means, as `aggregate` gives it: the one taking
/// that many, else a VARIADIC one taking at least the ones before its last (its number of arguments then `n`).
pub(super) fn aggregate_for(name: &str, n: usize) -> Option<(u8, usize)> {
    let d = DEFS.lock().unwrap();
    let mut variadic = None;
    for (i, a) in d.aggs.iter().enumerate() {
        if a.name != name {
            continue;
        }
        if a.args.len() == n {
            return Some((i as u8, n));
        }
        if a.variadic && n + 1 >= a.args.len() && variadic.is_none() {
            variadic = Some((i as u8, n));
        }
    }
    variadic
}

pub(super) fn aggregate_at(i: u8) -> Option<AggDef> {
    DEFS.lock().unwrap().aggs.get(i as usize).cloned()
}

const PREFIX: &str = "_sonos_defs/";

/// Whether CREATE OPERATOR CLASS made class `name` of access method `am` (kept as its name: see `named_stmt`).
pub(super) fn opclass_made(v: &View, name: &str, am: &str) -> Result<bool> {
    Ok(v.get(&format!("{PREFIX}opclass/{name}/{am}"))?.is_some())
}

/// The registered operators' symbols, longest first, for the lexer.
pub(super) fn symbols() -> Vec<String> {
    let mut v: Vec<String> = DEFS.lock().unwrap().ops.iter().map(|o| o.sym.clone()).collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.len()));
    v.dedup();
    v
}

/// A registered operator with this symbol and arity.
pub(super) fn operator(sym: &str, binary: bool) -> Option<OpDef> {
    DEFS.lock().unwrap().ops.iter().find(|o| o.sym == sym && (o.left.is_some() == binary)).cloned()
}

/// Placeholder call name for an operator symbol several definitions share: the binder picks one by argument types.
const OP_CALL: &str = "\u{1}op:";

/// The comparison operators Chronos has of its own, which a user's operator of the symbol shares.
const COMPARISONS: [(&str, Op); 7] =
    [("=", Op::Eq), ("<>", Op::Ne), ("!=", Op::Ne), ("<", Op::Lt), (">", Op::Gt), ("<=", Op::Le), (">=", Op::Ge)];

/// For a placeholder call of a user's comparison operator (`=`, `<`, ...) with operands of these types: the
/// built-in comparison when no definition of it takes them. Postgres picks a user's `=` only for operands of its
/// types, so `oid = oid` stays the built-in one after `CREATE OPERATOR alter1.= (leftarg = alter1.ctype, ...)`.
/// An operand whose type can't be told (a null, a quoted literal) fits any definition.
pub(super) fn builtin_comparison(name: &str, tys: &[Option<Type>]) -> Option<Op> {
    let (sym, bin) = name.strip_prefix(OP_CALL)?.rsplit_once(':')?;
    if bin != "1" || tys.len() != 2 {
        return None;
    }
    let op = COMPARISONS.iter().find(|(s, _)| *s == sym)?.1;
    let d = DEFS.lock().unwrap();
    let fits = |want: &Option<Type>, got: &Option<Type>| got.is_none() || want == got;
    let takes = d
        .ops
        .iter()
        .filter(|o| o.sym == sym && o.left.is_some())
        .any(|o| fits(&o.left, &tys[0]) && fits(&o.right, &tys[1]));
    (!takes).then_some(op)
}

/// The call of operator `sym` with these arguments: its function's name, or the placeholder when several
/// definitions of the symbol and arity differ in their functions (or the symbol is a built-in comparison's
/// too: see `builtin_comparison`).
fn op_call_name(sym: &str, binary: bool, def: &OpDef) -> String {
    if binary && COMPARISONS.iter().any(|(s, _)| *s == sym) {
        return format!("{OP_CALL}{sym}:1");
    }
    let d = DEFS.lock().unwrap();
    let mut same = d.ops.iter().filter(|o| o.sym == sym && o.left.is_some() == binary);
    if same.all(|o| o.func == def.func) {
        func_named(&def.func)
    } else {
        format!("{OP_CALL}{sym}:{}", u8::from(binary))
    }
}

/// The functions a placeholder call may stand for, every one of the operators sharing its symbol; None for a call
/// that isn't one.
pub(super) fn op_funcs(name: &str) -> Option<Vec<String>> {
    let (sym, bin) = name.strip_prefix(OP_CALL)?.rsplit_once(':')?;
    let d = DEFS.lock().unwrap();
    Some(d.ops.iter().filter(|o| o.sym == sym && o.left.is_some() == (bin == "1")).map(|o| o.func.clone()).collect())
}

/// The function a placeholder call stands for, given its arguments' types: the operator whose declared types match
/// (a null or unknown argument matches any), else the first.
pub(super) fn resolve_op_call(name: &str, tys: &[Option<Type>]) -> Option<String> {
    let rest = name.strip_prefix(OP_CALL)?;
    let (sym, bin) = rest.rsplit_once(':')?;
    let binary = bin == "1";
    let d = DEFS.lock().unwrap();
    let cands: Vec<&OpDef> = d.ops.iter().filter(|o| o.sym == sym && o.left.is_some() == binary).collect();
    let fits = |want: &Option<Type>, got: Option<&Option<Type>>| match (want, got) {
        (Some(w), Some(Some(g))) => w == g,
        _ => true,
    };
    let chosen = cands
        .iter()
        .find(|o| {
            if binary { fits(&o.left, tys.first()) && fits(&o.right, tys.get(1)) } else { fits(&o.right, tys.first()) }
        })
        .or(cands.first())?;
    Some(func_named(&chosen.func))
}

/// The function that casts `from` to `to`, if one was made.
pub(super) fn cast(from: Type, to: Type) -> Option<CastDef> {
    DEFS.lock().unwrap().casts.iter().find(|c| c.from == from && c.to == to).cloned()
}

fn tname(t: &Option<Type>) -> String {
    t.map_or("none".into(), |t| t.name().into())
}

pub(super) fn op_key(sym: &str, l: &Option<Type>, r: &Option<Type>) -> String {
    format!("{PREFIX}op/{sym}/{},{}", tname(l), tname(r))
}

pub(super) fn cast_key(from: Type, to: Type) -> String {
    format!("{PREFIX}cast/{},{}", from.name(), to.name())
}

/// Reads the stored definitions into the registry (a restart, or another database, forgets them).
pub(super) fn hydrate() {
    let Some(defs) = with_path_view(|v| {
        let mut d = Defs::default();
        let Ok(rows) = v.prefix(PREFIX) else { return d };
        for (k, raw) in rows.flatten() {
            let j: Json = serde_json::from_str(&raw).unwrap_or_default();
            let ty = |x: &Json| x.as_str().and_then(Type::from_name);
            if let Some(rest) = k.strip_prefix(&format!("{PREFIX}rule/")) {
                if let Some((table, _)) = rest.rsplit_once('/') {
                    super::rules::note(table);
                }
                continue;
            }
            if let Some(name) = k.strip_prefix(&format!("{PREFIX}tsdictionary/")) {
                let (Some(t), Some(o)) = (j["template"].as_str(), j["options"].as_object()) else { continue };
                if let Ok(d) = ts_dict_of(t, o) {
                    super::tsearch::register_dict(name, d);
                }
                continue;
            }
            if let Some(name) = k.strip_prefix(&format!("{PREFIX}tsconfiguration/")) {
                let maps = j["maps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| {
                        Some((
                            m[0].as_str()?.to_string(),
                            m[1].as_array()?.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                        ))
                    })
                    .collect();
                super::tsearch::register_config(super::tsearch::UserConfig { name: name.to_string(), maps });
                continue;
            }
            if k.starts_with(&format!("{PREFIX}agg/")) {
                let (Some(name), Some(stype)) = (j["name"].as_str(), ty(&j["stype"])) else { continue };
                let finalfunc = j["finalfunc"].as_str().map(String::from);
                let result = finalfunc
                    .as_deref()
                    .and_then(|f| functions::named(v, f).ok())
                    .and_then(|fs| fs.first().map(|f| f.value_type()))
                    .unwrap_or(stype);
                d.aggs.push(AggDef {
                    name: name.into(),
                    // (a type of no built-in name, "any", stands as text: it takes the place of an argument)
                    args: j["args"].as_array().into_iter().flatten().map(|x| ty(x).unwrap_or(Type::Text)).collect(),
                    sfunc: j["sfunc"].as_str().unwrap_or_default().into(),
                    stype,
                    finalfunc,
                    initcond: j["initcond"].as_str().map(String::from),
                    result,
                    variadic: j["variadic"] == true,
                    key: k.clone(),
                });
            } else if k.starts_with(&format!("{PREFIX}op/")) {
                if let Some(sym) = j["sym"].as_str() {
                    d.ops.push(OpDef {
                        sym: sym.into(),
                        left: ty(&j["left"]),
                        right: ty(&j["right"]),
                        func: j["func"].as_str().unwrap_or_default().into(),
                    });
                }
            } else if let (Some(from), Some(to)) = (ty(&j["from"]), ty(&j["to"])) {
                d.casts.push(CastDef {
                    from,
                    to,
                    func: j["func"].as_str().map(String::from),
                    binary: j["binary"] == true,
                });
            }
        }
        d
    }) else {
        return;
    };
    // (merged in, never replaced: another database's script may be reading the registry; a drop removes its own)
    let mut all = DEFS.lock().unwrap();
    for o in defs.ops {
        match all.ops.iter_mut().find(|x| x.sym == o.sym && x.left == o.left && x.right == o.right) {
            Some(x) => *x = o,
            None => all.ops.push(o),
        }
    }
    for c in defs.casts {
        match all.casts.iter_mut().find(|x| x.from == c.from && x.to == c.to) {
            Some(x) => *x = c,
            None => all.casts.push(c),
        }
    }
    for a in defs.aggs {
        match all.aggs.iter_mut().find(|x| x.name == a.name && x.args == a.args) {
            Some(x) => *x = a,
            None => all.aggs.push(a),
        }
    }
}

/// Support function `n` of operator class `name` of access method `am`, made with CREATE OPERATOR CLASS: the
/// function's name as it is stored. None for one of Postgres's own, or one without it.
pub(super) fn opclass_proc(v: &View, name: &str, am: &str, n: u32) -> Result<Option<String>> {
    let Some(raw) = v.get(&format!("{PREFIX}opclass/{name}/{am}"))? else { return Ok(None) };
    let j: Json = serde_json::from_str(&raw).unwrap_or(Json::Null);
    Ok(j["procs"][n.to_string()].as_str().map(String::from))
}

/// The partitioned tables whose hash key names operator class `key` (`_sonos_defs/opclass/<name>/<method>`): what
/// depends on the class, as a table depends on its partition key's classes (StorePartitionKey, heap.c:3830).
pub(super) fn opclass_users(v: &View, key: &str) -> Result<Vec<String>> {
    let Some((name, am)) = key.strip_prefix(&format!("{PREFIX}opclass/")).and_then(|r| r.rsplit_once('/')) else {
        return Ok(vec![]);
    };
    if am != "hash" {
        return Ok(vec![]);
    }
    Ok(super::schemas(v)?
        .into_iter()
        .filter(|t| {
            t.hier.part.as_ref().is_some_and(|k| {
                k.strategy == super::partition::Strategy::Hash && (0..k.parts.len()).any(|i| k.ops(i) == Some(name))
            })
        })
        .map(|t| t.name)
        .collect())
}

/// The indexes whose parts name operator class `key` (`_sonos_defs/opclass/<name>/<method>`) of their access method,
/// each as (its table, its name): what depends on the class, as an index depends on its parts' classes
/// (index_create, index.c:1050). A partition's copy of its parent's goes with that one.
pub(super) fn opclass_indexes(v: &View, key: &str) -> Result<Vec<(String, String)>> {
    let Some((name, am)) = key.strip_prefix(&format!("{PREFIX}opclass/")).and_then(|r| r.rsplit_once('/')) else {
        return Ok(vec![]);
    };
    let mut out = vec![];
    for t in super::schemas(v)? {
        for x in &t.indexes {
            if x.method() == am
                && !t.hier.inherited.contains(&x.name)
                && x.classes.iter().any(|c| c.as_deref() == Some(name))
            {
                out.push((t.name.clone(), super::qualify(schema_part(&t.name), &x.name)));
            }
        }
    }
    Ok(out)
}

/// DROP OPERATOR CLASS's refusal while indexes `indexes` use the class `key` names, or partition keys `users` hash
/// with it (2BP01).
pub(super) fn opclass_in_use(key: &str, indexes: &[String], users: &[String]) -> SqlError {
    let (name, am) =
        key.strip_prefix(&format!("{PREFIX}opclass/")).and_then(|r| r.rsplit_once('/')).unwrap_or(("", ""));
    let on = format!("operator class {name} for access method {am}");
    let lines: Vec<String> = (indexes.iter().map(|x| format!("index {} depends on {on}", rel_part(x))))
        .chain(users.iter().map(|t| format!("table {} depends on {on}", rel_part(t))))
        .collect();
    SqlError {
        code: "2BP01",
        message: format!("cannot drop {on} because other objects depend on it"),
        info: Some(Box::new(Violation {
            detail: Some(lines.join("\n")),
            hint: Some("Use DROP ... CASCADE to drop the dependent objects too.".into()),
            ..Violation::default()
        })),
    }
}

/// Takes a dropped definition out of the registry.
pub(super) fn forget(key: &str) {
    if let Some(n) = key.strip_prefix(&format!("{PREFIX}tsdictionary/")) {
        super::tsearch::forget_dict(n);
    }
    if let Some(n) = key.strip_prefix(&format!("{PREFIX}tsconfiguration/")) {
        super::tsearch::forget_config(n);
    }
    let mut all = DEFS.lock().unwrap();
    all.ops.retain(|o| op_key(&o.sym, &o.left, &o.right) != key);
    all.casts.retain(|c| cast_key(c.from, c.to) != key);
    // (an aggregate keeps its place: others are numbered by theirs; it is dropped by name, so it no longer parses)
    all.aggs.iter_mut().filter(|a| a.key == key).for_each(|a| a.name = String::new());
}

impl Parser {
    /// The operator symbol after CREATE/DROP OPERATOR: the symbol tokens up to the `(`.
    fn operator_symbol(&mut self) -> Result<String> {
        if matches!(self.peek(), Some(Tok::Word(_) | Tok::Ident(_))) && self.t.get(self.i + 1) == Some(&Tok::Sym(".")) {
            self.i += 2;
        }
        // (a name that isn't `schema.op` is a syntax error at what follows it, as Postgres's any_operator has it)
        if matches!(self.peek(), Some(Tok::Word(_) | Tok::Ident(_))) {
            self.i += 1;
            return self.fail("expected .");
        }
        // the operator's characters (a `;`, `,` or parenthesis ends it)
        let mut s = String::new();
        while let Some(Tok::Sym(x)) = self.peek() {
            if !x.chars().all(|c| "~!@#^&|`?+-*/%<>=".contains(c)) {
                break;
            }
            s += x;
            self.i += 1;
        }
        if s.is_empty() {
            return self.fail("expected an operator");
        }
        if s.contains(['(', ')', ',', ';']) {
            return err("42602", format!("operator name \"{s}\" is not valid"));
        }
        Ok(s)
    }

    /// `(left, right)` for DROP OPERATOR, NONE for a missing side.
    fn operator_types(&mut self) -> Result<(Option<Type>, Option<Type>)> {
        self.expect_sym("(")?;
        let side = |p: &mut Parser| -> Result<Option<Type>> { if p.kw("none") { Ok(None) } else { p.ty().map(Some) } };
        let l = side(self)?;
        // `(t)`: a unary operator's missing side is written NONE
        if matches!(self.peek(), Some(Tok::Sym(")"))) {
            let position = self.starts.get(self.i).map(|s| *s as u32 + 1);
            return Err(SqlError {
                code: "42601",
                message: "missing argument".into(),
                info: Some(Box::new(Violation {
                    position,
                    hint: Some("Use NONE to denote the missing argument of a unary operator.".into()),
                    ..Violation::default()
                })),
            });
        }
        self.expect_sym(",")?;
        let r = side(self)?;
        self.expect_sym(")")?;
        Ok((l, r))
    }

    /// After CREATE OPERATOR.
    pub(super) fn create_operator(&mut self) -> Result<Stmt> {
        let sym = self.operator_symbol()?;
        self.expect_sym("(")?;
        let (mut left, mut right, mut func) = (None, None, None);
        loop {
            // (an attribute's name may be a keyword: JOIN)
            // (a quoted one is its name as written: "Leftarg" is no attribute)
            let key = match self.next() {
                Some(Tok::Word(w)) => w.to_lowercase(),
                Some(Tok::Ident(w)) => w,
                t => {
                    self.put_back(t);
                    return self.fail("expected an operator attribute");
                }
            };
            const KNOWN: &[&str] = &[
                "leftarg",
                "rightarg",
                "function",
                "procedure",
                "commutator",
                "negator",
                "restrict",
                "join",
                "hashes",
                "merges",
                "sort1",
                "sort2",
                "ltcmp",
                "gtcmp",
            ];
            if !KNOWN.contains(&key.as_str()) {
                notice("WARNING", format!("operator attribute \"{key}\" not recognized"));
            }
            if self.sym("=") {
                if matches!(key.as_str(), "leftarg" | "rightarg") && self.kw("setof") {
                    return err("42P13", "SETOF type not allowed for operator argument");
                }
                match key.as_str() {
                    "leftarg" => left = Some(self.ty()?),
                    "rightarg" => right = Some(self.ty()?),
                    "function" | "procedure" => func = Some(self.qualified(false, Kind::Func)?),
                    _ => {
                        // commutator, negator, restrict, join: planner hints. A value is a name or OPERATOR(sym)
                        let mut depth = 0;
                        while let Some(t) = self.peek() {
                            match t {
                                Tok::Sym("(") => depth += 1,
                                Tok::Sym(")") if depth == 0 => break,
                                Tok::Sym(")") => depth -= 1,
                                Tok::Sym(",") if depth == 0 => break,
                                _ => {}
                            }
                            self.i += 1;
                        }
                    }
                }
            }
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        let Some(func) = func else { return err("42P13", "operator function must be specified") };
        if left.is_none() && right.is_none() {
            return err("42P13", "operator argument types must be specified");
        }
        // (a shell type, CREATE TYPE name, stays one here, its input and output being C functions: an operator over
        // it would compare values no type gives)
        for t in [left, right].into_iter().flatten() {
            let n = match t {
                Type::Enum(i) if i & UNRESOLVED_BIT != 0 => unresolved_name(i).unwrap_or_default(),
                t => t.name().to_string(),
            };
            if is_shell(&n) {
                return err(UNSUPPORTED, format!("type {n} is only a shell: an operator over it isn't supported"));
            }
        }
        let key = op_key(&sym, &left, &right);
        // (a side's type not looked up yet has to be there: `type "x" does not exist`)
        let needs = [left, right].into_iter().flatten().find_map(|t| match t {
            Type::Enum(i) if i & UNRESOLVED_BIT != 0 => {
                let n = unresolved_name(i)?;
                Some((type_key(&n), format!("type \"{}\" does not exist", shown_type(&n))))
            }
            _ => None,
        });
        let json = serde_json::json!({ "sym": sym, "left": left.map(|t| t.name()), "right": right.map(|t| t.name()), "func": func });
        Ok(Stmt::UserDef {
            needs,
            key,
            json: Some(json.to_string()),
            tag: "CREATE OPERATOR",
            missing: None,
            exists: Some(format!("operator {sym} already exists")),
            cascade: false,
        })
    }

    /// After DROP OPERATOR.
    pub(super) fn drop_operator(&mut self) -> Result<Stmt> {
        let if_exists = self.kw("if");
        if if_exists {
            self.expect_kw("exists")?;
        }
        let sym = self.operator_symbol()?;
        let (l, r) = self.operator_types()?;
        self.cascade();
        let key = op_key(&sym, &l, &r);
        // (IF EXISTS: Postgres's notice names the operator by its symbol alone)
        let missing = Some(match if_exists {
            true => format!("{SKIP}operator {sym} does not exist, skipping"),
            false => format!("operator does not exist: {} {sym} {}", tname(&l), tname(&r)),
        });
        // a side's type not looked up yet has to be there (`type "x" does not exist`, before the operator is looked for)
        let needs = [l, r].into_iter().flatten().find_map(|t| unresolved_needs(t, if_exists));
        Ok(Stmt::UserDef { needs, key, json: None, tag: "DROP OPERATOR", missing, exists: None, cascade: false })
    }

    /// After ALTER OPERATOR: `symbol (left, right) SET (...) | OWNER TO role | SET SCHEMA s`. The operator has to be
    /// there; what SET gives (RESTRICT, JOIN, COMMUTATOR, ...) only tunes Postgres's planner, and is not kept.
    pub(super) fn alter_operator(&mut self) -> Result<Stmt> {
        let sym = self.operator_symbol()?;
        let (l, r) = self.operator_types()?;
        self.skip_rest();
        let missing = Some(format!("operator does not exist: {} {sym} {}", tname(&l), tname(&r)));
        Ok(Stmt::UserDef {
            needs: None,
            key: op_key(&sym, &l, &r),
            json: None,
            tag: "ALTER OPERATOR",
            missing,
            exists: None,
            cascade: false,
        })
    }

    /// An aggregate's name: `name` or `schema.name`, kept by its name alone (a call finds an aggregate by its name).
    fn aggregate_name(&mut self) -> Result<String> {
        let mut name = self.name()?;
        while self.sym(".") {
            name = self.name()?;
        }
        Ok(name)
    }

    /// One argument of an aggregate's signature: `[IN | VARIADIC] [name] type`, added to `sig`.
    fn aggregate_arg(&mut self, sig: &mut AggSig) -> Result<()> {
        if self.is_kw("out") || self.is_kw("inout") {
            return err(UNSUPPORTED, "aggregates cannot have output arguments");
        }
        self.kw("in");
        let variadic = self.kw("variadic");
        let at = self.peek().cloned();
        let alone = self.attempt(|p| {
            let t = p.ty()?;
            if arg_ends(p) { Ok(t) } else { p.fail("") }
        });
        let (t, at) = match alone {
            Some(t) => (t, at),
            None => {
                self.name()?;
                let at = self.peek().cloned();
                (self.ty()?, at)
            }
        };
        sig.written.push(written_type(at.as_ref()).unwrap_or_else(|| t.name().to_string()));
        sig.args.push(t);
        sig.variadic = variadic;
        Ok(())
    }

    /// An aggregate's signature: `(*)`, `(args)`, `(ORDER BY args)` or `(args ORDER BY args)` (an ordered-set
    /// aggregate's direct arguments, then its aggregated ones).
    pub(super) fn aggregate_sig(&mut self) -> Result<AggSig> {
        self.expect_sym("(")?;
        let mut sig = AggSig::default();
        if self.sym("*") {
            sig.star = true;
            self.expect_sym(")")?;
            return Ok(sig);
        }
        let order_by = |p: &mut Parser| {
            let at = p.is_kw("order") && matches!(p.t.get(p.i + 1), Some(Tok::Word(w)) if w == "by");
            if at {
                p.i += 2;
            }
            at
        };
        let mut ordered = order_by(self);
        if !ordered {
            loop {
                self.aggregate_arg(&mut sig)?;
                if !self.sym(",") {
                    break;
                }
            }
            ordered = order_by(self);
        }
        if ordered {
            let direct = sig.args.len();
            let mut agg = AggSig::default();
            loop {
                self.aggregate_arg(&mut agg)?;
                if !self.sym(",") {
                    break;
                }
            }
            if sig.variadic {
                // `VARIADIC "any" ORDER BY VARIADIC "any"`: kept as its direct arguments alone, as Postgres keeps it
                if agg.args.len() != 1 || !agg.variadic || agg.args[0].name() != sig.args[direct - 1].name() {
                    return err(
                        "42P13",
                        "an ordered-set aggregate with a VARIADIC direct argument must have one VARIADIC aggregated argument of the same data type",
                    );
                }
            } else {
                sig.args.extend(agg.args);
                sig.written.extend(agg.written);
                sig.variadic = agg.variadic;
            }
            sig.direct = Some(direct);
        }
        self.expect_sym(")")?;
        Ok(sig)
    }

    /// A support function an aggregate names: `name` or `schema.name`, maybe with argument types
    /// (`balkifnull(int8, int4)`: read, not used, as Postgres reads them: the aggregate's types choose the
    /// function), as (the name kept, as `qualified` keeps it; the name as written, for messages).
    fn aggregate_func(&mut self) -> Result<(String, String)> {
        let mut shown = String::new();
        let mut k = self.i;
        while let Some(Tok::Word(w) | Tok::Ident(w)) = self.t.get(k) {
            shown.push_str(w);
            if self.t.get(k + 1) != Some(&Tok::Sym(".")) {
                break;
            }
            shown.push('.');
            k += 2;
        }
        let stored = self.qualified(false, Kind::Func)?;
        if self.sym("(") {
            let mut depth = 1;
            while depth > 0 {
                match self.next() {
                    Some(Tok::Sym("(")) => depth += 1,
                    Some(Tok::Sym(")")) => depth -= 1,
                    None => return self.fail("expected )"),
                    _ => {}
                }
            }
        }
        Ok((stored, shown))
    }

    /// An aggregate option's value: a word, a string or a (signed) number, as text.
    fn aggregate_value(&mut self) -> Result<String> {
        let neg = self.sym("-");
        match self.next() {
            Some(Tok::Num(n)) => Ok(if neg { format!("-{n}") } else { n }),
            Some(Tok::Str(s) | Tok::Word(s) | Tok::Ident(s)) if !neg => Ok(s),
            t => {
                self.put_back(t);
                self.fail("expected a value")
            }
        }
    }

    /// Skips an option's value, up to the `,` or `)` that ends it.
    fn aggregate_skip(&mut self) {
        let mut depth = 0;
        while let Some(t) = self.peek() {
            match t {
                Tok::Sym("(") => depth += 1,
                Tok::Sym(")") if depth == 0 => break,
                Tok::Sym(")") => depth -= 1,
                Tok::Sym(",") if depth == 0 => break,
                _ => {}
            }
            self.i += 1;
        }
    }

    /// BASETYPE's value: a type, or ANY (`'ANY'` too, in any case) for none.
    fn aggregate_basetype(&mut self) -> Result<Option<Type>> {
        match self.peek() {
            Some(Tok::Word(w)) if w == "any" => {
                self.i += 1;
                Ok(None)
            }
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.i += 1;
                if s.eq_ignore_ascii_case("any") {
                    return Ok(None);
                }
                Ok(Some(Parser::new(lex(&s)?).ty()?))
            }
            _ => self.ty().map(Some),
        }
    }

    /// After ALTER AGGREGATE: `name (signature) RENAME TO n | OWNER TO role | SET SCHEMA s`. The aggregate has to be
    /// there; RENAME moves it (see `agg_stmt`), the others aren't kept.
    pub(super) fn alter_aggregate(&mut self) -> Result<Stmt> {
        let name = self.aggregate_name()?;
        let sig = self.aggregate_sig()?;
        // RENAME TO, OWNER TO role, SET SCHEMA s (anything else is read and changes nothing here)
        let mut change = serde_json::json!({});
        if self.kw("rename") {
            self.expect_kw("to")?;
            change["rename"] = Json::from(self.name()?);
        } else if self.kw("owner") {
            self.expect_kw("to")?;
            change["owner_to"] = Json::from(self.name()?);
        } else if self.is_kw("set") && matches!(self.t.get(self.i + 1), Some(Tok::Word(w)) if w == "schema") {
            self.i += 2;
            change["schema"] = Json::from(self.name()?);
        } else {
            self.skip_rest();
        }
        Ok(Stmt::UserDef {
            needs: None,
            key: agg_key(&name, &sig.args),
            json: Some(change.to_string()),
            tag: "ALTER AGGREGATE",
            missing: Some(format!("aggregate {name}({}) does not exist", sig.shown())),
            exists: None,
            cascade: false,
        })
    }

    /// After CREATE [OR REPLACE] AGGREGATE: `name (signature) (option = value, ...)`, or the old form
    /// `name (BASETYPE = t, SFUNC = f, STYPE = t, ...)`, whose one argument BASETYPE gives (none for ANY). The
    /// options are checked here as Postgres's DefineAggregate checks them, the functions they name when it runs (see
    /// `create_agg`); SSPACE, SORTOP, PARALLEL and the *_MODIFY settings only tune how Postgres runs it, and are read
    /// and not kept.
    pub(super) fn create_aggregate(&mut self, replace: bool) -> Result<Stmt> {
        let name = self.aggregate_name()?;
        let old = self.peek() == Some(&Tok::Sym("("))
            && matches!(self.t.get(self.i + 1), Some(Tok::Word(_) | Tok::Ident(_)))
            && self.t.get(self.i + 2) == Some(&Tok::Sym("="));
        let sig = if old { AggSig::default() } else { self.aggregate_sig()? };
        let ordered = sig.direct.is_some();
        self.expect_sym("(")?;
        // the functions named, by option: as kept, and as written
        let (mut funcs, mut shown) = (serde_json::Map::new(), serde_json::Map::new());
        let (mut stype, mut mstype, mut basetype): (Option<Type>, Option<Type>, Option<Option<Type>>) =
            (None, None, None);
        let (mut initcond, mut minitcond, mut parallel) = (None, None, None);
        let (mut extra, mut hypothetical) = (false, false);
        loop {
            // (as Postgres compares them: a quoted "Sfunc" is no option of its)
            let key = self.name()?;
            let valued = self.sym("=");
            let needs = |valued: bool| -> Result<()> {
                if valued { Ok(()) } else { err(SYNTAX, format!("{key} requires a parameter")) }
            };
            match key.as_str() {
                "sfunc" | "sfunc1" | "finalfunc" | "combinefunc" | "serialfunc" | "deserialfunc" | "msfunc"
                | "minvfunc" | "mfinalfunc" => {
                    needs(valued)?;
                    let (stored, written) = self.aggregate_func()?;
                    let k = if key == "sfunc1" { "sfunc" } else { key.as_str() };
                    funcs.insert(k.to_string(), Json::from(stored));
                    shown.insert(k.to_string(), Json::from(written));
                }
                "stype" | "stype1" => {
                    needs(valued)?;
                    stype = Some(self.ty()?);
                }
                "mstype" => {
                    needs(valued)?;
                    mstype = Some(self.ty()?);
                }
                "basetype" => {
                    needs(valued)?;
                    basetype = Some(self.aggregate_basetype()?);
                }
                "initcond" | "initcond1" => {
                    needs(valued)?;
                    initcond = Some(self.aggregate_value()?);
                }
                "minitcond" => {
                    needs(valued)?;
                    minitcond = Some(self.aggregate_value()?);
                }
                "parallel" => {
                    needs(valued)?;
                    parallel = Some(self.aggregate_value()?);
                }
                "finalfunc_extra" | "mfinalfunc_extra" | "hypothetical" => {
                    let on = match valued {
                        false => true,
                        true => match self.aggregate_value()?.to_lowercase().as_str() {
                            "true" | "on" | "1" => true,
                            "false" | "off" | "0" => false,
                            _ => return err(SYNTAX, format!("{key} requires a Boolean value")),
                        },
                    };
                    if key == "finalfunc_extra" {
                        extra = on;
                    }
                    if key == "hypothetical" && on {
                        if !ordered {
                            return err("42P13", "only ordered-set aggregates can be hypothetical");
                        }
                        hypothetical = true;
                    }
                }
                "finalfunc_modify" | "mfinalfunc_modify" => {
                    needs(valued)?;
                    if !matches!(self.aggregate_value()?.as_str(), "read_only" | "shareable" | "read_write") {
                        return err(SYNTAX, format!("parameter \"{key}\" must be READ_ONLY, SHAREABLE, or READ_WRITE"));
                    }
                }
                "sspace" | "msspace" | "sortop" => self.aggregate_skip(),
                _ => {
                    notice("WARNING", format!("aggregate attribute \"{key}\" not recognized"));
                    self.aggregate_skip();
                }
            }
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        let Some(stype) = stype else { return err("42P13", "aggregate stype must be specified") };
        if !funcs.contains_key("sfunc") {
            return err("42P13", "aggregate sfunc must be specified");
        }
        // the moving-aggregate options go together
        if mstype.is_some() {
            for f in ["msfunc", "minvfunc"] {
                if !funcs.contains_key(f) {
                    return err("42P13", format!("aggregate {f} must be specified when mstype is specified"));
                }
            }
        } else {
            for f in ["msfunc", "minvfunc", "mfinalfunc"] {
                if funcs.contains_key(f) {
                    return err("42P13", format!("aggregate {f} must not be specified without mstype"));
                }
            }
            if minitcond.is_some() {
                return err("42P13", "aggregate minitcond must not be specified without mstype");
            }
        }
        let args: Vec<Type> = match (old, basetype) {
            (true, None) => return err("42P13", "aggregate input type must be specified"),
            (true, Some(b)) => b.into_iter().collect(),
            (false, Some(_)) => return err("42P13", "basetype is redundant with aggregate input type specification"),
            (false, None) => sig.args.clone(),
        };
        match (funcs.contains_key("serialfunc"), funcs.contains_key("deserialfunc")) {
            (true, true) if stype.name() != "internal" => {
                return err(
                    "42P13",
                    "serialization functions may be specified only when the aggregate transition data type is internal",
                );
            }
            (true, false) | (false, true) => {
                return err("42P13", "must specify both or neither of serialization and deserialization functions");
            }
            _ => {}
        }
        // an initial value has to be one of the state's type (unless that is polymorphic, or the user's)
        if let Some(c) = &initcond
            && !matches!(stype, Type::Poly(_) | Type::Enum(_) | Type::Record(_))
        {
            coerce(Value::Text(c.clone()), stype)?;
        }
        if let Some(p) = &parallel
            && !matches!(p.as_str(), "safe" | "restricted" | "unsafe")
        {
            return err(SYNTAX, "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE");
        }
        let names: Vec<String> = args.iter().map(|t| t.name().to_string()).collect();
        let kind = if hypothetical {
            "h"
        } else if ordered {
            "o"
        } else {
            "n"
        };
        let json = serde_json::json!({
            "name": name, "args": names, "direct": sig.direct, "variadic": sig.variadic && !old,
            "sfunc": funcs.get("sfunc"), "stype": stype.name(), "finalfunc": funcs.get("finalfunc"),
            "finalfunc_extra": extra, "combinefunc": funcs.get("combinefunc"), "serialfunc": funcs.get("serialfunc"),
            "deserialfunc": funcs.get("deserialfunc"), "mstype": mstype.map(|t| t.name().to_string()),
            "msfunc": funcs.get("msfunc"), "minvfunc": funcs.get("minvfunc"), "mfinalfunc": funcs.get("mfinalfunc"),
            "initcond": initcond, "minitcond": minitcond, "kind": kind, "shown": shown, "replace": replace,
        });
        Ok(Stmt::UserDef {
            needs: None,
            key: agg_key(&name, &args),
            json: Some(json.to_string()),
            tag: "CREATE AGGREGATE",
            missing: None,
            exists: None,
            cascade: false,
        })
    }

    /// After DROP AGGREGATE: `[IF EXISTS] name (signature) [CASCADE | RESTRICT]`.
    pub(super) fn drop_aggregate(&mut self) -> Result<Stmt> {
        let if_exists = self.guard(false)?;
        let name = self.aggregate_name()?;
        let sig = self.aggregate_sig()?;
        self.cascade();
        // (IF EXISTS: Postgres names the types as written, `int` as pg_catalog.int4, and `*` as nothing)
        let missing = match if_exists {
            false => format!("aggregate {name}({}) does not exist", sig.shown()),
            true => format!(
                "{SKIP}aggregate {name}({}) does not exist, skipping",
                if sig.star { String::new() } else { sig.written.join(",") }
            ),
        };
        // a type not looked up yet has to be there (`type "x" does not exist`, as Postgres looks the types up first);
        // the pseudo-types an aggregate takes (`VARIADIC "any"`, internal) always are
        let needs = sig.args.iter().find_map(|t| match *t {
            Type::Enum(i) if i & UNRESOLVED_BIT != 0 => {
                let n = unresolved_name(i)?;
                if ["any", "internal", "cstring", "record", "void"].contains(&rel_part(written(&n))) {
                    return None;
                }
                // (IF EXISTS: a notice, and nothing dropped)
                let skip = if if_exists { SKIP } else { "" };
                let tail = if if_exists { ", skipping" } else { "" };
                Some((type_key(&n), format!("{skip}type \"{}\" does not exist{tail}", shown_type(&n))))
            }
            _ => None,
        });
        Ok(Stmt::UserDef {
            needs,
            key: agg_key(&name, &sig.args),
            json: None,
            tag: "DROP AGGREGATE",
            missing: Some(missing),
            exists: None,
            cascade: false,
        })
    }

    /// After COMMENT ON AGGREGATE: `name (signature)`, as the key of its row and its name for a message.
    pub(super) fn aggregate_target(&mut self) -> Result<(String, String)> {
        let name = self.aggregate_name()?;
        let sig = self.aggregate_sig()?;
        Ok((agg_key(&name, &sig.args), format!("{name}({})", sig.shown())))
    }

    /// `(from AS to)`
    fn cast_pair(&mut self) -> Result<(Type, Type)> {
        self.expect_sym("(")?;
        let from = self.ty()?;
        self.expect_kw("as")?;
        let to = self.ty()?;
        self.expect_sym(")")?;
        Ok((from, to))
    }

    /// After CREATE CAST.
    pub(super) fn create_cast(&mut self) -> Result<Stmt> {
        let (from, to) = self.cast_pair()?;
        let (mut func, mut binary) = (None, false);
        if self.kw("with") {
            if self.kw("function") {
                func = Some(self.qualified(false, Kind::Func)?);
                if self.sym("(") {
                    while !self.sym(")") {
                        if self.next().is_none() {
                            return self.fail("expected )");
                        }
                    }
                }
            } else {
                self.expect_kw("inout")?;
            }
        } else {
            self.expect_kw("without")?;
            self.expect_kw("function")?;
            binary = true;
        }
        if self.kw("as") && !(self.kw("assignment") || self.kw("implicit")) {
            return self.fail("expected ASSIGNMENT or IMPLICIT");
        }
        let json = serde_json::json!({ "from": from.name(), "to": to.name(), "func": func, "binary": binary });
        Ok(Stmt::UserDef {
            needs: None,
            key: cast_key(from, to),
            json: Some(json.to_string()),
            tag: "CREATE CAST",
            missing: None,
            exists: Some(format!("cast from type {} to type {} already exists", from.name(), to.name())),
            cascade: false,
        })
    }

    /// After DROP CAST.
    pub(super) fn drop_cast(&mut self) -> Result<Stmt> {
        let if_exists = self.kw("if");
        if if_exists {
            self.expect_kw("exists")?;
        }
        let (from, to) = self.cast_pair()?;
        self.cascade();
        let missing = format!("cast from type {} to type {} does not exist", from.name(), to.name());
        let missing = Some(if if_exists { format!("{SKIP}{missing}, skipping") } else { missing });
        // a type not looked up yet has to be there, as Postgres looks the types up first
        let needs = [from, to].into_iter().find_map(|t| unresolved_needs(t, if_exists));
        Ok(Stmt::UserDef {
            needs,
            key: cast_key(from, to),
            json: None,
            tag: "DROP CAST",
            missing,
            exists: None,
            cascade: false,
        })
    }

    /// `OPERATOR(schema.sym)` next, as pg_dump writes an operator that isn't in pg_catalog: the symbol, and how
    /// many tokens it takes. The schema isn't looked at (operators are kept by their symbol and types).
    fn qualified_user_op(&self) -> Option<(&'static str, usize)> {
        if !matches!(self.peek(), Some(Tok::Word(w)) if w == "operator")
            || self.t.get(self.i + 1) != Some(&Tok::Sym("("))
        {
            return None;
        }
        let mut at = self.i + 2;
        if matches!(self.t.get(at), Some(Tok::Word(_) | Tok::Ident(_))) && self.t.get(at + 1) == Some(&Tok::Sym(".")) {
            at += 2;
        }
        let Some(Tok::Sym(s)) = self.t.get(at) else { return None };
        if self.t.get(at + 1) != Some(&Tok::Sym(")")) {
            return None;
        }
        Some((*s, at + 2 - self.i))
    }

    /// A registered operator's call, for the infix operator now next (taken), with `l` its left side.
    #[inline(never)]
    pub(super) fn user_infix(&mut self, l: &mut Option<Expr>) -> Result<bool> {
        let (s, width) = match self.peek() {
            Some(Tok::Sym(s)) => (*s, 1),
            _ => match self.qualified_user_op() {
                Some(found) => found,
                None => return Ok(false),
            },
        };
        let Some(def) = operator(s, true) else { return Ok(false) };
        self.i += width;
        self.deeper(1)?;
        let r = self.add()?;
        self.calls = true;
        let args = vec![l.take().unwrap(), r];
        *l = Some(Expr::Call(Box::new(Call {
            name: op_call_name(s, true, &def),
            args,
            names: vec![],
            def: None,
            via_call: false,
            builtin: false,
        })));
        Ok(true)
    }

    /// A registered prefix operator's call.
    #[inline(never)]
    pub(super) fn user_prefix(&mut self) -> Result<Option<Expr>> {
        let (s, width) = match self.peek() {
            Some(Tok::Sym(s)) => (*s, 1),
            _ => match self.qualified_user_op() {
                Some(found) => found,
                None => return Ok(None),
            },
        };
        let Some(def) = operator(s, false) else { return Ok(None) };
        self.i += width;
        self.calls = true;
        let r = self.nest(1, |p| p.add())?;
        Ok(Some(Expr::Call(Box::new(Call {
            name: op_call_name(s, false, &def),
            args: vec![r],
            names: vec![],
            def: None,
            via_call: false,
            builtin: false,
        }))))
    }
}

impl Exec<'_> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn user_def(
        &self,
        v: &View,
        key: &str,
        json: &Option<String>,
        missing: &Option<String>,
        exists: &Option<String>,
        needs: &Option<(String, String)>,
        tag: &str,
    ) -> Result<Vec<(String, Option<String>)>> {
        let there = v.get(key)?.is_some();
        if let Some((k, m)) = needs
            && v.get(k)?.is_none()
        {
            if let Some(m) = m.strip_prefix(SKIP) {
                notice("NOTICE", m.to_string());
                return Ok(vec![]);
            }
            return err("42704", m.clone());
        }
        if tag.ends_with(" AGGREGATE") {
            return agg_stmt(v, key, json, missing, tag, there);
        }
        // DROP RULE: the relation is looked for first
        if tag == "DROP RULE"
            && !there
            && missing.is_some()
            && let Some(table) = super::rules::table_of(key)
            && relation_kind(v, table)?.is_none()
        {
            let missing_table = format!("relation \"{}\" does not exist", rel_part(table));
            if missing.as_ref().is_some_and(|m| m.starts_with(SKIP)) {
                notice("NOTICE", format!("{missing_table}, skipping"));
                return Ok(vec![]);
            }
            return err(NO_TABLE, missing_table);
        }
        if tag.starts_with("ALTER") {
            return match (there, missing, json) {
                (false, Some(m), _) if m.starts_with(SKIP) => {
                    notice("NOTICE", m[SKIP.len()..].to_string());
                    Ok(vec![])
                }
                (false, Some(m), _) => err("42704", m.clone()),
                // (an event trigger's change: its fields merged into the row)
                (true, _, Some(change)) => {
                    let mut row: Json = serde_json::from_str(&v.get(key)?.unwrap_or_default()).unwrap_or_default();
                    let c: Json = serde_json::from_str(change).unwrap_or_default();
                    if c.get("op").is_some() {
                        ts_apply(&mut row, &c, key)?;
                    } else if let (Some(r), Some(c)) = (row.as_object_mut(), c.as_object()) {
                        r.extend(c.clone());
                    }
                    Ok(vec![(key.to_string(), Some(row.to_string()))])
                }
                _ => Ok(vec![]),
            };
        }
        match json {
            // (OR REPLACE: the row is written over)
            Some(j) if there && j.contains("\"replace\":true") => Ok(vec![(key.to_string(), json.clone())]),
            Some(_) if there && exists.is_none() => Ok(vec![]),
            Some(_) if there => err("42710", exists.clone().unwrap_or_default()),
            None if !there => match missing {
                Some(m) if m.starts_with(SKIP) => {
                    notice("NOTICE", m[SKIP.len()..].to_string());
                    Ok(vec![])
                }
                Some(m) => err(if tag.contains("OPERATOR") { "42883" } else { "42704" }, m.clone()),
                None => Ok(vec![]),
            },
            None => {
                forget(key);
                Ok(vec![(key.to_string(), None)])
            }
            _ => Ok(vec![(key.to_string(), json.clone())]),
        }
    }
}

fn run(name: &str, vals: Vec<Value>, state: Option<Type>, env: &Env) -> Result<Value> {
    let name = func_named(name);
    // (the state as its type: one kept as text, `{}`, is the int[] it stands for)
    let mut tys: Vec<Option<Type>> = vals.iter().map(functions::value_type_loose).collect();
    if let (Some(t), Some(first)) = (state, tys.first_mut())
        && !matches!(t, Type::Poly(_))
        && vals[0] != Value::Null
    {
        *first = Some(t);
    }
    let def = match functions::resolve_typed(&name, &tys) {
        Ok(def) => def,
        // (a built-in by its name, as a step or a final function: `sfunc = textcat`)
        Err(e) => match super::builtin_of(super::written(&name)) {
            Some((f, arity)) if arity.contains(&vals.len()) => {
                return eval(&Expr::Func(f, vals.into_iter().map(Expr::Lit).collect()), env);
            }
            // one of Postgres's by its C name (`sfunc = int4pl`): what it does, as SQL
            _ => {
                return match internal_sql(rel_part(written(&name))) {
                    Some(sql) => internal_call(&sql, vals, env),
                    None => Err(e),
                };
            }
        },
    };
    // (a polymorphic one made for the values' types: `anyarray` the array of what `anyelement` is)
    let def = functions::instance(&def, &tys, false)?;
    // (each value typed as its parameter is: a state kept as text, `{}`, is the int[] it stands for, so `$1 || $2`
    // appends to it rather than joins text)
    let args = vals
        .into_iter()
        .enumerate()
        .map(|(k, v)| match def.args.get(k).map(|a| a.1) {
            Some(t) if v != Value::Null && !matches!(t, Type::Poly(_) | Type::Text) => {
                Ok(Expr::Cast(Box::new(Expr::Lit(v)), t))
            }
            _ => Ok(Expr::Lit(v)),
        })
        .collect::<Result<Vec<_>>>()?;
    let call =
        Expr::Call(Box::new(Call { name, args, names: vec![], def: Some(def), via_call: false, builtin: false }));
    eval(&call, env)
}

/// What a user aggregate of no rows gives (before its final function): its initial value (as text when its state
/// type is polymorphic: no row says what that is), or null.
pub(super) fn start(i: u8) -> Result<Value> {
    match aggregate_at(i) {
        Some(AggDef { initcond: Some(c), stype: Type::Poly(_), .. }) => Ok(Value::Text(c)),
        Some(AggDef { initcond: Some(c), stype, .. }) => coerce(Value::Text(c), stype),
        _ => Ok(Value::Null),
    }
}

impl Parser {
    /// Whether a CREATE, DROP or ALTER (its first word taken) is of a foreign-data wrapper, server, user mapping or
    /// foreign table, or of another object `named_stmt` reads (a publication, a tablespace, ...).
    pub(super) fn foreign_object(&self) -> bool {
        let w = |n: usize| matches!(self.t.get(self.i + n), Some(Tok::Word(w)) if w == "foreign");
        let at = |n: usize, x: &str| matches!(self.t.get(self.i + n), Some(Tok::Word(w)) if w == x);
        w(0) || at(0, "server")
            || (at(0, "user") && at(1, "mapping"))
            || (at(0, "text") && at(1, "search"))
            || at(0, "publication")
            || at(0, "subscription")
            || at(0, "statistics")
            || at(0, "tablespace")
            || (at(0, "operator") && (at(1, "class") || at(1, "family")))
            || (at(0, "access") && at(1, "method"))
    }

    /// CREATE, ALTER and DROP of the objects kept as names only: publications, subscriptions, extended statistics,
    /// operator classes and families (nothing here uses them).
    fn named_stmt(&mut self, verb: &str) -> Result<Stmt> {
        if self.kw("publication") {
            return self.publication(verb);
        }
        if self.kw("statistics") {
            return self.statistics(verb);
        }
        if self.kw("tablespace") {
            return self.tablespace(verb);
        }
        let (what, kind, tag): (&str, &str, &str) = if self.kw("publication") {
            ("publication", "pub", "PUBLICATION")
        } else if self.kw("subscription") {
            ("subscription", "sub", "SUBSCRIPTION")
        } else if self.kw("statistics") {
            ("statistics object", "stat", "STATISTICS")
        } else {
            self.expect_kw("operator")?;
            if self.kw("class") {
                ("operator class", "opclass", "OPERATOR CLASS")
            } else {
                self.expect_kw("family")?;
                ("operator family", "opfamily", "OPERATOR FAMILY")
            }
        };
        let tag: &'static str = Box::leak(format!("{} {tag}", verb.to_uppercase()).into_boxed_str());
        let if_not = verb == "create" && self.guard(true)?;
        let if_exists = verb != "create" && self.guard(false)?;
        // an unnamed statistics object is named by its table and columns, as Postgres does
        let unnamed = kind == "stat"
            && verb == "create"
            && matches!(self.peek(), Some(Tok::Sym("(")) | Some(Tok::Word(_)))
            && (matches!(self.peek(), Some(Tok::Sym("("))) || self.is_kw("on"));
        let name = if unnamed {
            let (mut cols, mut table, mut j, mut on) = (vec![], String::new(), self.i, false);
            while j < self.t.len() && self.t[j] != Tok::Sym(";") {
                match &self.t[j] {
                    Tok::Word(w) if w == "on" => on = true,
                    Tok::Word(w) if w == "from" => {
                        if let Some(Tok::Word(t) | Tok::Ident(t)) = self.t.get(j + 1) {
                            table = t.clone();
                        }
                        break;
                    }
                    Tok::Word(w) | Tok::Ident(w) if on => cols.push(w.clone()),
                    _ => {}
                }
                j += 1;
            }
            format!("{table}_{}_stat", cols.join("_"))
        } else {
            let mut n = self.name()?;
            while self.sym(".") {
                n = self.name()?;
            }
            n
        };
        // an operator class's DEFAULT FOR TYPE type: what an index of its access method gets for a column of that type
        // that names no class (see `accessmethod::made_default_class`), kept with it
        let mut facts = serde_json::json!({});
        if kind == "opclass" && verb == "create" {
            let default = self.kw("default");
            if self.kw("for") {
                self.expect_kw("type")?;
                let at = self.i;
                match self.ty() {
                    Ok(t) => {
                        facts["type"] = Json::from(t.oid());
                        if default {
                            facts["default"] = Json::Bool(true);
                        }
                    }
                    // (a type this can't read: the class is kept as its name only)
                    Err(_) => self.i = at,
                }
            }
        }
        // an operator class or family is of an access method
        let using = if kind.starts_with("op") {
            let mut j = self.i;
            let mut m = String::new();
            while j < self.t.len() && self.t[j] != Tok::Sym(";") {
                if matches!(&self.t[j], Tok::Word(w) if w == "using")
                    && let Some(Tok::Word(a) | Tok::Ident(a)) = self.t.get(j + 1)
                {
                    m = a.clone();
                }
                j += 1;
            }
            m
        } else {
            String::new()
        };
        // (the access method is looked up first, IF EXISTS or not: Postgres's own, or an index method made here)
        if !using.is_empty() {
            super::accessmethod::opclass_am_at_parse(&using)?;
        }
        // an operator class's support functions (`FUNCTION n [(types)] name(argument types)`), by number: what a hash
        // partition key naming the class hashes its values with (its function 2)
        let (mut procs, mut sigs) = (serde_json::Map::new(), serde_json::Map::new());
        if kind == "opclass" && verb == "create" {
            let mut j = self.i;
            while j < self.t.len() && self.t[j] != Tok::Sym(";") {
                if matches!(&self.t[j], Tok::Word(w) if w == "function")
                    && let Some(Tok::Num(n)) = self.t.get(j + 1)
                {
                    // (its own copy: the types after it are read with the parser, which the tokens' borrow would hold)
                    let n = n.clone();
                    let mut k = j + 2;
                    if self.t.get(k) == Some(&Tok::Sym("(")) {
                        while k < self.t.len() && self.t[k] != Tok::Sym(")") {
                            k += 1;
                        }
                        k += 1;
                    }
                    let mut fname = String::new();
                    while let Some(Tok::Word(w) | Tok::Ident(w)) = self.t.get(k) {
                        fname.push_str(w);
                        k += 1;
                        if self.t.get(k) != Some(&Tok::Sym(".")) {
                            break;
                        }
                        fname.push('.');
                        k += 1;
                    }
                    // (kept as the function is stored: a schema's name qualified as `qualify` keeps it, public's bare; a
                    // plain name as the search_path finds it. A built-in, named in pg_catalog or found as no function
                    // made here is, is kept as pg_catalog's, so one made later of its name isn't taken for it)
                    if !fname.is_empty() {
                        let builtin = |f: &str| super::builtin_of(f).is_some();
                        let stored = match fname.rsplit_once('.') {
                            Some(("pg_catalog", f)) if builtin(f) => format!("pg_catalog.{f}"),
                            Some((schema, f)) => super::qualify(schema, f),
                            None if builtin(&fname) && super::funcs_on_path(&fname).is_empty() => {
                                format!("pg_catalog.{fname}")
                            }
                            None => super::find_named(&fname, Kind::Func),
                        };
                        procs.insert(n.clone(), Json::String(stored));
                        // and its argument types, which tell it from the function's overloads (see `opclasses_using`)
                        if self.t.get(k) == Some(&Tok::Sym("(")) {
                            let at = std::mem::replace(&mut self.i, k + 1);
                            let mut types = vec![];
                            let read = (|| -> Result<()> {
                                while !self.sym(")") {
                                    types.push(Json::String(self.ty()?.name().to_string()));
                                    self.sym(",");
                                }
                                Ok(())
                            })();
                            self.i = at;
                            if read.is_ok() {
                                sigs.insert(n.clone(), Json::Array(types));
                            }
                        }
                    }
                }
                j += 1;
            }
        }
        if !procs.is_empty() {
            facts["procs"] = Json::Object(procs);
            facts["sigs"] = Json::Object(sigs);
        }
        // (DROP ... CASCADE: an operator class's partition keys go with it; see `opclass_users`)
        let cascade = verb == "drop"
            && self.t[self.i..]
                .iter()
                .take_while(|t| **t != Tok::Sym(";"))
                .any(|t| matches!(t, Tok::Word(w) if w == "cascade"));
        self.skip_rest();
        let key = format!("{PREFIX}{kind}/{name}/{using}");
        let on = if using.is_empty() { String::new() } else { format!(" for access method \"{using}\"") };
        let label = format!("{what} \"{name}\"{on}");
        Ok(match verb {
            "create" => Stmt::UserDef {
                needs: None,
                key,
                json: Some(facts.to_string()),
                tag,
                missing: None,
                exists: (!if_not).then(|| format!("{label} already exists")),
                cascade: false,
            },
            _ => Stmt::UserDef {
                needs: None,
                key,
                json: None,
                tag,
                missing: Some(match if_exists {
                    true => format!("{SKIP}{what} \"{name}\" does not exist{on}, skipping"),
                    false => format!("{what} \"{name}\" does not exist{on}"),
                }),
                exists: None,
                cascade,
            },
        })
    }

    /// After CREATE, ALTER or DROP: `[DEFAULT] CONVERSION ...`. A conversion is kept as its name only: text here
    /// is UTF8, which nothing converts (CREATE CONVERSION name FOR 'from' TO 'to' FROM function).
    pub(super) fn conversion_stmt(&mut self, verb: &str) -> Result<Stmt> {
        self.kw("default");
        self.expect_kw("conversion")?;
        let tag: &'static str = match verb {
            "create" => "CREATE CONVERSION",
            "alter" => "ALTER CONVERSION",
            _ => "DROP CONVERSION",
        };
        let if_exists = verb == "drop" && self.guard(false)?;
        let mut name = self.name()?;
        while self.sym(".") {
            name = self.name()?;
        }
        self.skip_rest();
        let key = format!("{PREFIX}conversion/{name}");
        let missing = format!("conversion \"{name}\" does not exist");
        Ok(Stmt::UserDef {
            needs: None,
            key,
            json: (verb == "create").then(|| "{}".to_string()),
            tag,
            missing: Some(if if_exists { format!("{SKIP}{missing}, skipping") } else { missing }),
            exists: Some(format!("conversion \"{name}\" already exists")),
            cascade: false,
        })
    }

    /// `[IF [NOT] EXISTS]`, as (taken, whether it was there).
    fn guard(&mut self, not: bool) -> Result<bool> {
        if !self.kw("if") {
            return Ok(false);
        }
        if not {
            self.expect_kw("not")?;
        }
        self.expect_kw("exists")?;
        Ok(true)
    }

    /// CREATE, ALTER and DROP of text search objects, of the objects kept as names only (see `named_stmt`), and of
    /// foreign-data wrappers, servers, user mappings and foreign tables (see `foreign`).
    pub(super) fn foreign_stmt(&mut self, verb: &str) -> Result<Stmt> {
        if self.is_kw("access") {
            return self.access_method_stmt(verb);
        }
        if self.is_kw("text") {
            return self.tsearch_stmt(verb);
        }
        if !self.is_kw("foreign") && !self.is_kw("server") && !self.is_kw("user") {
            return self.named_stmt(verb);
        }
        self.sql_med(verb)
    }
}

thread_local! {
    /// The event whose triggers are running (its name and the command's tag): what TG_EVENT and TG_TAG read, and
    /// what keeps the DDL an event trigger runs from firing more.
    pub(super) static EVENT: std::cell::RefCell<Option<(String, String)>> = const { std::cell::RefCell::new(None) };
    /// A command that fires event triggers is running: the statements it runs itself (the drops a CASCADE makes, a
    /// schema's objects) fire none of their own, as Postgres fires a command's triggers once.
    pub(super) static IN_COMMAND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a command tag is one of what every database shares (databases, roles, tablespaces) or of event triggers,
/// for which event triggers don't fire.
pub(super) fn shared_tag(tag: &str) -> bool {
    let what = tag.split_once(' ').map_or("", |(_, w)| w);
    matches!(what, "DATABASE" | "ROLE" | "USER" | "GROUP" | "TABLESPACE" | "EVENT TRIGGER" | "SYSTEM")
}

/// Whether a command tag names a command event triggers can fire for (a DDL command's: what `WHEN TAG IN` takes).
fn known_tag(tag: &str) -> bool {
    ["CREATE ", "ALTER ", "DROP ", "COMMENT", "GRANT", "REVOKE", "SECURITY LABEL", "IMPORT FOREIGN SCHEMA"]
        .iter()
        .any(|v| tag.starts_with(v))
        || matches!(tag, "REFRESH MATERIALIZED VIEW" | "SELECT INTO")
}

/// CREATE EVENT TRIGGER's function, from the trigger's row: there, taking nothing, and returning event_trigger (kept
/// as a function returning nothing), as Postgres requires it.
pub(super) fn event_function(v: &View, json: Option<&str>) -> Result<()> {
    let j: Json = json.and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
    let name = func_named(j["func"].as_str().unwrap_or_default());
    match functions::named(v, &name)?.iter().find(|f| f.args.is_empty()) {
        None => err(NO_FUNCTION, format!("function {}() does not exist", written(&name))),
        Some(f) if !matches!(f.ret, functions::Ret::Void) => {
            err("42P17", format!("function {} must return type event_trigger", written(&name)))
        }
        Some(_) => Ok(()),
    }
}

/// The event triggers made here, by name, with their rows (what `pg_event_trigger` lists).
pub(super) fn event_triggers(v: &View) -> Result<Vec<(String, Json)>> {
    let at = format!("{PREFIX}evt/");
    let mut out = vec![];
    for r in v.prefix(&at)? {
        let (k, raw) = r?;
        out.push((k[at.len()..].to_string(), serde_json::from_str(&raw).unwrap_or_default()));
    }
    Ok(out)
}

const EVENTS: [&str; 4] = ["ddl_command_start", "ddl_command_end", "sql_drop", "table_rewrite"];

impl Parser {
    /// After CREATE EVENT: `TRIGGER name ON event [WHEN TAG IN ('t', ...)] EXECUTE FUNCTION f()`.
    pub(super) fn create_event_trigger(&mut self) -> Result<Stmt> {
        self.expect_kw("trigger")?;
        let name = self.name()?;
        self.expect_kw("on")?;
        let event = self.name()?.to_lowercase();
        if !EVENTS.contains(&event.as_str()) {
            return err(SYNTAX, format!("unrecognized event name \"{event}\""));
        }
        let mut tags: Vec<String> = vec![];
        if self.kw("when") {
            loop {
                let filter = self.name()?;
                if filter.to_lowercase() != "tag" {
                    return err(SYNTAX, format!("unrecognized filter variable \"{filter}\""));
                }
                self.expect_kw("in")?;
                self.expect_sym("(")?;
                loop {
                    match self.next() {
                        Some(Tok::Str(t)) => {
                            // (a tag is checked as Postgres checks it: known, and of a command that fires them)
                            let tag = t.to_uppercase();
                            if !known_tag(&tag) {
                                return err(
                                    SYNTAX,
                                    format!("filter value \"{t}\" not recognized for filter variable \"{filter}\""),
                                );
                            }
                            if shared_tag(&tag) {
                                return err(UNSUPPORTED, format!("event triggers are not supported for {t}"));
                            }
                            tags.push(tag)
                        }
                        _ => return self.fail("expected a command tag"),
                    }
                    if !self.sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
                if !self.kw("and") {
                    break;
                }
            }
        }
        self.expect_kw("execute")?;
        if !(self.kw("function") || self.kw("procedure")) {
            return self.fail("expected FUNCTION or PROCEDURE");
        }
        let func = self.qualified(false, Kind::Func)?;
        self.expect_sym("(")?;
        self.expect_sym(")")?;
        // (its owner: who makes it, a superuser, as Postgres records it)
        let owner = super::roles::current().map_or(super::roles::BOOTSTRAP, |w| w.oid);
        let json = serde_json::json!({ "event": event, "tags": tags, "func": func, "enabled": true, "owner": owner });
        Ok(Stmt::UserDef {
            needs: None,
            key: format!("{PREFIX}evt/{name}"),
            json: Some(json.to_string()),
            tag: "CREATE EVENT TRIGGER",
            missing: None,
            exists: Some(format!("event trigger \"{name}\" already exists")),
            cascade: false,
        })
    }

    /// After DROP EVENT TRIGGER or ALTER EVENT TRIGGER (EVENT taken).
    pub(super) fn event_trigger_stmt(&mut self, drop: bool) -> Result<Stmt> {
        self.expect_kw("trigger")?;
        let if_exists = self.guard(false)?;
        let name = self.name()?;
        let key = format!("{PREFIX}evt/{name}");
        let missing = (!if_exists).then(|| format!("event trigger \"{name}\" does not exist"));
        if drop {
            self.cascade();
            return Ok(Stmt::UserDef {
                needs: None,
                key,
                json: None,
                tag: "DROP EVENT TRIGGER",
                missing,
                exists: None,
                cascade: false,
            });
        }
        let json = if self.kw("disable") {
            Some(r#"{"enabled":false}"#.to_string())
        } else if self.kw("enable") {
            if !self.kw("replica") {
                self.kw("always");
            }
            Some(r#"{"enabled":true}"#.to_string())
        } else {
            None // OWNER TO, RENAME TO: nothing is kept of them
        };
        self.skip_rest();
        Ok(Stmt::UserDef { needs: None, key, json, tag: "ALTER EVENT TRIGGER", missing, exists: None, cascade: false })
    }
}

/// Runs the event triggers of `event` that a command tagged `tag` fires, in name order. DDL an event trigger's own
/// function runs fires nothing.
pub(super) fn fire(db: &Db, branch: &str, event: &str, tag: &str) -> Result<()> {
    if EVENT.with(|e| e.borrow().is_some())
        || !(tag.starts_with("CREATE") || tag.starts_with("ALTER") || tag.starts_with("DROP"))
    {
        return Ok(());
    }
    if tag.contains("EVENT TRIGGER") {
        return Ok(());
    }
    let v = db.view_tx(branch)?;
    let mut due: Vec<(String, String)> = vec![];
    for r in v.prefix(&format!("{PREFIX}evt/"))? {
        let (k, raw) = r?;
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        // (no WHEN TAG IN: every command's)
        let tags = j["tags"].as_array().filter(|t| !t.is_empty());
        let tagged = tags.is_none_or(|t| t.iter().filter_map(|x| x.as_str()).any(|x| x == tag));
        if j["event"] == event && j["enabled"] != false && tagged {
            due.push((k, j["func"].as_str().unwrap_or_default().to_string()));
        }
    }
    for (_, func) in due {
        let name = func_named(&func);
        let f = functions::named(&v, &name)?.iter().find(|f| f.args.is_empty()).cloned();
        let Some(f) = f else { return err(NO_FUNCTION, format!("function {}() does not exist", written(&name))) };
        let call = Expr::Call(Box::new(Call {
            name,
            args: vec![],
            names: vec![],
            def: Some(f),
            via_call: false,
            builtin: false,
        }));
        let st = Statement(Stmt::Select(Box::new(select_of(call))), PlanCache::default(), true);
        EVENT.with(|e| *e.borrow_mut() = Some((event.to_string(), tag.to_string())));
        let r = st.execute(db, branch, &[], &[]);
        EVENT.with(|e| *e.borrow_mut() = None);
        r?;
    }
    Ok(())
}

/// `CREATE FOREIGN TABLE`'s tokens (from TABLE on, before SERVER) and the tokens from SERVER on, as the SQL of the
/// view that reads the file they name; None when there is no `filename` option.
pub(super) fn file_table_view(head: &[Tok], tail: &[Tok]) -> Option<String> {
    let word = |t: &Tok| match t {
        Tok::Word(w) | Tok::Str(w) => Some(w.to_lowercase()),
        _ => None,
    };
    // OPTIONS (key 'value', ...)
    let at = tail.iter().position(|t| matches!(t, Tok::Word(w) if w == "options"))?;
    let mut opts: Vec<(String, String)> = vec![];
    let mut k = at + 2;
    while k + 1 < tail.len() && tail[k] != Tok::Sym(")") {
        if let (Some(key), Tok::Str(v) | Tok::Word(v)) = (word(&tail[k]), &tail[k + 1]) {
            opts.push((key, v.clone()));
        }
        k += 2;
        if tail.get(k) == Some(&Tok::Sym(",")) {
            k += 1;
        }
    }
    let opt = |n: &str| opts.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    let file = opt("filename")?;
    // name, then the column list
    let open = head.iter().position(|t| *t == Tok::Sym("("))?;
    let name_toks: Vec<Tok> =
        head[..open].iter().filter(|t| !matches!(t, Tok::Word(w) if w == "table")).cloned().collect();
    let (mut depth, mut cols, mut cur): (i32, Vec<Vec<Tok>>, Vec<Tok>) = (0, vec![], vec![]);
    for t in &head[open..] {
        match t {
            Tok::Sym("(") => {
                depth += 1;
                if depth > 1 {
                    cur.push(t.clone());
                }
            }
            Tok::Sym(")") => {
                depth -= 1;
                if depth >= 1 {
                    cur.push(t.clone());
                } else {
                    break;
                }
            }
            Tok::Sym(",") if depth == 1 => cols.push(std::mem::take(&mut cur)),
            t => cur.push(t.clone()),
        }
    }
    if !cur.is_empty() {
        cols.push(cur);
    }
    let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let stop = ["not", "null", "default", "options", "collate", "constraint", "check"];
    let mut items = vec![];
    for (n, c) in cols.iter().enumerate() {
        let ty_end = c
            .iter()
            .skip(1)
            .position(|t| matches!(t, Tok::Word(w) if stop.contains(&w.as_str())))
            .map_or(c.len(), |p| p + 1);
        items.push(format!("(f[{}])::{} as {}", n + 1, tokens_shown(&c[1..ty_end]), tokens_shown(&c[..1])));
    }
    let format = opt("format").unwrap_or_else(|| "text".into());
    Some(format!(
        "create view {} as select {} from __file_rows({}, {}, {}, {}, {}, {}) as c(f)",
        tokens_shown(&name_toks).trim().trim_start_matches("if not exists").trim(),
        items.join(", "),
        q(&file),
        q(&format),
        q(&opt("header").unwrap_or_else(|| "false".into())),
        q(&opt("delimiter").unwrap_or_default()),
        q(&opt("quote").unwrap_or_default()),
        opt("null").map_or("null".to_string(), |n| q(&n)),
    ))
}

const TS_KINDS: [&str; 4] = ["dictionary", "configuration", "parser", "template"];

impl Parser {
    /// `name = value, ...` inside parentheses: options, each a name and its value text.
    fn ts_options(&mut self) -> Result<Vec<(String, String)>> {
        self.expect_sym("(")?;
        let mut out = vec![];
        loop {
            let key = self.name()?.to_lowercase();
            let value = if self.sym("=") {
                match self.next() {
                    Some(Tok::Str(v) | Tok::Num(v) | Tok::Word(v) | Tok::Ident(v)) => {
                        let mut v = v;
                        while self.sym(".") {
                            if let Some(Tok::Str(w) | Tok::Word(w) | Tok::Ident(w)) = self.next() {
                                v = w; // (a schema's name before it is left)
                            }
                        }
                        v
                    }
                    _ => return self.fail("expected a value"),
                }
            } else {
                String::new()
            };
            out.push((key, value));
            if !self.sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(out)
    }

    fn ts_name(&mut self) -> Result<String> {
        let mut n = self.name()?;
        while self.sym(".") {
            n = self.name()?;
        }
        Ok(n)
    }

    /// CREATE, ALTER and DROP TEXT SEARCH DICTIONARY | CONFIGURATION | PARSER | TEMPLATE.
    pub(super) fn tsearch_stmt(&mut self, verb: &str) -> Result<Stmt> {
        self.expect_kw("text")?;
        self.expect_kw("search")?;
        let Some(kind) = TS_KINDS.iter().find(|k| self.kw(k)) else {
            return self.fail("expected DICTIONARY, CONFIGURATION, PARSER or TEMPLATE");
        };
        let tag: &'static str =
            Box::leak(format!("{} TEXT SEARCH {}", verb.to_uppercase(), kind.to_uppercase()).into_boxed_str());
        let if_exists = verb == "drop" && self.guard(false)?;
        let name = self.ts_name()?;
        let key = format!("{PREFIX}ts{kind}/{name}");
        let what = format!("text search {kind} \"{name}\"");
        let missing = Some(match if_exists {
            true => format!("{SKIP}{what} does not exist, skipping"),
            false => format!("{what} does not exist"),
        });
        if verb == "drop" {
            self.cascade();
            return Ok(Stmt::UserDef { needs: None, key, json: None, tag, missing, exists: None, cascade: false });
        }
        if verb == "create" {
            let opts = self.ts_options()?;
            let get = |k: &str| opts.iter().find(|(o, _)| o == k).map(|(_, v)| v.to_lowercase());
            let mut needs = None;
            let json = match *kind {
                "dictionary" => {
                    let Some(template) = get("template") else {
                        return err("42P17", "text search template is required");
                    };
                    if !matches!(template.as_str(), "simple" | "snowball" | "synonym" | "ispell" | "thesaurus") {
                        return err("42704", format!("text search template \"{template}\" does not exist"));
                    }
                    let options: serde_json::Map<String, Json> = opts
                        .iter()
                        .filter(|(k, _)| k != "template")
                        .map(|(k, v)| (k.clone(), Json::from(v.clone())))
                        .collect();
                    // (the files are read now, so a missing one is an error here)
                    ts_dict_of(&template, &options).map_err(|m| SqlError { code: "22023", message: m, info: None })?;
                    serde_json::json!({ "template": template, "options": options })
                }
                "configuration" => {
                    let maps: Vec<(String, Vec<String>)> = if let Some(base) = get("copy") {
                        needs = if matches!(base.as_str(), "english" | "simple") {
                            None
                        } else {
                            Some((
                                format!("{PREFIX}tsconfiguration/{base}"),
                                format!("text search configuration \"{base}\" does not exist"),
                            ))
                        };
                        ts_maps_of(&base)
                    } else {
                        if get("parser").is_some_and(|p| p != "default") {
                            return err("42704", "text search parser doesn't exist here: use default");
                        }
                        vec![]
                    };
                    serde_json::json!({ "maps": maps.into_iter().collect::<Vec<_>>() })
                }
                _ => serde_json::json!({ "options": opts }),
            };
            return Ok(Stmt::UserDef {
                needs,
                key,
                json: Some(json.to_string()),
                tag,
                missing: None,
                exists: Some(format!("{what} already exists")),
                cascade: false,
            });
        }
        // ALTER
        let json = match *kind {
            "configuration" if self.is_kw("add") || self.is_kw("alter") || self.is_kw("drop") => {
                let op = if self.kw("add") {
                    "add"
                } else if self.kw("alter") {
                    "alter"
                } else {
                    self.next();
                    "drop"
                };
                self.expect_kw("mapping")?;
                let ifex = op == "drop" && self.guard(false)?;
                let mut kinds = vec![];
                if self.kw("for") {
                    loop {
                        kinds.push(self.name()?.to_lowercase());
                        if !self.sym(",") {
                            break;
                        }
                    }
                }
                let (mut dicts, mut replace) = (vec![], None);
                if self.kw("replace") {
                    let old = self.ts_name()?;
                    self.expect_kw("with")?;
                    replace = Some((old, self.ts_name()?));
                } else if self.kw("with") {
                    loop {
                        dicts.push(self.ts_name()?);
                        if !self.sym(",") {
                            break;
                        }
                    }
                }
                Some(serde_json::json!({ "op": op, "kinds": kinds, "dicts": dicts, "replace": replace, "if_exists": ifex }).to_string())
            }
            "dictionary" if matches!(self.peek(), Some(Tok::Sym("("))) => {
                let opts = self.ts_options()?;
                Some(serde_json::json!({ "op": "options", "options": opts }).to_string())
            }
            _ => {
                self.skip_rest();
                None
            }
        };
        let json = json.or_else(|| Some(String::new())).filter(|j| !j.is_empty());
        Ok(Stmt::UserDef {
            needs: None,
            key,
            json,
            tag,
            missing: Some(format!("{what} does not exist")),
            exists: None,
            cascade: false,
        })
    }
}

/// The default mappings of a built-in configuration, or of a user's, as (token class, dictionaries).
pub(super) fn ts_maps_of(name: &str) -> Vec<(String, Vec<String>)> {
    use super::tsearch as ts;
    let cfg = ts::config(name).ok();
    let Some(cfg) = cfg else { return vec![] };
    ts::TOKEN_TYPES
        .iter()
        .filter_map(|(alias, _)| {
            let d = ts::mapped(cfg, alias);
            (!d.is_empty()).then(|| (alias.to_string(), d))
        })
        .collect()
}

/// A dictionary from its template and options (files read).
pub(super) fn ts_dict_of(
    template: &str,
    opts: &serde_json::Map<String, Json>,
) -> std::result::Result<super::tsearch::Dict, String> {
    use super::tsearch as ts;
    let get =
        |k: &str| opts.iter().find(|(o, _)| o.eq_ignore_ascii_case(k)).and_then(|(_, v)| v.as_str()).map(String::from);
    let stop = |f: Option<String>| -> std::result::Result<Vec<String>, String> {
        match f {
            Some(f) => ts::stop_file(&f),
            None => Ok(vec![]),
        }
    };
    Ok(match template {
        "simple" => ts::Dict::Simple {
            stop: stop(get("stopwords"))?,
            accept: get("accept")
                .is_none_or(|a| !matches!(a.to_lowercase().as_str(), "false" | "off" | "no" | "0" | "f")),
        },
        "snowball" => {
            let lang = get("language")
                .ok_or_else(|| "text search dictionary \"snowball\" needs the Language parameter".to_string())?;
            if !lang.eq_ignore_ascii_case("english") {
                return Err(format!("no Snowball stemmer for language \"{lang}\" here: english only"));
            }
            ts::Dict::Snowball { stop: stop(get("stopwords"))? }
        }
        "ispell" => {
            let (Some(df), Some(af)) = (get("dictfile"), get("afffile")) else {
                return Err("missing DictFile or AffFile parameter".into());
            };
            let dict = ts::data_file(&df, "dict").map_err(|e| e.replace("stop-word file", "dictionary file"))?;
            let aff = ts::data_file(&af, "affix").map_err(|e| e.replace("stop-word file", "affix file"))?;
            ts::Dict::Ispell {
                d: Box::new(
                    super::ispell::Ispell::load(&dict, &aff)
                        .map_err(|e| format!("invalid affix or dictionary file: {e}"))?,
                ),
                stop: stop(get("stopwords"))?,
            }
        }
        "thesaurus" => {
            let file = get("dictfile").ok_or_else(|| "missing DictFile parameter".to_string())?;
            let sub = get("dictionary").ok_or_else(|| "missing Dictionary parameter".to_string())?;
            ts::lexize(&sub, "x")?;
            ts::Dict::Thesaurus { rules: ts::thesaurus_file(&file, &sub)?, sub }
        }
        _ => {
            let cs = get("casesensitive")
                .is_some_and(|a| matches!(a.to_lowercase().as_str(), "true" | "on" | "yes" | "1" | "t"));
            let file = get("synonyms").ok_or_else(|| "missing Synonyms parameter".to_string())?;
            ts::Dict::Synonym { map: ts::synonym_file(&file, cs)?, case_sensitive: cs }
        }
    })
}

/// Applies an ALTER TEXT SEARCH ... change to a stored row.
pub(super) fn ts_apply(row: &mut Json, change: &Json, key: &str) -> Result<()> {
    let op = change["op"].as_str().unwrap_or_default();
    let strs = |j: &Json| {
        j.as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    match op {
        "options" => {
            let template = row["template"].as_str().unwrap_or("simple").to_string();
            let mut o = row["options"].as_object().cloned().unwrap_or_default();
            for kv in change["options"].as_array().into_iter().flatten() {
                if let (Some(k), Some(v)) = (kv[0].as_str(), kv[1].as_str()) {
                    o.insert(k.to_string(), Json::from(v));
                }
            }
            ts_dict_of(&template, &o).map_err(|m| SqlError { code: "22023", message: m, info: None })?;
            row["options"] = Json::Object(o);
        }
        "add" | "alter" | "drop" => {
            let mut maps: Vec<(String, Vec<String>)> = row["maps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| Some((m[0].as_str()?.to_string(), strs(&m[1]))))
                .collect();
            let kinds = strs(&change["kinds"]);
            let dicts = strs(&change["dicts"]);
            for k in &kinds {
                if !super::tsearch::TOKEN_TYPES.iter().any(|(a, _)| a == k) {
                    return err("22023", format!("token type \"{k}\" does not exist"));
                }
            }
            for d in &dicts {
                if super::tsearch::lexize(d, "x").is_err() {
                    return err("42704", format!("text search dictionary \"{d}\" does not exist"));
                }
            }
            let name = key.rsplit('/').next().unwrap_or_default();
            match op {
                "add" => {
                    for k in &kinds {
                        match maps.iter_mut().find(|(m, _)| m == k) {
                            Some(m) => m.1.extend(dicts.clone()),
                            None => maps.push((k.clone(), dicts.clone())),
                        }
                    }
                }
                "alter" => {
                    let all: Vec<String> =
                        if kinds.is_empty() { maps.iter().map(|m| m.0.clone()).collect() } else { kinds.clone() };
                    for k in &all {
                        let Some(m) = maps.iter_mut().find(|(m, _)| m == k) else {
                            return err("42704", format!("mapping for token type \"{k}\" does not exist"));
                        };
                        match change["replace"].as_array() {
                            Some(r) => {
                                let (old, new) = (r[0].as_str().unwrap_or_default(), r[1].as_str().unwrap_or_default());
                                for d in m.1.iter_mut().filter(|d| *d == old) {
                                    *d = new.to_string();
                                }
                            }
                            None => m.1 = dicts.clone(),
                        }
                    }
                }
                _ => {
                    for k in &kinds {
                        if !maps.iter().any(|(m, _)| m == k) && change["if_exists"] != true {
                            return err("42704", format!("mapping for token type \"{k}\" does not exist"));
                        }
                        maps.retain(|(m, _)| m != k);
                    }
                }
            }
            let _ = name;
            row["maps"] = serde_json::json!(maps);
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------- aggregates

/// An aggregate's signature as a statement writes it.
#[derive(Default)]
pub(super) struct AggSig {
    /// its arguments' types: the direct ones, then the aggregated ones (an ordered-set aggregate whose direct
    /// arguments end VARIADIC has those alone, as Postgres keeps it)
    pub(super) args: Vec<Type>,
    /// an ordered-set aggregate's number of direct arguments (None for another kind)
    pub(super) direct: Option<usize>,
    /// its last argument is VARIADIC
    pub(super) variadic: bool,
    /// written `(*)`: no arguments
    pub(super) star: bool,
    /// the types as written, as DROP ... IF EXISTS's notice names them (`pg_catalog.int4` for `int`)
    pub(super) written: Vec<String>,
}

impl AggSig {
    /// The types for a message: `*` for none written so, else `integer, text`.
    fn shown(&self) -> String {
        match self.star {
            true => "*".into(),
            false => self.args.iter().map(|t| pg_type(&t.name())).collect::<Vec<_>>().join(", "),
        }
    }
}

/// Whether an aggregate's argument ends here: at `,`, `)` or ORDER BY.
fn arg_ends(p: &Parser) -> bool {
    matches!(p.peek(), Some(Tok::Sym("," | ")")) | None)
        || (p.is_kw("order") && matches!(p.t.get(p.i + 1), Some(Tok::Word(w)) if w == "by"))
}

/// A type written with an SQL keyword as Postgres names it in a DROP ... IF EXISTS notice (`int` is
/// `pg_catalog.int4`); None for another (named as it is).
fn written_type(t: Option<&Tok>) -> Option<String> {
    let Some(Tok::Word(w)) = t else { return None };
    let n = match w.as_str() {
        "int" | "integer" => "int4",
        "smallint" => "int2",
        "bigint" => "int8",
        "real" => "float4",
        "float" | "double" => "float8",
        "numeric" | "decimal" | "dec" => "numeric",
        "boolean" => "bool",
        "varchar" => "varchar",
        "char" | "character" => "bpchar",
        _ => return None,
    };
    Some(format!("pg_catalog.{n}"))
}

/// Before a message that something isn't there: the statement said IF EXISTS, so it is a notice, not an error.
pub(super) const SKIP: &str = "\u{1}skip:";

/// For a type a statement named that wasn't looked up yet (an unresolved name): the row it needs, and Postgres's
/// error if it isn't there, or with IF EXISTS (`skip`) its notice (see `SKIP`).
fn unresolved_needs(t: Type, skip: bool) -> Option<(String, String)> {
    let Type::Enum(i) = t else { return None };
    if i & UNRESOLVED_BIT == 0 {
        return None;
    }
    let n = unresolved_name(i)?;
    let m = format!("type \"{}\" does not exist", shown_type(&n));
    Some((type_key(&n), if skip { format!("{SKIP}{m}, skipping") } else { m }))
}

/// The row of aggregate `name` with arguments of these types.
pub(super) fn agg_key(name: &str, args: &[Type]) -> String {
    let names: Vec<String> = args.iter().map(|t| t.name().to_string()).collect();
    format!("{PREFIX}agg/{name}({})", names.join(","))
}

/// An aggregate's signature from its row key, `name(int,text)`: what its privileges are kept by (see `acl::aggregate`).
pub(super) fn agg_sig(key: &str) -> &str {
    key.strip_prefix(PREFIX).and_then(|k| k.strip_prefix("agg/")).unwrap_or(key)
}

/// An aggregate's row key read back: its name, and its arguments' types.
fn key_parts(key: &str) -> (String, Vec<String>) {
    let rest = key.strip_prefix(&format!("{PREFIX}agg/")).unwrap_or(key);
    let (name, args) = rest.split_once('(').unwrap_or((rest, ")"));
    let args = args.strip_suffix(')').unwrap_or(args);
    (name.to_string(), if args.is_empty() { vec![] } else { args.split(',').map(String::from).collect() })
}

/// A type's name as the lookup of an aggregate's functions compares it: Chronos's (`timestamp`, `any`, `text` for
/// varchar) for Postgres's.
fn canon(t: &str) -> String {
    if let Some(e) = t.strip_suffix("[]") {
        return format!("{}[]", canon(e));
    }
    match t {
        "\"any\"" => "any",
        "timestamp without time zone" => "timestamp",
        "timestamp with time zone" => "timestamptz",
        "time without time zone" => "time",
        "character varying" | "character" | "bpchar" | "varchar" => "text",
        t => t,
    }
    .to_string()
}

/// A type's name as Postgres's messages write it (`format_type`).
fn pg_type(t: &str) -> String {
    if let Some(e) = t.strip_suffix("[]") {
        return format!("{}[]", pg_type(e));
    }
    match t {
        "any" => "\"any\"",
        "timestamp" => "timestamp without time zone",
        "timestamptz" => "timestamp with time zone",
        "time" => "time without time zone",
        t => t,
    }
    .to_string()
}

fn pg_types(ts: &[String], sep: &str) -> String {
    ts.iter().map(|t| pg_type(t)).collect::<Vec<_>>().join(sep)
}

/// The polymorphic types each of which a call's arguments make the same type of (and its elements' or arrays').
const FAMILY1: [&str; 6] = ["anyelement", "anyarray", "anynonarray", "anyenum", "anyrange", "anymultirange"];
const FAMILY2: [&str; 5] =
    ["anycompatible", "anycompatiblearray", "anycompatiblenonarray", "anycompatiblerange", "anycompatiblemultirange"];

fn poly(t: &str) -> bool {
    FAMILY1.contains(&t) || FAMILY2.contains(&t)
}

/// Whether `t` is a built-in type whose conversions the lookup knows: one of the user's (a domain, a composite, an
/// enum) is taken as it is.
fn known(t: &str) -> bool {
    let t = t.strip_suffix("[]").unwrap_or(t);
    poly(t)
        || matches!(
            t,
            "any"
                | "internal"
                | "smallint"
                | "integer"
                | "bigint"
                | "numeric"
                | "real"
                | "double precision"
                | "text"
                | "boolean"
                | "date"
                | "timestamp"
                | "timestamptz"
                | "time"
                | "interval"
                | "bytea"
                | "oid"
                | "jsonb"
                | "uuid"
                | "cstring"
                | "void"
                | "record"
                | "name"
                | "money"
        )
}

/// Whether a value of type `a` is one of type `b` as it is (Postgres's binary coercion, as far as it matters here):
/// the same type, or one of the user's.
fn same(a: &str, b: &str) -> bool {
    a == b || !known(a) || !known(b)
}

/// Whether Postgres converts `from` to `to` implicitly: a number to a wider one, a date to a timestamp.
fn implicit(from: &str, to: &str) -> bool {
    const NUMBERS: [&str; 6] = ["smallint", "integer", "bigint", "numeric", "real", "double precision"];
    match (NUMBERS.iter().position(|n| *n == from), NUMBERS.iter().position(|n| *n == to)) {
        (Some(a), Some(b)) => a < b,
        _ => matches!((from, to), ("date", "timestamp" | "timestamptz") | ("timestamp", "timestamptz")),
    }
}

/// Why polymorphic type `t` can't follow from arguments of these types (check_valid_polymorphic_signature's
/// detail); None if it can, or isn't polymorphic.
fn poly_detail(t: &str, args: &[String]) -> Option<String> {
    let (need, inputs): (&[&str], &str) = match t {
        "anyrange" | "anymultirange" => (&["anyrange", "anymultirange"][..], "anyrange or anymultirange"),
        "anycompatiblerange" | "anycompatiblemultirange" => {
            (&["anycompatiblerange", "anycompatiblemultirange"][..], "anycompatiblerange or anycompatiblemultirange")
        }
        t if FAMILY1.contains(&t) => {
            (&FAMILY1[..], "anyelement, anyarray, anynonarray, anyenum, anyrange, or anymultirange")
        }
        t if FAMILY2.contains(&t) => (
            &FAMILY2[..],
            "anycompatible, anycompatiblearray, anycompatiblenonarray, anycompatiblerange, or anycompatiblemultirange",
        ),
        _ => return None,
    };
    let given = args.iter().any(|a| need.contains(&a.as_str()));
    (!given).then(|| format!("A result of type {t} requires at least one input of type {inputs}."))
}

/// An error with a DETAIL or a HINT.
fn told(code: &'static str, message: &str, detail: Option<String>, hint: Option<String>) -> SqlError {
    SqlError { code, message: message.into(), info: Some(Box::new(Violation { detail, hint, ..Violation::default() })) }
}

/// A function an aggregate may name, as the lookup of its support functions sees it: its argument and result types
/// (as `canon` writes them), whether it is strict, whether it returns a set.
struct Cand {
    args: Vec<String>,
    ret: String,
    strict: bool,
    set: bool,
}

/// The functions `stored` (a name as `Parser::qualified` keeps it) may be: the user's, then Postgres's built-ins.
fn candidates(v: &View, stored: &str) -> Result<Vec<Cand>> {
    let lookup = if stored.contains('.') { stored.to_string() } else { func_named(stored) };
    let mut out = vec![];
    for f in functions::named(v, &lookup)? {
        if f.procedure {
            continue;
        }
        let ret = match &f.ret {
            functions::Ret::Value(t) | functions::Ret::SetOf(t) => t.name().to_string(),
            functions::Ret::Void => "void".into(),
            functions::Ret::Trigger => "trigger".into(),
            _ => "record".into(),
        };
        out.push(Cand {
            args: f.args.iter().map(|a| canon(&a.1.name())).collect(),
            ret: canon(&ret),
            strict: f.strict,
            set: f.ret.sets(),
        });
    }
    for (args, ret, strict, _) in catalog_schema::builtin_signatures(rel_part(written(stored))) {
        let set = ret.starts_with("SETOF ");
        out.push(Cand {
            args: args.iter().map(|a| canon(a)).collect(),
            ret: canon(ret.trim_start_matches("SETOF ")),
            strict,
            set,
        });
    }
    Ok(out)
}

/// How `c` takes arguments of types `given`, as Postgres's func_get_detail matches them (VARIADIC not expanded):
/// its argument types and result type with the polymorphic ones made what the given ones say, and whether each
/// argument is one of its type as it is (false: one needs converting). None if it can't take them.
fn takes(c: &Cand, given: &[String]) -> Option<(Vec<String>, String, bool)> {
    if c.args.len() != given.len() {
        return None;
    }
    // what the polymorphic parameters are given: anyelement's type, anyarray's, anycompatible's
    let (mut elem, mut arr, mut compat): (Option<&str>, Option<&str>, Vec<&str>) = (None, None, vec![]);
    for (d, g) in c.args.iter().zip(given) {
        let (d, g) = (d.as_str(), g.as_str());
        match d {
            "anyelement" | "anynonarray" | "anyenum" => {
                if elem.is_some_and(|e| e != g) {
                    return None;
                }
                elem = Some(g);
            }
            "anyarray" => {
                if arr.is_some_and(|a| a != g) {
                    return None;
                }
                arr = Some(g);
            }
            "anycompatible" | "anycompatiblenonarray" => compat.push(g),
            "anycompatiblearray" => match g.strip_suffix("[]") {
                Some(e) => compat.push(e),
                None if g == "anycompatiblearray" => compat.push("anycompatible"),
                None => return None,
            },
            _ if poly(d) || d == "any" => {}
            // (a polymorphic type goes only where one is declared)
            _ if poly(g) => return None,
            _ => {}
        }
    }
    // an array's elements are anyelement's type (anyarray given for anyarray: left to the call)
    if let Some(a) = arr
        && a != "anyarray"
    {
        let e = a.strip_suffix("[]")?;
        if elem.is_some_and(|x| x != e) {
            return None;
        }
        elem = Some(e);
    }
    // anycompatible's: the one the others convert to
    let common: Option<String> = match compat.first() {
        None => None,
        Some(first) if compat.iter().all(|t| t == first) => Some(first.to_string()),
        Some(_) => Some(compat.iter().find(|t| compat.iter().all(|o| o == *t || implicit(o, t)))?.to_string()),
    };
    let elem: Option<String> = elem.map(String::from);
    let sub = |d: &str| -> String {
        match d {
            "anyelement" | "anynonarray" | "anyenum" => elem.clone().unwrap_or_else(|| d.to_string()),
            "anyarray" => match (arr, &elem) {
                (Some(a), _) => a.to_string(),
                (None, Some(e)) if poly(e) => "anyarray".into(),
                (None, Some(e)) => format!("{e}[]"),
                (None, None) => d.to_string(),
            },
            "anycompatible" | "anycompatiblenonarray" => common.clone().unwrap_or_else(|| d.to_string()),
            "anycompatiblearray" => match &common {
                Some(c) if poly(c) => "anycompatiblearray".into(),
                Some(c) => format!("{c}[]"),
                None => d.to_string(),
            },
            d => d.to_string(),
        }
    };
    let args: Vec<String> = c.args.iter().map(|d| sub(d)).collect();
    let ret = sub(&c.ret);
    let mut exact = true;
    for ((d, t), g) in c.args.iter().zip(&args).zip(given) {
        if d == "any" || same(g, t) {
            continue;
        }
        if !implicit(g, t) {
            return None;
        }
        exact = false;
    }
    Some((args, ret, exact))
}

/// Support function `f` of an aggregate (its name as kept, and as written) looked up for arguments of types
/// `given`, as Postgres's lookup_agg_function does: its result type (the polymorphic made the arguments') and
/// whether it is strict.
fn agg_function(v: &View, f: &(String, String), given: &[String]) -> Result<(String, bool)> {
    let cands = candidates(v, &f.0)?;
    let found: Vec<(Vec<String>, String, bool, &Cand)> =
        cands.iter().filter_map(|c| takes(c, given).map(|(a, r, x)| (a, r, x, c))).collect();
    let Some(best) = found.iter().find(|x| x.2).or(found.first()) else {
        return err(NO_FUNCTION, format!("function {}({}) does not exist", f.1, pg_types(given, ", ")));
    };
    if best.3.set {
        return err("42804", format!("function {}({}) returns a set", f.1, pg_types(given, ", ")));
    }
    // (an aggregate's state machinery passes values as they are)
    if !best.2 {
        return err("42804", format!("function {}({}) requires run-time type coercion", f.1, pg_types(&best.0, ", ")));
    }
    Ok((best.1.clone(), best.3.strict))
}

/// Whether a function or procedure (not an aggregate) of this name takes arguments of these types: Some(whether
/// it is a procedure).
fn plain_function(v: &View, name: &str, types: &[String]) -> Result<Option<bool>> {
    Ok(functions::named(v, &func_named(name))?
        .iter()
        .find(|f| f.args.len() == types.len() && f.args.iter().zip(types).all(|(a, t)| canon(&a.1.name()) == canon(t)))
        .map(|f| f.procedure))
}

/// CREATE, ALTER and DROP AGGREGATE as they run (see `Exec::user_def`).
fn agg_stmt(
    v: &View,
    key: &str,
    json: &Option<String>,
    missing: &Option<String>,
    tag: &str,
    there: bool,
) -> Result<Vec<(String, Option<String>)>> {
    let mut j: Json = json.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
    let (name, types) = key_parts(key);
    // (only its owner, or its schema's, may drop it, change it or replace it, as Postgres's ownership checks)
    if there
        && (tag != "CREATE AGGREGATE" || j["replace"] == true)
        && !owns_agg(v, key, &name, tag == "ALTER AGGREGATE")?
    {
        // (ALTER's generic ownership check names it a function, as Postgres's does)
        let noun = if tag == "ALTER AGGREGATE" { "function" } else { "aggregate" };
        return err("42501", format!("must be owner of {noun} {}", rel_part(&name)));
    }
    match tag {
        "CREATE AGGREGATE" => {
            // its owner: the role that makes it (one replaced keeps its own)
            let kept = match there {
                true => agg_row(v, key)?["owner"].as_i64(),
                false => roles::current().filter(|_| v.core.has_privileges()).map(|w| w.oid),
            };
            if let Some(o) = kept {
                j["owner"] = Json::from(o);
            }
            create_agg(v, key, j)
        }
        _ if !there => {
            // (a function's signature is no aggregate's)
            if plain_function(v, &name, &types)?.is_some() {
                return err("42809", format!("function {name}({}) is not an aggregate", pg_types(&types, ", ")));
            }
            match missing {
                Some(m) if m.starts_with(SKIP) => {
                    notice("NOTICE", m[SKIP.len()..].to_string());
                    Ok(vec![])
                }
                Some(m) => err(NO_FUNCTION, m.clone()),
                None => Ok(vec![]),
            }
        }
        "DROP AGGREGATE" => {
            forget(key);
            let mut out = vec![(key.to_string(), None)];
            // (and its grants)
            if v.core.has_privileges() {
                out.push((acl::row_key(&acl::key_of(acl::Kind::Function, agg_sig(key))), None));
            }
            Ok(out)
        }
        _ => {
            // ALTER AGGREGATE ... OWNER TO: a role its owner may become (a superuser any)
            if let Some(role) = j["owner_to"].as_str() {
                let reg = roles::registry(v.core)?;
                let Some(new) = reg.named(role) else {
                    return err("42704", format!("role \"{role}\" does not exist"));
                };
                if let Some(who) = roles::current().filter(|w| !w.superuser)
                    && !reg.can_set(who.oid, new.oid)
                {
                    return err("42501", format!("must be able to SET ROLE \"{role}\""));
                }
                let mut row = agg_row(v, key)?;
                row["owner"] = Json::from(new.oid);
                forget(key);
                return Ok(vec![(key.to_string(), Some(row.to_string()))]);
            }
            // ... SET SCHEMA s: the row under the name in that schema
            let moved;
            let (to, schema) = match (j["rename"].as_str(), j["schema"].as_str()) {
                (Some(to), _) => {
                    // (in the schema it would be made in: the search_path's first)
                    let schema = path_schemas()
                        .and_then(|s| s.into_iter().find(|x| !matches!(x.as_str(), "pg_catalog" | "pg_temp" | "$user")))
                        .unwrap_or_else(|| "public".into());
                    (to, schema)
                }
                (None, Some(s)) => {
                    if !namespaces(v)?.iter().any(|n| n == s) {
                        return err("3F000", format!("schema \"{s}\" does not exist"));
                    }
                    moved = qualify(s, rel_part(&name));
                    if moved == name {
                        return Ok(vec![]);
                    }
                    (moved.as_str(), s.to_string())
                }
                _ => return Ok(vec![]),
            };
            // ALTER AGGREGATE ... RENAME TO: the row under its new name
            let new = format!("{PREFIX}agg/{to}({})", types.join(","));
            if v.get(&new)?.is_some() || plain_function(v, to, &types)?.is_some() {
                return err(
                    "42723",
                    format!(
                        "function {}({}) already exists in schema \"{schema}\"",
                        rel_part(to),
                        pg_types(&types, ", ")
                    ),
                );
            }
            let mut row: Json = serde_json::from_str(&v.get(key)?.unwrap_or_default()).unwrap_or_default();
            row["name"] = Json::from(to);
            forget(key);
            Ok(vec![(key.to_string(), None), (new, Some(row.to_string()))])
        }
    }
}

/// Aggregate `key`'s row (null when there is none).
fn agg_row(v: &View, key: &str) -> Result<Json> {
    Ok(v.get(key)?.and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default())
}

/// Aggregate `key`'s owner: the role that made it (one made before roles were, or by no role: the bootstrap
/// superuser).
pub(super) fn agg_owner(v: &View, key: &str) -> Result<i64> {
    Ok(agg_row(v, key)?["owner"].as_i64().unwrap_or(roles::BOOTSTRAP))
}

/// Whether the running role may drop or change aggregate `key` (named `name`): it has its owner's privileges, or
/// its schema's owner's (a superuser always).
fn owns_agg(v: &View, key: &str, name: &str, alter: bool) -> Result<bool> {
    let Some(who) = roles::current().filter(|w| !w.superuser) else { return Ok(true) };
    if !v.core.has_privileges() {
        return Ok(true);
    }
    // (ALTER is its owner's alone; DROP its schema's owner's too)
    Ok(who.has_privs_of(agg_owner(v, key)?) || (!alter && who.has_privs_of(acl::schema(v, schema_part(name))?.owner)))
}

/// What a support function is given: the state's type, then these.
fn given(state: &str, rest: &[String]) -> Vec<String> {
    std::iter::once(state.to_string()).chain(rest.iter().cloned()).collect()
}

/// A strict state function with no initial value starts from the first row's value, which has to be a state: the
/// aggregate's first argument is of the state's type.
fn starts(strict: bool, init: bool, args: &[String], state: &str) -> Result<()> {
    match strict && !init && (args.is_empty() || !same(&args[0], state)) {
        true => err(
            "42P13",
            "must not omit initial value when transition function is strict and transition type is not compatible with input type",
        ),
        false => Ok(()),
    }
}

/// CREATE [OR REPLACE] AGGREGATE as it runs, as Postgres's AggregateCreate checks it: each support function
/// looked up for the types it is given, its result's type checked; then what is there already; then the row, with
/// the type the aggregate gives.
fn create_agg(v: &View, key: &str, mut j: Json) -> Result<Vec<(String, Option<String>)>> {
    let list = |x: &Json| -> Vec<String> {
        x.as_array().into_iter().flatten().filter_map(|s| s.as_str()).map(canon).collect()
    };
    let name = j["name"].as_str().unwrap_or_default().to_string();
    let args = list(&j["args"]);
    let stype = canon(j["stype"].as_str().unwrap_or_default());
    let mstype = j["mstype"].as_str().map(canon);
    let direct = j["direct"].as_u64().map(|d| d as usize);
    let kind = j["kind"].as_str().unwrap_or("n").to_string();
    let replace = j["replace"] == true;
    let (initcond, minitcond) = (j["initcond"].is_string(), j["minitcond"].is_string());
    let extra = j["finalfunc_extra"] == true;
    let funcs: Vec<(&str, (String, String))> =
        ["sfunc", "finalfunc", "combinefunc", "serialfunc", "deserialfunc", "msfunc", "minvfunc", "mfinalfunc"]
            .into_iter()
            .filter_map(|k| {
                let stored = j[k].as_str()?.to_string();
                let shown = j["shown"][k].as_str().map_or_else(|| stored.clone(), String::from);
                Some((k, (stored, shown)))
            })
            .collect();
    let func = |k: &str| funcs.iter().find(|(f, _)| *f == k).map(|(_, f)| f.clone());
    // the transition types have to follow from the arguments
    for t in std::iter::once(&stype).chain(mstype.as_ref()) {
        if let Some(d) = poly_detail(t, &args) {
            return Err(told("42P13", "cannot determine transition data type", Some(d), None));
        }
    }
    // a state function is given the state and the aggregated arguments (an ordered-set aggregate's direct ones
    // aren't, unless they are all it has)
    let aggregated: &[String] = match direct {
        Some(d) if d < args.len() => &args[d..],
        _ => &args[..],
    };
    let sfunc = func("sfunc").unwrap_or_default();
    let (ret, strict) = agg_function(v, &sfunc, &given(&stype, aggregated))?;
    if !same(&ret, &stype) {
        return err("42804", format!("return type of transition function {} is not {}", sfunc.1, pg_type(&stype)));
    }
    starts(strict, initcond, &args, &stype)?;
    if let Some(ms) = &mstype {
        let msfunc = func("msfunc").unwrap_or_default();
        let (r, mstrict) = agg_function(v, &msfunc, &given(ms, aggregated))?;
        if !same(&r, ms) {
            return err("42804", format!("return type of transition function {} is not {}", msfunc.1, pg_type(ms)));
        }
        starts(mstrict, minitcond, &args, ms)?;
        let minvfunc = func("minvfunc").unwrap_or_default();
        let (r, istrict) = agg_function(v, &minvfunc, &given(ms, aggregated))?;
        if !same(&r, ms) {
            return err(
                "42804",
                format!("return type of inverse transition function {} is not {}", minvfunc.1, pg_type(ms)),
            );
        }
        if istrict != mstrict {
            return err("42P13", "strictness of aggregate's forward and inverse transition functions must match");
        }
    }
    // what it gives: the final function's result (it is given the state, and the direct arguments, or with
    // FINALFUNC_EXTRA all of them), else the state
    let mut result = stype.clone();
    if let Some(ff) = func("finalfunc") {
        let n = if extra { args.len() } else { direct.unwrap_or(0).min(args.len()) };
        let (r, fstrict) = agg_function(v, &ff, &given(&stype, &args[..n]))?;
        if extra && fstrict {
            return err("42P13", "final function with extra arguments must not be declared STRICT");
        }
        result = r;
    }
    if let Some(cf) = func("combinefunc") {
        let (r, cstrict) = agg_function(v, &cf, &given(&stype, std::slice::from_ref(&stype)))?;
        if !same(&r, &stype) {
            return err("42804", format!("return type of combine function {} is not {}", cf.1, pg_type(&stype)));
        }
        if stype == "internal" && cstrict {
            return err("42P13", "combine function with transition type internal must not be declared STRICT");
        }
    }
    if let Some(sf) = func("serialfunc") {
        let (r, _) = agg_function(v, &sf, &["internal".to_string()])?;
        if r != "bytea" {
            return err("42804", format!("return type of serialization function {} is not bytea", sf.1));
        }
    }
    if let Some(df) = func("deserialfunc") {
        let (r, _) = agg_function(v, &df, &["bytea".to_string(), "internal".to_string()])?;
        if r != "internal" {
            return err("42804", format!("return type of deserialization function {} is not internal", df.1));
        }
    }
    if let Some(d) = poly_detail(&result, &args) {
        return Err(told("42P13", "cannot determine result data type", Some(d), None));
    }
    if result == "internal" && !args.iter().any(|a| a == "internal") {
        let detail = "A result of type internal requires at least one input of type internal.".to_string();
        return Err(told("42P13", "unsafe use of pseudo-type \"internal\"", Some(detail), None));
    }
    if let Some(ms) = &mstype {
        let mresult = match func("mfinalfunc") {
            Some(mf) => agg_function(v, &mf, &given(ms, &args[..direct.unwrap_or(0).min(args.len())]))?.0,
            None => ms.clone(),
        };
        if !same(&mresult, &result) {
            return err(
                "42P13",
                format!(
                    "moving-aggregate implementation returns type {}, but plain implementation returns type {}",
                    pg_type(&mresult),
                    pg_type(&result)
                ),
            );
        }
    }
    // what is there already: an aggregate OR REPLACE replaces if it is of the same kind and gives the same type; a
    // function it can't
    let exists = || err("42723", format!("function \"{name}\" already exists with same argument types"));
    if let Some(old) = v.get(key)? {
        if !replace {
            return exists();
        }
        let o: Json = serde_json::from_str(&old).unwrap_or_default();
        if let Some(r) = o["result"].as_str()
            && !same(&canon(r), &result)
        {
            let hint = format!("Use DROP AGGREGATE {name}({}) first.", pg_types(&args, ", "));
            return Err(told("42P13", "cannot change return type of existing function", None, Some(hint)));
        }
        let was = o["kind"].as_str().unwrap_or("n");
        if was != kind {
            let what = match was {
                "o" => "an ordered-set aggregate",
                "h" => "a hypothetical-set aggregate",
                _ => "an ordinary aggregate function",
            };
            return Err(told("42809", "cannot change routine kind", Some(format!("\"{name}\" is {what}.")), None));
        }
        if o["direct"] != j["direct"] {
            return err("42P13", "cannot change number of direct arguments of an aggregate function");
        }
    } else if let Some(procedure) = plain_function(v, &name, &args)? {
        if !replace {
            return exists();
        }
        let what = if procedure { "a procedure" } else { "a function" };
        return Err(told("42809", "cannot change routine kind", Some(format!("\"{name}\" is {what}.")), None));
    }
    j["result"] = Json::from(result);
    if let Some(o) = j.as_object_mut() {
        o.remove("replace");
    }
    Ok(vec![(key.to_string(), Some(j.to_string()))])
}

/// The aggregates that use function `f` (as a state, final or other support function), as (row key, description):
/// what DROP FUNCTION refuses to leave without them, or drops with CASCADE.
pub(super) fn aggregates_using(v: &View, f: &functions::Function) -> Result<Vec<(String, String)>> {
    let mut out = vec![];
    for r in v.prefix(&format!("{PREFIX}agg/"))? {
        let (k, raw) = r?;
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        let uses =
            ["sfunc", "finalfunc", "combinefunc", "serialfunc", "deserialfunc", "msfunc", "minvfunc", "mfinalfunc"]
                .iter()
                .any(|n| j[*n].as_str().is_some_and(|s| rel_part(written(s)) == rel_part(&f.name)));
        if uses {
            let (name, types) = key_parts(&k);
            out.push((k.clone(), format!("function {name}({})", pg_types(&types, ","))));
        }
    }
    Ok(out)
}

/// The operator classes function `f` is a support function of (`FUNCTION n` of CREATE OPERATOR CLASS, kept by the
/// name the function is stored under): each class's key, and how Postgres names it.
pub(super) fn opclasses_using(v: &View, f: &functions::Function) -> Result<Vec<(String, String)>> {
    let mut out = vec![];
    for r in v.prefix(&format!("{PREFIX}opclass/"))? {
        let (k, raw) = r?;
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        // (by its name and, where the class kept them, its argument types: an overload of the name isn't the one)
        let args: Vec<String> = f.args.iter().map(|a| a.1.name().to_string()).collect();
        let uses = j["procs"].as_object().is_some_and(|ps| {
            ps.iter().any(|(n, p)| {
                p.as_str() == Some(f.name.as_str())
                    && j["sigs"][n].as_array().is_none_or(|ts| {
                        ts.iter().map(|t| t.as_str().unwrap_or("")).eq(args.iter().map(String::as_str))
                    })
            })
        });
        if uses && let Some((name, am)) = k.strip_prefix(&format!("{PREFIX}opclass/")).and_then(|r| r.rsplit_once('/'))
        {
            out.push((k.clone(), format!("operator class {name} for access method {am}")));
        }
    }
    Ok(out)
}

/// A polymorphic state type made the type arguments of these types give (an aggregate's arguments declared `decl`):
/// anyarray over integers is integer[]; another type is itself. With nothing to go by, text.
pub(super) fn resolved(t: Type, decl: &[Type], given: &[Option<Type>]) -> Type {
    let Type::Poly(k) = t else { return t };
    let mut elem = None;
    for (d, g) in decl.iter().zip(given) {
        if let (Type::Poly(dk), Some(g)) = (d, g) {
            elem = match (dk, g) {
                (1 | 6, Type::Array(e)) => Type::from_oid(*e),
                // (an array value whose element type isn't known; a range)
                (1 | 6, Type::Json | Type::Text) | (4 | 7 | 8 | 9, _) => None,
                // (a VARIADIC array's elements, given one by one)
                _ => Some(*g),
            };
            if elem.is_some() {
                break;
            }
        }
    }
    let e = elem.unwrap_or(Type::Text);
    match k {
        1 | 6 => Type::Array(e.oid()),
        _ => e,
    }
}

/// Whether state function `name` is strict, given these arguments: one made with CREATE FUNCTION says; one of
/// Postgres's built-ins by its catalog.
fn sfunc_strict(name: &str, vals: &[Value]) -> bool {
    match functions::resolve_for(&func_named(name), vals) {
        Ok(f) => f.strict,
        Err(_) => catalog_schema::builtin_signatures(rel_part(written(name))).first().is_none_or(|s| s.2),
    }
}

/// A user aggregate's state after its rows' arguments (in ORDER BY order), before its final function, as Postgres's
/// advance_transition_function folds them: a strict state function skips a row with a null argument, takes the
/// first row's value as the state when there is no initial value, and leaves a null state null; another sees every
/// row. A polymorphic state type is what the arguments make it.
pub(super) fn fold_rows<'a>(i: u8, rows: impl IntoIterator<Item = &'a Vec<Value>>, env: &Env) -> Result<Value> {
    let Some(d) = aggregate_at(i) else { return Ok(Value::Null) };
    let rows: Vec<&Vec<Value>> = rows.into_iter().collect();
    let n = d.args.len();
    let poly = matches!(d.stype, Type::Poly(_));
    let stype = match poly {
        true => {
            let first = rows
                .iter()
                .find(|r| d.args.iter().zip(r.iter()).any(|(t, x)| matches!(t, Type::Poly(_)) && *x != Value::Null));
            let types: Vec<Option<Type>> = first.map_or(vec![], |r| r.iter().map(functions::value_type).collect());
            resolved(d.stype, &d.args, &types)
        }
        false => d.stype,
    };
    let mut state = match &d.initcond {
        Some(c) => coerce(Value::Text(c.clone()), stype)?,
        None => Value::Null,
    };
    // (no state yet: a strict state function's first value is it)
    let mut empty = d.initcond.is_none();
    let mut strict = None;
    for r in rows {
        let vals: &[Value] = if d.variadic { &r[..] } else { &r[..n.min(r.len())] };
        let strict = *strict.get_or_insert_with(|| {
            let mut a = vec![state.clone()];
            a.extend(vals.iter().cloned());
            sfunc_strict(&d.sfunc, &a)
        });
        if strict {
            if vals.contains(&Value::Null) {
                continue;
            }
            if empty {
                if let Some(x) = vals.first() {
                    state = if poly { x.clone() } else { coerce(x.clone(), stype)? };
                    empty = false;
                }
                continue;
            }
            if state == Value::Null {
                continue;
            }
        }
        let mut a = Vec::with_capacity(vals.len() + 1);
        a.push(std::mem::take(&mut state));
        a.extend(vals.iter().cloned());
        state = run(&d.sfunc, a, Some(stype), env)?;
        empty = false;
    }
    Ok(state)
}

/// What a built-in of Postgres's does, by its C symbol (what LANGUAGE internal's `AS 'int4pl'` names, or an
/// aggregate's `SFUNC = int4pl`), as an expression over `$1`, `$2`, ...: an operator's (`$1::integer +
/// $2::integer`), or a call of the function it is (`dsin` is `sin($1::double precision)`). None when Postgres has
/// no such function.
pub(super) fn internal_sql(symbol: &str) -> Option<String> {
    static MADE: LazyLock<Mutex<HashMap<String, Option<String>>>> = LazyLock::new(Default::default);
    if let Some(s) = MADE.lock().unwrap().get(symbol) {
        return s.clone();
    }
    let made = catalog_schema::internal_function(symbol).map(|(name, args, _, _)| internal_expr(symbol, &name, &args));
    MADE.lock().unwrap().insert(symbol.to_string(), made.clone());
    made
}

/// The types a symbol's operator functions are named by (`int4` in `int4pl`, `text` in `texteq`).
const SYMBOL_TYPES: [&str; 27] = [
    "int2",
    "int4",
    "int8",
    "int24",
    "int42",
    "int28",
    "int82",
    "int48",
    "int84",
    "float4",
    "float8",
    "float48",
    "float84",
    "numeric_",
    "numeric",
    "text_",
    "text",
    "bool",
    "oid",
    "date_",
    "interval_",
    "cash_",
    "bpchar",
    "char",
    "name",
    "timestamp_",
    "time_",
];

fn internal_expr(symbol: &str, name: &str, types: &[String]) -> String {
    // each argument as its type, where that is one Chronos reads
    let arg = |i: usize| match types.get(i).map(String::as_str) {
        Some(
            t @ ("smallint" | "integer" | "bigint" | "real" | "double precision" | "numeric" | "text" | "boolean"
            | "date" | "interval" | "oid"),
        ) => format!("${}::{t}", i + 1),
        _ => format!("${}", i + 1),
    };
    // a function named by its type and what it does: `int4pl`, `float8mi`, `texteq`, `int2um`
    let typed = |what: &str| SYMBOL_TYPES.iter().any(|t| symbol.strip_suffix(what) == Some(*t));
    match (symbol, types.len()) {
        ("textcat", 2) => return format!("({} || {})", arg(0), arg(1)),
        ("int8inc" | "int8inc_any", n) if n > 0 => return format!("({} + 1)", arg(0)),
        // the larger (smaller) of two arrays, the second where they're equal: compared element by element (as text,
        // a polymorphic state's elements being of no type known here)
        ("array_larger", 2) => return "(case when $1::text[] > $2::text[] then $1 else $2 end)".into(),
        ("array_smaller", 2) => return "(case when $1::text[] < $2::text[] then $1 else $2 end)".into(),
        // a sum's state function: a null state is none yet, a null value is skipped
        ("int2_sum" | "int4_sum" | "int8_sum", 2) => {
            return format!("(case when $2 is null then $1 else coalesce({}, 0) + {} end)", arg(0), arg(1));
        }
        _ => {}
    }
    if types.len() == 2 {
        const OPS: [(&str, &str); 13] = [
            ("pl", "+"),
            ("add", "+"),
            ("mi", "-"),
            ("sub", "-"),
            ("mul", "*"),
            ("div", "/"),
            ("mod", "%"),
            ("eq", "="),
            ("ne", "<>"),
            ("lt", "<"),
            ("le", "<="),
            ("gt", ">"),
            ("ge", ">="),
        ];
        for (what, op) in OPS {
            if typed(what) {
                return format!("({} {op} {})", arg(0), arg(1));
            }
        }
        if typed("larger") {
            return format!("greatest({}, {})", arg(0), arg(1));
        }
        if typed("smaller") {
            return format!("least({}, {})", arg(0), arg(1));
        }
        // btint4cmp, bttextcmp: -1, 0 or 1
        if symbol.strip_prefix("bt").and_then(|s| s.strip_suffix("cmp")).is_some_and(|t| SYMBOL_TYPES.contains(&t)) {
            let (a, b) = (arg(0), arg(1));
            return format!("(case when {a} < {b} then -1 when {a} > {b} then 1 else 0 end)");
        }
    }
    if types.len() == 1 {
        if typed("um") {
            return format!("(-{})", arg(0));
        }
        if typed("up") {
            return format!("(+{})", arg(0));
        }
        if typed("abs") {
            return format!("abs({})", arg(0));
        }
    }
    let args: Vec<String> = (0..types.len()).map(arg).collect();
    format!("pg_catalog.{}({})", ident(name), args.join(", "))
}

/// Runs the SQL `internal_sql` gives for a built-in on these arguments.
fn internal_call(sql: &str, vals: Vec<Value>, env: &Env) -> Result<Value> {
    let tys: Vec<Option<Type>> = vals.iter().map(functions::value_type).collect();
    let e = Binder::plain(&[], None).with_params(&tys).bind(&check_expr(sql)?)?;
    eval(&e, &Env { cols: &[], row: &[], params: &vals, aggs: &[], sub: env.sub })
}
