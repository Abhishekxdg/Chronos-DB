//! CREATE TABLE AS as Postgres runs it: its query reads what it named before the table is made, so a temporary table
//! made from the world's table of its name (`CREATE TEMP TABLE point_tbl AS SELECT * FROM public.point_tbl`, as the
//! regression suite's create_index and arrays do) gets that table's rows, not the new table's none; and a function
//! makes a table so too (`EXECUTE 'CREATE TABLE ' || name || ' AS SELECT ...'`), in the transaction it runs in.

use chronos::sql::{Session, Value, take_notices};
use chronos::{AgentSpec, Can, Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

fn tag(db: &Db, s: &mut Session, q: &str) -> String {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().tag.clone()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code, e.message),
    }
}

fn notices() -> Vec<String> {
    take_notices().into_iter().map(|n| format!("{}: {}", n.level, n.message)).collect()
}

#[test]
fn a_temporary_table_made_from_the_world_table_of_its_name_gets_its_rows() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table point_tbl (f1 point); insert into point_tbl values ('(0,0)'), ('(1,1)'), ('(5,5)')");
    assert_eq!(tag(&db, &mut s, "create temp table point_tbl as select * from public.point_tbl"), "SELECT 3");
    run(&db, &mut s, "insert into point_tbl(f1) values (null)");
    assert_eq!(one(&db, &mut s, "select count(*) from point_tbl"), "4");
    assert_eq!(one(&db, &mut s, "select count(*) from point_tbl where f1 is null"), "1");
    assert_eq!(run(&db, &mut s, "select f1 from point_tbl order by f1 <-> '0,1'").len(), 4);
    // (by its bare name too, read before the temporary one is there)
    run(&db, &mut s, "create table nums (n int); insert into nums select generate_series(1, 5)");
    assert_eq!(tag(&db, &mut s, "create temp table nums as select n * 10 as n from nums"), "SELECT 5");
    assert_eq!(one(&db, &mut s, "select sum(n) from nums"), "150");
    // IF NOT EXISTS: left as it is, its query unrun
    take_notices();
    assert_eq!(
        tag(&db, &mut s, "create temp table if not exists nums as select n / 0 as n from nums"),
        "CREATE TABLE AS"
    );
    assert_eq!(notices(), ["NOTICE: relation \"nums\" already exists, skipping"]);
    assert_eq!(one(&db, &mut s, "select count(*) from nums"), "5");
    // a table already there, without it: Postgres's error (CreateTableAsRelExists's), before its query runs
    assert_eq!(
        fails(&db, &mut s, "create temp table nums as select n / 0 as n from nums"),
        ("42P07", "relation \"nums\" already exists".into())
    );
    // (a permanent one's, with no data too, as select_into and create_table have it)
    run(&db, &mut s, "create table ctas_ine_tbl as select 1");
    for q in ["create table ctas_ine_tbl as select 1 / 0", "create table ctas_ine_tbl as select 1 / 0 with no data"] {
        assert_eq!(fails(&db, &mut s, q), ("42P07", "relation \"ctas_ine_tbl\" already exists".into()), "{q}");
    }
    // no rows, and WITH NO DATA
    assert_eq!(tag(&db, &mut s, "create temp table none_ as select * from public.point_tbl where false"), "SELECT 0");
    assert_eq!(
        tag(&db, &mut s, "create temp table empty as select * from public.point_tbl with no data"),
        "CREATE TABLE AS"
    );
    assert_eq!(one(&db, &mut s, "select count(*) from empty"), "0");
    // a query that fails leaves no table behind
    let (code, _) = fails(&db, &mut s, "create temp table bad as select 1 / (n - 10) from nums");
    assert_eq!(code, "22012");
    assert_eq!(fails(&db, &mut s, "select * from bad").0, "42P01");
    // and in a transaction, it goes with it
    run(&db, &mut s, "begin");
    assert_eq!(tag(&db, &mut s, "create temp table point_tbl2 as select * from point_tbl"), "SELECT 4");
    assert_eq!(one(&db, &mut s, "select count(*) from point_tbl2"), "4");
    run(&db, &mut s, "rollback");
    assert_eq!(fails(&db, &mut s, "select * from point_tbl2").0, "42P01");
}

#[test]
fn a_permanent_table_made_as_a_query_is_as_before() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table src (a int, b text); insert into src values (1, 'x'), (2, 'y'), (3, 'z')");
    assert_eq!(tag(&db, &mut s, "create table dst as select a, b || '!' as b from src where a > 1"), "SELECT 2");
    assert_eq!(run(&db, &mut s, "select * from dst order by a"), [["2", "y!"], ["3", "z!"]]);
    assert_eq!(tag(&db, &mut s, "select a into dst2 from src"), "SELECT 3");
    // what it names that isn't there yet is no table of its own: 42P01, and nothing made
    assert_eq!(fails(&db, &mut s, "create table selfish as select * from selfish").0, "42P01");
    assert_eq!(one(&db, &mut s, "select count(*) from pg_class where relname = 'selfish'"), "0");
}

#[test]
fn a_function_makes_a_table_as_a_query() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table src (a int); insert into src values (1), (2), (3)");
    run(
        &db,
        &mut s,
        "create function mk(relname text) returns void language plpgsql as $$
         begin
           execute format('create table %I as select a, a * 2 as b from src where a > 1', relname);
         end $$",
    );
    run(&db, &mut s, "select mk('made')");
    assert_eq!(run(&db, &mut s, "select a, b from made order by a"), [["2", "4"], ["3", "6"]]);
    assert_eq!(fails(&db, &mut s, "select mk('made')"), ("42P07", "relation \"made\" already exists".into()));
    run(&db, &mut s, "drop table made");
    // (of catalog tables, as the regression suite's create_relfilenode_part reads pg_class)
    run(
        &db,
        &mut s,
        "create function snap(relname text) returns void language plpgsql as $$
         begin
           execute format('create table %I as select relname, relkind from pg_class where relname = %L', relname, 'src');
         end $$",
    );
    run(&db, &mut s, "select snap('status')");
    assert_eq!(run(&db, &mut s, "select relname, relkind from status"), [["src", "r"]]);
    run(&db, &mut s, "drop table status");
    // a statement of the function's own, IF NOT EXISTS and WITH NO DATA
    run(
        &db,
        &mut s,
        "create function more() returns int language plpgsql as $$
         declare n int;
         begin
           create table if not exists kept as select a from src;
           create table if not exists kept as select a / 0 as a from src;
           create table bare as select a from src with no data;
           execute 'create table counted as select a from src';
           get diagnostics n = row_count;
           return n;
         end $$",
    );
    take_notices();
    assert_eq!(one(&db, &mut s, "select more()"), "3");
    assert_eq!(notices(), ["NOTICE: relation \"kept\" already exists, skipping"]);
    assert_eq!(one(&db, &mut s, "select count(*) from kept"), "3");
    assert_eq!(one(&db, &mut s, "select count(*) from bare"), "0");
    assert_eq!(one(&db, &mut s, "select count(*) from counted"), "3");
    // in the caller's transaction: rolled back with it
    run(&db, &mut s, "begin");
    run(&db, &mut s, "select mk('gone')");
    assert_eq!(one(&db, &mut s, "select count(*) from gone"), "2");
    run(&db, &mut s, "rollback");
    assert_eq!(fails(&db, &mut s, "select * from gone").0, "42P01");
    // a query that fails leaves no table behind
    run(
        &db,
        &mut s,
        "create function bad() returns void language plpgsql as $$
         begin execute 'create table broken as select 1 / (a - 1) as x from src'; end $$",
    );
    assert_eq!(fails(&db, &mut s, "select bad()").0, "22012");
    assert_eq!(fails(&db, &mut s, "select * from broken").0, "42P01");
}

#[test]
fn a_function_s_execute_of_select_into_is_refused_as_in_postgres() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table src (a int); insert into src values (1), (2), (3)");
    // (pl_exec.c's SPI_OK_SELINTO: EXECUTE ... INTO or EXECUTE CREATE TABLE ... AS make the table instead; the error
    // leaves none)
    for body in [
        "execute 'select a into t from src'",
        "execute '  SELECT a INTO t FROM src'",
        "execute '/* a table */ select a into temp t from src'",
        "execute 'select a into t from src' into n",
    ] {
        run(
            &db,
            &mut s,
            &format!(
                "create or replace function si() returns void language plpgsql as $$
                 declare n int;
                 begin {body}; end $$"
            ),
        );
        let e = s.run(&db, "select si()", &[]).err().unwrap_or_else(|| panic!("{body}: should fail"));
        assert_eq!((e.code, e.message.as_str()), ("0A000", "EXECUTE of SELECT ... INTO is not implemented"), "{body}");
        assert_eq!(
            e.info.and_then(|i| i.hint).as_deref(),
            Some("You might want to use EXECUTE ... INTO or EXECUTE CREATE TABLE ... AS instead."),
            "{body}"
        );
        assert_eq!(fails(&db, &mut s, "select * from t").0, "42P01", "{body}");
    }
    // RETURN QUERY EXECUTE's own error comes first, as SPI's does
    run(
        &db,
        &mut s,
        "create function rq() returns setof int language plpgsql as $$
         begin return query execute 'select a into t from src'; end $$",
    );
    assert_eq!(fails(&db, &mut s, "select * from rq()"), ("42601", "SELECT INTO query does not return tuples".into()));
    // CREATE TABLE AS, EXECUTE's or the function's own (as EXECUTE 'CREATE ...'), still makes it
    run(
        &db,
        &mut s,
        "create function ok() returns void language plpgsql as $$
         begin
           execute '/* made */ CREATE TABLE t AS SELECT a FROM src';
           create table t2 as select a from src where a > 1;
         end $$",
    );
    run(&db, &mut s, "select ok()");
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "3");
    assert_eq!(one(&db, &mut s, "select count(*) from t2"), "2");
}

#[test]
fn a_temporary_table_made_as_a_query_in_a_function_is_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create function cachebug() returns void language plpgsql as $$
         begin
           create temp table temptable as select * from generate_series(1, 3) as f1;
         end $$",
    );
    let (code, message) = fails(&db, &mut s, "select cachebug()");
    assert_eq!(code, "0A000");
    assert!(message.contains("temporary"), "{message}");
}

#[test]
fn a_table_a_function_makes_of_masked_columns_stays_masked() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create table customers (id int primary key, email text);
         insert into customers values (1, 'ann@example.com');
         alter table customers alter column email set masked",
    );
    run(
        &db,
        &mut s,
        "create function copy_them() returns void language plpgsql as $$
         begin execute 'create table copied as select id, email from customers'; end $$",
    );
    run(&db, &mut s, "select copy_them()");
    // (a person reads the values; an agent without unmask, their masks)
    assert_eq!(one(&db, &mut s, "select email from copied"), "ann@example.com");
    db.create_agent("bot", &AgentSpec { can: Some(vec![Can::Read]), ..Default::default() }).unwrap();
    let mut b = Session::new(MAIN);
    b.agent = Some("bot".into());
    let email = one(&db, &mut b, "select email from copied");
    assert!(email.starts_with("m_"), "{email}");
}
