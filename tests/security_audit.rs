//! The security audit records privileged actions — agents and merge policies created, changed or
//! dropped, tokens rotated, ALTER SYSTEM — since the server started, with who did each. It is
//! readable only by the database's own user or an admin, through SHOW SECURITY AUDIT.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};

/// (action, target) pairs in the trail, newest first.
fn trail(db: &Db) -> Vec<(String, String)> {
    db.security_events(100).into_iter().map(|e| (e.action, e.target)).collect()
}

#[test]
fn privileged_actions_are_recorded() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    db.alter_agent("bot", &AgentSpec { max_worlds: Some(5), ..Default::default() }).unwrap();
    db.rotate_agent_token("bot").unwrap();
    run(&db, MAIN, "create merge policy p with (max_deletes = 0)", &[]).unwrap();
    run(&db, MAIN, "alter merge policy p set (max_rows = 100)", &[]).unwrap();
    run(&db, MAIN, "alter system set max_worlds = 1000", &[]).unwrap();
    db.drop_agent("bot").unwrap();

    let t = trail(&db);
    // newest first: the drop is first, the create agent last
    assert_eq!(t.first().unwrap().0, "drop agent");
    assert!(t.contains(&("create agent".into(), "bot".into())), "{t:?}");
    assert!(t.contains(&("alter agent".into(), "bot".into())), "{t:?}");
    assert!(t.contains(&("rotate token".into(), "bot".into())), "{t:?}");
    assert!(t.contains(&("create merge policy".into(), "p".into())), "{t:?}");
    assert!(t.contains(&("alter merge policy".into(), "p".into())), "{t:?}");
    assert!(t.iter().any(|(a, tg)| a == "alter system" && tg.starts_with("max_worlds")), "{t:?}");
}

#[test]
fn show_security_audit_lists_them_for_the_owner() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let out = run(&db, MAIN, "show security audit", &[]).unwrap();
    let rows = &out[0].rows;
    assert!(!rows.is_empty());
    // columns: at, actor, action, target
    let top = &rows[0];
    assert_eq!(top[1].to_text(), "owner", "an owner action is attributed to 'owner'");
    assert_eq!(top[2].to_text(), "create agent");
    assert_eq!(top[3].to_text(), "bot");
    assert!(run(&db, MAIN, "show security audit limit 1", &[]).unwrap()[0].rows.len() == 1);
}

#[test]
fn an_admin_agents_action_is_attributed_to_it() {
    let db = Db::new();
    db.create_agent("boss", &AgentSpec { can: Some(vec![Can::Admin]), ..Default::default() }).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("boss".into());
    s.run(&db, "create agent minion", &[]).unwrap();
    let t = db.security_events(100);
    let made = t.iter().find(|e| e.action == "create agent" && e.target == "minion").expect("recorded");
    assert_eq!(made.actor, "boss", "attributed to the admin agent that did it");
}

#[test]
fn a_non_admin_agent_cannot_read_the_security_audit() {
    let db = Db::new();
    db.create_agent("bot", &AgentSpec { can: Some(vec![Can::Read, Can::Fork, Can::WriteOwn]), ..Default::default() })
        .unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("bot".into());
    assert_eq!(s.run(&db, "show security audit", &[]).unwrap_err().code, "42501");
}

// ---------------------------------------------------------------------------------------------
// Gate S's statement classification (CHRONOS-PLAN-2026-11-17-r3.md, batch S5; approved decision 5): every kind of
// schema change, run by an agent held to a tenant in a world of its tenant, is refused when it touches a table held
// to the tenant (it has the tenant's column, or a foreign key to one that has), anything that exists in main, or
// privileges, roles, row security or shared definitions; and allowed on a table the agent made in its world without
// the tenant's column. A statement kind missing from the lists below is not classified: add it to one of them.
// ---------------------------------------------------------------------------------------------

fn tenant_setup() -> Db {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table leads (id int primary key, org_id int not null, name text, owner text);
         create table notes (id int primary key, lead_id int references leads (id), body text);
         create table plans (id int primary key, title text);
         create index leads_name on leads (name);
         create view lead_view as select * from leads;
         create materialized view lead_mv as select id, org_id, name from leads;
         create sequence seq1;
         create type mood as enum ('ok', 'sad');
         create domain posint as int check (value > 0);
         create schema extra;
         create function f1() returns int language sql as $$ select 1 $$;
         create function touch() returns trigger language plpgsql as $$ begin return new; end $$;
         create trigger leads_touch before update on leads for each row execute function touch();
         create agent run42 with (can = 'read,fork,write_own,merge_own', tenant = 'org_id = 42')",
        &[],
    )
    .unwrap();
    db
}

fn agent_in_world(db: &Db) -> Session {
    let mut s = Session::new(MAIN);
    s.agent = Some("run42".into());
    s.run(db, "create world w; use world w", &[]).unwrap();
    s
}

/// Schema changes of held tables, of objects in main, and of privileges: refused (42501), each in a fresh world.
const REFUSED: &[&str] = &[
    // held tables: the tenant's column, and a foreign key to it
    "alter table leads alter column org_id type bigint using 42",
    "alter table leads add column extra text",
    "alter table leads drop column owner",
    "alter table leads rename column owner to holder",
    "alter table leads add constraint c1 check (id > 0)",
    "alter table leads rename to leads2",
    "alter table notes alter column lead_id drop not null",
    "alter table notes drop constraint notes_lead_id_fkey",
    "create unique index leads_owner on leads (owner)",
    "create index notes_body on notes (body)",
    "drop index leads_name",
    "alter index leads_name rename to leads_name2",
    "truncate leads",
    "truncate notes",
    "drop table leads cascade",
    "drop table notes",
    "create trigger t2 before insert on leads for each row execute function touch()",
    "drop trigger leads_touch on leads",
    "alter trigger leads_touch on leads rename to t3",
    // objects in main, held or not
    "alter table plans add column price int",
    "drop table plans",
    "truncate plans",
    "create or replace view lead_view as select id from leads",
    "alter view lead_view rename to lv",
    "drop view lead_view",
    "refresh materialized view lead_mv",
    "drop materialized view lead_mv",
    "alter sequence seq1 restart with 100",
    "drop sequence seq1",
    // shared definitions, privileges, roles, row security
    "alter type mood add value 'mad'",
    "drop type mood",
    "alter domain posint drop constraint posint_check",
    "drop schema extra cascade",
    "drop function f1()",
    "create or replace function f1() returns int language sql as $$ select 2 $$",
    "alter function f1() security definer",
    "create role r1",
    "grant select on leads to public",
    "revoke select on leads from public",
    "alter table leads owner to r1",
    "alter table leads enable row level security",
    "create policy p1 on leads using (true)",
];

/// Changes the agent may make: of tables it made in its world without the tenant's column.
const ALLOWED: &[&str] = &[
    "create table scratch (id int primary key, v text)",
    "create table scratch (id int primary key, v text); alter table scratch add column w int",
    "create table scratch (id int primary key, v text); create index scratch_v on scratch (v)",
    "create table scratch (id int primary key, v text); truncate scratch",
    "create table scratch (id int primary key, v text); drop table scratch",
    "create view my_view as select id, name from leads",
];

#[test]
fn statement_classification_complete() {
    let db = tenant_setup();
    let mut failures = vec![];
    for (i, sql) in REFUSED.iter().enumerate() {
        let mut s = agent_in_world(&db);
        match s.run(&db, sql, &[]) {
            Err(e) if e.code == "42501" => {}
            Err(e) => failures.push(format!("refused list: {sql}: error {} (not 42501): {}", e.code, e.message)),
            Ok(_) => failures.push(format!("refused list: {sql}: allowed")),
        }
        s.run(&db, "use world main", &[]).unwrap();
        run(&db, MAIN, "drop world w", &[]).unwrap_or_else(|e| panic!("{i}: drop world w: {}", e.message));
    }
    for sql in ALLOWED {
        let mut s = agent_in_world(&db);
        if let Err(e) = s.run(&db, sql, &[]) {
            failures.push(format!("allowed list: {sql}: refused {}: {}", e.code, e.message));
        }
        s.run(&db, "use world main", &[]).unwrap();
        run(&db, MAIN, "drop world w", &[]).unwrap();
    }
    assert!(failures.is_empty(), "{} misclassified:\n{}", failures.len(), failures.join("\n"));
}

/// PR #362 review: a statement on a view with an INSTEAD OF trigger runs the trigger as an internal statement
/// (`FireInstead`); a tenant agent writing its own tenant's rows through such a view in its world is not changing
/// rules.
#[test]
fn instead_of_triggers_run_for_a_tenant_agent() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table leads (id int primary key, org_id int not null, name text);
         create view lead_names as select id, org_id, name from leads;
         create function put_lead() returns trigger language plpgsql as $$
           begin insert into leads values (new.id, new.org_id, new.name); return new; end $$;
         create trigger lead_names_put instead of insert on lead_names for each row execute function put_lead();
         create agent run42 with (can = 'read,fork,write_own,merge_own', tenant = 'org_id = 42')",
        &[],
    )
    .unwrap();
    let mut s = agent_in_world(&db);
    s.run(&db, "insert into lead_names values (1, 42, 'acme')", &[])
        .unwrap_or_else(|e| panic!("{}: {}", e.code, e.message));
    let out = s.run(&db, "select name from leads", &[]).unwrap();
    assert_eq!(out[0].rows[0][0].to_text(), "acme");
    // the rules themselves stay out of its reach
    assert_eq!(s.run(&db, "drop trigger lead_names_put on lead_names", &[]).unwrap_err().code, "42501");
}
