//! The SQL ORMs write, through a real driver with typed parameters: what the differential
//! tests (`pgdiff.rs`, literal SQL) can't show is that a driver can bind its parameters.

mod common;

use postgres::{Client, NoTls};
use std::process::{Child, Command};

struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(name: &str) -> (Server, Client) {
    let dir = std::env::temp_dir().join(format!("chronos-surface-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    let s = common::serve(&mut cmd, true);
    let c = Client::connect(&format!("host=127.0.0.1 port={} user=orm dbname=main", s.pg), NoTls).unwrap();
    (Server(s.child, dir), c)
}

fn ids(rows: Vec<postgres::Row>) -> Vec<i32> {
    rows.iter().map(|r| r.get(0)).collect()
}

#[test]
fn keyset_pagination_binds_its_row_parameters() {
    let (_server, mut c) = start("keyset");
    c.batch_execute(
        "create table posts (id int primary key, day int, title text);
         insert into posts values (1, 10, 'a'), (2, 10, 'b'), (3, 20, 'c'), (4, 20, 'd'), (5, 30, 'e')",
    )
    .unwrap();
    // (day, id) < ($1, $2): the page after the last row seen
    let page = c
        .query(
            "select id from posts where (day, id) < ($1, $2) order by day desc, id desc limit $3",
            &[&30i32, &5i32, &2i64],
        )
        .unwrap();
    assert_eq!(ids(page), [4, 3]);
    let next = c
        .query(
            "select id from posts where (day, id) < ($1, $2) order by day desc, id desc limit $3",
            &[&20i32, &3i32, &2i64],
        )
        .unwrap();
    assert_eq!(ids(next), [2, 1]);
    // ascending, with text parameters and a row IN list
    let after =
        c.query("select id from posts where (day, title) > ($1, $2) order by day, title", &[&10i32, &"a"]).unwrap();
    assert_eq!(ids(after), [2, 3, 4, 5]);
    let some = c
        .query(
            "select id from posts where (day, id) in (($1, $2), ($3, $4)) order by id",
            &[&10i32, &2i32, &30i32, &5i32],
        )
        .unwrap();
    assert_eq!(ids(some), [2, 5]);
    // a prepared statement, run again
    let stmt = c.prepare("select id from posts where (day, id) >= ($1, $2) order by day, id").unwrap();
    assert_eq!(ids(c.query(&stmt, &[&20i32, &4i32]).unwrap()), [4, 5]);
    assert_eq!(ids(c.query(&stmt, &[&10i32, &1i32]).unwrap()), [1, 2, 3, 4, 5]);
}

#[test]
fn fetch_first_and_offset_bind_as_bigint() {
    let (_server, mut c) = start("fetch");
    c.batch_execute("create table t (id int primary key); insert into t select g from generate_series(1, 6) g")
        .unwrap();
    let rows = c.query("select id from t order by id offset $1 rows fetch next $2 rows only", &[&2i64, &3i64]).unwrap();
    assert_eq!(ids(rows), [3, 4, 5]);
    let rows = c.query("select id from t order by id fetch first $1 rows only", &[&1i64]).unwrap();
    assert_eq!(ids(rows), [1]);
}

#[test]
fn distinct_on_with_parameters_and_arrays() {
    let (_server, mut c) = start("distinct");
    c.batch_execute(
        "create table events (id int primary key, kind text, at int, tags text[]);
         insert into events values (1, 'a', 5, '{x,y}'), (2, 'a', 9, '{y}'), (3, 'b', 1, '{x}'), (4, 'b', 2, '{z}'), (5, 'c', 7, '{}')",
    )
    .unwrap();
    // the latest event of each kind: what Django's distinct('kind') and Rails write
    let latest =
        c.query("select distinct on (kind) id from events where at >= $1 order by kind, at desc", &[&2i32]).unwrap();
    assert_eq!(ids(latest), [2, 4, 5]);
    // array operators with array parameters, and a correlated ARRAY subquery
    let tagged = c.query("select id from events where tags && $1 order by id", &[&vec!["x", "z"]]).unwrap();
    assert_eq!(ids(tagged), [1, 3, 4]);
    let has = c.query("select id from events where tags @> $1 order by id", &[&vec!["y"]]).unwrap();
    assert_eq!(ids(has), [1, 2]);
    // (read as text: array results aren't sent in binary yet, so a driver can't decode them as int4[])
    let ordered: String = c
        .query_one("select array(select e.id from events e where e.kind = $1 order by e.at desc)::text", &[&"a"])
        .unwrap()
        .get(0);
    assert_eq!(ordered, "{2,1}");
    let per_row = c
        .query(
            "select id, array(select e2.id from events e2 where e2.kind = events.kind and e2.id < events.id order by e2.id)::text \
             from events order by id",
            &[],
        )
        .unwrap();
    let got: Vec<(i32, String)> = per_row.iter().map(|r| (r.get(0), r.get(1))).collect();
    let want = [(1, "{}"), (2, "{1}"), (3, "{}"), (4, "{3}"), (5, "{}")];
    assert_eq!(got, want.map(|(i, a)| (i, a.to_string())));
    // IS TRUE with a boolean parameter, and a power with a double
    let flag: bool = c.query_one("select $1::boolean is true", &[&true]).unwrap().get(0);
    assert!(flag);
    let p: f64 = c.query_one("select $1::float8 ^ 2", &[&1.5f64]).unwrap().get(0);
    assert_eq!(p, 2.25);
}
