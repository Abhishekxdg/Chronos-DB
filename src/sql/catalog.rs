//! The catalog's constraints, indexes and schemas (pg_constraint, pg_index, pg_namespace,
//! information_schema's constraint views), the oids that tie them together, and how Postgres
//! writes them (pg_get_constraintdef, pg_get_indexdef, regclass).

use super::collation::{self, Coll, collatable};
use super::*;
use std::cell::RefCell;

/// What a foreign key references: its table, the columns there, and its (ON UPDATE, ON DELETE)
/// letters and the UNIQUE constraint it names (none: the primary key).
pub type Parent = (String, Vec<i64>, (char, char), Option<String>);

/// A constraint, as pg_constraint shows it.
#[derive(Clone, Debug)]
pub struct Con {
    pub name: String,
    /// p primary key, u unique, f foreign key, c check
    pub kind: char,
    /// its columns (attnums)
    pub cols: Vec<i64>,
    /// a foreign key's table (as stored), the columns it references there, its ON UPDATE and ON
    /// DELETE (pg_constraint's letters: a no action, r restrict, c cascade, n set null, d set
    /// default), and the UNIQUE constraint it references (none: the primary key)
    pub parent: Option<Parent>,
    /// as pg_get_constraintdef writes it, and pretty (see `expr_def`)
    pub def: String,
    pub pretty: String,
    /// DEFERRABLE, and INITIALLY DEFERRED (foreign keys only)
    pub defer: (bool, bool),
    /// pg_constraint.conislocal and coninhcount
    pub local: bool,
    pub inh: i64,
    /// NO INHERIT (a check)
    pub no_inherit: bool,
    /// a partition's constraint cloned from its parent's: that one's name
    pub given: Option<String>,
    /// convalidated (false: a check added NOT VALID, not yet validated)
    pub valid: bool,
    /// a check's condition as pg_get_expr(conbin) writes it: `(a <> 0)`
    pub bin: Option<String>,
    /// `given` names a constraint of the same table, not of its parent's: one of the constraints Postgres adds for
    /// a foreign key to a partitioned table, one per partition there (see `hier::referenced_clones`)
    pub given_here: bool,
}

/// A domain's CHECK condition as Postgres writes it: `(VALUE > 0)`, where it is kept as `value > 0`.
fn domain_check_cond(text: &str) -> String {
    let cols = [Col::new("", "value", Type::Text)];
    let cond = expr_def(text, &cols, true);
    // (the word VALUE, outside quoted text)
    let (mut out, mut quoted, mut word) = (String::new(), false, String::new());
    let flush = |word: &mut String, out: &mut String| {
        if word.eq_ignore_ascii_case("value") {
            out.push_str("VALUE");
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in cond.chars() {
        if quoted {
            out.push(c);
            quoted = c != '\'';
        } else if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
            quoted = c == '\'';
        }
    }
    flush(&mut word, &mut out);
    out
}

/// A constraint trigger's pg_constraint row (contype `t`): no columns, no index, no table it references, never
/// inherited (Postgres's CreateTrigger makes one for each table, a partition's copy too).
fn trigger_con(name: &str, deferrable: bool, deferred: bool) -> Con {
    Con {
        name: name.into(),
        kind: 't',
        cols: vec![],
        parent: None,
        def: "TRIGGER".into(),
        pretty: "TRIGGER".into(),
        defer: (deferrable, deferred),
        local: true,
        inh: 0,
        no_inherit: true,
        given: None,
        valid: true,
        bin: None,
        given_here: false,
    }
}

/// An index, as pg_index shows it.
#[derive(Clone, Debug, Default)]
pub struct Idx {
    pub name: String,
    pub unique: bool,
    pub primary: bool,
    /// its columns (attnums), 0 for an expression
    pub cols: Vec<i64>,
    /// each part as SQL: its column's name or its expression
    pub parts: Vec<String>,
    /// the same, as pg_get_indexdef(.., pretty) writes them: `(a + 0)` where `parts` has `((a + 0))`
    pub parts_pretty: Vec<String>,
    /// a partial index's WHERE, as Postgres writes it
    pub pred: Option<String>,
    /// the same, as pg_get_indexdef(.., pretty) writes it
    pub pred_pretty: Option<String>,
    /// false for an index made ON ONLY a partitioned table that a partition has no index of
    pub valid: bool,
    /// the built-in access method it is and does as (pg_index's properties: see `indexam`)
    pub method: &'static str,
    /// the access method CREATE ACCESS METHOD made that it is of (`method` is its handler's), as pg_class.relam and
    /// pg_get_indexdef name it
    pub made: Option<String>,
    /// NULLS NOT DISTINCT
    pub nnd: bool,
    /// a partition's index that is its parent's: that one's name
    pub given: Option<String>,
    /// the index the table was last clustered on (pg_index.indisclustered)
    pub clustered: bool,
    /// the index REPLICA IDENTITY USING INDEX named (pg_index.indisreplident)
    pub replident: bool,
    /// each part's collation (None: a type without one)
    pub(crate) colls: Vec<Option<Coll>>,
    /// each part's ` COLLATE c`, where its collation isn't its column's (or an expression's default)
    pub collate: Vec<String>,
    /// pg_class.reltablespace (0: the database's default) and its storage's generation (see `hier::Hier::tablespace`
    /// and `tablespace::Gens`, which `tables` gives it from)
    pub tablespace: i64,
    pub filenode: i64,
    /// how many of `cols` (and `parts`, `colls`) are key columns: those after them are INCLUDE's
    /// (pg_index.indnkeyatts)
    pub nkey: usize,
    /// each key column's pg_index.indoption, pg_index.indclass (0 for a class that isn't one of Postgres's own) and
    /// its class's name ("" for none), and the type its index column holds where that isn't the indexed type's
    /// (opckeytype; 0 where it is)
    pub opts: Vec<i64>,
    pub classes: Vec<i64>,
    pub class_names: Vec<String>,
    pub keytypes: Vec<i64>,
    /// each key column's operator class as pg_get_indexdef writes it after the column: ` text_pattern_ops`, empty
    /// for its type's default
    pub shown_classes: Vec<String>,
}

impl Idx {
    /// Its access method's name, as pg_am has it: the one CREATE ACCESS METHOD made that it is of, or its own.
    pub fn am_name(&self) -> &str {
        self.made.as_deref().unwrap_or(self.method)
    }

    /// Its columns as pg_get_indexdef writes them (ruleutils.c:1338-1452): `(a, b DESC) INCLUDE (c)`, each key
    /// column with its COLLATE, its operator class where it isn't its type's default, and its ordering where the
    /// access method orders (DESC with nulls first, its default, is just DESC). `pretty`: pg_get_indexdef(.., true)'s
    /// forms of expressions.
    pub fn columns_def(&self, pretty: bool) -> String {
        let parts = if pretty { &self.parts_pretty } else { &self.parts };
        let mut d = String::from("(");
        for (k, p) in parts.iter().enumerate() {
            if k == self.nkey {
                d.push_str(") INCLUDE (");
            } else if k > 0 {
                d.push_str(", ");
            }
            d.push_str(p);
            if k >= self.nkey {
                continue;
            }
            d.push_str(self.collate.get(k).map_or("", String::as_str));
            d.push_str(self.shown_classes.get(k).map_or("", String::as_str));
            let opt = self.opts.get(k).copied().unwrap_or(0);
            let (desc, first) =
                (opt & i64::from(btree::INDOPTION_DESC) != 0, opt & i64::from(btree::INDOPTION_NULLS_FIRST) != 0);
            d.push_str(match (desc, first) {
                (true, true) => " DESC",
                (true, false) => " DESC NULLS LAST",
                (false, true) => " NULLS FIRST",
                (false, false) => "",
            });
        }
        d.push(')');
        d
    }
}

/// What an index's key columns are beyond their values (see `Idx`): of access method `am`, each column's type, its
/// type's id (a varchar's and a char(n)'s own), the operator class it names and its indoption.
fn key_facts(idx: &mut Idx, am: &str, keys: &[(Type, i64, Option<&str>, i16)]) {
    idx.nkey = keys.len();
    for &(ty, oid, named, opt) in keys {
        let class = super::indexam::class_of(am, named, ty, oid);
        let (class_oid, name, keytype) = class.unwrap_or((0, String::new(), 0));
        idx.opts.push(i64::from(opt));
        idx.classes.push(class_oid);
        idx.shown_classes.push(match named {
            Some(n) if !super::indexam::is_default(am, n, ty, oid) => format!(" {}", ident(n)),
            _ => String::new(),
        });
        idx.class_names.push(name);
        idx.keytypes.push(keytype);
    }
}

/// A table's constraints and indexes. A UNIQUE constraint makes an index of its name, as in
/// Postgres, and CREATE UNIQUE INDEX makes only the index.
pub(super) fn describe(v: &View, t: &Table) -> (Vec<Con>, Vec<Idx>) {
    let num = |id: u32| t.cols.iter().position(|c| c.id == id).map_or(0, |i| t.attnum(i));
    let named = |id: u32| t.cols.iter().find(|c| c.id == id).map_or_else(String::new, |c| ident(&c.name));
    // a column's collation, for text
    let coll_of = |id: u32| t.cols.iter().find(|c| c.id == id).and_then(|c| collatable(c.ty).then_some(c.coll));
    // a column's type, and its type's id as pg_attribute has it (a varchar's and a char(n)'s their own)
    let typed = |id: u32| match t.cols.iter().find(|c| c.id == id) {
        Some(c) => (c.ty, super::col_oid(c)),
        None => (Type::Text, 25),
    };
    // a constraint's index: a btree of its columns' default classes, in their own order
    let plain_keys = |ids: &mut dyn Iterator<Item = u32>| -> Vec<(Type, i64, Option<&'static str>, i16)> {
        ids.map(|id| {
            let (ty, oid) = typed(id);
            (ty, oid, None, 0)
        })
        .collect()
    };
    let (mut cons, mut idx) = (vec![], vec![]);
    // what a partition has of its parent's (see `hier::clone_constraints`): the parent's name for it
    let up = t.hier.of.as_ref().and_then(|(p, _)| load(v, p).ok().flatten());
    let given = |n: &str| t.hier.inherited.iter().any(|x| x == n);
    let same_cols = |a: &[u32], b: &[u32]| {
        let names = |t: &Table, ids: &[u32]| -> Vec<Option<String>> {
            ids.iter().map(|i| t.cols.iter().find(|c| c.id == *i).map(|c| c.name.clone())).collect()
        };
        up.as_ref().is_some_and(|p| names(p, a) == names(t, b))
    };
    let valid_up = |n: &str| hier::index_valid(v, t, n);
    let clustered = |n: &str| t.hier.clustered.as_deref() == Some(n);
    let replident = |n: &str| matches!(&t.hier.replident, Some(('i', Some(x))) if x == n);
    let valid = valid_up(&t.pkey_name());
    let from_up_unique = |u: &Unique| {
        up.as_ref()
            .filter(|_| given(&u.name))
            .and_then(|p| p.uniques.iter().find(|pu| same_cols(&pu.cols, &u.cols)))
            .map(|pu| pu.name.clone())
    };
    if t.has_key() {
        let keys = t.key_cols();
        let parts: Vec<String> = keys.iter().map(|&i| ident(&t.cols[i].name)).collect();
        let cols: Vec<i64> = keys.iter().map(|&i| t.attnum(i)).collect();
        let name = t.pkey_name();
        let pgiven = given("*pkey") && up.is_some();
        let def = format!("PRIMARY KEY ({})", parts.join(", "));
        cons.push(Con {
            name: name.clone(),
            kind: 'p',
            cols: cols.clone(),
            parent: None,
            pretty: def.clone(),
            def,
            defer: (false, false),
            local: !pgiven,
            inh: i64::from(pgiven),
            no_inherit: false,
            given: pgiven.then(|| up.as_ref().map_or_else(String::new, Table::pkey_name)),
            valid: true,
            bin: None,
            given_here: false,
        });
        let is_clustered = clustered(&name);
        let is_replident = replident(&name);
        let pkey = name.clone();
        let mut x = Idx {
            name,
            unique: true,
            primary: true,
            cols,
            parts_pretty: parts.clone(),
            parts,
            pred: None,
            pred_pretty: None,
            valid,
            method: "btree",
            nnd: false,
            given: pgiven.then(|| up.as_ref().map_or_else(String::new, Table::pkey_name)),
            clustered: is_clustered,
            replident: is_replident,
            colls: keys.iter().map(|&i| coll_of(t.cols[i].id)).collect(),
            collate: vec![],
            tablespace: t.hier.index_space(&pkey),
            filenode: 0,
            ..Idx::default()
        };
        key_facts(&mut x, "btree", &plain_keys(&mut keys.iter().map(|&i| t.cols[i].id)));
        idx.push(x);
    }
    for u in t.uniques.iter().filter(|u| !t.indexes.iter().any(|x| x.name == u.name)) {
        let parts: Vec<String> = u.cols.iter().map(|&c| named(c)).collect();
        let cols: Vec<i64> = u.cols.iter().map(|&c| num(c)).collect();
        let def = match u.primary {
            true => format!("PRIMARY KEY ({})", parts.join(", ")),
            false => format!("UNIQUE {}({})", if u.nnd { "NULLS NOT DISTINCT " } else { "" }, parts.join(", ")),
        };
        let from = from_up_unique(u);
        cons.push(Con {
            name: u.name.clone(),
            kind: if u.primary { 'p' } else { 'u' },
            cols: cols.clone(),
            parent: None,
            pretty: def.clone(),
            def,
            defer: (false, false),
            local: from.is_none(),
            inh: i64::from(from.is_some()),
            no_inherit: false,
            given: from.clone(),
            valid: true,
            bin: None,
            given_here: false,
        });
        let mut x = Idx {
            name: u.name.clone(),
            unique: true,
            primary: u.primary,
            cols,
            parts_pretty: parts.clone(),
            parts,
            pred: None,
            pred_pretty: None,
            valid: valid_up(&u.name),
            method: "btree",
            nnd: u.nnd,
            given: from,
            clustered: clustered(&u.name),
            replident: replident(&u.name),
            colls: u.cols.iter().map(|&c| coll_of(c)).collect(),
            collate: vec![],
            tablespace: t.hier.index_space(&u.name),
            filenode: 0,
            ..Idx::default()
        };
        key_facts(&mut x, "btree", &plain_keys(&mut u.cols.iter().copied()));
        idx.push(x);
    }
    for x in &t.indexes {
        let mut exprs = x.exprs.iter();
        let (mut colls, mut collate, mut types) = (vec![], vec![], vec![]);
        let (mut cols, parts): (Vec<i64>, Vec<String>) = x
            .cols
            .iter()
            .enumerate()
            .map(|(k, &c)| {
                // a part's collation, and COLLATE after it where it isn't its column's (or an expression's default)
                let (n, part, own) = match c {
                    btree::EXPR => {
                        let text = exprs.next().map_or("", String::as_str);
                        let ty = super::index_expr(t, text)
                            .ok()
                            .and_then(|e| super::ty_in(&e, &t.cols, &[], &[]).ok())
                            .unwrap_or(Type::Text);
                        types.push((ty, i64::from(ty.oid())));
                        (0, expr_def(text, &t.cols, false), collatable(ty).then_some(Coll::DEFAULT))
                    }
                    c => {
                        types.push(typed(c));
                        (num(c), named(c), coll_of(c))
                    }
                };
                colls.push(own.map(|_| x.coll(k)));
                collate.push(match own {
                    Some(o) if o != x.coll(k) => format!(" COLLATE {}", x.coll(k).shown()),
                    _ => String::new(),
                });
                (n, part)
            })
            .unzip();
        let pred = x.filter.as_deref().map(|f| expr_def(f, &t.cols, true));
        let unique = t.uniques.iter().any(|u| u.name == x.name);
        let nnd = t.uniques.iter().any(|u| u.name == x.name && u.nnd);
        // (as pg_am names it: the built-in one it does as, which an access method made here is over, and that one)
        let method = AMS.iter().find(|(_, n, ..)| *n == x.kind()).map_or("btree", |(_, n, ..)| *n);
        let made = x.builtin.as_ref().map(|_| x.method().to_string());
        let from = up.as_ref().filter(|_| given(&x.name)).and_then(|p| {
            if unique {
                p.uniques.iter().find(|pu| same_cols(&pu.cols, &x.cols)).map(|pu| pu.name.clone())
            } else if let Some(up) = t.hier.index_up(&x.name).filter(|u| p.indexes.iter().any(|px| px.name == *u)) {
                // (the one it was made as a copy of, or attached to)
                Some(up.to_string())
            } else {
                // (of several alike, the partition's are its parent's in the order both were made)
                let alike = |a: &btree::Index| a.exprs == x.exprs && a.filter == x.filter;
                let rank = t
                    .indexes
                    .iter()
                    .take_while(|y| y.name != x.name)
                    .filter(|y| given(&y.name) && y.cols == x.cols && alike(y))
                    .count();
                let mut ups = p.indexes.iter().filter(|px| same_cols(&px.cols, &x.cols) && alike(px));
                let first = ups.clone().next();
                ups.nth(rank).or(first).map(|px| px.name.clone())
            }
        });
        let pred_pretty = x.filter.as_deref().map(|f| pretty_def(f, &t.cols));
        // (an expression that isn't a function call is in parentheses of its own, around its pretty form)
        let mut texts = x.exprs.iter();
        let parts_pretty: Vec<String> = x
            .cols
            .iter()
            .zip(&parts)
            .map(|(&c, part)| {
                if c != btree::EXPR {
                    return part.clone();
                }
                let text = texts.next().map_or("", String::as_str);
                let bare = check_expr(text).ok().and_then(|e| deparse(&e, &t.cols, false)).map(|(d, _)| d);
                match bare {
                    Some(b) if b != *part => format!("({})", pretty_def(text, &t.cols)),
                    _ => pretty_def(text, &t.cols),
                }
            })
            .collect();
        // each key part's operator class as written (a trigram index kept before classes were has its one in `trgm`)
        let keys: Vec<(Type, i64, Option<&str>, i16)> =
            types.iter().enumerate().map(|(k, &(ty, oid))| (ty, oid, x.said_class(k), x.opt(k))).collect();
        // INCLUDE's columns after the key parts: no collation, class or ordering of their own
        let (mut parts, mut parts_pretty) = (parts, parts_pretty);
        for &c in &x.include {
            cols.push(num(c));
            parts.push(named(c));
            parts_pretty.push(named(c));
            colls.push(None);
            collate.push(String::new());
        }
        let mut ix = Idx {
            name: x.name.clone(),
            unique,
            primary: false,
            cols,
            parts_pretty,
            parts,
            pred,
            pred_pretty,
            valid: valid_up(&x.name),
            method,
            made: made.clone(),
            nnd,
            given: from,
            clustered: clustered(&x.name),
            replident: replident(&x.name),
            colls,
            collate,
            tablespace: t.hier.index_space(&x.name),
            filenode: 0,
            ..Idx::default()
        };
        key_facts(&mut ix, made.as_deref().unwrap_or(method), &keys);
        // (an access method made here: each key column's class is one made for it, the one it names or the default
        // made for its type, which pg_get_indexdef leaves out)
        if let Some(am) = &made {
            for (k, &(ty, oid, named, _)) in keys.iter().enumerate() {
                let default = super::accessmethod::made_default_class(v, am, ty, oid).ok().flatten();
                if named.is_none() || named.map(String::from) == default {
                    ix.shown_classes[k] = String::new();
                }
                ix.class_names[k] = named.map(String::from).or(default).unwrap_or_default();
            }
        }
        idx.push(ix);
    }
    for (name, c, ops) in &t.ann {
        // the operator class, as Postgres writes it: only when it isn't the type's default
        let part = match ops {
            Some(o) => format!("{} {o}", named(*c)),
            None => named(*c),
        };
        idx.push(Idx {
            name: name.clone(),
            unique: false,
            primary: false,
            cols: vec![num(*c)],
            parts: vec![part.clone()],
            parts_pretty: vec![part],
            pred: None,
            pred_pretty: None,
            valid: true,
            method: "hnsw",
            made: None,
            nnd: false,
            given: None,
            clustered: false,
            replident: false,
            colls: vec![None],
            collate: vec![],
            tablespace: t.hier.index_space(name),
            filenode: 0,
            // (pgvector's classes have no pg_opclass row here: its part shows its own)
            nkey: 1,
            opts: vec![0],
            classes: vec![0],
            class_names: vec![ops.clone().unwrap_or_else(|| "vector_l2_ops".into())],
            keytypes: vec![0],
            shown_classes: vec![String::new()],
        });
    }
    for f in &t.fks {
        let parent = load(v, &f.table).ok().flatten();
        let pkeys = parent.as_ref().map(|p| ref_cols(p, f)).unwrap_or_default();
        let unique = parent.as_ref().filter(|_| !f.refs.is_empty()).and_then(|p| {
            let same = |u: &Unique| u.cols.len() == f.refs.len() && f.refs.iter().all(|c| u.cols.contains(c));
            p.uniques.iter().find(|u| uniq_index(p, u).is_none() && same(u)).map(|u| u.name.clone())
        });
        let pnames: Vec<String> =
            pkeys.iter().filter_map(|&i| parent.as_ref().map(|p| ident(&p.cols[i].name))).collect();
        let (del, action) = fk_actions(f, &|c| named(c));
        let parts: Vec<String> = f.cols.iter().map(|&c| named(c)).collect();
        let when = match (f.deferrable, f.deferred) {
            (false, _) => "",
            (true, false) => " DEFERRABLE",
            (true, true) => " DEFERRABLE INITIALLY DEFERRED",
        };
        let def = format!(
            "FOREIGN KEY ({}) REFERENCES {}({}){}{action}{when}{}",
            parts.join(", "),
            shown_name(&f.table),
            pnames.join(", "),
            if f.full { " MATCH FULL" } else { "" },
            if f.valid { "" } else { " NOT VALID" }
        );
        let parent = Some((
            f.table.clone(),
            pkeys.iter().map(|&i| parent.as_ref().map_or(i as i64 + 1, |p| p.attnum(i))).collect(),
            del,
            unique.clone(),
        ));
        let cols: Vec<i64> = f.cols.iter().map(|&c| num(c)).collect();
        // the parent's it is, by the parent's name for it (which a rename may have made another)
        let from = up.as_ref().filter(|_| given(&f.name)).map(|_| f.up.clone().unwrap_or_else(|| f.name.clone()));
        let own = from.is_none();
        cons.push(Con {
            name: f.name.clone(),
            kind: 'f',
            cols: cols.clone(),
            parent,
            pretty: def.clone(),
            def,
            defer: (f.deferrable, f.deferred),
            local: from.is_none(),
            inh: i64::from(from.is_some()),
            no_inherit: false,
            given: from,
            valid: f.valid,
            bin: None,
            given_here: false,
        });
        // the table's own foreign key to a partitioned table is a constraint of this table for each partition
        // there too, each under the one of the partition above it (Postgres's addFkRecurseReferenced)
        if !own {
            continue;
        }
        for (part, name, above) in hier::referenced_clones(v, t, f).unwrap_or_default() {
            let Some(p) = load(v, &part).ok().flatten() else { continue };
            let pk = ref_cols(&p, f);
            let pn: Vec<String> = pk.iter().map(|&i| ident(&p.cols[i].name)).collect();
            let def = format!(
                "FOREIGN KEY ({}) REFERENCES {}({}){}{action}{when}{}",
                parts.join(", "),
                shown_name(&part),
                pn.join(", "),
                if f.full { " MATCH FULL" } else { "" },
                if f.valid { "" } else { " NOT VALID" }
            );
            cons.push(Con {
                name,
                kind: 'f',
                cols: cols.clone(),
                parent: Some((part.clone(), pk.iter().map(|&i| i as i64 + 1).collect(), del, unique.clone())),
                pretty: def.clone(),
                def,
                defer: (f.deferrable, f.deferred),
                local: false,
                inh: 1,
                no_inherit: false,
                given: Some(above),
                valid: f.valid,
                bin: None,
                given_here: true,
            });
        }
    }
    for c in &t.checks {
        let mut cols = vec![];
        if let Ok(e) = check_expr(&c.text) {
            walk(&e, &mut |x| {
                if let Expr::Col(_, n) = x
                    && let Some(i) = t.cols.iter().position(|c| c.name == *n)
                {
                    cols.push(t.attnum(i));
                }
            });
        }
        cols.sort_unstable();
        cols.dedup();
        // (pg_get_constraintdef: NO INHERIT after the condition, then NOT VALID)
        let bin = expr_def(&c.text, &t.cols, true);
        let tail =
            format!("{}{}", if c.no_inherit { " NO INHERIT" } else { "" }, if c.valid { "" } else { " NOT VALID" });
        let def = format!("CHECK ({bin}){tail}");
        let pretty = format!("CHECK ({}){tail}", pretty_def(&c.text, &t.cols));
        cons.push(Con {
            name: c.name.clone(),
            kind: 'c',
            cols,
            parent: None,
            def,
            pretty,
            defer: (false, false),
            local: c.local,
            inh: i64::from(c.inh),
            no_inherit: c.no_inherit,
            given: None,
            valid: c.valid,
            bin: Some(bin),
            given_here: false,
        });
    }
    // an EXCLUDE constraint (contype x): its definition as pg_get_constraintdef writes it
    for x in &t.excludes {
        let mut cols = vec![];
        let parts: Vec<String> = x
            .parts
            .iter()
            .map(|(e, op)| {
                let shown = match t.cols.iter().position(|c| c.name == *e) {
                    Some(i) => {
                        cols.push(t.attnum(i));
                        ident(e)
                    }
                    None => {
                        if let Ok(ex) = check_expr(e) {
                            walk(&ex, &mut |y| {
                                if let Expr::Col(_, n) = y
                                    && let Some(i) = t.cols.iter().position(|c| c.name == *n)
                                {
                                    cols.push(t.attnum(i));
                                }
                            });
                        }
                        expr_def(e, &t.cols, false)
                    }
                };
                format!("{shown} WITH {op}")
            })
            .collect();
        let pred = x.pred.as_deref().map_or(String::new(), |p| format!(" WHERE ({})", expr_def(p, &t.cols, true)));
        let def = format!("EXCLUDE USING {} ({}){pred}", x.using, parts.join(", "));
        cols.dedup();
        cons.push(Con {
            name: x.name.clone(),
            kind: 'x',
            cols,
            parent: None,
            pretty: def.clone(),
            def,
            defer: (false, false),
            local: true,
            inh: 0,
            no_inherit: true,
            given: None,
            valid: true,
            bin: None,
            given_here: false,
        });
    }
    (cons, idx)
}

/// The name this session logged in as (its session user), when it isn't one of the roles the
/// catalog always has: a login that is no role shows as a superuser of its name.
fn login() -> Option<String> {
    let user = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| c.session_user.clone())))?;
    (user != "chronos" && user != "pg_database_owner").then_some(user)
}

/// A CHECK constraint's condition as information_schema.check_constraints shows it: its
/// definition without the word `CHECK` (so `((total >= (0)::numeric))`, in the parentheses of
/// the definition and of the operation).
fn check_clause(def: &str) -> &str {
    def.strip_prefix("CHECK ").unwrap_or(def)
}

/// A condition or expression (as SQL) as Postgres writes it back: each operation in
/// parentheses, keywords in capitals, constants cast to their types (`(qty > 0)`,
/// `(name <> 'x'::text)`); `pretty`, with only the parentheses precedence needs (`qty > 0`), as
/// pg_get_constraintdef(oid, true) writes it. `whole`: an operation stands in parentheses of
/// its own, as a CHECK's or a WHERE's does. One this doesn't know is left as written.
pub(super) fn expr_def(text: &str, cols: &[Col], whole: bool) -> String {
    match check_expr(text).ok().and_then(|e| deparse_any(&e, cols, false)) {
        Some((d, _)) if whole || !d.starts_with('(') => d,
        Some((d, _)) => format!("({d})"),
        None if whole => format!("({})", shown_text(text)),
        None => shown_text(text),
    }
}

/// An expression's stored text (its tokens, joined by spaces: `char_length ( title ) >= 1`) with the spaces a
/// reader doesn't write taken out again (`char_length(title) >= 1`), for an expression `deparse` has no form for.
fn shown_text(text: &str) -> String {
    lex(text).map_or_else(|_| text.to_string(), |t| tokens_shown(&t))
}

/// An expression as Postgres writes it back (see `expr_def`), and whether it is a function call.
pub(super) fn deparse_expr(e: &Expr, cols: &[Col]) -> Option<(String, bool)> {
    deparse_any(e, cols, false).map(|(d, _)| (d, matches!(e, Expr::Func(..) | Expr::Call(..))))
}

/// `deparse`, else the views' fuller writer (see `viewdef::column_expr`): what precedence an expression of the
/// second has is told by its parentheses.
fn deparse_any(e: &Expr, cols: &[Col], pretty: bool) -> Option<(String, Prec)> {
    deparse(e, cols, pretty).or_else(|| {
        let d = super::viewdef::column_expr(e, cols, pretty)?;
        let p = if d.starts_with('(') || !d.contains(' ') { Prec::Atom } else { Prec::Other };
        Some((d, p))
    })
}

/// A column default as `pg_get_expr(adbin, adrelid, true)` writes it (what psql's \d shows), from the form
/// `expr_def` kept for table `t` (`(a * 2)` is `a * 2`); the kept form where this has no pretty form of it.
pub fn pretty_default(t: &super::TableInfo, kept: &str) -> String {
    let cols: Vec<Col> = t.columns.iter().map(|(n, ty, ..)| Col::new(&t.name, n, *ty)).collect();
    check_expr(kept).ok().and_then(|e| deparse_any(&e, &cols, true)).map_or_else(|| kept.to_string(), |(d, _)| d)
}

/// `expr_def`, pretty.
pub(super) fn pretty_def(text: &str, cols: &[Col]) -> String {
    check_expr(text).ok().and_then(|e| deparse_any(&e, cols, true)).map_or_else(|| shown_text(text), |(d, _)| d)
}

/// How tightly an expression binds, for where Postgres's pretty form needs parentheses.
#[derive(Clone, Copy, PartialEq)]
enum Prec {
    Or,
    And,
    Not,
    Cmp,
    Other,
    Low,  // + -
    High, // * / %
    Atom,
}

/// Whether `child` (on the left or not) needs parentheses under `parent`, in the pretty form.
fn parens(child: Prec, parent: Prec, left: bool) -> bool {
    use Prec::*;
    match (child, parent) {
        (Atom, _) | (Cmp | Other, Or | And | Not) | (And | Not, Or | And) | (Or, Or) | (High, Low) => false,
        (High, High) | (Low, Low) => !left,
        _ => true,
    }
}

fn deparse(e: &Expr, cols: &[Col], pretty: bool) -> Option<(String, Prec)> {
    // (a constant converted to a column's type: a cast of it, in parentheses unless pretty)
    let bare = pretty;
    use Value::*;
    let ty = |e: &Expr| match e {
        Expr::Col(_, n) => cols.iter().find(|c| c.name == *n).map(|c| c.ty),
        _ => None,
    };
    // a constant, cast to the column type it's compared with where Postgres shows that
    let lit = |v: &Value, to: Option<Type>| -> Option<String> {
        Some(match (v, to) {
            (Null, _) => "NULL".into(),
            (Bool(b), _) => b.to_string(),
            // (a constant converted to the column's type: Postgres's implicit cast of it, in parentheses unless pretty)
            (Int(n), Some(t @ (Type::Numeric | Type::Float | Type::Float4))) => {
                let shown = t.pg_name();
                match (*n < 0, bare) {
                    (true, true) => format!("'{n}'::integer::{shown}"),
                    (true, false) => format!("('{n}'::integer)::{shown}"),
                    (false, true) => format!("{n}::{shown}"),
                    (false, false) => format!("({n})::{shown}"),
                }
            }
            (Numeric(d), Some(t @ (Type::Float | Type::Float4))) if !d.to_string().starts_with('-') => {
                let shown = t.pg_name();
                if bare { format!("{d}::{shown}") } else { format!("({d})::{shown}") }
            }
            (Float(x), Some(t @ (Type::Float | Type::Float4))) if *x >= 0.0 && x.is_finite() => {
                let (shown, d) = (t.pg_name(), Float(*x).to_text());
                if bare { format!("{d}::{shown}") } else { format!("({d})::{shown}") }
            }
            (Int(n), _) if *n < 0 => format!("'{n}'::integer"),
            (Int(n), _) => n.to_string(),
            (Numeric(d), _) => d.to_string(),
            (Text(s), Some(t @ (Type::Date | Type::Timestamp | Type::TimestampTz | Type::Time))) => {
                format!("'{}'::{}", s.replace('\'', "''"), t.pg_name())
            }
            // an address, in its own output form: against a cidr it is an inet, whose operators those are
            (Text(s), Some(t)) if net::kind_of(t).is_some() => {
                let to = if t == Type::Cidr { Type::Inet } else { t };
                let shown = coerce(Text(s.clone()), to).map_or_else(|_| s.clone(), |v| v.to_text());
                format!("'{}'::{}", shown.replace('\'', "''"), to.name())
            }
            (Text(s), _) => format!("'{}'::text", s.replace('\'', "''")),
            _ => return None,
        })
    };
    // an operand: under `parent`, in parentheses where the pretty form needs them
    let side = |x: &Expr, other: &Expr, parent: Prec, left: bool| -> Option<String> {
        let (d, p) = match x {
            Expr::Lit(v) => (lit(v, ty(other))?, Prec::Atom),
            x => deparse(x, cols, pretty)?,
        };
        Some(if pretty && parens(p, parent, left) { format!("({d})") } else { d })
    };
    let op = |d: String, p: Prec| Some((if pretty { d } else { format!("({d})") }, p));
    match e {
        Expr::Lit(v) => Some((lit(v, None)?, Prec::Atom)),
        Expr::Col(_, n) => Some((ident(n), Prec::Atom)),
        Expr::Bin(a, o, b) => {
            let (sym, p) = match o {
                Op::Or => ("OR", Prec::Or),
                Op::And => ("AND", Prec::And),
                Op::Eq => ("=", Prec::Cmp),
                Op::Ne => ("<>", Prec::Cmp),
                Op::Lt => ("<", Prec::Cmp),
                Op::Le => ("<=", Prec::Cmp),
                Op::Gt => (">", Prec::Cmp),
                Op::Ge => (">=", Prec::Cmp),
                Op::Add => ("+", Prec::Low),
                Op::Sub => ("-", Prec::Low),
                Op::Mul => ("*", Prec::High),
                Op::Div => ("/", Prec::High),
                Op::Mod => ("%", Prec::High),
                Op::Concat => ("||", Prec::Other),
                Op::BitAnd => ("&", Prec::Other),
                Op::BitOr => ("|", Prec::Other),
                Op::Shl => ("<<", Prec::Other),
                Op::Shr => (">>", Prec::Other),
                Op::WithinEq => ("<<=", Prec::Other),
                Op::ContainsEq => (">>=", Prec::Other),
                Op::Overlap => ("&&", Prec::Other),
                Op::Regex { ci: false, neg: false } => ("~", Prec::Other),
                Op::Regex { ci: true, neg: false } => ("~*", Prec::Other),
                Op::Regex { ci: false, neg: true } => ("!~", Prec::Other),
                Op::Regex { ci: true, neg: true } => ("!~*", Prec::Other),
                _ => return None,
            };
            op(format!("{} {sym} {}", side(a, b, p, true)?, side(b, a, p, false)?), p)
        }
        Expr::Not(x) => op(format!("NOT {}", side(x, x, Prec::Not, true)?), Prec::Not),
        // a cast to a network address type, as Postgres writes it: `(ip)::cidr`
        Expr::Cast(x, t) if net::kind_of(*t).is_some() => match &**x {
            Expr::Lit(v) => Some((lit(v, Some(*t))?, Prec::Atom)),
            x => Some((format!("({})::{}", deparse(x, cols, pretty)?.0, t.name()), Prec::Atom)),
        },
        Expr::IsNull(x, neg) => {
            op(format!("{} IS {}NULL", side(x, x, Prec::Cmp, true)?, if *neg { "NOT " } else { "" }), Prec::Cmp)
        }
        Expr::Like(a, p, ci, neg) => {
            let sym = match (ci, neg) {
                (false, false) => "~~",
                (true, false) => "~~*",
                (false, true) => "!~~",
                (true, true) => "!~~*",
            };
            op(format!("{} {sym} {}", side(a, p, Prec::Cmp, true)?, side(p, a, Prec::Cmp, false)?), Prec::Cmp)
        }
        Expr::In(x, list, neg) => {
            let items = list.iter().map(|i| side(i, x, Prec::Other, true)).collect::<Option<Vec<_>>>()?;
            let quant = if *neg { "<> ALL" } else { "= ANY" };
            op(format!("{} {quant} (ARRAY[{}])", side(x, x, Prec::Cmp, true)?, items.join(", ")), Prec::Cmp)
        }
        // the session's own names, as keywords
        Expr::Func(Func::Ctx(f), args) if args.is_empty() && !matches!(f, CtxFn::CurrentSetting) => Some((
            match f {
                CtxFn::CurrentUser => "CURRENT_USER",
                CtxFn::SessionUser => "SESSION_USER",
                CtxFn::CurrentRole => "CURRENT_ROLE",
                CtxFn::CurrentDatabase => "CURRENT_CATALOG",
                CtxFn::CurrentSchema => "CURRENT_SCHEMA",
                _ => return None,
            }
            .into(),
            Prec::Atom,
        )),
        // a setting, as tenant policies read it: `(current_setting('app.org'::text))::integer`
        Expr::Func(Func::Ctx(CtxFn::CurrentSetting), args) => {
            let args = args.iter().map(|a| deparse(a, cols, pretty).map(|d| d.0)).collect::<Option<Vec<_>>>()?;
            Some((format!("current_setting({})", args.join(", ")), Prec::Atom))
        }
        Expr::Cast(x, t) if matches!(**x, Expr::Func(Func::Ctx(CtxFn::CurrentSetting), _)) => {
            Some((format!("({})::{}", deparse(x, cols, pretty)?.0, t.pg_name()), Prec::Atom))
        }
        Expr::Func(f, args) => {
            let name = match f {
                Func::Lower => "lower",
                Func::Upper => "upper",
                Func::Length => "length",
                Func::Abs => "abs",
                Func::Scalar(funcs::Scalar::NetHost) => "host",
                Func::Scalar(funcs::Scalar::NetAbbrev) => "abbrev",
                Func::Scalar(funcs::Scalar::NetBroadcast) => "broadcast",
                Func::Scalar(funcs::Scalar::NetNetmask) => "netmask",
                Func::Scalar(funcs::Scalar::NetHostmask) => "hostmask",
                Func::Scalar(funcs::Scalar::NetNetwork) => "network",
                Func::Scalar(funcs::Scalar::NetMasklen) => "masklen",
                Func::Scalar(funcs::Scalar::NetFamily) => "family",
                Func::Scalar(funcs::Scalar::NetSetMasklen) => "set_masklen",
                Func::Scalar(funcs::Scalar::NetSameFamily) => "inet_same_family",
                Func::Scalar(funcs::Scalar::NetMerge) => "inet_merge",
                Func::Scalar(funcs::Scalar::NetSet7bit) => "macaddr8_set7bit",
                _ => return None,
            };
            let args = args.iter().map(|a| deparse(a, cols, pretty).map(|d| d.0)).collect::<Option<Vec<_>>>()?;
            Some((format!("{name}({})", args.join(", ")), Prec::Atom))
        }
        _ => None,
    }
}

/// The world's relations, their indexes and constraints, and its schemas, each with its oid worked out from its
/// name (see `Named`; public's is 2200, as in Postgres).
#[derive(Default)]
pub(crate) struct Catalog {
    pub rels: Vec<TableInfo>,
    /// the schemas other than public, by name
    pub schemas: Vec<String>,
    /// (relation, index), then (relation, constraint), in oid order
    pub idx: Vec<(usize, Idx)>,
    pub cons: Vec<(usize, Con)>,
    /// every database (world), oldest first: `pg_database`
    pub worlds: Vec<String>,
    /// the functions and procedures made with CREATE FUNCTION, in name order (see `catalog_objects`)
    pub(super) funcs: Vec<Arc<functions::Function>>,
    /// their oids (see `catalog_objects::func_oids`)
    pub(super) func_oids: Vec<i64>,
    /// the enum types, by stored name, with their labels (a domain is one with none)
    pub(super) enums: Vec<(String, Vec<String>)>,
    /// for each of `enums`, its labels' pg_enum.enumsortorder (see `enum_orders`)
    pub(super) enum_orders: Vec<Vec<f64>>,
    /// for each of `enums` that is a domain: whether it is NOT NULL, and its own CHECKs (name, condition)
    pub(super) domains: Vec<Option<DomainFacts>>,
    /// for each of `enums`, its typtype by its row (see `type_kind`)
    pub(super) type_kinds: Vec<char>,
    /// for each of `enums` that is a composite type: its attributes (name, type as written)
    pub(super) composites: Vec<Option<Vec<(String, String)>>>,
    /// each table column that has a default, as (relation, column)
    pub(super) defaults: Vec<(usize, usize)>,
    /// the views and materialized views (each has a `_RETURN` rule), by relation
    pub(super) rewrites: Vec<usize>,
    /// each trigger, as (relation, its place in the relation's)
    pub(super) triggers: Vec<(usize, usize)>,
    /// COMMENT ON's text, by key (see `comments`)
    pub(super) comments: HashMap<String, String>,
    /// foreign-data wrappers, servers and user mappings (see `foreign`)
    pub(super) foreign: super::foreign::Objects,
    /// the publications (see `publication`)
    pub(super) pubs: Vec<super::publication::Publication>,
    /// the statistics objects (see `statistics`)
    pub(super) stats: Vec<super::statistics::StatObj>,
    /// rules: the table, the rule's name and its row (see `rules`)
    pub(super) rules: Vec<(String, String, Json)>,
    /// owners and grants, by key (see `acl`)
    pub(super) acls: HashMap<String, super::acl::Entry>,
    /// the roles (see `roles`)
    pub(super) reg: Arc<super::roles::Registry>,
    /// the collations made with CREATE COLLATION, by stored name: name, definition (see `collation`)
    pub(super) colls: Vec<(String, Json)>,
    /// the extensions CREATE EXTENSION recorded (see `pgcrypto::EXTENSIONS`): name, schema, version
    pub(super) extensions: Vec<(String, String, String)>,
    /// the event triggers, by name, with their rows (see `userdefs::event_triggers`)
    pub(super) evts: Vec<(String, Json)>,
    /// the tablespaces, Postgres's two first (see `tablespace`)
    pub(super) spaces: Vec<super::tablespace::Space>,
    /// the access methods CREATE ACCESS METHOD made, by name (see `accessmethod`)
    pub(super) ams: Vec<super::accessmethod::MadeAm>,
    /// the oids of the relations, indexes, constraints, schemas, enum types, collations and row types, worked out
    /// from their names (see `Named`)
    pub(super) named: Named,
}

/// The oids of the objects made here that a reg value can name (relations, indexes, constraints, schemas, enum
/// types and domains, collations, and relations' row types), each worked out from its kind and stored name: FNV-1a
/// of the name, in a range of 2^26 of its own per kind below 2^30 (functions' oids are from 2^30 up), the next free
/// one where two names meet (in name order, so every world and every statement gives the same). An oid is then the
/// same in every world, and stays its object's whatever else is made or dropped: a kept regclass or regtype names
/// what it named. An enum's and a row type's array type is the oid after it (their slots go by twos).
#[derive(Default)]
pub(crate) struct Named {
    pub(super) rels: Vec<i64>,
    pub(super) idx: Vec<i64>,
    pub(super) cons: Vec<i64>,
    pub(super) schemas: Vec<i64>,
    pub(super) enums: Vec<i64>,
    pub(super) colls: Vec<i64>,
    pub(super) rows: Vec<i64>,
    /// the composite types' relations (pg_class relkind `c`), each with its place in `enums`
    pub(super) comps: Vec<(usize, i64)>,
    by: HashMap<i64, Slot>,
}

/// What a named oid is (see `Named`): a place in its kind's list in `Catalog`.
#[derive(Clone, Copy)]
enum Slot {
    Rel(usize),
    Idx(usize),
    Con(usize),
    Schema(usize),
    Enum(usize),
    Coll(usize),
    Row(usize),
    /// a composite type's relation, by its type's place in `enums`
    Comp(usize),
}

/// The kinds' ranges: the first at 2^26, each 2^26 long.
const NAMED_OIDS: i64 = 1 << 26;

/// The oids of `keys` (stored names) of the kind with this place (see `Named`), in their order; by twos with
/// `pairs`.
fn named_oids(kind: i64, keys: &[String], pairs: bool) -> Vec<i64> {
    let slots = if pairs { NAMED_OIDS / 2 } else { NAMED_OIDS };
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
    let (mut taken, mut out) = (HashSet::new(), vec![0; keys.len()]);
    for k in order {
        let h = keys[k].bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
        let mut slot = i64::from(h) % slots;
        while !taken.insert(slot) {
            slot = (slot + 1) % slots;
        }
        out[k] = (kind + 1) * NAMED_OIDS + if pairs { 2 * slot } else { slot };
    }
    out
}

/// The oids of the relations stored with these names (see `Named`): what `tableoid` and `pg_class.oid` say.
pub(super) fn rel_oids(names: &[String]) -> Vec<i64> {
    named_oids(0, names, false)
}

impl Named {
    fn of(c: &Catalog) -> Named {
        let rel_names: Vec<String> = c.rels.iter().map(|r| r.name.clone()).collect();
        let rels = rel_oids(&rel_names);
        let idx = named_oids(
            1,
            &c.idx.iter().map(|(r, x)| qualify(schema_part(&c.rels[*r].name), &x.name)).collect::<Vec<_>>(),
            false,
        );
        let cons = named_oids(
            2,
            &c.cons.iter().map(|(r, x)| format!("{}/{}", c.rels[*r].name, x.name)).collect::<Vec<_>>(),
            false,
        );
        let schemas = named_oids(3, &c.schemas, false);
        let enums = named_oids(4, &c.enums.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(), true);
        let colls = named_oids(5, &c.colls.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(), false);
        let rows = named_oids(6, &rel_names, true);
        // (a composite type's relation has an oid of a kind of its own, so making one moves no table's; kinds 7 to 10
        // meet the foreign objects' range, see `foreign::name_oids`. A shell type has none, as Postgres's has none)
        let made: Vec<usize> = (0..c.enums.len())
            .filter(|&k| c.composites.get(k).is_some_and(Option::is_some) && !c.is_shell(k))
            .collect();
        let comp_oids = named_oids(11, &made.iter().map(|&k| c.enums[k].0.clone()).collect::<Vec<_>>(), false);
        let comps: Vec<(usize, i64)> = made.into_iter().zip(comp_oids).collect();
        let mut by = HashMap::new();
        let mut add =
            |oids: &[i64], slot: fn(usize) -> Slot| by.extend(oids.iter().enumerate().map(|(k, o)| (*o, slot(k))));
        add(&rels, Slot::Rel);
        add(&idx, Slot::Idx);
        add(&cons, Slot::Con);
        add(&schemas, Slot::Schema);
        add(&enums, Slot::Enum);
        add(&colls, Slot::Coll);
        add(&rows, Slot::Row);
        by.extend(comps.iter().map(|&(k, o)| (o, Slot::Comp(k))));
        Named { rels, idx, cons, schemas, enums, colls, rows, comps, by }
    }
}

/// What an oid names.
pub(crate) enum Obj<'a> {
    Rel(usize),
    Index(usize, &'a Idx),
    Con(&'a Con),
    Schema,
    Func(usize),
    /// an enum type or its array type
    Enum,
    /// a trigger: relation, its place in the relation's
    Trigger(usize, usize),
}

impl Catalog {
    pub(crate) fn of(core: &crate::Core, branch: &str) -> Result<Catalog> {
        Catalog::build(core, branch, false)
    }

    /// `of`, with the running session's temporary tables too, in schema `pg_temp` (stored as `pg_temp.name`): what
    /// a query of the catalog and the catalog's functions (regclass, ...) see, as a Postgres session sees its own
    /// temporary tables in pg_class.
    pub(crate) fn of_session(core: &crate::Core, branch: &str) -> Result<Catalog> {
        Catalog::build(core, branch, true)
    }

    fn build(core: &crate::Core, branch: &str, temp: bool) -> Result<Catalog> {
        // (the world's own relations, not the temporary tables the session's statements find first by those names)
        let temp_tables = super::TEMP.take();
        let rels = tables(core, branch);
        super::TEMP.set(temp_tables);
        let mut rels = rels?;
        // (none when the statement runs in the temporary tables' database itself: they are its own then)
        if temp
            && let Some((db, temp_branch)) = super::TEMP.with_borrow(Clone::clone)
            && !std::ptr::eq(&*db.core, core)
        {
            for mut r in tables(&db.core, &temp_branch)? {
                if schema_part(&r.name) == "public" {
                    // (not `qualify`, which leaves pg_temp off as the parser's names have it)
                    r.name = format!("{TEMP_SCHEMA}.{}", r.name);
                    rels.push(r);
                }
            }
        }
        let schemas = namespaces(&core.view_tx(branch)?)?.into_iter().skip(1).collect();
        let idx = rels.iter().enumerate().flat_map(|(i, r)| r.indexes.iter().map(move |x| (i, x.clone()))).collect();
        let mut cons: Vec<(usize, Con)> =
            rels.iter().enumerate().flat_map(|(i, r)| r.constraints.iter().map(move |c| (i, c.clone()))).collect();
        // constraint triggers are constraints too (contype `t`), after the table's others
        for (i, r) in rels.iter().enumerate() {
            for (k, x) in r.triggers.iter().enumerate() {
                if let Some(Some((deferrable, deferred, _))) = r.trigger_cons.get(k) {
                    cons.push((i, trigger_con(&x.0, *deferrable, *deferred)));
                }
            }
        }
        let mut worlds = core.worlds();
        worlds.retain(|w| !w.name.starts_with("_tx_"));
        worlds.sort_by(|a, b| (a.created, &a.name).cmp(&(b.created, &b.name)));
        let worlds = worlds.into_iter().map(|w| w.name).collect();
        let v = core.view_tx(branch)?;
        let mut funcs = vec![];
        for r in v.prefix(&format!("{}/", functions::FUNCTIONS))? {
            funcs.push(functions::Function::from_raw(&r?.1)?);
        }
        let (mut enums, mut domains, mut enum_orders) = (vec![], vec![], vec![]);
        let (mut type_kinds, mut composites) = (vec![], vec![]);
        for r in v.prefix("_sonos_types/")? {
            let (k, raw) = r?;
            let j: Json = serde_json::from_str(&raw).unwrap_or_default();
            let labels = j["enum"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(String::from));
            let name = k["_sonos_types/".len()..].to_string();
            domains.push(j.get("domain").filter(|d| d.is_object()).map(|d| super::domain::own_constraints(d, &name)));
            composites.push(j["composite"].as_array().map(|fs| {
                fs.iter().filter_map(|f| Some((f[0].as_str()?.to_string(), f[1].as_str()?.to_string()))).collect()
            }));
            let labels: Vec<String> = labels.collect();
            enum_orders.push(super::enum_orders(&j, labels.len()));
            type_kinds.push(type_kind(&j));
            enums.push((name, labels));
        }
        let mut defaults = vec![];
        let (mut rewrites, mut triggers) = (vec![], vec![]);
        for (i, r) in rels.iter().enumerate() {
            if matches!(r.kind, 'r' | 'p' | 'f') {
                defaults.extend(r.columns.iter().enumerate().filter(|(_, c)| c.3.is_some()).map(|(j, _)| (i, j)));
            }
            if matches!(r.kind, 'v' | 'm') {
                rewrites.push(i);
            }
            triggers.extend((0..r.triggers.len()).map(|k| (i, k)));
        }
        let comments = super::comments::load(&v)?;
        let foreign = super::foreign::load(&v)?;
        let pubs = super::publication::load(&v)?;
        let stats = super::statistics::load(&v)?;
        let rules = super::rules::all(&v)?;
        let acls = super::acl::rows(&v)?;
        let reg = super::roles::registry(core)?;
        let func_oids = super::catalog_objects::func_oids(&funcs);
        let colls = super::collation::all(&v)?;
        let extensions = super::pgcrypto::extensions(&v)?;
        let evts = super::userdefs::event_triggers(&v)?;
        let spaces = super::tablespace::load(&v)?;
        let ams = super::accessmethod::all(&v)?;
        let mut cat = Catalog {
            colls,
            rels,
            schemas,
            idx,
            cons,
            worlds,
            funcs,
            func_oids,
            enums,
            enum_orders,
            domains,
            type_kinds,
            composites,
            defaults,
            rewrites,
            triggers,
            comments,
            foreign,
            pubs,
            stats,
            rules,
            acls,
            reg,
            extensions,
            evts,
            spaces,
            ams,
            named: Named::default(),
        };
        cat.named = Named::of(&cat);
        Ok(cat)
    }

    /// No objects at all: what a function that only reads types (format_type) is given outside a database.
    pub(crate) fn empty() -> Catalog {
        Catalog {
            rels: vec![],
            schemas: vec![],
            idx: vec![],
            cons: vec![],
            worlds: vec![],
            funcs: vec![],
            func_oids: vec![],
            enums: vec![],
            enum_orders: vec![],
            domains: vec![],
            type_kinds: vec![],
            composites: vec![],
            defaults: vec![],
            rewrites: vec![],
            triggers: vec![],
            comments: HashMap::new(),
            foreign: Default::default(),
            pubs: vec![],
            stats: vec![],
            rules: vec![],
            acls: HashMap::new(),
            reg: Arc::default(),
            colls: vec![],
            extensions: vec![],
            evts: vec![],
            spaces: vec![],
            ams: vec![],
            named: Named::default(),
        }
    }

    /// The `_sonos_comments` key of a relation of this pg_class kind.
    fn comment_kind(kind: char) -> Option<&'static str> {
        Some(match kind {
            'r' | 'p' | 'f' => "table",
            'v' => "view",
            'm' => "materialized view",
            'S' => "sequence",
            _ => return None,
        })
    }

    /// Whether a comment key still names something (see `comments::after`).
    pub(super) fn has_comment_target(&self, key: &str) -> bool {
        let Some((kind, rest)) = key.split_once('/') else { return false };
        let rel_named = |name: &str| self.rels.iter().find(|r| r.name == name);
        match kind {
            "table" | "view" | "materialized view" | "sequence" => {
                rel_named(rest).is_some_and(|r| Self::comment_kind(r.kind) == Some(kind))
            }
            "index" => self.idx.iter().any(|(r, x)| qualify(schema_part(&self.rels[*r].name), &x.name) == rest),
            "column" => {
                self.rels.iter().any(|r| {
                    rest.strip_prefix(&r.name)
                        .and_then(|c| c.strip_prefix('/'))
                        .is_some_and(|c| r.columns.iter().any(|x| x.0 == c))
                }) || rest.rsplit_once('/').is_some_and(|(t, c)| self.composite_attnum(t, c).is_some())
            }
            "constraint" => self.cons.iter().any(|(r, c)| {
                rest.strip_prefix(&self.rels[*r].name).and_then(|n| n.strip_prefix('/')).is_some_and(|n| n == c.name)
            }),
            "domain constraint" => rest.rsplit_once('/').is_some_and(|(d, n)| self.domain_check_oid(d, n).is_some()),
            "schema" => {
                matches!(rest, "public" | "pg_catalog" | "information_schema") || self.schemas.iter().any(|s| s == rest)
            }
            "fdw" => self.foreign.fdw(rest).is_some(),
            "server" => self.foreign.server(rest).is_some(),
            "type" => self.enums.iter().any(|(n, _)| n == rest),
            "function" => self.funcs.iter().any(|f| f.sig() == rest),
            // (lo_unlink takes a large object's comment with it)
            "large object" => true,
            "am" => self.am_oid(rest) != 0,
            _ => false,
        }
    }

    /// Every comment as pg_description holds it: object, catalog, column number (0: the object), text.
    pub(super) fn descriptions(&self) -> Vec<(i64, i64, i64, String)> {
        let mut out = vec![];
        for (key, text) in &self.comments {
            let Some((kind, rest)) = key.split_once('/') else { continue };
            let found = match kind {
                "table" | "view" | "materialized view" | "sequence" => self
                    .rels
                    .iter()
                    .position(|r| r.name == rest && Self::comment_kind(r.kind) == Some(kind))
                    .map(|i| (self.rel_oid(i), 1259, 0)),
                "index" => self
                    .idx
                    .iter()
                    .position(|(r, x)| qualify(schema_part(&self.rels[*r].name), &x.name) == rest)
                    .map(|k| (self.idx_oid(k), 1259, 0)),
                "column" => self
                    .rels
                    .iter()
                    .enumerate()
                    .find_map(|(i, r)| {
                        let c = rest.strip_prefix(&r.name)?.strip_prefix('/')?;
                        Some((self.rel_oid(i), 1259, r.attnum(r.columns.iter().position(|x| x.0 == c)?)))
                    })
                    // (a composite type's attribute: a column of its relation)
                    .or_else(|| {
                        let (t, c) = rest.rsplit_once('/')?;
                        let (k, n) = self.composite_attnum(t, c)?;
                        Some((self.comp_oid(k)?, 1259, n))
                    }),
                "domain constraint" => {
                    rest.rsplit_once('/').and_then(|(d, n)| self.domain_check_oid(d, n)).map(|o| (o, 2606, 0))
                }
                "constraint" => self.cons.iter().enumerate().find_map(|(k, (r, c))| {
                    let n = rest.strip_prefix(&self.rels[*r].name)?.strip_prefix('/')?;
                    (n == c.name).then(|| (self.con_oid(k), 2606, 0))
                }),
                "schema" => Some((self.schema_oid(rest), 2615, 0)).filter(|(o, ..)| *o != 0),
                "fdw" => self.foreign.fdw_oid(rest).map(|o| (o, 2328, 0)),
                "server" => self.foreign.server_oid(rest).map(|o| (o, 1417, 0)),
                "type" => self.enums.iter().position(|(n, _)| n == rest).map(|k| (self.enum_oid(k), 1247, 0)),
                "function" => self.funcs.iter().position(|f| f.sig() == rest).map(|k| (self.func_oid(k), 1255, 0)),
                // (pg_description has no role's: pg_shdescription does)
                "role" => None,
                "publication" => super::publication::publication_oids(self)
                    .into_iter()
                    .find(|(_, k)| self.pubs[*k].name == rest)
                    .map(|(o, _)| (o, 6104, 0)),
                "large object" => rest.parse().ok().map(|o| (o, 2613, 0)),
                "am" => Some((self.am_oid(rest), 2601, 0)).filter(|(o, ..)| *o != 0),
                _ => None,
            };
            if let Some((oid, class, sub)) = found {
                out.push((oid, class, sub, text.clone()));
            }
        }
        out.sort();
        out
    }

    /// obj_description and col_description: the comment on the object `oid` (or on its column
    /// number `sub`), in the catalog `class` names (any, for None).
    pub(super) fn comment_on(&self, oid: i64, class: Option<&str>, sub: i64) -> Option<&str> {
        let class = class.map(|c| c.trim_start_matches("pg_catalog.").to_lowercase());
        let is = |want: &str| class.as_deref().is_none_or(|c| c == want);
        let fixed = match oid {
            2200 => Some("public"),
            11 => Some("pg_catalog"),
            13000 => Some("information_schema"),
            _ => None,
        };
        let key = match (fixed, self.named.by.get(&oid)) {
            (Some(name), _) if sub == 0 && is("pg_namespace") => format!("schema/{name}"),
            (None, Some(Slot::Rel(k))) => {
                let rel = &self.rels[*k];
                match sub {
                    _ if !is("pg_class") => return None,
                    0 => format!("{}/{}", Self::comment_kind(rel.kind)?, rel.name),
                    n => {
                        format!("column/{}/{}", rel.name, rel.columns.get(rel.at_attnum(n)?)?.0)
                    }
                }
            }
            (None, Some(Slot::Idx(k))) if sub == 0 && is("pg_class") => {
                let (rel, i) = &self.idx[*k];
                format!("index/{}", qualify(schema_part(&self.rels[*rel].name), &i.name))
            }
            (None, Some(Slot::Con(k))) if sub == 0 && is("pg_constraint") => {
                let (rel, con) = &self.cons[*k];
                format!("constraint/{}/{}", self.rels[*rel].name, con.name)
            }
            (None, Some(Slot::Schema(k))) if sub == 0 && is("pg_namespace") => format!("schema/{}", self.schemas[*k]),
            (None, Some(Slot::Enum(k))) if sub == 0 && is("pg_type") => format!("type/{}", self.enums[*k].0),
            // (a composite type's attribute, a column of its relation)
            (None, Some(Slot::Comp(k))) if sub > 0 && is("pg_class") => {
                let fields = self.composites.get(*k)?.as_ref()?;
                let (attr, _) = fields.get(usize::try_from(sub - 1).ok()?)?;
                format!("column/{}/{attr}", self.enums[*k].0)
            }
            // (a domain's constraint)
            (None, None) if sub == 0 && is("pg_constraint") && self.domain_checks().iter().any(|d| d.0 == oid) => {
                let (_, k, n, _) = self.domain_checks().into_iter().find(|d| d.0 == oid)?;
                format!("domain constraint/{}/{n}", self.enums[k].0)
            }
            // (a publication's, by name)
            (None, _) if sub == 0 && is("pg_publication") => {
                let (_, k) = super::publication::publication_oids(self).into_iter().find(|(o, _)| *o == oid)?;
                format!("publication/{}", self.pubs[k].name)
            }
            // (a role's, which Postgres keeps in pg_shdescription: by its oid, so it stays through a rename)
            (None, _) if sub == 0 && is("pg_authid") && self.reg.by_oid(oid).is_some() => format!("role/{oid}"),
            _ if sub == 0 && class.as_deref() == Some("pg_largeobject") => format!("large object/{oid}"),
            // (an access method's, by name)
            (None, _) if sub == 0 && is("pg_am") && self.access_methods().iter().any(|a| a.0 == oid) => {
                format!("am/{}", self.access_methods().into_iter().find(|a| a.0 == oid)?.1)
            }
            (None, _) if sub == 0 && is("pg_proc") && self.func_oids.contains(&oid) => {
                let k = self.func_oids.iter().position(|o| *o == oid)?;
                format!("function/{}", self.funcs[k].sig())
            }
            _ => return None,
        };
        self.comments.get(&key).map(String::as_str)
    }

    /// Relation `r`'s owner and its ACL as pg_class shows them (a serial's sequence: its table's owner).
    pub(super) fn rel_acl(&self, r: &TableInfo) -> (i64, Value) {
        let e = self.acls.get(&format!("rel/{}", r.name));
        let owner = match (&r.owner, e.and_then(|e| e.owner)) {
            (Some((t, _, _)), None) if r.kind == 'S' => {
                self.acls.get(&format!("rel/{t}")).and_then(|e| e.owner).unwrap_or(super::roles::BOOTSTRAP)
            }
            _ => super::roles::BOOTSTRAP,
        };
        super::acl::shown(&self.reg, e, owner)
    }

    /// The access methods as pg_am lists them: Postgres's own (pgvector's hnsw and contrib/bloom's bloom once an index
    /// uses it, as Postgres has it only where the extension is), then those CREATE ACCESS METHOD made: oid, name,
    /// handler, kind (`i`, `t`).
    pub(super) fn access_methods(&self) -> Vec<(i64, String, String, char)> {
        let own = AMS
            .iter()
            .filter(|(_, n, ..)| !["hnsw", "bloom"].contains(n) || self.idx.iter().any(|(_, x)| x.method == *n))
            .map(|(o, n, h, k)| (*o, n.to_string(), h.to_string(), k.chars().next().unwrap_or('i')));
        own.chain(self.ams.iter().map(|a| (a.oid, a.name.clone(), a.handler.clone(), a.kind))).collect()
    }

    /// Access method `name`'s oid, Postgres's own or one made here (0: none).
    pub(super) fn am_oid(&self, name: &str) -> i64 {
        match AMS.iter().find(|(_, n, ..)| *n == name) {
            Some((o, ..)) => *o,
            None => self.ams.iter().find(|a| a.name == name).map_or(0, |a| a.oid),
        }
    }

    /// Relation `i`'s oid, worked out from its name (see `Named`).
    pub(crate) fn rel_oid(&self, i: usize) -> i64 {
        self.named.rels[i]
    }

    pub(crate) fn idx_oid(&self, k: usize) -> i64 {
        self.named.idx[k]
    }

    pub(crate) fn con_oid(&self, k: usize) -> i64 {
        self.named.cons[k]
    }

    /// A schema's oid: Postgres's for its own, else worked out from its name (0: no such schema).
    pub(crate) fn schema_oid(&self, s: &str) -> i64 {
        match s {
            "public" => 2200,
            "pg_catalog" => 11,
            "information_schema" => 13000,
            "pg_toast" => super::catalog_objects::TOAST_SCHEMA_OID,
            TEMP_SCHEMA => TEMP_SCHEMA_OID,
            s => self.schemas.iter().position(|x| x == s).map_or(0, |k| self.named.schemas[k]),
        }
    }

    /// The index relation `i` has named `name`'s oid.
    pub(crate) fn index_of(&self, rel: usize, name: &str) -> Option<i64> {
        self.idx.iter().position(|(r, x)| *r == rel && x.name == name).map(|k| self.idx_oid(k))
    }

    /// The CHECKs of the domains with their oids: (oid, the domain's place in `enums`, name, condition over VALUE).
    pub(super) fn domain_checks(&self) -> Vec<(i64, usize, &String, &String)> {
        let named: Vec<(usize, &String, &String)> = self
            .domains
            .iter()
            .enumerate()
            .filter_map(|(k, d)| Some((k, d.as_ref()?)))
            .flat_map(|(k, (_, checks))| checks.iter().map(move |(n, text)| (k, n, text)))
            .collect();
        let oids = named_oids(
            7,
            &named.iter().map(|(k, n, _)| format!("{}/{n}", self.enums[*k].0)).collect::<Vec<_>>(),
            false,
        );
        named.into_iter().zip(oids).map(|((k, n, text), oid)| (oid, k, n, text)).collect()
    }

    /// The oid of check `name` of domain `domain` (its stored name), if it has one so named.
    fn domain_check_oid(&self, domain: &str, name: &str) -> Option<i64> {
        self.domain_checks().into_iter().find(|(_, k, n, _)| self.enums[*k].0 == domain && *n == name).map(|d| d.0)
    }

    /// Composite type `name`'s (its stored name) attribute `attr`: the type's place in `enums`, and the attribute's
    /// number.
    pub(super) fn composite_attnum(&self, name: &str, attr: &str) -> Option<(usize, i64)> {
        let k = self.enums.iter().position(|(n, _)| n == name)?;
        let at = self.composites.get(k)?.as_ref()?.iter().position(|(a, _)| a == attr)?;
        Some((k, at as i64 + 1))
    }

    pub(crate) fn find(&self, oid: i64) -> Option<Obj<'_>> {
        match self.named.by.get(&oid) {
            Some(Slot::Rel(i)) => Some(Obj::Rel(*i)),
            Some(Slot::Idx(k)) => {
                let (rel, i) = &self.idx[*k];
                Some(Obj::Index(*rel, i))
            }
            Some(Slot::Con(k)) => Some(Obj::Con(&self.cons[*k].1)),
            Some(Slot::Schema(_)) => Some(Obj::Schema),
            _ => self.object(oid),
        }
    }

    /// Enum type `k`'s oid (see `Named`); its array type's is the next.
    pub(crate) fn enum_oid(&self, k: usize) -> i64 {
        self.named.enums[k]
    }

    /// The oid of collation `k` of those made with CREATE COLLATION (see `Named`).
    pub(crate) fn coll_oid(&self, k: usize) -> i64 {
        self.named.colls[k]
    }

    /// The oid of the composite type a typed table (`CREATE TABLE ... OF type`) was made of (`pg_class.reloftype`); 0
    /// for any other relation.
    pub(crate) fn of_type_oid(&self, r: &TableInfo) -> i64 {
        let Some(ty) = &r.hier.of_type else { return 0 };
        self.enums.iter().position(|(n, _)| n == ty).map_or(0, |k| self.enum_oid(k))
    }

    /// The oid of relation `i`'s row type (`pg_class.reltype`); its array type's is the next.
    pub(crate) fn row_type_oid(&self, i: usize) -> i64 {
        self.named.rows[i]
    }

    /// The oid of composite type `k`'s relation (`k` its place in `enums`; `pg_type.typrelid`), if it is one.
    pub(crate) fn comp_oid(&self, k: usize) -> Option<i64> {
        self.named.comps.iter().find(|(c, _)| *c == k).map(|(_, o)| *o)
    }

    /// The relation whose row type (or, with `array`, that type's array type) has this oid.
    pub(super) fn row_type_of(&self, oid: i64, array: bool) -> Option<usize> {
        match self.named.by.get(&(oid - i64::from(array))) {
            Some(Slot::Row(i)) if "rvmpf".contains(self.rels[*i].kind) => Some(*i),
            _ => None,
        }
    }

    /// The enum type (its place in `enums`) whose oid this is, or with `array` whose array type's (a shell type has
    /// none).
    pub(super) fn enum_at(&self, oid: i64, array: bool) -> Option<usize> {
        match self.named.by.get(&(oid - i64::from(array))) {
            Some(Slot::Enum(k)) if !(array && self.is_shell(*k)) => Some(*k),
            _ => None,
        }
    }

    /// Whether type `k` (its place in `enums`) is a shell type, `CREATE TYPE name` not yet filled in: as Postgres's,
    /// it has no relation and no array type (see `type_kind`).
    pub(super) fn is_shell(&self, k: usize) -> bool {
        self.type_kinds.get(k) == Some(&'p')
    }

    /// Whether this oid is an enum type's (or its array type's).
    pub(super) fn is_enum_oid(&self, oid: i64) -> bool {
        self.enum_at(oid, false).or(self.enum_at(oid, true)).is_some()
    }

    /// The collation made here (its place in `colls`) whose oid this is.
    pub(super) fn coll_at(&self, oid: i64) -> Option<usize> {
        match self.named.by.get(&oid) {
            Some(Slot::Coll(k)) => Some(*k),
            _ => None,
        }
    }

    /// The oid of the relation (or index) a stored name names.
    pub(crate) fn oid_of(&self, name: &str) -> Option<i64> {
        if let Some(i) = self.rels.iter().position(|r| r.name == name) {
            return Some(self.rel_oid(i));
        }
        let (s, n) = (schema_part(name), rel_part(name));
        let k = self.idx.iter().position(|(r, x)| x.name == n && schema_part(&self.rels[*r].name) == s)?;
        Some(self.idx_oid(k))
    }

    /// The oid a relation's name (as text, maybe schema-qualified, quoted or not) means; None if none.
    pub(super) fn named(&self, text: &str) -> Result<Option<i64>> {
        Ok(match name_parts(text).as_slice() {
            [n] => self.rel_named(None, n),
            [s, n] => self.rel_named(Some(s), n),
            _ => None,
        })
    }

    /// The oid of the relation (or index) `name` in `schema`, or along the search_path: a table, view, sequence or
    /// index made here, else a catalog table of the name, as Postgres numbers them.
    pub(super) fn rel_named(&self, schema: Option<&str>, name: &str) -> Option<i64> {
        match schema {
            Some("pg_catalog") => {
                return table_oid(name)
                    .or_else(|| self.oid_of(name))
                    .or_else(|| super::catalog_objects::kept_nowhere_named(self, "pg_catalog", name));
            }
            // (a TOAST table: see `catalog_objects::toast_class_rows`)
            Some("pg_toast") => return super::catalog_objects::kept_nowhere_named(self, "pg_toast", name),
            Some(TEMP_SCHEMA) => return self.oid_of(&format!("{TEMP_SCHEMA}.{name}")),
            Some(s) => return self.oid_of(&qualify(s, name)),
            None => {}
        }
        // a temporary table (or a temporary table's index) first, as Postgres reads pg_temp first; else the first
        // schema of the path that has a relation or an index of the name (a serial's sequence too), public only where
        // it is on the path
        let temp_index =
            || self.idx.iter().any(|(r, x)| x.name == name && schema_part(&self.rels[*r].name) == TEMP_SCHEMA);
        if (has_temp(name) || temp_index())
            && let Some(o) = self.oid_of(&format!("{TEMP_SCHEMA}.{name}"))
        {
            return Some(o);
        }
        let first = find_named(name, Kind::Rel);
        if (first != name || has_temp(name))
            && let Some(o) = self.oid_of(&first)
        {
            return Some(o);
        }
        for s in path_schemas().unwrap_or_else(|| vec!["public".into()]) {
            if let Some(o) = self.oid_of(&qualify(&s, name)) {
                return Some(o);
            }
        }
        // the catalog's own tables, as Postgres numbers them, and their primary keys' indexes
        table_oid(name).or_else(|| super::catalog_objects::kept_nowhere_named(self, "pg_catalog", name))
    }

    /// A relation's (or index's) name as a regclass shows it: bare where the search_path finds it by that name, else
    /// with its schema, each part quoted where it needs it.
    fn shown(&self, oid: i64) -> Option<String> {
        if let Some((name, _)) = CATALOG_OIDS.iter().find(|(_, o)| *o == oid).filter(|_| self.find(oid).is_none()) {
            return Some((*name).to_string());
        }
        // a catalog's primary key index (pg_catalog's, on every path) or a TOAST table (pg_toast's, on none)
        if self.find(oid).is_none()
            && let Some((schema, name)) = super::catalog_objects::kept_nowhere_name(self, oid)
        {
            return Some(if schema == "pg_toast" { format!("pg_toast.{name}") } else { name });
        }
        let stored = match self.find(oid)? {
            Obj::Rel(i) => self.rels[i].name.clone(),
            Obj::Index(r, x) => qualify(schema_part(&self.rels[r].name), &x.name),
            _ => return None,
        };
        let (schema, bare) = (schema_part(&stored), rel_part(&stored));
        let quoted = super::collation::quoted;
        Some(match self.rel_named(None, bare) == Some(oid) {
            true => quoted(bare),
            false => format!("{}.{}", quoted(schema), quoted(bare)),
        })
    }

    /// pg_get_indexdef: `CREATE [UNIQUE] INDEX name ON schema.table USING method (parts) [WHERE
    /// ...]`, or with `col` > 0 just that part. `pretty` leaves the schema off a table the search_path shows.
    fn index_def(&self, rel: usize, x: &Idx, col: i64, pretty: bool) -> String {
        if col > 0 {
            // (one part alone, an expression in the parentheses of its own the whole definition has it in too)
            let k = col as usize - 1;
            return if pretty { x.parts_pretty.get(k) } else { x.parts.get(k) }.cloned().unwrap_or_default();
        }
        let t = &self.rels[rel].name;
        let shown = shown_name(t);
        let on = if !pretty {
            format!("{}.{}", ident(schema_part(t)), ident(rel_part(t)))
        } else if shown.contains('.') {
            shown
        } else {
            ident(&shown)
        };
        let unique = if x.unique { "UNIQUE " } else { "" };
        // (an index of a partitioned table is made on the table alone, and its partitions' attached to it)
        let only = if self.rels[rel].kind == 'p' { "ONLY " } else { "" };
        let mut d = format!(
            "CREATE {unique}INDEX {} ON {only}{on} USING {} {}",
            ident(&x.name),
            ident(x.am_name()),
            x.columns_def(pretty)
        );
        if x.nnd {
            d.push_str(" NULLS NOT DISTINCT");
        }
        if let Some(p) = if pretty { &x.pred_pretty } else { &x.pred } {
            d.push_str(&format!(" WHERE {p}"));
        }
        d
    }
}

/// The world a statement reads: its database, its name, and its catalog once built.
/// A domain's NOT NULL, and its own CHECKs as (name, condition).
pub(super) type DomainFacts = (bool, Vec<(String, String)>);

pub(crate) type Reading = (Arc<crate::Core>, String, Option<Arc<Catalog>>);

thread_local! {
    /// The world the running statement reads, and its catalog once a function has needed it.
    static WORLD: RefCell<Option<Reading>> = const { RefCell::new(None) };
    /// The world's catalog is being built: what reads values while building it (a regproc shown in a row's JSON view)
    /// does without it, rather than building it again.
    static BUILDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

thread_local! {
    /// The session's own world while a statement runs in its temporary tables' database (see `at_home`).
    static HOME: RefCell<Option<Reading>> = const { RefCell::new(None) };
    /// The agent the session acts for there (empty: none): the temporary tables' database acts for no one.
    static HOME_ACTOR: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Runs `f`, a statement on the temporary tables' database (which has no types of its own), with what reads reg
/// values' names (a regtype[] going into a column) reading the session's world `branch` instead: its enums are
/// there, and its functions (see `home`). `actor`: the agent the session acts for, which `f` doesn't.
pub(super) fn at_home<T>(core: &Arc<crate::Core>, branch: &str, actor: &str, f: impl FnOnce() -> T) -> T {
    let before = HOME.replace(Some((core.clone(), branch.to_string(), None)));
    let who = HOME_ACTOR.replace(actor.to_string());
    let out = f();
    HOME.set(before);
    HOME_ACTOR.set(who);
    out
}

/// The session's world a statement on its temporary tables' database runs for (see `at_home`): its database and
/// name, and the agent the session acts for.
pub(super) fn home() -> Option<(Arc<crate::Core>, String, String)> {
    let (core, world, _) = HOME.with_borrow(Clone::clone)?;
    Some((core, world, HOME_ACTOR.with_borrow(Clone::clone)))
}

/// Runs `f` with the catalog functions (regclass, pg_get_constraintdef, ...) reading world
/// `branch`.
pub(super) fn in_world<T>(core: &Arc<crate::Core>, branch: &str, f: impl FnOnce() -> T) -> T {
    let before = WORLD.replace(Some((core.clone(), branch.to_string(), None)));
    let out = f();
    WORLD.set(before);
    out
}

/// The world the running statement reads (see `in_world`): what a helper thread working for it reads too (see
/// `restore_world`).
pub(crate) fn world_state() -> Option<Reading> {
    WORLD.with_borrow(Clone::clone)
}

/// Sets the world this thread's statement reads, as `world_state` gave it.
pub(crate) fn restore_world(w: Option<Reading>) {
    WORLD.set(w);
}

/// The database and name of the world the running statement reads (see `in_world`): `view_tx` of it is a view of
/// what reads rows the catalog doesn't keep (a large object's, a domain's).
pub(super) fn world_owner() -> Result<(Arc<crate::Core>, String)> {
    let Some((core, world, _)) = WORLD.with_borrow(Clone::clone) else {
        return err(UNSUPPORTED, "the catalog's functions need a database");
    };
    Ok((core, world))
}

/// Runs `f` with a view of the world the running statement reads.
pub(super) fn with_world_view<T>(f: impl FnOnce(&View) -> Result<T>) -> Result<T> {
    let (core, world) = world_owner()?;
    let v = core.view_tx(&world)?;
    f(&v)
}

/// A built-in type's `typname` (`int4`, `_text` is its array's): the first name regtype reads for it.
pub(super) fn builtin_typname(oid: i64) -> Option<&'static str> {
    PG_TYPES.iter().find(|t| t.0 == oid).and_then(|t| t.2.first()).map(|n| n.trim_matches('"'))
}

/// The running statement's world's catalog (built once for the statement).
pub(super) fn current_catalog() -> Result<Arc<Catalog>> {
    catalog_now()
}

/// The table (its stored name) `oid` is, or the index (its table's stored name and its own): what the
/// statistics functions find their object by (see `pgstat`).
pub(super) fn stored_rel(oid: i64) -> Option<(String, Option<String>)> {
    let cat = catalog_now().ok()?;
    match cat.find(oid)? {
        Obj::Rel(i) => Some((cat.rels[i].name.clone(), None)),
        Obj::Index(r, x) => Some((cat.rels[r].name.clone(), Some(x.name.clone()))),
        _ => None,
    }
}

/// The oid a relation stored as `name` has where no other name shares its slot (see `named_oids`): how the
/// statistics find one dropped since.
pub(super) fn lone_rel_oid(name: &str) -> i64 {
    named_oids(0, &[name.to_string()], false)[0]
}

/// The same for index `index` of table `table`.
pub(super) fn lone_index_oid(table: &str, index: &str) -> i64 {
    named_oids(1, &[qualify(schema_part(table), index)], false)[0]
}

fn catalog_now() -> Result<Arc<Catalog>> {
    let Some((core, world, cat)) = WORLD.with_borrow(Clone::clone) else {
        return err(UNSUPPORTED, "the catalog's functions need a database");
    };
    if let Some(c) = cat {
        return Ok(c);
    }
    if BUILDING.get() {
        return err(UNSUPPORTED, "the catalog is being built");
    }
    BUILDING.set(true);
    let built = Catalog::of_session(&core, &world);
    BUILDING.set(false);
    let c = Arc::new(built?);
    WORLD.with_borrow_mut(|w| {
        if let Some(w) = w {
            w.2 = Some(c.clone());
        }
    });
    Ok(c)
}

/// pg_partition_tree(table) and pg_partition_ancestors(table): their rows, as (relid, parentrelid,
/// isleaf, level) and (relid), relations as regclass values.
pub(super) fn partition_rows(name: &str, arg: &Value) -> Result<Vec<Vec<Value>>> {
    use Value::*;
    let cat = catalog_now()?;
    let oid = match arg {
        Int(o) => Some(*o),
        v => {
            let t = v.to_text();
            match t.trim().parse::<i64>() {
                Ok(o) => Some(o),
                Err(_) => cat.named(&t)?,
            }
        }
    };
    // a partitioned index's tree: its partitions' indexes attached to it, and theirs
    if let Some(Obj::Index(r, x)) = oid.and_then(|o| cat.find(o)) {
        let start = cat.idx.iter().position(|(q, y)| *q == r && y.name == x.name);
        return Ok(start.map_or_else(Vec::new, |k| index_tree_rows(&cat, name, k)));
    }
    let Some(Obj::Rel(start)) = oid.and_then(|o| cat.find(o)) else {
        // (an oid that names no table, or an index, has no tree; a name that names nothing is an error)
        if oid.is_none() {
            return err(NO_TABLE, format!("relation \"{}\" does not exist", arg.to_text()));
        }
        return Ok(vec![]);
    };
    let by_name = |n: &str| cat.rels.iter().position(|r| r.name == n);
    let show = |i: usize| Reg(cat.rel_oid(i), super::Reg::Class);
    let in_tree = |i: usize| cat.rels[i].hier.part.is_some() || cat.rels[i].hier.of.is_some();
    if name == "pg_partition_ancestors" {
        if !in_tree(start) {
            return Ok(vec![]);
        }
        let mut out = vec![vec![show(start)]];
        let mut cur = start;
        while let Some((p, _, _)) = &cat.rels[cur].hier.of {
            let Some(pi) = by_name(p) else { break };
            out.push(vec![show(pi)]);
            cur = pi;
        }
        return Ok(out);
    }
    // the tree below it (only a partitioned table or a partition is in one), each with its parent and level
    if !in_tree(start) {
        return Ok(vec![]);
    }
    let mut out = vec![];
    // (as Postgres lists them: a level at a time, each table's own in the order they were made)
    let mut level = vec![(start, None::<usize>)];
    let mut depth = 0;
    while !level.is_empty() {
        let mut next = vec![];
        for (i, parent) in &level {
            let r = &cat.rels[*i];
            out.push(vec![show(*i), parent.map_or(Null, show), Bool(r.hier.part.is_none()), Int(depth)]);
            for k in &r.hier.created {
                if let Some(ki) = cat.rels.iter().position(|x| x.name == *k)
                    && cat.rels[ki].hier.of.is_some()
                {
                    next.push((ki, Some(*i)));
                }
            }
        }
        level = next;
        depth += 1;
    }
    // (the table asked about shows its own parent, if it is a partition)
    if let Some(first) = out.first_mut()
        && let Some((p, ..)) = &cat.rels[start].hier.of
        && let Some(pi) = by_name(p)
    {
        first[1] = show(pi);
    }
    Ok(out)
}

/// Index `k`'s parent index (places in `Catalog::idx`): a partition's index attached to its parent's.
fn idx_parent(cat: &Catalog, k: usize) -> Option<usize> {
    let (ri, x) = &cat.idx[k];
    let given = x.given.as_ref().filter(|g| !g.is_empty())?;
    let (p, _, _) = cat.rels[*ri].hier.of.as_ref()?;
    cat.idx.iter().position(|(r, y)| cat.rels[*r].name == *p && y.name == *given)
}

/// Whether index `k` is in a partitioned index's tree (Postgres's check_rel_can_be_partition): the index of a
/// partitioned table, or one attached to its table's parent's.
fn idx_in_tree(cat: &Catalog, k: usize) -> bool {
    cat.rels[cat.idx[k].0].hier.part.is_some() || idx_parent(cat, k).is_some()
}

/// The indexes attached to index `k`, in the order their tables were made.
fn idx_children(cat: &Catalog, k: usize) -> Vec<usize> {
    let mut out = vec![];
    for c in &cat.rels[cat.idx[k].0].hier.created {
        for (j, (r, _)) in cat.idx.iter().enumerate() {
            if cat.rels[*r].name == *c && idx_parent(cat, j) == Some(k) {
                out.push(j);
            }
        }
    }
    out
}

/// The topmost index of index `k`'s tree (pg_partition_root of an index); None outside one.
fn idx_root(cat: &Catalog, k: usize) -> Option<usize> {
    if !idx_in_tree(cat, k) {
        return None;
    }
    let mut cur = k;
    while let Some(p) = idx_parent(cat, cur) {
        cur = p;
    }
    Some(cur)
}

/// `partition_rows` for index `start`: its tree a level at a time, or its ancestors.
fn index_tree_rows(cat: &Catalog, name: &str, start: usize) -> Vec<Vec<Value>> {
    use Value::*;
    if !idx_in_tree(cat, start) {
        return vec![];
    }
    let show = |k: usize| Reg(cat.idx_oid(k), super::Reg::Class);
    if name == "pg_partition_ancestors" {
        let mut out = vec![vec![show(start)]];
        let mut cur = start;
        while let Some(p) = idx_parent(cat, cur) {
            out.push(vec![show(p)]);
            cur = p;
        }
        return out;
    }
    // (a leaf is a table's index that has no partitions: one on a partitioned table isn't, even with none)
    let leaf = |k: usize| cat.rels[cat.idx[k].0].hier.part.is_none();
    let (mut out, mut depth) = (vec![], 0);
    let mut level = vec![(start, idx_parent(cat, start))];
    while !level.is_empty() {
        let mut next = vec![];
        for (k, parent) in &level {
            out.push(vec![show(*k), parent.map_or(Null, show), Bool(leaf(*k)), Int(depth)]);
            next.extend(idx_children(cat, *k).into_iter().map(|c| (c, Some(*k))));
        }
        level = next;
        depth += 1;
    }
    out
}

/// A type's typtype, its element type if it is a true array, and its base type if it is a domain (see `TYPE_SHAPES`).
type TypeShape = (char, Option<i64>, i64);

/// Postgres's built-in types (pg_type's rows, see `catalog_schema`) as IsBinaryCoercible asks of them, by oid: the
/// typtype, the element type of a true array (typelem set, typcategory `A`), and the base type of a domain.
static TYPE_SHAPES: std::sync::LazyLock<HashMap<i64, TypeShape>> = std::sync::LazyLock::new(|| {
    let cols = catalog_schema::columns("pg_type");
    let at = |n: &str| cols.iter().position(|(c, _)| c == n);
    let (Some(o), Some(kind), Some(category), Some(elem), Some(base)) =
        (at("oid"), at("typtype"), at("typcategory"), at("typelem"), at("typbasetype"))
    else {
        return HashMap::new();
    };
    let int = |v: &Value| if let Value::Int(n) = v { *n } else { 0 };
    (catalog_schema::builtin_rows("pg_type").iter())
        .map(|r| {
            let array = int(&r[elem]) != 0 && r[category].to_text() == "A";
            (int(&r[o]), (r[kind].to_text().chars().next().unwrap_or('b'), array.then(|| int(&r[elem])), int(&r[base])))
        })
        .collect()
});

/// Postgres's built-in casts that are binary-coercible (castmethod `b`) and implicit (castcontext `i`): (source,
/// target).
static BINARY_CASTS: std::sync::LazyLock<HashSet<(i64, i64)>> = std::sync::LazyLock::new(|| {
    let cols = catalog_schema::columns("pg_cast");
    let at = |n: &str| cols.iter().position(|(c, _)| c == n);
    let (Some(s), Some(t), Some(context), Some(method)) =
        (at("castsource"), at("casttarget"), at("castcontext"), at("castmethod"))
    else {
        return HashSet::new();
    };
    let int = |v: &Value| if let Value::Int(n) = v { *n } else { 0 };
    (catalog_schema::builtin_rows("pg_cast").iter())
        .filter(|r| r[context].to_text() == "i" && r[method].to_text() == "b")
        .map(|r| (int(&r[s]), int(&r[t])))
        .collect()
});

/// Postgres's built-in casts that are binary-coercible (castmethod `b`), in any context: (source, target).
static RELABEL_CASTS: std::sync::LazyLock<HashSet<(i64, i64)>> = std::sync::LazyLock::new(|| {
    let cols = catalog_schema::columns("pg_cast");
    let at = |n: &str| cols.iter().position(|(c, _)| c == n);
    let (Some(s), Some(t), Some(method)) = (at("castsource"), at("casttarget"), at("castmethod")) else {
        return HashSet::new();
    };
    let int = |v: &Value| if let Value::Int(n) = v { *n } else { 0 };
    (catalog_schema::builtin_rows("pg_cast").iter())
        .filter(|r| r[method].to_text() == "b")
        .map(|r| (int(&r[s]), int(&r[t])))
        .collect()
});

/// Whether a value of built-in type `source` becomes one of `target` by a cast that only relabels it (castmethod
/// `b`): coerce_type makes a RelabelType of it, not a function call.
pub(super) fn relabels(source: i64, target: i64) -> bool {
    RELABEL_CASTS.contains(&(source, target))
}

/// Whether `oid` is a built-in base type (typtype `b`) that isn't an array.
pub(super) fn builtin_scalar(oid: i64) -> bool {
    TYPE_SHAPES.get(&oid).is_some_and(|&(kind, elem, _)| kind == 'b' && elem.is_none())
}

/// The typtype of a type made with CREATE TYPE or CREATE DOMAIN, by its row: a domain's `d`, an enum's `e` (one with
/// no labels too), a composite type's `c` (one of no attributes too, whose row says `"empty": true`; a shell type's,
/// `CREATE TYPE name`, whose fields are none and whose row doesn't, `p`: see `sql::shell_row`), a range type's `r` and
/// a base type's `b`.
fn type_kind(j: &Json) -> char {
    match (j["composite"].as_array(), j["range"].is_object()) {
        _ if j.get("domain").is_some_and(Json::is_object) => 'd',
        _ if j.get("enum").is_some() => 'e',
        (Some(fields), _) if fields.is_empty() && j["empty"] != true => 'p',
        (Some(_), _) => 'c',
        (None, true) => 'r',
        (None, false) => 'b',
    }
}

/// Type `oid`'s typtype and, for a true array, its element type: a built-in type's, or one made here (an enum, a
/// domain, a composite, range or base type, a relation's row type, and their arrays); None for an oid no type has.
fn type_shape(cat: &Catalog, oid: i64) -> Option<(char, Option<i64>)> {
    if let Some(&(kind, elem, _)) = TYPE_SHAPES.get(&oid) {
        return Some((kind, elem));
    }
    // a range type made with CREATE TYPE ... AS RANGE, and its multirange, by the oids `range` gives them
    if let Ok(o) = u32::try_from(oid) {
        if super::range::kind(o).is_some() {
            return Some(('r', None));
        }
        if super::range::of_multi(o).is_some() {
            return Some(('m', None));
        }
    }
    // (the catalog keeps each type made with CREATE TYPE or CREATE DOMAIN with the enums)
    if let Some(k) = cat.enum_at(oid, false) {
        return Some((cat.type_kinds.get(k).copied().unwrap_or('b'), None));
    }
    if cat.enum_at(oid, true).is_some() || cat.row_type_of(oid, true).is_some() {
        return Some(('b', Some(oid - 1)));
    }
    cat.row_type_of(oid, false).map(|_| ('c', None))
}

/// getBaseType: the type a domain is over, through any domains it is over in turn; any other type is its own.
fn base_type(cat: &Catalog, mut oid: i64) -> i64 {
    for _ in 0..64 {
        let base = match TYPE_SHAPES.get(&oid) {
            Some(&(_, _, base)) => base,
            None => cat
                .enum_at(oid, false)
                .filter(|&k| cat.domains.get(k).is_some_and(Option::is_some))
                .and_then(|k| with_world_view(|v| super::domain::base_of(v, &cat.enums[k].0)).ok().flatten())
                .and_then(|text| reg_oid(cat, Reg::Type, &text).ok())
                .unwrap_or(0),
        };
        if base == 0 || base == oid {
            break;
        }
        oid = base;
    }
    oid
}

/// The regression tests' library's binary_coercible(source, target), Postgres's IsBinaryCoercible: whether a value of
/// type `source` is one of type `target` with no conversion. The same type is; any is one of `any`, `anyelement` and
/// `anycompatible`; a domain is its base type; the polymorphic types take what they say (an array, an enum, a range,
/// ...), and `record` a composite type; else only a cast WITHOUT FUNCTION that is implicit makes one.
fn binary_coercible(cat: &Catalog, source: i64, target: i64) -> bool {
    const ANY: i64 = 2276;
    const ANYELEMENT: i64 = 2283;
    const ANYCOMPATIBLE: i64 = 5077;
    const ANYARRAY: i64 = 2277;
    const ANYCOMPATIBLEARRAY: i64 = 5078;
    const ANYNONARRAY: i64 = 2776;
    const ANYCOMPATIBLENONARRAY: i64 = 5079;
    const ANYENUM: i64 = 3500;
    const ANYRANGE: i64 = 3831;
    const ANYCOMPATIBLERANGE: i64 = 5080;
    const ANYMULTIRANGE: i64 = 4537;
    const ANYCOMPATIBLEMULTIRANGE: i64 = 4538;
    const RECORD: i64 = 2249;
    const RECORDARRAY: i64 = 2287;
    if source == target || matches!(target, ANY | ANYELEMENT | ANYCOMPATIBLE) {
        return true;
    }
    let source = if source != 0 { base_type(cat, source) } else { source };
    if source == target {
        return true;
    }
    let (kind, elem) = type_shape(cat, source).unwrap_or(('b', None));
    let held = match target {
        ANYARRAY | ANYCOMPATIBLEARRAY => elem.is_some(),
        ANYNONARRAY | ANYCOMPATIBLENONARRAY => elem.is_none(),
        ANYENUM => kind == 'e',
        ANYRANGE | ANYCOMPATIBLERANGE => kind == 'r',
        ANYMULTIRANGE | ANYCOMPATIBLEMULTIRANGE => kind == 'm',
        RECORD => kind == 'c',
        // (an array of a domain over a composite type too: Postgres's ISCOMPLEX follows the domain)
        RECORDARRAY => elem.and_then(|e| type_shape(cat, base_type(cat, e))).is_some_and(|(k, _)| k == 'c'),
        _ => false,
    };
    held || BINARY_CASTS.contains(&(source, target))
}

/// The catalog's functions: regclass (a name or oid to the relation's name, or to its oid),
/// to_regclass, pg_get_constraintdef, pg_get_indexdef, pg_table_is_visible, current_schemas.
pub(super) fn call(f: CtxFn, a: &[Value]) -> Result<Value> {
    use Value::*;
    if f == CtxFn::CurrentSchemas {
        let path = path_schemas().unwrap_or_else(|| vec!["public".into()]);
        let mut shown: Vec<Value> = vec![];
        if a[0] == Bool(true) {
            shown.push(Text("pg_catalog".into()));
        }
        let v = WORLD.with_borrow(|w| w.as_ref().and_then(|(c, b, _)| c.view_tx(b).ok().map(|v| namespaces(&v))));
        let known = v.and_then(Result::ok).unwrap_or_else(|| vec!["public".into()]);
        shown.extend(path.into_iter().filter(|s| known.contains(s)).map(Text));
        return Ok(Text(array_text(shown.iter())));
    }
    if a[0] == Null {
        return Ok(Null);
    }
    // (format_type only reads type names: it answers outside a database too)
    let cat = match f {
        CtxFn::FormatType => catalog_now().unwrap_or_else(|_| Arc::new(Catalog::empty())),
        _ => catalog_now()?,
    };
    if let CtxFn::RegOid(kind) | CtxFn::RegName(kind) = f {
        return reg_cast(&cat, kind, f == CtxFn::RegOid(kind), &a[0]);
    }
    if let CtxFn::RegNames(kind) = f {
        return reg_array_names(&cat, kind, &a[0]);
    }
    // pg_get_object_address, pg_identify_object, pg_identify_object_as_address and pg_describe_object
    if let CtxFn::ObjAddr(which) = f {
        return super::objaddr::call(&cat, which, a);
    }
    // to_regclass and its kin: NULL where the name names nothing (or several), the cast's error for bad syntax
    if let CtxFn::ToReg(kind) = f {
        let given = match &a[0] {
            v @ (Int(_) | Reg(..)) => v.clone(),
            v => Text(v.to_text()),
        };
        return match reg_cast(&cat, kind, true, &given) {
            // (and, as Postgres 16's input functions report it softly, a name of the wrong syntax: `foo.bar` for a role)
            Err(e) if matches!(e.code, "42P01" | "42704" | "3F000" | "42883" | "42725" | "42P02" | "42602") => Ok(Null),
            r => r,
        };
    }
    if f == CtxFn::InputValid {
        return Ok(Bool(read_as(&cat, &a[0].to_text(), &a[1].to_text())?.is_ok()));
    }
    if f == CtxFn::BinaryCoercible {
        let oid = |v: &Value| match v {
            Reg(o, _) => Some(*o),
            v => match coerce(v.clone(), Type::BigInt) {
                Ok(Int(o)) => Some(o),
                _ => None,
            },
        };
        return Ok(match (oid(&a[0]), a.get(1).and_then(oid)) {
            (Some(source), Some(target)) => Bool(binary_coercible(&cat, source, target)),
            _ => Null,
        });
    }
    if f == CtxFn::FormatType {
        let Int(oid) = coerce(a[0].clone(), Type::BigInt)? else { return Ok(Null) };
        let typmod = match a.get(1).map(|m| coerce(m.clone(), Type::BigInt)).transpose()? {
            Some(Int(n)) => Some(n),
            _ => None,
        };
        // (a bpchar given no length is bpchar, not character, which is character(1): FORMAT_TYPE_TYPEMOD_GIVEN)
        return Ok(Text(match (oid, typmod) {
            (1042, Some(m)) if m < 0 => "bpchar".into(),
            (1014, Some(m)) if m < 0 => "bpchar[]".into(),
            _ => cat.format_type(oid, typmod.unwrap_or(-1)),
        }));
    }
    // (a relation named as text, as these functions read a regclass argument given as one)
    let oid = |v: &Value| -> Result<Option<i64>> {
        match v {
            Int(o) | Reg(o, _) => Ok(Some(*o)),
            v => {
                let t = v.to_text();
                match t.trim().parse::<i64>() {
                    Ok(o) => Ok(Some(o)),
                    Err(_) => cat.named(&t),
                }
            }
        }
    };
    Ok(match f {
        CtxFn::RegClassOid => Int(oid(&a[0])?.unwrap_or(0)),
        // (an unknown or missing object has no comment; so does one in a catalog it isn't in)
        CtxFn::Description => {
            let class = a.get(1).filter(|c| **c != Null).map(Value::to_text);
            let o = oid(&a[0]).ok().flatten();
            let text = match o.and_then(|o| cat.comment_on(o, class.as_deref(), 0)) {
                Some(t) => Some(t.to_string()),
                // (a built-in function's, operator's, access method's or type's is Postgres's own)
                None => o.and_then(|o| {
                    let wanted = class.as_deref().map(|c| c.trim_start_matches("pg_catalog.").to_lowercase());
                    [("pg_proc", 1255), ("pg_operator", 2617), ("pg_am", 2601), ("pg_type", 1247)]
                        .into_iter()
                        .filter(|(n, _)| wanted.as_deref().is_none_or(|w| w == *n))
                        .find_map(|(_, c)| catalog_schema::builtin_description(o, c))
                        .map(str::to_string)
                }),
            };
            text.map_or(Null, Text)
        }
        CtxFn::ColDescription => {
            let Some(Int(n)) = a.get(1).map(|n| coerce(n.clone(), Type::BigInt)).transpose()? else { return Ok(Null) };
            let found = oid(&a[0])
                .ok()
                .flatten()
                .and_then(|o| if n >= 0 { cat.comment_on(o, Some("pg_class"), n) } else { None });
            found.map_or(Null, |t| Text(t.into()))
        }
        CtxFn::ConstraintDef => {
            let o = oid(&a[0])?;
            match o.and_then(|o| cat.find(o)) {
                Some(Obj::Con(c)) if a.get(1) == Some(&Bool(true)) => Text(c.pretty.clone()),
                Some(Obj::Con(c)) => Text(c.def.clone()),
                // a domain's CHECK
                _ => cat
                    .domain_checks()
                    .into_iter()
                    .find(|(d, ..)| Some(*d) == o)
                    .map_or(Null, |(.., text)| Text(format!("CHECK ({})", domain_check_cond(text)))),
            }
        }
        CtxFn::IndexDef => {
            let col = match a.get(1) {
                Some(Int(n)) => *n,
                _ => 0,
            };
            match oid(&a[0])?.and_then(|o| cat.find(o)) {
                Some(Obj::Index(r, x)) => Text(cat.index_def(r, x, col, a.get(2) == Some(&Bool(true)))),
                _ => Null,
            }
        }
        // what an index access method, an index or an index's column can do (see `indexam::property`): NULL for
        // what isn't one, or a property no method has
        CtxFn::IndexProperty(which) => {
            let Some(prop) = a.last().filter(|p| **p != Null).map(Value::to_text) else { return Ok(Null) };
            let col = match which {
                2 => match coerce(a[1].clone(), Type::BigInt)? {
                    Int(n) if n > 0 => n,
                    _ => return Ok(Null),
                },
                _ => 0,
            };
            let found = match (which, oid(&a[0])?) {
                (0, Some(o)) => AMS
                    .iter()
                    .find(|(x, ..)| *x == o)
                    .map(|(_, n, ..)| *n)
                    .or_else(|| cat.ams.iter().find(|a| a.oid == o && a.kind == 'i').map(|a| a.builtin()))
                    .and_then(super::indexam::method)
                    .map(|am| (am, None)),
                (_, Some(o)) => match cat.find(o) {
                    Some(Obj::Index(_, x)) => super::indexam::method(x.method).map(|am| (am, Some(x))),
                    _ => None,
                },
                _ => None,
            };
            let Some((am, x)) = found else { return Ok(Null) };
            let facts = x.map(|x| super::indexam::Facts {
                natts: x.cols.len(),
                nkey: x.nkey,
                opts: &x.opts,
                classes: &x.class_names,
            });
            super::indexam::property(am, facts.as_ref(), col, &prop).map_or(Null, Bool)
        }
        CtxFn::PartKeyDef => match oid(&a[0])?.and_then(|o| cat.find(o)) {
            Some(Obj::Rel(i)) => cat.rels[i].hier.part.as_ref().map_or(Null, |p| Text(p.def.clone())),
            _ => Null,
        },
        CtxFn::PartConstraintDef => match oid(&a[0])?.and_then(|o| cat.find(o)) {
            Some(Obj::Rel(i)) => match &cat.rels[i].hier.of {
                Some((_, _, text)) => {
                    // a hash partition's constraint names its parent by oid
                    let mut text = text.clone();
                    while let Some(at) = text.find('@') {
                        let Some(len) = text[at + 1..].find('@') else { break };
                        let name = text[at + 1..at + 1 + len].to_string();
                        let o = cat.oid_of(&name).unwrap_or(0);
                        text.replace_range(at..at + len + 2, &o.to_string());
                    }
                    Text(text)
                }
                None => Null,
            },
            _ => Null,
        },
        CtxFn::PartRoot => match oid(&a[0])?.map(|o| cat.find(o)) {
            None if !matches!(a[0], Value::Null) => {
                return err(NO_TABLE, format!("relation \"{}\" does not exist", a[0].to_text()));
            }
            Some(Some(Obj::Rel(i))) => {
                // the table with no partition parent, up the chain (none: not a partition or a partitioned table)
                let part = |i: usize| cat.rels[i].hier.part.is_some() || cat.rels[i].hier.of.is_some();
                let up = |i: usize| {
                    let (p, _, _) = cat.rels[i].hier.of.as_ref()?;
                    cat.rels.iter().position(|r| r.name == *p)
                };
                if !part(i) {
                    Null
                } else {
                    let mut cur = i;
                    while let Some(p) = up(cur) {
                        cur = p;
                    }
                    Reg(cat.rel_oid(cur), super::Reg::Class)
                }
            }
            // an index's: the top of its partitioned index's tree
            Some(Some(Obj::Index(r, x))) => cat
                .idx
                .iter()
                .position(|(q, y)| *q == r && y.name == x.name)
                .and_then(|k| idx_root(&cat, k))
                .map_or(Null, |k| Reg(cat.idx_oid(k), super::Reg::Class)),
            _ => Null,
        },
        // a relation's pg_class.relfilenode, null for one with no storage of its own (a view, a partitioned table or
        // index, a foreign table). The argument is a regclass: a name of none is its error, an oid of none null.
        CtxFn::RelationFilenode => {
            let o = match &a[0] {
                Int(n) => int_oid(*n)?,
                Reg(o, _) => *o,
                v => reg_oid(&cat, super::Reg::Class, &v.to_text())?,
            };
            match cat.find(o) {
                Some(Obj::Rel(i)) => match cat.rels[i].kind {
                    'v' | 'p' | 'f' => Null,
                    _ => Int(super::tablespace::filenode(cat.rel_oid(i), cat.rels[i].hier.filenode)),
                },
                Some(Obj::Index(r, x)) if cat.rels[r].kind != 'p' => {
                    let k = cat.idx.iter().position(|(q, y)| *q == r && y.name == x.name);
                    k.map_or(Null, |k| Int(super::tablespace::filenode(cat.idx_oid(k), x.filenode)))
                }
                // (a TOAST table, a catalog table or its key's index: its oid, see `kept_nowhere`)
                _ if super::catalog_objects::kept_nowhere(&cat, o) => Int(o),
                _ => Null,
            }
        }
        // and the relation of a filenode in a tablespace (0 the database's default), as Postgres's
        // RelidByRelfilenumber: none for a temporary one, which isn't mapped. Both are oids: text read as oidin reads
        // it (`0x67F`, `-1` wrapped around; not as a relation's name, as a regclass is), an integer as its cast to an
        // oid takes it (22003 past 32 bits), and the function is strict: a NULL tablespace is none, not the default.
        CtxFn::FilenodeRelation => {
            let as_oid = |v: &Value| match v {
                Null => Ok(None),
                Int(n) => int_oid(*n).map(Some),
                Reg(o, _) => Ok(Some(*o)),
                v => super::oid_in(&v.to_text()).map(Some),
            };
            let (Some(space), Some(node)) = (as_oid(&a[0])?, a.get(1).map(as_oid).transpose()?.flatten()) else {
                return Ok(Null);
            };
            let space = if space == super::tablespace::DEFAULT { 0 } else { space };
            let temp = |name: &str| schema_part(name) == TEMP_SCHEMA;
            let rel = cat.rels.iter().enumerate().find(|(i, r)| {
                !matches!(r.kind, 'v' | 'p' | 'f')
                    && !temp(&r.name)
                    && r.hier.tablespace == space
                    && super::tablespace::filenode(cat.rel_oid(*i), r.hier.filenode) == node
            });
            if let Some((i, _)) = rel {
                return Ok(Reg(cat.rel_oid(i), super::Reg::Class));
            }
            let index = cat.idx.iter().enumerate().find(|(k, (r, x))| {
                cat.rels[*r].kind != 'p'
                    && !temp(&cat.rels[*r].name)
                    && x.tablespace == space
                    && super::tablespace::filenode(cat.idx_oid(*k), x.filenode) == node
            });
            match index {
                Some((k, _)) => Reg(cat.idx_oid(k), super::Reg::Class),
                None if space == 0 && super::catalog_objects::kept_nowhere(&cat, node) => Reg(node, super::Reg::Class),
                None => Null,
            }
        }
        // (a built-in function's from its line in builtin_procs.tsv)
        CtxFn::FunctionArgs(identity) => {
            let o = oid(&a[0])?;
            match (o.and_then(|o| catalog_schema::builtin_arguments(o, identity)), o.and_then(|o| cat.find(o))) {
                (Some(args), _) => Text(args),
                (None, Some(Obj::Func(k))) => Text(cat.function_arguments(&cat.funcs[k], identity)),
                _ => Null,
            }
        }
        CtxFn::FunctionResult => {
            let o = oid(&a[0])?;
            match (o.and_then(catalog_schema::builtin_result), o.and_then(|o| cat.find(o))) {
                (Some(r), _) => Text(r),
                (None, Some(Obj::Func(k))) => cat.function_result(&cat.funcs[k]).map_or(Null, Text),
                _ => Null,
            }
        }
        CtxFn::FunctionDef => {
            let o = oid(&a[0])?;
            match (o.and_then(catalog_schema::builtin_def), o.and_then(|o| cat.find(o))) {
                (Some(def), _) => Text(def?),
                (None, Some(Obj::Func(k))) => Text(cat.function_def(&cat.funcs[k])),
                _ => Null,
            }
        }
        // as CREATE TRIGGER wrote it, its WHEN written back as Postgres writes it; with `pretty` false, the table is
        // schema-qualified and the WHEN fully parenthesized, as Postgres's are
        CtxFn::TriggerDef => match oid(&a[0])?.and_then(|o| cat.find(o)) {
            Some(Obj::Trigger(r, k)) => {
                let pretty = a.get(1) == Some(&Bool(true));
                let rel = &cat.rels[r];
                let def = match pretty {
                    true => &rel.triggers[k].1,
                    false => rel.trigger_plain.get(k).unwrap_or(&rel.triggers[k].1),
                };
                let name = &rel.name;
                let (bare, whole) =
                    (ident_rel(name), format!("{}.{}", ident(schema_part(name)), ident(rel_part(name))));
                Text(if pretty {
                    def.clone()
                } else {
                    def.replacen(&format!(" ON {bare} "), &format!(" ON {whole} "), 1)
                })
            }
            _ => Null,
        },
        // a view's query as Postgres writes the parsed query back (see `viewdef`): pg_get_viewdef(v), (v, pretty) and
        // (v, wrap column), which is pretty
        CtxFn::ViewDef => match oid(&a[0])?.and_then(|o| cat.find(o)) {
            Some(Obj::Rel(i)) if matches!(cat.rels[i].kind, 'v' | 'm') => {
                let (pretty, wrap) = match a.get(1) {
                    Some(Bool(p)) => (*p, 0),
                    Some(Int(w)) => (true, i32::try_from(*w).unwrap_or(i32::MAX)),
                    _ => (false, 0),
                };
                super::viewdef::shown(&cat.rels, i, pretty, wrap).map_or(Null, Text)
            }
            _ => Null,
        },
        // the events a relation takes (INSERT 8, UPDATE 4, DELETE 16; see `updatable_events`): 0 for one that isn't
        // there, as Postgres's
        CtxFn::RelationIsUpdatable => match oid(&a[0])?.and_then(|o| cat.find(o)) {
            Some(Obj::Rel(i)) => {
                let triggers = a.get(1) == Some(&Bool(true));
                Int(i64::from(super::updatable_events(&cat, &cat.rels[i].name, triggers, None, 0)))
            }
            _ => Int(0),
        },
        // whether a column takes UPDATE and DELETE (a system column, attnum 0 or less, never does)
        CtxFn::ColumnIsUpdatable => {
            let attnum = match coerce(a[1].clone(), Type::BigInt)? {
                Int(n) => n,
                _ => 0,
            };
            match oid(&a[0])?.and_then(|o| cat.find(o)) {
                Some(Obj::Rel(i)) if attnum > 0 => {
                    let triggers = a.get(2) == Some(&Bool(true));
                    let col = [(attnum - 1) as usize];
                    let events = super::updatable_events(&cat, &cat.rels[i].name, triggers, Some(&col), 0);
                    Bool(events & 20 == 20)
                }
                _ => Bool(false),
            }
        }
        CtxFn::SqlBody => Null,
        // a statistics object as CREATE STATISTICS would make it again, or its columns and expressions (see
        // `statistics`)
        CtxFn::StatsDef(columns) => match oid(&a[0])? {
            Some(o) => super::statistics::def(&cat, o, columns).map_or(Null, Text),
            None => Null,
        },
        CtxFn::StatsVisible => match oid(&a[0])? {
            Some(o) => super::statistics::visible(&cat, o).map_or(Null, Bool),
            None => Null,
        },
        CtxFn::CollationVersion => match oid(&a[0])? {
            Some(o) if collation::builtins().any(|b| b.0 == o && b.3.is_some()) => Text(collation::VERSION.into()),
            Some(o) => match cat.coll_at(o) {
                Some(k) if cat.colls[k].1["provider"] == "icu" => {
                    Text(cat.colls[k].1["version"].as_str().unwrap_or(collation::VERSION).into())
                }
                _ => Null,
            },
            None => Null,
        },
        // pg_function_is_visible: along the search_path (pg_catalog first unless it names it later), the first
        // function of its name and argument types is this one
        _ if oid(&a[0])?.is_some_and(|o| function_visible(&cat, o).is_some()) => {
            Bool(oid(&a[0])?.and_then(|o| function_visible(&cat, o)) == Some(true))
        }
        // TableIsVisible: along the search_path, the first relation of its name is this one
        _ => match oid(&a[0])?.and_then(|o| cat.find(o)) {
            // (the session's temporary tables are first along it, as pg_temp is)
            Some(Obj::Rel(i)) if schema_part(&cat.rels[i].name) == TEMP_SCHEMA => Bool(true),
            Some(Obj::Index(r, _)) if schema_part(&cat.rels[r].name) == TEMP_SCHEMA => Bool(true),
            // (and hide the world's relations of their names)
            Some(Obj::Rel(i)) if has_temp(rel_part(&cat.rels[i].name)) => Bool(false),
            Some(Obj::Rel(i)) => {
                let name = &cat.rels[i].name;
                Bool(match on_path(rel_part(name), Kind::Rel) {
                    Some(found) => found == *name,
                    // (no relation of its name along the search_path: one of the catalog's, which is always on it,
                    // is still found)
                    None => catalog_table(name) || schema_part(name) == "pg_catalog",
                })
            }
            Some(Obj::Index(r, _)) => {
                let s = schema_part(&cat.rels[r].name);
                Bool(path_schemas().map_or(s == "public", |p| p.iter().any(|x| x == s)))
            }
            _ => Bool(true),
        },
    })
}

/// The name format_type gives the built-in type whose `typname` is `typname` (`int4` is `integer`), if Chronos knows it.
pub(super) fn builtin_type_shown(typname: &str) -> Option<&'static str> {
    PG_TYPES.iter().find(|(_, _, names)| names.contains(&typname)).map(|t| t.1)
}

/// Whether `typname` names one of Postgres's built-in types (as `typname` or another name it reads).
pub(super) fn is_builtin_type_name(typname: &str) -> bool {
    PG_TYPES.iter().any(|(_, _, names)| names.contains(&typname))
}

/// Postgres's types by name and oid: (oid, the name `regtype` shows, the names it reads).
const PG_TYPES: &[(i64, &str, &[&str])] = &[
    (2249, "record", &["record"]),
    (2278, "void", &["void"]),
    (705, "unknown", &["unknown"]),
    (2279, "trigger", &["trigger"]),
    (16, "boolean", &["bool", "boolean"]),
    (17, "bytea", &["bytea"]),
    (18, "\"char\"", &["\"char\""]),
    (19, "name", &["name"]),
    (20, "bigint", &["int8", "bigint"]),
    (21, "smallint", &["int2", "smallint"]),
    (23, "integer", &["int4", "integer", "int"]),
    (25, "text", &["text"]),
    (26, "oid", &["oid"]),
    (22, "int2vector", &["int2vector"]),
    (30, "oidvector", &["oidvector"]),
    (71, "pg_type", &["pg_type"]),
    (75, "pg_attribute", &["pg_attribute"]),
    (81, "pg_proc", &["pg_proc"]),
    (83, "pg_class", &["pg_class"]),
    (27, "tid", &["tid"]),
    (28, "xid", &["xid"]),
    (29, "cid", &["cid"]),
    (114, "json", &["json"]),
    (650, "cidr", &["cidr"]),
    (700, "real", &["float4", "real"]),
    (701, "double precision", &["float8", "double precision", "float"]),
    (774, "macaddr8", &["macaddr8"]),
    (829, "macaddr", &["macaddr"]),
    (869, "inet", &["inet"]),
    (1042, "character", &["bpchar", "character", "char"]),
    (1043, "character varying", &["varchar", "character varying"]),
    (1082, "date", &["date"]),
    (1083, "time without time zone", &["time", "time without time zone"]),
    (1114, "timestamp without time zone", &["timestamp", "timestamp without time zone"]),
    (1184, "timestamp with time zone", &["timestamptz", "timestamp with time zone"]),
    (1186, "interval", &["interval"]),
    (1700, "numeric", &["numeric", "decimal"]),
    (24, "regproc", &["regproc"]),
    (2202, "regprocedure", &["regprocedure"]),
    (2203, "regoper", &["regoper"]),
    (2204, "regoperator", &["regoperator"]),
    (2205, "regclass", &["regclass"]),
    (2206, "regtype", &["regtype"]),
    (3734, "regconfig", &["regconfig"]),
    (3769, "regdictionary", &["regdictionary"]),
    (4089, "regnamespace", &["regnamespace"]),
    (4096, "regrole", &["regrole"]),
    (4191, "regcollation", &["regcollation"]),
    (2950, "uuid", &["uuid"]),
    // (the citext extension's type, which Postgres numbers as it is made: text in citext's collation here)
    (super::collation::CITEXT_OID, "citext", &["citext"]),
    // (the hstore and ltree extensions' types, likewise)
    (super::hstore::OID as i64, "hstore", &["hstore"]),
    (super::ltree::OID as i64, "ltree", &["ltree"]),
    (super::ltree::LQUERY_OID as i64, "lquery", &["lquery"]),
    (3614, "tsvector", &["tsvector"]),
    (3615, "tsquery", &["tsquery"]),
    (3802, "jsonb", &["jsonb"]),
    (790, "money", &["money"]),
    (1560, "bit", &["bit"]),
    (1562, "bit varying", &["varbit", "bit varying"]),
    (1266, "time with time zone", &["timetz", "time with time zone"]),
    (5069, "xid8", &["xid8"]),
    (5038, "pg_snapshot", &["pg_snapshot"]),
    (2970, "txid_snapshot", &["txid_snapshot"]),
    (600, "point", &["point"]),
    (601, "lseg", &["lseg"]),
    (602, "path", &["path"]),
    (603, "box", &["box"]),
    (604, "polygon", &["polygon"]),
    (628, "line", &["line"]),
    (718, "circle", &["circle"]),
    (3220, "pg_lsn", &["pg_lsn"]),
    (142, "xml", &["xml"]),
    (4072, "jsonpath", &["jsonpath"]),
    (1790, "refcursor", &["refcursor"]),
    (1033, "aclitem", &["aclitem"]),
    (194, "pg_node_tree", &["pg_node_tree"]),
    (3361, "pg_ndistinct", &["pg_ndistinct"]),
    (3402, "pg_dependencies", &["pg_dependencies"]),
    (5017, "pg_mcv_list", &["pg_mcv_list"]),
    (32, "pg_ddl_command", &["pg_ddl_command"]),
    (3642, "gtsvector", &["gtsvector"]),
    (4600, "pg_brin_bloom_summary", &["pg_brin_bloom_summary"]),
    (4601, "pg_brin_minmax_multi_summary", &["pg_brin_minmax_multi_summary"]),
    // the pseudo-types, which only a function's arguments and result are of
    (2275, "cstring", &["cstring"]),
    (2276, "\"any\"", &["\"any\"", "any"]),
    (2277, "anyarray", &["anyarray"]),
    (2283, "anyelement", &["anyelement"]),
    (2776, "anynonarray", &["anynonarray"]),
    (3500, "anyenum", &["anyenum"]),
    (3831, "anyrange", &["anyrange"]),
    (4537, "anymultirange", &["anymultirange"]),
    (5077, "anycompatible", &["anycompatible"]),
    (5078, "anycompatiblearray", &["anycompatiblearray"]),
    (5079, "anycompatiblenonarray", &["anycompatiblenonarray"]),
    (5080, "anycompatiblerange", &["anycompatiblerange"]),
    (4538, "anycompatiblemultirange", &["anycompatiblemultirange"]),
    (2281, "internal", &["internal"]),
    (2280, "language_handler", &["language_handler"]),
    (3115, "fdw_handler", &["fdw_handler"]),
    (325, "index_am_handler", &["index_am_handler"]),
    (3310, "tsm_handler", &["tsm_handler"]),
    (269, "table_am_handler", &["table_am_handler"]),
    (3838, "event_trigger", &["event_trigger"]),
];

/// Postgres 16's built-in types' array types (pg_type's oid and typarray, of each built-in base, pseudo, range and
/// multirange type that has one): what `elem[]` names as a regtype.
const PG_ARRAYS: &[(i64, i64)] = &[
    (16, 1000),
    (17, 1001),
    (18, 1002),
    (19, 1003),
    (20, 1016),
    (21, 1005),
    (22, 1006),
    (23, 1007),
    (24, 1008),
    (25, 1009),
    (26, 1028),
    (27, 1010),
    (28, 1011),
    (29, 1012),
    (30, 1013),
    (114, 199),
    (142, 143),
    (600, 1017),
    (601, 1018),
    (602, 1019),
    (603, 1020),
    (604, 1027),
    (628, 629),
    (650, 651),
    (700, 1021),
    (701, 1022),
    (718, 719),
    (774, 775),
    (790, 791),
    (829, 1040),
    (869, 1041),
    (1033, 1034),
    (1042, 1014),
    (1043, 1015),
    (1082, 1182),
    (1083, 1183),
    (1114, 1115),
    (1184, 1185),
    (1186, 1187),
    (1266, 1270),
    (1560, 1561),
    (1562, 1563),
    (1700, 1231),
    (1790, 2201),
    (2202, 2207),
    (2203, 2208),
    (2204, 2209),
    (2205, 2210),
    (2206, 2211),
    (2249, 2287),
    (2275, 1263),
    (2950, 2951),
    (2970, 2949),
    (3220, 3221),
    (3614, 3643),
    (3615, 3645),
    (3642, 3644),
    (3734, 3735),
    (3769, 3770),
    (3802, 3807),
    (3904, 3905),
    (3906, 3907),
    (3908, 3909),
    (3910, 3911),
    (3912, 3913),
    (3926, 3927),
    (4072, 4073),
    (4089, 4090),
    (4096, 4097),
    (4191, 4192),
    (4451, 6150),
    (4532, 6151),
    (4533, 6152),
    (4534, 6153),
    (4535, 6155),
    (4536, 6157),
    (5038, 5039),
    (5069, 271),
];

/// Postgres 16's text search configurations (`pg_ts_config`), all in pg_catalog: what regconfig reads and shows.
pub(super) const TS_CONFIGS: [(i64, &str); 29] = [
    (3748, "simple"),
    (13627, "arabic"),
    (13629, "armenian"),
    (13631, "basque"),
    (13633, "catalan"),
    (13635, "danish"),
    (13637, "dutch"),
    (13639, "english"),
    (13641, "finnish"),
    (13643, "french"),
    (13645, "german"),
    (13647, "greek"),
    (13649, "hindi"),
    (13651, "hungarian"),
    (13653, "indonesian"),
    (13655, "irish"),
    (13657, "italian"),
    (13659, "lithuanian"),
    (13661, "nepali"),
    (13663, "norwegian"),
    (13665, "portuguese"),
    (13667, "romanian"),
    (13669, "russian"),
    (13671, "serbian"),
    (13673, "spanish"),
    (13675, "swedish"),
    (13677, "tamil"),
    (13679, "turkish"),
    (13681, "yiddish"),
];

/// Postgres 16's text search dictionaries (`pg_ts_dict`), all in pg_catalog: what regdictionary reads and shows.
pub(super) const TS_DICTS: [(i64, &str); 29] = [
    (3765, "simple"),
    (13626, "arabic_stem"),
    (13628, "armenian_stem"),
    (13630, "basque_stem"),
    (13632, "catalan_stem"),
    (13634, "danish_stem"),
    (13636, "dutch_stem"),
    (13638, "english_stem"),
    (13640, "finnish_stem"),
    (13642, "french_stem"),
    (13644, "german_stem"),
    (13646, "greek_stem"),
    (13648, "hindi_stem"),
    (13650, "hungarian_stem"),
    (13652, "indonesian_stem"),
    (13654, "irish_stem"),
    (13656, "italian_stem"),
    (13658, "lithuanian_stem"),
    (13660, "nepali_stem"),
    (13662, "norwegian_stem"),
    (13664, "portuguese_stem"),
    (13666, "romanian_stem"),
    (13668, "russian_stem"),
    (13670, "serbian_stem"),
    (13672, "spanish_stem"),
    (13674, "swedish_stem"),
    (13676, "tamil_stem"),
    (13678, "turkish_stem"),
    (13680, "yiddish_stem"),
];

/// A built-in operator (see `builtin_opers.tsv`): oid, name, left and right operand types' oids (0: none), and those
/// types' names as format_type writes them (`NONE`).
pub(super) type Oper = (i64, &'static str, i64, i64, &'static str, &'static str);

/// Postgres 16's built-in operators, from `builtin_opers.tsv`: what regoper and regoperator read and show.
pub(super) fn opers() -> &'static [Oper] {
    static OPERS: LazyLock<Vec<Oper>> = LazyLock::new(|| {
        let lines = include_str!("builtin_opers.tsv").lines().filter(|l| !l.starts_with('#') && !l.is_empty());
        lines
            .filter_map(|l| {
                let f: Vec<&'static str> = l.split('\t').collect();
                Some((
                    f.first()?.parse().ok()?,
                    *f.get(1)?,
                    f.get(2)?.parse().ok()?,
                    f.get(3)?.parse().ok()?,
                    f[4],
                    f[5],
                ))
            })
            .collect()
    });
    &OPERS
}

/// The function a regproc or regprocedure string names (`now`, `pg_catalog.int4in`, `"left"`, `int4in(cstring)`
/// for a regprocedure, an oid as text, `-` for none): its oid. A regproc's name must be one function's (42725
/// when several have it, as `sum` has); a regprocedure's types pick one.
pub(super) fn proc_oid(cat: &Catalog, kind: Reg, text: &str) -> Result<i64> {
    match procs_named(cat, kind, text)?[..] {
        [] => err("42883", format!("function \"{text}\" does not exist")),
        [o] => Ok(o),
        _ if kind == Reg::Proc => err("42725", format!("more than one function named \"{text}\"")),
        [o, ..] => Ok(o),
    }
}

/// A string read as a reg value where no cast names it (a value written to a column of one, a parameter): its
/// object's oid, as `reg_oid` finds it in the running statement's world (or among the built-in objects, outside one).
pub(super) fn reg_value(kind: Reg, text: &str) -> Result<i64> {
    if let Some(o) = oid_digits(text) {
        return o;
    }
    let cat = match HOME.with_borrow(Clone::clone) {
        // the session's world, from its temporary tables' database (which has no types of its own)
        Some((core, world, _)) => in_world(&core, &world, catalog_now),
        None => catalog_now(),
    };
    reg_oid(&cat.unwrap_or_else(|_| Arc::new(Catalog::empty())), kind, text)
}

/// A reg value's text: its object's name as `reg_shown` writes it, in the running statement's world (among the
/// built-in objects, outside one).
pub(super) fn reg_name(oid: i64, kind: Reg) -> String {
    let cat = catalog_now().unwrap_or_else(|_| Arc::new(Catalog::empty()));
    reg_shown(&cat, kind, oid)
}

/// The relation a name or an oid (`v`) names, as a regclass shows it; None where none has it (what `has_*_privilege`
/// and the size functions ask, where `to_regclass` of an oid gives the oid whether a relation has it or not).
pub(super) fn rel_shown(v: &Value) -> Result<Option<String>> {
    let cat = catalog_now()?;
    let oid = match v {
        Value::Null => return Ok(None),
        Value::Int(o) | Value::Reg(o, _) => Some(*o),
        v => match v.to_text().trim().parse::<i64>() {
            Ok(o) => Some(o),
            Err(_) => cat.named(&v.to_text())?,
        },
    };
    Ok(oid.and_then(|o| cat.shown(o)))
}

/// pg_typeof's value for type `t`: a regtype, its oid (an enum's in the running statement's world).
pub(super) fn regtype_of(t: Type) -> Value {
    let cat = catalog_now().unwrap_or_else(|_| Arc::new(Catalog::empty()));
    Value::Reg(cat.regtype_oid(t), Reg::Type)
}

/// The functions a regproc or regprocedure string names (see `proc_oid`): their oids.
fn procs_named(cat: &Catalog, kind: Reg, text: &str) -> Result<Vec<i64>> {
    if let Some(o) = oid_digits(text) {
        return Ok(vec![o?]);
    }
    if text == "-" {
        return Ok(vec![0]);
    }
    let text = text.trim();
    // the first parenthesis outside a quoted name (`"f("()`)
    let mut quoted = false;
    let open = text.char_indices().find(|&(_, c)| {
        quoted ^= c == '"';
        c == '(' && !quoted
    });
    let (name, args) = match (kind, open.map(|(i, _)| (&text[..i], &text[i + 1..]))) {
        (Reg::Procedure, Some((n, a))) => match a.trim_end().strip_suffix(')') {
            Some(a) => (n, Some(a)),
            None => return err("22P02", "expected a right parenthesis"),
        },
        (Reg::Procedure, None) => return err("22P02", "expected a left parenthesis"),
        _ => (text, None),
    };
    let parts = name_parts(name);
    let (schema, bare) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return err(UNSUPPORTED, format!("cross-database references are not implemented: {text}")),
    };
    // each argument type as regprocedure writes it, `int` as `integer` (a type this can't read, as given)
    let wanted = args.map(|a| {
        let types: Vec<String> = type_list(a)
            .iter()
            .map(|t| match type_named(cat, t.trim()) {
                Ok(o) => cat.format_type(o, -1),
                _ => t.trim().to_lowercase(),
            })
            .filter(|t| !t.is_empty())
            .collect();
        format!("({})", types.join(","))
    });
    let found = candidates(cat, schema, bare).into_iter();
    Ok(found.filter(|(_, a)| wanted.as_ref().is_none_or(|w| w == a)).map(|(o, _)| o).collect())
}

/// A list of type names (`integer, numeric(10,2)`), split at its commas outside parentheses and quotes.
fn type_list(a: &str) -> Vec<String> {
    let mut types = vec![String::new()];
    let (mut depth, mut quoted) = (0, false);
    for c in a.chars() {
        quoted ^= c == '"';
        match c {
            ',' if depth == 0 && !quoted => types.push(String::new()),
            c => {
                if !quoted {
                    depth += i32::from(c == '(') - i32::from(c == ')');
                }
                types.last_mut().unwrap().push(c);
            }
        }
    }
    types
}

/// `text` read as an oid when it is all digits, as regprocin reads one: by oidin's rules (strtoul with base 0, so a
/// leading 0 is octal), 22P02 for a digit octal lacks and 22003 past 32 bits. None when it is not all digits (`-1`,
/// ` 42`), which is a name.
fn oid_digits(text: &str) -> Option<Result<i64>> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (digits, radix) = match text.strip_prefix('0') {
        Some(rest) if !rest.is_empty() => (rest, 8),
        _ => (text, 10),
    };
    if radix == 8 && digits.bytes().any(|b| b > b'7') {
        return Some(err(BAD_INPUT, format!("invalid input syntax for type oid: \"{text}\"")));
    }
    Some(match u64::from_str_radix(digits, radix).ok().filter(|o| *o <= u64::from(u32::MAX)) {
        Some(o) => Ok(o as i64),
        None => err(RANGE, format!("value \"{text}\" is out of range for type oid")),
    })
}

/// An integer as the oid it casts to: a negative int4 wraps around to the oid of its bits, as Postgres's int4-to-oid
/// cast does, and anything else outside 32 bits is 22003.
pub(super) fn int_oid(n: i64) -> Result<i64> {
    match n {
        0..=0xFFFF_FFFF => Ok(n),
        -0x8000_0000..0 => Ok(n + (1 << 32)),
        _ => err(RANGE, "OID out of range"),
    }
}

/// The functions a plain name `bare` finds in `schema`, or along the search_path (pg_catalog first unless it names
/// it later): each one's oid and argument types, `(integer,text)`. One with the argument types of a function in a
/// schema ahead of it is left out, as Postgres hides it.
fn candidates(cat: &Catalog, schema: Option<&str>, bare: &str) -> Vec<(i64, String)> {
    let schemas = schema.map_or_else(func_path, |s| vec![s.to_string()]);
    let mut found: Vec<(i64, String)> = vec![];
    for s in &schemas {
        let here: Vec<(i64, String)> = if s == "pg_catalog" {
            let builtin = catalog_schema::builtin_procs(bare).into_iter();
            builtin.filter_map(|o| Some((o, catalog_schema::builtin_proc(o)?.1.to_string()))).collect()
        } else {
            let stored = qualify(s, bare);
            let funcs = cat.funcs.iter().enumerate().filter(|(_, f)| f.name == stored);
            funcs.map(|(k, f)| (cat.func_oid(k), format!("({})", arg_types(cat, f)))).collect()
        };
        for (o, args) in here {
            if !found.iter().any(|(_, a)| *a == args) {
                found.push((o, args));
            }
        }
    }
    found
}

/// Whether the function with this oid (a built-in one, or one made here) is visible: the search_path finds it by its
/// name and argument types, a function of a schema before it not having them. None if no function has the oid.
fn function_visible(cat: &Catalog, oid: i64) -> Option<bool> {
    let bare = match (catalog_schema::builtin_proc(oid), cat.find(oid)) {
        (Some((name, _)), _) => name.trim_matches('"').to_string(),
        (None, Some(Obj::Func(k))) => rel_part(&cat.funcs[k].name).to_string(),
        _ => return None,
    };
    Some(candidates(cat, None, &bare).iter().any(|(o, _)| *o == oid))
}

/// A function's argument types as regprocedure writes them, `integer,text`.
fn arg_types(cat: &Catalog, f: &functions::Function) -> String {
    f.args.iter().map(|(_, t)| cat.format_type(cat.type_oid(*t), -1)).collect::<Vec<_>>().join(",")
}

/// How a regproc (the name) or a regprocedure (with its argument types) shows the function with this oid: bare
/// when the search_path finds it so (a regproc's name finding it alone), else with its schema; `-` for 0, and the
/// number itself for an oid no function has.
pub(super) fn proc_shown(cat: &Catalog, kind: Reg, oid: i64) -> String {
    let (schema, bare, name, args) = match (catalog_schema::builtin_proc(oid), cat.find(oid)) {
        (Some((name, args)), _) => {
            ("pg_catalog", name.trim_matches('"').to_string(), name.to_string(), args.to_string())
        }
        (None, Some(Obj::Func(k))) => {
            let f = &cat.funcs[k];
            let bare = rel_part(&f.name).to_string();
            (schema_part(&f.name), bare.clone(), ident(&bare), format!("({})", arg_types(cat, f)))
        }
        _ if oid == 0 => return "-".into(),
        _ => return oid.to_string(),
    };
    let found = candidates(cat, None, &bare);
    let visible = if kind == Reg::Procedure {
        found.iter().find(|(_, a)| *a == args).is_some_and(|(o, _)| *o == oid)
    } else {
        found.len() == 1 && found[0].0 == oid
    };
    let shown = if kind == Reg::Procedure { format!("{name}{args}") } else { name };
    if visible { shown } else { format!("{}.{shown}", ident(schema)) }
}

/// Results' reg values (and arrays of them), the oids they are while a statement runs, as the names a client reads,
/// in the columns `shown` picks.
pub(super) fn show_regs(cols: &[(String, Type)], rows: &mut [Vec<Value>], shown: &dyn Fn(usize) -> bool) {
    let at: Vec<(usize, Reg, bool)> = (cols.iter().enumerate())
        .filter_map(|(i, (_, t))| match (t.reg(), t.reg_array()) {
            (Some(r), _) => Some((i, r, false)),
            (_, Some(r)) => Some((i, r, true)),
            _ => None,
        })
        .filter(|(i, ..)| shown(*i))
        .collect();
    if at.is_empty() || rows.is_empty() {
        return;
    }
    let cat = catalog_now().unwrap_or_else(|_| Arc::new(Catalog::empty()));
    for row in rows {
        for &(i, kind, array) in &at {
            match row.get_mut(i) {
                Some(v @ Value::Text(_)) if array => {
                    if let Ok(names) = reg_array_names(&cat, kind, v) {
                        *v = names;
                    }
                }
                Some(v @ (Value::Int(_) | Value::Reg(..))) => {
                    let (o, kind) = match *v {
                        Value::Reg(o, k) => (o, k),
                        Value::Int(o) => (o, kind),
                        _ => continue,
                    };
                    *v = Value::Text(reg_shown(&cat, kind, o));
                }
                _ => {}
            }
        }
    }
}

/// A reg array's text, its objects' oids, as the array of their names `reg_shown` writes: what a client reads, and
/// what `::text` and the functions of text give. An element that is a name already (a regtype[] kept by a version
/// that kept names) stays as it is.
pub(super) fn reg_array_names(cat: &Catalog, kind: Reg, v: &Value) -> Result<Value> {
    let Value::Text(t) = v else { return Ok(v.clone()) };
    let named = map_array_text(t, &mut |x| {
        Ok(x.map(|x| match x.parse::<i64>() {
            Ok(o) => reg_shown(cat, kind, o),
            Err(_) => x,
        }))
    })?;
    Ok(Value::Text(named))
}

/// A reg array's text with each element read as its reg type reads it (a name or an oid) and kept as the oid:
/// `{int4,25}` as a regtype[] is `{23,25}`. What a driver reads in binary; None for a value that isn't an array.
pub(super) fn reg_array_oids(kind: Reg, v: &Value) -> Result<Option<Value>> {
    let Value::Text(t) = v else { return Ok(None) };
    let cat = catalog_now().unwrap_or_else(|_| Arc::new(Catalog::empty()));
    let oids = map_array_text(t, &mut |x| x.map(|x| Ok(reg_oid(&cat, kind, &x).unwrap_or(0).to_string())).transpose())?;
    Ok(Some(Value::Text(oids)))
}

/// Postgres's name for a built-in type, as `format_type` and `regtype` show it.
pub(super) fn type_name(oid: i64) -> Option<&'static str> {
    PG_TYPES.iter().find(|t| t.0 == oid).map(|t| t.1)
}

/// The object a reg type's text names (see `Reg`): its oid. Digits are an oid (as oidin reads them), `-` none (`0`
/// for an operator, whose names `-` is one of); a name is read as the type's input function reads it (quoted or not,
/// maybe schema-qualified, along the search_path), and one that names nothing is the error Postgres gives for its
/// kind of object (42P01 for a relation, 42704 for a type, 3F000 for a schema, 42883 for a function or operator).
pub(super) fn reg_oid(cat: &Catalog, kind: Reg, text: &str) -> Result<i64> {
    if matches!(kind, Reg::Proc | Reg::Procedure) {
        return proc_oid(cat, kind, text);
    }
    if let Some(o) = oid_digits(text) {
        return o;
    }
    if text == if matches!(kind, Reg::Oper | Reg::Operator) { "0" } else { "-" } {
        return Ok(0);
    }
    let parts = name_parts(text);
    let missing = |code: &'static str, what: &str| SqlError { code, message: what.to_string(), info: None };
    // a name of one part, or of two in pg_catalog: what the kinds that live only in pg_catalog read
    let catalog_name = || match parts.as_slice() {
        [n] => Some(n.as_str()),
        [s, n] if s == "pg_catalog" => Some(n.as_str()),
        _ => None,
    };
    match kind {
        Reg::Class => {
            let found = match parts.as_slice() {
                [n] => cat.rel_named(None, n),
                [s, n] => cat.rel_named(Some(s), n),
                [_, _, _] => return err(UNSUPPORTED, format!("cross-database references are not implemented: {text}")),
                _ => return err(SYNTAX, format!("improper relation name (too many dotted names): {text}")),
            };
            found.ok_or_else(|| missing(NO_TABLE, &format!("relation \"{}\" does not exist", parts.join("."))))
        }
        Reg::Type => type_named(cat, text),
        Reg::Namespace => {
            let [n] = parts.as_slice() else { return err("42602", "invalid name syntax") };
            let o = match n.as_str() {
                "pg_catalog" => 11,
                "public" => 2200,
                "information_schema" => 13000,
                s => cat.schema_oid(s),
            };
            (o != 0).then_some(o).ok_or_else(|| missing("3F000", &format!("schema \"{n}\" does not exist")))
        }
        Reg::Role => {
            let [n] = parts.as_slice() else { return err("42602", "invalid name syntax") };
            match cat.reg.named(n) {
                Some(r) => Ok(r.oid),
                None if n == "postgres" => Ok(10),
                None => err("42704", format!("role \"{n}\" does not exist")),
            }
        }
        Reg::Collation => {
            // a name as an identifier reads it (`de-x-icu` quoted or not, `"C"`), maybe schema-qualified
            let (schema, n) = match &parts[..] {
                [s, n] => (Some(s.as_str()), n.as_str()),
                [n] => (None, n.as_str()),
                _ => (None, text),
            };
            let builtin = matches!(schema, None | Some("pg_catalog")).then(|| super::collation::builtin(n)).flatten();
            match builtin {
                Some((o, _)) => Ok(o),
                None => cat
                    .colls
                    .iter()
                    .position(|(stored, _)| match schema {
                        Some(s) => *stored == qualify(s, n),
                        None => path_schemas()
                            .unwrap_or_else(|| vec!["public".into()])
                            .iter()
                            .any(|s| *stored == qualify(s, n)),
                    })
                    .map(|k| cat.coll_oid(k))
                    .ok_or_else(|| super::collation::missing(schema, n)),
            }
        }
        Reg::Config | Reg::Dictionary => {
            let (all, what): (&[(i64, &str)], &str) = match kind {
                Reg::Config => (&TS_CONFIGS, "configuration"),
                _ => (&TS_DICTS, "dictionary"),
            };
            let found = catalog_name().and_then(|n| all.iter().find(|(_, x)| *x == n));
            found
                .map(|(o, _)| *o)
                .ok_or_else(|| missing("42704", &format!("text search {what} \"{text}\" does not exist")))
        }
        Reg::Oper => {
            let name = oper_name(text);
            match opers().iter().filter(|o| Some(o.1) == name).collect::<Vec<_>>()[..] {
                [] => err(NO_FUNCTION, format!("operator does not exist: {text}")),
                [o] => Ok(o.0),
                _ => err("42725", format!("more than one operator named {}", text.trim())),
            }
        }
        Reg::Operator => {
            let text = text.trim();
            let Some((name, args)) = text.split_once('(') else { return err(BAD_INPUT, "expected a left parenthesis") };
            let Some(args) = args.trim_end().strip_suffix(')') else {
                return err(BAD_INPUT, "expected a right parenthesis");
            };
            let types = type_list(args);
            let [l, r] = types.as_slice() else {
                return err("42P02", "missing argument");
            };
            // each operand as an oid (NONE: 0), or as its name where regtype can't read it
            let operand = |t: &str| match t.trim() {
                t if t.eq_ignore_ascii_case("none") => (Some(0), String::from("NONE")),
                t => (type_named(cat, t).ok(), t.to_lowercase()),
            };
            let ((lo, ln), (ro, rn)) = (operand(l), operand(r));
            let fits = |oid: Option<i64>, name: &str, have: i64, shown: &str| match oid {
                Some(o) => o == have,
                None => name == shown,
            };
            let name = oper_name(name);
            opers()
                .iter()
                .find(|o| Some(o.1) == name && fits(lo, &ln, o.2, o.4) && fits(ro, &rn, o.3, o.5))
                .map(|o| o.0)
                .ok_or_else(|| missing(NO_FUNCTION, &format!("operator does not exist: {text}")))
        }
        Reg::Proc | Reg::Procedure => unreachable!("read by proc_oid"),
    }
}

/// An operator's name as regoper reads it: as written, without spaces or a `pg_catalog.` before it (None for another
/// schema's, which has no built-in operators).
fn oper_name(text: &str) -> Option<&str> {
    let t = text.trim();
    match t.split_once('.') {
        Some((s, n)) if !n.is_empty() && s.trim_matches('"') == "pg_catalog" => Some(n.trim()),
        Some((s, _)) if s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '"') && !s.is_empty() => None,
        _ => Some(t),
    }
}

/// The type a regtype string names: a built-in type by any of its names (`int`, `int4`, `integer`, `character
/// varying(10)`, `pg_catalog.int4`, `"int4"`), an array of one (`integer[]`, `_int4`), an enum made here (along the
/// search_path, or schema-qualified), or Chronos's own vector; 42704 for a name no type has.
fn type_named(cat: &Catalog, text: &str) -> Result<i64> {
    let name = text.trim();
    if let Some(o) = oid_digits(name) {
        return o;
    }
    if name == "-" {
        return Ok(0);
    }
    let missing = |n: &str| SqlError { code: "42704", message: format!("type \"{n}\" does not exist"), info: None };
    // (brackets and parentheses must pair, as Postgres's grammar reads a type name: `int4[` is 42601)
    let (mut square, mut round, mut quoted) = (0i32, 0i32, false);
    for c in name.chars() {
        match c {
            '"' => quoted = !quoted,
            '[' if !quoted => square += 1,
            ']' if !quoted => square -= 1,
            '(' if !quoted => round += 1,
            ')' if !quoted => round -= 1,
            _ => {}
        }
        if square < 0 || round < 0 || square > 1 {
            break;
        }
    }
    if square != 0 || round != 0 || quoted {
        return err(SYNTAX, format!("syntax error in type name \"{name}\""));
    }
    // `"char"` is the one-byte type; `char` (unquoted) is `character`
    if name.trim_start_matches("pg_catalog.") == "\"char\"" {
        return Ok(18);
    }
    // `float(p)`: real up to 24 bits of precision, double precision past them
    let lower = name.to_lowercase();
    if let Some(p) = lower.strip_prefix("float").map(str::trim).and_then(|p| p.strip_prefix('(')?.strip_suffix(')'))
        && let Ok(p) = p.trim().parse::<u32>()
    {
        return Ok(if p <= 24 { 700 } else { 701 });
    }
    // (a type modifier is read and left: `numeric(10,2)`, `character varying(10)`)
    let mut bare = name.to_string();
    while let Some(open) = bare.find('(')
        && let Some(close) = bare[open..].find(')')
    {
        bare.replace_range(open..open + close + 1, "");
    }
    // `mood[]`, `integer[]`, `int4[][]`, `int4[3]`: the array type of one
    let mut elem = bare.trim().to_string();
    let mut dims = 0;
    while let Some(open) = elem.strip_suffix(']').and_then(|e| e.rfind('['))
        && elem[open + 1..elem.len() - 1].trim().chars().all(|c| c.is_ascii_digit())
    {
        elem.truncate(open);
        elem = elem.trim_end().to_string();
        dims += 1;
    }
    if dims > 0 {
        let e = type_named(cat, &elem)?;
        // (a shell type has none)
        let array = if cat.enum_at(e, false).is_some_and(|k| !cat.is_shell(k)) || cat.row_type_of(e, false).is_some() {
            Some(e + 1)
        } else {
            ARRAYS
                .iter()
                .find(|(x, a)| i64::from(*x) == e && x != a)
                .map(|(_, a)| i64::from(*a))
                // (Postgres's array type of a built-in type that Chronos keeps no arrays of: `cstring[]`)
                .or_else(|| PG_ARRAYS.iter().find(|(x, _)| *x == e).map(|(_, a)| *a))
        };
        return array.ok_or_else(|| missing(bare.trim()));
    }
    let bare = bare.trim();
    let parts = name_parts(bare);
    let (schema, plain) = match parts.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => return Err(missing(bare)),
    };
    // (a schema that isn't there is said first, as Postgres's LookupTypeName finds the schema before the type)
    if let Some(s) = schema
        && cat.schema_oid(s) == 0
        && !matches!(s, "pg_temp" | "pg_toast")
    {
        return err("3F000", format!("schema \"{s}\" does not exist"));
    }
    // (a built-in type's names, its words spaced as written: `double  precision`; an interval's fields are read and
    // left, `interval day to second`)
    let mut spaced = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    if spaced.starts_with("interval ") {
        spaced = "interval".into();
    }
    if matches!(schema, None | Some("pg_catalog")) {
        if let Some(t) = PG_TYPES.iter().find(|(_, shown, names)| names.contains(&spaced.as_str()) || *shown == spaced)
        {
            return Ok(t.0);
        }
        if let Some(r) = super::range::by_name(&spaced).or_else(|| super::range::multi_by_name(&spaced)) {
            return Ok(i64::from(r));
        }
        // `_int4`: a built-in type's array type, by its pg_type name
        if let Some(elem) = spaced.strip_prefix('_')
            && let Some(t) = PG_TYPES.iter().find(|(_, _, names)| names.contains(&elem))
            && let Some((_, a)) = ARRAYS.iter().find(|(x, a)| i64::from(*x) == t.0 && x != a)
        {
            return Ok(i64::from(*a));
        }
        // (Chronos's own vector, as regtype shows it: a kept regtype[] may name it)
        if spaced == "vector" {
            return Ok(i64::from(VECTOR_OID));
        }
    }
    // an enum made here: `public.mood` and `s."Mood"` too, the name with its schema, as regtype shows one off the path
    let user = |plain: &str| {
        let stored = match schema {
            Some(s) => qualify(s, plain),
            None => find_named(plain, Kind::Type),
        };
        let found = cat.enums.iter().position(|(n, _)| *n == stored || (schema.is_none() && shown_name(n) == plain));
        if let Some(k) = found {
            return Some(cat.enum_oid(k));
        }
        // a table's, view's or materialized view's row type
        let stored = match schema {
            Some(s) => qualify(s, plain),
            None => find_named(plain, Kind::Rel),
        };
        let i = cat.rels.iter().position(|r| r.name == stored && "rvmpf".contains(r.kind))?;
        Some(cat.row_type_oid(i))
    };
    if let Some(o) = user(plain) {
        return Ok(o);
    }
    // `_mood`, `_t`: the array type of one made here (a shell type has none)
    if let Some(o) = plain.strip_prefix('_').and_then(user)
        && cat.enum_at(o, false).is_none_or(|k| !cat.is_shell(k))
    {
        return Ok(o + 1);
    }
    Err(missing(bare))
}

/// How a reg value shows the object with this oid (see `Reg`): by name, quoted where it needs it, and with its schema
/// where the search_path doesn't find it by that name; `-` for 0 (`0` for an operator), and the number itself for an
/// oid no object of the kind has.
pub(super) fn reg_shown(cat: &Catalog, kind: Reg, oid: i64) -> String {
    if matches!(kind, Reg::Proc | Reg::Procedure) {
        return proc_shown(cat, kind, oid);
    }
    if oid == 0 {
        return if matches!(kind, Reg::Oper | Reg::Operator) { "0" } else { "-" }.into();
    }
    let shown = match kind {
        Reg::Class => cat.shown(oid),
        Reg::Type => Some(cat.regtype_name(oid)),
        Reg::Namespace => match oid {
            11 => Some("pg_catalog".into()),
            2200 => Some("public".into()),
            13000 => Some("information_schema".into()),
            super::catalog_objects::TOAST_SCHEMA_OID => Some("pg_toast".into()),
            TEMP_SCHEMA_OID => Some("pg_temp_3".into()),
            o => cat.schemas.iter().find(|s| cat.schema_oid(s) == o).map(|s| super::collation::quoted(s)),
        },
        Reg::Role => cat.reg.by_oid(oid).map(|r| super::collation::quoted(&r.name)),
        Reg::Collation => cat.collation_shown(oid),
        Reg::Config => TS_CONFIGS.iter().find(|(o, _)| *o == oid).map(|(_, n)| ident(n)),
        Reg::Dictionary => TS_DICTS.iter().find(|(o, _)| *o == oid).map(|(_, n)| ident(n)),
        // (an operator's name alone where no other built-in one has it, else with its schema)
        Reg::Oper => opers().iter().find(|o| o.0 == oid).map(|o| match opers().iter().filter(|x| x.1 == o.1).count() {
            1 => o.1.to_string(),
            _ => format!("pg_catalog.{}", o.1),
        }),
        Reg::Operator => opers().iter().find(|o| o.0 == oid).map(|o| format!("{}({},{})", o.1, o.4, o.5)),
        Reg::Proc | Reg::Procedure => None,
    };
    shown.unwrap_or_else(|| oid.to_string())
}

/// `'name'::regclass` and its kin: with `to_oid`, a value read as the reg type `kind` (a name, an integer, or a reg
/// value of a type that casts to it; 42846 for one that doesn't); else a reg value's (or an oid's) name as text, a
/// name that is already one staying as it is.
fn reg_cast(cat: &Catalog, kind: Reg, to_oid: bool, v: &Value) -> Result<Value> {
    use Value::*;
    if to_oid {
        let o = match v {
            Int(n) => int_oid(*n)?,
            Reg(o, from) if from.casts_to(kind) => *o,
            Reg(_, from) => return err("42846", format!("cannot cast type {} to {}", from.name(), kind.name())),
            v => reg_oid(cat, kind, &v.to_text())?,
        };
        return Ok(Reg(o, kind));
    }
    Ok(Text(match v {
        Reg(o, from) => reg_shown(cat, *from, *o),
        // (a name that is already a name: a catalog row's, as text)
        Text(t) if t.trim().parse::<i64>().is_err() => return Ok(v.clone()),
        v => match coerce(v.clone(), Type::BigInt)? {
            Int(o) => reg_shown(cat, kind, o),
            _ => return Ok(Null),
        },
    }))
}

/// The schema the session's temporary tables are in, as the catalog names them (see `Catalog::of_session`), and its
/// oid: Postgres's own `pg_temp_N` is made as a backend first needs it, so its oid is the first a database's objects
/// get.
pub(super) const TEMP_SCHEMA: &str = "pg_temp";
pub(super) const TEMP_SCHEMA_OID: i64 = 16384;

/// The oid Postgres gives the catalog table or view of this name (what `tableoid` says, and
/// `'pg_class'::regclass`). A view has its own, the one initdb gives it in Postgres 16.15 (system_views.sql makes
/// them from 12000, FirstUnpinnedObjectId), so `'pg_roles'::regclass` shows pg_roles and `1260::regclass` pg_authid.
pub(super) const CATALOG_OIDS: &[(&str, i64)] = &[
    ("pg_class", 1259),
    ("pg_roles", 12000),
    ("pg_shadow", 12005),
    ("pg_user", 12014),
    ("pg_views", 12028),
    ("pg_tables", 12033),
    ("pg_matviews", 12038),
    ("pg_indexes", 12043),
    ("pg_sequences", 12048),
    ("pg_settings", 12104),
    ("pg_type", 1247),
    ("pg_attribute", 1249),
    ("pg_proc", 1255),
    ("pg_database", 1262),
    ("pg_authid", 1260),
    ("pg_namespace", 2615),
    ("pg_constraint", 2606),
    ("pg_index", 2610),
    ("pg_inherits", 2611),
    ("pg_attrdef", 2604),
    ("pg_description", 2609),
    ("pg_sequence", 2224),
    ("pg_am", 2601),
    ("pg_opclass", 2616),
    ("pg_extension", 3079),
    ("pg_enum", 3501),
    ("pg_language", 2612),
    ("pg_depend", 2608),
    ("pg_trigger", 2620),
    ("pg_rewrite", 2618),
    ("pg_collation", 3456),
    ("pg_partitioned_table", 3350),
    ("pg_tablespace", 1213),
    ("pg_operator", 2617),
    ("pg_cast", 2605),
    ("pg_aggregate", 2600),
    ("pg_event_trigger", 3466),
    ("pg_publication", 6104),
    ("pg_subscription", 6100),
    ("pg_policy", 3256),
    ("pg_init_privs", 3394),
    ("pg_default_acl", 826),
    ("pg_shdepend", 1214),
    ("pg_foreign_data_wrapper", 2328),
    ("pg_foreign_server", 1417),
    ("pg_foreign_table", 3118),
    ("pg_statistic_ext", 3381),
    ("pg_transform", 3576),
    ("pg_ts_config", 3602),
    ("pg_ts_dict", 3600),
    ("pg_ts_parser", 3601),
    ("pg_ts_template", 3764),
    ("pg_opfamily", 2753),
    ("pg_largeobject_metadata", 2995),
    ("pg_auth_members", 1261),
    ("pg_user_mapping", 1418),
    ("pg_shdescription", 2396),
    ("pg_amop", 2602),
    ("pg_amproc", 2603),
    ("pg_conversion", 2607),
    ("pg_largeobject", 2613),
    ("pg_statistic", 2619),
    ("pg_db_role_setting", 2964),
    ("pg_statistic_ext_data", 3429),
    ("pg_range", 3541),
    ("pg_shseclabel", 3592),
    ("pg_seclabel", 3596),
    ("pg_ts_config_map", 3603),
    ("pg_replication_origin", 6000),
    ("pg_subscription_rel", 6102),
    ("pg_publication_rel", 6106),
    ("pg_publication_namespace", 6237),
    ("pg_parameter_acl", 6243),
];

pub(super) fn table_oid(name: &str) -> Option<i64> {
    CATALOG_OIDS.iter().find(|(n, _)| *n == name).map(|(_, o)| *o)
}

/// pg_am: oid, name, handler function, and kind (t table, i index); hnsw is pgvector's, bloom contrib/bloom's.
pub(super) const AMS: [(i64, &str, &str, &str); 9] = [
    (2, "heap", "heap_tableam_handler", "t"),
    (403, "btree", "bthandler", "i"),
    (405, "hash", "hashhandler", "i"),
    (783, "gist", "gisthandler", "i"),
    (2742, "gin", "ginhandler", "i"),
    (3580, "brin", "brinhandler", "i"),
    (4000, "spgist", "spghandler", "i"),
    (16300, "hnsw", "hnswhandler", "i"),
    (16303, "bloom", "blhandler", "i"),
];

/// The catalog tables made here, by name (see `CATALOG`).
pub(super) const TABLES: [&str; 21] = [
    "pg_roles",
    "pg_shadow",
    "pg_auth_members",
    "pg_timezone_names",
    "pg_timezone_abbrevs",
    "pg_user",
    "pg_authid",
    "pg_settings",
    "pg_database",
    "pg_inherits",
    "pg_partitioned_table",
    "pg_index",
    "pg_constraint",
    "pg_indexes",
    "pg_am",
    "information_schema.schemata",
    "information_schema.table_constraints",
    "information_schema.key_column_usage",
    "information_schema.referential_constraints",
    "information_schema.constraint_column_usage",
    "information_schema.check_constraints",
];

type Rows = (&'static [(&'static str, Type)], Vec<Vec<Value>>);

/// Catalog table `name` (one of `TABLES`) for world `branch`.
pub(super) fn rows(branch: &str, name: &str, cat: &Catalog) -> Option<Rows> {
    let db_acl = super::acl::shown(&cat.reg, cat.acls.get("db"), 10);
    let here = STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.as_ref().map(|c| c.home.clone())));
    let here = here.as_deref().unwrap_or(branch);
    use Type::{Bool as B, Int as I, Oid, SmallInt as I2, Text as T};
    use Value::{Bool as b, Int as int, Null};
    const SHORTS: Type = Type::Array(21); // int2[]
    const INT2VECTOR: Type = Type::Array(22);
    const OIDVECTOR: Type = Type::Array(30);
    let t = |s: &str| Value::Text(s.into());
    let list = |xs: &[i64]| Value::Text(array_text(xs.iter().map(|&x| int(x)).collect::<Vec<_>>().iter()));
    // int2vector and oidvector, as Postgres writes them: the numbers with spaces between (an array function reads
    // that as the array it is)
    let vector = |xs: &[i64]| Value::Text(xs.iter().map(i64::to_string).collect::<Vec<_>>().join(" "));
    let rel = |i: usize| &cat.rels[i].name;
    let col = |i: usize, n: i64| {
        let r = &cat.rels[i];
        r.at_attnum(n).and_then(|j| r.columns.get(j)).map_or_else(String::new, |c| c.0.clone())
    };
    let kind = |c: &Con| match c.kind {
        'p' => "PRIMARY KEY",
        'u' => "UNIQUE",
        'f' => "FOREIGN KEY",
        _ => "CHECK",
    };
    // a foreign key's parent, and its primary key's constraint name
    let parent = |c: &Con| {
        let (p, cols, del, unique) = c.parent.as_ref()?;
        let i = cat.rels.iter().position(|r| r.name == *p)?;
        Some((i, cols.clone(), *del, unique.clone()))
    };
    let pkey = |i: usize| cat.rels[i].constraints.iter().find(|c| c.kind == 'p').map(|c| c.name.clone());
    // the constraint a foreign key references: a UNIQUE one it names, or the parent's primary key
    let refd = |c: &Con| parent(c).and_then(|(p, _, _, unique)| unique.or_else(|| pkey(p)));
    Some(match name {
        "pg_index" => (
            &[
                ("indexrelid", Oid),
                ("indrelid", Oid),
                ("indnatts", I2),
                ("indnkeyatts", I2),
                ("indisunique", B),
                ("indnullsnotdistinct", B),
                ("indisprimary", B),
                ("indisexclusion", B),
                ("indimmediate", B),
                ("indisclustered", B),
                ("indisvalid", B),
                ("indcheckxmin", B),
                ("indisready", B),
                ("indislive", B),
                ("indisreplident", B),
                ("indkey", INT2VECTOR),
                ("indcollation", OIDVECTOR),
                ("indclass", OIDVECTOR),
                ("indoption", INT2VECTOR),
                ("indexprs", T),
                ("indpred", T),
            ],
            cat.idx
                .iter()
                .enumerate()
                .map(|(k, (i, x))| {
                    let n = x.cols.len() as i64;
                    // (indcollation, indclass and indoption are the key columns' alone: INCLUDE's have none)
                    let nkey = x.nkey.min(x.cols.len());
                    let colls: Vec<i64> = (0..nkey)
                        .map(|p| x.colls.get(p).copied().flatten().map_or(0, |c| cat.collation_oid(c)))
                        .collect();
                    let keyed =
                        |v: &[i64]| vector(&(0..nkey).map(|p| v.get(p).copied().unwrap_or(0)).collect::<Vec<_>>());
                    let exprs: Vec<&String> =
                        x.cols.iter().zip(&x.parts).filter(|(c, _)| **c == 0).map(|(_, p)| p).collect();
                    let exprs = if exprs.is_empty() {
                        Null
                    } else {
                        t(&exprs.iter().map(|e| e.as_str()).collect::<Vec<_>>().join(", "))
                    };
                    vec![
                        int(cat.idx_oid(k)),
                        int(cat.rel_oid(*i)),
                        int(n),
                        int(nkey as i64),
                        b(x.unique),
                        b(x.nnd),
                        b(x.primary),
                        b(false),
                        b(true),
                        b(x.clustered),
                        b(x.valid),
                        b(false),
                        b(true),
                        b(true),
                        b(x.replident),
                        vector(&x.cols),
                        vector(&colls),
                        keyed(&x.classes),
                        keyed(&x.opts),
                        exprs,
                        x.pred.as_deref().map_or(Null, t),
                    ]
                })
                // the system catalogs' primary keys (see `catalog_objects::catalog_pkeys`)
                .chain(super::catalog_objects::catalog_pkeys().into_iter().map(|k| {
                    let n = k.attnums.len() as i64;
                    vec![
                        int(k.oid),
                        int(k.table_oid),
                        int(n),
                        int(n),
                        b(true),
                        b(false),
                        b(true),
                        b(false),
                        b(true),
                        b(false),
                        b(true),
                        b(false),
                        b(true),
                        b(true),
                        b(false),
                        // (0s: the key columns aren't served in pg_attribute, which a key column has to be)
                        vector(&vec![0; k.attnums.len()]),
                        vector(&k.colls),
                        vector(&k.classes),
                        vector(&vec![0; k.attnums.len()]),
                        Null,
                        Null,
                    ]
                }))
                .collect(),
        ),
        "pg_constraint" => (
            &[
                ("oid", Oid),
                ("conname", T),
                ("connamespace", Oid),
                ("contype", Type::Letter),
                ("condeferrable", B),
                ("condeferred", B),
                ("convalidated", B),
                ("conrelid", Oid),
                ("contypid", Oid),
                ("conindid", Oid),
                ("conparentid", Oid),
                ("confrelid", Oid),
                ("confupdtype", Type::Letter),
                ("confdeltype", Type::Letter),
                ("confmatchtype", Type::Letter),
                ("conislocal", B),
                ("coninhcount", I2),
                ("connoinherit", B),
                ("conkey", SHORTS),
                ("confkey", SHORTS),
                ("conbin", T),
            ],
            cat.cons
                .iter()
                .enumerate()
                .map(|(k, (i, c))| {
                    let fk = parent(c);
                    let index = match &fk {
                        Some((p, ..)) => refd(c).and_then(|n| cat.index_of(*p, &n)),
                        None => cat.index_of(*i, &c.name),
                    };
                    let letter = |x: Option<char>| x.map_or_else(|| t(" "), |x| t(&x.to_string()));
                    // a partition's key or foreign key that is its parent's (a check is inherited, not given)
                    let given = c.given.as_ref().and_then(|g| {
                        // (a foreign key's for a partition of the table it references is under one of its own table)
                        if c.given_here {
                            return cat.cons.iter().position(|(r, o)| *r == *i && o.name == *g && o.kind == 'f');
                        }
                        let up = &cat.rels[*i].hier.of.as_ref()?.0;
                        cat.cons.iter().position(|(r, o)| cat.rels[*r].name == *up && o.name == *g)
                    });
                    vec![
                        int(cat.con_oid(k)),
                        t(&c.name),
                        int(cat.schema_oid(schema_part(rel(*i)))),
                        t(&c.kind.to_string()),
                        b(c.defer.0),
                        b(c.defer.1),
                        b(c.valid),
                        int(cat.rel_oid(*i)),
                        int(0),
                        int(index.unwrap_or(0)),
                        int(given.map_or(0, |g| cat.con_oid(g))),
                        int(fk.as_ref().map_or(0, |(p, ..)| cat.rel_oid(*p))),
                        letter(fk.as_ref().map(|f| f.2.0)),
                        letter(fk.as_ref().map(|f| f.2.1)),
                        letter(fk.as_ref().map(|_| 's')),
                        b(c.local),
                        int(c.inh),
                        // (a key or unique constraint is never inherited: a partition's is made for it)
                        b(c.no_inherit || matches!(c.kind, 'p' | 'u' | 'x')),
                        // (a constraint trigger has no columns: NULL)
                        if c.kind == 't' { Null } else { list(&c.cols) },
                        fk.as_ref().map_or(Null, |f| list(&f.1)),
                        c.bin.as_deref().map_or(Null, t),
                    ]
                })
                // the system catalogs' primary keys (see `catalog_objects::catalog_pkeys`), named as their indexes
                .chain(super::catalog_objects::catalog_pkeys().into_iter().map(|k| {
                    vec![
                        int(k.con_oid),
                        t(k.name),
                        int(11),
                        t("p"),
                        b(false),
                        b(false),
                        b(true),
                        int(k.table_oid),
                        int(0),
                        int(k.oid),
                        int(0),
                        int(0),
                        t(" "),
                        t(" "),
                        t(" "),
                        b(true),
                        int(0),
                        b(true),
                        // (none: the key columns aren't served in pg_attribute; see pg_index's)
                        Null,
                        Null,
                        Null,
                    ]
                }))
                // a domain's CHECKs: constraints of the type, not of a table (contypid, no conrelid)
                .chain({
                    cat.domain_checks().into_iter().map(|(oid, k, n, text)| {
                        let shown = domain_check_cond(text);
                        vec![
                            int(oid),
                            t(n),
                            int(cat.schema_oid(schema_part(&cat.enums[k].0))),
                            t("c"),
                            b(false),
                            b(false),
                            b(true),
                            int(0),
                            int(cat.enum_oid(k)),
                            int(0),
                            int(0),
                            int(0),
                            t(" "),
                            t(" "),
                            t(" "),
                            b(true),
                            int(0),
                            b(false),
                            Null,
                            Null,
                            t(&shown),
                        ]
                    })
                })
                .collect(),
        ),
        "pg_indexes" => (
            &[("schemaname", T), ("tablename", T), ("indexname", T), ("tablespace", T), ("indexdef", T)],
            cat.idx
                .iter()
                .map(|(i, x)| {
                    vec![
                        t(schema_part(rel(*i))),
                        t(rel_part(rel(*i))),
                        t(&x.name),
                        Null,
                        t(&cat.index_def(*i, x, 0, false)),
                    ]
                })
                .collect(),
        ),
        // every world is a database; Postgres's three are there too unless a world is named so (the one
        // asked in has its owner and grants)
        "pg_database" => (
            &[
                ("oid", Oid),
                ("datname", Type::Name),
                ("datdba", Oid),
                ("encoding", I),
                ("datlocprovider", T),
                ("datistemplate", B),
                ("datallowconn", B),
                ("datconnlimit", I),
                ("dattablespace", Oid),
                ("datcollate", T),
                ("datctype", T),
                ("datacl", Type::Array(25)),
            ],
            [("template1", 1, true), ("template0", 4, true), ("postgres", 5, false)]
                .into_iter()
                .filter(|(n, ..)| !cat.worlds.iter().any(|w| w == n))
                .map(|(n, o, template)| (n.to_string(), o, template))
                .chain(cat.worlds.iter().enumerate().map(|(i, w)| (w.clone(), 16384 + i as i64, false)))
                .map(|(n, o, template)| {
                    vec![
                        int(o),
                        t(&n),
                        int(if n == here { db_acl.0 } else { 10 }),
                        int(6),
                        t("c"),
                        b(template),
                        b(!template || n != "template0"),
                        int(-1),
                        int(1663),
                        t("C"),
                        t("C"),
                        if n == here { db_acl.1.clone() } else { Null },
                    ]
                })
                .collect(),
        ),
        "pg_am" => (
            &[("oid", Oid), ("amname", T), ("amhandler", Type::Reg(Reg::Proc)), ("amtype", T)],
            cat.access_methods()
                .into_iter()
                .map(|(o, n, h, k)| {
                    let handler = catalog_schema::builtin_procs(&h).first().copied().unwrap_or(0);
                    vec![int(o), t(&n), Value::Reg(handler, Reg::Proc), t(&k.to_string())]
                })
                .collect(),
        ),
        // the roles (see `roles`): the bootstrap superuser `chronos` (oid 10), Postgres's predefined ones, those
        // made with CREATE ROLE, and the name this session logged in as when it is none of them (a superuser,
        // since `where rolname = current_user` is how tools ask what they may do)
        "pg_roles" | "pg_authid" => (
            &[
                ("oid", Oid),
                ("rolname", T),
                ("rolsuper", B),
                ("rolinherit", B),
                ("rolcreaterole", B),
                ("rolcreatedb", B),
                ("rolcanlogin", B),
                ("rolreplication", B),
                ("rolconnlimit", I),
                ("rolpassword", T),
                ("rolvaliduntil", Type::TimestampTz),
                ("rolbypassrls", B),
                ("rolconfig", Type::Array(25)),
            ],
            // (an agent's statement is refused before it reads them: see `acl::check`; and should one
            // get here, it is shown no password)
            super::roles::role_rows(&cat.reg, login().as_deref(), name == "pg_authid" && !super::acl::agents()),
        ),
        "pg_user" | "pg_shadow" => (
            &[
                ("usename", T),
                ("usesysid", Oid),
                ("usecreatedb", B),
                ("usesuper", B),
                ("userepl", B),
                ("usebypassrls", B),
                ("passwd", T),
                ("valuntil", Type::TimestampTz),
                ("useconfig", Type::Array(25)),
            ],
            super::roles::user_rows(&cat.reg, login().as_deref(), name == "pg_shadow" && !super::acl::agents()),
        ),
        "pg_auth_members" => (
            &[
                ("oid", Oid),
                ("roleid", Oid),
                ("member", Oid),
                ("grantor", Oid),
                ("admin_option", B),
                ("inherit_option", B),
                ("set_option", B),
            ],
            super::roles::member_rows(&cat.reg),
        ),
        // the zones SET TIME ZONE takes (the system's tz database), as they are right now, and the abbreviations
        // AT TIME ZONE and timestamp text take
        "pg_timezone_names" => (
            &[("name", T), ("abbrev", T), ("utc_offset", Type::Interval), ("is_dst", B)],
            super::time::zone_table()
                .into_iter()
                .map(|(name, abbr, secs, dst)| {
                    let i = super::time::Interval { months: 0, days: 0, micros: i64::from(secs) * 1_000_000 };
                    vec![t(&name), t(&abbr), Value::Interval(i), b(dst)]
                })
                .collect(),
        ),
        "pg_timezone_abbrevs" => (
            &[("abbrev", T), ("utc_offset", Type::Interval), ("is_dst", B)],
            super::tz::abbrevs()
                .iter()
                .map(|(a, secs, dst)| {
                    let i = super::time::Interval { months: 0, days: 0, micros: i64::from(*secs) * 1_000_000 };
                    vec![t(a), Value::Interval(i), b(*dst)]
                })
                .collect(),
        ),
        // what SHOW ALL lists: the settings this session can read (see `all_settings`)
        "pg_settings" => (
            &[
                ("name", T),
                ("setting", T),
                ("unit", T),
                ("category", T),
                ("short_desc", T),
                ("extra_desc", T),
                ("context", T),
                ("vartype", T),
                ("source", T),
                ("min_val", T),
                ("max_val", T),
                ("enumvals", T),
                ("boot_val", T),
                ("reset_val", T),
                ("sourcefile", T),
                ("sourceline", I),
                ("pending_restart", B),
            ],
            super::all_settings()
                .into_iter()
                .map(|(name, value, source, read_only)| match super::settings::find(&name) {
                    // Postgres's own settings, with the facts it keeps of them
                    Some(d) => vec![
                        t(d.name),
                        t(&d.base(&value)),
                        if d.unit.is_empty() { Null } else { t(d.unit) },
                        t(d.category),
                        t(d.short),
                        if d.extra.is_empty() { Null } else { t(d.extra) },
                        t(d.context),
                        t(d.vartype),
                        t(source),
                        if d.min.is_empty() { Null } else { t(d.min) },
                        if d.max.is_empty() { Null } else { t(d.max) },
                        d.enum_array().map_or(Null, |e| t(&e)),
                        t(d.boot),
                        t(d.value),
                        Null,
                        Null,
                        b(false),
                    ],
                    // a custom one (`app.org_id`), or one only Chronos has
                    None => vec![
                        t(&name),
                        t(&value),
                        Null,
                        t("Customized Options"),
                        t(super::chronos_setting_about(&name)),
                        Null,
                        t(if read_only { "internal" } else { "user" }),
                        t("string"),
                        t(source),
                        Null,
                        Null,
                        Null,
                        t(&value),
                        t(&value),
                        Null,
                        Null,
                        b(false),
                    ],
                })
                .collect(),
        ),
        // each table below another (a partition, or one that INHERITS), and each partition's index that is its
        // parent's
        "pg_inherits" => (
            &[("inhrelid", Oid), ("inhparent", Oid), ("inhseqno", I), ("inhdetachpending", B)],
            cat.rels
                .iter()
                .enumerate()
                .flat_map(|(i, r)| {
                    let up = r.hier.parents.iter().enumerate().filter_map(move |(n, p)| {
                        let pi = cat.rels.iter().position(|x| x.name == *p)?;
                        Some(vec![int(cat.rel_oid(i)), int(cat.rel_oid(pi)), int(n as i64 + 1), b(false)])
                    });
                    up.collect::<Vec<_>>()
                })
                .chain(cat.idx.iter().enumerate().filter_map(|(k, (i, x))| {
                    let parent = &cat.rels[*i].hier.of.as_ref()?.0;
                    let pk = cat
                        .idx
                        .iter()
                        .position(|(r, y)| cat.rels[*r].name == *parent && Some(&y.name) == x.given.as_ref())?;
                    Some(vec![int(cat.idx_oid(k)), int(cat.idx_oid(pk)), int(1), b(false)])
                }))
                .collect(),
        ),
        "pg_partitioned_table" => (
            &[
                ("partrelid", Oid),
                ("partstrat", T),
                ("partnatts", I2),
                ("partdefid", Oid),
                ("partattrs", INT2VECTOR),
                ("partclass", OIDVECTOR),
                ("partcollation", OIDVECTOR),
                ("partexprs", T),
            ],
            cat.rels
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    let p = r.hier.part.as_ref()?;
                    let default = p.default.as_ref().and_then(|d| cat.rels.iter().position(|x| x.name == *d));
                    let zeros = vector(&vec![0; p.attrs.len()]);
                    Some(vec![
                        int(cat.rel_oid(i)),
                        t(&p.strat.to_string()),
                        int(p.attrs.len() as i64),
                        int(default.map_or(0, |d| cat.rel_oid(d))),
                        vector(&p.attrs),
                        zeros.clone(),
                        zeros,
                        p.exprs.as_deref().map_or(Null, t),
                    ])
                })
                .collect(),
        ),
        "information_schema.schemata" => (
            &[("catalog_name", T), ("schema_name", T), ("schema_owner", T)],
            ["information_schema", "pg_catalog", "public"]
                .into_iter()
                .chain(cat.schemas.iter().map(String::as_str))
                .map(|s| vec![t(branch), t(s), t("chronos")])
                .collect(),
        ),
        "information_schema.table_constraints" => (
            &[
                ("constraint_catalog", T),
                ("constraint_schema", T),
                ("constraint_name", T),
                ("table_catalog", T),
                ("table_schema", T),
                ("table_name", T),
                ("constraint_type", T),
                ("is_deferrable", T),
                ("initially_deferred", T),
                ("enforced", T),
                ("nulls_distinct", T),
            ],
            cat.cons
                .iter()
                // (an exclusion constraint isn't one information_schema lists)
                .filter(|(_, c)| !matches!(c.kind, 't' | 'x'))
                .map(|(i, c)| {
                    let s = schema_part(rel(*i));
                    vec![
                        t(branch),
                        t(s),
                        t(&c.name),
                        t(branch),
                        t(s),
                        t(rel_part(rel(*i))),
                        t(kind(c)),
                        t(if c.defer.0 { "YES" } else { "NO" }),
                        t(if c.defer.1 { "YES" } else { "NO" }),
                        t("YES"),
                        if c.kind == 'u' { t("YES") } else { Null },
                    ]
                })
                .collect(),
        ),
        "information_schema.key_column_usage" => (
            &[
                ("constraint_catalog", T),
                ("constraint_schema", T),
                ("constraint_name", T),
                ("table_catalog", T),
                ("table_schema", T),
                ("table_name", T),
                ("column_name", T),
                ("ordinal_position", I),
                ("position_in_unique_constraint", I),
            ],
            cat.cons
                .iter()
                .filter(|(_, c)| !matches!(c.kind, 'c' | 'x'))
                .flat_map(|(i, c)| {
                    let s = schema_part(rel(*i));
                    let fk = c.parent.is_some();
                    c.cols.iter().enumerate().map(move |(n, a)| {
                        vec![
                            t(branch),
                            t(s),
                            t(&c.name),
                            t(branch),
                            t(s),
                            t(rel_part(rel(*i))),
                            t(&col(*i, *a)),
                            int(n as i64 + 1),
                            if fk { int(n as i64 + 1) } else { Null },
                        ]
                    })
                })
                .collect(),
        ),
        "information_schema.referential_constraints" => (
            &[
                ("constraint_catalog", T),
                ("constraint_schema", T),
                ("constraint_name", T),
                ("unique_constraint_catalog", T),
                ("unique_constraint_schema", T),
                ("unique_constraint_name", T),
                ("match_option", T),
                ("update_rule", T),
                ("delete_rule", T),
            ],
            cat.cons
                .iter()
                .filter_map(|(i, c)| {
                    let (p, _, (upd, del), _) = parent(c)?;
                    let rule = |x: char| match x {
                        'c' => "CASCADE",
                        'n' => "SET NULL",
                        'd' => "SET DEFAULT",
                        'r' => "RESTRICT",
                        _ => "NO ACTION",
                    };
                    Some(vec![
                        t(branch),
                        t(schema_part(rel(*i))),
                        t(&c.name),
                        t(branch),
                        t(schema_part(rel(p))),
                        refd(c).map_or(Null, |n| t(&n)),
                        t("NONE"),
                        t(rule(upd)),
                        t(rule(del)),
                    ])
                })
                .collect(),
        ),
        "information_schema.constraint_column_usage" => (
            &[
                ("table_catalog", T),
                ("table_schema", T),
                ("table_name", T),
                ("column_name", T),
                ("constraint_catalog", T),
                ("constraint_schema", T),
                ("constraint_name", T),
            ],
            cat.cons
                .iter()
                .filter(|(_, c)| c.kind != 'x')
                .flat_map(|(i, c)| {
                    // a foreign key uses the columns it references; others, their own
                    let (on, cols) = parent(c).map_or_else(|| (*i, c.cols.clone()), |(p, cols, ..)| (p, cols));
                    let s = schema_part(rel(*i));
                    cols.into_iter().map(move |a| {
                        vec![
                            t(branch),
                            t(schema_part(rel(on))),
                            t(rel_part(rel(on))),
                            t(&col(on, a)),
                            t(branch),
                            t(s),
                            t(&c.name),
                        ]
                    })
                })
                .collect(),
        ),
        "information_schema.check_constraints" => (
            &[("constraint_catalog", T), ("constraint_schema", T), ("constraint_name", T), ("check_clause", T)],
            cat.cons
                .iter()
                .filter(|(_, c)| c.kind == 'c')
                .map(|(i, c)| vec![t(branch), t(schema_part(rel(*i))), t(&c.name), t(check_clause(&c.def))])
                .collect(),
        ),
        _ => return None,
    })
}

/// Reads `text` as the type `ty` names (a length or precision in it too): the type's own error where it doesn't.
fn read_as(cat: &Catalog, text: &str, ty: &str) -> Result<Result<()>> {
    // a domain: read as its base type, then held to its constraints (an error in a CHECK itself is raised)
    let domain = super::with_path_view(|v| -> Result<Option<Result<()>>> {
        match super::domain::named(v, ty)? {
            Some(n) => super::domain::read_as(v, &n, text, |base| read_as(cat, text, base)).map(Some),
            None => Ok(None),
        }
    });
    if let Some(r) = domain.transpose()?.flatten() {
        return Ok(r);
    }
    let oid = reg_oid(cat, super::Reg::Type, ty)?;
    // an enum made with CREATE TYPE: one of its labels (enum_in)
    if let Some(k) = (0..cat.enums.len()).find(|&k| cat.enum_oid(k) == oid) {
        let (name, labels) = &cat.enums[k];
        if labels.iter().any(|l| l == text) {
            return Ok(Ok(()));
        }
        return Ok(Err(SqlError {
            code: "22P02",
            message: format!("invalid input value for enum {}: \"{text}\"", rel_part(name)),
            info: None,
        }));
    }
    // (character(n), bpchar, is text with its length checked below)
    let Some(t) = Type::from_oid(oid as u32).or((oid == 1042).then_some(Type::Text)) else { return Ok(Ok(())) };
    // (an oidvector's or int2vector's elements are read one by one, as their types read them)
    if let Type::Array(v @ (22 | 30)) = t {
        let elem = if v == 30 { Type::Oid } else { Type::SmallInt };
        for word in text.split_whitespace() {
            if let Err(mut e) = coerce(Value::Text(word.into()), elem) {
                // (the vector types name what is left after the digits they read)
                if e.code == "22P02" {
                    let rest = word.trim_start_matches(|c: char| c.is_ascii_digit());
                    e.message = format!("invalid input syntax for type {}: \"{rest}\"", elem.name());
                }
                return Ok(Err(e));
            }
        }
        return Ok(Ok(()));
    }
    let read = match coerce(Value::Text(text.into()), t) {
        Err(e) => return Ok(Err(e)),
        Ok(v) => v,
    };
    // numeric(p, s): the value must fit (Postgres's apply_typmod)
    if let Value::Numeric(d) = &read
        && ty.contains('(')
        && let Ok(toks) = super::lex(&format!("x {ty}"))
        && let Ok(c) = super::Parser::new(toks).column()
        && let Some((p, s)) = c.numeric
    {
        return Ok(super::fit_numeric(d, p, s).map(|_| ()));
    }
    if ty.contains('(')
        && let Ok(toks) = super::lex(&format!("x {ty}"))
        && let Ok(c) = super::Parser::new(toks).column()
        && let Some((n, fixed)) = c.chars
    {
        if matches!(c.ty, Type::Bit | Type::VarBit) {
            let bits = super::bit::parse(text)?;
            return Ok(super::bit::assign(c.ty, &bits, n, fixed).map(|_| ()));
        }
        let len = text.trim_end_matches(' ').chars().count();
        if len > n as usize {
            let name = if fixed { "character" } else { "character varying" };
            return Ok(Err(SqlError {
                code: "22001",
                message: format!("value too long for type {name}({n})"),
                info: None,
            }));
        }
    }
    Ok(Ok(()))
}

/// pg_input_error_info(text, type): the error reading the text as the type raises, as (message, detail, hint,
/// sql_error_code); all null where it reads.
pub(super) fn input_error_info(text: &Value, ty: &Value) -> Result<Vec<Value>> {
    let cat = catalog_now()?;
    Ok(match read_as(&cat, &text.to_text(), &ty.to_text())? {
        Ok(_) => vec![Value::Null; 4],
        Err(e) => {
            let info = e.info.as_deref();
            let some = |s: Option<&String>| s.map_or(Value::Null, |s| Value::Text(s.clone()));
            vec![
                Value::Text(e.message.clone()),
                some(info.and_then(|i| i.detail.as_ref())),
                some(info.and_then(|i| i.hint.as_ref())),
                Value::Text(e.code.to_string()),
            ]
        }
    })
}
