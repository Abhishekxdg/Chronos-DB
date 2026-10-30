//! An agent held to a tenant (`CREATE AGENT ... WITH (tenant = 'org_id = 42')`): in every world it reads and
//! changes only the rows of tables with the tenant's column that hold its value, whatever its role and whatever
//! the table's own row security; it forks only that tenant's worlds; it sees the changes of its own worlds only;
//! and an agent's token stops working once its `valid_for` has passed.

use chronos::sql::{Session, run};
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
    let refused = chronos::sql::tenant_refuses(&db, "run42", MAIN, &["leads"]).unwrap();
    assert!(refused.is_some_and(|m| m.contains("SQL")));
    // a table without the column: as it is
    assert!(chronos::sql::tenant_refuses(&db, "run42", MAIN, &["plans"]).unwrap().is_none());
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
