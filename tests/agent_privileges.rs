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

// --- Gate S (CHRONOS-PLAN-2026-11-17-r3.md, batch S4; approved decision 3): a new agent reads its
// own worlds and main (`read_own`, `read_main`), not other agents' worlds; agents made before keep
// `read`; the setting `agent_default_read` gives new agents `read` as before. Added failing in S0. ---

/// An agent's rights by name (text, so the test builds before S4 adds the new rights).
fn rights(db: &Db, name: &str) -> Vec<&'static str> {
    db.agent(name).unwrap().can.iter().map(|c| c.name()).collect()
}

/// Two agents made with the new defaults, `a` with a world holding a change.
fn two_agents(db: &Db) -> (Session, Session) {
    setup(db);
    run(db, MAIN, "insert into orders values (1, 10, 'open')", &[]).unwrap();
    db.create_agent("a", &AgentSpec::default()).unwrap();
    db.create_agent("b", &AgentSpec::default()).unwrap();
    let (mut a, mut b) = (Session::new(MAIN), Session::new(MAIN));
    a.agent = Some("a".into());
    b.agent = Some("b".into());
    a.run(db, "create world aw; use world aw; update orders set status = 'secret plan' where id = 1", &[]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    a.run(db, "update orders set total = 99 where id = 1; use world main", &[]).unwrap();
    (a, b)
}

fn refused(db: &Db, s: &mut Session, sql: &str) {
    match s.run(db, sql, &[]) {
        Ok(out) => panic!("{sql}: allowed: {:?}", out.iter().map(|r| r.rows.len()).collect::<Vec<_>>()),
        Err(e) => assert_eq!(e.code, "42501", "{sql}: {}", e.message),
    }
}

#[test]
fn new_agent_default_rights() {
    let db = Db::new();
    setup(&db);
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let can = rights(&db, "bot");
    assert!(can.contains(&"read_own") && can.contains(&"read_main"), "{can:?}");
    assert!(!can.contains(&"read"), "{can:?}");
    run(&db, MAIN, "create agent sqlbot", &[]).unwrap();
    let can = rights(&db, "sqlbot");
    assert!(!can.contains(&"read"), "{can:?}");
}

#[test]
fn read_own_cannot_read_other_world() {
    let db = Db::new();
    let (mut a, mut b) = two_agents(&db);
    refused(&db, &mut b, "use world aw");
    let mut inside = Session::new("aw");
    inside.agent = Some("b".into());
    refused(&db, &mut inside, "select * from orders");
    refused(&db, &mut b, "fork world aw as bw");
    // its own world and main it reads
    assert!(b.run(&db, "select * from orders", &[]).is_ok());
    b.run(&db, "create world bw; use world bw", &[]).unwrap();
    assert!(b.run(&db, "select * from orders", &[]).is_ok());
    assert!(a.run(&db, "use world aw; select * from orders", &[]).is_ok());
}

#[test]
fn cannot_read_other_world_at_time() {
    let db = Db::new();
    let (_, mut b) = two_agents(&db);
    refused(&db, &mut b, "use world 'aw@-1 second'");
    refused(&db, &mut b, "fork world 'aw@-1 second' as bw");
    refused(&db, &mut b, "diff world 'aw@-1 second' to aw");
}

#[test]
fn cannot_diff_other_world() {
    let db = Db::new();
    let (_, mut b) = two_agents(&db);
    for sql in [
        "diff world aw",
        "diff world aw as sql",
        "diff world aw readers",
        "diff world main to aw",
        "merge world aw dry run",
    ] {
        refused(&db, &mut b, sql);
    }
}

#[test]
fn cannot_list_other_worlds_changes() {
    let db = Db::new();
    let (_, mut b) = two_agents(&db);
    for sql in ["show history limit 50", "show storage for world aw", "show reviews", "show audit"] {
        match b.run(&db, sql, &[]) {
            Err(e) => assert_eq!(e.code, "42501", "{sql}: {}", e.message),
            Ok(out) => {
                let text: String = out.iter().flat_map(|r| r.rows.iter().flatten().map(|v| v.to_text())).collect();
                assert!(!text.contains("secret plan") && !text.contains("aw"), "{sql}: {text}");
            }
        }
    }
}

/// Agents in a folder made before the change keep `read`: tests/fixtures/legacy-assign was made by
/// the 0.5.5 binary with agent run42 created `can = 'read,...'`.
#[test]
fn existing_agent_rights_unchanged_after_upgrade() {
    let from = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-assign");
    let dir = std::env::temp_dir().join(format!("chronos-agent-privileges-legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&from, &dir);
    let db = Db::open(&dir).unwrap();
    let can = rights(&db, "run42");
    assert!(can.contains(&"read"), "{can:?}");
    assert!(!can.contains(&"read_own"), "{can:?}");
    // and a new agent made in the upgraded folder gets the new default
    db.create_agent("fresh", &AgentSpec::default()).unwrap();
    assert!(!rights(&db, "fresh").contains(&"read"));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn legacy_default_setting() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "alter system set agent_default_read = 'read'", &[]).unwrap();
    db.create_agent("old_style", &AgentSpec::default()).unwrap();
    assert!(rights(&db, "old_style").contains(&"read"));
    run(&db, MAIN, "alter system set agent_default_read = 'read_own,read_main'", &[]).unwrap();
    db.create_agent("new_style", &AgentSpec::default()).unwrap();
    assert!(!rights(&db, "new_style").contains(&"read"));
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dst = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dst);
        } else {
            std::fs::copy(&p, dst).unwrap();
        }
    }
}

/// PR #362 review: SHOW BRANCHES lists the worlds SHOW WORLDS does: without `read`, main and the agent's own.
#[test]
fn cannot_list_other_worlds_as_branches() {
    let db = Db::new();
    let (mut a, mut b) = two_agents(&db);
    let names = |s: &mut Session| -> Vec<String> {
        s.run(&db, "show branches", &[]).unwrap().pop().unwrap().rows.iter().map(|r| r[0].to_text()).collect()
    };
    let seen = names(&mut b);
    assert!(seen.contains(&MAIN.to_string()) && !seen.contains(&"aw".to_string()), "{seen:?}");
    assert!(names(&mut a).contains(&"aw".to_string()));
    // nor through the catalog
    let datnames = |s: &mut Session| -> Vec<String> {
        s.run(&db, "select datname from pg_database", &[])
            .unwrap()
            .pop()
            .unwrap()
            .rows
            .iter()
            .map(|r| r[0].to_text())
            .collect()
    };
    let seen = datnames(&mut b);
    assert!(seen.contains(&MAIN.to_string()) && !seen.contains(&"aw".to_string()), "{seen:?}");
    assert!(datnames(&mut a).contains(&"aw".to_string()));
    let all = run(&db, MAIN, "show branches", &[]).unwrap().pop().unwrap();
    assert!(all.rows.iter().any(|r| r[0].to_text() == "aw"), "a person sees every world");
}

/// PR #362 review: SHOW AGENT ... PRIVILEGES IN WORLD w says what the agent may read in that world: `read_main` reads
/// main only, `read_own` its own worlds only.
#[test]
fn privileges_read_by_world() {
    let db = Db::new();
    let _ = two_agents(&db);
    let read_in = |agent: &str, world: &str| -> (String, String) {
        let out =
            run(&db, MAIN, &format!("show agent {agent} privileges in world {world}"), &[]).unwrap().pop().unwrap();
        let r = out.rows.iter().find(|r| r[0].to_text() == "orders" && r[1].to_text().is_empty()).unwrap();
        (r[2].to_text(), r[5].to_text())
    };
    assert_eq!(read_in("a", "aw").0, "yes", "its own world");
    assert_eq!(read_in("b", MAIN).0, "yes", "main");
    let (read, why) = read_in("b", "aw");
    assert_eq!(read, "no", "another agent's world: {why}");
    assert!(why.contains("neither main nor its own"), "{why}");
    db.create_agent(
        "mainonly",
        &AgentSpec { can: Some(vec![Can::ReadMain, Can::Fork, Can::WriteOwn]), ..Default::default() },
    )
    .unwrap();
    let mut m = Session::new(MAIN);
    m.agent = Some("mainonly".into());
    m.run(&db, "create world mw", &[]).unwrap();
    assert_eq!(read_in("mainonly", MAIN).0, "yes");
    assert_eq!(read_in("mainonly", "mw").0, "no", "no read_own: not even its own world");
    // and the agent's statements agree
    assert_eq!(m.run(&db, "use world mw; select * from orders", &[]).unwrap_err().code, "42501");
}
