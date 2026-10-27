//! CREATE DATABASE and DROP DATABASE: what Rails' `db:create`, Django's test runner, Prisma's shadow
//! database and every `docker-entrypoint-initdb.d` script send.
//!
//! A database is a world. `CREATE DATABASE app` makes a world with no parent and nothing in it
//! (Postgres copies `template1`, which starts empty), and a client connects to it by name
//! (`dbname=app`). `TEMPLATE some_world` forks that world: a copy-on-write copy, instant at any size,
//! and so a child of it (the world can't be dropped while the copy exists). `DROP DATABASE` drops the
//! world. Options that only describe how Postgres stores the database (owner, encoding, tablespace)
//! are checked and left; the ones that would change what a query returns (a locale, a connection
//! limit) are accepted with a warning that says they aren't applied.

use super::*;

/// `CREATE DATABASE`, its options read.
#[derive(Clone, Debug)]
pub(super) struct Create {
    pub name: String,
    /// TEMPLATE, unless it names one of Postgres's own (`template0`, `template1`: an empty database)
    pub template: Option<String>,
    /// What was asked for and isn't applied, as a warning says it
    pub ignored: Vec<String>,
}

/// The maintenance databases every Postgres has: `postgres` is where tools connect to create the
/// others, and here it is `main` unless a world is named so.
pub(super) const MAINTENANCE: [&str; 2] = ["postgres", "template1"];

/// The world a database name means: the world of that name, or `main` for the maintenance databases.
pub fn world_of(db: &Db, name: &str) -> Option<String> {
    if let Ok(w) = db.world(name) {
        return Some(w.name).filter(|n| !n.starts_with("_tx_"));
    }
    MAINTENANCE.contains(&name).then(|| crate::MAIN.to_string())
}

fn is_c_locale(v: &str) -> bool {
    matches!(v.to_lowercase().as_str(), "c" | "posix" | "c.utf-8" | "c.utf8")
}

impl Parser {
    /// One option's value: a name, a string, or a number (with its sign). `DEFAULT` is None.
    fn database_value(&mut self) -> Result<Option<String>> {
        let negative = self.sym("-");
        Ok(match self.next() {
            Some(Tok::Word(w)) if w == "default" && !negative => None,
            Some(Tok::Word(w) | Tok::Ident(w) | Tok::Str(w)) if !negative => Some(w),
            Some(Tok::Num(n)) => Some(if negative { format!("-{n}") } else { n }),
            t => {
                self.put_back(t);
                return self.fail("expected a value for the option");
            }
        })
    }

    /// After CREATE DATABASE: name [WITH] [option [=] value ...].
    pub(super) fn create_database(&mut self) -> Result<Stmt> {
        let name = self.name()?;
        self.kw("with");
        let mut c = Create { name, template: None, ignored: vec![] };
        let mut seen: Vec<String> = vec![];
        while !self.at_end() {
            let mut opt = match self.next() {
                Some(Tok::Word(w)) => w,
                t => {
                    self.put_back(t);
                    return self.fail("expected a database option");
                }
            };
            if opt == "connection" {
                self.expect_kw("limit")?;
                opt = "connection limit".into();
            }
            self.sym("=");
            if seen.contains(&opt) {
                return err(SYNTAX, "conflicting or redundant options");
            }
            let value = self.database_value()?;
            let text = value.clone().unwrap_or_default();
            let unapplied = |what: &str| format!("{what} isn't applied: Chronos sorts and stores text as UTF8 bytes");
            match opt.as_str() {
                "owner" | "oid" | "locale_provider" | "collation_version" => {} // no roles, no OIDs to choose
                "template" => c.template = value,
                "encoding" => {
                    if !matches!(text.to_lowercase().as_str(), "" | "utf8" | "utf-8" | "unicode" | "6") {
                        // a number, or a name of an encoding Postgres has: known, and not one this stores
                        let plain = |s: &str| s.replace(['-', '_'], "").to_lowercase();
                        let known = text.parse::<usize>().is_ok_and(|n| n < funcs::ENCODINGS.len())
                            || funcs::ENCODINGS.iter().any(|e| plain(e) == plain(&text));
                        return match known {
                            true => err(
                                UNSUPPORTED,
                                format!("encoding \"{text}\" isn't supported: Chronos stores text as UTF8"),
                            ),
                            false => err("42704", format!("\"{text}\" is not a valid encoding name")),
                        };
                    }
                }
                "strategy" => {
                    if !matches!(text.to_lowercase().as_str(), "" | "wal_log" | "file_copy") {
                        return err("22023", format!("invalid create database strategy \"{text}\""));
                    }
                }
                "locale" | "lc_collate" | "lc_ctype" | "icu_locale" | "icu_rules" => {
                    if !text.is_empty() && !is_c_locale(&text) {
                        c.ignored.push(unapplied(&format!("{} '{text}'", opt.to_uppercase())));
                    }
                }
                "tablespace" => {
                    if !matches!(text.as_str(), "" | "pg_default") {
                        return err("42704", format!("tablespace \"{text}\" does not exist"));
                    }
                }
                "allow_connections" | "is_template" => {
                    let on = match text.to_lowercase().as_str() {
                        "" | "true" | "on" | "yes" | "1" | "t" | "y" => true,
                        "false" | "off" | "no" | "0" | "f" | "n" => false,
                        _ => return err("22023", format!("{opt} requires a Boolean value")),
                    };
                    // the default of ALLOW_CONNECTIONS is true, of IS_TEMPLATE false
                    if on == (opt == "is_template") {
                        c.ignored.push(format!(
                            "{} = {on} isn't applied: every database here can be connected to and copied",
                            opt.to_uppercase()
                        ));
                    }
                }
                "connection limit" => {
                    let n: i64 = text.parse().map_err(|_| SqlError {
                        code: "22023",
                        message: format!("invalid connection limit: {text}"),
                        info: None,
                    })?;
                    if n >= 0 {
                        c.ignored.push(format!(
                            "CONNECTION LIMIT {n} isn't applied: Chronos doesn't limit connections per database"
                        ));
                    }
                }
                other => return err(SYNTAX, format!("option \"{other}\" not recognized")),
            }
            seen.push(opt);
        }
        Ok(Stmt::CreateDatabase(Box::new(c)))
    }

    /// After DROP DATABASE: [IF EXISTS] name [[WITH] (FORCE)].
    pub(super) fn drop_database(&mut self) -> Result<Stmt> {
        let if_exists = self.kw("if") && {
            self.expect_kw("exists")?;
            true
        };
        let name = self.name()?;
        if self.kw("with") || matches!(self.peek(), Some(Tok::Sym("("))) {
            self.expect_sym("(")?;
            self.expect_kw("force")?;
            self.expect_sym(")")?;
        }
        Ok(Stmt::DropDatabase { name, if_exists })
    }
}

/// CREATE DATABASE.
pub(super) fn create(db: &Db, c: &Create) -> Result<Output> {
    for what in &c.ignored {
        notice("WARNING", what.clone());
    }
    let exists = |name: &str| world_of(db, name).is_some() || name == "template0";
    if exists(&c.name) {
        return err("42P04", format!("database \"{}\" already exists", c.name));
    }
    let meta = serde_json::Value::Object(Default::default());
    match c.template.as_deref() {
        None | Some("template0" | "template1") => {
            db.create_empty(&c.name, meta)?;
        }
        Some(t) => {
            let Some(from) = world_of(db, t) else {
                return err("3D000", format!("template database \"{t}\" does not exist"));
            };
            db.fork_with(&from, &c.name, meta)?;
        }
    }
    tag("CREATE DATABASE")
}

/// DROP DATABASE `name`; `open` are the worlds the session is using, which can't go.
pub(super) fn drop(db: &Db, name: &str, if_exists: bool, open: &[&str]) -> Result<Output> {
    let Some(world) = world_of(db, name) else {
        if name == "template0" {
            return err("42809", "cannot drop a template database");
        }
        if if_exists {
            notice("NOTICE", format!("database \"{name}\" does not exist, skipping"));
            return tag("DROP DATABASE");
        }
        return err("3D000", format!("database \"{name}\" does not exist"));
    };
    if open.contains(&world.as_str()) {
        return err("55006", format!("cannot drop the currently open database \"{name}\""));
    }
    if world == crate::MAIN {
        return err(
            UNSUPPORTED,
            format!("can't drop \"{name}\": it is the main world, the one a fresh connection reaches"),
        );
    }
    db.discard(&world).map_err(|e| match e {
        crate::Error::HasChildren(_) => SqlError {
            code: "2BP01",
            message: format!(
                "cannot drop database \"{name}\" because worlds were forked from it (a database made with TEMPLATE is a fork of it); DROP WORLD \"{name}\" CASCADE drops it and them"
            ),
            info: None,
        },
        e => e.into(),
    })?;
    tag("DROP DATABASE")
}
