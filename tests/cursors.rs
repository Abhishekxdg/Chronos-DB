//! SQL cursors (DECLARE / FETCH / MOVE / CLOSE) and the protocol's row limit on Execute
//! (PortalSuspended): what Postgres does with each FETCH is in tests/pgdiff.rs (`cursors`). Here:
//! what a driver does with them, command tags, a cursor's snapshot, and what an agent may do.

mod common;

use chronos::sql::{Session, Value};
use chronos::{AgentSpec, Can, Db, MAIN};
use postgres::{Client, NoTls, SimpleQueryMessage};
use std::process::{Child, Command};

/// The server, stopped (and its folder removed) however the test ends.
struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn start(name: &str) -> (Server, Client) {
    let dir = std::env::temp_dir().join(format!("chronos-cursors-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.args(["serve", dir.to_str().unwrap()]);
    let s = common::serve(&mut cmd, true);
    let mut c = Client::connect(&format!("host=127.0.0.1 port={} user=t dbname=main", s.pg), NoTls).unwrap();
    c.batch_execute(
        "create table t (id int primary key, v text); insert into t select g, 'v' || g from generate_series(1, 10) g",
    )
    .unwrap();
    (Server(s.child, dir), c)
}

fn ids(rows: Vec<postgres::Row>) -> Vec<i32> {
    rows.iter().map(|r| r.get(0)).collect()
}

/// The number a command's tag ends in (`FETCH 3`, `MOVE 2`).
fn counted(c: &mut Client, sql: &str) -> u64 {
    c.simple_query(sql)
        .unwrap()
        .iter()
        .find_map(|m| if let SimpleQueryMessage::CommandComplete(n) = m { Some(*n) } else { None })
        .unwrap_or_else(|| panic!("{sql} has no command tag"))
}

#[test]
fn an_execute_with_a_row_limit_is_suspended_and_resumed() {
    let (_server, mut c) = start("limit");
    let mut tx = c.transaction().unwrap();
    let stmt = tx.prepare("select id from t where id > $1 order by id").unwrap();
    let portal = tx.bind(&stmt, &[&2i32]).unwrap();
    assert_eq!(ids(tx.query_portal(&portal, 3).unwrap()), [3, 4, 5]);
    assert_eq!(ids(tx.query_portal(&portal, 3).unwrap()), [6, 7, 8]);
    // fewer left than asked for: they all come, and the portal is done
    assert_eq!(ids(tx.query_portal(&portal, 3).unwrap()), [9, 10]);
    // no limit: everything
    let all = tx.bind(&stmt, &[&8i32]).unwrap();
    assert_eq!(ids(tx.query_portal(&all, 0).unwrap()), [9, 10]);
    // as many as there are still counts as "maybe more", as in Postgres: the next Execute finds nothing
    let exact = tx.bind(&stmt, &[&8i32]).unwrap();
    assert_eq!(ids(tx.query_portal(&exact, 2).unwrap()), [9, 10]);
    assert!(tx.query_portal(&exact, 2).unwrap().is_empty());
    // two portals of one statement each go on from where they are
    let (a, b) = (tx.bind(&stmt, &[&0i32]).unwrap(), tx.bind(&stmt, &[&5i32]).unwrap());
    assert_eq!(ids(tx.query_portal(&a, 2).unwrap()), [1, 2]);
    assert_eq!(ids(tx.query_portal(&b, 2).unwrap()), [6, 7]);
    assert_eq!(ids(tx.query_portal(&a, 2).unwrap()), [3, 4]);
    tx.commit().unwrap();
}

#[test]
fn a_portal_that_has_run_to_its_end_cannot_be_run_again() {
    let (_server, mut c) = start("done");
    let mut tx = c.transaction().unwrap();
    let stmt = tx.prepare("insert into t values ($1, 'x') returning id").unwrap();
    let portal = tx.bind(&stmt, &[&11i32]).unwrap();
    assert_eq!(ids(tx.query_portal(&portal, 0).unwrap()), [11]);
    // as in Postgres (55000): running it again would insert again
    let e = tx.query_portal(&portal, 0).unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("55000"), "{e}");
    drop(tx);
    assert_eq!(c.query_one("select count(*) from t where id = 11", &[]).unwrap().get::<_, i64>(0), 0);
}

#[test]
fn a_named_cursor_through_a_driver() {
    let (_server, mut c) = start("named");
    let mut tx = c.transaction().unwrap();
    // psycopg's server-side cursor: DECLARE with parameters, FETCH n, MOVE, CLOSE
    tx.execute("declare \"cur\" scroll cursor without hold for select id from t where id > $1 order by id", &[&4i32])
        .unwrap();
    assert_eq!(ids(tx.query("fetch forward 2 from \"cur\"", &[]).unwrap()), [5, 6]);
    assert_eq!(ids(tx.query("fetch forward 2 from \"cur\"", &[]).unwrap()), [7, 8]);
    assert_eq!(ids(tx.query("fetch backward 1 from \"cur\"", &[]).unwrap()), [7]);
    tx.batch_execute("move absolute 0 in \"cur\"").unwrap();
    assert_eq!(ids(tx.query("fetch all from \"cur\"", &[]).unwrap()), [5, 6, 7, 8, 9, 10]);
    let row = tx.query_one("fetch absolute 2 from \"cur\"", &[]).unwrap();
    assert_eq!((row.get::<_, i32>(0), row.columns()[0].name()), (6, "id"));
    tx.batch_execute("close \"cur\"").unwrap();
    let e = tx.query("fetch next from \"cur\"", &[]).unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("34000"), "{e}");
}

/// pg_cursors and pg_prepared_statements show the query string a driver's Parse carried, as Postgres does,
/// not the last simple query.
#[test]
fn the_views_show_the_text_a_driver_sent_through_the_extended_protocol() {
    let (_server, mut c) = start("text");
    let mut tx = c.transaction().unwrap();
    let declare = "declare k cursor for select id from t where id > $1";
    tx.execute(declare, &[&4i32]).unwrap();
    let row = tx.query_one("select name, statement from pg_cursors", &[]).unwrap();
    assert_eq!((row.get::<_, String>(0), row.get::<_, String>(1)), ("k".into(), declare.into()));
    let prepare = "prepare q as select 1";
    tx.execute(prepare, &[]).unwrap();
    let row = tx.query_one("select statement from pg_prepared_statements where name = 'q'", &[]).unwrap();
    assert_eq!(row.get::<_, String>(0), prepare);
    tx.rollback().unwrap();
}

/// A driver's named statement is listed in pg_prepared_statements with its parameter and result types as regtype
/// shows them, a declared parameter it doesn't use among them (and Bind takes a value for it).
#[test]
fn a_drivers_statement_lists_its_parameter_and_result_types() {
    use postgres::types::Type;
    let (_server, mut c) = start("types");
    let sql = "select id, v, now() from t where id = $1";
    let st = c.prepare_typed(sql, &[Type::INT4, Type::TEXT]).unwrap();
    assert_eq!(st.params(), [Type::INT4, Type::TEXT]);
    assert_eq!(c.query(&st, &[&3i32, &"x"]).unwrap().len(), 1);
    let listed = format!(
        "select parameter_types::text, result_types::text, array_length(parameter_types, 1) \
         from pg_prepared_statements where statement = '{sql}'"
    );
    let rows: Vec<Vec<Option<String>>> = (c.simple_query(&listed).unwrap().iter())
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| r.get(i).map(String::from)).collect()),
            _ => None,
        })
        .collect();
    let want = ["{integer,text}", "{integer,text,\"timestamp with time zone\"}", "2"];
    assert_eq!(rows, [want.map(|s| Some(s.to_string()))]);
}

/// A value as the server sent it, whatever its type.
struct Raw(Vec<u8>);

impl<'a> postgres::types::FromSql<'a> for Raw {
    fn from_sql(_: &postgres::types::Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(raw.to_vec()))
    }
    fn accepts(_: &postgres::types::Type) -> bool {
        true
    }
}

/// A regtype[] in binary as Postgres sends it: dimensions, a flag for nulls, element type regtype, then the length
/// and lower bound, then each 4-byte oid after its length (-1 for a null, written here as None).
fn regtype_array(oids: &[Option<i32>]) -> Vec<u8> {
    let nulls = oids.iter().any(Option::is_none) as i32;
    let head = [1, nulls, 2206, oids.len() as i32, 1];
    let body = oids.iter().flat_map(|o| o.map_or(vec![-1], |o| vec![4, o]));
    head.into_iter().chain(body).flat_map(i32::to_be_bytes).collect()
}

/// A driver reading pg_prepared_statements' type lists in binary gets regtype[] as Postgres sends it: a binary
/// array of type oids (an enum's its own, named as the search_path shows it), not the names' text.
#[test]
fn a_drivers_binary_read_of_the_type_lists_is_an_array_of_oids() {
    let (_server, mut c) = start("regtype-binary");
    c.batch_execute(
        "create schema s1; create type s1.mood as enum ('ok'); set search_path = s1, public; \
         prepare b1 (int, text, mood, timestamptz) as select $1, $3",
    )
    .unwrap();
    let rows = c
        .query(
            "select parameter_types, result_types, 'mood'::regtype::oid from pg_prepared_statements where name = 'b1'",
            &[],
        )
        .unwrap();
    assert_eq!(rows[0].columns()[0].type_().oid(), 2211);
    let mood = rows[0].get::<_, u32>(2) as i32; // (`::oid` is an oid)
    let array = |oids: &[i32]| regtype_array(&oids.iter().copied().map(Some).collect::<Vec<_>>());
    assert_eq!(rows[0].get::<_, Raw>(0).0, array(&[23, 25, mood, 1184]));
    assert_eq!(rows[0].get::<_, Raw>(1).0, array(&[23, mood]));
}

/// Every regtype[] goes out in binary as oids, never as its names' text: one naming tsvector and tsquery, one with
/// a null, and one in a table kept after the search_path stops finding its enum by the name stored, or after the
/// enum is dropped.
#[test]
fn every_regtype_array_goes_out_in_binary_as_oids() {
    let (_server, mut c) = start("regtype-binary-all");
    c.batch_execute(
        "create schema s2; create type s2.mood as enum ('ok'); set search_path = s2, public; \
         prepare z4 (tsvector, tsquery, int) as select $1 @@ $2, to_tsvector('a'); \
         prepare z5 (mood) as select $1; \
         create table rt as select name, parameter_types from pg_prepared_statements; \
         insert into rt values ('n', '{integer,NULL}'); \
         set search_path = public",
    )
    .unwrap();
    let mood = c.query_one("select 's2.mood'::regtype::oid::bigint", &[]).unwrap().get::<_, i64>(0) as i32;
    let row = c.query_one("select parameter_types, result_types from pg_prepared_statements where name = 'z4'", &[]);
    let row = row.unwrap();
    assert_eq!(row.get::<_, Raw>(0).0, regtype_array(&[Some(3614), Some(3615), Some(23)]));
    assert_eq!(row.get::<_, Raw>(1).0, regtype_array(&[Some(16), Some(3614)]));
    let kept: Vec<Vec<u8>> = (c.query("select parameter_types from s2.rt order by name", &[]).unwrap())
        .iter()
        .map(|r| r.get::<_, Raw>(0).0)
        .collect();
    let want = [Some(23), None];
    assert_eq!(
        kept,
        [regtype_array(&want), regtype_array(&[Some(3614), Some(3615), Some(23)]), regtype_array(&[Some(mood)])]
    );
    // a type dropped since: a regtype[] keeps its oid, as Postgres's does, and is still an array of oids; an enum
    // made after it has an oid of its own, so the kept one doesn't name it
    c.batch_execute("deallocate z5; drop type s2.mood; create type s2.mood2 as enum ('ok')").unwrap();
    let other = c.query_one("select 's2.mood2'::regtype::oid::bigint", &[]).unwrap().get::<_, i64>(0) as i32;
    assert_ne!(other, mood, "the next enum's oid isn't the dropped one's");
    let row = c.query_one("select parameter_types from s2.rt where name = 'z5'", &[]).unwrap();
    assert_eq!(row.get::<_, Raw>(0).0, regtype_array(&[Some(mood)]));
    let row = c.query_one("select parameter_types::text from s2.rt where name = 'z5'", &[]).unwrap();
    assert_ne!(row.get::<_, String>(0), "{s2.mood2}");
}

/// asyncpg looks up every type id a result names that it has no codec for (its `typeinfo_tree` query, which Chronos
/// answers itself): regtype[], the type of pg_prepared_statements' type lists, is answered as an array of regtype, so
/// asyncpg can read them.
#[test]
fn asyncpg_finds_the_regtype_array_type() {
    use postgres::types::Type;
    let (_server, mut c) = start("typeinfo");
    let q = "WITH RECURSIVE typeinfo_tree(oid) AS (SELECT unnest($1::oid[])) SELECT * FROM typeinfo_tree";
    let st = c.prepare_typed(q, &[Type::OID_ARRAY]).unwrap();
    let rows = c.query(&st, &[&vec![2211u32, 1007u32]]).unwrap();
    let mut found: Vec<(String, String)> =
        rows.iter().map(|r| (r.get::<_, String>("name"), r.get::<_, String>("elemtype_name"))).collect();
    found.sort();
    assert_eq!(found, [("_int4".to_string(), "integer".to_string()), ("_regtype".into(), "regtype".into())]);
}

#[test]
fn command_tags_count_the_rows() {
    let (_server, mut c) = start("tags");
    c.batch_execute("begin; declare k scroll cursor for select id from t order by id").unwrap();
    assert_eq!(counted(&mut c, "fetch 4 from k"), 4);
    assert_eq!(counted(&mut c, "fetch all from k"), 6);
    assert_eq!(counted(&mut c, "fetch next from k"), 0);
    assert_eq!(counted(&mut c, "move backward 3 from k"), 3);
    assert_eq!(counted(&mut c, "move backward all from k"), 7); // from the 8th row back over the 7 before it
    assert_eq!(counted(&mut c, "move forward 100 from k"), 10);
    assert_eq!(counted(&mut c, "fetch prior from k"), 1);
    c.batch_execute("rollback").unwrap();
}

#[test]
fn a_cursor_keeps_the_rows_it_was_declared_over() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    let mut b = Session::new(MAIN);
    a.run(&db, "create table t (id int primary key)", &[]).unwrap();
    a.run(&db, "insert into t values (1), (2), (3)", &[]).unwrap();
    a.run(&db, "begin", &[]).unwrap();
    a.run(&db, "declare k cursor for select id from t order by id", &[]).unwrap();
    b.run(&db, "insert into t values (4)", &[]).unwrap();
    b.run(&db, "delete from t where id = 1", &[]).unwrap();
    let out = a.run(&db, "fetch all from k", &[]).unwrap().pop().unwrap();
    let got: Vec<String> = out.rows.iter().map(|r| r[0].to_text()).collect();
    assert_eq!(got, ["1", "2", "3"], "what it was declared over, not what the table became");
    a.run(&db, "commit", &[]).unwrap();
    let out = a.run(&db, "select count(*) from t", &[]).unwrap().pop().unwrap();
    assert_eq!(out.rows[0][0], Value::Int(3));
}

#[test]
fn a_cursor_ends_with_its_transaction_and_discard_closes_the_rest() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for q in [
        "create table t (id int primary key)",
        "insert into t values (1), (2)",
        "begin",
        "declare gone cursor for select id from t",
        "declare kept cursor with hold for select id from t",
        "commit",
    ] {
        s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    assert_eq!(s.run(&db, "fetch gone", &[]).err().map(|e| e.code), Some("34000"));
    assert_eq!(s.run(&db, "fetch kept", &[]).unwrap()[0].rows.len(), 1);
    s.run(&db, "discard all", &[]).unwrap();
    assert_eq!(s.run(&db, "fetch kept", &[]).err().map(|e| e.code), Some("34000"));
}

#[test]
fn an_agent_that_can_only_read_may_hold_a_cursor_over_what_it_may_read() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    s.run(&db, "create table t (id int primary key)", &[]).unwrap();
    s.run(&db, "insert into t values (1), (2), (3)", &[]).unwrap();
    db.create_agent("reader", &AgentSpec { can: Some(vec![Can::Read]), ..Default::default() }).unwrap();
    let mut r = Session::new(MAIN);
    r.agent = Some("reader".into());
    r.run(&db, "declare k cursor with hold for select id from t order by id", &[]).unwrap();
    assert_eq!(r.run(&db, "fetch 2 from k", &[]).unwrap()[0].rows.len(), 2);
    r.run(&db, "close k", &[]).unwrap();
    // a cursor is no way round a write: its query is a SELECT
    let e = r.run(&db, "declare w cursor with hold for delete from t", &[]).err().unwrap();
    assert_eq!(e.code, "42601", "{e}");
}
