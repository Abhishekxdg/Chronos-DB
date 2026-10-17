//! The SQL engine on its own (the Postgres protocol is tested in pg.rs).

use chronos::sql::{Output, Type, Value, run};
use chronos::{Db, MAIN, Resolve};

fn sql(db: &Db, q: &str) -> Output {
    run(db, MAIN, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap()
}

fn fails(db: &Db, q: &str) -> (&'static str, String) {
    let e = run(db, MAIN, q, &[]).expect_err(q);
    (e.code, e.message)
}

/// Rows as text, for compact asserts; NULL is "null".
fn rows(o: &Output) -> Vec<String> {
    o.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn people() -> Db {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table people (id integer primary key, name text not null, age int, email varchar(100), admin boolean default false);
         insert into people (id, name, age, email) values
           (1, 'Ada', 36, 'ada@example.com'), (2, 'Alan', 41, null), (3, 'Grace', 85, 'grace@navy.mil'), (4, 'Linus', null, 'l@kernel.org');",
        &[],
    )
    .unwrap();
    db
}

#[test]
fn select_where_order_limit() {
    let db = people();
    let o = sql(&db, "select * from people");
    let cols: Vec<_> = o.columns.clone().unwrap().into_iter().map(|(n, t)| format!("{n}:{}", t.name())).collect();
    assert_eq!(cols, ["id:integer", "name:text", "age:integer", "email:text", "admin:boolean"]);
    assert_eq!(rows(&o)[0], "1|Ada|36|ada@example.com|false"); // default filled in
    assert_eq!(o.tag, "SELECT 4");

    assert_eq!(rows(&sql(&db, "select name from people where age > 40 order by age desc")), ["Grace", "Alan"]);
    assert_eq!(rows(&sql(&db, "select name from people where age is null")), ["Linus"]);
    assert_eq!(rows(&sql(&db, "select name from people where not (age > 40)")), ["Ada"]); // NULL age is neither
    assert_eq!(
        rows(&sql(&db, "select name, age from people order by age")),
        ["Ada|36", "Alan|41", "Grace|85", "Linus|null"]
    );
    assert_eq!(rows(&sql(&db, "select name from people order by age desc limit 2")), ["Linus", "Grace"]);
    assert_eq!(rows(&sql(&db, "select name from people order by 1 limit 2 offset 1")), ["Alan", "Grace"]);
    assert_eq!(rows(&sql(&db, "select name n, age + 1 as next from people where id = 1")), ["Ada|37"]);
    assert_eq!(rows(&sql(&db, "select name from people where name like 'A%' and id in (1, 2, 9)")), ["Ada", "Alan"]);
    assert_eq!(rows(&sql(&db, "select name from people where email ilike '%.ORG'")), ["Linus"]);
    assert_eq!(rows(&sql(&db, "select id from people where age between 40 and 90")), ["2", "3"]);
    assert_eq!(rows(&sql(&db, "select name || '!' from people where id = 3")), ["Grace!"]);
    assert_eq!(
        rows(&sql(&db, "SELECT 1 + 2 * 3, 7 / 2, 7.0 / 2, 'it''s', null IS NULL")),
        ["7|3|3.5000000000000000|it's|true"],
        "7.0 / 2 is numeric, as in Postgres"
    );
    assert_eq!(rows(&sql(&db, "select id from people where id = '2'")), ["2"]); // quoted literal read as a number
}

#[test]
fn parameters() {
    let db = people();
    let o = run(
        &db,
        MAIN,
        "select name from people where age > $1 and name <> $2",
        &[Value::Int(30), Value::Text("Ada".into())],
    )
    .unwrap()
    .pop()
    .unwrap();
    assert_eq!(rows(&o), ["Alan", "Grace"]);

    let s = &chronos::sql::parse("select name from people where id = $1 and age < $2 limit $3").unwrap()[0];
    assert_eq!(s.params(&db, MAIN, &[]).unwrap(), [Type::Int, Type::Int, Type::BigInt]);
    let s = &chronos::sql::parse("insert into people (id, name, admin) values ($1, $2, $3)").unwrap()[0];
    assert_eq!(s.params(&db, MAIN, &[]).unwrap(), [Type::Int, Type::Text, Type::Bool]);
    // text parameters (how most drivers send them) become the column's type
    let types = s.params(&db, MAIN, &[]).unwrap();
    let vals = ["9", "Tim", "t"].map(|v| Value::Text(v.into()));
    s.execute(&db, MAIN, &vals, &types).unwrap();
    assert_eq!(rows(&sql(&db, "select age, admin from people where id = 9")), ["null|true"]);
    assert_eq!(run(&db, MAIN, "select $1", &[]).unwrap_err().code, "08P01");
}

#[test]
fn writes_and_returning() {
    let db = people();
    let o = sql(&db, "update people set age = age + 1, admin = true where age < 50 returning id, age");
    assert_eq!((o.tag.as_str(), rows(&o)), ("UPDATE 2", vec!["1|37".to_string(), "2|42".into()]));
    // moving a row to a new key, and swapping keys within one statement
    sql(&db, "update people set id = 10 where id = 4");
    assert_eq!(rows(&sql(&db, "select name from people where id = 10")), ["Linus"]);
    sql(&db, "update people set id = 3 - id where id in (1, 2)");
    assert_eq!(rows(&sql(&db, "select id, name from people where id < 3")), ["1|Alan", "2|Ada"]);

    let o = sql(&db, "delete from people where admin returning name");
    assert_eq!((o.tag.as_str(), rows(&o)), ("DELETE 2", vec!["Alan".to_string(), "Ada".into()]));
    assert_eq!(sql(&db, "delete from people where id = 99").tag, "DELETE 0");
    let o = sql(&db, "insert into people values (5, 'Barbara', 1, null, true) returning *");
    assert_eq!(rows(&o), ["5|Barbara|1|null|true"]);
    assert_eq!(rows(&sql(&db, "select id from people")), ["3", "5", "10"]); // key order: integer keys as numbers
    // rows are the same JSON records the rest of Chronos reads
    assert_eq!(&*db.get(MAIN, "people/5").unwrap().unwrap(), r#"{"admin":true,"age":1,"id":5,"name":"Barbara"}"#);
}

/// A WITH query used more than once is computed once, and every use reads the same rows, as in
/// Postgres; one that reads an outer query's values is computed for each of them, and a loop in
/// a function computes it again each time round.
#[test]
fn a_with_query_used_twice_is_computed_once() {
    let db = Db::new();
    sql(&db, "create sequence s; create table t (id int primary key)");
    sql(&db, "insert into t values (1), (2), (3)");
    let q = "with r as (select nextval('s') as n) select a.n, b.n from r a, r b";
    assert_eq!(rows(&sql(&db, q)), ["1|1"]);
    assert_eq!(rows(&sql(&db, "select nextval('s')")), ["2"], "computed once");
    assert_eq!(rows(&sql(&db, "with r as (select random() as x) select a.x = b.x from r a, r b")), ["true"]);
    // in FROM and in expression subqueries alike
    let q = "with c as (select nextval('s') as n) select (select n from c), (select n from c), n from c";
    assert_eq!(rows(&sql(&db, q)), ["3|3|3"]);
    // in a subquery run for each outer row, correlated or LATERAL: right for each
    let q =
        "select id, (with c as (select id as k from t) select count(*) from c where c.k <= t.id) from t order by id";
    assert_eq!(rows(&sql(&db, q)), ["1|1", "2|2", "3|3"]);
    let q = "select t.id, x.n from t, lateral (with c as (select id from t) select count(*) as n from c where c.id <= t.id) x order by t.id";
    assert_eq!(rows(&sql(&db, q)), ["1|1", "2|2", "3|3"]);
    // a function's loop runs its statement again: fresh rows each time
    sql(
        &db,
        "create function grow() returns int language plpgsql as $$
         declare n int; total int := 0;
         begin
           for i in 1..3 loop
             insert into t values (100 + i);
             with c as (select count(*) as k from t) select k into n from c;
             total := total + n;
           end loop;
           return total;
         end $$",
    );
    assert_eq!(rows(&sql(&db, "select grow()")), ["15"]); // 4 + 5 + 6
}

/// Without a column list, an INSERT's values (or SELECT's columns) fill the first columns and the
/// rest take their defaults, serials included, as in Postgres; with a list the counts must match.
#[test]
fn insert_fills_the_rest_with_defaults() {
    let db = Db::new();
    sql(&db, "create table t (id int primary key, name text default 'anon', n serial, at int)");
    sql(&db, "insert into t values (1)");
    sql(&db, "insert into t select 2, 'two'");
    sql(&db, "insert into t select i from generate_series(3, 4) i");
    assert_eq!(
        rows(&sql(&db, "select id, name, n, at from t order by id")),
        ["1|anon|1|null", "2|two|2|null", "3|anon|3|null", "4|anon|4|null"]
    );
    assert_eq!(fails(&db, "insert into t (id, name) values (5)").0, "42601");
    assert_eq!(fails(&db, "insert into t (id, name) select 5").0, "42601");
    assert_eq!(fails(&db, "insert into t values (5, 'a', 5, 5, 5)").0, "42601");
}

/// Postgres works a column's DEFAULT out afresh for each row. Chronos does too: serial,
/// gen_random_uuid(), now() and current_date on their own, and any other default that would change
/// from row to row (random(), nextval('q') + 100, a function), which it keeps as its SQL and runs
/// for each row that takes it. A default that can't change is kept as one value.
#[test]
fn defaults_worked_out_for_each_row() {
    let db = Db::new();
    sql(&db, "create sequence q");
    sql(&db, "create function pick() returns int language sql as 'select 7'");
    sql(
        &db,
        "create table t (id int primary key, r float default random(), k int default nextval('q') + 100, \
         due timestamptz default now() + interval '7 days', d date default current_date + 1, \
         code text default md5(random()::text), p int default pick() * 2, secs int default 60 * 60, \
         tag text default 'a' || 'b')",
    );
    // (making the table took nothing from the sequence: the first row's is its first value)
    sql(&db, "insert into t (id) values (1), (2), (3)");
    assert_eq!(
        rows(&sql(
            &db,
            "select k, p, secs, tag, due > now() + interval '6 days', d = current_date + 1 from t order by id"
        )),
        ["101|14|3600|ab|true|true", "102|14|3600|ab|true|true", "103|14|3600|ab|true|true"]
    );
    assert_eq!(rows(&sql(&db, "select count(distinct r), count(distinct code) from t")), ["3|3"]);
    // a value given stands; DEFAULT in a SET is worked out then (each statement here is a session of
    // its own, and a session's unused reserved values go with it: later, not necessarily next)
    sql(&db, "insert into t (id, k) values (4, 9)");
    let k4: i64 = rows(&sql(&db, "update t set k = default where id = 4 returning k"))[0].parse().unwrap();
    assert!(k4 > 103, "{k4}");

    // still refused: a subquery, and a column of the row
    assert_eq!(fails(&db, "create table bad (id int, n int default (select 1))").0, "0A000");
    let (code, message) = fails(&db, "create table bad (id int, n int default id + 1)");
    assert_eq!(code, "0A000", "{message}");
    assert_eq!(message, "cannot use column reference in DEFAULT expression");

    // ALTER TABLE: SET DEFAULT takes one too. ADD COLUMN on a table with rows takes the clock's (the
    // rows there get what it gives now), and refuses one that would give each of them its own
    sql(&db, "alter table t alter column secs set default (pick() * 60)");
    sql(&db, "alter table t add column seen timestamptz default now() - interval '1 hour'");
    let (code, message) = fails(&db, "alter table t add column r2 float default random()");
    assert_eq!(code, "0A000", "{message}");
    assert!(message.contains("DEFAULT random()"), "{message}");
    sql(&db, "insert into t (id) values (5)");
    assert_eq!(rows(&sql(&db, &format!("select secs, k > {k4} from t where id = 5"))), ["420|true"]);
    assert_eq!(rows(&sql(&db, "select count(*) from t where seen < now()")), ["5"]);

    // pg_dump's form of a serial: the table first, its sequence default set after
    sql(&db, "create sequence u_id_seq");
    sql(&db, "create table u (id int not null, name text)");
    sql(&db, "alter table u alter column id set default nextval('u_id_seq'::regclass)");
    sql(&db, "insert into u (name) values ('a'), ('b')");
    assert_eq!(rows(&sql(&db, "select id from u order by name")), ["1", "2"]);
}

#[test]
fn constraints_and_errors() {
    let db = people();
    assert_eq!(fails(&db, "insert into people (id, name) values (1, 'Again')").0, "23505");
    assert_eq!(fails(&db, "insert into people (id, name) values (7, 'x'), (7, 'y')").0, "23505");
    assert_eq!(fails(&db, "update people set id = 2 where id = 1").0, "23505");
    assert_eq!(fails(&db, "insert into people (id) values (8)").0, "23502");
    assert_eq!(fails(&db, "update people set name = null").0, "23502");
    assert_eq!(fails(&db, "insert into people (id, name, age) values (8, 'x', 'old')").0, "22P02");
    assert_eq!(fails(&db, "insert into people (id, name, age) values (8, 'x', 3000000000)").0, "22003");
    assert_eq!(fails(&db, "select * from nobody").0, "42P01");
    assert_eq!(fails(&db, "select nope from people").0, "42703");
    assert_eq!(fails(&db, "select * from people where").0, "42601");
    assert_eq!(fails(&db, "create table people (id int)").0, "42P07");
    assert_eq!(fails(&db, "select 1 / 0").0, "22012");
    assert_eq!(fails(&db, "select no_such_function('x')").0, "42883", "undefined_function, as Postgres says");
    // a failed statement writes nothing
    assert_eq!(rows(&sql(&db, "select name from people where id = 7")), Vec::<String>::new());
    assert_eq!(sql(&db, "create table if not exists people (id int)").tag, "CREATE TABLE");
    sql(&db, "drop table people");
    assert_eq!(fails(&db, "select * from people").0, "42P01");
    assert!(db.prefix(MAIN, "people/").unwrap().is_empty());
    sql(&db, "drop table if exists people");
}

#[test]
fn tables_without_a_schema() {
    let db = Db::new();
    db.put(MAIN, "notes/a", r#"{"title": "hi", "stars": 3}"#).unwrap();
    db.put(MAIN, "notes/b", r#"{"title": "yo", "tags": ["x"], "stars": 4.5}"#).unwrap();
    let o = sql(&db, "select * from notes order by stars desc");
    let cols: Vec<_> = o.columns.clone().unwrap().into_iter().map(|(n, t)| format!("{n}:{}", t.name())).collect();
    assert_eq!(cols, ["id:text", "stars:double precision", "tags:jsonb", "title:text"]);
    assert_eq!(rows(&o), [r#"b|4.5|["x"]|yo"#, "a|3|null|hi"]);
    sql(&db, "insert into notes (id, title) values ('c', 'new')");
    sql(&db, "update notes set stars = 5, title = null where id = 'c'");
    assert_eq!(&*db.get(MAIN, "notes/c").unwrap().unwrap(), r#"{"stars":5}"#);
    assert_eq!(fails(&db, "insert into notes (title) values ('no id')").0, "23502");
}

#[test]
fn sql_on_branches() {
    let db = people();
    db.fork(MAIN, "agent").unwrap();
    run(
        &db,
        "agent",
        "update people set age = 99 where id = 1; insert into people (id, name) values (6, 'Katherine')",
        &[],
    )
    .unwrap();
    assert_eq!(rows(&sql(&db, "select age from people where id = 1")), ["36"]); // main untouched
    let on_branch = run(&db, "agent", "select name from people where age = 99 or id = 6", &[]).unwrap();
    assert_eq!(rows(&on_branch[0]), ["Ada", "Katherine"]);
    // a table created on a branch arrives with the merge
    run(&db, "agent", "create table todo (id text primary key, done bool)", &[]).unwrap();
    db.merge("agent", Resolve::Fail).unwrap();
    assert_eq!(rows(&sql(&db, "select name from people where age = 99 or id = 6")), ["Ada", "Katherine"]);
    assert_eq!(sql(&db, "select * from todo").tag, "SELECT 0");
}

/// Indexed WHERE (`col = value`) must answer exactly like a full scan, on main and on a branch,
/// through inserts, updates, key changes and deletes. `(expr) = true` hides the equality from the
/// planner, forcing a scan.
#[test]
fn indexed_lookups_match_a_full_scan() {
    let db = Db::new();
    sql(&db, "create table t (id int primary key, a int, b text, c boolean, d double precision)");
    let mut seed = 7u64;
    let mut rnd = |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    for i in 0..300 {
        let q = format!("insert into t values ({i}, {}, 'w{}', {}, {})", rnd(10), rnd(5), rnd(2) == 0, rnd(4));
        sql(&db, &q);
    }
    db.fork(MAIN, "f").unwrap();
    for step in 0..400 {
        let b = if step % 2 == 0 { MAIN } else { "f" };
        let q = match rnd(4) {
            0 => format!("update t set a = {}, b = 'w{}' where id = {}", rnd(10), rnd(5), rnd(400)),
            1 => format!("delete from t where id = {}", rnd(400)),
            2 => format!("update t set id = {} where id = {}", 400 + step, rnd(400)),
            _ => format!("insert into t (id, a, c) values ({}, {}, true)", 1000 + step, rnd(10)),
        };
        let _ = run(&db, b, &q, &[]); // duplicate keys may fail: fine
        if step % 20 == 0 {
            for (b, cond) in [(MAIN, format!("a = {}", rnd(10))), ("f", format!("b = 'w{}' and c", rnd(5)))] {
                let fast = run(&db, b, &format!("select * from t where {cond}"), &[]).unwrap();
                let slow = run(&db, b, &format!("select * from t where ({cond}) = true"), &[]).unwrap();
                assert_eq!(rows(&fast[0]), rows(&slow[0]), "{b}: {cond}");
            }
            let with_param =
                run(&db, "f", "select id from t where a = $1 and c = $2", &[Value::Int(3), Value::Bool(true)]);
            let slow = run(&db, "f", "select id from t where (a = 3 and c) = true", &[]);
            assert_eq!(rows(&with_param.unwrap()[0]), rows(&slow.unwrap()[0]));
        }
    }
    assert!(rows(&sql(&db, "select id from t where a = null")).is_empty());
}

#[test]
fn branches_in_sql() {
    let db = people();
    let mut s = chronos::sql::Session::new(MAIN);
    let q = |s: &mut chronos::sql::Session, sql: &str| s.run(&db, sql, &[]).map(|mut o| o.pop().unwrap());
    q(&mut s, "create branch agent_7").unwrap();
    q(&mut s, "create branch 'agent-8' from main").unwrap();
    assert_eq!(q(&mut s, "create branch agent_7").unwrap_err().code, "42710");
    q(&mut s, "use branch agent_7").unwrap();
    assert_eq!(s.branch, "agent_7");
    q(&mut s, "update people set age = 37 where id = 1; delete from people where id = 4").unwrap();
    let d = q(&mut s, "diff").unwrap();
    assert_eq!(
        rows(&d),
        [
            // the delete first
            r#"people|4|delete|{"id": 4, "name": "Linus", "admin": false, "email": "l@kernel.org"}|null|[]"#,
            r#"people|1|update|{"id": 1, "age": 36, "name": "Ada", "admin": false, "email": "ada@example.com"}|{"id": 1, "age": 37, "name": "Ada", "admin": false, "email": "ada@example.com"}|["age"]"#,
        ]
    );
    let b = q(&mut s, "show branches").unwrap();
    assert_eq!(rows(&b), ["agent-8|main|0", "agent_7|main|2", "main|null|null"]);

    // a conflicting change on main: nothing merges until a side is chosen
    run(&db, MAIN, "update people set age = 40 where id = 1", &[]).unwrap();
    let e = q(&mut s, "merge branch").unwrap_err();
    assert_eq!(e.code, "40001");
    assert!(e.message.contains("people/1") && e.message.contains("USING OURS"), "{}", e.message);
    assert_eq!(q(&mut s, "merge branch agent_7 using ours").unwrap().tag, "MERGE 2");
    assert_eq!(s.branch, "main", "merging the branch you're on moves you to its parent");
    assert_eq!(rows(&q(&mut s, "select age from people where id = 1").unwrap()), ["37"]);
    q(&mut s, "drop branch \"agent-8\"").unwrap();
    q(&mut s, "drop branch if exists nope").unwrap();
    assert_eq!(q(&mut s, "use branch nope").unwrap_err().code, "3D000");
}

/// A session's transaction world (`_tx_*`) is its own: no other session runs a statement on it
/// (one made on it, as an API request naming it as its branch would make, or one asked to USE it),
/// at a moment of it either, and its rows stay unseen and unchanged.
#[test]
fn a_transactions_world_is_no_other_sessions() {
    let db = people();
    let mut a = chronos::sql::Session::new(MAIN);
    a.run(&db, "begin; update people set age = 99 where id = 1", &[]).unwrap();
    let tx: Vec<String> = db.branches().into_iter().map(|(b, _)| b).filter(|b| b.starts_with("_tx_")).collect();
    assert_eq!(tx.len(), 1);
    for world in [tx[0].clone(), format!("{}@-1 hour", tx[0])] {
        let mut other = chronos::sql::Session::new(&world);
        for q in ["select age from people where id = 1", "update people set age = 1 where id = 1"] {
            // (a moment before the world was made is refused even before that: 22023)
            let code = other.run(&db, q, &[]).unwrap_err().code;
            assert!(code == "3D000" || (world.contains('@') && code == "22023"), "{world}: {q}: {code}");
        }
    }
    let mut other = chronos::sql::Session::new(MAIN);
    assert_eq!(other.run(&db, &format!("use branch \"{}\"", tx[0]), &[]).unwrap_err().code, "3D000");
    assert_eq!(other.branch, MAIN);
    a.run(&db, "commit", &[]).unwrap();
    assert_eq!(rows(&sql(&db, "select age from people where id = 1")), ["99"]);
}

#[test]
fn transactions() {
    let db = people();
    let (mut a, mut b) = (chronos::sql::Session::new(MAIN), chronos::sql::Session::new(MAIN));
    let q = |s: &mut chronos::sql::Session, sql: &str| s.run(&db, sql, &[]).map(|mut o| o.pop().unwrap());

    q(&mut a, "begin").unwrap();
    assert_eq!(a.status(), b'T');
    q(&mut a, "insert into people (id, name) values (7, 'Tim')").unwrap();
    assert_eq!(rows(&q(&mut a, "select name from people where id = 7").unwrap()), ["Tim"], "sees its own writes");
    assert!(rows(&q(&mut b, "select name from people where id = 7").unwrap()).is_empty(), "others don't, yet");
    assert_eq!(q(&mut a, "commit").unwrap().tag, "COMMIT");
    assert_eq!(rows(&q(&mut b, "select name from people where id = 7").unwrap()), ["Tim"]);

    q(&mut a, "begin; delete from people").unwrap();
    assert_eq!(q(&mut a, "rollback").unwrap().tag, "ROLLBACK");
    assert_eq!(q(&mut b, "select id from people").unwrap().rows.len(), 5);

    // both change Ada, one from a snapshot taken before the other committed (REPEATABLE READ): its write is refused
    // as Postgres refuses it, and the transaction can only be rolled back
    q(&mut b, "begin isolation level repeatable read; select 1").unwrap();
    q(&mut a, "begin; update people set age = age + 1 where id = 1").unwrap();
    q(&mut a, "commit").unwrap();
    let e = q(&mut b, "update people set age = age + 10 where id = 1").unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert_eq!(b.status(), b'E');
    assert_eq!(q(&mut b, "commit").unwrap().tag, "ROLLBACK");
    assert_eq!(b.status(), b'I');
    assert_eq!(rows(&q(&mut b, "select age from people where id = 1").unwrap()), ["37"]);
    // (and at READ COMMITTED the second reads the row as the first left it, and goes on)
    q(&mut b, "begin; select 1").unwrap();
    q(&mut a, "update people set age = age + 1 where id = 1").unwrap();
    q(&mut b, "update people set age = age + 10 where id = 1").unwrap();
    q(&mut b, "commit").unwrap();
    assert_eq!(rows(&q(&mut a, "select age from people where id = 1").unwrap()), ["48"]);
    q(&mut a, "update people set age = 37 where id = 1").unwrap();

    // after an error only ROLLBACK (or COMMIT, which rolls back) is accepted
    q(&mut a, "begin; insert into people (id, name) values (8, 'x')").unwrap();
    assert_eq!(q(&mut a, "insert into people (id, name) values (1, 'dup')").unwrap_err().code, "23505");
    assert_eq!(a.status(), b'E');
    assert_eq!(q(&mut a, "select 1").unwrap_err().code, "25P02");
    assert_eq!(q(&mut a, "commit").unwrap().tag, "ROLLBACK");
    assert!(rows(&q(&mut a, "select id from people where id = 8").unwrap()).is_empty());
    q(&mut a, "begin").unwrap();
    assert_eq!(q(&mut a, "create branch x").unwrap_err().code, "25001");
    q(&mut a, "rollback").unwrap();

    // a transaction left open at the end of a call is rolled back; stray ones are cleaned up
    run(&db, MAIN, "begin; insert into people (id, name) values (9, 'y')", &[]).unwrap();
    assert!(rows(&sql(&db, "select id from people where id = 9")).is_empty());
    q(&mut a, "begin").unwrap();
    assert!(db.branches().iter().any(|(n, _)| n.starts_with("_tx_")));
    chronos::sql::cleanup(&db);
    assert!(db.branches().iter().all(|(n, _)| !n.starts_with("_tx_")));
}

/// Concurrent transfers in transactions, retried on deadlocks: money is conserved.
#[test]
fn concurrent_transfers_conserve_money() {
    let db = Db::new();
    sql(&db, "create table acct (id int primary key, balance bigint not null)");
    for i in 0..10 {
        sql(&db, &format!("insert into acct values ({i}, 100)"));
    }
    let retries = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for t in 0..8u64 {
            let (db, retries) = (&db, &retries);
            sc.spawn(move || {
                let mut s = chronos::sql::Session::new(MAIN);
                for n in 0..50u64 {
                    let (from, to) = ((t * 7 + n) % 10, (t * 3 + n * 5 + 1) % 10);
                    if from == to {
                        continue;
                    }
                    loop {
                        // (a deadlock, which two transfers in opposite directions can make, is retried too)
                        let r = s.run(db, "begin", &[]).and_then(|_| {
                            let bal =
                                s.run(db, "select balance from acct where id = $1", &[Value::Int(from as i64)])?;
                            if bal[0].rows[0][0] == Value::Int(0) {
                                return Ok(());
                            }
                            s.run(
                                db,
                                "update acct set balance = balance - 1 where id = $1",
                                &[Value::Int(from as i64)],
                            )?;
                            s.run(db, "update acct set balance = balance + 1 where id = $1", &[Value::Int(to as i64)])?;
                            Ok(())
                        });
                        let done = r.and_then(|_| s.run(db, "commit", &[]));
                        match done {
                            Ok(_) => break,
                            Err(e) if e.code == "40001" || e.code == "40P01" => {
                                retries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                let _ = s.run(db, "rollback", &[]);
                            }
                            Err(e) => panic!("{e}"),
                        }
                    }
                }
            });
        }
    });
    let total: i64 =
        sql(&db, "select balance from acct").rows.iter().map(|r| if let Value::Int(n) = r[0] { n } else { 0 }).sum();
    assert_eq!(total, 1000);
    // (updates wait for each other at READ COMMITTED: only a deadlock is retried)
    let _ = retries.into_inner();
    assert!(db.branches().iter().all(|(n, _)| !n.starts_with("_tx_")));
}

fn shop() -> Db {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table customers (id int primary key, name text not null, city text);
         create table orders (id int primary key, customer int, total double precision, status text);
         insert into customers values (1, 'Ada', 'London'), (2, 'Alan', 'London'), (3, 'Grace', 'NYC'), (4, 'Linus', null);
         insert into orders values (10, 1, 20.5, 'paid'), (11, 1, 5, 'paid'), (12, 2, 100, 'open'), (13, 3, 7.25, 'paid'), (14, 9, 1, 'paid');",
        &[],
    )
    .unwrap();
    db
}

#[test]
fn joins() {
    let db = shop();
    let q = |sql: &str| rows(&sql_(&db, sql));
    assert_eq!(
        q(
            "select c.name, o.total from orders o join customers c on c.id = o.customer where o.status = 'paid' order by o.id"
        ),
        ["Ada|20.5", "Ada|5", "Grace|7.25"]
    );
    // LEFT JOIN keeps customers without orders; ON parts on the right table don't drop left rows
    assert_eq!(
        q(
            "select c.name, o.id from customers c left join orders o on o.customer = c.id and o.status = 'paid' order by c.id, o.id"
        ),
        ["Ada|10", "Ada|11", "Alan|null", "Grace|13", "Linus|null"]
    );
    assert_eq!(
        q("select c.name from customers c left join orders o on o.customer = c.id where o.id is null"),
        ["Linus"]
    );
    assert_eq!(q("select count(*) from customers, orders"), ["20"]);
    assert_eq!(q("select count(*) from customers cross join orders where customers.id = orders.customer"), ["4"]);
    // join on a non-key column, and a condition that isn't an equality
    assert_eq!(
        q("select a.name, b.name from customers a join customers b on a.city = b.city and a.id < b.id"),
        ["Ada|Alan"]
    );
    assert_eq!(q("select o.id from orders o join customers c on o.total > c.id * 30 where c.id = 3"), ["12"]);
    let cols = sql_(&db, "select o.*, c.name from orders o join customers c on c.id = o.customer where o.id = 12")
        .columns
        .unwrap();
    assert_eq!(cols.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["id", "customer", "total", "status", "name"]);
    assert_eq!(fails(&db, "select id from orders o join customers c on c.id = o.customer").0, "42702");
    assert_eq!(fails(&db, "select x.id from orders o").0, "42P01");
    assert_eq!(fails(&db, "select 1 from orders join orders on true").0, "42712");
}

#[test]
fn grouping_and_aggregates() {
    let db = shop();
    let q = |sql: &str| rows(&sql_(&db, sql));
    assert_eq!(q("select count(*), count(city), count(distinct city) from customers"), ["4|3|2"]);
    assert_eq!(
        q("select sum(total), min(total), max(total), avg(total) from orders where status = 'paid'"),
        ["33.75|1|20.5|8.4375"]
    );
    assert_eq!(q("select count(*), sum(total) from orders where id > 100"), ["0|null"]);
    assert_eq!(
        q("select status, count(*) n, sum(total) from orders group by status order by n desc"),
        ["paid|4|33.75", "open|1|100"]
    );
    assert_eq!(
        q(
            "select c.city, count(o.id) from customers c left join orders o on o.customer = c.id group by c.city order by 1"
        ),
        ["London|3", "NYC|1", "null|0"]
    );
    assert_eq!(q("select customer from orders group by customer having sum(total) > 10 order by customer"), ["1", "2"]);
    assert_eq!(q("select lower(status) s, count(*) from orders group by s order by s"), ["open|1", "paid|4"]);
    assert_eq!(q("select distinct status from orders order by status"), ["open", "paid"]);
    assert_eq!(q("select count(*) from orders group by customer order by count(*) desc limit 1"), ["2"]);
    assert_eq!(
        q(
            "select upper(name), length(name), coalesce(city, '?'), abs(-3), round(2.567, 2) from customers where id = 4"
        ),
        ["LINUS|5|?|3|2.57"]
    );
    let e = fails(&db, "select status, total from orders group by status");
    assert_eq!(e.0, "42803", "{}", e.1);
    assert_eq!(fails(&db, "select * from orders where count(*) > 1").0, "42803");
    assert_eq!(fails(&db, "select max(count(*)) from orders").0, "42803");
    let types: Vec<_> = sql_(&db, "select count(*), sum(id), sum(total), avg(id), min(status) from orders")
        .columns
        .unwrap()
        .into_iter()
        .map(|c| c.1.name())
        .collect();
    assert_eq!(types, ["bigint", "bigint", "double precision", "numeric", "text"]);
}

fn sql_(db: &Db, q: &str) -> Output {
    sql(db, q)
}

/// Joins and GROUP BY against the same answers worked out in plain Rust, on random data, with
/// a fork's changes mixed in.
#[test]
fn joins_and_groups_match_a_reference() {
    let db = Db::new();
    sql(&db, "create table a (id int primary key, k int, x int)");
    sql(&db, "create table b (id int primary key, k int, y int)");
    let mut seed = 11u64;
    let mut rnd = |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let (mut av, mut bv) = (vec![], vec![]);
    for i in 0..60i64 {
        let (k, x) = (rnd(8) as i64, rnd(100) as i64);
        let k = if rnd(10) == 0 { None } else { Some(k) };
        sql(&db, &format!("insert into a values ({i}, {}, {x})", k.map_or("null".into(), |k| k.to_string())));
        av.push((i, k, x));
        let (k2, y) = (rnd(8) as i64, rnd(100) as i64);
        sql(&db, &format!("insert into b values ({i}, {k2}, {y})"));
        bv.push((i, Some(k2), y));
    }
    // inner join a.k = b.k, grouped by a.k: count and sum(y)
    let mut want: std::collections::BTreeMap<i64, (i64, i64)> = Default::default();
    for (_, ak, _) in &av {
        for (_, bk, y) in &bv {
            if let (Some(ak), Some(bk)) = (ak, bk)
                && ak == bk
            {
                let e = want.entry(*ak).or_default();
                e.0 += 1;
                e.1 += y;
            }
        }
    }
    let want: Vec<String> = want.iter().map(|(k, (n, s))| format!("{k}|{n}|{s}")).collect();
    let got = rows(&sql(&db, "select a.k, count(*), sum(b.y) from a join b on a.k = b.k group by a.k order by a.k"));
    assert_eq!(got, want);
    // left join on the key: every a row, matched to b's row with the same id when x < 50
    let mut want: Vec<String> = av
        .iter()
        .map(|(id, _, x)| {
            let y = if *x < 50 { bv.iter().find(|b| b.0 == *id).map(|b| b.2.to_string()) } else { None };
            format!("{id}|{}", y.unwrap_or("null".into()))
        })
        .collect();
    want.sort();
    let mut got = rows(&sql(&db, "select a.id, b.y from a left join b on b.id = a.id and a.x < 50"));
    got.sort();
    assert_eq!(got, want);
    // the same on a fork, after changes there
    db.fork(MAIN, "f").unwrap();
    run(&db, "f", "update b set k = 99 where id < 30; delete from a where id >= 50", &[]).unwrap();
    let on_fork = run(&db, "f", "select count(*) from a join b on a.k = b.k", &[]).unwrap();
    let expect = av
        .iter()
        .filter(|a| a.0 < 50)
        .flat_map(|a| bv.iter().map(move |b| (a, b)))
        .filter(|(a, b)| a.1.is_some() && a.1 == if b.0 < 30 { Some(99) } else { b.1 })
        .count();
    assert_eq!(rows(&on_fork[0]), [expect.to_string()]);
}

/// SQL tables store rows packed; everything outside SQL still sees JSON, old JSON rows still
/// read, and it all survives reopening.
#[test]
fn packed_rows_look_like_json_outside_sql() {
    let dir = std::env::temp_dir().join(format!("chronos-packed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table t (id int primary key, name text, score double precision, tags jsonb, ok boolean)");
        sql(&db, r#"insert into t values (1, 'Ada', 9.5, '["x"]', true), (2, 'Al:an', null, null, false)"#);
        let raw = db.view(MAIN).unwrap().get(&chronos::sql::row_key_of(&db, MAIN, "t/1")).unwrap().unwrap();
        assert!(raw.starts_with('\u{1}'), "stored packed: {raw:?}");
        assert_eq!(
            &*db.get(MAIN, "t/1").unwrap().unwrap(),
            r#"{"id":1,"name":"Ada","ok":true,"score":9.5,"tags":["x"]}"#
        );
        // a row written as JSON through the JSON API reads alongside packed ones
        db.put(MAIN, "t/3", r#"{"id": 3, "name": "Grace", "score": 1}"#).unwrap();
        assert_eq!(
            rows(&sql(&db, "select id, name, score from t order by id")),
            ["1|Ada|9.5", "2|Al:an|null", "3|Grace|1"]
        );
        // search, diff and conflicts show JSON
        let (_, hits) = db
            .find(
                MAIN,
                &chronos::Query {
                    table: "t".into(),
                    filters: vec![("name".into(), "Al:an".into())],
                    k: 5,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(&*hits[0].row, r#"{"id":2,"name":"Al:an","ok":false}"#);
        db.fork(MAIN, "f").unwrap();
        run(&db, "f", "update t set name = 'Ada L.' where id = 1", &[]).unwrap();
        let d = db.diff("f").unwrap();
        assert_eq!(d[0].after.as_deref(), Some(r#"{"id":1,"name":"Ada L.","ok":true,"score":9.5,"tags":["x"]}"#));
        sql(&db, "update t set name = 'Ada M.' where id = 1");
        let Err(chronos::Error::Conflicts(c)) = db.merge("f", Resolve::Fail) else { panic!("expected a conflict") };
        assert!(c[0].theirs.as_deref().unwrap().contains(r#""name":"Ada M.""#), "{:?}", c[0]);
        db.discard("f").unwrap();
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(rows(&sql(&db, "select name from t where id = 2")), ["Al:an"]);
    assert_eq!(rows(&sql(&db, "select count(*), sum(score) from t")), ["3|10.5"]);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Reordered joins (start from the narrowest table, key and index lookups) must answer exactly
/// like the same query with its conditions hidden from the planner (`(...) = true`), which runs
/// every combination in the written order.
#[test]
fn join_order_does_not_change_answers() {
    let db = Db::new();
    sql(&db, "create table a (id int primary key, k int, name text)");
    sql(&db, "create table b (id int primary key, a_id int, y int)");
    sql(&db, "create table c (id int primary key, b_id int, z int, tag text)");
    let mut seed = 5u64;
    let mut rnd = |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    for i in 0..20 {
        sql(&db, &format!("insert into a values ({i}, {}, 'n{}')", rnd(5), rnd(3)));
    }
    for i in 0..40 {
        sql(&db, &format!("insert into b values ({i}, {}, {})", rnd(24), rnd(6)));
    }
    for i in 0..60 {
        sql(&db, &format!("insert into c values ({i}, {}, {}, 't{}')", rnd(44), rnd(8), rnd(3)));
    }
    db.fork(MAIN, "f").unwrap();
    run(&db, "f", "update c set z = 3 where id < 20; delete from b where id % 7 = 0", &[]).unwrap();
    // (planned, the same with every condition hidden)
    let pairs = [
        (
            "select a.id, b.id, c.id from a join b on b.a_id = a.id join c on c.b_id = b.id where c.z = 3",
            "select a.id, b.id, c.id from a, b, c where (b.a_id = a.id) = true and (c.b_id = b.id) = true and (c.z = 3) = true",
        ),
        (
            "select a.id, c.id from a join b on b.a_id = a.id join c on c.b_id = b.id where c.z = 3 and c.tag = 't1'",
            "select a.id, c.id from a, b, c where (b.a_id = a.id) = true and (c.b_id = b.id) = true and (c.z = 3 and c.tag = 't1') = true",
        ),
        (
            "select a.id, c.id from a join b on b.a_id = a.id join c on c.b_id = b.id where a.k = 2",
            "select a.id, c.id from a, b, c where (b.a_id = a.id) = true and (c.b_id = b.id) = true and (a.k = 2) = true",
        ),
        (
            "select a.id, b.id from a, b where a.k = b.y and b.y = 1",
            "select a.id, b.id from a, b where (a.k = b.y) = true and (b.y = 1) = true",
        ),
        (
            "select c.id, b.id from c join b on b.id = c.b_id where b.y = 4 and c.z > 2",
            "select c.id, b.id from c, b where (b.id = c.b_id) = true and (b.y = 4 and c.z > 2) = true",
        ),
        (
            "select a.name, count(*) from a join b on b.a_id = a.id join c on c.b_id = b.id where c.tag = 't2' group by a.name",
            "select a.name, count(*) from a, b, c where (b.a_id = a.id and c.b_id = b.id and c.tag = 't2') = true group by a.name",
        ),
    ];
    for branch in [MAIN, "f"] {
        for (fast, slow) in pairs {
            let mut a = rows(&run(&db, branch, fast, &[]).unwrap_or_else(|e| panic!("{fast}: {e}"))[0]);
            let mut b = rows(&run(&db, branch, slow, &[]).unwrap_or_else(|e| panic!("{slow}: {e}"))[0]);
            a.sort();
            b.sort();
            assert_eq!(a, b, "{branch}: {fast}");
            assert!(!a.is_empty(), "{branch}: {fast} should match something");
        }
    }
}

/// SERIAL / identity columns number rows from one sequence shared by every branch, so rows
/// inserted on two branches never collide at merge; UUID defaults are fresh per row.
#[test]
fn auto_numbered_ids() {
    let dir = std::env::temp_dir().join(format!("chronos-serial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    sql(&db, "create table todo (id serial primary key, title text not null, uid uuid default gen_random_uuid())");
    sql(&db, "create table log (n bigint generated always as identity, msg text)");
    let o = sql(&db, "insert into todo (title) values ('a'), ('b') returning id, uid");
    assert_eq!(o.rows.iter().map(|r| r[0].to_text()).collect::<Vec<_>>(), ["1", "2"]);
    let (u1, u2) = (o.rows[0][1].to_text(), o.rows[1][1].to_text());
    assert!(u1 != u2 && u1.len() == 36 && u1.as_bytes()[14] == b'4', "{u1} {u2}");
    assert_eq!(rows(&sql(&db, "insert into todo values (default, 'c', null) returning id, uid")), ["3|null"]);
    sql(&db, "insert into todo (id, title) values (100, 'explicit')");
    sql(&db, "insert into log (msg) values ('x'), ('y')");
    assert_eq!(rows(&sql(&db, "select n from log order by n")), ["1", "2"]);

    // two branches insert at once and merge cleanly: the numbers never overlap
    db.fork(MAIN, "f").unwrap();
    let on_f = rows(&run(&db, "f", "insert into todo (title) values ('on f') returning id", &[]).unwrap()[0]);
    let on_main = rows(&sql(&db, "insert into todo (title) values ('on main') returning id"));
    assert_ne!(on_f, on_main);
    db.merge("f", Resolve::Fail).unwrap();
    assert_eq!(rows(&sql(&db, "select count(*) from todo")), ["6"]);
    // like Postgres sequences, a rollback doesn't give numbers back
    run(&db, MAIN, "begin; insert into todo (title) values ('gone'); rollback", &[]).unwrap();
    let next = rows(&sql(&db, "insert into todo (title) values ('after') returning id"));
    assert_eq!(next, ["7"]);

    // after reopening, numbering continues past everything handed out before
    drop(db);
    let db = Db::open(&dir).unwrap();
    let after: i64 = rows(&sql(&db, "insert into todo (title) values ('reopened') returning id"))[0].parse().unwrap();
    assert!(after > 7, "{after}");
    assert_eq!(fails(&db, "create table bad (id text generated always as identity)").0, "22023");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn alter_table() {
    let db = Db::new();
    sql(&db, "create table p (id serial primary key, name text, age int)");
    sql(&db, "insert into p (name, age) values ('Ada', 36), ('Alan', null)");

    // adding a column: existing rows get its default, new rows whatever they're given
    sql(&db, "alter table p add column active boolean not null default true, add score double precision");
    sql(&db, "insert into p (name, active) values ('Grace', false)");
    assert_eq!(
        rows(&sql(&db, "select name, active, score from p order by id")),
        ["Ada|true|null", "Alan|true|null", "Grace|false|null"]
    );
    assert_eq!(fails(&db, "alter table p add email text not null").0, "23502");
    assert_eq!(fails(&db, "alter table p add name text").0, "42701");
    sql(&db, "alter table p add column if not exists name text");

    // renaming keeps the data; the JSON everyone else sees follows
    sql(&db, "alter table p rename column age to years");
    assert_eq!(rows(&sql(&db, "select years from p where id = 1")), ["36"]);
    assert_eq!(fails(&db, "select age from p").0, "42703");
    assert_eq!(&*db.get(MAIN, "p/1").unwrap().unwrap(), r#"{"active":true,"id":1,"name":"Ada","years":36}"#);

    // dropping a column: gone from old rows too, and a new column of the same name starts empty
    sql(&db, "alter table p drop column score");
    sql(&db, "alter table p drop column years");
    sql(&db, "alter table p add column years int");
    assert_eq!(rows(&sql(&db, "select * from p where id = 1")), ["1|Ada|true|null"]);
    assert_eq!(fails(&db, "alter table p drop column id").0, "2BP01");
    sql(&db, "alter table p drop column if exists nope");

    // changing a type converts every value, or refuses
    sql(&db, "update p set years = 40 where id = 1");
    sql(&db, "alter table p alter column years type text");
    assert_eq!(rows(&sql(&db, "select years || '!' from p where id = 1")), ["40!"]);
    sql(&db, "update p set years = 'many' where id = 2");
    assert_eq!(fails(&db, "alter table p alter column years type bigint").0, "22P02");
    sql(&db, "update p set years = '7' where id = 2");
    sql(&db, "alter table p alter years set data type bigint");
    assert_eq!(rows(&sql(&db, "select years + 1 from p order by id")), ["41", "8", "null"]);

    // defaults and NOT NULL
    assert_eq!(fails(&db, "alter table p alter column years set not null").0, "23502");
    sql(&db, "alter table p alter column years set default 1");
    sql(&db, "insert into p (name) values ('Tim')");
    assert_eq!(rows(&sql(&db, "select years from p where name = 'Tim'")), ["1"]);
    sql(&db, "update p set years = 0 where years is null; alter table p alter column years set not null");
    assert_eq!(fails(&db, "insert into p (name, years) values ('x', null)").0, "23502");
    sql(&db, "alter table p alter column years drop not null, alter column years drop default");

    // renaming the table moves its rows, and numbering carries on
    sql(&db, "alter table p rename to people");
    assert_eq!(fails(&db, "select * from p").0, "42P01");
    // 5 went to the insert that failed NOT NULL: like Postgres, a failed insert still uses a number
    let next = rows(&sql(&db, "insert into people (name) values ('Lin') returning id"));
    assert_eq!(next, ["6"]);
    assert_eq!(rows(&sql(&db, "select count(*) from people")), ["5"]);
    assert_eq!(fails(&db, "alter table people add primary key (name)").0, "42P16", "it has one");
}

/// Rows written under an older layout on another branch still read correctly after a merge.
#[test]
fn alter_table_across_branches() {
    let db = Db::new();
    sql(&db, "create table t (id int primary key, a text, b int)");
    sql(&db, "insert into t values (1, 'x', 10)");
    db.fork(MAIN, "f").unwrap();
    // main changes the table while the branch keeps writing the old layout
    sql(&db, "alter table t drop column a, add c text default 'new'");
    run(&db, "f", "insert into t values (2, 'y', 20); update t set b = 11 where id = 1", &[]).unwrap();
    db.merge("f", Resolve::Fail).unwrap();
    assert_eq!(rows(&sql(&db, "select id, b, c from t order by id")), ["1|11|new", "2|20|new"]);
    // and the other way round: the branch changes the table, main writes the old layout
    db.fork(MAIN, "g").unwrap();
    run(&db, "g", "alter table t rename column b to score, alter column score type text", &[]).unwrap();
    sql(&db, "insert into t (id, b) values (3, 30)");
    db.merge("g", Resolve::Fail).unwrap();
    assert_eq!(rows(&sql(&db, "select id, score || '' from t order by id")), ["1|11", "2|20", "3|30"]);
}

#[test]
fn dates_and_times() {
    let db = Db::new();
    sql(
        &db,
        "create table ev (id serial primary key, name text, at timestamptz default now(), day date default current_date, local timestamp, lasts interval)",
    );
    sql(
        &db,
        "insert into ev (name, at, day, local, lasts) values
           ('launch', '2024-03-10 14:05:09.25+02', '2024-03-10', '2024-03-10 14:05', '1 hour 30 minutes'),
           ('review', '2024-03-31T09:00:00Z', date '2024-03-31', timestamp '2024-03-31 09:00', interval '2 days'),
           ('old', '2023-12-25 00:00:00', '2023-12-25', null, '1 year 2 mons')",
    );
    // Postgres's text forms, stored in UTC
    assert_eq!(
        rows(&sql(&db, "select at, day, local, lasts from ev where id = 1")),
        ["2024-03-10 12:05:09.25+00|2024-03-10|2024-03-10 14:05:00|01:30:00"]
    );
    assert_eq!(rows(&sql(&db, "select lasts from ev where id = 3")), ["1 year 2 mons"]);
    // comparing, sorting, arithmetic
    assert_eq!(rows(&sql(&db, "select name from ev where at > '2024-01-01' order by at desc")), ["review", "launch"]);
    assert_eq!(
        rows(&sql(&db, "select name from ev where day between '2024-03-01' and '2024-03-31' order by day")),
        ["launch", "review"]
    );
    assert_eq!(
        rows(&sql(&db, "select day + 7, day - date '2024-03-01', at + lasts from ev where id = 2")),
        ["2024-04-07|30|2024-04-02 09:00:00+00"]
    );
    assert_eq!(
        rows(&sql(&db, "select at - interval '1 month', at - '2024-03-01'::timestamptz from ev where id = 2")),
        ["2024-02-29 09:00:00+00|30 days 09:00:00"]
    );
    assert_eq!(rows(&sql(&db, "select count(*) from ev where at > now() - interval '100 years'")), ["3"]);
    // date_trunc, extract, grouping by month
    assert_eq!(
        rows(&sql(
            &db,
            "select date_trunc('month', at), extract(dow from day), date_part('year', at) from ev where id = 1"
        )),
        ["2024-03-01 00:00:00+00|0|2024"]
    );
    assert_eq!(
        rows(&sql(&db, "select date_trunc('month', day) m, count(*) from ev group by m order by m")),
        // (a date is truncated as a timestamptz, as Postgres picks date_trunc(text, timestamptz) for it)
        ["2023-12-01 00:00:00+00|1", "2024-03-01 00:00:00+00|2"]
    );
    // defaults are worked out per row
    sql(&db, "insert into ev (name) values ('now')");
    let r = sql(&db, "select at, day, extract(epoch from now() - at) < 60 from ev where name = 'now'");
    assert!(matches!(r.rows[0][0], Value::TimestampTz(_)) && matches!(r.rows[0][1], Value::Date(_)), "{:?}", r.rows);
    assert_eq!(r.rows[0][2], Value::Bool(true));
    // types come out right, and JSON outside SQL is ISO 8601
    let cols: Vec<_> = sql(&db, "select at, day, local, lasts, now(), current_date from ev")
        .columns
        .unwrap()
        .into_iter()
        .map(|c| c.1.name())
        .collect();
    assert_eq!(cols, ["timestamptz", "date", "timestamp", "interval", "timestamptz", "date"]);
    assert_eq!(
        &*db.get(MAIN, "ev/2").unwrap().unwrap(),
        r#"{"at":"2024-03-31T09:00:00+00:00","day":"2024-03-31","id":2,"lasts":"2 days","local":"2024-03-31T09:00:00","name":"review"}"#
    );
    assert_eq!(fails(&db, "insert into ev (name, day) values ('x', '2024-02-30')").0, "22008");
    assert_eq!(fails(&db, "select day * 2 from ev").0, "42883");
    sql(&db, "create table t (x timetz)"); // (time with time zone is a type: its values read and compare)
}

#[test]
fn case_expressions() {
    let db = shop();
    let q = |sql: &str| rows(&sql_(&db, sql));
    assert_eq!(
        q(
            "select id, case when total >= 50 then 'big' when total >= 7 then 'mid' else 'small' end from orders order by id"
        ),
        ["10|mid", "11|small", "12|big", "13|mid", "14|small"]
    );
    assert_eq!(
        q("select case status when 'paid' then 1 when 'open' then 2 end from orders where id in (10, 12)"),
        ["1", "2"]
    );
    assert_eq!(q("select case when false then 1 end"), ["null"]);
    // inside aggregates, and aggregates inside it
    assert_eq!(
        q(
            "select sum(case when status = 'paid' then total else 0 end), count(case when total > 10 then 1 end) from orders"
        ),
        ["33.75|2"]
    );
    assert_eq!(
        q(
            "select customer, case when count(*) > 1 then 'repeat' else 'once' end from orders group by customer order by customer"
        ),
        ["1|repeat", "2|once", "3|once", "9|once"]
    );
    let cols: Vec<_> =
        sql_(&db, "select case when id > 1 then 1 else 2.5 end, case when id > 1 then 'a' end from orders")
            .columns
            .unwrap()
            .into_iter()
            .map(|c| c.1.name())
            .collect();
    assert_eq!(cols, ["numeric", "text"]);
    assert_eq!(fails(&db, "select case end").0, "42601");
}

#[test]
fn upserts() {
    let db = Db::new();
    sql(&db, "create table stock (sku text primary key, qty int not null, note text, seen int default 0)");
    sql(&db, "insert into stock values ('a', 5, 'first', 0)");
    // DO NOTHING skips the clash and keeps the rest
    let o = sql(&db, "insert into stock (sku, qty) values ('a', 99), ('b', 1) on conflict do nothing returning sku");
    assert_eq!((o.tag.as_str(), rows(&o)), ("INSERT 0 1", vec!["b".to_string()]));
    assert_eq!(rows(&sql(&db, "select qty from stock where sku = 'a'")), ["5"]);
    // DO UPDATE sees the existing row (bare names, or the table's) and EXCLUDED, the new one
    let o = sql(
        &db,
        "insert into stock (sku, qty, note) values ('a', 3, 'again'), ('c', 7, 'new')
         on conflict (sku) do update set qty = stock.qty + excluded.qty, note = excluded.note, seen = seen + 1
         returning sku, qty, note, seen",
    );
    assert_eq!(rows(&o), ["a|8|again|1", "c|7|new|0"]);
    assert_eq!(o.tag, "INSERT 0 2");
    // columns SET doesn't name keep the existing row's values, not the new row's
    sql(
        &db,
        "insert into stock (sku, qty, note) values ('c', 1, 'ignored') on conflict (sku) do update set qty = excluded.qty",
    );
    assert_eq!(rows(&sql(&db, "select qty, note from stock where sku = 'c'")), ["1|new"]);
    // WHERE decides which clashes update; parameters and an alias work
    let r = run(&db, MAIN, "insert into stock as s (sku, qty) values ($1, $2) on conflict (sku) do update set qty = excluded.qty where s.qty < excluded.qty", &[Value::Text("a".into()), Value::Int(2)]).unwrap();
    assert_eq!(r[0].tag, "INSERT 0 0");
    assert_eq!(rows(&sql(&db, "select qty from stock where sku = 'a'")), ["8"]);
    // errors, as Postgres gives them
    assert_eq!(fails(&db, "insert into stock (sku, qty) values ('a', 1) on conflict (qty) do nothing").0, "42P10");
    assert_eq!(fails(&db, "insert into stock (sku, qty) values ('a', 1) on conflict do update set qty = 1").0, "42601");
    assert_eq!(
        fails(&db, "insert into stock (sku, qty) values ('z', 1), ('z', 2) on conflict (sku) do update set qty = 0").0,
        "21000"
    );
    assert_eq!(
        fails(&db, "insert into stock (sku, qty) values ('a', 1) on conflict (sku) do update set qty = null").0,
        "23502"
    );
    assert_eq!(fails(&db, "insert into stock (sku, qty) values ('a', 1)").0, "23505");
    // a counter bumped from two branches merges as a conflict, never a lost update
    db.fork(MAIN, "f").unwrap();
    run(
        &db,
        "f",
        "insert into stock (sku, qty) values ('a', 1) on conflict (sku) do update set qty = stock.qty + 1",
        &[],
    )
    .unwrap();
    sql(&db, "insert into stock (sku, qty) values ('a', 1) on conflict (sku) do update set qty = stock.qty + 1");
    assert!(matches!(db.merge("f", Resolve::Fail), Err(chronos::Error::Conflicts(_))));
}

#[test]
fn unique_and_foreign_keys() {
    let db = Db::new();
    sql(&db, "create table orgs (id int primary key, name text not null unique)");
    sql(
        &db,
        "create table users (
            id serial primary key,
            email text unique,
            org int references orgs (id) on delete cascade,
            boss int,
            constraint users_boss_fkey foreign key (boss) references users on delete set null,
            unique (org, boss)
        )",
    );
    sql(&db, "create table notes (id int primary key, author int references users, body text)");
    sql(&db, "insert into orgs values (1, 'acme'), (2, 'globex')");
    sql(&db, "insert into users (email, org) values ('ada@x', 1), ('alan@x', 1), (null, 2), (null, 2)");

    // UNIQUE: duplicates refused, nulls never clash, several columns together
    assert_eq!(fails(&db, "insert into users (email) values ('ada@x')").0, "23505");
    assert_eq!(fails(&db, "update users set email = 'ada@x' where id = 2").0, "23505");
    assert_eq!(fails(&db, "insert into orgs values (3, 'acme')").0, "23505");
    assert_eq!(fails(&db, "insert into users (email) values ('p@x'), ('p@x')").0, "23505");
    sql(&db, "update users set email = 'new@x' where id = 1; update users set email = 'ada@x' where id = 2");
    sql(&db, "update users set boss = 1 where id = 2");
    assert_eq!(fails(&db, "update users set org = 1, boss = 1 where id = 1").0, "23505"); // (1, 1) is user 2's

    // FOREIGN KEY: the parent must exist; null references nothing; one statement can add both
    assert_eq!(fails(&db, "insert into users (email, org) values ('x@x', 9)").0, "23503");
    sql(&db, "insert into users (id, email, boss) values (10, 'b@x', null), (11, 'c@x', 10)");
    sql(&db, "insert into notes values (1, 11, 'hi')");
    // deleting: notes refuse (no action), users go null (boss) or away (org cascade)
    assert_eq!(fails(&db, "delete from users where id = 11").0, "23503");
    sql(&db, "delete from users where id = 10");
    assert_eq!(rows(&sql(&db, "select boss from users where id = 11")), ["null"]);
    sql(&db, "delete from orgs where id = 1");
    assert_eq!(rows(&sql(&db, "select id from users order by id")), ["3", "4", "11"]);
    sql(&db, "delete from notes; delete from users where id = 11");

    // constraints added to existing data are checked first; dropped ones stop applying
    sql(&db, "create table tags (id int primary key, label text)");
    sql(&db, "insert into tags values (1, 'a'), (2, 'a')");
    assert_eq!(fails(&db, "alter table tags add constraint tags_label_key unique (label)").0, "23505");
    sql(&db, "update tags set label = 'b' where id = 2; alter table tags add constraint tags_label_key unique (label)");
    assert_eq!(fails(&db, "insert into tags values (3, 'a')").0, "23505");
    sql(&db, "alter table tags drop constraint tags_label_key; insert into tags values (3, 'a')");
    assert_eq!(fails(&db, "alter table tags add foreign key (id) references orgs").0, "23503");

    // upserts on a UNIQUE column; DO NOTHING without a target skips any clash
    sql(&db, "insert into orgs values (5, 'initech') on conflict (name) do update set id = orgs.id");
    let o = sql(
        &db,
        "insert into orgs values (6, 'initech') on conflict (name) do update set name = 'initech2' returning id, name",
    );
    assert_eq!(rows(&o), ["5|initech2"]);
    assert_eq!(sql(&db, "insert into orgs values (7, 'globex') on conflict do nothing").tag, "INSERT 0 0");

    // dependent tables can't be dropped out from under their references; renames carry over
    assert_eq!(fails(&db, "drop table orgs").0, "2BP01");
    sql(&db, "alter table orgs rename to companies");
    assert_eq!(fails(&db, "insert into users (email, org) values ('z@x', 42)").0, "23503");
    assert_eq!(fails(&db, "insert into companies values (8, 'globex')").0, "23505", "unique survives the rename");
    sql(&db, "insert into users (email, org) values ('z@x', 2)");
    assert_eq!(fails(&db, "insert into users (email) values ('z@x')").0, "23505");
    sql(&db, "alter table users drop column email; insert into users (org) values (2)");
    // the rows behind constraints never show in DIFF
    db.fork(MAIN, "d").unwrap();
    run(&db, "d", "insert into companies values (9, 'hooli')", &[]).unwrap();
    let d = run(&db, "d", "diff", &[]).unwrap();
    assert_eq!(
        rows(&d[0]).iter().map(|r| r.split('|').take(2).collect::<Vec<_>>().join("/")).collect::<Vec<_>>(),
        ["companies/9"]
    );
}

/// Two branches, each valid alone, can break a constraint together: the merge is refused, with
/// nothing applied, and so is a transaction's commit.
#[test]
fn constraints_hold_across_merges() {
    let db = Db::new();
    sql(&db, "create table parents (id int primary key)");
    sql(&db, "create table kids (id int primary key, parent int references parents, email text unique)");
    sql(&db, "insert into parents values (1), (2)");
    db.fork(MAIN, "a").unwrap();
    run(&db, "a", "insert into kids values (1, 1, 'same@x')", &[]).unwrap();
    sql(&db, "insert into kids values (2, 2, 'same@x')");
    let e = db.merge("a", Resolve::Fail).unwrap_err();
    assert!(matches!(&e, chronos::Error::Constraint { code: "23505", .. }), "{e}");
    assert_eq!(rows(&sql(&db, "select count(*) from kids")), ["1"], "nothing merged");
    // picking a side for the rows can't sneak the duplicate in either
    for how in [Resolve::Ours, Resolve::Theirs] {
        let e = db.merge("a", how).unwrap_err();
        assert!(matches!(&e, chronos::Error::Constraint { code: "23505", .. }), "{e}");
    }
    db.discard("a").unwrap();

    db.fork(MAIN, "b").unwrap();
    run(&db, "b", "insert into kids values (3, 1, 'b@x')", &[]).unwrap();
    sql(&db, "delete from parents where id = 1");
    let e = db.merge("b", Resolve::Fail).unwrap_err();
    assert!(matches!(&e, chronos::Error::Constraint { code: "23503", .. }), "{e}");
    db.discard("b").unwrap();

    // a transaction is a branch: its commit is checked the same way
    let (mut s1, mut s2) = (chronos::sql::Session::new(MAIN), chronos::sql::Session::new(MAIN));
    s1.run(&db, "begin; insert into kids values (4, 2, 'tx@x')", &[]).unwrap();
    s2.run(&db, "insert into kids values (5, 2, 'tx@x')", &[]).unwrap();
    assert_eq!(s1.run(&db, "commit", &[]).unwrap_err().code, "23505");
    assert_eq!(rows(&sql(&db, "select id from kids order by id")), ["2", "5"]);
}

#[test]
fn subqueries() {
    let db = shop();
    let q = |sql: &str| rows(&sql_(&db, sql));
    // a value, IN a list, EXISTS: uncorrelated
    assert_eq!(q("select id from orders where total = (select max(total) from orders)"), ["12"]);
    assert_eq!(
        q("select name from customers where id in (select customer from orders where status = 'paid') order by id"),
        ["Ada", "Grace"]
    );
    assert_eq!(q("select name from customers where id not in (select customer from orders) order by id"), ["Linus"]);
    assert_eq!(
        q("select (select count(*) from orders), exists (select 1 from orders where total > 1000)"),
        ["5|false"]
    );
    // correlated: the subquery reads the outer row
    assert_eq!(
        q("select c.name, (select count(*) from orders o where o.customer = c.id) from customers c order by c.id"),
        ["Ada|2", "Alan|1", "Grace|1", "Linus|0"]
    );
    assert_eq!(
        q(
            "select name from customers c where exists (select 1 from orders o where o.customer = c.id and o.total > 50)"
        ),
        ["Alan"]
    );
    assert_eq!(
        q("select name from customers c where not exists (select 1 from orders where customer = c.id) order by name"),
        ["Linus"]
    );
    assert_eq!(
        q(
            "select id from orders o where total > (select avg(total) from orders i where i.status = o.status) order by id"
        ),
        ["10"]
    );
    // nested, in a join, with parameters, in UPDATE and DELETE
    assert_eq!(
        q(
            "select name from customers where id in (select customer from orders where total > (select min(total) from orders)) order by id"
        ),
        ["Ada", "Alan", "Grace"]
    );
    let r = run(
        &db,
        MAIN,
        "select name from customers where id in (select customer from orders where total > $1)",
        &[Value::Float(50.0)],
    )
    .unwrap();
    assert_eq!(rows(&r[0]), ["Alan"]);
    sql(
        &db,
        "update customers set city = (select status from orders where customer = customers.id limit 1) where id = 3",
    );
    assert_eq!(q("select city from customers where id = 3"), ["paid"]);
    sql(&db, "delete from orders where customer not in (select id from customers)");
    assert_eq!(q("select count(*) from orders"), ["4"]);
    // errors, as Postgres gives them
    assert_eq!(fails(&db, "select (select id from orders)").0, "21000");
    assert_eq!(fails(&db, "select (select id, total from orders limit 1)").0, "42601");
}

#[test]
fn union_intersect_except() {
    let db = shop();
    let q = |sql: &str| rows(&sql_(&db, sql));
    assert_eq!(
        q("select city from customers union select status from orders order by 1"),
        ["London", "NYC", "open", "paid", "null"]
    );
    assert_eq!(
        q("select customer from orders union all select id from customers order by customer limit 3"),
        ["1", "1", "1"]
    );
    assert_eq!(q("select id from customers intersect select customer from orders order by id"), ["1", "2", "3"]);
    assert_eq!(q("select customer from orders except select id from customers"), ["9"]);
    assert_eq!(q("select customer from orders intersect all select 1 union all select 1"), ["1", "1"]);
    assert_eq!(q("select customer from orders except all select 1 order by 1"), ["1", "2", "3", "9"]);
    // types widen across parts; names come from the first
    let o = sql_(&db, "select 1 as n union select 2.5 order by n");
    assert_eq!((rows(&o), o.columns.unwrap()[0].1.name()), (vec!["1".to_string(), "2.5".into()], "numeric".into()));
    assert_eq!(
        q("select name from customers where id = 1 union (select 'x' union select 'y') order by 1"),
        ["Ada", "x", "y"]
    );
    assert_eq!(fails(&db, "select 1 union select 1, 2").0, "42601");
    // (a column of a part isn't one of the union's: 42703, as Postgres has it)
    assert_eq!(fails(&db, "select id from orders union select id from customers order by total").0, "42703");
}

#[test]
fn derived_tables_and_with() {
    let db = shop();
    let q = |sql: &str| rows(&sql_(&db, sql));
    // a subquery in FROM, filtered, joined and grouped like a table
    assert_eq!(
        q("select t.customer, t.n from (select customer, count(*) n from orders group by customer) t where t.n > 1"),
        ["1|2"]
    );
    assert_eq!(
        q(
            "select c.name, s.total from customers c join (select customer, sum(total) total from orders group by customer) s on s.customer = c.id order by c.id"
        ),
        ["Ada|25.5", "Alan|100", "Grace|7.25"]
    );
    assert_eq!(
        q("select x, y from (select id, name from customers where id < 3) as t(x, y) order by x"),
        ["1|Ada", "2|Alan"]
    );
    // WITH: named queries, used by later ones, by the main query, and inside its subqueries
    assert_eq!(
        q("with paid as (select * from orders where status = 'paid'),
               big as (select customer from paid where total > 6)
          select name from customers where id in (select customer from big) order by name"),
        ["Ada", "Grace"]
    );
    assert_eq!(q("with t(n) as (select 1 union all select 2) select sum(n) from t"), ["3"]);
    assert_eq!(q("with c as (select id from customers) select count(*) from c a join c b on a.id = b.id"), ["4"]);
    assert_eq!(fails(&db, "select * from (select 1, 2) t(a, b, c)").0, "42601");
}

#[test]
fn window_functions() {
    let db = Db::new();
    sql(&db, "create table s (id int primary key, dept text, pay int)");
    sql(&db, "insert into s values (1, 'a', 10), (2, 'a', 20), (3, 'a', 20), (4, 'b', 5), (5, 'b', 7), (6, 'a', 30)");
    let q = |sql: &str| rows(&sql_(&db, sql));
    // numbering and ranking within partitions (ties share a rank)
    assert_eq!(
        q("select id, row_number() over (partition by dept order by pay), rank() over (partition by dept order by pay),
                  dense_rank() over (partition by dept order by pay) from s order by id"),
        ["1|1|1|1", "2|2|2|2", "3|3|2|2", "4|1|1|1", "5|2|2|2", "6|4|4|3"]
    );
    // running totals: by default the frame runs to the last row tied with this one
    assert_eq!(
        q(
            "select id, sum(pay) over (order by pay, id), sum(pay) over (order by pay), count(*) over () from s order by id"
        ),
        ["1|22|22|6", "2|42|62|6", "3|62|62|6", "4|5|5|6", "5|12|12|6", "6|92|92|6"]
    );
    // explicit frames: a moving average over the row and the one before
    assert_eq!(
        q("select id, avg(pay) over (order by id rows between 1 preceding and current row) from s order by id"),
        [
            "1|10.0000000000000000",
            "2|15.0000000000000000",
            "3|20.0000000000000000",
            "4|12.5000000000000000",
            "5|6.0000000000000000",
            "6|18.5000000000000000"
        ]
    );
    assert_eq!(
        q(
            "select id, max(pay) over (order by id rows between current row and 1 following) from s where dept = 'a' order by id"
        ),
        ["1|20", "2|20", "3|30", "6|30"]
    );
    // neighbours and positions
    assert_eq!(
        q("select id, lag(pay) over (order by id), lead(pay, 2, -1) over (order by id), first_value(pay) over (partition by dept order by id),
                  last_value(pay) over (partition by dept order by id rows between unbounded preceding and unbounded following) from s order by id"),
        ["1|null|20|10|30", "2|10|5|10|30", "3|20|7|10|30", "4|20|30|5|7", "5|5|-1|5|7", "6|7|-1|10|30"]
    );
    assert_eq!(
        q(
            "select id, ntile(4) over (order by id), percent_rank() over (order by pay), cume_dist() over (order by pay) from s where id <= 2 order by id"
        ),
        ["1|1|0|0.5", "2|2|1|1"]
    );
    // with GROUP BY: windows over the groups, and over aggregates
    assert_eq!(
        q(
            "select dept, sum(pay), rank() over (order by sum(pay) desc), sum(sum(pay)) over () from s group by dept order by dept"
        ),
        ["a|80|1|92", "b|12|2|92"]
    );
    // ORDER BY a window, and types
    assert_eq!(q("select id from s order by row_number() over (order by pay desc, id) limit 2"), ["6", "2"]);
    let cols: Vec<_> =
        sql_(&db, "select row_number() over (), percent_rank() over (), lag(dept) over (), sum(pay) over () from s")
            .columns
            .unwrap()
            .into_iter()
            .map(|c| c.1.name())
            .collect();
    assert_eq!(cols, ["bigint", "double precision", "text", "bigint"]);
    // errors, as Postgres gives them
    assert_eq!(fails(&db, "select * from s where row_number() over () > 1").0, "42P20");
    assert_eq!(fails(&db, "select rank() from s").0, "42809");
    assert_eq!(fails(&db, "select sum(row_number() over ()) over () from s").0, "42P20");
}

/// Running sums, sliding sums and ranks against the same worked out in plain Rust.
#[test]
fn window_functions_match_a_reference() {
    let db = Db::new();
    sql(&db, "create table w (id int primary key, g int, x int)");
    let mut seed = 3u64;
    let mut rnd = |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let data: Vec<(i64, i64, i64)> = (0..200).map(|i| (i, rnd(4) as i64, rnd(20) as i64)).collect();
    for (i, g, x) in &data {
        sql(&db, &format!("insert into w values ({i}, {g}, {x})"));
    }
    let got = rows(&sql(
        &db,
        "select id, sum(x) over (partition by g order by x, id), sum(x) over (partition by g order by id rows between 2 preceding and 1 following),
                rank() over (partition by g order by x) from w order by id",
    ));
    let want: Vec<String> = data
        .iter()
        .map(|(i, g, x)| {
            let mut part: Vec<&(i64, i64, i64)> = data.iter().filter(|r| r.1 == *g).collect();
            part.sort_by_key(|r| (r.2, r.0));
            let running: i64 = part.iter().take_while(|r| (r.2, r.0) <= (*x, *i)).map(|r| r.2).sum();
            part.sort_by_key(|r| r.0);
            let at = part.iter().position(|r| r.0 == *i).unwrap();
            let sliding: i64 = part[at.saturating_sub(2)..(at + 2).min(part.len())].iter().map(|r| r.2).sum();
            let rank = part.iter().filter(|r| r.2 < *x).count() + 1;
            format!("{i}|{running}|{sliding}|{rank}")
        })
        .collect();
    assert_eq!(got, want);
}

#[test]
fn with_recursive() {
    let db = Db::new();
    let q = |sql: &str| rows(&sql_(&db, sql));
    // counting, and a running sum alongside
    assert_eq!(
        q("with recursive t(n) as (select 1 union all select n + 1 from t where n < 5) select n from t"),
        ["1", "2", "3", "4", "5"]
    );
    assert_eq!(
        q(
            "with recursive t(n, total) as (select 1, 1 union all select n + 1, total + n + 1 from t where n < 4) select total from t order by n desc limit 1"
        ),
        ["10"]
    );
    // a tree: everyone under a manager, with their depth
    sql(&db, "create table staff (id int primary key, boss int, name text)");
    sql(
        &db,
        "insert into staff values (1, null, 'ceo'), (2, 1, 'cto'), (3, 2, 'dev'), (4, 2, 'ops'), (5, 1, 'cfo'), (6, 3, 'intern')",
    );
    assert_eq!(
        q("with recursive under(id, name, depth) as (
               select id, name, 0 from staff where id = 2
               union all
               select s.id, s.name, u.depth + 1 from staff s join under u on s.boss = u.id)
           select name, depth from under order by depth, name"),
        ["cto|0", "dev|1", "ops|1", "intern|2"]
    );
    // a graph with a cycle: UNION (not ALL) stops when nothing new turns up
    sql(&db, "create table edge (a int, b int)");
    sql(&db, "insert into edge (a, b) values (1, 2), (2, 3), (3, 1), (3, 4)");
    assert_eq!(
        q(
            "with recursive reach(n) as (select 1 union select e.b from edge e join reach r on e.a = r.n) select n from reach order by n"
        ),
        ["1", "2", "3", "4"]
    );
    // alongside ordinary WITH queries, and used twice
    assert_eq!(
        q("with recursive nums(n) as (select 1 union all select n + 1 from nums where n < 3), sq as (select n * n s from nums)
           select (select sum(s) from sq), count(*) from nums"),
        ["14|3"]
    );
    // a loop with no stop is refused, not left running
    assert_eq!(
        fails(&db, "with recursive t(n) as (select 1 union all select n + 1 from t) select count(*) from t").0,
        "54001"
    );
    // (Postgres's invalid_recursion)
    assert_eq!(fails(&db, "with recursive t(n) as (select 1 intersect select n from t) select * from t").0, "42P19");
}

/// JSON writes (put, delete, batch: what agents, HTTP and MCP use) into SQL tables follow the
/// same rules as SQL, and survive reopening.
#[test]
fn json_writes_follow_sql_rules() {
    let dir = std::env::temp_dir().join(format!("chronos-jsonrules-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table orgs (id int primary key, name text not null unique)");
        sql(
            &db,
            "create table users (id int primary key, email text unique, org int references orgs on delete cascade, n serial, role text default 'member')",
        );
        let code = |e: chronos::Error| match e {
            chronos::Error::Constraint { code, .. } => code,
            e => panic!("{e}"),
        };
        db.put(MAIN, "orgs/1", r#"{"name": "acme"}"#).unwrap(); // the key fills in the id
        // a row must fit its table
        assert_eq!(code(db.put(MAIN, "orgs/2", r#"{"name": "x", "color": "red"}"#).unwrap_err()), "42703");
        assert_eq!(code(db.put(MAIN, "orgs/2", r#"{"name": 5, "id": "two"}"#).unwrap_err()), "22P02");
        assert_eq!(code(db.put(MAIN, "orgs/2", r#"{"id": 3, "name": "x"}"#).unwrap_err()), "23514");
        assert_eq!(code(db.put(MAIN, "orgs/two", r#"{"name": "x"}"#).unwrap_err()), "22P02");
        assert_eq!(code(db.put(MAIN, "orgs/2", r#"{}"#).unwrap_err()), "23502");
        assert_eq!(code(db.put(MAIN, "orgs/2", "[1]").unwrap_err()), "22P02");
        // and its constraints
        assert_eq!(code(db.put(MAIN, "orgs/2", r#"{"name": "acme"}"#).unwrap_err()), "23505");
        assert_eq!(code(db.put(MAIN, "users/1", r#"{"email": "a@x", "org": 9}"#).unwrap_err()), "23503");
        db.batch(
            MAIN,
            &[("users/1", Some(r#"{"email": "a@x", "org": 1}"#)), ("users/2", Some(r#"{"email": "b@x", "org": 1}"#))],
        )
        .unwrap();
        assert_eq!(code(db.put(MAIN, "users/3", r#"{"email": "a@x"}"#).unwrap_err()), "23505");
        // defaults are filled in; the row is stored packed and read back as JSON
        // (number 1 went to the write refused above: a refused insert still uses one, as in Postgres)
        assert_eq!(rows(&sql(&db, "select n, role from users order by id")), ["2|member", "3|member"]);
        assert_eq!(
            &*db.get(MAIN, "users/1").unwrap().unwrap(),
            r#"{"email":"a@x","id":1,"n":2,"org":1,"role":"member"}"#
        );
        assert!(
            db.view(MAIN)
                .unwrap()
                .get(&chronos::sql::row_key_of(&db, MAIN, "users/1"))
                .unwrap()
                .unwrap()
                .starts_with('\u{1}')
        );
        // deletes follow ON DELETE; SQL's own keys are off limits
        db.put(MAIN, "orgs/2", r#"{"name": "globex"}"#).unwrap();
        db.put(MAIN, "users/3", r#"{"org": 2}"#).unwrap();
        db.del(MAIN, "orgs/2").unwrap();
        assert_eq!(rows(&sql(&db, "select id from users order by id")), ["1", "2"], "cascaded");
        assert_eq!(code(db.put(MAIN, "_sonos_tables/users", "{}").unwrap_err()), "42501");
        // tables without a schema stay free-form
        db.put(MAIN, "notes/x", r#"{"anything": [1, 2]}"#).unwrap();
        // and two branches each adding the same email can't merge
        db.fork(MAIN, "f").unwrap();
        db.put("f", "users/4", r#"{"email": "same@x"}"#).unwrap();
        db.put(MAIN, "users/5", r#"{"email": "same@x"}"#).unwrap();
        assert_eq!(code(db.merge("f", Resolve::Fail).unwrap_err()), "23505");
        db.discard("f").unwrap();
    }
    // reopening replays it all, rules and index rows included
    let db = Db::open(&dir).unwrap();
    assert_eq!(rows(&sql(&db, "select count(*) from users")), ["3"]);
    assert!(matches!(
        db.put(MAIN, "users/6", r#"{"email": "a@x"}"#),
        Err(chronos::Error::Constraint { code: "23505", .. })
    ));
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Tables big enough that scans, hash joins and grouping split across threads: answers must
/// match a plain model, with rows both in checkpointed pages and in the overlay since, on main
/// and on a fork, on disk and in memory.
#[test]
fn big_reports_match_a_model() {
    const NB: i64 = 9_000;
    const N: i64 = 30_000;
    let dir = std::env::temp_dir().join(format!("chronos-sql-par-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut float_sums = vec![]; // on disk, then in memory: they must agree to the last bit
    for db in [Db::open(&dir).unwrap(), Db::new()] {
        // refs past NB point nowhere; ids from 8000 not a multiple of 4 have no orders
        let o_row = |i: i64| {
            let r = if i % 4 == 0 { i * 7 % (NB + 500) } else { i * 7 % 8000 };
            (i, r, i % 1000, (i % 11 != 0).then(|| format!("n{i}")))
        };
        let mut b: Vec<(i64, i64, String)> = (0..NB).map(|i| (i, i % 50, format!("name {i:05}"))).collect();
        let mut o: Vec<(i64, i64, i64, Option<String>)> = (0..N).map(o_row).collect();
        sql(
            &db,
            "create table b (id bigint primary key, age int, name text); create table o (id bigint primary key, ref bigint, amount int, note text, kind int, f double precision)",
        );
        // a small third table for star joins: ages 40 and up have no label
        sql(&db, "create table c (age int primary key, label text)");
        let labels: Vec<String> = (0..40).map(|a| format!("({a}, 'L{}')", a % 7)).collect();
        sql(&db, &format!("insert into c values {}", labels.join(",")));
        let text = |n: &Option<String>| n.as_ref().map_or("null".into(), |n| format!("'{n}'"));
        for c in b.chunks(3000) {
            let v: Vec<String> = c.iter().map(|(i, a, n)| format!("({i}, {a}, '{n}')")).collect();
            sql(&db, &format!("insert into b values {}", v.join(",")));
        }
        let insert_o = |rows: &[(i64, i64, i64, Option<String>)]| {
            for c in rows.chunks(3000) {
                let v: Vec<String> = c
                    .iter()
                    .map(|(i, r, a, n)| format!("({i}, {r}, {a}, {}, {}, {})", text(n), i % 3, *i as f64 * 0.1))
                    .collect();
                sql(&db, &format!("insert into o values {}", v.join(",")));
            }
        };
        insert_o(&o);
        let _ = db.checkpoint(); // on disk: these rows move into pages; what follows stays in the overlay
        sql(
            &db,
            "delete from o where id % 7 = 3; update o set amount = amount + 1 where id % 5 = 0; delete from b where id % 13 = 4",
        );
        o.retain(|r| r.0 % 7 != 3);
        o.iter_mut().filter(|r| r.0 % 5 == 0).for_each(|r| r.2 += 1);
        b.retain(|r| r.0 % 13 != 4);
        let more: Vec<_> = (N..N + 3000).map(o_row).collect();
        insert_o(&more);
        o.extend(more);

        let check = |db: &Db, o: &[(i64, i64, i64, Option<String>)], b: &[(i64, i64, String)]| {
            let q = |s: &str| rows(&sql(db, s));
            let sum: i64 = o.iter().map(|r| r.2).sum();
            let min_note = o.iter().filter_map(|r| r.3.clone()).min().unwrap();
            let max_id = o.iter().map(|r| r.0).max().unwrap();
            assert_eq!(
                q("select count(*), sum(amount), min(note), max(id) from o"),
                [format!("{}|{sum}|{min_note}|{max_id}", o.len())]
            );
            let n = o.iter().filter(|r| r.2 > 500 && r.3.as_ref().is_some_and(|n| n.starts_with("n1"))).count();
            assert_eq!(q("select count(*) from o where amount > 500 and note like 'n1%'"), [n.to_string()]);

            let by_id: std::collections::HashMap<i64, &(i64, i64, String)> = b.iter().map(|r| (r.0, r)).collect();
            let mut groups: std::collections::BTreeMap<i64, (i64, i64, Option<String>, String)> = Default::default();
            for r in o {
                if let Some(br) = by_id.get(&r.1) {
                    let g = groups.entry(br.1).or_insert((0, 0, None, String::new()));
                    g.0 += 1;
                    g.1 += r.2;
                    if let Some(n) = &r.3 {
                        g.2 = Some(g.2.take().map_or(n.clone(), |m: String| m.min(n.clone())));
                    }
                    g.3 = g.3.clone().max(br.2.clone());
                }
            }
            let want: Vec<String> = groups
                .iter()
                .map(|(a, g)| format!("{a}|{}|{}|{}|{}", g.0, g.1, g.2.as_deref().unwrap_or("null"), g.3))
                .collect();
            assert_eq!(
                q(
                    "select b.age, count(*), sum(o.amount), min(o.note), max(b.name) from o join b on b.id = o.ref group by b.age order by b.age"
                ),
                want
            );
            let refs: std::collections::HashSet<i64> = o.iter().map(|r| r.1).collect();
            let lonely = b.iter().filter(|r| !refs.contains(&r.0)).count();
            assert!(lonely > 100);
            assert_eq!(
                q("select count(*) from b left join o on o.ref = b.id where o.id is null"),
                [lonely.to_string()]
            );
            let mut ids: Vec<String> =
                o.iter().filter(|r| by_id.get(&r.1).is_some_and(|br| br.1 == 7)).map(|r| r.0.to_string()).collect();
            ids.sort_by_key(|s| s.parse::<i64>().unwrap());
            assert_eq!(q("select o.id from o join b on b.id = o.ref where b.age = 7 order by o.id"), ids);
            // rows keep the order their table is read in (integer keys in number order) when there's no ORDER BY
            let mut in_key_order: Vec<_> = o.iter().collect();
            in_key_order.sort_by_key(|r| r.0);
            let young: Vec<String> = in_key_order
                .iter()
                .filter(|r| by_id.get(&r.1).is_some_and(|br| br.1 < 1))
                .map(|r| r.0.to_string())
                .collect();
            assert_eq!(q("select o.id from o join b on b.id = o.ref where b.age < 1"), young);

            // a star join: orders to users to labels, grouped
            let mut star: std::collections::BTreeMap<String, (i64, i64)> = Default::default();
            for r in o {
                if let Some(br) = by_id.get(&r.1).filter(|br| br.1 < 40) {
                    let g = star.entry(format!("L{}", br.1 % 7)).or_default();
                    g.0 += 1;
                    g.1 += r.2;
                }
            }
            assert_eq!(
                q(
                    "select c.label, count(*), sum(o.amount) from o join b on b.id = o.ref join c on c.age = b.age group by c.label order by c.label"
                ),
                star.iter().map(|(l, g)| format!("{l}|{}|{}", g.0, g.1)).collect::<Vec<_>>()
            );

            // few orders pass their own filter: joined by key lookups, not by reading all of b
            let mut few: Vec<(i64, String)> =
                o.iter().filter(|r| r.2 > 997).filter_map(|r| by_id.get(&r.1).map(|br| (r.0, br.2.clone()))).collect();
            few.sort();
            assert!(!few.is_empty() && few.len() < 1000);
            assert_eq!(
                q("select o.id, b.name from o join b on b.id = o.ref where o.amount > 997 order by o.id"),
                few.iter().map(|(i, n)| format!("{i}|{n}")).collect::<Vec<_>>()
            );

            // LEFT JOIN from the big side, then a WHERE that padded rows pass
            let (mut kept, mut matched) = (0, 0);
            for r in o {
                match by_id.get(&r.1) {
                    Some(br) if br.1 < 3 => (kept, matched) = (kept + 1, matched + 1),
                    Some(_) => {}
                    None => kept += 1,
                }
            }
            assert_eq!(
                q("select count(*), count(b.id) from o left join b on b.id = o.ref where b.age is null or b.age < 3"),
                [format!("{kept}|{matched}")]
            );

            // many orders pass their own filter, then join by key and group as they're read
            let mut big: std::collections::BTreeMap<i64, (i64, i64, String, String)> = Default::default();
            for r in o.iter().filter(|r| r.2 > 500) {
                if let Some(br) = by_id.get(&r.1) {
                    let g = big.entry(br.1).or_insert((0, 0, String::new(), String::new()));
                    g.0 += 1;
                    g.1 += r.2;
                    if let Some(n) = &r.3
                        && (g.2.is_empty() || *n < g.2)
                    {
                        g.2 = n.clone();
                    }
                    g.3 = g.3.clone().max(br.2.clone());
                }
            }
            assert_eq!(
                q(
                    "select b.age, count(*), sum(o.amount), min(o.note), max(b.name) from o join b on b.id = o.ref where o.amount > 500 group by b.age order by b.age"
                ),
                big.iter().map(|(a, g)| format!("{a}|{}|{}|{}|{}", g.0, g.1, g.2, g.3)).collect::<Vec<_>>()
            );

            // without ORDER BY, groups come in the order they first appear in the table read first
            let mut ages = vec![];
            for r in &in_key_order {
                if let Some(br) = by_id.get(&r.1)
                    && !ages.contains(&br.1)
                {
                    ages.push(br.1);
                }
            }
            let got: Vec<String> = q("select b.age, count(*) from o join b on b.id = o.ref group by b.age")
                .iter()
                .map(|r| r.split('|').next().unwrap().to_string())
                .collect();
            assert_eq!(got, ages.iter().map(|a| a.to_string()).collect::<Vec<_>>());
            let few_young: Vec<String> = in_key_order
                .iter()
                .filter(|r| r.2 > 900 && by_id.get(&r.1).is_some_and(|br| br.1 < 5))
                .map(|r| r.0.to_string())
                .collect();
            assert_eq!(q("select o.id from o join b on b.id = o.ref where o.amount > 900 and b.age < 5"), few_young);

            // a LEFT JOIN grouped, and the same with a subquery in the WHERE it lets be checked
            let mut left: std::collections::BTreeMap<Option<i64>, i64> = Default::default();
            let mut labelled: std::collections::BTreeMap<i64, i64> = Default::default();
            for r in o {
                match by_id.get(&r.1) {
                    Some(br) if br.1 < 3 => *left.entry(Some(br.1)).or_default() += 1,
                    Some(_) => {}
                    None => *left.entry(None).or_default() += 1,
                }
                if let Some(br) = by_id.get(&r.1).filter(|br| br.1 < 40) {
                    *labelled.entry(br.1).or_default() += 1;
                }
            }
            let mut want: Vec<String> = left.iter().filter_map(|(a, n)| a.map(|a| format!("{a}|{n}"))).collect();
            want.push(format!("null|{}", left[&None])); // nulls sort last
            assert_eq!(
                q(
                    "select b.age, count(*) from o left join b on b.id = o.ref where b.age is null or b.age < 3 group by b.age order by b.age"
                ),
                want
            );
            assert_eq!(
                q(
                    "select b.age, count(*) from o left join b on b.id = o.ref where exists (select 1 from c where c.age = b.age) group by b.age order by b.age"
                ),
                labelled.iter().map(|(a, n)| format!("{a}|{n}")).collect::<Vec<_>>()
            );

            // exactly FEW (1000) orders, then one more: lookups, then a hash join
            let mut ids: Vec<i64> = o.iter().map(|r| r.0).collect();
            ids.sort();
            for n in [1000, 1001] {
                let below = ids[n];
                let mut cnt = 0;
                let mut sum = 0;
                for r in o.iter().filter(|r| r.0 < below) {
                    if by_id.contains_key(&r.1) {
                        cnt += 1;
                        sum += r.2;
                    }
                }
                assert_eq!(
                    q(&format!("select count(*), sum(o.amount) from o join b on b.id = o.ref where o.id < {below}")),
                    [format!("{cnt}|{sum}")]
                );
            }

            // the first table found through an index, with more than FEW rows: kind = id % 3
            let mut by_kind: std::collections::BTreeMap<i64, (i64, i64)> = Default::default();
            let mut kind_ids: Vec<i64> = vec![];
            for r in o.iter().filter(|r| r.0 % 3 == 1) {
                if let Some(br) = by_id.get(&r.1) {
                    let g = by_kind.entry(br.1).or_default();
                    g.0 += 1;
                    g.1 += r.2;
                    if br.1 < 2 {
                        kind_ids.push(r.0);
                    }
                }
            }
            assert_eq!(
                q(
                    "select b.age, count(*), sum(o.amount) from o join b on b.id = o.ref where o.kind = 1 group by b.age order by b.age"
                ),
                by_kind.iter().map(|(a, g)| format!("{a}|{}|{}", g.0, g.1)).collect::<Vec<_>>()
            );
            let mut got: Vec<i64> = q("select o.id from o join b on b.id = o.ref where o.kind = 1 and b.age < 2")
                .iter()
                .map(|s| s.parse().unwrap())
                .collect();
            got.sort();
            kind_ids.sort();
            assert_eq!(got, kind_ids);

            // DISTINCT can't be added up part by part
            let mut distinct: std::collections::BTreeMap<i64, std::collections::HashSet<i64>> = Default::default();
            for r in o {
                if let Some(br) = by_id.get(&r.1) {
                    distinct.entry(br.1).or_default().insert(r.2);
                }
            }
            assert_eq!(
                q("select b.age, count(distinct o.amount) from o join b on b.id = o.ref group by b.age order by b.age"),
                distinct.iter().map(|(a, d)| format!("{a}|{}", d.len())).collect::<Vec<_>>()
            );

            // LEFT joins across three tables, and a CTE as the table joined
            let (mut all, mut labelled) = (0, 0);
            for r in o {
                match by_id.get(&r.1) {
                    None => all += 1,
                    Some(br) if br.1 < 45 => {
                        all += 1;
                        labelled += (br.1 < 40) as i64;
                    }
                    Some(_) => {}
                }
            }
            assert_eq!(
                q(
                    "select count(*), count(c.label) from o left join b on b.id = o.ref left join c on c.age = b.age where b.age is null or b.age < 45"
                ),
                [format!("{all}|{labelled}")]
            );
            let mut per_age: std::collections::BTreeMap<i64, i64> = Default::default();
            for r in o {
                if let Some(br) = by_id.get(&r.1) {
                    *per_age.entry(br.1).or_default() += 1;
                }
            }
            assert_eq!(
                q(
                    "with bb as (select id, age from b) select bb.age, count(*) from o join bb on bb.id = o.ref group by bb.age order by bb.age"
                ),
                per_age.iter().map(|(a, n)| format!("{a}|{n}")).collect::<Vec<_>>()
            );

            let mut per_ref: std::collections::BTreeMap<i64, i64> = Default::default();
            o.iter().for_each(|r| *per_ref.entry(r.1).or_default() += 1);
            let mut got = q("select ref, count(*) from o group by ref");
            got.sort_by_key(|s| s.split('|').next().unwrap().parse::<i64>().unwrap());
            assert_eq!(got, per_ref.iter().map(|(r, n)| format!("{r}|{n}")).collect::<Vec<_>>());
        };
        check(&db, &o, &b);
        // a float column sums in the same order as an expression over it (which isn't split by part)
        let bare = "select b.age, sum(o.f), avg(o.f) from o join b on b.id = o.ref group by b.age order by b.age";
        let expr =
            "select b.age, sum(o.f + 0), avg(o.f + 0) from o join b on b.id = o.ref group by b.age order by b.age";
        assert_eq!(rows(&sql(&db, bare)), rows(&sql(&db, expr)));
        float_sums.push(rows(&sql(
            &db,
            "select b.age, sum(o.amount * 0.1), avg(o.amount + '0.5'), sum(o.f), avg(o.f) from o join b on b.id = o.ref group by b.age order by b.age",
        )));

        // a fork: shares main's pages and overlay, then changes its own rows
        db.fork(MAIN, "f").unwrap();
        run(&db, "f", "delete from o where amount < 100; update o set amount = 0 where id % 3 = 0", &[]).unwrap();
        let mut fo = o.clone();
        fo.retain(|r| r.2 >= 100);
        fo.iter_mut().filter(|r| r.0 % 3 == 0).for_each(|r| r.2 = 0);
        let fork = |q: &str| rows(&run(&db, "f", q, &[]).unwrap().pop().unwrap());
        let sum: i64 = fo.iter().map(|r| r.2).sum();
        assert_eq!(fork("select count(*), sum(amount) from o"), [format!("{}|{sum}", fo.len())]);
        check(&db, &o, &b); // main unchanged
    }
    assert_eq!(float_sums[0], float_sums[1]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A few rows join by looking each match up, which converts the value to the key's type; a hash
/// join (for many rows) matches only values of the same kind. And sums that overflow say so.
#[test]
fn lookup_joins_and_sum_overflow() {
    let db = Db::new();
    sql(&db, "create table k (id bigint primary key, name text); create table t (id int primary key, ref text)");
    sql(&db, "insert into k values (5, 'five'), (7, 'seven'); insert into t values (1, '5'), (2, '7'), (3, 'x')");
    assert_eq!(
        rows(&sql(&db, "select t.id, k.name from t join k on k.id = t.ref order by t.id")),
        ["1|five", "2|seven"]
    );

    sql(
        &db,
        "create table big (id int primary key, n bigint); insert into big values (1, 9223372036854775807), (2, 1)",
    );
    // (a bigint's sum is a numeric, as Postgres's int8_sum has it: its total may pass 64 bits)
    assert_eq!(rows(&sql(&db, "select sum(n) from big")), ["9223372036854775808"]);
    assert_eq!(rows(&sql(&db, "select sum(n) from big where id = 1")), ["9223372036854775807"]);
    // only the total must fit, however the rows are split up to add them
    sql(&db, "insert into big values (3, -100)");
    assert_eq!(rows(&sql(&db, "select sum(n), avg(n) > 0 from big")), ["9223372036854775708|true"]);
}

/// Min and max of an expression mixing kinds depend on the order values meet (text against a
/// number compares as a number), so a big join keeps them in one pass, as with fewer rows.
#[test]
fn min_max_of_mixed_kinds_in_big_joins() {
    let db = Db::new();
    sql(
        &db,
        "create table k (id bigint primary key); insert into k values (0); create table mm (id bigint primary key, ref bigint, s text, n int)",
    );
    let v: Vec<String> = (0..20000)
        .map(|i| {
            let (s, n) = match i {
                1 => ("null".into(), "5".into()),
                4998 => ("'9'".into(), "null".into()),
                4999 => ("'abc'".into(), "null".into()),
                _ => ("null".to_string(), "null".to_string()),
            };
            format!("({i}, {}, {s}, {n})", if i < 5000 { 0 } else { 1 })
        })
        .collect();
    for c in v.chunks(5000) {
        sql(&db, &format!("insert into mm values {}", c.join(",")));
    }
    assert_eq!(rows(&sql(&db, "select max(coalesce(mm.s, mm.n)) from mm join k on k.id = mm.ref")), ["abc"]);
}

#[test]
fn zz_tmp_rows_first_fused() {
    let dir = std::env::temp_dir().join(format!("chronos-sql-zz-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for db in [Db::open(&dir).unwrap(), Db::new()] {
        sql(
            &db,
            "create table b (id bigint primary key, age int); create table o (id bigint primary key, ref bigint, amount int, kind int)",
        );
        let b: Vec<(i64, i64)> = (0..9000).filter(|i| i % 13 != 4).map(|i| (i, i % 50)).collect();
        for c in b.chunks(3000) {
            let v: Vec<String> = c.iter().map(|(i, a)| format!("({i}, {a})")).collect();
            sql(&db, &format!("insert into b values {}", v.join(",")));
        }
        let o: Vec<(i64, i64, i64, i64)> = (0..40000i64).map(|i| (i, (i * 7919) % 9500, i % 1000, i % 3)).collect();
        for c in o.chunks(3000) {
            let v: Vec<String> = c.iter().map(|(i, r, a, k)| format!("({i}, {r}, {a}, {k})")).collect();
            sql(&db, &format!("insert into o values {}", v.join(",")));
        }
        let _ = db.checkpoint();
        let bm: std::collections::HashMap<i64, i64> = b.iter().cloned().collect();
        let mut g: std::collections::BTreeMap<i64, (i64, i64)> = Default::default();
        for r in o.iter().filter(|r| r.3 == 1) {
            if let Some(a) = bm.get(&r.1) {
                let e = g.entry(*a).or_default();
                e.0 += 1;
                e.1 += r.2;
            }
        }
        let want: Vec<String> = g.iter().map(|(a, (c, s))| format!("{a}|{c}|{s}")).collect();
        assert_eq!(
            rows(&sql(
                &db,
                "select b.age, count(*), sum(o.amount) from o join b on b.id = o.ref where o.kind = 1 group by b.age order by b.age"
            )),
            want
        );
        let base = rows(&sql(&db, "select id from o where kind = 1"));
        let refs: std::collections::HashMap<i64, i64> = o.iter().map(|r| (r.0, r.1)).collect();
        let want2: Vec<String> =
            base.into_iter().filter(|id| bm.contains_key(&refs[&id.parse::<i64>().unwrap()])).collect();
        let got = rows(&sql(&db, "select o.id from o join b on b.id = o.ref where o.kind = 1"));
        assert!(want2.len() > 5000);
        assert_eq!(got, want2);
        let got3 = rows(&sql(&db, "select o.id, b.age from o left join b on b.id = o.ref where o.kind = 1"));
        assert_eq!(got3.len(), o.iter().filter(|r| r.3 == 1).count());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Worlds: branches with IDs and metadata, made and listed in SQL, and kept across restarts.
#[test]
fn worlds() {
    let dir = std::env::temp_dir().join(format!("chronos-sql-worlds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        let mut s = chronos::sql::Session::new(MAIN);
        let mut q = |sql: &str| s.run(&db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
        q("create table t (id int primary key, v text); insert into t values (1, 'main')");
        assert_eq!(q("create world agent_1 with (owner = 'claude', task = 42, draft = true)").tag, "CREATE WORLD");
        assert_eq!(q("fork world agent_1 as agent_1a meta '{\"tags\": [\"x\", \"y\"]}'").tag, "CREATE WORLD");
        assert_eq!(q("switch world agent_1a").tag, "SWITCH WORLD");
        q("update t set v = 'deep' where id = 1");
        assert_eq!(rows(&q("select v from t")), ["deep"]);

        let shown = q("show worlds");
        let cols: Vec<String> = shown.columns.clone().unwrap().into_iter().map(|c| c.0).collect();
        assert_eq!(
            cols,
            [
                "name", "id", "parent", "depth", "created", "version", "meta", "owner", "expires", "active", "pinned",
                "scope"
            ]
        );
        let by_name = |n: &str| shown.rows.iter().find(|r| r[0] == Value::Text(n.into())).unwrap().clone();
        let a = by_name("agent_1a");
        assert_eq!((&a[2], &a[3], &a[5]), (&Value::Text("agent_1".into()), &Value::Int(2), &Value::Int(1)));
        assert_eq!(a[6].to_text(), r#"{"tags": ["x", "y"]}"#);
        assert_eq!(by_name("agent_1")[6].to_text(), r#"{"task": 42, "draft": true, "owner": "claude"}"#);
        assert_eq!(by_name("main")[1], Value::Text("main".into()));
        let id = a[1].to_text();
        assert!(id.starts_with('w') && id.len() == 17, "{id}");

        // metadata changes; null removes a key
        let r = q("alter world agent_1 set (task = null, reviewer = 'ada')");
        assert_eq!(r.rows[0][6].to_text(), r#"{"draft": true, "owner": "claude", "reviewer": "ada"}"#);
        // a world is reached by its ID too
        assert_eq!(q(&format!("use world '{id}'")).tag, "SWITCH WORLD");
        assert_eq!(rows(&q("select v from t")), ["deep"]);
        q("use world main");
        assert_eq!(rows(&q("select v from t")), ["main"]);
        assert_eq!(db.world(&id).unwrap().name, "agent_1a");

        let e = s.run(&db, "create world bad meta '[1, 2]'", &[]).unwrap_err();
        assert_eq!(e.code, "22023");
        let mut s2 = chronos::sql::Session::new(MAIN);
        s2.run(&db, "create world gone; delete world gone", &[]).unwrap();
        assert!(db.world("gone").is_err());
    }
    // IDs and metadata survive a restart, from the log and then from a checkpoint
    let before = { Db::open(&dir).unwrap().worlds() };
    assert_eq!(before.iter().find(|w| w.name == "agent_1").unwrap().meta["reviewer"], "ada");
    let db = Db::open(&dir).unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let after = Db::open(&dir).unwrap().worlds();
    assert_eq!(before, after);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn insert_select() {
    let db = people();
    sql(&db, "create table archive (id int primary key, name text not null, note text default 'copied')");
    let o = sql(
        &db,
        "insert into archive (id, name) select id, name from people where age > 30 order by id returning id, note",
    );
    assert_eq!(rows(&o), ["1|copied", "2|copied", "3|copied"]);
    let o =
        run(&db, MAIN, "insert into archive (id, name) select id, name from people where id = $1", &[Value::Int(4)])
            .unwrap();
    assert_eq!(o.last().unwrap().tag, "INSERT 0 1");
    let e = run(&db, MAIN, "insert into archive (id, name) select id, name from people", &[]).unwrap_err();
    assert_eq!(e.code, "23505", "duplicates fail as with VALUES");
    sql(
        &db,
        "insert into archive (id, name) select id, name || '!' from people on conflict (id) do update set name = excluded.name",
    );
    assert_eq!(rows(&sql(&db, "select name from archive order by id")), ["Ada!", "Alan!", "Grace!", "Linus!"]);
}

/// DIFF shows the columns each row changed and changes to tables themselves; DIFF AS SQL gives
/// statements that, run on the world the diff starts from, make it equal to the one it ends at.
#[test]
fn diff_engine() {
    let db = Db::new();
    sql(&db, "create table t (id int primary key, name text not null, age int, meta jsonb)");
    sql(&db, "insert into t values (1, 'Ada', 36, '{\"a\": 1}'), (2, 'Alan', 41, null), (4, 'it''s', 7, '[1, \"x\"]')");
    sql(&db, "create table old (id int primary key)");
    sql(&db, "insert into old values (1)");
    db.fork(MAIN, "x").unwrap();
    let on = |b: &str, q: &str| run(&db, b, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    on("x", "alter table t add column email text default 'none'");
    on("x", "create index t_age on t (age)");
    on("x", "update t set name = 'Ada L.', age = 37 where id = 1");
    on("x", "delete from t where id = 2");
    on("x", "insert into t (id, name, meta) values (3, 'Grace', '{\"q\": \"o''k\"}')");
    on("x", "create table newt (id serial primary key, v text unique, n int not null default 5)");
    on("x", "insert into newt (v) values ('a'), ('b')");
    on("x", "drop table old");
    db.put("x", "notes/1", r#"{"text": "json"}"#).unwrap();

    let d = on("x", "diff world x");
    let got: Vec<String> = d
        .rows
        .iter()
        .map(|r| {
            format!(
                "{} {} {} {}",
                r[0].to_text(),
                if r[1] == Value::Null { "-".into() } else { r[1].to_text() },
                r[2].to_text(),
                r[5].to_text()
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            // deleted rows first, then the rest in key order
            r#"old 1 delete []"#,
            r#"t 2 delete []"#,
            r#"newt - create table []"#,
            r#"old - drop table []"#,
            r#"t - alter table ["+email text", "+index t_age"]"#,
            r#"newt 1 insert ["id", "n", "v"]"#,
            r#"newt 2 insert ["id", "n", "v"]"#,
            r#"notes 1 insert ["text"]"#,
            r#"t 1 update ["age", "email", "name"]"#,
            r#"t 3 insert ["email", "id", "meta", "name"]"#, // a null is no field
        ],
        "\n{}",
        got.join("\n")
    );

    // the SQL form, run on a copy of where the diff starts, gives where it ends
    let stmts: Vec<String> = on("x", "diff world x as sql").rows.iter().map(|r| r[0].to_text()).collect();
    assert!(stmts.iter().any(|s| s.starts_with("-- notes/1")), "{stmts:?}");
    db.fork(MAIN, "y").unwrap();
    for s in &stmts {
        if !s.starts_with("--") {
            on("y", s);
        }
    }
    for q in ["select * from t order by id", "select id, v, n from newt order by id"] {
        assert_eq!(rows(&on("y", q)), rows(&on("x", q)), "{q}\n{}", stmts.join("\n"));
    }
    assert!(run(&db, "y", "select * from old", &[]).is_err(), "old is dropped");
    assert_eq!(rows(&on("y", "diff world y to x")).len(), 1, "only the JSON note differs");
}

#[test]
fn show_metrics() {
    let db = Db::new();
    let mut s = chronos::sql::Session::new(MAIN);
    s.run(&db, "create table t (id int primary key, v int); insert into t values (1, 1), (2, 2)", &[]).unwrap();
    for _ in 0..10 {
        s.run(&db, "select * from t", &[]).unwrap();
    }
    s.run(&db, "begin; update t set v = 3 where id = 1; commit", &[]).unwrap();
    s.run(&db, "create world w; use world w; delete from t where id = 2; use world main; merge world w", &[]).unwrap();
    assert!(s.run(&db, "insert into t values (1, 1)", &[]).is_err());
    let out = s.run(&db, "show metrics", &[]).unwrap().pop().unwrap();
    let get = |name: &str| out.rows.iter().find(|r| r[0].to_text() == name).map(|r| r[1].to_text()).unwrap_or_default();
    assert_eq!(
        [
            "select",
            "insert",
            "update",
            "delete",
            "schema",
            "commit",
            "fork",
            "merge",
            "rows_written",
            "errors",
            "worlds"
        ]
        .map(get),
        ["10", "2", "1", "1", "1", "1", "1", "1", "5", "1", "1"],
        "BEGIN's own world is no fork, and its COMMIT is a commit, not a merge"
    );
    let select = out.rows.iter().find(|r| r[0].to_text() == "select").unwrap();
    let ms: Vec<f64> = select[3..7].iter().map(|v| v.to_text().parse().unwrap()).collect();
    assert!(ms.windows(2).all(|w| w[0] <= w[1]) && ms[3] > 0.0, "p50 <= p95 <= p99 <= max: {ms:?}");
}

#[test]
fn upserts_on_several_columns() {
    let db = Db::new();
    sql(&db, "create table seat (id serial primary key, room text, num int, who text, unique (room, num))");
    sql(&db, "insert into seat (room, num, who) values ('a', 1, 'Ada'), ('a', 2, 'Bo')");
    // the target's columns in any order name the constraint on them
    sql(
        &db,
        "insert into seat (room, num, who) values ('a', 1, 'Cy') on conflict (num, room) do update set who = excluded.who",
    );
    sql(
        &db,
        "insert into seat (room, num, who) values ('a', 2, 'Di') on conflict on constraint seat_room_num_key do update set who = seat.who || '+Di'",
    );
    sql(&db, "insert into seat (room, num, who) values ('b', 1, 'Ed') on conflict (room, num) do nothing");
    assert_eq!(
        rows(&sql(&db, "select room, num, who from seat order by room, num")),
        ["a|1|Cy", "a|2|Bo+Di", "b|1|Ed"]
    );
    assert_eq!(fails(&db, "insert into seat (room, num) values ('a', 1) on conflict (room) do nothing").0, "42P10");
    assert_eq!(
        fails(&db, "insert into seat (room, num) values ('a', 1) on conflict on constraint nope do nothing").0,
        "42P10"
    );
    // the primary key by its constraint name
    sql(
        &db,
        "insert into seat (id, room, num, who) values (1, 'z', 9, 'Fay') on conflict on constraint seat_pkey do update set who = excluded.who",
    );
    assert_eq!(rows(&sql(&db, "select who from seat where id = 1")), ["Fay"]);
}

#[test]
fn any_and_all() {
    let db = Db::new();
    sql(&db, "create table n (id int primary key, v int)");
    sql(&db, "insert into n values (1, 10), (2, 20), (3, 30), (4, null)");
    let ids = |q: &str| rows(&sql(&db, &format!("select id from n where {q} order by id")));
    // over a subquery
    assert_eq!(ids("v = any (select v from n where id <= 2)"), ["1", "2"]);
    assert_eq!(ids("v > all (select v from n where id <= 2)"), ["3"]);
    assert_eq!(ids("v >= some (select v from n where id >= 2 and id <= 3)"), ["2", "3"]);
    assert_eq!(ids("v <> all (select v from n where id = 1)"), ["2", "3"]);
    // a null in the set makes ALL unknown unless something decides it
    assert_eq!(ids("v > all (select v from n)"), Vec::<String>::new());
    assert_eq!(ids("v < all (select v from n where id > 100)"), ["1", "2", "3", "4"], "ALL over nothing is true");
    // ARRAY[...], a Postgres array literal, and a parameter
    assert_eq!(ids("v = any (array[10, 30])"), ["1", "3"]);
    assert_eq!(ids("v < all (array[25, 40])"), ["1", "2"]);
    assert_eq!(ids("v = any ('{20,30}')"), ["2", "3"]);
    assert_eq!(ids("not (v = any (array[]))"), ["1", "2", "3", "4"], "ANY over nothing is false");
    let p = run(&db, MAIN, "select id from n where id = any($1) order by id", &[Value::Text("{1,3}".into())]).unwrap();
    assert_eq!(rows(&p[0]), ["1", "3"]);
    let j =
        run(&db, MAIN, "select id from n where id = any($1) order by id", &[Value::Json(serde_json::json!([2, 4]))])
            .unwrap();
    assert_eq!(rows(&j[0]), ["2", "4"]);
    assert_eq!(
        rows(&sql(&db, "select 1 = any ('{\"1\", NULL}'), 2 = any ('{1, NULL}'), 2 = all ('{}')")),
        ["true|null|true"]
    );
}

#[test]
fn time_of_day() {
    let db = Db::new();
    sql(&db, "create table shift (id int primary key, starts time, ends time without time zone)");
    sql(&db, "insert into shift values (1, '09:00', '17:30'), (2, '22:00:00', '06:15:30.25'), (3, null, '00:00')");
    assert_eq!(
        rows(&sql(&db, "select starts, ends from shift order by id")),
        ["09:00:00|17:30:00", "22:00:00|06:15:30.25", "null|00:00:00"]
    );
    assert_eq!(rows(&sql(&db, "select id from shift where starts > time '12:00' order by id")), ["2"]);
    assert_eq!(rows(&sql(&db, "select id from shift order by ends")), ["3", "2", "1"]);
    assert_eq!(rows(&sql(&db, "select ends - starts from shift where id = 1")), ["08:30:00"]);
    assert_eq!(
        rows(&sql(&db, "select starts + interval '3 hours' from shift where id = 2")),
        ["01:00:00"],
        "wraps past midnight"
    );
    assert_eq!(rows(&sql(&db, "select starts - '10 hours' from shift where id = 1")), ["23:00:00"]);
    assert_eq!(rows(&sql(&db, "select date '2026-09-24' + starts from shift where id = 1")), ["2026-09-24 09:00:00"]);
    assert_eq!(
        rows(&sql(&db, "select extract(hour from ends), extract(minute from ends) from shift where id = 2")),
        ["6|15"]
    );
    assert_eq!(
        rows(&sql(&db, "select timestamp '2026-01-02 03:04:05'::time, '10:00'::time::interval")),
        ["03:04:05|10:00:00"]
    );
    assert_eq!(fails(&db, "insert into shift values (4, '25:00', null)").0, "22008");
    // (a field a time of day has no such part of: Postgres says 0A000, "unit not supported", not 22P02)
    assert_eq!(fails(&db, "select extract(day from starts) from shift").0, "0A000");
    assert_eq!(fails(&db, "select extract(bogus from starts) from shift").0, "22023");
    // indexes and grouping
    sql(&db, "create index shift_starts on shift (starts)");
    assert_eq!(rows(&sql(&db, "select id from shift where starts = '22:00'")), ["2"]);
    assert_eq!(rows(&sql(&db, "select count(*) from shift group by ends > '12:00' order by 1")), ["1", "2"]);
    assert_eq!(sql(&db, "select localtime").rows.len(), 1);
}

#[test]
fn function_columns_are_named_as_in_postgres() {
    let db = Db::new();
    let o = sql(&db, "select current_date, now(), split_part('a,b', ',', 1), date_part('year', now()), upper('x')");
    let names: Vec<String> = o.columns.unwrap().into_iter().map(|c| c.0).collect();
    assert_eq!(names, ["current_date", "now", "split_part", "date_part", "upper"]);
    // (extract is `extract` since Postgres 14; AT TIME ZONE is the function `timezone`)
    let o = sql(&db, "select extract(year from now()), now() at time zone 'UTC', timezone('UTC', now())");
    let names: Vec<String> = o.columns.unwrap().into_iter().map(|c| c.0).collect();
    assert_eq!(names, ["extract", "timezone", "timezone"]);
}

#[test]
fn subquery_columns_are_named_as_in_postgres() {
    let db = Db::new();
    // a scalar subquery is called what its column is; ARRAY(SELECT ...) is array; GROUPING() is grouping
    let o = sql(
        &db,
        "select (select count(*) from generate_series(1, 3)), (select 1 as one), (select 2), \
         array(select g from generate_series(1, 2) g), array(select g from generate_series(1, 2) g group by g)",
    );
    let names: Vec<String> = o.columns.unwrap().into_iter().map(|c| c.0).collect();
    assert_eq!(names, ["count", "one", "?column?", "array", "array"]);
    let o = sql(&db, "select (select grouping(v1)) from (values (1)) v(v1) group by cube(v1)");
    let names: Vec<String> = o.columns.unwrap().into_iter().map(|c| c.0).collect();
    assert_eq!(names, ["grouping"]);
    // sum(real) is a real; a real as numeric has its six digits
    let o = sql(&db, "select sum(x), avg(x), x::numeric from (values (324.78::real)) v(x) group by x");
    let types: Vec<String> = o.columns.clone().unwrap().into_iter().map(|c| c.1.name().into_owned()).collect();
    assert_eq!(types, ["real", "double precision", "numeric"]);
    assert_eq!(o.rows[0][2].to_text(), "324.78");
}

#[test]
fn a_primary_key_added_in_a_world_merges_and_reopens() {
    let dir = std::env::temp_dir().join(format!("chronos-add-pk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        sql(&db, "create table k (code text, n int)");
        sql(&db, "insert into k values ('a', 1), ('b', 2)");
        db.fork(MAIN, "w").unwrap();
        run(&db, "w", "alter table k add primary key (code)", &[]).unwrap();
        assert!(db.get("w", "k/a").unwrap().is_some(), "rows are keyed by code now");
        db.merge("w", Resolve::Fail).unwrap();
        assert_eq!(rows(&sql(&db, "select n from k where code = 'b'")), ["2"]);
    }
    let db = Db::open(&dir).unwrap();
    assert_eq!(fails(&db, "insert into k values ('a', 3)").0, "23505");
    assert_eq!(rows(&sql(&db, "select code from k order by code")), ["a", "b"]);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// pgvector's SQL, as Mem0's pgvector store sends it (psycopg2 inlines Python lists as ARRAY[...]).
#[test]
fn pgvector() {
    let db = Db::new();
    let q = |s: &str| rows(&sql(&db, s));
    assert_eq!(
        q("select table_name from information_schema.tables where table_schema = 'public'"),
        Vec::<String>::new()
    );
    q("create extension if not exists vector");
    q("create table if not exists mem (id uuid primary key, vector vector(3), payload jsonb)");
    q("create index if not exists mem_hnsw_idx on mem using hnsw (vector vector_cosine_ops)");
    q("create index if not exists mem_text_idx on mem using gin(to_tsvector('simple', payload->>'text_lemmatized'))");
    assert_eq!(q("select table_name from information_schema.tables where table_schema = 'public'"), ["mem"]);
    q(r#"insert into mem (id, vector, payload) values
        ('00000000-0000-4000-8000-00000000000a', ARRAY[1.0,0.0,0.0], '{"user_id":"u1","data":"likes tea","n":3}'),
        ('00000000-0000-4000-8000-00000000000b', ARRAY[0.0,1.0,0.0], '{"user_id":"u1","data":"lives in Paris","n":7}'),
        ('00000000-0000-4000-8000-00000000000c', '[0.9,0.1,0]', '{"user_id":"u2","data":"likes coffee","n":5}')"#);
    let near = "select id, vector <=> ARRAY[1.0,0.05,0.0]::vector as distance, payload from mem";
    assert_eq!(
        q(&format!("select id from ({near}) d order by distance limit 2")),
        ["00000000-0000-4000-8000-00000000000a", "00000000-0000-4000-8000-00000000000c"]
    );
    assert_eq!(
        q(&format!(
            "{} where payload->>'user_id' = 'u1' and (payload->>'n')::numeric > 2 order by distance limit 5",
            near
        )),
        q(&format!("{} where payload->>'user_id' = 'u1' order by distance limit 5", near)),
    );
    assert_eq!(
        q("select id from mem where payload->>'user_id' = 'u1' order by vector <-> '[0,1,0]' limit 1"),
        ["00000000-0000-4000-8000-00000000000b"]
    );
    assert_eq!(q("select vector <#> '[1,2,3]' from mem where id = '00000000-0000-4000-8000-00000000000a'"), ["-1"]);
    assert_eq!(q("select vector from mem where id = '00000000-0000-4000-8000-00000000000c'"), ["[0.9,0.1,0]"]); // as pgvector writes it
    // parameters: drivers send lists as arrays ('{...}') or pgvector's text ('[...]')
    let o =
        run(&db, MAIN, "select id from mem order by vector <=> $1::vector limit 1", &[Value::Text("{0,1,0}".into())])
            .unwrap();
    assert_eq!(rows(&o[0]), ["00000000-0000-4000-8000-00000000000b"]);
    q("update mem set vector = '[0,0,1]' where id = '00000000-0000-4000-8000-00000000000a'");
    assert_eq!(q("select id from mem order by vector <=> '[0,0,2]' limit 1"), ["00000000-0000-4000-8000-00000000000a"]);
    // dimensions are checked
    assert_eq!(fails(&db, "insert into mem values ('00000000-0000-4000-8000-00000000000d', '[1,2]', '{}')").0, "22000");
    assert_eq!(fails(&db, "select '[1,2]'::vector <-> '[1,2,3]'").0, "22000");
    assert_eq!(fails(&db, "select '[1,x]'::vector").0, "22P02");
    assert_eq!(fails(&db, "create extension plv8").0, "0A000");
}

/// With a pgvector index, ORDER BY a distance LIMIT k comes from the search index's candidates,
/// re-ranked exactly: the same rows as comparing every row (below the HNSW size, the search index
/// scans exactly), filters on columns and on jsonb ->> 'key' included.
#[test]
fn pgvector_index_route() {
    let db = Db::new();
    let q = |s: &str| rows(&sql(&db, s));
    q("create table m (id int primary key, v vector(4), payload jsonb, kind text)");
    let mut x: u64 = 7;
    let mut rnd = || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((x >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
    };
    let mut values = vec![];
    for i in 0..600 {
        let v: Vec<f64> = (0..4).map(|_| rnd()).collect();
        let n = v.iter().map(|a| a * a).sum::<f64>().sqrt();
        let v: Vec<String> = v.iter().map(|a| format!("{:.5}", a / n)).collect();
        values.push(format!(
            "({i}, '[{}]', '{{\"user_id\": \"u{}\", \"n\": {}}}', '{}')",
            v.join(","),
            i % 7,
            i % 10,
            ["a", "b"][i % 2]
        ));
    }
    q(&format!("insert into m values {}", values.join(",")));
    let queries = [
        "select id from m order by v <=> '[0.5,0.5,0.5,0.5]' limit 5",
        "select id, v <=> '[1,0,0,0]' as distance from m where payload->>'user_id' = 'u3' order by distance limit 5",
        "select id from m where kind = 'b' and payload->>'user_id' = 'u2' and (payload->>'n')::numeric > 4 order by v <-> '[0,1,0,0]' limit 4 offset 1",
        "select id from m where payload->>'user_id' = 'nobody' order by v <#> '[0,0,1,0]' limit 3",
    ];
    let exact: Vec<Vec<String>> = queries.iter().map(|s| q(s)).collect();
    q("create index if not exists m_v on m using hnsw (v vector_cosine_ops) with (m = 16)");
    q("create index if not exists m_v on m using hnsw (v vector_cosine_ops)");
    for (s, want) in queries.iter().zip(&exact) {
        assert_eq!(&q(s), want, "{s}");
    }
    // a branch's own writes are candidates too
    q("create world w");
    run(&db, "w", "insert into m values (1000, '[0.5,0.5,0.5,0.5]', '{\"user_id\": \"u3\"}', 'a')", &[]).unwrap();
    let o = run(
        &db,
        "w",
        "select id from m where payload->>'user_id' = 'u3' order by v <=> '[0.5,0.5,0.5,0.5]' limit 1",
        &[],
    )
    .unwrap();
    assert_eq!(rows(&o[0]), ["1000"]);
    q("drop index m_v");
    assert_eq!(fails(&db, "create index on m using hnsw (kind)").0, "0A000");
}

/// A pgvector index keeps its operator class (issue #32): pg_get_indexdef shows it as Postgres does (L2's, the
/// default, left out), the distance queries use is still any of the three, and an unknown class is refused as
/// Postgres refuses it.
#[test]
fn hnsw_keeps_its_operator_class() {
    let db = Db::new();
    let mut s = chronos::sql::Session::new(MAIN);
    let mut q = |x: &str| rows(&s.run(&db, x, &[]).unwrap_or_else(|e| panic!("{x}: {e}")).pop().unwrap());
    q("create table v (id int primary key, e vector(2))");
    q("insert into v values (1, '[1,0]'), (2, '[0,3]'), (3, '[2,2]')");
    q("create index v_cos on v using hnsw (e vector_cosine_ops)");
    q("create index v_l2 on v using hnsw (e vector_l2_ops) with (m = 16)");
    q("create index v_ip on v using hnsw (e vector_ip_ops)");
    q("create index v_plain on v using hnsw (e)");
    assert_eq!(
        q(
            "select indexname || ': ' || indexdef from pg_indexes where tablename = 'v' and indexname <> 'v_pkey' order by 1"
        ),
        [
            "v_cos: CREATE INDEX v_cos ON public.v USING hnsw (e vector_cosine_ops)",
            "v_ip: CREATE INDEX v_ip ON public.v USING hnsw (e vector_ip_ops)",
            "v_l2: CREATE INDEX v_l2 ON public.v USING hnsw (e)",
            "v_plain: CREATE INDEX v_plain ON public.v USING hnsw (e)",
        ]
    );
    // each distance still ranks as it should, whichever indexes the column has
    assert_eq!(q("select id from v order by e <=> '[1,0.1]' limit 1"), ["1"]);
    assert_eq!(q("select id from v order by e <-> '[0,2.5]' limit 1"), ["2"]);
    assert_eq!(q("select id from v order by e <#> '[1,1]' limit 1"), ["3"]);
    let (code, message) = fails(&db, "create index on v using hnsw (e vector_hamming_ops)");
    assert_eq!(code, "42704", "{message}");
    assert_eq!(message, "operator class \"vector_hamming_ops\" does not exist for access method \"hnsw\"");
}

#[test]
fn hnsw_ef_search_is_a_session_setting() {
    let db = Db::new();
    let mut s = chronos::sql::Session::new(MAIN);
    let mut q = |x: &str| rows(&s.run(&db, x, &[]).unwrap_or_else(|e| panic!("{x}: {e}")).pop().unwrap());
    q("create table v (id int primary key, e vector(2))");
    q("insert into v values (1, '[1,0]'), (2, '[0,1]')");
    q("create index on v using hnsw (e vector_cosine_ops)");
    q("set hnsw.ef_search = 10");
    q("set local hnsw.ef_search to 200");
    assert_eq!(q("select id from v order by e <=> '[1,0.1]' limit 1"), ["1"]);
    q("set hnsw.ef_search = default");
    assert_eq!(fails(&db, "set hnsw.ef_search = 0").0, "22023");
}

#[test]
fn search_function_ranks_with_the_text_index() {
    let db = Db::new();
    sql(&db, "create table docs (id int primary key, title text, body text)");
    sql(
        &db,
        "insert into docs values (1, 'Agent memory', 'how agents remember things across sessions'), (2, 'Worlds', 'forking a database for each agent'), (3, 'Cooking', 'how to bake bread')",
    );
    let o = sql(&db, "select id, row->>'title' from search('docs', 'agent memory')");
    assert_eq!(rows(&o), ["1|Agent memory", "2|Worlds"], "best first");
    let o = sql(&db, "select d.title from search('docs', 'bread', 5) s join docs d on d.id::text = s.id");
    assert_eq!(rows(&o), ["Cooking"]);
    assert_eq!(rows(&sql(&db, "select count(*) from search('docs', 'agent', 1)")), ["1"]);
    assert_eq!(rows(&sql(&db, "select count(*) from search('docs', 'zebra')")), ["0"]);
    // a world searches its own rows
    db.fork(MAIN, "w").unwrap();
    run(&db, "w", "insert into docs values (4, 'Zebras', 'zebra facts')", &[]).unwrap();
    assert_eq!(rows(&run(&db, "w", "select id from search('docs', 'zebra')", &[]).unwrap()[0]), ["4"]);
    assert_eq!(rows(&sql(&db, "select count(*) from search('docs', 'zebra')")), ["0"]);
    assert_eq!(fails(&db, "select * from search('_sonos_tables', 'x')").0, "42501");
}

/// A prepared SELECT plans once and reuses the plan, but never past a change to a table it reads:
/// columns added or renamed, a table dropped and made again, another world's schema.
#[test]
fn prepared_plan_follows_schema_changes() {
    let db = Db::new();
    sql(&db, "create table o (id int primary key, ref int, amount int)");
    sql(&db, "create table b (id int primary key, name text)");
    sql(&db, "insert into b values (1, 'one'), (2, 'two'); insert into o values (10, 1, 5), (20, 2, 7)");
    let st = chronos::sql::parse("select b.*, o.amount from o join b on b.id = o.ref where o.id = $1")
        .unwrap()
        .pop()
        .unwrap();
    let go = |branch: &str, id: i64| {
        rows(&st.execute(&db, branch, &[Value::Int(id)], &[Type::Int]).unwrap_or_else(|e| panic!("{branch}: {e}")))
    };
    assert_eq!(go(MAIN, 10), ["1|one|5"]);
    assert_eq!(go(MAIN, 20), ["2|two|7"], "the kept plan, new parameter");
    sql(&db, "alter table b add column note text default 'n'");
    assert_eq!(go(MAIN, 10), ["1|one|n|5"], "a column added since planning");
    sql(&db, "alter table b rename column name to title");
    let st2 =
        chronos::sql::parse("select b.title from o join b on b.id = o.ref where o.id = $1").unwrap().pop().unwrap();
    assert_eq!(rows(&st2.execute(&db, MAIN, &[Value::Int(20)], &[Type::Int]).unwrap()), ["two"]);
    sql(&db, "drop table b; create table b (id int primary key, name int)");
    sql(&db, "insert into b values (1, 42)");
    assert_eq!(go(MAIN, 10), ["1|42|5"], "the table made again, with another type");
    db.fork(MAIN, "w").unwrap();
    run(&db, "w", "alter table b add column extra int default 9", &[]).unwrap();
    assert_eq!(go("w", 10), ["1|42|9|5"], "another world, another schema");
    assert_eq!(go(MAIN, 10), ["1|42|5"], "and back");
}

/// As in Postgres, CREATE TABLE AS and CREATE MATERIALIZED VIEW find a relation already there (42P07) before they look
/// at a void column (42P16); a plain view, which Postgres checks the other way round, does not.
#[test]
fn relation_there_before_void_column() {
    let db = people();
    assert_eq!(fails(&db, "create table people as select pg_sleep(0) v").0, "42P07");
    assert_eq!(fails(&db, "create materialized view people as select pg_sleep(0) v").0, "42P07");
    assert_eq!(fails(&db, "create view people as select pg_sleep(0) v").0, "42P16");
    assert_eq!(fails(&db, "create table fresh as select pg_sleep(0) v").0, "42P16");
}

/// CREATE TABLE's clauses Postgres takes and that change nothing here: WITHOUT OIDS, and USING heap, WITH (...)
/// and TABLESPACE before CREATE TABLE AS's AS; a CHECK's NOT VALID and NO INHERIT in either order.
#[test]
fn create_table_clauses_that_change_nothing() {
    let db = Db::new();
    sql(&db, "create table w1 (a int, primary key (a)) without oids");
    sql(&db, "create table w2 using heap with (fillfactor = 70) without oids as select 1 as x");
    assert_eq!(rows(&sql(&db, "select x from w2")), ["1"]);
    assert_eq!(fails(&db, "create table w3 using heap2 as select 1 as x").0, "42704");
    sql(&db, "create table nv_parent (d date, check (false) no inherit not valid)");
    sql(&db, "create table nv_other (d int, check (d > 0) not valid no inherit, e int check (e > 0) not null)");
    assert_eq!(fails(&db, "insert into nv_other values (1, null)").0, "23502");
}

/// CREATE SCHEMA's elements are made tables first, then views, then indexes, whatever order they're written in; and
/// CREATE (a reserved word) starts the next element rather than being an alias.
#[test]
fn create_schema_elements_in_postgres_order() {
    let db = Db::new();
    sql(
        &db,
        "create schema ns1
           create unique index abc_a_idx on abc (a)
           create view abc_view as select a + 1 as a, b + 1 as b from abc
           create table abc (a serial, b int unique)",
    );
    sql(&db, "insert into ns1.abc (b) values (5)");
    assert_eq!(rows(&sql(&db, "select a, b from ns1.abc_view")), ["2|6"]);
}

/// A trigger's UPDATE OF column can't be dropped from under it (2BP01); with CASCADE the trigger goes too.
#[test]
fn drop_column_a_trigger_depends_on() {
    let db = Db::new();
    sql(&db, "create table mt (a int, b int, c int)");
    sql(&db, "create function tf() returns trigger language plpgsql as $$ begin return new; end $$");
    sql(&db, "create trigger t_b after update of b on mt for each row execute function tf()");
    let (code, message) = fails(&db, "alter table mt drop column b");
    assert_eq!(code, "2BP01");
    assert_eq!(message, "cannot drop column b of table mt because other objects depend on it");
    sql(&db, "alter table mt drop column a");
    sql(&db, "alter table mt drop column b cascade");
    assert!(rows(&sql(&db, "select tgname from pg_trigger where tgname = 't_b'")).is_empty());
    sql(&db, "update mt set c = 1");
}

/// COMMENT ON CONSTRAINT ... ON DOMAIN: the domain and its constraint have to be there.
#[test]
fn comment_on_a_domain_constraint() {
    let db = Db::new();
    sql(&db, "create domain dpos as int constraint the_constraint check (value > 0)");
    assert_eq!(sql(&db, "comment on constraint the_constraint on domain dpos is 'positive'").tag, "COMMENT");
    sql(&db, "alter domain dpos add constraint inv_ck check (value < 100) not valid");
    assert_eq!(sql(&db, "comment on constraint inv_ck on domain dpos is null").tag, "COMMENT");
    assert_eq!(
        fails(&db, "comment on constraint no_constraint on domain dpos is 'x'"),
        ("42704", "constraint \"no_constraint\" for domain dpos does not exist".to_string())
    );
    assert_eq!(fails(&db, "comment on constraint the_constraint on domain no_dom is 'x'").0, "42704");
}

/// There are no prepared transactions: PREPARE TRANSACTION fails, ending the transaction rolled back (as in
/// Postgres with max_prepared_transactions = 0); COMMIT PREPARED finds none.
#[test]
fn prepare_transaction_ends_the_transaction() {
    let db = people();
    let mut a = chronos::sql::Session::new(MAIN);
    let mut q = |sql: &str| a.run(&db, sql, &[]).map(|mut o| o.pop().unwrap());
    q("begin").unwrap();
    q("insert into people (id, name) values (20, 'p')").unwrap();
    assert_eq!(q("prepare transaction 'x'").unwrap_err().code, "55000");
    assert!(rows(&q("select id from people where id = 20").unwrap()).is_empty(), "rolled back");
    assert_eq!(q("prepare transaction 'y'").unwrap().tag, "ROLLBACK", "outside a transaction: a warning");
    // a failed transaction: it just rolls back
    q("begin").unwrap();
    assert!(q("select 1/0").is_err());
    assert_eq!(q("prepare transaction 'z'").unwrap().tag, "ROLLBACK");
    assert_eq!(q("select 1").unwrap().rows.len(), 1, "no transaction is left open");
    assert_eq!(fails(&db, "commit prepared 'x'").0, "42704");
    assert_eq!(fails(&db, "rollback prepared 'x'").0, "42704");
}

/// ON DELETE SET NULL (columns) / SET DEFAULT (columns): only those of the key's columns are set.
#[test]
fn on_delete_set_column_lists() {
    let db = Db::new();
    sql(&db, "create table pk (tid int, id int, primary key (tid, id))");
    let make = |fk: &str| fails(&db, &format!("create table f0 (tid int, id int, foo int, {fk})"));
    assert_eq!(
        make("foreign key (tid, id) references pk on delete set null (bar)"),
        ("42703", "column \"bar\" referenced in foreign key constraint does not exist".to_string())
    );
    assert_eq!(make("foreign key (tid, id) references pk on delete set null (foo)").0, "42P10");
    assert_eq!(make("foreign key (tid, foo) references pk on update set null (foo)").0, "0A000");
    assert_eq!(
        make("foreign key (nope) references pk").1,
        "column \"nope\" referenced in foreign key constraint does not exist"
    );
    sql(
        &db,
        "create table fk (tid int, id int, a int, b int default 0,
           foreign key (tid, a) references pk on delete set null (a),
           foreign key (tid, b) references pk on delete set default (b, b))",
    );
    assert_eq!(
        rows(&sql(&db, "select pg_get_constraintdef(oid) from pg_constraint where conname = 'fk_tid_a_fkey'")),
        ["FOREIGN KEY (tid, a) REFERENCES pk(tid, id) ON DELETE SET NULL (a)"]
    );
    sql(&db, "insert into pk values (1, 0), (1, 1), (1, 2)");
    sql(&db, "insert into fk values (1, 1, 1, null), (1, 2, null, 2)");
    sql(&db, "delete from pk where id = 1 or id = 2");
    assert_eq!(rows(&sql(&db, "select tid, id, a, b from fk order by id")), ["1|1|null|null", "1|2|null|0"]);
}

/// Small things whose absence stopped a regression file: a name primary key, circle_center,
/// pg_stat_force_next_flush.
#[test]
fn name_keys_circle_center_and_stat_flush() {
    let db = Db::new();
    sql(&db, "create table ua (pguser name primary key, seclv int)");
    sql(&db, "insert into ua values ('alice', 1), ('bob', 2)");
    assert_eq!(rows(&sql(&db, "select seclv from ua where pguser = 'bob'")), ["2"]);
    assert_eq!(rows(&sql(&db, "select circle_center(circle '<(1,2),3>')")), ["(1,2)"]);
    assert_eq!(sql(&db, "select pg_stat_force_next_flush()").rows.len(), 1);
}

/// The cumulative statistics count a session's rows by transaction, its scans, and its function calls, and the
/// statistics functions are named as Postgres names their columns.
#[test]
fn cumulative_statistics_count() {
    let db = Db::new();
    let s = std::cell::RefCell::new(chronos::sql::Session::new(chronos::MAIN));
    let run = |q: &str| s.borrow_mut().run(&db, q, &[]).unwrap().pop().unwrap();
    run("create table cs (id int primary key, v int)");
    run("insert into cs values (1, 1), (2, 2), (3, 3)");
    run("update cs set v = 9 where id = 1");
    run("delete from cs where id = 2");
    run("begin");
    run("insert into cs values (4, 4)");
    assert_eq!(rows(&run("select pg_stat_get_xact_tuples_inserted('cs'::regclass)")), ["1"]);
    run("rollback");
    assert_eq!(
        rows(&run(
            "select n_tup_ins, n_tup_upd, n_tup_del, n_live_tup, n_dead_tup from pg_stat_user_tables where relname = 'cs'"
        )),
        ["4|1|1|2|3"]
    );
    run("select id from cs where v = 3");
    assert_eq!(rows(&run("select seq_scan > 0 from pg_stat_user_tables where relname = 'cs'")), ["true"]);
    run("select v from cs where id = 3");
    assert_eq!(rows(&run("select idx_scan > 0 from pg_stat_user_indexes where indexrelname = 'cs_pkey'")), ["true"]);
    run("begin");
    run("truncate cs");
    run("insert into cs values (5, 5)");
    run("commit");
    assert_eq!(rows(&run("select n_live_tup, n_dead_tup from pg_stat_user_tables where relname = 'cs'")), ["1|0"]);
    run("create function cs_f() returns int language plpgsql as $$ begin return 1; end $$");
    run("set track_functions = 'pl'");
    run("select cs_f()");
    run("select cs_f()");
    assert_eq!(rows(&run("select funcname, calls from pg_stat_user_functions")), ["cs_f|2"]);
    run("drop function cs_f()");
    assert_eq!(rows(&run("select count(*) from pg_stat_user_functions")), ["0"]);
    assert_eq!(column_names(&db, "select pg_stat_force_next_flush()"), ["pg_stat_force_next_flush"]);
    assert_eq!(rows(&run("select count(*) from pg_stat_slru")), ["8"]);
    assert_eq!(rows(&run("select count(*) from pg_stat_io")), ["30"]);
    assert_eq!(s.borrow_mut().run(&db, "select pg_stat_have_stats('zaphod', 0, 0)", &[]).unwrap_err().code, "22023");
    run("checkpoint");
    assert_eq!(rows(&run("select checkpoints_req from pg_stat_bgwriter")), ["1"]);
}

/// The names of a query's columns.
fn column_names(db: &Db, q: &str) -> Vec<String> {
    sql(db, q).columns.unwrap().into_iter().map(|c| c.0).collect()
}

#[test]
fn json_columns_take_the_names_postgres_gives() {
    let db = Db::new();
    assert_eq!(
        column_names(
            &db,
            r#"select jsonb_set('{"a": 1}', '{a}', '2'), json_typeof('1'), jsonb_typeof('1'), to_jsonb(1), row_to_json(row(1)), jsonb_strip_nulls('{}')"#
        ),
        ["jsonb_set", "json_typeof", "jsonb_typeof", "to_jsonb", "row_to_json", "jsonb_strip_nulls"]
    );
    assert_eq!(
        column_names(
            &db,
            r#"select '{"a": 1}'::jsonb @? '$.a', '{"a": 1}'::jsonb #- '{a}', jsonb_path_exists('{"a": 1}', '$.a'), ('{"a": 1}'::jsonb)['a']"#
        ),
        ["?column?", "?column?", "jsonb_path_exists", "jsonb"]
    );
}

#[test]
fn jsonb_operator_functions_and_named_arguments() {
    let db = Db::new();
    let one = |q: &str| rows(&sql(&db, q)).join(",");
    assert_eq!(
        one(
            r#"select jsonb_contains('{"a":"b", "b":1}', '{"a":"b"}'), jsonb_contained('{"a":"b"}', '{"a":"b", "b":1}')"#
        ),
        "true|true"
    );
    assert_eq!(
        one(
            r#"select jsonb_exists('{"a":null}', 'a'), jsonb_exists_any('{"a":1}', array['b','a']), jsonb_exists_all('{"a":1}', array['a','b'])"#
        ),
        "true|true|false"
    );
    assert_eq!(
        one(r#"select jsonb_concat('{"a": 1}', '{"b": 2}'), jsonb_delete('{"a":1, "b":2}'::jsonb, 'a')"#),
        r#"{"a": 1, "b": 2}|{"b": 2}"#
    );
    assert_eq!(one(r#"select jsonb_delete_path('{"a": [1, 2]}', '{a,0}')"#), r#"{"a": [2]}"#);
    assert_eq!(
        one(
            r#"select jsonb_path_exists('1', 'strict $.a', silent => true), jsonb_path_query_first('{"a": 2}', '$.a ? (@ > $x)', vars => '{"x": 1}')"#
        ),
        // (a strict path's error, silenced, is null: Postgres 16 gives null here)
        "null|2"
    );
    assert_eq!(
        one(r#"select jsonb_set_lax('{"a":1,"b":2}', '{b}', null, null_value_treatment => 'delete_key')"#),
        r#"{"a": 1}"#
    );
    assert_eq!(rows(&sql(&db, "select * from jsonb_path_query('[1]', 'strict $[1]', silent => true)")).len(), 0);
    assert_eq!(
        fails(&db, r#"select jsonb_set_lax('{"a":1}', '{a}', null, null_value_treatment => 'raise_exception')"#).0,
        "22004"
    );
    assert_eq!(one(r#"select pg_column_size('{}'::jsonb || '{}'::jsonb) = pg_column_size('{}'::jsonb)"#), "true");
}

/// json keeps its text (stored, looked into, built), apart from jsonb, which normalises it.
#[test]
fn json_is_text_as_written() {
    let db = Db::new();
    sql(&db, "create table jd (id int primary key, j json, b jsonb)");
    sql(&db, r#"insert into jd values (1, '{"b": 1,  "a":[1,2], "b":3}', '{"b": 1,  "a":[1,2], "b":3}')"#);
    let o = sql(&db, "select j, b, j -> 'b', j -> 'a', j #>> '{a,1}', pg_typeof(j), pg_typeof(j -> 'a') from jd");
    assert_eq!(rows(&o), [r#"{"b": 1,  "a":[1,2], "b":3}|{"a": [1, 2], "b": 3}|3|[1,2]|2|json|json"#]);
    let cols = o.columns.clone().unwrap();
    assert_eq!((cols[0].1, cols[1].1), (Type::JsonText, Type::Json));
    // `\u0000` is json's, not jsonb's
    assert_eq!(rows(&sql(&db, r#"select '"\u0000"'::json"#)), [r#""\u0000""#]);
    assert_eq!(fails(&db, r#"select '"\u0000"'::json::jsonb"#).0, "22P05");
    assert_eq!(
        rows(&sql(
            &db,
            "select json_build_object('a', 1, 'b', array[1,2]), json_build_array(1, 'x'), to_json('a'::text)"
        )),
        [r#"{"a" : 1, "b" : [1,2]}|[1, "x"]|"a""#]
    );
    assert_eq!(rows(&sql(&db, "select json_agg(x order by x) from (values (2), (1)) v(x)")), ["[1, 2]"]);
    assert_eq!(
        rows(&sql(&db, "select row_to_json(jd) from jd")),
        [r#"{"id":1,"j":{"b": 1,  "a":[1,2], "b":3},"b":{"a": [1, 2], "b": 3}}"#]
    );
    assert_eq!(rows(&sql(&db, r#"select value from json_array_elements('[1, {"a" :2}]')"#)), ["1", r#"{"a" :2}"#]);
    assert_eq!(fails(&db, "select json_array_length('{}')").1, "cannot get array length of a non-array");
}

#[test]
fn json_input_errors_say_where() {
    let db = Db::new();
    let e = run(&db, MAIN, r#"select '{"abc":1,3}'::jsonb"#, &[]).expect_err("bad json");
    assert_eq!((e.code, e.message.as_str()), ("22P02", "invalid input syntax for type json"));
    let info = e.info.unwrap();
    assert_eq!(info.detail.as_deref(), Some("Expected string, but found \"3\"."));
    assert_eq!(info.context.as_deref(), Some(r#"JSON data, line 1: {"abc":1,3..."#));
    let e = run(&db, MAIN, "select '[1,2'::json", &[]).expect_err("bad json");
    assert_eq!(e.info.unwrap().detail.as_deref(), Some("The input string ended unexpectedly."));
    let e = run(&db, MAIN, r#"select '"\u0000"'::jsonb"#, &[]).expect_err("nul");
    assert_eq!((e.code, e.message.as_str()), ("22P05", "unsupported Unicode escape sequence"));
    let e = run(&db, MAIN, "select repeat('[', 10000)::jsonb", &[]).expect_err("deep");
    assert_eq!(e.code, "54001");
    let e = run(&db, MAIN, r#"select '$ ? (@ like_regex "p" flag "a")'::jsonpath"#, &[]).expect_err("flag");
    assert_eq!(e.message, "invalid input syntax for type jsonpath");
    assert_eq!(e.info.unwrap().detail.as_deref(), Some("Unrecognized flag character \"a\" in LIKE_REGEX predicate."));
}

#[test]
fn jsonb_paths_and_casts_fail_as_postgres_does() {
    let db = Db::new();
    assert_eq!(
        fails(&db, r#"select jsonb_set('{"d": {"1": 2}}', '{d,NULL,0}', '1')"#),
        ("22004", "path element at position 2 is null".into())
    );
    assert_eq!(
        fails(&db, r#"select jsonb_set('{"a": [1, 2, 3]}', '{a, non_integer}', '1')"#),
        ("22P02", "path element at position 2 is not an integer: \"non_integer\"".into())
    );
    assert_eq!(fails(&db, r#"select jsonb_set('"a"', '{a}', '"b"')"#), ("22023", "cannot set path in scalar".into()));
    assert_eq!(rows(&sql(&db, r#"select '[]'::jsonb #- '{a}'"#)), ["[]"]);
    assert_eq!(fails(&db, "select '[1]'::jsonb::boolean"), ("22023", "cannot cast jsonb array to type boolean".into()));
    assert_eq!(
        fails(&db, r#"select '"hello"'::jsonb::int"#),
        ("22023", "cannot cast jsonb string to type integer".into())
    );
    assert_eq!(fails(&db, "select jsonb_array_length('1')").1, "cannot get array length of a scalar");
    assert_eq!(fails(&db, "select jsonb_object_keys('[1]')").1, "cannot call jsonb_object_keys on an array");
    assert_eq!(
        rows(&sql(&db, r#"select ('[1, "2", null]'::jsonb)['a'], ('[1, "2", null]'::jsonb)['1']"#)),
        [r#"null|"2""#]
    );
    assert_eq!(rows(&sql(&db, "select json_object('{{a,1},{b,2}}')")), [r#"{"a" : "1", "b" : "2"}"#]);
    assert_eq!(fails(&db, "select json_object('{{a},{b}}')").1, "array must have two columns");
}

#[test]
fn populate_record_reads_arrays_and_composites() {
    let db = Db::new();
    run(&db, MAIN, "create type pop as (a text, b int); create type rec as (ia int[], r pop, t text)", &[]).unwrap();
    assert_eq!(
        rows(&sql(&db, r#"select ia from jsonb_populate_record(null::rec, '{"ia": [[1, 2], [null, 4]]}')"#)),
        ["{{1,2},{NULL,4}}"]
    );
    assert_eq!(
        rows(&sql(&db, r#"select r from jsonb_populate_record(null::rec, '{"r": {"a": "x y", "b": 3}}')"#)),
        [r#"("x y",3)"#]
    );
    let e =
        run(&db, MAIN, r#"select ia from jsonb_populate_record(null::rec, '{"ia": 123}')"#, &[]).expect_err("scalar");
    assert_eq!(e.message, "expected JSON array");
    assert_eq!(e.info.unwrap().hint.as_deref(), Some("See the value of key \"ia\"."));
    assert_eq!(
        fails(&db, r#"select ia from jsonb_populate_record(null::rec, '{"ia": [[1], [2, 3]]}')"#).1,
        "malformed JSON array"
    );
    assert_eq!(
        fails(&db, r#"select r from jsonb_populate_record(null::rec, '{"r": [1, 2]}')"#).1,
        "cannot call populate_composite on an array"
    );
}

/// An error leaving a PL/pgSQL function says where in it in its CONTEXT, as Postgres's does: the function and the
/// line and kind of statement it failed at, innermost first through the functions and SQL statements it came out
/// of; a RAISE's DETAIL and HINT go with it, and RAISE; raises it with the CONTEXT it had.
#[test]
fn plpgsql_error_context() {
    let db = Db::new();
    let context = |q: &str| run(&db, MAIN, q, &[]).expect_err(q).info.and_then(|i| i.context).unwrap_or_default();
    sql(
        &db,
        "create function ctx_inner(n int) returns int language plpgsql as $$
begin
  if n > 0 then
    raise exception 'too big: %', n using detail = 'some detail', hint = 'some hint';
  end if;
  return n;
end
$$",
    );
    let e = run(&db, MAIN, "select ctx_inner(5)", &[]).unwrap_err();
    assert_eq!(e.message, "too big: 5");
    let info = e.info.unwrap();
    assert_eq!(info.context.as_deref(), Some("PL/pgSQL function ctx_inner(integer) line 4 at RAISE"));
    assert_eq!(info.detail.as_deref(), Some("some detail"));
    assert_eq!(info.hint.as_deref(), Some("some hint"));
    sql(
        &db,
        "create function ctx_outer() returns void language plpgsql as $$\nbegin\n  perform ctx_inner(1);\nend\n$$",
    );
    assert_eq!(
        context("select ctx_outer()"),
        "PL/pgSQL function ctx_inner(integer) line 4 at RAISE\nSQL statement \"SELECT ctx_inner(1)\"\nPL/pgSQL function ctx_outer() line 3 at PERFORM"
    );
    // a variable's default fails at its declaration's line
    assert_eq!(
        context("do $$\ndeclare z int := 0;\n  x int := 1 / z;\nbegin\nend\n$$"),
        "PL/pgSQL function inline_code_block line 3 during statement block local variable initialization"
    );
    // RAISE; again: where it was first raised
    sql(
        &db,
        "create function ctx_again() returns void language plpgsql as $$\nbegin\n  raise 'check me';\nexception when others then\n  raise notice 'caught';\n  raise;\nend\n$$",
    );
    assert_eq!(context("select ctx_again()"), "PL/pgSQL function ctx_again() line 3 at RAISE");
    // an INTO that gets several rows, with Postgres's hint
    sql(&db, "create table ctx_t (a int)");
    sql(&db, "insert into ctx_t values (1), (2)");
    sql(
        &db,
        "create function ctx_strict() returns int language plpgsql as $$\ndeclare x int;\nbegin\n  select a into strict x from ctx_t;\n  return x;\nend\n$$",
    );
    let e = run(&db, MAIN, "select ctx_strict()", &[]).unwrap_err();
    let info = e.info.unwrap();
    assert_eq!(info.hint.as_deref(), Some("Make sure the query returns a single row, or use LIMIT 1."));
    assert_eq!(info.context.as_deref(), Some("PL/pgSQL function ctx_strict() line 4 at SQL statement"));
    // GET STACKED DIAGNOSTICS ... PG_EXCEPTION_CONTEXT: the functions it came out of, and this one
    sql(
        &db,
        "create function ctx_stacked() returns text language plpgsql as $$\ndeclare c text;\nbegin\n  perform ctx_inner(2);\nexception when others then\n  get stacked diagnostics c = pg_exception_context;\n  return c;\nend\n$$",
    );
    assert_eq!(
        rows(&sql(&db, "select ctx_stacked()")),
        [
            "PL/pgSQL function ctx_inner(integer) line 4 at RAISE\nSQL statement \"SELECT ctx_inner(2)\"\nPL/pgSQL function ctx_stacked() line 4 at PERFORM"
        ]
    );
}

/// pg_get_viewdef (and pg_views, pg_get_triggerdef's WHEN) write a query back as Postgres 16 writes the parsed query:
/// columns qualified where more than one relation is read, `*` as its columns, each item on a line of its own, and
/// (not pretty) every operation in parentheses.
#[test]
fn view_definitions_written_back() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table sd (shoename char(10), sh_avail int, slunit char(8), slminlen float);
         create table un (un_name char(8), un_fact float);
         create view shoe as select sh.shoename, sh.sh_avail, sh.slminlen * un.un_fact as slminlen_cm, sh.slunit
           from sd sh, un where sh.slunit = un.un_name;
         create table tt1 (f1 int, f2 int, f3 text);
         create table tx1 (x1 int, x2 int, x3 text);
         create view av as select * from tt1 where exists (select 1 from tx1 where tt1.f1 = tx1.x1);
         create view vv as values (1, 2);
         create view vx(x) as select * from (values (1, 2)) v(q, w);
         create function tgf() returns trigger language plpgsql as $$ begin return new; end $$;
         create trigger t1 before update on tt1 for each row when (old.f1 <> new.f1) execute function tgf();",
        &[],
    )
    .unwrap();
    let def = |q: &str| rows(&sql(&db, q)).join("\n");
    assert_eq!(
        def("select pg_get_viewdef('shoe')"),
        " SELECT sh.shoename,\n    sh.sh_avail,\n    (sh.slminlen * un.un_fact) AS slminlen_cm,\n    sh.slunit\n   FROM sd sh,\n    un\n  WHERE (sh.slunit = un.un_name);"
    );
    assert_eq!(
        def("select pg_get_viewdef('shoe', true)"),
        " SELECT sh.shoename,\n    sh.sh_avail,\n    sh.slminlen * un.un_fact AS slminlen_cm,\n    sh.slunit\n   FROM sd sh,\n    un\n  WHERE sh.slunit = un.un_name;"
    );
    assert_eq!(
        def("select pg_get_viewdef('av', true)"),
        " SELECT f1,\n    f2,\n    f3\n   FROM tt1\n  WHERE (EXISTS ( SELECT 1\n           FROM tx1\n          WHERE tt1.f1 = tx1.x1));"
    );
    assert_eq!(
        def("select definition from pg_views where viewname = 'av'"),
        " SELECT f1,\n    f2,\n    f3\n   FROM tt1\n  WHERE (EXISTS ( SELECT 1\n           FROM tx1\n          WHERE (tt1.f1 = tx1.x1)));"
    );
    assert_eq!(def("select pg_get_viewdef('vv')"), " VALUES (1,2);");
    assert_eq!(def("select pg_get_viewdef('vx', true)"), " SELECT q AS x,\n    w\n   FROM ( VALUES (1,2)) v(q, w);");
    assert_eq!(
        def("select pg_get_triggerdef(oid, true), pg_get_triggerdef(oid) from pg_trigger where tgname = 't1'"),
        "CREATE TRIGGER t1 BEFORE UPDATE ON tt1 FOR EACH ROW WHEN (old.f1 <> new.f1) EXECUTE FUNCTION tgf()|\
         CREATE TRIGGER t1 BEFORE UPDATE ON public.tt1 FOR EACH ROW WHEN ((old.f1 <> new.f1)) EXECUTE FUNCTION tgf()"
    );
    // (and the views and rules of Postgres's own catalog, as Postgres 16 has them)
    assert_eq!(def("select count(*) from pg_views where schemaname = 'pg_catalog'"), "76");
    assert_eq!(def("select count(*) from pg_rules where tablename = 'pg_settings'"), "2");
}

#[test]
fn foreign_data_wrappers_servers_and_user_mappings() {
    let db = Db::new();
    sql(&db, "create foreign data wrapper dummy");
    sql(&db, "create foreign data wrapper pg validator postgresql_fdw_validator");
    assert_eq!(
        fails(&db, "create foreign data wrapper dummy"),
        ("42710", "foreign-data wrapper \"dummy\" already exists".into())
    );
    assert_eq!(fails(&db, "create foreign data wrapper x validator bar").1, "function bar(text[], oid) does not exist");
    assert_eq!(fails(&db, "create foreign data wrapper x handler h handler i").1, "conflicting or redundant options");
    assert_eq!(fails(&db, "alter foreign data wrapper dummy").0, "42601");
    // generic options: ADD, SET and DROP, in place
    sql(&db, "create foreign data wrapper foo options (a '1', b '2')");
    sql(&db, "alter foreign data wrapper foo options (drop a, set b '3', add c '4')");
    assert_eq!(rows(&sql(&db, "select fdwoptions from pg_foreign_data_wrapper where fdwname = 'foo'")), ["{b=3,c=4}"]);
    assert_eq!(fails(&db, "alter foreign data wrapper foo options (b '4')").1, "option \"b\" provided more than once");
    assert_eq!(fails(&db, "alter foreign data wrapper foo options (set z '1')").1, "option \"z\" not found");
    assert_eq!(
        rows(&sql(
            &db,
            "select fdwname, fdwhandler::regproc, fdwvalidator::regproc from pg_foreign_data_wrapper order by 1"
        )),
        ["dummy|-|-", "foo|-|-", "pg|-|postgresql_fdw_validator"]
    );
    let e = run(&db, MAIN, "alter foreign data wrapper pg options (x '1')", &[]).expect_err("no options");
    assert_eq!(e.info.unwrap().hint.as_deref(), Some("There are no valid options in this context."));
    // servers, checked by their wrapper's validator
    sql(&db, "create server s1 type 'oracle' version '1.0' foreign data wrapper pg options (host 'a', dbname 'b')");
    assert_eq!(
        fails(&db, "create server s2 foreign data wrapper pg options (foo '1')"),
        ("42601", "invalid option \"foo\"".into())
    );
    assert_eq!(
        fails(&db, "create server s2 foreign data wrapper nope").1,
        "foreign-data wrapper \"nope\" does not exist"
    );
    assert_eq!(fails(&db, "create server s1 foreign data wrapper pg").0, "42710");
    sql(&db, "create server if not exists s1 foreign data wrapper pg");
    sql(&db, "alter server s1 version '1.1' options (drop host, add port '5')");
    assert_eq!(
        rows(&sql(
            &db,
            "select s.srvname, s.srvtype, s.srvversion, s.srvoptions, f.fdwname \
             from pg_foreign_server s join pg_foreign_data_wrapper f on f.oid = s.srvfdw"
        )),
        ["s1|oracle|1.1|{dbname=b,port=5}|pg"]
    );
    // user mappings
    sql(&db, "create user mapping for current_user server s1 options (user 'u')");
    assert_eq!(
        fails(&db, "create user mapping for user server s1").1,
        "user mapping for \"chronos\" already exists for server \"s1\""
    );
    sql(&db, "create user mapping if not exists for current_user server s1");
    let e = run(&db, MAIN, "create user mapping for public server s1 options (username 'x')", &[]).expect_err("option");
    assert_eq!(e.message, "invalid option \"username\"");
    assert_eq!(e.info.unwrap().hint.as_deref(), Some("Perhaps you meant the option \"user\"."));
    sql(&db, "create user mapping for public server s1");
    assert_eq!(
        rows(&sql(&db, "select srvname, usename, umoptions from pg_user_mappings order by 2")),
        ["s1|chronos|{user=u}", "s1|public|null"]
    );
    assert_eq!(fails(&db, "drop user mapping for public server nope").1, "server \"nope\" does not exist");
    sql(&db, "drop user mapping if exists for public server nope");
    assert_eq!(fails(&db, "drop foreign data wrapper pg").0, "2BP01");
    sql(&db, "drop server s1 cascade");
    assert_eq!(rows(&sql(&db, "select count(*) from pg_user_mapping")), ["0"]);
    // foreign tables: tables whose rows can't be read
    sql(&db, "create server s0 foreign data wrapper dummy");
    assert_eq!(
        fails(&db, "create foreign table ft (a int primary key) server s0").1,
        "primary key constraints are not supported on foreign tables"
    );
    assert_eq!(fails(&db, "create foreign table ft () server nope").1, "server \"nope\" does not exist");
    assert_eq!(fails(&db, "create foreign table ft ()").0, "42601");
    sql(&db, "create foreign table ft (a int options (col 'x') not null, b text) server s0 options (t '1')");
    assert_eq!(rows(&sql(&db, "select relkind from pg_class where relname = 'ft'")), ["f"]);
    assert_eq!(
        rows(&sql(
            &db,
            "select s.srvname, f.ftoptions from pg_foreign_table f join pg_foreign_server s on s.oid = f.ftserver"
        )),
        ["s0|{t=1}"]
    );
    assert_eq!(
        rows(&sql(
            &db,
            "select attname, attfdwoptions from pg_attribute where attrelid = 'ft'::regclass and attnum > 0 order by attnum"
        )),
        ["a|{col=x}", "b|null"]
    );
    assert_eq!(fails(&db, "select * from ft"), ("55000", "foreign-data wrapper \"dummy\" has no handler".into()));
    sql(&db, "alter foreign table ft options (set t '2')");
    sql(&db, "alter foreign table ft alter column b options (add k 'v')");
    assert_eq!(rows(&sql(&db, "select ftoptions from pg_foreign_table")), ["{t=2}"]);
    assert_eq!(
        rows(&sql(&db, "select attfdwoptions from pg_attribute where attrelid = 'ft'::regclass and attname = 'b'")),
        ["{k=v}"]
    );
    assert_eq!(fails(&db, "create index on ft (a)").1, "cannot create index on relation \"ft\"");
    assert_eq!(
        rows(&sql(&db, "select foreign_table_name, foreign_server_name from information_schema.foreign_tables")),
        ["ft|s0"]
    );
    assert_eq!(fails(&db, "drop server s0").0, "2BP01");
    assert_eq!(fails(&db, "drop table ft").0, "42809");
    sql(&db, "drop foreign table ft");
    assert_eq!(fails(&db, "drop foreign table ft").1, "foreign table \"ft\" does not exist");
    sql(&db, "drop foreign data wrapper dummy cascade");
    assert_eq!(rows(&sql(&db, "select count(*) from pg_foreign_server")), ["0"]);
}

/// READ ONLY transactions refuse writes with Postgres's messages (25006, 25001), and let temporary tables change.
#[test]
fn read_only_transactions_refuse_writes() {
    let db = Db::new();
    let mut s = chronos::sql::Session::new(MAIN);
    let mut q = |sql: &str| s.run(&db, sql, &[]).map(|_| ()).map_err(|e| (e.code, e.message));
    q("create table rot (a int)").unwrap();
    q("begin read only").unwrap();
    assert_eq!(
        q("insert into rot values (1)"),
        Err(("25006", "cannot execute INSERT in a read-only transaction".to_string()))
    );
    q("rollback").unwrap();
    q("begin read only").unwrap();
    assert_eq!(q("drop table rot"), Err(("25006", "cannot execute DROP TABLE in a read-only transaction".to_string())));
    q("rollback").unwrap();
    q("create temp table rot_tmp (a int)").unwrap();
    q("begin read only").unwrap();
    q("insert into rot_tmp values (1)").unwrap();
    q("select 1").unwrap();
    assert_eq!(
        q("set transaction read write"),
        Err(("25001", "transaction read-write mode must be set before any query".to_string()))
    );
    q("rollback").unwrap();
    q("insert into rot values (2)").unwrap();
}

/// EXPLAIN without ANALYZE runs a query only to see how it reads its tables, where Postgres doesn't run it: what its
/// functions RAISE isn't sent (row security's f_leak).
#[test]
fn explain_sends_no_notices_of_the_query() {
    let db = people();
    sql(
        &db,
        "create function loud(t text) returns boolean language plpgsql as $$ begin raise notice 'saw %', t; return true; end $$",
    );
    let _ = chronos::sql::take_notices();
    sql(&db, "explain select * from people where loud(name)");
    assert!(chronos::sql::take_notices().is_empty());
    sql(&db, "explain update people set age = 1 where loud(name)");
    assert!(chronos::sql::take_notices().is_empty());
    sql(&db, "select * from people where loud(name)");
    assert_eq!(chronos::sql::take_notices().len(), 4);
}

/// Runs `q` in session `s`, giving its error's SQLSTATE, message and DETAIL.
fn session_fails(s: &mut chronos::sql::Session, db: &Db, q: &str) -> (&'static str, String, Option<String>) {
    let e = s.run(db, q, &[]).expect_err(q);
    let detail = e.info.as_ref().and_then(|i| i.detail.clone());
    (e.code, e.message, detail)
}

/// Runs `q` in session `s`, which must succeed.
fn session_ok(s: &mut chronos::sql::Session, db: &Db, q: &str) -> Output {
    s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {} {}", e.code, e.message)).pop().unwrap()
}

#[test]
fn foreign_keys_as_postgres_reports_them() {
    let (db, mut s) = (Db::new(), chronos::sql::Session::new(MAIN));
    let s = &mut s;
    session_ok(s, &db, "create table pk (a int, b int, primary key (a, b))");
    session_ok(s, &db, "create table fk (a int, b int, foreign key (a, b) references pk)");
    session_ok(s, &db, "insert into pk values (1, 2)");
    session_ok(s, &db, "insert into fk values (1, 2)");
    // a key of several columns: each value, not the row's id
    let (code, _, detail) = session_fails(s, &db, "delete from pk");
    assert_eq!(code, "23503");
    assert_eq!(detail.as_deref(), Some("Key (a, b)=(1, 2) is still referenced from table \"fk\"."));
    // types that don't go together: the pair as the statement wrote it
    session_ok(s, &db, "create table pk1 (id int primary key)");
    let (code, message, detail) = session_fails(s, &db, "create table bad (x inet references pk1)");
    assert_eq!((code, message.as_str()), ("42804", "foreign key constraint \"bad_x_fkey\" cannot be implemented"));
    assert_eq!(detail.as_deref(), Some("Key columns \"x\" and \"id\" are of incompatible types: inet and integer."));
    let (_, _, detail) =
        session_fails(s, &db, "create table bad (x int, y inet, foreign key (x, y) references pk (b, a))");
    assert_eq!(detail.as_deref(), Some("Key columns \"y\" and \"a\" are of incompatible types: inet and integer."));
    // ALTER CONSTRAINT's refusals
    let (code, message, _) =
        session_fails(s, &db, "alter table fk alter constraint fk_a_b_fkey not deferrable initially deferred");
    assert_eq!((code, message.as_str()), ("42601", "constraint declared INITIALLY DEFERRED must be DEFERRABLE"));
    let (_, message, _) = session_fails(s, &db, "alter table fk alter constraint fk_a_b_fkey no inherit");
    assert_eq!(message, "FOREIGN KEY constraints cannot be marked NO INHERIT");
    let (_, message, _) = session_fails(s, &db, "alter table fk alter constraint fk_a_b_fkey not valid");
    assert_eq!(message, "FOREIGN KEY constraints cannot be marked NOT VALID");
    // a deferred check follows its row when the row's key changes, and holds through a released savepoint
    session_ok(s, &db, "insert into pk1 values (5)");
    session_ok(s, &db, "create table fkd (id int primary key, f int references pk1 deferrable initially deferred)");
    session_ok(s, &db, "begin");
    session_ok(s, &db, "insert into fkd values (0, 20)");
    session_ok(s, &db, "update fkd set id = id + 1");
    assert_eq!(session_fails(s, &db, "commit").0, "23503");
    session_ok(s, &db, "begin");
    session_ok(s, &db, "savepoint sp");
    session_ok(s, &db, "insert into fkd values (0, 20)");
    session_ok(s, &db, "release savepoint sp");
    assert_eq!(session_fails(s, &db, "commit").0, "23503");
    // and the constraint can't be dropped while its check is put off
    session_ok(s, &db, "begin");
    session_ok(s, &db, "insert into fkd values (2, 20)");
    let (code, message, _) = session_fails(s, &db, "alter table fkd drop constraint fkd_f_fkey");
    assert_eq!((code, message.as_str()), ("55006", "cannot ALTER TABLE \"fkd\" because it has pending trigger events"));
    session_ok(s, &db, "rollback");
    // the same for temporary tables, whose rows are in the session's own database
    session_ok(s, &db, "create temp table tpk (id int primary key)");
    session_ok(
        s,
        &db,
        "create temp table tfk (id int primary key, f int references tpk deferrable initially deferred)",
    );
    session_ok(s, &db, "begin");
    session_ok(s, &db, "insert into tfk values (0, 20)");
    assert_eq!(session_fails(s, &db, "commit").0, "23503");
    session_ok(s, &db, "begin");
    session_ok(s, &db, "savepoint sp");
    session_ok(s, &db, "insert into tfk values (0, 20)");
    session_ok(s, &db, "release savepoint sp");
    assert_eq!(session_fails(s, &db, "commit").0, "23503");
    session_ok(s, &db, "begin");
    session_ok(s, &db, "commit");
    // a partitioned table's foreign key: the referencing rows are its, as Postgres names them
    session_ok(s, &db, "create table ppk (a int primary key)");
    session_ok(s, &db, "create table pfk (a int references ppk) partition by list (a)");
    session_ok(s, &db, "create table pfk1 partition of pfk for values in (1)");
    session_ok(s, &db, "insert into ppk values (1)");
    session_ok(s, &db, "insert into pfk values (1)");
    let (_, _, detail) = session_fails(s, &db, "delete from ppk");
    assert_eq!(detail.as_deref(), Some("Key (a)=(1) is still referenced from table \"pfk\"."));
    let (code, message, _) = session_fails(s, &db, "alter table only pfk add foreign key (a) references ppk");
    assert_eq!(code, "42809");
    assert_eq!(message, "cannot use ONLY for foreign key on partitioned table \"pfk\" referencing relation \"ppk\"");
    let (_, message, detail) = session_fails(s, &db, "alter table pfk add foreign key (a) references ppk not valid");
    assert_eq!(message, "cannot add NOT VALID foreign key on partitioned table \"pfk\" referencing relation \"ppk\"");
    assert_eq!(detail.as_deref(), Some("This feature is not yet supported on partitioned tables."));
    // names cut to 63 bytes as Postgres cuts them, the longer part first
    session_ok(s, &db, "create table fktable2 (a_very_very_long_column_name_to_exceed_63_characters int)");
    for _ in 0..2 {
        session_ok(
            s,
            &db,
            "alter table fktable2 add foreign key (a_very_very_long_column_name_to_exceed_63_characters) references pk1",
        );
    }
    let mut names = rows(&session_ok(
        s,
        &db,
        "select conname from pg_constraint where conrelid = 'fktable2'::regclass and contype = 'f'",
    ));
    names.sort();
    assert_eq!(
        names,
        [
            "fktable2_a_very_very_long_column_name_to_exceed_63_charac_fkey1",
            "fktable2_a_very_very_long_column_name_to_exceed_63_charact_fkey",
        ]
    );
}

#[test]
fn domains_refuse_values_as_postgres_does() {
    let (db, mut s) = (Db::new(), chronos::sql::Session::new(MAIN));
    let s = &mut s;
    for q in [
        "create domain posint as int check (value > 0)",
        "create domain posint2 as posint check (value < 10)",
        "create table dt (a posint2)",
        "create domain nn as int not null",
        "create table dn (a nn)",
        "create domain dd as int constraint not_three check (value <> 3) check (value <> 4)",
        "create table ddt (a dd)",
    ] {
        session_ok(s, &db, q);
    }
    // a domain over a domain: the base one's check too, the error naming the domain the value is made
    let (code, message, detail) = session_fails(s, &db, "insert into dt values (0)");
    assert_eq!(
        (code, message.as_str()),
        ("23514", "value for domain posint2 violates check constraint \"posint_check\"")
    );
    assert_eq!(detail, None);
    let (_, message, _) = session_fails(s, &db, "insert into dt values (11)");
    assert_eq!(message, "value for domain posint2 violates check constraint \"posint2_check\"");
    session_ok(s, &db, "insert into dt values (5)");
    let (code, message, _) = session_fails(s, &db, "insert into dn values (null)");
    assert_eq!((code, message.as_str()), ("23502", "domain nn does not allow null values"));
    let (_, message, _) = session_fails(s, &db, "insert into ddt values (3)");
    assert_eq!(message, "value for domain dd violates check constraint \"not_three\"");
    let (_, message, _) = session_fails(s, &db, "insert into ddt values (4)");
    assert_eq!(message, "value for domain dd violates check constraint \"dd_check\"");
}

#[test]
fn records_read_and_compared_as_postgres_does() {
    let db = Db::new();
    sql(&db, "create type fullname as (first text, last text)");
    for (q, detail) in [
        ("select '(Joe)'::fullname", "Too few columns."),
        ("select '(Joe,,)'::fullname", "Too many columns."),
        ("select '[]'::fullname", "Missing left parenthesis."),
        ("select '(Joe,Blow) /'::fullname", "Junk after right parenthesis."),
    ] {
        let e = run(&db, MAIN, q, &[]).expect_err(q);
        assert_eq!(e.code, "22P02", "{q}");
        assert_eq!(e.info.as_ref().and_then(|i| i.detail.as_deref()), Some(detail), "{q}");
    }
    assert_eq!(
        rows(&sql(&db, r#"select '(Joe,"von Blow")'::fullname, '  (Joe,Blow)  '::fullname"#)),
        [r#"(Joe,"von Blow")|(Joe,Blow)"#]
    );
    // an anonymous row's fields are f1, f2, ...
    assert_eq!(rows(&sql(&db, "select (row(1, 2.0)).f1, (row(1, 2.0)).f2")), ["1|2.0"]);
    assert_eq!(fails(&db, "select (row(1, 2.0)).nosuch").1, "could not identify column \"nosuch\" in record data type");
    assert_eq!(fails(&db, "select row() = row()"), ("0A000", "cannot compare rows of zero length".into()));
    // record image comparisons: the fields' stored bytes, an integer's as unsigned
    sql(&db, "create type tt1 as (a int, b int)");
    sql(&db, "create type tt3 as (a int, b text)");
    sql(&db, "create type tt5 as (a int)");
    assert_eq!(
        rows(&sql(
            &db,
            "select row(1, 2)::tt1 *< row(1, 3)::tt1, row(1, -2)::tt1 *< row(1, 3)::tt1, row(1, 2)::tt1 *= row(1, 2)::tt1, \
             row(1, 2)::tt1 *<> row(1, 3)::tt1, row(1, 3)::tt1 *>= row(1, 2)::tt1, row(1, 'abc')::tt3 *< row(1, 'abcd')::tt3"
        )),
        ["true|false|true|true|true|true"]
    );
    assert_eq!(
        fails(&db, "select row(1, 2)::tt1 *< row(1, 'abc')::tt3").1,
        "cannot compare dissimilar column types integer and text at record column 2"
    );
    assert_eq!(
        fails(&db, "select row(1, 2)::tt1 *<> row(1)::tt5").1,
        "cannot compare record types with different numbers of columns"
    );
    // (a product is still a product)
    assert_eq!(rows(&sql(&db, "select 2*3, 2 * -3")), ["6|-6"]);
}

#[test]
fn checks_tried_by_name() {
    let db = Db::new();
    sql(&db, "create table ct (x int, z int, constraint ct_con check (x >= 3), check (x + z = 0))");
    let (code, message) = fails(&db, "insert into ct values (1, 1)");
    assert_eq!(code, "23514");
    assert!(message.contains("\"ct_check\""), "{message}");
    sql(&db, "create table un (i int unique nulls not distinct)");
    sql(&db, "insert into un values (null)");
    let e = run(&db, MAIN, "insert into un values (null)", &[]).expect_err("a second null");
    assert_eq!(e.info.as_ref().and_then(|i| i.detail.as_deref()), Some("Key (i)=(null) already exists."));
}

#[test]
fn views_dropped_with_a_table_are_listed_as_postgres_lists_them() {
    let db = Db::new();
    sql(&db, "create table dv_t (a int)");
    sql(&db, "create view dv_b as select * from dv_t");
    sql(&db, "create view dv_a as select * from dv_t");
    sql(&db, "create view dv_c as select * from dv_a");
    let _ = chronos::sql::take_notices();
    sql(&db, "drop table dv_t cascade");
    let notices: Vec<(String, Option<String>)> =
        chronos::sql::take_notices().into_iter().map(|n| (n.message, n.detail)).collect();
    assert_eq!(
        notices,
        [(
            "drop cascades to 3 other objects".to_string(),
            Some("drop cascades to view dv_b\ndrop cascades to view dv_a\ndrop cascades to view dv_c".to_string())
        )]
    );
}

#[test]
fn regress_library_interpt_pp_and_leaky_views() {
    let db = Db::new();
    sql(&db, "create function interpt_pp(path, path) returns point as '/tmp/lib/regress.so' language c strict");
    assert_eq!(rows(&sql(&db, "select interpt_pp('[(0,0),(2,2)]', '[(0,2),(2,0)]')")), ["(1,1)"]);
    assert_eq!(rows(&sql(&db, "select interpt_pp('[(0,0),(1,0)]', '[(0,5),(1,5)]')")), ["null"]);
    assert_eq!(fails(&db, "create function nope(int) returns int as '/tmp/lib/regress.so' language c").0, "0A000");
    sql(
        &db,
        "create function lk(text) returns bool language plpgsql cost 0.0000001 as 'begin raise notice ''lk %'', $1; return true; end'",
    );
    sql(&db, "create table lk_t (id int primary key, who text, secret text)");
    sql(&db, "insert into lk_t values (1, 'me', 's1'), (2, 'you', 's2'), (3, 'them', 's3')");
    sql(&db, "create view lk_plain as select * from lk_t where length(who) = 2");
    sql(&db, "create view lk_safe with (security_barrier) as select * from lk_t where length(who) = 2");
    let said = || -> Vec<String> { chronos::sql::take_notices().into_iter().map(|n| n.message).collect() };
    let _ = said();
    // a plain view is pulled up: the cheap leaky function runs first, on every row
    assert_eq!(rows(&sql(&db, "select id from lk_plain where lk(secret)")), ["1"]);
    assert_eq!(said(), ["lk s1", "lk s2", "lk s3"]);
    // a security_barrier view's own condition runs first
    assert_eq!(rows(&sql(&db, "select id from lk_safe where lk(secret)")), ["1"]);
    assert_eq!(said(), ["lk s1"]);
    // EXPLAIN without ANALYZE runs nothing anyone sees
    sql(&db, "explain select id from lk_plain where lk(secret)");
    assert!(said().is_empty());
    sql(&db, "alter view lk_safe set (security_barrier = false)");
    assert_eq!(rows(&sql(&db, "select id from lk_safe where lk(secret)")), ["1"]);
    assert_eq!(said(), ["lk s1", "lk s2", "lk s3"]);
}

/// EXPLAIN without ANALYZE takes no sequence values, as Postgres's doesn't run the query; EXECUTE's argument that
/// its parameter can't take says so as Postgres does.
#[test]
fn explain_takes_no_sequence_values() {
    let db = Db::new();
    let mut s = chronos::sql::Session::new(MAIN);
    let mut q = |sql: &str| s.run(&db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).pop().unwrap();
    q("create sequence es");
    q("explain select nextval('es')");
    assert_eq!(rows(&q("select nextval('es')")), ["1"]);
    q("prepare pq(float8) as select $1");
    let e = s.run(&db, "execute pq(true)", &[]).expect_err("a boolean for a float8");
    assert_eq!(
        (e.code, e.message.as_str()),
        ("42804", "parameter $1 of type boolean cannot be coerced to the expected type double precision")
    );
}

/// vacuum_parallel's sizes: a b-tree on a constant is deduplicated, as Postgres's is, so it is under 128kB where
/// one on 10000 distinct values is over it.
#[test]
fn a_deduplicated_index_is_smaller() {
    let db = Db::new();
    sql(&db, "create table pvt (a int)");
    sql(&db, "insert into pvt select i from generate_series(1, 10000) i");
    sql(&db, "create index pvt_regular on pvt (a)");
    sql(&db, "create index pvt_small on pvt ((1))");
    let o = sql(&db, "select pg_relation_size('pvt_small') < 131072, pg_relation_size('pvt_regular') >= 131072");
    assert_eq!(rows(&o), ["true|true"]);
}

/// What can't run inside a transaction block: a concurrent index build or drop, a REINDEX of a partitioned table
/// or of a schema.
#[test]
fn concurrent_index_statements_refuse_transactions() {
    let db = Db::new();
    sql(&db, "create table ct (a int)");
    sql(&db, "create table cp (a int) partition by range (a)");
    let mut s = chronos::sql::Session::new(MAIN);
    let mut q = |sql: &str| s.run(&db, sql, &[]).map(|mut o| o.pop().unwrap());
    for (stmt, message) in [
        ("create index concurrently ct_i on ct (a)", "CREATE INDEX CONCURRENTLY cannot run inside a transaction block"),
        ("drop index concurrently ct_j", "DROP INDEX CONCURRENTLY cannot run inside a transaction block"),
        ("reindex table cp", "REINDEX TABLE cannot run inside a transaction block"),
        ("reindex schema public", "REINDEX SCHEMA cannot run inside a transaction block"),
    ] {
        q("begin").unwrap();
        let e = q(stmt).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), ("25001", message), "{stmt}");
        q("rollback").unwrap();
    }
    q("create index ct_j on ct (a)").unwrap();
    q("begin").unwrap();
    q("create index ct_k on ct (a)").unwrap();
    q("reindex table ct").unwrap();
    q("commit").unwrap();
}

/// Batch 10 (bit, int8, inet, text, strings, regex, char, name, case, arrays, enum, ranges): column names as
/// Postgres's FigureColname gives them, and values and errors as Postgres gives them.
#[test]
fn exact_types_names_and_errors() {
    let db = Db::new();
    let names = |q: &str| -> Vec<String> { sql(&db, q).columns.unwrap().into_iter().map(|(n, _)| n).collect() };
    assert_eq!(names("select float8(2), int4('3'), text(4)"), ["float8", "int4", "text"]);
    assert_eq!(names("select substring('abc' from 2), substr('abc', 2)"), ["substring", "substr"]);
    assert_eq!(names("select case when true then 1 end, (case when true then 1 end)::text"), ["case", "text"]);
    assert_eq!(names("select case when true then array[1] else array[2] end"), ["array"]);
    assert_eq!(names("select '{1,2}'::int[], 'a'::\"char\", 'ab'::char(3)"), ["int4", "char", "bpchar"]);
    assert_eq!(names("select position(B'1' in B'0101')"), ["position"]);
    assert_eq!(
        rows(&sql(&db, "select position(B'' in B''), position(B'' in B'1'), position(B'01' in B'1101')")),
        ["0|1|3"]
    );
    assert_eq!(rows(&sql(&db, "select bit_count('\\x1234567890'::bytea)")), ["15"]);
    // "char": one byte, octal escapes in and out
    assert_eq!(
        rows(&sql(&db, "select '\\101'::\"char\", '\\377'::\"char\", '\\000'::\"char\"::text, 'abc'::\"char\"")),
        ["A|\\377||a"]
    );
    // a char(n) cast is shown blank-padded, and is its text without the blanks elsewhere
    assert_eq!(rows(&sql(&db, "select cast('ab' as char(4)) || '|'")), ["ab|"]);
    assert_eq!(rows(&sql(&db, "select cast('ab' as char(4))")), ["ab  "]);
    // VARIADIC arrays not written out
    assert_eq!(
        rows(&sql(&db, "select concat(variadic null::int[]) is null, concat(variadic '{}'::int[]) = ''")),
        ["true|true"]
    );
    assert_eq!(
        rows(&sql(&db, "select concat_ws(',', variadic array[1, 2]), format('%s-%s', variadic array['a', 'b'])")),
        ["1,2|a-b"]
    );
    assert_eq!(fails(&db, "select concat_ws(',', variadic 10)").0, "42804");
    // parse_ident as misc.c parses
    assert_eq!(rows(&sql(&db, "select parse_ident('foo.boo[]', strict => false)")), ["{foo,boo}"]);
    assert_eq!(fails(&db, "select parse_ident('foo.boo[]')").0, "22023");
    // a string meeting a name is a name, cut to 63 bytes
    sql(&db, "create table name_t (f1 name)");
    sql(&db, "insert into name_t values (repeat('x', 70))");
    assert_eq!(rows(&sql(&db, &format!("select count(*) from name_t where f1 = '{}'", "x".repeat(64)))), ["1"]);
    // regexp_match's g flag, escape strings
    assert_eq!(fails(&db, "select regexp_match('abc', 'Bd', 'ig')").0, "22023");
    assert_eq!(fails(&db, "select 'abcdefg' similar to '_bcd#%' escape '##'").0, "22025");
    // enums: names, labels, input checks
    sql(&db, "create type rainbow as enum ('red', 'orange')");
    assert_eq!(
        names("select 'red'::rainbow, enum_first(null::rainbow), enum_range(null::rainbow)"),
        ["rainbow", "enum_first", "enum_range"]
    );
    assert_eq!(
        rows(&sql(&db, "select pg_input_is_valid('mauve', 'rainbow'), pg_input_is_valid('red', 'rainbow')")),
        ["false|true"]
    );
    assert_eq!(fails(&db, &format!("alter type rainbow add value '{}'", "p".repeat(64))).0, "42602");
    // arrays: min and max in array order, width_bucket over thresholds, _int4, char(n)[]
    assert_eq!(
        rows(&sql(
            &db,
            "select max(a), min(a) from (values ('{1,2,3}'::int[]), ('{1,2,3,4}'::int[]), ('{}'::int[])) v(a)"
        )),
        ["{1,2,3,4}|{}"]
    );
    assert_eq!(
        rows(&sql(&db, "select width_bucket(5, array[1, 3, 5, 10]), width_bucket(0, array[1]), width_bucket(5, '{}')")),
        ["3|0|0"]
    );
    assert_eq!(fails(&db, "select width_bucket(5, array[3, 4, null])").0, "22004");
    assert_eq!(rows(&sql(&db, "select '{1,2}'::_int4, array_remove(array[1.0, 2.1, 3.3], 1)")), ["{1,2}|{2.1,3.3}"]);
    assert_eq!(
        rows(&sql(
            &db,
            "select array_length(array_sample('{1,2,3,4,5,6}'::int[], 3), 1), \
             array_dims(array_shuffle('[-1:2][2:3]={{1,2},{3,NULL},{5,6},{7,8}}'::int[]))"
        )),
        ["3|[-1:2][2:3]"]
    );
    sql(&db, "create table arr_t (i integer array[4], f char(5)[])");
    sql(&db, "insert into arr_t values ('{1,2}', '{\"abc\",abcde}')");
    assert_eq!(rows(&sql(&db, "select i, f from arr_t")), ["{1,2}|{\"abc  \",abcde}"]);
    // ranges: the operators' functions, multirange(range), multirange order
    assert_eq!(
        rows(&sql(
            &db,
            "select range_adjacent(numrange(2.0, 3.0), numrange(3.0, 4.0)), range_minus(int4range(1, 10), int4range(5, 20))"
        )),
        ["true|[1,5)"]
    );
    assert_eq!(names("select range_contains(int4range(1, 5), int4range(2, 3))"), ["range_contains"]);
    assert_eq!(
        rows(&sql(
            &db,
            "select int4range(1, 3)::int4multirange, 'empty'::int4range::int4multirange, multirange(int4range(1, 2))"
        )),
        ["{[1,3)}|{}|{[1,2)}"]
    );
    assert_eq!(rows(&sql(&db, "select '{[1,2)}'::int4multirange < '{[1,3)}'::int4multirange")), ["true"]);
}

/// numeric sorts and compares as Postgres has it: -Infinity, negatives (past 128 bits too), tiny fractions, positives,
/// Infinity, then NaN, whichever way it is read: a sequential scan, a primary key, an index, a unique constraint.
#[test]
fn numeric_orders_alike_in_scans_keys_and_indexes() {
    let db = Db::new();
    let ordered = [
        "-Infinity",
        "-1e200",
        "-1e60",
        "-170141183460469231731687303715884105729",
        "-5",
        "-0.51",
        "-0.5",
        "-1e-50",
        "0",
        "1e-50",
        "0.5",
        "0.51",
        "5",
        "170141183460469231731687303715884105728",
        "1e60",
        "1e200",
        "Infinity",
        "NaN",
    ];
    let n = ordered.len();
    sql(&db, "create table nt (n int, v numeric)");
    sql(&db, "create index nt_v on nt (v)");
    sql(&db, "create table nu (n int, v numeric)");
    sql(&db, "create table np (v numeric primary key, n int)");
    sql(&db, "create table nq (n int, v numeric unique)");
    // written in an order that is none of them
    for step in 0..n {
        let at = step * 7 % n;
        for t in ["nt", "nu", "np", "nq"] {
            let q = if t == "np" {
                format!("insert into np values ('{}', {at})", ordered[at])
            } else {
                format!("insert into {t} values ({at}, '{}')", ordered[at])
            };
            sql(&db, &q);
        }
    }
    let ints = |r: std::ops::Range<usize>| r.map(|i| i.to_string()).collect::<Vec<_>>();
    for t in ["nt", "nu", "np", "nq"] {
        assert_eq!(rows(&sql(&db, &format!("select n from {t} order by v"))), ints(0..n), "{t}");
        let mut desc = ints(0..n);
        desc.reverse();
        assert_eq!(rows(&sql(&db, &format!("select n from {t} order by v desc"))), desc, "{t}");
        for (k, x) in ordered.iter().enumerate() {
            let probe =
                |op: &str| rows(&sql(&db, &format!("select n from {t} where v {op} '{x}'::numeric order by v")));
            assert_eq!(probe("<"), ints(0..k), "{t} < {x}");
            assert_eq!(probe("<="), ints(0..k + 1), "{t} <= {x}");
            assert_eq!(probe("="), ints(k..k + 1), "{t} = {x}");
            assert_eq!(probe(">="), ints(k..n), "{t} >= {x}");
            assert_eq!(probe(">"), ints(k + 1..n), "{t} > {x}");
        }
        let between = format!("select n from {t} where v between -1 and 1 order by v");
        assert_eq!(rows(&sql(&db, &between)), ints(5..12), "{t}");
        assert_eq!(rows(&sql(&db, &format!("select n from {t} order by v limit 1"))), ["0"], "{t}");
        assert_eq!(rows(&sql(&db, &format!("select n from {t} order by v desc limit 1"))), [(n - 1).to_string()]);
    }
    // a value changed to and from NaN moves its entry; a row deleted takes its entry
    sql(&db, "update nt set v = 'NaN' where n = 3");
    assert_eq!(rows(&sql(&db, "select n from nt where v > 1e100 order by n")), ["3", "15", "16", "17"]);
    sql(&db, "update nt set v = '-170141183460469231731687303715884105729' where n = 3");
    assert_eq!(rows(&sql(&db, "select n from nt where v > 1e100 order by n")), ["15", "16", "17"]);
    sql(&db, "delete from nt where v = 'Infinity'");
    assert_eq!(rows(&sql(&db, "select n from nt where v > 1e100 order by v")), ["15", "17"]);
    // NaN equals NaN, so it can't repeat in a key (nor an infinity)
    for x in ["NaN", "-Infinity"] {
        assert_eq!(fails(&db, &format!("insert into np values ('{x}', 99)")).0, "23505", "np {x}");
        assert_eq!(fails(&db, &format!("insert into nq values (99, '{x}')")).0, "23505", "nq {x}");
    }
}

/// A chain of `||` is typed in time linear in its length (each level once asked its operands' types again, which
/// doubled the work with each term: 28 terms took 47 s).
#[test]
fn a_long_chain_of_concatenations_is_planned_in_linear_time() {
    // (on a server connection's 16 MB stack: a chain 100 deep is bound and evaluated 100 levels deep, which a debug
    // build's frames, several times a release build's, don't fit in a test thread's 2 MB)
    let h = std::thread::Builder::new().stack_size(16 << 20).spawn(|| {
        let db = Db::new();
        let chain = vec!["'x'"; 100].join(" || ");
        let start = std::time::Instant::now();
        let out = run(&db, MAIN, &format!("select length({chain})"), &[]).unwrap();
        assert_eq!(out[0].rows[0][0].to_text(), "100");
        let pow = vec!["1"; 30].join(" ^ ");
        run(&db, MAIN, &format!("select {pow}"), &[]).unwrap();
        let sum = vec!["1"; 100].join(" + ");
        run(&db, MAIN, &format!("select {sum}"), &[]).unwrap();
        assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
    });
    h.unwrap().join().unwrap();
}

/// Rows written to the database since it was opened (its `rows_written` counter).
fn rows_written(db: &Db) -> u64 {
    db.counters().into_iter().find(|(n, _)| *n == "rows_written").map_or(0, |(_, v)| v)
}

/// Runs `q`, and says how many rows it wrote.
fn writes_of(db: &Db, q: &str) -> u64 {
    let before = rows_written(db);
    sql(db, q);
    rows_written(db) - before
}

/// Postgres does these ALTERs in its catalog alone, and so does this: no row, and no row kept behind an index or a
/// constraint, is written (before, every one of them took every row's entries out and made them again).
#[test]
fn catalog_only_alters_write_no_rows() {
    let db = Db::new();
    sql(&db, "create table par (id integer primary key, k integer unique)");
    sql(&db, "insert into par select g, g from generate_series(1, 5000) g");
    sql(
        &db,
        "create table kid (id integer primary key, p integer references par(id), a integer, b text, c integer, \
         u integer, v varchar(10), ck integer, n numeric(10, 2))",
    );
    sql(
        &db,
        "insert into kid select g, g, g % 100, 'b' || g, g, g, 'v' || (g % 10), g, g from generate_series(1, 5000) g",
    );
    sql(&db, "create index kid_a on kid (a)");
    sql(&db, "create unique index kid_u on kid (u)");
    sql(&db, "alter table kid add constraint kid_c_ck check (ck > 0)");
    sql(&db, "alter table kid add constraint kid_c_uq unique (c)");
    for q in [
        "alter table kid rename column b to b2",
        "alter table kid rename column a to a2",
        "alter table kid rename column p to p2",
        "alter table kid drop column ck",
        "alter table kid add column d integer",
        "alter table kid add column e integer default 5",
        "alter table kid add column f text not null default 'x'",
        "alter table kid add column ts timestamptz default now()",
        "alter table kid alter column e set default 7",
        "alter table kid alter column e drop default",
        "alter table kid alter column v type varchar(20)",
        "alter table kid alter column v type text",
        "alter table kid alter column n type numeric(12, 2)",
        "alter table kid alter column b2 set not null",
        "alter table kid alter column b2 drop not null",
        "alter table kid rename constraint kid_c_uq to kid_c_uq2",
        "alter table kid add constraint kid_nv check (c > 0) not valid",
        "alter table kid validate constraint kid_nv",
        "alter table kid drop constraint kid_nv",
        "alter table kid add constraint kid_fk2 foreign key (c) references par(k) not valid",
        "alter table kid validate constraint kid_fk2",
        "alter table kid rename constraint kid_fk2 to kid_fk3",
        "alter table kid drop constraint kid_fk3",
        "comment on table kid is 'children'",
        "comment on column kid.a2 is 'bucket'",
    ] {
        // (a foreign key has the rows that mark each row's parent made, once, when it is added; one renamed has them
        // moved, one dropped has them dropped)
        let allowed = if q.contains("add constraint kid_fk2") {
            5010
        } else if q.contains("kid_fk2 to") || q.contains("kid_c_uq to") || q.contains("drop constraint kid_fk3") {
            2 * 5000 + 10
        } else {
            5
        };
        let n = writes_of(&db, q);
        assert!(n <= allowed, "{q} wrote {n} rows");
    }
    // everything still answers by its new name, and holds
    assert_eq!(rows(&sql(&db, "select count(*) from kid where a2 = 5")), ["50"]);
    assert_eq!(rows(&sql(&db, "select b2, e, f, d from kid where id = 7")), ["b7|5|x|null"]);
    assert_eq!(rows(&sql(&db, "select v, n from kid where id = 12")), ["v2|12.00"]);
    assert_eq!(fails(&db, "select a from kid").0, "42703");
    assert_eq!(fails(&db, "select ck from kid").0, "42703");
    let row = |id: u32, p: u32, c: u32, u: u32| {
        format!("insert into kid (id, p2, a2, b2, c, u, v, n) values ({id}, {p}, 1, 'x', {c}, {u}, 'v', 1)")
    };
    assert_eq!(fails(&db, &row(9001, 99999, 9001, 9001)).0, "23503");
    assert_eq!(fails(&db, "delete from par where id = 1").0, "23503");
    assert_eq!(fails(&db, &row(9002, 1, 9002, 7)).0, "23505");
    let (code, message) = fails(&db, &row(9003, 1, 7, 9003));
    assert_eq!(code, "23505");
    assert!(message.contains("kid_c_uq2"), "{message}");
    // the index was kept: an update moves its entry, the old value finds nothing, the new one finds the row
    sql(&db, "update kid set a2 = 1000 where id = 5");
    assert_eq!(rows(&sql(&db, "select id from kid where a2 = 1000")), ["5"]);
    assert_eq!(rows(&sql(&db, "select count(*) from kid where a2 = 5")), ["49"]);
    assert_eq!(
        rows(&sql(&db, "select indexdef from pg_indexes where tablename = 'kid' and indexname = 'kid_a'")),
        ["CREATE INDEX kid_a ON public.kid USING btree (a2)"]
    );
}

/// An index, a unique constraint or a foreign key renamed has its rows moved to its name, not made again from the table;
/// a table renamed, its rows moved to it (and the entries with it).
#[test]
fn renaming_what_has_entries_moves_them() {
    let db = Db::new();
    sql(&db, "create table par (id integer primary key)");
    sql(&db, "insert into par select g from generate_series(1, 3000) g");
    sql(&db, "create table kid (id integer primary key, p integer references par(id), a integer, u integer unique)");
    sql(&db, "insert into kid select g, g, g % 10, g from generate_series(1, 3000) g");
    sql(&db, "create index kid_a on kid (a)");
    // one entry's rows are moved: a write to take each away and one to put it back
    assert!(writes_of(&db, "alter index kid_a rename to kid_a_idx") <= 2 * 3000 + 5);
    assert!(writes_of(&db, "alter table kid rename constraint kid_p_fkey to kid_parent") <= 2 * 3000 + 5);
    assert!(writes_of(&db, "alter table kid rename constraint kid_u_key to kid_u_uq") <= 2 * 3000 + 5);
    assert_eq!(rows(&sql(&db, "select count(*) from kid where a = 3")), ["300"]);
    assert_eq!(fails(&db, "insert into kid values (9000, 99999, 1, 9000)").0, "23503");
    assert_eq!(fails(&db, "insert into kid values (9001, 1, 1, 5)").0, "23505");
    assert_eq!(fails(&db, "delete from par where id = 2").0, "23503");
    sql(&db, "alter table kid rename to kid2");
    assert_eq!(rows(&sql(&db, "select count(*) from kid2 where a = 3")), ["300"]);
    assert_eq!(fails(&db, "insert into kid2 values (9000, 99999, 1, 9000)").0, "23503");
    assert_eq!(fails(&db, "insert into kid2 values (9001, 1, 1, 5)").0, "23505");
    assert_eq!(fails(&db, "delete from par where id = 2").0, "23503");
    sql(&db, "delete from kid2 where id = 2");
    sql(&db, "delete from par where id = 2");
    assert_eq!(rows(&sql(&db, "select count(*) from kid2 where a = 2")), ["299"]);
    // a fork made before keeps the schema it had
    sql(&db, "alter table kid2 rename column a to a_new");
    db.fork(MAIN, "f").unwrap();
    sql(&db, "alter table kid2 rename column a_new to a_newer");
    let on_fork = run(&db, "f", "select count(*) from kid2 where a_new = 3", &[]).unwrap();
    assert_eq!(rows(&on_fork[0]), ["300"]);
    assert_eq!(rows(&sql(&db, "select count(*) from kid2 where a_newer = 3")), ["300"]);
    assert!(run(&db, "f", "select a_newer from kid2", &[]).is_err());
}

/// A column dropped on a table with indexes and foreign keys: those on it go (with their rows), the others stay.
#[test]
fn drop_column_takes_only_its_own_entries() {
    let db = Db::new();
    sql(&db, "create table par (id integer primary key)");
    sql(&db, "insert into par select g from generate_series(1, 2000) g");
    sql(
        &db,
        "create table kid (id integer primary key, p integer references par(id), q integer references par(id), \
         a integer, b integer, u integer unique, w integer unique)",
    );
    sql(&db, "insert into kid select g, g, g, g % 10, g % 20, g, g from generate_series(1, 2000) g");
    sql(&db, "create index kid_a on kid (a)");
    sql(&db, "create index kid_b on kid (b)");
    sql(&db, "create index kid_ab on kid (a, b)");
    let n = writes_of(&db, "alter table kid drop column a");
    // (kid_a's and kid_ab's 2000 entries each go; no other row is written)
    assert!((4000..4000 + 10).contains(&n), "wrote {n} rows");
    assert_eq!(rows(&sql(&db, "select count(*) from kid where b = 5")), ["100"]);
    let n = writes_of(&db, "alter table kid drop column q");
    assert!((2000..2000 + 10).contains(&n), "wrote {n} rows");
    let n = writes_of(&db, "alter table kid drop column w");
    assert!((2000..2000 + 10).contains(&n), "wrote {n} rows");
    assert_eq!(fails(&db, "insert into kid (id, p, b, u) values (9000, 99999, 1, 9000)").0, "23503");
    assert_eq!(fails(&db, "insert into kid (id, p, b, u) values (9001, 1, 1, 5)").0, "23505");
    assert_eq!(rows(&sql(&db, "select count(*) from pg_indexes where tablename = 'kid'")), ["3"]);
    // the dropped column's name is free again, as a new column
    sql(&db, "alter table kid add column a text default 'again'");
    assert_eq!(rows(&sql(&db, "select a from kid where id = 3")), ["again"]);
}

/// A foreign key added NOT VALID makes only its own rows, and holds for the rows written from then on; VALIDATE reads the
/// rows and writes none, and fails on a row with no parent.
#[test]
fn foreign_key_not_valid_then_validate() {
    let db = Db::new();
    sql(&db, "create table par (id integer primary key)");
    sql(&db, "insert into par select g from generate_series(1, 1000) g");
    sql(&db, "create table kid (id integer primary key, p integer)");
    sql(&db, "insert into kid select g, g from generate_series(1, 1000) g");
    sql(&db, "insert into kid values (2000, 5000)"); // no parent
    let n = writes_of(&db, "alter table kid add constraint kid_fk foreign key (p) references par(id) not valid");
    assert!((1000..1000 + 10).contains(&n), "wrote {n} rows");
    // the rows there are left alone, those written from now on are checked
    assert_eq!(fails(&db, "insert into kid values (2001, 5001)").0, "23503");
    assert_eq!(fails(&db, "delete from par where id = 3").0, "23503");
    assert_eq!(fails(&db, "alter table kid validate constraint kid_fk").0, "23503");
    sql(&db, "delete from kid where id = 2000");
    assert!(writes_of(&db, "alter table kid validate constraint kid_fk") <= 2);
    assert_eq!(rows(&sql(&db, "select convalidated from pg_constraint where conname = 'kid_fk'")), ["true"]);
    // a check made over rows that don't meet it is refused, one NOT VALID is not
    sql(&db, "alter table kid drop constraint kid_fk");
    sql(&db, "insert into kid values (3000, -1)");
    sql(&db, "alter table kid add constraint kid_pos check (p > 0) not valid");
    assert_eq!(fails(&db, "alter table kid validate constraint kid_pos").0, "23514");
    assert_eq!(fails(&db, "alter table kid add constraint kid_pos2 check (p > 0)").0, "23514");
}

/// The statements that were slow on a table with rows, indexes and foreign keys, on 100000 rows.
#[test]
fn alters_on_a_big_table_with_indexes_and_foreign_keys_take_no_time() {
    let db = Db::new();
    sql(&db, "create table par (id integer primary key)");
    sql(&db, "insert into par select g from generate_series(1, 100000) g");
    sql(&db, "create table kid (id integer primary key, p integer references par(id), a integer, b text, c integer)");
    sql(&db, "insert into kid select g, g, g % 1000, 'b' || g, g from generate_series(1, 100000) g");
    sql(&db, "create index kid_a on kid (a)");
    sql(&db, "create index kid_c on kid (c)");
    for q in [
        "alter table kid rename column b to b2",
        "alter table kid rename column a to a2",
        "alter table kid drop column b2",
        "alter table kid add column d integer",
        "alter table kid add column e integer default 3",
        "alter table kid alter column e set default 4",
        "alter table kid add constraint kid_ck check (c > 0) not valid",
        "alter table kid drop constraint kid_ck",
    ] {
        let start = std::time::Instant::now();
        sql(&db, q);
        assert!(start.elapsed().as_secs() < 2, "{q} took {:?}", start.elapsed());
    }
}

/// uuid-ossp's time-based UUIDs (version 1, with a random node), as a column's default too; and the channels a
/// session LISTENs on, which Postgres lists in the order they were listened.
#[test]
fn uuid_v1_and_listening_channels() {
    let db = Db::new();
    let o =
        sql(&db, "select uuid_generate_v1()::text, uuid_generate_v1mc()::text, pg_typeof(uuid_generate_v1())::text");
    let row = &o.rows[0];
    for u in [row[0].to_text(), row[1].to_text()] {
        let b = u.as_bytes();
        assert_eq!(u.len(), 36, "{u}");
        assert_eq!(b[14], b'1', "version 1: {u}");
        assert!(matches!(b[19], b'8' | b'9' | b'a' | b'b'), "RFC 4122 variant: {u}");
    }
    assert_ne!(row[0].to_text(), row[1].to_text());
    assert_eq!(row[2].to_text(), "uuid");
    sql(&db, "create table v1 (id uuid default uuid_generate_v1() primary key, n int)");
    sql(&db, "insert into v1 (n) values (1), (2), (3)");
    assert_eq!(
        rows(&sql(&db, "select count(distinct id), bool_and(substr(id::text, 15, 1) = '1') from v1")),
        ["3|true"]
    );

    // (one session: LISTEN is the session's)
    let mut s = chronos::sql::Session::new(MAIN);
    let mut run1 = |q: &str| s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    run1("listen foo");
    run1("listen bar");
    run1("listen foo");
    let names = |o: Output| o.rows.iter().map(|r| r[0].to_text()).collect::<Vec<_>>();
    assert_eq!(names(run1("select pg_listening_channels()")), ["foo", "bar"]);
    assert_eq!(names(run1("select * from pg_listening_channels()")), ["foo", "bar"]);
    run1("unlisten foo");
    assert_eq!(names(run1("select pg_listening_channels()")), ["bar"]);
    run1("unlisten *");
    assert!(run1("select pg_listening_channels()").rows.is_empty());
}

/// A long IN list of constants (and `= ANY` of a long constant array or array parameter, NOT IN and `<> ALL`) is
/// looked up in a hash set, not compared with each row: 100,000 values against 200,000 rows took minutes, and an
/// indexed column's 300,000 points 48 s. NULL keeps its meaning: a value not found where the list has a NULL is
/// unknown.
#[test]
fn long_in_lists_and_arrays_are_hashed() {
    let db = Db::new();
    sql(&db, "create table inl (id int primary key, v int)");
    sql(&db, "insert into inl select g, g % 10 from generate_series(1, 200000) g");
    sql(&db, "create table inn (id int, v int)");
    sql(&db, "insert into inn select g, g % 10 from generate_series(1, 200000) g");
    let list = (1..=100_000).map(|i| (i * 2).to_string()).collect::<Vec<_>>().join(",");
    let arr = Value::Text(format!("{{{list}}}"));
    let start = std::time::Instant::now();
    for t in ["inl", "inn"] {
        let q = |q: &str| rows(&sql(&db, &q.replace("TBL", t).replace("LIST", &list)));
        assert_eq!(q("select count(*), sum(v) from TBL where id in (LIST)"), ["100000|400000"], "{t}");
        assert_eq!(q("select count(*) from TBL where id not in (LIST)"), ["100000"], "{t}");
        assert_eq!(q("select count(*) from TBL where id = any('{LIST}'::int[])"), ["100000"], "{t}");
        assert_eq!(q("select count(*) from TBL where id <> all(array[LIST]::bigint[])"), ["100000"], "{t}");
        let o =
            run(&db, MAIN, &format!("select count(*) from {t} where id = any($1::int[])"), std::slice::from_ref(&arr))
                .unwrap();
        assert_eq!(rows(&o[0]), ["100000"], "{t}");
        let o =
            run(&db, MAIN, &format!("select count(*) from {t} where id <> all($1::int[])"), std::slice::from_ref(&arr))
                .unwrap();
        assert_eq!(rows(&o[0]), ["100000"], "{t}");
        let o =
            run(&db, MAIN, &format!("delete from {t} where id = any($1::int[]) and v = 9"), std::slice::from_ref(&arr))
                .unwrap();
        assert_eq!(o[0].tag, "DELETE 0", "{t}");
        let o = run(
            &db,
            MAIN,
            &format!("update {t} set v = v + 1 where id = any($1::int[]) and v = 0"),
            std::slice::from_ref(&arr),
        );
        assert_eq!(o.unwrap()[0].tag, "UPDATE 20000", "{t}");
    }
    assert!(start.elapsed().as_secs() < 90, "took {:?}", start.elapsed());
    let with_null = format!("{list},null");
    let q = format!("select 3 in ({with_null}), 4 in ({with_null}), 3 not in ({with_null}), 4 not in ({with_null})");
    assert_eq!(rows(&sql(&db, &q)), ["null|true|null|false"]);
    let q = format!("select null::int in ({list}), 3 in ({list}), 3 not in ({list})");
    assert_eq!(rows(&sql(&db, &q)), ["null|false|true"]);
    let some = Value::Text(format!("{{{with_null}}}"));
    let q = "select 3 = any($1::int[]), 4 = any($1::int[]), 3 <> all($1::int[]), 4 <> all($1::int[])";
    assert_eq!(rows(&run(&db, MAIN, q, &[some]).unwrap()[0]), ["null|true|null|false"]);
    // (over an empty array ANY is false and ALL true, whatever the value; text against text, byte for byte)
    let q = "select null::int = any($1::int[]), null::int <> all($1::int[])";
    assert_eq!(rows(&run(&db, MAIN, q, &[Value::Text("{}".into())]).unwrap()[0]), ["false|true"]);
    let words = (0..50).map(|i| format!("'w{i}'")).collect::<Vec<_>>().join(",");
    let q = format!("select 'w7' in ({words}), 'W7' in ({words}), 'w7 ' in ({words})");
    assert_eq!(rows(&sql(&db, &q)), ["true|false|false"]);
}

/// CREATE TABLE costs about the same with thousands of tables as with none (each looked every relation over for the
/// statistics of dropped ones: 10,000 tables made one take 65 ms), and pg_dump's query of pg_type, whose scalar
/// subqueries read pg_class and pg_type by the row's oid, reads each catalog table once rather than once per row
/// (with 10,000 tables it ran for more than 15 minutes).
// a debug build is tens of times slower at what each CREATE TABLE still reads per table (new_pkey_name's look for a
// taken `<table>_pkey`: 15 ms for the first 300 tables, 68 ms for the last 300 in release, 0.26 s and 2.5 s in debug),
// so the bound only holds there in release
#[cfg_attr(debug_assertions, ignore = "measures release-build time; the VM runs it in release")]
#[test]
fn many_tables_are_made_and_dumped_in_time() {
    let db = Db::new();
    let batch = |from: usize| {
        let start = std::time::Instant::now();
        for i in from..from + 300 {
            sql(&db, &format!("create table mt_{i} (id int primary key, v text)"));
        }
        start.elapsed()
    };
    let first = batch(0);
    for from in (300..2700).step_by(300) {
        batch(from);
    }
    let last = batch(2700);
    assert!(last < first * 4 + std::time::Duration::from_millis(500), "first 300: {first:?}, last 300: {last:?}");
    let start = std::time::Instant::now();
    let q = "select count(*), count(typrelkind), sum(case when isarray then 1 else 0 end) from (select typname, \
             case when typrelid = 0 then ' '::\"char\" else (select relkind from pg_class where oid = typrelid) end as \
             typrelkind, typname[0] = '_' and typelem != 0 and (select typarray from pg_type te where oid = \
             pg_type.typelem) = oid as isarray from pg_type) t where typname like '%mt\\_%'";
    assert_eq!(rows(&sql(&db, q)), ["6000|6000|3000"]);
    assert!(start.elapsed().as_secs() < 60, "took {:?}", start.elapsed());
}

/// A column a view or a SQL-standard function body reads: dropping it is 2BP01 naming them as Postgres does, with
/// CASCADE they go (NOTICE), and its type can't change (0A000).
#[test]
fn a_column_views_and_functions_read_names_them() {
    let db = Db::new();
    sql(&db, "create table vt (a int, b int, c int)");
    sql(&db, "create view vv as select * from vt");
    sql(&db, "create view vw as select b from vt where c > 0");
    sql(&db, "create function vf() returns int begin atomic select max(c) from vt; end");
    let e = run(&db, MAIN, "alter table vt drop column c", &[]).unwrap_err();
    let message = "cannot drop column c of table vt because other objects depend on it";
    assert_eq!((e.code, e.message.as_str()), ("2BP01", message));
    let info = e.info.unwrap();
    let detail = "view vv depends on column c of table vt\nview vw depends on column c of table vt\nfunction vf() \
                  depends on column c of table vt";
    assert_eq!(info.detail.as_deref(), Some(detail));
    assert_eq!(info.hint.as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    let e = run(&db, MAIN, "alter table vt alter column b type bigint", &[]).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), ("0A000", "cannot alter type of a column used by a view or rule"));
    assert_eq!(e.info.unwrap().detail.as_deref(), Some("rule _RETURN on view vv depends on column \"b\""));
    sql(&db, "alter table vt drop column c cascade");
    assert_eq!(rows(&sql(&db, "select count(*) from pg_views where viewname in ('vv', 'vw')")), ["0"]);
    assert_eq!(rows(&sql(&db, "select count(*) from pg_proc where proname = 'vf'")), ["0"]);
    sql(&db, "create table vg (a int, b int generated always as (a + 1) stored)");
    let e = run(&db, MAIN, "alter table vg drop column a", &[]).unwrap_err();
    assert_eq!(e.info.unwrap().detail.as_deref(), Some("column b of table vg depends on column a of table vg"));
    let e = run(&db, MAIN, "alter table vg alter column a type bigint", &[]).unwrap_err();
    assert_eq!(e.info.unwrap().detail.as_deref(), Some("Column \"a\" is used by generated column \"b\"."));
}

/// A transaction too big to merge row by row commits its own state whole (see `Core::merge_published`): with another
/// session's commits from meanwhile kept, its effects (NOTIFY ON MERGE) landing, and a foreign key the other side broke
/// refused, nothing of it merged.
#[test]
fn big_transactions_keep_what_others_committed_meanwhile() {
    use chronos::sql::Session;
    let db = Db::new();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    let on = |s: &mut Session, q: &str| rows(&s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap());
    on(&mut a, "create table t (id int primary key, g int, u text unique)");
    on(&mut a, "create index t_g on t (g)");
    on(&mut a, "create table p (id int primary key)");
    on(&mut a, "create table c (id int primary key, p int references p)");
    on(&mut a, "insert into p select generate_series(1, 10)");
    on(&mut a, "begin");
    on(&mut a, "insert into t select i, i % 97, 'u' || i from generate_series(1, 15000) i");
    on(&mut b, "insert into t values (90000, 5, 'theirs')");
    on(&mut a, "insert into t select i, i % 97, 'u' || i from generate_series(15001, 16000) i");
    on(&mut b, "insert into p values (11)");
    on(&mut a, "notify on merge done, 'loaded'");
    let mut w = Session::new(MAIN);
    on(&mut w, "listen done");
    on(&mut a, "commit");
    let heard = w.inbox.take();
    assert_eq!((heard.len(), heard[0].payload.as_str()), (1, "loaded"));
    assert_eq!(on(&mut b, "select count(*), count(*) filter (where g = 5) from t"), ["16001|166"]);
    assert_eq!(on(&mut b, "select count(*) from p"), ["11"]);
    assert_eq!(on(&mut b, "select count(*) from t where u = 'u15999'"), ["1"]);
    // a parent the other session deletes meanwhile, which the transaction's rows reference
    on(&mut a, "begin");
    on(&mut a, "insert into t select i, -5, 'v' || i from generate_series(20001, 35000) i");
    on(&mut a, "insert into c values (1, 3)");
    on(&mut b, "delete from p where id = 3");
    let e = a.run(&db, "commit", &[]).expect_err("commit");
    assert_eq!(e.code, "23503", "{}", e.message);
    assert_eq!(on(&mut b, "select count(*) from t where g = -5"), ["0"]);
    assert_eq!(on(&mut b, "select count(*) from c"), ["0"]);
    // rows the other session changes meanwhile, beside the transaction's: both stand (9,000 rows: past 10,000 the
    // transaction would hold the whole table, and the other session wait for it)
    on(&mut a, "begin");
    on(&mut a, "update t set g = -1 where id <= 9000");
    on(&mut b, "update t set g = 7 where id = 90000");
    on(&mut a, "update t set u = u || 'x' where id = 1");
    assert_eq!(on(&mut a, "select g from t where id = 90000"), ["7"]);
    on(&mut a, "commit");
    assert_eq!(on(&mut b, "select count(*) from t where g = -1"), ["9000"]);
    assert_eq!(on(&mut b, "select g from t where id = 90000"), ["7"]);
}

/// Reads of a database on disk (its rows in pages, read through the tree nodes a thread keeps near and the leaf its
/// last lookup ended in) see every write: an index read in order with a LIMIT, key lookups and an aggregate, after
/// updates and deletes, before and after checkpoints, with a second database of the same rows (the same pages, until
/// one changes) open beside it, and after reopening.
#[test]
fn reads_on_disk_see_every_write() {
    let base = std::env::temp_dir().join(format!("chronos-near-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dirs = [base.join("a"), base.join("b")];
    let q = "select id, k, v from r where k between 100 and 2000 order by k limit 5";
    let first = ["100|100|v100", "101|101|v101", "102|102|v102", "103|103|v103", "104|104|v104"];
    let changed = ["100|100|w100", "101|101|w101", "102|102|w102", "104|104|x", "105|105|v105"];
    {
        let dbs: Vec<Db> = dirs.iter().map(|d| Db::open(d).unwrap()).collect();
        for db in &dbs {
            sql(db, "create table r (id int primary key, k int, v text)");
            sql(db, "create index r_k on r (k)");
            sql(db, "insert into r select g, g, 'v' || g from generate_series(1, 5000) g");
            db.checkpoint().unwrap();
            assert_eq!(rows(&sql(db, q)), first);
            assert_eq!(rows(&sql(db, "select v from r where id = 104")), ["v104"]);
        }
        // one changes: the other reads its rows as they were
        sql(&dbs[0], "update r set v = 'w' || id where id between 100 and 102");
        sql(&dbs[0], "delete from r where id = 103");
        sql(&dbs[0], "update r set v = 'x' where id = 104");
        for checkpointed in [false, true] {
            assert_eq!(rows(&sql(&dbs[0], q)), changed, "checkpointed: {checkpointed}");
            assert_eq!(rows(&sql(&dbs[0], "select v from r where id = 104")), ["x"]);
            assert_eq!(rows(&sql(&dbs[0], "select v from r where id = 105")), ["v105"]);
            assert_eq!(
                rows(&sql(&dbs[0], "select count(*), sum(k) from r where id in (100, 103, 104, 9999)")),
                ["2|204"]
            );
            assert_eq!(rows(&sql(&dbs[0], "select count(*), sum(k) from r")), ["4999|12502397"]);
            assert_eq!(rows(&sql(&dbs[1], q)), first);
            assert_eq!(rows(&sql(&dbs[1], "select v from r where id = 104")), ["v104"]);
            assert_eq!(rows(&sql(&dbs[1], "select count(*), sum(k) from r")), ["5000|12502500"]);
            dbs[0].checkpoint().unwrap();
        }
        assert_eq!(rows(&sql(&dbs[0], "select id from r where k < 50 order by k desc limit 3")), ["49", "48", "47"]);
    }
    let db = Db::open(&dirs[0]).unwrap();
    assert_eq!(rows(&sql(&db, q)), changed);
    drop(db);
    std::fs::remove_dir_all(&base).unwrap();
}

/// pg_stat_statements keeps each statement by who ran it (and where): a role without
/// pg_read_all_stats sees its own statements' text and id, and others' as `<insufficient
/// privilege>` with no id, as the extension's view shows them; resetting is a superuser's, and
/// takes a user, database and query id to forget only those. EXECUTE's statement is counted
/// once, by its PREPARE's text, and statements whose constants differ in type are two.
#[test]
fn statement_statistics_are_kept_by_role() {
    use chronos::sql::Session;
    let db = Db::new();
    let mut s = Session::new(MAIN);
    let one = |s: &mut Session, q: &str| -> Vec<String> {
        rows(&s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap())
    };
    one(&mut s, "create role pss_a; create role pss_b; create role pss_mon");
    one(&mut s, "grant pg_read_all_stats to pss_mon");
    for r in ["pss_a", "pss_b"] {
        one(&mut s, &format!("set role {r}"));
        one(&mut s, "select 911 + 912 + 913 + 914 as pss_sum");
        one(&mut s, "reset role");
    }
    let q = "select calls from pg_stat_statements where query = 'select $1 + $2 + $3 + $4 as pss_sum' order by userid";
    assert_eq!(one(&mut s, q), ["1", "1"], "one entry per role");
    one(&mut s, "set role pss_b");
    assert_eq!(one(&mut s, q), ["1"], "only its own is shown with its text");
    let hidden = "select count(*) from pg_stat_statements where query = '<insufficient privilege>' and queryid is null";
    assert_ne!(one(&mut s, hidden), ["0"]);
    let e = s.run(&db, "select pg_stat_statements_reset()", &[]).expect_err("a superuser's");
    assert_eq!(e.code, "42501", "{e}");
    one(&mut s, "reset role");
    one(&mut s, "set role pss_mon");
    assert_eq!(one(&mut s, q), ["1", "1"], "pg_read_all_stats sees every role's");
    one(&mut s, "reset role");
    // forgetting one role's
    one(&mut s, "select pg_stat_statements_reset((select oid from pg_roles where rolname = 'pss_a'), 0, 0)");
    assert_eq!(one(&mut s, q), ["1"]);
    let who = "select r.rolname from pg_stat_statements p join pg_roles r on r.oid = p.userid \
               where p.query = 'select $1 + $2 + $3 + $4 as pss_sum'";
    assert_eq!(one(&mut s, who), ["pss_b"]);
    // and one statement, by its id
    one(&mut s, "select 21 * 22 * 23 * 24 as pss_mul");
    one(
        &mut s,
        "select pg_stat_statements_reset(0, 0, (select queryid from pg_stat_statements where query = 'select $1 * $2 * $3 * $4 as pss_mul'))",
    );
    assert_eq!(
        one(&mut s, "select count(*) from pg_stat_statements where query = 'select $1 * $2 * $3 * $4 as pss_mul'"),
        ["0"]
    );
    assert_eq!(one(&mut s, q), ["1"], "the others stay");
    // EXECUTE: counted once, by its PREPARE's text (PREPARE itself not), as the extension charges it
    one(&mut s, "prepare pss_q as select 931 - 932 - 933");
    one(&mut s, "execute pss_q");
    assert_eq!(one(&mut s, "select calls, rows from pg_stat_statements where query like 'prepare pss_q %'"), ["1|1"]);
    assert_eq!(one(&mut s, "select count(*) from pg_stat_statements where query like 'execute pss_q%'"), ["0"]);
    // constants of other types make another statement
    one(&mut s, "select 941, 942, 943, 944, 945 as pss_five");
    one(&mut s, "select 'a', 'b', 'c', 'd', 'e' as pss_five");
    assert_eq!(
        one(
            &mut s,
            "select count(distinct queryid) from pg_stat_statements where query = 'select $1, $2, $3, $4, $5 as pss_five'"
        ),
        ["2"]
    );
    // the database each ran in
    assert_eq!(
        one(
            &mut s,
            "select count(*) from pg_stat_statements p join pg_database d on d.oid = p.dbid \
                     where d.datname = 'main' and p.query = 'select $1 + $2 + $3 + $4 as pss_sum'"
        ),
        ["1"]
    );
}
