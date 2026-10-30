//! Functions and triggers made with CREATE FUNCTION and CREATE TRIGGER.
//!
//! A function is a row of `_sonos_functions`, keyed by its name and argument types
//! (`add1(integer)`) and holding its definition as JSON; a table's triggers are part of its
//! schema row. So both fork, merge, diff and travel in time with the data, and a restart replays
//! the rows they wrote rather than running them again.
//!
//! LANGUAGE sql bodies are SQL statements reading their arguments as `$1` or by name; LANGUAGE
//! plpgsql bodies run in `plpgsql`. Functions and triggers run inside the statement that calls
//! or fires them, and what they write commits with it (see `Exec::write`).

use super::*;
use std::rc::Rc;

/// Where functions live: one row per function, keyed `name(argument types)`.
pub(super) const FUNCTIONS: &str = "_sonos_functions";

/// How deep functions and triggers may call each other: deeper is an endless recursion, or
/// close to overflowing a server thread's 2 MB stack (a recursive SQL function does after about
/// 100 levels).
const MAX_DEPTH: usize = 40;

/// Whether this thread's stack is nearly spent: less left than a function call may take before
/// the next one checks (a statement nested as deep as the parser allows, and running it), or a
/// quarter of a small stack. Bounds unknown (other systems, or the query failed): measured from
/// where this thread first asked, against an assumed 2 MB stack (Rust's default for spawned threads).
fn stack_low() -> bool {
    thread_local! {
        // the stack's lowest address, and how much must be left above it
        static END: (usize, usize) = {
            let (end, size) = stack_bounds().unwrap_or_else(|| (stack_at().saturating_sub(2 << 20), 2 << 20));
            (end, (size / 4).min(4 << 20))
        };
    }
    END.with(|&(end, need)| stack_at().saturating_sub(end) < need)
}

/// This thread's stack: its lowest address and its size.
#[cfg(target_os = "macos")]
fn stack_bounds() -> Option<(usize, usize)> {
    // SAFETY: reads the calling thread's own attributes
    let (top, size) = unsafe {
        (
            libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize,
            libc::pthread_get_stacksize_np(libc::pthread_self()),
        )
    };
    Some((top.checked_sub(size)?, size))
}

#[cfg(target_os = "linux")]
fn stack_bounds() -> Option<(usize, usize)> {
    // SAFETY: attr is initialized by pthread_getattr_np before it's read, and destroyed after
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let (mut addr, mut size) = (std::ptr::null_mut(), 0);
        let ok = libc::pthread_attr_getstack(&attr, &mut addr, &mut size) == 0;
        libc::pthread_attr_destroy(&mut attr);
        ok.then_some((addr as usize, size))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn stack_bounds() -> Option<(usize, usize)> {
    None
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Lang {
    Sql,
    PlPgSql,
}

/// From the most volatile to the least.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub(super) enum Volatility {
    Volatile,
    Stable,
    Immutable,
}

impl Volatility {
    pub(super) fn name(self) -> &'static str {
        match self {
            Volatility::Volatile => "volatile",
            Volatility::Stable => "stable",
            Volatility::Immutable => "immutable",
        }
    }
}

/// What a function returns.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Ret {
    Value(Type),
    SetOf(Type),
    /// RETURNS TABLE (name type, ...)
    Table(Vec<(String, Type)>),
    /// RETURNS SETOF a table: rows with its columns
    RowsOf(String),
    Trigger,
    Void,
}

impl Ret {
    fn sql(&self) -> String {
        match self {
            Ret::Value(t) => t.name().into(),
            Ret::SetOf(t) => format!("setof {}", t.name()),
            Ret::Table(c) => {
                format!(
                    "table ({})",
                    c.iter().map(|(n, t)| format!("{} {}", ident(n), t.name())).collect::<Vec<_>>().join(", ")
                )
            }
            Ret::RowsOf(t) => format!("setof {}", ident_rel(t)),
            Ret::Trigger => "trigger".into(),
            Ret::Void => "void".into(),
        }
    }

    pub(super) fn sets(&self) -> bool {
        matches!(self, Ret::SetOf(_) | Ret::Table(_) | Ret::RowsOf(_))
    }
}

/// What ALTER FUNCTION, PROCEDURE or ROUTINE names: a routine of that kind (ROUTINE: either).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum RoutineKind {
    Function,
    Procedure,
    Routine,
}

impl RoutineKind {
    /// The word Postgres's messages name one by.
    pub(super) fn noun(self) -> &'static str {
        match self {
            RoutineKind::Function => "function",
            RoutineKind::Procedure => "procedure",
            RoutineKind::Routine => "routine",
        }
    }

    pub(super) fn tag(self) -> &'static str {
        match self {
            RoutineKind::Function => "ALTER FUNCTION",
            RoutineKind::Procedure => "ALTER PROCEDURE",
            RoutineKind::Routine => "ALTER ROUTINE",
        }
    }
}

/// What ALTER FUNCTION, PROCEDURE or ROUTINE does (besides OWNER TO).
#[derive(Clone, Debug)]
pub(super) enum RoutineAlter {
    /// its attributes, each with where in the text it is written (from 1)
    Attrs(Vec<(RoutineAttr, u32)>),
    Rename(String),
    Schema(String),
}

/// One attribute ALTER FUNCTION changes.
#[derive(Clone, Debug)]
pub(super) enum RoutineAttr {
    Volatility(Volatility),
    Strict(bool),
    Definer(bool),
    Leakproof(bool),
    Cost(f64),
    Rows(f64),
    Parallel(char),
    /// SUPPORT f: the planner's helper, not kept
    Support,
    /// SET name TO values (None: FROM CURRENT)
    Set(String, Option<Vec<String>>),
    /// RESET name (None: RESET ALL)
    Reset(Option<String>),
}

impl RoutineAttr {
    /// Whether a procedure has it too (SECURITY and the settings); the others only tune a function's calls.
    pub(super) fn procedures_have(&self) -> bool {
        matches!(self, RoutineAttr::Definer(_) | RoutineAttr::Set(..) | RoutineAttr::Reset(_))
    }
}

#[derive(PartialEq, Clone, Copy)]
enum ArgMode {
    In,
    Out,
    InOut,
    /// VARIADIC: the last parameter, an array of the arguments a call gives from there on
    Variadic,
}

/// A function made with CREATE FUNCTION, its body parsed.
pub(super) struct Function {
    pub(super) name: String,
    /// name (if it has one) and type, in order
    pub(super) args: Vec<(Option<String>, Type)>,
    /// a polymorphic function's instance (see `instance`): the types its parameters were declared with, which its
    /// CONTEXT lines name (`foreach_test(anyarray)`); empty for the function itself
    pub(super) declared: Vec<Type>,
    /// each parameter's DEFAULT, as SQL (only the last ones have any)
    pub(super) defaults: Vec<Option<String>>,
    pub(super) ret: Ret,
    pub(super) lang: Lang,
    pub(super) volatility: Volatility,
    pub(super) strict: bool,
    pub(super) body: String,
    /// made with CREATE PROCEDURE: called with CALL, not in a query
    pub(super) procedure: bool,
    /// the oid CREATE gave it (see `catalog_objects::func_oids`); none for one made before oids were kept
    pub(super) oid: Option<i64>,
    /// SECURITY DEFINER: it runs as its owner (see `acl::definer`)
    pub(super) definer: bool,
    /// made with OUT or INOUT parameters: `ret` is their columns, and a plpgsql body gives their variables as its row
    pub(super) outs: bool,
    /// RETURNS record without SETOF or OUT parameters: one row, whose columns the call lists; or RETURNS a table's row
    /// type without SETOF (`ret` then `Ret::RowsOf` the table): one row of its columns
    pub(super) single: bool,
    /// its last parameter is VARIADIC: an array of the arguments a call gives from there on
    pub(super) variadic: bool,
    /// COST n: what Postgres's planner reckons a call costs, which orders a scan's conditions (see `qual_cost`);
    /// None where none was given (100)
    pub(super) cost: Option<f64>,
    /// its body is the SQL standard's (RETURN expression, or BEGIN ATOMIC ... END), parsed when it was made: what it
    /// reads it depends on, as Postgres records it (a column of one can't be dropped, or its type changed, while it
    /// reads it); false for one made before this was kept
    pub(super) std_body: bool,
    /// its `SET name = value` clauses, in order: `search_path` holds for the length of each call (see
    /// `super::with_own_path`); the others are kept, and shown, but not applied
    pub(super) config: Vec<(String, Vec<String>)>,
    /// LEAKPROOF, ROWS and PARALLEL: what Postgres's planner reads, kept and shown as pg_proc keeps them
    pub(super) tuning: Tuning,
    /// a SQL-standard body's form (see `StdForm`)
    pub(super) std_form: StdForm,
    /// the NOTICEs Postgres gives of the `table.column%TYPE` references in its signature as CREATE FUNCTION runs
    /// (`type reference t.c%TYPE converted to integer`), in order; not kept
    pub(super) type_notes: Vec<String>,
    /// the composite types' shapes it was read with, by their names (its body's variables', its casts' too): read
    /// again once one is no longer its type's (see `from_raw`); not kept
    shapes: Vec<Type>,
    /// made with LANGUAGE C AS 'file', 'symbol' as one of the C trigger functions of the regression tests' libraries
    /// (see `regress_c`): the library file and the symbol, which pg_proc shows as probin and prosrc. It runs as that
    /// C does, in place of its body (see `Exec::run_code`)
    pub(super) c_trigger: Option<(String, String)>,
    code: Code,
}

/// A function's planner facts that change nothing here but are kept, and shown in pg_proc, as made or altered:
/// proleakproof, prorows (None: the default, 1000 for a set and 0 otherwise) and proparallel (None: `u`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Tuning {
    pub(super) leakproof: bool,
    pub(super) rows: Option<f64>,
    pub(super) parallel: Option<char>,
}

/// How a function's SQL-standard body was written, for pg_get_functiondef: `RETURN expr` (None), or BEGIN ATOMIC
/// with, for each statement, whether it was a RETURN (kept as the SELECT of its value).
pub(super) type StdForm = Option<Vec<bool>>;

enum Code {
    Sql(Vec<Stmt>),
    Pl(plpgsql::Block),
}

impl fmt::Debug for Function {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.sig())
    }
}

/// What running a function gave.
pub(super) enum Returned {
    Value(Value),
    Rows(Vec<Vec<Value>>),
    /// a trigger function's row (RETURN NEW), or None for RETURN NULL
    Record(Option<Vec<Value>>),
}

/// A type's name; an enum's without keeping a name the parser has not looked up (see `Type::name`).
fn type_named(t: &Type) -> String {
    if let Type::Enum(i) = t { enum_name(*i) } else { t.name().into() }
}

/// Parsed functions by their stored rows.
static PARSED_FUNCS: LazyLock<Mutex<HashMap<String, Arc<Function>>>> = LazyLock::new(Default::default);

/// Empties `PARSED_FUNCS`, whose functions hold enum types' places (see `Enums::sweep`).
pub(super) fn forget_parsed() {
    PARSED_FUNCS.lock().unwrap().clear();
}

impl Function {
    /// `name(type, ...)`, as Postgres names a function.
    pub(super) fn sig(&self) -> String {
        format!("{}({})", self.name, self.args.iter().map(|a| a.1.name()).collect::<Vec<_>>().join(", "))
    }

    pub(super) fn key(&self) -> String {
        format!("{FUNCTIONS}/{}", self.sig())
    }

    /// How many arguments a call must give: the parameters before the first default.
    pub(super) fn required(&self) -> usize {
        self.defaults.iter().position(Option::is_some).unwrap_or(self.args.len())
    }

    /// Whether a call with `n` arguments can be for it (the rest take their defaults).
    pub(super) fn takes(&self, n: usize) -> bool {
        (self.required()..=self.args.len()).contains(&n)
    }

    /// Parameter `i`'s default, as SQL.
    pub(super) fn default_sql(&self, i: usize) -> Option<&str> {
        self.defaults.get(i).and_then(|d| d.as_deref())
    }

    /// The type of the value a call gives (for a set, of each row's one column).
    pub(super) fn value_type(&self) -> Type {
        match self.ret {
            Ret::Value(t) | Ret::SetOf(t) => t,
            Ret::Void => Type::Void,
            // OUT parameters: the one's type, or a record of them, whose fields `(f()).name` reads
            Ret::Table(ref c) if self.outs => match &c[..] {
                [(_, t)] => *t,
                _ => Type::record(c.clone()),
            },
            _ => Type::Text,
        }
    }

    fn arg_named(&self, n: &str) -> Option<usize> {
        self.args.iter().position(|a| a.0.as_deref() == Some(n))
    }

    fn to_json(&self) -> Json {
        let pair = |n: &Option<String>, t: &Type| serde_json::json!([n, t.name()]);
        let returns = match &self.ret {
            Ret::Value(t) => serde_json::json!({ "value": t.name() }),
            Ret::SetOf(t) => serde_json::json!({ "setof": t.name() }),
            Ret::Table(c) => {
                serde_json::json!({ "table": c.iter().map(|(n, t)| pair(&Some(n.clone()), t)).collect::<Vec<_>>() })
            }
            Ret::RowsOf(t) => serde_json::json!({ "rows_of": t }),
            Ret::Trigger => Json::from("trigger"),
            Ret::Void => Json::from("void"),
        };
        let mut j = serde_json::json!({
            "name": self.name,
            "args": self.args.iter().map(|(n, t)| pair(n, t)).collect::<Vec<_>>(),
            "defaults": self.defaults.iter().any(Option::is_some).then_some(&self.defaults),
            "returns": returns,
            "language": if self.lang == Lang::Sql { "sql" } else { "plpgsql" },
            "volatility": self.volatility.name(),
            "strict": self.strict,
            "body": self.body,
            "procedure": self.procedure,
            "oid": self.oid,
        });
        if self.definer {
            j["definer"] = Json::Bool(true);
        }
        if self.outs {
            j["outs"] = Json::Bool(true);
        }
        if self.single {
            j["single"] = Json::Bool(true);
        }
        if self.variadic {
            j["variadic"] = Json::Bool(true);
        }
        if let Some(c) = self.cost {
            j["cost"] = Json::from(c);
        }
        if self.std_body {
            j["std_body"] = Json::Bool(true);
        }
        if !self.config.is_empty() {
            j["config"] = serde_json::json!(self.config);
        }
        if self.tuning.leakproof {
            j["leakproof"] = Json::Bool(true);
        }
        if let Some(r) = self.tuning.rows {
            j["rows"] = Json::from(r);
        }
        if let Some(p) = self.tuning.parallel {
            j["parallel"] = Json::String(p.to_string());
        }
        if let Some(a) = &self.std_form {
            j["atomic"] = serde_json::json!(a);
        }
        if let Some(c) = &self.c_trigger {
            j["c_trigger"] = serde_json::json!(c);
        }
        j
    }

    pub(super) fn from_json(j: &Json) -> Result<Function> {
        let bad =
            || SqlError { code: "XX001", message: format!("a function's definition is damaged: {j}"), info: None };
        // a name that isn't a built-in type is an enum's
        let ty =
            |x: &Json| x.as_str().map(|n| Type::from_name(n).unwrap_or_else(|| enum_named(n, None))).ok_or_else(bad);
        let pair = |x: &Json| Ok::<_, SqlError>((x[0].as_str().map(String::from), ty(&x[1])?));
        let list = |x: &Json| x.as_array().ok_or_else(bad)?.iter().map(pair).collect::<Result<Vec<_>>>();
        let r = &j["returns"];
        let ret = match r.as_str() {
            Some("trigger") => Ret::Trigger,
            Some("void") => Ret::Void,
            _ if r.get("value").is_some() => Ret::Value(ty(&r["value"])?),
            _ if r.get("setof").is_some() => Ret::SetOf(ty(&r["setof"])?),
            _ if r.get("rows_of").is_some() => Ret::RowsOf(r["rows_of"].as_str().ok_or_else(bad)?.into()),
            _ => Ret::Table(list(&r["table"])?.into_iter().map(|(n, t)| (n.unwrap_or_default(), t)).collect()),
        };
        let mut f = Function {
            name: j["name"].as_str().ok_or_else(bad)?.into(),
            args: list(&j["args"])?,
            declared: vec![],
            defaults: j["defaults"]
                .as_array()
                .map(|d| d.iter().map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            ret,
            lang: if j["language"] == "sql" { Lang::Sql } else { Lang::PlPgSql },
            volatility: match j["volatility"].as_str() {
                Some("stable") => Volatility::Stable,
                Some("immutable") => Volatility::Immutable,
                _ => Volatility::Volatile,
            },
            strict: j["strict"] == true,
            body: j["body"].as_str().ok_or_else(bad)?.into(),
            procedure: j["procedure"] == true,
            oid: j["oid"].as_i64(),
            definer: j["definer"] == true,
            outs: j["outs"] == true,
            single: j["single"] == true,
            variadic: j["variadic"] == true,
            cost: j["cost"].as_f64(),
            std_body: j["std_body"] == true,
            config: serde_json::from_value(j["config"].clone()).unwrap_or_default(),
            tuning: Tuning {
                leakproof: j["leakproof"] == true,
                rows: j["rows"].as_f64(),
                parallel: j["parallel"].as_str().and_then(|p| p.chars().next()),
            },
            std_form: serde_json::from_value(j["atomic"].clone()).ok().flatten(),
            type_notes: vec![],
            shapes: vec![],
            c_trigger: serde_json::from_value(j["c_trigger"].clone()).ok().flatten(),
            code: Code::Sql(vec![]),
        };
        f.compile()?;
        Ok(f)
    }

    /// A function from its stored row, parsed once.
    pub(super) fn from_raw(raw: &str) -> Result<Arc<Function>> {
        // its arguments' enum places held, unless one was forgotten (a sweep): then it's parsed
        // again, as `schema_of` does (checked once the cache is unlocked: a sweep locks ENUMS
        // then PARSED_FUNCS)
        // (one with SET search_path is bound along that path, in a world whose relations its names find, as the
        // running role sees it for a SECURITY INVOKER one: one for each, see `compile` and `own_path_key`)
        // (and its composite types as they are now, not as they were before an ALTER TYPE, nor as another world has
        // them: its signature's and those its body was read with)
        let held = |f: &Arc<Function>| {
            let stale_ret = match &f.ret {
                Ret::Value(t) | Ret::SetOf(t) => super::stale_record(*t),
                Ret::Table(c) => c.iter().any(|(_, t)| super::stale_record(*t)),
                _ => false,
            };
            f.args.iter().all(|(_, t)| !matches!(t, Type::Enum(i) if enum_of(*i).map(hold_enum).is_err()))
                && !stale_ret
                && !f.args.iter().any(|(_, t)| super::stale_record(*t))
                && !f.shapes.iter().any(|t| super::stale_record(*t))
        };
        let keyed = |f: &Function| match f.own_path() {
            Some(p) => format!("{raw}\0{}", super::own_path_key(p, !f.definer)),
            None => raw.to_string(),
        };
        let hit = PARSED_FUNCS.lock().unwrap().get(raw).cloned();
        if let Some(f) = hit
            && held(&f)
        {
            let key = keyed(&f);
            if key == raw {
                return Ok(f);
            }
            // (the cache unlocked before `held` locks ENUMS, the order a sweep takes them in)
            let kept = PARSED_FUNCS.lock().unwrap().get(&key).cloned();
            if let Some(g) = kept
                && held(&g)
            {
                return Ok(g);
            }
        }
        let j: Json = serde_json::from_str(raw).unwrap_or_default();
        let (f, shapes) = userdefs::noting(|| Function::from_json(&j));
        let f = Arc::new(Function { shapes, ..f? });
        let key = keyed(&f);
        let mut c = PARSED_FUNCS.lock().unwrap();
        if c.len() >= 1024 {
            c.clear(); // ponytail: forget them all rather than track recency
        }
        c.insert(raw.into(), f.clone());
        if key != raw {
            c.insert(key, f.clone());
        }
        Ok(f)
    }

    /// The function as a CREATE OR REPLACE FUNCTION statement (for DIFF ... AS SQL).
    pub(super) fn to_sql(&self) -> String {
        let args: Vec<String> = self
            .args
            .iter()
            .enumerate()
            .map(|(i, (n, t))| {
                let base = match n {
                    Some(n) => format!("{} {}", ident(n), t.name()),
                    None => t.name().into(),
                };
                match self.default_sql(i) {
                    Some(d) => format!("{base} default {d}"),
                    None => base,
                }
            })
            .collect();
        let mut tag = "$$".to_string();
        for i in 0.. {
            if !self.body.contains(&tag) {
                break;
            }
            tag = format!("$f{i}$");
        }
        // (a C trigger function as it was made: its library and symbol)
        let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let body = match &self.c_trigger {
            Some((file, symbol)) => format!("{}, {}", lit(file), lit(symbol)),
            None => format!("{tag}{}{tag}", self.body),
        };
        format!(
            "create or replace function {}({}) returns {} language {} {}{}{}{} as {body};",
            ident_rel(&self.name),
            args.join(", "),
            match &self.ret {
                Ret::RowsOf(t) if self.single => ident_rel(t),
                r => r.sql(),
            },
            self.lang_name(),
            self.volatility.name(),
            if self.strict { " strict" } else { "" },
            if self.definer { " security definer" } else { "" },
            self.config_sql(),
        )
    }

    /// Its language as pg_language names it: `c` for a C trigger function (see `c_trigger`), else its body's.
    pub(super) fn lang_name(&self) -> &'static str {
        match (&self.c_trigger, self.lang) {
            (Some(_), _) => "c",
            (None, Lang::Sql) => "sql",
            (None, Lang::PlPgSql) => "plpgsql",
        }
    }

    /// What pg_proc's prosrc shows: a C trigger function's symbol (see `c_trigger`), else its body.
    pub(super) fn src(&self) -> &str {
        self.c_trigger.as_ref().map_or(&self.body, |(_, symbol)| symbol)
    }

    /// The procost it has made without COST, as Postgres's CreateFunction gives it: 1 for a C function (see
    /// `c_trigger`), 100 for a SQL or PL/pgSQL one.
    pub(super) fn default_cost(&self) -> f64 {
        if self.c_trigger.is_some() { 1.0 } else { 100.0 }
    }

    /// Its SET clauses as CREATE FUNCTION writes them: ` set search_path to 'app', 'pg_temp'`.
    fn config_sql(&self) -> String {
        self.config
            .iter()
            .map(|(name, values)| format!(" set {name} to {}", super::setting_sql(name, values)))
            .collect()
    }

    /// The search_path its `SET search_path` gives each call, if it has one.
    pub(super) fn own_path(&self) -> Option<&[String]> {
        self.config.iter().find(|(n, _)| n == "search_path").map(|(_, v)| v.as_slice())
    }

    /// Parses the body: refuses what it can't run now rather than when it's called. With `SET search_path`, along
    /// that path, whoever's statement loads it: the names it binds are the function's, not its caller's.
    /// A SECURITY DEFINER function's along the whole path, never as the role whose statement loads it (not yet its
    /// owner): neither `$user` nor a schema that role may not use decides what its names are (ponytail: `$user` isn't
    /// searched for them); its owner's privileges are checked as it runs. A SECURITY INVOKER function's as the running
    /// role, whose own the call is: its body is kept apart for each path roles see (see `from_raw`).
    fn compile(&mut self) -> Result<()> {
        match self.own_path().map(<[String]>::to_vec) {
            Some(path) => super::with_own_path(&path, !self.definer, || self.compile_here()),
            None => self.compile_here(),
        }
    }

    fn compile_here(&mut self) -> Result<()> {
        self.code = match self.lang {
            Lang::Sql => Code::Sql(sql_body(self)?),
            Lang::PlPgSql => Code::Pl(plpgsql::parse(self)?),
        };
        Ok(())
    }

    /// The call's expression, when the function is a SQL `SELECT expression` with nothing else
    /// that it can stand for (as Postgres inlines such functions): arguments cast to their types
    /// in place of the parameters, the result cast to the function's. Not one deeper than half
    /// MAX_DEPTH: calls in calls' arguments would nest bodies in bodies past what the parser
    /// allows (and evaluating or dropping it could overflow the stack); those stay calls.
    fn inline(&self, args: &[Expr]) -> Option<Expr> {
        self.inline_with(args, false)
    }

    /// `inline`; with `strict_too`, a STRICT function's as well, null when an argument is (what a row's expression
    /// takes: see `inline_row_calls`).
    fn inline_with(&self, args: &[Expr], strict_too: bool) -> Option<Expr> {
        // (a SECURITY DEFINER function runs as its owner, and one with SET clauses with its own settings: never in its
        // caller's place, as in Postgres)
        if self.definer || !self.config.is_empty() {
            return None;
        }
        let (Code::Sql(stmts), Ret::Value(rt)) = (&self.code, &self.ret) else { return None };
        let ([Stmt::Select(q)], false) = (&stmts[..], self.strict && !strict_too) else { return None };
        let plain = q.set.is_empty()
            && !q.distinct
            && q.from.is_none()
            && q.joins.is_empty()
            && q.filter.is_none()
            && q.group.is_empty()
            && q.having.is_none()
            && q.order.is_empty()
            && q.limit.is_none()
            && q.offset.is_none();
        let ([Item::Expr(body, _)], true) = (&q.items[..], plain) else { return None };
        let (mut simple, mut uses) = (true, vec![0; args.len()]);
        walk(body, &mut |x| match x {
            Expr::Param(n) => match uses.get_mut(n - 1) {
                Some(u) => *u += 1,
                None => simple = false,
            },
            Expr::Col(None, n) => match self.arg_named(n) {
                Some(i) => uses[i] += 1,
                None => simple = false,
            },
            Expr::Col(..)
            | Expr::Agg(..)
            | Expr::Window(_)
            | Expr::Sub(_)
            | Expr::Exists(..)
            | Expr::InSub(..)
            | Expr::Call(_)
            | Expr::Default => simple = false,
            _ => {}
        });
        // an argument read twice is evaluated twice: only for ones that can't differ (a STRICT one's is read once more,
        // by the test for null)
        let cheap =
            |a: &Expr| matches!(a, Expr::Lit(_) | Expr::Idx(_) | Expr::Param(_) | Expr::Col(..) | Expr::AggRef(_));
        let reads = |u: usize| u + usize::from(self.strict);
        // and every argument is converted to its type, even one the body doesn't read
        if !simple || uses.iter().zip(args).any(|(u, a)| *u == 0 || (reads(*u) > 1 && !cheap(a))) {
            return None;
        }
        let arg = |i: usize| Expr::Cast(Box::new(args[i].clone()), self.args[i].1);
        let e = rewrite(body, &|x| match x {
            Expr::Param(n) => Some(arg(n - 1)),
            Expr::Col(None, n) => self.arg_named(n).map(arg),
            _ => None,
        });
        let mut e = Expr::Cast(Box::new(e), *rt);
        // STRICT: null when an argument is, as the function isn't called then
        if self.strict && !args.is_empty() {
            let null = args
                .iter()
                .map(|a| Expr::IsNull(Box::new(a.clone()), false))
                .reduce(|a, b| Expr::Bin(Box::new(a), Op::Or, Box::new(b)));
            let none = Expr::Cast(Box::new(Expr::Lit(Value::Null)), *rt);
            e = Expr::Case(None, vec![(null.expect("an argument"), none)], Some(Box::new(e)));
        }
        Some(e).filter(|e| depth(e) <= MAX_DEPTH / 2)
    }
}

/// `e` with each call of a LANGUAGE sql function made here whose body is one `SELECT expression` (and that only one
/// function of its name and number of arguments can be) put in its place, as Postgres inlines such functions: what a
/// CHECK or a generated column, which run where no query does, can run. A STRICT one's is null when an argument is. A
/// call this can't put in place is left, and fails where it runs. A name without a schema is looked for in the table's
/// `schema`, then in public.
pub(super) fn inline_row_calls(v: &View, e: &Expr, schema: &str) -> Result<Expr> {
    let mut e = e.clone();
    // (a body's own calls too, a few levels down)
    for _ in 0..8 {
        let mut names = vec![];
        walk(&e, &mut |x| {
            if let Expr::Call(c) = x
                && !c.builtin
            {
                names.push(c.name.clone());
            }
        });
        let mut fs: HashMap<String, Vec<Arc<Function>>> = HashMap::new();
        for n in names {
            if let std::collections::hash_map::Entry::Vacant(e) = fs.entry(n) {
                // (a name without a schema is the table's schema's, or public's: what it meant where the table is,
                // whatever search_path the row's writer has)
                let found = match e.key().strip_prefix(PLAIN) {
                    Some(plain) => {
                        let mut found = named(v, &qualify(schema, plain))?;
                        if found.is_empty() {
                            found = named(v, &qualify("public", plain))?;
                        }
                        found
                    }
                    None => named(v, e.key())?,
                };
                e.insert(found);
            }
        }
        let changed = std::cell::Cell::new(false);
        let denied: std::cell::RefCell<Option<SqlError>> = std::cell::RefCell::new(None);
        let who = roles::checked();
        let next = rewrite(&e, &|x| {
            let Expr::Call(c) = x else { return None };
            let mut takes = fs.get(&c.name)?.iter().filter(|f| f.args.len() == c.args.len());
            let (Some(f), None) = (takes.next(), takes.next()) else { return None };
            // (run as a query would run it: by a role that may, its arguments in the function's order)
            if let Some(w) = &who
                && let Err(e) = acl::may_execute(v, w, f)
            {
                denied.borrow_mut().get_or_insert(e);
                return None;
            }
            let body = f.inline_with(&in_order(f, c)?, true)?;
            changed.set(true);
            Some(body)
        });
        if let Some(e) = denied.into_inner() {
            return Err(e);
        }
        e = next;
        if !changed.get() {
            break;
        }
    }
    Ok(e)
}

/// Call `c`'s arguments in function `f`'s order, those it names (`b => 1`) put where `f` has them; None where one isn't
/// given (a default isn't put in here) or is given twice.
fn in_order(f: &Function, c: &super::Call) -> Option<Vec<Expr>> {
    if c.names.is_empty() {
        return Some(c.args.clone());
    }
    let mut out: Vec<Option<Expr>> = vec![None; f.args.len()];
    for (i, (n, a)) in c.names.iter().zip(&c.args).enumerate() {
        let at = if n.is_empty() { i } else { f.arg_named(n)? };
        if out.get(at)?.is_some() {
            return None;
        }
        out[at] = Some(a.clone());
    }
    out.into_iter().collect()
}

/// A LANGUAGE sql body: SELECT, INSERT, UPDATE and DELETE statements, the last one giving the
/// result unless the function returns void.
fn sql_body(f: &Function) -> Result<Vec<Stmt>> {
    if f.ret == Ret::Trigger {
        return err("42P13", "SQL functions cannot return type trigger");
    }
    let mut out = vec![];
    for s in parse(&f.body)? {
        match s.0 {
            s @ (Stmt::Select(_)
            | Stmt::Locking(..)
            | Stmt::Insert { .. }
            | Stmt::Update { .. }
            | Stmt::Delete { .. }) => out.push(s),
            // (the standard's body is parsed as it is made, as Postgres's, which takes no utility statement there)
            s if f.std_body && !matches!(s, Stmt::Call(_)) => {
                return err(UNSUPPORTED, format!("{} is not yet supported in unquoted SQL function body", what(&s)));
            }
            s => {
                return err(
                    UNSUPPORTED,
                    format!(
                        "{} in a function isn't supported: its body runs SELECT, INSERT, UPDATE and DELETE",
                        what(&s)
                    ),
                );
            }
        }
    }
    let rows = |s: &Stmt| match s {
        Stmt::Select(_) | Stmt::Locking(..) => true,
        Stmt::Insert { returning, .. } | Stmt::Update { returning, .. } | Stmt::Delete { returning, .. } => {
            returning.is_some()
        }
        _ => false,
    };
    if f.ret != Ret::Void && !out.last().is_some_and(rows) {
        return err(
            "42P13",
            format!(
                "return type mismatch in function declared to return {}: its final statement must be SELECT or INSERT/UPDATE/DELETE RETURNING",
                f.ret.sql()
            ),
        );
    }
    Ok(out)
}

/// A statement's kind, for messages refusing it.
pub(super) fn what(s: &Stmt) -> &'static str {
    match s {
        Stmt::Create { .. } => "CREATE TABLE",
        Stmt::Drop { .. } => "DROP TABLE",
        Stmt::Alter { .. } => "ALTER TABLE",
        Stmt::CreateIndex { .. } | Stmt::VectorIndex { .. } => "CREATE INDEX",
        Stmt::DropIndex { .. } => "DROP INDEX",
        Stmt::CreateFunction(..) => "CREATE FUNCTION",
        Stmt::DropFunction { .. } => "DROP FUNCTION",
        Stmt::Do(_) => "DO",
        Stmt::Call(_) => "CALL",
        Stmt::CreateTrigger(..) => "CREATE TRIGGER",
        Stmt::DropTrigger { .. } => "DROP TRIGGER",
        Stmt::RenameTrigger { .. } => "ALTER TRIGGER",
        Stmt::RenameRule { .. } => "ALTER RULE",
        Stmt::Begin
        | Stmt::BeginRead(_)
        | Stmt::SetReadOnly { .. }
        | Stmt::Commit
        | Stmt::Rollback
        | Stmt::Chain(_)
        | Stmt::Savepoint(_)
        | Stmt::Release(_)
        | Stmt::RollbackTo(_)
        | Stmt::PrepareTransaction => "transaction control",
        Stmt::Explain { .. } => "EXPLAIN",
        Stmt::Set | Stmt::SetEf(_) | Stmt::SetTimeout(_) | Stmt::SetZone(..) => "SET",
        _ => "this statement",
    }
}

// ---------------------------------------------------------------- triggers

/// Which statement a trigger fires for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Event {
    Insert,
    Update,
    Delete,
}

impl Event {
    pub(super) fn name(self) -> &'static str {
        match self {
            Event::Insert => "INSERT",
            Event::Update => "UPDATE",
            Event::Delete => "DELETE",
        }
    }
}

/// A trigger, as its table's schema keeps it.
#[derive(Clone, Debug)]
pub(super) struct Trigger {
    pub(super) name: String,
    pub(super) before: bool,
    pub(super) events: Vec<Event>,
    /// UPDATE OF these columns (by id); empty: any
    pub(super) cols: Vec<u32>,
    /// UPDATE OF as written, until CREATE TRIGGER finds the columns
    of: Vec<String>,
    /// FOR EACH ROW (else FOR EACH STATEMENT)
    pub(super) row: bool,
    /// WHEN (condition), as SQL over NEW and OLD
    when: Option<String>,
    pub(super) func: String,
    pub(super) args: Vec<String>,
    /// whether it fires (in a session whose session_replication_role is origin, the only one here)
    pub(super) enabled: bool,
    /// pg_trigger.tgenabled: `O` (ENABLE), `D` (DISABLE), `A` (ENABLE ALWAYS) or `R` (ENABLE REPLICA); it
    /// fires for `O` and `A`
    pub(super) mode: char,
    /// INSTEAD OF, on a view: it runs for each row in place of the statement (kept as a rule: see `rules`)
    pub(super) instead: bool,
    /// REFERENCING OLD TABLE AS / NEW TABLE AS: the names its function reads the statement's rows by
    pub(super) old_table: Option<String>,
    pub(super) new_table: Option<String>,
    /// ON TRUNCATE was among its events, as written (CREATE TRIGGER refuses it: see `Exec::create_trigger`)
    pub(super) truncate: bool,
    /// CREATE CONSTRAINT TRIGGER's: an AFTER ROW trigger that is a constraint too (pg_constraint's contype `t`),
    /// which a transaction may put off to its COMMIT; None for a plain trigger
    pub(super) constraint: Option<ConstraintTrig>,
}

/// What a constraint trigger has besides a trigger's: DEFERRABLE, INITIALLY DEFERRED, and the table its FROM names
/// (pg_trigger.tgconstrrelid).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ConstraintTrig {
    pub(super) deferrable: bool,
    pub(super) deferred: bool,
    pub(super) from: Option<String>,
}

/// pg_trigger_depth(): how many triggers are running now, one inside another.
pub(super) fn trigger_depth() -> usize {
    TRIGGER_DEPTH.get()
}

thread_local! {
    static TRIGGER_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// An UPDATE or DELETE of a parent whose triggers have transition tables, running on the tables below it (see
    /// `hier::update_tree`): the parent and its columns' names, whose transition tables get the rows they change
    static TRANSITION_ROOT: std::cell::RefCell<Option<Root>> = const { std::cell::RefCell::new(None) };
}

/// See `TRANSITION_ROOT`: the parent's name, its columns' names, and the tables below it the statement runs on.
pub(super) type Root = (String, Vec<String>, Vec<String>);

/// Sets the parent `t` whose transition tables the rows a statement changes in `leaves` go to (see
/// `TRANSITION_ROOT`), when its triggers have transition tables: what was set before, to put back.
pub(super) fn transition_root(t: &Table, leaves: &[Table]) -> Option<Root> {
    let root = t.triggers.iter().any(|x| x.old_table.is_some() || x.new_table.is_some()).then(|| {
        let cols = t.cols.iter().map(|c| c.name.clone()).collect();
        (t.name.clone(), cols, leaves.iter().map(|l| l.name.clone()).collect())
    });
    TRANSITION_ROOT.replace(root)
}

/// Puts back what `transition_root` replaced.
pub(super) fn transition_root_back(was: Option<Root>) {
    TRANSITION_ROOT.set(was);
}

impl Trigger {
    /// WHEN's condition, as SQL over NEW and OLD.
    pub(super) fn when_sql(&self) -> Option<&str> {
        self.when.as_deref()
    }

    pub(super) fn to_json(&self) -> Json {
        let mut j = self.plain_json();
        if let Some(c) = &self.constraint {
            j["constraint"] = serde_json::json!({ "deferrable": c.deferrable, "deferred": c.deferred, "from": c.from });
        }
        j
    }

    fn plain_json(&self) -> Json {
        serde_json::json!({
            "name": self.name,
            "timing": if self.instead { "instead" } else if self.before { "before" } else { "after" },
            "events": self.events.iter().map(|e| e.name().to_lowercase()).collect::<Vec<_>>(),
            "update_of": self.cols,
            "level": if self.row { "row" } else { "statement" },
            "when": self.when,
            "function": self.func,
            "args": self.args,
            "enabled": self.enabled,
            "mode": self.mode.to_string(),
            "old_table": self.old_table,
            "new_table": self.new_table,
        })
    }

    /// pg_trigger.tgtype's bits: ROW 1, BEFORE 2, INSERT 4, DELETE 8, UPDATE 16, INSTEAD 64.
    pub(super) fn tgtype(&self) -> i64 {
        let events: i64 = self
            .events
            .iter()
            .map(|e| match e {
                Event::Insert => 4,
                Event::Delete => 8,
                Event::Update => 16,
            })
            .sum();
        i64::from(self.row) | i64::from(self.before) << 1 | i64::from(self.instead) << 6 | events
    }

    /// The trigger as CREATE TRIGGER (as Postgres's pg_get_triggerdef writes it), its WHEN as the condition parsed
    /// is written back (see `viewdef::trigger_when`); `pretty`, with only the parentheses precedence needs.
    pub(super) fn shown(&self, table: &str, t: &Table, pretty: bool) -> String {
        let def = self.sql(table, t);
        let Some(w) = &self.when else { return def };
        let cols: Vec<_> = t.cols.iter().filter(|c| !c.hidden).map(super::viewdef::col_ty).collect();
        match super::viewdef::trigger_when(w, &cols, pretty) {
            Some(shown) => def.replacen(&format!(" WHEN ({w}) "), &format!(" WHEN ({shown}) "), 1),
            None => def,
        }
    }

    /// The trigger as CREATE TRIGGER, its WHEN as stored (what a dump runs again).
    pub(super) fn sql(&self, table: &str, t: &Table) -> String {
        // (in pg_get_triggerdef's order: INSERT, DELETE, UPDATE)
        let events: Vec<String> = [Event::Insert, Event::Delete, Event::Update]
            .iter()
            .filter(|e| self.events.contains(e))
            .map(|e| match e {
                Event::Update if !self.cols.is_empty() => {
                    let names: Vec<String> = self
                        .cols
                        .iter()
                        .filter_map(|id| t.cols.iter().find(|c| c.id == *id))
                        .map(|c| ident(&c.name))
                        .collect();
                    format!("UPDATE OF {}", names.join(", "))
                }
                e => e.name().into(),
            })
            .collect();
        let args: Vec<String> = self.args.iter().map(|a| format!("'{}'", a.replace('\'', "''"))).collect();
        // REFERENCING OLD TABLE AS x NEW TABLE AS y, as pg_get_triggerdef writes it
        let mut refs = String::new();
        if self.old_table.is_some() || self.new_table.is_some() {
            refs.push_str(" REFERENCING");
            if let Some(o) = &self.old_table {
                refs.push_str(&format!(" OLD TABLE AS {}", ident(o)));
            }
            if let Some(n) = &self.new_table {
                refs.push_str(&format!(" NEW TABLE AS {}", ident(n)));
            }
        }
        // a constraint trigger's FROM and deferral, as pg_get_triggerdef writes them
        let mut cons = String::new();
        if let Some(c) = &self.constraint {
            if let Some(f) = &c.from {
                cons.push_str(&format!(" FROM {}", ident_rel(f)));
            }
            cons.push_str(if c.deferrable { " DEFERRABLE" } else { " NOT DEFERRABLE" });
            cons.push_str(if c.deferred { " INITIALLY DEFERRED" } else { " INITIALLY IMMEDIATE" });
        }
        format!(
            "CREATE {}TRIGGER {} {} {} ON {}{cons}{refs} FOR EACH {}{} EXECUTE FUNCTION {}({})",
            if self.constraint.is_some() { "CONSTRAINT " } else { "" },
            ident(&self.name),
            if self.instead {
                "INSTEAD OF"
            } else if self.before {
                "BEFORE"
            } else {
                "AFTER"
            },
            events.join(" OR "),
            ident_rel(table),
            if self.row { "ROW" } else { "STATEMENT" },
            self.when.as_ref().map_or(String::new(), |w| format!(" WHEN ({w})")),
            ident_rel(&self.func),
            args.join(", ")
        )
    }

    pub(super) fn all_from_json(j: &Json) -> Vec<Trigger> {
        let strs = |x: &Json| -> Vec<String> {
            x.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
        };
        j.as_array()
            .into_iter()
            .flatten()
            .map(|t| Trigger {
                name: t["name"].as_str().unwrap_or_default().into(),
                before: t["timing"] == "before",
                events: strs(&t["events"])
                    .iter()
                    .filter_map(|e| match e.as_str() {
                        "insert" => Some(Event::Insert),
                        "update" => Some(Event::Update),
                        "delete" => Some(Event::Delete),
                        _ => None,
                    })
                    .collect(),
                cols: t["update_of"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_u64())
                    .map(|c| c as u32)
                    .collect(),
                of: vec![],
                row: t["level"] == "row",
                when: t["when"].as_str().map(String::from),
                func: t["function"].as_str().unwrap_or_default().into(),
                args: strs(&t["args"]),
                enabled: t["enabled"] != false,
                mode: t["mode"].as_str().and_then(|m| m.chars().next()).unwrap_or(match t["enabled"] != false {
                    true => 'O',
                    false => 'D',
                }),
                instead: t["timing"] == "instead",
                old_table: t["old_table"].as_str().map(String::from),
                new_table: t["new_table"].as_str().map(String::from),
                truncate: false,
                constraint: t["constraint"].is_object().then(|| ConstraintTrig {
                    deferrable: t["constraint"]["deferrable"] == true,
                    deferred: t["constraint"]["deferred"] == true,
                    from: t["constraint"]["from"].as_str().map(String::from),
                }),
            })
            .collect()
    }
}

/// Whether `t` has row triggers turned on.
pub(super) fn row_triggers(t: &Table) -> bool {
    // (a statement trigger with transition tables reads the rows too)
    t.triggers.iter().any(|x| x.enabled && (x.row || x.old_table.is_some() || x.new_table.is_some()))
}

/// Whether `t` has BEFORE ROW triggers of event `ev` turned on.
pub(super) fn before_row(t: &Table, ev: Event) -> bool {
    t.triggers.iter().any(|x| x.enabled && x.before && x.row && x.events.contains(&ev))
}

/// `t`'s triggers that fire for this event, in name order (as Postgres fires them).
fn firing<'t>(t: &'t Table, ev: Event, before: bool, row: bool, set: &[u32]) -> Vec<&'t Trigger> {
    t.triggers
        .iter()
        .filter(|x| x.enabled && x.before == before && x.row == row && x.events.contains(&ev))
        .filter(|x| ev != Event::Update || x.cols.is_empty() || x.cols.iter().any(|c| set.contains(c)))
        .collect()
}

/// A WHEN condition with its whole rows, `OLD.*` and `NEW.*`, written as ROW(old.a, old.b, ...) of `t`'s columns,
/// which compare (IS DISTINCT FROM) as the rows do.
fn whole_rows(w: &str, t: &Table) -> Result<String> {
    if !w.contains('*') {
        return Ok(w.to_string());
    }
    let toks = lex(w)?;
    let mut out: Vec<String> = vec![];
    let mut i = 0;
    while i < toks.len() {
        if let (Tok::Word(q), Some(Tok::Sym(".")), Some(Tok::Sym("*"))) = (&toks[i], toks.get(i + 1), toks.get(i + 2))
            && (q == "old" || q == "new")
        {
            let fields: Vec<String> =
                t.cols.iter().filter(|c| !c.hidden).map(|c| format!("{q}.{}", ident(&c.name))).collect();
            out.push(format!("row({})", fields.join(", ")));
            i += 3;
            continue;
        }
        out.push(tokens_shown(&toks[i..=i]));
        i += 1;
    }
    Ok(out.join(" "))
}

/// What trigger `x` reads, as it is, of the masked columns `masked` of its table's rows (see `mask::row_reads`): in its
/// WHEN or its function, or every column through its transition tables. A built-in trigger function's: none for
/// tsvector_update_trigger's (its column is masked too: see `mask::of`), the whole row for
/// suppress_redundant_updates_trigger's (whether an UPDATE is skipped tells whether the row was already so).
pub(super) fn trigger_reads_masked(v: &View, x: &Trigger, masked: &[(String, char)]) -> Result<Option<String>> {
    if x.old_table.is_some() || x.new_table.is_some() {
        return Ok(Some("the rows as they are, in its transition tables".into()));
    }
    if !x.row {
        return Ok(None);
    }
    if let Some(w) = &x.when
        && let Some(r) = mask::row_reads(w, masked)
    {
        return Ok(Some(r));
    }
    let f = named(v, &x.func)?.into_iter().find(|f| f.args.is_empty() && f.ret == Ret::Trigger);
    Ok(match f {
        Some(f) => mask::row_reads(&f.body, masked),
        None if x.func == SUPPRESS_REDUNDANT => Some("the whole row".into()),
        None => None,
    })
}

/// Postgres's `"rel" is a view` (a table, ...), with its DETAIL: what that kind of relation can't have.
fn is_a(code: &'static str, rel: &str, what: &str, detail: &str) -> SqlError {
    SqlError {
        code,
        message: format!("\"{rel}\" is a {what}"),
        info: Some(Box::new(Violation { detail: Some(detail.into()), ..Violation::default() })),
    }
}

/// That trigger `x`'s function exists and returns trigger.
fn trigger_function(v: &View, x: &Trigger) -> Result<()> {
    match named(v, &x.func)?.into_iter().find(|f| f.args.is_empty()) {
        // (pg_catalog's own trigger functions, which take their arguments from the trigger: see `tsvector_trigger`)
        None if builtin_trigger(&x.func).is_some() || x.func == SUPPRESS_REDUNDANT => Ok(()),
        None => err(NO_FUNCTION, format!("function {}() does not exist", x.func)),
        Some(f) if f.ret != Ret::Trigger => err("42P17", format!("function {} must return type trigger", x.func)),
        Some(_) => Ok(()),
    }
}

/// pg_catalog's trigger function that skips an UPDATE of a row that changes nothing in it (when none of that name was
/// made).
const SUPPRESS_REDUNDANT: &str = "suppress_redundant_updates_trigger";

/// The trigger functions of pg_catalog that are built in, when none of that name was made: whether the function
/// reads its configuration from a column (`tsvector_update_trigger_column`) rather than naming it.
pub(super) fn builtin_trigger(name: &str) -> Option<bool> {
    match name {
        "tsvector_update_trigger" => Some(false),
        "tsvector_update_trigger_column" => Some(true),
        _ => None,
    }
}

/// The writes that rename the copies of row trigger `name` the partitions of `t` (and theirs) have.
fn rename_copies(v: &View, t: &Table, name: &str, to: &str) -> Result<Vec<(String, Option<String>)>> {
    let (mark, moved) = (format!("trigger:{name}"), format!("trigger:{to}"));
    let mut writes = vec![];
    for mut kid in hier::children(v, &t.name)? {
        if kid.hier.of.is_none() || !kid.hier.inherited.contains(&mark) {
            continue;
        }
        if kid.triggers.iter().any(|x| x.name == to) {
            return err("42710", format!("trigger \"{to}\" for relation \"{}\" already exists", rel_part(&kid.name)));
        }
        for x in kid.triggers.iter_mut().filter(|x| x.name == name) {
            x.name = to.to_string();
        }
        kid.triggers.sort_by(|a, b| a.name.cmp(&b.name));
        for m in kid.hier.inherited.iter_mut().filter(|m| **m == mark) {
            m.clone_from(&moved);
        }
        writes.push((schema_key(&kid.name), Some(schema_json(&kid))));
        if kid.hier.part.is_some() {
            writes.extend(rename_copies(v, &kid, name, to)?);
        }
    }
    Ok(writes)
}

// ---------------------------------------------------------------- parsing

impl Parser {
    /// Runs `f`, or puts the tokens back as they were if it fails.
    pub(super) fn attempt<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Option<T> {
        let (i, saved, typmod, vdim) = (self.i, self.t[self.i..].to_vec(), self.typmod, self.vdim);
        match f(self) {
            Ok(x) => Some(x),
            Err(_) => {
                self.t.truncate(i);
                self.t.extend(saved);
                (self.i, self.typmod, self.vdim) = (i, typmod, vdim);
                None
            }
        }
    }

    /// `name` or `public.name`.
    fn routine_name(&mut self, create: bool, kind: Kind) -> Result<String> {
        let name = self.qualified(create, kind)?;
        if rel_part(&name).contains(['/', '(']) {
            return err(SYNTAX, "a function name can't contain '/' or '('");
        }
        Ok(name)
    }

    /// A parameter: `[IN] [name] type [DEFAULT expr | = expr]`; its default as SQL text, and the NOTICE a
    /// `table.column%TYPE` gives (see `plpgsql::type_reference`).
    #[allow(clippy::type_complexity)]
    fn fn_arg(&mut self) -> Result<(Option<String>, Type, Option<String>, ArgMode, Option<String>)> {
        // the mode before the name, or after it (`a variadic int[]`, `f2 out anyelement`), as Postgres's func_arg
        let mode_word = |p: &Self, k: usize| matches!(p.t.get(k), Some(Tok::Word(w)) if matches!(w.as_str(), "in" | "out" | "inout" | "variadic"));
        let named_first = !mode_word(self, self.i)
            && matches!(self.peek(), Some(Tok::Word(_) | Tok::Ident(_)))
            && mode_word(self, self.i + 1);
        let first = if named_first { Some(self.name()?) } else { None };
        let mode = if self.kw("inout") {
            ArgMode::InOut
        } else if self.kw("out") {
            ArgMode::Out
        } else if self.kw("in") {
            if self.kw("out") { ArgMode::InOut } else { ArgMode::In }
        } else {
            ArgMode::In
        };
        let mode = if self.kw("variadic") { ArgMode::Variadic } else { mode };
        if let Some(name) = first {
            let ty = self.ty()?;
            let default = if self.kw("default") || self.sym("=") { Some(self.default_expr()?.1) } else { None };
            return Ok((Some(name), ty, default, mode, None));
        }
        // [name] table.column%TYPE: the column's type
        let pct = (self.i..self.t.len())
            .take_while(|k| !matches!(self.t[*k], Tok::Sym("," | ")" | "=")))
            .find(|k| self.t[*k] == Tok::Sym("%"));
        if let Some(at) = pct
            && matches!(self.t.get(at + 1), Some(Tok::Word(w)) if w == "type")
        {
            // (the dotted name ends at the %; a word before it is the parameter's name)
            let mut start = at;
            while start >= self.i + 2 && self.t[start - 2] == Tok::Sym(".") {
                start -= 2;
            }
            let start = start.saturating_sub(1).max(self.i);
            let name = if start > self.i { Some(self.name()?) } else { None };
            let mut parts = vec![self.name()?];
            while self.sym(".") {
                parts.push(self.name()?);
            }
            self.i = at + 2;
            let (ty, note) = plpgsql::type_reference(&parts)?;
            let default = if self.kw("default") || self.sym("=") { Some(self.default_expr()?.1) } else { None };
            return Ok((name, ty, default, mode, Some(note)));
        }
        let alone = self.attempt(|p| {
            let t = p.ty()?;
            let ends = matches!(p.peek(), Some(Tok::Sym("," | ")" | "="))) || p.is_kw("default");
            if ends { Ok(t) } else { p.fail("") }
        });
        let (name, ty) = match alone {
            Some(t) => (None, t),
            None => (Some(self.name()?), self.ty()?),
        };
        let default = if self.kw("default") || self.sym("=") { Some(self.default_expr()?.1) } else { None };
        Ok((name, ty, default, mode, None))
    }

    /// `table.column%TYPE`, if that is next: the column's type, and the NOTICE Postgres gives of the reference (see
    /// `plpgsql::type_reference`).
    fn pct_type(&mut self) -> Result<Option<(Type, String)>> {
        let mut k = self.i;
        while matches!(self.t.get(k), Some(Tok::Word(_) | Tok::Ident(_))) && self.t.get(k + 1) == Some(&Tok::Sym(".")) {
            k += 2;
        }
        let named = matches!(self.t.get(k), Some(Tok::Word(_) | Tok::Ident(_)));
        if !(named
            && self.t.get(k + 1) == Some(&Tok::Sym("%"))
            && matches!(self.t.get(k + 2), Some(Tok::Word(w)) if w == "type"))
        {
            return Ok(None);
        }
        let mut parts = vec![self.name()?];
        while self.sym(".") {
            parts.push(self.name()?);
        }
        self.i += 2;
        Ok(Some(plpgsql::type_reference(&parts)?))
    }

    /// What RETURNS says; a `%TYPE` reference's NOTICE is added to `notes`.
    fn fn_returns(&mut self, notes: &mut Vec<String>) -> Result<Ret> {
        if let Some((t, note)) = self.pct_type()? {
            notes.push(note);
            return Ok(Ret::Value(t));
        }
        if self.kw("setof") {
            // SETOF record: rows whose columns the OUT parameters, or the call's column list, name
            if self.kw("record") {
                return Ok(Ret::Table(vec![]));
            }
            // SETOF table.column%TYPE: rows of the column's type
            if let Some((t, note)) = self.pct_type()? {
                notes.push(note);
                return Ok(Ret::SetOf(t));
            }
            // a name that isn't a built-in type: an enum, or a table's rows (see `create_function`)
            return Ok(Ret::SetOf(self.ty()?));
        }
        if self.kw("table") {
            self.expect_sym("(")?;
            let mut cols = vec![];
            loop {
                cols.push((self.name()?, self.ty()?));
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
            return Ok(Ret::Table(cols));
        }
        if self.kw("trigger") {
            return Ok(Ret::Trigger);
        }
        if self.kw("void") || self.kw("event_trigger") {
            return Ok(Ret::Void); // (an event trigger's function is called as one that returns nothing)
        }
        if self.kw("record") {
            return Ok(Ret::Table(vec![]));
        }
        Ok(Ret::Value(self.ty()?)) // a name that isn't a built-in type: an enum (see `create_function`)
    }

    /// After CREATE [OR REPLACE] FUNCTION.
    pub(super) fn create_function(&mut self, replace: bool, procedure: bool) -> Result<Stmt> {
        let name = self.routine_name(true, Kind::Func)?;
        self.expect_sym("(")?;
        let (mut args, mut defaults, mut outs) = (vec![], vec![], vec![]);
        let (mut ret_setof, mut ret_record) = (false, false);
        let mut variadic = false;
        // each parameter's name and mode so far, for interpret_function_parameter_list's checks
        let mut seen: Vec<(Option<String>, ArgMode)> = vec![];
        // the NOTICEs its `%TYPE` references give, parameters first (see `Function::type_notes`)
        let mut type_notes = vec![];
        if !self.sym(")") {
            loop {
                let (n, t, d, mode, note) = self.fn_arg()?;
                type_notes.extend(note);
                if mode != ArgMode::Out && variadic {
                    return err("42P13", "VARIADIC parameter must be the last input parameter");
                }
                if mode == ArgMode::Out && variadic && procedure {
                    return err("42P13", "VARIADIC parameter must be the last parameter");
                }
                if mode == ArgMode::Variadic && !matches!(t, Type::Array(_) | Type::Poly(1 | 6) | Type::Enum(_)) {
                    return err("42P13", "VARIADIC parameter must be an array");
                }
                // (an input's name may be an output's too, as Postgres allows; not another input's or output's)
                let input = |m: ArgMode| matches!(m, ArgMode::In | ArgMode::Variadic);
                if let Some(name) = n.as_deref().filter(|n| !n.is_empty())
                    && seen.iter().any(|(pn, pm)| {
                        pn.as_deref() == Some(name)
                            && !(*pm == ArgMode::Out && input(mode))
                            && !(input(*pm) && mode == ArgMode::Out)
                    })
                {
                    return err("42P13", format!("parameter name \"{name}\" used more than once"));
                }
                seen.push((n.clone(), mode));
                if d.is_some() && mode == ArgMode::Out {
                    return err("42P13", "only input parameters can have default values");
                }
                if matches!(mode, ArgMode::Out | ArgMode::InOut) {
                    outs.push((n.clone().unwrap_or_else(|| format!("column{}", outs.len() + 1)), t));
                }
                variadic |= mode == ArgMode::Variadic;
                if mode != ArgMode::Out {
                    if d.is_none() && defaults.iter().any(Option::is_some) {
                        return err("42P13", "input parameters after one with a default value must also have defaults");
                    }
                    args.push((n, t));
                    defaults.push(d);
                }
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
        }
        let (mut ret, mut lang, mut volatility, mut strict, mut body, mut inline) =
            (None, None, Volatility::Volatile, false, None, false);
        let mut definer = false;
        let mut config: Vec<(String, Vec<String>)> = vec![];
        // LANGUAGE internal: the body names one of Postgres's built-in functions by its C symbol
        let mut internal = false;
        // LANGUAGE C: the body is the object file, and this its symbol (AS 'file', 'symbol'; the function's name
        // when not given)
        let (mut c_lang, mut c_symbol) = (false, None);
        let mut cost: Option<f64> = None;
        let mut tuning = Tuning::default();
        let mut std_form: StdForm = None;
        loop {
            if procedure && self.is_kw("returns") {
                return self.fail("a procedure returns nothing, so it has no RETURNS");
            }
            // (what only tunes a function's calls a procedure doesn't have, as Postgres's compute_common_attribute
            // says, where it is written)
            let only_functions = [
                "immutable",
                "stable",
                "volatile",
                "strict",
                "called",
                "leakproof",
                "cost",
                "rows",
                "support",
                "parallel",
                "window",
            ];
            if procedure
                && (matches!(self.peek(), Some(Tok::Word(w)) if only_functions.contains(&w.as_str()))
                    || (self.is_kw("not") && matches!(self.t.get(self.i + 1), Some(Tok::Word(w)) if w == "leakproof")))
            {
                return Err(SqlError {
                    code: "42P13",
                    message: "invalid attribute in procedure definition".into(),
                    info: Some(Box::new(Violation {
                        position: self.starts.get(self.i).map(|s| *s as u32 + 1),
                        ..Violation::default()
                    })),
                });
            }
            if self.kw("returns") {
                if self.kw("null") {
                    self.expect_kw("on")?;
                    self.expect_kw("null")?;
                    self.expect_kw("input")?;
                    strict = true;
                } else {
                    let setof = self.is_kw("setof");
                    let record = matches!(self.t.get(self.i + usize::from(setof)), Some(Tok::Word(w)) if w == "record");
                    (ret_setof, ret_record) = (setof, record);
                    ret = Some(self.fn_returns(&mut type_notes)?);
                }
            } else if self.kw("language") {
                let l = match self.next() {
                    Some(Tok::Word(w) | Tok::Ident(w) | Tok::Str(w)) => w.to_lowercase(),
                    _ => return self.fail("expected a language"),
                };
                internal = l == "internal";
                c_lang = l == "c";
                lang = Some(match l.as_str() {
                    "sql" => Lang::Sql,
                    "plpgsql" => Lang::PlPgSql,
                    // (run as the SQL that does what the built-in does: see `userdefs::internal_sql`)
                    "internal" => Lang::Sql,
                    // (the functions of the regression tests' library that are here: see `regress_c`)
                    "c" => Lang::Sql,
                    _ => {
                        return err(
                            UNSUPPORTED,
                            format!("language \"{l}\" isn't supported: functions are LANGUAGE sql or plpgsql"),
                        );
                    }
                });
            } else if self.kw("immutable") {
                volatility = Volatility::Immutable;
            } else if self.kw("stable") {
                volatility = Volatility::Stable;
            } else if self.kw("volatile") {
                volatility = Volatility::Volatile;
            } else if self.kw("strict") {
                strict = true;
            } else if self.kw("called") {
                self.expect_kw("on")?;
                self.expect_kw("null")?;
                self.expect_kw("input")?;
                strict = false;
            } else if self.kw("not") {
                self.expect_kw("leakproof")?;
                tuning.leakproof = false;
            } else if self.kw("leakproof") {
                tuning.leakproof = true;
            } else if self.kw("external") || self.is_kw("security") {
                self.expect_kw("security")?;
                if self.kw("definer") {
                    definer = true;
                } else if self.kw("invoker") {
                    definer = false;
                } else {
                    return self.fail("expected DEFINER or INVOKER");
                }
            } else if self.kw("parallel") {
                tuning.parallel = Some(if self.kw("safe") {
                    's'
                } else if self.kw("restricted") {
                    'r'
                } else if self.kw("unsafe") {
                    'u'
                } else {
                    return self.fail("expected SAFE, UNSAFE or RESTRICTED");
                });
            } else if self.is_kw("cost") || self.is_kw("rows") {
                let is_cost = self.kw("cost");
                if !is_cost {
                    self.kw("rows");
                }
                let n = match self.next() {
                    Some(Tok::Num(n)) => n,
                    _ => return self.fail("expected a number"),
                };
                let x: f64 = n.parse().unwrap_or(0.0);
                if is_cost {
                    if x <= 0.0 {
                        return err("22023", "COST must be positive");
                    }
                    cost = Some(x);
                } else {
                    if x <= 0.0 {
                        return err("22023", "ROWS must be positive");
                    }
                    tuning.rows = Some(x);
                }
            } else if self.kw("as") {
                if inline {
                    return err("42P13", "duplicate function body specified");
                }
                match self.next() {
                    Some(Tok::Str(s)) => body = Some(s),
                    _ => return self.fail("expected the function's body as a string, like $$ ... $$"),
                }
                if self.sym(",") {
                    match self.next() {
                        Some(Tok::Str(s)) => c_symbol = Some(s),
                        _ => return self.fail("expected the function's link symbol as a string"),
                    }
                }
            } else if self.is_kw("return") || self.is_kw("begin") && body.is_some() {
                // RETURN expression: the SQL standard's body (where AS gave one too, two bodies)
                if body.is_some() {
                    return err("42P13", "duplicate function body specified");
                }
                self.i += 1;
                let (start, toks) = (self.i, self.t[self.i..].to_vec());
                self.expr()?;
                body = Some(format!("select {}", tokens_sql(&toks[..self.i - start])));
                inline = true;
            } else if self.is_kw("begin") && self.t.get(self.i + 1) == Some(&Tok::Word("atomic".into())) {
                self.i += 2;
                let (text, returns) = self.atomic_body()?;
                body = Some(text);
                std_form = Some(returns);
                inline = true;
            } else if self.kw("set") {
                // (FROM CURRENT is read, not kept: the session's value then isn't captured)
                if let (name, Some(values)) = self.routine_setting()? {
                    config.retain(|(n, _)| *n != name);
                    config.push((name, values));
                }
            } else if let Some(Tok::Word(w)) = self
                .peek()
                .filter(|t| matches!(t, Tok::Word(w) if ["window", "support", "transform"].contains(&w.as_str())))
            {
                return err(UNSUPPORTED, format!("{} in CREATE FUNCTION isn't supported", w.to_uppercase()));
            } else {
                break;
            }
        }
        // a procedure returns nothing; it is stored as a function that returns void, which CALL runs
        let ret = match ret {
            _ if procedure => Some(Ret::Void),
            None if !outs.is_empty() => Some(Ret::Table(outs.clone())),
            // RETURNS [SETOF] record with OUT parameters: they are its columns
            Some(Ret::Table(c)) if c.is_empty() && !outs.is_empty() => Some(Ret::Table(outs.clone())),
            // RETURNS [SETOF] t with one OUT parameter of type t: its column, named as it is
            Some(Ret::Value(t) | Ret::SetOf(t)) if matches!(&outs[..], [(_, o)] if *o == t) => {
                Some(Ret::Table(outs.clone()))
            }
            r => r,
        };
        let Some(ret) = ret else { return err("42P13", "function result type must be specified") };
        if c_symbol.is_some() && !c_lang {
            return err(UNSUPPORTED, "functions in object files (AS 'file', 'symbol') aren't supported");
        }
        // LANGUAGE C: one of the regression tests' library's functions that are here, run as the SQL that calls it;
        // any other C function isn't
        let mut c_trigger = None;
        let body = match (c_lang, body) {
            (true, Some(file)) => {
                let symbol = c_symbol.clone().unwrap_or_else(|| rel_part(&name).to_string());
                let types: Vec<Type> = args.iter().map(|a| a.1).collect();
                match regress_c(&file, &symbol, &types) {
                    Some((l, sql)) => {
                        lang = Some(l);
                        // (a C trigger function is kept as the C it is, which runs when it fires: see `c_trigger_of`)
                        if C_TRIGGERS.iter().any(|(s, _)| *s == symbol) {
                            c_trigger = Some((file, symbol));
                        }
                        Some(sql)
                    }
                    None => {
                        return err(
                            UNSUPPORTED,
                            "language \"c\" isn't supported: functions are LANGUAGE sql or plpgsql",
                        );
                    }
                }
            }
            (_, b) => b,
        };
        let Some(body) = body else { return err("42P13", "no function body specified") };
        // LANGUAGE internal AS 'int4pl': the SQL of what that built-in does (`$1::integer + $2::integer`), or a
        // call of the function it is (`dsin` is sin); a symbol Postgres has no function for is refused, as there
        let body = match internal && !inline {
            true => {
                let symbol = body.trim();
                let Some((_, _, _, set)) = catalog_schema::internal_function(symbol) else {
                    return err(NO_FUNCTION, format!("there is no built-in function named \"{symbol}\""));
                };
                let e = userdefs::internal_sql(symbol).unwrap_or_default();
                if set { format!("select * from {e}") } else { format!("select {e}") }
            }
            false => body,
        };
        // (the standard's body is parsed as it is made, for argument types it can't know if polymorphic)
        if inline && args.iter().any(|a| matches!(a.1, Type::Poly(_))) {
            return err("42P13", "SQL function with unquoted function body cannot have polymorphic arguments");
        }
        let lang = match (lang, inline) {
            (None, true) | (Some(Lang::Sql), _) => Lang::Sql,
            (Some(Lang::PlPgSql), true) => return err("42P13", "inline SQL function body only valid for language SQL"),
            (Some(l), false) => l,
            (None, false) => return err("42P13", "no language specified"),
        };
        if ret == Ret::Trigger && !args.is_empty() {
            return err("42P13", "trigger functions cannot have declared arguments");
        }
        // a polymorphic result needs a polymorphic argument to say what it is
        let rets: Vec<Type> = match &ret {
            Ret::Value(t) | Ret::SetOf(t) => vec![*t],
            Ret::Table(c) => c.iter().map(|(_, t)| *t).collect(),
            _ => vec![],
        };
        poly_signature(&rets, &args.iter().map(|a: &(Option<String>, Type)| a.1).collect::<Vec<_>>())?;
        // a default has to be a value of its parameter's type, or one converted to it on assignment (a number to
        // another number, anything to text)
        let number = |t: Type| {
            matches!(t, Type::SmallInt | Type::Int | Type::BigInt | Type::Numeric | Type::Float | Type::Float4)
        };
        let assigned = |g: Option<Type>, w: Type| g.is_some_and(|g| (number(g) && number(w)) || w == Type::Text);
        for (i, d) in defaults.iter().enumerate() {
            let Some(text) = d else { continue };
            let bound = Binder::plain(&[], None).bind(&check_expr(text)?)?;
            let given = match &bound {
                Expr::Lit(Value::Null | Value::Text(_)) | Expr::Param(_) => None,
                b => ty_in(b, &[], &[], &[]).ok(),
            };
            if fit(given, args[i].1).is_none() && !assigned(given, args[i].1) {
                return err(
                    "42804",
                    format!(
                        "argument of DEFAULT must be type {}, not type {}",
                        args[i].1.name(),
                        given.map_or("unknown".into(), |t| t.name())
                    ),
                );
            }
        }
        let mut f = Function {
            name,
            args,
            declared: vec![],
            defaults,
            ret,
            lang,
            volatility,
            strict,
            body,
            procedure,
            oid: None,
            definer,
            // (SETOF record with OUT parameters is a RETURNS TABLE of them: its rows come from RETURN NEXT and QUERY)
            outs: !outs.is_empty() && !ret_setof,
            single: ret_record && !ret_setof && outs.is_empty(),
            variadic,
            cost,
            std_body: inline,
            config,
            tuning,
            std_form,
            type_notes,
            shapes: vec![],
            c_trigger,
            code: Code::Sql(vec![]),
        };
        f.compile()?;
        Ok(Stmt::CreateFunction(Arc::new(f), replace, procedure))
    }

    /// After BEGIN ATOMIC: statements up to END, as SQL, and for each whether it was a RETURN.
    fn atomic_body(&mut self) -> Result<(String, Vec<bool>)> {
        let (mut parts, mut returns) = (vec![], vec![]);
        loop {
            while self.sym(";") {}
            if self.kw("end") {
                return Ok((parts.join("; "), returns));
            }
            // RETURN expression: a statement of the standard's body, the value the function gives
            if self.kw("return") {
                let (start, toks) = (self.i, self.t[self.i..].to_vec());
                self.expr()?;
                parts.push(format!("select {}", tokens_sql(&toks[..self.i - start])));
                returns.push(true);
            } else {
                let (start, toks) = (self.i, self.t[self.i..].to_vec());
                self.stmt()?;
                parts.push(tokens_sql(&toks[..self.i - start]));
                returns.push(false);
            }
            if !self.sym(";") {
                self.expect_kw("end")?;
                return Ok((parts.join("; "), returns));
            }
        }
    }

    /// After DO: `[LANGUAGE l] 'block' [LANGUAGE l]`, run as a function with no arguments that
    /// returns nothing.
    pub(super) fn do_block(&mut self) -> Result<Stmt> {
        let mut lang = Lang::PlPgSql;
        let mut body = None;
        loop {
            if self.kw("language") {
                let l = match self.next() {
                    Some(Tok::Word(w) | Tok::Ident(w) | Tok::Str(w)) => w.to_lowercase(),
                    _ => return self.fail("expected a language"),
                };
                lang = match l.as_str() {
                    "plpgsql" => Lang::PlPgSql,
                    "sql" => return err(UNSUPPORTED, "DO takes LANGUAGE plpgsql: SQL has no anonymous blocks"),
                    _ => return err(UNSUPPORTED, format!("language \"{l}\" isn't supported: DO is LANGUAGE plpgsql")),
                };
            } else if let (None, Some(Tok::Str(_))) = (&body, self.peek()) {
                let Some(Tok::Str(s)) = self.next() else { unreachable!() };
                body = Some(s);
            } else {
                break;
            }
        }
        let Some(body) = body else { return self.fail("expected the block as a string, like $$ ... $$") };
        let mut f = Function {
            name: "inline_code_block".into(),
            args: vec![],
            declared: vec![],
            defaults: vec![],
            ret: Ret::Void,
            lang,
            volatility: Volatility::Volatile,
            strict: false,
            body,
            procedure: false,
            oid: None,
            definer: false,
            outs: false,
            single: false,
            variadic: false,
            cost: None,
            std_body: false,
            config: vec![],
            tuning: Tuning::default(),
            std_form: None,
            type_notes: vec![],
            shapes: vec![],
            c_trigger: None,
            code: Code::Sql(vec![]),
        };
        f.compile()?;
        self.calls = true;
        let call = Expr::Call(Box::new(Call {
            name: f.name.clone(),
            args: vec![],
            names: vec![],
            def: Some(Arc::new(f)),
            via_call: true,
            builtin: false,
        }));
        Ok(Stmt::Do(Box::new(select_of(call))))
    }

    /// After CALL: `name(args)`, a procedure.
    pub(super) fn call_statement(&mut self) -> Result<Stmt> {
        // (a plain name: the procedures of that name in every schema on the search_path)
        let name = match self.t.get(self.i + 1) {
            Some(Tok::Sym(".")) => self.routine_name(false, Kind::Func)?,
            _ => func_named(&self.name()?),
        };
        self.expect_sym("(")?;
        let mut call = self.call(name)?;
        match &mut call {
            Expr::Call(c) => c.via_call = true, // (checked to be a procedure when it's bound)
            _ => return err("42809", "CALL takes a procedure"),
        }
        if rls::has_sublinks(&call) {
            return err(UNSUPPORTED, "cannot use subquery in CALL argument");
        }
        Ok(Stmt::Call(Box::new(select_of(call))))
    }

    /// After DROP FUNCTION.
    pub(super) fn drop_function(&mut self, procedure: bool) -> Result<Stmt> {
        let if_exists = self.kw("if");
        if if_exists {
            self.expect_kw("exists")?;
        }
        let mut names = vec![];
        loop {
            names.push(self.routine_ref()?);
            if !self.sym(",") {
                break;
            }
        }
        let cascade = self.kw("cascade");
        if !cascade {
            self.kw("restrict");
        }
        Ok(Stmt::DropFunction { names, if_exists, cascade, procedure, routine: false })
    }

    /// A routine as DROP and ALTER name it: its name (a plain one: the routines of that name in every schema on the
    /// search_path) and, if written, its argument types (an OUT parameter's not among them).
    fn routine_ref(&mut self) -> Result<(String, Option<Vec<Type>>)> {
        let name = match self.t.get(self.i + 1) {
            Some(Tok::Sym(".")) => self.routine_name(false, Kind::Func)?,
            _ => func_named(&self.name()?),
        };
        let types = if self.sym("(") {
            let mut ts = vec![];
            if !self.sym(")") {
                loop {
                    let (_, t, _, mode, _) = self.fn_arg()?;
                    if mode != ArgMode::Out {
                        ts.push(t);
                    }
                    if !self.sym(",") {
                        break;
                    }
                }
                self.expect_sym(")")?;
            }
            Some(ts)
        } else {
            None
        };
        Ok((name, types))
    }

    /// After ALTER FUNCTION, PROCEDURE or ROUTINE (`kind`): `name [(argument types)]` and what is done to it: OWNER
    /// TO, RENAME TO, SET SCHEMA, [NO] DEPENDS ON EXTENSION, or attributes (each with where it is written, for the
    /// error a procedure's that only a function has gives).
    pub(super) fn alter_routine(&mut self, kind: RoutineKind) -> Result<Stmt> {
        let (name, types) = self.routine_ref()?;
        if self.kw("owner") {
            self.expect_kw("to")?;
            let procedure = kind == RoutineKind::Procedure;
            return Ok(Stmt::AlterOwner(acl::Owned::Routine(name, types, procedure), self.role_spec(false)?));
        }
        let action = if self.kw("rename") {
            self.expect_kw("to")?;
            RoutineAlter::Rename(self.name()?)
        } else if self.is_kw("set") && matches!(self.t.get(self.i + 1), Some(Tok::Word(w)) if w == "schema") {
            self.i += 2;
            RoutineAlter::Schema(self.name()?)
        } else if self.is_kw("depends")
            || (self.is_kw("no") && matches!(self.t.get(self.i + 1), Some(Tok::Word(w)) if w == "depends"))
        {
            // (an extension's own objects aren't dropped with it here: nothing to record)
            self.skip_rest();
            return Ok(Stmt::NoopNotice(kind.tag(), "extensions don't own functions here: nothing to record"));
        } else {
            let mut attrs = vec![];
            loop {
                let at = self.starts.get(self.i).map_or(0, |s| *s as u32 + 1);
                let attr = if self.kw("immutable") {
                    RoutineAttr::Volatility(Volatility::Immutable)
                } else if self.kw("stable") {
                    RoutineAttr::Volatility(Volatility::Stable)
                } else if self.kw("volatile") {
                    RoutineAttr::Volatility(Volatility::Volatile)
                } else if self.kw("strict") {
                    RoutineAttr::Strict(true)
                } else if self.kw("called") {
                    self.expect_kw("on")?;
                    self.expect_kw("null")?;
                    self.expect_kw("input")?;
                    RoutineAttr::Strict(false)
                } else if self.kw("returns") {
                    self.expect_kw("null")?;
                    self.expect_kw("on")?;
                    self.expect_kw("null")?;
                    self.expect_kw("input")?;
                    RoutineAttr::Strict(true)
                } else if self.kw("leakproof") {
                    RoutineAttr::Leakproof(true)
                } else if self.is_kw("not") && matches!(self.t.get(self.i + 1), Some(Tok::Word(w)) if w == "leakproof")
                {
                    self.i += 2;
                    RoutineAttr::Leakproof(false)
                } else if self.kw("external") || self.is_kw("security") {
                    self.expect_kw("security")?;
                    if self.kw("definer") {
                        RoutineAttr::Definer(true)
                    } else {
                        self.expect_kw("invoker")?;
                        RoutineAttr::Definer(false)
                    }
                } else if self.kw("parallel") {
                    let p = match self.next() {
                        Some(Tok::Word(w) | Tok::Ident(w) | Tok::Str(w)) => w.to_lowercase(),
                        _ => return self.fail("expected SAFE, UNSAFE or RESTRICTED"),
                    };
                    match p.as_str() {
                        "safe" => RoutineAttr::Parallel('s'),
                        "restricted" => RoutineAttr::Parallel('r'),
                        "unsafe" => RoutineAttr::Parallel('u'),
                        _ => {
                            return err("22023", "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE");
                        }
                    }
                } else if self.is_kw("cost") || self.is_kw("rows") {
                    let cost = self.kw("cost");
                    if !cost {
                        self.kw("rows");
                    }
                    let n: f64 = match self.next() {
                        Some(Tok::Num(n)) => n.parse().unwrap_or(0.0),
                        _ => return self.fail("expected a number"),
                    };
                    if cost { RoutineAttr::Cost(n) } else { RoutineAttr::Rows(n) }
                } else if self.kw("support") {
                    self.routine_name(false, Kind::Func)?;
                    RoutineAttr::Support
                } else if self.kw("reset") {
                    if self.kw("all") {
                        RoutineAttr::Reset(None)
                    } else {
                        let mut n = self.name()?;
                        while self.sym(".") {
                            n = format!("{n}.{}", self.name()?);
                        }
                        RoutineAttr::Reset(Some(n))
                    }
                } else if self.kw("set") {
                    let (n, values) = self.routine_setting()?;
                    RoutineAttr::Set(n, values)
                } else {
                    break;
                };
                attrs.push((attr, at));
            }
            if attrs.is_empty() {
                return self.fail("expected RENAME TO, OWNER TO, SET SCHEMA or an attribute");
            }
            self.kw("restrict");
            RoutineAlter::Attrs(attrs)
        };
        Ok(Stmt::AlterRoutine { name, types, kind, action })
    }

    /// After a routine's SET: `name { TO | = } value [, ...]`, its values (search_path's string split into its
    /// schemas, DEFAULT its default), or `name FROM CURRENT` (None: the session's value then, not captured here).
    fn routine_setting(&mut self) -> Result<(String, Option<Vec<String>>)> {
        let mut name = self.name()?;
        while self.sym(".") {
            name = format!("{name}.{}", self.name()?);
        }
        if self.kw("from") {
            self.expect_kw("current")?;
            return Ok((name, None));
        }
        if !self.kw("to") {
            self.expect_sym("=")?;
        }
        let path = name == "search_path";
        let mut values = vec![];
        if path && self.kw("default") {
            values = vec!["$user".to_string(), "public".to_string()];
        } else {
            loop {
                match self.next() {
                    Some(Tok::Str(v)) if path => values.extend(super::path_items(&v)),
                    Some(Tok::Str(v) | Tok::Num(v) | Tok::Word(v) | Tok::Ident(v)) => values.push(v),
                    _ => return self.fail("expected a setting value"),
                }
                if !self.sym(",") {
                    break;
                }
            }
        }
        Ok((name, Some(values)))
    }

    /// After CREATE [OR REPLACE] TRIGGER, or CREATE CONSTRAINT TRIGGER (`constraint`): AFTER only, FROM and the
    /// deferral after ON, then FOR EACH ROW, as Postgres's grammar has it.
    pub(super) fn create_trigger(&mut self, replace: bool, constraint: bool) -> Result<Stmt> {
        let name = self.name()?;
        let mut instead = false;
        if constraint && !self.is_kw("after") {
            return self.fail("expected AFTER");
        }
        let before = if self.kw("before") {
            true
        } else if self.kw("after") {
            false
        } else if self.kw("instead") {
            self.expect_kw("of")?;
            instead = true;
            false
        } else {
            return self.fail("expected BEFORE or AFTER");
        };
        let (mut events, mut of, mut truncate) = (vec![], vec![], false);
        loop {
            let had = (events.len(), truncate);
            if self.kw("insert") {
                events.push(Event::Insert);
            } else if self.kw("update") {
                events.push(Event::Update);
                if self.kw("of") {
                    loop {
                        of.push(self.name()?);
                        if !self.sym(",") {
                            break;
                        }
                    }
                }
            } else if self.kw("delete") {
                events.push(Event::Delete);
            } else if self.kw("truncate") {
                truncate = true;
            } else {
                return self.fail("expected INSERT, UPDATE or DELETE");
            }
            // the grammar's own check: an event twice (UPDATE OR UPDATE OF a), at the token after it
            let again = match events.len() > had.0 {
                true => events[..had.0].contains(&events[had.0]),
                false => had.1,
            };
            if again {
                let mut e = self.fail::<()>("").unwrap_err();
                e.message = e.message.replacen("syntax error", "duplicate trigger events specified", 1);
                return Err(e);
            }
            if !self.kw("or") {
                break;
            }
        }
        events.sort();
        self.expect_kw("on")?;
        let table = self.routine_name(false, Kind::Rel)?;
        self.in_schema_being_made(&table)?;
        // a constraint trigger's [FROM table] and [NOT] DEFERRABLE / INITIALLY {IMMEDIATE | DEFERRED], in any order
        let mut cons = None;
        if constraint {
            let from = if self.kw("from") { Some(self.routine_name(false, Kind::Rel)?) } else { None };
            let (mut deferrable, mut initially): (Option<bool>, Option<bool>) = (None, None);
            loop {
                if self.is_kw("not") && matches!(self.t.get(self.i + 1), Some(Tok::Word(w)) if w == "deferrable") {
                    if deferrable == Some(true) {
                        return err(SYNTAX, "conflicting constraint properties");
                    }
                    self.i += 2;
                    deferrable = Some(false);
                } else if self.is_kw("deferrable") {
                    if deferrable == Some(false) {
                        return err(SYNTAX, "conflicting constraint properties");
                    }
                    self.i += 1;
                    deferrable = Some(true);
                } else if self.kw("initially") {
                    let d = if self.kw("deferred") {
                        true
                    } else {
                        self.expect_kw("immediate")?;
                        false
                    };
                    if initially.is_some_and(|i| i != d) {
                        return err(SYNTAX, "conflicting constraint properties");
                    }
                    initially = Some(d);
                } else {
                    break;
                }
                if deferrable == Some(false) && initially == Some(true) {
                    return err(SYNTAX, "constraint declared INITIALLY DEFERRED must be DEFERRABLE");
                }
            }
            // (INITIALLY DEFERRED makes it deferrable, as Postgres's processCASbits reads it)
            let deferred = initially == Some(true);
            cons = Some(ConstraintTrig { deferrable: deferrable == Some(true) || deferred, deferred, from });
            self.expect_kw("for")?;
            self.expect_kw("each")?;
            self.expect_kw("row")?;
        }
        // REFERENCING { OLD | NEW } TABLE [AS] name ...: the statement's rows, as tables the function reads
        let (mut old_table, mut new_table) = (None, None);
        if !constraint && self.kw("referencing") {
            while self.is_kw("old") || self.is_kw("new") {
                let old = self.kw("old");
                if !old {
                    self.expect_kw("new")?;
                }
                if self.kw("row") {
                    return Err(SqlError {
                        code: UNSUPPORTED,
                        message: "ROW variable naming in the REFERENCING clause is not supported".into(),
                        info: Some(Box::new(Violation {
                            hint: Some("Use OLD TABLE or NEW TABLE for naming transition tables.".into()),
                            ..Violation::default()
                        })),
                    });
                }
                self.expect_kw("table")?;
                self.kw("as");
                let n = Some(self.name()?);
                let slot = if old { &mut old_table } else { &mut new_table };
                if slot.is_some() {
                    let which = if old { "OLD" } else { "NEW" };
                    return err("42P17", format!("{which} TABLE cannot be specified multiple times"));
                }
                *slot = n;
            }
        }
        if !constraint && ["from", "deferrable", "not", "initially"].iter().any(|w| self.is_kw(w)) {
            // (Postgres's grammar has them only in CREATE CONSTRAINT TRIGGER)
            return self.fail("");
        }
        let mut row = constraint; // FOR EACH STATEMENT, Postgres's default; a constraint trigger's is ROW
        if !constraint && self.kw("for") {
            self.kw("each");
            row = self.kw("row");
            if !row {
                self.expect_kw("statement")?;
            }
        }
        // (the tokens of WHEN, kept to point at a reference it may not make)
        let (when_at, when_toks) = (self.i, self.ahead(false));
        let when = if self.kw("when") { Some(self.check_text()?) } else { None };
        self.expect_kw("execute")?;
        if !self.kw("function") {
            self.expect_kw("procedure")?;
        }
        let func = self.routine_name(false, Kind::Func)?;
        self.expect_sym("(")?;
        let mut args = vec![];
        if !self.sym(")") {
            loop {
                args.push(match self.next() {
                    Some(Tok::Str(s) | Tok::Num(s) | Tok::Word(s) | Tok::Ident(s)) => s,
                    _ => return self.fail("expected a string, number or name"),
                });
                if !self.sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
        }
        if let Some(w) = &when {
            let mut bad = None;
            walk(&check_expr(w)?, &mut |x| match x {
                Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) => {
                    bad.get_or_insert((UNSUPPORTED, "cannot use subquery in trigger WHEN condition".to_string(), None));
                }
                Expr::Col(Some(q), c) if q == "old" || q == "new" => {
                    let system = matches!(c.as_str(), "tableoid" | "ctid" | "xmin" | "xmax" | "cmin" | "cmax");
                    let (code, msg) = if !row {
                        ("42P17", Some("statement trigger's WHEN condition cannot reference column values"))
                    } else if q == "old" && events.contains(&Event::Insert) {
                        ("42P17", Some("INSERT trigger's WHEN condition cannot reference OLD values"))
                    } else if q == "new" && events.contains(&Event::Delete) {
                        ("42P17", Some("DELETE trigger's WHEN condition cannot reference NEW values"))
                    } else if q == "new" && before && system {
                        (UNSUPPORTED, Some("BEFORE trigger's WHEN condition cannot reference NEW system columns"))
                    } else {
                        ("", None)
                    };
                    if let Some(m) = msg {
                        bad.get_or_insert((code, m.to_string(), Some((q.clone(), c.clone()))));
                    }
                }
                Expr::Col(q, c) => {
                    bad.get_or_insert((
                        NO_COLUMN,
                        format!(
                            "column \"{}\" does not exist: WHEN reads NEW.column and OLD.column",
                            q.as_ref().map_or(c.clone(), |q| format!("{q}.{c}"))
                        ),
                        None,
                    ));
                }
                _ => {}
            });
            if let Some((code, message, at)) = bad {
                // (pointing at the reference, as Postgres's parser_errposition does)
                let word = |t: &Tok, w: &str| matches!(t, Tok::Word(x) | Tok::Ident(x) if x.eq_ignore_ascii_case(w));
                let place = at.and_then(|(q, c)| {
                    (0..when_toks.len()).find(|&k| {
                        when_toks.get(k).is_some_and(|t| word(t, &q))
                            && matches!(when_toks.get(k + 1), Some(Tok::Sym(".")))
                            && when_toks.get(k + 2).is_some_and(|t| word(t, &c) || (c == "*" && *t == Tok::Sym("*")))
                    })
                });
                let position = place.and_then(|k| self.starts.get(when_at + k)).map(|s| *s as u32 + 1);
                return Err(SqlError {
                    code,
                    message,
                    info: position.map(|p| Box::new(Violation { position: Some(p), ..Violation::default() })),
                });
            }
        }
        let t = Trigger {
            name,
            before,
            events,
            cols: vec![],
            of,
            row,
            when,
            func,
            args,
            enabled: true,
            mode: 'O',
            instead,
            old_table,
            new_table,
            truncate,
            constraint: cons,
        };
        if replace && constraint {
            return err(UNSUPPORTED, "CREATE OR REPLACE CONSTRAINT TRIGGER is not supported");
        }
        Ok(Stmt::CreateTrigger(Box::new(t), table, replace))
    }

    /// After DROP TRIGGER.
    pub(super) fn drop_trigger(&mut self) -> Result<Stmt> {
        let if_exists = self.kw("if");
        if if_exists {
            self.expect_kw("exists")?;
        }
        let name = self.name()?;
        self.expect_kw("on")?;
        let table = self.routine_name(false, Kind::Rel)?;
        let _ = self.kw("cascade") || self.kw("restrict");
        Ok(Stmt::DropTrigger { name, table, if_exists })
    }

    /// After ALTER TRIGGER: `name ON table RENAME TO new` (Postgres's grammar has no ONLY here).
    pub(super) fn alter_trigger(&mut self) -> Result<Stmt> {
        let name = self.name()?;
        self.expect_kw("on")?;
        if self.is_kw("only") {
            return self.fail("");
        }
        let table = self.routine_name(false, Kind::Rel)?;
        if !self.kw("rename") {
            return self.fail("expected RENAME TO");
        }
        self.expect_kw("to")?;
        let to = self.name()?;
        Ok(Stmt::RenameTrigger { name, table, to })
    }
}

// ---------------------------------------------------------------- finding functions

/// The functions whose bodies are the SQL standard's (see `Function::std_body`).
pub(super) fn std_bodies(v: &View) -> Result<Vec<Arc<Function>>> {
    let mut out = vec![];
    for r in v.prefix(&format!("{FUNCTIONS}/"))? {
        let raw = r?.1;
        if !raw.contains("\"std_body\"") {
            continue; // (not one: nothing to parse)
        }
        let f = Function::from_raw(&raw)?;
        if f.std_body {
            out.push(f);
        }
    }
    Ok(out)
}

impl Function {
    /// A SQL body's statements, as parsed (none for a PL/pgSQL one).
    pub(super) fn statements(&self) -> &[Stmt] {
        match &self.code {
            Code::Sql(s) => s,
            Code::Pl(_) => &[],
        }
    }

    /// The function as Postgres's getObjectDescription names it: `function f(integer,text)`.
    pub(super) fn described(&self) -> String {
        let what = if self.procedure { "procedure" } else { "function" };
        format!("{what} {}({})", self.name, self.args.iter().map(|a| type_named(&a.1)).collect::<Vec<_>>().join(","))
    }
}

/// The functions taking or returning enum type `name`.
/// A stored function read for what it names (its types, its signature), not to run: not through `from_raw`'s cache,
/// which keeps a function with its own search_path bound in the world reading it, and a DROP or ALTER reading it
/// there would leave that binding for its next call, made before the statement's change.
fn stored(raw: &str) -> Result<Arc<Function>> {
    let j: Json = serde_json::from_str(raw).unwrap_or_default();
    Ok(Arc::new(Function::from_json(&j)?))
}

pub(super) fn using_type(v: &View, name: &str) -> Result<Vec<Arc<Function>>> {
    let is = |t: &Type| match t {
        // (by its name: a table's row type is a name not looked up, `f(t onek)`)
        Type::Enum(i) => enum_name(*i) == name,
        Type::Base(i) => userdefs::base_name(*i) == name,
        // a composite type's record (RETURNS SETOF it and RETURNS TABLE of it too)
        Type::Record(_) => t.composite_of().as_deref() == Some(name),
        _ => false,
    };
    let mut out = vec![];
    for r in v.prefix(&format!("{FUNCTIONS}/"))? {
        let f = stored(&r?.1)?;
        let ret = match &f.ret {
            Ret::Value(t) | Ret::SetOf(t) => is(t),
            Ret::Table(c) => c.iter().any(|(_, t)| is(t)),
            _ => false,
        };
        if ret || f.args.iter().any(|(_, t)| is(t)) {
            out.push(f);
        }
    }
    Ok(out)
}

/// The functions that take or give a table's or view's row type: `f(t onek)`, RETURNS onek, RETURNS SETOF onek (as
/// Postgres's pg_depend has them on the relation's type), oldest first.
pub(super) fn using_row_type(v: &View, rel: &str) -> Result<Vec<Arc<Function>>> {
    let mut out = using_type(v, rel)?;
    for r in v.prefix(&format!("{FUNCTIONS}/"))? {
        let f = stored(&r?.1)?;
        if matches!(&f.ret, Ret::RowsOf(t) if t == rel) && !out.iter().any(|g| g.key() == f.key()) {
            out.push(f);
        }
    }
    out.sort_by_key(|f| f.oid);
    Ok(out)
}

/// The rows of the functions taking or returning type `from` (an enum, or a table's or view's row type, RETURNS SETOF
/// a table included), for it renamed `to`: a function is kept under its name and argument types.
pub(super) fn retype(v: &View, from: &str, to: &str) -> Result<Vec<(String, Option<String>)>> {
    let mut writes = vec![];
    for f in using_row_type(v, from)? {
        let mut j = f.to_json();
        let rename = |x: &mut Json| {
            if x.as_str() == Some(from) {
                *x = Json::from(to);
            }
        };
        for a in j["args"].as_array_mut().into_iter().flatten() {
            rename(&mut a[1]);
        }
        let r = &mut j["returns"];
        for k in ["value", "setof", "rows_of"] {
            if let Some(x) = r.get_mut(k) {
                rename(x);
            }
        }
        for c in r.get_mut("table").and_then(Json::as_array_mut).into_iter().flatten() {
            rename(&mut c[1]);
        }
        writes.push((f.key(), None));
        writes.push((Function::from_json(&j)?.key(), Some(j.to_string())));
    }
    Ok(writes)
}

/// The functions named `name` on a branch; for a call's plain name (see `PLAIN`), those of that name in each schema
/// of the running session's search_path in turn (which of them hide which depends on the call, see `shaped`).
pub(super) fn named(v: &View, name: &str) -> Result<Vec<Arc<Function>>> {
    // a statement on the session's temporary tables' database, which has no functions: the session's world's (see
    // `Exec::call_in_world`)
    let home = super::session_temp().filter(|t| std::ptr::eq(v.core, &***t)).and_then(|_| super::catalog::home());
    let world_view;
    let v = match &home {
        Some((core, world, _)) => {
            world_view = core.view_tx(world)?;
            &world_view
        }
        None => v,
    };
    let stored = match name.strip_prefix(PLAIN) {
        Some(plain) => match path_schemas() {
            Some(schemas) => schemas.iter().map(|s| qualify(s, plain)).collect(),
            None => vec![plain.to_string()],
        },
        None => vec![name.to_string()],
    };
    let mut out: Vec<Arc<Function>> = vec![];
    for (i, n) in stored.iter().enumerate() {
        if stored[..i].contains(n) {
            continue;
        }
        for r in v.prefix(&format!("{FUNCTIONS}/{n}("))? {
            out.push(Function::from_raw(&r?.1)?);
        }
    }
    Ok(out)
}

/// Type `t` as Postgres's TypeNameListToString writes a type a statement named: those SQL spells with keywords
/// (`int`, `boolean`, `double precision`) by their names in pg_catalog, others by their names.
fn written_type(t: &Type) -> String {
    let (elem, array) = match t {
        Type::Array(_) => (t.elem().unwrap_or(Type::Text), "[]"),
        t => (*t, ""),
    };
    let internal = match elem {
        Type::Int => "int4",
        Type::BigInt => "int8",
        Type::SmallInt => "int2",
        Type::Float => "float8",
        Type::Float4 => "float4",
        Type::Bool => "bool",
        Type::Numeric => "numeric",
        Type::Timestamp => "timestamp",
        Type::TimestampTz => "timestamptz",
        Type::Time => "time",
        Type::Interval => "interval",
        _ => return format!("{}{array}", elem.name()),
    };
    format!("pg_catalog.{internal}{array}")
}

/// The function or procedure `name` (with these argument types, if given) means, as DROP FUNCTION
/// finds it: None if there is none, 42725 if a name without types means several.
pub(super) fn find_routine(v: &View, name: &str, types: Option<&[Type]>) -> Result<Option<Arc<Function>>> {
    let all = visible(name, named(v, name)?);
    match types {
        Some(ts) => Ok(all.into_iter().find(|f| f.args.iter().map(|a| type_named(&a.1)).eq(ts.iter().map(type_named)))),
        None if all.len() > 1 => err(
            "42725",
            format!(
                "function name \"{}\" is not unique; give its argument types, like {}",
                written(name),
                all[0].sig()
            ),
        ),
        None => Ok(all.into_iter().next()),
    }
}

/// The routine ALTER FUNCTION, PROCEDURE or ROUTINE (`kind`) names, as Postgres's LookupFuncWithArgs finds it: by
/// its argument types where they are written, else the one of its name (42725 for several, 42883 for none), and
/// then of that kind (a FUNCTION isn't a procedure, a PROCEDURE is one; 42809).
pub(super) fn lookup_routine(v: &View, name: &str, types: Option<&[Type]>, kind: RoutineKind) -> Result<Arc<Function>> {
    let all = visible(name, named(v, name)?);
    let noun = kind.noun();
    let shown_types = |ts: &[Type]| ts.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ");
    let f = match types {
        Some(ts) => all.into_iter().find(|f| f.args.iter().map(|a| type_named(&a.1)).eq(ts.iter().map(type_named))),
        None if all.len() > 1 => {
            return Err(SqlError {
                code: "42725",
                message: format!("{noun} name \"{}\" is not unique", written(name)),
                info: Some(Box::new(Violation {
                    hint: Some(format!("Specify the argument list to select the {noun} unambiguously.")),
                    ..Violation::default()
                })),
            });
        }
        None => all.into_iter().next(),
    };
    let Some(f) = f else {
        // (a FUNCTION or ROUTINE that isn't there is named a function)
        let noun = if kind == RoutineKind::Procedure { "procedure" } else { "function" };
        return err(
            NO_FUNCTION,
            match types {
                Some(ts) => format!("{noun} {}({}) does not exist", written(name), shown_types(ts)),
                None => format!("could not find a {noun} named \"{}\"", written(name)),
            },
        );
    };
    let sig = format!("{}({})", written(name), f.args.iter().map(|a| a.1.name()).collect::<Vec<_>>().join(", "));
    match kind {
        RoutineKind::Function if f.procedure => err("42809", format!("{sig} is not a function")),
        RoutineKind::Procedure if !f.procedure => err("42809", format!("{sig} is not a procedure")),
        _ => Ok(f),
    }
}

/// The JSON a routine is stored as, with `edit` made to it (see `Function::to_json`).
pub(super) fn edited(f: &Function, edit: impl FnOnce(&mut Json)) -> String {
    let mut j = f.to_json();
    edit(&mut j);
    j.to_string()
}

/// `named`'s functions less each with the argument types of one in a schema before it on the search_path, which
/// hides it as in Postgres: those a name without argument types (DROP FUNCTION f) means.
fn visible(name: &str, fs: Vec<Arc<Function>>) -> Vec<Arc<Function>> {
    let mut out: Vec<(Arc<Function>, usize)> = vec![];
    for f in fs {
        let (types, at) = (|g: &Function| g.args.iter().map(|a| a.1).collect::<Vec<_>>(), place(name, &f));
        if !out.iter().any(|(g, p)| types(g) == types(&f) && *p < at) {
            out.push((f, at));
        }
    }
    out.into_iter().map(|(f, _)| f).collect()
}

/// Where `f`, one of the functions a call of `name` may mean, is on the running session's search_path: its
/// schema's place (0 for a call of a qualified name).
fn place(name: &str, f: &Function) -> usize {
    let Some(plain) = name.strip_prefix(PLAIN) else { return 0 };
    path_schemas().and_then(|s| s.iter().position(|s| qualify(s, plain) == f.name)).unwrap_or(0)
}

/// The functions of `fs` (all named `name`, in search_path order) a call with `n` arguments named as `names` says
/// ("" for one by position; none: all by position) can take, each with, for each of its parameters, which argument
/// goes there (usize::MAX: it takes its default). As in Postgres (FuncnameGetCandidates), one whose parameters
/// taking the arguments have the types of another's in a schema before it on the search_path is hidden by it, the
/// defaults either takes not counting.
fn shaped(name: &str, fs: &[Arc<Function>], n: usize, names: &[String]) -> Vec<(Arc<Function>, Vec<usize>)> {
    let mut out: Vec<(Arc<Function>, Vec<usize>)> = vec![];
    // (each one's types taking the arguments, and its place)
    let mut kept: Vec<(Vec<Type>, usize)> = vec![];
    for f in fs.iter().filter(|f| n <= f.args.len()) {
        let mut order = vec![usize::MAX; f.args.len()];
        let fits = (0..n).all(|i| {
            let at = match names.get(i).map(String::as_str) {
                None | Some("") => Some(i),
                Some(n) => f.args.iter().position(|(p, _)| p.as_deref() == Some(n)),
            };
            match at {
                Some(at) if order[at] == usize::MAX => {
                    order[at] = i;
                    true
                }
                _ => false,
            }
        });
        // a parameter no argument names takes its default, if it has one
        if !fits || !order.iter().enumerate().all(|(at, i)| *i != usize::MAX || f.default_sql(at).is_some()) {
            continue;
        }
        let mut taking = vec![Type::Text; n];
        for (at, &i) in order.iter().enumerate().filter(|(_, i)| **i != usize::MAX) {
            taking[i] = f.args[at].1;
        }
        let at = place(name, f);
        if !kept.iter().any(|(t, p)| *t == taking && *p < at) {
            out.push((f.clone(), order));
            kept.push((taking, at));
        }
    }
    out
}

fn shown(args: &[Option<Type>]) -> String {
    args.iter().map(|t| t.map_or("unknown".into(), Type::name)).collect::<Vec<_>>().join(", ")
}

/// How well an argument of type `given` (None: unknown, as NULL or a quoted literal is) fits a
/// parameter of type `want`; None if it doesn't.
fn fit(given: Option<Type>, want: Type) -> Option<u32> {
    use Type::*;
    let rank = |t: Type| match t {
        SmallInt => Some(0),
        Int => Some(1),
        BigInt => Some(2),
        Numeric => Some(3),
        // (real: what the integers and numeric convert to implicitly, and itself to double precision)
        Float4 => Some(4),
        Float => Some(5),
        _ => None,
    };
    match (given, want) {
        // (an array, or a quoted literal read as one; an anyarray value, as pg_statistic's stavalues1 is)
        (Some(Array(_) | Poly(1)) | None, Poly(1 | 6)) => return Some(1),
        (Some(_), Poly(1 | 6)) => return None,
        // (a range for anyrange, a multirange for anymultirange: see `poly_fits` for which)
        (Some(Range(_)) | None, Poly(4 | 7)) | (Some(Multirange(_)) | None, Poly(8 | 9)) => return Some(1),
        (Some(_), Poly(4 | 7 | 8 | 9)) => return None,
        (_, Poly(_)) => return Some(1),
        _ => {}
    }
    match given {
        None => Some(if want == Text { 2 } else { 1 }), // unknown prefers text, as in Postgres
        Some(g) if g == want => Some(4),
        // (a reg value is an oid, and a regproc a regprocedure and the other way (a regoper a regoperator), as
        // Postgres converts them implicitly; an oid is any reg type, and an integer an oid too)
        Some(Reg(g)) => matches!(want, Oid).then_some(3).or_else(|| want.reg().filter(|w| g.casts_to(*w)).map(|_| 3)),
        Some(Oid) => want.reg().map(|_| 3),
        Some(SmallInt | Int | BigInt) if want == Oid => Some(3),
        Some(Enum(a)) => matches!(want, Enum(b) if enum_type(a).name == enum_type(b).name).then_some(4),
        Some(g) => match (rank(g), rank(want), g, want) {
            (Some(a), Some(b), ..) if a < b => Some(3),
            (.., Date, Timestamp | TimestampTz) | (.., Timestamp, TimestampTz) => Some(3),
            _ => None,
        },
    }
}

/// The one function a call can mean, `f`, given arguments of these types: a regproc (or an oid) goes only where a
/// regproc or an oid can, as Postgres converts it implicitly to no text or other number (42883 otherwise).
fn takes_procs(name: &str, f: &Function, args: &[Option<Type>]) -> Result<()> {
    let proc = |a: &Option<Type>| matches!(a, Some(Type::Reg(_) | Type::Oid));
    // (nor does an integer or bigint go to a smallint: Postgres has no implicit cast narrowing one)
    let narrow =
        |a: &Option<Type>, p: Type| p == Type::SmallInt && a.is_some_and(|a| a.integral() && a != Type::SmallInt);
    if args.iter().zip(&f.args).all(|(a, p)| !(proc(a) || narrow(a, p.1)) || fit(*a, p.1).is_some()) {
        return Ok(());
    }
    err(NO_FUNCTION, format!("function {}({}) does not exist; there is {}", written(name), shown(args), f.sig()))
}

/// The function of `fs` (all named `name`) that arguments of these types call: of the ones taking that many,
/// the one `narrow` leaves.
/// A call's arguments, some given by name, in the order of the parameters of the function they choose, a parameter
/// left out taking its default: what a call that names none would write.
pub(super) fn positional(name: &str, args: Vec<Expr>, names: &[String]) -> Result<Vec<Expr>> {
    // (a constant's type, or a cast's, is known here: a quoted literal's, NULL's and anything else's aren't)
    let tys: Vec<Option<Type>> = args
        .iter()
        .map(|a| match a {
            Expr::Lit(Value::Null | Value::Text(_)) => None,
            Expr::Lit(v) => value_type(v),
            Expr::Cast(_, t) if !matches!(t, Type::Enum(_)) => Some(*t),
            _ => None,
        })
        .collect();
    let found =
        with_path_view(|v| -> Result<(Arc<Function>, Vec<usize>)> { choose(name, &named(v, name)?, &tys, names) });
    let Some(found) = found else {
        return err(UNSUPPORTED, "arguments given by name need a database to look the function up in");
    };
    let (f, order) = found?;
    order
        .into_iter()
        .enumerate()
        .map(|(at, i)| if i == usize::MAX { default_arg(&f, at) } else { Ok(args[i].clone()) })
        .collect()
}

/// The parameters VARIADIC function `f` takes a call's `n` arguments as: those before its VARIADIC one, then for each
/// argument from there on its array's element type (anyarray's anyelement, anycompatiblearray's anycompatible).
fn expanded(f: &Function, n: usize) -> Vec<Option<Type>> {
    let last = f.args.len().saturating_sub(1);
    let elem = match f.args.get(last).map(|a| a.1) {
        Some(Type::Array(e)) => Type::from_oid(e),
        Some(Type::Poly(1)) => Some(Type::Poly(0)),
        Some(Type::Poly(6)) => Some(Type::Poly(5)),
        _ => None,
    };
    (0..n).map(|i| if i < last { Some(f.args[i].1) } else { elem }).collect()
}

/// The function of `fs` (all named `name`) a call by position with arguments of these types means, and whether it is
/// a VARIADIC one taking the arguments from its last parameter on as that parameter's array. As in Postgres a
/// VARIADIC function is matched only so (a call's `VARIADIC ARRAY[...]` has been written out as its elements), after
/// the others: those `pick` chooses among.
fn pick_call(name: &str, fs: &[Arc<Function>], tys: &[Option<Type>]) -> Result<(Arc<Function>, bool)> {
    let plain: Vec<Arc<Function>> = fs.iter().filter(|f| !f.variadic).cloned().collect();
    let e = match pick(name, &plain, tys) {
        Ok(f) => return Ok((f, false)),
        Err(e) if e.code == "42725" => return Err(e),
        Err(e) => e,
    };
    let cands: Vec<Candidate<&Arc<Function>>> = fs
        .iter()
        // (at least one argument for its VARIADIC parameter, unless that parameter has a default)
        .filter(|f| f.variadic && !f.args.is_empty() && (tys.len() >= f.args.len() || f.takes(tys.len())))
        .map(|f| (f, expanded(f, tys.len())))
        .collect();
    match &narrow(tys, cands)[..] {
        // (given none from its last parameter on, it takes that one's default if it has one)
        [f] => {
            let n = tys.len();
            let packed = n >= f.args.len() || (n + 1 == f.args.len() && f.default_sql(n).is_none());
            Ok((Arc::clone(*f), packed))
        }
        [] => Err(e),
        _ => err("42725", format!("function {}({}) is not unique; add explicit type casts", written(name), shown(tys))),
    }
}

/// The type of the array a VARIADIC function `f` is given for arguments of these types from its last parameter on: its
/// parameter's, or for a polymorphic one the array of their common type (None: all of unknown type).
fn packed_type(f: &Function, rest: &[Option<Type>]) -> Option<Type> {
    match f.args.last()?.1 {
        t @ Type::Array(_) => Some(t),
        _ => {
            let known: Vec<Type> = rest.iter().flatten().copied().collect();
            common_type(&known).and_then(array_type)
        }
    }
}

/// A VARIADIC function's arguments from its last parameter on, made that parameter's array.
fn pack_variadic(f: &Function, args: &mut Vec<Expr>, tys: &[Option<Type>]) {
    let at = f.args.len().saturating_sub(1).min(args.len());
    let rest = args.split_off(at);
    let array = Expr::Func(Func::Scalar(funcs::Scalar::ArrayOf), rest);
    args.push(match packed_type(f, &tys[at.min(tys.len())..]) {
        Some(t) => Expr::Cast(Box::new(array), t),
        None => array,
    });
}

fn pick(name: &str, fs: &[Arc<Function>], args: &[Option<Type>]) -> Result<Arc<Function>> {
    choose(name, fs, args, &[]).map(|(f, _)| f)
}

/// The function of `fs` (all named `name`, in search_path order) that a call with arguments of these types, named
/// as `names` says ("" for one by position; none: all by position), means: of the ones `shaped` leaves, the one
/// `narrow` leaves (the only one, whatever the types). With it, for each of its parameters, which argument goes
/// there (usize::MAX: its default).
fn choose(
    name: &str,
    fs: &[Arc<Function>],
    args: &[Option<Type>],
    names: &[String],
) -> Result<(Arc<Function>, Vec<usize>)> {
    let same = shaped(name, fs, args.len(), names);
    if let [(f, order)] = &same[..] {
        if names.is_empty() {
            takes_procs(name, f, args)?;
        }
        // (a polymorphic one whose arguments' types don't agree, as check_generic_type_consistency sees them, doesn't
        // take them)
        let mut taking = vec![None; args.len()];
        for (at, &i) in order.iter().enumerate().filter(|(_, i)| **i != usize::MAX) {
            taking[i] = Some(f.args[at].1);
        }
        // (nor, called with names, one whose argument of a plain type doesn't convert implicitly to its parameter's:
        // `b => date` for a numeric b)
        let plain = |t: Type| {
            t.integral()
                || matches!(
                    t,
                    Type::Numeric
                        | Type::Float
                        | Type::Float4
                        | Type::Text
                        | Type::Bool
                        | Type::Date
                        | Type::Timestamp
                        | Type::TimestampTz
                        | Type::Interval
                        | Type::Time
                )
        };
        let unfit = !names.is_empty()
            && args.iter().zip(&taking).any(|(a, w)| match (a, w) {
                (Some(g), Some(w)) => *g != *w && plain(*g) && plain(*w) && fit(Some(*g), *w).is_none(),
                _ => false,
            });
        if unfit || !poly_fits(&taking, args) {
            let given = names.iter().map(String::as_str).chain(std::iter::repeat("")).zip(args).map(|(n, a)| {
                let t = a.map_or("unknown".into(), Type::name);
                if n.is_empty() { t.to_string() } else { format!("{n} => {t}") }
            });
            return err(
                NO_FUNCTION,
                format!(
                    "function {}({}) does not exist; there is {}",
                    written(name),
                    given.collect::<Vec<_>>().join(", "),
                    f.sig()
                ),
            );
        }
        return Ok((f.clone(), order.clone()));
    }
    // (a parameter of a table's or view's row type, `f(t pair)`, is compared as that row's record, as a whole-row
    // argument has it, so `f(p)` picks f(pair) beside f(integer); only for choosing: the function keeps its types)
    let as_row = |t: Type| match t {
        Type::Enum(i) if enum_type(i).labels.is_none() => with_path_view(|v| match v.get(&type_key(&enum_name(i))) {
            Ok(None) => row_type_named(v, t).ok().flatten(),
            _ => None,
        })
        .flatten()
        .unwrap_or(t),
        t => t,
    };
    let cands = same
        .iter()
        .map(|(f, order)| {
            let mut taking = vec![None; args.len()];
            for (at, &i) in order.iter().enumerate().filter(|(_, i)| **i != usize::MAX) {
                taking[i] = Some(as_row(f.args[at].1));
            }
            ((f, order), taking)
        })
        .collect();
    let top = narrow(args, cands);
    if let [(f, order)] = top[..] {
        return Ok((f.clone(), order.clone()));
    }
    let given = || {
        let given = names.iter().zip(args).map(|(n, a)| {
            let t = a.map_or("unknown".into(), Type::name);
            if n.is_empty() { t.to_string() } else { format!("{n} => {t}") }
        });
        given.collect::<Vec<_>>().join(", ")
    };
    match top[..] {
        [] if !names.is_empty() => err(
            NO_FUNCTION,
            format!(
                "function {}({}) does not exist; there is {}",
                written(name),
                given(),
                fs.iter().map(|f| f.sig()).collect::<Vec<_>>().join(", ")
            ),
        ),
        [] if fs.is_empty() => err(
            NO_FUNCTION,
            format!(
                "function {}({}) does not exist; there are the functions made with CREATE FUNCTION, and count, sum, avg, min, max, coalesce, lower, upper, length, abs, round, now, date_trunc, extract, date_part, gen_random_uuid, {}",
                written(name),
                shown(args),
                funcs::NAMES
            ),
        ),
        [] => err(
            NO_FUNCTION,
            format!(
                "function {}({}) does not exist; there is {}",
                written(name),
                shown(args),
                fs.iter().map(|f| f.sig()).collect::<Vec<_>>().join(", ")
            ),
        ),
        _ if !names.is_empty() => {
            err("42725", format!("function {}({}) is not unique; add explicit type casts", written(name), given()))
        }
        _ => {
            err("42725", format!("function {}({}) is not unique; add explicit type casts", written(name), shown(args)))
        }
    }
}

/// A built-in's parameter (see `builtin_params`): of a type Chronos has, taking any type (`anyelement`), of a
/// type no value of Chronos's has (`anyrange`, `internal`), or of one Chronos can't weigh (`anyarray`, `oid`).
#[derive(Clone, Copy, PartialEq)]
enum Param {
    Is(Type),
    Any,
    Never,
    Unsure,
}

/// The parameters of a built-in, from its argument types as regprocedure shows them, `(text,integer)`.
fn builtin_params(sig: &str) -> Vec<Param> {
    let sig = sig.trim_start_matches('(').trim_end_matches(')');
    if sig.is_empty() {
        return vec![];
    }
    sig.split(',')
        .map(|t| match t {
            "integer" => Param::Is(Type::Int),
            "smallint" => Param::Is(Type::SmallInt),
            "bigint" => Param::Is(Type::BigInt),
            "real" => Param::Is(Type::Float4),
            "double precision" => Param::Is(Type::Float),
            "numeric" => Param::Is(Type::Numeric),
            "text" | "character varying" | "character" | "name" => Param::Is(Type::Text),
            "boolean" => Param::Is(Type::Bool),
            "json" => Param::Is(Type::JsonText),
            "jsonb" => Param::Is(Type::Json),
            "date" => Param::Is(Type::Date),
            "timestamp without time zone" => Param::Is(Type::Timestamp),
            "timestamp with time zone" => Param::Is(Type::TimestampTz),
            "interval" => Param::Is(Type::Interval),
            "time without time zone" => Param::Is(Type::Time),
            "bytea" => Param::Is(Type::Bytea),
            "uuid" => Param::Is(Type::Uuid),
            "inet" => Param::Is(Type::Inet),
            "cidr" => Param::Is(Type::Cidr),
            "macaddr" => Param::Is(Type::MacAddr),
            "macaddr8" => Param::Is(Type::MacAddr8),
            "tsvector" => Param::Is(Type::TsVector),
            "tsquery" => Param::Is(Type::TsQuery),
            "hstore" => Param::Is(Type::Hstore),
            "ltree" => Param::Is(Type::Ltree),
            "lquery" => Param::Is(Type::Lquery),
            "\"any\"" | "anyelement" | "anynonarray" | "anycompatible" | "anycompatiblenonarray" => Param::Any,
            t if t.ends_with("[]") || t.starts_with("anyarray") || t.starts_with("anycompatiblearray") => Param::Unsure,
            t if t == "oid" || super::Reg::named(t).is_some() => Param::Unsure,
            _ => Param::Never,
        })
        .collect()
}

/// A function `narrow` chooses among, with its parameters' types (None: of any type).
type Candidate<T> = (T, Vec<Option<Type>>);

/// A type's category and whether it is the category's preferred type, as Postgres's pg_type has them (None: a
/// parameter of any type, a pseudo-type).
fn category(t: Option<Type>) -> (char, bool) {
    use Type::*;
    match t {
        Some(SmallInt | Int | BigInt | Numeric | Float4 | Reg(_)) => ('N', false),
        Some(Oid | Float) => ('N', true),
        Some(Text) => ('S', true),
        Some(Bool) => ('B', true),
        Some(Date | Timestamp | Time) => ('D', false),
        Some(TimestampTz) => ('D', true),
        Some(Interval) => ('T', true),
        Some(Inet) => ('I', true),
        Some(Cidr) => ('I', false),
        Some(Array(_)) => ('A', false),
        Some(Enum(_)) => ('E', false),
        Some(Record(_)) => ('C', false),
        Some(Void) | None => ('P', false),
        Some(_) => ('U', false),
    }
}

/// Which of these candidates, each with its parameters (None: of any type), Postgres narrows a call with
/// arguments of these types to (func_select_candidate): those each argument fits (as it is or converted
/// implicitly), then the ones with the most arguments of exactly their types, then the most converted to their
/// category's preferred type (double precision and oid for numbers, timestamptz for dates and times). Then where
/// arguments are unknown (NULL or a quoted literal): each such place takes a category, text's if any candidate takes
/// text there, else the one all candidates agree on, and its preferred type if any takes that; the candidates taking
/// those at every such place are kept, if any is (all of them otherwise). Last, when the known arguments are all of
/// one type, the one candidate that fits the unknown ones taken as of that type, if there is one.
fn narrow<T>(args: &[Option<Type>], cands: Vec<Candidate<T>>) -> Vec<T> {
    let fits = |ps: &[Option<Type>]| {
        args.iter().zip(ps).map(|(a, p)| (*a, p.map_or(Some(3), |p| fit(*a, p)), *p)).collect::<Vec<_>>()
    };
    let mut top: Vec<Candidate<T>> =
        cands.into_iter().filter(|c| fits(&c.1).iter().all(|x| x.1.is_some()) && poly_fits(&c.1, args)).collect();
    let keep_most = |top: &mut Vec<Candidate<T>>, n: &dyn Fn(&[Option<Type>]) -> usize| {
        let most = top.iter().map(|c| n(&c.1)).max();
        top.retain(|c| Some(n(&c.1)) == most);
    };
    keep_most(&mut top, &|ps| fits(ps).iter().filter(|x| x.1 == Some(4)).count());
    keep_most(&mut top, &|ps| {
        let preferred = |p: Option<Type>| matches!(p, Some(Type::Float | Type::TimestampTz | Type::Oid));
        fits(ps).iter().filter(|x| x.0.is_some() && x.1 != Some(4) && preferred(x.2)).count()
    });
    let unknown: Vec<usize> = (0..args.len()).filter(|&i| args[i].is_none()).collect();
    if top.len() < 2 || unknown.is_empty() {
        return top.into_iter().map(|c| c.0).collect();
    }
    // each unknown place's category and whether a candidate takes its preferred type there
    let mut slots = vec![];
    for &i in &unknown {
        let (mut slot, mut conflict): (Option<(char, bool)>, bool) = (None, false);
        for c in &top {
            let (cat, pref) = category(c.1[i]);
            slot = match slot {
                None => Some((cat, pref)),
                Some((s, p)) if s == cat => Some((s, p || pref)),
                Some(_) if cat == 'S' => Some((cat, pref)),
                Some(s) => {
                    conflict = true;
                    Some(s)
                }
            };
        }
        match slot {
            Some(s) if !conflict || s.0 == 'S' => slots.push((i, s)),
            _ => {
                slots.clear();
                break;
            }
        }
    }
    if !slots.is_empty() {
        let keep = |c: &Candidate<T>| {
            slots.iter().all(|&(i, (cat, pref))| {
                let (c, p) = category(c.1[i]);
                c == cat && (p || !pref)
            })
        };
        if top.iter().any(keep) {
            top.retain(keep);
        }
        if top.len() == 1 {
            return top.into_iter().map(|c| c.0).collect();
        }
    }
    let known: Vec<Type> = args.iter().flatten().copied().collect();
    if let Some(&k) = known.first().filter(|&&k| known.len() < args.len() && known.iter().all(|t| *t == k)) {
        let fitting = |c: &Candidate<T>| c.1.iter().all(|p| p.is_none_or(|p| fit(Some(k), p).is_some()));
        if top.iter().filter(|c| fitting(c)).count() == 1 {
            top.retain(fitting);
        }
    }
    top.into_iter().map(|c| c.0).collect()
}

/// The built-ins whose last parameter is VARIADIC, by oid, with the type each argument it takes has: `concat` takes
/// any number of arguments of any type, `json_extract_path` texts after its json.
const VARIADIC: &[(i64, Param)] = &[
    (438, Param::Any),
    (440, Param::Any),
    (3058, Param::Any),
    (3059, Param::Any),
    (3198, Param::Any),
    (3200, Param::Any),
    (3271, Param::Any),
    (3273, Param::Any),
    (3539, Param::Any),
    (3217, Param::Is(Type::Text)),
    (3343, Param::Is(Type::Text)),
    (3940, Param::Is(Type::Text)),
    (3951, Param::Is(Type::Text)),
    (3953, Param::Is(Type::Text)),
];

/// Whether a call of a built-in's name that functions made with CREATE FUNCTION on the search_path share (`name`,
/// see `Call::builtin`) means the built-in, given arguments of these types (None: unknown): unless `narrow` leaves
/// one of those functions alone among them and pg_catalog's of that name (an earlier schema's hiding a later one's
/// of the same argument types, pg_catalog's place on the path counting), a VARIADIC one taking as many arguments as
/// the call gives. Where it leaves one of them with another, the call is not unique (42725), as in Postgres. A
/// built-in Chronos can't weigh keeps it.
fn means_builtin(v: &View, name: &str, tys: &[Option<Type>]) -> Result<bool> {
    let w = written(name);
    let mut builtins = vec![];
    for oid in catalog_schema::builtin_procs(w) {
        let Some((_, sig)) = catalog_schema::builtin_proc(oid) else { continue };
        let mut ps = builtin_params(sig);
        if let Some((_, each)) = VARIADIC.iter().find(|(o, _)| *o == oid)
            && tys.len() >= ps.len()
        {
            ps.truncate(ps.len() - 1);
            ps.resize(tys.len(), *each);
        }
        if ps.len() != tys.len() || ps.contains(&Param::Never) {
            continue;
        }
        if ps.contains(&Param::Unsure) {
            return Ok(true);
        }
        builtins.push(ps.into_iter().map(|p| if let Param::Is(t) = p { Some(t) } else { None }).collect::<Vec<_>>());
    }
    // each function's place on the search_path, and pg_catalog's
    let path = func_path();
    let catalog = path.iter().position(|s| s == "pg_catalog").unwrap_or(0);
    let place = |f: &Function| path.iter().position(|s| s != "pg_catalog" && qualify(s, w) == f.name);
    // (a VARIADIC one taking the call's arguments from its last parameter on as its array's elements)
    let mine: Vec<(Arc<Function>, Vec<Option<Type>>)> = visible(name, named(v, name)?)
        .into_iter()
        .filter_map(|f| match f.variadic {
            false if f.args.len() == tys.len() => {
                let t = f.args.iter().map(|a| Some(a.1)).collect();
                Some((f, t))
            }
            true if !f.args.is_empty() && tys.len() + 1 >= f.args.len() => {
                let t = expanded(&f, tys.len());
                Some((f, t))
            }
            _ => None,
        })
        .collect();
    let before = |f: &Function| place(f).is_some_and(|i| i < catalog);
    builtins.retain(|b| !mine.iter().any(|(f, t)| before(f) && t == b));
    let mut cands: Vec<Candidate<Option<&Arc<Function>>>> =
        mine.iter().filter(|(f, t)| before(f) || !builtins.contains(t)).map(|(f, t)| (Some(f), t.clone())).collect();
    cands.extend(builtins.into_iter().map(|b| (None, b)));
    match narrow(tys, cands)[..] {
        [Some(_)] => Ok(false),
        ref top if top.iter().any(Option::is_some) => {
            err("42725", format!("function {w}({}) is not unique; add explicit type casts", shown(tys)))
        }
        _ => Ok(true),
    }
}

/// An argument `a` of a call not bound, its type for choosing the function the call means (None: unknown, as NULL, a
/// quoted literal or a parameter declared none is), or None where it can't be told: reading columns `cols`, and
/// parameters of the declared types `params` (none: not running, where a parameter's type isn't known). A call in it
/// of a function made here has the type the one its arguments choose returns, and one of a built-in's name that
/// such functions share (see `Call::builtin`) that of what they choose; a subquery's type isn't known unbound.
pub(super) fn unbound_type(a: &Expr, cols: &[Col], params: Option<&[Option<Type>]>) -> Option<Option<Type>> {
    match a {
        Expr::Lit(Value::Null | Value::Text(_)) => return Some(None),
        Expr::Param(n) => return Some(params?.get(n - 1).copied().flatten()),
        _ => {}
    }
    // (each such call: a NULL of the type it gives)
    let failed = std::cell::Cell::new(false);
    let a = rewrite(a, &|x| match x {
        Expr::Call(c) if c.def.is_none() => match unbound_call_type(c, cols, params) {
            Some(t) => Some(Expr::Cast(Box::new(Expr::Lit(Value::Null)), t)),
            None => {
                failed.set(true);
                Some(x.clone())
            }
        },
        _ => None,
    });
    let mut unsure = failed.get();
    walk(&a, &mut |x| {
        unsure |= match x {
            Expr::Param(_) => params.is_none(),
            Expr::WinRef(..) | Expr::Window(_) | Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) => true,
            _ => false,
        }
    });
    match unsure {
        true => None,
        false => ty_in(&a, cols, params.unwrap_or_default(), &[]).ok().map(Some),
    }
}

/// The type a call not bound of a function made here, or of a built-in's name such functions share, gives: of the
/// one its arguments' types choose (see `unbound_type`); None where that can't be told.
fn unbound_call_type(c: &Call, cols: &[Col], params: Option<&[Option<Type>]>) -> Option<Type> {
    let given = c.args.iter().map(|a| unbound_type(a, cols, params)).collect::<Option<Vec<_>>>()?;
    let found = with_path_view(|v| -> Result<Option<Type>> {
        if c.builtin && means_builtin(v, &c.name, &given)? {
            return Ok(unbound_type(&builtin_of_call(c)?, cols, params).flatten());
        }
        let fs = named(v, &c.name)?;
        let f = match c.names.is_empty() {
            true => {
                let (f, packed) = pick_call(&c.name, &fs, &given)?;
                let mut tys = given.clone();
                if packed {
                    let rest = tys.split_off(f.args.len().saturating_sub(1).min(tys.len()));
                    tys.push(packed_type(&f, &rest));
                }
                instance(&f, &tys, false)?
            }
            false => choose(&c.name, &fs, &given, &c.names)?.0,
        };
        Ok(Some(f.value_type()))
    });
    found?.ok()?
}

/// A call of a built-in's name that functions made here share (`c`, see `Call::builtin`), made what its arguments'
/// types choose, as the running session's search_path finds them: the built-in, or a call of those functions. An
/// argument's type comes from `cols`, and a parameter's from `params`, the declared types as the statement runs (one
/// declared none is unknown). Without `params` (not running) a parameter's type isn't known: the call is left as it
/// is (None), as it is where an argument's type can't be told (see `unbound_type`), or there's no session.
pub(super) fn settled(c: &Call, cols: &[Col], params: Option<&[Option<Type>]>) -> Option<Result<Expr>> {
    let tys = c.args.iter().map(|a| unbound_type(a, cols, params)).collect::<Option<Vec<_>>>()?;
    settled_as(c, &tys)
}

/// `settled` with the arguments of these types.
pub(super) fn settled_as(c: &Call, tys: &[Option<Type>]) -> Option<Result<Expr>> {
    let builtin = with_path_view(|v| means_builtin(v, &c.name, tys))?;
    Some(builtin.and_then(|b| match b {
        true => builtin_of_call(c),
        false => Ok(Expr::Call(Box::new(Call { builtin: false, ..c.clone() }))),
    }))
}

impl Binder<'_> {
    /// A call's argument `a`, bound, its type for choosing the function the call means: Some(None) where it is
    /// unknown (NULL, a quoted literal, a parameter of no declared type), None where it can't be told here.
    fn arg_type(&self, a: &Expr) -> Option<Option<Type>> {
        match a {
            Expr::Lit(Value::Null | Value::Text(_)) => Some(None),
            Expr::Param(n) => Some(self.params.get(n - 1).copied().flatten()),
            a => {
                // (a window function's result: typed by its call, collected apart)
                let wins = self.wins.as_deref().unwrap_or_default();
                let a = rewrite(a, &|x| match x {
                    Expr::WinRef(k, _) => wins.get(*k).map(|w| Expr::Window(Box::new(w.clone()))),
                    _ => None,
                });
                let mut unsure = false;
                walk(&a, &mut |x| {
                    unsure |= match x {
                        Expr::WinRef(..) => true,
                        Expr::Call(c) => c.def.is_none(),
                        _ => false,
                    }
                });
                match unsure {
                    true => None,
                    false => ty_in(&a, self.cols, self.params, &self.aggs).ok().map(Some),
                }
            }
        }
    }

    /// `means_builtin` for a call `c` with these arguments, bound: the built-in too where an argument's type can't
    /// be told. A binder with no branch (a CHECK, ON CONFLICT DO UPDATE's SET) finds the functions as the running
    /// session's search_path does; the built-in without a session.
    pub(super) fn means_builtin(&self, c: &Call, args: &[Expr]) -> Result<bool> {
        let Some(tys) = args.iter().map(|a| self.arg_type(a)).collect::<Option<Vec<_>>>() else { return Ok(true) };
        match self.v {
            Some(v) => means_builtin(v, &c.name, &tys),
            None => with_path_view(|v| means_builtin(v, &c.name, &tys)).unwrap_or(Ok(true)),
        }
    }

    /// For a call a user's comparison operator made, with these arguments bound: the built-in comparison it is
    /// instead, when no definition takes the arguments' types (see `userdefs::builtin_comparison`).
    pub(super) fn builtin_comparison(&self, c: &Call, args: &[Expr]) -> Option<Op> {
        let tys: Vec<Option<Type>> = args.iter().map(|a| self.arg_type(a).flatten()).collect();
        userdefs::builtin_comparison(&c.name, &tys)
    }

    /// A call, found among the functions on the branch by its arguments' types, or the body of a
    /// function simple enough to stand in for it (see `Function::inline`).
    pub(super) fn call(&mut self, c: &Call, mut args: Vec<Expr>, in_agg: bool) -> Result<Expr> {
        let tys = |args: &[Expr]| -> Vec<Option<Type>> { args.iter().map(|a| self.arg_type(a).flatten()).collect() };
        // an operator symbol several definitions share: the one whose types the arguments have
        let renamed;
        let c = match userdefs::resolve_op_call(&c.name, &tys(&args)) {
            Some(name) => {
                renamed = Call { name, ..c.clone() };
                &renamed
            }
            None => c,
        };
        // (the types the call gives, as its errors name them)
        let given_types = tys(&args);
        let def = match (&c.def, self.v) {
            // (a VARIADIC one takes the arguments from its last parameter on as its array's elements)
            (Some(d), _) if d.variadic && c.names.is_empty() && d.name != "inline_code_block" => {
                let given = tys(&args);
                let (f, packed) = pick_call(&c.name, std::slice::from_ref(d), &given)?;
                if packed {
                    pack_variadic(&f, &mut args, &given);
                }
                Some(f)
            }
            (Some(d), _) => {
                let given = tys(&args);
                takes_procs(&c.name, d, &given)?;
                // (a polymorphic one whose arguments' types don't agree doesn't take them)
                let params: Vec<Option<Type>> = d.args.iter().take(given.len()).map(|a| Some(a.1)).collect();
                if !poly_fits(&params, &given[..params.len()]) {
                    return err(
                        NO_FUNCTION,
                        format!(
                            "function {}({}) does not exist; there is {}",
                            written(&c.name),
                            shown(&given),
                            d.sig()
                        ),
                    );
                }
                Some(d.clone())
            }
            // f(b => 2, a => 1): the one function whose parameters have these names, its arguments in its order
            (None, Some(v)) if !c.names.is_empty() => {
                let (f, order) = choose(&c.name, &named(v, &c.name)?, &tys(&args), &c.names)?;
                let mut given = vec![];
                for (at, i) in order.into_iter().enumerate() {
                    given.push(if i == usize::MAX { default_arg(&f, at)? } else { args[i].clone() });
                }
                args = given;
                takes_procs(&c.name, &f, &tys(&args))?;
                Some(f)
            }
            (None, Some(v)) => {
                let fs = named(v, &c.name)?;
                // (a table named where a value is wanted, `f(t)`, is its row: of the record its columns make, as
                // Postgres's whole-row Var has the table's row type, so it chooses f(t onek) beside f(integer))
                let given: Vec<Option<Type>> = (tys(&args).into_iter().zip(&c.args))
                    .map(|(g, a)| match self.table_row(a) {
                        Some(of) => Some(Type::record(
                            of.iter().map(|&i| (self.cols[i].name.clone(), self.cols[i].ty)).collect(),
                        )),
                        None => g,
                    })
                    .collect();
                let (f, packed) = match pick_call(&c.name, &fs, &given) {
                    Ok(found) => found,
                    // CALL of what no procedure is: a built-in function (or aggregate) is one that isn't a procedure,
                    // else there is no such procedure
                    Err(e) if c.via_call && e.code == NO_FUNCTION && fs.iter().all(|f| !f.procedure) => {
                        let sig = format!("{}({})", written(&c.name), shown(&given));
                        let builtin = !catalog_schema::builtin_procs(rel_part(written(&c.name))).is_empty();
                        return Err(match builtin || !fs.is_empty() {
                            true => SqlError {
                                code: "42809",
                                message: format!("{sig} is not a procedure"),
                                info: Some(Box::new(Violation {
                                    hint: Some("To call a function, use SELECT.".into()),
                                    ..Violation::default()
                                })),
                            },
                            false => SqlError {
                                code: NO_FUNCTION,
                                message: format!("procedure {sig} does not exist"),
                                info: Some(Box::new(Violation {
                                    hint: Some(
                                        "No procedure matches the given name and argument types. You might need to \
                                         add explicit type casts."
                                            .into(),
                                    ),
                                    ..Violation::default()
                                })),
                            },
                        });
                    }
                    // a call that names a built-in type, of one quoted literal (`tsquery('a & b')`), is that type's
                    // cast, as Postgres takes it when no function of the name matches the call, other ones of the
                    // name notwithstanding (func_get_detail's FUNCDETAIL_COERCION)
                    // (a name of another schema isn't a built-in type's: `app.int4('1')` is no cast)
                    Err(e)
                        if e.code == NO_FUNCTION
                            && matches!(&args[..], [Expr::Lit(Value::Text(_))])
                            && (!written(&c.name).contains('.') || schema_part(written(&c.name)) == "pg_catalog")
                            && super::builtin_type_named(rel_part(written(&c.name))).is_some() =>
                    {
                        let to = super::builtin_type_named(rel_part(written(&c.name))).expect("checked");
                        return self.go(&Expr::Cast(Box::new(args[0].clone()), to), in_agg);
                    }
                    // one of Postgres's own functions called by its name, which is its C symbol too
                    // (`int4smaller(a, b)`): what it does, as SQL (see `userdefs::internal_sql`)
                    Err(e) if fs.is_empty() && e.code == NO_FUNCTION => {
                        let name = rel_part(written(&c.name));
                        let takes = catalog_schema::internal_function(name)
                            .is_some_and(|(proname, params, ..)| proname == name && params.len() == args.len());
                        match userdefs::internal_sql(name).filter(|s| takes && !s.starts_with("pg_catalog.")) {
                            Some(sql) => {
                                let body = check_expr(&sql)?;
                                let e = rewrite(&body, &|x| match x {
                                    Expr::Param(n) => args.get(n.wrapping_sub(1)).cloned(),
                                    _ => None,
                                });
                                return self.go(&e, in_agg);
                            }
                            None => return Err(e),
                        }
                    }
                    Err(e) => return Err(e),
                };
                // a VARIADIC function: the arguments from its last parameter on, as that parameter's array
                if packed {
                    pack_variadic(&f, &mut args, &given);
                }
                Some(f)
            }
            (None, None) => None,
        };
        // a polymorphic function, made for the types this call has (each told, unknown being a literal's, or not)
        let def = match def {
            Some(d) => {
                let sure = args.iter().all(|a| self.arg_type(a).is_some());
                Some(instance(&d, &tys(&args), sure)?)
            }
            None => None,
        };
        // the parameters a call leaves out take their defaults
        if let Some(d) = &def {
            for at in args.len()..d.args.len() {
                let e = default_arg(d, at)?;
                args.push(self.go(&e, in_agg)?);
            }
        }
        if let Some(d) = &def
            && d.procedure != c.via_call
            && d.name != "inline_code_block"
        {
            let (what, hint) = if c.via_call {
                ("is not a procedure", "To call a function, use SELECT.")
            } else {
                ("is a procedure", "To call a procedure, use CALL.")
            };
            return Err(SqlError {
                code: "42809",
                message: format!("{}({}) {what}", written(&c.name), shown(&given_types)),
                info: Some(Box::new(Violation { hint: Some(hint.into()), ..Violation::default() })),
            });
        }
        // EXECUTE on it (an inlined function is checked here, as Postgres checks before inlining)
        if let (Some(d), Some(v), Some(who)) = (&def, self.v, roles::checked()) {
            acl::may_execute(v, &who, d)?;
        }
        if let Some(e) = def.as_ref().and_then(|d| d.inline(&args)) {
            return self.go(&e, in_agg);
        }
        // an enum it returns compares and sorts by its labels
        let ret = def.as_ref().filter(|d| !d.ret.sets()).map(|d| d.value_type());
        let call = Expr::Call(Box::new(Call {
            name: c.name.clone(),
            args,
            names: vec![],
            def,
            via_call: false,
            builtin: false,
        }));
        Ok(match (ret, self.v) {
            (Some(t @ Type::Enum(_)), Some(v)) => Expr::Cast(Box::new(call), resolve_type(v, t)?),
            _ => call,
        })
    }
}

/// Parameter `at` of `f`, its default: the expression, in the parameter's type.
fn default_arg(f: &Function, at: usize) -> Result<Expr> {
    let Some(text) = f.default_sql(at) else {
        return err(NO_FUNCTION, format!("function {} needs {} arguments", f.sig(), f.required()));
    };
    let e = check_expr(text)?;
    Ok(match f.args[at].1 {
        Type::Enum(_) => e,
        t => Expr::Cast(Box::new(e), t),
    })
}

/// `s` with each call that only one function of its name and number of arguments could take
/// tied to that function, so its result's type is known before it runs.
pub(super) fn resolve_calls(s: &Stmt, v: &View) -> Result<Stmt> {
    let mut s = s.clone();
    let mut found: HashMap<String, Vec<Arc<Function>>> = HashMap::new();
    visit(
        &mut s,
        &mut |e| {
            if let Expr::Call(c) = e
                && c.def.is_none()
                && c.names.is_empty()
                && !c.builtin
            {
                if !found.contains_key(&c.name) {
                    found.insert(c.name.clone(), named(v, &c.name)?);
                }
                if let [(f, _)] = &shaped(&c.name, &found[&c.name], c.args.len(), &[])[..] {
                    c.def = Some(f.clone());
                }
            }
            Ok(())
        },
        &mut |q| select_list_srfs(v, q),
    )?;
    Ok(s)
}

/// A set-returning function made with CREATE FUNCTION in a select list (`SELECT f(x) FROM t`), as Postgres runs it
/// since version 10: for each row, a row for each of the function's (none: the row is gone). Made a LATERAL source of
/// the call, its column (a record of its columns, for several) where the call was, named as the function. A call
/// nested in another's arguments (`g(f(x))`) is made one first, and then the outer, each a level as Postgres's
/// ProjectSet nodes run them. First, attribute notation is made the calls it means (see `attribute_calls`).
fn select_list_srfs(v: &View, q: &mut Select) {
    attribute_calls(v, q);
    for level in 0..8 {
        let at = if level == 0 { SRF_SOURCE.to_string() } else { format!("{SRF_SOURCE}{level}") };
        if !srf_round(v, q, &at) {
            break;
        }
    }
}

/// The name of the LATERAL source a select list's set-returning call is made (with its level after it, past the first):
/// a name SQL can't write (\u{1} starts it, as `GROUP`'s), so no alias of the user's is taken for one
pub(super) const SRF_SOURCE: &str = "\u{1}usrf";

/// Whether a function made with CREATE FUNCTION named `name` takes one argument (so `name(x)` calls it, and is not
/// the field `name` of x)
fn takes_one(v: &View, name: &str) -> bool {
    named(v, name).is_ok_and(|fs| fs.iter().any(|f| f.takes(1)))
}

/// Attribute notation, as Postgres's ParseFuncOrColumn reads it: `t.f`, where FROM item `t` is a table or view
/// with no column `f` but a function `f` takes one argument, is `f(t)` (the call with t's whole row); and so is
/// `(x).f` where x is such a call whose rows have no field `f`. A column of the name is always the column.
fn attribute_calls(v: &View, q: &mut Select) {
    // the FROM items that are tables or views here, by the name they go by, with their columns
    let mut rels: Vec<(String, Vec<String>)> = vec![];
    for src in q.from.iter().chain(q.joins.iter().map(|j| &j.src)) {
        if src.srf.is_some() {
            continue;
        }
        // (a subquery's columns are its select list's names: an inherited table's rows, `person*`, are one over it
        // and its children, its own columns)
        let cols = match &src.sub {
            Some(sub) => match sub_columns(sub) {
                Some(c) => c,
                None => continue,
            },
            None => match super::view_def(v, &src.table) {
                Ok(Some(d)) => d.cols.clone(),
                _ => match super::load(v, &src.table) {
                    Ok(Some(t)) => t.cols.iter().map(|c| c.name.clone()).collect(),
                    _ => continue,
                },
            },
        };
        // (a column alias list, `t AS p(a, b)`, names the first columns as `sources` renames them)
        let cols = src.names.iter().cloned().chain(cols.into_iter().skip(src.names.len())).collect();
        rels.push((src.alias.clone(), cols));
    }
    if rels.is_empty() {
        return;
    }
    let fix = |e: &Expr| attr_fix(v, &rels, e);
    for it in &mut q.items {
        if let Item::Expr(e, _) = it {
            *e = fix(e);
        }
    }
    for (e, _) in &mut q.order {
        *e = fix(e);
    }
    if let Some(w) = &mut q.filter {
        *w = fix(w);
    }
}

/// A subquery's column names, where its select list says them all (an alias, or a column's own name); None where
/// one can't be told (a `*`, an expression with no alias)
fn sub_columns(q: &Select) -> Option<Vec<String>> {
    q.items
        .iter()
        .map(|it| match it {
            Item::Expr(_, Some(alias)) => Some(alias.clone()),
            Item::Expr(Expr::Col(_, n), None) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// `e` with its attribute notation made calls (see `attribute_calls`; `rels` the FROM items' columns by name)
fn attr_fix(v: &View, rels: &[(String, Vec<String>)], e: &Expr) -> Expr {
    let call = |name: &str, arg: Expr| {
        Expr::Call(Box::new(Call {
            name: name.into(),
            args: vec![arg],
            names: vec![],
            def: None,
            via_call: false,
            builtin: false,
        }))
    };
    rewrite(e, &|x| match x {
        Expr::Col(Some(t), f) => {
            let (_, cols) = rels.iter().find(|(a, _)| a == t)?;
            (!cols.iter().any(|c| c == f) && !SYSTEM_COLUMNS.contains(&f.as_str()) && takes_one(v, f))
                // (`t.*`, the row: a bare `t` would be t's column of that name, where it has one)
                .then(|| call(f, Expr::Col(Some(t.clone()), "*".into())))
        }
        // `(x).f`: x made a call first where it is attribute notation itself (`(t.g).f`); then, if x is a call whose
        // rows have no field f, f's call of it
        Expr::Func(Func::FieldOf, a) if a.len() == 2 => {
            let inner = attr_fix(v, rels, &a[0]);
            let Expr::Lit(Value::Text(f)) = &a[1] else { return None };
            let fields = match &inner {
                // (only where the call can be only one function: its rows' fields are then that function's, whatever
                // its arguments' types, which aren't known yet)
                Expr::Call(c)
                    if named(v, &c.name).is_ok_and(|fs| fs.iter().filter(|g| g.takes(c.args.len())).count() == 1) =>
                {
                    let tys: Vec<Option<Type>> = c.args.iter().map(|_| None).collect();
                    srf_columns(v, &c.name, &tys, "?v")
                        .ok()
                        .map(|cs| cs.into_iter().map(|(n, _)| n).collect::<Vec<_>>())
                }
                _ => None,
            };
            match fields {
                Some(fs) if !fs.iter().any(|n| n == f) && takes_one(v, f) => Some(call(f, inner)),
                _ => Some(Expr::Func(Func::FieldOf, vec![inner, a[1].clone()])),
            }
        }
        _ => None,
    })
}

/// The system columns every table has, which attribute notation never takes for a function
const SYSTEM_COLUMNS: [&str; 7] = ["ctid", "oid", "tableoid", "xmin", "xmax", "cmin", "cmax"];

/// One level of `select_list_srfs`: the innermost set-returning call in the select list (one whose arguments hold
/// none) made a LATERAL source named `at`, its column where the call was. False where there was none, or one this
/// leaves: beside an aggregate, a window function or a grouping, or another at its level (Postgres runs those in
/// step, which this doesn't).
fn srf_round(v: &View, q: &mut Select, at: &str) -> bool {
    // (a call whose functions of its name, taking that many arguments, all return sets, not one row)
    let sets = |c: &Call| -> bool {
        if !c.names.is_empty() || c.builtin {
            return false;
        }
        if let Some(f) = &c.def {
            return f.ret.sets() && !f.single && !f.outs;
        }
        let Ok(fs) = named(v, &c.name) else { return false };
        let n = c.args.len();
        let taking: Vec<&Arc<Function>> =
            fs.iter().filter(|f| f.takes(n) || (f.variadic && !f.args.is_empty() && n + 1 >= f.args.len())).collect();
        // (OUT parameters without SETOF are one row)
        !taking.is_empty() && taking.iter().all(|f| f.ret.sets() && !f.single && !f.outs && f.ret != Ret::Trigger)
    };
    let (mut calls, mut other): (Vec<Call>, bool) = (vec![], false);
    for it in &q.items {
        let Item::Expr(e, _) = it else { continue };
        walk(e, &mut |x| match x {
            Expr::Call(c) if sets(c) && !calls.iter().any(|d| format!("{d:?}") == format!("{c:?}")) => {
                calls.push((**c).clone());
            }
            Expr::Call(c) if one_column_srf(c) => other = true,
            Expr::Agg(..) | Expr::Window(_) => other = true,
            _ => {}
        });
    }
    // (the innermost: a call whose arguments hold no other, as Postgres's ProjectSet runs a nested one first)
    let innermost: Vec<&Call> = calls
        .iter()
        .filter(|c| {
            let mut inner = false;
            for a in &c.args {
                walk(a, &mut |x| inner |= matches!(x, Expr::Call(d) if sets(d)));
            }
            !inner
        })
        .collect();
    let [call] = &innermost[..] else { return false };
    if other || !q.group.is_empty() || q.having.is_some() {
        return false;
    }
    // its columns, from its arguments' types where they can be told unbound (unknown otherwise)
    let tys: Vec<Option<Type>> = call.args.iter().map(|a| unbound_type(a, &[], None).flatten()).collect();
    let Ok(cols) = srf_columns(v, &call.name, &tys, "?v") else { return false };
    if cols.is_empty() {
        return false;
    }
    let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
    let column = match &names[..] {
        [one] => Expr::Col(Some(at.into()), one.clone()),
        all => Expr::Func(Func::RowValue, all.iter().map(|n| Expr::Col(Some(at.into()), n.clone())).collect()),
    };
    let src = Source {
        table: at.into(),
        alias: at.into(),
        sub: None,
        names: names.clone(),
        rec: None,
        working: false,
        at: None,
        lateral: true,
        srf: Some(Box::new((call.name.clone(), call.args.clone()))),
        cte: 0,
        only: false,
        hier: false,
        policed: false,
        bound: false,
    };
    let key = format!("{call:?}");
    let is_call = |x: &Expr| matches!(x, Expr::Call(c) if format!("{c:?}") == key);
    // a field of the call's rows, `(f(x)).name` or `name(f(x))` (functional notation, where no function `name` takes
    // the row: Postgres's ParseFuncOrColumn), is that column of the source
    let field =
        |f: &str| (names.len() > 1 && names.iter().any(|n| n == f)).then(|| Expr::Col(Some(at.into()), f.into()));
    let swap = |e: &Expr| {
        rewrite(e, &|x| match x {
            Expr::Call(c) if format!("{c:?}") == key => Some(column.clone()),
            Expr::Func(Func::FieldOf, a) if a.len() == 2 && is_call(&a[0]) => match &a[1] {
                Expr::Lit(Value::Text(f)) => field(f),
                _ => None,
            },
            Expr::Call(c)
                if c.args.len() == 1 && c.names.is_empty() && is_call(&c.args[0]) && !takes_one(v, &c.name) =>
            {
                field(rel_part(written(&c.name)))
            }
            _ => None,
        })
    };
    let own = rel_part(written(&call.name)).to_string();
    for it in &mut q.items {
        if let Item::Expr(e, alias) = it {
            let bare = matches!(e, Expr::Call(c) if format!("{c:?}") == key);
            *e = swap(e);
            if bare && alias.is_none() {
                *alias = Some(own.clone());
            }
        }
    }
    for (e, _) in &mut q.order {
        *e = swap(e);
    }
    match q.from {
        None => q.from = Some(src),
        Some(_) => q.joins.push(Join {
            kind: JoinKind::Cross,
            src,
            on: None,
            using: vec![],
            natural: false,
            using_as: None,
            comma: true,
        }),
    }
    true
}

/// The table whose rows a call in FROM of `name` with `n` arguments gives, when it calls a function made with
/// CREATE FUNCTION that returns them (RETURNS [SETOF] t): its columns are the table's as they are now.
pub(super) fn rows_of_call(v: &View, name: &str, n: usize) -> Option<String> {
    if name.contains(COLDEFS) {
        return None;
    }
    let (f, _) = pick_call(name, &named(v, name).ok()?, &vec![None; n]).ok()?;
    match &f.ret {
        Ret::RowsOf(t) => Some(t.clone()),
        _ => None,
    }
}

/// A set-returning function's columns (see `sources`), from its arguments' types (None: unknown).
pub(super) fn srf_columns(v: &View, name: &str, tys: &[Option<Type>], alias: &str) -> Result<Vec<(String, Type)>> {
    // (a column definition list after the call: what a function returning record gives)
    let (name, defs) = name.split_once(COLDEFS).unwrap_or((name, ""));
    let (f, packed) = pick_call(name, &named(v, name)?, tys)?;
    // (a polymorphic one: its result's types as the arguments make them)
    let mut tys = tys.to_vec();
    if packed {
        let rest = tys.split_off(f.args.len().saturating_sub(1).min(tys.len()));
        tys.push(packed_type(&f, &rest));
    }
    let f = instance(&f, &tys, true)?;
    let composite = match &f.ret {
        Ret::Value(t) | Ret::SetOf(t) => row_fields(*t).is_some(),
        Ret::RowsOf(_) => true,
        _ => false,
    };
    if !defs.is_empty() {
        let message = match &f.ret {
            Ret::Table(c) if c.is_empty() => {
                return Ok(column_defs(defs)?.into_iter().map(|c| (c.name, c.ty)).collect());
            }
            Ret::Table(_) => "a column definition list is redundant for a function with OUT parameters",
            _ if composite => "a column definition list is redundant for a function returning a named composite type",
            _ => "a column definition list is only allowed for functions returning \"record\"",
        };
        return err("42601", message);
    }
    if matches!(&f.ret, Ret::Table(c) if c.is_empty()) {
        return err("42601", "a column definition list is required for functions returning \"record\"");
    }
    Ok(match &f.ret {
        // (a composite type's: its fields)
        Ret::Value(t) | Ret::SetOf(t) if composite => row_fields(*t).unwrap_or_default(),
        Ret::Value(t) | Ret::SetOf(t) => vec![(alias.into(), resolve_type(v, *t)?)],
        Ret::Void => vec![(alias.into(), Type::Void)],
        Ret::Table(c) => c.iter().map(|(n, t)| Ok((n.clone(), resolve_type(v, *t)?))).collect::<Result<_>>()?,
        Ret::RowsOf(t) => need_table(v, t)?.cols.iter().map(|c| (c.name.clone(), c.ty)).collect(),
        Ret::Trigger => return err(UNSUPPORTED, "trigger functions can only be called as triggers"),
    })
}

// ---------------------------------------------------------------- walking statements

/// Calls `e` on every expression of a statement (then on what's inside it, after `e` may have
/// replaced it) and `q` on every SELECT, subqueries and FROM included.
pub(super) fn visit(
    s: &mut Stmt,
    e: &mut dyn FnMut(&mut Expr) -> Result<()>,
    q: &mut dyn FnMut(&mut Select),
) -> Result<()> {
    let items =
        |its: &mut Option<Vec<Item>>, e: &mut dyn FnMut(&mut Expr) -> Result<()>, q: &mut dyn FnMut(&mut Select)| {
            for it in its.iter_mut().flatten() {
                if let Item::Expr(x, _) = it {
                    visit_expr(x, e, q)?;
                }
            }
            Ok::<_, SqlError>(())
        };
    match s {
        Stmt::Select(sel) | Stmt::Locking(sel, _) => visit_select(sel, e, q),
        Stmt::Insert { rows, query, conflict, returning, .. } => {
            for x in rows.iter_mut().flatten() {
                visit_expr(x, e, q)?;
            }
            if let Some(sel) = query {
                visit_select(sel, e, q)?;
            }
            if let Some(p) = conflict.as_mut().and_then(|c| c.pred.as_mut()) {
                visit_expr(p, e, q)?;
            }
            if let Some(OnConflict { update: Some((sets, filter)), .. }) = conflict {
                for x in sets.iter_mut().map(|(_, x)| x).chain(filter.iter_mut()) {
                    visit_expr(x, e, q)?;
                }
            }
            items(returning, e, q)
        }
        Stmt::Update { sets, filter, returning, .. } => {
            for x in sets.iter_mut().map(|(_, x)| x).chain(filter.iter_mut()) {
                visit_expr(x, e, q)?;
            }
            items(returning, e, q)
        }
        Stmt::Delete { filter, returning, .. } => {
            if let Some(x) = filter {
                visit_expr(x, e, q)?;
            }
            items(returning, e, q)
        }
        Stmt::Explain { stmt, .. } => visit(stmt, e, q),
        Stmt::Execute { args, .. } => args.iter_mut().try_for_each(|x| visit_expr(x, e, q)),
        Stmt::With(w) => {
            for (_, st) in &mut w.dml {
                visit(st, e, q)?;
            }
            visit(&mut w.main, e, q)
        }
        _ => Ok(()),
    }
}

fn visit_select(
    s: &mut Select,
    e: &mut dyn FnMut(&mut Expr) -> Result<()>,
    q: &mut dyn FnMut(&mut Select),
) -> Result<()> {
    q(s);
    for (_, _, p) in &mut s.set {
        visit_select(p, e, q)?;
    }
    for it in &mut s.items {
        if let Item::Expr(x, _) = it {
            visit_expr(x, e, q)?;
        }
    }
    for x in &mut s.distinct_on {
        visit_expr(x, e, q)?;
    }
    for src in s.from.iter_mut().chain(s.joins.iter_mut().map(|j| &mut j.src)) {
        if let Some(sub) = &mut src.sub {
            visit_select(sub, e, q)?;
        }
        if let Some(r) = &mut src.rec {
            visit_select(&mut r.base, e, q)?;
            visit_select(&mut r.step, e, q)?;
        }
        if let Some(f) = &mut src.srf {
            for x in &mut f.1 {
                visit_expr(x, e, q)?;
            }
        }
        if let Some(x) = &mut src.at {
            visit_expr(x, e, q)?;
        }
    }
    for x in s.joins.iter_mut().filter_map(|j| j.on.as_mut()) {
        visit_expr(x, e, q)?;
    }
    let rest = s.filter.iter_mut().chain(s.group.iter_mut()).chain(s.having.iter_mut());
    for x in rest.chain(s.order.iter_mut().map(|(x, _)| x)).chain(s.limit.iter_mut()).chain(s.offset.iter_mut()) {
        visit_expr(x, e, q)?;
    }
    Ok(())
}

pub(super) fn visit_expr(
    x: &mut Expr,
    e: &mut dyn FnMut(&mut Expr) -> Result<()>,
    q: &mut dyn FnMut(&mut Select),
) -> Result<()> {
    e(x)?;
    match x {
        Expr::Neg(a) | Expr::Not(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) | Expr::Agg(_, Some(a), _) => {
            visit_expr(a, e, q)
        }
        Expr::Bin(a, _, b) | Expr::Like(a, b, ..) => {
            visit_expr(a, e, q)?;
            visit_expr(b, e, q)
        }
        Expr::In(a, l, _) => {
            visit_expr(a, e, q)?;
            l.iter_mut().try_for_each(|y| visit_expr(y, e, q))
        }
        Expr::Func(_, args) => args.iter_mut().try_for_each(|y| visit_expr(y, e, q)),
        Expr::Call(c) => c.args.iter_mut().try_for_each(|y| visit_expr(y, e, q)),
        Expr::Case(o, ws, el) => {
            for y in o.iter_mut().chain(el.iter_mut()) {
                visit_expr(y, e, q)?;
            }
            for (w, t) in ws {
                visit_expr(w, e, q)?;
                visit_expr(t, e, q)?;
            }
            Ok(())
        }
        Expr::Window(w) => {
            let Win { args, partition, order, frame, .. } = &mut **w;
            // (and a frame's offsets that read parameters: a function's argument by name is made one here)
            let offsets = frame.as_mut().map(|(_, a, b)| [a, b]).into_iter().flatten().filter_map(|b| match b {
                Bound::Param(x, _) => Some(&mut **x),
                _ => None,
            });
            for y in args.iter_mut().chain(partition.iter_mut()).chain(order.iter_mut().map(|(y, _)| y)).chain(offsets)
            {
                visit_expr(y, e, q)?;
            }
            Ok(())
        }
        Expr::Sub(sq) | Expr::Exists(sq, _) => visit_select(&mut sq.q, e, q),
        Expr::InSub(a, sq, _) => {
            visit_expr(a, e, q)?;
            visit_select(&mut sq.q, e, q)
        }
        _ => Ok(()),
    }
}

/// The names a statement's own column references could mean: its relations' columns (tables', views', sequences'
/// and catalog tables', in FROM, in subqueries, and the one it writes), and the names given to subqueries' and functions'
/// columns and to select-list items.
pub(super) fn column_names(v: &View, s: &Stmt) -> Result<HashSet<String>> {
    names_in(v, s, true)
}

/// `column_names`; `target`: with the columns of the table an INSERT, UPDATE or DELETE writes.
fn names_in(v: &View, s: &Stmt, target: bool) -> Result<HashSet<String>> {
    let mut s = s.clone();
    let (mut tables, mut names) = (vec![], HashSet::new());
    if let (Stmt::Insert { table, .. } | Stmt::Update { table, .. } | Stmt::Delete { table, .. }, true) = (&s, target) {
        tables.push(table.clone());
    }
    visit(&mut s, &mut |_| Ok(()), &mut |q| {
        for it in &q.items {
            if let Item::Expr(_, Some(a)) = it {
                names.insert(a.clone());
            }
        }
        for src in q.from.iter().chain(q.joins.iter().map(|j| &j.src)) {
            if src.sub.is_some() || src.srf.is_some() {
                names.extend(src.names.iter().cloned());
                names.insert(src.alias.clone());
            } else {
                tables.push(src.table.clone());
            }
        }
    })?;
    for t in tables {
        if let Ok(Some(cols)) = relation_columns(v, &t) {
            names.extend(cols);
        }
    }
    Ok(names)
}

/// Calls `f` on every expression of `s` with the names its column references could mean there
/// (see `column_names`): an INSERT's VALUES and query don't see the table it writes; its
/// RETURNING and ON CONFLICT do.
pub(super) fn substitute(
    v: &View,
    s: &mut Stmt,
    f: &mut dyn FnMut(&mut Expr, &HashSet<String>) -> Result<()>,
) -> Result<()> {
    // (a WITH whose queries write: each of them, and its main statement, with the names it sees itself)
    if let Stmt::With(w) = s {
        for (_, st) in &mut w.dml {
            substitute(v, st, f)?;
        }
        return substitute(v, &mut w.main, f);
    }
    let all = column_names(v, s)?;
    let Stmt::Insert { .. } = s else { return visit(s, &mut |e| f(e, &all), &mut |_| {}) };
    let bare = names_in(v, s, false)?;
    let Stmt::Insert { rows, query, conflict, returning, .. } = s else { unreachable!() };
    for x in rows.iter_mut().flatten() {
        visit_expr(x, &mut |e| f(e, &bare), &mut |_| {})?;
    }
    if let Some(q) = query {
        visit_select(q, &mut |e| f(e, &bare), &mut |_| {})?;
    }
    if let Some(OnConflict { update: Some((sets, filter)), .. }) = conflict {
        for x in sets.iter_mut().map(|(_, x)| x).chain(filter.iter_mut()) {
            visit_expr(x, &mut |e| f(e, &all), &mut |_| {})?;
        }
    }
    for it in returning.iter_mut().flatten() {
        if let Item::Expr(x, _) = it {
            visit_expr(x, &mut |e| f(e, &all), &mut |_| {})?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- running

/// A value's type, for choosing the function it is given to: an integer that fits 32 bits an integer's, NULL unknown.
pub(super) fn value_type(v: &Value) -> Option<Type> {
    match v {
        Value::Null => None,
        Value::Int(n) if i32::try_from(*n).is_ok() => Some(Type::Int),
        v => v.ty(),
    }
}

/// `value_type`, but unknown for an array's value (`{1,2}` text, a JSON array) or a record's (`(1,2)` text): the value
/// doesn't say which array or record type it is of, so a polymorphic parameter isn't refused it.
pub(super) fn value_type_loose(v: &Value) -> Option<Type> {
    match v {
        // (and a range's, `[42,49)`, or an array's with bounds, `[0:1]={1,2}`)
        Value::Text(s) if s.trim_start().starts_with(['{', '(', '[']) => None,
        Value::Json(Json::Array(_)) => None,
        v => value_type(v),
    }
}

impl Exec<'_> {
    /// The functions named `name`, found once per statement.
    pub(super) fn functions(&self, name: &str) -> Result<Arc<Vec<Arc<Function>>>> {
        if let Some(f) = self.nest.found.borrow().get(name) {
            return Ok(f.clone());
        }
        let fs = Arc::new(named(&self.view()?, name)?);
        self.nest.found.borrow_mut().insert(name.into(), fs.clone());
        Ok(fs)
    }

    /// The function a call of `name`, not bound, with these argument values means: each argument's type as it is
    /// `written`, reading columns `cols`, where that tells (a cast, a column's, a parameter's declared type, as a
    /// PL/pgSQL variable's; a parameter declared none is unknown, as a quoted literal is), else its value's (a
    /// subquery's, a call's).
    /// The function is made for the arguments' types if it is polymorphic (see `instance`), and a VARIADIC one is
    /// given the arguments from its last parameter on as its array.
    fn by_values(&self, name: &str, written: &[Expr], cols: &[Col], args: &mut Vec<Value>) -> Result<Arc<Function>> {
        let mut tys: Vec<Option<Type>> = args
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let Some(a) = written.get(i) else { return value_type_loose(v) };
                let mut unsure = false;
                walk(a, &mut |x| {
                    unsure |= match x {
                        Expr::Param(n) => self.declared.get(n - 1).copied().flatten().is_none(),
                        Expr::Call(c) => c.def.is_none(),
                        Expr::Idx(i) => *i >= cols.len(),
                        Expr::AggRef(_) | Expr::WinRef(..) | Expr::Default => true,
                        Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) => true,
                        _ => false,
                    }
                });
                let typed = match a {
                    Expr::Lit(Value::Null | Value::Text(_)) => Some(None),
                    Expr::Param(n) => Some(self.declared.get(n - 1).copied().flatten()),
                    _ if unsure => None,
                    a => ty_in(a, cols, self.declared, &[]).ok().map(Some),
                };
                typed.unwrap_or_else(|| value_type_loose(v))
            })
            .collect();
        let (f, packed) = pick_call(name, &self.functions(name)?, &tys)?;
        if packed {
            let at = f.args.len().saturating_sub(1).min(args.len());
            let rest = args.split_off(at);
            let rest_tys = tys.split_off(at);
            let array_ty = packed_type(&f, &rest_tys);
            let array = Expr::Func(Func::Scalar(funcs::Scalar::ArrayOf), rest.into_iter().map(Expr::Lit).collect());
            let array = match array_ty {
                Some(t) => Expr::Cast(Box::new(array), t),
                None => array,
            };
            args.push(eval(&array, &Env { cols: &[], row: &[], params: self.params, aggs: &[], sub: Some(self) })?);
            tys.push(array_ty);
        }
        instance(&f, &tys, true)
    }

    /// A call of a function made in the session's world, by a statement run on its temporary tables' database
    /// (`UPDATE tmp SET a = f(b)`): the function runs in the world, as a statement of its own there, where its
    /// tables and types are; what it reads of the temporary tables it reads as a statement of the world does (see
    /// `temp_table`), and what it writes to them it writes as one does (see `Exec::temp_dml`). None: this
    /// statement isn't one on that database.
    fn call_in_world(&self, c: &Call, args: &[Value], cols: &[Col]) -> Result<Option<Value>> {
        let Some(temp) = super::session_temp() else { return Ok(None) };
        if !std::ptr::eq(self.db, &*temp) || !c.names.is_empty() {
            return Ok(None);
        }
        let Some((core, world, actor)) = super::catalog::home() else { return Ok(None) };
        let Some(db) = core.db() else { return Ok(None) };
        // (its arguments with the types they were written as: which function of the name is meant)
        let given: Vec<Expr> = args
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let ty = c.args.get(i).and_then(|a| ty_in(a, cols, self.declared, &[]).ok());
                match ty.or_else(|| value_type_loose(v)) {
                    Some(t) => Expr::Cast(Box::new(Expr::Lit(v.clone())), t),
                    None => Expr::Lit(v.clone()),
                }
            })
            .collect();
        let call = Call { name: c.name.clone(), args: given, names: vec![], def: c.def.clone(), ..c.clone() };
        let query = select_of(Expr::Call(Box::new(call)));
        let st = Statement(Stmt::Select(Box::new(query)), PlanCache::default(), true);
        let _as = crate::acting_as(&actor);
        let out = super::catalog::in_world(&core, &world, || st.execute(&db, &world, &[], &[]))?;
        Ok(Some(out.rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null)))
    }

    /// A call of a function in an expression, its arguments reading columns `cols`.
    pub(super) fn call_function(&self, c: &Call, mut args: Vec<Value>, cols: &[Col]) -> Result<Value> {
        if let Some(v) = self.call_in_world(c, &args, cols)? {
            return Ok(v);
        }
        let f = match &c.def {
            Some(f) => f.clone(),
            None => self.by_values(&c.name, &c.args, cols, &mut args)?,
        };
        match f.ret {
            Ret::Value(_) | Ret::Void => {}
            // (OUT parameters: one row, which a call in an expression gives as a record, or as its one value; a
            // record or a table's row, not SETOF: one row, as a record)
            Ret::Table(_) if f.outs => {}
            Ret::Table(_) | Ret::RowsOf(_) if f.single => {}
            Ret::Trigger => return err(UNSUPPORTED, "trigger functions can only be called as triggers"),
            _ => {
                return err(
                    UNSUPPORTED,
                    format!("{}() returns a set of rows: call it in FROM, like SELECT * FROM {}(...)", f.name, f.name),
                );
            }
        }
        let ahead = f.ret == Ret::Void && self.run_ahead(&f, &c.args)?;
        match self.invoke(&f, args, None)? {
            // (Postgres ran it while planning, as a SQL function: NULL)
            Returned::Value(_) if ahead => Ok(Value::Null),
            Returned::Value(v) => Ok(v),
            Returned::Rows(rows) if f.outs => Ok(match rows.into_iter().next() {
                Some(mut row) if row.len() == 1 => row.remove(0),
                Some(row) => Value::Text(super::composite_text(&row)),
                None => Value::Null,
            }),
            Returned::Rows(rows) if f.single => Ok(match rows.into_iter().next() {
                Some(row) => Value::Text(super::composite_text(&row)),
                None => Value::Null,
            }),
            Returned::Record(Some(row)) if f.single || matches!(f.ret, Ret::Value(_)) => {
                Ok(Value::Text(super::composite_text(&row)))
            }
            _ => Ok(Value::Null),
        }
    }

    /// Whether Postgres works out a call of `f` with arguments `args` while it plans the query, running it rather
    /// than inlining it: a SQL function marked IMMUTABLE whose arguments are all constants (its result then is what
    /// running it gives).
    fn run_ahead(&self, f: &Function, args: &[Expr]) -> Result<bool> {
        if f.lang != Lang::Sql || f.volatility != Volatility::Immutable {
            return Ok(false);
        }
        for a in args {
            let mut fixed = true;
            walk(a, &mut |x| {
                fixed &= !matches!(
                    x,
                    Expr::Col(..)
                        | Expr::Idx(_)
                        | Expr::Param(_)
                        | Expr::Sub(_)
                        | Expr::Exists(..)
                        | Expr::InSub(..)
                        | Expr::Agg(..)
                        | Expr::AggRef(_)
                        | Expr::Window(_)
                        | Expr::WinRef(..)
                )
            });
            if !fixed || self.volatility_of(a)? != Volatility::Immutable {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// How volatile `e` is: as its most volatile function, as Postgres marks them.
    fn volatility_of(&self, e: &Expr) -> Result<Volatility> {
        use CtxFn::*;
        use funcs::Scalar::{Random, RandomNormal, SetSeed};
        let mut v = Volatility::Immutable;
        let mut calls = vec![];
        walk(e, &mut |x| {
            let here = match x {
                Expr::Func(Func::GenRandomUuid | Func::ClockTs | Func::Scalar(Random | RandomNormal | SetSeed), _) => {
                    Volatility::Volatile
                }
                Expr::Func(Func::Scalar(f), _) if super::pgcrypto::volatile(*f) => Volatility::Volatile,
                Expr::Func(
                    Func::Ctx(
                        NextVal | CurrVal | SetVal | LastVal | Notify | Sleep | SetConfig | AdvisoryLock(..)
                        | AdvisoryUnlock(_) | AdvisoryUnlockAll | CancelBackend | TerminateBackend | BlockingPids,
                    ),
                    _,
                ) => Volatility::Volatile,
                Expr::Func(Func::Ctx(_) | Func::Now | Func::StatementTs | Func::CurrentDate, _) => Volatility::Stable,
                Expr::Call(c) => {
                    calls.push((c.name.clone(), c.def.clone()));
                    Volatility::Immutable
                }
                _ => Volatility::Immutable,
            };
            if here < v {
                v = here;
            }
        });
        for (name, def) in calls {
            for f in self.candidates(&name, def)?.iter() {
                if f.volatility < v {
                    v = f.volatility;
                }
            }
        }
        Ok(v)
    }

    /// The functions a call of `name` may mean: the one it was bound to (`def`), else all of that name.
    fn candidates(&self, name: &str, def: Option<Arc<Function>>) -> Result<Arc<Vec<Arc<Function>>>> {
        Ok(match def {
            Some(f) => Arc::new(vec![f]),
            None => self.functions(name)?,
        })
    }

    /// Whether `e` gives NULL whenever any of its arguments is NULL, everywhere in it, as Postgres's
    /// contain_nonstrict_functions sees it.
    fn strict_throughout(&self, e: &Expr) -> Result<bool> {
        use funcs::Scalar::{Concat, ConcatWs, Format, Greatest, Least, Nullif, NumNonnulls, NumNulls};
        let mut strict = true;
        let mut calls = vec![];
        walk(e, &mut |x| {
            strict &= !matches!(
                x,
                Expr::Case(..)
                    | Expr::IsNull(..)
                    | Expr::Bin(_, Op::And | Op::Or, _)
                    | Expr::Agg(..)
                    | Expr::Window(_)
                    | Expr::Sub(_)
                    | Expr::Exists(..)
                    | Expr::InSub(..)
                    | Expr::Func(
                        Func::Coalesce
                            | Func::Distinct(_)
                            | Func::Record(_)
                            | Func::Row
                            | Func::RowValue
                            | Func::Quant(..)
                            | Func::QuantArray(..)
                            | Func::Ctx(CtxFn::Notify | CtxFn::SetConfig | CtxFn::TypeOf)
                            | Func::Scalar(
                                Concat | ConcatWs | Format | Greatest | Least | Nullif | NumNulls | NumNonnulls
                            ),
                        _,
                    )
            );
            if let Expr::Call(c) = x {
                calls.push((c.name.clone(), c.def.clone()));
            }
        });
        for (name, def) in calls {
            strict &= self.candidates(&name, def)?.iter().all(|f| f.strict);
        }
        Ok(strict)
    }

    /// Whether Postgres inlines a call of `f`, a SQL function returning void whose body is `stmts` (its arguments made
    /// parameters) and whose last statement's column is of type `out`: it then gives what its body's expression gives
    /// (the void value of the function it calls), where running it gives NULL. That takes a body of one SELECT of one
    /// void value and nothing else (no FROM, WHERE, WITH, subquery, aggregate ...), no more volatile than `f`, and for
    /// a strict `f`, strict throughout and reading every parameter (inline_function).
    fn inlined(&self, f: &Function, stmts: &[Rc<Stmt>], out: Option<Type>) -> Result<bool> {
        let [s] = stmts else { return Ok(false) };
        let Stmt::Select(q) = &**s else { return Ok(false) };
        let [Item::Expr(e, _)] = q.items.as_slice() else { return Ok(false) };
        let with = f.body.trim_start_matches(|c: char| c == '(' || c.is_whitespace()).get(..4);
        if f.procedure
            || out != Some(Type::Void)
            || with.is_some_and(|w| w.eq_ignore_ascii_case("with"))
            || !q.set.is_empty()
            || q.distinct
            || q.from.is_some()
            || !q.joins.is_empty()
            || q.filter.is_some()
            || !q.group.is_empty()
            || q.having.is_some()
            || !q.order.is_empty()
            || q.limit.is_some()
            || q.offset.is_some()
        {
            return Ok(false);
        }
        let mut plain = true;
        let mut read = vec![false; f.args.len()];
        walk(e, &mut |x| match x {
            Expr::Agg(..) | Expr::Window(_) | Expr::Sub(_) | Expr::Exists(..) | Expr::InSub(..) => plain = false,
            Expr::Param(n) => {
                if let Some(r) = read.get_mut(*n - 1) {
                    *r = true;
                }
            }
            _ => {}
        });
        let enough = match f.volatility {
            Volatility::Volatile => true,
            v => self.volatility_of(e)? >= v,
        };
        Ok(plain && enough && (!f.strict || (read.iter().all(|r| *r) && self.strict_throughout(e)?)))
    }

    /// A call of a function in FROM: its rows, for arguments `written` (bound, reading columns `cols`) of these values.
    pub(super) fn call_srf(
        &self,
        name: &str,
        written: &[Expr],
        cols: &[Col],
        args: Vec<Value>,
    ) -> Result<Vec<Vec<Value>>> {
        let (name, defs) = name.split_once(COLDEFS).unwrap_or((name, ""));
        let mut args = args;
        let f = self.by_values(name, written, cols, &mut args)?;
        // the parameters a call leaves out take their defaults
        for at in args.len()..f.args.len() {
            let v = self.view()?;
            let e = Binder::plain(&[], Some(&v)).bind(&default_arg(&f, at)?)?;
            args.push(eval(&e, &Env { cols: &[], row: &[], params: self.params, aggs: &[], sub: Some(self) })?);
        }
        // (a composite type's fields are its columns)
        let fields = match &f.ret {
            Ret::Value(t) | Ret::SetOf(t) => row_fields(*t),
            _ => None,
        };
        let tys: Vec<Type> = match &f.ret {
            _ if fields.is_some() => fields.iter().flatten().map(|c| c.1).collect(),
            Ret::Value(t) | Ret::SetOf(t) => vec![self.resolved(*t)?],
            Ret::Void => vec![Type::Void],
            // (a function returning record: the columns the call lists)
            Ret::Table(c) if c.is_empty() => column_defs(defs)?.into_iter().map(|c| c.ty).collect(),
            Ret::Table(c) => c.iter().map(|c| self.resolved(c.1)).collect::<Result<_>>()?,
            Ret::RowsOf(t) => need_table(&self.view()?, t)?.cols.iter().map(|c| c.ty).collect(),
            Ret::Trigger => return err(UNSUPPORTED, "trigger functions can only be called as triggers"),
        };
        // (in FROM, its arguments are taken as constants: see `run_ahead`)
        let ahead = f.ret == Ret::Void && self.run_ahead(&f, &[])?;
        let rows = match self.invoke(&f, args, None)? {
            Returned::Rows(r) => r,
            Returned::Value(_) if ahead => vec![vec![Value::Null]],
            Returned::Value(v) => vec![vec![v]],
            Returned::Record(Some(row)) => vec![row],
            Returned::Record(None) => vec![],
        };
        // (a function returning a record or a table's row, not SETOF, gives its first row; none, a row of nulls)
        let rows = match f.single {
            true if rows.is_empty() => vec![vec![Value::Null; tys.len()]],
            true => rows.into_iter().take(1).collect(),
            false => rows,
        };
        // a composite value given as one (a PL/pgSQL row, a SQL function's single row as its text): its fields; a null
        // one, a row of nulls
        let rows = match tys.len() {
            n if fields.is_some() && n != 1 => rows
                .into_iter()
                .map(|r| {
                    if let [one] = &r[..] {
                        match one {
                            Value::Null => return Ok(vec![Value::Null; n]),
                            Value::Text(s) => {
                                return match super::composite_fields(s) {
                                    Some(fs) if fs.len() == n => {
                                        Ok(fs.into_iter().map(|x| x.map_or(Value::Null, Value::Text)).collect())
                                    }
                                    _ => err("22P02", format!("malformed record literal: \"{s}\"")),
                                };
                            }
                            _ => {}
                        }
                    }
                    Ok(r)
                })
                .collect::<Result<Vec<_>>>()?,
            _ => rows,
        };
        rows.into_iter()
            .map(|r| {
                if r.len() != tys.len() {
                    return err(
                        "42804",
                        format!(
                            "structure of query does not match function result type: {} columns returned, {} expected",
                            r.len(),
                            tys.len()
                        ),
                    );
                }
                r.into_iter().zip(&tys).map(|(v, t)| coerce(v, *t)).collect()
            })
            .collect()
    }

    /// Runs a function on its arguments (and a trigger's, for a trigger function).
    fn invoke(&self, f: &Arc<Function>, mut args: Vec<Value>, trig: Option<plpgsql::Trig>) -> Result<Returned> {
        tick()?;
        // an argument of a domain is made the domain's before the call (a NULL too, strict or not), as Postgres
        // coerces it where the call is
        if f.args.iter().any(|(_, t)| matches!(t, Type::Enum(_))) {
            let v = self.view()?;
            for (a, (_, t)) in args.iter_mut().zip(&f.args) {
                if super::domain::of_type(&v, *t)?.is_some() {
                    *a = coerce(std::mem::take(a), self.resolved(*t)?)?;
                    super::domain::check_type(&v, *t, a, Some(self as &dyn Runner))?;
                }
            }
        }
        if f.strict && args.contains(&Value::Null) {
            return Ok(if f.ret.sets() { Returned::Rows(vec![]) } else { Returned::Value(Value::Null) });
        }
        for (a, (_, t)) in args.iter_mut().zip(&f.args) {
            *a = coerce(std::mem::take(a), self.resolved(*t)?)?;
        }
        let d = self.nest.depth.get();
        if stack_low() {
            return err(
                "54001",
                "stack depth limit exceeded: functions nest too deep for the stack this thread has left",
            );
        }
        if d >= MAX_DEPTH {
            return err("54001", "stack depth limit exceeded".to_string());
        }
        // (kept out of this frame, which each level of a recursive function adds to the stack)
        let owner = self.runs_as(f, trig.is_none())?;
        // (timed for track_functions: see `pgstat`)
        let timing = super::pgstat::calling(f);
        self.nest.depth.set(d + 1);
        // (as its owner, along its own search_path: `$user` is the role it runs as)
        let call = || match f.own_path() {
            Some(path) => super::with_own_path(path, true, || self.run_code(f, args, trig)),
            None => self.run_code(f, args, trig),
        };
        let scope = super::FunctionSettings::enter(&f.config);
        let out = match owner {
            Some(o) => roles::acting(Some(o), call),
            None => call(),
        };
        scope.leave(out.is_ok());
        self.nest.depth.set(d);
        if let Some(t) = timing {
            super::pgstat::called(t, out.is_ok());
        }
        // a SQL function's result of a domain is held to the domain's constraints (PL/pgSQL's are where it returns)
        match (out, &f.ret, &f.code) {
            (Ok(Returned::Value(v)), Ret::Value(t @ Type::Enum(_)), Code::Sql(_)) => {
                super::domain::check_type(&self.view()?, *t, &v, Some(self as &dyn Runner))?;
                Ok(Returned::Value(v))
            }
            (out, ..) => out,
        }
    }

    /// EXECUTE on `f` (a trigger's function was checked when the trigger was made), and the role a
    /// SECURITY DEFINER function runs as: its owner.
    #[inline(never)]
    fn runs_as(&self, f: &Arc<Function>, checked: bool) -> Result<Option<Arc<roles::Who>>> {
        let who = match (checked, roles::checked()) {
            (true, Some(who)) => {
                acl::may_execute(&self.view()?, &who, f)?;
                Some(who)
            }
            _ => None,
        };
        if !f.definer {
            return Ok(None);
        }
        let who = who.or_else(roles::current).map_or_else(|| roles::Who::boot("chronos"), |w| (*w).clone());
        Ok(Some(Arc::new(acl::definer(&self.view()?, &who, f)?)))
    }

    fn run_code(&self, f: &Arc<Function>, args: Vec<Value>, trig: Option<plpgsql::Trig>) -> Result<Returned> {
        // (a C trigger function of the regression tests' libraries runs as its C does: see `regress_c`; called other
        // than by a row trigger, it fails as the C does, never running the body it is kept with)
        if let Some(c) = c_trigger_of(f) {
            return match &trig {
                Some(t) => self.c_trigger(c, t).map(Returned::Record),
                None => Err(elog(c.not_fired())),
            };
        }
        match &f.code {
            Code::Sql(stmts) => self.run_sql_function(f, stmts, args),
            Code::Pl(b) => plpgsql::run(self, f, b, args, trig),
        }
    }

    /// `t`, an enum's labels looked up if the parser only knew its name.
    pub(super) fn resolved(&self, t: Type) -> Result<Type> {
        match t {
            // (a parameter may be of a table's or view's row type, `f(t onek)`: a record of its columns)
            Type::Enum(i) if enum_type(i).labels.is_none() => {
                let v = self.view()?;
                match resolve_type(&v, t) {
                    Err(e) if e.code == "42704" => row_type_named(&v, t)?.ok_or(e),
                    r => r,
                }
            }
            t => Ok(t),
        }
    }

    fn run_sql_function(&self, f: &Arc<Function>, stmts: &[Stmt], args: Vec<Value>) -> Result<Returned> {
        let types: Vec<Option<Type>> = f.args.iter().map(|a| self.resolved(a.1).map(Some)).collect::<Result<_>>()?;
        let x = self.with_params(&args, &types, f.volatility != Volatility::Volatile);
        let mut last = None;
        let mut ran = vec![];
        // (its statements' sources are typed by its parameters' types: see `sources`)
        let declared: Vec<Type> = types.iter().map(|t| t.unwrap_or(Type::Text)).collect();
        let was = super::PARAM_TYPES.replace(declared);
        let ran_all = (|| -> Result<()> {
            for s in stmts {
                let s = self.named_args(f, s)?;
                last = Some(x.run_body(&s, true)?);
                ran.push(s);
            }
            Ok(())
        })();
        super::PARAM_TYPES.set(was);
        ran_all?;
        let (columns, rows) = last.map(|o| (o.columns, o.rows)).unwrap_or_default();
        let from = columns.as_ref().and_then(|c| c.first()).map(|c| c.1);
        Ok(match &f.ret {
            // a composite type's value: its first row's columns as its fields, as its text
            Ret::Value(t)
                if row_fields(*t)
                    .is_some_and(|fs| rows.first().is_some_and(|r| r.len() == fs.len() && r.len() != 1)) =>
            {
                let fs = row_fields(*t).unwrap_or_default();
                let r = rows.into_iter().next().unwrap_or_default();
                let vals = r.into_iter().zip(&fs).map(|(v, (_, ft))| coerce(v, *ft)).collect::<Result<Vec<_>>>()?;
                Returned::Value(Value::Text(super::composite_text(&vals)))
            }
            Ret::Value(t) => Returned::Value(match rows.first() {
                Some(r) if r.len() != 1 => {
                    return err(
                        "42P13",
                        format!(
                            "return type mismatch in function declared to return {}: its final statement returns {} columns",
                            t.name(),
                            r.len()
                        ),
                    );
                }
                Some(r) => assigned(r[0].clone(), from, self.resolved(*t)?)?,
                None => Value::Null,
            }),
            // a SQL function's void is NULL, as Postgres's is when it runs one, unless Postgres inlines it
            Ret::Void => Returned::Value(match rows.first() {
                Some(r) if self.inlined(f, &ran, from)? => r[0].clone(),
                _ => Value::Null,
            }),
            ret => {
                // a regproc returned as text is its function's name
                let to: Vec<Type> = match ret {
                    Ret::SetOf(t) => vec![self.resolved(*t)?],
                    Ret::Table(cols) => cols.iter().map(|c| c.1).collect(),
                    _ => vec![],
                };
                let from: Vec<Type> = columns.iter().flatten().map(|c| c.1).collect();
                let mut rows = rows;
                // a table's row type given as one composite value (`select row($1, $2)::int8_tbl`): its fields, as
                // Postgres takes a final SELECT of one column of the function's row type; told by the column's type,
                // so a table of one text column still takes a text value as it is
                if let (Ret::RowsOf(t), [Type::Record(_)]) = (ret, &from[..]) {
                    let n = need_table(&self.view()?, t)?.cols.len();
                    for r in rows.iter_mut() {
                        let fields = match &r[..] {
                            [Value::Null] => Some(vec![Value::Null; n]),
                            [Value::Text(s)] => super::composite_fields(s)
                                .filter(|fs| fs.len() == n)
                                .map(|fs| fs.into_iter().map(|x| x.map_or(Value::Null, Value::Text)).collect()),
                            _ => None,
                        };
                        if let Some(fs) = fields {
                            *r = fs;
                        }
                    }
                }
                for (j, _) in to.iter().enumerate().filter(|(_, t)| super::takes_text(**t)) {
                    let f = (from.get(j).copied()).filter(|t| t.reg_like());
                    for v in rows.iter_mut().filter_map(|r| r.get_mut(j)).filter(|_| f.is_some()) {
                        *v = proc_text(f, std::mem::take(v))?;
                    }
                }
                Returned::Rows(rows)
            }
        })
    }

    /// `s`, a SQL function's statement, with its arguments' names made parameters: `$1` for
    /// the first. As in Postgres, a column of the same name wins; `f.arg` is the argument.
    fn named_args(&self, f: &Arc<Function>, s: &Stmt) -> Result<Rc<Stmt>> {
        let at = s as *const Stmt as usize;
        if let Some((_, s)) = self.nest.prepared.borrow().get(&at) {
            return Ok(s.clone());
        }
        let mut out = s.clone();
        // an enum argument is read with its type, so it compares by its labels' places, a regproc so it is text
        // as its function's name, and a composite so its fields are known (`$1.f`)
        let typed = |t: Type| matches!(t, Type::Enum(_) | Type::Reg(_) | Type::Record(_));
        let param = |i: usize| match f.args[i].1 {
            t if typed(t) => Expr::Cast(Box::new(Expr::Param(i + 1)), t),
            _ => Expr::Param(i + 1),
        };
        let enums = f.args.iter().any(|a| typed(a.1));
        if enums {
            // `$n` first, as a name, so that the casts made below aren't looked at again: a name SQL can't write (a
            // NUL starts it), so a column of the body's own named "$1" isn't taken for the parameter
            visit(
                &mut out,
                &mut |e| {
                    if let Expr::Param(n) = e
                        && f.args.get(*n - 1).is_some_and(|a| typed(a.1))
                    {
                        *e = Expr::Col(None, format!("\0${n}"));
                    }
                    Ok(())
                },
                &mut |_| {},
            )?;
        }
        if enums || f.args.iter().any(|a| a.0.is_some()) {
            // (which parameters are rows: of a composite type, or a table's row type, `f(t onek)`)
            let rows: Vec<bool> = f
                .args
                .iter()
                .map(|a| match a.1 {
                    Type::Record(_) => true,
                    t @ Type::Enum(_) => matches!(self.resolved(t), Ok(Type::Record(_))),
                    _ => false,
                })
                .collect();
            substitute(&self.view()?, &mut out, &mut |e, cols| {
                let i = match e {
                    Expr::Col(None, n) if n.starts_with("\0$") => n[2..].parse::<usize>().ok().map(|k| k - 1),
                    Expr::Col(None, n) if !cols.contains(n) => f.arg_named(n),
                    Expr::Col(Some(q), n) if *q == f.name => f.arg_named(n),
                    // (p).f, p a composite parameter: its field f
                    Expr::Col(Some(q), n) if f.arg_named(q).is_some_and(|k| rows[k]) => {
                        let k = f.arg_named(q).unwrap_or_default();
                        *e = Expr::Func(Func::FieldOf, vec![param(k), Expr::Lit(Value::Text(n.clone()))]);
                        return Ok(());
                    }
                    _ => None,
                };
                if let Some(i) = i {
                    *e = param(i);
                }
                Ok(())
            })?;
        }
        let out = Rc::new(out);
        self.nest.prepared.borrow_mut().insert(at, (f.clone(), out.clone()));
        Ok(out)
    }

    // ------------------------------------------------------------ DDL

    pub(super) fn create_function(&self, f: &Function, replace: bool, procedure: bool) -> Result<Output> {
        // (its `%TYPE` references, as Postgres says them when it looks the signature's types up)
        for n in &f.type_notes {
            super::notice("NOTICE", n.clone());
        }
        // (what plpgsql.extra_warnings and extra_errors say of its body, as Postgres's compiler says it when made)
        if let Code::Pl(b) = &f.code {
            plpgsql::created(b)?;
        }
        // (LEAKPROOF says it reveals nothing of its arguments, which only a superuser may vouch for)
        if f.tuning.leakproof && roles::current().is_some_and(|w| !w.superuser) {
            return err("42501", "only superuser can define a leakproof function");
        }
        // a shell type (`CREATE TYPE name` alone) in its signature: Postgres's notices, the type to be filled in by
        // the CREATE TYPE these functions are for
        let shell = |t: &Type| {
            t.fields()
                .is_some_and(|f| f.is_empty())
                .then(|| userdefs::composite_name(*t))
                .flatten()
                .filter(|n| userdefs::is_shell(n))
        };
        if let Ret::Value(t) | Ret::SetOf(t) = &f.ret
            && let Some(n) = shell(t)
        {
            super::notice("NOTICE", format!("return type {} is only a shell", rel_part(&n)));
        }
        for (_, t) in &f.args {
            if let Some(n) = shell(t) {
                super::notice("NOTICE", format!("argument type {} is only a shell", rel_part(&n)));
            }
        }
        self.db.batch_with(self.branch, |v| {
            need_schema(v, &f.name)?;
            // names the parser took for enum types: each must be one, but SETOF a table's name is
            // its rows
            let known = |t: &Type| -> Result<bool> {
                Ok(match t {
                    Type::Enum(i) => v.get(&type_key(&enum_name(*i)))?.is_some() || enum_type(*i).labels.is_some(),
                    _ => true,
                })
            };
            let missing = |t: &Type| err("42704", format!("type \"{}\" does not exist", type_named(t)));
            // a relation's row type by its name as the search_path finds it, as a query's relation is (one outside
            // public is stored schema-qualified)
            let rel_named = |t: &Type| {
                let n = type_named(t);
                if n.contains('.') { n } else { super::find_named(&n, super::Kind::Rel) }
            };
            let rows_of = match &f.ret {
                Ret::SetOf(t) if !known(t)? && load(v, &rel_named(t))?.is_some_and(|t| t.schema) => {
                    let mut j = f.to_json();
                    j["returns"] = serde_json::json!({ "rows_of": rel_named(t) });
                    Some(Function::from_json(&j)?)
                }
                // a table's row type, not SETOF: one row of its columns (see `Function::single`)
                Ret::Value(t) if !known(t)? && load(v, &rel_named(t))?.is_some_and(|t| t.schema) => {
                    let mut j = f.to_json();
                    j["returns"] = serde_json::json!({ "rows_of": rel_named(t) });
                    j["single"] = Json::Bool(true);
                    Some(Function::from_json(&j)?)
                }
                Ret::SetOf(t) | Ret::Value(t) if !known(t)? => {
                    if matches!(f.ret, Ret::Value(_)) && load(v, &rel_named(t))?.is_some() {
                        let n = type_named(t);
                        return err(
                            UNSUPPORTED,
                            format!("returning a row of type {n} isn't supported; use RETURNS SETOF {n} or RETURNS TABLE (...)"),
                        );
                    }
                    return missing(t);
                }
                Ret::Table(c) => {
                    if let Some((_, t)) = c.iter().find(|(_, t)| !matches!(known(t), Ok(true))) {
                        return missing(t);
                    }
                    None
                }
                _ => None,
            };
            // (an argument may be of a table's or view's row type, `hobbies(person)`; by its bare name, as a call
            // looks the argument's type up by it)
            let relation = |t: &Type| -> Result<bool> {
                let n = type_named(t);
                Ok(load(v, &n)?.is_some_and(|t| t.schema) || super::view_def(v, &n)?.is_some())
            };
            if let Some((_, t)) =
                f.args.iter().find(|(_, t)| !matches!(known(t), Ok(true)) && !matches!(relation(t), Ok(true)))
            {
                return missing(t);
            }
            let f = rows_of.as_ref().unwrap_or(f);
            let key = f.key();
            if let Some(old) = v.get(&key)? {
                if !replace {
                    return err("42723", format!("function \"{}\" already exists with same argument types", f.name));
                }
                // (one OUT parameter's function returns its type, whatever the parameter is named)
                let same = match (&Function::from_raw(&old)?.ret, &f.ret) {
                    (Ret::Table(a), Ret::Table(b)) if a.len() == 1 && b.len() == 1 => a[0].1 == b[0].1,
                    (a, b) => a == b,
                };
                if !same {
                    return err(
                        "42P13",
                        format!("cannot change return type of existing function; DROP FUNCTION {} first", f.sig()),
                    );
                }
            }
            // its oid, kept from now on: the one it has (OR REPLACE), else a free one for its signature
            let mut j = f.to_json();
            j["oid"] = super::catalog_objects::new_func_oid(&self.db.core, v, &f.sig())?.into();
            Ok((vec![(key, Some(j.to_string()))], ()))
        })?;
        // function bodies parsed before it are parsed again: a call in one may mean it now
        forget_parsed();
        Ok(Output::tag(if procedure { "CREATE PROCEDURE" } else { "CREATE FUNCTION" }))
    }

    pub(super) fn drop_function(
        &self,
        names: &[(String, Option<Vec<Type>>)],
        if_exists: bool,
        cascade: bool,
        (procedure, routine): (bool, bool),
    ) -> Result<Output> {
        // (DROP ROUTINE: either kind; its messages name a function, as Postgres's do)
        let noun = match (procedure, routine) {
            (_, true) => "routine",
            (true, _) => "procedure",
            _ => "function",
        };
        let missing = if procedure && !routine { "procedure" } else { "function" };
        self.db.batch_with(self.branch, |v| {
            let mut writes = vec![];
            let mut tables: Vec<Table> = schemas(v)?;
            for (name, types) in names {
                // as in Postgres, a routine of the other kind still hides a later one of its argument types; without
                // them, only those of the kind dropped count
                let mut all = visible(name, named(v, name)?);
                if types.is_none() && !routine {
                    all.retain(|f| f.procedure == procedure);
                }
                let found = match types {
                    Some(ts) => all.iter().find(|f| f.args.iter().map(|a| type_named(&a.1)).eq(ts.iter().map(type_named))).cloned(),
                    None if all.len() > 1 => {
                        return Err(SqlError {
                            code: "42725",
                            message: format!("{noun} name \"{}\" is not unique", written(name)),
                            info: Some(Box::new(Violation {
                                hint: Some(format!("Specify the argument list to select the {noun} unambiguously.")),
                                ..Violation::default()
                            })),
                        });
                    }
                    None => all.first().cloned(),
                };
                let Some(f) = found else {
                    // an argument type that names nothing is what's missing, as Postgres looks the types up first
                    let unknown = types.iter().flatten().find_map(|t| match *t {
                        Type::Enum(i) if i & UNRESOLVED_BIT != 0 => {
                            unresolved_name(i).filter(|n| v.get(&type_key(n)).ok().flatten().is_none())
                        }
                        _ => None,
                    });
                    let shown = match (types, &unknown) {
                        (_, Some(n)) => format!("type \"{}\" does not exist", shown_type(n)),
                        (Some(ts), None) => format!("{missing} {}({}) does not exist", written(name), ts.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")),
                        (None, None) => format!("could not find a {missing} named \"{}\"", written(name)),
                    };
                    if if_exists {
                        // (Postgres names the types as the statement wrote them: `int` as pg_catalog.int4)
                        let shown = match (types, unknown) {
                            (Some(ts), None) => format!("function {}({}) does not exist", written(name), ts.iter().map(written_type).collect::<Vec<_>>().join(",")),
                            _ => shown,
                        };
                        notice("NOTICE", format!("{shown}, skipping"));
                        continue;
                    }
                    if unknown.is_some() {
                        return err("42704", shown);
                    }
                    return err(NO_FUNCTION, shown);
                };
                if f.procedure != procedure && !routine {
                    let what = if procedure { "a procedure" } else { "a function" };
                    return err("42809", format!("{} is not {what}", f.sig()));
                }
                if f.ret == Ret::Trigger {
                    // triggers call it by name
                    for t in &mut tables {
                        let before = t.triggers.len();
                        let using: Vec<String> = t.triggers.iter().filter(|x| x.func == f.name).map(|x| x.name.clone()).collect();
                        if using.is_empty() {
                            continue;
                        }
                        if !cascade {
                            return err(
                                "2BP01",
                                format!("cannot drop function {} because trigger {} on table {} depends on it; DROP ... CASCADE drops the triggers too", f.sig(), using[0], t.name),
                            );
                        }
                        t.triggers.retain(|x| x.func != f.name);
                        if t.triggers.len() != before {
                            writes.push((schema_key(&t.name), Some(schema_json(t))));
                        }
                    }
                }
                // the aggregates it is a state or other support function of: refused, or dropped with it (CASCADE)
                let using = userdefs::aggregates_using(v, &f)?;
                if !using.is_empty() {
                    let it = format!(
                        "function {}({})",
                        f.name,
                        f.args.iter().map(|a| type_named(&a.1)).collect::<Vec<_>>().join(",")
                    );
                    if !cascade {
                        let detail: Vec<String> = using.iter().map(|(_, a)| format!("{a} depends on {it}")).collect();
                        return Err(SqlError {
                            code: "2BP01",
                            message: format!("cannot drop {it} because other objects depend on it"),
                            info: Some(Box::new(Violation {
                                detail: Some(detail.join("\n")),
                                hint: Some("Use DROP ... CASCADE to drop the dependent objects too.".into()),
                                ..Violation::default()
                            })),
                        });
                    }
                    match &using[..] {
                        [(_, a)] => notice("NOTICE", format!("drop cascades to {a}")),
                        all => notice_detail(
                            "NOTICE",
                            format!("drop cascades to {} other objects", all.len()),
                            all.iter().map(|(_, a)| format!("drop cascades to {a}")).collect::<Vec<_>>().join("\n"),
                        ),
                    }
                    for (k, _) in using {
                        userdefs::forget(&k);
                        writes.push((k, None));
                    }
                }
                // the operator classes it is a support function of: refused, or dropped with it (CASCADE) where no
                // partition key hashes with them (a table is dropped by its own DROP, which this batch can't make)
                let classes = userdefs::opclasses_using(v, &f)?;
                if !classes.is_empty() {
                    let it = format!(
                        "function {}({})",
                        f.name,
                        f.args.iter().map(|a| type_named(&a.1)).collect::<Vec<_>>().join(",")
                    );
                    if !cascade {
                        let detail: Vec<String> = classes.iter().map(|(_, c)| format!("{c} depends on {it}")).collect();
                        return Err(SqlError {
                            code: "2BP01",
                            message: format!("cannot drop {it} because other objects depend on it"),
                            info: Some(Box::new(Violation {
                                detail: Some(detail.join("\n")),
                                hint: Some("Use DROP ... CASCADE to drop the dependent objects too.".into()),
                                ..Violation::default()
                            })),
                        });
                    }
                    for (k, c) in &classes {
                        if let Some(t) = userdefs::opclass_users(v, k)?.first() {
                            return err(
                                UNSUPPORTED,
                                format!("cannot drop {it} with CASCADE: table {} hashes its partition key with {c}; drop the table first", rel_part(t)),
                            );
                        }
                    }
                    for (k, c) in classes {
                        notice("NOTICE", format!("drop cascades to {c}"));
                        userdefs::forget(&k);
                        writes.push((k, None));
                    }
                }
                writes.push((f.key(), None));
            }
            Ok((writes, ()))
        })?;
        forget_parsed(); // (as CREATE FUNCTION does)
        Ok(Output::tag(match (procedure, routine) {
            (_, true) => "DROP ROUTINE",
            (true, _) => "DROP PROCEDURE",
            _ => "DROP FUNCTION",
        }))
    }

    /// CREATE TRIGGER on a view: INSTEAD OF row triggers and BEFORE or AFTER statement triggers, each kept as a
    /// rule per event (`name:event`, the trigger in it), which `rules` runs; the writes, after Postgres's checks.
    fn view_trigger(&self, v: &View, x: &Trigger, table: &str, replace: bool) -> Result<Vec<(String, Option<String>)>> {
        let rel = rel_part(table);
        if !x.instead && x.row {
            return Err(is_a("42809", rel, "view", "Views cannot have row-level BEFORE or AFTER triggers."));
        }
        if x.truncate {
            return Err(is_a("42809", rel, "view", "Views cannot have TRUNCATE triggers."));
        }
        if x.instead {
            if !x.row {
                return err(UNSUPPORTED, "INSTEAD OF triggers must be FOR EACH ROW");
            }
            if x.when.is_some() {
                return err(UNSUPPORTED, "INSTEAD OF triggers cannot have WHEN conditions");
            }
            if !x.of.is_empty() {
                return err(UNSUPPORTED, "INSTEAD OF triggers cannot have column lists");
            }
        }
        if x.old_table.is_some() || x.new_table.is_some() {
            return Err(is_a("42809", rel, "view", "Triggers on views cannot have transition tables."));
        }
        trigger_function(v, x)?;
        // one of the name already (any of its events)
        let mut old = vec![];
        for r in v.prefix(&rules::prefix_of(table))? {
            let k = r?.0;
            if k.rsplit('/').next().and_then(|n| n.rsplit_once(':')).is_some_and(|(n, _)| n == x.name) {
                old.push(k);
            }
        }
        if !old.is_empty() && !replace {
            return err("42710", format!("trigger \"{}\" for relation \"{rel}\" already exists", x.name));
        }
        let mut rows = vec![];
        for e in &x.events {
            let key = rules::key(table, &format!("{}:{}", x.name, e.name().to_lowercase()));
            let j = serde_json::json!({
                "event": e.name().to_lowercase(), "instead": x.instead, "cond": null, "actions": [], "enabled": true,
                "trigger": x.to_json(),
            });
            rows.push((key, Some(j.to_string())));
        }
        // (OR REPLACE: the events it no longer has go)
        for k in old {
            if !rows.iter().any(|(r, _)| *r == k) {
                rows.push((k, None));
            }
        }
        rules::note(table);
        Ok(rows)
    }

    pub(super) fn create_trigger(&self, x: &Trigger, table: &str, replace: bool) -> Result<Output> {
        self.db.batch_with(self.branch, |v| {
            if view_def(v, table)?.is_some() {
                return Ok((self.view_trigger(v, x, table, replace)?, ()));
            }
            let mut t = need_table(v, table)?;
            let rel = rel_part(table);
            let foreign = t.hier.foreign.is_some();
            let transition = x.old_table.is_some() || x.new_table.is_some();
            // what each kind of relation may have, in the order Postgres's CreateTrigger looks
            if x.instead {
                return Err(match foreign {
                    true => is_a("42809", rel, "foreign table", "Foreign tables cannot have INSTEAD OF triggers."),
                    false => is_a("42809", rel, "table", "Tables cannot have INSTEAD OF triggers."),
                });
            }
            if foreign && x.constraint.is_some() {
                return Err(is_a("42809", rel, "foreign table", "Foreign tables cannot have constraint triggers."));
            }
            // (the table a constraint trigger's FROM names has to be there)
            if let Some(f) = x.constraint.as_ref().and_then(|c| c.from.as_deref())
                && load(v, f)?.is_none()
                && view_def(v, f)?.is_none()
            {
                return err(NO_TABLE, format!("relation \"{}\" does not exist", rel_part(f)));
            }
            if t.hier.part.is_some() && x.row && transition {
                return Err(is_a(
                    UNSUPPORTED,
                    rel,
                    "partitioned table",
                    "ROW triggers with transition tables are not supported on partitioned tables.",
                ));
            }
            if !t.schema {
                return err(
                    "42809",
                    format!("table \"{table}\" has no schema: triggers go on tables made with CREATE TABLE"),
                );
            }
            if x.truncate && x.row {
                return err(UNSUPPORTED, "TRUNCATE FOR EACH ROW triggers are not supported");
            }
            if transition {
                // (a foreign table's statements' rows aren't here to give a transition table)
                if foreign {
                    return Err(is_a(
                        "42809",
                        rel,
                        "foreign table",
                        "Triggers on foreign tables cannot have transition tables.",
                    ));
                }
                if x.row && !t.hier.all_parents().is_empty() {
                    return err(
                        UNSUPPORTED,
                        match t.hier.of.is_some() {
                            true => "ROW triggers with transition tables are not supported on partitions",
                            false => "ROW triggers with transition tables are not supported on inheritance children",
                        },
                    );
                }
                if x.before {
                    return err("42P17", "transition table name can only be specified for an AFTER trigger");
                }
                if x.truncate {
                    return err(UNSUPPORTED, "TRUNCATE triggers with transition tables are not supported");
                }
                if x.events.len() != 1 {
                    return err(
                        UNSUPPORTED,
                        "transition tables cannot be specified for triggers with more than one event",
                    );
                }
                if !x.of.is_empty() {
                    return err(UNSUPPORTED, "transition tables cannot be specified for triggers with column lists");
                }
                let has = |e: Event| x.events.contains(&e);
                if x.old_table.is_some() && !has(Event::Delete) && !has(Event::Update) {
                    return err("42P17", "OLD TABLE can only be specified for a DELETE or UPDATE trigger");
                }
                if x.new_table.is_some() && !has(Event::Insert) && !has(Event::Update) {
                    return err("42P17", "NEW TABLE can only be specified for an INSERT or UPDATE trigger");
                }
                if x.old_table.is_some() && x.old_table == x.new_table {
                    return err("42P17", "OLD TABLE name and NEW TABLE name cannot be the same");
                }
            }
            if x.truncate {
                return err(UNSUPPORTED, "TRUNCATE triggers aren't supported");
            }
            trigger_function(v, x)?;
            let mut x = x.clone();
            for c in &x.of {
                match t.cols.iter().find(|col| col.name == *c) {
                    Some(col) if x.cols.contains(&col.id) => {
                        return err(DUP_COLUMN, format!("column \"{c}\" specified more than once"));
                    }
                    Some(col) => x.cols.push(col.id),
                    None => return err(NO_COLUMN, format!("column \"{c}\" of relation \"{rel}\" does not exist")),
                }
            }
            x.of.clear();
            let (made, made_row) = (x.name.clone(), x.row);
            match t.triggers.iter().position(|y| y.name == x.name) {
                // (a partition's copy of its parent's trigger is replaced from the parent only)
                Some(_) if replace && t.hier.of.is_some() && t.hier.inherited.contains(&format!("trigger:{made}")) => {
                    return err(
                        "42710",
                        format!("trigger \"{made}\" for relation \"{rel}\" is an internal or a child trigger"),
                    );
                }
                // (nor is a constraint trigger replaced)
                Some(i) if replace && t.triggers[i].constraint.is_some() => {
                    return err("42710", format!("trigger \"{made}\" for relation \"{rel}\" is a constraint trigger"));
                }
                Some(i) if replace => t.triggers[i] = x,
                Some(_) => {
                    return err("42710", format!("trigger \"{made}\" for relation \"{rel}\" already exists"));
                }
                None => t.triggers.push(x),
            }
            t.triggers.sort_by(|a, b| a.name.cmp(&b.name));
            let mut writes = vec![(schema_key(table), Some(schema_json(&t)))];
            // a row trigger of a partitioned table is each partition's too
            if t.hier.part.is_some()
                && made_row
                && let Some(trig) = t.triggers.iter().find(|y| y.name == made).cloned()
            {
                writes.extend(hier::spread_trigger(v, &t, &trig, replace)?);
            }
            Ok((writes, ()))
        })?;
        Ok(Output::tag("CREATE TRIGGER"))
    }

    pub(super) fn drop_trigger(&self, name: &str, table: &str, if_exists: bool) -> Result<Output> {
        self.db.batch_with(self.branch, |v| {
            // an INSTEAD OF trigger of a view: the rules it is kept as
            if view_def(v, table)?.is_some() {
                let mut gone = vec![];
                for r in v.prefix(&rules::prefix_of(table))? {
                    let k = r?.0;
                    if k.rsplit('/').next().and_then(|n| n.rsplit_once(':')).is_some_and(|(n, _)| n == name) {
                        gone.push((k, None));
                    }
                }
                if gone.is_empty() && !if_exists {
                    return err("42704", format!("trigger \"{name}\" for table \"{table}\" does not exist"));
                }
                return Ok((gone, ()));
            }
            let t = match load(v, table)? {
                Some(t) => Some(t),
                None if if_exists => {
                    // (Postgres names the table it didn't find, not the trigger)
                    notice("NOTICE", format!("relation \"{table}\" does not exist, skipping"));
                    return Ok((vec![], ()));
                }
                None => return err(NO_TABLE, format!("relation \"{table}\" does not exist")),
            };
            let Some(mut t) = t.filter(|t| t.triggers.iter().any(|x| x.name == name)) else {
                if if_exists {
                    notice("NOTICE", format!("trigger \"{name}\" for relation \"{table}\" does not exist, skipping"));
                    return Ok((vec![], ()));
                }
                return err("42704", format!("trigger \"{name}\" for table \"{table}\" does not exist"));
            };
            // a partition's trigger that is its parent's goes with the parent's
            if t.hier.of.is_some() && t.hier.inherited.iter().any(|x| *x == format!("trigger:{name}")) {
                let up = t.hier.of.as_ref().map_or("", |(p, _)| p.as_str());
                return Err(SqlError {
                    code: "2BP01",
                    message: format!(
                        "cannot drop trigger {name} on table {} because trigger {name} on table {} requires it",
                        rel_part(&t.name),
                        rel_part(up)
                    ),
                    info: Some(Box::new(Violation {
                        hint: Some(format!("You can drop trigger {name} on table {} instead.", rel_part(up))),
                        ..Violation::default()
                    })),
                });
            }
            t.triggers.retain(|x| x.name != name);
            let mut writes = vec![(schema_key(table), Some(schema_json(&t)))];
            writes.extend(hier::unspread_trigger(v, &t, name)?);
            Ok((writes, ()))
        })?;
        Ok(Output::tag("DROP TRIGGER"))
    }

    /// ALTER TRIGGER name ON table RENAME TO `to`: on a partitioned table, its partitions' copies of a row
    /// trigger too (not an inheritance child's own, as Postgres's renametrig has it).
    pub(super) fn rename_trigger(&self, name: &str, table: &str, to: &str) -> Result<Output> {
        self.db.batch_with(self.branch, |v| {
            // a view's: the rules it is kept as, one per event (see `view_trigger`)
            if view_def(v, table)?.is_some() {
                let mut writes = vec![];
                for r in v.prefix(&rules::prefix_of(table))? {
                    let (k, raw) = r?;
                    let Some((n, ev)) = k.rsplit('/').next().and_then(|n| n.rsplit_once(':')) else { continue };
                    let mut j: Json = serde_json::from_str(&raw).unwrap_or_default();
                    if !j["trigger"].is_object() {
                        continue;
                    }
                    if n == to && name != to {
                        return err(
                            "42710",
                            format!("trigger \"{to}\" for relation \"{}\" already exists", rel_part(table)),
                        );
                    }
                    if n != name {
                        continue;
                    }
                    j["trigger"]["name"] = Json::from(to);
                    let moved = rules::key(table, &format!("{to}:{ev}"));
                    writes.push((k.clone(), None));
                    writes.push((moved, Some(j.to_string())));
                }
                if writes.is_empty() {
                    return err(
                        "42704",
                        format!("trigger \"{name}\" for table \"{}\" does not exist", rel_part(table)),
                    );
                }
                if name == to {
                    return Ok((vec![], ()));
                }
                return Ok((writes, ()));
            }
            let mut t = need_table(v, table)?;
            if !t.triggers.iter().any(|x| x.name == name) {
                return err("42704", format!("trigger \"{name}\" for table \"{}\" does not exist", rel_part(table)));
            }
            // a partition's copy of its parent's trigger has its parent's name
            if let Some((up, _)) = &t.hier.of
                && t.hier.inherited.iter().any(|x| *x == format!("trigger:{name}"))
            {
                return Err(SqlError {
                    code: "XX000",
                    message: format!("cannot rename trigger \"{name}\" on table \"{}\"", rel_part(table)),
                    info: Some(Box::new(Violation {
                        hint: Some(format!(
                            "Rename the trigger on the partitioned table \"{}\" instead.",
                            rel_part(up)
                        )),
                        ..Violation::default()
                    })),
                });
            }
            if name == to {
                return Ok((vec![], ()));
            }
            if t.triggers.iter().any(|x| x.name == to) {
                return err("42710", format!("trigger \"{to}\" for relation \"{}\" already exists", rel_part(table)));
            }
            let row = t.triggers.iter().any(|x| x.name == name && x.row);
            for x in t.triggers.iter_mut().filter(|x| x.name == name) {
                x.name = to.to_string();
            }
            t.triggers.sort_by(|a, b| a.name.cmp(&b.name));
            let mut writes = vec![(schema_key(table), Some(schema_json(&t)))];
            if t.hier.part.is_some() && row {
                writes.extend(rename_copies(v, &t, name, to)?);
            }
            Ok((writes, ()))
        })?;
        Ok(Output::tag("ALTER TRIGGER"))
    }

    // ------------------------------------------------------------ firing triggers

    /// FOR EACH STATEMENT triggers of `t`, BEFORE or AFTER the statement.
    pub(super) fn fire_statement(&self, t: &Table, ev: Event, before: bool, set: &[u32]) -> Result<()> {
        if self.quiet.get() {
            return Ok(());
        }
        if before {
            self.transition.borrow_mut().remove(&t.name); // (a statement's rows are its own)
        } else {
            after_triggers_begin();
        }
        for x in firing(t, ev, before, false, set) {
            if self.when(t, x, ev, None, None)? {
                self.fire(t, x, ev, None, None)?;
            }
        }
        Ok(())
    }

    /// BEFORE ROW triggers of `t` for a row: the row to write, as they changed it, or None when
    /// one skips it (returns NULL). A DELETE's row is `old`, and stays as it is.
    pub(super) fn fire_before(
        &self,
        t: &Table,
        ev: Event,
        old: Option<&[Value]>,
        new: Option<Vec<Value>>,
        set: &[u32],
    ) -> Result<Option<Vec<Value>>> {
        let mut new = new;
        for x in firing(t, ev, true, true, set) {
            if !self.when(t, x, ev, old, new.as_deref())? {
                continue;
            }
            // (an UPDATE whose new row is the old one, skipped; any other row written as it is)
            if x.func == SUPPRESS_REDUNDANT && ev == Event::Update && !self.has_trigger_function(&x.func) {
                self.masked_trigger(t, x, ev)?;
                if old.is_some() && new.as_deref() == old {
                    return Ok(None);
                }
                continue;
            }
            let fired = match builtin_trigger(&x.func).filter(|_| !self.has_trigger_function(&x.func)) {
                Some(by_column) => self.tsvector_trigger(t, x, ev, new.as_deref(), set, by_column)?,
                None => self.fire(t, x, ev, old, new.as_deref())?,
            };
            match fired {
                None => return Ok(None),
                Some(r) if ev != Event::Delete => {
                    // a partition's copy of its parent's trigger may not send the row it inserts elsewhere
                    if ev == Event::Insert
                        && t.hier.of.is_some()
                        && t.hier.inherited.contains(&format!("trigger:{}", x.name))
                        && !self.partition_takes(&self.view()?, t, &r)?
                    {
                        return Err(SqlError {
                            code: UNSUPPORTED,
                            message: "moving row to another partition during a BEFORE FOR EACH ROW trigger is not \
                                      supported"
                                .into(),
                            info: Some(Box::new(Violation {
                                detail: Some(format!(
                                    "Before executing trigger \"{}\", the row was to be in partition \"{}.{}\".",
                                    x.name,
                                    schema_part(&t.name),
                                    rel_part(&t.name)
                                )),
                                ..Violation::default()
                            })),
                        });
                    }
                    new = Some(r)
                }
                Some(_) => {}
            }
        }
        Ok(Some(match new {
            Some(n) => n,
            None => old.unwrap_or_default().to_vec(),
        }))
    }

    /// The rows a statement changed in `t`, kept for the triggers of `t` that read them as transition tables.
    pub(super) fn keep_transition(&self, t: &Table, olds: &[Vec<Value>], news: &[Vec<Value>]) {
        if t.triggers.iter().any(|x| x.old_table.is_some() || x.new_table.is_some()) {
            let mut all = self.transition.borrow_mut();
            let kept = all.entry(t.name.clone()).or_default();
            kept.0.extend(olds.iter().cloned());
            kept.1.extend(news.iter().cloned());
        }
    }

    /// AFTER ROW triggers of `t` for the rows a statement changed: `olds` and `news`, either one
    /// empty for INSERT and DELETE.
    pub(super) fn fire_after(
        &self,
        t: &Table,
        ev: Event,
        olds: &[Vec<Value>],
        news: &[Vec<Value>],
        set: &[u32],
    ) -> Result<()> {
        // (kept for the triggers that read them as transition tables, statement triggers among them)
        self.keep_transition(t, olds, news);
        // a statement on a parent: its partition's or child's rows are the parent's, in the parent's columns
        let below = |(r, _, leaves): &Root| *r != t.name && leaves.contains(&t.name);
        if let Some((root, names, _)) = TRANSITION_ROOT.with_borrow(|r| r.clone()).filter(below) {
            let at: Vec<Option<usize>> = names.iter().map(|n| t.cols.iter().position(|c| c.name == *n)).collect();
            let conv = |r: &Vec<Value>| -> Vec<Value> {
                at.iter().map(|i| i.and_then(|i| r.get(i).cloned()).unwrap_or(Value::Null)).collect()
            };
            let mut all = self.transition.borrow_mut();
            let kept = all.entry(root).or_default();
            kept.0.extend(olds.iter().map(&conv));
            kept.1.extend(news.iter().map(&conv));
        }
        // a MERGE fires them once all its rows have changed, in the order they changed (see `Exec::merge_into`)
        if let Some(queue) = self.after_rows.borrow_mut().as_mut() {
            if !firing(t, ev, false, true, set).is_empty() {
                queue.push((t.clone(), ev, olds.to_vec(), news.to_vec(), set.to_vec()));
            }
            return Ok(());
        }
        self.fire_after_rows(t, ev, olds, news, set)
    }

    /// `fire_after`'s triggers, run now.
    pub(super) fn fire_after_rows(
        &self,
        t: &Table,
        ev: Event,
        olds: &[Vec<Value>],
        news: &[Vec<Value>],
        set: &[u32],
    ) -> Result<()> {
        after_triggers_begin();
        let xs = firing(t, ev, false, true, set);
        if xs.is_empty() {
            return Ok(());
        }
        // every WHEN is looked at first, as Postgres does as it queues the events, then the triggers fire in order
        let mut due: Vec<(usize, &Trigger)> = vec![];
        for i in 0..olds.len().max(news.len()) {
            let (old, new) = (olds.get(i).map(Vec::as_slice), news.get(i).map(Vec::as_slice));
            for x in &xs {
                if self.when(t, x, ev, old, new)? {
                    // a constraint trigger the transaction defers fires at its COMMIT
                    if super::trigger_defers(x) {
                        super::defer_trigger(&t.name, &x.name, ev, old, new);
                        continue;
                    }
                    due.push((i, *x));
                }
            }
        }
        for (i, x) in due {
            let (old, new) = (olds.get(i).map(Vec::as_slice), news.get(i).map(Vec::as_slice));
            self.fire(t, x, ev, old, new)?;
        }
        Ok(())
    }

    /// Where masks apply (an agent without 'unmask'), trigger `x` firing for an UPDATE or DELETE of `t`, which has
    /// masked columns, when it reads one of them as it is (see `trigger_reads_masked`): refused, as `mask::target`
    /// refuses a statement that would. An update's NEW holds the columns it didn't set as they were, and what the
    /// trigger does with them (copies them to a table of its own, raises them, acts on them) the agent could learn;
    /// an INSERT's NEW is the row the agent wrote.
    fn masked_trigger(&self, t: &Table, x: &Trigger, ev: Event) -> Result<()> {
        if ev == Event::Insert || !mask::on() {
            return Ok(());
        }
        let v = self.view()?;
        let masked = mask::of(&v, &t.name)?;
        if masked.is_empty() {
            return Ok(());
        }
        match trigger_reads_masked(&v, x, &masked)? {
            None => Ok(()),
            Some(what) => err(
                "42501",
                format!(
                    "trigger \"{}\" on table \"{}\" reads {what} of the rows it fires for as it is, and the table has \
                     masked columns: an agent without 'unmask' may not fire it with UPDATE or DELETE (have the trigger \
                     read only columns that aren't masked)",
                    x.name,
                    rel_part(&t.name)
                ),
            ),
        }
    }

    /// Whether trigger `x`'s WHEN holds for the row.
    fn when(&self, t: &Table, x: &Trigger, ev: Event, old: Option<&[Value]>, new: Option<&[Value]>) -> Result<bool> {
        let Some(w) = &x.when else { return Ok(true) };
        self.masked_trigger(t, x, ev)?;
        let e = check_expr(&whole_rows(w, t)?)?;
        let mut cols: Vec<Col> = t.cols.iter().map(|c| Col::new("new", &c.name, c.ty)).collect();
        cols.extend(t.cols.iter().map(|c| Col::new("old", &c.name, c.ty)));
        let n = t.cols.len();
        let mut row = new.map_or_else(|| vec![Value::Null; n], <[Value]>::to_vec);
        row.extend(old.map_or_else(|| vec![Value::Null; n], <[Value]>::to_vec));
        // NEW.tableoid and OLD.tableoid: the table's oid
        if w.contains("tableoid") && !t.cols.iter().any(|c| c.name == "tableoid") {
            let oid = hier::oids(&self.view()?, &[t.name.as_str()])?.first().copied().unwrap_or(0);
            for q in ["new", "old"] {
                let mut c = Col::new(q, "tableoid", Type::Oid);
                c.hidden = true;
                cols.push(c);
                row.push(Value::Int(oid));
            }
        }
        // (an hstore's or an ltree's operators are found by their operands' types: bound first)
        let e = match cols.iter().any(super::ext_col) || super::ext_marked(&e) {
            true => super::Binder::plain(&cols, None).bind(&e).unwrap_or(e),
            false => e,
        };
        Ok(truth(&eval(&e, &Env { cols: &cols, row: &row, params: &[], aggs: &[], sub: Some(self) })?)? == Some(true))
    }

    /// Whether a function `name` with no arguments that returns trigger was made (it is then the trigger's, whatever
    /// pg_catalog's built-in one of the name does).
    fn has_trigger_function(&self, name: &str) -> bool {
        self.functions(name).is_ok_and(|fs| fs.iter().any(|f| f.args.is_empty() && f.ret == Ret::Trigger))
    }

    /// `tsvector_update_trigger(tsvector_column, 'pg_catalog.english', text_column, ...)` and
    /// `tsvector_update_trigger_column(tsvector_column, config_column, text_column, ...)`, which Postgres has built in
    /// (tsvector_op.c): a BEFORE ROW trigger of INSERT or UPDATE that makes the tsvector column the words of the text
    /// columns, one document after another, and gives the row back. On UPDATE only when one of the text columns was
    /// assigned. Its errors are Postgres's, at the time the trigger fires.
    fn tsvector_trigger(
        &self,
        t: &Table,
        x: &Trigger,
        ev: Event,
        new: Option<&[Value]>,
        set: &[u32],
        by_column: bool,
    ) -> Result<Option<Vec<Value>>> {
        let internal =
            |m: &str| SqlError { code: "XX000", message: format!("tsvector_update_trigger: {m}"), info: None };
        if !x.row {
            return Err(internal("must be fired for row"));
        }
        if !x.before {
            return Err(internal("must be fired BEFORE event"));
        }
        let mut update_needed = match ev {
            Event::Insert => true,
            Event::Update => false,
            Event::Delete => return Err(internal("must be fired for INSERT or UPDATE")),
        };
        if x.args.len() < 3 {
            return Err(internal("arguments must be tsvector_field, ts_config, text_field1, ...)"));
        }
        let Some(mut row) = new.map(<[Value]>::to_vec) else { return Ok(None) };
        let column = |name: &str| t.cols.iter().position(|c| c.name == name && !c.hidden);
        let Some(target) = column(&x.args[0]) else {
            return err(NO_COLUMN, format!("tsvector column \"{}\" does not exist", x.args[0]));
        };
        if t.cols[target].ty != Type::TsVector {
            return err(WRONG_TYPE, format!("column \"{}\" is not of tsvector type", x.args[0]));
        }
        // the configuration: named (and schema-qualified, so that the result isn't search_path dependent), or the value
        // of a regconfig column
        let name = if by_column {
            let Some(at) = column(&x.args[1]) else {
                return err(NO_COLUMN, format!("configuration column \"{}\" does not exist", x.args[1]));
            };
            if t.cols[at].ty != Type::Reg(Reg::Config) {
                return err(WRONG_TYPE, format!("column \"{}\" is not of regconfig type", x.args[1]));
            }
            match &row[at] {
                Value::Null => {
                    return err("22004", format!("configuration column \"{}\" must not be null", x.args[1]));
                }
                Value::Reg(oid, _) => super::catalog::TS_CONFIGS
                    .iter()
                    .find(|(o, _)| o == oid)
                    .map_or_else(|| oid.to_string(), |(_, n)| (*n).to_string()),
                v => v.to_text(),
            }
        } else {
            if !x.args[1].contains('.') {
                return err(
                    "22023",
                    format!("text search configuration name \"{}\" must be schema-qualified", x.args[1]),
                );
            }
            x.args[1].clone()
        };
        let cfg = super::tsearch::config(&name).map_err(|_| SqlError {
            code: "42704",
            message: format!("text search configuration \"{name}\" does not exist"),
            info: None,
        })?;
        let (mut doc, mut pos) = (super::tsearch::TsVector::default(), 0usize);
        for n in &x.args[2..] {
            let Some(at) = column(n) else { return err(NO_COLUMN, format!("column \"{n}\" does not exist")) };
            if t.cols[at].ty != Type::Text {
                return err(WRONG_TYPE, format!("column \"{n}\" is not of a character type"));
            }
            if set.contains(&t.cols[at].id) {
                update_needed = true;
            }
            if let Value::Text(s) = &row[at] {
                super::tsearch::extend(cfg, s, &mut doc, &mut pos);
            }
        }
        if update_needed {
            row[target] = Value::Text(doc.to_string());
        }
        Ok(Some(row))
    }

    /// Runs trigger `x`'s function: the row it returns, None for NULL.
    pub(super) fn fire(
        &self,
        t: &Table,
        x: &Trigger,
        ev: Event,
        old: Option<&[Value]>,
        new: Option<&[Value]>,
    ) -> Result<Option<Vec<Value>>> {
        self.masked_trigger(t, x, ev)?;
        let f = self.functions(&x.func)?.iter().find(|f| f.args.is_empty() && f.ret == Ret::Trigger).cloned();
        // (a built-in trigger function fired as a statement's or after the row: Postgres's own refusals)
        if f.is_none() && x.func == SUPPRESS_REDUNDANT {
            let m = match (ev, x.before, x.row) {
                (Event::Update, true, true) => return Ok(new.map(<[Value]>::to_vec)),
                (Event::Update, true, false) => "must be called for each row",
                (Event::Update, false, _) => "must be called before update",
                _ => "must be called on update",
            };
            return err("39P01", format!("suppress_redundant_updates_trigger: {m}"));
        }
        if f.is_none() && builtin_trigger(&x.func).is_some() {
            let internal =
                |m: &str| SqlError { code: "XX000", message: format!("tsvector_update_trigger: {m}"), info: None };
            return Err(match (x.row, x.before) {
                (false, _) => internal("must be fired for row"),
                (true, false) => internal("must be fired BEFORE event"),
                (true, true) => internal("must be fired for INSERT or UPDATE"),
            });
        }
        let Some(f) = f else {
            return err(
                NO_FUNCTION,
                format!(
                    "function {}() does not exist (trigger \"{}\" on table \"{}\" calls it)",
                    x.func, x.name, t.name
                ),
            );
        };
        let trig = plpgsql::Trig { table: t, trigger: x, event: ev, old, new };
        // its transition tables, while it runs: the statement's rows under the names it gave them
        let named = [(&x.old_table, false), (&x.new_table, true)];
        let mut put = 0;
        for (name, new_rows) in named {
            let Some(name) = name else { continue };
            let rows = self.transition.borrow().get(&t.name).map(|(o, n)| if new_rows { n.clone() } else { o.clone() });
            let mut table = Table::default_named(name);
            table.cols = t.cols.iter().map(|c| Col { table: name.clone(), ..c.clone() }).collect();
            table.schema = true;
            table.mem = Some(Arc::new(rows.unwrap_or_default()));
            TRANSITION.with_borrow_mut(|ts| ts.push((name.clone(), table)));
            put += 1;
        }
        TRIGGER_DEPTH.set(TRIGGER_DEPTH.get() + 1);
        let returned = self.invoke(&f, vec![], Some(trig));
        TRIGGER_DEPTH.set(TRIGGER_DEPTH.get().saturating_sub(1));
        TRANSITION.with_borrow_mut(|ts| ts.truncate(ts.len() - put));
        match returned? {
            Returned::Record(None) | Returned::Value(Value::Null) => Ok(None),
            Returned::Record(Some(r)) if r.len() == t.cols.len() => {
                Ok(Some(r.into_iter().zip(&t.cols).map(|(v, c)| coerce(v, c.ty)).collect::<Result<_>>()?))
            }
            _ => err(
                "42804",
                format!("returned row structure does not match the structure of the triggering table \"{}\"", t.name),
            ),
        }
    }
}

/// The function `name` that arguments with these values choose, for a call made outside a query (a user aggregate's).
pub(super) fn resolve_for(name: &str, vals: &[Value]) -> Result<Arc<Function>> {
    let tys: Vec<Option<Type>> = vals.iter().map(value_type_loose).collect();
    resolve_typed(name, &tys)
}

/// `resolve_for`, the arguments' types given (a user aggregate's state is of its state type, though kept as text).
pub(super) fn resolve_typed(name: &str, tys: &[Option<Type>]) -> Result<Arc<Function>> {
    with_path_view(|v| pick(name, &named(v, name)?, tys))
        .unwrap_or_else(|| err(NO_FUNCTION, format!("function {} does not exist", written(name))))
}

/// `f` made for the types of a call: its polymorphic parameters and result (anyelement, anyarray, ...) become the
/// types the arguments give, as Postgres's enforce_generic_type_consistency works them out (see `poly_types`), an
/// argument of unknown type (a quoted literal, NULL) taking its parameter's; a function with none is itself. `sure`:
/// every argument's type is known (unknown meaning a literal's), so the errors Postgres gives are given; otherwise
/// (a type that couldn't be told, a user aggregate's state) what can't be worked out is text.
pub(super) fn instance(f: &Arc<Function>, given: &[Option<Type>], sure: bool) -> Result<Arc<Function>> {
    let poly = |t: &Type| matches!(t, Type::Poly(_));
    let rets: Vec<Type> = match &f.ret {
        Ret::Value(t) | Ret::SetOf(t) => vec![*t],
        Ret::Table(c) => c.iter().map(|(_, t)| *t).collect(),
        _ => vec![],
    };
    if !rets.iter().any(poly) && !f.args.iter().any(|a| poly(&a.1)) {
        return Ok(f.clone());
    }
    let decl: Vec<Type> = f.args.iter().map(|a| a.1).collect();
    let given = &given[..given.len().min(decl.len())];
    let found = match poly_types(&decl[..given.len()], given, &rets, true) {
        Ok(p) => p.unwrap_or_default(),
        Err(e) if sure => return Err(e),
        Err(_) => poly_types(&decl[..given.len()], given, &rets, false).ok().flatten().unwrap_or_default(),
    };
    let sub = |t: Type| found.sub(t);
    let ret = match &f.ret {
        Ret::Value(t) => Ret::Value(sub(*t)),
        Ret::SetOf(t) => Ret::SetOf(sub(*t)),
        Ret::Table(c) => Ret::Table(c.iter().map(|(n, t)| (n.clone(), sub(*t))).collect()),
        r => r.clone(),
    };
    let mut g = Function {
        name: f.name.clone(),
        args: f.args.iter().map(|(n, t)| (n.clone(), sub(*t))).collect(),
        declared: f.args.iter().map(|a| a.1).collect(),
        defaults: f.defaults.clone(),
        ret,
        lang: f.lang,
        volatility: f.volatility,
        strict: f.strict,
        body: f.body.clone(),
        procedure: f.procedure,
        oid: f.oid,
        definer: f.definer,
        outs: f.outs,
        single: f.single,
        variadic: f.variadic,
        cost: f.cost,
        std_body: f.std_body,
        config: f.config.clone(),
        tuning: f.tuning,
        std_form: f.std_form.clone(),
        type_notes: vec![],
        shapes: vec![],
        c_trigger: f.c_trigger.clone(),
        code: Code::Sql(vec![]),
    };
    g.compile()?;
    Ok(Arc::new(g))
}

/// The types a call's arguments make each polymorphic type: family 1's (anyelement and its array, range and
/// multirange) and family 2's (anycompatible's common type and its array, range and multirange). None: not known.
#[derive(Clone, Copy, Default)]
pub(super) struct PolyTypes {
    elem: Option<Type>,
    array: Option<Type>,
    range: Option<Type>,
    multi: Option<Type>,
    compat: Option<Type>,
    compat_array: Option<Type>,
    compat_range: Option<Type>,
    compat_multi: Option<Type>,
}

impl PolyTypes {
    /// Polymorphic type `t` made what the arguments make it; text (and its array) where nothing says what, as a call
    /// whose types couldn't all be told takes it. Another type is itself.
    fn sub(&self, t: Type) -> Type {
        let Type::Poly(k) = t else { return t };
        let elem = self.elem.unwrap_or(Type::Text);
        let compat = self.compat.unwrap_or(Type::Text);
        let found = match k {
            0 | 2 | 3 => self.elem,
            1 => self.array.or_else(|| array_type(elem)),
            // (a range's value, `[42,49)`, doesn't say its type: the range of the element type the other
            // arguments make)
            4 => self.range.or_else(|| self.elem.and_then(range_over)),
            8 => self.multi.or_else(|| self.range.or_else(|| self.elem.and_then(range_over)).and_then(range_multi)),
            5 | 10 => self.compat,
            6 => self.compat_array.or_else(|| array_type(compat)),
            7 => self.compat_range.or_else(|| self.compat.and_then(range_over)),
            9 => self.compat_multi.or_else(|| self.compat_range.and_then(range_multi)),
            _ => None,
        };
        found.unwrap_or(match k {
            1 => Type::Array(Type::Text.oid()),
            6 => Type::Array(compat.oid()),
            5 | 7 | 9 | 10 => compat,
            _ => elem,
        })
    }
}

/// The built-in range type over `sub` (int4range over integer).
fn range_over(sub: Type) -> Option<Type> {
    super::range::RANGES.iter().find(|r| r.2 == sub.oid()).map(|r| Type::Range(r.0))
}

/// An array type's element type (int2vector's smallint, oidvector's oid).
fn element_type(t: Type) -> Option<Type> {
    match t {
        Type::Array(e) => Type::from_oid(e),
        _ => None,
    }
}

/// The array type of `t` (none of an array or a pseudo-type).
fn array_type(t: Type) -> Option<Type> {
    match t {
        Type::Array(_) | Type::Poly(_) | Type::Void | Type::Record(_) => None,
        t => Some(Type::Array(t.oid())),
    }
}

/// A range type's subtype.
fn range_subtype(t: Type) -> Option<Type> {
    match t {
        Type::Range(r) => range::subtype(r),
        _ => None,
    }
}

/// A multirange type's range type.
fn multi_range(t: Type) -> Option<Type> {
    match t {
        Type::Multirange(m) => range::of_multi(m).map(Type::Range),
        _ => None,
    }
}

/// A range type's multirange type.
fn range_multi(t: Type) -> Option<Type> {
    match t {
        Type::Range(r) => range::multi_of(r).map(Type::Multirange),
        _ => None,
    }
}

/// The type arguments of these types convert to, as Postgres's select_common_type_from_oids chooses it: the first,
/// replaced by a later one of its category that it converts to implicitly (and not back), unless it is its
/// category's preferred type; None where two are of different categories, or one doesn't convert to it.
fn common_type(ts: &[Type]) -> Option<Type> {
    let (&first, rest) = ts.split_first()?;
    let mut c = first;
    for &t in rest {
        if t == c {
            continue;
        }
        let ((cc, cp), (tc, _)) = (category(Some(c)), category(Some(t)));
        if cc != tc {
            return None;
        }
        if !cp && fit(Some(c), t).is_some() && fit(Some(t), c).is_none() {
            c = t;
        }
    }
    ts.iter().all(|t| *t == c || fit(Some(*t), c).is_some()).then_some(c)
}

/// An error of a polymorphic call's types (42804), with a DETAIL.
fn poly_error(message: String, detail: Option<String>) -> SqlError {
    SqlError { code: "42804", message, info: Some(Box::new(Violation { detail, ..Violation::default() })) }
}

/// What a call with arguments of types `given` (None: unknown, as a quoted literal or NULL is) makes the polymorphic
/// parameters `decl` of a function whose results are of types `rets` (its return type, or its OUT parameters'), as
/// Postgres works it out: `enforce` as enforce_generic_type_consistency does, refusing arguments that don't agree with
/// its errors; otherwise as check_generic_type_consistency does, None for arguments that don't agree (the function
/// doesn't take them).
pub(super) fn poly_types(
    decl: &[Type],
    given: &[Option<Type>],
    rets: &[Type],
    enforce: bool,
) -> Result<Option<PolyTypes>> {
    macro_rules! fail {
        ($msg:expr) => {
            fail!($msg, None)
        };
        ($msg:expr, $detail:expr) => {{
            if enforce {
                return Err(poly_error($msg, $detail));
            }
            return Ok(None);
        }};
    }
    let name = |t: Type| t.pg_name().to_string();
    let versus = |a: Type, b: Type| Some(format!("{} versus {}", name(a), name(b)));
    let ret_has = |k: u8| rets.contains(&Type::Poly(k));
    let mut p = PolyTypes::default();
    let (mut have_nonarray, mut have_enum, mut have_multi) = (ret_has(2), ret_has(3), ret_has(8));
    let (mut have_compat_nonarray, mut have_compat_array) = (ret_has(10), ret_has(6));
    let (mut have_compat_range, mut have_compat_multi) = (ret_has(7), ret_has(9));
    let (mut n_poly, mut unknowns, mut any_compat) = (0usize, false, false);
    let mut compats: Vec<Type> = vec![];
    let mut compat_range_elem: Option<Type> = None;
    let mut compat_multi_range: Option<Type> = None;
    for (d, g) in decl.iter().zip(given) {
        let Type::Poly(k) = *d else { continue };
        // (an anyarray value, as pg_statistic's stavalues1 is, says no element type; another pseudo-type nothing)
        let g: Option<Type> = match (k, *g) {
            (1, Some(Type::Poly(1))) => Some(Type::Poly(1)),
            (_, Some(Type::Poly(_))) | (_, None) => None,
            (_, g) => g,
        };
        match k {
            0 | 2 | 3 => {
                n_poly += 1;
                have_nonarray |= k == 2;
                have_enum |= k == 3;
                let Some(g) = g else {
                    unknowns = true;
                    continue;
                };
                match p.elem {
                    Some(e) if e != g => {
                        fail!("arguments declared \"anyelement\" are not all alike".into(), versus(e, g))
                    }
                    _ => p.elem = Some(g),
                }
            }
            1 => {
                n_poly += 1;
                let Some(g) = g else {
                    unknowns = true;
                    continue;
                };
                match p.array {
                    Some(a) if a != g => {
                        fail!("arguments declared \"anyarray\" are not all alike".into(), versus(a, g))
                    }
                    _ => p.array = Some(g),
                }
            }
            4 => {
                n_poly += 1;
                let Some(g) = g else {
                    unknowns = true;
                    continue;
                };
                match p.range {
                    Some(r) if r != g => {
                        fail!("arguments declared \"anyrange\" are not all alike".into(), versus(r, g))
                    }
                    _ => p.range = Some(g),
                }
            }
            8 => {
                n_poly += 1;
                have_multi = true;
                let Some(g) = g else {
                    unknowns = true;
                    continue;
                };
                match p.multi {
                    Some(m) if m != g => {
                        fail!("arguments declared \"anymultirange\" are not all alike".into(), versus(m, g))
                    }
                    _ => p.multi = Some(g),
                }
            }
            5 | 10 => {
                any_compat = true;
                have_compat_nonarray |= k == 10;
                if let Some(g) = g {
                    compats.push(g);
                }
            }
            6 => {
                any_compat = true;
                have_compat_array = true;
                let Some(g) = g else { continue };
                let Some(e) = element_type(g) else {
                    fail!(format!("argument declared anycompatiblearray is not an array but type {}", name(g)))
                };
                compats.push(e);
            }
            7 => {
                any_compat = true;
                have_compat_range = true;
                let Some(g) = g else { continue };
                match p.compat_range {
                    Some(r) if r != g => {
                        fail!("arguments declared \"anycompatiblerange\" are not all alike".into(), versus(r, g))
                    }
                    Some(_) => {}
                    None => {
                        p.compat_range = Some(g);
                        let Some(e) = range_subtype(g) else {
                            fail!(format!(
                                "argument declared anycompatiblerange is not a range type but type {}",
                                name(g)
                            ))
                        };
                        compat_range_elem = Some(e);
                        compats.push(e);
                    }
                }
            }
            9 => {
                any_compat = true;
                have_compat_multi = true;
                let Some(g) = g else { continue };
                match p.compat_multi {
                    Some(m) if m != g => {
                        fail!("arguments declared \"anycompatiblemultirange\" are not all alike".into(), versus(m, g))
                    }
                    Some(_) => {}
                    None => {
                        p.compat_multi = Some(g);
                        let Some(r) = multi_range(g) else {
                            fail!(format!(
                                "argument declared anycompatiblemultirange is not a multirange type but type {}",
                                name(g)
                            ))
                        };
                        compat_multi_range = Some(r);
                    }
                }
            }
            _ => {}
        }
    }
    if n_poly == 0 && !any_compat {
        return Ok(Some(p));
    }
    if n_poly > 0 {
        if let Some(a) = p.array {
            let e = if a == Type::Poly(1) {
                // (anyarray given for anyarray: only where nothing else needs its element type)
                let family1 = |t: &Type| matches!(t, Type::Poly(0 | 2 | 3 | 4 | 8));
                if enforce && (n_poly != 1 || rets.iter().any(family1)) {
                    return Err(poly_error("cannot determine element type of \"anyarray\" argument".into(), None));
                }
                Type::Poly(0)
            } else {
                match element_type(a) {
                    Some(e) => e,
                    None => fail!(format!("argument declared anyarray is not an array but type {}", name(a))),
                }
            };
            match p.elem {
                None => p.elem = Some(e),
                Some(x) if x != e => fail!(
                    "argument declared anyarray is not consistent with argument declared anyelement".into(),
                    versus(a, x)
                ),
                _ => {}
            }
        }
        if let Some(m) = p.multi {
            let Some(r) = multi_range(m) else {
                fail!(format!("argument declared anymultirange is not a multirange type but type {}", name(m)))
            };
            match p.range {
                None => p.range = Some(r),
                Some(x) if x != r => fail!(
                    "argument declared anymultirange is not consistent with argument declared anyrange".into(),
                    versus(m, x)
                ),
                _ => {}
            }
        } else if have_multi {
            p.multi = p.range.and_then(range_multi);
        }
        if let Some(r) = p.range {
            let Some(e) = range_subtype(r) else {
                fail!(format!("argument declared anyrange is not a range type but type {}", name(r)))
            };
            match p.elem {
                None => p.elem = Some(e),
                Some(x) if x != e => fail!(
                    "argument declared anyrange is not consistent with argument declared anyelement".into(),
                    versus(r, x)
                ),
                _ => {}
            }
        }
        match p.elem {
            // (only family-1 arguments of unknown type)
            None if enforce => {
                return Err(poly_error(
                    "could not determine polymorphic type because input has type unknown".into(),
                    None,
                ));
            }
            None => {}
            Some(e) => {
                if have_nonarray && matches!(e, Type::Array(_)) {
                    fail!(format!("type matched to anynonarray is an array type: {}", name(e)));
                }
                if have_enum && e != Type::Poly(0) && !matches!(e, Type::Enum(_)) {
                    fail!(format!("type matched to anyenum is not an enum type: {}", name(e)));
                }
            }
        }
    }
    if any_compat {
        if let Some(m) = p.compat_multi {
            match p.compat_range {
                Some(r) if compat_multi_range != Some(r) => fail!(
                    "argument declared anycompatiblemultirange is not consistent with argument declared anycompatiblerange"
                        .into(),
                    versus(m, r)
                ),
                Some(_) => {}
                None => {
                    p.compat_range = compat_multi_range;
                    let Some(e) = compat_multi_range.and_then(range_subtype) else {
                        fail!(format!(
                            "argument declared anycompatiblemultirange is not a multirange type but type {}",
                            name(m)
                        ))
                    };
                    have_compat_range = true;
                    compat_range_elem = Some(e);
                    compats.push(e);
                }
            }
        } else if have_compat_multi {
            p.compat_multi = p.compat_range.and_then(range_multi);
        }
        if compats.is_empty() {
            // (only family-2 arguments of unknown type: text, but not a text range)
            if enforce && have_compat_range {
                return Err(poly_error(
                    "could not determine polymorphic type anycompatiblerange because input has type unknown".into(),
                    None,
                ));
            }
            if enforce && have_compat_multi {
                return Err(poly_error(
                    "could not determine polymorphic type anycompatiblemultirange because input has type unknown"
                        .into(),
                    None,
                ));
            }
            p.compat = Some(Type::Text);
            p.compat_array = array_type(Type::Text);
        } else {
            let Some(c) = common_type(&compats) else {
                fail!("arguments of anycompatible family cannot be cast to a common type".into())
            };
            p.compat = Some(c);
            if have_compat_array {
                p.compat_array = array_type(c);
            }
            if have_compat_nonarray && matches!(c, Type::Array(_)) {
                fail!(format!("type matched to anycompatiblenonarray is an array type: {}", name(c)));
            }
            if have_compat_range {
                let Some(r) = p.compat_range else {
                    fail!(
                        "could not determine polymorphic type anycompatiblerange because input has type unknown".into()
                    )
                };
                if compat_range_elem != Some(c) {
                    fail!(format!("anycompatiblerange type {} does not match anycompatible type {}", name(r), name(c)));
                }
            }
            if have_compat_multi {
                let Some(m) = p.compat_multi else {
                    fail!(
                        "could not determine polymorphic type anycompatiblemultirange because input has type unknown"
                            .into()
                    )
                };
                if compat_range_elem != Some(c) {
                    fail!(format!(
                        "anycompatiblemultirange type {} does not match anycompatible type {}",
                        name(m),
                        name(c)
                    ));
                }
            }
        }
    }
    // (a range or multirange of unknown type can't be worked out from the others)
    if enforce && unknowns {
        for (d, g) in decl.iter().zip(given) {
            match (d, g) {
                (Type::Poly(4), None) if p.range.is_none() => {
                    return Err(poly_error(
                        "could not determine polymorphic type anyrange because input has type unknown".into(),
                        None,
                    ));
                }
                (Type::Poly(8), None) if p.multi.is_none() => {
                    return Err(poly_error(
                        "could not determine polymorphic type anymultirange because input has type unknown".into(),
                        None,
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(Some(p))
}

/// Whether a function whose parameters taking a call's arguments are of these types (None: of any type) takes
/// arguments of the types `given` as far as its polymorphic ones go (check_generic_type_consistency).
fn poly_fits(params: &[Option<Type>], given: &[Option<Type>]) -> bool {
    if !params.iter().any(|p| matches!(p, Some(Type::Poly(_)))) {
        return true;
    }
    let decl: Vec<Type> = params.iter().map(|p| p.unwrap_or(Type::Void)).collect();
    matches!(poly_types(&decl, given, &[], false), Ok(Some(_)))
}

/// Why a function returning `ret` (or with OUT parameters of these types) and taking parameters `args` can't be
/// made: a polymorphic result no polymorphic parameter says the type of (check_valid_polymorphic_signature).
pub(super) fn poly_signature(rets: &[Type], args: &[Type]) -> Result<()> {
    let family1 = |t: &Type| matches!(t, Type::Poly(0 | 1 | 2 | 3 | 4 | 8));
    let family2 = |t: &Type| matches!(t, Type::Poly(5 | 6 | 7 | 9 | 10));
    for r in rets {
        let detail = match r {
            Type::Poly(4 | 8) if !args.iter().any(|a| matches!(a, Type::Poly(4 | 8))) => {
                format!("A result of type {} requires at least one input of type anyrange or anymultirange.", r.name())
            }
            Type::Poly(7 | 9) if !args.iter().any(|a| matches!(a, Type::Poly(7 | 9))) => format!(
                "A result of type {} requires at least one input of type anycompatiblerange or anycompatiblemultirange.",
                r.name()
            ),
            Type::Poly(4 | 8 | 7 | 9) => continue,
            t if family1(t) && !args.iter().any(family1) => format!(
                "A result of type {} requires at least one input of type anyelement, anyarray, anynonarray, anyenum, anyrange, or anymultirange.",
                r.name()
            ),
            t if family2(t) && !args.iter().any(family2) => format!(
                "A result of type {} requires at least one input of type anycompatible, anycompatiblearray, anycompatiblenonarray, anycompatiblerange, or anycompatiblemultirange.",
                r.name()
            ),
            _ => continue,
        };
        return Err(SqlError {
            code: "42P13",
            message: "cannot determine result data type".into(),
            info: Some(Box::new(Violation { detail: Some(detail), ..Violation::default() })),
        });
    }
    Ok(())
}

/// A composite type's fields (one made with CREATE TYPE ... AS (...)); None for another type.
pub(super) fn row_fields(t: Type) -> Option<Vec<(String, Type)>> {
    match t {
        Type::Record(_) => t.fields().filter(|f| !f.is_empty()).map(|f| f.to_vec()),
        _ => None,
    }
}

/// The SQL that runs `symbol` of C library `file`, for the functions of the regression tests' libraries that are here
/// (regress.so or regress.dylib, and contrib/spi's refint and autoinc, which triggers.sql loads), with the parameters
/// they take; None for any other.
fn regress_c(file: &str, symbol: &str, args: &[Type]) -> Option<(Lang, String)> {
    let base = file.rsplit('/').next().unwrap_or(file);
    let lib = base.split('.').next().unwrap_or(base);
    let sql = |s: &str| Some((Lang::Sql, s.to_string()));
    match (lib, symbol, args) {
        // the point where two paths first cross (see `geom`)
        ("regress", "interpt_pp", [Type::Path, Type::Path]) => sql("select __interpt_pp($1::path, $2::path)"),
        // an emp row whose salary is over 699 (regress.c's overpaid)
        ("regress", "overpaid", [_]) => sql("select $1.salary > 699"),
        // a name spelled backwards (regress.c's reverse_name)
        ("regress", "reverse_name", [_]) => sql("select reverse($1::text)::name"),
        // a trigger that gives back the row it fired on, unchanged: the new one for INSERT, else the old one
        // (regress.c's trigger_return_old, which returns tg_trigtuple)
        ("regress", "trigger_return_old", []) => {
            Some((Lang::PlPgSql, "begin if tg_op = 'INSERT' then return new; end if; return old; end".into()))
        }
        // a path as Postgres's canonicalize_path writes it (src/port/path.c: see `funcs::canonicalize_path`)
        ("regress", "test_canonicalize_path", [Type::Text]) => sql("select __canonicalize_path($1)"),
        // regress.c's encoding tests: its verification of each encoding's invalid string (a warning for each that
        // fails, none here), and a conversion and how many bytes of it were good (see `encoding`)
        ("regress", "test_enc_setup", []) => Some((
            Lang::PlPgSql,
            "declare w text := __test_enc_setup(); begin if w <> '' then raise warning '%', w; end if; end".into(),
        )),
        ("regress", "test_enc_conversion", [Type::Bytea, _, _, Type::Bool]) => {
            sql("select __test_enc_validlen($1, $2::text, $3::text, $4), __test_enc_result($1, $2::text, $3::text, $4)")
        }
        // whether a value of one type reads as another's with no conversion (IsBinaryCoercible: see
        // `catalog::binary_coercible`)
        ("regress", "binary_coercible", [Type::Oid, Type::Oid]) => sql("select __binary_coercible($1, $2)"),
        // ttdummy's switch, the session's, giving the state it was in (see `Exec::ttdummy`)
        ("regress", "set_ttdummy", [Type::Int]) => sql("select __set_ttdummy($1)"),
        // the trigger functions, run as their C does when they fire (see `Exec::c_trigger`)
        ("regress", "ttdummy", []) | ("autoinc", "autoinc", []) => Some((Lang::PlPgSql, c_trigger_body(symbol))),
        ("refint", "check_primary_key" | "check_foreign_key", []) => Some((Lang::PlPgSql, c_trigger_body(symbol))),
        _ => None,
    }
}

/// A trigger function in C of the regression tests' libraries that is here: regress.c's ttdummy, and contrib/spi's
/// autoinc (autoinc.c), check_primary_key and check_foreign_key (refint.c).
#[derive(Clone, Copy, PartialEq)]
enum CTrigger {
    TtDummy,
    AutoInc,
    CheckPrimaryKey,
    CheckForeignKey,
}

const C_TRIGGERS: [(&str, CTrigger); 4] = [
    ("ttdummy", CTrigger::TtDummy),
    ("autoinc", CTrigger::AutoInc),
    ("check_primary_key", CTrigger::CheckPrimaryKey),
    ("check_foreign_key", CTrigger::CheckForeignKey),
];

impl CTrigger {
    /// What its C's elog says called other than by the trigger manager (autoinc.c's alone names no function).
    fn not_fired(self) -> &'static str {
        match self {
            CTrigger::TtDummy => "ttdummy: not fired by trigger manager",
            CTrigger::AutoInc => "not fired by trigger manager",
            CTrigger::CheckPrimaryKey => "check_primary_key: not fired by trigger manager",
            CTrigger::CheckForeignKey => "check_foreign_key: not fired by trigger manager",
        }
    }
}

/// The PL/pgSQL body a function of C trigger `symbol` is kept with, which isn't run: the C is, failing as it does
/// called other than by a row trigger (see `Exec::run_code`). It doesn't say what the function is (a PL/pgSQL
/// function of the same body is one, run as it is written): what it was made as does (see `Function::c_trigger`).
fn c_trigger_body(symbol: &str) -> String {
    format!("begin raise exception '{symbol}: not fired by trigger manager'; end")
}

/// The C trigger function `f` was made as, if it is one (see `Function::c_trigger`).
fn c_trigger_of(f: &Function) -> Option<CTrigger> {
    let (_, symbol) = f.c_trigger.as_ref()?;
    C_TRIGGERS.iter().find(|(s, _)| s == symbol).map(|(_, c)| *c)
}

/// A C function's elog(ERROR, ...): an internal error, XX000.
fn elog(message: impl Into<String>) -> SqlError {
    SqlError { code: "XX000", message: message.into(), info: None }
}

/// ERRCODE_TRIGGERED_ACTION_EXCEPTION, 09000, which contrib/spi's triggers raise.
fn triggered_action(message: String, detail: Option<String>) -> SqlError {
    SqlError {
        code: "09000",
        message,
        info: detail.map(|d| Box::new(Violation { detail: Some(d), ..Violation::default() })),
    }
}

/// Column `name` of the trigger's table, as SPI_fnumber finds it: its place, None where there is none.
fn spi_fnumber(t: &Table, name: &str) -> Option<usize> {
    t.cols.iter().position(|c| c.name == name && !c.hidden)
}

/// refint's error for a key column the trigger's table hasn't.
fn no_attribute(name: &str, t: &Table) -> SqlError {
    SqlError {
        code: NO_COLUMN,
        message: format!("there is no attribute \"{name}\" in relation \"{}\"", rel_part(&t.name)),
        info: None,
    }
}

/// nextval of sequence `name` (as a text names one: `DirectFunctionCall1(nextval, ...)`), cut to an int4 as the C
/// functions take it.
fn nextval_int4(name: &str) -> Result<i64> {
    match coerce(ctx_call(CtxFn::NextVal, &[Value::Text(name.into())])?, Type::BigInt)? {
        Value::Int(n) => Ok(i64::from(n as i32)),
        _ => Ok(0),
    }
}

/// A text as quote_literal_cstr writes it: in single quotes, each doubled, and as an E'' string doubling each
/// backslash where it has one.
fn quote_literal_cstr(s: &str) -> String {
    match s.contains('\\') {
        true => format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
        false => format!("'{}'", s.replace('\'', "''")),
    }
}

/// SPI_prepare of `sql`, a C function's statement: parsed, one statement; an error out of that has the statement in
/// its CONTEXT (see `spi_error`).
fn spi_prepare(sql: &str) -> Result<Statement> {
    let parsed = super::parse(sql).and_then(|mut stmts| match stmts.pop().filter(|_| stmts.is_empty()) {
        Some(st) => Ok(st),
        None => err(SYNTAX, "a C function's statement is one statement"),
    });
    parsed.map_err(|e| spi_error(e, sql))
}

/// What SPI_prepare's parse analysis checks of `st`, a statement refint prepared, before anything runs it: that its
/// table is there (as running it finds one) and has each of the key's columns `cols` (each as the parser reads it
/// written alone: one written otherwise, and a view's, are left to running it), as Postgres's message says.
fn spi_analyze(v: &View, st: &Statement, cols: &[String]) -> Result<()> {
    let Some(name) = st.table_name() else { return Ok(()) };
    if load(v, name)?.is_none() && view_def(v, name)?.is_some() {
        return Ok(());
    }
    let Some(t) = st.table(v)? else { return Ok(()) };
    for c in cols.iter().filter_map(|c| folded_name(c)) {
        if !t.cols.iter().any(|x| x.name == c && !x.hidden) {
            return err(NO_COLUMN, format!("column \"{c}\" does not exist"));
        }
    }
    Ok(())
}

/// A name as the parser reads it written alone: a quoted one as it is between its quotes (each `""` a `"`), a plain
/// one in lower case (ASCII letters only, as downcase_identifier); None for anything else.
fn folded_name(s: &str) -> Option<String> {
    if let Some(q) = s.strip_prefix('"').and_then(|q| q.strip_suffix('"')).filter(|q| !q.is_empty()) {
        return Some(q.replace("\"\"", "\""));
    }
    let mut cs = s.chars();
    let starts = cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii());
    let plain = starts && cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii());
    plain.then(|| s.to_ascii_lowercase())
}

/// An error out of a statement a C function ran through SPI: with the statement in its CONTEXT, as SPI's error
/// callback says it, unless it points into the statement (a syntax error's position, kept as it is).
fn spi_error(e: SqlError, sql: &str) -> SqlError {
    match e.info.as_ref().is_some_and(|i| i.position.is_some()) {
        true => e,
        false => plpgsql::with_context(e, &format!("SQL statement \"{sql}\"")),
    }
}

/// Whether set_ttdummy(0) turned ttdummy off in this session (see `__set_ttdummy`).
fn ttdummy_off() -> bool {
    STOP.with_borrow(|s| s.as_ref().and_then(|s| s.ctx.clone())).is_some_and(|c| c.state.lock().unwrap().ttdummy_off)
}

impl Exec<'_> {
    /// Runs C trigger function `c` on the row trigger `tr` fired for, as its C does: the row it gives back, None
    /// for NULL (a BEFORE trigger's row skipped). As a C function's, its errors have no PL/pgSQL line in their
    /// CONTEXT, only the statement it ran through SPI that one came out of (see `spi`).
    fn c_trigger(&self, c: CTrigger, tr: &plpgsql::Trig) -> Result<Option<Vec<Value>>> {
        match c {
            CTrigger::TtDummy => self.ttdummy(tr),
            CTrigger::AutoInc => autoinc(tr),
            CTrigger::CheckPrimaryKey => self.check_primary_key(tr),
            CTrigger::CheckForeignKey => self.check_foreign_key(tr),
        }
    }

    /// Runs `sql` as a C function prepares a statement and runs it through SPI (see `spi_prepare` and `spi_execp`).
    fn spi(&self, sql: &str, vals: &[Value], types: &[Option<Type>]) -> Result<(Output, usize)> {
        self.spi_execp(&spi_prepare(sql)?, sql, vals, types)
    }

    /// Runs `st`, prepared from `sql`, as SPI_execp does: its `$1`, ... the values `vals`, of the types `types` (its
    /// sources typed by them too, as a SQL function's are by its parameters': see `run_sql_function`); an error out
    /// of it has the statement in its CONTEXT (see `spi_error`). Its output, and how many rows it read or wrote
    /// (SPI_processed).
    fn spi_execp(&self, st: &Statement, sql: &str, vals: &[Value], types: &[Option<Type>]) -> Result<(Output, usize)> {
        let declared: Vec<Type> = types.iter().map(|t| t.unwrap_or(Type::Text)).collect();
        let was = super::PARAM_TYPES.replace(declared);
        let ran = self.with_params(vals, types, self.readonly).run_body(&st.0, false);
        super::PARAM_TYPES.set(was);
        let out = ran.map_err(|e| spi_error(e, sql))?;
        let n = match st.0 {
            Stmt::Select(_) => out.rows.len(),
            _ => out.tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0),
        };
        Ok((out, n))
    }

    /// regress.c's ttdummy(start, stop), a BEFORE UPDATE OR DELETE row trigger keeping a table's past rows, each
    /// valid from its int4 `start` to its `stop` (999999 for the current one). While set_ttdummy has it on, an
    /// UPDATE of a current row inserts the new row, starting at nextval('ttdummy_seq'), and closes the old one there
    /// in its place; a DELETE of one inserts it closed there, and deletes it; an UPDATE or DELETE of a past row is
    /// skipped, and one changing `start` or `stop` refused. Off, rows pass as they are.
    fn ttdummy(&self, tr: &plpgsql::Trig) -> Result<Option<Vec<Value>>> {
        const INFINITY: i64 = 999_999;
        let (t, x) = (tr.table, tr.trigger);
        if !x.row {
            return Err(elog("ttdummy: must be fired for row"));
        }
        if !x.before {
            return Err(elog("ttdummy: must be fired before event"));
        }
        if tr.event == Event::Insert {
            return Err(elog("ttdummy: cannot process INSERT event"));
        }
        let trigtuple = tr.old.unwrap_or_default();
        let newtuple = if tr.event == Event::Update { tr.new } else { None };
        if ttdummy_off() {
            return Ok(Some(newtuple.unwrap_or(trigtuple).to_vec()));
        }
        let relname = rel_part(&t.name);
        if x.args.len() != 2 {
            return Err(elog(format!("ttdummy ({relname}): invalid (!= 2) number of arguments {}", x.args.len())));
        }
        let mut at = [0; 2];
        for (i, a) in x.args.iter().enumerate() {
            let Some(k) = spi_fnumber(t, a) else {
                return Err(elog(format!("ttdummy ({relname}): there is no attribute {a}")));
            };
            if t.cols[k].ty != Type::Int {
                return Err(elog(format!("ttdummy ({relname}): attribute {a} must be of integer type")));
            }
            at[i] = k;
        }
        let int = |row: &[Value], i: usize| -> Result<i64> {
            match coerce(row[at[i]].clone(), Type::BigInt)? {
                Value::Int(n) => Ok(n),
                _ => Err(elog(format!("ttdummy ({relname}): {} must be NOT NULL", x.args[i]))),
            }
        };
        let (oldon, oldoff) = (int(trigtuple, 0)?, int(trigtuple, 1)?);
        if let Some(new) = newtuple {
            let (newon, newoff) = (int(new, 0)?, int(new, 1)?);
            if oldon != newon || oldoff != newoff {
                return err(
                    UNSUPPORTED,
                    format!(
                        "ttdummy ({relname}): you cannot change {} and/or {} columns (use set_ttdummy)",
                        x.args[0], x.args[1]
                    ),
                );
            }
            if newoff != INFINITY {
                return Ok(None);
            }
        } else if oldoff != INFINITY {
            return Ok(None);
        }
        let stamp = nextval_int4("ttdummy_seq")?;
        // the row it inserts: the new one starting now, or the deleted one ending now
        let mut vals = newtuple.unwrap_or(trigtuple).to_vec();
        if newtuple.is_some() {
            vals[at[0]] = Value::Int(stamp);
            vals[at[1]] = Value::Int(INFINITY);
        } else {
            vals[at[1]] = Value::Int(stamp);
        }
        let shown: Vec<usize> = (0..t.cols.len()).filter(|&i| !t.cols[i].hidden).collect();
        let places: Vec<String> = (1..=shown.len()).map(|i| format!("${i}")).collect();
        let sql = format!("INSERT INTO {relname} VALUES ({})", places.join(", "));
        let params: Vec<Value> = shown.iter().map(|&i| vals[i].clone()).collect();
        let types: Vec<Option<Type>> = shown.iter().map(|&i| Some(t.cols[i].ty)).collect();
        self.spi(&sql, &params, &types)?;
        let mut kept = trigtuple.to_vec();
        if newtuple.is_some() {
            kept[at[1]] = Value::Int(stamp);
        }
        Ok(Some(kept))
    }

    /// refint.c's check_primary_key(fkcol, ..., pktable, pkcol, ...), a row trigger of INSERT or UPDATE: the row's
    /// key (unless a column of it is NULL) has to be one `pktable` has, else 09000.
    fn check_primary_key(&self, tr: &plpgsql::Trig) -> Result<Option<Vec<Value>>> {
        let (t, x) = (tr.table, tr.trigger);
        if !x.row {
            return Err(elog("check_primary_key: must be fired for row"));
        }
        if tr.event == Event::Delete {
            return Err(elog("check_primary_key: cannot process DELETE events"));
        }
        let tuple = tr.new.unwrap_or_default();
        let args = &x.args;
        if args.len() % 2 != 1 {
            return Err(elog("check_primary_key: odd number of arguments should be specified"));
        }
        let nkeys = args.len() / 2;
        let relname = &args[nkeys];
        let (mut vals, mut types) = (vec![], vec![]);
        for a in &args[..nkeys] {
            let Some(k) = spi_fnumber(t, a) else { return Err(no_attribute(a, t)) };
            if tuple[k] == Value::Null {
                return Ok(Some(tuple.to_vec()));
            }
            vals.push(tuple[k].clone());
            types.push(Some(t.cols[k].ty));
        }
        let mut sql = format!("select 1 from {relname} where ");
        for i in 1..=nkeys {
            sql.push_str(&format!("{} = ${i} ", args[i + nkeys]));
            if i < nkeys {
                sql.push_str("and ");
            }
        }
        let (_, found) = self.spi(&sql, &vals, &types)?;
        if found == 0 {
            return Err(triggered_action(
                "tuple references non-existent key".into(),
                Some(format!("Trigger \"{}\" found tuple referencing non-existent key in \"{relname}\".", x.name)),
            ));
        }
        Ok(Some(tuple.to_vec()))
    }

    /// refint.c's check_foreign_key(nrefs, action, pkcol, ..., fktable, fkcol, ..., ...), a row trigger of DELETE or
    /// UPDATE of a referenced table: where the row's key (none of it NULL; for an UPDATE, one that changed) is one
    /// the `nrefs` referencing tables have, `restrict` refuses it (09000), `cascade` deletes their rows (for an
    /// UPDATE gives them the new key) and `setnull` sets their key to NULL, each saying how many rows it changed.
    fn check_foreign_key(&self, tr: &plpgsql::Trig) -> Result<Option<Vec<Value>>> {
        let (t, x) = (tr.table, tr.trigger);
        if !x.row {
            return Err(elog("check_foreign_key: must be fired for row"));
        }
        if tr.event == Event::Insert {
            return Err(elog("check_foreign_key: cannot process INSERT events"));
        }
        let trigtuple = tr.old.unwrap_or_default();
        let newtuple = if tr.event == Event::Update { tr.new } else { None };
        let given = || Ok(Some(newtuple.unwrap_or(trigtuple).to_vec()));
        let args = &x.args;
        if args.len() < 5 {
            return Err(elog(format!("check_foreign_key: too short {} (< 5) list of arguments", args.len())));
        }
        let nrefs = match coerce(Value::Text(args[0].clone()), Type::Int)? {
            Value::Int(n) => n,
            _ => 0,
        };
        if nrefs < 1 {
            return Err(elog(format!("check_foreign_key: {nrefs} (< 1) number of references specified")));
        }
        let action = args[1].chars().next().map_or(' ', |c| c.to_ascii_lowercase());
        if !matches!(action, 'r' | 'c' | 's') {
            return Err(elog(format!("check_foreign_key: invalid action {}", args[1])));
        }
        let (rest, nrefs) = (&args[2..], nrefs as usize);
        let nkeys = rest.len().saturating_sub(nrefs) / (nrefs + 1);
        if nkeys == 0 || rest.len() != nrefs + nkeys * (nrefs + 1) {
            return Err(elog(format!(
                "check_foreign_key: invalid number of arguments {} for {nrefs} references",
                args.len()
            )));
        }
        let (mut vals, mut types, mut places, mut same) = (vec![], vec![], vec![], true);
        for a in &rest[..nkeys] {
            let Some(k) = spi_fnumber(t, a) else { return Err(no_attribute(a, t)) };
            if trigtuple[k] == Value::Null {
                return given();
            }
            // (an UPDATE's key compared as text, as SPI_getvalue writes each)
            if let Some(new) = newtuple
                && (new[k] == Value::Null || new[k].to_text() != trigtuple[k].to_text())
            {
                same = false;
            }
            vals.push(trigtuple[k].clone());
            types.push(Some(t.cols[k].ty));
            places.push(k);
        }
        // a statement for each referencing table, prepared (as SPI_prepare does: parsed, and its table and columns
        // looked up) before it is known whether there is anything to do
        let v = self.view()?;
        let mut stmts = vec![];
        for r in rest[nkeys..].chunks(nkeys + 1) {
            let (relname, cols) = (&r[0], &r[1..]);
            let mut sql = match (action, newtuple) {
                ('r', _) => format!("select 1 from {relname} where "),
                ('c', Some(new)) => {
                    let sets: Vec<String> = (cols.iter().zip(&places))
                        .map(|(c, &k)| match &new[k] {
                            Value::Null => format!(" {c} = NULL "),
                            v => format!(" {c} = {} ", quote_literal_cstr(&v.to_text())),
                        })
                        .collect();
                    format!("update {relname} set {} where ", sets.join(", "))
                }
                ('c', None) => format!("delete from {relname} where "),
                _ => {
                    let sets: Vec<String> = cols.iter().map(|c| format!("{c} = null")).collect();
                    format!("update {relname} set {} where ", sets.join(", "))
                }
            };
            for (i, c) in cols.iter().enumerate() {
                sql.push_str(&format!("{c} = ${} ", i + 1));
                if i + 1 < nkeys {
                    sql.push_str("and ");
                }
            }
            let st = spi_prepare(&sql)?;
            spi_analyze(&v, &st, cols).map_err(|e| spi_error(e, &sql))?;
            stmts.push((relname, sql, st));
        }
        drop(v);
        if newtuple.is_some() && same {
            return given();
        }
        for (relname, sql, st) in stmts {
            let (_, n) = self.spi_execp(&st, &sql, &vals, &types)?;
            if action == 'r' {
                if n > 0 {
                    return Err(triggered_action(
                        format!("\"{}\": tuple is referenced in \"{relname}\"", x.name),
                        None,
                    ));
                }
            } else {
                // (refint.so is built with REFINT_VERBOSE for the regression tests)
                let done = if action == 'c' { "deleted" } else { "set to null" };
                notice("NOTICE", format!("{}: {n} tuple(s) of {relname} are {done}", x.name));
            }
        }
        given()
    }
}

/// autoinc.c's autoinc(column, sequence, ...), a BEFORE INSERT OR UPDATE row trigger: each int4 `column` that is
/// NULL or 0 is set to nextval of its `sequence` (the next one again where that gives 0).
fn autoinc(tr: &plpgsql::Trig) -> Result<Option<Vec<Value>>> {
    let (t, x) = (tr.table, tr.trigger);
    if !x.row {
        return Err(elog("must be fired for row"));
    }
    if !x.before {
        return Err(elog("must be fired before event"));
    }
    if tr.event == Event::Delete {
        return Err(elog("cannot process DELETE events"));
    }
    let mut row = tr.new.unwrap_or_default().to_vec();
    let relname = rel_part(&t.name);
    if x.args.is_empty() || x.args.len() % 2 != 0 {
        return Err(elog(format!("autoinc ({relname}): even number gt 0 of arguments was expected")));
    }
    for pair in x.args.chunks(2) {
        let (col, seq) = (&pair[0], &pair[1]);
        let Some(k) = spi_fnumber(t, col) else {
            return Err(triggered_action(format!("\"{relname}\" has no attribute \"{col}\""), None));
        };
        if t.cols[k].ty != Type::Int {
            return Err(triggered_action(format!("attribute \"{col}\" of \"{relname}\" must be type INT4"), None));
        }
        if !matches!(row[k], Value::Null | Value::Int(0)) {
            continue;
        }
        let mut n = nextval_int4(seq)?;
        if n == 0 {
            n = nextval_int4(seq)?;
        }
        row[k] = Value::Int(n);
    }
    Ok(Some(row))
}
