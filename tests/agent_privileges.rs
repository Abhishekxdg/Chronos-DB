//! SHOW AGENT name PRIVILEGES: an agent's capabilities, a role of its name, row-level security, masked columns and
//! its merge policy, put together table by table.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};

/// SHOW AGENT `name` PRIVILEGES as a person: (table, column) -> (read, write, merge, why).
fn privileges(db: &Db, name: &str) -> Vec<[String; 6]> {
    let out = run(db, MAIN, &format!("show agent {name} privileges"), &[]).unwrap().pop().unwrap();
    out.rows
        .iter()
        .map(|r| {
            let t: Vec<String> = r.iter().map(|v| v.to_text()).collect();
            [t[0].clone(), t[1].clone(), t[2].clone(), t[3].clone(), t[4].clone(), t[5].clone()]
        })
        .collect()
}

fn row<'a>(rows: &'a [[String; 6]], table: &str, column: &str) -> &'a [String; 6] {
    rows.iter().find(|r| r[0] == table && r[1] == column).unwrap_or_else(|| panic!("no row {table}.{column}: {rows:?}"))
}

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create table orders (id int primary key, total int, status text);
         create table refunds (id int primary key, amount int);
         create table notes (id int primary key, body text, owner text)",
        &[],
    )
    .unwrap();
}

#[test]
fn a_new_agent_works_on_its_own_worlds_and_a_person_merges() {
    let db = Db::new();
    setup(&db);
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let rows = privileges(&db, "bot");
    assert_eq!(rows.len(), 3, "a row per table, none per column: {rows:?}");
    let o = row(&rows, "orders", "");
    assert_eq!(o[2], "yes");
    assert_eq!(o[3], "insert, update, delete in its own worlds");
    assert_eq!(o[4], "a person merges");
    assert!(o[5].contains("no 'merge_own' or 'merge'"), "{}", o[5]);
}

#[test]
fn a_merge_policy_decides_table_by_table() {
    let db = Db::new();
    setup(&db);
    run(
        &db,
        MAIN,
        "create merge policy careful with (tables = 'orders,refunds', review_tables = 'refunds', \
         review_columns = 'orders.status', max_deletes = 0)",
        &[],
    )
    .unwrap();
    let can = vec![Can::Read, Can::Fork, Can::WriteOwn, Can::WriteMain, Can::MergeOwn];
    db.create_agent("bot", &AgentSpec { can: Some(can), policy: Some("careful".into()), ..Default::default() })
        .unwrap();
    let rows = privileges(&db, "bot");
    let o = row(&rows, "orders", "");
    assert_eq!(o[3], "insert, update, delete in its own worlds", "a policy keeps it off main: {o:?}");
    assert_eq!(o[4], "on its own within policy careful");
    assert!(o[5].contains("max_deletes 0"), "{}", o[5]);
    assert_eq!(row(&rows, "orders", "status")[4], "waits for a person");
    assert_eq!(row(&rows, "refunds", "")[4], "waits for a person");
    assert_eq!(row(&rows, "notes", "")[4], "refused");
    // without write_main, merge_own reaches only worlds it may change
    db.create_agent(
        "small",
        &AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn, Can::MergeOwn]), ..Default::default() },
    )
    .unwrap();
    assert_eq!(row(&privileges(&db, "small"), "orders", "")[4], "not into main");
}

#[test]
fn a_role_of_its_name_must_allow_too() {
    let db = Db::new();
    setup(&db);
    run(
        &db,
        MAIN,
        "create role bot; grant select on orders to bot; grant select (id, owner) on notes to bot;
         grant select, insert on refunds to bot;
         alter table notes enable row level security;
         create policy mine on notes using (owner = current_user)",
        &[],
    )
    .unwrap();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let rows = privileges(&db, "bot");
    let o = row(&rows, "orders", "");
    assert_eq!((o[2].as_str(), o[3].as_str()), ("yes", "no"), "{o:?}");
    assert_eq!(row(&rows, "refunds", "")[3], "insert in its own worlds");
    let n = row(&rows, "notes", "");
    assert_eq!(n[2], "no", "no table-wide SELECT: {n:?}");
    assert_eq!(row(&rows, "notes", "owner")[2], "yes", "its column grant");
    assert!(rows.iter().all(|r| !(r[0] == "notes" && r[1] == "body")), "body differs from nothing");
}

#[test]
fn row_security_and_masks_show() {
    let db = Db::new();
    setup(&db);
    run(
        &db,
        MAIN,
        "create role bot; grant select on notes to bot;
         alter table notes enable row level security;
         create policy mine on notes using (owner = current_user);
         alter table notes alter column body set masked",
        &[],
    )
    .unwrap();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    db.create_agent("reader", &AgentSpec { can: Some(vec![Can::Read, Can::Unmask]), ..Default::default() }).unwrap();
    let rows = privileges(&db, "bot");
    assert_eq!(row(&rows, "notes", "")[2], "rows its policies allow");
    assert_eq!(row(&rows, "notes", "body")[2], "masked (hash)");
    let r = privileges(&db, "reader");
    let body = row(&r, "notes", "body");
    assert_eq!(body[2], "yes", "it has 'unmask': {body:?}");
    assert_eq!(row(&r, "orders", "")[3], "no");
}

#[test]
fn an_agent_may_show_its_own() {
    let db = Db::new();
    setup(&db);
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    db.create_agent("other", &AgentSpec::default()).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    assert_eq!(s.run(&db, "show agent bot privileges", &[]).unwrap()[0].rows.len(), 3);
    assert_eq!(s.run(&db, "show agent other privileges", &[]).unwrap_err().code, "42501");
    run(&db, MAIN, "create world w; use world w; create table extra (id int primary key)", &[]).unwrap();
    let out = run(&db, MAIN, "show agent bot privileges in world w", &[]).unwrap().pop().unwrap();
    assert_eq!(out.rows.len(), 4);
    assert!(run(&db, MAIN, "show agent nobody privileges", &[]).is_err());
}

#[test]
fn a_column_grant_to_write_shows_on_the_column() {
    let db = Db::new();
    setup(&db);
    run(
        &db,
        MAIN,
        "create role bot; grant select on orders to bot; grant update (status), insert (id, total) on orders to bot",
        &[],
    )
    .unwrap();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let rows = privileges(&db, "bot");
    let o = row(&rows, "orders", "");
    assert_eq!(o[3], "no", "nothing granted on the whole table: {o:?}");
    let status = row(&rows, "orders", "status");
    assert_eq!(status[3], "update in its own worlds", "{status:?}");
    assert!(status[5].contains("granted UPDATE on the column"), "{}", status[5]);
    assert_eq!(row(&rows, "orders", "total")[3], "insert in its own worlds");
    assert_eq!(row(&rows, "orders", "id")[3], "insert in its own worlds");
}
