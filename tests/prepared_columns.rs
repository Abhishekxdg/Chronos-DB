//! The columns a prepared statement describes (what a client's Describe of a portal asks before each Execute) are
//! kept with the statement; they must come out as a fresh analysis would after anything that changes them: DDL on
//! the same session or on another, a world's own schema, a temporary table that shadows a name, a changed
//! search_path, a view made again.

use chronos::sql::{Session, Statement, Type, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, sql: &str) {
    s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A prepared statement, as the protocol's Parse makes it: with its parameters' types.
fn prepare(db: &Db, s: &Session, sql: &str) -> (Statement, Vec<Type>) {
    let st = s.parse(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    let types = s.params(db, &st, &[]).unwrap();
    (st, types)
}

/// The names and types of the columns the statement gives now, as `name:type`.
fn described(db: &Db, s: &Session, st: &(Statement, Vec<Type>)) -> Vec<String> {
    let cols = s.columns(db, &st.0, &st.1).unwrap().unwrap_or_default();
    cols.into_iter().map(|(n, t)| format!("{n}:{}", t.name())).collect()
}

/// Describes it twice (the second is the kept one) and says what both gave; a copy of the statement, which keeps
/// nothing, describes it afresh and must say the same.
fn twice(db: &Db, s: &Session, st: &(Statement, Vec<Type>)) -> Vec<String> {
    let first = described(db, s, st);
    assert_eq!(first, described(db, s, st), "a second Describe differs from the first");
    let fresh = (st.0.clone(), st.1.clone());
    assert_eq!(first, described(db, s, &fresh), "the kept columns differ from a fresh analysis");
    first
}

#[test]
fn ddl_on_the_same_session_and_on_another() {
    let db = Db::new();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    run(&db, &mut a, "create table t (id int primary key, a int)");
    let st = prepare(&db, &a, "select * from t where id = $1");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    // the same session changes it
    run(&db, &mut a, "alter table t add column b text");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "b:text"]);
    // another session does
    run(&db, &mut b, "alter table t drop column a");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "b:text"]);
    run(&db, &mut b, "alter table t alter column b type int using 0");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "b:integer"]);
    run(&db, &mut b, "alter table t rename column b to c");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "c:integer"]);
    // dropped and made again with other columns
    run(&db, &mut b, "drop table t");
    assert!(s_err(&db, &a, &st), "the table is gone");
    run(&db, &mut b, "create table t (id bigint primary key, z text, y boolean)");
    assert_eq!(twice(&db, &a, &st), ["id:bigint", "z:text", "y:boolean"]);
    // writes to rows (not the schema) change nothing
    run(&db, &mut b, "insert into t values (1, 'x', true)");
    assert_eq!(twice(&db, &a, &st), ["id:bigint", "z:text", "y:boolean"]);
}

/// Whether describing it fails (no kept answer may stand in for the error).
fn s_err(db: &Db, s: &Session, st: &(Statement, Vec<Type>)) -> bool {
    s.columns(db, &st.0, &st.1).is_err()
}

#[test]
fn a_statement_with_expressions_and_joins() {
    let db = Db::new();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    run(&db, &mut a, "create table p (id int primary key, n text)");
    run(&db, &mut a, "create table c (id int primary key, p_id int, qty int)");
    let st = prepare(
        &db,
        &a,
        "select p.n, c.qty * 2 as dbl, count(*) over () as k from c join p on p.id = c.p_id where c.id = $1",
    );
    assert_eq!(twice(&db, &a, &st), ["n:text", "dbl:integer", "k:bigint"]);
    run(&db, &mut b, "alter table c alter column qty type numeric");
    assert_eq!(twice(&db, &a, &st), ["n:text", "dbl:numeric", "k:bigint"]);
    run(&db, &mut b, "alter table p alter column n type varchar(20)");
    let after = twice(&db, &a, &st);
    assert_eq!(after[1], "dbl:numeric");
    run(&db, &mut b, "alter table c drop column qty");
    assert!(s_err(&db, &a, &st), "a column the statement reads is gone");
    run(&db, &mut b, "alter table c add column qty bigint");
    assert_eq!(twice(&db, &a, &st)[1], "dbl:bigint");
}

#[test]
fn worlds_have_their_own_schemas() {
    let db = Db::new();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    run(&db, &mut a, "create table t (id int primary key, a int)");
    run(&db, &mut a, "create world w1");
    let st = prepare(&db, &a, "select * from t");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    // the session goes to the world: its schema is the fork's, until the world changes it
    run(&db, &mut a, "switch world w1");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    run(&db, &mut a, "alter table t add column w text");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "w:text"]);
    // main is another place: the same statement, described there
    run(&db, &mut b, "alter table t add column m boolean");
    assert_eq!(twice(&db, &b, &st), ["id:integer", "a:integer", "m:boolean"]);
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "w:text"]);
    run(&db, &mut a, "switch world main");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "m:boolean"]);
    run(&db, &mut a, "switch world w1");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "w:text"]);
    // a world made after the change forks from the changed main
    run(&db, &mut a, "switch world main");
    run(&db, &mut a, "create world w2");
    run(&db, &mut a, "switch world w2");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "m:boolean"]);
    // and a world's change comes to main when it is brought in
    run(&db, &mut a, "switch world main");
    run(&db, &mut a, "create world w3");
    run(&db, &mut a, "switch world w3");
    run(&db, &mut a, "alter table t add column w3 int");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "m:boolean", "w3:integer"]);
    run(&db, &mut a, "switch world main");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "m:boolean"]);
    run(&db, &mut a, "merge world w3 into main");
    assert_eq!(twice(&db, &b, &st), ["id:integer", "a:integer", "m:boolean", "w3:integer"]);
}

#[test]
fn temporary_tables_shadow_and_unshadow() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    run(&db, &mut a, "create table t (id int primary key, a int)");
    let st = prepare(&db, &a, "select * from t");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    run(&db, &mut a, "create temp table t (x text, y text, z text)");
    assert_eq!(twice(&db, &a, &st), ["x:text", "y:text", "z:text"]);
    run(&db, &mut a, "drop table t"); // the temporary one
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    // a temporary table of another name changes nothing, and one made later shadows again
    run(&db, &mut a, "create temp table other (q int)");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    run(&db, &mut a, "create temp table t (only_temp boolean)");
    assert_eq!(twice(&db, &a, &st), ["only_temp:boolean"]);
    run(&db, &mut a, "alter table t add column more int");
    assert_eq!(twice(&db, &a, &st), ["only_temp:boolean", "more:integer"]);
    // another session sees the permanent one
    let b = Session::new(MAIN);
    assert_eq!(twice(&db, &b, &st), ["id:integer", "a:integer"]);
}

#[test]
fn search_path_changes() {
    // (a name is found along the search_path when the statement is parsed, and stays the table it found)
    let db = Db::new();
    let mut a = Session::new(MAIN);
    run(&db, &mut a, "create schema s1");
    run(&db, &mut a, "create schema s2");
    run(&db, &mut a, "create table s1.t (id int, one text)");
    run(&db, &mut a, "create table s2.t (id int, two boolean, three int)");
    run(&db, &mut a, "set search_path = s1");
    let st1 = prepare(&db, &a, "select * from t");
    run(&db, &mut a, "set search_path = s2");
    let st2 = prepare(&db, &a, "select * from t");
    for _ in 0..2 {
        assert_eq!(twice(&db, &a, &st1), ["id:integer", "one:text"]);
        assert_eq!(twice(&db, &a, &st2), ["id:integer", "two:boolean", "three:integer"]);
        run(&db, &mut a, "set search_path = s1, s2");
        assert_eq!(twice(&db, &a, &st1), ["id:integer", "one:text"]);
        assert_eq!(twice(&db, &a, &st2), ["id:integer", "two:boolean", "three:integer"]);
        run(&db, &mut a, "set search_path = s2, s1");
    }
    // the table a statement found is dropped, and made again with other columns
    run(&db, &mut a, "drop table s1.t");
    assert!(s_err(&db, &a, &st1));
    assert_eq!(twice(&db, &a, &st2), ["id:integer", "two:boolean", "three:integer"]);
    run(&db, &mut a, "create table s1.t (id int, again date)");
    assert_eq!(twice(&db, &a, &st1), ["id:integer", "again:date"]);
    // a statement parsed after the search_path changed finds what it finds now
    run(&db, &mut a, "set search_path = s1");
    let st3 = prepare(&db, &a, "select * from t");
    assert_eq!(twice(&db, &a, &st3), ["id:integer", "again:date"]);
    run(&db, &mut a, "reset search_path");
    assert_eq!(twice(&db, &a, &st3), ["id:integer", "again:date"]);
}

#[test]
fn views_made_again_and_functions_and_enums() {
    let db = Db::new();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    run(&db, &mut a, "create table t (id int primary key, a int, b text)");
    run(&db, &mut a, "create view v as select id, a from t");
    let st = prepare(&db, &a, "select * from v");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    run(&db, &mut b, "create or replace view v as select id, a, b from t");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "b:text"]);
    run(&db, &mut b, "drop view v");
    run(&db, &mut b, "create view v as select b from t");
    assert_eq!(twice(&db, &a, &st), ["b:text"]);
    // a view over a view, whose base table changes
    run(&db, &mut a, "create view w as select * from v");
    let sw = prepare(&db, &a, "select * from w");
    assert_eq!(twice(&db, &a, &sw), ["b:text"]);
    run(&db, &mut b, "drop view w");
    run(&db, &mut b, "drop view v");
    run(&db, &mut b, "create view v as select id, a from t");
    run(&db, &mut b, "create view w as select * from v");
    assert_eq!(twice(&db, &a, &sw), ["id:integer", "a:integer"]);
    // a function whose result type changes when it is made again
    run(&db, &mut a, "create function f(x int) returns int language sql as 'select x + 1'");
    let sf = prepare(&db, &a, "select f(a) as r from t");
    assert_eq!(twice(&db, &a, &sf), ["r:integer"]);
    run(&db, &mut b, "drop function f(int)");
    run(&db, &mut b, "create function f(x int) returns text language sql as $$select 'a'::text$$");
    assert_eq!(twice(&db, &a, &sf), ["r:text"]);
    // an enum column keeps its type by name while labels are added
    run(&db, &mut a, "create type mood as enum ('sad', 'ok')");
    run(&db, &mut a, "create table m (id int, mood mood)");
    let sm = prepare(&db, &a, "select mood from m");
    let before = twice(&db, &a, &sm);
    assert_eq!(before, ["mood:mood"]);
    run(&db, &mut b, "alter type mood add value 'happy'");
    assert_eq!(twice(&db, &a, &sm), before);
    let out = a.run(&db, "insert into m values (1, 'happy')", &[]);
    assert!(out.is_ok());
}

#[test]
fn parameters_and_their_types() {
    let db = Db::new();
    let mut a = Session::new(MAIN);
    run(&db, &mut a, "create table t (id int primary key, a int)");
    let st = prepare(&db, &a, "select $1 as p, id from t where a = $2");
    // another statement object with other parameter types is its own: the kept columns are of these types
    let ints = vec![Type::Int, Type::Int];
    let texts = vec![Type::Text, Type::Int];
    let col = |types: &[Type]| a.columns(&db, &st.0, types).unwrap().unwrap()[0].1;
    assert_eq!(col(&ints), Type::Int);
    assert_eq!(col(&texts), Type::Text);
    assert_eq!(col(&ints), Type::Int);
    assert_eq!(col(&texts), Type::Text);
    // the rows still run
    let out = a.execute(&db, &st.0, &[Value::Int(7), Value::Int(1)], &ints).unwrap();
    assert!(out.rows.is_empty());
}

#[test]
fn a_describe_after_each_execute_of_a_changing_table() {
    // the pattern of libpq's PQexecPrepared: Describe the portal, then Execute, over and over, as the table changes
    let db = Db::new();
    let mut a = Session::new(MAIN);
    run(&db, &mut a, "create table t (id int primary key, a int)");
    run(&db, &mut a, "insert into t values (1, 10), (2, 20)");
    let st = prepare(&db, &a, "select * from t where id = $1");
    for i in 0..6 {
        let cols = described(&db, &a, &st);
        let out = a.execute(&db, &st.0, &[Value::Int(1)], &st.1).unwrap();
        assert_eq!(out.rows.len(), 1, "round {i}");
        assert_eq!(out.rows[0].len(), cols.len(), "round {i}: {cols:?}");
        run(&db, &mut a, &format!("alter table t add column c{i} int"));
    }
    assert_eq!(described(&db, &a, &st).len(), 8);
}

#[test]
fn transactions_with_ddl_and_another_sessions_end() {
    let db = Db::new();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    run(&db, &mut a, "create table t (id int primary key, a int)");
    run(&db, &mut a, "insert into t values (1, 10)");
    let st = prepare(&db, &a, "select * from t where id = $1");
    let width = |s: &mut Session| s.execute(&db, &st.0, &[Value::Int(1)], &st.1).unwrap().rows[0].len();
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    assert_eq!(width(&mut a), 2);
    // DDL inside a transaction is seen by the transaction (its statement runs and describes alike), not by others
    run(&db, &mut a, "begin");
    run(&db, &mut a, "alter table t add column x int");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "x:integer"]);
    assert_eq!(width(&mut a), 3);
    assert_eq!(twice(&db, &b, &st), ["id:integer", "a:integer"]);
    assert_eq!(width(&mut b), 2);
    run(&db, &mut a, "rollback");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    assert_eq!(width(&mut a), 2);
    // another session's transaction ends: seen from then on
    run(&db, &mut b, "begin");
    run(&db, &mut b, "alter table t add column y text");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
    assert_eq!(width(&mut a), 2);
    run(&db, &mut b, "commit");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "y:text"]);
    assert_eq!(width(&mut a), 3);
    // a savepoint rolled back takes its DDL with it
    run(&db, &mut a, "begin");
    run(&db, &mut a, "savepoint s");
    run(&db, &mut a, "alter table t drop column a");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "y:text"]);
    assert_eq!(width(&mut a), 2);
    run(&db, &mut a, "rollback to savepoint s");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "y:text"]);
    assert_eq!(width(&mut a), 3);
    run(&db, &mut a, "commit");
    assert_eq!(twice(&db, &b, &st), ["id:integer", "a:integer", "y:text"]);
}

#[test]
fn a_world_that_checks_its_reads_is_described_and_planned_through_them() {
    // (a world that records what it reads, for the stale-read check, goes the long way: the reads are recorded)
    let db = Db::new();
    let mut a = Session::new(MAIN);
    run(&db, &mut a, "create table t (id int primary key, a int)");
    run(&db, &mut a, "insert into t values (1, 10)");
    run(&db, &mut a, "create world w with (check_reads = true)");
    run(&db, &mut a, "switch world w");
    let st = prepare(&db, &a, "select * from t where id = $1");
    for _ in 0..3 {
        assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer"]);
        let out = a.execute(&db, &st.0, &[Value::Int(1)], &st.1).unwrap();
        assert_eq!(out.rows.len(), 1);
    }
    run(&db, &mut a, "alter table t add column b int");
    assert_eq!(twice(&db, &a, &st), ["id:integer", "a:integer", "b:integer"]);
    // main changed under the world since it forked: a merge of what it read is refused
    let mut m = Session::new(MAIN);
    run(&db, &mut m, "update t set a = 11 where id = 1");
    let merged = a.run(&db, "merge world w into main", &[]);
    assert!(merged.is_err(), "the world read a row that changed on main since it forked");
}
