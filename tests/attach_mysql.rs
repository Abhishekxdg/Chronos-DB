//! `chronos attach` beside a MySQL (or MariaDB) database, against a real one. CHRONOS_TEST_MYSQL is a URL to a
//! server where tests may create databases (`mysql://root:pw@127.0.0.1:3306/mysql`; skipped without it). Attach
//! copies the tables; a merge into main is written to MySQL first in one transaction, a row changed there meanwhile
//! comes back as a conflict with nothing written; a refresh brings main up to MySQL; what isn't done for MySQL yet
//! is refused, saying so.

use chronos::myclient::{self, Client};
use chronos::sql::run;
use chronos::{Db, MAIN};
use std::process::Command;

fn cell(db: &Db, world: &str, sql: &str) -> String {
    run(db, world, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message)).pop().unwrap().rows[0][0].to_text()
}

fn my_cell(c: &mut Client, sql: &str) -> String {
    let done = c.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    done.last().and_then(|d| d.rows.first()).and_then(|r| r[0].clone()).unwrap_or_else(|| "NULL".into())
}

/// CHRONOS_SOURCE_URL is the process's: one test at a time (under `cargo test`, which runs them as threads).
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A new database `chronos_<tag>_<pid>` on the test server, CHRONOS_SOURCE_URL set to it: (the admin's connection,
/// one to the database, its name). None: no CHRONOS_TEST_MYSQL.
fn throwaway(tag: &str) -> Option<(Client, Client, String)> {
    let admin_url = std::env::var("CHRONOS_TEST_MYSQL").ok()?;
    let name = format!("chronos_{tag}_{}", std::process::id());
    let mut admin = Client::connect(&myclient::parse(&admin_url).unwrap()).unwrap();
    admin.query(&format!("drop database if exists {name}; create database {name}")).unwrap();
    let url = match admin_url.split_once('?') {
        Some((base, q)) => format!("{}/{name}?{q}", base.rsplit_once('/').unwrap().0),
        None => format!("{}/{name}", admin_url.rsplit_once('/').unwrap().0),
    };
    unsafe { std::env::set_var("CHRONOS_SOURCE_URL", &url) };
    let my = Client::connect(&myclient::parse(&url).unwrap()).unwrap();
    Some((admin, my, name))
}

/// A row inserted into an AUTO_INCREMENT table has a provisional id in its world; the merge inserts it without one,
/// takes the id MySQL gave, and writes that where the provisional id stood: the row, and rows that refer to it by a
/// foreign key MySQL declares. A provisional id anywhere else is refused, nothing written.
#[test]
fn rows_are_inserted_into_auto_increment_tables() {
    let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
    let Some((mut admin, mut my, name)) = throwaway("auto") else {
        eprintln!("skipped: set CHRONOS_TEST_MYSQL to a MySQL URL");
        return;
    };
    my.query(
        "create table orders (id int unsigned auto_increment primary key, customer varchar(40) not null,
                              at datetime not null default current_timestamp,
                              status varchar(10) not null default 'new', qty int not null default 3,
                              ref char(36) not null default (uuid()),
                              label varchar(60) as (concat(customer, '!')) stored,
                              shout varchar(60) as (upper(customer)) virtual) engine = InnoDB;
         create table items (id bigint auto_increment primary key, order_id int unsigned not null, sku varchar(20),
                             parent_item bigint, foreign key (order_id) references orders (id),
                             foreign key (parent_item) references items (id)) engine = InnoDB;
         create table order_tags (order_id int unsigned not null, tag varchar(20) not null, primary key (order_id, tag),
                                  foreign key (order_id) references orders (id)) engine = InnoDB;
         create table audit (id int primary key, order_ref int, note varchar(40)) engine = InnoDB;
         create table tiny (id smallint auto_increment primary key, v int) engine = InnoDB;
         insert into orders (customer) values ('old'), ('older');
         insert into items (order_id, sku) values (1, 'a');
         alter table orders auto_increment = 50",
    )
    .unwrap();
    let dir = std::env::temp_dir().join(format!("chronos-attach-mysql-auto-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .arg("attach")
        .arg(&dir)
        .args(["--tables", "orders,items,order_tags,audit,tiny"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("tiny: its AUTO_INCREMENT column is narrower"), "{said}");
    let db = Db::open(&dir).unwrap();

    // new rows: provisional ids here (negative: no row of MySQL's has them), a child naming its parent's
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(&db, "w", "insert into orders (customer) values ('ada'), ('bo')", &[]).unwrap();
    let ada: i64 = cell(&db, "w", "select id from orders where customer = 'ada'").parse().unwrap();
    let bo: i64 = cell(&db, "w", "select id from orders where customer = 'bo'").parse().unwrap();
    assert!(ada < 0 && bo < 0 && ada != bo, "{ada} {bo}");
    run(
        &db,
        "w",
        &format!(
            "insert into items (order_id, sku) values ({ada}, 'x'), ({ada}, 'y'), ({bo}, 'z'), (1, 'old-order');
             insert into order_tags values ({ada}, 'rush'), (2, 'late');
             update items set order_id = {bo} where id = 1"
        ),
        &[],
    )
    .unwrap();
    // (a line under another new line: inserted after it)
    let x: i64 = cell(&db, "w", "select id from items where sku = 'x'").parse().unwrap();
    run(&db, "w", &format!("insert into items (order_id, sku, parent_item) values ({ada}, 'sub', {x})"), &[]).unwrap();
    assert_eq!(cell(&db, "w", "select status || qty from orders where customer = 'ada'"), "new3");
    // the dry run reads MySQL and finds nothing in its way
    let live = db.merge_preview("w", &Default::default()).unwrap().live.expect("checked against MySQL");
    assert!(live.error.is_none() && live.rows.iter().all(|r| !r.changed), "{live:?}");
    run(&db, MAIN, "merge world w", &[]).unwrap();

    // MySQL gave the ids (50, 51: its own counter), and every reference follows
    let real_ada: i64 = my_cell(&mut my, "select id from orders where customer = 'ada'").parse().unwrap();
    let real_bo: i64 = my_cell(&mut my, "select id from orders where customer = 'bo'").parse().unwrap();
    assert_eq!((real_ada, real_bo), (50, 51), "in the order the world made them");
    assert_eq!(my_cell(&mut my, "select count(*) from orders where id < 0"), "0");
    assert_eq!(
        my_cell(&mut my, &format!("select group_concat(sku order by sku) from items where order_id = {real_ada}")),
        "sub,x,y"
    );
    assert_eq!(my_cell(&mut my, &format!("select sku from items where order_id = {real_bo} and id <> 1")), "z");
    assert_eq!(my_cell(&mut my, "select order_id from items where id = 1"), real_bo.to_string());
    assert_eq!(
        my_cell(&mut my, "select p.sku from items c join items p on p.id = c.parent_item where c.sku = 'sub'"),
        "x"
    );
    assert_eq!(my_cell(&mut my, "select order_id from order_tags where tag = 'rush'"), real_ada.to_string());
    assert_eq!(my_cell(&mut my, "select count(*) from order_tags"), "2");
    // main holds MySQL's rows, under MySQL's ids, and none under a provisional one
    for (table, col) in [("orders", "id"), ("items", "id"), ("items", "order_id"), ("order_tags", "order_id")] {
        assert_eq!(cell(&db, MAIN, &format!("select count(*) from {table} where {col} < 0")), "0", "{table}.{col}");
    }
    assert_eq!(cell(&db, MAIN, "select id from orders where customer = 'ada'"), real_ada.to_string());
    assert_eq!(cell(&db, MAIN, "select count(*) from orders"), my_cell(&mut my, "select count(*) from orders"));
    assert_eq!(cell(&db, MAIN, "select count(*) from items"), my_cell(&mut my, "select count(*) from items"));
    assert_eq!(cell(&db, MAIN, "select count(*) from order_tags"), "2");
    assert!(!cell(&db, MAIN, "select at from orders where customer = 'ada'").is_empty(), "MySQL's default came back");
    // defaults: a literal one is the copy's too, an expression is MySQL's to fill, a generated column MySQL's alone
    assert_eq!(cell(&db, MAIN, "select status || qty from orders where customer = 'ada'"), "new3");
    assert_eq!(cell(&db, MAIN, "select length(ref) from orders where customer = 'ada'"), "36");
    assert_eq!(
        cell(&db, MAIN, "select ref from orders where customer = 'ada'"),
        my_cell(&mut my, "select ref from orders where customer = 'ada'")
    );
    assert_eq!(cell(&db, MAIN, "select label || shout from orders where customer = 'ada'"), "ada!ADA");
    assert_eq!(cell(&db, MAIN, "select label from orders where customer = 'old'"), "old!");
    // nothing is left to refresh
    let fresh = chronos::attach::refresh(&db).unwrap();
    assert_eq!(fresh.changed(), 0, "{:?}", fresh.to_json());

    // a provisional id where MySQL declares no foreign key: refused, nothing written
    run(&db, MAIN, "create world u", &[]).unwrap();
    run(&db, "u", "insert into orders (customer) values ('cy')", &[]).unwrap();
    let cy = cell(&db, "u", "select id from orders where customer = 'cy'");
    run(&db, "u", &format!("insert into audit values (1, {cy}, 'made')"), &[]).unwrap();
    let e = run(&db, MAIN, "merge world u", &[]).unwrap_err();
    assert!(e.message.contains("declares no foreign key") && e.message.contains("audit.order_ref"), "{}", e.message);
    assert_eq!(my_cell(&mut my, "select count(*) from orders where customer = 'cy'"), "0");
    assert_eq!(my_cell(&mut my, "select count(*) from audit"), "0");
    // without it, the row goes
    run(&db, "u", "delete from audit", &[]).unwrap();
    run(&db, MAIN, "merge world u", &[]).unwrap();
    assert_eq!(my_cell(&mut my, "select count(*) from orders where customer = 'cy'"), "1");

    // a generated column follows a change to what it is worked out from; a value given it by hand is refused
    run(&db, MAIN, "create world g", &[]).unwrap();
    run(&db, "g", "update orders set customer = 'eve' where customer = 'cy'", &[]).unwrap();
    run(&db, MAIN, "merge world g", &[]).unwrap();
    assert_eq!(cell(&db, MAIN, "select label || shout from orders where customer = 'eve'"), "eve!EVE");
    run(&db, MAIN, "create world g2", &[]).unwrap();
    run(&db, "g2", "update orders set label = 'mine' where customer = 'eve'", &[]).unwrap();
    let e = run(&db, MAIN, "merge world g2", &[]).unwrap_err();
    assert!(e.message.contains("MySQL generates that column"), "{}", e.message);

    // an id given by hand is MySQL's to give: refused
    run(&db, MAIN, "create world h", &[]).unwrap();
    run(&db, "h", "insert into orders (id, customer) values (900, 'hand')", &[]).unwrap();
    let e = run(&db, MAIN, "merge world h", &[]).unwrap_err();
    assert!(e.message.contains("AUTO_INCREMENT"), "{}", e.message);
    // a column too narrow for provisional ids takes no inserts, and says so
    run(&db, MAIN, "create world t", &[]).unwrap();
    let e = run(&db, "t", "insert into tiny (v) values (1)", &[]);
    assert!(e.is_err(), "an insert into tiny went through");

    // the folder opened again: ids go on below those given
    drop(db);
    let db = Db::open(&dir).unwrap();
    run(&db, MAIN, "create world again", &[]).unwrap();
    run(&db, "again", "insert into orders (customer) values ('dee')", &[]).unwrap();
    let dee: i64 = cell(&db, "again", "select id from orders where customer = 'dee'").parse().unwrap();
    assert!(dee < ada.min(bo), "{dee}");
    run(&db, MAIN, "merge world again", &[]).unwrap();
    assert_eq!(my_cell(&mut my, "select count(*) from orders where customer = 'dee'"), "1");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    admin.query(&format!("drop database {name}")).unwrap();
}

/// A merge's rows are kept in MySQL with the merge (`chronos_merge_rows`) and undo it by its id, as beside Postgres:
/// rows it added (under the ids MySQL gave) go, rows it changed or deleted come back as they were.
#[test]
fn a_merge_is_undone_by_its_id() {
    let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
    let Some((mut admin, mut my, name)) = throwaway("undoid") else {
        eprintln!("skipped: set CHRONOS_TEST_MYSQL to a MySQL URL");
        return;
    };
    my.query(
        "create table leads (id int auto_increment primary key, owner varchar(40), score decimal(6,2), meta json,
                             seen datetime(6), label varchar(60) as (concat(owner, '!')) stored) engine = InnoDB;
         insert into leads (owner, score, meta, seen) values ('bo', 1.50, '{\"a\": 1}', '2026-10-01 10:00:00.500000'),
                                                             ('bo', 2.00, null, null), ('eve', 3.25, '[1, 2]', null);
         create table notes (id int auto_increment primary key, lead_id int, body text,
                             foreign key (lead_id) references leads (id)) engine = InnoDB;
         insert into notes (lead_id, body) values (3, 'call eve')",
    )
    .unwrap();
    let dir = std::env::temp_dir().join(format!("chronos-attach-mysql-undoid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .arg("attach")
        .arg(&dir)
        .args(["--tables", "leads,notes"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let db = Db::open(&dir).unwrap();
    let merge = |w: &str, sql: &str| -> String {
        run(&db, MAIN, &format!("create world {w}"), &[]).unwrap();
        run(&db, w, sql, &[]).unwrap_or_else(|e| panic!("{sql}: {}", e.message));
        let _ = chronos::attach::take_merge_id();
        run(&db, MAIN, &format!("merge world {w}"), &[]).unwrap_or_else(|e| panic!("merge {w}: {}", e.message));
        chronos::attach::take_merge_id().expect("the merge's id")
    };
    let state = |my: &mut Client| {
        my_cell(
            my,
            "select concat((select group_concat(concat(id, ':', owner, ':', score, ':', label) order by id) from leads),
                           ' / ', coalesce((select group_concat(concat(lead_id, ':', body) order by id) from notes), ''))",
        )
    };
    let was = state(&mut my);

    // each kind of change, in two tables, a new row with a child of its own
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(
        &db,
        "w",
        "update leads set owner = 'ada', score = 9.75 where owner = 'bo';
         delete from notes where lead_id = 3; delete from leads where id = 3;
         insert into leads (owner, score) values ('new', 4)",
        &[],
    )
    .unwrap();
    let new_id = cell(&db, "w", "select id from leads where owner = 'new'");
    run(&db, "w", &format!("insert into notes (lead_id, body) values ({new_id}, 'welcome')"), &[]).unwrap();
    let _ = chronos::attach::take_merge_id();
    run(&db, MAIN, "merge world w", &[]).unwrap();
    let first = chronos::attach::take_merge_id().expect("the merge's id");
    assert_ne!(state(&mut my), was);
    assert_eq!(my_cell(&mut my, &format!("select count(*) from chronos_merge_rows where merge_id = '{first}'")), "6");
    // the record holds the new row under the id MySQL gave it
    assert_eq!(
        my_cell(
            &mut my,
            &format!(
                "select count(*) from chronos_merge_rows where merge_id = '{first}' and op = 'insert' and json_extract(row_key, '$.id') like '-%'"
            )
        ),
        "0"
    );
    let now = state(&mut my);
    let plan = db
        .undo_merges(
            std::slice::from_ref(&first),
            chronos::attach::UndoOptions { skip_changed: false, dry_run: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!((plan.undone, plan.skipped, plan.dry_run), (6, 0, true), "{plan:?}");
    assert!(plan.cascades.is_none(), "MySQL doesn't say what its cascades wrote: {plan:?}");
    assert_eq!(state(&mut my), now);
    let done = db
        .undo_merges(
            std::slice::from_ref(&first),
            chronos::attach::UndoOptions { skip_changed: false, dry_run: false, ..Default::default() },
        )
        .unwrap();
    assert_eq!((done.undone, done.skipped), (6, 0), "{done:?}");
    assert!(done.merge.is_some());
    assert_eq!(state(&mut my), was);
    assert_eq!(
        my_cell(&mut my, "select concat(json_extract(meta, '$.a'), seen) from leads where id = 1"),
        "12026-10-01 10:00:00.500000"
    );
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0, "main is MySQL's");

    // a row changed straight in MySQL since: left with skip_changed, the rest undone
    let c = merge("c", "update leads set owner = 'cy' where id in (1, 2, 3)");
    my.query("update leads set owner = 'pat' where id = 2").unwrap();
    let e = db
        .undo_merges(
            std::slice::from_ref(&c),
            chronos::attach::UndoOptions { skip_changed: false, dry_run: false, ..Default::default() },
        )
        .unwrap_err();
    assert!(e.to_string().contains("changed"), "{e}");
    let done = db
        .undo_merges(
            std::slice::from_ref(&c),
            chronos::attach::UndoOptions { skip_changed: true, dry_run: false, ..Default::default() },
        )
        .unwrap();
    assert_eq!((done.undone, done.skipped), (2, 1), "{done:?}");
    assert_eq!(my_cell(&mut my, "select group_concat(owner order by id) from leads"), "bo,pat,eve");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    admin.query(&format!("drop database {name}")).unwrap();
}

/// A world that checks its reads (`check_reads`): what it read is checked against MySQL inside the write-back,
/// before anything is written, and by CHECK READS between steps. A row changed straight in MySQL never reaches main,
/// so main's own check can't see it.
#[test]
fn reads_are_checked_against_mysql() {
    let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
    let Some((mut admin, mut my, name)) = throwaway("reads") else {
        eprintln!("skipped: set CHRONOS_TEST_MYSQL to a MySQL URL");
        return;
    };
    my.query(
        "create table leads (id int primary key, org_id int not null, email varchar(80), status varchar(40))
            engine = InnoDB;
         create table suppressed (id int primary key, org_id int not null, email varchar(80)) engine = InnoDB;
         create table optout (email varchar(80) primary key) engine = InnoDB;
         insert into leads values (1, 42, 'ada@x.com', 'new'), (2, 42, 'bo@x.com', 'new'), (3, 7, 'cy@y.com', 'new')",
    )
    .unwrap();
    let dir = std::env::temp_dir().join(format!("chronos-attach-mysql-reads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_chronos"))
        .arg("attach")
        .arg(&dir)
        .args(["--tables", "leads,suppressed,optout"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let db = Db::open(&dir).unwrap();
    assert!(db.attached().unwrap().capabilities().check_reads);
    let merges = |my: &mut Client| my_cell(my, "select count(*) from chronos_merges where id like 'merge:%'");

    // a filtered scan that found nothing
    run(&db, MAIN, "create world w with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "w", "select count(*) from suppressed where email = 'ada@x.com'"), "0");
    run(&db, "w", "update leads set status = 'ready_to_contact' where id = 1", &[]).unwrap();
    // a row its read couldn't have found: nothing it relied on moved
    my.query("insert into suppressed values (1, 42, 'someone@else.com')").unwrap();
    assert_eq!(run(&db, "w", "check reads", &[]).unwrap().pop().unwrap().rows.len(), 0);
    // the email itself, added straight into MySQL: CHECK READS says so before any merge, the world's view stays as
    // it was, and the merge is refused with nothing written
    my.query("insert into suppressed values (2, 42, 'ada@x.com')").unwrap();
    let found = run(&db, MAIN, "check reads of world w", &[]).unwrap().pop().unwrap();
    assert_eq!(found.rows.len(), 1, "{:?}", found.rows);
    assert!(found.rows[0][1].to_text().contains("changed in MySQL since it forked"), "{:?}", found.rows);
    assert_eq!(cell(&db, "w", "select count(*) from suppressed where email = 'ada@x.com'"), "0", "no rebase");
    let e = run(&db, MAIN, "merge world w", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in MySQL since it forked"), "{}", e.message);
    assert_eq!(my_cell(&mut my, "select status from leads where id = 1"), "new", "nothing written");
    assert_eq!(merges(&mut my), "0");
    assert!(db.world("w").is_ok(), "the world stays, to redo or drop");
    run(&db, MAIN, "drop world w", &[]).unwrap();

    // a key looked up that wasn't there, added in MySQL since
    run(&db, MAIN, "create world k with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "k", "select count(*) from optout where email = 'bo@x.com'"), "0");
    run(&db, "k", "update leads set status = 'ready_to_contact' where id = 2", &[]).unwrap();
    my.query("insert into optout values ('bo@x.com')").unwrap();
    let e = run(&db, MAIN, "merge world k", &[]).unwrap_err();
    assert!(e.message.contains("optout"), "{}", e.message);
    assert_eq!(my_cell(&mut my, "select status from leads where id = 2"), "new");
    run(&db, MAIN, "drop world k", &[]).unwrap();

    // a row read and then changed in MySQL
    run(&db, MAIN, "create world r with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "r", "select status from leads where id = 2"), "new");
    run(&db, "r", "update leads set status = 'ready_to_contact' where id = 1", &[]).unwrap();
    my.query("update leads set status = 'do_not_contact' where id = 2").unwrap();
    let e = run(&db, MAIN, "merge world r", &[]).unwrap_err();
    assert!(e.message.contains("leads"), "{}", e.message);
    assert_eq!(merges(&mut my), "0");
    run(&db, MAIN, "drop world r", &[]).unwrap();

    // what it read is still as MySQL has it: the merge goes through, reads checked
    chronos::attach::refresh(&db).unwrap();
    run(&db, MAIN, "create world ok with (check_reads = true)", &[]).unwrap();
    assert_eq!(cell(&db, "ok", "select count(*) from leads where status = 'new'"), "2");
    run(&db, "ok", "update leads set status = 'ready_to_contact' where id = 1", &[]).unwrap();
    run(&db, MAIN, "merge world ok", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(my_cell(&mut my, "select status from leads where id = 1"), "ready_to_contact");
    assert_eq!(merges(&mut my), "1");

    // a world that read for one tenant is held by that tenant's rows only: the app writing another tenant's rows
    // of a table it read whole doesn't hold it, a row of its own changed or added in MySQL does
    chronos::attach::refresh(&db).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent run42 with (can = '{can}', tenant = 'org_id = 42')"), &[]).unwrap();
    let work = |w: &str| {
        let mut s = chronos::sql::Session::new(MAIN);
        s.agent = Some("run42".into());
        let sql = format!(
            "create world {w} with (check_reads = true); use world {w}; select count(*) from leads;
             update leads set email = '{w}@x.com' where id = 1; use world main"
        );
        let out = s.run(&db, &sql, &[]).unwrap();
        assert_eq!(out[2].rows[0][0].to_text(), "2", "its tenant's leads");
    };
    work("others");
    my.query("update leads set status = 'app' where id = 3; insert into leads values (30, 7, 'new@y.com', 'new')")
        .unwrap();
    run(&db, MAIN, "merge world others", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(my_cell(&mut my, "select email from leads where id = 1"), "others@x.com");
    work("mine");
    my.query("insert into leads values (31, 42, 'new@x.com', 'new')").unwrap();
    let e = run(&db, MAIN, "merge world mine", &[]).unwrap_err();
    assert!(e.message.contains("changed in MySQL since it forked"), "{}", e.message);
    assert!(!e.message.contains("more broadly"), "{}", e.message);
    assert_eq!(my_cell(&mut my, "select email from leads where id = 1"), "others@x.com", "nothing written");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    admin.query(&format!("drop database {name}")).unwrap();
}

#[test]
fn merges_are_written_to_mysql_first() {
    let _one = ONE.lock().unwrap_or_else(|p| p.into_inner());
    let Ok(admin_url) = std::env::var("CHRONOS_TEST_MYSQL") else {
        eprintln!("skipped: set CHRONOS_TEST_MYSQL to a MySQL URL");
        return;
    };
    let name = format!("chronos_attach_{}", std::process::id());
    let mut admin = Client::connect(&myclient::parse(&admin_url).unwrap()).unwrap();
    admin.query(&format!("drop database if exists {name}; create database {name}")).unwrap();
    let url = match admin_url.split_once('?') {
        Some((base, q)) => format!("{}/{name}?{q}", base.rsplit_once('/').unwrap().0),
        None => format!("{}/{name}", admin_url.rsplit_once('/').unwrap().0),
    };
    unsafe { std::env::set_var("CHRONOS_SOURCE_URL", &url) };
    let mut my = Client::connect(&myclient::parse(&url).unwrap()).unwrap();
    my.query(
        "create table leads (id int primary key, name varchar(40) not null, score decimal(10,2), seen datetime(6),
                             meta json, flag tinyint(1) not null default 0) engine = InnoDB;
         insert into leads values (1, 'one', 1.50, '2026-10-01 10:00:00.500000', '{\"a\": 1}', 1),
                                  (2, 'two', 2.00, '2026-10-02 10:00:00', null, 0),
                                  (3, 'three', null, null, '[1, 2]', 0);
         create table notes (id int auto_increment primary key, body text) engine = InnoDB;
         insert into notes (body) values ('first');
         create table blobs (id int primary key, b blob) engine = InnoDB",
    )
    .unwrap();
    let dir = std::env::temp_dir().join(format!("chronos-attach-mysql-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let attach = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_chronos")).arg("attach").arg(&dir).args(args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
        )
    };
    // a binary column isn't attached yet: refused, nothing left behind
    let (ok, out) = attach(&["--tables", "leads,blobs"]);
    assert!(!ok && out.contains("binary column"), "{out}");
    assert!(!dir.exists());
    let (ok, out) = attach(&["--tables", "leads,notes"]);
    assert!(ok, "{out}");
    let db = Db::open(&dir).unwrap();
    assert!(db.attached().unwrap().is_mysql());
    // it says which of the two it is (from the server's version at the attach), and what it can't do yet
    let att = db.attached().unwrap();
    let maria = my_cell(&mut my, "select version()").contains("MariaDB");
    assert_eq!(att.kind().as_str(), if maria { "mariadb" } else { "mysql" });
    assert!(!att.capabilities().follow && !att.capabilities().act_as && att.capabilities().dry_run);
    assert_eq!(cell(&db, MAIN, "select count(*) from leads"), "3");
    assert_eq!(cell(&db, MAIN, "select score from leads where id = 1"), "1.50");

    // a merge: written to MySQL first, each kind of change
    run(&db, MAIN, "create world w", &[]).unwrap();
    run(
        &db,
        "w",
        "update leads set name = 'uno', score = 9.25 where id = 1;
         insert into leads (id, name, score, seen, meta, flag) values (10, 'ten', 10, '2026-11-01 08:00:00', '{\"b\": true}', 1);
         delete from leads where id = 2",
        &[],
    )
    .unwrap();
    run(&db, MAIN, "merge world w", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(my_cell(&mut my, "select name from leads where id = 1"), "uno");
    assert_eq!(my_cell(&mut my, "select score from leads where id = 1"), "9.25");
    assert_eq!(my_cell(&mut my, "select count(*) from leads where id = 10"), "1");
    assert_eq!(my_cell(&mut my, "select count(*) from leads where id = 2"), "0");
    assert_eq!(my_cell(&mut my, "select count(*) from chronos_merges where id like 'merge:%'"), "1");
    assert_eq!(cell(&db, MAIN, "select name from leads where id = 1"), "uno");

    // a row changed in MySQL since main had it: a conflict, nothing written, main takes MySQL's row
    my.query("update leads set name = 'app' where id = 3").unwrap();
    run(&db, MAIN, "create world c", &[]).unwrap();
    run(&db, "c", "update leads set name = 'mine' where id = 3; update leads set name = 'x' where id = 1", &[])
        .unwrap();
    let e = run(&db, MAIN, "merge world c", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("changed in MySQL since main last had them"), "{}", e.message);
    assert_eq!(my_cell(&mut my, "select name from leads where id = 1"), "uno", "nothing written");
    assert_eq!(cell(&db, MAIN, "select name from leads where id = 3"), "app");
    run(&db, MAIN, "merge world c using ours", &[]).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(my_cell(&mut my, "select name from leads where id = 3"), "mine");

    // values written as main holds them compare equal again: a row nobody changed is no conflict
    run(&db, MAIN, "create world same", &[]).unwrap();
    run(&db, "same", "update leads set flag = 0 where id = 10", &[]).unwrap();
    run(&db, MAIN, "merge world same", &[]).unwrap_or_else(|e| panic!("{}", e.message));

    // a refresh: MySQL's own changes come over
    my.query(
        "insert into leads (id, name) values (20, 'twenty'); update leads set score = 3.5 where id = 3;
         delete from leads where id = 10",
    )
    .unwrap();
    let r = chronos::attach::refresh(&db).unwrap();
    assert_eq!(r.tables.iter().find(|t| t.0 == "leads").map(|t| (t.1, t.2, t.3)), Some((1, 1, 1)), "{r:?}");
    assert_eq!(cell(&db, MAIN, "select name from leads where id = 20"), "twenty");
    assert_eq!(chronos::attach::refresh(&db).unwrap().changed(), 0, "nothing changed since");

    // an insert into a table keyed by AUTO_INCREMENT: refused, as its values are MySQL's to give
    run(&db, MAIN, "create world n", &[]).unwrap();
    assert!(
        run(&db, "n", "insert into notes (id, body) values (5, 'x')", &[]).is_err() || {
            run(&db, MAIN, "merge world n", &[]).is_err()
        }
    );

    // a dry run: each row the merge would write compared with MySQL's in one snapshot, nothing locked or written
    let was = my_cell(&mut my, "select name from leads where id = 1");
    run(&db, MAIN, "create world d", &[]).unwrap();
    run(&db, "d", "update leads set name = 'd' where id = 1; update leads set name = 'e' where id = 3", &[]).unwrap();
    let plan = db.merge_preview("d", &Default::default()).unwrap();
    let live = plan.live.expect("checked against MySQL");
    assert!(live.error.is_none(), "{:?}", live.error);
    assert_eq!(live.rows.len(), 2, "{live:?}");
    assert!(live.rows.iter().all(|r| !r.changed), "{live:?}");
    // a row MySQL changed since: the dry run says so, and says MySQL's row
    my.query("update leads set name = 'theirs' where id = 3").unwrap();
    let live = db.merge_preview("d", &Default::default()).unwrap().live.unwrap();
    let changed: Vec<_> = live.rows.iter().filter(|r| r.changed).collect();
    assert_eq!((live.rows.len(), changed.len()), (2, 1), "{live:?}");
    assert!(changed[0].postgres.as_deref().is_some_and(|j| j.contains("theirs")), "{live:?}");
    assert_eq!(my_cell(&mut my, "select name from leads where id = 1"), was, "nothing written");
    assert!(db.world("d").is_ok());

    // what isn't done for MySQL yet says so
    let e = run(&db, MAIN, "merge world d rehearse", &[]);
    if let Err(e) = e {
        assert!(e.message.contains("MySQL"), "{}", e.message);
    }

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    admin.query(&format!("drop database {name}")).unwrap();
}
