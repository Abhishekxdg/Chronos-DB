//! Cross-world reads: `FROM t AT WORLD w` reads table `t` as world `w` has it, in the same query as the session's
//! own world's tables; `AT WORLDS OF SET s` reads it in every world of a set, with a `_world` column. Read only.

use chronos::sql::{self, Value};
use chronos::{AgentSpec, Can, Db, MAIN};

fn rows(db: &Db, world: &str, q: &str) -> Vec<Vec<Value>> {
    sql::run(db, world, q, &[]).unwrap_or_else(|e| panic!("{q}: {e:?}")).pop().unwrap().rows
}

fn prices(db: &Db) {
    sql::run(db, MAIN, "create table prices (sku text primary key, price bigint)", &[]).unwrap();
    sql::run(db, MAIN, "insert into prices values ('a', 10), ('b', 20), ('c', 30)", &[]).unwrap();
}

#[test]
fn at_world_reads_other_world_in_join() {
    let db = Db::new();
    prices(&db);
    db.fork(MAIN, "pricing").unwrap();
    sql::run(&db, "pricing", "update prices set price = price * 2 where sku <> 'b'", &[]).unwrap();
    // a column only the other world has: its own table definition is read
    sql::run(&db, "pricing", "alter table prices add column note text", &[]).unwrap();
    let got = rows(
        &db,
        MAIN,
        "select a.sku, a.price as before, b.price as after \
         from prices a join prices at world pricing b using (sku) \
         where a.price <> b.price order by 1",
    );
    assert_eq!(
        got,
        vec![
            vec![Value::Text("a".into()), Value::Int(10), Value::Int(20)],
            vec![Value::Text("c".into()), Value::Int(30), Value::Int(60)],
        ]
    );
    // alias before AT WORLD, and a quoted name; the other world's own column
    let got = rows(&db, MAIN, "select count(note) , count(*) from prices p at world 'pricing'");
    assert_eq!(got, vec![vec![Value::Int(0), Value::Int(3)]]);
    // from a third world, both read in another
    db.fork(MAIN, "other").unwrap();
    let got = rows(&db, "other", "select sum(price) from prices at world pricing");
    assert_eq!(got[0][0].to_text(), "100");
    // main is unchanged, and the session's own world reads as itself
    assert_eq!(rows(&db, MAIN, "select sum(price) from prices")[0][0].to_text(), "60");
    // a world that doesn't exist
    assert!(sql::run(&db, MAIN, "select * from prices at world nope", &[]).is_err());
}

#[test]
fn at_world_refused_for_agent_without_read() {
    let db = Db::new();
    prices(&db);
    db.fork(MAIN, "secret").unwrap();
    // an agent that may not read: refused, whatever world it names
    db.create_agent("blind", &AgentSpec { can: Some(vec![Can::Fork]), ..Default::default() }).unwrap();
    let mut s = sql::Session::new(MAIN);
    s.agent = Some("blind".into());
    let e = s.run(&db, "select * from prices at world secret", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{e:?}");
    s.close(&db);
    // an agent that may read reads it
    db.create_agent("reader", &AgentSpec { can: Some(vec![Can::Read]), ..Default::default() }).unwrap();
    let mut s = sql::Session::new(MAIN);
    s.agent = Some("reader".into());
    let out = s.run(&db, "select count(*) from prices at world secret", &[]).unwrap();
    assert_eq!(out[0].rows[0][0], Value::Int(3));
    // never a session's own transaction world
    let mut other = sql::Session::new(MAIN);
    other.run(&db, "begin; insert into prices values ('z', 1)", &[]).unwrap();
    let tx = db.worlds().into_iter().map(|w| w.name).find(|w| w.starts_with("_tx_"));
    if let Some(tx) = tx {
        assert!(s.run(&db, &format!("select * from prices at world '{tx}'"), &[]).is_err());
    }
    other.run(&db, "rollback", &[]).unwrap();
    other.close(&db);
    s.close(&db);
}

#[test]
fn at_world_refused_as_dml_target() {
    let db = Db::new();
    prices(&db);
    db.fork(MAIN, "w").unwrap();
    for q in [
        "update prices at world w set price = 1",
        "delete from prices at world w",
        "insert into prices at world w values ('d', 1)",
    ] {
        let e = sql::run(&db, MAIN, q, &[]).unwrap_err();
        assert_eq!(e.code, "0A000", "{q}: {e:?}");
        assert!(e.message.contains("AT WORLD only reads"), "{e:?}");
    }
    // reading another world from a write's FROM is a read: the write is to the session's world
    sql::run(&db, "w", "update prices set price = 99 where sku = 'a'", &[]).unwrap();
    sql::run(&db, MAIN, "update prices set price = o.price from prices at world w o where o.sku = prices.sku", &[])
        .unwrap();
    assert_eq!(rows(&db, MAIN, "select price from prices where sku = 'a'")[0][0], Value::Int(99));
    assert_eq!(rows(&db, "w", "select price from prices where sku = 'b'")[0][0], Value::Int(20));
}

#[test]
fn at_worlds_of_set_adds_world_column() {
    let db = Db::new();
    prices(&db);
    db.fork(MAIN, "plan").unwrap();
    sql::run(&db, MAIN, "create world set s from world plan", &[]).unwrap();
    // a set with no worlds: the base's columns, no rows
    let out = sql::run(&db, MAIN, "select * from prices at worlds of set s", &[]).unwrap();
    let cols: Vec<&str> = out[0].columns.as_ref().unwrap().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(cols, ["sku", "price", "_world"]);
    assert!(out[0].rows.is_empty());
    sql::run(&db, MAIN, "fork world set s count 3 run $$update prices set price = price + $1$$ seed 1", &[]).unwrap();
    let got = rows(&db, MAIN, "select _world, sum(price) from prices at worlds of set s group by 1 order by 1");
    assert_eq!(
        got.iter().map(|r| (r[0].to_text(), r[1].to_text())).collect::<Vec<_>>(),
        [("s_0".into(), "60".into()), ("s_1".into(), "63".into()), ("s_2".into(), "66".into())]
    );
    // joined with the session's world
    let got = rows(
        &db,
        MAIN,
        "select w._world, count(*) from prices m join prices at worlds of set s w on w.sku = m.sku \
         where w.price > m.price group by 1 order by 1",
    );
    assert_eq!(got.len(), 2);
    // worlds whose columns differ: refused, by name
    sql::run(&db, "s_1", "alter table prices add column extra text", &[]).unwrap();
    let e = sql::run(&db, MAIN, "select * from prices at worlds of set s", &[]).unwrap_err();
    assert_eq!(e.code, "42804", "{e:?}");
    assert!(e.message.contains("s_1") && e.message.contains("AT WORLD"), "{e:?}");
    let e = sql::run(&db, MAIN, "select * from prices at worlds of set nope", &[]).unwrap_err();
    assert_eq!(e.code, "42704", "{e:?}");
}

#[test]
fn at_world_applies_tenant_per_world() {
    let db = Db::new();
    sql::run(&db, MAIN, "create table leads (id int primary key, org_id int not null, name text)", &[]).unwrap();
    sql::run(&db, MAIN, "insert into leads values (1, 7, 'a'), (2, 42, 'b')", &[]).unwrap();
    // (since the security release a tenant agent reads only its tenant's worlds and main, and a new agent has no
    // `read` of other worlds unless given it: `later` is tenant 7's, and the agent is given `read`)
    db.fork_with(MAIN, "later", serde_json::json!({"tenant": "org_id = 7"})).unwrap();
    sql::run(&db, "later", "insert into leads values (3, 7, 'c'), (4, 42, 'd'), (5, 42, 'e')", &[]).unwrap();
    db.fork(MAIN, "nobodys").unwrap();
    let spec = AgentSpec {
        tenant: Some(serde_json::json!({"org_id": 7})),
        can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn]),
        ..Default::default()
    };
    db.create_agent("run7", &spec).unwrap();
    let mut s = sql::Session::new(MAIN);
    s.agent = Some("run7".into());
    // its own tenant's rows, in the other world as in its own
    let out = s.run(&db, "select count(*) from leads at world later", &[]).unwrap();
    assert_eq!(out[0].rows[0][0], Value::Int(2));
    let out = s.run(&db, "select l.id from leads l join leads at world later n using (id) order by 1", &[]).unwrap();
    assert_eq!(out[0].rows, vec![vec![Value::Int(1)]]);
    // a world of no tenant it doesn't read at all
    assert_eq!(s.run(&db, "select count(*) from leads at world nobodys", &[]).unwrap_err().code, "42501");
    s.close(&db);
}
