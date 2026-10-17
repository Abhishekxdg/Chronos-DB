//! The stale-read check: a world that checks its reads is refused (or, for an agent under a
//! merge policy, queued) when rows it read changed in the world it merges into since it forked,
//! though it never wrote them. Its own writes to them conflict as always.

use chronos::sql::{Session, run};
use chronos::{Db, MAIN};

fn bank(db: &Db) {
    run(
        db,
        MAIN,
        "create table accounts (id int primary key, balance int); create table payouts (id int primary key, account int, amount int);
         insert into accounts select g, 100 from generate_series(1, 6) g",
        &[],
    )
    .unwrap();
}

#[test]
fn a_merge_that_relied_on_a_changed_row_is_refused() {
    let db = Db::new();
    bank(&db);
    // two workers decide a payout from account 1's balance; only the checking one is held
    for w in ["checked", "unchecked"] {
        let with = if w == "checked" { " with (check_reads = true)" } else { "" };
        run(&db, MAIN, &format!("create world {w}{with}"), &[]).unwrap();
        let mut s = Session::new(w);
        s.run(&db, "select balance from accounts where id = 1", &[]).unwrap();
        s.run(&db, "insert into payouts values (1, 1, 100)", &[]).unwrap();
    }
    run(&db, MAIN, "update accounts set balance = 0 where id = 1", &[]).unwrap(); // spent meanwhile
    let blocked = db.merge_preview("checked", &Default::default()).unwrap().blocked.unwrap_or_default();
    assert!(blocked.contains("read rows that changed since it forked: accounts/1"), "{blocked}");
    let e = run(&db, MAIN, "merge world checked", &[]).unwrap_err();
    assert_eq!(e.code, "40001", "{}", e.message);
    assert!(e.message.contains("merging checked read rows that changed since it forked: accounts/1"), "{}", e.message);
    assert!(db.world("checked").is_ok(), "nothing was merged");
    run(&db, MAIN, "merge world unchecked", &[]).unwrap(); // no check: its reads aren't kept

    // a row it didn't read may change
    run(&db, MAIN, "create world narrow with (check_reads = true)", &[]).unwrap();
    run(&db, "narrow", "select balance from accounts where id = 2", &[]).unwrap();
    run(&db, "narrow", "insert into payouts values (2, 2, 50)", &[]).unwrap();
    run(&db, MAIN, "update accounts set balance = 1 where id = 3", &[]).unwrap();
    run(&db, MAIN, "merge world narrow", &[]).unwrap();

    // a scan reads the table: a row added to it changes what it read
    run(&db, MAIN, "create world total with (check_reads = true)", &[]).unwrap();
    run(&db, "total", "select sum(balance) from accounts", &[]).unwrap();
    run(&db, "total", "insert into payouts values (3, 4, 10)", &[]).unwrap();
    run(&db, MAIN, "insert into accounts values (7, 5)", &[]).unwrap();
    let e = run(&db, MAIN, "merge world total", &[]).unwrap_err();
    assert!(e.code == "40001" && e.message.contains("accounts"), "{}", e.message);

    // reads inside a transaction count for its world
    run(&db, MAIN, "create world tx with (check_reads = true)", &[]).unwrap();
    let mut s = Session::new("tx");
    s.run(&db, "begin; select balance from accounts where id = 4; insert into payouts values (4, 4, 1); commit", &[])
        .unwrap();
    run(&db, MAIN, "update accounts set balance = 2 where id = 4", &[]).unwrap();
    let e = run(&db, MAIN, "merge world tx", &[]).unwrap_err();
    assert!(e.message.contains("accounts/4"), "{}", e.message);

    // turned on only while it has changed nothing; a person may turn it off
    assert!(run(&db, MAIN, "alter world total set (check_reads = true)", &[]).is_err());
    run(&db, MAIN, "alter world tx set (check_reads = false)", &[]).unwrap();
    run(&db, MAIN, "merge world tx", &[]).unwrap();
}

#[test]
fn an_agent_whose_policy_checks_reads_is_queued_and_cant_opt_out() {
    let db = Db::new();
    bank(&db);
    run(&db, MAIN, "create merge policy careful with (check_reads = true)", &[]).unwrap();
    let can = "read,fork,write_own,write_main,merge_own";
    run(&db, MAIN, &format!("create agent bot with (can = '{can}', policy = 'careful')"), &[]).unwrap();
    let mut bot = Session::new(MAIN);
    bot.agent = Some("bot".into());
    bot.run(&db, "create world pay; use world pay; select balance from accounts where id = 5; insert into payouts values (5, 5, 100)", &[])
        .unwrap();
    run(&db, MAIN, "update accounts set balance = 0 where id = 5", &[]).unwrap();
    bot.run(&db, "use world main", &[]).unwrap();
    let e = bot.run(&db, "merge world pay", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    assert!(
        e.message.contains("read rows that changed since it forked: accounts/5") && e.message.contains("SHOW REVIEWS")
    );
    assert_eq!(db.reviews().len(), 1);
    // it can't switch the check off on its own world; a person can
    let e = bot.run(&db, "alter world pay set (check_reads = false)", &[]).unwrap_err();
    assert_eq!(e.code, "42501", "{}", e.message);
    run(&db, MAIN, "alter world pay set (check_reads = false)", &[]).unwrap();
    bot.run(&db, "merge world pay", &[]).unwrap();
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

/// What a world read is kept across a clean restart, and checked as before; after a crash it's
/// unknown (even with a file left from an earlier clean close), so the merge is held.
#[test]
fn reads_survive_a_clean_restart_but_not_a_crash() {
    let base = std::env::temp_dir().join(format!("chronos-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (dir, saved, crashed) = (base.join("db"), base.join("saved"), base.join("crashed"));
    {
        let db = Db::open(&dir).unwrap();
        bank(&db);
        run(&db, MAIN, "create world w with (check_reads = true)", &[]).unwrap();
        run(&db, "w", "select balance from accounts where id = 1", &[]).unwrap(); // a row
        run(&db, "w", "select id from accounts where balance > 150", &[]).unwrap(); // a filter
        run(&db, "w", "insert into payouts values (1, 1, 1)", &[]).unwrap();
        run(&db, MAIN, "create world v with (check_reads = true)", &[]).unwrap();
        run(&db, "v", "select balance from accounts where id = 2", &[]).unwrap();
        run(&db, "v", "insert into payouts values (2, 2, 2)", &[]).unwrap();
    } // a clean close keeps them
    assert!(dir.join("readsets").exists());
    copy_dir(&dir, &saved);
    let db = Db::open(&dir).unwrap();
    assert!(!dir.join("readsets").exists(), "taken back, and deleted so a crash never finds it");
    // a row it didn't read changes, and one its filter matches now: the read was kept
    run(&db, MAIN, "update accounts set balance = 120 where id = 3", &[]).unwrap();
    run(&db, MAIN, "update accounts set balance = 500 where id = 4", &[]).unwrap();
    let e = run(&db, MAIN, "merge world w", &[]).unwrap_err();
    assert!(e.code == "40001" && e.message.contains("accounts/4"), "{}", e.message);
    assert!(!e.message.contains("restarted"), "{}", e.message);
    run(&db, MAIN, "merge world v", &[]).unwrap(); // read accounts/2 only: unchanged
    // a crash now ("kill -9"): what w read since the open is lost with it
    run(&db, "w", "select balance from accounts where id = 5", &[]).unwrap();
    copy_dir(&dir, &crashed);
    drop(db);
    // even with the file of the earlier clean close back in place: it's not this close's
    std::fs::copy(saved.join("readsets"), crashed.join("readsets")).unwrap();
    let db = Db::open(&crashed).unwrap();
    assert!(!crashed.join("readsets").exists());
    let version = db.world("w").unwrap().version; // open during the crash: confirmed by version
    let e = run(&db, MAIN, &format!("merge world w at version {version}"), &[]).unwrap_err();
    assert!(e.code == "40001" && e.message.contains("before the database restarted"), "{}", e.message);
    drop(db);
    std::fs::remove_dir_all(&base).unwrap();
}

/// A scan of a whole table with conditions on its columns read the rows those can match, not
/// the table: on a busy table, rows the agent couldn't have seen change without holding its merge.
#[test]
fn a_filtered_scan_holds_a_merge_only_for_rows_its_conditions_match() {
    let db = Db::new();
    bank(&db);
    // (accounts 1 to 6, each with 100; no index on balance, so these scan the table)
    let case = |n: i64, read: &str, change: &str| {
        let w = format!("w{n}");
        run(&db, MAIN, &format!("create world {w} with (check_reads = true)"), &[]).unwrap();
        run(&db, &w, read, &[]).unwrap();
        run(&db, &w, &format!("insert into payouts values ({n}, 1, 1)"), &[]).unwrap();
        run(&db, MAIN, change, &[]).unwrap();
        run(&db, MAIN, &format!("merge world {w}"), &[])
    };
    // a range: a row that stays outside it may change; one that moves into it holds the merge
    let over = "select id from accounts where balance > 150";
    assert!(case(1, over, "update accounts set balance = 120 where id = 3").is_ok());
    let e = case(2, over, "update accounts set balance = 200 where id = 4").unwrap_err();
    assert!(e.code == "40001" && e.message.contains("accounts/4"), "{}", e.message);
    run(&db, MAIN, "drop world w2", &[]).unwrap();
    // equality (through the search index): a row it matched before counts too
    let eq = "select id from accounts where balance = 120";
    assert!(case(3, eq, "update accounts set balance = 99 where id = 5").is_ok());
    let e = case(4, eq, "update accounts set balance = 1 where id = 3").unwrap_err();
    assert!(e.code == "40001" && e.message.contains("accounts/3"), "{}", e.message);
    run(&db, MAIN, "drop world w4", &[]).unwrap();
    // a row added that the conditions match changed what it read; one they don't, didn't
    let between = "select sum(balance) from accounts where balance >= 90 and balance <= 110";
    assert!(case(5, between, "insert into accounts values (7, 500)").is_ok());
    let e = case(6, between, "insert into accounts values (8, 95)").unwrap_err();
    assert!(e.code == "40001" && e.message.contains("accounts/8"), "{}", e.message);
    run(&db, MAIN, "drop world w6", &[]).unwrap();
    // conditions it can't bound (a function of the column) still read the whole table
    let e = case(9, "select id from accounts where balance * 2 > 1000", "update accounts set balance = 1 where id = 6")
        .unwrap_err();
    assert!(e.code == "40001" && e.message.contains("read accounts"), "{}", e.message);
    // and a rebase: the conditions hold it the same way
    run(&db, MAIN, "create world r with (check_reads = true)", &[]).unwrap();
    run(&db, "r", over, &[]).unwrap();
    run(&db, MAIN, "update accounts set balance = 130 where id = 1", &[]).unwrap();
    assert_eq!(run(&db, MAIN, "rebase world r", &[]).unwrap()[0].tag, "REBASE 1");
    run(&db, MAIN, "update accounts set balance = 300 where id = 2", &[]).unwrap();
    let e = run(&db, MAIN, "rebase world r", &[]).unwrap_err();
    assert!(e.code == "40001" && e.message.starts_with("rebasing r read rows"), "{}", e.message);
}

/// A merge of a world that checks its reads, done before a restart, is done after it: replaying
/// the log doesn't check reads it can't know.
#[test]
fn a_checked_merge_survives_a_restart() {
    let dir = std::env::temp_dir().join(format!("chronos-stale-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let db = Db::open(&dir).unwrap();
        bank(&db);
        run(&db, MAIN, "create world w with (check_reads = true)", &[]).unwrap();
        run(&db, "w", "select balance from accounts where id = 1", &[]).unwrap();
        run(&db, "w", "insert into payouts values (1, 1, 1)", &[]).unwrap();
        run(&db, MAIN, "merge world w", &[]).unwrap();
        run(&db, MAIN, "insert into payouts values (2, 2, 2)", &[]).unwrap(); // after it in the log
    }
    let db = Db::open(&dir).unwrap();
    let n = run(&db, MAIN, "select count(*) from payouts", &[]).unwrap()[0].rows[0][0].to_text();
    assert_eq!(n, "2", "the merge was replayed");
    assert!(db.world("w").is_err());
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
