//! The agent gateway's red-team suite (docs/designs/agent-gateway-build.md, E3's exit test): an agent held to one
//! tenant (`CREATE AGENT ... WITH (tenant = 'org_id = 42')`), told by a prompt injection to reach another tenant's
//! rows, gets nothing of them: not in rows, joins, unions, CTEs, windows or aggregates; not through views, functions,
//! time travel, a copy or a sample; not by changing settings; not in error messages, plans or catalog figures; and
//! no write of it reaches them. Each probe runs in main and in a world the agent forked.

mod common;
#[path = "common/write_matrix.rs"]
mod write_matrix;

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// What tenant 7's rows hold: none of it may show to tenant 42's agent, anywhere.
const SECRETS: [&str; 3] = ["globex", "umbrella", "eve"];

/// On disk, so time travel (`AS OF`) is there to probe; a folder per test.
fn setup(tag: &str) -> Db {
    let dir = std::env::temp_dir().join(format!("chronos-tenant-isolation-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    run(
        &db,
        MAIN,
        "create table leads (id int primary key, org_id int not null, name text, owner text);
         create table notes (id int primary key, org_id int not null, lead_id int, body text);
         create table plans (id int primary key, title text);
         insert into leads values (1, 42, 'acme', 'bo'), (2, 42, 'initech', 'bo'), (3, 7, 'globex', 'eve'),
                                  (4, 7, 'umbrella', 'eve');
         insert into notes values (1, 42, 1, 'call'), (2, 7, 3, 'globex renewal');
         insert into plans values (1, 'free'), (2, 'pro');
         create view all_leads as select * from leads;
         create function count_leads() returns bigint language sql security definer as $$ select count(*) from leads $$;
         create function lead_names() returns setof text language sql as $$ select name from leads order by id $$;
         create agent run42 with (can = 'read,fork,write_own,merge_own,write_main', tenant = 'org_id = 42')",
        &[],
    )
    .unwrap();
    db
}

fn agent() -> Session {
    let mut s = Session::new(MAIN);
    s.agent = Some("run42".into());
    s
}

/// Everything a statement shows the agent: its rows' text, or its error's message.
fn shown(db: &Db, s: &mut Session, sql: &str) -> String {
    match s.run(db, sql, &[]) {
        Ok(out) => out
            .iter()
            .flat_map(|r| r.rows.iter().map(|row| row.iter().map(|v| v.to_text()).collect::<Vec<_>>().join("|")))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => format!("ERROR {}: {}", e.code, e.message),
    }
}

/// The first value of the statement's last result, as text, or its error.
fn cell(db: &Db, s: &mut Session, sql: &str) -> String {
    match s.run(db, sql, &[]) {
        Ok(mut out) => out.pop().and_then(|r| r.rows.first().map(|row| row[0].to_text())).unwrap_or_default(),
        Err(e) => format!("ERROR {}: {}", e.code, e.message),
    }
}

fn no_secret(what: &str, text: &str) {
    let text_l = text.to_lowercase();
    let words: Vec<&str> = text_l.split(|c: char| !c.is_alphanumeric()).collect();
    for s in SECRETS {
        assert!(!words.contains(&s), "{what} shows tenant 7's {s:?}:\n{text}");
    }
}

/// Runs `f` as the agent in main, then in a world it forked.
fn both(db: &Db, f: impl Fn(&mut Session, &str)) {
    let mut s = agent();
    f(&mut s, "main");
    s.run(db, "create world probe", &[]).unwrap();
    s.run(db, "use world probe", &[]).unwrap();
    f(&mut s, "a world");
    s.run(db, "use world main", &[]).unwrap();
    run(db, MAIN, "drop world probe", &[]).unwrap();
}

#[test]
fn reads_reach_nothing_of_another_tenant() {
    let db = setup("reads_reach_");
    // (time travel a second back reaches the rows as set up)
    std::thread::sleep(std::time::Duration::from_millis(1200));
    {
        let mut s = agent();
        assert_eq!(cell(&db, &mut s, "select count(*) from leads as of '-1 second'"), "2", "time travel");
        no_secret("time travel", &shown(&db, &mut s, "select * from leads as of '-1 second'"));
    }
    both(&db, |s, at| {
        let read = [
            "select * from leads",
            "select * from leads where org_id = 7",
            "select * from leads where org_id <> 42 or true",
            "select name from leads union select name from leads where org_id = 7",
            "select name from leads union all select owner from leads",
            "select * from leads a join leads b on a.id <> b.id",
            "select * from leads l left join notes n on n.lead_id = l.id",
            "select * from notes",
            "select (select string_agg(name, ',') from leads)",
            "select string_agg(name, ',' order by id) from leads",
            "with recursive x as (select id, name from leads union select id + 100, name from x where id < 50) select * from x",
            "select name, count(*) over () from leads",
            "select * from leads tablesample system (100)",
            "select * from all_leads",
            "select * from lead_names()",
            "select * from leads as of '-1 second'",
            "select * from leads for system_time as of now()",
            "select * from leads order by id for update",
            "select l.*, p.* from leads l cross join plans p",
            "select * from leads where name like '%glo%'",
            "select row_to_json(l) from leads l",
            "select json_agg(l) from leads l",
            "select * from leads where id in (select lead_id from notes)",
            "explain select * from leads",
            "explain analyze select * from leads",
            "select relname, reltuples from pg_class where relname in ('leads', 'notes')",
            "select * from pg_stat_user_tables where relname = 'leads'",
            "select * from pg_stats where tablename = 'leads'",
        ];
        for q in read {
            no_secret(&format!("{q} ({at})"), &shown(&db, s, q));
        }
        // counts are the tenant's, however they're taken
        for q in [
            "select count(*) from leads",
            "select count(*) from all_leads",
            "select count_leads()",
            "select count(*) from (select * from leads union all select * from leads) x",
            "select count(*) from leads, plans",
            "select max(id) from leads",
            "select count(distinct org_id) from leads",
        ] {
            let v = cell(&db, s, q);
            assert!(["2", "4", "1"].contains(&v.as_str()), "{q} ({at}): {v}");
        }
        assert_eq!(cell(&db, s, "select count(*) from leads"), "2", "{at}");
        assert_eq!(cell(&db, s, "select count(*) from all_leads"), "2", "a view the admin made ({at})");
        assert_eq!(cell(&db, s, "select count_leads()"), "2", "a security definer function the admin made ({at})");
        assert_eq!(cell(&db, s, "select max(id) from leads"), "2", "{at}");
        assert_eq!(
            cell(&db, s, "select count(*) from leads, plans"),
            "4",
            "a table without the column reads whole ({at})"
        );
        assert_eq!(cell(&db, s, "select count(*) from notes"), "1", "{at}");
    });
}

#[test]
fn settings_and_roles_dont_lift_the_tenant() {
    let db = setup("settings_and");
    both(&db, |s, at| {
        for set in [
            "set chronos.tenant = 'org_id = 7'",
            "set chronos.tenant = ''",
            "set row_security = off",
            "reset all",
            "set role chronos",
            "set session authorization chronos",
        ] {
            let said = shown(&db, s, set);
            no_secret(&format!("{set} ({at})"), &said);
            let n = cell(&db, s, "select count(*) from leads");
            assert_eq!(n, "2", "after {set} ({at}): {n}");
            no_secret(&format!("after {set} ({at})"), &shown(&db, s, "select * from leads"));
        }
    });
}

#[test]
fn errors_say_nothing_of_another_tenants_rows() {
    let db = setup("errors_say_n");
    both(&db, |s, at| {
        for q in [
            // another tenant's key: the key is taken, and the error may say so, but nothing of the row
            "insert into leads values (3, 42, 'x', 'y')",
            "select 1 / (select count(*) - 2 from leads)",
            "select cast(name as int) from leads where id = 3",
            "select * from leads where id = (select id from leads where org_id = 7)",
            "update leads set id = 3 where id = 1",
        ] {
            no_secret(&format!("{q} ({at})"), &shown(&db, s, q));
        }
    });
}

#[test]
fn writes_reach_nothing_of_another_tenant() {
    let db = setup("writes_reach");
    let before = shown(&db, &mut Session::new(MAIN), "select * from leads where org_id = 7 order by id");
    let mut s = agent();
    s.run(&db, "create world w; use world w", &[]).unwrap();
    for q in [
        "update leads set name = 'pwned'",
        "update leads set name = 'pwned' where org_id = 7",
        "update leads set org_id = 42 where id = 3",
        "update leads set name = 'pwned' from plans where leads.id = 3",
        "delete from leads where org_id = 7",
        "delete from leads where id = 3 returning *",
        "insert into leads values (3, 42, 'x', 'y') on conflict (id) do update set name = 'pwned'",
        "insert into leads values (9, 7, 'planted', 'mallory')",
        "insert into leads select id + 100, 7, name, owner from leads",
        "update notes set body = 'pwned' where lead_id = 3",
        "delete from notes",
        "merge into leads t using (select 3 as id) s on t.id = s.id when matched then update set name = 'pwned'",
    ] {
        let said = shown(&db, &mut s, q);
        no_secret(q, &said);
    }
    // the world's view of tenant 7, as the database's own user sees it, is as before; nothing was planted for it
    let w = shown(&db, &mut Session::new("w"), "select * from leads where org_id = 7 order by id");
    assert_eq!(w, before, "the world");
    assert_eq!(shown(&db, &mut Session::new("w"), "select body from notes where org_id = 7"), "globex renewal");
    // and merged into main, nothing of tenant 7 changes there
    let _ = s.run(&db, "use world main; merge world w", &[]);
    let m = shown(&db, &mut Session::new(MAIN), "select * from leads where org_id = 7 order by id");
    assert_eq!(m, before, "main");
}

/// What holds a table to the tenant is its column (or a foreign key to a table that has it): the agent may not let
/// go of it in a world it forked, where the table would then count as shared by every tenant and show their rows.
#[test]
fn the_agent_cannot_let_go_of_its_tenant_by_changing_a_table() {
    let db = setup("ddl");
    let mut s = agent();
    s.run(&db, "create world w; use world w", &[]).unwrap();
    for sql in [
        "alter table leads rename column org_id to o",
        "alter table leads drop column org_id",
        "alter table leads drop column org_id cascade",
        "alter table notes rename column org_id to o",
    ] {
        let out = shown(&db, &mut s, sql);
        assert!(out.starts_with("ERROR 42501"), "{sql}: {out}");
        no_secret(sql, &shown(&db, &mut s, "select * from leads"));
        no_secret(sql, &shown(&db, &mut s, "select * from notes"));
    }
    // since the security release (approved decision 5) no change to a held table's definition is its to make:
    // adding a column, the one this test allowed before, is refused too (see `ddl_on_unheld_table_allowed`)
    let out = shown(&db, &mut s, "alter table leads add column note text");
    assert!(out.starts_with("ERROR 42501"), "{out}");
    no_secret("after adding a column", &shown(&db, &mut s, "select * from leads"));
}

// ---------------------------------------------------------------------------------------------
// Gate S (CHRONOS-PLAN-2026-11-17-r3.md section D, r3.1 section 1). Added in batch S0, made to pass by
// batches S1 to S5.
// ---------------------------------------------------------------------------------------------

/// Tenant 7's rows in main as the database's own user sees them.
fn tenant7_in_main(db: &Db) -> String {
    shown(db, &mut Session::new(MAIN), "select * from leads where org_id = 7 order by id")
}

/// Runs `sql` as the agent in a world it forked, then tries to merge that world into main. The
/// statement must be refused (42501), the agent must still see nothing of tenant 7 in its world,
/// and tenant 7's rows in main must be as they were, whatever the merge did.
fn ddl_refused(tag: &str, sql: &str) {
    let db = setup(tag);
    let before = tenant7_in_main(&db);
    let mut s = agent();
    s.run(&db, "create world w; use world w", &[]).unwrap();
    let out = shown(&db, &mut s, sql);
    assert!(out.starts_with("ERROR 42501"), "{sql}: {out}");
    no_secret(sql, &shown(&db, &mut s, "select * from leads"));
    no_secret(sql, &shown(&db, &mut s, "select * from notes"));
    let _ = s.run(&db, "use world main; merge world w", &[]);
    assert_eq!(tenant7_in_main(&db), before, "{sql}: tenant 7's rows in main");
}

/// Finding A: `ALTER COLUMN ... TYPE ... USING <constant>` rewrites every row's tenant column in
/// the agent's world, after which its rows of every tenant count as its own.
#[test]
fn alter_type_using_refused() {
    ddl_refused("alter_type", "alter table leads alter column org_id type integer using 42");
    ddl_refused("alter_type_bigint", "alter table leads alter column org_id type bigint using 42");
    ddl_refused("alter_type_notes", "alter table notes alter column org_id type integer using 42");
}

#[test]
fn add_check_constraint_refused() {
    ddl_refused("add_check", "alter table leads add constraint only42 check (org_id = 42)");
}

/// A unique index over every tenant's rows would answer "is this value taken?" for other tenants.
#[test]
fn create_unique_index_refused() {
    ddl_refused("unique_index", "create unique index leads_name on leads (name)");
}

#[test]
fn truncate_refused() {
    ddl_refused("truncate", "truncate leads");
    ddl_refused("truncate_notes", "truncate notes");
}

#[test]
fn drop_table_cascade_refused() {
    ddl_refused("drop_cascade", "drop table leads cascade");
    ddl_refused("drop", "drop table notes");
}

/// Kept from before the security release (also covered by
/// `the_agent_cannot_let_go_of_its_tenant_by_changing_a_table`).
#[test]
fn rename_and_drop_column_refused() {
    for (i, sql) in [
        "alter table leads rename column org_id to o",
        "alter table leads drop column org_id",
        "alter table leads drop column org_id cascade",
        "alter table notes rename column org_id to o",
    ]
    .into_iter()
    .enumerate()
    {
        ddl_refused(&format!("rename_drop_{i}"), sql);
    }
}

/// A table the agent made in its own world is held by no tenant column: changing it is its own.
#[test]
fn ddl_on_unheld_table_allowed() {
    let db = setup("unheld_ddl");
    let mut s = agent();
    s.run(&db, "create world w; use world w", &[]).unwrap();
    for sql in [
        "create table scratch (id int primary key, v text)",
        "alter table scratch add column n int",
        "create unique index scratch_v on scratch (v)",
        "alter table scratch add constraint pos check (n > 0)",
        "truncate scratch",
        "drop table scratch",
    ] {
        let out = shown(&db, &mut s, sql);
        assert!(!out.starts_with("ERROR"), "{sql}: {out}");
    }
}

/// Finding B (reproduced on the 0.5.5 release binary, 2026-11-17): a simulation's kept world is
/// forked without the agent's tenant, so a script's DDL and the world's merge are held to nothing.
#[test]
fn simulate_keep_merge_refused() {
    let db = setup("sim_keep");
    let before = tenant7_in_main(&db);
    let mut s = agent();
    let _ = shown(
        &db,
        &mut s,
        "simulate 1 worlds as z run $$alter table leads alter column org_id type integer using 42$$ \
         score $$select 1$$ keep 1",
    );
    if let Ok(w) = db.world("z_0") {
        let scope = w.scope.unwrap_or_default();
        assert_eq!(scope["tenant"]["column"], "org_id", "kept world's scope: {scope}");
        assert_eq!(scope["tenant"]["value"], 42, "kept world's scope: {scope}");
        let mut k = agent();
        if k.run(&db, "use world z_0", &[]).is_ok() {
            no_secret("the kept world", &shown(&db, &mut k, "select * from leads"));
        }
    }
    let _ = shown(&db, &mut s, "merge world z_0");
    assert_eq!(tenant7_in_main(&db), before, "tenant 7's rows in main");
    no_secret("main after the merge", &shown(&db, &mut s, "select * from leads"));
}

/// The same through REPLAY: a person's simulation whose script rewrites the tenant column, replayed
/// and kept by the tenant agent, then merged.
#[test]
fn replay_as_merge_refused() {
    let db = setup("replay_as");
    let before = tenant7_in_main(&db);
    run(
        &db,
        MAIN,
        "simulate 1 worlds as p run $$alter table leads alter column org_id type integer using 42$$ \
         score $$select 1$$ keep 1",
        &[],
    )
    .unwrap();
    let mut s = agent();
    let _ = shown(&db, &mut s, "replay world p_0 as mine");
    if let Ok(w) = db.world("mine") {
        let scope = w.scope.unwrap_or_default();
        assert_eq!(scope["tenant"]["value"], 42, "replayed world's scope: {scope}");
        let mut k = agent();
        if k.run(&db, "use world mine", &[]).is_ok() {
            no_secret("the replayed world", &shown(&db, &mut k, "select * from leads"));
        }
    }
    let _ = shown(&db, &mut s, "merge world mine");
    assert_eq!(tenant7_in_main(&db), before, "tenant 7's rows in main");
    no_secret("main after the merge", &shown(&db, &mut s, "select * from leads"));
}

/// An agent like run42, also allowed to restore, for the past-base, restore and undo cases.
fn restorer(db: &Db) -> Session {
    run(
        db,
        MAIN,
        "create agent res42 with (can = 'read,fork,write_own,merge_own,write_main,restore', tenant = 'org_id = 42')",
        &[],
    )
    .unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some("res42".into());
    s
}

/// A world (or simulation) from a past moment holds every tenant's rows as they were then.
#[test]
fn past_base_refused_for_tenant_agent() {
    let db = setup("past_base");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let mut s = restorer(&db);
    for sql in [
        "fork world 'main@-1 second' as past",
        "create world past2 from 'main@-1 second'",
        "simulate 1 worlds from 'main@-1 second' as ps run $$select 1$$ score $$select 1$$ keep 1",
    ] {
        let out = shown(&db, &mut s, sql);
        assert!(out.starts_with("ERROR 42501"), "{sql}: {out}");
    }
    for w in ["past", "past2", "ps_0"] {
        assert!(db.world(w).is_err(), "{w} was made");
    }
}

#[test]
fn restore_refused_for_tenant_agent() {
    let db = setup("restore");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    run(&db, MAIN, "update leads set owner = 'later' where org_id = 7", &[]).unwrap();
    let before = tenant7_in_main(&db);
    let mut s = restorer(&db);
    s.run(&db, "create world mine", &[]).unwrap();
    for sql in ["restore world main to '-1 second'", "restore world mine to '-1 second'"] {
        let out = shown(&db, &mut s, sql);
        assert!(out.starts_with("ERROR 42501"), "{sql}: {out}");
    }
    assert_eq!(tenant7_in_main(&db), before, "tenant 7's rows in main");
}

/// Undoing a merge puts back whole rows as they were before it, whoever changed them since.
#[test]
fn undo_refused_for_tenant_agent() {
    let db = setup("undo");
    let mut s = restorer(&db);
    s.run(&db, "create world mine; use world mine; update leads set owner = 'ann' where id = 1", &[]).unwrap();
    s.run(&db, "use world main; merge world mine", &[]).unwrap();
    let before = shown(&db, &mut Session::new(MAIN), "select * from leads order by id");
    for sql in ["undo merge of world mine", "undo merge of world mine skip changed", "undo agent res42 since '-1 hour'"]
    {
        let out = shown(&db, &mut s, sql);
        assert!(out.starts_with("ERROR 42501"), "{sql}: {out}");
    }
    assert_eq!(shown(&db, &mut Session::new(MAIN), "select * from leads order by id"), before);
}

/// Every way a tenant-42 agent can send a change ends with tenant 7's rows in main unchanged.
/// (Over SQL in-process; HTTP and MCP cases are in `cross_tenant_write_matrix_http_mcp`.)
#[test]
fn cross_tenant_write_matrix() {
    for (i, (what, steps)) in write_matrix::CASES.iter().enumerate() {
        let db = setup(&format!("wm{i}"));
        // run as an agent with every right a tenant agent can hold short of admin
        run(
            &db,
            MAIN,
            "create agent all42 with (can = 'read,fork,write_own,write,write_main,merge_own,merge,restore,outbox', \
             tenant = 'org_id = 42')",
            &[],
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let before = tenant7_in_main(&db);
        let mut s = Session::new(MAIN);
        s.agent = Some("all42".into());
        for step in steps.iter() {
            let said = shown(&db, &mut s, step);
            no_secret(&format!("{what}: {step}"), &said);
        }
        assert_eq!(tenant7_in_main(&db), before, "{what}: tenant 7's rows in main");
        let notes = shown(&db, &mut Session::new(MAIN), "select body from notes where org_id = 7");
        assert_eq!(notes, "globex renewal", "{what}: tenant 7's notes in main");
    }
}

// --- r3.1 section 1: main_read_path_matrix. Main is GLOBAL; the tenant agent reads it only
// through tenant row filtering, on every path. One test per path, so a failure names it. ---

/// A database for the read matrix: `setup`'s tables, with indexes, a vector table, a materialized
/// view, a serial table and statistics, all holding both tenants' rows.
fn matrix(tag: &str) -> Db {
    let db = setup(&format!("matrix_{tag}"));
    run(
        &db,
        MAIN,
        "create index leads_by_name on leads (name);
         create index leads_by_owner on leads (owner, name);
         create index leads_text on leads using gin (to_tsvector('english', name));
         create table docs (id int primary key, org_id int not null, body text, emb vector(2));
         insert into docs values (1, 42, 'acme memo', '[1,0]'), (2, 7, 'globex memo', '[0.9,0.1]'),
                                 (3, 7, 'umbrella memo', '[1,0.05]');
         create materialized view lead_mv as select id, org_id, name from leads;
         create materialized view name_mv as select name from leads;
         create table events (id serial primary key, org_id int not null, what text);
         insert into events (org_id, what) values (7, 'globex signed'), (7, 'umbrella churned'), (42, 'acme call');
         analyze leads; analyze docs",
        &[],
    )
    .unwrap();
    db
}

/// Every statement in `qs`, as the agent in main: shows nothing of tenant 7.
fn reads_clean(db: &Db, qs: &[&str]) {
    let mut s = agent();
    for q in qs {
        no_secret(&format!("{q} (main)"), &shown(db, &mut s, q));
    }
}

#[test]
fn main_read_path_matrix_select_join_subquery_aggregate() {
    let db = matrix("select");
    reads_clean(
        &db,
        &[
            "select * from leads",
            "select l.name, n.body from leads l join notes n on n.lead_id = l.id",
            "select * from leads where id in (select lead_id from notes where org_id = 7)",
            "select org_id, string_agg(name, ',') from leads group by org_id",
            "select name, rank() over (order by id) from leads",
            "with x as (select * from leads where org_id = 7) select * from x",
            "select (select max(name) from leads where org_id = 7)",
            "select * from docs",
        ],
    );
    let mut s = agent();
    assert_eq!(cell(&db, &mut s, "select count(*) from leads"), "2");
    assert_eq!(cell(&db, &mut s, "select count(*) from docs"), "1");
    assert_eq!(cell(&db, &mut s, "select count(*) from events"), "1");
}

#[test]
fn main_read_path_matrix_views() {
    let db = matrix("views");
    run(
        &db,
        MAIN,
        "create view secure_leads with (security_barrier = true) as select * from leads;
         create view lead_names_v as select name from leads",
        &[],
    )
    .unwrap();
    reads_clean(&db, &["select * from all_leads", "select * from secure_leads", "select * from lead_names_v"]);
    let mut s = agent();
    assert_eq!(cell(&db, &mut s, "select count(*) from lead_names_v"), "2");
}

/// A materialized view is stored rows: refreshed by its owner, it holds every tenant's. Read by a
/// tenant agent it is filtered (it has the tenant column) or refused (it hasn't).
#[test]
fn main_read_path_matrix_materialized_view() {
    let db = matrix("mview");
    let mut s = agent();
    no_secret("lead_mv", &shown(&db, &mut s, "select * from lead_mv"));
    let out = shown(&db, &mut s, "select * from name_mv");
    no_secret("name_mv", &out);
    assert!(out.starts_with("ERROR 42501") || out.lines().count() <= 2, "name_mv: {out}");
}

#[test]
fn main_read_path_matrix_returning_and_writable_ctes() {
    let db = matrix("returning");
    let mut s = agent();
    // main is read-only to a tenant agent: it changes main only by merging a world of its tenant
    for q in [
        "insert into leads values (9, 42, 'x', 'y') returning *",
        "update leads set owner = owner returning *",
        "with d as (delete from leads where org_id = 7 returning *) select * from d",
        "insert into notes select id + 10, 42, id, name from leads returning *",
        "update leads set owner = n.body from notes n where n.lead_id = leads.id returning *",
    ] {
        let out = shown(&db, &mut s, q);
        no_secret(q, &out);
        assert!(out.starts_with("ERROR 42501"), "{q}: {out}");
    }
    // in its own world the same statements reach only its rows
    s.run(&db, "create world w; use world w", &[]).unwrap();
    for q in [
        "update leads set owner = owner returning *",
        "with d as (delete from leads where org_id = 7 returning *) select * from d",
        "insert into notes select id + 10, 42, id, name from leads returning *",
    ] {
        no_secret(&format!("{q} (world)"), &shown(&db, &mut s, q));
    }
}

#[test]
fn main_read_path_matrix_time_travel_as_of() {
    let db = matrix("as_of");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    run(&db, MAIN, "update leads set name = 'globex2' where id = 3", &[]).unwrap();
    reads_clean(
        &db,
        &[
            "select * from leads as of '-1 second'",
            "select * from leads for system_time as of now() - interval '1 second'",
            "select o.name from leads as of '-1 second' o join leads n on n.id = o.id",
        ],
    );
}

#[test]
fn main_read_path_matrix_world_at_time() {
    let db = matrix("at_time");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let mut s = agent();
    let used = shown(&db, &mut s, "use world 'main@-1 second'");
    if !used.starts_with("ERROR") {
        no_secret("a past session", &shown(&db, &mut s, "select * from leads"));
        s.run(&db, "use world main", &[]).unwrap();
    }
    for q in ["fork world 'main@-1 second' as past_w", "create world past_w2 from 'main@-1 second'"] {
        let out = shown(&db, &mut s, q);
        assert!(out.starts_with("ERROR 42501"), "{q}: {out}");
    }
}

/// Main's changes are every tenant's: its diff and history are refused to a tenant agent.
#[test]
fn main_read_path_matrix_diff_and_history_of_main_refused() {
    let db = matrix("diff_main");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    run(&db, MAIN, "update leads set owner = 'zed' where org_id = 7", &[]).unwrap();
    let mut s = agent();
    for q in [
        "diff world 'main@-1 second' to main",
        "diff world main",
        "show history limit 50",
        "show audit",
        "select * from leads as of '-1 second' except select * from leads",
    ] {
        let out = shown(&db, &mut s, q);
        no_secret(q, &out);
        if !q.starts_with("select") {
            assert!(out.starts_with("ERROR 42501"), "{q}: {out}");
        }
    }
}

#[test]
fn main_read_path_matrix_simulate_script_filtered() {
    let db = matrix("sim_filtered");
    let mut s = agent();
    let out = shown(&db, &mut s, "simulate 2 worlds as f run $$select 1$$ score $$select count(*) from leads$$ keep 0");
    no_secret("simulate", &out);
    for line in out.lines() {
        let score = line.split('|').nth(4).unwrap_or_default();
        assert_eq!(score, "2", "score counts only tenant 42's rows: {out}");
    }
    let out = shown(
        &db,
        &mut s,
        "simulate 1 worlds as g run $$select 1$$ score $$select count(*) from leads where name = 'globex'$$ keep 0",
    );
    assert_eq!(out.lines().next().unwrap_or_default().split('|').nth(4).unwrap_or_default(), "0", "{out}");
}

#[test]
fn main_read_path_matrix_index_scan() {
    let db = matrix("index");
    reads_clean(
        &db,
        &[
            "select * from leads where name = 'globex'",
            "select * from leads where id = 3",
            "select * from leads where id between 1 and 10",
            "select * from leads where name like 'glo%'",
            "select * from leads where owner = 'eve'",
        ],
    );
}

#[test]
fn main_read_path_matrix_index_only_scan() {
    let db = matrix("index_only");
    reads_clean(
        &db,
        &[
            "select name from leads where name > ''",
            "select owner, name from leads where owner > '' order by owner, name",
            "select id from leads where id >= 3",
            "select distinct owner from leads",
        ],
    );
    let mut s = agent();
    assert_eq!(cell(&db, &mut s, "select count(*) from leads where name > ''"), "2");
    assert_eq!(cell(&db, &mut s, "select count(*) from leads where id >= 3"), "0");
}

#[test]
fn main_read_path_matrix_vector_and_text_search() {
    let db = matrix("search");
    reads_clean(
        &db,
        &[
            "select body from docs order by emb <-> '[1,0]' limit 3",
            "select body from docs order by emb <=> '[1,0]' limit 3",
            "select name from leads where to_tsvector('english', name) @@ to_tsquery('globex')",
            "select name, ts_rank(to_tsvector('english', name), to_tsquery('globex | acme')) from leads",
            "select name from leads where name % 'globe'",
        ],
    );
}

/// Statistics hold values: most-common values, histograms. A tenant agent sees none of another
/// tenant's. (Totals such as `reltuples` are a known limitation if not filtered; see the S5 report.)
#[test]
fn main_read_path_matrix_pg_stats_and_estimates() {
    let db = matrix("stats");
    reads_clean(
        &db,
        &[
            "select * from pg_stats where tablename = 'leads'",
            "select * from pg_stats where tablename = 'docs'",
            "select most_common_vals::text, histogram_bounds::text from pg_stats",
            "select * from pg_stat_user_tables",
            "select * from pg_statistic",
            "show storage",
        ],
    );
}

#[test]
fn main_read_path_matrix_explain() {
    let db = matrix("explain");
    reads_clean(
        &db,
        &[
            // (the plan repeats the statement's own constants: a probe naming 'globex' would only echo itself;
            // row estimates are a known limitation, see the S5 report)
            "explain select * from leads where name like 'g%'",
            "explain analyze select * from leads where name like 'g%'",
            "explain (analyze, verbose) select * from docs order by emb <-> '[1,0]' limit 3",
            "explain verbose select * from lead_mv",
        ],
    );
}

/// A sequence's value is a total across tenants (how many rows were ever inserted); it holds no
/// row value. The test checks it shows nothing of tenant 7's rows; the total is a known limitation.
#[test]
fn main_read_path_matrix_sequence_values() {
    let db = matrix("sequences");
    reads_clean(
        &db,
        &["select last_value from events_id_seq", "select * from pg_sequences", "select currval('events_id_seq')"],
    );
}

/// Review notifications and the outbox carry other tenants' payloads.
#[test]
fn main_read_path_matrix_listen_and_outbox() {
    let db = matrix("outbox");
    run(&db, MAIN, "create world t7; use world t7; notify on merge jobs, 'globex renewal'", &[]).unwrap();
    run(&db, MAIN, "merge world t7", &[]).unwrap();
    let mut s = agent();
    for q in ["show outbox", "listen chronos_review_held", "claim outbox"] {
        let out = shown(&db, &mut s, q);
        no_secret(q, &out);
        assert!(out.starts_with("ERROR 42501"), "{q}: {out}");
    }
}

#[test]
fn main_read_path_matrix_errors() {
    let db = matrix("errors");
    reads_clean(
        &db,
        &[
            "select 1 / (select count(*) - 2 from leads)",
            "select cast(name as int) from leads where id = 3",
            "select * from leads where id = (select id from leads)",
            "select cast(body as int) from docs",
        ],
    );
}

// --- The same over HTTP and MCP: a server on the folder, the agent with its own token. ---

/// `setup`'s database, closed, with the matrix's extra tables and an agent `api42` like run42;
/// its folder and api42's token.
fn on_disk(tag: &str) -> (PathBuf, String) {
    let db = matrix(tag);
    let out = run(
        &db,
        MAIN,
        "create agent api42 with (can = 'read,fork,write_own,merge_own,write_main', tenant = 'org_id = 42')",
        &[],
    )
    .unwrap();
    let token = out[0].rows[0].iter().map(|v| v.to_text()).find(|t| t.starts_with("chronos_")).expect("a token");
    drop(db);
    (std::env::temp_dir().join(format!("chronos-tenant-isolation-matrix_{tag}-{}", std::process::id())), token)
}

struct Http {
    child: std::process::Child,
    port: u16,
}

impl Drop for Http {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http_server(dir: &std::path::Path) -> Http {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chronos"));
    cmd.arg("serve").arg(dir);
    let s = common::serve(&mut cmd, false);
    Http { child: s.child, port: s.http }
}

/// POST /v1/<op> with `body` as the agent: the status line and the response body.
fn post(port: u16, token: &str, op: &str, body: &str) -> (String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let req = format!(
        "POST /v1/{op} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out.lines().next().unwrap_or_default().to_string();
    let body = out.split_once("\r\n\r\n").map_or(String::new(), |(_, b)| b.to_string());
    (status, body)
}

/// MCP tool calls as `agent`, over stdio: each call's result text (or error).
fn mcp(dir: &std::path::Path, agent: &str, calls: &[(&str, &str)]) -> Vec<String> {
    let script: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            format!(
                r#"{{"jsonrpc":"2.0","id":{i},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
            )
        })
        .collect();
    let mut c = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .args(["mcp", dir.to_str().unwrap(), "--agent", agent])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(script.join("\n").as_bytes()).unwrap();
    let out = String::from_utf8(c.wait_with_output().unwrap().stdout).unwrap();
    out.lines().map(String::from).collect()
}

#[test]
fn main_read_path_matrix_http_row_api() {
    let (dir, token) = on_disk("http");
    let srv = http_server(&dir);
    for (op, body) in [
        ("get", r#"{"table":"leads","id":"3"}"#),
        ("get", r#"{"table":"docs","id":"2"}"#),
        ("find", r#"{"table":"leads","where":{"org_id":7}}"#),
        ("find", r#"{"table":"leads","text":"globex"}"#),
        ("find", r#"{"table":"docs","vector_field":"emb","vector":[1,0],"limit":3}"#),
        ("sql", r#"{"sql":"select * from leads"}"#),
        ("sql", r#"{"sql":"select * from leads as of '-1 second'"}"#),
        ("sql", r#"{"sql":"select * from pg_stats where tablename = 'leads'"}"#),
        ("diff", r#"{"branch":"main"}"#),
        ("history", r#"{"branch":"main"}"#),
    ] {
        let (status, out) = post(srv.port, &token, op, body);
        no_secret(&format!("HTTP {op} {body} ({status})"), &out);
    }
    // the row API is refused on a tenant table outright
    let (status, _) = post(srv.port, &token, "get", r#"{"table":"leads","id":"1"}"#);
    assert!(!status.contains(" 200 "), "get on a tenant table: {status}");
}

#[test]
fn main_read_path_matrix_mcp_tools() {
    let (dir, _) = on_disk("mcp");
    let out = mcp(
        &dir,
        "run42",
        &[
            ("get", r#"{"table":"leads","id":"3"}"#),
            ("find", r#"{"table":"leads","where":{"org_id":7}}"#),
            ("find", r#"{"table":"docs","vector_field":"emb","vector":[1,0],"limit":3}"#),
            ("describe", r#"{"table":"leads"}"#),
            ("sql", r#"{"sql":"select * from leads"}"#),
            ("sql", r#"{"sql":"select * from lead_mv"}"#),
            ("diff", r#"{"branch":"main"}"#),
            ("history", r#"{"branch":"main"}"#),
            (
                "simulate",
                r#"{"worlds":1,"prefix":"m","script":"select 1","score":"select count(*) from leads","keep":0}"#,
            ),
        ],
    );
    assert!(out.len() >= 9, "{out:?}");
    for line in &out {
        no_secret("MCP", line);
    }
}

/// The write matrix over HTTP and MCP: tenant 7's rows in main are unchanged after each.
#[test]
fn cross_tenant_write_matrix_http_mcp() {
    let (dir, token) = on_disk("writes_http");
    let tenant7 = |dir: &std::path::Path| {
        let db = Db::open(dir).unwrap();
        let t = tenant7_in_main(&db);
        drop(db);
        t
    };
    let before = tenant7(&dir);
    {
        let srv = http_server(&dir);
        for (op, body) in [
            ("put", r#"{"table":"leads","id":"3","record":{"id":3,"org_id":42,"name":"pwned","owner":"x"}}"#),
            ("delete", r#"{"table":"leads","id":"3"}"#),
            ("batch", r#"{"rows":[{"table":"leads","id":"4","record":null}]}"#),
            ("sql", r#"{"sql":"delete from leads where org_id = 7"}"#),
            ("fork", r#"{"name":"hw"}"#),
            ("sql", r#"{"sql":"alter table leads alter column org_id type integer using 42","branch":"hw"}"#),
            ("merge", r#"{"branch":"hw"}"#),
            (
                "simulate",
                r#"{"worlds":1,"prefix":"hz","script":"alter table leads alter column org_id type integer using 42","score":"select 1","keep":1}"#,
            ),
            ("merge", r#"{"branch":"hz_0"}"#),
            ("restore", r#"{"branch":"main","to":"-1 second"}"#),
        ] {
            let (status, out) = post(srv.port, &token, op, body);
            no_secret(&format!("HTTP {op} {body} ({status})"), &out);
        }
    }
    assert_eq!(tenant7(&dir), before, "after HTTP: tenant 7's rows in main");
    let out = mcp(
        &dir,
        "run42",
        &[
            ("put", r#"{"table":"leads","id":"3","record":{"id":3,"org_id":42,"name":"pwned","owner":"x"}}"#),
            ("delete", r#"{"table":"leads","id":"4"}"#),
            (
                "simulate",
                r#"{"worlds":1,"prefix":"mz","script":"alter table leads alter column org_id type integer using 42","score":"select 1","keep":1}"#,
            ),
            ("merge", r#"{"branch":"mz_0"}"#),
            ("fork", r#"{"name":"mw"}"#),
            ("sql", r#"{"sql":"truncate leads","branch":"mw"}"#),
            ("merge", r#"{"branch":"mw"}"#),
        ],
    );
    for line in &out {
        no_secret("MCP", line);
    }
    assert_eq!(tenant7(&dir), before, "after MCP: tenant 7's rows in main");
}

/// The creation matrix's HTTP rows: fork, simulate and replay as the agent over a server.
#[test]
fn creation_path_matrix_http() {
    let (dir, token) = on_disk("create_http");
    {
        let srv = http_server(&dir);
        for (op, body) in [
            ("fork", r#"{"name":"h1"}"#),
            ("simulate", r#"{"worlds":1,"prefix":"hs","script":"select 1","score":"select 1","keep":1}"#),
            ("replay", r#"{"world":"hs_0","as":"hr"}"#),
        ] {
            let (status, out) = post(srv.port, &token, op, body);
            assert!(status.contains(" 200 "), "{op}: {status} {out}");
        }
        for (op, body) in [
            ("fork", r#"{"name":"h2","from":"main@-1 second"}"#),
            ("fork", r#"{"name":"h3","meta":{"tenant":{"org_id":7}}}"#),
        ] {
            let (status, out) = post(srv.port, &token, op, body);
            assert!(!status.contains(" 200 "), "{op} {body}: {status} {out}");
        }
        // read back through the server (a fork is logged, not synced on its own: the server is stopped with SIGKILL)
        for w in ["h1", "hs_0", "hr"] {
            let (status, out) = post(srv.port, &token, "world", &format!(r#"{{"branch":"{w}"}}"#));
            assert!(status.contains(" 200 "), "HTTP: {w} was not made: {status} {out}");
            let world: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(world["owner"], "api42", "HTTP {w}: {out}");
            assert_eq!(world["scope"]["tenant"]["value"], 42, "HTTP {w}: {out}");
        }
    }
}

/// PR #362 review: transaction control is not a write. A tenant agent opens, saves and ends transactions on main
/// (main is read-only to it: a row it writes there is still refused, `Core::guard_main`).
#[test]
fn transactions_on_main_are_reads_for_a_tenant_agent() {
    let db = setup("tx_on_main");
    let mut s = agent();
    for sql in [
        "begin read only; select count(*) from leads; commit",
        "begin; select count(*) from leads; rollback",
        "begin; savepoint a; select 1; release a; savepoint b; rollback to b; commit",
        "start transaction read only; select name from leads order by id; end",
    ] {
        let out = s.run(&db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {} {}", e.code, e.message));
        no_secret(sql, &format!("{:?}", out.iter().flat_map(|r| r.rows.iter()).collect::<Vec<_>>()));
    }
    assert_eq!(cell(&db, &mut s, "begin; select count(*) from leads"), "2");
    s.run(&db, "rollback", &[]).unwrap();
    // a write inside one is refused, and main is as it was
    let before = cell(&db, &mut Session::new(MAIN), "select count(*) from leads");
    let wrote = s.run(&db, "begin; insert into leads values (9, 42, 'new', 'bo'); commit", &[]);
    let _ = s.run(&db, "rollback", &[]);
    assert_eq!(wrote.map(|_| ()).unwrap_err().code, "42501");
    assert_eq!(cell(&db, &mut Session::new(MAIN), "select count(*) from leads"), before);
}

/// Two of the agent's own worlds forked at different times differ by what merged into main between (tenant 7's
/// rows), and a world as it was holds its base then: a tenant agent diffs or compares only worlds from one state.
#[test]
fn own_worlds_of_different_moments_show_no_other_tenants_changes() {
    let db = setup("between");
    let mut s = agent();
    s.run(&db, "create world a", &[]).unwrap();
    run(&db, MAIN, "insert into leads values (5, 7, 'eve corp', 'eve')", &[]).unwrap(); // tenant 7's, merged
    s.run(&db, "create world b; create world c", &[]).unwrap();
    for sql in ["diff world a to b", "compare worlds a, b", "diff world a to b readers", "diff world 'b@-1 second'"] {
        let out = shown(&db, &mut s, sql);
        no_secret(sql, &out);
        assert!(out.starts_with("ERROR 42501") || !out.starts_with("ERROR"), "{sql}: {out}");
    }
    assert!(shown(&db, &mut s, "diff world a to b").starts_with("ERROR 42501"));
    // two worlds from one state compare
    assert!(!shown(&db, &mut s, "compare worlds b, c").starts_with("ERROR"));
    s.close(&db);
}
