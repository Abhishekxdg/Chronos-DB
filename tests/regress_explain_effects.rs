//! EXPLAIN without ANALYZE changes nothing, as Postgres's runs nothing: Chronos runs a query to see how it reads its
//! tables only when running it can't change anything. One that calls a function that writes (made with CREATE
//! FUNCTION or CREATE AGGREGATE, or a built-in: nextval, setval, pg_notify, the large-object writers, ts_stat's query),
//! locks rows, or changes rows in a WITH is explained without being run; what a query reaches that it doesn't name (a
//! PL/pgSQL function's statements, a domain's CHECK, a function of CREATE CAST) is stopped before it changes
//! anything: a write, a notification, a cursor opened, moved or closed, the statements' statistics reset. So by every
//! way there is to EXPLAIN: Chronos's steps (the default), PostgreSQL's plan (chronos.explain = postgres, Chronos's
//! steps where it can't make one), exact mode (CHRONOS_PG_EXACT, the regression suite's: in a child process, as it is
//! read once), EXPLAIN EXECUTE and PL/pgSQL's FOR ... IN EXPLAIN. And an agent that may only read can't write by one.
//! A query that only reads, through built-ins or functions that only read, is still run, as before.

use chronos::sql::{Session, Value};
use chronos::{Db, MAIN};

fn run(db: &Db, s: &mut Session, q: &str) -> Vec<Vec<String>> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(Value::to_text).collect()).collect()
}

const SETUP: &str = "
    create table t (id int primary key, v text);
    insert into t values (1, 'a'), (2, 'b');
    create table log (n int);
    create sequence s;
    select lo_from_bytea(5001, 'kept');
    create function ins() returns int language sql as $$ insert into log values (1); select 1 $$;
    create function churn() returns bool language plpgsql as $$
        begin update t set v = 'x'; delete from log; insert into log values (2); return true; end $$;
    create function noisy() returns int language plpgsql as $$ begin perform pg_notify('c', 'noisy'); return 1; end $$;
    create function step(acc int, x int) returns int language plpgsql as $$
        begin insert into log values (x); return coalesce(acc, 0) + x; end $$;
    create aggregate logged(int) (sfunc = step, stype = int);
    create view calls_ins as select id, ins() as n from t;
    create function chk(int) returns bool language plpgsql as $$ begin insert into log values (9); return true; end $$;
    create domain dd as int check (chk(value));
    create domain dn as int check (nextval('s') > 0);
    create type pt as (a int);
    create function to_pt(int) returns pt language plpgsql as $$
        begin insert into log values (7); return row($1)::pt; end $$;
    create cast (int as pt) with function to_pt(int);
    create function lt_int(int, int) returns bool immutable strict language internal as 'int4lt';
    create operator <<< (procedure = int4lt, leftarg = int, rightarg = int);
    create function dbl(int) returns int immutable language sql as 'select $1 * 2';
    create aggregate mysum(int) (sfunc = int4pl, stype = int);
    create function rd(int) returns int stable language plpgsql as $$ begin return $1; end $$;
    create function leak(text) returns bool language plpgsql as $$ begin raise notice 'leak %', $1; return true; end $$;
    create function opens(int) returns bool language plpgsql as $$
        declare c refcursor := 'leftopen'; begin open c for select 1; return true; end $$;
    create function moves() returns bool language plpgsql as $$
        declare c refcursor := 'leftopen'; x int; begin fetch c into x; return true; end $$;
    create function closes() returns bool language plpgsql as $$
        declare c refcursor := 'leftopen'; begin close c; return true; end $$;
    create domain dc as int check (opens(value));
    create function i8t_eq(int8, text) returns bool strict immutable language internal as 'int8eq';
    create operator = (procedure = i8t_eq, leftarg = int8, rightarg = text);
    create function i82_eq(int8, int2) returns bool immutable language sql as 'select $1 = $2::int8';
    create operator = (procedure = i82_eq, leftarg = int8, rightarg = int2);
    begin;
    create function bogus_int4_text_eq(int4, text) returns bool language sql as 'select $1::text = $2';
    create operator = (procedure = bogus_int4_text_eq, leftarg = int4, rightarg = text);
    rollback";

/// What the queries below could change: t's rows, log's, s's counter, large object 5001, and which of the large
/// objects they would make there are.
fn state(db: &Db, s: &mut Session) -> Vec<String> {
    let q = "select (select string_agg(id || v, ',' order by id) from t), (select count(*) from log), \
        (select last_value || ' ' || is_called from s)";
    let mut now = run(db, s, q).remove(0);
    for oid in [5001, 6001, 6002, 6003, 6004, 6005, 6006, 6007, 6008, 7001] {
        if let Ok(out) = s.run(db, &format!("select convert_from(lo_get({oid}), 'utf8')"), &[]) {
            now.push(format!("large object {oid}: {}", out[0].rows[0][0].to_text()));
        }
    }
    now
}

/// Queries that change something when they run.
const QUERIES: &[&str] = &[
    // built-in functions that change something
    "select lo_create(6001)",
    "select lo_from_bytea(6002, 'new')",
    "select lo_put(5001, 0, 'gone')",
    "select lo_unlink(5001)",
    "select nextval('s')",
    "select setval('s', 100)",
    "select * from t where nextval('s') > 0",
    "select pg_notify('c', 'x')",
    "select * from t, lateral (select pg_notify('c', t.v)) n",
    // functions made with CREATE FUNCTION and CREATE AGGREGATE, called, or read through a view
    "select ins()",
    "select churn()",
    "select * from t where churn()",
    "select noisy() from t",
    "select logged(id) from t",
    "select * from calls_ins",
    // rows changed by a WITH, or locked
    "with d as (delete from t returning *) select * from d",
    "with u as (update t set v = 'y' returning *) select * from u",
    "with i as (insert into log values (3) returning *) select * from i",
    "select * from t for update",
    "select * from (select * from t for update) x",
    // writes, whose WHERE (or the rows they write) would change something
    "update t set v = 'y' where nextval('s') > 0",
    "update t set v = 'z' where churn()",
    "delete from t where ins() = 1",
    "delete from t where lo_create(6003) > 0",
    "insert into log select ins()",
    "insert into log values (nextval('s'))",
    "merge into t using (select 1 as id) x on t.id = x.id when matched then delete",
    // DISTINCT ON's expressions, the query's own or a subquery's
    "select distinct on (lo_unlink(5001)) id from t",
    "select distinct on (lo_create(6006)) id from t",
    "select distinct on (nextval('s')) id from t",
    "select distinct on (ins()) id from t",
    "select distinct on (pg_notify('c', 'on')::text) id from t",
    "select * from t where id in (select distinct on (nextval('s')) id from t)",
    "update t set v = 'z' where id in (select distinct on (nextval('s')) id from t)",
    // queries given as text
    "select * from ts_stat('delete from t returning to_tsvector(v)')",
    "select * from ts_stat('select to_tsvector(''a'') from (select ins()) x')",
    "select * from ts_stat('select to_tsvector(lo_create(6007)::text)')",
    "select * from ts_stat('select to_tsvector(nextval(''s'')::text)')",
    "select * from t where exists (select * from ts_stat('select to_tsvector(nextval(''s'')::text)'))",
    "update t set v = 'q' where v in (select word from ts_stat('delete from t returning to_tsvector(v)'))",
    "select query_to_xml('select lo_create(6008)', true, false, '')",
    "select * from crosstab('select 1, ''a''::text, ins()::text') as ct(r int, c1 text)",
    // what a cast runs, which the query doesn't name: a domain's CHECK, CREATE CAST's function
    "select 1::dd",
    "select id::dd from t",
    "select 5::dn",
    "select 1::pt",
    "select * from t where (id::pt).a > 0",
];

/// Queries that only read, through built-ins, functions that only read (LANGUAGE internal's, SQL's, PL/pgSQL's) and
/// an operator and an aggregate of built-in functions: still run to see how they read t. So is `=`, which `SETUP`'s
/// operators of the symbol leave as it was: theirs compare with `=` themselves, and one's CREATE was rolled back (as
/// the regression suite's equivclass and subselect make them).
const READS: &[&str] = &[
    "select * from t where v = 'a'",
    "select * from t where id = 1",
    "select * from t where v = 'a' and id = 2",
    "select * from t where rd(id) > 0",
    "select * from t where leak(v)",
    "select * from t where ts_rewrite('a'::tsquery, 'a'::tsquery, 'b'::tsquery) is not null",
    "select * from t where lo_get(5001) is not null",
    "select * from t, upper('x') u",
    "select * from t, now() n",
    "select * from t cross join lateral upper(t.v) u",
    "select * from t, rows from (generate_series(1, 2)) g",
    "select * from t, jsonb_to_recordset('[{\"id\": \"1\"}]') as j (id text)",
    "select * from t where lt_int(id, 5)",
    "select * from t where id <<< 5",
    "select dbl(id) from t",
    "select mysum(id) from t",
    "select distinct on (v) id from t",
    "select random() from t",
];

/// A database with what `SETUP` makes, a session on it, and another LISTENing on channel c.
fn setup() -> (Db, Session, Session) {
    let (db, mut s, mut listener) = (Db::new(), Session::new(MAIN), Session::new(MAIN));
    run(&db, &mut s, SETUP);
    run(&db, &mut listener, "listen c");
    (db, s, listener)
}

/// Runs `sql` as `s`, which must work (or, for an agent's session with its `owner`'s, may be refused with 42501), and
/// says what it changed or whom it notified, if anything.
fn effect(db: &Db, s: &mut Session, mut owner: Option<&mut Session>, listener: &Session, sql: &str) -> Option<String> {
    let before = state(db, owner.as_deref_mut().unwrap_or(&mut *s));
    let ran = s.run(db, sql, &[]);
    let after = state(db, owner.as_deref_mut().unwrap_or(&mut *s));
    let heard: Vec<String> = listener.inbox.take().into_iter().map(|n| n.payload).collect();
    match ran {
        Err(e) if !(owner.is_some() && e.code == "42501") => Some(format!("{sql}: {} {}", e.code, e.message)),
        _ if after != before => Some(format!("{sql}: changed {before:?} to {after:?}")),
        _ if !heard.is_empty() => Some(format!("{sql}: notified {heard:?}")),
        _ => None,
    }
}

/// Every query, explained with and without COSTS OFF, directly, by EXPLAIN EXECUTE and from PL/pgSQL; and rows that
/// EXPLAIN of a FOR UPDATE leaves for another transaction to lock.
fn explains_without_effects(db: &Db, s: &mut Session, listener: &Session) {
    run(
        db,
        s,
        "create function plan_of(q text) returns setof text language plpgsql as $$
         declare ln text;
         begin for ln in execute 'explain ' || q loop return next ln; end loop; end $$",
    );
    let mut wrong = vec![];
    for q in QUERIES {
        for explain in ["explain", "explain (costs off)"] {
            wrong.extend(effect(db, s, None, listener, &format!("{explain} {q}")));
        }
        wrong.extend(effect(db, s, None, listener, &format!("select * from plan_of('{}')", q.replace('\'', "''"))));
    }
    // a prepared statement's, and FOR ... IN EXPLAIN's own
    run(db, s, "prepare effects as select ins(), nextval('s'), pg_notify('c', 'p'), lo_create(6004), churn()");
    run(db, s, "prepare change(int) as update t set v = 'q' where id = $1 and churn()");
    for explain in ["explain", "explain (costs off)"] {
        wrong.extend(effect(db, s, None, listener, &format!("{explain} execute effects")));
        wrong.extend(effect(db, s, None, listener, &format!("{explain} execute change(1)")));
    }
    run(
        db,
        s,
        "create function static_plan() returns setof text language plpgsql as $$
         declare ln text;
         begin
           for ln in explain select ins(), nextval('s'), pg_notify('c', 'static'), lo_create(6005), churn() loop
             return next ln;
           end loop;
         end $$",
    );
    wrong.extend(effect(db, s, None, listener, "select * from static_plan()"));
    assert!(wrong.is_empty(), "EXPLAIN without ANALYZE changed something:\n{}", wrong.join("\n"));
    // no row is left locked, in a transaction that goes on
    let mut other = Session::new(MAIN);
    run(db, s, "begin");
    for q in ["select * from t for update", "select * from (select * from t for update) x"] {
        for explain in ["explain", "explain (costs off)"] {
            run(db, s, &format!("{explain} {q}"));
        }
    }
    run(db, &mut other, "begin");
    assert_eq!(run(db, &mut other, "select id from t order by id for update nowait"), [["1"], ["2"]]);
    run(db, &mut other, "rollback");
    run(db, s, "rollback");
    // nor a cursor opened, moved or closed: one that PL/pgSQL keeps in the session, by a function called or a domain's
    // CHECK
    run(db, s, "begin");
    for q in ["select 1::dc", "select * from t where opens(id)"] {
        for explain in ["explain", "explain (costs off)"] {
            run(db, s, &format!("{explain} {q}"));
        }
    }
    let left = s.run(db, "fetch next from leftopen", &[]).err().map(|e| e.code);
    assert_eq!(left, Some("34000"), "EXPLAIN left a cursor open");
    run(db, s, "rollback");
    run(db, s, "begin");
    run(db, s, "select opens(1)");
    for q in ["select * from t where moves()", "select * from t where closes()"] {
        for explain in ["explain", "explain (costs off)"] {
            run(db, s, &format!("{explain} {q}"));
        }
    }
    assert_eq!(run(db, s, "fetch next from leftopen"), [["1"]], "EXPLAIN moved or closed a cursor");
    run(db, s, "rollback");
    // and a query that changes nothing is still run to see how it reads its tables
    for q in READS {
        let lines = run(db, s, &format!("explain {q}"));
        assert!(lines.iter().any(|l| l[0].contains(" on t")), "{q}: {lines:?}");
    }
}

#[test]
fn with_chronos_steps() {
    let (db, mut s, listener) = setup();
    explains_without_effects(&db, &mut s, &listener);
}

#[test]
fn with_postgres_plans() {
    let (db, mut s, listener) = setup();
    run(&db, &mut s, "set chronos.explain = postgres");
    explains_without_effects(&db, &mut s, &listener);
}

#[test]
fn in_exact_mode() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "in_exact_mode_child", "--nocapture", "--test-threads=1"])
        .env("CHRONOS_PG_EXACT", "1")
        .output()
        .unwrap();
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success() && said.contains("1 passed"), "{said}");
}

/// `in_exact_mode`'s child: nothing unless CHRONOS_PG_EXACT is set.
#[test]
fn in_exact_mode_child() {
    if std::env::var("CHRONOS_PG_EXACT").as_deref() != Ok("1") {
        return;
    }
    let (db, mut s, listener) = setup();
    explains_without_effects(&db, &mut s, &listener);
}

/// The query ts_stat, ts_rewrite and query_to_xml are given as text runs as Postgres's SPI runs it for them,
/// read-only: one that would change rows or lock them is refused (0A000); a cursor's (ts_stat's, ts_rewrite's) is of
/// one statement that gives rows (42P11); and nothing changes. One that reads runs as before.
#[test]
fn queries_given_as_text_only_read() {
    let (db, mut s, _listener) = setup();
    let before = state(&db, &mut s);
    for (sql, code) in [
        ("select * from ts_stat('delete from t returning to_tsvector(v)')", "0A000"),
        ("select * from ts_stat('select to_tsvector(v) from t for update')", "0A000"),
        ("select * from ts_stat('select to_tsvector(v) from (select * from t for update) x')", "0A000"),
        ("select * from ts_stat('select to_tsvector(v) from t; delete from t')", "42P11"),
        ("select * from ts_stat('delete from t')", "42P11"),
        ("select ts_rewrite('a'::tsquery, 'delete from t returning ''a''::tsquery, ''b''::tsquery')", "0A000"),
        ("select query_to_xml('delete from t returning id', true, false, '')", "0A000"),
        ("select query_to_xml('select 1; update t set v = ''x''', true, false, '')", "0A000"),
    ] {
        let e = s.run(&db, sql, &[]).err().unwrap_or_else(|| panic!("{sql}: ran"));
        assert_eq!(e.code, code, "{sql}: {}", e.message);
    }
    assert_eq!(state(&db, &mut s), before);
    let words = run(&db, &mut s, "select word from ts_stat('select to_tsvector(''simple'', v) from t') order by word");
    assert_eq!(words, [["a"], ["b"]]);
}

/// EXPLAIN of random() leaves setseed's numbers as they were: the next random() is the one it would have been.
#[test]
fn random_numbers_go_on_as_if_unexplained() {
    let (db, mut s, _listener) = setup();
    run(&db, &mut s, "select setseed(0.5)");
    let unexplained = run(&db, &mut s, "select random()");
    for q in ["explain select random() from t", "explain (costs off) select * from t where random() > 0"] {
        run(&db, &mut s, "select setseed(0.5)");
        run(&db, &mut s, q);
        assert_eq!(run(&db, &mut s, "select random()"), unexplained, "{q}");
    }
}

/// EXPLAIN of a query calling a function there isn't fails, as Postgres's does (and as it did before): it runs, as
/// the call changes nothing.
#[test]
fn a_function_there_isnt_is_an_error() {
    let (db, mut s, _listener) = setup();
    for q in ["select * from t where no_such_function(id)", "select * from t, no_such_rows(1) r"] {
        let e = s.run(&db, &format!("explain {q}"), &[]).err().map(|e| e.code);
        assert_eq!(e, Some("42883"), "{q}");
    }
}

/// EXPLAIN of pg_stat_statements_reset() leaves the statements' statistics as they were.
#[test]
fn statement_statistics_are_kept() {
    let (db, mut s, _listener) = setup();
    run(&db, &mut s, "create extension pg_stat_statements");
    for q in ["select count(*) from t", "select count(*) from log", "select 1"] {
        run(&db, &mut s, q);
    }
    let count = |s: &mut Session| run(&db, s, "select count(*) from pg_stat_statements")[0][0].parse::<i64>().unwrap();
    let before = count(&mut s);
    for q in ["select pg_stat_statements_reset()", "select * from t where pg_stat_statements_reset() is null"] {
        for explain in ["explain", "explain (costs off)"] {
            run(&db, &mut s, &format!("{explain} {q}"));
        }
    }
    assert!(count(&mut s) >= before, "EXPLAIN reset the statements' statistics");
}

/// An agent that may only read EXPLAINs what it may read; one of a query that would change something is refused, or
/// explained without being run. EXECUTE and EXPLAIN EXECUTE work out their arguments first (as Postgres's EXPLAIN
/// EXECUTE does too): one that changes something is a write.
#[test]
fn an_agent_that_may_only_read_cannot_write_by_explain() {
    let (db, mut owner, listener) = setup();
    run(&db, &mut owner, "create agent reader with (can = 'read')");
    let mut reader = Session::new(MAIN);
    reader.agent = Some("reader".into());
    run(&db, &mut reader, "prepare any_of(int) as select $1");
    let mut tried: Vec<String> = QUERIES.iter().map(|q| format!("explain {q}")).collect();
    // (and what DISTINCT ON and ts_stat's query run, without EXPLAIN: a write)
    for q in [
        "select distinct on (lo_unlink(5001)) id from t",
        "select distinct on (nextval('s')) id from t",
        "select * from ts_stat('delete from t returning to_tsvector(v)')",
        "select * from ts_stat('select to_tsvector(''a'') from (select ins()) x')",
        "select * from crosstab('select 1, ''a''::text, ins()::text') as ct(r int, c1 text)",
        "select query_to_xml('select lo_create(6008)', true, false, '')",
    ] {
        tried.push(q.to_string());
    }
    for arg in ["lo_create(7001)", "nextval('s')", "case when pg_notify('c', 'arg') is null then 1 end"] {
        tried.push(format!("explain execute any_of({arg})"));
        tried.push(format!("execute any_of({arg})"));
    }
    let wrong: Vec<String> =
        tried.iter().filter_map(|sql| effect(&db, &mut reader, Some(&mut owner), &listener, sql)).collect();
    assert!(wrong.is_empty(), "an agent that may only read changed something:\n{}", wrong.join("\n"));
    // what it may read it explains (with no `=`, which `SETUP`'s operators of the symbol make a call of a function)
    let lines = run(&db, &mut reader, "explain select * from t where v > 'a'");
    assert!(lines.iter().any(|l| l[0].contains(" on t")), "{lines:?}");
}
