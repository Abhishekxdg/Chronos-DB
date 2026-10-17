//! What a change affects: `DIFF ... READERS` lists what in the database reads each column a world
//! changed (views, materialized views, merge checks, triggers, functions, keys, foreign keys,
//! CHECK constraints, indexes), then says what it couldn't see.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

/// (kind, reader, columns) for each reader of table `table`, sorted; coverage rows left out.
fn readers(db: &Db, world: &str, table: &str) -> Vec<(String, String, String)> {
    let out = run(db, MAIN, &format!("diff world {world} readers"), &[]).unwrap().pop().unwrap();
    let mut r: Vec<(String, String, String)> = out
        .rows
        .iter()
        .filter(|r| r[0].to_text() == table)
        .map(|r| (r[2].to_text(), r[3].to_text(), r[1].to_text()))
        .collect();
    r.sort();
    r
}

fn crm(db: &Db) {
    run(
        db,
        MAIN,
        "create table lists (id int primary key, name text, owner text);
         create table leads (id int primary key, list int references lists, who text,
                             status text check (status in ('new', 'won')), score int);
         create index leads_score on leads (score);
         create index leads_who on leads (lower(who));
         insert into lists values (1, 'vip', 'ada'), (2, 'cold', 'bo');
         insert into leads values (1, 1, 'cy', 'new', 5), (2, 1, 'di', 'new', 7), (3, 2, 'ed', 'won', 1);
         create view open_leads as select l.id, l.who from leads l where l.status = 'new';
         create view list_sizes as select name, (select count(*) from leads d where d.list = lists.id) as n from lists;
         create view everything as select * from lists;
         create view open_names as select who from open_leads;
         create materialized view scores as select sum(score) as total from leads;
         create function bump() returns trigger language plpgsql as $$ begin new.score := new.score + 1; return new; end $$;
         create trigger leads_bump before update of score on leads for each row execute function bump();
         create function owned(o text) returns bigint language sql as 'select count(*) from lists where owner = o';
         create merge check lists_keep_leads on tables (leads, lists) as select id from lists where id not in (select list from leads)",
        &[],
    )
    .unwrap();
}

fn r(kind: &str, name: &str, cols: &str) -> (String, String, String) {
    (kind.into(), name.into(), cols.into())
}

#[test]
fn an_update_names_what_reads_its_columns() {
    let db = Db::new();
    crm(&db);
    db.fork(MAIN, "won").unwrap();
    run(&db, "won", "update leads set status = 'won' where id = 1", &[]).unwrap();
    // status: its CHECK, the view filtering on it, and the view reading that view; not the
    // views, check, trigger, index or function reading other columns
    assert_eq!(
        readers(&db, "won", "leads"),
        [
            r("check", "leads_status_check", "status"),
            r("view", "open_leads", "status"),
            r("view", "open_names", "status")
        ]
    );
    // what wasn't traced is said, always
    let out = run(&db, MAIN, "diff world won readers", &[]).unwrap().pop().unwrap();
    let coverage: Vec<String> =
        out.rows.iter().filter(|r| r[2].to_text() == "coverage").map(|r| r[4].to_text()).collect();
    assert_eq!(coverage.len(), 3);
    assert!(coverage[1].starts_with("clients' reads recorded since"), "{coverage:?}");

    // UPDATE OF score: the trigger fires, and the index, the materialized view read it
    db.fork(MAIN, "rescore").unwrap();
    run(&db, "rescore", "update leads set score = 9 where id = 2", &[]).unwrap();
    let got = readers(&db, "rescore", "leads");
    for want in [
        r("index", "leads_score", "score"),
        r("materialized view", "scores", "score"),
        r("trigger", "leads_bump", "score"),
    ] {
        assert!(got.contains(&want), "{want:?} in {got:?}");
    }
    assert!(!got.iter().any(|x| x.1 == "open_leads"), "{got:?}");

    // an expression index reads the columns in it
    db.fork(MAIN, "rename").unwrap();
    run(&db, "rename", "update leads set who = 'CY' where id = 1", &[]).unwrap();
    let got = readers(&db, "rename", "leads");
    assert!(got.contains(&r("index", "leads_who", "who")) && got.contains(&r("view", "open_leads", "who")), "{got:?}");
}

#[test]
fn inserts_deletes_and_new_columns_reach_every_reader_of_the_table() {
    let db = Db::new();
    crm(&db);
    // a deleted list: its key, the foreign key pointing at it, every view and check reading lists,
    // and the function naming lists and owner
    db.fork(MAIN, "drop2").unwrap();
    run(&db, "drop2", "delete from leads where list = 2; delete from lists where id = 2", &[]).unwrap();
    let got = readers(&db, "drop2", "lists");
    for want in [
        r("primary key", "lists_pkey", "id"),
        r("foreign key", "leads_list_fkey", "id"),
        r("view", "everything", "id, name, owner"),
        r("view", "list_sizes", "id, name"),
        r("merge check", "lists_keep_leads", "id"),
        r("function", "owned", "owner"),
    ] {
        assert!(got.contains(&want), "{want:?} in {got:?}");
    }
    let leads = readers(&db, "drop2", "leads");
    assert!(leads.contains(&r("merge check", "lists_keep_leads", "list")), "{leads:?}");
    assert!(!leads.iter().any(|x| x.0 == "trigger"), "an UPDATE trigger doesn't fire on a delete: {leads:?}");
    let out = run(&db, MAIN, "diff world drop2 readers", &[]).unwrap().pop().unwrap();
    let fk = out.rows.iter().find(|r| r[3].to_text() == "leads_list_fkey" && r[0].to_text() == "lists").unwrap();
    assert_eq!(fk[4].to_text(), "leads references lists");
    let star = out.rows.iter().find(|r| r[3].to_text() == "everything").unwrap();
    assert_eq!(star[4].to_text(), "reads every column of lists");

    // a column added: only what reads every column of the table sees it
    db.fork(MAIN, "note").unwrap();
    run(&db, "note", "alter table lists add column note text", &[]).unwrap();
    assert_eq!(readers(&db, "note", "lists"), [r("view", "everything", "note")]);

    // nothing changed: only what wasn't traced
    db.fork(MAIN, "idle").unwrap();
    let out = run(&db, MAIN, "diff world idle readers", &[]).unwrap().pop().unwrap();
    assert!(out.rows.iter().all(|r| r[2].to_text() == "coverage") && out.rows.len() == 3, "{:?}", out.rows);
    // from one world to another, and reading it changes nothing (agents with read may)
    let out = run(&db, "drop2", "diff world main to drop2 readers", &[]).unwrap().pop().unwrap();
    assert!(out.rows.iter().any(|r| r[3].to_text() == "lists_pkey"), "{:?}", out.rows);
}

/// Clients seen reading: a session with a client records the columns its statements read (a
/// SELECT's, an UPDATE's WHERE and SET), and a change's readers name them with when they last
/// read. Things outside SQL are registered by hand.
#[test]
fn clients_seen_reading_and_registered_readers() {
    let db = Db::new();
    crm(&db);
    let mut sync = Session::new(MAIN);
    sync.client = Some("crm_sync".into());
    sync.run(&db, "select id, status from leads where list = 1", &[]).unwrap();
    let mut billing = Session::new(MAIN);
    billing.client = Some("billing (nightly)".into());
    billing.run(&db, "update leads set score = score + 1 where status = 'won'", &[]).unwrap();
    let mut audit = Session::new(MAIN);
    audit.client = Some("audit".into());
    audit.run(&db, "select * from lists", &[]).unwrap();
    let mut nobody = Session::new(MAIN); // no client: nothing recorded
    nobody.run(&db, "select who from leads", &[]).unwrap();
    let (_, seen) = db.reads();
    assert!(seen.iter().any(|r| r.client == "crm_sync" && r.table == "leads" && r.column == "status"), "{seen:?}");
    assert!(!seen.iter().any(|r| r.column == "who"), "{seen:?}");

    db.fork(MAIN, "won").unwrap();
    run(&db, "won", "update leads set status = 'won' where id = 1", &[]).unwrap();
    let got = readers(&db, "won", "leads");
    for want in [r("client", "billing (nightly)", "status"), r("client", "crm_sync", "status")] {
        assert!(got.contains(&want), "{want:?} in {got:?}");
    }
    let out = run(&db, MAIN, "diff world won readers", &[]).unwrap().pop().unwrap();
    let row = out.rows.iter().find(|r| r[3].to_text() == "crm_sync").unwrap();
    assert!(row[4].to_text().starts_with("last read 20"), "{:?}", row[4]);

    // registered by hand: an export of leads.status, a dump of all of lists
    run(&db, MAIN, "register reader nightly_export on leads (status); register reader dump on lists", &[]).unwrap();
    db.fork(MAIN, "won2").unwrap();
    run(&db, "won2", "update leads set status = 'won' where id = 2; update lists set owner = 'x' where id = 1", &[])
        .unwrap();
    assert!(readers(&db, "won2", "leads").contains(&r("registered reader", "nightly_export", "status")));
    let lists = readers(&db, "won2", "lists");
    for want in [r("registered reader", "dump", "owner"), r("client", "audit", "owner")] {
        assert!(lists.contains(&want), "{want:?} in {lists:?}");
    }
    let shown = run(&db, MAIN, "show readers", &[]).unwrap().pop().unwrap();
    let kinds: Vec<(String, String, String)> =
        shown.rows.iter().map(|r| (r[0].to_text(), r[1].to_text(), r[2].to_text())).collect();
    for want in
        [("dump", "registered", "lists"), ("nightly_export", "registered", "leads"), ("crm_sync", "client", "leads")]
    {
        assert!(kinds.contains(&(want.0.into(), want.1.into(), want.2.into())), "{want:?} in {kinds:?}");
    }
    run(&db, MAIN, "unregister reader nightly_export on leads", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "unregister reader nightly_export", &[]).unwrap_err().code, "42704");
    assert_eq!(run(&db, MAIN, "register reader x on leads (nope)", &[]).unwrap_err().code, "42703");
    assert_eq!(run(&db, MAIN, "register reader x on nowhere", &[]).unwrap_err().code, "42P01");
}

/// The recorded reads are the database's, kept in its folder across a restart, with since when.
#[test]
fn recorded_reads_survive_a_restart() {
    let dir = std::env::temp_dir().join(format!("chronos-reads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let since = {
        let db = Db::open(&dir).unwrap();
        run(&db, MAIN, "create table t (id int primary key, a text)", &[]).unwrap();
        let mut s = Session::new(MAIN);
        s.client = Some("reporter".into());
        s.run(&db, "select a from t", &[]).unwrap();
        db.reads().0
    };
    let db = Db::open(&dir).unwrap();
    let (again, seen) = db.reads();
    assert_eq!(again, since, "recording started then, not at this open");
    assert!(seen.iter().any(|r| r.client == "reporter" && r.table == "t" && r.column == "a"), "{seen:?}");
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Critical readers: a mark lives in the world (forks carry it, a diff shows it, who and when
/// are kept), sorts its reader first, and a merge policy holds an agent's merge that changes a
/// column a critical reader reads, unless the policy allows it.
#[test]
fn critical_readers_come_first_and_hold_agents_merges() {
    let db = Db::new();
    crm(&db);
    let mut billing = Session::new(MAIN);
    billing.client = Some("billing".into());
    billing.run(&db, "select id, status, score from leads", &[]).unwrap();
    run(&db, MAIN, "mark reader billing critical; mark reader not_seen_yet critical", &[]).unwrap();
    let shown = run(&db, MAIN, "show readers", &[]).unwrap().pop().unwrap();
    let row = shown.rows.iter().find(|r| r[0].to_text() == "billing").unwrap();
    assert_eq!((row[5].to_text(), row[6].to_text()), ("true".into(), "a person".into()), "{row:?}");
    let unseen = shown.rows.iter().find(|r| r[0].to_text() == "not_seen_yet").unwrap();
    assert_eq!(unseen[1].to_text(), "marked");

    // the critical reader first, and flagged
    db.fork(MAIN, "won").unwrap();
    run(&db, "won", "update leads set status = 'won' where id = 1", &[]).unwrap();
    let out = run(&db, MAIN, "diff world won readers", &[]).unwrap().pop().unwrap();
    assert_eq!(
        (out.rows[0][3].to_text(), out.rows[0][5].to_text()),
        ("billing".into(), "true".into()),
        "{:?}",
        out.rows
    );

    // a mark changed in a world shows in its diff, as SQL too
    db.fork(MAIN, "calm").unwrap();
    run(&db, "calm", "unmark reader billing", &[]).unwrap();
    let d = run(&db, MAIN, "diff world calm", &[]).unwrap().pop().unwrap();
    assert!(d.rows.iter().any(|r| r[0].to_text() == "billing" && r[2].to_text() == "unmark critical"), "{:?}", d.rows);
    let sql = run(&db, MAIN, "diff world calm as sql", &[]).unwrap().pop().unwrap();
    assert!(sql.rows.iter().any(|r| r[0].to_text() == "unmark reader billing;"), "{:?}", sql.rows);
    assert_eq!(run(&db, "calm", "unmark reader billing", &[]).unwrap_err().code, "42704");

    // an agent under a policy: a merge changing what billing reads waits for a person
    run(&db, MAIN, "create merge policy p", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'p')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(&db, "create world b1; use world b1; update leads set status = 'won' where id = 1; use world main", &[])
        .unwrap();
    let e = bot.run(&db, "merge world b1", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(e.message.contains("changes leads.status, which critical client billing reads"), "{}", e.message);
    // what billing doesn't read merges on its own
    bot.run(&db, "create world b2; use world b2; update leads set who = 'x' where id = 2; use world main", &[])
        .unwrap();
    bot.run(&db, "merge world b2", &[]).unwrap();
    // taking the mark away is itself a schema change a person reviews
    bot.run(&db, "create world b3; use world b3; unmark reader billing; use world main", &[]).unwrap();
    let e = bot.run(&db, "merge world b3", &[]).unwrap_err();
    assert!(e.message.contains("changes the schema: critical billing"), "{}", e.message);
    // a policy that allows it
    run(&db, MAIN, "alter merge policy p set (critical = true)", &[]).unwrap();
    bot.run(&db, "merge world b1", &[]).unwrap();
}
