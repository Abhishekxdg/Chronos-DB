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
//! Not yet for MySQL (each refused, saying so): `serve --follow`, rehearsals, merge checks run in
//! the source, write-back rules, `pg_role`, `--lazy-tenant`, binary and spatial columns.
//!
//! A row inserted into an AUTO_INCREMENT table has a provisional id here, and MySQL's own from the merge on: see
//! `New`.

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

/// The rows each merge wrote, kept to undo it by (see `super::undo`): `chronos.merge_rows` as MySQL holds it.
const MERGE_ROWS: &str = "chronos_merge_rows";

/// Whether write-backs keep undo records: the table is there (made now, if the user may) and takes a row from this
/// user (tried, and rolled back).
pub(super) fn undo_ready() -> bool {
    let Ok(mut c) = url().and_then(|u| open(&u)) else { return false };
    // (tried only where the table isn't there: see the same for `MERGES` at attach)
    let there = c.query(&format!("select 1 from {MERGE_ROWS} limit 0")).is_ok();
    if !there {
        let _ = c.query(&format!(
            "create table if not exists {MERGE_ROWS} (merge_id varchar(255) not null, n int not null, table_name \
         varchar(255) not null, op varchar(10) not null, row_key json, before_row json, after_row json, at \
         datetime(6) not null default current_timestamp(6), primary key (merge_id, n), key {MERGE_ROWS}_at (at)) \
         engine = InnoDB"
        ));
    }
    let tried = c.query_all(&format!(
        "start transaction; insert into {MERGE_ROWS} (merge_id, n, table_name, op) values ('probe', 0, '', '')"
    ));
    let _ = c.query("rollback");
    matches!(tried, Ok((_, None)))
}

/// The records of merge `id` as MySQL holds them, in the order it wrote them, and when it was (ms since 1970).
pub(super) fn undo_read(id: &str) -> std::result::Result<(u64, Vec<super::undo::Raw>), String> {
    let mut c = open(&url()?)?;
    let sql = format!(
        "select table_name, op, row_key, before_row, after_row, cast(unix_timestamp(at) * 1000 as unsigned) from \
         {MERGE_ROWS} where merge_id = {} order by n",
        lit(Some(id))
    );
    let rows: Vec<Texts> =
        c.query(&sql).map_err(|e| format!("reading {MERGE_ROWS}: {e}"))?.into_iter().flat_map(|d| d.rows).collect();
    let at = rows.first().and_then(|r| r[5].as_deref()).and_then(|v| v.parse().ok()).unwrap_or(0);
    Ok((at, rows.into_iter().map(|r| [r[0].clone(), r[1].clone(), r[2].clone(), r[3].clone(), r[4].clone()]).collect()))
}

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
    let vals: Vec<Vec<String>> =
        keys.iter().map(|k| t.key.iter().zip(k).map(|(&i, v)| typed(t, i, v.as_deref())).collect()).collect();
    by_key_sql(t, &vals)
}

/// `by_keys`, of keys already written as SQL (a part may be a variable: see `New`).
fn by_key_sql(t: &Table, keys: &[Vec<String>]) -> String {
    let cols: Vec<String> = t.key.iter().map(|&i| mq(&t.cols[i].name)).collect();
    let vals: Vec<String> = keys.iter().map(|k| format!("({})", k.join(", "))).collect();
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
    core: &Core,
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
    if !checks.is_empty() {
        return no("running merge checks in the source");
    }
    let (mut c, kept) = lease(att).map_err(|e| refused(format!("{e}; nothing was merged")))?;
    let out = transaction(core, att, &mut c, kept, world, world_id, rows, reads);
    if out.is_err() && !c.broken && c.in_tx {
        let _ = c.query("rollback");
    }
    put(att, c);
    out
}

/// `super::check_reads`, against MySQL: what the world read, checked as the write-back checks it, outside any merge.
pub(super) fn check_reads(core: &Core, att: &Attached, r: &Reads) -> std::result::Result<Vec<String>, String> {
    let (mut c, _) = lease(att)?;
    let out = c
        .query("start transaction read only")
        .map_err(|e| failed("starting the read", &e))
        .and_then(|_| super::live_reads(core, &mut MyLive(&mut c), att, r));
    put(att, c);
    out.map_err(|e| e.to_string())
}

/// `super::dry_run`'s read of MySQL: one snapshot, no locks, rolled back. Each row the merge would write compared
/// with MySQL's as the write-back compares it, the world's reads checked, and the tables' triggers named (not run).
pub(super) fn dry_run(
    core: &Core,
    att: &Attached,
    rows: &[Row],
    reads: Option<&Reads>,
    live: &mut crate::LiveCheck,
) -> std::result::Result<(), String> {
    let (mut c, _) = lease(att)?;
    let out = dry_read(core, att, &mut c, rows, reads, live);
    put(att, c);
    out.map_err(|e| e.to_string())
}

fn dry_read(
    core: &Core,
    att: &Attached,
    c: &mut My,
    rows: &[Row],
    reads: Option<&Reads>,
    live: &mut crate::LiveCheck,
) -> Result<()> {
    let start =
        "set transaction isolation level repeatable read; start transaction with consistent snapshot, read only";
    let marker = format!("select count(*) from {MERGES} where id = {}", lit(Some(&att.marker())));
    let got = send(c, &[("starting the read".into(), start.into()), ("reading chronos_merges".into(), marker)])?;
    if got.last().and_then(|d| d.rows.first()).and_then(|r| r[0].as_deref()) != Some("1") {
        return Err(refused(format!(
            "the MySQL CHRONOS_SOURCE_URL names isn't the one this folder was attached to ({}): its chronos_merges \
             has no row {}",
            att.source(),
            att.marker()
        )));
    }
    live.at = crate::now_ms();
    if let Some(r) = reads {
        live.stale_reads =
            super::live_reads(core, &mut MyLive(c), att, r)?.iter().map(|w| crate::agents::for_caller(w)).collect();
    }
    let tables = att.tables();
    let by_table = by_table(rows);
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let changed: HashMap<usize, Option<Texts>> = MyLive(c).changed(t, rs)?.into_iter().collect();
        for (at, r) in rs.iter().enumerate() {
            live.rows.push(crate::LiveRow {
                key: r.key.clone(),
                changed: changed.contains_key(&at),
                // (a row as main has it is MySQL's row, by the comparison)
                postgres: match changed.get(&at) {
                    Some(now) => now.as_ref().map(|n| row_json(t, n)),
                    None => r.before.as_ref().map(|b| row_json(t, b)),
                },
            });
        }
    }
    let names: Vec<String> = by_table.keys().map(|&ti| lit(Some(&tables[ti].name))).collect();
    let sql = format!(
        "select event_object_table, trigger_name from information_schema.triggers where event_object_schema = \
         database() and event_object_table in ({}) order by 1, 2",
        names.join(", ")
    );
    for d in send(c, &[("reading the tables' triggers".into(), sql)])? {
        for r in d.rows {
            live.triggers.push(format!("{}: {}", r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default()));
        }
    }
    let _ = c.query("rollback");
    Ok(())
}

/// `super::Live`, of MySQL on the write-back's connection: rows compared here, by type (see `same`), and a table
/// read more broadly than by keys read whole, for the world's tenant when it read for one.
struct MyLive<'a>(&'a mut My);

/// `super::tenant_cond`, in MySQL's SQL.
fn tenant_cond(
    v: &crate::View,
    of: &mut crate::sql::Tenants,
    att: &Attached,
    name: &str,
    tenant: &Json,
    a: &str,
    hops: usize,
) -> Result<Option<String>> {
    use crate::sql::TenantReach;
    let sql_err = |e: crate::sql::SqlError| Error::Constraint { code: e.code, message: e.message };
    let tables = att.tables();
    let Some(t) = att.table(name).map(|i| &tables[i]) else { return Ok(None) };
    match of.reach(v, name).map_err(sql_err)? {
        Some(TenantReach::Column) => {
            let col = tenant["column"].as_str().unwrap_or_default();
            let Some(c) = t.cols.iter().find(|c| c.name == col) else { return Ok(None) };
            let number = matches!(kind(&c.ty), Kind::Int | Kind::Unsigned | Kind::Decimal | Kind::Float);
            let text = |s: &str| Some(format!("cast({a}.{} as char) = {}", mq(&c.name), lit(Some(s))));
            Ok(match &tenant["value"] {
                Json::Number(n) if number => Some(format!("{a}.{} = {n}", mq(&c.name))),
                Json::Number(n) => text(&n.to_string()),
                Json::String(s) => text(s),
                Json::Bool(b) => text(&b.to_string()),
                _ => None,
            })
        }
        Some(TenantReach::Through { cols, parent, refs, .. }) if hops < crate::sql::MOST_TENANT_HOPS => {
            let p = format!("{a}p");
            let Some(cond) = tenant_cond(v, of, att, &parent, tenant, &p, hops + 1)? else { return Ok(None) };
            let Some(pt) = att.table(&parent).map(|i| &tables[i]) else { return Ok(None) };
            let list =
                |al: &str, cs: &[String]| cs.iter().map(|c| format!("{al}.{}", mq(c))).collect::<Vec<_>>().join(", ");
            Ok(Some(format!("({}) in (select {} from {} {p} where {cond})", list(a, &cols), list(&p, &refs), pt.pg)))
        }
        _ => Ok(None),
    }
}

impl super::Live for MyLive<'_> {
    fn name(&self) -> &'static str {
        "MySQL"
    }

    /// (Filters aren't written in MySQL's SQL yet: a table read by a filtered scan is read whole.)
    fn narrow(&self, _filters: &[crate::sql::RowFilter], _types: &[(String, String)]) -> Option<String> {
        None
    }

    fn tenant_cond(
        &self,
        v: &crate::View,
        of: &mut crate::sql::Tenants,
        att: &Attached,
        name: &str,
        tenant: &Json,
    ) -> Result<Option<String>> {
        tenant_cond(v, of, att, name, tenant, "t", 0)
    }

    fn changed(&mut self, t: &Table, rs: &[&Row]) -> Result<Vec<(usize, Option<Texts>)>> {
        let keys: Vec<Vec<Option<String>>> = rs.iter().map(|r| r.key_texts(t)).collect();
        let mut now: HashMap<Vec<Option<String>>, Texts> = HashMap::new();
        for part in keys.chunks(KEYS) {
            let sql = format!("select {} from {} where {}", select_cols(t), t.pg, by_keys(t, part));
            for d in send(self.0, &[(format!("reading rows of {}", t.name), sql)])? {
                for r in d.rows {
                    now.insert(key_of(t, &r), r);
                }
            }
        }
        let mut out = vec![];
        for (at, (r, key)) in rs.iter().zip(&keys).enumerate() {
            let theirs = now.get(&key_of_key(t, key));
            let same_row = match (&r.before, theirs) {
                (Some(b), Some(n)) => {
                    t.cols.iter().enumerate().all(|(i, col)| same(&col.ty, b[i].as_deref(), n[i].as_deref()))
                }
                (None, None) => true,
                _ => false,
            };
            if !same_row {
                out.push((at, theirs.cloned()));
            }
        }
        Ok(out)
    }

    fn added(&mut self, t: &Table, rs: &[&Row], only: &[String]) -> Result<Vec<Texts>> {
        let known: HashSet<Vec<Option<String>>> = rs.iter().map(|r| key_of_key(t, &r.key_texts(t))).collect();
        let cols: Vec<String> = t.cols.iter().map(|c| format!("t.{}", mq(&c.name))).collect();
        let cond = match only.is_empty() {
            true => String::new(),
            false => format!(" where {}", only.join(" and ")),
        };
        // (one more than could be told apart from too many, counting those the fork point has)
        let limit = super::LIVE_ROWS + known.len() + 1;
        let sql = format!("select {} from {} t{cond} limit {limit}", cols.join(", "), t.pg);
        let mut out = vec![];
        for d in send(self.0, &[(format!("reading rows of {} the copy hasn't", t.name), sql)])? {
            out.extend(d.rows.into_iter().filter(|r| !known.contains(&key_of(t, r))));
        }
        Ok(out)
    }
}

/// The rows a merge inserts into AUTO_INCREMENT tables, each under a provisional id (see `super::PROVISIONAL`), and
/// where those ids stand in the merge's other rows. MySQL gives each row its id as it is inserted; the write-back
/// keeps it in a session variable (`@chr_p<n>`, from LAST_INSERT_ID), and writes the variable wherever the
/// provisional id stood: only in a column MySQL declares a foreign key from, to the new row's own column. A
/// provisional id anywhere else is refused: no number can be written there that would be right.
struct New<'a> {
    /// each new row: its table, the row, its AUTO_INCREMENT column, its provisional id
    rows: Vec<(usize, &'a Row, usize, i64)>,
    /// a provisional id's place in `rows`
    at: HashMap<i64, usize>,
    /// (table, column) MySQL declares a foreign key from, to the AUTO_INCREMENT column of this table
    refs: HashMap<(usize, usize), usize>,
}

fn var(n: usize) -> String {
    format!("@chr_p{n}")
}

impl<'a> New<'a> {
    fn of(att: &Attached, tables: &[Table], rows: &'a [Row]) -> New<'a> {
        let mut new = New { rows: vec![], at: HashMap::new(), refs: HashMap::new() };
        for r in rows.iter().filter(|r| r.before.is_none()) {
            let (t, Some(a)) = (&tables[r.table], &r.after) else { continue };
            let found = t.seqs.iter().find_map(|sq| {
                let v = a[sq.col].as_deref()?.trim().parse::<i64>().ok()?;
                att.provisional(v).then_some((sq.col, v))
            });
            if let Some((col, v)) = found {
                new.at.insert(v, new.rows.len());
                new.rows.push((r.table, r, col, v));
            }
        }
        new
    }

    /// The foreign keys MySQL declares to the new rows' tables, read on the write-back's connection.
    fn fk_sql(&self, tables: &[Table]) -> String {
        let mut names: Vec<String> = self.rows.iter().map(|(ti, ..)| lit(Some(&tables[*ti].name))).collect();
        names.sort();
        names.dedup();
        format!(
            "select table_name, column_name, referenced_table_name, referenced_column_name from \
             information_schema.key_column_usage where table_schema = database() and referenced_table_schema = \
             database() and referenced_table_name in ({})",
            names.join(", ")
        )
    }

    fn take_refs(&mut self, att: &Attached, tables: &[Table], found: &[Texts]) {
        for r in found {
            let name = |i: usize| r[i].as_deref().unwrap_or_default();
            let (Some(child), Some(parent)) = (att.table(name(0)), att.table(name(2))) else { continue };
            let Some(col) = tables[child].cols.iter().position(|c| c.name == name(1)) else { continue };
            let to_seq = tables[parent].seqs.iter().any(|sq| tables[parent].cols[sq.col].name == name(3));
            if to_seq {
                self.refs.insert((child, col), parent);
            }
        }
    }

    /// The variable to write for column `i` of a row of table `ti` holding `v`, when `v` is a provisional id: Err
    /// where no foreign key says whose id it is.
    fn var_for(&self, att: &Attached, tables: &[Table], ti: usize, i: usize, v: Option<&str>) -> Result<Option<usize>> {
        let t = &tables[ti];
        if !matches!(kind(&t.cols[i].ty), Kind::Int | Kind::Unsigned | Kind::Decimal) {
            return Ok(None);
        }
        let Some(id) = v.and_then(|v| v.trim().parse::<i64>().ok()).filter(|n| att.provisional(*n)) else {
            return Ok(None);
        };
        let col = format!("{}.{}", t.name, t.cols[i].name);
        match (self.refs.get(&(ti, i)), self.at.get(&id)) {
            (Some(&parent), Some(&n)) if self.rows[n].0 == parent => Ok(Some(n)),
            (_, Some(&n)) => {
                let p = &tables[self.rows[n].0];
                Err(refused(format!(
                    "{col} holds {id}, the provisional id of a new row of {}, and MySQL declares no foreign key from \
                     {col} to {}.{}: the id MySQL gives the row can't be put there. Declare the foreign key in MySQL \
                     and attach again, or merge the new row of {} first and write its real id; nothing was merged",
                    p.name, p.name, p.cols[self.rows[n].2].name, p.name
                )))
            }
            (_, None) => Err(refused(format!(
                "{col} holds {id}, a provisional id this folder gave a new row that this merge doesn't insert (it was \
                 merged before, or never): read the row's id from main and write that; nothing was merged"
            ))),
        }
    }

    /// Each column of `r` (a row the merge writes) a variable stands in for, with the variable's number.
    fn places(&self, att: &Attached, tables: &[Table], r: &Row) -> Result<Vec<(usize, usize)>> {
        let Some(a) = &r.after else { return Ok(vec![]) };
        let own = self.rows.iter().position(|(_, row, ..)| std::ptr::eq(*row, r));
        let mut out = vec![];
        for (i, v) in a.iter().enumerate() {
            match own {
                Some(n) if self.rows[n].2 == i => out.push((i, n)),
                _ => out.extend(self.var_for(att, tables, r.table, i, v.as_deref())?.map(|n| (i, n))),
            }
        }
        Ok(out)
    }

    /// `rows` in an order that inserts a row before any that refers to it.
    fn order(&self, places: &HashMap<&str, Vec<(usize, usize)>>) -> Result<Vec<usize>> {
        let mut left: Vec<usize> = (0..self.rows.len()).collect();
        let mut done: HashSet<usize> = HashSet::new();
        let mut out = vec![];
        while !left.is_empty() {
            let ready: Vec<usize> = left
                .iter()
                .copied()
                .filter(|&n| places[self.rows[n].1.key.as_str()].iter().all(|&(_, m)| m == n || done.contains(&m)))
                .collect();
            // (a row whose own column, the AUTO_INCREMENT one, is its only variable depends on none)
            let ready: Vec<usize> = ready
                .into_iter()
                .filter(|&n| !places[self.rows[n].1.key.as_str()].iter().any(|&(i, m)| m == n && i != self.rows[n].2))
                .collect();
            if ready.is_empty() {
                return Err(refused(
                    "new rows of AUTO_INCREMENT tables refer to each other in a circle (or one to itself), so none \
                     can be inserted first: merge them in two steps; nothing was merged",
                ));
            }
            // (in the order the world made them: each id is below the one before)
            let mut ready = ready;
            ready.sort_by_key(|&n| std::cmp::Reverse(self.rows[n].3));
            left.retain(|n| !ready.contains(n));
            done.extend(&ready);
            out.extend(ready);
        }
        Ok(out)
    }
}

/// What each statement of the write-back's second round trip must answer.
enum Want {
    /// rows affected, of that table, by what
    Affected(usize, usize, &'static str),
    /// rows of that table read back as MySQL stored them
    Back(usize),
    /// rows of that table read back by keys the merge didn't know (see `New`): that many
    BackNew(usize, usize),
    /// the ids MySQL gave those new rows
    Ids(Vec<usize>),
    Nothing,
}

#[allow(clippy::too_many_arguments)]
fn transaction(
    core: &Core,
    att: &Attached,
    c: &mut My,
    kept: bool,
    world: &str,
    world_id: &str,
    rows: &[Row],
    read_set: Option<&Reads>,
) -> Result<Option<Held>> {
    let tables = att.tables();
    let by_table = by_table(rows);
    let keeps_undo = super::undo::ready(att);
    let mut new = New::of(att, &tables, rows);
    // 1. the transaction, the folder's marker, and every row it writes locked and read (one round trip)
    let mut first = vec![
        ("starting the write-back".to_string(), "start transaction".to_string()),
        (
            "reading chronos_merges".to_string(),
            format!("select count(*) from {MERGES} where id = {}", lit(Some(&att.marker()))),
        ),
    ];
    if !new.rows.is_empty() {
        first.push(("reading the tables' foreign keys".to_string(), new.fk_sql(&tables)));
    }
    let lead = first.len();
    // a row inserted under a provisional id (its own, or its parent's in its key) has no row in MySQL to lock or
    // compare: MySQL holds no such number
    let unkeyed = |t: &Table, r: &Row| {
        r.before.is_none()
            && t.key.iter().any(|&i| {
                let v = r.after.as_ref().and_then(|a| a[i].as_deref()).and_then(|v| v.trim().parse::<i64>().ok());
                matches!(kind(&t.cols[i].ty), Kind::Int | Kind::Unsigned | Kind::Decimal)
                    && v.is_some_and(|v| att.provisional(v))
            })
    };
    let mut reads: Vec<usize> = vec![]; // the table each read after those is of
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let keys: Vec<Vec<Option<String>>> = rs.iter().filter(|r| !unkeyed(t, r)).map(|r| r.key_texts(t)).collect();
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
    if !new.rows.is_empty() {
        new.take_refs(att, &tables, &got[2].rows);
    }
    let new = new;
    let mut now: HashMap<(usize, Vec<Option<String>>), Texts> = HashMap::new();
    for (d, &ti) in got[lead..].iter().zip(&reads) {
        for r in &d.rows {
            now.insert((ti, key_of(&tables[ti], r)), r.clone());
        }
    }
    // compared: a row MySQL changed (or added, or deleted) since main had it is a conflict, and nothing is written
    let mut conflicts: Vec<(usize, &Row, Option<Texts>, Texts)> = vec![];
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        for r in rs.iter().filter(|r| !unkeyed(t, r)) {
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
    // what the world read, checked against MySQL before anything is written (see `super::live_reads`): changed,
    // the merge is held for a person, and nothing is written
    if let Some(r) = read_set {
        let stale = super::live_reads(core, &mut MyLive(c), att, r)?;
        if !stale.is_empty() {
            let _ = c.query("rollback");
            return Ok(Some(Held::Stale(stale)));
        }
    }
    // 2. the writes, the rows read back as MySQL stored them, and the merge's own row (one round trip)
    let merge_id = format!("merge:{}", random_id());
    // where a variable stands for a provisional id, by row (see `New`)
    let mut places: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
    for r in rows {
        places.insert(r.key.as_str(), new.places(att, &tables, r)?);
    }
    let value = |r: &Row, i: usize, v: Option<&str>| match places[r.key.as_str()].iter().find(|p| p.0 == i) {
        Some(&(_, n)) => var(n),
        None => lit(v),
    };
    // (in a row inserted: a column MySQL fills, left to it)
    let put = |t: &Table, r: &Row, i: usize, v: Option<&str>| match v {
        None if t.cols[i].fill => "default".to_string(),
        v => value(r, i, v),
    };
    // a generated column is MySQL's to work out: a world that gave it a value of its own is refused
    for r in rows {
        let t = &tables[r.table];
        let Some(a) = &r.after else { continue };
        for i in (0..t.cols.len()).filter(|&i| t.cols[i].computed) {
            let was = r.before.as_ref().and_then(|b| b[i].as_deref());
            if !same(&t.cols[i].ty, was, a[i].as_deref()) {
                let _ = c.query("rollback");
                return Err(refused(format!(
                    "{world} gives {}.{} a value, and MySQL generates that column: leave it as it is (main takes \
                     MySQL's value with the row at the merge); nothing was merged",
                    t.name, t.cols[i].name
                )));
            }
        }
    }
    // (a row whose key holds a variable: read back, and kept by main, under the key MySQL gave)
    let rekeyed = |t: &Table, r: &Row| places[r.key.as_str()].iter().any(|(i, _)| t.key.contains(i));
    let own: HashMap<&str, usize> = new.rows.iter().enumerate().map(|(n, (_, r, ..))| (r.key.as_str(), n)).collect();
    let mut writes: Vec<(String, String)> = vec![];
    let mut expect: Vec<Want> = vec![];
    // the new rows of AUTO_INCREMENT tables first, a row before any that refers to it, each one's id kept
    for n in new.order(&places)? {
        let (ti, r, col, _) = new.rows[n];
        let t = &tables[ti];
        let a = r.after.as_ref().unwrap();
        let cols: Vec<usize> =
            (0..t.cols.len()).filter(|i| *i != col && !r.unseen.contains(i) && !t.cols[*i].computed).collect();
        let names: Vec<String> = cols.iter().map(|&i| mq(&t.cols[i].name)).collect();
        let vals: Vec<String> = cols.iter().map(|&i| put(t, r, i, a[i].as_deref())).collect();
        writes.push((
            format!("insert into {}", t.name),
            format!("insert into {} ({}) values ({})", t.pg, names.join(", "), vals.join(", ")),
        ));
        expect.push(Want::Affected(1, ti, "inserted"));
        writes.push((
            format!("taking the id MySQL gave a row of {}", t.name),
            format!("set {} = last_insert_id()", var(n)),
        ));
        expect.push(Want::Nothing);
    }
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        for r in rs.iter().filter(|r| r.before.is_some() && r.after.is_some()) {
            let after = r.after.as_ref().unwrap();
            let sets: Vec<String> = (0..t.cols.len())
                .filter(|i| !t.key.contains(i) && !t.cols[*i].computed)
                .map(|i| format!("{} = {}", mq(&t.cols[i].name), value(r, i, after[i].as_deref())))
                .collect();
            if sets.is_empty() {
                continue;
            }
            writes.push((
                format!("update {}", t.name),
                format!("update {} set {} where {}", t.pg, sets.join(", "), by_keys(t, &[r.key_texts(t)])),
            ));
            expect.push(Want::Affected(1, ti, "updated"));
        }
        let inserts: Vec<&&Row> =
            rs.iter().filter(|r| r.before.is_none() && !own.contains_key(r.key.as_str())).collect();
        for part in inserts.chunks(KEYS) {
            let unseen = &part[0].unseen;
            let cols: Vec<usize> = (0..t.cols.len()).filter(|i| !unseen.contains(i) && !t.cols[*i].computed).collect();
            let names: Vec<String> = cols.iter().map(|&i| mq(&t.cols[i].name)).collect();
            let vals: Vec<String> = part
                .iter()
                .map(|r| {
                    let a = r.after.as_ref().unwrap();
                    format!("({})", cols.iter().map(|&i| put(t, r, i, a[i].as_deref())).collect::<Vec<_>>().join(", "))
                })
                .collect();
            writes.push((
                format!("insert into {}", t.name),
                format!("insert into {} ({}) values {}", t.pg, names.join(", "), vals.join(", ")),
            ));
            expect.push(Want::Affected(part.len(), ti, "inserted"));
        }
    }
    for (&ti, rs) in by_table.iter().rev() {
        let t = &tables[ti];
        let keys: Vec<Vec<Option<String>>> = rs.iter().filter(|r| r.after.is_none()).map(|r| r.key_texts(t)).collect();
        for part in keys.chunks(KEYS) {
            writes
                .push((format!("delete from {}", t.name), format!("delete from {} where {}", t.pg, by_keys(t, part))));
            expect.push(Want::Affected(part.len(), ti, "deleted"));
        }
    }
    // (MySQL has no RETURNING: the rows written read back, so main takes what defaults and triggers stored)
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let keys: Vec<Vec<Option<String>>> =
            rs.iter().filter(|r| r.after.is_some() && !rekeyed(t, r)).map(|r| r.after_key(t)).collect();
        for part in keys.chunks(KEYS) {
            writes.push((
                format!("reading back rows of {}", t.name),
                format!("select {} from {} where {}", select_cols(t), t.pg, by_keys(t, part)),
            ));
            expect.push(Want::Back(ti));
        }
        let keys: Vec<Vec<String>> = rs
            .iter()
            .filter(|r| r.after.is_some() && rekeyed(t, r))
            .map(|r| {
                let a = r.after.as_ref().unwrap();
                t.key
                    .iter()
                    .map(|&i| match places[r.key.as_str()].iter().find(|p| p.0 == i) {
                        Some(&(_, n)) => var(n),
                        None => typed(t, i, a[i].as_deref()),
                    })
                    .collect()
            })
            .collect();
        for part in keys.chunks(KEYS) {
            writes.push((
                format!("reading back rows of {}", t.name),
                format!("select {} from {} where {}", select_cols(t), t.pg, by_key_sql(t, part)),
            ));
            expect.push(Want::BackNew(ti, part.len()));
        }
    }
    let all: Vec<usize> = (0..new.rows.len()).collect();
    for part in all.chunks(KEYS) {
        let vars: Vec<String> = part.iter().map(|&n| var(n)).collect();
        writes.push(("reading the ids MySQL gave".into(), format!("select {}", vars.join(", "))));
        expect.push(Want::Ids(part.to_vec()));
    }
    writes.push(("recording the merge".into(), format!("insert into {MERGES} (id) values ({})", lit(Some(&merge_id)))));
    expect.push(Want::Affected(1, usize::MAX, "recorded"));
    let done = send(c, &writes)?;
    let mut sent_back: Vec<Stored> = vec![];
    let mut real: Vec<Option<String>> = vec![None; new.rows.len()];
    for (d, want) in done.iter().zip(&expect) {
        match want {
            Want::Affected(n, ti, what) if d.affected as usize != *n => {
                let t = tables.get(*ti).map_or(MERGES, |t| t.name.as_str());
                return Err(refused(format!(
                    "MySQL {what} {} rows of {t} where the merge sent {n}: nothing was merged",
                    d.affected
                )));
            }
            Want::Affected(..) | Want::Nothing => {}
            Want::Back(ti) | Want::BackNew(ti, _) => {
                if let Want::BackNew(_, n) = want
                    && d.rows.len() != *n
                {
                    return Err(refused(format!(
                        "MySQL has {} of the {n} rows the merge just inserted into {} under the ids it gave them (a \
                         trigger there gives ids of its own, or moves rows): nothing was merged",
                        d.rows.len(),
                        tables[*ti].name
                    )));
                }
                for r in &d.rows {
                    let key = tables[*ti].key.iter().map(|&i| r[i].clone()).collect();
                    sent_back.push(Stored { table: *ti, key, row: Some(r.clone()) });
                }
            }
            Want::Ids(ns) => {
                let row = d.rows.first();
                for (at, &n) in ns.iter().enumerate() {
                    real[n] = row.and_then(|r| r[at].clone()).filter(|v| v != "0");
                    if real[n].is_none() {
                        return Err(refused(format!(
                            "MySQL gave no id to a row the merge inserted into {}: nothing was merged",
                            tables[new.rows[n].0].name
                        )));
                    }
                }
            }
        }
    }
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        // (and the row main holds under the provisional key goes: MySQL's is under its own)
        for r in rs.iter().filter(|r| r.after.is_none() || rekeyed(t, r)) {
            let key = match &r.after {
                Some(a) => t.key.iter().map(|&i| a[i].clone()).collect(),
                None => r.key_texts(t),
            };
            sent_back.push(Stored { table: ti, key, row: None });
        }
    }
    // what recovery needs should the commit's outcome be lost
    let mut touched: BTreeMap<String, Vec<Vec<Option<String>>>> = BTreeMap::new();
    for (&ti, rs) in &by_table {
        let t = &tables[ti];
        let keys = touched.entry(t.name.clone()).or_default();
        for r in rs {
            keys.push(r.key_texts(t));
            // (and the key MySQL gave it, which recovery reads its row by)
            if let (Some(a), true) = (&r.after, rekeyed(t, r)) {
                let key = t.key.iter().map(|&i| match places[r.key.as_str()].iter().find(|p| p.0 == i) {
                    Some(&(_, n)) => real[n].clone(),
                    None => a[i].clone(),
                });
                keys.push(key.collect());
            }
        }
    }
    // (an undo's dry run stops here; what MySQL's cascades and triggers wrote besides isn't counted)
    super::undo::before_commit(&[])?;
    // each row written, as it was and as MySQL stored it: what undoes this merge later (see `super::undo`). One
    // more round trip, in the transaction
    if keeps_undo {
        let back: HashMap<(usize, Vec<Option<String>>), &Texts> = sent_back
            .iter()
            .filter_map(|s| Some(((s.table, key_of_key(&tables[s.table], &s.key)), s.row.as_ref()?)))
            .collect();
        let landed = |r: &Row| {
            let (t, a) = (&tables[r.table], r.after.as_ref()?);
            // (under the key MySQL gave it, where a variable stood in its key)
            let key: Vec<Option<String>> = t
                .key
                .iter()
                .map(|&i| match places[r.key.as_str()].iter().find(|p| p.0 == i) {
                    Some(&(_, n)) => real[n].clone(),
                    None => a[i].clone(),
                })
                .collect();
            back.get(&(r.table, key_of_key(t, &key))).map(|row| (*row).clone())
        };
        let recs = super::undo::records(&tables, rows, &landed);
        let mut stmts = vec![];
        for part in recs.chunks(200) {
            let vals: Vec<String> = part
                .iter()
                .map(|j| {
                    let text = |k: &str| match &j[k] {
                        Json::Null => "null".to_string(),
                        v => lit(Some(&v.to_string())),
                    };
                    format!(
                        "({}, {}, {}, {}, {}, {}, {})",
                        lit(Some(&merge_id)),
                        j["n"],
                        lit(j["table"].as_str()),
                        lit(j["op"].as_str()),
                        text("key"),
                        text("before"),
                        text("after")
                    )
                })
                .collect();
            stmts.push((
                "recording the merge's rows for undo".to_string(),
                format!(
                    "insert into {MERGE_ROWS} (merge_id, n, table_name, op, row_key, before_row, after_row) values {}",
                    vals.join(", ")
                ),
            ));
        }
        send(c, &stmts)?;
    }
    let pending = super::pending_record(&merge_id, world, world_id, json!(touched), json!([]));
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
    super::undo::merged(&merge_id);
    if keeps_undo {
        // (records past the undo window go, a batch at a time; a user who may not delete them leaves them)
        let days = super::window_days();
        let _ = c.query(&format!("delete from {MERGE_ROWS} where at < now(6) - interval {days} day limit 5000"));
    }
    AFTER.set(Some(After { rows: sent_back, committed: true, refresh: vec![] }));
    Ok(None)
}

impl Row {
    /// Its key as the merge leaves it (for a row it inserts or updates).
    fn after_key(&self, t: &Table) -> Vec<Option<String>> {
        let r = self.after.as_ref().or(self.before.as_ref()).expect("a row on one side at least");
        t.key.iter().map(|&i| r[i].clone()).collect()
    }
}

/// MySQL's settings that let it lose a commit it acknowledged: the redo log not flushed at each commit
/// (`innodb_flush_log_at_trx_commit` not 1), or, with the binary log on, it not synced at each (`sync_binlog` not 1).
pub(super) fn unsafe_durability(att: &Attached) -> std::result::Result<Option<String>, String> {
    let sql = "select @@innodb_flush_log_at_trx_commit, @@sync_binlog, @@log_bin";
    let (mut c, reused) = lease(att)?;
    let done = match c.query(sql) {
        // a kept connection MySQL closed while it was idle: once more on a new one (this reads only)
        Err(_) if reused => {
            c = open(&url()?)?;
            c.query(sql)
        }
        out => out,
    }
    .map_err(|e| format!("reading MySQL's settings: {e}"))?;
    put(att, c);
    let r = done.last().and_then(|d| d.rows.first()).ok_or("reading MySQL's settings: no row")?;
    let v = |i: usize| r[i].as_deref().unwrap_or("");
    let mut bad = vec![];
    if v(0) != "1" {
        bad.push(format!("innodb_flush_log_at_trx_commit = {}", v(0)));
    }
    if v(2) == "1" && v(1) != "1" {
        bad.push(format!("sync_binlog = {}", v(1)));
    }
    Ok((!bad.is_empty()).then(|| bad.join(", ")))
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

/// `super::reconcile` of a MySQL folder: `refresh`'s read and comparison, nothing taken.
pub(super) fn reconcile(core: &Core, att: &Attached) -> std::result::Result<Refreshed, String> {
    let (mut c, _) = lease(att)?;
    let out = refresh_read(core, att, &mut c, None);
    put(att, c);
    out.map(|(done, _)| done)
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

/// What the copy's column takes for MySQL's default of it.
#[derive(Debug, PartialEq)]
enum Dflt {
    None,
    /// the same default here, in Chronos's SQL: a row inserted in a world holds the value MySQL would give
    Sql(String),
    /// an expression only MySQL works out: NULL here, DEFAULT at the merge (see `Col::fill`)
    Fill,
}

/// MySQL's default of a column of type `ty` (`information_schema.columns`' COLUMN_DEFAULT and EXTRA, which MySQL
/// and MariaDB write differently: MariaDB as SQL, MySQL a literal's bare text).
fn default_of(maria: bool, ty: &str, def: Option<&str>, extra: &str) -> Dflt {
    let Some(def) = def else { return Dflt::None };
    let low = def.to_ascii_lowercase();
    let now = low.starts_with("current_timestamp") || low.starts_with("now(");
    let text = match maria {
        true if low == "null" => return Dflt::None,
        true if now => None,
        true if def.len() >= 2 && def.starts_with('\'') && def.ends_with('\'') => {
            Some(def[1..def.len() - 1].replace("''", "'").replace("\\'", "'").replace("\\\\", "\\"))
        }
        true if def.trim().parse::<f64>().is_ok() => Some(def.trim().to_string()),
        true => return Dflt::Fill,
        false if extra.contains("default_generated") && !now => return Dflt::Fill,
        false if now => None,
        false => Some(def.to_string()),
    };
    match (kind(ty), text) {
        (Kind::DateTime, None) => Dflt::Sql("current_timestamp".into()),
        (_, None) => Dflt::Fill,
        (Kind::Int | Kind::Unsigned | Kind::Decimal | Kind::Float, Some(v)) => match v.trim().parse::<f64>() {
            Ok(_) => Dflt::Sql(v.trim().to_string()),
            Err(_) => Dflt::Fill,
        },
        (Kind::Text, Some(v)) => Dflt::Sql(super::lit(&v)),
        // (MySQL's zero date is no date here)
        (Kind::Date | Kind::DateTime, Some(v)) if !v.starts_with("0000") => Dflt::Sql(super::lit(&v)),
        _ => Dflt::Fill,
    }
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
        "\nan administrator runs, once:\n\n  create table {db}.{MERGES} (id varchar(255) primary key, at datetime(6) not \
         null default current_timestamp(6));\n  grant select, insert on {db}.{MERGES} to <the user the URL names>;\n  \
         create table {db}.{MERGE_ROWS} (merge_id varchar(255) not null, n int not null, table_name varchar(255) not \
         null, op varchar(10) not null, row_key json, before_row json, after_row json, at datetime(6) not null \
         default current_timestamp(6), primary key (merge_id, n), key {MERGE_ROWS}_at (at));\n  grant select, \
         insert, delete on {db}.{MERGE_ROWS} to <the user the URL names>;\n",
        db = mq(&url.database)
    );
    // made here only where it isn't there: MySQL asks for the right to create a table even of one that exists, and
    // the user an administrator set up (the statements above) has no such right, and needs none
    if c.query(&format!("select 1 from {MERGES} limit 0")).is_err() {
        c.query(&format!(
            "create table if not exists {MERGES} (id varchar(255) primary key, at datetime(6) not null default \
             current_timestamp(6)) engine = InnoDB"
        ))
        .map_err(|e| format!("{} can't make the table {MERGES} in MySQL ({e}){setup}", url.user))?;
    }
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
            "select table_name, column_name, column_type, is_nullable, extra, ordinal_position, column_default from \
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
            // (MySQL 8 says DEFAULT_GENERATED of a column whose default is an expression: not a generated column)
            let computed = extra.contains("virtual") || extra.contains("stored") || extra.contains("persistent");
            let auto = extra.contains("auto_increment");
            let default = match computed || auto {
                true => Dflt::None,
                false => default_of(maria, &ty, col[6].as_deref(), &extra),
            };
            let fill = matches!(default, Dflt::Fill);
            match chronos_type(&ty) {
                // (held as a plain column, which MySQL alone gives values: see `Col::computed`)
                Ok(cty) if computed => defs.push(format!("{} {cty}", q(&cname))),
                // (a new row's id here is a provisional one, till MySQL gives its own at the merge: see `New`)
                Ok(cty) if auto && matches!(cty.as_str(), "integer" | "bigint") => {
                    defs.push(format!("{} {cty} generated by default as identity", q(&cname)));
                    t.seqs.push(Seq { col: t.cols.len(), pg: Some(super::PROVISIONAL.into()), counter: None });
                }
                Ok(cty) => {
                    let d = match &default {
                        Dflt::Sql(d) => format!(" default {d}"),
                        _ => String::new(),
                    };
                    // (a column MySQL fills is NULL here till it does)
                    let not_null = if nullable || fill { "" } else { " not null" };
                    defs.push(format!("{} {cty}{d}{not_null}", q(&cname)));
                    if auto {
                        // (no provisional id fits a column this narrow, or is told from a BIGINT UNSIGNED's own)
                        t.seqs.push(Seq { col: t.cols.len(), pg: None, counter: None });
                    }
                }
                Err(e) => {
                    refused_why.push(format!("  - {name}.{cname}: {e}"));
                    continue;
                }
            }
            let attnum = v(col, 5).parse::<i16>().unwrap_or(0);
            t.cols.push(Col { name: cname, ty, always: false, generated: false, attnum, fill, computed });
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
        for (t, defs, _) in made.iter_mut() {
            crate::sql::run(&db, MAIN, &format!("create table {} ({})", t.sql, defs.join(", ")), &[])
                .map_err(|e| format!("making {} here: {}", t.name, e.message))?;
            for sq in t.seqs.iter_mut().filter(|sq| sq.pg.is_some()) {
                sq.counter = crate::sql::column_counter(&db, &t.name, &t.cols[sq.col].name);
                if sq.counter.is_none() {
                    return Err(format!("making {} here: its AUTO_INCREMENT column has no counter", t.name));
                }
            }
            let t = &*t;
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
        let att = Attached::new_mysql(path, &url, made.into_iter().map(|(t, _, _)| t).collect(), maria);
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
        if t.seqs.iter().any(|sq| sq.pg.is_none()) {
            println!(
                "  {}: its AUTO_INCREMENT column is narrower than INT, or BIGINT UNSIGNED, so rows can't be inserted \
                 here (updates and deletes go)",
                t.name
            );
        }
    }
    if let Some(w) = super::durability_warning(&att) {
        println!("\n{w}");
    }
    println!(
        "\nnext: chronos serve {dir} (with CHRONOS_SOURCE_URL set), fork a world, change rows there, and MERGE WORLD:\n\
         the change goes to MySQL first. Not yet with MySQL: serve --follow, rehearsals, merge checks, \
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
    fn defaults_come_over_or_are_left_to_mysql() {
        // MySQL: a literal's bare text; an expression marked DEFAULT_GENERATED
        assert_eq!(default_of(false, "varchar(10)", Some("new"), ""), Dflt::Sql("'new'".into()));
        assert_eq!(default_of(false, "int", Some("3"), ""), Dflt::Sql("3".into()));
        assert_eq!(
            default_of(false, "datetime", Some("CURRENT_TIMESTAMP"), "default_generated"),
            Dflt::Sql("current_timestamp".into())
        );
        assert_eq!(default_of(false, "char(36)", Some("uuid()"), "default_generated"), Dflt::Fill);
        assert_eq!(default_of(false, "datetime", Some("0000-00-00 00:00:00"), ""), Dflt::Fill);
        assert_eq!(default_of(false, "int", None, ""), Dflt::None);
        // MariaDB: SQL, and NULL for none
        assert_eq!(default_of(true, "varchar(10)", Some("'it''s'"), ""), Dflt::Sql("'it''s'".into()));
        assert_eq!(default_of(true, "int", Some("3"), ""), Dflt::Sql("3".into()));
        assert_eq!(default_of(true, "int", Some("NULL"), ""), Dflt::None);
        assert_eq!(
            default_of(true, "datetime(6)", Some("current_timestamp(6)"), ""),
            Dflt::Sql("current_timestamp".into())
        );
        assert_eq!(default_of(true, "char(36)", Some("uuid()"), ""), Dflt::Fill);
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
