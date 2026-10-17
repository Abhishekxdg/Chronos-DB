//! SQL `MERGE INTO`: what Postgres does is in tests/pgdiff.rs (`merge_into`). Here: it is atomic,
//! it takes a driver's parameters, its command tag, schema-qualified targets and views as the
//! source, and an agent that can only read can't run it.

mod common;

use chronos::sql::{Session, Value, take_notices};
use chronos::{AgentSpec, Can, Db, MAIN};
use postgres::{Client, NoTls};
use std::process::{Child, Command};

/// The server, stopped (and its folder removed) however the test ends: a panic must not leave it
/// holding the test's output open.
struct Server(Child, std::path::PathBuf);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn rows(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    let out = s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}")).pop().unwrap();
    out.rows
        .iter()
        .map(|r| {
            r.iter().map(|v| if *v == Value::Null { "null".into() } else { v.to_text() }).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn fails(db: &Db, s: &mut Session, q: &str) -> chronos::sql::SqlError {
    s.run(db, q, &[]).err().unwrap_or_else(|| panic!("{q} should fail"))
}

#[test]
fn a_failing_clause_undoes_the_ones_before_it() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for q in [
        "create table t (id int primary key, v int, code text unique)",
        "create table src (id int, v int, code text)",
        "insert into t values (1, 1, 'a'), (2, 2, 'b')",
        // the update of 1 is fine; the insert of 5 takes a code the table already has
        "insert into src values (1, 10, 'a'), (5, 50, 'b')",
    ] {
        s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    let e = fails(
        &db,
        &mut s,
        "merge into t using src on t.id = src.id when matched then update set v = src.v when not matched then insert values (src.id, src.v, src.code)",
    );
    assert_eq!(e.code, "23505", "{e}");
    assert_eq!(rows(&db, &mut s, "select id, v from t order by id"), ["1|1", "2|2"], "the update went with it");
    let out =
        s.run(&db, "merge into t using src on t.id = src.id when matched then update set v = src.v", &[]).unwrap();
    assert_eq!(out[0].tag, "MERGE 1");
}

#[test]
fn schemas_views_and_agents() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    for q in [
        "create schema app",
        "create table app.stock (sku text primary key, qty int not null default 0)",
        "create table deliveries (sku text, n int)",
        "create view arrivals as select sku, sum(n) as n from deliveries group by sku",
        "insert into app.stock values ('a', 5)",
        "insert into deliveries values ('a', 2), ('a', 3), ('b', 7)",
    ] {
        s.run(&db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    let out = s
        .run(
            &db,
            r#"merge into app.stock as "S" using arrivals as a on "S".sku = a.sku
               when matched then update set qty = "S".qty + a.n
               when not matched then insert (sku, qty) values (a.sku, a.n)"#,
            &[],
        )
        .unwrap();
    assert_eq!(out[0].tag, "MERGE 2");
    assert_eq!(rows(&db, &mut s, "select sku, qty from app.stock order by sku"), ["a|10", "b|7"]);
    // an agent that can only read
    db.create_agent("reader", &AgentSpec { can: Some(vec![Can::Read]), ..Default::default() }).unwrap();
    let mut r = Session::new(MAIN);
    r.agent = Some("reader".into());
    let e = fails(&db, &mut r, "merge into app.stock using arrivals a on stock.sku = a.sku when matched then delete");
    assert_eq!(e.code, "42501", "{e}");
    // a temporary table: the MERGE runs among the session's own tables, so a source that is one of them (or
    // VALUES) works, and one that isn't is missing there, not silently left out
    s.run(&db, "create temp table scratch (id int primary key, n int)", &[]).unwrap();
    s.run(&db, "merge into scratch using (values (1, 1)) v(id, n) on scratch.id = v.id when not matched then insert values (v.id, v.n)", &[])
        .unwrap();
    assert_eq!(rows(&db, &mut s, "select id, n from scratch"), ["1|1"]);
    let e = fails(
        &db,
        &mut s,
        "merge into scratch using arrivals a on scratch.id = 1 when not matched then insert values (1, 1)",
    );
    assert_eq!(e.code, "42P01", "{e}");
    // a table without a key names its rows by all their columns
    s.run(&db, "create table nokey (a int)", &[]).unwrap();
    s.run(&db, "insert into nokey values (1), (2)", &[]).unwrap();
    s.run(&db, "merge into nokey using (values (1)) v(a) on nokey.a = v.a when matched then delete", &[]).unwrap();
    assert_eq!(rows(&db, &mut s, "select a from nokey"), ["2"]);
}

fn run_all(db: &Db, s: &mut Session, qs: &[&str]) {
    for q in qs {
        s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

/// The notices `q` sends, as their messages.
fn notices(db: &Db, s: &mut Session, q: &str) -> Vec<String> {
    take_notices();
    s.run(db, q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
    take_notices().into_iter().map(|n| n.message).collect()
}

fn merge_tables(db: &Db, s: &mut Session) {
    run_all(
        db,
        s,
        &[
            "create table target (tid integer, balance integer)",
            "create table source (sid integer, delta integer)",
            "insert into target values (1, 10), (2, 20), (3, 30)",
            "insert into source values (4, 40)",
        ],
    );
}

#[test]
fn what_postgres_refuses_it_refuses_in_its_words() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    merge_tables(&db, &mut s);
    let cases: &[(&str, &str, &str)] = &[
        // the word as written
        (
            "MERGE INTO target t RANDOMWORD USING source AS s ON t.tid = s.sid WHEN MATCHED THEN UPDATE SET balance = 0",
            "42601",
            "syntax error at or near \"RANDOMWORD\"",
        ),
        (
            "MERGE INTO target t USING source AS s ON t.tid = s.sid WHEN MATCHED THEN INSERT DEFAULT VALUES",
            "42601",
            "syntax error at or near \"INSERT\"",
        ),
        (
            "MERGE INTO target t USING source AS s ON t.tid = s.sid WHEN NOT MATCHED THEN UPDATE SET balance = 0",
            "42601",
            "syntax error at or near \"UPDATE\"",
        ),
        (
            "MERGE INTO target t USING source AS s ON t.tid = s.sid WHEN NOT MATCHED THEN INSERT SELECT (1, 1)",
            "42601",
            "syntax error at or near \"SELECT\"",
        ),
        (
            "merge into target t using source as s on t.tid = s.sid when matched then delete when matched then update set balance = 0",
            "42601",
            "unreachable WHEN clause specified after unconditional WHEN clause",
        ),
        (
            "merge into target using target on tid = tid when matched then do nothing",
            "42712",
            "name \"target\" specified more than once",
        ),
        (
            "with foo as (merge into target using source on (true) when matched then delete) select * from foo",
            "0A000",
            "MERGE not supported in WITH query",
        ),
        (
            "copy (merge into target using source on (true) when matched then delete) to stdout",
            "0A000",
            "MERGE not supported in COPY",
        ),
        (
            "merge into target t using (select * from source where t.tid > sid) s on t.tid = s.sid when not matched then insert default values",
            "42P01",
            "invalid reference to FROM-clause entry for table \"t\"",
        ),
        (
            "merge into target t using source s on t.tid = s.sid when not matched then insert (tid, balance) values (t.tid, s.delta)",
            "42P01",
            "invalid reference to FROM-clause entry for table \"t\"",
        ),
        (
            "merge into target t using source s on t.tid = s.sid when not matched and t.balance = 100 then insert (tid) values (s.sid)",
            "42P01",
            "invalid reference to FROM-clause entry for table \"t\"",
        ),
        (
            "merge into target t using source s on t.tid = s.sid when matched and t.xmin = t.xmax then delete",
            "42P10",
            "cannot use system column \"xmin\" in MERGE WHEN condition",
        ),
    ];
    for (q, code, message) in cases {
        let e = fails(&db, &mut s, q);
        assert_eq!((e.code, e.message.as_str()), (*code, *message), "{q}");
    }
    let e = fails(&db, &mut s, "merge into target using target on tid = tid when matched then do nothing");
    let detail = e.info.and_then(|i| i.detail);
    assert_eq!(detail.as_deref(), Some("The name is used both as MERGE target table and data source."));
    let e = fails(
        &db,
        &mut s,
        "merge into target t using source s on t.tid = s.sid when not matched and t.balance = 100 then insert (tid) values (s.sid)",
    );
    let at = e.info.as_ref().and_then(|i| i.position);
    assert_eq!(at, Some(74), "at the t of t.balance");
    // a materialized view, and a table with rules
    run_all(&db, &mut s, &["create materialized view mv as select * from target", "create table ruled (a int)"]);
    let e = fails(
        &db,
        &mut s,
        "merge into mv t using source s on t.tid = s.sid when not matched then insert default values",
    );
    assert_eq!((e.code, e.message.as_str()), ("0A000", "cannot execute MERGE on relation \"mv\""));
    assert_eq!(
        e.info.and_then(|i| i.detail).as_deref(),
        Some("This operation is not supported for materialized views.")
    );
    run_all(&db, &mut s, &["create rule ruled_r as on insert to ruled do instead nothing"]);
    let e = fails(
        &db,
        &mut s,
        "merge into ruled r using source s on r.a = s.sid when not matched then insert values (s.sid)",
    );
    assert_eq!(e.info.and_then(|i| i.detail).as_deref(), Some("MERGE is not supported for relations with rules."));
    // a name a MATCHED clause reads that both sides have; a NOT MATCHED clause reads the source's
    run_all(
        &db,
        &mut s,
        &[
            "create table sq_target (tid integer not null, balance integer)",
            "create table sq_source (delta integer, sid integer, balance integer default 0)",
            "insert into sq_target values (1, 100), (2, 200)",
            "insert into sq_source (sid, delta, balance) values (-1, -10, -1)",
            "create view v as select * from sq_source where sid < 2",
        ],
    );
    let q =
        "merge into sq_target using v on tid = sid when matched and tid > 2 then update set balance = balance + delta";
    let e = fails(&db, &mut s, q);
    assert_eq!((e.code, e.message.as_str()), ("42702", "column reference \"balance\" is ambiguous"));
    assert_eq!(e.info.and_then(|i| i.position), Some(q.find("balance +").unwrap() as u32 + 1));
    s.run(
        &db,
        "merge into sq_target t using v on tid = sid when matched and tid > 2 then update set balance = t.balance + delta \
         when not matched then insert (balance, tid) values (balance + delta, sid)",
        &[],
    )
    .unwrap();
    assert_eq!(rows(&db, &mut s, "select tid, balance from sq_target order by tid"), ["-1|-11", "1|100", "2|200"]);
    // a target row two source rows reach: 21000, with Postgres's hint
    run_all(&db, &mut s, &["insert into source values (2, 5), (2, 6)"]);
    let e = fails(
        &db,
        &mut s,
        "merge into target t using source s on t.tid = s.sid when matched then update set balance = 0",
    );
    assert_eq!(e.code, "21000", "{e}");
    assert_eq!(
        e.info.and_then(|i| i.hint).as_deref(),
        Some("Ensure that not more than one source row matches any one target row.")
    );
    // tableoid may be read; pg_class's planner statistics change nothing
    run_all(
        &db,
        &mut s,
        &[
            "delete from source where sid = 2",
            "merge into target t using source s on t.tid = s.sid when matched and t.tableoid >= 0 then delete",
            "merge into pg_class c using (select 'pg_depend'::regclass as oid) as j on j.oid = c.oid \
             when matched then update set reltuples = reltuples + 1",
        ],
    );
}

const TRIGGER_FUNC: &str = r#"create function merge_trigfunc() returns trigger language plpgsql as $$
begin
    if TG_LEVEL = 'ROW' then
        if TG_OP = 'DELETE' then
            raise notice '% % ROW %', TG_WHEN, TG_OP, old.tid;
        else
            raise notice '% % ROW %', TG_WHEN, TG_OP, new.tid;
        end if;
    else
        raise notice '% % STATEMENT', TG_WHEN, TG_OP;
    end if;
    if TG_WHEN = 'BEFORE' and TG_LEVEL = 'ROW' then
        if TG_OP = 'DELETE' then
            return old;
        end if;
        return new;
    end if;
    return null;
end;
$$"#;

#[test]
fn triggers_fire_as_postgres_fires_them() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    run_all(
        &db,
        &mut s,
        &[
            "create table target (tid integer, balance integer)",
            "create table source (sid integer, delta integer)",
            "insert into target values (1, 10), (2, 20), (3, 0)",
            "insert into source values (3, 20)",
            "insert into source values (2, 5)",
            "insert into source values (4, 40)",
            TRIGGER_FUNC,
        ],
    );
    for (when, ev, each) in [
        ("before", "insert", "statement"),
        ("before", "update", "statement"),
        ("before", "delete", "statement"),
        ("after", "insert", "statement"),
        ("after", "update", "statement"),
        ("after", "delete", "statement"),
        ("before", "insert", "row"),
        ("before", "update", "row"),
        ("before", "delete", "row"),
        ("after", "insert", "row"),
        ("after", "update", "row"),
        ("after", "delete", "row"),
    ] {
        let name = format!("merge_{when}_{ev}_{each}");
        let sql =
            format!("create trigger {name} {when} {ev} on target for each {each} execute procedure merge_trigfunc()");
        s.run(&db, &sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    s.run(&db, "begin", &[]).unwrap();
    let said = notices(
        &db,
        &mut s,
        "merge into target t using source as s on t.tid = s.sid \
         when matched and t.balance > s.delta then update set balance = t.balance - s.delta \
         when matched then delete \
         when not matched then insert values (s.sid, s.delta)",
    );
    assert_eq!(
        said,
        [
            "BEFORE INSERT STATEMENT",
            "BEFORE UPDATE STATEMENT",
            "BEFORE DELETE STATEMENT",
            "BEFORE DELETE ROW 3",
            "BEFORE UPDATE ROW 2",
            "BEFORE INSERT ROW 4",
            "AFTER DELETE ROW 3",
            "AFTER UPDATE ROW 2",
            "AFTER INSERT ROW 4",
            "AFTER DELETE STATEMENT",
            "AFTER UPDATE STATEMENT",
            "AFTER INSERT STATEMENT",
        ]
    );
    assert_eq!(rows(&db, &mut s, "select tid, balance from target order by tid"), ["1|10", "2|15", "4|40"]);
    s.run(&db, "rollback", &[]).unwrap();
    // a statement trigger fires for each kind of action there is, though no row takes it
    let said = notices(
        &db,
        &mut s,
        "merge into target t using (select tid as sid, balance as delta from target where balance > 0) as s \
         on t.tid = s.sid when not matched then insert (tid, balance) values (s.sid, s.delta)",
    );
    assert_eq!(said, ["BEFORE INSERT STATEMENT", "AFTER INSERT STATEMENT"]);
}

#[test]
fn transition_tables_hold_each_kind_of_actions_rows() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    run_all(
        &db,
        &mut s,
        &[
            "create table mt (a int primary key, b text)",
            "create table ms (a int, b text)",
            "insert into ms values (1, 'initial1'), (2, 'initial2'), (3, 'initial3'), (4, 'initial4')",
            "create function dump_ins() returns trigger language plpgsql as $$ declare n int; begin \
             select count(*) into n from new_table; raise notice 'inserted %', n; return null; end; $$",
            "create function dump_upd() returns trigger language plpgsql as $$ declare n int; begin \
             select count(*) into n from new_table; raise notice 'updated %', n; return null; end; $$",
            "create function dump_del() returns trigger language plpgsql as $$ declare n int; begin \
             select count(*) into n from old_table; raise notice 'deleted %', n; return null; end; $$",
            "create trigger mt_ins after insert on mt referencing new table as new_table for each statement execute procedure dump_ins()",
            "create trigger mt_upd after update on mt referencing old table as old_table new table as new_table \
             for each statement execute procedure dump_upd()",
            "create trigger mt_del after delete on mt referencing old table as old_table for each statement execute procedure dump_del()",
        ],
    );
    let merge = "merge into mt t using ms s on t.a = s.a \
                 when matched and s.a <= 2 then update set b = t.b || ' updated by merge' \
                 when matched and s.a > 2 then delete \
                 when not matched then insert values (a, b)";
    assert_eq!(notices(&db, &mut s, merge), ["deleted 0", "updated 0", "inserted 4"]);
    assert_eq!(notices(&db, &mut s, merge), ["deleted 2", "updated 2", "inserted 0"]);
    assert_eq!(notices(&db, &mut s, merge), ["deleted 0", "updated 2", "inserted 2"]);
}

#[test]
fn joins_only_default_and_row_sets() {
    let db = Db::new();
    let mut s = Session::new(MAIN);
    run_all(
        &db,
        &mut s,
        &[
            "create table cj_target (tid integer, balance float, val text)",
            "create table cj_source1 (sid1 integer, scat integer, delta integer)",
            "create table cj_source2 (sid2 integer, sval text)",
            "insert into cj_source1 values (1, 10, 100), (1, 20, 200), (2, 20, 300), (3, 10, 400)",
            "insert into cj_source2 values (1, 'initial source2'), (2, 'initial source2'), (3, 'initial source2')",
            // USING a join: its columns by their own names, or by their source's
            "merge into cj_target t using cj_source1 s1 inner join cj_source2 s2 on sid1 = sid2 on t.tid = sid1 \
             when not matched then insert values (sid1, delta, sval)",
            "merge into cj_target t using cj_source2 s2 inner join cj_source1 s1 on sid1 = sid2 and scat = 20 \
             on t.tid = sid1 when matched then update set val = val || ' ' || s1.delta::text",
        ],
    );
    assert_eq!(
        rows(&db, &mut s, "select tid, balance, val from cj_target order by tid, balance"),
        [
            "1|100|initial source2 200",
            "1|200|initial source2 200",
            "2|300|initial source2 300",
            "3|400|initial source2"
        ]
    );
    // ONLY: not the rows of the tables that inherit from it
    run_all(
        &db,
        &mut s,
        &[
            "create table p (a int, b int default 7)",
            "create table c () inherits (p)",
            "insert into p values (1, 1)",
            "insert into c values (2, 2)",
            "merge into only p using (values (2)) v(a) on p.a = v.a when matched then delete \
             when not matched then insert (a) values (v.a)",
        ],
    );
    assert_eq!(rows(&db, &mut s, "select a, b from only p order by a"), ["1|1", "2|7"]);
    assert_eq!(rows(&db, &mut s, "select a, b from c"), ["2|2"]);
    // SET (a, b) = (...)
    run_all(
        &db,
        &mut s,
        &[
            "create table d (id int primary key, x int default 5, y text)",
            "insert into d values (1, 1, 'one'), (2, 2, 'two')",
            "merge into d using (values (1, 'uno'), (2, 'dos')) v(id, y) on d.id = v.id \
             when matched and d.id = 1 then update set x = 5 \
             when matched then update set (x, y) = (v.id * 10, v.y)",
        ],
    );
    assert_eq!(rows(&db, &mut s, "select id, x, y from d order by id"), ["1|5|one", "2|20|dos"]);
}

#[test]
fn a_driver_binds_parameters() {
    let dir = std::env::temp_dir().join(format!("chronos-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let s = common::serve(Command::new(env!("CARGO_BIN_EXE_chronos")).args(["serve", dir.to_str().unwrap()]), true);
    let _server = Server(s.child, dir);
    let mut c = Client::connect(&format!("host=127.0.0.1 port={} user=tester dbname=main", s.pg), NoTls).unwrap();
    c.batch_execute("create table counters (name text primary key, n int not null)").unwrap();
    let merge = c
        .prepare(
            "merge into counters c using (select $1::text as name, $2::int as n) s on c.name = s.name \
             when matched then update set n = c.n + s.n when not matched then insert values (s.name, s.n)",
        )
        .unwrap();
    for (name, n) in [("hits", 1i32), ("hits", 4), ("errors", 2)] {
        assert_eq!(c.execute(&merge, &[&name, &n]).unwrap(), 1);
    }
    let r = c.query("select name, n from counters order by name", &[]).unwrap();
    let got: Vec<(String, i32)> = r.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, [("errors".to_string(), 2), ("hits".to_string(), 5)]);
}
