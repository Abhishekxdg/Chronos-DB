//! Masked columns (`ALTER TABLE ... ALTER COLUMN ... SET MASKED`): an agent without `unmask` reads a mask of each
//! value wherever it reads the table, can't learn the value by filtering on it, and is refused what would read it as
//! it is; people and agents with `unmask` read the values.

use chronos::sql::{Session, run};
use chronos::{AgentSpec, Can, Db, MAIN};

fn setup(db: &Db) {
    run(
        db,
        MAIN,
        "create table customers (id int primary key, email text unique, phone text, notes text, tier text);
         insert into customers values (1, 'ann@example.com', '555-0101', 'vip', 'gold'),
                                      (2, 'bob@example.com', '555-0102', 'late payer', 'free');
         create table leads (id int primary key, email text);
         insert into leads values (10, 'bob@example.com'), (11, 'cy@example.com');
         alter table customers alter column email set masked;
         alter table customers alter column phone set masked with partial;
         alter table customers alter column notes set masked with null;
         alter table leads alter column email set masked",
        &[],
    )
    .unwrap();
}

fn agent(db: &Db, name: &str, can: &[Can]) -> Session {
    db.create_agent(name, &AgentSpec { can: Some(can.to_vec()), ..Default::default() }).unwrap();
    let mut s = Session::new(MAIN);
    s.agent = Some(name.into());
    s
}

fn bot(db: &Db) -> Session {
    agent(db, "bot", &[Can::Read, Can::Fork, Can::WriteOwn])
}

/// The rows of the last statement of `sql`, as text.
fn rows(db: &Db, s: &mut Session, sql: &str) -> Vec<Vec<String>> {
    let out = s.run(db, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    out.last().unwrap().rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect()).collect()
}

fn refused(db: &Db, s: &mut Session, sql: &str) -> String {
    let e = s.run(db, sql, &[]).expect_err(sql);
    assert_eq!(e.code, "42501", "{sql}: {}", e.message);
    e.message
}

#[test]
fn an_agent_reads_masks_and_a_person_reads_values() {
    let db = Db::new();
    setup(&db);
    let mut s = bot(&db);
    let r = rows(&db, &mut s, "select id, email, phone, notes, tier from customers order by id");
    assert!(r[0][1].starts_with("m_") && r[0][1].len() == 26, "{r:?}");
    assert_ne!(r[0][1], r[1][1], "different values, different masks");
    assert_eq!(r[0][2], "****0101");
    assert_eq!(r[0][3], "", "NULL");
    assert_eq!(r[0][4], "gold", "an unmasked column is as it is");
    // a person reads the values
    let p = run(&db, MAIN, "select email, phone, notes from customers where id = 1", &[]).unwrap();
    let p: Vec<String> = p[0].rows[0].iter().map(|v| v.to_text()).collect();
    assert_eq!(p, ["ann@example.com", "555-0101", "vip"]);
    // and so does an agent with 'unmask'
    let mut u = agent(&db, "support", &[Can::Read, Can::Unmask]);
    assert_eq!(rows(&db, &mut u, "select email from customers where id = 1")[0][0], "ann@example.com");
    // `*`, a view a person made, a CTE and a subquery read masks too
    run(&db, MAIN, "create view vips as select id, email from customers where tier = 'gold'", &[]).unwrap();
    for sql in [
        "select email from (select * from customers) c where id = 1",
        "select email from vips",
        "with c as (select email, id from customers) select email from c where id = 1",
        "select (select email from customers where id = 1)",
    ] {
        assert_eq!(rows(&db, &mut s, sql)[0][0], r[0][1], "{sql}");
    }
}

#[test]
fn filters_see_only_the_mask() {
    let db = Db::new();
    setup(&db);
    let mut s = bot(&db);
    for sql in [
        "select count(*) from customers where email = 'ann@example.com'",
        "select count(*) from customers where email like 'ann%'",
        "select count(*) from customers where position('@' in email) > 0",
        "select count(*) from customers where notes is not null",
        "select count(*) from customers where phone like '555%'",
    ] {
        assert_eq!(rows(&db, &mut s, sql)[0][0], "0", "{sql}");
    }
    // equal values mask alike, across tables: joins and DISTINCT still work
    let j = rows(&db, &mut s, "select c.id, l.id from customers c join leads l on l.email = c.email");
    assert_eq!(j, [["2", "10"]]);
    let n = rows(
        &db,
        &mut s,
        "select count(distinct email) from (select email from customers union all select email from leads) u",
    );
    assert_eq!(n[0][0], "3");
}

#[test]
fn what_would_read_values_as_they_are_is_refused() {
    let db = Db::new();
    setup(&db);
    let mut s = bot(&db);
    s.run(&db, "create world w; use world w", &[]).unwrap();
    // writing by key is fine, and so is writing a masked column
    s.run(&db, "update customers set tier = 'silver' where id = 2", &[]).unwrap();
    s.run(&db, "update customers set email = 'new@example.com' where id = 2", &[]).unwrap();
    s.run(&db, "insert into customers (id, email, tier) values (3, 'dee@example.com', 'free') returning id", &[])
        .unwrap();
    s.run(&db, "delete from customers where id in (select id from customers where tier = 'free')", &[]).unwrap();
    for sql in [
        "update customers set tier = 'x' where email = 'ann@example.com'",
        "update customers set tier = email where id = 1",
        "update customers set tier = 'x' where id = 1 returning *",
        "insert into customers (id, tier) values (4, 'free') returning email",
        "delete from customers where phone like '555%'",
        "insert into customers (id, email) values (1, 'z') on conflict (id) do update set tier = customers.email",
        "merge into customers c using leads l on c.id = l.id when matched then update set tier = 'x'",
        "alter table customers alter column email drop masked",
        "alter table customers add column copy text generated always as (email) stored",
        "create index on customers (email)",
        "select * from search('customers', 'ann')",
        "diff world w as sql",
    ] {
        refused(&db, &mut s, sql);
    }
    // and the row API, which shows rows as they are
    let m = chronos::sql::row_security_refuses(&db, "bot", "w", &["customers"]).unwrap().unwrap();
    assert!(m.contains("masked"), "{m}");
    assert!(chronos::sql::row_security_refuses(&db, "bot", "w", &["leads2"]).unwrap().is_none());
}

#[test]
fn diffs_and_errors_show_no_values() {
    let db = Db::new();
    setup(&db);
    let mut s = bot(&db);
    s.run(&db, "create world w; use world w; update customers set email = 'eve@example.com' where id = 1", &[])
        .unwrap();
    let d = s.run(&db, "diff world w", &[]).unwrap().pop().unwrap();
    let text = format!("{:?}", d.rows);
    assert!(!text.contains("@example.com") && !text.contains("555-01"), "{text}");
    // a person's diff shows them
    let p = run(&db, MAIN, "diff world w", &[]).unwrap().pop().unwrap();
    assert!(format!("{:?}", p.rows).contains("eve@example.com"));
    // a unique violation doesn't tell the key
    let e = s.run(&db, "insert into customers (id, email) values (9, 'bob@example.com')", &[]).unwrap_err();
    assert_eq!(e.code, "23505");
    let all = format!("{} {:?}", e.message, e.info);
    assert!(!all.contains("bob@example.com"), "{all}");
}

#[test]
fn masks_follow_the_column() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "alter table customers rename column email to mail", &[]).unwrap();
    let mut s = bot(&db);
    assert!(rows(&db, &mut s, "select mail from customers where id = 1")[0][0].starts_with("m_"));
    // a fork has them, and dropping one in a world leaves main's
    run(&db, MAIN, "create world p; use world p", &[]).unwrap();
    run(&db, "p", "alter table customers alter column mail drop masked", &[]).unwrap();
    s.run(&db, "use world p", &[]).unwrap();
    assert_eq!(rows(&db, &mut s, "select mail from customers where id = 1")[0][0], "ann@example.com");
    s.run(&db, "use world main", &[]).unwrap();
    assert!(rows(&db, &mut s, "select mail from customers where id = 1")[0][0].starts_with("m_"));
    // a partition of a masked table is masked too
    run(
        &db,
        MAIN,
        "create table events (id int, at date, who text) partition by range (at);
         create table events_26 partition of events for values from ('2026-01-01') to ('2027-01-01');
         insert into events values (1, '2026-05-01', 'ann');
         alter table events alter column who set masked",
        &[],
    )
    .unwrap();
    assert!(rows(&db, &mut s, "select who from events")[0][0].starts_with("m_"));
    assert!(rows(&db, &mut s, "select who from events_26")[0][0].starts_with("m_"));
}

#[test]
fn set_masked_is_parsed_and_kept() {
    let db = Db::new();
    setup(&db);
    let e = run(&db, MAIN, "alter table customers alter column tier set masked with sha1", &[]).unwrap_err();
    assert_eq!(e.code, "42601", "{}", e.message);
    // it survives a reopen of the schema (a fresh session), and SHOW AGENT ... PRIVILEGES lists it
    db.create_agent("bot", &AgentSpec::default()).unwrap();
    let p = run(&db, MAIN, "show agent bot privileges", &[]).unwrap().pop().unwrap();
    let cols: Vec<Vec<String>> = p.rows.iter().map(|r| r.iter().map(|v| v.to_text()).collect()).collect();
    let email = cols.iter().find(|r| r[0] == "customers" && r[1] == "email").expect("a row for the column");
    assert_eq!(email[2], "masked (hash)");
    let phone = cols.iter().find(|r| r[0] == "customers" && r[1] == "phone").unwrap();
    assert_eq!(phone[2], "masked (partial)");
}

/// An agent that owns a world (so may change its tables' definitions where nothing is masked) may not drop or rename
/// a masked table's triggers and policies, nor touch its rules: each could read or show the values as they are.
#[test]
fn rules_triggers_and_policies_of_a_masked_table_stay_as_they_are() {
    let db = Db::new();
    setup(&db);
    run(
        &db,
        MAIN,
        "create table copies (id int, secret text);
         create function keep() returns trigger language plpgsql as $$ begin return new; end $$;
         create trigger audit before update on customers for each row execute function keep();
         create rule log as on delete to customers do also insert into copies values (old.id, 'gone');
         alter table customers enable row level security;
         alter table customers force row level security;
         create policy only_gold on customers using (tier = 'gold')",
        &[],
    )
    .unwrap();
    let mut s = bot(&db);
    s.run(&db, "create world w; use world w", &[]).unwrap();
    for sql in [
        "drop trigger audit on customers",
        "alter trigger audit on customers rename to audit2",
        "drop policy only_gold on customers",
    ] {
        let m = refused(&db, &mut s, sql);
        assert!(m.contains("masked"), "{sql}: {m}");
    }
    // (rules need 'admin', which reads masked columns as they are: refused before masks are looked at)
    for sql in [
        "create rule steal as on update to customers do also insert into copies values (old.id, old.email)",
        "drop rule log on customers",
        "alter rule log on customers rename to log2",
        "alter table customers disable rule log",
    ] {
        refused(&db, &mut s, sql);
    }
    // a table without masked columns is the agent's to change in its world
    s.run(&db, "create trigger t2 before update on copies for each row execute function keep()", &[]).unwrap();
    s.run(&db, "drop trigger t2 on copies", &[]).unwrap();
}

/// A generated column made from a masked one is masked too, whether it came first or after: it holds the value.
#[test]
fn a_generated_copy_of_a_masked_column_is_masked() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table people (id int primary key, secret text, hint text,
                              exposed text generated always as (secret) stored,
                              shout text generated always as (upper(hint)) stored);
         insert into people (id, secret, hint) values (1, 'hunter2', 'pets');
         alter table people alter column secret set masked;
         alter table people alter column hint set masked with null",
        &[],
    )
    .unwrap();
    let mut s = bot(&db);
    let r = rows(&db, &mut s, "select secret, exposed, shout from people");
    assert!(r[0][1].starts_with("m_"), "{r:?}");
    assert_eq!(r[0][0], r[0][1], "the copy masks as its value does");
    assert_eq!(r[0][2], "", "made from a NULL-masked column: NULL");
    assert_eq!(rows(&db, &mut s, "select count(*) from people where exposed = 'hunter2'")[0][0], "0");
    // one a person adds later is masked as well
    run(&db, MAIN, "alter table people add column again text generated always as (secret || '!') stored", &[]).unwrap();
    assert!(rows(&db, &mut s, "select again from people")[0][0].starts_with("m_"));
    // a person reads them all
    let p = run(&db, MAIN, "select exposed, shout from people", &[]).unwrap();
    assert_eq!(p[0].rows[0][0].to_text(), "hunter2");
    assert_eq!(p[0].rows[0][1].to_text(), "PETS");
}

/// INSERT ... RETURNING the target's whole row would return the masked values it computed.
#[test]
fn returning_a_whole_row_of_a_masked_table_is_refused() {
    let db = Db::new();
    setup(&db);
    let mut s = bot(&db);
    s.run(&db, "create world w; use world w", &[]).unwrap();
    for sql in [
        "insert into customers (id, tier) values (3, 'free') returning row_to_json(customers)",
        "insert into customers (id, tier) values (4, 'free') returning to_jsonb(customers.*)",
        "insert into customers as c (id, tier) values (5, 'free') returning row_to_json(c)",
        "insert into customers (id, tier) values (6, 'free') returning customers",
    ] {
        refused(&db, &mut s, sql);
    }
    // its unmasked columns it may return
    assert_eq!(
        rows(&db, &mut s, "insert into customers (id, tier) values (7, 'free') returning id, tier")[0][1],
        "free"
    );
}

/// FOR UPDATE locks a masked table's rows as any table's (by its key, which is not masked here), and `ctid` reads.
#[test]
fn locking_and_ctid_read_a_masked_table_as_a_table() {
    let db = Db::new();
    setup(&db);
    let mut s = bot(&db);
    s.run(&db, "create world w; use world w", &[]).unwrap();
    s.run(&db, "begin", &[]).unwrap();
    let r = rows(&db, &mut s, "select id, email from customers where id = 1 for update");
    assert_eq!(r[0][0], "1");
    assert!(r[0][1].starts_with("m_"), "{r:?}");
    rows(&db, &mut s, "select id from customers c where id = 2 for update of c");
    s.run(&db, "commit", &[]).unwrap();
    // a second session waits for the lock the first holds, as on any table
    let mut a = bot2(&db);
    let mut b = bot2_peer(&db);
    for x in [&mut a, &mut b] {
        x.run(&db, "use world w", &[]).unwrap();
    }
    a.run(&db, "begin; select id from customers where id = 1 for update", &[]).unwrap();
    b.run(&db, "set lock_timeout = '200ms'", &[]).unwrap();
    let e = b.run(&db, "begin; select id from customers where id = 1 for update", &[]).unwrap_err();
    assert_eq!(e.code, "55P03", "it waited for the lock: {}", e.message);
    a.run(&db, "rollback", &[]).unwrap();
    b.run(&db, "rollback", &[]).unwrap();
    // a table found again by a masked column (it has no key) can't be locked through its masks
    run(
        &db,
        MAIN,
        "create table tags (name text, note text); insert into tags values ('a', 'b');
                   alter table tags alter column name set masked",
        &[],
    )
    .unwrap();
    s.run(&db, "use world main", &[]).unwrap();
    refused(&db, &mut s, "select note from tags for update");
    // ctid
    let c = rows(&db, &mut s, "select ctid, id from customers order by id");
    assert_eq!(c[0][0], "(0,1)", "{c:?}");
    let star = s.run(&db, "select * from customers where id = 1", &[]).unwrap().pop().unwrap();
    assert_eq!(star.columns.unwrap().len(), 5, "`*` gives no ctid");
}

fn bot2(db: &Db) -> Session {
    agent(db, "bot2", &[Can::Read, Can::Fork, Can::Write])
}

fn bot2_peer(db: &Db) -> Session {
    let mut s = Session::new(MAIN);
    s.agent = Some("bot2".into());
    let _ = db;
    s
}

/// FORMAT keeps a value's shape (letters, digits, `@` and `.` where they were, the same length) with keyed stand-ins,
/// the same for the same value, for agents that validate formats. Issue #45.
#[test]
fn a_format_mask_keeps_the_shape() {
    let db = Db::new();
    setup(&db);
    run(&db, MAIN, "alter table customers alter column email set masked with format", &[]).unwrap();
    run(&db, MAIN, "alter table leads alter column email set masked with format", &[]).unwrap();
    let mut b = bot(&db);
    let got = rows(&db, &mut b, "select email from customers order by id");
    let (ann, bob) = (&got[0][0], &got[1][0]);
    assert_ne!(ann, "ann@example.com");
    assert_eq!(ann.len(), "ann@example.com".len());
    let shape = |s: &str| s.chars().map(|c| if c.is_ascii_lowercase() { 'a' } else { c }).collect::<String>();
    assert_eq!(shape(ann), "aaa@aaaaaaa.aaa", "{ann}");
    assert_ne!(ann, bob);
    // the same value masks the same, so joins still work
    let joined = rows(&db, &mut b, "select l.id from leads l join customers c on c.email = l.email");
    assert_eq!(joined, [["10"]]);
    // and a filter on it tests the stand-in, not the value
    assert!(rows(&db, &mut b, "select id from customers where email = 'ann@example.com'").is_empty());
    let shown = run(&db, MAIN, "show agent bot privileges", &[]).unwrap().pop().unwrap();
    assert!(shown.rows.iter().any(|r| r[2].to_text() == "masked (format)"));
}

/// An agent that may insert into a table with a unique masked column can tell whether a value is there: SHOW AGENT
/// PRIVILEGES says so on the column. Issue #45.
#[test]
fn a_unique_masked_column_says_an_insert_reveals_it() {
    let db = Db::new();
    setup(&db);
    bot(&db);
    let shown = run(&db, MAIN, "show agent bot privileges", &[]).unwrap().pop().unwrap();
    let email = shown.rows.iter().find(|r| r[0].to_text() == "customers" && r[1].to_text() == "email").unwrap();
    assert!(email[3].to_text().contains("an insert reveals whether a value is there"), "{:?}", email);
    let phone = shown.rows.iter().find(|r| r[0].to_text() == "customers" && r[1].to_text() == "phone").unwrap();
    assert!(!phone[3].to_text().contains("reveals"), "not unique: {:?}", phone);
}

/// A table or materialized view a person makes from masked columns masks its copies of them: a column copied as it
/// is keeps its mode, one worked out from one is masked with HASH (NULL from a NULL one), the rest aren't. Issue #45.
#[test]
fn copies_a_person_makes_stay_masked() {
    let db = Db::new();
    setup(&db);
    run(
        &db,
        MAIN,
        "create table copy as select id, email, phone, lower(email) as low, notes || '!' as n2, tier from customers;
         create materialized view mv as select c.id, c.email, l.id as lead from customers c join leads l on l.email = c.email;
         create table everything as select * from customers;
         create table both_ as select email from customers union select email from leads",
        &[],
    )
    .unwrap();
    let mut b = bot(&db);
    let got = rows(&db, &mut b, "select id, email, phone, low, n2, tier from copy where id = 1");
    let r = &got[0];
    assert_eq!(r[0], "1");
    assert!(r[1].starts_with("m_"), "{r:?}");
    assert_eq!(r[2], "****0101");
    assert!(r[3].starts_with("m_"), "{r:?}");
    assert_eq!(r[4], "", "NULL");
    assert_eq!(r[5], "gold");
    let mv = rows(&db, &mut b, "select id, email, lead from mv");
    assert_eq!(mv[0][0], "2");
    assert!(mv[0][1].starts_with("m_") && mv[0][2] == "10", "{mv:?}");
    let all = rows(&db, &mut b, "select email, tier from everything where id = 2");
    assert!(all[0][0].starts_with("m_") && all[0][1] == "free", "{all:?}");
    // too involved to follow column by column: every column masked
    let u = rows(&db, &mut b, "select email from both_ limit 1");
    assert!(u[0][0].starts_with("m_"), "{u:?}");
    // a person reads them as they are
    let p = run(&db, MAIN, "select email from copy where id = 1", &[]).unwrap().pop().unwrap();
    assert_eq!(p.rows[0][0].to_text(), "ann@example.com");
}

/// A person's trigger that reads a masked column of the rows it fires for (in NEW or OLD, its WHEN, or its transition
/// tables) would hand the agent the value as it is (copied to a table of its own, raised, acted on): it may not fire
/// for an agent's UPDATE or DELETE. An INSERT's NEW is the agent's own row, and a trigger that reads only columns that
/// aren't masked fires as ever. Issue #45.
#[test]
fn a_trigger_that_reads_a_masked_column_does_not_fire_for_an_agent() {
    let db = Db::new();
    setup(&db);
    run(
        &db,
        MAIN,
        "create table audit (id int, what text);
         create function copy_email() returns trigger language plpgsql as $$
           begin insert into audit values (new.id, new.email); return new; end $$;
         create function copy_row() returns trigger language plpgsql as $$
           begin insert into audit values (old.id, row_to_json(old)::text); return old; end $$;
         create function copy_tier() returns trigger language plpgsql as $$
           begin insert into audit values (new.id, new.tier); return new; end $$;
         create trigger cp after insert or update on customers for each row execute function copy_email();
         create trigger gone after delete on customers for each row execute function copy_row()",
        &[],
    )
    .unwrap();
    let mut s = bot(&db);
    s.run(&db, "create world w; use world w", &[]).unwrap();
    let m = refused(&db, &mut s, "update customers set tier = 'silver' where id = 1");
    assert!(m.contains("trigger \"cp\"") && m.contains("column \"email\""), "{m}");
    let m = refused(&db, &mut s, "delete from customers where id = 2");
    assert!(m.contains("trigger \"gone\"") && m.contains("the whole row"), "{m}");
    assert_eq!(rows(&db, &mut s, "select count(*) from audit"), [["0"]], "nothing was copied");
    // an insert's NEW is the agent's own row
    s.run(&db, "insert into customers values (3, 'cy@example.com', null, null, 'free')", &[]).unwrap();
    let p = run(&db, "w", "select what from audit", &[]).unwrap().pop().unwrap();
    assert_eq!(p.rows[0][0].to_text(), "cy@example.com");
    // what it says to the agent's owner
    let shown = run(&db, MAIN, "show agent bot privileges", &[]).unwrap().pop().unwrap();
    let email = shown.rows.iter().find(|r| r[0].to_text() == "customers" && r[1].to_text() == "email").unwrap();
    assert!(email[3].to_text().contains("fires trigger cp is refused"), "{email:?}");
    assert!(email[5].to_text().contains("trigger gone reads the whole row"), "{email:?}");
    // a trigger that reads only what isn't masked fires
    run(
        &db,
        MAIN,
        "drop trigger cp on customers; drop trigger gone on customers;
         create trigger t after update on customers for each row execute function copy_tier()",
        &[],
    )
    .unwrap();
    let mut s = bot2(&db);
    s.run(&db, "create world w2; use world w2", &[]).unwrap();
    s.run(&db, "update customers set tier = 'silver' where id = 1", &[]).unwrap();
    // a WHEN that tests a masked column is a read of it
    run(
        &db,
        MAIN,
        "create trigger probe after update on customers for each row when (old.email like 'a%') execute function copy_tier()",
        &[],
    )
    .unwrap();
    let mut s = bot2_peer(&db);
    s.run(&db, "create world w3; use world w3", &[]).unwrap();
    let m = refused(&db, &mut s, "update customers set tier = 'silver' where id = 1");
    assert!(m.contains("trigger \"probe\"") && m.contains("column \"email\""), "{m}");
    // a person, and an agent with 'unmask', fire them
    run(&db, MAIN, "update customers set tier = 'bronze' where id = 1", &[]).unwrap();
    let mut u = agent(&db, "support", &[Can::Read, Can::Fork, Can::WriteOwn, Can::Unmask]);
    u.run(&db, "create world w4; use world w4", &[]).unwrap();
    u.run(&db, "update customers set tier = 'silver' where id = 1", &[]).unwrap();
}

/// tsvector_update_trigger copies the words of the text columns it reads into its tsvector column: of a masked
/// column, that column is masked too. Issue #45.
#[test]
fn a_tsvector_made_from_a_masked_column_is_masked() {
    let db = Db::new();
    run(
        &db,
        MAIN,
        "create table docs (id int primary key, body text, tsv tsvector);
         create trigger words before insert or update on docs for each row
           execute function tsvector_update_trigger(tsv, 'pg_catalog.english', body);
         insert into docs values (1, 'secret merger plans', null);
         alter table docs alter column body set masked",
        &[],
    )
    .unwrap();
    let p = run(&db, MAIN, "select tsv::text from docs", &[]).unwrap().pop().unwrap();
    assert!(p.rows[0][0].to_text().contains("merger"), "a person reads the words");
    let mut s = bot(&db);
    let r = rows(&db, &mut s, "select tsv::text from docs");
    assert!(r[0][0].starts_with("m_"), "{r:?}");
}
