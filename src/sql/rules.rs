//! CREATE RULE: `ON INSERT | UPDATE | DELETE TO table [WHERE cond] DO [ALSO | INSTEAD] NOTHING | command | (commands)`.
//! A rule is kept as a row under `_sonos_defs/rule/` (so it forks and merges with the data). When a statement
//! changes a table that has rules, its rows are worked out first (NEW for an INSERT, OLD and NEW for an UPDATE, OLD
//! for a DELETE), and each rule's commands run once for each row with `NEW.col` and `OLD.col` written as the row's
//! values; an INSTEAD rule takes the place of the statement for the rows it holds for.

use super::*;
use std::collections::HashSet;
use std::sync::Mutex;

const PREFIX: &str = "_sonos_defs/rule/";

static TABLES: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub(super) fn note(table: &str) {
    TABLES.lock().unwrap().get_or_insert_with(HashSet::new).insert(table.to_string());
}

fn active(table: &str) -> bool {
    TABLES.lock().unwrap().as_ref().is_some_and(|t| t.contains(table))
}

thread_local! {
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug)]
pub(super) struct Rule {
    #[allow(dead_code)] // (kept as stored; rules are found by their table and event)
    pub name: String,
    pub event: String,
    pub instead: bool,
    pub cond: Option<String>,
    pub actions: Vec<String>,
    pub enabled: bool,
    /// an INSTEAD OF trigger of a view, kept as a rule: its trigger, which runs for each row
    pub trigger: Option<Json>,
}

/// The prefix of the keys of a table's rules.
pub(super) fn prefix_of(table: &str) -> String {
    format!("{PREFIX}{table}/")
}

pub(super) fn key(table: &str, name: &str) -> String {
    format!("{PREFIX}{table}/{name}")
}

/// The relation a rule's key (see `key`) is on.
pub(super) fn table_of(key: &str) -> Option<&str> {
    key.strip_prefix(PREFIX)?.rsplit_once('/').map(|(t, _)| t)
}

fn rules_of(db: &Db, branch: &str, table: &str) -> Result<Vec<Rule>> {
    let v = db.view_tx(branch)?;
    let mut out = vec![];
    for r in v.prefix(&format!("{PREFIX}{table}/"))? {
        let (k, raw) = r?;
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        out.push(Rule {
            name: k[PREFIX.len() + table.len() + 1..].to_string(),
            event: j["event"].as_str().unwrap_or_default().to_string(),
            instead: j["instead"] == true,
            cond: j["cond"].as_str().map(String::from),
            actions: j["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(String::from))
                .collect(),
            enabled: j["enabled"] != false,
            trigger: j.get("trigger").filter(|t| t.is_object()).cloned(),
        });
    }
    Ok(out)
}

impl Parser {
    /// After CREATE [OR REPLACE] RULE.
    pub(super) fn create_rule(&mut self, replace: bool) -> Result<Stmt> {
        let name = self.name()?;
        self.expect_kw("as")?;
        self.expect_kw("on")?;
        let event = if self.kw("select") {
            "select"
        } else if self.kw("insert") {
            "insert"
        } else if self.kw("update") {
            "update"
        } else {
            self.expect_kw("delete")?;
            "delete"
        };
        self.expect_kw("to")?;
        let table = self.qualified(false, Kind::Rel)?;
        let mut cond = None;
        if self.kw("where") {
            let (start, toks) = (self.i, self.ahead(false));
            self.expr()?;
            cond = Some(tokens_shown(self.since(&toks, start)?));
        }
        self.expect_kw("do")?;
        let instead = self.kw("instead");
        if !instead {
            self.kw("also");
        }
        let mut actions = vec![];
        if self.kw("nothing") {
        } else if self.sym("(") {
            loop {
                while self.sym(";") {}
                if self.sym(")") {
                    break;
                }
                actions.push(self.rule_action()?);
            }
        } else {
            actions.push(self.rule_action()?);
        }
        if event == "select" {
            // (the view rule only, as Postgres has it)
            if actions.len() > 1 {
                return err("0A000", "multiple actions for rules on SELECT are not implemented");
            }
            if cond.is_some() {
                return err("0A000", "event qualifications are not implemented for rules on SELECT");
            }
            if !instead || actions.is_empty() {
                return err("42P17", "rules on SELECT must have action INSTEAD SELECT");
            }
            if !name.eq_ignore_ascii_case("_RETURN") {
                return err("42P17", format!("view rule for \"{}\" must be named \"_RETURN\"", rel_part(&table)));
            }
            let mut p = Parser::new(lex(&actions[0])?);
            let Stmt::Select(query) = p.stmt()? else {
                return err("42P17", "rules on SELECT must have action INSTEAD SELECT");
            };
            let view = Stmt::CreateView {
                name: table.clone(),
                cols: vec![],
                query,
                sql: actions[0].clone(),
                // (CREATE OR REPLACE RULE "_RETURN": the view's query replaced, as CREATE OR REPLACE VIEW does)
                replace,
                materialized: false,
                data: true,
                if_not_exists: false,
                check: None,
                invoker: false,
                barrier: false,
            };
            return Ok(Stmt::TableToView { table, view: Box::new(view) });
        }
        // RETURNING: in one action at most, of an unconditional INSTEAD rule (as DefineQueryRewrite has it)
        match actions.iter().filter(|a| returns_rows(a)).count() {
            0 => {}
            _ if cond.is_some() => return err(UNSUPPORTED, "RETURNING lists are not supported in conditional rules"),
            _ if !instead => return err(UNSUPPORTED, "RETURNING lists are not supported in non-INSTEAD rules"),
            1 => {}
            _ => return err(UNSUPPORTED, "cannot have multiple RETURNING lists in a rule"),
        }
        if name == "_RETURN" {
            return err("42P17", format!("non-view rule for \"{}\" must not be named \"_RETURN\"", rel_part(&table)));
        }
        let json = serde_json::json!({
            "event": event, "instead": instead, "cond": cond, "actions": actions, "enabled": true, "replace": replace,
        });
        Ok(Stmt::UserDef {
            needs: None,
            key: key(&table, &name),
            json: Some(json.to_string()),
            tag: "CREATE RULE",
            missing: None,
            exists: Some(format!("rule \"{name}\" for relation \"{}\" already exists", rel_part(&table))),
        })
    }

    /// After ALTER RULE: `name ON table RENAME TO new`.
    pub(super) fn alter_rule(&mut self) -> Result<Stmt> {
        let name = self.name()?;
        self.expect_kw("on")?;
        let table = self.qualified(false, Kind::Rel)?;
        if !self.kw("rename") {
            return self.fail("expected RENAME TO");
        }
        self.expect_kw("to")?;
        let to = self.name()?;
        Ok(Stmt::RenameRule { name, table, to })
    }

    /// One command of a rule, as SQL text.
    fn rule_action(&mut self) -> Result<String> {
        let (start, toks) = (self.i, self.ahead(false));
        // (checked as a statement; run as text, with NEW and OLD filled in)
        self.stmt()?;
        Ok(tokens_shown(self.since(&toks, start)?))
    }

    /// After DROP RULE: `[IF EXISTS] name ON table [CASCADE | RESTRICT]`.
    pub(super) fn drop_rule(&mut self) -> Result<Stmt> {
        let if_exists = self.kw("if");
        if if_exists {
            self.expect_kw("exists")?;
        }
        let name = self.name()?;
        self.expect_kw("on")?;
        let table = self.qualified(false, Kind::Rel)?;
        self.cascade();
        let missing =
            (!if_exists).then(|| format!("rule \"{name}\" for relation \"{}\" does not exist", rel_part(&table)));
        Ok(Stmt::UserDef { needs: None, key: key(&table, &name), json: None, tag: "DROP RULE", missing, exists: None })
    }
}

/// Whether a rule's command (SQL text) has a RETURNING list.
fn returns_rows(sql: &str) -> bool {
    matches!(
        parse(sql).ok().and_then(|v| v.into_iter().next()).map(|st| st.0),
        Some(
            Stmt::Insert { returning: Some(_), .. }
                | Stmt::Update { returning: Some(_), .. }
                | Stmt::Delete { returning: Some(_), .. }
        )
    )
}

/// The table an INSERT, UPDATE or DELETE changes, and its event.
fn target(s: &Stmt) -> Option<(&str, &'static str)> {
    match s {
        Stmt::Insert { table, .. } => Some((table, "insert")),
        Stmt::Update { table, from: None, .. } => Some((table, "update")),
        Stmt::Delete { table, from: None, .. } => Some((table, "delete")),
        _ => None,
    }
}

/// What COPY (INSERT | UPDATE | DELETE ...) TO refuses of its table's rules, as Postgres does and in
/// its words: the statement must stay one statement, and one that gives rows.
pub(super) fn copy_check(db: &Db, branch: &str, s: &Stmt, returns: bool) -> Result<()> {
    let Some((table, event)) = target(s).filter(|(t, _)| active(t)) else { return Ok(()) };
    let rules: Vec<Rule> = rules_of(db, branch, table)?
        .into_iter()
        .filter(|r| r.event == event && r.enabled && r.trigger.is_none())
        .collect();
    if rules.is_empty() {
        return Ok(());
    }
    let gives_rows = |sql: &String| {
        matches!(
            parse(sql).ok().and_then(|v| v.into_iter().next()).map(|st| st.0),
            Some(
                Stmt::Insert { returning: Some(_), .. }
                    | Stmt::Update { returning: Some(_), .. }
                    | Stmt::Delete { returning: Some(_), .. }
            )
        )
    };
    // a RETURNING needs an INSTEAD rule that has one too: the rewriter's error, before COPY's own
    if returns
        && rules.iter().any(|r| r.instead)
        && !rules.iter().any(|r| r.instead && r.cond.is_none() && r.actions.iter().any(gives_rows))
    {
        let ev = event.to_uppercase();
        return Err(SqlError {
            code: UNSUPPORTED,
            message: format!("cannot perform {ev} RETURNING on relation \"{}\"", rel_part(table)),
            info: Some(Box::new(Violation {
                hint: Some(format!("You need an unconditional ON {ev} DO INSTEAD rule with a RETURNING clause.")),
                ..Violation::default()
            })),
        });
    }
    let refuse = |what: &str| err(UNSUPPORTED, format!("{what} rules are not supported for COPY"));
    for r in &rules {
        if r.instead && r.cond.is_some() {
            return refuse("conditional DO INSTEAD");
        }
        if !r.instead {
            return refuse("DO ALSO");
        }
    }
    let actions: Vec<&String> = rules.iter().flat_map(|r| &r.actions).collect();
    match actions.as_slice() {
        [] => refuse("DO INSTEAD NOTHING"),
        [one] => match parse(one).ok().and_then(|v| v.into_iter().next()).map(|st| st.0) {
            Some(
                Stmt::Insert { returning: Some(_), .. }
                | Stmt::Update { returning: Some(_), .. }
                | Stmt::Delete { returning: Some(_), .. },
            )
            | None => Ok(()),
            Some(Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. } | Stmt::Select(_)) => {
                err(UNSUPPORTED, "COPY query must have a RETURNING clause")
            }
            Some(_) => err(UNSUPPORTED, "COPY query must not be a utility command"),
        },
        _ => refuse("multi-statement DO INSTEAD"),
    }
}

/// The data-changing WITH queries of `w` as the rules of their tables make them, as Postgres's rewriter does: one
/// unconditional DO INSTEAD rule of one command takes the query's place; any other rule is refused, in its words. None
/// when no rule applies.
pub(super) fn with_rules(db: &Db, branch: &str, w: &WithDml) -> Result<Option<WithDml>> {
    let mut out: Option<WithDml> = None;
    for (k, (_, st)) in w.dml.iter().enumerate() {
        let Some((table, event)) = target(st).filter(|(t, _)| active(t)) else { continue };
        let rules: Vec<Rule> = rules_of(db, branch, table)?
            .into_iter()
            .filter(|r| r.event == event && r.enabled && r.trigger.is_none())
            .collect();
        if rules.is_empty() {
            continue;
        }
        let refuse = |what: &str| {
            err(UNSUPPORTED, format!("{what} rules are not supported for data-modifying statements in WITH"))
        };
        // what the rewrite gives: the statement itself unless an unconditional INSTEAD rule replaces it, and every
        // rule's commands
        let replaced = rules.iter().any(|r| r.instead && r.cond.is_none());
        let actions: Vec<&String> = rules.iter().flat_map(|r| &r.actions).collect();
        // (a command that does the same to the same table sets the rule off again: Postgres's rewriter finds that
        // loop before it looks at what WITH can take)
        let bare = super::rel_part(table);
        for a in &actions {
            let toks = lex(a)?;
            let word = |i: usize| match toks.get(i) {
                Some(Tok::Word(w) | Tok::Ident(w)) => w.as_str(),
                _ => "",
            };
            let named = |i: usize| word(i) == bare || (word(i + 2) == bare && toks.get(i + 1) == Some(&Tok::Sym(".")));
            let again = match (word(0), word(1)) {
                ("insert", "into") => named(2) && event == "insert",
                ("update", _) => named(1) && event == "update",
                ("delete", "from") => named(2) && event == "delete",
                _ => false,
            };
            if again {
                return err("42P17", format!("infinite recursion detected in rules for relation \"{bare}\""));
            }
        }
        let n = usize::from(!replaced) + actions.len();
        if n == 0 {
            return refuse("DO INSTEAD NOTHING");
        }
        if n > 1 {
            for r in &rules {
                if r.instead && r.cond.is_some() && !r.actions.is_empty() {
                    return refuse("conditional DO INSTEAD");
                }
                if !r.instead && !r.actions.is_empty() {
                    return refuse("DO ALSO");
                }
            }
            return refuse("multi-statement DO INSTEAD");
        }
        if !replaced {
            // (rules without commands: DO ALSO NOTHING leaves the statement as it is; a conditional DO INSTEAD
            // NOTHING would hold back the rows its condition holds for)
            if rules.iter().any(|r| r.instead) {
                return err(
                    UNSUPPORTED,
                    "conditional DO INSTEAD NOTHING rules aren't supported for data-modifying statements in WITH yet",
                );
            }
            continue;
        }
        let action = actions[0];
        let toks = lex(action)?;
        if toks.windows(2).any(|p| matches!(p, [Tok::Word(x), Tok::Sym(".")] if x == "new" || x == "old")) {
            return err(
                UNSUPPORTED,
                "a rule command that reads NEW or OLD isn't supported for data-modifying statements in WITH yet",
            );
        }
        let st = match parse(action)?.into_iter().next().map(|s| s.0) {
            Some(s @ (Stmt::Insert { .. } | Stmt::Update { .. } | Stmt::Delete { .. })) => s,
            Some(Stmt::Select(_)) => {
                return err(
                    UNSUPPORTED,
                    "a rule's SELECT in place of a data-modifying statement in WITH isn't supported yet",
                );
            }
            _ => return refuse("DO INSTEAD NOTIFY"),
        };
        out.get_or_insert_with(|| w.clone()).dml[k].1 = st;
    }
    Ok(out)
}

/// Whether `table` has rules (not a view's INSTEAD OF triggers, kept as rules): MERGE refuses such a
/// table, as Postgres's does.
pub(super) fn has_rules(db: &Db, branch: &str, table: &str) -> Result<bool> {
    if !active(table) {
        return Ok(false);
    }
    Ok(rules_of(db, branch, table)?.iter().any(|r| r.trigger.is_none()))
}

/// How view `view`'s rules and INSTEAD OF triggers take `event` ("insert", "update", "delete"), as Postgres's
/// rewriter looks: Some(true) where an unconditional DO INSTEAD rule or an INSTEAD OF trigger does the statement's
/// work, Some(false) where only DO INSTEAD rules with conditions would (the view then can't be written), None where
/// none of them does.
pub(super) fn instead_for(v: &View, view: &str, event: &str) -> Result<Option<bool>> {
    let mut conditional = false;
    for r in v.prefix(&prefix_of(view))? {
        let (_, raw) = r?;
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        if j["event"] != event || j["enabled"] == false || j["instead"] != true {
            continue;
        }
        if j["trigger"].is_object() || j["cond"].as_str().is_none() {
            return Ok(Some(true));
        }
        conditional = true;
    }
    Ok(conditional.then_some(false))
}

/// The rewriter's error for a write to a view whose DO INSTEAD rules for it all have conditions.
pub(super) fn conditional_refused(event: &str, view: &str) -> SqlError {
    let what = match event {
        "insert" => "insert into",
        "update" => "update",
        _ => "delete from",
    };
    // (55000, as RewriteQuery has it)
    view_refused(what, view, "Views with conditional DO INSTEAD rules are not automatically updatable.")
}

/// Whether the statement is for a table with rules, which then decide what it does.
pub(super) fn applies(s: &Stmt) -> bool {
    target(s).is_some_and(|(t, _)| active(t)) && DEPTH.get() < 1000
}

fn plain_select(table: &str, items: Vec<Item>, filter: Option<Expr>) -> Select {
    let src = Source {
        table: table.into(),
        alias: rel_part(table).to_string(),
        sub: None,
        names: vec![],
        rec: None,
        working: false,
        at: None,
        lateral: false,
        srf: None,
        cte: 0,
        only: false,
        hier: false,
        policed: false,
        bound: false,
    };
    Select {
        set: vec![],
        distinct: false,
        distinct_on: vec![],
        items,
        from: Some(src),
        joins: vec![],
        filter,
        group: vec![],
        having: None,
        order: vec![],
        limit: None,
        offset: None,
        locking: vec![],
    }
}

fn run_stmt(db: &Db, branch: &str, s: Stmt, params: &[Value], types: &[Type]) -> Result<Output> {
    Statement(s, PlanCache::default(), true).execute(db, branch, params, types)
}

fn run_sql(db: &Db, branch: &str, sql: &str) -> Result<Output> {
    let mut last = Output::tag("");
    for st in parse(sql)? {
        last = st.execute(db, branch, &[], &[])?;
    }
    Ok(last)
}

/// `new.col` and `old.col` in a command, written as the values of those rows.
fn fill(sql: &str, cols: &[(String, Type)], old: Option<&[Value]>, new: Option<&[Value]>) -> Result<String> {
    let toks = lex(sql)?;
    let mut out: Vec<String> = vec![];
    let mut i = 0;
    let lit = |v: &Value, t: Type| match v {
        Value::Null => "null".to_string(),
        v => {
            let j = v.to_json();
            format!("{}::{}", sql_lit(&j, Some(t)), t.name())
        }
    };
    let value = |row: Option<&[Value]>, at: usize| match row {
        Some(r) => lit(&r[at], cols[at].1),
        None => "null".into(),
    };
    while i < toks.len() {
        if let (Tok::Word(w), Some(Tok::Sym(".")), Some(Tok::Word(c) | Tok::Ident(c))) =
            (&toks[i], toks.get(i + 1), toks.get(i + 2))
            && (w == "new" || w == "old")
            && let Some(at) = cols.iter().position(|(n, _)| n == c)
        {
            out.push(value(if w == "new" { new } else { old }, at));
            i += 3;
            continue;
        }
        // new.* and old.*: the row's values, one after another
        if let (Tok::Word(w), Some(Tok::Sym(".")), Some(Tok::Sym("*"))) = (&toks[i], toks.get(i + 1), toks.get(i + 2))
            && (w == "new" || w == "old")
        {
            let row = if w == "new" { new } else { old };
            out.push((0..cols.len()).map(|at| value(row, at)).collect::<Vec<_>>().join(", "));
            i += 3;
            continue;
        }
        out.push(tokens_shown(&toks[i..=i]));
        i += 1;
    }
    Ok(out.join(" "))
}

/// A rule's WHERE with `fill`'s values in, where a column named alone is NEW's for an INSERT rule and OLD's for a
/// DELETE rule (Postgres puts only that one in the condition's namespace; an UPDATE rule's must be qualified).
fn fill_cond(
    sql: &str,
    cols: &[(String, Type)],
    event: &str,
    old: Option<&[Value]>,
    new: Option<&[Value]>,
) -> Result<String> {
    let alone = match event {
        "insert" => "new",
        "delete" => "old",
        _ => return fill(sql, cols, old, new),
    };
    let toks = lex(sql)?;
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    for (i, t) in toks.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|p| toks.get(p));
        let next = toks.get(i + 1);
        let named = match t {
            Tok::Word(c) | Tok::Ident(c) => cols.iter().any(|(n, _)| n == c),
            _ => false,
        };
        // (not a qualified name's part, a function's name or a type after ::)
        let free = !matches!(prev, Some(Tok::Sym("." | "::"))) && !matches!(next, Some(Tok::Sym("." | "(")));
        if named && free {
            out.extend([Tok::Word(alone.into()), Tok::Sym("."), t.clone()]);
        } else {
            out.push(t.clone());
        }
    }
    fill(&tokens_shown(&out), cols, old, new)
}

/// Runs a statement that changes a table with rules.
pub(super) fn execute(s: &Statement, db: &Db, branch: &str, params: &[Value], types: &[Type]) -> Result<Output> {
    let Some((table, event)) = target(&s.0) else { return s.execute_inner(db, branch, params, types) };
    let table = table.to_string();
    let all = rules_of(db, branch, &table)?;
    // ON CONFLICT on a table with INSERT rules (or, for DO UPDATE, UPDATE rules) is refused, as the rewriter does
    if let Stmt::Insert { conflict: Some(c), .. } = &s.0
        && all.iter().any(|r| {
            r.enabled && r.trigger.is_none() && (r.event == "insert" || (r.event == "update" && c.update.is_some()))
        })
    {
        return err(
            UNSUPPORTED,
            "INSERT with ON CONFLICT clause cannot be used with table that has INSERT or UPDATE rules",
        );
    }
    let rules: Vec<Rule> = all.into_iter().filter(|r| r.event == event && r.enabled).collect();
    if rules.is_empty() {
        return s.execute_inner(db, branch, params, types);
    }
    // a view whose DO INSTEAD rules for the event all have conditions (and no INSTEAD OF trigger takes it) can't be
    // written: the rewriter's error, before any rule runs
    {
        let v = db.view_tx(branch)?;
        if view_def(&v, &table)?.is_some() && instead_for(&v, &table, event)? == Some(false) {
            return Err(conditional_refused(event, &table));
        }
    }
    // RETURNING of a statement INSTEAD rules take: one of theirs, unconditional, must give it
    let returning = matches!(
        &s.0,
        Stmt::Insert { returning: Some(_), .. }
            | Stmt::Update { returning: Some(_), .. }
            | Stmt::Delete { returning: Some(_), .. }
    );
    let real = || rules.iter().filter(|r| r.trigger.is_none() && r.instead);
    if returning
        && real().next().is_some()
        && !real().any(|r| r.cond.is_none() && r.actions.iter().any(|a| returns_rows(a)))
    {
        let ev = event.to_uppercase();
        return Err(SqlError {
            code: UNSUPPORTED,
            message: format!("cannot perform {ev} RETURNING on relation \"{}\"", rel_part(&table)),
            info: Some(Box::new(Violation {
                hint: Some(format!("You need an unconditional ON {ev} DO INSTEAD rule with a RETURNING clause.")),
                ..Violation::default()
            })),
        });
    }
    if DEPTH.get() > 20 {
        return err("54001", format!("infinite recursion detected in rules for relation \"{}\"", rel_part(&table)));
    }
    DEPTH.set(DEPTH.get() + 1);
    // (the rule commands of a rule command run inside the outermost statement's fork)
    let r = match DEPTH.get() {
        1 => as_one(db, branch, |b| apply(s, &table, event, &rules, db, b, params, types)),
        _ => apply(s, &table, event, &rules, db, branch, params, types),
    };
    DEPTH.set(DEPTH.get() - 1);
    r
}

/// Runs `f` on a fork of `branch` that is merged back once it's done: the statement and every rule command it
/// ran, each a statement (and a write) of its own, land as one write, as Postgres's rewritten query is one
/// statement. An error leaves nothing of them, so a statement run again (as `Session::execute` does when a
/// sequence's reserved values run out) doesn't repeat the commands its first run did.
fn as_one(db: &Db, branch: &str, f: impl FnOnce(&str) -> Result<Output>) -> Result<Output> {
    let tmp = format!("{TX_PREFIX}{}", new_id());
    db.fork_info(branch, &tmp, crate::Info::new())?; // a transaction's name: not a user-facing fork
    let out = f(&tmp).and_then(|o| {
        db.merge(&tmp, crate::Resolve::Fail)?;
        Ok(o)
    });
    if out.is_err() {
        let _ = db.discard(&tmp);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn apply(
    s: &Statement,
    table: &str,
    event: &str,
    rules: &[Rule],
    db: &Db,
    branch: &str,
    params: &[Value],
    types: &[Type],
) -> Result<Output> {
    // the table's columns, as they read
    let star = run_stmt(
        db,
        branch,
        Stmt::Select(Box::new(plain_select(table, vec![Item::Star], Some(Expr::Lit(Value::Bool(false)))))),
        &[],
        &[],
    )?;
    let cols: Vec<(String, Type)> = star.columns.clone().unwrap_or_default();
    let n = cols.len();
    // a view's BEFORE and AFTER statement triggers (kept as its rules: see `Exec::view_trigger`), in name order
    let statement_triggers = |before: bool| -> Result<()> {
        for rule in rules.iter().filter(|r| !r.instead) {
            let Some(t) = &rule.trigger else { continue };
            if t["level"] != "statement" || (t["timing"] == "before") != before {
                continue;
            }
            let fire = Stmt::FireInstead {
                view: table.to_string(),
                cols: cols.clone(),
                trigger: t.to_string(),
                event: event.to_string(),
                old: None,
                new: None,
            };
            run_stmt(db, branch, fire, &[], &[])?;
        }
        Ok(())
    };
    statement_triggers(true)?;
    // the rows the statement works on: (old, new)
    #[allow(clippy::type_complexity)]
    let mut rows: Vec<(Option<Vec<Value>>, Option<Vec<Value>>)> = vec![];
    match &s.0 {
        Stmt::Insert { cols: given, rows: values, query, .. } => {
            // (without a column list, VALUES fills the first columns; the rest take their defaults)
            let names: Vec<String> = given.clone().unwrap_or_else(|| {
                let w = match query {
                    None => values.iter().map(Vec::len).max().unwrap_or(0),
                    Some(_) => cols.len(),
                };
                cols.iter().take(w).map(|c| c.0.clone()).collect()
            });
            let v = db.view_tx(branch)?;
            let defaults: Vec<Option<Value>> = match load(&v, table)? {
                Some(t) => cols
                    .iter()
                    .map(|(c, _)| t.cols.iter().find(|x| x.name == *c).and_then(|x| x.default.clone()))
                    .collect(),
                // a view's columns: their own defaults (ALTER VIEW ... SET DEFAULT), or NULL, as Postgres puts in
                // what a rule or an INSTEAD OF trigger gets (not its table's)
                None => match view_def(&v, table)? {
                    Some(d) if !d.defaults.is_empty() => {
                        let mut out = vec![];
                        for (c, _) in &cols {
                            out.push(match d.defaults.iter().find(|(x, _)| x == c) {
                                Some((_, sql)) => run_sql(db, branch, &format!("select {sql}"))?
                                    .rows
                                    .into_iter()
                                    .next()
                                    .and_then(|r| r.into_iter().next()),
                                None => None,
                            });
                        }
                        out
                    }
                    _ => vec![None; n],
                },
            };
            let source: Vec<Vec<Value>> = match query {
                Some(q) => run_stmt(db, branch, Stmt::Select(q.clone()), params, types)?.rows,
                None => {
                    let mut out = vec![];
                    for row in values {
                        // VALUES (..., DEFAULT): the column's default (NEW has it)
                        let items = row
                            .iter()
                            .map(|e| match e {
                                Expr::Default => Expr::Lit(Value::Null),
                                e => e.clone(),
                            })
                            .map(|e| Item::Expr(e, None))
                            .collect();
                        let sel = Select { from: None, ..plain_select(table, items, None) };
                        for mut r in run_stmt(db, branch, Stmt::Select(Box::new(sel)), params, types)?.rows {
                            for (k, e) in row.iter().enumerate() {
                                let at = names.get(k).and_then(|n| cols.iter().position(|(c, _)| c == n));
                                if let (Expr::Default, Some(i), Some(slot)) = (e, at, r.get_mut(k)) {
                                    *slot = defaults[i].clone().unwrap_or(Value::Null);
                                }
                            }
                            out.push(r);
                        }
                    }
                    out
                }
            };
            for r in source {
                let new: Vec<Value> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, (c, _))| match names.iter().position(|x| x == c) {
                        Some(k) => r.get(k).cloned().unwrap_or(Value::Null),
                        None => defaults[i].clone().unwrap_or(Value::Null),
                    })
                    .collect();
                rows.push((None, Some(new)));
            }
        }
        Stmt::Update { sets, filter, .. } => {
            let mut items = vec![Item::Star];
            items.extend(sets.iter().map(|(_, e)| Item::Expr(e.clone(), None)));
            let out = run_stmt(
                db,
                branch,
                Stmt::Select(Box::new(plain_select(table, items, filter.clone()))),
                params,
                types,
            )?;
            for r in out.rows {
                let old = r[..n].to_vec();
                let mut new = old.clone();
                for (k, (c, _)) in sets.iter().enumerate() {
                    if let Some(at) = cols.iter().position(|x| x.0 == *c) {
                        new[at] = r[n + k].clone();
                    }
                }
                rows.push((Some(old), Some(new)));
            }
        }
        Stmt::Delete { filter, .. } => {
            let out = run_stmt(
                db,
                branch,
                Stmt::Select(Box::new(plain_select(table, vec![Item::Star], filter.clone()))),
                params,
                types,
            )?;
            for r in out.rows {
                rows.push((Some(r), None));
            }
        }
        _ => unreachable!(),
    }
    let holds = |rule: &Rule, old: Option<&[Value]>, new: Option<&[Value]>| -> Result<bool> {
        let Some(c) = &rule.cond else { return Ok(true) };
        let out = run_sql(db, branch, &format!("select ({})", fill_cond(c, &cols, event, old, new)?))?;
        Ok(matches!(out.rows.first().and_then(|r| r.first()), Some(Value::Bool(true))))
    };
    // the rules (not a view's triggers kept as rules), in name order as Postgres fires them, and for each the rows
    // its condition holds for (it reads NEW and OLD only)
    let real: Vec<&Rule> = rules.iter().filter(|r| r.trigger.is_none()).collect();
    let mut holding: Vec<Vec<bool>> = vec![];
    for r in &real {
        let mut h = vec![];
        for (old, new) in &rows {
            h.push(holds(r, old.as_deref(), new.as_deref())?);
        }
        holding.push(h);
    }
    // INSTEAD rules take rows out of the statement's way; the others run beside it
    let kept: Vec<usize> =
        (0..rows.len()).filter(|&k| !real.iter().zip(&holding).any(|(r, h)| r.instead && h[k])).collect();
    let unconditional = real.iter().any(|r| r.instead && r.cond.is_none());
    let triggered = rules.iter().any(|r| r.instead && r.trigger.is_some());
    // each rule's commands, for each row it holds for, rule after rule
    let run_actions = |returned: &mut Option<Output>| -> Result<()> {
        for (r, h) in real.iter().zip(&holding) {
            for (k, (old, new)) in rows.iter().enumerate() {
                if !h[k] {
                    continue;
                }
                for a in &r.actions {
                    let out = run_sql(db, branch, &fill(a, &cols, old.as_deref(), new.as_deref())?)?;
                    if out.columns.is_none() || !(r.instead || returned.is_none()) {
                        continue;
                    }
                    // (an INSTEAD rule's RETURNING gives a row for each row it ran for)
                    match returned.as_mut().filter(|p| p.columns == out.columns) {
                        Some(p) => {
                            p.rows.extend(out.rows);
                            p.tag = out.tag;
                        }
                        None => *returned = Some(out),
                    }
                }
            }
        }
        Ok(())
    };
    let mut returned: Option<Output> = None;
    // an INSERT's rules run after it, an UPDATE's and a DELETE's before it (as Postgres's rewriter orders them)
    let insert = event == "insert";
    if !insert {
        run_actions(&mut returned)?;
    }
    // the rows INSTEAD OF triggers did not skip (returned NULL for): what they returned, a DELETE's the row it
    // deleted; the statement's count and what its RETURNING reads
    let mut fired: Vec<Vec<Value>> = vec![];
    let mut result = if unconditional {
        None
    } else if triggered {
        // a view's INSTEAD OF triggers, in the statement's place, for each row no rule took
        for &k in &kept {
            let (old, new) = &rows[k];
            for rule in rules.iter().filter(|r| r.instead) {
                let Some(t) = &rule.trigger else { continue };
                let fire = Stmt::FireInstead {
                    view: table.to_string(),
                    cols: cols.clone(),
                    trigger: t.to_string(),
                    event: event.to_string(),
                    old: old.clone(),
                    new: new.clone(),
                };
                if let Some(r) = run_stmt(db, branch, fire, &[], &[])?.rows.into_iter().next() {
                    fired.push(match (event, old) {
                        ("delete", Some(o)) => o.clone(),
                        _ => r,
                    });
                }
            }
        }
        None
    } else if kept.len() == rows.len() {
        // no row was taken: the statement as written
        Some(s.execute_inner(db, branch, params, types)?)
    } else if insert {
        // some rows were taken: the others go in one at a time
        let mut last = Output::tag("INSERT 0 0");
        for &k in &kept {
            let new = rows[k].1.clone().unwrap_or_default();
            let vals: Vec<String> = new
                .iter()
                .zip(&cols)
                .map(|(v, (_, t))| {
                    if *v == Value::Null {
                        "null".to_string()
                    } else {
                        format!("{}::{}", sql_lit(&v.to_json(), Some(*t)), t.name())
                    }
                })
                .collect();
            let names: Vec<String> = cols.iter().map(|c| ident(&c.0)).collect();
            last = run_sql(
                db,
                branch,
                &format!("insert into {} ({}) values ({})", ident_rel(table), names.join(", "), vals.join(", ")),
            )?;
        }
        Some(last)
    } else {
        // UPDATE or DELETE: the statement, with the rows taken out of its filter
        let mut st = s.0.clone();
        let (Stmt::Update { filter, .. } | Stmt::Delete { filter, .. }) = &mut st else { unreachable!() };
        let mut guard: Vec<Expr> = vec![];
        for rule in real.iter().filter(|r| r.instead) {
            if let Some(c) = &rule.cond {
                let plain = fill(c, &cols, None, None).unwrap_or_else(|_| c.clone());
                let plain = plain.replace(" old . ", " ").replace("old.", "");
                let mut p = Parser::new(lex(&plain)?);
                guard.push(Expr::Not(Box::new(p.expr()?)));
            }
        }
        for g in guard {
            *filter = Some(match filter.take() {
                Some(f) => Expr::Bin(Box::new(f), Op::And, Box::new(g)),
                None => g,
            });
        }
        Some(run_stmt(db, branch, st, params, types)?)
    };
    if insert {
        run_actions(&mut returned)?;
    }
    // a view's INSTEAD OF trigger did the statement's work: its tag counts the rows it did, and its RETURNING
    // reads them
    if result.is_none() && returned.is_none() && triggered && !unconditional {
        let n = fired.len();
        let tag = match event {
            "insert" => format!("INSERT 0 {n}"),
            "update" => format!("UPDATE {n}"),
            _ => format!("DELETE {n}"),
        };
        let items = match &s.0 {
            Stmt::Insert { returning, .. } | Stmt::Update { returning, .. } | Stmt::Delete { returning, .. } => {
                returning.clone()
            }
            _ => None,
        };
        result = Some(match items {
            Some(items) => {
                let back = Stmt::ViewReturning { view: table.to_string(), cols: cols.clone(), rows: fired, items, tag };
                run_stmt(db, branch, back, &[], &[])?
            }
            None => Output::tag(tag),
        });
    }
    statement_triggers(false)?;
    if result.is_none() || returned.is_some() && unconditional {
        let tag = match event {
            "insert" => "INSERT 0 0",
            "update" => "UPDATE 0",
            _ => "DELETE 0",
        };
        let mut out = returned.take().unwrap_or_else(|| Output::tag(tag));
        // (a rule's RETURNING is the statement's only when the statement has one; its tag is the statement's)
        let asked = matches!(
            &s.0,
            Stmt::Insert { returning: Some(_), .. }
                | Stmt::Update { returning: Some(_), .. }
                | Stmt::Delete { returning: Some(_), .. }
        );
        if !asked && !out.tag.starts_with("SELECT") {
            out = Output::tag(out.tag);
        }
        result = Some(out);
    }
    Ok(result.unwrap())
}

/// A view's triggers (INSTEAD OF row triggers, BEFORE and AFTER statement triggers), kept as its rules one per
/// event: each once, in name order, as pg_trigger has them.
pub(super) fn view_triggers(v: &View, view: &str) -> Result<Vec<functions::Trigger>> {
    let mut out: Vec<functions::Trigger> = vec![];
    for r in v.prefix(&prefix_of(view))? {
        let (_, raw) = r?;
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        let Some(t) = j.get("trigger").filter(|t| t.is_object()) else { continue };
        let Some(x) = functions::Trigger::all_from_json(&Json::Array(vec![t.clone()])).pop() else { continue };
        if !out.iter().any(|y| y.name == x.name) {
            out.push(x);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// ALTER RULE name ON table RENAME TO `to`: the rule's row under its new name, after Postgres's
/// RenameRewriteRule checks (a view's _RETURN rule is its query, kept as the view).
pub(super) fn rename(v: &View, table: &str, name: &str, to: &str) -> Result<Vec<(String, Option<String>)>> {
    let rel = rel_part(table);
    let view = view_def(v, table)?.is_some();
    if !view && load(v, table)?.is_none() {
        return err(NO_TABLE, format!("relation \"{table}\" does not exist"));
    }
    let on_select = view && name == "_RETURN";
    // (a view trigger's rows are named `name:event`: they aren't rules)
    let raw = if on_select || name.contains(':') { None } else { v.get(&key(table, name))?.map(|r| r.to_string()) };
    if !on_select && raw.is_none() {
        return err("42704", format!("rule \"{name}\" for relation \"{rel}\" does not exist"));
    }
    if (view && to == "_RETURN") || (!to.contains(':') && v.get(&key(table, to))?.is_some()) {
        return err("42710", format!("rule \"{to}\" for relation \"{rel}\" already exists"));
    }
    if on_select {
        return err("42P17", "renaming an ON SELECT rule is not allowed");
    }
    if to == "_RETURN" {
        return err("42P17", format!("non-view rule for \"{rel}\" must not be named \"_RETURN\""));
    }
    Ok(vec![(key(table, name), None), (key(table, to), raw)])
}

/// Every rule: its table, its name and its row.
pub(super) fn all(v: &View) -> Result<Vec<(String, String, Json)>> {
    let mut out = vec![];
    for r in v.prefix(PREFIX)? {
        let (k, raw) = r?;
        let rest = &k[PREFIX.len()..];
        if let Some((table, name)) = rest.rsplit_once('/') {
            out.push((table.to_string(), name.to_string(), serde_json::from_str(&raw).unwrap_or_default()));
        }
    }
    Ok(out)
}

/// A rule as pg_rules shows it.
pub(super) fn definition(table: &str, name: &str, j: &Json) -> String {
    let event = j["event"].as_str().unwrap_or("insert").to_uppercase();
    let acts: Vec<String> =
        j["actions"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect();
    let cond = j["cond"].as_str().map_or(String::new(), |c| format!("\n  WHERE {c}"));
    let mode = if j["instead"] == true { "INSTEAD" } else { "" };
    let body = match acts.len() {
        0 => "NOTHING".to_string(),
        1 => acts[0].clone(),
        _ => format!("({})", acts.join("; ")),
    };
    format!("CREATE RULE {name} AS\n    ON {event} TO {}{cond} DO {mode} {body};", ident_rel(table))
}
