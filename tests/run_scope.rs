//! A run's declared scope: a world forked `WITH (may_change = ..., tenant = ...)` merges only
//! what it said it would change. What strays is refused, or, for an agent keeping to a merge
//! policy, queued for a person; an admin can widen the scope.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn crm(db: &Db) {
    run(
        db,
        MAIN,
        "create table leads (id int primary key, org int, status text, owner text);
         create table lists (id int primary key, name text);
         insert into leads values (1, 1, 'open', 'ada'), (2, 1, 'open', 'bo'), (3, 2, 'open', 'cy');
         insert into lists values (1, 'vip')",
        &[],
    )
    .unwrap();
}

/// Forks `w` with the scope `with`, runs `sql` in it, and merges it: the merge's error, if any.
fn attempt(db: &Db, w: &str, with: &str, sql: &str) -> Option<String> {
    run(db, MAIN, &format!("create world {w} with ({with})"), &[]).unwrap();
    run(db, w, sql, &[]).unwrap();
    match run(db, MAIN, &format!("merge world {w}"), &[]) {
        Ok(_) => None,
        Err(e) => {
            assert_eq!(e.code, "42501", "{}", e.message);
            assert!(db.world(w).is_ok(), "nothing was merged");
            Some(e.message)
        }
    }
}

#[test]
fn a_merge_outside_its_declared_scope_is_refused() {
    let db = Db::new();
    crm(&db);
    let scope = "may_change = 'leads.status', tenant = 'org = 1'";

    // declared at the fork, and shown with the world
    run(&db, MAIN, &format!("create world w with ({scope}, intent = 'close stale leads', run = 'planner/3')"), &[])
        .unwrap();
    let s = db.world("w").unwrap().scope.expect("a scope");
    assert_eq!(s["may_change"], serde_json::json!([{ "table": "leads", "column": "status" }]));
    assert_eq!(s["tenant"], serde_json::json!({ "column": "org", "value": 1 }));
    assert_eq!((s["intent"].as_str(), s["run"].as_str()), (Some("close stale leads"), Some("planner/3")));
    assert!(db.world("w").unwrap().meta.get("may_change").is_none(), "kept apart from its notes");
    let out = run(&db, MAIN, "show worlds", &[]).unwrap().pop().unwrap();
    let col = out.columns.as_ref().unwrap().iter().position(|(n, _)| n == "scope").unwrap();
    assert!(out.rows.iter().any(|r| r[col].to_text().contains("planner/3")));

    // within it: merged
    assert_eq!(attempt(&db, "ok", scope, "update leads set status = 'closed' where id = 1"), None);

    // a column it didn't list
    let e = attempt(&db, "col", scope, "update leads set owner = 'x' where id = 2").unwrap();
    assert!(e.contains("merging col changes leads.owner: leads/2"), "{e}");
    assert!(e.contains("outside the scope it was forked with (may change leads.status; tenant org = 1)"), "{e}");

    // a row of another tenant, even in the listed column
    let e = attempt(&db, "other", scope, "update leads set status = 'closed' where id = 3").unwrap();
    assert!(e.contains("changes leads rows outside tenant org = 1: leads/3"), "{e}");

    // moving a row out of the tenant counts too
    let e = attempt(&db, "moved", "may_change = 'leads', tenant = 'org = 1'", "update leads set org = 2 where id = 1")
        .unwrap();
    assert!(e.contains("outside tenant org = 1: leads/1"), "{e}");

    // a listed column is updated, not inserted into or deleted from
    let e = attempt(&db, "ins", scope, "insert into leads values (9, 1, 'open', 'dee')").unwrap();
    assert!(e.contains("inserts into leads: leads/9"), "{e}");
    let e = attempt(&db, "del", scope, "delete from leads where id = 2").unwrap();
    assert!(e.contains("deletes from leads: leads/2"), "{e}");

    // another table, and definitions
    let e = attempt(&db, "lists", "may_change = 'leads'", "update lists set name = 'x'").unwrap();
    assert!(e.contains("changes lists.name: lists/1"), "{e}");
    let e = attempt(&db, "view", "may_change = 'leads'", "create view open_leads as select * from leads").unwrap();
    assert!(e.contains("creates view open_leads"), "{e}");

    // a dry run says so
    let blocked = db.merge_preview("col", &Default::default()).unwrap().blocked.unwrap_or_default();
    assert!(blocked.contains("changes leads.owner"), "{blocked}");

    // only an intent: nothing to hold it to
    assert_eq!(attempt(&db, "noted", "intent = 'tidy up'", "update leads set owner = 'z' where id = 3"), None);

    // an admin widens the scope, and the merge goes through
    run(&db, MAIN, "alter world col set (may_change = 'leads.status, leads.owner')", &[]).unwrap();
    run(&db, MAIN, "merge world col", &[]).unwrap();
    // or drops it
    run(&db, MAIN, "alter world lists set (may_change = null)", &[]).unwrap();
    assert!(db.world("lists").unwrap().scope.is_none());
    run(&db, MAIN, "merge world lists", &[]).unwrap();

    // a table's own change is in scope only when the whole table is, and no tenant limits it
    let e = attempt(&db, "reshape", scope, "alter table leads add column note text").unwrap();
    assert!(e.contains("alters table leads"), "{e}");
    assert_eq!(attempt(&db, "whole", "may_change = 'leads'", "alter table leads add column note text"), None);
}

#[test]
fn a_scope_must_name_what_is_there() {
    let db = Db::new();
    crm(&db);
    let code = |with: &str| run(&db, MAIN, &format!("create world x with ({with})"), &[]).unwrap_err();
    let e = code("may_change = 'nosuch'");
    assert_eq!(e.code, "42P01", "{}", e.message);
    assert!(e.message.contains("may_change names nosuch"), "{}", e.message);
    assert!(code("may_change = 'leads.nosuch'").message.contains("leads.nosuch"));
    let e = code("may_change = 'lists', tenant = 'org = 1'");
    assert!(e.message.contains("table lists in may_change has no column org"), "{}", e.message);
    assert!(code("tenant = 'org'").message.contains("tenant must be"));
    assert!(code("may_change = ''").message.contains("names nothing"));
    // a text tenant, and names as SQL reads them
    run(&db, MAIN, "create table \"Regions\" (id int primary key, \"Code\" text)", &[]).unwrap();
    run(&db, MAIN, "create world eu with (may_change = '\"Regions\".\"Code\"', tenant = '\"Code\" = ''eu''')", &[])
        .unwrap();
    let s = db.world("eu").unwrap().scope.unwrap();
    assert_eq!(s["may_change"], serde_json::json!([{ "table": "Regions", "column": "Code" }]));
    assert_eq!(s["tenant"], serde_json::json!({ "column": "Code", "value": "eu" }));
}

#[test]
fn an_agent_under_a_policy_is_queued_and_may_have_to_declare_one() {
    let db = Db::new();
    crm(&db);
    run(&db, MAIN, "create merge policy scoped with (require_scope = true)", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'scoped')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());

    // its policy wants a scope at the fork
    let e = bot.run(&db, "create world loose", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("merge policy scoped has its agents say what a world may change"), "{}", e.message);
    assert!(bot.run(&db, "create world loose with (intent = 'just a note')", &[]).is_err());

    bot.run(&db, "create world w with (may_change = 'leads.status', tenant = 'org = 1'); use world w", &[]).unwrap();
    bot.run(&db, "update leads set status = 'closed', owner = 'bot' where id = 1; use world main", &[]).unwrap();
    let e = bot.run(&db, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("changes leads.owner: leads/1, outside its declared scope"), "{}", e.message);
    assert!(e.message.contains("SHOW REVIEWS"), "{}", e.message);
    let reviews = db.reviews();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].scope.as_ref().unwrap()["tenant"]["column"], "org");

    // it can't widen its own scope; an admin (or a person) can
    let e = bot.run(&db, "alter world w set (may_change = 'leads')", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("only an admin may"), "{}", e.message);
    run(&db, MAIN, "alter world w set (may_change = 'leads')", &[]).unwrap();
    bot.run(&db, "merge world w", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "select owner from leads where id = 1", &[]).unwrap()[0].rows[0][0].to_text(), "bot");
}

#[test]
fn what_a_merge_does_to_main_is_what_is_checked() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table tickets (id text primary key, status text, owner text);
         insert into tickets values ('t1', 'open', 'ada'), ('t2', 'open', 'bo')",
        &[],
    )
    .unwrap();
    let with = "may_change = 'tickets.status'";
    // keeping its row over main's change to another column puts that column back: that changes it
    run(&db, MAIN, &format!("create world kept with ({with})"), &[]).unwrap();
    run(&db, "kept", "update tickets set status = 'done' where id = 't1'", &[]).unwrap();
    run(&db, MAIN, "update tickets set owner = 'cy' where id = 't1'", &[]).unwrap();
    let e = run(&db, MAIN, "merge world kept resolve ('tickets/t1' = ours)", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("changes tickets.owner: tickets/t1"), "{}", e.message);
    // combined by columns, the same merge changes only the status
    run(&db, MAIN, "merge world kept by columns", &[]).unwrap();
    assert_eq!(
        run(&db, MAIN, "select status || ' ' || owner from tickets where id = 't1'", &[]).unwrap()[0].rows[0][0]
            .to_text(),
        "done cy"
    );

    // a row picked by hand is checked as it is, not as the world changed it
    run(&db, MAIN, &format!("create world pick with ({with})"), &[]).unwrap();
    run(&db, "pick", "update tickets set status = 'held' where id = 't2'", &[]).unwrap();
    run(&db, MAIN, "update tickets set status = 'waiting' where id = 't2'", &[]).unwrap();
    let e = run(
        &db,
        MAIN,
        r#"merge world pick resolve ('tickets/t2' = '{"id": "t2", "status": "held", "owner": "zed"}')"#,
        &[],
    )
    .unwrap_err();
    assert!(e.message.contains("changes tickets.owner: tickets/t2"), "{}", e.message);
    run(&db, MAIN, r#"merge world pick resolve ('tickets/t2' = '{"id": "t2", "status": "held", "owner": "bo"}')"#, &[])
        .unwrap();
}

#[test]
fn a_scope_changes_with_the_rest_of_its_patch_or_not_at_all() {
    let db = Db::new();
    crm(&db);
    run(&db, MAIN, "create world w with (may_change = 'leads.status')", &[]).unwrap();
    let before = db.world("w").unwrap().scope;
    // the note is past the metadata limit, so neither it nor the wider scope is written
    let big = "x".repeat(70_000);
    let e = run(&db, MAIN, &format!("alter world w set (may_change = 'leads', note = '{big}')"), &[]).unwrap_err();
    assert!(e.message.contains("limited to"), "{}", e.message);
    assert_eq!(db.world("w").unwrap().scope, before, "the scope wasn't widened");
    run(&db, MAIN, "alter world w set (may_change = 'leads', note = 'wider')", &[]).unwrap();
    let w = db.world("w").unwrap();
    assert_eq!(w.scope.unwrap()["may_change"], serde_json::json!([{ "table": "leads" }]));
    assert_eq!(w.meta["note"], "wider");
}

#[test]
fn a_world_without_a_scope_waits_when_its_policy_requires_one() {
    let db = Db::new();
    crm(&db);
    run(&db, MAIN, "create merge policy p", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'p')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    // forked before the policy asked for a scope (simulate's kept worlds have none either)
    bot.run(&db, "create world w; use world w; update leads set status = 'won' where id = 1; use world main", &[])
        .unwrap();
    run(&db, MAIN, "alter merge policy p set (require_scope = true)", &[]).unwrap();
    let e = bot.run(&db, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("declares no scope (merge policy p has its agents say"), "{}", e.message);
    // given one by an admin, it merges
    run(&db, MAIN, "alter world w set (may_change = 'leads.status')", &[]).unwrap();
    bot.run(&db, "merge world w", &[]).unwrap();
}

#[test]
fn rows_main_laid_out_anew_are_read_as_main_has_them() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table tickets (id text primary key, status text, owner text);
         insert into tickets values ('t1', 'open', 'ada')",
        &[],
    )
    .unwrap();
    run(&db, MAIN, "create world w with (may_change = 'tickets.status')", &[]).unwrap();
    run(&db, "w", "update tickets set status = 'done' where id = 't1'", &[]).unwrap();
    // main adds a column and changes another: combined by columns, only the status is the world's
    run(&db, MAIN, "alter table tickets add column prio int", &[]).unwrap();
    run(&db, MAIN, "update tickets set owner = 'cy', prio = 2 where id = 't1'", &[]).unwrap();
    run(&db, MAIN, "merge world w by columns", &[]).unwrap();
    let got = run(&db, MAIN, "select status || ' ' || owner || ' ' || prio from tickets", &[]).unwrap();
    assert_eq!(got[0].rows[0][0].to_text(), "done cy 2");
}

#[test]
fn a_run_declares_how_many_rows_it_changes_and_deletes() {
    let db = Db::new();
    crm(&db);
    // within its counts: merged
    assert_eq!(
        attempt(&db, "few", "max_rows = 2, max_deletes = 0", "update leads set status = 'won' where id <= 2"),
        None
    );
    // more rows than it declared, across tables
    let e = attempt(
        &db,
        "many",
        "may_change = 'leads, lists', max_rows = '2'",
        "update leads set owner = 'x' where org = 1; update lists set name = 'y'",
    )
    .unwrap();
    assert!(e.contains("merging many changes 3 rows (it declared at most 2)"), "{e}");
    assert!(e.contains("may change leads, lists; at most 2 rows"), "{e}");
    // a whole table, but no deletes
    let e = attempt(&db, "del", "may_change = 'leads', max_deletes = 0", "delete from leads where id = 3").unwrap();
    assert!(e.contains("deletes 1 rows (it declared at most 0)"), "{e}");
    assert!(e.contains("no deletes"), "{e}");
    assert_eq!(
        attempt(&db, "ins", "may_change = 'leads', max_deletes = 0", "insert into leads values (9, 1, 'open', 'dee')"),
        None
    );
    // a dry run says so
    run(&db, MAIN, "create world zero with (max_rows = 0)", &[]).unwrap();
    run(&db, "zero", "update leads set owner = 'z' where id = 1", &[]).unwrap();
    let blocked = db.merge_preview("zero", &Default::default()).unwrap().blocked.unwrap_or_default();
    assert!(blocked.contains("changes 1 rows (it declared at most 0)"), "{blocked}");
    // shown, and an admin can raise it
    let s = db.world("zero").unwrap().scope.unwrap();
    assert_eq!(s["max_rows"], 0);
    run(&db, MAIN, "alter world zero set (max_rows = 5)", &[]).unwrap();
    run(&db, MAIN, "merge world zero", &[]).unwrap();
    // a row left in conflict writes nothing: the conflict is what it gets, not the count
    run(&db, MAIN, "create world clash with (max_rows = 0)", &[]).unwrap();
    run(&db, "clash", "update leads set owner = 'w' where id = 2", &[]).unwrap();
    run(&db, MAIN, "create world first; ", &[]).unwrap();
    run(&db, "first", "update leads set owner = 'm' where id = 2", &[]).unwrap();
    run(&db, MAIN, "merge world first", &[]).unwrap();
    let e = run(&db, MAIN, "merge world clash", &[]).unwrap_err();
    assert!(!e.message.contains("declared at most"), "{}", e.message);
    // not a count
    for bad in ["max_rows = -1", "max_rows = 'lots'", "max_deletes = 1.5"] {
        let e = run(&db, MAIN, &format!("create world x with ({bad})"), &[]).unwrap_err();
        assert!(e.message.contains("must be a whole number of rows"), "{bad}: {}", e.message);
    }
}

#[test]
fn an_agent_declares_less_than_its_policy_never_more() {
    let db = Db::new();
    crm(&db);
    run(&db, MAIN, "create merge policy p with (max_rows = 10, max_deletes = 1)", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'p')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    let e = bot.run(&db, "create world big with (max_rows = 11)", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("max_rows = 11 is more than merge policy p allows (max_rows 10)"), "{}", e.message);
    assert!(bot.run(&db, "create world big with (max_deletes = 2)", &[]).is_err());

    // tighter: its own count holds the merge for a person, under the policy's
    bot.run(&db, "create world w with (max_rows = 1); use world w", &[]).unwrap();
    bot.run(&db, "update leads set status = 'won' where org = 1; use world main", &[]).unwrap();
    let e = bot.run(&db, "merge world w", &[]).unwrap_err();
    assert!(e.message.contains("changes 2 rows (it declared at most 1), outside its declared scope"), "{}", e.message);
    assert_eq!(db.reviews().len(), 1);
}
