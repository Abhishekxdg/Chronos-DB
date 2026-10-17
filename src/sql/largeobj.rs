//! Large objects: `lo_creat`, `lo_open`, `loread`, `lowrite`, `lo_lseek`, `lo_unlink`, `lo_get`, `lo_put`, ... Each is a
//! length row under `_sonos_lo/` and its bytes in 64 KiB chunk rows under it (hex), so it forks and merges with the
//! data, and a read or write touches only the chunks it covers. Like Postgres's the descriptors belong to the session;
//! the writes are the transaction's. (An object an earlier version stored whole, as hex in its one row, is read as it
//! was and made chunks when it is next changed.)

use super::*;

const PREFIX: &str = "_sonos_lo/";
const CHUNK: u64 = 64 << 10;
const READ: i64 = 0x40000;
const WRITE: i64 = 0x20000;

/// An open descriptor: the object, whether it may be written, and where it is.
#[derive(Clone, Copy)]
pub(super) struct Open {
    oid: i64,
    write: bool,
    pos: i64,
}

fn key(oid: i64) -> String {
    format!("{PREFIX}{oid:012}")
}

fn chunk_key(oid: i64, i: u64) -> String {
    format!("{PREFIX}{oid:012}/{i:08}")
}

fn bad(oid: i64) -> SqlError {
    SqlError { code: "42704", message: format!("large object {oid} does not exist"), info: None }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

/// An object as stored: chunked, with its length; or whole (the earlier form).
enum Obj {
    Chunked(u64),
    Whole(Vec<u8>),
}

impl Obj {
    fn len(&self) -> u64 {
        match self {
            Obj::Chunked(n) => *n,
            Obj::Whole(d) => d.len() as u64,
        }
    }
}

type Rows = Vec<(String, Option<String>)>;

/// Whether large object `oid` is there (Postgres's LargeObjectExists).
pub(super) fn exists(v: &View, oid: i64) -> Result<bool> {
    Ok(v.get(&key(oid))?.is_some())
}

fn stat(v: &View, oid: i64) -> Result<Option<Obj>> {
    Ok(v.get(&key(oid))?.map(|raw| match raw.strip_prefix('L') {
        Some(n) => Obj::Chunked(n.parse().unwrap_or(0)),
        None => Obj::Whole(unhex(&raw)),
    }))
}

/// `len` bytes of the object from `off` (fewer at its end); a chunk never written reads as zeros.
fn read(v: &View, oid: i64, obj: &Obj, off: u64, len: u64) -> Result<Vec<u8>> {
    let total = obj.len();
    let (from, to) = (off.min(total), off.saturating_add(len).min(total));
    if let Obj::Whole(d) = obj {
        return Ok(d[from as usize..to as usize].to_vec());
    }
    let mut out = vec![0u8; (to - from) as usize];
    if to > from {
        for i in from / CHUNK..=(to - 1) / CHUNK {
            let Some(raw) = v.get(&chunk_key(oid, i))? else { continue };
            let data = unhex(&raw);
            let base = i * CHUNK;
            let (lo, hi) = (from.max(base), to.min(base + data.len() as u64));
            if hi > lo {
                out[(lo - from) as usize..(hi - from) as usize]
                    .copy_from_slice(&data[(lo - base) as usize..(hi - base) as usize]);
            }
        }
    }
    Ok(out)
}

/// The rows that make the object `data` from `off` over what it holds (grown to hold it, a gap zeros).
fn written(v: &View, oid: i64, obj: &Obj, off: u64, data: &[u8]) -> Result<Rows> {
    use std::collections::BTreeMap;
    let mut chunks: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    if let Obj::Whole(d) = obj {
        // (the earlier form: every chunk of it is written)
        for (i, c) in d.chunks(CHUNK as usize).enumerate() {
            chunks.insert(i as u64, c.to_vec());
        }
    }
    let end = off + data.len() as u64;
    if !data.is_empty() {
        for i in off / CHUNK..=(end - 1) / CHUNK {
            let base = i * CHUNK;
            let mut c = match chunks.remove(&i) {
                Some(c) => c,
                None => v.get(&chunk_key(oid, i))?.map(|raw| unhex(&raw)).unwrap_or_default(),
            };
            let (lo, hi) = (off.max(base), end.min(base + CHUNK));
            if c.len() < (hi - base) as usize {
                c.resize((hi - base) as usize, 0);
            }
            c[(lo - base) as usize..(hi - base) as usize]
                .copy_from_slice(&data[(lo - off) as usize..(hi - off) as usize]);
            chunks.insert(i, c);
        }
    }
    let mut rows: Rows = chunks.into_iter().map(|(i, c)| (chunk_key(oid, i), Some(hex(&c)))).collect();
    rows.push((key(oid), Some(format!("L{}", obj.len().max(end)))));
    Ok(rows)
}

/// The rows that make the object `len` long: chunks past it gone, the last cut to fit; longer, it reads zeros there.
fn truncated(v: &View, oid: i64, obj: &Obj, len: u64) -> Result<Rows> {
    if let Obj::Whole(d) = obj {
        let mut d = d.clone();
        d.resize(len as usize, 0);
        let whole = Obj::Chunked(0);
        return written(v, oid, &whole, 0, &d);
    }
    let mut rows: Rows = vec![(key(oid), Some(format!("L{len}")))];
    if len < obj.len() {
        for r in v.prefix(&format!("{PREFIX}{oid:012}/"))? {
            let (k, raw) = r?;
            let i: u64 = k.rsplit('/').next().and_then(|n| n.parse().ok()).unwrap_or(0);
            if i * CHUNK >= len {
                rows.push((k, None));
            } else if (i + 1) * CHUNK > len {
                let mut c = unhex(&raw);
                c.truncate((len - i * CHUNK) as usize);
                rows.push((k, Some(hex(&c))));
            }
        }
    }
    Ok(rows)
}

/// The rows of a new object holding `data`.
fn made(oid: i64, data: &[u8]) -> Rows {
    let mut rows: Rows =
        data.chunks(CHUNK as usize).enumerate().map(|(i, c)| (chunk_key(oid, i as u64), Some(hex(c)))).collect();
    rows.push((key(oid), Some(format!("L{}", data.len()))));
    rows
}

fn exists_ctx(ctx: &Ctx, oid: i64) -> Result<bool> {
    exists_in(&ctx.core.view_tx(&ctx.world)?, oid)
}

/// Whether large object `oid` is in world `v` (GRANT ... ON LARGE OBJECT looks it up).
pub(super) fn exists_in(v: &View, oid: i64) -> Result<bool> {
    Ok(v.get(&key(oid))?.is_some())
}

/// Whether `lo_compat_privileges` is on: large objects are then read, written and unlinked without their
/// privileges checked, as before Postgres 9.0.
fn compat() -> bool {
    roles::setting_now("lo_compat_privileges").is_some_and(|s| !rls::setting_off(&s))
}

/// The running role, when large objects' privileges apply to it: not a superuser, not with
/// `lo_compat_privileges` on, and in a database that has roles.
fn checked(ctx: &Ctx) -> Option<Arc<roles::Who>> {
    roles::current().filter(|w| !w.superuser && ctx.core.has_privileges() && !compat())
}

/// 42501 unless the running role holds `privs` (acl::SELECT, acl::UPDATE) on large object `oid`, as Postgres's
/// inv_open checks them when it is opened: `permission denied for large object 1002`.
fn may(ctx: &Ctx, oid: i64, privs: u32) -> Result<()> {
    let Some(who) = checked(ctx) else { return Ok(()) };
    let o = acl::large_object(&ctx.core.view_tx(&ctx.world)?, oid)?;
    if o.has(&who, privs) { Ok(()) } else { Err(o.denied()) }
}

/// The row that makes the running role the owner of new large object `oid` (none where the database has no roles).
fn owned(ctx: &Ctx, oid: i64) -> Option<(String, Option<String>)> {
    let who = roles::current().filter(|_| ctx.core.has_privileges())?;
    let e = acl::Entry { owner: Some(who.oid), acl: None };
    Some(acl::write_entry(&acl::key_of(acl::Kind::LargeObject, &oid.to_string()), &e))
}

/// lo_import and lo_export reach the server's files: Postgres grants them to no one but superusers.
/// An agent's session (the guest's too) is never one here, whatever its login's identity says (an
/// agent that is no role runs as the bootstrap superuser), nor is a call outside a session's
/// statement; and the server must let anyone (see `copy::server_files_allowed`).
fn superuser_only(name: &str) -> Result<()> {
    if acl::agents() || !roles::current().is_some_and(|w| w.superuser) || !super::copy::server_files_allowed() {
        return err("42501", format!("permission denied for function {name}"));
    }
    Ok(())
}

fn put_rows(ctx: &Ctx, rows: Rows) -> Result<()> {
    // (inside a write the gate is held already)
    ctx.core.batch_in(&ctx.world, ctx.direct, |_| Ok::<_, SqlError>((rows, ()))).map(|_| ())
}

/// Changes object `oid` by the rows `f` gives from it.
fn change(ctx: &Ctx, oid: i64, f: impl FnOnce(&View, &Obj) -> Result<Rows>) -> Result<()> {
    ctx.core
        .batch_in(&ctx.world, ctx.direct, |v| {
            let obj = stat(v, oid)?.ok_or_else(|| bad(oid))?;
            Ok::<_, SqlError>((f(v, &obj)?, ()))
        })
        .map(|_| ())
}

/// The object whole (what lo_get and lo_export give), and its size.
fn load(ctx: &Ctx, oid: i64) -> Result<Option<Vec<u8>>> {
    let v = ctx.core.view_tx(&ctx.world)?;
    let Some(obj) = stat(&v, oid)? else { return Ok(None) };
    read(&v, oid, &obj, 0, obj.len()).map(Some)
}

fn size(ctx: &Ctx, oid: i64) -> Result<u64> {
    let v = ctx.core.view_tx(&ctx.world)?;
    stat(&v, oid)?.map(|o| o.len()).ok_or_else(|| bad(oid))
}

fn int(v: &Value) -> Result<i64> {
    match coerce(v.clone(), Type::BigInt)? {
        Value::Int(n) => Ok(n),
        _ => err("22004", "a null argument"),
    }
}

fn bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Bytea(b) => b.clone(),
        v => v.to_text().into_bytes(),
    }
}

/// The next unused oid.
fn fresh(ctx: &Ctx) -> Result<i64> {
    let v = ctx.core.view_tx(&ctx.world)?;
    let mut top = 16384;
    for r in v.prefix(PREFIX)? {
        let (k, _) = r?;
        top = top.max(k[PREFIX.len()..].get(..12).and_then(|n| n.parse::<i64>().ok()).unwrap_or(0));
    }
    Ok(top + 1)
}

pub(super) fn call(ctx: &Ctx, name: &str, a: &[Value]) -> Result<Value> {
    use Value::{Bytea, Int, Null};
    if a.contains(&Null) && !matches!(name, "lo_get" | "lo_put") {
        return Ok(Null);
    }
    if super::xmlmap::handles(name) {
        return super::xmlmap::call(ctx, name, a);
    }
    let fd = |ctx: &Ctx, n: i64| -> Result<Open> {
        let st = ctx.state.lock().unwrap();
        st.los.get(&n).copied().ok_or_else(|| SqlError {
            code: "42704",
            message: format!("invalid large-object descriptor: {n}"),
            info: None,
        })
    };
    let set = |ctx: &Ctx, n: i64, o: Open| {
        ctx.state.lock().unwrap().los.insert(n, o);
    };
    let writable = |o: &Open, n: i64| -> Result<()> {
        if o.write { Ok(()) } else { err("55000", format!("large object descriptor {n} was not opened for writing")) }
    };
    Ok(match name {
        "lo_creat" | "lo_create" | "lo_from_bytea" => {
            let given = if name == "lo_creat" { 0 } else { int(&a[0])? };
            let oid = if given == 0 { fresh(ctx)? } else { given };
            if name != "lo_creat" && exists_ctx(ctx, oid)? {
                return err("42710", format!("large object {oid} already exists"));
            }
            let mut rows = made(oid, &if name == "lo_from_bytea" { bytes(&a[1]) } else { vec![] });
            rows.extend(owned(ctx, oid));
            put_rows(ctx, rows)?;
            Value::Int(oid)
        }
        "lo_open" => {
            let (oid, mode) = (int(&a[0])?, int(&a[1])?);
            if !exists_ctx(ctx, oid)? {
                return Err(bad(oid));
            }
            // (opened for writing it is read too: SELECT, and UPDATE, as the mode asks)
            let need = match (mode & WRITE != 0, mode & READ != 0) {
                (true, _) => acl::SELECT | acl::UPDATE,
                (false, true) => acl::SELECT,
                (false, false) => return err("22023", format!("invalid flags for opening a large object: {mode}")),
            };
            may(ctx, oid, need)?;
            let mut st = ctx.state.lock().unwrap();
            let n = (0..).find(|n| !st.los.contains_key(n)).unwrap_or(0);
            st.los.insert(n, Open { oid, write: mode & WRITE != 0, pos: 0 });
            Int(n)
        }
        "lo_close" => {
            let n = int(&a[0])?;
            fd(ctx, n)?;
            ctx.state.lock().unwrap().los.remove(&n);
            Int(0)
        }
        "loread" => {
            let (n, len) = (int(&a[0])?, int(&a[1])?.max(0) as u64);
            let mut o = fd(ctx, n)?;
            let v = ctx.core.view_tx(&ctx.world)?;
            let obj = stat(&v, o.oid)?.ok_or_else(|| bad(o.oid))?;
            let got = read(&v, o.oid, &obj, o.pos.max(0) as u64, len)?;
            o.pos = o.pos.max(0) + got.len() as i64;
            set(ctx, n, o);
            Bytea(got)
        }
        "lowrite" => {
            let (n, data) = (int(&a[0])?, bytes(&a[1]));
            let mut o = fd(ctx, n)?;
            writable(&o, n)?;
            let at = o.pos.max(0) as u64;
            change(ctx, o.oid, |v, obj| written(v, o.oid, obj, at, &data))?;
            o.pos = (at + data.len() as u64) as i64;
            set(ctx, n, o);
            Int(data.len() as i64)
        }
        "lo_lseek" | "lo_lseek64" => {
            let (n, off, whence) = (int(&a[0])?, int(&a[1])?, int(&a[2])?);
            let mut o = fd(ctx, n)?;
            o.pos = match whence {
                0 => off,
                1 => o.pos + off,
                2 => size(ctx, o.oid)? as i64 + off,
                w => return err("22023", format!("invalid whence setting: {w}")),
            };
            if o.pos < 0 {
                return err("22023", "invalid seek position");
            }
            set(ctx, n, o);
            Int(o.pos)
        }
        "lo_tell" | "lo_tell64" => Int(fd(ctx, int(&a[0])?)?.pos),
        "lo_truncate" | "lo_truncate64" => {
            let (n, len) = (int(&a[0])?, int(&a[1])?.max(0) as u64);
            let o = fd(ctx, n)?;
            writable(&o, n)?;
            change(ctx, o.oid, |v, obj| truncated(v, o.oid, obj, len))?;
            Int(0)
        }
        "lo_unlink" => {
            let oid = int(&a[0])?;
            let v = ctx.core.view_tx(&ctx.world)?;
            if stat(&v, oid)?.is_none() {
                return Err(bad(oid));
            }
            // (only its owner may, as Postgres's lo_unlink checks)
            if let Some(who) = checked(ctx) {
                let o = acl::large_object(&v, oid)?;
                if !who.has_privs_of(o.owner) {
                    return Err(o.not_owner());
                }
            }
            let mut rows: Rows = vec![(key(oid), None)];
            if ctx.core.has_privileges() {
                rows.push((acl::row_key(&acl::key_of(acl::Kind::LargeObject, &oid.to_string())), None));
            }
            for r in v.prefix(&format!("{PREFIX}{oid:012}/"))? {
                rows.push((r?.0, None));
            }
            put_rows(ctx, rows)?;
            Int(1)
        }
        "lo_get" => {
            if a[0] == Null {
                return Ok(Null);
            }
            let oid = int(&a[0])?;
            let v = ctx.core.view_tx(&ctx.world)?;
            let obj = stat(&v, oid)?.ok_or_else(|| bad(oid))?;
            may(ctx, oid, acl::SELECT)?;
            let (off, len) = match a.len() {
                3 => (int(&a[1])?.max(0) as u64, int(&a[2])?.max(0) as u64),
                _ => (0, obj.len()),
            };
            Bytea(read(&v, oid, &obj, off, len)?)
        }
        "lo_put" => {
            let (oid, off) = (int(&a[0])?, int(&a[1])?.max(0) as u64);
            let data = bytes(&a[2]);
            // (it is opened for writing: SELECT and UPDATE)
            if !exists_ctx(ctx, oid)? {
                return Err(bad(oid));
            }
            may(ctx, oid, acl::SELECT | acl::UPDATE)?;
            change(ctx, oid, |v, obj| written(v, oid, obj, off, &data))?;
            Value::Text(String::new()) // void
        }
        "lo_import" => {
            superuser_only(name)?;
            let path = a[0].to_text();
            let data = std::fs::read(&path).map_err(|e| SqlError {
                code: "58P01",
                message: format!("could not open server file \"{path}\": {e}"),
                info: None,
            })?;
            let oid = match a.get(1) {
                Some(v) if int(v)? != 0 => int(v)?,
                _ => fresh(ctx)?,
            };
            let mut rows = made(oid, &data);
            rows.extend(owned(ctx, oid));
            put_rows(ctx, rows)?;
            Int(oid)
        }
        "lo_export" => {
            superuser_only(name)?;
            let oid = int(&a[0])?;
            let data = load(ctx, oid)?.ok_or_else(|| bad(oid))?;
            let path = a[1].to_text();
            std::fs::write(&path, data).map_err(|e| SqlError {
                code: "58P01",
                message: format!("could not create server file \"{path}\": {e}"),
                info: None,
            })?;
            Int(1)
        }
        _ => return err(UNSUPPORTED, format!("{name}() isn't supported")),
    })
}

pub(super) const NAMES: &[(&str, std::ops::RangeInclusive<usize>)] = &[
    ("query_to_xml", 4..=4),
    ("table_to_xml", 4..=4),
    ("query_to_xmlschema", 4..=4),
    ("table_to_xmlschema", 4..=4),
    ("schema_to_xml", 4..=4),
    ("schema_to_xmlschema", 4..=4),
    ("query_to_xml_and_xmlschema", 4..=4),
    ("table_to_xml_and_xmlschema", 4..=4),
    ("schema_to_xml_and_xmlschema", 4..=4),
    ("cursor_to_xml", 5..=5),
    ("cursor_to_xmlschema", 4..=4),
    ("lo_creat", 1..=1),
    ("lo_create", 1..=1),
    ("lo_from_bytea", 2..=2),
    ("lo_open", 2..=2),
    ("lo_close", 1..=1),
    ("loread", 2..=2),
    ("lowrite", 2..=2),
    ("lo_lseek", 3..=3),
    ("lo_lseek64", 3..=3),
    ("lo_tell", 1..=1),
    ("lo_tell64", 1..=1),
    ("lo_truncate", 2..=2),
    ("lo_truncate64", 2..=2),
    ("lo_unlink", 1..=1),
    ("lo_get", 1..=3),
    ("lo_put", 3..=3),
    ("lo_import", 1..=2),
    ("lo_export", 2..=2),
];

/// Whether a call of `name` changes something: a large object (the world's rows), or a server file.
pub(super) fn writes(name: &str) -> bool {
    matches!(
        name,
        "lo_creat"
            | "lo_create"
            | "lo_from_bytea"
            | "lowrite"
            | "lo_truncate"
            | "lo_truncate64"
            | "lo_unlink"
            | "lo_put"
            | "lo_import"
            | "lo_export"
    )
}

/// What a call of `name` gives.
pub(super) fn result(name: &str) -> Type {
    match name {
        n if super::xmlmap::handles(n) => Type::Xml,
        "lo_creat" | "lo_create" | "lo_from_bytea" | "lo_import" => Type::Oid,
        "loread" | "lo_get" => Type::Bytea,
        "lo_put" => Type::Void,
        _ => Type::Int,
    }
}
