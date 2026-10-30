//! An agent held to a tenant (`CREATE AGENT ... WITH (tenant = 'org_id = 42')`): in every world it reads and
//! changes only the rows of tables with the tenant's column that hold its value, whatever its role and whatever
//! the table's own row security; a table without the column is held to it through its foreign keys (up to three),
//! and one with neither is shared by every tenant, read whole and never written; it forks only that tenant's worlds;
//! it sees the changes of its own worlds only; and an agent's token stops working once its `valid_for` has passed.

use chronos::sql::{Session, Value, parse, run};
use chronos::{AgentSpec, Db, MAIN};

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create table leads (id int primary key, org_id int not null, name text, owner text);
         create table plans (id int primary key, title text);
         insert into leads values (1, 42, 'acme', 'bo'), (2, 42, 'initech', 'bo'), (3, 7, 'globex', 'eve'),
                                  (4, 7, 'umbrella', 'eve');
         insert into plans values (1, 'free'), (2, 'pro')",
        &[],
    )
    .unwrap();
    run(
        db,
        MAIN,
        "create agent run42 with (can = 'read,fork,write_own,merge_own,write_main', tenant = 'org_id = 42')",
        &[],
    )
    .unwrap();
}

fn agent(name: &str) -> Session {
    let mut s = Session::new(MAIN);
    s.agent = Some(name.into());
    s
}

/// COPY `t` FROM STDIN (csv) with `data`, as a client sends it.
fn copy_in(db: &Db, s: &mut Session, t: &str, data: &str) -> Result<String, chronos::sql::SqlError> {
    let c = s.copy_in(db, &parse(&format!("copy {t} from stdin csv")).unwrap().pop().unwrap())?;
    let mut piece = Some(data.as_bytes().to_vec());
    c.run(s, db, Box::new(move || Ok(piece.take()))).map(|o| o.tag)
}

fn texts(db: &Db, s: &mut Session, sql: &str) -> Vec<String> {
    let out = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap();
    out.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")).collect()
}

#[test]
fn reads_only_its_tenants_rows_everywhere() {
    let db = Db::new();
    setup(&db);
    let mut s = agent("run42");
    assert_eq!(texts(&db, &mut s, "select name from leads order by id"), ["acme", "initech"]);
    assert_eq!(texts(&db, &mut s, "select count(*) from leads"), ["2"]);
    // a filter, a join, a subquery, an aggregate: none reaches another tenant's rows
    assert_eq!(texts(&db, &mut s, "select count(*) from leads where org_id = 7"), ["0"]);
    assert_eq!(texts(&db, &mut s, "select count(*) from leads a join leads b on a.id <> b.id"), ["2"]);
    assert_eq!(texts(&db, &mut s, "select (select max(name) from leads where org_id <> 42)"), [""]);
    assert_eq!(texts(&db, &mut s, "select count(*) from (select * from leads union all select * from leads) x"), ["4"]);
    // a table without the column reads whole
    assert_eq!(texts(&db, &mut s, "select count(*) from plans"), ["2"]);
    // a world it forks reads the same
    s.run(&db, "create world w42", &[]).unwrap();
    s.run(&db, "use world w42", &[]).unwrap();
    assert_eq!(texts(&db, &mut s, "select count(*) from leads"), ["2"]);
    // turning row security off doesn't lift it
    s.run(&db, "set row_security = off", &[]).unwrap();
    assert_eq!(texts(&db, &mut s, "select count(*) from leads"), ["2"]);
    // the database's own user sees every row
    assert_eq!(run(&db, MAIN, "select count(*) from leads", &[]).unwrap()[0].rows[0][0].to_text(), "4");
}

#[test]
fn changes_only_its_tenants_rows() {
    let db = Db::new();
    setup(&db);
    let mut s = agent("run42");
    s.run(&db, "create world w", &[]).unwrap();
    s.run(&db, "use world w", &[]).unwrap();
    // updates and deletes reach its rows only
    let n = s.run(&db, "update leads set owner = 'ada'", &[]).unwrap().pop().unwrap();
    assert_eq!(n.tag, "UPDATE 2");
    let n = s.run(&db, "delete from leads where org_id = 7", &[]).unwrap().pop().unwrap();
    assert_eq!(n.tag, "DELETE 0");
    // a row it writes holds its tenant, before and after
    let e = s.run(&db, "insert into leads values (9, 7, 'evil', 'x')", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    let e = s.run(&db, "update leads set org_id = 7 where id = 1", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    s.run(&db, "insert into leads values (10, 42, 'hooli', 'ada')", &[]).unwrap();
    // its world merges into main: the scope it was forked with is its tenant's
    s.run(&db, "use world main", &[]).unwrap();
    s.run(&db, "merge world w", &[]).unwrap();
    let owners = run(&db, MAIN, "select string_agg(owner, ',' order by id) from leads", &[]).unwrap();
    assert_eq!(owners[0].rows[0][0].to_text(), "ada,ada,eve,eve,ada");
}

#[test]
fn forks_only_its_tenants_worlds() {
    let db = Db::new();
    setup(&db);
    let mut s = agent("run42");
    s.run(&db, "create world mine", &[]).unwrap();
    let scope = db.world("mine").unwrap().scope.unwrap();
    assert_eq!(scope["tenant"]["column"], "org_id", "{scope}");
    assert_eq!(scope["tenant"]["value"], 42, "{scope}");
    let e = s.run(&db, "create world other with (tenant = 'org_id = 7')", &[]).unwrap_err();
    assert!(e.message.contains("works for tenant"), "{}", e.message);
}

#[test]
fn sees_changes_of_its_own_worlds_only() {
    let db = Db::new();
    setup(&db);
    // another agent's world, with another tenant's rows changed in it
    run(&db, MAIN, "create world other_w; use world other_w; update leads set owner = 'zed' where org_id = 7", &[])
        .unwrap();
    let mut s = agent("run42");
    for q in ["diff world other_w", "show reviews", "show audit", "merge world other_w dry run"] {
        let e = s.run(&db, q, &[]).unwrap_err();
        assert_eq!(e.code, "42501", "{q}: {}", e.message);
    }
    s.run(&db, "create world mine", &[]).unwrap();
    s.run(&db, "use world mine", &[]).unwrap();
    s.run(&db, "update leads set owner = 'ada' where id = 1", &[]).unwrap();
    let d = s.run(&db, "diff world mine", &[]).unwrap().pop().unwrap();
    assert_eq!(d.rows.len(), 1);
}

#[test]
fn the_row_api_is_refused_where_the_tenant_limits_it() {
    let db = Db::new();
    setup(&db);
    let refused = chronos::sql::tenant_refuses(&db, "run42", MAIN, &["leads"], false).unwrap();
    assert!(refused.is_some_and(|m| m.contains("SQL")));
    // a table without the column, shared by every tenant: read as it is, never written
    assert!(chronos::sql::tenant_refuses(&db, "run42", MAIN, &["plans"], false).unwrap().is_none());
    let refused = chronos::sql::tenant_refuses(&db, "run42", MAIN, &["plans"], true).unwrap();
    assert!(refused.is_some_and(|m| m.contains("may only read table \"plans\"")));
}

#[test]
fn a_token_stops_working_once_valid_for_has_passed() {
    let db = Db::new();
    setup(&db);
    let spec = AgentSpec { valid_for: Some(1), ..Default::default() };
    let (_, token) = db.create_agent("brief", &spec).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(db.agent_by_token(&token).is_err());
    assert!(db.as_agent("brief").is_err());
    let (_, token) =
        db.create_agent("lasting", &AgentSpec { valid_for: Some(3_600_000), ..Default::default() }).unwrap();
    assert!(db.agent_by_token(&token).is_ok());
    // SHOW AGENTS shows both the tenant and the expiry
    let out = run(&db, MAIN, "show agents", &[]).unwrap().pop().unwrap();
    let col = |n: &str| out.columns.as_ref().unwrap().iter().position(|c| c.0 == n).unwrap();
    let r = out.rows.iter().find(|r| r[0].to_text() == "run42").unwrap();
    assert_eq!(r[col("tenant")].to_text(), "\"org_id\" = 42");
    let r = out.rows.iter().find(|r| r[0].to_text() == "lasting").unwrap();
    assert_ne!(r[col("expires")].to_text(), "NULL");
}

#[test]
fn a_tenant_column_no_table_has_is_refused() {
    let db = Db::new();
    setup(&db);
    // a typo: no table has the column, so the agent would be held nowhere and see every tenant's rows
    let spec = AgentSpec { tenant: Some(serde_json::json!({"nosuch": 1})), ..Default::default() };
    let e = db.create_agent("typo", &spec).unwrap_err().to_string();
    assert!(e.contains("no table has a column \"nosuch\""), "{e}");
    assert!(e.contains("leads (id, org_id, name, owner)") && e.contains("plans (id, title)"), "{e}");
    assert!(db.agent("typo").is_err(), "nothing was made");
    // so in SQL, and when changing an agent's tenant
    let e = run(&db, MAIN, "create agent typo2 with (tenant = 'org = 42')", &[]).unwrap_err();
    assert!(e.message.contains("no table has a column \"org\""), "{}", e.message);
    let e = run(&db, MAIN, "alter agent run42 set (tenant = 'org = 42')", &[]).unwrap_err();
    assert!(e.message.contains("no table has a column \"org\""), "{}", e.message);
    assert_eq!(db.agent("run42").unwrap().tenant_sql().unwrap(), "\"org_id\" = 42", "left as it was");
    // a column some table has is taken, tables without it (plans) being shared lookup tables
    let spec = AgentSpec { tenant: Some(serde_json::json!({"org_id": 7})), ..Default::default() };
    let (a, _) = db.create_agent("run7", &spec).unwrap();
    assert_eq!(a.tenant_sql().unwrap(), "\"org_id\" = 7");
    let mut s = agent("run7");
    assert_eq!(texts(&db, &mut s, "select count(*) from leads"), ["2"]);
    assert_eq!(texts(&db, &mut s, "select count(*) from plans"), ["2"]);
    // a database with no tables yet: nothing to check it against
    let empty = Db::new();
    empty.create_agent("early", &spec).unwrap();
}

/// Orders with the tenant's column; line items and their notes without it, held to it through their foreign keys
/// (one and two hops); invoices in a schema of their own; shipments keyed by two columns; two tables whose keys point
/// at each other; and countries, a lookup table shared by every tenant.
fn setup_keys(db: &Db) {
    run(
        db,
        MAIN,
        "create table orders (id int primary key, org_id int not null, title text);
         create table order_items (id int primary key, order_id int references orders (id), sku text);
         create table item_notes (id int primary key, item_id int references order_items (id), body text);
         create schema app;
         create table app.invoices (id int primary key, order_id int not null references orders (id));
         create table app.invoice_lines (id int primary key, invoice_id int references app.invoices (id), cents int);
         create table shipments (id int, version int, org_id int not null, primary key (id, version));
         create table shipment_events (id int primary key, ship int, ver int,
                                       foreign key (ship, ver) references shipments (id, version));
         create table pb (id int primary key, pa_id int);
         create table pa (id int primary key, pb_id int references pb (id), order_id int references orders (id));
         alter table pb add foreign key (pa_id) references pa (id);
         create table countries (code text primary key, name text);
         insert into orders values (1, 42, 'mine'), (2, 42, 'also mine'), (3, 7, 'globex');
         insert into order_items values (10, 1, 'a'), (11, 2, 'b'), (12, 3, 'c'), (13, null, 'orphan');
         insert into item_notes values (100, 10, 'call'), (101, 12, 'globex renewal'), (102, 13, 'loose');
         insert into app.invoices values (1, 1), (2, 3);
         insert into app.invoice_lines values (1, 1, 500), (2, 2, 900);
         insert into shipments values (1, 1, 42), (1, 2, 7);
         insert into shipment_events values (1, 1, 1), (2, 1, 2);
         insert into pa values (1, null, 1), (2, null, 3);
         insert into pb values (1, 1), (2, 2);
         update pa set pb_id = id;
         insert into countries values ('de', 'Germany'), ('fr', 'France');
         create agent run42 with (can = 'read,fork,write_own,merge_own,write_main', tenant = 'org_id = 42')",
        &[],
    )
    .unwrap();
}

#[test]
fn a_table_without_the_column_is_held_through_its_foreign_keys() {
    let db = Db::new();
    setup_keys(&db);
    let mut s = agent("run42");
    // the table with the column, as before
    assert_eq!(texts(&db, &mut s, "select id from orders order by id"), ["1", "2"]);
    // one hop: items of its orders; an item of no order (a null key) is no tenant's
    assert_eq!(texts(&db, &mut s, "select id from order_items order by id"), ["10", "11"]);
    // two hops: notes on its orders' items
    assert_eq!(texts(&db, &mut s, "select body from item_notes"), ["call"]);
    // a schema of its own, two hops
    assert_eq!(texts(&db, &mut s, "select id from app.invoices"), ["1"]);
    assert_eq!(texts(&db, &mut s, "select cents from app.invoice_lines"), ["500"]);
    // a key of two columns
    assert_eq!(texts(&db, &mut s, "select id from shipment_events"), ["1"]);
    // keys pointing at each other: pa through orders, pb through pa, no loop
    assert_eq!(texts(&db, &mut s, "select id from pa"), ["1"]);
    assert_eq!(texts(&db, &mut s, "select id from pb"), ["1"]);
    // a join, an aggregate, a subquery
    assert_eq!(texts(&db, &mut s, "select count(*) from order_items i join item_notes n on n.item_id = i.id"), ["1"]);
    assert_eq!(texts(&db, &mut s, "select (select string_agg(sku, ',' order by id) from order_items)"), ["a,b"]);
    // a shared table reads whole
    assert_eq!(texts(&db, &mut s, "select count(*) from countries"), ["2"]);
    // the database's own user sees every row
    assert_eq!(run(&db, MAIN, "select count(*) from order_items", &[]).unwrap()[0].rows[0][0].to_text(), "4");
}

#[test]
fn a_child_row_is_written_only_under_the_tenants_parent() {
    let db = Db::new();
    setup_keys(&db);
    let mut s = agent("run42");
    s.run(&db, "create world w; use world w", &[]).unwrap();
    // under another tenant's order, or no order: refused
    for q in [
        "insert into order_items values (14, 3, 'x')",
        "insert into order_items values (14, null, 'x')",
        "insert into item_notes values (103, 12, 'x')",
        "insert into app.invoice_lines values (3, 2, 1)",
        "update order_items set order_id = 3 where id = 10",
    ] {
        let e = s.run(&db, q, &[]).unwrap_err();
        assert_eq!(e.code, "42501", "{q}: {}", e.message);
    }
    // under its own: written
    s.run(&db, "insert into order_items values (14, 1, 'd'); insert into item_notes values (103, 14, 'ok')", &[])
        .unwrap();
    // updates and deletes reach its rows only
    assert_eq!(s.run(&db, "update order_items set sku = 'z'", &[]).unwrap().pop().unwrap().tag, "UPDATE 3");
    assert_eq!(s.run(&db, "delete from order_items where id in (12, 13)", &[]).unwrap().pop().unwrap().tag, "DELETE 0");
    assert_eq!(s.run(&db, "delete from item_notes", &[]).unwrap().pop().unwrap().tag, "DELETE 2");
    // the other tenant's rows, as the database's own user sees them in the world, are as they were
    let w =
        run(&db, "w", "select string_agg(sku, ',' order by id) from order_items where id in (12, 13)", &[]).unwrap();
    assert_eq!(w[0].rows[0][0].to_text(), "c,orphan");
    let w = run(&db, "w", "select string_agg(body, ',' order by id) from item_notes", &[]).unwrap();
    assert_eq!(w[0].rows[0][0].to_text(), "globex renewal,loose");
}

#[test]
fn a_shared_table_is_read_only_for_a_tenant_agent() {
    let db = Db::new();
    setup_keys(&db);
    let mut s = agent("run42");
    for at in ["main", "a world"] {
        if at == "a world" {
            s.run(&db, "create world w; use world w", &[]).unwrap();
        }
        assert_eq!(texts(&db, &mut s, "select name from countries order by code"), ["Germany", "France"]);
        for q in [
            "insert into countries values ('it', 'Italy')",
            "insert into countries values ('it', 'Italy') on conflict do nothing",
            "update countries set name = 'x'",
            "update countries set name = 'x' where false",
            "delete from countries",
            "insert into countries select 'x' || id, title from orders",
            "merge into countries t using (select 'de' as code) s on t.code = s.code when matched then delete",
        ] {
            let e = s.run(&db, q, &[]).unwrap_err();
            assert_eq!(e.code, "42501", "{q} ({at}): {}", e.message);
            assert!(e.message.contains("may only read table \"countries\""), "{q} ({at}): {}", e.message);
            assert!(e.message.contains("no column org_id and no foreign key"), "{q} ({at}): {}", e.message);
        }
    }
    // COPY FROM writes as INSERT does: refused on the shared table, held to the tenant on a child table
    let e = copy_in(&db, &mut s, "countries", "it,Italy\n").unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("may only read table \"countries\""), "{}", e.message);
    let e = copy_in(&db, &mut s, "order_items", "14,3,x\n").unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert_eq!(copy_in(&db, &mut s, "order_items", "14,1,d\n").unwrap(), "COPY 1");
    let n = run(&db, "w", "select string_agg(name, ',' order by code) from countries", &[]).unwrap();
    assert_eq!(n[0].rows[0][0].to_text(), "Germany,France", "nothing changed");
    // the database's own user writes it
    run(&db, MAIN, "insert into countries values ('it', 'Italy')", &[]).unwrap();
}

#[test]
fn show_agent_privileges_says_how_each_table_is_held() {
    let db = Db::new();
    setup_keys(&db);
    let out = run(&db, MAIN, "show agent run42 privileges", &[]).unwrap().pop().unwrap();
    let col = |n: &str| out.columns.as_ref().unwrap().iter().position(|c| c.0 == n).unwrap();
    let row = |t: &str| {
        let r = out
            .rows
            .iter()
            .find(|r| r[0].to_text() == t && matches!(r[1], Value::Null))
            .unwrap_or_else(|| panic!("{t}"));
        (r[col("read")].to_text(), r[col("write")].to_text(), r[col("why")].to_text())
    };
    let (read, write, why) = row("orders");
    assert_eq!(read, "its tenant's rows", "{why}");
    assert!(write.starts_with("insert, update, delete"), "{write}");
    assert!(why.contains("its rows with the tenant's org_id"), "{why}");
    let (read, _, why) = row("order_items");
    assert_eq!(read, "its tenant's rows", "{why}");
    assert!(why.contains("whose order_id is a row of orders that is the tenant's (through 1 foreign key)"), "{why}");
    let (read, _, why) = row("item_notes");
    assert_eq!(read, "its tenant's rows", "{why}");
    assert!(why.contains("through 2 foreign keys"), "{why}");
    let (read, write, why) = row("countries");
    assert_eq!((read.as_str(), write.as_str()), ("yes", "no"), "{why}");
    assert!(why.contains("shared by every tenant: no column org_id and no foreign key"), "{why}");
}
