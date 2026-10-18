//! Domains, checked where Postgres checks them. A table column of a domain is its base type with the domain's NOT
//! NULL and CHECKs as its own (see `domain_column`); everywhere else a value is made a domain's here: a cast to one
//! (`'x'::d`, `CAST(x AS d)`: see `cast_expr` and `Func::Domain`), a function's argument or result of one, a
//! PL/pgSQL variable declared as one, `pg_input_is_valid` of one. As Postgres's `domain_check` runs them, a domain
//! over another meets the other's constraints first (its base's base first), and each domain's own are its NOT
//! NULL, then its CHECKs by name.

use super::*;

/// The domains `Func::Domain` checks, by its number (their stored names).
static NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `Func::Domain`'s number for domain `name` (its stored name).
pub(super) fn place(name: &str) -> u32 {
    let mut names = NAMES.lock().unwrap();
    match names.iter().position(|n| n == name) {
        Some(i) => i as u32,
        None => {
            names.push(name.to_string());
            (names.len() - 1) as u32
        }
    }
}

/// The domain `Func::Domain(i)` checks.
pub(super) fn name_at(i: u32) -> String {
    NAMES.lock().unwrap().get(i as usize).cloned().unwrap_or_default()
}

/// Domain `name`'s definition in `v` (CREATE DOMAIN's, as ALTER DOMAIN left it); None if it isn't a domain.
fn definition(v: &View, name: &str) -> Result<Option<Json>> {
    let Some(raw) = v.get(&type_key(name))? else { return Ok(None) };
    let j: Json = serde_json::from_str(&raw).unwrap_or_default();
    Ok(j.get("domain").cloned())
}

/// The type a domain is over, as written (`varchar(5)`, `int[]`, another domain's name).
fn base_text(dom: &Json) -> String {
    dom["base"].as_str().unwrap_or("text").to_string()
}

/// The domain a type the parser knows by name only (`Type::Enum`, see `resolve_type`) names, if it names one: its
/// stored name.
pub(super) fn of_type(v: &View, t: Type) -> Result<Option<String>> {
    let Type::Enum(i) = t else { return Ok(None) };
    let name = match i & UNRESOLVED_BIT {
        0 if enum_type(i).labels.is_some() => return Ok(None),
        0 => enum_type(i).name.to_string(),
        _ => match unresolved_name(i) {
            Some(n) => n,
            None => return Ok(None),
        },
    };
    Ok(definition(v, &name)?.map(|_| name))
}

/// The domain a type name as written names (`positiveint`, `"Pos"`, `public.d`; a length or an array's brackets
/// after it are not a domain's), if it names one: its stored name.
pub(super) fn named(v: &View, text: &str) -> Result<Option<String>> {
    let t = text.trim();
    if t.ends_with(']') || t.contains('(') {
        return Ok(None);
    }
    let name = bare_type(t);
    Ok(definition(v, &name)?.map(|_| name))
}

/// A type name as written, as types are stored: its parts unquoted (lowercased where they aren't quoted), public's
/// bare, without its modifier (`(5)`) or array brackets.
fn bare_type(text: &str) -> String {
    let mut t = text.trim();
    while let Some(s) = t.strip_suffix(']') {
        t = s.rfind('[').map_or(s, |i| &s[..i]).trim_end();
    }
    let t = match t.find('(') {
        Some(i) => t[..i].trim_end(),
        None => t,
    };
    let parts: Vec<String> = t
        .split('.')
        .map(|p| {
            let p = p.trim();
            match p.strip_prefix('"').and_then(|p| p.strip_suffix('"')) {
                Some(q) => q.replace("\"\"", "\""),
                None => p.to_lowercase(),
            }
        })
        .collect();
    match parts.as_slice() {
        [s, n] if s == "public" => n.clone(),
        _ => parts.join("."),
    }
}

/// Domain `name`'s own constraints (not those of a domain it is over): whether it is NOT NULL, and its CHECKs
/// (name, condition over VALUE) by name, the order Postgres runs them in. The names are CREATE DOMAIN's for its
/// checks in order (`constraint_names`, `<domain>_check` where it named none), then ALTER DOMAIN ... ADD's (`named`).
pub(super) fn own_constraints(dom: &Json, name: &str) -> (bool, Vec<(String, String)>) {
    let strs = |x: &Json| -> Vec<String> {
        x.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect()
    };
    let checks = strs(&dom["checks"]);
    let added: Vec<(String, String)> = (dom["named"].as_array().into_iter().flatten())
        .filter_map(|n| Some((n[0].as_str()?.to_string(), n[1].as_str()?.to_string())))
        .collect();
    let given = strs(&dom["constraint_names"]);
    let made = checks.len().saturating_sub(added.len());
    let base = format!("{}_check", rel_part(name));
    let mut own: Vec<(String, String)> = checks[..made]
        .iter()
        .enumerate()
        .map(|(k, text)| {
            let n = given.get(k).cloned().unwrap_or_else(|| if k == 0 { base.clone() } else { format!("{base}{k}") });
            (n, text.clone())
        })
        .collect();
    own.extend(added);
    own.sort();
    (dom["not_null"].as_bool() == Some(true), own)
}

/// A domain's own constraints, ready to run on a value of its base type (`base`): each CHECK bound to one column,
/// `value`.
struct Compiled {
    cols: Vec<Col>,
    not_null: bool,
    checks: Vec<(String, Expr)>,
}

thread_local! {
    /// Domains' constraints as `compiled` made them, by name and definition (so a changed domain is made again).
    static COMPILED: std::cell::RefCell<HashMap<String, std::rc::Rc<Compiled>>> = Default::default();
}

/// Domain `name`'s own constraints in `v`, compiled; None if it isn't a domain.
fn compiled(v: &View, name: &str) -> Result<Option<std::rc::Rc<Compiled>>> {
    let Some(raw) = v.get(&type_key(name))? else { return Ok(None) };
    let key = format!("{name}\u{0}{raw}");
    if let Some(c) = COMPILED.with_borrow(|m| m.get(&key).cloned()) {
        return Ok(Some(c));
    }
    let j: Json = serde_json::from_str(&raw).unwrap_or_default();
    let Some(dom) = j.get("domain") else { return Ok(None) };
    let made = Parser::new(lex(&format!("x {}", base_text(dom)))?).column()?;
    let cols = vec![Col::new("", "value", resolve_type(v, made.ty)?)];
    let (not_null, texts) = own_constraints(dom, name);
    let mut checks = vec![];
    for (n, text) in texts {
        // VALUE is the one column
        let read: Vec<Tok> = lex(&text)?
            .into_iter()
            .map(|t| match t {
                Tok::Word(w) if w == "value" => Tok::Ident("value".into()),
                t => t,
            })
            .collect();
        let e = Binder::plain(&cols, Some(v)).bind(&check_expr(&tokens_sql(&read))?)?;
        checks.push((n, e));
    }
    let c = std::rc::Rc::new(Compiled { cols, not_null, checks });
    COMPILED.with_borrow_mut(|m| {
        if m.len() > 256 {
            m.clear();
        }
        m.insert(key, c.clone());
    });
    Ok(Some(c))
}

/// The error Postgres gives for `x` (a value of its base type), which breaks domain `name`'s own constraints (not a
/// base domain's: see `breaks`); None where it meets them. An error in a CHECK's condition itself (a division by
/// zero) is that error. `sub` runs the functions made with CREATE FUNCTION a condition calls.
pub(super) fn breaks_own(v: &View, name: &str, x: &Value, sub: Option<&dyn Runner>) -> Result<Option<SqlError>> {
    let Some(c) = compiled(v, name)? else { return Ok(None) };
    if *x == Value::Null && c.not_null {
        let message = format!("domain {} does not allow null values", shown_type(name));
        return Ok(Some(SqlError { code: NOT_NULL, message, info: None }));
    }
    if c.checks.is_empty() {
        return Ok(None);
    }
    let x = match x {
        Value::Text(_) if c.cols[0].ty != Type::Text => coerce(x.clone(), c.cols[0].ty)?,
        x => x.clone(),
    };
    let row = [x];
    let env = Env { cols: &c.cols, row: &row, params: &[], aggs: &[], sub };
    for (n, e) in &c.checks {
        if truth(&eval(e, &env)?)? == Some(false) {
            let message = format!("value for domain {} violates check constraint \"{n}\"", shown_type(name));
            return Ok(Some(SqlError { code: "23514", message, info: None }));
        }
    }
    Ok(None)
}

/// The domains a value of domain `name` is of: the domain, the one it is over, and so on (a domain over an array
/// or a composite is the last), the base-most first.
fn chain(v: &View, name: &str) -> Result<Vec<String>> {
    let mut out = vec![name.to_string()];
    while out.len() < 64 {
        let Some(dom) = definition(v, out.last().map_or("", |n| n.as_str()))? else { break };
        let Ok(c) = Parser::new(lex(&format!("x {}", base_text(&dom)))?).column() else { break };
        match of_type(v, c.ty)? {
            Some(n) if !out.contains(&n) => out.push(n),
            _ => break,
        }
    }
    out.reverse();
    Ok(out)
}

/// `breaks_own` for all of domain `name`'s constraints: a domain it is over first.
pub(super) fn breaks(v: &View, name: &str, x: &Value, sub: Option<&dyn Runner>) -> Result<Option<SqlError>> {
    for n in chain(v, name)? {
        if let Some(mut e) = breaks_own(v, &n, x, sub)? {
            // (named as the domain the value is made, as Postgres's domain_check_input names it, whichever of
            // the domains under it has the constraint)
            if n != name {
                let (inner, outer) = (shown_type(&n), shown_type(name));
                e.message = e.message.replacen(&format!("domain {inner} "), &format!("domain {outer} "), 1);
            }
            return Ok(Some(e));
        }
    }
    Ok(None)
}

/// Refuses `x` (a value of its base type) where it breaks domain `name` (its stored name), as Postgres refuses a
/// value made a domain's: 23502 for a null where the domain (or one it is over) is NOT NULL, 23514 for a CHECK.
pub(super) fn check(v: &View, name: &str, x: &Value, sub: Option<&dyn Runner>) -> Result<()> {
    match breaks(v, name, x, sub)? {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `check`, for a value of type `t` as the parser knows it (a function's argument or result, a PL/pgSQL variable):
/// nothing to check unless `t` names a domain.
pub(super) fn check_type(v: &View, t: Type, x: &Value, sub: Option<&dyn Runner>) -> Result<()> {
    match of_type(v, t)? {
        Some(n) => check(v, &n, x, sub),
        None => Ok(()),
    }
}

/// `inner` made a value of domain `t` (a type the parser knows by name only), if `t` names one: cast to the domain's
/// base type as written (`varchar(5)` cuts to 5 characters, as an explicit cast does; a domain over another is a cast
/// to that one first), then held to the domain's own constraints (`Func::Domain`). Unbound, as the parser makes it.
pub(super) fn cast_expr(v: &View, t: Type, inner: Expr) -> Result<Option<Expr>> {
    let Some(name) = of_type(v, t)? else { return Ok(None) };
    let Some(dom) = definition(v, &name)? else { return Ok(None) };
    let mut p = Parser::new(lex(&base_text(&dom))?);
    p.typmod = None;
    let base = p.ty()?;
    // (an array's elements' length or scale is the array's elements' business)
    if matches!(base, Type::Array(_)) {
        (p.chars, p.typmod) = (None, None);
    }
    let cast = match std::mem::take(&mut p.citext) && base == Type::Text {
        true => Expr::Func(Func::Implicit(collation::citext()), vec![p.cast(inner, base)]),
        false => p.cast(inner, base),
    };
    Ok(Some(Expr::Func(Func::Domain(place(&name)), vec![cast])))
}

/// `x` cast to type `t` where what runs the cast hasn't bound it (a VALUES row, a PL/pgSQL expression, a prepared
/// statement's `$1::d`): as `cast_expr` makes it, if `t` names a domain; None if it doesn't.
pub(super) fn cast_unbound(t: Type, x: &Value, env: &Env) -> Result<Option<Value>> {
    let done = with_path_view(|v| -> Result<Option<Value>> {
        let Some(e) = cast_expr(v, t, Expr::Lit(x.clone()))? else { return Ok(None) };
        let e = Binder::plain(&[], Some(v)).bind(&e)?;
        eval(&e, &Env { cols: &[], row: &[], params: env.params, aggs: &[], sub: env.sub }).map(Some)
    });
    done.unwrap_or(Ok(None))
}

/// Runs `Func::Domain(d)`'s check on `x`, its argument's value: `x` where it meets the domain's own constraints.
pub(super) fn held(d: u32, x: Value, env: &Env) -> Result<Value> {
    let name = name_at(d);
    let broken = with_path_view(|v| breaks_own(v, &name, &x, env.sub)).transpose()?.flatten();
    match broken {
        Some(e) => Err(e),
        None => Ok(x),
    }
}

/// The domain a bound expression's value is of, as pg_typeof sees it: a cast to one, a column of one, COALESCE of
/// values all of one. None for a value of a base type.
pub(super) fn of_expr(e: &Expr, cols: &[Col]) -> Option<String> {
    match e {
        Expr::Func(Func::Domain(d), _) => Some(name_at(*d)),
        Expr::Idx(i) => cols.get(*i).and_then(|c| c.domain.clone()),
        Expr::Func(Func::Collate(_) | Func::Implicit(_), a) => a.first().and_then(|x| of_expr(x, cols)),
        Expr::Func(Func::Coalesce, a) => {
            let mut each = a.iter().filter(|x| !matches!(x, Expr::Lit(Value::Null))).map(|x| of_expr(x, cols));
            let first = each.next()??;
            each.all(|n| n.as_deref() == Some(first.as_str())).then_some(first)
        }
        _ => None,
    }
}

/// pg_typeof's value for domain `name`: its regtype.
pub(super) fn regtype(name: &str) -> Option<Value> {
    catalog::reg_value(Reg::Type, &shown_type(name)).ok().map(|o| Value::Reg(o, Reg::Type))
}

/// What a cast to a domain was made of (see `cast_expr`): the value cast, inside the base type's cast and its
/// length's cut, for naming the column it makes.
pub(super) fn operand(e: &Expr) -> &Expr {
    match e {
        Expr::Cast(x, _) => operand(x),
        Expr::Func(Func::Domain(_) | Func::Implicit(_) | Func::Round | Func::Scalar(funcs::Scalar::Left), a)
            if !a.is_empty() =>
        {
            operand(&a[0])
        }
        e => e,
    }
}

/// pg_input_is_valid and pg_input_error_info of a domain: `text` read as domain `name`'s base type (`read`, which
/// reads it as a type named as written), then held to the domain's constraints. The error, where it doesn't read or
/// a constraint refuses it; an error in a CHECK's condition itself is raised.
pub(super) fn read_as(
    v: &View,
    name: &str,
    text: &str,
    read: impl Fn(&str) -> Result<Result<()>>,
) -> Result<Result<()>> {
    let Some(dom) = definition(v, name)? else { return Ok(Ok(())) };
    let base = base_text(&dom);
    if let Err(e) = read(&base)? {
        return Ok(Err(e));
    }
    let made = Parser::new(lex(&format!("x {base}"))?).column()?;
    let x = match coerce(Value::Text(text.into()), resolve_type(v, made.ty)?) {
        Ok(x) => x,
        Err(e) => return Ok(Err(e)),
    };
    // (the base type's own domain, where it is one, was held to its constraints reading it)
    Ok(match breaks_own(v, name, &x, None)? {
        Some(e) => Err(e),
        None => Ok(()),
    })
}

// ---------------------------------------------------------------- ALTER DOMAIN and DROP TYPE

/// How a column declared as type `text` holds domain `name`: as its value (the domain, or a domain over it, so
/// ALTER DOMAIN checks the column's values: Some(false)), inside another type (an array of it, a composite's field, a
/// range over it: Some(true), which ALTER DOMAIN can't check), or not at all (None).
pub(super) fn holds(v: &View, text: &str, name: &str) -> Result<Option<bool>> {
    fn walk(v: &View, text: &str, name: &str, inside: bool, depth: u32) -> Result<Option<bool>> {
        if depth > 32 {
            return Ok(None);
        }
        let inside = inside || text.trim_end().ends_with(']');
        let bare = bare_type(text);
        if bare == name {
            return Ok(Some(inside));
        }
        let Some(raw) = v.get(&type_key(&bare))? else { return Ok(None) };
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        if let Some(d) = j.get("domain") {
            return walk(v, &base_text(d), name, inside, depth + 1);
        }
        for t in made_of(&j) {
            if walk(v, &t, name, true, depth + 1)?.is_some() {
                return Ok(Some(true));
            }
        }
        Ok(None)
    }
    walk(v, text, name, false, 0)
}

/// The types a composite (its fields') or a range (its subtype) is made of, as written.
fn made_of(j: &Json) -> Vec<String> {
    if let Some(fields) = j["composite"].as_array() {
        return fields.iter().filter_map(|f| f[1].as_str().map(String::from)).collect();
    }
    j["range"]["subtype"].as_str().map(String::from).into_iter().collect()
}

/// The type a column was declared as, by a name `holds` reads: its domain, its composite type (with `[]` for an array
/// of one), its range type. None for a built-in type.
pub(super) fn column_type(c: &Col) -> Option<String> {
    if let Some(d) = &c.domain {
        return Some(d.clone());
    }
    match c.ty {
        t @ Type::Record(_) => userdefs::composite_name(t),
        Type::Array(e) => Type::from_oid(e)
            .filter(|t| matches!(t, Type::Record(_)))
            .and_then(userdefs::composite_name)
            .map(|n| format!("{n}[]")),
        Type::Range(k) => range::name_of(k).map(|n| n.to_string()),
        _ => None,
    }
}

/// The types made over type `name` (their stored names): the domains over it or an array of it, the range types over
/// it. What DROP TYPE ... CASCADE drops with it. (A composite type with a field of it would lose that field, which
/// isn't done here.)
pub(super) fn made_over(v: &View, name: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    for r in v.prefix("_sonos_types/")? {
        let (k, raw) = r?;
        let n = &k["_sonos_types/".len()..];
        if n == name {
            continue;
        }
        let j: Json = serde_json::from_str(&raw).unwrap_or_default();
        let texts: Vec<String> = match j.get("domain") {
            Some(d) => vec![base_text(d)],
            None => j["range"]["subtype"].as_str().map(String::from).into_iter().collect(),
        };
        if texts.iter().any(|t| bare_type(t) == name) {
            out.push(n.to_string());
        }
    }
    Ok(out)
}

/// The table column checks that are domain check `text` (over VALUE) on column `col`, compared as tokens: what
/// `domain_column` made of one of CREATE DOMAIN's checks.
pub(super) fn same_check(text: &str, col: &str, table_check: &str) -> bool {
    let Ok(toks) = lex(text) else { return false };
    let read: Vec<Tok> = toks
        .into_iter()
        .map(|t| match t {
            Tok::Word(w) if w == "value" => Tok::Ident(col.to_string()),
            t => t,
        })
        .collect();
    let norm = |s: &str| lex(s).map(|t| tokens_sql(&t)).unwrap_or_default();
    norm(&tokens_sql(&read)) == norm(table_check)
}
