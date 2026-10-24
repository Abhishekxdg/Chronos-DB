//! COPY: loading and unloading rows in bulk over the Postgres protocol's copy sub-protocol.
//!
//!   COPY t [(cols)] FROM STDIN [options]    rows stream in, and go in a part at a time (`CopyIn::run`)
//!   COPY t [(cols)] | (query) TO STDOUT     rows stream out (`Session::copy_out`)
//!
//! Text, CSV and (in) binary, with Postgres's options (FORMAT, DELIMITER, NULL, HEADER, QUOTE, ESCAPE,
//! FORCE_QUOTE, FORCE_NOT_NULL, FORCE_NULL), in both the `WITH (...)` and the old syntax psql's
//! \copy sends. A COPY FROM is one INSERT of values (never parsed SQL), so all or nothing: it takes
//! the rows a part at a time as the data arrives, and a big one goes on in parts as any big
//! INSERT does (see `bulk`). Values are parsed to their column's type as they arrive, so a bad
//! one names its line and column.
//! Programs on the server are refused, and so are its files unless CHRONOS_ALLOW_SERVER_COPY=1, then
//! only for the roles Postgres lets (see `server_file`): the client reads and writes its files (\copy).

use super::*;

/// How COPY reads or writes rows.
#[derive(Clone, Debug)]
pub(super) struct Format {
    csv: bool,
    /// COPY FROM only: Postgres's binary format (see `CopyIn::binary_in`)
    binary: bool,
    header: bool,
    /// HEADER MATCH: the header line's fields must be the columns' names (COPY FROM only)
    header_match: bool,
    /// FREEZE: refused on a partitioned table, as in Postgres (rows are never frozen here)
    freeze: bool,
    /// DEFAULT 'marker': a field that is it takes its column's default (COPY FROM only)
    default: Option<String>,
    delim: u8,
    null: String,
    quote: u8,
    escape: u8,
    /// FORCE_QUOTE: these columns (all with `*`) are always quoted; CSV TO only
    force_quote: Vec<String>,
    force_all: bool,
    /// FORCE_NOT_NULL / FORCE_NULL: an unquoted NULL string is text / a quoted one is NULL; CSV FROM only
    force_not_null: Vec<String>,
    force_null: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct Copy {
    pub(super) table: Option<String>,
    pub(super) cols: Option<Vec<String>>,
    pub(super) query: Option<Box<Select>>,
    pub(super) from: bool,
    /// COPY ... FROM 'path' / TO 'path', read and written by the server (only with CHRONOS_ALLOW_SERVER_COPY=1, and
    /// for the roles Postgres lets: see `server_file`)
    pub(super) file: Option<String>,
    format: Format,
    /// COPY FROM ... WHERE: the rows that don't meet it are skipped
    pub(super) filter: Option<Expr>,
    /// COPY (INSERT | UPDATE | DELETE ... RETURNING ...) TO: the statement, whose RETURNING rows go out
    pub(super) dml: Option<Box<Stmt>>,
}

fn feature(message: impl Into<String>) -> SqlError {
    SqlError { code: UNSUPPORTED, message: message.into(), info: None }
}

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError { code: "22023", message: message.into(), info: None }
}

impl Parser {
    /// After COPY: `table [(cols)] FROM STDIN` or `table [(cols)] | (query) TO STDOUT`, then options.
    pub(super) fn copy(&mut self) -> Result<Stmt> {
        let (mut table, mut cols, mut query) = (None, None, None);
        if self.sym("(") {
            let q = match self.stmt()? {
                Stmt::Select(q) | Stmt::Locking(q, _) => q,
                // (whether it has a RETURNING is asked when it runs, after its table's rules: see `copy_out`)
                st @ (Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. }) => {
                    self.expect_sym(")")?;
                    self.expect_kw("to")?;
                    let mut file = None;
                    match self.next() {
                        Some(Tok::Word(w)) if w == "stdout" => {}
                        // (who may, and whether the server lets anyone, is asked as it runs: see `server_file`)
                        Some(Tok::Str(path)) => file = Some(path),
                        t => {
                            self.put_back(t);
                            return self.fail("expected STDOUT");
                        }
                    }
                    let format = self.copy_options(false)?;
                    return Ok(Stmt::Copy(Box::new(Copy {
                        table: None,
                        cols: None,
                        query: None,
                        from: false,
                        file,
                        format,
                        filter: None,
                        dml: Some(Box::new(st)),
                    })));
                }
                Stmt::CreateAs { .. } => return err(UNSUPPORTED, "COPY (SELECT INTO) is not supported"),
                Stmt::Merge(_) => return err(UNSUPPORTED, "MERGE not supported in COPY"),
                _ => return err(UNSUPPORTED, "COPY (query) TO takes a SELECT"),
            };
            self.expect_sym(")")?;
            query = Some(q);
        } else {
            let mut t = self.name()?;
            if self.sym(".") {
                let n = self.name()?;
                t = if t == "public" { n } else { format!("{t}.{n}") };
            } else {
                t = find_named(&t, Kind::Rel);
            }
            table = Some(t);
            if self.sym("(") {
                cols = Some(self.names()?);
            }
        }
        let from = query.is_none() && self.kw("from");
        if !from {
            self.expect_kw("to")?;
        }
        if from && query.is_some() {
            return err(SYNTAX, "COPY (query) goes TO STDOUT only; COPY FROM needs a table");
        }
        let mut file = None;
        match self.next() {
            Some(Tok::Word(w)) if w == if from { "stdin" } else { "stdout" } => {}
            // (who may, and whether the server lets anyone, is asked as it runs: see `server_file`)
            Some(Tok::Str(path)) => file = Some(path),
            Some(Tok::Word(w)) if w == "program" => return Err(server_files()),
            t => {
                self.put_back(t);
                return self.fail(if from { "expected STDIN" } else { "expected STDOUT" });
            }
        }
        let format = self.copy_options(from)?;
        let mut filter = None;
        if self.is_kw("where") {
            if !from {
                return self.err_at(self.i, UNSUPPORTED, "WHERE clause not allowed with COPY TO");
            }
            self.i += 1;
            let (places, aggs) = (self.copy_where_places(), self.agg_at.len());
            let e = self.expr()?;
            self.copy_where_allowed(&e, places, aggs)?;
            filter = Some(e);
        }
        Ok(Stmt::Copy(Box::new(Copy { table, cols, query, from, file, format, filter, dml: None })))
    }

    /// Where in COPY FROM's WHERE condition (its tokens from here, before it is read) Postgres places what it refuses
    /// there: the first function called (an aggregate, window or set-returning function), and the first subquery
    /// (at its IN or EXISTS, as its SubLink is placed), as characters from 1.
    fn copy_where_places(&self) -> (Option<u32>, Option<u32>) {
        let end = (self.i..self.t.len()).find(|&k| self.t[k] == Tok::Sym(";")).unwrap_or(self.t.len());
        let toks = &self.t[self.i..end];
        let linking = |t: Option<&Tok>| matches!(t, Some(Tok::Word(w)) if ["in", "exists", "any", "all", "some"].contains(&w.as_str()));
        let call = (0..toks.len()).find(|&k| {
            matches!(&toks[k], Tok::Word(_) | Tok::Ident(_))
                && !linking(Some(&toks[k]))
                && !matches!(&toks[k], Tok::Word(w) if w == "array" || w == "row" || w == "not")
                && toks.get(k + 1) == Some(&Tok::Sym("("))
        });
        let sub = (0..toks.len())
            .find(|&k| toks[k] == Tok::Sym("(") && matches!(toks.get(k + 1), Some(Tok::Word(w)) if w == "select"));
        let sub = sub.map(|k| if k > 0 && linking(toks.get(k - 1)) { k - 1 } else { k });
        let at = |k: usize| self.starts.get(self.i + k).map(|s| *s as u32 + 1);
        (call.and_then(at), sub.and_then(at))
    }

    /// COPY FROM's WHERE condition `e`: no subquery, aggregate, window function or set-returning function, as
    /// Postgres's transformExpr refuses them there, each at where it is written (`places`: see `copy_where_places`;
    /// an aggregate's where the parser read it, the first from `aggs` in `agg_at`).
    fn copy_where_allowed(&self, e: &Expr, places: (Option<u32>, Option<u32>), aggs: usize) -> Result<()> {
        let (mut sub, mut agg, mut win, mut srf) = (false, false, false, false);
        walk(e, &mut |x| match x {
            Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) => sub = true,
            Expr::Agg(..) => agg = true,
            Expr::Window(_) => win = true,
            Expr::Call(c) if one_column_srf(c) => srf = true,
            _ => {}
        });
        let (call, subquery) = places;
        let refused = if sub {
            Some((subquery, UNSUPPORTED, "cannot use subquery in COPY FROM WHERE condition"))
        } else if agg {
            let at = self.agg_at.get(aggs).and_then(|a| self.starts.get(a.0)).map(|s| *s as u32 + 1).or(call);
            Some((at, "42803", "aggregate functions are not allowed in COPY FROM WHERE conditions"))
        } else if win {
            Some((call, "42P20", "window functions are not allowed in COPY FROM WHERE conditions"))
        } else if srf {
            Some((call, UNSUPPORTED, "set-returning functions are not allowed in COPY FROM WHERE conditions"))
        } else {
            None
        };
        match refused {
            Some((position, code, m)) => Err(SqlError {
                code,
                message: m.into(),
                info: Some(Box::new(Violation { position, ..Violation::default() })),
            }),
            None => Ok(()),
        }
    }

    /// A one-character option's text: a string, or E'\t'.
    fn copy_string(&mut self, what: &str) -> Result<String> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(s),
            Some(Tok::Word(w)) if w == "e" && matches!(self.peek(), Some(Tok::Str(_))) => {
                let Some(Tok::Str(s)) = self.next() else { unreachable!() };
                Ok(unescape_c(&s))
            }
            t => {
                self.put_back(t);
                self.fail(&format!("expected a string for {what}"))
            }
        }
    }

    /// `a, b` (the old syntax, no parentheses) or `(a, b)`, or `*`.
    fn copy_columns(&mut self, star: &mut bool) -> Result<Vec<String>> {
        if self.sym("*") {
            *star = true;
            return Ok(vec![]);
        }
        if self.sym("(") {
            return self.names();
        }
        let mut v = vec![self.name()?];
        while self.sym(",") {
            v.push(self.name()?);
        }
        Ok(v)
    }

    /// ENCODING 'name': the data's encoding. Data here is UTF8; SQL_ASCII (bytes taken as they are) reads it as it
    /// is too, any other encoding is refused.
    fn copy_encoding(&mut self) -> Result<()> {
        let e = self.copy_string("ENCODING")?;
        if !matches!(e.to_lowercase().replace(['-', '_'], "").as_str(), "utf8" | "unicode" | "sqlascii") {
            return Err(feature(format!("COPY encoding \"{e}\" isn't supported: data is UTF8")));
        }
        Ok(())
    }

    fn copy_options(&mut self, from: bool) -> Result<Format> {
        let (mut csv, mut binary, mut header) = (false, false, false);
        let (mut header_match, mut freeze, mut default) = (false, false, None);
        let (mut delim, mut null, mut quote, mut escape) = (None, None, None, None);
        let (mut force_quote, mut force_all, mut force_not_null, mut force_null) = (vec![], false, vec![], vec![]);
        let mut seen: Vec<String> = vec![];
        let mut no_star = false;
        self.kw("with");
        if self.sym("(") {
            loop {
                let at = self.i;
                let opt = match self.next() {
                    Some(Tok::Word(w) | Tok::Ident(w)) => w,
                    t => {
                        self.put_back(t);
                        return self.fail("expected a COPY option");
                    }
                };
                // an option named twice (at the second)
                if seen.contains(&opt) {
                    return self.err_at(at, "42601", "conflicting or redundant options");
                }
                seen.push(opt.clone());
                match opt.as_str() {
                    "format" => match self.next() {
                        Some(Tok::Word(w) | Tok::Ident(w) | Tok::Str(w)) if w == "csv" => csv = true,
                        Some(Tok::Word(w) | Tok::Ident(w) | Tok::Str(w)) if w == "text" => {
                            (csv, binary) = (false, false)
                        }
                        Some(Tok::Word(w) | Tok::Ident(w) | Tok::Str(w)) if w == "binary" => {
                            (csv, binary) = (false, true)
                        }
                        Some(Tok::Word(w) | Tok::Ident(w) | Tok::Str(w)) => {
                            return Err(invalid(format!("COPY format \"{w}\" not recognized")));
                        }
                        t => {
                            self.put_back(t);
                            return self.fail("expected text, csv or binary");
                        }
                    },
                    "header" => {
                        header = match self.peek() {
                            Some(Tok::Sym("," | ")")) => true,
                            _ => match self.next() {
                                Some(Tok::Word(w) | Tok::Str(w)) => match w.as_str() {
                                    "match" if !from => {
                                        return Err(feature("cannot use \"match\" with HEADER in COPY TO"));
                                    }
                                    "match" => {
                                        header_match = true;
                                        true
                                    }
                                    "true" | "on" => true,
                                    "false" | "off" => false,
                                    _ => return err(SYNTAX, "header requires a Boolean value or \"match\""),
                                },
                                Some(Tok::Num(n)) if n == "1" || n == "0" => n == "1",
                                _ => return err(SYNTAX, "header requires a Boolean value or \"match\""),
                            },
                        }
                    }
                    "delimiter" => delim = Some(self.copy_string("DELIMITER")?),
                    "null" => null = Some(self.copy_string("NULL")?),
                    "default" => default = Some(self.copy_string("DEFAULT")?),
                    "quote" => quote = Some(self.copy_string("QUOTE")?),
                    "escape" => escape = Some(self.copy_string("ESCAPE")?),
                    "force_quote" => force_quote = self.copy_columns(&mut force_all)?,
                    "force_not_null" => force_not_null = self.copy_columns(&mut no_star)?,
                    "force_null" => force_null = self.copy_columns(&mut no_star)?,
                    "encoding" => self.copy_encoding()?,
                    // (which columns a COPY FROM converts: all of them here)
                    "convert_selectively" => {
                        self.copy_columns(&mut no_star)?;
                    }
                    "freeze" => {
                        freeze = !(self.kw("false") || self.kw("off"));
                        let _ = self.kw("true") || self.kw("on");
                    }
                    o => return err(SYNTAX, format!("option \"{o}\" not recognized")),
                }
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
        } else {
            // the old syntax, which psql's \copy sends: [BINARY] [DELIMITER [AS] 'c'] [NULL [AS] 'n']
            // [CSV [HEADER] [QUOTE [AS] 'q'] [ESCAPE [AS] 'e'] [FORCE QUOTE cols | *] [FORCE NOT NULL cols]]
            loop {
                if self.kw("binary") {
                    binary = true;
                } else if self.kw("csv") {
                    csv = true;
                } else if self.kw("header") {
                    header = true;
                } else if self.kw("delimiter") {
                    self.kw("as");
                    delim = Some(self.copy_string("DELIMITER")?);
                } else if self.kw("null") {
                    self.kw("as");
                    null = Some(self.copy_string("NULL")?);
                } else if self.kw("quote") {
                    self.kw("as");
                    quote = Some(self.copy_string("QUOTE")?);
                } else if self.kw("escape") {
                    self.kw("as");
                    escape = Some(self.copy_string("ESCAPE")?);
                } else if self.kw("encoding") {
                    self.copy_encoding()?;
                } else if self.kw("freeze") {
                    freeze = true;
                } else if self.kw("force") {
                    if self.kw("quote") {
                        force_quote = self.copy_columns(&mut force_all)?;
                    } else {
                        self.expect_kw("not")?;
                        self.expect_kw("null")?;
                        force_not_null = self.copy_columns(&mut no_star)?;
                    }
                } else {
                    break;
                }
            }
        }

        // Postgres's checks, with its messages
        if binary && !from {
            return Err(feature("COPY TO's binary format isn't supported yet: use FORMAT text or csv"));
        }
        if binary {
            let set = [
                (csv, "CSV"),
                (delim.is_some(), "DELIMITER"),
                (null.is_some(), "NULL"),
                (default.is_some(), "DEFAULT"),
                (header, "HEADER"),
            ];
            if let Some((_, what)) = set.into_iter().find(|s| s.0) {
                return err(SYNTAX, format!("cannot specify {what} in BINARY mode"));
            }
        }
        let one = |s: &Option<String>, what: &str| -> Result<Option<u8>> {
            match s {
                None => Ok(None),
                Some(s) if s.len() == 1 => Ok(Some(s.as_bytes()[0])),
                Some(_) => Err(feature(format!("COPY {what} must be a single one-byte character"))),
            }
        };
        let delim = one(&delim, "delimiter")?.unwrap_or(if csv { b',' } else { b'\t' });
        let null = null.unwrap_or_else(|| if csv { String::new() } else { "\\N".into() });
        if delim == b'\n' || delim == b'\r' {
            return Err(invalid("COPY delimiter cannot be newline or carriage return"));
        }
        if null.contains(['\n', '\r']) {
            return Err(invalid("COPY null representation cannot use newline or carriage return"));
        }
        if default.as_ref().is_some_and(|d| d.contains(['\n', '\r'])) {
            return Err(invalid("COPY default representation cannot use newline or carriage return"));
        }
        if !csv && b"\\.abcdefghijklmnopqrstuvwxyz0123456789".contains(&delim) {
            return Err(invalid(format!("COPY delimiter cannot be \"{}\"", delim as char)));
        }
        if !csv && quote.is_some() {
            return Err(feature("COPY quote available only in CSV mode"));
        }
        if !csv && escape.is_some() {
            return Err(feature("COPY escape available only in CSV mode"));
        }
        let quote = one(&quote, "quote")?.unwrap_or(b'"');
        let escape = one(&escape, "escape")?.unwrap_or(quote);
        if csv && delim == quote {
            return Err(invalid("COPY delimiter and quote must be different"));
        }
        if null.as_bytes().contains(&delim) {
            return Err(invalid("COPY delimiter character must not appear in the NULL specification"));
        }
        if csv && null.as_bytes().contains(&quote) {
            return Err(invalid("CSV quote character must not appear in the NULL specification"));
        }
        if (!force_quote.is_empty() || force_all) && (!csv || from) {
            return Err(feature(if csv {
                "COPY force quote only available using COPY TO"
            } else {
                "COPY force quote available only in CSV mode"
            }));
        }
        if let Some(d) = &default {
            if !from {
                return Err(feature("COPY DEFAULT only available using COPY FROM"));
            }
            if d.as_bytes().contains(&delim) {
                return Err(feature("COPY delimiter must not appear in the DEFAULT specification"));
            }
            if csv && d.as_bytes().contains(&quote) {
                return Err(feature("CSV quote character must not appear in the DEFAULT specification"));
            }
            if *d == null {
                return Err(feature("NULL specification and DEFAULT specification cannot be the same"));
            }
        }
        for (f, name) in [(&force_not_null, "force not null"), (&force_null, "force null")] {
            if !f.is_empty() && !csv {
                return Err(feature(format!("COPY {name} available only in CSV mode")));
            }
            if !f.is_empty() && !from {
                return Err(feature(format!("COPY {name} only available using COPY FROM")));
            }
        }
        Ok(Format {
            csv,
            binary,
            header,
            header_match,
            freeze,
            default,
            delim,
            null,
            quote,
            escape,
            force_quote,
            force_all,
            force_not_null,
            force_null,
        })
    }
}

fn server_files() -> SqlError {
    SqlError {
        code: "42501",
        message: "COPY to or from a file or program on the server isn't allowed: use COPY ... FROM STDIN or \
                  TO STDOUT (psql's \\copy reads and writes files on the client)"
            .into(),
        info: None,
    }
}

/// Whether the server reads and writes its files for anyone at all (COPY to or from a file, file_fdw's
/// `filename`, lo_import and lo_export): only with CHRONOS_ALLOW_SERVER_COPY=1, and then only for the
/// roles Postgres lets. Never in a server that hands SQL to people (`serve --studio-api`, see `NO_SERVER_PATHS`),
/// whatever the variable says.
pub(super) fn server_files_allowed() -> bool {
    std::env::var("CHRONOS_ALLOW_SERVER_COPY").is_ok_and(|v| v == "1")
        && !super::NO_SERVER_PATHS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Postgres's 42501 for a COPY FROM (`from`) or TO a file by a role without the privileges of
/// pg_read_server_files (pg_write_server_files), with its DETAIL and HINT.
fn copy_file_denied(from: bool) -> SqlError {
    let (what, role) = if from { ("from", "pg_read_server_files") } else { ("to", "pg_write_server_files") };
    SqlError {
        code: "42501",
        message: format!("permission denied to COPY {what} a file"),
        info: Some(Box::new(Violation {
            detail: Some(format!("Only roles with privileges of the \"{role}\" role may COPY {what} a file.")),
            hint: Some("Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone.".into()),
            ..Violation::default()
        })),
    }
}

/// A COPY FROM (`from`) or TO a file on the server, as the running statement's role: Postgres's rule
/// first (a superuser, or a role with the privileges of pg_read_server_files or pg_write_server_files;
/// never an agent's session: see `roles::may_use_server_files`), then whether the server lets anyone.
pub(super) fn server_file(from: bool) -> Result<()> {
    let role = if from { roles::READ_SERVER_FILES } else { roles::WRITE_SERVER_FILES };
    if !roles::may_use_server_files(role) {
        return Err(copy_file_denied(from));
    }
    if !server_files_allowed() {
        return Err(server_files());
    }
    Ok(())
}

impl Session {
    /// `server_file` for a COPY this session runs over the Postgres port (as its own identity: no
    /// statement is running yet), checked before anything is read, written or run.
    fn may_copy_file(&self, db: &Db, from: bool) -> Result<()> {
        let role = if from { roles::READ_SERVER_FILES } else { roles::WRITE_SERVER_FILES };
        let (current, _) = self.who(db)?;
        if self.agent.is_some() || !current.has_privs_of(role) {
            return Err(copy_file_denied(from));
        }
        if !server_files_allowed() {
            return Err(server_files());
        }
        Ok(())
    }
}

/// C-style escapes in E'...' strings: \t, \n, \r, \\ and \'.
fn unescape_c(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(c) => out.push(c),
            None => out.push('\\'),
        }
    }
    out
}

impl Statement {
    /// For COPY ... FROM or TO a file on the server: the path.
    pub fn copy_file(&self) -> Option<&str> {
        match &self.0 {
            Stmt::Copy(c) => c.file.as_deref(),
            _ => None,
        }
    }

    /// For COPY: Some(true) for COPY ... FROM STDIN, Some(false) for COPY ... TO STDOUT.
    pub fn copy_direction(&self) -> Option<bool> {
        match &self.0 {
            Stmt::Copy(c) => Some(c.from),
            _ => None,
        }
    }
}

/// A COPY ... FROM STDIN under way: `feed` it the data as it comes, then `finish` (or `abort`).
pub struct CopyIn {
    table: String,
    names: Vec<String>,
    types: Vec<Type>,
    format: Format,
    /// per column: FORCE_NOT_NULL, FORCE_NULL
    not_null: Vec<bool>,
    force_null: Vec<bool>,
    buf: Vec<u8>,
    /// the rows read for the INSERT's next part (see `run`), and about how much they hold
    rows: Vec<Vec<Value>>,
    bytes: usize,
    /// about how much the part last handed out holds (see `Replay`)
    handed: usize,
    line: u64,
    count: u64,
    header: bool, // the header line is still to come
    ended: bool,  // saw the end marker, \.
    /// the record at the start of `buf`, as far as it was read before its data ran out
    partial: Option<Partial>,
    /// its statement_timeout and cancel requests, which a long COPY stops for
    stop: Option<Stop>,
    /// binary format: reads a field's bytes as its column's type (the wire's, see `decode_with`)
    decode: Option<Decode>,
    /// WHERE, with the columns it reads (the ones COPY names)
    filter: Option<(Expr, Vec<Col>)>,
    /// rows the filter left out
    skipped: u64,
    /// per column, what DEFAULT's marker gives: None, no default (an error); Some(None), one worked out per row
    /// (refused); Some(Some(v)), the constant
    defaults: Vec<Option<Option<Value>>>,
    /// the text record being read, as written (kept for an error's CONTEXT, as Postgres's names it), and the field
    /// of it being read as its column's value
    rec: Vec<u8>,
    field: Option<String>,
    /// a refusal Postgres makes once the COPY has begun (FREEZE's: see `Session::copy_in`), which `run` gives after
    /// dropping the client's data
    late: Option<SqlError>,
}

/// What COPY's DEFAULT marker gives column `c` (see `CopyIn::defaults`).
fn copy_default(c: &Col) -> Option<Option<Value>> {
    let per_row = c.default_sql.is_some() || c.auto.is_some() || c.identity.is_some() || c.seq.is_some();
    match &c.default {
        _ if per_row => Some(None),
        Some(v) => Some(Some(v.clone())),
        // (a domain's default, which the column takes when it has none of its own: not worked out here)
        None if c.domain.is_some() => Some(None),
        None => None,
    }
}

/// A binary value's bytes, as a column of this type.
pub type Decode = fn(&[u8], Type) -> Result<Value>;

/// A record read up to `at` (from its start): its fields so far.
#[derive(Default)]
struct Partial {
    fields: Vec<(Vec<u8>, bool)>,
    field: Vec<u8>,
    quoted: bool,
    in_quote: bool,
    at: usize,
}

/// A part of a COPY's rows, as the INSERT takes them (see `CopyIn::run`): at most this many, or
/// about this many bytes.
const PART_ROWS: usize = 16_384;
const PART_BYTES: usize = 32 << 20;

/// The most a record may take, as the most a value may (Postgres's 1 GB).
const MAX_RECORD: usize = (1 << 30) - 1;

impl Session {
    /// Starts a COPY ... FROM STDIN (`s`): the client then sends the data (see `CopyIn`).
    pub fn copy_in(&mut self, db: &Db, s: &Statement) -> Result<CopyIn> {
        let Stmt::Copy(c) = &s.0 else { return err(UNSUPPORTED, "not a COPY statement") };
        if !c.from {
            return err(UNSUPPORTED, "COPY ... TO STDOUT sends rows: see Session::copy_out");
        }
        if self.status() == b'E' {
            return err("25P02", "current transaction is aborted, commands ignored until end of transaction block");
        }
        if c.file.is_some() {
            self.may_copy_file(db, true)?;
        }
        let table = c.table.clone().unwrap_or_default();
        let t = self.with_temp(|| need_table(&db.view_tx(self.target())?, &table))?;
        // a READ ONLY transaction copies into a temporary table only: refused before the client sends any data, as
        // Postgres's DoCopy refuses it (failing the transaction)
        if self.read_only_now() && !self.temp_writes_only(s)? {
            self.abort();
            return err("25006", "cannot execute COPY FROM in a read-only transaction");
        }
        let names = match &c.cols {
            Some(n) => {
                no_generated(&t, n)?;
                n.clone()
            }
            None if t.schema => t.cols.iter().filter(|c| c.generated.is_none()).map(|c| c.name.clone()).collect(),
            None => {
                return err(
                    SYNTAX,
                    format!(
                        "table \"{table}\" has no schema: name the columns, e.g. COPY {table} (id, name) FROM STDIN"
                    ),
                );
            }
        };
        let types = names
            .iter()
            .map(|n| match (t.schema, col_index(&t.cols, n)) {
                (true, Ok(i)) => Ok(t.cols[i].ty),
                (true, Err(e)) if e.code == NO_COLUMN => {
                    err(NO_COLUMN, format!("column \"{n}\" of relation \"{}\" does not exist", rel_part(&table)))
                }
                (true, Err(e)) => Err(e),
                (false, _) => Ok(Type::Text),
            })
            .collect::<Result<Vec<_>>>()?;
        let listed = |l: &[String], what: &str| -> Result<Vec<bool>> {
            if let Some(n) = l.iter().find(|n| !names.contains(n)) {
                return err("42P10", format!("{what} column \"{n}\" not referenced by COPY"));
            }
            Ok(names.iter().map(|n| l.contains(n)).collect())
        };
        let not_null = listed(&c.format.force_not_null, "FORCE_NOT_NULL")?;
        let force_null = listed(&c.format.force_null, "FORCE_NULL")?;
        // INSERT on its columns, refused before the client sends any data (as Postgres does)
        if !self.with_temp(|| has_temp(&table)) {
            let insert = Stmt::Insert {
                table: table.clone(),
                alias: None,
                cols: Some(names.clone()),
                rows: vec![],
                query: None,
                conflict: None,
                returning: None,
                overriding: None,
                checks: None,
            };
            self.may_run(db, &insert)?;
            // a table whose policies would check each row: refused, as in Postgres
            let (current, _) = self.who(db)?;
            // (with the session's row_security off, it is refused as any statement it would limit is: 42501)
            let on = {
                let st = self.state.lock().unwrap();
                let set = st.local.get("row_security").or_else(|| st.settings.get("row_security"));
                !set.is_some_and(|s| super::rls::setting_off(s))
            };
            if super::rls::applies_as(&db.view_tx(self.target())?, &t.name, Some(&current), on)? {
                return Err(SqlError {
                    code: UNSUPPORTED,
                    message: "COPY FROM not supported with row-level security".into(),
                    info: Some(Box::new(Violation {
                        hint: Some("Use INSERT statements instead.".into()),
                        ..Violation::default()
                    })),
                });
            }
        }
        // FREEZE: not into a partitioned table, and only into a table this transaction made or truncated, at the
        // savepoint level it is at (its rows would otherwise show to transactions that shouldn't see them, in
        // Postgres). Refused once the COPY has begun, as Postgres's CopyFrom refuses it after the CopyInResponse: the
        // client sends its data, which is dropped, rather than reading it as statements
        let made_here = || {
            self.tx
                .as_ref()
                .is_some_and(|tx| tx.fresh.iter().any(|(n, depth)| *n == rel_part(&t.name) && *depth == tx.saves.len()))
        };
        let late = if !c.format.freeze {
            None
        } else if t.hier.part.is_some() {
            Some(feature("cannot perform COPY FREEZE on a partitioned table"))
        } else if !made_here() {
            Some(SqlError {
                code: "55000",
                message: "cannot perform COPY FREEZE because the table was not created or truncated in the current subtransaction".into(),
                info: None,
            })
        } else {
            None
        };
        // the defaults it may take, worked out before any data comes, as Postgres's are (a constant that doesn't fit
        // its column fails the COPY then): those of the columns it leaves out, and with DEFAULT those of all
        let mut defaults = vec![];
        if t.schema {
            for col in &t.cols {
                let named = names.contains(&col.name);
                if col.generated.is_some() || (named && c.format.default.is_none()) {
                    continue;
                }
                if let (Some(Value::Text(d)), Some((n, fixed))) = (&col.default, col.chars) {
                    fit_text(col.ty, d, n, fixed)?;
                }
            }
            defaults =
                names.iter().map(|n| t.cols.iter().find(|x| x.name == *n).map_or(Some(None), copy_default)).collect();
        }
        self.cancel.store(false, std::sync::atomic::Ordering::Relaxed); // as a statement starting does
        let agent = self.agent.as_deref().and_then(|a| db.agent(a).ok()); // its max_query_ms caps the timeout
        let ms = self.timeout(db, agent.as_ref());
        let stop = Stop {
            deadline: (ms > 0).then(|| std::time::Instant::now() + std::time::Duration::from_millis(ms)),
            cancel: self.cancel.clone(),
            gone: self.gone.clone(),
            ctx: None,
            mem: None,
            threads: agent.as_ref().map_or(0, |a| a.max_threads as usize),
        };
        let filter = c.filter.clone().map(|f| {
            let cols = t.cols.iter().filter(|x| names.contains(&x.name)).cloned().collect::<Vec<_>>();
            (f, cols)
        });
        // (a column it reads that the table hasn't: 42703 before any data comes, at the name, as Postgres places it)
        if let Some((f, _)) = &filter
            && let Err(mut e) = Binder::plain(&t.cols, None).bind(f)
        {
            if e.code == NO_COLUMN
                && let Some(name) = e.message.strip_prefix("column \"").and_then(|r| r.split('"').next())
            {
                let name = name.to_string();
                if pg_exact()
                    && let Some(at) = e.message.find("; did you mean")
                {
                    e.message.truncate(at);
                }
                let sql = self.sql.lock().unwrap_or_else(|p| p.into_inner()).clone();
                if let Ok((toks, starts)) = lex_at(&sql)
                    && let Some(w) = toks.iter().position(|t| *t == Tok::Word("where".into()))
                    && let Some(k) = (w..toks.len()).find(|&k| toks[k] == Tok::Word(name.to_lowercase()))
                {
                    let mut info = e.info.take().map(|b| *b).unwrap_or_default();
                    info.position = starts.get(k).map(|s| *s as u32 + 1);
                    e.info = Some(Box::new(info));
                }
            }
            return Err(e);
        }
        Ok(CopyIn {
            table,
            names,
            types,
            format: c.format.clone(),
            not_null,
            force_null,
            buf: vec![],
            rows: vec![],
            bytes: 0,
            handed: 0,
            line: 0,
            count: 0,
            header: c.format.header || c.format.binary, // binary's is its signature
            ended: false,
            partial: None,
            stop: Some(stop),
            decode: None,
            filter,
            skipped: 0,
            defaults,
            rec: vec![],
            field: None,
            late,
        })
    }

    /// Runs a COPY ... TO STDOUT (`s`): its column count, and its data, a row (or the header)
    /// per piece, as CopyData messages carry them.
    pub fn copy_out(&mut self, db: &Db, s: &Statement) -> Result<CopyOut> {
        let Stmt::Copy(c) = &s.0 else { return err(UNSUPPORTED, "not a COPY statement") };
        // TO a file: refused before its query (or its data-changing statement) runs
        if c.file.is_some() {
            self.may_copy_file(db, false)?;
        }
        // (a data-changing statement's RETURNING rows)
        if let Some(d) = &c.dml {
            let returns = matches!(
                &**d,
                Stmt::Insert { returning: Some(_), .. }
                    | Stmt::Update { returning: Some(_), .. }
                    | Stmt::Delete { returning: Some(_), .. }
            );
            rules::copy_check(db, self.target(), d, returns)?;
            if !returns {
                return Err(feature("COPY query must have a RETURNING clause"));
            }
            // FORCE_QUOTE's columns are checked before the statement runs: it has stored its rows by the time the
            // data is written, and a COPY that fails then would leave them there
            if !c.format.force_quote.is_empty()
                && let Ok(Some(q)) = super::returning_select(d)
            {
                let shape = Statement(Stmt::Select(Box::new(q)), PlanCache::default(), s.2);
                let cols = self.execute(db, &shape, &[], &[])?.columns.unwrap_or_default();
                force_quote_named(&c.format, &cols, true)?;
            }
            let mut out = self.execute(db, &Statement((**d).clone(), PlanCache::default(), s.2), &[], &[])?;
            self.show_regs(db, &mut out, &|_| true);
            return copy_data(c, out);
        }
        let q = match (&c.query, &c.table) {
            (Some(q), _) => (**q).clone(),
            (None, table) => {
                let table = table.clone().unwrap_or_default();
                let view = db.view_tx(self.target())?;
                if view.get(&format!("_sonos_views/{table}")).ok().flatten().is_some() {
                    return Err(SqlError {
                        code: "42809",
                        message: format!("cannot copy from view \"{}\"", rel_part(&table)),
                        info: Some(Box::new(Violation {
                            hint: Some("Try the COPY (SELECT ...) TO variant.".into()),
                            ..Violation::default()
                        })),
                    });
                }
                let t = self.with_temp(|| need_table(&db.view_tx(self.target())?, &table))?;
                if t.hier.part.is_some() {
                    return Err(SqlError {
                        code: "42809",
                        message: format!("cannot copy from partitioned table \"{}\"", rel_part(&table)),
                        info: Some(Box::new(Violation {
                            hint: Some("Try the COPY (SELECT ...) TO variant.".into()),
                            ..Violation::default()
                        })),
                    });
                }
                let items = match &c.cols {
                    Some(cols) => {
                        no_generated(&t, cols)?;
                        cols.iter().map(|n| Item::Expr(Expr::Col(None, n.clone()), None)).collect()
                    }
                    // a generated column is left out unless it is named, as Postgres does
                    None if t.cols.iter().any(|c| c.generated.is_some()) => t
                        .cols
                        .iter()
                        .filter(|c| c.generated.is_none())
                        .map(|c| Item::Expr(Expr::Col(None, c.name.clone()), None))
                        .collect(),
                    None => vec![Item::Star],
                };
                let from = Source {
                    table: table.clone(),
                    alias: table,
                    sub: None,
                    names: vec![],
                    rec: None,
                    working: false,
                    at: None,
                    lateral: false,
                    srf: None,
                    cte: 0,
                    // (only the table's own rows: a table that has children is not copied with them)
                    only: true,
                    hier: false,
                    policed: false,
                    bound: false,
                };
                Select {
                    set: vec![],
                    distinct: false,
                    distinct_on: vec![],
                    items,
                    from: Some(from),
                    joins: vec![],
                    filter: None,
                    group: vec![],
                    having: None,
                    order: vec![],
                    limit: None,
                    offset: None,
                    locking: vec![],
                }
            }
        };
        // COPY table TO: SELECT on it is checked before its row security is, as Postgres's DoCopy checks it (a
        // query's are checked after, as its executor does)
        if let (None, Some(table)) = (&c.query, &c.table)
            && !self.with_temp(|| has_temp(table))
        {
            self.may_run(db, &Stmt::Select(Box::new(q.clone())))?;
        }
        let mut out = self.execute(db, &Statement(Stmt::Select(Box::new(q)), PlanCache::default(), s.2), &[], &[])?;
        self.show_regs(db, &mut out, &|_| true);
        copy_data(c, out)
    }
}

/// A COPY TO's rows as the data it sends.
/// The error for a FORCE_QUOTE column the COPY doesn't give: of a query's (`query`), a column that doesn't exist,
/// as Postgres says; of a table's, one its column list leaves out.
fn force_quote_named(f: &Format, cols: &[(String, Type)], query: bool) -> Result<()> {
    match f.force_quote.iter().find(|n| !cols.iter().any(|(c, _)| c == *n)) {
        Some(n) if query => err(NO_COLUMN, format!("column \"{n}\" does not exist")),
        Some(n) => err("42P10", format!("FORCE_QUOTE column \"{n}\" not referenced by COPY")),
        None => Ok(()),
    }
}

fn copy_data(c: &Copy, out: Output) -> Result<CopyOut> {
    {
        let cols = out.columns.unwrap_or_default();
        let f = &c.format;
        let force: Vec<bool> = cols.iter().map(|(n, _)| f.force_all || f.force_quote.contains(n)).collect();
        force_quote_named(f, &cols, c.query.is_some() || c.dml.is_some())?;
        let mut data = Vec::with_capacity(out.rows.len() + 1);
        if f.header {
            let names: Vec<Option<String>> = cols.iter().map(|(n, _)| Some(n.clone())).collect();
            data.push(f.line(&names, &vec![false; cols.len()]));
        }
        for row in &out.rows {
            let texts: Vec<Option<String>> = row
                .iter()
                .map(|v| match v {
                    Value::Null => None,
                    Value::Bool(b) => Some(if *b { "t" } else { "f" }.into()),
                    v => Some(v.to_text()),
                })
                .collect();
            data.push(f.line(&texts, &force));
        }
        Ok(CopyOut { columns: cols.len(), tag: format!("COPY {}", out.rows.len()), data })
    }
}

/// What a COPY ... TO STDOUT sends: `data` in CopyData messages, then `tag`.
pub struct CopyOut {
    pub columns: usize,
    pub data: Vec<Vec<u8>>,
    pub tag: String,
}

impl Format {
    /// A row (None: NULL) as a line of text or CSV.
    fn line(&self, fields: &[Option<String>], force: &[bool]) -> Vec<u8> {
        let mut out = vec![];
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                out.push(self.delim);
            }
            let Some(s) = f else {
                out.extend_from_slice(self.null.as_bytes());
                continue;
            };
            if !self.csv {
                for &b in s.as_bytes() {
                    let esc = match b {
                        b'\\' => b'\\',
                        b'\n' => b'n',
                        b'\r' => b'r',
                        b'\t' => b't',
                        8 => b'b',
                        12 => b'f',
                        11 => b'v',
                        b if b == self.delim => b,
                        _ => {
                            out.push(b);
                            continue;
                        }
                    };
                    out.extend([b'\\', esc]);
                }
                continue;
            }
            let quoted = force[i]
                || *s == self.null
                || (fields.len() == 1 && s == "\\.")
                || s.bytes().any(|b| b == self.delim || b == self.quote || b == b'\n' || b == b'\r');
            if !quoted {
                out.extend_from_slice(s.as_bytes());
                continue;
            }
            out.push(self.quote);
            for &b in s.as_bytes() {
                if b == self.quote || b == self.escape {
                    out.push(self.escape);
                }
                out.push(b);
            }
            out.push(self.quote);
        }
        out.push(b'\n');
        out
    }
}

thread_local! {
    /// The COPY whose INSERT is running: the statement (its address), and its rows (see `Replay`).
    static REPLAY: std::cell::RefCell<Option<(usize, std::rc::Rc<std::cell::RefCell<Replay>>)>> =
        const { std::cell::RefCell::new(None) };
}

/// What gives a COPY's next part from the client: its rows and about how many bytes they hold; None at the end.
type NextPart = Box<dyn FnMut() -> Result<Option<(Vec<Vec<Value>>, usize)>>>;

/// A COPY's rows for the INSERT that stores them, which may run more than once: a statement whose
/// sequence values run out mid-way is run again from its start (see `RESERVE`), and the client
/// doesn't send its rows twice. So parts handed out are kept to be handed out again: the first
/// one always, and the ones after it up to `work_mem` (the first sequence value may be taken at
/// any row: a trigger that calls nextval for some rows only). Past that a run again is refused:
/// storing the rows that came after, and saying COPY n, would lose the ones before without a word.
struct Replay {
    /// the next part from the client, and about how much it holds
    next: NextPart,
    kept: Vec<Vec<Vec<Value>>>,
    bytes: usize,
    at: usize,   // parts of `kept` handed out in this run
    read: usize, // parts read from the client
}

impl Replay {
    fn reader(me: &std::rc::Rc<std::cell::RefCell<Replay>>) -> Copied {
        let me = me.clone();
        Box::new(move || me.borrow_mut().part())
    }

    fn part(&mut self) -> Result<Option<Vec<Vec<Value>>>> {
        if self.at < self.kept.len() {
            self.at += 1;
            return Ok(Some(self.kept[self.at - 1].clone()));
        }
        let Some((rows, bytes)) = (self.next)()? else { return Ok(None) };
        self.read += 1;
        // (kept only while every part so far is, up to CHRONOS_WORK_MEM: a run again needs them all, and the
        // first sequence value may be taken at any row)
        if self.kept.len() + 1 == self.read {
            if self.read == 1 || self.bytes + bytes <= spill::work_mem() {
                self.bytes += bytes;
                self.kept.push(rows.clone());
                self.at += 1;
            } else {
                (self.kept, self.bytes) = (vec![], 0);
            }
        }
        Ok(Some(rows))
    }
}

/// `s` is about to be run again (see `RESERVE`): if it is a COPY's INSERT, its rows start over.
pub(super) fn run_again(s: &Statement) -> Result<()> {
    let Some((_, replay)) = REPLAY.with_borrow(|r| r.clone().filter(|(at, _)| *at == std::ptr::from_ref(s) as usize))
    else {
        return Ok(());
    };
    let mut r = replay.borrow_mut();
    if r.kept.len() < r.read {
        return err(
            "54000",
            format!(
                "this COPY takes sequence values as it goes (in a trigger, a default or a function) and ran out \
                 of those reserved after more rows than it keeps to store again ({} MB, CHRONOS_WORK_MEM). \
                 Nothing was stored. Load it in smaller COPYs, or with INSERT ... SELECT",
                spill::work_mem() >> 20
            ),
        );
    }
    r.at = 0;
    drop(r);
    COPIED.set(Some(Replay::reader(&replay)));
    Ok(())
}

impl CopyIn {
    /// How many columns each row has.
    pub fn columns(&self) -> usize {
        self.names.len()
    }

    /// Whether the data comes in the binary format: it then needs `decode_with`.
    pub fn binary(&self) -> bool {
        self.format.binary
    }

    /// How binary values are read.
    pub fn decode_with(&mut self, f: Decode) {
        self.decode = Some(f);
    }

    /// Runs the COPY: one INSERT, which takes the rows a part at a time as `next` brings the
    /// data (None: its end), so they're never all held, nor written aside first. On an error
    /// the session's transaction, if any, is marked failed; the rest of the data is the
    /// caller's to skip.
    ///
    /// The INSERT holds the branch's write lock while the client sends, as a big INSERT does
    /// while it runs: statement_timeout and cancels are checked as data arrives, and the caller
    /// bounds a wait for data that doesn't come (pg.rs drops a client silent for COPY_IDLE).
    pub fn run(
        mut self,
        s: &mut Session,
        db: &Db,
        mut next: Box<dyn FnMut() -> Result<Option<Vec<u8>>>>,
    ) -> Result<Output> {
        // a refusal made once the COPY has begun: the client's data up to its CopyDone (or CopyFail) is read and
        // dropped first, as when a row fails partway, so the client gets the error, and one ReadyForQuery, after it
        if let Some(e) = self.late.take() {
            while let Ok(Some(_)) = next() {}
            s.abort();
            return Err(e);
        }
        let insert = Stmt::Insert {
            table: self.table.clone(),
            alias: None,
            cols: Some(self.names.clone()),
            rows: vec![], // they come from COPIED instead
            query: None,
            conflict: None,
            returning: None,
            overriding: None,
            checks: None,
        };
        let me = std::rc::Rc::new(std::cell::RefCell::new(self));
        let parts = me.clone();
        let mut eof = false;
        let replay = std::rc::Rc::new(std::cell::RefCell::new(Replay {
            next: Box::new(move || {
                let mut c = parts.borrow_mut();
                let part = c.part(&mut next, &mut eof)?;
                Ok(part.map(|rows| (rows, c.handed)))
            }),
            kept: vec![],
            bytes: 0,
            at: 0,
            read: 0,
        }));
        let insert = Statement(insert, PlanCache::default(), false);
        let was = REPLAY.replace(Some((std::ptr::from_ref(&insert) as usize, replay.clone())));
        COPIED.set(Some(Replay::reader(&replay)));
        let done = s.execute(db, &insert, &[], &[]);
        COPIED.take(); // untaken if the INSERT never ran
        REPLAY.set(was);
        match done {
            Ok(_) => Ok(Output::tag(format!("COPY {}", me.borrow().count))),
            Err(e) => {
                s.abort();
                Err(e)
            }
        }
    }

    /// The next part of the rows, read from as much data as it takes; None after the last.
    fn part(
        &mut self,
        next: &mut dyn FnMut() -> Result<Option<Vec<u8>>>,
        eof: &mut bool,
    ) -> Result<Option<Vec<Vec<Value>>>> {
        while !self.full() {
            // records already here first: a message can hold far more than a part
            if !self.buf.is_empty() && !self.ended {
                let was = (self.rows.len(), self.buf.len());
                let used = self.records(*eof)?;
                self.buf.drain(..used);
                if (self.rows.len(), self.buf.len()) != was {
                    continue;
                }
            }
            if *eof {
                break;
            }
            match next()? {
                Some(data) => self.feed(&data)?,
                None => *eof = true, // round again: what's left is the last record
            }
        }
        self.handed = std::mem::take(&mut self.bytes);
        Ok((!self.rows.is_empty()).then(|| std::mem::take(&mut self.rows)))
    }

    /// Takes the next piece of data: its whole records become rows.
    fn feed(&mut self, data: &[u8]) -> Result<()> {
        if self.ended {
            return Ok(()); // after \., as Postgres does: the rest is ignored
        }
        self.stopped()?;
        self.buf.extend_from_slice(data);
        let used = self.records(false)?;
        self.buf.drain(..used);
        // the record still to come, and what its fields so far hold besides their bytes
        let fields = self.partial.as_ref().map_or(0, |p| p.fields.len());
        if self.buf.len().saturating_add(fields.saturating_mul(std::mem::size_of::<(Vec<u8>, bool)>())) > MAX_RECORD {
            self.line += 1;
            let e = SqlError {
                code: "54000",
                message: format!("a record is more than {MAX_RECORD} bytes long"),
                info: None,
            };
            return Err(self.context(e, None));
        }
        Ok(())
    }

    /// Whether the next part is full: records stop being read until it's taken.
    fn full(&self) -> bool {
        self.rows.len() >= PART_ROWS || self.bytes >= PART_BYTES
    }

    /// A row read: held for the next part, unless WHERE leaves it out (an error working WHERE out fails the COPY).
    fn push(&mut self, row: Vec<Value>) -> Result<()> {
        if let Some((f, cols)) = &self.filter {
            // (the row in the order of the columns it names, which `cols` is put in)
            let named: Vec<Value> = cols
                .iter()
                .map(|c| self.names.iter().position(|n| *n == c.name).map_or(Value::Null, |i| row[i].clone()))
                .collect();
            let env = Env { cols, row: &named, params: &[], aggs: &[], sub: None };
            if truth(&eval(f, &env)?)? != Some(true) {
                self.skipped += 1;
                return Ok(());
            }
        }
        self.bytes += spill::size_of(&row);
        self.rows.push(row);
        self.count += 1;
        Ok(())
    }

    fn stopped(&self) -> Result<()> {
        self.stop.as_ref().map_or(Ok(()), Stop::check)
    }

    /// Error `e` reading line `line` (column `col` of it), with Postgres's CONTEXT: `COPY t, line 1: "the line"`, or
    /// `COPY t, line 1, column a: "the value"` (each cut to 100 characters, as Postgres's limit_printout_length).
    fn context(&self, e: SqlError, col: Option<usize>) -> SqlError {
        let cut = |b: &[u8]| {
            let t = String::from_utf8_lossy(b);
            match t.char_indices().nth(100) {
                Some((at, _)) => format!("{}...", &t[..at]),
                None => t.into_owned(),
            }
        };
        let table = rel_part(&self.table);
        let context = match col {
            Some(i) => match &self.field {
                Some(v) => {
                    format!("COPY {table}, line {}, column {}: \"{}\"", self.line, self.names[i], cut(v.as_bytes()))
                }
                None => format!("COPY {table}, line {}, column {}", self.line, self.names[i]),
            },
            None if self.format.binary => format!("COPY {table}, line {}", self.line),
            None => format!("COPY {table}, line {}: \"{}\"", self.line, cut(&self.rec)),
        };
        let mut info = e.info.map(|b| *b).unwrap_or_default();
        info.context = Some(context);
        SqlError { code: e.code, message: e.message, info: Some(Box::new(info)) }
    }

    /// Reads the complete records at the start of `buf` (and at the end of the data, `eof`, a
    /// last one without a newline); returns how many bytes they took.
    fn records(&mut self, eof: bool) -> Result<usize> {
        let buf = std::mem::take(&mut self.buf);
        let r = self.records_in(&buf, eof);
        self.buf = buf;
        r
    }

    fn records_in(&mut self, b: &[u8], eof: bool) -> Result<usize> {
        if self.format.binary {
            return self.binary_in(b, eof);
        }
        let f = &self.format;
        let (delim, quote, escape, csv) = (f.delim, f.quote, f.escape, f.csv);
        let mut at = 0;
        while at < b.len() && !self.ended && !self.full() {
            // one record: its fields as raw bytes, and whether each was quoted (CSV); where the last
            // call stopped reading it, if it did
            let Partial { mut fields, mut field, mut quoted, mut in_quote, at: from } =
                self.partial.take().unwrap_or_default();
            let mut i = at + from;
            // the rest of the record is still to come: taken up from here when it does
            macro_rules! wait {
                () => {{
                    self.partial = Some(Partial { fields, field, quoted, in_quote, at: i - at });
                    return Ok(at);
                }};
            }
            let rec = at;
            let end = loop {
                let Some(&c) = b.get(i) else {
                    if !eof {
                        wait!();
                    }
                    if in_quote {
                        self.line += 1;
                        return Err(self.context(
                            SqlError { code: "22P04", message: "unterminated CSV quoted field".into(), info: None },
                            None,
                        ));
                    }
                    break i;
                };
                if csv && in_quote {
                    if c == escape && i + 1 == b.len() && !eof {
                        wait!(); // an escape, or a closing quote: the next byte says
                    }
                    if c == escape && b.get(i + 1).is_some_and(|&n| n == quote || n == escape) {
                        field.push(b[i + 1]);
                        i += 2;
                    } else if c == quote {
                        in_quote = false;
                        i += 1;
                    } else {
                        field.push(c);
                        i += 1;
                    }
                    continue;
                }
                match c {
                    b'\n' => break i,
                    b'\\' if !csv => {
                        if i + 1 == b.len() && !eof {
                            wait!();
                        }
                        field.push(c);
                        field.extend(b.get(i + 1));
                        i += 2;
                    }
                    c if csv && c == quote => {
                        in_quote = true;
                        quoted = true;
                        i += 1;
                    }
                    c if c == delim => {
                        // more fields than columns: refused now, not after a newline that may not come
                        // (a header's are read and dropped)
                        // (HEADER MATCH counts a header's)
                        if fields.len() + 1 >= self.names.len() && !(self.header && self.format.header_match) {
                            if !self.header {
                                self.line += 1;
                                let to = b[rec..].iter().position(|&c| c == b'\n').map_or(b.len(), |n| rec + n);
                                self.rec.clear();
                                self.rec.extend_from_slice(b[rec..to].strip_suffix(b"\r").unwrap_or(&b[rec..to]));
                                let e = SqlError {
                                    code: "22P04",
                                    message: "extra data after last expected column".into(),
                                    info: None,
                                };
                                return Err(self.context(e, None));
                            }
                            field.clear();
                        } else {
                            fields.push((std::mem::take(&mut field), quoted));
                        }
                        quoted = false;
                        i += 1;
                    }
                    c => {
                        field.push(c);
                        i += 1;
                    }
                }
            };
            if field.last() == Some(&b'\r') && !quoted {
                field.pop(); // \r\n line ends
            }
            fields.push((field, quoted));
            at = (end + 1).min(b.len());
            self.line += 1;
            self.rec.clear();
            self.rec.extend_from_slice(b[rec..end].strip_suffix(b"\r").unwrap_or(&b[rec..end]));
            if self.line.is_multiple_of(4096) {
                self.stopped()?;
            }
            if fields.len() == 1 && !fields[0].1 && fields[0].0 == b"\\." {
                self.ended = true; // the end marker
                break;
            }
            if self.header {
                self.header = false;
                if self.format.header_match {
                    self.match_header(&fields, &b[rec..end])?;
                }
                continue;
            }
            if end == b.len() && fields.len() == 1 && fields[0].0.is_empty() && !fields[0].1 {
                break; // nothing after the last newline
            }
            self.row(fields)?;
        }
        Ok(at)
    }

    /// Binary data: a signature, flags and an extension area, then per row its field count and
    /// each field's length (-1: NULL) and bytes; a count of -1 ends it.
    fn binary_in(&mut self, b: &[u8], eof: bool) -> Result<usize> {
        let fail = |c: &mut Self, message: String| {
            c.line += 1;
            Err(c.context(SqlError { code: "22P04", message, info: None }, None))
        };
        let Some(decode) = self.decode else {
            return err(UNSUPPORTED, "binary COPY data needs the wire protocol's binary values");
        };
        if self.ended {
            return Ok(b.len()); // after the trailer: ignored
        }
        let int = |i: usize, n: usize| b.get(i..i + n).map(|w| w.iter().fold(0u32, |a, &x| a << 8 | x as u32));
        let mut at = 0;
        if self.header {
            const SIG: &[u8] = b"PGCOPY\n\xff\r\n\0";
            let (Some(flags), Some(ext)) = (int(11, 4), int(15, 4)) else {
                return if eof { fail(self, "COPY file signature not recognized".into()) } else { Ok(0) };
            };
            if &b[..11] != SIG {
                return fail(self, "COPY file signature not recognized".into());
            }
            if flags & 1 << 16 != 0 {
                return fail(self, "invalid COPY file header (WITH OIDS)".into());
            }
            if flags & 0xffff_0000 != 0 {
                return fail(self, "unrecognized critical flags in COPY file header".into());
            }
            at = 19usize.saturating_add(ext as usize);
            if at > b.len() {
                return if eof { fail(self, "invalid COPY file header (missing extension)".into()) } else { Ok(0) };
            }
            self.header = false;
        }
        let width = self.names.len();
        let mut spans = Vec::with_capacity(width);
        'rows: while !self.ended && !self.full() {
            let Some(n) = int(at, 2) else { break };
            let n = n as u16 as i16;
            if n == -1 {
                self.ended = true; // the trailer
                return Ok(b.len());
            }
            if n as usize != width {
                return fail(self, format!("row field count is {n}, expected {width}"));
            }
            spans.clear();
            let mut i = at + 2;
            for _ in 0..width {
                let Some(len) = int(i, 4) else { break 'rows };
                i += 4;
                let len = len as i32;
                if len == -1 {
                    spans.push(None);
                    continue;
                }
                if len < 0 {
                    return fail(self, "invalid field size".into());
                }
                let end = i + len as usize;
                if end > b.len() {
                    break 'rows;
                }
                spans.push(Some(i..end));
                i = end;
            }
            self.line += 1;
            if self.line.is_multiple_of(4096) {
                self.stopped()?;
            }
            let mut row = Vec::with_capacity(width);
            for (c, span) in spans.iter().enumerate() {
                row.push(match span {
                    None => Value::Null,
                    Some(r) => decode(&b[r.clone()], self.types[c]).map_err(|e| self.context(e, Some(c)))?,
                });
            }
            self.push(row)?;
            at = i;
        }
        if eof && at < b.len() && !self.full() {
            return fail(self, "unexpected EOF in COPY data".into());
        }
        Ok(at)
    }

    /// One record's fields: NULL or its column's value, checked as it's read.
    /// HEADER MATCH: the header line (`line`, its fields) names the columns COPY reads, in order, as Postgres
    /// checks it.
    fn match_header(&self, fields: &[(Vec<u8>, bool)], line: &[u8]) -> Result<()> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let context =
            format!("COPY {}, line {}: \"{}\"", rel_part(&self.table), self.line, String::from_utf8_lossy(line));
        let fail = |message: String| SqlError {
            code: "22P04",
            message,
            info: Some(Box::new(Violation { context: Some(context.clone()), ..Violation::default() })),
        };
        if fields.len() != self.names.len() {
            return Err(fail(format!(
                "wrong number of fields in header line: got {}, expected {}",
                fields.len(),
                self.names.len()
            )));
        }
        for (k, ((raw, quoted), name)) in fields.iter().zip(&self.names).enumerate() {
            if *raw == self.format.null.as_bytes() && !(self.format.csv && *quoted) {
                return Err(fail(format!(
                    "column name mismatch in header line field {}: got null value (\"{}\"), expected \"{name}\"",
                    k + 1,
                    self.format.null
                )));
            }
            let got = if self.format.csv { raw.clone() } else { unescape_text(raw) };
            let got = String::from_utf8_lossy(&got);
            if got != name.as_str() {
                return Err(fail(format!(
                    "column name mismatch in header line field {}: got \"{got}\", expected \"{name}\"",
                    k + 1
                )));
            }
        }
        Ok(())
    }

    fn row(&mut self, fields: Vec<(Vec<u8>, bool)>) -> Result<()> {
        if fields.len() > self.names.len() {
            return Err(self.context(
                SqlError { code: "22P04", message: "extra data after last expected column".into(), info: None },
                None,
            ));
        }
        // (a record short of fields reads the ones it has first, as Postgres's NextCopyFrom does: one that fails is
        // the error, before the missing one)
        let given = fields.len();
        let mut row = Vec::with_capacity(self.names.len());
        for (i, (raw, quoted)) in fields.into_iter().enumerate() {
            let is_null = raw == self.format.null.as_bytes();
            let null = if self.format.csv {
                is_null && if quoted { self.force_null[i] } else { !self.not_null[i] }
            } else {
                is_null
            };
            if null {
                row.push(Value::Null);
                continue;
            }
            // DEFAULT's marker, as written (unquoted): the column's default
            if let Some(d) = &self.format.default
                && raw == d.as_bytes()
                && !(self.format.csv && quoted)
                && let Some(def) = self.defaults.get(i)
            {
                match def {
                    Some(Some(v)) => {
                        let v = match coerce(v.clone(), self.types[i]) {
                            Ok(v) => v,
                            Err(e) => {
                                self.field = Some(String::from_utf8_lossy(&raw).into_owned());
                                return Err(self.context(e, Some(i)));
                            }
                        };
                        row.push(v);
                        continue;
                    }
                    Some(None) => {
                        let e = feature(format!(
                            "COPY DEFAULT for column \"{}\" isn't supported: its default is worked out for each row",
                            self.names[i]
                        ));
                        return Err(self.context(e, Some(i)));
                    }
                    None => {
                        let e = SqlError {
                            code: "22P04",
                            message: "unexpected default marker in COPY data".into(),
                            info: None,
                        };
                        let mut e = self.context(e, None);
                        if let Some(info) = e.info.as_mut() {
                            info.detail = Some(format!("Column \"{}\" has no default value.", self.names[i]));
                        }
                        return Err(e);
                    }
                }
            }
            let bytes = if self.format.csv { raw } else { unescape_text(&raw) };
            let text = String::from_utf8(bytes).map_err(|_| {
                self.field = None;
                self.context(
                    SqlError {
                        code: "22021",
                        message: "invalid byte sequence for encoding \"UTF8\"".into(),
                        info: None,
                    },
                    Some(i),
                )
            })?;
            // (the field's text, for the CONTEXT of an error: kept where reading it as its type may fail)
            let kept = (self.types[i] != Type::Text).then(|| text.clone());
            let v = match coerce(Value::Text(text), self.types[i]) {
                Ok(v) => v,
                Err(e) => {
                    self.field = kept;
                    return Err(self.context(e, Some(i)));
                }
            };
            row.push(v);
        }
        if given < self.names.len() {
            let message = format!("missing data for column \"{}\"", self.names[given]);
            return Err(self.context(SqlError { code: "22P04", message, info: None }, None));
        }
        self.push(row)
    }
}

/// A text-format field's backslash escapes: \b \f \n \r \t \v, \ and up to 3 octal digits,
/// \x and up to 2 hex digits; any other character after \ is itself.
fn unescape_text(raw: &[u8]) -> Vec<u8> {
    if !raw.contains(&b'\\') {
        return raw.to_vec();
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i];
        i += 1;
        if c != b'\\' || i == raw.len() {
            out.push(c);
            continue;
        }
        let n = raw[i];
        i += 1;
        out.push(match n {
            b'b' => 8,
            b'f' => 12,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 11,
            b'0'..=b'7' => {
                let mut v = (n - b'0') as u32;
                for _ in 0..2 {
                    match raw.get(i) {
                        Some(d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                v as u8
            }
            b'x' if raw.get(i).is_some_and(u8::is_ascii_hexdigit) => {
                let mut v = 0u32;
                for _ in 0..2 {
                    match raw.get(i).and_then(|d| (*d as char).to_digit(16)) {
                        Some(d) => {
                            v = v * 16 + d;
                            i += 1;
                        }
                        None => break,
                    }
                }
                v as u8
            }
            n => n,
        });
    }
    out
}

/// 42P10: COPY naming a generated column, which it can neither read nor write.
fn no_generated(t: &Table, names: &[String]) -> Result<()> {
    match names.iter().find(|n| t.cols.iter().any(|c| c.generated.is_some() && c.name == **n)) {
        Some(n) => Err(SqlError {
            code: "42P10",
            message: format!("column \"{n}\" is a generated column"),
            info: Some(Box::new(Violation {
                detail: Some("Generated columns cannot be used in COPY.".into()),
                ..Violation::default()
            })),
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(sql: &str) -> Format {
        match parse(sql).unwrap().pop().unwrap().0 {
            Stmt::Copy(c) => c.format,
            _ => panic!(),
        }
    }

    fn copy_in(sql: &str) -> CopyIn {
        let format = fmt(sql);
        CopyIn {
            table: "t".into(),
            names: vec!["a".into(), "b".into()],
            types: vec![Type::Text, Type::Text],
            not_null: vec![false; 2],
            force_null: vec![false; 2],
            header: format.header,
            format,
            buf: vec![],
            rows: vec![],
            bytes: 0,
            handed: 0,
            line: 0,
            count: 0,
            ended: false,
            partial: None,
            stop: None,
            decode: None,
            filter: None,
            skipped: 0,
            defaults: vec![],
            rec: vec![],
            field: None,
            late: None,
        }
    }

    fn rows(c: &CopyIn) -> Vec<Vec<Option<String>>> {
        c.rows.iter().map(|r| r.iter().map(|v| (*v != Value::Null).then(|| v.to_text())).collect()).collect()
    }

    /// Feeds the data in pieces of every size: the rows don't depend on where pieces break.
    fn read(sql: &str, data: &str) -> Vec<Vec<Option<String>>> {
        let mut want = None;
        for step in 1..=data.len().max(1) {
            let mut c = copy_in(sql);
            let mut at = 0;
            while at < data.len() {
                let piece = &data.as_bytes()[at..(at + step).min(data.len())];
                c.buf.extend_from_slice(piece);
                let used = c.records(false).unwrap();
                c.buf.drain(..used);
                at += step;
            }
            c.records(true).unwrap();
            let got = rows(&c);
            assert_eq!(want.get_or_insert_with(|| got.clone()), &got, "pieces of {step}");
        }
        want.unwrap()
    }

    #[test]
    fn text_and_csv_records() {
        let s = |x: &str| Some(x.to_string());
        let text = read("copy t from stdin", "1\tx\\ty\n\\N\t\\\\\\n\\101\\x41\r\n");
        assert_eq!(text, [vec![s("1"), s("x\ty")], vec![None, s("\\\nAA")]]);
        let csv = read("copy t from stdin csv header", "a,b\n1,\"x, \"\"y\"\"\nz\"\n,\"\"\n\\.\nignored\n");
        assert_eq!(csv, [vec![s("1"), s("x, \"y\"\nz")], vec![None, s("")]]);
        let semi =
            read("copy t from stdin with (format csv, delimiter ';', null 'NA', quote '''')", "NA;'a;b'\n'NA';x");
        assert_eq!(semi, [vec![None, s("a;b")], vec![s("NA"), s("x")]]);
        let esc = read("copy t from stdin (format csv, escape '\\')", "\"a\\\"b\",\"c\\\\\"\n");
        assert_eq!(esc, [vec![s("a\"b"), s("c\\")]]);
    }

    /// HEADER MATCH: the header names the columns, in order; DEFAULT's marker takes a column's default.
    #[test]
    fn header_match_and_default_marker() {
        let s = |x: &str| Some(x.to_string());
        assert_eq!(read("copy t from stdin (header match)", "a\tb\n1\t2\n"), [vec![s("1"), s("2")]]);
        assert_eq!(read("copy t from stdin (format csv, header match)", "a,b\n1,2\n"), [vec![s("1"), s("2")]]);
        let fails = |sql: &str, data: &str| {
            let mut c = copy_in(sql);
            c.buf.extend_from_slice(data.as_bytes());
            let e = c.records(true).unwrap_err();
            (e.code, e.message, e.info.and_then(|i| i.context))
        };
        assert_eq!(
            fails("copy t from stdin (header match)", "b\ta\n"),
            (
                "22P04",
                "column name mismatch in header line field 1: got \"b\", expected \"a\"".to_string(),
                Some("COPY t, line 1: \"b\ta\"".to_string())
            )
        );
        assert_eq!(
            fails("copy t from stdin (header match)", "a\t\\N\n").1,
            "column name mismatch in header line field 2: got null value (\"\\N\"), expected \"b\""
        );
        assert_eq!(
            fails("copy t from stdin (header match)", "a\tb\tc\n").1,
            "wrong number of fields in header line: got 3, expected 2"
        );
        let code = |sql: &str| parse(sql).err().map(|e| e.code);
        assert_eq!(code("copy t to stdout (header match)"), Some(UNSUPPORTED));
        assert_eq!(code("copy t to stdout (default 'd')"), Some(UNSUPPORTED));
        assert_eq!(code("copy t from stdin (default '\\N')"), Some(UNSUPPORTED));
        // the marker: a column's constant default, or (none) an error
        let mut c = copy_in("copy t from stdin (default 'D')");
        c.defaults = vec![Some(Some(Value::Text("x".into()))), None];
        c.buf.extend_from_slice(b"D\t1\n");
        let used = c.records(true).unwrap();
        c.buf.drain(..used);
        assert_eq!(rows(&c), [vec![s("x"), s("1")]]);
        c.buf.extend_from_slice(b"1\tD\n");
        assert_eq!(c.records(true).unwrap_err().code, "22P04");
    }

    #[test]
    fn binary_records() {
        let mut data = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\x02ab".to_vec(); // a 2-byte extension
        for (a, b) in [(Some("x"), None), (Some(""), Some("yz"))] {
            data.extend(2i16.to_be_bytes());
            for f in [a, b] {
                match f {
                    Some(f) => data.extend((f.len() as i32).to_be_bytes().into_iter().chain(f.bytes())),
                    None => data.extend((-1i32).to_be_bytes()),
                }
            }
        }
        data.extend((-1i16).to_be_bytes());
        data.extend(b"ignored");
        for step in 1..=data.len() {
            let mut c = copy_in("copy t from stdin (format binary)");
            c.header = true;
            c.decode_with(|b, _| Ok(Value::Text(String::from_utf8(b.to_vec()).unwrap())));
            for piece in data.chunks(step) {
                c.buf.extend_from_slice(piece);
                let used = c.records(false).unwrap();
                c.buf.drain(..used);
            }
            c.records(true).unwrap();
            let s = |x: &str| Some(x.to_string());
            assert_eq!(rows(&c), [vec![s("x"), None], vec![s(""), s("yz")]], "pieces of {step}");
        }
        let mut c = copy_in("copy t from stdin (format binary)");
        c.header = true;
        c.decode_with(|_, _| Ok(Value::Null));
        c.buf = data[..data.len() - 12].to_vec(); // cut inside the last row
        assert_eq!(c.records(true).unwrap_err().code, "22P04");
    }

    /// One message can hold far more rows than a part: they're read a part at a time, the rest
    /// waiting in the buffer, so a 64 MB message of tiny rows isn't held as rows all at once.
    #[test]
    fn a_big_message_goes_in_part_by_part() {
        let text: String = (0..40_000).map(|i| format!("{i}\tx\n")).collect();
        let mut binary = b"PGCOPY\n\xff\r\n\0".to_vec();
        binary.extend([0u8; 8]);
        for i in 0..40_000u32 {
            binary.extend(2i16.to_be_bytes());
            for f in [i.to_string(), "x".into()] {
                binary.extend((f.len() as i32).to_be_bytes().into_iter().chain(f.into_bytes()));
            }
        }
        binary.extend((-1i16).to_be_bytes());
        for (sql, data) in [("copy t from stdin", text.into_bytes()), ("copy t from stdin (format binary)", binary)] {
            let mut c = copy_in(sql);
            c.header = sql.contains("binary");
            c.decode_with(|b, _| Ok(Value::Text(String::from_utf8(b.to_vec()).unwrap())));
            let mut data = Some(data);
            let mut next = move || Ok(data.take());
            let (mut eof, mut sizes) = (false, vec![]);
            while let Some(p) = c.part(&mut next, &mut eof).unwrap() {
                sizes.push(p.len());
            }
            assert_eq!(sizes, [16_384, 16_384, 7_232], "{sql}");
        }
    }

    #[test]
    fn lines_out() {
        let f = fmt("copy t to stdout csv");
        let row = [Some("a,b".to_string()), None, Some(String::new()), Some("q\"".into()), Some("plain".into())];
        assert_eq!(f.line(&row, &[false; 5]), b"\"a,b\",,\"\",\"q\"\"\",plain\n");
        let f = fmt("copy t to stdout");
        let row = [Some("a\tb\\c\nd".to_string()), None];
        assert_eq!(f.line(&row, &[false; 2]), b"a\\tb\\\\c\\nd\t\\N\n");
    }

    #[test]
    fn options_as_postgres_checks_them() {
        let code = |sql: &str| parse(sql).err().map(|e| e.code);
        // (a file is parsed; who may read it, and whether the server lets anyone, is asked as it runs: `server_file`)
        assert_eq!(code("copy t from '/etc/passwd'"), None);
        assert_eq!(code("copy t to program 'ls'"), Some("42501"));
        assert_eq!(code("copy t from stdin (format binary)"), None);
        assert_eq!(code("copy t to stdout (format binary)"), Some(UNSUPPORTED));
        assert_eq!(code("copy t from stdin (format binary, header)"), Some(SYNTAX));
        assert_eq!(code("copy t from stdin (delimiter ',,')"), Some(UNSUPPORTED));
        assert_eq!(code("copy t from stdin (quote '\"')"), Some(UNSUPPORTED));
        assert_eq!(code("copy t from stdin (format csv, delimiter '\"')"), Some("22023"));
        assert_eq!(code("copy t to stdout (format csv, force_quote *)"), None);
        assert_eq!(code("copy t from stdin (format csv, force_quote *)"), Some(UNSUPPORTED));
        assert_eq!(code("copy (select 1) to stdout with csv header force quote *"), None);
    }
}
