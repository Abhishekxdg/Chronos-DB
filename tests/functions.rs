//! CREATE FUNCTION and CREATE TRIGGER as Chronos has them beyond Postgres: in worlds (fork, merge,
//! diff, time travel), across restarts (the log holds what triggers wrote, so they never fire
//! again), refusals of what isn't supported, and statement timeouts in endless loops.
//! Results that Postgres also gives are checked against it in tests/pgdiff.rs.

use chronos::sql::{self, Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> (&'static str, String) {
    match s.run(db, q, &[]) {
        Ok(_) => panic!("{q}: should fail"),
        Err(e) => (e.code, e.message),
    }
}

fn one(db: &Db, s: &mut Session, q: &str) -> String {
    run(db, s, q)[0][0].clone()
}

const AUDITED: &str = "
    create table item (id int primary key, name text, qty int);
    create table audit (id serial primary key, op text, item int);
    create function log_it() returns trigger language plpgsql as $$
    begin
      insert into audit (op, item) values (tg_op, coalesce(new.id, old.id));
      return null;
    end $$;
    create trigger item_audit after insert or update or delete on item for each row execute function log_it();
";

#[test]
fn refuses_what_it_does_not_run() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    for (body, construct) in [
        ("begin commit; return 1; end", "COMMIT"),
        ("declare r record; begin r := null; return 1; end", "whole record"),
    ] {
        let ret = if body.contains("next") { "setof int" } else { "int" };
        let q = format!("create function f() returns {ret} language plpgsql as $$ {body} $$");
        let (code, msg) = fails(&db, &mut s, &q);
        assert_eq!(code, "0A000", "{body}: {msg}");
        assert!(msg.contains(construct), "{body}: {msg}");
    }
    for (q, construct) in [
        ("create function f() returns int language python as 'x'", "python"),
        ("create function f() returns int language sql as 'create table q (a int)'", "CREATE TABLE"),
    ] {
        let (code, msg) = fails(&db, &mut s, q);
        assert_eq!(code, "0A000", "{q}: {msg}");
        assert!(msg.contains(construct), "{q}: {msg}");
    }
    // mistakes, as Postgres reports them when the function is made
    for (body, code) in [
        ("begin y := 1; return 1; end", "42601"),
        ("begin exit; end", "42601"),
        ("begin raise notice 'a % %', 1; return 1; end", "42601"),
        ("begin return 1; exception when no_such_thing then return 2; end", "42704"),
        ("declare m text; begin get diagnostics m = message_text; return 1; end", "42601"),
        // (a FOREACH over an array with a variable that isn't declared; an EXIT of a label no block or loop has)
        ("begin foreach x slice 1 in array '{1}' loop end loop; return 1; end", "42601"),
        ("begin loop exit outer; end loop; return 1; end", "42601"),
    ] {
        let q = format!("create function f() returns int language plpgsql as $$ {body} $$");
        assert_eq!(fails(&db, &mut s, &q).0, code, "{body}");
    }
    // a labelled outer block is Postgres's own: the function is made and runs
    run(&db, &mut s, "create function lab() returns int language plpgsql as $$ <<outer>> begin return 1; end $$");
    assert_eq!(one(&db, &mut s, "select lab()"), "1");
}

#[test]
fn raise_aborts_the_statement_and_the_transaction() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(&db, AUDITED, &[]).unwrap();
    s.run(
        &db,
        "create function no_big() returns trigger language plpgsql as $$
         begin raise exception 'qty % is too big', new.qty using errcode = 'check_violation'; end $$;
         create trigger item_guard before insert on item for each row when (new.qty > 100) execute function no_big();",
        &[],
    )
    .unwrap();
    // a failing row takes the statement's other rows, and what triggers wrote, with it
    let (code, msg) = fails(&db, &mut s, "insert into item values (1, 'a', 5), (2, 'b', 500)");
    assert_eq!((code, msg.as_str()), ("23514", "qty 500 is too big"));
    assert_eq!(one(&db, &mut s, "select count(*) from item"), "0");
    assert_eq!(one(&db, &mut s, "select count(*) from audit"), "0");
    // in a transaction: the transaction fails, and ROLLBACK undoes the rows before it
    run(&db, &mut s, "begin");
    run(&db, &mut s, "insert into item values (3, 'c', 5)");
    fails(&db, &mut s, "insert into item values (4, 'd', 999)");
    assert_eq!(fails(&db, &mut s, "select 1").0, "25P02");
    run(&db, &mut s, "commit");
    assert_eq!(one(&db, &mut s, "select count(*) from item"), "0");
    assert_eq!(one(&db, &mut s, "select count(*) from audit"), "0");
    run(&db, &mut s, "insert into item values (5, 'e', 50)");
    assert_eq!(run(&db, &mut s, "select op, item from audit"), [["INSERT", "5"]]);
}

#[test]
fn a_statement_and_its_rule_commands_are_one_write() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(
        &db,
        "create table ro1 (a int);
         create table ro2 (a int, b int, c text);
         create sequence ro_seq;
         create rule ro_r3 as on insert to ro1 do instead insert into ro2 values (new.a, nextval('ro_seq'), 'r3');
         create rule ro_r4 as on insert to ro1 where a < 100 do instead insert into ro2 values (new.a, nextval('ro_seq'), 'r4');
         create rule ro_r2 as on insert to ro1 do insert into ro2 values (new.a, nextval('ro_seq'), 'r2');
         create rule ro_r1 as on insert to ro1 do instead insert into ro2 values (new.a, nextval('ro_seq'), 'r1');",
        &[],
    )
    .unwrap();
    // the session runs the INSERT again once the sequence's reserved values run out, at the third rule's
    // nextval: the commands of the first run are gone with it, not run twice
    run(&db, &mut s, "insert into ro1 values (1)");
    assert_eq!(
        run(&db, &mut s, "select b, c from ro2 order by b"),
        [["1", "r1"], ["2", "r2"], ["3", "r3"], ["4", "r4"]]
    );
    // a rule command that fails takes the statement and the commands before it with it
    s.run(
        &db,
        "create table rk (k int primary key);
         create table rl (a int);
         create rule rk_a as on insert to rl do also insert into rk values (new.a);
         create rule rk_b as on insert to rl do also insert into rk values (new.a * 2);",
        &[],
    )
    .unwrap();
    run(&db, &mut s, "insert into rk values (6)");
    assert_eq!(fails(&db, &mut s, "insert into rl values (2), (3)").0, "23505");
    assert_eq!(one(&db, &mut s, "select count(*) from rl"), "0");
    assert_eq!(run(&db, &mut s, "select k from rk"), [["6"]]);
    // in a transaction: ROLLBACK TO a savepoint before it keeps what came before, and none of it
    run(&db, &mut s, "begin");
    run(&db, &mut s, "insert into rl values (10)");
    run(&db, &mut s, "savepoint p");
    fails(&db, &mut s, "insert into rl values (4), (3)");
    run(&db, &mut s, "rollback to p");
    run(&db, &mut s, "commit");
    assert_eq!(run(&db, &mut s, "select a from rl"), [["10"]]);
    assert_eq!(run(&db, &mut s, "select k from rk order by k"), [["6"], ["10"], ["20"]]);
    // (the forks the statements ran on are gone)
    assert!(db.branches().iter().all(|(b, _)| !b.starts_with("_tx_")), "{:?}", db.branches());
}

#[test]
fn functions_and_triggers_live_in_worlds() {
    let dir = std::env::temp_dir().join(format!("chronos-functions-worlds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (db, mut s) = (Db::open(&dir).unwrap(), Session::new(MAIN)); // on disk: time travel
    s.run(&db, AUDITED, &[]).unwrap();
    run(&db, &mut s, "insert into item values (1, 'a', 1)");
    // a fork has the trigger; what it fires there stays there until the merge
    run(&db, &mut s, "create world w");
    let mut w = Session::new("w");
    run(&db, &mut w, "insert into item values (2, 'b', 2)");
    run(&db, &mut w, "create function twice(x int) returns int language sql as 'select 2 * x'");
    assert_eq!(one(&db, &mut w, "select twice(21)"), "42");
    assert_eq!(fails(&db, &mut s, "select twice(21)").0, "42883", "not on main yet");
    assert_eq!(one(&db, &mut w, "select count(*) from audit"), "2");
    assert_eq!(one(&db, &mut s, "select count(*) from audit"), "1");
    let diff = run(&db, &mut w, "diff world w");
    assert!(diff.iter().any(|r| r[0] == "twice(integer)" && r[2] == "create function"), "{diff:?}");
    let as_sql = run(&db, &mut w, "diff world w as sql");
    assert!(
        as_sql.iter().any(|r| r[0].starts_with("create or replace function twice(x integer) returns integer")),
        "{as_sql:?}"
    );
    // merging applies the rows the trigger wrote: it doesn't fire again
    run(&db, &mut s, "merge world w");
    assert_eq!(one(&db, &mut s, "select twice(5)"), "10");
    assert_eq!(run(&db, &mut s, "select op, item from audit order by item"), [["INSERT", "1"], ["INSERT", "2"]]);
    run(&db, &mut s, "update item set qty = 9 where id = 2");
    assert_eq!(one(&db, &mut s, "select count(*) from audit"), "3");
    // a trigger dropped on a branch goes with the merge
    run(&db, &mut s, "create world w2");
    let mut w2 = Session::new("w2");
    run(&db, &mut w2, "drop trigger item_audit on item");
    run(&db, &mut w2, "insert into item values (3, 'c', 3)");
    assert_eq!(one(&db, &mut w2, "select count(*) from audit"), "3");
    run(&db, &mut s, "insert into item values (4, 'd', 4)");
    assert_eq!(one(&db, &mut s, "select count(*) from audit"), "4", "main still has it");
    let d2 = run(&db, &mut w2, "diff world w2");
    assert!(
        d2.iter().any(|r| r[0] == "item" && r[2] == "alter table" && r[5].contains("-trigger item_audit")),
        "{d2:?}"
    );
    let d2 = run(&db, &mut w2, "diff world w2 as sql");
    assert!(d2.iter().any(|r| r[0] == "drop trigger item_audit on item;"), "{d2:?}");
    run(&db, &mut s, "merge world w2");
    run(&db, &mut s, "insert into item values (5, 'e', 5)");
    assert_eq!(one(&db, &mut s, "select count(*) from audit"), "4", "gone after the merge");
    // time travel: the function as it was
    std::thread::sleep(std::time::Duration::from_millis(20));
    let then = one(&db, &mut s, "select now()");
    std::thread::sleep(std::time::Duration::from_millis(20));
    run(&db, &mut s, "create or replace function twice(x int) returns int language sql as 'select 3 * x'");
    assert_eq!(one(&db, &mut s, "select twice(5)"), "15");
    let mut past = Session::new(&format!("main@{then}"));
    assert_eq!(one(&db, &mut past, "select twice(5)"), "10");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_restart_replays_what_triggers_wrote_without_firing_them() {
    let dir = std::env::temp_dir().join(format!("chronos-functions-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(MAIN);
        s.run(&db, AUDITED, &[]).unwrap();
        run(&db, &mut s, "insert into item values (1, 'a', 1), (2, 'b', 2)");
        run(&db, &mut s, "update item set qty = qty + 1");
        run(&db, &mut s, "delete from item where id = 1");
        assert_eq!(one(&db, &mut s, "select count(*) from audit"), "5");
    }
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(one(&db, &mut s, "select count(*) from audit"), "5", "replay wrote the rows, not the triggers");
    assert_eq!(one(&db, &mut s, "select count(*) from item"), "1");
    // the trigger and its function came back with the data, and serial numbers go on
    run(&db, &mut s, "insert into item values (3, 'c', 3)");
    assert_eq!(one(&db, &mut s, "select count(*) from audit"), "6");
    assert_eq!(run(&db, &mut s, "select op, item from audit order by id desc limit 1"), [["INSERT", "3"]]);
    assert_eq!(one(&db, &mut s, "select count(distinct id) from audit"), "6");
    // statements numbering rows through a trigger leave no gaps between them
    run(&db, &mut s, "insert into item values (4, 'd', 4)");
    run(&db, &mut s, "insert into item values (5, 'e', 5)");
    let ids = run(&db, &mut s, "select id from audit order by id desc limit 2");
    assert_eq!(ids[0][0].parse::<i64>().unwrap(), ids[1][0].parse::<i64>().unwrap() + 1, "{ids:?}");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn statement_timeout_stops_an_endless_loop() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create function spin() returns int language plpgsql as $$ declare i int := 0; begin loop i := i + 1; end loop; end $$",
    );
    run(&db, &mut s, "set statement_timeout = 200");
    let t = std::time::Instant::now();
    let (code, _) = fails(&db, &mut s, "select spin()");
    assert_eq!(code, "57014");
    assert!(t.elapsed() < std::time::Duration::from_secs(5));
    run(&db, &mut s, "reset statement_timeout");
    assert_eq!(one(&db, &mut s, "select 1"), "1");
}

#[test]
fn endless_recursion_is_stopped() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create function down(n int) returns int language plpgsql as $$ begin return 1 + down(n - 1); end $$",
    );
    run(&db, &mut s, "create table chain (id int primary key)");
    run(
        &db,
        &mut s,
        "create function more() returns trigger language plpgsql as $$ begin insert into chain values (new.id + 1); return new; end $$",
    );
    run(&db, &mut s, "create trigger again after insert on chain for each row execute function more()");
    // on a server thread's stack, as the Postgres port runs them
    let db = std::sync::Arc::new(db);
    let d = db.clone();
    std::thread::spawn(move || {
        let mut s = Session::new(MAIN);
        assert_eq!(fails(&d, &mut s, "select down(5)").0, "54001");
        assert_eq!(fails(&d, &mut s, "insert into chain values (1)").0, "54001");
        assert_eq!(one(&d, &mut s, "select count(*) from chain"), "0");
    })
    .join()
    .unwrap();
}

#[test]
fn json_writes_to_a_table_with_triggers_are_refused() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(&db, AUDITED, &[]).unwrap();
    let e = db.put(MAIN, "item/1", r#"{"name": "a", "qty": 1}"#).unwrap_err().to_string();
    assert!(e.contains("has triggers"), "{e}");
    run(&db, &mut s, "alter table item disable trigger item_audit");
    db.put(MAIN, "item/1", r#"{"name": "a", "qty": 1}"#).unwrap();
    assert_eq!(one(&db, &mut s, "select count(*) from item"), "1");
}

#[test]
fn notices_and_writes_from_select() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table hits (id serial primary key, who text)");
    run(
        &db,
        &mut s,
        "create function hit(who text) returns int language plpgsql as $$
         declare n int;
         begin
           insert into hits (who) values (who);
           select count(*) into n from hits;
           raise notice 'hit % (% so far)', who, n;
           return n;
         end $$",
    );
    let _ = sql::take_notices();
    assert_eq!(run(&db, &mut s, "select hit('a'), hit('b')"), [["1", "2"]]);
    let n: Vec<String> = sql::take_notices().into_iter().map(|n| format!("{} {}", n.level, n.message)).collect();
    assert_eq!(n, ["NOTICE hit a (1 so far)", "NOTICE hit b (2 so far)"]);
    // a function that isn't VOLATILE may not write
    run(
        &db,
        &mut s,
        "create function sneaky() returns int language sql stable as $$ insert into hits (who) values ('x') returning id $$",
    );
    assert_eq!(fails(&db, &mut s, "select sneaky()").0, "0A000");
    // the statement fails as a whole: what its functions wrote goes too
    assert_eq!(fails(&db, &mut s, "select hit('c'), 1 / 0").0, "22012");
    assert_eq!(one(&db, &mut s, "select count(*) from hits"), "2");
}

/// A function that isn't VOLATILE may read and no more: Postgres names the command (0A000), in SQL and PL/pgSQL
/// functions, for what they run and what they run dynamically; a VOLATILE function it calls writes as itself.
#[test]
fn a_function_that_is_not_volatile_may_not_change_anything() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int)");
    run(&db, &mut s, "create temp table tt (a int)");
    run(
        &db,
        &mut s,
        "create function vol(i int) returns int language plpgsql as $$ begin insert into t values (i); return i; end $$",
    );
    for (name, volatility, body, tag) in [
        ("s1", "stable", "insert into t values (1) returning a", "INSERT"),
        ("s2", "immutable", "update t set a = 1 returning a", "UPDATE"),
        ("s3", "stable", "delete from t returning a", "DELETE"),
        ("s4", "stable", "insert into tt values (1) returning a", "INSERT"),
        ("s5", "stable", "update tt set a = 2 returning a", "UPDATE"),
    ] {
        run(&db, &mut s, &format!("create function {name}() returns int language sql {volatility} as $$ {body} $$"));
        let (code, msg) = fails(&db, &mut s, &format!("select {name}()"));
        assert_eq!(code, "0A000", "{name}");
        assert_eq!(msg, format!("{tag} is not allowed in a non-volatile function"), "{name}");
    }
    for (name, body, tag) in [
        ("p1", "insert into t values (1);", "INSERT"),
        ("p2", "execute 'delete from t';", "DELETE"),
        ("p3", "insert into tt values (1);", "INSERT"),
        ("p4", "execute 'update tt set a = 1';", "UPDATE"),
        ("p5", "truncate t;", "TRUNCATE TABLE"),
        ("p6", "create table made (a int);", "CREATE TABLE"),
    ] {
        run(
            &db,
            &mut s,
            &format!(
                "create function {name}() returns int language plpgsql stable as $$ begin {body} return 1; end $$"
            ),
        );
        let (code, msg) = fails(&db, &mut s, &format!("select {name}()"));
        assert_eq!(code, "0A000", "{name}");
        assert_eq!(msg, format!("{tag} is not allowed in a non-volatile function"), "{name}");
    }
    assert_eq!(one(&db, &mut s, "select count(*) from t"), "0");
    assert_eq!(one(&db, &mut s, "select count(*) from tt"), "0");
    // a volatile function it calls is not held to it
    run(&db, &mut s, "create function ok() returns int language plpgsql stable as $$ begin return vol(7); end $$");
    assert_eq!(one(&db, &mut s, "select ok()"), "7");
    assert_eq!(one(&db, &mut s, "select a from t"), "7");
}

/// What a function writes to a temporary table is the session's, with its transaction: kept at COMMIT, gone at
/// ROLLBACK, as the statement that called the function was; the world holds none of it.
#[test]
fn functions_write_temporary_tables_with_their_transactions() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create temp table tt (a int)");
    run(
        &db,
        &mut s,
        "create function add(i int) returns int language plpgsql as $$ begin insert into tt values (i); return i; end $$",
    );
    assert_eq!(one(&db, &mut s, "select add(x) from generate_series(1, 3) x where x = 2"), "2");
    run(&db, &mut s, "begin");
    run(&db, &mut s, "select add(10)");
    run(&db, &mut s, "savepoint a");
    run(&db, &mut s, "select add(11)");
    run(&db, &mut s, "rollback to a");
    run(&db, &mut s, "select add(12)");
    run(&db, &mut s, "commit");
    assert_eq!(run(&db, &mut s, "select a from tt order by a"), [["2"], ["10"], ["12"]]);
    run(&db, &mut s, "begin");
    run(&db, &mut s, "select add(20)");
    run(&db, &mut s, "rollback");
    assert_eq!(one(&db, &mut s, "select count(*) from tt"), "3");
    // the world holds none of it: no table of that name, no rows of one
    let mut other = Session::new(MAIN);
    assert_eq!(fails(&db, &mut other, "select * from tt").0, "42P01");
    assert!(db.prefix(MAIN, "tt/").unwrap().is_empty());
    // a trigger on a temporary table would run its function where the world's tables aren't: refused, not made
    // in the world, as a table of no rows, where it would never fire
    run(&db, &mut s, "create function trf() returns trigger language plpgsql as $$ begin return new; end $$");
    let (code, _) = fails(&db, &mut s, "create trigger t1 before insert on tt for each row execute function trf()");
    assert_eq!(code, "0A000");
    assert_eq!(fails(&db, &mut other, "select * from tt").0, "42P01");
}

#[test]
fn drivers_learn_parameter_and_result_types() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create function add1(int) returns int language sql as 'select $1 + 1'");
    run(&db, &mut s, "create table t (id int primary key, n bigint)");
    let st = sql::parse("select add1($1) as x").unwrap().pop().unwrap();
    assert_eq!(st.params(&db, MAIN, &[]).unwrap(), [sql::Type::Int]);
    assert_eq!(st.columns(&db, MAIN, &[sql::Type::Int]).unwrap().unwrap(), [("x".to_string(), sql::Type::Int)]);
    let ins = sql::parse("insert into t values ($1, 5) returning add1(id)").unwrap().pop().unwrap();
    assert_eq!(ins.columns(&db, MAIN, &[sql::Type::Int]).unwrap().unwrap(), [("add1".to_string(), sql::Type::Int)]);
}

#[test]
fn sequences_inside_functions_and_triggers() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    s.run(
        &db,
        "create sequence ticket;
         create table item (id int primary key, name text);
         create table log (id bigint primary key default nextval('ticket'), item int, n bigint);
         create function note() returns trigger language plpgsql as $$
         begin
           insert into log (item, n) values (new.id, nextval('ticket') * 0 + 1);
           return new;
         end $$;
         create trigger item_log after insert on item for each row execute function note();
         create function take() returns bigint language sql as $$ select nextval('ticket') $$;",
        &[],
    )
    .unwrap();
    // a write whose trigger takes more values than were reserved for it runs again with more
    run(&db, &mut s, "insert into item select g, 'x' from generate_series(1, 50) g");
    assert_eq!(one(&db, &mut s, "select count(*) from log"), "50");
    assert_eq!(one(&db, &mut s, "select count(distinct id) from log"), "50");
    // a SELECT calling a function that takes one
    let a: i64 = one(&db, &mut s, "select take()").parse().unwrap();
    let b: i64 = one(&db, &mut s, "select take()").parse().unwrap();
    assert!(b > a, "{a} {b}");
    assert_eq!(one(&db, &mut s, "select count(*) from log where id = 0"), "0");
}

#[test]
fn exception_blocks_take_back_writes_in_triggers_transactions_and_restarts() {
    let dir = std::env::temp_dir().join(format!("chronos-exceptions-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = Session::new(MAIN);
        // a trigger whose handler catches a unique_violation: the row goes to `dupes` instead
        s.run(
            &db,
            "create table item (id int primary key, name text);
             create table seen (name text primary key, n serial);
             create table dupes (id int primary key, name text);
             create function note() returns trigger language plpgsql as $$
             begin
               begin
                 insert into seen (name) values (new.name);
               exception when unique_violation then
                 insert into dupes values (new.id, new.name);
               end;
               return new;
             end $$;
             create trigger item_note after insert on item for each row execute function note();",
            &[],
        )
        .unwrap();
        run(&db, &mut s, "insert into item values (1, 'a'), (2, 'b'), (3, 'a')");
        run(&db, &mut s, "insert into item values (4, 'b')");
        assert_eq!(run(&db, &mut s, "select name from seen order by name"), [["a"], ["b"]]);
        assert_eq!(run(&db, &mut s, "select id, name from dupes order by id"), [["3", "a"], ["4", "b"]]);
        // an error no handler catches still stops the whole statement
        run(
            &db,
            &mut s,
            "create function boom() returns trigger language plpgsql as $$ begin if new.id = 9 then raise exception 'no 9'; end if; return new; end $$",
        );
        run(&db, &mut s, "create trigger item_boom before insert on item for each row execute function boom()");
        assert_eq!(fails(&db, &mut s, "insert into item values (8, 'c'), (9, 'd')").0, "P0001");
        assert_eq!(one(&db, &mut s, "select count(*) from item"), "4");
        // in a transaction, between savepoints: the block's writes go, the rest stays
        run(
            &db,
            &mut s,
            "create function try_add(k int) returns text language plpgsql as $$
             begin
               insert into dupes values (k, 'tx');
               begin
                 insert into dupes values (k + 1, 'tx-inner');
                 insert into dupes values (k, 'again');
               exception when unique_violation then
                 return 'caught';
               end;
               return 'ok';
             end $$",
        );
        run(&db, &mut s, "begin");
        assert_eq!(one(&db, &mut s, "select try_add(100)"), "caught");
        run(&db, &mut s, "savepoint a");
        assert_eq!(one(&db, &mut s, "select try_add(200)"), "caught");
        run(&db, &mut s, "rollback to savepoint a");
        assert_eq!(one(&db, &mut s, "select try_add(300)"), "caught");
        run(&db, &mut s, "commit");
        assert_eq!(run(&db, &mut s, "select id from dupes where id >= 100 order by id"), [["100"], ["300"]]);
    }
    // a restart replays the rows that stayed, and nothing the handlers took back
    let db = Db::open(&dir).unwrap();
    let mut s = Session::new(MAIN);
    assert_eq!(run(&db, &mut s, "select name from seen order by name"), [["a"], ["b"]]);
    assert_eq!(run(&db, &mut s, "select id from dupes order by id"), [["3"], ["4"], ["100"], ["300"]]);
    assert_eq!(one(&db, &mut s, "select count(*) from item"), "4");
    // serial numbers taken inside a block that was rolled back are skipped, never handed out twice
    run(&db, &mut s, "insert into item values (10, 'z')");
    let ns = run(&db, &mut s, "select n from seen order by n");
    assert_eq!(ns.len(), 3);
    assert!(ns[2][0].parse::<i64>().unwrap() > ns[1][0].parse::<i64>().unwrap(), "{ns:?}");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_statement_timeout_is_not_caught_by_when_others() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create function spin_safely() returns int language plpgsql as $$
         declare i int := 0;
         begin
           begin
             loop i := i + 1; end loop;
           exception when others then return -1;
           end;
         end $$",
    );
    run(&db, &mut s, "set statement_timeout = 200");
    let t = std::time::Instant::now();
    assert_eq!(fails(&db, &mut s, "select spin_safely()").0, "57014");
    assert!(t.elapsed() < std::time::Duration::from_secs(5));
    // named, it is caught (as in Postgres), and the statement is still stopped at its next check
    run(
        &db,
        &mut s,
        "create function spin_named() returns int language plpgsql as $$
         declare i int := 0;
         begin
           begin
             loop i := i + 1; end loop;
           exception when query_canceled then
             loop i := i + 1; end loop;
           end;
         end $$",
    );
    assert_eq!(fails(&db, &mut s, "select spin_named()").0, "57014");
    assert!(t.elapsed() < std::time::Duration::from_secs(5));
    run(&db, &mut s, "reset statement_timeout");
    assert_eq!(one(&db, &mut s, "select 1"), "1");
}

#[test]
fn enum_changes_fork_merge_travel_and_replay() {
    let dir = std::env::temp_dir().join(format!("chronos-enum-worlds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let then;
    {
        let (db, mut s) = (Db::open(&dir).unwrap(), Session::new(MAIN));
        s.run(
            &db,
            "create type mood as enum ('sad', 'ok', 'happy');
             create table p (id int primary key, m mood);
             insert into p values (1, 'sad'), (2, 'happy');
             create function lift(x mood) returns mood language plpgsql as $$ begin return enum_last(x); end $$;",
            &[],
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        then = one(&db, &mut s, "select now()");
        std::thread::sleep(std::time::Duration::from_millis(20));
        // on a branch: labels added and renamed there only, until the merge
        run(&db, &mut s, "create world w");
        let mut w = Session::new("w");
        run(&db, &mut w, "alter type mood add value 'meh' before 'ok'");
        run(&db, &mut w, "alter type mood rename value 'sad' to 'down'");
        run(&db, &mut w, "insert into p values (3, 'meh')");
        assert_eq!(run(&db, &mut w, "select id from p order by m"), [["1"], ["3"], ["2"]]);
        assert_eq!(fails(&db, &mut s, "select 'meh'::mood").0, "22P02");
        assert_eq!(one(&db, &mut s, "select m from p where id = 1"), "sad");
        run(&db, &mut s, "merge world w");
        assert_eq!(one(&db, &mut s, "select enum_range(null::mood)"), "{down,meh,ok,happy}");
        assert_eq!(run(&db, &mut s, "select id, m from p order by m"), [["1", "down"], ["3", "meh"], ["2", "happy"]]);
        // time travel: the type and the rows as they were
        let mut past = Session::new(&format!("main@{then}"));
        assert_eq!(one(&db, &mut past, "select enum_range(null::mood)"), "{sad,ok,happy}");
        assert_eq!(one(&db, &mut past, "select m from p where id = 1"), "sad");
        run(&db, &mut s, "alter type mood rename to feeling");
        assert_eq!(one(&db, &mut s, "select lift('meh')"), "happy");
    }
    // a restart replays the rows ALTER TYPE wrote
    let (db, mut s) = (Db::open(&dir).unwrap(), Session::new(MAIN));
    assert_eq!(one(&db, &mut s, "select enum_range(null::feeling)"), "{down,meh,ok,happy}");
    assert_eq!(run(&db, &mut s, "select id, m from p order by m"), [["1", "down"], ["3", "meh"], ["2", "happy"]]);
    assert_eq!(one(&db, &mut s, "select lift('ok')"), "happy");
    run(&db, &mut s, "drop type feeling cascade");
    assert_eq!(run(&db, &mut s, "select * from p order by id"), [["1"], ["2"], ["3"]]);
    assert_eq!(fails(&db, &mut s, "select lift('ok')").0, "42883");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn out_parameters_polymorphism_and_user_definitions() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create function f(a int, out s int, out p int) language plpgsql as $$ begin s := a + 1; p := a * 2; end $$",
    );
    assert_eq!(run(&db, &mut s, "select s, p from f(3)"), vec![vec!["4", "6"]]);
    run(
        &db,
        &mut s,
        "create function g(a anyelement, b anyelement) returns anyelement language sql as 'select case when a > b then a else b end'",
    );
    assert_eq!(one(&db, &mut s, "select g(3, 7)"), "7");
    assert_eq!(one(&db, &mut s, "select g('a'::text, 'b'::text)"), "b");
    run(&db, &mut s, "create function plus(a int, b int) returns int language sql as 'select a + b + 100'");
    run(&db, &mut s, "create operator === (leftarg = int, rightarg = int, function = plus)");
    assert_eq!(one(&db, &mut s, "select 1 === 2"), "103");
    // with its schema, as pg_dump writes an operator that isn't in pg_catalog
    assert_eq!(one(&db, &mut s, "select 1 operator(public.===) 2"), "103");
    assert_eq!(one(&db, &mut s, "select 7 operator(pg_catalog.*) 2, 7 operator(pg_catalog.%) 2"), "14");
    run(&db, &mut s, "drop operator === (int, int)");
    run(&db, &mut s, "create function addone(s int, x int) returns int language sql as 'select s + x'");
    run(&db, &mut s, "create aggregate mysum(int) (sfunc = addone, stype = int, initcond = '0')");
    run(&db, &mut s, "create table t (a int); insert into t values (1), (2), (3)");
    assert_eq!(one(&db, &mut s, "select mysum(a) from t"), "6");
    assert_eq!(one(&db, &mut s, "select mysum(a) from t where a > 5"), "0");
}

#[test]
fn user_aggregates_rules_dynamic_ddl_and_the_rest() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    // a user aggregate takes ORDER BY, DISTINCT and a window
    run(&db, &mut s, "create function cat_s(s text, x text) returns text language sql as 'select s || x'");
    run(&db, &mut s, "create aggregate cat(text) (sfunc = cat_s, stype = text, initcond = '')");
    run(&db, &mut s, "create table t (a text, b int); insert into t values ('x', 3), ('y', 1), ('z', 2), ('y', 1)");
    assert_eq!(run(&db, &mut s, "select cat(a order by b), cat(distinct a) from t"), vec![vec!["yyzx", "xyz"]]);
    assert_eq!(
        one(
            &db,
            &mut s,
            "select cat(a) over (order by a, b rows between unbounded preceding and current row) from t order by a, b limit 1"
        ),
        "x"
    );
    // a window function sees every row, whatever the LIMIT
    assert_eq!(one(&db, &mut s, "select count(*) over () from t limit 1"), "4");
    // a table isn't made a view by a _RETURN rule (Postgres 16 converts none); a view's is replaced by OR REPLACE
    run(&db, &mut s, "create table r (a int)");
    let (code, msg) = fails(&db, &mut s, "create rule \"_RETURN\" as on select to r do instead select b as a from t");
    assert_eq!((code, msg.as_str()), ("42809", "relation \"r\" cannot have ON SELECT rules"));
    run(&db, &mut s, "drop table r; create view r as select 1 as a");
    assert_eq!(
        fails(&db, &mut s, "create rule \"_RETURN\" as on select to r do instead select b as a from t").0,
        "55000"
    );
    run(&db, &mut s, "create or replace rule \"_RETURN\" as on select to r do instead select b as a from t");
    assert_eq!(one(&db, &mut s, "select sum(a) from r"), "7");
    // DDL from EXECUTE and written out in a function's body
    run(
        &db,
        &mut s,
        "create function mk() returns void language plpgsql as $$ begin create table m1 (x int); insert into m1 values (4); execute 'create table m2 (y int)'; execute 'insert into m2 values (5)'; end $$",
    );
    run(&db, &mut s, "select mk()");
    assert_eq!(one(&db, &mut s, "select (select x from m1) + (select y from m2)"), "9");
    // array elements and FOREACH in PL/pgSQL
    run(
        &db,
        &mut s,
        "create function sm(a int[]) returns int language plpgsql as $$ declare x int; n int := 0; begin a[3] := 30; foreach x in array a loop n := n + coalesce(x, 0); end loop; return n; end $$",
    );
    assert_eq!(one(&db, &mut s, "select sm('{1,2}')"), "33");
    // hypothetical-set aggregates
    run(&db, &mut s, "create table h (a int, b int); insert into h values (1,1),(1,4),(1,5),(3,1),(3,2)");
    assert_eq!(
        run(
            &db,
            &mut s,
            "select rank(3,1) within group (order by a,b), dense_rank(3,2) within group (order by a,b) from h"
        ),
        vec![vec!["4", "5"]]
    );
    // large objects past a chunk, with the transaction
    run(&db, &mut s, "select lo_from_bytea(7000, decode(repeat('ab', 100000), 'hex'))");
    assert_eq!(one(&db, &mut s, "select length(lo_get(7000))"), "100000");
    run(&db, &mut s, "select lo_put(7000, 65534, 'ZZZZ'::bytea)");
    assert_eq!(one(&db, &mut s, "select encode(lo_get(7000, 65533, 6), 'hex')"), "ab5a5a5a5aab");
    // XML mapping
    let xml = one(&db, &mut s, "select query_to_xml('select a from t where b = 3', false, true, '')");
    assert!(xml.contains("<row xmlns:xsi=") && xml.contains("<a>x</a>"), "{xml}");
    // ON CONFLICT through a view
    run(&db, &mut s, "create table base (a int primary key, b text); insert into base values (1, 'x')");
    run(&db, &mut s, "create view v as select b as bb, a as aa from base");
    run(
        &db,
        &mut s,
        "insert into v (aa, bb) values (1, 'y'), (2, 'z') on conflict (aa) do update set bb = excluded.bb || '!'",
    );
    assert_eq!(run(&db, &mut s, "select b from base order by a"), vec![vec!["y!"], vec!["z"]]);
    // jsonpath is written in its canonical form
    assert_eq!(one(&db, &mut s, "select '$.a'::jsonpath::text"), "$.\"a\"");
}

#[test]
fn records_variadics_rows_from_and_instead_of_triggers() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    // functions returning record: the call's column list, or OUT parameters, name the columns
    run(
        &db,
        &mut s,
        "create function r1() returns setof record language sql as $$ select 1, 'x'::text union all select 2, 'y' $$",
    );
    assert_eq!(
        run(&db, &mut s, "select n, s from r1() as t(n int, s text) order by n"),
        vec![vec!["1", "x"], vec!["2", "y"]]
    );
    run(
        &db,
        &mut s,
        "create function gd(v int, out a int, out b int) returns setof record language plpgsql as $$ begin return query select v, i from generate_series(1, 2) i; end $$",
    );
    assert_eq!(run(&db, &mut s, "select a, b from gd(5) order by b"), vec![vec!["5", "1"], vec!["5", "2"]]);
    // VARIADIC
    run(
        &db,
        &mut s,
        "create function least_of(variadic a numeric[]) returns numeric language sql as $$ select min(x) from unnest(a) x $$",
    );
    assert_eq!(
        run(&db, &mut s, "select least_of(10, 1, 20), least_of(7), least_of(variadic array[5, 4])"),
        vec![vec!["1", "7", "4"]]
    );
    assert_eq!(one(&db, &mut s, "select concat(variadic array[1, 2, 3])"), "123");
    // arguments by name, to a function in FROM
    run(
        &db,
        &mut s,
        "create function df(a int, b int = 5, c int = 7) returns table (x int, y int, z int) language sql as $$ select a, b, c $$",
    );
    assert_eq!(run(&db, &mut s, "select * from df(c => 3, a => 1)"), vec![vec!["1", "5", "3"]]);
    // ROWS FROM
    assert_eq!(
        run(&db, &mut s, "select * from rows from (generate_series(1, 3), generate_series(10, 11)) as t(a, b)"),
        vec![vec!["1", "10"], vec!["2", "11"], vec!["3", ""]]
    );
    // %TYPE
    run(
        &db,
        &mut s,
        "create table item (id int primary key, qty numeric(6,2), name text); insert into item values (1, 2.5, 'a')",
    );
    run(
        &db,
        &mut s,
        "create function nm(k item.id%type) returns item.name%type language plpgsql as $$ declare q item.qty%type; begin select qty into q from item where id = k; return q::text; end $$",
    );
    assert_eq!(one(&db, &mut s, "select nm(1)"), "2.50");
    // cursors in PL/pgSQL
    run(&db, &mut s, "create table ct (a int, b text); insert into ct values (1, 'x'), (2, 'y'), (3, 'z')");
    run(
        &db,
        &mut s,
        "create function cur() returns text language plpgsql as $$ declare c cursor for select a, b from ct order by a; r record; o text := ''; begin open c; loop fetch c into r; exit when not found; o := o || r.a || r.b; end loop; fetch prior from c into r; o := o || '|' || r.b; move first in c; fetch relative 1 from c into r; close c; return o || '|' || r.a; end $$",
    );
    assert_eq!(one(&db, &mut s, "select cur()"), "1x2y3z|z|2");
    run(
        &db,
        &mut s,
        "create function cur2() returns int language plpgsql as $$ declare rc refcursor; v int; n int := 0; begin open rc for execute 'select a from ct where a > $1 order by a' using 1; loop fetch rc into v; exit when not found; n := n * 10 + v; end loop; close rc; return n; end $$",
    );
    assert_eq!(one(&db, &mut s, "select cur2()"), "23");
    // INSTEAD OF triggers on a view
    run(&db, &mut s, "create table base (a int primary key, b text); create view v as select a, b from base");
    run(
        &db,
        &mut s,
        "create function v_ins() returns trigger language plpgsql as $$ begin insert into base values (new.a, upper(new.b)); return new; end $$",
    );
    run(&db, &mut s, "create trigger ti instead of insert on v for each row execute function v_ins()");
    run(&db, &mut s, "insert into v values (1, 'x'), (2, 'y')");
    assert_eq!(run(&db, &mut s, "select b from base order by a"), vec![vec!["X"], vec!["Y"]]);
}

#[test]
fn grouping_sets_windows_merge_defaults_and_sql_json() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table g (a int, b int, c int); insert into g values (1, 1, 10), (1, 2, 20), (2, 1, 30)");
    // a window function runs over every grouping set's rows together
    assert_eq!(
        run(&db, &mut s, "select sum(sum(c)) over (order by a, b) from g group by rollup (a, b) order by 1"),
        vec![vec!["10"], vec!["30"], vec!["60"], vec!["90"], vec!["120"], vec!["180"]]
    );
    assert_eq!(one(&db, &mut s, "select sum(c) from g group by cube (a, b) order by sum(c) desc, a, b limit 1"), "60");
    // ALTER TABLE ... TYPE ... USING an expression
    run(&db, &mut s, "create table t (id int primary key, n text, d int); insert into t values (1, '10', 3)");
    run(&db, &mut s, "alter table t alter column n type int using n::int * 2 + d");
    assert_eq!(one(&db, &mut s, "select n + 1 from t"), "24");
    // MERGE ... INSERT DEFAULT VALUES
    run(&db, &mut s, "create table tg (id int generated always as identity primary key, v text default 'dflt')");
    run(
        &db,
        &mut s,
        "merge into tg using (values (1), (2)) s(k) on tg.id = s.k + 100 when not matched then insert default values",
    );
    assert_eq!(one(&db, &mut s, "select count(*) from tg where v = 'dflt'"), "2");
    // to_char for numbers
    assert_eq!(
        run(&db, &mut s, "select to_char(-34338492.215, '9.999EEEE'), to_char(1994, 'FMRN'), to_char(23, 'FM99th')"),
        vec![vec!["-3.434e+07", "MCMXCIV", "23rd"]]
    );
    // text pattern operators, INSERT (SELECT), a slice with a bound left out
    assert_eq!(one(&db, &mut s, "select 'a' ~<~ 'b' and 'b' ~>=~ 'b'"), "true");
    run(&db, &mut s, "create table ar (id int primary key, a int[]); insert into ar (select 1, '{1,2,3,4,5}'::int[])");
    run(&db, &mut s, "update ar set a[:2] = '{9,8}'");
    assert_eq!(one(&db, &mut s, "select a from ar"), "{9,8,3,4,5}");
    // SQL/JSON
    assert_eq!(one(&db, &mut s, "select json_object('a': 1, 'b': null absent on null)::text"), "{\"a\" : 1}");
    assert_eq!(one(&db, &mut s, "select json_array(1, null, 'x')::text"), "[1, \"x\"]");
    assert_eq!(one(&db, &mut s, "select '[1]' is json array and 'x' is not json"), "true");
    // ALTER VIEW ... RENAME COLUMN
    run(&db, &mut s, "create view gv as select a, b from g; alter view gv rename column a to aa");
    assert_eq!(one(&db, &mut s, "select count(aa) from gv"), "3");
}

#[test]
fn transition_tables_domains_and_intervals() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    // REFERENCING OLD TABLE / NEW TABLE: a statement trigger reads the statement's rows
    run(&db, &mut s, "create table acct (id int primary key, bal int); create table audit (what text)");
    run(
        &db,
        &mut s,
        "create function upd_f() returns trigger language plpgsql as $$ begin insert into audit select o.id || ':' || o.bal || '>' || n.bal from old_rows o join new_rows n using (id) order by o.id; return null; end $$",
    );
    run(
        &db,
        &mut s,
        "create function ins_f() returns trigger language plpgsql as $$ begin insert into audit select 'ins ' || count(*) || '/' || sum(bal) from new_rows; return null; end $$",
    );
    run(
        &db,
        &mut s,
        "create trigger t_upd after update on acct referencing old table as old_rows new table as new_rows for each statement execute function upd_f()",
    );
    run(
        &db,
        &mut s,
        "create trigger t_ins after insert on acct referencing new table as new_rows for each statement execute function ins_f()",
    );
    run(&db, &mut s, "insert into acct values (1, 10), (2, 20), (3, 30)");
    run(&db, &mut s, "update acct set bal = bal + 1 where id < 3");
    assert_eq!(run(&db, &mut s, "select what from audit"), vec![vec!["ins 3/60"], vec!["1:10>11"], vec!["2:20>21"]]);
    // ALTER DOMAIN reaches the columns of the domain
    run(
        &db,
        &mut s,
        "create domain posint as int; create table dt (id int primary key, v posint); insert into dt values (1, 5)",
    );
    run(&db, &mut s, "alter domain posint add constraint pos check (value > 0)");
    assert_eq!(fails(&db, &mut s, "insert into dt values (2, -1)").0, "23514");
    run(&db, &mut s, "alter domain posint drop constraint pos; insert into dt values (2, -1)");
    run(&db, &mut s, "insert into dt values (3, null)");
    assert_eq!(fails(&db, &mut s, "alter domain posint set not null").0, "23502");
    // intervals: ISO 8601, and past a field's range is its own error
    assert_eq!(one(&db, &mut s, "select interval 'P1Y2M3DT4H5M6S'"), "1 year 2 mons 3 days 04:05:06");
    assert_eq!(fails(&db, &mut s, "select interval '2147483648 days'").0, "22015");
    assert_eq!(one(&db, &mut s, "select interval '-9223372036854775808 us'"), "-2562047788:00:54.775808");
    // CREATE TABLE AS EXECUTE
    run(&db, &mut s, "prepare q (int) as select id from acct where id >= $1; create table kept as execute q(2)");
    assert_eq!(one(&db, &mut s, "select count(*) from kept"), "2");
}

/// CREATE AGGREGATE as Postgres's regression suite writes it: the old form and its obsolete spellings, built-ins
/// named by their C symbols, `(*)`, ORDER BY and VARIADIC signatures, support functions looked up for the types
/// they're given (and refused as Postgres refuses them), strict and non-strict state functions, OR REPLACE,
/// RENAME, COMMENT ON, and DROP FUNCTION of a state function.
#[test]
fn aggregates_as_postgres_defines_them() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(&db, &mut s, "create table t (a int, b float8); insert into t values (1, 1.5), (2, null), (3, 2.5)");
    run(&db, &mut s, "create aggregate newsum (sfunc1 = int4pl, basetype = int4, stype1 = int4, initcond1 = '0')");
    assert_eq!(one(&db, &mut s, "select newsum(a) from t"), "6");
    run(&db, &mut s, "create aggregate oldcnt (sfunc = int8inc, basetype = 'ANY', stype = int8, initcond = '0')");
    assert_eq!(one(&db, &mut s, "select oldcnt(*) from t"), "3");
    run(&db, &mut s, "create aggregate newcnt (*) (sfunc = int8inc, stype = int8, initcond = '0', parallel = safe)");
    assert_eq!(one(&db, &mut s, "select newcnt(*) from t"), "3");
    // a strict state function skips the nulls and starts from the first value
    run(&db, &mut s, "create aggregate fsum(float8) (sfunc = float8pl, stype = float8)");
    assert_eq!(one(&db, &mut s, "select fsum(b) from t"), "4");
    // SFUNC = f(types): the types are read and not used
    run(&db, &mut s, "create aggregate isum(int4) (sfunc = int4_sum(int8, int4), stype = int8, initcond = '0')");
    assert_eq!(one(&db, &mut s, "select isum(a) from t"), "6");
    // a non-strict one sees every row, the first with a null state; named as the aggregate with its final function
    run(
        &db,
        &mut s,
        "create function sum_t(s int, n int) returns int language sql as 'select coalesce(s, 0) + coalesce(n, 0)'",
    );
    run(&db, &mut s, "create function half(s int) returns int language sql as 'select s / 2'");
    run(&db, &mut s, "create aggregate my_half_sum(int) (stype = int, sfunc = sum_t, finalfunc = half)");
    let out =
        s.run(&db, "select my_half_sum(a) from t", &[]).unwrap_or_else(|e| panic!("{}", e.message)).pop().unwrap();
    assert_eq!(out.columns.unwrap()[0].0, "my_half_sum");
    assert_eq!(out.rows[0][0].to_text(), "3");
    // what Postgres refuses
    for (q, code, msg) in [
        ("create aggregate x (sfunc = int4pl, stype = int4)", "42P13", "aggregate input type must be specified"),
        ("create aggregate x (int) (sfunc = int4pl)", "42P13", "aggregate stype must be specified"),
        ("create aggregate x (int) (stype = int)", "42P13", "aggregate sfunc must be specified"),
        (
            "create aggregate x (int) (stype = int, sfunc = int4pl, parallel = pear)",
            "42601",
            "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE",
        ),
        (
            "create aggregate x (int4) (sfunc = int4pl, stype = int4, finalfunc = int2um, initcond = '0')",
            "42883",
            "function int2um(integer) does not exist",
        ),
        (
            "create aggregate x (int4) (stype = int8, sfunc = int8pl)",
            "42804",
            "function int8pl(bigint, bigint) requires run-time type coercion",
        ),
        (
            "create aggregate x (numeric) (stype = internal, sfunc = numeric_avg_accum, serialfunc = numeric_avg_serialize)",
            "42P13",
            "must specify both or neither of serialization and deserialization functions",
        ),
        (
            "create aggregate x (numeric) (stype = internal, sfunc = numeric_avg_accum, \
             serialfunc = numeric_avg_deserialize, deserialfunc = numeric_avg_deserialize)",
            "42883",
            "function numeric_avg_deserialize(internal) does not exist",
        ),
        (
            "create aggregate x (int) (stype = anyarray, sfunc = array_append)",
            "42P13",
            "cannot determine transition data type",
        ),
        (
            "create aggregate x (float8) (stype = float8, sfunc = float8pl, mstype = float8, msfunc = float8pl)",
            "42P13",
            "aggregate minvfunc must be specified when mstype is specified",
        ),
        (
            "create aggregate x (int) (stype = int, sfunc = int4pl, hypothetical)",
            "42P13",
            "only ordered-set aggregates can be hypothetical",
        ),
    ] {
        assert_eq!(fails(&db, &mut s, q), (code, msg.to_string()), "{q}");
    }
    // moving-aggregate options, and ordered-set and hypothetical-set signatures
    run(
        &db,
        &mut s,
        "create aggregate sumdouble (float8) (stype = float8, sfunc = float8pl, mstype = float8, msfunc = float8pl, \
         minvfunc = float8mi)",
    );
    run(
        &db,
        &mut s,
        "create aggregate my_percentile_disc(float8 order by anyelement) (stype = internal, \
         sfunc = ordered_set_transition, finalfunc = percentile_disc_final, finalfunc_extra = true, \
         finalfunc_modify = read_write)",
    );
    run(
        &db,
        &mut s,
        "create aggregate my_rank(variadic \"any\" order by variadic \"any\") (stype = internal, \
         sfunc = ordered_set_transition_multi, finalfunc = rank_final, finalfunc_extra = true, hypothetical)",
    );
    run(&db, &mut s, "alter aggregate my_rank(variadic \"any\" order by variadic \"any\") rename to test_rank");
    run(&db, &mut s, "drop aggregate test_rank(variadic \"any\" order by variadic \"any\")");
    // a function by an aggregate's signature, OR REPLACE
    run(
        &db,
        &mut s,
        "create function sum3(int8, int8, int8) returns int8 language sql strict as 'select $1 + $2 + $3'",
    );
    run(
        &db,
        &mut s,
        "create function sum4(int8, int8, int8, int8) returns int8 language sql strict as 'select $1 + $2 + $3 + $4'",
    );
    let q = "create or replace aggregate sum3 (int8, int8, int8) (stype = int8, sfunc = sum4)";
    assert_eq!(fails(&db, &mut s, q), ("42809", "cannot change routine kind".to_string()));
    run(&db, &mut s, "create aggregate myavg (numeric) (stype = numeric, sfunc = numeric_add)");
    run(
        &db,
        &mut s,
        "create or replace aggregate myavg (numeric) (stype = numeric, sfunc = numeric_add, initcond = '0')",
    );
    let q = "create aggregate myavg (numeric) (stype = numeric, sfunc = numeric_add)";
    assert_eq!(
        fails(&db, &mut s, q),
        ("42723", "function \"myavg\" already exists with same argument types".to_string())
    );
    let q =
        "create or replace aggregate myavg (numeric) (stype = numeric, sfunc = numeric_add, finalfunc = numeric_out)";
    assert_eq!(fails(&db, &mut s, q), ("42P13", "cannot change return type of existing function".to_string()));
    let q = "create or replace aggregate myavg (order by numeric) (stype = numeric, sfunc = numeric_add)";
    assert_eq!(fails(&db, &mut s, q), ("42809", "cannot change routine kind".to_string()));
    // RENAME, COMMENT ON, DROP
    run(&db, &mut s, "alter aggregate newsum(int4) rename to newsum2");
    assert_eq!(one(&db, &mut s, "select newsum2(a) from t"), "6");
    let q = "comment on aggregate newsum(int4) is 'x'";
    assert_eq!(fails(&db, &mut s, q), ("42883", "aggregate newsum(integer) does not exist".to_string()));
    run(&db, &mut s, "comment on aggregate newsum2(int4) is 'x'");
    let q = "drop aggregate sum3(int8, int8, int8)";
    assert_eq!(
        fails(&db, &mut s, q),
        ("42809", "function sum3(bigint, bigint, bigint) is not an aggregate".to_string())
    );
    assert_eq!(
        fails(&db, &mut s, "drop aggregate nosuch(*)"),
        ("42883", "aggregate nosuch(*) does not exist".to_string())
    );
    run(&db, &mut s, "drop aggregate if exists nosuch(int)");
    // a state function isn't dropped from under its aggregate, unless with it
    run(&db, &mut s, "create function least_accum(int8, int8) returns int8 language sql as 'select least($1, $2)'");
    let q = "create aggregate least_agg(int4) (stype = int8, sfunc = least_accum)";
    let coercion = "function least_accum(bigint, bigint) requires run-time type coercion";
    assert_eq!(fails(&db, &mut s, q), ("42804", coercion.to_string()));
    run(&db, &mut s, "create aggregate least_agg(int8) (stype = int8, sfunc = least_accum)");
    assert_eq!(fails(&db, &mut s, "drop function least_accum(int8, int8)").0, "2BP01");
    run(&db, &mut s, "drop function least_accum(int8, int8) cascade");
    assert_eq!(fails(&db, &mut s, "drop aggregate least_agg(int8)").0, "42883");
    // a polymorphic state type is what the arguments make it
    // ponytail: a SQL function as the step of an array state (`select $1 || $2`) joins the state's text instead of
    // appending to the array (22P02); a built-in step is what this checks
    run(&db, &mut s, "create aggregate collect(int) (stype = int[], sfunc = array_append, initcond = '{}')");
    assert_eq!(one(&db, &mut s, "select array_length(collect(a), 1) from t"), "3");
}

/// LANGUAGE internal: a function that is one of Postgres's built-ins by its C symbol runs as what that does.
#[test]
fn language_internal_functions() {
    let (db, mut s) = (Db::new(), Session::new(MAIN));
    run(
        &db,
        &mut s,
        "create function my_int_eq(int, int) returns bool language internal strict immutable parallel safe as $$int4eq$$",
    );
    assert_eq!(run(&db, &mut s, "select my_int_eq(2, 2), my_int_eq(2, 3)"), vec![vec!["true", "false"]]);
    run(
        &db,
        &mut s,
        "create function leak(integer, integer) returns boolean as 'int4lt' language internal immutable strict",
    );
    assert_eq!(one(&db, &mut s, "select leak(1, 2)"), "true");
    run(&db, &mut s, "create function plus(int, int) returns int language internal as 'int4pl'");
    assert_eq!(one(&db, &mut s, "select plus(2, 3)"), "5");
    run(&db, &mut s, "create function cat2(text, text) returns text language internal as 'textcat'");
    assert_eq!(one(&db, &mut s, "select cat2('a', 'b')"), "ab");
    run(
        &db,
        &mut s,
        "create function my_sin(float8) returns float8 language internal immutable parallel safe strict as 'dsin'",
    );
    assert_eq!(one(&db, &mut s, "select my_sin(0)"), "0");
    run(
        &db,
        &mut s,
        "create function my_gen_series(int, int) returns setof integer language internal strict as $$generate_series_int4$$",
    );
    assert_eq!(one(&db, &mut s, "select count(*) from my_gen_series(1, 10)"), "10");
    let q = "create function test1 (int) returns int language internal as 'nosuch'";
    assert_eq!(fails(&db, &mut s, q), ("42883", "there is no built-in function named \"nosuch\"".to_string()));
}
