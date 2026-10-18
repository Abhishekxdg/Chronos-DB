//! What drivers, pools and admin tools ask a server about itself and the session, mostly on
//! connect: `version()`, `current_user`, `pg_backend_pid()`, `has_table_privilege()` and their
//! kin. They reach past the row, to the session (see `Ctx`), so they are `CtxFn`s.

use super::*;

/// The functions here.
pub(super) fn handles(f: CtxFn) -> bool {
    matches!(
        f,
        CtxFn::CurrentUser
            | CtxFn::SessionUser
            | CtxFn::CurrentRole
            | CtxFn::Version
            | CtxFn::BackendPid
            | CtxFn::TriggerDepth
            | CtxFn::NotifyQueueUsage
            | CtxFn::DbEncoding
            | CtxFn::ExportSnapshot
            | CtxFn::InRecovery
            | CtxFn::PostmasterStart
            | CtxFn::InetNull(_)
            | CtxFn::Sleep
            | CtxFn::CancelBackend
            | CtxFn::TerminateBackend
            | CtxFn::BlockingPids
            | CtxFn::HasPrivilege(_)
            | CtxFn::TxidCurrent
            | CtxFn::XactId(..)
            | CtxFn::CurrentSnapshot(_)
            | CtxFn::XactStatus(_)
            | CtxFn::SerialSequence
            | CtxFn::SizePretty
            | CtxFn::SizeBytes
            | CtxFn::RelationSize(_)
            | CtxFn::DatabaseSize
            | CtxFn::GinCleanPendingList
    )
}

/// What `version()` says. Clients read the leading `PostgreSQL 16.15` (SQLAlchemy, Prisma and
/// Flyway parse it); the rest says what this is.
fn version() -> String {
    format!(
        "PostgreSQL {SERVER_VERSION} (Chronos {}) on {}, compiled by rustc, {}-bit",
        env!("CARGO_PKG_VERSION"),
        target_triple(),
        usize::BITS
    )
}

/// The platform this was built for, as a target triple names it (`x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`),
/// which is where Postgres's version() names its host.
fn target_triple() -> String {
    let vendor = if cfg!(target_vendor = "apple") {
        "apple"
    } else if cfg!(target_vendor = "pc") {
        "pc"
    } else {
        "unknown"
    };
    let env = if cfg!(target_env = "gnu") {
        "-gnu"
    } else if cfg!(target_env = "musl") {
        "-musl"
    } else if cfg!(target_env = "msvc") {
        "-msvc"
    } else {
        ""
    };
    format!("{}-{vendor}-{}{env}", std::env::consts::ARCH, std::env::consts::OS)
}

pub(super) fn call(ctx: &Ctx, f: CtxFn, a: &[Value]) -> Result<Value> {
    use Value::*;
    Ok(match f {
        // (a SECURITY DEFINER function's current_user is its owner)
        CtxFn::CurrentUser | CtxFn::CurrentRole => {
            Text(roles::current().map_or_else(|| ctx.user.clone(), |w| w.name.clone()))
        }
        CtxFn::SessionUser => Text(ctx.session_user.clone()),
        CtxFn::Version => Text(version()),
        CtxFn::BackendPid => Int(ctx.pid.into()),
        CtxFn::TriggerDepth => Int(functions::trigger_depth() as i64),
        CtxFn::NotifyQueueUsage => Float(0.0),
        CtxFn::DbEncoding => Text("UTF8".into()),
        CtxFn::ExportSnapshot => {
            if ctx.tx.is_some() {
                ctx.state.lock().unwrap().exported = true;
            }
            Text(export_snapshot(&ctx.core, &ctx.home, ctx.session, ctx.txid, ctx.tx.as_deref())?)
        }
        CtxFn::InRecovery => Bool(false),
        CtxFn::PostmasterStart => TimestampTz(time::started()),
        CtxFn::TxidCurrent => Int(current_xact_id(ctx.txid, ctx.tx.is_some(), false).unwrap_or(ctx.txid)),
        CtxFn::XactId(xid8, if_assigned) => match current_xact_id(ctx.txid, ctx.tx.is_some(), if_assigned) {
            Some(n) if xid8 => coerce(Int(n), Type::Xid8)?,
            Some(n) => Int(n),
            None => Null,
        },
        CtxFn::CurrentSnapshot(_) => Text(current_snapshot(ctx.txid)),
        CtxFn::XactStatus(_) => match coerce(a[0].clone(), Type::Xid8)? {
            Null => Null,
            v => match v.to_text().parse::<i64>() {
                Ok(n) => xact_status(n)?,
                Err(_) => return err("22023", format!("transaction ID {} is in the future", v.to_text())),
            },
        },
        // null, as Postgres answers for a connection over a Unix socket
        CtxFn::InetNull(_) => Null,
        CtxFn::Sleep => {
            if a[0] == Null {
                return Ok(Null);
            }
            sleep(as_f64(&a[0]))?;
            Text(String::new()) // void: shows as nothing
        }
        CtxFn::CancelBackend | CtxFn::TerminateBackend => {
            let terminate = f == CtxFn::TerminateBackend;
            if ctx.agent {
                return err(
                    "42501",
                    format!(
                        "permission denied to {} a connection: an agent can't",
                        if terminate { "terminate" } else { "cancel" }
                    ),
                );
            }
            if a[0] == Null {
                return Ok(Null);
            }
            let pid = match coerce(a[0].clone(), Type::BigInt)? {
                Int(n) => i32::try_from(n).ok(),
                _ => None,
            };
            let found = pid.and_then(|p| {
                let all = BACKENDS.lock().unwrap_or_else(|p| p.into_inner());
                all.get(&p).map(|b| {
                    let role = b.info.lock().unwrap_or_else(|p| p.into_inner()).role;
                    (b.cancel.clone(), b.socket.as_ref().and_then(|s| s.try_clone().ok()), role)
                })
            });
            // Postgres 16's rule: a superuser's connection only a superuser may signal; another's,
            // a role with that role's privileges or pg_signal_backend's
            if let (Some((.., role)), Some(who)) = (&found, roles::checked()) {
                let reg = roles::registry(&ctx.core)?;
                let (what, whose) = if terminate {
                    ("terminate process", "terminate processes of")
                } else {
                    ("cancel query", "cancel queries of")
                };
                if role.is_none_or(|r| reg.by_oid(r).is_some_and(|r| r.superuser)) {
                    return Err(roles::denied(
                        format!("permission denied to {what}"),
                        format!(
                            "Only roles with the SUPERUSER attribute may {whose} roles with the SUPERUSER attribute."
                        ),
                    ));
                }
                if role.is_some_and(|r| !who.has_privs_of(r)) && !who.has_privs_of(roles::SIGNAL_BACKEND) {
                    let detail = match terminate {
                        true => {
                            "Only roles with privileges of the role whose process is being terminated or with privileges of the \"pg_signal_backend\" role may terminate this process."
                        }
                        false => {
                            "Only roles with privileges of the role whose query is being canceled or with privileges of the \"pg_signal_backend\" role may cancel this query."
                        }
                    };
                    return Err(roles::denied(format!("permission denied to {what}"), detail));
                }
            }
            match &found {
                Some((cancel, socket, _)) => {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    // ending it: the socket closes, and the connection's thread sees the client gone
                    if let (true, Some(s)) = (terminate, socket) {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                    }
                }
                None => notice("WARNING", format!("PID {} is not a PostgreSQL backend process", a[0].to_text())),
            }
            Bool(found.is_some())
        }
        // the connections whose row intents, advisory locks or sequences hold up `pid`'s wait (see
        // `conn_waits`); none for a backend that isn't waiting, or isn't one, as in Postgres
        CtxFn::BlockingPids => {
            if a[0] == Null {
                return Ok(Null);
            }
            let pid = match coerce(a[0].clone(), Type::BigInt)? {
                Int(n) => i32::try_from(n).ok(),
                _ => None,
            };
            let mut waits = conn_waits(Some(&*ctx.core));
            let blocking = pid.and_then(|p| waits.remove(&p)).map(|w| w.blocking).unwrap_or_default();
            let vals: Vec<Value> = blocking.into_iter().map(|p| Int(p.into())).collect();
            Text(array_text(vals.iter()))
        }
        CtxFn::HasPrivilege(kind) => return privilege(ctx, kind, a),
        CtxFn::SerialSequence => return serial_sequence(ctx, a),
        CtxFn::SizePretty => size_pretty(&a[0]),
        CtxFn::SizeBytes => size_bytes(&a[0])?,
        CtxFn::RelationSize(what) => relation_size(ctx, what, a)?,
        CtxFn::DatabaseSize => database_size(ctx, &a[0])?,
        CtxFn::GinCleanPendingList => gin_clean_pending_list(ctx, &a[0])?,
        _ => unreachable!("not a sysinfo function"),
    })
}

/// pg_sleep: waits, and stops early when the statement is canceled or times out.
fn sleep(secs: f64) -> Result<()> {
    let end = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs.clamp(0.0, 1e9));
    loop {
        stopped()?;
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(std::time::Duration::from_millis(10)));
    }
}

/// The privilege names Postgres accepts for each kind of object.
fn privileges(kind: &str) -> &'static [&'static str] {
    match kind {
        "table" => &["select", "insert", "update", "delete", "truncate", "references", "trigger"],
        "column" | "any_column" => &["select", "insert", "update", "references"],
        "schema" => &["create", "usage"],
        "database" => &["create", "connect", "temporary", "temp"],
        "sequence" => &["select", "update", "usage"],
        "function" => &["execute"],
        "role" => &["member", "usage", "set"],
        _ => &["usage"],
    }
}

/// has_table_privilege([role,] table, privilege) and the others, and pg_has_role([role,] role,
/// privilege): what a role (the current one, if none is given) may do, by the object's owner and
/// grants (see `acl`). An agent's rights are capabilities that differ by world, which a yes or no
/// here would hide, so an agent's session is refused.
fn privilege(ctx: &Ctx, kind: &str, a: &[Value]) -> Result<Value> {
    if a.contains(&Value::Null) {
        return Ok(Value::Null);
    }
    if ctx.agent {
        return err(
            UNSUPPORTED,
            "has_*_privilege() isn't available to an agent: its rights are its capabilities, which depend on the world",
        );
    }
    // (role,) object (, column), privilege
    let columns = usize::from(kind == "column");
    let has_role = a.len() == 3 + columns;
    let object = &a[usize::from(has_role)];
    let reg = roles::registry(&ctx.core)?;
    let who = match has_role {
        true => roles::Who::for_role(&reg, roles::find(&reg, &a[0], kind != "role")?),
        false => roles::current().map_or_else(|| roles::Who::boot(&ctx.user), |w| (*w).clone()),
    };
    // an object given by an oid that names none is null, as in Postgres (a name that names none is an error)
    let by_oid = matches!(object, Value::Int(_));
    let mut name = object.to_text();
    if let (Value::Int(o), "schema" | "database") = (object, kind) {
        let cat = catalog::Catalog::of(&ctx.core, &ctx.world)?;
        let found = match kind {
            "schema" => match o {
                11 => Some("pg_catalog".to_string()),
                2200 => Some("public".into()),
                13000 => Some("information_schema".into()),
                o => cat.schemas.iter().find(|s| cat.schema_oid(s) == *o).cloned(),
            },
            _ => match o {
                1 => Some("template1".to_string()),
                4 => Some("template0".into()),
                5 => Some("postgres".into()),
                o => usize::try_from(o - 16384).ok().and_then(|i| cat.worlds.get(i)).cloned(),
            },
        };
        match found {
            Some(n) => name = n,
            None => return Ok(Value::Null),
        }
    }
    // a foreign-data wrapper or server by its oid (see `foreign`)
    if let (Value::Int(o), "fdw" | "server") = (object, kind) {
        match super::foreign::name_of_oid(&ctx.core.view_tx(&ctx.world)?, kind, *o)? {
            Some(n) => name = n,
            None => return Ok(Value::Null),
        }
    }
    match kind {
        "table" | "sequence" | "column" | "any_column" if catalog::rel_shown(object)?.is_none() => {
            if by_oid {
                return Ok(Value::Null);
            }
            return err(NO_TABLE, format!("relation \"{name}\" does not exist"));
        }
        "schema" => {
            let known = namespaces(&ctx.core.view_tx(&ctx.world)?)?;
            if !known.contains(&name) && !matches!(name.as_str(), "pg_catalog" | "information_schema" | "public") {
                return err("3F000", format!("schema \"{name}\" does not exist"));
            }
        }
        "database" if !database_exists(ctx, &name) => {
            return err("3D000", format!("database \"{name}\" does not exist"));
        }
        "fdw" | "server" if !super::foreign::exists(&ctx.core.view_tx(&ctx.world)?, kind, &name)? => {
            let noun = if kind == "fdw" { "foreign-data wrapper" } else { "server" };
            return err("42704", format!("{noun} \"{name}\" does not exist"));
        }
        _ => {}
    }
    let text = a[a.len() - 1].to_text();
    for token in text.split(',') {
        let token = token.trim().to_lowercase();
        let token = token
            .strip_suffix("with grant option")
            .or_else(|| token.strip_suffix("with admin option"))
            .map_or(token.as_str(), str::trim_end);
        if !privileges(kind).contains(&token) {
            return err("22023", format!("unrecognized privilege type: \"{}\"", text.trim()));
        }
    }
    if kind == "role" {
        let role = roles::find(&reg, object, false)?;
        // (a login that is no role is the bootstrap superuser: every role's member)
        let member = if who.superuser { roles::BOOTSTRAP } else { who.oid };
        return Ok(Value::Bool(text.split(',').any(|t| roles::has_role(&reg, member, role, t))));
    }
    // the database a function asks about is the one it's in
    let world = if kind == "database" { ctx.home.as_str() } else { ctx.world.as_str() };
    let col = (kind == "column").then(|| &a[a.len() - 2]);
    let text = text.replace("temporary", "temp");
    let object =
        if by_oid && matches!(kind, "schema" | "database" | "fdw" | "server") { &Value::Text(name) } else { object };
    Ok(acl::has_privilege(&ctx.core, world, kind, &who, object, col, &text)?.map_or(Value::Null, Value::Bool))
}

/// The name Postgres gives the sequence of a serial or identity column: `public.t_id_seq`, quoted
/// where an identifier needs it.
fn serial_name(table: &str, column: &str) -> String {
    format!("{}.{}", ident(schema_part(table)), ident(&format!("{}_{column}_seq", rel_part(table))))
}

/// A serial or identity column's own counter (it has no sequence of its own to name).
pub(super) fn own_counter(v: &View, c: &Col) -> bool {
    c.auto == Some(Auto::Serial) && c.seq.as_deref().is_none_or(|s| s.contains('.') && !named_sequence(v, s))
}

/// pg_get_serial_sequence(table, column): the sequence behind a serial, identity or `nextval('s')`
/// column, else null. The table is read as SQL reads a name; the column is taken as written.
fn serial_sequence(ctx: &Ctx, a: &[Value]) -> Result<Value> {
    if a.contains(&Value::Null) {
        return Ok(Value::Null);
    }
    let v = ctx.core.view_tx(&ctx.world)?;
    let text = a[0].to_text();
    let name = text_rel(&text);
    let Some(t) = load(&v, &name)? else {
        return err(NO_TABLE, format!("relation \"{text}\" does not exist"));
    };
    let column = a[1].to_text();
    let Some(c) = t.cols.iter().find(|c| c.name == column) else {
        return err("42703", format!("column \"{column}\" of relation \"{}\" does not exist", rel_part(&name)));
    };
    Ok(match &c.seq {
        _ if own_counter(&v, c) => Value::Text(serial_name(&name, &c.name)),
        // a sequence made with CREATE SEQUENCE (and Postgres's dumps say it is owned by the column)
        Some(seq) => Value::Text(format!("{}.{}", ident(schema_part(seq)), ident(rel_part(seq)))),
        None => Value::Null,
    })
}

/// The counter of the serial column that `name` (as stored: `t_id_seq`, `app.t_id_seq`) is
/// Postgres's name for, as a sequence: whole numbers up, from 1.
pub(super) fn implicit_sequence(v: &View, name: &str) -> Result<Option<SeqDef>> {
    let Some(stem) = rel_part(name).strip_suffix("_seq") else { return Ok(None) };
    let schema = schema_part(name);
    for entry in v.prefix(&format!("{SCHEMAS}/"))? {
        let table = entry?.0[SCHEMAS.len() + 1..].to_string();
        if schema_part(&table) != schema || !stem.starts_with(rel_part(&table)) {
            continue;
        }
        let Some(t) = load(v, &table)? else { continue };
        for c in t.cols.iter().filter(|c| own_counter(v, c)) {
            if format!("{}_{}", rel_part(&table), c.name) != stem {
                continue;
            }
            let (ty, max) = match c.ty {
                Type::SmallInt => ("smallint", i64::from(i16::MAX)),
                Type::Int => ("integer", i64::from(i32::MAX)),
                _ => ("bigint", i64::MAX),
            };
            let id = c.seq.clone().unwrap_or_else(|| format!("{table}.{}", c.name));
            let owned = Some((table.clone(), c.name.clone()));
            return Ok(Some(SeqDef { id, ty, start: 1, inc: 1, min: 1, max, cycle: false, owned, cache: 1 }));
        }
    }
    Ok(None)
}

/// pg_size_pretty(bytes) as Postgres 16 writes it: bytes up to 10240, then the unit at which the
/// number (rounded half away from zero) is under 20480.
fn size_pretty(v: &Value) -> Value {
    if *v == Value::Null {
        return Value::Null;
    }
    // (name, limit, rounds, bits): a number of a unit is `bits` of shifting from a byte
    const UNITS: [(&str, i128, bool, u32); 6] = [
        ("bytes", 10 * 1024, false, 0),
        ("kB", 20 * 1024 - 1, true, 10),
        ("MB", 20 * 1024 - 1, true, 20),
        ("GB", 20 * 1024 - 1, true, 30),
        ("TB", 20 * 1024 - 1, true, 40),
        ("PB", 20 * 1024 - 1, true, 50),
    ];
    // a numeric below the first limit is shown as it is, fraction and all
    let text = v.to_text();
    if let Value::Numeric(_) = v
        && text.contains('.')
        && text
            .trim_start_matches('-')
            .split('.')
            .next()
            .is_some_and(|w| w.len() < 5 && w.parse::<i64>().is_ok_and(|n| n < 10 * 1024))
    {
        return Value::Text(format!("{text} bytes"));
    }
    // (above it, cut to a whole number, as Postgres's shifting does)
    let mut size: i128 = text.split('.').next().and_then(|w| w.parse().ok()).unwrap_or(0);
    for (i, &(name, limit, rounds, bits)) in UNITS.iter().enumerate() {
        if i + 1 == UNITS.len() || size.abs() < limit {
            if rounds {
                size = (size + if size < 0 { -1 } else { 1 }) / 2;
            }
            return Value::Text(format!("{size} {name}"));
        }
        let (_, _, next_rounds, next_bits) = UNITS[i + 1];
        size /= 1i128 << (next_bits - bits - u32::from(next_rounds) + u32::from(rounds));
    }
    unreachable!("the last unit ends the loop")
}

/// pg_size_bytes('1.5 MB'): a number, and bytes, kB, MB, GB, TB or PB (in any case), as a whole
/// number of bytes.
fn size_bytes(v: &Value) -> Result<Value> {
    if *v == Value::Null {
        return Ok(Value::Null);
    }
    let text = v.to_text();
    let bad = |detail: Option<String>| SqlError {
        code: "22023",
        message: format!("invalid size: \"{text}\""),
        info: detail.map(|d| {
            Box::new(Violation {
                detail: Some(d),
                hint: Some("Valid units are \"bytes\", \"B\", \"kB\", \"MB\", \"GB\", \"TB\", and \"PB\".".into()),
                ..Violation::default()
            })
        }),
    };
    let s = text.trim_start();
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let digits = |from: usize| b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let whole = digits(i);
    i += whole;
    let mut fraction = 0;
    if b.get(i) == Some(&b'.') {
        fraction = digits(i + 1);
        i += 1 + fraction;
    }
    if whole + fraction == 0 {
        return Err(bad(None));
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(b.get(i + 1), Some(b'+' | b'-')));
        let exp = digits(i + 1 + sign);
        if exp > 0 {
            i += 1 + sign + exp;
        }
    }
    // (an exponent past what numeric holds is its own error)
    if let Some(e) = s[..i].split(['e', 'E']).nth(1).and_then(|e| e.parse::<i128>().ok())
        && e.abs() > 100_000_000
    {
        return err("22003", "value overflows numeric format");
    }
    let number: f64 = s[..i].parse().map_err(|_| bad(None))?;
    let unit = s[i..].trim();
    let factor: f64 = match unit.to_lowercase().as_str() {
        "" | "b" | "bytes" => 1.0,
        "kb" => 1024.0,
        "mb" => 1024.0 * 1024.0,
        "gb" => 1024.0f64.powi(3),
        "tb" => 1024.0f64.powi(4),
        "pb" => 1024.0f64.powi(5),
        _ => return Err(bad(Some(format!("Invalid size unit: \"{unit}\".")))),
    };
    let bytes = (number * factor).round();
    if !(bytes >= i64::MIN as f64 && bytes < i64::MAX as f64) {
        return err(RANGE, "bigint out of range");
    }
    Ok(Value::Int(bytes as i64))
}

/// Postgres's page, which its sizes are whole numbers of.
const PAGE: i64 = 8192;

/// A number of bytes as whole pages, of which a fraction `fill` is used.
fn pages(bytes: i64, fill: f64) -> i64 {
    (bytes as f64 / (PAGE as f64 * fill)).ceil() as i64
}

/// What a relation's storage takes, as Postgres would count it in pages: the table's rows (with
/// a tuple header each), and each index's entries. Chronos keeps rows and index entries as small
/// rows of a tree, so the numbers are estimates in Postgres's units, from what is stored.
fn relation_size(ctx: &Ctx, what: &str, a: &[Value]) -> Result<Value> {
    if a[0] == Value::Null {
        return Ok(Value::Null);
    }
    if let Some(fork) = a.get(1) {
        let fork = fork.to_text();
        if !matches!(fork.as_str(), "main" | "fsm" | "vm" | "init") {
            return err("22023", format!("invalid fork name: \"{fork}\""));
        }
    }
    let Some(shown) = catalog::rel_shown(&a[0])? else {
        return err(NO_TABLE, format!("relation \"{}\" does not exist", a[0].to_text()));
    };
    let v = ctx.core.view_tx(&ctx.world)?;
    let name = text_rel(&shown);
    // how many entries there are under a prefix, and the bytes of them (and of their keys)
    let bytes = |prefix: &str| -> Result<(i64, i64, i64)> {
        let (mut rows, mut total, mut keys) = (0, 0, 0);
        for r in v.prefix(prefix)? {
            let (k, raw) = r?;
            rows += 1;
            total += (k.len() + raw.len()) as i64;
            keys += k.len() as i64;
        }
        Ok((rows, total, keys))
    };
    // a non-unique b-tree's entries, as Postgres's deduplication keeps them: a run of equal keys
    // is one posting-list tuple, the key once and a 6-byte heap pointer per further row (so an
    // index on a constant is a fraction of one on distinct values); the tuples, and their bytes
    let deduped = |prefix: &str| -> Result<(i64, i64)> {
        let (mut tuples, mut total) = (0, 0);
        let mut last: Option<String> = None;
        for r in v.prefix(prefix)? {
            let (k, raw) = r?;
            let key = k.rsplit_once('\u{1}').map_or(k.as_str(), |(value, _)| value);
            if last.as_deref() == Some(key) {
                total += 6;
            } else {
                tuples += 1;
                total += (k.len() + raw.len()) as i64;
                last = Some(key.to_string());
            }
        }
        Ok((tuples, total))
    };
    // a table's heap, and its indexes by name (the primary key's first)
    let table = |t: &Table| -> Result<(i64, Vec<(String, i64)>)> {
        let (rows, stored, keys) = bytes(&format!("{}/", t.name))?;
        let heap = if rows == 0 { 0 } else { pages(stored + 28 * rows, 0.99) * PAGE };
        let index = |entries: i64, total: i64| {
            let leaves = pages(total + 8 * entries, 0.9);
            (1 + leaves + leaves / 300) * PAGE
        };
        let mut indexes = vec![];
        if t.has_key() {
            indexes.push((t.pkey_name(), index(rows, keys)));
        }
        for u in &t.uniques {
            let (n, b, _) = bytes(&format!("{UNIQ}/{}/{}/", t.name, u.name))?;
            indexes.push((u.name.clone(), index(n, b)));
        }
        for x in t.indexes.iter().filter(|x| !t.uniques.iter().any(|u| u.name == x.name)) {
            let (n, b) = if x.gin {
                let (n, b, _) = bytes(&btree::prefix(&t.name, &x.name))?;
                (n, b)
            } else {
                deduped(&btree::prefix(&t.name, &x.name))?
            };
            indexes.push((x.name.clone(), index(n, b)));
        }
        Ok((heap, indexes))
    };
    if let Some(t) = load(&v, &name)? {
        let (heap, indexes) = table(&t)?;
        let indexes: i64 = indexes.iter().map(|(_, n)| n).sum();
        return Ok(Value::Int(match what {
            "indexes" => indexes,
            "total" => heap + indexes,
            _ => heap,
        }));
    }
    // an index: by its own name (an index has no indexes)
    for t in schemas(&v)?.into_iter().filter(|t| schema_part(&t.name) == schema_part(&name)) {
        if let Some((_, size)) = table(&t)?.1.into_iter().find(|(n, _)| *n == rel_part(&name)) {
            return Ok(Value::Int(if what == "indexes" { 0 } else { size }));
        }
    }
    // a sequence is a page; a view has no storage
    Ok(Value::Int(if seq_def(&v, &name)?.is_some() { PAGE } else { 0 }))
}

/// gin_clean_pending_list(index): how many pages of the index's pending list (the entries its fast updates put
/// off) were moved into the index proper. A GIN index here makes its entries as each row is written, so it has
/// none pending: 0. A relation that isn't one is refused as Postgres refuses it.
fn gin_clean_pending_list(ctx: &Ctx, rel: &Value) -> Result<Value> {
    if *rel == Value::Null {
        return Ok(Value::Null);
    }
    let Some(shown) = catalog::rel_shown(rel)? else {
        return err(NO_TABLE, format!("relation \"{}\" does not exist", rel.to_text()));
    };
    let v = ctx.core.view_tx(&ctx.world)?;
    let name = text_rel(&shown);
    let (schema, of) = (schema_part(&name), rel_part(&name));
    for t in schemas(&v)?.into_iter().filter(|t| schema_part(&t.name) == schema) {
        if let Some(x) = t.indexes.iter().find(|x| x.name == of) {
            if !x.gin {
                return err("42809", format!("\"{of}\" is not a GIN index"));
            }
            return Ok(Value::Int(0));
        }
        if t.uniques.iter().any(|u| u.name == of) || (t.has_key() && t.pkey_name() == of) {
            return err("42809", format!("\"{of}\" is not a GIN index"));
        }
    }
    err("42809", format!("\"{of}\" is not an index"))
}

/// pg_database_size(name): the bytes the database's files take (worlds share them).
fn database_size(ctx: &Ctx, name: &Value) -> Result<Value> {
    if *name == Value::Null {
        return Ok(Value::Null);
    }
    if let Value::Text(n) = name
        && !database_exists(ctx, n)
    {
        return err("3D000", format!("database \"{n}\" does not exist"));
    }
    let used = ctx.core.disk_usage().map_err(|e| SqlError { code: "58030", message: e.to_string(), info: None })?;
    Ok(Value::Int((used.live + used.history + used.log) as i64))
}

/// Whether `name` is a database: the session's world, another world, or one of the placeholders
/// `pg_database` lists besides them.
fn database_exists(ctx: &Ctx, name: &str) -> bool {
    name == ctx.home || matches!(name, "postgres" | "template0" | "template1") || ctx.core.view_tx(name).is_ok()
}
