//! The agent gateway's red-team suite (docs/designs/agent-gateway-build.md, E3's exit test): an agent held to one
//! tenant (`CREATE AGENT ... WITH (tenant = 'org_id = 42')`), told by a prompt injection to reach another tenant's
//! rows, gets nothing of them: not in rows, joins, unions, CTEs, windows or aggregates; not through views, functions,
//! time travel, a copy or a sample; not by changing settings; not in error messages, plans or catalog figures; and
//! no write of it reaches them. Each probe runs in main and in a world the agent forked.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

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
