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
