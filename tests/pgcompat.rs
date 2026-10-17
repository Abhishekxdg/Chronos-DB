//! The Phase 6 exit test: app-style queries give the same rows on Chronos as on Postgres, and Chronos
//! never reads a whole table where Postgres would use an index.
//!
//! Needs a Postgres at host=/tmp (it's skipped when there's none): the same schema, indexes and
//! data go into both. The Postgres database uses the C collation, because Chronos compares text by
//! its bytes. PGCOMPAT_QUERIES sets how many queries (default 240).

use chronos::sql::{Value, run};
use chronos::{Db, MAIN};
use postgres::{Client, NoTls, SimpleQueryMessage};

/// The reference Postgres: its socket in /tmp, on the port PGPORT names (as psql takes it), else 5432.
fn oracle(dbname: &str) -> String {
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    format!("host=/tmp port={port} dbname={dbname}")
}

const SCHEMA: &str = "
create table users (id int primary key, name text not null, email text unique, age int, city text,
                    active boolean, joined date);
create table orders (id int primary key, user_id int references users (id), amount int, status text,
                     placed timestamp);
create index users_age on users (age);
create index users_city_age on users (city, age);
create index orders_user on orders (user_id);
create index orders_placed on orders (placed);
create index orders_status_amount on orders (status, amount)";

const CITIES: [&str; 6] = ["Austin", "Berlin", "Chennai", "Delhi", "berlin", "Zurich"];
const STATUSES: [&str; 4] = ["new", "paid", "shipped", "void"];
const USERS: u64 = 4000;
const ORDERS: u64 = 12000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

fn data(r: &mut Rng) -> Vec<String> {
    let mut out = vec![];
    let mut rows = vec![];
    for id in 1..=USERS {
        let age = if r.below(20) == 0 { "null".to_string() } else { (18 + r.below(60)).to_string() };
        let city = if r.below(15) == 0 { "null".to_string() } else { format!("'{}'", r.pick(&CITIES)) };
        let active = ["true", "false"][r.below(2) as usize];
        let joined = format!("'202{}-{:02}-{:02}'", r.below(5), 1 + r.below(12), 1 + r.below(28));
        rows.push(format!("({id}, 'user {id}', 'u{id}@x.io', {age}, {city}, {active}, {joined})"));
        if rows.len() == 500 {
            out.push(format!("insert into users values {}", rows.join(", ")));
            rows.clear();
        }
    }
    for id in 1..=ORDERS {
        let user = 1 + r.below(USERS);
        let amount = if r.below(25) == 0 { "null".to_string() } else { (r.below(1000)).to_string() };
        let placed = format!(
            "'202{}-{:02}-{:02} {:02}:{:02}:00'",
            r.below(5),
            1 + r.below(12),
            1 + r.below(28),
            r.below(24),
            r.below(60)
        );
        rows.push(format!("({id}, {user}, {amount}, '{}', {placed})", r.pick(&STATUSES)));
        if rows.len() == 500 {
            out.push(format!("insert into orders values {}", rows.join(", ")));
            rows.clear();
        }
    }
    out
}

/// A random app-style query. Queries with ORDER BY order by a unique column last, so the row
/// order is fixed; the others are compared as sets of rows.
fn query(r: &mut Rng) -> String {
    let age = 18 + r.below(60);
    let age2 = age + r.below(15);
    let city = r.pick(&CITIES);
    let status = r.pick(&STATUSES);
    let amt = r.below(1000);
    let user = 1 + r.below(USERS);
    let day = format!("'202{}-{:02}-{:02}'", r.below(5), 1 + r.below(12), 1 + r.below(28));
    let lim = 1 + r.below(15);
    match r.below(40) {
        0 => format!("select id, name from users where id = {user}"),
        1 => format!("select id from users where age = {age}"),
        2 => format!("select id from users where age between {age} and {age2}"),
        3 => format!("select id from users where age > {age} and age <= {age2} and active"),
        4 => format!("select id from users where city = '{city}' and age >= {age}"),
        5 => format!("select id, age from users where city = '{city}' order by age desc, id limit {lim}"),
        6 => format!("select id from users where {age} < age and age < {age2} and city = '{city}'"),
        7 => format!("select id from users where email = 'u{user}@x.io'"),
        8 => "select count(*) from users where age is null".to_string(),
        9 => "select city, count(*), min(age), max(age) from users group by city".to_string(),
        10 => format!("select city, count(*) from users where age > {age} group by city having count(*) > 5"),
        11 => format!("select id from orders where user_id = {user}"),
        12 => format!("select id, amount from orders where status = '{status}' and amount > {amt}"),
        13 => format!("select id from orders where placed >= {day} and placed < {day}::date + 7 order by placed, id"),
        14 => format!("select id, placed from orders order by placed desc, id limit {lim}"),
        15 => "select status, count(*), sum(amount) from orders group by status".to_string(),
        16 => "select status, avg(amount) from orders where amount is not null group by status".to_string(),
        17 => format!(
            "select u.id, count(o.id) from users u join orders o on o.user_id = u.id where u.age = {age} group by u.id"
        ),
        18 => format!(
            "select u.name, o.amount from users u join orders o on o.user_id = u.id where o.status = '{status}' and o.amount > {amt}"
        ),
        19 => format!(
            "select u.id from users u left join orders o on o.user_id = u.id where o.id is null and u.age = {age}"
        ),
        20 => format!(
            "select id from users where id in (select user_id from orders where amount > {})",
            900 + r.below(100)
        ),
        21 => format!(
            "select id from users u where exists (select 1 from orders o where o.user_id = u.id and o.status = '{status}') and age = {age}"
        ),
        22 => format!("select id from users where city like '{}%'", &city[..1]),
        23 => format!("select id from users where lower(city) = '{}' and age < {age}", city.to_lowercase()),
        24 => "select coalesce(city, 'none'), count(*) from users group by coalesce(city, 'none')".to_string(),
        25 => format!(
            "select id, case when amount >= 500 then 'big' when amount is null then 'unknown' else 'small' end from orders where user_id = {user}"
        ),
        26 => format!("select distinct status from orders where amount < {amt}"),
        27 => format!("select id from users where age = {age} union select user_id from orders where amount = {amt}"),
        28 => format!(
            "with big as (select user_id, sum(amount) s from orders group by user_id) select user_id from big where s > {}",
            3000 + r.below(2000)
        ),
        29 => format!(
            "select id, amount, rank() over (order by amount desc) from orders where user_id = {user} order by amount desc, id"
        ),
        30 => {
            format!("select id from orders where amount between {amt} and {} order by amount, id limit {lim}", amt + 20)
        }
        31 => format!("select count(*) from orders where placed < {day}"),
        32 => format!("select id from users where joined > {day} and active = false"),
        33 => format!("select max(amount), min(placed) from orders where status = '{status}'"),
        34 => format!("select id from users where age in ({age}, {age2}, {})", age + 1),
        35 => format!("select id from users where not (age > {age}) and city = '{city}'"),
        36 => {
            format!("select user_id, count(*) from orders group by user_id order by count(*) desc, user_id limit {lim}")
        }
        37 => format!("select id, amount from orders where status = '{status}' order by amount desc, id limit {lim}"),
        38 => format!(
            "select id from users where age is not null and age < {} order by age, id limit {lim}",
            20 + r.below(5)
        ),
        _ => format!("select name from users where id = {user} and email like 'u%'"),
    }
}

/// A cell as text, with the spellings that differ only in form made the same.
fn norm(s: Option<&str>) -> String {
    match s {
        None => "null".into(),
        Some("t") => "true".into(),
        Some("f") => "false".into(),
        Some(x) => match x.parse::<f64>() {
            Ok(n) if x.contains('.') || x.contains('e') => format!("{:.6}", n),
            _ => x.to_string(),
        },
    }
}

fn chronos_rows(db: &Db, q: &str) -> Result<Vec<Vec<String>>, String> {
    let o = run(db, MAIN, q, &[]).map_err(|e| e.to_string())?.pop().unwrap();
    Ok(o.rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => norm(None),
                    Value::Float(f) => format!("{f:.6}"),
                    v => norm(Some(&v.to_text())),
                })
                .collect()
        })
        .collect())
}

fn pg_rows(c: &mut Client, q: &str) -> Result<Vec<Vec<String>>, String> {
    let msgs = c.simple_query(q).map_err(|e| e.to_string())?;
    Ok(msgs
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| norm(r.get(i))).collect()),
            _ => None,
        })
        .collect())
}

/// The tables a plan reads through an index, and those it reads whole.
fn scans(plan: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut indexed, mut whole) = (vec![], vec![]);
    for l in plan {
        let on = |l: &str| l.split(" on ").nth(1).and_then(|t| t.split_whitespace().next()).map(String::from);
        if l.contains("Index Scan") || l.contains("Index Only Scan") || l.contains("Bitmap Scan") {
            indexed.extend(on(l));
        } else if l.contains("Seq Scan") {
            whole.extend(on(l));
        }
    }
    (indexed, whole)
}

/// The tables Postgres reads through an index for a selective condition: its estimate of the
/// rows the index finds is under a fifth of the table (the point past which Chronos, like any
/// engine reading from memory, reads the table in order instead).
fn pg_selective(plan: &[String]) -> Vec<String> {
    let rows = |l: &str| -> f64 {
        l.split("rows=")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|r| r.parse().ok())
            .unwrap_or(f64::MAX)
    };
    let size = |t: &str| match t {
        "users" | "u" => USERS as f64,
        "orders" | "o" => ORDERS as f64,
        _ => f64::MAX,
    };
    let table = |l: &str| l.split(" on ").nth(1).and_then(|t| t.split_whitespace().next()).map(String::from);
    let (mut out, mut heap): (Vec<String>, Option<(String, f64)>) = (vec![], None);
    let flush = |heap: &mut Option<(String, f64)>, out: &mut Vec<String>| {
        if let Some((t, n)) = heap.take()
            && n < size(&t) / 5.0
        {
            out.push(t);
        }
    };
    for l in plan {
        if l.contains("Bitmap Heap Scan") {
            flush(&mut heap, &mut out);
            heap = table(l).map(|t| (t, 0.0));
        } else if l.contains("Bitmap Index Scan") {
            if let Some((_, n)) = &mut heap {
                *n += rows(l);
            }
        } else if l.contains("Index Scan") || l.contains("Index Only Scan") {
            flush(&mut heap, &mut out);
            if let Some(t) = table(l)
                && (rows(l) < size(&t) / 5.0 || l.contains("Limit") || plan.iter().any(|p| p.starts_with("Limit")))
            {
                out.push(t);
            }
        }
    }
    flush(&mut heap, &mut out);
    out
}

#[test]
fn same_rows_as_postgres_and_indexes_where_postgres_uses_them() {
    let Ok(mut admin) = Client::connect(&oracle("postgres"), NoTls) else {
        eprintln!("skipped: no Postgres at host=/tmp");
        return;
    };
    let _ = admin.batch_execute("drop database if exists chronos_compat");
    admin
        .batch_execute("create database chronos_compat template template0 encoding 'UTF8' lc_collate 'C' lc_ctype 'C'")
        .unwrap();
    let mut pg = Client::connect(&oracle("chronos_compat"), NoTls).unwrap();
    let db = Db::new();
    let mut r = Rng(20260923);
    pg.batch_execute(SCHEMA).unwrap();
    run(&db, MAIN, SCHEMA, &[]).unwrap();
    for stmt in data(&mut r) {
        pg.batch_execute(&stmt).unwrap();
        run(&db, MAIN, &stmt, &[]).unwrap();
    }
    pg.batch_execute("analyze").unwrap();

    let n: usize = std::env::var("PGCOMPAT_QUERIES").ok().and_then(|s| s.parse().ok()).unwrap_or(240);
    let (mut wrong, mut scanned, mut indexed_by_pg) = (vec![], vec![], 0);
    for _ in 0..n {
        let q = query(&mut r);
        let (mut a, mut b) = (chronos_rows(&db, &q), pg_rows(&mut pg, &q));
        if !q.contains("order by") {
            for x in [&mut a, &mut b].into_iter().flatten() {
                x.sort();
            }
        }
        if a != b {
            wrong.push(format!("{q}\n  chronos:    {:?}\n  postgres: {:?}", trunc(&a), trunc(&b)));
            continue;
        }
        // where Postgres reads a table through an index, Chronos must not read all of it
        let pg_plan: Vec<String> = pg_rows(&mut pg, &format!("explain {q}")).unwrap().into_iter().flatten().collect();
        let pg_ix = pg_selective(&pg_plan);
        indexed_by_pg += !pg_ix.is_empty() as usize;
        let Ok(plan) = chronos_rows(&db, &format!("explain {q}")) else { continue };
        let plan: Vec<String> = plan.into_iter().flatten().collect();
        let (_, whole) = scans(&plan);
        let alias_of = |t: &str| {
            if t == "u" {
                "users"
            } else if t == "o" {
                "orders"
            } else {
                t
            }
            .to_string()
        };
        let pg_ix: Vec<String> = pg_ix.iter().map(|t| alias_of(t)).collect();
        if let Some(t) = whole.iter().find(|t| pg_ix.contains(t)) {
            scanned.push(format!("{q}\n  Chronos reads all of {t}; Postgres: {pg_plan:?}\n  Chronos: {plan:?}"));
        }
    }
    let _ = admin.batch_execute("drop database if exists chronos_compat");
    eprintln!("{n} queries; Postgres used an index in {indexed_by_pg}");
    assert!(wrong.is_empty(), "{} of {n} queries differ from Postgres:\n{}", wrong.len(), wrong.join("\n"));
    assert!(scanned.is_empty(), "{} full scans where Postgres used an index:\n{}", scanned.len(), scanned.join("\n"));
}

fn trunc(x: &Result<Vec<Vec<String>>, String>) -> String {
    match x {
        Ok(rows) if rows.len() > 6 => format!("{} rows, first {:?}", rows.len(), &rows[..6]),
        Ok(rows) => format!("{rows:?}"),
        Err(e) => format!("error: {e}"),
    }
}
