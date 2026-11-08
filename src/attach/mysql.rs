//! Attached to MySQL (or MariaDB) instead of Postgres: `chronos attach <dir> --tables a,b` with CHRONOS_SOURCE_URL
//! `mysql://...`. The same promise as with Postgres: main is a copy of the tables; a merge into main is written to
//! MySQL first, in one InnoDB transaction, after checking each row it writes is still as main has it (else a
//! conflict, and nothing is written); a crash around COMMIT is settled from `chronos_merges`, MySQL's copy of
//! `chronos.merges`; `chronos refresh` brings main up to MySQL.
//!
//! A write-back is three round trips, whatever its size up to 4 MB of SQL a trip (see `Client::query_all`):
//!
//!   1. START TRANSACTION; the folder's marker row; each table's rows SELECTed FOR UPDATE by key
//!      (compared here, by type: see `same`)
//!   2. the UPDATEs, INSERTs and DELETEs; the rows read back as MySQL stored them; the merge's row
//!      (the counts checked here; the pending record written)
//!   3. COMMIT
//!
//! Not yet for MySQL (each refused, saying so): `serve --follow`, dry runs and rehearsals, CHECK READS and merge
//! checks run in the source, write-back rules, `pg_role`, `--lazy-tenant`, binary, spatial and generated columns,
//! and inserts into tables keyed by AUTO_INCREMENT (updates and deletes go).

use super::*;
use crate::myclient::{self, Client as My, Done, lit, my_error, q as mq};

/// The SQL one round trip carries at most; more goes in another (each is still one transaction).
const TRIP_BYTES: usize = 4 << 20;
/// Rows one statement locks, reads or deletes by key at most.
const KEYS: usize = 1000;

/// What every connection sets first: UTC (DATETIME and TIMESTAMP read and written as main holds them), READ
/// COMMITTED as Postgres's write-back, errors rather than values silently cut or changed.
const SESSION: &str = "set session time_zone = '+00:00'; set session transaction isolation level read committed; \
     set session innodb_lock_wait_timeout = 10; \
     set session sql_mode = trim(both ',' from concat(@@session.sql_mode, ',STRICT_ALL_TABLES'))";

/// The marker and merge table in MySQL: `chronos.merges` as MySQL holds it, in the attached database.
const MERGES: &str = "chronos_merges";

const CHANGED: &str = super::CHANGED_IN_MYSQL;

/// The source's connection string: a MySQL one, or Err saying the folder needs one.
fn url() -> std::result::Result<myclient::Url, String> {
    myclient::parse(&super::source_text()?)
}

fn open(url: &myclient::Url) -> std::result::Result<My, String> {
    let mut c = My::connect(url).map_err(|e| format!("MySQL at {}:{}: {e}", url.host, url.port))?;
    c.query(SESSION).map_err(|e| format!("MySQL at {}:{}: {e}", url.host, url.port))?;
    Ok(c)
}

/// The kept connection, or a new one (and whether it was kept: its first use may find it closed meanwhile).
fn lease(att: &Attached) -> std::result::Result<(My, bool), String> {
    if let Some(c) = att.my.lock().unwrap_or_else(|p| p.into_inner()).take()
        && !c.broken
    {
        return Ok((c, true));
    }
    Ok((open(&url()?)?, false))
}

/// Keeps `c` for the next use, if it is good and no transaction is left open on it.
fn put(att: &Attached, mut c: My) {
    if c.in_tx {
        let _ = c.query("rollback");
    }
    if !c.broken && !c.in_tx {
        *att.my.lock().unwrap_or_else(|p| p.into_inner()) = Some(c);
    }
}

/// A MySQL failure as the merge's error, as `super::failed` says a Postgres one.
fn failed(what: &str, e: &io::Error) -> Error {
    let (code, retry) = match my_error(e) {
        Some(m) => (
            match m.code {
                1062 | 1586 => "23505",
                1216 | 1217 | 1451 | 1452 => "23503",
                1048 | 1364 => "23502",
                3819 => "23514",
                1406 => "22001",
                1264 | 1690 => "22003",
                1292 | 1366 | 1411 => "22P02",
                1044 | 1045 | 1142 | 1143 => "42501",
                _ => "23000",
            },
            matches!(m.code, 1205 | 1213 | 1317 | 3024 | 1927),
        ),
        None => ("08006", true),
    };
    let again = if retry { "; nothing was written: safe to merge again" } else { "; nothing was written" };
    Error::Constraint { code: if retry { "40001" } else { "23000" }, message: format!("MySQL, {what}: {e}{again}") }
        .with_code(code)
}

// ---------------------------------------------------------------- values, compared by type

/// The kind of a MySQL column, by its COLUMN_TYPE (`int unsigned`, `decimal(10,2)`, `datetime(6)`), for comparing
/// and casting its values.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Int,
    Unsigned,
    Decimal,
    Float,
    Date,
    DateTime,
    Json,
    Text,
}

fn kind(ty: &str) -> Kind {
    let t = ty.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    match base {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" if t.contains("unsigned") => {
            Kind::Unsigned
        }
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" | "bool" | "boolean" => Kind::Int,
        "decimal" | "numeric" | "dec" | "fixed" => Kind::Decimal,
        "float" | "double" | "real" => Kind::Float,
        "date" => Kind::Date,
        "datetime" | "timestamp" => Kind::DateTime,
        "json" => Kind::Json,
        _ => Kind::Text,
    }
}

/// A value of column type `ty` in one form whatever wrote it (MySQL's text, or Chronos's of the type it maps to):
/// integers and decimals without leading or trailing zeros, timestamps without trailing fractional zeros.
fn canon(ty: &str, v: &str) -> String {
    match kind(ty) {
        Kind::Int | Kind::Unsigned => v.trim().parse::<i128>().map_or_else(|_| v.to_string(), |n| n.to_string()),
        Kind::Decimal => {
            let v = v.trim();
            let (neg, digits) = match v.strip_prefix('-') {
                Some(d) => (true, d),
                None => (false, v.strip_prefix('+').unwrap_or(v)),
            };
            let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
            let int = int.trim_start_matches('0');
            let frac = frac.trim_end_matches('0');
            let body = match (int.is_empty(), frac.is_empty()) {
                (true, true) => return "0".into(),
                (_, true) => int.to_string(),
                (true, false) => format!("0.{frac}"),
                (false, false) => format!("{int}.{frac}"),
            };
            if neg { format!("-{body}") } else { body }
        }
        Kind::Float => v.trim().parse::<f64>().map_or_else(|_| v.to_string(), |f| format!("{f:?}")),
        Kind::DateTime => {
            let v = v.trim().replacen('T', " ", 1);
            let v = v.strip_suffix("+00").or_else(|| v.strip_suffix("+00:00")).map(String::from).unwrap_or(v);
            match v.split_once('.') {
                Some((s, f)) => match f.trim_end_matches('0') {
                    "" => s.to_string(),
                    f => format!("{s}.{f}"),
                },
                None => v,
            }
        }
        Kind::Json => serde_json::from_str::<Json>(v).map_or_else(|_| v.to_string(), |j| j.to_string()),
        Kind::Date | Kind::Text => v.to_string(),
    }
}

/// Whether two values of column type `ty` are the same value (see `canon`).
fn same(ty: &str, a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b || canon(ty, a) == canon(ty, b),
        _ => false,
    }
}

/// A row's key, in the one form `canon` gives each part: how rows read from MySQL and main's are matched.
fn key_of(t: &Table, row: &[Option<String>]) -> Vec<Option<String>> {
    t.key.iter().map(|&i| row[i].as_deref().map(|v| canon(&t.cols[i].ty, v))).collect()
}

/// `v` as a value of column `i` of `t` in MySQL's SQL: cast to its type, so a key is looked up by its index.
fn typed(t: &Table, i: usize, v: Option<&str>) -> String {
    let ty = t.cols[i].ty.to_ascii_lowercase();
    let l = lit(v);
    if v.is_none() {
        return l;
    }
    match kind(&ty) {
        Kind::Int => format!("cast({l} as signed)"),
        Kind::Unsigned => format!("cast({l} as unsigned)"),
        Kind::Decimal => {
            let args = ty.split_once('(').and_then(|(_, a)| a.split_once(')')).map_or("65,30", |(a, _)| a);
            format!("cast({l} as decimal({args}))")
        }
        Kind::Float => format!("cast({l} as double)"),
        Kind::Date => format!("cast({l} as date)"),
        Kind::DateTime => {
            let fsp = ty.split_once('(').and_then(|(_, a)| a.split_once(')')).map_or("0", |(a, _)| a);
            format!("cast({l} as datetime({fsp}))")
        }
        Kind::Json | Kind::Text => l,
    }
}

/// `where` for the rows of `t` whose keys are `keys` (as main writes them).
fn by_keys(t: &Table, keys: &[Vec<Option<String>>]) -> String {
    let cols: Vec<String> = t.key.iter().map(|&i| mq(&t.cols[i].name)).collect();
    let vals: Vec<String> = keys
        .iter()
        .map(|k| {
            let parts: Vec<String> = t.key.iter().zip(k).map(|(&i, v)| typed(t, i, v.as_deref())).collect();
            format!("({})", parts.join(", "))
        })
        .collect();
    format!("({}) in ({})", cols.join(", "), vals.join(", "))
}

fn select_cols(t: &Table) -> String {
    t.cols.iter().map(|c| mq(&c.name)).collect::<Vec<_>>().join(", ")
}

/// Statements sent a trip of at most `TRIP_BYTES` at a time, in order, on `c`: every outcome, or the first failure
/// with what it was.
fn send(c: &mut My, stmts: &[(String, String)]) -> Result<Vec<Done>> {
    let mut out = vec![];
    let mut i = 0;
    while i < stmts.len() {
        let mut sql = String::new();
        let start = i;
        while i < stmts.len() && (i == start || sql.len() + stmts[i].1.len() < TRIP_BYTES) {
            if !sql.is_empty() {
                sql.push_str(";\n");
            }
            sql.push_str(&stmts[i].1);
            i += 1;
        }
        let (done, bad) = c.query_all(&sql).map_err(|e| failed(&stmts[start].0, &e))?;
        if let Some((at, e)) = bad {
            return Err(failed(&stmts[start + at].0, &e));
        }
        out.extend(done);
    }
    Ok(out)
}

// ---------------------------------------------------------------- the write-back

#[allow(clippy::too_many_arguments)]
pub(super) fn write_back(
    att: &Attached,
    world: &str,
    world_id: &str,
    rows: &[Row],
    reads: Option<&Reads>,
    checks: &[crate::checks::MergeCheck],
    who: Option<&ActAs>,
    rules: &Rules,
) -> Result<Option<Held>> {
    let no = |what: &str| {
        Err(refused(format!(
            "{what} works with a Postgres source only for now, and this folder is attached to MySQL ({}): nothing was \
             merged",
            att.source()
        )))
    };
    if who.is_some() {
        return no("writing as the end user's role (pg_role)");
    }
    if rules.observe || rules.serializable || rules.has_tables() {
        return no("write-back rules (writeback.json)");
    }
    if reads.is_some() {
        return no("checking a world's reads against the source (check_reads)");
    }
    if !checks.is_empty() {
        return no("running merge checks in the source");
    }
    let (mut c, kept) = lease(att).map_err(|e| refused(format!("{e}; nothing was merged")))?;
    let out = transaction(att, &mut c, kept, world, world_id, rows);
    if out.is_err() && !c.broken && c.in_tx {
        let _ = c.query("rollback");
    }
    put(att, c);
    out.map(|()| None)
}

fn transaction(att: &Attached, c: &mut My, kept: bool, world: &str, world_id: &str, rows: &[Row]) -> Result<()> {
    let tables = att.tables();
    let by_table = by_table(rows);
    // 1. the transaction, the folder's marker, and every row it writes locked and read (one round trip)
    let mut first = vec![
        ("starting the write-back".to_string(), "start transaction".to_string()),
        (
            "reading chronos_merges".to_string(),
            format!("select count(*) from {MERGES} where id = {}", lit(Some(&att.marker()))),
        ),
    ];
    let mut reads: Vec<usize> = vec![]; // the table each read after the first two is of
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let keys: Vec<Vec<Option<String>>> = rs.iter().map(|r| r.key_texts(t)).collect();
        for part in keys.chunks(KEYS) {
            first.push((
                format!("locking rows of {}", t.name),
                format!("select {} from {} where {} for update", select_cols(t), t.pg, by_keys(t, part)),
            ));
            reads.push(ti);
        }
    }
    let got = match send(c, &first) {
        // (a kept connection MySQL closed while unused: once more on a new one, as nothing was written)
        Err(_) if kept && c.broken => {
            *c = open(&url().map_err(refused)?).map_err(|e| refused(format!("{e}; nothing was merged")))?;
            send(c, &first)?
        }
        other => other?,
    };
    if got.get(1).and_then(|d| d.rows.first()).and_then(|r| r[0].as_deref()) != Some("1") {
        return Err(refused(format!(
            "the MySQL CHRONOS_SOURCE_URL names isn't the one this folder was attached to ({}): its chronos_merges \
             has no row {}; nothing was merged",
            att.source(),
            att.marker()
        )));
    }
    let mut now: HashMap<(usize, Vec<Option<String>>), Texts> = HashMap::new();
    for (d, &ti) in got[2..].iter().zip(&reads) {
        for r in &d.rows {
            now.insert((ti, key_of(&tables[ti], r)), r.clone());
        }
    }
    // compared: a row MySQL changed (or added, or deleted) since main had it is a conflict, and nothing is written
    let mut conflicts: Vec<(usize, &Row, Option<Texts>, Texts)> = vec![];
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        for r in rs {
            let key = r.key_texts(t);
            let theirs = now.get(&(ti, key_of_key(t, &key))).cloned();
            let changed = match (&r.before, &theirs) {
                (None, None) => false,
                (Some(b), Some(n)) => {
                    t.cols.iter().enumerate().any(|(i, col)| !same(&col.ty, b[i].as_deref(), n[i].as_deref()))
                }
                _ => true,
            };
            if changed {
                let key = theirs.as_ref().map_or(key.clone(), |n| t.key.iter().map(|&i| n[i].clone()).collect());
                conflicts.push((ti, r, theirs, key));
            }
        }
    }
    if !conflicts.is_empty() {
        let _ = c.query("rollback");
        let mut lines = vec![];
        let mut stored = vec![];
        for (ti, row, theirs, key) in &conflicts {
            let t = &tables[*ti];
            let show = |r: &Option<Vec<Option<String>>>| r.as_ref().map_or("no row".to_string(), |r| row_json(t, r));
            if lines.len() < 10 {
                lines.push(format!(
                    "  {}: the world's {}, main's {}, MySQL's {}",
                    crate::sql::show_key(&row.key),
                    show(&row.after),
                    show(&row.before),
                    show(theirs)
                ));
            }
            stored.push(Stored { table: *ti, key: key.clone(), row: theirs.clone() });
        }
        if conflicts.len() > lines.len() {
            lines.push(format!("  ... and {} more", conflicts.len() - lines.len()));
        }
        AFTER.set(Some(After { rows: stored, committed: false, refresh: vec![] }));
        return Err(Error::Constraint {
            code: "40001",
            message: format!(
                "{} {world} changes {CHANGED} (attached {}); nothing was written to MySQL and nothing merged. main \
                 now holds MySQL's values for them: merge again, and settle them as any conflict (OURS, THEIRS, or \
                 row by row).\n{}",
                crate::agents::n_rows(conflicts.len() as u64),
                att.source(),
                lines.join("\n")
            ),
        });
    }
    // 2. the writes, the rows read back as MySQL stored them, and the merge's own row (one round trip)
    let merge_id = format!("merge:{}", random_id());
    let mut writes: Vec<(String, String)> = vec![];
    // (what each statement's affected rows must be; None: a read back, of that table)
    let mut expect: Vec<(Option<usize>, usize, &'static str)> = vec![];
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        for r in rs.iter().filter(|r| r.before.is_some() && r.after.is_some()) {
            let after = r.after.as_ref().unwrap();
            let sets: Vec<String> = (0..t.cols.len())
                .filter(|i| !t.key.contains(i))
                .map(|i| format!("{} = {}", mq(&t.cols[i].name), lit(after[i].as_deref())))
                .collect();
            if sets.is_empty() {
                continue;
            }
            writes.push((
                format!("update {}", t.name),
                format!("update {} set {} where {}", t.pg, sets.join(", "), by_keys(t, &[r.key_texts(t)])),
            ));
            expect.push((Some(1), ti, "updated"));
        }
        let inserts: Vec<&&Row> = rs.iter().filter(|r| r.before.is_none()).collect();
        for part in inserts.chunks(KEYS) {
            let unseen = &part[0].unseen;
            let cols: Vec<usize> = (0..t.cols.len()).filter(|i| !unseen.contains(i)).collect();
            let names: Vec<String> = cols.iter().map(|&i| mq(&t.cols[i].name)).collect();
            let vals: Vec<String> = part
                .iter()
                .map(|r| {
                    let a = r.after.as_ref().unwrap();
                    format!("({})", cols.iter().map(|&i| lit(a[i].as_deref())).collect::<Vec<_>>().join(", "))
                })
                .collect();
            writes.push((
                format!("insert into {}", t.name),
                format!("insert into {} ({}) values {}", t.pg, names.join(", "), vals.join(", ")),
            ));
            expect.push((Some(part.len()), ti, "inserted"));
        }
    }
    for (&ti, rs) in by_table.iter().rev() {
        let t = &tables[ti];
        let keys: Vec<Vec<Option<String>>> = rs.iter().filter(|r| r.after.is_none()).map(|r| r.key_texts(t)).collect();
        for part in keys.chunks(KEYS) {
            writes
                .push((format!("delete from {}", t.name), format!("delete from {} where {}", t.pg, by_keys(t, part))));
            expect.push((Some(part.len()), ti, "deleted"));
        }
    }
    // (MySQL has no RETURNING: the rows written read back, so main takes what defaults and triggers stored)
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let keys: Vec<Vec<Option<String>>> = rs.iter().filter(|r| r.after.is_some()).map(|r| r.after_key(t)).collect();
        for part in keys.chunks(KEYS) {
            writes.push((
                format!("reading back rows of {}", t.name),
                format!("select {} from {} where {}", select_cols(t), t.pg, by_keys(t, part)),
            ));
            expect.push((None, ti, ""));
        }
    }
    writes.push(("recording the merge".into(), format!("insert into {MERGES} (id) values ({})", lit(Some(&merge_id)))));
    expect.push((Some(1), usize::MAX, "recorded"));
    let done = send(c, &writes)?;
    let mut sent_back: Vec<Stored> = vec![];
    for (d, (n, ti, what)) in done.iter().zip(&expect) {
        match n {
            Some(n) if d.affected as usize != *n => {
                let t = tables.get(*ti).map_or(MERGES, |t| t.name.as_str());
                return Err(refused(format!(
                    "MySQL {what} {} rows of {t} where the merge sent {n}: nothing was merged",
                    d.affected
                )));
            }
            Some(_) => {}
            None => {
                for r in &d.rows {
                    let key = tables[*ti].key.iter().map(|&i| r[i].clone()).collect();
                    sent_back.push(Stored { table: *ti, key, row: Some(r.clone()) });
                }
            }
        }
    }
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        for r in rs.iter().filter(|r| r.after.is_none()) {
            sent_back.push(Stored { table: ti, key: r.key_texts(t), row: None });
        }
    }
    // what recovery needs should the commit's outcome be lost
    let mut touched: BTreeMap<String, Vec<Vec<Option<String>>>> = BTreeMap::new();
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        touched.entry(t.name.clone()).or_default().extend(rs.iter().map(|r| r.key_texts(t)));
    }
    let pending = json!({"merge": merge_id, "world": world, "world_id": world_id, "rows": touched, "others": []});
    crate::disk::write_file(&att.pending_path(), pending.to_string().as_bytes()).map_err(Error::Io)?;
    crash_point("before_commit");
    // 3. COMMIT
    let outcome = c.query("commit");
    crash_point("after_commit");
    match outcome {
        Ok(_) if !c.in_tx => {}
        Ok(_) => {
            let _ = std::fs::remove_file(att.pending_path());
            return Err(refused("MySQL didn't end the write-back's transaction at COMMIT; nothing was merged"));
        }
        Err(e) if my_error(&e).is_some() => {
            let _ = std::fs::remove_file(att.pending_path());
            return Err(failed("at COMMIT", &e));
        }
        Err(e) => match committed(&merge_id) {
            Ok(true) => {}
            Ok(false) => {
                let _ = std::fs::remove_file(att.pending_path());
                return Err(failed("the connection was lost at COMMIT, which didn't happen", &e));
            }
            Err(why) => {
                return Err(refused(format!(
                    "the connection to MySQL was lost during COMMIT ({e}), and whether it committed can't be told yet \
                     ({why}): merges into main wait until it can; {world} is as it was"
                )));
            }
        },
    }
    AFTER.set(Some(After { rows: sent_back, committed: true, refresh: vec![] }));
    Ok(())
}

impl Row {
    /// Its key as the merge leaves it (for a row it inserts or updates).
    fn after_key(&self, t: &Table) -> Vec<Option<String>> {
        let r = self.after.as_ref().or(self.before.as_ref()).expect("a row on one side at least");
        t.key.iter().map(|&i| r[i].clone()).collect()
    }
}

/// Whether the write-back that inserted `merge_id` committed (see `super::committed`): its row inserted again from a
/// new connection waits for the first transaction to end, then is a duplicate if it committed.
pub(super) fn committed(merge_id: &str) -> std::result::Result<bool, String> {
    let mut c = open(&url()?)?;
    let sql = format!(
        "set session innodb_lock_wait_timeout = 60; start transaction; insert into {MERGES} (id) values ({})",
        lit(Some(merge_id))
    );
    let out = match c.query_all(&sql) {
        Ok((_, None)) => Ok(false),
        Ok((_, Some((_, e)))) if my_error(&e).is_some_and(|m| m.code == 1062) => Ok(true),
        Ok((_, Some((_, e)))) | Err(e) => Err(e.to_string()),
    };
    let _ = c.query("rollback");
    out
}

/// The rows `touched` names (table name: keys), as MySQL has them now: for `resolve`.
pub(super) fn reread(
    att: &Attached,
    touched: &serde_json::Map<String, Json>,
) -> std::result::Result<Vec<Stored>, String> {
    let mut c = open(&url()?)?;
    let tables = att.tables();
    let mut out = vec![];
    for (name, keys) in touched {
        let Some(ti) = att.table(name) else { continue };
        let t = &tables[ti];
        let keys: Vec<Vec<Option<String>>> = serde_json::from_value(keys.clone()).unwrap_or_default();
        for part in keys.chunks(KEYS) {
            let sql = format!("select {} from {} where {}", select_cols(t), t.pg, by_keys(t, part));
            let found = c.query(&sql).map_err(|e| e.to_string())?;
            let mut by: HashMap<Vec<Option<String>>, Texts> = HashMap::new();
            for r in found.into_iter().flat_map(|d| d.rows) {
                by.insert(key_of(t, &r), r);
            }
            for k in part {
                let row = by.get(&key_of_key(t, k)).cloned();
                out.push(Stored { table: ti, key: k.clone(), row });
            }
        }
    }
    Ok(out)
}

/// `key_of`, of a key alone.
fn key_of_key(t: &Table, k: &[Option<String>]) -> Vec<Option<String>> {
    t.key.iter().zip(k).map(|(&i, v)| v.as_deref().map(|v| canon(&t.cols[i].ty, v))).collect()
}

// ---------------------------------------------------------------- refresh

/// `super::refresh` of a MySQL folder: each table read whole from MySQL in one consistent snapshot, compared with
/// main's rows by type (see `same`), main taking the differences in one change. A table whose columns changed in
/// MySQL is left as it was, and said: attach again (columns don't come over by a refresh yet, as with Postgres).
pub(super) fn refresh(core: &Core, att: &Attached, only: Option<&[usize]>) -> std::result::Result<Refreshed, String> {
    let (mut c, _) = lease(att)?;
    let out = refresh_read(core, att, &mut c, only);
    put(att, c);
    let (done, rows) = out?;
    store(core, att, &rows)?;
    if only.is_none() {
        att.refreshed(done.at).map_err(|e| e.to_string())?;
    }
    Ok(done)
}

fn refresh_read(
    core: &Core,
    att: &Attached,
    c: &mut My,
    only: Option<&[usize]>,
) -> std::result::Result<(Refreshed, Vec<Stored>), String> {
    let start = format!(
        "set transaction isolation level repeatable read; start transaction with consistent snapshot, read only; \
         select count(*) from {MERGES} where id = {}",
        lit(Some(&att.marker()))
    );
    let got = c.query(&start).map_err(|e| format!("MySQL, starting the refresh: {e}"))?;
    if got.get(2).and_then(|d| d.rows.first()).and_then(|r| r[0].as_deref()) != Some("1") {
        let _ = c.query("rollback");
        return Err(format!(
            "the MySQL CHRONOS_SOURCE_URL names isn't the one this folder was attached to ({}): nothing was refreshed",
            att.source()
        ));
    }
    let out = refresh_tables(core, att, c, only);
    let _ = c.query("rollback");
    out
}

fn refresh_tables(
    core: &Core,
    att: &Attached,
    c: &mut My,
    only: Option<&[usize]>,
) -> std::result::Result<(Refreshed, Vec<Stored>), String> {
    let mut out = Refreshed { at: crate::now_ms(), ..Default::default() };
    let mut rows = vec![];
    let tables = att.tables();
    let main = core.view(MAIN).map_err(|e| e.to_string())?;
    for (ti, t) in tables.iter().enumerate() {
        if only.is_some_and(|o| !o.contains(&ti)) {
            continue;
        }
        let shape = c
            .query(&format!(
                "select column_name, column_type from information_schema.columns where table_schema = database() and \
                 table_name = {} order by ordinal_position",
                lit(Some(&t.name))
            ))
            .map_err(|e| format!("MySQL, reading {}'s columns: {e}", t.name))?;
        let now: Vec<(String, String)> = shape
            .into_iter()
            .flat_map(|d| d.rows)
            .map(|r| (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default()))
            .collect();
        let was: Vec<(String, String)> = t.cols.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
        if now != was {
            let show =
                |v: &[(String, String)]| v.iter().map(|(n, t)| format!("{n} {t}")).collect::<Vec<_>>().join(", ");
            out.skipped.push(format!(
                "{}: changed in MySQL since the attach (its columns are ({}), not ({})). Attach it again into a new \
                 folder",
                t.name,
                show(&now),
                show(&was)
            ));
            continue;
        }
        // MySQL's rows, by key
        let mut theirs: HashMap<Vec<Option<String>>, Texts> = HashMap::new();
        c.query_each(&format!("select {} from {}", select_cols(t), t.pg), &mut |r| {
            theirs.insert(key_of(t, &r), r);
            Ok(())
        })
        .map_err(|e| format!("MySQL, reading {}: {e}", t.name))?;
        let (mut ins, mut upd, mut del) = (0, 0, 0);
        let prefix = format!("{}/", t.name);
        for r in core.rows(&main.s, &prefix).map_err(|e| e.to_string())? {
            let (k, raw) = r.map_err(|e| e.to_string())?;
            if !k.starts_with(&prefix) {
                break;
            }
            let ours = texts(core, &main.s, t, &k, &raw).map_err(|e| e.to_string())?;
            let key: Vec<Option<String>> = t.key.iter().map(|&i| ours[i].clone()).collect();
            match theirs.remove(&key_of(t, &ours)) {
                None => {
                    del += 1;
                    rows.push(Stored { table: ti, key, row: None });
                }
                Some(n) => {
                    if t.cols.iter().enumerate().any(|(i, col)| !same(&col.ty, ours[i].as_deref(), n[i].as_deref())) {
                        upd += 1;
                        rows.push(Stored { table: ti, key, row: Some(n) });
                    }
                }
            }
        }
        for (_, n) in theirs {
            ins += 1;
            let key = t.key.iter().map(|&i| n[i].clone()).collect();
            rows.push(Stored { table: ti, key, row: Some(n) });
        }
        out.tables.push((t.name.clone(), ins, upd, del));
    }
    Ok((out, rows))
}

// ---------------------------------------------------------------- attach

/// The Chronos type MySQL column type `ty` (COLUMN_TYPE) is held as, or why it can't be attached yet.
fn chronos_type(ty: &str) -> std::result::Result<String, String> {
    let t = ty.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("").to_string();
    let args = t.split_once('(').and_then(|(_, a)| a.split_once(')')).map(|(a, _)| a.to_string());
    let unsigned = t.contains("unsigned");
    Ok(match base.as_str() {
        "tinyint" => "smallint".into(),
        "smallint" if unsigned => "integer".into(),
        "smallint" => "smallint".into(),
        "mediumint" => "integer".into(),
        "int" | "integer" if unsigned => "bigint".into(),
        "int" | "integer" => "integer".into(),
        "bigint" if unsigned => "numeric(20, 0)".into(),
        "bigint" => "bigint".into(),
        "year" => "integer".into(),
        "decimal" | "numeric" | "dec" | "fixed" => match args {
            Some(a) => format!("numeric({a})"),
            None => "numeric(10, 0)".into(),
        },
        "float" | "double" | "real" => "double precision".into(),
        "char" | "varchar" => match args {
            Some(a) => format!("varchar({a})"),
            None => "text".into(),
        },
        "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set" | "time" => "text".into(),
        "date" => "date".into(),
        "datetime" | "timestamp" => "timestamp".into(),
        "json" => "jsonb".into(),
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit" => {
            return Err(format!("a binary column ({ty}): not attached from MySQL yet"));
        }
        _ => return Err(format!("a column of type {ty}: not attached from MySQL yet")),
    })
}

/// `chronos attach <dir>` with a MySQL CHRONOS_SOURCE_URL: tables of the URL's database, copied in one consistent
/// snapshot into a new folder, then recorded (`chronos_merges`'s marker row and the folder's record).
pub fn attach(dir: &str, opts: &crate::Open, named: &[String], yes: bool) -> std::result::Result<(), String> {
    let path = Path::new(dir);
    if path.exists() {
        return Err(format!("{dir} exists: attach makes a new folder (to attach again, give another)"));
    }
    if named.is_empty() {
        return Err("name the tables to attach: --tables a,b (tables of the database the URL names)".into());
    }
    let url = url()?;
    let mut c = open(&url)?;
    let v = c.version.clone();
    let num = |s: &str| -> (u32, u32) {
        let mut p = s.split(|ch: char| !ch.is_ascii_digit()).filter(|x| !x.is_empty()).map(|x| x.parse().unwrap_or(0));
        (p.next().unwrap_or(0), p.next().unwrap_or(0))
    };
    let (major, minor) = num(v.trim_start_matches("5.5.5-")); // (old MariaDB says "5.5.5-10.x" first)
    let maria = v.contains("MariaDB");
    if (maria && (major, minor) < (10, 5)) || (!maria && major < 8) {
        return Err(format!(
            "MySQL {v} is too old: attach needs MySQL 8.0 or later, or MariaDB 10.5 or later (nothing was copied)"
        ));
    }
    let tables: Vec<String> = named
        .iter()
        .map(|n| match n.split_once('.') {
            Some((db, t)) if db == url.database => Ok(t.to_string()),
            Some(_) => Err(format!("{n}: only tables of the database the URL names ({}) are attached", url.database)),
            None => Ok(n.clone()),
        })
        .collect::<std::result::Result<_, _>>()?;
    let setup = format!(
        "\nan administrator runs, once:\n\n  create table {}.{MERGES} (id varchar(255) primary key, at datetime(6) not \
         null default current_timestamp(6));\n  grant select, insert on {}.{MERGES} to <the user the URL names>;\n",
        mq(&url.database),
        mq(&url.database)
    );
    c.query(&format!(
        "create table if not exists {MERGES} (id varchar(255) primary key, at datetime(6) not null default \
         current_timestamp(6)) engine = InnoDB"
    ))
    .map_err(|e| format!("{} can't make the table {MERGES} in MySQL ({e}){setup}", url.user))?;
    // one consistent read of everything from here on
    c.query("set transaction isolation level repeatable read; start transaction with consistent snapshot, read only")
        .map_err(|e| format!("starting the read: {e}"))?;
    let list = tables.iter().map(|t| lit(Some(t))).collect::<Vec<_>>().join(", ");
    let rel = c
        .query(&format!(
            "select table_name, table_type, coalesce(engine, ''), coalesce(table_rows, 0) from information_schema.tables \
             where table_schema = database() and table_name in ({list}) order by table_name"
        ))
        .map_err(|e| format!("reading MySQL: {e}"))?;
    let rel: Vec<Texts> = rel.into_iter().flat_map(|d| d.rows).collect();
    let missing: Vec<&String> =
        tables.iter().filter(|t| !rel.iter().any(|r| r[0].as_deref() == Some(t.as_str()))).collect();
    if !missing.is_empty() {
        return Err(format!(
            "no table {} in MySQL's {}",
            missing.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(", "),
            url.database
        ));
    }
    let cols = c
        .query(&format!(
            "select table_name, column_name, column_type, is_nullable, extra, ordinal_position from \
             information_schema.columns where table_schema = database() and table_name in ({list}) order by \
             table_name, ordinal_position"
        ))
        .map_err(|e| format!("reading MySQL: {e}"))?;
    let cols: Vec<Texts> = cols.into_iter().flat_map(|d| d.rows).collect();
    let keys = c
        .query(&format!(
            "select table_name, column_name from information_schema.statistics where table_schema = database() and \
             table_name in ({list}) and index_name = 'PRIMARY' order by table_name, seq_in_index"
        ))
        .map_err(|e| format!("reading MySQL: {e}"))?;
    let keys: Vec<Texts> = keys.into_iter().flat_map(|d| d.rows).collect();
    let v = |r: &Texts, i: usize| r[i].clone().unwrap_or_default();
    let mut refused_why = vec![];
    let mut made: Vec<(Table, Vec<String>, i64)> = vec![]; // the record's table, Chronos's column definitions, rows
    let mut total = 0i64;
    for name in &tables {
        let r = rel.iter().find(|r| r[0].as_deref() == Some(name.as_str())).unwrap();
        if v(r, 1) != "BASE TABLE" {
            refused_why.push(format!("  - {name}: a view: attach the tables it reads"));
            continue;
        }
        if !v(r, 2).eq_ignore_ascii_case("InnoDB") {
            refused_why.push(format!(
                "  - {name}: its engine is {}, which has no transactions: only InnoDB tables are attached",
                v(r, 2)
            ));
            continue;
        }
        let mine: Vec<&Texts> = cols.iter().filter(|c| c[0].as_deref() == Some(name.as_str())).collect();
        let key_names: Vec<String> =
            keys.iter().filter(|k| k[0].as_deref() == Some(name.as_str())).map(|k| v(k, 1)).collect();
        if key_names.is_empty() {
            refused_why.push(format!(
                "  - {name}: no primary key, so a row can't be told from another: ALTER TABLE {name} ADD PRIMARY KEY \
                 (...), then attach"
            ));
            continue;
        }
        let mut t = Table {
            name: name.clone(),
            sql: q(name),
            pg: format!("{}.{}", mq(&url.database), mq(name)),
            cols: vec![],
            key: vec![],
            seqs: vec![],
        };
        let mut defs = vec![];
        for col in &mine {
            let (cname, ty, nullable, extra) = (v(col, 1), v(col, 2), v(col, 3) == "YES", v(col, 4).to_lowercase());
            if extra.contains("generated") {
                refused_why.push(format!("  - {name}.{cname}: a generated column: not attached from MySQL yet"));
                continue;
            }
            match chronos_type(&ty) {
                Ok(cty) => defs.push(format!("{} {cty}{}", q(&cname), if nullable { "" } else { " not null" })),
                Err(e) => {
                    refused_why.push(format!("  - {name}.{cname}: {e}"));
                    continue;
                }
            }
            if extra.contains("auto_increment") {
                // (values MySQL gives: no block of them can be reserved here, so the table takes no inserts)
                t.seqs.push(Seq { col: t.cols.len(), pg: None, counter: None });
            }
            let attnum = v(col, 5).parse::<i16>().unwrap_or(0);
            t.cols.push(Col { name: cname, ty, always: false, generated: false, attnum });
        }
        t.key = key_names.iter().filter_map(|k| t.cols.iter().position(|c| &c.name == k)).collect();
        let rows: i64 = v(r, 3).parse().unwrap_or(0);
        total += rows;
        println!("{name}: about {rows} rows");
        let keyed: Vec<String> = key_names.iter().map(|k| q(k)).collect();
        defs.push(format!("primary key ({})", keyed.join(", ")));
        made.push((t, defs, rows));
    }
    if !refused_why.is_empty() {
        return Err(format!("can't attach, nothing was copied:\n{}", refused_why.join("\n")));
    }
    if total > 1_000_000 && !yes {
        return Err(format!(
            "about {total} rows in all (MySQL's estimate): copying holds one read open on MySQL for that long. Run \
             again with --yes to go on"
        ));
    }
    let db = crate::Db::open_with(dir, opts).map_err(|e| format!("{dir}: {e}"))?;
    let done = (|| -> std::result::Result<Attached, String> {
        for (t, defs, _) in &made {
            crate::sql::run(&db, MAIN, &format!("create table {} ({})", t.sql, defs.join(", ")), &[])
                .map_err(|e| format!("making {} here: {}", t.name, e.message))?;
            let mut batch: Vec<String> = vec![];
            let mut n = 0u64;
            let flush = |batch: &mut Vec<String>| -> std::result::Result<(), String> {
                if batch.is_empty() {
                    return Ok(());
                }
                let sql = format!("insert into {} values {}", t.sql, batch.join(", "));
                batch.clear();
                crate::sql::run(&db, MAIN, &sql, &[])
                    .map(|_| ())
                    .map_err(|e| format!("copying {}: {}", t.name, e.message))
            };
            let mut failed_copy = None;
            let read = c.query_each(&format!("select {} from {}", select_cols(t), t.pg), &mut |r| {
                let vals: Vec<String> = r.iter().map(|v| v.as_deref().map_or("null".into(), super::lit)).collect();
                batch.push(format!("({})", vals.join(", ")));
                n += 1;
                if batch.len() >= 500
                    && let Err(e) = flush(&mut batch)
                {
                    failed_copy = Some(e);
                    return Err(io::Error::other("stopped"));
                }
                Ok(())
            });
            read.map_err(|e| failed_copy.clone().unwrap_or_else(|| format!("reading {} from MySQL: {e}", t.name)))?;
            flush(&mut batch)?;
            println!("{}: {n} rows", t.name);
        }
        c.query("commit").map_err(|e| format!("ending the read: {e}"))?;
        let att = Attached::new_mysql(path, &url, made.into_iter().map(|(t, _, _)| t).collect());
        c.query(&format!("insert into {MERGES} (id) values ({})", lit(Some(&att.marker()))))
            .map_err(|e| format!("recording the attach in {MERGES}: {e}{setup}"))?;
        att.save().map_err(|e| e.to_string())?;
        Ok(att)
    })();
    drop(db);
    let att = match done {
        Ok(a) => a,
        Err(e) => {
            let _ = std::fs::remove_dir_all(path);
            return Err(e);
        }
    };
    println!("\nattached {} tables of {} (MySQL) to {dir}", att.tables().len(), att.source());
    for t in att.tables().iter() {
        if !t.seqs.is_empty() {
            println!(
                "  {}: keyed by AUTO_INCREMENT, so rows can't be inserted here yet (updates and deletes go)",
                t.name
            );
        }
    }
    println!(
        "\nnext: chronos serve {dir} (with CHRONOS_SOURCE_URL set), fork a world, change rows there, and MERGE WORLD:\n\
         the change goes to MySQL first. Not yet with MySQL: serve --follow, dry runs, check_reads, merge checks, \
         writeback.json and pg_role."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_compare_by_type() {
        assert!(same("decimal(10,2)", Some("1.50"), Some("1.5")));
        assert!(same("decimal(10,2)", Some("-0.00"), Some("0")));
        assert!(!same("decimal(10,2)", Some("1.50"), Some("1.51")));
        assert!(same("int", Some("007"), Some("7")));
        assert!(same("datetime(6)", Some("2026-01-01 10:00:00.500000"), Some("2026-01-01 10:00:00.5")));
        assert!(same("datetime", Some("2026-01-01 10:00:00"), Some("2026-01-01T10:00:00")));
        assert!(same("json", Some(r#"{"a": 1, "b": [1, 2]}"#), Some(r#"{"b":[1,2],"a":1}"#)));
        assert!(same("double", Some("1e20"), Some("100000000000000000000")));
        assert!(!same("varchar(10)", Some("abc"), Some("ABC")), "text compares exactly, whatever the collation");
        assert!(!same("int", None, Some("0")));
        assert!(same("int", None, None));
    }

    #[test]
    fn types_map_or_are_refused() {
        assert_eq!(chronos_type("int unsigned").unwrap(), "bigint");
        assert_eq!(chronos_type("int").unwrap(), "integer");
        assert_eq!(chronos_type("tinyint(1)").unwrap(), "smallint");
        assert_eq!(chronos_type("bigint unsigned").unwrap(), "numeric(20, 0)");
        assert_eq!(chronos_type("decimal(10,2)").unwrap(), "numeric(10,2)");
        assert_eq!(chronos_type("varchar(40)").unwrap(), "varchar(40)");
        assert_eq!(chronos_type("datetime(6)").unwrap(), "timestamp");
        assert_eq!(chronos_type("enum('a','b')").unwrap(), "text");
        assert!(chronos_type("blob").is_err());
        assert!(chronos_type("geometry").is_err());
    }
}
