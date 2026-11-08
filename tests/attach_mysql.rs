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

#[test]
fn merges_are_written_to_mysql_first() {
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
    assert!(out.contains("AUTO_INCREMENT"), "{out}");
    let db = Db::open(&dir).unwrap();
    assert!(db.attached().unwrap().is_mysql());
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

    // what isn't done for MySQL yet says so
    let e = run(
        &db,
        MAIN,
        "create world d; use world d; update leads set name = 'd' where id = 1; use world main; merge world d dry run",
        &[],
    );
    if let Err(e) = e {
        assert!(e.message.contains("MySQL"), "{}", e.message);
    }

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    admin.query(&format!("drop database {name}")).unwrap();
}
