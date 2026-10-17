//! Reg values (regclass, regtype, ...) where Postgres can't be compared with: kept across forks and merges, read
//! and written by the HTTP row API, and the names a result's columns get.

mod common;

use chronos::sql::{Output, Session, Value, run};
use chronos::{Db, MAIN};
use std::io::{Read, Write};
use std::process::{Child, Command};

fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

/// A regclass written in a fork names the same table on main after the merge, though each world made other
/// tables meanwhile: an oid is the same in every world.
#[test]
fn a_kept_regclass_names_its_table_in_every_world() {
    let db = Db::new();
    run(&db, MAIN, "create table rgf_t (id int); create table rgf_k (r regclass)", &[]).unwrap();
    let mut s = Session::new(MAIN);
    let mut q = |sql: &str| s.run(&db, sql, &[]).map(|mut o| o.pop().unwrap()).unwrap();
    q("create branch rgf_b");
    q("use branch rgf_b");
    q("create table rgf_a0 (id int)");
    q("insert into rgf_k values ('rgf_t')");
    assert_eq!(rows(&q("select r from rgf_k")), ["rgf_t"]);
    run(&db, MAIN, "create table rgf_a1 (id int); create table rgf_a2 (id int)", &[]).unwrap();
    q("merge branch rgf_b");
    let on_main = run(&db, MAIN, "select r, r = 'rgf_t'::regclass from rgf_k", &[]).unwrap().pop().unwrap();
    assert_eq!(rows(&on_main), ["rgf_t|true"]);
}

/// The names a result's columns get, as Postgres's FigureColname gives them: a cast of a cast takes the outer type's
/// name, a cast of a column the column's.
#[test]
fn reg_casts_name_their_columns_as_postgres_does() {
    let db = Db::new();
    run(&db, MAIN, "create table rgn_t (r text)", &[]).unwrap();
    let o = run(
        &db,
        MAIN,
        "select 'rgn_t'::regclass::oid, 'int4'::regtype, r::regclass::text, r::regclass, to_regclass('rgn_t'), \
         pg_typeof(1), 'rgn_t'::regclass from rgn_t right join (select 1) x on true",
        &[],
    )
    .unwrap()
    .pop()
    .unwrap();
    let names: Vec<&str> = o.columns.as_ref().unwrap().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["oid", "regtype", "r", "r", "to_regclass", "pg_typeof", "regclass"]);
}

/// A string compared with a regclass is read as an oid, and says so (`invalid input syntax for type oid`).
#[test]
fn a_string_beside_a_regclass_is_read_as_an_oid() {
    let db = Db::new();
    let e = run(&db, MAIN, "select 'pg_class'::regclass = 'pg_class'", &[]).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("22P02", "invalid input syntax for type oid: \"pg_class\""));
}

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

/// POSTs `body` to /v1/`op`: the status and the reply.
fn post(port: u16, op: &str, body: &str) -> (u16, serde_json::Value) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let req = format!(
        "POST /v1/{op} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, json) = raw.split_once("\r\n\r\n").unwrap();
    (head.split(' ').nth(1).unwrap().parse().unwrap(), serde_json::from_str(json).unwrap_or_default())
}

/// The HTTP row API reads and writes a reg column by name, as SQL shows it: `put` takes a table's name (or an oid)
/// and `get` and `find` give the name back.
#[test]
fn the_row_api_reads_and_writes_reg_values_by_name() {
    let dir = std::env::temp_dir().join(format!("chronos-reg-oids-{}-api", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    let s = common::serve(&mut cmd, true);
    let server = Server(s.child, dir);
    let mut c =
        postgres::Client::connect(&format!("host=127.0.0.1 port={} user=admin dbname=main", s.pg), postgres::NoTls)
            .unwrap();
    c.batch_execute(
        "create table orders (id int primary key); create table rga (id text primary key, r regclass, t regtype)",
    )
    .unwrap();
    let (code, reply) = post(s.http, "put", r#"{"table": "rga", "id": "a", "record": {"r": "orders", "t": "int4"}}"#);
    assert_eq!(code, 200, "{reply}");
    let (code, reply) = post(s.http, "get", r#"{"table": "rga", "id": "a"}"#);
    assert_eq!(code, 200, "{reply}");
    assert_eq!((reply["row"]["r"].as_str(), reply["row"]["t"].as_str()), (Some("orders"), Some("integer")), "{reply}");
    let (code, reply) = post(s.http, "find", r#"{"table": "rga"}"#);
    assert_eq!(code, 200, "{reply}");
    assert_eq!(reply["hits"][0]["row"]["r"].as_str(), Some("orders"), "{reply}");
    let r: String = c.query_one("select r::text from rga where id = 'a'", &[]).unwrap().get(0);
    assert_eq!(r, "orders");
    drop(server);
}

/// The row API's reading of reg columns leaves every other row as the engine gives it: a scan of the whole keyspace
/// of a database with SQL tables (the database's own `_sonos` rows too) and a get of one of those rows work as
/// before, while a SQL table's reg column reads as its name.
#[test]
fn the_row_api_passes_other_rows_through() {
    let db = Db::new();
    run(&db, MAIN, "create table rgs_t (id int primary key, r regclass); insert into rgs_t values (1, 'rgs_t')", &[])
        .unwrap();
    db.put(MAIN, "notes/1", r#"{"text": "hi"}"#).unwrap();
    let rows = db.scan(MAIN, "", usize::MAX).unwrap();
    let own: Vec<&String> = rows.iter().map(|(k, _)| k).filter(|k| k.starts_with("_sonos")).collect();
    assert!(!own.is_empty(), "the database's own rows are scanned too: {rows:?}");
    assert!(db.get(MAIN, own[0]).unwrap().is_some());
    let row = rows.iter().find(|(k, _)| k.starts_with("rgs_t/")).map(|(_, v)| v.to_string()).unwrap();
    assert!(row.contains(r#""r":"rgs_t""#), "{row}");
    assert!(rows.iter().any(|(k, v)| k == "notes/1" && v.contains("hi")));
}
