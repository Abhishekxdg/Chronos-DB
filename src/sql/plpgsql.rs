//! PL/pgSQL, the part of it Chronos runs (docs/sql.md lists it): blocks with DECLARE, `:=`,
//! IF, CASE, LOOP, WHILE, FOR over numbers and over a query's rows, FOREACH [SLICE], labels
//! (`<<name>>`, `name.var`), EXIT and CONTINUE [label], RETURN
//! (NEXT, QUERY), PERFORM, SELECT / INSERT / UPDATE / DELETE [RETURNING] ... INTO [STRICT],
//! RAISE, ASSERT, GET DIAGNOSTICS ... ROW_COUNT, FOUND, ALIAS FOR, cursors (bound, with arguments,
//! refcursors, OPEN / FETCH / MOVE / CLOSE, FOR over one; kept as the session's portals, so a
//! refcursor a function returns can be FETCHed after), and in trigger functions NEW, OLD and the
//! TG_ variables. An error leaving a function says where in it, in its CONTEXT (`PL/pgSQL function
//! f(integer) line 3 at RAISE`), as Postgres's do. Anything else is refused when the function is
//! made (0A000, naming it), never guessed at when it runs.
//!
//! Expressions and statements are SQL, parsed by SQL's parser. Variables are columns of a row
//! the expressions run over (a record's fields named `rec.field`); a statement gets them as its
//! parameters, `$1` for the first variable, so it plans and runs like any other.

use super::functions::{Event, Function, Ret, Returned, Trigger, Volatility};
use super::*;
use std::rc::Rc;

/// A trigger firing: what its function sees as NEW, OLD and TG_OP, TG_TABLE_NAME, ...
pub(super) struct Trig<'t> {
    pub(super) table: &'t Table,
    pub(super) trigger: &'t Trigger,
    pub(super) event: Event,
    pub(super) old: Option<&'t [Value]>,
    pub(super) new: Option<&'t [Value]>,
}

/// A parsed body: `[<<label>>] [DECLARE ...] BEGIN ... [EXCEPTION WHEN ... THEN ...] END [label]`.
pub(super) struct Block {
    decls: Vec<Decl>,
    body: Vec<S>,
    handlers: Vec<Handler>,
    /// the function's own block: the records its expressions use whole (`to_jsonb(new)`), see `Frame::whole`
    whole: Vec<String>,
    /// the line of its BEGIN in the function's body, from 1 (an error's CONTEXT says it)
    line: u32,
    /// its label, which `EXIT label` leaves it by
    label: Option<String>,
    /// the function's own block: the declarations that hide a variable of the same name further out, in order (what
    /// plpgsql.extra_warnings = 'shadowed_variables' warns of, see `created`)
    shadows: Vec<String>,
    /// the function's own block: `#variable_conflict` and `#print_strict_params`, which win over the settings
    conflict: Option<Conflict>,
    strict_params: Option<bool>,
}

/// How a name that is both a variable and a column of a statement's tables is read (plpgsql.variable_conflict).
#[derive(Clone, Copy, PartialEq)]
enum Conflict {
    /// 42702, the default
    Error,
    UseVariable,
    UseColumn,
}

/// `WHEN cond [OR cond ...] THEN body`: codes caught (`None`: OTHERS), and what runs then.
struct Handler {
    conds: Vec<Option<String>>,
    body: Vec<S>,
}

impl Handler {
    /// Whether it catches an error with SQLSTATE `code`, as Postgres decides: the code itself,
    /// or its class for a class's code (`22000` catches `22012`); OTHERS catches everything but
    /// a cancel or statement timeout (57014) and a failed ASSERT (P0004), which only their own
    /// names catch. Nothing catches the statement having to stop: its client gone (08006, 08003)
    /// or it past its agent's memory limit (53200), which it would only run into again. Nor does
    /// OTHERS catch an agent's limit (53400: writes per minute, worlds): a stop-now signal a
    /// retry loop would only spin on until its timeout.
    fn catches(&self, code: &str) -> bool {
        if matches!(code, "08006" | "08003" | "53200") {
            return false;
        }
        self.conds.iter().any(|c| match c {
            None => code != CANCELED && code != "P0004" && code != "53400",
            Some(c) => c == code || (c.ends_with("000") && code.get(..2) == c.get(..2)),
        })
    }
}

struct Decl {
    name: String,
    /// None: RECORD
    ty: Option<(Type, Option<(u32, i32)>)>,
    not_null: bool,
    default: Option<E>,
    /// where it is declared (the line an error in its default names)
    line: u32,
}

/// A cursor's declaration, `name [[NO] SCROLL] CURSOR [(arg type, ...)] FOR query`: what OPEN and FOR
/// over it run.
#[derive(Clone)]
struct Cur {
    name: String,
    query: Stmt,
    /// its arguments: variables its query reads, set by OPEN's values
    params: Vec<(String, Type)>,
    /// Some(false): NO SCROLL
    scroll: Option<bool>,
    /// its query as written (pg_cursors)
    text: String,
}

/// An expression; `query`: it has subqueries, so it runs as a query with the variables as its
/// parameters (others read the variables directly). `sel` is it as `SELECT expression`, which
/// runs bound, as SQL runs: for enums, which compare by their labels' places, and regprocs, which are their
/// functions' names where text is wanted (`bound`: it names an enum type or a regproc, or reads a parameter by
/// number, `$1`, which is then the parameter whatever names hide it, with its declared type; see `Frame::eval`).
struct E {
    e: Expr,
    query: bool,
    bound: bool,
    sel: Box<Stmt>,
}

enum Target {
    Record(String),
    Vars(Vec<String>),
}

/// Where FETCH and MOVE go in a cursor's rows.
enum Go {
    Next,
    Prior,
    First,
    Last,
    Abs(E),
    Rel(E),
    /// FORWARD [n | ALL] and BACKWARD [n | ALL] (`back`): n rows, 1 without it, None for ALL
    Many {
        back: bool,
        n: Option<E>,
        all: bool,
    },
}

enum S {
    /// A statement where it is in the body: its line, and what an error's CONTEXT calls it there (`at RAISE`)
    At(u32, &'static str, Box<S>),
    /// OPEN cursor [(args)] [[NO] SCROLL] [FOR query | FOR EXECUTE text [USING ...]]: a bound cursor's query is its
    /// declaration's, its arguments' values (by parameter, in order) set as variables it reads
    Open {
        var: String,
        query: Option<Box<Stmt>>,
        dynamic: Option<(E, Vec<E>)>,
        args: Vec<(String, Type, E)>,
        scroll: Option<bool>,
        /// the variable is CONSTANT: it can't be given a portal's name
        constant: bool,
        text: String,
    },
    /// FETCH [direction] cursor INTO targets, and MOVE (no targets)
    Fetch {
        var: String,
        go: Go,
        into: Option<Target>,
    },
    Close(String),
    /// FOR rec IN cursor [(args)] LOOP, `rec` a record the loop itself declares
    ForCursor {
        rec: String,
        var: String,
        cur: Box<Cur>,
        args: Vec<(String, Type, E)>,
        body: Vec<S>,
    },
    /// `<<label>>` and the loop it labels, which `EXIT label` leaves and `CONTINUE label` goes round again
    Labeled(String, Box<S>),
    /// FOREACH var SLICE n IN ARRAY expr: the array's n-dimensional slices in turn (`var` None when the target isn't
    /// one scalar variable, which is an error when it runs, as Postgres's)
    ForEach {
        var: Option<String>,
        slice: usize,
        array: E,
        body: Vec<S>,
    },
    /// RETURN QUERY of a SELECT INTO (its text): Postgres's error when it runs
    NoTuples(String),
    Assign(String, E),
    If(Vec<(E, Vec<S>)>, Vec<S>),
    Case(Option<E>, Vec<(Vec<E>, Vec<S>)>, Option<Vec<S>>),
    Loop(Vec<S>),
    While(E, Vec<S>),
    ForInt {
        var: String,
        reverse: bool,
        lo: E,
        hi: E,
        by: Option<E>,
        body: Vec<S>,
    },
    /// FOR targets IN query LOOP (`text`: the query as written, which an error's CONTEXT shows; empty for a
    /// FOREACH, which has none)
    ForQuery {
        target: Target,
        query: Box<Stmt>,
        body: Vec<S>,
        text: String,
    },
    /// EXIT and CONTINUE [label] [WHEN cond]
    Exit(Option<String>, Option<E>),
    Continue(Option<String>, Option<E>),
    Return(Option<E>),
    ReturnRecord(String),
    ReturnNext(Option<E>),
    ReturnNextRecord(String),
    ReturnQuery(Box<Stmt>, String),
    ReturnQueryExecute(E, Vec<E>),
    ForExecute {
        target: Target,
        query: E,
        using: Vec<E>,
        body: Vec<S>,
    },
    /// PERFORM (`perform`), or a SELECT, INSERT, UPDATE or DELETE [... INTO [STRICT] targets]
    Sql {
        stmt: Box<Stmt>,
        into: Option<(Target, bool)>,
        perform: bool,
        /// the statement as Postgres shows it in an error's CONTEXT: as written, its INTO blanked out, PERFORM
        /// as SELECT
        text: String,
    },
    Raise {
        level: &'static str,
        cond: Option<(&'static str, String)>,
        fmt: Option<String>,
        args: Vec<E>,
        opts: Vec<(String, E)>,
    },
    Assert(E, Option<E>),
    /// GET [CURRENT | STACKED] DIAGNOSTICS: whether STACKED, and (variable, item)s
    Diag(bool, Vec<(String, String)>),
    /// RAISE without parameters: the error being handled, again
    Reraise,
    /// EXECUTE (dynamic SQL): the statement's text, INTO targets, USING values ($1, ...)
    Execute {
        query: E,
        into: Option<(Target, bool)>,
        using: Vec<E>,
    },
    Block(Block),
    Null,
    /// COMMIT [AND [NO] CHAIN], in a procedure or a DO block
    Commit,
}

/// What an error's CONTEXT says a statement is, as Postgres's plpgsql_stmt_typename names it (`first`: the word it
/// begins with, for those read as others: FOREACH as a FOR loop, DDL as EXECUTE, MOVE as FETCH).
fn what(s: &S, first: &str) -> &'static str {
    match s {
        S::At(_, w, _) => w,
        S::Labeled(_, s) => what(s, first),
        S::ForEach { .. } => "at FOREACH over array",
        S::NoTuples(_) => "at RETURN QUERY",
        S::Assign(..) => "at assignment",
        S::If(..) => "at IF",
        S::Case(..) => "at CASE",
        S::Loop(_) => "at LOOP",
        S::While(..) => "at WHILE",
        S::ForInt { .. } => "at FOR with integer loop variable",
        S::ForQuery { .. } if first == "foreach" => "at FOREACH over array",
        S::ForQuery { .. } => "at FOR over SELECT rows",
        S::ForCursor { .. } => "at FOR over cursor",
        S::ForExecute { .. } => "at FOR over EXECUTE statement",
        S::Exit(..) => "at EXIT",
        S::Continue(..) => "at CONTINUE",
        S::Return(_) | S::ReturnRecord(_) => "at RETURN",
        S::ReturnNext(_) | S::ReturnNextRecord(_) => "at RETURN NEXT",
        S::ReturnQuery(..) | S::ReturnQueryExecute(..) => "at RETURN QUERY",
        S::Sql { perform: true, .. } => "at PERFORM",
        S::Sql { .. } => "at SQL statement",
        S::Execute { .. } if first != "execute" => "at SQL statement",
        S::Execute { .. } => "at EXECUTE",
        S::Raise { .. } | S::Reraise => "at RAISE",
        S::Assert(..) => "at ASSERT",
        S::Diag(true, _) => "at GET STACKED DIAGNOSTICS",
        S::Diag(false, _) => "at GET DIAGNOSTICS",
        S::Open { .. } => "at OPEN",
        S::Fetch { into: None, .. } if first == "move" => "at MOVE",
        S::Fetch { .. } => "at FETCH",
        S::Close(_) => "at CLOSE",
        S::Block(_) => "at statement block",
        S::Null => "at NULL",
        S::Commit => "at COMMIT",
    }
}

/// Postgres's names for error conditions RAISE and USING ERRCODE take (the ones Chronos raises
/// itself, and PL/pgSQL's own).
const CONDITIONS: &[(&str, &str)] = &[
    ("raise_exception", "P0001"),
    ("no_data_found", "P0002"),
    ("too_many_rows", "P0003"),
    ("assert_failure", "P0004"),
    ("data_exception", "22000"),
    ("numeric_value_out_of_range", "22003"),
    ("null_value_not_allowed", "22004"),
    ("division_by_zero", "22012"),
    ("invalid_parameter_value", "22023"),
    ("invalid_text_representation", "22P02"),
    ("integrity_constraint_violation", "23000"),
    ("not_null_violation", "23502"),
    ("foreign_key_violation", "23503"),
    ("unique_violation", "23505"),
    ("check_violation", "23514"),
    ("serialization_failure", "40001"),
    ("insufficient_privilege", "42501"),
    ("undefined_column", "42703"),
    ("undefined_table", "42P01"),
    ("undefined_function", "42883"),
    ("object_not_in_prerequisite_state", "55000"),
    ("query_canceled", "57014"),
    ("feature_not_supported", "0A000"),
    ("case_not_found", "20000"),
    ("plpgsql_error", "P0000"),
    ("cardinality_violation", "21000"),
    ("string_data_right_truncation", "22001"),
    ("invalid_datetime_format", "22007"),
    ("datetime_field_overflow", "22008"),
    ("invalid_time_zone_displacement_value", "22009"),
    ("invalid_escape_character", "22019"),
    ("invalid_argument_for_logarithm", "2201E"),
    ("invalid_argument_for_power_function", "2201F"),
    ("invalid_row_count_in_limit_clause", "2201W"),
    ("invalid_row_count_in_result_offset_clause", "2201X"),
    ("invalid_regular_expression", "2201B"),
    ("interval_field_overflow", "22015"),
    ("array_subscript_error", "2202E"),
    ("floating_point_exception", "22P01"),
    ("invalid_binary_representation", "22P03"),
    ("invalid_json_text", "22032"),
    ("restrict_violation", "23001"),
    ("exclusion_violation", "23P01"),
    ("invalid_transaction_state", "25000"),
    ("read_only_sql_transaction", "25006"),
    ("in_failed_sql_transaction", "25P02"),
    ("invalid_authorization_specification", "28000"),
    ("dependent_objects_still_exist", "2BP01"),
    ("invalid_schema_name", "3F000"),
    ("transaction_rollback", "40000"),
    ("deadlock_detected", "40P01"),
    ("syntax_error_or_access_rule_violation", "42000"),
    ("syntax_error", "42601"),
    ("grouping_error", "42803"),
    ("datatype_mismatch", "42804"),
    ("wrong_object_type", "42809"),
    ("undefined_parameter", "42P02"),
    ("undefined_object", "42704"),
    ("duplicate_column", "42701"),
    ("duplicate_function", "42723"),
    ("duplicate_table", "42P07"),
    ("duplicate_object", "42710"),
    ("ambiguous_column", "42702"),
    ("ambiguous_function", "42725"),
    ("invalid_column_reference", "42P10"),
    ("invalid_function_definition", "42P13"),
    ("program_limit_exceeded", "54000"),
    ("statement_too_complex", "54001"),
    ("insufficient_resources", "53000"),
    ("out_of_memory", "53200"),
    ("too_many_connections", "53300"),
    ("lock_not_available", "55P03"),
    ("operator_intervention", "57000"),
    ("connection_exception", "08000"),
    ("connection_failure", "08006"),
    ("triggered_action_exception", "09000"),
    ("sql_routine_exception", "2F000"),
    ("function_executed_no_return_statement", "2F005"),
    ("invalid_cursor_state", "24000"),
    ("internal_error", "XX000"),
    ("data_corrupted", "XX001"),
];

/// Every condition name Postgres has (errcodes.txt), with its SQLSTATE.
fn named_condition(name: &str) -> Option<&'static str> {
    if let Some((_, c)) = CONDITIONS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Some(c);
    }
    include_str!("errcodes.tsv").lines().find_map(|l| {
        let (n, c) = l.split_once('\t')?;
        n.eq_ignore_ascii_case(name).then_some(c)
    })
}

/// A condition's SQLSTATE by its name (as WHEN and RAISE name them).
fn condition(name: &str) -> Result<&'static str> {
    match named_condition(name) {
        Some(c) => Ok(c),
        None => err("42704", format!("unrecognized exception condition \"{name}\"")),
    }
}

/// GET [CURRENT] DIAGNOSTICS's items.
const CURRENT: [&str; 3] = ["row_count", "pg_context", "pg_routine_oid"];

/// GET STACKED DIAGNOSTICS's items.
const STACKED: [&str; 10] = [
    "returned_sqlstate",
    "message_text",
    "pg_exception_detail",
    "pg_exception_hint",
    "pg_exception_context",
    "column_name",
    "constraint_name",
    "pg_datatype_name",
    "table_name",
    "schema_name",
];

/// Where a running function is, as its CONTEXT line says: the statement's line and what it is (`at RAISE`).
type At = Rc<std::cell::Cell<Option<(u32, &'static str)>>>;

thread_local! {
    /// The last RAISE's error, with the DETAIL and HINT errors don't carry, for a handler that
    /// catches it (GET STACKED DIAGNOSTICS) or raises it again.
    static RAISED: std::cell::RefCell<Option<Caught>> = const { std::cell::RefCell::new(None) };
    /// The PL/pgSQL functions running on this thread, the innermost last, each with where it is now: what GET
    /// DIAGNOSTICS ... PG_CONTEXT lists (see `pg_context`).
    static CALLS: std::cell::RefCell<Vec<(Arc<Function>, At)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The call stack as Postgres's PG_CONTEXT gives it: each running PL/pgSQL function's CONTEXT line, the innermost
/// first.
fn pg_context() -> String {
    CALLS.with_borrow(|c| c.iter().rev().map(|(f, at)| context_line(f, at.get())).collect::<Vec<_>>().join("\n"))
}

/// A function as an error's CONTEXT names it, `f(integer,text)` (a DO block's `inline_code_block`).
fn signature(f: &Function) -> String {
    if f.name == "inline_code_block" {
        return f.name.clone();
    }
    let types: Vec<String> = f.args.iter().map(|(_, t)| t.pg_name().into_owned()).collect();
    format!("{}({})", f.name, types.join(","))
}

/// A function's CONTEXT line where it is, as Postgres's plpgsql_exec_error_callback writes it.
fn context_line(f: &Function, at: Option<(u32, &'static str)>) -> String {
    match at {
        Some((line, what)) => format!("PL/pgSQL function {} line {line} {what}", signature(f)),
        None => format!("PL/pgSQL function {}", signature(f)),
    }
}

/// An error being handled: what GET STACKED DIAGNOSTICS, SQLSTATE, SQLERRM and RAISE; see.
#[derive(Clone)]
struct Caught {
    e: SqlError,
    detail: String,
    hint: String,
    /// RAISE ... USING DATATYPE's, which no error carries
    datatype: String,
    /// the CONTEXT line of the function that caught it, at the statement it failed in: what Postgres collected
    /// when it was raised, which RAISE; raises it with again (None: it has it already)
    here: Option<String>,
}

impl Caught {
    fn of(e: SqlError) -> Caught {
        let info = e.info.as_deref();
        let (mut detail, mut hint) = (
            info.and_then(|i| i.detail.clone()).unwrap_or_default(),
            info.and_then(|i| i.hint.clone()).unwrap_or_default(),
        );
        let mut datatype = String::new();
        // (the same error with the CONTEXT functions it came out of gave it since)
        if let Some(c) = RAISED.take()
            && c.e.code == e.code
            && c.e.message == e.message
        {
            if detail.is_empty() {
                detail = c.detail;
            }
            if hint.is_empty() {
                hint = c.hint;
            }
            datatype = c.datatype;
        }
        Caught { e, detail, hint, datatype, here: None }
    }

    /// GET STACKED DIAGNOSTICS's COLUMN_NAME, CONSTRAINT_NAME, TABLE_NAME or SCHEMA_NAME: what the error says.
    fn field(&self, item: &str) -> String {
        let Some(i) = self.e.info.as_deref() else { return String::new() };
        let f = match item {
            "column_name" => &i.column,
            "constraint_name" => &i.constraint,
            "table_name" => &i.table,
            _ => &i.schema,
        };
        f.clone().unwrap_or_default()
    }

    /// Fails with it, DETAIL and HINT kept for a handler further out.
    fn raise<T>(self) -> Result<T> {
        let e = self.e.clone();
        RAISED.set(Some(self));
        Err(e)
    }
}

/// A condition name or SQLSTATE as the `'static` code errors carry.
fn sqlstate(s: &str) -> Result<&'static str> {
    static SEEN: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(Default::default);
    if let Some(c) = named_condition(s) {
        return Ok(c);
    }
    let c = s.to_ascii_uppercase();
    if c.len() != 5 || !c.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase()) || c == "00000" {
        return err("42704", format!("unrecognized exception condition \"{s}\""));
    }
    let mut seen = SEEN.lock().unwrap();
    if let Some(c) = seen.get(c.as_str()) {
        return Ok(c);
    }
    if seen.len() >= 1000 {
        return Ok("P0001"); // ponytail: a bounded set of codes kept for ever; past it, the generic one
    }
    let c: &'static str = Box::leak(c.into_boxed_str());
    seen.insert(c);
    Ok(c)
}

// ---------------------------------------------------------------- parsing

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Scalar,
    Const,
    Record,
}

/// A level of names: a block's variables (or a FOR loop's own, or an exception handler's), and a label's.
struct Scope {
    vars: Vec<(String, Kind)>,
    /// `<<name>>` of a block or a loop (the function's own level is labeled with the function's name)
    label: Option<String>,
    /// it labels a loop, which CONTINUE may name
    is_loop: bool,
    /// a FOR loop's own variables, which the loop's label (one level out) names: `<<l>> for i in ...` and `l.i`
    loop_vars: bool,
}

impl Scope {
    fn of(vars: Vec<(String, Kind)>) -> Scope {
        Scope { vars, label: None, is_loop: false, loop_vars: false }
    }
}

/// `<<l>> ... END l`: an end label must be the block's or loop's own.
fn end_label(start: &Option<String>, end: &str) -> Result<()> {
    match start {
        None => err(SYNTAX, format!("end label \"{end}\" specified for unlabeled block")),
        Some(s) if s != end => err(SYNTAX, format!("end label \"{end}\" differs from block's label \"{s}\"")),
        Some(_) => Ok(()),
    }
}

/// Variable `name` as it is named while a declaration further in hides it: `name\u{1}k`, k the declarations of the
/// name hidden before it (see `Frame::declare`). Its own name again when it is no longer hidden.
fn hidden_as(name: &str, k: usize) -> String {
    format!("{name}\u{1}{k}")
}

/// A variable's name as written, without what hiding it added (see `hidden_as`).
fn shown(n: &str) -> String {
    let mut out = String::with_capacity(n.len());
    let mut chars = n.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1}' && chars.peek().is_some_and(char::is_ascii_digit) {
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

struct P<'f> {
    p: Parser,
    f: &'f Function,
    scopes: Vec<Scope>,
    loops: usize,
    /// the label read before the block or loop being parsed, which it takes
    pending_block: Option<String>,
    pending_loop: Option<String>,
    /// declarations that hide another variable of their name (see `Block::shadows`)
    shadows: Vec<String>,
    /// the text of the statement `sql_tokens` last read as written, its INTO kept
    raw: String,
    /// the scalar variables declared so far with their types, for `name%TYPE`
    #[allow(clippy::type_complexity)]
    typed: Vec<(String, Type, Option<(u32, i32)>)>,
    /// the cursors declared with their queries (`c CURSOR FOR SELECT ...`), which OPEN c and FOR r IN c run
    cursors: Vec<Cur>,
    /// the records expressions use whole
    whole: std::cell::RefCell<Vec<String>>,
    /// where each token of `p` starts in the body, in characters (kept with the tokens as they are spliced)
    pos: Vec<usize>,
    /// the body's characters, and where its lines start (for a statement's line and text)
    src: Vec<char>,
    lines: Vec<usize>,
    /// the text of the statement `sql_tokens` last read, as an error's CONTEXT shows it
    text: String,
}

/// The type of `table.column` (or `schema.table.column`), with a numeric's precision and scale: what `%TYPE` names.
pub(super) fn column_type(parts: &[String]) -> Result<(Type, Option<(u32, i32)>)> {
    let [table @ .., column] = parts else { return err(SYNTAX, "expected table.column%TYPE") };
    if table.is_empty() {
        return err("42704", format!("type \"{column}\" does not exist"));
    }
    let name = match table {
        [s, t] if s != "public" => format!("{s}.{t}"),
        [.., t] => find_named(t, super::Kind::Rel),
        [] => unreachable!(),
    };
    let found = with_path_view(|v| -> Result<(Type, Option<(u32, i32)>)> {
        let t = need_table(v, &name)?;
        let c = &t.cols[col_index(&t.cols, column)?];
        Ok((c.ty, c.numeric))
    });
    found.unwrap_or_else(|| err(UNSUPPORTED, "%TYPE needs a database to look the column up in"))
}

/// Parses a LANGUAGE plpgsql body.
pub(super) fn parse(f: &Function) -> Result<Block> {
    let mut first = vec![];
    for (i, (n, _)) in f.args.iter().enumerate() {
        first.push((n.clone().unwrap_or_else(|| format!("${}", i + 1)), Kind::Scalar));
    }
    first.push(("found".into(), Kind::Scalar));
    if let Ret::Table(cols) = &f.ret {
        // (an INOUT parameter is one variable, declared with the arguments: see `run`)
        let inout = |n: &String| f.outs && f.args.iter().any(|a| a.0.as_ref() == Some(n));
        first.extend(cols.iter().filter(|(n, _)| !inout(n)).map(|(n, _)| (n.clone(), Kind::Scalar)));
    }
    if f.ret == Ret::Trigger {
        first.extend(["new", "old"].map(|n| (n.to_string(), Kind::Record)));
        for n in TG_VARS {
            first.push((n.to_string(), Kind::Scalar));
        }
    }
    let typed = f.args.iter().filter_map(|(n, t)| n.clone().map(|n| (n, *t, None))).collect();
    let (toks, pos) = lex_at(&f.body)?;
    let src: Vec<char> = f.body.chars().collect();
    let lines = std::iter::once(0).chain((0..src.len()).filter(|&i| src[i] == '\n').map(|i| i + 1)).collect();
    // (the function's own level is labeled with its name: `f.param` is its parameter, whatever hides it)
    let own =
        Scope { vars: first, label: Some(super::rel_part(&f.name).to_string()), is_loop: false, loop_vars: false };
    let mut p = P {
        p: Parser::new(toks),
        f,
        scopes: vec![own],
        loops: 0,
        pending_block: None,
        pending_loop: None,
        shadows: vec![],
        raw: String::new(),
        typed,
        cursors: vec![],
        whole: Default::default(),
        pos,
        src,
        lines,
        text: String::new(),
    };
    // `#variable_conflict ...`, `#print_strict_params on|off` and `#option dump` before the block
    let (mut conflict, mut strict_params) = (None, None);
    while p.p.sym("#") {
        let option = match p.p.next() {
            Some(Tok::Word(w)) => w,
            _ => return p.p.fail("expected an option after #"),
        };
        let value = match p.p.next() {
            Some(Tok::Word(w)) => w,
            _ => return p.p.fail(&format!("expected a value for #{option}")),
        };
        match (option.as_str(), value.as_str()) {
            ("variable_conflict", "error") => conflict = Some(Conflict::Error),
            ("variable_conflict", "use_variable") => conflict = Some(Conflict::UseVariable),
            ("variable_conflict", "use_column") => conflict = Some(Conflict::UseColumn),
            ("print_strict_params", "on") => strict_params = Some(true),
            ("print_strict_params", "off") => strict_params = Some(false),
            ("option", "dump") => {} // (the compiled function written to the server's log)
            _ => return err(SYNTAX, format!("syntax error at or near \"{value}\"")),
        }
    }
    if p.p.peek() == Some(&Tok::Sym("<<")) {
        p.pending_block = Some(p.label()?);
    }
    if !(p.p.is_kw("declare") || p.p.is_kw("begin")) {
        return p.p.fail("expected DECLARE or BEGIN: a PL/pgSQL body is a block");
    }
    let mut b = p.block()?;
    p.p.sym(";");
    if p.p.peek().is_some() {
        return p.p.fail("expected the end of the function's body after END");
    }
    b.whole = p.whole.take();
    b.shadows = std::mem::take(&mut p.shadows);
    (b.conflict, b.strict_params) = (conflict, strict_params);
    Ok(b)
}

/// What plpgsql.extra_warnings and plpgsql.extra_errors say of a function being made, or a DO block about to run, as
/// Postgres's compiler says it: each declaration that hides a variable of its name is a WARNING, or with extra_errors
/// the error (42712).
pub(super) fn created(b: &Block) -> Result<()> {
    if b.shadows.is_empty() {
        return Ok(());
    }
    let (warn, error) = (Checks::read("plpgsql.extra_warnings"), Checks::read("plpgsql.extra_errors"));
    if !(warn.shadowed || error.shadowed) {
        return Ok(());
    }
    for name in &b.shadows {
        let message = format!("variable \"{name}\" shadows a previously defined variable");
        if error.shadowed {
            return err("42712", message);
        }
        super::notice_raised("WARNING", "42712", message, None, None);
    }
    Ok(())
}

/// The checks plpgsql.extra_warnings or plpgsql.extra_errors turns on: `none`, `all`, or a list of them.
#[derive(Clone, Copy, Default)]
struct Checks {
    shadowed: bool,
    too_many_rows: bool,
    multi: bool,
}

impl Checks {
    fn read(setting: &str) -> Checks {
        let v = super::session_setting(setting).unwrap_or_default().to_ascii_lowercase();
        let mut c = Checks::default();
        for w in v.split(',').map(str::trim) {
            match w {
                "all" => c = Checks { shadowed: true, too_many_rows: true, multi: true },
                "shadowed_variables" => c.shadowed = true,
                "too_many_rows" => c.too_many_rows = true,
                "strict_multi_assignment" => c.multi = true,
                _ => {}
            }
        }
        c
    }
}

/// What the session's plpgsql settings say for a call, read when first wanted (Postgres reads them when it compiles
/// the function): the extra checks, how a name that is a variable and a column too is read, whether STRICT's errors
/// list the statement's variables, and whether ASSERT is checked.
struct Opts {
    warn: Checks,
    error: Checks,
    conflict: Conflict,
    strict_params: bool,
    asserts: bool,
}

impl Opts {
    fn read(b: (Option<Conflict>, Option<bool>)) -> Opts {
        let on = |n: &str, default: bool| {
            super::session_setting(n).and_then(|v| super::settings::parse_bool(&v)).unwrap_or(default)
        };
        let conflict = b.0.unwrap_or_else(|| {
            match super::session_setting("plpgsql.variable_conflict").unwrap_or_default().to_ascii_lowercase().as_str()
            {
                "use_variable" => Conflict::UseVariable,
                "use_column" => Conflict::UseColumn,
                _ => Conflict::Error,
            }
        });
        Opts {
            warn: Checks::read("plpgsql.extra_warnings"),
            error: Checks::read("plpgsql.extra_errors"),
            conflict,
            strict_params: b.1.unwrap_or_else(|| on("plpgsql.print_strict_params", false)),
            asserts: on("plpgsql.check_asserts", true),
        }
    }
}

/// What follows `*name` in the names of the variables that say whether record `name` is null as a row is, and
/// whether it is not null (see `P::fix`).
const NULL_ROW: &str = "\u{1}null";
const NOT_NULL_ROW: &str = "\u{1}notnull";
/// What follows `*name` in the name of the variable holding record `name`'s row as text, `(1,"a b",)` (see `P::fix`).
const ROW_TEXT: &str = "\u{1}row";

/// Whether `e` gives a record used whole (`new`, `old`: see `P::fix`), or the one of several that a COALESCE or a CASE
/// picks: what a trigger function may return besides NULL.
fn row_valued(e: &Expr) -> bool {
    let row_or_null = |x: &Expr| row_valued(x) || matches!(x, Expr::Lit(Value::Null));
    match e {
        Expr::Col(None, n) => n.starts_with('*') && n.ends_with(ROW_TEXT),
        Expr::Func(Func::Coalesce, args) => args.iter().any(row_valued) && args.iter().all(row_or_null),
        Expr::Case(_, whens, other) => {
            let results = whens.iter().map(|(_, r)| r).chain(other.iter().map(|o| &**o));
            let mut any = false;
            let all = results.fold(true, |all, r| {
                any |= row_valued(r);
                all && row_or_null(r)
            });
            any && all
        }
        _ => false,
    }
}

const TG_VARS: [&str; 10] = [
    "tg_relid",
    "tg_name",
    "tg_when",
    "tg_level",
    "tg_op",
    "tg_table_name",
    "tg_relname",
    "tg_table_schema",
    "tg_nargs",
    "tg_argv",
];

impl P<'_> {
    /// Where token `i` starts in the body, in characters.
    fn at_char(&self, i: usize) -> usize {
        self.pos.get(i).copied().unwrap_or(self.src.len()).min(self.src.len())
    }

    /// The line of the body token `i` is on, from 1, as Postgres numbers a function's lines.
    fn line(&self, i: usize) -> u32 {
        let at = self.at_char(i);
        self.lines.partition_point(|&l| l <= at).max(1) as u32
    }

    /// The body's text from token `a` up to token `b`, trailing space dropped.
    fn source(&self, a: usize, b: usize) -> String {
        let (a, b) = (self.at_char(a), self.at_char(b));
        let s: String = self.src[a.min(b)..b].iter().collect();
        s.trim_end().to_string()
    }

    /// `self.p.t.splice(range, toks)`, the tokens' places kept beside them (the new ones where the first replaced
    /// one was).
    fn splice(&mut self, range: std::ops::Range<usize>, toks: Vec<Tok>) {
        let at = self.at_char(range.start);
        let n = toks.len();
        self.p.t.splice(range.clone(), toks);
        if range.start <= self.pos.len() {
            let end = range.end.min(self.pos.len());
            self.pos.splice(range.start..end, std::iter::repeat_n(at, n));
        }
    }

    fn kind(&self, n: &str) -> Option<Kind> {
        // (a hidden variable a label names, `x\u{1}0`, is of its name's kind)
        let n = n.split('\u{1}').next().unwrap_or(n);
        self.scopes.iter().rev().flat_map(|s| s.vars.iter().rev()).find(|(x, _)| x == n).map(|x| x.1)
    }

    /// Notes a declaration of `name` that hides a variable further out (see `Block::shadows`).
    fn shadow(&mut self, name: &str) {
        let here = self.scopes.last().is_some_and(|s| s.vars.iter().any(|(n, _)| n == name));
        if !here && self.kind(name).is_some() {
            self.shadows.push(name.to_string());
        }
    }

    /// `<<name>>`.
    fn label(&mut self) -> Result<String> {
        self.p.expect_sym("<<")?;
        let l = match self.p.peek() {
            Some(Tok::Word(w) | Tok::Ident(w)) => w.clone(),
            _ => return self.p.fail("expected a label"),
        };
        self.p.i += 1;
        self.p.expect_sym(">>")?;
        Ok(l)
    }

    /// The variable `label.x` names: `x` of the block or loop labeled so (the function's parameters by the function's
    /// name), as it is named where it is now (see `hidden_as`). None if the label names no such level, or a variable
    /// further in is called `label` (then it is a record's field).
    fn labeled(&self, label: &str, x: &str) -> Option<String> {
        let d = self.scopes.iter().rposition(|s| s.label.as_deref() == Some(label))?;
        if self.scopes[d + 1..].iter().any(|s| s.vars.iter().any(|(n, _)| n == label)) {
            return None;
        }
        let has = |s: &Scope| s.vars.iter().any(|(n, _)| n == x);
        let at = if has(&self.scopes[d]) {
            d
        } else if self.scopes[d].is_loop && self.scopes.get(d + 1).is_some_and(|s| s.loop_vars && has(s)) {
            d + 1
        } else {
            return None;
        };
        // hidden by one further in: named by how many of the name it came after
        if self.scopes[at + 1..].iter().any(has) {
            let before: usize = self.scopes[..at].iter().map(|s| s.vars.iter().filter(|(n, _)| n == x).count()).sum();
            return Some(hidden_as(x, before));
        }
        Some(x.to_string())
    }

    /// Label-qualified variables from here to the end of the statement (`;`; with `words`, a THEN or LOOP too), each
    /// made the one name it stands for (see `labeled`): `blk.x`, `f.param` and `blk.rec.field` (as `rec.field` of that
    /// record). In a SQL statement (`sql`) a label that is also written alone there may be a table's name, and is left
    /// for the statement to read.
    fn qualify(&mut self, words: bool, sql: bool) {
        let (mut k, mut depth) = (self.p.i, 0i32);
        while k + 2 < self.p.t.len() {
            match &self.p.t[k] {
                Tok::Sym("(" | "[") => depth += 1,
                Tok::Sym(")" | "]") => depth -= 1,
                Tok::Sym(";") => return,
                Tok::Word(w) if words && depth == 0 && (w == "then" || w == "loop") => return,
                _ => {}
            }
            let label = match &self.p.t[k] {
                Tok::Word(w) | Tok::Ident(w) => w.clone(),
                _ => {
                    k += 1;
                    continue;
                }
            };
            let x = match self.p.t.get(k + 2) {
                Some(Tok::Word(x) | Tok::Ident(x)) if self.p.t[k + 1] == Tok::Sym(".") => x.clone(),
                _ => {
                    k += 1;
                    continue;
                }
            };
            let after_dot = k > 0 && self.p.t[k - 1] == Tok::Sym(".");
            let name = if after_dot { None } else { self.labeled(&label, &x) };
            if let Some(name) = name {
                let alone = sql && {
                    let end = (k..self.p.t.len()).find(|&j| self.p.t[j] == Tok::Sym(";")).unwrap_or(self.p.t.len());
                    (self.p.i..end).any(|j| {
                        j != k
                            && matches!(&self.p.t[j], Tok::Word(w) | Tok::Ident(w) if *w == label)
                            && self.p.t.get(j + 1) != Some(&Tok::Sym("."))
                    })
                };
                if !alone {
                    self.splice(k..k + 3, vec![Tok::Ident(name)]);
                }
            }
            k += 1;
        }
    }

    fn semi(&mut self) -> Result<()> {
        self.p.expect_sym(";")
    }

    fn word(&self) -> Option<&str> {
        match self.p.peek() {
            Some(Tok::Word(w)) => Some(w),
            _ => None,
        }
    }

    /// Record fields as variables name them (`new.id`), and TG_ARGV counted from 0.
    fn fix(&self, e: &mut Expr) -> Result<()> {
        // `rec IS NULL` and `rec IS NOT NULL` as a row is: null when every field is, not null when none is (and a
        // record that holds no row is null). Each is a variable of its own beside `*name` (see `sync_whole`)
        if let Expr::IsNull(x, not) = e
            && let Expr::Col(None, n) = &**x
        {
            let r = n.strip_prefix('*').unwrap_or(n);
            if self.kind(r) == Some(Kind::Record) {
                let mut whole = self.whole.borrow_mut();
                if !whole.iter().any(|w| w == r) {
                    whole.push(r.to_string());
                }
                *e = Expr::Col(None, format!("*{r}{}", if *not { NOT_NULL_ROW } else { NULL_ROW }));
                return Ok(());
            }
        }
        // a record made JSON (`to_jsonb(new)`, `hstore(new)`) or compared whole (`new IS DISTINCT FROM old`): the
        // variable `*name`, which holds its fields as a JSON object, or NULL while it holds no row (see
        // `Frame::whole`). (Its arguments are looked at next, and the name `*name` is no record's.)
        match e {
            Expr::Func(
                Func::Scalar(
                    funcs::Scalar::ToJson
                    | funcs::Scalar::JsonBuildObject
                    | funcs::Scalar::JsonBuildArray
                    | funcs::Scalar::JsonFn(_)
                    | funcs::Scalar::JsonCtor
                    | funcs::Scalar::Hstore(_),
                )
                | Func::Distinct(_),
                args,
            ) => args.iter_mut().for_each(|a| self.as_json(a)),
            Expr::Bin(a, Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge, b) => {
                self.as_json(a);
                self.as_json(b);
            }
            _ => {}
        }
        match e {
            // `rec.*`: its fields, which the statement is given when it is made for them (see `Frame::stars`)
            Expr::Col(Some(q), f) if f == "*" && self.kind(q) == Some(Kind::Record) => {}
            Expr::Col(Some(q), f) if self.kind(q) == Some(Kind::Record) => *e = Expr::Col(None, format!("{q}.{f}")),
            // a record used whole anywhere else (`RAISE NOTICE '%', new`, `new::text`, `format('%s', r)`): the
            // variable `*name` + ROW_TEXT, its row's text as Postgres writes a composite, `(3,30)`
            Expr::Col(None, n) if self.kind(n) == Some(Kind::Record) => {
                let mut whole = self.whole.borrow_mut();
                if !whole.contains(n) {
                    whole.push(n.clone());
                }
                *e = Expr::Col(None, format!("*{n}{ROW_TEXT}"));
            }
            Expr::Func(Func::Scalar(funcs::Scalar::ArrayAt), args)
                if self.f.ret == Ret::Trigger && matches!(&args[0], Expr::Col(None, n) if n == "tg_argv") =>
            {
                args[1] = bin(args[1].clone(), Op::Add, Expr::Lit(Value::Int(1)));
            }
            _ => {}
        }
        Ok(())
    }

    /// A record named alone (`new`) as its fields' JSON object, `*name` (see `fix`).
    fn as_json(&self, x: &mut Expr) {
        if let Expr::Col(None, n) = x
            && self.kind(n) == Some(Kind::Record)
        {
            let mut whole = self.whole.borrow_mut();
            if !whole.contains(n) {
                whole.push(n.clone());
            }
            *x = Expr::Col(None, format!("*{n}"));
        }
    }

    fn expr(&mut self) -> Result<E> {
        self.qualify(true, false);
        self.select_from();
        self.bare_expr()
    }

    /// An expression with FROM ... after it (`IF count(*) = 0 FROM t WHERE ... THEN`): PL/pgSQL runs an expression
    /// as `SELECT expression`, so the rest is that query's, and it is read as the scalar subquery it is. (Up to the
    /// THEN, LOOP or `;` that ends it; `IS DISTINCT FROM` is no query's FROM.)
    fn select_from(&mut self) {
        let (mut depth, mut from) = (0i32, None);
        let start = self.p.i;
        for k in start..self.p.t.len() {
            match &self.p.t[k] {
                Tok::Sym("(" | "[") => depth += 1,
                Tok::Sym(")" | "]") => {
                    depth -= 1;
                    if depth < 0 {
                        return;
                    }
                }
                Tok::Word(w)
                    if depth == 0
                        && w == "from"
                        && k > start
                        && from.is_none()
                        && !matches!(&self.p.t[k - 1], Tok::Word(p) if p == "distinct") =>
                {
                    from = Some(k);
                }
                Tok::Word(w) if depth == 0 && (w == "then" || w == "loop") => return self.as_select(from, k),
                Tok::Sym(";") if depth == 0 => return self.as_select(from, k),
                _ => {}
            }
        }
    }

    /// The tokens from here to `end` made `(SELECT ...)`, if they have a FROM (see `select_from`).
    fn as_select(&mut self, from: Option<usize>, end: usize) {
        if from.is_none() {
            return;
        }
        let at = self.p.i;
        self.splice(end..end, vec![Tok::Sym(")")]);
        self.splice(at..at, vec![Tok::Sym("("), Tok::Word("select".into())]);
    }

    /// `expr`, but the expression ends at a FROM (a FETCH's count: `FETCH RELATIVE -2 FROM c`).
    fn bare_expr(&mut self) -> Result<E> {
        let mut e = self.p.expr()?;
        functions::visit_expr(&mut e, &mut |x| self.fix(x), &mut |_| {})?;
        Ok(self.wrap(e))
    }

    /// A FETCH or MOVE count: it ends at its FROM or IN (`move relative 2 in c` is no `2 IN (...)`).
    fn count_expr(&mut self) -> Result<E> {
        let (mut j, mut depth) = (self.p.i, 0i32);
        while let Some(t) = self.p.t.get(j) {
            match t {
                Tok::Sym("(") => depth += 1,
                Tok::Sym(")") => depth -= 1,
                Tok::Sym(";") if depth == 0 => break,
                Tok::Word(w) if depth == 0 && (w == "from" || w == "in") => break,
                _ => {}
            }
            j += 1;
        }
        let tail = self.p.t.split_off(j);
        let parsed = self.p.expr();
        self.p.t.extend(tail);
        let mut e = parsed?;
        functions::visit_expr(&mut e, &mut |x| self.fix(x), &mut |_| {})?;
        Ok(self.wrap(e))
    }

    /// An expression with what running it needs to know of it.
    fn wrap(&self, e: Expr) -> E {
        let (mut query, mut bound) = (false, false);
        walk(&e, &mut |x| {
            query |= matches!(x, Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..));
            bound |= matches!(x, Expr::Param(_) | Expr::Func(Func::Enum(..) | Func::Ctx(CtxFn::RegOid(_)), _))
                || matches!(x, Expr::Cast(_, t) if matches!(t, Type::Enum(_) | Type::Hstore | Type::Ltree | Type::Lquery) || t.reg_like())
                || matches!(
                    x,
                    Expr::Func(
                        Func::Scalar(
                            super::funcs::Scalar::Hstore(_)
                                | super::funcs::Scalar::Ltree(_)
                                | super::funcs::Scalar::JsonFn(_)
                                | super::funcs::Scalar::JsonCtor
                        ),
                        _
                    )
                )
                // (`rec.*` is given its fields as a statement is made: see `Frame::stars`)
                || matches!(x, Expr::Col(Some(_), c) if c == "*");
        });
        let sel = Box::new(Stmt::Select(Box::new(Select {
            set: vec![],
            distinct: false,
            distinct_on: vec![],
            items: vec![Item::Expr(e.clone(), None)],
            from: None,
            joins: vec![],
            filter: None,
            group: vec![],
            having: None,
            order: vec![],
            limit: None,
            offset: None,
            locking: vec![],
        })));
        E { e, query, bound, sel }
    }

    fn block(&mut self) -> Result<Block> {
        let label = self.pending_block.take();
        self.scopes.push(Scope { vars: vec![], label: label.clone(), is_loop: false, loop_vars: false });
        let mut decls = vec![];
        if self.p.kw("declare") {
            while !self.p.is_kw("begin") {
                if self.p.peek().is_none() {
                    return self.p.fail("expected BEGIN");
                }
                if self.p.peek() == Some(&Tok::Sym("<<")) {
                    return err(SYNTAX, "block label must be placed before DECLARE, not after");
                }
                if let Some(d) = self.decl()? {
                    decls.push(d);
                }
            }
        }
        let line = self.line(self.p.i);
        self.p.expect_kw("begin")?;
        let body = self.stmts(&["end", "exception"])?;
        let mut handlers = vec![];
        if self.p.kw("exception") {
            // handlers see the block's variables, and SQLSTATE and SQLERRM
            self.scopes.push(Scope::of(vec![("sqlstate".into(), Kind::Scalar), ("sqlerrm".into(), Kind::Scalar)]));
            loop {
                self.p.expect_kw("when")?;
                let mut conds = vec![self.condition()?];
                while self.p.kw("or") {
                    conds.push(self.condition()?);
                }
                self.p.expect_kw("then")?;
                handlers.push(Handler { conds, body: self.stmts(&["when", "end"])? });
                if !self.p.is_kw("when") {
                    break;
                }
            }
            self.scopes.pop();
        }
        self.p.expect_kw("end")?;
        if let Some(Tok::Word(w) | Tok::Ident(w)) = self.p.peek().cloned() {
            self.p.i += 1;
            end_label(&label, &w)?;
        }
        self.scopes.pop();
        Ok(Block {
            decls,
            body,
            handlers,
            whole: vec![],
            line,
            label,
            shadows: vec![],
            conflict: None,
            strict_params: None,
        })
    }

    /// A WHEN's condition: a name, `SQLSTATE 'xxxxx'`, or OTHERS (None).
    fn condition(&mut self) -> Result<Option<String>> {
        if self.p.kw("sqlstate") {
            return match self.p.next() {
                Some(Tok::Str(c))
                    if c.len() == 5
                        && c.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
                        && c != "00000" =>
                {
                    Ok(Some(c))
                }
                Some(Tok::Str(_)) => err(SYNTAX, "invalid SQLSTATE code"),
                _ => self.p.fail("expected SQLSTATE 'xxxxx'"),
            };
        }
        let n = self.p.name()?;
        if n == "others" {
            return Ok(None);
        }
        Ok(Some(condition(&n)?.into()))
    }

    /// A declaration; None for an ALIAS, which declares no variable of its own.
    fn decl(&mut self) -> Result<Option<Decl>> {
        let line = self.line(self.p.i);
        let name = self.p.name()?;
        // name ALIAS FOR target: another name for a parameter ($n) or a variable, which the rest of the body's
        // references to the name are made into
        if self.p.kw("alias") {
            self.p.expect_kw("for")?;
            let target = match self.p.next() {
                Some(Tok::Param(n)) => Tok::Param(n),
                Some(Tok::Word(w) | Tok::Ident(w)) if self.kind(&w).is_some() => Tok::Word(w),
                Some(Tok::Word(w) | Tok::Ident(w)) => return err(SYNTAX, format!("variable \"{w}\" does not exist")),
                _ => return self.p.fail("expected a variable or $n after ALIAS FOR"),
            };
            self.semi()?;
            if self.scopes.last().is_some_and(|s| s.vars.iter().any(|(n, _)| *n == name)) {
                return err(SYNTAX, format!("duplicate declaration of \"{name}\""));
            }
            self.shadow(&name);
            if let Tok::Word(w) = &target {
                let kind = self.kind(w).unwrap_or(Kind::Scalar);
                self.scopes.last_mut().unwrap().vars.push((name.clone(), kind));
            }
            for k in self.p.i..self.p.t.len() {
                let after_dot = k > 0 && self.p.t[k - 1] == Tok::Sym(".");
                if !after_dot && matches!(&self.p.t[k], Tok::Word(w) | Tok::Ident(w) if *w == name) {
                    self.p.t[k] = target.clone();
                }
            }
            return Ok(None);
        }
        // name [[NO] SCROLL] CURSOR [(arg type, ...)] FOR query: a cursor bound to its query, a variable holding
        // its name
        let word = |me: &Self, k: usize, w: &str| matches!(me.p.t.get(me.p.i + k), Some(Tok::Word(x)) if x == w);
        let skip =
            if word(self, 0, "no") && word(self, 1, "scroll") { 2 } else { usize::from(word(self, 0, "scroll")) };
        if word(self, skip, "cursor") {
            let scroll = match skip {
                2 => Some(false),
                1 => Some(true),
                _ => None,
            };
            self.p.i += skip + 1;
            let mut params: Vec<(String, Type)> = vec![];
            if self.p.sym("(") {
                loop {
                    let n = self.p.name()?;
                    self.shadow(&n);
                    self.p.typmod = None;
                    let t = self.p.ty()?;
                    params.push((n, t));
                    if !self.p.sym(",") {
                        break;
                    }
                }
                self.p.expect_sym(")")?;
            }
            if !(self.p.kw("for") || self.p.kw("is")) {
                return self.p.fail("expected FOR or IS");
            }
            let (toks, into) = self.sql_tokens(false)?;
            let text = std::mem::take(&mut self.text);
            let q = self.statement(toks)?;
            if into.is_some() || !matches!(q, Stmt::Select(_) | Stmt::Locking(..)) {
                return err(SYNTAX, "a cursor's query must be a SELECT");
            }
            self.semi()?;
            self.cursors.push(Cur { name: name.clone(), query: q, params, scroll, text });
            self.shadow(&name);
            self.scopes.last_mut().unwrap().vars.push((name.clone(), Kind::Scalar));
            self.typed.push((name.clone(), Type::Text, None));
            let default = Some(self.wrap(Expr::Lit(Value::Text(name.clone()))));
            return Ok(Some(Decl { name, ty: Some((Type::Text, None)), not_null: false, default, line }));
        }
        let constant = self.p.kw("constant");
        // var%TYPE, table.column%TYPE and table%ROWTYPE: the type of that variable or column, or a row of the table
        let pct = (self.p.i..self.p.t.len())
            .take_while(|k| !matches!(self.p.t[*k], Tok::Sym(";" | ":=" | "=")))
            .find(|k| self.p.t[*k] == Tok::Sym("%"));
        let ty = if let Some(at) = pct {
            let rowtype = matches!(self.p.t.get(at + 1), Some(Tok::Word(w)) if w == "rowtype");
            if !rowtype && !matches!(self.p.t.get(at + 1), Some(Tok::Word(w)) if w == "type") {
                return self.p.fail("expected %TYPE or %ROWTYPE");
            }
            let mut parts = vec![self.p.name()?];
            while self.p.sym(".") {
                parts.push(self.p.name()?);
            }
            self.p.i = at + 2;
            if rowtype {
                None
            } else if let (1, Some((_, t, m))) = (parts.len(), self.typed.iter().rev().find(|(n, ..)| *n == parts[0])) {
                Some((*t, *m))
            } else {
                Some(column_type(&parts)?)
            }
        } else if self.p.kw("record") {
            None
        } else {
            self.p.typmod = None;
            let t = self.p.ty()?;
            Some((t, self.p.typmod.take()))
        };
        // COLLATE: one of bytes is what text has anyway; an ICU one would change how the variable compares
        if self.p.kw("collate") && !self.p.collation_name()?.bytes() {
            return err(UNSUPPORTED, "COLLATE with an ICU collation on a PL/pgSQL variable isn't supported yet");
        }
        let not_null = self.p.kw("not");
        if not_null {
            self.p.expect_kw("null")?;
        }
        let default =
            if self.p.kw("default") || self.p.sym(":=") || self.p.sym("=") { Some(self.expr()?) } else { None };
        self.semi()?;
        if not_null && default.is_none() {
            return err(SYNTAX, format!("variable \"{name}\" must have a default value, since it's declared NOT NULL"));
        }
        if ty.is_none() && default.is_some() {
            return err(UNSUPPORTED, "a RECORD variable can't have a default");
        }
        if self.scopes.last().is_some_and(|s| s.vars.iter().any(|(n, _)| *n == name)) {
            return err(SYNTAX, format!("duplicate declaration of \"{name}\""));
        }
        self.shadow(&name);
        let kind = match (&ty, constant) {
            (None, _) => Kind::Record,
            (_, true) => Kind::Const,
            _ => Kind::Scalar,
        };
        self.scopes.last_mut().unwrap().vars.push((name.clone(), kind));
        if let Some((t, m)) = &ty {
            self.typed.push((name.clone(), *t, *m));
        }
        Ok(Some(Decl { name, ty, not_null, default, line }))
    }

    /// Statements until one of the keywords `ends` (not taken).
    fn stmts(&mut self, ends: &[&str]) -> Result<Vec<S>> {
        // a nested block is a level deeper, as a subquery is (see `Parser::deeper`)
        let at = self.p.depth;
        let out = self.p.deeper(SUBQUERY).and_then(|_| self.stmts_in(ends));
        self.p.depth = at;
        out
    }

    fn stmts_in(&mut self, ends: &[&str]) -> Result<Vec<S>> {
        let mut out = vec![];
        loop {
            match self.p.peek() {
                None => return self.p.fail("expected END"),
                Some(Tok::Word(w)) if ends.contains(&w.as_str()) => return Ok(out),
                _ => {
                    // (a labeled loop's line and kind are its keyword's, after `<<label>>`)
                    let k = if self.p.peek() == Some(&Tok::Sym("<<")) { self.p.i + 3 } else { self.p.i };
                    let first = match self.p.t.get(k) {
                        Some(Tok::Word(w)) => Some(w.clone()),
                        _ => None,
                    };
                    let line = self.line(k);
                    let s = self.stmt()?;
                    let what = what(&s, first.as_deref().unwrap_or(""));
                    out.push(S::At(line, what, Box::new(s)));
                }
            }
        }
    }

    fn assignment_ahead(&self) -> bool {
        let t = |k: usize| self.p.t.get(self.p.i + k);
        let name = |k: usize| matches!(t(k), Some(Tok::Word(_) | Tok::Ident(_)));
        let to = |k: usize| matches!(t(k), Some(Tok::Sym(":=" | "=")));
        // (`$1 := ...` too: a parameter by its number)
        (matches!(t(0), Some(Tok::Param(_))) && to(1))
            || (name(0)
                && (to(1) || (t(1) == Some(&Tok::Sym(".")) && name(2) && to(3)) || t(1) == Some(&Tok::Sym("["))))
    }

    fn stmt(&mut self) -> Result<S> {
        // `<<label>>` before a block or a loop
        if self.p.peek() == Some(&Tok::Sym("<<")) {
            let label = self.label()?;
            return match self.word().map(String::from).as_deref() {
                Some("declare" | "begin") => {
                    self.pending_block = Some(label);
                    let b = self.block()?;
                    self.semi()?;
                    Ok(S::Block(b))
                }
                Some("loop" | "while" | "for" | "foreach") => {
                    // (a level of its own, which EXIT and CONTINUE find it by, and its FOR variables are one further in)
                    self.scopes.push(Scope {
                        vars: vec![],
                        label: Some(label.clone()),
                        is_loop: true,
                        loop_vars: false,
                    });
                    self.pending_loop = Some(label.clone());
                    let s = self.stmt();
                    self.scopes.pop();
                    self.pending_loop = None;
                    Ok(S::Labeled(label, Box::new(s?)))
                }
                _ => self.p.fail("expected a block or a loop after the label"),
            };
        }
        if matches!(self.p.peek(), Some(Tok::Sym("<"))) {
            return self.p.fail("expected a PL/pgSQL statement");
        }
        // (a label-qualified assignment target, `blk.x := ...`)
        self.qualify(true, true);
        if self.assignment_ahead() {
            return self.assign();
        }
        let Some(w) = self.word().map(String::from) else { return self.p.fail("expected a PL/pgSQL statement") };
        let refuse = |what: &str| err(UNSUPPORTED, format!("{what} isn't supported in PL/pgSQL functions"));
        match w.as_str() {
            "select" | "with" | "insert" | "update" | "delete" | "merge" => return self.sql(false),
            "perform" => return self.sql(true),
            "begin" | "declare" => {
                let b = self.block()?;
                self.semi()?;
                return Ok(S::Block(b));
            }
            "execute" => return self.execute(),
            "open" | "fetch" | "close" | "move" => return self.cursor_stmt(&w),
            "foreach" => return self.foreach_loop(),
            // COMMIT [AND [NO] CHAIN] in a procedure or a DO block (see `S::Commit`): the chained transaction is one
            // like the one it ends, which is all a commit here is
            "commit" if self.f.procedure || self.f.name == "inline_code_block" => {
                self.p.i += 1;
                if self.p.kw("and") {
                    self.p.kw("no");
                    self.p.expect_kw("chain")?;
                }
                self.semi()?;
                return Ok(S::Commit);
            }
            "commit" | "rollback" | "call" => return refuse("COMMIT in a function, ROLLBACK and CALL"),
            // DDL written out: run as the dynamic statement it is (variables in it are not filled in)
            "create" | "drop" | "alter" | "truncate" | "grant" | "revoke" => return self.ddl_as_execute(),
            "copy" | "set" | "reset" | "show" | "notify" | "listen" | "vacuum" | "analyze" | "lock" => {
                return refuse(&w.to_uppercase());
            }
            _ => {}
        }
        self.p.i += 1;
        Ok(match w.as_str() {
            "if" => {
                let mut arms = vec![];
                loop {
                    let c = self.expr()?;
                    self.p.expect_kw("then")?;
                    arms.push((c, self.stmts(&["elsif", "elseif", "else", "end"])?));
                    if !(self.p.kw("elsif") || self.p.kw("elseif")) {
                        break;
                    }
                }
                let other = if self.p.kw("else") { self.stmts(&["end"])? } else { vec![] };
                self.p.expect_kw("end")?;
                self.p.expect_kw("if")?;
                self.semi()?;
                S::If(arms, other)
            }
            "case" => {
                let operand = if self.p.is_kw("when") { None } else { Some(self.expr()?) };
                let mut arms = vec![];
                while self.p.kw("when") {
                    let mut vals = vec![self.expr()?];
                    while operand.is_some() && self.p.sym(",") {
                        vals.push(self.expr()?);
                    }
                    self.p.expect_kw("then")?;
                    arms.push((vals, self.stmts(&["when", "else", "end"])?));
                }
                if arms.is_empty() {
                    return self.p.fail("expected WHEN");
                }
                let other = if self.p.kw("else") { Some(self.stmts(&["end"])?) } else { None };
                self.p.expect_kw("end")?;
                self.p.expect_kw("case")?;
                self.semi()?;
                S::Case(operand, arms, other)
            }
            "loop" => S::Loop(self.loop_body()?),
            "while" => {
                let c = self.expr()?;
                self.p.expect_kw("loop")?;
                S::While(c, self.loop_body()?)
            }
            "for" => self.for_loop()?,
            "exit" | "continue" => {
                let exit = w == "exit";
                // EXIT [label] [WHEN cond]: the label of a block or loop it is in (CONTINUE's a loop's)
                let label = match self.p.peek() {
                    Some(Tok::Word(l)) if l != "when" => Some(l.clone()),
                    Some(Tok::Ident(l)) => Some(l.clone()),
                    _ => None,
                };
                match &label {
                    Some(l) => {
                        self.p.i += 1;
                        match self.scopes.iter().rev().find(|s| s.label.as_deref() == Some(l.as_str())) {
                            None => {
                                return err(
                                    SYNTAX,
                                    format!(
                                        "there is no label \"{l}\" attached to any block or loop enclosing this statement"
                                    ),
                                );
                            }
                            Some(s) if !exit && !s.is_loop => {
                                return err(SYNTAX, format!("block label \"{l}\" cannot be used in CONTINUE"));
                            }
                            Some(_) => {}
                        }
                    }
                    None if self.loops == 0 => {
                        let m = if exit {
                            "EXIT cannot be used outside a loop, unless it has a label"
                        } else {
                            "CONTINUE cannot be used outside a loop"
                        };
                        return err(SYNTAX, m);
                    }
                    None => {}
                }
                let c = if self.p.kw("when") { Some(self.expr()?) } else { None };
                self.semi()?;
                if exit { S::Exit(label, c) } else { S::Continue(label, c) }
            }
            "return" => self.ret()?,
            "raise" => self.raise()?,
            "assert" => {
                let c = self.expr()?;
                let m = if self.p.sym(",") { Some(self.expr()?) } else { None };
                self.semi()?;
                S::Assert(c, m)
            }
            "get" => {
                let stacked = self.p.kw("stacked");
                if !stacked {
                    self.p.kw("current");
                }
                self.p.expect_kw("diagnostics")?;
                let mut items = vec![];
                loop {
                    let v = self.p.name()?;
                    match self.kind(&v) {
                        Some(Kind::Scalar) => {}
                        Some(Kind::Const) => return err(SYNTAX, format!("variable \"{v}\" is declared CONSTANT")),
                        Some(Kind::Record) => return err(SYNTAX, format!("\"{v}\" is not a scalar variable")),
                        None => return err(SYNTAX, format!("\"{v}\" is not a known variable")),
                    }
                    if !self.p.sym(":=") {
                        self.p.expect_sym("=")?;
                    }
                    let item = self.p.name()?;
                    let here = if stacked { "STACKED" } else { "CURRENT" };
                    let fits =
                        if stacked { STACKED.contains(&item.as_str()) } else { CURRENT.contains(&item.as_str()) };
                    if !fits {
                        if CURRENT.contains(&item.as_str()) || STACKED.contains(&item.as_str()) {
                            return err(
                                SYNTAX,
                                format!(
                                    "diagnostics item {} is not allowed in GET {here} DIAGNOSTICS",
                                    item.to_uppercase()
                                ),
                            );
                        }
                        return err(SYNTAX, format!("unrecognized GET DIAGNOSTICS item at or near \"{item}\""));
                    }
                    items.push((v, item));
                    if !self.p.sym(",") {
                        break;
                    }
                }
                self.semi()?;
                S::Diag(stacked, items)
            }
            "null" => {
                self.semi()?;
                S::Null
            }
            _ => {
                // (the word as written: Postgres says it in its own case)
                let shown = self.source(self.p.i - 1, self.p.i);
                let shown = if shown.is_empty() { w.clone() } else { shown };
                return err(SYNTAX, format!("syntax error at or near \"{shown}\""));
            }
        })
    }

    /// A loop's body, through `END LOOP [label];` (the loop's label, if it has one, see `pending_loop`).
    fn loop_body(&mut self) -> Result<Vec<S>> {
        let label = self.pending_loop.take();
        self.loops += 1;
        let body = self.stmts(&["end"]);
        self.loops -= 1;
        let body = body?;
        self.p.expect_kw("end")?;
        self.p.expect_kw("loop")?;
        if let Some(Tok::Word(w) | Tok::Ident(w)) = self.p.peek().cloned() {
            self.p.i += 1;
            end_label(&label, &w)?;
        }
        self.semi()?;
        Ok(body)
    }

    /// A parameter's variable by its number, `$n`: an input parameter's (named or not), or in a function with no
    /// input parameters, its n-th OUT parameter's.
    fn param_var(&self, n: usize) -> Result<String> {
        if let Some((name, _)) = n.checked_sub(1).and_then(|k| self.f.args.get(k)) {
            return Ok(name.clone().unwrap_or_else(|| format!("${n}")));
        }
        let outs: Vec<&String> = match &self.f.ret {
            Ret::Table(cols) => {
                cols.iter().map(|(c, _)| c).filter(|c| !self.f.args.iter().any(|a| a.0.as_ref() == Some(*c))).collect()
            }
            _ => vec![],
        };
        // (with input parameters too, which come first isn't kept: refused rather than guessed)
        match outs.get(n.wrapping_sub(1)) {
            Some(c) if self.f.args.is_empty() => Ok((*c).clone()),
            Some(_) => err(UNSUPPORTED, format!("assigning to ${n}, an OUT parameter, isn't supported: name it")),
            None => err(SYNTAX, format!("there is no parameter ${n}")),
        }
    }

    fn assign(&mut self) -> Result<S> {
        let name = match self.p.peek() {
            Some(&Tok::Param(n)) => {
                self.p.i += 1;
                self.param_var(n)?
            }
            _ => self.p.name()?,
        };
        let field = if self.p.sym(".") { Some(self.p.name()?) } else { None };
        // name[i] := v, for an array variable: the variable set to the array with that element changed
        if field.is_none() && self.p.sym("[") {
            let idx = self.expr()?;
            self.p.expect_sym("]")?;
            if !self.p.sym(":=") {
                self.p.expect_sym("=")?;
            }
            match self.kind(&name) {
                Some(Kind::Scalar) => {}
                None => return err(SYNTAX, format!("\"{name}\" is not a known variable")),
                Some(Kind::Const) => return err(SYNTAX, format!("variable \"{name}\" is declared CONSTANT")),
                Some(Kind::Record) => return err(SYNTAX, format!("\"{name}\" is not an array")),
            }
            let val = self.expr()?;
            self.semi()?;
            let call =
                Expr::Func(Func::Scalar(funcs::Scalar::ArraySet), vec![Expr::Col(None, name.clone()), idx.e, val.e]);
            return Ok(S::Assign(name, self.wrap(call)));
        }
        if self.p.sym("[") {
            return err(UNSUPPORTED, "assigning to a field's array element isn't supported in PL/pgSQL functions");
        }
        if !self.p.sym(":=") {
            self.p.expect_sym("=")?;
        }
        let target = match (self.kind(&name), field) {
            (None, _) => return err(SYNTAX, format!("\"{name}\" is not a known variable")),
            (Some(Kind::Const), _) => return err(SYNTAX, format!("variable \"{name}\" is declared CONSTANT")),
            (Some(Kind::Record), Some(f)) => format!("{name}.{f}"),
            (Some(Kind::Record), None) => {
                return err(
                    UNSUPPORTED,
                    format!("assigning the whole record \"{name}\" isn't supported: assign its fields"),
                );
            }
            (Some(Kind::Scalar), Some(_)) => return err(SYNTAX, format!("\"{name}\" is not a record")),
            (Some(Kind::Scalar), None) => name,
        };
        let e = self.expr()?;
        self.semi()?;
        Ok(S::Assign(target, e))
    }

    /// `FOREACH x IN ARRAY expr LOOP ...`, as the FOR loop over the array's elements it is.
    fn foreach_loop(&mut self) -> Result<S> {
        let start = self.p.i;
        // FOREACH x SLICE n IN ARRAY expr LOOP: `x` (an array) each n-dimensional slice of the array in turn
        self.p.i += 1;
        let mut names = vec![self.p.name()?];
        while self.p.sym(",") {
            names.push(self.p.name()?);
        }
        if self.p.kw("slice") {
            let slice = match self.p.next() {
                Some(Tok::Num(n)) => n.parse::<usize>().ok(),
                _ => None,
            };
            let Some(slice) = slice else { return self.p.fail("expected an integer after SLICE") };
            self.p.expect_kw("in")?;
            self.p.expect_kw("array")?;
            let array = self.expr()?;
            self.p.expect_kw("loop")?;
            for n in &names {
                match self.kind(n) {
                    Some(Kind::Const) => return err(SYNTAX, format!("variable \"{n}\" is declared CONSTANT")),
                    None => {
                        return err(SYNTAX, "loop variable of FOREACH must be a known variable or list of variables");
                    }
                    _ => {}
                }
            }
            let var = match &names[..] {
                [n] if self.kind(n) == Some(Kind::Scalar) => Some(n.clone()),
                _ if slice == 0 => {
                    return err(UNSUPPORTED, "FOREACH ... SLICE 0 into a record or several variables isn't supported");
                }
                _ => None,
            };
            let body = self.loop_body()?;
            return Ok(S::ForEach { var, slice, array, body });
        }
        self.p.i = start;
        let (mut depth, mut at, mut end) = (0i32, None, None);
        for j in start + 1..self.p.t.len() {
            match &self.p.t[j] {
                Tok::Sym("(") => depth += 1,
                Tok::Sym(")") => depth -= 1,
                Tok::Word(w) if w == "array" && depth == 0 && at.is_none() => at = Some(j),
                Tok::Word(w) if w == "loop" && depth == 0 && at.is_some() => {
                    end = Some(j);
                    break;
                }
                _ => {}
            }
        }
        let (Some(at), Some(end)) = (at, end) else { return self.p.fail("expected IN ARRAY ... LOOP") };
        self.splice(end..end, vec![Tok::Sym(")")]);
        self.splice(at..at + 1, vec![Tok::Word("select".into()), Tok::Word("unnest".into()), Tok::Sym("(")]);
        self.p.i = start + 1;
        let mut s = self.for_loop()?;
        if let S::ForQuery { text, .. } = &mut s {
            text.clear(); // (no query of its own to show)
        }
        Ok(s)
    }

    fn for_loop(&mut self) -> Result<S> {
        let mut names = vec![self.p.name()?];
        while self.p.sym(",") {
            names.push(self.p.name()?);
        }
        self.p.expect_kw("in")?;
        // FOR r IN cursor [(args)] LOOP: the rows of a bound cursor's query, `r` a record the loop itself declares
        if let Some(Tok::Word(c) | Tok::Ident(c)) = self.p.peek().cloned()
            && matches!(self.kind(&c), Some(Kind::Scalar | Kind::Const))
            && matches!(self.p.t.get(self.p.i + 1), Some(Tok::Sym("(")) | Some(Tok::Word(_)))
            && (self.p.t.get(self.p.i + 1) == Some(&Tok::Sym("("))
                || matches!(self.p.t.get(self.p.i + 1), Some(Tok::Word(w)) if w == "loop"))
        {
            let Some(cur) = self.cursors.iter().rev().find(|k| k.name == c).cloned() else {
                return err(SYNTAX, "cursor FOR loop must use a bound cursor variable");
            };
            self.p.i += 1;
            let args = self.cursor_args(&cur)?;
            self.p.expect_kw("loop")?;
            let [r] = &names[..] else { return err(SYNTAX, "cursor FOR loop must have only one target variable") };
            let rec = r.clone();
            self.scopes.push(Scope { loop_vars: true, ..Scope::of(vec![(rec.clone(), Kind::Record)]) });
            let body = self.loop_body();
            self.scopes.pop();
            return Ok(S::ForCursor { rec, var: c, cur: Box::new(cur), args, body: body? });
        }
        let execute = self.p.is_kw("execute");
        // (a query: SELECT, WITH, or VALUES's rows, `FOR t IN VALUES ('a'), ('b') LOOP`)
        let values = self.p.is_kw("values") && matches!(self.p.t.get(self.p.i + 1), Some(Tok::Sym("(")));
        if execute || values || self.p.is_kw("select") || self.p.is_kw("with") {
            let target = match &names[..] {
                [r] if self.kind(r) == Some(Kind::Record) => Target::Record(r.clone()),
                _ if names.iter().all(|n| self.kind(n) == Some(Kind::Scalar)) => Target::Vars(names),
                _ => {
                    return err(
                        SYNTAX,
                        "loop variable of loop over rows must be a record variable or list of scalar variables",
                    );
                }
            };
            if execute {
                let (query, _, using) = self.dynamic(false)?;
                self.p.expect_kw("loop")?;
                return Ok(S::ForExecute { target, query, using, body: self.loop_body()? });
            }
            let (toks, into) = self.sql_tokens(true)?;
            let text = std::mem::take(&mut self.text);
            if into.is_some() {
                return err(SYNTAX, "INTO in a FOR loop's query: the loop's variables get its rows");
            }
            let query = self.statement(toks)?;
            if !matches!(query, Stmt::Select(_) | Stmt::Locking(..)) {
                return err(UNSUPPORTED, "a FOR loop's query must be a SELECT here");
            }
            self.p.expect_kw("loop")?;
            return Ok(S::ForQuery { target, query: Box::new(query), body: self.loop_body()?, text });
        }
        let [var] = &names[..] else { return err(SYNTAX, "a FOR loop over numbers has one variable") };
        let var = var.clone();
        let reverse = self.p.kw("reverse");
        let lo = self.expr()?;
        self.p.expect_sym("..")?;
        let hi = self.expr()?;
        let by = if self.p.kw("by") { Some(self.expr()?) } else { None };
        self.p.expect_kw("loop")?;
        self.scopes.push(Scope { loop_vars: true, ..Scope::of(vec![(var.clone(), Kind::Scalar)]) });
        let body = self.loop_body();
        self.scopes.pop();
        Ok(S::ForInt { var, reverse, lo, hi, by, body: body? })
    }

    fn ret(&mut self) -> Result<S> {
        let sets = matches!(self.f.ret, Ret::SetOf(_) | Ret::Table(_) | Ret::RowsOf(_)) && !self.f.single;
        let record_alone = |me: &Self| match me.p.peek() {
            Some(Tok::Word(w) | Tok::Ident(w))
                if me.kind(w) == Some(Kind::Record) && me.p.t.get(me.p.i + 1) == Some(&Tok::Sym(";")) =>
            {
                Some(w.clone())
            }
            _ => None,
        };
        if self.p.kw("next") {
            if !sets {
                return err("42804", "cannot use RETURN NEXT in a non-SETOF function");
            }
            if self.p.sym(";") {
                if !matches!(self.f.ret, Ret::Table(_)) {
                    return err(SYNTAX, "RETURN NEXT must have a parameter");
                }
                return Ok(S::ReturnNext(None));
            }
            if matches!(self.f.ret, Ret::Table(_)) {
                return err("42804", "RETURN NEXT cannot have a parameter in function with OUT parameters");
            }
            if let Some(r) = record_alone(self) {
                self.p.i += 2;
                return Ok(S::ReturnNextRecord(r));
            }
            let e = self.expr()?;
            self.semi()?;
            return Ok(S::ReturnNext(Some(e)));
        }
        if self.p.kw("query") {
            if !sets {
                return err("42804", "cannot use RETURN QUERY in a non-SETOF function");
            }
            if self.p.is_kw("execute") {
                let (query, _, using) = self.dynamic(false)?;
                self.semi()?;
                return Ok(S::ReturnQueryExecute(query, using));
            }
            let (toks, into) = self.sql_tokens(false)?;
            let text = std::mem::take(&mut self.text);
            let q = self.statement(toks)?;
            // (a SELECT INTO makes a table and returns no rows: Postgres's error when it runs)
            if into.is_some() && matches!(q, Stmt::Select(_)) {
                self.semi()?;
                return Ok(S::NoTuples(std::mem::take(&mut self.raw)));
            }
            if into.is_some() || !matches!(q, Stmt::Select(_)) {
                return err(SYNTAX, "RETURN QUERY takes a SELECT");
            }
            self.semi()?;
            return Ok(S::ReturnQuery(Box::new(q), text));
        }
        if self.p.sym(";") {
            return match self.f.ret {
                Ret::Value(_) | Ret::Trigger => err(SYNTAX, "missing expression at or near \";\""),
                _ => Ok(S::Return(None)),
            };
        }
        if sets {
            return err(
                "42804",
                "RETURN cannot have a parameter in function returning set; use RETURN NEXT or RETURN QUERY",
            );
        }
        if self.f.ret == Ret::Void {
            return err("42804", "RETURN cannot have a parameter in function returning void");
        }
        if let Some(r) = record_alone(self) {
            // (a function returning a table's row type or a composite type returns a row: one named so, before CREATE
            // FUNCTION looks the name up, is parsed as an enum's)
            let row = matches!(self.f.ret, Ret::RowsOf(_) | Ret::Value(Type::Enum(_) | Type::Record(_)));
            if self.f.ret != Ret::Trigger && !self.f.single && !row {
                return err(
                    UNSUPPORTED,
                    "returning a record from a function isn't supported: return a value, or use RETURNS TABLE",
                );
            }
            self.p.i += 2;
            return Ok(S::ReturnRecord(r));
        }
        let e = self.expr()?;
        self.semi()?;
        Ok(S::Return(Some(e)))
    }

    fn raise(&mut self) -> Result<S> {
        let level = match self.word() {
            Some("debug") => "DEBUG",
            Some("log") => "LOG",
            Some("info") => "INFO",
            Some("notice") => "NOTICE",
            Some("warning") => "WARNING",
            Some("exception") => "EXCEPTION",
            _ => "",
        };
        let level = if level.is_empty() {
            "EXCEPTION" // the default
        } else {
            self.p.i += 1;
            level
        };
        let (mut cond, mut fmt, mut args, mut opts) = (None, None, vec![], vec![]);
        match self.p.peek().cloned() {
            Some(Tok::Str(s)) => {
                self.p.i += 1;
                fmt = Some(s);
                while self.p.sym(",") {
                    args.push(self.expr()?);
                }
            }
            Some(Tok::Word(w)) if w == "sqlstate" => {
                self.p.i += 1;
                match self.p.next() {
                    Some(Tok::Str(s)) => cond = Some((sqlstate(&s)?, s)),
                    _ => return self.p.fail("expected SQLSTATE 'xxxxx'"),
                }
            }
            Some(Tok::Word(w)) if w != "using" => {
                self.p.i += 1;
                cond = Some((condition(&w)?, w));
            }
            Some(Tok::Sym(";")) if level == "EXCEPTION" && self.p.t[self.p.i - 1] == Tok::Word("raise".into()) => {
                self.p.i += 1;
                return Ok(S::Reraise);
            }
            _ => {}
        }
        if self.p.kw("using") {
            loop {
                // (COLUMN, CONSTRAINT and TABLE are reserved words, and option names here; one given twice is an error
                // when it runs, as Postgres's)
                let o = match self.p.next() {
                    Some(Tok::Word(w)) => w,
                    _ => return self.p.fail("expected a RAISE option"),
                };
                if !["message", "detail", "hint", "errcode", "column", "constraint", "datatype", "table", "schema"]
                    .contains(&o.as_str())
                {
                    return err(SYNTAX, format!("unrecognized RAISE statement option at or near \"{o}\""));
                }
                if !self.p.sym(":=") {
                    self.p.expect_sym("=")?;
                }
                opts.push((o, self.expr()?));
                if !self.p.sym(",") {
                    break;
                }
            }
        }
        self.semi()?;
        if let Some(f) = &fmt {
            let wanted = f.replace("%%", "").matches('%').count();
            if wanted > args.len() {
                return err(SYNTAX, "too few parameters specified for RAISE");
            }
            if wanted < args.len() {
                return err(SYNTAX, "too many parameters specified for RAISE");
            }
        }
        Ok(S::Raise { level, cond, fmt, args, opts })
    }

    /// The tokens of the SQL statement starting here, up to its `;` (or with `until_loop`, a FOR
    /// loop's `LOOP`), with its `INTO [STRICT] targets` taken out.
    #[allow(clippy::type_complexity)]
    fn sql_tokens(&mut self, until_loop: bool) -> Result<(Vec<Tok>, Option<(Vec<String>, bool)>)> {
        self.qualify(until_loop, true);
        let (mut depth, mut j) = (0i32, self.p.i);
        loop {
            match self.p.t.get(j) {
                None => return err(SYNTAX, "syntax error at end of input: expected ;"),
                Some(Tok::Sym("(")) => depth += 1,
                Some(Tok::Sym(")")) => depth -= 1,
                Some(Tok::Sym(";")) if depth == 0 && !until_loop => break,
                Some(Tok::Word(w)) if depth == 0 && until_loop && w == "loop" => break,
                _ => {}
            }
            j += 1;
        }
        let mut toks = self.p.t[self.p.i..j].to_vec();
        let start = self.p.i;
        self.text = self.source(start, j);
        self.raw = self.text.clone();
        self.p.i = j;
        let mut depth = 0i32;
        for k in 0..toks.len() {
            match &toks[k] {
                Tok::Sym("(") => depth += 1,
                Tok::Sym(")") => depth -= 1,
                Tok::Word(w)
                    if w == "into"
                        && depth == 0
                        && !(k > 0 && matches!(&toks[k - 1], Tok::Word(p) if p == "insert" || p == "merge")) =>
                {
                    let mut e = k + 1;
                    let strict = toks.get(e) == Some(&Tok::Word("strict".into()));
                    e += strict as usize;
                    let mut names = vec![];
                    loop {
                        let mut name = match toks.get(e) {
                            Some(Tok::Word(w) | Tok::Ident(w)) => w.clone(),
                            _ => return err(SYNTAX, "expected a variable after INTO"),
                        };
                        e += 1;
                        if toks.get(e) == Some(&Tok::Sym(".")) {
                            match toks.get(e + 1) {
                                Some(Tok::Word(f) | Tok::Ident(f)) => name = format!("{name}.{f}"),
                                _ => return err(SYNTAX, "expected a field after ."),
                            }
                            e += 2;
                        }
                        names.push(name);
                        if toks.get(e) != Some(&Tok::Sym(",")) {
                            break;
                        }
                        e += 1;
                    }
                    toks.drain(k..e);
                    // (the text with its INTO clause blanked out, as Postgres keeps it)
                    let (a, b, c, d) =
                        (self.at_char(start), self.at_char(start + k), self.at_char(start + e), self.at_char(j));
                    if a <= b && b <= c && c <= d {
                        let mut text: String = self.src[a..b].iter().collect();
                        text.extend(std::iter::repeat_n(' ', c - b));
                        text.extend(&self.src[c..d]);
                        self.text = text.trim_end().to_string();
                    }
                    return Ok((toks, Some((names, strict))));
                }
                _ => {}
            }
        }
        Ok((toks, None))
    }

    /// A statement from its tokens, record fields named as variables name them.
    fn statement(&self, toks: Vec<Tok>) -> Result<Stmt> {
        let mut q = Parser::new(toks);
        let mut s = q.stmt()?;
        if q.peek().is_some() {
            return q.fail("expected the end of the statement");
        }
        if !matches!(
            s,
            Stmt::Select(_)
                | Stmt::Locking(..)
                | Stmt::Insert { .. }
                | Stmt::Update { .. }
                | Stmt::Delete { .. }
                | Stmt::Merge(_)
        ) {
            return err(UNSUPPORTED, format!("{} isn't supported in PL/pgSQL functions", functions::what(&s)));
        }
        functions::visit(&mut s, &mut |e| self.fix(e), &mut |_| {})?;
        Ok(s)
    }

    fn sql(&mut self, perform: bool) -> Result<S> {
        let (mut toks, into) = self.sql_tokens(false)?;
        let mut text = std::mem::take(&mut self.text);
        if perform {
            toks[0] = Tok::Word("select".into());
            // (Postgres writes SELECT over the word PERFORM)
            text = format!("SELECT{}", text.chars().skip(7).collect::<String>());
        }
        let stmt = self.statement(toks)?;
        self.semi()?;
        let into = match into {
            None => None,
            Some(_) if perform => return err(SYNTAX, "PERFORM runs a query for its effects: it has no INTO"),
            Some((names, strict)) => {
                let returns = match &stmt {
                    Stmt::Insert { returning, .. }
                    | Stmt::Update { returning, .. }
                    | Stmt::Delete { returning, .. } => returning.is_some(),
                    _ => true,
                };
                if !returns {
                    return err(SYNTAX, "INTO used with a command that cannot return data");
                }
                Some((self.targets(names)?, strict))
            }
        };
        Ok(S::Sql { stmt: Box::new(stmt), into, perform, text })
    }

    /// INTO's targets: one record, or variables and records' fields.
    fn targets(&self, names: Vec<String>) -> Result<Target> {
        Ok(match &names[..] {
            [r] if self.kind(r) == Some(Kind::Record) => Target::Record(r.clone()),
            _ => {
                for n in &names {
                    let base = n.split('.').next().unwrap_or(n);
                    match (self.kind(base), n.contains('.')) {
                        (Some(Kind::Scalar), false) | (Some(Kind::Record), true) => {}
                        (Some(Kind::Const), _) => {
                            return err(SYNTAX, format!("variable \"{n}\" is declared CONSTANT"));
                        }
                        (Some(Kind::Record), false) => {
                            return err(
                                UNSUPPORTED,
                                format!("a record (\"{n}\") among several INTO targets isn't supported"),
                            );
                        }
                        _ => return err(SYNTAX, format!("\"{n}\" is not a known variable")),
                    }
                }
                Target::Vars(names)
            }
        })
    }

    /// `EXECUTE string [INTO [STRICT] targets] [USING expression, ...]`, and after FOR ... IN
    /// and RETURN QUERY, `EXECUTE string [USING ...]` (`into`: whether INTO may come).
    #[allow(clippy::type_complexity)]
    fn dynamic(&mut self, into_ok: bool) -> Result<(E, Option<(Target, bool)>, Vec<E>)> {
        self.p.expect_kw("execute")?;
        let query = self.expr()?;
        let (mut into, mut using) = (None, vec![]);
        loop {
            if into_ok && into.is_none() && self.p.kw("into") {
                let strict = self.p.kw("strict");
                let mut names = vec![];
                loop {
                    let mut n = self.p.name()?;
                    if self.p.sym(".") {
                        n = format!("{n}.{}", self.p.name()?);
                    }
                    names.push(n);
                    if !self.p.sym(",") {
                        break;
                    }
                }
                into = Some((self.targets(names)?, strict));
            } else if using.is_empty() && self.p.kw("using") {
                loop {
                    using.push(self.expr()?);
                    if !self.p.sym(",") {
                        break;
                    }
                }
            } else {
                return Ok((query, into, using));
            }
        }
    }

    /// A DDL statement in the body, as `EXECUTE 'its text'`.
    fn ddl_as_execute(&mut self) -> Result<S> {
        let (toks, _) = self.sql_tokens(false)?;
        // (the tokens as text, the way EXECUTE would be given them)
        let text = tokens_shown(&toks);
        let at = self.p.i;
        self.splice(at..at, vec![Tok::Word("execute".into()), Tok::Str(text)]);
        self.execute()
    }

    /// OPEN, FETCH, MOVE and CLOSE.
    fn cursor_stmt(&mut self, w: &str) -> Result<S> {
        self.p.i += 1;
        let var = |me: &mut Self| -> Result<String> {
            let n = me.p.name()?;
            match me.kind(&n) {
                Some(Kind::Scalar | Kind::Const) => Ok(n),
                _ => err(SYNTAX, format!("variable \"{n}\" must be of type cursor or refcursor")),
            }
        };
        match w {
            "close" => {
                let v = var(self)?;
                self.semi()?;
                Ok(S::Close(v))
            }
            "open" => {
                let v = var(self)?;
                let constant = self.kind(&v) == Some(Kind::Const);
                // a bound cursor: its query, given its arguments
                if let Some(cur) = self.cursors.iter().rev().find(|k| k.name == v).cloned() {
                    let args = self.cursor_args(&cur)?;
                    self.semi()?;
                    let (query, scroll, text) = (Some(Box::new(cur.query)), cur.scroll, cur.text);
                    return Ok(S::Open { var: v, query, dynamic: None, args, scroll, constant, text });
                }
                let scroll = if self.p.kw("no") {
                    self.p.expect_kw("scroll")?;
                    Some(false)
                } else if self.p.kw("scroll") {
                    Some(true)
                } else {
                    None
                };
                self.p.expect_kw("for")?;
                if self.p.is_kw("execute") {
                    let (query, _, using) = self.dynamic(false)?;
                    self.semi()?;
                    let dynamic = Some((query, using));
                    let text = String::new();
                    return Ok(S::Open { var: v, query: None, dynamic, args: vec![], scroll, constant, text });
                }
                let (toks, into) = self.sql_tokens(false)?;
                let text = std::mem::take(&mut self.text);
                let q = self.statement(toks)?;
                if into.is_some() || !matches!(q, Stmt::Select(_) | Stmt::Locking(..)) {
                    return err(SYNTAX, "a cursor's query must be a SELECT");
                }
                self.semi()?;
                Ok(S::Open { var: v, query: Some(Box::new(q)), dynamic: None, args: vec![], scroll, constant, text })
            }
            _ => {
                // (a direction's word, unless it is the cursor's own name: then INTO or the end follows it)
                let named = |me: &Self| {
                    matches!(me.p.t.get(me.p.i + 1), Some(Tok::Word(x)) if x == "into")
                        || me.p.t.get(me.p.i + 1) == Some(&Tok::Sym(";"))
                };
                let word = self.word().map(String::from).unwrap_or_default();
                let go = if named(self) {
                    Go::Next
                } else {
                    match word.as_str() {
                        "next" | "prior" | "first" | "last" => {
                            self.p.i += 1;
                            match word.as_str() {
                                "next" => Go::Next,
                                "prior" => Go::Prior,
                                "first" => Go::First,
                                _ => Go::Last,
                            }
                        }
                        "absolute" | "relative" => {
                            self.p.i += 1;
                            let n = self.count_expr()?;
                            if word == "absolute" { Go::Abs(n) } else { Go::Rel(n) }
                        }
                        "forward" | "backward" | "all" => {
                            self.p.i += 1;
                            let back = word == "backward";
                            let all = word == "all" || self.p.kw("all");
                            let n = if all || self.p.is_kw("from") || self.p.is_kw("in") || named(self) {
                                None
                            } else {
                                Some(self.count_expr()?)
                            };
                            Go::Many { back, n, all }
                        }
                        _ => Go::Next,
                    }
                };
                let _ = self.p.kw("from") || self.p.kw("in");
                let v = var(self)?;
                let into = if w == "fetch" {
                    self.p.expect_kw("into")?;
                    let mut names = vec![];
                    loop {
                        let mut n = self.p.name()?;
                        if self.p.sym(".") {
                            n = format!("{n}.{}", self.p.name()?);
                        }
                        names.push(n);
                        if !self.p.sym(",") {
                            break;
                        }
                    }
                    Some(self.targets(names)?)
                } else {
                    None
                };
                self.semi()?;
                Ok(S::Fetch { var: v, go, into })
            }
        }
    }

    /// A bound cursor's arguments after its name in OPEN and FOR, `(value, name := value, ...)`, as Postgres reads
    /// them: a value written by position is its place's parameter's; by parameter, in order.
    fn cursor_args(&mut self, cur: &Cur) -> Result<Vec<(String, Type, E)>> {
        let c = &cur.name;
        if cur.params.is_empty() {
            if self.p.peek() == Some(&Tok::Sym("(")) {
                return err(SYNTAX, format!("cursor \"{c}\" has no arguments"));
            }
            return Ok(vec![]);
        }
        if !self.p.sym("(") {
            return err(SYNTAX, format!("cursor \"{c}\" has arguments"));
        }
        let mut given: Vec<Option<E>> = (0..cur.params.len()).map(|_| None).collect();
        let mut k = 0;
        loop {
            let named = matches!(self.p.peek(), Some(Tok::Word(_) | Tok::Ident(_)))
                && matches!(self.p.t.get(self.p.i + 1), Some(Tok::Sym(":=" | "=>")));
            let at = if named {
                let n = self.p.name()?;
                self.p.i += 1;
                match cur.params.iter().position(|(p, _)| *p == n) {
                    Some(at) => at,
                    None => return err(SYNTAX, format!("cursor \"{c}\" has no argument named \"{n}\"")),
                }
            } else {
                k
            };
            if at >= cur.params.len() {
                return err(SYNTAX, format!("too many arguments for cursor \"{c}\""));
            }
            if given[at].is_some() {
                let p = &cur.params[at].0;
                return err(SYNTAX, format!("value for parameter \"{p}\" of cursor \"{c}\" specified more than once"));
            }
            given[at] = Some(self.expr()?);
            k += 1;
            if !self.p.sym(",") {
                break;
            }
        }
        self.p.expect_sym(")")?;
        let mut out = vec![];
        for ((n, t), e) in cur.params.iter().zip(given) {
            let Some(e) = e else { return err(SYNTAX, format!("not enough arguments for cursor \"{c}\"")) };
            out.push((n.clone(), *t, e));
        }
        Ok(out)
    }

    fn execute(&mut self) -> Result<S> {
        let (query, into, using) = self.dynamic(true)?;
        self.semi()?;
        Ok(S::Execute { query, into, using })
    }
}

// ---------------------------------------------------------------- running

enum Flow {
    Next,
    /// EXIT [label]: out of the innermost loop, or of the block or loop labeled so
    Exit(Option<String>),
    Continue(Option<String>),
    Return(Returned),
}

struct Var {
    typmod: Option<(u32, i32)>,
    not_null: bool,
    /// declared as this domain (its stored name): what it is set to is held to the domain's constraints
    domain: Option<String>,
}

struct Frame<'a> {
    x: &'a Exec<'a>,
    f: &'a Arc<Function>,
    /// the variables, as columns of the row expressions run over; a record's fields `rec.field`
    cols: Vec<Col>,
    vals: Vec<Value>,
    types: Vec<Option<Type>>,
    vars: Vec<Var>,
    /// record variables, and whether each holds a row yet
    records: Vec<(String, bool)>,
    /// the rows RETURN NEXT and RETURN QUERY gave so far
    rows: Vec<Vec<Value>>,
    row_count: usize,
    /// a hash of the variables' names: statements made for them are kept by it, per statement
    /// run (see `query`)
    shape: u64,
    /// the errors being handled, the innermost last (see `guarded`)
    caught: Vec<Caught>,
    /// the cursors it has open, by portal name, where no session keeps them (see `portals`)
    cursors: HashMap<String, Cursor>,
    /// how many cursors it has opened without a name of their own, where no session counts them
    portals: u64,
    /// the statement running, as an error's CONTEXT names it: its line and what it is (`at RAISE`), Postgres's
    /// err_stmt (shared with `CALLS`, which PG_CONTEXT reads)
    at: At,
    /// the error coming out has this function's CONTEXT line already (a RAISE; of one it caught)
    given: bool,
    /// the label of the loop about to run, from the `S::Labeled` around it, which the loop takes as it starts
    label: Option<String>,
    /// the session's plpgsql settings, read when first wanted (see `Opts`), and the function's own `#` options
    opts: std::cell::OnceCell<Opts>,
    options: (Option<Conflict>, Option<bool>),
    /// the values the last EXECUTE was given by USING ($1, ...), for print_strict_params
    using: Vec<Value>,
    /// whether an expression with subqueries (by `pl_exprs`'s key) has one giving a regproc, or one without (by where it
    /// is alone) calls a function giving one (see `typed`)
    procs: HashMap<(usize, u64), bool>,
    /// a variable is an enum, a regproc, a smallint or an oid: expressions then run bound (see `eval`)
    bound: bool,
    /// the records the function's expressions use whole: each has a variable `*name`, its fields
    /// as a JSON object (NULL while it holds no row), made afresh before an expression or a
    /// statement runs (see `sync_whole`)
    whole: Vec<String>,
}

/// Runs a PL/pgSQL function on its arguments (coerced to their types).
pub(super) fn run(x: &Exec, f: &Arc<Function>, b: &Block, args: Vec<Value>, trig: Option<Trig>) -> Result<Returned> {
    // (a DO block is compiled as it runs: what plpgsql.extra_warnings says of it comes now)
    if f.name == "inline_code_block" {
        created(b)?;
    }
    let fx = x.with_params(&[], &[], f.volatility != Volatility::Volatile);
    let mut fr = Frame {
        x: &fx,
        f,
        cols: vec![],
        vals: vec![],
        types: vec![],
        vars: vec![],
        records: vec![],
        rows: vec![],
        row_count: 0,
        shape: 0,
        caught: vec![],
        cursors: HashMap::new(),
        portals: 0,
        at: Rc::new(std::cell::Cell::new(None)),
        given: false,
        label: None,
        opts: std::cell::OnceCell::new(),
        options: (b.conflict, b.strict_params),
        using: vec![],
        procs: HashMap::new(),
        bound: false,
        whole: b.whole.clone(),
    };
    for (i, ((n, t), v)) in f.args.iter().zip(args).enumerate() {
        fr.declare(n.clone().unwrap_or_else(|| format!("${}", i + 1)), x.resolved(*t)?, v, None, false);
        // (a parameter of a domain: the call held its value to the domain; what it is set to here is held too)
        if let Type::Enum(_) = t {
            let d = super::domain::of_type(&x.view()?, *t)?;
            if let Some(last) = fr.vars.last_mut() {
                last.domain = d;
            }
        }
    }
    fr.declare("found".into(), Type::Bool, Value::Bool(false), None, false);
    if let Ret::Table(cols) = &f.ret {
        for (n, t) in cols {
            if f.outs && f.args.iter().any(|a| a.0.as_deref() == Some(n.as_str())) {
                continue; // an INOUT parameter, declared with the arguments
            }
            fr.declare(n.clone(), x.resolved(*t)?, Value::Null, None, false);
        }
    }
    if let Some((ev, tag)) = super::userdefs::EVENT.with(|e| e.borrow().clone()) {
        fr.declare("tg_event".into(), Type::Text, Value::Text(ev), None, false);
        fr.declare("tg_tag".into(), Type::Text, Value::Text(tag), None, false);
    }
    if let Some(t) = &trig {
        let fields: Vec<(String, Type)> = t.table.cols.iter().map(|c| (c.name.clone(), c.ty)).collect();
        for (r, vals) in [("new", t.new), ("old", t.old)] {
            // one that isn't there (OLD in an INSERT) is NULL, and so are its fields
            fr.records.push((r.into(), false));
            fr.set_record(r, &fields, vals.map_or_else(|| vec![Value::Null; fields.len()], <[Value]>::to_vec));
            fr.records.last_mut().unwrap().1 = vals.is_some();
        }
        let text = |s: &str| Value::Text(s.into());
        let x = t.trigger;
        let vars = [
            ("tg_name", text(&x.name)),
            (
                "tg_when",
                text(if x.instead {
                    "INSTEAD OF"
                } else if x.before {
                    "BEFORE"
                } else {
                    "AFTER"
                }),
            ),
            ("tg_level", text(if x.row { "ROW" } else { "STATEMENT" })),
            ("tg_op", text(t.event.name())),
            ("tg_table_name", text(super::rel_part(&t.table.name))),
            ("tg_relname", text(super::rel_part(&t.table.name))),
            ("tg_table_schema", text(super::schema_part(&t.table.name))),
            ("tg_nargs", Value::Int(x.args.len() as i64)),
            (
                "tg_argv",
                Value::Text(array_text(x.args.iter().map(|a| Value::Text(a.clone())).collect::<Vec<_>>().iter())),
            ),
        ];
        // TG_RELID, the table's oid, for a function that names it: it is worked out from every relation's name
        // (and a variable of type oid makes the function's expressions run bound: see `changed`)
        if f.body.to_ascii_lowercase().contains("tg_relid") {
            let oid = super::hier::oids(&fr.x.view()?, &[t.table.name.as_str()])?.first().copied().unwrap_or(0);
            fr.declare("tg_relid".into(), Type::Oid, Value::Int(oid), None, false);
        }
        for (n, v) in vars {
            let ty = if n == "tg_nargs" {
                Type::Int
            } else if n == "tg_argv" {
                Type::Array(25)
            } else {
                Type::Text
            };
            fr.declare(n.into(), ty, v, None, false);
        }
    }
    fr.at.set(Some((b.line, "at statement block")));
    CALLS.with_borrow_mut(|c| c.push((f.clone(), fr.at.clone())));
    let ran = fr.block(b);
    CALLS.with_borrow_mut(|c| {
        c.pop();
    });
    let out = match ran {
        Err(e) => Err(e),
        // a result of a domain is held to the domain's constraints, as Postgres casts it to the function's type
        Ok(Flow::Return(Returned::Value(v))) if matches!(f.ret, Ret::Value(Type::Enum(_))) => {
            let Ret::Value(t) = f.ret else { unreachable!("matched a value") };
            match x.view().and_then(|view| super::domain::check_type(&view, t, &v, Some(x as &dyn Runner))) {
                Ok(()) => Ok(Returned::Value(v)),
                Err(e) => {
                    fr.given = true;
                    let line = format!(
                        "PL/pgSQL function {} while casting return value to function's return type",
                        signature(fr.f)
                    );
                    Err(with_context(e, &line))
                }
            }
        }
        Ok(Flow::Return(r)) => Ok(r),
        Ok(_) => {
            fr.at.set(None); // (said of the function, at no line)
            match &f.ret {
                Ret::Table(_) if f.outs => Ok(Returned::Rows(vec![fr.out_row()])),
                Ret::SetOf(_) | Ret::Table(_) | Ret::RowsOf(_) => Ok(Returned::Rows(std::mem::take(&mut fr.rows))),
                Ret::Void => Ok(Returned::Value(Value::Text(String::new()))), // the void value, not NULL
                Ret::Trigger => err("2F005", "control reached end of trigger procedure without RETURN"),
                Ret::Value(_) => err("2F005", "control reached end of function without RETURN"),
            }
        }
    };
    out.map_err(|e| fr.context(e))
}

/// `assigned` as PL/pgSQL assigns (to a variable, or a function's result): where SQL has no conversion from a
/// number's or a boolean's type to the variable's, its text is read as the variable's type, as Postgres falls back on
/// an I/O conversion there (`1` into a boolean is true; `20150717` into a date is 2015-07-17). Not where a CREATE CAST
/// made one, which Postgres would use and Chronos doesn't here, nor into a row, which Postgres refuses otherwise.
fn pl_assigned(v: Value, from: Option<Type>, to: Type) -> Result<Value> {
    let (text, types): (Option<String>, &[Type]) = match &v {
        Value::Int(n) => (Some(n.to_string()), &[Type::SmallInt, Type::Int, Type::BigInt][..]),
        Value::Numeric(_) => (Some(v.to_text()), &[Type::Numeric][..]),
        Value::Bool(b) => (Some(if *b { "t" } else { "f" }.to_string()), &[Type::Bool][..]),
        _ => (None, &[][..]),
    };
    match assigned(v, from, to) {
        Err(e) if e.code == WRONG_TYPE && e.message.starts_with("can't use ") => {
            let cast = types.iter().any(|t| super::userdefs::cast(*t, to).is_some());
            match text {
                Some(t) if !cast && !matches!(to, Type::Record(_)) => coerce(Value::Text(t), to),
                _ => Err(e),
            }
        }
        r => r,
    }
}

/// A value as print_strict_params lists it: its text quoted (quotes in it doubled), NULL bare.
fn quoted_param(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => format!("'{}'", if *b { "t" } else { "f" }),
        v => format!("'{}'", v.to_text().replace('\'', "''")),
    }
}

/// `e` with `line` after its CONTEXT lines (the innermost first, as Postgres collects them).
fn with_context(mut e: SqlError, line: &str) -> SqlError {
    if line.is_empty() {
        return e;
    }
    let info = e.info.get_or_insert_with(Default::default);
    info.context = Some(match info.context.take() {
        Some(c) if !c.is_empty() => format!("{c}\n{line}"),
        _ => line.to_string(),
    });
    e
}

/// What the running statement reaches of its session: where the cursors PL/pgSQL opens are kept (see
/// `SessState::portals`), none for a statement run without one.
fn session() -> Option<Arc<Ctx>> {
    super::STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone()))
}

impl Frame<'_> {
    /// `f` on the open cursor (portal) `name`, None if there is none: the session's, which outlive the call that
    /// opened them until the transaction ends, or the function's own where no session keeps them.
    fn portal<T>(&mut self, name: &str, f: impl FnOnce(&mut Cursor) -> T) -> Option<T> {
        match session() {
            Some(ctx) => {
                let mut st = ctx.state.lock().unwrap();
                st.portals.get_mut(name).map(f)
            }
            None => self.cursors.get_mut(name).map(f),
        }
    }

    /// Whether cursor name `name` is in use: a portal open, or a cursor the session DECLAREd (one namespace,
    /// as Postgres's portals are).
    fn in_use(&mut self, name: &str) -> bool {
        self.portal(name, |_| ()).is_some() || session().is_some_and(|c| c.cursors.iter().any(|n| n == name))
    }

    /// Opens cursor `name` on a query's rows, in the savepoint level the statement runs at (a ROLLBACK TO an
    /// older one closes it, as Postgres closes the portals a rolled-back subtransaction opened).
    fn open(&mut self, name: String, out: Output, scroll: Option<bool>, text: &str) {
        let (depth, tx) = session().map_or((0, false), |c| (c.depth, c.tx.is_some()));
        let c = Cursor {
            text: Arc::from(text),
            at: super::now_us(),
            columns: out.columns.unwrap_or_default(),
            typmods: out.typmods,
            rows: out.rows,
            pos: 0,
            hold: false,
            scroll,
            depth,
            tx,
        };
        match session() {
            Some(ctx) => {
                ctx.state.lock().unwrap().portals.insert(name, c);
            }
            None => {
                self.cursors.insert(name, c);
            }
        }
    }

    /// Closes cursor `name`; None if it isn't open.
    fn close(&mut self, name: &str) -> Option<()> {
        match session() {
            Some(ctx) => {
                let mut st = ctx.state.lock().unwrap();
                st.portals.remove(name).map(|_| ())
            }
            None => self.cursors.remove(name).map(|_| ()),
        }
    }

    /// A new portal's name, `<unnamed portal n>`, counted through the session as Postgres counts them.
    fn unnamed(&mut self) -> String {
        let n = match session() {
            Some(ctx) => {
                let mut st = ctx.state.lock().unwrap();
                st.portal_no += 1;
                st.portal_no
            }
            None => {
                self.portals += 1;
                self.portals
            }
        };
        format!("<unnamed portal {n}>")
    }

    /// `f` with a bound cursor's arguments set: each a variable of its parameter's name and type, the values worked
    /// out first (so `p1 := p1` reads the function's own `p1`).
    fn with_args<T>(&mut self, args: &[(String, Type, E)], f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if args.is_empty() {
            return f(self);
        }
        let mut vals = vec![];
        for (n, t, e) in args {
            vals.push((n.clone(), self.x.resolved(*t)?, self.eval(e)?));
        }
        let mut mine = vec![];
        let out = (|| {
            for (n, t, v) in vals {
                let hidden = self.declare(n.clone(), t, Value::Null, None, false);
                mine.push((n.clone(), hidden));
                self.set(&n, v)?;
            }
            f(self)
        })();
        self.leave(mine);
        out
    }

    /// This function's CONTEXT line for an error now, as Postgres's plpgsql_exec_error_callback writes it.
    fn context_line(&self) -> String {
        context_line(self.f, self.at.get())
    }

    /// The session's plpgsql settings for this call (see `Opts`).
    fn opts(&self) -> &Opts {
        self.opts.get_or_init(|| Opts::read(self.options))
    }

    /// An error leaving the function, its CONTEXT line added (unless a RAISE; gave it one already).
    fn context(&mut self, e: SqlError) -> SqlError {
        if std::mem::take(&mut self.given) {
            return e;
        }
        let line = self.context_line();
        with_context(e, &line)
    }

    /// An error out of statement `text` that ran (not a syntax error, which points into it): the statement in its
    /// CONTEXT, as Postgres's SPI says it, before the function's own line.
    fn statement_context(e: SqlError, text: &str) -> SqlError {
        if text.is_empty() || e.info.as_ref().is_some_and(|i| i.position.is_some()) {
            return e;
        }
        let shown = format!("SQL statement \"{text}\"");
        with_context(e, &shown)
    }

    /// An OUT-parameter function's one row: its result variables' values.
    fn out_row(&self) -> Vec<Value> {
        let Ret::Table(cols) = &self.f.ret else { return vec![] };
        cols.iter().map(|(n, _)| self.find(n).map_or(Value::Null, |i| self.vals[i].clone())).collect()
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.name == name)
    }

    fn changed(&mut self) {
        // (a smallint's arithmetic is checked to 16 bits, and an integer compared with an oid converted to one, only
        // where the variables are typed: bound)
        // (and an hstore's or an ltree's operators are found by their operands' types)
        self.bound = (self.cols.iter()).any(|c| {
            matches!(c.ty, Type::Enum(_) | Type::SmallInt | Type::Oid | Type::Hstore | Type::Ltree | Type::Lquery)
                || c.ty.reg_like()
        });
        self.shape = 0; // worked out again when a statement needs it (see `settle`)
    }

    /// The variables' shape and types, after they changed.
    fn settle(&mut self) {
        use std::hash::{Hash, Hasher};
        if self.shape != 0 {
            return;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.cols.iter().for_each(|c| c.name.hash(&mut h));
        self.shape = h.finish() | 1;
        self.types = self.cols.iter().map(|c| Some(c.ty)).collect();
    }

    /// A new variable, hiding any of the same name until its block ends. Returns what to undo.
    fn declare(
        &mut self,
        name: String,
        ty: Type,
        v: Value,
        typmod: Option<(u32, i32)>,
        not_null: bool,
    ) -> Option<String> {
        // (named by how many of the name are hidden already, as the parser names one a label reaches: see `hidden_as`)
        let hidden = match self.find(&name) {
            Some(i) => {
                let h = hidden_as(&name, self.hidden_count(&name));
                self.cols[i].name = h.clone();
                Some(h)
            }
            None => None,
        };
        self.cols.push(Col::new("", &name, ty));
        self.vals.push(v);
        self.vars.push(Var { typmod, not_null, domain: None });
        self.changed();
        hidden
    }

    /// How many variables (and records) of name `name` are hidden now (see `hidden_as`).
    fn hidden_count(&self, name: &str) -> usize {
        let of = |n: &str| {
            n.strip_prefix(name)
                .and_then(|r| r.strip_prefix('\u{1}'))
                .is_some_and(|k| !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()))
        };
        self.cols.iter().filter(|c| of(&c.name)).count() + self.records.iter().filter(|(x, _)| of(x)).count()
    }

    /// A new record variable `name`, hiding one of the same name (and its fields) until its block ends, as `declare`
    /// hides a variable. Returns what to undo.
    fn declare_record(&mut self, name: &str) -> Option<String> {
        let hidden = match self.records.iter().rposition(|(x, _)| x == name) {
            Some(r) => {
                let h = hidden_as(name, self.hidden_count(name));
                self.records[r].0 = h.clone();
                self.rename_fields(name, &h);
                self.changed();
                Some(h)
            }
            None => None,
        };
        self.records.push((name.to_string(), false));
        hidden
    }

    /// Record `from`'s fields named as record `to`'s.
    fn rename_fields(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}.");
        for c in &mut self.cols {
            if let Some(f) = c.name.strip_prefix(&prefix) {
                c.name = format!("{to}.{f}");
            }
        }
    }

    /// Ends variables `names` (each with the variable it hid, if any).
    fn leave(&mut self, names: Vec<(String, Option<String>)>) {
        for (n, hidden) in names.into_iter().rev() {
            if let Some(r) = self.records.iter().rposition(|(x, _)| *x == n) {
                self.records.remove(r);
                self.drop_fields(&n);
            }
            if let Some(i) = self.find(&n) {
                self.cols.remove(i);
                self.vals.remove(i);
                self.vars.remove(i);
            }
            if let Some(h) = hidden {
                if let Some(i) = self.find(&h) {
                    self.cols[i].name = n.clone();
                }
                if let Some(r) = self.records.iter().rposition(|(x, _)| *x == h) {
                    self.records[r].0 = n.clone();
                    self.rename_fields(&h, &n);
                }
            }
        }
        self.changed();
    }

    fn drop_fields(&mut self, r: &str) {
        let prefix = format!("{r}.");
        while let Some(i) = self.cols.iter().position(|c| c.name.starts_with(&prefix)) {
            self.cols.remove(i);
            self.vals.remove(i);
            self.vars.remove(i);
        }
    }

    /// Record `r` given a row: its fields `fields`, their values `vals`.
    fn set_record(&mut self, r: &str, fields: &[(String, Type)], vals: Vec<Value>) {
        let prefix = format!("{r}.");
        let now: Vec<usize> = (0..self.cols.len()).filter(|&i| self.cols[i].name.starts_with(&prefix)).collect();
        let same = now.len() == fields.len()
            && now
                .iter()
                .zip(fields)
                .all(|(&i, (n, t))| self.cols[i].name[prefix.len()..] == **n && self.cols[i].ty == *t);
        if same {
            for (&i, v) in now.iter().zip(vals) {
                self.vals[i] = v;
            }
        } else {
            self.drop_fields(r);
            for ((n, t), v) in fields.iter().zip(vals) {
                self.cols.push(Col::new("", &format!("{r}.{n}"), *t));
                self.vals.push(v);
                self.vars.push(Var { typmod: None, not_null: false, domain: None });
            }
            self.changed();
        }
        if let Some(x) = self.records.iter_mut().rev().find(|(x, _)| x == r) {
            x.1 = true;
        }
    }

    /// Brings the whole-record variables (`*name`, see `whole`) up to their records' fields.
    fn sync_whole(&mut self) {
        for k in 0..self.whole.len() {
            let r = self.whole[k].clone();
            let prefix = format!("{r}.");
            let held = self.records.iter().rev().find(|(x, _)| *x == r).is_some_and(|x| x.1);
            let fields: Vec<(String, &Value)> = (self.cols.iter().zip(&self.vals))
                .filter(|(c, _)| c.name.starts_with(&prefix))
                .map(|(c, v)| (c.name[prefix.len()..].to_string(), v))
                .collect();
            // null as a row is: it holds none, or every field is null; not null: it holds one with no null field
            let null = !held || fields.iter().all(|(_, v)| **v == Value::Null);
            let not_null = held && fields.iter().all(|(_, v)| **v != Value::Null);
            // its text as Postgres's record_out writes it, `(3,"a b",)`
            let vals: Vec<Value> = fields.iter().map(|(_, v)| Value::clone(v)).collect();
            let row = held.then(|| super::composite_text(&vals));
            let object = fields.into_iter().map(|(n, v)| (n, v.to_json())).collect();
            let v = if held { Value::Json(Json::Object(object)) } else { Value::Null };
            self.hidden(format!("*{r}"), Type::Json, v);
            self.hidden(format!("*{r}{ROW_TEXT}"), Type::Text, row.map_or(Value::Null, Value::Text));
            self.hidden(format!("*{r}{NULL_ROW}"), Type::Bool, Value::Bool(null));
            self.hidden(format!("*{r}{NOT_NULL_ROW}"), Type::Bool, Value::Bool(not_null));
        }
    }

    /// Sets the variable `name` that no declaration made (a whole record's), making it first if it isn't there.
    fn hidden(&mut self, name: String, ty: Type, v: Value) {
        match self.cols.iter().position(|c| c.name == name) {
            Some(i) => self.vals[i] = v,
            None => {
                self.cols.push(Col::new("", &name, ty));
                self.vals.push(v);
                self.vars.push(Var { typmod: None, not_null: false, domain: None });
                self.changed();
            }
        }
    }

    /// The row a trigger function returned as the text of a record, `(1,"a b",)`: its fields as the triggering
    /// table's columns' types (what NEW's fields are variables of), as Postgres's record_in reads it.
    fn trigger_row(&self, text: &str) -> Result<Vec<Value>> {
        let fields: Vec<(String, Type)> =
            (self.cols.iter()).filter_map(|c| c.name.strip_prefix("new.").map(|f| (f.to_string(), c.ty))).collect();
        super::record_in(text, &fields)
    }

    /// Record `r`'s values, None if it holds no row.
    fn record(&self, r: &str) -> Option<Vec<Value>> {
        if !self.records.iter().rev().find(|(x, _)| x == r)?.1 {
            return None;
        }
        let prefix = format!("{r}.");
        Some(
            self.cols
                .iter()
                .zip(&self.vals)
                .filter(|(c, _)| c.name.starts_with(&prefix))
                .map(|(_, v)| v.clone())
                .collect(),
        )
    }

    /// The error for a variable that isn't there: `rec.field` of a record without a row, or
    /// without that field.
    fn missing(&self, n: &str) -> SqlError {
        let e = |code, message| SqlError { code, message, info: None };
        let shown_as = shown(n);
        let n = shown_as.as_str();
        match n.split_once('.') {
            Some((r, _)) if self.records.iter().any(|(x, has)| x == r && !has) => {
                e("55000", format!("record \"{r}\" is not assigned yet"))
            }
            Some((r, f)) => e(NO_COLUMN, format!("record \"{r}\" has no field \"{f}\"")),
            None => e(NO_COLUMN, format!("column \"{n}\" does not exist")),
        }
    }

    /// Sets a variable, converting the value to its type (and rounding to numeric(p, s)).
    fn set(&mut self, n: &str, v: Value) -> Result<()> {
        self.set_from(n, v, None)
    }

    /// `set`, the value of type `from`: a regproc set to text is its function's name.
    fn set_from(&mut self, n: &str, v: Value, from: Option<Type>) -> Result<()> {
        let Some(i) = self.find(n) else { return Err(self.missing(n)) };
        let mut v = pl_assigned(v, from, self.cols[i].ty)?;
        if let (Value::Numeric(d), Some((_, s))) = (&v, self.vars[i].typmod) {
            v = Value::Numeric(round_dec(d, s)?);
        }
        if v == Value::Null && self.vars[i].not_null {
            let n = shown(n);
            return err("22004", format!("null value cannot be assigned to variable \"{n}\" declared NOT NULL"));
        }
        // a domain's variable takes only what the domain takes (its constraints as they are now)
        if let Some(d) = self.vars[i].domain.clone() {
            super::domain::check(&self.x.view()?, &d, &v, Some(self.x as &dyn Runner))?;
        }
        self.vals[i] = v;
        // a field set in a record of known fields that holds no row (NEW in a DELETE trigger) makes it hold one, its
        // other fields null, as in Postgres
        if let Some((r, _)) = n.split_once('.')
            && let Some(x) = self.records.iter_mut().rev().find(|(x, _)| x == r)
        {
            x.1 = true;
        }
        Ok(())
    }

    fn eval(&mut self, e: &E) -> Result<Value> {
        self.typed(e).map(|(v, _)| v)
    }

    /// `e`'s value, and its type when that is regproc or regprocedure (a function's oid, which is the function's
    /// name where text is wanted: see `proc_text`).
    fn typed(&mut self, e: &E) -> Result<(Value, Option<Type>)> {
        if !self.whole.is_empty() {
            self.sync_whole();
        }
        if self.bound || e.bound {
            return self.bound_eval(e);
        }
        if !e.query {
            if self.calls_proc(e) {
                return self.bound_eval(e);
            }
            let env = Env { cols: &self.cols, row: &self.vals, params: &self.vals, aggs: &[], sub: Some(self.x) };
            let v = super::eval(&e.e, &env).map_err(|er| self.better(&e.e, er))?;
            return Ok((v, self.proc_of(e)));
        }
        self.query_eval(e)
    }

    /// `typed` for an expression with subqueries. Apart from it, whose frame every nested call takes.
    #[inline(never)]
    fn query_eval(&mut self, e: &E) -> Result<(Value, Option<Type>)> {
        // a subquery of a table with row security (or of a view) runs as a statement, which has its
        // policies put in (and its views expanded) for whoever runs it each time
        if self.x.db.core.has_row_security() && rls::touched(&self.x.view()?, &e.sel)? {
            return self.bound_eval(e);
        }
        self.settle();
        let key = (&e.e as *const Expr as usize, self.shape);
        let found = self.x.nest.pl_exprs.borrow().get(&key).map(|x| x.1.clone());
        let ex = match found {
            Some(x) => x,
            None => {
                // names its subqueries' own columns could mean, as for a statement (see `query`)
                let (v, mut cols, mut subs) = (self.x.view()?, HashSet::new(), vec![]);
                walk(&e.e, &mut |x| {
                    if let Expr::Sub(sq) | Expr::Exists(sq, _) | Expr::InSub(_, sq, _) = x {
                        subs.push(Stmt::Select(sq.q.clone()));
                    }
                });
                for s in &subs {
                    cols.extend(functions::column_names(&v, s)?);
                }
                let mut x = e.e.clone();
                self.params_for(&mut x, &cols)?;
                let x = Rc::new(x);
                self.x.nest.pl_exprs.borrow_mut().insert(key, (self.f.clone(), x.clone()));
                x
            }
        };
        // a subquery giving a regproc: the whole runs bound, so the regproc is a name where text is wanted
        let procs = match self.procs.get(&key) {
            Some(&p) => p,
            None => {
                let (v, mut p) = (self.x.view()?, false);
                walk(&ex, &mut |x| {
                    if let Expr::Sub(sq) = x {
                        let cols = select_columns(&v, &sq.q, &self.types).unwrap_or_default();
                        p |= cols.iter().any(|c| matches!(c.1, Type::Reg(_)));
                    }
                });
                self.procs.insert(key, p);
                p
            }
        };
        if procs {
            return self.bound_eval(e);
        }
        Ok((self.x.with_params(&self.vals, &self.types, self.x.readonly).constant(&ex)?, None))
    }

    /// Whether `e` calls a function made here that returns a regproc: it then runs bound (see `typed`). Apart from
    /// `typed`, as `query_eval` is.
    #[inline(never)]
    fn calls_proc(&mut self, e: &E) -> bool {
        let key = (&e.e as *const Expr as usize, 0);
        if let Some(&p) = self.procs.get(&key) {
            return p;
        }
        let mut calls = vec![];
        walk(&e.e, &mut |x| {
            if let Expr::Call(c) = x {
                calls.push((c.name.clone(), c.args.len()));
            }
        });
        let proc = |f: &Arc<Function>| matches!(f.ret, Ret::Value(Type::Reg(_)));
        let p =
            calls.iter().any(|(n, k)| self.x.functions(n).is_ok_and(|fs| fs.iter().any(|f| f.takes(*k) && proc(f))));
        self.procs.insert(key, p);
        p
    }

    /// `e` run as `SELECT e`, bound as SQL is, and the type of its regproc (see `typed`).
    fn bound_eval(&mut self, e: &E) -> Result<(Value, Option<Type>)> {
        let out = self.query(&e.sel)?;
        let ty = out.columns.as_ref().and_then(|c| c.first()).map(|c| c.1);
        let v = out.rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null);
        Ok((v, ty.filter(|t| t.reg_like())))
    }

    /// A clearer error than "column does not exist" for a record's missing field.
    fn better(&self, e: &Expr, er: SqlError) -> SqlError {
        if er.code != NO_COLUMN {
            return er;
        }
        let mut found = None;
        walk(e, &mut |x| {
            if let Expr::Col(None, n) = x
                && self.find(n).is_none()
            {
                found.get_or_insert_with(|| self.missing(n));
            }
        });
        found.unwrap_or(er)
    }

    /// The regproc type of `e` where that is plain (see `proc_type`): a variable or record field of one, too.
    fn proc_of(&self, e: &E) -> Option<Type> {
        match &e.e {
            Expr::Col(q, n) => {
                let i = self.find(&q.as_ref().map_or_else(|| n.clone(), |q| format!("{q}.{n}")))?;
                Some(self.cols[i].ty).filter(|t| t.reg_like())
            }
            x => proc_type(x, &self.cols),
        }
    }

    /// `n := e`. Apart from `stmt`, whose frame every nested call takes.
    #[inline(never)]
    fn assign_to(&mut self, n: &str, e: &E) -> Result<()> {
        let (v, from) = self.typed(e)?;
        self.set_from(n, v, from)
    }

    /// `RETURN e` (or `RETURN NEXT e`) in a function returning `t`. Apart from `stmt`, as `assign_to` is.
    #[inline(never)]
    fn returned(&mut self, e: &E, t: Type) -> Result<Value> {
        let (v, from) = self.typed(e)?;
        pl_assigned(v, from, self.x.resolved(t)?)
    }

    /// RETURN QUERY's rows are of the function's columns' types: a regproc for a column of another type, or another
    /// type for a regproc's, is 42804, as in Postgres, which converts nothing there. Text for a regproc's is let
    /// through, read as one: an element of an array of regprocs, which Chronos keeps as text[], is text.
    #[inline(never)]
    fn query_returns(&self, out: &Output) -> Result<()> {
        let to: Vec<Type> = match &self.f.ret {
            Ret::SetOf(t) => vec![*t],
            Ret::Table(cols) => cols.iter().map(|c| c.1).collect(),
            _ => return Ok(()),
        };
        let proc = |t: Type| matches!(t, Type::Reg(_));
        for (i, (c, t)) in out.columns.iter().flatten().zip(&to).enumerate() {
            if (proc(c.1) || proc(*t)) && c.1 != *t && !(c.1 == Type::Text && proc(*t)) {
                return err(
                    "42804",
                    format!(
                        "structure of query does not match function result type: returned type {} does not match expected type {} in column {}",
                        c.1.pg_name(),
                        t.pg_name(),
                        i + 1
                    ),
                );
            }
        }
        Ok(())
    }

    /// `e`'s value where text is wanted (a RAISE's message and options): a regproc is its function's name, a boolean
    /// `t` or `f`, as their output functions write them.
    fn text(&mut self, e: &E) -> Result<Value> {
        let (v, from) = self.typed(e)?;
        Ok(match proc_text(from, v)? {
            Value::Bool(b) => Value::Text(if b { "t" } else { "f" }.into()),
            v => v,
        })
    }

    fn cond(&mut self, e: &E) -> Result<bool> {
        Ok(truth(&self.eval(e)?)? == Some(true))
    }

    /// Variables in `x` made parameters (`$1` for the first); a name that's also one of `cols`
    /// is read as plpgsql.variable_conflict (or the function's `#variable_conflict`) says: an error, as in Postgres
    /// by default, the variable, or the column.
    fn params_for(&self, x: &mut Expr, cols: &HashSet<String>) -> Result<()> {
        functions::visit_expr(x, &mut |e| self.param(e, cols), &mut |_| {})
    }

    fn param(&self, e: &mut Expr, cols: &HashSet<String>) -> Result<()> {
        if let Expr::Col(None, n) = e {
            let conflict = if cols.contains(n) && self.find(n).is_some() { Some(self.opts().conflict) } else { None };
            match conflict {
                Some(Conflict::Error) => {
                    return Err(SqlError {
                        code: "42702",
                        message: format!("column reference \"{n}\" is ambiguous"),
                        info: Some(Box::new(Violation {
                            detail: Some("It could refer to either a PL/pgSQL variable or a table column.".into()),
                            ..Violation::default()
                        })),
                    });
                }
                Some(Conflict::UseColumn) => return Ok(()),
                _ => {}
            }
            match self.find(n) {
                // an enum's type goes with it, so it compares by its labels' places, a regproc's (or regproc[]'s), so it
                // is text as its function's name, text's, so it is no string literal a regproc reads as an oid, and a
                // smallint's and an oid's, so their operators are theirs
                Some(i) => match self.cols[i].ty {
                    t if t.reg_like() || matches!(t, Type::Enum(_) | Type::Text | Type::SmallInt | Type::Oid) => {
                        *e = Expr::Cast(Box::new(Expr::Param(i + 1)), t)
                    }
                    _ => *e = Expr::Param(i + 1),
                },
                None if n.contains('.') => return Err(self.missing(n)),
                None => {}
            }
        }
        Ok(())
    }

    /// MERGE keeps its clauses as text (see `Exec::merge_into`): the variables named in them,
    /// and records' fields (`new.f`), are the parameters that hold them. A word set by SET
    /// (`a = ...` first or after a comma) is a column.
    fn merge_vars(&self, m: &mut MergeStmt) -> Result<()> {
        let fix = |text: &mut String| -> Result<()> {
            let toks = lex(text)?;
            let mut out = Vec::with_capacity(toks.len());
            let mut i = 0;
            while i < toks.len() {
                let prev = i.checked_sub(1).map(|k| &toks[k]);
                let next = toks.get(i + 1);
                if let Tok::Word(w) = &toks[i]
                    && !matches!(prev, Some(Tok::Sym(".")))
                    && !matches!(next, Some(Tok::Sym("(")))
                    && !(matches!(next, Some(Tok::Sym("="))) && matches!(prev, None | Some(Tok::Sym(","))))
                {
                    if let (Some(Tok::Sym(".")), Some(Tok::Word(f) | Tok::Ident(f))) = (next, toks.get(i + 2)) {
                        if let Some(k) = self.find(&format!("{w}.{f}")) {
                            out.push(Tok::Param(k + 1));
                            i += 3;
                            continue;
                        }
                    } else if let Some(k) = self.find(w) {
                        out.push(Tok::Param(k + 1));
                        i += 1;
                        continue;
                    }
                }
                out.push(toks[i].clone());
                i += 1;
            }
            *text = tokens_sql(&out);
            Ok(())
        };
        fix(&mut m.source)?;
        fix(&mut m.on)?;
        for w in &mut m.whens {
            if let Some(c) = &mut w.cond {
                fix(c)?;
            }
            match &mut w.action {
                MergeAction::Update(set, _) => fix(set)?,
                MergeAction::Insert(_, values) if values != MERGE_DEFAULTS => fix(values)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// `rec.*` of a record in `st` as its fields, where Postgres puts them: a list's items (`VALUES (NEW.*)`,
    /// `SELECT r.*`, `ROW(OLD.*)`), a row compared field by field (`ROW(OLD.*) = ROW(NEW.*)`, `NEW.* IS NULL`), and
    /// elsewhere a row value of them, whose text is the record's (`NEW.*::text`). Made with the statement for the
    /// variables there are (see `query`), as they are what say a record's fields.
    fn stars(&self, st: &mut Stmt) -> Result<()> {
        let fields = |x: &Expr| -> Option<Vec<Expr>> {
            let Expr::Col(Some(r), c) = x else { return None };
            if c != "*" || !self.records.iter().any(|(n, _)| n == r) {
                return None;
            }
            let prefix = format!("{r}.");
            Some(
                self.cols
                    .iter()
                    .filter(|c| c.name.starts_with(&prefix))
                    .map(|c| Expr::Col(None, c.name.clone()))
                    .collect(),
            )
        };
        let spread = |list: &mut Vec<Expr>| {
            if list.iter().any(|x| fields(x).is_some()) {
                let old = std::mem::take(list);
                *list = old.into_iter().flat_map(|x| fields(&x).unwrap_or_else(|| vec![x])).collect();
            }
        };
        if let Stmt::Insert { rows, .. } = st {
            rows.iter_mut().for_each(&spread);
        }
        let row = |x: &Expr| match fields(x) {
            Some(f) => Expr::Func(Func::RowValue, f),
            None => x.clone(),
        };
        functions::visit(
            st,
            &mut |e| {
                let new = match &*e {
                    Expr::Bin(a, op @ (Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge), b)
                        if fields(a).is_some() || fields(b).is_some() =>
                    {
                        Some(super::row_compare(row(a), *op, row(b))?)
                    }
                    Expr::IsNull(x, neg) => fields(x).map(|f| super::row_null_test(f, *neg)),
                    x @ Expr::Col(..) => fields(x).map(|f| Expr::Func(Func::RowValue, f)),
                    _ => None,
                };
                match new {
                    Some(n) => *e = n,
                    None => {
                        if let Expr::Func(Func::RowValue, args) = e {
                            spread(args);
                        }
                    }
                }
                Ok(())
            },
            &mut |q| {
                // (a record's, not a table's of the query: `t.*` of a table keeps its meaning)
                let record = |i: &Item| match i {
                    Item::TableStar(t) if fields(&Expr::Col(Some(t.clone()), "*".into())).is_some() => Some(t.clone()),
                    _ => None,
                };
                if q.items.iter().any(|i| record(i).is_some()) {
                    let old = std::mem::take(&mut q.items);
                    let mut items = vec![];
                    for i in old {
                        let Some(t) = record(&i) else {
                            items.push(i);
                            continue;
                        };
                        let prefix = format!("{t}.");
                        for c in self.cols.iter().filter(|c| c.name.starts_with(&prefix)) {
                            let alias = c.name[prefix.len()..].to_string();
                            items.push(Item::Expr(Expr::Col(None, c.name.clone()), Some(alias)));
                        }
                    }
                    q.items = items;
                }
            },
        )
    }

    /// Runs a statement of the body, the variables its parameters.
    fn query(&mut self, s: &Stmt) -> Result<Output> {
        if !self.whole.is_empty() {
            self.sync_whole();
        }
        self.settle();
        let key = (s as *const Stmt as usize, self.shape);
        let found = self.x.nest.pl.borrow().get(&key).map(|x| x.1.clone());
        let st = match found {
            Some(x) => x,
            None => {
                let mut st = s.clone();
                if !self.records.is_empty() {
                    self.stars(&mut st)?;
                }
                functions::substitute(&self.x.view()?, &mut st, &mut |e, cols| self.param(e, cols))?;
                if let Stmt::Merge(m) = &mut st {
                    self.merge_vars(m)?;
                }
                let st = Rc::new(st);
                self.x.nest.pl.borrow_mut().insert(key, (self.f.clone(), st.clone()));
                st
            }
        };
        self.x.with_params(&self.vals, &self.types, self.x.readonly).run_body(&st, true)
    }

    fn assign(&mut self, t: &Target, columns: &Option<Columns>, row: Option<Vec<Value>>) -> Result<()> {
        match t {
            Target::Vars(names) => {
                // plpgsql's strict_multi_assignment check: a row's columns one for one with the variables
                if let (Some(cols), Some(_)) = (columns, &row)
                    && cols.len() != names.len()
                {
                    let (error, warn) = (self.opts().error.multi, self.opts().warn.multi);
                    if error || warn {
                        let message = "number of source and target fields in assignment does not match".to_string();
                        let of = if error { "extra_errors" } else { "extra_warnings" };
                        let detail = format!("strict_multi_assignment check of {of} is active.");
                        let hint = "Make sure the query returns the exact list of columns.".to_string();
                        if error {
                            return Err(SqlError {
                                code: "42804",
                                message,
                                info: Some(Box::new(Violation {
                                    detail: Some(detail),
                                    hint: Some(hint),
                                    ..Violation::default()
                                })),
                            });
                        }
                        // (a WARNING for each variable no column is left for, or one for the columns left over)
                        for _ in 0..names.len().saturating_sub(cols.len()).max(1) {
                            super::notice_raised(
                                "WARNING",
                                "42804",
                                message.clone(),
                                Some(detail.clone()),
                                Some(hint.clone()),
                            );
                        }
                    }
                }
                let mut row = row.unwrap_or_default().into_iter();
                for (k, n) in names.iter().enumerate() {
                    let from = columns.as_ref().and_then(|c| c.get(k)).map(|c| c.1);
                    self.set_from(n, row.next().unwrap_or(Value::Null), from)?;
                }
            }
            Target::Record(r) => {
                let fields = columns.clone().unwrap_or_default();
                let vals = row.unwrap_or_else(|| vec![Value::Null; fields.len()]);
                self.set_record(r, &fields, vals);
            }
        }
        Ok(())
    }

    fn block(&mut self, b: &Block) -> Result<Flow> {
        let mut mine = vec![];
        let out = (|| {
            for d in &b.decls {
                match d.ty {
                    None => {
                        let hidden = self.declare_record(&d.name);
                        mine.push((d.name.clone(), hidden));
                    }
                    Some((t, typmod)) => {
                        self.at.set(Some((d.line, "during statement block local variable initialization")));
                        let (v, from) = match &d.default {
                            Some(e) => self.typed(e)?,
                            None => (Value::Null, None),
                        };
                        // (a domain's: its value, even the NULL of no default, is held to the domain's constraints)
                        let domain = match t {
                            Type::Enum(_) => super::domain::of_type(&self.x.view()?, t)?,
                            _ => None,
                        };
                        let t = self.x.resolved(t)?;
                        let hidden = self.declare(d.name.clone(), t, Value::Null, typmod, false);
                        mine.push((d.name.clone(), hidden));
                        if let Some(i) = self.find(&d.name) {
                            self.vars[i].domain = domain;
                        }
                        self.set_from(&d.name, v, from)?;
                        let i = self.find(&d.name).unwrap();
                        self.vars[i].not_null = d.not_null;
                    }
                }
            }
            if b.handlers.is_empty() { self.stmts(&b.body) } else { self.guarded(b) }
        })();
        self.leave(mine);
        out
    }

    /// A block's body under its EXCEPTION handlers: an error one of them catches takes back
    /// what the body wrote (Postgres's implicit savepoint: the rows it pushed into the calling
    /// statement's write), then that handler runs, and what it writes stays.
    fn guarded(&mut self, b: &Block) -> Result<Flow> {
        let sp = self.x.savepoint()?;
        let e = match self.stmts(&b.body) {
            Err(e) => e,
            ok => {
                self.x.release(sp);
                return ok;
            }
        };
        let Some(h) = b.handlers.iter().find(|h| h.catches(e.code)) else { return Err(e) };
        self.x.rollback_to(sp);
        let mut c = Caught::of(e);
        // (this function's line, where it failed: Postgres has it in the error's CONTEXT already)
        c.here = (!std::mem::take(&mut self.given)).then(|| self.context_line());
        let text = |s: &str| Value::Text(s.into());
        let hid = [
            self.declare("sqlstate".into(), Type::Text, text(c.e.code), None, false),
            self.declare("sqlerrm".into(), Type::Text, text(&c.e.message), None, false),
        ];
        self.caught.push(c);
        let out = self.stmts(&h.body);
        self.caught.pop();
        let [a, b] = hid;
        self.leave(vec![("sqlstate".into(), a), ("sqlerrm".into(), b)]);
        out
    }

    /// Runs dynamic SQL: the statement `q` gives, its `$1`, ... the values of `using`. Its
    /// output, how many rows it read or wrote, and whether it returns rows.
    fn dynamic(&mut self, q: &E, using: &[E]) -> Result<(Output, usize, bool)> {
        self.dynamic_as(q, using, false)
    }

    /// `dynamic`; with `rows`, a statement that returns none is refused before it runs (RETURN QUERY EXECUTE's),
    /// as Postgres's SPI refuses it: `INSERT query does not return tuples`.
    fn dynamic_as(&mut self, q: &E, using: &[E], rows: bool) -> Result<(Output, usize, bool)> {
        let sql = match self.eval(q)? {
            Value::Null => return err("22004", "query string argument of EXECUTE is null"),
            v => v.to_text(),
        };
        let vals = using.iter().map(|e| self.eval(e)).collect::<Result<Vec<_>>>()?;
        // (a variable's declared type, a NULL's too; else its value's)
        let declared = |e: &E| match &e.e {
            Expr::Col(None, n) => self.find(n).map(|i| self.cols[i].ty),
            Expr::Param(n) => self.cols.get(n.wrapping_sub(1)).map(|c| c.ty),
            _ => None,
        };
        let types: Vec<Option<Type>> = using.iter().zip(&vals).map(|(e, v)| declared(e).or_else(|| v.ty())).collect();
        let mut stmts = super::parse(&sql)?;
        if stmts.len() != 1 {
            return err(SYNTAX, "EXECUTE runs one statement");
        }
        let st = stmts.pop().unwrap().0;
        let returns = match &st {
            Stmt::Select(_) | Stmt::Locking(..) => true,
            Stmt::Insert { returning, .. } | Stmt::Update { returning, .. } | Stmt::Delete { returning, .. } => {
                returning.is_some()
            }
            Stmt::Explain { .. } => true,
            // (DDL and the rest: run as they are, giving no rows)
            _ => false,
        };
        if rows && !returns {
            let tag = match &st {
                Stmt::CreateAs { .. }
                    if sql.trim_start().get(..6).is_some_and(|w| w.eq_ignore_ascii_case("select")) =>
                {
                    "SELECT INTO"
                }
                s => functions::what(s),
            };
            let e = SqlError { code: SYNTAX, message: format!("{tag} query does not return tuples"), info: None };
            return Err(Frame::statement_context(e, &sql));
        }
        let select = matches!(st, Stmt::Select(_) | Stmt::Locking(..) | Stmt::Explain { .. });
        let out = self.x.with_params(&vals, &types, self.x.readonly).run_body(&st, false)?;
        let n =
            if select { out.rows.len() } else { out.tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0) };
        self.using = vals;
        Ok((out, n, returns))
    }

    /// A FOR loop over a query's rows (`label`: the loop's, see `body`).
    fn for_rows(&mut self, target: &Target, out: Output, body: &[S], label: &Option<String>) -> Result<Flow> {
        let any = !out.rows.is_empty();
        for row in out.rows {
            self.assign(target, &out.columns, Some(row))?;
            if let Some(f) = self.body(body, label)? {
                self.set("found", Value::Bool(any))?;
                return Ok(f);
            }
        }
        self.set("found", Value::Bool(any))?;
        Ok(Flow::Next)
    }

    /// print_strict_params's DETAIL for statement `s` that just ran: the variables it reads and their values, in the
    /// order they were declared (`parameters: p1 = '2', p3 = 'foo'`). None when it is off, or the statement reads none.
    fn strict_params(&self, s: &Stmt) -> Option<String> {
        if !self.opts().strict_params {
            return None;
        }
        let key = (s as *const Stmt as usize, self.shape);
        let made = self.x.nest.pl.borrow().get(&key).map(|x| x.1.clone())?;
        let mut st = (*made).clone();
        let mut used = std::collections::BTreeSet::new();
        functions::visit(
            &mut st,
            &mut |e| {
                if let Expr::Param(n) = e {
                    used.insert(*n);
                }
                Ok(())
            },
            &mut |_| {},
        )
        .ok()?;
        let listed: Vec<String> = used
            .into_iter()
            .filter_map(|n: usize| {
                let i = n.checked_sub(1)?;
                Some(format!("{} = {}", shown(&self.cols.get(i)?.name), quoted_param(self.vals.get(i)?)))
            })
            .collect();
        (!listed.is_empty()).then(|| format!("parameters: {}", listed.join(", ")))
    }

    /// print_strict_params's DETAIL for an EXECUTE's USING values: `parameters: $1 = '0', $2 = 'foo'`.
    fn strict_using(&self, using: &[E]) -> Option<String> {
        if !self.opts().strict_params || using.is_empty() {
            return None;
        }
        let shown: Vec<String> =
            self.using.iter().enumerate().map(|(i, v)| format!("${} = {}", i + 1, quoted_param(v))).collect();
        Some(format!("parameters: {}", shown.join(", ")))
    }

    fn stmts(&mut self, ss: &[S]) -> Result<Flow> {
        for s in ss {
            match self.stmt(s)? {
                Flow::Next => {}
                f => return Ok(f),
            }
        }
        Ok(Flow::Next)
    }

    /// A loop's body once: whether to go on, or the flow that leaves the loop. (`label`: the loop's, which a labeled
    /// EXIT or CONTINUE names; one naming a loop or block further out leaves this loop for it.)
    fn body(&mut self, body: &[S], label: &Option<String>) -> Result<Option<Flow>> {
        tick()?; // a statement timeout or cancel stops an endless loop
        Ok(match self.stmts(body)? {
            Flow::Next | Flow::Continue(None) => None,
            Flow::Continue(Some(l)) if label.as_ref() == Some(&l) => None,
            Flow::Exit(None) => Some(Flow::Next),
            Flow::Exit(Some(l)) if label.as_ref() == Some(&l) => Some(Flow::Next),
            f => Some(f),
        })
    }

    fn int(&mut self, e: &E, what: &str) -> Result<i64> {
        match coerce(self.eval(e)?, Type::Int)? {
            Value::Int(n) => Ok(n),
            _ => err("22004", format!("{what} of FOR loop cannot be null")),
        }
    }

    fn stmt(&mut self, s: &S) -> Result<Flow> {
        match s {
            // (the statement an error names: it, until it is done, as Postgres's err_stmt)
            S::At(line, what, s) => {
                let saved = self.at.replace(Some((*line, *what)));
                let out = self.stmt(s);
                if out.is_ok() {
                    self.at.set(saved);
                }
                return out;
            }
            // (the loop takes its label as it starts: see `label`)
            S::Labeled(l, s) => {
                self.label = Some(l.clone());
                let out = self.stmt(s);
                self.label = None;
                return out;
            }
            S::ForEach { var, slice, array, body } => {
                let label = self.label.take();
                return self.foreach(var.as_deref(), *slice, array, body, &label);
            }
            S::NoTuples(text) => {
                let e =
                    SqlError { code: SYNTAX, message: "SELECT INTO query does not return tuples".into(), info: None };
                return Err(Frame::statement_context(e, text));
            }
            S::Assign(n, e) => self.assign_to(n, e)?,
            S::If(arms, other) => {
                for (c, body) in arms {
                    if self.cond(c)? {
                        return self.stmts(body);
                    }
                }
                return self.stmts(other);
            }
            S::Case(operand, arms, other) => {
                let x = operand.as_ref().map(|e| self.eval(e)).transpose()?;
                for (vals, body) in arms {
                    for v in vals {
                        let hit = match &x {
                            Some(x) => compare_op(Op::Eq, x, &self.eval(v)?)? == Value::Bool(true),
                            None => self.cond(v)?,
                        };
                        if hit {
                            return self.stmts(body);
                        }
                    }
                }
                return match other {
                    Some(b) => self.stmts(b),
                    None => Err(SqlError {
                        code: "20000",
                        message: "case not found".into(),
                        info: Some(Box::new(Violation {
                            hint: Some("CASE statement is missing ELSE part.".into()),
                            ..Violation::default()
                        })),
                    }),
                };
            }
            S::Loop(body) => {
                let label = self.label.take();
                loop {
                    if let Some(f) = self.body(body, &label)? {
                        return Ok(f);
                    }
                }
            }
            S::While(c, body) => {
                let label = self.label.take();
                while self.cond(c)? {
                    if let Some(f) = self.body(body, &label)? {
                        return Ok(f);
                    }
                }
            }
            S::ForInt { var, reverse, lo, hi, by, body } => {
                let label = self.label.take();
                let (mut i, end) = (self.int(lo, "lower bound")?, self.int(hi, "upper bound")?);
                let step = match by {
                    Some(b) => self.int(b, "BY value")?,
                    None => 1,
                };
                if step <= 0 {
                    return err("22023", "BY value of FOR loop must be greater than zero");
                }
                let hidden = self.declare(var.clone(), Type::Int, Value::Null, None, false);
                let slot = self.find(var).unwrap();
                let mut any = false;
                let out: Result<Option<Flow>> = (|| {
                    while if *reverse { i >= end } else { i <= end } {
                        any = true;
                        self.vals[slot] = Value::Int(i);
                        if let Some(f) = self.body(body, &label)? {
                            return Ok(Some(f));
                        }
                        match if *reverse { i.checked_sub(step) } else { i.checked_add(step) } {
                            Some(n) => i = n,
                            None => break,
                        }
                    }
                    Ok(None)
                })();
                self.leave(vec![(var.clone(), hidden)]);
                self.set("found", Value::Bool(any))?;
                if let Some(f) = out? {
                    return Ok(f);
                }
            }
            S::ForQuery { target, query, body, text } => {
                let label = self.label.take();
                let out = self.query(query).map_err(|e| Frame::statement_context(e, text))?;
                return self.for_rows(target, out, body, &label);
            }
            S::ForExecute { target, query, using, body } => {
                let label = self.label.take();
                let (out, _, returns) = self.dynamic(query, using)?;
                if !returns {
                    return err(SYNTAX, "cannot open a query that returns no rows as a FOR loop's");
                }
                return self.for_rows(target, out, body, &label);
            }
            S::Exit(l, c) | S::Continue(l, c) => {
                let go = match c {
                    Some(c) => self.cond(c)?,
                    None => true,
                };
                if go {
                    return Ok(if matches!(s, S::Exit(..)) {
                        Flow::Exit(l.clone())
                    } else {
                        Flow::Continue(l.clone())
                    });
                }
            }
            S::Return(e) => {
                let r = match (e, &self.f.ret) {
                    (None, Ret::Table(_)) if self.f.outs => Returned::Rows(vec![self.out_row()]),
                    (None, Ret::SetOf(_) | Ret::Table(_) | Ret::RowsOf(_)) => {
                        Returned::Rows(std::mem::take(&mut self.rows))
                    }
                    (None, Ret::Void) => Returned::Value(Value::Text(String::new())),
                    (None, _) => Returned::Value(Value::Null),
                    (Some(e), Ret::Value(t)) => Returned::Value(self.returned(e, *t)?),
                    // (a trigger function's result is a row of the table's type, or NULL: `coalesce(new, old)`,
                    // a CASE of them. A record used whole is its row's text, read back as the table's columns)
                    (Some(e), Ret::Trigger) => match self.eval(e)? {
                        Value::Null => Returned::Record(None),
                        Value::Text(text) if row_valued(&e.e) => Returned::Record(Some(self.trigger_row(&text)?)),
                        _ => {
                            return err(
                                "42804",
                                "cannot return non-composite value from function returning composite type",
                            );
                        }
                    },
                    (Some(e), _) => Returned::Value(self.eval(e)?),
                };
                return Ok(Flow::Return(r));
            }
            S::ReturnRecord(r) => return Ok(Flow::Return(Returned::Record(self.record(r)))),
            S::ReturnNext(e) => {
                let row = match (e, &self.f.ret) {
                    // (a regproc returned as text is its function's name)
                    (Some(e), Ret::SetOf(t)) => vec![self.returned(e, *t)?],
                    (Some(e), _) => vec![self.eval(e)?],
                    (None, ret) => match ret {
                        Ret::Table(cols) => cols
                            .iter()
                            .map(|(n, _)| self.find(n).map_or(Value::Null, |i| self.vals[i].clone()))
                            .collect(),
                        _ => vec![],
                    },
                };
                self.rows.push(row);
            }
            S::ReturnNextRecord(r) => match self.record(r) {
                Some(row) => self.rows.push(row),
                None => return err("55000", format!("record \"{r}\" is not assigned yet")),
            },
            S::ReturnQuery(q, text) => {
                let out = self.query(q).map_err(|e| Frame::statement_context(e, text))?;
                self.query_returns(&out)?;
                self.row_count = out.rows.len();
                self.set("found", Value::Bool(!out.rows.is_empty()))?;
                self.rows.extend(out.rows);
            }
            S::ReturnQueryExecute(q, using) => {
                let (out, n, _) = self.dynamic_as(q, using, true)?;
                self.row_count = n;
                self.set("found", Value::Bool(n > 0))?;
                self.rows.extend(out.rows);
            }
            S::Execute { query, into, using } => {
                let (out, n, returns) = self.dynamic(query, using)?;
                self.row_count = n; // EXECUTE leaves FOUND as it was, as in Postgres
                if let Some((t, strict)) = into {
                    if !returns {
                        return err(SYNTAX, "INTO used with a command that cannot return data");
                    }
                    // (STRICT's errors; with print_strict_params, the USING values as their DETAIL)
                    let strict_error = |code: &'static str, message: &str, detail: Option<String>| SqlError {
                        code,
                        message: message.into(),
                        info: detail.map(|d| Box::new(Violation { detail: Some(d), ..Violation::default() })),
                    };
                    if *strict && out.rows.is_empty() {
                        return Err(strict_error("P0002", "query returned no rows", self.strict_using(using)));
                    }
                    if *strict && out.rows.len() > 1 {
                        return Err(strict_error(
                            "P0003",
                            "query returned more than one row",
                            self.strict_using(using),
                        ));
                    }
                    let row = out.rows.into_iter().next();
                    self.assign(t, &out.columns, row)?;
                }
            }
            S::Sql { stmt, into, perform, text } => {
                let select = matches!(**stmt, Stmt::Select(_) | Stmt::Locking(..));
                if select && into.is_none() && !perform {
                    return err(
                        SYNTAX,
                        "query has no destination for result data; use PERFORM to run a SELECT for its effects",
                    );
                }
                let out = self.query(stmt).map_err(|e| Frame::statement_context(e, text))?;
                let n = if select {
                    out.rows.len()
                } else {
                    out.tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
                };
                self.row_count = n;
                if let Some((t, strict)) = into {
                    if *strict && out.rows.is_empty() {
                        let detail = self.strict_params(stmt);
                        let info = detail.map(|d| Box::new(Violation { detail: Some(d), ..Violation::default() }));
                        return Err(SqlError { code: "P0002", message: "query returned no rows".into(), info });
                    }
                    // more than one row: an error for STRICT and for a write's RETURNING, and for a SELECT what
                    // plpgsql's too_many_rows check says (an error, a WARNING, or nothing)
                    let (error, warn) = (self.opts().error.too_many_rows, self.opts().warn.too_many_rows);
                    if out.rows.len() > 1 && (*strict || !select || error || warn) {
                        let message = "query returned more than one row".to_string();
                        let hint = "Make sure the query returns a single row, or use LIMIT 1.".to_string();
                        let detail = self.strict_params(stmt);
                        if *strict || !select || error {
                            let v = Violation { detail, hint: Some(hint), ..Violation::default() };
                            return Err(SqlError { code: "P0003", message, info: Some(Box::new(v)) });
                        }
                        super::notice_raised("WARNING", "P0003", message, detail, Some(hint));
                    }
                    let row = out.rows.into_iter().next();
                    self.assign(t, &out.columns, row)?;
                }
                self.set("found", Value::Bool(n > 0))?;
            }
            S::Raise { level, cond, fmt, args, opts } => self.raise(level, cond, fmt, args, opts)?,
            // (not checked at all with plpgsql.check_asserts off)
            S::Assert(c, m) => {
                if self.opts().asserts && !self.cond(c)? {
                    let message = match m {
                        Some(m) => self.text(m)?.to_text(),
                        None => "assertion failed".into(),
                    };
                    return err("P0004", message);
                }
            }
            S::Diag(stacked, items) => {
                let c = match (stacked, self.caught.last()) {
                    (false, _) => None,
                    (true, Some(c)) => Some(c.clone()),
                    (true, None) => {
                        return err("0Z002", "GET STACKED DIAGNOSTICS cannot be used outside an exception handler");
                    }
                };
                for (v, item) in items {
                    let val = match &c {
                        // the call stack, and the function's oid (0 for a DO block, which has none)
                        None if item == "pg_context" => Value::Text(pg_context()),
                        None if item == "pg_routine_oid" => Value::Int(self.f.oid.unwrap_or(0)),
                        None => Value::Int(self.row_count as i64),
                        Some(c) => Value::Text(match item.as_str() {
                            "returned_sqlstate" => c.e.code.into(),
                            "message_text" => c.e.message.clone(),
                            "pg_exception_detail" => c.detail.clone(),
                            "pg_exception_hint" => c.hint.clone(),
                            // the functions it came out of, then this one at the statement it failed in
                            "pg_exception_context" => {
                                let inner = c.e.info.as_ref().and_then(|i| i.context.clone()).unwrap_or_default();
                                let here = c.here.clone().unwrap_or_default();
                                [inner, here].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n")
                            }
                            "pg_datatype_name" => c.datatype.clone(),
                            // COLUMN_NAME, CONSTRAINT_NAME, TABLE_NAME, SCHEMA_NAME: what the error says (empty where it
                            // says none)
                            _ => c.field(item),
                        }),
                    };
                    self.set(v, val)?;
                }
            }
            // the error again as it was raised: its CONTEXT what Postgres collected then, this function's line too
            S::Reraise => {
                return match self.caught.last() {
                    Some(c) => {
                        let mut c = c.clone();
                        if let Some(here) = c.here.take() {
                            c.e = with_context(c.e, &here);
                        }
                        self.given = true;
                        c.raise()
                    }
                    None => err("0Z002", "RAISE without parameters cannot be used outside an exception handler"),
                };
            }
            // (EXIT with its label leaves it)
            S::Block(b) => {
                return match self.block(b)? {
                    Flow::Exit(Some(l)) if b.label.as_deref() == Some(l.as_str()) => Ok(Flow::Next),
                    f => Ok(f),
                };
            }
            S::Open { var, query, dynamic, args, scroll, constant, text } => {
                let name = match self.find(var).map(|i| self.vals[i].clone()) {
                    // a refcursor with no name yet is given a new portal's, unless it is CONSTANT
                    Some(Value::Null) | None => {
                        if *constant {
                            return err("22005", format!("variable \"{var}\" is declared CONSTANT"));
                        }
                        let n = self.unnamed();
                        self.set(var, Value::Text(n.clone()))?;
                        n
                    }
                    Some(v) => v.to_text(),
                };
                if self.in_use(&name) {
                    return err("42P03", format!("cursor \"{name}\" already in use"));
                }
                let out = match (query, dynamic) {
                    (Some(q), _) => self.with_args(args, |me| me.query(q))?,
                    (None, Some((q, using))) => self.dynamic(q, using)?.0,
                    (None, None) => return err(SYNTAX, "a cursor needs a query"),
                };
                self.open(name, out, *scroll, text);
            }
            S::Fetch { var, go, into } => {
                let name = match self.find(var).map(|i| self.vals[i].clone()) {
                    Some(Value::Null) | None => return err("22004", format!("cursor variable \"{var}\" is null")),
                    Some(v) => v.to_text(),
                };
                let count = |me: &mut Self, e: &E| -> Result<i64> {
                    match coerce(me.eval(e)?, Type::BigInt)? {
                        Value::Int(n) => Ok(n),
                        _ => Ok(0),
                    }
                };
                // where it goes, as SQL's FETCH and MOVE go (a negative count the other way)
                let dir = match go {
                    Go::Next => Move::Fwd(Some(1)),
                    Go::Prior => Move::Back(Some(1)),
                    Go::First => Move::First,
                    Go::Last => Move::Last,
                    Go::Abs(e) => Move::Abs(count(self, e)?),
                    Go::Rel(e) => Move::Rel(count(self, e)?),
                    Go::Many { back: true, all: true, .. } => Move::Back(None),
                    Go::Many { all: true, .. } => Move::Fwd(None),
                    Go::Many { back, n, .. } => {
                        let n = match n {
                            Some(e) => count(self, e)?,
                            None => 1,
                        };
                        if *back != (n < 0) { Move::Back(Some(n.abs())) } else { Move::Fwd(Some(n.abs())) }
                    }
                };
                let moved = self.portal(&name, |c| -> Result<(Vec<Vec<Value>>, Columns)> {
                    c.forward_only(dir)?;
                    let (rows, to) = walk_cursor(c.pos, c.rows.len(), dir);
                    c.pos = to;
                    Ok((rows.into_iter().map(|i| c.rows[i].clone()).collect(), c.columns.clone()))
                });
                let Some(moved) = moved else { return err("34000", format!("cursor \"{name}\" does not exist")) };
                let (rows, columns) = moved?;
                // FETCH gives the row it lands on; MOVE finds one where it went past any
                let found = !rows.is_empty();
                if let Some(t) = into {
                    self.assign(t, &Some(columns), rows.into_iter().last())?;
                }
                self.set("found", Value::Bool(found))?;
            }
            S::Close(var) => {
                let name = match self.find(var).map(|i| self.vals[i].clone()) {
                    Some(Value::Null) | None => return err("22004", format!("cursor variable \"{var}\" is null")),
                    Some(v) => v.to_text(),
                };
                if self.close(&name).is_none() {
                    return err("34000", format!("cursor \"{name}\" does not exist"));
                }
            }
            // FOR over a bound cursor: it is opened under its variable's portal name (a new one if it has none, the
            // variable NULL again after), read, and closed
            S::ForCursor { rec, var, cur, args, body } => {
                let label = self.label.take();
                let named = self.find(var).map(|i| self.vals[i].clone()).filter(|v| *v != Value::Null);
                let name = match &named {
                    Some(v) => v.to_text(),
                    None => {
                        let n = self.unnamed();
                        self.set(var, Value::Text(n.clone()))?;
                        n
                    }
                };
                if self.in_use(&name) {
                    return err("42P03", format!("cursor \"{name}\" already in use"));
                }
                let out = self.with_args(args, |me| me.query(&cur.query))?;
                let shown = Output { typmods: vec![], tag: String::new(), columns: out.columns.clone(), rows: vec![] };
                self.open(name.clone(), shown, cur.scroll, &cur.text);
                let hidden = self.declare_record(rec);
                let flow = self.for_rows(&Target::Record(rec.clone()), out, body, &label);
                self.leave(vec![(rec.clone(), hidden)]);
                self.close(&name);
                if named.is_none() {
                    self.set(var, Value::Null)?;
                }
                return flow;
            }
            S::Null => {}
            // What the procedure has done is kept. A CALL is one statement here, and its work is stored together when
            // it ends, so there is nothing to store at this point; inside a transaction block Postgres refuses it too.
            // (Where this differs: a procedure that fails after its COMMIT keeps nothing, where Postgres keeps what
            // was committed.)
            S::Commit => {
                let in_block = super::STOP
                    .with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone()))
                    .is_some_and(|ctx| ctx.tx.is_some());
                if in_block {
                    return err("2D000", "invalid transaction termination");
                }
            }
        }
        Ok(Flow::Next)
    }

    /// FOREACH var SLICE n IN ARRAY expr: `var` (an array) set to each n-dimensional slice of the array in turn, its
    /// last n dimensions, in the order the elements are stored (row-major), as Postgres's array_iterate gives them; with
    /// SLICE 0, each element.
    fn foreach(
        &mut self,
        var: Option<&str>,
        slice: usize,
        array: &E,
        body: &[S],
        label: &Option<String>,
    ) -> Result<Flow> {
        let v = self.eval(array)?;
        if v == Value::Null {
            return err("22004", "FOREACH expression must not be null");
        }
        let a = super::mdarray::read(&v)?.unwrap_or_else(super::mdarray::Arr::empty);
        let n = a.dims.len();
        if slice > n {
            return err("2202E", format!("slice dimension ({slice}) is out of the valid range 0..{n}"));
        }
        let ty = var.and_then(|x| self.find(x)).map(|i| self.cols[i].ty);
        let array_var = matches!(ty, Some(Type::Array(_)));
        let var = match var {
            Some(x) if slice > 0 && array_var => x,
            Some(x) if slice == 0 && !array_var => x,
            _ if slice > 0 => return err("42804", "FOREACH ... SLICE loop variable must be of an array type"),
            _ => return err("42804", "FOREACH loop variable must not be of an array type"),
        };
        let dims = a.dims[n - slice..].to_vec();
        let size: usize = dims.iter().map(|d| d.len).product::<usize>().max(1);
        let mut any = false;
        for part in a.elems.chunks(size) {
            any = true;
            let v = if slice == 0 {
                part[0].clone()
            } else {
                super::mdarray::Arr { dims: dims.clone(), elems: part.to_vec() }.value()
            };
            self.set(var, v)?;
            if let Some(f) = self.body(body, label)? {
                self.set("found", Value::Bool(any))?;
                return Ok(f);
            }
        }
        self.set("found", Value::Bool(any))?;
        Ok(Flow::Next)
    }

    fn raise(
        &mut self,
        level: &'static str,
        cond: &Option<(&'static str, String)>,
        fmt: &Option<String>,
        args: &[E],
        opts: &[(String, E)],
    ) -> Result<()> {
        let mut message = match fmt {
            Some(f) => {
                let (mut out, mut args, mut chars) = (String::new(), args.iter(), f.chars().peekable());
                while let Some(c) = chars.next() {
                    match c {
                        '%' if chars.peek() == Some(&'%') => {
                            chars.next();
                            out.push('%');
                        }
                        '%' => match args.next() {
                            Some(a) => match self.text(a)? {
                                Value::Null => out.push_str("<NULL>"),
                                v => out.push_str(&v.to_text()),
                            },
                            None => out.push('%'),
                        },
                        c => out.push(c),
                    }
                }
                Some(out)
            }
            None => None,
        };
        // as Postgres's exec_stmt_raise: a condition's name is the message where none is given (else the SQLSTATE),
        // and each option may be given once, ERRCODE not beside a condition
        let mut condname = cond.as_ref().map(|c| c.1.clone());
        let mut code = cond.as_ref().map(|c| c.0);
        // DETAIL, HINT, COLUMN, CONSTRAINT, DATATYPE, TABLE, SCHEMA
        let mut fields: [Option<String>; 7] = Default::default();
        const NAMES: [&str; 7] = ["detail", "hint", "column", "constraint", "datatype", "table", "schema"];
        let again = |o: &str| err(SYNTAX, format!("RAISE option already specified: {}", o.to_uppercase()));
        for (o, e) in opts {
            let v = self.text(e)?;
            if v == Value::Null {
                return err("22004", "RAISE statement option cannot be null");
            }
            let v = v.to_text();
            match o.as_str() {
                "errcode" => {
                    if code.is_some() {
                        return again(o);
                    }
                    code = Some(sqlstate(&v)?);
                    condname = Some(v);
                }
                "message" => {
                    if message.is_some() {
                        return again(o);
                    }
                    message = Some(v);
                }
                o => {
                    let Some(k) = NAMES.iter().position(|n| *n == o) else { continue };
                    if fields[k].is_some() {
                        return again(o);
                    }
                    fields[k] = Some(v);
                }
            }
        }
        let code = code.unwrap_or(if level == "EXCEPTION" { "P0001" } else { "00000" });
        let message = message.or(condname).unwrap_or_else(|| code.to_string());
        let [detail, hint, column, constraint, datatype, table, schema] = fields;
        match level {
            "EXCEPTION" => {
                // (its DETAIL and HINT sent with it, and the fields it names, as Postgres sends them)
                let any = [&detail, &hint, &column, &constraint, &table, &schema].iter().any(|f| f.is_some());
                let info = any.then(|| {
                    Box::new(Violation {
                        detail: detail.clone(),
                        hint: hint.clone(),
                        column,
                        constraint,
                        table,
                        schema,
                        ..Violation::default()
                    })
                });
                let e = SqlError { code, message, info };
                let (detail, hint, datatype) =
                    (detail.unwrap_or_default(), hint.unwrap_or_default(), datatype.unwrap_or_default());
                Caught { e, detail, hint, datatype, here: None }.raise()
            }
            "DEBUG" | "LOG" => Ok(()), // below what clients are sent, by default
            _ => {
                super::notice_raised(level, code, message, detail, hint);
                Ok(())
            }
        }
    }
}
